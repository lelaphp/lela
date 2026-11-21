//! `xtask phpt --base [REF]`: the runs `--against` holds a subset to,
//! taken at the merge base in one call. A run is kept at the tree
//! the engine is built from ([`against`]), so the merge base's is taken
//! by a checkout of that commit with its engine built, and it outlives
//! that checkout only once it is carried into the main worktree.
//!
//! The call does each of those steps: the merge base of `HEAD` with
//! `REF`, `main` unless named, as `done --base` takes it; the subset
//! named, all of the per-commit lane with `--per-commit`, or each of the
//! touched crates the per-commit lane runs a subset of
//! ([`done::measured_by_the_lane`]), less the ones a run is kept for
//! at that base already, so a base one call took serves every later one
//! while the engine's tree stands; a detached worktree at the base beside
//! the others ([`worktree::homes`]), its `target/<lane profile>` seeded
//! from this checkout's and every host built there in the lanes' profile
//! ([`profile::build_hosts`]); each subset run there under this
//! checkout's manifest and registry, so the two runs are over one corpus
//! and one set of tests and differ in the engine alone; the runs carried
//! into the main worktree ([`worktree::carry_runs`]) and the worktree
//! removed, whatever the build or the runs came to, the call stopped by a
//! signal among them ([`Laid`]). The readings of the
//! runs at the base are printed and not recorded: they are the base's,
//! and what the call keeps is the runs. While a subset it was asked for
//! has no run kept at the base afterwards, a subset refused there among
//! the causes, it could not run to its end, and exits so
//! (`exit::UNFINISHED`), naming why.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use super::{Ask, Subset, against};
use crate::carried::Carried;
use crate::divergence::Registry;
use crate::done::{self, Base};
use crate::git::{self, git};
use crate::manifest::Manifest;
use crate::oracle::profile;
use crate::reading::listed;
use crate::track::{self, short};
use crate::worktree;

/// The prefix of every line the call prints.
const BASE: &str = "phpt --base";

/// The call that takes the run of `scope` at `commit`: what the skip of a
/// base with no run kept names.
pub(crate) fn call(commit: &str, scope: &str) -> String {
    format!("cargo xtask phpt --base {commit} --subset {scope}")
}

/// Take the runs `ask` names, or the touched crates', at the merge base
/// of the workspace at `root` with `since`, and keep them in the main
/// worktree.
pub(crate) fn take(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
    since: &str,
    per_commit: bool,
) -> Result<()> {
    let base = Base::of(root, Some(since))?;
    let commit = short(&base.commit);
    let wanted = wanted(root, manifest, ask, &base, per_commit)?;
    if wanted.is_empty() {
        say(&format!("no run to take at {base}"));
        return Ok(());
    }
    let main = track::main_worktree(root)?;
    let dir = worktree::homes(&main)?.join(format!("base-{commit}-{}", std::process::id()));
    let laid = Laid::at(root, &dir, &base.commit, std::process::id())
        .with_context(|| format!("laying a worktree at {base} down at {}", dir.display()))?;
    say(&format!("laid {} at {base}", dir.display()));
    let ran = run_at(root, &dir, manifest, registry, ask, &wanted);
    let carried = worktree::carry_runs(&dir, BASE);
    let removed = laid.remove();
    ran?;
    carried?;
    removed?;
    say(&format!("removed {}", dir.display()));
    kept(root, manifest, &base, &wanted)?;
    say(&format!("kept: {} at {base}", listed(&wanted)));
    Ok(())
}

/// Nothing where every run of `wanted` is kept at `base`; else the call
/// could not run to its end, a subset refused at the base among the
/// causes, naming the runs not kept.
fn kept(root: &Path, manifest: &Manifest, base: &Base, wanted: &[String]) -> Result<()> {
    let mut missing = Vec::new();
    for scope in wanted {
        if against::kept_at(root, manifest, &base.commit, scope)?.is_err() {
            missing.push(scope.as_str());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let why = anyhow::anyhow!(
        "{} of {} runs asked for are not kept at {base}: {}; the readings at the base above say why",
        missing.len(),
        wanted.len(),
        listed(&missing)
    );
    Err(crate::exit::Unfinished::of(super::INSTRUMENT, why))
}

/// The selected scopes without a valid run under the caller's recipe.
fn wanted(
    root: &Path,
    manifest: &Manifest,
    ask: &Ask,
    base: &Base,
    per_commit: bool,
) -> Result<Vec<String>> {
    let source = manifest.pin()?.source.tree()?;
    let names = match &ask.subset {
        Some(name) => vec![name.clone()],
        None if per_commit => Subset::lane(manifest, &Carried::binary(root)?, &source)
            .into_iter()
            .map(|subset| subset.scope)
            .collect(),
        None => touched(root, manifest, base)?,
    };
    let mut wanted = Vec::new();
    for name in names {
        let subset = Subset::resolve(manifest, &source, &name)?;
        if per_commit && subset.testless(manifest)? {
            say(&format!("{} has no tests of its own", subset.scope));
            continue;
        }
        let scope = subset.scope;
        if against::kept_at(root, manifest, &base.commit, &scope)?.is_ok() {
            say(&format!("a run of {scope} is kept at {base} already"));
        } else {
            wanted.push(scope);
        }
    }
    Ok(wanted)
}

/// The touched crates the per-commit lane runs a subset of, since `base`.
fn touched(root: &Path, manifest: &Manifest, base: &Base) -> Result<Vec<String>> {
    let changed = git::changed_since(root, &base.commit)?;
    let paths: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    let carried = Carried::binary(root)?;
    let crates: Vec<String> = manifest
        .crates_of(&paths)
        .into_iter()
        .filter(|krate| done::measured_by_the_lane(krate, &carried))
        .map(|krate| krate.name.clone())
        .collect();
    if crates.is_empty() {
        say(&format!(
            "no crate touched since {base} owns a subset the per-commit lane runs"
        ));
    }
    Ok(crates)
}

/// In the worktree at `dir`: the lanes' build seeded from `root`'s, every
/// host built, and each of `scopes` run, its readings printed.
fn run_at(
    root: &Path,
    dir: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
    scopes: &[String],
) -> Result<()> {
    let build = profile::LANE.dir();
    match worktree::seed_build(dir, root, build) {
        Ok(true) => say(&format!(
            "seeded target/{build} from {}'s by cp -c",
            root.display()
        )),
        Ok(false) => say(&format!(
            "{} holds no target/{build}, so the build starts cold",
            root.display()
        )),
        Err(why) => say(&format!("{why}; the build starts cold")),
    }
    profile::build_hosts(dir, profile::LANE)?;
    for scope in scopes {
        let ask = Ask {
            subset: Some(scope.clone()),
            census: ask.census,
            jobs: ask.jobs,
            locks: ask.locks.clone(),
            ..Ask::default()
        };
        for reading in super::run(dir, manifest, registry, &ask)?.readings {
            say(&format!("at the base, {}", reading.message));
        }
    }
    Ok(())
}

/// A detached worktree laid down for a base's runs, which goes however the
/// call that laid it ends: [`Laid::remove`] where it runs to the
/// end, its drop where it returns early or panics, and, where the process
/// that owns it is stopped by a signal, which no drop outlives, a watcher
/// in a process group of its own, so the terminal's interrupt does not
/// reach it, that removes the worktree once the owner is gone. Before
/// this, a `--base` run stopped by its caller left its worktree, a
/// checkout and a build of about 3 GB, standing.
pub(crate) struct Laid {
    root: PathBuf,
    dir: PathBuf,
    removed: bool,
}

impl Laid {
    /// `commit` laid down at `dir`, detached, by the repository at `root`,
    /// watched for the process `owner`.
    pub(crate) fn at(root: &Path, dir: &Path, commit: &str, owner: u32) -> Result<Laid> {
        let at = dir.to_str().context("the worktree's path is not UTF-8")?;
        git(root, &["worktree", "add", "-q", "--detach", at, commit])?;
        let laid = Laid {
            root: root.to_path_buf(),
            dir: dir.to_path_buf(),
            removed: false,
        };
        Command::new("sh")
            .args(["-c", WATCH, "watch"])
            .arg(owner.to_string())
            .args([&laid.root, &laid.dir])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("starting the worktree's watcher")?;
        Ok(laid)
    }

    /// The worktree removed now, or why it could not be.
    pub(crate) fn remove(mut self) -> Result<()> {
        self.removed = true;
        self.unlay()
    }

    fn unlay(&self) -> Result<()> {
        let at = self.dir.to_string_lossy();
        let removed = git(&self.root, &["worktree", "remove", "--force", &at]);
        removed
            .with_context(|| format!("removing the worktree at {at}"))
            .map(drop)
    }
}

impl Drop for Laid {
    fn drop(&mut self) {
        if !self.removed {
            self.unlay().ok();
        }
    }
}

/// What the watcher runs: while the owner `$1` lives, wait; then, where
/// the worktree `$3` still stands, have the repository at `$2` remove it.
const WATCH: &str = "while kill -0 \"$1\" 2>/dev/null; do sleep 1; done; \
    if [ -d \"$3\" ]; then git -C \"$2\" worktree remove --force \"$3\"; fi";

fn say(line: &str) {
    println!("{BASE}: {line}");
}

#[cfg(test)]
mod tests;
