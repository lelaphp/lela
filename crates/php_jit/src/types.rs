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
//!   where the back edge already has the type, or where an inline
//!   arithmetic made what it brings and holds it long within itself,
//!   rerun where it overflows, as php's "propagate guards through Phi
//!   sources" does with `is_checked_guard`. What an opline writes is typed by
//!   what php's inference knows of its operands, so where it reads a
//!   variable a symbol table reaches, which php types as anything, the
//!   opline that reads what it wrote holds it, as php's does.
//! - **Aliasing.** A variable a symbol table reaches, every variable of a
//!   file's own code and of a function that reads its variables by name,
//!   is kept at guard strength: guarded at every op that reads it, never
//!   narrowed past it, as php's `zend_jit_var_may_alias` keeps it, and at
//!   the oplines php's code generator holds an operand's recorded type
//!   at where its inference adds no guard, a concatenation's and a rope
//!   part's, once a guard of the inference holds the same value; an
//!   array of one is held to its layout at each read by a long key.
//! - **`SMART_BRANCH`.** A comparison fused to the jump after it is one
//!   compare-and-branch, whose mispredicted direction leaves through an
//!   exit at the opline the jump goes to the other way, as php's lands
//!   there, the comparison's value dead past the jump.
//! - **The clobber rule.** After a kernel whose opcode carries the `call`
//!   effect in `ops.toml`, every value whose facts came from memory other
//!   code can write, a slot a symbol table reaches or one that may hold a
//!   reference, is read again as a new value; only what the trace owns
//!   survives. A kernel with the `throw` effect alone clobbers nothing but
//!   may unwind. An array the trace owns keeps its layout across a call:
//!   no other code writes it without separating it first.
//! - **An [`ExitSpec`] per exit**, naming the frame and opline the
//!   interpreter goes on at, the [`Landing`], the slots whose values live
//!   in registers and are stored, the slots whose types the trace knows
//!   there, whether a side trace may grow from it, and the trace a `Link`
//!   stop links to. Places that leave with an equal spec naming an opline
//!   share one exit, php's reuse in `zend_jit_trace_get_exit_point`.
//! - **php's `RECV` exits first.** An entered trace of a function with
//!   typed parameters numbers the exits php's `zend_jit_recv` takes in
//!   its prologue ahead of its own, though the prologue ran before
//!   `Site::Entry` and no code tests them, so every later exit carries
//!   php's number (#3269).
//! - **No exit for a throw.** A kernel that leaves a throwable pending,
//!   bails out or panics leaves through an [`Unwind`], which stores the
//!   registers and is numbered apart from the exits, as php's compiled
//!   code jumps to its stub for a throwable and takes no exit point
//!   (`zend_jit_check_exception`, `Z/ext/opcache/jit/zend_jit_ir.c:3985`).
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

use std::num::{NonZeroU32, NonZeroUsize};

use php_engine::api::abi::EngineState;
use php_engine::tier::{
    EpochClassId, EpochFunction, FramelessAt, FuncRef, MapPtrRaw, PlainCall, UnitView,
};
use php_heap::ZvalType;
use php_ssa::Range;
use php_unit::{ConstIdx, Opcode, OplineIdx};

pub(crate) use lattice::Types;

use crate::analysis::Analysed;
use crate::record::{Began, Callee, Start, Stop, TraceRecord};
use crate::table::TraceId;

/// One exit of a trace, by its place among the trace's exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EngineState)]
pub(crate) struct ExitId(pub(crate) u16);

/// One unwind of a trace, by its place among the trace's unwinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct UnwindId(pub(crate) u16);

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
    pub(crate) unwinds: Vec<Unwind>,
    pub(crate) map_ptrs: Vec<MapPtrRaw>,
    pub(crate) end: End,
    /// Where a loop's trace is entered from its back edge, where that
    /// lands past its header ([`BodyEntry`]): none for a trace entered at
    /// its first op alone.
    pub(crate) body: Option<BodyEntry>,
}

/// The second way into a trace rooted at a loop's condition, php's header
/// (`jit-design.md` §4.1, revision 53, as built until a person rules
/// on #3178), whether it closes the loop or leaves it: at the opline the
/// loop's back edge lands on, the body of a `for` or a `while`. The engine's
/// back edge stands the frame there, past the condition the interpreter
/// just served; entering at the body takes the trace's ops from `at` on
/// with what the frame holds, each value the trace keeps there read from
/// its slot and held to what the trace knows of it. A frame holding
/// anything else declines the entry, taking no exit, and `dispatch`
/// steps on to the condition and enters there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BodyEntry {
    /// The op the entry goes on at.
    pub(crate) at: OpId,
    /// Its opline, in the frame the trace entered in, where the frame
    /// stands.
    pub(crate) opline: OplineIdx,
    /// What each slot of that frame holds there, the trace's value of it.
    pub(crate) holds: Vec<(Slot, ValueId)>,
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
    /// Where the op array's table of constants begins, the address of its
    /// first zval, which the trace's epoch holds: where an inline form
    /// reads a constant no scalar is from, php's `RT_CONSTANT`.
    pub(crate) literals: usize,
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
    /// What php's inference knows of the long it holds, where it knows.
    pub(crate) range: Option<Range>,
    pub(crate) def: Def,
    pub(crate) home: Home,
    /// What it holds before the guard that narrowed `types`, where that
    /// guard is held after the op that made it.
    pub(crate) unguarded: Option<Unguarded>,
}

/// A value as the ops before the first guard on it read it: `types` from
/// where it was made, which a guard held before `until` goes on to narrow.
/// `until`, not `types`, says where the narrowing comes true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unguarded {
    pub(crate) until: OpId,
    pub(crate) types: Types,
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

/// What holds the VM stack's top page to the room an `INIT_*` pushed in
/// place takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushRoom {
    /// A stack check, the push's own or one before it that covers it
    /// ([`Check::Room`]), as `zend_jit_push_call_frame` trusts php's
    /// `checked_stack`: the push tests nothing.
    Checked,
    /// No stack check holds this push, one whose callee the record names
    /// no size of, so it tests the page itself.
    Unchecked,
}

/// One thing the trace does, in the order the trace does it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    /// One recorded opline, or a comparison and the jump fused to it.
    Line(Line),
    /// A call an `INIT_*` began in `frame`, to `callee`: at an opline of
    /// the trace, or before it reached `frame`, php's fake init, whose
    /// call compiled code finds where the interpreter stands, `EX(call)`,
    /// once the calls begun after it are made. `plain` where compiled code
    /// pushes it in place, as php's `zend_jit_push_call_frame` does for a
    /// recorded `func` it needs no guard on: an `INIT_FCALL` of a user
    /// function of the engine's arena, or a recursive call
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:8790-8906`), whose push, the
    /// `DO_*CALL` that makes it and the callee's `RETURN` take no exit;
    /// `room` says what holds its page to the room it takes.
    InitCall {
        frame: Frame,
        callee: Callee,
        began: Began,
        plain: Option<PlainCall>,
        room: PushRoom,
    },
    /// The frame a call pushed runs. `stores` are the values the caller
    /// holds in registers, written to their slots as the call enters, so
    /// that what reads the caller's frame where the trace does not, a
    /// recursion's next turn, the interpreter past a recursive call or a
    /// return trace, reads them there: php keeps no register across a call
    /// into a frame its trace inlines (`zend_jit_trace_allocate_registers`,
    /// `Z/ext/opcache/jit/zend_jit_trace.c:3169-3180`).
    Enter { frame: Frame, stores: Vec<ValueId> },
    /// A return lands in `frame`, and `out` is what the call's slot holds
    /// after it, where the call keeps one. A `RETURN` php compiles with
    /// `zend_jit_return` past the trace's lowest frame `escapes` unless
    /// the interpreter stands at that opline of that op array after it,
    /// php's IP guard in `zend_jit_leave_func`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11188-11224`): no exit, the frame
    /// left to the handler of the opline it stands at
    /// ([`crate::lower::ESCAPED`]). A return php hands to its handler
    /// escapes nowhere: its line holds that opline with a `Check::Stands`
    /// and an exit, as php's `zend_jit_trace_handler` does.
    Back {
        frame: Frame,
        out: Option<ValueId>,
        escapes: Option<(FuncRef, OplineIdx)>,
    },
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
    /// Where it leaves when it cannot finish: an inline form the
    /// interpreter serves again.
    pub(crate) exit: Option<ExitId>,
    /// Where it leaves when a kernel it calls throws, bails out or
    /// panics: a stepped opline that may throw, an inline assignment over
    /// a counted value, whose destructor may, and an inline fetch from a
    /// temporary array, which it frees.
    pub(crate) unwind: Option<UnwindId>,
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

/// What a call made in place checks of its arguments before its
/// activation passes over the `RECV`s that would check them, as php's
/// trace compiles `zend_jit_recv` with the check inline where its stack
/// map does not know the argument of a type the parameter takes
/// (`Z/ext/opcache/jit/zend_jit_ir.c:10825-10873`): per argument, from
/// the first, the types its `RECV` takes untouched, php's `op2.num`, one
/// bit per `IS_*` type, held to what the argument holds, through a
/// reference where it is one; `None` where the trace knows the argument
/// of such a type or the parameter checks none. A call with a check past
/// [`ArgChecks::ARGS`] is the call kernel's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ArgChecks([Option<NonZeroU32>; ArgChecks::ARGS]);

impl ArgChecks {
    /// The arguments a call checks in place at most.
    pub(crate) const ARGS: usize = 8;

    /// These checks with argument `at`, from zero, held to `mask`; none
    /// past [`ArgChecks::ARGS`].
    pub(crate) fn with(mut self, at: usize, mask: NonZeroU32) -> Option<ArgChecks> {
        *self.0.get_mut(at)? = Some(mask);
        Some(self)
    }

    /// Each argument checked, from zero, and the mask it is held to.
    pub(crate) fn each(self) -> impl Iterator<Item = (u32, NonZeroU32)> {
        (0u32..)
            .zip(self.0)
            .filter_map(|(at, mask)| Some((at, mask?)))
    }
}

/// What a call made in place copies into the parameters it is passed no
/// argument for, as php's trace compiles `zend_jit_recv_init` past the
/// arguments its frame knows were passed (`jit_ZVAL_COPY_CONST`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:10886-10950`): per parameter, from
/// the first, the address of its `RECV_INIT`'s default among the op
/// array's constants, which the trace's epoch holds, a constant no
/// reference counts; `None` for one passed. A call with a default that
/// names a constant expression, is counted, or stands past
/// [`ArgChecks::ARGS`] is the call kernel's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Defaults([Option<NonZeroUsize>; ArgChecks::ARGS]);

impl Defaults {
    /// These defaults with parameter `at`, from zero, given the constant
    /// at `address`; none past [`ArgChecks::ARGS`].
    pub(crate) fn with(mut self, at: usize, address: NonZeroUsize) -> Option<Defaults> {
        *self.0.get_mut(at)? = Some(address);
        Some(self)
    }

    /// Each parameter given its default, from zero, and the address of
    /// the constant copied into it.
    pub(crate) fn each(self) -> impl Iterator<Item = (u32, NonZeroUsize)> {
        (0u32..)
            .zip(self.0)
            .filter_map(|(at, address)| Some((at, address?)))
    }
}

/// What an opline names beside its operands, where its inline form reads
/// it: php's `op2.num` of a send, an `UNUSED` operand that is a number,
/// and the operator an `ASSIGN_OP`'s `extended_value` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Imm {
    None,
    /// The argument a send writes, from one: a `SEND_VAL`'s or a
    /// `SEND_VAR`'s, and a `SEND_VAL_EX`'s whose callee the trace recorded
    /// takes it by value, which php decides as it compiles
    /// (`zend_jit_send_val`, `Z/ext/opcache/jit/zend_jit_ir.c:9466-9506`).
    Arg(u32),
    /// The binary operator of an `ASSIGN_OP` or an `ASSIGN_DIM_OP`.
    Binary(Opcode),
    /// A `DO_*CALL` that makes a call compiled code pushed in place, and
    /// makes it the callee's frame in place where nothing refuses it
    /// (`zend_jit_do_fcall`, `Z/ext/opcache/jit/zend_jit_ir.c:10016-10600`):
    /// the slot of its frame that takes what the callee returns, where the
    /// opline reads it.
    Make {
        returns: Option<u32>,
        checks: ArgChecks,
        defaults: Defaults,
    },
    /// A `RETURN` compiled code leaves its frame by in place
    /// (`zend_jit_leave_func`, `Z/ext/opcache/jit/zend_jit_ir.c:11035-11263`):
    /// a function's frame, with no observer declared; what it hands its
    /// caller, the variables it gives up as it leaves ([`Owners`]), and
    /// whether its frame may have a receiver, which it gives up after
    /// them ([`php_engine::tier::UnitView::receives`]), and whether it is
    /// a closure's, whose closure it gives up last
    /// ([`crate::record::closure`]).
    Leave {
        owners: Owners,
        value: Returned,
        receives: bool,
        closure: bool,
    },
    /// The native a `FRAMELESS_ICALL_*` calls, its place in the engine's
    /// frameless table, the opline's `extended_value`, php's
    /// `ZEND_FLF_HANDLER(opline)`, which compiled code hands the
    /// `frameless` kernel so it reads no opline.
    Frameless(FramelessAt),
    /// A `FETCH_DIM_R` of a string's offset, which php's
    /// `zend_jit_fetch_dim_read` calls a function of its own for
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:12720-12755`): no exit of its own
    /// and no guard within, what it writes being a string.
    StrOffset,
    /// The types a `TYPE_CHECK` asks for, its `extended_value`: one bit
    /// per type, `1 << IS_*`, php's `MAY_BE_*` mask.
    Mask(u32),
    /// A property opline compiled in place on the declared slot its name
    /// is placed at on objects of the class a guard before it holds
    /// (`zend_jit_fetch_obj`, `zend_jit_assign_obj`, `zend_jit_assign_obj_op`,
    /// `zend_jit_incdec_obj`, `Z/ext/opcache/jit/zend_jit_ir.c:14206-15700`
    /// over a known `prop_info`): where it finds the slot, and an
    /// `ASSIGN_OBJ_OP`'s operator.
    Property {
        slot: Slotted,
        binary: Option<Opcode>,
    },
    /// A function's call by its name pushed in place under the guard on
    /// the callee its own runtime-cache slot holds ([`Line::map_ptr`]), as
    /// php's code compares `CACHED_PTR(opline->result.num)` with the
    /// function it recorded (`zend_jit_init_fcall`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:8860-8880`). A slot that holds
    /// another or none yet goes to the `push_call` kernel, which finds the
    /// name, and to the opline's exit where that is another function.
    Named,
    /// A method's call pushed in place under the guard on the pair its
    /// own runtime-cache slot holds ([`Line::map_ptr`]), as php's code
    /// reads `CACHED_PTR(opline->result.num)` and the callee past it
    /// before it pushes (`zend_jit_init_method_call`,
    /// `zend_jit_init_static_method_call`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:9025-9056`, `:9255-9265`): the
    /// callee the pair is held to, and the class.
    Pair {
        function: PairCallee,
        class: PairClass,
    },
}

/// The callee the pair in a call's runtime-cache slot is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairCallee {
    /// A published method's number, the same in every request of the
    /// epoch ([`php_engine::tier::CalleeSlot::function`]), compared as a
    /// constant.
    Number(EpochFunction),
    /// A request-scoped class's own method, numbered by each request for
    /// itself: the pair's record compared with the record of the header
    /// the request laid out for the call, which names the method the
    /// request found for the class's name over the op array the trace
    /// recorded, as php's `zend_jit_func_guard` holds a function that is
    /// not immutable by its opcodes (`Z/ext/opcache/jit/zend_jit_ir.c:8999-9024`).
    Record,
}

/// The class the pair in a call's runtime-cache slot is held to, and so
/// where the called scope of the call pushed in place comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairClass {
    /// The receiver's, of a request-scoped class an `INIT_METHOD_CALL`
    /// calls a method on: the pair's class compared with `obj->ce`, as
    /// php compares `CACHED_PTR(opline->result.num)` with it, and the
    /// called scope written from it.
    Receiver,
    /// This published class, the one an `INIT_STATIC_METHOD_CALL` names,
    /// which the call's header names as its called scope in every
    /// request, as php takes an immutable linked class as a constant.
    Published(EpochClassId),
    /// The request-scoped class an `INIT_STATIC_METHOD_CALL` names,
    /// whatever its request resolved the name to: the called scope
    /// written from the pair, as php reads it from the slot.
    Named,
    /// The class `self` or `parent` names, which the scope of the op array
    /// fixes, so the pair's class is not compared, as php reads `fbc`
    /// alone from the slot of an `UNUSED` class operand; a static callee,
    /// whose called scope is the calling frame's own, forwarded, as php's
    /// `zend_jit_init_static_method_call` takes `EX(This)`'s
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:9340-9358`).
    Forwarded,
    /// A callee that takes `$this`, called by a static call's syntax: the
    /// calling frame's `$this`, lent, held to be of `receiver`, a
    /// published class and the call's called scope, as php's
    /// `zend_jit_push_this_method_call_frame` passes `This` where it is an
    /// instance of the class named (`Z/ext/opcache/jit/zend_jit_helpers.c:262-273`);
    /// and the pair's class compared with `named` where the opline names a
    /// published class, and with nothing where `self` or `parent` or a
    /// request-scoped name names it.
    This {
        named: Option<EpochClassId>,
        receiver: EpochClassId,
    },
}

/// Where a property opline compiled in place finds its declared slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slotted {
    /// By its number, on objects of the published class a guard before
    /// the opline holds.
    Declared(u32),
    /// In the request's own runtime-cache slot the opline reads
    /// ([`Line::map_ptr`]), under the guard compiled code holds there on
    /// its tag and its class, as php's code reads `CACHED_PTR(cache_slot)`
    /// and the offset past it for a class that is not immutable
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14341-14370`): a request-scoped
    /// class's, whose id and slots are the request's.
    Cached,
}

/// The object a property opline reads and a class guard holds: `$this`
/// of a frame, `Z_OBJ(EX(This))`, or a value the trace holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Object {
    This(Frame),
    Value(ValueId),
}

/// The variables of a frame a `RETURN` left in place gives up, as php's
/// trace compiles the loop over `op_array->last_var` before
/// `zend_jit_leave_func` (`Z/ext/opcache/jit/zend_jit_trace.c:5586-5634`):
/// each that the trace does not hold uncounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owners {
    /// Every variable holds what no reference counts: the leave frees
    /// nothing.
    Uncounted,
    /// The variables that may own something, each freed in place in slot
    /// order, `zend_jit_free_cv` unrolled.
    Each(Vars),
    /// More variables than [`Vars`] holds, their type words tested at
    /// once and the leave kernel's to free where one owns something, as
    /// php calls `zend_free_compiled_variables` where it will not unroll
    /// (`zend_jit_trace.c:5593-5598`).
    Kernel,
}

/// A set of a frame's variables, by number, the first [`Vars::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Vars(u64);

impl Vars {
    /// How many variables a set can name.
    pub(crate) const MAX: u32 = u64::BITS;

    /// The set with `var` added; `None` past [`Vars::MAX`].
    pub(crate) fn with(self, var: u32) -> Option<Vars> {
        let bit = 1u64.checked_shl(var)?;
        Some(Vars(self.0 | bit))
    }

    /// Whether the set names no variable.
    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The variables named, in slot order.
    pub(crate) fn iter(self) -> impl Iterator<Item = u32> {
        (0..Vars::MAX).filter(move |var| (self.0 >> var) & 1 == 1)
    }
}

/// What a `RETURN` left in place hands its caller, as
/// `zend_jit_return` writes it (`Z/ext/opcache/jit/zend_jit_ir.c:11265-11400`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Returned {
    /// A scalar, or nothing, `null`.
    Scalar,
    /// A constant no reference counts, an immutable array or an interned
    /// string, copied as it is (`ZVAL_COPY_CONST`): the address of its
    /// zval among the op array's constants, which the trace's epoch
    /// holds.
    Constant(usize),
    /// What a temporary holds, moved whole with its owner
    /// (`ZVAL_COPY_VALUE`), given up where the caller reads no return.
    Moved,
    /// What a variable holds, read through the reference it holds where
    /// it holds one (`ZVAL_DEREF`), and copied with an owner more where
    /// it is counted (`ZVAL_COPY`) before the frame's variables are given
    /// up; nothing where the caller reads no return.
    Shared,
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
    /// direction, where a step or a return left the interpreter.
    Within(OpId),
}

/// What a guard holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    /// The value is of this one type, which is no reference.
    Type { value: ValueId, tag: ZvalType },
    /// The slot holds a reference, and what it refers to is of this type.
    Deref { value: ValueId, tag: ZvalType },
    /// The slot holds a reference still, of whatever it refers to, php's
    /// `zend_jit_guard_reference` (`Z/ext/opcache/jit/zend_jit_ir.c:16990-17019`):
    /// an `ASSIGN` to a variable the recording saw through a reference,
    /// which writes through it. Its exit goes back to the opline.
    Reference { value: ValueId },
    /// The `VAR` holds the address of a zval, `IS_INDIRECT`, as a fetch
    /// for writing leaves it, and that zval is of this type, php's
    /// `zend_jit_fetch_indirect_var` (`Z/ext/opcache/jit/zend_jit_ir.c:17087-17141`):
    /// the `VAR`'s own type is tested where its value may be other than
    /// [`Types::INDIRECT`], where php's `add_indirect_guard` is set, the
    /// `indirect_reference` mark past an inline fetch for writing clear
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:5951`), a type here. Where the
    /// zval it points at is a reference, the guard holds it one and the
    /// value it refers to of this type, as php's `zend_jit_fetch_reference`
    /// follows it after (`Z/ext/opcache/jit/zend_jit_ir.c:17021-17085`); the
    /// exit goes back to the opline.
    Indirect {
        value: ValueId,
        tag: ZvalType,
        pointee: Pointee,
    },
    /// The array is laid out so, held in the slot or through the
    /// reference the slot holds: php's packed guard.
    Layout {
        value: ValueId,
        layout: Layout,
        read: Read,
    },
    /// The jump goes the way it went.
    Direction { taken: bool },
    /// The interpreter stands at this opline of this op array once the op
    /// is done, php's IP guard (`jit_CMP_IP`): where a return past the
    /// trace's lowest frame lands, and where a stepped opline whose next
    /// opline the record does not say went, a table's jump. Its exit names
    /// no opline, so the interpreter goes on where it stands and the exit
    /// is right whatever the guard held.
    Stands { func: FuncRef, opline: OplineIdx },
    /// The VM stack's top page has room for this many bytes of frames,
    /// php's `zend_jit_stack_check`: at a loop's entry for what its calls
    /// push, and before an `INIT_*` that pushes past what that held. Its
    /// exit goes back to the interpreter, which pushes the frame on a page
    /// of its own.
    Room { bytes: u32 },
    /// The object is of this class, php's `zend_jit_class_guard`, which
    /// compares `obj->ce` (`Z/ext/opcache/jit/zend_jit_ir.c:14191-14204`):
    /// a published class, whose id names the one linked class in every
    /// request of the trace's epoch.
    Class { object: Object, class: EpochClassId },
    /// The call the stepped `INIT_*` before it pushed through its handler
    /// serves this op array, php's `zend_jit_func_guard` on `call->func`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:9283-9300`), so the trace's
    /// plain call of it is the call made. Its exit goes on where the
    /// interpreter stands, the call pushed.
    Callee { func: FuncRef },
    /// A table's jump went elsewhere than this opline: php's exit per
    /// target of `zend_jit_switch`, taken where the stepped jump went
    /// there and landing where it went, so exit-fuzz fails it never.
    Arm { opline: OplineIdx },
}

/// How an array is laid out, php's `HASH_FLAG_PACKED` set or clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    Packed,
    Hash,
}

/// What an exit does: where the interpreter goes on, what the trampoline
/// does first, what compiled code stores, what the trace knows of the
/// frame's slots there, whether a side trace grows from it, and the trace
/// it links to.
///
/// It is all an exit is: two places that leave with an equal spec that
/// names an opline leave through one exit, as php's
/// `zend_jit_trace_get_exit_point` reuses one whose opline, `flags` and
/// stack map match (`Z/ext/opcache/jit/zend_jit_trace.c:168-185`). The
/// stack map is the stores, a register per slot, php's `STACK_REF`, and
/// the known types, php's `STACK_TYPE`, which a guard sets for the slot it
/// holds once it held (`SET_STACK_TYPE`, `:4303-4316`), so successive
/// guards at one place leave through exits of their own. php never
/// reuses a method call's exit, `ZEND_JIT_EXIT_METHOD_CALL`, which keeps
/// the callee it guarded for a side trace; no guard here holds a method
/// call's callee, so no exit is one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ExitSpec {
    pub(crate) frame: Frame,
    /// The opline the interpreter goes on at; none where the frame
    /// already stands where it goes on, as php's exit with no opline
    /// does after a return to a caller its guard did not expect.
    pub(crate) opline: Option<OplineIdx>,
    pub(crate) landing: Landing,
    pub(crate) stores: Vec<Store>,
    /// The slots of the exit's frame whose type the trace knows where it
    /// leaves, by slot: what a side trace grown from it enters with.
    pub(crate) known: Vec<Known>,
    pub(crate) grows: Grows,
    pub(crate) link: Option<TraceId>,
    /// Whether a guard on an array's layout takes it, php's
    /// `ZEND_JIT_EXIT_PACKED_GUARD` (`Z/ext/opcache/jit/zend_jit_ir.c:11967`,
    /// `:13779`): a bit nothing reads but the reuse, so a layout guard
    /// leaves through an exit of its own beside a type guard at its place.
    pub(crate) layout: bool,
}

/// One slot whose type the trace knows where an exit leaves, php's
/// `STACK_TYPE` of a stack map's slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Known {
    pub(crate) slot: Slot,
    pub(crate) tag: ZvalType,
}

/// Whether a side trace may grow from an exit, php's `flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EngineState)]
pub(crate) enum Grows {
    /// A hot exit records a side trace, php's `flags` 0.
    Side,
    /// The exit goes back to the interpreter alone and never grows one,
    /// php's `ZEND_JIT_EXIT_TO_VM`: a loop's interrupt test
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:7223`), and an entered trace's
    /// `RECV` exits, which `zend_jit_recv` takes
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:10825-10872`).
    Never,
}

/// Where an opline leaves of its own: through an exit, or through an
/// unwind, which takes none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leave {
    Exit(ExitId),
    Unwind(UnwindId),
}

/// Where compiled code goes when a kernel answers anything but
/// `Continue`: the scalars the trace keeps in registers stored, so the
/// interpreter unwinds over its slots from where the throwable was
/// raised. No exit is taken, so none is numbered, counted toward
/// `ZEND_JIT_TRACE_MAX_EXITS` or written as a `TRACE n exit m` line, as
/// php's compiled code jumps to its stub for a throwable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Unwind {
    pub(crate) stores: Vec<Store>,
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
    /// A fetched element whose type its guard did not hold, which the
    /// guard's own place copies into the fetch's slot as the fetch copies
    /// it before it leaves, read in place or through the reference the
    /// element is: the slot is written, and the exit stores nothing there.
    /// No other place leaves with it, as php's stack map keeps that slot
    /// in memory where the next opline's keeps it in a register, and the
    /// two reads leave apart, as php's two guards of an element, one per
    /// way it is read, hold the element's address in registers of their
    /// own (`zend_jit_fetch_dim_read`).
    Copied { value: ValueId, read: Read },
    /// `null`, which the fetch that found no element wrote to its slot
    /// before it left, as php's stack map holds it, `null(type_only)`.
    Null,
    /// The double an `ADD` or a `SUB` of `1`, an increment or a
    /// decrement overflowed to, past `ZEND_LONG_MAX` where it went `up`
    /// and below `ZEND_LONG_MIN` otherwise, which the site writes to the
    /// slot in its own frame before it leaves, as php's frame-relative
    /// stack map holds it as a constant, `float(9.22337e+18)`
    /// (`zend_jit_math_long_long`, `zend_jit_inc_dec`).
    Overflowed { up: bool },
    /// The long a `POST_INC` or a `POST_DEC` that overflowed read, and
    /// writes to its `out`: `ZEND_LONG_MAX` where it went `up`,
    /// `ZEND_LONG_MIN` otherwise, which the site writes as it writes
    /// [`Stored::Overflowed`], as php's stack map holds it,
    /// `int(9223372036854775807)` (`zend_jit_inc_dec`).
    Limit { up: bool },
    /// A long an increment or a decrement wrote where it did not
    /// overflow and a guard held what it writes a double, the way its
    /// record went: the long it went to, in its variable and in a
    /// `PRE_*`'s `out`, or, `read`, the long the variable held, in a
    /// `POST_*`'s `out`. The site writes it to the slot in its own frame
    /// before it leaves, as php's stack map holds it as the step's
    /// register, `int(x1)` (`zend_jit_inc_dec`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:4824-4856`); `value` is what the
    /// step writes to that slot, so no other site's exit is this one.
    Stayed { value: ValueId, read: bool },
}

impl Stored {
    /// The type the site sets the slot to where it writes it before it
    /// leaves, php's `SET_STACK_TYPE` before `zend_jit_inc_dec` and
    /// `zend_jit_math_long_long` take their exits: the double an
    /// overflow went to, the long a `POST_*` read at the limit, or the
    /// long a step wrote that did not overflow; none of a value the exit
    /// stores, nor of an element a fetch copied.
    pub(crate) fn stack_type(self) -> Option<ZvalType> {
        match self {
            Stored::Overflowed { .. } => Some(ZvalType::Double),
            Stored::Limit { .. } | Stored::Stayed { .. } => Some(ZvalType::Long),
            Stored::Value(_) | Stored::Copied { .. } | Stored::Null => None,
        }
    }
}

/// How a zval is read: in place, through the reference it is, or
/// through the address an indirect `VAR` holds ([`Check::Indirect`]), to
/// the zval there or the value of the reference it is: a fetched element,
/// the array a layout guard holds, and the array an element write writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Read {
    InPlace,
    Through,
    Indirect(Pointee),
}

/// What an indirect `VAR` is read as: the zval it points at, or the value
/// of the reference that zval is, as php follows `zend_jit_fetch_indirect_var`
/// with `zend_jit_fetch_reference` where the recorder saw both
/// (`Z/ext/opcache/jit/zend_jit_ir.c:17087-17141`, `:17021-17085`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Pointee {
    Value,
    Reference,
}

impl Read {
    /// How the recorder reached the zval it sampled: through an
    /// `IS_INDIRECT`, then through the `IS_REFERENCE` it points at where
    /// it is one, through an `IS_REFERENCE`, or in place.
    pub(crate) fn sampled(sample: php_engine::tier::Sample) -> Read {
        match (sample.indirect, sample.reference) {
            (false, false) => Read::InPlace,
            (false, true) => Read::Through,
            (true, false) => Read::Indirect(Pointee::Value),
            (true, true) => Read::Indirect(Pointee::Reference),
        }
    }
}

/// What the trampoline does at an exit before the interpreter goes on
/// (the glossary's Landing), beside standing the frame at its opline:
/// two things apart, as php's exit frees an opline's operands and
/// restores a call under way each on its own (`ZEND_JIT_EXIT_FREE_OP1`,
/// `ZEND_JIT_EXIT_FREE_OP2`, `ZEND_JIT_EXIT_RESTORE_CALL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EngineState)]
pub(crate) struct Landing {
    /// The temporaries the opline before the exit's read, which compiled
    /// code has not freed yet.
    pub(crate) frees: Frees,
    /// Whether a call an `INIT_*` began is under way, and is restored.
    pub(crate) restores_call: bool,
}

impl Landing {
    /// A landing that stands the frame and does nothing else.
    pub(crate) const PLAIN: Landing = Landing {
        frees: Frees::NONE,
        restores_call: false,
    };

    /// A landing that restores the call under way and frees nothing.
    pub(crate) const RESTORE_CALL: Landing = Landing {
        frees: Frees::NONE,
        restores_call: true,
    };
}

/// Which of the temporaries an opline read a landing frees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EngineState)]
pub(crate) struct Frees {
    pub(crate) op1: bool,
    pub(crate) op2: bool,
}

impl Frees {
    /// None of them.
    pub(crate) const NONE: Frees = Frees {
        op1: false,
        op2: false,
    };

    /// Whether it frees either.
    pub(crate) fn any(self) -> bool {
        self.op1 || self.op2
    }
}

/// How the trace ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// It goes back to its header.
    Loop(LoopHeader),
    /// It leaves through this exit.
    Leave(ExitId),
    /// Its last op, the `RETURN` it stops at, leaves its frame in place,
    /// and the interpreter goes on where the return left it, as php's
    /// code goes on through `zend_jit_trace_return`, taking no exit; it
    /// leaves through this exit, at that `RETURN`, where the leave is not
    /// made in place.
    Return(ExitId),
}

/// Where a looping trace goes back to, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoopHeader {
    /// The op the back edge goes to.
    pub(crate) at: OpId,
    pub(crate) repeat: Repeat,
    /// Where the interrupt test leaves, or that the loop takes none
    /// (`jit-design.md` §4.3).
    pub(crate) interrupt: Interrupt,
}

/// Where a loop's interrupt test goes once the interrupt is raised, as
/// php's `zend_jit_trace` chooses its `timeout_exit_addr`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:7210-7234`), or that it takes
/// none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interrupt {
    /// Through this exit, php's `ZEND_JIT_EXIT_TO_VM` exit point at the
    /// loop's header, which never grows a side trace, numbered after
    /// every exit of the loop's body as php takes it where it closes the
    /// loop: where php's code does not keep the instruction pointer the
    /// trace entered with, or keeps values in registers there. In an
    /// entered trace whose first `RECV` takes an exit, and where the test
    /// stores no register, it is that `RECV`'s, exit 0, landing at the
    /// entry's opline, as php's test reuses the exit at opline zero
    /// (`jit-design.md` §4.2, #3269).
    Exit(ExitId),
    /// Through php's interrupt stub, `jit_stub_interrupt_handler`, which
    /// takes no exit: the interrupt is handled and the trace entered again
    /// at its header, as php's stub goes on at the header's handler,
    /// which is the trace. Where php's code keeps its initial
    /// instruction pointer and holds nothing in registers at the loop's
    /// end, so the interpreter needs nothing back. The header's opline,
    /// where the frame is stood for the stub.
    Stub(OplineIdx),
    /// Nowhere: a loop by recursive return takes no test and numbers no
    /// exit for one, as php's `zend_jit_trace` sets no
    /// `timeout_exit_addr` where the trace stops at
    /// `ZEND_JIT_TRACE_STOP_RECURSIVE_RET` and `zend_jit_trace_end_loop`
    /// emits no check without one (`Z/ext/opcache/jit/zend_jit_ir.c:17378-17386`).
    /// Each turn returns one frame shallower, so the loop ends where the
    /// stack it unwinds does. Nor does a side trace's link to it test the
    /// interrupt, as php sets no `ZEND_JIT_TRACE_CHECK_INTERRUPT` for it
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:7209-7212`).
    ///
    /// A recursion with no end that a trace by recursive return serves
    /// meets the timeout all the same: the recursive call's loop tests
    /// it, through the `RECV` exit its test reuses (#3269), and each
    /// trace the recursion leaves hands the interpreter its interrupt
    /// check, where the timeout's fatal is raised:
    ///
    /// ```php
    /// set_time_limit(1);
    /// function f(int $d) { if ($d === 0) { return 0; } $x = f($d - 1); $y = f($d - 1); return $x; }
    /// echo f(60), "\n";
    /// ```
    /// ```expect exit=255 regime=jit-inline process wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:2
    /// ---- TRACE 1 abort (deep recursion)
    ///
    /// ---- TRACE 1 start (enter) f() %s:3
    /// ---- TRACE 1 stop (recursive call)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 1 f() %s:3
    /// ---- TRACE 2 start (side trace 1/1) f() %s:3
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (return) f() %s:3
    /// ---- TRACE 3 stop (recursive return)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 3 exit 3 f() %s:3
    /// ---- TRACE 4 start (side trace 3/3) f() %s:3
    /// ---- TRACE 4 stop (link to 1)
    /// ---- TRACE 4 compiled
    /// ```
    ///
    /// So does a single recursion, whose returns the trace by recursive
    /// return serves alone:
    ///
    /// ```php
    /// set_time_limit(1);
    /// function f($n) { return $n ? f($n - 1) + 1 : 0; } $t = true; while ($t) { f(50); }
    /// ```
    /// ```expect exit=255 regime=jit-inline process wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:2
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 abort (deep recursion)
    ///
    /// ---- TRACE 1 start (enter) f() %s:3
    /// ---- TRACE 1 stop (recursive call)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 1 f() %s:3
    /// ---- TRACE 2 start (side trace 1/1) f() %s:3
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (return) f() %s:3
    /// ---- TRACE 3 stop (recursive return)
    /// ---- TRACE 3 compiled
    ///
    /// ---- TRACE 4 start (loop) $main() %s:3
    /// ---- TRACE 4 stop (link to 1)
    /// ---- TRACE 4 compiled
    /// ```
    None,
}

impl Interrupt {
    /// Whether the loop tests the interrupt, at its header and before a
    /// side trace's link to it.
    pub(crate) fn tested(self) -> bool {
        !matches!(self, Interrupt::None)
    }
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

/// Whether an inline arithmetic `op` by `by` raises no error the
/// interpreter would throw: every operator but a division, a modulo and
/// the two shifts, and those four by a constant that cannot raise one, a
/// divisor other than zero and a shift that is not negative, where php's
/// code generator tests nothing and takes no exit point
/// (`zend_jit_long_math`).
pub(crate) fn cannot_raise(op: Opcode, by: Operand) -> bool {
    match (op, by) {
        (Opcode::Div | Opcode::Mod, Operand::Constant(Constant::Long(n))) => n != 0,
        (Opcode::Div, Operand::Constant(Constant::Double(d))) => d != 0.0,
        (Opcode::Sl | Opcode::Sr, Operand::Constant(Constant::Long(n))) => n >= 0,
        (Opcode::Div | Opcode::Mod | Opcode::Sl | Opcode::Sr, _) => false,
        _ => true,
    }
}

/// The types a value of the declared type `ty` may be, as php's
/// `zend_fetch_arg_info_type` reads an `arg_info`: what a native's call
/// returns, where it declares a return type.
pub(crate) fn declared(ty: &php_unit::decl::Type) -> Types {
    opcodes::of_type(Some(ty), false)
}

/// The property oplines compiled code serves in place on a declared slot
/// ([`Imm::Property`]), whose place the recorder takes from the engine's
/// cache once the handler has primed it (`UnitView::hold_place`).
pub(crate) fn on_declared_slot(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::FetchObjR
            | Opcode::FetchObjW
            | Opcode::AssignObj
            | Opcode::AssignObjOp
            | Opcode::PreIncObj
            | Opcode::PreDecObj
            | Opcode::PostIncObj
            | Opcode::PostDecObj
    )
}

/// The trace `rec` builds, read through `view` and what php's analysis
/// of each op array it passes through learned, or why it builds none.
pub(crate) fn build(
    rec: &TraceRecord,
    view: &UnitView,
    analysed: &Analysed,
) -> Result<TraceIr, Abort> {
    let tables = analysed.tables(rec, view);
    let inference = tables.frames();
    build::build(rec, view, analysed, &inference)
}
