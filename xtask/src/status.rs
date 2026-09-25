//! `xtask status`, the one renderer (harness-design.md §4.6). It reads the
//! manifest, the divergence registry, the store and its log, and for the
//! `git` section the repository, and computes nothing else; a section that
//! needs a command of its own is a missing instrument. `--session` prints
//! the map and the local facts for the function hook's `prompt.context`
//! (§5), as one text or, in JSON, as the two blocks the hook hands the
//! engine, laying the local file down from its template when the clone
//! has none.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::api_check;
use crate::cli::{Format, Section};
use crate::crosscheck;
use crate::divergence::{Kind, Registry, Status};
use crate::git::git;
use crate::manifest::{Manifest, Measure, SCRIPTS, Week};
use crate::oracle::{PROMISE_VARIABLE, Pin, TREE_VARIABLE};
use crate::permit;
use crate::port_map;
use crate::reading::{self, Log, Reading, Store};
use crate::sources::NOT_YET_CREATED;
use crate::track_work;
use crate::usage;

/// What a section says when no instrument has measured a row.
pub(crate) const NO_READING: &str = "no reading";
/// The map: what an agent reads first (harness-design.md §5).
pub(crate) const MAP: &str = "AGENTS.md";
/// The local region file, gitignored, rendered only by name (§5).
pub(crate) const LOCAL: &str = ".claude/harness.local.md";
/// The volatile sections of the local file, in the order it renders them:
/// §5's eight, the instrument readings first and `port-map` with them
/// since #148, a reading that moves with the store like the others and is
/// otherwise renderable only by name.
pub(crate) const LOCAL_SECTIONS: [&str; 8] = [
    "budget",
    "phpt",
    "witness",
    "mutants",
    "port-map",
    "divergence",
    "permits",
    "git",
];
/// What `--session` says under a local file whose regions have no sums.
const UNRENDERED: &str = "The local facts above are unrendered: `computed run .claude/harness.local.md` \
fills them once a person has run `computed trust` for this clone (harness-design.md §11).";

/// Render one section to a string in the asked format; `week` picks one
/// row of the `week` section and `session_id` names "this session" of the
/// `permits` section, each belonging to no other.
#[allow(clippy::too_many_arguments)]
pub fn render(
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    root: &Path,
    section: Section,
    format: Format,
    week: Option<usize>,
    session_id: Option<&str>,
) -> Result<String> {
    if week.is_some() && section != Section::Week {
        bail!("--week picks a row of the week section; use --section week");
    }
    if session_id.is_some() && section != Section::Permits {
        bail!("--session-id names this session of the permits section; use --section permits");
    }
    let readings = store.readings()?;
    let (rows, columns): (Vec<Row>, Columns) = match section {
        Section::Rules => (rules(manifest, &readings), RULES),
        Section::Locks => (locks(manifest, &readings), LOCKS),
        Section::Budget => (
            fold_uncreated(budget(manifest, &readings), "crate")?,
            BUDGET,
        ),
        Section::Divergence => return divergence(registry, format),
        Section::Phpt => (fold_uncreated(standing(store, root)?, "scope")?, INSTRUMENT),
        Section::Witness => (
            fold_uncreated(instrument(&readings, "witness"), "scope")?,
            INSTRUMENT,
        ),
        Section::Test => (
            fold_uncreated(instrument(&readings, crate::test::INSTRUMENT), "scope")?,
            INSTRUMENT,
        ),
        Section::Mutants => (
            fold_uncreated(instrument(&readings, "mutants"), "scope")?,
            INSTRUMENT,
        ),
        Section::Bench => (instrument(&readings, crate::bench::INSTRUMENT), INSTRUMENT),
        Section::PortMap => (instrument(&readings, port_map::INSTRUMENT), INSTRUMENT),
        Section::Crosscheck => (instrument(&readings, crosscheck::INSTRUMENT), INSTRUMENT),
        Section::Ticket => (instrument(&readings, "ticket"), TICKET),
        Section::Track => (last_run(&store.log()?), TRACK),
        Section::Usage => (last_per_scope(&store.log()?, usage::INSTRUMENT), USAGE),
        Section::Permits => {
            return permits(manifest, &readings, &store.log()?, format, session_id);
        }
        Section::Week => return week_section(manifest, &readings, week, format),
        Section::Bundle => (bundle(manifest, &readings), BUNDLE),
        Section::Git => return git_section(root, format),
        Section::Oracle => return oracle_section(&manifest.pin, format),
    };
    Ok(match format {
        Format::Json => {
            serde_json::to_string_pretty(&rows.iter().map(Row::json).collect::<Result<Vec<_>>>()?)?
        }
        Format::Md if rows.is_empty() => format!("{NO_READING}\n"),
        Format::Md => table(columns, &rows),
    })
}

/// The `session_id` of the hook's stdin head, the JSON every hook event
/// carries (harness-design.md D.16); none at a terminal, on an empty
/// stdin, or on a head that is not JSON, since a session is a fact and not
/// a condition.
pub(crate) fn session_of_stdin() -> Option<String> {
    use std::io::{IsTerminal, Read};
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut text = String::new();
    stdin.read_to_string(&mut text).ok()?;
    let head: Value = serde_json::from_str(&text).ok()?;
    head["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// The map and the local facts, the latter followed, when `session_id`
/// is known and the log holds a denial of its own, by the rules this
/// session fired with their counts (§4.6, "this session"). As Markdown
/// the two are one text, the map first; as JSON they are the object
/// `{"map", "facts"}`, since the engine caps a context block and the hook
/// hands them over as two (#64). The map is tracked and must exist; the
/// local file is written from `local_template` when the clone has none,
/// so the hook's `computed run` has a file to render next time.
pub fn session(
    manifest: &Manifest,
    store: &Store,
    root: &Path,
    session_id: Option<&str>,
    format: Format,
) -> Result<String> {
    let mut map = fs::read_to_string(root.join(MAP)).with_context(|| format!("reading {MAP}"))?;
    if !map.ends_with('\n') {
        map.push('\n');
    }
    let local_path = root.join(LOCAL);
    if !local_path.exists() {
        if let Some(dir) = local_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&local_path, local_template()).with_context(|| format!("writing {LOCAL}"))?;
    }
    let mut facts = fs::read_to_string(&local_path).with_context(|| format!("reading {LOCAL}"))?;
    if !facts.ends_with('\n') {
        facts.push('\n');
    }
    if facts.lines().any(|l| l.trim() == "<!-- /computed -->") {
        facts.push('\n');
        facts.push_str(UNRENDERED);
        facts.push('\n');
    }
    if let Some(id) = session_id {
        let log = store.log()?;
        let rows: Vec<Row> = permit_rows(manifest, &store.readings()?, &log.readings, Some(id))
            .into_iter()
            .filter(|r| r.facts.get("this_session").and_then(Value::as_u64) > Some(0))
            .collect();
        if !rows.is_empty() {
            let _ = write!(
                facts,
                "\n## permits this session\n\n{}",
                table(PERMITS, &rows)
            );
        }
        if let Some(note) = log.unreadable_note() {
            let _ = writeln!(facts, "\n{note}");
        }
    }
    Ok(match format {
        Format::Json => serde_json::to_string(&json!({ "map": map, "facts": facts }))?,
        Format::Md => format!("{map}\n{facts}"),
    })
}

/// The local file with every region unrendered: one volatile `exec` per
/// section of `LOCAL_SECTIONS`, each `cargo xtask status --section <name>`.
pub(crate) fn local_template() -> String {
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

/// The widest a reading's message renders in a Markdown cell, in
/// characters. A Markdown table is what a session scans, and the local
/// facts it is handed at start are eight sections under the engine's cap
/// on one block (§5, #64); so a row says its message in one line of at
/// most this many, and a number the reading holds at zero, which says
/// nothing happened, is left out of its `numbers` cell. `--format json`
/// is the reading whole, every number and the message uncut (#820). A
/// person's words, a rule's text and a divergence's reason, are
/// rendered as written.
const MESSAGE_WIDTH: usize = 120;

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

    /// The reading's message as a cell of at most [`MESSAGE_WIDTH`]
    /// characters, the last an ellipsis where it was cut.
    fn message(&self) -> String {
        self.reading
            .as_ref()
            .map(|x| match x.message.char_indices().nth(MESSAGE_WIDTH - 1) {
                Some((at, _)) if x.message[at..].chars().nth(1).is_some() => {
                    cell(&format!("{}…", &x.message[..at]))
                }
                _ => cell(&x.message),
            })
            .unwrap_or_default()
    }

    /// Every number of the reading but those at zero, as `key=value`,
    /// space-separated.
    fn numbers(&self) -> String {
        self.numbers_but(&[])
    }

    /// Every number of the reading but those at zero and the keys named,
    /// which a section renders in columns of their own, as `key=value`,
    /// space-separated.
    fn numbers_but(&self, shown: &[&str]) -> String {
        self.reading
            .as_ref()
            .map(|x| {
                x.numbers
                    .iter()
                    .filter(|(k, v)| !shown.contains(&k.as_str()) && v.as_f64() != 0.0)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    }

    /// The `sessions` fact as `id=count` pairs, the session by its first
    /// eight characters, a call without one as `-`.
    fn sessions(&self) -> String {
        self.facts
            .get("sessions")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|s| {
                        let id = s["session"].as_str().unwrap_or("");
                        let id = if id.is_empty() { "-" } else { short(id) };
                        format!("{id}={}", s["denials"])
                    })
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

/// The budget section: one row per `[[crate]]`, then `scripts/**` against
/// `[budget] scripts`, whose ceiling cell is empty while a person has not
/// written the row.
fn budget(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    let mut rows: Vec<Row> = manifest
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
        .collect();
    rows.push(Row::new(
        &[
            ("crate", json!(SCRIPTS)),
            (
                "ceiling",
                json!(manifest.budget.scripts.as_ref().map(|s| s.ceiling)),
            ),
        ],
        latest_of(readings, "budget", SCRIPTS),
    ));
    rows
}

/// The rows whose reading says its crate is not yet created, folded into
/// one at the end of the section (#64): twenty rows of "not yet created"
/// carry one fact, and the session block has a cap. The row's `key` cell
/// says "N crates not yet created", its reading is a skip naming them,
/// stamped at the newest of the folded, and a lone row stays as it is.
fn fold_uncreated(rows: Vec<Row>, key: &str) -> Result<Vec<Row>> {
    let uncreated = |row: &Row| {
        row.reading.as_ref().is_some_and(|r| {
            r.message
                .starts_with(&format!("{}: {NOT_YET_CREATED}", r.scope))
        })
    };
    if rows.iter().filter(|r| uncreated(r)).count() < 2 {
        return Ok(rows);
    }
    let (folded, mut kept): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(uncreated);
    let readings: Vec<&Reading> = folded.iter().filter_map(|r| r.reading.as_ref()).collect();
    let names: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    let Some(newest) = readings.iter().max_by_key(|r| r.at.to_string()) else {
        return Ok(kept);
    };
    let count = readings.len();
    let mut reading = Reading::new(&newest.instrument, &format!("{count} crates"));
    reading.at = newest.at;
    reading.message = format!("{NOT_YET_CREATED}: {}", names.join(", "));
    reading.count("crates", count as u64)?;
    kept.push(Row::new(
        &[(key, json!(format!("{count} crates {NOT_YET_CREATED}")))],
        Some(reading),
    ));
    Ok(kept)
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

/// The `phpt` section: per subset, the reading off the log that stands
/// for the track `root` is on, [`reading::Chain::standing`], and never a
/// retired branch's appended after it (#787). The dashboard's data step
/// reads this rendering as JSON rather than taking the log's last line.
fn standing(store: &Store, root: &Path) -> Result<Vec<Row>> {
    let log = store.log()?;
    let chain = reading::Chain::of_head(root)?;
    let rows = chain.standing(&log.readings, "phpt").into_iter();
    Ok(rows
        .map(|r| Row::new(&[("scope", json!(r.scope))], Some(r.clone())))
        .collect())
}

/// The `track` section: the last run of `xtask track work`, read off the
/// append-only log, whose order is the order the readings were written in
/// (#117). One row per step of that run, then the run's own, scoped to
/// [`track_work::RUN`], which is what closes it; a run still going is
/// every step it has taken so far. The runs before it are in the log and
/// not here: the section is the last run.
fn last_run(log: &Log) -> Vec<Row> {
    let of_the_instrument: Vec<&Reading> = log
        .readings
        .iter()
        .filter(|r| r.instrument == track_work::INSTRUMENT)
        .collect();
    let closed = |readings: &[&Reading]| readings.iter().rposition(|r| r.scope == track_work::RUN);
    let end = closed(&of_the_instrument).map_or(of_the_instrument.len(), |at| at + 1);
    let start = closed(&of_the_instrument[..end.saturating_sub(1)]).map_or(0, |at| at + 1);
    of_the_instrument[start..end]
        .iter()
        .map(|r| Row::new(&[("ticket", json!(r.scope))], Some((*r).clone())))
        .collect()
}

/// The `usage` section (#210): the last reading of `instrument` under each
/// scope, read off the append-only log as the `track` section is, since a
/// retired worktree's readings reach the track's store through its log
/// alone; one row per scope, a ticket's number or a session's id, in
/// scope order.
fn last_per_scope(log: &Log, instrument: &str) -> Vec<Row> {
    let mut last: BTreeMap<&str, &Reading> = BTreeMap::new();
    for reading in log.readings.iter().filter(|r| r.instrument == instrument) {
        last.insert(reading.scope.as_str(), reading);
    }
    last.into_iter()
        .map(|(scope, r)| Row::new(&[("scope", json!(scope))], Some(r.clone())))
        .collect()
}

/// The `permits` section (harness-design.md §4.6, §10 rule 6): each rule
/// `permit` judges, its last finding in the store, and the denials the
/// append-only log holds per session, newest session first, and in total;
/// then the query behind CI's §7 section: the log per instrument, its
/// readings and failures, and whether none failed in the last fourteen
/// days, which makes it a candidate for silent.
fn permits(
    manifest: &Manifest,
    readings: &[Reading],
    log: &Log,
    format: Format,
    this: Option<&str>,
) -> Result<String> {
    let rules = permit_rows(manifest, readings, &log.readings, this);
    let window = manifest.telemetry.as_ref().map(|t| t.silent_days);
    let instruments = instruments(&log.readings, window)?;
    Ok(match format {
        Format::Json => serde_json::to_string_pretty(&json!({
            "session": this,
            "silent_days": window,
            "unreadable": log.unreadable,
            "rules": rules.iter().map(Row::json).collect::<Result<Vec<_>>>()?,
            "instruments": instruments.iter().map(InstrumentLine::json).collect::<Vec<_>>(),
        }))?,
        Format::Md if rules.is_empty() && instruments.is_empty() => format!("{NO_READING}\n"),
        Format::Md => {
            let mut out = table(PERMITS, &rules);
            let _ = writeln!(out);
            let (head, rule) = if window.is_some() {
                (" silent |", "---|")
            } else {
                ("", "")
            };
            let _ = writeln!(
                out,
                "| instrument | readings | failures | last failure |{head}"
            );
            let _ = writeln!(out, "|---|---|---|---|{rule}");
            for i in &instruments {
                let candidate = match i.silent {
                    Some(true) => " candidate |",
                    Some(false) => "  |",
                    None => "",
                };
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} |{candidate}",
                    cell(&i.name),
                    i.readings,
                    i.failures,
                    i.last_failure.as_deref().unwrap_or("")
                );
            }
            if window.is_none() {
                let _ = writeln!(
                    out,
                    "\nThe candidates for silent are not rendered: `[telemetry] silent_days` is unset in seams.toml (harness-design.md §7)."
                );
            }
            if let Some(note) = log.unreadable_note() {
                let _ = writeln!(out, "\n{note}");
            }
            out
        }
    })
}

/// One row per rule `permit` judges by: its last finding, the denials per
/// session and in total, the calls handed to the next candidate, and, when
/// `this` names a session, its count. The manifest's rules, then permit's
/// own three, which no manifest row names and which a section that read
/// the manifest alone would leave unsaid (#16).
///
/// The two columns are two questions and #118 is what came of answering
/// them with one number: `denials` counts what a rule refused, `handed on`
/// counts the calls a build passed to the candidate that rebuilds, which
/// judged them. A walk is not a refusal, so the second never adds to the
/// first.
fn permit_rows(
    manifest: &Manifest,
    readings: &[Reading],
    log: &[Reading],
    this: Option<&str>,
) -> Vec<Row> {
    let manifests = manifest
        .rules
        .iter()
        .filter(|rule| permit::judged_at_the_call(rule))
        .map(|rule| (rule.id.clone(), rule.adr.iter().collect::<Vec<_>>()));
    let permits = [
        permit::REGION_RULE,
        permit::STALE_RULE,
        permit::UNJUDGED_RULE,
    ]
    .into_iter()
    .map(|id| (id.to_owned(), Vec::new()));
    manifests
        .chain(permits)
        .map(|(id, adr)| {
            let sessions = per_session(log, &id, reading::Status::Fail);
            let denials: usize = sessions.iter().map(|(_, n)| n).sum();
            let handed_on: usize = per_session(log, &id, reading::Status::Skip)
                .iter()
                .map(|(_, n)| n)
                .sum();
            let this_session = this.map_or(Value::Null, |session| {
                json!(
                    sessions
                        .iter()
                        .find(|(s, _)| s == session)
                        .map_or(0, |(_, n)| *n)
                )
            });
            Row::new(
                &[
                    ("rule", json!(id)),
                    ("adr", json!(adr)),
                    ("denials", json!(denials)),
                    ("handed_on", json!(handed_on)),
                    ("this_session", this_session),
                    (
                        "sessions",
                        json!(
                            sessions
                                .iter()
                                .map(|(s, n)| json!({ "session": s, "denials": n }))
                                .collect::<Vec<_>>()
                        ),
                    ),
                ],
                latest(readings, |r| {
                    r.instrument == permit::INSTRUMENT && r.rule == id
                }),
            )
        })
        .collect()
}

/// The readings of one rule at one status in the log, counted per session
/// and ordered by each session's last one, newest first; the log is
/// append-only, so its order is time's. `Fail` is what a rule denied,
/// `Skip` what a build handed to the next candidate.
fn per_session(log: &[Reading], rule: &str, status: reading::Status) -> Vec<(String, usize)> {
    let mut seen: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (i, r) in log.iter().enumerate() {
        if r.instrument == permit::INSTRUMENT && r.rule == rule && r.status == status {
            let entry = seen.entry(&r.session).or_insert((i, 0));
            entry.0 = i;
            entry.1 += 1;
        }
    }
    let mut sessions: Vec<_> = seen.into_iter().collect();
    sessions.sort_by_key(|(_, (last, _))| std::cmp::Reverse(*last));
    sessions
        .into_iter()
        .map(|(s, (_, n))| (s.to_owned(), n))
        .collect()
}

/// One instrument's line of what CI renders (harness-design.md §7).
struct InstrumentLine {
    name: String,
    readings: usize,
    failures: usize,
    last_failure: Option<String>,
    /// No failure within the manifest's window; none when it has no window.
    silent: Option<bool>,
}

impl InstrumentLine {
    fn json(&self) -> Value {
        json!({
            "instrument": self.name,
            "readings": self.readings,
            "failures": self.failures,
            "last_failure": self.last_failure,
            "silent": self.silent,
        })
    }
}

/// The log per instrument, in name order: readings, failures, the last
/// failure's time, and, when the manifest has a window, whether no failure
/// lies within it of now.
fn instruments(log: &[Reading], window_days: Option<u64>) -> Result<Vec<InstrumentLine>> {
    let since = match window_days {
        Some(days) => {
            let days = i64::try_from(days).context("silent_days is past the calendar")?;
            Some(reading::epoch_seconds(&reading::now()?)? - days * 86_400)
        }
        None => None,
    };
    let mut names: Vec<&str> = log.iter().map(|r| r.instrument.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let of: Vec<&Reading> = log.iter().filter(|r| r.instrument == name).collect();
            let failed: Vec<&Reading> = of
                .iter()
                .copied()
                .filter(|r| r.status == reading::Status::Fail)
                .collect();
            let silent = match since {
                Some(since) => {
                    let mut recent = false;
                    for r in &failed {
                        recent |= reading::epoch_seconds(&r.at)? >= since;
                    }
                    Some(!recent)
                }
                None => None,
            };
            Ok(InstrumentLine {
                name: name.to_owned(),
                readings: of.len(),
                failures: failed.len(),
                last_failure: failed.iter().map(|r| r.at.to_string()).max(),
                silent,
            })
        })
        .collect()
}

/// The weekly exit bar table (harness-design.md §9): one row per `[[week]]`
/// of the manifest, or the one `--week` names, each read the bar names
/// judged against the latest reading of its instrument over its scope.
fn week_section(
    manifest: &Manifest,
    readings: &[Reading],
    week: Option<usize>,
    format: Format,
) -> Result<String> {
    let weeks: Vec<(usize, &Week)> = match week {
        Some(n) => {
            let row = n
                .checked_sub(1)
                .and_then(|i| manifest.weeks.get(i))
                .with_context(|| {
                    format!(
                        "--week {n}: the manifest's [[week]] table has {} rows",
                        manifest.weeks.len()
                    )
                })?;
            vec![(n, row)]
        }
        None => manifest
            .weeks
            .iter()
            .enumerate()
            .map(|(i, w)| (i + 1, w))
            .collect(),
    };
    let rows: Vec<(usize, &Week, Vec<Judged>)> = weeks
        .into_iter()
        .map(|(n, w)| (n, w, w.reads.iter().map(|r| judge(r, readings)).collect()))
        .collect();
    Ok(match format {
        Format::Json => serde_json::to_string_pretty(
            &rows
                .iter()
                .map(|(n, w, reads)| {
                    json!({
                        "week": n,
                        "days": w.days,
                        "text": w.text,
                        "verdict": verdict_of(reads).as_str(),
                        "reads": reads.iter().map(Judged::json).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>(),
        )?,
        Format::Md if rows.is_empty() => format!("{NO_READING}\n"),
        Format::Md => {
            let mut out = String::new();
            let _ = writeln!(out, "| week | days | reading | bar | readings |");
            let _ = writeln!(out, "|---|---|---|---|---|");
            for (n, w, reads) in &rows {
                let _ = writeln!(
                    out,
                    "| {n} | {} | {} | {} | {} |",
                    cell(&w.days),
                    verdict_of(reads).as_str(),
                    cell(&w.text),
                    reads
                        .iter()
                        .map(Judged::text)
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            out
        }
    })
}

/// What a bar's row says: pass when every read holds, fail when one
/// contradicts, "no reading" while one is unmeasured and none contradicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    NoReading,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::NoReading => NO_READING,
        }
    }
}

fn verdict_of(reads: &[Judged]) -> Verdict {
    if reads.iter().any(|r| r.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if reads.iter().any(|r| r.verdict == Verdict::NoReading) {
        Verdict::NoReading
    } else {
        Verdict::Pass
    }
}

/// One read of a bar against the store: the reading it found, if any, and
/// the verdict, with the number and the bound it missed when it failed one.
struct Judged<'a> {
    read: &'a Measure,
    reading: Option<Reading>,
    verdict: Verdict,
    /// The number read, when the read names one and the reading has it.
    value: Option<reading::Number>,
    /// `< min` or `> max`, when the number missed a bound.
    missed: Option<String>,
}

impl Judged<'_> {
    fn json(&self) -> Value {
        json!({
            "instrument": self.read.instrument,
            "scope": self.read.scope,
            "number": self.read.number,
            "verdict": self.verdict.as_str(),
            "status": self.reading.as_ref().map(|r| r.status.as_str()),
            "value": self.value,
            "missed": self.missed,
        })
    }

    /// The read as one cell: `phpt Zend/tests percent=42.5 < 60`.
    fn text(&self) -> String {
        let head = format!("{} {}", self.read.instrument, self.read.scope);
        match (&self.read.number, self.value) {
            (Some(number), Some(value)) => format!(
                "{head} {number}={value}{}",
                self.missed
                    .as_deref()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default()
            ),
            _ => format!("{head} {}", self.verdict.as_str()),
        }
    }
}

/// Judge one read: a status read holds on pass and fails on fail; a number
/// read holds within its bounds, and never on a reading that failed, since
/// a partial run's number is not a bar's. A skip, or a reading without the
/// number, is no reading.
fn judge<'a>(read: &'a Measure, readings: &[Reading]) -> Judged<'a> {
    let reading = latest_of(readings, &read.instrument, &read.scope);
    let mut judged = Judged {
        read,
        reading: None,
        verdict: Verdict::NoReading,
        value: None,
        missed: None,
    };
    let Some(reading) = reading else {
        return judged;
    };
    match &read.number {
        None => {
            judged.verdict = match reading.status {
                reading::Status::Pass => Verdict::Pass,
                reading::Status::Fail => Verdict::Fail,
                reading::Status::Skip => Verdict::NoReading,
            };
        }
        Some(number) => {
            if let Some(value) = reading.numbers.get(number).copied() {
                let n = value.as_f64();
                judged.value = Some(value);
                judged.missed = match (read.min, read.max) {
                    (Some(min), _) if n < min => Some(format!("< {min}")),
                    (_, Some(max)) if n > max => Some(format!("> {max}")),
                    _ => None,
                };
                judged.verdict =
                    if judged.missed.is_some() || reading.status == reading::Status::Fail {
                        Verdict::Fail
                    } else {
                        Verdict::Pass
                    };
            }
        }
    }
    judged.reading = Some(reading);
    judged
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

/// The paths of the working tree's status the `git` section prints below
/// the branch's line: a merge that stages hundreds of files would
/// otherwise push the local facts over the engine's cap alone (#822).
const STATUS_LINES: usize = 20;

/// The `git` section, volatile: the last ten commits and the status of
/// the working tree, as git prints them, cut after [`STATUS_LINES`] paths
/// with the number left out named. The JSON's `status` is cut the same
/// way, the number beside it as `status_omitted`.
fn git_section(root: &Path, format: Format) -> Result<String> {
    let log = git(root, &["log", "--oneline", "-10"], None)?;
    let status = git(root, &["status", "--short", "--branch"], None)?;
    let mut lines: Vec<&str> = status.lines().collect();
    let omitted = lines.len().saturating_sub(STATUS_LINES + 1);
    lines.truncate(STATUS_LINES + 1);
    Ok(match format {
        Format::Json => serde_json::to_string_pretty(&json!({
            "log": log.lines().collect::<Vec<_>>(),
            "status": lines,
            "status_omitted": omitted,
        }))?,
        Format::Md => {
            let kept = lines.join("\n");
            let more = match omitted {
                0 => String::new(),
                n => format!("… and {n} more; git status --short\n"),
            };
            format!(
                "```\n$ git log --oneline -10\n{log}\n$ git status --short --branch\n{kept}\n{more}```\n"
            )
        }
    })
}

/// The `oracle` section: the pin as this machine has it (#36), from the
/// pin and the process's environment alone. Four rows: the tree's
/// variable by name, its value and whether it is set; the promise's
/// likewise; the pin's path and whether it is a directory; and where
/// [`crate::oracle::Source::tree`] found the tree, the variable's or the
/// pin's, or the absence it named. CI reads the two names off the JSON,
/// `tree.variable` and `promise.variable`, and sets them under those
/// names, so the workflow spells neither.
fn oracle_section(pin: &Pin, format: Format) -> Result<String> {
    let source = &pin.source;
    let named = source.named();
    let promise = std::env::var_os(PROMISE_VARIABLE).map(|v| v.to_string_lossy().into_owned());
    let pinned = source.path();
    let found = source.tree();
    let at = match &found {
        Ok(tree) if named == Some(tree.as_path()) => Some("variable"),
        Ok(_) => Some("pin"),
        Err(_) => None,
    };
    let set = |is: bool| if is { "set" } else { "unset" };
    let display = |p: Option<&Path>| p.map(|p| p.display().to_string());
    Ok(match format {
        Format::Json => serde_json::to_string_pretty(&json!({
            "tree": {
                "variable": TREE_VARIABLE,
                "value": display(named),
                "set": named.is_some(),
            },
            "promise": {
                "variable": PROMISE_VARIABLE,
                "value": promise,
                "set": promise.is_some(),
            },
            "pin": {
                "path": pinned.display().to_string(),
                "directory": pinned.is_dir(),
            },
            "found": {
                "path": found.as_ref().ok().map(|p| p.display().to_string()),
                "at": at,
                "error": found.as_ref().err().map(|e| format!("{e:#}")),
            },
        }))?,
        Format::Md => {
            let mut out = String::new();
            let _ = writeln!(out, "| fact | variable | value | state |");
            let _ = writeln!(out, "|---|---|---|---|");
            let _ = writeln!(
                out,
                "| tree | {TREE_VARIABLE} | {} | {} |",
                cell(&display(named).unwrap_or_default()),
                set(named.is_some())
            );
            let _ = writeln!(
                out,
                "| promise | {PROMISE_VARIABLE} | {} | {} |",
                cell(promise.as_deref().unwrap_or_default()),
                set(promise.is_some())
            );
            let _ = writeln!(
                out,
                "| pin |  | {} | {} |",
                cell(&pinned.display().to_string()),
                if pinned.is_dir() {
                    "a directory"
                } else {
                    "not a directory"
                }
            );
            let (path, state) = match &found {
                Ok(tree) => (
                    tree.display().to_string(),
                    match at {
                        Some("variable") => format!("named by {TREE_VARIABLE}"),
                        _ => "the pin's path".to_owned(),
                    },
                ),
                Err(e) => (String::new(), format!("absent: {e:#}")),
            };
            let _ = writeln!(out, "| found |  | {} | {} |", cell(&path), cell(&state));
            out
        }
    })
}

/// One string as a Markdown cell: pipes escaped, one line.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The newest reading that matches, by its `at`. The store holds one file
/// per instrument and scope, so a rule measured by two instruments has two
/// readings; the newer is shown. `at` is always UTC to the second in one
/// format (`reading::now`), so its text orders as its instant does.
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

/// The first eight characters of a session id, enough to tell sessions
/// apart in a row.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// The `ticket` section: one row per ticket the instrument read, with
/// what closed it, the commit and the session, from the store alone.
const TICKET: Columns = (
    &[
        "ticket", "reading", "numbers", "commit", "session", "message",
    ],
    |r| {
        vec![
            r.fact("scope"),
            r.status(),
            r.numbers(),
            r.reading
                .as_ref()
                .map(|x| x.commit.clone())
                .unwrap_or_default(),
            r.reading
                .as_ref()
                .map(|x| short(&x.session).to_owned())
                .unwrap_or_default(),
            r.message(),
        ]
    },
);

/// The numbers the `track` section shows in columns of their own: what
/// the ticket's session spent (#210), blank where no usage reading said.
const SPENT: [&str; 3] = ["usd", "tokens", "turn"];

/// The `track` section: one row per step of the last run, the ticket it
/// worked, its outcome, what its session spent, the commit its branch is
/// at and the reason; the run's own row says `run`, carries its counts
/// and the `usd` total.
const TRACK: Columns = (
    &[
        "ticket", "reading", "usd", "tokens", "turn", "numbers", "commit", "message",
    ],
    |r| {
        vec![
            r.fact("ticket"),
            r.status(),
            r.number("usd"),
            r.number("tokens"),
            r.number("turn"),
            r.numbers_but(&SPENT),
            r.reading
                .as_ref()
                .map(|x| x.commit.clone())
                .unwrap_or_default(),
            r.message(),
        ]
    },
);

/// The `usage` section: the last reading per scope, the session it was
/// taken in, the turn it reached, its tokens and its usd, when, and the
/// window's fill as the message.
const USAGE: Columns = (
    &["scope", "session", "turn", "tokens", "usd", "at", "message"],
    |r| {
        vec![
            r.fact("scope"),
            r.reading
                .as_ref()
                .map(|x| short(&x.session).to_owned())
                .unwrap_or_default(),
            r.number("turn"),
            r.number("tokens"),
            r.number("usd"),
            r.reading
                .as_ref()
                .map(|x| x.at.to_string())
                .unwrap_or_default(),
            r.message(),
        ]
    },
);

const PERMITS: Columns = (
    &[
        "rule",
        "adr",
        "reading",
        "at",
        "denials",
        "handed on",
        "this session",
        "sessions",
        "message",
    ],
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
            r.fact("handed_on"),
            r.fact("this_session"),
            r.sessions(),
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

#[cfg(test)]
mod tests {
    use super::{LOCAL_SECTIONS, local_template};

    /// The opener of a region by name, if the text has one.
    fn opener<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        text.lines()
            .find(|l| l.starts_with("<!-- computed ") && l.contains(&format!(" name={name}")))
    }

    #[test]
    fn the_local_template_is_the_eight_volatile_sections() {
        let template = local_template();
        assert_eq!(LOCAL_SECTIONS.len(), 8, "harness-design.md §5 says eight");
        for name in LOCAL_SECTIONS {
            let line = opener(&template, name).unwrap();
            assert!(line.contains(" volatile "), "{line}");
            assert!(line.contains(&format!("status --section {name}")), "{line}");
        }
        assert_eq!(
            template.matches("<!-- /computed -->").count(),
            LOCAL_SECTIONS.len()
        );
        let ignored = std::fs::read_to_string(crate::built_root().join(".gitignore")).unwrap();
        assert!(
            ignored.contains("/.claude/harness.local.md"),
            "the local file is gitignored"
        );
    }
}
