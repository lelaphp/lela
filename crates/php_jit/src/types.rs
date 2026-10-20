//! The trace SSA (`jit-design.md` §4.2): [`build`] reads a
//! [`TraceRecord`] through the [`UnitView`] it was recorded with and
//! writes a [`TraceIr`], the value `lower` compiles and `lower::dump`
//! renders. Pure: a record and a view in, a trace or an [`Abort`] out,
//! with no request, no engine and nothing shared written, so it goes on the compiler's
//! thread.
//!
//! What it is, php's `zend_jit_trace_build_tssa`
//! (`Z/ext/opcache/jit/zend_jit_trace.c:1319-2800`) over this engine's
//! record:
//!
//! - **Values over frame slots.** Every slot of every frame the trace
//!   touches holds one [`Value`] at a time, a new one each time an op
//!   writes it; what a slot held as the trace entered is read where the
//!   trace first reads it. A trace that loops has a phi at its header for
//!   every slot its body reads before it writes, php's
//!   `zend_jit_trace_add_phis`, `_call_phis` and `_ret_phis`.
//! - **php's guard set.** Where an op reads a value whose recorded type
//!   is narrower than what the op array alone implies, a [`Guard`] holds
//!   it there, at the operands php's switch names
//!   (`zend_jit_trace_add_op_guard` and its callers): a type, a packed
//!   array, a reference's inner type, a conditional jump's direction, the
//!   caller a return lands in. A guard on what the trace entered with is
//!   held once at entry, and a loop's phi guard moves to its entry value
//!   where the back edge already has the type, as php's "propagate
//!   guards through Phi sources" does.
//! - **Aliasing.** A variable a symbol table reaches, every variable of a
//!   file's own code and of a function that reads its variables by name,
//!   is kept at guard strength: guarded at every op that reads it, never
//!   narrowed past it, as php's `zend_jit_var_may_alias` keeps it.
//! - **`SMART_BRANCH`.** A comparison fused to the jump after it is one
//!   compare-and-branch, whose mispredicted direction leaves through an
//!   exit at the jump with the comparison's value written as a constant.
//! - **The clobber rule.** After a kernel whose opcode carries the `call`
//!   effect in `ops.toml`, every value whose facts came from memory other
//!   code can write, a slot a symbol table reaches or one that may hold a
//!   reference, is read again as a new value; only what the trace owns
//!   survives. A kernel with the `throw` effect alone clobbers nothing but
//!   may exit. An array the trace owns keeps its layout across a call:
//!   no other code writes it without separating it first.
//! - **An [`ExitSpec`] per exit**, naming the frame and opline the
//!   interpreter goes on at, the [`Landing`], the slots whose values live
//!   in registers and are stored, and the trace a `Link` stop links to.
//!   Places that leave with an equal spec naming an opline share one
//!   exit, php's reuse in `zend_jit_trace_get_exit_point`.
//!
//! What it is not: php's ranges and reference counts. A long arithmetic is
//! long or double here where php's range inference proves it stays long,
//! so it carries a guard php's does not; and what a loop variable written
//! before its loop holds is guarded once at entry where php's static
//! inference over the whole function already knew it.

mod build;
pub(crate) mod lattice;
mod opcodes;
#[cfg(test)]
mod tests;

use php_engine::tier::{FuncRef, MapPtrRaw, UnitView};
use php_heap::ZvalType;
use php_unit::{ConstIdx, Opcode, OplineIdx};

pub(crate) use lattice::Types;

use crate::record::{Start, Stop, TraceRecord};
use crate::table::TraceId;

/// One exit of a trace, by its place among the trace's exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExitId(pub(crate) u16);

/// The trace a record builds: the frames it runs in, its values, its ops
/// in the order they are served, the guards and exits between them, the slots
/// of the requests' runtime caches its code reads, and how it ends.
#[derive(Debug, Clone)]
pub(crate) struct TraceIr {
    pub(crate) entry: Entry,
    pub(crate) frames: Vec<FrameInfo>,
    pub(crate) values: Vec<Value>,
    pub(crate) ops: Vec<Op>,
    pub(crate) guards: Vec<Guard>,
    pub(crate) exits: Vec<ExitSpec>,
    pub(crate) map_ptrs: Vec<MapPtrRaw>,
    pub(crate) end: End,
}

/// Where the trace was recorded from and why the recording stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) start: Start,
    pub(crate) root: FuncRef,
    pub(crate) stop: Stop,
}

/// A frame of the trace, by its place in [`TraceIr::frames`]; the first
/// is the one the trace entered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Frame(pub(crate) u16);

impl Frame {
    /// The frame the trace entered in.
    pub(crate) const ROOT: Frame = Frame(0);
}

/// One frame the trace runs in: the op array it serves, its slots, and
/// how it stands to the trace's other frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameInfo {
    pub(crate) func: FuncRef,
    pub(crate) place: Place,
    /// How many of its slots are variables; the temporaries follow.
    pub(crate) cvs: u32,
    pub(crate) temps: u32,
}

/// How a frame stands to the trace's others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// The frame the trace entered in.
    Root,
    /// A frame the trace pushed: a call from the frame named.
    Callee(Frame),
    /// The frame below the one named, which a return of it landed in.
    Caller(Frame),
}

/// One slot of one frame: a variable, numbered as php numbers it, or a
/// temporary numbered past the variables (`EX_VAR_TO_NUM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Slot {
    pub(crate) frame: Frame,
    pub(crate) var: u32,
}

/// An op of the trace, by its place in [`TraceIr::ops`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct OpId(pub(crate) u32);

impl OpId {
    /// The op's place in [`TraceIr::ops`].
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// A value of the trace, by its place in [`TraceIr::values`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ValueId(pub(crate) u32);

/// What one slot holds from where it is written until it is written
/// again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Value {
    pub(crate) slot: Slot,
    pub(crate) types: Types,
    pub(crate) def: Def,
    pub(crate) home: Home,
}

/// Where a value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Def {
    /// The slot as it stood when the trace, or the frame's part of it,
    /// began: read from memory where the trace first reads it.
    Entry,
    /// A loop's header: the value entering it, and the value its back
    /// edge brings.
    Phi { entry: ValueId, back: ValueId },
    /// Written by the op.
    Op(OpId),
    /// Read again after the call-effect kernel at that place, the clobber
    /// rule.
    Reload(OpId),
}

/// Where a value lives between the ops that read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Home {
    /// In its slot, where the interpreter reads it.
    Memory,
    /// In a register: a scalar the trace computed and owns, which an exit
    /// stores before it leaves.
    Register,
}

/// One thing the trace does, in the order the trace does it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    /// One recorded opline, or a comparison and the jump fused to it.
    Line(Line),
    /// A call an `INIT_*` began in `frame`, to the op array named, none
    /// for a native's.
    InitCall {
        frame: Frame,
        target: Option<FuncRef>,
    },
    /// The frame a call pushed runs.
    Enter { frame: Frame },
    /// A return lands in `frame`, and `out` is what the call's slot holds
    /// after it, where the call keeps one.
    Back { frame: Frame, out: Option<ValueId> },
}

/// One recorded opline: where it stands, what it reads and writes, how
/// it is compiled, and where it leaves the trace when it cannot finish.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Line {
    pub(crate) frame: Frame,
    pub(crate) opline: OplineIdx,
    pub(crate) opcode: Opcode,
    pub(crate) op1: Operand,
    pub(crate) op2: Operand,
    /// The `OP_DATA` after an assignment, its `op1`.
    pub(crate) data: Operand,
    /// What the opline writes: its `out` slot, a variable `op1`, a
    /// variable or the value slot of a `foreach` in `op2`.
    pub(crate) out: Option<ValueId>,
    pub(crate) op1_def: Option<ValueId>,
    pub(crate) op2_def: Option<ValueId>,
    /// The jump one opline on that a comparison is fused to, one
    /// compare-and-branch (design §5.8), where it is.
    pub(crate) fused: Option<Fused>,
    /// What the opline names beside its operands that its inline form
    /// reads.
    pub(crate) imm: Imm,
    pub(crate) form: Form,
    /// The slot of [`TraceIr::map_ptrs`] the opline reads.
    pub(crate) map_ptr: Option<usize>,
    pub(crate) exit: Option<ExitId>,
    /// Where it leaves where, done, freeing a temporary it read threw:
    /// an inline fetch from a temporary array.
    pub(crate) freed: Option<ExitId>,
}

/// The jump a comparison is fused to: which value of the comparison it
/// jumps on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fused {
    /// `JMPZ`: it jumps where the comparison is false.
    Jmpz,
    /// `JMPNZ`: it jumps where the comparison is true.
    Jmpnz,
}

/// What an opline names beside its operands, where its inline form reads
/// it: php's `op2.num` of a send, an `UNUSED` operand that is a number,
/// and the operator an `ASSIGN_OP`'s `extended_value` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Imm {
    None,
    /// The argument a send writes, from one.
    Arg(u32),
    /// The binary operator of an `ASSIGN_OP`.
    Binary(Opcode),
}

/// An operand as the op reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Operand {
    Unused,
    Constant(Constant),
    Value(ValueId),
}

/// A constant operand: a scalar by its value, anything else by its place
/// in the op array's table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Constant {
    Null,
    False,
    True,
    Long(i64),
    Double(f64),
    Other(ConstIdx),
}

/// How an opline is compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    /// In compiled code: the scalar core over the types its guards hold.
    Inline,
    /// Through `step`, the interpreter's own handler (`jit-design.md`
    /// §5.2), over values in their slots.
    Step,
}

/// A guard: what it holds, where, and the exit it takes when it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Guard {
    pub(crate) at: GuardAt,
    pub(crate) check: Check,
    pub(crate) exit: ExitId,
}

/// Where a guard is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardAt {
    /// Once, as the trace enters, before a loop's header.
    Entry,
    /// Before the op reads the value.
    Before(OpId),
    /// By the op, on what it computes, before it writes
    /// it: an arithmetic's overflow, a fetched element's type, a jump's
    /// direction, a table's target, a return's caller.
    Within(OpId),
}

/// What a guard holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    /// The value is of this one type, which is no reference.
    Type { value: ValueId, tag: ZvalType },
    /// The slot holds a reference, and what it refers to is of this type.
    Deref { value: ValueId, tag: ZvalType },
    /// The array is laid out so: php's packed guard.
    Layout { value: ValueId, layout: Layout },
    /// The jump goes the way it went.
    Direction { taken: bool },
    /// A table's jump, `SWITCH_LONG`, `SWITCH_STRING` or `MATCH`, goes to
    /// the opline the trace serves next, as php's `zend_jit_switch` leaves
    /// through an exit for each target the trace does not follow. Its exit
    /// lands where the jump left the frame, so it is right whichever way
    /// the jump went.
    Table,
    /// The return lands in this op array at this opline, php's IP guard.
    Caller { func: FuncRef, opline: OplineIdx },
}

/// How an array is laid out, php's `HASH_FLAG_PACKED` set or clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    Packed,
    Hash,
}

/// What an exit does: where the interpreter goes on, what the trampoline
/// does first, what compiled code stores, and the trace it links to.
///
/// It is all an exit is: two places that leave with an equal spec that
/// names an opline leave through one exit, as php's
/// `zend_jit_trace_get_exit_point` reuses one whose opline, `flags` and
/// stack map match (`Z/ext/opcache/jit/zend_jit_trace.c:168-185`). php
/// never reuses a method call's exit, `ZEND_JIT_EXIT_METHOD_CALL`, which
/// keeps the callee it guarded for a side trace; no guard here holds a
/// method call's callee, so no exit is one. php's stack map also holds
/// each slot's known type, and its `flags` `ZEND_JIT_EXIT_TO_VM`, which
/// a spec does not carry until side traces read them (#2964), so entry
/// guards and a loop's interrupt test share exits php keeps apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ExitSpec {
    pub(crate) frame: Frame,
    /// The opline the interpreter goes on at; none where the frame
    /// already stands where it goes on, as php's exit with no opline
    /// does after a return to a caller its guard did not expect.
    pub(crate) opline: Option<OplineIdx>,
    pub(crate) landing: Landing,
    pub(crate) stores: Vec<Store>,
    pub(crate) link: Option<TraceId>,
}

/// One slot an exit writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Store {
    pub(crate) slot: Slot,
    pub(crate) from: Stored,
}

/// What an exit writes to a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Stored {
    /// A value the trace holds.
    Value(ValueId),
    /// A boolean the exit knows: a mispredicted comparison's.
    Bool(bool),
    /// A fetched element whose type its guard did not hold, which the
    /// guard's own place copies into the fetch's slot as the fetch copies
    /// it before it leaves: the slot is written, and the exit stores
    /// nothing there. No other place leaves with it, as php's stack map
    /// keeps that slot in memory where the next opline's keeps it in a
    /// register.
    Copied(ValueId),
}

/// What the trampoline does at an exit before the interpreter goes on
/// (the glossary's Landing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Landing {
    Plain,
    /// The opline before the exit's read these temporaries, and compiled
    /// code has not freed them yet.
    FreeTemps {
        op1: bool,
        op2: bool,
    },
    /// A call an `INIT_*` began is under way, and is restored.
    RestoreCall,
}

/// How the trace ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// It goes back to its header.
    Loop(LoopHeader),
    /// It leaves through this exit.
    Leave(ExitId),
}

/// Where a looping trace goes back to, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoopHeader {
    /// The op the back edge goes to.
    pub(crate) at: OpId,
    pub(crate) repeat: Repeat,
    /// Where the interrupt test leaves (`jit-design.md` §4.3).
    pub(crate) interrupt: ExitId,
}

/// What a looping trace repeats: php's three stops that make a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Repeat {
    /// A loop of one frame, `ZEND_JIT_TRACE_STOP_LOOP`.
    Loop,
    /// A call into its own root, each time one frame deeper,
    /// `ZEND_JIT_TRACE_STOP_RECURSIVE_CALL`.
    Call,
    /// A return into a caller of the root's op array, each time one
    /// frame shallower, `ZEND_JIT_TRACE_STOP_RECURSIVE_RET`.
    Return,
}

/// Why a record builds no trace: the stop it counts as, and in what way
/// the record did not fit (`jit-design.md` §4.2). A record that stopped
/// on a failed attempt is refused with its own stop; one with more exits
/// than php's `ZEND_JIT_TRACE_MAX_EXITS`, once those that match share
/// one, with `TooManyExits`; one that
/// does not fit the op arrays its view holds is php's `COMPILER_ERROR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Abort {
    pub(crate) stop: Stop,
    pub(crate) why: &'static str,
}

/// The trace `rec` builds, read through `view`, or why it builds none.
pub(crate) fn build(rec: &TraceRecord, view: &UnitView) -> Result<TraceIr, Abort> {
    build::build(rec, view)
}
