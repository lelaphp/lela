//! The divergence registry, `docs/divergence.toml` (harness-design.md §4.3):
//! one row per accepted difference from the oracle, keyed by a witness hash,
//! a phpt path, a compile-identity path, an extension's name, a trace site
//! or a site where php's own behaviour is undefined. A row whose one reason
//! holds for many ids of its kind names them in `covers`, and
//! [`Registry::row`] answers each of them with the row as it answers the
//! row's own id; the row counts once (revision 62). A `trace:` row's
//! `covers` may also name `witness:` ids, the fences that pin the engine's
//! side of the row: such a fence is held to its pinned `expect` against
//! the engine alone, so the row accepts no difference for it
//! ([`Registry::pinning`]; harness §4.3, #1650). A person
//! writes a row and its reason; `compare`'s caller or the fuzzer may propose
//! one with status `open` and an empty reason; the `divergence-reason`
//! instrument fails an accepted row without a reason. The TOML is the truth;
//! `status --section divergence` is the rendering.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::reading::{self, Number, Reading};

/// The registry's path, relative to the workspace root.
pub const FILE: &str = "docs/divergence.toml";
/// The instrument that holds the registry to its rule: the subcommand, the
/// reading's `instrument`, and what the manifest's `check` predicate names.
pub const INSTRUMENT: &str = "divergence-reason";

/// One divergence key: `witness:<hash>`, `phpt:<path>`, `compile:<path>`,
/// `crosscheck:<name>`, `trace:<site>` or `php-ub:<site>`. Only [`Id::of`]
/// makes one, so an id in hand carries the prefix of the kind it was
/// decoded under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    /// `text` as an id of `kind`, refused unless it starts with the kind's
    /// prefix: the one place that rule is held, for a row's own id, for
    /// each id it covers and for a proposed one.
    pub fn of(kind: Kind, text: &str) -> Result<Id> {
        if !text.starts_with(kind.prefix()) {
            bail!(
                "the id {text:?} is of kind {}, whose ids start with {:?}",
                kind.as_str(),
                kind.prefix()
            );
        }
        Ok(Id(text.to_owned()))
    }

    /// `text` as an id a row of `kind` covers: one of the kind's own, as
    /// [`Id::of`] holds it, or under a `trace:` row a `witness:` id, a
    /// fence that pins the engine's side of the row (harness §4.3, #1650).
    fn covered(kind: Kind, text: &str) -> Result<Id> {
        if kind == Kind::Trace && text.starts_with(Kind::Witness.prefix()) {
            return Ok(Id(text.to_owned()));
        }
        Id::of(kind, text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Id {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One row of the registry, decoded: its own id and every id it covers
/// held to its kind's prefix, and no id named twice within it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Row", into = "Row")]
pub struct Divergence {
    /// What names the row; the prefix is the kind's, and `runner` shares
    /// `phpt:`.
    pub id: Id,
    pub kind: Kind,
    /// Written by a person; empty on a proposed row.
    pub reason: String,
    pub status: Status,
    /// The date the row was written, `YYYY-MM-DD`, as the file quotes it.
    pub since: String,
    /// The other ids of the row's kind its one reason holds for, read as
    /// the row's own id is (harness-design.md §4.3, revision 62); empty
    /// on a row that holds for its own id alone.
    pub covers: BTreeSet<Id>,
}

impl Divergence {
    /// Every id the row holds: its own, then the ones it covers.
    pub fn ids(&self) -> impl Iterator<Item = &Id> {
        std::iter::once(&self.id).chain(&self.covers)
    }

    /// Whether `id`, an id the row holds, is a fence pinning the engine's
    /// side of the row rather than a difference the row accepts: a
    /// `witness:` id a `trace:` row covers (#1650).
    fn pins(&self, id: &str) -> bool {
        self.kind == Kind::Trace && id.starts_with(Kind::Witness.prefix())
    }
}

/// One row as the file spells it, before its ids are decoded.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    id: String,
    kind: Kind,
    reason: String,
    status: Status,
    since: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    covers: Vec<String>,
}

impl TryFrom<Row> for Divergence {
    type Error = anyhow::Error;

    fn try_from(row: Row) -> Result<Divergence> {
        let id = Id::of(row.kind, &row.id)?;
        let mut covers = BTreeSet::new();
        for text in &row.covers {
            let covered =
                Id::covered(row.kind, text).map_err(|e| anyhow!("{e}, in the covers of {id}"))?;
            if covered == id || !covers.insert(covered) {
                bail!("the id {text:?} is named twice in the row {id}");
            }
        }
        Ok(Divergence {
            id,
            kind: row.kind,
            reason: row.reason,
            status: row.status,
            since: row.since,
            covers,
        })
    }
}

impl From<Divergence> for Row {
    fn from(row: Divergence) -> Row {
        Row {
            id: row.id.0,
            kind: row.kind,
            reason: row.reason,
            status: row.status,
            since: row.since,
            covers: row.covers.into_iter().map(|id| id.0).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Witness,
    Phpt,
    /// A `phpt:` id where the nightly runner control, not the engine, disagrees.
    Runner,
    Compile,
    /// A `crosscheck:<name>` id: one extension whose `php --re <name>`
    /// rendering differs from the oracle's (harness §7).
    Crosscheck,
    Trace,
    /// A `php-ub:<site>` id: a place where php's own behaviour
    /// is not defined, and the
    /// engine does something defined instead, its `covers` naming the
    /// fixture that holds the engine's side. Not parity debt, so counted
    /// apart from the day-30 target (#2220).
    #[serde(rename = "php-ub")]
    PhpUb,
}

impl Kind {
    /// The seven kinds, in the registry's order: the parity kinds, then
    /// the one counted apart.
    pub const ALL: [Kind; 7] = [
        Kind::Witness,
        Kind::Phpt,
        Kind::Runner,
        Kind::Compile,
        Kind::Crosscheck,
        Kind::Trace,
        Kind::PhpUb,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Witness => "witness",
            Kind::Phpt => "phpt",
            Kind::Runner => "runner",
            Kind::Compile => "compile",
            Kind::Crosscheck => "crosscheck",
            Kind::Trace => "trace",
            Kind::PhpUb => "php-ub",
        }
    }

    /// Whether a row of this kind is parity debt, counted against the
    /// day-30 target of under 20 rows (harness-design.md §4.3): every kind
    /// but `php-ub`, whose rows record php's undefined behaviour rather
    /// than a difference the engine owes (#2220).
    pub fn is_parity(self) -> bool {
        self != Kind::PhpUb
    }

    /// The prefix an id of this kind carries, colon included.
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Witness => "witness:",
            Kind::Phpt | Kind::Runner => "phpt:",
            Kind::Compile => "compile:",
            Kind::Crosscheck => "crosscheck:",
            Kind::Trace => "trace:",
            Kind::PhpUb => "php-ub:",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Proposed; `done` fails while one touches the scope.
    Open,
    /// A person wrote the reason.
    Accepted,
    /// The difference is gone; the row stays as the record.
    Fixed,
}

impl Status {
    /// The three statuses, in the registry's order.
    pub const ALL: [Status; 3] = [Status::Open, Status::Accepted, Status::Fixed];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Accepted => "accepted",
            Status::Fixed => "fixed",
        }
    }
}

/// The registry: its rows, every id they hold to the row that holds it,
/// and the file they came from when loaded.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    rows: Vec<Divergence>,
    /// Each row's own id and each id it covers, to the row's place in
    /// `rows`: what [`Registry::row`] reads, built once as the rows load.
    held: BTreeMap<Id, usize>,
    path: Option<PathBuf>,
}

/// The grammar of the file: `[[divergence]]` rows and nothing else.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileShape {
    #[serde(default)]
    divergence: Vec<Divergence>,
}

impl Registry {
    /// Load `docs/divergence.toml` from the workspace root.
    pub fn load(root: &Path) -> Result<Registry> {
        let path = root.join(FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut registry = Registry::parse(&text).with_context(|| format!("{}", path.display()))?;
        registry.path = Some(path);
        Ok(registry)
    }

    /// Parse the registry's text and hold it to the row shape: an id whose
    /// prefix is not its row's kind's, in `id` or in `covers`, is refused,
    /// and so is an id two rows hold, whether as their own or covered.
    pub fn parse(text: &str) -> Result<Registry> {
        let shape: FileShape = toml::from_str(text)?;
        let mut registry = Registry::default();
        for row in shape.divergence {
            registry.hold(row)?;
        }
        Ok(registry)
    }

    /// Append one decoded row, refusing it when another row already holds
    /// one of its ids.
    fn hold(&mut self, row: Divergence) -> Result<&Divergence> {
        let place = self.rows.len();
        if let Some((id, other)) = row.ids().find_map(|id| self.held.get(id).map(|&n| (id, n))) {
            bail!(
                "the id {:?} has two rows, the registry's rows {} and {}",
                id.as_str(),
                other + 1,
                place + 1
            );
        }
        for id in row.ids() {
            self.held.insert(id.clone(), place);
        }
        self.rows.push(row);
        self.rows
            .get(place)
            .ok_or_else(|| anyhow!("the row just held is not at {place}"))
    }

    /// The rows, in the file's order.
    pub fn rows(&self) -> &[Divergence] {
        &self.rows
    }

    /// The row that holds `id`, as its own id or as one it covers: the one
    /// lookup every comparison, verdict and rendering goes through.
    pub fn row(&self, id: &str) -> Option<&Divergence> {
        self.held.get(id).and_then(|&n| self.rows.get(n))
    }

    /// The row that accepts a difference for `id`, if one does. A fence
    /// an accepted `trace:` row covers has none: the row is what it pins,
    /// and [`Registry::pinning`] answers for it.
    pub fn accepted(&self, id: &str) -> Option<&Divergence> {
        self.row(id)
            .filter(|r| r.status == Status::Accepted && !r.pins(id))
    }

    /// The accepted `trace:` row that covers `id`, a `witness:` id: the
    /// fence pins the engine's side of the row, so it is held to its
    /// pinned `expect` against the engine alone, and the oracle's run is
    /// recorded and not judged (harness §4.3, §8; #1650). An `open` row
    /// pins nothing, as it accepts nothing.
    pub(crate) fn pinning(&self, id: &str) -> Option<&Divergence> {
        self.row(id)
            .filter(|r| r.status == Status::Accepted && r.pins(id))
    }

    /// Propose a row: status `open`, empty reason, dated today, appended to
    /// the file after its prose. The id must be new and carry the kind's
    /// prefix, and the registry must have been loaded from its file.
    pub fn propose(&mut self, id: &str, kind: Kind) -> Result<&Divergence> {
        let Some(path) = self.path.clone() else {
            bail!("the registry was parsed from text, not loaded from {FILE}; nothing to append to")
        };
        if let Some(row) = self.row(id) {
            bail!(
                "the id {id:?} already has a row, {}, with status {}",
                row.id,
                row.status.as_str()
            );
        }
        let since = reading::now()?
            .date
            .context("the clock gave no date")?
            .to_string();
        let row = Divergence {
            id: Id::of(kind, id)?,
            kind,
            reason: String::new(),
            status: Status::Open,
            since,
            covers: BTreeSet::new(),
        };
        let text = toml::to_string(&Rows {
            divergence: std::slice::from_ref(&row),
        })
        .context("rendering the row")?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        write!(file, "\n{text}").with_context(|| format!("appending to {}", path.display()))?;
        self.hold(row)
    }

    /// Accepted rows whose reason nobody wrote.
    pub fn unreasoned(&self) -> Vec<&Divergence> {
        self.rows
            .iter()
            .filter(|r| r.status == Status::Accepted && r.reason.trim().is_empty())
            .collect()
    }

    /// Rows by kind and by status, a covering row once, and the ids the
    /// rows hold.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts {
            ids: self.held.len() as u64,
            ..Counts::default()
        };
        for row in &self.rows {
            *counts.by_kind.entry(row.kind).or_default() += 1;
            *counts.by_status.entry(row.status).or_default() += 1;
            *counts.cells.entry((row.kind, row.status)).or_default() += 1;
        }
        counts
    }
}

#[derive(Serialize)]
struct Rows<'a> {
    divergence: &'a [Divergence],
}

/// How many rows of each kind, of each status, and of each pair, a row
/// that covers many ids counting once, since the day-30 target counts
/// reasons (harness-design.md §4.3); and how many ids the rows hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub ids: u64,
    pub by_kind: BTreeMap<Kind, u64>,
    pub by_status: BTreeMap<Status, u64>,
    pub cells: BTreeMap<(Kind, Status), u64>,
}

impl Counts {
    pub fn cell(&self, kind: Kind, status: Status) -> u64 {
        self.cells.get(&(kind, status)).copied().unwrap_or(0)
    }

    /// The parity rows of one status, every kind [`Kind::is_parity`]
    /// summed: the total the day-30 target reads.
    pub fn parity(&self, status: Status) -> u64 {
        self.cells
            .iter()
            .filter(|((kind, s), _)| kind.is_parity() && *s == status)
            .map(|(_, n)| n)
            .sum()
    }
}

/// The `divergence-reason` instrument's reading over the workspace: fail
/// when an accepted row has no reason, naming each such row, and how many
/// ids it covers where it covers any, and printing the text of the
/// manifest rule whose `check` predicate names this instrument. A covering
/// row is held to the rule once, as one reason. The counts are the numbers.
pub fn reason(registry: &Registry, manifest: &Manifest, root: &Path) -> Result<Reading> {
    let rule = manifest.rule_checked_by(INSTRUMENT);
    let mut reading =
        Reading::for_scope(INSTRUMENT, "workspace", manifest, &Snapshot::disk(root)?)?;
    if let Some(rule) = rule {
        reading.rule.clone_from(&rule.id);
        reading.adr = rule.adr.iter().collect::<Vec<_>>().join(", ");
    }
    let rows = registry.rows().len();
    let counts = registry.counts();
    let unreasoned = registry.unreasoned();
    reading.numbers.insert("rows".into(), Number::count(rows));
    reading
        .numbers
        .insert("ids".into(), Number::count(counts.ids));
    for status in Status::ALL {
        let n = counts.by_status.get(&status).copied().unwrap_or(0);
        reading
            .numbers
            .insert(status.as_str().into(), Number::count(n));
    }
    reading
        .numbers
        .insert("unreasoned".into(), Number::count(unreasoned.len()));
    if unreasoned.is_empty() {
        reading.status = reading::Status::Pass;
        reading.message = format!(
            "{FILE}: {rows} rows holding {} ids, every accepted row has a reason",
            counts.ids
        );
    } else {
        reading.status = reading::Status::Fail;
        let named: Vec<String> = unreasoned
            .iter()
            .map(|r| match r.covers.len() {
                0 => r.id.to_string(),
                n => format!("{} (covering {n} ids)", r.id),
            })
            .collect();
        reading.message = format!(
            "{FILE}: {} accepted row{} without a reason: {}. {}",
            named.len(),
            if named.len() == 1 { "" } else { "s" },
            named.join(", "),
            rule.map(|r| r.text.as_str()).unwrap_or_default()
        );
    }
    Ok(reading)
}
