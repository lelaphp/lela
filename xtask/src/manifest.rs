//! The seam manifest, `seams.toml`, and the oracle pin it points at,
//! `oracle.toml`, loaded by one reader (harness-design.md §4.1, §4.3).
//!
//! The grammar is `[oracle] [budget] [[crate]] [[seam]] [[edge]] [[rule]]`,
//! each rule carrying exactly one predicate. A manifest with a key outside
//! the grammar is refused with the offending key named; so is a rule with
//! zero or two predicates. The numbers live in the file; nothing here
//! restates one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The file name of the manifest at the workspace root.
pub const FILE: &str = "seams.toml";

/// `seams.toml` at its current value, with the oracle pin loaded through it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub oracle: PinRow,
    pub budget: Budget,
    #[serde(rename = "crate", default)]
    pub crates: Vec<Crate>,
    #[serde(rename = "seam", default)]
    pub seams: Vec<Seam>,
    #[serde(rename = "edge", default)]
    pub edges: Vec<Edge>,
    #[serde(rename = "rule", default)]
    pub rules: Vec<Rule>,
    /// The pin the `[oracle]` row names, loaded from that file.
    #[serde(skip)]
    pub pin: Oracle,
    /// The workspace the manifest was loaded from; empty for `parse`.
    #[serde(skip)]
    pub root: PathBuf,
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

/// `[[crate]]`: one crate and its ceiling; nothing about `unsafe`.
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

/// `oracle.toml`: the pinned oracle and its invocation recipe. Read through
/// the manifest's `[oracle]` row and nowhere else; the version is a value of
/// this file, not of any code. Its grammar is the oracle module's to hold
/// (harness-design.md §4.3), so an unknown key here is not refused.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Oracle {
    pub oracle: Binary,
    pub source: Source,
    pub recipe: Recipe,
    #[serde(default)]
    pub regime: BTreeMap<String, Regime>,
}

/// The `php` binary: version, where it is on this machine, and its digest.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Binary {
    pub version: String,
    pub path: PathBuf,
    pub md5: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Source {
    pub tag: String,
    pub commit: String,
    pub path: PathBuf,
    pub phpt_files: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Recipe {
    pub script_name: String,
    pub preamble: String,
    pub args: Vec<String>,
    pub ini: Vec<String>,
    pub env_clear: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub limits: Limits,
    pub normalise: Normalise,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Limits {
    pub timeout_s: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Normalise {
    pub crlf_to_lf: bool,
    pub trim: String,
    pub scratch_token: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Regime {
    pub ini: Vec<String>,
}

impl Manifest {
    /// Load `seams.toml` from the workspace root, then the pin it names.
    pub fn load(root: &Path) -> Result<Manifest> {
        let path = root.join(FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut manifest = Manifest::parse(&text).with_context(|| format!("{}", path.display()))?;
        let pin_path = root.join(&manifest.oracle.pin);
        let pin_text = fs::read_to_string(&pin_path)
            .with_context(|| format!("reading the oracle pin {}", pin_path.display()))?;
        manifest.pin =
            toml::from_str(&pin_text).with_context(|| format!("{}", pin_path.display()))?;
        manifest.root = root.to_path_buf();
        Ok(manifest)
    }

    /// Parse the manifest text alone and hold it to the grammar. The pin is
    /// left empty; `load` fills it.
    pub fn parse(text: &str) -> Result<Manifest> {
        let manifest: Manifest = toml::from_str(text)?;
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

    /// The path of a reading's scope, relative to the root: a crate's
    /// sources, or the whole workspace for `"workspace"`.
    pub fn scope_path(&self, scope: &str) -> Option<PathBuf> {
        if scope == "workspace" {
            return Some(PathBuf::from("."));
        }
        self.crate_named(scope).map(Crate::path)
    }
}
