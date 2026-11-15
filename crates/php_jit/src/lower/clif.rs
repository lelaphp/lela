//! One trace as one Cranelift function (`jit-design.md` §4.3): the walk
//! over a [`TraceIr`]'s ops in order, each guard where it is held, each
//! exit a cold block that stores what the trace kept in registers and
//! answers its number, and every kernel called through [`Emit::call`],
//! the one emitter, which tests the status word the kernel answers.
//!
//! **Slots.** A slot is read and written in its frame's room at
//! `ROOM_BYTES + 16 * var`, its word at offset zero and its type word,
//! php's `Z_TYPE_INFO`, at [`Zval::TYPE_AT`]. A value `types` keeps in a
//! register is a Cranelift variable, `i64` for a long, `f64` for a double
//! and `i8` for a boolean, written to its slot with its type word only
//! where an exit leaves, a kernel reads it as an operand, or an
//! assignment overwrote a counted value there; every other value is read
//! from its slot where an op reads it, by the type its guard holds.
//!
//! **What each inline form compiles to.** The arithmetic, the
//! comparisons, the jumps and the assignments of the scalar core are
//! compiled code over their operands, with php's overflow to a double,
//! its division and shift errors left to the interpreter through the
//! opline's exit, none tested by a constant that cannot raise one, and a
//! long arithmetic `types` holds long leaving where it overflows. A
//! packed array is read and written in place: a fetch past `nNumUsed` or
//! of a hole leaves, its element held to its type read in place and read
//! through the reference it is, and a write leaves unless the
//! array is counted and held once and the element is there and counts
//! nothing. A send writes the call's slot. The frame chain is the
//! engine's: an inline `INIT_*` is [`Kernel::PushCall`], which refuses a
//! callee other than the one recorded, and an inline `DO_*CALL`,
//! `RETURN` and `SEND_VAL_EX`, whose by-reference check is the callee's
//! declaration, are [`Kernel::Step`]; compiled code follows the frame
//! chain by the [`Stood`] each writes.

use std::collections::HashSet;
use std::mem::offset_of;

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I8, I32, I64};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, Function, InstBuilder, MemFlagsData, SigRef, Signature, SourceLoc,
    StackSlot, StackSlotData, StackSlotKind, Type, UserFuncName, Value,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use php_engine::tier::{EngineFuncRef, FRAME, PlainCall};
use php_heap::{Arr, ROOM_BYTES, Ref, Str, Zval, ZvalType};
use php_unit::{ConstIdx, Opcode, OplineIdx};

use super::{
    ABANDONED, DECLINED, Door, ENTRY_BODY, ENTRY_PLACE, ENTRY_UNREADY, INTERRUPT_AT, INTERRUPTED,
    Kernel, RETURNED, STACK_AT, Stood, Symbols, TRACE_AT, UNWOUND, engine_offset, located,
};

mod calls;
mod dims;
mod doors;
mod fetches;
mod objects;
mod strings;
mod unread;
use self::dims::Held;
use crate::record::{Began, Callee, Stop, leaves, returns};
use crate::types::{
    Abort, BodyEntry, Check, Constant, Def, End, ExitId, Form, Frame, Fused, Guard, GuardAt, Home,
    Imm, Interrupt, Layout, Leave, Line, Op, OpId, Operand, Place, Pointee, PushRoom, Read, Repeat,
    Slot, Store, Stored, TraceIr, Types, UnwindId, ValueId, cannot_raise,
};

/// Where an inline arithmetic that would raise goes instead
/// ([`Emit::refuse_if`]): the opline's exit, for the interpreter to serve
/// it again, or the block where its handler serves the write.
#[derive(Debug, Clone, Copy)]
enum Refuse {
    Exit(ExitId),
    To(Block),
}

/// A layout offset as the immediate a load or a store takes.
const fn imm(at: usize) -> i32 {
    assert!(at < 0x7fff_ffff, "a layout offset past an immediate");
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "held below i32::MAX above"
    )]
    let at = at as i32;
    at
}

/// The type word of a zval, php's `Z_TYPE_INFO`.
const TYPE_AT: i32 = imm(Zval::TYPE_AT);
/// `IS_TYPE_REFCOUNTED` in the type word: the flags byte follows the type
/// byte, as php's `Z_TYPE_FLAGS_SHIFT` lays it.
const COUNTED: i64 = (Zval::TYPE_REFCOUNTED as i64) << 8;
/// The type byte in the type word, php's `Z_TYPE` of `Z_TYPE_INFO`.
const KIND: i64 = 0xff;
const _: () = assert!(Zval::FLAGS_AT == Zval::TYPE_AT + 1);
/// A slot, `sizeof(zval)`.
const SLOT: i64 = 16;
/// A `foreach` iterator's position in its slot, php's `Z_FE_POS`.
const FE_POS_AT: i32 = imm(Zval::FE_POS_AT);
const _: () = assert!(size_of::<Zval>() == 16);
/// Where a frame's opline is in its room, `EX(opline)`.
const EX_OPLINE: i32 = imm(FRAME.opline() as usize);
const STOOD_FRAME: i32 = imm(offset_of!(Stood, frame));
const STOOD_CALL: i32 = imm(offset_of!(Stood, call));
const STOOD_FUNC: i32 = imm(offset_of!(Stood, func));
const STOOD_OPLINE: i32 = imm(offset_of!(Stood, opline));
const STOOD_ROOM: i32 = imm(offset_of!(Stood, room));
const _: () = assert!(STOOD_OPLINE == STOOD_FUNC + 4 && size_of::<Stood>() == 32);

/// Where compiled code finds the room of a call under way.
#[derive(Debug, Clone, Copy)]
enum CallRoom {
    /// In this variable, read where the trace began the call.
    Held(Variable),
    /// Where the interpreter stands, `EX(call)`: a call its frame began
    /// before the trace reached it, php's fake init, which is the
    /// innermost under way wherever compiled code reaches it, since every
    /// call the trace began after it is made by then and the last kernel
    /// stood the frame, or `enter` did.
    Stood,
}

/// Where a body holds what it reads of its entry for as long as it runs.
/// A trace keeps both in registers. A loop's trace that pushes calls in
/// place keeps them in its frame, past the [`Stood`], whose address it holds
/// already, and reads them where its code does: its pushes hold registers
/// across the loop already, and two more put the loop's own values in its
/// frame, reloaded every turn (spectralnorm's room and place).
#[derive(Debug, Clone, Copy)]
enum EntryHeld {
    Registers { images: Value, unready: Value },
    Frame,
}

/// The request's addresses of a loop's trace ([`EntryHeld::Frame`]), past
/// the [`Stood`].
const HELD_IMAGES: i32 = imm(size_of::<Stood>());

/// Whether the request is not ready, an `i8` past [`HELD_IMAGES`].
const HELD_UNREADY: i32 = HELD_IMAGES + imm(size_of::<usize>());

/// The [`Stood`]'s own address, which a loop's trace keeps in its frame past
/// [`HELD_UNREADY`] and loads for the kernel that takes it. An address the
/// body makes anywhere is hoisted out of the loop and holds a register
/// across every turn, for calls a turn may never make.
const HELD_STOOD: i32 = HELD_UNREADY + imm(size_of::<usize>());

/// The bytes of the frame's slot of the [`Stood`] and what is held past it.
const STOOD_SLOT: i32 = HELD_STOOD + imm(size_of::<usize>());

/// The variables [`Emit`] holds the [`Stood`] in, one per field.
#[derive(Debug, Clone, Copy)]
struct StoodVars {
    frame: Variable,
    call: Variable,
    func: Variable,
    opline: Variable,
    room: Variable,
}

/// Where the code goes where the interrupt test finds the interrupt
/// raised ([`Emit::interrupt_test`]).
#[derive(Debug, Clone, Copy)]
enum Raised {
    /// Through this exit, php's exit point at a loop's header.
    Exit(ExitId),
    /// To php's interrupt stub, [`INTERRUPTED`].
    Stub,
    /// To this block, which answers a link's exit: the root it links to
    /// meets the interrupt at its own header.
    Answer(Block),
}

/// The type byte of `tag`, as an immediate.
fn tag(tag: ZvalType) -> i64 {
    i64::from(tag as u8)
}

/// Every load and store of compiled code is aligned and of memory the
/// trace's guards and kernels hold live.
fn trusted() -> MemFlagsData {
    MemFlagsData::trusted()
}

/// php's `COMPILER_ERROR`, for a trace whose shape the lowering does not
/// expect of `types`.
fn mismatch(why: &'static str) -> Abort {
    Abort {
        stop: Stop::CompilerError,
        why,
    }
}

/// The body `ir` lowers to on the target `config` describes, calling
/// the kernels at `symbols`, and, under exit-fuzz, failing each guard
/// once by its byte of `fuzz`; answering with its number and jumping
/// through its link as `joins` says; with how many such guards it holds
/// and the doors it jumps through, in the order their words stand.
pub(super) fn lower(
    ir: &TraceIr,
    config: TargetFrontendConfig,
    (symbols, held): (&Symbols, &[PlainCall]),
    (fuzz, joins): (Option<Fuzz>, Joins),
) -> Result<(Function, usize, Box<[Door]>), Abort> {
    let mut func = Function::with_name_signature(UserFuncName::user(0, 0), body_signature());
    let mut context = FunctionBuilderContext::new();
    let builder = FunctionBuilder::new(&mut func, &mut context);
    let mut emit = Emit::new(ir, builder, config.default_call_conv, (symbols, held));
    emit.fuzz = fuzz;
    emit.joins = joins;
    emit.trace()?;
    let sites = emit.fuzzed_sites;
    let doors = std::mem::take(&mut emit.made).into_boxed_slice();
    let stood_loads = std::mem::take(&mut emit.stood_loads);
    emit.b.finalize(config);
    unread::unread(&mut func, &stood_loads);
    Ok((func, sites, doors))
}

/// How many parameters a body takes (`super`'s *The entry*): the
/// `ExitCtx`, the room of the frame it enters on, the entry word, the
/// request's addresses of the trace, the call under way and where the
/// frame stands.
const ENTRY_PARAMS: usize = 6;

/// A trace's body's signature: its entry, [`ENTRY_PARAMS`] words, and the
/// exit or the answer it leaves with, on Cranelift's tail convention, so
/// a body hands on to another by a tail call with the entry in registers.
fn body_signature() -> Signature {
    let mut sig = Signature::new(CallConv::Tail);
    sig.params.extend([AbiParam::new(I64); ENTRY_PARAMS]);
    sig.returns.push(AbiParam::new(I32));
    sig
}

/// The trace prologue (`super::Backend`): on the host's convention, the
/// `ExitCtx`, the room, the body's address and the rest of the entry, the
/// body called on the tail convention with the entry and its answer
/// answered.
pub(super) fn prologue(config: TargetFrontendConfig) -> Function {
    let mut sig = Signature::new(config.default_call_conv);
    sig.params.extend([AbiParam::new(I64); ENTRY_PARAMS + 1]);
    sig.returns.push(AbiParam::new(I32));
    let mut func = Function::with_name_signature(UserFuncName::user(0, 1), sig);
    let mut context = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut context);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);
    let params = b.block_params(entry).to_vec();
    let body = b.import_signature(body_signature());
    let mut handed = params[..2].to_vec();
    handed.extend_from_slice(&params[3..]);
    let call = b.ins().call_indirect(body, params[2], &handed);
    let answer = b.inst_results(call).to_vec();
    b.ins().return_(&answer);
    b.finalize(config);
    func
}

/// What a body joins besides its exits (ruled 2026-10-22): the address of
/// its number, which its answers write at [`TRACE_AT`], where its escape
/// reads the trace compiled at the landing, and the trace its link jumps
/// to, where it links to one with code.
#[derive(Debug, Clone, Copy)]
pub(super) struct Joins {
    pub(super) number: usize,
    pub(super) escapes: Option<super::Escapes>,
    pub(super) linked: Option<super::Linked>,
}

/// Exit-fuzz's bytes as the lowering hands them out (`jit.mode=exit-fuzz`):
/// the address of the first and how many there are, one per guard in the
/// order the lowering meets them. [`Fuzz::COUNTING`] hands out as many as
/// asked, at no address, for the pass that counts them.
#[derive(Debug, Clone, Copy)]
pub(super) struct Fuzz {
    pub(super) base: usize,
    pub(super) len: usize,
}

impl Fuzz {
    /// The bytes of the pass that counts the guards, which runs no code.
    pub(super) const COUNTING: Fuzz = Fuzz {
        base: 0,
        len: usize::MAX,
    };
}

/// The kinds a value of the scalar core is, in a register or in its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Long,
    Double,
    /// False or true, which the value says.
    Bool,
    Null,
    False,
    True,
}

impl Kind {
    /// The one kind `types` are, where they are one of the scalar core's.
    fn of(types: Types) -> Option<Kind> {
        if !types.scalar() {
            return None;
        }
        match types.tag() {
            Some(tag) => Kind::of_tag(tag),
            None => Some(Kind::Bool),
        }
    }

    /// The kind of a value of the type byte `tag`.
    fn of_tag(tag: ZvalType) -> Option<Kind> {
        Some(match tag {
            ZvalType::Long => Kind::Long,
            ZvalType::Double => Kind::Double,
            ZvalType::Null => Kind::Null,
            ZvalType::False => Kind::False,
            ZvalType::True => Kind::True,
            _ => return None,
        })
    }

    /// Whether a register of the kind holds a value the op that writes it
    /// defines, a long, a double or a boolean: a `null`, `false` or
    /// `true` register is a constant and defines nothing.
    fn carries(self) -> bool {
        matches!(self, Kind::Long | Kind::Double | Kind::Bool)
    }

    /// The Cranelift type a register of the kind is.
    fn ty(self) -> Type {
        match self {
            Kind::Long => I64,
            Kind::Double => F64,
            Kind::Bool | Kind::Null | Kind::False | Kind::True => I8,
        }
    }
}

/// A scalar compiled code holds: a long, a double, a boolean as an `i8` of
/// zero or one, or null.
#[derive(Debug, Clone, Copy)]
enum Scalar {
    Long(Value),
    Double(Value),
    Bool(Value),
    Null,
}

/// What an arithmetic computed: a long, a double, or, where a long one
/// overflowed or a division was inexact, either, which `overflowed`
/// says, what php's `ZEND_SIGNED_MULTIPLY_LONG` and `div_function`
/// leave it.
#[derive(Debug, Clone, Copy)]
enum Num {
    Long(Value),
    Double(Value),
    Either {
        overflowed: Value,
        long: Value,
        double: Value,
    },
}

/// An operand of an inline arithmetic that holds a number of either
/// kind ([`Emit::mixed`]): in its slot, its type byte read where it is
/// read, or kept in registers by the op before ([`Kept`]), its kind the
/// overflow bit that op computed.
#[derive(Debug, Clone, Copy)]
enum Mixed {
    Slot(Slot),
    Kept {
        overflowed: Value,
        long: Value,
        double: Value,
    },
}

/// An operand of a stepped arithmetic ([`Emit::stepped_numbers`]): a
/// number the trace holds, or a slot read by its type byte, with the
/// types php's analysis gives what it holds.
#[derive(Debug, Clone, Copy)]
enum Numeric {
    Held(Scalar),
    Slot(Slot, Types),
}

/// A number of either kind an inline arithmetic wrote to a temporary
/// that the inline arithmetic after it alone reads ([`Emit::keeps`]):
/// held in registers and not written, so the reader tests the overflow bit the
/// op computed rather than the type byte it would store, as php's
/// code keeps a value in a register its stack map stores only where it
/// leaves. A place that leaves before the reader is done writes it
/// first ([`Emit::leave_where`]), so the interpreter finds it where its
/// opline reads it.
#[derive(Debug, Clone, Copy)]
struct Kept {
    value: ValueId,
    slot: Slot,
    reader: OpId,
    num: Num,
}

/// The longs an arithmetic made that a site writes before it leaves past
/// itself ([`Emit::leave_past`]): the long it went to, and the long an
/// increment or a decrement read, which a `POST_*` that did not overflow
/// writes to its `out` (`Stored::Stayed`).
#[derive(Debug, Clone, Copy)]
struct Wrote {
    long: Value,
    read: Option<Value>,
}

/// Where a kernel serving `line` leaves: its exit, where an inline form
/// has one, else its unwind.
fn leave_of(line: &Line) -> Option<Leave> {
    (line.exit.map(Leave::Exit)).or_else(|| line.unwind.map(Leave::Unwind))
}

/// Where an opline's handler goes on: in its own frame, having done
/// nothing else to the frame chain but pop a native's call it made, or
/// thrown there. What [`Kernel::Handle`] serves, as php's code calls the
/// handler of each such opline it compiles no inline code for directly
/// (`zend_jit_handler`, `Z/ext/opcache/jit/zend_jit_ir.c:4120-4157`).
/// The kernel stands the frame at the opline before the handler runs,
/// php's `SAVE_OPLINE()`, so what the handler raises, and the unwind of
/// what it throws, see the frame where php's do, and compiled code
/// writes where it stands past it, which the handler leaves no `Stood`
/// of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Onward {
    /// This many oplines past it, one or two past an `OP_DATA`, with no
    /// IP guard, which `zend_jit_trace_handler` takes only past an
    /// opline `zend_jit_trace_may_exit` names (`:17129-17236`;
    /// `Z/ext/opcache/jit/zend_jit_trace.c:261-330`).
    Past(u32),
    /// Where its jump went, the frame's `EX(opline)` as the handler left
    /// it, held by the opline's direction guards: php's
    /// `zend_jit_trace_handler` past a branch `zend_jit_trace_may_exit`
    /// names, its IP compared with the opline the trace recorded next
    /// and its exit taken where they differ (`:17201-17271`).
    Went,
}

impl Onward {
    /// Where `opcode`'s handler goes on, for the oplines whose handler
    /// can neither leave its frame, push or replace one, nor enter the
    /// loop with the frame unstood: the jumps php's code compiles no
    /// inline form for, `?:`'s `JMP_SET`, `??`'s `COALESCE` and `?->`'s
    /// `JMP_NULL`, which go where their handler went
    /// ([`Onward::Went`]); and, past them, building an array and
    /// adding to it, a native's call, made and popped by its handler, a
    /// concatenation and a rope's pieces, whose string arms the handler
    /// holds as php's own functions over two strings hold them, an
    /// assignment, a write to an element and a compound one, a send, a
    /// frameless call to a native, `func_num_args()`, a string's length, a cast to a
    /// boolean, the free of a `foreach`'s array, a property's fetch,
    /// write, compound write, increment and decrement, whose `__get` and
    /// `__set` the handler serves through an `execute` of their own, as
    /// php's helpers do, and whose typed property it coerces or refuses,
    /// an element's fetch and `isset()`, whose `ArrayAccess` methods it
    /// serves so, and an increment or a decrement, of a `VAR` an inline
    /// fetch left indirect among them.
    /// None for every other
    /// opline: a `JMPZ` and its kin, which php's code compiles inline
    /// whatever its value (`zend_jit_bool_jmpznz`), and a comparison,
    /// whose handler may take the jump fused to it, a call's `INIT_*` and its `DO_*CALL` into user code,
    /// a return, an include or an `eval`, a generator's opcodes, a
    /// `finally`'s `FAST_CALL` and `FAST_RET`, and the two silence
    /// opcodes, whose observers a step tells. A property's `isset()` and
    /// `empty()` is among them, as php's code calls its handler; fused to
    /// the jump after it, its handler takes that jump, and its direction
    /// guard has it stepped, as every opline that holds a guard within it
    /// is ([`Emit::step_kernel`]).
    ///
    /// A property hook writes a variable through the reference the object
    /// holds to it as the handler serves its `get` and `set`, which the
    /// loop reads after; and `isset()` and `empty()` of a property served by
    /// their handler, written to a variable and fused to a jump each way:
    ///
    /// ```php
    /// class H { public $r; public int $v = 0 { get { $this->r = $this->r + 0.5; return $this->v; } set { $this->v = $value * 2; } } }
    /// function f($n) { $x = 0; $h = new H; $h->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $h->v = $i; $t += $h->v; $t += $x; } return [$t, $x]; }
    /// echo json_encode(f(10)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [117.5,5]
    /// ```
    ///
    /// ```php
    /// class H { public $r; public int $v = 0 { get { $this->r = $this->r + 0.5; return $this->v; } set { $this->v = $value * 2; } } }
    /// function f($n) { $x = 0; $h = new H; $h->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $h->v = $i; $t += $h->v; $t += $x; } return [$t, $x]; }
    /// echo json_encode(f(10)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [117.5,5]
    /// ```
    ///
    /// ```php
    /// class S { public $b = 0; public $n = null; public $z = "0"; function run($k) { $c = 0; for ($i = 0; $i < $k; $i++) { $x = isset($this->b); $y = empty($this->z); if (isset($this->n)) { $c += 100; } if (!empty($this->b)) { $c += 10; } $c += $x + $y; $this->b = $i % 2; $this->n = $i == 3 ? 1 : null; } return $c; } }
    /// echo (new S)->run(8), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 146
    /// ```
    ///
    /// ```php
    /// class S { public $b = 0; public $n = null; public $z = "0"; function run($k) { $c = 0; for ($i = 0; $i < $k; $i++) { $x = isset($this->b); $y = empty($this->z); if (isset($this->n)) { $c += 100; } if (!empty($this->b)) { $c += 10; } $c += $x + $y; $this->b = $i % 2; $this->n = $i == 3 ? 1 : null; } return $c; } }
    /// echo (new S)->run(8), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 146
    /// ```
    ///
    /// A `__get` the handler serves writes a variable through the
    /// reference an object holds to it, which the loop reads after; a
    /// `__set` writes one, and the object's declared property; a typed
    /// property's coercion throws, caught in the loop: none of the three
    /// oplines has the `call` effect, and the trace keeps no fact the user
    /// code can change, a reference's inner type held at every read, as
    /// revision 83's analysis has it (`jit-design.md` §4.2):
    ///
    /// ```php
    /// class G { public $r; private $v = 3; function __get($n) { $this->r = $this->r + 0.5; return $this->v; } }
    /// function f($n) { $x = 0; $o = new G; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->missing; $t += $x; $o->r = $i; } return [$t, $x]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [241,19]
    /// ```
    ///
    /// ```php
    /// class G { public $r; private $v = 3; function __get($n) { $this->r = $this->r + 0.5; return $this->v; } }
    /// function f($n) { $x = 0; $o = new G; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->missing; $t += $x; $o->r = $i; } return [$t, $x]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [241,19]
    /// ```
    ///
    /// ```php
    /// class S { public $p = 0; public $r; function __set($n, $v) { $this->r = $v * 2.5; $this->p = $this->p + 1; } }
    /// function f($n) { $x = 0; $o = new S; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $o->missing = $i; $t += $x + $o->p; } return [$t, $x, $o->p]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [685,47.5,20]
    /// ```
    ///
    /// ```php
    /// class S { public $p = 0; public $r; function __set($n, $v) { $this->r = $v * 2.5; $this->p = $this->p + 1; } }
    /// function f($n) { $x = 0; $o = new S; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $o->missing = $i; $t += $x + $o->p; } return [$t, $x, $o->p]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [685,47.5,20]
    /// ```
    ///
    /// ```php
    /// class T { public int $n = 0; public float $f = 0.0; }
    /// function f($n) { $o = new T; $c = 0; for ($i = 0; $i < $n; $i++) { try { $o->n = $i == 7 ? "x" : $i; $o->f = $i; $c += $o->n + $o->f; } catch (TypeError $e) { $c += 100; } } return [$c, $o->n, $o->f]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [466,19,19]
    /// ```
    ///
    /// ```php
    /// class T { public int $n = 0; public float $f = 0.0; }
    /// function f($n) { $o = new T; $c = 0; for ($i = 0; $i < $n; $i++) { try { $o->n = $i == 7 ? "x" : $i; $o->f = $i; $c += $o->n + $o->f; } catch (TypeError $e) { $c += 100; } } return [$c, $o->n, $o->f]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [466,19,19]
    /// ```
    ///
    /// An `ArrayAccess` object's `offsetGet`, `offsetExists` and
    /// `offsetSet`, which the handlers of an element's fetch, `isset()`
    /// and write serve, write a variable through the reference the object
    /// holds to it, which the loop reads after:
    ///
    /// ```php
    /// class A implements ArrayAccess { public $r; function offsetGet($k): mixed { $this->r = $this->r + 0.5; return 3; } function offsetExists($k): bool { $this->r = $this->r + 1; return $k == 'x'; } function offsetSet($k, $v): void { $this->r = $v; } function offsetUnset($k): void {} }
    /// function f($n) { $x = 0; $o = new A; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o['k']; $t += $x; if (isset($o['x'])) { $t++; } $t += $x; $o['k'] = $i; } return [$t, $x]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [462,19]
    /// ```
    ///
    /// ```php
    /// class A implements ArrayAccess { public $r; function offsetGet($k): mixed { $this->r = $this->r + 0.5; return 3; } function offsetExists($k): bool { $this->r = $this->r + 1; return $k == 'x'; } function offsetSet($k, $v): void { $this->r = $v; } function offsetUnset($k): void {} }
    /// function f($n) { $x = 0; $o = new A; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o['k']; $t += $x; if (isset($o['x'])) { $t++; } $t += $x; $o['k'] = $i; } return [$t, $x]; }
    /// echo json_encode(f(20)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [462,19]
    /// ```
    ///
    /// An array built per turn, and a key the build throws at, caught in
    /// the frame:
    ///
    /// ```php
    /// function pairs($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $p = [$i, $i * 2, "k" => $i]; $s += $p[1] + $p["k"]; } return $s; }
    /// function bad($n) { $c = 0; for ($i = 0; $i < $n; $i++) { try { $a = [$i => 1, ($i == 5 ? [] : $i + 1) => 2]; $c += count($a); } catch (TypeError $e) { $c += 100; } } return $c; }
    /// echo pairs(20), " ", bad(8), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 570 114
    /// ```
    ///
    /// A line of fields joined, split and normalised, each piece of it
    /// served by its handler:
    ///
    /// ```php
    /// function line($i) { $p = "/r/" . ($i % 7) . "-x"; $s = "  $i , $p ,  ok-$i "; $t = ""; $t .= $s; $t .= "|"; return $t; }
    /// function munge($n) {
    ///     $c = 0; $all = "";
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $f = explode(",", line($i)); $f[1] = trim($f[1]); $f[2] = str_replace("ok", "OK", $f[2]);
    ///         $all .= strtoupper($f[1]) . substr($f[2], 0, 4); $c += strlen($f[0]) + (strpos($f[2], "OK") !== false);
    ///     }
    ///     return [$c, strlen($all), crc32($all)];
    /// }
    /// echo json_encode(munge(40)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [230,400,1340466966]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function line($i) { $p = "/r/" . ($i % 7) . "-x"; $s = "  $i , $p ,  ok-$i "; $t = ""; $t .= $s; $t .= "|"; return $t; }
    /// function munge($n) {
    ///     $c = 0; $all = "";
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $f = explode(",", line($i)); $f[1] = trim($f[1]); $f[2] = str_replace("ok", "OK", $f[2]);
    ///         $all .= strtoupper($f[1]) . substr($f[2], 0, 4); $c += strlen($f[0]) + (strpos($f[2], "OK") !== false);
    ///     }
    ///     return [$c, strlen($all), crc32($all)];
    /// }
    /// echo json_encode(munge(40)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [230,400,1340466966]
    /// ```
    ///
    /// A handled concatenation that throws, an object no string converts,
    /// caught in the loop, under force and where every guard fails once:
    ///
    /// ```php
    /// function f($n) { $c = 0; $o = new stdClass; for ($i = 0; $i < $n; $i++) { try { $s = "x" . ($i == 3 ? $o : $i); $t = "<$s>"; $t .= $i; $c += strlen($t); } catch (Error $e) { $c += 100; } } return $c; }
    /// echo f(8), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 135
    /// ```
    ///
    /// ```php
    /// function f($n) { $c = 0; $o = new stdClass; for ($i = 0; $i < $n; $i++) { try { $s = "x" . ($i == 3 ? $o : $i); $t = "<$s>"; $t .= $i; $c += strlen($t); } catch (Error $e) { $c += 100; } } return $c; }
    /// echo f(8), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 135
    /// ```
    ///
    /// A handled concatenation whose `__toString` writes a variable through
    /// the reference an object holds to it, which the loop reads after:
    /// the handler serves user code with no `call` effect, and the trace
    /// keeps no fact that code can change, as revision 83's analysis has
    /// it (`jit-design.md` §4.2), a reference's inner type held at every
    /// read:
    ///
    /// ```php
    /// class S { public $r; function __toString(): string { $this->r = $this->r + 1; return "s"; } }
    /// function f($n) { $x = 0; $o = new S; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $c = "a" . $o; $t += $x; } return [$t, $x, $c]; }
    /// echo json_encode(f(10)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [55,10,"as"]
    /// ```
    ///
    /// ```php
    /// class S { public $r; function __toString(): string { $this->r = $this->r + 1; return "s"; } }
    /// function f($n) { $x = 0; $o = new S; $o->r = &$x; $t = 0; for ($i = 0; $i < $n; $i++) { $c = "a" . $o; $t += $x; } return [$t, $x, $c]; }
    /// echo json_encode(f(10)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [55,10,"as"]
    /// ```
    fn of(opcode: Opcode) -> Option<Onward> {
        match opcode {
            Opcode::JmpSet | Opcode::Coalesce | Opcode::JmpNull => Some(Onward::Went),
            Opcode::InitArray
            | Opcode::AddArrayElement
            | Opcode::DoIcall
            | Opcode::Concat
            | Opcode::FastConcat
            | Opcode::RopeInit
            | Opcode::RopeAdd
            | Opcode::RopeEnd
            | Opcode::Assign
            | Opcode::AssignOp
            | Opcode::SendVal
            | Opcode::SendVar
            | Opcode::FramelessIcall0
            | Opcode::FramelessIcall1
            | Opcode::FramelessIcall2
            | Opcode::Strlen
            | Opcode::Bool
            | Opcode::FeFree
            | Opcode::FuncNumArgs
            | Opcode::FetchObjR
            | Opcode::FetchObjIs
            | Opcode::FetchObjW
            | Opcode::FetchObjRw
            | Opcode::PreIncObj
            | Opcode::PreDecObj
            | Opcode::PostIncObj
            | Opcode::PostDecObj
            | Opcode::IssetIsemptyPropObj
            | Opcode::FetchDimR
            | Opcode::FetchDimIs
            | Opcode::FetchDimW
            | Opcode::FetchDimRw
            | Opcode::IssetIsemptyDimObj
            | Opcode::PreInc
            | Opcode::PreDec
            | Opcode::PostInc
            | Opcode::PostDec => Some(Onward::Past(1)),
            Opcode::AssignDim
            | Opcode::AssignDimOp
            | Opcode::AssignObj
            | Opcode::AssignObjOp
            | Opcode::FramelessIcall3 => Some(Onward::Past(2)),
            _ => None,
        }
    }

    /// Whether the handler alone serves an opline that holds `guards`
    /// within it: none past an opline that goes on past itself, and
    /// past a jump its direction guards alone, which hold where it went.
    fn serves(self, guards: &[Guard]) -> bool {
        match self {
            Onward::Past(_) => guards.is_empty(),
            Onward::Went => guards
                .iter()
                .all(|guard| matches!(guard.check, Check::Direction { .. })),
        }
    }
}

/// What the handler of `?:`, `??` or `?->` tests of its operand, which
/// [`Emit::tested`] tests in place where it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Test {
    /// `JMP_SET`: its truth, `zend_is_true`; the jump taken on true.
    Truth,
    /// `COALESCE`: that it is defined and not `null`; the jump taken so.
    Defined,
    /// `JMP_NULL`: that it is not `null`; the jump taken where it is.
    NotNull,
}

impl Test {
    /// The test `opcode`'s handler makes, for the three.
    fn of(opcode: Opcode) -> Option<Test> {
        match opcode {
            Opcode::JmpSet => Some(Test::Truth),
            Opcode::Coalesce => Some(Test::Defined),
            Opcode::JmpNull => Some(Test::NotNull),
            _ => None,
        }
    }
}

/// How a value given up is given up: as php's `zval_ptr_dtor`, which
/// buffers a possible root where other owners stand, or as
/// `zval_ptr_dtor_nogc`, a temporary's `FREE_OP`, which never does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dtor {
    Gc,
    Nogc,
}

/// One trace's function as it is built.
struct Emit<'a> {
    ir: &'a TraceIr,
    b: FunctionBuilder<'a>,
    call_conv: CallConv,
    symbols: &'a Symbols,
    /// The plain calls whose headers the code copies, where they live as
    /// long as it ([`super::Compiled::calls`]).
    held: &'a [PlainCall],
    /// The request's VM stack, read once at entry by a trace that pushes,
    /// makes or leaves a call in place.
    stack: Option<Value>,
    /// Per frame of the trace, its place on the stack, a `FrameRef`'s
    /// number, kept where `stack` is: none until the entry, the frame's
    /// `Op::Enter` or the `Op::Back` that lands in it defined it
    /// ([`Emit::place_frame`]).
    refs: Vec<Option<Variable>>,
    /// The place of the frame the last return leaves to, which its
    /// `Op::Back` names the frame it lands in by.
    returned_to: Option<Variable>,
    /// The `ExitCtx`, the function's first parameter.
    cx: Option<Value>,
    /// The request's addresses of the trace, its fourth parameter: its
    /// calls' headers, its cache slots and its doors' words; and whether
    /// the request makes the trace's calls in place, from the entry word
    /// ([`ENTRY_UNREADY`]): an `i8` that is zero where it does.
    /// Whether the body keeps the [`Stood`]'s address in its frame
    /// ([`HELD_STOOD`]).
    stood_held: bool,
    entry_held: Option<EntryHeld>,
    /// The address of the request's interrupt byte, read once as a
    /// looping trace whose header tests it enters.
    interrupt: Option<Value>,
    /// Where a kernel writes where the interpreter stands, which compiled
    /// code reads back into [`Emit::stood_vars`] past the call.
    stood: StackSlot,
    /// Where the interpreter stands as compiled code knows it, the
    /// [`Stood`] the last kernel wrote or code in place stood, held in
    /// variables and never stored: a word no path reads costs nothing,
    /// as php's code keeps `EX(opline)` and `EX(call)` in registers where
    /// it knows them and stores them only for the handler that reads
    /// them.
    stood_vars: StoodVars,
    /// Where a kernel that answers a word or a zval writes it, made where
    /// one is first called.
    answer: Option<StackSlot>,
    /// The trace's room for a frameless call's operand zvals, laid once
    /// ([`Emit::frameless_room`]).
    frameless: Option<StackSlot>,
    /// Per frame of the trace, the room it runs in.
    rooms: Vec<Variable>,
    /// Per value the trace keeps in a register, its variable.
    regs: Vec<Option<Variable>>,
    /// The rooms of the calls under way, the innermost last, each with
    /// the plain call it is where compiled code pushed it in place and
    /// what held the page at that push.
    calls: Vec<(CallRoom, Option<(&'a PlainCall, PushRoom)>)>,
    /// Per frame of the trace, what the call made in place that entered
    /// it fixes of it.
    held_calls: Vec<Option<calls::Made>>,
    /// Per kernel, its signature once imported.
    sigs: [Option<SigRef>; Kernel::ALL.len()],
    /// Per exit, its block once a site branches to it.
    exits: Vec<Option<Block>>,
    /// Per unwind, its block once a kernel's status branches to it.
    unwinds: Vec<Option<Block>>,
    /// The block a kernel's refusal with no exit to take answers from.
    abandon: Option<Block>,
    /// The block a return past the lowest frame escapes from.
    escape: Option<Block>,
    /// The block the interrupt test answers [`INTERRUPTED`] from, where
    /// it takes php's stub.
    stub: Option<Block>,
    /// The block a body entry that declines answers [`DECLINED`] from.
    declined: Option<Block>,
    /// The op being lowered, which reads a value as a guard before it
    /// left it ([`Emit::value`]).
    reading: Option<OpId>,
    /// The frame the trace runs in now.
    running: Frame,
    /// Under `jit.mode=exit-fuzz`, the bytes each guard fails once by.
    fuzz: Option<Fuzz>,
    /// How many of them the guards met so far took.
    fuzzed_sites: usize,
    /// The room [`Emit::carrying`] made.
    carrying: Option<StackSlot>,
    /// What the last call carried ([`Emit::doubles_held`]).
    carried_last: Vec<Variable>,
    /// What the guards of the op being lowered read of its array.
    guards_read: GuardsRead,
    /// The registers a memory phi's back edge brings that their op writes
    /// to their slot as it defines them, php's `ZREG_STORE`, so they need
    /// live no further than their last read ([`stored_at_def`]).
    stored_at_def: HashSet<ValueId>,
    /// Its number's address and its link (`doors`).
    joins: Joins,
    /// Per exit, the block a failed guard leaves it through where the
    /// exit may jump to its side trace, taking the call under way there.
    doors: Vec<Option<Block>>,
    /// The doors the code jumps through, in the order it made them,
    /// which is the order their words stand in ([`Door`]).
    made: Vec<Door>,
    /// Per exit, the block that answers its number and stores nothing,
    /// where a jump finds no trace to enter.
    answers: Vec<Option<Block>>,
    /// The number the last inline arithmetic kept for the op after it.
    kept: Option<Kept>,
    /// The loads made for a [`Stood`] word alone, dropped where no path
    /// reads it ([`unread::unread`]).
    stood_loads: Vec<cranelift_codegen::ir::Inst>,
}

impl<'a> Emit<'a> {
    fn new(
        ir: &'a TraceIr,
        mut b: FunctionBuilder<'a>,
        call_conv: CallConv,
        (symbols, held): (&'a Symbols, &'a [PlainCall]),
    ) -> Emit<'a> {
        let rooms = ir.frames.iter().map(|_| b.declare_var(I64)).collect();
        let stood_vars = StoodVars {
            frame: b.declare_var(I64),
            call: b.declare_var(I64),
            func: b.declare_var(I64),
            opline: b.declare_var(I64),
            room: b.declare_var(I64),
        };
        let stood = b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            STOOD_SLOT.unsigned_abs(),
            3,
        ));
        Emit {
            ir,
            b,
            call_conv,
            symbols,
            held,
            stack: None,
            refs: vec![None; ir.frames.len()],
            returned_to: None,
            cx: None,
            entry_held: None,
            stood_held: false,
            interrupt: None,
            stood,
            stood_vars,
            answer: None,
            frameless: None,
            rooms,
            regs: vec![None; ir.values.len()],
            calls: Vec::new(),
            held_calls: vec![None; ir.frames.len()],
            sigs: [None; Kernel::ALL.len()],
            exits: vec![None; ir.exits.len()],
            unwinds: vec![None; ir.unwinds.len()],
            abandon: None,
            escape: None,
            stub: None,
            declined: None,
            reading: None,
            running: Frame::ROOT,
            fuzz: None,
            fuzzed_sites: 0,
            carrying: None,
            carried_last: Vec::new(),
            guards_read: GuardsRead::default(),
            stored_at_def: stored_at_def(ir),
            joins: Joins {
                number: 0,
                escapes: None,
                linked: None,
            },
            doors: vec![None; ir.exits.len()],
            made: Vec::new(),
            answers: vec![None; ir.exits.len()],
            kept: None,
            stood_loads: Vec::new(),
        }
    }

    // ------------------------------------------------------------ the trace

    /// The whole trace: its entry and the guards held there, the ops
    /// before a loop's header, the header with its interrupt test, the
    /// body and the back edge, or the exit a trace that does not loop
    /// leaves through; then every exit's block.
    fn trace(&mut self) -> Result<(), Abort> {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.b.switch_to_block(entry);
        self.b.seal_block(entry);
        let [cx, room, word, images, call, stands] = *self.b.block_params(entry) else {
            return Err(mismatch(
                "a trace's function takes its entry's six parameters",
            ));
        };
        self.cx = Some(cx);
        self.b.def_var(self.room_var(Frame::ROOT)?, room);
        let header = match self.ir.end {
            End::Loop(header) => Some(header),
            End::Leave(_) | End::Return(_) => None,
        };
        if header.is_some_and(|header| header.interrupt.tested()) {
            self.interrupt = Some(self.b.ins().load(I64, trusted(), cx, INTERRUPT_AT));
        }
        // The stack and the entered frame's place on it, which calls made
        // in place read, and a jump to another trace hands it.
        self.stack = Some(self.b.ins().load(I64, trusted(), cx, STACK_AT));
        let place = self.b.ins().band_imm_u(word, ENTRY_PLACE);
        self.place_frame(Frame::ROOT, place)?;
        self.hold_entry(images, word, header.is_some());
        // Laid down before either way in: a guard at the body's entry
        // reads it too.
        self.stood_at_entry([room, call, stands])?;
        // A trace entered at its loop's body goes there; at its first op,
        // on.
        let body = match &self.ir.body {
            Some(body) => {
                let (into, first) = (self.b.create_block(), self.b.create_block());
                let at_body = self.b.ins().band_imm_u(word, ENTRY_BODY);
                self.b.ins().brif(at_body, into, &[], first, &[]);
                self.b.switch_to_block(first);
                self.b.seal_block(first);
                Some((body, into, self.b.create_block()))
            }
            None => None,
        };
        for guard in self.ir.guards.iter().filter(|g| g.at == GuardAt::Entry) {
            self.guard(*guard)?;
        }
        let split = header.map_or(self.ir.ops.len(), |header| header.at.index());
        self.ops(0..split, body)?;
        match self.ir.end {
            End::Loop(header) => {
                let phis = self.register_phis();
                let entered = phis
                    .iter()
                    .map(|&(phi, entry, _)| Ok((phi, self.entered(phi, entry)?)))
                    .collect::<Result<Vec<_>, Abort>>()?;
                for (phi, value) in entered {
                    self.define(phi, value)?;
                }
                let head = self.b.create_block();
                self.b.ins().jump(head, &[]);
                self.b.switch_to_block(head);
                self.b.set_srcloc(located(header.at.index()));
                // The interrupt test, one load and one branch (§4.3); a
                // loop by recursive return takes none, as php's takes none.
                match header.interrupt {
                    Interrupt::Exit(exit) => self.interrupt_test(Raised::Exit(exit))?,
                    Interrupt::Stub(_) => self.interrupt_test(Raised::Stub)?,
                    Interrupt::None => {}
                }
                self.ops(split..self.ir.ops.len(), body)?;
                let brought = phis
                    .iter()
                    .map(|&(phi, _, back)| Ok((phi, self.register(back)?)))
                    .collect::<Result<Vec<_>, Abort>>()?;
                for (phi, value) in brought {
                    self.define(phi, value)?;
                }
                self.store_brought()?;
                if matches!(header.repeat, Repeat::Call | Repeat::Return) {
                    // A recursion's next turn runs one frame on, where
                    // its last call or return reached, in the frame its
                    // header serves.
                    let looped = match self.ir.ops.get(header.at.index()) {
                        Some(Op::Line(line)) => line.frame,
                        _ => Frame::ROOT,
                    };
                    let last = self.room(self.running)?;
                    self.b.def_var(self.room_var(looped)?, last);
                    if self.stack.is_some() {
                        let place = self.frame_ref(self.running)?;
                        self.place_frame(looped, place)?;
                    }
                }
                self.b.ins().jump(head, &[]);
                self.b.seal_block(head);
            }
            End::Leave(exit) => self.leave_at_end(exit)?,
            // The `RETURN` left in place went on to the escape or the exit.
            End::Return(_) => {}
        }
        if let Some((entry, into, block)) = body {
            self.b.set_srcloc(SourceLoc::default());
            self.b.switch_to_block(into);
            self.b.seal_block(into);
            self.enter_body(entry, block)?;
            self.b.seal_block(block);
        }
        self.cold(Self::exit_blocks)
    }

    /// Where the frame stands as the trace enters, from its entry, held
    /// as where the last kernel left it, for a trace whose code reads it
    /// ([`super::stood_at_entry`]): the frame it is entered on, the call
    /// under way in it, its op array and opline from the one word, and the
    /// room the stack's top page has left, read where the code reads it.
    fn stood_at_entry(&mut self, [room, call, stands]: [Value; 3]) -> Result<(), Abort> {
        if !super::stood_at_entry(self.ir) {
            return Ok(());
        }
        let func = self.b.ins().ireduce(I32, stands);
        let opline = self.b.ins().ushr_imm_u(stands, 32);
        let opline = self.b.ins().ireduce(I32, opline);
        let stack = self.stack()?;
        let top = self.b.ins().load(I64, trusted(), stack, calls::TOP);
        let end = self.b.ins().load(I64, trusted(), stack, calls::END);
        self.stood_loads([top, end]);
        let left = self.b.ins().isub(end, top);
        self.stand([room, call, func, opline, left]);
        Ok(())
    }

    /// Answer `answer`, the trace's number written in the `ExitCtx` first
    /// ([`TRACE_AT`]), so `enter` reads it against this trace however it
    /// was entered.
    fn answer_with(&mut self, answer: Value) -> Result<(), Abort> {
        let cx = self
            .cx
            .ok_or_else(|| mismatch("an answer before the entry"))?;
        let number = self.number()?;
        self.b.ins().store(trusted(), number, cx, TRACE_AT);
        self.b.ins().return_(&[answer]);
        Ok(())
    }

    /// The trace's number, an `i32` read where the publish wrote it
    /// (`super::Numbered`).
    fn number(&mut self) -> Result<Value, Abort> {
        let at = i64::try_from(self.joins.number)
            .map_err(|_| mismatch("a trace's number past the host's addresses"))?;
        let at = self.b.ins().iconst(I64, at);
        Ok(self.b.ins().load(I32, trusted(), at, 0))
    }

    /// The ops of `range`, the way in at the body (`body`: the entry, its
    /// block and the block the entry jumps to) opened where its op is.
    fn ops(
        &mut self,
        range: std::ops::Range<usize>,
        body: Option<(&BodyEntry, Block, Block)>,
    ) -> Result<(), Abort> {
        for at in range {
            if let Some((entry, _, block)) = body
                && entry.at.index() == at
            {
                self.b.ins().jump(block, &[]);
                self.b.switch_to_block(block);
            }
            self.op(at)?;
        }
        Ok(())
    }

    /// The way in at a loop's body (`types::BodyEntry`): every guard the
    /// trace holds at entry, and every value it holds there, read from
    /// the frame and held to what the trace knows of it, its type byte
    /// one of its kinds' and an array's layout the one it knows, a
    /// register's loaded into it; anything else declines the entry,
    /// [`DECLINED`], taking no exit. Then on at the body's first op, `to`.
    fn enter_body(&mut self, body: &BodyEntry, to: Block) -> Result<(), Abort> {
        let entered: Vec<Guard> = (self.ir.guards.iter())
            .filter(|guard| guard.at == GuardAt::Entry)
            .copied()
            .collect();
        for guard in entered {
            match guard.check {
                Check::Type { value, tag: t } => {
                    let slot = self.value(value)?.slot;
                    self.decline_unless(slot, &[t], None)?;
                }
                Check::Layout {
                    value,
                    layout,
                    read: Read::InPlace,
                } => {
                    let slot = self.value(value)?.slot;
                    self.decline_unless(slot, &[ZvalType::Array], Some(layout))?;
                }
                Check::Layout {
                    read: Read::Through | Read::Indirect(_),
                    ..
                } => return Err(mismatch("a reference's layout held as a trace enters")),
                Check::Indirect { .. } => {
                    return Err(mismatch("an indirect `VAR` held as a trace enters"));
                }
                Check::Reference { .. } => {
                    return Err(mismatch("an assignment's reference held as a trace enters"));
                }
                Check::Deref { value, tag: t } => {
                    let (room, at) = self.place(self.value(value)?.slot)?;
                    let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                    let reference = tag(ZvalType::Reference);
                    let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, reference);
                    self.decline_if(other);
                    let held = self.b.ins().load(I64, trusted(), room, at);
                    let inner = imm(Ref::VALUE_AT) + TYPE_AT;
                    let inner = self.b.ins().load(I8, trusted(), held, inner);
                    let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, inner, tag(t));
                    self.decline_if(other);
                }
                Check::Room { bytes } => {
                    let short = self.short_of(bytes);
                    self.decline_if(short);
                }
                Check::Direction { .. }
                | Check::Stands { .. }
                | Check::Callee { .. }
                | Check::Arm { .. }
                | Check::Class { .. } => {
                    return Err(mismatch("a body entry past a guard it cannot hold"));
                }
            }
        }
        for &(slot, value) in &body.holds {
            let held = self.value(value)?;
            let layout = match held.types.tag() {
                Some(ZvalType::Array) if !held.types.either_layout() => {
                    Some(if held.types.meets(Types::PACKED) {
                        Layout::Packed
                    } else {
                        Layout::Hash
                    })
                }
                _ => None,
            };
            if let Some(tags) = held.types.tags() {
                self.decline_unless(slot, &tags.collect::<Vec<_>>(), layout)?;
            }
            if held.home == Home::Register {
                let kind =
                    Kind::of(held.types).ok_or_else(|| mismatch("a register of no scalar kind"))?;
                let loaded = self.load(slot, kind, false)?;
                self.define(value, loaded)?;
            }
        }
        self.b.ins().jump(to, &[]);
        Ok(())
    }

    /// Decline a body entry where `slot` holds none of `tags`, or an
    /// array not laid out as `layout` says.
    fn decline_unless(
        &mut self,
        slot: Slot,
        tags: &[ZvalType],
        layout: Option<Layout>,
    ) -> Result<(), Abort> {
        let (room, at) = self.place(slot)?;
        let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
        let mut other = self.b.ins().iconst(I8, 1);
        for t in tags {
            let not = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, tag(*t));
            other = self.b.ins().band(other, not);
        }
        self.decline_if(other);
        if let Some(layout) = layout {
            let array = self.b.ins().load(I64, trusted(), room, at);
            let flags = self.b.ins().load(I32, trusted(), array, imm(Arr::FLAGS_AT));
            let packed = self.b.ins().band_imm_u(flags, i64::from(Arr::PACKED));
            let cc = match layout {
                Layout::Packed => IntCC::Equal,
                Layout::Hash => IntCC::NotEqual,
            };
            let other = self.b.ins().icmp_imm_s(cc, packed, 0);
            self.decline_if(other);
        }
        Ok(())
    }

    /// Whether the stack's top page has less room than `bytes` for
    /// frames, where the last kernel, or `enter`, stood it.
    fn short_of(&mut self, bytes: u32) -> Value {
        let room = self.stood_word(I64, STOOD_ROOM);
        self.b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThan, room, i64::from(bytes))
    }

    /// Answer [`DECLINED`] where `cond` is not zero.
    fn decline_if(&mut self, cond: Value) {
        let to = if let Some(block) = self.declined {
            block
        } else {
            let block = self.b.create_block();
            self.b.set_cold_block(block);
            self.declined = Some(block);
            block
        };
        let on = self.b.create_block();
        self.b.ins().brif(cond, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
    }

    /// The phis the trace keeps in registers: each, what it enters with
    /// and what its back edge brings.
    fn register_phis(&self) -> Vec<(ValueId, ValueId, ValueId)> {
        (0u32..)
            .map(ValueId)
            .zip(&self.ir.values)
            .filter_map(|(id, value)| match value.def {
                Def::Phi { entry, back } if value.home == Home::Register => Some((id, entry, back)),
                _ => None,
            })
            .collect()
    }

    /// What the back edge brings to a phi that stays in its slot, where a
    /// register holds it, written to the slot the next turn reads it from,
    /// as php stores a phi's source that has a register where the phi has
    /// none (`ZREG_STORE`, `Z/ext/opcache/jit/zend_jit_trace.c:3335-3337`).
    ///
    /// A variable that enters its loop as one type and turns another in
    /// it, read past the loop, in a loop on its own and in one a side
    /// trace links to from a pass of the loop around it, and the boolean
    /// of an `&&` in the loop:
    ///
    /// ```php
    /// function show($x) { return var_export($x, true); }
    /// function one($n) { $x = 7; for ($j = 0; $j < $n; $j++) { $x = $j * 1.5; } return $x; }
    /// function two($n) { $x = 7; for ($j = 0; $j < $n; $j++) { $x = $j == 3; } return show($x); }
    /// function inner($n) { $o = []; for ($i = 0; $i < $n; $i++) { $x = 7; for ($j = 0; $j < 3; $j++) { $x = $j * 1.5; } $o[] = show($x); } return implode(" ", $o); }
    /// function andor($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; for ($j = 0; $j < 3; $j++) { $x = $d && $j; } $o[] = show($x); } return implode(" ", $o); }
    /// echo one(6), " ", two(6), " ", two(4), "\n", inner(5), "\n", andor([0.0, 2.5], 6), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 7.5 false true
    /// 3.0 3.0 3.0 3.0 3.0
    /// false true false true false true
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function show($x) { return var_export($x, true); }
    /// function one($n) { $x = 7; for ($j = 0; $j < $n; $j++) { $x = $j * 1.5; } return $x; }
    /// function two($n) { $x = 7; for ($j = 0; $j < $n; $j++) { $x = $j == 3; } return show($x); }
    /// function inner($n) { $o = []; for ($i = 0; $i < $n; $i++) { $x = 7; for ($j = 0; $j < 3; $j++) { $x = $j * 1.5; } $o[] = show($x); } return implode(" ", array_slice($o, -2)); }
    /// function andor($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; for ($j = 0; $j < 3; $j++) { $x = $d && $j; } $o[] = show($x); } return implode(" ", array_slice($o, -2)); }
    /// echo one(40), " ", two(40), " ", two(4), "\n", inner(40), "\n", andor([0.0, 2.5], 40), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 58.5 false true
    /// 3.0 3.0
    /// false true
    /// ```
    fn store_brought(&mut self) -> Result<(), Abort> {
        let ir = self.ir;
        for value in &ir.values {
            let Def::Phi { back, .. } = value.def else {
                continue;
            };
            let brought = self.value(back)?;
            if value.home == Home::Memory
                && brought.home == Home::Register
                && !self.stored_at_def.contains(&back)
            {
                let held = self.register(back)?;
                self.store(brought.slot, held)?;
            }
        }
        Ok(())
    }

    /// What the phi `phi` enters its loop with, its entry value read as
    /// the kind the phi is.
    fn entered(&mut self, phi: ValueId, entry: ValueId) -> Result<Scalar, Abort> {
        let held = self.value(entry)?;
        if held.home == Home::Register {
            return self.register(entry);
        }
        let kind = Kind::of(self.value(phi)?.types)
            .ok_or_else(|| mismatch("a register phi of no scalar kind"))?;
        self.load(held.slot, kind, false)
    }

    /// The op at `at`: the guards held before it, then the op.
    fn op(&mut self, at: usize) -> Result<(), Abort> {
        let id = OpId(u32::try_from(at).map_err(|_| mismatch("too many ops"))?);
        self.b.set_srcloc(located(at));
        if self.kept.is_some_and(|kept| kept.reader != id) {
            self.write_kept()?;
        }
        self.reading = Some(id);
        self.guards_read = GuardsRead::default();
        let done = self.op_at(id, at);
        self.reading = None;
        if self.kept.is_some_and(|kept| kept.reader == id) {
            self.kept = None;
        }
        done
    }

    /// The op at `at`, its guards before it held first.
    fn op_at(&mut self, id: OpId, at: usize) -> Result<(), Abort> {
        let before: Vec<Guard> = self
            .ir
            .guards
            .iter()
            .filter(|g| g.at == GuardAt::Before(id))
            .copied()
            .collect();
        for guard in before {
            self.guard(guard)?;
        }
        let ir = self.ir;
        let Some(op) = ir.ops.get(at) else {
            return Err(mismatch("an op past the trace"));
        };
        match op {
            Op::Line(line) => {
                self.running = line.frame;
                match line.form {
                    Form::Step => self.stepped(id, line),
                    Form::Inline => self.inline(id, line),
                }
            }
            Op::InitCall {
                began: Began::Here,
                plain,
                room,
                ..
            } => {
                let call = self.stood_word(I64, STOOD_CALL);
                let var = self.b.declare_var(I64);
                self.b.def_var(var, call);
                let plain = plain.as_ref().map(|plain| (plain, *room));
                self.calls.push((CallRoom::Held(var), plain));
                Ok(())
            }
            Op::InitCall {
                began: Began::Before,
                ..
            } => {
                self.calls.push((CallRoom::Stood, None));
                Ok(())
            }
            Op::Enter { frame, stores } => {
                for &value in stores {
                    let held = self.register(value)?;
                    self.store(self.value(value)?.slot, held)?;
                }
                let room = self.stood_word(I64, STOOD_FRAME);
                self.b.def_var(self.room_var(*frame)?, room);
                self.running = *frame;
                if let Some(stack) = self.stack {
                    // The frame entered is the stack's top record.
                    let len = self.b.ins().load(I64, trusted(), stack, calls::FRAMES_LEN);
                    let place = self.b.ins().iadd_imm_s(len, -1);
                    self.place_frame(*frame, place)?;
                }
                Ok(())
            }
            Op::Back { frame, escapes, .. } => {
                let room = self.stood_word(I64, STOOD_FRAME);
                self.b.def_var(self.room_var(*frame)?, room);
                self.running = *frame;
                if let Some(var) = self.returned_to.take() {
                    let place = self.b.use_var(var);
                    self.place_frame(*frame, place)?;
                }
                if let Some((func, opline)) = *escapes {
                    self.escapes(func, opline)?;
                }
                if !self.within(id).is_empty() {
                    return Err(mismatch("a return holds a guard"));
                }
                Ok(())
            }
        }
    }

    // ------------------------------------------------------------ guards

    /// The guards the op at `at` holds itself.
    fn within(&self, at: OpId) -> Vec<Guard> {
        self.ir
            .guards
            .iter()
            .filter(|g| g.at == GuardAt::Within(at))
            .copied()
            .collect()
    }

    /// A guard held at entry or before an op: what it holds read from the
    /// value's slot, or from its register where a boolean is held to the
    /// one the recording saw, or at entry where the frame stands,
    /// leaving through its exit where it does not hold.
    fn guard(&mut self, guard: Guard) -> Result<(), Abort> {
        match guard.check {
            Check::Type { value, tag: t } => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return match (Kind::of(held.types), Kind::of_tag(t)) {
                        (kind, wanted) if kind == wanted => Ok(()),
                        (Some(Kind::Bool), Some(wanted @ (Kind::False | Kind::True))) => {
                            let Scalar::Bool(truth) = self.register(value)? else {
                                return Err(mismatch("a boolean register read as another kind"));
                            };
                            let other = self.b.ins().icmp_imm_s(
                                IntCC::NotEqual,
                                truth,
                                i64::from(wanted == Kind::True),
                            );
                            self.leave_if(other, guard.exit)
                        }
                        _ => Err(mismatch("a register guarded as another type")),
                    };
                }
                let (room, at) = self.place(held.slot)?;
                let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, tag(t));
                self.leave_if(other, guard.exit)?;
            }
            Check::Deref { value, .. } | Check::Reference { value } => {
                let held = self.value(value)?;
                let (room, at) = self.place(held.slot)?;
                let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                let reference = tag(ZvalType::Reference);
                let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, reference);
                self.leave_if(other, guard.exit)?;
                if let Check::Deref { tag: t, .. } = guard.check {
                    let reference = self.b.ins().load(I64, trusted(), room, at);
                    self.guards_read.reference = Some((held.slot, reference));
                    let inner = imm(Ref::VALUE_AT) + TYPE_AT;
                    let inner = self.b.ins().load(I8, trusted(), reference, inner);
                    let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, inner, tag(t));
                    self.leave_if(other, guard.exit)?;
                }
            }
            Check::Indirect {
                value,
                tag: t,
                pointee,
            } => {
                let held = self.value(value)?;
                let (room, at) = self.place(held.slot)?;
                if held.types != Types::INDIRECT {
                    let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                    let indirect = tag(ZvalType::Indirect);
                    let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, indirect);
                    self.leave_if(other, guard.exit)?;
                }
                let mut target = self.b.ins().load(I64, trusted(), room, at);
                let mut value_at = 0;
                if pointee == Pointee::Reference {
                    let byte = self.b.ins().load(I8, trusted(), target, TYPE_AT);
                    let reference = tag(ZvalType::Reference);
                    let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, reference);
                    self.leave_if(other, guard.exit)?;
                    target = self.b.ins().load(I64, trusted(), target, 0);
                    value_at = imm(Ref::VALUE_AT);
                }
                let byte = self.b.ins().load(I8, trusted(), target, value_at + TYPE_AT);
                let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, tag(t));
                self.leave_if(other, guard.exit)?;
            }
            Check::Layout {
                value,
                layout,
                read,
            } => self.layout_guard(value, layout, read, guard.exit)?,
            // A side trace's IP guard, where its parent's exit left the
            // frame where it stood: `trace` laid down where the frame
            // stands as the last kernel's.
            Check::Stands { func, opline } if guard.at == GuardAt::Entry => {
                self.stands(func, opline, guard.exit)?;
            }
            // php's stack check (`zend_jit_stack_check`), on the room the
            // last kernel, or `enter`, stood.
            Check::Room { bytes } => {
                let short = self.short_of(bytes);
                self.leave_if(short, guard.exit)?;
            }
            Check::Class { object, class } => self.class_guard(object, class, guard.exit)?,
            Check::Direction { .. }
            | Check::Stands { .. }
            | Check::Callee { .. }
            | Check::Arm { .. } => {
                return Err(mismatch("an op's own guard held before it"));
            }
        }
        Ok(())
    }

    /// The jump `jumps`, an `i8` of zero or one, held to the way each of
    /// the op's direction guards says it went.
    fn direction(&mut self, at: OpId, jumps: Value) -> Result<(), Abort> {
        for guard in self.within(at) {
            let Check::Direction { taken } = guard.check else {
                return Err(mismatch("a jump holds a guard of another kind"));
            };
            let other = self
                .b
                .ins()
                .icmp_imm_s(IntCC::Equal, jumps, i64::from(!taken));
            self.leave_where(other, guard.exit)?;
        }
        Ok(())
    }

    /// What an op computed, held to the type guards it holds on the values
    /// in `defs`: a number that may be either kind leaves where it is not
    /// the one held, and is the long where every value it writes is typed
    /// long alone, which php's ranges proved cannot overflow, as php's
    /// `zend_jit_math_long_long` checks no overflow where `may_overflow`
    /// is false (`Z/ext/opcache/jit/zend_jit_ir.c:5059-5200`). `read` is
    /// the long an increment or a decrement read, which a `POST_*` that
    /// leaves past itself writes to its `out`.
    fn settle(
        &mut self,
        at: OpId,
        defs: &[ValueId],
        num: Num,
        read: Option<Value>,
    ) -> Result<Num, Abort> {
        let num = self.held(at, defs, num, read)?;
        if let Num::Either { long, .. } = num
            && !defs.is_empty()
        {
            let mut long_only = true;
            for &value in defs {
                long_only &= self.value(value)?.types.kinds() == Types::LONG;
            }
            if long_only {
                return Ok(Num::Long(long));
            }
        }
        Ok(num)
    }

    /// What an op computed, held to the type guards it holds on the values
    /// in `defs`, `read` the long an increment or a decrement read.
    fn held(
        &mut self,
        at: OpId,
        defs: &[ValueId],
        num: Num,
        read: Option<Value>,
    ) -> Result<Num, Abort> {
        let mut num = num;
        for guard in self.within(at) {
            let Check::Type { value, tag: t } = guard.check else {
                return Err(mismatch("an arithmetic holds a guard of another kind"));
            };
            if !defs.contains(&value) {
                return Err(mismatch("an arithmetic guards a value it does not write"));
            }
            num = match (num, Kind::of_tag(t)) {
                (
                    Num::Either {
                        overflowed, long, ..
                    },
                    Some(Kind::Long),
                ) => {
                    if self.leaves_past(guard.exit) {
                        let frame = self.value(value)?.slot.frame;
                        let wrote = Wrote { long, read };
                        self.leave_past(overflowed, guard.exit, frame, wrote)?;
                    } else {
                        self.leave_if(overflowed, guard.exit)?;
                    }
                    Num::Long(long)
                }
                (
                    Num::Either {
                        overflowed,
                        long,
                        double,
                    },
                    Some(Kind::Double),
                ) => {
                    let exact = self.b.ins().icmp_imm_s(IntCC::Equal, overflowed, 0);
                    if self.leaves_past(guard.exit) {
                        let frame = self.value(value)?.slot.frame;
                        let wrote = Wrote { long, read };
                        self.leave_past(exact, guard.exit, frame, wrote)?;
                    } else {
                        self.leave_if(exact, guard.exit)?;
                    }
                    Num::Double(double)
                }
                (Num::Long(_), Some(Kind::Long)) | (Num::Double(_), Some(Kind::Double)) => num,
                // A pair of kinds [`Emit::dispatched`] made that makes the
                // other kind always leaves; nothing reads what it holds.
                (Num::Double(_), Some(Kind::Long)) => {
                    let always = self.b.ins().iconst(I8, 1);
                    self.leave_where(always, guard.exit)?;
                    Num::Long(self.b.ins().iconst(I64, 0))
                }
                (Num::Long(_), Some(Kind::Double)) => {
                    let always = self.b.ins().iconst(I8, 1);
                    self.leave_where(always, guard.exit)?;
                    Num::Double(self.b.ins().f64const(0.0))
                }
                _ => return Err(mismatch("an arithmetic guarded as a type it cannot make")),
            };
        }
        Ok(num)
    }

    /// The array `value` holds, read so, held to `layout` through `exit`.
    fn layout_guard(
        &mut self,
        value: ValueId,
        layout: Layout,
        read: Read,
        exit: ExitId,
    ) -> Result<(), Abort> {
        let slot = self.value(value)?.slot;
        let array = self.array_of(slot, read)?;
        self.guards_read.array = Some((slot, read, array));
        let flags = self.b.ins().load(I32, trusted(), array, imm(Arr::FLAGS_AT));
        let packed = self.b.ins().band_imm_u(flags, i64::from(Arr::PACKED));
        let cc = match layout {
            Layout::Packed => IntCC::Equal,
            Layout::Hash => IntCC::NotEqual,
        };
        let other = self.b.ins().icmp_imm_s(cc, packed, 0);
        self.leave_if(other, exit)
    }

    /// `cond`, a guard's failure, or under exit-fuzz that or the guard's
    /// first pass: the guard's byte read and set, so it fails once
    /// whatever it holds (`jit-design.md` §4.6, §7).
    fn fuzzed(&mut self, cond: Value) -> Result<Value, Abort> {
        let Some(fuzz) = self.fuzz else {
            return Ok(cond);
        };
        let at = self.fuzzed_sites;
        if at >= fuzz.len {
            return Err(mismatch("more guards fuzzed than the trace counted"));
        }
        self.fuzzed_sites += 1;
        let address = fuzz
            .base
            .checked_add(at)
            .and_then(|address| i64::try_from(address).ok())
            .ok_or_else(|| mismatch("a fuzzed byte past the host's addresses"))?;
        let byte = self.b.ins().iconst(I64, address);
        let failed = self.b.ins().load(I8, trusted(), byte, 0);
        let one = self.b.ins().iconst(I8, 1);
        self.b.ins().store(trusted(), one, byte, 0);
        let first = self.b.ins().icmp_imm_s(IntCC::Equal, failed, 0);
        Ok(self.b.ins().bor(cond, first))
    }

    /// Where an inline arithmetic goes that would raise, as `refuse`
    /// says: through its exit, or to the handler of the write it serves.
    fn refuse_if(&mut self, cond: Value, refuse: Refuse) -> Result<(), Abort> {
        match refuse {
            Refuse::Exit(exit) => self.leave_if(cond, exit),
            Refuse::To(handled) => {
                self.divert_if(cond, handled);
                Ok(())
            }
        }
    }

    /// Leave through `exit` where `cond` is not zero, a guard's failure,
    /// and under exit-fuzz on the guard's first pass too, its exit being
    /// right whatever the guard holds; go on otherwise.
    fn leave_if(&mut self, cond: Value, exit: ExitId) -> Result<(), Abort> {
        let cond = self.fuzzed(cond)?;
        self.leave_where(cond, exit)
    }

    /// Leave through `exit` where `cond` is not zero and only there, never
    /// failed by exit-fuzz: a jump's direction or a table's arm, whose exit lands where the
    /// jump the guard did not expect went, and a refused call, whose exit
    /// is right only where nothing was pushed.
    fn leave_where(&mut self, cond: Value, exit: ExitId) -> Result<(), Abort> {
        let (out, on) = (self.b.create_block(), self.b.create_block());
        self.b.set_cold_block(out);
        self.b.ins().brif(cond, out, &[], on, &[]);
        self.b.switch_to_block(out);
        self.b.seal_block(out);
        // What the way out takes is made on it, the guard's own way an
        // ordinary compare and branch: the number kept in registers written
        // where the exit's opline reads it, and what a door is handed.
        if let Some(kept) = self.kept {
            let (room, at) = self.place(kept.slot)?;
            self.write_num(room, at, kept.num);
        }
        let (to, args) = self.guard_exit(exit)?;
        self.b.ins().jump(to, &args);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The number kept for the op after the op that made it written to its slot,
    /// where an op other than its reader comes first.
    fn write_kept(&mut self) -> Result<(), Abort> {
        if let Some(kept) = self.kept.take() {
            let (room, at) = self.place(kept.slot)?;
            self.write_num(room, at, kept.num);
        }
        Ok(())
    }

    /// Whether `exit` lands past an arithmetic with what it overflowed
    /// to in its slots (`Stored::Overflowed`, `Stored::Limit`), or with
    /// the longs a step wrote that did not overflow (`Stored::Stayed`).
    fn leaves_past(&self, exit: ExitId) -> bool {
        (self.ir.exits.get(usize::from(exit.0)))
            .is_some_and(|spec| (spec.stores.iter()).any(|store| store.from.stack_type().is_some()))
    }

    /// Leave through `exit` where `cond` is not zero, the overflow of an
    /// `ADD` or a `SUB` of `1`, an increment or a decrement held long, or
    /// for a step held a double that it did not overflow, writing first
    /// each value the exit names the site's to its slot in `frame`, the
    /// one this site runs in, which is the one the exit lands in at
    /// whichever inlined level takes it, as php's frame-relative stack
    /// map holds it: the double it went to,
    /// `(double)ZEND_LONG_MAX + 1.0` where it went up and
    /// `(double)ZEND_LONG_MIN - 1.0`, which rounds to `ZEND_LONG_MIN`'s
    /// double, otherwise, and the long a `POST_*` read at the limit, in
    /// its `out`; or the long a step went to, and the long a `POST_*`
    /// read in its `out`, from `wrote`. Never failed by exit-fuzz: the
    /// exit lands past the op, right only where it went the way `cond`
    /// says.
    fn leave_past(
        &mut self,
        cond: Value,
        exit: ExitId,
        frame: Frame,
        wrote: Wrote,
    ) -> Result<(), Abort> {
        let (to, args) = self.guard_exit(exit)?;
        let stores = (self.ir.exits.get(usize::from(exit.0)))
            .map(|spec| spec.stores.clone())
            .unwrap_or_default();
        let (writes, on) = (self.b.create_block(), self.b.create_block());
        self.b.set_cold_block(writes);
        self.b.ins().brif(cond, writes, &[], on, &[]);
        self.b.switch_to_block(writes);
        self.b.seal_block(writes);
        if let Some(kept) = self.kept {
            let (room, at) = self.place(kept.slot)?;
            self.write_num(room, at, kept.num);
        }
        for store in stores {
            let held = match store.from {
                Stored::Overflowed { up } => {
                    let double = if up {
                        9_223_372_036_854_775_808.0
                    } else {
                        -9_223_372_036_854_775_808.0
                    };
                    Scalar::Double(self.b.ins().f64const(double))
                }
                Stored::Limit { up } => {
                    let long = if up { i64::MAX } else { i64::MIN };
                    Scalar::Long(self.b.ins().iconst(I64, long))
                }
                Stored::Stayed { read: false, .. } => Scalar::Long(wrote.long),
                Stored::Stayed { read: true, .. } => {
                    Scalar::Long((wrote.read).ok_or_else(|| mismatch("a step's out read no long"))?)
                }
                Stored::Value(_) | Stored::Copied { .. } | Stored::Null => continue,
            };
            self.store(
                Slot {
                    frame,
                    ..store.slot
                },
                held,
            )?;
        }
        self.b.ins().jump(to, &args);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The block of `exit`, made the first time a site branches to it.
    fn exit_block(&mut self, exit: ExitId) -> Result<Block, Abort> {
        let at = usize::from(exit.0);
        let slot = self
            .exits
            .get_mut(at)
            .ok_or_else(|| mismatch("an exit past the trace's"))?;
        if let Some(block) = *slot {
            return Ok(block);
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.exits[at] = Some(block);
        Ok(block)
    }

    /// The block of `unwind`, made the first time a kernel's status
    /// branches to it.
    fn unwind_block(&mut self, unwind: UnwindId) -> Result<Block, Abort> {
        let at = usize::from(unwind.0);
        let slot = self
            .unwinds
            .get_mut(at)
            .ok_or_else(|| mismatch("an unwind past the trace's"))?;
        if let Some(block) = *slot {
            return Ok(block);
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.unwinds[at] = Some(block);
        Ok(block)
    }

    /// Leave for php's interrupt stub where `cond` is not zero, or under
    /// exit-fuzz on the test's first pass: [`INTERRUPTED`], with nothing
    /// to store, as `types` takes the stub only where nothing stands in a
    /// register at the test.
    fn stub_if(&mut self, cond: Value) -> Result<(), Abort> {
        let cond = self.fuzzed(cond)?;
        let to = if let Some(block) = self.stub {
            block
        } else {
            let block = self.b.create_block();
            self.b.set_cold_block(block);
            self.stub = Some(block);
            block
        };
        let on = self.b.create_block();
        self.b.ins().brif(cond, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The request's interrupt tested, the one emitter of every test a
    /// compiled loop makes of it (`jit-design.md` §4.3), php's
    /// `vm_interrupt` test: at a loop's header (`zend_jit_trace_end_loop`),
    /// and before a side trace's link to its own root where the root's
    /// header tests it
    /// (`zend_jit_check_timeout` in `zend_jit_trace_link_to_root`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:17418-17432`). The byte the
    /// `ExitCtx`'s pointer names is loaded, and where it is not zero, or
    /// under exit-fuzz on the test's first pass, the code goes where
    /// `raised` says. The bench's untested build alone emits no test,
    /// the header's nor the link's, to measure what it costs
    /// (§10).
    fn interrupt_test(&mut self, raised: Raised) -> Result<(), Abort> {
        if cfg!(php_jit_untested_loops) {
            return Ok(());
        }
        let pointer = if let Some(pointer) = self.interrupt {
            pointer
        } else {
            let cx = self
                .cx
                .ok_or_else(|| mismatch("an interrupt test before the entry"))?;
            self.b.ins().load(I64, trusted(), cx, INTERRUPT_AT)
        };
        let byte = self.b.ins().load(I8, trusted(), pointer, 0);
        match raised {
            Raised::Exit(exit) => self.leave_if(byte, exit),
            Raised::Stub => self.stub_if(byte),
            Raised::Answer(block) => {
                let byte = self.fuzzed(byte)?;
                self.divert_if(byte, block);
                Ok(())
            }
        }
    }

    /// The block a kernel's refusal answers [`ABANDONED`] from.
    fn abandoned(&mut self) -> Block {
        if let Some(block) = self.abandon {
            return block;
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.abandon = Some(block);
        block
    }

    /// Every exit's block a site branched to: the scalars the trace keeps
    /// in registers stored to their slots with their type words, and the
    /// exit's number answered; every unwind's, its stores and [`UNWOUND`].
    fn exit_blocks(&mut self) -> Result<(), Abort> {
        self.b.set_srcloc(SourceLoc::default());
        let ir = self.ir;
        for at in 0..self.exits.len() {
            let Some(block) = self.exits[at] else {
                continue;
            };
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let spec = ir
                .exits
                .get(at)
                .ok_or_else(|| mismatch("an exit past the trace's"))?;
            self.stores(&spec.stores)?;
            let number = u32::try_from(at).map_err(|_| mismatch("too many exits"))?;
            let number = self.b.ins().iconst(I32, i64::from(number));
            self.answer_with(number)?;
        }
        self.door_blocks()?;
        for at in 0..self.answers.len() {
            let Some(block) = self.answers[at] else {
                continue;
            };
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let number = u32::try_from(at).map_err(|_| mismatch("too many exits"))?;
            let number = self.b.ins().iconst(I32, i64::from(number));
            self.answer_with(number)?;
        }
        for at in 0..self.unwinds.len() {
            let Some(block) = self.unwinds[at] else {
                continue;
            };
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let unwind = ir
                .unwinds
                .get(at)
                .ok_or_else(|| mismatch("an unwind past the trace's"))?;
            self.stores(&unwind.stores)?;
            let answer = self.b.ins().iconst(I32, i64::from(UNWOUND));
            self.answer_with(answer)?;
        }
        if let Some(block) = self.abandon {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let answer = self.b.ins().iconst(I32, i64::from(ABANDONED));
            self.answer_with(answer)?;
        }
        if let Some(block) = self.escape {
            self.escaped(block)?;
        }
        if let Some(block) = self.stub {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let answer = self.b.ins().iconst(I32, i64::from(INTERRUPTED));
            self.answer_with(answer)?;
        }
        if let Some(block) = self.declined {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let answer = self.b.ins().iconst(I32, i64::from(DECLINED));
            self.answer_with(answer)?;
        }
        Ok(())
    }

    /// What an exit or an unwind stores before it answers.
    fn stores(&mut self, stores: &[Store]) -> Result<(), Abort> {
        for store in stores {
            match store.from {
                Stored::Value(value) => {
                    if self.value(value)?.home == Home::Register {
                        let held = self.register(value)?;
                        self.store(store.slot, held)?;
                    }
                }
                // Its site wrote the slot before it branched here
                // ([`Emit::leave_past`] the constants and a step's longs).
                Stored::Copied { .. }
                | Stored::Null
                | Stored::Overflowed { .. }
                | Stored::Limit { .. }
                | Stored::Stayed { .. } => {}
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ kernels

    /// The one kernel-call emitter (`jit-design.md` §4.3): `kernel`'s
    /// wrapper called at its address with the `ExitCtx` and `args`, and its
    /// status word tested, leaving as `leave` says where it is not zero, or
    /// answering [`ABANDONED`] where the opline names no way to leave.
    fn call(&mut self, kernel: Kernel, args: &[Value], leave: Option<Leave>) -> Result<(), Abort> {
        let status = self.call_status(kernel, args, leave)?;
        self.left_unless(status, leave)
    }

    /// The variables of the doubles in registers that `leave` stores, or,
    /// an op that has no way to leave naming none, those the last call
    /// carried, a list the code is right with whichever it holds: what a
    /// double not on it costs is a spill, and one that is dead a store.
    fn doubles_held(&mut self, leave: Option<Leave>) -> Vec<Variable> {
        let stores = match leave {
            Some(Leave::Exit(exit)) => {
                (self.ir.exits.get(usize::from(exit.0))).map(|spec| spec.stores.as_slice())
            }
            Some(Leave::Unwind(unwind)) => {
                (self.ir.unwinds.get(usize::from(unwind.0))).map(|spec| spec.stores.as_slice())
            }
            None => return self.carried_last.clone(),
        };
        let mut carried = Vec::new();
        for store in stores.unwrap_or_default() {
            let Stored::Value(value) = store.from else {
                continue;
            };
            let at = value.0 as usize;
            if let (Some(held), Some(Some(var))) = (self.ir.values.get(at), self.regs.get(at))
                && held.home == Home::Register
                && Kind::of(held.types) == Some(Kind::Double)
                && !carried.contains(var)
            {
                carried.push(*var);
            }
        }
        self.carried_last.clone_from(&carried);
        carried
    }

    /// The trace's room for the doubles a call carries ([`Emit::call`]), a
    /// word for every one it keeps in a register.
    fn carrying(&mut self) -> StackSlot {
        if let Some(room) = self.carrying {
            return room;
        }
        let doubles = (self.ir.values.iter())
            .filter(|value| {
                value.home == Home::Register && Kind::of(value.types) == Some(Kind::Double)
            })
            .count();
        let size = u32::try_from(doubles.saturating_mul(8)).unwrap_or(u32::MAX);
        let room = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        self.carrying = Some(room);
        room
    }

    /// A kernel's `status` tested: on where it is `Continue`, leaving as
    /// `leave` says otherwise ([`Emit::left_by`]).
    fn left_unless(&mut self, status: Value, leave: Option<Leave>) -> Result<(), Abort> {
        let to = self.left_by(leave)?;
        let on = self.b.create_block();
        self.b.ins().brif(status, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// `kernel`'s wrapper called at its address with the `ExitCtx` and
    /// `args`, and its status word, an `i32`, for the caller to test on
    /// to `leave`.
    ///
    /// Cranelift keeps no vector register across a call, so a double the
    /// trace holds would live on the stack from its definition on, where
    /// php's allocator keeps one in `d8` to `d15`. The doubles `leave`
    /// stores are written out and read back around the call instead, on
    /// the way that makes it, and leave the code that makes none alone.
    fn call_status(
        &mut self,
        kernel: Kernel,
        args: &[Value],
        leave: Option<Leave>,
    ) -> Result<Value, Abort> {
        let carried = self.doubles_held(leave);
        let room = self.carrying();
        for (held, &var) in (0..).step_by(8).zip(&carried) {
            let double = self.b.use_var(var);
            self.b.ins().stack_store(I64, double, room, held);
        }
        let status = self.kernel_status(kernel, args)?;
        for (held, &var) in (0..).step_by(8).zip(&carried) {
            let double = self.b.ins().stack_load(I64, F64, room, held);
            self.b.def_var(var, double);
        }
        Ok(status)
    }

    /// [`Emit::call_status`]'s call.
    fn kernel_status(&mut self, kernel: Kernel, args: &[Value]) -> Result<Value, Abort> {
        let sig = self.signature(kernel);
        let address = i64::try_from(self.symbols.address(kernel))
            .map_err(|_| mismatch("a kernel address past the host's"))?;
        let callee = self.b.ins().iconst(I64, address);
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a kernel called before the entry"))?;
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(cx);
        all.extend_from_slice(args);
        let inst = self.b.ins().call_indirect(sig, callee, &all);
        self.b
            .inst_results(inst)
            .first()
            .copied()
            .ok_or_else(|| mismatch("a kernel that answers nothing"))
    }

    /// The block a kernel's status other than `Continue` goes to: `leave`'s
    /// exit or unwind, or [`ABANDONED`] where it names none.
    fn left_by(&mut self, leave: Option<Leave>) -> Result<Block, Abort> {
        Ok(match leave {
            Some(Leave::Exit(exit)) => self.exit_block(exit)?,
            Some(Leave::Unwind(unwind)) => self.unwind_block(unwind)?,
            None => self.abandoned(),
        })
    }

    /// `kernel`'s signature, imported once.
    fn signature(&mut self, kernel: Kernel) -> SigRef {
        if let Some(sig) = self.sigs[kernel.index()] {
            return sig;
        }
        let mut sig = Signature::new(self.call_conv);
        // The `ExitCtx`, the frame's room and the opline come first.
        let params: &[Type] = match kernel {
            Kernel::Handle => &[I64, I64, I32, I32],
            Kernel::Frameless => &[I64, I64, I32, I32, I64, I32, I64],
            Kernel::Step | Kernel::CallNative | Kernel::GetConstant => &[I64, I64, I32, I32, I64],
            Kernel::FreeThis
            | Kernel::FreeClosure
            | Kernel::PopCall
            | Kernel::Interrupt
            | Kernel::NanToBool => &[I64, I64, I32],
            Kernel::PushCall | Kernel::Call => &[I64, I64, I32, I32, I32, I64],
            Kernel::Release | Kernel::FreeCv | Kernel::Destruct => &[I64, I64, I32, I64, I32],
            Kernel::Identical => &[I64, I64, I32, I64, I32, I64, I32, I64],
            Kernel::RopeInit | Kernel::Leave => &[I64, I64, I32, I64, I32, I64],
            Kernel::RopeAdd | Kernel::AssignConcat => &[I64, I64, I32, I64, I64, I32],
            Kernel::RopeEnd | Kernel::StrOffset => &[I64, I64, I32, I64, I64, I32, I64],
            Kernel::Concat => &[I64, I64, I32, I64, I32, I64, I32, I32, I64],
            Kernel::Find | Kernel::Lookup => &[I64, I64, I32, I64, I64, I32, I32, I64],
            Kernel::MakeRef => &[I64, I64, I32, I64, I64],
        };
        sig.params
            .extend(params.iter().map(|ty| AbiParam::new(*ty)));
        sig.returns.push(AbiParam::new(I32));
        let sig = self.b.import_signature(sig);
        self.sigs[kernel.index()] = Some(sig);
        sig
    }

    /// The room and the opline index a kernel serving `line` is called
    /// with.
    fn served(&mut self, line: &Line) -> Result<[Value; 2], Abort> {
        let room = self.room(line.frame)?;
        let opline = self.b.ins().iconst(I32, i64::from(line.opline.0));
        Ok([room, opline])
    }

    /// The engine offset of the op array `frame` serves, which a step of
    /// one of its oplines serves it from.
    fn func_of(&mut self, frame: Frame) -> Result<Value, Abort> {
        let func = self
            .ir
            .frames
            .get(usize::from(frame.0))
            .map(|info| info.func)
            .ok_or_else(|| mismatch("a frame past the trace's"))?;
        Ok(self.b.ins().iconst(I32, i64::from(engine_offset(func))))
    }

    /// Where a kernel that answers a word or a zval writes it.
    fn answer(&mut self) -> StackSlot {
        *self.answer.get_or_insert_with(|| {
            self.b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                u32::try_from(SLOT).unwrap_or(u32::MAX),
                3,
            ))
        })
    }

    /// A word of the [`Stood`] the last kernel wrote, or code in place
    /// stood: its frame, its call or the stack's room as an `I64`, its op
    /// array or its opline as an `I32`, or both in one `I64` as the
    /// struct lays them out, the op array in the low half.
    fn stood_word(&mut self, ty: Type, at: i32) -> Value {
        let vars = self.stood_vars;
        match (at, ty == I32) {
            (STOOD_FUNC, true) => {
                let func = self.b.use_var(vars.func);
                self.b.ins().ireduce(I32, func)
            }
            (STOOD_OPLINE, true) => {
                let opline = self.b.use_var(vars.opline);
                self.b.ins().ireduce(I32, opline)
            }
            (STOOD_FUNC, false) => {
                let func = self.b.use_var(vars.func);
                let opline = self.b.use_var(vars.opline);
                let high = self.b.ins().ishl_imm_u(opline, 32);
                self.b.ins().bor(func, high)
            }
            (STOOD_FRAME, _) => self.b.use_var(vars.frame),
            (STOOD_CALL, _) => self.b.use_var(vars.call),
            _ => self.b.use_var(vars.room),
        }
    }

    /// Hold what the body reads of its entry as it runs ([`EntryHeld`]).
    fn hold_entry(&mut self, images: Value, word: Value, loops: bool) {
        let unready = self.b.ins().band_imm_u(word, ENTRY_UNREADY);
        let unready = self.b.ins().icmp_imm_s(IntCC::NotEqual, unready, 0);
        if loops {
            let stood = self.b.ins().stack_addr(I64, self.stood, 0);
            self.b.ins().stack_store(I64, stood, self.stood, HELD_STOOD);
            self.stood_held = true;
        }
        self.entry_held = Some(if loops && !self.held.is_empty() {
            self.b
                .ins()
                .stack_store(I64, images, self.stood, HELD_IMAGES);
            self.b
                .ins()
                .stack_store(I64, unready, self.stood, HELD_UNREADY);
            EntryHeld::Frame
        } else {
            EntryHeld::Registers { images, unready }
        });
    }

    /// Where the interpreter stands as code in place left it: the frame,
    /// the call under way in it, its op array and opline, and the room
    /// the stack's top page has left. The op array and the opline are held
    /// zero-extended, each an `I32` or an `I64` already wide, so the word
    /// a jump passes and an escape's door is held to is one `orr` of what
    /// a leave read wide.
    fn stand(&mut self, [frame, call, func, opline, room]: [Value; 5]) {
        let vars = self.stood_vars;
        self.b.def_var(vars.frame, frame);
        self.b.def_var(vars.call, call);
        self.stand_at(func, opline);
        self.b.def_var(vars.room, room);
    }

    /// The op array and the opline of [`Emit::stand`] alone, for a frame
    /// that goes on where it was.
    fn stand_at(&mut self, func: Value, opline: Value) {
        let vars = self.stood_vars;
        let [func, opline] = [func, opline].map(|half| {
            if self.b.func.dfg.value_type(half) == I32 {
                self.b.ins().uextend(I64, half)
            } else {
                half
            }
        });
        self.b.def_var(vars.func, func);
        self.b.def_var(vars.opline, opline);
    }

    /// `loads`, made for a [`Stood`] word alone, dropped where no path
    /// reads the word ([`unread::unread`]).
    fn stood_loads<const N: usize>(&mut self, loads: [Value; N]) {
        for load in loads {
            if let Some(inst) = self.b.func.dfg.value_def(load).inst() {
                self.stood_loads.push(inst);
            }
        }
    }

    /// The address of the [`Stood`] a kernel writes, for its call.
    fn stood_out(&mut self) -> Value {
        if self.stood_held {
            self.b.ins().stack_load(I64, I64, self.stood, HELD_STOOD)
        } else {
            self.b.ins().stack_addr(I64, self.stood, 0)
        }
    }

    /// Where the kernel just called stood the interpreter, read back from
    /// what it wrote, on the path where it answered `Continue`.
    fn stood_back(&mut self) {
        let frame = self.b.ins().stack_load(I64, I64, self.stood, STOOD_FRAME);
        let call = self.b.ins().stack_load(I64, I64, self.stood, STOOD_CALL);
        let func = self.b.ins().stack_load(I64, I32, self.stood, STOOD_FUNC);
        let opline = self.b.ins().stack_load(I64, I32, self.stood, STOOD_OPLINE);
        let room = self.b.ins().stack_load(I64, I64, self.stood, STOOD_ROOM);
        self.stand([frame, call, func, opline, room]);
    }

    /// `line` served whole by [`Kernel::Step`]: the operands it reads from
    /// registers stored first, so the interpreter reads them in their
    /// slots, and the way a jump it served went held to its guards.
    ///
    /// A return, any opline [`returns`] names, first reads the caller its
    /// frame's header names, the place of the frame its `Op::Back` lands
    /// in, as php's trace takes the frame it goes on in from where the
    /// handler of a `RETURN` or a `RETURN_BY_REF` left the interpreter
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:17157-17199`). A
    /// `GENERATOR_RETURN` lands in no `Op::Back`: its frame is a resumed
    /// generator's, the serving's entry, whose leaving the recorder never
    /// steps, as php's trace halts there (`:17219-17224`). A return trace
    /// whose `RETURN_BY_REF` the handler serves, its caller going on to
    /// call `h()` in place:
    ///
    /// ```php
    /// function h() { return 1; }
    /// function &p($n) { static $z = 0; if ($n == 0) { return $z; } $x = &p($n - 1); h(); return $x; }
    /// $r = &p(6); echo $r, "\n";
    /// for ($i = 0; $i < 5; $i++) { $r = &p(8); echo $r, "\n"; }
    /// echo "done\n";
    /// ```
    /// ```expect regime=jit-tracing process
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// done
    /// ```
    ///
    /// and the same trace compiled before it runs, under `jit.mode=force`:
    ///
    /// ```php
    /// function h() { return 1; }
    /// function &p($n) { static $z = 0; if ($n == 0) { return $z; } $x = &p($n - 1); h(); return $x; }
    /// $r = &p(6); echo $r, "\n";
    /// for ($i = 0; $i < 5; $i++) { $r = &p(8); echo $r, "\n"; }
    /// echo "done\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// 0
    /// done
    /// ```
    fn stepped(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        if returns(line.opcode) {
            self.returns_to(line)?;
        }
        if let Some(onward @ Onward::Went) = Onward::of(line.opcode)
            && onward.serves(&self.within(at))
        {
            return self.branched(at, line);
        }

        if matches!(
            line.opcode,
            Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div
        ) && line.op1_def.is_none()
            && self.within(at).is_empty()
            && let Some(out) = line.out
            && let (Some(a), Some(b)) = (self.numeric(line.op1)?, self.numeric(line.op2)?)
            && matches!((a, b), (Numeric::Slot(..), _) | (_, Numeric::Slot(..)))
        {
            return self.stepped_numbers(at, line, out, [a, b]);
        }
        self.step_kernel(at, line)?;
        self.after_step(at, line)
    }

    /// An operand of a stepped arithmetic as [`Emit::stepped_numbers`]
    /// reads it: a number it holds, or a slot whose type byte says; none
    /// where it is neither.
    fn numeric(&mut self, operand: Operand) -> Result<Option<Numeric>, Abort> {
        Ok(match operand {
            Operand::Constant(Constant::Long(_) | Constant::Double(_)) => {
                Some(Numeric::Held(self.scalar_unguarded(operand)?))
            }
            Operand::Value(value) => {
                let held = self.value(value)?;
                match (held.home, Kind::of(held.types)) {
                    (Home::Memory, _) if held.types.meets(Types::NUMBER) => {
                        Some(Numeric::Slot(held.slot, held.types))
                    }
                    (Home::Register, Some(Kind::Long | Kind::Double)) => {
                        Some(Numeric::Held(self.register(value)?))
                    }
                    (Home::Memory | Home::Register, _) => None,
                }
            }
            Operand::Constant(_) | Operand::Unused => None,
        })
    }

    /// `+`, `-`, `*` or `/` the trace steps, its operands' types unknown
    /// to it, served in place where each operand's type byte reads a long
    /// or a double and no division is by zero, as `zend_jit_math_helper`
    /// serves what its fast paths take and calls a function for the rest
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:5332-5520`): what it makes written
    /// to `out`'s slot with its type word, as the handler writes it. Every
    /// other operand, a division by zero among them, goes to the handler,
    /// which serves it whole; either way on with the slot written, so the
    /// trace leaves through no exit of its own here.
    ///
    /// A division whose dividend php's analysis cannot type, in place
    /// over numbers and through the handler over the rest:
    ///
    /// ```php
    /// $vs = [3, 2.5, "6", null, 7, true, 9.0];
    /// $t = [];
    /// for ($i = 0; $i < 70; $i++) {
    ///     $v = $vs[$i % 7];
    ///     try { $t[] = (2.0 * $i / ($i % 5)) + $v / 2; } catch (DivisionByZeroError $e) { $t[] = "z"; }
    /// }
    /// echo implode(",", array_slice($t, 0, 14)), " ", count($t), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// z,3.25,5,2,5.5,z,16.5,8.5,6.5833333333333,7.5,z,25.5,12.5,13.166666666667 70
    /// ```
    fn stepped_numbers(
        &mut self,
        at: OpId,
        line: &Line,
        out: ValueId,
        operands: [Numeric; 2],
    ) -> Result<(), Abort> {
        let handler = self.b.create_block();
        let done = self.b.create_block();
        // The kinds each operand may hold: a slot's, those of the two its
        // types name; a held number's, its own.
        let kinds = |operand: Numeric| -> Result<Vec<Kind>, Abort> {
            Ok(match operand {
                Numeric::Slot(_, types) => {
                    [(Types::LONG, Kind::Long), (Types::DOUBLE, Kind::Double)]
                        .into_iter()
                        .filter(|&(of, _)| types.meets(of))
                        .map(|(_, kind)| kind)
                        .collect()
                }
                Numeric::Held(Scalar::Long(_)) => vec![Kind::Long],
                Numeric::Held(Scalar::Double(_)) => vec![Kind::Double],
                Numeric::Held(_) => return Err(mismatch("a stepped arithmetic over no number")),
            })
        };
        let (ka, kb) = (kinds(operands[0])?, kinds(operands[1])?);
        // A slot's type byte: each kind it may hold to its block, anything
        // else to the handler.
        let branch = |emit: &mut Self, slot: Slot, to: &[(Kind, Block)]| {
            let (room, at) = emit.place(slot)?;
            let byte = emit.b.ins().load(I8, trusted(), room, at + TYPE_AT);
            for &(kind, block) in to {
                let t = if kind == Kind::Long {
                    ZvalType::Long
                } else {
                    ZvalType::Double
                };
                let is = emit.b.ins().icmp_imm_s(IntCC::Equal, byte, tag(t));
                emit.divert_if(is, block);
            }
            emit.b.ins().jump(handler, &[]);
            Ok::<(), Abort>(())
        };
        // One block per pair of kinds, reached by testing each operand
        // held in a slot in turn.
        let mut pairs: Vec<(Kind, Kind, Block)> = Vec::new();
        let firsts: Vec<(Kind, Block)> = ka.iter().map(|&k| (k, self.b.create_block())).collect();
        match operands[0] {
            Numeric::Slot(slot, _) => branch(self, slot, &firsts)?,
            Numeric::Held(_) => {
                let [(_, first)] = firsts.as_slice() else {
                    return Err(mismatch("a held operand of more than one kind"));
                };
                self.b.ins().jump(*first, &[]);
            }
        }
        for &(k1, first) in &firsts {
            self.b.switch_to_block(first);
            self.b.seal_block(first);
            let seconds: Vec<(Kind, Block)> =
                kb.iter().map(|&k| (k, self.b.create_block())).collect();
            match operands[1] {
                Numeric::Slot(slot, _) => branch(self, slot, &seconds)?,
                Numeric::Held(_) => {
                    let [(_, second)] = seconds.as_slice() else {
                        return Err(mismatch("a held operand of more than one kind"));
                    };
                    self.b.ins().jump(*second, &[]);
                }
            }
            pairs.extend(seconds.into_iter().map(|(k2, block)| (k1, k2, block)));
        }
        let out_slot = self.value(out)?.slot;
        for (ka, kb, block) in pairs {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let read = |emit: &mut Self, operand: Numeric, kind: Kind| match operand {
                Numeric::Slot(slot, _) => emit.load(slot, kind, false),
                Numeric::Held(held) => Ok(held),
            };
            let a = read(self, operands[0], ka)?;
            let b = read(self, operands[1], kb)?;
            let num = self.binary(line.opcode, a, b, Some(Refuse::To(handler)))?;
            let (room, at) = self.place(out_slot)?;
            self.write_num(room, at, num);
            self.b.ins().jump(done, &[]);
        }
        self.b.switch_to_block(handler);
        self.b.seal_block(handler);
        self.b.set_cold_block(handler);
        self.step_kernel(at, line)?;
        self.after_step(at, line)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }

    /// The kernel that serves `line` whole, [`Kernel::Step`], or
    /// [`Kernel::Call`] for a call the trace follows into its callee, its
    /// operands held in registers stored to their slots first; past a
    /// step of an opline that leaves a frame the trace did not push, the
    /// trace halted where the step ended the serving ([`Emit::halts`]).
    fn step_kernel(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        self.spill_operands(line)?;
        let [room, opline] = self.served(line)?;
        let func = self.func_of(line.frame)?;
        match self.entered_by(at, line) {
            Some(callee) => {
                let callee = self.func_of(callee)?;
                let stood = self.stood_out();
                self.call(
                    Kernel::Call,
                    &[room, opline, func, callee, stood],
                    leave_of(line),
                )?;
                self.stood_back();
            }
            None if let Some(onward) = Onward::of(line.opcode)
                && onward.serves(&self.within(at)) =>
            {
                self.handled(line, onward, [room, opline, func])?;
            }
            None => {
                let stood = self.stood_out();
                self.call(Kernel::Step, &[room, opline, func, stood], leave_of(line))?;
                self.stood_back();
                if leaves(line.opcode) && self.may_be_top(line.frame)? {
                    self.halts()?;
                }
            }
        }
        Ok(())
    }

    /// Whether `frame` may be the entry of the serving the trace runs in,
    /// php's `may_be_top_frame`, `!TRACE_FRAME_IS_NESTED`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11056-11059`): the frame the trace
    /// entered in, or one a return past it landed in, and never one the
    /// trace pushed.
    fn may_be_top(&self, frame: Frame) -> Result<bool, Abort> {
        let info = (self.ir.frames.get(usize::from(frame.0)))
            .ok_or_else(|| mismatch("a step in a frame the trace does not hold"))?;
        Ok(!matches!(info.place, Place::Callee(_)))
    }

    /// `line`'s operands held in registers stored to their slots, for a
    /// kernel that serves it through its handler, which reads them there.
    pub(super) fn spill_operands(&mut self, line: &Line) -> Result<(), Abort> {
        for operand in [line.op1, line.op2, line.data] {
            if let Operand::Value(value) = operand
                && self.value(value)?.home == Home::Register
            {
                let held = self.register(value)?;
                self.store(self.value(value)?.slot, held)?;
            }
        }
        Ok(())
    }

    /// `line` served by its handler through [`Kernel::Handle`], with no
    /// step, and where the interpreter stands after it written: past it,
    /// in its frame; for a native's `DO_ICALL`, which pops its call, the
    /// call under way now read off the frame table, the record on top
    /// where it is not the frame's own ([`Emit::call_left`]), as php's
    /// `zend_jit_do_fcall` calls the native's handler directly and goes on
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:10511-10600`). `sqrt` in a loop, a
    /// native whose return is unused, one sent as another's argument, and
    /// one that throws out of the loop:
    ///
    /// ```php
    /// function f($n) {
    ///     $s = 0.0; $t = 0;
    ///     for ($i = 1; $i <= $n; $i++) { $s += sqrt($i); abs($i); $t += strlen(str_repeat("a", $i % 4)); }
    ///     return [round($s, 6), $t];
    /// }
    /// function g($n) { $t = 0; for ($i = 1; $i <= $n; $i++) { $t += strlen(str_repeat("b", 4 - $i)); } return $t; }
    /// echo json_encode(f(30)), "\n";
    /// try { g(10); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// [112.082845,45]
    /// str_repeat(): Argument #2 ($times) must be greater than or equal to 0
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($n) {
    ///     $s = 0.0; $t = 0;
    ///     for ($i = 1; $i <= $n; $i++) { $s += sqrt($i); abs($i); $t += strlen(str_repeat("a", $i % 4)); }
    ///     return [round($s, 6), $t];
    /// }
    /// function g($n) { $t = 0; for ($i = 1; $i <= $n; $i++) { $t += strlen(str_repeat("b", 4 - $i)); } return $t; }
    /// echo json_encode(f(30)), "\n";
    /// try { g(10); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [112.082845,45]
    /// str_repeat(): Argument #2 ($times) must be greater than or equal to 0
    /// ```
    ///
    /// A native that calls back into user code which writes what the
    /// trace holds, a reference its closure binds, read after the call:
    ///
    /// ```php
    /// function h($n) {
    ///     $r = 0; $t = 0;
    ///     $w = function ($x) use (&$r) { if ($x == 5) { $r = 0.5; } return $x; };
    ///     for ($i = 0; $i < $n; $i++) { $r = $i; array_map($w, [$i]); $t = $t + $r; }
    ///     return $t;
    /// }
    /// var_dump(h(8));
    /// ```
    /// ```expect regime=jit-force process
    /// float(23.5)
    /// ```
    ///
    /// A native's `DO_ICALL` is made in place ([`Emit::native_call`]), the
    /// native alone served through [`Kernel::CallNative`], and a frameless
    /// call's operands are laid for [`Kernel::Frameless`]
    /// ([`Emit::frameless_call`]); neither kernel reads an opline. One that
    /// refuses its argument throws out of the loop:
    ///
    /// ```php
    /// function f($n) { $t = 0; for ($i = 1; $i < $n; $i++) { $t += max(10, min(200, $i & 255)) + strlen(substr("abcdef", $i % 4)); } return $t; }
    /// function g($n, $a) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += str_contains("hay", $i < 7 ? "a" : $a) ? 1 : 0; } return $t; }
    /// echo f(300), "\n";
    /// try { g(10, [1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// 33490
    /// str_contains(): Argument #2 ($needle) must be of type string, array given
    /// ```
    ///
    /// ```php
    /// function f($n) { $t = 0; for ($i = 1; $i < $n; $i++) { $t += max(10, min(200, $i & 255)) + strlen(substr("abcdef", $i % 4)); } return $t; }
    /// function g($n, $a) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += str_contains("hay", $i < 7 ? "a" : $a) ? 1 : 0; } return $t; }
    /// echo f(300), "\n";
    /// try { g(10, [1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 33490
    /// str_contains(): Argument #2 ($needle) must be of type string, array given
    /// ```
    ///
    /// The frame stood at the call before the native runs, so what a
    /// native throws is raised at its line, what it returns given up where
    /// nothing reads it, and a frameless call's at its own:
    ///
    /// ```php
    /// function f($n) {
    ///     $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         str_repeat("ab", $i);
    ///         $t += intdiv(100, 7 - $i);
    ///     }
    ///     return $t;
    /// }
    /// function g($n, $a) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += strlen(implode(",", $i < 7 ? [$i] : $a)); } return $t; }
    /// try { f(10); } catch (DivisionByZeroError $e) { echo $e->getMessage(), " at ", $e->getLine(), "\n"; }
    /// try { g(10, 1); } catch (TypeError $e) { echo $e->getMessage(), " at ", $e->getLine(), "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// Division by zero at 6
    /// implode(): Argument #2 ($array) must be of type ?array, int given at 10
    /// ```
    ///
    /// ```php
    /// function f($n) {
    ///     $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         str_repeat("ab", $i);
    ///         $t += intdiv(100, 7 - $i);
    ///     }
    ///     return $t;
    /// }
    /// function g($n, $a) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += strlen(implode(",", $i < 7 ? [$i] : $a)); } return $t; }
    /// try { f(10); } catch (DivisionByZeroError $e) { echo $e->getMessage(), " at ", $e->getLine(), "\n"; }
    /// try { g(10, 1); } catch (TypeError $e) { echo $e->getMessage(), " at ", $e->getLine(), "\n"; }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// Division by zero at 6
    /// implode(): Argument #2 ($array) must be of type ?array, int given at 10
    /// ```
    fn handled(
        &mut self,
        line: &Line,
        onward: Onward,
        [room, opline, func]: [Value; 3],
    ) -> Result<(), Abort> {
        if line.opcode == Opcode::DoIcall {
            self.native_call(line, [room, opline])?;
        } else if line.opcode.is_frameless_icall() {
            self.frameless_call(line, [room, opline, func])?;
        } else {
            self.call(Kernel::Handle, &[room, opline, func], leave_of(line))?;
        }
        let next = match onward {
            Onward::Past(places) => self.b.ins().iconst(I32, i64::from(line.opline.0 + places)),
            Onward::Went => self.b.ins().load(I32, trusted(), room, EX_OPLINE),
        };
        self.stand_at(func, next);
        if line.opcode == Opcode::DoIcall {
            let call = self.call_left(line.frame)?;
            self.b.def_var(self.stood_vars.call, call);
        }
        Ok(())
    }

    /// `?:`'s `JMP_SET`, `??`'s `COALESCE` and `?->`'s `JMP_NULL`, which
    /// php's code compiles no inline form for
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:261-330`), served as php's
    /// code serves them: the handler called directly, `zend_jit_handler`, and
    /// the way it went, `EX(opline)` as the handler left it, held to the
    /// way the trace recorded by the opline's direction guards,
    /// `zend_jit_trace_handler`'s IP guard
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:17201-17271`), so the trace
    /// takes the exits php's takes and no other. Before the handler, a
    /// variable's value whose test reads its type byte and at most one
    /// word is tested in place ([`Emit::tested`]), as the handler tests
    /// it, and goes on with no call.
    ///
    /// `?:`, `??` and `?->` over a variable each kind of value passes
    /// through, in place and by the handler: an object, a reference, a temporary, a
    /// double and an undefined variable among them, whose warning the
    /// error handler turns into a throw caught outside the loop:
    ///
    /// ```php
    /// class P { public $v = 7; }
    /// function f($n) {
    ///     $vs = [null, false, true, 0, 5, 0.0, 2.5, "", "0", "00", "a", [], [1], new P, null];
    ///     $os = [null, new P, null]; $t = []; $r = 1; $ref = &$r;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $v = $vs[$i % 15]; $o = $os[$i % 3];
    ///         $a = $v ?: "e"; $b = $v ?? "n"; $c = $o?->v; $d = $ref ?: 0; $e = $ref ?? 0;
    ///         $g = ($i % 2 ? $v : null) ?? "t"; $h = ($i % 2 ? $v : 0) ?: "t";
    ///         $t[] = json_encode([$a, $b, $c, $d, $e, $g, $h]);
    ///     }
    ///     return implode(",", $t);
    /// }
    /// echo f(30), "\n";
    /// function g($n) { $s = 0; for ($i = 0; $i < $n; $i++) { if ($i == 5) { unset($u); } else { $u = $i; } $s += $u ?: 1; } return $s; }
    /// set_error_handler(function ($no, $msg) { throw new Exception($msg); });
    /// try { echo g(10), "\n"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// ["e","n",null,1,1,"t","t"],["e",false,7,1,1,false,"t"],[true,true,null,1,1,"t","t"],["e",0,null,1,1,0,"t"],[5,5,7,1,1,"t","t"],["e",0,null,1,1,0,"t"],[2.5,2.5,null,1,1,"t","t"],["e","",7,1,1,"","t"],["e","0",null,1,1,"t","t"],["00","00",null,1,1,"00","00"],["a","a",7,1,1,"t","t"],["e",[],null,1,1,[],"t"],[[1],[1],null,1,1,"t","t"],[{"v":7},{"v":7},7,1,1,{"v":7},{"v":7}],["e","n",null,1,1,"t","t"],["e","n",null,1,1,"t","t"],["e",false,7,1,1,"t","t"],[true,true,null,1,1,true,true],["e",0,null,1,1,"t","t"],[5,5,7,1,1,5,5],["e",0,null,1,1,"t","t"],[2.5,2.5,null,1,1,2.5,2.5],["e","",7,1,1,"t","t"],["e","0",null,1,1,"0","t"],["00","00",null,1,1,"t","t"],["a","a",7,1,1,"a","a"],["e",[],null,1,1,"t","t"],[[1],[1],null,1,1,[1],[1]],[{"v":7},{"v":7},7,1,1,"t","t"],["e","n",null,1,1,"t","t"]
    /// Undefined variable $u
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class P { public $v = 7; }
    /// function f($n) {
    ///     $vs = [null, false, true, 0, 5, 0.0, 2.5, "", "0", "00", "a", [], [1], new P, null];
    ///     $os = [null, new P, null]; $t = []; $r = 1; $ref = &$r;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $v = $vs[$i % 15]; $o = $os[$i % 3];
    ///         $a = $v ?: "e"; $b = $v ?? "n"; $c = $o?->v; $d = $ref ?: 0; $e = $ref ?? 0;
    ///         $g = ($i % 2 ? $v : null) ?? "t"; $h = ($i % 2 ? $v : 0) ?: "t";
    ///         $t[] = json_encode([$a, $b, $c, $d, $e, $g, $h]);
    ///     }
    ///     return implode(",", $t);
    /// }
    /// echo f(30), "\n";
    /// function g($n) { $s = 0; for ($i = 0; $i < $n; $i++) { if ($i == 5) { unset($u); } else { $u = $i; } $s += $u ?: 1; } return $s; }
    /// set_error_handler(function ($no, $msg) { throw new Exception($msg); });
    /// try { echo g(10), "\n"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// ["e","n",null,1,1,"t","t"],["e",false,7,1,1,false,"t"],[true,true,null,1,1,"t","t"],["e",0,null,1,1,0,"t"],[5,5,7,1,1,"t","t"],["e",0,null,1,1,0,"t"],[2.5,2.5,null,1,1,"t","t"],["e","",7,1,1,"","t"],["e","0",null,1,1,"t","t"],["00","00",null,1,1,"00","00"],["a","a",7,1,1,"t","t"],["e",[],null,1,1,[],"t"],[[1],[1],null,1,1,"t","t"],[{"v":7},{"v":7},7,1,1,{"v":7},{"v":7}],["e","n",null,1,1,"t","t"],["e","n",null,1,1,"t","t"],["e",false,7,1,1,"t","t"],[true,true,null,1,1,true,true],["e",0,null,1,1,"t","t"],[5,5,7,1,1,5,5],["e",0,null,1,1,"t","t"],[2.5,2.5,null,1,1,2.5,2.5],["e","",7,1,1,"t","t"],["e","0",null,1,1,"0","t"],["00","00",null,1,1,"t","t"],["a","a",7,1,1,"a","a"],["e",[],null,1,1,"t","t"],[[1],[1],null,1,1,[1],[1]],[{"v":7},{"v":7},7,1,1,"t","t"],["e","n",null,1,1,"t","t"]
    /// Undefined variable $u
    /// ```
    fn branched(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        self.spill_operands(line)?;
        let handler = self.b.create_block();
        let done = self.b.create_block();
        self.tested(at, line, handler, done)?;
        self.b.switch_to_block(handler);
        self.b.seal_block(handler);
        let [room, opline] = self.served(line)?;
        let func = self.func_of(line.frame)?;
        self.handled(line, Onward::Went, [room, opline, func])?;
        self.after_step(at, line)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }

    /// The test of `?:`'s `JMP_SET`, `??`'s `COALESCE` or `?->`'s
    /// `JMP_NULL` on a variable, made in place where it reads the value's
    /// type byte and its word, or the count or the length and first byte
    /// that word points at, as the handler makes it (`ZEND_JMP_SET`,
    /// `ZEND_COALESCE`, `ZEND_JMP_NULL`, `Z/Zend/zend_vm_def.h`):
    /// [`Test`]'s three. A `?:` or `??` that jumps copies the value to
    /// the opline's out slot with one owner more, as `ZVAL_COPY` copies a
    /// variable; either way the direction guards are held on the way it
    /// went, which is the way the recording names, as the jump's target is
    /// never the opline after it: each of the three jumps past an operand
    /// or a member the compiler lays down between. Then on to `done`.
    /// Every other value, a temporary's among them, goes to `handler`,
    /// which serves it whole.
    fn tested(&mut self, at: OpId, line: &Line, handler: Block, done: Block) -> Result<(), Abort> {
        let (Operand::Value(value), Some(test)) = (line.op1, Test::of(line.opcode)) else {
            self.b.ins().jump(handler, &[]);
            return Ok(());
        };
        let slot = self.value(value)?.slot;
        if self.is_temporary(slot)? {
            self.b.ins().jump(handler, &[]);
            return Ok(());
        }
        let (room, from_at) = self.place(slot)?;
        let info = self.b.ins().load(I32, trusted(), room, from_at + TYPE_AT);
        let kind = self.b.ins().band_imm_u(info, KIND);
        let goes_on = self.b.create_block();
        let taken = self.b.create_block();
        let is =
            |emit: &mut Emit<'_>, t: ZvalType| emit.b.ins().icmp_imm_u(IntCC::Equal, kind, tag(t));
        match test {
            Test::Truth => {
                let word = self.b.ins().load(I64, trusted(), room, from_at);
                let null_or_false = self.b.ins().iadd_imm_s(kind, -tag(ZvalType::Null));
                let null_or_false =
                    self.b
                        .ins()
                        .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, null_or_false, 1);
                self.divert_if(null_or_false, goes_on);
                let true_ = is(self, ZvalType::True);
                self.divert_if(true_, taken);
                let long = is(self, ZvalType::Long);
                self.when_else(long, |emit| {
                    emit.b.ins().brif(word, taken, &[], goes_on, &[]);
                });
                let array = is(self, ZvalType::Array);
                self.when_else(array, |emit| {
                    let count = emit
                        .b
                        .ins()
                        .load(I32, trusted(), word, imm(Arr::ELEMENTS_AT));
                    emit.b.ins().brif(count, taken, &[], goes_on, &[]);
                });
                let string = is(self, ZvalType::String);
                self.when_else(string, |emit| {
                    // `zend_is_true`: longer than one byte, or one byte
                    // that is not `"0"`.
                    let len = emit.b.ins().load(I64, trusted(), word, imm(Str::LEN_AT));
                    let byte = emit.b.ins().load(I8, trusted(), word, imm(Str::VAL_AT));
                    let long = emit.b.ins().icmp_imm_u(IntCC::UnsignedGreaterThan, len, 1);
                    let one = emit.b.ins().icmp_imm_u(IntCC::Equal, len, 1);
                    let zero = emit
                        .b
                        .ins()
                        .icmp_imm_u(IntCC::NotEqual, byte, i64::from(b'0'));
                    let one = emit.b.ins().band(one, zero);
                    let truth = emit.b.ins().bor(long, one);
                    emit.b.ins().brif(truth, taken, &[], goes_on, &[]);
                });
                self.b.ins().jump(handler, &[]);
            }
            Test::Defined => {
                let null = is(self, ZvalType::Null);
                self.divert_if(null, goes_on);
                let undef = is(self, ZvalType::Undef);
                let reference = is(self, ZvalType::Reference);
                let other = self.b.ins().bor(undef, reference);
                self.b.ins().brif(other, handler, &[], taken, &[]);
            }
            Test::NotNull => {
                let undef = is(self, ZvalType::Undef);
                let null = is(self, ZvalType::Null);
                let reference = is(self, ZvalType::Reference);
                let other = self.b.ins().bor(undef, null);
                let other = self.b.ins().bor(other, reference);
                self.b.ins().brif(other, handler, &[], goes_on, &[]);
            }
        }
        self.b.switch_to_block(goes_on);
        self.b.seal_block(goes_on);
        let no = self.b.ins().iconst(I8, 0);
        self.direction(at, no)?;
        self.b.ins().jump(done, &[]);
        if test == Test::NotNull {
            // `?->`'s jump writes the chain's value where its operand is
            // `null`, which the handler serves.
            return Ok(());
        }
        self.b.switch_to_block(taken);
        self.b.seal_block(taken);
        let out = line
            .out
            .ok_or_else(|| mismatch("a jump that copies its value to no slot"))?;
        let out_at = self.place(self.value(out)?.slot)?;
        let zval = self.b.ins().iadd_imm_s(room, i64::from(from_at));
        self.copy_at(zval, info, out_at)?;
        let yes = self.b.ins().iconst(I8, 1);
        self.direction(at, yes)?;
        self.b.ins().jump(done, &[]);
        Ok(())
    }

    /// Where `cond` is not zero, `body`, which ends its block itself; on
    /// in a fresh block otherwise.
    fn when_else(&mut self, cond: Value, body: impl FnOnce(&mut Emit<'a>)) {
        let then = self.b.create_block();
        let on = self.b.create_block();
        self.b.ins().brif(cond, then, &[], on, &[]);
        self.b.switch_to_block(then);
        self.b.seal_block(then);
        body(self);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
    }

    /// What follows a stepped `line`: a call it made off the calls under
    /// way, the way a jump it served went held to its guards, read off
    /// the `Stood` the kernel, or the code that served it in place, wrote,
    /// and what a property fetch wrote held to its type.
    fn after_step(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        if matches!(
            line.opcode,
            Opcode::DoFcall | Opcode::DoIcall | Opcode::DoUcall | Opcode::DoFcallByName
        ) {
            self.calls.pop();
        }
        for guard in self.within(at) {
            match guard.check {
                Check::Direction { .. } => {
                    let went = self.stood_word(I32, STOOD_OPLINE);
                    let other = if let Some(next) = self.successor(at, line.frame) {
                        self.b
                            .ins()
                            .icmp_imm_u(IntCC::NotEqual, went, i64::from(next))
                    } else {
                        let landing = self
                            .ir
                            .exits
                            .get(usize::from(guard.exit.0))
                            .and_then(|exit| exit.opline)
                            .ok_or_else(|| mismatch("a stepped jump with nowhere to hold"))?;
                        self.b
                            .ins()
                            .icmp_imm_u(IntCC::Equal, went, i64::from(landing.0))
                    };
                    self.leave_where(other, guard.exit)?;
                }
                Check::Stands { func, opline } => self.stands(func, opline, guard.exit)?,
                Check::Callee { func } => self.callee_guard(func, guard.exit)?,
                Check::Arm { opline } => {
                    let went = self.stood_word(I32, STOOD_OPLINE);
                    let there = self
                        .b
                        .ins()
                        .icmp_imm_u(IntCC::Equal, went, i64::from(opline.0));
                    self.leave_where(there, guard.exit)?;
                }
                // What a property fetch wrote, held where php's inline
                // fetch holds it (`types`' `hold_in_fetch`): read from
                // the slot the handler wrote, which it dereferenced.
                Check::Type { value, tag: t } | Check::Deref { value, tag: t }
                    if line.opcode == Opcode::FetchObjR && line.out == Some(value) =>
                {
                    let (room, offset) = self.place(self.value(value)?.slot)?;
                    let byte = self.b.ins().load(I8, trusted(), room, offset + TYPE_AT);
                    let other = match guard.check {
                        Check::Deref { .. } => {
                            let reference = tag(ZvalType::Reference);
                            self.b.ins().icmp_imm_s(IntCC::Equal, byte, reference)
                        }
                        _ => self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, tag(t)),
                    };
                    self.leave_if(other, guard.exit)?;
                }
                Check::Type { .. }
                | Check::Deref { .. }
                | Check::Reference { .. }
                | Check::Indirect { .. }
                | Check::Layout { .. }
                | Check::Room { .. }
                | Check::Class { .. } => {
                    return Err(mismatch("a stepped opline holds a guard of another kind"));
                }
            }
        }
        Ok(())
    }

    /// php's `jit_stub_trace_halt` past the step of an opline that leaves
    /// its frame ([`leaves`]), a frame that may be the serving's entry
    /// ([`Emit::may_be_top`]), where the step left that entry, ending the
    /// serving the trace runs in: the [`Kernel::Step`] stands nowhere
    /// ([`Stood::NOWHERE`]), and compiled code answers
    /// [`RETURNED`] with nothing to store, as php's trace guards the IP
    /// that `zend_jit_leave_func_helper` or the handler of a `RETURN`
    /// answers for a `ZEND_CALL_TOP` frame and halts
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11095-11112`, `:17218-17228`).
    /// The recorder stops before such an opline only in the serving it
    /// records in (`jit-design.md` §4.1's entry-frame rule), so a return
    /// trace recorded where the lowest frame it returns past has a user
    /// caller runs, in a serving a native began, past that serving's
    /// entry. Here `p`'s return trace is recorded under the loop, and
    /// `array_map` calls `p` again, its outermost frame the entry of the
    /// serving `array_map` began:
    ///
    /// ```php
    /// function h() { return 1; }
    /// function p($n) { if ($n == 0) return 0; $x = p($n - 1); h(); return $x; }
    /// for ($i = 0; $i < 3; $i++) p(6);
    /// echo implode(",", array_map('p', [6, 6])), "\n";
    /// echo "done\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 0,0
    /// done
    /// ```
    ///
    /// and the same, every guard failed, and compiled on the worker:
    ///
    /// ```php
    /// function h() { return 1; }
    /// function p($n) { if ($n == 0) return 0; $x = p($n - 1); h(); return $x; }
    /// for ($i = 0; $i < 3; $i++) p(6);
    /// echo implode(",", array_map('p', [6, 6])), "\n";
    /// echo "done\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 0,0
    /// done
    /// ```
    ///
    /// ```php
    /// function h() { return 1; }
    /// function p($n) { if ($n == 0) return 0; $x = p($n - 1); h(); return $x; }
    /// for ($i = 0; $i < 3; $i++) p(6);
    /// echo implode(",", array_map('p', [6, 6])), "\n";
    /// echo "done\n";
    /// ```
    /// ```expect regime=jit-tracing process
    /// 0,0
    /// done
    /// ```
    fn halts(&mut self) -> Result<(), Abort> {
        let frame = self.stood_word(I64, STOOD_FRAME);
        let ended = self.b.ins().icmp_imm_s(IntCC::Equal, frame, 0);
        let (halt, on) = (self.b.create_block(), self.b.create_block());
        self.b.set_cold_block(halt);
        self.b.ins().brif(ended, halt, &[], on, &[]);
        self.b.switch_to_block(halt);
        self.b.seal_block(halt);
        let answer = self.b.ins().iconst(I32, i64::from(RETURNED));
        self.answer_with(answer)?;
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The frame a `DO_*CALL` at `at` enters, where the trace follows the
    /// call into its callee: what [`Kernel::Call`] makes the call to.
    fn entered_by(&self, at: OpId, line: &Line) -> Option<Frame> {
        if !matches!(
            line.opcode,
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName
        ) {
            return None;
        }
        match self.ir.ops.get(at.index() + 1) {
            Some(Op::Enter { frame, .. }) => Some(*frame),
            _ => None,
        }
    }

    /// php's IP guard: leave through `exit` unless the last kernel left
    /// the interpreter at `opline` of `func`. The exit names no opline, so
    /// exit-fuzz fails it too.
    fn stands(
        &mut self,
        func: EngineFuncRef,
        opline: OplineIdx,
        exit: ExitId,
    ) -> Result<(), Abort> {
        let landed = self.stood_word(I32, STOOD_FUNC);
        let at = self.stood_word(I32, STOOD_OPLINE);
        let other_func =
            self.b
                .ins()
                .icmp_imm_u(IntCC::NotEqual, landed, i64::from(engine_offset(func)));
        let other_opline = self
            .b
            .ins()
            .icmp_imm_u(IntCC::NotEqual, at, i64::from(opline.0));
        let other = self.b.ins().bor(other_func, other_opline);
        self.leave_if(other, exit)
    }

    /// php's IP guard past a trace's lowest frame (`zend_jit_leave_func`):
    /// on unless the last kernel left the interpreter elsewhere than
    /// `opline` of `func`, where the trace escapes ([`ESCAPED`](super::ESCAPED)) with
    /// nothing to store, as php's `jit_stub_trace_escape` takes no exit.
    /// Never failed by exit-fuzz, as it names no exit.
    fn escapes(&mut self, func: EngineFuncRef, opline: OplineIdx) -> Result<(), Abort> {
        let landed = self.stood_word(I32, STOOD_FUNC);
        let at = self.stood_word(I32, STOOD_OPLINE);
        let other_func =
            self.b
                .ins()
                .icmp_imm_u(IntCC::NotEqual, landed, i64::from(engine_offset(func)));
        let other_opline = self
            .b
            .ins()
            .icmp_imm_u(IntCC::NotEqual, at, i64::from(opline.0));
        let other = self.b.ins().bor(other_func, other_opline);
        let to = self.escape_block();
        let place = self.frame_ref(self.running)?;
        let on = self.b.create_block();
        self.b
            .ins()
            .brif(other, to, &[BlockArg::Value(place)], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The trace's one escape block, made the first time a return
    /// branches to it with the place of the frame it returned into:
    /// [`ESCAPED`](super::ESCAPED) answered, nothing stored, past the jump to the trace
    /// compiled at the landing, whose word it reads ([`Emit::escaped`]).
    fn escape_block(&mut self) -> Block {
        if let Some(block) = self.escape {
            return block;
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.b.append_block_param(block, I64);
        self.escape = Some(block);
        block
    }

    /// The opline the trace serves next in `frame` after the op at `at`,
    /// where it serves one there: where a stepped jump the trace follows
    /// went.
    fn successor(&self, at: OpId, frame: Frame) -> Option<u32> {
        let next = self
            .ir
            .ops
            .iter()
            .skip(at.index() + 1)
            .find_map(|op| match op {
                Op::Line(line) => Some(line),
                _ => None,
            });
        let next = match (next, self.ir.end) {
            (Some(line), _) => Some(line),
            (None, End::Loop(header)) => match self.ir.ops.get(header.at.index()) {
                Some(Op::Line(line)) => Some(line),
                _ => None,
            },
            (None, End::Leave(_) | End::Return(_)) => None,
        };
        next.filter(|line| line.frame == frame)
            .map(|line| line.opline.0)
    }

    // ------------------------------------------------------------ inline forms

    /// Whether an inline `VERIFY_RETURN_TYPE` leaves its operand where it
    /// stands: read from memory and written back to the same slot, as a
    /// value of no scalar kind, an array, is.
    fn stays(&self, line: &Line) -> Result<bool, Abort> {
        let (Operand::Value(read), Some(written), None) = (line.op1, line.op1_def, line.out) else {
            return Ok(false);
        };
        let (read, written) = (self.value(read)?, self.value(written)?);
        Ok(read.home == Home::Memory
            && written.home == Home::Memory
            && read.slot == written.slot
            && Kind::of(read.types).is_none())
    }

    /// One inline opline (`jit-design.md` §4.3), the scalar core.
    fn inline(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        match line.opcode {
            Opcode::Jmp => Ok(()),
            Opcode::Jmpz | Opcode::Jmpnz | Opcode::JmpzEx | Opcode::JmpnzEx => self.jump(at, line),
            // A return value of the declared type the slot already holds:
            // nothing to check and nothing to move, as php's code emits none.
            Opcode::VerifyReturnType if self.stays(line)? => Ok(()),
            Opcode::QmAssign | Opcode::VerifyReturnType => {
                let held = self.scalar(at, line.op1)?;
                for value in [line.out, line.op1_def].into_iter().flatten() {
                    self.define(value, held)?;
                }
                Ok(())
            }
            Opcode::IsSmaller
            | Opcode::IsSmallerOrEqual
            | Opcode::IsEqual
            | Opcode::IsNotEqual
            | Opcode::IsIdentical
            | Opcode::IsNotIdentical => self.compare(at, line),
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Sl
            | Opcode::Sr
            | Opcode::BwAnd
            | Opcode::BwOr
            | Opcode::BwXor => self.arithmetic(at, line, line.opcode),
            Opcode::AssignOp if line.imm == Imm::Binary(Opcode::Concat) => self.assign_concat(line),
            Opcode::AssignOp => {
                let Imm::Binary(binary) = line.imm else {
                    return Err(mismatch("an ASSIGN_OP with no operator"));
                };
                self.arithmetic(at, line, binary)
            }
            Opcode::Assign => self.assign(at, line),
            Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec => {
                self.increment(at, line)
            }
            Opcode::FetchDimR if line.imm == Imm::StrOffset => self.str_offset(at, line),
            Opcode::FetchDimR => self.fetch(at, line),
            Opcode::IssetIsemptyDimObj => self.isset_dim(at, line),
            Opcode::FetchDimIs => self.fetch_is(at, line),
            Opcode::FetchObjR => self.fetch_obj(at, line),
            Opcode::FetchObjW => self.fetch_obj_w(line),
            Opcode::AssignObj => self.assign_obj(at, line),
            Opcode::AssignObjOp
            | Opcode::PreIncObj
            | Opcode::PreDecObj
            | Opcode::PostIncObj
            | Opcode::PostDecObj => self.compound_obj(at, line),
            Opcode::FeResetR => self.fe_reset(line),
            Opcode::FeFetchR => self.fe_fetch(at, line),
            Opcode::AssignDim => self.assign_dim(at, line),
            Opcode::FetchDimW | Opcode::FetchDimRw => self.fetch_dim_w(at, line),
            Opcode::AssignDimOp => self.assign_dim_op(at, line),
            Opcode::Strlen => self.strlen(line),
            Opcode::Count => self.count(at, line),
            Opcode::FetchConstant => self.fetch_constant(at, line),
            Opcode::FetchStaticPropR => self.fetch_static(line),
            Opcode::RopeInit | Opcode::RopeAdd | Opcode::RopeEnd => self.rope(line),
            Opcode::Concat | Opcode::FastConcat => self.concat(line),
            Opcode::Bool | Opcode::BoolNot => self.bool(at, line),
            Opcode::FeFree | Opcode::Free => self.free_temp(line, line.op1),
            Opcode::SendVal | Opcode::SendVar => self.send(at, line),
            Opcode::SendRef => self.send_ref(line),
            Opcode::CheckFuncArg => self.check_func_arg(),
            Opcode::TypeCheck => self.type_check(at, line),
            // A send to a callee that takes the argument by value.
            Opcode::SendValEx if matches!(line.imm, Imm::Arg(_)) => self.send(at, line),
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName => match line.imm {
                Imm::Make {
                    returns,
                    checks,
                    defaults,
                } => self.make_in_place(at, line, (returns, checks, defaults)),
                _ => self.stepped(at, line),
            },
            Opcode::Return if matches!(line.imm, Imm::Leave { .. }) => {
                self.leave_in_place(at, line)
            }
            Opcode::SendValEx | Opcode::Return => self.stepped(at, line),
            Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName => {
                match self.pushed_by(at) {
                    Some((plain, room)) => self.push_in_place(at, line, plain, room),
                    None => self.push_call(at, line),
                }
            }
            Opcode::InitMethodCall | Opcode::InitStaticMethodCall => match self.pushed_by(at) {
                Some((plain, room)) => self.push_in_place(at, line, plain, room),
                None => Err(mismatch("a method's call in place with no call held")),
            },
            Opcode::InitDynamicCall => {
                self.spill_operands(line)?;
                self.push_call(at, line)
            }
            _ => Err(mismatch("an inline opline the lowering does not serve")),
        }
    }

    /// The plain call the `INIT_*` at `at` pushes in place, and what holds
    /// its page.
    fn pushed_by(&self, at: OpId) -> Option<(&'a PlainCall, PushRoom)> {
        match self.ir.ops.get(at.index() + 1) {
            Some(Op::InitCall {
                plain: Some(plain),
                room,
                ..
            }) => Some((plain, *room)),
            _ => None,
        }
    }

    /// A conditional jump on a scalar: its direction held, and the
    /// boolean a `JMPZ_EX` or `JMPNZ_EX` writes, before the guard as
    /// php's `zend_jit_bool_jmpznz` writes it
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:7778-7783`), since the target
    /// the jump did not go to reads it too, and its exit stores it.
    ///
    /// `&&` and `||` as the first test of a value, of a double, of a long
    /// and of a temporary, whose slot the boolean takes over, each trace
    /// going the other way on the next pass:
    ///
    /// ```php
    /// function show($x) { return var_export($x, true); }
    /// function ad($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d && $i; $o[] = show($x); } return implode(" ", $o); }
    /// function al($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d && $i; $o[] = show($x); } return implode(" ", $o); }
    /// function at($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = ($d + 1) && $i; $o[] = show($x); } return implode(" ", $o); }
    /// function od($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d || $i % 3; $o[] = show($x); } return implode(" ", $o); }
    /// function ol($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d || $i % 3; $o[] = show($x); } return implode(" ", $o); }
    /// function ot($v, $n) { $o = []; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = ($d + 1) || $i % 3; $o[] = show($x); } return implode(" ", $o); }
    /// echo ad([2.5, 0.0], 6), "\n", al([2, 0], 6), "\n", at([2, -1], 6), "\n";
    /// echo od([0.0, 2.5], 6), "\n", ol([0, 2], 6), "\n", ot([-1, 2], 6), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// false false true false true false
    /// false false true false true false
    /// false false true false true false
    /// false true true true true true
    /// false true true true true true
    /// false true true true true true
    /// ```
    ///
    /// The same over enough passes that exit-fuzz has failed every guard
    /// before the jump and the jump's own fails, as it does not over six:
    ///
    /// ```php
    /// function ad($v, $n) { $o = ["true" => 0, "false" => 0]; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d && $i; $o[var_export($x, true)]++; } return json_encode($o); }
    /// function al($v, $n) { $o = ["true" => 0, "false" => 0]; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d && $i; $o[var_export($x, true)]++; } return json_encode($o); }
    /// function od($v, $n) { $o = ["true" => 0, "false" => 0]; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d || $i % 3; $o[var_export($x, true)]++; } return json_encode($o); }
    /// function ol($v, $n) { $o = ["true" => 0, "false" => 0]; for ($i = 0; $i < $n; $i++) { $d = $v[$i % 2]; $x = 7; $x = $d || $i % 3; $o[var_export($x, true)]++; } return json_encode($o); }
    /// echo ad([2.5, 0.0], 300), ad([0.0, 2.5], 300), al([2, 0], 300), al([0, 2], 300), "\n";
    /// echo od([0.0, 2.5], 300), od([2.5, 0.0], 300), ol([0, 2], 300), ol([2, 0], 300), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// {"true":149,"false":151}{"true":150,"false":150}{"true":149,"false":151}{"true":150,"false":150}
    /// {"true":250,"false":50}{"true":250,"false":50}{"true":250,"false":50}{"true":250,"false":50}
    /// ```
    fn jump(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let held = self.scalar(at, line.op1)?;
        let truth = self.warned_truth(line, held)?;
        if let Some(out) = line.out {
            self.define(out, Scalar::Bool(truth))?;
        }
        let jumps = match line.opcode {
            Opcode::Jmpz | Opcode::JmpzEx => self.b.ins().icmp_imm_s(IntCC::Equal, truth, 0),
            _ => truth,
        };
        self.direction(at, jumps)
    }

    /// A comparison of two scalars: the boolean it writes, or, fused to
    /// the jump after it, the way that jump goes held.
    fn compare(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        if matches!(line.opcode, Opcode::IsIdentical | Opcode::IsNotIdentical)
            && (self.is_string(at, line.op1) || self.is_string(at, line.op2))
        {
            return self.compare_strings(at, line);
        }
        let a = self.scalar(at, line.op1)?;
        let b = self.scalar(at, line.op2)?;
        let result = self.comparison(line.opcode, a, b)?;
        let jumps = match line.fused {
            Some(Fused::Jmpz) => self.b.ins().icmp_imm_s(IntCC::Equal, result, 0),
            Some(Fused::Jmpnz) => result,
            None => {
                if let Some(out) = line.out {
                    self.define(out, Scalar::Bool(result))?;
                }
                result
            }
        };
        self.direction(at, jumps)
    }

    /// The arithmetic `op` over the opline's two operands, written to what
    /// it writes: its `out`, or the variable an `ASSIGN_OP` assigns.
    fn arithmetic(&mut self, at: OpId, line: &Line, op: Opcode) -> Result<(), Abort> {
        let defs: Vec<ValueId> = [line.out, line.op1_def].into_iter().flatten().collect();
        let raises = if cannot_raise(op, line.op2) {
            None
        } else {
            Some(Refuse::Exit(line.exit.ok_or_else(|| {
                mismatch("an arithmetic that cannot leave")
            })?))
        };
        let num = match (self.mixed(at, line.op1)?, self.mixed(at, line.op2)?) {
            (None, None) => {
                let a = self.scalar(at, line.op1)?;
                let b = self.scalar(at, line.op2)?;
                self.binary(op, a, b, raises)?
            }
            (a, b) => self.dispatched(at, line, (op, &defs), [a, b], raises)?,
        };
        let num = self.settle(at, &defs, num, None)?;
        if let ([value], Num::Either { .. }) = (defs.as_slice(), num)
            && let Some(reader) = self.keeps(at, *value)?
        {
            let slot = self.value(*value)?.slot;
            self.kept = Some(Kept {
                value: *value,
                slot,
                reader,
                num,
            });
            return Ok(());
        }
        for value in defs {
            self.define_num(value, num)?;
        }
        Ok(())
    }

    /// The op that alone reads `value`, a number of either kind the inline
    /// arithmetic at `at` writes to a temporary, where it is the op after
    /// it and an inline arithmetic that tests its kind where it reads it
    /// ([`Emit::mixed`]): nothing else reads the slot until that op is
    /// done, but an exit, which [`Emit::leave_where`] writes it for.
    ///
    /// A product that overflows to a double on some passes, read by a
    /// division whose quotient leaves where it is not a long, and the
    /// interpreter reads the product again from its slot:
    ///
    /// ```php
    /// function f($n, $k) { $s = 0; for ($i = 0; $i < $n; $i++) { $s = $s + (($i - 5) * $k) / 2 + 1; } return $s; }
    /// function g($n, $k) { $s = 0.0; for ($i = 0; $i < $n; $i++) { if ($i != 5) { $s += 1.0 / (($i - 5) * $k); } } return $s; }
    /// echo f(12, 2), " ", f(12, 3), " ", f(12, intdiv(PHP_INT_MAX, 2)), " ", g(12, 3), " ", g(12, intdiv(PHP_INT_MAX, 2)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 18 21 1.3835058055282E+19 0.055555555555556 3.6140072416184E-20
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($n, $k) { $s = 0; for ($i = 0; $i < $n; $i++) { $s = $s + (($i - 5) * $k) / 2 + 1; } return $s; }
    /// function g($n, $k) { $s = 0.0; for ($i = 0; $i < $n; $i++) { if ($i != 5) { $s += 1.0 / (($i - 5) * $k); } } return $s; }
    /// echo f(12, 2), " ", f(12, 3), " ", f(12, intdiv(PHP_INT_MAX, 2)), " ", g(12, 3), " ", g(12, intdiv(PHP_INT_MAX, 2)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 18 21 1.3835058055282E+19 0.055555555555556 3.6140072416184E-20
    /// ```
    fn keeps(&self, at: OpId, value: ValueId) -> Result<Option<OpId>, Abort> {
        let held = self.value(value)?;
        if held.home != Home::Memory || !self.is_temporary(held.slot)? {
            return Ok(None);
        }
        let reader = OpId(at.0 + 1);
        let Some(Op::Line(line)) = self.ir.ops.get(reader.index()) else {
            return Ok(None);
        };
        let read = Operand::Value(value);
        let reads = |line: &Line| {
            [line.op1, line.op2, line.data]
                .iter()
                .filter(|&&operand| operand == read)
                .count()
        };
        if line.form != Form::Inline
            || !matches!(
                line.opcode,
                Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div
            )
            || reads(line) != 1
            || self.mixed(reader, read)?.is_none()
        {
            return Ok(None);
        }
        let others = self.ir.ops.iter().filter_map(|op| match op {
            Op::Line(line) => Some(reads(line)),
            _ => None,
        });
        if others.sum::<usize>() != 1 {
            return Ok(None);
        }
        let named =
            self.ir.values.iter().any(
                |v| matches!(v.def, Def::Phi { entry, back } if entry == value || back == value),
            ) || self.ir.exits.iter().any(|exit| {
                exit.stores.iter().any(|store| match store.from {
                    Stored::Value(v)
                    | Stored::Copied { value: v, .. }
                    | Stored::Stayed { value: v, .. } => v == value,
                    Stored::Null | Stored::Overflowed { .. } | Stored::Limit { .. } => false,
                })
            });
        Ok((!named).then_some(reader))
    }

    /// An operand of the inline op at `at` that holds a number of either
    /// kind, which no guard before the op holds to one: kept in registers
    /// by the op before ([`Kept`]), or in its slot, read by its type where
    /// it is read, as `zend_jit_math_helper` tests it.
    fn mixed(&self, at: OpId, operand: Operand) -> Result<Option<Mixed>, Abort> {
        let Operand::Value(value) = operand else {
            return Ok(None);
        };
        if let Some(kept) = self.kept
            && kept.value == value
            && let Num::Either {
                overflowed,
                long,
                double,
            } = kept.num
        {
            return Ok(Some(Mixed::Kept {
                overflowed,
                long,
                double,
            }));
        }
        let held = self.value(value)?;
        if held.home == Home::Register || held.types != Types::NUMBER {
            return Ok(None);
        }
        Ok(match self.kind_at(at, value)? {
            (None, false) => Some(Mixed::Slot(held.slot)),
            _ => None,
        })
    }

    /// `op` over operands of which those in `either` hold a number of
    /// either kind, as `zend_jit_math_helper` serves them
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:5332-5520`): each such operand's
    /// kind tested, its type byte or the overflow bit the op that kept it
    /// computed, the operation made for each pair of kinds and held there
    /// to the guards the op holds on what it writes ([`Emit::held`]), and
    /// what the pairs that go on made carried to one block.
    fn dispatched(
        &mut self,
        at: OpId,
        line: &Line,
        (op, defs): (Opcode, &[ValueId]),
        either: [Option<Mixed>; 2],
        raises: Option<Refuse>,
    ) -> Result<Num, Abort> {
        let fixed = [
            either[0].map_or_else(|| self.scalar(at, line.op1).map(Some), |_| Ok(None))?,
            either[1].map_or_else(|| self.scalar(at, line.op2).map(Some), |_| Ok(None))?,
        ];
        // The kinds each operand may be read as: both where it holds a
        // number of either kind, its own otherwise.
        let kinds = |mixed: Option<Mixed>, fixed: Option<Scalar>| -> &'static [Kind] {
            match (mixed, fixed) {
                (Some(_), _) => &[Kind::Long, Kind::Double],
                (None, Some(Scalar::Double(_))) => &[Kind::Double],
                (None, _) => &[Kind::Long],
            }
        };
        // One block per pair of kinds, reached by testing each operand of
        // either kind in turn.
        let mut pairs: Vec<(Kind, Kind, Block)> = Vec::new();
        for &ka in kinds(either[0], fixed[0]) {
            for &kb in kinds(either[1], fixed[1]) {
                pairs.push((ka, kb, self.b.create_block()));
            }
        }
        let branch = |emit: &mut Self, mixed: Mixed, longs: Block, doubles: Block| {
            match mixed {
                Mixed::Slot(slot) => {
                    let (room, at) = emit.place(slot)?;
                    let byte = emit.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                    let is_long = emit
                        .b
                        .ins()
                        .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Long));
                    emit.b.ins().brif(is_long, longs, &[], doubles, &[]);
                }
                Mixed::Kept { overflowed, .. } => {
                    emit.b.ins().brif(overflowed, doubles, &[], longs, &[]);
                }
            }
            Ok::<(), Abort>(())
        };
        match either {
            [Some(a), Some(b)] => {
                let (first_long, first_double) = (self.b.create_block(), self.b.create_block());
                branch(self, a, first_long, first_double)?;
                for (block, first) in [(first_long, 0), (first_double, 2)] {
                    self.b.switch_to_block(block);
                    self.b.seal_block(block);
                    branch(self, b, pairs[first].2, pairs[first + 1].2)?;
                }
            }
            [Some(mixed), None] | [None, Some(mixed)] => {
                branch(self, mixed, pairs[0].2, pairs[1].2)?;
            }
            [None, None] => return Err(mismatch("a dispatch over no number of either kind")),
        }
        let (join, made) = self.join_of(at, &pairs);
        for (ka, kb, block) in pairs {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let read = |emit: &mut Self,
                        mixed: Option<Mixed>,
                        kind: Kind,
                        fixed: Option<Scalar>| match (mixed, fixed, kind) {
                (Some(Mixed::Slot(slot)), _, _) => emit.load(slot, kind, false),
                (Some(Mixed::Kept { long, .. }), _, Kind::Long) => Ok(Scalar::Long(long)),
                (Some(Mixed::Kept { double, .. }), _, _) => Ok(Scalar::Double(double)),
                (None, Some(scalar), _) => Ok(scalar),
                (None, None, _) => Err(mismatch("an operand of no kind")),
            };
            let a = read(self, either[0], ka, fixed[0])?;
            let b = read(self, either[1], kb, fixed[1])?;
            let num = self.binary(op, a, b, raises)?;
            let num = self.held(at, defs, num, None)?;
            let args = self.carried(made, num)?;
            self.b.ins().jump(join, &args);
        }
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        Ok(made)
    }

    /// The block the pairs of kinds [`Emit::dispatched`] makes go on to,
    /// and what it carries: the one kind every pair makes and holds, where
    /// they make one, a pair of longs a number of either kind and any
    /// other a double, each held to the kind a guard on what the op at
    /// `at` writes holds it to.
    fn join_of(&mut self, at: OpId, pairs: &[(Kind, Kind, Block)]) -> (Block, Num) {
        let held_kind = self.within(at).iter().find_map(|guard| match guard.check {
            Check::Type { tag: t, .. } => Kind::of_tag(t),
            _ => None,
        });
        let makes = |ka: Kind, kb: Kind| match (held_kind, ka, kb) {
            (Some(kind), _, _) => Some(kind),
            (None, Kind::Long, Kind::Long) => None,
            (None, _, _) => Some(Kind::Double),
        };
        let join = self.b.create_block();
        let made = match pairs
            .iter()
            .map(|&(ka, kb, _)| makes(ka, kb))
            .reduce(|a, b| if a == b { a } else { None })
            .flatten()
        {
            Some(Kind::Long) => Num::Long(self.b.append_block_param(join, I64)),
            Some(Kind::Double) => Num::Double(self.b.append_block_param(join, F64)),
            _ => Num::Either {
                overflowed: self.b.append_block_param(join, I8),
                long: self.b.append_block_param(join, I64),
                double: self.b.append_block_param(join, F64),
            },
        };
        (join, made)
    }

    /// What a pair that made `num` hands the join that carries `made`
    /// ([`Emit::join_of`]).
    fn carried(&mut self, made: Num, num: Num) -> Result<Vec<BlockArg>, Abort> {
        Ok(match (made, num) {
            (Num::Long(_), Num::Long(x)) | (Num::Double(_), Num::Double(x)) => {
                vec![x.into()]
            }
            (Num::Either { .. }, Num::Long(l)) => {
                let d = self.b.ins().f64const(0.0);
                vec![self.b.ins().iconst(I8, 0).into(), l.into(), d.into()]
            }
            (Num::Either { .. }, Num::Double(d)) => {
                let l = self.b.ins().iconst(I64, 0);
                vec![self.b.ins().iconst(I8, 1).into(), l.into(), d.into()]
            }
            (
                Num::Either { .. },
                Num::Either {
                    overflowed,
                    long,
                    double,
                },
            ) => vec![overflowed.into(), long.into(), double.into()],
            _ => return Err(mismatch("a pair of kinds made another kind")),
        })
    }

    /// An increment or a decrement of a number in a variable: the variable
    /// written, and what the opline writes beside it, the new value or,
    /// for a `POST_*`, the old.
    fn increment(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let old = self.scalar(at, line.op1)?;
        let up = matches!(line.opcode, Opcode::PreInc | Opcode::PostInc);
        let new = match old {
            Scalar::Long(x) => {
                let (long, overflowed) = if up {
                    let one = self.b.ins().iconst(I64, 1);
                    self.b.ins().sadd_overflow(x, one)
                } else {
                    let one = self.b.ins().iconst(I64, 1);
                    self.b.ins().ssub_overflow(x, one)
                };
                let real = self.b.ins().fcvt_from_sint(F64, x);
                let one = self.b.ins().f64const(1.0);
                let double = if up {
                    self.b.ins().fadd(real, one)
                } else {
                    self.b.ins().fsub(real, one)
                };
                Num::Either {
                    overflowed,
                    long,
                    double,
                }
            }
            Scalar::Double(x) => {
                let one = self.b.ins().f64const(1.0);
                Num::Double(if up {
                    self.b.ins().fadd(x, one)
                } else {
                    self.b.ins().fsub(x, one)
                })
            }
            Scalar::Bool(_) | Scalar::Null => {
                return Err(mismatch("an inline increment of no number"));
            }
        };
        let pre = matches!(line.opcode, Opcode::PreInc | Opcode::PreDec);
        let mut defs: Vec<ValueId> = line.op1_def.into_iter().collect();
        if pre {
            defs.extend(line.out);
        }
        let read = match old {
            Scalar::Long(x) => Some(x),
            _ => None,
        };
        let new = self.settle(at, &defs, new, read)?;
        for value in defs {
            self.define_num(value, new)?;
        }
        if let (false, Some(out)) = (pre, line.out) {
            self.define(out, old)?;
        }
        Ok(())
    }

    /// An assignment to a variable, php's `zend_jit_assign` over
    /// `zend_jit_assign_to_variable` (`Z/ext/opcache/jit/zend_jit_ir.c:5793-5950`):
    /// a scalar written, or a value no scalar moved from a temporary,
    /// copied from a variable with one owner more, or copied as it is from
    /// a constant no reference counts ([`Emit::copy_operand`]); and where
    /// what the variable held was counted, released after, as
    /// `zend_assign_to_variable` frees it, what its destructor throws
    /// leaving through the opline's unwind. A register's slot is written
    /// too where the old value was counted, so no slot names a value
    /// freed. Strings, arrays and a constant assigned over strings and
    /// arrays, an object's destructor among the releases:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// function f($n) {
    ///     $s = ""; $a = [1]; $k = "k"; $o = new D;
    ///     for ($i = 0; $i < $n; $i++) { $t = str_repeat("x", $i % 3); $s = $t; $b = $a; $a = [$i]; $k = "k"; if ($i == 1) { $o = $s; } }
    ///     return [$s, $b, $a, $k, $o];
    /// }
    /// echo json_encode(f(5)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// d["x",[3],[4],"k","x"]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// function f($n) {
    ///     $s = ""; $a = [1]; $k = "k"; $o = new D;
    ///     for ($i = 0; $i < $n; $i++) { $t = str_repeat("x", $i % 3); $s = $t; $b = $a; $a = [$i]; $k = "k"; if ($i == 1) { $o = $s; } }
    ///     return [$s, $b, $a, $k, $o];
    /// }
    /// echo json_encode(f(5)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// d["x",[3],[4],"k","x"]
    /// ```
    fn assign(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(target) = line.op1 else {
            return Err(mismatch("an assignment to no variable"));
        };
        let slot = self.value(target)?.slot;
        let held = match self.scalar_kind(line.op2)? {
            Some(_) => Some(self.scalar(at, line.op2)?),
            None => None,
        };
        if line.unwind.is_none() {
            return self.assigned(line, slot, held);
        }
        let (room, place) = self.place(slot)?;
        let info = self.b.ins().load(I32, trusted(), room, place + TYPE_AT);
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        let (plain, owning, done) = (
            self.b.create_block(),
            self.b.create_block(),
            self.b.create_block(),
        );
        self.b.set_cold_block(owning);
        self.b.ins().brif(counted, owning, &[], plain, &[]);
        self.b.switch_to_block(plain);
        self.b.seal_block(plain);
        self.assigned(line, slot, held)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(owning);
        self.b.seal_block(owning);
        self.cold(|emit| {
            let word = emit.b.ins().load(I64, trusted(), room, place);
            emit.assigned(line, slot, held)?;
            // Nothing the last owner's destructor reads holds a value
            // given up: a register's value is written over it.
            if let (Some(held), Some(def)) = (held, line.op1_def)
                && emit.value(def)?.home == Home::Register
            {
                emit.store(slot, held)?;
            }
            emit.release(line, word, info, Dtor::Gc)?;
            emit.b.ins().jump(done, &[]);
            Ok(())
        })?;
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }

    /// `line`'s assignment made: the scalar `held` given to the values the
    /// op defines, or the value no scalar copied whole to `slot`.
    fn assigned(&mut self, line: &Line, slot: Slot, held: Option<Scalar>) -> Result<(), Abort> {
        let Some(held) = held else {
            let into = self.place(slot)?;
            return self.copy_operand(line.frame, line.op2, into);
        };
        if let Some(def) = line.op1_def {
            self.define(def, held)?;
        }
        if let Some(out) = line.out {
            self.define(out, held)?;
        }
        Ok(())
    }

    /// Whether `operand` is a scalar compiled code holds as one kind: a
    /// scalar constant, or a value of one kind of the scalar core; none
    /// for a constant no scalar is and a value of no one scalar kind.
    fn scalar_kind(&self, operand: Operand) -> Result<Option<Kind>, Abort> {
        Ok(match operand {
            Operand::Constant(Constant::Other(_)) | Operand::Unused => None,
            Operand::Constant(Constant::Null) => Some(Kind::Null),
            Operand::Constant(Constant::False) => Some(Kind::False),
            Operand::Constant(Constant::True) => Some(Kind::True),
            Operand::Constant(Constant::Long(_)) => Some(Kind::Long),
            Operand::Constant(Constant::Double(_)) => Some(Kind::Double),
            Operand::Value(value) => Kind::of(self.value(value)?.types),
        })
    }

    /// The value no scalar `operand` names, of `frame`, written whole to
    /// the zval at `base + at`: what a temporary holds moved with its
    /// owner, its slot left `UNDEF` as the handler takes the operand,
    /// php's `ZVAL_COPY_VALUE`; what a variable holds copied with one
    /// owner more, `ZVAL_COPY`; a constant no reference counts copied as
    /// it is from the op array's constants, `ZVAL_COPY_CONST`.
    fn copy_operand(
        &mut self,
        frame: Frame,
        operand: Operand,
        (base, at): (Value, i32),
    ) -> Result<(), Abort> {
        match operand {
            Operand::Value(value) => {
                let from = self.value(value)?.slot;
                let (room, from_at) = self.place(from)?;
                let info = self.b.ins().load(I32, trusted(), room, from_at + TYPE_AT);
                if self.is_temporary(from)? {
                    let word = self.b.ins().load(I64, trusted(), room, from_at);
                    self.b.ins().store(trusted(), word, base, at);
                    self.b.ins().store(trusted(), info, base, at + TYPE_AT);
                    let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
                    self.b
                        .ins()
                        .store(trusted(), undef, room, from_at + TYPE_AT);
                } else {
                    let zval = self.b.ins().iadd_imm_s(room, i64::from(from_at));
                    self.copy_at(zval, info, (base, at))?;
                }
            }
            Operand::Constant(Constant::Other(idx)) => {
                let zval = self.constant_zval(frame, idx)?;
                let word = self.b.ins().load(I64, trusted(), zval, 0);
                let info = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
                self.b.ins().store(trusted(), word, base, at);
                self.b.ins().store(trusted(), info, base, at + TYPE_AT);
            }
            _ => return Err(mismatch("a value copied whole from a scalar")),
        }
        Ok(())
    }

    /// The address of constant `idx` of the op array `frame` serves, its
    /// zval among the op array's constants, which the trace's epoch
    /// holds, php's `RT_CONSTANT`.
    fn constant_zval(&mut self, frame: Frame, idx: ConstIdx) -> Result<Value, Abort> {
        let literals = self
            .ir
            .frames
            .get(usize::from(frame.0))
            .ok_or_else(|| mismatch("a constant of a frame the trace does not have"))?
            .literals;
        let address = (idx.0 as usize)
            .checked_mul(size_of::<Zval>())
            .and_then(|offset| literals.checked_add(offset))
            .and_then(|address| i64::try_from(address).ok())
            .ok_or_else(|| mismatch("a constant past the host's addresses"))?;
        Ok(self.b.ins().iconst(I64, address))
    }

    /// A fetched zval held to its type by its guards, `in place` and,
    /// where it may be a reference, `through` it, as php's
    /// `zend_jit_guard_fetch_result_type` holds one
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:8024-8090`): one compare, the
    /// type the trace recorded, on the way the code goes on, and all else
    /// on a cold way. A zval of another type is `UNDEF`, which goes on as
    /// `element` says, php's check past its guard (`:12120`;
    /// `Z/ext/opcache/jit/zend_jit_trace.c:8724-8742` in its
    /// deoptimization); or a reference, read through by a guard of
    /// its own, php's second read; or the first guard's failure. The
    /// value then written to what the fetch writes.
    ///
    /// A trace of doubles read from a packed array, then given an element of
    /// another type, a hole, a reference, a hash and a list too short:
    ///
    /// ```php
    /// function sum($a, $n) { $s = 0.0; for ($i = 0; $i < $n; $i++) { $v = $a[$i % 3]; $s += $v; } return $s; }
    /// $h = [1.5, 2.5, 3.5]; unset($h[1]); $x = 6.5;
    /// echo sum([1.5, 2.5, 3.5], 9), "\n", sum($h, 3), "\n", sum([1.5, &$x, 3.5], 6), " ", sum(["x" => 0, 0 => 1.5, 1 => 2.5, 2 => 3.5], 6), "\n", sum([1.5], 3), "\n", sum([1.5, 2, 3.5], 6), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 22.5
    ///
    /// Warning: Undefined array key 1 in %s on line 2
    /// 5
    /// 23 15
    ///
    /// Warning: Undefined array key 1 in %s on line 2
    ///
    /// Warning: Undefined array key 2 in %s on line 2
    /// 1.5
    /// 14
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function sum($a, $n) { $s = 0.0; for ($i = 0; $i < $n; $i++) { $v = $a[$i % 3]; $s += $v; } return $s; }
    /// $h = [1.5, 2.5, 3.5]; unset($h[1]); $x = 6.5;
    /// echo sum([1.5, 2.5, 3.5], 9), "\n", sum($h, 3), "\n", sum([1.5, &$x, 3.5], 6), " ", sum(["x" => 0, 0 => 1.5, 1 => 2.5, 2 => 3.5], 6), "\n", sum([1.5], 3), "\n", sum([1.5, 2, 3.5], 6), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 22.5
    ///
    /// Warning: Undefined array key 1 in %s on line 2
    /// 5
    /// 23 15
    ///
    /// Warning: Undefined array key 1 in %s on line 2
    ///
    /// Warning: Undefined array key 2 in %s on line 2
    /// 1.5
    /// 14
    /// ```
    ///
    /// The element of another type leaves through the exit php's guard
    /// does, which grows the side trace php's grows:
    ///
    /// ```php
    /// function f($a, $n) { $s = 0.0; for ($i = 0; $i < $n; $i++) { $s += $a[$i % 3]; } return $s; }
    /// echo f([1.5, 2.5, 3.5], 100), "\n", f([1.5, 2, 3.5], 100), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 249
    /// 232.5
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 5 f() %s:2
    /// ---- TRACE 2 start (side trace 1/5) f() %s:2
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (enter) f() %s:2
    /// ---- TRACE 3 stop (link to 1)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 1 exit 7 f() %s:2
    /// ---- TRACE 4 start (side trace 1/7) f() %s:2
    /// ---- TRACE 4 stop (link to 1)
    /// ---- TRACE 4 compiled
    /// ```
    fn held_element(
        &mut self,
        in_place: Guard,
        through: Option<Guard>,
        element: Held<'_>,
        out: ValueId,
    ) -> Result<(), Abort> {
        let Check::Type { value, tag: t } = in_place.check else {
            return Err(mismatch("a fetch holds guards of another kind"));
        };
        let deref = match through.map(|through| through.check) {
            None => None,
            Some(Check::Deref { value: by, tag }) if by == out && tag == t => through,
            Some(_) => return Err(mismatch("a fetch holds guards of another kind")),
        };
        if value != out {
            return Err(mismatch("a fetch guards a value it does not write"));
        }
        let (zval, unset) = match element {
            Held::Set(zval) => (zval, None),
            Held::MaybeUndef(zval, missing) => (zval, Some(missing)),
        };
        if unset.is_some() && t == ZvalType::Undef {
            return Err(mismatch("a guard holds a fetch to UNDEF"));
        }
        let out_slot = self.value(out)?.slot;
        let byte = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, tag(t));
        let other = self.fuzzed(other)?;
        let (failed, read) = (self.b.create_block(), self.b.create_block());
        self.b.set_cold_block(failed);
        let (held, read_args) = if deref.is_some() {
            let held = self.b.append_block_param(read, I64);
            (held, vec![BlockArg::Value(zval)])
        } else {
            (zval, Vec::new())
        };
        self.b.ins().brif(other, failed, &[], read, &read_args);
        self.b.switch_to_block(failed);
        self.b.seal_block(failed);
        self.cold(|emit| {
            if let Some(missing) = unset {
                let undef = emit
                    .b
                    .ins()
                    .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Undef));
                emit.unset_if(undef, missing)?;
            }
            let Some(through) = deref else {
                return emit.element_left(in_place, zval, out, out_slot);
            };
            let is_ref = emit
                .b
                .ins()
                .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Reference));
            let (referred, direct) = (emit.b.create_block(), emit.b.create_block());
            emit.b.ins().brif(is_ref, referred, &[], direct, &[]);
            emit.b.switch_to_block(direct);
            emit.b.seal_block(direct);
            emit.element_left(in_place, zval, out, out_slot)?;
            emit.b.switch_to_block(referred);
            emit.b.seal_block(referred);
            let reference = emit.b.ins().load(I64, trusted(), zval, 0);
            let inner = emit
                .b
                .ins()
                .iadd_imm_s(reference, i64::from(imm(Ref::VALUE_AT)));
            emit.element_guard(through, inner, out, out_slot)?;
            emit.b.ins().jump(read, &[inner.into()]);
            Ok(())
        })?;
        self.b.switch_to_block(read);
        self.b.seal_block(read);
        if let Some(kind) = Kind::of_tag(t) {
            let read = self.read(held, 0, kind);
            self.define(out, read)
        } else {
            let info = self.b.ins().load(I32, trusted(), held, TYPE_AT);
            self.copy(held, info, out_slot)
        }
    }

    /// `body`'s code laid out of line, every block it makes, as php's
    /// `ir_IF_FALSE_cold` lays the way it does not expect: a block left
    /// among the hot ones costs the way the code goes on a jump over it.
    fn cold<T>(&mut self, body: impl FnOnce(&mut Self) -> Result<T, Abort>) -> Result<T, Abort> {
        let first = self.b.func.dfg.num_blocks();
        let made = body(self)?;
        for block in first..self.b.func.dfg.num_blocks() {
            self.b.set_cold_block(Block::new(block));
        }
        Ok(made)
    }

    /// The guard of the value read through a reference, on the cold way
    /// it is read on: on where the value at `element` is of the type it
    /// holds, and leaving as [`Emit::element_left`] says where it is not.
    fn element_guard(
        &mut self,
        guard: Guard,
        element: Value,
        out: ValueId,
        out_slot: Slot,
    ) -> Result<(), Abort> {
        let Check::Deref { tag: t, .. } = guard.check else {
            return Err(mismatch("a fetch holds a guard of another kind"));
        };
        let byte = self.b.ins().load(I8, trusted(), element, TYPE_AT);
        let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, byte, tag(t));
        let other = self.fuzzed(other)?;
        let (failed, on) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(other, failed, &[], on, &[]);
        self.b.switch_to_block(failed);
        self.b.seal_block(failed);
        self.element_left(guard, element, out, out_slot)?;
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// Leave through `guard`'s exit, a fetch's value not of the type the
    /// guard holds, copying the zval at `element` to the slot the fetch
    /// writes first where the exit goes on past the fetch.
    fn element_left(
        &mut self,
        guard: Guard,
        element: Value,
        out: ValueId,
        out_slot: Slot,
    ) -> Result<(), Abort> {
        let goes_on = self
            .ir
            .exits
            .get(usize::from(guard.exit.0))
            .is_some_and(|spec| {
                spec.stores
                    .iter()
                    .any(|store| matches!(store.from, Stored::Copied { value, .. } if value == out))
            });
        if goes_on {
            // The exit goes on past the fetch: the element is the slot's,
            // copied as the fetch copies it, through the reference it may
            // be.
            let element = self.deref(element);
            let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
            self.copy(element, info, out_slot)?;
        }
        let (to, args) = self.guard_exit(guard.exit)?;
        self.b.ins().jump(to, &args);
        Ok(())
    }

    /// The zval at `zval`, of type word `old`, written with `line`'s
    /// value as `zend_jit_assign_to_variable` writes it, then on at
    /// `done`: written over where it counts nothing, given up after where
    /// it counts, through the release kernel where it is the last owner;
    /// a reference to `handled`, whose handler writes through it.
    fn written_over(
        &mut self,
        line: &Line,
        (zval, old): (Value, Value),
        value: Option<Scalar>,
        (handled, done): (Block, Block),
    ) -> Result<(), Abort> {
        let (plain, owning) = (self.b.create_block(), self.b.create_block());
        let counted = self.b.ins().band_imm_u(old, COUNTED);
        self.b.ins().brif(counted, owning, &[], plain, &[]);
        self.b.switch_to_block(plain);
        self.b.seal_block(plain);
        self.written(line, zval, value)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(owning);
        self.b.seal_block(owning);
        let kind = self.b.ins().band_imm_u(old, KIND);
        let reference = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, kind, tag(ZvalType::Reference));
        self.divert_if(reference, handled);
        let word = self.b.ins().load(I64, trusted(), zval, 0);
        self.written(line, zval, value)?;
        self.release(line, word, old, Dtor::Gc)?;
        self.b.ins().jump(done, &[]);
        Ok(())
    }

    /// The `handled` block, `line` served by its handler, then on at
    /// `done`, where an inline write and its handler meet.
    fn handled_then(
        &mut self,
        at: OpId,
        line: &Line,
        (handled, done): (Block, Block),
    ) -> Result<(), Abort> {
        self.b.set_cold_block(handled);
        self.b.switch_to_block(handled);
        self.b.seal_block(handled);
        self.cold(|emit| {
            emit.stepped(at, line)?;
            emit.b.ins().jump(done, &[]);
            Ok(())
        })?;
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }

    /// The number at `zval`, of type byte `kind`, written over with `op`
    /// of it and `by` in place where php's `zend_jit_math_helper` and
    /// `zend_jit_long_math_helper` serve it inline, then on at `done`: a
    /// long or a double under `+`, `-`, `*` and `/`, a long overflowing
    /// to a double as php's does, and a long under `%`, the shifts and
    /// the bitwise operators; to `handled` where it is anything else, and
    /// where `raises` and `by` is a divisor of zero or a negative shift,
    /// whose throw is the handler's.
    ///
    /// A double written with a double takes no type word, and a long
    /// beside it, written by the same op on its cold way, takes one, an
    /// overflow's among them:
    ///
    /// ```php
    /// function f($a, $n, $by) { for ($i = 0; $i < $n; $i++) { $a[$i % 3] -= $by; } return json_encode($a); }
    /// echo f([1.5, 2.5, 3.5], 9, 0.5), f([1, 2.5, 3], 9, 0.5), f([1.5, 2, 3], 9, 1), f([PHP_INT_MIN, 2.5, 3], 3, 1), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [0,1,2][-0.5,1,1.5][-1.5,-1,0][-9.223372036854776e+18,1.5,2]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($a, $n, $by) { for ($i = 0; $i < $n; $i++) { $a[$i % 3] -= $by; } return json_encode($a); }
    /// echo f([1.5, 2.5, 3.5], 9, 0.5), f([1, 2.5, 3], 9, 0.5), f([1.5, 2, 3], 9, 1), f([PHP_INT_MIN, 2.5, 3], 3, 1), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [0,1,2][-0.5,1,1.5][-1.5,-1,0][-9.223372036854776e+18,1.5,2]
    /// ```
    fn compound_at(
        &mut self,
        (zval, kind): (Value, Value),
        (op, by, raises): (Opcode, Scalar, bool),
        (handled, done): (Block, Block),
    ) -> Result<(), Abort> {
        let numbers = matches!(op, Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div);
        // The kind `by` is first, the other on its cold way.
        let tried: &[(ZvalType, Kind)] = match (numbers, by) {
            (false, _) => &[(ZvalType::Long, Kind::Long)],
            (true, Scalar::Double(_)) => &[
                (ZvalType::Double, Kind::Double),
                (ZvalType::Long, Kind::Long),
            ],
            (true, _) => &[
                (ZvalType::Long, Kind::Long),
                (ZvalType::Double, Kind::Double),
            ],
        };
        let mut ways = Vec::with_capacity(tried.len());
        for (at, &(zval_type, of)) in tried.iter().enumerate() {
            let (way, other) = (self.b.create_block(), self.b.create_block());
            if at > 0 {
                self.b.set_cold_block(way);
            }
            self.b.set_cold_block(other);
            let is = self.b.ins().icmp_imm_s(IntCC::Equal, kind, tag(zval_type));
            self.b.ins().brif(is, way, &[], other, &[]);
            self.b.switch_to_block(other);
            self.b.seal_block(other);
            ways.push((way, of));
        }
        self.b.ins().jump(handled, &[]);
        let refuse = raises.then_some(Refuse::To(handled));
        for (way, of) in ways {
            self.b.switch_to_block(way);
            self.b.seal_block(way);
            let x = self.read(zval, 0, of);
            match self.binary(op, x, by, refuse)? {
                // A double stays one: its type word stands, as php's
                // `zend_jit_math_helper` leaves one its `res_use_info` holds.
                Num::Double(x) if of == Kind::Double => {
                    self.b.ins().store(trusted(), x, zval, 0);
                }
                num => self.write_num(zval, 0, num),
            }
            self.b.ins().jump(done, &[]);
        }
        Ok(())
    }

    /// The element at `element` written with `line`'s value: the scalar
    /// `value`, or the value no scalar its `OP_DATA` names, copied as an
    /// assignment copies it ([`Emit::copy_operand`]).
    fn written(&mut self, line: &Line, element: Value, value: Option<Scalar>) -> Result<(), Abort> {
        match value {
            Some(value) => {
                self.write(element, 0, value);
                Ok(())
            }
            None => self.copy_operand(line.frame, line.data, (element, 0)),
        }
    }

    /// Go to `to` where `cond` is not zero, a block whose branches the
    /// caller seals once every one is made; go on otherwise.
    fn divert_if(&mut self, cond: Value, to: Block) {
        let on = self.b.create_block();
        self.b.ins().brif(cond, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
    }

    /// The array `slot` holds, in place or through the reference it holds,
    /// as `read` says: php's `zend_jit_fetch_reference` past its guards (`Z/ext/opcache/jit/zend_jit_ir.c:16952-17010`).
    /// A by-reference array read by a long key, made a hash, then an array
    /// with a reference element, under the trace:
    ///
    /// ```php
    /// function sum(array &$a, $n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $a[$i % 3]; if ($i == 5) { $a = ["x" => 1, 0 => 10, 1 => 20, 2 => 30]; } if ($i == 8) { $a = [7, 8, &$GLOBALS['r']]; } } return $s; }
    /// $r = 100; $b = [1, 2, 3];
    /// echo sum($b, 12), " ", json_encode($b), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 187 [7,8,100]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function sum(array &$a, $n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $a[$i % 3]; if ($i == 5) { $a = ["x" => 1, 0 => 10, 1 => 20, 2 => 30]; } if ($i == 8) { $a = [7, 8, &$GLOBALS['r']]; } } return $s; }
    /// $r = 100; $b = [1, 2, 3];
    /// echo sum($b, 12), " ", json_encode($b), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 187 [7,8,100]
    /// ```
    fn array_of(&mut self, slot: Slot, read: Read) -> Result<Value, Abort> {
        if let Some((guarded, how, array)) = self.guards_read.array
            && guarded == slot
            && how == read
        {
            return Ok(array);
        }
        let (base, at) = match self.guards_read.reference {
            Some((guarded, reference)) if guarded == slot && read == Read::Through => {
                (reference, imm(Ref::VALUE_AT))
            }
            _ => self.zval_of(slot, read)?,
        };
        Ok(self.b.ins().load(I64, trusted(), base, at))
    }

    /// Where the zval `slot` holds, read so, is: the slot itself, the
    /// value of the reference it holds, the zval its indirect `VAR` points
    /// at, or the value of the reference that zval is.
    pub(super) fn zval_of(&mut self, slot: Slot, read: Read) -> Result<(Value, i32), Abort> {
        let (room, at) = self.place(slot)?;
        let mut to = || self.b.ins().load(I64, trusted(), room, at);
        Ok(match read {
            Read::InPlace => (room, at),
            Read::Through => (to(), imm(Ref::VALUE_AT)),
            Read::Indirect(Pointee::Value) => (to(), 0),
            Read::Indirect(Pointee::Reference) => {
                let pointee = to();
                let reference = self.b.ins().load(I64, trusted(), pointee, 0);
                (reference, imm(Ref::VALUE_AT))
            }
        })
    }

    /// How the op at `at` reads `value`: through the reference its slot
    /// holds, or the address its indirect `VAR` holds, where a guard
    /// before the op held one, in place otherwise.
    fn read_of(&self, at: OpId, value: ValueId) -> Read {
        (self.ir.guards.iter())
            .filter(|g| g.at == GuardAt::Before(at))
            .find_map(|g| match g.check {
                Check::Deref { value: v, .. } if v == value => Some(Read::Through),
                Check::Indirect {
                    value: v, pointee, ..
                } if v == value => Some(Read::Indirect(pointee)),
                _ => None,
            })
            .unwrap_or(Read::InPlace)
    }

    /// Whether `key` is past the packed array `arr`'s `nNumUsed`.
    fn past_used(&mut self, arr: Value, key: Value) -> Value {
        let used = self.b.ins().load(I32, trusted(), arr, imm(Arr::USED_AT));
        let used = self.b.ins().uextend(I64, used);
        self.b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, key, used)
    }

    /// The address of the slot `key` names in the packed array `arr`.
    fn slot_at(&mut self, arr: Value, key: Value) -> Value {
        let data = self.b.ins().load(I64, trusted(), arr, imm(Arr::DATA_AT));
        let offset = self.b.ins().imul_imm_s(key, SLOT);
        self.b.ins().iadd(data, offset)
    }

    /// The zval at `zval`, or, where it is a reference, the one the
    /// reference holds: php's `ZVAL_DEREF`.
    fn deref(&mut self, zval: Value) -> Value {
        let byte = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        let is_ref = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Reference));
        let through = self.b.create_block();
        let merged = self.b.create_block();
        let at = self.b.append_block_param(merged, I64);
        self.b
            .ins()
            .brif(is_ref, through, &[], merged, &[zval.into()]);
        self.b.switch_to_block(through);
        self.b.seal_block(through);
        let reference = self.b.ins().load(I64, trusted(), zval, 0);
        let inner = self
            .b
            .ins()
            .iadd_imm_s(reference, i64::from(imm(Ref::VALUE_AT)));
        self.b.ins().jump(merged, &[inner.into()]);
        self.b.switch_to_block(merged);
        self.b.seal_block(merged);
        at
    }

    /// The zval at `zval`, no reference, of type word `info`, copied to
    /// `slot` with one owner more where it is counted, php's `ZVAL_COPY`
    /// with `GC_ADDREF` inline (`jit_ZVAL_COPY` and `jit_GC_ADDREF`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:1610-1613`, `:1672-1719`); the count saturates
    /// beneath the heap's pin, as the heap's own adds do. A string, an
    /// array and the array a reference element holds, each read by a long
    /// key and kept past the writes after:
    ///
    /// ```php
    /// function pick($a, $n) { $s = []; for ($i = 0; $i < $n; $i++) { $s[] = $a[$i % 3]; } return $s; }
    /// $r = [7]; $b = [str_repeat("ab", 2), [1, 2], &$r];
    /// $out = pick($b, 6);
    /// $b[1][] = 3; $r[] = 8; unset($b);
    /// echo json_encode($out), " ", json_encode($r), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// ["abab",[1,2],[7],"abab",[1,2],[7]] [7,8]
    /// ```
    fn copy(&mut self, zval: Value, info: Value, slot: Slot) -> Result<(), Abort> {
        let dest = self.place(slot)?;
        self.copy_at(zval, info, dest)
    }

    /// The zval at `zval`, no reference, of type word `info`, copied to
    /// the zval at `base + at` with one owner more where it is counted, as
    /// [`Emit::copy`] copies it to a slot.
    fn copy_at(&mut self, zval: Value, info: Value, (base, at): (Value, i32)) -> Result<(), Abort> {
        let word = self.b.ins().load(I64, trusted(), zval, 0);
        self.b.ins().store(trusted(), word, base, at);
        self.b.ins().store(trusted(), info, base, at + TYPE_AT);
        self.addref(word, info)
    }

    /// One owner more of the value of type word `info` at `word` where it
    /// is counted, `Z_TRY_ADDREF`, the count kept below the pin it may
    /// carry.
    fn addref(&mut self, word: Value, info: Value) -> Result<(), Abort> {
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        self.when(counted, |emit| {
            let count_at = imm(Zval::COUNT_AT);
            let count = emit.b.ins().load(I32, trusted(), word, count_at);
            let bits = emit.b.ins().band_imm_u(count, i64::from(Zval::COUNT_BITS));
            let room = emit
                .b
                .ins()
                .icmp_imm_u(IntCC::NotEqual, bits, i64::from(Zval::COUNT_BITS));
            let room = emit.b.ins().uextend(I32, room);
            let count = emit.b.ins().iadd(count, room);
            emit.b.ins().store(trusted(), count, word, count_at);
            Ok(())
        })
    }

    /// A `foreach`'s reset over an array, php's `zend_jit_fe_reset`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:13756-13775`): the array into
    /// the iterator's slot, with one owner more where a variable holds it,
    /// moved whole from a temporary, whose slot is left `UNDEF` as the
    /// handler takes it, and the iterator's position, `Z_FE_POS`, zero,
    /// the slot's own word and not the array's. An empty array goes on as
    /// any other, as the handler's does, and the fetch after it finds the
    /// end.
    fn fe_reset(&mut self, line: &Line) -> Result<(), Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("a reset of no array"));
        };
        let out = line
            .out
            .ok_or_else(|| mismatch("a reset that writes no iterator"))?;
        let from = self.value(array)?.slot;
        let to = self.value(out)?.slot;
        let (room, at) = self.place(from)?;
        let zval = self.b.ins().iadd_imm_s(room, i64::from(at));
        let info = self.b.ins().load(I32, trusted(), room, at + TYPE_AT);
        if self.is_temporary(from)? {
            let word = self.b.ins().load(I64, trusted(), room, at);
            let (dest, dest_at) = self.place(to)?;
            self.b.ins().store(trusted(), word, dest, dest_at);
            self.b.ins().store(trusted(), info, dest, dest_at + TYPE_AT);
            let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
            self.b.ins().store(trusted(), undef, room, at + TYPE_AT);
        } else {
            self.copy(zval, info, to)?;
        }
        let (dest, dest_at) = self.place(to)?;
        let zero = self.b.ins().iconst(I32, 0);
        self.b
            .ins()
            .store(trusted(), zero, dest, dest_at + FE_POS_AT);
        Ok(())
    }

    /// A `foreach`'s fetch over a packed array into a variable, where the
    /// loop goes on, php's `zend_jit_fe_fetch`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:13800-14120`): from the
    /// iterator's position, the holes passed over, the end of the array
    /// leaving through the direction's exit for the opline past the loop,
    /// where the handler would jump; else the position moved past the
    /// element, its position the key where one is written, and the
    /// element, through the reference it may be, copied into the variable
    /// with one owner more, what the variable held given up after, as
    /// `zend_assign_to_variable` gives it up. A variable that holds a
    /// reference is written through by the handler, which serves the
    /// opline whole. Keys and values, a hole, a reference among the
    /// elements and a variable bound to one:
    ///
    /// ```php
    /// function sum($a) { $s = 0; $t = ""; foreach ($a as $k => $v) { $s += $k; $t .= is_array($v) ? "A" : $v; } return "$s:$t"; }
    /// $r = "R"; $a = ["a", "b", [1], &$r, str_repeat("c", 2)]; unset($a[1]);
    /// echo sum($a), " ", sum(["x", "y"]), " ", sum([]), "\n";
    /// function bound($a) { $v = 0; $w = &$v; foreach ($a as $v) { $w .= "!"; } return $v; }
    /// echo bound(["p", "q"]), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 9:aARcc 1:xy 0:
    /// q!
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function sum($a) { $s = 0; $t = ""; foreach ($a as $k => $v) { $s += $k; $t .= is_array($v) ? "A" : $v; } return "$s:$t"; }
    /// $r = "R"; $a = ["a", "b", [1], &$r, str_repeat("c", 2)]; unset($a[1]);
    /// echo sum($a), " ", sum(["x", "y"]), " ", sum([]), "\n";
    /// function bound($a) { $v = 0; $w = &$v; foreach ($a as $v) { $w .= "!"; } return $v; }
    /// echo bound(["p", "q"]), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 9:aARcc 1:xy 0:
    /// q!
    /// ```
    fn fe_fetch(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(iterator) = line.op1 else {
            return Err(mismatch("a fetch from no iterator"));
        };
        let Operand::Value(old) = line.op2 else {
            return Err(mismatch("a fetch into no variable"));
        };
        let target = self.value(old)?.slot;
        let (room, iter_at) = self.place(self.value(iterator)?.slot)?;
        let (var_room, var_at) = self.place(target)?;
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let old_word = self.b.ins().load(I64, trusted(), var_room, var_at);
        let old_info = self
            .b
            .ins()
            .load(I32, trusted(), var_room, var_at + TYPE_AT);
        let kind = self.b.ins().band_imm_u(old_info, KIND);
        let bound = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, kind, tag(ZvalType::Reference));
        self.divert_if(bound, handled);
        let arr = self.b.ins().load(I64, trusted(), room, iter_at);
        let first = self.b.ins().load(I32, trusted(), room, iter_at + FE_POS_AT);
        let used = self.b.ins().load(I32, trusted(), arr, imm(Arr::USED_AT));
        let data = self.b.ins().load(I64, trusted(), arr, imm(Arr::DATA_AT));
        let walk = self.b.create_block();
        let pos = self.b.append_block_param(walk, I32);
        let found = self.b.create_block();
        self.b.ins().jump(walk, &[first.into()]);
        self.b.switch_to_block(walk);
        let ended = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, pos, used);
        self.direction(at, ended)?;
        let offset = self.b.ins().uextend(I64, pos);
        let offset = self.b.ins().imul_imm_s(offset, SLOT);
        let element = self.b.ins().iadd(data, offset);
        let byte = self.b.ins().load(I8, trusted(), element, TYPE_AT);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Undef));
        let past = self.b.ins().iadd_imm_s(pos, 1);
        self.b.ins().brif(hole, walk, &[past.into()], found, &[]);
        self.b.seal_block(walk);
        self.b.switch_to_block(found);
        self.b.seal_block(found);
        self.b
            .ins()
            .store(trusted(), past, room, iter_at + FE_POS_AT);
        if let Some(key) = line.out {
            let key_value = self.b.ins().uextend(I64, pos);
            self.define(key, Scalar::Long(key_value))?;
        }
        let element = self.deref(element);
        let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        self.copy(element, info, target)?;
        self.release(line, old_word, old_info, Dtor::Gc)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(handled);
        self.b.seal_block(handled);
        self.stepped(at, line)?;
        if let Some(key) = line.out
            && self.value(key)?.home == Home::Register
        {
            let held = self.load(self.value(key)?.slot, Kind::Long, false)?;
            self.define(key, held)?;
        }
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }

    /// A cast of a scalar to a boolean, its truth, or its negation for
    /// `!`, as php's `zend_jit_bool_jmpznz` writes it
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:7533-7800`).
    fn bool(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let held = self.scalar(at, line.op1)?;
        let truth = self.warned_truth(line, held)?;
        let truth = match line.opcode {
            Opcode::BoolNot => self.b.ins().icmp_imm_s(IntCC::Equal, truth, 0),
            _ => truth,
        };
        if let Some(out) = line.out {
            self.define(out, Scalar::Bool(truth))?;
        }
        Ok(())
    }

    /// `FREE_OP` of `operand` where it is a temporary, as the handler
    /// takes it: what it holds out of its slot, which is left `UNDEF`,
    /// then its owner given up as [`Emit::release`] gives it up under
    /// [`Dtor::Nogc`], as the handler's `FREE_OP` and php's `jit_FREE_OP`
    /// give it up (`Z/ext/opcache/jit/zend_jit_ir.c:1905-1915`), what a
    /// destructor throws leaving through the opline's unwind. A
    /// `foreach`'s array freed at its loop's end is this, php's
    /// `zend_jit_free` of `FE_FREE` over an array, which holds no iterator
    /// to delete (`Z/ext/opcache/jit/zend_jit_ir.c:11543-11580`), and a
    /// `FREE` of a call's unused value is this too, php's `zend_jit_free`
    /// of a `FREE`. An object a call returns, unused, its destructor served
    /// at the `FREE`, beside `func_num_args()`, whose handler the trace
    /// calls:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// function mk($i) { return $i % 3 ? new D : [$i]; }
    /// function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { mk($i); $t += func_num_args(); } return $t; }
    /// echo f(7), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// dddd7
    /// ```
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// function mk($i) { return $i % 3 ? new D : [$i]; }
    /// function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { mk($i); $t += func_num_args(); } return $t; }
    /// echo f(7), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// dddd7
    /// ```
    fn free_temp(&mut self, line: &Line, operand: Operand) -> Result<(), Abort> {
        let Operand::Value(value) = operand else {
            return Ok(());
        };
        let held = self.value(value)?;
        if held.home == Home::Register || !self.is_temporary(held.slot)? {
            return Ok(());
        }
        self.free_slot(line, held.slot)
    }

    /// The temporary `slot` freed as [`Emit::free_temp`] frees it.
    fn free_slot(&mut self, line: &Line, slot: Slot) -> Result<(), Abort> {
        let room = self.room(slot.frame)?;
        let at = slot_offset(slot.var)?;
        let info = self.b.ins().load(I32, trusted(), room, at + TYPE_AT);
        let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
        self.b.ins().store(trusted(), undef, room, at + TYPE_AT);
        let word = self.b.ins().load(I64, trusted(), room, at);
        self.release(line, word, info, Dtor::Nogc)
    }

    /// What a slot held, `word` and its type word `info`, given up after
    /// an inline write over it or as a temporary is freed, as php's
    /// `jit_ZVAL_PTR_DTOR` gives it up (`Z/ext/opcache/jit/zend_jit_ir.c:1819-1903`):
    /// where it is counted, a `GC_DELREF` inline where another owner
    /// stands and the collector holds the value already or never takes
    /// it (`GC_MAY_LEAK`'s bits, [`Zval::LEAK_BITS`]), as a string's;
    /// the last owner, a value the collector may take and a reference,
    /// whose value it looks through to, through the release kernel, a
    /// destructor's throw leaving through the opline's unwind; under
    /// [`Dtor::Nogc`] the last owner alone, as `zval_ptr_dtor_nogc` buffers
    /// no possible root. A string
    /// two variables hold, one of them written over, and an object whose
    /// last owner an assignment gives up, its destructor called there:
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n; } }
    /// function f($n) { $s = str_repeat("ab", 2); for ($i = 0; $i < $n; $i++) { $t = $s; $t = "c"; $o = new D($i); } return $s; }
    /// echo f(6), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 012345abab
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n; } }
    /// function f($n) { $s = str_repeat("ab", 2); for ($i = 0; $i < $n; $i++) { $t = $s; $t = "c"; $o = new D($i); } return $s; }
    /// echo f(6), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 012345abab
    /// ```
    fn release(&mut self, line: &Line, word: Value, info: Value, dtor: Dtor) -> Result<(), Abort> {
        let leave = line.unwind.map(Leave::Unwind);
        self.release_through(line, [word, info], dtor, |emit, args| {
            emit.call(Kernel::Release, args, leave)
        })
    }

    /// [`Emit::release`] with what the code does at the release kernel
    /// its caller's, `kernel`, which is handed the kernel's arguments.
    fn release_through(
        &mut self,
        line: &Line,
        [word, info]: [Value; 2],
        dtor: Dtor,
        kernel: impl FnOnce(&mut Emit<'a>, &[Value; 4]) -> Result<(), Abort>,
    ) -> Result<(), Abort> {
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        let [room, opline] = self.served(line)?;
        self.when(counted, |emit| {
            let count_at = imm(Zval::COUNT_AT);
            let count = emit.b.ins().load(I32, trusted(), word, count_at);
            let called = emit.kernel_releases(word, info, count, dtor);
            let (cold, inline, done) = (
                emit.b.create_block(),
                emit.b.create_block(),
                emit.b.create_block(),
            );
            emit.b.set_cold_block(cold);
            emit.b.ins().brif(called, cold, &[], inline, &[]);
            emit.b.switch_to_block(inline);
            emit.b.seal_block(inline);
            let fewer = emit.b.ins().iadd_imm_s(count, -1);
            emit.b.ins().store(trusted(), fewer, word, count_at);
            emit.b.ins().jump(done, &[]);
            emit.b.switch_to_block(cold);
            emit.b.seal_block(cold);
            kernel(emit, &[room, opline, word, info])?;
            emit.b.ins().jump(done, &[]);
            emit.b.switch_to_block(done);
            emit.b.seal_block(done);
            Ok(())
        })
    }

    /// Whether giving up one owner of the counted value at `word`, of type
    /// word `info` and count word `count`, is the release kernel's, as
    /// php's `jit_ZVAL_PTR_DTOR` calls out (`Z/ext/opcache/jit/zend_jit_ir.c:1819-1903`):
    /// the last owner, or none, which the heap's own release reads with
    /// its pin; a reference, whose value the collector looks through to;
    /// and a value the collector may take as a possible root, whose
    /// `GC_TYPE_INFO` holds no bit of php's `GC_MAY_LEAK` mask
    /// ([`Zval::LEAK_BITS`]). Otherwise a `GC_DELREF` inline gives it up.
    fn kernel_releases(&mut self, word: Value, info: Value, count: Value, dtor: Dtor) -> Value {
        let owners = self.b.ins().band_imm_u(count, i64::from(Zval::COUNT_BITS));
        let last = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, owners, 1);
        if dtor == Dtor::Nogc {
            return last;
        }
        let kind = self.b.ins().band_imm_u(info, KIND);
        let reference = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, kind, tag(ZvalType::Reference));
        let gc = self
            .b
            .ins()
            .load(I32, trusted(), word, imm(Zval::GC_INFO_AT));
        let held = self.b.ins().band_imm_u(gc, i64::from(Zval::LEAK_BITS));
        let leaks = self.b.ins().icmp_imm_u(IntCC::Equal, held, 0);
        let called = self.b.ins().bor(last, reference);
        self.b.ins().bor(called, leaks)
    }

    /// A send, php's `zend_jit_send_val` and `zend_jit_send_var`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:9466-9790`): the call's slot for
    /// the argument written with a scalar, or with a value no scalar moved
    /// from a temporary, copied from a variable with one owner more, or
    /// copied as it is from a constant ([`Emit::copy_operand`]).
    fn send(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Imm::Arg(number) = line.imm else {
            return Err(mismatch("a send with no argument number"));
        };
        let arg = slot_offset(
            number
                .checked_sub(1)
                .ok_or_else(|| mismatch("an argument numbered zero"))?,
        )?;
        if self.scalar_kind(line.op1)?.is_none() {
            let call = self.call_room()?;
            return self.copy_operand(line.frame, line.op1, (call, arg));
        }
        let held = self.scalar(at, line.op1)?;
        let call = self.call_room()?;
        self.write(call, arg, held);
        Ok(())
    }

    /// A `SEND_REF` of a variable, php's `zend_jit_send_ref`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:9792-9860`): the reference the
    /// variable holds written into the call's slot for the argument with
    /// one owner more, `GC_ADDREF` and `ZVAL_REF`. A variable that holds no
    /// reference yet is made one in place, `ZVAL_MAKE_REF`, its reference
    /// laid down by [`Kernel::MakeRef`] as php's code calls the allocator
    /// for it, and then sent as one; the handler serves the send, cold,
    /// only where the memory limit refused the reference. A variable the
    /// trace holds in a register holds a scalar, and is stored to its slot
    /// first. Arrays passed by reference in a loop, each written through
    /// its reference by the callee, one made a reference at its first
    /// send, and a variable unset before each send, made a reference over
    /// `null`:
    ///
    /// ```php
    /// function bump(array &$a, array &$b, int &$n) { $a[0] += 1; $b[] = $n; $n++; }
    /// function f($k) { $x = [0]; $y = []; $c = 0; for ($i = 0; $i < $k; $i++) { bump($x, $y, $c); } return [$x[0], count($y), $c]; }
    /// function put(&$v) { $v = ($v ?? 0) + 2; }
    /// function g($k) { $t = 0; for ($i = 0; $i < $k; $i++) { unset($u); put($u); $t += $u; } return $t; }
    /// echo json_encode(f(9)), " ", g(9), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [9,9,9] 18
    /// ```
    ///
    /// ```php
    /// function bump(array &$a, array &$b, int &$n) { $a[0] += 1; $b[] = $n; $n++; }
    /// function f($k) { $x = [0]; $y = []; $c = 0; for ($i = 0; $i < $k; $i++) { bump($x, $y, $c); } return [$x[0], count($y), $c]; }
    /// function put(&$v) { $v = ($v ?? 0) + 2; }
    /// function g($k) { $t = 0; for ($i = 0; $i < $k; $i++) { unset($u); put($u); $t += $u; } return $t; }
    /// echo json_encode(f(9)), " ", g(9), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [9,9,9] 18
    /// ```
    ///
    /// An object sent by reference whose callee writes over it, its
    /// destructor served there, through the reference the send made:
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n; } }
    /// function drop(&$o) { $o = null; }
    /// function f($k) { $t = 0; for ($i = 0; $i < $k; $i++) { $o = new D($i); drop($o); $t += $o === null ? 1 : 0; } return $t; }
    /// $t = f(7); echo " $t\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 0123456 7
    /// ```
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n; } }
    /// function drop(&$o) { $o = null; }
    /// function f($k) { $t = 0; for ($i = 0; $i < $k; $i++) { $o = new D($i); drop($o); $t += $o === null ? 1 : 0; } return $t; }
    /// $t = f(7); echo " $t\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 0123456 7
    /// ```
    fn send_ref(&mut self, line: &Line) -> Result<(), Abort> {
        let Imm::Arg(number) = line.imm else {
            return Err(mismatch("a send with no argument number"));
        };
        let arg = slot_offset(
            number
                .checked_sub(1)
                .ok_or_else(|| mismatch("an argument numbered zero"))?,
        )?;
        let Operand::Value(value) = line.op1 else {
            return Err(mismatch("a send by reference of no variable"));
        };
        self.spill_operands(line)?;
        let (room, from_at) = self.place(self.value(value)?.slot)?;
        let zval = self.b.ins().iadd_imm_s(room, i64::from(from_at));
        let info = self.b.ins().load(I32, trusted(), room, from_at + TYPE_AT);
        let kind = self.b.ins().band_imm_u(info, KIND);
        let reference = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, kind, tag(ZvalType::Reference));
        let (sent, make, refused, on) = (
            self.b.create_block(),
            self.b.create_block(),
            self.b.create_block(),
            self.b.create_block(),
        );
        self.b.set_cold_block(refused);
        self.b.ins().brif(reference, sent, &[], make, &[]);
        // `ZVAL_MAKE_REF`: the reference laid down over what the variable
        // holds, then sent as one.
        self.b.switch_to_block(make);
        self.b.seal_block(make);
        let [served_room, opline] = self.served(line)?;
        let answer = self.answer();
        let made_at = self.b.ins().stack_addr(I64, answer, 0);
        self.call(
            Kernel::MakeRef,
            &[served_room, opline, zval, made_at],
            leave_of(line),
        )?;
        let laid = self.b.ins().stack_load(I64, I64, answer, 0);
        self.b.ins().brif(laid, sent, &[], refused, &[]);
        self.b.switch_to_block(sent);
        self.b.seal_block(sent);
        let info = self.b.ins().load(I32, trusted(), room, from_at + TYPE_AT);
        let call = self.call_room()?;
        self.copy_at(zval, info, (call, arg))?;
        self.b.ins().jump(on, &[]);
        // The memory limit refused the reference: the handler's send, which
        // raises its fatal.
        self.b.switch_to_block(refused);
        self.b.seal_block(refused);
        let func = self.func_of(line.frame)?;
        self.handled(line, Onward::Past(1), [served_room, opline, func])?;
        self.b.ins().jump(on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// A `TYPE_CHECK`, php's `zend_jit_type_check`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:8162-8330`): whether the value's
    /// type is one the mask names, `mask >> type & 1`, from the type the
    /// trace knows where it knows one and the slot's type byte otherwise;
    /// branched on where a jump is fused to it, written where not. A
    /// temporary it reads is given up after, `FREE_OP1`, through the
    /// release kernel where it is counted, as php's code calls
    /// `zval_ptr_dtor` there; its slot is left `UNDEF`, as the
    /// handler takes the operand. A node's element held to `null` and
    /// the elements of a list held to their types:
    ///
    /// ```php
    /// function leaves($node) {
    ///     if ($node[0] === null) { return 1; }
    ///     return leaves($node[0]) + leaves($node[1]);
    /// }
    /// function kinds($a) {
    ///     $s = "";
    ///     foreach ($a as $v) {
    ///         $s .= is_int($v) ? "i" : (is_string($v) ? "s" : (is_array($v) ? "a" : (is_null($v) ? "n" : (is_bool($v) ? "b" : "?"))));
    ///     }
    ///     return $s;
    /// }
    /// $t = [[[null, null], [null, null]], [null, null]];
    /// echo leaves($t), " ", kinds([1, "x", [2], null, true, 1.5, false, "y" . 3]), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 3 isanb?bs
    /// ```
    fn type_check(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Imm::Mask(mask) = line.imm else {
            return Err(mismatch("a type check with no mask"));
        };
        let Operand::Value(value) = line.op1 else {
            return Err(mismatch("a type check of no value"));
        };
        let asked = |tag: ZvalType| (mask >> u32::from(tag as u8)) & 1;
        let answer = if Kind::of(self.value(value)?.types).is_some() {
            if let Scalar::Bool(truth) = self.scalar(at, line.op1)? {
                let (yes, no) = (asked(ZvalType::True), asked(ZvalType::False));
                let truth = self.b.ins().icmp_imm_s(IntCC::NotEqual, truth, 0);
                let yes = self.b.ins().iconst(I8, i64::from(yes));
                let no = self.b.ins().iconst(I8, i64::from(no));
                self.b.ins().select(truth, yes, no)
            } else {
                let tag = self
                    .value(value)?
                    .types
                    .tag()
                    .ok_or_else(|| mismatch("a scalar of no one type"))?;
                self.b.ins().iconst(I8, i64::from(asked(tag)))
            }
        } else {
            let (room, slot_at) = self.place(self.value(value)?.slot)?;
            let byte = self.b.ins().load(I8, trusted(), room, slot_at + TYPE_AT);
            let byte = self.b.ins().uextend(I32, byte);
            let mask = self.b.ins().iconst(I32, i64::from(mask));
            let shifted = self.b.ins().ushr(mask, byte);
            let bit = self.b.ins().band_imm_u(shifted, 1);
            self.b.ins().ireduce(I8, bit)
        };
        let slot = self.value(value)?.slot;
        if self.is_temporary(slot)? && Kind::of(self.value(value)?.types).is_none() {
            // `FREE_OP1`: the temporary out of its slot, then its owner
            // given up.
            let (room, slot_at) = self.place(slot)?;
            let word = self.b.ins().load(I64, trusted(), room, slot_at);
            let info = self.b.ins().load(I32, trusted(), room, slot_at + TYPE_AT);
            let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
            self.b
                .ins()
                .store(trusted(), undef, room, slot_at + TYPE_AT);
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            let [room_arg, opline] = self.served(line)?;
            self.when(counted, |emit| {
                emit.call(
                    Kernel::Release,
                    &[room_arg, opline, word, info],
                    line.unwind.map(Leave::Unwind),
                )
            })?;
        }
        let jumps = match line.fused {
            Some(Fused::Jmpz) => self.b.ins().icmp_imm_s(IntCC::Equal, answer, 0),
            Some(Fused::Jmpnz) => self.b.ins().icmp_imm_s(IntCC::NotEqual, answer, 0),
            None => {
                if let Some(out) = line.out {
                    self.define(out, Scalar::Bool(answer))?;
                }
                answer
            }
        };
        self.direction(at, jumps)
    }

    /// The room of the call under way, php's `EX(call)`: in a register
    /// where compiled code pushed it, as the kernel that pushed it stood
    /// it otherwise.
    fn call_room(&mut self) -> Result<Value, Abort> {
        let (call, _) = *self
            .calls
            .last()
            .ok_or_else(|| mismatch("an argument with no call under way"))?;
        Ok(match call {
            CallRoom::Held(var) => self.b.use_var(var),
            CallRoom::Stood => self.stood_word(I64, STOOD_CALL),
        })
    }

    /// A `CHECK_FUNC_ARG` of an argument the callee takes by value:
    /// the call's bit that says the argument after it is taken by
    /// reference cleared, as `zend_jit_check_func_arg` clears
    /// `ZEND_CALL_SEND_ARG_BY_REF` (`Z/ext/opcache/jit/zend_jit_ir.c:9925-9960`).
    /// The `FETCH_DIM_FUNC_ARG` and the `SEND_FUNC_ARG` after it are then
    /// a read and a send by value, compiled inline, and a `SEND_VAR_EX` to
    /// a parameter taken by value is a `SEND_VAR`, which leaves its
    /// variable as it was; a parameter taken by reference is still the
    /// handlers', a write to the element:
    ///
    /// ```php
    /// function count_nodes($node) {
    ///     if ($node[0] === null) { return 1; }
    ///     return 1 + count_nodes($node[0]) + count_nodes($node[1]);
    /// }
    /// function make($d) { if ($d === 0) { return [null, null]; } $e = $d - 1; return [make($e), make($e)]; }
    /// function mark(&$x) { $x = 7; }
    /// function marks($a, $n) { for ($i = 0; $i < $n; $i++) { mark($a[$i]); } return $a; }
    /// echo count_nodes(make(8)), " ", implode(",", marks([1, 2, 3], 3)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 511 7,7,7
    /// ```
    fn check_func_arg(&mut self) -> Result<(), Abort> {
        let call = self.call_room()?;
        let info = imm(FRAME.info() as usize);
        let flags = self.b.ins().load(I32, trusted(), call, info);
        let flags = self
            .b
            .ins()
            .band_imm_u(flags, i64::from(!FRAME.send_by_ref()));
        self.b.ins().store(trusted(), flags, call, info);
        Ok(())
    }

    /// An inline `INIT_*` through [`Kernel::PushCall`], held to the callee
    /// the trace recorded: a refusal leaves through the opline's exit
    /// before anything is pushed.
    pub(super) fn push_call(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Some(Op::InitCall {
            callee: Callee::User(callee),
            ..
        }) = self.ir.ops.get(at.index() + 1)
        else {
            return Err(mismatch("an inline call with no callee recorded"));
        };
        let exit = line
            .exit
            .ok_or_else(|| mismatch("an inline call that cannot leave"))?;
        let callee = self.b.ins().iconst(I32, i64::from(engine_offset(*callee)));
        let [room, opline] = self.served(line)?;
        let func = self.func_of(line.frame)?;
        let stood = self.stood_out();
        self.call(
            Kernel::PushCall,
            &[room, opline, func, callee, stood],
            Some(Leave::Exit(exit)),
        )?;
        self.stood_back();
        let call = self.stood_word(I64, STOOD_CALL);
        let refused = self.b.ins().icmp_imm_s(IntCC::Equal, call, 0);
        self.leave_where(refused, exit)?;
        Ok(())
    }

    /// `body` where `cond` is not zero, then on.
    fn when(
        &mut self,
        cond: Value,
        body: impl FnOnce(&mut Emit<'a>) -> Result<(), Abort>,
    ) -> Result<(), Abort> {
        let then = self.b.create_block();
        let on = self.b.create_block();
        self.b.ins().brif(cond, then, &[], on, &[]);
        self.b.switch_to_block(then);
        self.b.seal_block(then);
        body(self)?;
        self.b.ins().jump(on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    // ------------------------------------------------------------ arithmetic

    /// php's binary arithmetic `op` over two scalars of the scalar core,
    /// leaving through `raises` where php raises an error the interpreter
    /// throws: a division or a modulo by zero, a shift by a negative
    /// number; none where the operand is a constant that cannot
    /// ([`cannot_raise`]), and nothing is tested.
    fn binary(
        &mut self,
        op: Opcode,
        a: Scalar,
        b: Scalar,
        raises: Option<Refuse>,
    ) -> Result<Num, Abort> {
        match (op, a, b) {
            (Opcode::Add | Opcode::Sub | Opcode::Mul, Scalar::Long(x), Scalar::Long(y)) => {
                let (long, overflowed) = match op {
                    Opcode::Add => self.b.ins().sadd_overflow(x, y),
                    Opcode::Sub => self.b.ins().ssub_overflow(x, y),
                    _ => self.b.ins().smul_overflow(x, y),
                };
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                let double = self.float(op, rx, ry);
                Ok(Num::Either {
                    overflowed,
                    long,
                    double,
                })
            }
            (Opcode::Add | Opcode::Sub | Opcode::Mul, _, _) => {
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                Ok(Num::Double(self.float(op, rx, ry)))
            }
            (Opcode::Div, Scalar::Long(x), Scalar::Long(y)) => {
                if let Some(refuse) = raises {
                    let zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                    self.refuse_if(zero, refuse)?;
                }
                let minus_one = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                let one = self.b.ins().iconst(I64, 1);
                let divisor = self.b.ins().select(minus_one, one, y);
                let quotient = self.b.ins().sdiv(x, divisor);
                let negated = self.b.ins().ineg(x);
                let long = self.b.ins().select(minus_one, negated, quotient);
                let remainder = self.b.ins().srem(x, divisor);
                let inexact = self.b.ins().icmp_imm_s(IntCC::NotEqual, remainder, 0);
                let lowest = self.b.ins().icmp_imm_s(IntCC::Equal, x, i64::MIN);
                let overflows = self.b.ins().band(minus_one, lowest);
                let overflowed = self.b.ins().bor(inexact, overflows);
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                let double = self.b.ins().fdiv(rx, ry);
                Ok(Num::Either {
                    overflowed,
                    long,
                    double,
                })
            }
            (Opcode::Div, _, _) => {
                if let Some(refuse) = raises {
                    // A long divisor is zero as a long, before it is
                    // converted.
                    let by_zero = if let Scalar::Long(y) = b {
                        self.b.ins().icmp_imm_s(IntCC::Equal, y, 0)
                    } else {
                        let ry = self.real(b)?;
                        let zero = self.b.ins().f64const(0.0);
                        self.b.ins().fcmp(FloatCC::Equal, ry, zero)
                    };
                    self.refuse_if(by_zero, refuse)?;
                }
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                Ok(Num::Double(self.b.ins().fdiv(rx, ry)))
            }
            (Opcode::Mod, Scalar::Long(x), Scalar::Long(y)) => {
                if let Some(refuse) = raises {
                    let zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                    self.refuse_if(zero, refuse)?;
                }
                // php answers zero for `% -1` rather than overflow.
                let minus_one = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                let one = self.b.ins().iconst(I64, 1);
                let divisor = self.b.ins().select(minus_one, one, y);
                Ok(Num::Long(self.b.ins().srem(x, divisor)))
            }
            (Opcode::Sl | Opcode::Sr, Scalar::Long(x), Scalar::Long(y)) => {
                if let Some(refuse) = raises {
                    let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, y, 0);
                    self.refuse_if(negative, refuse)?;
                }
                let wide = self
                    .b
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, y, 64);
                let (shifted, past) = if op == Opcode::Sl {
                    (self.b.ins().ishl(x, y), self.b.ins().iconst(I64, 0))
                } else {
                    (self.b.ins().sshr(x, y), self.b.ins().sshr_imm_u(x, 63))
                };
                Ok(Num::Long(self.b.ins().select(wide, past, shifted)))
            }
            (Opcode::BwAnd, Scalar::Long(x), Scalar::Long(y)) => {
                Ok(Num::Long(self.b.ins().band(x, y)))
            }
            (Opcode::BwOr, Scalar::Long(x), Scalar::Long(y)) => {
                Ok(Num::Long(self.b.ins().bor(x, y)))
            }
            (Opcode::BwXor, Scalar::Long(x), Scalar::Long(y)) => {
                Ok(Num::Long(self.b.ins().bxor(x, y)))
            }
            _ => Err(mismatch(
                "an inline arithmetic over operands it does not serve",
            )),
        }
    }

    /// The double arithmetic of `op`.
    fn float(&mut self, op: Opcode, x: Value, y: Value) -> Value {
        match op {
            Opcode::Add => self.b.ins().fadd(x, y),
            Opcode::Sub => self.b.ins().fsub(x, y),
            _ => self.b.ins().fmul(x, y),
        }
    }

    /// A number as a double, a long converted as php's `(double)` does.
    fn real(&mut self, held: Scalar) -> Result<Value, Abort> {
        match held {
            Scalar::Long(x) => Ok(self.b.ins().fcvt_from_sint(F64, x)),
            Scalar::Double(x) => Ok(x),
            Scalar::Bool(_) | Scalar::Null => Err(mismatch("an inline arithmetic over no number")),
        }
    }

    /// php's comparison `op` of two scalars, an `i8` of zero or one: two
    /// longs as longs, a long beside a double as two doubles
    /// (`compare_longs_to_doubles`), so NaN is equal to nothing and below
    /// nothing; identity as php's `zend_is_identical` of scalars.
    fn comparison(&mut self, op: Opcode, a: Scalar, b: Scalar) -> Result<Value, Abort> {
        let numeric = |cc: IntCC, fc: FloatCC| (cc, fc);
        let (cc, fc) = match op {
            Opcode::IsSmaller => numeric(IntCC::SignedLessThan, FloatCC::LessThan),
            Opcode::IsSmallerOrEqual => {
                numeric(IntCC::SignedLessThanOrEqual, FloatCC::LessThanOrEqual)
            }
            Opcode::IsEqual => numeric(IntCC::Equal, FloatCC::Equal),
            Opcode::IsNotEqual => numeric(IntCC::NotEqual, FloatCC::NotEqual),
            Opcode::IsIdentical | Opcode::IsNotIdentical => {
                let same = self.identical(a, b);
                return Ok(if op == Opcode::IsIdentical {
                    same
                } else {
                    self.b.ins().icmp_imm_s(IntCC::Equal, same, 0)
                });
            }
            _ => return Err(mismatch("an inline comparison it does not serve")),
        };
        if let (Scalar::Long(x), Scalar::Long(y)) = (a, b) {
            return Ok(self.b.ins().icmp(cc, x, y));
        }
        let (rx, ry) = (self.real(a)?, self.real(b)?);
        Ok(self.b.ins().fcmp(fc, rx, ry))
    }

    /// Whether two scalars are identical: of one type and one value.
    fn identical(&mut self, a: Scalar, b: Scalar) -> Value {
        match (a, b) {
            (Scalar::Long(x), Scalar::Long(y)) | (Scalar::Bool(x), Scalar::Bool(y)) => {
                self.b.ins().icmp(IntCC::Equal, x, y)
            }
            (Scalar::Double(x), Scalar::Double(y)) => self.b.ins().fcmp(FloatCC::Equal, x, y),
            (Scalar::Null, Scalar::Null) => self.b.ins().iconst(I8, 1),
            _ => self.b.ins().iconst(I8, 0),
        }
    }

    /// php's `zend_is_true` of a scalar: a long or a double not zero, NaN
    /// true.
    fn truth(&mut self, held: Scalar) -> Value {
        match held {
            Scalar::Long(x) => self.b.ins().icmp_imm_s(IntCC::NotEqual, x, 0),
            Scalar::Double(x) => {
                let zero = self.b.ins().f64const(0.0);
                self.b.ins().fcmp(FloatCC::NotEqual, x, zero)
            }
            Scalar::Bool(b) => b,
            Scalar::Null => self.b.ins().iconst(I8, 0),
        }
    }

    // ------------------------------------------------------------ values

    /// A value as the op being lowered reads it: its types from where it
    /// was made until a guard before this op or at it read them
    /// ([`Unguarded`](crate::types::Unguarded)). php's trace SSA numbers a
    /// variable anew where a line reads it and may take an owner of it, so
    /// a guard after that line marks the new number and the line reads the
    /// old one, unmarked, as anything; the trace holds one value in the
    /// slot, so it is this that reads it so. A frameless call before the
    /// `$w = $e` that holds a string warns of what it finds unset, where a
    /// side trace unset it and linked back:
    ///
    /// ```php
    /// set_error_handler(function ($no, $msg) { global $seen; $seen[$msg] = ($seen[$msg] ?? 0) + 1; return true; });
    /// function f($n, $l) { $e = "a"; $c = 0; for ($i = 0; $i < $n; $i++) { $c += (int)str_contains($e, "a"); $w = $e; if ($i % 90 == 89) { unset($e); } else { $e = current($l); } } return $c; }
    /// echo f(200, ["abc"]), " ", json_encode($seen), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 198 {"Undefined variable $e":4,"str_contains(): Passing null to parameter #1 ($haystack) of type string is deprecated":2}
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// set_error_handler(function ($no, $msg) { global $seen; $seen[$msg] = ($seen[$msg] ?? 0) + 1; return true; });
    /// function f($n, $l) { $e = "a"; $c = 0; for ($i = 0; $i < $n; $i++) { $c += (int)str_contains($e, "a"); $w = $e; if ($i % 90 == 89) { unset($e); } else { $e = current($l); } } return $c; }
    /// echo f(200, ["abc"]), " ", json_encode($seen), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 198 {"Undefined variable $e":4,"str_contains(): Passing null to parameter #1 ($haystack) of type string is deprecated":2}
    /// ```
    fn value(&self, value: ValueId) -> Result<crate::types::Value, Abort> {
        let held = self
            .ir
            .values
            .get(value.0 as usize)
            .copied()
            .ok_or_else(|| mismatch("a value past the trace's"))?;
        Ok(match (held.unguarded, self.reading) {
            (Some(unguarded), Some(at)) if at < unguarded.until => crate::types::Value {
                types: unguarded.types,
                ..held
            },
            _ => held,
        })
    }

    /// The variables of `frame`, past which its slots are temporaries.
    fn frame_cvs(&self, frame: Frame) -> Result<u32, Abort> {
        self.ir
            .frames
            .get(usize::from(frame.0))
            .map(|info| info.cvs)
            .ok_or_else(|| mismatch("a frame past the trace's"))
    }

    /// Whether `slot` is a temporary, numbered past its frame's variables.
    fn is_temporary(&self, slot: Slot) -> Result<bool, Abort> {
        Ok(slot.var >= self.frame_cvs(slot.frame)?)
    }

    /// The variable of `frame`'s room.
    fn room_var(&self, frame: Frame) -> Result<Variable, Abort> {
        self.rooms
            .get(usize::from(frame.0))
            .copied()
            .ok_or_else(|| mismatch("a frame past the trace's"))
    }

    /// Defines `frame`'s place on the stack as `place`, its variable
    /// declared where this is its first definition.
    fn place_frame(&mut self, frame: Frame, place: Value) -> Result<(), Abort> {
        let held = (self.refs.get_mut(usize::from(frame.0)))
            .ok_or_else(|| mismatch("a frame past the trace's"))?;
        let var = *held.get_or_insert_with(|| self.b.declare_var(I64));
        self.b.def_var(var, place);
        Ok(())
    }

    /// `frame`'s place on the stack, a `FrameRef`'s number. It is read
    /// only after the entry, the frame's `Op::Enter` or the `Op::Back`
    /// that lands in it defined it, the ops lowered in trace order: a
    /// read before is a lowering that named no place for the frame,
    /// which Cranelift would answer with zero, the stack's first frame,
    /// so it is php's `COMPILER_ERROR` instead, as a register
    /// read before its definition is ([`Emit::register`]).
    fn frame_ref(&mut self, frame: Frame) -> Result<Value, Abort> {
        let var = (self.refs.get(usize::from(frame.0)).copied())
            .ok_or_else(|| mismatch("a frame past the trace's"))?
            .ok_or_else(|| mismatch("a frame's place read before anything defined it"))?;
        Ok(self.b.use_var(var))
    }

    /// `frame`'s room.
    fn room(&mut self, frame: Frame) -> Result<Value, Abort> {
        let var = self.room_var(frame)?;
        Ok(self.b.use_var(var))
    }

    /// Where `slot` is: its frame's room and its offset there.
    fn place(&mut self, slot: Slot) -> Result<(Value, i32), Abort> {
        Ok((self.room(slot.frame)?, slot_offset(slot.var)?))
    }

    /// The variable of a value the trace keeps in a register, and its kind.
    fn register_var(&mut self, value: ValueId) -> Result<(Variable, Kind), Abort> {
        let kind = Kind::of(self.value(value)?.types)
            .ok_or_else(|| mismatch("a register of no scalar kind"))?;
        let at = value.0 as usize;
        let var = if let Some(var) = self.regs.get(at).copied().flatten() {
            var
        } else {
            let var = self.b.declare_var(kind.ty());
            if let Some(slot) = self.regs.get_mut(at) {
                *slot = Some(var);
            }
            var
        };
        Ok((var, kind))
    }

    /// What a value the trace keeps in a register holds. A register of a
    /// kind that carries a value is read only after the op that writes
    /// it defined it: a read before is a lowering that wrote the value's
    /// slot alone, which Cranelift would answer with zero, so it
    /// is php's `COMPILER_ERROR` instead. The order held is the order the
    /// ops are lowered in, not dominance: a definition on one arm of a
    /// branch and a read on the other is not caught here.
    fn register(&mut self, value: ValueId) -> Result<Scalar, Abort> {
        let defined = (self.regs.get(value.0 as usize).copied().flatten()).is_some();
        let (var, kind) = self.register_var(value)?;
        if !defined && kind.carries() {
            return Err(mismatch("a register read before anything defined it"));
        }
        Ok(match kind {
            Kind::Long => Scalar::Long(self.b.use_var(var)),
            Kind::Double => Scalar::Double(self.b.use_var(var)),
            Kind::Bool => Scalar::Bool(self.b.use_var(var)),
            Kind::Null => Scalar::Null,
            Kind::False => Scalar::Bool(self.b.ins().iconst(I8, 0)),
            Kind::True => Scalar::Bool(self.b.ins().iconst(I8, 1)),
        })
    }

    /// An operand of the inline op at `at` as the scalar it reads: a
    /// constant, a register, or its slot read as the type the guard
    /// before the op holds, through a reference where the guard holds one.
    fn scalar(&mut self, at: OpId, operand: Operand) -> Result<Scalar, Abort> {
        let Operand::Value(value) = operand else {
            return self.scalar_unguarded(operand);
        };
        let held = self.value(value)?;
        if held.home == Home::Register {
            return self.register(value);
        }
        let slot = held.slot;
        let (kind, through) = self.kind_at(at, value)?;
        let kind =
            kind.ok_or_else(|| mismatch("an inline opline reads a value of no scalar kind"))?;
        self.load(slot, kind, through)
    }

    /// The scalar kind the op at `at` reads `value` as, and whether it
    /// reads it through the reference it holds: the one a guard before
    /// the op held, or its own types' otherwise.
    fn kind_at(&self, at: OpId, value: ValueId) -> Result<(Option<Kind>, bool), Abort> {
        let guarded = self.ir.guards.iter().find_map(|g| match g.check {
            Check::Type { value: v, tag: t } if v == value && g.at == GuardAt::Before(at) => {
                Some((t, false))
            }
            Check::Deref { value: v, tag: t } if v == value && g.at == GuardAt::Before(at) => {
                Some((t, true))
            }
            _ => None,
        });
        Ok(match guarded {
            Some((t, through)) => (Kind::of_tag(t), through),
            None => (Kind::of(self.value(value)?.types), false),
        })
    }

    /// Whether the op at `at` reads `operand` as a long.
    fn long_at(&self, at: OpId, operand: Operand) -> Result<bool, Abort> {
        Ok(match operand {
            Operand::Value(value) => self.kind_at(at, value)?.0 == Some(Kind::Long),
            operand => self.scalar_kind(operand)? == Some(Kind::Long),
        })
    }

    /// An operand that is a constant or a value read as its own types say.
    fn scalar_unguarded(&mut self, operand: Operand) -> Result<Scalar, Abort> {
        match operand {
            Operand::Constant(constant) => Ok(match constant {
                Constant::Null => Scalar::Null,
                Constant::False => Scalar::Bool(self.b.ins().iconst(I8, 0)),
                Constant::True => Scalar::Bool(self.b.ins().iconst(I8, 1)),
                Constant::Long(n) => Scalar::Long(self.b.ins().iconst(I64, n)),
                Constant::Double(d) => Scalar::Double(self.b.ins().f64const(d)),
                Constant::Other(_) => {
                    return Err(mismatch("an inline opline reads a constant of no scalar"));
                }
            }),
            Operand::Value(value) => {
                let held = self.value(value)?;
                if held.home == Home::Register {
                    return self.register(value);
                }
                let kind = Kind::of(held.types)
                    .ok_or_else(|| mismatch("an inline opline reads a value of no scalar kind"))?;
                self.load(held.slot, kind, false)
            }
            Operand::Unused => Err(mismatch("an inline opline reads an unused operand")),
        }
    }

    /// `slot` read as `kind`, through the reference it holds where
    /// `through`.
    fn load(&mut self, slot: Slot, kind: Kind, through: bool) -> Result<Scalar, Abort> {
        let (room, at) = self.place(slot)?;
        if through {
            let reference = self.b.ins().load(I64, trusted(), room, at);
            return Ok(self.read(reference, imm(Ref::VALUE_AT), kind));
        }
        Ok(self.read(room, at, kind))
    }

    /// The zval at `base + at` read as `kind`.
    fn read(&mut self, base: Value, at: i32, kind: Kind) -> Scalar {
        match kind {
            Kind::Long => Scalar::Long(self.b.ins().load(I64, trusted(), base, at)),
            Kind::Double => Scalar::Double(self.b.ins().load(F64, trusted(), base, at)),
            Kind::Bool => {
                let byte = self.b.ins().load(I8, trusted(), base, at + TYPE_AT);
                Scalar::Bool(
                    self.b
                        .ins()
                        .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::True)),
                )
            }
            Kind::Null => Scalar::Null,
            Kind::False => Scalar::Bool(self.b.ins().iconst(I8, 0)),
            Kind::True => Scalar::Bool(self.b.ins().iconst(I8, 1)),
        }
    }

    /// `held` written to `slot` with its type word, as php's `ZVAL_LONG`,
    /// `ZVAL_DOUBLE`, `ZVAL_BOOL` and `ZVAL_NULL` write one.
    fn store(&mut self, slot: Slot, held: Scalar) -> Result<(), Abort> {
        let (room, at) = self.place(slot)?;
        self.write(room, at, held);
        Ok(())
    }

    /// `held` written to the zval at `base + at` with its type word.
    fn write(&mut self, base: Value, at: i32, held: Scalar) {
        let info = match held {
            Scalar::Long(x) => {
                self.b.ins().store(trusted(), x, base, at);
                self.b.ins().iconst(I32, tag(ZvalType::Long))
            }
            Scalar::Double(x) => {
                self.b.ins().store(trusted(), x, base, at);
                self.b.ins().iconst(I32, tag(ZvalType::Double))
            }
            Scalar::Bool(b) => {
                let b = self.b.ins().uextend(I32, b);
                self.b.ins().iadd_imm_s(b, tag(ZvalType::False))
            }
            Scalar::Null => self.b.ins().iconst(I32, tag(ZvalType::Null)),
        };
        self.b.ins().store(trusted(), info, base, at + TYPE_AT);
    }

    /// The value `value` an op writes, as `held`: its register, or its
    /// slot.
    fn define(&mut self, value: ValueId, held: Scalar) -> Result<(), Abort> {
        let target = self.value(value)?;
        if target.home == Home::Memory {
            return self.store(target.slot, held);
        }
        let (var, kind) = self.register_var(value)?;
        match (kind, held) {
            (Kind::Long, Scalar::Long(x))
            | (Kind::Double, Scalar::Double(x))
            | (Kind::Bool, Scalar::Bool(x)) => self.b.def_var(var, x),
            (Kind::Null, Scalar::Null) | (Kind::False | Kind::True, Scalar::Bool(_)) => {}
            _ => return Err(mismatch("a register written with another kind")),
        }
        if self.stored_at_def.contains(&value) {
            self.store(target.slot, held)?;
        }
        Ok(())
    }

    /// The value `value` an arithmetic writes, as `num`: one that may be
    /// either kind goes to its slot with the type word it turned out.
    fn define_num(&mut self, value: ValueId, num: Num) -> Result<(), Abort> {
        match num {
            Num::Long(x) => self.define(value, Scalar::Long(x)),
            Num::Double(x) => self.define(value, Scalar::Double(x)),
            Num::Either { .. } => {
                let target = self.value(value)?;
                if target.home == Home::Register {
                    return Err(mismatch("a register written with a number of either kind"));
                }
                let (room, at) = self.place(target.slot)?;
                self.write_num(room, at, num);
                Ok(())
            }
        }
    }

    /// `num` written to the zval at `base + at` with the type word it
    /// turned out.
    ///
    /// Variables of a file's own code outside any function, which php's analysis cannot type,
    /// incremented and multiplied past `PHP_INT_MAX`:
    ///
    /// ```php
    /// $a = PHP_INT_MAX - 3; $t = [];
    /// for ($i = 0; $i < 8; $i++) { $a++; $b = $a * 2; $c = $i * 3 - 1; $t[] = gettype($a) . ":" . $b . ":" . $c; }
    /// echo implode(",", $t), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// integer:1.844674407371E+19:-1,integer:1.844674407371E+19:2,integer:1.844674407371E+19:5,double:1.844674407371E+19:8,double:1.844674407371E+19:11,double:1.844674407371E+19:14,double:1.844674407371E+19:17,double:1.844674407371E+19:20
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// $a = PHP_INT_MAX - 3; $t = [];
    /// for ($i = 0; $i < 8; $i++) { $a++; $b = $a * 2; $c = $i * 3 - 1; $t[] = gettype($a) . ":" . $b . ":" . $c; }
    /// echo implode(",", $t), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// integer:1.844674407371E+19:-1,integer:1.844674407371E+19:2,integer:1.844674407371E+19:5,double:1.844674407371E+19:8,double:1.844674407371E+19:11,double:1.844674407371E+19:14,double:1.844674407371E+19:17,double:1.844674407371E+19:20
    /// ```
    fn write_num(&mut self, base: Value, at: i32, num: Num) {
        match num {
            Num::Long(x) => self.write(base, at, Scalar::Long(x)),
            Num::Double(x) => self.write(base, at, Scalar::Double(x)),
            Num::Either {
                overflowed,
                long,
                double,
            } => {
                // The double written on its own cold way, as php's
                // `zend_jit_math_long_long` writes an overflow's, so the
                // way a long takes makes none.
                let (as_long, as_double, done) = (
                    self.b.create_block(),
                    self.b.create_block(),
                    self.b.create_block(),
                );
                self.b.set_cold_block(as_double);
                self.b.ins().brif(overflowed, as_double, &[], as_long, &[]);
                for (block, held) in [
                    (as_long, Scalar::Long(long)),
                    (as_double, Scalar::Double(double)),
                ] {
                    self.b.switch_to_block(block);
                    self.b.seal_block(block);
                    self.write(base, at, held);
                    self.b.ins().jump(done, &[]);
                }
                self.b.switch_to_block(done);
                self.b.seal_block(done);
            }
        }
    }
}

/// What the guards of an op read of its array, which the op's own read goes
/// on from: Cranelift forgets a load at every block of a loop that stores,
/// and each guard ends a block. Nothing writes either word between a
/// guard and the op it stands before, the guards of one op running in
/// a line, and an op starts with none.
#[derive(Clone, Copy, Default)]
struct GuardsRead {
    /// The reference a variable holds, by its slot.
    reference: Option<(Slot, Value)>,
    /// The array a variable holds, by its slot and the way it is read.
    array: Option<(Slot, Read, Value)>,
}

/// The values [`Emit::define`] writes to their slot as it defines them:
/// what a memory phi's back edge brings from a register an op defines,
/// as php's allocator marks such a use `ZREG_STORE`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:3339-3345`). The rest,
/// [`Emit::store_brought`] writes at the back edge.
///
/// A loop left between the write and the back edge, the slot written
/// already, and read from a callee that sees the frame:
///
/// ```php
/// function f($n) { $x = 7; $y = 0.5; for ($j = 0; $j < $n; $j++) { $x = $j * 1.5; $y = $x + 1; if ($j == 3) { break; } } return [$x, $y, get_defined_vars()['x']]; }
/// echo json_encode(f(9)), json_encode(f(2)), json_encode(f(0)), "\n";
/// ```
/// ```expect regime=jit-force process
/// [4.5,5.5,4.5][1.5,2.5,1.5][7,0.5,7]
/// ```
///
/// The same where every guard fails once:
///
/// ```php
/// function f($n) { $x = 7; $y = 0.5; for ($j = 0; $j < $n; $j++) { $x = $j * 1.5; $y = $x + 1; if ($j == 3) { break; } } return [$x, $y, get_defined_vars()['x']]; }
/// echo json_encode(f(9)), json_encode(f(2)), json_encode(f(0)), "\n";
/// ```
/// ```expect regime=jit-exit-fuzz process
/// [4.5,5.5,4.5][1.5,2.5,1.5][7,0.5,7]
/// ```
fn stored_at_def(ir: &TraceIr) -> HashSet<ValueId> {
    let brought = |value: &crate::types::Value| {
        let Def::Phi { back, .. } = value.def else {
            return None;
        };
        let held = ir.values.get(back.0 as usize)?;
        let carried = matches!(
            Kind::of(held.types),
            Some(Kind::Long | Kind::Double | Kind::Bool)
        );
        let made = matches!(held.def, Def::Op(_));
        (value.home == Home::Memory && held.home == Home::Register && made && carried)
            .then_some(back)
    };
    ir.values.iter().filter_map(brought).collect()
}

/// Where slot `var` is in its frame's room: past php's
/// `ZEND_CALL_FRAME_SLOT`, sixteen bytes a slot.
fn slot_offset(var: u32) -> Result<i32, Abort> {
    i64::from(var)
        .checked_mul(SLOT)
        .and_then(|at| at.checked_add(i64::from(imm(ROOM_BYTES))))
        .and_then(|at| i32::try_from(at).ok())
        .ok_or_else(|| mismatch("a slot past an immediate"))
}
