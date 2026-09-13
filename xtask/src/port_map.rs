//! `xtask port-map`: the Mahout census joined to `api.lock` (design §4.4;
//! harness-design.md §4.2, §7). Every item the 19 `mahout_ext_*` crates
//! call is one row: `same`, kept under its name; `renamed`, with the `api`
//! item that replaces it; or `gone`, with the rule that replaces the need.
//! The join holds each row's `api` item against the lock and counts the
//! rows the port has not taken off the checklist, which is the reading's
//! number. A row is unmatched, the failure §4.4 calls a CI failure from
//! day 3, in either of the two shapes its sentence has: a row naming an
//! `api` item the lock does not carry, and — under `--tree` — an item the
//! census finds that no row names, since "every Mahout item is `same`,
//! `renamed` or `gone`" and an item with no row is none of the three.
//!
//! **Matched, and resolved** (design §4.4). A row is *matched* when its
//! replacement exists: a `same` or `renamed` row's `api` item is a line
//! of the lock, a `gone` row's rule is written. That is the join above,
//! and an unmatched row is the CI failure. *Resolved* is the port's own
//! work, and the design says what it is: a row is resolved when every
//! extension the census counted for it is ported. The census names them
//! per row, the `ext` column beside the count, and *ported* is what the
//! harness reads without a binary — the crate's `extension()` registers
//! at least one native.
//!
//! **A row that names no extension is unresolved.** The committed census
//! carries the count alone and gains the names with the first port, so
//! every row of it names none today. The join reads such a row as on the
//! checklist, which is the branch [`joined`] takes by name: an empty list
//! satisfies "every named extension is ported" for the engine that ports
//! nothing as readily as for the one that ports everything, and a row
//! resolved that way would be resolved by the absence of evidence. The
//! reading counts those rows as `unnamed` beside `unresolved`, so the two
//! shapes of unresolved are told apart in the number itself. The twenty
//! crates `new-ext` laid down register no native, so on this tree the two
//! numbers are one: 651 rows, all unresolved, none named — design §11's
//! row 1–2 reproduced by the measure and not asserted beside it.
//!
//! **When the column lands** (#170), a row carries `ext = [..]` naming as
//! many extensions as it counts, `--tree` regenerates both together, and
//! the row comes off the checklist on the day its last named extension
//! registers a native: the rows one extension called go first and the
//! ones every extension called go last, and the number reaches zero on
//! the day the last inherited extension is ported.
//!
//! **The census.** `data/port-map.v0.toml` is the census as
//! `scripts/census.sh` printed it at Mahout `1b3104a8`, joined by hand to
//! `api.lock` v0 (`data/api-lock-v0.md`). `--tree <path>` regenerates it
//! against the inherited tree, which lives outside this repository, and
//! joins that instead; a tree the census cannot read is a skip of the
//! regeneration and never a pass, and the reading says which of the two
//! it read. The census prints `sites crates item` and no crate names, so
//! a regenerated run carries the counts and a row's `ext` column is still
//! the map's until #170 lands the names in both.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::api_check::lock::Lock;
use crate::manifest::{Crate, Manifest};
use crate::reading::{self, Reading, Store, plural};
use crate::sources;

/// The subcommand, and the reading's `instrument`.
pub const INSTRUMENT: &str = "port-map";
/// The committed census joined to the lock, relative to the workspace root.
pub const SEED: &str = "data/port-map.v0.toml";
/// What regenerates the census against a Mahout tree, relative to the root.
pub const CENSUS: &str = "scripts/census.sh";
/// The lock the census joins to; the manifest says which crate holds it.
pub(crate) const LOCK: &str = "api.lock";

/// What became of one Mahout item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    /// Kept by name: `api` names the same item.
    Same,
    /// `api` names the item that replaces it.
    Renamed,
    /// `rule` says what replaces the need; there is no item.
    Gone,
}

impl Status {
    /// The three statuses, in the census's order.
    pub(crate) const ALL: [Status; 3] = [Status::Same, Status::Renamed, Status::Gone];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Status::Same => "same",
            Status::Renamed => "renamed",
            Status::Gone => "gone",
        }
    }
}

/// One row: a Mahout item, what it costs the port, and what replaces it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Row {
    /// The qualified Mahout item; `{A,B}::m` marks a method name several
    /// value types define, which the census prints by name alone.
    pub(crate) mahout: String,
    /// Call sites in the ext crates, test modules left out.
    pub(crate) sites: usize,
    /// How many extension crates call it.
    pub(crate) crates: usize,
    /// Which of them do, by the name their crate has under `ext/`: the
    /// column that resolves the row (design §4.4). It is regenerated with
    /// `crates` and names as many, so a row carries both or the count
    /// alone; the committed census carries the count alone and gains the
    /// names with the first port, and a row that names none is unresolved
    /// however the port stands.
    #[serde(default)]
    pub(crate) ext: Vec<String>,
    pub(crate) status: Status,
    /// The `api` item that replaces it; `same` and `renamed` carry one.
    #[serde(default)]
    pub(crate) api: Option<String>,
    /// The rule that replaces the need; a `gone` row carries one.
    #[serde(default)]
    pub(crate) rule: Option<String>,
    /// The census table the row sits under, from the `# ----` heading
    /// above it; the parse fills it, the file does not carry it.
    #[serde(skip)]
    pub(crate) table: String,
}

impl Row {
    /// What replaces the item: the `api` item, or the `gone` rule.
    pub(crate) fn replacement(&self) -> &str {
        self.api
            .as_deref()
            .or(self.rule.as_deref())
            .unwrap_or_default()
    }

    /// The name the census prints for this row, the join's key.
    pub(crate) fn key(&self) -> String {
        key(&self.table, &self.mahout)
    }
}

/// The tables `scripts/census.sh` prints, in its order, and the prefix a
/// row's `mahout` name carries for each. The first table's items are
/// printed by method name alone, since a name is matched on any receiver,
/// so the census joined to the lock qualifies them by the value type or
/// types that define them and the join reads the last segment.
const TABLES: [(&str, &str); 12] = [
    (
        "mahout_value methods called from ext crates",
        "mahout_value::",
    ),
    (
        "mahout_value free functions called from ext crates",
        "mahout_value::",
    ),
    (
        "mahout_value associated functions and variants named from ext crates",
        "mahout_value::",
    ),
    ("RequestCtx methods called from ext crates", "RequestCtx::"),
    (
        "RequestCtx public-field members reached from ext crates",
        "RequestCtx::",
    ),
    ("semantics:: items used from ext crates", "semantics::"),
    (
        "mahout_native_abi items used from ext crates",
        "mahout_native_abi::",
    ),
    (
        "mahout_native_macros attributes and their arguments",
        "mahout_native_macros::",
    ),
    (
        "mahout_linker items imported or path-qualified by ext crates",
        "mahout_linker::",
    ),
    (
        "mahout_code_meta items imported or path-qualified by ext crates",
        "mahout_code_meta::",
    ),
    (
        "mahout_native items imported or path-qualified by ext crates",
        "mahout_native::",
    ),
    (
        "other mahout_runtime items imported or called from ext crates",
        "mahout_runtime::",
    ),
];

/// The one table whose items the census prints by method name alone.
const METHODS: &str = TABLES[0].0;

/// The name the census prints for an item of `table`: the value methods
/// join on the last segment, every other table on the name with the
/// table's prefix taken off. The census marks the `ExtState` row it finds
/// as an impl; the suffix is not part of the name.
fn key(table: &str, name: &str) -> String {
    let name = name.strip_suffix(" (impl)").unwrap_or(name);
    if table == METHODS {
        return leaf(name).to_owned();
    }
    let prefix = TABLES
        .iter()
        .find(|(title, _)| *title == table)
        .map_or("", |(_, prefix)| *prefix);
    name.strip_prefix(prefix).unwrap_or(name).to_owned()
}

/// The last segment of a qualified name.
fn leaf(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// The census joined to the lock: the rows of `data/port-map.v0.toml`.
#[derive(Debug, Clone, Default)]
pub(crate) struct PortMap {
    pub(crate) rows: Vec<Row>,
}

/// The grammar of the file: `[[item]]` rows and nothing else.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileShape {
    #[serde(default)]
    item: Vec<Row>,
}

impl PortMap {
    /// Load `data/port-map.v0.toml` from the workspace root.
    pub(crate) fn load(root: &Path) -> Result<PortMap> {
        let path = root.join(SEED);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        PortMap::parse(&text).with_context(|| format!("{}", path.display()))
    }

    /// Parse the file and hold it to the row shape: every row under one of
    /// the census's tables, a `same` or `renamed` row with an `api` item
    /// and no rule, a `gone` row with a rule and no item, and one row per
    /// Mahout item.
    pub(crate) fn parse(text: &str) -> Result<PortMap> {
        let shape: FileShape = toml::from_str(text)?;
        let mut rows = shape.item;
        let tables = tables_of(text);
        if tables.len() != rows.len() {
            bail!(
                "{} rows sit under {} `# ----` headings; every row is under the census table it came from",
                rows.len(),
                tables.len()
            );
        }
        let mut seen = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for (row, table) in rows.iter_mut().zip(tables) {
            row.table = table;
            if !TABLES.iter().any(|(title, _)| *title == row.table) {
                bail!(
                    "the row {:?} sits under {:?}, which is no table {CENSUS} prints",
                    row.mahout,
                    row.table
                );
            }
            match row.status {
                Status::Same | Status::Renamed if row.api.is_none() => {
                    bail!(
                        "the row {:?} is {} and names no api item",
                        row.mahout,
                        row.status.as_str()
                    )
                }
                Status::Gone if row.rule.is_none() => {
                    bail!("the gone row {:?} carries no rule", row.mahout)
                }
                Status::Gone if row.api.is_some() => {
                    bail!("the gone row {:?} names an api item", row.mahout)
                }
                Status::Same | Status::Renamed if row.rule.is_some() => {
                    bail!("the row {:?} is not gone and carries a rule", row.mahout)
                }
                _ => {}
            }
            // The names and the count are one column, regenerated
            // together: a row that names its extensions names as many as
            // it counts, each once. A row that names none is the seed's
            // shape and is held to nothing here.
            let named: BTreeSet<&str> = row.ext.iter().map(String::as_str).collect();
            if named.len() != row.ext.len() {
                bail!(
                    "the row {:?} names an extension twice: {}",
                    row.mahout,
                    row.ext.join(", ")
                );
            }
            if !row.ext.is_empty() && row.ext.len() != row.crates {
                bail!(
                    "the row {:?} counts {} crates and names {}: {}",
                    row.mahout,
                    row.crates,
                    row.ext.len(),
                    row.ext.join(", ")
                );
            }
            // A name the census counts in two tables is two rows: the
            // method `Value::string` and the associated function of that
            // name are counted apart, so a row's identity is its table's.
            if !seen.insert((row.table.clone(), row.mahout.clone())) {
                bail!(
                    "the item {:?} has two rows under {:?}",
                    row.mahout,
                    row.table
                );
            }
            if !keys.insert((row.table.clone(), row.key())) {
                bail!(
                    "the item {:?} joins on {:?}, which another row of {:?} joins on too",
                    row.mahout,
                    row.key(),
                    row.table
                );
            }
        }
        Ok(PortMap { rows })
    }

    /// The row whose census key is `key`, under `table`.
    pub(crate) fn row(&self, table: &str, key: &str) -> Option<&Row> {
        self.rows
            .iter()
            .find(|r| r.table == table && r.key() == key)
    }
}

/// The census table each `[[item]]` sits under, in the file's order, read
/// from the `# ---- <table>` headings between the rows.
fn tables_of(text: &str) -> Vec<String> {
    let mut table = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix("# ---- ") {
            heading.trim().clone_into(&mut table);
        } else if line == "[[item]]" {
            out.push(table.clone());
        }
    }
    out
}

/// One line of a census table as `scripts/census.sh` printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Counted {
    pub(crate) table: String,
    pub(crate) item: String,
    pub(crate) sites: usize,
    pub(crate) crates: usize,
}

/// The rows of a census run, its tables read in the order it prints them.
/// A line under no table — the per-crate lists, the counts, the headers —
/// is passed over, since the tables are what the join reads. A run that
/// printed no table row yields none, which [`read`] refuses.
pub(crate) fn census(text: &str) -> Vec<Counted> {
    let mut table = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            table = TABLES
                .iter()
                .find(|(title, _)| heading.starts_with(title))
                .map_or_else(String::new, |(title, _)| (*title).to_owned());
            continue;
        }
        if table.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(sites), Some(crates), Some(item)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(sites), Ok(crates)) = (sites.parse::<usize>(), crates.parse::<usize>()) else {
            continue;
        };
        // `~` marks a name std also defines, whose count is an upper bound
        // after the receiver filtering; the name is what the join reads.
        let item = item.strip_prefix('~').unwrap_or(item);
        let rest = fields.collect::<Vec<_>>().join(" ");
        let item = if rest.is_empty() {
            item.to_owned()
        } else {
            format!("{item} {rest}")
        };
        out.push(Counted {
            table: table.clone(),
            item,
            sites,
            crates,
        });
    }
    out
}

/// The extensions ported here: the `ext/*` crates the manifest names
/// whose `extension()` registers a native. Which ones they are is the
/// evidence a row is resolved on (design §4.4), so the set is the names
/// and never their number.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ported {
    /// The crates, by the manifest's name for them, `ext/<name>`.
    pub(crate) crates: Vec<String>,
}

impl Ported {
    /// Read the ported extensions under the root.
    pub(crate) fn read(root: &Path, manifest: &Manifest) -> Ported {
        Ported {
            crates: manifest
                .crates
                .iter()
                .filter(|c| c.name.starts_with("ext/") && sources::exists(root, c))
                .filter(|c| registers_a_native(root, c))
                .map(|c| c.name.clone())
                .collect(),
        }
    }

    /// Whether the extension the census calls `ext` is ported: its crate
    /// is `ext/<ext>`, and the port has reached it.
    pub(crate) fn carries(&self, ext: &str) -> bool {
        self.crates
            .iter()
            .any(|c| c.strip_prefix("ext/").unwrap_or(c) == ext)
    }
}

/// The builder steps that register a native on an extension:
/// `abi::ExtensionBuilder::functions`, `::classes` and `::constants` of
/// `api.lock`. The other steps declare state, ini or a lifecycle hook,
/// none of which is a native, and none of which ports a crate.
const REGISTERS: [&str; 3] = [".functions(", ".classes(", ".constants("];

/// Whether a crate's `extension()` registers a native, which is what the
/// design reads *ported* as without a binary (§4.4): one of the crate's
/// sources takes a registering step of the builder. It is read over the
/// sources and not over the body of `extension()` alone, since an
/// extension may build its declarations in a function of its own and its
/// entry is one per crate (ADR-0004), so a registration the crate carries
/// is one `extension()` carries. This tells a ported extension from one
/// `new-ext` laid down, whose `extension()` declares its state and
/// registers nothing; a crate whose sources cannot be listed or read
/// registers none, since a port is evidence and an unread crate is not.
fn registers_a_native(root: &Path, krate: &Crate) -> bool {
    sources::rust(root, krate)
        .unwrap_or_default()
        .iter()
        .any(|path| fs::read_to_string(root.join(path)).is_ok_and(|text| registers(&text)))
}

/// Whether one source registers a native, read line by line: the step is
/// a call on the builder, so it is what a line of the chain carries after
/// the receiver, and a line of prose that spells one is a comment.
pub(crate) fn registers(text: &str) -> bool {
    text.lines()
        .map(str::trim_start)
        .any(|line| !line.starts_with("//") && REGISTERS.iter().any(|step| line.contains(step)))
}

/// Which census the run read.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// The census committed at `data/port-map.v0.toml`.
    Seed,
    /// The census regenerated against the Mahout tree at this path.
    Tree(PathBuf),
    /// The tree was asked for and the census could not read it: the
    /// regeneration is a skip, and the committed census is not dressed up
    /// as one.
    Refused { tree: PathBuf, why: String },
}

impl std::fmt::Display for Source {
    /// What the reading says it read.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Seed => write!(f, "the committed census {SEED}"),
            Source::Tree(tree) => {
                write!(f, "the census regenerated against {}", tree.display())
            }
            Source::Refused { tree, .. } => {
                write!(f, "no census: the tree at {} is unread", tree.display())
            }
        }
    }
}

/// Read the census the run joins: the committed one, or a regenerated one
/// when a tree is named. A tree without `crates/`, or one `scripts/census.sh`
/// refuses, is [`Source::Refused`] and nothing is joined.
pub(crate) fn read(root: &Path, tree: Option<&Path>) -> Result<(Source, Vec<Counted>)> {
    let Some(tree) = tree else {
        return Ok((Source::Seed, Vec::new()));
    };
    let tree = tree.to_path_buf();
    if !tree.join("crates").is_dir() {
        return Ok((
            Source::Refused {
                tree,
                why: "it holds no crates/, so it is no Mahout tree".to_owned(),
            },
            Vec::new(),
        ));
    }
    let recipe = root.join(CENSUS);
    let run = Command::new(&recipe)
        .arg(&tree)
        .current_dir(root)
        .output()
        .with_context(|| format!("running {}", recipe.display()))?;
    if !run.status.success() {
        let said = String::from_utf8_lossy(&run.stderr);
        let said = said
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("without a word")
            .trim();
        let why = format!("{CENSUS} refused it: {said}");
        return Ok((Source::Refused { tree, why }, Vec::new()));
    }
    let rows = census(&String::from_utf8_lossy(&run.stdout));
    if rows.is_empty() {
        return Ok((
            Source::Refused {
                tree,
                why: format!("{CENSUS} read it and printed no table row"),
            },
            Vec::new(),
        ));
    }
    Ok((Source::Tree(tree), rows))
}

/// One row of the join: the row, the counts the census read for it, and
/// the two questions the instrument asks of it.
#[derive(Debug, Clone)]
pub(crate) struct Joined<'a> {
    pub(crate) row: &'a Row,
    pub(crate) sites: usize,
    /// How many extensions called the item, as the census that was read
    /// counts them; which ones they are is the row's `ext` column until
    /// the census prints the names too (#170).
    pub(crate) crates: usize,
    /// Whether the lock carries the row's `api` item; a `gone` row is
    /// matched by its rule.
    pub(crate) matched: bool,
    /// Whether every extension the census counted for the row is ported
    /// (design §4.4).
    pub(crate) resolved: bool,
    /// Whether the census named them at all: a row that names none is
    /// unresolved on that ground and on no other.
    pub(crate) named: bool,
}

/// What one run joined.
#[derive(Debug, Clone)]
pub(crate) struct Join<'a> {
    pub(crate) rows: Vec<Joined<'a>>,
    pub(crate) source: Source,
    /// Items the regenerated census found that the port map has no row
    /// for: the map does not cover the tree.
    pub(crate) uncovered: Vec<String>,
    /// Rows of the port map the regenerated census no longer finds.
    pub(crate) stale: usize,
}

impl Join<'_> {
    /// Rows whose `api` item the lock does not carry, by item.
    pub(crate) fn unmatched(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter(|j| !j.matched)
            .map(|j| j.row.replacement())
            .collect()
    }

    /// Rows an unported extension is still left on: the reading's number
    /// (design §4.4).
    pub(crate) fn unresolved(&self) -> usize {
        self.rows.iter().filter(|j| !j.resolved).count()
    }

    /// Rows the census has named no extension for, which are unresolved
    /// whatever the port has reached; they are all of them until the
    /// column lands (#170).
    pub(crate) fn unnamed(&self) -> usize {
        self.rows.iter().filter(|j| !j.named).count()
    }

    /// How many rows carry each status.
    pub(crate) fn rows_with(&self, status: Status) -> usize {
        self.rows.iter().filter(|j| j.row.status == status).count()
    }
}

/// Join the port map to the lock over the census that was read: every row
/// when it is the committed one, and the rows the regenerated census still
/// finds when it is a tree's, with that census's counts.
pub(crate) fn join<'a>(
    map: &'a PortMap,
    lock: &BTreeSet<String>,
    ported: &Ported,
    source: Source,
    counted: &[Counted],
) -> Join<'a> {
    let mut rows = Vec::new();
    let mut uncovered = Vec::new();
    let mut stale = 0;
    match source {
        Source::Seed => {
            for row in &map.rows {
                rows.push(joined(row, row.sites, row.crates, lock, ported));
            }
        }
        Source::Tree(_) => {
            for found in counted {
                match map.row(&found.table, &key(&found.table, &found.item)) {
                    Some(row) => rows.push(joined(row, found.sites, found.crates, lock, ported)),
                    None => uncovered.push(found.item.clone()),
                }
            }
            stale = map.rows.len().saturating_sub(rows.len());
        }
        // The regeneration was asked for and did not happen, so nothing
        // is joined: the committed census is not read in its place.
        Source::Refused { .. } => {}
    }
    Join {
        rows,
        source,
        uncovered,
        stale,
    }
}

/// One row against the lock and the extensions ported here: matched when
/// its replacement exists, resolved when every extension the census
/// counted for it is ported (design §4.4).
fn joined<'a>(
    row: &'a Row,
    sites: usize,
    crates: usize,
    lock: &BTreeSet<String>,
    ported: &Ported,
) -> Joined<'a> {
    let matched = match &row.api {
        Some(api) => lock.contains(api),
        None => row.rule.is_some(),
    };
    let (named, resolved) = match row.ext.as_slice() {
        // The census counted the extensions and did not name them, so
        // nothing here says which crates the port would have to reach:
        // the row stays on the checklist. This branch is written out
        // rather than left to `all` over an empty list, which is true of
        // the engine that ports nothing as readily as of the one that
        // ports everything (#104).
        [] => (false, false),
        ext => (true, ext.iter().all(|name| ported.carries(name))),
    };
    Joined {
        row,
        sites,
        crates,
        matched,
        resolved,
        named,
    }
}

/// The crate `api.lock` lives with, whose name the reading's scope is and
/// whose directory holds the file the join reads.
pub(crate) fn home_of_the_lock(manifest: &Manifest) -> Result<&Crate> {
    let home = manifest
        .lock_home(LOCK)
        .with_context(|| format!("the manifest names no crate holding {LOCK}"))?;
    manifest
        .crate_named(&home)
        .with_context(|| format!("the manifest names no crate {home}"))
}

/// The reading of one run, at the crate the lock lives with: the counts
/// are its numbers, the unresolved rows the one the lanes read, and a row
/// naming an `api` item the lock does not carry is the failure.
pub(crate) fn reading(join: &Join, ported: &Ported, krate: &Crate, root: &Path) -> Result<Reading> {
    let mut reading = Reading::for_crate(INSTRUMENT, krate, root)?;
    reading.count("rows", join.rows.len() as u64)?;
    for status in Status::ALL {
        reading.count(status.as_str(), join.rows_with(status) as u64)?;
    }
    reading.count("unmatched", join.unmatched().len() as u64)?;
    reading.count("uncovered", join.uncovered.len() as u64)?;
    reading.count("stale", join.stale as u64)?;
    reading.count("unresolved", join.unresolved() as u64)?;
    reading.count("unnamed", join.unnamed() as u64)?;
    let source = &join.source;
    if let Source::Refused { why, .. } = &join.source {
        reading.status = reading::Status::Skip;
        reading.message = format!(
            "{source}: {why}. The regeneration is a skip; {SEED} was not read as one (design §4.4)"
        );
        return Ok(reading);
    }
    let unmatched = join.unmatched();
    let lock = format!("{}/{LOCK}", krate.path().display());
    reading.message = format!(
        "{} rows from {source}, joined to {lock}: {} same, {} renamed, {} gone; {} unresolved, {}",
        join.rows.len(),
        join.rows_with(Status::Same),
        join.rows_with(Status::Renamed),
        join.rows_with(Status::Gone),
        join.unresolved(),
        ported_says(join, ported),
    );
    if !unmatched.is_empty() {
        reading.status = reading::Status::Fail;
        reading.message = format!(
            "{}. {} row{} name an api item {lock} does not carry: {}",
            reading.message,
            unmatched.len(),
            plural(unmatched.len()),
            unmatched.join(", ")
        );
        return Ok(reading);
    }
    if join.stale > 0 {
        reading.message = format!(
            "{}; {} row{} the census no longer finds",
            reading.message,
            join.stale,
            plural(join.stale)
        );
    }
    if !join.uncovered.is_empty() {
        reading.status = reading::Status::Fail;
        reading.message = format!(
            "{}. {} census item{} have no row in {SEED}: {}",
            reading.message,
            join.uncovered.len(),
            plural(join.uncovered.len()),
            join.uncovered.join(", ")
        );
        return Ok(reading);
    }
    reading.status = reading::Status::Pass;
    Ok(reading)
}

/// Why the unresolved count is what it is: which extensions are ported,
/// and how many rows name none of them to be measured against.
fn ported_says(join: &Join, ported: &Ported) -> String {
    let ported = if ported.crates.is_empty() {
        "no extension registers a native, so no row is off the checklist".to_owned()
    } else {
        format!(
            "{} resolved by the {} extension{} ported",
            join.rows.len() - join.unresolved(),
            ported.crates.len(),
            plural(ported.crates.len())
        )
    };
    match join.unnamed() {
        0 => ported,
        unnamed => format!(
            "{ported}; {unnamed} row{} name no extension at all",
            plural(unnamed)
        ),
    }
}

/// One line per row: what became of the item, whether the port has taken
/// it off the checklist, the Mahout item, what replaces it, and the
/// extensions that called it, named where the census names them.
pub(crate) fn render(join: &Join) -> String {
    let mut out = String::new();
    for joined in &join.rows {
        let resolution = if joined.resolved {
            "resolved"
        } else {
            "unresolved"
        };
        let unmatched = if joined.matched {
            ""
        } else {
            "  (not in the lock)"
        };
        let named = if joined.named {
            format!(" ({})", joined.row.ext.join(", "))
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{:<8} {:<11} {} → {}  {} site{}, {} crate{}{named}{unmatched}",
            joined.row.status.as_str(),
            resolution,
            joined.row.mahout,
            joined.row.replacement(),
            joined.sites,
            plural(joined.sites),
            joined.crates,
            plural(joined.crates),
        );
    }
    out
}

/// `xtask port-map`: the census read, joined, printed row by row, and the
/// one reading recorded, whose outcome is the exit.
pub fn command(root: &Path, manifest: &Manifest, store: &Store, tree: Option<&Path>) -> Result<()> {
    let map = PortMap::load(root)?;
    let krate = home_of_the_lock(manifest)?;
    // The lock's items, its comments left out; `api_check` owns the parser.
    let lock = Lock::read(&root.join(krate.path()).join(LOCK))?.lines;
    let ported = Ported::read(root, manifest);
    let (source, counted) = read(root, tree)?;
    let join = join(&map, &lock, &ported, source, &counted);
    let reading = reading(&join, &ported, krate, root)?;
    print!("{}", render(&join));
    println!(
        "{} {INSTRUMENT} {}: {}",
        reading.status.as_str(),
        reading.scope,
        reading.message
    );
    store.record(std::slice::from_ref(&reading))?.exit()
}

#[cfg(test)]
mod tests;
