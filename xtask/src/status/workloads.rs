//! The `workloads` section: `docs/perf/workloads.toml` as measurement
//! protocol §11's table, one row per declared workload in the
//! declaration's order and then one per page it declares, so the table is
//! the declaration and not a second list of it (#4344, #4367).

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use super::{Table, cell, grid};
use crate::workloads::{ANSWERS, Declaration, Text, Workload};

/// One row: the workload or page, where it is, its role, its runs, how
/// each ends, the ticket that first read it and what it is.
struct Row<'d> {
    named: &'d str,
    place: String,
    role: &'static str,
    runs: String,
    ends: String,
    first_read: u32,
    about: &'d str,
}

/// Each workload's row, then each page's.
fn rows(declared: &Declaration) -> Vec<Row<'_>> {
    let mut rows: Vec<Row<'_>> = (declared.workloads.iter())
        .map(|w| Row {
            named: &w.stem,
            place: place(w),
            role: w.role.name(),
            runs: runs(w),
            ends: w.ends.to_string(),
            first_read: w.first_read,
            about: &w.about,
        })
        .collect();
    if let Some(pages) = &declared.pages {
        rows.extend(pages.page.iter().map(|page| Row {
            named: &page.name,
            place: format!("page `{}` of the rig", page.path),
            role: pages.role.name(),
            runs: format!("served after {} warm-up requests", pages.warm),
            ends: format!("answers {ANSWERS}"),
            first_read: pages.first_read,
            about: &pages.about,
        }));
    }
    rows
}

/// The section over the workspace at `root`, in `format`.
pub(super) fn render(root: &Path, format: Table) -> Result<String> {
    let declared = Declaration::load(root)?;
    let rows = rows(&declared);
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(
            &(rows.iter())
                .map(|row| {
                    json!({
                        "workload": row.named,
                        "where": row.place,
                        "role": row.role,
                        "runs": row.runs,
                        "ends": row.ends,
                        "first_read": row.first_read,
                        "about": row.about,
                    })
                })
                .collect::<Vec<Value>>(),
        )?,
        Table::Md => grid(
            &[
                "workload",
                "role",
                "each run's arguments",
                "ends on",
                "first read by",
                "what it is",
            ],
            rows.iter().map(|row| {
                [
                    row.place.clone(),
                    row.role.to_owned(),
                    row.runs.clone(),
                    row.ends.clone(),
                    format!("#{}", row.first_read),
                    cell(row.about),
                ]
            }),
        ),
    })
}

/// Where the workload's text is, as the table's first cell says it.
fn place(workload: &Workload) -> String {
    match workload.text() {
        Text::File(path) => {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            format!("`{name}`")
        }
        Text::Pinned(path) => format!("`{}` of the pinned php-src", path.display()),
        Text::Code(_) => format!("`{}`, its code in the declaration", workload.stem),
    }
}

/// Its runs, each by its profile's name and arguments, or "none" where it
/// runs once with none.
fn runs(workload: &Workload) -> String {
    let profiles = workload.profiles();
    if profiles.is_empty() {
        return "none".to_owned();
    }
    let each = profiles.iter().map(|p| format!("`{}`", p.args.join(" ")));
    crate::reading::joined(each, ", ")
}
