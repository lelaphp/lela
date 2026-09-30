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
//! what makes the two comparable. Until `sapi/cli` is built there is no
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
//! **Why a test missed.** A test that fails, borks or that the compiler
//! refused keeps its first differing lines and a signature on its verdict,
//! decoded once where the verdict is made (the `miss` module), and the
//! kept run carries them; `--census` groups every miss of the run by that
//! signature, the widest first (#1185).
//!
//! **Held against a commit.** Every run the engine judged whole is kept
//! as its verdicts, and `--against <commit>` holds each subset to the run
//! kept at the commit, test by test, in one more reading per subset; the
//! `against` module says what keys a run and what gives no verdict (#664).
//!
//! **What one run is.** One process per test, on a pool of threads the
//! size of the machine's cores or of `--jobs`, and the tests held apart as
//! upstream's parallel runner holds them. Its workers are handed batches
//! of neighbours in the tests' order and run each batch one test after
//! another (run-tests.php:1537-1546, 1742-1744), so the tests of a batch
//! here are judged in sequence and the batches beside each other: two
//! neighbours that make one path relative to php-src's root, where every
//! program runs, do not meet (#1091). A batch is neighbours and not a
//! directory, as upstream's is, so its length is upstream's too, from the
//! pool's size. Two tests naming one key in their `--CONFLICTS--`, or in
//! their directory's `CONFLICTS` file, never run at once, and one naming
//! `all` runs with nothing beside it (run-tests.php:1341-1372). A test
//! past the recipe's limit holds up only its batch and the tests it
//! conflicts with. The verdicts are printed and kept in the
//! tests' order whatever order they finish in, so the lines, the reading
//! and the kept run are the ones a pool of one writes (#938).
//!
//! **Where a test writes.** Nowhere in the pinned tree, which is read
//! only (#1183). A run copies the files the pin holds, what git tracks in
//! a checkout, into a scratch tree of its own, and every subset of the run
//! stands there: each test's program and its `SKIPIF` and `CLEAN` sections
//! beside it, where upstream puts them, the working directory and
//! `TEST_PHP_SRCDIR` its root, so `__FILE__`, `__DIR__`, `{PWD}`, a
//! relative path and a relative include are run-tests.php's, and whatever
//! a test writes lands in the copy. The copy goes when the run ends, and a
//! run killed first leaves it to the next run's sweep (the `scratch`
//! module), never to a later test's bork. Two runs over one subset never
//! meet, each in its own copy; within a run a test's three paths are among
//! its keys, so two tests whose files would stand at one path never run at
//! once.
//!
//! **Which subsets a lane runs.** The `[[crate]]` row, and nothing here:
//! a crate owns the directories of the corpus its row names, an extension
//! owns `ext/<name>/tests` by the design, and a crate that owns none is
//! asked for no phpt reading. A bare call is the per-commit lane, which is
//! every subset of every crate the binary under test carries, in manifest
//! order (harness §7, revision 63; [`Carried`]); `--subset` is one crate's
//! subsets or one directory of the pinned tree.
//!
//! **What is not run here.** The full corpus, the nightly verdict control
//! and the tier's lanes are CI's, under the manifest's `lanes.nightly`
//! rule; the per-commit lane and one subset are an agent's to run. A
//! `--PHPDBG--` test and a `--REDIRECTTEST--` are skipped with their
//! reason; a CGI-shaped one runs through the CGI host beside the binary,
//! the php-cgi `[sapi]` pins for the control and the engine's own
//! `php-cgi`, `sapi/cgi`'s binary, for the engine, as the `cgi` module
//! has it (#1198, ADR-0008). Beyond those, the process adapter
//! gives a test one stdin and two streams captured as one, so a test that
//! wants a terminal on a stream it did not capture, an interactive shell
//! or a listening server is where the runner and upstream still part; the
//! four of `sapi/cli/tests` that do are named in harness-design.md §12
//! item 17.

mod adapter;
pub(crate) use adapter::env_of;
pub(crate) mod against;
pub(crate) mod cgi;
/// The suffix of a control run's scope: the subset under the oracle.
pub(crate) const CONTROL: &str = "@control";
mod census;
pub(crate) mod file;
pub(crate) mod miss;

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::carried::{BINARY, Carried};
use crate::divergence::Registry;
use crate::manifest::{Crate, Manifest};
use crate::oracle::{Oracle, Runner, Source};
use crate::reading::{Reading, listed};
use crate::scratch::Tree;
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
    /// The php-src tree the subset stands in: the pinned one, which it is
    /// resolved and read against, or the run's copy of it, which it runs
    /// in (#1183). The working directory every program of the subset runs
    /// in, as run-tests.php runs them from `TEST_PHP_SRCDIR`
    /// (run-tests.php:1173, #952).
    pub root: PathBuf,
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
                root: source.to_path_buf(),
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
            root: source.to_path_buf(),
        })
    }

    /// The same subset standing in the tree at `root`, a copy of the one
    /// it was resolved in: its directories at their places there, and its
    /// programs run from there.
    pub(crate) fn rooted(&self, root: &Path) -> Subset {
        Subset {
            scope: self.scope.clone(),
            dirs: self
                .dirs
                .iter()
                .map(|place| Dir::at(root, place.under.clone()))
                .collect(),
            root: root.to_path_buf(),
        }
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

    /// The per-commit lane (harness §7, revision 63): every subset of
    /// every crate the binary under test carries, in manifest order, under
    /// the pinned tree at `source`. The row is the statement of which
    /// subsets a crate owns and `carried` of which crates the binary has,
    /// each read in one place, so the lane is never a second copy of the
    /// design's table; a crate behind a feature the binary does not turn
    /// on is outside the lane and in the nightly one.
    pub(crate) fn lane(manifest: &Manifest, carried: &Carried, source: &Path) -> Vec<Subset> {
        manifest
            .crates
            .iter()
            .filter(|krate| carried.carries(krate))
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
                    root: source.to_path_buf(),
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
            scope.push_str(CONTROL);
        }
        if let Some(lane) = lane {
            scope.push('@');
            scope.push_str(lane.as_str());
        }
        scope
    }
}

/// What one test came to: upstream's outcomes less the two a leak checker
/// decides, and `Diverged`, which is the registry's answer. Spelled in a
/// kept run as [`tally::Judgement::as_str`] spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

impl Judgement {
    /// Whether the test missed and keeps why: it fails the reading, or the
    /// compiler refused it. Each such verdict carries a [`miss::Miss`].
    pub(crate) fn misses(self) -> bool {
        matches!(self, Judgement::Fail | Judgement::Bork | Judgement::Unread)
    }
}

impl tally::Judgement for Judgement {
    /// A fail, a bork or an unread test keeps its first differing lines
    /// and its signature on the run (#1185).
    type Miss = miss::Miss;
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
/// the judgement, the note, and what a miss keeps.
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
    /// How many tests run at once.
    pub jobs: NonZeroUsize,
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
        ..
    } = under;
    let scope = subset.scope(runner, lane);
    let absent = subset.absent();
    // A directory the design derives for an extension and the pinned tree
    // has not: an extension with no tests of its own, whose reading is a
    // skip naming the directory, and not the manifest and the pin
    // disagreeing, which a row that names its directories is (#1231).
    let testless = manifest
        .crate_named(&subset.scope)
        .is_some_and(Crate::derives_subset);
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
    let outcome = if !absent.is_empty() && testless {
        Outcome::Absent(NO_TESTS)
    } else if !absent.is_empty() {
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

/// Why an extension whose derived directory the pinned tree has not has
/// no reading but a skip (#1231).
const NO_TESTS: &str = "the extension has no tests of its own in php-src, so its subset is none";

/// The reading's words: the rung's sentence over the subset's numbers,
/// or the directories the subset names and the pinned tree has not.
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
        let (whose, why) = match outcome {
            Outcome::Absent(why) => ("the design derives", format!("; {why}")),
            _ => ("the [[crate]] row names", String::new()),
        };
        format!(
            "{scope}: {whose} {}, and the pinned php-src at {} ({}) has no such directory{why}",
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
    /// Render the census beside the run: every miss grouped by the
    /// signature it kept (#1185), and the unread constructs among them,
    /// whose groups each reading carries (#617). The lane, the verdicts
    /// and every count are the same either way: this asks for one more
    /// rendering of what the run already reached, never for a second run.
    pub census: bool,
    /// The commit whose kept run each subset is held to, test by test:
    /// one more reading per subset, at its scope with `@against` after it,
    /// failing while a test met there is not met here (#664).
    pub against: Option<String>,
    /// How many tests run at once; the machine's cores unless named.
    pub jobs: Option<NonZeroUsize>,
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
        // a subset and that the binary under test carries (harness §7,
        // revision 63). The row and the binary's declaration are where the
        // lane lives, so neither this function nor the design's table is a
        // second copy of either.
        None => {
            let lane = Subset::lane(manifest, &Carried::binary(root)?, &source);
            let scopes: Vec<&str> = lane.iter().map(|s| s.scope.as_str()).collect();
            println!(
                "phpt: the per-commit lane, {} subset{} the manifest names of what {BINARY} carries: {}",
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
        jobs: ask.jobs.unwrap_or_else(tally::cores),
    };
    // The run's own copy of the pinned tree, which every subset runs in and
    // which goes when the run does; none where no test will run.
    let copy = match (runner.binary(), &oracle_absent) {
        (Some(_), None) => Some(copy_of(&source, under.jobs)?),
        _ => None,
    };
    let mut readings = Vec::new();
    // The lane's census, folded over the subsets as each is run, so the
    // widest cause of the whole corpus is one line at the end and not a
    // sum a reader does by hand.
    let mut lane = census::Census::default();
    let runs = against::Runs::new(root, manifest, ask.against.as_deref())?;
    for subset in &subsets {
        let scope = subset.scope(&runner, ask.lane);
        if let Some(unheld) = runs.unheld(manifest, &scope)? {
            readings.push(unheld);
            continue;
        }
        let within = copy.as_ref().map(|copy| subset.rooted(copy));
        let tally = run_subset(&oracle, registry, within.as_ref().unwrap_or(subset), under)?;
        let mut reading = reading(&tally, root, manifest, &source, subset, under)?;
        let held = runs.after(manifest, &scope, &tally, &runner, &reading)?;
        if ask.census {
            let mut counted = census::Census::default();
            counted.add(census::misses(&tally));
            counted
                .accounts_for(census::verdicts(&tally))
                .map_err(|why| anyhow::anyhow!("{}: {why}", subset.scope))?;
            counted.print(&subset.scope);
            for row in counted.unread_rows() {
                reading.group(&row.signature.to_string(), row.tests.len() as u64)?;
            }
            lane.add(census::misses(&tally));
        }
        readings.push(reading);
        readings.extend(held);
    }
    // One subset's census is the census; a lane's is worth a second line
    // only where there was more than one subset to fold.
    if ask.census && subsets.len() > 1 {
        lane.print("the lane");
    }
    Ok(readings)
}

/// A copy of the pinned tree at `source` made for one run (#1183): the
/// files the pin holds ([`Source::files`]), each cloned where the file
/// system clones, on `jobs` threads, into a scratch tree the run owns. A
/// test run there writes nothing into the pinned tree, and a file some
/// earlier run left standing there is not in the copy to bork a test.
///
/// [`Source::files`]: crate::oracle::Source::files
fn copy_of(source: &Path, jobs: NonZeroUsize) -> Result<Tree> {
    let files = Source::files(source)?;
    let copy = Tree::new("phpt-tree")?;
    let each = files.len().div_ceil(jobs.get()).max(1);
    thread::scope(|scope| {
        let copying: Vec<_> = files
            .chunks(each)
            .map(|files| {
                let copy = copy.path();
                scope.spawn(move || {
                    files
                        .iter()
                        .try_for_each(|file| copy_one(&source.join(file), &copy.join(file)))
                })
            })
            .collect();
        copying.into_iter().try_for_each(|copying| {
            copying
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("copying the pinned tree panicked")))
        })
    })?;
    println!(
        "phpt: the run's copy of the pinned tree at {}, {} files, is {}",
        source.display(),
        files.len(),
        copy.display()
    );
    Ok(copy)
}

/// One file of the pinned tree into the run's copy: a link as a link, a
/// file through `fs::copy`, which clones it where the file system can.
/// A file the tree lists and no longer has is not in the tree.
fn copy_one(from: &Path, to: &Path) -> Result<()> {
    let kind = match fs::symlink_metadata(from) {
        Ok(meta) => meta.file_type(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", from.display())),
    };
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let copied = if kind.is_symlink() {
        fs::read_link(from).and_then(|target| std::os::unix::fs::symlink(target, to))
    } else if kind.is_dir() {
        fs::create_dir_all(to)
    } else {
        fs::copy(from, to).map(drop)
    };
    copied.with_context(|| format!("copying {} to {}", from.display(), to.display()))
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
        jobs,
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
    let control = matches!(runner, Runner::Control(_));
    let host = cgi::host(oracle.pin(), binary, control);
    let adapter = Adapter::new(oracle, registry, binary, host, lane);
    let mut dirs = BTreeMap::new();
    let apart: Vec<_> = tests
        .iter()
        .map(|test| adapter::apart(test, &mut dirs))
        .collect();
    let batch = adapter::batch(&apart, jobs);
    Tally::judge_apart(&tests, jobs, batch, &apart, |test| {
        adapter.judge(subset, test)
    })
}

#[cfg(test)]
mod tests;
