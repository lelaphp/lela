//! The seam manifest, `seams.toml`, and the oracle pin it points at,
//! `oracle.toml`, loaded by one reader (harness-design.md §4.1, §4.3); the
//! pin's grammar is the oracle module's.
//!
//! The grammar is `[oracle] [budget] [telemetry] [[crate]] [[seam]]
//! [[edge]] [[rule]] [[bench]]`, each rule carrying exactly one predicate.
//! A manifest with a key outside the grammar is refused with the offending
//! key named; so is a rule with zero or two predicates. The numbers live
//! in the file; nothing here restates one, and an optional table that is
//! absent is absent, never defaulted.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::layout;
use crate::oracle::Pin;
use crate::phpt::Scope;
use crate::track::Ticket;

/// The file name of the manifest at the workspace root.
pub const FILE: &str = "seams.toml";

/// The scope of `budget`'s reading over `scripts/**`, and the directory
/// it is stamped over: the one tree beside the crates that a `[budget]`
/// row holds (`[budget] scripts`).
pub const SCRIPTS: &str = "scripts";

/// `seams.toml` at its current value, with the oracle pin loaded through it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Manifest {
    pub oracle: PinRow,
    pub budget: Budget,
    pub telemetry: Option<Telemetry>,
    #[serde(rename = "crate", default)]
    pub crates: Vec<Crate>,
    #[serde(rename = "seam", default)]
    pub seams: Vec<Seam>,
    #[serde(rename = "edge", default)]
    pub edges: Vec<Edge>,
    #[serde(rename = "rule", default)]
    pub rules: Vec<Rule>,
    /// `[lock_home]`: the crate a lock's file lives with, for a lock no
    /// `[[crate]]` row names. A seam whose producer is a file and not a
    /// crate (`ops`, generated from `ops.toml`) has no row to carry it,
    /// and the file still lives somewhere; this table says where.
    #[serde(rename = "lock_home", default)]
    pub lock_homes: BTreeMap<String, String>,
    /// `[parity]`: the suites parity does not count, each under the ruling
    /// that put it there; empty where the file has no table.
    #[serde(default)]
    pub(crate) parity: Parity,
    /// `[divergence]`: the day-30 target the registry's debt rows are held
    /// to; `None` where the file has no table, which `status` says.
    pub divergence: Option<DivergenceTable>,
    /// `[[bench]]`: each perf bar's tolerance and the nights it was
    /// derived over (#2659); a bar without a row is a series held to none.
    #[serde(rename = "bench", default)]
    pub(crate) tolerances: Vec<Tolerance>,
    /// The pin the `[oracle]` row names, loaded from that file by the
    /// oracle module, whose grammar it is.
    #[serde(skip)]
    pub pin: Pin,
    /// The workspace the manifest was loaded from; empty for `parse`.
    #[serde(skip)]
    pub root: PathBuf,
}

/// `[telemetry]`: the one number the silent-instrument query of
/// harness-design.md §7 reads, optional; without it `status` renders no
/// silent column and says so.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Telemetry {
    /// An instrument with readings and no failure this many days is a
    /// candidate for silent.
    pub silent_days: u64,
}

/// `[divergence]`: what the divergence registry is held to (harness-design.md
/// §4.3). `target` is the day-30 target: the registry's `debt` rows, a
/// covering row once, number under it (Mitchell, under 10, on #2460); no
/// other cause counts toward it (#2462).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DivergenceTable {
    pub target: u64,
}

/// One `[[bench]]` row (#2659): a bar's tolerance, the 75th percentile of
/// its A/A control's nightly |paired delta| over `from` to `to`, floored at
/// 2%, in percentage points of the second arm; the first baseline is the
/// median of the bar's own nights over the same dates.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tolerance {
    pub bar: String,
    #[serde(rename = "tolerance_pct")]
    pub pct: f64,
    pub from: toml::value::Datetime,
    pub to: toml::value::Datetime,
}

/// `[parity]`: the suites php builds that parity does not count (design
/// §3, §12), each with the ruling that put it there; the reason is §12's
/// prose, which names this table (#1739).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Parity {
    #[serde(default)]
    pub out: Vec<RuledOut>,
}

/// One entry of `[parity] out`: suites of the pinned corpus, relative to
/// it, and the ticket whose ruling took them out, an issue's number and
/// never its prose.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuledOut {
    pub suites: Vec<PathBuf>,
    pub ruling: Ticket,
}

impl Parity {
    /// Refuse a suite ruled out twice, or inside or around another ruled
    /// suite, naming both entries and their rulings: a suite is ruled out
    /// once, under one ruling, so every reader names it once (#1930). Then
    /// refuse a suite ruled out that a `[[crate]]` row's subsets own,
    /// naming both: a suite is measured or ruled out, never both. A suite
    /// inside a row's subset is the row's, and so is a subset inside a
    /// suite.
    fn hold(&self, crates: &[Crate]) -> Result<()> {
        let ruled: Vec<(&Path, Ticket)> = self
            .out
            .iter()
            .flat_map(|out| out.suites.iter().map(|suite| (suite.as_path(), out.ruling)))
            .collect();
        for (at, &(suite, ruling)) in ruled.iter().enumerate() {
            if let Some((other, under)) =
                ruled[..at].iter().find(|(other, _)| overlaps(suite, other))
            {
                bail!(
                    "[parity] out rules {} out under {under}, and {} under {ruling}; a suite is ruled out once, under one ruling, never twice or inside another",
                    other.display(),
                    suite.display()
                );
            }
            let owned = crates.iter().find_map(|krate| {
                krate
                    .subsets()
                    .into_iter()
                    .find(|subset| overlaps(suite, subset))
                    .map(|subset| (krate, subset))
            });
            if let Some((krate, subset)) = owned {
                bail!(
                    "[parity] out rules {} out under {ruling}, and the [[crate]] row {} owns {} among its subsets; a suite parity does not count is no row's to measure",
                    suite.display(),
                    krate.name,
                    subset.display()
                );
            }
        }
        Ok(())
    }
}

/// Whether `a` and `b` are one place or one is a directory inside the
/// other, component by component: the one rule parity's suites, rulings
/// and rows overlap by, which `hold` and `status::parity::owned` both read.
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// `[oracle]`: the only place the manifest says where the pin is.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PinRow {
    pub pin: String,
}

/// `[budget]`: the workspace-wide ceilings (design §4.1, §10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Budget {
    pub total: u64,
    pub new: u64,
    pub docs: u64,
    pub harness: u64,
    pub lock_packages: u64,
    pub clean_build_release_s: u64,
    pub api_items: u64,
    /// `api.lock`'s `__private` lines, the helpers only a macro expansion
    /// names, held apart from `api_items` (design §4.4, revision 34);
    /// absent while a person has not written the row, and the whole lock
    /// is then held to `api_items`.
    pub api_private_items: Option<u64>,
    pub bindings: u64,
    /// `scripts/**` against its ceiling, with what under it is a generator
    /// and not counted; absent while a person has not written the row,
    /// and `budget` then counts the tree and holds it to nothing.
    pub scripts: Option<Scripts>,
}

impl Budget {
    /// Every ceiling `n`: the table a test builds when no ceiling is what
    /// it measures. The optional `scripts` and `api_private_items` rows
    /// stay absent, as the file without them parses.
    pub fn flat(n: u64) -> Budget {
        Budget {
            total: n,
            new: n,
            docs: n,
            harness: n,
            lock_packages: n,
            clean_build_release_s: n,
            api_items: n,
            api_private_items: None,
            bindings: n,
            scripts: None,
        }
    }
}

/// What one lock's lines are held to: `items` over every line but the
/// `__private` module's, and `private` over those, where a person has
/// written that row; without it, `items` holds every line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockCeiling {
    pub items: u64,
    pub private: Option<u64>,
}

/// `[budget] scripts`: the one statement of what `scripts/**` is held to
/// and what under it counts. Every tracked file under `scripts/` counts
/// except the `generators`, named by path relative to the root; which
/// files those are, and why, is the row's own comment to say. `budget`
/// reads this row and keeps no list of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Scripts {
    pub ceiling: u64,
    #[serde(default)]
    pub generators: Vec<String>,
}

impl Scripts {
    /// The row as the file would parse `ceiling` and `generators`: what a
    /// test builds to hold a scratch tree without writing the row.
    pub fn new(ceiling: u64, generators: &[&str]) -> Scripts {
        Scripts {
            ceiling,
            generators: generators.iter().map(|g| (*g).to_owned()).collect(),
        }
    }
}

/// `[[crate]]`: one crate, its ceiling and the subsets of the pinned
/// corpus it is measured by; nothing about `unsafe`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
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
    /// Whether this extension's port is not finished, so that its
    /// `crosscheck` and `phpt` readings are the port's meter and not a
    /// condition of done: `done` prints their numbers and holds the row
    /// met, the way it holds the skip the missing engine binary causes
    /// (harness §4.5 condition 1, §7; #383). Every other instrument holds
    /// the crate as it holds any other. A crate without the key is held to
    /// both readings the day it is created, and the key's deletion, a
    /// person's act, is the port's completion certificate.
    #[serde(default)]
    pub porting: bool,
    /// The oracle variant the crate is read under (#1782): `crosscheck`,
    /// `witness` and `phpt` run it under the pin's `[variant.<name>]` for
    /// the platform they run on, verified by its md5, and every crate
    /// without the key under the main oracle. A name the pin does not
    /// carry fails the crate's readings in those lanes, naming it.
    pub oracle: Option<String>,
}

impl Crate {
    /// A row by name alone, as the file would parse `name` and a ceiling of
    /// one: what a test that cares for the name builds. The rows are
    /// non-exhaustive outside `manifest`, so a test names the field it
    /// cares about here or by assignment and never spells out the rest.
    pub fn named(name: &str) -> Crate {
        Crate {
            name: name.to_owned(),
            budget: 1,
            ..Crate::default()
        }
    }

    /// The row read under the oracle variant `variant`.
    #[must_use]
    pub fn with_oracle(mut self, variant: &str) -> Crate {
        self.oracle = Some(variant.to_owned());
        self
    }

    /// The row with these `phpt` subsets, relative to the pinned tree.
    #[must_use]
    pub fn with_phpt(mut self, subsets: &[&str]) -> Crate {
        self.phpt = subsets.iter().map(|s| (*s).to_owned()).collect();
        self
    }

    /// The row behind `feature`, and so outside the default build.
    #[must_use]
    pub fn with_feature(mut self, feature: &str) -> Crate {
        self.feature = Some(feature.to_owned());
        self
    }

    /// Where the crate's sources are, relative to the workspace root:
    /// `ext/<name>` for an extension, `sapi/<name>` for a host,
    /// `crates/<name>` otherwise, and `xtask` for the harness
    /// ([`crate::layout`]).
    pub fn path(&self) -> PathBuf {
        layout::path(&self.name)
    }

    /// The crate's cargo package: `php_ext_<name>` for `ext/<name>`
    /// (ADR-0004), `php_sapi_<name>` for `sapi/<name>` (ADR-0008), the
    /// name itself otherwise ([`crate::layout`]).
    pub(crate) fn package(&self) -> String {
        layout::package(&self.name)
    }

    /// The subsets of the pinned corpus this crate owns, relative to the
    /// pinned tree: the ones its row names, or `ext/<name>` for an
    /// extension, whose subset the design already states (design §12,
    /// `ext/AGENTS.md`): every `.phpt` under the extension's directory, as
    /// run-tests.php walks one, so `ext/intl/uchar/tests` is `ext/intl`'s
    /// beside `ext/intl/tests` with no key in the row (#1590, #1598). A
    /// crate that owns none is not asked for a phpt reading, and `phpt`
    /// refuses to guess one for it (harness §4.5 condition 1, §7).
    pub fn subsets(&self) -> Vec<PathBuf> {
        if !self.phpt.is_empty() {
            return self.phpt.iter().map(PathBuf::from).collect();
        }
        match self.name.strip_prefix("ext/") {
            Some(extension) => vec![derived_subset(extension)],
            None => Vec::new(),
        }
    }

    /// Whether the crate's subset is the one the design derives for an
    /// extension, `ext/<name>`, and not one its row names. A derived
    /// directory that holds no `.phpt` in the pinned tree, or that the
    /// tree does not have, is an extension with no tests of its own, as
    /// `ext/lexbor`, php-src's vendored library whose tests are
    /// `ext/dom`'s and `ext/uri`'s, is: its reading is a skip that says
    /// so. A named one the tree does not have is the manifest and the pin
    /// disagreeing, and its reading fails (#18, #1231).
    pub(crate) fn derives_subset(&self) -> bool {
        self.phpt.is_empty() && self.name.starts_with("ext/")
    }

    /// Whether the crate is behind a feature, and so outside the default
    /// build the per-commit lane measures (harness §7).
    pub fn optional(&self) -> bool {
        self.feature.is_some()
    }
}

/// The subset the design derives for the extension `name`, relative to
/// the pinned tree: its directory, every `.phpt` under which is the
/// subset's (#1590). The one statement of the rule, read by a row with no
/// `phpt` key and by the nightly lane's corpus alike.
pub(crate) fn derived_subset(name: &str) -> PathBuf {
    Path::new("ext").join(name)
}

/// `[[seam]]`: one locked seam (design §4.5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
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
    pub sections: Vec<Section>,
    /// The one module of the producer the seam is (#1959): `layer`, the
    /// face `php_engine` keeps for the stream layer alone. Its lock lives
    /// with the producer and lists that module's public items as an item
    /// lock lists a crate's, and a crate outside `consumers` that `use`s
    /// the module fails the seam's reading.
    pub module: Option<String>,
}

/// One section of a sectioned lock (#1369): a name, which the seam's
/// consumers all read, or a name with the consumers whose imports it holds
/// alone, `{ name = "kernels", consumers = ["php_jit"] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Section {
    Every(String),
    Only {
        name: String,
        consumers: Vec<String>,
    },
}

impl Section {
    pub fn name(&self) -> &str {
        match self {
            Section::Every(name) | Section::Only { name, .. } => name,
        }
    }

    /// The consumers whose imports the section holds.
    pub fn readers<'a>(&'a self, seam: &'a Seam) -> &'a [String] {
        match self {
            Section::Every(_) => &seam.consumers,
            Section::Only { consumers, .. } => consumers,
        }
    }
}

impl Seam {
    /// A seam by name alone, its lock `<name>.lock` and its producer
    /// `producer`, the two keys the file cannot leave out; nothing else.
    pub fn named(name: &str, producer: &str) -> Seam {
        Seam {
            name: name.to_owned(),
            producer: producer.to_owned(),
            lock: format!("{name}.lock"),
            ..Seam::default()
        }
    }
}

/// `[[edge]]`: one permitted (or denied) dependency edge (design §4.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
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

impl Edge {
    /// A permitted edge from `from` to `to` alone.
    pub fn between(from: &str, to: &str) -> Edge {
        Edge {
            from: from.to_owned(),
            to: OneOrMany::One(to.to_owned()),
            ..Edge::default()
        }
    }
}

/// `[[rule]]`: an `id`, a `scope` or a `tool`, one predicate, an optional
/// `adr`, and the `text` a failure prints; for an `on` rule, which never
/// denies, it is the note an allow carries. Two rules of the tracked
/// manifest (`unsafe.homes`, `generated.committed`) carry neither `scope`
/// nor `tool`, so their absence is not refused. `exempt` is the paths the
/// rule does not judge, in `scope`'s grammar: `permit` reads it for every
/// predicate it judges (#23), so `xtask/src/witness/fence.rs`, the parser
/// of the fence grammar `witness.exit-authored` denies, can carry the
/// shape it parses. `exempt_label` is the same idea for a call that names
/// an issue rather than a file: the tracker's labels under which the rule
/// does not judge the call (#341), which only a rule `permit` judges over
/// a command may carry. `where` is the same idea for the checkout a call
/// runs in ([`Where`]), and the same rules alone may carry it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub scope: OneOrMany,
    #[serde(default)]
    pub tool: OneOrMany,
    #[serde(default)]
    pub exempt: Vec<String>,
    #[serde(default)]
    pub exempt_label: Vec<String>,
    /// `where`: the checkouts the rule judges a call in (#1149).
    #[serde(default, rename = "where")]
    pub within: Where,
    #[serde(default)]
    pub adr: OneOrMany,
    pub text: String,
    #[serde(flatten)]
    pub predicate: Predicates,
}

/// `where`: the checkouts a rule judges a call in. A builder works in a
/// worktree whose branch records its ticket (`branch.<b>.ticket`, which
/// `xtask worktree add --ticket` writes and `xtask track` reads), and the
/// merge check runs in main's checkout, which records none; a rule on
/// what a builder may run is one on the first alone (#1145, #1149).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Where {
    /// Wherever the call stands: a row without `where`.
    #[default]
    Anywhere,
    /// A checkout whose branch records a ticket, read through
    /// [`crate::track`]'s record; a record that is not a number is one.
    Ticketed,
    /// A checkout where a merge, cherry-pick, revert or rebase stands
    /// that the calling session did not start (#2476), read in
    /// `permit::operation`.
    Operating,
    /// The repository's main checkout, the one whose `.git` is a
    /// directory, and never a linked worktree: where merges into `main`
    /// are made (#2751).
    Main,
    /// A checkout of the repository, the main one or a linked worktree,
    /// as `git worktree list` names them, and never a directory outside
    /// them, such as a scratch repository: read where each command of a
    /// shell line runs, and judged wherever `permit` cannot say (#2887).
    Checkout,
}

/// The predicates of harness-design.md §4.1, which cites [`Self::NAMES`]
/// and [`Self::orphaned_qualifier`] and counts neither (#2250), each
/// optional in the grammar so the file parses;
/// `Manifest::parse` then holds every rule to exactly one. `unless` and
/// `unless_pattern` qualify `deny_pattern`; `with` qualifies `deny_argv` and
/// `require_argv`, one phrase or a list of them, a word or a command and
/// its subcommand (`"git add"`); `unless_argv` qualifies `deny_argv`;
/// `deny_macros` qualifies `deny_types`; `allow_in_tests` qualifies
/// `deny_lints`; each is refused beside any other predicate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Predicates {
    pub deny_types: Option<Vec<String>>,
    /// The macros denied beside the types, each by its full path
    /// (`std::thread_local`), a wildcard refused: what a type ban cannot
    /// see, since a macro declares the state it hides (#1781). It renders to clippy's
    /// `disallowed-macros` beside `deny_types`' `disallowed-types`, and
    /// `permit` reads an invocation of one as it reads a mention of a type.
    pub deny_macros: Option<Vec<String>>,
    /// The functions and methods denied, each by its full path
    /// (`std::thread::Builder::spawn`), a wildcard refused: clippy's
    /// `disallowed-methods`, which resolves a call however it was imported
    /// (#1950). It renders into the `clippy.toml` its scope reaches, and a
    /// site the rule allows carries `#[expect(clippy::disallowed_methods)]`.
    pub deny_methods: Option<Vec<String>>,
    pub allow: Option<Vec<String>>,
    pub deny_pattern: Option<String>,
    pub unless: Option<String>,
    /// A hit of `deny_pattern` whose command, the text between the shell
    /// separators around it, also matches this is let through (#1149):
    /// the `regex` crate has no look-ahead, so "without `--subset`" is a
    /// second pattern and not a clause of the first.
    pub unless_pattern: Option<String>,
    pub deny_argv: Option<Vec<String>>,
    /// A `deny_argv` element let through where one `with` phrase runs
    /// bounded by an option, as `require_argv` reads one: `{ "--jit" =
    /// "xtask phpt --subset" }`, the tier's one-subset reading (#3052,
    /// #3068).
    pub unless_argv: Option<BTreeMap<String, String>>,
    pub require_argv: Option<String>,
    #[serde(default)]
    pub with: OneOrMany,
    pub on: Option<String>,
    pub deny: Option<bool>,
    pub deny_lints: Option<Vec<String>>,
    /// The `deny_lints` lints a test may still trip, each rendered as
    /// clippy's own `allow-*-in-tests` key ([`IN_TESTS`]) into the
    /// `clippy.toml` the scope reaches, so a `#[test]` fn and a
    /// `#[cfg(test)]` module may `unwrap`, `expect` and `panic!` while
    /// the deny holds every other line (#3078). A lint clippy has no such
    /// key for, or one the rule does not deny, is refused.
    pub allow_in_tests: Option<Vec<String>>,
    pub check: Option<String>,
    pub avoid_from: Option<AvoidFrom>,
    pub generated: Option<Generated>,
}

/// `generated`: the files the manifest renders, each from every rule whose
/// scope reaches its directory ([`crate::generated`]), so the row names
/// files and no rule (#2473). A file that is not a `clippy.toml` would
/// need its source named again; [`crate::generated::render`] says where.
/// A row that spells anything else, the map of each file to one rule
/// that #1950's generator wrote among them, is refused naming the row and
/// what it takes ([`generated_refusal`], #2513).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Generated(Vec<String>);

impl Generated {
    /// The files, relative to the root, as the row lists them.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// The refusal of a manifest `toml` would not decode, where a rule spells
/// `generated` as anything but a list: the row by its id and what it
/// takes, which `toml`'s own refusal names by the row's line alone.
fn generated_refusal(text: &str) -> Option<anyhow::Error> {
    let table: toml::Table = toml::from_str(text).ok()?;
    table.get("rule")?.as_array()?.iter().find_map(|rule| {
        let generated = rule.get("generated").filter(|g| !g.is_array())?;
        let id = rule.get("id").and_then(toml::Value::as_str).unwrap_or("");
        Some(anyhow::anyhow!(
            "rule \"{id}\" spells `generated` as a {}; the row takes a list of the files the \
             manifest renders, such as [\"ext/clippy.toml\"]",
            generated.type_str()
        ))
    })
}

/// `avoid_from`: the glossary a `vocab` rule reads for its `_Avoid_` lists,
/// whole (`avoid_from = "CONTEXT.md"`) or one heading of it
/// (`avoid_from = { file = "CONTEXT.md", section = "Harness" }`), whose
/// terms alone the scope is held to (harness-design.md §5). The harness
/// means `lock`, `region` and `registry` in the sense the glossary's
/// Harness terms give them, which its engine terms avoid, so the harness is
/// held to that section and the engine to the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AvoidFrom {
    File(String),
    Section { file: String, section: String },
}

impl AvoidFrom {
    /// The glossary file, relative to the root.
    pub fn file(&self) -> &str {
        match self {
            AvoidFrom::File(file) | AvoidFrom::Section { file, .. } => file,
        }
    }

    /// The heading the scope is held to; `None` for the whole file.
    pub fn section(&self) -> Option<&str> {
        match self {
            AvoidFrom::File(_) => None,
            AvoidFrom::Section { section, .. } => Some(section),
        }
    }
}

/// Each lint clippy exempts a test from by a key of its own
/// configuration, with that key: what `allow_in_tests` may list and
/// [`crate::generated`] renders. `unreachable`, `todo` and
/// `unimplemented` have none; `allow-print-in-tests` covers two lints and
/// is left out, so a key never allows more than the lint the row names.
pub(crate) const IN_TESTS: [(&str, &str); 6] = [
    ("clippy::unwrap_used", "allow-unwrap-in-tests"),
    ("clippy::expect_used", "allow-expect-in-tests"),
    ("clippy::panic", "allow-panic-in-tests"),
    ("clippy::dbg_macro", "allow-dbg-in-tests"),
    (
        "clippy::indexing_slicing",
        "allow-indexing-slicing-in-tests",
    ),
    ("clippy::useless_vec", "allow-useless-vec-in-tests"),
];

/// The clippy key that exempts tests from `lint`, where it has one.
pub(crate) fn in_tests_key(lint: &str) -> Option<&'static str> {
    IN_TESTS
        .iter()
        .find(|(l, _)| *l == lint)
        .map(|(_, key)| *key)
}

impl Predicates {
    /// The predicate names, in the design's order, `require_argv` after
    /// the `deny_argv` it stands beside.
    pub const NAMES: [&'static str; 12] = [
        "deny_types",
        "deny_methods",
        "allow",
        "deny_pattern",
        "deny_argv",
        "require_argv",
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
            self.deny_methods.is_some(),
            self.allow.is_some(),
            self.deny_pattern.is_some(),
            self.deny_argv.is_some(),
            self.require_argv.is_some(),
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
    /// can judge it before the call runs: `deny`, `deny_argv`, `require_argv`,
    /// `deny_pattern`, `deny_types`, `allow`, `on`. The other five read the
    /// tree after the write (harness-design.md §4.4).
    pub fn describes_a_call(&self) -> bool {
        self.deny.is_some()
            || self.deny_argv.is_some()
            || self.require_argv.is_some()
            || self.deny_pattern.is_some()
            || self.deny_types.is_some()
            || self.allow.is_some()
            || self.on.is_some()
    }

    /// A qualifier that has no predicate to qualify: `unless` without
    /// `deny_pattern`, `with` without `deny_argv` or `require_argv`,
    /// `unless_argv` without `deny_argv`, `deny_macros` without
    /// `deny_types`, `allow_in_tests` without `deny_lints`.
    pub fn orphaned_qualifier(&self) -> Option<&'static str> {
        if self.unless.is_some() && self.deny_pattern.is_none() {
            Some("unless")
        } else if self.unless_pattern.is_some() && self.deny_pattern.is_none() {
            Some("unless_pattern")
        } else if !self.with.is_empty() && self.deny_argv.is_none() && self.require_argv.is_none() {
            Some("with")
        } else if self.unless_argv.is_some() && self.deny_argv.is_none() {
            Some("unless_argv")
        } else if self.deny_macros.is_some() && self.deny_types.is_none() {
            Some("deny_macros")
        } else if self.allow_in_tests.is_some() && self.deny_lints.is_none() {
            Some("allow_in_tests")
        } else {
            None
        }
    }
}

impl Rule {
    /// Whether the rule reads the reply at the Stop rather than a call:
    /// `tool = "Stop"`, the hook event where it is read, so `permit` never
    /// judges it and `done --hook` does (#32).
    pub(crate) fn at_the_stop(&self) -> bool {
        self.tool.iter().any(|t| t == "Stop")
    }

    /// A rule by id alone over `scope`, carrying `deny = true`, the one
    /// predicate with nothing to name, so the row passes the grammar
    /// `parse` holds a rule to; its text is its id.
    pub fn named(id: &str, scope: &str) -> Rule {
        Rule {
            id: id.to_owned(),
            scope: OneOrMany::One(scope.to_owned()),
            text: id.to_owned(),
            predicate: Predicates {
                deny: Some(true),
                ..Predicates::default()
            },
            ..Rule::default()
        }
    }

    /// Whether the rule's `scope` covers `path`: the manifest's `scope` key
    /// is one grammar, and every sensor that reads it reads it here
    /// (harness-design.md §4.1). A rule without a scope covers nothing on
    /// its own; what such a rule judges is its predicate's.
    pub fn covers(&self, path: &Path) -> bool {
        any_matches(self.scope.iter(), path)
    }

    /// Whether the rule's `exempt` covers `path`, under the grammar
    /// `covers` reads: a call at an exempt path is not judged by the rule.
    /// A rule without an exempt exempts nothing, and so does an exempt for
    /// a call without a file, a `Bash` call: an exempt names files. The
    /// sensor after a `Bash` call reads `harness.self`'s exempt over the
    /// files the call wrote, in this grammar (`permit::after`, #626).
    pub fn exempts(&self, path: &Path) -> bool {
        any_matches(self.exempt.iter().map(String::as_str), path)
    }
}

/// The one reading of a list of scope patterns against a path, which
/// `scope` and `exempt` share.
fn any_matches<'a>(mut patterns: impl Iterator<Item = &'a str>, path: &Path) -> bool {
    patterns.any(|pattern| scope_matches(pattern, path))
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

impl Manifest {
    /// A manifest of these `[[crate]]` rows and nothing else: the `[oracle]`
    /// row names `oracle.toml`, every ceiling is one, and the pin and the
    /// root are empty as `parse` leaves them, for a test to fill from the
    /// tracked manifest when the rows are resolved against the pinned tree.
    pub fn of(crates: impl IntoIterator<Item = Crate>) -> Manifest {
        Manifest {
            oracle: PinRow {
                pin: "oracle.toml".to_owned(),
            },
            budget: Budget::flat(1),
            crates: crates.into_iter().collect(),
            ..Manifest::default()
        }
    }

    /// A manifest of one crate row by name alone ([`Crate::named`]).
    pub fn named(krate: &str) -> Manifest {
        Manifest::of([Crate::named(krate)])
    }

    /// Load `seams.toml` from the workspace root, then the pin it names.
    pub fn load(root: &Path) -> Result<Manifest> {
        let path = root.join(FILE);
        let text = crate::read(&path)?;
        let mut manifest = Manifest::parse(&text).with_context(|| format!("{}", path.display()))?;
        manifest.pin = Pin::load(&root.join(&manifest.oracle.pin))?;
        manifest.root = root.to_path_buf();
        Ok(manifest)
    }

    /// Every oracle variant a `[[crate]]` row names and the pin carries,
    /// by name, each as the pin its crates run under on this machine
    /// ([`Pin::variant`]): what a lane makes its
    /// [`crate::oracle::Oracles`] from, so each variant's digest is
    /// asserted once per run. A name the pin does not carry is left out,
    /// and [`crate::oracle::Oracles::of`] refuses it for the crate whose
    /// row names it, a failing reading of that crate and no other.
    pub fn variants(&self) -> BTreeMap<String, Pin> {
        self.crates
            .iter()
            .filter_map(|k| k.oracle.as_deref())
            .filter_map(|name| Some((name.to_owned(), self.pin.variant(name).ok()?)))
            .collect()
    }

    /// The oracle variant the row of crate `name` names; none where the
    /// row names none or there is no such row, which run under the main
    /// oracle.
    pub fn variant_of(&self, name: &str) -> Option<&str> {
        self.crate_named(name).and_then(|k| k.oracle.as_deref())
    }

    /// Parse the manifest text alone and hold it to the grammar. The pin is
    /// left empty; `load` fills it.
    pub fn parse(text: &str) -> Result<Manifest> {
        let manifest: Manifest = toml::from_str(text)
            .map_err(|err| generated_refusal(text).unwrap_or_else(|| err.into()))?;
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
            if rule.predicate.with.iter().any(|w| w.trim().is_empty()) {
                bail!(
                    "rule \"{}\" has an empty `with` phrase; a phrase is a word or a command and its subcommand",
                    rule.id
                );
            }
            if let Some(element) = rule
                .predicate
                .deny_argv
                .iter()
                .flatten()
                .find(|d| crate::permit::Element::parse(d).is_none())
            {
                bail!(
                    "rule \"{}\" has the `deny_argv` element {element:?}; an element is an option (`-n`), an option and the name its value sets (`-c core.hooksPath`), or a subcommand (`close`)",
                    rule.id
                );
            }
            if rule.within != Where::Anywhere && !rule.predicate.describes_a_call() {
                bail!(
                    "rule \"{}\" has `where` beside `{}`; the checkout is the one a call runs in, so `where` qualifies a rule permit judges at the call",
                    rule.id,
                    named[0]
                );
            }
            if !rule.exempt_label.is_empty() && !rule.predicate.describes_a_call() {
                bail!(
                    "rule \"{}\" has `exempt_label` beside `{}`; a label is read off the issue a command names, so `exempt_label` qualifies a rule permit judges at the call",
                    rule.id,
                    named[0]
                );
            }
            for (kind, paths) in [
                ("macros", &rule.predicate.deny_macros),
                ("methods", &rule.predicate.deny_methods),
            ] {
                if let Some(wildcard) = paths.iter().flatten().find(|m| m.ends_with("::*")) {
                    bail!(
                        "rule \"{}\" denies the {kind} {wildcard}; each is denied by its full path, since clippy matches paths and no table of a module's {kind} expands a wildcard",
                        rule.id
                    );
                }
            }
            if let Some(qualifier) = rule.predicate.orphaned_qualifier() {
                bail!(
                    "rule \"{}\" has `{qualifier}` beside `{}`; `unless` and `unless_pattern` qualify deny_pattern, `with` qualifies deny_argv or require_argv, `unless_argv` qualifies deny_argv, `deny_macros` qualifies deny_types and `allow_in_tests` qualifies deny_lints",
                    rule.id,
                    named[0]
                );
            }
            let p = &rule.predicate;
            if let Some(lint) = p.allow_in_tests.iter().flatten().find(|lint| {
                in_tests_key(lint).is_none() || !p.deny_lints.iter().flatten().any(|d| d == *lint)
            }) {
                bail!(
                    "rule \"{}\" allows {lint} in tests; `allow_in_tests` lists lints the rule denies that clippy has an allow-*-in-tests key for: {}",
                    rule.id,
                    IN_TESTS.map(|(lint, _)| lint).join(", ")
                );
            }
            if let Some((element, excuse)) =
                p.unless_argv.iter().flatten().find(|(element, excuse)| {
                    !p.deny_argv.iter().flatten().any(|d| d == *element)
                        || !excuse.rsplit_once(' ').is_some_and(|(phrase, option)| {
                            option.starts_with('-') && p.with.iter().any(|w| w == phrase)
                        })
                })
            {
                bail!(
                    "rule \"{}\" excuses {element:?} by {excuse:?}; `unless_argv` maps a `deny_argv` element to one `with` phrase and the option that bounds it (`\"--jit\" = \"xtask phpt --subset\"`)",
                    rule.id
                );
            }
        }
        manifest.parity.hold(&manifest.crates)?;
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

    /// The ceilings on a seam's lock width: `[budget] api_items` and
    /// `api_private_items` for the api seam (design §4.4, §4.5); no other
    /// lock has one.
    pub fn lock_ceiling(&self, seam: &Seam) -> Option<LockCeiling> {
        (seam.name == "api").then_some(LockCeiling {
            items: self.budget.api_items,
            private: self.budget.api_private_items,
        })
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

    /// The rows `only` names, in the manifest's order, or every row where
    /// it names none: an instrument's `--crate`, refused for a name no row
    /// carries.
    pub(crate) fn crates_named(&self, only: &[String]) -> Result<Vec<&Crate>> {
        if let Some(name) = only.iter().find(|n| self.crate_named(n).is_none()) {
            bail!("the manifest has no crate named {name:?}");
        }
        Ok(self
            .crates
            .iter()
            .filter(|k| only.is_empty() || only.contains(&k.name))
            .collect())
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
    /// `php_ext`), else the crate `[lock_home]` names for a lock whose
    /// seam has no producing crate at all (`ops.lock`, rendered from
    /// `ops.toml`), else the producer of a seam that is one module of it
    /// (`layer.lock`, #1959). `None` for a lock nothing names.
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
            .or_else(|| self.lock_homes.get(lock).cloned())
            .or_else(|| {
                self.seams
                    .iter()
                    .find(|s| s.lock == lock && s.module.is_some())
                    .map(|s| s.producer.clone())
            })
    }

    /// The `on = "pub"` rule named for `lock`, `<lock>-line`: the note at
    /// the call that a new public item needs its lock line in the same
    /// commit (`api.lock-line` for `api.lock`, `xtask.lock-line` for
    /// `xtask.lock`). The lock's reading carries the rule's id, so the note
    /// and the failing diff read as one rule. The name and not the crate
    /// row is the key, since one row lists several locks and the rule is
    /// one lock's. `None` for a lock with no such rule.
    pub fn lock_line_rule(&self, lock: &str) -> Option<&Rule> {
        let id = format!("{lock}-line");
        self.rules
            .iter()
            .find(|r| r.id == id && r.predicate.on.as_deref() == Some("pub"))
    }

    /// Every file a `generated` rule names, in the manifest's order.
    pub fn generated_files(&self) -> Vec<&str> {
        self.rules
            .iter()
            .filter_map(|r| r.predicate.generated.as_ref())
            .flat_map(Generated::files)
            .collect()
    }

    /// The path a reading's `tree` is hashed over, relative to the root:
    /// the one mapping from a scope to its sources, so what an instrument
    /// stamps and what `done` hashes agree (harness-design.md §4.2, §4.5
    /// condition 1). One arm per scope kind a reading carries:
    ///
    /// - `"workspace"`: the whole tree.
    /// - [`SCRIPTS`]: the `scripts/` directory, which `[budget] scripts`
    ///   holds.
    /// - a crate: its sources ([`Crate::path`]).
    /// - a subset: the crate whose subset it is, under the crate's name
    ///   and beside a tier lane, a control of the runner or a comparison
    ///   alike (`ext/pcre@force`, `ext/pcre@control`, the grammar
    ///   [`Scope`] reads), so `done` finds the
    ///   engine's reading under the crate; a directory of the pinned
    ///   corpus no crate owns (`Zend/tests`) is the whole tree, since no
    ///   crate's sources are what it measures and `done` never asks for it.
    /// - a lock: the seam's name maps to the directory the lock lives in,
    ///   the crate [`Manifest::lock_home`] names, and the whole tree for a
    ///   lock no row names.
    /// - a generated file the manifest names (`ext/clippy.toml`): its
    ///   directory, since `budget` holds the file and every clippy
    ///   configuration below it that would stand nearer to a crate (#2475).
    ///
    /// `None` for a name none of these know, or a suffix outside that
    /// grammar, which the reading's constructor refuses by name rather than
    /// stamping over a guess.
    pub fn scope_path(&self, scope: &str) -> Option<PathBuf> {
        if scope == "workspace" {
            return Some(PathBuf::from("."));
        }
        if scope == SCRIPTS {
            return Some(PathBuf::from(SCRIPTS));
        }
        let Ok(Scope { subset: name, .. }) = scope.parse() else {
            return None;
        };
        let name = name.as_str();
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
        if self.generated_files().contains(&name) {
            let dir = name.rsplit_once('/').map_or(".", |(dir, _)| dir);
            return Some(PathBuf::from(dir));
        }
        if let Ok(corpus) = self.pin.source.tree()
            && corpus.join(name).is_dir()
        {
            return Some(PathBuf::from("."));
        }
        None
    }
}

#[cfg(test)]
mod tests;
