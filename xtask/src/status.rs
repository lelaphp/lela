//! `xtask status`, the one renderer (harness-design.md §4.6). It reads the
//! manifest, the divergence registry, the store and its log, and what the
//! repository and the pinned tree hold on this machine (the `git`, `oracle`
//! and `parity` sections), only ever reading them: never the tracker or the
//! network, never a build or a run. It computes nothing else; a section
//! that needs a command of its own is a missing instrument.
//! `--session` prints the map and the local facts for the function hook's
//! `prompt.context` (§5), as one text or, in JSON, as the two blocks the
//! hook hands the engine, laying the local file down from its template
//! when the clone has none.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

mod conditions;
pub(crate) mod every;
mod facts;
mod graph;
mod invocation;
mod parity;
mod review;
mod tickets;
mod workloads;
pub(crate) use every::PerReading;

use crate::api_check::{self, lock::Lock};
use crate::cli::{Format, Section};
use crate::divergence::{Counts, Registry, Retires, Status};
use crate::git::git;
use crate::layout;
use crate::manifest::{LockCeiling, Manifest, SCRIPTS, Seam};
use crate::oracle::hosts::{EngineHost, HOSTS};
use crate::oracle::{Closure, PROMISE_VARIABLE, Pin, Platform, TREE_VARIABLE, Through};
use crate::permit;
use crate::phpt;
use crate::reading::{self, Log, Reading, Store, joined};
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
/// §5's eight and the review coverage line, the instrument readings first
/// and `port-map` with them since harness revision 31, a reading that
/// moves with the store like the others and is otherwise renderable only
/// by name. `review`, one line of the coverage `worktree land` writes, is
/// the ninth, as Round 4's Q35 ruled; §5's sentence that counts them waits
/// on a person's wording. `mutants` waits on its instrument and `permits`
/// is rendered by name, its "this session" line kept (harness revision
/// 180). `ci` and `tickets` come last, which ask GitHub, so they are read
/// where the local file renders and never at a prompt.
pub(crate) const LOCAL_SECTIONS: [&str; 9] = [
    "budget",
    "phpt",
    "witness",
    "port-map",
    "divergence",
    "review",
    "git",
    "ci",
    "tickets",
];
/// What `--session` says under a local file whose regions have no sums.
const UNRENDERED: &str = "The local facts above are unrendered: `computed run .claude/harness.local.md` \
fills them, once a person has run `computed trust` for this clone where it has none (harness-design.md §11).";

/// What a section is rendered against: this checkout, its store and what
/// its machine holds, or the tree alone, as a tracked region carries it.
/// Against the tree no reading is read, so the render is the same bytes on
/// a full worktree and a fresh clone and only a tracked file moves it
/// (harness-design.md §5, revision 6), and a table drops its
/// `reading` column, which could only say `no reading`.
#[derive(Debug, Clone, Copy)]
pub enum Against<'a> {
    Checkout(&'a Store),
    Tree,
}

impl<'a> Against<'a> {
    /// The checkout's readings, or none against the tree. A file of the
    /// store that is no reading costs its own row and is named on stderr,
    /// never the section: stdout is the rendering a region holds,
    /// and a tracked region never carries this machine's store.
    fn readings(self) -> Result<Vec<Reading>> {
        let Against::Checkout(store) = self else {
            return Ok(Vec::new());
        };
        let files = store.readings()?;
        if let Some(note) = files.unreadable_note(store.root()) {
            eprintln!("{note}");
        }
        Ok(files.readings)
    }

    /// The columns a table leaves out: `reading` against the tree.
    fn omitted(self) -> &'static [&'static str] {
        match self {
            Against::Checkout(_) => &[],
            Against::Tree => &["reading"],
        }
    }

    /// The checkout's store, for a section [`refuse_under_tracked`] refuses
    /// against the tree before it asks.
    fn checkout(self) -> Result<&'a Store> {
        match self {
            Against::Checkout(store) => Ok(store),
            Against::Tree => {
                bail!("a section of this checkout asked for its store against the tree")
            }
        }
    }
}

/// Refuses `section` under `--tracked` where its rows are this checkout's
/// alone, its store's log or what its machine holds, which no tracked
/// region carries.
fn refuse_under_tracked(section: Section, against: Against<'_>) -> Result<()> {
    let alone = matches!(
        section,
        Section::Phpt
            | Section::Track
            | Section::Usage
            | Section::Permits
            | Section::Git
            | Section::Oracle
            | Section::Corpus
            | Section::Parity
            | Section::Tickets
            | Section::Ci
            | Section::Review
    );
    if alone && matches!(against, Against::Tree) {
        bail!(
            "the {} section is this checkout's, and --tracked renders the tree alone",
            every::name(section)
        );
    }
    Ok(())
}

/// Render one section to a string in the asked format; `session_id` names
/// "this session" of the `permits` section, and belongs to no other.
#[allow(clippy::too_many_arguments)]
pub fn render(
    manifest: &Manifest,
    registry: &Registry,
    against: Against<'_>,
    root: &Path,
    section: Section,
    format: Format,
    session_id: Option<&str>,
    uncut: bool,
) -> Result<String> {
    let format = Table::of(format)?;
    if session_id.is_some() && section != Section::Permits {
        bail!("--session-id names this session of the permits section; use --section permits");
    }
    refuse_under_tracked(section, against)?;
    let readings = against.readings()?;
    let (rows, columns): (Vec<Row>, Columns) = match section {
        Section::Rules => (
            rules(manifest, &readings),
            if uncut { RULES } else { &RULES[..RULE_TEXT] },
        ),
        Section::Locks => (locks(manifest, root, &readings)?, LOCKS),
        Section::Budget => (
            fold_uncreated(budget(manifest, &readings), "crate")?,
            BUDGET,
        ),
        Section::Divergence => {
            let out = divergence(manifest, registry, format, uncut)?;
            return Ok(match format {
                Table::Md => format!("{out}\n{}\n", facts::moves(root, registry, tickets::DAYS)),
                Table::Json => out,
            });
        }
        Section::Phpt => {
            let rows = fold_uncreated(standing(manifest, against.checkout()?, root)?, "scope")?;
            match format {
                Table::Md => (compared(outermost(rows)), PHPT),
                Table::Json => (rows, INSTRUMENT),
            }
        }
        // A section named as its instrument is: the readings of that
        // instrument, a crate's not yet created folded where it has one.
        s @ (Section::Witness | Section::Test | Section::Mutants) => (
            fold_uncreated(instrument(&readings, &every::name(s)), "scope")?,
            INSTRUMENT,
        ),
        s @ (Section::Bench | Section::PortMap | Section::Crosscheck) => {
            (instrument(&readings, &every::name(s)), INSTRUMENT)
        }
        Section::Ticket => (
            instrument(&last_of_each(against, &readings, "ticket")?, "ticket"),
            TICKET,
        ),
        Section::Tickets => {
            let week = tickets::render(against.checkout()?, root, format)?;
            return Ok(match format {
                Table::Md => format!("{week}{}\n", facts::tracker(root, tickets::DAYS)),
                Table::Json => week,
            });
        }
        Section::Ci => return Ok(facts::ci(root)),
        Section::Review => return review::render(manifest, against.checkout()?, root, format),
        Section::Track => (
            last_run(&against.checkout()?.history(track_work::INSTRUMENT)?),
            TRACK,
        ),
        Section::Usage => (
            last_per_scope(
                &against.checkout()?.history(usage::INSTRUMENT)?,
                usage::INSTRUMENT,
            ),
            USAGE,
        ),
        Section::Permits => {
            let store = against.checkout()?;
            return permits(
                manifest,
                &readings,
                (&store.history(permit::INSTRUMENT)?, &permit::judged(store)),
                &store.tallies()?,
                format,
                session_id,
                uncut,
            );
        }
        Section::Bundle => (bundle(manifest, &readings), BUNDLE),
        Section::Crates => return graph::crates(manifest, root, format, uncut),
        Section::Edges => return graph::edges(manifest, format),
        Section::Modules => return modules(manifest, &readings, format),
        Section::Git => return git_section(root, format),
        Section::Oracle => return oracle_section(manifest.pin()?, format),
        Section::Corpus => return corpus_section(root, manifest.pin()?, format),
        Section::Invocation => return invocation::render(root, format),
        Section::Conditions => return conditions::render(root, format),
        Section::Workloads => return workloads::render(root, format),
        Section::Parity => {
            return parity::render(manifest, against.checkout()?, root, format);
        }
    };
    Ok(match format {
        Table::Json => {
            serde_json::to_string_pretty(&rows.iter().map(Row::json).collect::<Result<Vec<_>>>()?)?
        }
        Table::Md if rows.is_empty() => format!("{NO_READING}\n"),
        Table::Md => {
            let omit = against.omitted();
            match settled_rows(section).filter(|_| !uncut) {
                Some((name, settled)) => folded(columns, rows, name, settled, omit),
                None => table(columns, &rows, omit),
            }
        }
    })
}

/// The two formats a section's standing rows render in; `--format
/// jsonl` is `--every`'s, one line per reading ([`every`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Md,
    Json,
}

impl Table {
    /// The table format `format` asks for; `jsonl` is refused here, being
    /// `--every`'s.
    fn of(format: Format) -> Result<Table> {
        match format {
            Format::Md => Ok(Table::Md),
            Format::Json => Ok(Table::Json),
            Format::Jsonl => bail!("--format jsonl renders one row per reading; add --every"),
        }
    }
}

/// The sections that grow by a row per crate, each with its name on the
/// command line and what its settled rows are, in the words of the line
/// that counts them. Each is a section of the local facts, which the
/// engine caps as one block (§5), and a row per crate is a block that
/// outgrows the cap as the crates land; so without `--uncut` the rows a
/// session acts on are rendered, and the settled are one line.
fn settled_rows(section: Section) -> Option<(&'static str, &'static str)> {
    match section {
        Section::Budget => Some((
            "budget",
            "crates pass, each with more than a tenth of its ceiling to spare",
        )),
        Section::Phpt => Some(("phpt", "subsets pass, none below the target it is held to")),
        Section::Witness => Some(("witness", "crates pass")),
        Section::Mutants => Some(("mutants", "crates pass")),
        _ => None,
    }
}

/// A budget row within a tenth of its ceiling is rendered whatever its
/// reading says: it is the crate the next commit pushes over.
const NEAR_CEILING: f64 = 0.9;

/// Whether a row is settled: its reading passes, no comparison folded
/// into it by [`compared`] fails, and a budget row has more than a tenth of
/// its ceiling to spare. A row with no reading, a skip, a crate not yet
/// created and a failure are never settled.
fn settled(row: &Row) -> bool {
    let passes = row
        .reading
        .as_ref()
        .is_some_and(|r| r.status == reading::Status::Pass);
    let compared_clean = phpt::Comparison::ALL.iter().all(|how| {
        !row.fact(how.as_str())
            .starts_with(reading::Status::Fail.as_str())
    });
    let lines = row
        .reading
        .as_ref()
        .and_then(|r| r.numbers.get("lines"))
        .map(|n| n.as_f64());
    let ceiling = row.facts.get("ceiling").and_then(Value::as_f64);
    let spare = match (lines, ceiling) {
        (Some(lines), Some(ceiling)) => lines < ceiling * NEAR_CEILING,
        _ => true,
    };
    passes && compared_clean && spare
}

/// The rows a session acts on as a table, then one line counting the
/// settled rows left out and naming the command that renders every row,
/// so the section grows by the rows that need a look and not by the
/// crates that landed. A section with every row settled is the line alone.
fn folded(columns: Columns, rows: Vec<Row>, name: &str, what: &str, omit: &[&str]) -> String {
    let (settled, shown): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(settled);
    if settled.is_empty() {
        return table(columns, &shown, omit);
    }
    let (mut out, more) = if shown.is_empty() {
        (String::new(), "")
    } else {
        (table(columns, &shown, omit) + "\n", " more")
    };
    writeln!(
        out,
        "{}{more} {what}; every row: `cargo xtask status --section {name} --uncut`.",
        settled.len()
    )
    .ok();
    out
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

/// The map and the local facts, the latter followed by the week's ticket
/// closes in one line ([`tickets`]) and, when `session_id` is
/// known and the log holds a denial of its own, by the rules this session
/// fired with their counts (§4.6, "this session"). As Markdown
/// the two are one text, the map first; as JSON they are the object
/// `{"map", "facts"}`, since the engine caps a context block and the hook
/// hands them over as two. The map is tracked and must exist; the
/// local file is written from `local_template` when the clone has none or
/// its [`skeleton`] is not the template, so a template that moved reaches
/// a clone that rendered the old one (harness revision 180), and the
/// hook's `computed run` has the file to render next time.
pub fn session(
    manifest: &Manifest,
    store: &Store,
    root: &Path,
    session_id: Option<&str>,
    format: Format,
) -> Result<String> {
    let format = Table::of(format)?;
    let mut map = fs::read_to_string(root.join(MAP)).with_context(|| format!("reading {MAP}"))?;
    if !map.ends_with('\n') {
        map.push('\n');
    }
    let local_path = root.join(LOCAL);
    let template = local_template();
    if !fs::read_to_string(&local_path).is_ok_and(|laid| skeleton(&laid) == template) {
        crate::make_parent(&local_path)?;
        fs::write(&local_path, &template).with_context(|| format!("writing {LOCAL}"))?;
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
    // What a person's file carries that parses but does not validate, so
    // an edit is seen before the next call meets it (ruling 4 of harness
    // revision 184).
    let defects = manifest.defects();
    if !defects.is_empty() {
        write!(facts, "\n## defects\n\n").ok();
        for defect in defects {
            writeln!(facts, "- {defect}").ok();
        }
    }
    if let Some(id) = session_id {
        let log = store.history(permit::INSTRUMENT)?;
        let rows: Vec<Row> = permit_rows(
            manifest,
            &store.readings()?.readings,
            (&log.readings, &BTreeMap::new()),
            Some(id),
        )
        .into_iter()
        .filter(|r| r.facts.get("this_session").and_then(Value::as_u64) > Some(0))
        .collect();
        if !rows.is_empty() {
            write!(
                facts,
                "\n## permits this session\n\n{}",
                table(THIS_SESSION, &rows, &[])
            )
            .ok();
        }
        if let Some(note) = log.unreadable_note() {
            writeln!(facts, "\n{note}").ok();
        }
    }
    Ok(match format {
        Table::Json => serde_json::to_string(&json!({ "map": map, "facts": facts }))?,
        Table::Md => format!("{map}\n{facts}"),
    })
}

/// A local file as it was laid: each region's body taken out and its
/// closer's sums with it, so a rendering of the template is the template.
fn skeleton(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("<!-- /computed") {
            inside = false;
            out.push_str("<!-- /computed -->\n");
        } else if !inside {
            inside = line.starts_with("<!-- computed ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
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
        write!(
            out,
            "\n## {name}\n\n<!-- computed exec cmd=\"cargo xtask status --section {name}\" volatile name={name} | do not edit; run computed -->\n<!-- /computed -->\n"
        ).ok();
    }
    out
}

/// What the divergence section says under its counts when it shows the
/// `open` rows alone: where the rest is read, reasons whole.
const EVERY_ROW: &str = "Every row with its reason whole: \
`cargo xtask status --section divergence --uncut`, which `docs/divergence.md` renders.";

/// What the divergence section says under its counts when a cause other
/// than debt has rows: that the total leaves them out (ruled 2026-10-07).
const APART: &str = "A row of another cause is counted on its own line after the total: \
`php-ub` records php's undefined behaviour or crash, `by-design` a difference the design or an ADR \
chose, `oracle-build` one of the oracle's build until what its `retires` names.";

/// What the divergence section says of the total: the debt rows, against
/// the manifest's `[divergence] target`, or that the manifest names none.
fn against_target(manifest: &Manifest) -> String {
    match &manifest.divergence {
        Some(table) => format!(
            "The total is the debt rows, held to the day-30 target of under {} \
             (`[divergence] target` in seams.toml).",
            table.target
        ),
        None => "The total is the debt rows; seams.toml has no `[divergence] target`, \
                 which a person writes, so they are held to none."
            .to_owned(),
    }
}

/// The widest a reading's message renders in a Markdown cell, in
/// characters. A Markdown table is what a session scans, and the local
/// facts it is handed at start are eight sections under the engine's cap
/// on one block (§5); so a row says its message in one line of at
/// most this many, and a number the reading holds at zero, which says
/// nothing happened, is left out of its `numbers` cell. `--format json`
/// is the reading whole, every number and the message uncut. A
/// rule's text is rendered as written. An `open` divergence's reason is
/// cut to the same width, the only rows the facts block lists;
/// every reason stays whole in `docs/divergence.toml`, under `--uncut`
/// (which `docs/divergence.md` renders) and under `--format json`
/// (Mitchell, 2026-09-26).
const MESSAGE_WIDTH: usize = 120;

/// `text` as a cell of at most [`MESSAGE_WIDTH`] characters, the last an
/// ellipsis where it was cut.
fn clipped(text: &str) -> String {
    match text.char_indices().nth(MESSAGE_WIDTH - 1) {
        Some((at, _)) if text[at..].chars().nth(1).is_some() => cell(&format!("{}…", &text[..at])),
        _ => cell(text),
    }
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
        let reading = self
            .reading
            .as_ref()
            .map_or(Ok(Value::Null), Reading::to_json)?;
        object.insert("reading".to_owned(), reading);
        Ok(Value::Object(object))
    }

    /// One fact as a Markdown cell: a list renders as ADR numbers.
    fn fact(&self, key: &str) -> String {
        match self.facts.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => cell(s),
            Some(Value::Array(a)) => joined(
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|n| format!("ADR-{n}")),
                ", ",
            ),
            Some(other) => other.to_string(),
        }
    }

    fn status(&self) -> String {
        let reading = self.reading.as_ref();
        reading.map_or(NO_READING, |r| r.status.as_str()).to_owned()
    }

    /// What `cell` makes of the reading, empty where there is none.
    fn of(&self, cell: impl Fn(&Reading) -> String) -> String {
        self.reading.as_ref().map(cell).unwrap_or_default()
    }

    /// The reading's message as a cell of at most [`MESSAGE_WIDTH`]
    /// characters, the last an ellipsis where it was cut.
    fn message(&self) -> String {
        self.of(|x| clipped(&x.message))
    }

    /// Every number of the reading but those at zero and the keys named,
    /// which a section renders in columns of their own, as `key=value`,
    /// space-separated.
    fn numbers_but(&self, shown: &[&str]) -> String {
        self.of(|x| {
            joined(
                x.numbers
                    .iter()
                    .filter(|(k, v)| !shown.contains(&k.as_str()) && v.as_f64() != 0.0)
                    .map(|(k, v)| format!("{k}={v}")),
                " ",
            )
        })
    }

    /// The `sessions` fact as `id=count` pairs, newest first, the session
    /// by its first eight characters, a call without one as `-`; with
    /// `keep`, the newest `keep` and the rest counted as `+N more`, so the
    /// cell does not grow with the append-only log.
    fn sessions(&self, keep: Option<usize>) -> String {
        self.facts
            .get("sessions")
            .and_then(Value::as_array)
            .map(|list| {
                let shown = keep.unwrap_or(list.len()).min(list.len());
                let mut pairs = list[..shown]
                    .iter()
                    .map(|s| {
                        let id = s["session"].as_str().unwrap_or("");
                        let id = if id.is_empty() { "-" } else { short(id) };
                        format!("{id}={}", s["denials"])
                    })
                    .collect::<Vec<_>>();
                if list.len() > shown {
                    pairs.push(format!("+{} more", list.len() - shown));
                }
                pairs.join(" ")
            })
            .unwrap_or_default()
    }

    fn number(&self, key: &str) -> String {
        self.of(|x| {
            x.numbers
                .get(key)
                .map(ToString::to_string)
                .unwrap_or_default()
        })
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
                // A clippy reading names every `deny_lints` rule that
                // reaches its crate, comma-separated.
                latest(readings, |r| r.rule.split(", ").any(|id| id == rule.id)),
            )
        })
        .collect()
}

/// One row per seam: its lock, the items the tree's lock lists, counted
/// as api-check counts them, its ceiling, and the api-check reading.
fn locks(manifest: &Manifest, root: &Path, readings: &[Reading]) -> Result<Vec<Row>> {
    manifest
        .seams
        .iter()
        .map(|seam| {
            let ceiling = manifest.lock_ceiling(seam);
            Ok(Row::new(
                &[
                    ("seam", json!(seam.name)),
                    ("lock", json!(seam.lock)),
                    (
                        "items",
                        json!(listed(manifest, root, seam, ceiling.as_ref())?),
                    ),
                    ("ceiling", ceiling.map_or(Value::Null, |n| json!(n.items))),
                ],
                latest_of(readings, api_check::INSTRUMENT, &seam.name),
            ))
        })
        .collect()
}

/// The items `seam`'s lock lists in the tree, or none while no crate is
/// its home or its file is not there yet: a fact of the tree, so the map
/// renders it against an empty store and its region names the lock.
fn listed(
    manifest: &Manifest,
    root: &Path,
    seam: &Seam,
    ceiling: Option<&LockCeiling>,
) -> Result<Option<usize>> {
    let Some(home) = manifest.lock_home(&seam.lock) else {
        return Ok(None);
    };
    let path = root.join(layout::path(&home)).join(&seam.lock);
    if !path.is_file() {
        return Ok(None);
    }
    let lock = Lock::read(&path)?;
    Ok(Some(api_check::Counted::of(&lock.lines, ceiling).items()))
}

/// The budget section: one row per `[[crate]]`, then `scripts/**` against
/// `[budget] scripts`, whose ceiling cell is empty while a person has not
/// written the row.
fn budget(manifest: &Manifest, readings: &[Reading]) -> Vec<Row> {
    let scripts = manifest.budget.scripts.as_ref().map(|s| s.ceiling);
    (manifest.crates.iter())
        .map(|krate| (krate.name.as_str(), Some(krate.budget)))
        .chain([(SCRIPTS, scripts)])
        .map(|(name, ceiling)| {
            let facts = [("crate", json!(name)), ("ceiling", json!(ceiling))];
            Row::new(&facts, latest_of(readings, "budget", name))
        })
        .collect()
}

/// The rows whose reading says its crate is not yet created, folded into
/// one at the end of the section: twenty rows of "not yet created"
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

/// The divergence section: the registry's rows, a covering row's id with
/// how many ids it covers beside its own, then counts by kind (rows) and
/// status (columns) of the debt rows with their total, a covering row
/// counted once, and a line per other cause after it. The total is
/// what the day-30 target reads, held to the manifest's `[divergence]
/// target`. It reads no reading; the registry is the truth and this is its
/// view (harness-design.md §4.3).
///
/// Under `uncut` the rows are every row with its reason whole, which is
/// what `docs/divergence.md` renders. Without it they are the `open` rows
/// alone, since `done` fails while one touches the scope, followed by the
/// command that reads the rest: the local facts carry this, and it grows
/// with the kinds and not with each accepted row.
fn divergence(
    manifest: &Manifest,
    registry: &Registry,
    format: Table,
    uncut: bool,
) -> Result<String> {
    let counts = registry.counts();
    match format {
        Table::Json => {
            let rows: Vec<Value> = registry
                .rows()
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "kind": r.kind.as_str(),
                        "cause": r.cause.as_str(),
                        "retires": r.retires.map(Retires::as_str),
                        "status": r.status.as_str(),
                        "since": r.since,
                        "covers": r.covers,
                        "reason": r.reason,
                    })
                })
                .collect();
            let lines: Map<String, Value> = (count_lines(&counts).into_iter())
                .map(|(name, of)| {
                    let by = Status::ALL.into_iter().zip(of);
                    let cells = by.map(|(s, n)| (s.as_str().to_owned(), json!(n)));
                    (name.to_owned(), Value::Object(cells.collect()))
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({
                "rows": rows,
                "counts": lines,
                "target": manifest.divergence.as_ref().map(|d| d.target),
            }))?)
        }
        Table::Md => {
            let mut out = String::new();
            let shown: Vec<_> = registry
                .rows()
                .iter()
                .filter(|r| uncut || r.status == Status::Open)
                .collect();
            let reason: fn(&str) -> String = if uncut { cell } else { clipped };
            let lines = shown.iter().map(|row| {
                let id = match row.covers.len() {
                    0 => cell(row.id.as_str()),
                    n => format!("{} (covers {n})", cell(row.id.as_str())),
                };
                let said = [row.kind.as_str(), row.cause.as_str(), row.status.as_str()];
                let [kind, cause, status] = said.map(str::to_owned);
                [
                    id,
                    kind,
                    cause,
                    status,
                    row.since.clone(),
                    reason(&row.reason),
                ]
            });
            if !shown.is_empty() {
                let head = ["id", "kind", "cause", "status", "since", "reason"];
                writeln!(out, "{}", grid(&head, lines)).ok();
            }
            counts_table(&counts, &mut out);
            writeln!(out, "\n{}", against_target(manifest)).ok();
            if !counts.apart().is_empty() {
                writeln!(out, "\n{APART}").ok();
            }
            if !uncut {
                writeln!(out, "\n{EVERY_ROW}").ok();
            }
            Ok(out)
        }
    }
}

/// The divergence section's counts: a line per kind a debt row is of, the
/// total of those, which the day-30 target reads, then a line per other
/// cause a row is of (ruled 2026-10-07).
fn counts_table(counts: &Counts, out: &mut String) {
    let lines = (count_lines(counts).into_iter()).map(|(name, [open, accepted, fixed])| {
        let counted = [open, accepted, fixed, open + accepted + fixed];
        std::iter::once(name.to_owned()).chain(counted.map(|n| n.to_string()))
    });
    out.push_str(&grid(
        &["kind or cause", "open", "accepted", "fixed", "rows"],
        lines,
    ));
}

/// The divergence counts, a line each: per kind a debt row is of, the
/// total of those, then per other cause a row is of, each line's counts
/// in [`Status::ALL`]'s order.
fn count_lines(counts: &Counts) -> Vec<(&'static str, [u64; 3])> {
    let of = |n: &dyn Fn(Status) -> u64| Status::ALL.map(n);
    let mut lines: Vec<_> = (counts.debt_kinds().into_iter())
        .map(|kind| (kind.as_str(), of(&|s| counts.debt(kind, s))))
        .collect();
    lines.push(("total", of(&|s| counts.total(s))));
    for cause in counts.apart() {
        lines.push((cause.as_str(), of(&|s| counts.of(cause, None, s))));
    }
    lines
}

/// Of `name`'s readings among `readings`, the scope files, each scope's
/// last reading: its file's, or the index's latest where a land put a
/// later one there ([`Store::last_of`]). What the `ticket` section
/// renders, so a ticket whose close was landed reads as the close and not
/// as the file's older box state (Round 1, Q12, c2.i).
fn last_of_each(against: Against<'_>, readings: &[Reading], name: &str) -> Result<Vec<Reading>> {
    let Against::Checkout(store) = against else {
        return Ok(readings.to_vec());
    };
    (readings.iter().filter(|r| r.instrument == name))
        .map(|r| Ok(store.last_of(name, &r.scope)?.unwrap_or_else(|| r.clone())))
        .collect()
}

/// Every reading of one instrument, one row per scope, the numbers as
/// `key=value` pairs; a `witness` record of the verdicts `--accept` wrote
/// is no crate's reading and is left out ([`crate::witness::is_accepted`]). Sections whose instrument does not exist yet
/// (`phpt`, `witness`, `mutants`) render this way from the store alone.
fn instrument(readings: &[Reading], name: &str) -> Vec<Row> {
    let mut scopes: Vec<&str> = readings
        .iter()
        .filter(|r| r.instrument == name && !crate::witness::is_accepted(r))
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
/// retired branch's appended after it. This is xtask's rule; the
/// dashboard keeps one of its own, the latest reading in time on the
/// chain, read off the store's log itself and not off this section, as its
/// README says. A reading that [`outlived`] its subset is
/// left out.
fn standing(manifest: &Manifest, store: &Store, root: &Path) -> Result<Vec<Row>> {
    let log = store.history("phpt")?;
    let chain = reading::Chain::of_head(root)?;
    let rows = chain.standing(&log.readings, "phpt").into_iter();
    Ok(rows
        .map(|r| Row::new(&[("scope", json!(r.scope))], Some(r.clone())))
        .filter(|row| !outlived(manifest, row))
        .collect())
}

/// Whether a phpt row is the reading of a crate whose row owns no subset
/// any more, as `php_sapi`'s is since each host's row took its own
/// directory (ADR-0008): those tests are the hosts' rows' now, and
/// the reading that outlived its subset would stand beside them for good.
fn outlived(manifest: &Manifest, row: &Row) -> bool {
    scope_of(row).is_some_and(|scope| {
        manifest
            .crate_named(&scope.subset)
            .is_some_and(|krate| krate.subsets().is_empty())
    })
}

/// A phpt row's scope, decoded; none for one outside the grammar, which
/// is then a row of its own that nothing folds, narrows or retires.
fn scope_of(row: &Row) -> Option<phpt::Scope> {
    row.facts.get("scope")?.as_str()?.parse().ok()
}

/// The rows of the phpt section a session is handed: each subset's, but
/// not a narrower run inside one the section already shows
/// (`Zend/tests/coalesce` inside `Zend/tests`, `ext/standard/tests/streams`
/// inside `ext/standard`), whose tests the enclosing reading already
/// holds. A narrower run is a builder's working reading, and its row would
/// stand in the local facts for good, which have a cap. `--format json`
/// keeps every reading.
fn outermost(rows: Vec<Row>) -> Vec<Row> {
    let base = |row: &Row| -> String {
        scope_of(row).map_or_else(|| row.fact("scope"), |scope| scope.subset)
    };
    let bases: BTreeSet<String> = rows.iter().map(base).collect();
    rows.into_iter()
        .filter(|row| {
            let own = base(row);
            !bases
                .iter()
                .any(|other| *other != own && own.starts_with(&format!("{other}/")))
        })
        .collect()
}

/// The counts that size a run rather than judge it; the subset's row
/// carries them, so a comparison's cell leaves them out.
const SIZE: [&str; 8] = [
    "passed", "percent", "run", "skipped", "tests", "unread", "warned", "xfailed",
];

/// The phpt rows as Markdown: one per subset, each comparison folded into
/// its subset's row. A comparison is its status and the numbers
/// that judge, `held=4833` against the base, `borked=4 failed=2` under the
/// oracle; a comparison with no row of a run to fold into, which a
/// control's comparison against a commit is too, since the control's own
/// row is folded, stays a row of its own. The
/// message is a failing row's, the scope it repeats cut off, since a
/// passing row's says what its numbers say. So a subset costs one row of
/// the local facts, not three.
fn compared(rows: Vec<Row>) -> Vec<Row> {
    let scope = |row: &Row| row.fact("scope");
    let scopes: BTreeSet<String> = rows.iter().map(scope).collect();
    let of = |row: &Row| {
        let (run, how) = scope_of(row)?.compares()?;
        let own = run.compares().is_none() && scopes.contains(&run.to_string());
        own.then(|| (run.to_string(), how))
    };
    let (folded, mut kept): (Vec<Row>, Vec<Row>) =
        rows.into_iter().partition(|row| of(row).is_some());
    for row in folded {
        let (Some((base, how)), Some(r)) = (of(&row), &row.reading) else {
            continue;
        };
        let cell = format!("{} {}", r.status.as_str(), row.numbers_but(&SIZE));
        if let Some(own) = kept.iter_mut().find(|k| scope(k) == base) {
            own.facts
                .insert(how.as_str().to_owned(), json!(cell.trim_end()));
        }
    }
    kept
}

/// The `track` section: the last run of `xtask track work`, read off the
/// append-only log, whose order is the order the readings were written in.
/// One row per step of that run, then the run's own, scoped to
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

/// The `usage` section: the last reading of `instrument` under each
/// scope, read off the append-only log as the `track` section is, since a
/// retired worktree's readings reach the main checkout's store through its
/// log alone; one row per scope, a ticket's number or a session's id, in
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
///
/// Without `uncut` a rule's `sessions` cell is its newest
/// [`SESSIONS_SHOWN`] sessions and a count of the rest, every session
/// one `--uncut` away and in the JSON whatever the flag.
fn permits(
    manifest: &Manifest,
    readings: &[Reading],
    (log, judged): (&Log, &BTreeMap<String, u64>),
    tallies: &BTreeMap<String, Vec<(reading::Status, String)>>,
    format: Table,
    this: Option<&str>,
    uncut: bool,
) -> Result<String> {
    let rules = permit_rows(manifest, readings, (&log.readings, judged), this);
    let window = manifest.telemetry.as_ref().map(|t| t.silent_days);
    let instruments = instruments(tallies, window)?;
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&json!({
            "session": this,
            "silent_days": window,
            "unreadable": log.unreadable,
            "rules": rules.iter().map(Row::json).collect::<Result<Vec<_>>>()?,
            "instruments": instruments.iter().map(InstrumentLine::json).collect::<Vec<_>>(),
        }))?,
        Table::Md if rules.is_empty() && instruments.is_empty() => format!("{NO_READING}\n"),
        Table::Md => {
            let whole: Column = Cell("sessions", |r| r.sessions(None));
            let columns: Vec<Column> = (PERMITS.iter())
                .map(|&c| {
                    if uncut && c.head() == "sessions" {
                        whole
                    } else {
                        c
                    }
                })
                .collect();
            let mut out = table(&columns, &rules, &[]);
            writeln!(out).ok();
            let head = [
                "instrument",
                "readings",
                "failures",
                "last reading",
                "last failure",
            ];
            out.push_str(&grid(
                &head,
                instruments.iter().map(|i| {
                    let (readings, failures) = (i.readings.to_string(), i.failures.to_string());
                    let failed = i.last_failure.clone().unwrap_or_default();
                    [
                        cell(&i.name),
                        readings,
                        failures,
                        i.last_reading.clone(),
                        failed,
                    ]
                }),
            ));
            match window {
                None => writeln!(
                    out,
                    "\nThe candidates for silent are not rendered: `[telemetry] silent_days` is unset in seams.toml (harness-design.md §7)."
                ),
                Some(days) => {
                    let named: Vec<&str> = (instruments.iter())
                        .filter(|i| i.silent == Some(true))
                        .map(|i| i.name.as_str())
                        .collect();
                    let named = if named.is_empty() { "none".to_owned() } else { named.join(", ") };
                    writeln!(
                        out,
                        "\nThe candidates for silent, read within {days} days with no failure in them (harness-design.md §7): {named}. `{}` is never one, since its reading cannot fail.",
                        usage::INSTRUMENT
                    )
                }
            }
            .ok();
            if let Some(note) = log.unreadable_note() {
                writeln!(out, "\n{note}").ok();
            }
            out
        }
    })
}

/// One row per rule `permit` judges by: its last finding, the denials per
/// session and in total, the calls handed to the next candidate, and, when
/// `this` names a session, its count. The manifest's rules, then permit's
/// own three, which no manifest row names and which a section that read
/// the manifest alone would leave unsaid.
///
/// The two columns are two questions, and answering them with one number
/// read a stale build that handed its call on as a failing reading:
/// `denials` counts what a rule refused, `handed on` counts the calls a
/// build passed to the candidate that rebuilds, which judged them. A walk
/// is not a refusal, so the second never adds to the
/// first. `judged` is the calls the rule judged, the denominator of its
/// denials, and `at pre-commit` what a diff judged there was refused,
/// never an agent's call.
fn permit_rows(
    manifest: &Manifest,
    readings: &[Reading],
    (log, judged): (&[Reading], &BTreeMap<String, u64>),
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
            let (pre_commit, sessions): (Vec<_>, Vec<_>) =
                per_session(log, &id, reading::Status::Fail)
                    .into_iter()
                    .partition(|(s, _)| s == permit::PRE_COMMIT);
            let denials: usize = sessions.iter().map(|(_, n)| n).sum();
            let at_pre_commit: usize = pre_commit.iter().map(|(_, n)| n).sum();
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
                    (
                        "judged",
                        json!(judged.get(&id).copied().unwrap_or_default()),
                    ),
                    ("at_pre_commit", json!(at_pre_commit)),
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
    /// When the instrument was last read, failing or not.
    last_reading: String,
    last_failure: Option<String>,
    /// Read within the manifest's window and no failure within it; none
    /// when it has no window, and for an instrument whose reading cannot
    /// fail, which is never quiet.
    silent: Option<bool>,
}

impl InstrumentLine {
    fn json(&self) -> Value {
        json!({
            "instrument": self.name,
            "readings": self.readings,
            "failures": self.failures,
            "last_reading": self.last_reading,
            "last_failure": self.last_failure,
            "silent": self.silent,
        })
    }
}

/// The log per instrument, in name order: readings, failures, the last
/// reading's and the last failure's time, and, when the manifest has a
/// window, whether a reading and no failure lies within it of now; read
/// off each line's status and time alone ([`Store::tallies`]).
fn instruments(
    tallies: &BTreeMap<String, Vec<(reading::Status, String)>>,
    window_days: Option<u64>,
) -> Result<Vec<InstrumentLine>> {
    let since = match window_days {
        Some(days) => {
            let days = i64::try_from(days).context("silent_days is past the calendar")?;
            Some(reading::days_ago(days)?)
        }
        None => None,
    };
    tallies
        .iter()
        .filter(|(_, lines)| !lines.is_empty())
        .map(|(name, lines)| {
            let failed: Vec<&str> = lines
                .iter()
                .filter(|(status, _)| *status == reading::Status::Fail)
                .map(|(_, at)| at.as_str())
                .collect();
            let within =
                |at: &str| -> Result<bool> {
                    let at = at
                        .parse()
                        .context("a log line whose `at` is not a datetime")?;
                    Ok(since
                        .is_some_and(|since| reading::epoch_seconds(&at).is_ok_and(|s| s >= since)))
                };
            let mut recent = (false, false);
            for (status, at) in lines {
                let now = within(at)?;
                recent.0 |= now;
                recent.1 |= now && *status == reading::Status::Fail;
            }
            let silent =
                (since.is_some() && name != usage::INSTRUMENT).then_some(recent == (true, false));
            Ok(InstrumentLine {
                name: name.clone(),
                readings: lines.len(),
                failures: failed.len(),
                last_reading: lines
                    .iter()
                    .map(|(_, at)| at.as_str())
                    .max()
                    .unwrap_or_default()
                    .to_owned(),
                last_failure: failed.iter().max().map(|at| (*at).to_owned()),
                silent,
            })
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

/// The `modules` section: each item lock's lines counted by the
/// module that defines them, from the `groups` of the seam's latest
/// api-check reading, a seam in the manifest's order and its modules the
/// widest first, each with its share of the lock and the commit the
/// reading was taken at, so a count cited before and after a move says
/// which tree it is of. A seam whose reading keeps no groups, one taken
/// before api-check counted them or a sectioned lock's, has no row.
fn modules(manifest: &Manifest, readings: &[Reading], format: Table) -> Result<String> {
    let latest: Vec<(&Seam, Reading)> = manifest
        .seams
        .iter()
        .filter_map(|seam| {
            Some((
                seam,
                latest_of(readings, api_check::INSTRUMENT, &seam.name)?,
            ))
        })
        .collect();
    let mut rows = Vec::new();
    for (seam, reading) in &latest {
        let total: u64 = reading.groups.values().map(|n| every::whole(*n)).sum();
        let first = rows.len();
        for (module, n) in &reading.groups {
            let items = every::whole(*n);
            #[allow(clippy::cast_precision_loss)]
            let share = items as f64 / total as f64;
            rows.push(ModuleRow {
                seam: &seam.name,
                lock: &seam.lock,
                module,
                items,
                share,
                commit: &reading.commit,
                at: reading.at.to_string(),
            });
        }
        rows[first..].sort_by(|a, b| b.items.cmp(&a.items).then_with(|| a.module.cmp(b.module)));
    }
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&rows)?,
        Table::Md if rows.is_empty() => format!("{NO_READING}\n"),
        Table::Md => grid(
            &["seam", "module", "items", "share", "commit"],
            rows.iter().map(|row| {
                let share = format!("{:.1}%", row.share * 100.0);
                let (seam, module) = (cell(row.seam), cell(row.module));
                [
                    seam,
                    module,
                    row.items.to_string(),
                    share,
                    short(row.commit).to_owned(),
                ]
            }),
        ),
    })
}

/// One row of the `modules` section: a module's lines in one seam's lock,
/// and the reading that counted them.
#[derive(Debug, serde::Serialize)]
struct ModuleRow<'a> {
    seam: &'a str,
    lock: &'a str,
    module: &'a str,
    items: u64,
    /// Of the lock's lines, the fraction this module defines.
    share: f64,
    commit: &'a str,
    at: String,
}

/// The paths of the working tree's status the `git` section prints below
/// the branch's line: a merge that stages hundreds of files would
/// otherwise push the local facts over the engine's cap alone.
const STATUS_LINES: usize = 20;

/// The `git` section, volatile: the last ten commits and the status of
/// the working tree, as git prints them, cut after [`STATUS_LINES`] paths
/// with the number left out named. The JSON's `status` is cut the same
/// way, the number beside it as `status_omitted`.
fn git_section(root: &Path, format: Table) -> Result<String> {
    let log = git(root, &["log", "--oneline", "-10"])?;
    let status = git(root, &["status", "--short", "--branch"])?;
    let mut lines: Vec<&str> = status.lines().collect();
    let omitted = lines.len().saturating_sub(STATUS_LINES + 1);
    lines.truncate(STATUS_LINES + 1);
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&json!({
            "log": log.lines().collect::<Vec<_>>(),
            "status": lines,
            "status_omitted": omitted,
        }))?,
        Table::Md => {
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

/// The `oracle` section: the pin as this machine has it, from the
/// pin and the process's environment alone. Seven rows: the tree's
/// variable by name, its value and whether it is set; the promise's
/// likewise; the pin's path and whether it is a directory, or that the
/// pin states none; its tag and commit as `[source]` states
/// them, which the corpus section clones at; where
/// [`crate::oracle::Source::tree`] found the tree, the variable's or the
/// pin's, or the absence it named; and the pinned php, its version, keg
/// path and md5, the Homebrew formula the keg is poured from, `php`
/// where the pin names none, and the bottle of it the keg path names,
/// the version with the formula's revision where it has one. An
/// eighth, `closure`, names the kegs
/// `[closure]` records php loading, each at its version, where the pin
/// records them, with the record's digest, and a `variant` row
/// per `[variant.<name>]` that pins a build for this machine's platform,
/// its path and md5, and the macOS build's closure digest.
/// CI reads the two names off the JSON, `tree.variable` and
/// `promise.variable`, and sets them under those names, so the workflow
/// spells neither; the oracle runner pours the formula `binary` names at
/// its bottle, holds the keg's php to that md5 and pins the keg,
/// pours and pins each keg `closure` names at its version, and
/// lays each of `variants` at its path by its md5.
fn oracle_section(pin: &Pin, format: Table) -> Result<String> {
    let source = &pin.source;
    let php = &pin.oracle;
    let platform = Platform::this();
    let builds: Vec<_> = (pin.variant.iter())
        .filter_map(|(name, variant)| Some((name, variant.build(platform?)?)))
        .collect();
    let named = source.named();
    let promise = std::env::var_os(PROMISE_VARIABLE).map(|v| v.to_string_lossy().into_owned());
    let pinned = source.path();
    let found = source.found();
    let at = match &found {
        Ok((Through::Variable, _)) => Some("variable"),
        Ok((Through::Pin, _)) => Some("pin"),
        Err(_) => None,
    };
    let set = |is: bool| if is { "set" } else { "unset" };
    let display = |p: Option<&Path>| p.map(|p| p.display().to_string());
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&json!({
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
                "path": display(pinned),
                "directory": pinned.is_some_and(Path::is_dir),
                "tag": source.tag,
                "commit": source.commit,
            },
            "found": {
                "path": found.as_ref().ok().map(|(_, p)| p.display().to_string()),
                "at": at,
                "error": found.as_ref().err().map(|e| format!("{e:#}")),
            },
            "binary": {
                "version": php.version,
                "formula": php.formula.to_string(),
                "bottle": php.bottle(),
                "path": php.path.display().to_string(),
                "md5": php.md5,
            },
            "closure": pin.closure.as_ref().map(Closure::kegs),
            "closure_digest": pin.closure.as_ref().map(|c| c.digest().to_string()),
            "variants": builds.iter().map(|(name, build)| ((*name).clone(), json!({
                "platform": platform.map(Platform::key),
                "path": build.path.display().to_string(),
                "md5": build.md5,
                "closure": build.provenance.closure.as_ref().map(ToString::to_string),
            }))).collect::<serde_json::Map<_, _>>(),
        }))?,
        Table::Md => {
            let mut out = "| fact | variable | value | state |\n|---|---|---|---|\n".to_owned();
            let (o, row) = (&mut out, oracle_row);
            let tree = display(named).unwrap_or_default();
            row(o, "tree", TREE_VARIABLE, &tree, set(named.is_some()));
            let promised = promise.as_deref().unwrap_or_default();
            row(
                o,
                "promise",
                PROMISE_VARIABLE,
                promised,
                set(promise.is_some()),
            );
            let dir = match pinned {
                Some(pinned) if pinned.is_dir() => "a directory",
                Some(_) => "not a directory",
                None => "not stated",
            };
            row(o, "pin", "", &display(pinned).unwrap_or_default(), dir);
            row(o, "tag", "", &source.tag, "the pin's");
            row(o, "commit", "", &source.commit, "the pin's");
            let (path, state) = match &found {
                Ok((place, tree)) => (tree.display().to_string(), place.to_string()),
                Err(e) => (String::new(), format!("absent: {e:#}")),
            };
            row(o, "found", "", &path, &state);
            let binary = format!(
                "php {}, md5 {}, poured from the formula {}",
                php.version, php.md5, php.formula
            );
            row(o, "binary", "", &php.path.display().to_string(), &binary);
            closure_row(o, pin.closure.as_ref());
            for (name, build) in &builds {
                let fact = format!("variant {name}");
                let closure = (build.provenance.closure.as_ref()).map(|c| format!(", closure {c}"));
                let md5 = format!("md5 {}{}", build.md5, closure.unwrap_or_default());
                row(o, &fact, "", &build.path.display().to_string(), &md5);
            }
            out
        }
    })
}

/// The oracle section's `closure` row: each keg `[closure]` records and its
/// version, and the record's digest, none where the pin records no closure.
fn closure_row(out: &mut String, closure: Option<&Closure>) {
    let Some(closure) = closure else { return };
    let kegs: Vec<String> = closure
        .kegs()
        .iter()
        .map(|(k, v)| format!("{k} {v}"))
        .collect();
    let (count, digest) = (kegs.len(), closure.digest());
    let count = format!("{count} kegs, the pin's, digest {digest}");
    oracle_row(out, "closure", "", &kegs.join(", "), &count);
}

/// One row of the oracle section's table, its value and state as cells.
fn oracle_row(out: &mut String, fact: &str, variable: &str, value: &str, state: &str) {
    let (value, state) = (cell(value), cell(state));
    writeln!(out, "| {fact} | {variable} | {value} | {state} |").ok();
}

/// The `corpus` section: what CI checks out of the pinned php-src,
/// from the pin, [`phpt::ROOTS`] and the table of ports at `root`. The tag
/// and the commit to clone at, the `.phpt` files the pin counts in the
/// whole tree, which a checkout holding fewer has narrowed, git's sparse
/// patterns, the roots and after them each file a notice under `LICENSES/`
/// is held to ([`crate::licence::sources`]), which the licence check reads
/// in the same checkout, and the cache key, the commit and a digest of the
/// patterns, so a change to either never restores a checkout made by the
/// old ones.
/// Beside the checkout, `targets`: where a workspace keeps the targets
/// under the pin, the main oracle's and each variant's this machine pins,
/// relative to its root, which CI keeps between runs so an engine's
/// reading is held to its subset's target and a bound on `target/` keeps
/// alone; and `hosts`, each host the lane hands a test to by its crate,
/// package and binary ([`HOSTS`]), which CI builds.
fn corpus_section(root: &Path, pin: &Pin, format: Table) -> Result<String> {
    let source = &pin.source;
    let mut patterns: Vec<String> = phpt::ROOTS.iter().map(|root| root.pattern()).collect();
    patterns.extend(
        crate::licence::sources(root)?
            .iter()
            .map(|file| format!("/{file}")),
    );
    let mut digest = sha1_smol::Sha1::new();
    digest.update(patterns.join("\n").as_bytes());
    let digest = digest.digest().to_string();
    let key = format!("php-src-{}-{}", source.commit, &digest[..12]);
    let variants = pin.variant.keys().filter_map(|name| pin.variant(name).ok());
    let pins = std::iter::once(pin.clone()).chain(variants.filter(|v| !v.oracle.md5.is_empty()));
    let targets: Vec<String> = pins
        .map(|p| phpt::target::kept_under(&p).display().to_string())
        .collect();
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&json!({
            "tag": source.tag,
            "commit": source.commit,
            "phpt_files": source.phpt_files,
            "patterns": patterns,
            "key": key,
            "targets": targets,
            "hosts": HOSTS.map(|host| json!({
                "crate": host.krate,
                "package": host.package(),
                "binary": host.binary,
            })),
        }))?,
        Table::Md => {
            let mut out = "| fact | value |\n|---|---|\n".to_owned();
            writeln!(out, "| tag | {} |", cell(&source.tag)).ok();
            writeln!(out, "| commit | {} |", cell(&source.commit)).ok();
            writeln!(out, "| phpt_files | {} |", source.phpt_files).ok();
            writeln!(out, "| patterns | {} |", cell(&patterns.join(" "))).ok();
            writeln!(out, "| key | {key} |").ok();
            writeln!(out, "| targets | {} |", cell(&targets.join(" "))).ok();
            let packages: Vec<String> = HOSTS.map(EngineHost::package).into();
            writeln!(out, "| hosts | {} |", cell(&packages.join(" "))).ok();
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

/// One column of a Markdown table: a fact or a number of the row shown
/// under its own name, or a head and the cell it shows of a row.
#[derive(Clone, Copy)]
enum Column {
    Fact(&'static str),
    Number(&'static str),
    Cell(&'static str, fn(&Row) -> String),
}

impl Column {
    fn head(self) -> &'static str {
        match self {
            Column::Fact(head) | Column::Number(head) | Column::Cell(head, _) => head,
        }
    }

    fn cell(self, row: &Row) -> String {
        match self {
            Column::Fact(key) => row.fact(key),
            Column::Number(key) => row.number(key),
            Column::Cell(_, cell) => cell(row),
        }
    }
}

/// A Markdown table's columns, in order.
type Columns = &'static [Column];

use Column::{Cell, Fact, Number};

const READING: Column = Cell("reading", Row::status);
const NUMBERS: Column = Cell("numbers", |r| r.numbers_but(&[]));
const MESSAGE: Column = Cell("message", Row::message);
const COMMIT: Column = Cell("commit", |r| r.of(|x| x.commit.clone()));
const SESSION: Column = Cell("session", |r| r.of(|x| short(&x.session).to_owned()));
const AT: Column = Cell("at", |r| r.of(|x| x.at.to_string()));

/// The rules as the map carries them: rule, ADR, predicate and reading.
/// The text a rule's failure prints is the last column, which `--uncut`
/// and JSON alone show, so the map holds its room under the engine's cap with
/// every row at "no reading" (harness-design.md §4.6).
/// Where [`RULES`]' text column stands, the columns before it the map's.
const RULE_TEXT: usize = 4;

const RULES: Columns = &[
    Cell("rule", |r| r.fact("id")),
    Fact("adr"),
    Fact("predicate"),
    READING,
    Fact("text"),
];

const LOCKS: Columns = &[
    Fact("seam"),
    Fact("lock"),
    Fact("items"),
    Fact("ceiling"),
    READING,
];

const BUDGET: Columns = &[
    Fact("crate"),
    Number("lines"),
    Fact("ceiling"),
    READING,
    MESSAGE,
];

const INSTRUMENT: Columns = &[Fact("scope"), READING, NUMBERS, MESSAGE];

/// The `phpt` section as Markdown, after [`compared`]: a passing row's
/// message is left out, a failing one's less its scope.
const PHPT: Columns = &[
    Fact("scope"),
    READING,
    NUMBERS,
    Fact("against"),
    Fact("control"),
    Cell("message", |r| {
        let failing = r
            .reading
            .as_ref()
            .filter(|x| x.status != reading::Status::Pass);
        failing.map_or_else(String::new, |x| {
            let own = format!("{}: ", x.scope);
            clipped(x.message.strip_prefix(&own).unwrap_or(&x.message))
        })
    }),
];

/// The first eight characters of a session id, enough to tell sessions
/// apart in a row.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// The `ticket` section: one row per ticket the instrument read, with
/// what closed it, the commit and the session, from the store alone.
const TICKET: Columns = &[
    Cell("ticket", |r| r.fact("scope")),
    READING,
    NUMBERS,
    COMMIT,
    SESSION,
    MESSAGE,
];

/// The numbers the `track` section shows in columns of their own: what
/// the ticket's session spent, blank where no usage reading said.
const SPENT: [&str; 3] = ["usd", "tokens", "turn"];

/// The `track` section: one row per step of the last run, the ticket it
/// worked, its outcome, what its session spent, the commit its branch is
/// at and the reason; the run's own row says `run`, carries its counts
/// and the `usd` total.
const TRACK: Columns = &[
    Fact("ticket"),
    READING,
    Number("usd"),
    Number("tokens"),
    Number("turn"),
    Cell("numbers", |r| r.numbers_but(&SPENT)),
    COMMIT,
    MESSAGE,
];

/// The `usage` section: the last reading per scope, the session it was
/// taken in, the turn it reached, its tokens and its usd, when, and the
/// window's fill as the message.
const USAGE: Columns = &[
    Fact("scope"),
    SESSION,
    Number("turn"),
    Number("tokens"),
    Number("usd"),
    AT,
    MESSAGE,
];

/// The sessions a rule's `sessions` cell names without `--uncut`, the
/// newest; the rest are one `+N more`.
const SESSIONS_SHOWN: usize = 3;

/// The permits section's rules; `--uncut` shows its `sessions` whole.
const PERMITS: Columns = &[
    Fact("rule"),
    Fact("adr"),
    READING,
    AT,
    Fact("denials"),
    Fact("judged"),
    Cell("at pre-commit", |r| r.fact("at_pre_commit")),
    Cell("handed on", |r| r.fact("handed_on")),
    FIRED,
    Cell("sessions", |r| r.sessions(Some(SESSIONS_SHOWN))),
    MESSAGE,
];

/// The rules this session fired, each with its count: the rest of each
/// row is the local facts' `permits` section already.
const THIS_SESSION: Columns = &[Fact("rule"), FIRED];

/// How often a rule fired in this session.
const FIRED: Column = Cell("this session", |r| r.fact("this_session"));

const BUNDLE: Columns = &[
    Fact("crate"),
    Fact("order"),
    Number("extensions"),
    READING,
    MESSAGE,
];

/// The rows as a Markdown table, less the columns `omit` names.
fn table(columns: &[Column], rows: &[Row], omit: &[&str]) -> String {
    let shown: Vec<Column> = (columns.iter().copied())
        .filter(|c| !omit.contains(&c.head()))
        .collect();
    let head: Vec<&str> = shown.iter().map(|c| c.head()).collect();
    grid(
        &head,
        rows.iter().map(|row| shown.iter().map(|c| c.cell(row))),
    )
}

/// A Markdown table: its head, the rule under it, and a line per row of
/// cells, each already escaped.
pub(crate) fn grid<R: IntoIterator<Item = String>>(
    head: &[&str],
    rows: impl IntoIterator<Item = R>,
) -> String {
    let mut out = format!("| {} |\n|{}\n", head.join(" | "), "---|".repeat(head.len()));
    for row in rows {
        writeln!(
            out,
            "| {} |",
            row.into_iter().collect::<Vec<_>>().join(" | ")
        )
        .ok();
    }
    out
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use super::{LOCAL_SECTIONS, Row, compared, local_template, outermost, outlived, skeleton};
    use serde_json::json;

    /// The opener of a region by name, if the text has one.
    fn opener<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        text.lines()
            .find(|l| l.starts_with("<!-- computed ") && l.contains(&format!(" name={name}")))
    }

    /// The `ticket` section renders a ticket's last reading: a close
    /// another checkout took after the file's failing read, landed in the
    /// index and not the file, is the row (c2.i).
    #[test]
    fn the_ticket_section_renders_a_tickets_last_reading_a_landed_one_included() {
        use crate::reading::{Reading, Status, Store};
        use crate::scratch::Tree;
        let ticket = |status: Status, at: &str| {
            let mut reading = Reading::keyed("ticket", "3785", format!("{:040x}", 1)).unwrap();
            reading.status = status;
            reading.at = at.parse().unwrap();
            reading
        };
        let dir = Tree::new("status-ticket-last").unwrap();
        let store = Store::at(dir.path()).in_session(None);
        store
            .record(&[ticket(Status::Fail, "2026-11-01T10:00:00Z")])
            .unwrap();
        let other = Tree::new("status-ticket-last-other").unwrap();
        let theirs = Store::at(other.path()).in_session(None);
        let closed = ticket(Status::Pass, "2026-11-01T11:00:00Z");
        theirs.record(std::slice::from_ref(&closed)).unwrap();
        store.land(&theirs).unwrap();
        let files = store.readings().unwrap().readings;
        assert_eq!(files[0].status, Status::Fail, "the file is as it was");
        let against = super::Against::Checkout(&store);
        assert_eq!(
            super::last_of_each(against, &files, "ticket").unwrap(),
            [closed]
        );
    }

    /// The sections `render` reads by their own name read the instrument
    /// of that name, so the two spellings are held to each other here.
    #[test]
    fn a_section_read_by_its_name_is_named_as_its_instrument_is() {
        use crate::cli::Section;
        for (section, instrument) in [
            (Section::Witness, crate::witness::INSTRUMENT),
            (Section::Test, crate::test::INSTRUMENT),
            (Section::Mutants, "mutants"),
            (Section::Bench, crate::bench::INSTRUMENT),
            (Section::PortMap, crate::port_map::INSTRUMENT),
            (Section::Crosscheck, crate::crosscheck::INSTRUMENT),
        ] {
            assert_eq!(super::every::name(section), instrument);
        }
    }

    #[test]
    fn the_local_template_is_the_nine_volatile_sections() {
        let template = local_template();
        assert_eq!(
            LOCAL_SECTIONS.len(),
            9,
            "harness-design.md §5's eight and the review line Round 4's Q35 added"
        );
        for name in LOCAL_SECTIONS {
            let line = opener(&template, name).unwrap();
            assert!(line.contains(" volatile "), "{line}");
            assert!(line.contains(&format!("status --section {name}")), "{line}");
        }
        for name in ["mutants", "permits"] {
            assert!(
                opener(&template, name).is_none(),
                "{name} is rendered by name"
            );
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

    /// `computed` renders a region as its opener, a blank line, the body,
    /// a blank line and a closer carrying its sums; the skeleton of that is
    /// the template again, and the skeleton of a template with a section
    /// more or less is not, which is what lays the local file down again.
    #[test]
    fn a_rendering_of_the_template_has_the_template_as_its_skeleton() {
        let template = local_template();
        let rendered = template.replace(
            "<!-- /computed -->\n",
            "\n| crate | lines |\n|---|---|\n| xtask | 1 |\n\n<!-- /computed in=ab out=cd -->\n",
        );
        assert_ne!(rendered, template);
        assert_eq!(skeleton(&rendered), template);
        let older = template.replace(
            "\n## git\n",
            "\n## mutants\n\n<!-- computed exec cmd=\"cargo xtask status --section mutants\" volatile name=mutants | do not edit; run computed -->\n<!-- /computed -->\n\n## git\n",
        );
        assert_ne!(skeleton(&older), template, "a section the template dropped");
        let shorter = template.replace("\n## git\n", "\n");
        assert_ne!(skeleton(&shorter), template);
    }

    /// A narrower run inside a subset the section shows folds away, its
    /// comparison with it; a subset nothing encloses stays, whatever its
    /// name shares with another's.
    #[test]
    fn the_phpt_rows_are_the_outermost_subsets() {
        let row = |scope: &str| Row::new(&[("scope", json!(scope))], None);
        let rows = [
            "Zend/tests",
            "Zend/tests@against",
            "Zend/tests/coalesce",
            "Zend/tests/coalesce@against",
            "ext/standard",
            "ext/standard/tests/streams",
            "ext/standard/tests/streams@control",
            "ext/pdo",
            "ext/pdo_sqlite",
        ]
        .map(row);
        let kept: Vec<String> = outermost(rows.into())
            .iter()
            .map(|r| r.facts["scope"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            kept,
            [
                "Zend/tests",
                "Zend/tests@against",
                "ext/standard",
                "ext/pdo",
                "ext/pdo_sqlite"
            ]
        );
    }

    /// A reading of a crate whose row names no subset outlived its subset,
    /// its comparisons with it; a host's, a directory's and an extension's
    /// stand.
    #[test]
    fn a_reading_of_a_crate_with_no_subset_outlived_it() {
        let manifest = crate::manifest::Manifest::load(&crate::built_root()).unwrap();
        let row = |scope: &str| Row::new(&[("scope", json!(scope))], None);
        let dropped: Vec<&str> = [
            "php_sapi",
            "php_sapi@control",
            "sapi/cli",
            "Zend/tests",
            "tests/lang",
            "ext/pdo",
        ]
        .into_iter()
        .filter(|scope| outlived(&manifest, &row(scope)))
        .collect();
        assert_eq!(dropped, ["php_sapi", "php_sapi@control"]);
    }

    /// Each comparison folds into the row of the run it compares, read off
    /// the one grammar: `@against` into the scope without it, a lane's
    /// included, and `@control` into the engine's run, of a lane too. A
    /// control's own
    /// comparison has no row to fold into, its control's being folded, so
    /// it stays a row of its own rather than vanishing.
    #[test]
    fn a_comparison_folds_into_the_row_of_its_run() {
        let row = |scope: &str| {
            let reading = crate::reading::Reading::new(crate::phpt::INSTRUMENT, scope);
            Row::new(&[("scope", json!(scope))], Some(reading))
        };
        let rows = [
            "X",
            "X@against",
            "X@control",
            "X@control@against",
            "X@force",
            "X@force@against",
            "X@control@force",
            "Y@against",
        ]
        .map(row);
        let kept: Vec<(String, bool, bool)> = compared(rows.into())
            .iter()
            .map(|r| {
                let scope = r.facts["scope"].as_str().unwrap_or_default().to_owned();
                let has = |key: &str| r.facts.contains_key(key);
                (scope, has("against"), has("control"))
            })
            .collect();
        let kept: Vec<(&str, bool, bool)> =
            kept.iter().map(|(s, a, c)| (s.as_str(), *a, *c)).collect();
        assert_eq!(
            kept,
            [
                ("X", true, true),
                ("X@control@against", false, false),
                ("X@force", true, true),
                ("Y@against", false, false),
            ]
        );
    }
}
