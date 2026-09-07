//! `xtask status`, the one renderer (harness-design.md §4.6). It reads the
//! manifest and the store and computes nothing else; a section that needs a
//! command of its own is a missing instrument.

use std::fmt::Write;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::{Format, Section};
use crate::manifest::Manifest;
use crate::reading::{Reading, Store};

/// What a section says when no instrument has measured a row.
pub const NO_READING: &str = "no reading";

/// Render one section to a string in the asked format.
pub fn render(
    manifest: &Manifest,
    store: &Store,
    section: Section,
    format: Format,
) -> Result<String> {
    let readings = store.readings()?;
    let rows: Vec<Row> = match section {
        Section::Rules => rules(manifest, &readings),
        Section::Locks => locks(manifest, &readings),
        Section::Budget => budget(manifest, &readings),
    };
    Ok(match format {
        Format::Json => {
            serde_json::to_string_pretty(&rows.iter().map(|r| r.json()).collect::<Vec<_>>())?
        }
        Format::Md => table(section, &rows),
    })
}

/// One rendered row: the manifest's facts and the reading, if any.
#[derive(Debug, Serialize)]
struct Row {
    facts: Vec<(&'static str, Value)>,
    reading: Option<Reading>,
}

impl Row {
    fn json(&self) -> Value {
        let mut object = serde_json::Map::new();
        for (key, value) in &self.facts {
            object.insert((*key).to_owned(), value.clone());
        }
        let reading = match &self.reading {
            Some(r) => {
                let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
                v["at"] = Value::String(r.at.to_string());
                v
            }
            None => Value::Null,
        };
        object.insert("reading".to_owned(), reading);
        Value::Object(object)
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
            .map(|n| n.to_string())
            .unwrap_or_default()
    }
}

fn rules(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    manifest
        .rules
        .iter()
        .map(|rule| Row {
            facts: vec![
                ("id", json!(rule.id)),
                ("adr", json!(rule.adr.iter().collect::<Vec<_>>())),
                (
                    "predicate",
                    json!(rule.predicate.named().first().copied().unwrap_or("")),
                ),
                ("text", json!(rule.text())),
            ],
            reading: latest(readings, |r| r.rule == rule.id),
        })
        .collect()
}

fn locks(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    manifest
        .seams
        .iter()
        .map(|seam| Row {
            facts: vec![
                ("seam", json!(seam.name)),
                ("lock", json!(seam.lock)),
                (
                    "ceiling",
                    if seam.lock == "api.lock" {
                        json!(manifest.budget.api_items)
                    } else {
                        Value::Null
                    },
                ),
            ],
            reading: latest(readings, |r| {
                r.instrument == "api-check" && r.scope == seam.name
            }),
        })
        .collect()
}

fn budget(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    manifest
        .crates
        .iter()
        .map(|krate| Row {
            facts: vec![
                ("crate", json!(krate.name)),
                ("ceiling", json!(krate.budget)),
            ],
            reading: latest(readings, |r| {
                r.instrument == "budget" && r.scope == krate.name
            }),
        })
        .collect()
}

/// The newest reading that matches, by its `at`.
fn latest(readings: &[Reading], pick: impl Fn(&Reading) -> bool) -> Option<Reading> {
    readings
        .iter()
        .filter(|r| pick(r))
        .max_by_key(|r| r.at.to_string())
        .cloned()
}

/// The cells of one Markdown row.
type Line = fn(&Row) -> Vec<String>;

fn table(section: Section, rows: &[Row]) -> String {
    let mut out = String::new();
    let (head, line): (&[&str], Line) = match section {
        Section::Rules => (&["rule", "adr", "predicate", "reading", "text"], |r| {
            vec![
                cell(&r.facts[0].1),
                adrs(&r.facts[1].1),
                cell(&r.facts[2].1),
                r.status(),
                cell(&r.facts[3].1),
            ]
        }),
        Section::Locks => (&["seam", "lock", "items", "ceiling", "reading"], |r| {
            vec![
                cell(&r.facts[0].1),
                cell(&r.facts[1].1),
                r.number("items"),
                cell(&r.facts[2].1),
                r.status(),
            ]
        }),
        Section::Budget => (&["crate", "lines", "ceiling", "reading", "message"], |r| {
            vec![
                cell(&r.facts[0].1),
                r.number("lines"),
                cell(&r.facts[1].1),
                r.status(),
                r.reading
                    .as_ref()
                    .map(|x| x.message.clone())
                    .unwrap_or_default(),
            ]
        }),
    };
    let _ = writeln!(out, "| {} |", head.join(" | "));
    let _ = writeln!(out, "|{}", "---|".repeat(head.len()));
    for row in rows {
        let _ = writeln!(out, "| {} |", line(row).join(" | "));
    }
    out
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.replace('|', "\\|").replace('\n', " "),
        other => other.to_string(),
    }
}

fn adrs(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|n| format!("ADR-{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}
