//! `xtask compile-identity`, op-array identity (design §6.1; harness-design.md
//! §4.3, §7). php prints its own op arrays before the optimizer runs, one
//! block per op array with the opline number, the operand the opline
//! writes into, the opcode, the operands with their kinds and literal
//! values, the jump targets and the live-range table; `Unit::dump` prints
//! the same format.
//! The lane compiles one corpus twice, once under the pinned oracle and
//! once under the engine, and holds every file to byte-identity through
//! [`oracle::compare`], so the registry is what decides whether a
//! difference is accepted, here as everywhere. A deliberate difference is
//! a row of kind `compile`, whose id is `compile:php-src/<path in the
//! corpus>`, and the target is zero rows.
//!
//! **What no row accepts.** The values each binary folds from its own
//! build, `PHP_BINARY` and `PHP_BUILD_DATE`, differ by construction and
//! are no divergence: the lane asks each binary for its own once per run
//! and puts a placeholder where exactly those values appear in that
//! binary's dump before comparing (#2692), so a compiler that folds any
//! value but its own still differs ([`dump`]).
//!
//! **What takes the dump.** php's side runs under the pin's recipe and
//! its `dump` regime, which is php's own recipe from §6.1, with `-l`
//! before the program so that a file compiles and does not run: a corpus
//! file's own bytes never reach the dump, and nothing of the corpus is
//! executed to measure the compiler. A run may name one more regime of
//! the pin with `--regime`, which both sides stand under after `dump`,
//! its arguments and its ini each after `dump`'s: `extended-info`, php's
//! `-e`, is the second regime the per-commit corpus is held under
//! (#1822), and its reading keeps a scope of its own,
//! `php-src@regime=extended-info`, beside the plain one. The engine's side is the ladder of
//! [`oracle::Runner`]: the engine's binary where one is named or built,
//! which honours php's dump flags; the harness links no compiler of its
//! own (harness §8, revision 84; #2166), so where none is built the lane
//! dumps nothing and its reading says why. The pinned oracle named as the
//! binary is the lane's control, which must be identical on every file,
//! and whose reading is a skip and not a reading of the engine.
//!
//! **What a corpus is.** A directory of files that are PHP: a `.phpt` of
//! the pinned tree, whose program is its `--FILE--` section, and a `.php`
//! beside it, which is a program already. The per-commit corpus is design
//! §6.1's, `Zend/tests`, `tests` and `sapi/cli` of the pinned php-src, and
//! the nightly whole is a corpus named with `--corpus`, which the
//! manifest's `lanes.nightly` rule denies at the call.
//!
//! **What is the tally's.** The verdicts, the print loop, the runner
//! ladder and the reading's status are [`tally`]'s, shared with `phpt` and
//! `witness`; this module contributes [`Judgement`], the per-file judge in
//! [`dump`] and the words of the corpus's message.

mod dump;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Oracle, Runner};
use crate::phpt::Scope;
use crate::reading::{Reading, Store, listed};
use crate::tally::{self, Judgement as _, Outcome};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "compile-identity";
/// The per-commit corpus's scope, and the first segment of its files'
/// divergence keys, so a row names the corpus and not the path this
/// machine keeps it at.
pub(crate) const CORPUS: &str = "php-src";
/// The kind of divergence a difference here is, and so the first word of
/// every key the lane compares under (harness §4.3).
pub(crate) const KIND: &str = "compile";
/// The `[regime.<name>]` of the pin both binaries dump under: php's
/// op-array dump of design §6.1. The ini is the pin's and is stated
/// nowhere here; a pin without the regime refuses the run naming it. The
/// name is public because it is what a caller hands the pin: the tests
/// that hold the lane before `oracle.toml` carries the regime name it
/// from here and never spell it again.
pub const REGIME: &str = "dump";
/// The flag before the program that compiles a file and does not run it.
pub(crate) const LINT: &str = "-l";
/// The per-commit corpus (design §6.1): every file of these three
/// directories of the pinned php-src, ≈ 6.5k. The design is the sentence
/// and this is its only reader; when a `[[crate]]` row or a lane row of
/// the manifest comes to own the corpus, it is read from there.
pub(crate) const PER_COMMIT: [&str; 3] = ["Zend/tests", "tests", "sapi/cli"];

/// One corpus the lane compiles: where its files are, and what a reading
/// of it is scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Corpus {
    /// The reading's scope: `php-src` for the per-commit corpus of the
    /// pinned tree, and the directory's own name for one named with
    /// `--corpus`.
    pub scope: String,
    /// The directory every file is named inside, on this machine.
    pub root: PathBuf,
    /// The directories under it the lane walks, in the order named.
    pub dirs: Vec<PathBuf>,
}

impl Corpus {
    /// The per-commit corpus at the pinned tree: design §6.1's three
    /// directories, named inside the tree.
    pub(crate) fn per_commit(tree: &Path) -> Corpus {
        Corpus {
            scope: CORPUS.to_owned(),
            root: tree.to_path_buf(),
            dirs: PER_COMMIT.iter().map(PathBuf::from).collect(),
        }
    }

    /// A corpus at a directory of its own: the nightly whole, and what a
    /// test lays down. Its scope is the directory's name.
    pub(crate) fn at(dir: &Path) -> Result<Corpus> {
        let scope = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .with_context(|| format!("{} names no directory", dir.display()))?;
        Ok(Corpus {
            scope,
            root: dir.to_path_buf(),
            dirs: vec![PathBuf::from(".")],
        })
    }

    /// The directories the corpus names that are not there, by their place
    /// inside it.
    pub(crate) fn absent(&self) -> Vec<&Path> {
        self.dirs
            .iter()
            .filter(|under| !self.root.join(under).is_dir())
            .map(PathBuf::as_path)
            .collect()
    }

    /// Every file of the corpus, sorted within each directory: the `.phpt`
    /// of the pinned tree and the `.php` beside them.
    pub(crate) fn files(&self) -> Result<Vec<PathBuf>> {
        let mut all = Vec::new();
        for under in &self.dirs {
            let dir = self.root.join(under);
            let mut found = Vec::new();
            tally::files_under(&dir, &mut found)
                .with_context(|| format!("listing the corpus at {}", dir.display()))?;
            found.retain(|path| path.extension().is_some_and(|e| e == "phpt" || e == "php"));
            found.sort();
            all.append(&mut found);
        }
        Ok(all)
    }

    /// Where the file stands inside the corpus, which is what a reading
    /// names it by and what its divergence key carries. A file that is not
    /// the corpus's is named by its own name alone: the path this machine
    /// keeps it at is no part of a key a person writes a row against.
    pub(crate) fn name(&self, file: &Path) -> String {
        match file.strip_prefix(&self.root) {
            Ok(inside) => inside.to_string_lossy().into_owned(),
            Err(_) => file
                .file_name()
                .unwrap_or(file.as_os_str())
                .to_string_lossy()
                .into_owned(),
        }
    }

    /// The divergence key of one file: `compile:php-src/Zend/tests/x.phpt`,
    /// so a row names the corpus and not this machine.
    pub(crate) fn id(&self, file: &Path) -> String {
        format!("{KIND}:{}/{}", self.scope, self.name(file))
    }

    /// The reading's scope: the corpus's, and what the run was beside it,
    /// so a control of the runner and a run under a regime beside `dump`
    /// each keep a reading of their own and never displace the engine's
    /// plain one.
    pub(crate) fn scope(&self, runner: &Runner, regime: Option<&str>) -> String {
        let scope = Scope {
            control: matches!(runner, Runner::Control(_)),
            regime: regime.map(str::to_owned),
            ..Scope::of(&self.scope)
        };
        scope.to_string()
    }
}

/// What one file of the corpus came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Judgement {
    /// The two dumps are the same bytes.
    Pass,
    /// They differ and an accepted registry row says so.
    Diverged,
    /// They differ and nothing accepts it.
    Fail,
    /// The file holds no program this lane can dump.
    Skip,
    /// The file is not one this lane can read at all.
    Bork,
}

impl tally::Judgement for Judgement {
    type Miss = std::convert::Infallible;
    const PASS: Judgement = Judgement::Pass;

    fn as_str(self) -> &'static str {
        match self {
            Judgement::Pass => "pass",
            Judgement::Diverged => "diverged",
            Judgement::Fail => "fail",
            Judgement::Skip => "skip",
            Judgement::Bork => "bork",
        }
    }

    /// Whether the corpus's reading fails on it.
    fn counts_against(self) -> bool {
        matches!(self, Judgement::Fail | Judgement::Bork)
    }

    fn met(self) -> bool {
        matches!(self, Judgement::Pass | Judgement::Diverged)
    }
}

/// One file's verdict: its divergence key, where it stands in the corpus
/// as its name, the judgement and the note.
pub(crate) type Judged = tally::Judged<Judgement>;

/// What one corpus's run came to.
pub(crate) type Tally = tally::Tally<Judgement>;

/// The corpus's reading: the counts in `numbers`, the files that differ
/// named in the message, and the status the ladder allows. The denominator
/// is in every message, whatever ran: what a person reads first of this
/// lane is how much of the corpus is identical, and a message that names
/// no denominator says nothing about the compiler's distance.
pub(crate) fn reading(
    tally: &Tally,
    snapshot: &Snapshot,
    corpus: &Corpus,
    runner: &Runner,
    regime: Option<&str>,
    oracle_absent: Option<&str>,
) -> Result<Reading> {
    let scope = corpus.scope(runner, regime);
    let absent = corpus.absent();
    let files = tally.found;
    // A directory the corpus names and this machine has not: no run can
    // settle it, and it is the reading ahead of whether an engine is
    // built, since building one would not answer it.
    let outcome = if !absent.is_empty() {
        Outcome::Failed
    } else if let Some(why) = oracle_absent {
        Outcome::Absent(why)
    } else {
        tally.outcome(runner)
    };
    let message = if absent.is_empty() {
        match &outcome {
            Outcome::Failed => format!(
                "{scope}: {} of {files} files differ from the oracle's dump{}",
                tally.against().len(),
                tally.missed()
            ),
            Outcome::Absent(why) => {
                format!("{scope}: no file of {files} dumped; {why}")
            }
            Outcome::Control(binary) => format!(
                "{scope}: {} of {files} differ under {}, the pinned oracle; a control of the lane, not a reading of the engine{}",
                tally.against().len(),
                binary.display(),
                tally.missed()
            ),
            Outcome::NothingRan => format!(
                "{scope}: no file of {files} was dumped on both sides; {} held no program to dump",
                tally.count(Judgement::Skip)
            ),
            // A rung this lane never asks for: an empty corpus is not a
            // measurement of the compiler. The words are here so that no
            // rung is without any.
            Outcome::OracleAbsent(why) => {
                format!("{scope}: nothing to dump and the pinned oracle is absent; {why}")
            }
            Outcome::Passed => format!(
                "{scope}: {} of {files} files dump byte-identical to the oracle's, {} accepted by the registry, {} skipped",
                tally.count(Judgement::Pass),
                tally.count(Judgement::Diverged),
                tally.count(Judgement::Skip)
            ),
        }
    } else {
        let named: Vec<String> = absent
            .iter()
            .map(|under| under.to_string_lossy().into_owned())
            .collect();
        format!(
            "{scope}: the corpus names {}, and {} has no such directory",
            listed(&named),
            corpus.root.display()
        )
    };
    // Which binary dumped it, as `phpt`'s reading names the one that ran
    // (#475, #506): two readings over one tree under two binaries are
    // told apart by it. A rung with no binary of the engine's names none.
    let message = match runner.stamp() {
        Some(engine) => format!("{message}; dumped by {engine}"),
        None => message,
    };
    // The corpus has no sources of its own in the workspace: what the
    // dump under measurement is a function of is the whole engine, so the
    // reading is stamped over the workspace.
    let mut reading = tally.reading(
        Reading::stamped_over(INSTRUMENT, &scope, snapshot, Path::new("."))?,
        &outcome,
        message,
    )?;
    reading.count("files", files as u64)?;
    reading.count("identical", tally.count(Judgement::Pass))?;
    reading.count("differ", tally.count(Judgement::Fail))?;
    reading.count("skipped", tally.count(Judgement::Skip))?;
    reading.count("borked", tally.count(Judgement::Bork))?;
    reading.count("diverged", tally.count(Judgement::Diverged))?;
    reading
        .numbers
        .insert("percent".to_owned(), tally.percent().into());
    Ok(reading)
}

/// What one `xtask compile-identity` run was asked for, flattened onto the
/// command line as it is.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Ask {
    /// One file, on request: its verdict is printed, a difference is the
    /// exit, and nothing is recorded.
    #[arg(long, value_name = "PATH", conflicts_with = "corpus")]
    pub file: Option<PathBuf>,
    /// A corpus at a directory of its own: the nightly whole, which CI
    /// runs.
    #[arg(long, value_name = "DIR")]
    pub corpus: Option<PathBuf>,
    /// The binary whose dump is held to the oracle's; without it, the
    /// engine's own where cargo puts it.
    #[arg(long, value_name = "PATH")]
    pub engine: Option<PathBuf>,
    /// A `[regime.<name>]` of the pin both sides dump under after `dump`:
    /// `extended-info`, php's `-e`, is the per-commit corpus's second
    /// (#1822). The reading's scope carries it, `php-src@regime=<name>`.
    #[arg(long, value_name = "NAME")]
    pub regime: Option<String>,
}

impl Ask {
    /// The regimes of the pin both sides dump under, in order: `dump`,
    /// and the one asked for after it.
    fn regimes(&self) -> Vec<&str> {
        std::iter::once(REGIME)
            .chain(self.regime.as_deref())
            .collect()
    }
}

/// `xtask compile-identity`: [`run`] under the tracked registry, its
/// readings recorded.
pub(crate) fn command(root: &Path, manifest: &Manifest, store: &Store, ask: &Ask) -> Result<()> {
    let registry = Registry::load(root)?;
    crate::record(store, &run(root, manifest, &registry, ask)?)
}

/// Run what was asked for: the reading of the corpus, written by the
/// caller through `record`, and nothing for one file, whose verdict is
/// the exit.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    // The regime is the pin's and the lane states no ini of its own; a pin
    // without it refuses the run before anything is dumped, naming the
    // block and the file a person writes it in.
    manifest.pin.regime(REGIME).with_context(|| {
        format!(
            "{INSTRUMENT}: the recipe php dumps its op arrays under is `[regime.{REGIME}]` of {} (design §6.1), and the pin moves only by a person's hand",
            manifest.oracle.pin
        )
    })?;
    if let Some(name) = &ask.regime {
        manifest
            .pin
            .regime(name)
            .with_context(|| format!("{INSTRUMENT}: `--regime {name}`"))?;
    }
    let regimes = ask.regimes();
    // The tree before anything is dumped: what the reading is stamped
    // with, so an edit while the corpus runs stales it.
    let snapshot = Snapshot::disk(root)?;
    // The engine's binary, built from the tree the reading is stamped
    // with; one the tree has moved past, or none, dumps nothing, and the
    // lane says why (#475, #2166).
    let runner = Runner::resolve(root, &manifest.pin, ask.engine.as_deref())?;
    if let Runner::Absent(why) | Runner::Stale(why) = &runner {
        println!("{INSTRUMENT}: {why}");
    }
    // One oracle for the run: the digest is asserted before the first file
    // and never per file (harness §4.3). A binary that is not at the pin's
    // path refuses nothing: nothing is dumped, every file is counted as
    // not dumped, and the reading says why.
    let oracle = manifest.pin.oracle();
    let oracle_absent = manifest.pin.oracle.absent();
    if oracle_absent.is_none() {
        oracle.asserted()?;
    }
    if let Some(file) = &ask.file {
        let corpus = around(manifest, file)?;
        return one(
            &oracle,
            registry,
            &runner,
            &regimes,
            oracle_absent.as_deref(),
            &corpus,
            file,
        )
        .map(|()| Vec::new());
    }
    let corpus = match &ask.corpus {
        Some(dir) => Corpus::at(dir)?,
        None => Corpus::per_commit(&manifest.pin.source.tree()?),
    };
    let tally = judge(
        &oracle,
        registry,
        &runner,
        &regimes,
        oracle_absent.as_deref(),
        &corpus,
    )?;
    Ok(vec![reading(
        &tally,
        &snapshot,
        &corpus,
        &runner,
        ask.regime.as_deref(),
        oracle_absent.as_deref(),
    )?])
}

/// Every file of the corpus through the two dumps, or none of them when
/// nothing is there to dump one side: then each file is counted as found
/// and not run, which is the denominator the reading states.
pub(crate) fn judge(
    oracle: &Oracle<'_>,
    registry: &Registry,
    runner: &Runner,
    regimes: &[&str],
    oracle_absent: Option<&str>,
    corpus: &Corpus,
) -> Result<Tally> {
    let files = corpus.files()?;
    let (Some(compiler), None) = (runner.binary(), oracle_absent) else {
        return Ok(Tally::found(files.len()));
    };
    let dumps = dump::Dumps::new(oracle, registry, compiler, regimes)?;
    Tally::judge(&files, |file| dumps.judge(corpus, file))
}

/// The corpus one file on request is named inside: the pinned tree's,
/// when the file stands in it, so the key a difference is keyed by is the
/// key a run of the corpus would give it; otherwise the directory the
/// file stands in, which is a corpus of the caller's own.
fn around(manifest: &Manifest, file: &Path) -> Result<Corpus> {
    if let Ok(tree) = manifest.pin.source.tree()
        && file.starts_with(&tree)
    {
        return Ok(Corpus::per_commit(&tree));
    }
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Corpus::at(&dir)
}

/// One file, on request: the verdict printed as the corpus's print loop
/// prints it, and a difference as the exit. Nothing is recorded, since one
/// file is a look and not a measurement of a corpus.
fn one(
    oracle: &Oracle<'_>,
    registry: &Registry,
    runner: &Runner,
    regimes: &[&str],
    oracle_absent: Option<&str>,
    corpus: &Corpus,
    file: &Path,
) -> Result<()> {
    if let Some(why) = oracle_absent {
        bail!("{INSTRUMENT}: {why}");
    }
    let Some(compiler) = runner.binary() else {
        bail!("{INSTRUMENT}: there is nothing to dump the file with; build the engine's binary");
    };
    let dumps = dump::Dumps::new(oracle, registry, compiler, regimes)?;
    let judged = dumps.judge(corpus, file)?;
    println!(
        "{} {}: {}",
        judged.judgement.as_str(),
        judged.id,
        judged.note
    );
    if judged.judgement.counts_against() {
        bail!("{INSTRUMENT}: {} is not the oracle's dump", judged.name);
    }
    Ok(())
}
