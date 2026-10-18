//! `xtask test` (harness-design.md §4.5 condition 1, §6): each crate's
//! tests, as a reading per crate. What this instrument adds over a bare
//! `cargo test` is the reading `done` reads, stamped with the crate's tree,
//! so a crate changed since its tests last passed is stale and not green,
//! which a bare `cargo test` in a hook has no way to say (#191). One
//! reading per crate that exists; `--crate` narrows it to the ones named,
//! `--staged` to the crates with a staged change. The numbers are the sums
//! over every test binary and the doctests of the crate: `passed`,
//! `failed`, `ignored` and `filtered`, with `ms` beside them; a crate whose
//! tests do not build has none and fails all the same.
//!
//! A run is three steps over every crate at once, as `cargo test` would
//! take them one crate at a time (#2165), and a crate with a binary takes
//! them alone, so its binary is built with its own features
//! ([`batch::runs`], #2606). One `cargo build --tests
//! --keep-going` builds every crate's test targets, cargo building crates
//! side by side, and a crate that does not compile stops only the crates
//! built on it ([`batch::Run`]). Next, one `cargo test --doc` runs the
//! doctests of every crate that built, what it printed read back crate by
//! crate from cargo's `Doc-tests` lines. Last, each crate's test
//! executables run, crate after crate, each with the working directory and
//! the environment `cargo test` gives it ([`environment`]), and each
//! running its own tests in parallel as it always has. Crates are not run
//! side by side, since their tests may share System V objects, sockets, a
//! `MySQL` server and scratch paths, and for the same reason a crate's own
//! executables run one at a time unless its `Cargo.toml` declares that
//! they share nothing, `executables = "side-by-side"` under
//! `[package.metadata.xtask.test]`: then as many run at once as
//! [`Beside::schedule`] says, an executable that does share something held
//! apart from the others that name its key ([`Beside`]), what each one
//! printed kept whole and read in the executables' order (#2869). The
//! reading is stamped once they have ended, `ms` counts how long they
//! ran, and a group per executable, `test worktree`, how long that one
//! did.
//!
//! **A pinned error code.** A `compile_fail,E0382` block passes the stable
//! rustdoc when it fails to compile for any reason; only a nightly's
//! checks the code (#2049). So the doctests of the files that pin one are
//! run once more, one at a time with the compiler's words let through
//! (`--nocapture`), and a block whose
//! compile names no `error[E0382]` fails its crate's reading as a failed
//! test, as the nightly's rustdoc would fail it ([`Pin`]).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::batch::{self, Built, Executable, Package, Run, Targets, tail};
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Number, Reading, Status, Store, millis, plural};
use crate::sources;
use crate::tally::{self, Apart};

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "test";

pub use crate::batch::without_git;

/// `path` as the index the commit's tree is written from holds it, `git
/// show :<path>` in `root`: under pre-commit the one git handed the hook,
/// which [`without_git`] keeps for a test as `XTASK_COMMIT_INDEX`, where
/// it lies in `root`'s git directory, and the checkout's own elsewhere, a
/// scratch repository's among them (#1416, #2747).
pub fn committed(root: &Path, path: &str) -> Result<String> {
    let mut git = Command::new("git");
    if let Some(index) = std::env::var_os(batch::COMMIT_INDEX) {
        let dir = crate::git::git(root, &["rev-parse", "--absolute-git-dir"], None)
            .ok()
            .and_then(|dir| fs::canonicalize(dir.trim()).ok());
        let lies = Path::new(&index)
            .parent()
            .and_then(|p| fs::canonicalize(p).ok());
        if dir.is_some() && lies == dir {
            git.env("GIT_INDEX_FILE", index);
        }
    }
    let out = crate::git::run(git, root, &["show", &format!(":{path}")], None)?;
    String::from_utf8(out).with_context(|| format!("{path} is not UTF-8"))
}

/// Run the tests of every crate that exists, or of `only`; one reading
/// each.
pub fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    let mut crates = manifest.crates_named(only)?;
    crates.retain(|k| sources::exists(root, k));
    // The key each reading carries is the tree's before cargo built it,
    // so a file written while the crates are measured stales the reading.
    let snapshot = Snapshot::disk(root)?;
    if crates.is_empty() {
        return Ok(Vec::new());
    }
    let mut readings = Vec::with_capacity(crates.len());
    for run in batch::runs(root, &crates)? {
        let built = Run::of(root, &["build", "--keep-going"], &run, Targets::Tests)?;
        let docs = doctests(root, &built, &run)?;
        let missed = Pin::missed(root, &built, &run)?;
        for krate in run {
            let missed = missed.get(&krate.name).map_or(&[][..], Vec::as_slice);
            readings.push(reading(
                &snapshot,
                krate,
                &built,
                (docs.get(&krate.name), missed),
            )?);
        }
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

/// What one test executable, or one crate's doctests, did: the counts its
/// `test result:` lines carry, whether it ended well, and what it printed.
#[derive(Debug, Default)]
struct Ran {
    counts: Counts,
    passed: bool,
    text: String,
}

impl Ran {
    /// `other` summed into this: its counts added, and what it printed
    /// kept where it did not end well.
    fn add(&mut self, other: &Ran) {
        self.counts.add(&other.counts);
        if !other.passed {
            self.passed = false;
            self.text.push_str(&other.text);
        }
    }
}

/// One crate's reading: pass when every test executable of the crate and
/// its doctests ended well and no pinned code was `missed`, fail with the
/// tail of what the ones that did not printed otherwise, each missed pin
/// a failed test, the numbers summed over all of them either way.
fn reading(
    snapshot: &Snapshot,
    krate: &Crate,
    run: &Run,
    (doc, missed): (Option<&Ran>, &[String]),
) -> Result<Reading> {
    let name = &krate.name;
    let Some(built) = run.crates.get(name).filter(|b| b.whole) else {
        let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
        let said = run
            .crates
            .get(name)
            .map(Built::rendered)
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| run.stderr.clone());
        Counts::default().count(&mut reading)?;
        reading.status = Status::Fail;
        reading.message = format!("{name}: the tests did not build\n{}", tail(&said, 40));
        return Ok(reading);
    };
    let started = Instant::now();
    let (jobs, apart) = Beside::of(&built.package)?.schedule(name, &built.executables)?;
    let mut all = Ran {
        passed: true,
        ..Ran::default()
    };
    let mut took = BTreeMap::new();
    let each = |executable: &Executable| {
        let started = Instant::now();
        let ran = execute(executable, built, run)?;
        let named = format!("{} {}", executable.kind, executable.name);
        Ok((named, millis(started.elapsed()), ran))
    };
    let on = |(named, ms, ran): (String, u64, Ran)| {
        took.insert(named, Number::count(ms));
        all.add(&ran);
    };
    let batch = NonZeroUsize::MIN;
    tally::pool(&built.executables, jobs, batch, &apart, each, on)?;
    // Stamped once the executables have ended, so a span read off the
    // store's stamps holds their run (#2869).
    let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
    reading.count("ms", millis(started.elapsed()))?;
    reading.groups = took;
    if let Some(doc) = doc {
        all.add(doc);
    }
    for pin in missed {
        all.counts.failed += 1;
        all.passed = false;
        all.text.push_str(pin);
        all.text.push('\n');
    }
    let Ran { counts, passed, .. } = &all;
    counts.count(&mut reading)?;
    let (n, verb) = if *passed {
        reading.status = Status::Pass;
        (counts.passed, "passed")
    } else {
        reading.status = Status::Fail;
        (counts.failed, "failed")
    };
    let s = plural(usize::try_from(n).unwrap_or(usize::MAX));
    reading.message = format!("{name}: {n} test{s} {verb}");
    if !passed {
        reading.message = format!("{}\n{}", reading.message, tail(&all.text, 40));
    }
    Ok(reading)
}

/// How a crate's test executables stand beside each other, as its
/// `Cargo.toml` declares under `[package.metadata.xtask.test]`: one at a
/// time unless it says otherwise, since a crate's tests may share System
/// V objects, sockets, a `MySQL` server and scratch paths across its
/// executables as well as across crates.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(tag = "executables", rename_all = "kebab-case", deny_unknown_fields)]
enum Beside {
    #[default]
    OneAtATime,
    /// Side by side, as many at once as [`Beside::schedule`] says, each apart
    /// only from those it shares a key with: `apart`, by target name, is
    /// what an executable holds that another may hold too.
    SideBySide {
        #[serde(default)]
        apart: BTreeMap<String, Vec<String>>,
    },
}

impl Beside {
    /// What `package` declares; a declaration this does not read is an
    /// error, never one at a time in silence.
    fn of(package: &Package) -> Result<Beside> {
        let Some(table) = (package.metadata.as_ref()).and_then(|m| m.get("xtask")?.get("test"))
        else {
            return Ok(Beside::OneAtATime);
        };
        let at = package.manifest_path.display();
        serde_json::from_value(table.clone())
            .with_context(|| format!("{at}: [package.metadata.xtask.test]"))
    }

    /// How many of `all`, `krate`'s executables, run at once, and what each may
    /// not run beside, in their order. Side by side, as many as the share of
    /// the machine cargo was given, `CARGO_BUILD_JOBS`, where the session
    /// sets one, and otherwise the cores, since each executable runs its
    /// own tests on threads of its own. A key held for a name that is no
    /// executable of the crate is an error.
    fn schedule(&self, krate: &str, all: &[Executable]) -> Result<(NonZeroUsize, Vec<Apart>)> {
        let Beside::SideBySide { apart } = self else {
            return Ok((NonZeroUsize::MIN, vec![Apart::default(); all.len()]));
        };
        if let Some(name) = apart.keys().find(|n| all.iter().all(|e| &e.name != *n)) {
            bail!(
                "{krate}: [package.metadata.xtask.test] holds `{name}` apart, which is no test executable of it"
            );
        }
        let jobs = std::env::var("CARGO_BUILD_JOBS")
            .ok()
            .and_then(|j| j.parse().ok());
        let keys = |e: &Executable| apart.get(&e.name).cloned().unwrap_or_default();
        let held = all.iter().map(|e| Apart {
            keys: keys(e),
            ..Apart::default()
        });
        Ok((jobs.unwrap_or_else(tally::cores), held.collect()))
    }
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

/// `cargo test --doc` over `crates` at `root` with `args` after them,
/// both streams written to one file, where a test's line, cargo's
/// `Doc-tests <lib>` and the compiler's words stand in the order they were
/// written: whether it ended well, and the file's text.
fn doc_run(root: &Path, run: &Run, crates: &[&Crate], args: Vec<String>) -> Result<(bool, String)> {
    let dir = &run.target_dir;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let written = dir.join(format!("xtask-doctests-{}.out", std::process::id()));
    let file = File::create(&written).with_context(|| format!("creating {}", written.display()))?;
    let mut command = Command::new("cargo");
    command
        .args(["test", "--doc", "--no-fail-fast", "--color", "never"])
        .current_dir(root)
        .stdout(Stdio::from(
            file.try_clone().context("sharing the doctests' file")?,
        ))
        .stderr(Stdio::from(file));
    for krate in crates {
        command.args(["-p", &krate.package()]);
    }
    command.args(args);
    batch::without_git(&mut command);
    let status = command.status().context("running cargo test --doc");
    let text = fs::read_to_string(&written).unwrap_or_default();
    fs::remove_file(&written).ok();
    Ok((status?.success(), text))
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
    let (ended, text) = doc_run(root, run, &with, Vec::new())?;
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
        let reached = counts.binaries > 0 || ended;
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

/// A `compile_fail` block pinned to the error codes its compile must
/// name: where it stands, as rustdoc names its test, and the codes.
#[derive(Debug, PartialEq, Eq)]
struct Pin {
    file: String,
    line: usize,
    codes: Vec<String>,
}

impl Pin {
    /// The pins of `text`, the source at `file`: each doc comment's fence
    /// that opens a `compile_fail` block with a code among its words, at
    /// the fence's own line, as rustdoc numbers the block's test.
    fn of(file: &str, text: &str) -> Vec<Pin> {
        let mut pins = Vec::new();
        for (at, line) in text.lines().enumerate() {
            let Some(words) = ["//!", "///"]
                .iter()
                .find_map(|doc| line.trim_start().strip_prefix(doc))
                .and_then(|rest| rest.trim_start().strip_prefix("```"))
            else {
                continue;
            };
            let words: Vec<&str> = words
                .split(|c: char| c == ',' || c.is_whitespace())
                .collect();
            let codes: Vec<String> = words
                .iter()
                .filter(|w| {
                    w.len() == 5 && w.starts_with('E') && w[1..].bytes().all(|b| b.is_ascii_digit())
                })
                .map(|w| (*w).to_owned())
                .collect();
            if words.contains(&"compile_fail") && !codes.is_empty() {
                pins.push(Pin {
                    file: file.to_owned(),
                    line: at + 1,
                    codes,
                });
            }
        }
        pins
    }

    /// Each pin of `crates` whose block's compile named none of its codes,
    /// in words, by crate: the doctests of the files that pin a code, in
    /// crates whose doctests built, run one at a time with the compiler's
    /// words on the stream (`--nocapture`), and each `compile_fail`
    /// block's codes read from its `error[E....]` lines, the words the
    /// nightly's rustdoc reads them from.
    fn missed(root: &Path, run: &Run, crates: &[&Crate]) -> Result<BTreeMap<String, Vec<String>>> {
        let mut pinned: Vec<(&Crate, Vec<Pin>)> = Vec::new();
        for krate in crates {
            if !run
                .crates
                .get(&krate.name)
                .is_some_and(|b| b.whole && b.doctests)
            {
                continue;
            }
            let mut pins = Vec::new();
            for file in sources::rust(root, krate)? {
                let text = fs::read_to_string(root.join(&file))
                    .with_context(|| format!("reading {file}"))?;
                pins.extend(Pin::of(&file, &text));
            }
            if !pins.is_empty() {
                pinned.push((krate, pins));
            }
        }
        if pinned.is_empty() {
            return Ok(BTreeMap::new());
        }
        // A test's name is its file, its item and its line; `compile fail`
        // is printed beside it and filters nothing, so the filters are the
        // pinned files and every doctest of them runs.
        let mut args: Vec<String> = ["--", "--nocapture", "--test-threads=1"]
            .map(String::from)
            .into();
        let files: std::collections::BTreeSet<String> = pinned
            .iter()
            .flat_map(|(_, pins)| pins.iter().map(|pin| format!("{} - ", pin.file)))
            .collect();
        args.extend(files);
        let krates: Vec<&Crate> = pinned.iter().map(|(krate, _)| *krate).collect();
        let said = Pin::said(&doc_run(root, run, &krates, args)?.1);
        let mut missed = BTreeMap::new();
        for (krate, pins) in pinned {
            let words: Vec<String> = pins.iter().filter_map(|pin| pin.missed_in(&said)).collect();
            if !words.is_empty() {
                missed.insert(krate.name.clone(), words);
            }
        }
        Ok(missed)
    }

    /// The codes each `compile fail` test's compile named in `text`, by
    /// the file and line its name carries: `test <file> - <item> (line
    /// <n>) - compile fail ... ` and every `error[E....]` after it until
    /// the next test.
    fn said(text: &str) -> BTreeMap<(String, usize), Vec<String>> {
        let mut said: BTreeMap<(String, usize), Vec<String>> = BTreeMap::new();
        let mut current: Option<(String, usize)> = None;
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("test ") {
                current = name.split_once(" - compile fail").and_then(|(name, _)| {
                    let (file, rest) = name.split_once(" - ")?;
                    let at = rest.rsplit_once("(line ")?.1.strip_suffix(')')?;
                    Some((file.to_owned(), at.parse().ok()?))
                });
            }
            let Some(at) = &current else {
                continue;
            };
            let codes = said.entry(at.clone()).or_default();
            for (from, _) in line.match_indices("error[E") {
                let code = &line[from + "error[".len()..];
                if let Some(end) = code.find(']') {
                    codes.push(code[..end].to_owned());
                }
            }
        }
        said
    }

    /// This pin in words where the compile `said` names none of its codes,
    /// or its block was not compiled at all.
    fn missed_in(&self, said: &BTreeMap<(String, usize), Vec<String>>) -> Option<String> {
        let at = format!("{}:{}", self.file, self.line);
        let pinned = self.codes.join(", ");
        match said.get(&(self.file.clone(), self.line)) {
            None => Some(format!(
                "{at}: a compile_fail block pinned to {pinned} was not compiled"
            )),
            Some(named) if !self.codes.iter().any(|c| named.contains(c)) => Some(format!(
                "{at}: a compile_fail block pinned to {pinned} failed with {}",
                if named.is_empty() {
                    "no error code".to_owned()
                } else {
                    named.join(", ")
                }
            )),
            Some(_) => None,
        }
    }
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
    /// The counts as the reading's numbers.
    fn count(&self, reading: &mut Reading) -> Result<()> {
        reading.count("passed", self.passed)?;
        reading.count("failed", self.failed)?;
        reading.count("ignored", self.ignored)?;
        reading.count("filtered", self.filtered)
    }

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
    use super::{Counts, Pin};

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

    /// A pin is a `compile_fail` fence of a doc comment with a code among
    /// its words, at the fence's line; one with no code, or a code on a
    /// fence that is not `compile_fail`, pins nothing (#2049).
    #[test]
    fn a_pin_is_a_compile_fail_fence_with_a_code_at_its_line() {
        let text = "/// ```compile_fail,E0382\n/// x\n/// ```\n//! ```compile_fail\n//! ```\n    /// ```rust, compile_fail, E0277 E0308\n/// ```ignore,E0001\n";
        let pin = |line, codes: &[&str]| Pin {
            file: "src/a.rs".into(),
            line,
            codes: codes.iter().map(|c| (*c).to_owned()).collect(),
        };
        assert_eq!(
            Pin::of("src/a.rs", text),
            [pin(1, &["E0382"]), pin(6, &["E0277", "E0308"])]
        );
    }

    /// The codes are read off each `compile_fail` test's words in the order
    /// one process wrote them, and a pin is missed where its block's
    /// compile named another code, none, or did not run: the mutation a
    /// stable rustdoc passes (#2049).
    #[test]
    fn a_pin_its_compile_does_not_name_is_missed() {
        let text = "running 2 tests\ntest crates/a/src/o.rs - o::T (line 115) - compile fail ... error[E0382]: use of moved value\nerror: aborting due to 1 previous error\n\nFor more information about this error, try `rustc --explain E0382`.\nok\ntest crates/a/src/o.rs - o::T (line 162) - compile fail ... error: cannot construct\nok\ntest result: ok. 2 passed\n";
        let said = Pin::said(text);
        let pin = |line, code: &str| Pin {
            file: "crates/a/src/o.rs".into(),
            line,
            codes: vec![code.into()],
        };
        assert_eq!(pin(115, "E0382").missed_in(&said), None);
        assert_eq!(
            pin(115, "E0999").missed_in(&said).as_deref(),
            Some("crates/a/src/o.rs:115: a compile_fail block pinned to E0999 failed with E0382")
        );
        assert_eq!(
            pin(162, "E0451").missed_in(&said).as_deref(),
            Some(
                "crates/a/src/o.rs:162: a compile_fail block pinned to E0451 failed with no error code"
            )
        );
        assert_eq!(
            pin(9, "E0382").missed_in(&said).as_deref(),
            Some("crates/a/src/o.rs:9: a compile_fail block pinned to E0382 was not compiled")
        );
    }
}
