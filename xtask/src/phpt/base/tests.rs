//! The base's worktree goes however the call that laid it ends (#3110).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::Laid;
use crate::git::git;
use crate::scratch::{Tree, committing};

/// A repository with one commit, and where a base's worktree is laid
/// beside it.
fn repository(name: &str) -> (Tree, Tree) {
    let (main, beside) = (
        Tree::new(name).unwrap(),
        Tree::new(&format!("{name}-beside")).unwrap(),
    );
    committing(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("Cargo.toml"), "[workspace]\n").unwrap();
    committing(&main, &["add", "--", "Cargo.toml"]);
    committing(&main, &["commit", "-q", "-m", "base"]);
    (main, beside)
}

/// Whether git lists a worktree at `dir`.
fn listed(root: &Path, dir: &Path) -> bool {
    let list = git(root, &["worktree", "list", "--porcelain"], None).unwrap();
    list.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|at| Path::new(at).ends_with(dir.file_name().unwrap()))
    })
}

/// The process that owns a base's worktree stopped by a signal, which no
/// drop outlives, leaves no worktree behind: the watcher removes it once
/// the owner is gone, and git lists it no more.
#[test]
fn a_base_run_stopped_by_a_signal_leaves_no_worktree_behind() {
    let (main, beside) = repository("base-signalled");
    let dir = beside.join("base-0000000-1");
    let mut owner = Command::new("sleep").arg("60").spawn().unwrap();
    let laid = Laid::at(&main, &dir, "HEAD", owner.id()).unwrap();
    assert!(dir.is_dir() && listed(&main, &dir));
    let pid = nix::unistd::Pid::from_raw(i32::try_from(owner.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    owner.wait().unwrap();
    let waited = Instant::now();
    while dir.exists() && waited.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!dir.exists(), "the worktree outlived its owner");
    assert!(!listed(&main, &dir), "git still lists the worktree");
    drop(laid);
}

/// A call that returns early or panics drops its worktree, and one that
/// runs to the end removes it.
#[test]
fn a_base_run_that_returns_early_drops_its_worktree() {
    let (main, beside) = repository("base-dropped");
    let (dropped, removed) = (beside.join("base-0000000-2"), beside.join("base-0000000-3"));
    let laid = Laid::at(&main, &dropped, "HEAD", std::process::id()).unwrap();
    drop(laid);
    assert!(!dropped.exists() && !listed(&main, &dropped));
    let laid = Laid::at(&main, &removed, "HEAD", std::process::id()).unwrap();
    laid.remove().unwrap();
    assert!(!removed.exists() && !listed(&main, &removed));
}
