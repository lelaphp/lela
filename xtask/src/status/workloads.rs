//! The `workloads` section: `docs/perf/workloads.toml` as measurement
//! protocol §11's table, one row per declared workload in the
//! declaration's order, so the table is the declaration and not a second
//! list of it (#4344).

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use super::{Table, cell, grid};
use crate::workloads::{Declaration, Text, Workload};

/// The section over the workspace at `root`, in `format`.
pub(super) fn render(root: &Path, format: Table) -> Result<String> {
    let declared = Declaration::load(root)?;
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(
            &(declared.workloads.iter())
                .map(|w| {
                    json!({
                        "workload": w.stem,
                        "where": place(w),
                        "role": w.role.name(),
                        "runs": runs(w),
                        "ends": w.ends.to_string(),
                        "first_read": w.first_read,
                        "about": w.about,
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
            declared.workloads.iter().map(|w| {
                [
                    place(w),
                    w.role.name().to_owned(),
                    runs(w),
                    w.ends.to_string(),
                    format!("#{}", w.first_read),
                    cell(&w.about),
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
