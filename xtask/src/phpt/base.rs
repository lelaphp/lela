//! `xtask phpt --base [REF]`: the runs `--against` holds a subset to,
//! taken at the merge base in one call (#2364). A run is kept at the tree
//! the engine is built from ([`against`]), so the merge base's is taken
//! by a checkout of that commit with its engine built, and it outlives
//! that checkout only once it is carried into the main worktree.
//!
//! The call does each of those steps: the merge base of `HEAD` with
//! `REF`, `main` unless named, as `done --base` takes it; the subset
//! named, or each of the touched crates the per-commit lane runs a subset
//! of ([`done::measured_by_the_lane`]), less the ones a run is kept for
//! at that base already, so a base one call took serves every later one
//! while the engine's tree stands; a detached worktree at the base beside
//! the others ([`worktree::homes`]), its `target/<lane profile>` seeded
//! from this checkout's and every host built there in the lanes' profile
//! ([`profile::build_hosts`]); each subset run there under this
//! checkout's manifest and registry, so the two runs are over one corpus
//! and one set of tests and differ in the engine alone; the runs carried
//! into the main worktree ([`worktree::carry_runs`]) and the worktree
//! removed, whatever the build or the runs came to. The readings of the
//! runs at the base are printed and not recorded: they are the base's,
//! and what the call keeps is the runs. Its exit is 1 while a subset it
//! was asked for has no run kept at the base afterwards, naming why.

use std::path::Path;

use anyhow::{Context, Result, bail};

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
) -> Result<()> {
    let base = Base::of(root, Some(since))?;
    let commit = short(&base.commit);
    let source = manifest.pin.source.tree()?;
    let names = match &ask.subset {
        Some(name) => vec![name.clone()],
        None => touched(root, manifest, &base)?,
    };
    let mut wanted = Vec::new();
    for name in names {
        let scope = Subset::resolve(manifest, &source, &name)?.scope;
        if against::kept_at(root, &manifest.pin, &base.commit, &scope)?.is_ok() {
            say(&format!("a run of {scope} is kept at {base} already"));
        } else {
            wanted.push(scope);
        }
    }
    if wanted.is_empty() {
        say(&format!("no run to take at {base}"));
        return Ok(());
    }
    let main = track::main_worktree(root)?;
    let dir = worktree::homes(&main)?.join(format!("base-{commit}-{}", std::process::id()));
    let at = dir.to_string_lossy();
    git(
        root,
        &["worktree", "add", "-q", "--detach", &at, &base.commit],
        None,
    )
    .with_context(|| format!("laying a worktree at {base} down at {at}"))?;
    say(&format!("laid {at} at {base}"));
    let ran = run_at(root, &dir, manifest, registry, ask, &wanted);
    let carried = worktree::carry_runs(&dir, BASE);
    let removed = git(root, &["worktree", "remove", "--force", &at], None)
        .with_context(|| format!("removing the worktree at {at}"));
    ran?;
    carried?;
    removed?;
    say(&format!("removed {at}"));
    let mut missing = Vec::new();
    for scope in &wanted {
        if against::kept_at(root, &manifest.pin, &base.commit, scope)?.is_err() {
            missing.push(scope.as_str());
        }
    }
    if !missing.is_empty() {
        bail!(
            "{} of {} runs asked for are not kept at {base}: {}; the readings at the base above say why",
            missing.len(),
            wanted.len(),
            listed(&missing)
        );
    }
    let kept: Vec<&str> = wanted.iter().map(String::as_str).collect();
    say(&format!("kept: {} at {base}", listed(&kept)));
    Ok(())
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
    profile::build_hosts(dir)?;
    for scope in scopes {
        let ask = Ask {
            subset: Some(scope.clone()),
            census: ask.census,
            jobs: ask.jobs,
            locks: ask.locks.clone(),
            ..Ask::default()
        };
        for reading in super::run(dir, manifest, registry, &ask)? {
            say(&format!("at the base, {}", reading.message));
        }
    }
    Ok(())
}

fn say(line: &str) {
    println!("{BASE}: {line}");
}
