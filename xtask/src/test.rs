//! `xtask test` (harness-design.md §4.5 condition 1, §6): each crate's
//! tests, as a reading per crate. What this instrument adds over a bare
//! `cargo test` is the reading `done` reads, stamped with the crate's tree,
//! so a crate changed since its tests last passed is stale and not green,
//! which a bare `cargo test` in a hook has no way to say (#191). One
//! reading per crate that exists; `--crate` narrows it to the ones named,
//! `--staged` to the crates with a staged change. The numbers are the sums
//! over every test binary and the doctests of the crate: `passed`,
//! `failed`, `ignored` and `filtered`; a crate whose tests do not build has
//! none and fails all the same.
//!
//! A run is three steps over every crate at once, as `cargo test` would
//! take them one crate at a time (#2165). One `cargo build --tests
//! --keep-going` builds every crate's test targets, cargo building crates
//! side by side, and a crate that does not compile stops only the crates
//! built on it ([`batch::Run`]). Each crate's test executables then run,
//! crate after crate, each with the working directory and the environment
//! `cargo test` gives it ([`environment`]); crates are not run side by
//! side, since their tests may share System V objects, sockets, a `MySQL`
//! server and scratch paths, and each binary runs its own tests in
//! parallel as it always has. Last, one `cargo test --doc` runs the
//! doctests of every crate that built, what it printed read back crate by crate
//! from cargo's `Doc-tests` lines.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::batch::{self, Built, Executable, Run, Targets, tail};
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status, Store, plural};
use crate::sources;

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "test";

/// Run the tests of every crate that exists, or of `only`; one reading
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
    let built = Run::of(root, &["build", "--keep-going"], &crates, Targets::Tests)?;
    let docs = doctests(root, &built, &crates)?;
    crates
        .iter()
        .map(|krate| reading(&snapshot, krate, &built, docs.get(&krate.name)))
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

/// What one test executable, or one crate's doctests, did: the counts its
/// `test result:` lines carry, whether it ended well, and what it printed.
#[derive(Debug, Default)]
struct Ran {
    counts: Counts,
    passed: bool,
    text: String,
}

/// One crate's reading: pass when every test executable of the crate and
/// its doctests ended well, fail with the tail of what the ones that did
/// not printed otherwise, the numbers summed over all of them either way.
fn reading(snapshot: &Snapshot, krate: &Crate, run: &Run, doc: Option<&Ran>) -> Result<Reading> {
    let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
    let name = &krate.name;
    let Some(built) = run.crates.get(name).filter(|b| b.whole) else {
        let said = run
            .crates
            .get(name)
            .map(Built::rendered)
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| run.stderr.clone());
        for key in ["passed", "failed", "ignored", "filtered"] {
            reading.count(key, 0)?;
        }
        reading.status = Status::Fail;
        reading.message = format!("{name}: the tests did not build\n{}", tail(&said, 40));
        return Ok(reading);
    };
    let mut counts = Counts::default();
    let mut passed = true;
    let mut failing = String::new();
    for executable in &built.executables {
        let ran = execute(executable, built, run)?;
        counts.add(&ran.counts);
        if !ran.passed {
            passed = false;
            failing.push_str(&ran.text);
        }
    }
    if let Some(doc) = doc {
        counts.add(&doc.counts);
        if !doc.passed {
            passed = false;
            failing.push_str(&doc.text);
        }
    }
    reading.count("passed", counts.passed)?;
    reading.count("failed", counts.failed)?;
    reading.count("ignored", counts.ignored)?;
    reading.count("filtered", counts.filtered)?;
    if passed {
        reading.status = Status::Pass;
        reading.message = format!(
            "{name}: {} test{} passed",
            counts.passed,
            plural(usize::try_from(counts.passed).unwrap_or(usize::MAX))
        );
    } else {
        reading.status = Status::Fail;
        reading.message = format!(
            "{name}: {} test{} failed\n{}",
            counts.failed,
            plural(usize::try_from(counts.failed).unwrap_or(usize::MAX)),
            tail(&failing, 40)
        );
    }
    Ok(reading)
}

/// One test executable run as `cargo test` runs it: no argument, in the
/// package's directory, with cargo's environment for it.
fn execute(executable: &Executable, built: &Built, run: &Run) -> Result<Ran> {
    let mut command = Command::new(&executable.path);
    command.current_dir(built.package.dir());
    for (key, value) in environment(executable, built, run) {
        command.env(key, value);
    }
    batch::without_git(&mut command);
    let out = command
        .output()
        .with_context(|| format!("running {}", executable.path.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    Ok(Ran {
        counts: Counts::of(&stdout),
        passed: out.status.success(),
        text: format!("{stdout}{stderr}"),
    })
}

/// The environment `cargo test` runs a test executable of `built` with
/// (the Cargo book, "Environment variables Cargo sets for crates", which
/// holds for `cargo test` as for `cargo run`): the package's manifest
/// values, `CARGO`, `OUT_DIR` where a `build.rs` wrote one,
/// `CARGO_TARGET_TMPDIR` for an integration test, and the dynamic library
/// path with every directory a `build.rs` named for the linker, the
/// profile's own directories and the toolchain's ahead of what the path
/// held.
fn environment(executable: &Executable, built: &Built, run: &Run) -> Vec<(String, OsString)> {
    let package = &built.package;
    let mut version = package.version.splitn(3, '.');
    let (major, minor) = (version.next().unwrap_or("0"), version.next().unwrap_or("0"));
    let rest = version.next().unwrap_or("0");
    let (patch, pre) = rest.split_once('-').unwrap_or((rest, ""));
    let text = |s: &str| OsString::from(s);
    let optional = |s: &Option<String>| OsString::from(s.as_deref().unwrap_or_default());
    let mut env = vec![
        (
            "CARGO".to_owned(),
            std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        ),
        ("CARGO_MANIFEST_DIR".to_owned(), package.dir().into()),
        (
            "CARGO_MANIFEST_PATH".to_owned(),
            package.manifest_path.clone().into(),
        ),
        ("CARGO_PKG_NAME".to_owned(), text(&package.name)),
        ("CARGO_PKG_VERSION".to_owned(), text(&package.version)),
        ("CARGO_PKG_VERSION_MAJOR".to_owned(), text(major)),
        ("CARGO_PKG_VERSION_MINOR".to_owned(), text(minor)),
        ("CARGO_PKG_VERSION_PATCH".to_owned(), text(patch)),
        ("CARGO_PKG_VERSION_PRE".to_owned(), text(pre)),
        (
            "CARGO_PKG_AUTHORS".to_owned(),
            text(&package.authors.join(":")),
        ),
        (
            "CARGO_PKG_DESCRIPTION".to_owned(),
            optional(&package.description),
        ),
        ("CARGO_PKG_HOMEPAGE".to_owned(), optional(&package.homepage)),
        (
            "CARGO_PKG_REPOSITORY".to_owned(),
            optional(&package.repository),
        ),
        ("CARGO_PKG_LICENSE".to_owned(), optional(&package.license)),
        (
            "CARGO_PKG_LICENSE_FILE".to_owned(),
            optional(&package.license_file),
        ),
        (
            "CARGO_PKG_RUST_VERSION".to_owned(),
            optional(&package.rust_version),
        ),
        ("CARGO_PKG_README".to_owned(), optional(&package.readme)),
        ("CARGO_PRIMARY_PACKAGE".to_owned(), text("1")),
    ];
    if let Some(out_dir) = &built.out_dir {
        env.push(("OUT_DIR".to_owned(), out_dir.clone().into()));
    }
    if executable.kind == "test" {
        env.push((
            "CARGO_TARGET_TMPDIR".to_owned(),
            run.target_dir.join("tmp").into(),
        ));
    }
    let variable = if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    let mut paths: Vec<PathBuf> = run.linked.clone();
    if let Some(deps) = executable.path.parent() {
        paths.push(deps.to_path_buf());
        if let Some(profile) = deps.parent() {
            paths.push(profile.to_path_buf());
        }
    }
    paths.extend(run.toolchain.iter().cloned());
    match std::env::var_os(variable) {
        Some(held) => paths.extend(std::env::split_paths(&held)),
        // macOS's own fallback when the variable is unset, which cargo
        // keeps behind what it adds.
        None if cfg!(target_os = "macos") => {
            if let Some(home) = std::env::var_os("HOME") {
                paths.push(PathBuf::from(home).join("lib"));
            }
            paths.extend(["/usr/local/lib", "/usr/lib"].map(PathBuf::from));
        }
        None => {}
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        env.push((variable.to_owned(), joined));
    }
    env
}

/// Every built crate's doctests in one `cargo test --doc`, read back by
/// crate. Cargo names each crate's run on stderr, `Doc-tests <lib>`, and
/// the run's lines follow on stdout, so both are written to one file,
/// where they stand in the order they were written. A crate whose library
/// carries no doctests, or that did not build, has none.
fn doctests(root: &Path, run: &Run, crates: &[&Crate]) -> Result<BTreeMap<String, Ran>> {
    let with: Vec<&Crate> = crates
        .iter()
        .copied()
        .filter(|k| {
            run.crates
                .get(&k.name)
                .is_some_and(|b| b.whole && b.doctests)
        })
        .collect();
    if with.is_empty() {
        return Ok(BTreeMap::new());
    }
    let by_lib: BTreeMap<String, String> = with
        .iter()
        .filter_map(|k| {
            let lib = run.crates.get(&k.name)?.lib.clone()?;
            Some((lib, k.name.clone()))
        })
        .collect();
    fs::create_dir_all(&run.target_dir)
        .with_context(|| format!("creating {}", run.target_dir.display()))?;
    let written = run
        .target_dir
        .join(format!("xtask-doctests-{}.out", std::process::id()));
    let file = File::create(&written).with_context(|| format!("creating {}", written.display()))?;
    let mut command = Command::new("cargo");
    command
        .args(["test", "--doc", "--no-fail-fast", "--color", "never"])
        .current_dir(root)
        .stdout(Stdio::from(
            file.try_clone().context("sharing the doctests' file")?,
        ))
        .stderr(Stdio::from(file));
    for krate in &with {
        command.args(["-p", &krate.package()]);
    }
    batch::without_git(&mut command);
    let status = command.status().context("running cargo test --doc")?;
    let text = fs::read_to_string(&written).unwrap_or_default();
    fs::remove_file(&written).ok();
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(lib) = line.trim().strip_prefix("Doc-tests ") {
            current = by_lib.get(lib.trim()).cloned();
        }
        if let Some(name) = &current {
            let section = sections.entry(name.clone()).or_default();
            section.push_str(line);
            section.push('\n');
        }
    }
    let mut ran = BTreeMap::new();
    for krate in &with {
        let section = sections.remove(&krate.name).unwrap_or_default();
        let counts = Counts::of(&section);
        // A crate's run that reported no result, in a run that failed,
        // is one cargo never reached: what cargo said is its text.
        let reached = counts.binaries > 0 || status.success();
        let passed = reached
            && !section
                .lines()
                .any(|l| l.starts_with("test result: FAILED"));
        let text = if reached { section } else { tail(&text, 40) };
        ran.insert(
            krate.name.clone(),
            Ran {
                counts,
                passed,
                text,
            },
        );
    }
    Ok(ran)
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
    /// `other` added to these counts.
    fn add(&mut self, other: &Counts) {
        self.binaries += other.binaries;
        self.passed += other.passed;
        self.failed += other.failed;
        self.ignored += other.ignored;
        self.filtered += other.filtered;
    }

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

#[cfg(test)]
mod tests {
    use super::Counts;

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
}
