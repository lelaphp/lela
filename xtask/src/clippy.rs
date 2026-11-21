//! `xtask clippy` (harness-design.md §4.5 condition 1, §6): `cargo clippy`
//! on every target of each crate under every feature it declares
//! ([`Targets::All`]), every warning a failure, as a reading per
//! crate. What this instrument adds is the reading `done` reads,
//! stamped with the crate's tree, so a crate changed since its last clean
//! run is stale and not clean. One reading per crate that exists;
//! `--crate` narrows it to the ones named.
//!
//! One `cargo clippy --keep-going --all-targets --all-features` checks
//! every crate at once, cargo checking crates side by side.
//! Warnings are not denied on the command line, so a lint stays a warning
//! and the crates built on the one that carries it are still checked; each
//! warning is counted against the crate it is of, by its package
//! ([`batch::Run`]), and a crate fails on any. The verdict over a crate is the one `-D warnings`
//! gave it, and one crate's lint no longer blocks the crates above it.
//!
//! A crate whose library or binary sources name `debug_assertions` is
//! linted a second time, in the release profile on its library and
//! binaries, as a second reading, `<crate>/release` ([`Profile`]):
//! the dev run lints one side of the cfg split alone, and the bench lane
//! and the shipped binaries build the other. The crates are found by a
//! scan of their sources ([`splits`]), not a list, so the cost is the
//! split's and not the workspace's. `release-da` would not serve: it
//! keeps debug assertions on, the dev profile's side of the split.
//!
//! The reading is also the sensor of the manifest's `deny_lints` rules
//! (`lints.no-panic`, `unsafe.one-op`): a clean run says nothing of
//! a lint the crate never turned on, so each source a rule's scope covers
//! must sit under a module that denies, or forbids, every lint the rule
//! lists, read off the inner attributes of the source and of each module
//! file above it to the crate root, and at the root off the crate's
//! Cargo `[lints]`, the workspace's where it inherits them. A crate a
//! rule reaches carries the rule's id on its reading, and fails naming
//! the sources and the lints no module above them denies.
//!
//! It is the sensor of `tests.no-silent-return` too, the rule whose
//! `check` names [`returns::CHECK`]: a crate the rule reaches fails naming
//! each return at a `#[test]` fn's own level ([`returns`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use syn::punctuated::Punctuated;

use crate::batch::{Run, Targets, tail};
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status, Store, joined};
use crate::rust;
use crate::sources::{self, Standing};

mod returns;

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "clippy";

/// Run clippy over every crate that exists, or over `only`; one reading
/// each, and a second, in release, for each crate that [`splits`].
pub fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    let mut crates = manifest.crates_named(only)?;
    crates.retain(|k| sources::exists(root, k));
    if crates.is_empty() {
        return Ok(Vec::new());
    }
    // The key each reading carries is the tree's before cargo built it,
    // graph and all, so a file written while the crates are measured
    // stales the reading.
    let snapshot = Snapshot::disk(root)?.with_graph()?;
    let _seat = crate::seat::Seats::of_env().take(INSTRUMENT);
    let mut release = Vec::new();
    for krate in &crates {
        if splits(root, krate)? {
            release.push(*krate);
        }
    }
    let mut readings = Vec::new();
    for (profile, linted) in [(Profile::Dev, crates), (Profile::Release, release)] {
        if linted.is_empty() {
            continue;
        }
        let checked = Run::of(root, profile.args(), &linted, profile.targets())?;
        for krate in linted {
            let denied = match profile {
                Profile::Dev => Denied::of(root, manifest, krate)?,
                Profile::Release => Denied::default(),
            };
            readings.push(reading(&snapshot, krate, profile, &checked, &denied)?);
        }
    }
    Ok(readings)
}

/// The profile a reading lints under, the side of the `debug_assertions`
/// split it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// Every target under every feature in the dev profile, the reading
    /// over the crate's name.
    Dev,
    /// The library and binaries, cargo's default targets (`--lib
    /// --bins`), under every feature in the release profile, the reading
    /// over `<crate>/release`, for a crate that [`splits`].
    Release,
}

impl Profile {
    /// The profiles `krate` is linted under: dev, and release where its
    /// sources name `debug_assertions`.
    pub(crate) fn of(root: &Path, krate: &Crate) -> Result<Vec<Profile>> {
        Ok(if splits(root, krate)? {
            vec![Profile::Dev, Profile::Release]
        } else {
            vec![Profile::Dev]
        })
    }

    /// The scope of the reading over the crate named `name`.
    pub(crate) fn scope(self, name: &str) -> String {
        match self {
            Profile::Dev => name.to_owned(),
            Profile::Release => format!("{name}/release"),
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Profile::Dev => &["clippy", "--keep-going"],
            Profile::Release => &["clippy", "--keep-going", "--release"],
        }
    }

    fn targets(self) -> Targets {
        match self {
            Profile::Dev => Targets::All,
            Profile::Release => Targets::Shipped,
        }
    }
}

/// Whether a source of `krate`'s library or binaries, under `src/` and
/// outside its test modules, names `debug_assertions` in its code: in a
/// `cfg`, a `cfg_attr` or `cfg!`, and not in a comment or a string.
fn splits(root: &Path, krate: &Crate) -> Result<bool> {
    let src = krate.path().join("src");
    for source in sources::rust(root, krate)? {
        if !Path::new(&source).starts_with(&src) {
            continue;
        }
        let text =
            fs::read_to_string(root.join(&source)).with_context(|| format!("reading {source}"))?;
        if text.contains("debug_assertions") && names_debug_assertions(&text) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `text`'s code, read through the one scanner of what is not
/// code ([`rust::without_comments_or_literals`]), names `debug_assertions`.
fn names_debug_assertions(text: &str) -> bool {
    let code = rust::without_comments_or_literals(text);
    let ident = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    code.match_indices("debug_assertions").any(|(at, word)| {
        !ident(code[..at].chars().next_back()) && !ident(code[at + word.len()..].chars().next())
    })
}

/// `--staged`: the crates with a staged change, pre-commit's touched
/// crates (harness-design.md §6), less those each of whose readings
/// stands for what is staged; no readings when none is left.
pub fn run_staged(root: &Path, manifest: &Manifest, store: &Store) -> Result<Vec<Reading>> {
    let touched = sources::staged_crates(root, manifest)?;
    if touched.is_empty() {
        println!("{INSTRUMENT}: no crate has a staged change");
        return Ok(Vec::new());
    }
    let mut scopes = Vec::new();
    for krate in touched.iter().filter_map(|name| manifest.crate_named(name)) {
        for profile in Profile::of(root, krate)? {
            scopes.push((profile.scope(&krate.name), krate.path()));
        }
    }
    let standing = Standing::for_commit_over(root, manifest, store, INSTRUMENT, &scopes)?;
    let measured: Vec<String> = touched
        .into_iter()
        .filter(|name| {
            let its = [name.clone(), Profile::Release.scope(name)];
            scopes
                .iter()
                .any(|(scope, _)| its.contains(scope) && standing.get(scope).is_none())
        })
        .collect();
    if measured.is_empty() {
        return Ok(Vec::new());
    }
    run(root, manifest, &measured)
}

/// One crate's reading: pass when every target was checked and clippy
/// said nothing of the crate and every `deny_lints` rule reaching it is
/// met, fail with what was not otherwise.
fn reading(
    snapshot: &Snapshot,
    krate: &Crate,
    profile: Profile,
    run: &Run,
    denied: &Denied,
) -> Result<Reading> {
    let scope = profile.scope(&krate.name);
    let mut reading = Reading::stamped_over(INSTRUMENT, &scope, snapshot, &krate.path())?;
    reading.rule = denied.rules.join(", ");
    reading.adr = denied.adrs.join(", ");
    clippy_said(&mut reading, krate, run)?;
    if !denied.unmet.is_empty() {
        reading.status = Status::Fail;
        reading.message = format!(
            "{}: {}\n{}",
            reading.scope,
            denied.unmet.join("; "),
            reading.message
        );
    }
    Ok(reading)
}

/// What clippy said of the crate, as the reading's status and message.
fn clippy_said(reading: &mut Reading, krate: &Crate, run: &Run) -> Result<()> {
    let name = reading.scope.clone();
    let Some(checked) = run.crates.get(&krate.name) else {
        reading.status = Status::Fail;
        reading.message = format!("{name}: clippy did not check it\n{}", tail(&run.stderr, 40));
        return Ok(());
    };
    let warnings = checked.count("warning");
    let errors = checked.count("error");
    if checked.whole && warnings == 0 && errors == 0 {
        reading.status = Status::Pass;
        reading.message = format!("{name}: clippy clean, warnings denied");
        return Ok(());
    }
    reading.count("warnings", warnings as u64)?;
    reading.count("errors", errors as u64)?;
    reading.status = Status::Fail;
    let said = checked.rendered();
    reading.message = if said.is_empty() {
        format!(
            "{name}: clippy did not check it: a crate it is built from did not compile\n{}",
            tail(&run.stderr, 40)
        )
    } else {
        format!("{name}: clippy is not clean\n{}", tail(&said, 40))
    };
    Ok(())
}

/// What the manifest's `deny_lints` rules, and the rule [`returns`]
/// senses, ask of one crate: the rules whose scope covers a source of it,
/// their ADRs, and one line per rule a covered source is not held to.
#[derive(Debug, Default)]
struct Denied {
    rules: Vec<String>,
    adrs: Vec<String>,
    unmet: Vec<String>,
}

impl Denied {
    fn of(root: &Path, manifest: &Manifest, krate: &Crate) -> Result<Denied> {
        let src = krate.path().join("src");
        let sources: Vec<PathBuf> = sources::rust(root, krate)?
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.starts_with(&src))
            .collect();
        let cargo = cargo_lints(root, krate)?;
        let mut files = Files::default();
        let mut out = Denied::default();
        for rule in &manifest.rules {
            let Some(lints) = &rule.predicate.deny_lints else {
                continue;
            };
            let covered: Vec<&PathBuf> = sources
                .iter()
                .filter(|p| rule.covers(p) && !rule.exempts(p))
                .collect();
            if covered.is_empty() {
                continue;
            }
            let mut lacking = BTreeSet::new();
            let mut not_held = Vec::new();
            for source in covered {
                let missing: Vec<String> = match files.levels(root, &src, source, &cargo) {
                    Ok(levels) => lints
                        .iter()
                        .filter(|l| !levels.get(*l).is_some_and(|level| level.denies()))
                        .cloned()
                        .collect(),
                    Err(e) => vec![format!("{e:#}")],
                };
                if !missing.is_empty() {
                    lacking.extend(missing);
                    not_held.push(source.display().to_string());
                }
            }
            out.rules.push(rule.id.clone());
            out.adrs.extend(rule.adr.iter().map(str::to_owned));
            if let Some(first) = not_held.first() {
                let more = match not_held.len() {
                    1 => String::new(),
                    n => format!(" and {} more", n - 1),
                };
                let cited = rule.adr.adr_cited();
                out.unmet.push(format!(
                    "{first}{more}: {} not denied at the source, a module above it or the crate root (seams.toml rule {}: {}{}{cited})",
                    lacking.into_iter().collect::<Vec<_>>().join(", "),
                    rule.id,
                    rule.text,
                    if cited.is_empty() { "" } else { " " },
                ));
            }
        }
        if let Some(rule) = manifest.rule_checked_by(returns::CHECK)
            && let Some(sites) = returns::sites(root, krate, rule)?
        {
            out.rules.push(rule.id.clone());
            out.adrs.extend(rule.adr.iter().map(str::to_owned));
            if !sites.is_empty() {
                out.unmet.push(format!(
                    "{}: a return at a #[test] fn's own level (seams.toml rule {}: {})",
                    sites.join(", "),
                    rule.id,
                    rule.said()
                ));
            }
        }
        out.adrs.sort();
        out.adrs.dedup();
        Ok(out)
    }
}

/// A lint's level as an attribute or a `[lints]` table sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Deny,
    /// A denial no module below can lift.
    Forbid,
    /// `allow`, `warn` or `expect`: a module below lifted the denial.
    Lifted,
}

impl Level {
    fn of(name: &str) -> Option<Level> {
        match name {
            "deny" => Some(Level::Deny),
            "forbid" => Some(Level::Forbid),
            "allow" | "warn" | "expect" => Some(Level::Lifted),
            _ => None,
        }
    }

    fn denies(self) -> bool {
        self != Level::Lifted
    }
}

/// Lints and their levels, a later setting standing over an earlier one
/// as rustc's do, a `forbid` over all.
type Levels = BTreeMap<String, Level>;

fn apply(levels: &mut Levels, set: &Levels) {
    for (lint, level) in set {
        if levels.get(lint) != Some(&Level::Forbid) {
            levels.insert(lint.clone(), *level);
        }
    }
}

/// The files whose lint attributes reach `source`, from the crate root
/// down to the source itself: `lib.rs`, or `main.rs` in a crate without
/// one, then each module on the path at `<dir>.rs` or `<dir>/mod.rs` as
/// rustc finds it; `main.rs` beside a `lib.rs`, and a file under
/// `src/bin/`, are roots of their own.
fn modules(root: &Path, src: &Path, source: &Path) -> Vec<PathBuf> {
    let rel = source.strip_prefix(src).unwrap_or(source);
    let alone = rel == Path::new("main.rs") || rel.starts_with("bin");
    let mut out = Vec::new();
    if !alone {
        let lib = src.join("lib.rs");
        out.push(if root.join(&lib).is_file() {
            lib
        } else {
            src.join("main.rs")
        });
        let mut dir = src.to_path_buf();
        for part in rel.parent().into_iter().flat_map(Path::components) {
            dir.push(part);
            let file = dir.with_extension("rs");
            out.push(if root.join(&file).is_file() {
                file
            } else {
                dir.join("mod.rs")
            });
        }
    }
    if out.last().map(PathBuf::as_path) != Some(source) {
        out.push(source.to_path_buf());
    }
    out
}

/// One module file's lint attributes: its own inner ones, and the outer
/// ones on each `mod` item it declares, by the module's name.
#[derive(Debug, Default)]
struct Module {
    inner: Levels,
    children: BTreeMap<String, Levels>,
}

/// Each module file's attributes, parsed once.
#[derive(Default)]
struct Files(BTreeMap<PathBuf, Module>);

impl Files {
    /// The levels the lints stand at in `source`: the crate's Cargo
    /// `[lints]`, then each module from the root down, the attributes on
    /// its `mod` item in the file above and its own inner ones.
    fn levels(&mut self, root: &Path, src: &Path, source: &Path, cargo: &Levels) -> Result<Levels> {
        let mut levels = cargo.clone();
        let chain = modules(root, src, source);
        for (at, file) in chain.iter().enumerate() {
            if at > 0 {
                let name = match file.file_stem().and_then(|s| s.to_str()) {
                    Some("mod") => file.parent().and_then(Path::file_name),
                    _ => file.file_stem(),
                };
                let declared = name.and_then(|n| n.to_str()).and_then(|n| {
                    self.module(root, &chain[at - 1])
                        .ok()?
                        .children
                        .get(n)
                        .cloned()
                });
                apply(&mut levels, &declared.unwrap_or_default());
            }
            let inner = self.module(root, file)?.inner.clone();
            apply(&mut levels, &inner);
        }
        Ok(levels)
    }

    /// A file's attributes; none for a file not there.
    fn module(&mut self, root: &Path, file: &Path) -> Result<&Module> {
        if !self.0.contains_key(file) {
            let module = match crate::read_if(&root.join(file))? {
                Some(text) => {
                    let parsed = crate::rust::parsed(&text, file)?;
                    let mut module = Module {
                        inner: levels_of(&parsed.attrs, file)?,
                        children: BTreeMap::new(),
                    };
                    for item in &parsed.items {
                        if let syn::Item::Mod(declared) = item {
                            module.children.insert(
                                declared.ident.to_string(),
                                levels_of(&declared.attrs, file)?,
                            );
                        }
                    }
                    module
                }
                None => Module::default(),
            };
            self.0.insert(file.to_path_buf(), module);
        }
        Ok(&self.0[file])
    }
}

/// The lints a list of attributes sets a level for, in order, as written
/// (`clippy::unwrap_used`).
fn levels_of(attrs: &[syn::Attribute], file: &Path) -> Result<Levels> {
    let mut levels = Levels::new();
    for attr in attrs {
        let Some(level) = attr
            .path()
            .get_ident()
            .and_then(|i| Level::of(&i.to_string()))
        else {
            continue;
        };
        let metas = attr
            .parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
            .with_context(|| format!("reading {}'s lints", file.display()))?;
        let named = metas.iter().filter_map(|meta| match meta {
            syn::Meta::Path(path) => Some(path_text(path)),
            _ => None,
        });
        let mut set = Levels::new();
        for lint in named {
            set.insert(lint, level);
        }
        apply(&mut levels, &set);
    }
    Ok(levels)
}

fn path_text(path: &syn::Path) -> String {
    joined(path.segments.iter().map(|s| s.ident.to_string()), "::")
}

/// The levels the crate's Cargo `[lints]` set, the workspace's
/// `[workspace.lints]` where the crate inherits them: `clippy::<name>`
/// for a linter's table, the name alone for `rust`'s.
fn cargo_lints(root: &Path, krate: &Crate) -> Result<Levels> {
    let own: toml::Table = crate::read_toml(&root.join(krate.path()).join("Cargo.toml"))?;
    let Some(lints) = own.get("lints").and_then(toml::Value::as_table) else {
        return Ok(Levels::new());
    };
    let workspace;
    let lints = if lints.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
        workspace = crate::read_toml::<toml::Table>(&root.join("Cargo.toml"))?;
        match workspace
            .get("workspace")
            .and_then(|w| w.get("lints"))
            .and_then(toml::Value::as_table)
        {
            Some(inherited) => inherited,
            None => return Ok(Levels::new()),
        }
    } else {
        lints
    };
    let mut out = Levels::new();
    for (linter, table) in lints.iter().filter_map(|(t, v)| Some((t, v.as_table()?))) {
        for (name, level) in table {
            let level = level
                .as_str()
                .or_else(|| level.get("level").and_then(toml::Value::as_str))
                .and_then(Level::of);
            if let Some(level) = level {
                out.insert(
                    if linter == "rust" {
                        name.clone()
                    } else {
                        format!("{linter}::{name}")
                    },
                    level,
                );
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::names_debug_assertions;

    #[test]
    fn the_cfg_is_named_by_its_token_and_not_by_a_comment_or_a_string() {
        assert!(names_debug_assertions(
            "#[cfg(debug_assertions)]\nfn f() {}"
        ));
        assert!(names_debug_assertions(
            "fn f() -> bool { cfg!(debug_assertions) }"
        ));
        assert!(names_debug_assertions(
            "#[cfg_attr(debug_assertions, track_caller)]\nfn f() {}"
        ));
        assert!(!names_debug_assertions(
            "// debug_assertions\n/// debug_assertions\nfn f() {}"
        ));
        assert!(!names_debug_assertions(
            "const S: &str = \"debug_assertions\";"
        ));
    }
}
