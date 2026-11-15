//! The rule a bar with a `[[bench]]` tolerance is held by, as Mitchell
//! ruled 2026-10-11 (protocol §12). Each night is the paired delta as a
//! share of the second arm, its median and quartiles, set beside the bar's
//! baseline and its tolerance, both in percentage points.
//!
//! A night whose whole IQR lies worse than the baseline by more than the
//! tolerance is an established regression, and a `NOISE-SWAMPED` night is
//! never one (§1.2). One such night warns on its reading; the second in a
//! row fails it, and the run, naming both nights and the commits from the
//! last night that held to the first that did not.
//!
//! The baseline is a reading, scope `<bar>/baseline`, never a manifest
//! value. The first is the median of the bar's own nights over the
//! tolerance's dates. It moves only when two nights in a row each beat it
//! as an established improvement, the IQR wholly better by more than the
//! tolerance, and then to the worse of their two medians; each move is a
//! reading of its own naming the two nights, so the history says when and
//! why a bar tightened.

use std::fmt::Write as _;

use super::{Sign, Unit, rounded, sign, stats};
use crate::manifest::Tolerance;
use crate::reading::{Reading, Status};
use crate::status::every::count;

/// One night of a bar, in percent of the second arm's median, a cost: a
/// rate's night is turned so that more is worse (`Unit::worse`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Night {
    median: f64,
    q1: f64,
    q3: f64,
    swamped: bool,
}

impl Night {
    /// The night a bar's reading measured; none for one that measured
    /// nothing, or whose second arm took no time.
    fn of(reading: &Reading) -> Option<Night> {
        let unit = Unit::of(reading);
        let n = |key: &str| (reading.numbers.get(&unit.key(key))).map(|n| n.as_f64());
        let share = 100.0 * unit.worse() / n("b_median").filter(|b| *b > 0.0)?;
        let (q1, q3) = (n("delta_q1")?, n("delta_q3")?);
        let reps = usize::try_from(count(reading, "reps")?).ok()?;
        let (low, high) = (q1 * share, q3 * share);
        Some(Night {
            median: n("delta_median")? * share,
            q1: low.min(high),
            q3: low.max(high),
            swamped: sign(reps, q1, q3, unit) == Sign::Swamped,
        })
    }

    /// Where the night stands beside `baseline` and `tolerance`.
    fn beside(self, baseline: f64, tolerance: f64) -> Beside {
        if self.swamped {
            Beside::Held
        } else if self.q1 > baseline + tolerance {
            Beside::Worse
        } else if self.q3 < baseline - tolerance {
            Beside::Better
        } else {
            Beside::Held
        }
    }
}

/// A night beside the baseline: an established regression, an established
/// improvement, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Beside {
    Held,
    Worse,
    Better,
}

/// What the rule makes of a night beside the one before it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Verdict {
    Held,
    Warned,
    Failed,
    /// Two improvements in a row: the baseline moves to this.
    Moved(f64),
}

fn verdict(before: Option<Night>, tonight: Night, baseline: f64, tolerance: f64) -> Verdict {
    let was = before.map(|night| night.beside(baseline, tolerance));
    match (tonight.beside(baseline, tolerance), before, was) {
        (Beside::Worse, _, Some(Beside::Worse)) => Verdict::Failed,
        (Beside::Worse, ..) => Verdict::Warned,
        (Beside::Better, Some(before), Some(Beside::Better)) => {
            Verdict::Moved(tonight.median.max(before.median))
        }
        _ => Verdict::Held,
    }
}

/// The scope a bar's baseline readings are kept under.
pub(crate) fn baseline_scope(bar: &str) -> String {
    format!("{bar}/baseline")
}

/// The day a reading was taken, `YYYY-MM-DD`.
fn day(at: &toml::value::Datetime) -> String {
    at.to_string().chars().take(10).collect()
}

/// A night as a sentence names it.
fn named(reading: &Reading) -> String {
    format!("{} at {}", reading.at, reading.commit)
}

/// Tonight's reading of the bar `tolerance` names held by the rule over
/// the readings `log` already holds, its status and message amended: the
/// baseline reading to record beside it, where the baseline was first set
/// or moved tonight, and the sentence the run fails with, where it failed.
pub(crate) fn hold(
    tolerance: &Tolerance,
    log: &[Reading],
    tonight: &mut Reading,
) -> (Option<Reading>, Option<String>) {
    let (bar, pct) = (&tolerance.bar, tolerance.pct);
    let Some(now) = Night::of(tonight) else {
        return (None, None);
    };
    let nights: Vec<(&Reading, Night)> = (log.iter())
        .filter(|r| &r.scope == bar)
        .filter_map(|r| Night::of(r).map(|night| (r, night)))
        .collect();
    let kept = baseline_scope(bar);
    let mut first = None;
    let dates = (tolerance.nights.as_ref()).map(|[from, to]| (day(from), day(to)));
    let baseline = if let Some(kept) = log.iter().rev().find(|r| r.scope == kept) {
        kept.numbers.get("baseline_pct").map(|n| n.as_f64())
    } else if let Some((from, to)) = &dates {
        let window: Vec<f64> = (nights.iter())
            .filter(|(r, _)| (from.as_str()..=to.as_str()).contains(&day(&r.at).as_str()))
            .map(|(_, night)| night.median)
            .collect();
        first = Some(format!(
            "the median of its {} nights from {from} to {to}",
            window.len()
        ));
        (!window.is_empty()).then(|| stats(&window).median)
    } else {
        None
    };
    let Some(baseline) = baseline else {
        let waits = dates.map_or_else(
            || "its row names no nights, the floor alone".to_owned(),
            |(from, to)| format!("no night of the bar stands from {from} to {to}"),
        );
        write!(
            tonight.message,
            "; its tolerance of {pct}% waits on a baseline: {waits}"
        )
        .ok();
        return (None, None);
    };
    let before = nights.last().map(|(_, night)| *night);
    let verdict = verdict(before, now, baseline, pct);
    tonight
        .numbers
        .insert("baseline_pct".to_owned(), rounded(baseline).into());
    write!(
        tonight.message,
        "; held to its baseline of {baseline:.2}% within its tolerance of {pct}% ({}): {}",
        super::PROTOCOL,
        match verdict {
            Verdict::Held | Verdict::Moved(_) => "it holds",
            Verdict::Warned => "warning, an established regression; a second night in a row fails",
            Verdict::Failed => "an established regression two nights running",
        }
    )
    .ok();
    let moved = match (verdict, first) {
        (Verdict::Moved(to), _) => Some((
            to,
            format!(
                "two established improvements in a row, {} and {}, the worse median of the two",
                nights.last().map(|(r, _)| named(r)).unwrap_or_default(),
                named(tonight)
            ),
        )),
        (_, Some(why)) => Some((baseline, why)),
        _ => None,
    };
    let recorded = moved.map(|(to, why)| {
        let mut kept = tonight.clone();
        kept.scope = baseline_scope(bar);
        kept.status = Status::Pass;
        kept.numbers.clear();
        kept.numbers
            .insert("baseline_pct".to_owned(), rounded(to).into());
        kept.message = format!("{bar}: the baseline is {to:.2}%, {why}");
        kept
    });
    if verdict != Verdict::Failed {
        return (recorded, None);
    }
    tonight.status = Status::Fail;
    // The streak's first failing night, and the last night before it.
    let streak = (nights.iter().rev())
        .take_while(|(_, night)| night.beside(baseline, pct) == Beside::Worse)
        .count();
    let first_failing = nights[nights.len() - streak].0;
    let held = nights.len().checked_sub(streak + 1).map(|at| nights[at].0);
    let failed = format!(
        "{bar} regressed beyond its tolerance of {pct}% over its baseline of {baseline:.2}% two nights running, {} and {}; the commits from the last night that held to the first that did not: {}..{}",
        named(first_failing),
        named(tonight),
        held.map_or("the series' first night", |r| r.commit.as_str()),
        first_failing.commit
    );
    (recorded, Some(failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reading::Number;

    fn night(median: f64, q1: f64, q3: f64) -> Night {
        Night {
            median,
            q1,
            q3,
            swamped: q1 <= 0.0 && q3 >= 0.0,
        }
    }

    /// The rule's fixture series, against a baseline of 10% and a
    /// tolerance of 2 points.
    #[test]
    fn a_night_warns_fails_or_moves_the_baseline_as_the_night_before_it_stood() {
        let (held, worse, better) = (
            night(10.0, 9.0, 11.0),
            night(20.0, 15.0, 25.0),
            night(5.0, 4.0, 6.0),
        );
        let swamped = night(20.0, -1.0, 40.0);
        let judged = |before, tonight| verdict(before, tonight, 10.0, 2.0);
        assert_eq!(
            judged(Some(worse), swamped),
            Verdict::Held,
            "a NOISE-SWAMPED night passes"
        );
        assert_eq!(
            judged(Some(held), worse),
            Verdict::Warned,
            "one regression warns"
        );
        assert_eq!(judged(None, worse), Verdict::Warned);
        assert_eq!(judged(Some(worse), worse), Verdict::Failed, "two fail");
        let worse_of = night(7.0, 6.0, 7.5);
        assert_eq!(
            judged(Some(worse_of), better),
            Verdict::Moved(7.0),
            "to the worse median"
        );
        assert_eq!(
            judged(Some(held), better),
            Verdict::Held,
            "one improvement moves nothing"
        );
    }

    /// A reading of `bar` measured at `pct` of a 100 ms second arm, IQR
    /// one point either side, at `commit`.
    fn reading(bar: &str, pct: f64, commit: &str) -> Reading {
        let mut reading = Reading::new(super::super::INSTRUMENT, bar);
        for (key, value) in [
            ("b_median_ms", 100.0),
            ("delta_median_ms", pct),
            ("delta_q1_ms", pct - 1.0),
            ("delta_q3_ms", pct + 1.0),
        ] {
            reading.numbers.insert(key.to_owned(), Number::Float(value));
        }
        reading.numbers.insert("reps".to_owned(), Number::Int(12));
        commit.clone_into(&mut reading.commit);
        reading
    }

    /// A rate's night is a cost: 30 requests a second short of a second
    /// arm's 100 is 30% worse, its quartiles turned with it.
    #[test]
    fn a_rate_s_night_reads_as_a_cost() {
        let mut reading = Reading::new(super::super::INSTRUMENT, "b");
        for (key, value) in [
            ("b_median_rps", 100.0),
            ("delta_median_rps", -30.0),
            ("delta_q1_rps", -35.0),
            ("delta_q3_rps", -25.0),
        ] {
            reading.numbers.insert(key.to_owned(), Number::Float(value));
        }
        reading.numbers.insert("reps".to_owned(), Number::Int(12));
        let night = Night::of(&reading).unwrap();
        assert_eq!(
            night,
            super::Night {
                median: 30.0,
                q1: 25.0,
                q3: 35.0,
                swamped: false
            }
        );
        assert_eq!(night.beside(10.0, 2.0), Beside::Worse);
    }

    /// The first baseline is the median over the tolerance's dates and is
    /// recorded; a second regression fails and names the commits from the
    /// last night that held.
    #[test]
    fn the_first_baseline_is_the_window_s_median_and_a_failure_names_the_commits() {
        let tolerance = Tolerance {
            bar: "b".to_owned(),
            pct: 2.0,
            nights: Some(["1970-01-01", "2999-01-01"].map(|day| day.parse().unwrap())),
        };
        let log = [
            reading("b", 10.0, "c1"),
            reading("b", 12.0, "c2"),
            reading("b", 30.0, "c3"),
        ];
        let mut tonight = reading("b", 30.0, "c4");
        let (kept, failed) = hold(&tolerance, &log, &mut tonight);
        let kept = kept.expect("the first baseline is recorded");
        assert_eq!(kept.scope, "b/baseline");
        let moved = kept.numbers["baseline_pct"].as_f64();
        assert!((moved - 12.0).abs() < 1e-9, "{}", kept.message);
        assert_eq!(tonight.status, Status::Fail, "{}", tonight.message);
        assert!(failed.unwrap().ends_with("c2..c3"));
        let mut after = reading("b", 30.0, "c5");
        let (kept, _) = hold(&tolerance, &[kept], &mut after);
        assert!(kept.is_none(), "a kept baseline is not set again");
        assert!(after.status != Status::Fail && after.message.contains("warning"));
    }
}
