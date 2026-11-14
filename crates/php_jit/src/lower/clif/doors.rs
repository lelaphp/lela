//! A body's jumps to another trace (#3229's ruling, `jit-design.md` §4.3,
//! §4.6): a link, a jump in the body's own bytes to the trace it links
//! to, written before the body is published, as php's
//! `zend_jit_trace_link_to_root` writes it
//! (`Z/ext/opcache/jit/zend_jit_ir.c:17418-17432`); an exit's jump to the
//! side trace grown from it, where php patches the exit's jump
//! (`zend_jit_link_side_trace`, `:17413`); and an escape's jump to the
//! return root compiled at the landing it escaped to, as php's escape
//! stub runs that opline's handler (`jit-design.md` §4.9).
//!
//! Each is one tail call through its [`Door`], made by [`Emit::hand_on`]
//! alone: the door's words read in the request's addresses of the trace,
//! which name the body of the trace it enters and the request's addresses
//! of that trace, written by the crossing once it entered that trace from
//! the door's answer with its calls ready, and, for an escape, the landing
//! they were written for, held to where the frame stands; then the entry
//! passed in registers, where the exit leaves the frame, with the frame
//! stood at the exit's opline as `enter`'s landing stands it. Where an
//! exit's door is not written, the exit's number is answered, with nothing
//! stored twice, and `dispatch` goes on from there as it did before a jump
//! existed: the exit is landed, and the side trace, or the trace the link
//! names, entered through the crossing, which writes the door for the next
//! jump. Where an escape's door is not written, or names another landing,
//! the escape reads the landing's word ([`Emit::landed`]), enters a trace
//! compiled there through its entrance, and leaves its number in that
//! trace's row, from which the next entry through `enter` writes the door;
//! a word that reads anything else answers [`ESCAPED`].
//!
//! An exit jumps only where its entry can be passed from code: an exit
//! that grows side traces, names its opline and lands plainly or with a
//! call under way, which frees nothing, and is not the trace's end. Every
//! other is answered as before.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, Value};
use php_engine::tier::{LANDING_CHUNK_BITS, LandingIndex, LandingRows, Word};

use super::{
    CallRoom, EX_OPLINE, Emit, Raised, STOOD_CALL, STOOD_FRAME, STOOD_FUNC, STOOD_OPLINE,
    body_signature, engine_offset, imm, mismatch, trusted,
};
use crate::lower::{
    DOOR_BODY, DOOR_IMAGES, DOOR_KEY, DOOR_WORDS, Door, ENTRANCE_BODY, ENTRANCE_TRACE, ESCAPED,
    Escapes, Linked, ROOT_AT, ROW_BYTES, ROW_ESCAPED_BY, ROW_IMAGES, ROW_READY, ROW_SEGMENT,
    ROWS_AT, Stands,
};
use crate::policy::{COMPILED, ID_SHIFT, STATE};
use crate::types::{Abort, End, ExitId, Grows};

/// Where the frame a jump enters on stands as the trace leaves it, which
/// the jump passes as the entry: its room and place, the call under way
/// in it, its op array and opline as one word, as a `Stood` holds them,
/// and the opline to stand the frame at where the jump leaves through an
/// exit, which the frame does not stand at yet.
#[derive(Debug, Clone, Copy)]
struct Leaving {
    room: Value,
    place: Value,
    call: Value,
    stands: Value,
    stand: Option<Value>,
}

/// A trace's number's place in its segment of rows, as a mask.
const ROW_MASK: i32 = imm(ROW_SEGMENT - 1);

/// An address's width, as the shift that scales a number of them.
const ADDRESS_SHIFT: i64 = 3;
const _: () = assert!(size_of::<usize>() == 1 << ADDRESS_SHIFT);

/// A landing word's width, and where its state half stands in it, as
/// compiled code reads a landing's (`php_engine::tier::LandingIndex`).
const WORD_BYTES: i32 = imm(size_of::<Word>());
const WORD_STATE: i32 = imm(Word::STATE_AT);

/// The width of one op array's place in a chunk of the landing index:
/// the address of its words, and how many there are.
const LANDING_PLACE_SHIFT: i64 = ADDRESS_SHIFT + 1;
const LANDING_COUNT_AT: i32 = imm(size_of::<usize>());

impl Emit<'_> {
    /// Where the frame goes on as a trace is entered from `exit`, its op
    /// array by the engine's number and its opline, where the entry can be
    /// passed from code: the opline the exit names, or, where it names none, the
    /// opline a link's trace opens at, `linked`'s, where the link's frame
    /// stands already, in a frame of that op array; and a landing that
    /// frees nothing, as `enter`'s
    /// landing of it does nothing but stand the frame.
    fn goes_on_at(&self, exit: ExitId, linked: Option<Linked>) -> Option<Stands> {
        let spec = self.ir.exits.get(usize::from(exit.0))?;
        if spec.landing.frees.any() {
            return None;
        }
        if let Some(opline) = spec.opline {
            let frame = self.ir.frames.get(usize::from(spec.frame.0))?;
            return Some(Stands {
                func: engine_offset(frame.func),
                opline: opline.0,
            });
        }
        let opens = linked?.opens?;
        let running = self.ir.frames.get(usize::from(self.running.0))?;
        (engine_offset(running.func) == opens.func).then_some(opens)
    }

    /// Whether the entry of a trace entered from `exit` can be passed from
    /// code ([`Emit::goes_on_at`]).
    fn jumps(&self, exit: ExitId) -> bool {
        self.goes_on_at(exit, None).is_some()
    }

    /// Whether a guard that fails into `exit` jumps through its door: the
    /// exit jumps, and it grows side traces and is not the trace's end,
    /// which `dispatch` follows no side from.
    fn opens(&self, exit: ExitId) -> bool {
        let leaves = match self.ir.end {
            End::Leave(exit) | End::Return(exit) => Some(exit),
            End::Loop(_) => None,
        };
        leaves != Some(exit)
            && (self.ir.exits.get(usize::from(exit.0)))
                .is_some_and(|spec| spec.grows == Grows::Side)
            && self.jumps(exit)
    }

    /// The block a guard that fails leaves `exit` through, with what it
    /// hands the block: the exit's door, which takes the call under way at
    /// the guard, where the exit [`opens`](Emit::opens), and the exit's
    /// block otherwise.
    pub(super) fn guard_exit(&mut self, exit: ExitId) -> Result<(Block, Vec<BlockArg>), Abort> {
        if !self.opens(exit) || !self.runs_in(exit) {
            return Ok((self.exit_block(exit)?, Vec::new()));
        }
        let site = self.site()?;
        let at = usize::from(exit.0);
        let door = if let Some(door) = self.doors.get(at).copied().flatten() {
            door
        } else {
            let door = self.b.create_block();
            self.b.set_cold_block(door);
            for _ in &site {
                self.b.append_block_param(door, I64);
            }
            *self
                .doors
                .get_mut(at)
                .ok_or_else(|| mismatch("an exit past the trace's"))? = Some(door);
            door
        };
        Ok((door, site.map(BlockArg::Value).to_vec()))
    }

    /// Whether the code runs, where it leaves through `exit`, in a frame of
    /// the op array the exit names its opline in. An exit is one for every
    /// place that leaves with an equal spec, and a recursion's frames,
    /// each one frame on from the last, leave through the exits of the
    /// frame its header serves, so the frame an exit lands in is the one
    /// the code runs in, as `enter`'s landing finds it standing innermost,
    /// and not the frame the spec names.
    ///
    /// A recursion's direction exit, taken in a frame one or two calls
    /// deep, enters the side trace grown from it on that frame, so the
    /// leaf's `return $n` returns its own `$n`:
    ///
    /// ```php
    /// function fib(int $n): int { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }
    /// echo fib(8), " ", fib(15), "\n";
    /// ```
    /// ```expect exit=0 regime=jit-force process
    /// 21 610
    /// ```
    fn runs_in(&self, exit: ExitId) -> bool {
        let func = |frame: crate::types::Frame| {
            (self.ir.frames.get(usize::from(frame.0))).map(|frame| frame.func)
        };
        (self.ir.exits.get(usize::from(exit.0))).is_some_and(|spec| {
            func(spec.frame).is_some() && func(spec.frame) == func(self.running)
        })
    }

    /// Where the code stands at a place that leaves: the call under way,
    /// the room of the frame it runs in and that frame's place, which an
    /// exit's door lays the entry from.
    fn site(&mut self) -> Result<[Value; 3], Abort> {
        let call = self.call_under_way();
        let room = self.room(self.running)?;
        let place = self.frame_ref(self.running)?;
        Ok([call, room, place])
    }

    /// The room of the innermost call under way, `EX(call)`, or zero where
    /// none is: what a kernel's `Stood` would say of it here.
    fn call_under_way(&mut self) -> Value {
        match self.calls.last() {
            Some((CallRoom::Held(var), _)) => {
                let var = *var;
                self.b.use_var(var)
            }
            Some((CallRoom::Stood, _)) => self.stood_word(I64, STOOD_CALL),
            None => self.b.ins().iconst(I64, 0),
        }
    }

    /// Every door a guard branched to: the exit's stores, then the jump
    /// through its [`Door`].
    pub(super) fn door_blocks(&mut self) -> Result<(), Abort> {
        let ir = self.ir;
        for at in 0..self.doors.len() {
            let Some(door) = self.doors[at] else {
                continue;
            };
            self.b.switch_to_block(door);
            self.b.seal_block(door);
            let [call, room, place] = *self.b.block_params(door) else {
                return Err(mismatch("a door that takes no site"));
            };
            let spec = ir
                .exits
                .get(at)
                .ok_or_else(|| mismatch("an exit past the trace's"))?;
            self.stores(&spec.stores)?;
            let exit = ExitId(u16::try_from(at).map_err(|_| mismatch("too many exits"))?);
            let closed = self.answer_block(exit)?;
            let at = (self.goes_on_at(exit, None))
                .ok_or_else(|| mismatch("a door from an exit that names no opline"))?;
            let leaving = self.exit_leaving(at, [call, room, place]);
            self.hand_on(leaving, Door::Exit(exit), false, closed)?;
        }
        Ok(())
    }

    /// The trace's end through `exit`: where it links to the trace the
    /// body was compiled against and the exit jumps, its stores and the
    /// link's jump; otherwise the exit's block.
    ///
    /// #3195's program: a loop that closes through a side trace linking
    /// to its own root. The side's jump lands at the root's first op, and
    /// php's side tests the interrupt before it jumps
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:7285-7303`), so the side's
    /// jump meets the timeout as php's does, at the loop's line:
    ///
    /// ```php
    /// set_time_limit(1);
    /// $s = 0;
    /// for ($i = 0; $i >= 0; $i++) {
    ///     if ($i % 2) {
    ///         $s += 1;
    ///     } else {
    ///         $s -= 1;
    ///     }
    /// }
    /// ```
    /// ```expect exit=255 regime=jit-inline process wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 4
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:2
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) $main() %s:4
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 4 $main() %s:6
    /// ---- TRACE 2 start (side trace 1/4) $main() %s:6
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    /// ```
    pub(super) fn leave_at_end(&mut self, exit: ExitId) -> Result<(), Abort> {
        let ir = self.ir;
        let spec = ir
            .exits
            .get(usize::from(exit.0))
            .ok_or_else(|| mismatch("an exit past the trace's"))?;
        match (self.joins.linked, spec.link) {
            (Some(linked), Some(link)) if linked.trace == link.get() && self.runs_in(exit) => {
                let Some(at) = self.goes_on_at(exit, Some(linked)) else {
                    let block = self.exit_block(exit)?;
                    self.b.ins().jump(block, &[]);
                    return Ok(());
                };
                let site = self.site()?;
                self.stores(&spec.stores)?;
                let closed = self.answer_block(exit)?;
                let leaving = self.exit_leaving(at, site);
                self.hand_on(leaving, Door::Exit(exit), linked.interrupt, closed)
            }
            _ => {
                let block = self.exit_block(exit)?;
                self.b.ins().jump(block, &[]);
                Ok(())
            }
        }
    }

    /// The block that answers `exit`'s number and stores nothing, the
    /// stores made already: where a jump finds no trace to enter.
    fn answer_block(&mut self, exit: ExitId) -> Result<Block, Abort> {
        let at = usize::from(exit.0);
        let slot = self
            .answers
            .get_mut(at)
            .ok_or_else(|| mismatch("an exit past the trace's"))?;
        if let Some(block) = *slot {
            return Ok(block);
        }
        let block = self.b.create_block();
        self.b.set_cold_block(block);
        self.answers[at] = Some(block);
        Ok(block)
    }

    /// The escape `block`, which takes the place of the frame the return
    /// left: php's escape stub, which runs the handler of the opline the
    /// frame stands at (`zend_jit_trace_escape_stub`), so the jump through
    /// the escape's [`Door`] where it names the landing the last `Stood`
    /// says the frame stands at; anywhere else, or where the door is
    /// closed, the landing's word read as php's stub reads the opline's
    /// handler ([`Emit::landed`]), and a trace compiled there entered
    /// through its entrance where the request's row of it is ready;
    /// [`ESCAPED`] answered where none is. No interrupt is tested, as
    /// php's escape stub tests none before the handler it jumps to; the
    /// trace entered tests it where php's does.
    ///
    /// One escape reaches two landings, the opline past each of `f`'s two
    /// calls: its door names one and the other's word is read, and each
    /// lands where the frame stands:
    ///
    /// ```php
    /// function f(int $n): int { if ($n < 2) { return $n; } return f($n - 1) + f($n - 2); }
    /// echo f(20), "\n";
    /// ```
    /// ```expect regime=jit-inline process capture
    /// 6765
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
    /// ---- TRACE 1 abort (deep recursion)
    ///
    /// ---- TRACE 1 start (enter) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 1 stop (recursive call)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 1 f() %SCRATCH%/witness.php:2
    /// ---- TRACE 2 start (side trace 1/1) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    ///
    /// ---- TRACE 3 start (return) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 3 stop (recursive return)
    /// ---- TRACE 3 compiled
    ///
    ///      TRACE 3 exit 2 f() %SCRATCH%/witness.php:2
    /// ---- TRACE 4 start (side trace 3/2) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 4 stop (link to 1)
    /// ---- TRACE 4 compiled
    /// ```
    pub(super) fn escaped(&mut self, block: Block) -> Result<(), Abort> {
        self.b.switch_to_block(block);
        self.b.seal_block(block);
        let place = *self
            .b
            .block_params(block)
            .first()
            .ok_or_else(|| mismatch("an escape that takes no place"))?;
        let (read, answer) = (self.b.create_block(), self.b.create_block());
        self.b.set_cold_block(read);
        self.b.set_cold_block(answer);
        let leaving = Leaving {
            room: self.stood_word(I64, STOOD_FRAME),
            place,
            call: self.stood_word(I64, STOOD_CALL),
            stands: self.stood_word(I64, STOOD_FUNC),
            stand: None,
        };
        self.hand_on(leaving, Door::Escape, false, read)?;
        self.b.switch_to_block(read);
        self.b.seal_block(read);
        if let Some(escapes) = self.joins.escapes {
            let entrance = self.landed(escapes, answer)?;
            self.enter_entrance(leaving, entrance, answer)?;
        } else {
            self.b.ins().jump(answer, &[]);
        }
        self.b.switch_to_block(answer);
        self.b.seal_block(answer);
        // The frame the interpreter goes on in, where `enter` reads it
        // back, as php's stub goes on from `execute_data` in hand.
        let cx = self
            .cx
            .ok_or_else(|| mismatch("an escape before the entry"))?;
        let root = self.b.ins().ireduce(I32, place);
        self.b.ins().store(trusted(), root, cx, ROOT_AT);
        let escaped = self.b.ins().iconst(I32, i64::from(ESCAPED));
        self.answer_with(escaped)
    }

    /// The entrance of the trace compiled at the landing the last `Stood`
    /// says the frame stands at (`lower::Escapes`): the op array's landing
    /// words found through the epoch's index by its offset, at the place
    /// `LandingIndex::place` says, the epoch's own stretch first and the
    /// preload's after, the word of
    /// the opline read, and where it reads compiled, the entrance of the
    /// trace it names read from the table's; `answer` where the op array
    /// is past the index or its words are not laid, where the opline is
    /// past them, where the word reads anything else, and where the table
    /// holds no entrance for it yet.
    fn landed(&mut self, escapes: Escapes, answer: Block) -> Result<Value, Abort> {
        let Escapes {
            landings,
            entrances,
            traces,
        } = escapes;
        let func = self.stood_word(I32, STOOD_FUNC);
        let opline = self.stood_word(I32, STOOD_OPLINE);
        let past = self.landing_place(func, landings, answer);
        let index = self.address(landings.chunks)?;
        let chunk = self.b.ins().ushr_imm_u(past, i64::from(LANDING_CHUNK_BITS));
        let chunk = self.address_at(index, chunk, answer);
        let within = (self.b.ins()).band_imm_u(past, (1_i64 << LANDING_CHUNK_BITS) - 1);
        let place = self.b.ins().ishl_imm_u(within, LANDING_PLACE_SHIFT);
        let place = self.b.ins().iadd(chunk, place);
        let words = self.b.ins().atomic_load(I64, trusted(), place);
        let none = self.b.ins().icmp_imm_u(IntCC::Equal, words, 0);
        self.cold_if(none, answer);
        let count = self.b.ins().load(I64, trusted(), place, LANDING_COUNT_AT);
        let opline = self.b.ins().uextend(I64, opline);
        let past_last = (self.b.ins()).icmp(IntCC::UnsignedGreaterThanOrEqual, opline, count);
        self.cold_if(past_last, answer);
        let word = self.b.ins().imul_imm_u(opline, i64::from(WORD_BYTES));
        let word = self.b.ins().iadd(words, word);
        let state = self.b.ins().load(I32, trusted(), word, WORD_STATE);
        let kind = self.b.ins().band_imm_u(state, i64::from(STATE));
        let other = (self.b.ins()).icmp_imm_u(IntCC::NotEqual, kind, i64::from(COMPILED));
        self.cold_if(other, answer);
        let id = self.b.ins().ushr_imm_u(state, i64::from(ID_SHIFT));
        let unheld =
            (self.b.ins()).icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, id, i64::from(traces));
        self.cold_if(unheld, answer);
        let id = self.b.ins().uextend(I64, id);
        let entrances = self.address(entrances)?;
        Ok(self.address_at(entrances, id, answer))
    }

    /// The place the op array at offset `func` is named at in `landings`,
    /// as `LandingIndex::place` says: in the epoch's own stretch where that
    /// names it, and in the preload's where that one does; on to `answer`
    /// past both.
    fn landing_place(&mut self, func: Value, landings: LandingIndex, answer: Block) -> Value {
        let (past, outside) = self.past_first(func, landings.own);
        let Some(preloaded) = landings.preloaded else {
            self.cold_if(outside, answer);
            return self.placed(past, landings.own);
        };
        let named = self.b.create_block();
        self.b.set_cold_block(named);
        let place = self.b.append_block_param(named, I64);
        let below = self.b.create_block();
        self.b.set_cold_block(below);
        let ours = self.placed(past, landings.own);
        self.b
            .ins()
            .brif(outside, below, &[], named, &[BlockArg::Value(ours)]);
        self.b.switch_to_block(below);
        self.b.seal_block(below);
        let (past, outside) = self.past_first(func, preloaded);
        self.cold_if(outside, answer);
        let theirs = self.placed(past, preloaded);
        self.b.ins().jump(named, &[BlockArg::Value(theirs)]);
        self.b.switch_to_block(named);
        self.b.seal_block(named);
        place
    }

    /// The offset `func` past the first of `rows`, and whether the stretch
    /// leaves it out.
    fn past_first(&mut self, func: Value, rows: LandingRows) -> (Value, Value) {
        let first = self.b.ins().iconst(I32, i64::from(rows.first));
        let past = self.b.ins().isub(func, first);
        let outside = (self.b.ins()).icmp_imm_u(
            IntCC::UnsignedGreaterThanOrEqual,
            past,
            i64::from(rows.held),
        );
        (past, outside)
    }

    /// The place `rows` names the op array `past` its first at: from the
    /// place the stretch starts at.
    fn placed(&mut self, past: Value, rows: LandingRows) -> Value {
        let past = self.b.ins().uextend(I64, past);
        match rows.place {
            0 => past,
            from => self.b.ins().iadd_imm_u(past, i64::from(from)),
        }
    }

    /// The constant `address`.
    fn address(&mut self, address: usize) -> Result<Value, Abort> {
        let address = i64::try_from(address).map_err(|_| mismatch("an address past the host's"))?;
        Ok(self.b.ins().iconst(I64, address))
    }

    /// The address written `at` addresses past `base`, loaded with
    /// acquire, as it was written with release; on to `answer` where it
    /// is zero, none written there yet.
    fn address_at(&mut self, base: Value, at: Value, answer: Block) -> Value {
        let at = self.b.ins().ishl_imm_u(at, ADDRESS_SHIFT);
        let at = self.b.ins().iadd(base, at);
        let address = self.b.ins().atomic_load(I64, trusted(), at);
        let none = self.b.ins().icmp_imm_u(IntCC::Equal, address, 0);
        self.cold_if(none, answer);
        address
    }

    /// Go to `to` where `cond` holds, and on in a cold block otherwise:
    /// a test on the way past the escape's door, which is cold as a whole.
    fn cold_if(&mut self, cond: Value, to: Block) {
        let on = self.b.create_block();
        self.b.set_cold_block(on);
        self.b.ins().brif(cond, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
    }

    /// Enter the trace `entrance` names (`lower::Entrance`) from where
    /// `leaving` says the frame stands: its body, with the request's
    /// addresses of it from its row (`enter::Rows`), which is told this
    /// trace came by for the crossing to open the escape's door; `answer`
    /// where the crossing has not entered it in the request with its calls
    /// ready.
    fn enter_entrance(
        &mut self,
        leaving: Leaving,
        entrance: Value,
        answer: Block,
    ) -> Result<(), Abort> {
        let cx = self.cx.ok_or_else(|| mismatch("a jump before the entry"))?;
        let body = self.b.ins().load(I64, trusted(), entrance, ENTRANCE_BODY);
        let trace = self.b.ins().load(I32, trusted(), entrance, ENTRANCE_TRACE);
        // The request's row of the trace: its segment, then the row in it.
        let rows = self.b.ins().load(I64, trusted(), cx, ROWS_AT);
        let number = self.b.ins().uextend(I64, trace);
        let segment = (self.b.ins()).ushr_imm_u(number, i64::from(ROW_SEGMENT.trailing_zeros()));
        let segment = self.address_at_plain(rows, segment);
        self.closed_unless(segment, answer);
        let within = self.b.ins().band_imm_u(number, i64::from(ROW_MASK));
        let within = self.b.ins().imul_imm_u(within, ROW_BYTES);
        let row = self.b.ins().iadd(segment, within);
        let ready = self.b.ins().uload8(I32, trusted(), row, ROW_READY);
        self.closed_unless(ready, answer);
        let images = self.b.ins().load(I64, trusted(), row, ROW_IMAGES);
        let number = self.number()?;
        self.b.ins().store(trusted(), number, row, ROW_ESCAPED_BY);
        self.tail_call(leaving, body, images)
    }

    /// The address written `at` addresses past `base`, in the request's
    /// own memory, so a plain load.
    fn address_at_plain(&mut self, base: Value, at: Value) -> Value {
        let at = self.b.ins().ishl_imm_u(at, ADDRESS_SHIFT);
        let at = self.b.ins().iadd(base, at);
        self.b.ins().load(I64, trusted(), at, 0)
    }

    /// Where `door`'s words stand in the request's addresses of the
    /// trace, in bytes: past its calls' headers and its cache slots, and
    /// past the doors made before it, the door numbered the first time it
    /// is asked.
    fn door_words(&mut self, door: Door) -> Result<i32, Abort> {
        let index = if let Some(index) = self.made.iter().position(|made| *made == door) {
            index
        } else {
            self.made.push(door);
            self.made.len() - 1
        };
        index
            .checked_mul(DOOR_WORDS)
            .and_then(|words| words.checked_add(self.held.len()))
            .and_then(|words| words.checked_add(self.ir.map_ptrs.len()))
            .and_then(|words| words.checked_mul(size_of::<usize>()))
            .and_then(|bytes| i32::try_from(bytes).ok())
            .ok_or_else(|| mismatch("a door past an immediate"))
    }

    /// Go to `closed` where `value` is zero; go on otherwise.
    fn closed_unless(&mut self, value: Value, closed: Block) {
        let zero = self.b.ins().icmp_imm_s(IntCC::Equal, value, 0);
        self.divert_if(zero, closed);
    }

    /// Where an exit leaves the frame (`enter::land`), from the `site` it
    /// leaves at: the call under way there, the room and the place of the
    /// frame the code runs in, the op array and opline it goes on at, and
    /// the frame to be stood at that opline.
    fn exit_leaving(&mut self, stands: Stands, [call, room, place]: [Value; 3]) -> Leaving {
        Leaving {
            room,
            place,
            call,
            stands: self.b.ins().iconst(I64, stands.word().cast_signed()),
            stand: Some(self.b.ins().iconst(I32, i64::from(stands.opline))),
        }
    }

    /// Hand on, from where `leaving` says the frame stands, through
    /// `door` (`jit-design.md` §4.6), the interrupt tested first where
    /// `interrupt` says, as a link to a looping root tests it. Where the
    /// interrupt is raised, the door is not written, or an escape's door
    /// names another landing, the code goes to `closed`. Otherwise the
    /// jump to the body the door names ([`Emit::tail_call`]).
    ///
    /// A loop that closes through its sides, each entered through its exit's
    /// door and each linking to the root through its own, the exit's stores
    /// made before the jump:
    ///
    /// ```php
    /// function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { if ($i % 3 == 0) { $s += 5; } else { $s -= 1; } } return $s; }
    /// echo f(3000), "\n";
    /// ```
    /// ```expect regime=jit-inline process capture
    /// 3000
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 4 f() %SCRATCH%/witness.php:2
    /// ---- TRACE 2 start (side trace 1/4) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 3 f() %SCRATCH%/witness.php:2
    /// ---- TRACE 3 start (side trace 1/3) f() %SCRATCH%/witness.php:2
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    fn hand_on(
        &mut self,
        leaving: Leaving,
        door: Door,
        interrupt: bool,
        closed: Block,
    ) -> Result<(), Abort> {
        if interrupt {
            self.interrupt_test(Raised::Answer(closed))?;
        }
        let images = self.images()?;
        let words = self.door_words(door)?;
        let body = match door {
            // Written for this landing, or for none, whose key no `Stood`
            // holds; and with it, the body.
            Door::Escape => {
                let key = self.b.ins().load(I64, trusted(), images, words + DOOR_KEY);
                let other = self.b.ins().icmp(IntCC::NotEqual, key, leaving.stands);
                self.divert_if(other, closed);
                self.b.ins().load(I64, trusted(), images, words + DOOR_BODY)
            }
            Door::Exit(_) => {
                let body = self.b.ins().load(I64, trusted(), images, words + DOOR_BODY);
                self.closed_unless(body, closed);
                body
            }
        };
        let entered = (self.b.ins()).load(I64, trusted(), images, words + DOOR_IMAGES);
        self.tail_call(leaving, body, entered)
    }

    /// The tail call: the frame stood at the opline an exit names, as
    /// `enter`'s landing stands it, and `body` called with the entry: the
    /// `ExitCtx`, the frame's room, its place as the entry word of calls
    /// ready, the request's addresses of the trace entered, `images`, the
    /// call under way and where the frame stands.
    ///
    /// An exit taken with a call under way: the side entered through the
    /// door makes that call, its header and its room the source pushed:
    ///
    /// ```php
    /// function h($x) { return $x + 1; }
    /// function g($a, $b) { return $a + $b; }
    /// function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += g($i, $i % 2 ? h($i) : 0); } return $s; }
    /// echo f(1000), "\n";
    /// ```
    /// ```expect regime=jit-inline process capture
    /// 750000
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:5
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:4
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 5 f() %SCRATCH%/witness.php:4
    /// ---- TRACE 2 start (side trace 1/5) f() %SCRATCH%/witness.php:4
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 4 f() %SCRATCH%/witness.php:4
    /// ---- TRACE 3 start (side trace 1/4) f() %SCRATCH%/witness.php:4
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    fn tail_call(&mut self, leaving: Leaving, body: Value, images: Value) -> Result<(), Abort> {
        let cx = self.cx.ok_or_else(|| mismatch("a jump before the entry"))?;
        let Leaving {
            room,
            place,
            call,
            stands,
            stand,
        } = leaving;
        // The frame stood where the exit lands it.
        if let Some(opline) = stand {
            self.b.ins().store(trusted(), opline, room, EX_OPLINE);
        }
        let sig = self.b.import_signature(body_signature());
        self.b
            .ins()
            .return_call_indirect(sig, body, &[cx, room, place, images, call, stands]);
        Ok(())
    }
}
