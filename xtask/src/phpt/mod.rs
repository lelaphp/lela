//! `xtask phpt`, the subset runner (harness-design.md §3, §7, §8; design
//! §8). One subset of the pinned corpus is run through the engine under the
//! oracle's own recipe, each test's output held to its `--EXPECT*--`
//! section with php's normalisation, and the subset ends as one reading of
//! instrument `phpt`, scoped to the crate whose subset it is, which is what
//! `done` reads as condition 1.
//!
//! **What runs the test.** Design §8's `ScriptExec` seam has two adapters:
//! the fresh-process one, which is [`Runner`] here, and the in-process
//! `Capture` sink under `Engine::serve`, which arrives with the engine. The
//! process adapter runs the engine's own `php` under [`oracle::run_under`],
//! the same recipe the oracle runs under, which is what makes the two
//! comparable. Until `crates/php` is built there is no binary and the
//! reading is a skip that says so.
//!
//! **What a difference means.** Every verdict goes through
//! [`oracle::verdict`], so the divergence registry is what decides whether
//! a difference is accepted, here as everywhere (harness §4.3). The key is
//! `phpt:php-src/<path in the corpus>`, so a row names the corpus and not
//! this machine.
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

pub mod expect;
pub mod file;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use regex_lite::Regex;

use crate::git::Stamp;
use crate::manifest::{Crate, Manifest};
use crate::oracle::{self, Invocation, Output, Pin, Program, Verdict as Compared};
use crate::reading::{Reading, Status};
use expect::Expectation;
use file::Phpt;

use crate::divergence::Registry;

/// The instrument's name, the reading's `instrument` field.
pub const INSTRUMENT: &str = "phpt";
/// The first segment of a phpt divergence key, so a row names the corpus
/// and not the path this machine keeps it at.
pub const CORPUS: &str = "php-src";
/// The engine's binary, the one `crates/php` builds (design §4.1).
pub const ENGINE: &str = "php";
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
        let source = &manifest.pin.source.path;
        let named = name.trim_matches('/');
        let as_crate = format!("ext/{named}");
        if let Some(krate) = manifest
            .crate_named(named)
            .or_else(|| manifest.crate_named(&as_crate))
        {
            if let Some(subset) = Subset::of(manifest, krate) {
                return Ok(subset);
            }
            let owned: Vec<String> = krate
                .subsets()
                .iter()
                .map(|under| under.to_string_lossy().into_owned())
                .collect();
            if owned.is_empty() {
                bail!(
                    "{}: the [[crate]] row names no phpt subset, so the crate has none and `done` asks it for no phpt reading",
                    krate.name
                );
            }
            let named: Vec<&str> = owned.iter().map(String::as_str).collect();
            bail!(
                "{}: the [[crate]] row names {}, and the pinned php-src at {} ({}) has none of them",
                krate.name,
                listed(&named),
                source.display(),
                manifest.pin.source.tag
            );
        }
        let under = PathBuf::from(named);
        if source.join(&under).is_dir() {
            return Ok(Subset {
                scope: named.to_owned(),
                dirs: vec![Dir::at(source, under)],
            });
        }
        bail!(
            "no subset {name:?}: the manifest has no crate {named} and no crate {as_crate}, and the pinned php-src at {} ({}) has no {named}",
            source.display(),
            manifest.pin.source.tag
        )
    }

    /// The subset one crate owns: the directories of its row that the
    /// pinned tree has, and nothing when it owns none or the tree has none
    /// of them.
    pub fn of(manifest: &Manifest, krate: &Crate) -> Option<Subset> {
        let source = &manifest.pin.source.path;
        let dirs: Vec<Dir> = krate
            .subsets()
            .into_iter()
            .filter(|under| source.join(under).is_dir())
            .map(|under| Dir::at(source, under))
            .collect();
        (!dirs.is_empty()).then(|| Subset {
            scope: krate.name.clone(),
            dirs,
        })
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
        let source = &manifest.pin.source.path;
        let mut subsets = Vec::new();
        let mut push = |under: PathBuf| {
            if source.join(&under).is_dir() {
                subsets.push(Subset {
                    scope: under.to_string_lossy().into_owned(),
                    dirs: vec![Dir::at(source, under)],
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
            return test.to_string_lossy().into_owned();
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

    /// The crate the subset belongs to, whose sources the reading is
    /// stamped over so `done` can tell a stale reading from a fresh one.
    fn stamped(&self, manifest: &Manifest) -> PathBuf {
        match manifest.crate_named(&self.scope) {
            Some(krate) => krate.path(),
            None => PathBuf::from("."),
        }
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

/// What runs a test's program: the fresh-process adapter of design §8's
/// `ScriptExec` seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    /// The engine's own binary, one process per test.
    Engine(PathBuf),
    /// The pinned oracle standing in for the engine: a control of the
    /// runner itself and never a reading of the engine, so its reading is
    /// a skip whatever the tests say.
    Control(PathBuf),
    /// Nothing to run, and why.
    Absent(String),
}

impl Runner {
    /// The binary to run: the one named, or the engine's where cargo puts
    /// it. A binary whose digest is the pin's is the runner's control.
    pub fn resolve(root: &Path, pin: &Pin, named: Option<&Path>) -> Result<Runner> {
        if let Some(path) = named {
            if !path.is_file() {
                bail!("no binary at {}", path.display());
            }
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let digest = format!("{:x}", md5::compute(bytes));
            let path = path.to_path_buf();
            return Ok(if digest == pin.oracle.md5 {
                Runner::Control(path)
            } else {
                Runner::Engine(path)
            });
        }
        for profile in ["release", "debug"] {
            let built = root.join("target").join(profile).join(ENGINE);
            if built.is_file() {
                return Ok(Runner::Engine(built));
            }
        }
        Ok(Runner::Absent(format!(
            "the engine's binary is not built: no target/release/{ENGINE} and no target/debug/{ENGINE}; a CLI request reaches `serve` with php_engine (design §5)"
        )))
    }

    /// The binary, when there is one.
    pub fn binary(&self) -> Option<&Path> {
        match self {
            Runner::Engine(path) | Runner::Control(path) => Some(path),
            Runner::Absent(_) => None,
        }
    }
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

impl Judgement {
    pub fn as_str(self) -> &'static str {
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
}

/// One test's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    /// The divergence key, whatever the judgement.
    pub id: String,
    /// The test's name inside its subset.
    pub name: String,
    pub judgement: Judgement,
    /// Why: the skip's reason, the borkage, the accepted row's reason, or
    /// the difference.
    pub note: String,
}

impl Judged {
    fn new(id: &str, name: &str, judgement: Judgement, note: impl Into<String>) -> Judged {
        Judged {
            id: id.to_owned(),
            name: name.to_owned(),
            judgement,
            note: note.into(),
        }
    }
}

/// What one subset's run came to.
#[derive(Debug, Clone, Default)]
pub struct Tally {
    pub judged: Vec<Judged>,
    /// The tests found, which is more than `judged` when nothing ran.
    pub found: usize,
}

impl Tally {
    fn count(&self, judgement: Judgement) -> u64 {
        self.judged
            .iter()
            .filter(|j| j.judgement == judgement)
            .count() as u64
    }

    /// The tests that fail the subset, by name.
    pub fn against(&self) -> Vec<&str> {
        self.judged
            .iter()
            .filter(|j| j.judgement.counts_against())
            .map(|j| j.name.as_str())
            .collect()
    }

    /// The subset's reading: the counts in `numbers`, the failures named in
    /// the message, and the status the runner allows.
    pub fn reading(
        &self,
        root: &Path,
        manifest: &Manifest,
        subset: &Subset,
        runner: &Runner,
        lane: Option<Lane>,
    ) -> Result<Reading> {
        let scope = subset.scope(runner, lane);
        let mut reading = Reading::new(INSTRUMENT, &scope);
        reading.count("tests", self.found as u64)?;
        reading.count("run", self.judged.len() as u64)?;
        reading.count("passed", self.count(Judgement::Pass))?;
        reading.count("failed", self.count(Judgement::Fail))?;
        reading.count("skipped", self.count(Judgement::Skip))?;
        reading.count("borked", self.count(Judgement::Bork))?;
        reading.count("xfailed", self.count(Judgement::XFail))?;
        reading.count("diverged", self.count(Judgement::Diverged))?;
        // The burndown's number: what met its expectation, over the tests
        // the subset holds, so a `[[week]]` bar can read one figure.
        let met = self.count(Judgement::Pass) + self.count(Judgement::Diverged);
        reading
            .numbers
            .insert("percent".to_owned(), percent(met, self.found as u64).into());
        let against = self.against();
        let missed = if against.is_empty() {
            String::new()
        } else {
            format!(": {}", listed(&against))
        };
        match runner {
            Runner::Absent(why) => {
                reading.status = Status::Skip;
                reading.message =
                    format!("{scope}: no subset run over {} tests; {why}", self.found);
            }
            Runner::Control(binary) => {
                reading.status = Status::Skip;
                reading.message = format!(
                    "{scope}: {} of {} differ under {}, the pinned oracle; a control of the runner, not a reading of the engine{missed}",
                    against.len(),
                    self.judged.len(),
                    binary.display()
                );
            }
            Runner::Engine(_) if !against.is_empty() => {
                reading.status = Status::Fail;
                reading.message = format!(
                    "{scope}: {} of {} missed their expectation{missed}",
                    against.len(),
                    self.judged.len()
                );
            }
            // Nothing met an expectation: an empty subset, or one every
            // test skipped out of. A green reading there would say the
            // engine was measured when it was not.
            Runner::Engine(_)
                if self.count(Judgement::Pass) + self.count(Judgement::Diverged) == 0 =>
            {
                reading.status = Status::Skip;
                reading.message = format!(
                    "{scope}: no test of the subset ran; {} found, {} skipped",
                    self.found,
                    self.count(Judgement::Skip)
                );
            }
            Runner::Engine(_) => {
                reading.status = Status::Pass;
                reading.message = format!(
                    "{scope}: {} passed, {} skipped of {} tests",
                    self.count(Judgement::Pass),
                    self.count(Judgement::Skip),
                    self.judged.len()
                );
            }
        }
        reading.stamp(&Stamp::take(root, &subset.stamped(manifest))?);
        Ok(reading)
    }
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

/// At most five names, and how many more there are; `witness` names its
/// failures the same way.
pub(crate) fn listed(names: &[&str]) -> String {
    let shown = names.iter().take(5).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 5 {
        format!("{shown} and {} more", names.len() - 5)
    } else {
        shown
    }
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
            manifest.pin.source.path.display()
        );
    }
    let runner = Runner::resolve(root, &manifest.pin, ask.engine.as_deref())?;
    if let Runner::Absent(why) = &runner {
        println!("phpt: {why}");
    }
    let mut readings = Vec::new();
    for subset in &subsets {
        let tally = run_subset(manifest, registry, &runner, ask.lane, subset)?;
        readings.push(tally.reading(root, manifest, subset, &runner, ask.lane)?);
    }
    Ok(readings)
}

/// Run one subset: every test through the runner, each verdict printed as
/// it is reached unless it passed.
pub fn run_subset(
    manifest: &Manifest,
    registry: &Registry,
    runner: &Runner,
    lane: Option<Lane>,
    subset: &Subset,
) -> Result<Tally> {
    let tests = subset.tests()?;
    let mut tally = Tally {
        judged: Vec::new(),
        found: tests.len(),
    };
    let Some(binary) = runner.binary() else {
        return Ok(tally);
    };
    let adapter = Adapter::new(&manifest.pin, registry, binary, lane);
    for test in &tests {
        let judged = adapter.judge(subset, test)?;
        if judged.judgement != Judgement::Pass {
            // A failure prints its difference: that is what a person reads
            // to decide whether it is the engine's or the corpus's. Every
            // other judgement is one line, its reason.
            let note = if judged.judgement.counts_against() {
                judged.note.clone()
            } else {
                judged.note.lines().next().unwrap_or_default().to_owned()
            };
            // The line is keyed by the divergence id, which is what a
            // person writes a registry row against when the difference is
            // one to accept (harness §4.3).
            println!("{} {}: {note}", judged.judgement.as_str(), judged.id);
        }
        tally.judged.push(judged);
    }
    Ok(tally)
}

/// The fresh-process adapter of design §8's `ScriptExec` seam, bound to
/// one run: the pin and the registry, the binary it runs, the tier lane it
/// runs under, and the extensions that binary loads, which
/// `--EXTENSIONS--` is answered from.
struct Adapter<'a> {
    pin: &'a Pin,
    registry: &'a Registry,
    binary: &'a Path,
    lane: Option<Lane>,
    loaded: Vec<String>,
}

impl<'a> Adapter<'a> {
    fn new(
        pin: &'a Pin,
        registry: &'a Registry,
        binary: &'a Path,
        lane: Option<Lane>,
    ) -> Adapter<'a> {
        let loaded = loaded_extensions(pin, binary).unwrap_or_else(|e| {
            println!("phpt: {} lists no extension ({e:#})", binary.display());
            Vec::new()
        });
        Adapter {
            pin,
            registry,
            binary,
            lane,
            loaded,
        }
    }

    /// One test, end to end.
    fn judge(&self, subset: &Subset, test: &Path) -> Result<Judged> {
        let id = subset.id(test);
        let name = subset.name(test);
        let phpt = match Phpt::load(test) {
            Ok(phpt) => phpt,
            Err(borked) => return Ok(Judged::new(&id, &name, Judgement::Bork, borked.0)),
        };
        if let Some(why) = unhosted(&phpt) {
            return Ok(Judged::new(&id, &name, Judgement::Skip, why));
        }
        if let Some(why) = self.missing(&phpt) {
            return Ok(Judged::new(&id, &name, Judgement::Skip, why));
        }
        let mut ini = match ini_of(&phpt, test) {
            Ok(ini) => ini,
            Err(why) => return Ok(Judged::new(&id, &name, Judgement::Skip, why)),
        };
        ini.extend(self.lane.map(Lane::ini));
        let mut expected_to_fail = phpt.text("XFAIL").map(|why| why.trim().to_owned());
        if let Some(body) = phpt.filled("SKIPIF") {
            let out = self.run_section(test, "skip", body)?;
            let said = String::from_utf8_lossy(&out.stdout).into_owned();
            if let Some(reason) = reason_after("skip", &said) {
                return Ok(Judged::new(&id, &name, Judgement::Skip, reason));
            }
            if let Some(reason) = reason_after("xfail", &said) {
                expected_to_fail = Some(reason);
            }
        }

        let actual = match self.run_program(&phpt, test, &ini)? {
            Ok(actual) => actual,
            Err(borked) => return Ok(Judged::new(&id, &name, Judgement::Bork, borked)),
        };
        let Some(expectation) = expectation_of(self.pin, &phpt) else {
            return Ok(Judged::new(
                &id,
                &name,
                Judgement::Bork,
                "missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--",
            ));
        };
        let (met, trouble) = match expectation.met(&actual) {
            Ok(met) => (met, String::new()),
            Err(e) => (false, format!("{e:#}\n")),
        };
        let compared = oracle::verdict(self.registry, &id, met, expectation.text(), &actual);
        Ok(match (compared, expected_to_fail) {
            (Compared::Same, Some(why)) => Judged::new(
                &id,
                &name,
                Judgement::Pass,
                format!("the XFAIL section says {why:?} and the test passes"),
            ),
            (Compared::Same, None) => Judged::new(&id, &name, Judgement::Pass, ""),
            (Compared::Diverged(row), _) => {
                Judged::new(&id, &name, Judgement::Diverged, row.reason)
            }
            (Compared::Differs(difference), Some(why)) => {
                Judged::new(&id, &name, Judgement::XFail, format!("{why}\n{difference}"))
            }
            (Compared::Differs(difference), None) => Judged::new(
                &id,
                &name,
                Judgement::Fail,
                format!(
                    "{trouble}the {} section is not what ran:\n{difference}",
                    expectation.section()
                ),
            ),
        })
    }

    /// The test's program, written where upstream writes it, run through
    /// the binary under test with the sections that shape one run, and
    /// taken away after; then `CLEAN`, which takes away what the test
    /// left. A file already standing where the program goes borks the
    /// test rather than being overwritten.
    fn run_program(
        &self,
        phpt: &Phpt,
        test: &Path,
        ini: &[String],
    ) -> Result<Result<Vec<u8>, String>> {
        let program = test.with_extension("php");
        if program.exists() {
            return Ok(Err(format!(
                "a file already stands where the test's program goes, {}",
                program.display()
            )));
        }
        let body = phpt.section("FILE").unwrap_or_default().to_vec();
        let written = Written::new(&program, &body)?;
        let args = words(&phpt.text("ARGS").unwrap_or_default());
        let mut env = executable(self.binary);
        env.extend(env_of(phpt, test));
        let output = oracle::run_under(
            self.pin,
            &Invocation {
                ini,
                args: &args,
                env: &env,
                stdin: phpt.section("STDIN"),
                merge_streams: merges_streams(phpt),
                ..Invocation::new(self.binary, Program::File(&program))
            },
        )
        .with_context(|| format!("running {}", test.display()))?;
        drop(written);
        if let Some(body) = phpt.filled("CLEAN") {
            self.run_section(test, "clean", body)?;
        }
        Ok(Ok(output.stdout))
    }

    /// A section that runs beside the test rather than as it: `SKIPIF`,
    /// which decides whether the test applies at all, and `CLEAN`, which
    /// takes away what it left. Neither is given the test's own `--INI--`,
    /// as upstream gives neither. Both run under the oracle, not the
    /// engine: what they answer must be true of the corpus whatever the
    /// engine can do yet, and a young engine must not be trusted with
    /// cleanup. Upstream runs them under the binary it is testing, so a
    /// `SKIPIF` that asks what the engine has is answered here by php; it
    /// is the runner's one known difference from upstream's verdicts.
    fn run_section(&self, test: &Path, kind: &str, body: &[u8]) -> Result<Output> {
        let program = test.with_extension(format!("{kind}.php"));
        let written = Written::new(&program, body)?;
        let out = oracle::run(self.pin, Program::File(&program), None)
            .with_context(|| format!("running the {kind} section of {}", test.display()))?;
        drop(written);
        Ok(out)
    }

    /// The `--EXTENSIONS--` the runner's binary does not load, in
    /// upstream's words.
    fn missing(&self, phpt: &Phpt) -> Option<String> {
        let wanted = phpt.text("EXTENSIONS").unwrap_or_default().into_owned();
        let missing: Vec<&str> = wanted
            .split_whitespace()
            .filter(|name| !self.loaded.iter().any(|had| had == &name.to_lowercase()))
            .collect();
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "Required extension{} missing: {}",
            if missing.len() > 1 { "s" } else { "" },
            missing.join(", ")
        ))
    }
}

/// The extensions a binary loads under the recipe, as upstream asks for
/// them: `Core` dropped, `Zend OPcache` mapped to `opcache` and the rest
/// lowered (run-tests.php:2908-2916).
fn loaded_extensions(pin: &Pin, binary: &Path) -> Result<Vec<String>> {
    let snippet = r#"echo implode("\n", get_loaded_extensions());"#;
    let out = oracle::run_under(pin, &Invocation::new(binary, Program::Snippet(snippet)))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "Core")
        .map(|name| {
            if name == "Zend OPcache" {
                "opcache".to_owned()
            } else {
                name.to_lowercase()
            }
        })
        .collect())
}

/// Why this runner cannot host a test: it serves the command line, and the
/// three shapes below are another host's or another program's.
fn unhosted(phpt: &Phpt) -> Option<String> {
    if phpt.has("REDIRECTTEST") {
        return Some("a --REDIRECTTEST--; this runner runs a subset, not a redirected set".into());
    }
    if phpt.is_cgi() {
        return Some("a CGI test; this runner hosts the command line".into());
    }
    if phpt.has("PHPDBG") {
        return Some("a --PHPDBG-- test; this engine has no debugger host".into());
    }
    None
}

/// Whether the two streams are captured as one, which is upstream's
/// default and what `--CAPTURE_STDIO--` can narrow.
fn merges_streams(phpt: &Phpt) -> bool {
    match phpt.text("CAPTURE_STDIO") {
        Some(capture) => {
            let capture = capture.to_uppercase();
            capture.contains("STDOUT") && capture.contains("STDERR")
        }
        None => true,
    }
}

/// The expectation the test carries, normalised as php normalises it.
fn expectation_of(pin: &Pin, phpt: &Phpt) -> Option<Expectation> {
    if let Some(body) = phpt.section("EXPECT") {
        return Some(Expectation::Exact(trimmed(pin, body)));
    }
    if let Some(body) = phpt.text("EXPECTF") {
        return Some(Expectation::Format(trimmed_text(pin, &body)));
    }
    phpt.text("EXPECTREGEX")
        .map(|body| Expectation::Pattern(trimmed_text(pin, &body)))
}

/// The recipe's normalisation over a section, which is what the output it
/// is compared with has been through; the oracle module owns the rule.
fn trimmed(pin: &Pin, body: &[u8]) -> Vec<u8> {
    oracle::normalise(&pin.recipe.normalise, body.to_vec())
}

/// The same over a section a runner reads as words.
fn trimmed_text(pin: &Pin, text: &str) -> String {
    String::from_utf8_lossy(&trimmed(pin, text.as_bytes())).into_owned()
}

/// A test's `--INI--`, one `-d` line each, with upstream's four
/// substitutions; an unset `{ENV:NAME}` is a skip with upstream's reason.
fn ini_of(phpt: &Phpt, test: &Path) -> Result<Vec<String>, String> {
    let Some(body) = phpt.text("INI") else {
        return Ok(Vec::new());
    };
    let body = substituted(&body, test);
    let mail = Regex::new(r"\{MAIL:(\S+)\}").map_err(|e| e.to_string())?;
    let body = mail.replace_all(&body, "tee $1 >/dev/null").into_owned();
    let env = Regex::new(r"\{ENV:(\S+)\}").map_err(|e| e.to_string())?;
    let mut unset = None;
    let body = replace_all(&env, &body, |name| {
        std::env::var(name).unwrap_or_else(|_| {
            unset = Some(format!("Environment variable {name} is not set"));
            String::new()
        })
    });
    if let Some(reason) = unset {
        return Err(reason);
    }
    Ok(body
        .lines()
        .map(str::trim)
        .filter(|line| line.contains('='))
        .map(str::to_owned)
        .collect())
}

/// The binary under test, under the two names the corpus spawns it by
/// (run-tests.php:670-673). The recipe's `TEST_PHP_EXECUTABLE` names the
/// pinned oracle, which is right for a witness and wrong for a phpt lane:
/// what a test spawns is the binary being measured, so these two stand
/// over the recipe's and under the test's own `--ENV--`.
fn executable(binary: &Path) -> Vec<(String, String)> {
    let path = binary.to_string_lossy().into_owned();
    let escaped = format!("'{}'", path.replace('\'', r"'\''"));
    vec![
        ("TEST_PHP_EXECUTABLE".to_owned(), path),
        ("TEST_PHP_EXECUTABLE_ESCAPED".to_owned(), escaped),
    ]
}

/// A test's `--ENV--`, one `NAME=value` a line.
fn env_of(phpt: &Phpt, test: &Path) -> Vec<(String, String)> {
    substituted(&phpt.text("ENV").unwrap_or_default(), test)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// `{PWD}` and `{TMP}`, which stand for the test's own directory and the
/// scratch one.
fn substituted(body: &str, test: &Path) -> String {
    let beside = test
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let scratch = std::env::temp_dir().to_string_lossy().into_owned();
    body.replace("{PWD}", &beside).replace("{TMP}", &scratch)
}

/// `Regex::replace_all` with a closure over the one capture, which
/// `regex_lite` does not take directly.
fn replace_all(regex: &Regex, text: &str, mut with: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in regex.captures_iter(text) {
        let Some(whole) = found.get(0) else { continue };
        let name = found.get(1).map_or("", |m| m.as_str());
        out.push_str(&text[last..whole.start()]);
        out.push_str(&with(name));
        last = whole.end();
    }
    out.push_str(&text[last..]);
    out
}

/// The reason a `SKIPIF` gave, when its first word is `word`:
/// upstream's `/^skip\s*(.+)/i` over the section's output.
fn reason_after(word: &str, said: &str) -> Option<String> {
    let said = said.trim_start();
    if said.len() < word.len() || !said[..word.len()].eq_ignore_ascii_case(word) {
        return None;
    }
    Some(
        said[word.len()..]
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .to_owned(),
    )
}

/// A command line's words, split on whitespace: a test's `--ARGS--`.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// A file written beside a test for the length of its run and taken away
/// after, which is where upstream puts a test's program too.
struct Written(PathBuf);

impl Written {
    fn new(path: &Path, body: &[u8]) -> Result<Written> {
        fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(Written(path.to_path_buf()))
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lane_names_the_engine_only_directive() {
        assert_eq!(Lane::Force.ini(), "jit.mode=force");
        assert_eq!(Lane::ExitFuzz.ini(), "jit.mode=exit-fuzz");
    }

    #[test]
    fn a_skipif_reason_is_the_text_after_the_word() {
        assert_eq!(
            reason_after("skip", "skip no pcre\n").as_deref(),
            Some("no pcre")
        );
        assert_eq!(
            reason_after("skip", "SKIP  no pcre").as_deref(),
            Some("no pcre")
        );
        assert_eq!(reason_after("skip", "skip").as_deref(), Some(""));
        assert_eq!(reason_after("skip", "nothing"), None);
        assert_eq!(
            reason_after("xfail", "xfail known"),
            Some("known".to_owned())
        );
    }

    #[test]
    fn the_streams_are_one_unless_capture_stdio_says_otherwise() {
        let phpt = |text: &str| Phpt::parse(Path::new("x.phpt"), text.as_bytes()).unwrap();
        let plain = phpt("--TEST--\nn\n--FILE--\nx\n--EXPECT--\n1\n");
        assert!(merges_streams(&plain));
        let one =
            phpt("--TEST--\nn\n--CAPTURE_STDIO--\nSTDIN STDOUT\n--FILE--\nx\n--EXPECT--\n1\n");
        assert!(!merges_streams(&one));
    }
}
