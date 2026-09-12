//! `xtask port-map`: the Mahout census joined to `api.lock` (design §4.4;
//! harness-design.md §4.2, §7). Every item the 19 `mahout_ext_*` crates
//! call is one row: `same`, kept under its name; `renamed`, with the `api`
//! item that replaces it; or `gone`, with the rule that replaces the need.
//! The join holds each row's `api` item against the lock and counts the
//! rows the port has not reached yet, which is the reading's number. A
//! row is unmatched, the failure §4.4 calls a CI failure from day 3, in
//! either of the two shapes the design's sentence has: a row naming an
//! `api` item the lock does not carry, and — under `--tree` — an item the
//! census finds that no row names, since "every Mahout item is `same`,
//! `renamed` or `gone`" and an item with no row is none of the three.
//!
//! **Resolved.** A row's sites sat in `crates` of the 19 inherited
//! extensions, and the row is resolved when the port has reached that
//! many: `crates` extensions under `ext/` here declare a native. A crate
//! `new-ext` laid down is not a port — its `extension()` declares
//! nothing and calls no `api` item — so it is not the evidence a row is
//! resolved on. An engine with no extension ported resolves nothing,
//! which is this engine today — 651 rows, all unresolved (design §11,
//! row 1–2) — and the count rises with the port, the rows one extension
//! called first and the ones every extension called last.
//!
//! It is a proxy, and a stated one: it counts how many extensions are
//! ported and not whether the ported ones are the crates that called the
//! item. §4.4 says the join is the porting checklist and never says what
//! takes a row off it, while §11 holds the number to 150, 50 and 0, so
//! what resolves a row is a decision the design of record owes and #104
//! asks for; a sharper measure replaces this function and nothing else.
//! What the harness can read without it — that a replacement exists — is
//! the match, which is the other column and the one that fails.
//!
//! **The census.** `data/port-map.v0.toml` is the census as
//! `scripts/census.sh` printed it at Mahout `1b3104a8`, joined by hand to
//! `api.lock` v0 (`data/api-lock-v0.md`). `--tree <path>` regenerates it
//! against the inherited tree, which lives outside this repository, and
//! joins that instead; a tree the census cannot read is a skip of the
//! regeneration and never a pass, and the reading says which of the two
//! it read.

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
    /// How many ext crates call it.
    pub(crate) crates: usize,
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
/// that declare a native. How many there are is the evidence a row is
/// resolved on.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ported {
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
                .filter(|c| declares_a_native(root, c))
                .map(|c| c.name.clone())
                .collect(),
        }
    }

    /// Whether the port has reached `crates` extensions, the number one
    /// row's sites sat in.
    pub(crate) fn reached(&self, crates: usize) -> bool {
        crates > 0 && self.crates.len() >= crates
    }
}

/// Whether a crate declares a native: an attribute of `php_ext::attr` on
/// an item of its sources, `#[php_function]`, `#[php_class]`,
/// `#[php_const]` or `#[tentative]` (design §7.1). This tells a ported
/// extension from one `new-ext` laid down, whose `extension()` declares
/// nothing and calls no `api` item; a crate whose sources cannot be
/// listed or read declares none, since a port is evidence and an unread
/// crate is not.
fn declares_a_native(root: &Path, krate: &Crate) -> bool {
    sources::rust(root, krate)
        .unwrap_or_default()
        .iter()
        .any(|path| fs::read_to_string(root.join(path)).is_ok_and(|text| declares(&text)))
}

/// Whether one source declares a native, read line by line: the
/// attribute heads an item, so it is the line's first word.
pub(crate) fn declares(text: &str) -> bool {
    const ATTRIBUTES: [&str; 4] = [
        "#[php_function",
        "#[php_class",
        "#[php_const",
        "#[tentative",
    ];
    text.lines().any(|line| {
        ATTRIBUTES
            .iter()
            .any(|attr| line.trim_start().starts_with(attr))
    })
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
    pub(crate) crates: usize,
    /// Whether the lock carries the row's `api` item; a `gone` row is
    /// matched by its rule.
    pub(crate) matched: bool,
    /// Whether the ported bodies show the port took the row.
    pub(crate) resolved: bool,
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

    /// Rows the port has not taken yet: the reading's number.
    pub(crate) fn unresolved(&self) -> usize {
        self.rows.iter().filter(|j| !j.resolved).count()
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

/// One row against the lock and the extensions ported here.
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
    let resolved = ported.reached(crates);
    Joined {
        row,
        sites,
        crates,
        matched,
        resolved,
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

/// What the ported bodies say about the unresolved count.
fn ported_says(join: &Join, ported: &Ported) -> String {
    if ported.crates.is_empty() {
        "no extension is ported, so the port has reached no row".to_owned()
    } else {
        format!(
            "{} reached by the {} extension{} ported",
            join.rows.len() - join.unresolved(),
            ported.crates.len(),
            plural(ported.crates.len())
        )
    }
}

/// One line per row: what became of the item, whether the port has taken
/// it, the Mahout item and what replaces it.
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
        let _ = writeln!(
            out,
            "{:<8} {:<11} {} → {}  {} site{}, {} crate{}{unmatched}",
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
