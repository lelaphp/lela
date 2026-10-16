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
/// a call without a file in a worktree; `session_id` is what a reading
/// carries so denials count per session (§10 rule 6); the other keys of
/// the recorded shape (`permission_mode`, …) are carried by the JSON and
/// not read.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Call {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Arguments,
    #[serde(default)]
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
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// The subagent the call runs in, absent on the main loop.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// How long the call ran, which the `PostToolUse` hook carries: what
    /// began during it began no earlier than this before the hook.
    #[serde(default)]
    pub(super) duration_ms: Option<u64>,
    /// The hook event the call came in on, which the hook's answer names
    /// again: `PostToolUse`, or `PostToolUseFailure` for a `Bash` call that
    /// exited non-zero, which gets no `PostToolUse` (#2762).
    #[serde(default)]
    pub(super) hook_event_name: Option<String>,
    /// The answered `AskUserQuestion` calls the function hook read from
    /// the engine, read only by that adapter ([`Evidence`]).
    #[serde(default)]
    pub(super) asked: Option<Vec<Asked>>,
    /// Which of the two the adapter judging the call reads.
    #[serde(skip)]
    pub(super) evidence: Evidence,
    /// The approvals, read once for every rule that asks.
    #[serde(skip)]
    approvals: OnceCell<Approvals>,
}

/// The fields of `tool_input` a rule can read: an `Edit`'s, a `Write`'s or
/// a `Bash` call's.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Arguments {
    pub file_path: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
    #[serde(default)]
    pub replace_all: bool,
    pub content: Option<String>,
    pub command: Option<String>,
}

impl Call {
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

    pub(super) fn command(&self) -> Option<&str> {
        self.tool_input.command.as_deref()
    }

    /// The file the call touches, relative to the workspace root; `None`
    /// for a call without one or for a file outside the workspace, which
    /// no rule of the manifest describes.
    pub fn path(&self, root: &Path) -> Option<PathBuf> {
        let given = Path::new(self.tool_input.file_path.as_deref()?);
        let relative = if given.is_absolute() {
            let root = canonical(root);
            canonical(given).strip_prefix(&root).ok()?.to_path_buf()
        } else {
            given.to_path_buf()
        };
        let clean: PathBuf = relative
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        Some(clean)
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
    /// handed it over (#1047). A call without one is placed by its `cwd`
    /// in a checkout alone, the nearest
    /// ancestor with a manifest that also has a `.git`: a `Bash` call whose
    /// shell stands in a fixture directory is the checkout's, whose
    /// manifest carries the shell rules, since a fixture's manifest
    /// describes the fixture's sources and not what a shell may run.
    pub(super) fn nested(&self, root: &Path) -> Option<PathBuf> {
        let root = canonical(root);
        match (self.tool_input.file_path.as_deref(), self.cwd.as_deref()) {
            (Some(file), _) => nearest(&root, &canonical(&root.join(file)), &[manifest::FILE]),
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
/// file that does not exist yet still has a place.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_path_buf(),
    }
}
