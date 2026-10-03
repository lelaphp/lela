//! The `parity` section (design §1, §12; #1761): met-of-target per suite
//! and in total, as design §1 says `xtask status` states it. A suite is a
//! subset parity counts, by the scope its reading is taken under:
//! `Zend/tests` and `tests`, the core's, and every `[[crate]]` row that
//! owns a subset, by the row's name, since the manifest derives a row for
//! every extension and SAPI the pinned php builds. Each is read off the
//! phpt reading that stands for it, as the `phpt` section picks it, and a
//! reading's facts are its row's ([`phpt::row::Row`]), the rule source the
//! dashboard reads too. The suites outside parity are named apart: those
//! `[parity] out` rules out, with the ruling, and those a reading measured
//! that no row owns, which the pinned php does not build. The section
//! writes no reading.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use super::{Table, cell, outlived, standing};
use crate::manifest::Manifest;
use crate::phpt::row::{Row, Target};
use crate::phpt::{self, Root};
use crate::reading::{self, Store};
use crate::status::PerReading;

/// The section: the suites inside parity, their total, and the suites
/// outside it.
#[derive(Debug, Serialize)]
struct Parity {
    suites: Vec<Suite>,
    total: Total,
    outside: Vec<Outside>,
}

/// One suite inside parity and the reading that stands for it.
#[derive(Debug, Serialize)]
struct Suite {
    /// The subset, relative to the pinned tree.
    #[serde(rename = "suite")]
    name: String,
    /// The reading that stands for the suite; null where none does.
    reading: Option<Taken>,
    /// Met-of-target; null where no reading stands or the one that does
    /// was held to no target, which no gauge counts.
    target: Option<Target>,
}

/// When the reading that stands was taken, and its verdict.
#[derive(Debug, Serialize)]
struct Taken {
    status: reading::Status,
    at: String,
    commit: String,
}

/// The suites inside parity, summed over those held to a target.
#[derive(Debug, Default, Serialize)]
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

/// A suite parity does not count, and why.
#[derive(Debug, Serialize)]
struct Outside {
    suite: String,
    #[serde(flatten)]
    why: Why,
}

/// Why a suite is outside parity.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "why", rename_all = "lowercase")]
enum Why {
    /// `[parity] out` names it under this ticket's ruling (design §12).
    Ruling { ruling: u64 },
    /// A reading measured it and no `[[crate]]` row owns it: the pinned
    /// php does not build it.
    Unbuilt,
}

/// The section in `format`, from the readings that stand for the
/// checkout at `root`.
pub(super) fn render(
    manifest: &Manifest,
    store: &Store,
    root: &Path,
    format: Table,
) -> Result<String> {
    let rows = standing(store, root)?;
    let readings: Vec<&reading::Reading> = rows
        .iter()
        .filter(|row| !outlived(manifest, row))
        .filter_map(|row| row.reading.as_ref())
        .collect();
    let parity = parity(manifest, &readings);
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&parity)? + "\n",
        Table::Md => markdown(&parity),
    })
}

/// The section's rules, a function of the manifest and the readings that
/// stand: each suite's own run, and whatever run no suite owns, by the
/// suite its first two segments name under `ext/` or `sapi/`.
fn parity(manifest: &Manifest, readings: &[&reading::Reading]) -> Parity {
    let inside: BTreeSet<PathBuf> = phpt::ROOTS
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
        .collect();
    let mut outside: BTreeMap<PathBuf, Why> = manifest
        .parity
        .out
        .iter()
        .flat_map(|out| {
            let why = Why::Ruling {
                ruling: out.ruling.number(),
            };
            out.suites.iter().map(move |suite| (suite.clone(), why))
        })
        .collect();
    let mut runs: BTreeMap<PathBuf, (&reading::Reading, Row)> = BTreeMap::new();
    for reading in readings {
        let row = <Row as PerReading>::of(reading);
        let Some(subset) = row.run_of().map(PathBuf::from) else {
            continue;
        };
        if inside.contains(&subset) {
            runs.insert(subset, (reading, row));
        } else if !inside
            .iter()
            .any(|suite| subset.starts_with(suite) || suite.starts_with(&subset))
            && let Some(suite) = built_suite(&subset)
        {
            // A suite ruled out is in the map already, under its ruling.
            outside.entry(suite).or_insert(Why::Unbuilt);
        }
    }

    let mut total = Total::default();
    let suites = inside
        .into_iter()
        .map(|suite| {
            let run = runs.get(&suite);
            let target = run.and_then(|(_, row)| row.target());
            total.suites += 1;
            if let Some(target) = target {
                total.held += 1;
                total.met += target.met;
                total.target += target.n;
            }
            Suite {
                name: suite.display().to_string(),
                reading: run.map(|(reading, _)| Taken {
                    status: reading.status,
                    at: reading.at.to_string(),
                    commit: reading.commit.clone(),
                }),
                target,
            }
        })
        .collect();
    let outside = outside
        .into_iter()
        .map(|(suite, why)| Outside {
            suite: suite.display().to_string(),
            why,
        })
        .collect();
    Parity {
        suites,
        total,
        outside,
    }
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
/// outside parity on one line each for the ruling and the unbuilt.
fn markdown(parity: &Parity) -> String {
    let mut out = String::from("| suite | met | target | outside | predated | reading |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for suite in &parity.suites {
        let status = suite
            .reading
            .as_ref()
            .map_or(super::NO_READING, |r| r.status.as_str());
        let cells = match suite.target {
            Some(t) => [t.met, t.n, t.outside, t.predated].map(|n| n.to_string()),
            None if suite.reading.is_some() => [
                "no target".to_owned(),
                String::new(),
                String::new(),
                String::new(),
            ],
            None => [String::new(), String::new(), String::new(), String::new()],
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            cell(&suite.name),
            cells.join(" | "),
            status
        );
    }
    let total = &parity.total;
    let _ = writeln!(
        out,
        "\n{} of {} target tests met over the {} of {} suites held to a target; the other {} count in no total.",
        total.met,
        total.target,
        total.held,
        total.suites,
        total.suites - total.held
    );
    let mut rulings: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
    for o in &parity.outside {
        if let Why::Ruling { ruling } = o.why {
            rulings.entry(ruling).or_default().push(&o.suite);
        }
    }
    for (ruling, suites) in rulings {
        let _ = writeln!(
            out,
            "\nOutside parity by the ruling on #{ruling}: {}.",
            suites.join(", ")
        );
    }
    let unbuilt: Vec<&str> = parity
        .outside
        .iter()
        .filter(|o| matches!(o.why, Why::Unbuilt))
        .map(|o| o.suite.as_str())
        .collect();
    if !unbuilt.is_empty() {
        let _ = writeln!(
            out,
            "\nOutside parity, not built by the pinned php: {}.",
            unbuilt.join(", ")
        );
    }
    out
}
