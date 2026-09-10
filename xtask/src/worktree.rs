//! A track's worktrees (harness-design.md §4.2, §5, §11): where one lives,
//! `xtask worktree add`, which lays one down ready to be worked in, and
//! `xtask worktree retire`, which keeps its evidence and removes it. A
//! track runs an agent per worktree on a branch off its track branch, and
//! three facts about a fresh worktree are why the add is an instrument and
//! not a bare `git worktree add`:
//!
//! * `computed` trust is keyed by the root path, so a new worktree is
//!   untrusted: its regions answer `exec untrusted skipped`, the session
//!   hook renders nothing and pre-commit refuses the first commit. The
//!   grant is a person's (§11), so the line is printed, never run; what
//!   `computed` answered is read by [`computed::render`], once.
//! * `PreToolUse` denies an `Edit` or a `Write` that no candidate judged,
//!   and in a worktree with a cold `target/` the only candidate is `cargo
//!   run … permit` inside the hook's 60 s timeout. So `xtask` is built
//!   here, before the agent arrives, at the worktree's own HEAD, which is
//!   what `built` compares its digests against (§12 item 8).
//! * `core.hooksPath` is repository configuration and `.claude/settings.json` and
//!   `oracle.toml` are tracked, so all three carry over on their own, as
//!   the php-src tree the pin names does by being outside the clone. They
//!   are reported, not set again.
//!
//! The branch names its own track ([`Track::of`]): the longest existing
//! branch that is a proper path prefix of it, `main` for a branch on none,
//! which is the track `xtask done` will judge the worktree against (§4.5).
//! A branch that exists, on the clone or on `origin`, and a directory that
//! exists, refuse before anything is laid down, as `new-ext` does. Nothing
//! is measured here and no reading is written: the readings are the
//! worktree's own, once an agent is in it.
//!
//! The retire is where a track keeps that evidence. A worktree's readings
//! live in its own `target/harness/` and removing the worktree removes
//! them, so the worktree's `log.jsonl` is appended into the track's store
//! first, the root [`Track::of`] names for the branch, and only then is
//! the worktree removed, which keeps a merged agent's denials in the
//! `permits` section the console reads (§4.2). The log alone, through the
//! store's own append, one line per write, so a retire during a run tears
//! no line of either log (#56). The per-key files beside it stay with the
//! worktree, each being a scope's latest run over a tree the track does
//! not have, until the design says they are the track's (§4.2). A
//! worktree with uncommitted work is a refusal after the append, since
//! the worktree's copy is emptied once appended and the call that follows
//! cannot count the same readings twice. The branch is kept; deleting it
//! is a person's line, printed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::computed;
use crate::git::git;
use crate::reading::{self, Store};
use crate::track::{self, Track};

/// Where a worktree lives, under the main checkout, one directory per
/// branch: the one convention `permit` resolves a call's workspace by and
/// this instrument lays a worktree under, so a worktree it made is a
/// checkout of its own to the judge. Claude Code's own worktrees are laid
/// here too, and the directory is ignored.
pub(crate) const WORKTREES: &str = ".claude/worktrees";

/// The local file the session hook renders (harness-design.md §5).
const LOCAL: &str = ".claude/harness.local.md";

/// The prefix of every line a half prints, so a person reads whose line
/// it is.
const ADD: &str = "worktree add";
const RETIRE: &str = "worktree retire";

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

/// Lay the worktree for `branch` down under `root`, or refuse and change
/// nothing. `Ok` is the worktree's directory, ready; [`NotReady`] is the
/// directory laid down with a fact missing.
pub fn add(root: &Path, branch: &str) -> Result<PathBuf> {
    let dir = root.join(WORKTREES).join(branch);
    if git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        None,
    )
    .is_ok()
    {
        bail!(
            "the branch {branch} exists; work in its worktree, retire it, or name another branch"
        );
    }
    let remote = format!("refs/remotes/origin/{branch}");
    if git(root, &["rev-parse", "--verify", "--quiet", &remote], None).is_ok() {
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

    let track = Track::of(root, Some(branch))
        .with_context(|| format!("which track {branch} is on could not be said"))?;
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
        None,
    )
    .context("git worktree add refused; nothing was laid down")?;
    say(
        ADD,
        &format!(
            "{branch} off the track {} at {} ({})",
            track.name,
            track.at,
            &track.base[..track.base.len().min(7)]
        ),
    );
    say(ADD, &format!("worktree {}", dir.display()));

    let mut ready = true;
    ready &= build(&dir);
    ready &= lay_local(&dir);
    trust(&dir);
    carried_over(&dir);
    if ready {
        say(ADD, &format!("ready: cd {}", dir.display()));
        Ok(dir)
    } else {
        Err(NotReady { dir }.into())
    }
}

fn say(half: &str, line: &str) {
    println!("{half}: {line}");
}

fn complain(half: &str, line: &str) {
    eprintln!("{half}: {line}");
}

/// Retire the worktree on `branch`: its log appended into the track's
/// store and emptied, the worktree removed, the branch kept. A branch no
/// worktree is on, the main worktree and uncommitted work are refusals;
/// the last one after the append, which the lines say.
pub fn retire(root: &Path, branch: &str) -> Result<()> {
    let track = Track::of(root, Some(branch))
        .with_context(|| format!("which track {branch} is on could not be said"))?;
    let store = &track.store;
    let dir = track::worktrees(store)?
        .into_iter()
        .find_map(|(path, on)| (on.as_deref() == Some(branch)).then_some(path))
        .with_context(|| format!("no worktree is on {branch}"))?;
    if dir == *store {
        bail!(
            "{} is the main worktree, which is the track's own root; it is not retired",
            dir.display()
        );
    }
    let appended = append_log(&dir, store)?;
    git(store, &["worktree", "remove", &dir.to_string_lossy()], None).with_context(|| {
        if appended {
            "git worktree remove refused; the readings are appended already, so commit or discard what is uncommitted and call again"
        } else {
            "git worktree remove refused; commit or discard what is uncommitted and call again"
        }
    })?;
    say(RETIRE, &format!("removed {}", dir.display()));
    say(
        RETIRE,
        &format!(
            "the branch is kept; a person deletes it once it is merged: git branch -D {branch}"
        ),
    );
    Ok(())
}

/// The worktree's log appended into the store at `root` and emptied,
/// so a removal that refuses and the call that follows it cannot count
/// the same readings twice; the per-key files beside it stay. Whether
/// anything was appended.
fn append_log(dir: &Path, root: &Path) -> Result<bool> {
    let from = dir.join(reading::DIR).join(reading::LOG);
    // Bytes, not text: a line the worktree wrote is appended as it is.
    let text = match fs::read(&from) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", from.display())),
    };
    if text.is_empty() {
        say(
            RETIRE,
            &format!(
                "{} is missing or empty: this worktree wrote no readings",
                from.display()
            ),
        );
        return Ok(false);
    }
    let store = Store::of_workspace(root);
    let to = store.root().join(reading::LOG);
    let appended = store
        .append(&text)
        .with_context(|| format!("cannot append to {}", to.display()))?;
    fs::write(&from, "").with_context(|| format!("cannot empty {}", from.display()))?;
    say(
        RETIRE,
        &format!(
            "appended {appended} lines to {} and emptied the worktree's; the per-key files beside it stay with the worktree",
            to.display()
        ),
    );
    Ok(true)
}

/// `xtask` built here, so the first `Edit` is judged rather than waiting
/// on a build; whether it was.
fn build(dir: &Path) -> bool {
    let built = Command::new("cargo")
        .args(["build", "-q", "-p", "xtask"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .status();
    match built {
        Ok(status) if status.success() => {
            say(
                ADD,
                "built target/debug/xtask: the first Edit in it is judged by this build, not by a cold cargo run",
            );
            true
        }
        Ok(_) => {
            complain(
                ADD,
                "cargo build -p xtask failed; an Edit there is judged by a cold cargo run or denied",
            );
            false
        }
        Err(err) => {
            complain(
                ADD,
                &format!("cargo did not run ({err}); an Edit there is judged by nothing"),
            );
            false
        }
    }
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
fn lay_local(dir: &Path) -> bool {
    let Some(built) = built(dir) else {
        complain(
            ADD,
            &format!(
                "no built xtask under {}/target; the worktree has no local file",
                dir.display()
            ),
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
            say(ADD, &format!("laid {LOCAL} down"));
            true
        }
        _ => {
            complain(
                ADD,
                "xtask status --session failed; the worktree has no local file",
            );
            false
        }
    }
}

/// The local file rendered, read as [`computed::Answer`]: the regions
/// rendered where the worktree is trusted, and the grant printed as a
/// person's line where it is not (§11); a broken marker and a missing
/// `computed` are complaints, since the file is laid down either way.
fn trust(dir: &Path) {
    let files = [LOCAL.to_owned()];
    match computed::render(dir, &files) {
        Ok(computed::Answer::Rendered(_)) => say(ADD, &format!("rendered {LOCAL}")),
        Ok(computed::Answer::Untrusted { .. }) => say(ADD, &computed::trust_line(dir)),
        Ok(answer) => complain(ADD, &answer.refusal().unwrap_or_default()),
        Err(err) => complain(ADD, &format!("{err:#}")),
    }
}

/// What the clone already has, reported rather than set again.
fn carried_over(dir: &Path) {
    match git(dir, &["config", "core.hooksPath"], None) {
        Ok(hooks) if !hooks.trim().is_empty() => say(
            ADD,
            &format!(
                "carried over: core.hooksPath={}, which is repository config",
                hooks.trim()
            ),
        ),
        _ => say(
            ADD,
            "core.hooksPath is unset, so pre-commit and pre-push are not wired; the session hook points it at scripts/hooks",
        ),
    }
    if dir.join(".claude/settings.json").is_file() {
        say(
            ADD,
            "carried over: .claude/settings.json, which is tracked, so the plugin settings are the clone's",
        );
    } else {
        say(
            ADD,
            ".claude/settings.json is missing, so this worktree loads no harness plugin",
        );
    }
    if dir.join("oracle.toml").is_file() {
        say(
            ADD,
            "carried over: oracle.toml, which is tracked, and the php-src tree it pins, which is outside the clone; `xtask phpt` says if that tree is gone",
        );
    } else {
        say(
            ADD,
            "oracle.toml is missing, so this worktree has no oracle and no pinned php-src",
        );
    }
}
