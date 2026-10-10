//! The git operation in progress in a checkout, and whose it is (#2476,
//! harness-design.md §4.4). A merge, cherry-pick, revert, rebase or `am`
//! in progress ends in a commit of the whole index, so a path another
//! session stages in that checkout lands in that commit: on 2026-10-09 a
//! settled divergence row staged by pathspec in main's checkout rode into
//! `1bd555d0`, an unrelated merge. A rule carrying `where = "operating"`
//! judges a call only where such an operation stands and the calling
//! session did not start it ([`foreign`]), and its denial names the
//! operation and the checkout.
//!
//! Whose an operation is cannot be read from git, so permit records it at
//! the one moment the starting session is known: when it admits the `git
//! merge`, `pull`, `cherry-pick`, `revert` or `rebase` that starts it in a
//! checkout where none stands ([`record`]). The record names the session
//! and its subagent, and the commit `HEAD` stands on, which is the
//! operation's base: `HEAD` itself while a merge or one pick stands,
//! `sequencer/head` through a sequence and `orig-head` through a rebase.
//! It lives in the main worktree's store beside the approvals' audit,
//! `permit/operations/`, one file per git dir, so a linked worktree's
//! operation is its own. An operation is the caller's only where the
//! record names the caller and the operation stands on the record's base.
//! Every other is foreign and refused: one started outside permit, in a
//! person's terminal or by a hook, which has no record; an `am`, which
//! keeps no base; one another session started while a record stood, since
//! no record is taken where an operation already stands.
//!
//! A record is dropped by the next line that runs git in its checkout and
//! finds nothing standing, since what it named has ended or never began (a
//! merge already up to date, a call the person refused, one a `reset`
//! ended), and by a `--abort` or `--quit`. What that leaves open, stated:
//! between a start that began nothing and the next git line in that
//! checkout, an operation another hand starts outside permit on the same
//! `HEAD` would read as the recorded session's. A rebase whose `orig-head`
//! is not the `HEAD` it was started on (`git rebase <upstream> <branch>`)
//! is refused to its own starter, the safe side. The recording runs only
//! where the manifest carries such a rule, so the mechanism is inert until
//! a person lands the row.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::argv::{Argv, Line};
use super::call::Call;
use super::rules::runs_in;
use crate::reading::{self, Store};
use crate::{git, track};

/// An operation that ends in a commit of the whole index, by the marker
/// git keeps in the checkout's git dir while it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Merge,
    CherryPick,
    Revert,
    Rebase,
    /// `git am`, which stands in `rebase-apply/` beside its `applying`
    /// marker and keeps no `orig-head`, so it is never a session's own.
    Am,
}

impl Operation {
    /// The operation standing in `git_dir`, if one does.
    fn in_progress(git_dir: &Path) -> Option<Operation> {
        [
            ("MERGE_HEAD", Operation::Merge),
            ("CHERRY_PICK_HEAD", Operation::CherryPick),
            ("REVERT_HEAD", Operation::Revert),
            ("rebase-apply/applying", Operation::Am),
            ("rebase-merge", Operation::Rebase),
            ("rebase-apply", Operation::Rebase),
        ]
        .into_iter()
        .find(|(marker, _)| git_dir.join(marker).exists())
        .map(|(_, operation)| operation)
    }

    /// The commit the operation started from, as git keeps it.
    fn base(self, git_dir: &Path, checkout: &Path) -> Option<String> {
        let read = |file: &str| {
            fs::read_to_string(git_dir.join(file))
                .ok()
                .map(|text| text.trim().to_owned())
        };
        match self {
            Operation::Am => None,
            Operation::Rebase => {
                read("rebase-merge/orig-head").or_else(|| read("rebase-apply/orig-head"))
            }
            Operation::Merge | Operation::CherryPick | Operation::Revert => {
                read("sequencer/head").or_else(|| head(checkout))
            }
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Operation::Merge => "a merge",
            Operation::CherryPick => "a cherry-pick",
            Operation::Revert => "a revert",
            Operation::Rebase => "a rebase",
            Operation::Am => "an am",
        })
    }
}

/// The git subcommands that start an operation a session may own: each
/// leaves one of [`Operation`]'s markers but `am`'s, a `pull` a merge's or
/// a rebase's.
const STARTING: [&str; 5] = [
    "git merge",
    "git pull",
    "git cherry-pick",
    "git revert",
    "git rebase",
];

/// An operation standing in the checkout a call runs git in, which the
/// calling session did not start: what a `where = "operating"` rule
/// refuses, said in its denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Foreign {
    pub(crate) operation: Operation,
    pub(crate) checkout: PathBuf,
}

impl fmt::Display for Foreign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is in progress in {} and this session did not start it.",
            self.operation,
            self.checkout.display()
        )
    }
}

/// The session a call is made in, and the subagent within it; `None` for a
/// call that names no session, which is never an operation's starter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Session {
    id: String,
    agent: Option<String>,
}

impl Session {
    fn of(call: &Call) -> Option<Session> {
        (!call.session_id.is_empty()).then(|| Session {
            id: call.session_id.clone(),
            agent: call.agent_id.clone(),
        })
    }
}

/// Who started an operation, and the commit it started from: one file of
/// `permit/operations/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Starter {
    session: Session,
    base: String,
}

/// The operation standing where a command of `line` that `fires` runs
/// git, from `root` or the directory its leading `cd` and git's own `-C`
/// name, when the session making `call` did not start it; `None` where
/// nothing stands or it is the caller's own. A call with no line is judged
/// in `root`.
pub(crate) fn foreign(
    root: &Path,
    line: Option<&Line>,
    call: &Call,
    fires: impl Fn(&Argv) -> bool,
) -> Option<Foreign> {
    let checkouts: Vec<PathBuf> = match line {
        Some(line) => line
            .commands()
            .iter()
            .filter(|argv| fires(argv))
            .map(|argv| checkout_of(root, Some(line), argv))
            .collect(),
        None => vec![root.to_path_buf()],
    };
    let caller = Session::of(call);
    checkouts.into_iter().find_map(|checkout| {
        let git_dir = git_dir(&checkout)?;
        let operation = Operation::in_progress(&git_dir)?;
        let ours = read(&starter_file(&checkout, &git_dir)).is_some_and(|starter| {
            caller.as_ref() == Some(&starter.session)
                && operation.base(&git_dir, &checkout).as_ref() == Some(&starter.base)
        });
        (!ours).then_some(Foreign {
            operation,
            checkout,
        })
    })
}

/// What `line` does to the records of the checkouts it runs git in: each
/// git command that finds nothing standing drops the record, as does an
/// `--abort` or `--quit`; then the session making `call` is recorded as
/// the starter of each operation the line starts where none stands. A
/// command run while an operation stands, its `--continue`, `--skip` or
/// `--edit-todo` among them, leaves the record as it is.
pub(crate) fn record(root: &Path, call: &Call, line: &Line) -> Result<()> {
    let mut starting = Vec::new();
    for git in line.commands().iter().filter(|argv| argv.runs("git")) {
        let checkout = checkout_of(root, Some(line), git);
        let Some(git_dir) = git_dir(&checkout) else {
            continue;
        };
        let ends = ["--abort", "--quit"]
            .iter()
            .any(|flag| git.words().iter().any(|w| w == flag));
        let standing = Operation::in_progress(&git_dir).is_some();
        if !standing || ends {
            let file = starter_file(&checkout, &git_dir);
            match fs::remove_file(&file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(err).with_context(|| format!("removing {}", file.display()));
                }
                _ => {}
            }
        }
        if !standing && !ends && STARTING.iter().any(|phrase| git.runs(phrase)) {
            starting.push((checkout, git_dir));
        }
    }
    let Some(session) = Session::of(call) else {
        return Ok(());
    };
    for (checkout, git_dir) in starting {
        let Some(base) = head(&checkout) else {
            continue;
        };
        let starter = Starter {
            session: session.clone(),
            base,
        };
        let text = serde_json::to_string(&starter).context("rendering the operation's starter")?;
        reading::replace(
            &starter_file(&checkout, &git_dir),
            format!("{text}\n").as_bytes(),
        )?;
    }
    Ok(())
}

/// The checkout a command of `line` runs in: the call's, moved by a
/// leading `cd`, then, for git, by each `-C` among the options before its
/// subcommand, as git reads them; a `-C` after the subcommand is the
/// subcommand's own (`git commit -C HEAD`).
fn checkout_of(root: &Path, line: Option<&Line>, argv: &Argv) -> PathBuf {
    let mut dir = runs_in(root, line);
    if argv.program() != "git" {
        return dir;
    }
    let mut words = argv.words().iter().skip(1);
    while let Some(word) = words.next() {
        match word.as_str() {
            "-C" => {
                if let Some(to) = words.next() {
                    dir = dir.join(to);
                }
            }
            "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" => {
                words.next();
            }
            option if option.starts_with('-') => {}
            _ => break,
        }
    }
    dir
}

/// The checkout a call runs in: its `workspace` moved by a leading `cd`
/// ([`runs_in`]), at the top of its worktree, a linked one's own; the
/// workspace where the `cd` names no checkout. What `permit --after`
/// holds, and the one checkout it puts a file back in (#2597). A `git -C`
/// moves one command and not the call, so it is not read here.
pub(super) fn ran_in(workspace: &Path, line: Option<&Line>) -> PathBuf {
    let top = git::git(
        &runs_in(workspace, line),
        &["rev-parse", "--show-toplevel"],
        None,
    );
    let top = top.map_or_else(|_| workspace.to_path_buf(), |top| PathBuf::from(top.trim()));
    fs::canonicalize(&top).unwrap_or(top)
}

/// The operation standing in `checkout` that has staged `path`, its index
/// entry other than `HEAD`'s: the operation's commit carries it, so
/// `permit --after` never puts it back (#2597). An index git cannot read
/// is read as staged, the side that puts nothing back. What the index
/// cannot say is whose hand staged the entry, so a call's own `git add`
/// while the operation stands is read as the operation's and rides its
/// commit; another session's staging there is what a `where =
/// "operating"` rule refuses at the call ([`foreign`]).
pub(super) fn staged(checkout: &Path, path: &str) -> Option<Operation> {
    let operation = Operation::in_progress(&git_dir(checkout)?)?;
    let args = ["diff", "--cached", "--name-only", "-z", "--", path];
    let differs = git::git(checkout, &args, None).map_or(true, |d| !d.is_empty());
    differs.then_some(operation)
}

/// The checkout's own git dir, a linked worktree's under the main one's
/// `worktrees/`, with its links resolved so `/tmp` and `/private/tmp` name
/// one; `None` outside a repository.
fn git_dir(checkout: &Path) -> Option<PathBuf> {
    let dir = git::git(checkout, &["rev-parse", "--absolute-git-dir"], None).ok()?;
    let dir = PathBuf::from(dir.trim());
    Some(fs::canonicalize(&dir).unwrap_or(dir))
}

fn head(checkout: &Path) -> Option<String> {
    git::git(
        checkout,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        None,
    )
    .ok()
    .map(|head| head.trim().to_owned())
}

/// Where the starter of `git_dir`'s operation is recorded: the main
/// worktree's store, named by the git dir.
fn starter_file(checkout: &Path, git_dir: &Path) -> PathBuf {
    let main = track::main_worktree(checkout).unwrap_or_else(|_| checkout.to_path_buf());
    let name = git::blob_id(git_dir.to_string_lossy().as_bytes());
    Store::of_workspace(&main)
        .root()
        .join("permit")
        .join("operations")
        .join(format!("{name}.json"))
}

fn read(file: &Path) -> Option<Starter> {
    serde_json::from_str(&fs::read_to_string(file).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::{Foreign, Line, Operation, foreign, record};
    use crate::permit::call::Call;
    use crate::scratch::Tree;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=x", "-c", "user.email=x@x"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/var/empty",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {}", args.join(" "));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A repository of one commit on `main`, with the merge marker git
    /// writes when a merge stops before its commit.
    fn repository(name: &str) -> Tree {
        let dir = Tree::new(name).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        dir
    }

    fn merging(dir: &Path) {
        let head = git(dir, &["rev-parse", "HEAD"]);
        let git_dir = git(dir, &["rev-parse", "--absolute-git-dir"]);
        std::fs::write(Path::new(&git_dir).join("MERGE_HEAD"), head + "\n").unwrap();
    }

    fn ended(dir: &Path) {
        let git_dir = git(dir, &["rev-parse", "--absolute-git-dir"]);
        std::fs::remove_file(Path::new(&git_dir).join("MERGE_HEAD")).unwrap();
    }

    fn call(session: &str, agent: Option<&str>) -> Call {
        let call =
            serde_json::json!({"tool_name": "Bash", "session_id": session, "agent_id": agent});
        Call::parse(&call.to_string()).unwrap()
    }

    fn standing(dir: &Path, line: &str, session: &str, agent: Option<&str>) -> Option<Operation> {
        let line = Line::read(line);
        foreign(dir, Some(&line), &call(session, agent), |_| true).map(|f| f.operation)
    }

    /// No marker, nothing stands; a merge permit saw no one start is
    /// foreign to every session, a person's terminal's or a hook's, and
    /// a session that runs `git merge` while it stands takes no record.
    /// Once a session's `git merge` is recorded where nothing stood, the
    /// merge is that session's alone: not another's, not its own
    /// subagent's, never a call naming no session. What it runs while
    /// the merge stands keeps the record, and its abort drops it.
    #[test]
    fn a_merge_is_the_session_s_that_started_it_and_foreign_to_every_other() {
        let dir = repository("operation-merge");
        let add = "git add -- a.txt";
        assert_eq!(standing(&dir, add, "s1", None), None);
        merging(&dir);
        assert_eq!(standing(&dir, add, "s1", None), Some(Operation::Merge));
        record(&dir, &call("s2", None), &Line::read("git merge x")).unwrap();
        assert_eq!(standing(&dir, add, "s2", None), Some(Operation::Merge));
        ended(&dir);

        let start = Line::read("git merge --no-commit topic");
        record(&dir, &call("s1", None), &start).unwrap();
        merging(&dir);
        assert_eq!(standing(&dir, add, "s1", None), None);
        assert_eq!(standing(&dir, "git commit -F m", "s1", None), None);
        assert_eq!(standing(&dir, add, "s2", None), Some(Operation::Merge));
        let subagent = standing(&dir, add, "s1", Some("a1"));
        assert_eq!(subagent, Some(Operation::Merge));
        assert_eq!(standing(&dir, add, "", None), Some(Operation::Merge));
        let said = foreign(&dir, Some(&Line::read(add)), &call("s2", None), |_| true).unwrap();
        assert_eq!(
            said.to_string(),
            format!(
                "a merge is in progress in {} and this session did not start it.",
                dir.display()
            )
        );

        for line in ["git merge --continue", "git status", "git merge y"] {
            record(&dir, &call("s2", None), &Line::read(line)).unwrap();
            assert_eq!(standing(&dir, add, "s1", None), None, "{line} keeps it");
        }
        record(&dir, &call("s1", None), &Line::read("git merge --abort")).unwrap();
        assert_eq!(standing(&dir, add, "s1", None), Some(Operation::Merge));
    }

    /// A start that began nothing, a merge already up to date, leaves a
    /// record the next git line in the checkout drops, so a merge a person
    /// then starts on the same `HEAD` is not the session's.
    #[test]
    fn a_record_the_next_git_line_finds_nothing_standing_for_is_dropped() {
        let dir = repository("operation-nothing");
        record(&dir, &call("s1", None), &Line::read("git merge topic")).unwrap();
        record(&dir, &call("s2", None), &Line::read("git log -1")).unwrap();
        merging(&dir);
        assert_eq!(
            standing(&dir, "git add -- a", "s1", None),
            Some(Operation::Merge)
        );
    }

    /// A record outlives the operation it named, and the next operation
    /// stands on another base: once `HEAD` has moved, a merge started
    /// outside permit is not the old starter's.
    #[test]
    fn a_record_whose_base_the_operation_does_not_stand_on_is_stale() {
        let dir = repository("operation-stale");
        record(&dir, &call("s1", None), &Line::read("git merge topic")).unwrap();
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "the merge"]);
        merging(&dir);
        assert_eq!(
            standing(&dir, "git add -- a", "s1", None),
            Some(Operation::Merge)
        );
    }

    /// A linked worktree's operation is its own: its git dir is under the
    /// main one's `worktrees/`, found from the `.git` file, and a leading
    /// `cd` or git's `-C` moves the call to the checkout it names.
    #[test]
    fn a_linked_worktree_s_operation_is_read_from_its_own_git_dir() {
        let dir = repository("operation-worktree");
        let linked = dir.join("linked");
        git(
            &dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "t1",
                linked.to_str().unwrap(),
            ],
        );
        merging(&dir);
        let add = "git add -- a";
        assert_eq!(standing(&linked, add, "s1", None), None);
        let moved = format!("cd {} && {add}", dir.display());
        assert_eq!(
            standing(&linked, &moved, "s1", None),
            Some(Operation::Merge)
        );
        let flagged = format!("git -C {} add -- a", dir.display());
        assert_eq!(
            standing(&linked, &flagged, "s1", None),
            Some(Operation::Merge)
        );
        // A `-C` after the subcommand is the subcommand's own.
        let reused = "git commit -C HEAD -- a";
        assert_eq!(standing(&dir, reused, "s1", None), Some(Operation::Merge));
        // The command the rule fires on is judged, not the line's first.
        let line = Line::read(&format!("git -C {} status && {add}", linked.display()));
        let fired = foreign(&dir, Some(&line), &call("s1", None), |a| a.runs("git add"));
        assert_eq!(fired.map(|f| f.operation), Some(Operation::Merge));

        merging(&linked);
        let Some(Foreign { checkout, .. }) = foreign(
            &dir,
            Some(&Line::read(&format!("cd {} && {add}", linked.display()))),
            &call("s1", None),
            |_| true,
        ) else {
            panic!("the linked worktree's merge stands");
        };
        assert!(checkout.ends_with("linked"), "{}", checkout.display());
    }

    /// The checkout a call ran in is its workspace moved by a leading
    /// `cd`, past the assignments before it, at the top of the worktree
    /// the `cd` names; a `cd` into no checkout leaves the workspace.
    #[test]
    fn a_call_ran_in_the_top_of_the_worktree_its_leading_cd_names() {
        let dir = repository("operation-ran-in");
        let base = Tree::new("operation-ran-in-linked").unwrap();
        let linked = base.join("t1");
        git(
            &dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "t1",
                linked.to_str().unwrap(),
            ],
        );
        std::fs::create_dir(linked.join("sub")).unwrap();
        let top = linked.canonicalize().unwrap();
        let ran_in = |line: &str| super::ran_in(&dir, Some(&Line::read(line)));
        let at = linked.display();
        assert_eq!(ran_in(&format!("cd {at}/sub && touch x")), top);
        assert_eq!(ran_in(&format!("S=/tmp/s && cd {at} && touch x")), top);
        let exported = format!("export A=1 B=2 && cd {at} && touch x");
        assert_eq!(ran_in(&exported), top);
        let workspace = dir.canonicalize().unwrap();
        assert_eq!(ran_in("touch x"), workspace);
        assert_eq!(ran_in("cd /var/empty/none && touch x"), workspace);
    }

    /// A rebase's base is `orig-head`, which `HEAD` leaves behind as the
    /// rebase applies its commits.
    #[test]
    fn a_rebase_stands_on_its_orig_head() {
        let dir = repository("operation-rebase");
        record(&dir, &call("s1", None), &Line::read("git rebase main")).unwrap();
        let base = git(&dir, &["rev-parse", "HEAD"]);
        let git_dir = git(&dir, &["rev-parse", "--absolute-git-dir"]);
        let rebase = Path::new(&git_dir).join("rebase-merge");
        std::fs::create_dir(&rebase).unwrap();
        std::fs::write(rebase.join("orig-head"), base + "\n").unwrap();
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "applied"]);
        assert_eq!(standing(&dir, "git add -- a", "s1", None), None);
        assert_eq!(
            standing(&dir, "git add -- a", "s2", None),
            Some(Operation::Rebase)
        );
        // An `am` stands in the same directory and keeps no base.
        std::fs::write(rebase.join("applying"), "").unwrap();
        std::fs::rename(&rebase, Path::new(&git_dir).join("rebase-apply")).unwrap();
        assert_eq!(
            standing(&dir, "git add -- a", "s1", None),
            Some(Operation::Am)
        );
    }
}
