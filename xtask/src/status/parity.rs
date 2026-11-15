//! The `parity` section (design §1, §12): met-of-target per suite
//! and in total, as design §1 says `xtask status` states it. A suite is a
//! subset parity counts, by the scope its reading is taken under:
//! `Zend/tests` and `tests`, the core's, and every `[[crate]]` row that
//! owns a subset, by the row's name, since the manifest derives a row for
//! every extension and SAPI the pinned php builds. Each is read off the
//! phpt reading that stands for it, as the `phpt` section picks it, and a
//! reading's facts are its row's ([`phpt::row::Row`]), the rendering
//! `--every` hands the dashboard. A suite counts in the total only where
//! its reading was held to a target; every other suite says which of the
//! other states it is in. The suites outside parity are named apart: those
//! `[parity] out` rules out, with the ruling, and those no row owns, which
//! the pinned php does not build: every `ext/*` and `sapi/*` suite of the
//! pinned tree that holds a test where the tree is on this machine,
//! and every suite a reading measured, which is all a machine
//! without the tree can name and the section says so. The section writes
//! no reading.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use super::{Table, cell, standing};
use crate::manifest::{Manifest, overlaps};
use crate::phpt::row::{Row, Target};
use crate::phpt::{self, Root, Subset};
use crate::reading::Reading;
use crate::reading::{self, Store};
use crate::status::PerReading;

/// The section: the suites inside parity, their total, the suites
/// outside it, and where the unbuilt among those were named from.
#[derive(Debug, Serialize)]
struct Parity {
    suites: Vec<Suite>,
    total: Total,
    outside: Vec<Outside>,
    unbuilt_from: UnbuiltFrom,
}

/// Where the suites outside parity as [`Why::Unbuilt`] were named from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum UnbuiltFrom {
    /// The pinned tree, every `ext/*` and `sapi/*` suite of it that holds
    /// a `.phpt`, and the readings beside it: the list is whole.
    Tree,
    /// The readings alone, since the pinned tree is not on this machine at
    /// the pin's commit: a suite no reading measured is named nowhere.
    Readings,
}

/// One suite inside parity and the reading that stands for it.
#[derive(Debug, Serialize)]
struct Suite {
    /// The subset, by the scope its reading is taken under.
    #[serde(rename = "suite")]
    name: String,
    /// Where the suite stands, and met-of-target where it was held to one.
    #[serde(flatten)]
    state: State,
    /// The reading that stands for the suite; null where none does.
    reading: Option<Taken>,
}

/// Where one suite stands; only a suite `held` counts in the total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum State {
    /// Its reading was held to the subset's target.
    Held { target: Target },
    /// Its reading ran and was held to no target: none is kept under the
    /// pin for the subset.
    Untargeted,
    /// Its reading ran nothing: the engine absent or stale, or the crate
    /// not yet created.
    Unmeasured,
    /// The subset holds no test, as `ext/lexbor` holds none of its own.
    Empty,
    /// No reading stands for it.
    Missing,
}

impl State {
    /// The state of a suite whose reading is `row`, if one stands.
    fn of(row: Option<&Row>) -> State {
        match row {
            None => State::Missing,
            Some(row) => match row.target() {
                Some(target) => State::Held { target },
                None if row.tests() == 0 => State::Empty,
                None if !row.measured() => State::Unmeasured,
                None => State::Untargeted,
            },
        }
    }

    /// The state's word, as its JSON tag spells it.
    fn as_str(self) -> &'static str {
        match self {
            State::Held { .. } => "held",
            State::Untargeted => "untargeted",
            State::Unmeasured => "unmeasured",
            State::Empty => "empty",
            State::Missing => "missing",
        }
    }
}

/// When the reading that stands was taken, and its verdict.
#[derive(Debug, Serialize)]
struct Taken {
    status: reading::Status,
    at: String,
    commit: String,
}

/// The suites inside parity, summed over those held to a target.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
struct Total {
    /// The suites inside parity.
    suites: usize,
    /// Those whose reading was held to a target, which the sum is over.
    held: usize,
    /// The targets' tests the engine met.
    met: u64,
    /// The targets' tests.
    target: u64,
}

impl Total {
    /// The total over `suites`.
    fn of(suites: &[Suite]) -> Total {
        let held: Vec<Target> = suites
            .iter()
            .filter_map(|suite| match suite.state {
                State::Held { target } => Some(target),
                _ => None,
            })
            .collect();
        Total {
            suites: suites.len(),
            held: held.len(),
            met: held.iter().map(|t| t.met).sum(),
            target: held.iter().map(|t| t.n).sum(),
        }
    }
}

/// A suite parity does not count, and why.
#[derive(Debug, Serialize)]
struct Outside {
    suite: String,
    #[serde(flatten)]
    why: Why,
}

/// Why a suite is outside parity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "why", rename_all = "lowercase")]
enum Why {
    /// `[parity] out` names it under this ruling, as the table cites it:
    /// `design §12`.
    Ruling { ruling: String },
    /// A reading measured it and no `[[crate]]` row owns it: the pinned
    /// php does not build it.
    Unbuilt,
}

/// The section in `format`, from the readings that stand for the
/// checkout at `root` and the corpus of the pinned tree where it is on
/// this machine.
pub(super) fn render(
    manifest: &Manifest,
    store: &Store,
    root: &Path,
    format: Table,
) -> Result<String> {
    let rows = standing(manifest, store, root)?;
    let readings: Vec<&Reading> = rows.iter().filter_map(|row| row.reading.as_ref()).collect();
    let corpus: Option<Vec<PathBuf>> = manifest
        .pin
        .as_ref()
        .and_then(|pin| pin.source.tree().ok())
        .map(|tree| {
            Subset::corpus(&tree)
                .into_iter()
                .map(|subset| PathBuf::from(subset.scope))
                .collect()
        });
    let parity = parity(manifest, &readings, corpus.as_deref());
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&parity)? + "\n",
        Table::Md => markdown(&parity),
    })
}

/// The section's rules, a function of the manifest, the readings that
/// stand and the places of the pinned tree's corpus, `None` where the tree
/// is not on this machine.
fn parity(manifest: &Manifest, readings: &[&Reading], corpus: Option<&[PathBuf]>) -> Parity {
    let inside = inside(manifest);
    let runs: Vec<(PathBuf, &Reading, Row)> = readings
        .iter()
        .filter_map(|reading| {
            let row = <Row as PerReading>::of(reading);
            let subset = PathBuf::from(row.run_of()?);
            Some((subset, *reading, row))
        })
        .collect();
    let suites: Vec<Suite> = inside
        .iter()
        .map(|suite| {
            let run = runs.iter().find(|(subset, _, _)| subset == suite);
            Suite {
                name: suite.display().to_string(),
                state: State::of(run.map(|(_, _, row)| row)),
                reading: run.map(|(_, reading, _)| Taken {
                    status: reading.status,
                    at: reading.at.to_string(),
                    commit: reading.commit.clone(),
                }),
            }
        })
        .collect();
    let measured = runs.iter().map(|(subset, _, _)| subset);
    let outside = outside(
        manifest,
        &inside,
        measured.chain(corpus.unwrap_or_default()),
    );
    Parity {
        total: Total::of(&suites),
        suites,
        outside,
        unbuilt_from: match corpus {
            Some(_) => UnbuiltFrom::Tree,
            None => UnbuiltFrom::Readings,
        },
    }
}

/// The suites parity counts: the core's, the `Root::Dir` rows of
/// [`phpt::ROOTS`], and every `[[crate]]` row that owns a subset, by its
/// name, which is the scope its reading is taken under.
fn inside(manifest: &Manifest) -> BTreeSet<PathBuf> {
    phpt::ROOTS
        .iter()
        .filter_map(|root| match root {
            Root::Dir(dir) => Some(PathBuf::from(dir)),
            Root::Each(_) => None,
        })
        .chain(
            manifest
                .crates
                .iter()
                .filter(|krate| !krate.subsets().is_empty())
                .map(|krate| PathBuf::from(&krate.name)),
        )
        .collect()
}

/// The suites outside parity: every one `[parity] out` names, under its
/// one ruling (`Parity::hold` refuses a suite ruled twice or inside
/// another), whether a reading measured it or not; then the suite of each
/// place nothing owns, a run a reading measured or a subset of the pinned
/// tree's corpus, by the suite its first two segments name under `ext/` or
/// `sapi/`, as not built.
fn outside<'a>(
    manifest: &Manifest,
    inside: &BTreeSet<PathBuf>,
    places: impl Iterator<Item = &'a PathBuf>,
) -> Vec<Outside> {
    let mut outside: BTreeMap<PathBuf, Why> = manifest
        .parity
        .out
        .iter()
        .flat_map(|out| {
            let why = Why::Ruling {
                ruling: out.ruling.clone(),
            };
            out.suites
                .iter()
                .map(move |suite| (suite.clone(), why.clone()))
        })
        .collect();
    for place in places {
        if let Some(suite) = built_suite(place).filter(|_| !owned(manifest, inside, place)) {
            outside.entry(suite).or_insert(Why::Unbuilt);
        }
    }
    outside
        .into_iter()
        .map(|(suite, why)| Outside {
            suite: suite.display().to_string(),
            why,
        })
        .collect()
}

/// Whether a suite inside parity or a `[[crate]]` row owns `place`: the
/// place is inside or around the suite, the row's name or a directory of
/// its subset, as a narrower run, a scope a suite was once read under, a
/// host's `tests` directory, or the suite of a row that owns no subset yet
/// (`sapi/embed`) is. A row stands for what the pinned php builds, so a
/// place a row owns is never one it does not build. The dashboard's
/// `whyOut` (dashboard/collect/parity.ts) is the same rule, a place inside
/// or around a row's name or subset, and re-judges this section's list by
/// it.
fn owned(manifest: &Manifest, inside: &BTreeSet<PathBuf>, place: &Path) -> bool {
    inside.iter().any(|suite| overlaps(place, suite))
        || manifest.crates.iter().any(|krate| {
            overlaps(place, Path::new(&krate.name))
                || krate.subsets().iter().any(|subset| overlaps(place, subset))
        })
}

/// The suite of an extension or a host that `subset` is in, `ext/<name>`
/// or `sapi/<name>`: what the manifest would derive a row for, were the
/// pinned php to build it. None for any other part of the tree.
fn built_suite(subset: &Path) -> Option<PathBuf> {
    let mut parts = subset.components();
    let kind = parts.next()?.as_os_str();
    let name = parts.next()?.as_os_str();
    (kind == "ext" || kind == "sapi").then(|| Path::new(kind).join(name))
}

/// The section as Markdown: one row per suite, the total, then the suites
/// outside parity on one line per ruling and one for the unbuilt, and
/// where the tree was absent, a line saying the unbuilt are the measured
/// ones alone.
fn markdown(parity: &Parity) -> String {
    let head = [
        "suite", "state", "met", "target", "outside", "predated", "reading",
    ];
    let mut out = super::grid(
        &head,
        parity.suites.iter().map(|suite| {
            let counts = match suite.state {
                State::Held { target } => {
                    [target.met, target.n, target.outside, target.predated].map(|n| n.to_string())
                }
                _ => Default::default(),
            };
            let status = suite
                .reading
                .as_ref()
                .map_or(super::NO_READING, |r| r.status.as_str());
            let named = [cell(&suite.name), suite.state.as_str().to_owned()];
            named.into_iter().chain(counts).chain([status.to_owned()])
        }),
    );
    let total = &parity.total;
    writeln!(
        out,
        "\n{} of {} target tests met over the {} of {} suites held to a target; the other {} count in no total.",
        total.met,
        total.target,
        total.held,
        total.suites,
        total.suites - total.held
    ).ok();
    let mut rulings: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut unbuilt = Vec::new();
    for o in &parity.outside {
        match &o.why {
            Why::Ruling { ruling } => rulings.entry(ruling).or_default().push(&o.suite),
            Why::Unbuilt => unbuilt.push(o.suite.as_str()),
        }
    }
    for (ruling, suites) in rulings {
        writeln!(
            out,
            "\nOutside parity under {ruling}: {}.",
            suites.join(", ")
        )
        .ok();
    }
    if !unbuilt.is_empty() {
        writeln!(
            out,
            "\nOutside parity, not built by the pinned php: {}.",
            unbuilt.join(", ")
        )
        .ok();
    }
    if parity.unbuilt_from == UnbuiltFrom::Readings {
        writeln!(
            out,
            "\nWithout the pinned php-src at the pin's commit on this machine (`status --section oracle` says why), the suites not built are named only where a reading measured them."
        ).ok();
    }
    out
}
