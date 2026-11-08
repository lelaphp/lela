//! A ticket's worktree (harness-design.md §4.2, §5, §11): where one
//! lives, `xtask worktree add`, which lays one down ready to be worked in,
//! and `xtask worktree retire`, which keeps its evidence and removes it.
//! Every ticket is worked by an agent in a worktree on its own branch cut
//! from `main` (ADR-0009), and four facts about a fresh worktree are why
//! the add is an instrument and not a bare `git worktree add`:
//!
//! * `computed` trust is a person's grant (§11), and a grant covers every
//!   repository under the granted path (#74, revision 18). The worktrees
//!   are laid down beside the clone ([`homes`], revision 56), so the grant
//!   that covers them is the one over that directory, made once; the
//!   local file is rendered here where it is, and where `computed`
//!   answers untrusted the file is said to be unrendered with that grant's
//!   line, and nothing waits on it. What `computed` answered is read by
//!   [`computed::render`], once.
//! * `PreToolUse` denies an `Edit` or a `Write` that no candidate judged,
//!   and in a worktree with a cold `target/` the only candidate is `cargo
//!   run … permit` inside the hook's 60 s timeout. So `xtask` is built
//!   here, before the agent arrives, at the worktree's own HEAD, which is
//!   what `built` compares its digests against (§12 item 8).
//! * `core.hooksPath` is repository configuration and `.claude/settings.json` and
//!   `oracle.toml` are tracked, so all three carry over on their own, as
//!   the php-src tree the pin names does by being outside the clone. They
//!   are reported, not set again.
//! * A worktree's `target/` is its own, and that build starts warm: `add`
//!   seeds `target/debug` from the clone's with `cp -c -R`, a
//!   clone-on-write copy where the volume supports one, so the registry
//!   dependencies are blocks the two directories share and cargo compiles
//!   the workspace's own crates alone, since a fresh checkout's files are
//!   newer than the seeded fingerprints (§5, "Worktrees"; #245).
//!   What it must not do is give two worktrees one `CARGO_TARGET_DIR`:
//!   cargo keys a workspace member's artifact by its path under the
//!   workspace root and judges the slot fresh by mtime, so a checkout
//!   whose files predate another worktree's build would run that
//!   worktree's binary (#251), where `built_root`, `built::judges` and
//!   `permit` each rest on a binary that belongs to one checkout. So no
//!   variable is set and no `.cargo/config.toml` is written; the seeding
//!   is best-effort and a `cp -c` that cannot is one line and a cold start.
//!
//! The branch is cut from `main`'s tip ([`Track::of`]'s base for a branch
//! not yet made), which is what `xtask done` will judge the worktree
//! against (§4.5). A branch that exists, on the clone or on `origin`, a
//! directory that exists, and a ticket another branch already records,
//! refuse before anything is laid down, as `new-ext` does. Nothing is measured here and
//! no reading is written: the readings are the worktree's own, once an
//! agent is in it.
//!
//! The session that lays the worktree down owns it (#3618 M1, #3621):
//! the add records it ([`crate::owner`]), so pre-commit and `permit`
//! refuse another session's writes there. `xtask worktree own` records
//! the owner of a checkout no add laid down, main's and the merge
//! worktree, or lets one go; `xtask worktree whose` says whose a
//! checkout is and refuses another session, which is what pre-commit and
//! pre-merge-commit ask before anything else runs.
//!
//! The ticket the branch's work is for is the add's to record (#30):
//! `--ticket` writes it on the branch, where the track module reads it by
//! name, so a reader that asks whether the work of a branch is done need
//! not be told the number. A branch laid down with none records none,
//! and `xtask track` says so rather than guessing from the name. One
//! branch per ticket: a second add on a ticket a branch records is
//! refused, naming that branch, so no ticket is closed twice.
//!
//! The retire is where that evidence is kept. A worktree's readings live
//! in its own `target/harness/` and removing the worktree removes them,
//! so the worktree's log is landed in the one store first, the main
//! checkout's, which [`Track::of`] names, and only then is the worktree
//! removed, which keeps a merged agent's denials in the `permits` section
//! the console reads (§4.2). The log alone, each reading into the store's
//! log and index through the store's own append and rename, one line per
//! write, so a retire during a run tears no line of either log (#56,
//! #2164). The scope's files beside it stay with the worktree, each being a scope's latest run over a tree the main
//! checkout does not have, and copying one in would hand `done` a reading
//! that is fresh for nothing (§5, "Worktrees"; #35). What the log
//! carries is found at the branch's merge in the index, by the tree each
//! reading was taken over, so a pass over what the merge stages is not run again
//! there (`Store::over`, #1252). The worktree's kept phpt runs
//! are carried into the main worktree's beside the log, each one main
//! keeps none of: a run is keyed by the engine's tree and not by the
//! checkout, so it is the same run wherever it was taken, and one main
//! keeps already stays (#1268). A
//! worktree with uncommitted work is a refusal after the append, since
//! the worktree's copy is emptied once appended and the call that follows
//! cannot count the same readings twice. The branch is kept, and the
//! ticket record with it; deleting it is a person's line, printed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::computed;
use crate::git::{git, has_ref};
use crate::owner::{self, SessionId};
use crate::phpt::against;
use crate::reading::Store;
use crate::track::{self, MAIN, Ticket, Track, short};

/// One line of `whose` on stdout, its text formatted in place.
macro_rules! say {
    ($whose:expr, $($text:tt)+) => {
        println!("{}: {}", $whose, format_args!($($text)+))
    };
}
pub(crate) use say;

/// One line of `whose` on stderr, its text formatted in place.
macro_rules! complain {
    ($whose:expr, $($text:tt)+) => {
        eprintln!("{}: {}", $whose, format_args!($($text)+))
    };
}

/// Where the worktrees of the repository whose main checkout is
/// `checkout` are laid down, one directory per branch: beside the
/// checkout, in the directory named after it, `<checkout>-worktrees`, as
/// `dax-php-worktrees` is beside `dax-php`. Outside the checkout, since
/// everything under `.claude/` is the harness's own and an agent working
/// there meets the rules held for its files (revision 56). Laying one
/// down is the one thing that asks: a worktree that exists is found
/// through git's own record, `git worktree list` ([`track::worktrees`])
/// or its `.git`, so one laid under `.claude/worktrees/` before revision
/// 56 is judged and retired as any other.
pub(crate) fn homes(checkout: &Path) -> Result<PathBuf> {
    beside(checkout, "worktrees")
}

/// A directory beside `root`, named after it with `-<what>` added, as
/// `dax-php-t-opus-5` and `dax-php-worktrees` are beside `dax-php`.
pub(crate) fn beside(root: &Path, what: &str) -> Result<PathBuf> {
    let parent = root
        .parent()
        .context("the repository has no parent directory to lay anything beside")?;
    let mut name = root
        .file_name()
        .context("the repository's directory has no name")?
        .to_os_string();
    name.push(format!("-{what}"));
    Ok(parent.join(name))
}

/// The local file the session hook renders (harness-design.md §5).
const LOCAL: &str = ".claude/harness.local.md";

/// The prefix of every line a half prints, so a person reads whose line
/// it is.
const ADD: &str = "worktree add";
const RETIRE: &str = "worktree retire";
const OWN: &str = "worktree own";
const WHOSE: &str = "worktree whose";

/// A worktree that is laid down but not ready to be worked in: the lines
/// printed before it say what failed. Its own error, so the exit can say
/// that nothing needs undoing, as a refusal's does that nothing was done.
#[derive(Debug)]
pub(crate) struct NotReady {
    pub dir: PathBuf,
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is laid down but not ready to be worked in; the lines above say what failed",
            self.dir.display()
        )
    }
}

impl std::error::Error for NotReady {}

/// Lay the worktree for `branch` down under `root`, recording `ticket` on
/// the branch where one is given and `owner` as the session that owns it,
/// or refuse and change nothing. `Ok` is the worktree's directory, ready;
/// [`NotReady`] is the directory laid down with a fact missing.
pub fn add(
    root: &Path,
    branch: &str,
    ticket: Option<Ticket>,
    owner: Option<&SessionId>,
) -> Result<PathBuf> {
    let track = Track::of(root, Some(branch))
        .with_context(|| format!("where {branch} is taken off could not be said"))?;
    let homes = homes(&track.store)?;
    let dir = homes.join(branch);
    if has_ref(root, &format!("refs/heads/{branch}")) {
        bail!(
            "the branch {branch} exists; work in its worktree, retire it, or name another branch"
        );
    }
    if has_ref(root, &format!("refs/remotes/origin/{branch}")) {
        bail!(
            "origin has {branch}, so an agent is on it; check it out with `git worktree add <dir> {branch}`, or name another branch"
        );
    }
    if dir.exists() {
        bail!(
            "{} exists; retire it with `xtask worktree retire`, or name another branch",
            dir.display()
        );
    }
    if let Some(ticket) = ticket
        && let Some(other) = track::branch_of_ticket(root, ticket)
    {
        bail!(
            "{ticket} is {other}'s already; work in its worktree, or have a person delete that branch once its work is merged, or name another ticket"
        );
    }

    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            &dir.to_string_lossy(),
            &track.base,
        ],
    )
    .context("git worktree add refused; nothing was laid down")?;
    say!(ADD, "{branch} off {MAIN} at {}", short(&track.base));
    say!(ADD, "worktree {}", dir.display());
    match ticket {
        Some(ticket) => {
            track::record_ticket(root, branch, ticket)?;
            say!(
                ADD,
                "for {ticket}, recorded on the branch, which `xtask track` reads by name"
            );
        }
        None => say!(
            ADD,
            "for no ticket: none was named, and `xtask track` says so rather than guessing from the branch name"
        ),
    }
    record_owner(&dir, owner, ADD)?;
    make_ready(dir, root, &homes, ADD, &["debug"])
}

/// `owner` recorded as the session that owns the checkout at `dir`, said
/// under `whose`; a checkout laid down outside a session is said to be
/// no session's.
pub(crate) fn record_owner(dir: &Path, owner: Option<&SessionId>, whose: &str) -> Result<()> {
    match owner {
        Some(session) => {
            owner::record(dir, session)?;
            say!(
                whose,
                "owned by session {session}: pre-commit and permit refuse another session's writes there"
            );
        }
        None => say!(
            whose,
            "owned by no session, being laid down outside one; `xtask worktree own --session <id>` there names one"
        ),
    }
    Ok(())
}

/// The worktree at `dir`, laid down beside the clone at `root`, made
/// ready to be worked in, its lines under `whose`: each of `builds` seeded
/// from the clone's ([`seed`]), `xtask` built, the local file laid and
/// rendered under the grant over `homes`, and what the clone carries over
/// said. `Ok` is the directory; [`NotReady`] names it with a fact
/// missing.
pub(crate) fn make_ready(
    dir: PathBuf,
    root: &Path,
    homes: &Path,
    whose: &str,
    builds: &[&str],
) -> Result<PathBuf> {
    seed(&dir, root, whose, builds);
    let mut ready = true;
    ready &= build(&dir, whose);
    ready &= lay_local(&dir, whose);
    trust(&dir, homes, whose);
    carried_over(&dir, whose);
    if ready {
        say!(whose, "ready: cd {}", dir.display());
        Ok(dir)
    } else {
        Err(NotReady { dir }.into())
    }
}

/// Record the owner of the checkout `root` stands in: `named`, or the
/// `caller`'s own session; or, with `release`, take its record away. A
/// session records itself alone, and neither takes nor lets go of a
/// checkout another session owns; a person, who calls from no session,
/// names any session and hands a checkout over.
pub fn own(
    root: &Path,
    caller: Option<&SessionId>,
    named: Option<SessionId>,
    release: bool,
) -> Result<()> {
    let checkout = owner::checkout_of(root)
        .with_context(|| format!("{} is in no checkout", root.display()))?;
    let at = checkout.display();
    let held = owner::of(&checkout)?;
    if let Some(other) = owner::another(&checkout, caller)? {
        bail!(
            "{at} is session {other}'s; {other} lets it go with `xtask worktree own --release`, or a person hands it over with `xtask worktree own --session <id>` in a terminal"
        );
    }
    if release {
        let was = owner::release(&checkout)?;
        say!(
            OWN,
            "{at} is no session's{}",
            if was { " now" } else { " already" }
        );
        return Ok(());
    }
    let session = match (named, caller) {
        (Some(named), Some(caller)) if named != *caller => bail!(
            "a session records itself as an owner, and {named} is not this one; a person names another session from a terminal"
        ),
        (Some(named), _) => named,
        (None, Some(caller)) => caller.clone(),
        (None, None) => bail!(
            "no session runs this; a person names the session that owns {at} with --session <id>"
        ),
    };
    owner::record(&checkout, &session)?;
    let taken = match held {
        Some(was) if was != session => format!(", taken from {was}"),
        _ => String::new(),
    };
    say!(OWN, "{at} is session {session}'s{taken}");
    Ok(())
}

/// Say whose the checkout `root` stands in is, and refuse where another
/// session than the `caller` owns it: what pre-commit and
/// pre-merge-commit ask before anything runs, so a commit there is the
/// owner's or a person's.
pub fn whose(root: &Path, caller: Option<&SessionId>) -> Result<()> {
    let checkout = owner::checkout_of(root)
        .with_context(|| format!("{} is in no checkout", root.display()))?;
    let at = checkout.display();
    if let Some(other) = owner::another(&checkout, caller)? {
        let caller = caller.map(SessionId::as_str).unwrap_or_default();
        bail!(
            "{at} is session {other}'s and this is session {caller}: one session writes in a checkout, its owner (#3618 M1). Instead: work in your own worktree (`xtask worktree add`), or have {other} let it go with `xtask worktree own --release`"
        );
    }
    match (owner::of(&checkout)?, caller) {
        (None, _) => say!(WHOSE, "{at} is no session's"),
        (Some(session), Some(_)) => say!(WHOSE, "{at} is session {session}'s, this one"),
        (Some(session), None) => say!(WHOSE, "{at} is session {session}'s; a person is admitted"),
    }
    Ok(())
}

/// Retire the worktree on `branch`: its log appended into the main
/// checkout's store and emptied, the worktree removed, the branch kept.
/// A branch no worktree is on, the main worktree and uncommitted work are
/// refusals; the last one after the append, which the lines say.
pub fn retire(root: &Path, branch: &str) -> Result<()> {
    let track = Track::of(root, Some(branch))
        .with_context(|| format!("where {branch} was taken off could not be said"))?;
    let store = &track.store;
    let dir = track::worktrees(store)?
        .into_iter()
        .find_map(|(path, on)| (on.as_deref() == Some(branch)).then_some(path))
        .with_context(|| format!("no worktree is on {branch}"))?;
    if dir == *store {
        bail!(
            "{} is the main worktree, which holds the one store; it is not retired",
            dir.display()
        );
    }
    let appended = append_log(&dir, store)?;
    carry_runs(&dir, RETIRE)?;
    git(store, &["worktree", "remove", &dir.to_string_lossy()]).with_context(|| {
        if appended {
            "git worktree remove refused; the readings are appended already, so commit or discard what is uncommitted and call again"
        } else {
            "git worktree remove refused; commit or discard what is uncommitted and call again"
        }
    })?;
    say!(RETIRE, "removed {}", dir.display());
    let record = match track.ticket {
        Some(ticket) => format!(", and the record that it is for {ticket} with it"),
        None => String::new(),
    };
    say!(
        RETIRE,
        "the branch is kept{record}; a person deletes it once it is merged: git branch -D {branch}"
    );
    Ok(())
}

/// The worktree's readings landed in the store at `root`, its log and
/// its index ([`Store::land`]), and the worktree's log emptied, so a
/// removal that refuses and the call that follows it cannot count the
/// same readings twice; the per-key files beside it stay. Whether
/// anything was landed.
fn append_log(dir: &Path, root: &Path) -> Result<bool> {
    let from = Store::of_workspace(dir);
    let store = Store::of_workspace(root);
    let landed = store.land(&from).with_context(|| {
        format!(
            "cannot land {} in {}",
            from.root().display(),
            store.root().display()
        )
    })?;
    if landed == 0 {
        say!(
            RETIRE,
            "{} holds no log: this worktree wrote no readings",
            from.root().display()
        );
        return Ok(false);
    }
    say!(
        RETIRE,
        "landed {landed} lines in {}'s log and index and emptied the worktree's; the per-key files beside it stay with the worktree",
        store.root().display()
    );
    Ok(true)
}

/// The worktree's kept phpt runs carried into the main worktree's, which
/// outlives it: `phpt --against` looks for a base's run in every worktree
/// that stands ([`against::carry`]), so a run left here would go with the
/// worktree (#1268). Each one main keeps none of is copied, and one it
/// keeps already stays as it stood. What it did is said under `whose`.
pub(crate) fn carry_runs(dir: &Path, whose: &str) -> Result<()> {
    let main = track::main_worktree(dir)?;
    let carried = against::carry(dir, &main)?;
    let runs = main.join(against::RUNS);
    say!(
        whose,
        "{}",
        match carried {
            against::Carried {
                carried: 0,
                kept: 0,
            } => "this worktree kept no phpt run".to_owned(),
            against::Carried { carried, kept } => format!(
                "carried {carried} kept phpt run{} into {}; {kept} it keeps already stayed as they stood",
                if carried == 1 { "" } else { "s" },
                runs.display()
            ),
        }
    );
    Ok(())
}

/// Each of `builds`, `target/<build>` of the worktree, seeded from the
/// clone's at `root` ([`seed_build`]), so the build that follows compiles
/// the workspace's own crates and no dependency (harness-design.md §5,
/// "Worktrees"; #245). Nothing beside them is copied: the worktree's
/// `target/harness/` is its own from its first instrument. Best-effort: a
/// clone with no such build, and a `cp -c` that fails, are one line and a
/// cold start. Each seeded `xtask` binary is taken away too, so the one
/// under the worktree's `target/` is built from the worktree's sources or
/// is not there: a build that fails leaves no binary whose root is the
/// clone's for the hook's walk to find.
fn seed(dir: &Path, root: &Path, whose: &str, builds: &[&str]) {
    for build in builds {
        match seed_build(dir, root, build) {
            Ok(true) => {
                fs::remove_file(dir.join("target").join(build).join("xtask")).ok();
                say!(
                    whose,
                    "seeded target/{build} from the clone's by cp -c: the dependencies are blocks the two share, and the build compiles the workspace's own crates"
                );
            }
            Ok(false) => say!(
                whose,
                "nothing to seed target/{build} from: {} holds no build, so it starts cold",
                root.join("target").join(build).display()
            ),
            Err(failed) => complain!(
                whose,
                "{failed}; target/{build} of the worktree starts cold"
            ),
        }
    }
}

/// `target/<build>` of the worktree at `dir` seeded from the one at
/// `root` with `cp -c -R`, a clone-on-write copy where the volume
/// supports one, so the dependencies are blocks the two share: whether
/// there was a build to copy, or why the copy failed, with what it left
/// removed so the worktree is as cold as that says.
pub(crate) fn seed_build(dir: &Path, root: &Path, build: &str) -> Result<bool, String> {
    let from = root.join("target").join(build);
    if !from.is_dir() {
        return Ok(false);
    }
    let to = dir.join("target").join(build);
    let copied = fs::create_dir_all(dir.join("target")).and_then(|()| {
        Command::new("cp")
            .args(["-c", "-R"])
            .arg(&from)
            .arg(&to)
            .stdin(Stdio::null())
            .output()
    });
    let failed = match copied {
        Ok(out) if out.status.success() => return Ok(true),
        Ok(out) => {
            let why = String::from_utf8_lossy(&out.stderr);
            format!(
                "cp -c -R {} failed ({})",
                from.display(),
                why.lines().next().unwrap_or("").trim()
            )
        }
        Err(err) => format!("cp did not run ({err})"),
    };
    fs::remove_dir_all(&to).ok();
    Err(failed)
}

/// `xtask` built here, so the first `Edit` is judged rather than waiting
/// on a build; whether it was, with what the build compiled on the line:
/// the workspace's own crates, which a seeded worktree compiles, and the
/// dependencies, which it does not.
fn build(dir: &Path, whose: &str) -> bool {
    let built = Command::new("cargo")
        .args(["build", "-p", "xtask"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output();
    match built {
        Ok(out) if out.status.success() => {
            let (own, deps) = compiled(dir, &String::from_utf8_lossy(&out.stderr));
            let what = if own + deps == 0 {
                "nothing".to_owned()
            } else {
                format!("{own} crates of the workspace and {deps} dependencies")
            };
            say!(
                whose,
                "built target/debug/xtask, compiling {what}: the first Edit in it is judged by this build, not by a cold cargo run"
            );
            true
        }
        Ok(out) => {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            complain!(
                whose,
                "cargo build -p xtask failed; an Edit there is judged by a cold cargo run or denied"
            );
            false
        }
        Err(err) => {
            complain!(
                whose,
                "cargo did not run ({err}); an Edit there is judged by nothing"
            );
            false
        }
    }
}

/// What a build compiled, read from cargo's `Compiling` lines: the crates
/// of the workspace at `dir`, whose line names a path under it, and the
/// dependencies, whose line names none or one elsewhere.
fn compiled(dir: &Path, stderr: &str) -> (usize, usize) {
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut own = 0;
    let mut deps = 0;
    for line in stderr.lines() {
        let Some(rest) = line.trim_start().strip_prefix("Compiling ") else {
            continue;
        };
        let at = rest
            .rsplit_once(" (")
            .and_then(|(_, path)| path.strip_suffix(')'))
            .and_then(|path| fs::canonicalize(path).ok());
        if at.is_some_and(|path| path.starts_with(&dir)) {
            own += 1;
        } else {
            deps += 1;
        }
    }
    (own, deps)
}

/// The built harness of `dir`, release before debug as the command hook
/// tries them.
fn built(dir: &Path) -> Option<PathBuf> {
    ["target/release/xtask", "target/debug/xtask"]
        .into_iter()
        .map(|candidate| dir.join(candidate))
        .find(|candidate| candidate.is_file())
}

/// The local file, laid down by the build just made and rendered by name;
/// whether it is there.
fn lay_local(dir: &Path, whose: &str) -> bool {
    let Some(built) = built(dir) else {
        complain!(
            whose,
            "no built xtask under {}/target; the worktree has no local file",
            dir.display()
        );
        return false;
    };
    let laid = Command::new(&built)
        .args(["status", "--session"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status();
    match laid {
        Ok(status) if status.success() => {
            say!(whose, "laid {LOCAL} down");
            true
        }
        _ => {
            complain!(
                whose,
                "xtask status --session failed; the worktree has no local file"
            );
            false
        }
    }
}

/// The local file rendered, read as [`computed::Answer`]: the regions
/// rendered where a grant covers the worktree, and said to be unrendered
/// where `computed` answers untrusted, with the line a person runs once
/// over `homes`, which covers every worktree laid down there (§11); the
/// clone's grant does not, the worktrees being beside it (revision 56).
/// Nothing waits on the line. A broken marker and a missing `computed`
/// are complaints, since the file is laid down either way.
fn trust(dir: &Path, homes: &Path, whose: &str) {
    let files = [LOCAL.to_owned()];
    match computed::render(dir, &files) {
        Ok(computed::Answer::Rendered(_)) => say!(whose, "rendered {LOCAL}"),
        Ok(computed::Answer::Untrusted { .. }) => say!(
            whose,
            "{LOCAL}'s exec regions are unrendered here: no grant covers {}, where the worktrees are laid down; a person runs, once, for every worktree there (harness-design.md §11): computed trust {}",
            homes.display(),
            homes.display()
        ),
        Ok(answer) => complain!(whose, "{}", answer.refusal().unwrap_or_default()),
        Err(err) => complain!(whose, "{err:#}"),
    }
}

/// What the clone already has, said rather than set again.
fn carried_over(dir: &Path, whose: &str) {
    match git(dir, &["config", "core.hooksPath"]) {
        Ok(hooks) if !hooks.trim().is_empty() => say!(
            whose,
            "carried over: core.hooksPath={}, which is repository config",
            hooks.trim()
        ),
        _ => say!(
            whose,
            "core.hooksPath is unset, so pre-commit and pre-push are not wired; the session hook points it at scripts/hooks"
        ),
    }
    if dir.join(".claude/settings.json").is_file() {
        say!(
            whose,
            "carried over: .claude/settings.json, which is tracked, so the plugin settings are the clone's"
        );
    } else {
        say!(
            whose,
            ".claude/settings.json is missing, so this worktree loads no harness plugin"
        );
    }
    if dir.join("oracle.toml").is_file() {
        say!(
            whose,
            "carried over: oracle.toml, which is tracked, and the php-src tree it pins, which is outside the clone; `xtask phpt` says if that tree is gone"
        );
    } else {
        say!(
            whose,
            "oracle.toml is missing, so this worktree has no oracle and no pinned php-src"
        );
    }
}
