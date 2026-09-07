//! The divergence registry, `docs/divergence.toml` (harness-design.md §4.3):
//! one row per accepted difference from the oracle, keyed by a witness hash,
//! a phpt path, a compile-identity path or a trace site. A person writes a
//! row and its reason; `compare`'s caller or the fuzzer may propose one with
//! status `open` and an empty reason; the `divergence-reason` instrument
//! fails an accepted row without a reason. The TOML is the truth; `status
//! --section divergence` is the rendering.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;
use crate::reading::{self, Number, Reading};

/// The registry's path, relative to the workspace root.
pub const FILE: &str = "docs/divergence.toml";
/// The instrument that holds the registry to its rule: the subcommand, the
/// reading's `instrument`, and what the manifest's `check` predicate names.
pub const INSTRUMENT: &str = "divergence-reason";

/// One row of the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Divergence {
    /// `witness:<hash> | phpt:<path> | compile:<path> | trace:<site>`; the
    /// prefix is the kind's, and `runner` shares `phpt:`.
    pub id: String,
    pub kind: Kind,
    /// Written by a person; empty on a proposed row.
    pub reason: String,
    pub status: Status,
    /// The date the row was written, `YYYY-MM-DD`, as the file quotes it.
    pub since: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Witness,
    Phpt,
    /// A `phpt:` id where the nightly runner control, not the engine, disagrees.
    Runner,
    Compile,
    Trace,
}

impl Kind {
    /// The five kinds, in the registry's order.
    pub const ALL: [Kind; 5] = [
        Kind::Witness,
        Kind::Phpt,
        Kind::Runner,
        Kind::Compile,
        Kind::Trace,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Witness => "witness",
            Kind::Phpt => "phpt",
            Kind::Runner => "runner",
            Kind::Compile => "compile",
            Kind::Trace => "trace",
        }
    }

    /// The prefix an id of this kind carries, colon included.
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Witness => "witness:",
            Kind::Phpt | Kind::Runner => "phpt:",
            Kind::Compile => "compile:",
            Kind::Trace => "trace:",
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

/// The registry: its rows, and the file they came from when loaded.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub rows: Vec<Divergence>,
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

    /// Parse the registry's text and hold it to the row shape; a duplicate
    /// id, or an id whose prefix is not its kind's, is refused.
    pub fn parse(text: &str) -> Result<Registry> {
        let shape: FileShape = toml::from_str(text)?;
        let mut seen = std::collections::BTreeSet::new();
        for row in &shape.divergence {
            if !seen.insert(&row.id) {
                bail!("the id {:?} has two rows", row.id);
            }
            if !row.id.starts_with(row.kind.prefix()) {
                bail!(
                    "the id {:?} is of kind {}, whose ids start with {:?}",
                    row.id,
                    row.kind.as_str(),
                    row.kind.prefix()
                );
            }
        }
        Ok(Registry {
            rows: shape.divergence,
            path: None,
        })
    }

    pub fn row(&self, id: &str) -> Option<&Divergence> {
        self.rows.iter().find(|r| r.id == id)
    }

    /// The row that accepts a difference for `id`, if one does.
    pub fn accepted(&self, id: &str) -> Option<&Divergence> {
        self.row(id).filter(|r| r.status == Status::Accepted)
    }

    /// Propose a row: status `open`, empty reason, dated today, appended to
    /// the file after its prose. The id must be new and carry the kind's
    /// prefix, and the registry must have been loaded from its file.
    pub fn propose(&mut self, id: &str, kind: Kind) -> Result<&Divergence> {
        let Some(path) = &self.path else {
            bail!("the registry was parsed from text, not loaded from {FILE}; nothing to append to")
        };
        if let Some(row) = self.row(id) {
            bail!(
                "the id {id:?} already has a row with status {}",
                row.status.as_str()
            );
        }
        if !id.starts_with(kind.prefix()) {
            bail!(
                "the id {id:?} is not of kind {}, whose ids start with {:?}",
                kind.as_str(),
                kind.prefix()
            );
        }
        let since = crate::git::now()?
            .date
            .context("the clock gave no date")?
            .to_string();
        let row = Divergence {
            id: id.to_owned(),
            kind,
            reason: String::new(),
            status: Status::Open,
            since,
        };
        let text = toml::to_string(&Rows {
            divergence: std::slice::from_ref(&row),
        })
        .context("rendering the row")?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        write!(file, "\n{text}").with_context(|| format!("appending to {}", path.display()))?;
        self.rows.push(row);
        Ok(self
            .rows
            .last()
            .unwrap_or_else(|| unreachable!("just pushed")))
    }

    /// Accepted rows whose reason nobody wrote.
    pub fn unreasoned(&self) -> Vec<&Divergence> {
        self.rows
            .iter()
            .filter(|r| r.status == Status::Accepted && r.reason.trim().is_empty())
            .collect()
    }

    /// Rows by kind and by status.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
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

/// How many rows of each kind, of each status, and of each pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub by_kind: BTreeMap<Kind, u64>,
    pub by_status: BTreeMap<Status, u64>,
    pub cells: BTreeMap<(Kind, Status), u64>,
}

impl Counts {
    pub fn cell(&self, kind: Kind, status: Status) -> u64 {
        self.cells.get(&(kind, status)).copied().unwrap_or(0)
    }
}

/// The `divergence-reason` instrument's reading over the workspace: fail
/// when an accepted row has no reason, naming each such row and printing
/// the text of the manifest rule whose `check` predicate names this
/// instrument; the counts are the numbers. The stamp is the caller's.
pub fn reason(registry: &Registry, manifest: &Manifest) -> Reading {
    let rule = manifest
        .rules
        .iter()
        .find(|r| r.predicate.check.as_deref() == Some(INSTRUMENT));
    let mut reading = Reading::new(INSTRUMENT, "workspace");
    if let Some(rule) = rule {
        reading.rule.clone_from(&rule.id);
        reading.adr = rule.adr.iter().collect::<Vec<_>>().join(", ");
    }
    let counts = registry.counts();
    let unreasoned = registry.unreasoned();
    reading
        .numbers
        .insert("rows".into(), Number::count(registry.rows.len()));
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
            "{FILE}: {} rows, every accepted row has a reason",
            registry.rows.len()
        );
    } else {
        reading.status = reading::Status::Fail;
        let ids: Vec<&str> = unreasoned.iter().map(|r| r.id.as_str()).collect();
        reading.message = format!(
            "{FILE}: {} accepted row{} without a reason: {}. {}",
            ids.len(),
            if ids.len() == 1 { "" } else { "s" },
            ids.join(", "),
            rule.map(|r| r.text.as_str()).unwrap_or_default()
        );
    }
    reading
}
