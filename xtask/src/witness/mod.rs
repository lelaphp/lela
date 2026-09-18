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
//! **What runs the snippet.** The seam `phpt` names (design §8's
//! `ScriptExec`) has two adapters, and a witness stands on either. The
//! engine's own binary through [`oracle::Runner`] is one, a process per
//! witness under the recipe the oracle runs under; the engine linked into
//! the harness is the other, [`oracle::linked`], one fresh engine per
//! snippet over the `Capture` sink and no binary needed. Until
//! `crates/php` reaches `serve` there is none, and the linked engine is
//! what the lane stands on: it is the engine either way, so the reading is
//! a pass or a fail on its own merits and `done` holds the crate to it
//! (#119). The oracle runs beside whichever served the snippet, because
//! the status a fence pins is the oracle's and `--accept` is where it is
//! captured; the pinned oracle standing in *for* the engine, which a named
//! binary whose digest is the pin's is, is a control and reads as a skip
//! whatever the fences say. `phpt` has no second adapter yet and keeps its
//! own rung in `done`, since nothing stands in for a corpus (#132). A
//! reading with nothing judged, `run == 0`, is the run the pinned oracle
//! was absent for, and is a skip that says so.
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
//! check revision 43 names, failing.
//!
//! **What `--accept` writes.** The `expect` fence line, and nothing else:
//! the oracle's status in place of a bare `exit` or a stale one, so the
//! status is a function of snippet and oracle, captured and never authored.
//! The manifest's `witness.exit-authored` rule denies the other path at the
//! call, and this is why: nothing an agent writes can put a number
//! there. Read the diff it produces.
//!
//! **What coverage is.** The manifest's `witness.coverage` rule names the
//! scope, `crates/php_engine/src/ops/**` and `ext/*/src/**`; a kernel or a
//! native there without a witness raises `missing`, and `done` prints it
//! per crate. What counts as one is [`fence::Subject`]'s sentence.
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

pub(crate) mod fence;
mod judge;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::manifest::{Crate, Manifest, Rule};
use crate::oracle::{Oracle, Runner};
use crate::reading::{Reading, Status, Store, listed};
use crate::sources;
use crate::tally::{self, Outcome};
use fence::Extracted;

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "witness";
/// What the manifest's coverage rule names in its `check` predicate.
pub(crate) const COVERAGE: &str = "witness-coverage";

/// What one witness came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Judgement {
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

impl tally::Judgement for Judgement {
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
pub(crate) type Judged = tally::Judged<Judgement>;

/// The one thing `--accept` writes: a fence line, by the file and the line
/// it stands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Captured {
    pub file: PathBuf,
    /// 1-based.
    pub line: usize,
    pub was: String,
    pub now: String,
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
        run.complete(tally.oracle_absent.as_deref())
    } else {
        run.outcome(runner)
    };
    // What did not run for want of the oracle, as a message's tail.
    let unrun = match (&tally.oracle_absent, run.not_run()) {
        (Some(why), n) if n > 0 => {
            let s = if n == 1 { "" } else { "es" };
            format!("; {n} witness{s} did not run, {why}")
        }
        _ => String::new(),
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
    reading.count("not_run", run.not_run())?;
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
    /// Only the crates with a staged change, pre-commit's touched crates
    /// (harness §6), as `clippy --staged` runs over; with `crates` empty.
    pub staged: bool,
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
    let crates = if ask.staged {
        if !ask.crates.is_empty() {
            bail!("witness: --staged names its own crates; drop --crate");
        }
        let touched = sources::staged_crates(root, manifest)?;
        if touched.is_empty() {
            println!("witness: no crate has a staged change");
            return Ok(Vec::new());
        }
        touched
    } else {
        ask.crates.clone()
    };
    let rule = coverage_rule(manifest);
    // The ladder's rung below the binary is the linked engine, which a
    // snippet needs no binary for (#119).
    let runner = Runner::resolve(root, &manifest.pin, None)?.or_linked();
    if runner == Runner::Linked {
        println!("witness: no engine binary is built; the linked engine serves each snippet");
    }
    // One oracle for the run: the digest is asserted before the first
    // snippet and never per witness or per crate (harness §4.3).
    let oracle = manifest.pin.oracle();
    let mut readings = Vec::new();
    for krate in &manifest.crates {
        if !crates.is_empty() && !crates.iter().any(|n| n == &krate.name) {
            continue;
        }
        // The reading of a crate no witness runs for: the harness's, or
        // one not yet created, which carries no stamp.
        let mut unrun = under_rule(Reading::for_crate(INSTRUMENT, krate, root)?, rule);
        if krate.name == crate::done::HARNESS {
            // The harness measures the engine and carries no witness of its
            // own; `done` asks it for none either (§4.5).
            unrun.message = format!("{}: the harness carries no witness", krate.name);
            readings.push(unrun);
            continue;
        }
        if !sources::exists(root, krate) {
            unrun.message = format!("{}: {}", krate.name, sources::NOT_YET_CREATED);
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
        if let (Some(why), n) = (&tally.oracle_absent, tally.witnesses.not_run())
            && n > 0
        {
            println!("witness: {n} of {} did not run; {why}", krate.name);
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
pub(crate) fn coverage_rule(manifest: &Manifest) -> Option<&Rule> {
    manifest.rule_checked_by(COVERAGE)
}

/// Every witness in a crate's sources, and every kernel or native the
/// coverage rule's scope covers there.
pub(crate) fn extract(root: &Path, krate: &Crate, rule: Option<&Rule>) -> Result<Extracted> {
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
pub(crate) fn judge(
    oracle: &Oracle<'_>,
    registry: &Registry,
    runner: &Runner,
    ask: &Ask,
    found: &Extracted,
) -> Result<Tally> {
    // Coverage is a count over the source and needs no php; the digest is
    // asserted once, here, before the first snippet, and a mismatch refuses
    // the whole run (§4.3), never caught per witness as a bork. A binary
    // that is not at the path is no refusal: nothing runs, each witness is
    // a snippet that did not run, and the reading says why (#65).
    let pinned = &oracle.pin().oracle.path;
    let oracle_absent = if pinned.is_file() {
        oracle.asserted()?;
        None
    } else {
        Some(format!("the pinned php is not at {}", pinned.display()))
    };
    let mut captured = Vec::new();
    let mut wall = Duration::ZERO;
    let witnesses = if oracle_absent.is_some() {
        tally::Tally::found(found.witnesses.len())
    } else {
        tally::Tally::judge(&found.witnesses, |witness| {
            wall += witness.wall.unwrap_or_default();
            judge::one(oracle, registry, runner, ask, witness, &mut captured)
        })?
    };
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
        oracle_absent,
    })
}

/// Write the fence lines a run captured: each file under `root` read, the
/// lines replaced, the file written back. Nothing else of the file moves,
/// which is what makes the diff `--accept` produces one a person can read.
pub(crate) fn capture(root: &Path, captured: &[Captured]) -> Result<()> {
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
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    crates: Vec<String>,
    accept: bool,
    staged: bool,
) -> Result<()> {
    let registry = Registry::load(root)?;
    let ask = Ask {
        crates,
        accept,
        staged,
    };
    crate::record(store, &run(root, manifest, &registry, &ask)?)
}
