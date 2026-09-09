//! The seam manifest, `seams.toml`, and the oracle pin it points at,
//! `oracle.toml`, loaded by one reader (harness-design.md §4.1, §4.3); the
//! pin's grammar is the oracle module's.
//!
//! The grammar is `[oracle] [budget] [telemetry] [[crate]] [[seam]]
//! [[edge]] [[rule]] [[week]]`, each rule carrying exactly one predicate
//! and each week's read naming an instrument `xtask` has, as a status or
//! a number against a bound. A manifest with a key outside the grammar is
//! refused with the offending key named; so is a rule with zero or two
//! predicates. The numbers live in the file; nothing here restates one,
//! and an optional table that is absent is absent, never defaulted.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::oracle::Pin;

/// The file name of the manifest at the workspace root.
pub const FILE: &str = "seams.toml";

/// `seams.toml` at its current value, with the oracle pin loaded through it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub oracle: PinRow,
    pub budget: Budget,
    #[serde(default)]
    pub telemetry: Option<Telemetry>,
    #[serde(rename = "crate", default)]
    pub crates: Vec<Crate>,
    #[serde(rename = "seam", default)]
    pub seams: Vec<Seam>,
    #[serde(rename = "edge", default)]
    pub edges: Vec<Edge>,
    #[serde(rename = "rule", default)]
    pub rules: Vec<Rule>,
    #[serde(rename = "week", default)]
    pub weeks: Vec<Week>,
    /// The pin the `[oracle]` row names, loaded from that file by the
    /// oracle module, whose grammar it is.
    #[serde(skip)]
    pub pin: Pin,
    /// The workspace the manifest was loaded from; empty for `parse`.
    #[serde(skip)]
    pub root: PathBuf,
}

/// `[telemetry]`: the sensor telemetry's one number (harness-design.md
/// §7), optional; without it `status` renders no silent column and says so.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    /// An instrument with readings and no failure this many days is a
    /// candidate for silent.
    pub silent_days: u64,
}

/// `[oracle]`: the only place the manifest says where the pin is.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinRow {
    pub pin: String,
}

/// `[budget]`: the workspace-wide ceilings (design §4.1, §10).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub total: u64,
    pub new: u64,
    pub docs: u64,
    pub harness: u64,
    pub lock_packages: u64,
    pub clean_build_release_s: u64,
    pub api_items: u64,
    pub bindings: u64,
}

/// `[[crate]]`: one crate, its ceiling and the subsets of the pinned
/// corpus it is measured by; nothing about `unsafe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Crate {
    pub name: String,
    pub budget: u64,
    #[serde(default)]
    pub lock: OneOrMany,
    #[serde(default)]
    pub lock_from: BTreeMap<String, String>,
    pub feature: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    /// The directories of the pinned php-src tree whose `.phpt` tests are
    /// this crate's, relative to it (`Zend/tests`). An extension's are its
    /// own by the design and are not written here; a crate with neither is
    /// not asked for a phpt reading.
    #[serde(default)]
    pub phpt: Vec<String>,
}

impl Crate {
    /// Where the crate's sources are, relative to the workspace root:
    /// `ext/<name>` for an extension, `crates/<name>` otherwise, and
    /// `xtask` for the harness.
    pub fn path(&self) -> PathBuf {
        if self.name == "xtask" || self.name.starts_with("ext/") {
            PathBuf::from(&self.name)
        } else {
            Path::new("crates").join(&self.name)
        }
    }

    /// The subsets of the pinned corpus this crate owns, relative to the
    /// pinned tree: the ones its row names, or `ext/<name>/tests` for an
    /// extension, whose subset the design already states (design §12,
    /// `ext/AGENTS.md`). A crate that owns none is not asked for a phpt
    /// reading, and `phpt` refuses to guess one for it (harness §4.5
    /// condition 1, §7).
    pub fn subsets(&self) -> Vec<PathBuf> {
        if !self.phpt.is_empty() {
            return self.phpt.iter().map(PathBuf::from).collect();
        }
        match self.name.strip_prefix("ext/") {
            Some(extension) => vec![Path::new("ext").join(extension).join("tests")],
            None => Vec::new(),
        }
    }

    /// Whether the crate is behind a feature, and so outside the default
    /// build the per-commit lane measures (harness §7).
    pub fn optional(&self) -> bool {
        self.feature.is_some()
    }
}

/// `[[seam]]`: one locked seam (design §4.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seam {
    pub name: String,
    pub producer: String,
    pub facade: Option<String>,
    #[serde(default)]
    pub consumers: Vec<String>,
    pub lock: String,
    #[serde(default)]
    pub adr: OneOrMany,
    #[serde(default)]
    pub generated: bool,
    #[serde(default)]
    pub allowlist: bool,
    #[serde(default)]
    pub sections: Vec<String>,
}

/// `[[edge]]`: one permitted (or denied) dependency edge (design §4.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub from: String,
    pub to: OneOrMany,
    #[serde(default)]
    pub adr: OneOrMany,
    #[serde(default)]
    pub reexports_only: bool,
    #[serde(default)]
    pub only: bool,
    pub order: Option<String>,
    pub allowlist: Option<String>,
    pub lock: Option<String>,
    #[serde(default)]
    pub deny: bool,
}

/// `[[rule]]`: an `id`, a scope or a tool, one predicate, an optional `adr`,
/// and the `text` a failure prints; for an `on` rule, which never denies,
/// it is the note an allow carries. Two rules of the tracked manifest
/// (`unsafe.homes`, `generated.committed`) carry neither scope nor tool, so
/// their absence is not refused.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub scope: OneOrMany,
    #[serde(default)]
    pub tool: OneOrMany,
    #[serde(default)]
    pub exempt: Vec<String>,
    #[serde(default)]
    pub adr: OneOrMany,
    pub text: String,
    #[serde(flatten)]
    pub predicate: Predicates,
}

/// The ten predicates of harness-design.md §4.1, each optional in the
/// grammar so the file parses; `Manifest::parse` then holds every rule to
/// exactly one. `unless` and `with` qualify `deny_pattern` and `deny_argv`
/// and are refused beside any other predicate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predicates {
    pub deny_types: Option<Vec<String>>,
    pub allow: Option<Vec<String>>,
    pub deny_pattern: Option<String>,
    pub unless: Option<String>,
    pub deny_argv: Option<Vec<String>>,
    pub with: Option<String>,
    pub on: Option<String>,
    pub deny: Option<bool>,
    pub deny_lints: Option<Vec<String>>,
    pub check: Option<String>,
    pub avoid_from: Option<String>,
    pub generated: Option<BTreeMap<String, String>>,
}

impl Predicates {
    /// The ten predicate names, in the design's order.
    pub const NAMES: [&'static str; 10] = [
        "deny_types",
        "allow",
        "deny_pattern",
        "deny_argv",
        "on",
        "deny",
        "deny_lints",
        "check",
        "avoid_from",
        "generated",
    ];

    /// The names of the predicates this rule carries.
    pub fn named(&self) -> Vec<&'static str> {
        let set = [
            self.deny_types.is_some(),
            self.allow.is_some(),
            self.deny_pattern.is_some(),
            self.deny_argv.is_some(),
            self.on.is_some(),
            self.deny.is_some(),
            self.deny_lints.is_some(),
            self.check.is_some(),
            self.avoid_from.is_some(),
            self.generated.is_some(),
        ];
        Self::NAMES
            .iter()
            .zip(set)
            .filter(|(_, on)| *on)
            .map(|(name, _)| *name)
            .collect()
    }

    /// Whether the predicate describes one write or one command, so `permit`
    /// can judge it before the tool runs: `deny`, `deny_argv`, `deny_pattern`,
    /// `deny_types`, `allow`, `on`. The other four read the tree after the
    /// write (harness-design.md §4.4).
    pub fn describes_a_call(&self) -> bool {
        self.deny.is_some()
            || self.deny_argv.is_some()
            || self.deny_pattern.is_some()
            || self.deny_types.is_some()
            || self.allow.is_some()
            || self.on.is_some()
    }

    /// A qualifier that has no predicate to qualify: `unless` without
    /// `deny_pattern`, `with` without `deny_argv`.
    pub fn orphaned_qualifier(&self) -> Option<&'static str> {
        if self.unless.is_some() && self.deny_pattern.is_none() {
            Some("unless")
        } else if self.with.is_some() && self.deny_argv.is_none() {
            Some("with")
        } else {
            None
        }
    }
}

impl Rule {
    /// Whether the rule's `scope` covers `path`: the manifest's `scope` key
    /// is one grammar, and every sensor that reads it reads it here
    /// (harness-design.md §4.1). A rule without a scope covers nothing on
    /// its own; what such a rule judges is its predicate's.
    pub fn covers(&self, path: &Path) -> bool {
        self.scope
            .iter()
            .any(|pattern| scope_matches(pattern, path))
    }
}

/// Whether `path` is at or under the scope pattern: `*` is one segment or
/// part of one, `**` any number of segments, and a pattern that ends before
/// the path does names a directory the path is under.
pub fn scope_matches(pattern: &str, path: &Path) -> bool {
    let pattern: Vec<&str> = pattern
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let path = path.to_string_lossy();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    segments_match(&pattern, &segments)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (None, _) => true,
        (Some((&"**", rest)), _) => {
            segments_match(rest, path)
                || path
                    .first()
                    .is_some_and(|_| segments_match(pattern, &path[1..]))
        }
        (Some((first, rest)), Some((segment, tail))) => {
            segment_matches(first, segment) && segments_match(rest, tail)
        }
        (Some(_), None) => false,
    }
}

/// One segment against a pattern in which `*` matches any run of characters.
fn segment_matches(pattern: &str, segment: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == segment,
        Some((head, tail)) => {
            segment.starts_with(head)
                && segment[head.len()..]
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain(std::iter::once(segment.len() - head.len()))
                    .any(|i| segment_matches(tail, &segment[head.len() + i..]))
        }
    }
}

/// A key the file writes as one string or as a list of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    /// The ADR numbers as a reading's `adr` field: `"0004, 0005"`.
    pub fn adr_field(&self) -> String {
        self.iter().collect::<Vec<_>>().join(", ")
    }

    /// The ADR numbers as a failure text cites them: `"(ADR-0004, ADR-0005)"`,
    /// empty when there are none (harness-design.md §10 rule 2).
    pub fn adr_cited(&self) -> String {
        let cited: Vec<String> = self.iter().map(|n| format!("ADR-{n}")).collect();
        if cited.is_empty() {
            String::new()
        } else {
            format!("({})", cited.join(", "))
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        match self {
            OneOrMany::None => [].iter(),
            OneOrMany::One(one) => std::slice::from_ref(one).iter(),
            OneOrMany::Many(many) => many.iter(),
        }
        .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

impl<const N: usize> PartialEq<[&str; N]> for OneOrMany {
    fn eq(&self, other: &[&str; N]) -> bool {
        self.iter().eq(other.iter().copied())
    }
}

/// `[[week]]`: one exit bar of the thirty days (design §11), measured by
/// the readings it names and rendered by `status --section week` (§9).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Week {
    /// The days of the bar as the design's table writes them, `"3-7"`.
    pub days: String,
    /// The bar in the design's words.
    pub text: String,
    /// The readings the bar is measured by; the bar passes when every one
    /// holds.
    #[serde(default)]
    pub reads: Vec<Measure>,
}

/// One reading a bar names: the latest of `instrument` over `scope`, which
/// holds when its status is pass, or, with `number`, when that number lies
/// within `min` and `max`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measure {
    pub instrument: String,
    pub scope: String,
    pub number: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Manifest {
    /// Load `seams.toml` from the workspace root, then the pin it names.
    pub fn load(root: &Path) -> Result<Manifest> {
        let path = root.join(FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut manifest = Manifest::parse(&text).with_context(|| format!("{}", path.display()))?;
        manifest.pin = Pin::load(&root.join(&manifest.oracle.pin))?;
        manifest.root = root.to_path_buf();
        Ok(manifest)
    }

    /// Parse the manifest text alone and hold it to the grammar. The pin is
    /// left empty; `load` fills it.
    pub fn parse(text: &str) -> Result<Manifest> {
        let manifest: Manifest = toml::from_str(text)?;
        let instruments = crate::cli::instruments();
        for week in &manifest.weeks {
            for read in &week.reads {
                if !instruments.contains(&read.instrument) {
                    bail!(
                        "week \"{}\" reads {} {}, and {} is not an instrument xtask has; the instrument ships its row",
                        week.days,
                        read.instrument,
                        read.scope,
                        read.instrument
                    );
                }
                let bounded = read.min.is_some() || read.max.is_some();
                if read.number.is_some() && !bounded {
                    bail!(
                        "week \"{}\" reads {} {} by a number without a min or a max",
                        week.days,
                        read.instrument,
                        read.scope
                    );
                }
                if read.number.is_none() && bounded {
                    bail!(
                        "week \"{}\" bounds {} {} without naming the number",
                        week.days,
                        read.instrument,
                        read.scope
                    );
                }
            }
        }
        for rule in &manifest.rules {
            let named = rule.predicate.named();
            if named.len() != 1 {
                bail!(
                    "rule \"{}\" carries {} predicates ({}); each rule carries exactly one of {}",
                    rule.id,
                    named.len(),
                    named.join(", "),
                    Predicates::NAMES.join(", ")
                );
            }
            if let Some(qualifier) = rule.predicate.orphaned_qualifier() {
                bail!(
                    "rule \"{}\" has `{qualifier}` beside `{}`; `unless` qualifies deny_pattern and `with` qualifies deny_argv",
                    rule.id,
                    named[0]
                );
            }
        }
        Ok(manifest)
    }

    pub fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// The rule whose `check` predicate names `instrument`: how a sensor
    /// that reads the tree after the write finds the rule it evaluates
    /// (harness-design.md §4.1, §4.4).
    pub fn rule_checked_by(&self, instrument: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|r| r.predicate.check.as_deref() == Some(instrument))
    }

    /// The ceiling on a seam's lock width: `[budget] api_items` for the api
    /// seam (design §4.4, §4.5); no other lock has one.
    pub fn lock_ceiling(&self, seam: &Seam) -> Option<u64> {
        (seam.name == "api").then_some(self.budget.api_items)
    }

    /// The title of `docs/adr/<number>-*.md`, when the workspace has it.
    pub fn adr_title(&self, number: &str) -> Option<String> {
        let entry = fs::read_dir(self.root.join("docs/adr"))
            .ok()?
            .flatten()
            .find(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|f| f.starts_with(&format!("{number}-")))
            })?;
        let text = fs::read_to_string(entry.path()).ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("# "))
            .map(|t| t.trim().to_owned())
    }

    pub fn crate_named(&self, name: &str) -> Option<&Crate> {
        self.crates.iter().find(|c| c.name == name)
    }

    /// The crate whose sources a path lies under: one function over the
    /// `[[crate]]` rows (harness-design.md §4.4).
    pub fn crate_of(&self, path: &Path) -> Option<&Crate> {
        self.crates.iter().find(|c| path.starts_with(c.path()))
    }

    /// The crates any of `paths` lies under, in manifest order, each once:
    /// the touched crates of a diff (harness-design.md §4.5 condition 1,
    /// §6 "touched crates").
    pub fn crates_of<P: AsRef<Path>>(&self, paths: &[P]) -> Vec<&Crate> {
        self.crates
            .iter()
            .filter(|c| paths.iter().any(|p| p.as_ref().starts_with(c.path())))
            .collect()
    }

    /// The crate a lock lives with: the one whose row names it, or the
    /// crate its `lock_from` entry names for a lock listed on one row and
    /// generated from another (`api.lock` on `php_engine`'s row, from
    /// `php_ext`). `None` for a lock no row names.
    pub fn lock_home(&self, lock: &str) -> Option<String> {
        self.crates
            .iter()
            .find(|c| c.lock.iter().any(|l| l == lock))
            .map(|c| {
                c.lock_from
                    .get(lock)
                    .cloned()
                    .unwrap_or_else(|| c.name.clone())
            })
    }

    /// The path a reading's `tree` is hashed over, relative to the root:
    /// the one mapping from a scope to its sources, so what an instrument
    /// stamps and what `done` hashes agree (harness-design.md §4.2, §4.5
    /// condition 1). One arm per scope kind a reading carries:
    ///
    /// - `"workspace"`: the whole tree.
    /// - a crate: its sources ([`Crate::path`]).
    /// - a subset: the crate whose subset it is, under the crate's name
    ///   and beside a tier lane or a control of the runner alike
    ///   (`ext/pcre@jit`, `ext/pcre@control`), so `done` finds the
    ///   engine's reading under the crate; a directory of the pinned
    ///   corpus no crate owns (`Zend/tests`) is the whole tree, since no
    ///   crate's sources are what it measures and `done` never asks for it.
    /// - a lock: the seam's name maps to the directory the lock lives in,
    ///   the crate [`Manifest::lock_home`] names, and the whole tree for a
    ///   lock no row names.
    /// - a generated file the manifest names (`ext/clippy.toml`): itself.
    ///
    /// `None` for a name none of these know, which the reading's
    /// constructor refuses by name rather than stamping over a guess.
    pub fn scope_path(&self, scope: &str) -> Option<PathBuf> {
        if scope == "workspace" {
            return Some(PathBuf::from("."));
        }
        let name = scope.split_once('@').map_or(scope, |(name, _)| name);
        if let Some(krate) = self.crate_named(name) {
            return Some(krate.path());
        }
        if let Some(seam) = self.seams.iter().find(|s| s.name == name) {
            return Some(
                self.lock_home(&seam.lock)
                    .and_then(|home| self.crate_named(&home))
                    .map_or_else(|| PathBuf::from("."), Crate::path),
            );
        }
        let generated = self
            .rules
            .iter()
            .flat_map(|r| r.predicate.generated.iter().flatten())
            .any(|(file, _)| file == name);
        if generated {
            return Some(PathBuf::from(name));
        }
        let corpus = self.pin.source.tree();
        if !corpus.as_os_str().is_empty() && corpus.join(name).is_dir() {
            return Some(PathBuf::from("."));
        }
        None
    }
}
