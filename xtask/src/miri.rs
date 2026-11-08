//! `xtask miri` (third-engine-design.md §9's "Miri on `php_heap`'s test
//! suite per commit"; harness-design.md §7's Miri row, #1916): a crate's
//! library and integration tests run under Miri, as one reading per crate
//! with what ran, failed and was ignored in its numbers. The crate is
//! `php_heap` unless `--crate` names others.
//!
//! **What a run is.** `cargo miri nextest run` on the pinned nightly
//! ([`TOOLCHAIN`]) and cargo-nextest ([`NEXTEST`]): each test in a Miri
//! process of its own, as many at once as the instrument's seat is a share
//! of the machine's cores ([`crate::seat::jobs`], the reading's `jobs`), so
//! the lane is not one core for the whole suite (#4117). Miri ends a
//! process at the first undefined behaviour or unsupported operation it
//! meets, which now ends only the test that met it; the test is judged by
//! the error it printed on its own stderr. The doctests are not run.
//!
//! **What it runs under, said and never chosen silently.** Tree Borrows
//! ([`MODEL`]) with permissive provenance, as #2415 ruled: `php_heap`'s
//! pointers carried through integers (`expose_provenance`,
//! `with_exposed_provenance_mut`) are allowed with a warning. `MIRIFLAGS`
//! adds to that, and `-Zmiri-strict-provenance` among them makes each such
//! pointer an unsupported operation. The reading's message names the model
//! and the provenance it ran under. A test Miri cannot run at all carries
//! `#[cfg_attr(miri, ignore = "<why>")]` in its source, the reason with it.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::batch::{self, tail};
use crate::carried::Carried;
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status};
use crate::seat;
use crate::sources;

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "miri";

/// The nightly Miri runs on, by rustup's name: one of the workspace's own
/// Rust (`rust-toolchain.toml`), so it builds every crate as the stable
/// compiler does, with no feature turned on and no `rust-version` waived
/// (#2417). CI's job installs the same one with its `miri` component.
pub const TOOLCHAIN: &str = "nightly-2026-02-20";

/// The cargo-nextest the lane runs each test under Miri with, one Miri
/// process a test (#4117). CI's job installs this release; on a machine
/// without one the reading is a skip naming the command that does.
pub const NEXTEST: &str = "0.9.127";

/// The aliasing model every run stands under, ruled on #2415; on
/// [`TOOLCHAIN`] it takes exposed provenance.
const MODEL: &str = "-Zmiri-tree-borrows";

/// Run every crate of `only`, or where none is named each crate the
/// manifest's `[[owed]]` rows owe a `miri` reading, as `done` asks it
/// ([`crate::done::owed`]; design §9's memory model, #3206), under Miri;
/// one reading each. Rows that owe it of no crate are refused, naming
/// `--crate`. A crate whose sources do not exist yet writes none; a
/// machine without the toolchain writes a skip naming the command that
/// installs it.
pub fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    let crates = if only.is_empty() {
        let carried = Carried::binary(root)?;
        let asked = |k: &&Crate| crate::done::owed(manifest, k, &carried).contains(&INSTRUMENT);
        let crates: Vec<&Crate> = manifest.crates.iter().filter(asked).collect();
        if crates.is_empty() {
            bail!("no [[owed]] row owes {INSTRUMENT} of a crate; name the crates with --crate");
        }
        crates
    } else {
        manifest.crates_named(only)?
    };
    let snapshot = Snapshot::disk(root)?;
    let installed = installed(root);
    let mut readings = Vec::new();
    for krate in crates {
        let name = &krate.name;
        if !sources::exists(root, krate) {
            continue;
        }
        let mut reading = Reading::for_crate(INSTRUMENT, krate, &snapshot)?;
        if let Err(why) = &installed {
            reading.status = Status::Skip;
            reading.message = format!("{name}: not run, {why}");
            readings.push(reading);
            continue;
        }
        let ran = suite(root, krate)?;
        ran.read_into(&mut reading, name)?;
        readings.push(reading);
    }
    Ok(readings)
}

/// Whether `cargo miri` answers on [`TOOLCHAIN`], and `cargo nextest`
/// beside it; what was missing and the command that installs it where not.
fn installed(root: &Path) -> Result<(), String> {
    answers(root, "miri").map_err(|said| {
        format!(
            "`cargo +{TOOLCHAIN} miri` is not here: {said}; `rustup toolchain install {TOOLCHAIN} --profile minimal --component miri,rust-src` installs it"
        )
    })?;
    answers(root, "nextest").map_err(|said| nextest_missing(&said))
}

/// The refusal where `cargo nextest` does not answer, after what it said.
fn nextest_missing(said: &str) -> String {
    format!(
        "`cargo nextest`, which runs each test in a Miri of its own, is not here: {said}; `cargo install --locked cargo-nextest@{NEXTEST}` installs it"
    )
}

/// Whether `cargo +<TOOLCHAIN> <subcommand> --version` succeeds; what it
/// said where not.
fn answers(root: &Path, subcommand: &str) -> Result<(), String> {
    let mut asked = cargo(root);
    asked.args([subcommand, "--version"]);
    match asked.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
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

/// What Miri made of one crate's tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Suite {
    pub passed: u64,
    pub ignored: u64,
    /// Each test that failed, as nextest names it, with what ended it.
    pub failed: Vec<(String, Ended)>,
    /// How many Miri processes ran at once.
    pub jobs: u64,
    /// What cargo said where the tests did not build, or where the run
    /// failed with no test failed or did not account for every test.
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

/// Every test of `krate` under Miri, one Miri process a test and
/// [`seat::jobs`] of them at once.
fn suite(root: &Path, krate: &Crate) -> Result<Suite> {
    let extra = std::env::var("MIRIFLAGS").unwrap_or_default();
    let flags = format!("{MODEL} {extra}");
    let jobs = seat::jobs().get();
    let mut miri = cargo(root);
    miri.args(["miri", "nextest", "run", "-p", &krate.package()])
        .args(["--lib", "--tests", "--no-fail-fast", "--color", "never"])
        .args(["--retries", "0", "--status-level", "skip"])
        .args(["--final-status-level", "none", "--success-output", "never"])
        .args(["--failure-output", "immediate", "-j", &jobs.to_string()])
        .env("MIRIFLAGS", flags.trim());
    let out = miri
        .output()
        .with_context(|| format!("running cargo +{TOOLCHAIN} miri nextest run"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut suite = Suite::of(&Run::of(&stderr), out.status.success(), &stderr);
    suite.under = under(&extra);
    suite.jobs = jobs as u64;
    Ok(suite)
}

/// The model and provenance a run stands under, [`MODEL`] and the `extra`
/// flags `MIRIFLAGS` adds, as a reading says it.
fn under(extra: &str) -> String {
    let provenance = if extra
        .split_whitespace()
        .any(|word| word == "-Zmiri-strict-provenance")
    {
        "strict"
    } else {
        "permissive"
    };
    match extra.trim() {
        "" => format!("Tree Borrows, {provenance} provenance"),
        more => format!("Tree Borrows, {provenance} provenance, with MIRIFLAGS `{more}`"),
    }
}

/// What one `cargo miri nextest run` printed on stderr, read back. nextest
/// prints one line a test as it finishes, `PASS [ 1.2s] (3/5) <binary>
/// <test>`, `SKIP` for one ignored and `FAIL` or the signal for one that
/// failed, the test's own stdout and stderr under a failed one, then a
/// `Summary` line counting the tests run.
#[derive(Debug, Default, PartialEq, Eq)]
struct Run {
    /// Whether nextest began the tests: their `Starting` line.
    started: bool,
    passed: Vec<String>,
    ignored: Vec<String>,
    failed: Vec<(String, Ended)>,
    /// The tests the `Summary` line says ran.
    ran: Option<u64>,
}

/// What a test came to, by the word nextest's line for it begins with.
enum Verdict {
    Passed,
    Ignored,
    Failed,
}

impl Run {
    fn of(stderr: &str) -> Run {
        let mut run = Run::default();
        // The failed test whose own stdout and stderr are being read, and those lines.
        let mut open: Option<(String, Vec<&str>)> = None;
        for line in stderr.lines() {
            let word = line.trim_start();
            let verdict = verdict(word);
            let summary = word.strip_prefix("Summary [");
            if verdict.is_some() || summary.is_some() {
                run.close(open.take());
            }
            if let Some((verdict, test)) = verdict {
                match verdict {
                    Verdict::Passed => run.passed.push(test),
                    Verdict::Ignored => run.ignored.push(test),
                    Verdict::Failed => open = Some((test, Vec::new())),
                }
            } else if let Some(rest) = summary {
                run.ran = rest
                    .split_once("] ")
                    .and_then(|(_, said)| said.split_whitespace().next())
                    .and_then(|n| n.parse().ok());
            } else if let Some((_, said)) = &mut open {
                said.push(word);
            } else if word.starts_with("Starting ") {
                run.started = true;
            }
        }
        run.close(open);
        run
    }

    /// The failed test judged by what it printed: the error Miri ended it
    /// with, libtest's `FAILED` where Miri said none, and the first other
    /// error where neither.
    fn close(&mut self, open: Option<(String, Vec<&str>)>) {
        let Some((test, said)) = open else {
            return;
        };
        let mut errors = said.iter().filter(|line| {
            line.starts_with("error: ") && !line.starts_with("error: aborting due to")
        });
        let miri = errors
            .clone()
            .find(|line| !matches!(Ended::of(line), Ended::Aborted(_)));
        let ended = match miri {
            Some(line) => Ended::of(line),
            None if said.iter().any(|line| line.ends_with(" ... FAILED")) => Ended::Failed,
            None => Ended::of(errors.next().unwrap_or(&"error: no word from Miri")),
        };
        self.failed.push((test, ended));
    }
}

/// The verdict and the test of one of nextest's status lines, `<WORD> [
/// <time>] (<n>/<of>) <binary> <test>`, the count left out by some
/// releases; none for any other line, and for the status of a test still
/// running (`SLOW`).
fn verdict(word: &str) -> Option<(Verdict, String)> {
    let (status, rest) = word.split_once(" [")?;
    if status.is_empty() || !status.bytes().all(|b| b.is_ascii_uppercase() || b == b'-') {
        return None;
    }
    let (_, rest) = rest.split_once("] ")?;
    let rest = match rest.strip_prefix('(') {
        Some(counted) => counted.split_once(") ")?.1,
        None => rest,
    };
    let (binary, test) = rest.trim().split_once(' ')?;
    let verdict = match status {
        "PASS" | "LEAK" => Verdict::Passed,
        "SKIP" => Verdict::Ignored,
        "FAIL" | "LEAK-FAIL" | "TIMEOUT" | "ABORT" => Verdict::Failed,
        signal if signal.starts_with("SIG") => Verdict::Failed,
        _ => return None,
    };
    Some((verdict, format!("{binary} {test}")))
}

impl Suite {
    /// The suite one run came to, `succeeded` its exit: a run whose tests
    /// did not build, one that failed with no test failed, and one whose
    /// tests the `Summary` line counts otherwise than its lines did, each
    /// say so with the end of what cargo printed.
    fn of(run: &Run, succeeded: bool, stderr: &str) -> Suite {
        let mut suite = Suite {
            passed: run.passed.len() as u64,
            ignored: run.ignored.len() as u64,
            failed: run.failed.clone(),
            ..Suite::default()
        };
        let read = suite.passed + suite.failed.len() as u64;
        suite.after = if !run.started && !succeeded {
            Some(format!("the tests did not build\n{}", tail(stderr, 40)))
        } else if run.started && run.ran != Some(read) {
            let ran = run
                .ran
                .map_or("an uncounted number of".to_owned(), |n| n.to_string());
            Some(format!(
                "nextest ran {ran} tests and {read} of them were read\n{}",
                tail(stderr, 20)
            ))
        } else if !succeeded && suite.failed.is_empty() {
            Some(tail(stderr, 20))
        } else {
            None
        };
        suite
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
        reading.count("jobs", self.jobs)?;
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

    /// What nextest printed under Miri for a crate of six tests, from a run
    /// of the use-after-free control with three tests and a second binary
    /// added: one ignored, a read after a free, a foreign call, a panic,
    /// and two that pass, one in an integration test.
    const NEXTEST_RUN: &str = "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.28s
────────────
 Nextest run ID b6ba4551 with nextest profile: default-miri
    Starting 5 tests across 2 binaries (1 test skipped)
        SKIP [         ] (───) php_heap more::ignored_one
        FAIL [   1.419s] (1/5) php_heap tests::a_read_after_the_free
  stdout ───

    running 1 test
    test tests::a_read_after_the_free ... 
  stderr ───
    error: Undefined Behavior: memory access failed: alloc42301 has been freed, so this pointer is dangling
      --> crates/php_heap/src/lib.rs:15:29
       = note: this is on thread `tests::a_read_after_the_free`

    error: aborting due to 1 previous error; 1 warning emitted

    (test failed with exit code 1)

        FAIL [   1.445s] (2/5) php_heap more::unsupported_one
  stdout ───

    running 1 test
    test more::unsupported_one ... 
  stderr ───
    error: unsupported operation: can't call foreign function `mkfifo` on OS `macos`
      --> crates/php_heap/src/lib.rs:39:26

    error: aborting due to 1 previous error; 1 warning emitted

    (test failed with exit code 1)

        PASS [   1.545s] (3/5) php_heap tests::a_test_beside_it_still_runs
        FAIL [   1.615s] (4/5) php_heap more::fails_one
  stdout ───

    running 1 test
    test more::fails_one ... FAILED

    test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.20s

  stderr ───
    thread 'more::fails_one' (1) panicked at crates/php_heap/src/lib.rs:33:9:
    assertion `left == right` failed: one is not two

        PASS [   1.673s] (5/5) php_heap::integ integ_ok
────────────
     Summary [   3.099s] 5 tests run: 2 passed, 3 failed, 1 skipped
error: test run failed
";

    /// Each test is its own Miri process, so a read after a free ends
    /// only the test it is in: every line nextest printed is read, the
    /// failed tests by what each printed of what ended it (#1916, #4117).
    #[test]
    fn a_run_reads_the_finished_tests_and_the_one_miri_ended() {
        let run = Run::of(NEXTEST_RUN);
        assert!(run.started);
        assert_eq!(
            run.passed,
            [
                "php_heap tests::a_test_beside_it_still_runs",
                "php_heap::integ integ_ok"
            ]
        );
        assert_eq!(run.ignored, ["php_heap more::ignored_one"]);
        assert_eq!(run.ran, Some(5));
        assert_eq!(
            run.failed,
            [
                (
                    "php_heap tests::a_read_after_the_free".to_owned(),
                    Ended::Undefined(
                        "error: Undefined Behavior: memory access failed: alloc42301 has been freed, so this pointer is dangling".to_owned()
                    )
                ),
                (
                    "php_heap more::unsupported_one".to_owned(),
                    Ended::Unsupported(
                        "error: unsupported operation: can't call foreign function `mkfifo` on OS `macos`".to_owned()
                    )
                ),
                ("php_heap more::fails_one".to_owned(), Ended::Failed),
            ]
        );
        let suite = Suite::of(&run, false, NEXTEST_RUN);
        assert_eq!(suite.after, None, "{suite:#?}");
        let mut reading = Reading::new(INSTRUMENT, "php_heap");
        Suite {
            under: under(""),
            jobs: 4,
            ..suite
        }
        .read_into(&mut reading, "php_heap")
        .unwrap();
        assert_eq!(reading.status, Status::Fail);
        for (key, n) in [
            ("ran", 5),
            ("passed", 2),
            ("failed", 3),
            ("ignored", 1),
            ("undefined", 1),
            ("unsupported", 1),
            ("jobs", 4),
        ] {
            assert_eq!(reading.int(key), Some(n), "{key} in {reading:#?}");
        }
        assert!(
            reading.message.starts_with(
                "php_heap: 3 of 5 tests fail under Miri (Tree Borrows, permissive provenance), 1 of them undefined behaviour and 1 an unsupported operation, 1 ignored\n"
            ),
            "{}",
            reading.message
        );
        assert!(
            reading.message.contains(
                "php_heap tests::a_read_after_the_free: Undefined Behavior: memory access failed"
            ),
            "{}",
            reading.message
        );
    }

    /// A run whose tests did not build, and one whose `Summary` counts a
    /// test no line was read for, fail with what cargo said, never as a
    /// pass of the tests that were read.
    #[test]
    fn a_run_that_did_not_build_or_left_a_test_unread_says_so() {
        let unbuilt = "   Compiling php_heap\nerror[E0425]: cannot find value `x`\nerror: could not compile `php_heap`\n";
        let suite = Suite::of(&Run::of(unbuilt), false, unbuilt);
        let after = suite.after.unwrap_or_default();
        assert!(after.starts_with("the tests did not build\n"), "{after}");
        assert!(after.contains("E0425"), "{after}");

        let unread = "    Starting 2 tests across 1 binary\n        PASS [   1.0s] (1/2) php_heap a\n  CRASHED [   1.0s] (2/2) php_heap b\n     Summary [   2.0s] 2 tests run: 1 passed, 1 failed, 0 skipped\n";
        let suite = Suite::of(&Run::of(unread), false, unread);
        assert_eq!(suite.passed, 1);
        let after = suite.after.unwrap_or_default();
        assert!(
            after.starts_with("nextest ran 2 tests and 1 of them were read\n"),
            "{after}"
        );

        let passed = "    Starting 1 test across 1 binary\n        PASS [   1.0s] (1/1) php_heap a\n     Summary [   1.0s] 1 test run: 1 passed, 0 skipped\n";
        assert_eq!(Suite::of(&Run::of(passed), true, passed).after, None);
    }

    /// The refusal where nextest is missing names the release CI installs
    /// and the command that installs it.
    #[test]
    fn a_missing_nextest_is_refused_with_the_command_that_installs_it() {
        let said = nextest_missing("error: no such command: `nextest`");
        assert!(said.contains("no such command"), "{said}");
        assert!(
            said.contains(&format!(
                "`cargo install --locked cargo-nextest@{NEXTEST}` installs it"
            )),
            "{said}"
        );
    }

    /// The reading says what the runs stood under, the lane's model and
    /// any flags added to it, and never leaves the provenance unsaid.
    #[test]
    fn the_model_and_the_provenance_are_named() {
        assert_eq!(under(""), "Tree Borrows, permissive provenance");
        assert_eq!(
            under(" -Zmiri-strict-provenance "),
            "Tree Borrows, strict provenance, with MIRIFLAGS `-Zmiri-strict-provenance`"
        );
    }
}
