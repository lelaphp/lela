//! `xtask status`, the one renderer (harness-design.md §4.6). It reads the
//! manifest, the divergence registry, the store and its log, and for the
//! `git` section the repository, and computes nothing else; a section that
//! needs a command of its own is a missing instrument. `--session` prints
//! the map and the local facts in one block for the session-start hook
//! (§5), laying the local file down from its template when the clone has
//! none.

use std::fmt::Write;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use crate::api_check;
use crate::cli::{Format, Section};
use crate::divergence::{Kind, Registry, Status};
use crate::git::git;
use crate::manifest::Manifest;
use crate::permit;
use crate::reading::{self, Reading, Store};

/// What a section says when no instrument has measured a row.
pub const NO_READING: &str = "no reading";
/// The map: what an agent reads first (harness-design.md §5).
pub const MAP: &str = "AGENTS.md";
/// The local region file, gitignored, rendered only by name (§5).
pub const LOCAL: &str = ".claude/harness.local.md";
/// The volatile sections of the local file, in the order it renders them.
pub const LOCAL_SECTIONS: [&str; 7] = [
    "budget",
    "phpt",
    "witness",
    "mutants",
    "divergence",
    "permits",
    "git",
];
/// What `--session` says under a local file whose regions have no sums.
const UNRENDERED: &str = "The local facts above are unrendered: `computed run .claude/harness.local.md` \
fills them once a person has run `computed trust` for this clone (harness-design.md §11).";

/// Render one section to a string in the asked format.
pub fn render(
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    root: &Path,
    section: Section,
    format: Format,
) -> Result<String> {
    let readings = store.readings()?;
    let (rows, columns): (Vec<Row>, Columns) = match section {
        Section::Rules => (rules(manifest, &readings), RULES),
        Section::Locks => (locks(manifest, &readings), LOCKS),
        Section::Budget => (budget(manifest, &readings), BUDGET),
        Section::Divergence => return divergence(registry, format),
        Section::Phpt => (instrument(&readings, "phpt"), INSTRUMENT),
        Section::Witness => (instrument(&readings, "witness"), INSTRUMENT),
        Section::Mutants => (instrument(&readings, "mutants"), INSTRUMENT),
        Section::Permits => (permits(manifest, &readings, &store.log()?), PERMITS),
        Section::Bundle => (bundle(manifest, &readings), BUNDLE),
        Section::Git => return git_section(root, format),
    };
    Ok(match format {
        Format::Json => {
            serde_json::to_string_pretty(&rows.iter().map(Row::json).collect::<Result<Vec<_>>>()?)?
        }
        Format::Md if rows.is_empty() => format!("{NO_READING}\n"),
        Format::Md => table(columns, &rows),
    })
}

/// The map and the local facts in one block. The map is tracked and must
/// exist; the local file is written from `local_template` when the clone
/// has none, so the hook's `computed run` has a file to render next time.
pub fn session(root: &Path) -> Result<String> {
    let map = fs::read_to_string(root.join(MAP)).with_context(|| format!("reading {MAP}"))?;
    let local_path = root.join(LOCAL);
    if !local_path.exists() {
        if let Some(dir) = local_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&local_path, local_template()).with_context(|| format!("writing {LOCAL}"))?;
    }
    let local = fs::read_to_string(&local_path).with_context(|| format!("reading {LOCAL}"))?;
    let mut out = map;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&local);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if local.lines().any(|l| l.trim() == "<!-- /computed -->") {
        out.push('\n');
        out.push_str(UNRENDERED);
        out.push('\n');
    }
    Ok(out)
}

/// The local file with every region unrendered: one volatile `exec` per
/// section of `LOCAL_SECTIONS`, each `cargo xtask status --section <name>`.
pub fn local_template() -> String {
    let mut out = String::from(
        "# Local facts\n\n\
         The volatile sections of harness-design.md §5, one region per section, \
         each a rendering of the store by `xtask status`. This file is \
         gitignored and rendered only by name, `computed run .claude/harness.local.md`, \
         which the SessionStart hook does before `xtask status --session`.\n",
    );
    for name in LOCAL_SECTIONS {
        let _ = write!(
            out,
            "\n## {name}\n\n<!-- computed exec cmd=\"cargo xtask status --section {name}\" volatile name={name} | do not edit; run computed -->\n<!-- /computed -->\n"
        );
    }
    out
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

    fn message(&self) -> String {
        self.reading
            .as_ref()
            .map(|x| cell(&x.message))
            .unwrap_or_default()
    }

    /// Every number of the reading as `key=value`, space-separated.
    fn numbers(&self) -> String {
        self.reading
            .as_ref()
            .map(|x| {
                x.numbers
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
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
                latest_of(readings, api_check::INSTRUMENT, &seam.name),
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
                latest_of(readings, "budget", &krate.name),
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

/// Every reading of one instrument, one row per scope, the numbers as
/// `key=value` pairs. Sections whose instrument does not exist yet
/// (`phpt`, `witness`, `mutants`) render this way from the store alone.
fn instrument(readings: &[Reading], name: &str) -> Vec<Row> {
    let mut scopes: Vec<&str> = readings
        .iter()
        .filter(|r| r.instrument == name)
        .map(|r| r.scope.as_str())
        .collect();
    scopes.sort_unstable();
    scopes.dedup();
    scopes
        .into_iter()
        .map(|scope| Row::new(&[("scope", json!(scope))], latest_of(readings, name, scope)))
        .collect()
}

/// Each rule `permit` judges (harness-design.md §4.4): its last finding in
/// the store, and how many denials the append-only log holds across
/// sessions (§10 rule 6).
fn permits(manifest: &Manifest, readings: &[Reading], log: &[Reading]) -> Vec<Row> {
    manifest
        .rules
        .iter()
        .filter(|rule| permit::judged_at_the_tool_call(rule))
        .map(|rule| {
            let denials = log
                .iter()
                .filter(|r| {
                    r.instrument == permit::INSTRUMENT
                        && r.rule == rule.id
                        && r.status == reading::Status::Fail
                })
                .count();
            Row::new(
                &[
                    ("rule", json!(rule.id)),
                    ("adr", json!(rule.adr.iter().collect::<Vec<_>>())),
                    ("denials", json!(denials)),
                ],
                latest(readings, |r| {
                    r.instrument == permit::INSTRUMENT && r.rule == rule.id
                }),
            )
        })
        .collect()
}

/// The bundle's order: each crate whose edge carries an `order`, against
/// the `api-check` reading that held the order to the crate graph.
fn bundle(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    manifest
        .edges
        .iter()
        .filter_map(|edge| edge.order.as_ref().map(|order| (edge, order)))
        .map(|(edge, order)| {
            Row::new(
                &[("crate", json!(edge.from)), ("order", json!(order))],
                latest_of(readings, api_check::INSTRUMENT, &edge.from),
            )
        })
        .collect()
}

/// The `git` section, volatile: the last ten commits and the status of
/// the working tree, as git prints them.
fn git_section(root: &Path, format: Format) -> Result<String> {
    let log = git(root, &["log", "--oneline", "-10"], None)?;
    let status = git(root, &["status", "--short", "--branch"], None)?;
    Ok(match format {
        Format::Json => serde_json::to_string_pretty(&json!({
            "log": log.lines().collect::<Vec<_>>(),
            "status": status.lines().collect::<Vec<_>>(),
        }))?,
        Format::Md => format!(
            "```\n$ git log --oneline -10\n{log}\n$ git status --short --branch\n{status}```\n"
        ),
    })
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

/// The newest reading of one instrument over one scope.
fn latest_of(readings: &[Reading], instrument: &str, scope: &str) -> Option<Reading> {
    latest(readings, |r| r.instrument == instrument && r.scope == scope)
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
        r.message(),
    ]
});

const INSTRUMENT: Columns = (&["scope", "reading", "numbers", "message"], |r| {
    vec![r.fact("scope"), r.status(), r.numbers(), r.message()]
});

const PERMITS: Columns = (
    &["rule", "adr", "reading", "at", "denials", "message"],
    |r| {
        vec![
            r.fact("rule"),
            r.fact("adr"),
            r.status(),
            r.reading
                .as_ref()
                .map(|x| x.at.to_string())
                .unwrap_or_default(),
            r.fact("denials"),
            r.message(),
        ]
    },
);

const BUNDLE: Columns = (
    &["crate", "order", "extensions", "reading", "message"],
    |r| {
        vec![
            r.fact("crate"),
            r.fact("order"),
            r.number("extensions"),
            r.status(),
            r.message(),
        ]
    },
);

fn table((head, line): Columns, rows: &[Row]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "| {} |", head.join(" | "));
    let _ = writeln!(out, "|{}", "---|".repeat(head.len()));
    for row in rows {
        let _ = writeln!(out, "| {} |", line(row).join(" | "));
    }
    out
}
