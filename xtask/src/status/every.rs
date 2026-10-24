//! `--every` and `--log` (harness-design.md §4.6, #1745): a section that
//! keeps one reading per scope renders every reading instead, in the log's
//! order, picking none; from the store's log, or from a log it is handed,
//! another store's or the published history. Under `--format jsonl` a row
//! is one line and carries `v`, the version of its shape. What is here is
//! the section's alone in one place, its row ([`PerReading`]); the source,
//! the order and the lines are every section's, and so is what every row
//! opens with ([`Head`]) and how a count is read ([`count`]).

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Serialize;

use crate::cli::{Format, Section};
use crate::reading::{LOG, Log, Number, Reading, Status, Store};
use crate::{api_check, bench, budget, phpt, witness};

/// What `--log` takes for the standard input.
const STDIN: &str = "-";

/// A section's row under `--every`: one per reading, the facts of that
/// reading alone, in a shape whose own `v` field versions it. Which
/// reading stands, and what a series sums, stay the reader's.
pub(crate) trait PerReading: Serialize {
    /// The instrument whose readings the section renders.
    const INSTRUMENT: &'static str;

    /// The row of one reading.
    fn of(reading: &Reading) -> Self;
}

/// Every reading of the section's instrument, one line each: from the
/// log at `log`, `-` for the standard input, or else from the store's.
/// A line that is no reading is counted on stderr and skipped; only a log
/// that cannot be read at all is an error.
pub(crate) fn render(
    store: &Store,
    section: Section,
    format: Format,
    log: Option<&Path>,
) -> Result<String> {
    if format != Format::Jsonl {
        bail!("--every renders one row per reading, as --format jsonl");
    }
    let Some((_, rows)) = ROWS.iter().find(|(named, _)| *named == section) else {
        let built: Vec<String> = ROWS.iter().map(|(named, _)| name(*named)).collect();
        bail!(
            "--every renders a section with a row per reading: {}; {} has none yet",
            built.join(", "),
            name(section)
        );
    };
    let (log, source) = read(store, log)?;
    if let Some(note) = log.unreadable_note_of(&source) {
        eprintln!("{note}");
    }
    rows(&log)
}

/// How a section renders a log under `--every`: its row's lines.
type Lines = fn(&Log) -> Result<String>;

/// The sections `--every` renders, each with its row: the one table a
/// section joins by its line.
const ROWS: &[(Section, Lines)] = &[
    (Section::Phpt, lines::<phpt::row::Row>),
    (Section::Budget, lines::<budget::row::Row>),
    // `--section api-check` names it too: a seam's lock, and under
    // `--every` each crate's edges beside it.
    (Section::Locks, lines::<api_check::row::Row>),
    (Section::Witness, lines::<witness::row::Row>),
    (Section::Bench, lines::<bench::row::Row>),
];

/// What every row opens with, whatever its section: the version of the
/// row's shape and where the reading was taken. A row flattens it first,
/// so the fields after it are the section's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Head {
    /// The version of the row's shape, its section's own.
    v: u32,
    /// The instrument that took the reading.
    instrument: String,
    /// The scope as the reading spells it.
    scope: String,
    /// The reading's own verdict.
    status: Status,
    /// When the reading was taken, RFC 3339.
    at: String,
    /// The commit it was taken at, as the reading records it; null where
    /// it records none.
    commit: Option<String>,
    /// The digest of the oracle pin the run was taken under, where the
    /// reading records one (#1387).
    #[serde(skip_serializing_if = "Option::is_none")]
    recipe: Option<String>,
}

impl Head {
    /// The head of `reading`'s row, of the shape `v`.
    pub(crate) fn of(reading: &Reading, v: u32) -> Head {
        Head {
            v,
            instrument: reading.instrument.clone(),
            scope: reading.scope.clone(),
            status: reading.status,
            at: reading.at.to_string(),
            commit: Some(reading.commit.clone()).filter(|commit| !commit.is_empty()),
            recipe: reading.recipe.clone(),
        }
    }
}

/// The count `reading` keeps under `key`, as a whole number; none where it
/// keeps none.
pub(crate) fn count(reading: &Reading, key: &str) -> Option<u64> {
    reading.numbers.get(key).map(|n| whole(*n))
}

/// A reading of `instrument` of `scope` with the counts named, for a
/// row's tests.
#[cfg(test)]
pub(crate) fn reading(instrument: &str, scope: &str, counts: &[(&str, i64)]) -> Reading {
    let mut reading = Reading::new(instrument, scope);
    reading.numbers = counts
        .iter()
        .map(|(key, n)| ((*key).to_owned(), Number::Int(*n)))
        .collect();
    reading
}

/// A count as a whole number: a negative one, which no instrument writes,
/// is none, and a measurement is rounded.
pub(crate) fn whole(n: Number) -> u64 {
    match n {
        Number::Int(n) => u64::try_from(n).unwrap_or(0),
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Number::Float(n) => n.max(0.0).round() as u64,
    }
}

/// A section as the command line spells it.
pub(super) fn name(section: Section) -> String {
    section
        .to_possible_value()
        .map_or_else(|| format!("{section:?}"), |v| v.get_name().to_owned())
}

/// The log `--every` renders and the name its note gives it: the one at
/// `path` or, for `-`, the standard input, each read as a log handed in
/// ([`Log::handed`]); or else the store's own, read as every other section
/// reads it.
fn read(store: &Store, path: Option<&Path>) -> Result<(Log, String)> {
    let (text, name) = match path {
        None => return Ok((store.history_all()?, LOG.to_owned())),
        Some(path) if path == Path::new(STDIN) => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("reading the log from the standard input")?;
            (text, "stdin".to_owned())
        }
        Some(path) => (
            fs::read_to_string(path)
                .with_context(|| format!("reading the log at {}", path.display()))?,
            path.display().to_string(),
        ),
    };
    Ok((Log::handed(&text), name))
}

/// The rows of `R`'s instrument in `log`, in its order, one JSON line each.
fn lines<R: PerReading>(log: &Log) -> Result<String> {
    let mut out = String::new();
    for reading in log
        .readings
        .iter()
        .filter(|r| r.instrument == R::INSTRUMENT && !crate::witness::is_accepted(r))
    {
        out.push_str(&serde_json::to_string(&R::of(reading)).context("rendering a row")?);
        out.push('\n');
    }
    Ok(out)
}
