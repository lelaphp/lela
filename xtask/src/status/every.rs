//! `--every` and `--log` (harness-design.md §4.6, #1745): a section that
//! keeps one reading per scope renders every reading instead, in the log's
//! order, picking none; from the store's log, or from a log it is handed,
//! another store's or the published history. Under `--format jsonl` a row
//! is one line and carries `v`, the version of its shape. What is here is
//! the section's alone in one place, its row ([`PerReading`]); the source,
//! the order and the lines are every section's.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Serialize;

use crate::cli::{Format, Section};
use crate::phpt;
use crate::reading::{LOG, Log, Reading, Store};

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
/// section joins by its line (#1763 adds budget, api-check and witness).
const ROWS: &[(Section, Lines)] = &[(Section::Phpt, lines::<phpt::row::Row>)];

/// A section as the command line spells it.
fn name(section: Section) -> String {
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
        None => return Ok((store.log()?, LOG.to_owned())),
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
        .filter(|r| r.instrument == R::INSTRUMENT)
    {
        out.push_str(&serde_json::to_string(&R::of(reading)).context("rendering a row")?);
        out.push('\n');
    }
    Ok(out)
}
