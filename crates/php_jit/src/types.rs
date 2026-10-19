//! Trace SSA over a record (`jit-design.md` §4.2), so far the number of a
//! trace's exit, [`ExitId`], which a side trace's start names. `build`,
//! `TraceIr`, `ExitSpec` and `Landing` land with the recorder's records.
/// One exit of a trace, by its place among the trace's exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExitId(pub(crate) u16);
