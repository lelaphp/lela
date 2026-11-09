//! The land's judges, built at main's tip (#4372, #4348 ruling 5).
//!
//! The merge lanes in `_merge` run the xtask `staging` builds, so the
//! readings of `budget`, `api-check` and `unsafe-audit` that `done` held a
//! merge to were the merge's own harness's, and a land run from `_merge`
//! asked `done`, and the `permit --diff` inside it, of that same build: a
//! branch could weaken the judge that judged it. So [`super::land`] has an
//! xtask built at main's tip judge `_merge` ([`Judge::over`]), built once
//! per tip and kept under main's `target/judge/<tip>/` ([`Judge::at`]).
//! What that build runs is `xtask worktree judge` ([`run`]): it reads the
//! harness's own files with main's readers, takes `budget`, `api-check`
//! and `unsafe-audit` over `_merge`'s tree, judges each harness file the
//! merge changed with steer's judge ([`crate::steer::since`]), and asks
//! `done` since main's tip. `test`, `clippy`, `witness` and `miri` still
//! build and measure the change, as the merge lanes take them, and `done`
//! holds their readings as ever.
//!
//! A merge whose harness files main's readers cannot read is refused with
//! the two steps that land it ([`TWO_STEPS`]): first the xtask that reads
//! the new grammar, then the row that uses it. #4343 was such a merge, and
//! landed from `_merge` by hand.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::manifest::Manifest;
use crate::reading::{Reading, Status, Store};
use crate::track::short;
use crate::worktree::say;
use crate::{api_check, budget, done, steer, unsafe_audit};

/// Where the judges are built and kept, under main's checkout: one
/// directory per tip holding its `xtask`, the cargo target directory they
/// are built in, `build`, and the tip's tree they are built from, `src`,
/// while a build stands.
const HOME: &str = "target/judge";

/// What a land refuses a merge whose harness files main's xtask cannot
/// read with: the two steps such a change lands in (merging.md).
pub(crate) const TWO_STEPS: &str = "a change to the grammar of the harness's own files lands in two \
     steps: first the xtask that reads the new grammar, merged and landed with no row that uses it; \
     then, once main's xtask reads it, the row that does (.claude/skills/specs/merging.md)";

/// What the judges say a line as.
const JUDGE: &str = "worktree judge";

/// An xtask built at one tip of `main`, the binary that judges a merge
/// into it.
pub(crate) struct Judge {
    binary: PathBuf,
    tip: String,
}

impl Judge {
    /// The xtask built at `tip` of the repository whose main checkout is
    /// `main`: the one kept under [`HOME`] where it stands, else built
    /// there from the tip's tree, laid down by `git archive`, which no
    /// worktree's edits reach, and the builds kept for other tips removed.
    pub(crate) fn at(main: &Path, tip: &str) -> Result<Judge> {
        let home = main.join(HOME);
        let binary = home.join(tip).join("xtask");
        let judge = Judge {
            binary: binary.clone(),
            tip: tip.to_owned(),
        };
        if binary.is_file() {
            say!(
                super::LAND,
                "main's xtask at {} judges, built at {}",
                short(tip),
                binary.display()
            );
            return Ok(judge);
        }
        say!(
            super::LAND,
            "building main's xtask at {} in {} to judge the merge",
            short(tip),
            home.display()
        );
        let src = home.join("src");
        lay(main, tip, &src)?;
        let built = build(&src, &home.join("build"))?;
        crate::make_parent(&binary)?;
        fs::copy(&built, &binary)
            .with_context(|| format!("copying {} to {}", built.display(), binary.display()))?;
        remove_dir(&src)?;
        prune(&home, tip)?;
        Ok(judge)
    }

    /// [`run`] over `merge` since this judge's tip, by this judge: what it
    /// prints on this process's streams, and an error where it refuses.
    pub(crate) fn over(&self, merge: &Path) -> Result<()> {
        let status = Command::new(&self.binary)
            .arg("--root")
            .arg(merge)
            .args(["worktree", "judge", "--base", &self.tip])
            .current_dir(merge)
            .env_remove("CARGO_MANIFEST_DIR")
            .status()
            .with_context(|| format!("running {}", self.binary.display()))?;
        if !status.success() {
            bail!(
                "main's xtask at {} refuses {} ({status})",
                short(&self.tip),
                merge.display()
            );
        }
        Ok(())
    }
}

/// `tip`'s tree laid down at `src` afresh, from the object store of the
/// repository at `main`.
fn lay(main: &Path, tip: &str, src: &Path) -> Result<()> {
    remove_dir(src)?;
    crate::make_dir(src)?;
    let mut archive = Command::new("git")
        .args(["archive", "--format=tar", tip])
        .current_dir(main)
        .stdout(Stdio::piped())
        .spawn()
        .context("git did not run")?;
    let tar = archive.stdout.take().context("git archive's output")?;
    let unpacked = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(src)
        .stdin(tar)
        .status()
        .context("tar did not run")?;
    let archived = archive.wait().context("git archive")?;
    if !archived.success() || !unpacked.success() {
        bail!(
            "laying {} down at {} failed: git archive {archived}, tar {unpacked}",
            short(tip),
            src.display()
        );
    }
    Ok(())
}

/// `xtask` built from the tree at `src` into the target directory
/// `target`, on the cargo `CARGO` names or the one on `PATH`; where the
/// binary is.
fn build(src: &Path, target: &Path) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "--locked",
            "--package",
            "xtask",
            "--bin",
            "xtask",
        ])
        .arg("--target-dir")
        .arg(target)
        .current_dir(src)
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .context("cargo did not run")?;
    let built = target.join("debug").join("xtask");
    if !status.success() || !built.is_file() {
        bail!(
            "cargo build of xtask in {} failed ({status}); main is untouched",
            src.display()
        );
    }
    Ok(built)
}

/// Every judge kept under `home` for a tip other than `tip` removed, so
/// one build is kept, the current tip's.
fn prune(home: &Path, tip: &str) -> Result<()> {
    for dir in crate::entries(home)? {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let a_tip = name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit());
        if a_tip && name != tip {
            remove_dir(&dir)?;
        }
    }
    Ok(())
}

/// The directory `dir` removed with what it holds, where it stands.
fn remove_dir(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

/// `xtask worktree judge --base <tip>`: the land's judgement of the
/// checkout at `root`, `_merge` when [`Judge::over`] asks it, since
/// `base`, main's tip, by this build. The harness's own files read with
/// this build's readers, refused with [`TWO_STEPS`] where one does not
/// read; `budget`, `api-check` and `unsafe-audit` taken over the tree and
/// recorded in its store, whichever way they read, since `done` decides
/// what a failing one holds; each harness file the commits since `base`
/// changed judged by steer, printed and not recorded, a proposal that
/// would not load refused;
/// and `done` since `base`, as
/// land's caller, whose verdict is the exit.
pub(crate) fn run(root: &Path, base: &str) -> Result<()> {
    let manifest = Manifest::load(root).map_err(|err| unread(root, base, &err))?;
    let registry = Registry::load(root).map_err(|err| unread(root, base, &err))?;
    let store = Store::of_workspace(root);
    let mut readings = budget::measure(root, &manifest, &store, false)?;
    readings.extend(api_check::hold(&manifest, root, false)?);
    readings.extend(unsafe_audit::measure(root, &manifest, &store, false)?);
    store.record(&readings)?;
    let steered = steer::since(root, base)?;
    readings.extend(steered.iter().cloned());
    tell(&readings);
    if let Some(unloaded) = steered.iter().find(|r| r.status == Status::Fail) {
        bail!("{}", unloaded.message);
    }
    done::run(
        root,
        &manifest,
        &registry,
        &store,
        (Some(base), done::Caller::Land),
        &[],
    )
}

/// The refusal of a merge whose harness files this build cannot read.
fn unread(root: &Path, base: &str, err: &anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "main's xtask at {} cannot read {}'s harness files: {err:#}. {TWO_STEPS}",
        short(base),
        root.display()
    )
}

/// What the judges measured, a line per instrument and a line per reading
/// that did not pass.
fn tell(readings: &[Reading]) {
    let mut instruments: Vec<&str> = readings.iter().map(|r| r.instrument.as_str()).collect();
    instruments.dedup();
    for instrument in instruments {
        let of: Vec<&Reading> = (readings.iter())
            .filter(|r| r.instrument == instrument)
            .collect();
        let passed = of.iter().filter(|r| r.status == Status::Pass).count();
        say!(JUDGE, "{instrument}: {passed} of {} pass", of.len());
        for reading in of.iter().filter(|r| r.status != Status::Pass) {
            println!("{reading}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Building a judge keeps the current tip's alone: every other tip's
    /// directory goes, and what is not a tip's stays.
    #[test]
    fn a_prune_keeps_the_current_tips_judge_and_what_is_not_a_tips() {
        let tree = crate::scratch::Tree::new("judge-prune").unwrap();
        let home = tree.join("judge");
        let (tip, old) = ("a".repeat(40), "b".repeat(40));
        for dir in [tip.as_str(), old.as_str(), "build", "src"] {
            crate::make_dir(&home.join(dir)).unwrap();
        }
        prune(&home, &tip).unwrap();
        let mut left: Vec<String> = crate::entries(&home)
            .unwrap()
            .iter()
            .filter_map(|d| d.file_name()?.to_str().map(str::to_owned))
            .collect();
        left.sort();
        assert_eq!(left, [tip, "build".to_owned(), "src".to_owned()]);
    }
}
