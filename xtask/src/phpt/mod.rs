//! `xtask phpt`, the subset runner (harness-design.md §3, §7, §8; design
//! §8). One subset of the pinned corpus is run through the engine under the
//! oracle's own recipe, each test's output held to its `--EXPECT*--`
//! section with php's normalisation, and the subset ends as one reading of
//! instrument `phpt`, scoped to the crate whose subset it is, which is what
//! `done` reads as condition 1.
//!
//! **What runs the test.** Design §8's `ScriptExec` seam has two adapters:
//! the fresh-process one, [`oracle::Runner`], and the in-process `Capture`
//! sink under `Engine::serve`, which arrives with the engine. The process
//! adapter, `adapter`, runs the engine's own `php` under
//! `Oracle::run_under`, the same recipe the oracle runs under, which is
//! what makes the two comparable. Until `crates/php` is built there is no
//! binary and the reading is a skip that says so.
//!
//! **What a difference means.** Every verdict goes through
//! [`oracle::verdict`], so the divergence registry is what decides whether
//! a difference is accepted, here as everywhere (harness §4.3). The key is
//! `phpt:php-src/<path in the corpus>`, so a row names the corpus and not
//! this machine.
//!
//! **What is the lane module's.** The tally, the print loop, the runner
//! ladder and the reading's status are [`lane`]'s, shared with `witness`;
//! this module contributes [`Judgement`], the per-item judge and the words
//! of the subset's message.
//!
//! **What one run is.** One process per test, one test at a time, so
//! `--CONFLICTS--` is honoured by construction: nothing else of the corpus
//! is running while a test does.
//!
//! **Which subsets a lane runs.** The `[[crate]]` row, and nothing here:
//! a crate owns the directories of the corpus its row names, an extension
//! owns `ext/<name>/tests` by the design, and a crate that owns none is
//! asked for no phpt reading. A bare call is the per-commit lane, which is
//! every subset of every crate the default build has, in manifest order
//! (harness §7); `--subset` is one crate's subsets or one directory of the
//! pinned tree.
//!
//! **What is not run here.** The full corpus, the nightly verdict control
//! and the tier's lanes are CI's, under the manifest's `lanes.nightly`
//! rule; the per-commit lane and one subset are an agent's to run. A CGI-shaped test, a
//! `--PHPDBG--` one and a `--REDIRECTTEST--` are skipped with their reason:
//! this runner hosts the command line. Beyond those, the process adapter
//! gives a test one stdin and two streams captured as one, so a test that
//! wants a terminal on a stream it did not capture, an interactive shell
//! or a listening server is where the runner and upstream still part; the
//! four of `sapi/cli/tests` that do are named in harness-design.md §12
//! item 17.

mod adapter;
pub mod file;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::divergence::Registry;
use crate::lane::{self, Outcome};
use crate::manifest::{Crate, Manifest};
use crate::oracle::{Oracle, Runner};
use crate::reading::{Reading, listed};
use adapter::Adapter;

/// The instrument's name, the reading's `instrument` field.
pub const INSTRUMENT: &str = "phpt";
/// The first segment of a phpt divergence key, so a row names the corpus
/// and not the path this machine keeps it at.
pub const CORPUS: &str = "php-src";
/// The compiled tier's lane (jit-design.md §7): the engine runs with the
/// `jit.mode` directive set and the corpus's expectations must hold
/// unchanged under it, which is G5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Lane {
    /// Every eligible root is compiled at its first execution.
    Force,
    /// Every guard fails once, so every landing is taken.
    ExitFuzz,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Force => "force",
            Lane::ExitFuzz => "exit-fuzz",
        }
    }

    /// The engine-only directive the lane sets (jit-design.md §7).
    fn ini(self) -> String {
        format!("jit.mode={}", self.as_str())
    }
}

/// One directory of the corpus: where it stands in the pinned tree, and
/// where it stands on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    /// The directory, relative to the pinned php-src tree.
    pub under: PathBuf,
    /// The same directory on this machine.
    pub dir: PathBuf,
}

impl Dir {
    fn at(source: &Path, under: PathBuf) -> Dir {
        Dir {
            dir: source.join(&under),
            under,
        }
    }

    /// Where the test stands inside this directory.
    fn inside<'a>(&self, test: &'a Path) -> &'a Path {
        test.strip_prefix(&self.dir).unwrap_or(test)
    }
}

/// One subset of the corpus: the directories its tests are in and what a
/// reading of it is scoped to. A crate's subset is every directory its
/// `[[crate]]` row owns, so the crate is measured by one reading whatever
/// number of the corpus's directories it is held to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subset {
    /// The reading's scope: the crate whose subset it is (`ext/pcre`,
    /// `php_engine`), or the corpus directory when a caller named one
    /// (`Zend/tests`), which no crate owns and `done` never reads.
    pub scope: String,
    /// The directories, in the order the row names them.
    pub dirs: Vec<Dir>,
}

impl Subset {
    /// The subset a name asks for: a crate of the manifest by name
    /// (`php_engine`, `pcre`, `ext/pcre`), whose subsets its row owns, or
    /// a directory of the pinned tree (`Zend/tests`), whose scope is that
    /// path. A crate that owns none is refused by saying so: which
    /// subsets are a crate's is the manifest's sentence, and an absent one
    /// is an answer, not a lookup that failed.
    pub fn resolve(manifest: &Manifest, name: &str) -> Result<Subset> {
        let source = manifest.pin.source.tree();
        let named = name.trim_matches('/');
        let as_crate = format!("ext/{named}");
        if let Some(krate) = manifest
            .crate_named(named)
            .or_else(|| manifest.crate_named(&as_crate))
        {
            if let Some(subset) = Subset::of(manifest, krate) {
                return Ok(subset);
            }
            bail!(
                "{}: the [[crate]] row names no phpt subset, so the crate has none and `done` asks it for no phpt reading",
                krate.name
            );
        }
        let under = PathBuf::from(named);
        if source.join(&under).is_dir() {
            return Ok(Subset {
                scope: named.to_owned(),
                dirs: vec![Dir::at(&source, under)],
            });
        }
        bail!(
            "no subset {name:?}: the manifest has no crate {named} and no crate {as_crate}, and the pinned php-src at {} ({}) has no {named}",
            source.display(),
            manifest.pin.source.tag
        )
    }

    /// The subset one crate owns: every directory its row names, and
    /// nothing when it names none. Whether the pinned tree has them is not
    /// asked here: a directory the tree lacks is a reading that says so,
    /// since dropping it quietly would leave `done` asking a crate for a
    /// phpt reading nothing could write, which is the hole #18 closed.
    pub fn of(manifest: &Manifest, krate: &Crate) -> Option<Subset> {
        let source = manifest.pin.source.tree();
        let dirs: Vec<Dir> = krate
            .subsets()
            .into_iter()
            .map(|under| Dir::at(&source, under))
            .collect();
        (!dirs.is_empty()).then(|| Subset {
            scope: krate.name.clone(),
            dirs,
        })
    }

    /// The directories the subset names that the pinned tree does not
    /// have, by their place in it.
    pub fn absent(&self) -> Vec<&Path> {
        self.dirs
            .iter()
            .filter(|place| !place.dir.is_dir())
            .map(|place| place.under.as_path())
            .collect()
    }

    /// The per-commit lane (harness §7): every subset of every crate the
    /// default build has, in manifest order. The row is the statement and
    /// this is its only reader, so the lane is never a second copy of the
    /// design's table; a crate behind a feature is outside the default
    /// build and outside the lane.
    pub fn lane(manifest: &Manifest) -> Vec<Subset> {
        manifest
            .crates
            .iter()
            .filter(|krate| !krate.optional())
            .filter_map(|krate| Subset::of(manifest, krate))
            .collect()
    }

    /// Every subset of the corpus: the nightly lane's, which is `Zend`,
    /// `tests` and every `tests` directory under `ext` and `sapi`.
    pub fn corpus(manifest: &Manifest) -> Vec<Subset> {
        let source = manifest.pin.source.tree();
        let mut subsets = Vec::new();
        let mut push = |under: PathBuf| {
            if source.join(&under).is_dir() {
                subsets.push(Subset {
                    scope: under.to_string_lossy().into_owned(),
                    dirs: vec![Dir::at(&source, under)],
                });
            }
        };
        for name in ["Zend/tests", "tests"] {
            push(PathBuf::from(name));
        }
        for parent in ["ext", "sapi"] {
            let Ok(entries) = fs::read_dir(source.join(parent)) else {
                continue;
            };
            let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            names.sort();
            for path in names {
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                push(Path::new(parent).join(name).join("tests"));
            }
        }
        subsets
    }

    /// Every `.phpt` under the subset's directories, sorted within each.
    pub fn tests(&self) -> Result<Vec<PathBuf>> {
        let mut all = Vec::new();
        for place in &self.dirs {
            let mut found = Vec::new();
            walk(&place.dir, &mut found)
                .with_context(|| format!("listing the subset at {}", place.dir.display()))?;
            found.retain(|path| path.extension().is_some_and(|e| e == "phpt"));
            found.sort();
            all.append(&mut found);
        }
        Ok(all)
    }

    /// The divergence key of one test: `phpt:php-src/ext/pcre/tests/x.phpt`,
    /// where the test stands in the corpus whichever directory of the
    /// subset it came from.
    pub fn id(&self, test: &Path) -> String {
        let under = match self.found_in(test) {
            Some(place) => place.under.join(place.inside(test)),
            None => PathBuf::from(self.name(test)),
        };
        format!("phpt:{CORPUS}/{}", under.display())
    }

    /// The test's name inside the subset: inside its directory, with the
    /// directory named as well when the subset has more than one, so two
    /// tests of the same name are told apart.
    pub fn name(&self, test: &Path) -> String {
        let Some(place) = self.found_in(test) else {
            // Not a test of this subset: its name is its own, never the
            // path this machine keeps it at, which `id` would carry into a
            // divergence key.
            return test
                .file_name()
                .unwrap_or(test.as_os_str())
                .to_string_lossy()
                .into_owned();
        };
        let inside = place.inside(test);
        let named = if self.dirs.len() > 1 {
            place.under.join(inside)
        } else {
            inside.to_path_buf()
        };
        named.to_string_lossy().into_owned()
    }

    /// The directory of the subset the test was found under.
    fn found_in(&self, test: &Path) -> Option<&Dir> {
        self.dirs.iter().find(|place| test.starts_with(&place.dir))
    }

    /// The reading's scope: the subset's, and what the run was beside it,
    /// so that a tier lane and a control of the runner each keep a reading
    /// of their own and neither displaces the engine's.
    pub fn scope(&self, runner: &Runner, lane: Option<Lane>) -> String {
        let mut scope = self.scope.clone();
        if matches!(runner, Runner::Control(_)) {
            scope.push_str("@control");
        }
        if let Some(lane) = lane {
            scope.push('@');
            scope.push_str(lane.as_str());
        }
        scope
    }
}

/// Every file under `dir`, recursively; a missing directory has none.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// What one test came to: upstream's outcomes less the two a leak checker
/// decides, and `Diverged`, which is the registry's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    Pass,
    /// The expectation is missed and an accepted registry row says so.
    Diverged,
    Fail,
    Skip,
    /// The file is not a test this runner can read.
    Bork,
    /// A `--XFAIL--` test that failed, which is what that section asks for.
    XFail,
}

impl lane::Judgement for Judgement {
    const PASS: Judgement = Judgement::Pass;

    fn as_str(self) -> &'static str {
        match self {
            Judgement::Pass => "pass",
            Judgement::Diverged => "diverged",
            Judgement::Fail => "fail",
            Judgement::Skip => "skip",
            Judgement::Bork => "bork",
            Judgement::XFail => "xfail",
        }
    }

    /// Whether the subset's reading fails on it.
    fn counts_against(self) -> bool {
        matches!(self, Judgement::Fail | Judgement::Bork)
    }

    fn met(self) -> bool {
        matches!(self, Judgement::Pass | Judgement::Diverged)
    }
}

/// One test's verdict: its divergence key, its name inside the subset,
/// the judgement and the note.
pub type Judged = lane::Judged<Judgement>;

/// What one subset's run came to.
pub type Tally = lane::Tally<Judgement>;

/// The subset's reading: the counts in `numbers`, the failures named in
/// the message, and the status the ladder allows.
pub fn reading(
    tally: &Tally,
    root: &Path,
    manifest: &Manifest,
    subset: &Subset,
    runner: &Runner,
    lane: Option<Lane>,
) -> Result<Reading> {
    let scope = subset.scope(runner, lane);
    let absent = subset.absent();
    // A directory the row names and the pinned tree has not: the manifest
    // and the pin disagree, and no run can settle it. It is the reading,
    // ahead of whether an engine is built, because building one would not
    // answer it.
    let outcome = if absent.is_empty() {
        tally.outcome(runner)
    } else {
        Outcome::Failed
    };
    let message = if absent.is_empty() {
        match &outcome {
            Outcome::Failed => format!(
                "{scope}: {} of {} missed their expectation{}",
                tally.against().len(),
                tally.judged.len(),
                tally.missed()
            ),
            Outcome::Absent(why) => {
                format!("{scope}: no subset run over {} tests; {why}", tally.found)
            }
            Outcome::Control(binary) => format!(
                "{scope}: {} of {} differ under {}, the pinned oracle; a control of the runner, not a reading of the engine{}",
                tally.against().len(),
                tally.judged.len(),
                binary.display(),
                tally.missed()
            ),
            Outcome::NothingRan => format!(
                "{scope}: no test of the subset ran; {} found, {} skipped",
                tally.found,
                tally.count(Judgement::Skip)
            ),
            Outcome::Passed => format!(
                "{scope}: {} passed, {} skipped of {} tests",
                tally.count(Judgement::Pass),
                tally.count(Judgement::Skip),
                tally.judged.len()
            ),
        }
    } else {
        let named: Vec<String> = absent
            .iter()
            .map(|under| under.to_string_lossy().into_owned())
            .collect();
        let named: Vec<&str> = named.iter().map(String::as_str).collect();
        format!(
            "{scope}: the [[crate]] row names {}, and the pinned php-src at {} ({}) has no such directory",
            listed(&named),
            manifest.pin.source.tree().display(),
            manifest.pin.source.tag
        )
    };
    // Stamped through the manifest's scope mapping: the crate whose subset
    // it is, so `done` can tell a stale reading from a fresh one.
    let mut reading = tally.reading(
        Reading::for_scope(INSTRUMENT, &scope, manifest, root)?,
        &outcome,
        message,
    )?;
    reading.count("tests", tally.found as u64)?;
    reading.count("passed", tally.count(Judgement::Pass))?;
    reading.count("failed", tally.count(Judgement::Fail))?;
    reading.count("skipped", tally.count(Judgement::Skip))?;
    reading.count("borked", tally.count(Judgement::Bork))?;
    reading.count("xfailed", tally.count(Judgement::XFail))?;
    reading.count("diverged", tally.count(Judgement::Diverged))?;
    // The burndown's number: what met its expectation, over the tests the
    // subset holds, so a `[[week]]` bar can read one figure.
    reading.numbers.insert(
        "percent".to_owned(),
        percent(tally.met(), tally.found as u64).into(),
    );
    Ok(reading)
}

/// What `met` is of `found`, to one decimal; nothing found is nothing met.
fn percent(met: u64, found: u64) -> f64 {
    if found == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = (met as f64 / found as f64) * 1000.0;
    ratio.round() / 10.0
}

/// What one `xtask phpt` run was asked for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    /// One subset by name; one of this and `full` is named.
    pub subset: Option<String>,
    /// The whole corpus: the nightly lane, denied at the tool call.
    pub full: bool,
    pub lane: Option<Lane>,
    /// The binary the process adapter runs; without it, the engine's own
    /// where cargo puts it.
    pub engine: Option<PathBuf>,
}

/// Run what was asked for: one reading per subset, each written by the
/// caller through `record`.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    let subsets = match &ask.subset {
        Some(name) => vec![Subset::resolve(manifest, name)?],
        None if ask.full => Subset::corpus(manifest),
        // The per-commit lane, composed from the `[[crate]]` rows that own
        // a subset (harness §7). The row is where the lane lives, so
        // neither this function nor the design's table is a second copy of
        // the other.
        None => {
            let lane = Subset::lane(manifest);
            let scopes: Vec<&str> = lane.iter().map(|s| s.scope.as_str()).collect();
            println!(
                "phpt: the per-commit lane, {} subset{} the manifest names: {}",
                lane.len(),
                if lane.len() == 1 { "" } else { "s" },
                listed(&scopes)
            );
            lane
        }
    };
    if subsets.is_empty() {
        bail!(
            "no subset to run: no [[crate]] row of the manifest owns one the pinned php-src at {} has",
            manifest.pin.source.tree().display()
        );
    }
    let runner = Runner::resolve(root, &manifest.pin, ask.engine.as_deref())?;
    if let Runner::Absent(why) = &runner {
        println!("phpt: {why}");
    }
    // One oracle for the run: the digest is asserted before the first
    // program and never per test, subset or section (harness §4.3).
    let oracle = manifest.pin.oracle();
    let mut readings = Vec::new();
    for subset in &subsets {
        let tally = run_subset(&oracle, registry, &runner, ask.lane, subset)?;
        readings.push(reading(&tally, root, manifest, subset, &runner, ask.lane)?);
    }
    Ok(readings)
}

/// Run one subset: every test through the runner, each verdict printed as
/// it is reached unless it passed.
pub fn run_subset(
    oracle: &Oracle<'_>,
    registry: &Registry,
    runner: &Runner,
    lane: Option<Lane>,
    subset: &Subset,
) -> Result<Tally> {
    let tests = subset.tests()?;
    let Some(binary) = runner.binary() else {
        return Ok(Tally::found(tests.len()));
    };
    // A digest mismatch refuses the whole run (§4.3), before the first
    // test rather than at its first SKIPIF.
    oracle.asserted()?;
    let adapter = Adapter::new(oracle, registry, binary, lane);
    Tally::judge(&tests, |test| adapter.judge(subset, test))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lane_names_the_engine_only_directive() {
        assert_eq!(Lane::Force.ini(), "jit.mode=force");
        assert_eq!(Lane::ExitFuzz.ini(), "jit.mode=exit-fuzz");
    }
}
