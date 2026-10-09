//! `xtask clippy` (harness-design.md §4.5 condition 1, §6): `cargo clippy`
//! on every target of each crate under every feature it declares
//! ([`Targets::All`], #140), every warning a failure, as a reading per
//! crate. What this instrument adds is the reading `done` reads,
//! stamped with the crate's tree, so a crate changed since its last clean
//! run is stale and not clean. One reading per crate that exists;
//! `--crate` narrows it to the ones named.
//!
//! One `cargo clippy --keep-going --all-targets --all-features` checks
//! every crate at once, cargo checking crates side by side (#2165).
//! Warnings are not denied on the command line, so a lint stays a warning
//! and the crates built on the one that carries it are still checked; each
//! warning is counted against the crate it is of, by its package
//! ([`batch::Run`]), and a crate fails on any. The verdict over a crate is the one `-D warnings`
//! gave it, and one crate's lint no longer blocks the crates above it.
//!
//! The reading is also the sensor of the manifest's `deny_lints` rules
//! (`lints.no-panic`, `unsafe.one-op`; #1917): a clean run says nothing of
//! a lint the crate never turned on, so each source a rule's scope covers
//! must sit under a module that denies, or forbids, every lint the rule
//! lists, read off the inner attributes of the source and of each module
//! file above it to the crate root, and at the root off the crate's
//! Cargo `[lints]`, the workspace's where it inherits them. A crate a
//! rule reaches carries the rule's id on its reading, and fails naming
//! the sources and the lints no module above them denies.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use syn::punctuated::Punctuated;

use crate::batch::{Run, Targets, tail};
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status, Store};
use crate::sources;

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "clippy";

/// Run clippy over every crate that exists, or over `only`; one reading
/// each.
pub fn run(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    for name in only {
        if manifest.crate_named(name).is_none() {
            bail!("the manifest has no crate named {name:?}");
        }
    }
    // The key each reading carries is the tree's before cargo built it,
    // so a file written while the crates are measured stales the reading.
    let snapshot = Snapshot::disk(root)?;
    let crates: Vec<&Crate> = manifest
        .crates
        .iter()
        .filter(|k| only.is_empty() || only.iter().any(|n| n == &k.name))
        .filter(|k| sources::exists(root, k))
        .collect();
    if crates.is_empty() {
        return Ok(Vec::new());
    }
    let checked = Run::of(root, &["clippy", "--keep-going"], &crates, Targets::All)?;
    crates
        .iter()
        .map(|krate| {
            let denied = Denied::of(root, manifest, krate)?;
            reading(&snapshot, krate, &checked, &denied)
        })
        .collect()
}

/// `--staged`: the crates with a staged change, pre-commit's touched
/// crates (harness-design.md §6), less those whose reading stands for
/// what is staged (#1143); no readings when none is left.
pub fn run_staged(root: &Path, manifest: &Manifest, store: &Store) -> Result<Vec<Reading>> {
    let measured = sources::for_commit(root, manifest, store, INSTRUMENT)?;
    if measured.is_empty() {
        return Ok(Vec::new());
    }
    run(root, manifest, &measured)
}

/// The command line's entry: the crates named, or with `--staged` the
/// ones with a staged change, and every reading written.
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    crates: &[String],
    staged: bool,
) -> Result<()> {
    let readings = if staged {
        run_staged(root, manifest, store)?
    } else {
        run(root, manifest, crates)?
    };
    crate::record(store, &readings)
}

/// One crate's reading: pass when every target was checked and clippy
/// said nothing of the crate, fail with what it said otherwise.
fn reading(snapshot: &Snapshot, krate: &Crate, run: &Run, denied: &Denied) -> Result<Reading> {
    let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
    reading.rule = denied.rules.join(", ");
    reading.adr = denied.adrs.join(", ");
    let name = &krate.name;
    if !denied.unmet.is_empty() {
        reading.status = Status::Fail;
        reading.message = format!("{name}: {}", denied.unmet.join("; "));
        return Ok(reading);
    }
    let Some(checked) = run.crates.get(name) else {
        reading.status = Status::Fail;
        reading.message = format!("{name}: clippy did not check it\n{}", tail(&run.stderr, 40));
        return Ok(reading);
    };
    let warnings = checked.count("warning");
    let errors = checked.count("error");
    if checked.whole && warnings == 0 && errors == 0 {
        reading.status = Status::Pass;
        reading.message = format!("{name}: clippy clean, warnings denied");
        return Ok(reading);
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
    Ok(reading)
}

/// What the manifest's `deny_lints` rules ask of one crate: the rules
/// whose scope covers a source of it, their ADRs, and one line per rule
/// a covered source is under no denial of.
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
            let covered = sources
                .iter()
                .filter(|p| rule.covers(p) && !rule.exempts(p));
            let mut lacking = BTreeSet::new();
            let mut under_none = Vec::new();
            let mut reached = false;
            for source in covered {
                reached = true;
                let mut held = cargo.clone();
                for module in modules(root, &src, source) {
                    if lints.iter().all(|l| held.contains(l)) {
                        break;
                    }
                    held.extend(files.denied(root, &module)?.iter().cloned());
                }
                let missing: Vec<&String> = lints.iter().filter(|l| !held.contains(*l)).collect();
                if !missing.is_empty() {
                    lacking.extend(missing);
                    under_none.push(source.display().to_string());
                }
            }
            if !reached {
                continue;
            }
            out.rules.push(rule.id.clone());
            out.adrs.extend(rule.adr.iter().map(str::to_owned));
            if let Some(first) = under_none.first() {
                let more = match under_none.len() {
                    1 => String::new(),
                    n => format!(" and {} more", n - 1),
                };
                let cited = rule.adr.adr_cited();
                out.unmet.push(format!(
                    "{first}{more}: no #![deny] of {} at the source, a module above it or the crate root (seams.toml rule {}: {}{}{cited})",
                    lacking.into_iter().cloned().collect::<Vec<_>>().join(", "),
                    rule.id,
                    rule.text,
                    if cited.is_empty() { "" } else { " " },
                ));
            }
        }
        out.adrs.dedup();
        Ok(out)
    }
}

/// The files whose inner attributes reach `source`, from the crate root
/// down to the source itself: `lib.rs` (or `main.rs` for itself, and a
/// file of `src/bin/` for itself), then each module on the path, at
/// `<dir>.rs` or `<dir>/mod.rs`, as rustc finds it.
fn modules(root: &Path, src: &Path, source: &Path) -> Vec<PathBuf> {
    let rel = source.strip_prefix(src).unwrap_or(source);
    let alone = rel == Path::new("main.rs") || rel.parent() == Some(Path::new("bin"));
    let mut out = Vec::new();
    if !alone {
        out.push(src.join("lib.rs"));
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

/// Each module file's denied lints, parsed once.
#[derive(Default)]
struct Files(BTreeMap<PathBuf, BTreeSet<String>>);

impl Files {
    /// The lints a file's inner `#![deny(..)]` and `#![forbid(..)]` name,
    /// as written (`clippy::unwrap_used`); none for a file not there.
    fn denied(&mut self, root: &Path, file: &Path) -> Result<&BTreeSet<String>> {
        if !self.0.contains_key(file) {
            let mut denied = BTreeSet::new();
            if let Ok(text) = fs::read_to_string(root.join(file)) {
                let parsed = syn::parse_file(&text)
                    .with_context(|| format!("parsing {}", file.display()))?;
                for attr in &parsed.attrs {
                    if !(attr.path().is_ident("deny") || attr.path().is_ident("forbid")) {
                        continue;
                    }
                    let metas = attr
                        .parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
                        .with_context(|| format!("reading {}'s lints", file.display()))?;
                    denied.extend(metas.iter().filter_map(|meta| match meta {
                        syn::Meta::Path(path) => Some(path_text(path)),
                        _ => None,
                    }));
                }
            }
            self.0.insert(file.to_path_buf(), denied);
        }
        Ok(&self.0[file])
    }
}

fn path_text(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// The lints the crate's Cargo `[lints]` deny or forbid, the workspace's
/// `[workspace.lints]` where the crate inherits them: `clippy::<name>`
/// for a tool's table, the name alone for `rust`'s.
fn cargo_lints(root: &Path, krate: &Crate) -> Result<BTreeSet<String>> {
    let read = |path: PathBuf| -> Result<toml::Table> {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{}", path.display()))
    };
    let own = read(root.join(krate.path()).join("Cargo.toml"))?;
    let Some(lints) = own.get("lints").and_then(toml::Value::as_table) else {
        return Ok(BTreeSet::new());
    };
    let workspace;
    let lints = if lints.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
        workspace = read(root.join("Cargo.toml"))?;
        match workspace
            .get("workspace")
            .and_then(|w| w.get("lints"))
            .and_then(toml::Value::as_table)
        {
            Some(inherited) => inherited,
            None => return Ok(BTreeSet::new()),
        }
    } else {
        lints
    };
    let mut out = BTreeSet::new();
    for (tool, table) in lints.iter().filter_map(|(t, v)| Some((t, v.as_table()?))) {
        for (name, level) in table {
            let level = level
                .as_str()
                .or_else(|| level.get("level").and_then(toml::Value::as_str));
            if matches!(level, Some("deny" | "forbid")) {
                out.insert(if tool == "rust" {
                    name.clone()
                } else {
                    format!("{tool}::{name}")
                });
            }
        }
    }
    Ok(out)
}
