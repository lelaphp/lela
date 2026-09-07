//! `xtask status`, the one renderer (harness-design.md §4.6). It reads the
//! manifest, the divergence registry and the store and computes nothing
//! else; a section that needs a command of its own is a missing instrument.

use std::fmt::Write;

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::cli::{Format, Section};
use crate::divergence::{Kind, Registry, Status};
use crate::manifest::Manifest;
use crate::reading::{Reading, Store};

/// What a section says when no instrument has measured a row.
pub const NO_READING: &str = "no reading";

/// Render one section to a string in the asked format.
pub fn render(
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    section: Section,
    format: Format,
) -> Result<String> {
    let readings = store.readings()?;
    let (rows, columns): (Vec<Row>, Columns) = match section {
        Section::Rules => (rules(manifest, &readings), RULES),
        Section::Locks => (locks(manifest, &readings), LOCKS),
        Section::Budget => (budget(manifest, &readings), BUDGET),
        Section::Divergence => return divergence(registry, format),
    };
    Ok(match format {
        Format::Json => {
            serde_json::to_string_pretty(&rows.iter().map(Row::json).collect::<Result<Vec<_>>>()?)?
        }
        Format::Md => table(columns, &rows),
    })
}

/// One rendered row: the manifest's facts by name, and the reading, if any.
#[derive(Debug)]
struct Row {
    facts: Map<String, Value>,
    reading: Option<Reading>,
}

impl Row {
    fn new(facts: &[(&str, Value)], reading: Option<Reading>) -> Row {
        let facts = facts
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        Row { facts, reading }
    }

    fn json(&self) -> Result<Value> {
        let mut object = self.facts.clone();
        let reading = match &self.reading {
            Some(r) => r.to_json()?,
            None => Value::Null,
        };
        object.insert("reading".to_owned(), reading);
        Ok(Value::Object(object))
    }

    /// One fact as a Markdown cell: a list renders as ADR numbers.
    fn fact(&self, key: &str) -> String {
        match self.facts.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => cell(s),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(|n| format!("ADR-{n}"))
                .collect::<Vec<_>>()
                .join(", "),
            Some(other) => other.to_string(),
        }
    }

    fn status(&self) -> String {
        match &self.reading {
            Some(r) => r.status.as_str().to_owned(),
            None => NO_READING.to_owned(),
        }
    }

    fn number(&self, key: &str) -> String {
        self.reading
            .as_ref()
            .and_then(|r| r.numbers.get(key))
            .map(ToString::to_string)
            .unwrap_or_default()
    }
}

fn rules(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    manifest
        .rules
        .iter()
        .map(|rule| {
            Row::new(
                &[
                    ("id", json!(rule.id)),
                    ("adr", json!(rule.adr.iter().collect::<Vec<_>>())),
                    (
                        "predicate",
                        json!(rule.predicate.named().first().copied().unwrap_or("")),
                    ),
                    ("text", json!(rule.text)),
                ],
                latest(readings, |r| r.rule == rule.id),
            )
        })
        .collect()
}

fn locks(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    manifest
        .seams
        .iter()
        .map(|seam| {
            let ceiling = manifest
                .lock_ceiling(seam)
                .map_or(Value::Null, |n| json!(n));
            Row::new(
                &[
                    ("seam", json!(seam.name)),
                    ("lock", json!(seam.lock)),
                    ("ceiling", ceiling),
                ],
                latest(readings, |r| {
                    r.instrument == "api-check" && r.scope == seam.name
                }),
            )
        })
        .collect()
}

fn budget(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    manifest
        .crates
        .iter()
        .map(|krate| {
            Row::new(
                &[
                    ("crate", json!(krate.name)),
                    ("ceiling", json!(krate.budget)),
                ],
                latest(readings, |r| {
                    r.instrument == "budget" && r.scope == krate.name
                }),
            )
        })
        .collect()
}

/// The divergence section: the registry's rows, then counts by kind (rows)
/// and status (columns) with a total. It reads no reading; the registry is
/// the truth and this is its view (harness-design.md §4.3).
fn divergence(registry: &Registry, format: Format) -> Result<String> {
    let counts = registry.counts();
    let kinds = Kind::ALL
        .into_iter()
        .filter(|k| counts.by_kind.contains_key(k));
    let of = |status: Status| counts.by_status.get(&status).copied().unwrap_or(0);
    match format {
        Format::Json => {
            let rows: Vec<Value> = registry
                .rows
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "kind": r.kind.as_str(),
                        "status": r.status.as_str(),
                        "since": r.since,
                        "reason": r.reason,
                    })
                })
                .collect();
            let mut by_kind = Map::new();
            for kind in kinds {
                let cells: Map<String, Value> = Status::ALL
                    .into_iter()
                    .map(|s| (s.as_str().to_owned(), json!(counts.cell(kind, s))))
                    .collect();
                by_kind.insert(kind.as_str().to_owned(), Value::Object(cells));
            }
            let total: Map<String, Value> = Status::ALL
                .into_iter()
                .map(|s| (s.as_str().to_owned(), json!(of(s))))
                .collect();
            by_kind.insert("total".to_owned(), Value::Object(total));
            Ok(serde_json::to_string_pretty(&json!({
                "rows": rows,
                "counts": by_kind,
            }))?)
        }
        Format::Md => {
            let mut out = String::new();
            let _ = writeln!(out, "| id | kind | status | since | reason |");
            let _ = writeln!(out, "|---|---|---|---|---|");
            for row in &registry.rows {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} |",
                    cell(&row.id),
                    row.kind.as_str(),
                    row.status.as_str(),
                    row.since,
                    cell(&row.reason)
                );
            }
            let _ = writeln!(out);
            let _ = writeln!(out, "| kind | open | accepted | fixed | rows |");
            let _ = writeln!(out, "|---|---|---|---|---|");
            for kind in kinds {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} |",
                    kind.as_str(),
                    counts.cell(kind, Status::Open),
                    counts.cell(kind, Status::Accepted),
                    counts.cell(kind, Status::Fixed),
                    counts.by_kind[&kind]
                );
            }
            let _ = writeln!(
                out,
                "| total | {} | {} | {} | {} |",
                of(Status::Open),
                of(Status::Accepted),
                of(Status::Fixed),
                registry.rows.len()
            );
            Ok(out)
        }
    }
}

/// One string as a Markdown cell: pipes escaped, one line.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The newest reading that matches, by its `at`. The store holds one file
/// per instrument and scope, so a rule measured by two instruments has two
/// readings; the newer is shown. `at` is always UTC to the second in one
/// format (`git::now`), so its text orders as its instant does.
fn latest(readings: &[Reading], pick: impl Fn(&Reading) -> bool) -> Option<Reading> {
    readings
        .iter()
        .filter(|r| pick(r))
        .max_by_key(|r| r.at.to_string())
        .cloned()
}

/// A Markdown table's columns: the header, and the cells of one row.
type Columns = (&'static [&'static str], fn(&Row) -> Vec<String>);

const RULES: Columns = (&["rule", "adr", "predicate", "reading", "text"], |r| {
    vec![
        r.fact("id"),
        r.fact("adr"),
        r.fact("predicate"),
        r.status(),
        r.fact("text"),
    ]
});

const LOCKS: Columns = (&["seam", "lock", "items", "ceiling", "reading"], |r| {
    vec![
        r.fact("seam"),
        r.fact("lock"),
        r.number("items"),
        r.fact("ceiling"),
        r.status(),
    ]
});

const BUDGET: Columns = (&["crate", "lines", "ceiling", "reading", "message"], |r| {
    vec![
        r.fact("crate"),
        r.number("lines"),
        r.fact("ceiling"),
        r.status(),
        r.reading
            .as_ref()
            .map(|x| x.message.clone())
            .unwrap_or_default(),
    ]
});

fn table((head, line): Columns, rows: &[Row]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "| {} |", head.join(" | "));
    let _ = writeln!(out, "|{}", "---|".repeat(head.len()));
    for row in rows {
        let _ = writeln!(out, "| {} |", line(row).join(" | "));
    }
    out
}
