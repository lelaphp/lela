//! What the compiled tier's crossing reaches of the engine
//! (`jit-design.md` §4.6, §5.3): a frame by its room, php's
//! `execute_data`, which compiled code is entered with and reads its
//! slots past; the frame the loop stands in once a trace has left;
//! `SAVE_OPLINE`; the callee an `INIT_*`'s cache slot holds; the room
//! the stack has left for the frames compiled code pushes; and the two
//! releases, a kernel's `zval_ptr_dtor` and an exit's freeing of the
//! temporaries an opline read. Compiled code holds a frame by its room
//! and the engine by its position, and these are the two ways across.

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
/// innermost down, where the frames compiled code runs in stand.
#[must_use]
pub fn at_room(req: &Request, room: NonNull<u8>) -> Option<FrameRef> {
    let stack = &req.vm().stack;
    let slots = room.addr().get().checked_add(ROOM_BYTES)?;
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

/// php's `zval_ptr_dtor` of `zval`, a counted value compiled code gives
/// up: a destructor it runs may leave a throwable pending.
pub fn release(req: &mut Request, zval: Zval) {
    req.release(Value::of_slot(zval));
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
