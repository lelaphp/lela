//! A known callee's push, the call that makes it its frame and the
//! callee's leave, written in place over the engine's frame layout
//! (#3222's ruling, `jit-design.md` §5.3): php's `zend_jit_push_call_frame`,
//! `zend_jit_do_fcall` with `i_init_func_execute_data` inline and
//! `zend_jit_leave_func` (`Z/ext/opcache/jit/zend_jit_ir.c:8466-8650`,
//! `:10016-10600`, `:11035-11263`). Every offset is
//! [`php_engine::tier::FRAME`]'s, read here as Rust constants.
//!
//! Each writes what the engine's own push, call and leave write, as the
//! parity test in `enter`'s tests holds it, and the [`super::Stood`] the
//! kernel it stands in for would have written, so the code after it reads
//! where the interpreter stands as it reads it after a kernel. Each falls
//! back to that kernel where the engine's own path would refuse the plain
//! case: the page's end or a full frame table at the push; the interrupt
//! raised, or a callee's cache not yet laid down, at the call; and at the
//! leave a frame that holds a receiver, a closure, an annex or arguments
//! past its parameters, one a native's call made, one off the top page,
//! or a caller's slot that holds a counted value. None takes an exit of
//! its own: php's code takes none there.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I32, I64};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, Value};
use php_engine::tier::{FRAME, FrameLayout, PlainCall};
use php_heap::{Zval, ZvalType};

use super::{
    COUNTED, Emit, Kernel, STOOD_CALL, STOOD_FRAME, STOOD_FUNC, STOOD_OPLINE, STOOD_ROOM, Scalar,
    imm, leave_of, mismatch, trusted,
};
use crate::lower::{IMAGES_AT, INTERRUPT_AT, READY_AT, THREW};
use crate::types::{Abort, End, ExitId, Frame, Imm, Line, OpId, Operand, Owners, Returned, Vars};

/// What a leave in place writes into the caller's slot: a scalar, or a
/// zval's two words, copied from a constant or moved from a temporary.
#[derive(Clone, Copy)]
enum Out {
    Scalar(Scalar),
    Copied(Value, Value),
    Moved(Value, Value),
}

/// The engine's frame layout, as compiled.
const L: FrameLayout = FRAME;

/// A layout offset as the immediate a load or a store takes.
const fn at(word: u32) -> i32 {
    imm(word as usize)
}

/// Where a counted block keeps its count and its `GC_TYPE_INFO`.
const COUNT_AT: i32 = imm(Zval::COUNT_AT);
const GC_INFO_AT: i32 = imm(Zval::GC_INFO_AT);
/// A throw's status, as compiled code compares it.
const EXCEPTION: i64 = THREW as i64;

/// Where variable `var` of a frame stands past its room.
fn var_at(var: u32) -> Result<i32, Abort> {
    let slot = u64::from(L.room()) + u64::from(SLOT) * u64::from(var);
    i32::try_from(slot).map_err(|_| mismatch("a variable past an immediate"))
}

/// A slot, `sizeof(zval)`, the layout's.
const SLOT: u32 = L.slot();
const _: () = assert!(L.header_bytes().is_multiple_of(8));

/// Where the stack keeps its words.
pub(super) const TOP: i32 = at(L.top());
const END: i32 = at(L.end());
const FRAMES: i32 = at(L.frames());
pub(super) const FRAMES_LEN: i32 = at(L.frames_len());
const FRAMES_CAP: i32 = at(L.frames_cap());
const INNERMOST: i32 = at(L.innermost());

impl Emit<'_> {
    /// The request's VM stack, read at entry.
    pub(super) fn stack(&self) -> Result<Value, Abort> {
        self.stack
            .ok_or_else(|| mismatch("a call in place with no stack read"))
    }

    /// `where_` and `then` blocks for a path in place and its kernel's,
    /// the second cold.
    fn split(&mut self) -> (Block, Block) {
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        (slow, self.b.create_block())
    }

    /// The `Stood` a kernel would write, written by code in place.
    fn stand(&mut self, [frame, call, func, opline, room]: [Value; 5]) {
        for (value, at) in [
            (frame, STOOD_FRAME),
            (call, STOOD_CALL),
            (func, STOOD_FUNC),
            (opline, STOOD_OPLINE),
            (room, STOOD_ROOM),
        ] {
            self.b.ins().stack_store(I64, value, self.stood, at);
        }
    }

    /// The bytes the top page has left past `top`, `EG(vm_stack_end) - top`.
    pub(super) fn left_past(&mut self, stack: Value, top: Value) -> Value {
        let end = self.b.ins().load(I64, trusted(), stack, END);
        self.b.ins().isub(end, top)
    }

    /// Where the request's header for `plain` is read from: its place
    /// among the trace's calls in place, in bytes past [`IMAGES_AT`]'s
    /// address.
    fn header_of(&self, plain: &PlainCall) -> Result<i32, Abort> {
        let at = (self.held.iter())
            .position(|held| held == plain)
            .ok_or_else(|| mismatch("a call in place the trace does not hold"))?;
        // One address a call, `IMAGES_AT`'s table.
        i32::try_from(at * size_of::<usize>()).map_err(|_| mismatch("too many calls in place"))
    }

    /// Whether the request is not ready for the trace's calls in place,
    /// an `i8` ([`READY_AT`]).
    fn unready(&mut self) -> Result<Value, Abort> {
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a call in place before the entry"))?;
        let ready = self.b.ins().load(I32, trusted(), cx, READY_AT);
        Ok(self.b.ins().icmp_imm_s(IntCC::Equal, ready, 0))
    }

    /// An inline `INIT_*` whose callee needs no guard pushed in place, as
    /// `zend_jit_push_call_frame` pushes it and `push_call` lays it down:
    /// the room taken off the top of the stack's top page, its link the
    /// innermost room and the word moved to it, the header the push wrote
    /// copied whole, every slot `UNDEF`, and its record on the frame table.
    /// Where the page has less room than the frame takes or the table is
    /// full, the opline is stepped, which pushes the same call; php's own
    /// stack check, a guard before the push, leaves first where the page
    /// is short of what the trace pushes ([`crate::types::Check::Room`]).
    pub(super) fn push_in_place(&mut self, line: &Line, plain: &PlainCall) -> Result<(), Abort> {
        let stack = self.stack()?;
        let header = self.header_of(plain)?;
        let unready = self.unready()?;
        let used = i64::from(L.room() + SLOT * plain.slots());
        let top = self.b.ins().load(I64, trusted(), stack, TOP);
        let left = self.left_past(stack, top);
        let short = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, left, used);
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let cap = self.b.ins().load(I64, trusted(), stack, FRAMES_CAP);
        let full = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, len, cap);
        let refused = self.b.ins().bor(short, full);
        let refused = self.b.ins().bor(refused, unready);
        let (slow, join) = self.split();
        self.divert_if(refused, slow);
        // The link, the room linked before this one, and the header.
        let below = self.b.ins().load(I64, trusted(), stack, INNERMOST);
        self.b.ins().store(trusted(), below, top, at(L.link()));
        let cx = self.cx.ok_or_else(|| mismatch("a push before the entry"))?;
        let images = self.b.ins().load(I64, trusted(), cx, IMAGES_AT);
        let header = self.b.ins().load(I64, trusted(), images, header);
        for word in (0..L.header_bytes()).step_by(8) {
            let value = self.b.ins().load(I64, trusted(), header, at(word));
            self.b
                .ins()
                .store(trusted(), value, top, at(L.header() + word));
        }
        let zero = self.b.ins().iconst(I64, 0);
        for slot in 0..plain.slots() {
            let cell = L.room() + SLOT * slot;
            self.b.ins().store(trusted(), zero, top, at(cell));
            self.b.ins().store(trusted(), zero, top, at(cell + 8));
        }
        self.b.ins().store(trusted(), top, stack, INNERMOST);
        // The record, then the table's length and the top.
        let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
        let offset = self.b.ins().imul_imm_s(len, i64::from(L.record()));
        let record = self.b.ins().iadd(frames, offset);
        let first = self.b.ins().iadd_imm_s(top, i64::from(L.room()));
        self.b
            .ins()
            .store(trusted(), first, record, at(L.record_at()));
        let slots = self.b.ins().iconst(I64, i64::from(plain.slots()));
        self.b
            .ins()
            .store(trusted(), slots, record, at(L.record_len()));
        let tag = self.b.ins().iconst(I8, i64::from(L.on_page()));
        self.b
            .ins()
            .store(trusted(), tag, record, at(L.record_placed()));
        let grown = self.b.ins().iadd_imm_s(len, 1);
        self.b.ins().store(trusted(), grown, stack, FRAMES_LEN);
        let past = self.b.ins().iadd_imm_s(top, used);
        self.b.ins().store(trusted(), past, stack, TOP);
        // Where the interpreter stands after the `INIT_*`: on past it in
        // the frame, the call pushed under way.
        let room = self.room(line.frame)?;
        let func = self.func_of(line.frame)?;
        let next = self.b.ins().iconst(I32, i64::from(line.opline.0 + 1));
        let left = self.left_past(stack, past);
        self.stand([room, top, func, next, left]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let [room, opline] = self.served(line)?;
        let func = self.func_of(line.frame)?;
        let stood = self.b.ins().stack_addr(I64, self.stood, 0);
        self.call(Kernel::Step, &[room, opline, func, stood], leave_of(line))?;
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        Ok(())
    }

    /// A `DO_*CALL` of a call pushed in place made its callee's frame in
    /// place, as `zend_jit_do_fcall` makes it with
    /// `i_init_func_execute_data` inline and `Request::enter_call` writes
    /// it: the flags, the opline past the prologue, the callee's own cache,
    /// the slot of the caller's that takes the return and the caller
    /// written over the pushed header, and the caller stood at the call,
    /// php's `SET_EX_OPLINE`. Where the interrupt is raised, which the
    /// callee's activation tests, or the callee's cache is not laid down,
    /// [`Kernel::Call`] makes the call; where the callee serves a prologue
    /// it does on every pass.
    pub(super) fn make_in_place(
        &mut self,
        id: OpId,
        line: &Line,
        returns: Option<u32>,
    ) -> Result<(), Abort> {
        let (call, plain) = *self
            .calls
            .last()
            .ok_or_else(|| mismatch("a call made with no call under way"))?;
        let callee = self
            .entered_by(id, line)
            .ok_or_else(|| mismatch("a call made in place the trace does not follow"))?;
        let (Some(activation), super::CallRoom::Held(var)) =
            (plain.and_then(PlainCall::activation), call)
        else {
            return self.stepped(id, line);
        };
        let stack = self.stack()?;
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a call made before the entry"))?;
        let interrupt = self.b.ins().load(I64, trusted(), cx, INTERRUPT_AT);
        let raised = self.b.ins().load(I8, trusted(), interrupt, 0);
        let raised = self.b.ins().icmp_imm_s(IntCC::NotEqual, raised, 0);
        let unready = self.unready()?;
        let refused = self.b.ins().bor(raised, unready);
        let (slow, join) = self.split();
        self.divert_if(refused, slow);
        let room = self.b.use_var(var);
        let flags = self.b.ins().load(I32, trusted(), room, at(L.info()));
        let kept = self.b.ins().band_imm_u(flags, i64::from(activation.keep()));
        let set = i64::from(activation.info(returns.is_some()));
        let flags = self.b.ins().bor_imm_u(kept, set);
        self.b.ins().store(trusted(), flags, room, at(L.info()));
        let words = [
            (L.opline(), activation.opline()),
            (L.cache(), activation.cache()),
            (
                L.returns_to(),
                returns.map_or(0, |slot| slot + L.numbered_from()),
            ),
        ];
        for (field, word) in words {
            let word = self.b.ins().iconst(I32, i64::from(word));
            self.b.ins().store(trusted(), word, room, at(field));
        }
        let place = self.frame_ref(line.frame)?;
        let place = self.b.ins().ireduce(I32, place);
        let named = self.b.ins().iadd_imm_s(place, i64::from(L.numbered_from()));
        self.b.ins().store(trusted(), named, room, at(L.caller()));
        let from = self.room(line.frame)?;
        let opline = self.b.ins().iconst(I32, i64::from(line.opline.0));
        self.b.ins().store(trusted(), opline, from, at(L.opline()));
        // Where the interpreter stands: in the callee, at its entry.
        let none = self.b.ins().iconst(I64, 0);
        let func = self.func_of(callee)?;
        let entry = self.b.ins().iconst(I32, i64::from(activation.opline()));
        let top = self.b.ins().load(I64, trusted(), stack, TOP);
        let left = self.left_past(stack, top);
        self.stand([room, none, func, entry, left]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        self.step_kernel(id, line)?;
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        self.after_step(id, line)
    }

    /// Whether a variable of `frame`, whose room is `room`, owns
    /// something: its type word counted, every variable's folded into one
    /// test, as `Request::leaves_plainly` folds them. What a leave over
    /// more variables than it frees one by one ([`Owners::Kernel`]) asks.
    fn owners(&mut self, room: Value, frame: Frame) -> Result<Value, Abort> {
        let cvs = (self.ir.frames.get(usize::from(frame.0)))
            .ok_or_else(|| mismatch("a leave in a frame the trace does not hold"))?
            .cvs;
        let mut words = self.b.ins().iconst(I32, 0);
        for var in 0..cvs {
            let word = self
                .b
                .ins()
                .load(I32, trusted(), room, var_at(var)? + super::TYPE_AT);
            words = self.b.ins().bor(words, word);
        }
        let counted = self.b.ins().band_imm_u(words, COUNTED);
        Ok(self.b.ins().icmp_imm_s(IntCC::NotEqual, counted, 0))
    }

    /// The place of the frame `line`'s `RETURN` leaves to, read off its
    /// header's caller before it leaves, for the `Op::Back` after it.
    pub(super) fn returns_to(&mut self, line: &Line) -> Result<Option<Value>, Abort> {
        if self.stack.is_none() {
            return Ok(None);
        }
        let room = self.room(line.frame)?;
        let caller = self.b.ins().load(I32, trusted(), room, at(L.caller()));
        let caller = self.b.ins().uextend(I64, caller);
        let place = self
            .b
            .ins()
            .iadd_imm_s(caller, -i64::from(L.numbered_from()));
        let var = self.b.declare_var(I64);
        self.b.def_var(var, place);
        self.returned_to = Some(var);
        Ok(Some(caller))
    }

    /// A function's `RETURN` that leaves its frame in place, as
    /// `zend_jit_leave_func` leaves it and `Request::leave_plainly` leaves
    /// a frame `leaves_plainly` finds: the value into the caller's slot
    /// where the call reads it, the frame's record off the table, the word
    /// moved back to its link and the top back to its room, and the caller
    /// on past its call. `types` holds its value a scalar, a constant no
    /// reference counts or a temporary ([`Imm::Leave`], [`Returned`]);
    /// the frame's header and the caller's slot are read here, and where
    /// the leave kernel would serve the frame, the opline is stepped.
    /// Once nothing refuses it, the variables `types` does not hold
    /// uncounted are given up in place ([`Owners`], `Emit::free_cvs`)
    /// before the value is written and the frame popped, as php's trace
    /// frees them before `zend_jit_leave_func`; the order is the
    /// interpreter's owning leave's, the variables before the receiver
    /// and the closure, of which a frame left in place holds neither.
    ///
    /// A variable that owns something is given up as its frame leaves, its
    /// destructor called at the return, before the caller goes on:
    ///
    /// ```php
    /// class D { function __destruct() { echo "freed\n"; } }
    /// function f($i) { $d = $i == 30 ? new D : "s$i"; return $i; }
    /// $s = 0;
    /// for ($i = 0; $i < 40; $i++) { $s += f($i); }
    /// echo $s, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// freed
    /// 780
    /// ```
    ///
    /// and what that destructor throws unwinds from the return:
    ///
    /// ```php
    /// class T { function __destruct() { throw new Exception("at return"); } }
    /// function g($i) { $t = $i == 30 ? new T : null; return $i; }
    /// try {
    ///     for ($i = 0; $i < 40; $i++) { g($i); }
    /// } catch (Exception $e) { echo $e->getMessage(), " ", $i, "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// at return 30
    /// ```
    ///
    /// A trace that stops at a `RETURN` leaves its frame in place and goes
    /// on in the caller ([`End::Return`]); the interpreter serves the
    /// `RETURN` of a frame a native called, through the trace's end:
    ///
    /// ```php
    /// function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }
    /// function sq($x) { return $x * $x; }
    /// echo fib(20), " ", array_sum(array_map('sq', range(1, 100))), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 6765 338350
    /// ```
    pub(super) fn leave_in_place(&mut self, id: OpId, line: &Line) -> Result<(), Abort> {
        let Imm::Leave { owners, .. } = line.imm else {
            return Err(mismatch("a leave in place with no value named"));
        };
        let returned = self.returned(id, line)?;
        let stack = self.stack()?;
        let caller = self
            .returns_to(line)?
            .ok_or_else(|| mismatch("a leave in place with no caller read"))?;
        let room = self.room(line.frame)?;
        let (slow, join) = self.split();
        let refused = self.leave_refused(room, caller, line)?;
        self.divert_if(refused, slow);
        // The frame is the table's last record, on the top page.
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
        let last = self.b.ins().iadd_imm_s(len, -1);
        let refused = self.not_on_top(frames, last, room);
        self.divert_if(refused, slow);
        // The caller's place, and the slot of its that takes the return,
        // which holds nothing counted.
        let place = self
            .b
            .ins()
            .iadd_imm_s(caller, -i64::from(L.numbered_from()));
        let named = self.b.ins().load(I32, trusted(), room, at(L.returns_to()));
        let returns = self.b.ins().uextend(I64, named);
        let reads = self.b.ins().icmp_imm_s(IntCC::NotEqual, returns, 0);
        let (callers, target) = self.taken_at(frames, place, returns);
        let kind = self.b.ins().load(I32, trusted(), target, super::TYPE_AT);
        let counted = self.b.ins().band_imm_u(kind, COUNTED);
        let counted = self.b.ins().icmp_imm_s(IntCC::NotEqual, counted, 0);
        let counted = self.b.ins().band(reads, counted);
        self.divert_if(counted, slow);
        if let Out::Moved(_, info) = returned {
            // What the caller does not read is given up, which the leave
            // kernel does.
            let owned = self.b.ins().band_imm_u(info, COUNTED);
            let owned = self.b.ins().icmp_imm_s(IntCC::NotEqual, owned, 0);
            let unread = self.b.ins().icmp_imm_s(IntCC::Equal, returns, 0);
            let dropped = self.b.ins().band(owned, unread);
            self.divert_if(dropped, slow);
        }
        // Nothing refuses the leave from here: what the variables own is
        // given up, and a destructor that ran may have grown the frame
        // table, whose records are read again after it.
        let (threw, frames, callers, target) = match owners {
            Owners::Each(vars) => {
                let threw = self.free_cvs(room, line, vars)?;
                let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
                let (callers, target) = self.taken_at(frames, place, returns);
                (Some(threw), frames, callers, target)
            }
            Owners::None | Owners::Kernel => (None, frames, callers, target),
        };
        self.when(reads, |emit| {
            match returned {
                Out::Scalar(scalar) => emit.write(target, 0, scalar),
                Out::Copied(word, info) | Out::Moved(word, info) => {
                    emit.b.ins().store(trusted(), word, target, 0);
                    emit.b.ins().store(trusted(), info, target, super::TYPE_AT);
                }
            }
            Ok(())
        })?;
        // The record off the table, the word and the top back.
        self.b.ins().store(trusted(), last, stack, FRAMES_LEN);
        let below = self.b.ins().load(I64, trusted(), room, at(L.link()));
        self.b.ins().store(trusted(), below, stack, INNERMOST);
        self.b.ins().store(trusted(), room, stack, TOP);
        // The caller on past its call, `LOAD_NEXT_OPLINE()`; or, where a
        // destructor threw, at its call, where the throwable is raised,
        // `zend_rethrow_exception` with the return written, which
        // `HANDLE_EXCEPTION` frees as the call's result.
        let back = self.b.ins().iadd_imm_s(callers, -i64::from(L.room()));
        let at_call = self.b.ins().load(I32, trusted(), back, at(L.opline()));
        let mut past = self.b.ins().iadd_imm_s(at_call, 1);
        if let Some(threw) = threw {
            past = self.b.ins().select(threw, at_call, past);
        }
        self.b.ins().store(trusted(), past, back, at(L.opline()));
        // Where the interpreter stands: in the caller, the call under way
        // in it the record now on top where that is not the caller's own.
        let under = self.b.ins().iadd_imm_s(len, -2);
        let first = self.record_word(frames, under, L.record_at());
        let built = self.b.ins().iadd_imm_s(first, -i64::from(L.room()));
        let own = self.b.ins().icmp(IntCC::Equal, under, place);
        let nothing = self.b.ins().iconst(I64, 0);
        let call = self.b.ins().select(own, nothing, built);
        let func = self.b.ins().load(I32, trusted(), back, at(L.cache()));
        let left = self.left_past(stack, room);
        self.stand([back, call, func, past, left]);
        if let Some(threw) = threw {
            // php's `jit_stub_leave_throw` after `zend_jit_leave_func`: the
            // caller unwinds from its call.
            let unwind = line
                .unwind
                .ok_or_else(|| mismatch("a leave that frees with no unwind"))?;
            let to = self.unwind_block(unwind)?;
            self.b.ins().brif(threw, to, &[], join, &[]);
        } else {
            self.b.ins().jump(join, &[]);
        }
        self.left_or_not(id, line, (slow, join))
    }

    /// The caller's slots, by its record in the frame table at `frames`,
    /// and the slot of them a call that returns into `returns`, numbered
    /// past [`FrameLayout::numbered_from`], takes the value in.
    fn taken_at(&mut self, frames: Value, place: Value, returns: Value) -> (Value, Value) {
        let callers = self.record_word(frames, place, L.record_at());
        let taken = self
            .b
            .ins()
            .iadd_imm_s(returns, -i64::from(L.numbered_from()));
        let offset = self.b.ins().imul_imm_s(taken, i64::from(SLOT));
        (callers, self.b.ins().iadd(callers, offset))
    }

    /// php's `zend_jit_free_cv` over each of `vars`, the variables of the
    /// frame in `room` that `line`'s `RETURN` gives up, in slot order as
    /// `i_free_compiled_variables` gives them up, each owner given up as
    /// `jit_ZVAL_PTR_DTOR` gives it up
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:1819-1903`): a type test, then,
    /// where another owner stands and the collector holds the value already
    /// or never takes it (`GC_MAY_LEAK`'s bits, [`Zval::LEAK_BITS`]), a
    /// `GC_DELREF` inline; the last owner, a value the collector may take
    /// and a reference, whose value the collector looks through to, are
    /// [`Kernel::FreeCv`]'s, which takes the frame off the chain first, as
    /// `zend_jit_leave_frame` does before the first free. Each slot is left
    /// `UNDEF` before its owner goes, so the frame owns only what is still
    /// to be freed whatever a destructor does. Whether a destructor threw,
    /// an `i32`: the frees go on past a throw, as php's do, and a fatal
    /// or a panic unwinds.
    ///
    /// A destructor served at the leave sees the caller as the frame that
    /// runs, the leaving frame off the chain:
    ///
    /// ```php
    /// class D {
    ///     function __construct(public $n) {}
    ///     function __destruct() {
    ///         if ($this->n % 10 == 7) {
    ///             echo __LINE__, " ", $this->n;
    ///             foreach (debug_backtrace() as $f) { echo " ", $f['function'], "@", $f['line'] ?? '-'; }
    ///             echo "\n";
    ///         }
    ///     }
    /// }
    /// function f($i) { $d = new D($i); $s = "s$i"; return $i; }
    /// $t = 0;
    /// for ($i = 0; $i < 30; $i++) { $t += f($i); }
    /// echo $t, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 6 7 __destruct@14
    /// 6 17 __destruct@14
    /// 6 27 __destruct@14
    /// 435
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D {
    ///     function __construct(public $n) {}
    ///     function __destruct() {
    ///         if ($this->n % 10 == 7) {
    ///             echo __LINE__, " ", $this->n;
    ///             foreach (debug_backtrace() as $f) { echo " ", $f['function'], "@", $f['line'] ?? '-'; }
    ///             echo "\n";
    ///         }
    ///     }
    /// }
    /// function f($i) { $d = new D($i); $s = "s$i"; return $i; }
    /// $t = 0;
    /// for ($i = 0; $i < 30; $i++) { $t += f($i); }
    /// echo $t, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 6 7 __destruct@14
    /// 6 17 __destruct@14
    /// 6 27 __destruct@14
    /// 435
    /// ```
    ///
    /// A destructor that throws: the variable after it is freed all the
    /// same, the array returned is freed once, and the caller unwinds from
    /// its call:
    ///
    /// ```php
    /// class T { function __construct(public $n) {} function __destruct() { if ($this->n == 25) { throw new Exception("at leave {$this->n}"); } } }
    /// class U { function __destruct() { echo "u"; } }
    /// function g($i) { $t = new T($i); $u = new U; return [$i]; }
    /// $s = 0;
    /// for ($i = 0; $i < 30; $i++) {
    ///     try { $r = g($i); $s += $r[0]; } catch (Exception $e) { echo $e->getMessage(), " ", $i, "\n"; }
    /// }
    /// echo "\n", $s, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// uuuuuuuuuuuuuuuuuuuuuuuuuuat leave 25 25
    /// uuuu
    /// 410
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class T { function __construct(public $n) {} function __destruct() { if ($this->n == 25) { throw new Exception("at leave {$this->n}"); } } }
    /// class U { function __destruct() { echo "u"; } }
    /// function g($i) { $t = new T($i); $u = new U; return [$i]; }
    /// $s = 0;
    /// for ($i = 0; $i < 30; $i++) {
    ///     try { $r = g($i); $s += $r[0]; } catch (Exception $e) { echo $e->getMessage(), " ", $i, "\n"; }
    /// }
    /// echo "\n", $s, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// uuuuuuuuuuuuuuuuuuuuuuuuuuat leave 25 25
    /// uuuu
    /// 410
    /// ```
    ///
    /// Cycles left as possible roots, an array through a reference to
    /// itself and an object that holds itself, each counted by the
    /// collection after as php counts them:
    ///
    /// ```php
    /// function f($i) { $a = [$i]; $a[] = &$a; $b = [$i]; $o = new stdClass; $o->self = $o; return $i; }
    /// function g($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += f($i); } return $s; }
    /// gc_collect_cycles();
    /// echo g(50), " ";
    /// $s = gc_status();
    /// echo $s['roots'], " ", gc_collect_cycles(), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1225 100 100
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($i) { $a = [$i]; $a[] = &$a; $b = [$i]; $o = new stdClass; $o->self = $o; return $i; }
    /// function g($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += f($i); } return $s; }
    /// gc_collect_cycles();
    /// echo g(50), " ";
    /// $s = gc_status();
    /// echo $s['roots'], " ", gc_collect_cycles(), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 1225 100 100
    /// ```
    fn free_cvs(&mut self, room: Value, line: &Line, vars: Vars) -> Result<Value, Abort> {
        let unwind = line
            .unwind
            .ok_or_else(|| mismatch("a leave that frees with no unwind"))?;
        let threw = self.b.declare_var(I32);
        let none = self.b.ins().iconst(I32, 0);
        self.b.def_var(threw, none);
        let [_, opline] = self.served(line)?;
        for var in vars.iter() {
            let slot = var_at(var)?;
            let (owned, kernel, next) = (
                self.b.create_block(),
                self.b.create_block(),
                self.b.create_block(),
            );
            self.b.set_cold_block(kernel);
            let info = self
                .b
                .ins()
                .load(I32, trusted(), room, slot + super::TYPE_AT);
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            self.b.ins().brif(counted, owned, &[], next, &[]);
            self.b.switch_to_block(owned);
            self.b.seal_block(owned);
            let undef = self.b.ins().iconst(I32, super::tag(ZvalType::Undef));
            self.b
                .ins()
                .store(trusted(), undef, room, slot + super::TYPE_AT);
            let word = self.b.ins().load(I64, trusted(), room, slot);
            let count = self.b.ins().load(I32, trusted(), word, COUNT_AT);
            let owners = self.b.ins().band_imm_u(count, i64::from(Zval::COUNT_BITS));
            let last = self.b.ins().icmp_imm_u(IntCC::Equal, owners, 1);
            let kind = self.b.ins().band_imm_u(info, 0xff);
            let reference =
                self.b
                    .ins()
                    .icmp_imm_u(IntCC::Equal, kind, super::tag(ZvalType::Reference));
            let gc = self.b.ins().load(I32, trusted(), word, GC_INFO_AT);
            let held = self.b.ins().band_imm_u(gc, i64::from(Zval::LEAK_BITS));
            let leaks = self.b.ins().icmp_imm_u(IntCC::Equal, held, 0);
            let called = self.b.ins().bor(last, reference);
            let called = self.b.ins().bor(called, leaks);
            let inline = self.b.create_block();
            self.b.ins().brif(called, kernel, &[], inline, &[]);
            self.b.switch_to_block(inline);
            self.b.seal_block(inline);
            let fewer = self.b.ins().iadd_imm_s(count, -1);
            self.b.ins().store(trusted(), fewer, word, COUNT_AT);
            self.b.ins().jump(next, &[]);
            self.b.switch_to_block(kernel);
            self.b.seal_block(kernel);
            let status = self.call_status(Kernel::FreeCv, &[room, opline, word, info])?;
            let unwinds = self
                .b
                .ins()
                .icmp_imm_u(IntCC::UnsignedGreaterThan, status, EXCEPTION);
            let to = self.unwind_block(unwind)?;
            let on = self.b.create_block();
            self.b.ins().brif(unwinds, to, &[], on, &[]);
            self.b.switch_to_block(on);
            self.b.seal_block(on);
            let thrown = self.b.ins().icmp_imm_u(IntCC::Equal, status, EXCEPTION);
            let thrown = self.b.ins().uextend(I32, thrown);
            let before = self.b.use_var(threw);
            let now = self.b.ins().bor(before, thrown);
            self.b.def_var(threw, now);
            self.b.ins().jump(next, &[]);
            self.b.switch_to_block(next);
            self.b.seal_block(next);
        }
        Ok(self.b.use_var(threw))
    }

    /// What a `RETURN` left in place hands its caller ([`Returned`]): a
    /// scalar, `null` for none; a constant's two words, read from the op
    /// array the trace's epoch holds; or a temporary's, moved. A tree's
    /// leaves returned as a constant and its nodes as the arrays built,
    /// read back, and returns no caller reads, given up:
    ///
    /// ```php
    /// function leaf($d) { if ($d === 0) { return [null, null]; } $e = $d - 1; return [leaf($e), leaf($e)]; }
    /// function tally($t) { return $t[0] === null ? 1 : tally($t[0]) + tally($t[1]); }
    /// function drop($n) { for ($i = 0; $i < $n; $i++) { leaf(2); } return $i; }
    /// echo tally(leaf(10)), " ", drop(50), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1024 50
    /// ```
    fn returned(&mut self, id: OpId, line: &Line) -> Result<Out, Abort> {
        let Imm::Leave { value, .. } = line.imm else {
            return Err(mismatch("a leave in place with no value named"));
        };
        let (zval, at, moved) = match (value, line.op1) {
            (Returned::Scalar, Operand::Unused) => return Ok(Out::Scalar(Scalar::Null)),
            (Returned::Scalar, operand) => return Ok(Out::Scalar(self.scalar(id, operand)?)),
            (Returned::Constant(address), _) => {
                let address = i64::try_from(address)
                    .map_err(|_| mismatch("a constant past a 64-bit address"))?;
                (self.b.ins().iconst(I64, address), 0, false)
            }
            (Returned::Moved, Operand::Value(value)) => {
                let (room, at) = self.place(self.value(value)?.slot)?;
                (room, at, true)
            }
            (Returned::Moved, _) => return Err(mismatch("a temporary moved from no slot")),
        };
        let word = self.b.ins().load(I64, trusted(), zval, at);
        let info = self.b.ins().load(I32, trusted(), zval, at + super::TYPE_AT);
        Ok(if moved {
            Out::Moved(word, info)
        } else {
            Out::Copied(word, info)
        })
    }

    /// What the header of the frame in `room`, which leaves to `caller`,
    /// says the leave kernel serves: a receiver, a closure, an annex, a
    /// call not made a frame or arguments past the parameters, a caller
    /// that is none or the loop's, and, where the frame has more variables
    /// than the leave frees one by one ([`Owners::Kernel`]), a variable
    /// that owns something, every variable's type word tested at once.
    /// The interpreter's owning leave gives up the receiver and the
    /// closure after the variables; a frame that holds either is the leave
    /// kernel's, so the variables are the last of what a leave in place
    /// gives up.
    fn leave_refused(&mut self, room: Value, caller: Value, line: &Line) -> Result<Value, Abort> {
        let this = self.b.ins().load(I64, trusted(), room, at(L.this()));
        let closure = self.b.ins().load(I64, trusted(), room, at(L.closure()));
        let held = self.b.ins().bor(this, closure);
        let mut refused = self.b.ins().icmp_imm_s(IntCC::NotEqual, held, 0);
        let annex = self.b.ins().load(I32, trusted(), room, at(L.annex()));
        let info = self.b.ins().load(I32, trusted(), room, at(L.info()));
        let flagged = self
            .b
            .ins()
            .band_imm_u(info, i64::from(L.leaves_by_kernel()));
        let owns = self.b.ins().bor(annex, flagged);
        let owns = self.b.ins().icmp_imm_s(IntCC::NotEqual, owns, 0);
        refused = self.b.ins().bor(refused, owns);
        let none = self.b.ins().icmp_imm_s(IntCC::Equal, caller, 0);
        let looped = self.b.ins().band_imm_u(caller, i64::from(L.caller_loop()));
        let looped = self.b.ins().icmp_imm_s(IntCC::NotEqual, looped, 0);
        let outside = self.b.ins().bor(none, looped);
        refused = self.b.ins().bor(refused, outside);
        if let Imm::Leave {
            owners: Owners::Kernel,
            ..
        } = line.imm
        {
            let owners = self.owners(room, line.frame)?;
            refused = self.b.ins().bor(refused, owners);
        }
        Ok(refused)
    }

    /// Whether the record `last` of the frame table at `frames` is not the
    /// frame in `room`, or not on the top page.
    fn not_on_top(&mut self, frames: Value, last: Value, room: Value) -> Value {
        let offset = self.b.ins().imul_imm_s(last, i64::from(L.record()));
        let record = self.b.ins().iadd(frames, offset);
        let first = self.b.ins().load(I64, trusted(), record, at(L.record_at()));
        let own = self.b.ins().iadd_imm_s(room, i64::from(L.room()));
        let elsewhere = self.b.ins().icmp(IntCC::NotEqual, first, own);
        let tag = self
            .b
            .ins()
            .load(I8, trusted(), record, at(L.record_placed()));
        let off_page = self
            .b
            .ins()
            .icmp_imm_s(IntCC::NotEqual, tag, i64::from(L.on_page()));
        self.b.ins().bor(elsewhere, off_page)
    }

    /// The room of the call under way in `frame` as the frame table has
    /// it, as the interpreter's `EX(call)` reads after a call it made is
    /// popped: the record on top where it is not `frame`'s own, null where
    /// it is.
    pub(super) fn call_left(&mut self, frame: Frame) -> Result<Value, Abort> {
        let stack = self.stack()?;
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
        let last = self.b.ins().iadd_imm_s(len, -1);
        let first = self.record_word(frames, last, L.record_at());
        let top = self.b.ins().iadd_imm_s(first, -i64::from(L.room()));
        let room = self.room(frame)?;
        let own = self.b.ins().icmp(IntCC::Equal, top, room);
        let nothing = self.b.ins().iconst(I64, 0);
        Ok(self.b.ins().select(own, nothing, top))
    }

    /// The word at `field` of record `place` of the frame table at
    /// `frames`.
    fn record_word(&mut self, frames: Value, place: Value, field: u32) -> Value {
        let offset = self.b.ins().imul_imm_s(place, i64::from(L.record()));
        let record = self.b.ins().iadd(frames, offset);
        self.b.ins().load(I64, trusted(), record, at(field))
    }

    /// A leave's way out where it was not made in place, from `slow`, and
    /// on from `join`: the leave kernel, or, for the `RETURN` a trace stops
    /// at, its end exit, through which the interpreter serves it; and past
    /// that `RETURN` left in place, the interpreter goes on in the caller,
    /// php's `zend_jit_trace_return`, with no exit taken.
    fn left_or_not(
        &mut self,
        id: OpId,
        line: &Line,
        (slow, join): (Block, Block),
    ) -> Result<(), Abort> {
        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let at_end = self.returns_at_end(id);
        if let Some(exit) = at_end {
            let block = self.exit_block(exit)?;
            self.b.ins().jump(block, &[]);
        } else {
            self.step_kernel(id, line)?;
            self.b.ins().jump(join, &[]);
        }
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        if at_end.is_some() {
            let escape = self.escape_block();
            let place = self
                .returned_to
                .ok_or_else(|| mismatch("a leave in place with no caller read"))?;
            let place = self.b.use_var(place);
            self.b.ins().jump(escape, &[BlockArg::Value(place)]);
            Ok(())
        } else {
            self.after_step(id, line)
        }
    }

    /// The exit the trace takes where the `RETURN` at `id`, its last op,
    /// is not left in place ([`End::Return`]); none for any other.
    fn returns_at_end(&self, id: OpId) -> Option<ExitId> {
        match self.ir.end {
            End::Return(exit) if id.index() + 1 == self.ir.ops.len() => Some(exit),
            End::Return(_) | End::Leave(_) | End::Loop(_) => None,
        }
    }
}
