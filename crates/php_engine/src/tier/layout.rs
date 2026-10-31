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

use super::{ClassId, FuncRef};
use crate::api::call::Function;
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
    callee: u32,
    opline: u32,
    info: u32,
    num_args: u32,
    caller: u32,
    returns_to: u32,
    cache: u32,
    annex: u32,
    this: u32,
    closure: u32,
    called_scope: u32,
    leaves_by_kernel: u32,
    release_this: u32,
    send_by_ref: u32,
    building: u32,
    return_unused: u32,
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
    callee: word(HEADER_AT + Header::CALLEE_AT),
    opline: word(HEADER_AT + Header::OPLINE_AT),
    info: word(HEADER_AT + Header::INFO_AT),
    num_args: word(HEADER_AT + Header::NUM_ARGS_AT),
    caller: word(HEADER_AT + Header::CALLER_AT),
    returns_to: word(HEADER_AT + Header::RETURNS_TO_AT),
    cache: word(HEADER_AT + Header::CACHE_AT),
    annex: word(HEADER_AT + Header::ANNEX_AT),
    this: word(HEADER_AT + Header::THIS_AT),
    closure: word(HEADER_AT + Header::CLOSURE_AT),
    called_scope: word(HEADER_AT + Header::SCOPE_AT),
    leaves_by_kernel: CallInfo::LEAVES_BY_KERNEL,
    release_this: CallInfo::RELEASE_THIS,
    send_by_ref: CallInfo::SEND_BY_REF,
    building: CallInfo::BUILDING,
    return_unused: CallInfo::RETURN_UNUSED,
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
    assert!(FRAME.called_scope.is_multiple_of(4));
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

    /// `EX(func)` as another process reads it, a word: the address of
    /// the callee's record, one per function in a request, which a call
    /// a handler pushed is held to as php's `zend_jit_func_guard` holds
    /// `call->func`.
    #[must_use]
    pub const fn callee(&self) -> u32 {
        self.callee
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

    /// The called scope, `static`, a half-word: the number of its class,
    /// as `php_heap::Obj::CLASS_AT` and a runtime-cache slot hold it, with
    /// every bit inverted; zero for none.
    #[must_use]
    pub const fn called_scope(&self) -> u32 {
        self.called_scope
    }

    /// The flags of a frame whose leave the leave kernel serves: a call
    /// no `DO_*CALL` made a user frame of, and arguments moved past the
    /// declared parameters.
    #[must_use]
    pub const fn leaves_by_kernel(&self) -> u32 {
        self.leaves_by_kernel
    }

    /// `ZEND_CALL_RELEASE_THIS`: the frame owns one owner of its
    /// receiver, [`FrameLayout::this`], which its leave gives up; with it
    /// clear the receiver is pinned, and the leave lets the pin go.
    #[must_use]
    pub const fn release_this(&self) -> u32 {
        self.release_this
    }

    /// The bit of a call being built that says the argument its last
    /// `CHECK_FUNC_ARG` checked is taken by reference, php's
    /// `ZEND_CALL_SEND_ARG_BY_REF`, which the `_FUNC_ARG` fetch and the
    /// `SEND_FUNC_ARG` after it read.
    #[must_use]
    pub const fn send_by_ref(&self) -> u32 {
        self.send_by_ref
    }

    /// The bit of a call whose `SEND_*` oplines are placing its
    /// arguments, which its `DO_*CALL` clears as it makes the call the one
    /// being served, php's `EG(current_execute_data) = call`.
    #[must_use]
    pub const fn building(&self) -> u32 {
        self.building
    }

    /// The bit of a call being served whose `DO_*CALL` reads nothing it
    /// returns, `!RETURN_VALUE_USED(opline)`, which its activation writes
    /// beside clearing [`FrameLayout::building`].
    #[must_use]
    pub const fn return_unused(&self) -> u32 {
        self.return_unused
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

/// Where a runtime-cache slot keeps a property's place, as compiled code
/// reads it in the request's own slot (`jit-design.md` §4.3, #3346), as
/// php's code reads `CACHED_PTR(cache_slot)` and the offset past it for a
/// class that is not immutable (`zend_jit_fetch_obj`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:14341-14370`), and a method's call
/// its class and callee, as php's code reads `CACHED_PTR(result.num)`
/// and the callee past it (`zend_jit_init_method_call`,
/// `zend_jit_init_static_method_call`, `:9025-9056`, `:9255-9265`): offsets in bytes into
/// one slot ([`touch`](super::touch)), the tags compiled code tests before
/// it reads past them, and the bit of a slot's type that says it has
/// none. One value, [`PLACE`], every offset `offset_of!` over the
/// interpreter's own types and RFC 2195's mirror of their variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceLayout {
    tag: u32,
    unprimed: u32,
    property: u32,
    pair: u32,
    pair_class: u32,
    pair_function: u32,
    pair_record: u32,
    class: u32,
    offset_tag: u32,
    declared: u8,
    ty: u32,
    slot: u32,
    untyped: u32,
}

/// The engine's runtime-cache slot layout.
pub const PLACE: PlaceLayout = {
    use crate::exec::{Cached, PropertyOffset, PropertyPlace, SlotType};
    let offset = Cached::PLACE_AT + PropertyPlace::OFFSET_AT;
    PlaceLayout {
        tag: word(Cached::TAG_AT),
        unprimed: crate::exec::UNPRIMED,
        property: crate::exec::PROPERTY,
        pair: crate::exec::CLASS_AND_CALLEE,
        pair_class: word(Cached::PAIR_CLASS_AT),
        pair_function: word(Cached::PAIR_FUNCTION_AT),
        pair_record: word(Cached::PAIR_RECORD_AT),
        class: word(Cached::PLACE_AT + PropertyPlace::CLASS_AT),
        offset_tag: word(offset + PropertyOffset::TAG_AT),
        declared: PropertyOffset::DECLARED,
        ty: word(offset + PropertyOffset::TY_AT),
        slot: word(offset + PropertyOffset::SLOT_AT),
        untyped: SlotType::NO_TYPE as u32,
    }
};

impl PlaceLayout {
    /// Where the slot's tag is, four bytes.
    #[must_use]
    pub const fn tag(&self) -> u32 {
        self.tag
    }

    /// The tag of a slot nothing was written to.
    #[must_use]
    pub const fn unprimed(&self) -> u32 {
        self.unprimed
    }

    /// The tag of a slot that holds a property's place.
    #[must_use]
    pub const fn property(&self) -> u32 {
        self.property
    }

    /// The tag of a slot that holds the class and the callee a method's
    /// call resolved together, php's `CACHE_POLYMORPHIC_PTR(num, ce, fbc)`.
    #[must_use]
    pub const fn pair(&self) -> u32 {
        self.pair
    }

    /// Where the pair keeps its class, a half-word, the number
    /// `php_heap::Obj::CLASS_AT` holds.
    #[must_use]
    pub const fn pair_class(&self) -> u32 {
        self.pair_class
    }

    /// Where the pair keeps its callee, a half-word, its number
    /// ([`CalleeSlot::function`](super::CalleeSlot::function)).
    #[must_use]
    pub const fn pair_function(&self) -> u32 {
        self.pair_function
    }

    /// Where the pair keeps its callee's record, a word, the one a frame
    /// serving the callee is pushed with
    /// ([`FrameLayout::callee`](super::FrameLayout::callee)): how
    /// compiled code holds the slot to a request's own method, whose
    /// number is the request's alone.
    #[must_use]
    pub const fn pair_record(&self) -> u32 {
        self.pair_record
    }

    /// Where a property's place keeps the class it was found on, a
    /// half-word, the number `php_heap::Obj::CLASS_AT` holds.
    #[must_use]
    pub const fn class(&self) -> u32 {
        self.class
    }

    /// Where the place keeps its offset's tag, a byte.
    #[must_use]
    pub const fn offset_tag(&self) -> u32 {
        self.offset_tag
    }

    /// The offset's tag of a declared slot, php's offset past
    /// `ZEND_FIRST_PROPERTY_OFFSET`.
    #[must_use]
    pub const fn declared(&self) -> u8 {
        self.declared
    }

    /// Where a declared slot's type is, sixteen bits: [`PlaceLayout::untyped`]
    /// for a property with no type, else one bit per kind of value that
    /// lands in it as it stands ([`PlaceLayout::kind`]), none for a
    /// `readonly` slot and one of a set visibility the scope lacks.
    #[must_use]
    pub const fn ty(&self) -> u32 {
        self.ty
    }

    /// Where a declared slot's number is, a half-word: `OBJ_PROP_NUM`.
    #[must_use]
    pub const fn slot(&self) -> u32 {
        self.slot
    }

    /// The bit of a slot's type that admits a value of `kind` as it
    /// stands, the one [`PlaceLayout::ty`] keeps for it.
    #[must_use]
    pub const fn kind(&self, kind: php_heap::ZvalType) -> u32 {
        crate::exec::SlotType::bit(kind) as u32
    }

    /// The bit of a slot's type that says the property has none; every
    /// kind's bit is set beside it.
    #[must_use]
    pub const fn untyped(&self) -> u32 {
        self.untyped
    }
}

// What compiled code reads of a slot, each at its own alignment.
const _: () = {
    assert!(PLACE.class.is_multiple_of(4) && PLACE.slot.is_multiple_of(4));
    assert!(PLACE.ty.is_multiple_of(2) && PLACE.unprimed != PLACE.property);
    assert!(PLACE.pair_class.is_multiple_of(4) && PLACE.pair_function.is_multiple_of(4));
    assert!(PLACE.pair_record.is_multiple_of(8));
    assert!(PLACE.pair != PLACE.unprimed && PLACE.pair != PLACE.property);
};

/// A call to a user function as `push_call` laid it down and as a
/// `DO_*CALL`'s inline path activates it, or to a native as `INIT_FCALL`
/// pushes it, taken by a recording where the call stood built: what
/// compiled code writes in place for the same callee and argument count,
/// as php's trace writes a recorded `func`'s call
/// (`zend_jit_push_call_frame`, `Z/ext/opcache/jit/zend_jit_ir.c:8466-8650`, which pushes a native's as a user
/// function's). Its header is the one [`call_image`] lays out for the
/// request the code runs in, since the function's number and its record
/// are the request's.
#[derive(Debug, Clone)]
pub struct PlainCall {
    serves: Serves,
    count: u32,
    /// The function's name as the function table holds it, lowered.
    name: Box<[u8]>,
    slots: u32,
    activation: Option<Activation>,
    checks: bool,
    /// A method's call: the class of its receiver, its called scope, and
    /// the method its methods answered. The class is published, or the
    /// request's own, whose number the recording's request gave it and
    /// compiled code reads where it runs.
    method: Option<(ClassId, Method)>,
}

/// What a plain call serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Serves {
    /// A native, which the call's name names.
    Native,
    /// A user function's op array, published.
    Published(FuncRef),
    /// An op array of the request's own, an evaluated class's own
    /// method's: no number names it past the request, so each request
    /// finds it under the call's names, and the call is held to the
    /// room the recording's took ([`call_image`]; #3570).
    Request,
}

/// The method a method's plain call serves, as a request that runs the
/// code finds it ([`call_image`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Method {
    /// A published method, the same number in every request of the epoch.
    Published(Function),
    /// A request-scoped class's own method, `zend_lazy_class_load`'s
    /// copy's, an enum's, an evaluated class's: numbered by each request
    /// for itself, so found in the request that runs the code under the
    /// name of the class that answered it, lowered, and the call's own
    /// name, and held there to the op array the call serves.
    Own(Box<[u8]>),
}

/// Two calls of one callee, one argument count and one called scope are
/// the same call: a published user function by its op array, a native
/// and a request's own op array by its name, and a request-scoped class's
/// own method by its class's name too.
impl PartialEq for PlainCall {
    fn eq(&self, other: &PlainCall) -> bool {
        (self.serves, self.count) == (other.serves, other.count)
            && self.scope() == other.scope()
            && self.own_class() == other.own_class()
            && (self.callee().is_some() || self.name == other.name)
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
    /// The published op array the call serves; none for a native's call
    /// and a request's own op array's.
    #[must_use]
    pub fn callee(&self) -> Option<FuncRef> {
        match self.serves {
            Serves::Published(f) => Some(f),
            Serves::Native | Serves::Request => None,
        }
    }

    /// Whether the call serves a native.
    #[must_use]
    pub fn is_native(&self) -> bool {
        self.serves == Serves::Native
    }

    /// Whether the call serves an op array of the request's own, one an
    /// `eval` declared, which no trace names (`jit-design.md` §4.1).
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.serves == Serves::Request
    }

    /// The callee's name as the function table holds it, lowered.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
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
    /// path makes it with no `RECV*` to serve, with `RECV`s that only
    /// check ([`PlainCall::checks`]), or with the `RECV_INIT`s of the
    /// parameters past [`PlainCall::count`], whose defaults compiled code
    /// copies or leaves the call to its kernel; none where the callee's
    /// activation serves its prologue or reads a cache of its own.
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

    /// A method's call: the class of the receiver it was pushed over,
    /// which its called scope is and whose table answered the method;
    /// none for a function's. A class that is not
    /// [`ClassId::is_published`] is the recording's request's own, the
    /// one its number names there alone: compiled code that pushes the
    /// call writes the called scope it reads in the request it runs in
    /// ([`FrameLayout::called_scope`]), and the header [`call_image`]
    /// lays out names none.
    #[must_use]
    pub fn scope(&self) -> Option<ClassId> {
        self.method.as_ref().map(|(class, _)| *class)
    }

    /// The name, lowered, of the request-scoped class whose own method the
    /// call serves, under which each request finds the method for itself
    /// ([`call_image`]); none for a function's call and a published
    /// method's.
    #[must_use]
    pub fn own_class(&self) -> Option<&[u8]> {
        match &self.method {
            Some((_, Method::Own(class))) => Some(class),
            _ => None,
        }
    }
}

/// The plain call `call` is, the record on top of the stack being built:
/// a call to a user function whose `INIT_*` found its entry plain, or to a
/// native of the engine's table, given no receiver, closure, scope or
/// annex, with no observer declared; or to a method of a class a
/// published method answered, over a receiver of that class or none,
/// the class its called scope, as `INIT_METHOD_CALL`,
/// `INIT_STATIC_METHOD_CALL` and `NEW` push it: a published class, or a
/// request-scoped one whose method is published (#3558), one it inherits
/// from a published class, or its own (#3561), the request's, numbered
/// for it alone as php's `zend_lazy_class_load` copies each into the
/// request ([`Method::Own`]); none for any other.
#[must_use]
pub fn plain_call(req: &Request, call: FrameRef) -> Option<PlainCall> {
    let (top, laid) = req.vm().stack.top()?;
    let header = laid.header();
    let function = header.function?;
    let plain = top == call
        && header.info.building()
        && header.closure.is_none()
        && header.annex.is_none()
        && !crate::observer::observing(req);
    if !plain {
        return None;
    }
    let method = match (header.called_scope(), header.this()) {
        (None, None) => None,
        (Some(class), this) if this.is_none_or(|this| this.class() == class) => {
            let method = if function.published_number().is_some() {
                Method::Published(function)
            } else {
                let name = req.symbols().class_name(class).to_ascii_lowercase();
                Method::Own(name.into_boxed_slice())
            };
            Some((class, method))
        }
        _ => return None,
    };
    let name = || {
        let decl = req.symbols().function_decl(function)?;
        Some(decl.name.0.to_ascii_lowercase().into_boxed_slice())
    };
    let user = req.user_function(function);
    if method.is_some() && user.is_none() {
        return None;
    }
    let Some(user) = user else {
        // A native's call: its arguments' room and its record, as
        // `INIT_FCALL` pushes `fbc` (`init_native`).
        req.native_entry(function)?;
        req.shared.native_callee(function)?;
        return Some(PlainCall {
            serves: Serves::Native,
            count: header.num_args,
            name: name()?,
            slots: u32::try_from(laid.len()).ok()?,
            activation: None,
            checks: false,
            method: None,
        });
    };
    if !header.info.plain_entry() {
        return None;
    }
    let code = crate::exec::body_of(&user.unit, user.body)?;
    let name = name()?;
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
    // One `RECV*` per parameter: a function that declares a type keeps
    // the passed ones' checks, and one passed fewer arguments than it has
    // parameters keeps the `RECV_INIT`s of the rest, whose defaults
    // compiled code copies where it can, as php's trace compiles
    // `zend_jit_recv_init` (`Z/ext/opcache/jit/zend_jit_ir.c:10886-10950`).
    let prologue = shape.entry == shape.parameters;
    let checks = shape.typed && count <= shape.parameters && prologue;
    let defaults = count < shape.parameters && prologue;
    let enters = count <= shape.parameters
        && slots >= shape.slots
        && (opline == Some(shape.entry) || checks || defaults);
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
    let served = FuncRef::at(code.runtime_cache);
    Some(PlainCall {
        serves: match served.offset() {
            MapPtrOffset::Engine(_) => Serves::Published(served),
            MapPtrOffset::Request(_) => Serves::Request,
        },
        count,
        name,
        slots,
        activation,
        checks,
        method,
    })
}

/// The header `push_call` lays down for `call` in `req`, as compiled code
/// that pushes it in place copies it: the function the request's table
/// holds under the call's name, where it serves the call's op array, its
/// record, the flags its `INIT_*` writes, and a published class as its
/// called scope, where compiled code writes a request-scoped one. None where the request has
/// declared no such function yet, its entry is no longer plain, or its
/// room is not the one the recording's request laid.
#[must_use]
pub fn call_image(req: &Request, call: &PlainCall) -> Option<HeaderImage> {
    let function = match &call.method {
        Some((_, Method::Published(method))) => *method,
        Some((_, Method::Own(class))) => {
            let class = req.class_named(Bare::resolved(class))?;
            req.symbols().method_named(class, &call.name)?
        }
        None => req.function_named(Bare::resolved(&call.name))?,
    };
    let served = if let Some(user) = req.user_function(function) {
        let code = crate::exec::body_of(&user.unit, user.body)?;
        match code.runtime_cache {
            MapPtrOffset::Engine(_) => Serves::Published(FuncRef::at(code.runtime_cache)),
            MapPtrOffset::Request(_) => Serves::Request,
        }
    } else {
        req.native_entry(function)?;
        Serves::Native
    };
    if served != call.serves {
        return None;
    }
    let mut header = Header::call(function, call.count);
    header.set_called_scope(call.scope().filter(|class| class.is_published()));
    let (room, header) = req.laid_call(header)?;
    // The room compiled code lays is the recording's: a request's own op
    // array another request found under the same names may take another.
    if u32::try_from(room).ok() != Some(call.slots) {
        return None;
    }
    // A header with no record of its callee is one `push_call` completes
    // as it lays it down, and so not one to copy.
    let plain = call.is_native() || header.info.plain_entry();
    if !plain || header.callee.is_none() {
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
