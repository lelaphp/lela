//! What becomes of a [`TraceIr`](crate::types::TraceIr) (`jit-design.md`
//! §4.3): the seam is the value, and two modules read it. [`dump`]
//! renders it as text, which `types` is held to before code is made; the
//! `Backend` that compiles it to final bytes lands with #2955, and what it
//! makes is a [`Compiled`], which `table` publishes.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the pipeline renders a trace under jit_debug with #2958"
    )
)]

pub(crate) mod dump;

use php_engine::tier::{FuncRef, MapPtrRaw};
use php_unit::OplineIdx;

use crate::table::TraceName;
use crate::types::ExitSpec;

/// A trace in final bytes (`jit-design.md` §4.3), what the `Backend` makes
/// and `table::TraceTable::publish` copies into the code arena once:
/// the bytes, where they are entered, the alignment their first byte
/// needs, one [`ExitSpec`] per exit in [`ExitId`](crate::types::ExitId)
/// order, the runtime-cache slots the code reads, where each recorded
/// opline's code begins, and what a profiler names the trace by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Compiled {
    pub(crate) bytes: Vec<u8>,
    /// The offset in `bytes` the trace is entered at.
    pub(crate) entry_offset: usize,
    /// A power of two.
    pub(crate) align: usize,
    pub(crate) exits: Box<[ExitSpec]>,
    /// The slots of the requests' runtime caches the code reads, which
    /// `enter` lays down on a request's first entry.
    pub(crate) map_ptrs: Box<[MapPtrRaw]>,
    pub(crate) marks: Box<[Mark]>,
    /// php's `zend_jit_trace_name` but the number, which the table
    /// assigns: the job's, named where the trace's `TRACE start` line is.
    pub(crate) name: TraceName,
}

/// Where one recorded opline's code begins: the op array, named with
/// its function since a trace inlines calls, the opline, and the offset
/// in the trace's bytes. What a profiler and a disassembly read; it costs
/// nothing in the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) func: FuncRef,
    pub(crate) opline: OplineIdx,
    pub(crate) offset: u32,
}
