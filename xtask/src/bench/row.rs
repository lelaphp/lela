//! One bench reading as a row of `status --section bench --format jsonl
//! --every` (harness-design.md §4.6, #2646): a bar's night, under a shape
//! `v` versions, so a bar's series reads as its readings in the log's
//! order, over the store's log or the published history (`--log`). A row
//! says what one reading measured and leaves a number it does not keep
//! null.

use serde::Serialize;

use super::{INSTRUMENT, Unit, sign};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count};

/// The version of [`Row`]'s shape, as every row's `v` is kept
/// (`witness::row::V`).
pub(crate) const V: u32 = 2;

/// One bar's reading: the unit it measured in, the paired delta, its
/// spread and its verdict, the distance a series has to go, the reps and
/// the load beside them, each null where the reading keeps none. The
/// field order is the line's. Shape 2 names the unit, `ms` or `rps`
/// (#2660), where shape 1 kept milliseconds in `delta_median_ms` and
/// `delta_iqr_ms`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Row {
    #[serde(flatten)]
    head: Head,
    /// `ms`, a run's time, or `rps`, its clients' requests a second.
    unit: Unit,
    /// The median of the paired per-rep deltas, in the unit.
    delta_median: Option<f64>,
    /// Their interquartile range, in the unit.
    delta_iqr: Option<f64>,
    /// The paired delta as a share of the second arm, %.
    paired_pct: Option<f64>,
    /// Whether the quartiles establish the delta's sign, as the run
    /// judged it: `resolved` or `NOISE-SWAMPED`.
    verdict: Option<&'static str>,
    /// How far a series has to go to its bar, %; none for a control.
    to_go_pct: Option<f64>,
    /// The baseline a series with a tolerance is held to, %, and on a
    /// `<bar>/baseline` row the value it moved to (#2659).
    baseline_pct: Option<f64>,
    reps: Option<u64>,
    /// The load average over 1, 5 and 15 minutes as the run began.
    load: Option<[f64; 3]>,
}

impl PerReading for Row {
    const INSTRUMENT: &'static str = INSTRUMENT;

    fn of(reading: &Reading) -> Row {
        let unit = Unit::of(reading);
        let n = |key: &str| reading.numbers.get(key).map(|n| n.as_f64());
        let of = |key: &str| n(&unit.key(key));
        let reps = count(reading, "reps");
        let quartiles = of("delta_q1").zip(of("delta_q3"));
        Row {
            head: Head::of(reading, V),
            unit,
            delta_median: of("delta_median"),
            delta_iqr: of("delta_iqr"),
            paired_pct: n("paired_pct"),
            verdict: reps
                .zip(quartiles)
                .map(|(reps, (q1, q3))| sign(usize::try_from(reps).unwrap_or(usize::MAX), q1, q3)),
            to_go_pct: n("to_go_pct"),
            baseline_pct: n("baseline_pct"),
            reps,
            load: n("load_1")
                .zip(n("load_5"))
                .zip(n("load_15"))
                .map(|((a, b), c)| [a, b, c]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reading::Number;

    /// A reading's measurement is its row, the verdict judged from its
    /// quartiles as the run judged it; a number it does not keep is null.
    #[test]
    fn a_bench_reading_is_a_row_of_its_delta_verdict_load_and_distance() {
        let mut reading = Reading::new(INSTRUMENT, "makala-fpm/ucall");
        for (key, n) in [
            ("delta_median_ms", 4.5),
            ("delta_q1_ms", 4.0),
            ("delta_q3_ms", 5.0),
            ("delta_iqr_ms", 1.0),
            ("paired_pct", 7.25),
            ("to_go_pct", 12.0),
            ("load_1", 1.5),
            ("load_5", 1.25),
            ("load_15", 1.0),
        ] {
            reading.numbers.insert(key.to_owned(), Number::Float(n));
        }
        reading.numbers.insert("reps".to_owned(), Number::Int(8));
        let line = serde_json::to_value(Row::of(&reading)).unwrap();
        assert_eq!(line["verdict"], "resolved");
        assert_eq!(
            (line["unit"].as_str(), &line["delta_median"]),
            (Some("ms"), &serde_json::json!(4.5))
        );
        assert_eq!(line["load"], serde_json::json!([1.5, 1.25, 1.0]));
        assert_eq!(line["to_go_pct"], 12.0);
        reading
            .numbers
            .insert("delta_q1_ms".to_owned(), Number::Float(-1.0));
        reading.numbers.remove("to_go_pct");
        let line = serde_json::to_value(Row::of(&reading)).unwrap();
        assert_eq!(line["verdict"], "NOISE-SWAMPED");
        assert_eq!(line["to_go_pct"], serde_json::Value::Null);
        let mut rate = Reading::new(INSTRUMENT, "makala-fpm-c8/ucall");
        for (key, n) in [
            ("b_median_rps", 40.0),
            ("delta_median_rps", -8.0),
            ("delta_iqr_rps", 2.0),
        ] {
            rate.numbers.insert(key.to_owned(), Number::Float(n));
        }
        let line = serde_json::to_value(Row::of(&rate)).unwrap();
        assert_eq!(
            (line["unit"].as_str(), &line["delta_median"]),
            (Some("rps"), &serde_json::json!(-8.0))
        );
        assert_eq!(line["delta_iqr"], 2.0);
        let mut per_call = Reading::new(INSTRUMENT, "jit-wrapper-unwind");
        (per_call.numbers).insert("delta_median_ns".to_owned(), Number::Float(0.5));
        let line = serde_json::to_value(Row::of(&per_call)).unwrap();
        assert_eq!(line["unit"].as_str(), Some("ns"));
    }
}
