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

use std::mem::offset_of;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I8, I32, I64};
use cranelift_codegen::ir::{
    AbiParam, Block, BlockArg, Function, InstBuilder, MemFlagsData, SigRef, Signature, SourceLoc,
    StackSlot, StackSlotData, StackSlotKind, Type, UserFuncName, Value,
};
use cranelift_codegen::isa::{CallConv, TargetFrontendConfig};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use php_engine::tier::{FRAME, FuncRef, PlainCall};
use php_heap::{Arr, ROOM_BYTES, Ref, Zval, ZvalType};
use php_unit::{Opcode, OplineIdx};

use super::{
    ABANDONED, DECLINED, ENTRY_AT, INTERRUPT_AT, INTERRUPTED, Kernel, ROOT_AT, STACK_AT, STOOD_AT,
    Stood, Symbols, UNWOUND, located,
};

mod calls;
mod doors;
use crate::record::{Began, Callee, Stop};
use crate::types::{
    Abort, BodyEntry, Check, Constant, Def, End, ExitId, Form, Frame, Fused, Guard, GuardAt, Home,
    Imm, Interrupt, Layout, Leave, Line, Op, OpId, Operand, Read, Repeat, Slot, Store, Stored,
    TraceIr, Types, UnwindId, ValueId, cannot_raise,
};

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
/// once by its byte of `fuzz`; jumping through its entry slots and its
/// link as `joins` says; with how many such guards it holds.
pub(super) fn lower(
    ir: &TraceIr,
    config: TargetFrontendConfig,
    (symbols, held): (&Symbols, &[PlainCall]),
    (fuzz, joins): (Option<Fuzz>, Joins),
) -> Result<(Function, usize), Abort> {
    let mut func = Function::with_name_signature(UserFuncName::user(0, 0), body_signature());
    let mut context = FunctionBuilderContext::new();
    let builder = FunctionBuilder::new(&mut func, &mut context);
    let mut emit = Emit::new(ir, builder, config.default_call_conv, (symbols, held));
    emit.fuzz = fuzz;
    emit.joins = joins;
    emit.trace()?;
    let sites = emit.fuzzed_sites;
    emit.b.finalize(config);
    Ok((func, sites))
}

/// A trace's body's signature: the `ExitCtx` and the room of the frame
/// it enters on, the exit or the answer it leaves with, on Cranelift's
/// tail convention, so a body hands on to another by a tail call.
fn body_signature() -> Signature {
    let mut sig = Signature::new(CallConv::Tail);
    sig.params.extend([AbiParam::new(I64), AbiParam::new(I64)]);
    sig.returns.push(AbiParam::new(I32));
    sig
}

/// The trace prologue (`super::Backend`): on the host's convention, the
/// `ExitCtx`, the room and the body's address, the body called on the
/// tail convention and its answer answered.
pub(super) fn prologue(config: TargetFrontendConfig) -> Function {
    let mut sig = Signature::new(config.default_call_conv);
    sig.params.extend([AbiParam::new(I64); 3]);
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
    let call = b.ins().call_indirect(body, params[2], &params[..2]);
    let answer = b.inst_results(call).to_vec();
    b.ins().return_(&answer);
    b.finalize(config);
    func
}

/// Where a body jumps besides its exits (#3229's ruling): the address of
/// its first entry slot, one per exit, of its first landing slot, and the
/// trace its link jumps to, where it links to one with code.
#[derive(Debug, Clone, Copy)]
pub(super) struct Joins {
    pub(super) slots: usize,
    pub(super) landings: usize,
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

/// Where a kernel serving `line` leaves: its exit, where an inline form
/// has one, else its unwind.
fn leave_of(line: &Line) -> Option<Leave> {
    (line.exit.map(Leave::Exit)).or_else(|| line.unwind.map(Leave::Unwind))
}

/// Where an opline's handler goes on: in its own frame, this many
/// oplines past it, one or two past an `OP_DATA`, having done nothing
/// else to the frame chain but pop a native's call it made, or thrown
/// there. What [`Kernel::Handle`] serves, as php's code calls the
/// handler of each such opline it compiles no inline code for directly
/// (`zend_jit_handler`, `Z/ext/opcache/jit/zend_jit_ir.c:4120-4157`) and
/// goes on with no IP guard, which `zend_jit_trace_handler` takes only
/// past an opline `zend_jit_trace_may_exit` names (`:17129-17236`;
/// `Z/ext/opcache/jit/zend_jit_trace.c:261-330`). The kernel stands the
/// frame at the opline before the handler runs, php's `SAVE_OPLINE()`,
/// so what the handler raises, and the unwind of what it throws, see the
/// frame where php's do, and compiled code writes where it stands past
/// it, which the handler leaves no `Stood` of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Onward(u32);

impl Onward {
    /// Where `opcode`'s handler goes on, for the oplines whose handler
    /// can neither leave its frame, push or replace one, enter the
    /// loop with the frame unstood nor jump: building an array and
    /// adding to it, a native's call, made and popped by its handler, a
    /// concatenation and a rope's pieces, whose string arms the handler
    /// holds as php's own functions over two strings hold them, an
    /// assignment, a write to an element and a compound one, a send, a
    /// frameless call to a native, a string's length, a cast to a
    /// boolean and the free of a `foreach`'s array. None for every other
    /// opline: a jump and a comparison, whose handler may take the jump
    /// fused to it, a call's `INIT_*` and its `DO_*CALL` into user code,
    /// a return, an include or an `eval`, a generator's opcodes, a
    /// `finally`'s `FAST_CALL` and `FAST_RET`, and the two silence
    /// opcodes, whose observers a step tells.
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
    fn of(opcode: Opcode) -> Option<Onward> {
        match opcode {
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
            | Opcode::FeFree => Some(Onward(1)),
            Opcode::AssignDim | Opcode::AssignDimOp | Opcode::FramelessIcall3 => Some(Onward(2)),
            _ => None,
        }
    }
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
    /// number, kept where `stack` is.
    refs: Vec<Variable>,
    /// The place of the frame the last `RETURN` leaves to, which its
    /// `Op::Back` names the frame it lands in by.
    returned_to: Option<Variable>,
    /// The `ExitCtx`, the function's first parameter.
    cx: Option<Value>,
    /// The address of the request's interrupt byte, read once as a
    /// looping trace whose header tests it enters.
    interrupt: Option<Value>,
    /// Where a kernel writes where the interpreter stands.
    stood: StackSlot,
    /// Per frame of the trace, the room it runs in.
    rooms: Vec<Variable>,
    /// Per value the trace keeps in a register, its variable.
    regs: Vec<Option<Variable>>,
    /// The rooms of the calls under way, the innermost last, each with
    /// the plain call it is where compiled code pushed it in place.
    calls: Vec<(CallRoom, Option<&'a PlainCall>)>,
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
    /// The frame the trace runs in now.
    running: Frame,
    /// Under `jit.mode=exit-fuzz`, the bytes each guard fails once by.
    fuzz: Option<Fuzz>,
    /// How many of them the guards met so far took.
    fuzzed_sites: usize,
    /// Its entry slots and its link (`doors`).
    joins: Joins,
    /// Per exit, the block a failed guard leaves it through where the
    /// exit may jump to its side trace, taking the call under way there.
    doors: Vec<Option<Block>>,
    /// Per exit, the block that answers its number and stores nothing,
    /// where a jump finds no trace to enter.
    answers: Vec<Option<Block>>,
}

impl<'a> Emit<'a> {
    fn new(
        ir: &'a TraceIr,
        mut b: FunctionBuilder<'a>,
        call_conv: CallConv,
        (symbols, held): (&'a Symbols, &'a [PlainCall]),
    ) -> Emit<'a> {
        let rooms = ir.frames.iter().map(|_| b.declare_var(I64)).collect();
        let refs = ir.frames.iter().map(|_| b.declare_var(I64)).collect();
        let stood = b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            u32::try_from(size_of::<Stood>()).unwrap_or(u32::MAX),
            3,
        ));
        Emit {
            ir,
            b,
            call_conv,
            symbols,
            held,
            stack: None,
            refs,
            returned_to: None,
            cx: None,
            interrupt: None,
            stood,
            rooms,
            regs: vec![None; ir.values.len()],
            calls: Vec::new(),
            sigs: [None; Kernel::ALL.len()],
            exits: vec![None; ir.exits.len()],
            unwinds: vec![None; ir.unwinds.len()],
            abandon: None,
            escape: None,
            stub: None,
            declined: None,
            running: Frame::ROOT,
            fuzz: None,
            fuzzed_sites: 0,
            joins: Joins {
                slots: 0,
                landings: 0,
                linked: None,
            },
            doors: vec![None; ir.exits.len()],
            answers: vec![None; ir.exits.len()],
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
        let [cx, room] = *self.b.block_params(entry) else {
            return Err(mismatch("a trace's function takes two parameters"));
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
        let place = self.b.ins().load(I32, trusted(), cx, ROOT_AT);
        let place = self.b.ins().uextend(I64, place);
        self.b.def_var(self.ref_var(Frame::ROOT)?, place);
        // Laid down before either way in: a guard at the body's entry
        // reads it too.
        self.stood_at_entry(cx);
        // A trace entered at its loop's body goes there; at its first op,
        // on.
        let body = match &self.ir.body {
            Some(body) => {
                let (into, first) = (self.b.create_block(), self.b.create_block());
                let at_body = self.b.ins().load(I32, trusted(), cx, ENTRY_AT);
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
                        self.b.def_var(self.ref_var(looped)?, place);
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
        self.exit_blocks()
    }

    /// Where the frame stands as the trace enters, which `enter` laid
    /// down in `cx`, held as where the last kernel left it, for a trace
    /// whose code reads it ([`super::stood_at_entry`]): its frame, its
    /// call, in one word its op array and opline, and the stack's room,
    /// laid out alike in both.
    fn stood_at_entry(&mut self, cx: Value) {
        if !super::stood_at_entry(self.ir) {
            return;
        }
        for at in [STOOD_FRAME, STOOD_CALL, STOOD_FUNC, STOOD_ROOM] {
            let word = self.b.ins().load(I64, trusted(), cx, STOOD_AT + at);
            self.b.ins().stack_store(I64, word, self.stood, at);
        }
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
                    read: Read::Through,
                    ..
                } => return Err(mismatch("a reference's layout held as a trace enters")),
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
                Check::Direction { .. } | Check::Stands { .. } | Check::Arm { .. } => {
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
                ..
            } => {
                let call = self.stood_word(I64, STOOD_CALL);
                let var = self.b.declare_var(I64);
                self.b.def_var(var, call);
                self.calls.push((CallRoom::Held(var), plain.as_ref()));
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
                    self.b.def_var(self.ref_var(*frame)?, place);
                }
                Ok(())
            }
            Op::Back { frame, escapes, .. } => {
                let room = self.stood_word(I64, STOOD_FRAME);
                self.b.def_var(self.room_var(*frame)?, room);
                self.running = *frame;
                if let Some(var) = self.returned_to.take() {
                    let place = self.b.use_var(var);
                    self.b.def_var(self.ref_var(*frame)?, place);
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
    /// one the recording saw (#3076), or at entry where the frame stands,
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
            Check::Deref { value, tag: t } => {
                let held = self.value(value)?;
                let (room, at) = self.place(held.slot)?;
                let byte = self.b.ins().load(I8, trusted(), room, at + TYPE_AT);
                let other =
                    self.b
                        .ins()
                        .icmp_imm_s(IntCC::NotEqual, byte, tag(ZvalType::Reference));
                self.leave_if(other, guard.exit)?;
                let reference = self.b.ins().load(I64, trusted(), room, at);
                let inner =
                    self.b
                        .ins()
                        .load(I8, trusted(), reference, imm(Ref::VALUE_AT) + TYPE_AT);
                let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, inner, tag(t));
                self.leave_if(other, guard.exit)?;
            }
            Check::Layout {
                value,
                layout,
                read,
            } => {
                let held = self.value(value)?;
                let array = self.array_of(held.slot, read)?;
                let flags = self.b.ins().load(I32, trusted(), array, imm(Arr::FLAGS_AT));
                let packed = self.b.ins().band_imm_u(flags, i64::from(Arr::PACKED));
                let cc = match layout {
                    Layout::Packed => IntCC::Equal,
                    Layout::Hash => IntCC::NotEqual,
                };
                let other = self.b.ins().icmp_imm_s(cc, packed, 0);
                self.leave_if(other, guard.exit)?;
            }
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
            Check::Direction { .. } | Check::Stands { .. } | Check::Arm { .. } => {
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
    /// the one held.
    fn settle(&mut self, at: OpId, defs: &[ValueId], num: Num) -> Result<Num, Abort> {
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
                    self.leave_if(overflowed, guard.exit)?;
                    Num::Long(long)
                }
                (
                    Num::Either {
                        overflowed, double, ..
                    },
                    Some(Kind::Double),
                ) => {
                    let exact = self.b.ins().icmp_imm_s(IntCC::Equal, overflowed, 0);
                    self.leave_if(exact, guard.exit)?;
                    Num::Double(double)
                }
                (Num::Long(_), Some(Kind::Long)) | (Num::Double(_), Some(Kind::Double)) => num,
                _ => return Err(mismatch("an arithmetic guarded as a type it cannot make")),
            };
        }
        Ok(num)
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
        let (to, args) = self.guard_exit(exit)?;
        let on = self.b.create_block();
        self.b.ins().brif(cond, to, &args, on, &[]);
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
    /// the header's nor the link's, to measure what it costs (#2960,
    /// §10).
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
            self.b.ins().return_(&[number]);
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
            self.b.ins().return_(&[number]);
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
            self.b.ins().return_(&[answer]);
        }
        if let Some(block) = self.abandon {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let answer = self.b.ins().iconst(I32, i64::from(ABANDONED));
            self.b.ins().return_(&[answer]);
        }
        if let Some(block) = self.escape {
            self.escaped(block)?;
        }
        if let Some(block) = self.stub {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let answer = self.b.ins().iconst(I32, i64::from(INTERRUPTED));
            self.b.ins().return_(&[answer]);
        }
        if let Some(block) = self.declined {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let answer = self.b.ins().iconst(I32, i64::from(DECLINED));
            self.b.ins().return_(&[answer]);
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
                // Its site wrote the slot before it branched here.
                Stored::Copied { .. } => {}
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
        let status = self.call_status(kernel, args)?;
        let to = self.left_by(leave)?;
        let on = self.b.create_block();
        self.b.ins().brif(status, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// `kernel`'s wrapper called at its address with the `ExitCtx` and
    /// `args`, and its status word, an `i32`, for the caller to test.
    fn call_status(&mut self, kernel: Kernel, args: &[Value]) -> Result<Value, Abort> {
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
            Kernel::Step => &[I64, I64, I32, I32, I64],
            Kernel::Handle => &[I64, I64, I32, I32],
            Kernel::PushCall | Kernel::Call => &[I64, I64, I32, I32, I32, I64],
            Kernel::Release | Kernel::FreeCv => &[I64, I64, I32, I64, I32],
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
        Ok(self.b.ins().iconst(I32, i64::from(engine_offset(func)?)))
    }

    /// A word of the [`Stood`] the last kernel wrote.
    fn stood_word(&mut self, ty: Type, at: i32) -> Value {
        self.b.ins().stack_load(I64, ty, self.stood, at)
    }

    /// `line` served whole by [`Kernel::Step`]: the operands it reads from
    /// registers stored first, so the interpreter reads them in their
    /// slots, and the way a jump it served went held to its guards.
    fn stepped(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        if line.opcode == Opcode::Return {
            self.returns_to(line)?;
        }

        self.step_kernel(at, line)?;
        self.after_step(at, line)
    }

    /// The kernel that serves `line` whole, [`Kernel::Step`], or
    /// [`Kernel::Call`] for a call the trace follows into its callee, its
    /// operands held in registers stored to their slots first.
    fn step_kernel(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        for operand in [line.op1, line.op2, line.data] {
            if let Operand::Value(value) = operand
                && self.value(value)?.home == Home::Register
            {
                let held = self.register(value)?;
                self.store(self.value(value)?.slot, held)?;
            }
        }
        let [room, opline] = self.served(line)?;
        let func = self.func_of(line.frame)?;
        let stood = self.b.ins().stack_addr(I64, self.stood, 0);
        match self.entered_by(at, line) {
            Some(callee) => {
                let callee = self.func_of(callee)?;
                self.call(
                    Kernel::Call,
                    &[room, opline, func, callee, stood],
                    leave_of(line),
                )?;
            }
            None if let Some(onward) = Onward::of(line.opcode)
                && self.within(at).is_empty() =>
            {
                self.handled(line, onward, [room, opline, func])?;
            }
            None => self.call(Kernel::Step, &[room, opline, func, stood], leave_of(line))?,
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
    fn handled(
        &mut self,
        line: &Line,
        Onward(places): Onward,
        [room, opline, func]: [Value; 3],
    ) -> Result<(), Abort> {
        self.call(Kernel::Handle, &[room, opline, func], leave_of(line))?;
        let next = self.b.ins().iconst(I32, i64::from(line.opline.0 + places));
        self.b.ins().stack_store(I64, func, self.stood, STOOD_FUNC);
        self.b
            .ins()
            .stack_store(I64, next, self.stood, STOOD_OPLINE);
        if line.opcode == Opcode::DoIcall {
            let call = self.call_left(line.frame)?;
            self.b.ins().stack_store(I64, call, self.stood, STOOD_CALL);
        }
        Ok(())
    }

    /// What follows a stepped `line`: a call it made off the calls under
    /// way, and the way a jump it served went held to its guards, read off
    /// the `Stood` the kernel, or the code that served it in place, wrote.
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
                Check::Arm { opline } => {
                    let went = self.stood_word(I32, STOOD_OPLINE);
                    let there = self
                        .b
                        .ins()
                        .icmp_imm_u(IntCC::Equal, went, i64::from(opline.0));
                    self.leave_where(there, guard.exit)?;
                }
                Check::Type { .. }
                | Check::Deref { .. }
                | Check::Layout { .. }
                | Check::Room { .. } => {
                    return Err(mismatch("a stepped opline holds a guard of another kind"));
                }
            }
        }
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
    fn stands(&mut self, func: FuncRef, opline: OplineIdx, exit: ExitId) -> Result<(), Abort> {
        let landed = self.stood_word(I32, STOOD_FUNC);
        let at = self.stood_word(I32, STOOD_OPLINE);
        let other_func =
            self.b
                .ins()
                .icmp_imm_u(IntCC::NotEqual, landed, i64::from(engine_offset(func)?));
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
    fn escapes(&mut self, func: FuncRef, opline: OplineIdx) -> Result<(), Abort> {
        let landed = self.stood_word(I32, STOOD_FUNC);
        let at = self.stood_word(I32, STOOD_OPLINE);
        let other_func =
            self.b
                .ins()
                .icmp_imm_u(IntCC::NotEqual, landed, i64::from(engine_offset(func)?));
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
    /// [`ESCAPED`](super::ESCAPED) answered, nothing stored, past the jump to the trace a
    /// landing slot names ([`Emit::escaped`]).
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
            Opcode::FetchDimR => self.fetch(at, line),
            Opcode::FeResetR => self.fe_reset(line),
            Opcode::FeFetchR => self.fe_fetch(at, line),
            Opcode::AssignDim => self.assign_dim(at, line),
            Opcode::AssignDimOp => self.assign_dim_op(at, line),
            Opcode::SendVal | Opcode::SendVar => self.send(at, line),
            Opcode::CheckFuncArg => self.check_func_arg(),
            Opcode::TypeCheck => self.type_check(at, line),
            // A send to a callee that takes the argument by value.
            Opcode::SendValEx if matches!(line.imm, Imm::Arg(_)) => self.send(at, line),
            Opcode::DoUcall | Opcode::DoFcall | Opcode::DoFcallByName => match line.imm {
                Imm::Make { returns } => self.make_in_place(at, line, returns),
                _ => self.stepped(at, line),
            },
            Opcode::Return if matches!(line.imm, Imm::Leave { .. }) => {
                self.leave_in_place(at, line)
            }
            Opcode::SendValEx | Opcode::Return => self.stepped(at, line),
            Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName => {
                match self.ir.ops.get(at.index() + 1) {
                    Some(Op::InitCall {
                        plain: Some(plain), ..
                    }) => self.push_in_place(line, plain),
                    _ => self.push_call(at, line),
                }
            }
            _ => Err(mismatch("an inline opline the lowering does not serve")),
        }
    }

    /// A conditional jump on a scalar: its direction held, and the
    /// boolean a `JMPZ_EX` or `JMPNZ_EX` writes.
    fn jump(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let held = self.scalar(at, line.op1)?;
        let truth = self.truth(held);
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
        let a = self.scalar(at, line.op1)?;
        let b = self.scalar(at, line.op2)?;
        let raises = if cannot_raise(op, line.op2) {
            None
        } else {
            Some(
                line.exit
                    .ok_or_else(|| mismatch("an arithmetic that cannot leave"))?,
            )
        };
        let num = self.binary(op, a, b, raises)?;
        let defs: Vec<ValueId> = [line.out, line.op1_def].into_iter().flatten().collect();
        let num = self.settle(at, &defs, num)?;
        for value in defs {
            self.define_num(value, num)?;
        }
        Ok(())
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
        let new = self.settle(at, &defs, new)?;
        for value in defs {
            self.define_num(value, new)?;
        }
        if let (false, Some(out)) = (pre, line.out) {
            self.define(out, old)?;
        }
        Ok(())
    }

    /// An assignment of a scalar to a variable: the variable written, and
    /// where what it held was counted, released after, as
    /// `zend_assign_to_variable` frees it, what its destructor throws
    /// leaving through the opline's exit. A register's slot is written
    /// too where the old value was counted, so no slot names a value
    /// freed.
    fn assign(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let held = self.scalar(at, line.op2)?;
        let Operand::Value(target) = line.op1 else {
            return Err(mismatch("an assignment to no variable"));
        };
        let slot = self.value(target)?.slot;
        let old = match line.unwind {
            Some(_) => {
                let (room, at) = self.place(slot)?;
                let word = self.b.ins().load(I64, trusted(), room, at);
                let info = self.b.ins().load(I32, trusted(), room, at + TYPE_AT);
                Some((word, info))
            }
            None => None,
        };
        if let Some(def) = line.op1_def {
            self.define(def, held)?;
            if old.is_some() && self.value(def)?.home == Home::Register {
                self.store(slot, held)?;
            }
        }
        if let Some(out) = line.out {
            self.define(out, held)?;
        }
        if let Some((word, info)) = old {
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            let [room, opline] = self.served(line)?;
            self.when(counted, |emit| {
                emit.call(
                    Kernel::Release,
                    &[room, opline, word, info],
                    line.unwind.map(Leave::Unwind),
                )
            })?;
        }
        Ok(())
    }

    /// A read of a packed array's element by a long key: past `nNumUsed`
    /// or a hole leaves for the interpreter to serve it again; an element
    /// that is a reference is read through it; one of the type the op's
    /// guard holds is read, and one of another copied to the slot the
    /// fetch writes as php's `ZVAL_COPY_DEREF` copies it, for the exit to
    /// go on past the fetch. A temporary array the fetch read is freed
    /// after, as its handler frees it, what a destructor throws leaving
    /// through the exit `types` gives the free.
    fn fetch(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("a fetch that cannot leave"))?;
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("a fetch from no array"));
        };
        let array_slot = self.value(array)?.slot;
        let Scalar::Long(key) = self.scalar(at, line.op2)? else {
            return Err(mismatch("a fetch by no long key"));
        };
        let out = line
            .out
            .ok_or_else(|| mismatch("a fetch that writes nothing"))?;
        let (room, array_at) = self.place(array_slot)?;
        let arr = self.array_of(array_slot, self.read_of(at, array))?;
        let element = self.element(arr, key, exit)?;
        let hole = self.b.ins().load(I8, trusted(), element, TYPE_AT);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, hole, tag(ZvalType::Undef));
        self.leave_if(hole, exit)?;
        match self.within(at).as_slice() {
            [] => {
                let element = self.deref(element);
                let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
                let out_slot = self.value(out)?.slot;
                self.copy(element, info, out_slot)?;
            }
            [in_place, through] => self.held_element([*in_place, *through], element, out)?,
            _ => {
                return Err(mismatch(
                    "a fetch holds other guards than its element's two",
                ));
            }
        }
        if array_slot.var >= self.frame_cvs(array_slot.frame)? {
            let [room_arg, opline] = self.served(line)?;
            // `FREE_OP1`, as the interpreter takes it: the array out of its
            // slot, which is left `UNDEF`, then its owner given up.
            let info = self.b.ins().load(I32, trusted(), room, array_at + TYPE_AT);
            let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
            self.b
                .ins()
                .store(trusted(), undef, room, array_at + TYPE_AT);
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            self.when(counted, |emit| {
                emit.call(
                    Kernel::Release,
                    &[room_arg, opline, arr, info],
                    line.unwind.map(Leave::Unwind),
                )
            })?;
        }
        Ok(())
    }

    /// A fetched element at `element` held to its type by its two guards,
    /// `[in place, through a reference]`: read in place, or through the
    /// reference it is, each by a guard of its own, as php's two reads
    /// hold it, then written to what the fetch writes.
    fn held_element(
        &mut self,
        [in_place, through]: [Guard; 2],
        element: Value,
        out: ValueId,
    ) -> Result<(), Abort> {
        let (
            Check::Type { value, tag: t },
            Check::Deref {
                value: by,
                tag: t_by,
            },
        ) = (in_place.check, through.check)
        else {
            return Err(mismatch("a fetch holds guards of another kind"));
        };
        if value != out || by != out || t != t_by {
            return Err(mismatch("a fetch guards a value it does not write"));
        }
        let out_slot = self.value(out)?.slot;
        let byte = self.b.ins().load(I8, trusted(), element, TYPE_AT);
        let is_ref = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Reference));
        let referred = self.b.create_block();
        let direct = self.b.create_block();
        let read = self.b.create_block();
        let held = self.b.append_block_param(read, I64);
        self.b.ins().brif(is_ref, referred, &[], direct, &[]);
        self.b.switch_to_block(direct);
        self.b.seal_block(direct);
        self.element_guard(in_place, element, out, out_slot)?;
        self.b.ins().jump(read, &[element.into()]);
        self.b.switch_to_block(referred);
        self.b.seal_block(referred);
        let reference = self.b.ins().load(I64, trusted(), element, 0);
        let inner = self
            .b
            .ins()
            .iadd_imm_s(reference, i64::from(imm(Ref::VALUE_AT)));
        self.element_guard(through, inner, out, out_slot)?;
        self.b.ins().jump(read, &[inner.into()]);
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

    /// One guard of a fetched element at `element`: leave through its exit
    /// where the element is of another type than it holds, copying it to
    /// the slot the fetch writes first where the exit goes on past the
    /// fetch; go on otherwise.
    fn element_guard(
        &mut self,
        guard: Guard,
        element: Value,
        out: ValueId,
        out_slot: Slot,
    ) -> Result<(), Abort> {
        let (Check::Type { tag: t, .. } | Check::Deref { tag: t, .. }) = guard.check else {
            return Err(mismatch("a fetch holds a guard of another kind"));
        };
        let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        let kind = self.b.ins().band_imm_u(info, KIND);
        let other = self.b.ins().icmp_imm_s(IntCC::NotEqual, kind, tag(t));
        let other = self.fuzzed(other)?;
        let goes_on = self
            .ir
            .exits
            .get(usize::from(guard.exit.0))
            .is_some_and(|spec| {
                spec.stores
                    .iter()
                    .any(|store| matches!(store.from, Stored::Copied { value, .. } if value == out))
            });
        let failed = self.b.create_block();
        self.b.set_cold_block(failed);
        let on = self.b.create_block();
        self.b.ins().brif(other, failed, &[], on, &[]);
        self.b.switch_to_block(failed);
        self.b.seal_block(failed);
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
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// A write of a scalar to an array's element by a long key, php's
    /// `zend_jit_assign_dim` (`Z/ext/opcache/jit/zend_jit_ir.c:13352-13526`):
    /// in place where the array is counted and held once, so php's
    /// `SEPARATE_ARRAY` has nothing to do, packed, the key below
    /// `nNumUsed` and the element there and counting nothing, so nothing
    /// is freed (`zend_jit_fetch_dimension_address_inner`'s `BP_VAR_W`
    /// path, `:11953-12276`); through the handler otherwise, where php's
    /// code calls its helpers, the array's copy, the hash's lookup or
    /// insert and the old value's release, so the write takes no exit.
    ///
    /// An immutable array, one shared, a key past `nNumUsed`, a counted
    /// element, a hole and an array turned hash are each served by the
    /// handler, in place of the write, and the loop goes on in the trace:
    ///
    /// ```php
    /// class D { function __destruct() { echo "freed\n"; } }
    /// function w($n) {
    ///     $a = [0, 0, 0, 0]; $b = null;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if ($i == 50) { $b = $a; }
    ///         if ($i == 60) { $a[1] = new D; }
    ///         if ($i == 70) { unset($a[2]); }
    ///         if ($i == 80) { $a[1000] = -1; }
    ///         $a[$i % 6] = $i;
    ///     }
    ///     foreach ($a as $k => $v) { echo "$k=$v "; }
    ///     echo count($b), "\n";
    /// }
    /// w(100);
    /// ```
    /// ```expect regime=jit-force process
    /// freed
    /// 0=96 1=97 3=99 4=94 5=95 2=98 1000=-1 6
    /// ```
    ///
    /// Under exit-fuzz the handler serves the write once in each trace, in
    /// place of an exit:
    ///
    /// ```php
    /// function s($n) {
    ///     $a = []; for ($i = 0; $i < $n; $i++) { $a[] = true; }
    ///     for ($i = 2; $i < $n; $i++) {
    ///         if ($a[$i]) { for ($j = $i + $i; $j < $n; $j += $i) { $a[$j] = false; } }
    ///     }
    ///     $c = 0; foreach ($a as $p) { if ($p) { $c++; } }
    ///     return $c;
    /// }
    /// echo s(1000), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 170
    /// ```
    fn assign_dim(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element write to no array"));
        };
        let Scalar::Long(key) = self.scalar(at, line.op2)? else {
            return Err(mismatch("an element write by no long key"));
        };
        let value = self.scalar(at, line.data)?;
        let held = self.value(array)?;
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let (room, array_at) = self.place(held.slot)?;
        let hash = held.types.meets(Types::HASH);
        let (element, old) = self.written_in_place((room, array_at), key, hash, handled);
        let counts = self.b.ins().band_imm_u(old, COUNTED);
        let counts = self.fuzzed(counts)?;
        self.divert_if(counts, handled);
        self.write(element, 0, value);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(handled);
        self.b.seal_block(handled);
        self.stepped(at, line)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        if let Some(out) = line.out {
            self.define(out, value)?;
        }
        Ok(())
    }

    /// The element `key` names of the array the zval at `base + at`
    /// holds, and the element's type word, where php's code writes it in
    /// place: the array counted and held once, so `SEPARATE_ARRAY` has
    /// nothing to do, packed where it may be a hash (`hash`), the key below
    /// `nNumUsed` and the element there (`zend_jit_fetch_dimension_address_inner`'s
    /// `BP_VAR_W` and `BP_VAR_RW` paths, `Z/ext/opcache/jit/zend_jit_ir.c:11953-12276`);
    /// to `handled` otherwise, where php's code calls its helpers.
    fn written_in_place(
        &mut self,
        (base, at): (Value, i32),
        key: Value,
        hash: bool,
        handled: Block,
    ) -> (Value, Value) {
        let info = self.b.ins().load(I32, trusted(), base, at + TYPE_AT);
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        let immutable = self.b.ins().icmp_imm_s(IntCC::Equal, counted, 0);
        self.divert_if(immutable, handled);
        let arr = self.b.ins().load(I64, trusted(), base, at);
        let count = self
            .b
            .ins()
            .load(I32, trusted(), arr, imm(Arr::REFCOUNT_AT));
        let shared = self.b.ins().icmp_imm_s(IntCC::NotEqual, count, 1);
        self.divert_if(shared, handled);
        if hash {
            let flags = self.b.ins().load(I32, trusted(), arr, imm(Arr::FLAGS_AT));
            let packed = self.b.ins().band_imm_u(flags, i64::from(Arr::PACKED));
            let hash = self.b.ins().icmp_imm_s(IntCC::Equal, packed, 0);
            self.divert_if(hash, handled);
        }
        let past = self.past_used(arr, key);
        self.divert_if(past, handled);
        let element = self.slot_at(arr, key);
        let old = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        let kind = self.b.ins().band_imm_u(old, KIND);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
        self.divert_if(hole, handled);
        (element, old)
    }

    /// A compound write of a number to an array's element by a long key,
    /// the array a variable's or the one the reference it holds refers
    /// to, php's `zend_jit_assign_dim_op` (`Z/ext/opcache/jit/zend_jit_ir.c:13528-13750`):
    /// in place where the array can be written in place
    /// ([`Emit::written_in_place`]) and the element is a long or a
    /// double, which php's `zend_jit_math_helper` serves inline, a long
    /// overflowing to a double as php's does; through the handler
    /// otherwise, where php's code calls its helpers (the array's copy, the
    /// hash's lookup, `zend_binary_op` over anything else), so the write
    /// takes no exit, as php's takes none where the record holds no type
    /// of the element.
    ///
    /// By-reference arrays of floats updated in a loop, as nbody updates
    /// them, an element a long that overflows, one a string, a shared
    /// array, a hole and an array turned hash each served by the handler
    /// in turn:
    ///
    /// ```php
    /// function step(array &$v, array &$w, $n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $k = $i % 4;
    ///         $v[$k] -= 0.5 * $k; $w[$k] += $k; $w[$k] *= 3;
    ///     }
    /// }
    /// $v = [1.0, 2.0, 3.0, 4.0]; $w = [0, 1, PHP_INT_MAX, 3];
    /// step($v, $w, 8);
    /// $w[1] = "7"; $copy = $v; step($v, $w, 8);
    /// unset($v[2]); step($v, $w, 4);
    /// $v["x"] = 1; step($v, $w, 4);
    /// var_dump($v, $w, $copy[1]);
    /// ```
    /// ```expect regime=jit-force process
    /// Warning: Undefined array key 2 in %s on line 5
    /// array(5) {
    ///   [0]=>
    ///   float(1)
    ///   [1]=>
    ///   float(-1)
    ///   [3]=>
    ///   float(-5)
    ///   [2]=>
    ///   float(-2)
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// array(4) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(687)
    ///   [2]=>
    ///   float(6.723838214867132E+21)
    ///   [3]=>
    ///   int(5463)
    /// }
    /// float(1)
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function step(array &$v, array &$w, $n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $k = $i % 4;
    ///         $v[$k] -= 0.5 * $k; $w[$k] += $k; $w[$k] *= 3;
    ///     }
    /// }
    /// $v = [1.0, 2.0, 3.0, 4.0]; $w = [0, 1, PHP_INT_MAX, 3];
    /// step($v, $w, 8);
    /// $w[1] = "7"; $copy = $v; step($v, $w, 8);
    /// unset($v[2]); step($v, $w, 4);
    /// $v["x"] = 1; step($v, $w, 4);
    /// var_dump($v, $w, $copy[1]);
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// Warning: Undefined array key 2 in %s on line 5
    /// array(5) {
    ///   [0]=>
    ///   float(1)
    ///   [1]=>
    ///   float(-1)
    ///   [3]=>
    ///   float(-5)
    ///   [2]=>
    ///   float(-2)
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// array(4) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(687)
    ///   [2]=>
    ///   float(6.723838214867132E+21)
    ///   [3]=>
    ///   int(5463)
    /// }
    /// float(1)
    /// ```
    fn assign_dim_op(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element's compound write to no array"));
        };
        let Imm::Binary(binary) = line.imm else {
            return Err(mismatch("an ASSIGN_DIM_OP with no operator"));
        };
        let Scalar::Long(key) = self.scalar(at, line.op2)? else {
            return Err(mismatch("an element's compound write by no long key"));
        };
        let value = self.scalar(at, line.data)?;
        let held = self.value(array)?;
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let (room, array_at) = self.place(held.slot)?;
        // What a reference holds is of either layout; a variable's array is
        // of the one its layout guard held, where one did.
        let (zval, hash) = match self.read_of(at, array) {
            Read::InPlace => ((room, array_at), held.types.meets(Types::HASH)),
            Read::Through => {
                let reference = self.b.ins().load(I64, trusted(), room, array_at);
                ((reference, imm(Ref::VALUE_AT)), true)
            }
        };
        let (element, old) = self.written_in_place(zval, key, hash, handled);
        let kind = self.b.ins().band_imm_u(old, KIND);
        let is_long = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Long));
        let is_double = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Double));
        let long = self.b.create_block();
        let not_long = self.b.create_block();
        self.b.ins().brif(is_long, long, &[], not_long, &[]);
        self.b.switch_to_block(not_long);
        self.b.seal_block(not_long);
        let double = self.b.create_block();
        self.b.ins().brif(is_double, double, &[], handled, &[]);
        for (block, kind) in [(long, Kind::Long), (double, Kind::Double)] {
            self.b.switch_to_block(block);
            self.b.seal_block(block);
            let x = self.read(element, 0, kind);
            let num = self.binary(binary, x, value, None)?;
            self.write_num(element, 0, num);
            self.b.ins().jump(done, &[]);
        }
        self.b.switch_to_block(handled);
        self.b.seal_block(handled);
        self.stepped(at, line)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }

    /// Go to `to` where `cond` is not zero, a block whose branches the
    /// caller seals once every one is made; go on otherwise.
    fn divert_if(&mut self, cond: Value, to: Block) {
        let on = self.b.create_block();
        self.b.ins().brif(cond, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
    }

    /// The address of the slot `key` names in the packed array `arr`,
    /// leaving through `exit` where the key is past `nNumUsed`.
    fn element(&mut self, arr: Value, key: Value, exit: ExitId) -> Result<Value, Abort> {
        let past = self.past_used(arr, key);
        self.leave_if(past, exit)?;
        Ok(self.slot_at(arr, key))
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
        let (room, at) = self.place(slot)?;
        let held = self.b.ins().load(I64, trusted(), room, at);
        Ok(match read {
            Read::InPlace => held,
            Read::Through => self.b.ins().load(I64, trusted(), held, imm(Ref::VALUE_AT)),
        })
    }

    /// How the op at `at` reads `value`: through the reference its slot
    /// holds where a guard before the op held one, in place otherwise.
    fn read_of(&self, at: OpId, value: ValueId) -> Read {
        let through = self.ir.guards.iter().any(|g| {
            g.at == GuardAt::Before(at)
                && matches!(g.check, Check::Deref { value: v, .. } if v == value)
        });
        if through {
            Read::Through
        } else {
            Read::InPlace
        }
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
        let word = self.b.ins().load(I64, trusted(), zval, 0);
        let (room, at) = self.place(slot)?;
        self.b.ins().store(trusted(), word, room, at);
        self.b.ins().store(trusted(), info, room, at + TYPE_AT);
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
        if from.var >= self.frame_cvs(from.frame)? {
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
        self.release(line, old_word, old_info)?;
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

    /// What a slot held, `word` and its type word `info`, given up after
    /// an inline write over it, as `zval_ptr_dtor` gives it up: through
    /// the release kernel where it is counted, a destructor's throw
    /// leaving through the opline's unwind.
    fn release(&mut self, line: &Line, word: Value, info: Value) -> Result<(), Abort> {
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        let [room, opline] = self.served(line)?;
        self.when(counted, |emit| {
            emit.call(
                Kernel::Release,
                &[room, opline, word, info],
                line.unwind.map(Leave::Unwind),
            )
        })
    }

    /// A send of a scalar: the call's slot for the argument written.
    fn send(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Imm::Arg(number) = line.imm else {
            return Err(mismatch("a send with no argument number"));
        };
        let arg = slot_offset(
            number
                .checked_sub(1)
                .ok_or_else(|| mismatch("an argument numbered zero"))?,
        )?;
        if let Operand::Value(value) = line.op1
            && Kind::of(self.value(value)?.types).is_none()
            && self.value(value)?.slot.var >= self.frame_cvs(line.frame)?
        {
            // What a temporary holds, moved into the call whole, its
            // owner with it, as `ZVAL_COPY_VALUE` moves it, and the slot
            // it leaves `UNDEF`, as the handler takes the operand.
            let (room, from) = self.place(self.value(value)?.slot)?;
            let word = self.b.ins().load(I64, trusted(), room, from);
            let info = self.b.ins().load(I32, trusted(), room, from + TYPE_AT);
            let call = self.call_room()?;
            self.b.ins().store(trusted(), word, call, arg);
            self.b.ins().store(trusted(), info, call, arg + TYPE_AT);
            let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
            self.b.ins().store(trusted(), undef, room, from + TYPE_AT);
            return Ok(());
        }
        let held = self.scalar(at, line.op1)?;
        let call = self.call_room()?;
        self.write(call, arg, held);
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
        if slot.var >= self.frame_cvs(slot.frame)? && Kind::of(self.value(value)?.types).is_none() {
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
    fn push_call(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
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
        let callee = self.b.ins().iconst(I32, i64::from(engine_offset(*callee)?));
        let [room, opline] = self.served(line)?;
        let func = self.func_of(line.frame)?;
        let stood = self.b.ins().stack_addr(I64, self.stood, 0);
        self.call(
            Kernel::PushCall,
            &[room, opline, func, callee, stood],
            Some(Leave::Exit(exit)),
        )?;
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
        raises: Option<ExitId>,
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
                if let Some(exit) = raises {
                    let zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                    self.leave_if(zero, exit)?;
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
                let (rx, ry) = (self.real(a)?, self.real(b)?);
                if let Some(exit) = raises {
                    let zero = self.b.ins().f64const(0.0);
                    let by_zero = self.b.ins().fcmp(FloatCC::Equal, ry, zero);
                    self.leave_if(by_zero, exit)?;
                }
                Ok(Num::Double(self.b.ins().fdiv(rx, ry)))
            }
            (Opcode::Mod, Scalar::Long(x), Scalar::Long(y)) => {
                if let Some(exit) = raises {
                    let zero = self.b.ins().icmp_imm_s(IntCC::Equal, y, 0);
                    self.leave_if(zero, exit)?;
                }
                // php answers zero for `% -1` rather than overflow.
                let minus_one = self.b.ins().icmp_imm_s(IntCC::Equal, y, -1);
                let one = self.b.ins().iconst(I64, 1);
                let divisor = self.b.ins().select(minus_one, one, y);
                Ok(Num::Long(self.b.ins().srem(x, divisor)))
            }
            (Opcode::Sl | Opcode::Sr, Scalar::Long(x), Scalar::Long(y)) => {
                if let Some(exit) = raises {
                    let negative = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, y, 0);
                    self.leave_if(negative, exit)?;
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

    fn value(&self, value: ValueId) -> Result<crate::types::Value, Abort> {
        self.ir
            .values
            .get(value.0 as usize)
            .copied()
            .ok_or_else(|| mismatch("a value past the trace's"))
    }

    /// The variables of `frame`, past which its slots are temporaries.
    fn frame_cvs(&self, frame: Frame) -> Result<u32, Abort> {
        self.ir
            .frames
            .get(usize::from(frame.0))
            .map(|info| info.cvs)
            .ok_or_else(|| mismatch("a frame past the trace's"))
    }

    /// The variable of `frame`'s room.
    fn room_var(&self, frame: Frame) -> Result<Variable, Abort> {
        self.rooms
            .get(usize::from(frame.0))
            .copied()
            .ok_or_else(|| mismatch("a frame past the trace's"))
    }

    /// The variable of `frame`'s place on the stack.
    fn ref_var(&self, frame: Frame) -> Result<Variable, Abort> {
        self.refs
            .get(usize::from(frame.0))
            .copied()
            .ok_or_else(|| mismatch("a frame past the trace's"))
    }

    /// `frame`'s place on the stack, a `FrameRef`'s number.
    fn frame_ref(&mut self, frame: Frame) -> Result<Value, Abort> {
        let var = self.ref_var(frame)?;
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

    /// What a value the trace keeps in a register holds.
    fn register(&mut self, value: ValueId) -> Result<Scalar, Abort> {
        let (var, kind) = self.register_var(value)?;
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
        let guarded = self.ir.guards.iter().find_map(|g| match g.check {
            Check::Type { value: v, tag: t } if v == value && g.at == GuardAt::Before(at) => {
                Some((t, false))
            }
            Check::Deref { value: v, tag: t } if v == value && g.at == GuardAt::Before(at) => {
                Some((t, true))
            }
            _ => None,
        });
        let (kind, through) = match guarded {
            Some((t, through)) => (Kind::of_tag(t), through),
            None => (Kind::of(held.types), false),
        };
        let kind =
            kind.ok_or_else(|| mismatch("an inline opline reads a value of no scalar kind"))?;
        self.load(held.slot, kind, through)
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
            | (Kind::Bool, Scalar::Bool(x)) => {
                self.b.def_var(var, x);
                Ok(())
            }
            (Kind::Null, Scalar::Null) | (Kind::False | Kind::True, Scalar::Bool(_)) => Ok(()),
            _ => Err(mismatch("a register written with another kind")),
        }
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
    fn write_num(&mut self, base: Value, at: i32, num: Num) {
        match num {
            Num::Long(x) => self.write(base, at, Scalar::Long(x)),
            Num::Double(x) => self.write(base, at, Scalar::Double(x)),
            Num::Either {
                overflowed,
                long,
                double,
            } => {
                let bits = self.b.ins().bitcast(I64, MemFlagsData::new(), double);
                let word = self.b.ins().select(overflowed, bits, long);
                let as_double = self.b.ins().iconst(I32, tag(ZvalType::Double));
                let as_long = self.b.ins().iconst(I32, tag(ZvalType::Long));
                let info = self.b.ins().select(overflowed, as_double, as_long);
                self.b.ins().store(trusted(), word, base, at);
                self.b.ins().store(trusted(), info, base, at + TYPE_AT);
            }
        }
    }
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

/// The engine offset that numbers an op array a trace holds; a
/// request-scoped one is never held (`jit-design.md` §4.1).
fn engine_offset(func: FuncRef) -> Result<u32, Abort> {
    super::engine_offset(func).ok_or_else(|| mismatch("a request-scoped op array in a trace"))
}
