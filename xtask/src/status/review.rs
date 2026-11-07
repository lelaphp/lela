//! The `review` section: the review coverage of the last seven days in
//! one line, as Mitchell ruled in Round 4 (Q35, c6.h): the class A merges
//! covered, partial and unreviewed, and the class B merges beside them.
//! `worktree land` writes one reading per merge into the main worktree's
//! store as it moves `main`, so this counts those readings, the last of
//! each merge across that store and this checkout's, and judges nothing:
//! the join was the land's (harness-design.md §4.6).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde_json::json;

use super::Table;
use super::tickets::stores;
use crate::manifest::Manifest;
use crate::reading::{self, Reading, Store};
use crate::review::{self, Coverage, coverage::Counted};

/// The merges of the window, counted.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    covered: u64,
    partial: u64,
    unreviewed: u64,
    class_b: u64,
}

impl Counts {
    /// The last reading of each merge among `readings`, counted where the
    /// merge was made at or after `since`, seconds past the epoch.
    fn of<'a>(readings: impl IntoIterator<Item = &'a Reading>, since: i64) -> Counts {
        let mut last: BTreeMap<&str, &Reading> = BTreeMap::new();
        for r in readings {
            if last
                .get(r.scope.as_str())
                .is_none_or(|held| r.at.to_string() >= held.at.to_string())
            {
                last.insert(&r.scope, r);
            }
        }
        let mut counts = Counts::default();
        for counted in last.values().filter_map(|r| Counted::of(r)) {
            if counted.merged_at < since {
                continue;
            }
            match (counted.class_a, counted.coverage) {
                (false, _) => counts.class_b += 1,
                (true, Coverage::Covered) => counts.covered += 1,
                (true, Coverage::Partial(_)) => counts.partial += 1,
                (true, Coverage::Unreviewed) => counts.unreviewed += 1,
            }
        }
        counts
    }

    fn class_a(&self) -> u64 {
        self.covered + self.partial + self.unreviewed
    }

    /// The one line the section and the local facts carry; `tabled` is
    /// whether the manifest has a `[review]` table to class by.
    fn line(&self, tabled: bool) -> String {
        let days = review::DAYS;
        let judged = self.class_a() + self.class_b;
        if !tabled {
            return format!(
                "seams.toml has no `[review]` table, so no merge is class A; the last {days} days: {judged} merge{} judged",
                reading::plural(usize::try_from(judged).unwrap_or(usize::MAX))
            );
        }
        if judged == 0 {
            return format!("the last {days} days: no merge judged");
        }
        format!(
            "the last {days} days: {} of {} class A merges covered, {} partial, {} unreviewed; {} class B",
            self.covered,
            self.class_a(),
            self.partial,
            self.unreviewed,
            self.class_b
        )
    }
}

/// The section, in `format`.
pub(super) fn render(
    manifest: &Manifest,
    checkout: &Store,
    root: &Path,
    format: Table,
) -> Result<String> {
    let since = reading::days_ago(review::DAYS)?;
    let mut logs = Vec::new();
    for store in stores(checkout, root) {
        logs.push(store.history(review::INSTRUMENT)?);
    }
    let counts = Counts::of(logs.iter().flat_map(|log| &log.readings), since);
    let tabled = manifest.review.is_some();
    Ok(match format {
        Table::Md => format!("{}\n", counts.line(tabled)),
        Table::Json => serde_json::to_string_pretty(&json!({
            "days": review::DAYS,
            "table": tabled,
            "class_a": counts.class_a(),
            "covered": counts.covered,
            "partial": counts.partial,
            "unreviewed": counts.unreviewed,
            "class_b": counts.class_b,
        }))?,
    })
}

#[cfg(test)]
mod tests {
    use super::Counts;
    use crate::reading::{Reading, epoch_seconds};

    fn merge(commit: &str, at: &str, merged_at: i64, numbers: &[(&str, i64)]) -> Reading {
        let mut r = Reading::new("review", &format!("merge/{commit}"));
        r.at = at.parse().unwrap();
        r.numbers.insert("merged_at".to_owned(), merged_at.into());
        for (key, n) in numbers {
            r.numbers.insert((*key).to_owned(), (*n).into());
        }
        r
    }

    /// Each merge counts once, by its last reading; a merge before the
    /// window, a finding and a range count nothing.
    #[test]
    fn the_window_counts_each_merge_by_its_last_reading() {
        let since = epoch_seconds(&"2026-11-01T00:00:00Z".parse().unwrap()).unwrap();
        let inside = since + 60;
        let readings = [
            merge(
                "aaaaaaa",
                "2026-11-02T00:00:00Z",
                inside,
                &[("class_a", 1), ("unreviewed", 1)],
            ),
            merge(
                "aaaaaaa",
                "2026-11-03T00:00:00Z",
                inside,
                &[("class_a", 1), ("covered", 1)],
            ),
            merge(
                "bbbbbbb",
                "2026-11-02T00:00:00Z",
                inside,
                &[("class_a", 1), ("partial", 2)],
            ),
            merge(
                "ccccccc",
                "2026-11-02T00:00:00Z",
                inside,
                &[("class_a", 0), ("unreviewed", 1)],
            ),
            merge(
                "ddddddd",
                "2026-11-02T00:00:00Z",
                since - 1,
                &[("class_a", 1), ("covered", 1)],
            ),
            Reading::new("review", "4268/aaaaaaa"),
            Reading::new("review", "4268/aaaaaaa-0123456789ab"),
        ];
        let counts = Counts::of(&readings, since);
        assert_eq!(
            counts,
            Counts {
                covered: 1,
                partial: 1,
                unreviewed: 0,
                class_b: 1
            }
        );
        assert_eq!(
            counts.line(true),
            "the last 7 days: 1 of 2 class A merges covered, 1 partial, 0 unreviewed; 1 class B"
        );
        assert_eq!(
            counts.line(false),
            "seams.toml has no `[review]` table, so no merge is class A; the last 7 days: 3 merges judged"
        );
        assert_eq!(
            Counts::default().line(true),
            "the last 7 days: no merge judged"
        );
    }
}
