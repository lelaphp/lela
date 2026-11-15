//! The `crates` and `edges` sections: the manifest's `[[crate]]`
//! and `[[edge]]` rows, which the design documents and the map render as
//! regions rather than restate by hand (design §4.1, §4.2). Both read the
//! manifest alone, so a region over either is held by `inputs=seams.toml`
//! and the renderer's own files, `xtask/src/status.rs` and this one;
//! only the crates' every-row rendering also reads whether a crate's
//! directory holds a `Cargo.toml`.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::{Table, cell, grid};
use crate::layout::HARNESS;
use crate::manifest::{Edge, Manifest};
use crate::sources;

/// One `[[crate]]` row as the every-row rendering shows it.
#[derive(Debug, Serialize)]
struct CrateRow<'a> {
    #[serde(rename = "crate")]
    name: &'a str,
    directory: String,
    feature: Option<&'a str>,
    /// Whether the directory holds a `Cargo.toml` ([`sources::exists`]).
    created: bool,
}

/// The `crates` section. In Markdown, without `uncut`, one line per
/// directory crates are laid down in and one for the harness, each with
/// how many rows the manifest has there and how many of them are behind a
/// feature; with it, and in JSON, every row with its directory, its
/// feature and whether it is created.
pub(super) fn crates(
    manifest: &Manifest,
    root: &Path,
    format: Table,
    uncut: bool,
) -> Result<String> {
    if format == Table::Md && !uncut {
        let layout = manifest.layout();
        let homes = (layout.dirs().map(|dir| (format!("{dir}/"), dir)))
            .chain(std::iter::once((HARNESS.to_owned(), HARNESS)));
        let lines = homes.map(|(label, dir)| {
            let rows: Vec<_> = manifest
                .crates
                .iter()
                .filter(|c| c.path().starts_with(dir))
                .collect();
            let featured = rows.iter().filter(|c| c.optional()).count();
            [label, rows.len().to_string(), featured.to_string()]
        });
        return Ok(grid(&["directory", "crates", "behind a feature"], lines));
    }
    let rows: Vec<CrateRow> = manifest
        .crates
        .iter()
        .map(|c| CrateRow {
            name: &c.name,
            directory: c.path().display().to_string(),
            feature: c.feature.as_deref(),
            created: sources::exists(root, c),
        })
        .collect();
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&rows)?,
        Table::Md => grid(
            &["crate", "directory", "feature", "created"],
            rows.iter().map(|row| {
                let created = if row.created { "yes" } else { "no" };
                let feature = cell(row.feature.unwrap_or_default());
                [
                    cell(row.name),
                    cell(&row.directory),
                    feature,
                    created.to_owned(),
                ]
            }),
        ),
    })
}

/// The `edges` section: every `[[edge]]` in the manifest's order. In JSON
/// the rows as the manifest parses them; in Markdown each with where it
/// reaches, how the row qualifies the edge ([`qualified`]) and its ADRs.
pub(super) fn edges(manifest: &Manifest, format: Table) -> Result<String> {
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&manifest.edges)?,
        Table::Md => grid(
            &["from", "to", "as", "adr"],
            manifest.edges.iter().map(|edge| {
                let adr: Vec<String> = edge.adr.iter().map(|n| format!("ADR-{n}")).collect();
                let to = cell(&edge.to.iter().collect::<Vec<_>>().join(", "));
                [
                    cell(&edge.from),
                    to,
                    cell(&qualified(edge).join("; ")),
                    adr.join(", "),
                ]
            }),
        ),
    })
}

/// What an edge's row says beside its two ends, in the order the row's
/// keys are declared: a denied dependency, a facade's, a closed list, an
/// order, an allowlist or a lock it goes through.
fn qualified(edge: &Edge) -> Vec<String> {
    let mut said = Vec::new();
    if edge.deny {
        said.push("denied".to_owned());
    }
    if edge.reexports_only {
        said.push("re-exports only".to_owned());
    }
    if edge.only {
        said.push("these only".to_owned());
    }
    if let Some(order) = &edge.order {
        said.push(format!("{order} order"));
    }
    if let Some(lock) = &edge.allowlist {
        said.push(format!("imports only what {lock} lists"));
    }
    if let Some(lock) = &edge.lock {
        said.push(format!("through {lock}"));
    }
    said
}
