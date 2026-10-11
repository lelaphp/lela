//! `xtask api-check`: the instrument that holds the seams (harness-design.md
//! §3 interface locks and dependency direction, §4.1, §6). It reads the
//! manifest's `[[seam]]` and `[[edge]]` rows and the crates that exist, and
//! writes one reading per seam and one per crate:
//!
//! - A seam's lock is regenerated and diffed against the committed file. An
//!   item lock (`heap.lock`, `unit.lock`, `api.lock`, and `xtask.lock`, the
//!   harness's own, #55) is the public items of the crate the lock lives
//!   with, `lock_from` deciding which for the facade, or of one module of
//!   its producer for a seam with `module` (`layer.lock`, #1959), whose
//!   `use` by a crate outside the seam's consumers fails; an import lock
//!   (`sapi.lock`, `tier.lock`, the ones an `[[edge]]` cites by `lock` or
//!   `allowlist`, or a seam with `allowlist = true`) is what the consumers
//!   `use` from the producer; a rendered lock (`ops.lock`, the seam whose
//!   producer is a file rather than a crate) is that file's rows, one line
//!   an opcode. A seam with `sections` is an import lock diffed per
//!   `[name]` section, each against the consumers that read it (#1369), and
//!   an import another lock of the consumer's holds is not this one's.
//!   `api.lock` is held under `[budget] api_items`, and its
//!   `__private` lines, where a person has written the row, under
//!   `api_private_items` instead.
//! - A crate's `Cargo.toml` carries `[lints] workspace = true`, the one
//!   way the root manifest's `[workspace.lints]` reaches it (#2111).
//! - A crate's workspace dependencies are held to the `[[edge]]` rows whose
//!   `from` matches it: every row is exclusive, so `only = true` restates
//!   what the instrument does for each crate. A `use` path across a `deny = true`
//!   module edge fails; a `reexports_only` crate may define nothing.
//! - `php_bundle`'s registrations, read as the `php_ext_<name>::extension`
//!   paths in its sources and the features they stand behind, are held to
//!   the `ext/*` dependency graph under its row's `order =
//!   "registration"`, the one order #1778 ruled and api-check serves: the
//!   order written is php's and free, and each extension's feature switches
//!   on the features of every extension it declares `Required` ([`bundle`]).
//!
//! An item lock's reading counts its lines by the module that defines each
//! in its `groups`, a key per module, beside the total in `numbers` (#1795):
//! a type whose methods are written in one file per area is counted per
//! file, so a split of one module into several, or the growth of one, is
//! read off the reading and not off `api.lock`, whose format it leaves as
//! it is. The counts are held to no ceiling, a per-area ceiling having
//! been ruled out on 2026-10-01; `status --section modules` renders them.
//! Every line is counted, `__private`'s included, which all stand under
//! `php_engine::api::__private` and so are that module's row; an import
//! lock, sectioned or not, and a rendered lock keep no groups.
//!
//! Design §4.10's list of surfaces is held at two places ([`surfaces`],
//! #2251): an `ext/*` crate's library dependency on another fails its
//! reading unless a row a person wrote names both, and an extension's
//! `pub` items beyond `extension()` are its `surface.lock`, regenerated
//! and diffed as an item lock is; `--write` rewrites those too.

mod bundle;
mod inventory;
pub(crate) mod lock;
mod ops;
pub(crate) mod row;
mod surfaces;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::key::Snapshot;
use crate::manifest::{Edge, LockCeiling, Manifest, Seam, Section};
use crate::reading::{Number, Reading, Status, Store};
use crate::sources::Standing;

use inventory::{Inventory, Leaf, uses};
use lock::{Block, Diff, Lock, list};
use surfaces::Surfaces;
use workspace::{Crates, Present};

pub(crate) const INSTRUMENT: &str = "api-check";

/// The keys of an api-check reading's counts, named once for the
/// instrument that writes them and the rendering that reads them.
pub(crate) mod numbers {
    /// A seam's lock lines: its public items, or the opcodes of `ops.toml`.
    pub(crate) const ITEMS: &str = "items";
    /// Of a seam's items, those `__private` names.
    pub(crate) const PRIVATE: &str = "private";
    /// A crate's workspace dependencies, each held to its edges.
    pub(crate) const EDGES: &str = "edges";
    /// The extensions the bundle registers, held to the crate graph.
    pub(crate) const EXTENSIONS: &str = "extensions";
}

/// Run the instrument over `root`, print one line per reading, and record
/// them; the outcome is the exit. With `staged`, a crate whose reading
/// stands for the commit is left out ([`measure`]).
pub fn run(
    manifest: &Manifest,
    root: &Path,
    store: &Store,
    write: bool,
    staged: bool,
) -> Result<()> {
    let standing = Standing::of_every_crate(staged, root, manifest, store, INSTRUMENT)?;
    let readings = measure(manifest, root, write, &standing)?;
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
pub fn hold(manifest: &Manifest, root: &Path, write: bool) -> Result<Vec<Reading>> {
    measure(manifest, root, write, &Standing::default())
}

/// [`hold`] less the crates whose reading stands for the commit (#1143),
/// save one whose verdict reads the other crates' edges, as the bundle's
/// order does, which its own stamp does not cover. Every seam is held:
/// an import lock reads its consumers, which its stamp does not cover
/// either.
pub(crate) fn measure(
    manifest: &Manifest,
    root: &Path,
    write: bool,
    standing: &Standing,
) -> Result<Vec<Reading>> {
    let crates = Crates::scan(manifest, root)?;
    let mut inventory = Inventory::new(&crates);
    // Written before the snapshot, so each extension's reading is stamped
    // over the lock it holds.
    if write {
        surfaces::write(&crates, &mut inventory)?;
    }
    let snapshot = Snapshot::disk(root)?;
    let surfaces = Surfaces::read(root)?;
    let mut readings = Vec::new();
    for seam in &manifest.seams {
        readings.push(seam_reading(
            manifest,
            &snapshot,
            &crates,
            &mut inventory,
            seam,
            write,
        )?);
    }
    for krate in crates.iter() {
        let ordered = manifest
            .edges
            .iter()
            .any(|e| e.order.is_some() && matches(&e.from, &krate.name));
        if standing.get(&krate.name).is_some() && !ordered {
            continue;
        }
        readings.push(crate_reading(
            manifest,
            &snapshot,
            &crates,
            &mut inventory,
            &surfaces,
            krate,
        )?);
    }
    Ok(readings)
}

/// The public items of the crate `name` under `root`, as an item lock's
/// lines, its re-exports from the workspace included, each with the module
/// that defines it as a path from its crate's name (`php_engine::engine`);
/// the sources are read and nothing is built. The new-sapi fixture's
/// stand-ins are held to the real crates through it, by name (#2544) and
/// by shape (#2545).
pub fn defining_modules(
    manifest: &Manifest,
    root: &Path,
    name: &str,
) -> Result<BTreeMap<String, String>> {
    let crates = Crates::scan(manifest, root)?;
    let krate = crates
        .named(name)
        .with_context(|| format!("no crate {name} under {}", root.display()))?;
    Inventory::new(&crates).defining_modules(krate)
}

// ---------------------------------------------------------------- seams

/// Which inventory a lock is: the items of the crate it lives with, the
/// consumers' imports from the producer, or the rows of the file the seam
/// is generated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockKind {
    Items,
    Imports,
    Rendered,
}

impl LockKind {
    /// What one line of such a lock names, for the message a stale line
    /// carries.
    fn line(self) -> &'static str {
        match self {
            LockKind::Items => "public item",
            LockKind::Imports => "import",
            LockKind::Rendered => "opcode",
        }
    }

    /// What the lines of such a lock at `home` are, for a reading's
    /// message.
    fn subject(self, home: &str, seam: &Seam) -> String {
        match self {
            LockKind::Items => format!("public items of {home}"),
            LockKind::Imports => format!("imports from {}", seam.producer),
            LockKind::Rendered => format!("{} of {}", ops::SUBJECT, seam.producer),
        }
    }
}

fn lock_kind(manifest: &Manifest, seam: &Seam) -> LockKind {
    if seam.generated {
        return LockKind::Rendered;
    }
    if seam.module.is_some() {
        return LockKind::Items;
    }
    let cited = manifest.edges.iter().any(|e| {
        e.lock.as_deref() == Some(seam.lock.as_str())
            || e.allowlist.as_deref() == Some(seam.lock.as_str())
    });
    // A section is read by consumers, so a sectioned lock is an import lock.
    if seam.allowlist || cited || !seam.sections.is_empty() {
        LockKind::Imports
    } else {
        LockKind::Items
    }
}

fn seam_reading(
    manifest: &Manifest,
    snapshot: &Snapshot,
    crates: &Crates,
    inventory: &mut Inventory<'_>,
    seam: &Seam,
    write: bool,
) -> Result<Reading> {
    // Stamped over the lock's home, the manifest's mapping of the seam;
    // taken again after a rewrite, over a snapshot taken after it, which
    // the stamp is then over.
    let stamped = |snapshot: &Snapshot| -> Result<Reading> {
        let mut reading = Reading::for_scope(INSTRUMENT, &seam.name, manifest, snapshot)?;
        reading.adr = adrs(&seam.adr);
        // The manifest's `on = "pub"` rule over the lock's home, the note at
        // the tool call that a new public item needs its line: the reading
        // carries its id, so the note and the failing diff read as one rule.
        if let Some(rule) = manifest.lock_line_rule(&seam.lock) {
            reading.rule.clone_from(&rule.id);
        }
        Ok(reading)
    };
    let mut reading = stamped(snapshot)?;
    let skip = |mut reading: Reading, message: String| {
        reading.status = Status::Skip;
        reading.message = message;
        Ok(reading)
    };
    let kind = lock_kind(manifest, seam);
    if kind == LockKind::Rendered && seam.producer != ops::PRODUCER {
        return skip(
            reading,
            format!(
                "{} is generated from {}, which the harness renders no lock from",
                seam.lock, seam.producer
            ),
        );
    }
    let Some(home) = manifest.lock_home(&seam.lock) else {
        return skip(
            reading,
            format!("no [[crate]] row or [lock_home] entry names {}", seam.lock),
        );
    };
    let Some(krate) = crates.named(&home) else {
        return skip(reading, format!("{home} does not exist yet"));
    };
    let path = krate.dir.join(&seam.lock);
    let shown = crates.relative(&path).display().to_string();
    let committed = path.is_file().then(|| Lock::read(&path)).transpose()?;
    let (blocks, mut problems) = if seam.sections.is_empty() {
        let lines = match kind {
            LockKind::Items => {
                lines_counted_by_module(inventory, krate, seam.module.as_deref(), &mut reading)?
            }
            LockKind::Imports => imports(manifest, crates, seam)?
                .into_values()
                .flatten()
                .collect(),
            LockKind::Rendered => ops::lines(&crates.root().join(&seam.producer))?,
        };
        (vec![Block::of(None, lines)], Vec::new())
    } else {
        sections(manifest, crates, seam, &shown, committed.as_ref())?
    };
    let generated: BTreeSet<String> = blocks.iter().flat_map(|b| b.lines.clone()).collect();
    reading
        .numbers
        .insert(numbers::ITEMS.to_owned(), count(generated.len()));
    let subject = kind.subject(&home, seam);

    problems.extend(hold_lock(
        seam, kind, &path, &shown, &subject, committed, &blocks, write,
    )?);
    problems.extend(strangers(crates, seam)?);
    if write {
        let mut rewritten = stamped(&Snapshot::disk(crates.root())?)?;
        rewritten.numbers = std::mem::take(&mut reading.numbers);
        rewritten.groups = std::mem::take(&mut reading.groups);
        reading = rewritten;
    }
    if let Some(ceiling) = manifest.lock_ceiling(seam) {
        let counted = Counted::of(&generated, Some(&ceiling));
        let items = counted.items();
        let mut beside = "";
        if let (Some(cap), Counted::Split { private, .. }) = (ceiling.private, counted) {
            // Split, `items` is what `api_items` holds, as the locks
            // section reads it against that ceiling.
            beside = " beside __private's";
            reading.count(numbers::ITEMS, items as u64)?;
            reading.count(numbers::PRIVATE, private as u64)?;
            if private as u64 > cap {
                problems.push(format!(
                    "{shown} has {private} {PRIVATE} items; the ceiling is {cap} (api_private_items)"
                ));
            }
        }
        if items as u64 > ceiling.items {
            problems.push(format!(
                "{shown} has {items} items{beside}; the ceiling is {} (api_items)",
                ceiling.items
            ));
        }
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

/// Diff one lock against its regenerated blocks, or rewrite it; what is
/// wrong with the committed file, as messages. A block is diffed against
/// the committed block of its name, a section's against its `[name]`
/// marker's, and a committed line under a marker the manifest does not
/// name stands in no section. An extension's `surface.lock` is held here
/// with no seam, as an item lock ([`surfaces`]).
#[allow(clippy::too_many_arguments)]
fn hold_lock<'s>(
    seam: impl Into<Option<&'s Seam>>,
    kind: LockKind,
    path: &Path,
    shown: &str,
    subject: &str,
    committed: Option<Lock>,
    blocks: &[Block],
    write: bool,
) -> Result<Vec<String>> {
    if write {
        let lock = committed.unwrap_or_default();
        fs::write(path, lock.render(blocks))
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(Vec::new());
    }
    let Some(committed) = committed else {
        return Ok(vec![format!(
            "{shown} does not exist; `xtask api-check --write` regenerates it"
        )]);
    };
    let seam: Option<&Seam> = seam.into();
    let sections = seam.map_or(&[][..], |s| &s.sections[..]);
    let mut problems = Vec::new();
    for block in blocks {
        let (at, listed) = match &block.name {
            None => (String::new(), committed.lines.clone()),
            Some(name) => (
                format!(" [{name}]"),
                committed
                    .section(name)
                    .map(|b| b.lines.clone())
                    .unwrap_or_default(),
            ),
        };
        let diff = Diff::between(&listed, &block.lines);
        if !diff.missing.is_empty() {
            problems.push(format!(
                "{} {subject} have no line in {shown}{at}: {}",
                diff.missing.len(),
                list(&diff.missing, 8)
            ));
        }
        // An allowlist may name more than its consumer imports today.
        if !diff.stale.is_empty() && !seam.is_some_and(|s| s.allowlist) {
            problems.push(format!(
                "{} lines of {shown}{at} name no {}: {}",
                diff.stale.len(),
                kind.line(),
                list(&diff.stale, 8)
            ));
        }
    }
    if !sections.is_empty() {
        let astray: Vec<String> = committed
            .blocks
            .iter()
            .filter(|b| !blocks.iter().any(|g| g.name == b.name))
            .flat_map(|b| b.as_written.clone())
            .collect();
        if !astray.is_empty() {
            let names: Vec<&str> = sections.iter().map(Section::name).collect();
            problems.push(format!(
                "{} lines of {shown} stand in no section of {}: {}",
                astray.len(),
                names.join(", "),
                list(&astray, 8)
            ));
        }
    }
    // The one order a lock's lines stand in (`lock`, #403), read off the
    // committed file: what `--write` would rewrite even where every line
    // is the right one, which is the churn a write taken for another seam
    // carried into a commit.
    if let Some(disorder) = committed.disorder() {
        problems.push(format!(
            "{shown} is not in the order `xtask api-check --write` renders: {disorder}"
        ));
    }
    Ok(problems)
}

/// The paths each of the seam's consumers `use` from its producer, as lock
/// lines, less what the lock of another seam it consumes from the producer
/// holds (#1369): `php_sapi`'s `tier::*` imports are `tier.lock`'s face, and
/// so not `sapi.lock`'s. An import neither lists is missing from both.
fn imports(
    manifest: &Manifest,
    crates: &Crates,
    seam: &Seam,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut out = BTreeMap::new();
    let Some(producer) = crates.named(&seam.producer) else {
        return Ok(out);
    };
    for consumer in crates.iter() {
        if !seam.consumers.iter().any(|c| matches(c, &consumer.name)) {
            continue;
        }
        let Some(dep) = consumer.deps.iter().find(|d| d.name == producer.name) else {
            continue;
        };
        let mut held = BTreeSet::new();
        for other in manifest.seams.iter().filter(|o| {
            o.name != seam.name
                && o.producer == seam.producer
                && o.consumers.iter().any(|c| matches(c, &consumer.name))
        }) {
            let home = manifest.lock_home(&other.lock);
            if let Some(krate) = home.and_then(|h| crates.named(&h)) {
                let path = krate.dir.join(&other.lock);
                if path.is_file() {
                    held.extend(Lock::read(&path)?.lines);
                }
            }
        }
        let lines: &mut BTreeSet<String> = out.entry(consumer.name.clone()).or_default();
        for file in consumer.sources()? {
            for path in uses(&file)? {
                if path.segs.first() != Some(&dep.alias) || path.segs.len() < 2 {
                    continue;
                }
                let mut line = path.segs[1..].join("::");
                if path.leaf == Leaf::Glob {
                    line.push_str("::*");
                }
                if !held.contains(&line) {
                    lines.insert(line);
                }
            }
        }
    }
    Ok(out)
}

/// The `use` paths into a seam's `module` of its producer written by a
/// crate outside the seam's consumers (#1959): `php_ext` re-exporting
/// `php_engine::layer`, or an extension reaching past its facade for it,
/// each a face the seam keeps for its consumers alone. None for a seam
/// that is no module.
fn strangers(crates: &Crates, seam: &Seam) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let (Some(module), Some(producer)) = (&seam.module, crates.named(&seam.producer)) else {
        return Ok(out);
    };
    for krate in crates.iter() {
        if krate.name == producer.name || seam.consumers.iter().any(|c| matches(c, &krate.name)) {
            continue;
        }
        let Some(dep) = krate.deps.iter().find(|d| d.name == producer.name) else {
            continue;
        };
        for file in krate.sources()? {
            for path in uses(&file)? {
                if path.segs.first() == Some(&dep.alias) && path.segs.get(1) == Some(module) {
                    out.push(format!(
                        "{} uses {}; {}::{module} is {}'s alone",
                        crates.relative(&file).display(),
                        path.segs.join("::"),
                        producer.ident(),
                        seam.consumers.join(", "),
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// A sectioned lock's blocks, one per section in the manifest's order, and
/// what is wrong with where the committed file places an import (#1369).
/// Each consumer's import stands in the section the file lists it under
/// when the consumer reads that section; one the file does not list goes
/// to the first section the consumer reads, where the diff finds it
/// missing; one listed only under a section the consumer does not read is
/// a failure, `kernels` being `php_jit`'s alone. An allowlist keeps every
/// line it lists in a section the manifest names.
fn sections(
    manifest: &Manifest,
    crates: &Crates,
    seam: &Seam,
    shown: &str,
    committed: Option<&Lock>,
) -> Result<(Vec<Block>, Vec<String>)> {
    let listed = |section: &Section| committed.and_then(|l| l.section(section.name()));
    let mut blocks: Vec<Block> = seam
        .sections
        .iter()
        .map(|section| {
            let kept = listed(section).filter(|_| seam.allowlist);
            Block::of(
                Some(section.name()),
                kept.map(|b| b.lines.clone()).unwrap_or_default(),
            )
        })
        .collect();
    let mut problems = Vec::new();
    for (consumer, imports) in imports(manifest, crates, seam)? {
        let reads = |section: &Section| section.readers(seam).iter().any(|r| matches(r, &consumer));
        let Some(first) = seam.sections.iter().position(reads) else {
            continue;
        };
        for line in imports {
            let under = seam
                .sections
                .iter()
                .position(|s| listed(s).is_some_and(|b| b.lines.contains(&line)));
            match under {
                Some(at) if !reads(&seam.sections[at]) => problems.push(format!(
                    "{consumer} imports {line}, which {shown} lists under [{}], read by {} alone",
                    seam.sections[at].name(),
                    seam.sections[at].readers(seam).join(", ")
                )),
                Some(at) => {
                    blocks[at].lines.insert(line);
                }
                None => {
                    blocks[first].lines.insert(line);
                }
            }
        }
    }
    Ok((blocks, problems))
}

// ---------------------------------------------------------------- crates

fn crate_reading(
    manifest: &Manifest,
    snapshot: &Snapshot,
    crates: &Crates,
    inventory: &mut Inventory<'_>,
    surfaces: &Surfaces,
    krate: &Present,
) -> Result<Reading> {
    let mut reading = Reading::for_scope(INSTRUMENT, &krate.name, manifest, snapshot)?;
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
        .insert(numbers::EDGES.to_owned(), count(krate.deps.len()));

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

    // The workspace's lints reach a member only through its own
    // `[lints] workspace = true` (#1998, #2111).
    if !krate.inherits_lints {
        problems.push(format!(
            "{}'s Cargo.toml lacks `[lints] workspace = true`, so the root manifest's [workspace.lints] do not reach it (#2111)",
            krate.name
        ));
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

    // The bundle's order, held to the ext/* graph as the row's `order`
    // says.
    if let Some(order) = rows.iter().find_map(|e| e.order.as_deref()) {
        if order == bundle::REGISTRATION {
            let read = bundle::Bundle::read(krate)?;
            reading
                .numbers
                .insert(numbers::EXTENSIONS.to_owned(), count(read.names().len()));
            problems.extend(bundle::unswitched(crates, &read));
        } else {
            problems.push(format!(
                "order = \"{order}\" is not an order api-check knows; it serves \"{}\" alone (#2255)",
                bundle::REGISTRATION
            ));
        }
    }

    // An extension's links to others, each held to a row of design §4.10's
    // list of surfaces, and its surface to its lock (#2251).
    if krate.name.starts_with("ext/") {
        reading.rule = String::from("ext.pub-beyond-extension");
        let held = surfaces.hold(crates, inventory, krate)?;
        problems.extend(held.problems);
        notes.extend(held.notes);
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

// ---------------------------------------------------------------- shared

/// Whether a manifest pattern (`ext/*`, or a name) matches a crate name.
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => name.starts_with(prefix) && name[prefix.len()..].starts_with('/'),
        None => pattern == name,
    }
}

/// The module the lines `api_private_items` holds stand under: the helpers
/// only a macro expansion names (design §4.4, revision 34).
const PRIVATE: &str = "__private";

/// Whether a lock line stands under [`PRIVATE`], read off its own path.
fn private(line: &str) -> bool {
    line.strip_prefix(PRIVATE)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

/// A lock's lines as its `items` number counts them: every line, or,
/// where the seam's ceiling holds the lines under [`PRIVATE`] apart
/// (`api_private_items`), the rest beside those. api-check counts the
/// lines it regenerates, and the `locks` section the lines the tree's lock
/// lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counted {
    Whole(usize),
    Split { items: usize, private: usize },
}

impl Counted {
    pub(crate) fn of(lines: &BTreeSet<String>, ceiling: Option<&LockCeiling>) -> Counted {
        if ceiling.and_then(|c| c.private).is_none() {
            return Counted::Whole(lines.len());
        }
        let private = lines.iter().filter(|line| private(line)).count();
        Counted::Split {
            items: lines.len() - private,
            private,
        }
    }

    /// What `items` holds: every line, or those beside `__private`'s.
    pub(crate) fn items(self) -> usize {
        match self {
            Counted::Whole(items) | Counted::Split { items, .. } => items,
        }
    }
}

/// The public items of `krate` as lock lines, or of its one `module` alone
/// (#1959); on the way, `reading` gains one group per module that defines
/// any, its count of them (#1795).
fn lines_counted_by_module(
    inventory: &mut Inventory<'_>,
    krate: &Present,
    module: Option<&str>,
    reading: &mut Reading,
) -> Result<BTreeSet<String>> {
    let mut modules = inventory.defining_modules(krate)?;
    if let Some(module) = module {
        modules.retain(|line, _| {
            line.strip_prefix(module)
                .is_some_and(|rest| rest.starts_with("::"))
        });
    }
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for module in modules.values() {
        *counts.entry(module).or_default() += 1;
    }
    for (module, n) in counts {
        reading.group(module, n)?;
    }
    Ok(modules.into_keys().collect())
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
