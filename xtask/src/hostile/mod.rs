//! `xtask hostile`, the hostile lane (harness row H15, #4265): a fixed
//! corpus of PHP programs whose error handler disturbs the opline that
//! raised the diagnostic, mid-step, each held to the oracle. The corpus is
//! `data/hostile.toml` ([`corpus`]); each program runs under the oracle's
//! recipe on the pinned php and on the engine's `php` built in the lanes'
//! profile, and the engine's again under `[regime.jit-force]`.
//!
//! **What refuses a run** (Round 1, Q5 (b)). A fault, under either pass:
//! a signal, the recipe's limit, or a line in which the engine says an
//! invariant of its own failed ([`judge::Fault`]). And a program that met
//! php at the merge base and differs now: the interpreter's verdicts are
//! kept per engine tree ([`kept`]), and the run is held to the one kept at
//! the base's. A difference otherwise, one standing at the base, one of a
//! program the base's run did not hold, or one where no run is kept at the
//! base, is counted and refuses nothing. A difference a person accepts is a
//! `hostile:<id>` a `trace:` row covers, with no kind of its own (Round 4,
//! Q33); growing a standing row's list is a person's hand.
//!
//! **The compiled tier** (Round 3, Q24 (a)). The second pass runs the
//! engine's `php`, which carries the tier in every build that keeps its
//! default features (ADR-0017, #3676), under `[regime.jit-force]`, and
//! judges faults alone: a byte verdict against the interpreter is what
//! design §3.2 forbids, and one against the oracle would double the kept
//! runs for a pass that differs nowhere the interpreter does not.
//!
//! **What it writes.** One reading per crate that owes it, [`OWING`],
//! each stamped over its crate and all carrying the one run's verdict
//! (Round 3, Q24 (a)); the oracle's side kept per pin, so a run after the
//! first under a pin runs the engine alone (Round 2, Q31 (a)); and the
//! interpreter's verdicts as the run [`kept`] holds the next one to.

mod corpus;
mod judge;
mod kept;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::divergence::Registry;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::built_from::BuiltFrom;
use crate::oracle::hosts::CLI;
use crate::oracle::{ENGINE, Invocation, Oracle, PastLimit, Program, profile};
use crate::reading::{Reading, Status, joined};
use crate::track::{self, Track};
use corpus::Corpus;
use judge::{Came, Fault, Found, Judged, Said};
use kept::{Base, Run};

/// The instrument's name, the reading's `instrument` field.
pub const INSTRUMENT: &str = "hostile";

/// The crates whose change owes a fresh reading, as Round 3's Q24 (a)
/// ruled: what a free, a table or a step is made of, and the tier.
pub const OWING: [&str; 5] = [
    "php_heap",
    "php_unit",
    "php_compile",
    "php_engine",
    "php_jit",
];

/// The regime the compiled tier's pass runs under.
const TIER: &str = "jit-force";

/// `xtask hostile`: [`run`] under the tracked registry, its readings
/// recorded.
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &crate::reading::Store,
) -> Result<()> {
    let registry = Registry::load(root)?;
    crate::record(store, &run(root, manifest, &registry)?)
}

/// Run the corpus: one reading per crate of [`OWING`], the run kept.
pub fn run(root: &Path, manifest: &Manifest, registry: &Registry) -> Result<Vec<Reading>> {
    let started = Instant::now();
    let pin = manifest.pin()?;
    let snapshot = Snapshot::disk(root)?;
    let corpus = Corpus::load(root)?;
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
    let engine = match profile::build(root, &CLI.package(), ENGINE, None, None) {
        Ok(engine) => engine,
        Err(why) => {
            let message = format!("the engine's php did not build: {why:#}");
            said(&mut readings, Status::Fail, &message);
            return Ok(readings);
        }
    };
    let sides = Sides {
        oracle: &oracle,
        engine: &engine,
        kept: root.join(kept::RUNS).join("oracle").join(pin.digest()),
        registry,
    };
    let found = sides.found(&corpus)?;
    let base = Track::of(root, None)?.base;
    let commit = track::short(&base).to_owned();
    let at_base = kept::at(root, &base, pin.digest())?;
    let ids = corpus.programs.iter().map(|p| p.id.as_str());
    let verdicts = match &at_base {
        Base::Kept(run) => Some((commit.as_str(), &run.verdicts)),
        Base::Stale | Base::None => None,
    };
    let held = Said::of(ids.clone().zip(&found), verdicts);
    let built_from = BuiltFrom::of(&engine).keyed(root, &manifest.layout());
    let tree = snapshot.tracked_over(&built_from)?;
    let run = Run {
        commit: track::short(&crate::git::head_commit(root)?).to_owned(),
        recipe: pin.digest().to_owned(),
        corpus: corpus.digest.clone(),
        built_from,
        verdicts: (ids.zip(&found))
            .filter_map(|(id, found)| match found {
                Found::Held { interpreter, .. } => Some((id.to_owned(), interpreter.verdict())),
                Found::Unheld(_) => None,
            })
            .collect(),
    };
    let file = run.keep(root, &tree)?;
    let wall = started.elapsed();
    println!(
        "{INSTRUMENT}: {} programs in {:.1}s; the run is kept at {}",
        found.len(),
        wall.as_secs_f64(),
        file.strip_prefix(root).unwrap_or(&file).display()
    );
    let message = message(&held, found.len(), &commit, &at_base);
    let status = Status::of(held.refusals.is_empty());
    let mut forms = std::collections::BTreeMap::new();
    for program in &corpus.programs {
        *forms.entry(program.form.name()).or_insert(0) += 1;
    }
    for reading in &mut readings {
        numbers(reading, &held, found.len())?;
        reading.count("wall_ms", crate::reading::millis(wall))?;
        for (form, n) in &forms {
            reading.group(form, *n)?;
        }
    }
    said(&mut readings, status, &message);
    Ok(readings)
}

/// The counts every reading of a run carries.
fn numbers(reading: &mut Reading, held: &Said, programs: usize) -> Result<()> {
    let counts = [
        ("programs", programs as u64),
        ("met", held.met),
        ("accepted", held.accepted),
        ("differs", held.differs),
        ("regressed", held.regressed),
        ("faults", held.faults),
        ("tier_faults", held.tier_faults),
        ("unheld", held.unheld),
        ("held_at_base", held.held_at_base),
    ];
    for (key, n) in counts {
        reading.count(key, n)?;
    }
    Ok(())
}

/// What a run says, after the crate's name: the refusals first where
/// there are any, then the counts and what the base held.
fn message(held: &Said, programs: usize, commit: &str, base: &Base) -> String {
    let counted = format!(
        "{programs} programs, {} met php, {} accepted by a row, {} differ, {} of them met php at {commit}; {} faults, {} under [regime.{TIER}]",
        held.met, held.accepted, held.differs, held.regressed, held.faults, held.tier_faults
    );
    let at_base = match base {
        Base::Kept(_) => None,
        Base::Stale => Some(format!(
            "the runs kept at {commit}'s engine are under another pin, so no difference is held against them; a run taken in a checkout at {commit} keeps one under this pin"
        )),
        Base::None => Some(format!(
            "no run is kept at {commit}'s engine, so no difference is held against it; a run taken in a checkout at {commit} keeps one"
        )),
    };
    let refused = (!held.refusals.is_empty()).then(|| {
        let n = held.refusals.len();
        format!(
            "{n} refuse{} the run: {}",
            if n == 1 { "s" } else { "" },
            held.refusals.join("; ")
        )
    });
    joined(refused.into_iter().chain([counted]).chain(at_base), "; ")
}

/// What both sides of a run run under, and where the oracle's side of
/// each program is kept under the pin.
struct Sides<'a> {
    oracle: &'a Oracle<'a>,
    engine: &'a Path,
    kept: PathBuf,
    registry: &'a Registry,
}

impl Sides<'_> {
    /// Each program of `corpus` as the run finds it, in the corpus's
    /// order, run as many at once as the seat's share of the machine.
    fn found(&self, corpus: &Corpus) -> Result<Vec<Found>> {
        let programs = &corpus.programs;
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<Result<Found>>>> =
            programs.iter().map(|_| Mutex::new(None)).collect();
        std::thread::scope(|scope| {
            for _ in 0..crate::seat::jobs().get().min(programs.len()) {
                scope.spawn(|| {
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let Some(program) = programs.get(n) else {
                            break;
                        };
                        *crate::locked(&slots[n]) = Some(self.one(program));
                    }
                });
            }
        });
        let taken = slots.into_iter().zip(programs);
        taken
            .map(|(slot, program)| {
                let found = slot
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                found
                    .with_context(|| format!("no thread took {}", program.id))?
                    .with_context(|| format!("running {}", program.id))
            })
            .collect()
    }

    /// One program: the oracle's side, kept or run now; the engine's
    /// under the interpreter, held to it; the engine's under the tier.
    fn one(&self, program: &corpus::Program) -> Result<Found> {
        let theirs = match self.theirs(program)? {
            Ok(theirs) => theirs,
            Err(why) => return Ok(Found::Unheld(why)),
        };
        let interpreter = self.came(self.engine, &program.code, &[])?;
        let interpreter = Judged::of(self.registry, &program.id, &theirs, &interpreter);
        let tier = self.came(self.engine, &program.code, &[TIER])?;
        Ok(Found::Held {
            interpreter,
            tier: Fault::of(&tier, &theirs),
        })
    }

    /// The oracle's side of `program`, [`judge::seen`], kept under the pin
    /// and the code's blob id, so an edited program runs again; or how the
    /// oracle's run ended where it did not complete it, which is not kept.
    fn theirs(&self, program: &corpus::Program) -> Result<Result<Vec<u8>, String>> {
        let code = crate::git::blob_id(program.code.as_bytes());
        let file = self.kept.join(format!("{}-{code}.out", program.id));
        if let Ok(theirs) = std::fs::read(&file) {
            return Ok(Ok(theirs));
        }
        let php = &self.oracle.pin().oracle.path;
        let observed = match self.came(php, &program.code, &[])? {
            Came::PastLimit(limit) => return Ok(Err(format!("past the recipe's {limit}s"))),
            Came::Observed(observed) => observed,
        };
        if let crate::oracle::Ended::Signal(signal) = observed.exit {
            return Ok(Err(format!("on signal {signal}")));
        }
        let theirs = judge::seen(&observed);
        crate::make_parent(&file)?;
        crate::reading::replace(&file, &theirs)?;
        Ok(Ok(theirs))
    }

    /// `code` run by `binary` under the recipe and `regimes`.
    fn came(&self, binary: &Path, code: &str, regimes: &[&str]) -> Result<Came> {
        let ran = self.oracle.run_under(&Invocation {
            regimes,
            ..Invocation::new(binary, Program::Snippet(code))
        });
        match ran {
            Ok(observed) => Ok(Came::Observed(observed)),
            Err(why) => match why.downcast_ref::<PastLimit>() {
                Some(past) => Ok(Came::PastLimit(past.limit_s)),
                None => Err(why),
            },
        }
    }
}
