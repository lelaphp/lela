//! One witness reading as a row of `status --section witness --format jsonl
//! --every` (harness-design.md §4.6): the crate's tally, under a
//! shape `v` versions. Which reading stands for a crate, and what a reader
//! sums over crates, stay the reader's; a row says what one reading counted
//! and leaves a count it does not keep null, never 0.

use serde::Serialize;

use super::{INSTRUMENT, numbers};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count};
use crate::tally;

/// The version of [`Row`]'s shape, which changes only when the shape does
/// and never for a new reading; a reader refuses a `v` it does not know
/// (harness-design.md §4.6, revision 76). A field a row may omit, whose
/// absence a reader of the earlier shape already handles correctly, is
/// added under the same `v`; a field removed, renamed or retyped, or a
/// meaning changed, is a new `v`.
pub(crate) const V: u32 = 1;

/// One witness reading: what the lane judged of the crate's witnesses and
/// what its coverage found, each null where the reading keeps no such
/// count, as the harness's skip, a run cut short by a signal, or a reading
/// from before the count was kept. The field order is the line's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Row {
    #[serde(flatten)]
    head: Head,
    /// The witnesses the lane judged.
    run: Option<u64>,
    /// Those that met their pinned verdict.
    passed: Option<u64>,
    /// Those that count against the crate.
    failed: Option<u64>,
    /// Those that differ under an accepted registry row.
    diverged: Option<u64>,
    /// Those an accepted `trace:` row covers, held to their pinned
    /// verdict against the engine alone.
    covered: Option<u64>,
    /// Those with no verdict pinned yet.
    unpinned: Option<u64>,
    /// Those the lane skipped.
    skipped: Option<u64>,
    /// Found and not run, the oracle not here to run them.
    not_run: Option<u64>,
    /// Not served by the engine, each standing as one the oracle's run
    /// failed does.
    unserved: Option<u64>,
    /// The blocks the fence grammar could not read.
    unread: Option<u64>,
    /// The kernels and natives the coverage scope holds.
    subjects: Option<u64>,
    /// Those of them without a witness.
    missing: Option<u64>,
    /// The fence lines `--accept` wrote.
    accepted: Option<u64>,
}

impl PerReading for Row {
    const INSTRUMENT: &'static str = INSTRUMENT;

    /// The tally of one reading, each count under the key its lane keeps
    /// it by ([`numbers`], [`tally::RUN`]).
    fn of(reading: &Reading) -> Row {
        let n = |key: &str| count(reading, key);
        Row {
            head: Head::of(reading, V),
            run: n(tally::RUN),
            passed: n(numbers::PASSED),
            failed: n(numbers::FAILED),
            diverged: n(numbers::DIVERGED),
            covered: n(numbers::COVERED),
            unpinned: n(numbers::UNPINNED),
            skipped: n(numbers::SKIPPED),
            not_run: n(numbers::NOT_RUN),
            unserved: n(numbers::UNSERVED),
            unread: n(numbers::UNREAD),
            subjects: n(numbers::SUBJECTS),
            missing: n(numbers::MISSING),
            accepted: n(numbers::ACCEPTED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Row;
    use crate::reading::Reading;
    use crate::status::{PerReading, every};

    /// A reading of `scope` with the counts named.
    fn reading(scope: &str, counts: &[(&str, i64)]) -> Reading {
        every::reading(super::INSTRUMENT, scope, counts)
    }

    /// A crate's tally renders count for count, `covered` beside `diverged`,
    /// and a reading that counts nothing, the harness's skip, a
    /// run cut short or one from before `covered` was kept, renders nulls.
    #[test]
    fn a_tally_renders_count_for_count_and_none_as_null() {
        let engine = Row::of(&reading(
            "php_engine",
            &[
                ("run", 900),
                ("passed", 897),
                ("failed", 1),
                ("diverged", 2),
                ("covered", 3),
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
                engine.covered,
                engine.subjects
            ),
            (Some(900), Some(897), Some(1), Some(2), Some(3), Some(1200))
        );
        let cut = Row::of(&reading("ext/standard", &[("in_flight", 3), ("signal", 9)]));
        assert_eq!(
            (cut.run, cut.passed, cut.failed, cut.covered),
            (None, None, None, None)
        );
        let line = serde_json::to_value(&cut).unwrap();
        assert_eq!(
            (&line["v"], line.get("wall_ms")),
            (&serde_json::json!(1), None)
        );
    }
}
