//! Design §4.10's list of surfaces, held at two places, as #2020 ruled:
//! the edge by a row a person writes, the items by a lock the instrument
//! writes (#2251).
//!
//! **The edge.** Every library dependency of an `ext/*` crate on another
//! (a `[dependencies]` entry, plain or under a target; a dev- or
//! build-dependency links nothing) stands on a row of the section's table
//! whose Surface cell names the producer before its `→` and the consumer
//! after it: `` | `pcre` → `spl`, `filter` (#716) | ``. The cell is read
//! where a person wrote it and nowhere else; a computed region in the
//! section is skipped, so a row rendered from the tree holds nothing.
//!
//! **The items.** What an extension exports beyond `extension()` is its
//! surface, written to `ext/<name>/surface.lock` and diffed exactly as
//! `api.lock` is ([`super::hold_lock`]): the lock is the one place the
//! items live, landed in the same diff as the code that changes them, and
//! the rows' Items and Mirrors cells are a person's prose about them.
//!
//! **The marker.** An item of the surface may carry `#[mirrors]`, the
//! php-src declaration it stands for, held against the pinned tree
//! ([`super::mirrors`], #2353), in a crate with a surface or without one.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::LockKind;
use super::inventory::Inventory;
use super::lock::{Block, Lock};
use super::mirrors::Pinned;
use super::workspace::{Crates, Present};
use crate::oracle::Source;

/// The design of record, at the workspace root.
const DESIGN: &str = "third-engine-design.md";

/// The section of [`DESIGN`] that carries the list of surfaces.
const SECTION: &str = "4.10";

/// The lock beside an extension's `Cargo.toml` that lists its surface.
const LOCK: &str = "surface.lock";

/// The edges §4.10's rows name, producer then consumer, each an extension
/// as `ext/<name>` spells it.
#[derive(Debug, Default)]
pub(crate) struct Surfaces {
    /// Whether [`DESIGN`] has a §4.10 at all: without one, no edge stands
    /// on a row and the reading says why.
    found: bool,
    edges: BTreeSet<(String, String)>,
    /// The pin's php-src, which a marker is held against.
    pinned: Pinned,
}

/// What the hold makes of one extension: problems, which fail the reading,
/// and notes, which it carries either way.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pub(crate) problems: Vec<String>,
    pub(crate) notes: Vec<String>,
}

impl Surfaces {
    /// §4.10 of `root`'s [`DESIGN`], with the markers to be held against
    /// `source`, or none where the manifest names no pin; a workspace
    /// without the file has no rows.
    pub(crate) fn read(root: &Path, source: Result<&Source>) -> Result<Surfaces> {
        let text = crate::read_if(&root.join(DESIGN))?;
        let surfaces = text.map_or_else(Surfaces::default, |text| Surfaces::parse(&text));
        Ok(Surfaces {
            pinned: Pinned::new(source),
            ..surfaces
        })
    }

    /// The rows as `text`, a design document, writes them in its §4.10.
    fn parse(text: &str) -> Surfaces {
        let mut out = Surfaces::default();
        let heading = format!("### {SECTION} ");
        let mut lines = text.lines();
        if !lines.by_ref().any(|line| line.starts_with(&heading)) {
            return out;
        }
        out.found = true;
        let mut rendered = false;
        for line in lines {
            if ends_section(line) {
                break;
            }
            if line.starts_with("<!-- computed") {
                rendered = true;
            } else if line.starts_with("<!-- /computed") {
                rendered = false;
            } else if !rendered && let Some((producer, consumers)) = row(line) {
                out.edges
                    .extend(consumers.map(|c| (producer.to_owned(), c.to_owned())));
            }
        }
        out
    }

    /// Hold `krate`, an `ext/*` crate: each extension it links to a row,
    /// and its surface to its [`LOCK`].
    pub(crate) fn hold(
        &self,
        crates: &Crates,
        inventory: &mut Inventory<'_>,
        krate: &Present,
    ) -> Result<Held> {
        let mut held = Held::default();
        let Some(consumer) = krate.name.strip_prefix("ext/") else {
            return Ok(held);
        };
        let mut rowed = 0;
        for dep in krate.deps.iter().filter(|d| d.linked) {
            let Some(producer) = dep.name.strip_prefix("ext/") else {
                continue;
            };
            let edge = (producer.to_owned(), consumer.to_owned());
            if self.edges.contains(&edge) {
                rowed += 1;
            } else if self.found {
                held.problems.push(format!(
                    "{} links {}, and no row of design §{SECTION}'s list of surfaces names `{producer}` → `{consumer}`",
                    krate.name, dep.name
                ));
            } else {
                held.problems.push(format!(
                    "{} links {}, and {DESIGN} has no §{SECTION} here to name the edge",
                    krate.name, dep.name
                ));
            }
        }
        if rowed > 0 {
            held.notes.push(format!(
                "{rowed} {} on a row of design §{SECTION}'s list of surfaces",
                if rowed == 1 {
                    "link to an extension stands"
                } else {
                    "links to extensions stand"
                }
            ));
        }

        let marked = self.pinned.hold(krate)?;
        held.problems.extend(marked.problems);
        held.notes.extend(marked.notes);

        let items = surface(inventory, krate)?;
        let path = krate.dir.join(LOCK);
        let committed = path.is_file().then(|| Lock::read(&path)).transpose()?;
        if items.is_empty() && committed.is_none() {
            return Ok(held);
        }
        let shown = crates.relative(&path).display().to_string();
        let subject = format!("pub items of {} beyond extension()", krate.name);
        let n = items.len();
        let blocks = [Block::of(None, items)];
        let problems = super::hold_lock(
            None,
            LockKind::Items,
            &path,
            &shown,
            &subject,
            committed,
            &blocks,
            false,
        )?;
        if problems.is_empty() {
            held.notes.push(format!("{n} {subject} match {shown}"));
        }
        held.problems.extend(problems);
        Ok(held)
    }
}

/// Write each `ext/*` crate's [`LOCK`] as its surface reads now, its
/// header kept; a crate that exports nothing beyond `extension()` keeps
/// none.
pub(crate) fn write(crates: &Crates, inventory: &mut Inventory<'_>) -> Result<()> {
    for krate in crates.iter().filter(|k| k.name.starts_with("ext/")) {
        let items = surface(inventory, krate)?;
        let path = krate.dir.join(LOCK);
        let committed = path.is_file().then(|| Lock::read(&path)).transpose()?;
        if items.is_empty() {
            if committed.is_some() {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            }
            continue;
        }
        let lock = committed.unwrap_or_else(|| Lock {
            header: vec![format!(
                "The pub items of {} beyond extension(), its surface: design §{SECTION}'s rows hold its edges, this file its items, and `xtask api-check --write` rewrites it.",
                krate.name
            )],
            ..Lock::default()
        });
        crate::write(&path, lock.render(&[Block::of(None, items)]))?;
    }
    Ok(())
}

/// The `pub` items of `krate` beyond `extension()`, as lock lines.
fn surface(inventory: &mut Inventory<'_>, krate: &Present) -> Result<BTreeSet<String>> {
    let mut items = inventory.public_items(krate)?;
    items.remove("extension");
    Ok(items)
}

/// `` | `pcre` → `spl`, `zip` (#1520) | items | mirrors | ``: the producer
/// and the consumers its Surface cell names; the header and the rule
/// beneath it have no `→`.
fn row(line: &str) -> Option<(&str, impl Iterator<Item = &str>)> {
    let cell = line.strip_prefix("| ")?.split(" | ").next()?;
    let (left, right) = cell.split_once('→')?;
    Some((spans(left).next()?, spans(right)))
}

/// The code spans of `text` that spell an extension's name as `ext/<name>`
/// does.
fn spans(text: &str) -> impl Iterator<Item = &str> {
    text.split('`').skip(1).step_by(2).filter(|span| {
        !span.is_empty()
            && span
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

/// Whether `line` is a heading of level one to three, which ends a `###`
/// section; a `####` heading stands inside it.
fn ends_section(line: &str) -> bool {
    let hashes = line.len() - line.trim_start_matches('#').len();
    (1..=3).contains(&hashes) && line[hashes..].starts_with(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Design

### 4.9 Before

| Surface | Items | Mirrors |
|---|---|---|
| `pcre` → `hash` | `Elsewhere` | not this section |

### 4.10 Extension crates

| Surface | Items | Mirrors |
|---|---|---|
| `pcre` → `spl`, `zip` (#1520) | `Regex`, with `compile` | `php_pcre.h`'s `php_pcre_split_impl` |
| `pdo` → `pdo_mysql`, `pdo_sqlite` | `connector::Connection` | `php_pdo_driver.h` |
| `libxml` → the other XML extensions | prose | `php_libxml.h` |

<!-- computed exec cmd=\"cargo xtask status --section surfaces\" name=surfaces | do not edit; run computed -->
| `pcre` → `filter` | `Rendered` | rendered |
<!-- /computed in=0 out=0 -->

#### 4.10.1 A subsection stays in the section

| `random` → `standard` | `Rng` | `php_random.h` |

### 4.11 After

| `pcre` → `mbstring` | `After` | not this section |
";

    #[test]
    fn a_row_names_its_producer_and_each_consumer_its_surface_cell_spells() {
        let surfaces = Surfaces::parse(DOC);
        assert!(surfaces.found);
        let edges: Vec<(&str, &str)> = surfaces
            .edges
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        assert_eq!(
            edges,
            [
                ("pcre", "spl"),
                ("pcre", "zip"),
                ("pdo", "pdo_mysql"),
                ("pdo", "pdo_sqlite"),
                ("random", "standard"),
            ],
            "prose names no consumer, a rendered row and a row of another section name nothing, and a deeper heading is still §4.10"
        );
    }

    #[test]
    fn a_design_without_the_section_names_no_edge() {
        let surfaces = Surfaces::parse("### 4.9 Before\n\n| `pcre` → `spl` | `Regex` | x |\n");
        assert!(!surfaces.found);
        assert!(surfaces.edges.is_empty());
    }

    #[test]
    fn only_a_span_that_spells_an_extension_is_one() {
        assert_eq!(
            spans("`spl`, `php_pcre.h`, `Regex`, `zip` (#1520), `pdo_sqlite`, ``")
                .collect::<Vec<_>>(),
            ["spl", "zip", "pdo_sqlite"]
        );
    }
}
