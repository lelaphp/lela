//! `xtask permit`, the judge at the tool call (harness-design.md §4.4). It
//! reads one intended tool call as the `PreToolUse` hook receives it, evaluates
//! the region-body check and then every manifest rule that describes a write
//! or a command, in the manifest's order, and prints the hook's JSON: a deny
//! with the rule's text and ADR, an allow whose `additionalContext` carries
//! the notes, or nothing. Exit 0 whatever it decides, since the decision is
//! in the JSON; the one non-zero exit is the refusal to decide at all, a
//! build meeting a manifest it was not compiled against (`built`, #16),
//! which is what each adapter walks its next candidate on. Every denial,
//! every note and every refusal is a reading with `instrument = "permit"`.
//!
//! What permit judges: `deny`, `deny_argv`, `deny_pattern`, `deny_types`,
//! `allow` and `on`. The other four predicates (`check`, `deny_lints`,
//! `avoid_from`, `generated`) are sensors that read the tree after the write
//! and have no meaning for one call; their twins run on save and per commit
//! (§6). Bash arguments are split conservatively on unquoted whitespace and
//! `;`, `&&`, `|`; pre-commit remains the sensor of record for what a shell
//! can hide.
//!
//! The interface is three things: [`judge`], one call against the manifest;
//! [`run`], the command each adapter is over; and [`judge_files`], the diff
//! judge that `done` and pre-commit's `--diff` read one verdict from. Each
//! is one file under this one, with the call and its path resolution in
//! [`call`], the rules in [`rules`], the region-body check in [`region`],
//! the adapters and the recording in [`adapters`], the diff judge in
//! [`diff`] and the bench of §4.4's cost bar in [`bench`]. The ids a reading
//! carries and the predicate `status` renders the rules by are here, since
//! the fixture test and the `permits` section read them.

mod adapters;
mod bench;
mod call;
mod diff;
mod region;
mod rules;

pub use adapters::{Adapter, run};
pub(crate) use bench::run_bench;
pub use call::{Call, ToolInput};
pub(crate) use diff::judge_files;
pub(crate) use diff::{run_diff, working_files};
pub use rules::{Finding, Judgement, Kind, judge, judged_at_the_tool_call};

/// The instrument name every reading of permit carries.
pub(crate) const INSTRUMENT: &str = "permit";
/// The region-body check is computed's ADR 0005, not a manifest rule; this
/// is the id its readings and denials carry.
pub(crate) const REGION_RULE: &str = "computed.region-body";
/// A build refusing to judge by a manifest it was not compiled against is
/// permit's own, not a manifest rule either; this is the id its readings
/// carry (#16).
pub(crate) const STALE_RULE: &str = "permit.stale-build";
