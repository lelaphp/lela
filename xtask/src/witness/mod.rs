//! `xtask witness`, the witness sensor (design §8; harness-design.md §3,
//! §4.3, §4.5 conditions 1 and 2, §7, §8). Every `php` fence and its
//! `expect` fence in a crate's rustdoc is extracted, run under the oracle's
//! recipe as `witness.php`, and held to what the fence pins: the first
//! stream, the second stream and the status, through [`oracle::verdict`],
//! so the divergence registry is what decides whether a difference is
//! accepted, here as everywhere. One reading per crate, of instrument
//! `witness`, with `run`, `failed` and `missing` in `numbers`, which is
//! what `done` reads as its conditions 1 and 2.
//!
//! **What runs the snippet.** The engine linked into this process,
//! [`oracle::linked`], one fresh engine per snippet over the `Capture`
//! sink, and never the engine's own binary (harness §7, revision 48;
//! #401). The seam `phpt` names (design §8's `ScriptExec`) has two
//! adapters and this lane stands on one of them by decision: the linked
//! engine carries every extension feature (revision 43, #314) where
//! `target/*/php` is php's default build, so under a lane that resolved a
//! binary the same fence beside `ext/curl` read pass or fail by which rung
//! happened to run, which is a reading that says nothing. A witness is a
//! snippet served in this process beside the code it exercises; the
//! binary's job is the corpus and the CLI's own behaviour, which is
//! `phpt`'s, `compile-identity`'s and `crosscheck`'s, and they keep it.
//! What it gives up: a witness never crosses the process boundary, so a
//! defect that appears only through the binary's startup is not a
//! witness's to catch.
//!
//! **The one fence it does not serve: one marked `process`.** A snippet that forks,
//! sends or takes a signal, or sets the process's ids, session or limits
//! is served by the `php` binary in a process of its own ([`process`];
//! harness §8, revision 61; #1062): pcntl's fences, posix's that change the process, and the
//! engine's that send one a signal. So no witness forks this process, and
//! the linked engine does not carry pcntl at all, so a fence that would
//! fork here and is not marked fails on the undefined function rather
//! than forking the harness. Coverage is observed the same way: the
//! binary appends what each request entered to a file the lane names. `phpt` has no second adapter and keeps its own rung
//! in `done`, since nothing stands in for a corpus (#132). The oracle runs
//! beside the linked engine and is held to the fences by the same rule,
//! because what a fence pins is the oracle's bytes: a fence the oracle
//! does not meet fails and says so, whatever the engine wrote (#783), and
//! the oracle's status is what `--accept` captures. A reading with nothing
//! judged, `run == 0`, is the run the pinned oracle was absent for, and is
//! a skip that says so.
//!
//! **A host's own lines** ([`host`], #1975). A host crate's
//! `tests/lines.toml` holds the command lines no fence can carry, each run
//! under the pinned host of its kind and then the engine's, and held
//! equal; they are judged among the crate's witnesses, and one whose
//! pinned host is absent or drifted fails rather than skips.
//!
//! **Which process serves it.** Not the one the command line started:
//! that one starts this binary again over the crates the run is for and
//! watches it, so a run that dies on a signal still leaves a failing
//! reading naming the signal and the snippets in flight ([`watch`]; #1037).
//! The engine is the same linked one either way.
//!
//! **What a feature row reads.** Nothing here. The bundle the harness
//! links carries every extension feature `php_bundle` declares, so a crate
//! the manifest marks `feature =` is held to its witnesses under
//! [`Runner::Linked`] as any other crate is, pass or fail on its merits
//! (harness §7, revision 43; #314). The rows name what stands outside
//! php's default build, which is the binary's and the phpt lane's concern:
//! a witness is a claim about a kernel or native, and a claim the harness
//! cannot run is not a reading. The skip #292 gave the lane, `unlinked`,
//! went with the reason for it; a reading carrying that number is the
//! check revision 43 names, failing. A fence marked `process` of such a
//! crate is served by `php` built with its feature where the binary under
//! test does not turn it on, as the phpt lane runs its subset (#2721).
//!
//! **What `--accept` writes.** The `expect` fence line, and nothing else:
//! the oracle's status in place of a bare `exit` or a stale one, so the
//! status is a function of snippet and oracle, captured and never authored.
//! The manifest's `witness.exit-authored` rule denies the other path at the
//! call, and this is why: nothing an agent writes can put a number
//! there. Read the diff it produces.
//!
//! **What `--accept --streams` writes** (harness §8, #801). The same, and
//! into each fence marked `capture` the oracle's two streams after the
//! recipe's normalisation, the `expect` fence's body and a `stderr`
//! fence's; a fence not marked is left byte for byte. The oracle runs the
//! snippet again from a scratch directory apart
//! ([`oracle::Place::Apart`]), and a fence whose two runs differ is not
//! written: the line that differs is named and the fence is `unpinned`
//! until its author writes that line with a wildcard by hand, since the
//! capture never invents one. What it wrote is printed fence by fence,
//! and held as any fence is.
//!
//! **What coverage is.** The manifest's `witness.coverage` rule names the
//! scope, `crates/php_engine/src/ops/**` and `ext/*/src/**`; a kernel or a
//! native there without a witness raises `missing`, and `done` prints it
//! per crate. What counts as one is the attribute that declares it, in a
//! crate whose natives stand on the attributes ([`anchor`]), and
//! [`fence::Subject`]'s sentence in an extension whose natives do not yet;
//! a crate outside `ext/` with no native is counted by its kernels alone.
//!
//! **What is the tally's.** The verdicts, the print loop, the runner
//! ladder and the reading's status are [`tally`]'s, shared with `phpt`;
//! this module contributes [`Judgement`], the per-item judge in `judge`
//! and the words of the crate's message. A crate whose sources cannot be
//! read is a failing reading naming the crate and the file, never a crate
//! quietly skipped, as a phpt subset the pinned tree has not is.
//!
//! **What `wall=D` is.** Disclosure. The declared durations are summed into
//! the reading's `wall_ms` and stop nothing; the limit a run has is the
//! recipe's.
//!
//! **What `regime=<name>` is.** One `[regime.<name>]` of the pin, whose
//! ini stands after the recipe's for the oracle run and for the linked
//! engine alike (harness §8, ledger 55): the way a witness pins a
//! behaviour the recipe's own ini holds off. The values are the pin's and
//! a person writes them, so a fence carries the name alone; a name the pin
//! does not carry is read before anything runs, and the block is one the
//! lane cannot run, named beside the regimes the pin has. A regime's
//! `args` stand after the recipe's on a binary's command line (#1780), the
//! oracle's and, for a fence marked `process`, the engine's; the linked
//! engine and the request hosts take none, so any other fence under such
//! a regime is a block the lane cannot run, read before anything runs as
//! an unknown name is.
//!
//! **What `locale=<name>` is.** A locale the snippet needs the host to
//! know, as a `.phpt`'s `--SKIPIF--` asks `setlocale` for one (#1678).
//! Where neither the oracle nor the linked engine can move `LC_ALL` to it,
//! the witness is not run: it is a skip that names the locale, counted as
//! `skipped` in the reading, and neither a pass nor a fail.

mod anchor;
pub(crate) mod fence;
mod fpm;
mod host;
mod judge;
pub(crate) mod makala;
mod process;
pub(crate) mod row;
#[cfg(test)]
#[cfg(tracked)]
mod tests;
mod watch;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::carried::Carried;
use crate::cfg::Target;
use crate::divergence::Registry;
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest, Rule};
use crate::oracle::{Linked, Oracle, Oracles, Pin, Regime, Runner};
use crate::reading::{Reading, Status, Store, listed, millis};
use crate::scratch::Full;
use crate::sources;
use crate::tally::{self, Outcome};
use anchor::Record;
use fence::Extracted;
pub(crate) use host::{ATTEMPTS, free_port};
pub(crate) use process::{Binary, host_of, served_as, under_recipe};
use watch::{Journal, Watch};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "witness";

/// The keys of a witness reading's counts beside the tally's `run`, named
/// once for the lane that writes them and the rendering that reads those it
/// renders.
pub(crate) mod numbers {
    /// Witnesses that met their pinned verdict.
    pub(crate) const PASSED: &str = "passed";
    /// Witnesses that count against the crate.
    pub(crate) const FAILED: &str = "failed";
    /// Witnesses that differ under an accepted registry row.
    pub(crate) const DIVERGED: &str = "diverged";
    /// Witnesses a `trace:` row covers, held to the engine alone (#1650).
    pub(crate) const COVERED: &str = "covered";
    /// Witnesses with no verdict pinned yet.
    pub(crate) const UNPINNED: &str = "unpinned";
    /// Witnesses the lane skipped.
    pub(crate) const SKIPPED: &str = "skipped";
    /// The kernels and natives the coverage scope holds.
    pub(crate) const SUBJECTS: &str = "subjects";
    /// Those of them without a witness.
    pub(crate) const MISSING: &str = "missing";
    /// The fence lines `--accept` wrote.
    pub(crate) const ACCEPTED: &str = "accepted";
    /// The blocks the fence grammar could not read.
    pub(crate) const UNREAD: &str = "unread";
    /// Witnesses found and not run, the oracle not here to run them.
    pub(crate) const NOT_RUN: &str = "not_run";
    /// Witnesses the engine did not serve, each standing as one the
    /// oracle's run failed does (#1535).
    pub(crate) const UNSERVED: &str = "unserved";
    /// The sum of the witnesses' declared durations, in milliseconds.
    pub(crate) const WALL_MS: &str = "wall_ms";
}
/// What the manifest's coverage rule names in its `check` predicate.
pub(crate) const COVERAGE: &str = "witness-coverage";

/// What one witness came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Judgement {
    Pass,
    /// The bytes differ and an accepted registry row says so.
    Diverged,
    /// A fence an accepted `trace:` row covers, which the engine met: it
    /// is held to its pinned `expect` against the engine alone, and the
    /// note records how the oracle's run ended, not judged (harness §4.3,
    /// §8; #1650).
    Covered,
    /// The bytes differ and nothing accepts it.
    Fail,
    /// A bare `exit`: the run has no status to hold the snippet to until
    /// `--accept` captures the oracle's.
    Unpinned,
    /// The witness could not be run or read at all.
    Bork,
    /// A `locale=<name>` the host knows to neither run: not run, counted,
    /// and neither a pass nor a fail (#1678).
    Skipped,
}

impl tally::Judgement for Judgement {
    type Miss = std::convert::Infallible;
    const PASS: Judgement = Judgement::Pass;

    fn as_str(self) -> &'static str {
        match self {
            Judgement::Pass => "pass",
            Judgement::Diverged => "diverged",
            Judgement::Covered => "covered",
            Judgement::Fail => "fail",
            Judgement::Unpinned => "unpinned",
            Judgement::Bork => "bork",
            Judgement::Skipped => "skip",
        }
    }

    /// Whether the crate's reading fails on it.
    fn counts_against(self) -> bool {
        matches!(
            self,
            Judgement::Fail | Judgement::Unpinned | Judgement::Bork
        )
    }

    fn met(self) -> bool {
        matches!(
            self,
            Judgement::Pass | Judgement::Diverged | Judgement::Covered
        )
    }
}

/// One witness's verdict: its divergence key, where it stands as its
/// name, the judgement and the note.
pub(crate) type Judged = tally::Judged<Judgement>;

/// The one thing `--accept` writes: a block's fence lines, by the file and
/// the line the first stands on, the `expect` fence's line alone for a
/// status and the fences whole for the streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Captured {
    pub file: PathBuf,
    /// 1-based.
    pub line: usize,
    pub was: Vec<String>,
    pub now: Vec<String>,
}

/// What one crate's run came to: the witnesses judged, and the coverage
/// half beside them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tally {
    pub witnesses: tally::Tally<Judgement>,
    /// The kernels and natives the coverage scope holds.
    pub subjects: usize,
    /// Those of them without a witness, as `file:line item`.
    pub missing: Vec<String>,
    /// The blocks the fence grammar could not read.
    pub problems: Vec<String>,
    /// The sum of the declared durations, disclosure only.
    pub wall: Duration,
    /// The fence lines `--accept` wrote.
    pub captured: Vec<Captured>,
    /// Where the pinned php is not, when it is not on this machine: then
    /// no witness ran, each is counted as not run, and a complete tally's
    /// green would be a claim. A binary at the path whose digest is not
    /// the pin's refuses the run instead (§4.3).
    pub oracle_absent: Option<String>,
    /// The fences the engine did not serve where the oracle is absent,
    /// each as `at: trouble`: each stands as [`Record::Unserved`], as one
    /// the oracle's run failed does, so its natives are no witness short,
    /// and the reading names why it did not serve (#1535).
    pub unserved: Vec<String>,
}

impl Tally {
    /// Whether the crate's reading fails: a witness that missed, a block
    /// the grammar could not read, or a kernel or native without one.
    pub(crate) fn failed(&self) -> bool {
        !self.witnesses.against().is_empty()
            || !self.problems.is_empty()
            || !self.missing.is_empty()
    }
}

/// The crate's reading: the counts in `numbers`, what failed named in the
/// message, and the status the ladder allows. Coverage is judged first and
/// needs no php: a kernel or native without a witness fails the crate
/// whether or not the oracle is here. A witness that missed fails the
/// crate whatever ran it, since what a fence pins is the oracle's and the
/// oracle ran. A crate where nothing ran, no witness or witnesses the
/// absent oracle could not run, is complete on coverage, and green there
/// only while the pinned oracle is here: without it the pass would be a
/// claim, and the reading is a skip that counts what did not run (harness
/// §4.3; #60, #65).
pub(crate) fn reading(
    tally: &Tally,
    snapshot: &Snapshot,
    krate: &Crate,
    rule: Option<&Rule>,
) -> Result<Reading> {
    let name = &krate.name;
    let run = &tally.witnesses;
    let outcome = if tally.failed() {
        Outcome::Failed
    } else if run.judged.is_empty() {
        run.complete(tally.oracle_absent.as_deref())
    } else {
        // The ladder's one rung here: the lane is the linked engine, so
        // the reading is a pass or a fail on what the engine wrote.
        run.outcome(&Runner::Linked)
    };
    let unrun = tail(tally);
    let message = match &outcome {
        Outcome::Failed => {
            let uncovered = if tally.missing.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} of {} kernels or natives without a witness: {}. {}",
                    tally.missing.len(),
                    tally.subjects,
                    listed(&tally.missing),
                    rule.map(|r| r.text.as_str()).unwrap_or_default()
                )
            };
            let unread = if tally.problems.is_empty() {
                String::new()
            } else {
                format!("; {}", tally.problems.join("; "))
            };
            format!(
                "{name}: {} of {} witnesses missed what they pin{}{uncovered}{unread}{unrun}",
                run.against().len(),
                run.judged.len(),
                run.missed()
            )
        }
        Outcome::Passed if run.judged.is_empty() => {
            format!("{name}: no witness, and no kernel or native that needs one")
        }
        Outcome::OracleAbsent(why) if run.found == 0 => format!(
            "{name}: coverage complete, oracle absent; no witness, and no kernel or native that needs one, and a green line without the pinned oracle is a claim; {why}"
        ),
        Outcome::OracleAbsent(_) => format!(
            "{name}: coverage complete, oracle absent; every one of {} kernels or natives has a witness, and a green line without the pinned oracle is a claim{unrun}",
            tally.subjects
        ),
        Outcome::Passed => format!(
            "{name}: {} witnesses pass over {} kernels or natives{unrun}",
            run.met(),
            tally.subjects
        ),
        // The ladder's two rungs for a lane that resolves a binary, which
        // this one does not (harness §7, revision 48): the words are here
        // so the match is over every rung, and a reading that carries them
        // is this lane standing on a runner it never chooses.
        Outcome::Control(binary) => format!(
            "{name}: the witness lane serves every snippet through the linked engine and chose {}, the pinned oracle; it resolves no binary",
            binary.display()
        ),
        Outcome::Absent(why) => format!(
            "{name}: the witness lane serves every snippet through the linked engine and chose a runner that is absent; {why}"
        ),
        // A witness that met nothing counts against the crate, so this
        // rung is one the ladder has and this lane's pre-rung reaches
        // first; the words are here so no rung is without any.
        Outcome::NothingRan => format!("{name}: no witness ran of {} found{unrun}", run.found),
    };
    // Over the snapshot the run took after any `--accept` it made, since
    // that writes the sources the stamp is over.
    let mut reading = run.reading(under_rule(krate, snapshot, rule)?, &outcome, message)?;
    reading.count(numbers::PASSED, run.count(Judgement::Pass))?;
    reading.count(numbers::FAILED, run.against().len() as u64)?;
    reading.count(numbers::DIVERGED, run.count(Judgement::Diverged))?;
    reading.count(numbers::COVERED, run.count(Judgement::Covered))?;
    reading.count(numbers::UNPINNED, run.count(Judgement::Unpinned))?;
    reading.count(numbers::SKIPPED, run.count(Judgement::Skipped))?;
    reading.count(numbers::SUBJECTS, tally.subjects as u64)?;
    reading.count(numbers::MISSING, tally.missing.len() as u64)?;
    reading.count(numbers::ACCEPTED, tally.captured.len() as u64)?;
    reading.count(numbers::UNREAD, tally.problems.len() as u64)?;
    reading.count(numbers::NOT_RUN, run.not_run())?;
    reading.count(numbers::UNSERVED, tally.unserved.len() as u64)?;
    reading.count(numbers::WALL_MS, millis(tally.wall))?;
    Ok(reading)
}

/// What the reading's message says after its verdict: the witnesses that
/// did not run for want of the oracle, the fences the engine did not
/// serve, those skipped for a locale the host lacks, and what the oracle
/// did on each fence a `trace:` row holds to the engine alone.
fn tail(tally: &Tally) -> String {
    let run = &tally.witnesses;
    // What the oracle did on each fence a `trace:` row holds to the engine
    // alone, recorded and not judged (harness §4.3, §8; #1650).
    let covered: Vec<String> = run
        .judged
        .iter()
        .filter(|j| j.judgement == Judgement::Covered)
        .map(|j| format!("{}: {}", j.id, j.note))
        .collect();
    let why = tally.oracle_absent.as_deref();
    // Each clause with its count, said where the count is not zero: what
    // did not run for want of the oracle, what the engine did not serve for
    // its trace there, what a `locale=<name>` the host lacks kept from
    // running (#1678), and the fences held to the engine alone.
    let clauses = [
        (
            why.map_or(0, |_| run.not_run()),
            "witness",
            "es",
            format!("did not run, {}", why.unwrap_or_default()),
        ),
        (
            tally.unserved.len() as u64,
            "fence",
            "s",
            format!("the engine did not serve: {}", tally.unserved.join("; ")),
        ),
        (
            run.count(Judgement::Skipped),
            "witness",
            "es",
            "skipped for a locale this host lacks".to_owned(),
        ),
        (
            covered.len() as u64,
            "fence",
            "s",
            format!("held to the engine alone: {}", covered.join("; ")),
        ),
    ];
    let said = clauses.into_iter().filter(|(n, ..)| *n > 0);
    let said = said.map(|(n, what, es, clause)| {
        let es = if n == 1 { "" } else { es };
        format!("; {n} {what}{es} {clause}")
    });
    said.collect::<Vec<_>>().concat()
}

/// `krate`'s reading over `snapshot`, under the coverage rule where the
/// manifest has one: the rule's id and its ADR, on every reading the
/// instrument writes.
fn under_rule(krate: &Crate, snapshot: &Snapshot, rule: Option<&Rule>) -> Result<Reading> {
    let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
    if let Some(rule) = rule {
        reading.rule.clone_from(&rule.id);
        reading.adr = rule.adr.adr_field();
    }
    Ok(reading)
}

/// What one `xtask witness` run was asked for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    /// Only these crates, by manifest name; empty is every crate that exists.
    pub crates: Vec<String>,
    /// What the run writes from the oracle's runs onto the fences.
    pub accept: Accept,
    /// Only the crates with a staged change, pre-commit's touched crates
    /// (harness §6), as `clippy --staged` runs over; with `crates` empty.
    pub staged: bool,
    /// Which of each crate's fences the run serves, and so whether what it
    /// judged is a reading.
    pub scope: Scope,
}

/// What `--accept` writes (design §8; harness §8, #801).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Accept {
    /// Nothing: the run judges and writes no fence.
    #[default]
    Nothing,
    /// `--accept`: the oracle's status onto each bare `exit`.
    Status,
    /// `--accept --streams`: the status, and the oracle's two streams into
    /// each fence marked `capture`, where a second run from a scratch
    /// directory apart prints them byte for byte.
    Streams,
}

impl From<crate::cli::Accepting> for Accept {
    fn from(asked: crate::cli::Accepting) -> Accept {
        match (asked.accept, asked.streams) {
            (false, _) => Accept::Nothing,
            (true, false) => Accept::Status,
            (true, true) => Accept::Streams,
        }
    }
}

impl Accept {
    /// The flags that carry it to the served process ([`watch`]).
    fn args(self) -> &'static [&'static str] {
        match self {
            Accept::Nothing => &[],
            Accept::Status => &["--accept"],
            Accept::Streams => &["--accept", "--streams"],
        }
    }
}

/// Which of a crate's fences a run serves (#1146). A crate's reading is a
/// claim over every fence it carries and the coverage beside them, and
/// only [`Scope::Crate`] makes it: a narrowed run serves the fences it
/// names, `--accept` pins those alone, coverage is not counted, and what
/// it judged is printed and never written, so `done` and pre-commit
/// never take it for the crate's reading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Scope {
    /// Every fence of the crate, and the kernels and natives without one.
    #[default]
    Crate,
    /// The fences in rustdoc under one source file, by its path from the
    /// workspace root or any tail of it (`src/natives.rs`).
    File(PathBuf),
    /// The fences on one item, by the name its declaration writes
    /// ([`fence::Witness::item`]).
    Item(String),
}

impl Scope {
    /// The part of a crate's fences this scope serves: all of them for the
    /// crate; for a narrowed run the named ones, the grammar's problems
    /// where they stand in the named file, and no subject.
    fn narrow(&self, mut found: Extracted) -> Extracted {
        let keeps = |file: &Path, item: Option<&str>| match self {
            Scope::Crate => true,
            Scope::File(path) => file.ends_with(path),
            Scope::Item(name) => item == Some(name.as_str()),
        };
        if *self != Scope::Crate {
            found
                .witnesses
                .retain(|w| keeps(&w.file, w.item.as_deref()));
            found.problems.retain(|p| keeps(&p.file, None));
            found.subjects.clear();
            found.anchored = None;
        }
        found
    }

    /// The flags that carry this scope to the served process ([`watch`]).
    fn args(&self) -> Vec<std::ffi::OsString> {
        match self {
            Scope::Crate => Vec::new(),
            Scope::File(path) => vec!["--file".into(), path.into()],
            Scope::Item(name) => vec!["--item".into(), name.into()],
        }
    }

    /// What becomes of the readings a run judged: the crate's are
    /// recorded, a narrowed run's printed, with the exit either way.
    fn record(&self, store: &Store, readings: &[Reading]) -> Result<()> {
        if *self == Scope::Crate {
            return crate::record(store, readings);
        }
        for reading in readings {
            println!("{reading}");
        }
        println!("witness: a run narrowed to the fences {self} writes no reading");
        crate::reading::Outcome::of(readings).exit()
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Crate => write!(f, "of the crate"),
            Scope::File(path) => write!(f, "under {}", path.display()),
            Scope::Item(name) => write!(f, "on {name}"),
        }
    }
}

/// Run what was asked for: one reading per crate, each written by the
/// caller through `record`.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    run_in(root, manifest, registry, ask, None, &Binary::of(root))
}

/// The crates `ask` names, every crate where it names none, or with
/// `--staged` the crates a staged change touches; none where `--staged`
/// finds no such change, which is said and runs nothing.
fn asked(root: &Path, manifest: &Manifest, ask: &Ask) -> Result<Option<Vec<String>>> {
    manifest.crates_named(&ask.crates)?;
    if !ask.staged {
        return Ok(Some(ask.crates.clone()));
    }
    if !ask.crates.is_empty() {
        bail!("witness: --staged names its own crates; drop --crate");
    }
    let touched = sources::staged_crates(root, manifest)?;
    if touched.is_empty() {
        println!("witness: no crate has a staged change");
        return Ok(None);
    }
    Ok(Some(touched))
}

/// [`run`], each snippet's start and end and each crate's reading
/// appended to `journal` where there is one ([`watch`]), and every host
/// served by `binary`, each built at the first fence or line that asks
/// for it.
fn run_in(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
    journal: Option<&Journal>,
    binary: &Binary,
) -> Result<Vec<Reading>> {
    let pin = manifest.pin()?;
    let Some(crates) = asked(root, manifest, ask)? else {
        return Ok(Vec::new());
    };
    let rule = coverage_rule(manifest);
    // One oracle for the run, and one for each variant a row names: each
    // digest is asserted before the first snippet under it and never per
    // witness or per crate (harness §4.3). A crate whose row names a
    // variant is read under it (#1782).
    let variants = manifest.variants();
    let oracles = Oracles::new(pin, &variants);
    // Each crate is read in three steps (#2212): found and prepared in the
    // manifest's order, its fences served on one pool with every other
    // crate's, so no crate's last fences leave a thread idle while the
    // next crate's wait, and finished in the manifest's order again.
    let (slots, ready, served) = find(root, manifest, &crates, &oracles, ask, journal);
    // No room for a scratch tree (#644): the volume is full for every
    // crate the pool serves, so none was measured, and each one's reading
    // is a skip naming the volume rather than a bork or a refusal.
    let (mut finished, full): (Vec<Option<Tally>>, Option<anyhow::Error>) =
        match serve_all(&ready, binary, registry, ask) {
            Ok(tallies) => (tallies.into_iter().map(Some).collect(), None),
            Err(why) if Full::of(&why).is_some() => {
                (ready.iter().map(|_| None).collect(), Some(why))
            }
            Err(why) => return Err(why),
        };
    // The tree before any witness runs, taken again after each `--accept`
    // that wrote a fence line: what each crate's reading is stamped with.
    let mut snapshot = Snapshot::disk(root)?;
    let mut readings = Vec::new();
    for slot in slots {
        let (one, tally) = match slot {
            Slot::Unrun(krate, why, failed) => {
                let mut unrun = under_rule(krate, &snapshot, rule)?.under(manifest.pin()?);
                unrun.message = format!("{}: {why}", krate.name);
                if failed {
                    unrun.status = Status::Fail;
                }
                readings.push(unrun);
                continue;
            }
            Slot::Ready(at) => (&ready[at], finished[at].take()),
        };
        if let Some(why) = &full {
            let unrun = under_rule(one.krate, &snapshot, rule)?.under(manifest.pin()?);
            readings.push(Full::skipped(unrun, &one.krate.name, why)?);
            continue;
        }
        let Some(tally) = tally else { continue };
        let reading = finished_reading(root, manifest, rule, one, &tally, &mut snapshot, ask)?;
        if let Some(journal) = journal {
            journal.judged(&reading)?;
        }
        readings.push(reading);
    }
    if ask.scope != Scope::Crate && served == 0 && readings.iter().all(|r| r.status != Status::Fail)
    {
        bail!(
            "witness: no fence stands {} in the crates asked for",
            ask.scope
        );
    }
    Ok(readings)
}

/// The crates `run` reads, in the manifest's order: those whose reading
/// is said without running anything, and those it serves, each found as
/// the source stands, its borks printed; and how many fences were found.
fn find<'a>(
    root: &Path,
    manifest: &'a Manifest,
    crates: &[String],
    oracles: &'a Oracles<'a>,
    ask: &Ask,
    journal: Option<&'a Journal>,
) -> (Vec<Slot<'a>>, Vec<Ready<'a>>, usize) {
    let rule = coverage_rule(manifest);
    let mut slots = Vec::new();
    let mut ready: Vec<Ready<'_>> = Vec::new();
    let mut served = 0;
    for krate in &manifest.crates {
        if !crates.is_empty() && !crates.iter().any(|n| n == &krate.name) {
            continue;
        }
        // The harness measures the engine and carries no witness of its
        // own; `done` asks it for none either (§4.5).
        let unrun = if krate.name == crate::layout::HARNESS {
            Some("the harness carries no witness")
        } else {
            (!sources::exists(root, krate)).then_some(sources::NOT_YET_CREATED)
        };
        if let Some(why) = unrun {
            slots.push(Slot::Unrun(krate, why.to_owned(), false));
            continue;
        }
        // A host's own lines are read before anything runs, as its fences
        // are, so a file that cannot be read fails the crate as a source
        // would.
        let picked = oracles.of(krate.oracle.as_deref()).and_then(|oracle| {
            let found = extract(root, krate, rule, Target::built())?;
            Ok((oracle, found, host::Held::of(root, krate)?))
        });
        let (oracle, found, held) = match picked {
            Ok((oracle, found, held)) => (
                oracle,
                ask.scope.narrow(found),
                held.map(|held| held.narrowed(&ask.scope)),
            ),
            // A source the grammar could not read, an oracle variant the
            // pin does not carry, or a host's lines that could not be read:
            // the crate's reading fails and names it, since `done` asks the crate for a witness reading and a
            // pass over sources never seen, or never held to an oracle,
            // would say the crate was measured when it was not.
            Err(unread) => {
                slots.push(Slot::Unrun(krate, format!("{unread:#}"), true));
                continue;
            }
        };
        for problem in &found.problems {
            println!("bork {}", problem.at_line());
        }
        served += found.witnesses.len() + held.as_ref().map_or(0, |held| held.lines.len());
        slots.push(Slot::Ready(ready.len()));
        ready.push(Ready {
            krate,
            oracle,
            found,
            held,
            watch: journal.map(|j| j.over(&krate.name)),
        });
    }
    (slots, ready, served)
}

/// Every crate's fences on one pool (#2212), in the order the crates
/// stand, and each crate's tally finished: a crate where the oracle is
/// absent served for its trace alone, as [`judge`] serves it.
fn serve_all(
    ready: &[Ready<'_>],
    binary: &Binary,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Tally>> {
    let prepared = ready
        .iter()
        .map(|one| prepare(one.oracle, &one.found))
        .collect::<Result<Vec<_>>>()?;
    let gathered: Vec<Gathered> = ready.iter().map(|_| Gathered::default()).collect();
    let items: Vec<(usize, &fence::Witness, Option<&Regime>)> = prepared
        .iter()
        .enumerate()
        .filter(|(_, crate_)| crate_.oracle_absent.is_none())
        .flat_map(|(at, crate_)| crate_.runnable.iter().map(move |(w, r)| (at, *w, *r)))
        .collect();
    let pooled = tally::Tally::judge_beside(&items, |(at, witness, regime)| {
        let one = &ready[*at];
        serve(
            one.oracle,
            binary,
            registry,
            ask,
            (witness, *regime),
            one.watch,
            &gathered[*at],
        )
    })?;
    let mut pooled = pooled.judged.into_iter();
    let mut finished = Vec::new();
    for ((one, crate_), gathered) in ready.iter().zip(prepared).zip(gathered) {
        let found_ = crate_.runnable.len();
        let (witnesses, unserved) = if crate_.oracle_absent.is_some() {
            let unserved = serve_absent(one.oracle, binary, &one.found, &crate_, &gathered);
            (tally::Tally::found(found_), unserved)
        } else {
            let judged = pooled.by_ref().take(found_).collect();
            (
                tally::Tally {
                    judged,
                    found: found_,
                },
                Vec::new(),
            )
        };
        finished.push(finish(&one.found, crate_, witnesses, gathered, unserved));
    }
    // A host's own lines beside its fences (#1975): each judged among the
    // crate's witnesses, whether or not the pinned php is here, since a
    // line whose pinned host is absent fails and is never a skip.
    let lines: Vec<(usize, &host::Held, &host::Line)> = ready
        .iter()
        .enumerate()
        .filter_map(|(at, one)| one.held.as_ref().map(|held| (at, held)))
        .flat_map(|(at, held)| held.lines.iter().map(move |line| (at, held, line)))
        .collect();
    let judged = tally::Tally::judge_beside(&lines, |(at, held, line)| {
        Ok(held.judge(ready[*at].oracle, binary.host(held.host()), registry, line))
    })?;
    for ((at, _, _), judged) in lines.iter().zip(judged.judged) {
        if let Some(tally) = finished.get_mut(*at) {
            tally.witnesses.judged.push(judged);
            tally.witnesses.found += 1;
        }
    }
    Ok(finished)
}

/// A crate the lane serves, as it was found before anything ran.
struct Ready<'a> {
    krate: &'a Crate,
    oracle: &'a Oracle<'a>,
    found: Extracted,
    held: Option<host::Held>,
    watch: Option<Watch<'a>>,
}

/// A crate's place in the manifest's order: one whose reading is said
/// without running anything, with its reason and whether it fails, or one
/// the lane serves, by its place among those.
enum Slot<'a> {
    Unrun(&'a Crate, String, bool),
    Ready(usize),
}

/// What one crate's finished tally prints, captures and reads as: the
/// missing and unserved, what did not run, the lines `--accept` writes
/// before the reading is stamped, and the declared wall.
fn finished_reading(
    root: &Path,
    manifest: &Manifest,
    rule: Option<&Rule>,
    one: &Ready<'_>,
    tally: &Tally,
    snapshot: &mut Snapshot,
    ask: &Ask,
) -> Result<Reading> {
    let krate = one.krate;
    for missing in &tally.missing {
        println!("missing {missing}");
    }
    for unserved in &tally.unserved {
        println!("unserved {unserved}");
    }
    if let (Some(why), n) = (&tally.oracle_absent, tally.witnesses.not_run())
        && n > 0
    {
        println!("witness: {n} of {} did not run; {why}", krate.name);
    }
    if ask.accept != Accept::Nothing && !tally.captured.is_empty() {
        capture(root, &tally.captured)?;
        *snapshot = Snapshot::disk(root)?;
    }
    // The diff is the review, fence by fence, as a person reads it before
    // the commit (#801).
    for captured in &tally.captured {
        print!(
            "accepted {}:{}:\n{}",
            captured.file.display(),
            captured.line,
            crate::oracle::diff(
                captured.was.join("\n").as_bytes(),
                captured.now.join("\n").as_bytes()
            )
        );
    }
    // `wall=D` is disclosure and gates nothing; the budget line is
    // where the declared durations are summed (design §8).
    if !tally.wall.is_zero() {
        println!(
            "witness: {} declares {:.3} s of wall over {} witnesses",
            krate.name,
            tally.wall.as_secs_f64(),
            tally.witnesses.judged.len()
        );
    }
    let mut judged = reading(tally, snapshot, krate, rule)?.under(manifest.pin()?);
    if let Some(under) = one.oracle.pin().oracle.read_under() {
        judged.message = format!("{}; {under}", judged.message);
    }
    Ok(judged)
}

/// The manifest rule whose `check` predicate names this instrument's
/// coverage half.
pub(crate) fn coverage_rule(manifest: &Manifest) -> Option<&Rule> {
    manifest.rule_checked_by(COVERAGE)
}

/// Every witness in a crate's sources, and every kernel or native the
/// coverage rule's scope covers there: the natives the attributes declare
/// where the crate's covered sources carry one, less those `target`'s
/// `#[cfg]` compiles out ([`anchor`]), beside the crate's kernels; and
/// [`fence::Subject`]'s count where they carry none, in an extension; a
/// crate outside `ext/` that declares no native is counted by its
/// kernels alone, since nothing else of it is a kernel or a native.
///
/// **A kernel is a function of the crate's `ops` module** (design §8,
/// `php_engine::ops`): [`fence::Subject`]'s count over `src/ops.rs` and
/// `src/ops/**`, which is what the rule's scope read for `php_engine`
/// before the anchor (#1264). An anchored crate keeps its kernels as
/// subjects beside its natives, since the anchor replaced the count of a
/// crate's other functions and not of its kernels; a native the attributes
/// declare in `ops` is counted once, as a native.
pub(crate) fn extract(
    root: &Path,
    krate: &Crate,
    rule: Option<&Rule>,
    target: &Target,
) -> Result<Extracted> {
    let mut out = Extracted::default();
    let mut anchored = anchor::Anchored::default();
    // The `php` that serves its `process` fences carries the crate's own
    // extension (#2721).
    let feature = (krate.feature.clone())
        .filter(|_| !Carried::binary(root).is_ok_and(|carried| carried.carries(krate)));
    for path in sources::rust(root, krate)? {
        let file = root.join(&path);
        let text = fs::read_to_string(&file)
            .with_context(|| format!("{path} is not a source the fence grammar can read"))?;
        let mut found = fence::extract(Path::new(&path), &text);
        for witness in &mut found.witnesses {
            witness.feature.clone_from(&feature);
        }
        if covered(rule, Path::new(&path)) {
            anchored.take(anchor::scan(
                Path::new(&path),
                &text,
                &found.witnesses,
                target,
            ));
        } else {
            found.subjects.clear();
        }
        out.take(found);
    }
    anchored.settle();
    // A crate whose natives stand on the attributes is counted by them,
    // once the fences have run, and by its kernels; so is a crate outside
    // `ext/` that declares none, whose other functions are neither (#1917).
    if anchored.declares() || !krate.path().starts_with("ext") {
        let ops = krate.path().join("src/ops");
        out.subjects.retain(|subject| {
            (subject.file.starts_with(&ops) || subject.file == ops.with_extension("rs"))
                && !anchored.declares_at(&subject.file, subject.line)
        });
    }
    if anchored.declares() {
        out.anchored = Some(anchor::Coverage {
            package: krate.package(),
            anchored,
        });
    }
    Ok(out)
}

/// Whether the coverage rule's scope covers a file. Without such a rule
/// nothing is counted, since the scope is the manifest's word and not a
/// copy here.
fn covered(rule: Option<&Rule>, path: &Path) -> bool {
    rule.is_some_and(|rule| rule.covers(path))
}

/// Judge every witness of one crate, and count the kernels and natives
/// without one: [`prepare`], the crate's fences on a pool of their own,
/// and [`finish`]. `run` serves every crate's fences on one pool instead
/// (#2212); a crate's tally is the same either way.
#[cfg(test)]
#[cfg(tracked)]
pub(crate) fn judge(
    oracle: &Oracle<'_>,
    binary: &Binary,
    registry: &Registry,
    ask: &Ask,
    found: &Extracted,
    watch: Option<Watch<'_>>,
) -> Result<Tally> {
    let prepared = prepare(oracle, found)?;
    let gathered = Gathered::default();
    let (witnesses, unserved) = if prepared.oracle_absent.is_some() {
        let unserved = serve_absent(oracle, binary, found, &prepared, &gathered);
        (tally::Tally::found(prepared.runnable.len()), unserved)
    } else {
        let served = tally::Tally::judge_beside(&prepared.runnable, |(witness, regime)| {
            serve(
                oracle,
                binary,
                registry,
                ask,
                (witness, *regime),
                watch,
                &gathered,
            )
        })?;
        (served, Vec::new())
    };
    Ok(finish(found, prepared, witnesses, gathered, unserved))
}

/// One crate's fences as they stand before any runs: the oracle's digest
/// asserted, each fence's regime read off the pin, and the blocks that
/// cannot run named.
pub(crate) struct Prepared<'a> {
    problems: Vec<String>,
    runnable: Vec<(&'a fence::Witness, Option<&'a Regime>)>,
    oracle_absent: Option<String>,
}

/// What serving one crate's fences kept beside their verdicts: the fence
/// lines `--accept` captured, and what each fence's run entered, by its id.
#[derive(Default)]
pub(crate) struct Gathered {
    captured: Mutex<Vec<Captured>>,
    entered: Mutex<HashMap<String, Record>>,
}

/// One crate made ready to serve. Coverage is a count over the source and
/// needs no php; the digest is asserted once, here, before the first
/// snippet, and a mismatch refuses the whole run (§4.3), never caught per
/// witness as a bork. A binary that is not at the path is no refusal:
/// nothing runs, each witness is a snippet that did not run, and the
/// reading says why (#65).
fn prepare<'a>(oracle: &'a Oracle<'_>, found: &'a Extracted) -> Result<Prepared<'a>> {
    let oracle_absent = oracle.pin().oracle.absent();
    if oracle_absent.is_none() {
        oracle.asserted()?;
        if found.witnesses.iter().any(|w| w.server) {
            crate::phpt::server::version(oracle, "the witnesses marked mysql")?;
        }
    }
    // Which regime each witness stands under, read off the pin before
    // anything runs and without php: a fence names one `[regime.<name>]`
    // and the values are the pin's, so a name the pin does not carry is a
    // block the lane cannot run, whether or not the oracle is here.
    let mut problems: Vec<String> = found.problems.iter().map(fence::Problem::at_line).collect();
    let mut runnable = Vec::new();
    for witness in &found.witnesses {
        match under(oracle.pin(), witness) {
            Ok(regime) => runnable.push((witness, regime)),
            Err(unknown) => problems.push(unknown),
        }
    }
    Ok(Prepared {
        problems,
        runnable,
        oracle_absent,
    })
}

/// One fence judged beside the oracle, what its run left gathered for
/// its crate and its start and end on the watch journal.
fn serve(
    oracle: &Oracle<'_>,
    binary: &Binary,
    registry: &Registry,
    ask: &Ask,
    (witness, regime): (&fence::Witness, Option<&Regime>),
    watch: Option<Watch<'_>>,
    gathered: &Gathered,
) -> Result<Judged> {
    if let Some(watch) = watch {
        watch.serving(witness)?;
    }
    let mut kept = judge::Kept::default();
    let judged = judge::one(oracle, registry, ask, witness, regime, binary, &mut kept)?;
    crate::locked(&gathered.captured).extend(kept.captured);
    crate::locked(&gathered.entered).insert(witness.id(), kept.entered);
    if let Some(watch) = watch {
        watch.served(witness)?;
    }
    Ok(judged)
}

/// Coverage is what a fence ran, and neither the linked engine nor the
/// binary needs php: where the oracle is absent and the attributes declare
/// natives, each fence is still served, for its trace alone, though
/// nothing is judged. A serve that fails is `Unserved`, as it is beside
/// the oracle, and named, never a fence that entered nothing (#1535).
fn serve_absent(
    oracle: &Oracle<'_>,
    binary: &Binary,
    found: &Extracted,
    prepared: &Prepared<'_>,
    gathered: &Gathered,
) -> Vec<String> {
    let mut unserved = Vec::new();
    if found.anchored.is_none() {
        return unserved;
    }
    for (witness, regime) in &prepared.runnable {
        let record = match judge::serve(oracle, binary, witness, *regime) {
            Ok((_, natives)) => Record::Entered(natives),
            Err(trouble) => {
                unserved.push(format!("{}: {trouble:#}", witness.at()));
                Record::Unserved
            }
        };
        crate::locked(&gathered.entered).insert(witness.id(), record);
    }
    unserved
}

/// One crate's tally once its fences are judged: an anchored crate's
/// natives by what the fences entered and its kernels by the witness in
/// their own doc block ([`extract`]), and the captured lines in the order
/// the fences stand.
fn finish(
    found: &Extracted,
    prepared: Prepared<'_>,
    witnesses: tally::Tally<Judgement>,
    gathered: Gathered,
    unserved: Vec<String>,
) -> Tally {
    let wall = if prepared.oracle_absent.is_some() {
        Duration::ZERO
    } else {
        prepared.runnable.iter().filter_map(|(w, _)| w.wall).sum()
    };
    let entered = gathered
        .entered
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let subjects: Vec<fence::Subject> = match &found.anchored {
        Some(coverage) => coverage
            .subjects(&entered)
            .into_iter()
            .chain(found.subjects.iter().cloned())
            .collect(),
        None => found.subjects.clone(),
    };
    // Captured in the order the threads finished; written and printed in
    // the order the fences stand.
    let mut captured = gathered
        .captured
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    captured.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    Tally {
        witnesses,
        subjects: subjects.len(),
        missing: subjects
            .iter()
            .filter(|s| !s.witnessed)
            .map(fence::Subject::at)
            .collect(),
        problems: prepared.problems,
        wall,
        captured,
        oracle_absent: prepared.oracle_absent,
        unserved,
    }
}

/// The regime one witness stands under, by the name its fence carries;
/// nothing where it carries none, and where it names one the pin does not
/// have, the line a person reads, at the fence and beside the regimes the
/// pin does have. A regime that carries `args` reaches a binary's command
/// line (#1780): a fence marked `process` hands them to the engine's
/// binary as the oracle's run hands them to php, and the linked engine
/// takes `-e` as a host builds it and no other ([`Linked`], #1822).
/// php-cgi takes a regime on its command line as `php` does, in the
/// oracle's run and the engine's (#1729); php-fpm and phpdbg are handed
/// none, so a `request fpm` or `request phpdbg` fence under one would run
/// under the recipe alone (#1932). A fence served where its regime cannot
/// reach is the same kind of line, naming why.
fn under<'a>(pin: &'a Pin, witness: &fence::Witness) -> Result<Option<&'a Regime>, String> {
    let Some(name) = witness.regime.as_deref() else {
        return Ok(None);
    };
    let regime = pin
        .regime(name)
        .map_err(|why| format!("{}: {why:#}", witness.at()))?;
    let refused = match witness.served {
        fence::Served::Binary | fence::Served::Cgi => None,
        fence::Served::Linked => Linked::of([regime]).err().map(|why| {
            format!("{why:#}; only a fence marked `process` stands under it")
        }),
        fence::Served::Fpm | fence::Served::Phpdbg | fence::Served::Makala => Some(
            "reaches neither php-fpm, phpdbg nor makala, in the oracle's run or the engine's, so a `request fpm`, `request phpdbg` or `request makala` fence cannot stand under it"
                .to_owned(),
        ),
    };
    match refused {
        Some(why) => Err(format!("{}: `[regime.{name}]` {why}", witness.at())),
        None => Ok(Some(regime)),
    }
}

/// Write the fence lines a run captured: each file under `root` read, the
/// lines replaced, the file written back. Nothing else of the file moves,
/// which is what makes the diff `--accept` produces one a person can read.
/// A file's blocks are replaced from its last up, so a block whose lines
/// grow or shrink moves no line a block above it was read at.
pub(crate) fn capture(root: &Path, captured: &[Captured]) -> Result<()> {
    let mut by_file: BTreeMap<&Path, Vec<&Captured>> = BTreeMap::new();
    for one in captured {
        by_file.entry(&one.file).or_default().push(one);
    }
    for (name, mut blocks) in by_file {
        let file = root.join(name);
        let text = crate::read(&file)?;
        let ends_with_newline = text.ends_with('\n');
        let mut kept: Vec<String> = text.lines().map(str::to_owned).collect();
        blocks.sort_by_key(|one| std::cmp::Reverse(one.line));
        for one in blocks {
            let at = one.line - 1;
            if kept.get(at..at + one.was.len()) != Some(&one.was[..]) {
                bail!(
                    "{}:{} is not the fence it was when it was read; run the instrument again",
                    file.display(),
                    one.line
                );
            }
            kept.splice(at..at + one.was.len(), one.now.iter().cloned());
        }
        let mut out = kept.join("\n");
        if ends_with_newline {
            out.push('\n');
        }
        crate::write(&file, out)?;
    }
    Ok(())
}

/// Every crate that exists less those whose `witness` reading passes and
/// was taken over the crate's key now ([`Snapshot::key`], its tree and
/// its dependency closure's, #1318) under the pin now, `done`'s fresh:
/// what the merge check measures after pre-commit measured the staged
/// crates (#1312).
fn unmeasured(root: &Path, manifest: &Manifest, store: &Store) -> Result<Vec<String>> {
    let snapshot = Snapshot::disk(root)?;
    let mut left = Vec::new();
    for krate in manifest.crates.iter().filter(|k| sources::exists(root, k)) {
        let tree = snapshot.key(&krate.path())?;
        let stands = store
            .read(INSTRUMENT, &krate.name)?
            .is_some_and(|r| r.status == Status::Pass && r.fresh_for(&tree, manifest.pin_digest()));
        if !stands {
            left.push(krate.name.clone());
        }
    }
    Ok(left)
}

/// The command line's entry. Without `journal`, the run is served in a
/// process of its own and watched, and every reading is written here
/// ([`watch`]); with one, this is that process: the run served, and
/// what it judged appended to the journal for the watching process to
/// write.
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    asked: crate::cli::Witness,
) -> Result<()> {
    let scope = match (asked.file, asked.item) {
        (Some(file), _) => Scope::File(file.strip_prefix(root).unwrap_or(&file).to_path_buf()),
        (None, Some(item)) => Scope::Item(item),
        (None, None) => Scope::Crate,
    };
    let crates = if asked.unmeasured {
        let left = unmeasured(root, manifest, store)?;
        if left.is_empty() {
            println!("witness: every crate stands on a pass over its tree now; not run again");
            return Ok(());
        }
        left
    } else {
        asked.crates
    };
    let ask = Ask {
        crates,
        accept: asked.accepting.into(),
        staged: asked.staged,
        scope,
    };
    let Some(journal) = asked.journal else {
        return watch::watch(root, manifest, store, &ask);
    };
    let registry = Registry::load(root)?;
    let journal = Journal::open(&journal)?;
    run_in(
        root,
        manifest,
        &registry,
        &ask,
        Some(&journal),
        &Binary::of(root),
    )
    .map(|_| ())
}
