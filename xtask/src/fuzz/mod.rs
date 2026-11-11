//! `xtask fuzz`, the differential lane (harness §4.2, §7's `fuzz` row,
//! nightly; design §8): programs held to the
//! pinned php, each run under the oracle's recipe on php and on the
//! engine's `php`, under the interpreter and again under
//! `[regime.jit-force]`, as the hostile lane runs its own.
//!
//! **What a run holds.** The corpus's programs as they stand, a directory
//! of `.php` files ([`CORPUS`] unless one is named), then as many again
//! as asked, each drawn from the run's seed and its place: the inherited
//! grammar generator's or a mutation of a corpus program ([`generate`]).
//! The seed is in the reading, so `--seed` draws the same programs again.
//!
//! **How a program is judged.** Both sides' first stream, second stream
//! and exit, as one text ([`judge::seen`]), through [`crate::oracle::compare`]
//! under `fuzz:<blob id of the program>`; a signal, the recipe's limit or
//! a line saying an invariant of the engine failed is a fault whatever
//! php wrote ([`judge::Fault`]). A program whose oracle run php did not
//! complete holds nothing and is counted. A pass under the compiled tier
//! is judged only where the interpreter met php, so a finding there is
//! the tier's own.
//!
//! **A finding.** A program the engine differs on or faults under is
//! minimised: its lines are dropped a run at a time for as long as the
//! same kind of disagreement stands, as many trials as [`TRIALS`]. What is
//! left is kept in the store at [`FOUND`]`/<seed>/<label>.php`, and the
//! reading names that path and the first line that differs. The fuzzer
//! proposes no registry row itself: `docs/divergence.toml` is a person's
//! file, and the registry has no kind whose ids name a fuzz program, so
//! the reading names the finding and stops there.
//!
//! **What bounds a run.** Each program, the recipe's limit; a trap never
//! lands on a line that names a loop; a time-out is kept whole rather
//! than minimised, since each trial would run to the limit; and the run's
//! budget, `--minutes`, past which no program starts and no finding is
//! minimised. The reading says how many programs and findings each cut.
//!
//! **What it writes.** One reading per crate of [`OWING`], each stamped
//! over its crate and all carrying the one run's verdict, as `hostile`'s
//! do: a fail while a finding stands or where no program was held to
//! php, a skip where the pinned php is not
//! on this machine.

mod generate;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::hostile::judge::{self, Came, Fault, Judged};
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::hosts::CLI;
use crate::oracle::{ENGINE, Ended, Oracle, profile};
use crate::reading::{Reading, Status, joined};
use generate::Rng;

/// The instrument's name, the reading's `instrument` field.
pub const INSTRUMENT: &str = "fuzz";

/// The crates a run's verdict is written over: those a program goes
/// through from its text to what it prints, the tier among them.
pub(crate) const OWING: [&str; 6] = [
    "php_syntax",
    "php_heap",
    "php_unit",
    "php_compile",
    "php_engine",
    "php_jit",
];

/// The corpus a run holds where none is named, relative to the root.
pub(crate) const CORPUS: &str = "data/fuzz";

/// Where a run keeps its minimised findings, relative to the root.
pub(crate) const FOUND: &str = "target/harness/fuzz";

/// What a fuzz program's divergence key starts with.
const PREFIX: &str = "fuzz:";

/// How many runs of a program minimising one finding may take.
const TRIALS: usize = 64;

/// What one `xtask fuzz` run was asked for.
#[derive(Debug, Clone, clap::Args)]
pub struct Ask {
    /// The corpus: a directory of `.php` programs, each held as it stands
    /// and mutated; default `data/fuzz`.
    #[arg(long, value_name = "DIR")]
    pub corpus: Option<PathBuf>,
    /// The seed the drawn programs come from; default the clock's
    /// seconds. The reading names it.
    #[arg(long)]
    pub seed: Option<u32>,
    /// How many programs to draw beside the corpus's own.
    #[arg(long, value_name = "N", default_value_t = 32)]
    pub programs: usize,
    /// The run's budget in minutes: a program not started by then is not
    /// run, and a finding met then is kept whole. The reading says how
    /// many of each.
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub minutes: u64,
}

/// `xtask fuzz`: [`run`] under the tracked registry, its readings
/// recorded.
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &crate::reading::Store,
    ask: &Ask,
) -> Result<()> {
    let registry = Registry::load(root)?;
    crate::record(store, &run(root, manifest, &registry, ask)?)
}

/// Run the lane: one reading per crate of [`OWING`].
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    let started = Instant::now();
    let pin = manifest.pin()?;
    let snapshot = Snapshot::disk(root)?;
    let mut readings = Vec::new();
    for name in OWING {
        let krate = manifest.crate_named(name).with_context(|| {
            format!("the manifest has no crate {name}, which owes {INSTRUMENT}")
        })?;
        readings.push(Reading::for_crate(INSTRUMENT, krate, &snapshot)?.under(pin));
    }
    let said = |readings: &mut Vec<Reading>, status: Status, message: &str| {
        for reading in readings.iter_mut() {
            reading.status = status;
            reading.message = format!("{}: {message}", reading.scope);
        }
    };
    if let Some(why) = pin.oracle.absent() {
        said(&mut readings, Status::Skip, &format!("nothing ran; {why}"));
        return Ok(readings);
    }
    let oracle = pin.oracle();
    oracle.asserted()?;
    let corpus = Corpus::load(&root.join(ask.corpus.as_deref().unwrap_or(Path::new(CORPUS))))?;
    let engine = match profile::build(root, &CLI.package(), ENGINE, None, None) {
        Ok(engine) => engine,
        Err(why) => {
            let message = format!("the engine's php did not build: {why:#}");
            said(&mut readings, Status::Fail, &message);
            return Ok(readings);
        }
    };
    let seed = ask.seed.unwrap_or_else(clock);
    let programs = corpus.drawn(seed, ask.programs);
    let sides = Sides {
        oracle: &oracle,
        php: &pin.oracle.path,
        engine: &engine,
        registry,
        deadline: started + Duration::from_secs(ask.minutes.saturating_mul(60)),
    };
    let hunt = sides.hunt(&programs, &root.join(FOUND).join(seed.to_string()))?;
    let wall = started.elapsed();
    let message = hunt.message(root, seed, &corpus, ask);
    for reading in &mut readings {
        hunt.numbers(reading, seed, &programs)?;
        reading.count("wall_ms", crate::reading::millis(wall))?;
    }
    said(&mut readings, hunt.status(), &message);
    Ok(readings)
}

/// The clock's seconds, the seed a run takes where none is asked for.
fn clock() -> u32 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |since| {
        u32::try_from(since.as_secs()).unwrap_or(u32::MAX)
    })
}

/// The corpus: its programs by their file's stem, in the stems' order,
/// and the directory it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Corpus {
    pub dir: PathBuf,
    pub programs: Vec<(String, String)>,
}

impl Corpus {
    /// The `.php` files of `dir`, refused where it holds none.
    pub(crate) fn load(dir: &Path) -> Result<Corpus> {
        let mut programs = Vec::new();
        for path in crate::entries(dir)? {
            if path.extension().is_some_and(|e| e == "php") {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                programs.push((stem.into_owned(), crate::read(&path)?));
            }
        }
        if programs.is_empty() {
            bail!("the corpus {} holds no .php program", dir.display());
        }
        programs.sort();
        Ok(Corpus {
            dir: dir.to_path_buf(),
            programs,
        })
    }

    /// The programs a run under `seed` holds: the corpus's as they stand,
    /// then `n` drawn, the `i`th from `seed` and `i` alone, a corpus
    /// program mutated or one the grammar generates, half and half.
    pub(crate) fn drawn(&self, seed: u32, n: usize) -> Vec<Drawn> {
        let mut drawn: Vec<Drawn> = (self.programs.iter())
            .map(|(stem, code)| Drawn {
                label: stem.clone(),
                code: code.clone(),
            })
            .collect();
        for i in 0..n {
            let mut rng = Rng::new(u64::from(seed) << 32 | i as u64);
            let (label, code) = if rng.below(2) == 0 {
                let (stem, code) = &self.programs[rng.below(self.programs.len())];
                (format!("m{i}-{stem}"), generate::mutated(code, &mut rng))
            } else {
                (format!("g{i}"), generate::program(&mut rng))
            };
            drawn.push(Drawn { label, code });
        }
        drawn
    }
}

/// One program of a run: what names it in the run, its corpus file's
/// stem, `g<i>` where the grammar drew it, `m<i>-<stem>` where it is a
/// mutation of that corpus program; and its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Drawn {
    pub label: String,
    pub code: String,
}

/// The two passes the engine runs a program under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    Interpreter,
    /// Under `[regime.jit-force]`.
    Tier,
}

impl Pass {
    fn regimes(self) -> &'static [&'static str] {
        match self {
            Pass::Interpreter => &[],
            Pass::Tier => &["jit-force"],
        }
    }
}

/// What both sides of a run run as, the registry `compare` reads, and
/// when the run's budget is spent.
pub(crate) struct Sides<'a> {
    pub oracle: &'a Oracle<'a>,
    pub php: &'a Path,
    pub engine: &'a Path,
    pub registry: &'a Registry,
    pub deadline: Instant,
}

/// How one program's run came out.
enum Ran {
    /// The run's budget was spent before it started.
    Cut,
    /// The oracle did not complete it.
    Unheld,
    /// Its verdict under the interpreter, and under the tier where the
    /// interpreter met php.
    Held(Judged, Option<Judged>),
}

/// One program the engine disagrees with php on, minimised: where it is
/// kept, the pass, and the verdict on what was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub path: PathBuf,
    pub pass: Pass,
    pub judged: Judged,
}

/// What a run found: the counts and each finding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Hunt {
    pub met: u64,
    pub accepted: u64,
    pub unheld: u64,
    /// Programs not run, the run's budget spent before they started.
    pub cut: u64,
    /// Findings kept whole: a time-out, which minimising would spend the
    /// recipe's limit on per trial, or one met with the budget spent.
    pub whole: u64,
    pub findings: Vec<Finding>,
}

/// Whether `judged` is a disagreement a finding is made of.
fn disagrees(judged: &Judged) -> bool {
    !matches!(judged, Judged::Met | Judged::Accepted(_))
}

impl Sides<'_> {
    /// Each of `programs` run and judged, as many at once as the seat's
    /// share of the machine; each finding minimised and kept under `dir`.
    pub(crate) fn hunt(&self, programs: &[Drawn], dir: &Path) -> Result<Hunt> {
        let found = crate::tally::each_beside(programs, |p| self.found(&p.code))?;
        let mut hunt = Hunt::default();
        for (program, found) in programs.iter().zip(found) {
            let (interpreter, tier) =
                match found.with_context(|| format!("running {}", program.label))? {
                    Ran::Cut => {
                        hunt.cut += 1;
                        continue;
                    }
                    Ran::Unheld => {
                        hunt.unheld += 1;
                        continue;
                    }
                    Ran::Held(interpreter, tier) => (interpreter, tier),
                };
            let (pass, judged) = match (&interpreter, tier) {
                (judged, _) if disagrees(judged) => (Pass::Interpreter, interpreter),
                (_, Some(tier)) if disagrees(&tier) => (Pass::Tier, tier),
                (Judged::Accepted(_), _) => {
                    hunt.accepted += 1;
                    continue;
                }
                _ => {
                    hunt.met += 1;
                    continue;
                }
            };
            let tail = if pass == Pass::Tier { "-tier" } else { "" };
            let path = dir.join(format!("{}{tail}.php", program.label));
            let minimised = self.minimised(&program.code, pass, &judged)?;
            hunt.whole += u64::from(minimised.is_none());
            let code = minimised.as_deref().unwrap_or(&program.code);
            crate::make_parent(&path)?;
            crate::reading::replace(&path, code.as_bytes())?;
            let judged = if minimised.is_some() {
                self.judged(code, pass)?.unwrap_or(judged)
            } else {
                judged
            };
            hunt.findings.push(Finding { path, pass, judged });
        }
        Ok(hunt)
    }

    /// `code` run and judged, unless the run's budget is spent.
    fn found(&self, code: &str) -> Result<Ran> {
        if Instant::now() >= self.deadline {
            return Ok(Ran::Cut);
        }
        let Some(theirs) = self.theirs(code)? else {
            return Ok(Ran::Unheld);
        };
        let interpreter = self.held(code, &theirs, Pass::Interpreter)?;
        let tier = if disagrees(&interpreter) {
            None
        } else {
            Some(self.held(code, &theirs, Pass::Tier)?)
        };
        Ok(Ran::Held(interpreter, tier))
    }

    /// `code` judged under `pass` alone; none where the oracle did not
    /// complete it.
    fn judged(&self, code: &str, pass: Pass) -> Result<Option<Judged>> {
        let Some(theirs) = self.theirs(code)? else {
            return Ok(None);
        };
        self.held(code, &theirs, pass).map(Some)
    }

    /// The oracle's side of `code`, none where a signal or the limit ended it.
    fn theirs(&self, code: &str) -> Result<Option<Vec<u8>>> {
        Ok(match judge::came(self.oracle, self.php, code, &[])? {
            Came::Observed(observed) if !matches!(observed.exit, Ended::Signal(_)) => {
                Some(judge::seen(&observed))
            }
            Came::Observed(_) | Came::PastLimit(_) => None,
        })
    }

    fn held(&self, code: &str, theirs: &[u8], pass: Pass) -> Result<Judged> {
        let came = judge::came(self.oracle, self.engine, code, pass.regimes())?;
        let key = format!("{PREFIX}{}", crate::git::blob_id(code.as_bytes()));
        Ok(Judged::keyed(self.registry, &key, theirs, &came))
    }

    /// `code` with as many of its lines dropped as leave the disagreement
    /// `judged` standing under `pass`, of the same kind: runs of lines
    /// halving down to one, within [`TRIALS`] runs and the run's budget.
    /// The opening tag stays. None for a time-out, each trial of which
    /// would run to the recipe's limit, and where the budget is spent.
    pub(crate) fn minimised(
        &self,
        code: &str,
        pass: Pass,
        judged: &Judged,
    ) -> Result<Option<String>> {
        let spent = || Instant::now() >= self.deadline;
        if matches!(judged, Judged::Fault(Fault::PastLimit(_))) || spent() {
            return Ok(None);
        }
        let render = |lines: &[&str]| {
            lines.iter().fold(String::new(), |mut out, line| {
                out.push_str(line);
                out.push('\n');
                out
            })
        };
        let mut lines: Vec<&str> = code.lines().collect();
        let head = usize::from(lines.first().is_some_and(|l| l.starts_with("<?php")));
        let mut chunk = lines.len().saturating_sub(head).div_ceil(2).max(1);
        let mut trials = 0;
        loop {
            let mut at = head;
            while at < lines.len() && trials < TRIALS && !spent() {
                let end = (at + chunk).min(lines.len());
                let fewer: Vec<&str> = lines[..at].iter().chain(&lines[end..]).copied().collect();
                trials += 1;
                let now = self.judged(&render(&fewer), pass)?;
                let same =
                    |now: &Judged| std::mem::discriminant(now) == std::mem::discriminant(judged);
                if now.as_ref().is_some_and(same) {
                    lines = fewer;
                } else {
                    at = end;
                }
            }
            if chunk == 1 || trials >= TRIALS || spent() {
                return Ok(Some(render(&lines)));
            }
            chunk = chunk.div_ceil(2);
        }
    }
}

impl Hunt {
    /// A pass where the run held a program to php and found nothing; a
    /// run whose every program the oracle did not complete compared
    /// nothing, and fails as one with a finding does.
    pub(crate) fn status(&self) -> Status {
        Status::of(self.findings.is_empty() && self.met + self.accepted > 0)
    }

    /// What a run says, after the crate's name: the findings first where
    /// there are any, then the counts and how to draw the programs again.
    fn message(&self, root: &Path, seed: u32, corpus: &Corpus, ask: &Ask) -> String {
        let drawn = ask.programs;
        let found = (!self.findings.is_empty()).then(|| {
            let each = self.findings.iter().map(|f| {
                let path = f.path.strip_prefix(root).unwrap_or(&f.path).display();
                let tier = if f.pass == Pass::Tier {
                    " under [regime.jit-force]"
                } else {
                    ""
                };
                format!("{path}{tier}: {}", worded(&f.judged))
            });
            format!(
                "{} finding{}: {}",
                self.findings.len(),
                crate::reading::plural(self.findings.len()),
                joined(each, "; ")
            )
        });
        let dir = corpus
            .dir
            .strip_prefix(root)
            .unwrap_or(&corpus.dir)
            .display();
        let counted = format!(
            "seed {seed}, {} programs of {dir} and {drawn} drawn, each held to php: {} met, {} accepted by a row, {} the oracle did not complete; `cargo xtask fuzz --seed {seed} --programs {drawn}` draws them again",
            corpus.programs.len(),
            self.met,
            self.accepted,
            self.unheld
        );
        let empty = (self.findings.is_empty() && self.met + self.accepted == 0)
            .then(|| "no program was held to php, so the run compared nothing".to_owned());
        let cut = (self.cut + self.whole > 0).then(|| {
            format!(
                "cut short: {} not run and {} finding{} kept whole, a time-out's or past the run's {} minutes",
                self.cut,
                self.whole,
                crate::reading::plural(usize::try_from(self.whole).unwrap_or(usize::MAX)),
                ask.minutes
            )
        });
        joined(
            found.into_iter().chain(empty).chain(cut).chain([counted]),
            "; ",
        )
    }

    /// The counts a run's readings carry.
    fn numbers(&self, reading: &mut Reading, seed: u32, programs: &[Drawn]) -> Result<()> {
        let tier = self
            .findings
            .iter()
            .filter(|f| f.pass == Pass::Tier)
            .count();
        let faults = (self.findings.iter()).filter(|f| matches!(f.judged, Judged::Fault(_)));
        let counts = [
            ("seed", u64::from(seed)),
            ("programs", programs.len() as u64),
            ("met", self.met),
            ("accepted", self.accepted),
            ("unheld", self.unheld),
            ("cut", self.cut),
            ("whole", self.whole),
            ("findings", self.findings.len() as u64),
            ("faults", faults.count() as u64),
            ("tier_findings", tier as u64),
        ];
        for (key, n) in counts {
            reading.count(key, n)?;
        }
        Ok(())
    }
}

/// A finding's verdict, as the reading words it.
fn worded(judged: &Judged) -> String {
    match judged {
        Judged::Differs(first) => format!("differs, {first}"),
        Judged::Fault(fault) => format!("faults, {fault}"),
        Judged::Moved(moved) => moved.clone(),
        Judged::Met | Judged::Accepted(_) => "met php once minimised".to_owned(),
    }
}
