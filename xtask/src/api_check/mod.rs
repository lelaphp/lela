//! `xtask api-check`: the instrument that holds the seams (harness-design.md
//! §3 interface locks and dependency direction, §4.1, §6). It reads the
//! manifest's `[[seam]]` and `[[edge]]` rows and the crates that exist, and
//! writes one reading per seam and one per crate:
//!
//! - A seam's lock is regenerated and diffed against the committed file. An
//!   item lock (`heap.lock`, `unit.lock`, `api.lock`, and `xtask.lock`, the
//!   harness's own, #55) is the public items of the crate the lock lives
//!   with, `lock_from` deciding which for the facade; an import lock
//!   (`sapi.lock`, `tier.lock`, the ones an `[[edge]]` cites by `lock` or
//!   `allowlist`, or a seam with `allowlist = true`) is what the consumers
//!   `use` from the producer. `api.lock` is held under `[budget] api_items`.
//! - A crate's workspace dependencies are held to the `[[edge]]` rows whose
//!   `from` matches it: every row is exclusive, so `only = true` restates
//!   what the check does for each crate. A `use` path across a `deny = true`
//!   module edge fails; a `reexports_only` crate may define nothing.
//! - `php_bundle`'s registration order, read as the order of
//!   `php_ext_<name>::extension()` in its sources, is held topological over
//!   the `ext/*` dependency graph (`order = "topological"`).
//!
//! An extension's `pub` items beyond `extension()` are printed in its
//! reading, as the manifest's `ext.pub-beyond-extension` note asks.

mod inventory;
mod lock;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::{Edge, Manifest, Seam};
use crate::reading::{Number, Reading, Status, Store};

use inventory::{Inventory, Leaf, extension_calls, uses};
use lock::{Diff, Lock, list};
use workspace::{Crates, Present};

pub(crate) const INSTRUMENT: &str = "api-check";

/// Run the instrument over `root`, print one line per reading, and record
/// them; the outcome is the exit.
pub fn run(manifest: &Manifest, root: &Path, store: &Store, write: bool) -> Result<()> {
    let readings = check(manifest, root, write)?;
    for reading in &readings {
        println!(
            "{INSTRUMENT} {:<14} {:<4} {}",
            reading.scope,
            reading.status.as_str(),
            reading.message
        );
    }
    store.record(&readings)?.exit()
}

/// Every reading of one run: one per seam, then one per crate present.
/// With `write`, a lock that differs is rewritten and its reading reports
/// that instead of failing.
pub fn check(manifest: &Manifest, root: &Path, write: bool) -> Result<Vec<Reading>> {
    let crates = Crates::scan(manifest, root)?;
    let mut inventory = Inventory::new(&crates);
    let mut readings = Vec::new();
    for seam in &manifest.seams {
        readings.push(seam_reading(
            manifest,
            &crates,
            &mut inventory,
            seam,
            write,
        )?);
    }
    for krate in crates.iter() {
        readings.push(crate_reading(manifest, &crates, &mut inventory, krate)?);
    }
    Ok(readings)
}

// ---------------------------------------------------------------- seams

/// Which inventory a lock is: the items of the crate it lives with, or the
/// consumers' imports from the producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockKind {
    Items,
    Imports,
}

fn lock_kind(manifest: &Manifest, seam: &Seam) -> LockKind {
    let cited = manifest.edges.iter().any(|e| {
        e.lock.as_deref() == Some(seam.lock.as_str())
            || e.allowlist.as_deref() == Some(seam.lock.as_str())
    });
    if seam.allowlist || cited {
        LockKind::Imports
    } else {
        LockKind::Items
    }
}

fn seam_reading(
    manifest: &Manifest,
    crates: &Crates,
    inventory: &mut Inventory<'_>,
    seam: &Seam,
    write: bool,
) -> Result<Reading> {
    // Stamped over the lock's home, the manifest's mapping of the seam;
    // taken again after a rewrite, which the stamp is then over.
    let stamped = || -> Result<Reading> {
        let mut reading = Reading::for_scope(INSTRUMENT, &seam.name, manifest, crates.root())?;
        reading.adr = adrs(&seam.adr);
        // The manifest's `on = "pub"` rule over the lock's home, the note at
        // the tool call that a new public item needs its line: the reading
        // carries its id, so the note and the failing diff read as one rule.
        if let Some(rule) = manifest.lock_line_rule(&seam.lock) {
            reading.rule.clone_from(&rule.id);
        }
        Ok(reading)
    };
    let mut reading = stamped()?;
    let home = manifest.lock_home(&seam.lock);
    let skip = |mut reading: Reading, message: String| {
        reading.status = Status::Skip;
        reading.message = message;
        Ok(reading)
    };
    if seam.generated {
        return skip(
            reading,
            format!(
                "{} is generated from {}; its generator holds it",
                seam.lock, seam.producer
            ),
        );
    }
    let Some(home) = home else {
        return skip(reading, format!("no [[crate]] row names {}", seam.lock));
    };
    let Some(krate) = crates.named(&home) else {
        return skip(reading, format!("{home} does not exist yet"));
    };
    if !seam.sections.is_empty() {
        return skip(
            reading,
            format!(
                "{} is diffed per section ({}); sectioned locks are not measured yet",
                seam.lock,
                seam.sections.join(", ")
            ),
        );
    }
    let kind = lock_kind(manifest, seam);
    let generated = match kind {
        LockKind::Items => inventory.public_items(krate)?,
        LockKind::Imports => imports(crates, seam)?,
    };
    reading
        .numbers
        .insert("items".to_owned(), count(generated.len()));
    let path = krate.dir.join(&seam.lock);
    let shown = crates.relative(&path).display().to_string();
    let subject = match kind {
        LockKind::Items => format!("public items of {home}"),
        LockKind::Imports => format!("imports from {}", seam.producer),
    };

    let mut problems = hold_lock(seam, kind, &path, &shown, &subject, &generated, write)?;
    if write {
        let mut rewritten = stamped()?;
        rewritten.numbers = std::mem::take(&mut reading.numbers);
        reading = rewritten;
    }
    if let Some(ceiling) = manifest.lock_ceiling(seam)
        && generated.len() as u64 > ceiling
    {
        problems.push(format!(
            "{shown} has {} items; the ceiling is {ceiling} (api_items)",
            generated.len()
        ));
    }
    if problems.is_empty() {
        reading.status = Status::Pass;
        reading.message = if write {
            format!("{shown} regenerated: {} {subject}", generated.len())
        } else {
            format!("{} {subject} match {shown}", generated.len())
        };
    } else {
        reading.status = Status::Fail;
        reading.message = problems.join("; ");
    }
    Ok(reading)
}

/// Diff one lock against its regenerated lines, or rewrite it; what is
/// wrong with the committed file, as messages.
fn hold_lock(
    seam: &Seam,
    kind: LockKind,
    path: &Path,
    shown: &str,
    subject: &str,
    generated: &BTreeSet<String>,
    write: bool,
) -> Result<Vec<String>> {
    let committed = if path.is_file() {
        Some(Lock::read(path)?)
    } else {
        None
    };
    if write {
        let lock = committed.unwrap_or_default();
        fs::write(path, lock.render(generated))
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(Vec::new());
    }
    let Some(committed) = committed else {
        return Ok(vec![format!(
            "{shown} does not exist; `xtask api-check --write` regenerates it"
        )]);
    };
    let diff = Diff::between(&committed.lines, generated);
    let mut problems = Vec::new();
    if !diff.missing.is_empty() {
        problems.push(format!(
            "{} {subject} have no line in {shown}: {}",
            diff.missing.len(),
            list(&diff.missing, 8)
        ));
    }
    // An allowlist may name more than its consumer imports today.
    if !diff.stale.is_empty() && !seam.allowlist {
        problems.push(format!(
            "{} lines of {shown} name no {}: {}",
            diff.stale.len(),
            match kind {
                LockKind::Items => "public item",
                LockKind::Imports => "import",
            },
            list(&diff.stale, 8)
        ));
    }
    Ok(problems)
}

/// The paths the seam's consumers `use` from its producer, as lock lines.
fn imports(crates: &Crates, seam: &Seam) -> Result<BTreeSet<String>> {
    let Some(producer) = crates.named(&seam.producer) else {
        return Ok(BTreeSet::new());
    };
    let mut out = BTreeSet::new();
    for consumer in crates.iter() {
        if !seam.consumers.iter().any(|c| matches(c, &consumer.name)) {
            continue;
        }
        let Some(dep) = consumer.deps.iter().find(|d| d.name == producer.name) else {
            continue;
        };
        for file in consumer.sources()? {
            for path in uses(&file)? {
                if path.segs.first() != Some(&dep.alias) || path.segs.len() < 2 {
                    continue;
                }
                let mut line = path.segs[1..].join("::");
                if path.leaf == Leaf::Glob {
                    line.push_str("::*");
                }
                out.insert(line);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- crates

fn crate_reading(
    manifest: &Manifest,
    crates: &Crates,
    inventory: &mut Inventory<'_>,
    krate: &Present,
) -> Result<Reading> {
    let mut reading = Reading::for_scope(INSTRUMENT, &krate.name, manifest, crates.root())?;
    let rows: Vec<&Edge> = manifest
        .edges
        .iter()
        .filter(|e| !e.deny && !e.from.contains("::") && matches(&e.from, &krate.name))
        .collect();
    let mut adr: Vec<&str> = rows.iter().flat_map(|e| e.adr.iter()).collect();
    adr.dedup();
    reading.adr = adr.join(", ");
    reading
        .numbers
        .insert("edges".to_owned(), count(krate.deps.len()));

    let mut problems = Vec::new();
    let mut notes = Vec::new();

    // Dependencies against the permitted edges (design §4.2).
    let permitted: Vec<&str> = rows.iter().flat_map(|e| e.to.iter()).collect();
    for dep in &krate.deps {
        if permitted.iter().any(|p| matches(p, &dep.name)) {
            continue;
        }
        let cite = adr_cite(manifest, &adr);
        problems.push(if rows.is_empty() {
            format!(
                "{} depends on {}; no [[edge]] row gives {} a dependency (design §4.2)",
                krate.name, dep.name, krate.name
            )
        } else {
            format!(
                "{} depends on {}; its [[edge]] rows permit {}{cite}",
                krate.name,
                dep.name,
                permitted.join(", ")
            )
        });
    }

    // `use` paths across a denied module edge.
    problems.extend(denied_uses(manifest, krate)?);

    // A facade defines nothing (`reexports_only`).
    if rows.iter().any(|e| e.reexports_only) {
        let defined = inventory.defines_beyond_reexports(krate)?;
        if !defined.is_empty() {
            problems.push(format!(
                "{} is re-exports only (design §4.2) but defines {}",
                krate.name,
                list(&defined, 8)
            ));
        }
    }

    // The bundle's order over the ext/* graph.
    if let Some(order) = rows.iter().find_map(|e| e.order.as_deref()) {
        if order == "topological" {
            let listed = bundle_order(krate)?;
            reading
                .numbers
                .insert("extensions".to_owned(), count(listed.len()));
            problems.extend(not_topological(crates, &listed));
        } else {
            problems.push(format!(
                "order = \"{order}\" is not an order api-check knows"
            ));
        }
    }

    // What an extension exports beyond `extension()` (design §4.10), the
    // manifest's `ext.pub-beyond-extension` note.
    if krate.name.starts_with("ext/") {
        reading.rule = String::from("ext.pub-beyond-extension");
        let beyond: Vec<String> = inventory
            .public_items(krate)?
            .into_iter()
            .filter(|item| item != "extension")
            .collect();
        if !beyond.is_empty() {
            notes.push(format!("pub beyond extension(): {}", list(&beyond, 12)));
        }
    }

    if problems.is_empty() {
        reading.status = Status::Pass;
        notes.insert(
            0,
            format!(
                "{} workspace {} within its edges",
                krate.deps.len(),
                if krate.deps.len() == 1 {
                    "dependency"
                } else {
                    "dependencies"
                }
            ),
        );
        reading.message = notes.join("; ");
    } else {
        reading.status = Status::Fail;
        problems.extend(notes);
        reading.message = problems.join("; ");
    }
    Ok(reading)
}

/// The `use` paths of `krate` that cross a `deny = true` module edge such
/// as `php_engine::{executor,kernels}` to `php_engine::{serve,...}`.
fn denied_uses(manifest: &Manifest, krate: &Present) -> Result<Vec<String>> {
    let rows: Vec<(Vec<String>, Vec<String>)> = manifest
        .edges
        .iter()
        .filter(|e| e.deny)
        .filter_map(|e| {
            let (from_crate, from_mods) = module_set(&e.from)?;
            let to_mods: Vec<String> =
                e.to.iter()
                    .filter_map(module_set)
                    .filter(|(c, _)| *c == from_crate)
                    .flat_map(|(_, mods)| mods)
                    .collect();
            (from_crate == krate.ident() || from_crate == krate.name)
                .then_some((from_mods, to_mods))
        })
        .collect();
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for file in krate.sources()? {
        let Some(at) = module_of(krate, &file) else {
            continue;
        };
        let Some(top) = at.first() else {
            continue;
        };
        for (from_mods, to_mods) in &rows {
            if !from_mods.contains(top) {
                continue;
            }
            for path in uses(&file)? {
                let Some(resolved) = path.in_crate(&at) else {
                    continue;
                };
                if let Some(target) = resolved.first()
                    && to_mods.contains(target)
                {
                    out.push(format!(
                        "{} uses crate::{}; {}::{{{}}} may not use {}::{{{}}} (design §4.2)",
                        krate.relative(&file).display(),
                        resolved.join("::"),
                        krate.ident(),
                        from_mods.join(","),
                        krate.ident(),
                        to_mods.join(","),
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// `php_engine::{executor,kernels}` as the crate and its module names;
/// `php_engine::serve` as the crate and one module.
fn module_set(text: &str) -> Option<(String, Vec<String>)> {
    let (krate, mods) = text.split_once("::")?;
    let mods = mods.trim_start_matches('{').trim_end_matches('}');
    Some((
        krate.to_owned(),
        mods.split(',').map(|m| m.trim().to_owned()).collect(),
    ))
}

/// The module path of a file under `src/`: `src/lib.rs` is the root,
/// `src/a/mod.rs` and `src/a.rs` are `a`, `src/a/b.rs` is `a::b`. A file
/// outside `src/` has none.
fn module_of(krate: &Present, file: &Path) -> Option<Vec<String>> {
    let rel = file.strip_prefix(krate.dir.join("src")).ok()?;
    let mut parts: Vec<String> = rel
        .iter()
        .filter_map(|p| p.to_str())
        .map(|p| p.trim_end_matches(".rs").to_owned())
        .collect();
    if matches!(
        parts.last().map(String::as_str),
        Some("lib" | "main" | "mod")
    ) {
        parts.pop();
    }
    Some(parts)
}

/// The extensions `php_bundle` registers: every `php_ext_<name>::extension`
/// path in its `src/` files, in the order written, files in path order.
fn bundle_order(krate: &Present) -> Result<Vec<String>> {
    let mut listed: Vec<String> = Vec::new();
    for file in krate.sources()? {
        if module_of(krate, &file).is_none() {
            continue;
        }
        for name in extension_calls(&file)? {
            if !listed.contains(&name) {
                listed.push(name);
            }
        }
    }
    Ok(listed)
}

/// Why `listed` is not a topological order of the `ext/*` graph: an
/// extension before one it depends on, or one whose dependency is absent.
fn not_topological(crates: &Crates, listed: &[String]) -> Vec<String> {
    let position: BTreeMap<&str, usize> = listed
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();
    let mut out = Vec::new();
    for name in listed {
        let Some(krate) = crates.named(name) else {
            continue;
        };
        for dep in krate.deps.iter().filter(|d| d.name.starts_with("ext/")) {
            match position.get(dep.name.as_str()) {
                None => out.push(format!(
                    "the bundle lists {name} but not {}, which it depends on",
                    dep.name
                )),
                Some(&at) if at > position[name.as_str()] => out.push(format!(
                    "the bundle lists {name} before {}, which it depends on; the order is not topological (design §4.10)",
                    dep.name
                )),
                Some(_) => {}
            }
        }
    }
    out
}

// ---------------------------------------------------------------- shared

/// Whether a manifest pattern (`ext/*`, or a name) matches a crate name.
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => name.starts_with(prefix) && name[prefix.len()..].starts_with('/'),
        None => pattern == name,
    }
}

/// A count as a reading's number.
fn count(n: usize) -> Number {
    Number::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn adrs(adr: &crate::manifest::OneOrMany) -> String {
    adr.iter().collect::<Vec<_>>().join(", ")
}

/// ` (ADR-0004: <title>)` for the row's ADRs, the title read from the
/// tracked workspace's `docs/adr/`.
fn adr_cite(manifest: &Manifest, adr: &[&str]) -> String {
    if adr.is_empty() {
        return String::new();
    }
    let cites: Vec<String> = adr
        .iter()
        .map(|n| match manifest.adr_title(n) {
            Some(title) => format!("ADR-{n}: {title}"),
            None => format!("ADR-{n}"),
        })
        .collect();
    format!(" ({})", cites.join("; "))
}
