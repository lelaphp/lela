//! The reading and its store (harness-design.md §4.2). Every instrument ends
//! by writing one reading; every rendering reads them; nothing else touches
//! the store. The store is `target/harness/<instrument>/<scope>.toml`, one
//! file per key, overwritten on each run, and `target/harness/log.jsonl`,
//! append-only, one line per reading.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml::value::Datetime;

use crate::git::Stamp;

/// Where the store lives, relative to the workspace root.
pub const DIR: &str = "target/harness";
/// The append-only log, inside the store.
pub const LOG: &str = "log.jsonl";

/// One reading, the one shape every instrument writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    /// `witness | phpt | compile-identity | crosscheck | port-map | api-check
    /// | budget | unsafe-audit | bench | fuzz | mutants | permit | done | vocab
    /// | divergence-reason`.
    pub instrument: String,
    /// A crate, a corpus subset, a gate id, or `"workspace"`.
    pub scope: String,
    /// A manifest rule id when one applies, else empty.
    pub rule: String,
    pub status: Status,
    pub numbers: BTreeMap<String, Number>,
    pub message: String,
    /// From the manifest when the rule names one, else empty.
    pub adr: String,
    /// The git tree hash of the scope's sources at the time.
    pub tree: String,
    pub commit: String,
    pub at: Datetime,
}

impl Reading {
    /// A reading of `instrument` over `scope` with every other field at its
    /// empty value and `at` at the Unix epoch; the instrument fills the rest.
    pub fn new(instrument: &str, scope: &str) -> Reading {
        Reading {
            instrument: instrument.to_owned(),
            scope: scope.to_owned(),
            rule: String::new(),
            status: Status::Skip,
            numbers: BTreeMap::new(),
            message: String::new(),
            adr: String::new(),
            tree: String::new(),
            commit: String::new(),
            at: EPOCH
                .parse()
                .unwrap_or_else(|_| unreachable!("{EPOCH} is a datetime")),
        }
    }
}

impl Reading {
    /// Record one count under `key`.
    pub fn count(&mut self, key: &str, n: u64) -> Result<()> {
        let n = i64::try_from(n).with_context(|| format!("{key} = {n} overflows a reading"))?;
        self.numbers.insert(key.to_owned(), n.into());
        Ok(())
    }

    /// Put the stamp's tree, commit and time on the reading.
    pub fn stamp(&mut self, stamp: &Stamp) {
        self.tree.clone_from(&stamp.tree);
        self.commit.clone_from(&stamp.commit);
        self.at = stamp.at;
    }

    /// The reading as JSON, `at` as an RFC 3339 string: the log line, and
    /// what `status --format json` carries.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(self).context("rendering the reading as JSON")?;
        value["at"] = serde_json::Value::String(self.at.to_string());
        Ok(value)
    }
}

const EPOCH: &str = "1970-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

/// One entry of `numbers`: a count or a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Number {
    Int(i64),
    Float(f64),
}

impl From<i64> for Number {
    fn from(n: i64) -> Number {
        Number::Int(n)
    }
}

impl Number {
    /// A count as a number; one past `i64::MAX` is not a count this store
    /// will see, and saturates.
    pub fn count(n: impl TryInto<i64>) -> Number {
        Number::Int(n.try_into().unwrap_or(i64::MAX))
    }
}

impl From<f64> for Number {
    fn from(n: f64) -> Number {
        Number::Float(n)
    }
}

impl std::fmt::Display for Number {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Number::Int(n) => write!(f, "{n}"),
            Number::Float(n) => write!(f, "{n}"),
        }
    }
}

/// The store: a directory of readings and its log.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The store of the workspace at `root`.
    pub fn of_workspace(root: &Path) -> Store {
        Store::at(root.join(DIR))
    }

    /// A store rooted at `dir`, whether or not it exists yet.
    pub fn at(dir: impl Into<PathBuf>) -> Store {
        Store { root: dir.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file of one instrument and scope.
    pub fn path(&self, instrument: &str, scope: &str) -> Result<PathBuf> {
        for part in [instrument, scope] {
            let clean = Path::new(part)
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
            if part.is_empty() || !clean {
                bail!(
                    "a reading's instrument and scope name a file under the store; {part:?} does not"
                );
            }
        }
        Ok(self.root.join(instrument).join(format!("{scope}.toml")))
    }

    /// Write one reading: its file, overwritten, and one line of the log.
    pub fn write(&self, reading: &Reading) -> Result<PathBuf> {
        let path = self.path(&reading.instrument, &reading.scope)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string(reading).context("rendering the reading")?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;

        let line = reading.to_json()?;
        let log = self.root.join(LOG);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("opening {}", log.display()))?;
        writeln!(file, "{line}").with_context(|| format!("appending to {}", log.display()))?;
        Ok(path)
    }

    /// The reading of one instrument and scope, if one has been written.
    pub fn read(&self, instrument: &str, scope: &str) -> Result<Option<Reading>> {
        let path = self.path(instrument, scope)?;
        if !path.exists() {
            return Ok(None);
        }
        Self::read_file(&path).map(Some)
    }

    /// Every reading in the store, in path order. A missing store is empty.
    pub fn readings(&self) -> Result<Vec<Reading>> {
        let mut files = Vec::new();
        if self.root.is_dir() {
            walk(&self.root, &mut files)?;
        }
        files.sort();
        files
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .map(|p| Self::read_file(p))
            .collect()
    }

    fn read_file(path: &Path) -> Result<Reading> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{}", path.display()))
    }
}

/// Every file under `dir`, recursively, in directory order.
pub(crate) fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}
