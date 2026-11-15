//! The facts harness revision 184 made honest, each read where the local
//! facts render and never at a prompt: whether CI runs and passes on
//! `main`, the tracker's week beside the store's closes, and the
//! divergence rows that moved cause beside the rows fixed. What GitHub
//! cannot be asked is said as unknown, never as green.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::divergence::{self, Cause, Registry};
use crate::reading;

/// What a row says when GitHub did not answer.
const UNKNOWN: &str = "unknown: GitHub could not be asked";

/// `gh` asked `args` in `root`, its stdout where it answered.
fn gh(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("gh")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The calendar day `days` before now, as GitHub's search reads one.
fn day(days: i64) -> Option<String> {
    let (y, m, d) = reading::civil_from_days(reading::days_ago(days).ok()?.div_euclid(86_400));
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// One run of a workflow, as `gh run list --json` gives it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    conclusion: String,
    created_at: String,
    url: String,
}

/// The CI row: the last run on `main` that executed a job, read as one
/// that concluded in success or failure, since a cancelled or skipped run
/// may have run none, and the last that passed.
pub(super) fn ci(root: &Path) -> String {
    let asked = [
        "run",
        "list",
        "--branch",
        "main",
        "--limit",
        "100",
        "--json",
        "conclusion,createdAt,url",
    ];
    ci_line(gh(root, &asked).as_deref())
}

/// The row over `gh`'s answer, newest run first, or none.
fn ci_line(answer: Option<&str>) -> String {
    let runs: Option<Vec<Run>> = answer.and_then(|a| serde_json::from_str(a).ok());
    let Some(runs) = runs else {
        return format!("CI on main: {UNKNOWN}\n");
    };
    let none = format!("none among the last {} runs", runs.len());
    let said = |run: Option<&Run>| {
        run.map_or_else(|| none.clone(), |r| format!("{} ({})", r.created_at, r.url))
    };
    let ran = runs
        .iter()
        .find(|r| matches!(r.conclusion.as_str(), "success" | "failure"));
    let passed = runs.iter().find(|r| r.conclusion == "success");
    format!(
        "CI on main: the last run that executed a job {}; the last that passed {}\n",
        said(ran),
        said(passed)
    )
}

/// The tracker's week: tickets created and closed in the last `days`, and
/// the tickets open at the window's start and now.
pub(super) fn tracker(root: &Path, days: i64) -> String {
    let count = |args: &[&str]| -> Option<usize> {
        let answer = gh(root, args)?;
        Some(
            serde_json::from_str::<Vec<serde_json::Value>>(&answer)
                .ok()?
                .len(),
        )
    };
    let since = day(days).unwrap_or_default();
    let list = |state: &str, search: String| {
        let args = [
            "issue", "list", "--state", state, "--limit", "5000", "--json", "number",
        ];
        let search = ["--search".to_owned(), search];
        count(
            &args
                .iter()
                .copied()
                .chain(search.iter().map(String::as_str))
                .collect::<Vec<_>>(),
        )
    };
    let created = list("all", format!("created:>={since}"));
    let closed = list("closed", format!("closed:>={since}"));
    let open = list("open", String::new());
    tracker_line(
        days,
        created.zip(closed).zip(open).map(|((a, b), c)| (a, b, c)),
    )
}

/// The tracker's line over `(created, closed, open now)`.
fn tracker_line(days: i64, counted: Option<(usize, usize, usize)>) -> String {
    let Some((created, closed, open)) = counted else {
        return format!("on the tracker: {UNKNOWN}");
    };
    let then = (open + closed).saturating_sub(created);
    let per = match closed {
        0 => "no close".to_owned(),
        n => {
            let real = |n: usize| f64::from(u32::try_from(n).unwrap_or(u32::MAX));
            format!("{:.2} per close", real(created) / real(n))
        }
    };
    format!(
        "on the tracker, the last {days} days: {created} created against {closed} closed, {per}; the tickets open went from {then} to {open}"
    )
}

/// The registry as it stood `days` ago against now: how many rows were
/// fixed and how many left the debt for another cause in that time.
pub(super) fn moves(root: &Path, now: &Registry, days: i64) -> String {
    let before = format!("--before={days}.days.ago");
    let then = crate::git::git(root, &["rev-list", "-1", &before, "HEAD"]).ok();
    let at = then.map(|c| format!("{}:{}", c.trim(), divergence::FILE));
    let text = at.and_then(|at| crate::git::git(root, &["show", &at]).ok());
    match text.and_then(|t| Registry::parse(&t).ok()) {
        Some(then) => moved_line(days, &then, now),
        None => format!("Over the last {days} days: the registry then could not be read."),
    }
}

/// The moves between `then` and `now`, by row id.
fn moved_line(days: i64, then: &Registry, now: &Registry) -> String {
    let (mut fixed, mut moved) = (0, 0);
    for row in now.rows() {
        let Some(was) = then.rows().iter().find(|r| r.id == row.id) else {
            continue;
        };
        fixed += usize::from(was.status != row.status && row.status == divergence::Status::Fixed);
        moved += usize::from(was.cause == Cause::Debt && row.cause != Cause::Debt);
    }
    format!(
        "Over the last {days} days: {fixed} rows fixed, and {moved} rows moved out of the debt to another cause, which lowers the total without fixing it."
    )
}

#[cfg(test)]
mod tests {
    use super::{ci_line, moved_line, tracker_line};
    use crate::divergence::Registry;

    /// The row over `gh run list`'s answer on `main`, recorded 2026-10-30:
    /// 90 cancelled runs and 10 failures, none passed; no answer
    /// is unknown, never green.
    #[test]
    fn the_ci_row_reads_the_last_run_that_executed_a_job_and_the_last_that_passed() {
        let recorded = crate::package_dir().join("fixtures/status/gh-runs.json");
        let line = ci_line(Some(&std::fs::read_to_string(recorded).unwrap()));
        assert!(
            line.contains("executed a job 2026-10-30T02:29:52Z ("),
            "{line}"
        );
        assert!(
            line.contains("passed none among the last 100 runs"),
            "{line}"
        );
        let cancelled = r#"[{"conclusion":"cancelled","createdAt":"b","url":"u"},{"conclusion":"success","createdAt":"a","url":"u"}]"#;
        assert!(
            ci_line(Some(cancelled)).contains("executed a job a (u); the last that passed a (u)")
        );
        assert_eq!(
            ci_line(None),
            "CI on main: unknown: GitHub could not be asked\n"
        );
    }

    #[test]
    fn the_tracker_line_shows_creations_against_closes_and_the_open_tickets_change() {
        let line = tracker_line(7, Some((143, 117, 171)));
        assert!(
            line.contains("143 created against 117 closed, 1.22 per close"),
            "{line}"
        );
        assert!(line.contains("went from 145 to 171"), "{line}");
        assert!(tracker_line(7, None).contains("unknown"));
    }

    /// A row that moved from the debt to another cause is counted apart
    /// from a row fixed.
    #[test]
    fn rows_that_moved_cause_are_counted_apart_from_rows_fixed() {
        let row = |id: &str, cause: &str, status: &str| {
            format!(
                "[[divergence]]\nid = \"{id}\"\nkind = \"witness\"\ncause = \"{cause}\"\nstatus = \"{status}\"\nsince = \"2026-10-01\"\nreason = \"r\"\n"
            )
        };
        let (a, b) = (
            format!("witness:{}", "a".repeat(40)),
            format!("witness:{}", "b".repeat(40)),
        );
        let then = [row(&a, "debt", "open"), row(&b, "debt", "open")].concat();
        let now = [row(&a, "debt", "fixed"), row(&b, "by-design", "accepted")].concat();
        let line = moved_line(
            7,
            &Registry::parse(&then).unwrap(),
            &Registry::parse(&now).unwrap(),
        );
        assert!(
            line.contains("1 rows fixed, and 1 rows moved out of the debt"),
            "{line}"
        );
    }
}
