//! The frame's layout across the tier face, #3222's ruling
//! (`jit-design.md` §5.3, ADR-0006): [`FRAME`], one value that says
//! where a frame's header words stand in its room, which of its flags a
//! plain leave refuses, and where the VM stack keeps its top, its end,
//! its frame table and the innermost room's address, every offset
//! computed by `offset_of!` over the engine's own types; and the
//! [`PlainCall`] a recording takes of a call `push_call` laid down, so
//! that compiled code writes the same call's push, its activation and
//! its callee's leave in place, as php's code generator does
//! (`zend_jit_push_call_frame`, `zend_jit_do_fcall`, `zend_jit_leave_func`,
//! `Z/ext/opcache/jit/zend_jit_ir.c:8466-8650`, `:10016-10600`,
//! `:11035-11263`).
//!
//! Each accessor of [`FrameLayout`] is an item of `tier.lock`, so a field
//! or a bit added, removed or renamed is a diff of that file, while a
//! moved offset is not; no offset is written in `php_jit` or in a seam's
//! file.

use php_heap::{FrameRef, HEADER_AT, HeaderImage, ROOM_BYTES, StackLayout, VmStack, Zval};
use php_unit::MapPtrOffset;
use php_unit::decl::Bare;

use super::FuncRef;
use crate::api::request::Request;
use crate::exec::{CallInfo, Header};

/// A layout offset or a word as the `u32` [`FrameLayout`] keeps it.
const fn word(at: usize) -> u32 {
    assert!(at <= u32::MAX as usize, "a layout offset past a word");
    #[allow(clippy::cast_possible_truncation, reason = "held below u32::MAX above")]
    let at = at as u32;
    at
}

/// The stack the engine's frames stand on, as compiled code reads it.
const STACK: StackLayout = VmStack::<Header>::LAYOUT;

/// Where a frame's words stand, as compiled code that pushes, makes and
/// leaves a plain call reads and writes them (`jit-design.md` §5.3):
/// offsets in bytes into a frame's room, php's `execute_data`, into the
/// request's VM stack, and into a record of its frame table. One value,
/// [`FRAME`]; read at compile time, every accessor is a `const fn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    slot: u32,
    room: u32,
    link: u32,
    header: u32,
    header_bytes: u32,
    opline: u32,
    info: u32,
    num_args: u32,
    caller: u32,
    returns_to: u32,
    cache: u32,
    annex: u32,
    this: u32,
    closure: u32,
    leaves_by_kernel: u32,
    send_by_ref: u32,
    caller_loop: u32,
    numbered_from: u32,
    top: u32,
    end: u32,
    frames: u32,
    frames_len: u32,
    frames_cap: u32,
    innermost: u32,
    record: u32,
    record_at: u32,
    record_len: u32,
    record_placed: u32,
    on_page: u32,
}

/// The engine's frame layout.
pub const FRAME: FrameLayout = FrameLayout {
    slot: word(size_of::<Zval>()),
    room: word(ROOM_BYTES),
    link: 0,
    header: word(HEADER_AT),
    header_bytes: word(size_of::<Header>()),
    opline: word(HEADER_AT + Header::OPLINE_AT),
    info: word(HEADER_AT + Header::INFO_AT),
    num_args: word(HEADER_AT + Header::NUM_ARGS_AT),
    caller: word(HEADER_AT + Header::CALLER_AT),
    returns_to: word(HEADER_AT + Header::RETURNS_TO_AT),
    cache: word(HEADER_AT + Header::CACHE_AT),
    annex: word(HEADER_AT + Header::ANNEX_AT),
    this: word(HEADER_AT + Header::THIS_AT),
    closure: word(HEADER_AT + Header::CLOSURE_AT),
    leaves_by_kernel: CallInfo::LEAVES_BY_KERNEL,
    send_by_ref: CallInfo::SEND_BY_REF,
    caller_loop: Header::CALLER_LOOP,
    numbered_from: Header::NUMBERED_FROM,
    top: word(STACK.top),
    end: word(STACK.end),
    frames: word(STACK.frames + STACK.table.at),
    frames_len: word(STACK.frames + STACK.table.len),
    frames_cap: word(STACK.frames + STACK.table.cap),
    innermost: word(STACK.innermost),
    record: word(STACK.table.record),
    record_at: word(STACK.table.record_at),
    record_len: word(STACK.table.record_len),
    record_placed: word(STACK.table.record_placed),
    on_page: STACK.table.on_page as u32,
};

// The words compiled code moves are words and half-words it can move
// whole: the header's eight-byte words and its half-words at their own
// alignment.
const _: () = {
    assert!(FRAME.header_bytes.is_multiple_of(8) && FRAME.header.is_multiple_of(8));
    assert!(FRAME.header + FRAME.header_bytes == FRAME.room);
    // The link is the room's first word, the header the words after it.
    assert!(FRAME.link == 0 && FRAME.header as usize == size_of::<usize>());
    assert!(FRAME.room.is_multiple_of(FRAME.slot));
    assert!(
        FRAME.opline.is_multiple_of(4)
            && FRAME.info.is_multiple_of(4)
            && FRAME.num_args.is_multiple_of(4)
    );
    assert!(FRAME.caller.is_multiple_of(4) && FRAME.returns_to.is_multiple_of(4));
    assert!(FRAME.cache.is_multiple_of(4) && FRAME.annex.is_multiple_of(4));
    assert!(FRAME.this.is_multiple_of(8) && FRAME.closure.is_multiple_of(8));
};

impl FrameLayout {
    /// A slot's bytes, `sizeof(zval)`, the stride of a frame's slots and
    /// of a room's.
    #[must_use]
    pub const fn slot(&self) -> u32 {
        self.slot
    }

    /// Where a room keeps its link to the room linked before it, php's
    /// `prev_execute_data`, a word.
    #[must_use]
    pub const fn link(&self) -> u32 {
        self.link
    }

    /// The bytes of a room before its first slot, `ZEND_CALL_FRAME_SLOT`
    /// zvals: a frame's slot `n` is at `room() + 16 * n`. Its first word
    /// is the link to the room linked before it, php's
    /// `prev_execute_data`.
    #[must_use]
    pub const fn room(&self) -> u32 {
        self.room
    }

    /// Where the header starts in the room, past the link.
    #[must_use]
    pub const fn header(&self) -> u32 {
        self.header
    }

    /// The header's bytes, a whole number of words, which a push copies
    /// from a [`PlainCall`]'s header whole.
    #[must_use]
    pub const fn header_bytes(&self) -> u32 {
        self.header_bytes
    }

    /// `EX(opline)`, a half-word: the opline's number in its op array.
    #[must_use]
    pub const fn opline(&self) -> u32 {
        self.opline
    }

    /// `ZEND_CALL_INFO`, a half-word.
    #[must_use]
    pub const fn info(&self) -> u32 {
        self.info
    }

    /// `ZEND_CALL_NUM_ARGS`, a half-word.
    #[must_use]
    pub const fn num_args(&self) -> u32 {
        self.num_args
    }

    /// The caller, a half-word: zero for none, else the caller's frame
    /// numbered from [`FrameLayout::numbered_from`], with
    /// [`FrameLayout::caller_loop`] set where the call was served from
    /// outside the loop.
    #[must_use]
    pub const fn caller(&self) -> u32 {
        self.caller
    }

    /// The slot of the caller's that takes what the frame returns, a
    /// half-word: zero for none, else the slot numbered from
    /// [`FrameLayout::numbered_from`].
    #[must_use]
    pub const fn returns_to(&self) -> u32 {
        self.returns_to
    }

    /// `EX(run_time_cache)`, a half-word: the engine's map-pointer offset
    /// of the cache for one of the engine's op arrays.
    #[must_use]
    pub const fn cache(&self) -> u32 {
        self.cache
    }

    /// What the frame owns beyond words, a half-word: zero for nothing.
    #[must_use]
    pub const fn annex(&self) -> u32 {
        self.annex
    }

    /// `Z_OBJ(EX(This))`, a word: zero for none.
    #[must_use]
    pub const fn this(&self) -> u32 {
        self.this
    }

    /// The closure whose call pushed the frame, a word: zero for none.
    #[must_use]
    pub const fn closure(&self) -> u32 {
        self.closure
    }

    /// The flags of a frame whose leave the leave kernel serves: a call
    /// no `DO_*CALL` made a user frame of, and arguments moved past the
    /// declared parameters.
    #[must_use]
    pub const fn leaves_by_kernel(&self) -> u32 {
        self.leaves_by_kernel
    }

    /// The bit of a call being built that says the argument its last
    /// `CHECK_FUNC_ARG` checked is taken by reference, php's
    /// `ZEND_CALL_SEND_ARG_BY_REF`, which the `_FUNC_ARG` fetch and the
    /// `SEND_FUNC_ARG` after it read.
    #[must_use]
    pub const fn send_by_ref(&self) -> u32 {
        self.send_by_ref
    }

    /// The caller's bit that says the call was served from outside the
    /// loop, which the leave kernel serves.
    #[must_use]
    pub const fn caller_loop(&self) -> u32 {
        self.caller_loop
    }

    /// What a caller's frame and a return slot are written past, so that
    /// none is zero.
    #[must_use]
    pub const fn numbered_from(&self) -> u32 {
        self.numbered_from
    }

    /// `EG(vm_stack_top)` in the VM stack, a word: the first free slot of
    /// the top page.
    #[must_use]
    pub const fn top(&self) -> u32 {
        self.top
    }

    /// `EG(vm_stack_end)` in the VM stack, a word: one past the top
    /// page's last slot.
    #[must_use]
    pub const fn end(&self) -> u32 {
        self.end
    }

    /// The frame table's first record in the VM stack, a word.
    #[must_use]
    pub const fn frames(&self) -> u32 {
        self.frames
    }

    /// How many records the frame table holds live, a word: the number a
    /// frame pushed now takes.
    #[must_use]
    pub const fn frames_len(&self) -> u32 {
        self.frames_len
    }

    /// How many records the frame table has room for, a word.
    #[must_use]
    pub const fn frames_cap(&self) -> u32 {
        self.frames_cap
    }

    /// The innermost linked room's address in the VM stack, a word, which
    /// a push links below its room and moves to it.
    #[must_use]
    pub const fn innermost(&self) -> u32 {
        self.innermost
    }

    /// A record's size, its stride in the frame table.
    #[must_use]
    pub const fn record(&self) -> u32 {
        self.record
    }

    /// A record's frame's first slot, a word: its room's address plus
    /// [`FrameLayout::room`].
    #[must_use]
    pub const fn record_at(&self) -> u32 {
        self.record_at
    }

    /// A record's slot count, a word.
    #[must_use]
    pub const fn record_len(&self) -> u32 {
        self.record_len
    }

    /// A record's placement, whose first byte is its tag.
    #[must_use]
    pub const fn record_placed(&self) -> u32 {
        self.record_placed
    }

    /// The placement tag of a frame on the top page, whose pop gives its
    /// room back to the page's top.
    #[must_use]
    pub const fn on_page(&self) -> u32 {
        self.on_page
    }
}

/// A call to a user function as `push_call` laid it down and as a
/// `DO_*CALL`'s inline path activates it, taken by a recording where the
/// call stood built: what compiled code writes in place for the same
/// callee and argument count, as php's trace writes a recorded `func`'s
/// call. Its header is the one [`call_image`] lays out for the request
/// the code runs in, since the function's number and its record are the
/// request's.
#[derive(Debug, Clone)]
pub struct PlainCall {
    callee: FuncRef,
    count: u32,
    /// The function's name as the function table holds it, lowered.
    name: Box<[u8]>,
    slots: u32,
    activation: Option<Activation>,
    checks: bool,
}

/// Two calls of one callee and one argument count are the same call.
impl PartialEq for PlainCall {
    fn eq(&self, other: &PlainCall) -> bool {
        (self.callee, self.count) == (other.callee, other.count)
    }
}

/// What a `DO_*CALL`'s inline path writes into a plain call's header to
/// make it its callee's frame (`i_init_func_execute_data`, as
/// [`Request::enter_call`] writes it), past the caller and the slot that
/// takes its return: where the call has no `RECV*` left to serve and
/// reads its function's own cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activation {
    keep: u32,
    used: u32,
    unused: u32,
    opline: u32,
    cache: u32,
}

impl Activation {
    /// The flags of the call the activation leaves as the call's `SEND_*`
    /// oplines left them; every other it writes ([`Activation::info`]).
    #[must_use]
    pub const fn keep(&self) -> u32 {
        self.keep
    }

    /// The flags the activation sets, of those it does not keep, its
    /// return read or not: the call's flags made a frame's are
    /// `flags & keep() | info(return_used)`.
    #[must_use]
    pub const fn info(&self, return_used: bool) -> u32 {
        if return_used { self.used } else { self.unused }
    }

    /// The opline the frame stands at, its entry, past its `RECV*`.
    #[must_use]
    pub const fn opline(&self) -> u32 {
        self.opline
    }

    /// `EX(run_time_cache)`, the callee's own, of the engine's table.
    #[must_use]
    pub const fn cache(&self) -> u32 {
        self.cache
    }
}

impl PlainCall {
    /// The op array the call serves.
    #[must_use]
    pub fn callee(&self) -> FuncRef {
        self.callee
    }

    /// How many arguments its `INIT_*` says are passed.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The slots the call's room holds past its header.
    #[must_use]
    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// What the call's activation writes, where a `DO_*CALL`'s inline
    /// path makes it with no `RECV*` to serve, or with `RECV`s that only
    /// check ([`PlainCall::checks`]); none where the callee's activation
    /// serves its prologue or reads a cache of its own.
    #[must_use]
    pub fn activation(&self) -> Option<Activation> {
        self.activation
    }

    /// Whether the activation passes over `RECV` oplines that check the
    /// types the parameters declare, which compiled code makes in place
    /// only where it knows each argument is of the type its parameter
    /// declares, so no check would refuse or coerce it.
    #[must_use]
    pub fn checks(&self) -> bool {
        self.checks
    }
}

/// The plain call `call` is, the record on top of the stack being built:
/// a call to a user function whose `INIT_*` found its entry plain and gave
/// it no receiver, closure, scope or annex, with no observer declared;
/// none for any other.
#[must_use]
pub fn plain_call(req: &Request, call: FrameRef) -> Option<PlainCall> {
    let (top, laid) = req.vm().stack.top()?;
    let header = laid.header();
    let function = header.function?;
    let plain = top == call
        && header.info.building()
        && header.info.plain_entry()
        && header.this().is_none()
        && header.closure.is_none()
        && header.called_scope().is_none()
        && header.annex.is_none()
        && !crate::observer::observing(req);
    if !plain {
        return None;
    }
    let user = req.user_function(function)?;
    let code = crate::exec::body_of(&user.unit, user.body)?;
    let name = req
        .symbols()
        .function_decl(function)?
        .name
        .0
        .to_ascii_lowercase();
    let shape = user.shape;
    let slots = u32::try_from(laid.len()).ok()?;
    let count = header.num_args;
    let opline = crate::exec::skipped_recv(count, shape.parameters, shape.typed, shape.entry);
    // A function that declares a type keeps its `RECV*`, each a check; with
    // every parameter passed and one `RECV` each, the checks are all its
    // prologue does to an argument of the type declared, which compiled
    // code that knows the argument's type makes in place of them, as
    // php's trace compiles `zend_jit_recv` with no check where its stack
    // map knows the argument of a type the declaration holds
    // (`Z/ext/opcache/jit/zend_jit_ir.c:10825-10873`, the skip in
    // `zend_jit_verify_arg_type` at `:10773-10781`).
    let checks = shape.typed && count == shape.parameters && shape.entry == count;
    let enters = count <= shape.parameters
        && slots >= shape.slots
        && (opline == Some(shape.entry) || checks);
    let activation = match shape.cache.offset() {
        MapPtrOffset::Engine(cache) if enters => {
            // What `enter_call` writes into the flags, applied to all of
            // them set and to none: a bit it sets is set in both, one it
            // clears in neither, and one it keeps in the first alone.
            let entered = |bits: u32, used: bool| {
                let mut entered = header.clone();
                entered.info = CallInfo::of(bits);
                entered.info.set_serving();
                entered.info.set_return_used(used);
                entered.activate(shape.cache, shape.entry, false);
                entered.info.bits()
            };
            Some(Activation {
                keep: entered(u32::MAX, true) & !entered(0, true),
                used: entered(0, true),
                unused: entered(0, false),
                opline: shape.entry,
                cache,
            })
        }
        _ => None,
    };
    Some(PlainCall {
        callee: FuncRef::at(code.runtime_cache),
        count,
        name: name.into_boxed_slice(),
        slots,
        activation,
        checks,
    })
}

/// The header `push_call` lays down for `call` in `req`, as compiled code
/// that pushes it in place copies it: the function the request's table
/// holds under the call's name, where it serves the call's op array, its
/// record, and the flags its `INIT_*` writes. None where the request has
/// declared no such function yet, or its entry is no longer plain.
#[must_use]
pub fn call_image(req: &Request, call: &PlainCall) -> Option<HeaderImage> {
    let function = req.function_named(Bare::resolved(&call.name))?;
    let user = req.user_function(function)?;
    let code = crate::exec::body_of(&user.unit, user.body)?;
    if FuncRef::at(code.runtime_cache) != call.callee {
        return None;
    }
    let (_, header) = req.laid_call(Header::call(function, call.count))?;
    // A header with no record of its callee is one `push_call` completes
    // as it lays it down, and so not one to copy.
    if !header.info.plain_entry() || header.callee.is_none() {
        return None;
    }
    HeaderImage::of(&header)
}

#[cfg(test)]
mod tests {
    use php_heap::FrameRef;

    use super::FRAME;
    use crate::exec::Header;

    /// The caller and the return slot are written as the layout says
    /// compiled code writes them: numbered from one, the loop's bit clear.
    #[test]
    fn the_layout_numbers_a_caller_and_a_return_slot_as_the_header_does() {
        assert_eq!(Header::caller_word(FrameRef(7)), 7 + FRAME.numbered_from());
        assert_eq!(Header::caller_word(FrameRef(7)) & FRAME.caller_loop(), 0);
        assert_eq!(Header::returns_word(Some(3)), 3 + FRAME.numbered_from());
        assert_eq!(Header::returns_word(None), 0);
    }
}
