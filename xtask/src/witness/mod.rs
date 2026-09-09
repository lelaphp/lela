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
//! `ScriptExec`): the engine's own binary through [`phpt::Runner`], one
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
//! **What `wall=D` is.** Disclosure. The declared durations are summed into
//! the reading's `wall_ms` and gate nothing; the limit a run has is the
//! recipe's.

pub mod fence;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::git::Stamp;
use crate::manifest::{Crate, Manifest, Rule};
use crate::oracle::{self, Invocation, Output, Pin, Program, Verdict as Compared};
use crate::phpt::expect::Expectation;
use crate::phpt::{Runner, listed};
use crate::reading::{Reading, Status, Store};
use crate::sources;
use fence::{Exit, Extracted, Witness};

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

impl Judgement {
    pub fn as_str(self) -> &'static str {
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
}

/// One witness's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    /// The divergence key, whatever the judgement.
    pub id: String,
    /// Where the witness stands, `ext/pcre/src/lib.rs:31`.
    pub at: String,
    /// The item it documents, empty for a file's own doc.
    pub subject: String,
    pub judgement: Judgement,
    /// Why: the difference, the accepted row's reason, or the trouble.
    pub note: String,
}

impl Judged {
    fn new(witness: &Witness, judgement: Judgement, note: impl Into<String>) -> Judged {
        Judged {
            id: witness.id(),
            at: witness.at(),
            subject: witness.subject.clone(),
            judgement,
            note: note.into(),
        }
    }
}

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

/// What one crate's run came to.
#[derive(Debug, Clone, Default)]
pub struct Tally {
    pub judged: Vec<Judged>,
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
    fn count(&self, judgement: Judgement) -> u64 {
        self.judged
            .iter()
            .filter(|j| j.judgement == judgement)
            .count() as u64
    }

    /// The witnesses that fail the crate, by where they stand.
    pub fn against(&self) -> Vec<&str> {
        self.judged
            .iter()
            .filter(|j| j.judgement.counts_against())
            .map(|j| j.at.as_str())
            .collect()
    }

    /// Whether the crate's reading fails: a witness that missed, a block
    /// the grammar could not read, or a kernel or native without one.
    pub fn failed(&self) -> bool {
        !self.against().is_empty() || !self.problems.is_empty() || !self.missing.is_empty()
    }

    /// The crate's reading: the counts in `numbers`, what failed named in
    /// the message, and the status the runner allows.
    pub fn reading(
        &self,
        root: &Path,
        krate: &Crate,
        rule: Option<&Rule>,
        runner: &Runner,
    ) -> Result<Reading> {
        let name = &krate.name;
        let mut reading = Reading::new(INSTRUMENT, name);
        if let Some(rule) = rule {
            reading.rule.clone_from(&rule.id);
            reading.adr = rule.adr.adr_field();
        }
        reading.count("run", self.judged.len() as u64)?;
        reading.count("passed", self.count(Judgement::Pass))?;
        reading.count("failed", self.against().len() as u64)?;
        reading.count("diverged", self.count(Judgement::Diverged))?;
        reading.count("unpinned", self.count(Judgement::Unpinned))?;
        reading.count("subjects", self.subjects as u64)?;
        reading.count("missing", self.missing.len() as u64)?;
        reading.count("accepted", self.captured.len() as u64)?;
        reading.count("unread", self.problems.len() as u64)?;
        reading.count("wall_ms", declared_ms(self.wall))?;

        let uncovered = if self.missing.is_empty() {
            String::new()
        } else {
            format!(
                "; {} of {} kernels or natives without a witness: {}. {}",
                self.missing.len(),
                self.subjects,
                listed(&self.missing.iter().map(String::as_str).collect::<Vec<_>>()),
                rule.map(|r| r.text.as_str()).unwrap_or_default()
            )
        };
        let unread = if self.problems.is_empty() {
            String::new()
        } else {
            format!("; {}", self.problems.join("; "))
        };
        if self.failed() {
            let against = self.against();
            let missed = if against.is_empty() {
                String::new()
            } else {
                format!(": {}", listed(&against))
            };
            reading.status = Status::Fail;
            reading.message = format!(
                "{name}: {} of {} witnesses missed what they pin{missed}{uncovered}{unread}",
                against.len(),
                self.judged.len()
            );
        } else if self.judged.is_empty() {
            reading.status = Status::Pass;
            reading.message = format!("{name}: no witness, and no kernel or native that needs one");
        } else {
            // Every witness met what it pins. Whether that is a reading of
            // the engine is the runner's to say: without the engine's own
            // binary the pinned oracle ran them, which measures the fences
            // and the oracle, and a green line must not stand where the
            // engine's belongs.
            match runner {
                Runner::Engine(_) => {
                    reading.status = Status::Pass;
                    reading.message = format!(
                        "{name}: {} witnesses pass over {} kernels or natives",
                        self.judged.len(),
                        self.subjects
                    );
                }
                Runner::Control(binary) => {
                    reading.status = Status::Skip;
                    reading.message = format!(
                        "{name}: {} witnesses agree with {}, the pinned oracle; a control of the fences, not a reading of the engine",
                        self.judged.len(),
                        binary.display()
                    );
                }
                Runner::Absent(why) => {
                    reading.status = Status::Skip;
                    reading.message = format!(
                        "{name}: {} witnesses agree with the pinned oracle, and the engine ran none; {why}",
                        self.judged.len()
                    );
                }
            }
        }
        reading.stamp(&Stamp::take(root, &krate.path())?);
        Ok(reading)
    }
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
    let mut readings = Vec::new();
    for krate in &manifest.crates {
        if !ask.crates.is_empty() && !ask.crates.iter().any(|n| n == &krate.name) {
            continue;
        }
        let mut reading = Reading::new(INSTRUMENT, &krate.name);
        if let Some(rule) = rule {
            reading.rule.clone_from(&rule.id);
            reading.adr = rule.adr.adr_field();
        }
        if krate.name == crate::done::HARNESS {
            // The harness measures the engine and carries no witness of its
            // own; `done` asks it for none either (§4.5).
            reading.message = format!("{}: the harness carries no witness", krate.name);
            readings.push(reading);
            continue;
        }
        if !sources::exists(root, krate) {
            reading.message = format!("{}: not yet created", krate.name);
            readings.push(reading);
            continue;
        }
        let found = extract(root, krate, rule)?;
        let tally = judge(&manifest.pin, registry, &runner, ask, &found)?;
        for line in &tally.problems {
            println!("bork {line}");
        }
        for judged in &tally.judged {
            if judged.judgement != Judgement::Pass {
                println!(
                    "{} {} at {}{}: {}",
                    judged.judgement.as_str(),
                    judged.id,
                    judged.at,
                    if judged.subject.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", judged.subject)
                    },
                    judged.note
                );
            }
        }
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
                tally.judged.len()
            );
        }
        readings.push(tally.reading(root, krate, rule, &runner)?);
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
        let Ok(text) = fs::read_to_string(&file) else {
            // A source that is not text is not one this grammar reads;
            // `budget` is the sensor that says so.
            continue;
        };
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
    pin: &Pin,
    registry: &Registry,
    runner: &Runner,
    ask: &Ask,
    found: &Extracted,
) -> Result<Tally> {
    let mut tally = Tally {
        subjects: found.subjects.len(),
        missing: found
            .subjects
            .iter()
            .filter(|s| !s.witnessed)
            .map(fence::Subject::at)
            .collect(),
        problems: found.problems.iter().map(fence::Problem::at_line).collect(),
        ..Tally::default()
    };
    if !found.witnesses.is_empty() {
        // The pin refuses the whole run on a digest mismatch (§4.3), so it
        // is asserted once, before the first snippet and not per witness.
        pin.assert_digest()?;
    }
    for witness in &found.witnesses {
        tally.wall += witness.wall.unwrap_or_default();
        let judged = judge_one(pin, registry, runner, ask, witness, &mut tally)?;
        tally.judged.push(judged);
    }
    Ok(tally)
}

/// One witness, end to end: the oracle's run, the status `--accept` may
/// capture from it, the run of record, and the verdict.
fn judge_one(
    pin: &Pin,
    registry: &Registry,
    runner: &Runner,
    ask: &Ask,
    witness: &Witness,
    tally: &mut Tally,
) -> Result<Judged> {
    let oracle_out = match oracle::run(pin, Program::Snippet(&witness.snippet), None) {
        Ok(out) => out,
        Err(trouble) => {
            return Ok(Judged::new(
                witness,
                Judgement::Bork,
                format!("the oracle did not run it: {trouble:#}"),
            ));
        }
    };
    let Some(status) = oracle_out.exit else {
        return Ok(Judged::new(
            witness,
            Judgement::Bork,
            "the oracle ended on a signal, so there is no status to pin",
        ));
    };

    // The bare `exit` is the opt-in, and the only thing `--accept` writes:
    // a status the fence already pins and the oracle no longer gives is a
    // difference for the registry or for a fix, never one to repin over
    // (design §8, the paragraph on witnesses in rustdoc).
    let mut exit = witness.exit;
    if ask.accept && exit == Exit::Bare {
        tally.captured.push(Captured {
            file: witness.file.clone(),
            line: witness.fence,
            was: witness.fence_line.clone(),
            now: fence::pinned(&witness.fence_line, status),
        });
        exit = Exit::Pinned(status);
    }
    if exit == Exit::Bare {
        return Ok(Judged::new(
            witness,
            Judgement::Unpinned,
            format!(
                "the fence's `exit` is bare and the oracle exits {status}; `xtask witness --accept` writes it, and nothing else does"
            ),
        ));
    }

    let ran = match runner {
        Runner::Engine(binary) => oracle::run_under(
            pin,
            &Invocation::new(binary, Program::Snippet(&witness.snippet)),
        )
        .with_context(|| format!("running {} through {}", witness.at(), binary.display()))?,
        _ => oracle_out,
    };
    let met = match met(pin, witness, exit, &ran) {
        Ok(met) => met,
        Err(trouble) => {
            return Ok(Judged::new(
                witness,
                Judgement::Bork,
                format!("the pinned stream is not one this engine can match: {trouble:#}"),
            ));
        }
    };
    let id = witness.id();
    let expected = shown(
        &witness.stdout,
        witness.stderr.as_deref().unwrap_or_default(),
        exit.asserted(),
    );
    let actual = shown(
        &String::from_utf8_lossy(&ran.stdout),
        &String::from_utf8_lossy(&ran.stderr),
        ran.exit,
    );
    Ok(
        match oracle::verdict(registry, &id, met, &expected, &actual) {
            Compared::Same => Judged::new(witness, Judgement::Pass, ""),
            Compared::Diverged(row) => Judged::new(witness, Judgement::Diverged, row.reason),
            Compared::Differs(difference) => Judged::new(
                witness,
                Judgement::Fail,
                format!("what the fences pin is not what ran:\n{difference}"),
            ),
        },
    )
}

/// Whether a run meets what the fences pin: the two streams as the phpt
/// runner matches an `--EXPECTF--` section, since a witness's output lines
/// carry the same wildcards (design §8), and the status exactly.
fn met(pin: &Pin, witness: &Witness, exit: Exit, ran: &Output) -> Result<bool> {
    if ran.exit != exit.asserted() {
        return Ok(false);
    }
    let stdout = Expectation::Format(normalised(pin, &witness.stdout));
    if !stdout.met(&ran.stdout)? {
        return Ok(false);
    }
    let stderr = Expectation::Format(normalised(
        pin,
        witness.stderr.as_deref().unwrap_or_default(),
    ));
    stderr.met(&ran.stderr)
}

/// A pinned stream under the recipe's normalisation, which is what the run's
/// own streams have been through; the oracle module owns the rule.
fn normalised(pin: &Pin, text: &str) -> String {
    let bytes = oracle::normalise(&pin.recipe.normalise, text.as_bytes().to_vec());
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The three streams of one run as one document: what a difference is shown
/// as, and never a format anything reads back.
fn shown(stdout: &str, stderr: &str, exit: Option<i32>) -> Vec<u8> {
    let status = exit.map_or_else(|| "a signal".to_owned(), |code| code.to_string());
    format!("status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}\n").into_bytes()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shown_document_carries_the_status_and_both_streams() {
        let text = String::from_utf8(shown("out", "err", Some(3))).unwrap();
        assert_eq!(text, "status 3\nstdout:\nout\nstderr:\nerr\n");
        let signal = String::from_utf8(shown("", "", None)).unwrap();
        assert!(signal.starts_with("status a signal"), "{signal}");
    }

    #[test]
    fn at_most_five_names_are_listed_and_the_rest_counted() {
        assert_eq!(listed(&["a", "b"]), "a, b");
        assert_eq!(
            listed(&["a", "b", "c", "d", "e", "f"]),
            "a, b, c, d, e and 1 more"
        );
    }
}
