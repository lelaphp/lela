//! `xtask clippy` (harness-design.md §4.5 condition 1, §6): `cargo clippy`
//! on every target of each crate, every warning a failure, as a reading
//! per crate. What this instrument adds is the reading `done` reads,
//! stamped with the crate's tree, so a crate changed since its last clean
//! run is stale and not clean. One reading per crate that exists;
//! `--crate` narrows it to the ones named.
//!
//! One `cargo clippy --keep-going --all-targets` checks every crate at
//! once, cargo checking crates side by side (#2165). Warnings are not
//! denied on the command line, so a lint stays a warning and the crates
//! built on the one that carries it are still checked; each warning is
//! counted against the crate it is of, by its package ([`batch::Run`]), and
//! a crate fails on any. The verdict over a crate is the one `-D warnings`
//! gave it, and one crate's lint no longer blocks the crates above it.

use std::path::Path;

use anyhow::{Result, bail};

use crate::batch::{Run, Targets, tail};
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status, Store};
use crate::sources;

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "clippy";

/// Run clippy over every crate that exists, or over `only`; one reading
/// each.
pub fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    for name in only {
        if manifest.crate_named(name).is_none() {
            bail!("the manifest has no crate named {name:?}");
        }
    }
    // The key each reading carries is the tree's before cargo built it,
    // so a file written while the crates are measured stales the reading.
    let snapshot = Snapshot::disk(root)?;
    let crates: Vec<&Crate> = manifest
        .crates
        .iter()
        .filter(|k| only.is_empty() || only.iter().any(|n| n == &k.name))
        .filter(|k| sources::exists(root, k))
        .collect();
    if crates.is_empty() {
        return Ok(Vec::new());
    }
    let checked = Run::of(
        root,
        &["clippy", "--keep-going", "--all-targets"],
        &crates,
        Targets::All,
    )?;
    crates
        .iter()
        .map(|krate| reading(&snapshot, krate, &checked))
        .collect()
}

/// `--staged`: the crates with a staged change, pre-commit's touched
/// crates (harness-design.md §6), less those whose reading stands for
/// what is staged (#1143); no readings when none is left.
pub fn run_staged(root: &Path, manifest: &Manifest, store: &Store) -> Result<Vec<Reading>> {
    let measured = sources::for_commit(root, manifest, store, INSTRUMENT)?;
    if measured.is_empty() {
        return Ok(Vec::new());
    }
    run(root, manifest, &measured)
}

/// The command line's entry: the crates named, or with `--staged` the
/// ones with a staged change, and every reading written.
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    crates: &[String],
    staged: bool,
) -> Result<()> {
    let readings = if staged {
        run_staged(root, manifest, store)?
    } else {
        run(root, manifest, crates)?
    };
    crate::record(store, &readings)
}

/// One crate's reading: pass when every target was checked and clippy
/// said nothing of the crate, fail with what it said otherwise.
fn reading(snapshot: &Snapshot, krate: &Crate, run: &Run) -> Result<Reading> {
    let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
    let name = &krate.name;
    let Some(checked) = run.crates.get(name) else {
        reading.status = Status::Fail;
        reading.message = format!("{name}: clippy did not check it\n{}", tail(&run.stderr, 40));
        return Ok(reading);
    };
    let warnings = checked.count("warning");
    let errors = checked.count("error");
    if checked.whole && warnings == 0 && errors == 0 {
        reading.status = Status::Pass;
        reading.message = format!("{name}: clippy clean, warnings denied");
        return Ok(reading);
    }
    reading.count("warnings", warnings as u64)?;
    reading.count("errors", errors as u64)?;
    reading.status = Status::Fail;
    let said = checked.rendered();
    reading.message = if said.is_empty() {
        format!(
            "{name}: clippy did not check it: a crate it is built from did not compile\n{}",
            tail(&run.stderr, 40)
        )
    } else {
        format!("{name}: clippy is not clean\n{}", tail(&said, 40))
    };
    Ok(reading)
}
