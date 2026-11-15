//! One budget reading as a row of `status --section budget --format jsonl
//! --every` (harness-design.md §4.6): what the reading counted,
//! under a shape `v` versions. Which crate a scope is, and which reading
//! stands for it, depend on the manifest at the ref a reader views, so they
//! stay the reader's; a count the reading does not keep is null, never 0.

use serde::Serialize;

use super::{INSTRUMENT, numbers};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count};

/// The version of [`Row`]'s shape, which changes only when the shape does
/// and never for a new reading; a reader refuses a `v` it does not know
/// (harness-design.md §4.6, revision 76). A field a row may omit, whose
/// absence a reader of the earlier shape already handles correctly, is
/// added under the same `v`; a field removed, renamed or retyped, or a
/// meaning changed, is a new `v`.
pub(crate) const V: u32 = 1;

/// One budget reading: a crate's lines against its ceiling, `scripts/**`'s,
/// the workspace's pools against `[budget]`, or a generated file's verdict,
/// which counts nothing. The field order is the line's.
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
    /// A crate's binding lines, counted apart; null where it has none.
    bindings: Option<u64>,
    /// The tracked files `scripts/**` counted; null for every other
    /// reading.
    files: Option<u64>,
    /// The workspace's pools, on its reading alone: the one reading that
    /// counts a total.
    workspace: Option<Workspace>,
}

/// The workspace's pools, each against its `[budget]` ceiling; a pool a
/// reading from before it was counted does not keep is null.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Workspace {
    /// Every counted line, bindings with them, against `[budget] total`.
    total: Pool,
    bindings: Option<Pool>,
    docs: Option<Pool>,
    /// The workload programs, where `[budget] workloads` holds them apart
    /// from `docs`.
    workloads: Option<Pool>,
    harness: Option<Pool>,
    lock_packages: Option<Pool>,
    /// `[budget] new`, stated and not yet measured.
    new_ceiling: Option<u64>,
}

/// One pool's count and the ceiling the manifest holds it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Pool {
    n: u64,
    ceiling: Option<u64>,
}

impl PerReading for Row {
    const INSTRUMENT: &'static str = INSTRUMENT;

    /// What one reading counted, each count under the key its writer keeps
    /// it by ([`numbers`]); the workspace's `bindings` is its pool, not a
    /// crate's.
    fn of(reading: &Reading) -> Row {
        let pool = |name: &str| {
            count(reading, name).map(|n| Pool {
                n,
                ceiling: count(reading, &numbers::ceiling_of(name)),
            })
        };
        let head = Head::of(reading, V);
        if let Some(total) = pool(numbers::TOTAL) {
            let workspace = Workspace {
                total,
                bindings: pool(numbers::BINDINGS),
                docs: pool(numbers::DOCS),
                workloads: pool(numbers::WORKLOADS),
                harness: pool(numbers::HARNESS),
                lock_packages: pool(numbers::LOCK_PACKAGES),
                new_ceiling: count(reading, numbers::NEW_CEILING),
            };
            return Row {
                head,
                lines: None,
                ceiling: None,
                bindings: None,
                files: None,
                workspace: Some(workspace),
            };
        }
        Row {
            head,
            lines: count(reading, numbers::LINES),
            ceiling: count(reading, numbers::CEILING),
            bindings: count(reading, numbers::BINDINGS),
            files: count(reading, numbers::FILES),
            workspace: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Pool, Row};
    use crate::reading::Reading;
    use crate::status::{PerReading, every};

    /// A reading of `scope` with the counts named.
    fn reading(scope: &str, counts: &[(&str, i64)]) -> Reading {
        every::reading(super::INSTRUMENT, scope, counts)
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
            (ffi.lines, ffi.ceiling, ffi.bindings, ffi.workspace),
            (Some(8582), Some(14000), Some(120), None)
        );
        let workspace = Row::of(&reading(
            "workspace",
            &[
                ("total", 411_160),
                ("total_ceiling", 440_000),
                ("bindings", 26027),
                ("bindings_ceiling", 32000),
                ("workloads", 1562),
                ("workloads_ceiling", 2500),
            ],
        ));
        let pools = workspace.workspace.unwrap();
        assert_eq!(
            (
                workspace.lines,
                workspace.bindings,
                pools.total,
                pools.bindings
            ),
            (
                None,
                None,
                Pool {
                    n: 411_160,
                    ceiling: Some(440_000)
                },
                Some(Pool {
                    n: 26027,
                    ceiling: Some(32000)
                })
            )
        );
        assert_eq!(pools.docs, None, "a pool the reading does not keep");
        assert_eq!(
            pools.workloads,
            Some(Pool {
                n: 1562,
                ceiling: Some(2500)
            }),
            "the workload programs, held apart from docs"
        );
        let uncreated = Row::of(&reading("php_jit", &[("ceiling", 9000)]));
        assert_eq!((uncreated.lines, uncreated.ceiling), (None, Some(9000)));
        let generated = serde_json::to_value(Row::of(&reading("ext/clippy.toml", &[]))).unwrap();
        assert_eq!(
            (
                &generated["v"],
                &generated["lines"],
                &generated["workspace"],
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
