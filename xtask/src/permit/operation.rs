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
//! Whose an operation is cannot be read from git, so permit records it
//! when the starting session is known: when the command hook admits the
//! `git merge`, `pull`, `cherry-pick`, `revert` or `rebase` that starts
//! it in a checkout where none stands ([`record`]), the function hook
//! above it admitting a call the command hook then judges again (#2734);
//! and when `permit --after` finds an operation that began while such a
//! start ran and that no record names ([`settle`]), a start admitted while
//! another operation stood (#2723). The record names the session and its
//! subagent, and the commit `HEAD` stands on, which is the operation's
//! base: `HEAD` itself while a merge or one pick stands, `sequencer/head`
//! through a sequence and `orig-head` through a rebase; and, once the
//! operation stands, its identity, what tells it from the next one on the
//! same base ([`Operation::identity`]). It lives in the main worktree's
//! store beside the approvals' audit, `permit/operations/`, one file per
//! git dir, so a linked worktree's operation is its own. An operation is
//! the caller's only where the record names the caller, its base and its
//! identity. Every other is foreign and refused: one started outside
//! permit, in a person's terminal or by a hook, which has no record; an
//! `am`, which keeps no base; one another session started while a record
//! stood.
//!
//! A record whose operation stands and that names another base or
//! identity is stale, and the next git line in that checkout drops it, as
//! it does one whose operation has ended; a start still waiting for git's
//! marker is dropped only by its own session's line or its own call's
//! end, or once `HEAD` leaves its base, never by another hand's line in
//! that window (#2723). So an operation's own `--abort` drops its record
//! once it has run, and one refused after the hook admitted it leaves the
//! starter its operation. What that leaves open, stated: a start that
//! began nothing and exited non-zero gets no `PostToolUse`, so until its
//! session runs git in that checkout again or `HEAD` moves, an operation
//! another hand starts outside permit on the same `HEAD` reads as that
//! session's. A start admitted while another operation stood that stops
//! non-zero, a merge on a conflict, is refused to its own starter. Of two
//! starts admitted in one checkout before either's marker, the record is
//! contested and names no one: the operation is foreign to both until the
//! `--after` of the start whose words name its identity takes it, so one
//! that stops non-zero is refused to both, and two starts naming the same
//! commit give it to the first to end (#2764); a contested sequence or
//! rebase, whose identity no start's words name, stays no one's. A
//! session's own git line run beside its start, before the marker, drops
//! the start's record, which its `PostToolUse` takes again, unless the
//! record is contested. A
//! rebase whose `orig-head` is not the `HEAD` it was started on (`git
//! rebase <upstream> <branch>`) is refused to its own starter, the safe
//! side. A start no command hook judges, its every candidate failing to
//! run, is handed on unrecorded and is refused to its own starter.
//!
//! A git command past a directory change the line makes after its first
//! command is read where that change took the shell, never where it
//! started (#2720, [`placed`]): a sibling agent's `cat f; cd <its
//! worktree>; git merge --ff-only main`, read in the main checkout its
//! shell starts in, dropped a merge's record there and took that merge as
//! its own start; an orchestrator's `cat $S/owner; cd <main>; git merge --no-commit
//! t1955`, left unread, started a merge no record named (#2723). A change
//! inside a subshell moves that subshell alone, so `(cd /wt && git
//! status); git merge x` records the merge where the shell stands
//! (#2764). Each record is read and written under its lock, so two hooks
//! at once never remove what the other wrote. The
//! recording runs whatever the manifest holds, since an operation can
//! bring the first such rule into its own working tree (#2694).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::argv::{Argv, Line};
use super::call::Call;
use super::rules::{acts_in, fired_in, placed};
use crate::manifest::OneOrMany;
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
        Operation::marker(git_dir).map(|(operation, _)| operation)
    }

    /// The operation standing in `git_dir` and the marker git keeps for it.
    fn marker(git_dir: &Path) -> Option<(Operation, PathBuf)> {
        [
            ("MERGE_HEAD", Operation::Merge),
            ("CHERRY_PICK_HEAD", Operation::CherryPick),
            ("REVERT_HEAD", Operation::Revert),
            ("rebase-apply/applying", Operation::Am),
            ("rebase-merge", Operation::Rebase),
            ("rebase-apply", Operation::Rebase),
        ]
        .into_iter()
        .map(|(marker, operation)| (operation, git_dir.join(marker)))
        .find(|(_, marker)| marker.exists())
    }

    /// The commit the operation started from, as git keeps it.
    fn base(self, git_dir: &Path, checkout: &Path) -> Option<String> {
        let read = |file: &str| marker_text(git_dir, file);
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

    /// What tells this operation from the next one on the same base, as
    /// git keeps it: the commit a merge or one pick takes, a sequence's
    /// `sequencer/head`, a rebase's `onto`; `None` for an `am`.
    fn identity(self, git_dir: &Path) -> Option<String> {
        let read = |file: &str| marker_text(git_dir, file);
        match self {
            Operation::Am => None,
            Operation::Rebase => read("rebase-merge/onto").or_else(|| read("rebase-apply/onto")),
            Operation::Merge => read("MERGE_HEAD"),
            Operation::CherryPick => read("sequencer/head").or_else(|| read("CHERRY_PICK_HEAD")),
            Operation::Revert => read("sequencer/head").or_else(|| read("REVERT_HEAD")),
        }
    }
}

/// Whether `git` runs `subcommand` to start an operation: not with one
/// of the flags that step or end one standing ([`STEPPING`]) among its
/// own words.
fn starts(git: &Argv, subcommand: &str) -> bool {
    let flags = STEPPING.map(str::to_owned);
    git.runs(subcommand) && !git.denies(&flags, &OneOrMany::One(subcommand.to_owned()))
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

/// The flags that step or end an operation, given to the subcommand that
/// started it.
const STEPPING: [&str; 6] = [
    "--abort",
    "--quit",
    "--continue",
    "--skip",
    "--edit-todo",
    "--show-current-patch",
];

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

/// Who started an operation, the commit it started from, and the
/// operation's [`Operation::identity`] once it stands: one file of
/// `permit/operations/`. A start recorded before its call ran has none
/// yet, and takes the first operation that stands on its base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Starter {
    session: Session,
    base: String,
    #[serde(default)]
    identity: Option<String>,
    /// Another session's start was admitted beside this one before
    /// either's marker: the operation is no one's until the call that
    /// began it ends ([`settle`], #2764).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    contested: bool,
}

impl Starter {
    /// Whether this is the record of `operation`, standing in `git_dir`.
    fn names(&self, operation: Operation, git_dir: &Path, checkout: &Path) -> bool {
        operation.base(git_dir, checkout).as_ref() == Some(&self.base)
            && self
                .identity
                .as_ref()
                .is_none_or(|id| operation.identity(git_dir).as_ref() == Some(id))
    }
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
                && !starter.contested
                && starter.names(operation, &git_dir, &checkout)
        });
        (!ours).then_some(Foreign {
            operation,
            checkout,
        })
    })
}

/// Before a call: what `line` does to the records of the checkouts it runs
/// git in ([`tend`]), then the session making `call` recorded as the
/// starter of each operation the line starts where none stands, with no
/// identity until it does; beside another session's start still waiting
/// for its marker, that record is contested instead.
pub(crate) fn record(root: &Path, call: &Call, line: &Line) -> Result<()> {
    let caller = Session::of(call);
    let (_held, tended) = tend(root, caller.as_ref(), line)?;
    for Tended {
        checkout,
        file,
        standing,
        starting,
        ..
    } in tended
    {
        if let (None, Some(_), Some(session), Some(base)) =
            (standing, starting, &caller, head(&checkout))
        {
            let starter = match read(&file) {
                Some(waiting) if waiting.identity.is_none() && waiting.session != *session => {
                    Starter {
                        contested: true,
                        ..waiting
                    }
                }
                _ => Starter {
                    session: session.clone(),
                    base,
                    identity: None,
                    contested: false,
                },
            };
            write(&file, &starter)?;
        }
    }
    Ok(())
}

/// After a call that ran, `permit --after`'s: what `line` does to the
/// records again, now that git has written what it started, then the
/// session recorded as the starter of each operation that began since the
/// call did ([`began_during`]) where the line started one and no record
/// names it: a start admitted while another operation stood, which
/// [`record`] could not take (#2723). A contested record is taken by the
/// start whose words name every commit of the operation's identity
/// ([`aimed`]), since the start that failed beside it can end without
/// error behind a pipe (#2764).
pub(crate) fn settle(
    root: &Path,
    call: &Call,
    line: &Line,
    since: Option<SystemTime>,
) -> Result<()> {
    let caller = Session::of(call);
    let (_held, tended) = tend(root, caller.as_ref(), line)?;
    for Tended {
        checkout,
        git_dir,
        file,
        standing,
        starting,
    } in tended
    {
        let (Some(operation), Some(start), Some(session)) = (standing, starting, &caller) else {
            continue;
        };
        let free = read(&file).is_none_or(|starter| {
            starter.contested && aimed(start, operation, &git_dir, &checkout)
        });
        if free
            && began_during(&git_dir, since)
            && let Some(base) = operation.base(&git_dir, &checkout)
        {
            let starter = Starter {
                session: session.clone(),
                base,
                identity: operation.identity(&git_dir),
                contested: false,
            };
            write(&file, &starter)?;
        }
    }
    Ok(())
}

/// Whether the start `git` ran the operation standing: every commit of
/// its identity is one a word of the start names. A sequence's identity,
/// the `HEAD` it began on, is named by none, so a contested sequence
/// stays no one's.
fn aimed(git: &Argv, operation: Operation, git_dir: &Path, checkout: &Path) -> bool {
    let named: Vec<String> = git
        .words()
        .iter()
        .skip(1)
        .filter(|word| !word.starts_with('-'))
        .filter_map(|word| {
            let commit = format!("{word}^{{commit}}");
            let args = ["rev-parse", "--verify", "--quiet", commit.as_str()];
            git::git(checkout, &args, None).ok()
        })
        .map(|commit| commit.trim().to_owned())
        .collect();
    operation.identity(git_dir).is_some_and(|identity| {
        identity
            .lines()
            .all(|id| named.iter().any(|n| n == id.trim()))
    })
}

/// Whether the operation standing in `git_dir` began while the call ran:
/// its marker written no earlier than `since`, when the call began; never
/// where that is not known. So an operation that stood before the call, a
/// person's, is never taken for the start a line read in that checkout
/// (#2723).
fn began_during(git_dir: &Path, since: Option<SystemTime>) -> bool {
    let Some((_, marker)) = Operation::marker(git_dir) else {
        return false;
    };
    let written = fs::metadata(marker).and_then(|m| m.modified()).ok();
    matches!((written, since), (Some(written), Some(since)) if written >= since)
}

/// A file of git's in `git_dir`, trimmed; `None` where it is not there.
fn marker_text(git_dir: &Path, file: &str) -> Option<String> {
    fs::read_to_string(git_dir.join(file))
        .ok()
        .map(|text| text.trim().to_owned())
}

/// Each checkout `line` runs git in whose place it reads ([`placed`]),
/// its git dir, the operation standing there and whether the line starts
/// one there, with its record tended on the way. A record whose operation
/// stands takes that operation's identity, or is dropped where it names
/// another: another base, or another identity. Where nothing stands, a
/// record that took an identity is dropped, since its operation has ended,
/// as is one whose base `HEAD` has left; one still waiting for its
/// operation is dropped only by its own session's line, since another
/// hand's, judged between the start's admission and the marker git
/// writes, would otherwise take it from its starter (#2723). So an
/// operation's own `--abort` drops its record only once it has run
/// (#2734).
fn tend<'a>(
    root: &Path,
    caller: Option<&Session>,
    line: &'a Line,
) -> Result<(Vec<fs::File>, Vec<Tended<'a>>)> {
    let placed: Vec<_> = placed(root, line)
        .into_iter()
        .filter(|(argv, _)| argv.runs("git"))
        .filter_map(|(git, checkout)| {
            let git_dir = git_dir(&checkout)?;
            let file = starter_file(&checkout, &git_dir);
            Some((git, checkout, git_dir, file))
        })
        .collect();
    let held = hold(placed.iter().map(|(.., file)| file.as_path()))?;
    let mut tended = Vec::new();
    for (git, checkout, git_dir, file) in placed {
        let standing = Operation::in_progress(&git_dir);
        let starting = STARTING.iter().any(|phrase| starts(git, phrase));
        if let Some(starter) = read(&file) {
            let stale = match standing {
                Some(operation) => !starter.names(operation, &git_dir, &checkout),
                None => {
                    starter.identity.is_some()
                        || (caller == Some(&starter.session) && !starter.contested)
                        || head(&checkout).as_ref() != Some(&starter.base)
                }
            };
            match standing {
                _ if stale => crate::remove(&file)?,
                Some(operation) if starter.identity.is_none() && !starter.contested => {
                    let identity = operation.identity(&git_dir);
                    write(
                        &file,
                        &Starter {
                            identity,
                            ..starter
                        },
                    )?;
                }
                _ => {}
            }
        }
        tended.push(Tended {
            checkout,
            git_dir,
            file,
            standing,
            starting: starting.then_some(git),
        });
    }
    Ok((held, tended))
}

/// One checkout a line runs git in, as [`tend`] left it.
struct Tended<'a> {
    checkout: PathBuf,
    git_dir: PathBuf,
    /// Its record ([`starter_file`]).
    file: PathBuf,
    standing: Option<Operation>,
    /// The command that starts an operation there, if the line has one.
    starting: Option<&'a Argv>,
}

/// Each record's lock held, in one order so two hooks never wait on each
/// other: `tend`, `record` and `settle` read a record and then write it,
/// and a hook beside them would otherwise remove what one just wrote
/// (#2764). Released when the files drop.
fn hold<'a>(files: impl Iterator<Item = &'a Path>) -> Result<Vec<fs::File>> {
    let mut locks: Vec<PathBuf> = files.map(|file| file.with_extension("lock")).collect();
    locks.sort();
    locks.dedup();
    locks
        .iter()
        .map(|lock| {
            if let Some(dir) = lock.parent() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            let file = fs::File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock);
            let file = file.with_context(|| format!("opening {}", lock.display()))?;
            file.lock()
                .with_context(|| format!("locking {}", lock.display()))?;
            Ok(file)
        })
        .collect()
}

fn write(file: &Path, starter: &Starter) -> Result<()> {
    let text = serde_json::to_string(starter).context("rendering the operation's starter")?;
    reading::replace(file, format!("{text}\n").as_bytes())
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
/// entry other than `HEAD`'s and not a conflict's: the operation's commit
/// carries it, so `permit --after` never puts it back (#2597), where a
/// conflicted path it leaves to the hand that resolves it (#2880). An
/// index git cannot read is read as staged, the side that puts nothing
/// back. What the index cannot say is whose hand staged the entry, so a
/// call's own `git add` while the operation stands is read as the
/// operation's and rides its commit; another session's staging there is what a `where =
/// "operating"` rule refuses at the call ([`foreign`]).
pub(super) fn staged(checkout: &Path, path: &str) -> Option<Operation> {
    let operation = Operation::in_progress(&git_dir(checkout)?)?;
    let args = ["diff", "--cached", "--quiet", "--diff-filter=u", "--", path];
    let differs = git::git(checkout, &args, None).is_err();
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

    use super::{Foreign, Line, Operation, foreign, record, settle};
    use crate::permit::call::Call;
    use crate::scratch::Tree;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(crate::scratch::UNHOOKED)
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
        merging_in(dir, &git(dir, &["rev-parse", "HEAD"]));
    }

    /// A merge of `commit` stopped before its commit.
    fn merging_in(dir: &Path, commit: &str) {
        let git_dir = git(dir, &["rev-parse", "--absolute-git-dir"]);
        std::fs::write(
            Path::new(&git_dir).join("MERGE_HEAD"),
            format!("{commit}\n"),
        )
        .unwrap();
    }

    fn ended(dir: &Path) {
        let git_dir = git(dir, &["rev-parse", "--absolute-git-dir"]);
        std::fs::remove_file(Path::new(&git_dir).join("MERGE_HEAD")).unwrap();
    }

    /// When a call `settle` is handed began: a moment before the test's
    /// marker is written.
    fn since() -> Option<std::time::SystemTime> {
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(5))
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
    /// the merge stands keeps the record, and its abort drops it once it
    /// has run.
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
        let abort = Line::read("git merge --abort");
        record(&dir, &call("s1", None), &abort).unwrap();
        ended(&dir);
        settle(&dir, &call("s1", None), &abort, since()).unwrap();
        merging(&dir);
        assert_eq!(standing(&dir, add, "s1", None), Some(Operation::Merge));
    }

    /// #2734: a record is never dropped while its operation stands.
    /// Another subcommand's abort, a `--abort` that is a message or a
    /// pattern, and a step of the operation leave the merge its starter's,
    /// whoever runs them; so does the merge's own `--quit` or `--abort`,
    /// through `-C` among them, admitted and then refused before it runs
    /// (#2723). Once it has run, the merge it ended is gone and its record
    /// with it, so a merge a person starts after on the same `HEAD` is not
    /// the starter's.
    #[test]
    fn a_record_stands_while_its_operation_does_and_its_abort_drops_it_once_run() {
        let dir = repository("operation-own-abort");
        let at = dir.display();
        for ends in ["git merge --quit", &format!("git -C {at} merge --abort")] {
            let start = Line::read("git merge --no-commit topic");
            record(&dir, &call("s1", None), &start).unwrap();
            merging(&dir);
            for line in [
                "git rebase --abort",
                "git cherry-pick --quit",
                "git am --abort",
                "git commit -m --abort -- a",
                "git log --grep --abort",
                "git merge --continue",
                "git status --short",
            ] {
                record(&dir, &call("s2", None), &Line::read(line)).unwrap();
                assert_eq!(standing(&dir, "git add -- a", "s1", None), None, "{line}");
            }
            record(&dir, &call("s1", None), &Line::read(ends)).unwrap();
            let refused = standing(&dir, "git merge --abort", "s1", None);
            assert_eq!(refused, None, "{ends} admitted and never run");
            ended(&dir);
            settle(&dir, &call("s1", None), &Line::read(ends), since()).unwrap();
            merging(&dir);
            let after = standing(&dir, "git add -- a", "s1", None);
            assert_eq!(after, Some(Operation::Merge), "{ends}");
            ended(&dir);
        }
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
    /// shell starts, and finding nothing standing there it leaves a start
    /// still waiting for its marker to its starter (#2723).
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
        assert_eq!(own, None);
    }

    /// #2723: a start is recorded when it is admitted, before git writes
    /// its marker, and another session's or agent's git line judged in
    /// that window finds nothing standing: it leaves the record, so the
    /// merge is its starter's to commit and abort, and still another's to
    /// be refused. A start that began nothing, a merge already up to date,
    /// leaves no record once its own call has ended, so a merge a person
    /// then starts on the same `HEAD` is not the session's, nor one #2720
    /// leaves unrecorded, a start written past a later `cd`.
    #[test]
    fn a_start_s_record_outlives_another_hand_s_line_and_not_its_own_call() {
        let dir = repository("operation-window");
        let (starter, other) = (Some("starter"), Some("other"));
        let start = Line::read("git merge --no-ff --no-commit topic");
        record(&dir, &call("s1", starter), &start).unwrap();
        for (session, agent) in [("s2", None), ("s1", other)] {
            record(&dir, &call(session, agent), &Line::read("git log -1")).unwrap();
        }
        merging(&dir);
        for line in ["git commit -F m", "git merge --abort"] {
            assert_eq!(standing(&dir, line, "s1", starter), None, "{line}");
            let refused = standing(&dir, line, "s1", other);
            assert_eq!(refused, Some(Operation::Merge), "{line}");
            assert_eq!(standing(&dir, line, "s2", None), Some(Operation::Merge));
        }
        ended(&dir);

        let up_to_date = Line::read("git merge topic");
        record(&dir, &call("s1", starter), &up_to_date).unwrap();
        settle(&dir, &call("s1", starter), &up_to_date, since()).unwrap();
        merging(&dir);
        let claimed = standing(&dir, "git add -- a", "s1", starter);
        assert_eq!(claimed, Some(Operation::Merge));
    }

    /// The merge of 2026-10-12, as FACE's line wrote it: a `cd` into the
    /// checkout past the line's first command, then the start. HOST's
    /// record stands on the old base and never makes the merge anyone's.
    /// Admitted where nothing stands, the start is recorded before it runs,
    /// so a merge that stops on a conflict, whose call no `PostToolUse`
    /// follows, is its starter's; admitted while HOST's merge still stood,
    /// its commit in its hook, it is recorded once it has run. Another
    /// agent's commit is refused either way. A merge that stood before the
    /// call, a person's, is never taken for the start, and a record whose
    /// merge has another identity on the same base is stale: a merge a
    /// person starts after an abort outside permit is not the old
    /// starter's.
    #[test]
    fn a_start_past_a_later_cd_or_while_another_merge_stood_is_its_starter_s() {
        let dir = repository("operation-replay");
        let (host, face) = (Some("host"), Some("face"));
        let commit = "git commit -F m";
        let face_start = Line::read(&format!(
            "S=/var/empty; cat $S/owner; cd {}; git status --short | head -3; \
             git merge --no-ff --no-commit t1955 2>&1 | tail -6",
            dir.display()
        ));
        for admitted_while_host_stood in [false, true] {
            let start = Line::read("git merge --no-ff --no-commit t2636");
            record(&dir, &call("s1", host), &start).unwrap();
            merging_in(&dir, &"a".repeat(40));
            record(&dir, &call("s1", host), &Line::read(commit)).unwrap();
            if admitted_while_host_stood {
                record(&dir, &call("s1", face), &face_start).unwrap();
            }
            ended(&dir);
            git(
                &dir,
                &["commit", "-q", "--allow-empty", "-m", "HOST's merge"],
            );
            if !admitted_while_host_stood {
                record(&dir, &call("s1", face), &face_start).unwrap();
            }
            merging_in(&dir, &"b".repeat(40));
            let before = standing(&dir, commit, "s1", face);
            let expected = admitted_while_host_stood.then_some(Operation::Merge);
            assert_eq!(before, expected, "before its PostToolUse");
            settle(&dir, &call("s1", face), &face_start, since()).unwrap();
            assert_eq!(standing(&dir, commit, "s1", face), None);
            assert_eq!(standing(&dir, commit, "s1", host), Some(Operation::Merge));
            assert_eq!(standing(&dir, commit, "s2", None), Some(Operation::Merge));
            ended(&dir);
        }

        merging_in(&dir, &"c".repeat(40));
        assert_eq!(standing(&dir, commit, "s1", face), Some(Operation::Merge));
        let git_dir = git(&dir, &["rev-parse", "--absolute-git-dir"]);
        let marker = std::fs::File::options()
            .write(true)
            .open(Path::new(&git_dir).join("MERGE_HEAD"))
            .unwrap();
        let earlier = std::time::SystemTime::now() - std::time::Duration::from_mins(1);
        marker.set_modified(earlier).unwrap();
        let start = Line::read("git merge --no-commit t2636");
        settle(&dir, &call("s1", host), &start, since()).unwrap();
        assert_eq!(standing(&dir, commit, "s1", host), Some(Operation::Merge));
    }

    /// #2764: of two starts admitted in one checkout before either's
    /// marker, the record names neither, and the first starter's own line
    /// before the marker leaves it so, so the merge that runs is foreign
    /// to both until the call that ran it ends: its `--after` takes it,
    /// the start whose target the merge took, and the other start's,
    /// ending without error behind a pipe, does not. Neither ending first
    /// decides it.
    #[test]
    fn of_two_starts_before_a_marker_the_merge_is_the_one_its_target_names() {
        let dir = repository("operation-two-starts");
        let branch = |name: &str| {
            let commit = git(&dir, &["commit-tree", "-m", name, "HEAD^{tree}"]);
            git(&dir, &["branch", name, &commit]);
            commit
        };
        let (a, b) = (branch("topic-a"), branch("topic-b"));
        let start =
            |name: &str| Line::read(&format!("git merge --no-commit {name} 2>&1 | tail -3"));
        let commit = "git commit -F m";
        for (ran, first, second) in [(&a, "a", "b"), (&b, "b", "a"), (&a, "b", "a")] {
            record(&dir, &call("s1", Some("a")), &start("topic-a")).unwrap();
            record(&dir, &call("s2", Some("b")), &start("topic-b")).unwrap();
            record(&dir, &call("s1", Some("a")), &Line::read("git log -1")).unwrap();
            merging_in(&dir, ran);
            assert_eq!(
                standing(&dir, commit, "s1", Some("a")),
                Some(Operation::Merge)
            );
            assert_eq!(
                standing(&dir, commit, "s2", Some("b")),
                Some(Operation::Merge)
            );
            for agent in [first, second] {
                let session = if agent == "a" { "s1" } else { "s2" };
                let line = start(&format!("topic-{agent}"));
                settle(&dir, &call(session, Some(agent)), &line, since()).unwrap();
            }
            let (own, other) = if ran == &a {
                (("s1", "a"), ("s2", "b"))
            } else {
                (("s2", "b"), ("s1", "a"))
            };
            assert_eq!(
                standing(&dir, commit, own.0, Some(own.1)),
                None,
                "{first} settled first"
            );
            let refused = standing(&dir, commit, other.0, Some(other.1));
            assert_eq!(refused, Some(Operation::Merge), "{first} settled first");
            ended(&dir);
        }
    }

    /// #2764: a record is read and written under its lock, so a hook
    /// beside another waits for it rather than remove what it wrote.
    #[test]
    fn a_record_is_written_under_its_lock() {
        let dir = repository("operation-lock");
        let git_dir = git(&dir, &["rev-parse", "--absolute-git-dir"]);
        let file = super::starter_file(&dir, Path::new(&git_dir));
        let held = super::hold([file.as_path()].into_iter()).unwrap();
        let waiting = std::thread::spawn({
            let dir = dir.to_path_buf();
            move || record(&dir, &call("s1", None), &Line::read("git merge x")).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!file.exists(), "written while another held the lock");
        drop(held);
        waiting.join().unwrap();
        assert!(file.exists());
    }

    /// #2764: a `cd` inside a subshell moves that subshell alone, so a
    /// merge the line starts after it is recorded where the shell stands
    /// and nothing is recorded where the subshell went; one the line does
    /// not say where it goes leaves the rest of that subshell unread.
    #[test]
    fn a_start_after_a_subshell_s_cd_is_recorded_where_the_shell_stands() {
        let other = repository("operation-subshell-other");
        let lines = [
            format!(
                "(cd {} && git status); git merge --no-commit x",
                other.display()
            ),
            "(cd \"$UNSET\" && git status); git merge --no-commit x".to_owned(),
        ];
        for (n, line) in lines.iter().enumerate() {
            let dir = repository(&format!("operation-subshell-{n}"));
            record(&dir, &call("s1", None), &Line::read(line)).unwrap();
            merging(&dir);
            assert_eq!(standing(&dir, "git add -- a", "s1", None), None, "{line}");
            let after = format!("(cd {} && ls); git add -- a", other.display());
            assert_eq!(standing(&dir, &after, "s2", None), Some(Operation::Merge));
        }
        merging(&other);
        let elsewhere = standing(&other, "git add -- a", "s1", None);
        assert_eq!(elsewhere, Some(Operation::Merge));
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
