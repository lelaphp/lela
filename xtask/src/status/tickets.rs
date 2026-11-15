//! The `tickets` section: the week's closes, the successors they opened,
//! the withdrawals among them and the successors opened per close, the
//! ratio adopted in place of a limit on `--opens`. Read from the
//! `ticket` readings of the stores alone, never the tracker: a close
//! lands at the merge into `main`, whose hook writes it into the main
//! worktree's store, and a withdrawal is written where its session
//! ran until `worktree retire` carries it there, so both stores are read.

use std::path::Path;

use anyhow::Result;
use serde_json::json;

use super::Table;
use crate::reading::{self, Reading, Store};
use crate::ticket::{self, Closed};

/// How far back the section counts, in days.
pub(super) const DAYS: i64 = 7;

/// The closes of the window, counted.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    closes: u64,
    withdrawn: u64,
    opens: u64,
}

impl Counts {
    /// The closes among `readings` taken at or after `since`, seconds
    /// past the epoch.
    fn of<'a>(readings: impl IntoIterator<Item = &'a Reading>, since: i64) -> Result<Counts> {
        let mut week = Counts::default();
        for r in readings {
            let Some(closed) = Closed::of(r) else {
                continue;
            };
            if reading::epoch_seconds(&r.at)? < since {
                continue;
            }
            week.closes += 1;
            week.withdrawn += u64::from(closed.withdrawn);
            week.opens += closed.opens;
        }
        Ok(week)
    }

    /// Successors opened per close, none with no close.
    #[allow(clippy::cast_precision_loss)]
    fn per_close(&self) -> Option<f64> {
        (self.closes > 0).then(|| self.opens as f64 / self.closes as f64)
    }

    /// The one line the section and the session's local facts carry.
    fn line(&self) -> String {
        match self.per_close() {
            None => format!("the last {DAYS} days: no close"),
            Some(ratio) => format!(
                "the last {DAYS} days: {} closes, {} of them withdrawals, opened {} tickets, {ratio:.2} per close",
                self.closes, self.withdrawn, self.opens
            ),
        }
    }
}

/// The stores a close is written in: the main worktree's, where `git`
/// names one, and the checkout's when it is another.
pub(super) fn stores(checkout: &Store, root: &Path) -> Vec<Store> {
    let mut stores = vec![checkout.clone()];
    if let Ok(main) = crate::track::main_worktree(root) {
        let main = Store::of_workspace(&main);
        if !crate::built::same_directory(main.root(), checkout.root()) {
            stores.insert(0, main);
        }
    }
    stores
}

/// The week over every store a close is written in.
fn counted(checkout: &Store, root: &Path) -> Result<Counts> {
    let since = reading::days_ago(DAYS)?;
    let mut logs = Vec::new();
    for store in stores(checkout, root) {
        logs.push(store.history(ticket::INSTRUMENT)?);
    }
    Counts::of(logs.iter().flat_map(|log| &log.readings), since)
}

/// The section, in `format`.
pub(super) fn render(checkout: &Store, root: &Path, format: Table) -> Result<String> {
    let week = counted(checkout, root)?;
    Ok(match format {
        Table::Md => format!("{}\n", week.line()),
        Table::Json => serde_json::to_string_pretty(&json!({
            "days": DAYS,
            "closes": week.closes,
            "withdrawn": week.withdrawn,
            "opens": week.opens,
            "opens_per_close": week.per_close(),
        }))?,
    })
}

#[cfg(test)]
mod tests {
    use super::Counts;
    use crate::reading::{Reading, Status, epoch_seconds};

    fn reading(at: &str, status: Status, numbers: &[(&str, i64)]) -> Reading {
        let mut r = Reading::new("ticket", "1");
        r.status = status;
        r.at = at.parse().unwrap();
        for (key, n) in numbers {
            r.numbers.insert((*key).to_owned(), (*n).into());
        }
        r
    }

    /// A close counts once, a withdrawal among the closes; a ready record,
    /// a read, a refused close and one before the window count nothing.
    #[test]
    fn the_week_counts_the_closes_and_what_they_opened() {
        let since = epoch_seconds(&"2026-10-02T00:00:00Z".parse().unwrap()).unwrap();
        let readings = [
            reading(
                "2026-10-03T00:00:00Z",
                Status::Pass,
                &[("opens", 2), ("withdrawn", 0)],
            ),
            reading(
                "2026-10-04T00:00:00Z",
                Status::Pass,
                &[("opens", 1), ("withdrawn", 1)],
            ),
            reading(
                "2026-10-04T00:00:00Z",
                Status::Pass,
                &[("opens", 0), ("withdrawn", 0)],
            ),
            reading("2026-10-05T00:00:00Z", Status::Pass, &[("opens", 5)]),
            reading("2026-10-05T00:00:00Z", Status::Pass, &[("criteria", 3)]),
            reading(
                "2026-10-05T00:00:00Z",
                Status::Fail,
                &[("opens", 4), ("withdrawn", 0)],
            ),
            reading(
                "2026-10-01T00:00:00Z",
                Status::Pass,
                &[("opens", 9), ("withdrawn", 0)],
            ),
        ];
        let week = Counts::of(&readings, since).unwrap();
        assert_eq!(
            week,
            Counts {
                closes: 3,
                withdrawn: 1,
                opens: 3
            }
        );
        assert_eq!(
            week.line(),
            "the last 7 days: 3 closes, 1 of them withdrawals, opened 3 tickets, 1.00 per close"
        );
        assert_eq!(Counts::default().line(), "the last 7 days: no close");
    }
}
