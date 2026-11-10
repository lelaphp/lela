//! One intended call as the hook receives it, and where it lands: the
//! file relative to the workspace, the workspace whose manifest judges the
//! call, and the checkout that workspace is in.

use std::cell::OnceCell;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::approval::{Approvals, Asked, Evidence};
use crate::{git, manifest};

/// One intended call, as the hook receives it on stdin. `cwd` places
/// a main loop's call without a file in a worktree, and a subagent's
/// line in the checkout it starts in; `session_id` is what a reading
/// carries so denials count per session (§10 rule 6); the other keys of
/// the recorded shape (`permission_mode`, …) are carried by the JSON and
/// not read.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Call {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Arguments,
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: String,
    /// The hook's id for this one call, the same before it and after it:
    /// what pairs `permit --after` with what `permit` took before the call.
    #[serde(default)]
    pub tool_use_id: String,
    /// What the call displaces, where the caller knows it and the file at
    /// the path is not it: the diff judge's, the tracked content at the
    /// base it measures since, or in a merge each parent's ([`Call::diff_of`],
    /// #2783). Empty at the hook, whose JSON never carries it, since a call
    /// there displaces what is at the path.
    #[serde(skip)]
    pub(super) displaced: Vec<String>,
    /// The session's transcript, where Claude Code writes the person's
    /// answers ([`super::approval`]); the hook's, which in a subagent is
    /// the main session's.
    pub transcript_path: Option<String>,
    /// The subagent the call runs in, absent on the main loop: a call of
    /// one without a file is judged at the session root ([`Call::nested`]).
    pub agent_id: Option<String>,
    /// The hook event the call came in on, which the hook's answer names
    /// again: `PostToolUse`, or `PostToolUseFailure` for a `Bash` call that
    /// exited non-zero, which gets no `PostToolUse` (#2762).
    pub(super) hook_event_name: Option<String>,
    /// The answered `AskUserQuestion` calls the function hook read from
    /// the engine, read only by that adapter ([`Evidence`]).
    pub(super) asked: Option<Vec<Asked>>,
    /// Which of the two the adapter judging the call reads.
    #[serde(skip)]
    pub(super) evidence: Evidence,
    /// The approvals, read once for every rule that asks.
    #[serde(skip)]
    approvals: OnceCell<Approvals>,
}

/// The fields of `tool_input` a rule can read: an `Edit`'s, a `Write`'s, a
/// `Bash` call's or a `Monitor`'s.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Arguments {
    pub file_path: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
    #[serde(default)]
    pub replace_all: bool,
    pub content: Option<String>,
    pub command: Option<String>,
    /// A `Bash` call the runtime runs in the background and notifies the
    /// end of, which a `runs = "foreground"` rule does not judge (#3624).
    #[serde(default)]
    pub run_in_background: bool,
}

impl Call {
    /// The text of the call `json` carries, or else the one on stdin.
    pub(super) fn text(json: Option<&str>) -> Result<String> {
        if let Some(json) = json {
            return Ok(json.to_owned());
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)
            .context("reading the tool call from stdin")?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The call from the hook's JSON.
    pub fn parse(text: &str) -> Result<Call> {
        serde_json::from_str(text).context("reading the tool call")
    }

    /// The `Write` that would put `content` at `path`.
    pub(super) fn write_of(path: &str, content: Option<&str>) -> Call {
        Call {
            tool_name: "Write".to_owned(),
            tool_input: Arguments {
                file_path: Some(path.to_owned()),
                content: content.map(str::to_owned),
                ..Arguments::default()
            },
            ..Call::default()
        }
    }

    /// One file of a diff as the `Write` of its content: the same call
    /// with what the base carries at that path beside it, so a rule that
    /// asks what the write adds is asked of the diff and not of a file
    /// against itself.
    pub(super) fn diff_of(path: &str, content: &str, displaced: &[String]) -> Call {
        Call {
            displaced: displaced.to_vec(),
            ..Call::write_of(path, Some(content))
        }
    }

    /// The person's approvals the call carries evidence of, read on the
    /// first ask ([`Approvals::of`]).
    pub(super) fn approvals(&self) -> &Approvals {
        self.approvals.get_or_init(|| Approvals::of(self))
    }

    /// The text the call would put in the file: an `Edit`'s `new_string` or
    /// a `Write`'s `content`.
    pub(super) fn written(&self) -> Option<&str> {
        self.tool_input
            .new_string
            .as_deref()
            .or(self.tool_input.content.as_deref())
    }

    /// What the call displaces and what it would leave in its place, both
    /// as whole texts, for a rule that fires on what a write adds rather
    /// than on what the file already carries (#510). `None` for a call
    /// that writes no file.
    ///
    /// A `Write` leaves its `content`; what it displaces is the caller's
    /// `displaced` where there is one, the diff judge's base, and what
    /// stands at the path otherwise. An `Edit` leaves the file with the
    /// replacement made, so a line the file already carries is counted
    /// once on both sides and only the edit's own addition stands out; an
    /// `old_string` the file has not is an edit that cannot apply, and
    /// what it would write is read as if it were appended, which denies
    /// where reading it alone would excuse.
    pub(super) fn rewrite(
        &self,
        root: &Path,
        path: Option<&Path>,
    ) -> Option<(Vec<String>, String)> {
        let standing = path
            .map(|path| fs::read_to_string(root.join(path)).unwrap_or_default())
            .unwrap_or_default();
        if let Some(content) = self.tool_input.content.as_deref() {
            let before = match &self.displaced[..] {
                [] => vec![standing],
                displaced => displaced.to_vec(),
            };
            return Some((before, content.to_owned()));
        }
        let new = self.tool_input.new_string.as_deref()?;
        let old = self.tool_input.old_string.as_deref().unwrap_or_default();
        let after = if !old.is_empty() && standing.contains(old) {
            if self.tool_input.replace_all {
                standing.replace(old, new)
            } else {
                standing.replacen(old, new, 1)
            }
        } else {
            format!("{standing}{new}")
        };
        Some((vec![standing], after))
    }

    /// Whether the runtime runs the call in the background and notifies
    /// its end: a `Bash` call with `run_in_background`, or a `Monitor`,
    /// which runs so by construction (#3637). A `Monitor` is known only by
    /// its name: the runtime hands a hook whose matcher names `Bash` alone
    /// the `Monitor`'s `command` and `description` under the name `Bash`,
    /// nothing else of it, which no judge tells from a foreground call, and
    /// hands one whose matcher names `Monitor` the call as it is (#3781).
    pub(super) fn in_background(&self) -> bool {
        self.tool_input.run_in_background || self.tool_name == "Monitor"
    }

    /// The name a rule's `tool` reads the call by: a `Monitor` that runs a
    /// command is the `Bash` call it runs, as the runtime reads it when it
    /// matches a hook, so every shell rule judges what a `Monitor` runs;
    /// every other call goes by its `tool_name` (#3781).
    pub(super) fn judged_as(&self) -> &str {
        match (self.tool_name.as_str(), self.command()) {
            ("Monitor", Some(_)) => "Bash",
            (name, _) => name,
        }
    }

    /// Whether the call runs in a subagent: an `agent_id` the hook set,
    /// which the main loop's calls carry none of.
    pub(super) fn in_subagent(&self) -> bool {
        self.agent_id.as_deref().is_some_and(|id| !id.is_empty())
    }

    pub(super) fn command(&self) -> Option<&str> {
        self.tool_input.command.as_deref()
    }

    /// The file the call touches, relative to the workspace root; `None`
    /// for a call without one or for a file outside the workspace, which
    /// no rule of the manifest describes. Read off [`Call::file`], so
    /// `seams.toml`, `./seams.toml` and `docs/../seams.toml` are one path
    /// and every scope sees it as such (#4430).
    pub fn path(&self, root: &Path) -> Option<PathBuf> {
        let file = self.file(root)?;
        Some(file.strip_prefix(canonical(root)).ok()?.to_path_buf())
    }

    /// The call, and the `text` it was read from, returned, with a
    /// relative `file_path` made the absolute file it names
    /// ([`Call::file`]), once, where the call comes in: the workspace that
    /// places the call, the scopes that judge it, the reading that names
    /// it and the worktree's build it is handed to each read the path
    /// under a root of its own, to which a relative one would be a place
    /// of its own (#4430). Both as they came where the path is absent or
    /// absolute already, or resolves to a name that is not UTF-8.
    pub(super) fn anchor(&mut self, root: &Path, text: String) -> String {
        let given = self.tool_input.file_path.as_deref();
        let relative = given.is_some_and(|file| Path::new(file).is_relative());
        let Some(file) = self.file(root).filter(|_| relative) else {
            return text;
        };
        let Ok(file) = file.into_os_string().into_string() else {
            return text;
        };
        self.tool_input.file_path = Some(file.clone());
        let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) else {
            return text;
        };
        let Some(slot) = (json.get_mut("tool_input")).and_then(|input| input.get_mut("file_path"))
        else {
            return text;
        };
        *slot = serde_json::Value::String(file);
        json.to_string()
    }

    /// The file the call touches as the one place it names, absolute and
    /// [`canonical`]: a relative `file_path` is read from the directory the
    /// call runs in, the hook's `cwd` where it carries one and `root`
    /// otherwise, as the tool that runs it reads it, and an absolute one
    /// as it stands. Every spelling of a file is that file, a `..` climbing
    /// out of `root` among them, which names the place it climbs to and
    /// is judged where that place is (#4430). `None` for a call without
    /// one.
    pub(super) fn file(&self, root: &Path) -> Option<PathBuf> {
        let given = Path::new(self.tool_input.file_path.as_deref()?);
        let from = match self.cwd.as_deref() {
            Some(cwd) => root.join(cwd),
            None => root.to_path_buf(),
        };
        Some(canonical(&from.join(given)))
    }

    /// The workspace whose manifest judges the call: the nearest ancestor
    /// of the file, or failing that of the call's `cwd`, that carries a
    /// `seams.toml` of its own under `root` or in another checkout of
    /// `root`'s repository ([`nearest`]), else `root`. A directory with a
    /// manifest is a workspace of its own, and a call into it is judged
    /// by that manifest (#57): a worktree of the repository, wherever it
    /// was laid down, whose files are judged against that checkout and
    /// not as `.claude/**` of the main one or as no file of the manifest
    /// at all, and a fixture under `xtask/fixtures/**` that carries one,
    /// whose sources are what its own rules describe and not what the
    /// workspace's do. One rule covers both, and a fixture without a
    /// manifest is the workspace's.
    pub fn workspace(&self, root: &Path) -> PathBuf {
        let root = canonical(root);
        self.nested(&root).unwrap_or(root)
    }

    /// The workspace of its own the call lands in, `None` for a call
    /// judged by `root`'s manifest. The file places the call, wherever it
    /// is: a file in `root` is `root`'s whatever directory the shell stands
    /// in, since a worktree's build handed a call in its own tree would
    /// otherwise place it by the session's directory in the checkout that
    /// handed it over (#1047). A call without one in a subagent, one whose
    /// `agent_id` is set, is `root`'s wherever its shell stands, so a
    /// builder whose shell starts in its worktree is judged by the session
    /// root's manifest and build, and a branch's `seams.toml` never judges
    /// its own builder's lines (Round 3, Q14 (a′)); [`Call::start`] still
    /// reads the line from that worktree. Any other call without one is
    /// placed by its `cwd` in a checkout alone, the nearest
    /// ancestor with a manifest that also has a `.git`: a `Bash` call whose
    /// shell stands in a fixture directory is the checkout's, whose
    /// manifest carries the shell rules, since a fixture's manifest
    /// describes the fixture's sources and not what a shell may run. A
    /// fixture's manifest itself, one in a directory with no `.git`, is
    /// test data and not that workspace's harness file, so it is placed
    /// from the directory above (#3644 ruling 2): the guard on a negative
    /// control is its test, every rule having a fixture that must fail.
    pub(super) fn nested(&self, root: &Path) -> Option<PathBuf> {
        let root = canonical(root);
        match (self.file(&root), self.cwd.as_deref()) {
            (Some(file), _) => {
                let place = match file.parent() {
                    Some(dir)
                        if file.file_name() == Some(manifest::FILE.as_ref())
                            && dir != root
                            && !dir.join(".git").exists() =>
                    {
                        dir.parent().unwrap_or(dir)
                    }
                    _ => &file,
                };
                nearest(&root, place, &[manifest::FILE])
            }
            (None, Some(_)) if self.in_subagent() => None,
            (None, Some(cwd)) => nearest(
                &root,
                &canonical(&root.join(cwd)),
                &[manifest::FILE, ".git"],
            ),
            (None, None) => None,
        }
    }

    /// The checkout the call's workspace is in: the nearest ancestor of it
    /// that has a `.git` ([`nearest`]), else `root`. A worktree is one; a
    /// fixture workspace is not, so the build that judges by its manifest
    /// is held to the checkout's, the one it could have been compiled
    /// against, and its readings go to the checkout's store.
    pub(super) fn checkout(&self, root: &Path) -> PathBuf {
        let root = canonical(root);
        self.nested(&root)
            .and_then(|workspace| nearest(&root, &workspace, &[".git"]))
            .unwrap_or(root)
    }

    /// Where the walk of the call's shell line starts
    /// ([`super::shell::Walk`]): the one start that `judge`, the checkout
    /// `permit` holds a `Bash` call in before and after it, and the write
    /// the Stop keeps are read from (Round 1, Q17 (a)). The `workspace`
    /// whose manifest judges the call, for a call with no line and for one
    /// whose `cwd` stands in no checkout of its repository; the `cwd`,
    /// resolved, for a line whose shell stands under that workspace or in
    /// another checkout of the repository ([`nearest`]), such as the
    /// worktree of a subagent whose line the session root judges
    /// ([`Call::nested`]), so `cd ..` and a relative word are read from
    /// where the shell is. Told by the paths and `.git` files alone, with
    /// no subprocess.
    pub(super) fn start(&self, workspace: &Path) -> PathBuf {
        let Some(cwd) = self.cwd.as_deref().filter(|_| self.command().is_some()) else {
            return workspace.to_path_buf();
        };
        let (cwd, top) = (canonical(&workspace.join(cwd)), canonical(workspace));
        let placed = cwd.starts_with(&top) || nearest(&top, &cwd, &[".git"]).is_some();
        if placed && cwd != top {
            cwd
        } else {
            workspace.to_path_buf()
        }
    }

    /// What a reading names: the path, or the command's first line.
    pub(super) fn subject(&self, root: &Path) -> String {
        match (self.path(root), self.command()) {
            (Some(path), _) => path.display().to_string(),
            (None, Some(command)) => {
                let line = command.lines().next().unwrap_or_default();
                let mut head: String = line.chars().take(80).collect();
                if head.len() < line.len() {
                    head.push('…');
                }
                head
            }
            (None, None) => String::new(),
        }
    }
}

/// The nearest directory at or above `place` that holds every one of
/// `names`, looked for under `root` ([`nearest_under`]) or, for a place
/// outside it, under the checkout `place` is in when that checkout is a
/// worktree of `root`'s repository with a manifest of its own, the
/// checkout itself failing a nearer one. Which checkout that is, git's
/// own `.git` says ([`git::common_dir`]) and not where worktrees are
/// laid down, so a worktree beside the checkout and one under
/// `.claude/worktrees` are found alike (harness-design.md §5, "A track's
/// worktrees"); a place in no checkout of the repository has none, and
/// no rule of the manifest describes it.
fn nearest(root: &Path, place: &Path, names: &[&str]) -> Option<PathBuf> {
    if place.starts_with(root) {
        return nearest_under(root, place, names);
    }
    let checkout = place.ancestors().find(|dir| dir.join(".git").exists())?;
    let ours = checkout.join(manifest::FILE).is_file()
        && git::common_dir(checkout).is_some_and(|common| git::common_dir(root) == Some(common));
    ours.then(|| nearest_under(checkout, place, names).unwrap_or_else(|| checkout.to_path_buf()))
}

/// The nearest directory at or above `place` that is strictly under
/// `root` and holds every one of `names`: a workspace by its manifest, a
/// checkout by its `.git`. `root` itself is never the answer, being the
/// fallback; a place outside `root` has none.
fn nearest_under(root: &Path, place: &Path, names: &[&str]) -> Option<PathBuf> {
    place
        .ancestors()
        .take_while(|dir| *dir != root)
        .find(|dir| dir.starts_with(root) && names.iter().all(|name| dir.join(name).exists()))
        .map(Path::to_path_buf)
}

/// The path with its deepest existing ancestor resolved, so that a file
/// under `/tmp` and one under `/private/tmp` compare equal on macOS and a
/// file that does not exist yet still has a place. Read a component at a
/// time from the left, each prefix resolved where it is on disk, so a `..`
/// steps up from the place a link leads, as the kernel steps, and steps
/// up lexically past a directory not made yet: no `.` or `..` is left in
/// what it returns, for `docs/../seams.toml` and `new/../seams.toml` alike
/// (#4430).
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut place = PathBuf::new();
    let mut on_disk = true;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                place.pop();
                on_disk = true;
            }
            other => {
                place.push(other);
                if on_disk {
                    match place.canonicalize() {
                        Ok(real) => place = real,
                        Err(_) => on_disk = false,
                    }
                }
            }
        }
    }
    place
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::Call;
    use crate::manifest;
    use crate::permit::argv::Line;
    use crate::permit::shell::Walk;
    use crate::scratch::Tree;

    /// The call as the hook hands it: a `Bash` line in `cwd`, or an `Edit`
    /// with no line.
    fn bash(cwd: &Path, command: Option<&str>) -> Call {
        let input = match command {
            Some(command) => serde_json::json!({"command": command}),
            None => serde_json::json!({"file_path": "x", "old_string": "a", "new_string": "b"}),
        };
        let tool = if command.is_some() { "Bash" } else { "Edit" };
        let call = serde_json::json!({"tool_name": tool, "cwd": cwd.to_str().unwrap(),
            "tool_input": input});
        Call::parse(&call.to_string()).unwrap()
    }

    /// A checkout `root` with a manifest, and a worktree of its repository
    /// beside it with a manifest of its own and a directory `sub`, laid by
    /// the `.git` files git reads and no git, both canonical.
    fn beside(tree: &Tree) -> (PathBuf, PathBuf) {
        let (root, worktree) = (tree.join("repo"), tree.join("repo-worktrees/t-x"));
        let link = root.join(".git/worktrees/t-x");
        for dir in [&link, &worktree.join("sub")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(link.join("commondir"), "../..\n").unwrap();
        let gitdir = format!("gitdir: {}\n", link.display());
        std::fs::write(worktree.join(".git"), gitdir).unwrap();
        for checkout in [&root, &worktree] {
            std::fs::write(checkout.join(manifest::FILE), "").unwrap();
        }
        (
            root.canonicalize().unwrap(),
            worktree.canonicalize().unwrap(),
        )
    }

    /// The call `bash` builds, in the subagent `agent`.
    fn in_agent(mut call: Call, agent: &str) -> Call {
        call.agent_id = Some(agent.to_owned());
        call
    }

    /// Round 3, Q14 (a′): a subagent's line whose shell stands in a
    /// worktree is judged at the session root, by its manifest and in its
    /// checkout, and is still read from where its shell stands; the main
    /// loop's is the worktree's, as before.
    #[test]
    fn a_subagents_line_in_a_worktree_is_judged_at_the_session_root() {
        let tree = Tree::new("call-nested").unwrap();
        let (root, worktree) = beside(&tree);
        let sub = worktree.join("sub");
        let agents = in_agent(bash(&sub, Some("git status")), "a1b2c3");
        assert_eq!(agents.nested(&root), None);
        assert_eq!(agents.workspace(&root), root);
        assert_eq!(agents.checkout(&root), root);
        assert_eq!(agents.start(&agents.workspace(&root)), sub);
        let mains = bash(&sub, Some("git status"));
        assert_eq!(mains.workspace(&root), worktree);
        assert_eq!(mains.checkout(&root), worktree);
        assert_eq!(mains.start(&mains.workspace(&root)), sub);
    }

    /// Round 3, Q14 (a′): a subagent's line with no `cwd`, or one whose
    /// `cwd` stands in no checkout of the repository, is judged and starts
    /// at the session root, as the main loop's; a subagent's file still
    /// places its call in the worktree it is in.
    #[test]
    fn a_subagents_call_with_no_cwd_outside_the_workspace_or_with_a_file_is_placed_as_before() {
        let tree = Tree::new("call-nested-else").unwrap();
        let (root, worktree) = beside(&tree);
        let mut unplaced = in_agent(bash(&root, Some("git status")), "a1b2c3");
        unplaced.cwd = None;
        assert_eq!(unplaced.workspace(&root), root);
        assert_eq!(unplaced.start(&root), root);
        let outside = tree.canonicalize().unwrap();
        for call in [bash(&outside, Some("ls")), bash(Path::new("/"), Some("ls"))] {
            for call in [in_agent(call.clone(), "a1b2c3"), call] {
                assert_eq!(call.workspace(&root), root);
                assert_eq!(call.start(&root), root);
            }
        }
        let mut edit = in_agent(bash(&worktree, None), "a1b2c3");
        edit.tool_input.file_path = Some(worktree.join("sub/x").display().to_string());
        assert_eq!(edit.workspace(&root), worktree);
        assert_eq!(edit.checkout(&root), worktree);
    }

    /// An `Edit` of `file`, read from `cwd` where it is given.
    fn edit(file: &str, cwd: Option<&Path>) -> Call {
        let mut call = bash(cwd.unwrap_or(Path::new("/")), None);
        call.tool_input.file_path = Some(file.to_owned());
        call.cwd = cwd.map(|cwd| cwd.display().to_string());
        call
    }

    /// #4430: every spelling of a file is the one path, a `..` through a
    /// directory on disk, one not made yet and one under a link alike,
    /// read from the call's `cwd` where it carries one and from the root
    /// otherwise.
    #[test]
    fn every_spelling_of_a_file_is_one_path() {
        let tree = Tree::new("call-spelling").unwrap();
        let (root, _) = beside(&tree);
        std::fs::create_dir_all(root.join("docs/deep")).unwrap();
        let seams = Some(PathBuf::from(manifest::FILE));
        for file in [
            "seams.toml",
            "./seams.toml",
            "docs/../seams.toml",
            "docs/deep/../../seams.toml",
            "not-made-yet/../seams.toml",
            "not/made/../../seams.toml",
            "docs/./../docs/.././seams.toml",
        ] {
            assert_eq!(edit(file, None).path(&root), seams, "{file}");
        }
        let absolute = format!("{}/not-made-yet/../seams.toml", root.display());
        assert_eq!(edit(&absolute, None).path(&root), seams);
        let deep = root.join("docs/deep");
        assert_eq!(edit("../../seams.toml", Some(&deep)).path(&root), seams);
        assert_eq!(
            edit("x.md", Some(&deep)).path(&root),
            Some(PathBuf::from("docs/deep/x.md"))
        );
        // A `..` under a link steps up from where the link leads, as the
        // kernel does, and the absolute spelling is resolved the same.
        std::os::unix::fs::symlink(&deep, root.join("link")).unwrap();
        let through = |file: &str| edit(file, None).path(&root);
        assert_eq!(through("link/../x.md"), Some(PathBuf::from("docs/x.md")));
        let linked = format!("{}/link/../x.md", root.display());
        assert_eq!(through(&linked), Some(PathBuf::from("docs/x.md")));
    }

    /// #4430: a relative path that climbs out of the workspace names the
    /// place it climbs to, judged as that absolute path is: in a worktree
    /// of the repository, by that worktree's manifest and as its file;
    /// anywhere else, as no file of the manifest.
    #[test]
    fn a_relative_path_climbing_out_of_the_workspace_is_the_absolute_path_it_names() {
        let tree = Tree::new("call-climbing").unwrap();
        let (root, worktree) = beside(&tree);
        let named = worktree.join("seams.toml").display().to_string();
        let text =
            r#"{"tool_name":"Edit","tool_input":{"file_path":"../repo-worktrees/t-x/seams.toml"}}"#;
        let mut climbing = Call::parse(text).unwrap();
        let anchored = Call::parse(&climbing.anchor(&root, text.to_owned())).unwrap();
        let absolute = edit(&named, None);
        for call in [&climbing, &anchored, &absolute] {
            assert_eq!(call.tool_input.file_path.as_deref(), Some(named.as_str()));
            assert_eq!(call.workspace(&root), worktree);
            assert_eq!(call.checkout(&root), worktree);
            assert_eq!(call.path(&worktree), Some(PathBuf::from(manifest::FILE)));
            assert_eq!(call.path(&root), None);
        }
        let away = edit("../elsewhere/seams.toml", None);
        assert_eq!(away.workspace(&root), root);
        assert_eq!(away.path(&root), None);
        let sub = worktree.join("sub");
        let from_sub = edit("../../../repo/docs/../seams.toml", Some(&sub));
        assert_eq!(from_sub.workspace(&root), root);
        assert_eq!(from_sub.path(&root), Some(PathBuf::from(manifest::FILE)));
    }

    /// Round 1, Q17 (a): a call with no line, and a line whose `cwd`
    /// stands outside the workspace, start where the workspace is.
    #[test]
    fn a_call_with_no_line_or_a_cwd_outside_the_workspace_starts_at_the_workspace() {
        let tree = Tree::new("call-start").unwrap();
        let workspace = tree.join("workspace");
        std::fs::create_dir_all(workspace.join("sub")).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let outside = tree.canonicalize().unwrap();
        let start = |cwd: &Path, command| bash(cwd, command).start(&workspace);
        assert_eq!(start(&workspace.join("sub"), None), workspace);
        assert_eq!(start(&outside, Some("touch x")), workspace);
        assert_eq!(start(Path::new("/"), Some("touch x")), workspace);
    }

    /// Round 1, Q17 (a): a line whose `cwd` stands under the workspace
    /// starts there, resolved, and its walk reads `cd ..` and the places of
    /// its commands from there; the workspace itself is its own start.
    #[test]
    fn a_line_whose_cwd_stands_under_the_workspace_starts_there() {
        let tree = Tree::new("call-start-under").unwrap();
        let workspace = tree.join("workspace");
        std::fs::create_dir_all(workspace.join("sub/deeper")).unwrap();
        let sub = workspace.canonicalize().unwrap().join("sub");
        let call = |cwd: &Path, command| bash(cwd, Some(command));
        assert_eq!(
            call(&workspace.join("sub"), "touch x").start(&workspace),
            sub
        );
        assert_eq!(call(Path::new("sub"), "touch x").start(&workspace), sub);
        assert_eq!(call(&workspace, "touch x").start(&workspace), workspace);
        let (deeper, up) = (workspace.join("sub/deeper"), "cd .. && git status");
        let start = call(&deeper, up).start(&workspace);
        let stands = Walk::of(&start, Some(&Line::read(up))).runs_in();
        assert_eq!(stands.canonicalize().unwrap(), sub);
    }
}
