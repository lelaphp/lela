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
//! **What runs the snippet.** The same seam `phpt` names (design §8's
//! `ScriptExec`): the engine's own binary through [`oracle::Runner`], one
//! process per witness under the recipe the oracle runs under. Until
//! `crates/php` reaches `serve` there is no binary, and then the pinned
//! oracle stands in: the run measures the fences and the oracle, not the
//! engine, so the reading is a skip that says so rather than passing. The
//! oracle runs in either case, because the status a fence pins is the
//! oracle's and `--accept` is where it is captured.
//!
//! **What `--accept` writes.** The `expect` fence line, and nothing else:
//! the oracle's status in place of a bare `exit` or a stale one, so the
//! status is a function of snippet and oracle, captured and never authored.
//! The manifest's `witness.exit-authored` rule denies the other path at the
//! tool call, and this is why: nothing an agent writes can put a number
//! there. Read the diff it produces.
//!
//! **What coverage is.** The manifest's `witness.coverage` rule names the
//! scope, `crates/php_engine/src/ops/**` and `ext/*/src/**`; a kernel or a
//! native there without a witness raises `missing`, and `done` prints it
//! per crate. What counts as one is [`fence::Subject`]'s sentence.
//!
//! **What is the lane module's.** The tally, the print loop, the runner
//! ladder and the reading's status are [`lane`]'s, shared with `phpt`;
//! this module contributes [`Judgement`], the per-item judge in `judge`
//! and the words of the crate's message. A crate whose sources cannot be
//! read is a failing reading naming the crate and the file, never a crate
//! quietly skipped, as a phpt subset the pinned tree has not is.
//!
//! **What `wall=D` is.** Disclosure. The declared durations are summed into
//! the reading's `wall_ms` and gate nothing; the limit a run has is the
//! recipe's.

pub mod fence;
mod judge;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::lane::{self, Outcome};
use crate::manifest::{Crate, Manifest, Rule};
use crate::oracle::{Oracle, Runner};
use crate::reading::{Reading, Status, Store, listed};
use crate::sources;
use fence::Extracted;

/// The instrument's name, the reading's `instrument` field.
pub const INSTRUMENT: &str = "witness";
/// What the manifest's coverage rule names in its `check` predicate.
pub const COVERAGE: &str = "witness-coverage";

/// What one witness came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    Pass,
    /// The bytes differ and an accepted registry row says so.
    Diverged,
    /// The bytes differ and nothing accepts it.
    Fail,
    /// A bare `exit`: the run has no status to hold the snippet to until
    /// `--accept` captures the oracle's.
    Unpinned,
    /// The witness could not be run or read at all.
    Bork,
}

impl lane::Judgement for Judgement {
    const PASS: Judgement = Judgement::Pass;

    fn as_str(self) -> &'static str {
        match self {
            Judgement::Pass => "pass",
            Judgement::Diverged => "diverged",
            Judgement::Fail => "fail",
            Judgement::Unpinned => "unpinned",
            Judgement::Bork => "bork",
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
        matches!(self, Judgement::Pass | Judgement::Diverged)
    }
}

/// One witness's verdict: its divergence key, where it stands as its
/// name, the judgement and the note.
pub type Judged = lane::Judged<Judgement>;

/// The one thing `--accept` writes: a fence line, by the file and the line
/// it stands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub file: PathBuf,
    /// 1-based.
    pub line: usize,
    pub was: String,
    pub now: String,
}

/// What one crate's run came to: the witnesses judged, and the coverage
/// half beside them.
#[derive(Debug, Clone, Default)]
pub struct Tally {
    pub witnesses: lane::Tally<Judgement>,
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
}

impl Tally {
    /// Whether the crate's reading fails: a witness that missed, a block
    /// the grammar could not read, or a kernel or native without one.
    pub fn failed(&self) -> bool {
        !self.witnesses.against().is_empty()
            || !self.problems.is_empty()
            || !self.missing.is_empty()
    }
}

/// The crate's reading: the counts in `numbers`, what failed named in the
/// message, and the status the ladder allows. A witness that missed fails
/// the crate whatever ran it, since what a fence pins is the oracle's and
/// the oracle ran; a crate with no witness and nothing that needs one
/// passes whatever ran, since the coverage half was measured.
pub fn reading(
    tally: &Tally,
    root: &Path,
    krate: &Crate,
    rule: Option<&Rule>,
    runner: &Runner,
) -> Result<Reading> {
    let name = &krate.name;
    let run = &tally.witnesses;
    let outcome = if tally.failed() {
        Outcome::Failed
    } else if run.judged.is_empty() {
        Outcome::Passed
    } else {
        run.outcome(runner)
    };
    let message = match &outcome {
        Outcome::Failed => {
            let uncovered = if tally.missing.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} of {} kernels or natives without a witness: {}. {}",
                    tally.missing.len(),
                    tally.subjects,
                    listed(&tally.missing.iter().map(String::as_str).collect::<Vec<_>>()),
                    rule.map(|r| r.text.as_str()).unwrap_or_default()
                )
            };
            let unread = if tally.problems.is_empty() {
                String::new()
            } else {
                format!("; {}", tally.problems.join("; "))
            };
            format!(
                "{name}: {} of {} witnesses missed what they pin{}{uncovered}{unread}",
                run.against().len(),
                run.judged.len(),
                run.missed()
            )
        }
        Outcome::Passed if run.judged.is_empty() => {
            format!("{name}: no witness, and no kernel or native that needs one")
        }
        Outcome::Passed => format!(
            "{name}: {} witnesses pass over {} kernels or natives",
            run.judged.len(),
            tally.subjects
        ),
        // Every witness met what it pins, and whether that is a reading of
        // the engine is the runner's to say: without the engine's own
        // binary the pinned oracle ran them, which measures the fences and
        // the oracle, and a green line must not stand where the engine's
        // belongs.
        Outcome::Control(binary) => format!(
            "{name}: {} witnesses agree with {}, the pinned oracle; a control of the fences, not a reading of the engine",
            run.judged.len(),
            binary.display()
        ),
        Outcome::Absent(why) => format!(
            "{name}: {} witnesses agree with the pinned oracle, and the engine ran none; {why}",
            run.judged.len()
        ),
        // A witness that met nothing counts against the crate, so this
        // rung is one the ladder has and this lane's pre-rung reaches
        // first; the words are here so no rung is without any.
        Outcome::NothingRan => format!("{name}: no witness ran of {} found", run.found),
    };
    // Constructed here and not before the run, since `--accept` writes the
    // sources the stamp is over.
    let mut reading = run.reading(
        under_rule(Reading::for_crate(INSTRUMENT, krate, root)?, rule),
        &outcome,
        message,
    )?;
    reading.count("passed", run.count(Judgement::Pass))?;
    reading.count("failed", run.against().len() as u64)?;
    reading.count("diverged", run.count(Judgement::Diverged))?;
    reading.count("unpinned", run.count(Judgement::Unpinned))?;
    reading.count("subjects", tally.subjects as u64)?;
    reading.count("missing", tally.missing.len() as u64)?;
    reading.count("accepted", tally.captured.len() as u64)?;
    reading.count("unread", tally.problems.len() as u64)?;
    reading.count("wall_ms", declared_ms(tally.wall))?;
    Ok(reading)
}

/// The reading under the coverage rule, when the manifest has one: the
/// rule's id and its ADR, on every reading the instrument writes.
fn under_rule(mut reading: Reading, rule: Option<&Rule>) -> Reading {
    if let Some(rule) = rule {
        reading.rule.clone_from(&rule.id);
        reading.adr = rule.adr.adr_field();
    }
    reading
}

/// The declared wall as the reading counts it; a witness the recipe would
/// time out on long before is not one this saturation can reach.
fn declared_ms(wall: Duration) -> u64 {
    u64::try_from(wall.as_millis()).unwrap_or(u64::MAX)
}

/// What one `xtask witness` run was asked for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    /// Only these crates, by manifest name; empty is every crate that exists.
    pub crates: Vec<String>,
    /// Capture the oracle's status onto each fence line that does not
    /// already pin it.
    pub accept: bool,
}

/// Run what was asked for: one reading per crate, each written by the
/// caller through `record`.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    for name in &ask.crates {
        if manifest.crate_named(name).is_none() {
            bail!("the manifest has no crate named {name:?}");
        }
    }
    let rule = coverage_rule(manifest);
    let runner = Runner::resolve(root, &manifest.pin, None)?;
    if let Runner::Absent(why) = &runner {
        println!("witness: {why}");
    }
    // One oracle for the run: the digest is asserted before the first
    // snippet and never per witness or per crate (harness §4.3).
    let oracle = manifest.pin.oracle();
    let mut readings = Vec::new();
    for krate in &manifest.crates {
        if !ask.crates.is_empty() && !ask.crates.iter().any(|n| n == &krate.name) {
            continue;
        }
        // The reading of a crate no witness runs for, which nothing
        // measured and so carries no stamp: the harness's, or one not yet
        // created.
        let mut unrun = under_rule(Reading::new(INSTRUMENT, &krate.name), rule);
        if krate.name == crate::done::HARNESS {
            // The harness measures the engine and carries no witness of its
            // own; `done` asks it for none either (§4.5).
            unrun.message = format!("{}: the harness carries no witness", krate.name);
            readings.push(unrun);
            continue;
        }
        if !sources::exists(root, krate) {
            unrun.message = format!("{}: not yet created", krate.name);
            readings.push(unrun);
            continue;
        }
        let found = match extract(root, krate, rule) {
            Ok(found) => found,
            // A source the grammar could not read: the crate's reading
            // fails and names it, since `done` asks the crate for a
            // witness reading and a pass over sources never seen would
            // say the crate was measured when it was not.
            Err(unread) => {
                let mut failed = under_rule(Reading::for_crate(INSTRUMENT, krate, root)?, rule);
                failed.status = Status::Fail;
                failed.message = format!("{}: {unread:#}", krate.name);
                readings.push(failed);
                continue;
            }
        };
        for problem in &found.problems {
            println!("bork {}", problem.at_line());
        }
        let tally = judge(&oracle, registry, &runner, ask, &found)?;
        for missing in &tally.missing {
            println!("missing {missing}");
        }
        if ask.accept {
            capture(root, &tally.captured)?;
        }
        for captured in &tally.captured {
            println!(
                "accepted {}:{}: {}",
                captured.file.display(),
                captured.line,
                captured.now.trim()
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
        readings.push(reading(&tally, root, krate, rule, &runner)?);
    }
    Ok(readings)
}

/// The manifest rule whose `check` predicate names this instrument's
/// coverage half.
pub fn coverage_rule(manifest: &Manifest) -> Option<&Rule> {
    manifest.rule_checked_by(COVERAGE)
}

/// Every witness in a crate's sources, and every kernel or native the
/// coverage rule's scope covers there.
pub fn extract(root: &Path, krate: &Crate, rule: Option<&Rule>) -> Result<Extracted> {
    let mut out = Extracted::default();
    for path in sources::rust(root, krate)? {
        let file = root.join(&path);
        let text = fs::read_to_string(&file)
            .with_context(|| format!("{path} is not a source the fence grammar can read"))?;
        let mut found = fence::extract(Path::new(&path), &text);
        if !covered(rule, Path::new(&path)) {
            found.subjects.clear();
        }
        out.take(found);
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
/// without one.
pub fn judge(
    oracle: &Oracle<'_>,
    registry: &Registry,
    runner: &Runner,
    ask: &Ask,
    found: &Extracted,
) -> Result<Tally> {
    if !found.witnesses.is_empty() {
        // A digest mismatch refuses the whole run (§4.3): asserted here,
        // before the first snippet, and not caught per witness as a bork.
        oracle.asserted()?;
    }
    let mut captured = Vec::new();
    let mut wall = Duration::ZERO;
    let witnesses = lane::Tally::judge(&found.witnesses, |witness| {
        wall += witness.wall.unwrap_or_default();
        judge::one(oracle, registry, runner, ask, witness, &mut captured)
    })?;
    Ok(Tally {
        witnesses,
        subjects: found.subjects.len(),
        missing: found
            .subjects
            .iter()
            .filter(|s| !s.witnessed)
            .map(fence::Subject::at)
            .collect(),
        problems: found.problems.iter().map(fence::Problem::at_line).collect(),
        wall,
        captured,
    })
}

/// Write the fence lines a run captured: each file under `root` read, the
/// lines replaced, the file written back. Nothing else of the file moves,
/// which is what makes the diff `--accept` produces one a person can read.
pub fn capture(root: &Path, captured: &[Captured]) -> Result<()> {
    let mut by_file: BTreeMap<&Path, Vec<&Captured>> = BTreeMap::new();
    for one in captured {
        by_file.entry(&one.file).or_default().push(one);
    }
    for (name, lines) in by_file {
        let file = root.join(name);
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let ends_with_newline = text.ends_with('\n');
        let mut kept: Vec<String> = text.lines().map(str::to_owned).collect();
        for one in lines {
            let at = one.line - 1;
            if kept.get(at) != Some(&one.was) {
                bail!(
                    "{}:{} is not the fence line it was when it was read; run the instrument again",
                    file.display(),
                    one.line
                );
            }
            kept[at].clone_from(&one.now);
        }
        let mut out = kept.join("\n");
        if ends_with_newline {
            out.push('\n');
        }
        fs::write(&file, out).with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(())
}

/// The command line's entry: run what the two flags asked for and write
/// every reading.
pub fn command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    crates: Vec<String>,
    accept: bool,
) -> Result<()> {
    let registry = Registry::load(root)?;
    let ask = Ask { crates, accept };
    crate::record(store, &run(root, manifest, &registry, &ask)?)
}
