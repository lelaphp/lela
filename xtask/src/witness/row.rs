//! One witness reading as a row of `status --section witness --format jsonl
//! --every` (harness-design.md §4.6, #1763): the crate's tally, under a
//! shape `v` versions. Which reading stands for a crate, and what a reader
//! sums over crates, stay the reader's; a row says what one reading counted.

use serde::Serialize;

use super::{INSTRUMENT, numbers};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count};
use crate::tally;

/// The version of [`Row`]'s shape. It changes when the shape does, a
/// field added, removed, renamed or read differently, and never for a
/// new reading; a reader refuses a `v` it does not know.
pub(crate) const V: u32 = 1;

/// One witness reading: what the lane judged of the crate's witnesses and
/// what its coverage found, each 0 where the reading counts none, as one
/// the lane never ran, a crate that carries no witness, or a run cut short,
/// does. The field order is the line's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Row {
    #[serde(flatten)]
    head: Head,
    /// The witnesses the lane judged.
    run: u64,
    /// Those that met their pinned verdict.
    passed: u64,
    /// Those that count against the crate.
    failed: u64,
    /// Those that differ under an accepted registry row.
    diverged: u64,
    /// Those with no verdict pinned yet.
    unpinned: u64,
    /// Those the lane skipped.
    skipped: u64,
    /// Found and not run, the oracle not here to run them.
    not_run: u64,
    /// Not served by the engine, each standing as one the oracle's run
    /// failed does (#1535).
    unserved: u64,
    /// The blocks the fence grammar could not read.
    unread: u64,
    /// The kernels and natives the coverage scope holds.
    subjects: u64,
    /// Those of them without a witness.
    missing: u64,
}

impl PerReading for Row {
    const INSTRUMENT: &'static str = INSTRUMENT;

    /// The tally of one reading, each count under the key its lane keeps
    /// it by ([`numbers`], [`tally::RUN`]).
    fn of(reading: &Reading) -> Row {
        let n = |key: &str| count(reading, key).unwrap_or(0);
        Row {
            head: Head::of(reading, V),
            run: n(tally::RUN),
            passed: n(numbers::PASSED),
            failed: n(numbers::FAILED),
            diverged: n(numbers::DIVERGED),
            unpinned: n(numbers::UNPINNED),
            skipped: n(numbers::SKIPPED),
            not_run: n(numbers::NOT_RUN),
            unserved: n(numbers::UNSERVED),
            unread: n(numbers::UNREAD),
            subjects: n(numbers::SUBJECTS),
            missing: n(numbers::MISSING),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Row;
    use crate::reading::{Number, Reading};
    use crate::status::PerReading;

    /// A witness reading of `scope` with the counts named.
    fn reading(scope: &str, counts: &[(&str, i64)]) -> Reading {
        let mut reading = Reading::new(super::INSTRUMENT, scope);
        reading.numbers = counts
            .iter()
            .map(|(key, n)| ((*key).to_owned(), Number::Int(*n)))
            .collect();
        reading
    }

    /// A crate's tally renders count for count, and a reading that counts
    /// nothing, the harness's skip or a run cut short, renders zeros.
    #[test]
    fn a_tally_renders_count_for_count_and_none_as_zero() {
        let engine = Row::of(&reading(
            "php_engine",
            &[
                ("run", 900),
                ("passed", 897),
                ("failed", 1),
                ("diverged", 2),
                ("subjects", 1200),
                ("missing", 0),
                ("wall_ms", 81_000),
            ],
        ));
        assert_eq!(
            (
                engine.run,
                engine.passed,
                engine.failed,
                engine.diverged,
                engine.subjects
            ),
            (900, 897, 1, 2, 1200)
        );
        let cut = Row::of(&reading("ext/standard", &[("in_flight", 3), ("signal", 9)]));
        assert_eq!((cut.run, cut.passed, cut.failed), (0, 0, 0));
        let line = serde_json::to_value(&cut).unwrap();
        assert_eq!(
            (&line["v"], line.get("wall_ms")),
            (&serde_json::json!(1), None)
        );
    }
}
