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
//! **Held to the target.** The runner's control keeps the oracle's
//! verdict on every test of a subset under the pin, and every engine's
//! reading of the subset carries met-of-target, `target`, `met` and
//! `outside`, with the target's misses named and `missed_skipped` counting
//! those the engine skipped (#1834), and `predated` where the target was
//! kept over another corpus than the subset holds now, with the command
//! that keeps it again (#1644); the `target` module says what is in it
//! (#1184).
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
//! holds its conflict keys among every run on the machine too, since a
//! port or a server is the machine's and not the run's copy of the tree,
//! and the reading counts the tests that waited (the `across` module,
//! #1313). A test past the recipe's limit holds up only its batch and the
//! tests it conflicts with. The verdicts are printed and kept in the
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
//! **What a test leaves of System V IPC.** A segment, a semaphore set or
//! a queue a test creates and leaves outlives its process, and the
//! kernel's tables are small; after each subset the objects the run's own
//! processes left are removed, told by the processes the kernel names on
//! them, and one that names none by its maker, its private key and the
//! second it was made in, inside one of the subset's programs' runs; the
//! reading counts them as `sysv_removed` (the `sysv` module, #1412,
//! #1482). On Linux a System V subset's programs run in a private IPC
//! namespace where `unshare` permits, and the run's line says so.
//!
//! **Which subsets a lane runs.** The `[[crate]]` row, and nothing here:
//! a crate owns the directories of the corpus its row names, an extension
//! owns every `.phpt` under `ext/<name>` by the design (#1590), and a
//! crate that owns none is asked for no phpt reading. A bare call is the
//! per-commit lane, which is every subset of every crate the binary under
//! test carries, in manifest order (harness §7, revision 63;
//! [`Carried`]); `--subset` is one crate's subsets or one directory of
//! the pinned tree.
//!
//! **What is not run here.** The full corpus, the nightly verdict control
//! and the tier's lanes are CI's, under the manifest's
//! `lanes.nightly-flags` rule; the per-commit lane and one subset are an agent's to run. A
//! `--PHPDBG--` test runs through the debugger host beside the binary, the
//! phpdbg `[sapi]` pins for the control and the engine's own `phpdbg`,
//! `sapi/phpdbg`'s binary, for the engine, as the `phpdbg` module has it
//! (#1304); a `--REDIRECTTEST--` runs
//! as the set it names, counted in its own subset (the `redirect` module,
//! #814); a CGI-shaped one runs through the CGI host beside the binary,
//! the php-cgi `[sapi]` pins for the control and the engine's own
//! `php-cgi`, `sapi/cgi`'s binary, for the engine, as the `cgi` module
//! has it (#1198, ADR-0008); and a test of `sapi/fpm/tests` finds the
//! engine's own `php-fpm` where upstream's `FPM\Tester` looks, as the
//! `fpm` module places it (#1291). Beyond those, the process adapter
//! gives a test run-tests.php's environment and pipes for its streams, the
//! `environment` module's and [`oracle::Streams::Pipes`] (#1186), and
//! never a terminal, so it sets `SKIP_IO_CAPTURE_TESTS` as upstream does
//! off one; a test that wants a terminal, an interactive shell or a
//! listening server is where the runner and upstream still part; the four
//! of `sapi/cli/tests` that do are named in harness-design.md §12 item 17.
//!
//! [`oracle::Streams::Pipes`]: crate::oracle::Streams::Pipes

mod adapter;
pub(crate) mod environment;
pub(crate) use environment::env_of;
mod across;
pub(crate) mod against;
pub(crate) mod cgi;
mod fpm;
pub(crate) mod phpdbg;
mod scope;
pub(crate) use scope::{Comparison, Scope};
mod census;
pub(crate) mod file;
pub(crate) mod miss;
mod redirect;
pub(crate) mod row;
pub(crate) mod server;
mod sysv;
pub(crate) mod target;

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::carried::{BINARY, Carried};
use crate::divergence::Registry;
use crate::manifest::{Crate, Manifest};
use crate::oracle::{Oracle, Pin, Runner, Source};
use crate::reading::{Reading, listed, plural};
use crate::scratch::Tree;
use crate::sources;
use crate::tally::{self, Outcome};
use adapter::Adapter;
use target::{Measure, Target};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "phpt";
/// The keys of a phpt reading's `numbers` that its rendering reads back,
/// beside the verdicts' ([`Judgement::key`]), the target's
/// ([`target::Measure`]) and the tally's `run` ([`tally::RUN`]): the run
/// writes each under its name here (#1745).
pub(crate) mod numbers {
    /// The tests the subset holds.
    pub(crate) const TESTS: &str = "tests";
    /// The corpus's files the subset holds (#1589).
    pub(crate) const FILES: &str = "files";
    /// 1 where nothing ran because the engine has no binary (#132).
    pub(crate) const ENGINE_ABSENT: &str = "engine_absent";
    /// 1 where nothing ran because the tree moved past the binary (#475).
    pub(crate) const ENGINE_STALE: &str = "engine_stale";
}
/// The first segment of a phpt divergence key, so a row names the corpus
/// and not the path this machine keeps it at.
pub(crate) const CORPUS: &str = "php-src";
/// The compiled tier's lane (jit-design.md §7): the engine runs with the
/// `jit.mode` directive set and the corpus's expectations must hold
/// unchanged under it, which is G5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
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

/// Where the corpus's subsets stand in the pinned tree: the one table the
/// nightly lane's corpus is walked by and CI checks the tree out by,
/// through `status --section corpus` (#1713), so the checkout is never
/// narrower than a subset the lane or a `[[crate]]` row holds. An
/// extension's place is the rule a row with no `phpt` key derives,
/// `ext/<name>` whole (#1590), and a host's is its `tests` directory.
pub(crate) const ROOTS: [Root; 4] = [
    Root::Dir("Zend/tests"),
    Root::Dir("tests"),
    Root::Each(crate::manifest::derived_subset),
    Root::Each(host_tests),
];

/// One row of [`ROOTS`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Root {
    /// One directory of the tree, one subset: `Zend/tests`.
    Dir(&'static str),
    /// One subset per directory a name stands for: the place the function
    /// gives the directory named, `ext/<name>` for `ext/*`.
    Each(fn(&str) -> PathBuf),
}

impl Root {
    /// The name that stands for every directory of an [`Root::Each`], one
    /// path segment as git's sparse patterns and the shell's globs read it.
    const ANY: &'static str = "*";

    /// The place of the subsets in the tree, with [`Root::ANY`] for the
    /// name of an [`Root::Each`]: `Zend/tests`, `ext/*`.
    fn place(self) -> PathBuf {
        match self {
            Root::Dir(dir) => PathBuf::from(dir),
            Root::Each(subset) => subset(Root::ANY),
        }
    }

    /// The root as one of git's non-cone sparse-checkout patterns: the
    /// directory anchored at the tree's top, everything under it taken.
    pub(crate) fn pattern(self) -> String {
        format!("/{}/", self.place().display())
    }

    /// The root's subsets in the tree at `source`, relative to it: the
    /// directory itself, or one per directory under the name's parent,
    /// sorted by name.
    fn subsets(self, source: &Path) -> Vec<PathBuf> {
        let subset = match self {
            Root::Dir(dir) => return vec![PathBuf::from(dir)],
            Root::Each(subset) => subset,
        };
        let parent: PathBuf = self
            .place()
            .iter()
            .take_while(|segment| *segment != Root::ANY)
            .collect();
        let Ok(entries) = fs::read_dir(source.join(parent)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names.iter().map(|name| subset(name)).collect()
    }
}

/// Whether the file at `path` is a test of the corpus: a `.phpt`.
fn is_phpt(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "phpt")
}

/// The subset of the host `name`: its `tests` directory, as the
/// `[[crate]]` rows of `sapi/*` name it.
fn host_tests(name: &str) -> PathBuf {
    Path::new("sapi").join(name).join("tests")
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

    /// Every subset of the corpus at `source`: the nightly lane's, one per
    /// directory [`ROOTS`] places in the tree, in the table's order, each
    /// where it holds a `.phpt`. Whether one does is read up to the first
    /// found, so naming the corpus costs no walk of its tests.
    pub(crate) fn corpus(source: &Path) -> Vec<Subset> {
        ROOTS
            .iter()
            .flat_map(|root| root.subsets(source))
            .map(|under| Subset {
                scope: under.to_string_lossy().into_owned(),
                dirs: vec![Dir::at(source, under)],
                root: source.to_path_buf(),
            })
            .filter(|subset| {
                subset
                    .dirs
                    .iter()
                    .any(|place| tally::any_file_under(&place.dir, is_phpt))
            })
            .collect()
    }

    /// Every `.phpt` under the subset's directories, sorted within each.
    pub(crate) fn tests(&self) -> Result<Vec<PathBuf>> {
        let mut all = Vec::new();
        for place in &self.dirs {
            let mut found = Vec::new();
            tally::files_under(&place.dir, &mut found)
                .with_context(|| format!("listing the subset at {}", place.dir.display()))?;
            found.retain(|path| is_phpt(path));
            found.sort();
            all.append(&mut found);
        }
        Ok(all)
    }

    /// The divergence key of one test: `phpt:php-src/ext/pcre/tests/x.phpt`,
    /// where the test stands in the corpus whichever directory of the
    /// subset it came from.
    pub(crate) fn id(&self, test: &Path) -> String {
        format!("phpt:{CORPUS}/{}", self.under(test).display())
    }

    /// Where the test stands in the corpus, whichever directory of the
    /// subset it came from.
    fn under(&self, test: &Path) -> PathBuf {
        match self.found_in(test) {
            Some(place) => place.under.join(place.inside(test)),
            None => PathBuf::from(self.name(test)),
        }
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
    pub(crate) fn scope(&self, runner: &Runner, lane: Option<Lane>) -> Scope {
        Scope {
            control: matches!(runner, Runner::Control(_)),
            lane,
            ..Scope::of(&self.scope)
        }
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
    /// Every verdict, in the order the reading counts them.
    pub(crate) const ALL: [Judgement; 7] = [
        Judgement::Pass,
        Judgement::Fail,
        Judgement::Skip,
        Judgement::Bork,
        Judgement::XFail,
        Judgement::Diverged,
        Judgement::Unread,
    ];

    /// Whether the test missed and keeps why: it fails the reading, or the
    /// compiler refused it. Each such verdict carries a [`miss::Miss`].
    pub(crate) fn misses(self) -> bool {
        matches!(self, Judgement::Fail | Judgement::Bork | Judgement::Unread)
    }

    /// The key of a reading's `numbers` that counts the verdict: the one
    /// place it is named, for the run that writes the count and the
    /// rendering that reads it back (#1745).
    pub(crate) fn key(self) -> &'static str {
        match self {
            Judgement::Pass => "passed",
            Judgement::Diverged => "diverged",
            Judgement::Fail => "failed",
            Judgement::Skip => "skipped",
            Judgement::Bork => "borked",
            Judgement::XFail => "xfailed",
            Judgement::Unread => "unread",
        }
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
    /// The `MySQL` server's `SELECT VERSION()`, for a subset that needs
    /// one (the `server` module); a runner handed such a subset without it
    /// refuses.
    pub served: Option<&'a str>,
    /// The directory the tests' conflict keys are held in as locks.
    pub locks: &'a Path,
}

impl<'a> Under<'a> {
    /// The same run, with the server that answered for one subset.
    fn serving(self, served: Option<&'a str>) -> Under<'a> {
        Under { served, ..self }
    }
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
    let scope = subset.scope(runner, lane).to_string();
    let absent = subset.absent();
    // A directory the design derives for an extension that the pinned tree
    // has not, or that holds no `.phpt`: an extension with no tests of its
    // own, whose reading is a skip naming the directory, and not the
    // manifest and the pin disagreeing, which a row that names its
    // directories is (#1231, #1598).
    let testless = manifest
        .crate_named(&subset.scope)
        .is_some_and(Crate::derives_subset)
        && (!absent.is_empty() || subset.tests()?.is_empty());
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
    let outcome = if testless {
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
    let message = match under.served {
        Some(version) => format!("{message}; served by MySQL {version}"),
        None => message,
    };
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
        Reading::for_scope(INSTRUMENT, &scope, manifest, root)?.under(&manifest.pin),
        &outcome,
        message,
    )?;
    reading.count(numbers::TESTS, tally.found as u64)?;
    // The corpus's files the subset holds, which are its tests but where a
    // `--REDIRECTTEST--` ran a set in its place: a PDO driver's reading
    // judges `ext/pdo/tests` beside its own, which `ext/pdo`'s reading holds
    // too, so a count of the lane sums this and not `tests` (#1589). A
    // subset whose directory the tree has not holds none.
    let files = if absent.is_empty() {
        subset.tests()?.len()
    } else {
        0
    };
    reading.count(numbers::FILES, files as u64)?;
    // A count per verdict. `unread` is the tests the compiler refused with
    // its own parse error before any test code ran: a skip of the
    // compiler's, held to that cause alone, and the one `done` meets
    // beside `engine_absent` when it is every test of the subset (harness
    // §4.5 condition 1, #319).
    for judgement in Judgement::ALL {
        reading.count(judgement.key(), tally.count(judgement))?;
    }
    // Why nothing ran, where nothing did: the engine has no binary yet,
    // which is the one cause `done` excuses (#132). A subset of a crate
    // not yet created, and a subset whose every test skipped, are the
    // other two ways this lane skips and are not this number.
    reading.count(
        numbers::ENGINE_ABSENT,
        u64::from(!uncreated && matches!(runner, Runner::Absent(_))),
    )?;
    // The other way nothing ran: a binary is built and the tree has moved
    // past it, so running it would measure an engine this tree no longer
    // describes. It is a skip of its own and not the absent binary `done`
    // excuses, since the engine is there to be measured once it is rebuilt
    // (#475).
    reading.count(
        numbers::ENGINE_STALE,
        u64::from(!uncreated && matches!(runner, Runner::Stale(_))),
    )?;
    // The burndown's number: what met its expectation, over the tests the
    // subset holds, so a `[[week]]` bar can read one figure.
    reading
        .numbers
        .insert("percent".to_owned(), tally.percent().into());
    Ok(reading)
}

/// The reading held to the target of the subset `scope` names (#1184). A
/// run of the engine that judged a test carries met-of-target, its numbers
/// `target`, `met`, `outside` and `missed_skipped` and the target's misses
/// named, and `predated` with the command that keeps the target again
/// where the kept target holds no verdict for some of the subset's tests
/// (#1644), or says how to take a target where none is kept under the
/// pin; the runner's control says it kept its verdicts as the target,
/// where it judged every test.
fn held_to(
    reading: &mut Reading,
    (tally, scope): (&Tally, &str),
    (target, kept): (Option<&Target>, bool),
    registry: &Registry,
    (runner, pin): (&Runner, &Pin),
) -> Result<()> {
    let key = target::key(pin);
    let keeps = format!(
        "`xtask phpt --subset {scope} --engine {}` keeps it again",
        pin.oracle.path.display()
    );
    let words = match (runner, target) {
        (Runner::Control(_), _) if kept => format!("kept as the subset's target under {key}"),
        (Runner::Engine(_) | Runner::Linked, Some(target)) if !tally.judged.is_empty() => {
            let verdicts = tally.judged.iter().map(|j| (j.id.as_str(), j.judgement));
            let measure = Measure::of(target, registry, verdicts);
            measure.write(reading)?;
            measure.words(&keeps)
        }
        (Runner::Engine(_) | Runner::Linked, None) if !tally.judged.is_empty() => format!(
            "no target is kept under {key}; `xtask phpt --subset <scope> --engine {}` keeps one",
            pin.oracle.path.display()
        ),
        _ => return Ok(()),
    };
    told(reading, runner, &words);
    Ok(())
}

/// `words` added to the reading's message before the stamp of the engine
/// that ran, which stays the message's last words.
fn told(reading: &mut Reading, runner: &Runner, words: &str) {
    let stamp = runner
        .stamp()
        .map(|engine| format!("; run by {engine}"))
        .unwrap_or_default();
    let said = reading
        .message
        .strip_suffix(&stamp)
        .unwrap_or(&reading.message);
    reading.message = format!("{said}; {words}{stamp}");
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
    // A derived subset the tree has but holds no `.phpt`: named as an
    // absent one is, by its directory.
    let empty = absent.is_empty() && matches!(outcome, Outcome::Absent(why) if *why == NO_TESTS);
    if uncreated && absent.is_empty() && !empty {
        format!(
            "{scope}: {}; the subset holds {} tests",
            sources::NOT_YET_CREATED,
            tally.found
        )
    } else if absent.is_empty() && !empty {
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
        let named: Vec<String> = if empty {
            subset
                .dirs
                .iter()
                .map(|place| place.under.to_string_lossy().into_owned())
                .collect()
        } else {
            absent
                .iter()
                .map(|under| under.to_string_lossy().into_owned())
                .collect()
        };
        let named: Vec<&str> = named.iter().map(String::as_str).collect();
        let (whose, why) = match outcome {
            Outcome::Absent(why) => ("the design derives", format!("; {why}")),
            _ => ("the [[crate]] row names", String::new()),
        };
        let has = if empty {
            "holds no .phpt under it"
        } else {
            "has no such directory"
        };
        format!(
            "{scope}: {whose} {}, and the pinned php-src at {} ({}) {has}{why}",
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
    /// The directory a test's conflict keys are held in as locks, where
    /// every run on the machine meets (#1313): the temporary directory
    /// unless named. A test of the harness names one of its own, so a real
    /// run beside it holding `all` makes it wait for nothing (#1675).
    pub locks: Option<PathBuf>,
}

/// Run what was asked for: one reading per subset, each written by the
/// caller through `record`; a subset refused is its own failing reading,
/// and the rest of the lane runs (#1612).
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    // Where the tree is on this machine is the oracle module's one answer;
    // an absent tree refuses the run naming both places it looked.
    let source = manifest.pin.source.tree()?;
    let subsets = asked(root, manifest, &source, ask)?;
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
    let locks = ask.locks.clone().unwrap_or_else(across::machine);
    let under = Under {
        runner: &runner,
        lane: ask.lane,
        oracle_absent: oracle_absent.as_deref(),
        jobs: ask.jobs.unwrap_or_else(tally::cores),
        served: None,
        locks: &locks,
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
    // One subset's readings: its own and the one holding it against a
    // commit. A subset refused, for a server that did not answer or for
    // any other cause, is that subset's failing reading, and the lane goes
    // on, so one missing service never takes the readings the subsets
    // before and after it wrote with it (#1612).
    let mut taken = |subset: &Subset, held_as: &Scope| -> Result<Vec<Reading>> {
        let scope = &held_as.to_string();
        let own = Own::of(&oracle, subset, scope, copy.is_some())?;
        let beside = own.pin.as_ref().map(|pin| oracle.beside(pin));
        let oracle = beside.as_ref().unwrap_or(&oracle);
        let under = under.serving(own.served.as_deref());
        let ((tally, waited), removed) = swept_around(oracle, scope, copy.is_some(), || {
            let within = copy.as_ref().map(|copy| subset.rooted(copy));
            run_subset(oracle, registry, within.as_ref().unwrap_or(subset), under)
        })?;
        let mut reading = reading(&tally, root, manifest, &source, subset, under)?;
        around(&mut reading, &runner, removed, waited)?;
        let (target, kept) =
            runs.target_of(&manifest.pin, &subset.scope, (&tally, &runner), ask.lane)?;
        let against = (target.as_ref(), kept);
        held_to(
            &mut reading,
            (&tally, &subset.scope),
            against,
            registry,
            (&runner, &manifest.pin),
        )?;
        let held = runs.after(
            manifest,
            held_as,
            (&tally, &runner),
            &reading,
            target.as_ref(),
        )?;
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
        Ok(std::iter::once(reading).chain(held).collect())
    };
    for subset in &subsets {
        let scope = subset.scope(&runner, ask.lane);
        match taken(subset, &scope) {
            Ok(taken) => readings.extend(taken),
            Err(why) => readings.push(refused(root, manifest, &scope.to_string(), &why)?),
        }
    }
    // One subset's census is the census; a lane's is worth a second line
    // only where there was more than one subset to fold.
    if ask.census && subsets.len() > 1 {
        lane.print("the lane");
    }
    Ok(readings)
}

/// The subsets `ask` names: one crate's or directory's, the whole corpus,
/// or the per-commit lane; none is a refusal naming the tree.
fn asked(root: &Path, manifest: &Manifest, source: &Path, ask: &Ask) -> Result<Vec<Subset>> {
    let subsets = match &ask.subset {
        Some(name) => vec![Subset::resolve(manifest, source, name)?],
        None if ask.full => Subset::corpus(source),
        // The per-commit lane, composed from the `[[crate]]` rows that own
        // a subset and that the binary under test carries (harness §7,
        // revision 63). The row and the binary's declaration are where the
        // lane lives, so neither this function nor the design's table is a
        // second copy of either.
        None => {
            let lane = Subset::lane(manifest, &Carried::binary(root)?, source);
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
    Ok(subsets)
}

/// The reading of a subset the lane could not run, a fail whose message
/// is the cause and whose `refused` counts it, so the lane's other
/// readings are written beside it and its exit says it (#1612).
fn refused(root: &Path, manifest: &Manifest, scope: &str, why: &anyhow::Error) -> Result<Reading> {
    let mut reading = Reading::for_scope(INSTRUMENT, scope, manifest, root)?.under(&manifest.pin);
    reading.status = crate::reading::Status::Fail;
    reading.message = format!("{why:#}");
    reading.count("refused", 1)?;
    Ok(reading)
}

/// What one subset runs with beside the run: the `MySQL` server's version
/// where it needs the server and its tests run (the `server` module), and
/// where it runs its programs otherwise than the run does, a pin of its
/// own beside the run's: a `MySQL` one in a database of its own on the
/// server, made here and dropped when this drops (#1546), and a System V
/// one in a namespace (#1482), each said on its own line.
struct Own<'o> {
    served: Option<String>,
    pin: Option<Pin>,
    _database: Option<server::Database<'o>>,
}

impl<'o> Own<'o> {
    fn of(oracle: &'o Oracle<'o>, subset: &Subset, scope: &str, runs: bool) -> Result<Own<'o>> {
        let served = server::served(oracle, subset, runs)?;
        let database = match served {
            Some(_) => Some(server::Database::made(oracle, &subset.scope)?),
            None => None,
        };
        let namespace = sysv::Namespace::of(&subset.scope).filter(|_| runs);
        let pin = (database.is_some() || namespace.is_some()).then(|| {
            let mut pin = oracle.pin().clone();
            if let Some(database) = &database {
                println!(
                    "phpt: {scope}: runs in a database of its own on the server, {}, dropped when it ends",
                    database.name
                );
                database.lay_over(&mut pin);
            }
            if let Some(namespace) = namespace {
                println!("phpt: {scope}: {namespace}");
                pin = pin.wrapped(namespace.0);
            }
            pin
        });
        Ok(Own {
            served,
            pin,
            _database: database,
        })
    }
}

/// One subset run by `run`, and where its tests run, what its processes
/// left of System V IPC removed after it, the line that says so printed:
/// the objects standing are read before it, and the processes spawned
/// before it forgotten (the `sysv` module, #1412). The count is none
/// where nothing was removed, and where the machine could not be read,
/// which the line printed says.
fn swept_around<T>(
    oracle: &Oracle<'_>,
    scope: &str,
    runs: bool,
    run: impl FnOnce() -> Result<T>,
) -> Result<(T, Option<u64>)> {
    if !runs {
        return Ok((run()?, None));
    }
    oracle.take_spawned();
    let before = sysv::Standing::now();
    let tally = run()?;
    let swept = before.and_then(|before| sysv::sweep(&before, &oracle.take_spawned()));
    let removed = match swept {
        Ok(swept) => {
            if swept.removed() > 0 || swept.unnamed > 0 {
                println!("phpt: {scope}: {swept}");
            }
            Some(swept.removed() as u64).filter(|n| *n > 0)
        }
        Err(why) => {
            println!("phpt: {scope}: what its tests left of System V IPC is not removed: {why:#}");
            None
        }
    };
    Ok((tally, removed))
}

/// What the run around a subset adds to its reading: the System V objects
/// its tests left and the run removed (the `sysv` module, #1412), and the
/// tests that waited for a key another run on the machine held, and for
/// how long, in its numbers and the tail of its message (the `across`
/// module, #1313); each only where there was any.
fn around(
    reading: &mut Reading,
    runner: &Runner,
    removed: Option<u64>,
    waited: across::Waited,
) -> Result<()> {
    if let Some(removed) = removed {
        reading.count("sysv_removed", removed)?;
    }
    if waited.tests == 0 {
        return Ok(());
    }
    reading.count("waited", waited.tests)?;
    reading.count("waited_ms", waited.for_ms)?;
    let words = format!(
        "{} test{} waited {:.1}s in all for a conflict key another run held",
        waited.tests,
        plural(usize::try_from(waited.tests).unwrap_or(usize::MAX)),
        Duration::from_millis(waited.for_ms).as_secs_f64()
    );
    told(reading, runner, &words);
    Ok(())
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
) -> Result<(Tally, across::Waited)> {
    let Under {
        runner,
        lane,
        oracle_absent,
        jobs,
        served,
        locks,
    } = under;
    let tests = subset.tests()?;
    let unrun = (Tally::found(tests.len()), across::Waited::default());
    let Some(binary) = runner.binary() else {
        return Ok(unrun);
    };
    if oracle_absent.is_some() {
        return Ok(unrun);
    }
    // A digest mismatch refuses the whole run (§4.3), before the first
    // test rather than at its first SKIPIF.
    oracle.asserted()?;
    if served.is_none() && server::needs(subset) {
        bail!(
            "{}: refused, not skipped: the subset needs a MySQL server and the run named none; {}",
            subset.scope,
            server::START
        );
    }
    let control = matches!(runner, Runner::Control(_));
    fpm::place(subset, fpm::host(oracle.pin(), binary, control).as_deref())?;
    let hosts = adapter::Hosts {
        cgi: cgi::host(oracle.pin(), binary, control),
        phpdbg: phpdbg::host(oracle.pin(), binary, control),
    };
    let adapter = Adapter::new(oracle, registry, binary, hosts, lane);
    let items = adapter.listed(subset, tests)?;
    let mut dirs = BTreeMap::new();
    let apart: Vec<_> = items
        .iter()
        .map(|item| adapter::apart(item, &mut dirs))
        .collect();
    // What each test holds among every run on the machine beside what the
    // run holds apart within itself (#1313).
    let held: Vec<(_, Vec<String>)> = items
        .iter()
        .map(|item| (item, adapter::held(item, &mut dirs)))
        .collect();
    let batch = adapter::batch(&apart, jobs);
    let waited = Mutex::new(across::Waited::default());
    let tally = Tally::judge_apart(&held, jobs, batch, &apart, |(item, keys)| {
        let (_held, wait) = across::hold(locks, keys)?;
        waited
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .add(wait);
        adapter.judge(subset, item)
    })?;
    let waited = waited.into_inner().unwrap_or_else(PoisonError::into_inner);
    Ok((tally, waited))
}

#[cfg(test)]
mod tests;
