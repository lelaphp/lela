//! `xtask phpt`, the subset runner (harness-design.md §3, §7, §8; design
//! §8). One subset of the pinned corpus is run through the engine under the
//! oracle's own recipe, what each test wrote held to its `--EXPECT*--`
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
//! binary and the reading is a skip that says so. The recipe is the pin's,
//! so the pin's digest is asserted once before the first test where the
//! pinned php is on this machine; where it is not, as on the runner (harness
//! D.20), nothing runs and the reading is a skip that says so, as the
//! witness lane's is, rather than a refusal over a binary no test spawns.
//!
//! **Which engine wrote the numbers.** The reading names the binary that
//! ran it, its path and the md5 of its bytes, and a binary the tree has
//! moved past runs nothing at all: its reading is a skip counting
//! `engine_stale`, beside the `engine_absent` one `done` excuses. A
//! reading is stamped with the tree, and before this one tree carried two
//! readings 37 tests apart depending on which binary happened to stand in
//! `target` (#475).
//!
//! **What a difference means.** Every verdict goes through
//! [`oracle::verdict`], so the divergence registry is what decides whether
//! a difference is accepted, here as everywhere (harness §4.3). The key is
//! `phpt:php-src/<path in the corpus>`, so a row names the corpus and not
//! this machine.
//!
//! **What is the tally's.** The verdicts, the print loop, the runner
//! ladder and the reading's status are [`tally`]'s, shared with `witness`;
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
mod census;
pub(crate) mod file;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::divergence::Registry;
use crate::manifest::{Crate, Manifest};
use crate::oracle::{Oracle, Runner};
use crate::reading::{Reading, listed};
use crate::sources;
use crate::tally::{self, Outcome};
use adapter::Adapter;

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "phpt";
/// The first segment of a phpt divergence key, so a row names the corpus
/// and not the path this machine keeps it at.
pub(crate) const CORPUS: &str = "php-src";
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
    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) struct Dir {
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
pub(crate) struct Subset {
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
    /// a directory of the pinned tree at `source` (`Zend/tests`), whose
    /// scope is that path. A crate that owns none is refused by saying so:
    /// which subsets are a crate's is the manifest's sentence, and an
    /// absent one is an answer, not a lookup that failed.
    pub(crate) fn resolve(manifest: &Manifest, source: &Path, name: &str) -> Result<Subset> {
        let named = name.trim_matches('/');
        let as_crate = format!("ext/{named}");
        if let Some(krate) = manifest
            .crate_named(named)
            .or_else(|| manifest.crate_named(&as_crate))
        {
            if let Some(subset) = Subset::of(source, krate) {
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
                dirs: vec![Dir::at(source, under)],
            });
        }
        bail!(
            "no subset {name:?}: the manifest has no crate {named} and no crate {as_crate}, and the pinned php-src at {} ({}) has no {named}",
            source.display(),
            manifest.pin.source.tag
        )
    }

    /// The subset one crate owns: every directory its row names under the
    /// pinned tree at `source`, and nothing when it names none. Whether
    /// the tree has them is not asked here: a directory the tree lacks is
    /// a reading that says so, since dropping it quietly would leave
    /// `done` asking a crate for a phpt reading nothing could write, which
    /// is the hole #18 closed.
    pub(crate) fn of(source: &Path, krate: &Crate) -> Option<Subset> {
        let dirs: Vec<Dir> = krate
            .subsets()
            .into_iter()
            .map(|under| Dir::at(source, under))
            .collect();
        (!dirs.is_empty()).then(|| Subset {
            scope: krate.name.clone(),
            dirs,
        })
    }

    /// The directories the subset names that the pinned tree does not
    /// have, by their place in it.
    pub(crate) fn absent(&self) -> Vec<&Path> {
        self.dirs
            .iter()
            .filter(|place| !place.dir.is_dir())
            .map(|place| place.under.as_path())
            .collect()
    }

    /// The per-commit lane (harness §7): every subset of every crate the
    /// default build has, in manifest order, under the pinned tree at
    /// `source`. The row is the statement and this is its only reader, so
    /// the lane is never a second copy of the design's table; a crate
    /// behind a feature is outside the default build and outside the lane.
    pub(crate) fn lane(manifest: &Manifest, source: &Path) -> Vec<Subset> {
        manifest
            .crates
            .iter()
            .filter(|krate| !krate.optional())
            .filter_map(|krate| Subset::of(source, krate))
            .collect()
    }

    /// Every subset of the corpus at `source`: the nightly lane's, which
    /// is `Zend`, `tests` and every `tests` directory under `ext` and
    /// `sapi`.
    pub(crate) fn corpus(source: &Path) -> Vec<Subset> {
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
    pub(crate) fn tests(&self) -> Result<Vec<PathBuf>> {
        let mut all = Vec::new();
        for place in &self.dirs {
            let mut found = Vec::new();
            tally::files_under(&place.dir, &mut found)
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
    pub(crate) fn id(&self, test: &Path) -> String {
        let under = match self.found_in(test) {
            Some(place) => place.under.join(place.inside(test)),
            None => PathBuf::from(self.name(test)),
        };
        format!("phpt:{CORPUS}/{}", under.display())
    }

    /// The test's name inside the subset: inside its directory, with the
    /// directory named as well when the subset has more than one, so two
    /// tests of the same name are told apart.
    pub(crate) fn name(&self, test: &Path) -> String {
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
    pub(crate) fn scope(&self, runner: &Runner, lane: Option<Lane>) -> String {
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

/// What one test came to: upstream's outcomes less the two a leak checker
/// decides, and `Diverged`, which is the registry's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Judgement {
    Pass,
    /// The expectation is missed and an accepted registry row says so.
    Diverged,
    Fail,
    Skip,
    /// The file is not a test this runner can read.
    Bork,
    /// A `--XFAIL--` test that failed, which is what that section asks for.
    XFail,
    /// The compiler refused the test's program with its own parse error,
    /// the `cannot read yet` refusal `php_compile` raises for a construct
    /// it does not read, before any test code ran: a skip of the
    /// compiler's and not a failure of the crate whose subset it is
    /// (harness §4.5 condition 1, #319).
    Unread,
}

impl tally::Judgement for Judgement {
    const PASS: Judgement = Judgement::Pass;

    fn as_str(self) -> &'static str {
        match self {
            Judgement::Pass => "pass",
            Judgement::Diverged => "diverged",
            Judgement::Fail => "fail",
            Judgement::Skip => "skip",
            Judgement::Bork => "bork",
            Judgement::XFail => "xfail",
            Judgement::Unread => "unread",
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
pub(crate) type Judged = tally::Judged<Judgement>;

/// What one subset's run came to.
pub(crate) type Tally = tally::Tally<Judgement>;

/// What a subset ran under: the runner, the lane it was asked for, and
/// why the pinned php is not on this machine when it is not, in which
/// case nothing ran under the engine and the reading says so.
#[derive(Clone, Copy)]
pub(crate) struct Under<'a> {
    pub runner: &'a Runner,
    pub lane: Option<Lane>,
    pub oracle_absent: Option<&'a str>,
}

/// The subset's reading: the counts in `numbers`, the failures named in
/// the message, and the status the ladder allows.
pub(crate) fn reading(
    tally: &Tally,
    root: &Path,
    manifest: &Manifest,
    source: &Path,
    subset: &Subset,
    under: Under<'_>,
) -> Result<Reading> {
    let Under {
        runner,
        lane,
        oracle_absent,
    } = under;
    let scope = subset.scope(runner, lane);
    let absent = subset.absent();
    // A subset of a crate not yet created: its tests are that crate's, and
    // an engine built without it would not answer for it, so the reading
    // says so ahead of whether an engine is built, as `budget` and
    // `witness` do; the control of the runner is the oracle's, whatever
    // stands where the crate would (#64).
    let uncreated = !matches!(runner, Runner::Control(_))
        && manifest
            .crate_named(&subset.scope)
            .is_some_and(|krate| !sources::exists(root, krate));
    // A directory the row names and the pinned tree has not: the manifest
    // and the pin disagree, and no run can settle it. It is the reading,
    // ahead of whether an engine is built, because building one would not
    // answer it.
    let outcome = if !absent.is_empty() {
        Outcome::Failed
    } else if uncreated {
        Outcome::Absent(sources::NOT_YET_CREATED)
    } else if let Some(why) = oracle_absent
        && matches!(runner, Runner::Engine(_))
    {
        // A binary is built and the pinned php is not here: the engine
        // could run, and the recipe it would run under is the pin's,
        // whose digest cannot be asserted. Nothing ran, and this is why;
        // `done` excuses the absent engine and not this.
        Outcome::OracleAbsent(why)
    } else {
        tally.outcome(runner)
    };
    let message = words(&scope, tally, &outcome, subset, uncreated, source, manifest);
    // Which engine wrote these numbers. The reading's own stamp is the
    // tree's, and one tree had two binaries under it and two readings 37
    // tests apart before this line, with nothing to tell them apart
    // (#475); the binary's path and the md5 of its bytes are what does.
    let message = match runner.stamp() {
        Some(engine) => format!("{message}; run by {engine}"),
        None => message,
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
    // The tests the compiler refused with its own parse error before any
    // test code ran: a skip of the compiler's, held to that cause alone,
    // and the one `done` meets beside `engine_absent` when it is every
    // test of the subset (harness §4.5 condition 1, #319).
    reading.count("unread", tally.count(Judgement::Unread))?;
    // Why nothing ran, where nothing did: the engine has no binary yet,
    // which is the one cause `done` excuses (#132). A subset of a crate
    // not yet created, and a subset whose every test skipped, are the
    // other two ways this lane skips and are not this number.
    reading.count(
        "engine_absent",
        u64::from(!uncreated && matches!(runner, Runner::Absent(_))),
    )?;
    // The other way nothing ran: a binary is built and the tree has moved
    // past it, so running it would measure an engine this tree no longer
    // describes. It is a skip of its own and not the absent binary `done`
    // excuses, since the engine is there to be measured once it is rebuilt
    // (#475).
    reading.count(
        "engine_stale",
        u64::from(!uncreated && matches!(runner, Runner::Stale(_))),
    )?;
    // The burndown's number: what met its expectation, over the tests the
    // subset holds, so a `[[week]]` bar can read one figure.
    reading
        .numbers
        .insert("percent".to_owned(), tally.percent().into());
    Ok(reading)
}

/// The reading's words: the rung's sentence over the subset's numbers,
/// or the directories the row names and the pinned tree has not.
fn words(
    scope: &str,
    tally: &Tally,
    outcome: &Outcome<'_>,
    subset: &Subset,
    uncreated: bool,
    source: &Path,
    manifest: &Manifest,
) -> String {
    let absent = subset.absent();
    if uncreated && absent.is_empty() {
        format!(
            "{scope}: {}; the subset holds {} tests",
            sources::NOT_YET_CREATED,
            tally.found
        )
    } else if absent.is_empty() {
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
                "{scope}: no test of the subset ran; {} found, {} skipped, {} unread by the compiler",
                tally.found,
                tally.count(Judgement::Skip),
                tally.count(Judgement::Unread)
            ),
            Outcome::OracleAbsent(why) => format!(
                "{scope}: no test of the subset ran, {} found; the recipe is the pinned oracle's and {why}",
                tally.found
            ),
            Outcome::Passed => format!(
                "{scope}: {} passed, {} skipped, {} unread by the compiler of {} tests",
                tally.count(Judgement::Pass),
                tally.count(Judgement::Skip),
                tally.count(Judgement::Unread),
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
            source.display(),
            manifest.pin.source.tag
        )
    }
}

/// What one `xtask phpt` run was asked for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    /// One subset by name; one of this and `full` is named.
    pub subset: Option<String>,
    /// The whole corpus: the nightly lane, denied at the call.
    pub full: bool,
    pub lane: Option<Lane>,
    /// The binary the process adapter runs; without it, the engine's own
    /// where cargo puts it.
    pub engine: Option<PathBuf>,
    /// Render the unread census beside the run and carry its groups on
    /// each reading (#617). The lane, the verdicts and every count are
    /// the same either way: this asks for one more rendering of what the
    /// run already reached, never for a second run.
    pub census: bool,
}

/// Run what was asked for: one reading per subset, each written by the
/// caller through `record`.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    // Where the tree is on this machine is the oracle module's one answer;
    // an absent tree refuses the run naming both places it looked.
    let source = manifest.pin.source.tree()?;
    let subsets = match &ask.subset {
        Some(name) => vec![Subset::resolve(manifest, &source, name)?],
        None if ask.full => Subset::corpus(&source),
        // The per-commit lane, composed from the `[[crate]]` rows that own
        // a subset (harness §7). The row is where the lane lives, so
        // neither this function nor the design's table is a second copy of
        // the other.
        None => {
            let lane = Subset::lane(manifest, &source);
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
            source.display()
        );
    }
    let runner = Runner::resolve(root, &manifest.pin, ask.engine.as_deref())?;
    // Nothing to run, or a binary the tree has moved past: either way the
    // lane says so before the subsets rather than at the reading alone.
    if let Runner::Absent(why) | Runner::Stale(why) = &runner {
        println!("phpt: {why}");
    }
    // One oracle for the run: the digest is asserted before the first
    // program and never per test, subset or section (harness §4.3). A
    // pinned php that is not on this machine is no refusal, as it is none
    // for `witness` and `crosscheck`: nothing runs, and each reading says
    // why.
    let oracle = manifest.pin.oracle();
    let pinned = &manifest.pin.oracle.path;
    let oracle_absent = if pinned.is_file() {
        None
    } else {
        Some(format!("the pinned php is not at {}", pinned.display()))
    };
    if let Some(why) = &oracle_absent
        && matches!(runner, Runner::Engine(_))
    {
        println!("phpt: nothing runs; {why}");
    }
    let under = Under {
        runner: &runner,
        lane: ask.lane,
        oracle_absent: oracle_absent.as_deref(),
    };
    let mut readings = Vec::new();
    // The lane's census, folded over the subsets as each is run, so the
    // widest construct of the whole corpus is one line at the end and not
    // a sum a reader does by hand.
    let mut lane = census::Census::default();
    for subset in &subsets {
        let tally = run_subset(&oracle, registry, subset, under)?;
        let mut reading = reading(&tally, root, manifest, &source, subset, under)?;
        if ask.census {
            let mut counted = census::Census::default();
            counted.add(&tally);
            counted.print(&subset.scope);
            for row in counted.rows() {
                reading.group(&row.construct, row.tests.len() as u64)?;
            }
            lane.add(&tally);
        }
        readings.push(reading);
    }
    // One subset's census is the census; a lane's is worth a second line
    // only where there was more than one subset to fold.
    if ask.census && subsets.len() > 1 {
        lane.print("the lane");
    }
    Ok(readings)
}

/// Run one subset: every test through the runner, each verdict printed as
/// it is reached unless it passed. Where `under` says the pinned php is
/// not on this machine, the tests are found and none runs, since the
/// digest the run asserts first is of a binary that is not there.
pub(crate) fn run_subset(
    oracle: &Oracle<'_>,
    registry: &Registry,
    subset: &Subset,
    under: Under<'_>,
) -> Result<Tally> {
    let Under {
        runner,
        lane,
        oracle_absent,
    } = under;
    let tests = subset.tests()?;
    let Some(binary) = runner.binary() else {
        return Ok(Tally::found(tests.len()));
    };
    if oracle_absent.is_some() {
        return Ok(Tally::found(tests.len()));
    }
    // A digest mismatch refuses the whole run (§4.3), before the first
    // test rather than at its first SKIPIF.
    oracle.asserted()?;
    let adapter = Adapter::new(oracle, registry, binary, lane);
    Tally::judge(&tests, |test| adapter.judge(subset, test))
}

#[cfg(test)]
mod tests;
