//! `xtask permit`, the judge at the hook's call (harness-design.md §4.4). It
//! reads one intended call as the `PreToolUse` hook receives it, evaluates
//! the region-body rule and then every manifest rule that describes a write
//! or a command, in the manifest's order, and prints the hook's JSON: a deny
//! with the rule's text and ADR, an allow whose `additionalContext` carries
//! the notes, or nothing. Exit 0 whatever it decides, since the decision is
//! in the JSON; the one non-zero exit is the refusal to decide at all, a
//! build meeting a manifest it was not compiled against (`built`, #16),
//! which is what each adapter walks its next candidate on. A worktree's
//! manifest is not met that way: a call there is handed to the worktree's
//! own build, whose decision is this one's (#1047). Every denial,
//! every note and every refusal is a reading with `instrument = "permit"`,
//! and a refusal that hands the call on is a skip, since the candidate
//! that rebuilds judges it (#118).
//!
//! What permit judges: `deny`, `deny_argv`, `deny_pattern`, `deny_types`
//! (with the `deny_macros` beside it), `allow` and `on`. The other five
//! predicates (`deny_methods`, `check`, `deny_lints`, `avoid_from`,
//! `generated`) are sensors
//! that read the tree after the write and have no meaning for one call;
//! their twins run on save and per commit (§6). A Bash line is read as the
//! argv of each simple command it runs, quotes removed and heredoc bodies
//! skipped, in [`argv`] (#1001), and a `deny_pattern` reads the same line
//! less the heredoc bodies that reading skips as text (#1832); what a shell
//! can hide beyond that is `done`'s to find at pre-push and in CI.
//!
//! The interface is four things: [`judge`], one call against the manifest;
//! [`run`], the command each adapter is over; [`after`], the `PostToolUse`
//! hook's call, which puts back a file of `harness.self`'s that a `Bash`
//! call moved; and [`judge_files`], the diff judge that `done` and
//! pre-commit's `--diff` read one verdict from. Each
//! is one file under this one, with the call and its path resolution in
//! [`call`], the rules in [`rules`], a shell line's argv in [`argv`], the
//! region-body rule in [`region`],
//! the sensor over `harness.self`'s files around a `Bash` call in
//! [`after`], the changes it kept, which `done` holds, in [`kept`], the
//! person's approval that lifts `harness.self` from one
//! change in [`approval`], the audit record of each change it admitted in
//! [`audit`], whether a line can write one of them in [`writes`], the
//! `unless` qualifier's sensor in [`unless`], the `exempt_label`
//! qualifier's in [`label`], the operation in progress in a checkout and
//! whose it is, which `where = "operating"` reads, in [`operation`], the
//! adapters and the
//! recording in [`adapters`], the call handed to a worktree's own build in
//! [`hand_off`], the diff judge in [`diff`] and the bench of
//! §4.4's cost bar in [`bench`]. The ids a reading
//! carries and the predicate `status` renders the rules by are here, since
//! the fixture test and the `permits` section read them.

mod adapters;
mod after;
mod approval;
mod argv;
mod audit;
mod bench;
mod call;
mod diff;
mod hand_off;
mod kept;
mod label;
mod ledger;
mod operation;
mod region;
mod rules;
mod unless;
mod writes;

pub(crate) use adapters::PRE_COMMIT;
pub use adapters::{Adapter, run};
pub use after::after;
pub(crate) use argv::Element;
pub(crate) use bench::{BAR_MS, run_bench, scratch_workspace};
pub use call::{Arguments, Call};
pub(crate) use diff::judge_files;
pub(crate) use diff::{run_diff, working_files};
pub(crate) use hand_off::walk;
pub(crate) use kept::kept_changes;
pub(crate) use ledger::{judged, written};
pub use rules::{Finding, Judgement, Kind, judge, judged_at_the_call};

/// The instrument name every reading of permit carries.
pub(crate) const INSTRUMENT: &str = "permit";
/// The region-body rule is computed's ADR 0005, not a manifest rule; this
/// is the id its readings and denials carry.
pub(crate) const REGION_RULE: &str = "computed.region-body";
/// A build refusing to judge by a manifest it was not compiled against is
/// permit's own, not a manifest rule either; this is the id its readings
/// carry (#16). The reading is a skip: the call went to the candidate that
/// rebuilds, so nothing was judged here and nothing went unjudged (#118).
pub(crate) const STALE_RULE: &str = "permit.stale-build";
/// The refusal that no rebuild repairs, a manifest no build of this
/// harness reads: the call reaches the end of the walk unjudged, which is
/// the outage of #16, and the reading fails under this id (#118).
pub(crate) const UNJUDGED_RULE: &str = "permit.unjudged-call";
