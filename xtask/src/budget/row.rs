//! One budget reading as a row of `status --section budget --format jsonl
//! --every` (harness-design.md §4.6, #1763): what the reading counted,
//! under a shape `v` versions. Which crate a scope is, and which reading
//! stands for it, depend on the manifest at the ref a reader views, so they
//! stay the reader's; a count the reading does not keep is null, never 0.

use serde::Serialize;

use super::{INSTRUMENT, numbers};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count};

/// The version of [`Row`]'s shape. It changes when the shape does, a
/// field added, removed, renamed or read differently, and never for a
/// new reading; a reader refuses a `v` it does not know.
pub(crate) const V: u32 = 1;

/// One budget reading: a crate's lines against its ceiling, `scripts/**`'s,
/// the workspace's total against `[budget] total`, or a generated file's
/// verdict, which counts nothing. The field order is the line's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Row {
    #[serde(flatten)]
    head: Head,
    /// The lines counted, a crate's own or `scripts/**`'s; null where the
    /// reading counted none: a crate not yet created, the workspace, a
    /// generated file.
    lines: Option<u64>,
    /// The ceiling those lines are held to; null where none is.
    ceiling: Option<u64>,
    /// The binding lines counted apart, a crate's or the workspace's pool;
    /// null where the reading counted none.
    bindings: Option<u64>,
    /// The workspace's every counted line against `[budget] total`; null
    /// for every other reading.
    total: Option<Total>,
}

/// The workspace's counted lines, bindings with them, and the ceiling the
/// manifest holds them to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Total {
    lines: u64,
    ceiling: Option<u64>,
}

impl PerReading for Row {
    const INSTRUMENT: &'static str = INSTRUMENT;

    /// What one reading counted, each count under the key its writer keeps
    /// it by ([`numbers`]).
    fn of(reading: &Reading) -> Row {
        Row {
            head: Head::of(reading, V),
            lines: count(reading, numbers::LINES),
            ceiling: count(reading, numbers::CEILING),
            bindings: count(reading, numbers::BINDINGS),
            total: count(reading, numbers::TOTAL).map(|lines| Total {
                lines,
                ceiling: count(reading, &numbers::ceiling_of(numbers::TOTAL)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Row, Total};
    use crate::reading::{Number, Reading};
    use crate::status::PerReading;

    /// A budget reading of `scope` with the counts named.
    fn reading(scope: &str, counts: &[(&str, i64)]) -> Reading {
        let mut reading = Reading::new(super::INSTRUMENT, scope);
        reading.numbers = counts
            .iter()
            .map(|(key, n)| ((*key).to_owned(), Number::Int(*n)))
            .collect();
        reading
    }

    /// A crate's lines and ceiling, the workspace's total, a crate not
    /// yet created and a generated file each say what they counted, and
    /// nothing they did not.
    #[test]
    fn each_reading_renders_what_it_counted_and_null_for_the_rest() {
        let ffi = Row::of(&reading(
            "ext/ffi",
            &[("lines", 8582), ("ceiling", 14000), ("bindings", 120)],
        ));
        assert_eq!(
            (ffi.lines, ffi.ceiling, ffi.bindings, ffi.total),
            (Some(8582), Some(14000), Some(120), None)
        );
        let workspace = Row::of(&reading(
            "workspace",
            &[
                ("total", 411_160),
                ("total_ceiling", 440_000),
                ("bindings", 26027),
                ("bindings_ceiling", 32000),
            ],
        ));
        assert_eq!(
            (workspace.lines, workspace.total),
            (
                None,
                Some(Total {
                    lines: 411_160,
                    ceiling: Some(440_000)
                })
            )
        );
        let uncreated = Row::of(&reading("php_jit", &[("ceiling", 9000)]));
        assert_eq!((uncreated.lines, uncreated.ceiling), (None, Some(9000)));
        let generated = serde_json::to_value(Row::of(&reading("ext/clippy.toml", &[]))).unwrap();
        assert_eq!(
            (
                &generated["v"],
                &generated["lines"],
                &generated["total"],
                &generated["commit"]
            ),
            (
                &serde_json::json!(1),
                &serde_json::Value::Null,
                &serde_json::Value::Null,
                &serde_json::Value::Null
            )
        );
    }
}
