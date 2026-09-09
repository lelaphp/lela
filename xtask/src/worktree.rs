//! A track's worktrees (harness-design.md §4.2, §5, §11): where one lives,
//! and `xtask worktree add`, which lays one down ready to be worked in. A
//! track runs an agent per worktree on a branch off its track branch, and
//! three facts about a fresh worktree are why this is an instrument and
//! not a bare `git worktree add`:
//!
//! * `computed` trust is keyed by the root path, so a new worktree is
//!   untrusted: its regions answer `exec untrusted skipped`, the session
//!   hook renders nothing and pre-commit refuses the first commit. The
//!   grant is a person's (§11), so the line is printed, never run.
//! * `PreToolUse` denies an `Edit` or a `Write` that no candidate judged,
//!   and in a worktree with a cold `target/` the only candidate is `cargo
//!   run … permit` inside the hook's 60 s timeout. So `xtask` is built
//!   here, before the agent arrives, at the worktree's own HEAD, which is
//!   what `built` compares its digests against (§12 item 8).
//! * `core.hooksPath` is repository config and `.claude/settings.json` and
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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::git::git;
use crate::track::Track;

/// Where a worktree lives, under the main checkout, one directory per
/// branch: the one convention `permit` resolves a call's workspace by and
/// this instrument lays a worktree under, so a worktree it made is a
/// checkout of its own to the judge. Claude Code's own worktrees are laid
/// here too, and the directory is ignored.
pub const WORKTREES: &str = ".claude/worktrees";

/// The local file the session hook renders (harness-design.md §5).
const LOCAL: &str = ".claude/harness.local.md";

/// The prefix of every line printed, so a person reads whose line it is.
const SAY: &str = "worktree add";

/// A worktree that is laid down but not ready to be worked in: the lines
/// printed before it say what failed. Its own error, so the exit can say
/// that nothing needs undoing, as a refusal's does that nothing was done.
#[derive(Debug)]
pub struct NotReady {
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
            "{} exists; retire it with scripts/worktree-retire.sh, or name another branch",
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
    say(&format!(
        "{branch} off the track {} at {} ({})",
        track.name,
        track.at,
        &track.base[..track.base.len().min(7)]
    ));
    say(&format!("worktree {}", dir.display()));

    let mut ready = true;
    ready &= build(&dir);
    ready &= lay_local(&dir);
    trust(&dir);
    carried_over(&dir);
    if ready {
        say(&format!("ready: cd {}", dir.display()));
        Ok(dir)
    } else {
        Err(NotReady { dir }.into())
    }
}

fn say(line: &str) {
    println!("{SAY}: {line}");
}

fn complain(line: &str) {
    eprintln!("{SAY}: {line}");
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
                "built target/debug/xtask: the first Edit in it is judged by this build, not by a cold cargo run",
            );
            true
        }
        Ok(_) => {
            complain(
                "cargo build -p xtask failed; an Edit there is judged by a cold cargo run or denied",
            );
            false
        }
        Err(err) => {
            complain(&format!(
                "cargo did not run ({err}); an Edit there is judged by nothing"
            ));
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
        complain(&format!(
            "no built xtask under {}/target; the worktree has no local file",
            dir.display()
        ));
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
            say(&format!("laid {LOCAL} down"));
            true
        }
        _ => {
            complain("xtask status --session failed; the worktree has no local file");
            false
        }
    }
}

/// The regions rendered where the worktree is trusted, and the grant
/// printed as a person's line where it is not (§11).
fn trust(dir: &Path) {
    let ran = Command::new("computed")
        .args(["run", LOCAL])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output();
    let trusted = match ran {
        Ok(out) if matches!(out.status.code(), Some(0 | 1)) => {
            !String::from_utf8_lossy(&out.stderr).contains(" untrusted")
        }
        Ok(out) if out.status.code() == Some(2) => {
            complain(&format!("a computed marker in {LOCAL} is broken"));
            false
        }
        _ => {
            complain("computed did not run (cargo install computed)");
            false
        }
    };
    if trusted {
        say(&format!("rendered {LOCAL}"));
    } else {
        say(
            "this worktree is untrusted, so its regions are unrendered; a person runs, once (§11):",
        );
        println!("    computed trust {}", dir.display());
    }
}

/// What the clone already has, reported rather than set again.
fn carried_over(dir: &Path) {
    match git(dir, &["config", "core.hooksPath"], None) {
        Ok(hooks) if !hooks.trim().is_empty() => say(&format!(
            "carried over: core.hooksPath={}, which is repository config",
            hooks.trim()
        )),
        _ => say(
            "core.hooksPath is unset, so pre-commit and pre-push are not wired; the session hook points it at scripts/hooks",
        ),
    }
    if dir.join(".claude/settings.json").is_file() {
        say(
            "carried over: .claude/settings.json, which is tracked, so the plugin settings are the clone's",
        );
    } else {
        say(".claude/settings.json is missing, so this worktree loads no harness plugin");
    }
    if dir.join("oracle.toml").is_file() {
        say(
            "carried over: oracle.toml, which is tracked, and the php-src tree it pins, which is outside the clone; `xtask phpt` says if that tree is gone",
        );
    } else {
        say("oracle.toml is missing, so this worktree has no oracle and no pinned php-src");
    }
}
