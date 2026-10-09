//! `xtask miri` (third-engine-design.md §9's "Miri on `php_heap`'s test
//! suite per commit"; harness-design.md §7's Miri row, #1916): a crate's
//! library and integration tests run under Miri, as one reading per crate
//! with what ran, failed and was ignored in its numbers. The crate is
//! `php_heap` unless `--crate` names others.
//!
//! **What a run is.** `cargo miri test` on the pinned nightly
//! ([`TOOLCHAIN`]), one test at a time. Miri ends the whole test binary at
//! the first undefined behaviour or unsupported operation it meets, so one
//! such test would hide every test after it: the test the run ended in is
//! the last line libtest began and never finished, and the run goes again
//! with that test and every test already judged skipped, until a run ends
//! with none in flight. The doctests are not run.
//!
//! **What it runs under, said and never chosen silently.** Miri's own
//! defaults unless `MIRIFLAGS` says otherwise: Stacked Borrows, and
//! permissive provenance, under which `php_heap`'s pointers carried
//! through integers (`expose_provenance`, `with_exposed_provenance_mut`)
//! are allowed with a warning. `-Zmiri-strict-provenance` makes each of
//! them an unsupported operation, and `-Zmiri-tree-borrows` the other
//! aliasing model. The reading's message names the model and the
//! provenance it ran under.
//!
//! **What the nightly costs.** The pinned nightly predates the
//! workspace's Rust ([`FEATURES`]), so the language and library features
//! the workspace uses that it had not stabilised are turned on by
//! `-Zcrate-attr`, and `--ignore-rust-version` lets it build a crate whose
//! `rust-version` is later than itself. A test Miri cannot run at all
//! carries `#[cfg_attr(miri, ignore = "<why>")]` in its source, the reason
//! with it.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::batch::{self, tail};
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status, Store};
use crate::sources;

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "miri";

/// The nightly Miri runs on, by rustup's name; CI's job installs the same
/// one with its `miri` component.
pub const TOOLCHAIN: &str = "nightly-2025-01-08";

/// The features the workspace uses that [`TOOLCHAIN`] had not stabilised,
/// turned on in every crate the run builds.
pub const FEATURES: &str = "let_chains,integer_sign_cast,unsigned_is_multiple_of,trait_upcasting";

/// The crates a run takes when none is named: the memory model, design
/// §9's.
pub const CRATES: [&str; 1] = ["php_heap"];

/// Run every crate of `only`, or [`CRATES`], under Miri; one reading each.
/// A crate whose sources do not exist yet writes none; a machine without
/// the toolchain writes a skip naming the command that installs it.
pub fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    let names: Vec<&str> = if only.is_empty() {
        CRATES.to_vec()
    } else {
        only.iter().map(String::as_str).collect()
    };
    let snapshot = Snapshot::disk(root)?;
    let installed = installed(root);
    let mut readings = Vec::new();
    for name in names {
        let Some(krate) = manifest.crate_named(name) else {
            bail!("the manifest has no crate named {name:?}");
        };
        if !sources::exists(root, krate) {
            continue;
        }
        let mut reading = Reading::for_crate(INSTRUMENT, krate, &snapshot)?;
        if let Err(why) = &installed {
            reading.status = Status::Skip;
            reading.message = format!(
                "{name}: not run, {why}; `rustup toolchain install {TOOLCHAIN} --profile minimal --component miri` installs it"
            );
            readings.push(reading);
            continue;
        }
        let ran = suite(root, krate)?;
        ran.read_into(&mut reading, name)?;
        readings.push(reading);
    }
    Ok(readings)
}

/// The command line's entry: the crates named, or [`CRATES`], and every
/// reading written.
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    crates: &[String],
) -> Result<()> {
    let readings = run(root, manifest, crates)?;
    crate::record(store, &readings)
}

/// Whether `cargo miri` answers on [`TOOLCHAIN`]; what it said where not.
fn installed(root: &Path) -> Result<(), String> {
    let mut asked = cargo(root);
    asked.args(["miri", "--version"]);
    match asked.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "`cargo +{TOOLCHAIN} miri` is not here: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(why) => Err(format!("cargo did not start: {why}")),
    }
}

/// `cargo +<TOOLCHAIN>` in `root`, with none of what the cargo that ran
/// this binary set for it: `CARGO` is what `cargo-miri` would call, and
/// `RUSTUP_TOOLCHAIN` the workspace's own toolchain.
fn cargo(root: &Path) -> Command {
    let mut command = Command::new("cargo");
    command.arg(format!("+{TOOLCHAIN}")).current_dir(root);
    for name in [
        "CARGO",
        "RUSTUP_TOOLCHAIN",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTDOC",
    ] {
        command.env_remove(name);
    }
    batch::without_git(&mut command);
    command
}

/// What Miri made of one crate's tests over every run it took.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Suite {
    pub passed: u64,
    pub ignored: u64,
    /// Each test that failed, by name, with what ended it.
    pub failed: Vec<(String, Ended)>,
    /// The runs it took: one, and one more per test that ended a run.
    pub runs: u64,
    /// What Miri said after the tests where no test was in flight, a leak
    /// at the end of the run; or what cargo said where nothing ran.
    pub after: Option<String>,
    /// The aliasing model and the provenance the runs stood under.
    pub under: String,
}

/// How a test ended that did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// libtest's own `FAILED`: an assertion, a panic.
    Failed,
    /// Miri's `Undefined Behavior`, with its first line.
    Undefined(String),
    /// Miri's `unsupported operation`, with its first line.
    Unsupported(String),
    /// Anything else that ended the run in the test, with its first line.
    Aborted(String),
}

impl Ended {
    /// What ended the run, from the first line Miri printed for it.
    fn of(said: &str) -> Ended {
        let said = said.trim().to_owned();
        if said.starts_with("error: Undefined Behavior") {
            Ended::Undefined(said)
        } else if said.starts_with("error: unsupported operation") {
            Ended::Unsupported(said)
        } else {
            Ended::Aborted(said)
        }
    }
}

/// Every test of `krate` under Miri, a run again after each one that
/// ended a run, until a run ends with no test in flight.
fn suite(root: &Path, krate: &Crate) -> Result<Suite> {
    let flags = std::env::var("MIRIFLAGS").unwrap_or_default();
    let mut suite = Suite {
        under: under(&flags),
        ..Suite::default()
    };
    let mut judged: Vec<String> = Vec::new();
    loop {
        let mut miri = cargo(root);
        miri.args(["miri", "test", "-p", &krate.package()])
            .args([
                "--lib",
                "--tests",
                "--no-fail-fast",
                "--ignore-rust-version",
            ])
            .args(["--color", "never"])
            .env("RUSTFLAGS", format!("-Zcrate-attr=feature({FEATURES})"))
            .env("MIRIFLAGS", &flags)
            .args(["--", "--test-threads=1", "--exact"]);
        for name in &judged {
            miri.args(["--skip", name]);
        }
        let out = miri
            .output()
            .with_context(|| format!("running cargo +{TOOLCHAIN} miri test"))?;
        suite.runs += 1;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let run = Run::of(&stdout, &stderr);
        if !stdout.contains("running ") && !out.status.success() {
            suite.after = Some(format!("the tests did not build\n{}", tail(&stderr, 40)));
            return Ok(suite);
        }
        suite.passed += run.passed.len() as u64;
        suite.ignored += run.ignored.len() as u64;
        judged.extend(run.passed);
        judged.extend(run.ignored);
        for name in run.failed {
            judged.push(name.clone());
            suite.failed.push((name, Ended::Failed));
        }
        if run.ended.is_empty() {
            if !out.status.success() && suite.failed.is_empty() {
                suite.after = Some(tail(&stderr, 20));
            }
            return Ok(suite);
        }
        for (name, said) in run.ended {
            judged.push(name.clone());
            suite.failed.push((name, Ended::of(&said)));
        }
    }
}

/// The model and provenance `flags` puts Miri under, as a reading says it.
fn under(flags: &str) -> String {
    let words: Vec<&str> = flags.split_whitespace().collect();
    let model = if words.contains(&"-Zmiri-tree-borrows") {
        "Tree Borrows"
    } else {
        "Stacked Borrows"
    };
    let provenance = if words.contains(&"-Zmiri-strict-provenance") {
        "strict provenance"
    } else {
        "permissive provenance"
    };
    if flags.trim().is_empty() {
        format!("{model}, {provenance}, Miri's defaults")
    } else {
        format!("{model}, {provenance}, under MIRIFLAGS `{}`", flags.trim())
    }
}

/// What one `cargo miri test` printed, read back: the tests libtest
/// finished, by verdict, and those it began and never finished, each with
/// the error Miri printed for it. libtest prints `test <name> ... ` and the
/// verdict after it on stdout; Miri prints the error that ends the binary
/// on stderr, one `error: ` line per ended binary in the order they ran.
#[derive(Debug, Default, PartialEq, Eq)]
struct Run {
    passed: Vec<String>,
    ignored: Vec<String>,
    failed: Vec<String>,
    ended: Vec<(String, String)>,
}

impl Run {
    fn of(stdout: &str, stderr: &str) -> Run {
        let mut run = Run::default();
        let mut began = Vec::new();
        for line in stdout.lines() {
            let Some(rest) = line.strip_prefix("test ") else {
                continue;
            };
            let Some((name, verdict)) = rest.split_once(" ... ") else {
                continue;
            };
            let name = name.to_owned();
            match verdict.trim() {
                "ok" => run.passed.push(name),
                "FAILED" => run.failed.push(name),
                v if v.starts_with("ignored") => run.ignored.push(name),
                _ => began.push(name),
            }
        }
        let mut errors = stderr.lines().filter(|line| {
            line.starts_with("error: ")
                && !line.starts_with("error: test failed")
                && !line.starts_with("error: aborting due to")
                && !line.contains("target failed")
                && !line.contains("targets failed")
        });
        run.ended = began
            .into_iter()
            .map(|name| {
                (
                    name,
                    errors
                        .next()
                        .unwrap_or("error: no word from Miri")
                        .to_owned(),
                )
            })
            .collect();
        run
    }
}

impl Suite {
    /// The crate's reading from what its runs came to.
    fn read_into(&self, reading: &mut Reading, name: &str) -> Result<()> {
        let failed = self.failed.len() as u64;
        let count =
            |kind: fn(&Ended) -> bool| self.failed.iter().filter(|(_, e)| kind(e)).count() as u64;
        reading.count("ran", self.passed + failed)?;
        reading.count("passed", self.passed)?;
        reading.count("failed", failed)?;
        reading.count("ignored", self.ignored)?;
        reading.count("undefined", count(|e| matches!(e, Ended::Undefined(_))))?;
        reading.count("unsupported", count(|e| matches!(e, Ended::Unsupported(_))))?;
        reading.count("runs", self.runs)?;
        let under = &self.under;
        if self.failed.is_empty() && self.after.is_none() {
            reading.status = Status::Pass;
            reading.message = format!(
                "{name}: {} tests pass under Miri ({under}), {} ignored",
                self.passed, self.ignored
            );
            return Ok(());
        }
        reading.status = Status::Fail;
        let mut said: Vec<String> = self
            .failed
            .iter()
            .map(|(test, ended)| match ended {
                Ended::Failed => format!("{test}: FAILED"),
                Ended::Undefined(line) | Ended::Unsupported(line) | Ended::Aborted(line) => {
                    format!("{test}: {}", line.trim_start_matches("error: "))
                }
            })
            .collect();
        if let Some(after) = &self.after {
            said.push(after.clone());
        }
        reading.message = format!(
            "{name}: {failed} of {} tests fail under Miri ({under}), {} of them undefined behaviour and {} an unsupported operation, {} ignored\n{}",
            self.passed + failed,
            count(|e| matches!(e, Ended::Undefined(_))),
            count(|e| matches!(e, Ended::Unsupported(_))),
            self.ignored,
            said.join("\n")
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary Miri ended in a test, and one that ran to its end: the
    /// test in flight is the one begun and never finished, and the error
    /// is the first Miri printed (#1916).
    #[test]
    fn a_run_reads_the_finished_tests_and_the_one_miri_ended() {
        let stdout = "\nrunning 3 tests\ntest a::one ... ok\ntest a::two ... ignored, needs mmap\ntest a::three ... \nrunning 1 test\ntest b::four ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed;\n";
        let stderr = "   Compiling php_heap\nerror: Undefined Behavior: attempting a read access\n  --> src/lib.rs:1:1\nerror: aborting due to 1 previous error\nerror: test failed, to rerun pass `-p php_heap --lib`\n";
        let run = Run::of(stdout, stderr);
        assert_eq!(run.passed, ["a::one"]);
        assert_eq!(run.ignored, ["a::two"]);
        assert_eq!(run.failed, ["b::four"]);
        assert_eq!(
            run.ended,
            [(
                "a::three".to_owned(),
                "error: Undefined Behavior: attempting a read access".to_owned()
            )]
        );
        assert!(matches!(Ended::of(&run.ended[0].1), Ended::Undefined(_)));
        assert!(matches!(
            Ended::of("error: unsupported operation: can't call foreign function"),
            Ended::Unsupported(_)
        ));
    }

    /// The reading says what the runs stood under, Miri's defaults or the
    /// flags, and never leaves the provenance unsaid.
    #[test]
    fn the_model_and_the_provenance_are_named() {
        assert_eq!(
            under(""),
            "Stacked Borrows, permissive provenance, Miri's defaults"
        );
        assert_eq!(
            under("-Zmiri-strict-provenance -Zmiri-tree-borrows"),
            "Tree Borrows, strict provenance, under MIRIFLAGS `-Zmiri-strict-provenance -Zmiri-tree-borrows`"
        );
    }
}
