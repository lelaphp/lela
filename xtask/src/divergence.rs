//! The divergence registry, `docs/divergence.toml` (harness-design.md §4.3):
//! one row per accepted difference from the oracle, keyed by a witness hash,
//! a phpt path, a compile-identity path, an extension's name, a trace site,
//! for a difference on the wire, which the RFCs judge through a
//! conformance suite rather than the oracle, a suite's case (#2334), or a
//! test the production server answers otherwise by design (#2629). A
//! row's `kind` says which lane reads it and its `cause` whether it is
//! parity debt, counted against the manifest's day-30 target, or a
//! difference that does not count (#2460, #2462). A row whose one reason
//! holds for many ids of its kind names them in `covers`, and
//! [`Registry::row`] answers each of them with the row as it answers the
//! row's own id; the row counts once (revision 62). A `trace:` row's
//! `covers` may also name `witness:` ids, the fences that pin the engine's
//! side of the row: such a fence is held to its pinned `expect` against
//! the engine alone, so the row accepts no difference for it
//! ([`Registry::pinning`]; harness §4.3, #1650). A row may name the
//! [`Digests`] of both sides' lines it was ruled on, and accepts its
//! difference only while both stand; a `trace:dump/` row must (#3291's
//! ruling). A person
//! writes a row and its reason; `compare`'s caller or the fuzzer may propose
//! one with status `open` and an empty reason; the `divergence-reason`
//! instrument fails an accepted row without a reason, a `by-design` row
//! whose reason cites no design section or ADR, and an `oracle-build` row
//! that names nothing it retires at, and an accepted `trace:dump/` row
//! that names no digests. The TOML is the truth;
//! `status --section divergence` is the rendering.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::reading::{self, Reading};
use crate::sha256::Sha256;

/// The registry's path, relative to the workspace root.
pub const FILE: &str = "docs/divergence.toml";
/// The instrument that holds the registry to its rule: the subcommand, the
/// reading's `instrument`, and what the manifest's `check` predicate names.
pub const INSTRUMENT: &str = "divergence-reason";
/// What the id of a trace-dump row starts with, `trace:dump/<kernel>`:
/// the rows that must name their [`Digests`] (#3291's ruling).
pub const DUMP: &str = "trace:dump/";

/// One divergence key: `witness:<hash>`, `phpt:<path>`, `compile:<path>`,
/// `crosscheck:<name>`, `trace:<site>`, `wire:<suite>/<case>` or
/// `makala:<test path>`. Only
/// [`Id::of`] makes one, so an id in hand carries the prefix of the kind it
/// was decoded under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    /// `text` as an id of `kind`, refused unless it starts with the kind's
    /// prefix: the one place that rule is held, for a row's own id, for
    /// each id it covers and for a proposed one. A `wire:` id names a suite
    /// and a case after it.
    pub fn of(kind: Kind, text: &str) -> Result<Id> {
        let (prefix, kind_name) = (kind.prefix(), kind.as_str());
        let Some(rest) = text.strip_prefix(prefix) else {
            bail!("the id {text:?} is of kind {kind_name}, whose ids start with {prefix:?}");
        };
        let named = |(suite, case): (&str, &str)| !suite.is_empty() && !case.is_empty();
        if kind == Kind::Wire && !rest.split_once('/').is_some_and(named) {
            bail!("the id {text:?} is of kind {kind_name}, whose ids are {prefix:?}<suite>/<case>");
        }
        Ok(Id(text.to_owned()))
    }

    /// `text` as an id a row of `kind` covers: one of the row's own, as
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
    /// Which lane reads the row.
    pub kind: Kind,
    /// Whether the row is parity debt, and if not, why not; `debt` where
    /// the file writes no `cause`.
    pub cause: Cause,
    /// What the row stops being needed at, where it names one: an
    /// `oracle-build` row must, a `php-ub` row reported upstream may.
    pub retires: Option<Retires>,
    /// Written by a person; empty on a proposed row.
    pub reason: String,
    pub status: Status,
    /// The date the row was written, `YYYY-MM-DD`, as the file quotes it.
    pub since: String,
    /// The other ids of the row's kind its one reason holds for, read as
    /// the row's own id is (harness-design.md §4.3, revision 62); empty
    /// on a row that holds for its own id alone.
    pub covers: BTreeSet<Id>,
    /// Both sides' lines as the row was ruled on, where it names them:
    /// the difference it accepts is that one alone (#3291).
    pub digests: Option<Digests>,
}

/// The two sides a comparison holds lines of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The pinned php's.
    Oracle,
    /// The engine's.
    Engine,
}

impl Side {
    /// The side's key in a row, the extension of the file `trace-dump`
    /// keeps its lines in, and its name in a message.
    pub fn name(self) -> &'static str {
        match self {
            Side::Oracle => "oracle",
            Side::Engine => "engine",
        }
    }
}

/// The digests of both sides' lines, the oracle's and the engine's: what
/// an accepted row pins of the difference it was ruled on, so a pin bump
/// that moves php's lines or a change that moves the engine's is a
/// difference nobody ruled on (#3291). A re-pin is a changed value, a
/// person's hand edit, as a new acceptance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digests {
    pub oracle: Sha256,
    pub engine: Sha256,
}

impl Digests {
    /// The digests of the lines a comparison held, the oracle's
    /// `expected` and the engine's `actual`: the one place a row's
    /// digests are computed, for the check and for the row proposed.
    pub fn of(expected: &[u8], actual: &[u8]) -> Digests {
        Digests {
            oracle: Sha256::of(expected),
            engine: Sha256::of(actual),
        }
    }

    fn of_side(&self, side: Side) -> Sha256 {
        match side {
            Side::Oracle => self.oracle,
            Side::Engine => self.engine,
        }
    }
}

impl fmt::Display for Digests {
    /// The two as a row spells them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "oracle = \"{}\", engine = \"{}\"",
            self.oracle, self.engine
        )
    }
}

/// An accepted row whose digests the lines held no longer match: the
/// difference it accepted is not the one compared now (#3291).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// The row's id.
    pub id: Id,
    /// The digests the row pins.
    pub pinned: Digests,
    /// The digests of the lines compared now.
    pub now: Digests,
}

impl Moved {
    /// The sides whose lines are not the ones the row pins, the oracle's
    /// first; empty where both stand.
    pub fn sides(&self) -> Vec<Side> {
        let moved = |side: &Side| self.pinned.of_side(*side) != self.now.of_side(*side);
        [Side::Oracle, Side::Engine]
            .into_iter()
            .filter(moved)
            .collect()
    }
}

impl fmt::Display for Moved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} accepts the difference it was ruled on, and", self.id)?;
        for (n, side) in self.sides().into_iter().enumerate() {
            let and = if n == 0 { "" } else { " and" };
            write!(
                f,
                "{and} the {}'s lines moved: the row pins {}, they are {} now",
                side.name(),
                self.pinned.of_side(side),
                self.now.of_side(side)
            )?;
        }
        write!(
            f,
            "; a re-pin is a new acceptance, a person's edit of the row (#3291)"
        )
    }
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

/// One row as the file spells it, before its kind and ids are decoded: a
/// kind [`Kind`] does not name is refused with the row's id, `php-ub`
/// among them since #2460 made it a cause (#2510).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cause: Option<Cause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retires: Option<Retires>,
    reason: String,
    status: Status,
    since: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    covers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oracle: Option<Sha256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engine: Option<Sha256>,
}

impl TryFrom<Row> for Divergence {
    type Error = anyhow::Error;

    fn try_from(row: Row) -> Result<Divergence> {
        let kind: Kind = row
            .kind
            .parse()
            .map_err(|e| anyhow!("{e}, in the row {:?}", row.id))?;
        let cause = row.cause.unwrap_or_default();
        let id = Id::of(kind, &row.id)?;
        let mut covers = BTreeSet::new();
        for text in &row.covers {
            let covered =
                Id::covered(kind, text).map_err(|e| anyhow!("{e}, in the covers of {id}"))?;
            if covered == id || !covers.insert(covered) {
                bail!("the id {text:?} is named twice in the row {id}");
            }
        }
        let digests = match (row.oracle, row.engine) {
            (Some(oracle), Some(engine)) => Some(Digests { oracle, engine }),
            (None, None) => None,
            _ => bail!(
                "the row {id} names one side's digest and not the other's; a row pins both, `oracle` and `engine`, or neither (#3291)"
            ),
        };
        Ok(Divergence {
            id,
            kind,
            cause,
            retires: row.retires,
            reason: row.reason,
            status: row.status,
            since: row.since,
            covers,
            digests,
        })
    }
}

impl From<Divergence> for Row {
    fn from(row: Divergence) -> Row {
        Row {
            id: row.id.0,
            kind: row.kind.as_str().to_owned(),
            cause: (row.cause != Cause::Debt).then_some(row.cause),
            retires: row.retires,
            reason: row.reason,
            status: row.status,
            since: row.since,
            covers: row.covers.into_iter().map(|id| id.0).collect(),
            oracle: row.digests.map(|d| d.oracle),
            engine: row.digests.map(|d| d.engine),
        }
    }
}

/// Which lane reads a row: what its id names and which comparison
/// consults it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// A `wire:<suite>/<case>` id: one case of a conformance suite the
    /// production server's wire is held to, which the RFCs judge and php
    /// does not, since php ships no such server (#2334). The case may
    /// carry slashes of its own, as h2spec's `http2/6.5.3/1` does.
    Wire,
    /// A `makala:<test path>` id, the path relative to php-src: one test
    /// where the production server differs from php's SAPIs by design,
    /// which `fpm-compare` and the served phpt lane consult
    /// ([`Registry::makala`], #2629, #2639).
    Makala,
}

impl Kind {
    /// The eight kinds, in the registry's order.
    pub const ALL: [Kind; 8] = [
        Kind::Witness,
        Kind::Phpt,
        Kind::Runner,
        Kind::Compile,
        Kind::Crosscheck,
        Kind::Trace,
        Kind::Wire,
        Kind::Makala,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Witness => "witness",
            Kind::Phpt => "phpt",
            Kind::Runner => "runner",
            Kind::Compile => "compile",
            Kind::Crosscheck => "crosscheck",
            Kind::Trace => "trace",
            Kind::Wire => "wire",
            Kind::Makala => "makala",
        }
    }

    /// The prefix an id of this kind carries, colon included.
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Witness => "witness:",
            Kind::Phpt | Kind::Runner => "phpt:",
            Kind::Compile => "compile:",
            Kind::Crosscheck => "crosscheck:",
            Kind::Trace => "trace:",
            Kind::Wire => "wire:",
            Kind::Makala => "makala:",
        }
    }
}

impl std::str::FromStr for Kind {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Kind> {
        Kind::ALL
            .into_iter()
            .find(|k| k.as_str() == text)
            .ok_or_else(|| {
                let names: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
                anyhow!(
                    "unknown kind {text:?}, expected one of {}",
                    names.join(", ")
                )
            })
    }
}

/// Why a row differs from the oracle, apart from which lane reads it
/// (#2460): parity debt, counted against the manifest's day-30 target, or
/// one of the causes counted on lines of their own.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// A difference the engine owes; a row that writes no `cause` is one.
    #[default]
    Debt,
    /// The engine does something defined where php's own behaviour is
    /// not, its `covers` naming the fixture that holds the engine's side.
    PhpUb,
    /// The engine differs on purpose, as the design section or ADR its
    /// reason cites decided.
    ByDesign,
    /// The oracle's build differs from what the engine is held to, until
    /// what the row's `retires` names.
    OracleBuild,
}

impl Cause {
    /// The four causes, debt first and then the ones counted apart, in
    /// the order the counts render them.
    pub const ALL: [Cause; 4] = [
        Cause::Debt,
        Cause::PhpUb,
        Cause::ByDesign,
        Cause::OracleBuild,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Debt => "debt",
            Cause::PhpUb => "php-ub",
            Cause::ByDesign => "by-design",
            Cause::OracleBuild => "oracle-build",
        }
    }
}

/// What a row stops being needed at, so an oracle bump can list the rows
/// it should retire (Mitchell, 2026-10-09, on #2462): the guard reads this
/// and never the reason's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Retires {
    /// The oracle rebuilt under the pin it has, linking what the engine does.
    OracleRebuild,
    /// The pin moved to a php that carries the fix upstream.
    Pin,
    /// The fixture the row names changed.
    Fixture,
    /// The platform the row holds on is no longer one the lanes run.
    Platform,
}

impl Retires {
    pub fn as_str(self) -> &'static str {
        match self {
            Retires::OracleRebuild => "oracle-rebuild",
            Retires::Pin => "pin",
            Retires::Fixture => "fixture",
            Retires::Platform => "platform",
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

/// The ids a registry's text names, each row's own and those it covers,
/// read without holding the rows to today's shape: `done`'s base is any
/// older commit, and a kind or field since removed (`php-ub`, #2510) must
/// still answer which rows the diff added.
pub(crate) fn named_ids(text: &str) -> Result<BTreeSet<String>> {
    #[derive(Deserialize)]
    struct Named {
        id: String,
        #[serde(default)]
        covers: Vec<String>,
    }
    #[derive(Deserialize)]
    struct Names {
        #[serde(default)]
        divergence: Vec<Named>,
    }
    let names: Names = toml::from_str(text)?;
    Ok(names
        .divergence
        .into_iter()
        .flat_map(|row| std::iter::once(row.id).chain(row.covers))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::named_ids;

    /// A workspace with no registry, a repository that runs no php, has no
    /// row, so `done` and `status` read it rather than stop (#3181).
    #[test]
    fn a_workspace_without_the_registry_has_no_row() {
        let root = crate::scratch::Tree::new("divergence-none").unwrap();
        let registry = super::Registry::load(&root).unwrap();
        assert!(registry.rows().is_empty());
    }

    #[test]
    fn an_older_registrys_ids_are_read_whatever_its_kinds() {
        let old = r#"
[[divergence]]
id = "php-ub:heap-openbsd-stack-base"
kind = "php-ub"
reason = "r"
status = "accepted"
since = "2026-09-01"
covers = ["php-ub:other-site"]
"#;
        let ids = named_ids(old).unwrap_or_default();
        assert!(ids.contains("php-ub:heap-openbsd-stack-base"), "{ids:?}");
        assert!(ids.contains("php-ub:other-site"), "{ids:?}");
        assert!(super::Registry::parse(old).is_err());
    }
}

impl Registry {
    /// Load `docs/divergence.toml` from the workspace root; a workspace
    /// without the file has no row, so nothing it measures is an accepted
    /// divergence.
    pub fn load(root: &Path) -> Result<Registry> {
        let path = root.join(FILE);
        let text = crate::read_if(&path)?.unwrap_or_default();
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

    /// The accepted `makala:` row that holds `test`, a path in php-src, as
    /// its own id or one it covers: the one lookup both of the production
    /// server's comparisons make, `fpm-compare` and the served phpt lane
    /// (#2629, #2639), through [`crate::makala::Consulted`].
    pub fn makala(&self, test: &str) -> Option<&Divergence> {
        self.accepted(&format!("{}{test}", Kind::Makala.prefix()))
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
            cause: Cause::Debt,
            retires: None,
            reason: String::new(),
            status: Status::Open,
            since,
            covers: BTreeSet::new(),
            digests: None,
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

    /// The accepted rows the guard refuses, each with what it lacks.
    pub(crate) fn refused(&self) -> Vec<(&Divergence, Lack)> {
        self.rows
            .iter()
            .filter(|r| r.status == Status::Accepted)
            .filter_map(|r| r.lack().map(|lack| (r, lack)))
            .collect()
    }

    /// Rows by status, and by cause, kind and status, a covering row
    /// once, and the ids the rows hold.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts {
            ids: self.held.len() as u64,
            ..Counts::default()
        };
        for row in &self.rows {
            *counts.by_status.entry(row.status).or_default() += 1;
            *counts
                .cells
                .entry((row.cause, row.kind, row.status))
                .or_default() += 1;
        }
        counts
    }
}

/// What an accepted row lacks that the guard asks of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lack {
    /// No reason at all.
    Reason,
    /// A `by-design` row whose reason cites no design section or ADR.
    Citation,
    /// An `oracle-build` row that names nothing it retires at.
    Retires,
    /// A `trace:dump/` row that names no digests (#3291).
    Digests,
}

impl Lack {
    /// Each lack, in the order the guard's message names them.
    const ALL: [Lack; 4] = [Lack::Reason, Lack::Citation, Lack::Retires, Lack::Digests];

    /// The reading's number counting the rows that lack it.
    fn number(self) -> &'static str {
        match self {
            Lack::Reason => "unreasoned",
            Lack::Citation => "uncited",
            Lack::Retires => "unretired",
            Lack::Digests => "unpinned",
        }
    }

    /// What the message says of a row that lacks it.
    fn says(self) -> &'static str {
        match self {
            Lack::Reason => "without a reason",
            Lack::Citation => "of cause by-design whose reason cites no design section (§) or ADR",
            Lack::Retires => "of cause oracle-build that carries no `retires`",
            Lack::Digests => {
                "of a trace dump that pins neither side's lines, its `oracle` and `engine` digests"
            }
        }
    }
}

impl Divergence {
    /// Whether the row accepts its difference only as its [`Digests`]
    /// pin it: a `trace:dump/` row, as #3291 ruled. Which other kinds must
    /// is a later ruling; a row of any kind that names digests is held to
    /// them.
    pub fn must_pin(&self) -> bool {
        self.id.as_str().starts_with(DUMP)
    }

    /// What the guard finds missing from the row were it accepted: a
    /// reason, then what its cause asks for, then a dump's digests.
    fn lack(&self) -> Option<Lack> {
        if self.reason.trim().is_empty() {
            Some(Lack::Reason)
        } else if self.cause == Cause::ByDesign && !cites_design(&self.reason) {
            Some(Lack::Citation)
        } else if self.cause == Cause::OracleBuild && self.retires.is_none() {
            Some(Lack::Retires)
        } else if self.must_pin() && self.digests.is_none() {
            Some(Lack::Digests)
        } else {
            None
        }
    }
}

/// Whether a reason cites a section of the design of record (`§`, as
/// `third-engine-design.md §5.2` or `harness-design.md §4.3` write it) or
/// an ADR (`ADR-0005`).
fn cites_design(reason: &str) -> bool {
    reason.contains('§')
        || reason
            .match_indices("ADR-")
            .any(|(at, _)| reason[at + 4..].starts_with(|c: char| c.is_ascii_digit()))
}

#[derive(Serialize)]
struct Rows<'a> {
    divergence: &'a [Divergence],
}

/// How many rows of each status, and of each cause, kind and status, a
/// row that covers many ids counting once, since the day-30 target counts
/// reasons (harness-design.md §4.3); and how many ids the rows hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub ids: u64,
    pub by_status: BTreeMap<Status, u64>,
    pub cells: BTreeMap<(Cause, Kind, Status), u64>,
}

impl Counts {
    /// The rows of one cause and status, a kind or every kind.
    pub(crate) fn of(&self, cause: Cause, kind: Option<Kind>, status: Status) -> u64 {
        self.cells
            .iter()
            .filter(|((c, k, s), _)| {
                *c == cause && kind.is_none_or(|kind| *k == kind) && *s == status
            })
            .map(|(_, n)| n)
            .sum()
    }

    /// The debt rows of one kind and status: a line above the total.
    pub fn debt(&self, kind: Kind, status: Status) -> u64 {
        self.of(Cause::Debt, Some(kind), status)
    }

    /// The debt rows of one status, every kind summed: the total the
    /// day-30 target reads.
    pub fn total(&self, status: Status) -> u64 {
        self.of(Cause::Debt, None, status)
    }

    /// The rows of `cause`, every status.
    pub fn rows_of(&self, cause: Cause) -> u64 {
        Status::ALL
            .into_iter()
            .map(|s| self.of(cause, None, s))
            .sum()
    }

    /// The kinds a debt row is of, in the registry's order.
    pub(crate) fn debt_kinds(&self) -> Vec<Kind> {
        Kind::ALL
            .into_iter()
            .filter(|k| Status::ALL.into_iter().any(|s| self.debt(*k, s) > 0))
            .collect()
    }

    /// The causes other than debt a row is of, in [`Cause::ALL`]'s order.
    pub(crate) fn apart(&self) -> Vec<Cause> {
        Cause::ALL
            .into_iter()
            .filter(|c| *c != Cause::Debt && self.rows_of(*c) > 0)
            .collect()
    }
}

/// The `divergence-reason` instrument's reading over the workspace: fail
/// when an accepted row lacks what the guard asks ([`Lack`]): a reason,
/// a design citation on a `by-design` row, a `retires` on an
/// `oracle-build` row, the digests of a `trace:dump/` row. It names each such row, how many ids it covers
/// where it covers any, and prints the text of the manifest rule whose
/// `check` predicate names this instrument. A covering row is held to the
/// rule once, as one reason. The counts are the numbers.
pub fn reason(registry: &Registry, manifest: &Manifest, root: &Path) -> Result<Reading> {
    let rule = manifest.rule_checked_by(INSTRUMENT);
    let mut reading =
        Reading::for_scope(INSTRUMENT, "workspace", manifest, &Snapshot::disk(root)?)?;
    if let Some(rule) = rule {
        reading.rule.clone_from(&rule.id);
        reading.adr = rule.adr.adr_field();
    }
    let rows = registry.rows().len();
    let counts = registry.counts();
    let refused = registry.refused();
    reading.count("rows", u64::try_from(rows)?)?;
    reading.count("ids", counts.ids)?;
    for status in Status::ALL {
        let n = counts.by_status.get(&status).copied().unwrap_or(0);
        reading.count(status.as_str(), n)?;
    }
    let mut failures = Vec::new();
    for lack in Lack::ALL {
        let named: Vec<String> = refused
            .iter()
            .filter(|(_, l)| *l == lack)
            .map(|(r, _)| match r.covers.len() {
                0 => r.id.to_string(),
                n => format!("{} (covering {n} ids)", r.id),
            })
            .collect();
        reading.count(lack.number(), u64::try_from(named.len())?)?;
        if !named.is_empty() {
            failures.push(format!(
                "{} accepted row{} {}: {}",
                named.len(),
                reading::plural(named.len()),
                lack.says(),
                named.join(", ")
            ));
        }
    }
    if failures.is_empty() {
        reading.status = reading::Status::Pass;
        reading.message = format!(
            "{FILE}: {rows} rows holding {} ids, every accepted row has a reason, every by-design row a citation, every oracle-build row a `retires` and every trace dump its digests",
            counts.ids
        );
    } else {
        reading.status = reading::Status::Fail;
        reading.message = format!(
            "{FILE}: {}. {}",
            failures.join("; "),
            rule.map(|r| r.text.as_str()).unwrap_or_default()
        );
    }
    Ok(reading)
}
