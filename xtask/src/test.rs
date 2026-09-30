//! `xtask test` (harness-design.md §4.5 condition 1, §6): `cargo test` on
//! one crate, as a reading. The verdict is cargo's, the same `cargo test
//! -p <crate>` a session runs by hand; what this instrument adds is the
//! reading `done` reads, stamped with the crate's tree, so a crate changed
//! since its tests last passed is stale and not green, which a bare
//! `cargo test` in a hook has no way to say (#191). One reading per crate
//! that exists; `--crate` narrows it to the ones named, `--staged` to the
//! crates with a staged change. The numbers are cargo's own sums over
//! every test binary of the crate: `passed`, `failed`, `ignored` and
//! `filtered`; a crate whose tests do not build has none and fails all
//! the same.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status, Store, plural};
use crate::sources;

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "test";

/// Run the tests of every crate that exists, or of `only`; one reading
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

/// One crate's reading: pass when cargo exits 0, fail with the tail of
/// what it printed otherwise, the numbers summed off the lines cargo's
/// harness prints per test binary either way.
pub(crate) fn check(root: &Path, krate: &Crate) -> Result<Reading> {
    let package = krate.package();
    let mut cargo = Command::new("cargo");
    cargo.args(["test", "-q", "-p", &package]).current_dir(root);
    // Pre-commit runs this, and git exports `GIT_DIR`, `GIT_INDEX_FILE`
    // and their kin to a hook; a test that lays a scratch repository
    // down would otherwise write into the repository being committed to.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cargo.env_remove(&key);
        }
    }
    let out = cargo
        .output()
        .with_context(|| format!("running cargo test -p {package}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let counts = Counts::of(&stdout);
    let mut reading = Reading::for_crate(INSTRUMENT, krate, root)?;
    reading.count("passed", counts.passed)?;
    reading.count("failed", counts.failed)?;
    reading.count("ignored", counts.ignored)?;
    reading.count("filtered", counts.filtered)?;
    let name = &krate.name;
    if out.status.success() {
        reading.status = Status::Pass;
        reading.message = format!(
            "{name}: {} test{} passed",
            counts.passed,
            plural(usize::try_from(counts.passed).unwrap_or(usize::MAX))
        );
    } else {
        reading.status = Status::Fail;
        let what = if counts.binaries == 0 {
            "the tests did not build".to_owned()
        } else {
            format!(
                "{} test{} failed",
                counts.failed,
                plural(usize::try_from(counts.failed).unwrap_or(usize::MAX))
            )
        };
        reading.message = format!("{name}: {what}\n{}", tail(&format!("{stdout}{stderr}"), 40));
    }
    Ok(reading)
}

/// The sums over every `test result:` line cargo's harness prints, one
/// per test binary run.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    binaries: u64,
    passed: u64,
    failed: u64,
    ignored: u64,
    filtered: u64,
}

impl Counts {
    /// `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0
    /// filtered out; finished in 0.00s`, summed over the lines of `text`
    /// that begin that way; a number the line does not carry counts as
    /// nothing.
    fn of(text: &str) -> Counts {
        let mut counts = Counts::default();
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("test result:") else {
                continue;
            };
            counts.binaries += 1;
            // `ok.` or `FAILED.` stands before the first number.
            let rest = rest.split_once(". ").map_or(rest, |(_, r)| r);
            for field in rest.split(';') {
                let mut words = field.split_whitespace();
                let Some(n) = words.next().and_then(|w| w.parse::<u64>().ok()) else {
                    continue;
                };
                match words.next() {
                    Some("passed") => counts.passed += n,
                    Some("failed") => counts.failed += n,
                    Some("ignored") => counts.ignored += n,
                    Some("filtered") => counts.filtered += n,
                    _ => {}
                }
            }
        }
        counts
    }
}

/// The last `n` lines of `text`.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Counts, tail};

    #[test]
    fn the_counts_are_summed_over_every_test_binary() {
        let text = "\nrunning 2 tests\n..\ntest result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 3 filtered out; finished in 0.00s\n\n\nrunning 1 test\nF\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        assert_eq!(
            Counts::of(text),
            Counts {
                binaries: 2,
                passed: 2,
                failed: 1,
                ignored: 1,
                filtered: 3,
            }
        );
    }

    #[test]
    fn a_build_that_failed_prints_no_line_and_counts_nothing() {
        assert_eq!(
            Counts::of("error[E0425]: cannot find value `x`\n"),
            Counts::default()
        );
    }

    #[test]
    fn the_tail_is_the_last_lines() {
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a", 5), "a");
    }
}
