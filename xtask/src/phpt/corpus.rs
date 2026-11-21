//! The corpus reading (harness-design.md §7): what a run that set out for
//! the whole corpus came to, one reading beside the subsets', scoped
//! [`CORPUS`] with the run's suffixes (`php-src@control`, `php-src@force`)
//! so a control or a tier lane never folds into the engine's. Only
//! `--full` writes it; a subset or the per-commit lane writes none, and
//! `done` is owed none, since the scope maps to the whole workspace.
//!
//! **Each test once.** A test is counted by the id its verdicts carry:
//! its path in the corpus or, for a test a `--REDIRECTTEST--` ran, its
//! path and the test that redirected it, so each PDO driver's run of
//! `ext/pdo/tests` is tests of its own and the driverless run's skip is
//! its own id. An id judged twice is met only where every judgement of it
//! is. `files` counts the distinct paths, held to the pin's
//! `[source] phpt_files`.
//!
//! **Whole.** The run is whole where every subset it set out for gave its
//! verdicts, every test found was judged ([`Tally::whole`], the one rule a
//! subset's kept run is held to as well) and the files found are the ones
//! the pin counts. A whole run's status is the tally's over the union; any
//! other is a skip, `whole = 0`, naming what is missing, and the run exits
//! as one that could not run to its end.
//!
//! [`Tally::whole`]: crate::tally::Tally::whole

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::Result;

use super::{CORPUS, INSTRUMENT, Judged, Judgement, Lane, Scope, Subset, Tally, numbers, redirect};
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::Runner;
use crate::reading::{Reading, listed, plural};
use crate::tally::{Judgement as _, Outcome};

/// The subsets' verdicts as the run takes them, one per id.
#[derive(Debug, Default)]
pub(crate) struct Union {
    /// Each id's verdict: its first, or the first that did not meet.
    judged: BTreeMap<String, Judged>,
    /// The tests found that nothing judged.
    not_run: usize,
    /// The subsets whose verdicts were taken, by scope.
    taken: BTreeSet<String>,
}

impl Union {
    /// One subset's verdicts folded in.
    pub(crate) fn add(&mut self, subset: &Subset, tally: Tally) {
        self.not_run += tally.found.saturating_sub(tally.judged.len());
        self.taken.insert(subset.scope.clone());
        for judged in tally.judged {
            let kept = self.judged.get(&judged.id);
            if kept.is_none_or(|kept| kept.judgement.met() && !judged.judgement.met()) {
                self.judged.insert(judged.id.clone(), judged);
            }
        }
    }
}

/// What a run that set out for the whole corpus did not take of it.
#[derive(Debug)]
struct Short {
    /// The subsets of the run that gave no verdicts, by scope.
    unrun: Vec<String>,
    /// The corpus files found, and the count the pin holds them to.
    files: (usize, u64),
    /// The tests found and judged by nothing.
    unjudged: u64,
}

impl Short {
    /// What is missing, or none where the run was whole.
    fn of(unrun: Vec<String>, files: (usize, u64), tally: &Tally) -> Option<Short> {
        let whole = unrun.is_empty() && files.0 as u64 == files.1 && tally.whole();
        (!whole).then(|| Short {
            unrun,
            files,
            unjudged: tally.not_run(),
        })
    }
}

impl fmt::Display for Short {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut said = Vec::new();
        if !self.unrun.is_empty() {
            let n = self.unrun.len();
            said.push(format!(
                "{n} subset{} unrun: {}",
                plural(n),
                listed(&self.unrun)
            ));
        }
        let (found, pinned) = self.files;
        if found as u64 != pinned {
            said.push(format!(
                "{found} file{} found of the {pinned} `[source] phpt_files` pins",
                plural(found)
            ));
        }
        if self.unjudged > 0 {
            said.push(format!("{} tests found and not judged", self.unjudged));
        }
        write!(f, "{}", said.join("; "))
    }
}

/// The corpus reading of a run over `subsets` under `runner`, and whether
/// the run took the whole corpus, the reading naming what it did not.
pub(crate) fn reading(
    union: Union,
    subsets: &[Subset],
    (runner, lane): (&Runner, Option<Lane>),
    (manifest, snapshot): (&Manifest, &Snapshot),
) -> Result<(Reading, bool)> {
    let mut files = BTreeSet::new();
    for subset in subsets {
        files.extend(subset.tests()?.iter().map(|test| subset.under(test)));
    }
    let unrun = (subsets.iter().map(|subset| &subset.scope))
        .filter(|scope| !union.taken.contains(*scope))
        .cloned()
        .collect();
    let again = (union.judged.keys())
        .filter(|id| redirect::redirected(id))
        .count();
    let found = union.judged.len() + union.not_run;
    let tally = Tally {
        judged: union.judged.into_values().collect(),
        found,
    };
    let pin = manifest.pin()?;
    let short = Short::of(unrun, (files.len(), pin.source.phpt_files), &tally);
    let scope = Scope {
        control: matches!(runner, Runner::Control(_)),
        lane,
        ..Scope::of(CORPUS)
    }
    .to_string();
    let missing = short.as_ref().map(|short| format!("not whole: {short}"));
    let outcome = match &missing {
        Some(missing) => Outcome::Absent(missing),
        None => tally.outcome(runner),
    };
    let words = match &outcome {
        Outcome::Absent(why) | Outcome::OracleAbsent(why) => (*why).to_owned(),
        Outcome::Failed => format!(
            "{} missed their expectation{}",
            tally.against().len(),
            tally.missed()
        ),
        Outcome::Control(binary) => format!(
            "{} differ under {}, the pinned oracle; a control of the runner, not a reading of the engine{}",
            tally.against().len(),
            binary.display(),
            tally.missed()
        ),
        Outcome::NothingRan => "none met its expectation".to_owned(),
        Outcome::Passed => format!("{} met", tally.met()),
    };
    let stamp = (runner.stamp())
        .map(|engine| format!("; run by {engine}"))
        .unwrap_or_default();
    let message = format!(
        "{scope}: {} files, {found} tests ({again} run again under a driver); {words}{stamp}",
        files.len()
    );
    let stamped = Reading::for_scope(INSTRUMENT, &scope, manifest, snapshot)?.under(pin);
    let mut reading = tally.reading(stamped, &outcome, message)?;
    reading.profile = runner.profile();
    reading.count(numbers::WHOLE, u64::from(short.is_none()))?;
    reading.count(numbers::FILES, files.len() as u64)?;
    reading.count(numbers::TESTS, found as u64)?;
    reading.count(numbers::MET, tally.met())?;
    for judgement in Judgement::ALL {
        reading.count(judgement.key(), tally.count(judgement))?;
    }
    (reading.numbers).insert("percent".to_owned(), tally.percent().into());
    Ok((reading, short.is_none()))
}
