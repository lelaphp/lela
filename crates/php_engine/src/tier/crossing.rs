//! What the compiled tier's crossing reaches of the engine
//! (`jit-design.md` §4.6, §5.3): a frame by its room, php's
//! `execute_data`, which compiled code is entered with and reads its
//! slots past; the frame the loop stands in once a trace has left;
//! `SAVE_OPLINE`; the callee an `INIT_*`'s cache slot holds; the room
//! the stack has left for the frames compiled code pushes; and the three
//! releases, a kernel's `zval_ptr_dtor`, a leave's free of a variable
//! and an exit's freeing of the temporaries an opline read. Compiled
//! code holds a frame by its room and the engine by its position, and
//! these are the two ways across.

use std::ptr::NonNull;

use php_heap::{FrameRef, ROOM_BYTES, Zval};
use php_unit::{CacheSlot, Opcode, OplineIdx};

use super::{FuncRef, Held};
use crate::api::request::Request;
use crate::api::values::Value;
use crate::exec;

/// The room of the frame standing at `frame`, php's `execute_data`: the
/// address a trace is entered with and a kernel is handed, its slots
/// [`ROOM_BYTES`] past it. None for a position no frame stands at on
/// the request's stack, a generator's frame on the heap included.
#[must_use]
pub fn room(req: &mut Request, frame: FrameRef) -> Option<NonNull<u8>> {
    let slots = req.vm_mut().stack.frame_mut(frame)?.slots_mut();
    NonNull::new(slots.as_mut_ptr().cast::<u8>().wrapping_sub(ROOM_BYTES))
}

/// The bytes the request's stack has left on its top page for frames,
/// php's `EG(vm_stack_end) - EG(vm_stack_top)`, which compiled code holds
/// against the frames it pushes as php's `zend_jit_stack_check` does.
#[must_use]
pub fn stack_room(req: &Request) -> usize {
    req.vm().stack.room()
}

/// The address of the request's VM stack, the running one's, where
/// [`super::FRAME`]'s stack offsets are taken from: what compiled code
/// that pushes and pops a plain call's frame reads and writes the top, the
/// end, the frame table and the innermost room through. It is the
/// request's for as long as the request runs; a fiber's switch swaps the
/// stacks standing there and moves none.
#[must_use]
pub fn stack(req: &mut Request) -> NonNull<u8> {
    NonNull::from(&mut req.vm_mut().stack).cast()
}

/// Whether the runtime cache of the op array `f` names is laid down in
/// the request, `RUN_TIME_CACHE(op_array)` not `NULL`: what a call's
/// activation in place reads its callee's cache from.
#[must_use]
pub fn laid(req: &Request, f: FuncRef) -> bool {
    req.cache_laid(exec::CacheAt::of(f.offset()))
}

/// The frame whose room is at `room` ([`room`]), looked for from the
/// innermost down, where the frames compiled code runs in stand. The
/// top is asked first, where a kernel's frame stands unless a call is
/// under way over it, with no bounds check or index conversion of the
/// walk's (#4087); the walk visits the same frames in the same order,
/// so what is found is what it found, and it costs the frames above the
/// one found, not the stack's depth.
#[must_use]
#[inline]
pub fn at_room(req: &Request, room: NonNull<u8>) -> Option<FrameRef> {
    let stack = &req.vm().stack;
    let slots = room.addr().get().checked_add(ROOM_BYTES)?;
    if let Some((top, frame)) = stack.top()
        && frame.slots().as_ptr().addr() == slots
    {
        return Some(top);
    }
    (0..stack.len())
        .rev()
        .find(|&at| {
            stack
                .frame(FrameRef(u32::try_from(at).unwrap_or(u32::MAX)))
                .is_some_and(|f| f.slots().as_ptr().addr() == slots)
        })
        .and_then(|at| u32::try_from(at).ok().map(FrameRef))
}

/// The innermost user frame, the one the loop goes on in: where a trace
/// that left stands, after whatever calls and returns its kernels made.
#[must_use]
pub fn current(req: &Request) -> Option<FrameRef> {
    let at = req.vm().innermost_at()?;
    u32::try_from(at).ok().map(FrameRef)
}

/// `EX(opline) = opline`, php's `SAVE_OPLINE()`: `frame` stands at `at`,
/// where a step serves it from and where what a kernel raises is
/// reported, and where an exit lands it.
pub fn stand(req: &mut Request, frame: FrameRef, at: OplineIdx) {
    req.vm_mut().stand(frame, at.0);
}

/// The callee the cache slot of the `INIT_FCALL`, `INIT_FCALL_BY_NAME` or
/// `INIT_NS_FCALL_BY_NAME` at `at` holds, by the op array it serves,
/// where a pass primed it with a user function: what each pushes with no
/// lookup (`ZEND_INIT_FCALL_SPEC_CONST`'s `CACHED_PTR`), so compiled code
/// that inlined a call to one callee asks before the engine pushes one.
/// None for a slot not primed, a native and any other opline. `held` is
/// the op array `frame` serves, as compiled code holds it.
#[must_use]
pub fn primed(req: &Request, frame: FrameRef, held: Held<'_>, at: OplineIdx) -> Option<FuncRef> {
    let op = exec::opline_at(held.code(), at.0)?;
    let init = matches!(
        op.opcode,
        Opcode::InitFcall | Opcode::InitFcallByName | Opcode::InitNsFcallByName
    );
    let exec::Cached::Call(call) = req.cached(frame, CacheSlot(op.out)) else {
        return None;
    };
    let user = req.user_function(call.function).filter(|_| init)?;
    let body = exec::body_of(&user.unit, user.body)?;
    Some(FuncRef::at(body.runtime_cache))
}

/// The serving's entry frame `frame` returned `zval`, which compiled code
/// hands over with its owner, as `RETURN`'s handler leaves a frame
/// `zend_call_function` served (`ZEND_CALL_TOP`), through the handler's
/// own entry return: the observer told, and the value the serving's
/// return value, which its caller takes once the serving is over, the
/// frame popped there. Nothing for a frame that is not the serving's
/// entry ([`entry`](super::entry)), whose `false` says so.
pub fn return_entry(req: &mut Request, frame: FrameRef, zval: Zval) -> bool {
    if req.stack().entry() != frame {
        return false;
    }
    req.entry_returned(frame, Value::of_slot(zval));
    true
}

/// php's `zval_ptr_dtor` of `zval`, a counted value compiled code gives
/// up: a destructor it runs may leave a throwable pending.
pub fn release(req: &mut Request, zval: Zval) {
    req.release(Value::of_slot(zval));
}

/// `ZVAL_MAKE_REF` of the variable at `place`, which holds no reference,
/// as php's `zend_jit_send_ref` makes it in place
/// (`Z/ext/opcache/jit/zend_jit_ir.c:9792-9860`): a new reference over
/// what it held, `null` for what nothing wrote, written back into the
/// place, which keeps the one owner of it; `false` where the memory
/// limit refused it: a fatal the request ends in, which the crossing
/// answers as its unwinding, or, under a fallible scope, a refusal that
/// ends nothing, whose send the handler then serves.
pub fn make_ref(req: &mut Request, place: php_heap::ZvalRef) -> bool {
    req.make_ref(place).is_ok()
}

/// php's `zend_jit_free_cv` of a variable of `frame`, which a `RETURN`
/// compiled code leaves the frame by in place gives up where it is the
/// last owner, or where it leaves owners of a value the collector may
/// take as a possible root: `frame` off the chain a backtrace reads
/// first, `EG(current_execute_data) = EX(prev_execute_data)` as
/// `zend_jit_leave_frame` stores it before the first free, then
/// `i_zval_ptr_dtor`, whose destructor sees the caller as the frame that
/// runs and may leave a throwable pending.
pub fn free_cv(req: &mut Request, frame: FrameRef, zval: Zval) {
    if let Some(header) = req.vm_mut().at_mut(frame) {
        header.info.set_leaving(true);
    }
    req.release(Value::of_slot(zval));
}

/// The receiver of `frame`, a method's frame a `RETURN` compiled code
/// leaves by in place gives up, after its variables, where its owner or
/// its pin is the last: `frame` off the chain a backtrace reads, as
/// [`free_cv`] takes it, then `OBJ_RELEASE(Z_OBJ(execute_data->This))`
/// under `ZEND_CALL_RELEASE_THIS`, as `zend_jit_leave_func` gives it up
/// (`Z/ext/opcache/jit/zend_jit_ir.c:11135-11160`), or the pin of a
/// receiver the frame was lent let go ([`Request::give_up_receiver`]).
/// A destructor it serves may leave a throwable pending.
pub fn free_this(req: &mut Request, frame: FrameRef) {
    let Some(header) = req.vm_mut().at_mut(frame) else {
        return;
    };
    header.info.set_leaving(true);
    let receiver = header.receiver();
    header.set_receiver(None);
    if let Some(receiver) = receiver {
        req.give_up_receiver(receiver);
    }
}

/// The closure whose call pushed `frame`, a closure's frame a `RETURN`
/// compiled code leaves by in place gives up, after its variables and its
/// receiver, where its owner is the last: `frame` off the chain a backtrace
/// reads, as [`free_cv`] takes it, then
/// `OBJ_RELEASE(ZEND_CLOSURE_OBJECT(EX(func)))`, as `zend_jit_leave_func`
/// gives it up (`Z/ext/opcache/jit/zend_jit_ir.c:11125-11133`). A
/// destructor of what it bound may leave a throwable pending.
pub fn free_closure(req: &mut Request, frame: FrameRef) {
    let Some(header) = req.vm_mut().at_mut(frame) else {
        return;
    };
    header.info.set_leaving(true);
    if let Some(closure) = header.take_closure() {
        req.release(closure.as_value());
    }
}

/// `zend_nan_coerced_to_type_warning(_IS_BOOL)`: the warning of a NAN
/// whose truth compiled code reads, in place of the one [`crate::ops::is_true`]
/// raises for the interpreter, which an error handler may answer with a
/// throw left pending.
pub fn nan_to_bool(req: &mut Request) {
    crate::api::raising::warning(req, "unexpected NAN value was coerced to bool");
}

/// `FREE_OP1` and `FREE_OP2` of the opline at `at` of `frame`, as the
/// handler would have freed them had it finished, each where it is
/// asked and a temporary: php's exit with `ZEND_JIT_EXIT_FREE_OP1` or
/// `_OP2`, `zval_ptr_dtor_nogc(EX_VAR(opline->op1.var))`.
pub fn free_operands(req: &mut Request, frame: FrameRef, at: OplineIdx, op1: bool, op2: bool) {
    let Some(serving) = req.serving(frame) else {
        return;
    };
    let Some(code) = serving.code() else {
        return;
    };
    let Some(&op) = exec::opline_at(code, at.0) else {
        return;
    };
    if op1 {
        exec::free_operand(req, frame, op.op1_type, op.op1);
    }
    if op2 {
        exec::free_operand(req, frame, op.op2_type, op.op2);
    }
}
