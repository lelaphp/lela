//! A body's jumps to another trace (#3229's ruling, `jit-design.md` §4.3,
//! §4.6): a link, a jump in the body's own bytes to the trace it links
//! to, written before the body is published, as php's
//! `zend_jit_trace_link_to_root` writes it
//! (`Z/ext/opcache/jit/zend_jit_ir.c:17418-17432`); and an exit's jump to
//! the side trace grown from it, through the exit's entry slot, which the
//! table writes once as the side publishes, where php patches the exit's
//! jump (`zend_jit_link_side_trace`, `:17413`).
//!
//! Each is one tail call, made by [`Emit::hand_on`] alone: the trace it
//! enters found, its row in the request found ready, and the entry laid
//! down in the `ExitCtx` as `enter` lays it, where the exit leaves the
//! frame, with the frame stood at the exit's opline as `enter`'s landing
//! stands it. Where any of it is not there the exit's number is answered,
//! with nothing stored twice, and `dispatch` goes on from there as it did
//! before a jump existed: the exit is landed, and the side trace, or the
//! trace the link names, entered through the crossing, which lays the
//! row down for the next jump.
//!
//! An exit jumps only where its entry can be laid in code: an exit that
//! grows side traces, names its opline and lands plainly or with a call
//! under way, which frees nothing, and is not the trace's end. Every
//! other is answered as before.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, Value};
use php_engine::tier::FRAME;

use super::calls::TOP;
use super::{
    CallRoom, Emit, Raised, STOOD_CALL, STOOD_FRAME, STOOD_FUNC, STOOD_ROOM, body_signature,
    engine_offset, imm, mismatch, trusted,
};
use crate::lower::{
    CACHES_AT, ENTRANCE_BODY, ENTRANCE_TRACE, ENTRY_AT, ESCAPED, IMAGES_AT, LANDING_BYTES,
    LANDING_KEY, LANDING_TAG, LANDINGS, Linked, READY_AT, ROOT_AT, ROW_BYTES, ROW_CACHES,
    ROW_IMAGES, ROW_READY, ROW_SEGMENT, ROWS_AT, STOOD_AT, Stands, TRACE_AT,
};
use crate::types::{Abort, End, ExitId, Grows, Landing};

/// Where a jump finds the trace it enters.
#[derive(Debug, Clone, Copy)]
enum Target {
    /// Through the entry slot at this address, set or not.
    Slot(usize),
    /// At this entrance, read from a landing slot.
    Entrance(Value),
    /// The trace a link names, whose entrance is known as the body is
    /// compiled.
    Linked(Linked),
}

/// Where the frame a jump enters on stands as the trace leaves it, which
/// the jump lays down as the entry: its room and place, the call under
/// way in it, its op array and opline as one word, as a `Stood` holds them,
/// the stack's room, and the opline to stand the frame at where the jump
/// leaves through an exit, which the frame does not stand at yet.
#[derive(Debug, Clone, Copy)]
struct Leaving {
    room: Value,
    place: Value,
    call: Value,
    at: Value,
    left: Value,
    stand: Option<Value>,
}

/// A trace's number's place in its segment of rows, as a mask.
const ROW_MASK: i32 = imm(ROW_SEGMENT - 1);

/// Where the frame's opline is in its room, `EX(opline)`.
const OPLINE: i32 = imm(FRAME.opline() as usize);

impl Emit<'_> {
    /// Where the frame goes on as a trace is entered from `exit`, its op
    /// array by the engine's number and its opline, where the entry can be
    /// laid in code: the opline the exit names, or, where it names none, the
    /// opline a link's trace opens at, `linked`'s, where the link's frame
    /// stands already, in a frame of that op array; and a landing that
    /// frees nothing, as `enter`'s
    /// landing of it does nothing but stand the frame.
    fn goes_on_at(&self, exit: ExitId, linked: Option<Linked>) -> Option<Stands> {
        let spec = self.ir.exits.get(usize::from(exit.0))?;
        if !matches!(spec.landing, Landing::Plain | Landing::RestoreCall) {
            return None;
        }
        if let Some(opline) = spec.opline {
            let frame = self.ir.frames.get(usize::from(spec.frame.0))?;
            return Some(Stands {
                func: engine_offset(frame.func).ok()?,
                opline: opline.0,
            });
        }
        let opens = linked?.opens?;
        let running = self.ir.frames.get(usize::from(self.running.0))?;
        (engine_offset(running.func).ok()? == opens.func).then_some(opens)
    }

    /// Whether the entry of a trace entered from `exit` can be laid in
    /// code ([`Emit::goes_on_at`]).
    fn jumps(&self, exit: ExitId) -> bool {
        self.goes_on_at(exit, None).is_some()
    }

    /// Whether a guard that fails into `exit` jumps through its entry
    /// slot: the body has slots, the exit jumps, and it grows side traces
    /// and is not the trace's end, which `dispatch` follows no side from.
    fn opens(&self, exit: ExitId) -> bool {
        let leaves = match self.ir.end {
            End::Leave(exit) | End::Return(exit) => Some(exit),
            End::Loop(_) => None,
        };
        self.joins.slots != 0
            && leaves != Some(exit)
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
    /// through its entry slot.
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
            let slot = at
                .checked_mul(size_of::<usize>())
                .and_then(|past| self.joins.slots.checked_add(past))
                .ok_or_else(|| mismatch("an entry slot past the host's addresses"))?;
            let exit = ExitId(u16::try_from(at).map_err(|_| mismatch("too many exits"))?);
            let closed = self.answer_block(exit)?;
            let at = (self.goes_on_at(exit, None))
                .ok_or_else(|| mismatch("a door from an exit that names no opline"))?;
            let leaving = self.exit_leaving(at, [call, room, place])?;
            self.hand_on(leaving, Target::Slot(slot), closed)?;
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
                let leaving = self.exit_leaving(at, site)?;
                self.hand_on(leaving, Target::Linked(linked), closed)
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
    /// left: where the frame stands, its op array and opline as the last
    /// `Stood` says, compared with each landing slot, and the jump to the
    /// trace the slot that holds it names, as php's escape goes on into
    /// the handler of the opline the frame stands at, the trace compiled
    /// there (`jit_stub_trace_escape`); [`ESCAPED`] answered where none
    /// does or the jump finds the trace's row unready.
    pub(super) fn escaped(&mut self, block: Block) -> Result<(), Abort> {
        self.b.switch_to_block(block);
        self.b.seal_block(block);
        let place = *self
            .b
            .block_params(block)
            .first()
            .ok_or_else(|| mismatch("an escape that takes no place"))?;
        let answer = self.b.create_block();
        self.b.set_cold_block(answer);
        if self.joins.landings != 0 {
            let at = self.stood_word(I64, STOOD_FUNC);
            let key = self.b.ins().bor_imm_u(at, LANDING_TAG.cast_signed());
            let found = self.b.create_block();
            self.b.set_cold_block(found);
            self.b.append_block_param(found, I64);
            for slot in 0..LANDINGS {
                let address = (slot.checked_mul(LANDING_BYTES))
                    .and_then(|past| self.joins.landings.checked_add(past))
                    .and_then(|address| i64::try_from(address).ok())
                    .ok_or_else(|| mismatch("a landing slot past the host's addresses"))?;
                let address = self.b.ins().iconst(I64, address);
                let entrance = self.b.ins().atomic_load(I64, trusted(), address);
                let next = self.b.create_block();
                self.b.set_cold_block(next);
                self.closed_unless(entrance, next);
                let held = self.b.ins().load(I64, trusted(), address, LANDING_KEY);
                let other = self.b.ins().icmp(IntCC::NotEqual, held, key);
                self.b
                    .ins()
                    .brif(other, next, &[], found, &[BlockArg::Value(entrance)]);
                self.b.switch_to_block(next);
                self.b.seal_block(next);
            }
            self.b.ins().jump(answer, &[]);
            self.b.switch_to_block(found);
            self.b.seal_block(found);
            let entrance = *self
                .b
                .block_params(found)
                .first()
                .ok_or_else(|| mismatch("a landing found with no entrance"))?;
            let leaving = Leaving {
                room: self.stood_word(I64, STOOD_FRAME),
                place,
                call: self.stood_word(I64, STOOD_CALL),
                at,
                left: self.stood_word(I64, STOOD_ROOM),
                stand: None,
            };
            self.hand_on(leaving, Target::Entrance(entrance), answer)?;
        } else {
            self.b.ins().jump(answer, &[]);
        }
        self.b.switch_to_block(answer);
        self.b.seal_block(answer);
        let escaped = self.b.ins().iconst(I32, i64::from(ESCAPED));
        self.b.ins().return_(&[escaped]);
        Ok(())
    }

    /// Go to `closed` where `value` is zero; go on otherwise.
    fn closed_unless(&mut self, value: Value, closed: Block) {
        let zero = self.b.ins().icmp_imm_s(IntCC::Equal, value, 0);
        self.divert_if(zero, closed);
    }

    /// Where an exit leaves the frame (`enter::land`), from the `site` it
    /// leaves at: the call under way there, the room and the place of the
    /// frame the code runs in, the op array and opline it goes on at, the
    /// stack's room, and the frame to be stood at that opline.
    fn exit_leaving(
        &mut self,
        stands: Stands,
        [call, room, place]: [Value; 3],
    ) -> Result<Leaving, Abort> {
        let at = stands.word();
        let at = self.b.ins().iconst(I64, at.cast_signed());
        let stack = self.stack()?;
        let top = self.b.ins().load(I64, trusted(), stack, TOP);
        Ok(Leaving {
            room,
            place,
            call,
            at,
            left: self.left_past(stack, top),
            stand: Some(self.b.ins().iconst(I32, i64::from(stands.opline))),
        })
    }

    /// Hand on, from where `leaving` says the frame stands, to the trace
    /// `target` finds: the jump (`jit-design.md` §4.6). Where the slot is
    /// unset, a link's interrupt is raised, or the request's row of the
    /// trace is not ready, the code goes to `closed`, which answers what
    /// the trace answers without the jump. Otherwise the entry is laid
    /// down as `enter` lays it, at the trace's first op: where the frame
    /// stands as a kernel would say, its op array and opline, the call
    /// under way and the stack's room; the frame's place; the trace's
    /// calls' headers in the request, ready; and its number. The frame is
    /// stood at the opline an exit names, as `enter`'s landing stands it,
    /// and the trace's body is called in tail position with the `ExitCtx`
    /// and the frame's room.
    fn hand_on(&mut self, leaving: Leaving, target: Target, closed: Block) -> Result<(), Abort> {
        let cx = self.cx.ok_or_else(|| mismatch("a jump before the entry"))?;
        let entrance = match target {
            Target::Slot(slot) => {
                let slot = i64::try_from(slot)
                    .map_err(|_| mismatch("an entry slot past the host's addresses"))?;
                let slot = self.b.ins().iconst(I64, slot);
                let entrance = self.b.ins().atomic_load(I64, trusted(), slot);
                self.closed_unless(entrance, closed);
                Some(entrance)
            }
            Target::Entrance(entrance) => Some(entrance),
            Target::Linked(_) => None,
        };
        let (body, trace) = match (entrance, target) {
            (Some(entrance), _) => (
                self.b.ins().load(I64, trusted(), entrance, ENTRANCE_BODY),
                self.b.ins().load(I32, trusted(), entrance, ENTRANCE_TRACE),
            ),
            (None, Target::Linked(linked)) => {
                if linked.interrupt {
                    self.interrupt_test(Raised::Answer(closed))?;
                }
                let body = i64::try_from(linked.body)
                    .map_err(|_| mismatch("a linked body past the host's addresses"))?;
                (
                    self.b.ins().iconst(I64, body),
                    self.b.ins().iconst(I32, i64::from(linked.trace)),
                )
            }
            (None, _) => return Err(mismatch("a jump with no trace to enter")),
        };
        // The request's row of the trace: its segment, then the row in it.
        let rows = self.b.ins().load(I64, trusted(), cx, ROWS_AT);
        let number = self.b.ins().uextend(I64, trace);
        let segment = self
            .b
            .ins()
            .ushr_imm_u(number, i64::from(ROW_SEGMENT.trailing_zeros()));
        let segment = self.b.ins().ishl_imm_u(segment, 3);
        let segment = self.b.ins().iadd(rows, segment);
        let first = self.b.ins().load(I64, trusted(), segment, 0);
        self.closed_unless(first, closed);
        let within = self.b.ins().band_imm_u(number, i64::from(ROW_MASK));
        let within = self.b.ins().imul_imm_u(within, ROW_BYTES);
        let row = self.b.ins().iadd(first, within);
        let ready = self.b.ins().load(I32, trusted(), row, ROW_READY);
        self.closed_unless(ready, closed);
        let images = self.b.ins().load(I64, trusted(), row, ROW_IMAGES);
        let caches = self.b.ins().load(I64, trusted(), row, ROW_CACHES);
        // The entry, as `enter` lays it.
        let Leaving {
            room,
            place,
            call,
            at,
            left,
            stand,
        } = leaving;
        let place = self.b.ins().ireduce(I32, place);
        let first_op = self.b.ins().iconst(I32, 0);
        let one = self.b.ins().iconst(I32, 1);
        for (value, field) in [
            (first_op, ENTRY_AT),
            (images, IMAGES_AT),
            (caches, CACHES_AT),
            (one, READY_AT),
            (place, ROOT_AT),
            (trace, TRACE_AT),
            (room, STOOD_AT + STOOD_FRAME),
            (call, STOOD_AT + STOOD_CALL),
            (at, STOOD_AT + STOOD_FUNC),
            (left, STOOD_AT + STOOD_ROOM),
        ] {
            self.b.ins().store(trusted(), value, cx, field);
        }
        // The frame stood where the exit lands it.
        if let Some(opline) = stand {
            self.b.ins().store(trusted(), opline, room, OPLINE);
        }
        let sig = self.b.import_signature(body_signature());
        self.b.ins().return_call_indirect(sig, body, &[cx, room]);
        Ok(())
    }
}
