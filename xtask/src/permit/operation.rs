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
//! is refused to its own starter, the safe side.
//!
//! A git command past a directory change the line makes after its first
//! command touches no record, since where it runs is not read (#2720): a
//! sibling agent's `cat f; cd <its worktree>; git merge --ff-only main`,
//! read in the main checkout its shell starts in, dropped a merge's record
//! there and took that merge as its own start. So a start written that
//! way is not recorded and is refused to its own starter, and a record
//! left by a start that began nothing on the same `HEAD` would claim it.
//! A git line of another hand read in the checkout between a start's
//! admission and the marker git writes still drops the starter's record,
//! and its start there takes it (#2723). The recording runs whatever the
//! manifest holds, since an operation can bring the first such
//! rule into its own working tree (#2694).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::argv::{Argv, Line};
use super::call::Call;
use super::rules::{acts_in, fired_in, placed};
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
/// git, from `root` moved by the line's leading `cd` or `pushd` and the
/// command's own `-C` ([`fired_in`]), when the session making `call` did
/// not start it; `None` where nothing stands or it is the caller's own. A
/// call with no line is judged in `root`.
pub(crate) fn foreign(
    root: &Path,
    line: Option<&Line>,
    call: &Call,
    fires: impl Fn(&Argv) -> bool,
) -> Option<Foreign> {
    let checkouts = match line {
        Some(line) => fired_in(root, line, fires),
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
/// git command whose place it reads ([`placed`]) and that finds nothing
/// standing drops the record, as does an `--abort` or `--quit`; then the
/// session making `call` is recorded as the starter of each operation the
/// line starts where none stands. A
/// command run while an operation stands, its `--continue`, `--skip` or
/// `--edit-todo` among them, leaves the record as it is.
pub(crate) fn record(root: &Path, call: &Call, line: &Line) -> Result<()> {
    let mut starting = Vec::new();
    for (git, checkout) in placed(root, line)
        .into_iter()
        .filter(|(argv, _)| argv.runs("git"))
    {
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

/// The checkout a call runs in: where it acts ([`acts_in`]), its
/// `workspace` moved by the line's leading `cd`, `pushd` and `popd`, or
/// the one directory git's `-C` names where every command past them is
/// git, at the top of its worktree, a linked one's own; the workspace
/// where that names no checkout, as it is for a `cd` to a variable the
/// line did not assign. What `permit --after` holds, and the one
/// checkout it puts a file back in (#2597, #2607). A line whose commands
/// run in several checkouts is held where its shell stands, since a
/// `-C` moves one command and the call is held in one.
pub(super) fn ran_in(workspace: &Path, line: Option<&Line>) -> PathBuf {
    let top = git::git(
        &acts_in(workspace, line),
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

    /// #2720: a sibling agent's line that `cd`s into its own worktree past
    /// its first command, and runs `git merge --ff-only` there, was read
    /// in the checkout its shell starts in. Judged between the starter's
    /// admitted `git merge` and the marker that merge writes, it dropped
    /// the starter's record and took the merge as its own start. A git
    /// command past a directory change the line makes after its first
    /// command does not run where its shell starts, and is not read
    /// there, a subshell's `cd` among them, so the merge stays its
    /// starter's to commit and abort and stays foreign to the sibling. A
    /// git command the line runs before any such change is read where the
    /// shell starts, as it was, and still drops the record.
    #[test]
    fn a_sibling_s_git_line_past_a_later_cd_leaves_the_starter_s_record() {
        let (starter, sibling) = (Some("the-starter-agent"), Some("a-sibling"));
        let dir = repository("operation-sibling");
        let start = Line::read("git merge --no-ff --no-commit topic");
        record(&dir, &call("s1", starter), &start).unwrap();
        for elsewhere in [
            format!(
                "S=/var/empty; cat $S/owner; cd /var/empty/t2645; git status --short; \
                 git merge --ff-only main 2>&1 | tail -1; cd {}; git log --oneline -3",
                dir.display()
            ),
            "(cd /var/empty/t2645 && git merge --ff-only main)".to_owned(),
        ] {
            record(&dir, &call("s1", sibling), &Line::read(&elsewhere)).unwrap();
        }
        merging(&dir);
        for line in ["git add -- a", "git commit -F m -- a", "git merge --abort"] {
            assert_eq!(standing(&dir, line, "s1", starter), None, "{line}");
            let other = standing(&dir, line, "s1", sibling);
            assert_eq!(other, Some(Operation::Merge), "{line}");
        }
        ended(&dir);
        let before_the_cd = "git log -1; cd /var/empty; git status";
        record(&dir, &call("s1", sibling), &Line::read(before_the_cd)).unwrap();
        merging(&dir);
        let own = standing(&dir, "git add -- a", "s1", starter);
        assert_eq!(own, Some(Operation::Merge));
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

    /// #2607: a `cd` to a variable the line assigned before it, plainly or
    /// by `export`, and a `pushd` move the call to the worktree they name,
    /// and a `popd` returns to where the `pushd` left; git's `-C` does
    /// where every command past them is git and names one directory. A
    /// change the line cannot read leaves it where it stood, the
    /// workspace for the first.
    #[test]
    fn a_call_ran_in_the_worktree_an_assigned_cd_a_pushd_or_git_s_c_names() {
        let dir = repository("operation-ran-in-shapes");
        let base = Tree::new("operation-ran-in-shapes-linked").unwrap();
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
        let workspace = dir.canonicalize().unwrap();
        let ran_in = |line: &str| super::ran_in(&dir, Some(&Line::read(line)));
        let at = linked.display();
        assert_eq!(ran_in(&format!("W={at} && cd \"$W\" && touch x")), top);
        assert_eq!(ran_in(&format!("W={at}; cd $W/sub; touch x")), top);
        let exported = format!("export A=1 W={at} && cd \"${{W}}\" && touch x");
        assert_eq!(ran_in(&exported), top);
        assert_eq!(ran_in("cd \"$UNSET_BY_THE_LINE\" && touch x"), workspace);
        assert_eq!(ran_in(&format!("W={at} cd \"$W\" && touch x")), workspace);
        assert_eq!(ran_in(&format!("pushd {at}/sub && touch x")), top);
        assert_eq!(ran_in(&format!("pushd {at} && popd && touch x")), workspace);
        assert_eq!(ran_in("popd && touch x"), workspace);
        let flagged = format!("git -C {at} checkout -- x && git -C {at} status");
        assert_eq!(ran_in(&flagged), top);
        let assigned = format!("export W={at} && git -C \"$W\" checkout -- x");
        assert_eq!(ran_in(&assigned), top);
        assert_eq!(ran_in(&format!("cd {at} && git -C sub status")), top);
        // A `-C` moves its command, not the shell: a line whose other
        // commands run where the shell stands is held there.
        let later = format!("touch x && git -C {at} status");
        assert_eq!(ran_in(&later), workspace);
        let beside = format!("git -C {at} status && git checkout -- x");
        assert_eq!(ran_in(&beside), workspace);
        let after = format!("git -C {at} status && touch x");
        assert_eq!(ran_in(&after), workspace);
        // A change the line cannot read leaves it where it stood.
        assert_eq!(ran_in(&format!("cd {at} && cd \"$UNSET\" && touch x")), top);
        assert_eq!(ran_in(&format!("cd {at} && popd && touch x")), top);
        assert_eq!(ran_in(&format!("pushd -n {at} && touch x")), workspace);
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
