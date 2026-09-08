//! `xtask clippy` (harness-design.md §4.5 condition 1, §6): `cargo clippy`
//! with every warning denied, on every target of one crate, as a reading.
//! Clippy's verdict is cargo's; what this instrument adds is the reading
//! `done` reads, stamped with the crate's tree, so a crate changed since
//! its last clean run is stale and not clean. One reading per crate that
//! exists; `--crate` narrows it to the ones named.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::git::{self, Stamp};
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status};
use crate::sources;

/// The instrument name the reading carries.
pub const INSTRUMENT: &str = "clippy";

/// The cargo package of a manifest crate: `php_ext_<name>` for
/// `ext/<name>` (ADR-0004), the name itself otherwise.
pub fn package(krate: &Crate) -> String {
    match krate.name.strip_prefix("ext/") {
        Some(name) => format!("php_ext_{name}"),
        None => krate.name.clone(),
    }
}

/// Run clippy over every crate that exists, or over `only`; one reading
/// each. The cargo invocation is the one CI and the pre-commit hook run.
pub fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    for name in only {
        if manifest.crate_named(name).is_none() {
            bail!("the manifest has no crate named {name:?}");
        }
    }
    let mut readings = Vec::new();
    for krate in &manifest.crates {
        if !only.is_empty() && !only.iter().any(|n| n == &krate.name) {
            continue;
        }
        if !sources::exists(root, krate) {
            continue;
        }
        readings.push(check(root, krate)?);
    }
    Ok(readings)
}

/// `--staged`: the crates with a staged change, pre-commit's touched
/// crates (harness-design.md §6); no readings when no crate is touched.
pub fn run_staged(root: &Path, manifest: &Manifest) -> Result<Vec<Reading>> {
    let staged = git::staged(root)?;
    let paths: Vec<&str> = staged.iter().map(|c| c.path.as_str()).collect();
    let touched: Vec<String> = manifest
        .crates_of(&paths)
        .into_iter()
        .map(|c| c.name.clone())
        .collect();
    if touched.is_empty() {
        println!("clippy: no crate has a staged change");
        return Ok(Vec::new());
    }
    run(root, manifest, &touched)
}

/// One crate's reading: pass when cargo exits 0, fail with the tail of
/// what clippy printed otherwise.
pub fn check(root: &Path, krate: &Crate) -> Result<Reading> {
    let package = package(krate);
    let out = Command::new("cargo")
        .args([
            "clippy",
            "-q",
            "-p",
            &package,
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ])
        .current_dir(root)
        .output()
        .with_context(|| format!("running cargo clippy -p {package}"))?;
    let mut reading = Reading::new(INSTRUMENT, &krate.name);
    if out.status.success() {
        reading.status = Status::Pass;
        reading.message = format!("{}: clippy clean, warnings denied", krate.name);
    } else {
        let text = String::from_utf8_lossy(&out.stderr);
        let warnings = text.lines().filter(|l| l.starts_with("warning:")).count();
        let errors = text.lines().filter(|l| l.starts_with("error")).count();
        reading.count("warnings", warnings as u64)?;
        reading.count("errors", errors as u64)?;
        reading.status = Status::Fail;
        reading.message = format!("{}: clippy is not clean\n{}", krate.name, tail(&text, 40));
    }
    reading.stamp(&Stamp::take(root, &krate.path())?);
    Ok(reading)
}

/// The last `n` lines of `text`.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn krate(name: &str) -> Crate {
        toml::from_str(&format!("name = \"{name}\"\nbudget = 1")).unwrap()
    }

    #[test]
    fn the_package_is_the_cargo_name() {
        assert_eq!(package(&krate("xtask")), "xtask");
        assert_eq!(package(&krate("php_heap")), "php_heap");
        assert_eq!(package(&krate("ext/pcre")), "php_ext_pcre");
    }

    #[test]
    fn the_tail_is_the_last_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }
}
