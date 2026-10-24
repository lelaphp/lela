//! The VM stack (design §5.6): php's `zend_vm_stack`, pages of
//! sixteen-byte slots taken from the request's heap, so the frames a
//! program pushes are memory `memory_limit` bounds and
//! `memory_get_usage()` reads (design §5.13: the allocator is the only
//! accountant); and the entry slot the loop reads at every `RETURN*` to
//! know whether the frame returning is the entry frame of the innermost
//! serving (`jit-design.md` §2.2).
//!
//! The arithmetic is php's, because the pages are what a deep call
//! chain moves `memory_get_usage()` by and what an unbounded one is
//! refused on. A page is 256 KiB, `ZEND_VM_STACK_PAGE_SIZE`, or 16 KiB
//! on a fiber's stack, `ZEND_FIBER_VM_STACK_SIZE`, and starts with the
//! three words of its own header; a frame takes `ZEND_CALL_FRAME_SLOT`
//! slots for php's `zend_execute_data`, then the CVs and the
//! temporaries, then its extra arguments, which is
//! `zend_vm_calc_used_stack`. A frame that does not fit on the page
//! takes a page of its own, `zend_vm_stack_extend`, and gives it back
//! when it pops, `zend_vm_stack_free_call_frame`; so a program that
//! recurses without end asks the heap for 262144 bytes it may not have,
//! and ends in php's fatal naming them.
//!
//! A frame's room starts with its header, php's `zend_execute_data`:
//! the engine's typed record of the call, `H`, laid in the
//! `ZEND_CALL_FRAME_SLOT` slots php's header takes, so the record and
//! the slots after it are one piece of memory pushed and popped
//! together, and the pages fill at the depths php's fill. The header
//! is the engine's type and the heap knows of it only that it fits
//! the room and owns nothing a drop would give back; the slots of the
//! extra arguments past a frame's addressable slots are laid down and
//! not addressed.
//!
//! A page is an allocation of the heap the stack is pushed and popped
//! with, and one stack is only ever given the heap of the request it is
//! the stack of: a frame's slots are live while the frame is on the
//! stack and that heap stands. Nothing gives a page back but
//! [`VmStack::pop_frame`] and [`VmStack::close`]; a stack dropped
//! without closing leaves its pages to the heap's own end, which is
//! php's mass release.
//!
//! An unobserved frameless call's frame, which php lays down nowhere,
//! stands off the pages and off the heap ([`VmStack::push_frameless`]),
//! so it takes no room php's call does not.
//!
//! **A chain another process can read** (design §5.6, revision 59). The
//! room's first word is the stack's, not the header's: the address of
//! the room of the record pushed before it, php's `prev_execute_data`,
//! and the stack keeps the innermost room's address in one word
//! ([`VmStack::link_word`]) that a push moves only once the room below
//! it is whole. So a process stopped between any two instructions holds
//! a chain of rooms another one can walk from that word, reading bytes
//! as `fpm_php_trace_dump` walks `EG(current_execute_data)`: the header
//! after the link says what each room serves. Every push links its
//! room, a call being built and a native's call as a user frame, and a
//! pop moves the word back to the room's link, so the chain is the
//! stack's records whichever tier the engine is bound to; the one room never linked
//! is an unobserved frameless call's, which php's chain does not hold
//! either (#2553, #2665).

use std::marker::PhantomData;
use std::mem::{MaybeUninit, align_of, needs_drop, offset_of, size_of};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

use crate::hooks::Fatal;
use crate::{Heap, Zval, ZvalRef};

mod frames;

use frames::Frames;
pub use frames::FramesLayout;

/// `sizeof(zval)`: a slot.
const SLOT: usize = size_of::<Zval>();
/// `ZEND_VM_STACK_PAGE_SIZE` (`Z/Zend/zend_execute.c:186`):
/// `ZEND_VM_STACK_PAGE_SLOTS`, sixteen thousand slots.
const PAGE_SIZE: usize = 16 * 1024 * SLOT;
/// `ZEND_FIBER_VM_STACK_SIZE` (`Z/Zend/zend_fibers.h:29`).
const FIBER_PAGE_SIZE: usize = 1024 * SLOT;
/// `ZEND_VM_STACK_HEADER_SLOTS`: `top`, `end` and `prev`, three words
/// rounded up to slots.
const PAGE_HEADER_SLOTS: usize = (3 * size_of::<usize>()).div_ceil(SLOT);
/// `ZEND_CALL_FRAME_SLOT`: `sizeof(zend_execute_data)`, eighty bytes on
/// a 64-bit build, rounded up to slots.
const FRAME_HEADER_SLOTS: usize = 80usize.div_ceil(SLOT);
/// The bytes of a frame's room before its first CV, php's
/// `zend_execute_data`: the link and then the header. What a reader of
/// the chain reads at each room's address.
pub const ROOM_BYTES: usize = FRAME_HEADER_SLOTS * SLOT;
/// Where in the room the engine's header starts: past the link, the
/// address of the room linked before this one (the room's first word).
pub const HEADER_AT: usize = size_of::<usize>();
/// The operands a frameless call passes at most, a
/// `ZEND_FRAMELESS_ICALL_3`'s.
const LOOSE_OPERANDS: usize = 3;
/// The room a frameless call's frame takes, never more: the header's
/// and three operands (design §5.13).
const LOOSE_SLOTS: usize = FRAME_HEADER_SLOTS + LOOSE_OPERANDS;

/// A slot index within one frame: a CV, a temporary or an extra argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot(pub u32);

/// A frame's position on the stack; frames never move, so the position
/// is the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameRef(pub u32);

/// Whose stack this is, which is what php's `EG(vm_stack_page_size)`
/// and the bottom of the first page say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Kind {
    /// The request's own, `zend_vm_stack_init`.
    #[default]
    Request,
    /// A fiber's, laid down by `zend_fiber_execute`.
    Fiber,
}

impl Kind {
    /// The size of every page but those a frame too large for one opens.
    const fn page_size(self) -> usize {
        match self {
            Kind::Request => PAGE_SIZE,
            Kind::Fiber => FIBER_PAGE_SIZE,
        }
    }

    /// The slots the first page holds below every frame: none on the
    /// request's, and on a fiber's the `zend_execute_data` of
    /// `zend_fiber_function` that `zend_fiber_execute` places there
    /// (`EG(vm_stack_top) = stack->top + ZEND_CALL_FRAME_SLOT`).
    const fn bottom(self) -> usize {
        match self {
            Kind::Request => 0,
            Kind::Fiber => FRAME_HEADER_SLOTS,
        }
    }
}

/// Every cell `UNDEF`, as `i_init_func_execute_data` leaves the CVs a
/// call did not pass: a frame of a few slots, nearly every one, written
/// by stores of a known width in place, and only a larger one through
/// `memset`, which costs more than the stores for a frame of one or
/// two slots.
#[inline(always)] // once per push, in the push's inline path
#[allow(clippy::inline_always)]
fn undefine(cells: &mut [Zval]) {
    match cells.len() {
        0 => {}
        1 => undefine_first::<1>(cells),
        2 => undefine_first::<2>(cells),
        3 => undefine_first::<3>(cells),
        4 => undefine_first::<4>(cells),
        5 => undefine_first::<5>(cells),
        6 => undefine_first::<6>(cells),
        7 => undefine_first::<7>(cells),
        8 => undefine_first::<8>(cells),
        _ => cells.fill(Zval::UNDEF),
    }
}

/// The first `N` cells `UNDEF`, a width the stores know.
#[inline(always)] // as `undefine`
#[allow(clippy::inline_always)]
fn undefine_first<const N: usize>(cells: &mut [Zval]) {
    if let Some(first) = cells.first_chunk_mut::<N>() {
        *first = [Zval::UNDEF; N];
    }
}

/// One `zend_vm_stack` page: `slots` slots at `at`, the first
/// [`PAGE_HEADER_SLOTS`] of them the header's. While it is the top page,
/// the stack's own two words say where its frames end and it ends
/// (`EG(vm_stack_top)`, `EG(vm_stack_end)`); `saved` is the first slot
/// no frame held when a page above it opened, php's
/// `EG(vm_stack)->top` as `zend_vm_stack_extend` writes it, which the
/// stack's top goes back to once that page is given back.
#[derive(Debug)]
struct Page {
    at: NonNull<Zval>,
    slots: usize,
    saved: usize,
}

/// Where a frame stands: on the top page, its room given back to it by
/// the frame's pop (`EG(vm_stack_top) = call`); at the start of a page
/// it opened, php's `ZEND_CALL_ALLOCATED`, which the pop gives back; in
/// an allocation of its own beginning at `block`, the generator's
/// `emalloc(used_stack)`; or in a room off the pages and off the
/// request's heap ([`Loose`]). `#[repr(u8)]`, so a record's placement
/// begins with its tag and [`ON_PAGE`] is the tag of the first, which
/// compiled code writes for a frame it pushes and reads for one it pops
/// ([`FramesLayout`]).
#[derive(Debug)]
#[repr(u8)]
enum Placed {
    OnPage = ON_PAGE,
    Opened = 1,
    Heap { block: NonNull<Zval> } = 2,
    Loose(Loose) = 3,
}

/// The tag of [`Placed::OnPage`].
const ON_PAGE: u8 = 0;

impl Placed {
    /// Whether the frame stands on a page of the stack's.
    fn on_page(&self) -> bool {
        matches!(self, Placed::OnPage | Placed::Opened)
    }
}

/// The room of a frame php lays down nowhere, an unobserved frameless
/// call's (`ZEND_FRAMELESS_ICALL_*` hands its native the operands where
/// they stand and pushes no frame): slots of the process's allocator
/// and not of the request's heap, so the frame opens no page, moves
/// neither `memory_get_usage()` nor its peak, and meets no
/// `memory_limit`, as php's call meets none. A room holds a header and
/// three operands and never more, one per such call in flight, and
/// the stack keeps the last one given back for the next call
/// ([`VmStack::push_frameless`]); a room is freed when it is dropped.
/// The frame stands among the stack's frames, the chain a
/// backtrace walks, as php's backtrace adds a frame for the call
/// (`Z/Zend/zend_builtin_functions.c:1984`), and is never linked into
/// the chain another process reads (design §5.6), which php's call is
/// not on.
#[derive(Debug)]
struct Loose(NonNull<[MaybeUninit<Zval>; LOOSE_SLOTS]>);

// SAFETY: a room is the request's, as a page is, and read on its
// thread alone.
unsafe impl Send for Loose {}

impl Loose {
    /// A room of `LOOSE_SLOTS` slots, untyped as a page's are until a
    /// push writes the header and the slots.
    fn new() -> Loose {
        Loose(NonNull::from(Box::leak(Box::new(
            [const { MaybeUninit::uninit() }; LOOSE_SLOTS],
        ))))
    }

    /// The room's first slot.
    fn at(&self) -> NonNull<Zval> {
        self.0.cast()
    }
}

impl Drop for Loose {
    fn drop(&mut self) {
        // SAFETY: the room is the box `Loose::new` leaked, and this
        // value, its one owner, is dropped once.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

/// One call frame: its header `H`, the engine's record of the call laid
/// in the room before the slots, and its slots, typed and
/// bounds-checked. A slot past the end reads as `UNDEF` and a write to
/// it is dropped, as an uninitialised zval reads. `#[repr(C)]`: it is one
/// record of the stack's frame table, whose fields compiled code reads
/// and writes in place ([`FramesLayout`]).
#[derive(Debug)]
#[repr(C)]
pub struct Frame<H> {
    /// The frame's first CV, in the room its push gave it; the header
    /// stands in the
    /// [`FRAME_HEADER_SLOTS`] slots before it.
    at: NonNull<Zval>,
    len: usize,
    placed: Placed,
    header: PhantomData<H>,
}

// SAFETY: a frame's header and slots are the request's, moved with it
// and read on its thread alone, as a `Block`'s bytes are; the header is
// `Send` by the bound.
unsafe impl<H: Send> Send for Frame<H> {}

// SAFETY: as a frame's, a page is the request's and read on its thread.
unsafe impl Send for Page {}

impl<H> Frame<H> {
    /// What the room holds past its link: the header no larger than
    /// what php's `zend_execute_data` leaves after the link's word and
    /// aligned no more strictly than that word, so it sits in the slots
    /// before the frame's first CV, and owning nothing a drop would
    /// give back, since a pop frees the room without reading it.
    const FITS: () = assert!(
        size_of::<H>() <= ROOM_BYTES - HEADER_AT
            && align_of::<H>() <= align_of::<usize>()
            && !needs_drop::<H>(),
        "a frame header must fit php's zend_execute_data room past its link and own nothing"
    );

    /// The room, the `FRAME_HEADER_SLOTS` slots before the first CV:
    /// its link, then its header.
    fn room(&self) -> NonNull<Zval> {
        // SAFETY: every frame's `at` is `FRAME_HEADER_SLOTS` slots past
        // the start of its room, inside the page, the block or the room
        // off the pages it was laid in.
        unsafe { self.at.sub(FRAME_HEADER_SLOTS) }
    }

    /// The room's address, which the link of the room above names and
    /// the stack's word names while this is the innermost.
    fn room_address(&self) -> usize {
        self.room().as_ptr().expose_provenance()
    }

    /// The link's place, the room's first word.
    fn link_at(&self) -> NonNull<usize> {
        self.room().cast::<usize>()
    }

    /// The header's place, past the link.
    fn header_at(&self) -> NonNull<H> {
        // SAFETY: the room is `ROOM_BYTES` long and the header starts
        // `HEADER_AT` bytes in, inside it.
        unsafe { self.room().cast::<u8>().add(HEADER_AT) }.cast::<H>()
    }

    /// The address of the room linked before this one, which the push
    /// wrote.
    fn linked_below(&self) -> usize {
        // SAFETY: the push wrote the link before it linked the room, and
        // the room is the frame's until it pops.
        unsafe { self.link_at().read() }
    }

    /// Whether the frame's room is on the chain another process reads:
    /// every room but an unobserved frameless call's.
    fn linked(&self) -> bool {
        !matches!(self.placed, Placed::Loose(_))
    }

    /// The frame's header, php's `execute_data` fields.
    #[must_use]
    pub fn header(&self) -> &H {
        // SAFETY: the push wrote a whole `H` there (`FITS` holds its size
        // and alignment) and nothing but this frame names the room, so a
        // borrow of the frame is a borrow of the header.
        unsafe { self.header_at().as_ref() }
    }

    /// The header copied out, the frame's own left where it stands:
    /// what a stack that lets the frame go hands back.
    fn header_copied(&self) -> H {
        // SAFETY: the header owns nothing (`FITS`), so the bytes copied
        // are the whole of it and leave no second owner behind; the push
        // or the move wrote the room.
        unsafe { self.header_at().read() }
    }

    /// The frame's header, to write.
    pub fn header_mut(&mut self) -> &mut H {
        // SAFETY: as `header`, and the mutable borrow of the frame is the
        // one writer.
        unsafe { self.header_at().as_mut() }
    }

    /// A frame no stack holds given back: the allocation of its own a
    /// generator's frame stands in, `efree(execute_data)`, and nothing
    /// for a frame on a page, whose room its stack gives back. Its
    /// values are the caller's to have released.
    pub fn free(self, heap: &mut Heap) {
        if let Placed::Heap { block } = self.placed {
            // SAFETY: the block came from this heap's `alloc` in
            // `move_to_heap`, and this frame, taken by value, was its last
            // name, so it is freed once.
            unsafe { heap.mm.free(block.cast()) };
        }
    }

    /// How many slots the frame has: the CVs and the temporaries, which
    /// is what `i_free_compiled_variables` and the temporaries' release
    /// walk when the frame leaves.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the frame has no slots at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The zval in `slot`, copied.
    #[must_use]
    pub fn slot(&self, slot: Slot) -> Zval {
        self.cells()
            .get(slot.0 as usize)
            .copied()
            .unwrap_or(Zval::UNDEF)
    }

    /// Write `z` into `slot`.
    pub fn set(&mut self, slot: Slot, z: Zval) {
        if let Some(cell) = self.cells_mut().get_mut(slot.0 as usize) {
            *cell = z;
        }
    }

    /// The zval in `slot` in place, php's `EX_VAR(var)`: what a
    /// `FETCH_*_W` points its `VAR` at and what the `ASSIGN*` over that
    /// `VAR` reads through (design §5.6). The page never moves while
    /// the frame lives, so the name holds as long as the frame does.
    /// None for a slot the frame does not have.
    pub fn slot_ptr(&mut self, slot: Slot) -> Option<ZvalRef> {
        self.cells_mut()
            .get_mut(slot.0 as usize)
            .map(|cell| ZvalRef::new(NonNull::from(cell)))
    }

    /// Every slot as it stands, the CVs and then the temporaries and the
    /// extra arguments past them.
    #[must_use]
    pub fn slots(&self) -> &[Zval] {
        self.cells()
    }

    /// Every slot in place, php's `EX_VAR` base: the CVs, then the
    /// temporaries past them. What a handler's inline path reads its
    /// operands from and writes its temporary into with one lookup of the
    /// frame for all three, as php's handler indexes off `execute_data`
    /// in a register (design §5.8).
    pub fn slots_mut(&mut self) -> &mut [Zval] {
        self.cells_mut()
    }

    fn cells(&self) -> &[Zval] {
        // SAFETY: the frame's `len` slots at `at` are inside the page, the
        // block or the room off the pages it was laid in, written `UNDEF`
        // by the push and live until the frame pops, and the borrow of
        // the frame is a borrow of the stack that alone names them.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    fn cells_mut(&mut self) -> &mut [Zval] {
        // SAFETY: as `cells`, and the mutable borrow of the frame is the
        // one writer.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

/// The words of a room past its link, php's `zend_execute_data` past
/// `prev_execute_data`.
pub const HEADER_WORDS: usize = (ROOM_BYTES - HEADER_AT) / size_of::<u64>();

/// A frame header's bytes as a push lays them in a room, copied out of
/// the header and never read as one again: what code that pushes the same
/// call in place copies into its room, from the address this holds for
/// as long as it lives (`jit-design.md` §5.3). Bytes and nothing else, so
/// it is shared across threads as the code that reads it is.
#[derive(Debug, Clone)]
pub struct HeaderImage(Box<[MaybeUninit<u64>; HEADER_WORDS]>);

impl HeaderImage {
    /// The bytes of `header`, a header a push lays in a room whole, past
    /// the link ([`Frame`]'s `FITS`); none for a header of another size.
    #[must_use]
    pub fn of<H>(header: &H) -> Option<HeaderImage> {
        if size_of::<H>() != HEADER_WORDS * size_of::<u64>() || needs_drop::<H>() {
            return None;
        }
        let mut words = Box::new([MaybeUninit::<u64>::uninit(); HEADER_WORDS]);
        // SAFETY: the header is `size_of::<H>()` bytes, as many as the
        // image's words were just allocated for, and the two do not
        // overlap; an untyped copy carries whatever the bytes are, and no
        // code reads them as a header again.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(header).cast::<u8>(),
                words.as_mut_ptr().cast::<u8>(),
                size_of::<H>(),
            );
        };
        Some(HeaderImage(words))
    }

    /// The address of the image's first byte.
    #[must_use]
    pub fn address(&self) -> usize {
        self.0.as_ptr().addr()
    }
}

/// A room no frame stands in, held where its owner holds it and not on
/// the stack's pages: php's `execute_fake`, the `zend_execute_data` a
/// generator object carries as a field, which a resumption through a
/// `yield from` links between the frame it runs and the resumer's
/// ([`VmStack::link_room`]). Its link and its header are laid as a
/// frame's room's are, `ROOM_BYTES` in all, so another process reads it
/// as it reads any room on the chain.
#[derive(Debug)]
#[repr(C)]
pub struct FakeRoom<H> {
    link: usize,
    header: H,
}

impl<H> FakeRoom<H> {
    /// The header fills the room past its link exactly, so a reader that
    /// reads `ROOM_BYTES` reads this room and nothing past it, and owns
    /// nothing, as a frame's header owns nothing.
    const FITS: () = assert!(
        size_of::<H>() == ROOM_BYTES - HEADER_AT
            && align_of::<H>() <= align_of::<usize>()
            && !needs_drop::<H>(),
        "a fake room's header must fill php's zend_execute_data room past its link and own nothing"
    );

    /// A room of `header`, linked to nothing until the stack links it.
    #[must_use]
    pub const fn new(header: H) -> FakeRoom<H> {
        let () = Self::FITS;
        FakeRoom { link: 0, header }
    }

    /// The room's address, which the link of the room above it names and
    /// the stack's word names while it is the innermost.
    fn address(&self) -> usize {
        std::ptr::from_ref(self).expose_provenance()
    }
}

/// The stack of one request, or of one fiber: pages of slots and the
/// entry slot.
///
/// The words a push and a pop move, the top page's top and end, the frame
/// table and the innermost room's address, stand where [`StackLayout`]
/// says, so compiled code that pushes and pops a plain call's frame
/// inline moves them as [`VmStack::push_frame`] and [`VmStack::pop_frame`]
/// do (`jit-design.md` §5.3, #3230).
#[derive(Debug)]
pub struct VmStack<H> {
    kind: Kind,
    /// php's `EG(vm_stack)` and every page its `prev` reaches, oldest
    /// first; frames are pushed on the last.
    pages: Vec<Page>,
    /// `EG(vm_stack_top)`: the first slot of the top page no frame holds;
    /// dangling, and `end` with it, while no page stands.
    top: NonNull<Zval>,
    /// `EG(vm_stack_end)`: one past the top page's last slot.
    end: NonNull<Zval>,
    frames: Frames<H>,
    entries: Vec<FrameRef>,
    /// The room off the pages the last frameless call's frame gave
    /// back, the next one's.
    spare: Option<Loose>,
    /// `EG(current_execute_data)` as another process reads it: the
    /// address of the innermost linked room, zero where none stands
    /// ([`VmStack::link_word`]). An atomic for its one store of a whole
    /// word: only this thread writes it, and a reader reads it with the
    /// thread stopped, so `Relaxed` with the compiler fence before each
    /// move ([`VmStack::link`]) is the order it needs.
    innermost: AtomicUsize,
}

// SAFETY: the stack's top and end name its own top page, which is the
// request's or the fiber's and moves with it, read on its thread alone,
// as the pages and the frames they name are.
unsafe impl<H: Send> Send for VmStack<H> {}

/// Where a [`VmStack`]'s words stand in it, and its frame table's in the
/// table, for code that pushes and pops a frame in place: offsets in
/// bytes, each computed by `offset_of!` over the stack's own declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackLayout {
    /// `EG(vm_stack_top)`, a slot's address.
    pub top: usize,
    /// `EG(vm_stack_end)`, a slot's address.
    pub end: usize,
    /// The frame table, a [`FramesLayout`] from here.
    pub frames: usize,
    /// The innermost linked room's address ([`VmStack::link_word`]).
    pub innermost: usize,
    /// The frame table's words and a record's fields.
    pub table: FramesLayout,
}

impl<H> Default for VmStack<H> {
    fn default() -> VmStack<H> {
        VmStack {
            kind: Kind::Request,
            pages: Vec::new(),
            top: NonNull::dangling(),
            end: NonNull::dangling(),
            frames: Frames::new(),
            entries: Vec::new(),
            spare: None,
            innermost: AtomicUsize::new(0),
        }
    }
}

impl<H> VmStack<H> {
    /// The request's stack, no page laid down yet: [`VmStack::open`]
    /// lays the first, and so does the first push.
    #[must_use]
    pub fn new() -> VmStack<H> {
        VmStack::default()
    }

    /// A fiber's stack: 16 KiB pages, the first holding the fiber
    /// function's frame at its bottom.
    #[must_use]
    pub fn fiber() -> VmStack<H> {
        VmStack {
            kind: Kind::Fiber,
            ..VmStack::default()
        }
    }

    /// `zend_vm_stack_init`, or the page `zend_fiber_execute` takes: the
    /// first page, out of `heap`, where none stands yet.
    pub fn open(&mut self, heap: &mut Heap) -> Result<(), Fatal> {
        if !self.pages.is_empty() {
            return Ok(());
        }
        let mut page = Page::take(heap, self.kind.page_size())?;
        page.saved += self.kind.bottom();
        self.pages.push(page);
        self.stand_on_top();
        Ok(())
    }

    /// Where the stack's words, its frame table's and a record's fields
    /// stand, for code that pushes and pops a frame in place.
    pub const LAYOUT: StackLayout = StackLayout {
        top: offset_of!(VmStack<H>, top),
        end: offset_of!(VmStack<H>, end),
        frames: offset_of!(VmStack<H>, frames),
        innermost: offset_of!(VmStack<H>, innermost),
        table: Frames::<H>::LAYOUT,
    };

    /// The slots the top page has left for frames, `EG(vm_stack_end) -
    /// EG(vm_stack_top)` over `sizeof(zval)`: none while no page stands.
    #[inline]
    fn left(&self) -> usize {
        let (top, end) = (self.top.as_ptr().addr(), self.end.as_ptr().addr());
        end.saturating_sub(top) / SLOT
    }

    /// The top and the end made the top page's, where its frames stood
    /// when a page above it opened (`zend_vm_stack_free_call_frame_ex`'s
    /// `EG(vm_stack_top) = prev->top`); both dangling where no page
    /// stands.
    fn stand_on_top(&mut self) {
        if let Some(page) = self.pages.last() {
            self.top = page.slot(page.saved);
            self.end = page.end();
        } else {
            self.top = NonNull::dangling();
            self.end = NonNull::dangling();
        }
    }

    /// `page` laid above the top page, which keeps where its frames end
    /// (`zend_vm_stack_extend`'s `EG(vm_stack)->top = EG(vm_stack_top)`),
    /// and made the top.
    fn extend(&mut self, page: Page) {
        let top = self.top;
        if let Some(below) = self.pages.last_mut() {
            below.saved = below.index_of(top);
        }
        self.pages.push(page);
        self.stand_on_top();
    }

    /// The room of `used` slots at the top taken off the top page, which
    /// has at least that many left: php's `EG(vm_stack_top) += used`. The
    /// first slot past the room's header, where the frame's first CV is.
    #[inline(always)] // part of the push's inline path
    #[allow(clippy::inline_always)]
    fn take_room(&mut self, used: usize) -> NonNull<Zval> {
        let room = self.top;
        debug_assert!(used >= FRAME_HEADER_SLOTS && used <= self.left());
        // SAFETY: the top page has `left` slots from the top, at least
        // `used`, so the end of the room is inside the page or one past it.
        self.top = unsafe { room.add(used) };
        // SAFETY: the room is `used` slots, at least the header's.
        unsafe { room.add(FRAME_HEADER_SLOTS) }
    }

    /// `zend_vm_stack_destroy`: every page given back to `heap`, the
    /// newest first, and every frame with them, the rooms off the pages
    /// and the spare one with those. The headers of the
    /// frames still standing are handed back, the newest first, so what
    /// each names beyond its words is given up where the engine keeps
    /// it; a stack every frame was popped off hands back none.
    #[must_use = "a frame's header names what the engine keeps for it beyond the stack"]
    pub fn close(&mut self, heap: &mut Heap) -> Vec<H> {
        let mut dropped = Vec::with_capacity(self.frames.len());
        while let Some(frame) = self.frames.pop() {
            dropped.push(frame.header_copied());
        }
        self.innermost.store(0, Ordering::Relaxed);
        for page in self.pages.drain(..).rev() {
            page.give_back(heap);
        }
        self.stand_on_top();
        self.spare = None;
        dropped
    }

    /// Push a frame of `slots` addressable slots, with the room of
    /// `extra_args` extra arguments after them, its header the one
    /// `header` makes, and return its position:
    /// `zend_vm_stack_push_call_frame_ex` over
    /// `zend_vm_calc_used_stack`. The header is made once the room is
    /// found, so a push the top page has room for writes it where it
    /// stands, as php's push writes `call->func` and `This` into the
    /// room, and never lays it out anywhere else first. A frame the top
    /// page cannot hold takes a page of its own from `heap`, the stack's
    /// page size or, for a frame larger than that, the page size it
    /// rounds up to; a heap that cannot give it refuses the push with
    /// the fatal php raises.
    ///
    /// So a recursion without end is refused where php's is, asking for
    /// a page, and not after the machine's memory is gone:
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// function f() { f(); }
    /// f();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 262144 bytes) in %s on line 3
    /// ```
    ///
    /// And a deep call chain moves `memory_get_usage()` by the pages it
    /// took, at the depths php takes them, and gives them back as it
    /// returns. A frame of `f` is ten slots, php's five for the header,
    /// one CV and four temporaries, so a page holds 1638 of them.
    ///
    /// ```php
    /// function f($n) { return $n ? f($n - 1) : memory_get_usage(); }
    /// $base = memory_get_usage();
    /// foreach ([10, 1000, 2000, 3000, 20000] as $depth) {
    ///     echo $depth, ': ', intdiv(f($depth) - $base, 262144), "\n";
    /// }
    /// echo intdiv(memory_get_usage() - $base, 262144), "\n";
    /// ```
    /// ```expect
    /// 10: 0
    /// 1000: 0
    /// 2000: 1
    /// 3000: 1
    /// 20000: 12
    /// 0
    /// ```
    #[inline(always)] // laid in its two callers, the engine's push and a native's `INIT_FCALL`
    #[allow(clippy::inline_always)]
    pub fn push_frame(
        &mut self,
        heap: &mut Heap,
        slots: usize,
        extra_args: usize,
        header: impl FnOnce() -> H,
    ) -> Result<FrameRef, Fatal> {
        let used = FRAME_HEADER_SLOTS + slots + extra_args;
        // `zend_vm_stack_push_call_frame_ex`'s test and bump, where the
        // top page has the room, which is every call in a loop.
        if used <= self.left() {
            let at = self.take_room(used);
            return Ok(self.lay(at, slots, Placed::OnPage, header, undefine));
        }
        // The room a page opens is found out of line and the frame laid
        // here, so what makes the header is never handed to the cold
        // half, which would hold it in memory for the hot one too.
        let placed = self.opening(heap, used)?;
        let at = self.take_room(used);
        Ok(self.lay(at, slots, placed, header, undefine))
    }

    /// The room [`VmStack::push_frame`] takes where no page stands yet or
    /// the top one has no room for `used` slots: the first page laid
    /// down, and a page of the frame's own where that one cannot hold it
    /// either (`zend_vm_stack_extend`); where the frame stands then.
    #[cold]
    #[inline(never)]
    fn opening(&mut self, heap: &mut Heap, used: usize) -> Result<Placed, Fatal> {
        self.open(heap)?;
        let placed = if used <= self.left() {
            Placed::OnPage
        } else {
            // `zend_vm_stack_extend`: the stack's page, or for a frame
            // that would not fit one, `ZEND_VM_STACK_PAGE_ALIGNED_SIZE`.
            let page_size = self.kind.page_size();
            let size = used * SLOT;
            let page = if size < page_size - PAGE_HEADER_SLOTS * SLOT {
                page_size
            } else {
                (size + PAGE_HEADER_SLOTS * SLOT).next_multiple_of(page_size)
            };
            let page = Page::take(heap, page)?;
            self.extend(page);
            Placed::Opened
        };
        if used > self.left() {
            return Err(Fatal::error(
                "InternalError: a VM stack with no room for a frame after its push laid a page down",
            ));
        }
        Ok(placed)
    }

    /// A frame of `len` slots from `at` laid on top of the stack where
    /// `placed` says it stands, the header `header` makes written in the
    /// room before `at` and its slots by `cells`, every one `UNDEF` as
    /// `i_init_func_execute_data` finds them where the push has nothing
    /// to place ([`undefine`]); the room is the push's,
    /// `FRAME_HEADER_SLOTS` and `len` slots long, which nothing else names.
    #[inline(always)] // part of the push's inline path, so the header is made in its room
    #[allow(clippy::inline_always)]
    fn lay(
        &mut self,
        at: NonNull<Zval>,
        len: usize,
        placed: Placed,
        header: impl FnOnce() -> H,
        cells: impl FnOnce(&mut [Zval]),
    ) -> FrameRef {
        let () = Frame::<H>::FITS;
        let mut frame = Frame {
            at,
            len,
            placed,
            header: PhantomData,
        };
        // SAFETY: the header's room is the `FRAME_HEADER_SLOTS` slots the
        // push gave the frame before its first CV, which `FITS` holds an
        // `H` to fit, aligned, and nothing else names.
        unsafe { frame.header_at().write(header()) };
        cells(frame.cells_mut());
        self.stand(frame)
    }

    /// `frame` on top of the stack, its position the one it is named by,
    /// and its room linked above the innermost one, unless it is an
    /// unobserved frameless call's.
    fn stand(&mut self, frame: Frame<H>) -> FrameRef {
        if frame.linked() {
            self.link(&frame);
        }
        let at = u32::try_from(self.frames.len()).unwrap_or(u32::MAX);
        self.frames.push(frame);
        FrameRef(at)
    }

    /// `frame`'s room made the innermost: its link the room the word
    /// names, then the word moved to it, php's
    /// `call->prev_execute_data = EG(current_execute_data)` and then
    /// `EG(current_execute_data) = call`, the fence keeping the compiler from moving the
    /// room's bytes past the word, so a stopped process whose word
    /// names a room holds that room whole.
    #[inline]
    fn link(&self, frame: &Frame<H>) {
        let below = self.innermost.load(Ordering::Relaxed);
        // SAFETY: the room is the frame's, laid by its push or its move,
        // and its first word is the link's, which nothing else writes.
        unsafe { frame.link_at().write(below) };
        compiler_fence(Ordering::Release);
        self.innermost
            .store(frame.room_address(), Ordering::Relaxed);
    }

    /// The word moved back to the room `frame` links, as it leaves the
    /// stack: `EG(current_execute_data) = EX(prev_execute_data)`. Read
    /// before the room is given back.
    #[inline]
    fn unlink(&self, frame: &Frame<H>) {
        if frame.linked() {
            self.innermost
                .store(frame.linked_below(), Ordering::Relaxed);
        }
    }

    /// The address of the stack's word, which names the innermost linked
    /// room, `&EG(current_execute_data)`: what a host publishes once per
    /// request for another process to read the chain from. A request's
    /// stack stands in one place for its life, and a fiber's stack
    /// changes places with it at a switch, so the word at this address is
    /// the running stack's.
    #[must_use]
    pub fn link_word(&self) -> usize {
        std::ptr::from_ref(&self.innermost).expose_provenance()
    }

    /// The address the word holds: the innermost linked room's, or zero.
    #[must_use]
    pub fn innermost(&self) -> usize {
        self.innermost.load(Ordering::Relaxed)
    }

    /// The bottom of this stack's chain linked to `below`, the innermost
    /// room of the stack that resumed it, as `zend_fiber_resume` writes
    /// `fiber->stack_bottom->prev_execute_data`: the link of its first
    /// linked room, or, where none stands yet, the word, so the first
    /// push links there.
    pub fn link_below(&mut self, below: usize) {
        match self.frames.iter().find(|frame| frame.linked()) {
            // SAFETY: the room is the standing frame's and its first word
            // the link's.
            Some(bottom) => unsafe { bottom.link_at().write(below) },
            None => self.innermost.store(below, Ordering::Relaxed),
        }
    }

    /// `room` made the innermost, off the pages: its link the room the
    /// word names, then the word moved to it, as `zend_generator_resume`
    /// writes the placeholder's link,
    /// `orig_generator->execute_fake.prev_execute_data`, before the frame
    /// it resumes links to the placeholder
    /// (`Z/Zend/zend_generators.c:807`), so the next push links above it. The room is on the chain until
    /// [`VmStack::unlink_room`], and stands where it is until then; it
    /// takes nothing from the pages.
    #[inline]
    pub fn link_room(&self, room: &mut FakeRoom<H>) {
        room.link = self.innermost.load(Ordering::Relaxed);
        compiler_fence(Ordering::Release);
        self.innermost.store(room.address(), Ordering::Relaxed);
    }

    /// The word moved back to the room `room` links, as php's resumption
    /// moves `EG(current_execute_data)` back to the resumer's once the
    /// frame it linked above the placeholder has left.
    #[inline]
    pub fn unlink_room(&self, room: &FakeRoom<H>) {
        self.innermost.store(room.link, Ordering::Relaxed);
    }

    /// Push the frame of a call php lays down nowhere, an unobserved
    /// frameless call's, its slots the `operands` and the header's room
    /// before them: in a room off the pages and off `heap` ([`Loose`]), the
    /// one the last such frame gave back where it stands spare. php's
    /// `ZEND_FRAMELESS_ICALL_*` pushes no frame unless an observer
    /// watches the function (`Z/Zend/zend_vm_def.h:9775`), so this one
    /// opens no page where the top page has no room for it, reads in
    /// neither `memory_get_usage()` nor its peak, and leaves the pages
    /// as they stood for what the native pushes while it is served.
    /// More than three slots, past what a frameless call passes, is the
    /// engine's error.
    #[inline(always)] // the engine's frameless handler is its one caller
    #[allow(clippy::inline_always)]
    pub fn push_frameless(&mut self, operands: &[Zval], header: H) -> Result<FrameRef, Fatal> {
        let slots = operands.len();
        if slots > LOOSE_OPERANDS {
            return Err(Fatal::error(format!(
                "InternalError: a frameless call's frame of {slots} slots, past the {LOOSE_OPERANDS} operands its room holds"
            )));
        }
        let room = self.spare.take().unwrap_or_else(Loose::new);
        // SAFETY: the room holds `LOOSE_SLOTS` slots, so the first CV's
        // place, `FRAME_HEADER_SLOTS` slots in, is inside it.
        let at = unsafe { room.at().add(FRAME_HEADER_SLOTS) };
        // Written slot by slot, as the handler lays its operands, where a
        // copy of a length the compiler does not know is a call.
        let laid = |cells: &mut [Zval]| match (cells, operands) {
            ([first], [one]) => *first = *one,
            ([first, second], [one, two]) => (*first, *second) = (*one, *two),
            ([first, second, third], [one, two, three]) => {
                (*first, *second, *third) = (*one, *two, *three);
            }
            (cells, _) => undefine(cells),
        };
        Ok(self.lay(at, slots, Placed::Loose(room), || header, laid))
    }

    /// Pop the frame the loop is executing,
    /// `zend_vm_stack_free_call_frame`: the page it stood on keeps its
    /// room for the next push, and a page the frame opened goes back to
    /// `heap`.
    #[inline(always)] // once per return, in the loop's RETURN arms
    #[allow(clippy::inline_always)]
    pub fn pop_frame(&mut self, heap: &mut Heap) {
        // `zend_vm_stack_free_call_frame_ex` over a frame on the top page,
        // `EG(vm_stack_top) = call`: every return of a call in a loop, its
        // room linked as every room on a page is.
        if let Some((below, room)) = self.frames.pop_on_page() {
            self.innermost.store(below, Ordering::Relaxed);
            self.top = room;
            return;
        }
        let Some(frame) = self.frames.pop() else {
            return;
        };
        // An unobserved frameless call's room, which links nothing: kept
        // for the next where none is spare, every frameless call's pop.
        if let Placed::Loose(room) = frame.placed {
            self.spare.get_or_insert(room);
            return;
        }
        self.unlink(&frame);
        self.leave(heap, frame);
    }

    /// The frame on top of the stack, the one the loop is executing or
    /// the call being built, and its position: what [`VmStack::current`]
    /// and [`VmStack::frame`] answer together, read once.
    #[must_use]
    #[inline]
    pub fn top(&self) -> Option<(FrameRef, &Frame<H>)> {
        let at = self.frames.len().checked_sub(1)?;
        let frame = self.frames.last()?;
        Some((FrameRef(u32::try_from(at).ok()?), frame))
    }

    /// The frame on top of the stack, its position and every frame below
    /// it, mutably, from one look at the table: the frame a return leaves
    /// and the one it returns into, which a plain leave reads and writes
    /// together, as php's `zend_leave_helper` reaches `execute_data` and
    /// `EX(prev_execute_data)`.
    #[inline]
    #[allow(clippy::type_complexity)] // three borrows of one look, named where they are taken
    pub fn top_over_mut(&mut self) -> Option<(FrameRef, &mut Frame<H>, &mut [Frame<H>])> {
        let (top, below) = self.frames.split_last_mut()?;
        // A place past `u32::MAX` would be 2^32 frames of at least eighty
        // bytes each, which no request's memory holds.
        #[allow(clippy::cast_possible_truncation)]
        let at = FrameRef(below.len() as u32);
        Some((at, top, below))
    }

    /// The frame on top of the stack, mutably, and its position.
    #[inline]
    pub fn top_mut(&mut self) -> Option<(FrameRef, &mut Frame<H>)> {
        let at = self.frames.len().checked_sub(1)?;
        let frame = self.frames.last_mut()?;
        Some((FrameRef(u32::try_from(at).ok()?), frame))
    }

    /// The frame on top of the stack and the frame at `f` below it, both
    /// mutably, from one look at the table: the call being built and the
    /// frame building it, which a `SEND` reads its operand from and
    /// writes the argument into, as php's handler reaches `EX(call)` and
    /// `EX_VAR` off two registers. None where `f` is the top itself or
    /// no frame stands at it.
    #[inline]
    pub fn top_and_frame_mut(&mut self, f: FrameRef) -> Option<(&mut Frame<H>, &mut Frame<H>)> {
        let (top, below) = self.frames.split_last_mut()?;
        Some((top, below.get_mut(f.0 as usize)?))
    }

    /// The frame on top, `f`, grown by `more` addressable slots, each
    /// `UNDEF`: php's `zend_vm_stack_extend_call_frame`, what a call
    /// whose arguments are unpacked or spread takes when they outnumber
    /// what its `INIT` made room for. The room after the top frame takes
    /// them where the page has more than `more` slots left, php's test;
    /// otherwise the frame is copied, header and slots, to a page of its
    /// own (`zend_vm_stack_copy_call_frame`), its old room given back,
    /// and the page it stood on with it where that page holds nothing
    /// else. A frame that is not the top, or lives in an allocation of
    /// its own, is not grown, and that is the engine's error.
    pub fn grow_top(&mut self, heap: &mut Heap, f: FrameRef, more: usize) -> Result<(), Fatal> {
        let top = self.frames.len().checked_sub(1);
        let Some((on_page, len, room)) = self
            .frames
            .last()
            .filter(|_| top == Some(f.0 as usize))
            .map(|frame| (frame.placed.on_page(), frame.len, frame.room()))
        else {
            return Err(Fatal::error(
                "InternalError: a VM stack frame grown that is not the top one",
            ));
        };
        if !on_page || self.pages.is_empty() {
            return Err(Fatal::error(
                "InternalError: a frame off the VM stack's pages grown on it",
            ));
        }
        if self.left() > more {
            // SAFETY: the top page has more than `more` slots left past
            // the top.
            self.top = unsafe { self.top.add(more) };
        } else {
            let used = FRAME_HEADER_SLOTS + len + more;
            let page_size = self.kind.page_size();
            let size = used * SLOT;
            let size = if size < page_size - PAGE_HEADER_SLOTS * SLOT {
                page_size
            } else {
                (size + PAGE_HEADER_SLOTS * SLOT).next_multiple_of(page_size)
            };
            let mut page = Page::take(heap, size)?;
            let start = page.saved;
            page.saved += used;
            // SAFETY: the old room's header and `len` slots are the frame's,
            // inside its page; the new page was just allocated for `used`
            // slots past its header, so the two do not overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    room.as_ptr(),
                    page.slot(start).as_ptr(),
                    FRAME_HEADER_SLOTS + len,
                );
            }
            let at = page.slot(start + FRAME_HEADER_SLOTS);
            // The copy carried the link; the word follows the room before
            // the old one can go back with its page.
            compiler_fence(Ordering::Release);
            self.innermost.store(
                page.slot(start).as_ptr().expose_provenance(),
                Ordering::Relaxed,
            );
            let bottom = if self.pages.len() == 1 {
                self.kind.bottom()
            } else {
                0
            };
            // The old room goes back to its page, which goes back to the
            // heap where it then holds no frame.
            self.top = room;
            let emptied = self
                .pages
                .last()
                .is_some_and(|below| below.index_of(room) == PAGE_HEADER_SLOTS + bottom);
            if emptied && let Some(below) = self.pages.pop() {
                below.give_back(heap);
                self.stand_on_top();
            }
            self.extend(page);
            if let Some(frame) = self.frames.last_mut() {
                frame.at = at;
                frame.placed = Placed::Opened;
            }
        }
        if let Some(frame) = self.frames.last_mut() {
            frame.len = len + more;
            if let Some(added) = frame.cells_mut().get_mut(len..) {
                added.fill(Zval::UNDEF);
            }
        }
        Ok(())
    }

    /// Where a frame the stack let go of stood given back: its room on
    /// the page, the page it opened, the allocation of its own, or the
    /// room off the pages, kept for the next frameless call unless one
    /// is kept already.
    #[cold]
    #[inline(never)]
    fn leave(&mut self, heap: &mut Heap, frame: Frame<H>) {
        match frame.placed {
            Placed::Heap { .. } => frame.free(heap),
            Placed::Loose(room) => {
                if self.spare.is_none() {
                    self.spare = Some(room);
                }
            }
            Placed::OnPage => self.top = frame.room(),
            Placed::Opened => {
                if let Some(page) = self.pages.pop() {
                    page.give_back(heap);
                }
                self.stand_on_top();
            }
        }
    }

    /// The frame the loop is executing, off the stack and handed over
    /// whole with every slot it holds (design §5.6, §5.11): what
    /// `GENERATOR_CREATE` and `YIELD` do to a generator's frame, which
    /// lives on the heap from its creation as php's `zend_generator`
    /// keeps its own `execute_data`. The first time, the frame, its
    /// header and extra arguments with it, is copied into an allocation
    /// of its own, `emalloc(used_stack)` and the `memcpy` beside it
    /// (`Z/Zend/zend_vm_def.h:4674-4682`), and its room on the page is
    /// given back; after that the slots are one allocation that moves
    /// with the frame and not through it, so a `ZvalRef` into one names
    /// the same zval while the frame is off the stack, and a frame
    /// pushed while it runs never lands on them. None on an empty stack.
    pub fn move_to_heap(&mut self, heap: &mut Heap) -> Result<Option<Frame<H>>, Fatal> {
        let Some(frame) = self.frames.pop() else {
            return Ok(None);
        };
        self.unlink(&frame);
        let start = match frame.placed {
            Placed::Heap { .. } => return Ok(Some(frame)),
            Placed::OnPage | Placed::Opened => frame.room(),
            Placed::Loose(_) => {
                self.frames.push(frame);
                return Err(Fatal::error(
                    "InternalError: a frameless call's frame moved off the VM stack",
                ));
            }
        };
        if self.pages.is_empty() {
            return Err(Fatal::error(
                "InternalError: a frame moved off a VM stack with no page",
            ));
        }
        let used = self
            .top
            .as_ptr()
            .addr()
            .saturating_sub(start.as_ptr().addr())
            / SLOT;
        let block = match heap.mm.alloc(used * SLOT) {
            Ok(block) => block.cast::<Zval>(),
            Err(fatal) => {
                self.stand(frame);
                return Err(fatal);
            }
        };
        // SAFETY: the top page's `used` slots from `start` to its top are
        // the frame's, inside the page's allocation, and the block was just
        // allocated for `used` slots; the two do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(start.as_ptr(), block.as_ptr(), used) };
        let len = frame.len;
        self.leave(heap, frame);
        Ok(Some(Frame {
            // SAFETY: the header's slots are inside the block.
            at: unsafe { block.add(FRAME_HEADER_SLOTS) },
            len,
            placed: Placed::Heap { block },
            header: PhantomData,
        }))
    }

    /// A frame [`VmStack::move_to_heap`] took, back on top of this
    /// stack, whichever stack that is: the generator's resumption,
    /// which pushes the frame where the resumer stands, on a fiber's
    /// stack when a fiber resumes it (design §5.11).
    pub fn resume_frame(&mut self, frame: Frame<H>) -> FrameRef {
        self.stand(frame)
    }

    /// A frame [`VmStack::move_to_heap`] took, copied back onto this
    /// stack's pages, header and slots, and its own allocation given
    /// back: `zend_generator_restore_call_stack`, which copies the calls
    /// a generator froze back where its resumer stands, so they are
    /// built on and grown as any call on the stack is. A frame that
    /// stands on no allocation of its own is pushed as it is.
    pub fn restore_frame(&mut self, heap: &mut Heap, frame: Frame<H>) -> Result<FrameRef, Fatal> {
        if !matches!(frame.placed, Placed::Heap { .. }) {
            return Ok(self.resume_frame(frame));
        }
        let len = frame.len;
        let header = frame.header_copied();
        let at = self.push_frame(heap, len, 0, || header)?;
        if let (Some(cells), Some(to)) = (frame.cells().get(..len), self.frames.last_mut()) {
            to.cells_mut().copy_from_slice(cells);
        }
        frame.free(heap);
        Ok(at)
    }

    /// The frame at `f`, if it is live.
    #[must_use]
    pub fn frame(&self, f: FrameRef) -> Option<&Frame<H>> {
        self.frames.get(f.0 as usize)
    }

    /// The frame at `f`, mutably, if it is live.
    pub fn frame_mut(&mut self, f: FrameRef) -> Option<&mut Frame<H>> {
        self.frames.get_mut(f.0 as usize)
    }

    /// The header of the frame at `f`, if it is live.
    #[must_use]
    pub fn header(&self, f: FrameRef) -> Option<&H> {
        self.frame(f).map(Frame::header)
    }

    /// The header of the frame at `f`, mutably, if it is live.
    pub fn header_mut(&mut self, f: FrameRef) -> Option<&mut H> {
        self.frame_mut(f).map(Frame::header_mut)
    }

    /// The bytes the top page has left for frames, php's
    /// `EG(vm_stack_end) - EG(vm_stack_top)`: what a frame pushed there
    /// takes room from, which php's compiled code holds against the
    /// frames it pushes (`zend_jit_stack_check`). Zero where no page
    /// stands yet.
    #[must_use]
    pub fn room(&self) -> usize {
        self.left() * SLOT
    }

    /// How many frames stand on the stack.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether no frame stands on the stack.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The frame the loop is executing: the top of the stack.
    #[must_use]
    pub fn current(&self) -> FrameRef {
        FrameRef(u32::try_from(self.frames.len().saturating_sub(1)).unwrap_or(u32::MAX))
    }

    /// The entry frame of the innermost serving on this stack; the
    /// current frame when nothing has pushed an entry.
    #[must_use]
    pub fn entry(&self) -> FrameRef {
        self.entries
            .last()
            .copied()
            .unwrap_or_else(|| self.current())
    }

    /// Pushed by the loop on entry, and popped by [`VmStack::pop_entry`]
    /// when that serving ends, on the fatal path as on the normal one,
    /// since shutdown functions and destructors serve again after a
    /// fatal. The guard is the receipt: the loop holds the request and
    /// not the stack while it dispatches, so the pairing is a value it
    /// carries rather than a borrow it keeps.
    #[must_use = "the entry slot is popped by VmStack::pop_entry, which takes this"]
    pub fn push_entry(&mut self, f: FrameRef) -> EntryGuard {
        self.entries.push(f);
        EntryGuard {
            at: self.entries.len(),
        }
    }

    /// Pop the entry slot the guard is the receipt for, and any an inner
    /// serving left standing, so a serving that ended without popping its
    /// own cannot leave one behind.
    // Taking the receipt by value is the point: one serving pops its own
    // entry slot once, and a `Copy` guard could pop it twice.
    #[allow(clippy::needless_pass_by_value)]
    pub fn pop_entry(&mut self, guard: EntryGuard) {
        let EntryGuard { at } = guard;
        self.entries.truncate(at.saturating_sub(1));
    }
}

impl Page {
    /// `zend_vm_stack_new_page`: `size` bytes of `heap`, the top just
    /// past the header.
    fn take(heap: &mut Heap, size: usize) -> Result<Page, Fatal> {
        let at = heap.mm.alloc(size)?.cast::<Zval>();
        Ok(Page {
            at,
            slots: size / SLOT,
            saved: PAGE_HEADER_SLOTS,
        })
    }

    /// One past the page's last slot, `zend_vm_stack->end`.
    fn end(&self) -> NonNull<Zval> {
        // SAFETY: one past the last of the `slots` slots of the page's one
        // allocation is its end, which a pointer may name.
        unsafe { self.at.add(self.slots) }
    }

    /// The place of the slot at `at`, a slot of this page or its end.
    fn index_of(&self, at: NonNull<Zval>) -> usize {
        at.as_ptr().addr().saturating_sub(self.at.as_ptr().addr()) / SLOT
    }

    /// The page's slot `at`, which is inside it, or its end.
    fn slot(&self, at: usize) -> NonNull<Zval> {
        // SAFETY: every caller's `at` is at most `slots`, the slots of the
        // one allocation `take` made, so the offset stays inside it or
        // names its end.
        unsafe { self.at.add(at) }
    }

    /// `efree(page)`.
    fn give_back(self, heap: &mut Heap) {
        // SAFETY: the page came from this heap's `alloc` in `take`, and
        // leaving the stack's list was its last name, so it is freed once.
        unsafe { heap.mm.free(self.at.cast()) };
    }
}

/// The frames of a stack and its entry slots, set aside by
/// [`VmStack::set_aside`] and put back by [`VmStack::put_back`].
#[derive(Debug)]
#[must_use = "the frames are put back by VmStack::put_back, which takes this"]
pub struct FramesAside<H> {
    frames: Frames<H>,
    entries: Vec<FrameRef>,
    /// The word as it stood, the innermost of the frames set aside.
    innermost: usize,
}

impl<H> VmStack<H> {
    /// The stack's frames and entry slots set aside, its pages kept with
    /// the room the frames hold: what is pushed next lands above them on
    /// the same page, as a frame pushed with no `execute_data` standing
    /// lands on php's one VM stack, and is numbered from the bottom.
    pub fn set_aside(&mut self) -> FramesAside<H> {
        FramesAside {
            frames: self.frames.take(),
            entries: std::mem::take(&mut self.entries),
            innermost: self.innermost.swap(0, Ordering::Relaxed),
        }
    }

    /// What [`VmStack::set_aside`] took put back, over every frame
    /// pushed since popped; a frame still standing is let go first,
    /// newest first, so the room is the set-aside frames' again, and its
    /// header handed back as [`VmStack::close`] hands one back.
    // Taking the receipt by value is the point: what was set aside is
    // put back once.
    #[allow(clippy::needless_pass_by_value)]
    #[must_use = "a frame's header names what the engine keeps for it beyond the stack"]
    pub fn put_back(&mut self, heap: &mut Heap, aside: FramesAside<H>) -> Vec<H> {
        let mut dropped = Vec::new();
        while let Some(frame) = self.frames.pop() {
            self.unlink(&frame);
            dropped.push(frame.header_copied());
            self.leave(heap, frame);
        }
        self.frames = aside.frames;
        self.entries = aside.entries;
        self.innermost.store(aside.innermost, Ordering::Relaxed);
        dropped
    }
}

/// What [`VmStack::push_entry`] hands back, and what
/// [`VmStack::pop_entry`] takes: one serving's claim on the entry slot.
#[derive(Debug)]
pub struct EntryGuard {
    /// The depth of the entry slot stack with this serving's frame on it.
    at: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::serial::alone;

    /// A header as the engine's is: words, and a cell an engine writes
    /// through a shared borrow.
    #[derive(Debug, Default, Clone, PartialEq)]
    struct Head {
        function: u32,
        opline: u32,
        this: Option<NonNull<u8>>,
        seen: std::cell::Cell<u64>,
    }

    #[test]
    fn the_entry_slot_pops_with_its_guard() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut stack = VmStack::<Head>::new();
        let outer = stack
            .push_frame(&mut heap, 2, 0, Head::default)
            .expect("a first frame fits the first page");
        let guard = stack.push_entry(outer);
        let inner = stack.push_frame(&mut heap, 1, 0, Head::default);
        assert_eq!(stack.entry(), outer);
        assert_eq!(inner.ok(), Some(stack.current()));
        stack.pop_frame(&mut heap);
        stack.pop_entry(guard);
        assert_eq!(stack.entry(), outer);
        if let Some(f) = stack.frame_mut(outer) {
            f.set(Slot(1), Zval::long(9));
        }
        assert_eq!(
            stack.frame(outer).map(|f| f.slot(Slot(1))),
            Some(Zval::long(9))
        );
        assert_eq!(
            stack.frame(outer).map(|f| f.slot(Slot(5))),
            Some(Zval::UNDEF)
        );
        drop(stack.close(&mut heap));
    }

    /// The top and a frame below it are lent together, each its own:
    /// a write through one is read back through the other's frame, and
    /// the top itself, or a frame that does not stand, lends nothing.
    #[test]
    fn the_top_and_a_frame_below_it_are_lent_together() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut stack = VmStack::<Head>::new();
        let sending = stack
            .push_frame(&mut heap, 2, 0, Head::default)
            .expect("two frames fit the first page");
        let called = stack
            .push_frame(&mut heap, 3, 0, Head::default)
            .expect("two frames fit the first page");
        let (top, below) = stack
            .top_and_frame_mut(sending)
            .expect("a frame below the top is lent with it");
        below.set(Slot(1), Zval::long(4));
        top.set(Slot(2), below.slot(Slot(1)));
        top.set(Slot(0), Zval::long(5));
        below.set(Slot(0), top.slot(Slot(0)));
        assert_eq!(
            stack.frame(called).map(|f| f.slot(Slot(2))),
            Some(Zval::long(4))
        );
        assert_eq!(
            stack.frame(sending).map(|f| f.slot(Slot(0))),
            Some(Zval::long(5))
        );
        assert!(stack.top_and_frame_mut(called).is_none());
        assert!(stack.top_and_frame_mut(FrameRef(7)).is_none());
        stack.pop_frame(&mut heap);
        assert!(stack.top_and_frame_mut(sending).is_none());
        drop(stack.close(&mut heap));
    }

    /// A stack that lets frames go without their pops hands their
    /// headers back, the newest first: the frames standing when it is
    /// closed, and those pushed over frames set aside and standing when
    /// they are put back, which are then the set-aside frames again.
    #[test]
    fn frames_let_go_without_their_pops_hand_their_headers_back() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut stack = VmStack::<Head>::new();
        let made = |function| Head {
            function,
            ..Head::default()
        };
        assert!(stack.push_frame(&mut heap, 2, 0, || made(1)).is_ok());
        let aside = stack.set_aside();
        assert!(stack.push_frame(&mut heap, 2, 0, || made(2)).is_ok());
        assert!(stack.push_frame(&mut heap, 2, 0, || made(3)).is_ok());
        let dropped = stack.put_back(&mut heap, aside);
        assert_eq!(dropped, [made(3), made(2)]);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.header(FrameRef(0)), Some(&made(1)));
        let aside = stack.set_aside();
        assert_eq!(stack.put_back(&mut heap, aside), []);
        assert!(stack.push_frame(&mut heap, 2, 0, || made(4)).is_ok());
        assert_eq!(stack.close(&mut heap), [made(4), made(1)]);
        assert_eq!(stack.close(&mut heap), []);
    }

    /// php's page arithmetic, to the byte `memory_get_usage()` reads:
    /// the first page is 256 KiB of the heap, a frame that does not fit
    /// what is left of it opens a second of the same size, and the pop
    /// of that frame gives the second back while the first stays. A
    /// frame of ten slots and its five header slots is 240 bytes, so
    /// 1092 of them fill the first page's 16382 slots past its header
    /// and the 1093rd opens the next.
    #[test]
    fn a_frame_past_the_page_takes_a_page_of_its_own_and_gives_it_back() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut stack = VmStack::<Head>::new();
        assert_eq!(stack.open(&mut heap), Ok(()));
        assert_eq!(heap.memory_usage(false) - start, PAGE_SIZE);
        for _ in 0..1092 {
            assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        }
        assert_eq!(heap.memory_usage(false) - start, PAGE_SIZE);
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        assert_eq!(heap.memory_usage(false) - start, 2 * PAGE_SIZE);
        stack.pop_frame(&mut heap);
        assert_eq!(heap.memory_usage(false) - start, PAGE_SIZE);
        // A frame larger than a page takes one of its own, rounded up
        // to the page size past the header.
        assert!(
            stack
                .push_frame(&mut heap, PAGE_SIZE / SLOT, 0, Head::default)
                .is_ok()
        );
        assert_eq!(heap.memory_usage(false) - start, 3 * PAGE_SIZE);
        drop(stack.close(&mut heap));
        assert_eq!(heap.memory_usage(false), start);
    }

    /// A pop puts the stack's top and its innermost room back where the
    /// frame below left them, whichever room the frame stood in: one on
    /// the top page, read where it stands; one that opened a page, which
    /// goes back to the heap; and one a generator's move put in an
    /// allocation of its own and a resumption put back on top, under a
    /// frame on the page above it.
    #[test]
    fn a_pop_puts_the_top_back_from_every_kind_of_room() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut stack = VmStack::<Head>::new();
        assert_eq!(stack.open(&mut heap), Ok(()));
        let (empty_room, empty_link) = (stack.room(), stack.innermost());
        for _ in 0..1092 {
            assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        }
        let (full, link) = (stack.room(), stack.innermost());
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        assert_eq!(heap.memory_usage(false) - start, 2 * PAGE_SIZE);
        stack.pop_frame(&mut heap);
        assert_eq!(heap.memory_usage(false) - start, PAGE_SIZE);
        assert_eq!((stack.room(), stack.innermost()), (full, link));
        stack.pop_frame(&mut heap);
        let (below, below_link) = (stack.room(), stack.innermost());
        assert!(stack.push_frame(&mut heap, 2, 0, Head::default).is_ok());
        let moved = stack
            .move_to_heap(&mut heap)
            .expect("the move to the heap")
            .expect("the frame moves to the heap");
        let resumed = stack.resume_frame(moved);
        let above = stack
            .push_frame(&mut heap, 3, 0, Head::default)
            .expect("a frame above the resumed one");
        assert_eq!(above.0, resumed.0 + 1);
        stack.pop_frame(&mut heap);
        assert_ne!(stack.innermost(), below_link);
        stack.pop_frame(&mut heap);
        assert_eq!((stack.room(), stack.innermost()), (below, below_link));
        for _ in 0..1091 {
            stack.pop_frame(&mut heap);
        }
        assert!(stack.is_empty());
        assert_eq!((stack.room(), stack.innermost()), (empty_room, empty_link));
        stack.pop_frame(&mut heap);
        assert!(stack.is_empty());
        drop(stack.close(&mut heap));
        assert_eq!(heap.memory_usage(false), start);
    }

    /// A frameless call's frame takes no page room: on a full page it
    /// opens none and the heap reads the same, its slots and header are
    /// a frame's as any other's, a frame pushed above it lands on the
    /// pages where it would have without it, and its room is the next
    /// one's after it pops: one room per such call in flight, each a
    /// header and three operands, and a frame of more is the engine's
    /// error.
    #[test]
    fn a_frameless_frame_opens_no_page_and_leaves_the_pages_as_they_stood() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut stack = VmStack::<Head>::new();
        for _ in 0..1092 {
            assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        }
        let full = heap.memory_usage(false);
        assert_eq!(full - start, PAGE_SIZE);
        let called = Head {
            function: 7,
            ..Head::default()
        };
        heap.reset_peak();
        let call = stack
            .push_frameless(&[Zval::UNDEF; 3], called.clone())
            .expect("a frameless call");
        assert_eq!(heap.memory_usage(false), full);
        assert_eq!(heap.memory_peak(), full);
        assert_eq!(stack.current(), call);
        assert_eq!(stack.header(call), Some(&called));
        if let Some(f) = stack.frame_mut(call) {
            f.set(Slot(2), Zval::long(3));
        }
        assert_eq!(
            stack.frame(call).map(|f| f.slot(Slot(2))),
            Some(Zval::long(3))
        );
        assert_eq!(stack.frame(call).map(Frame::len), Some(3));
        // A callback the native serves opens the page php's opens, and a
        // frameless call it makes takes a room of its own.
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        assert_eq!(heap.memory_usage(false) - start, 2 * PAGE_SIZE);
        let inner = stack.push_frameless(&[Zval::UNDEF], Head::default());
        assert!(
            stack
                .push_frameless(&[Zval::UNDEF; 4], Head::default())
                .is_err()
        );
        let room = |stack: &VmStack<Head>, f: Option<FrameRef>| {
            f.and_then(|f| stack.frames.get(f.0 as usize)).map(|f| f.at)
        };
        let (outer_room, inner_room) = (room(&stack, Some(call)), room(&stack, inner.ok()));
        assert_eq!(stack.current().0, 1094);
        assert!(outer_room.is_some() && inner_room.is_some() && outer_room != inner_room);
        stack.pop_frame(&mut heap);
        stack.pop_frame(&mut heap);
        assert!(stack.spare.is_some());
        assert_eq!(heap.memory_usage(false), full);
        stack.pop_frame(&mut heap);
        // On a page with room, as on a full one: the room is the one
        // kept spare, and the page's top stands where it stood; the
        // outer call's room, given back with one kept, is freed.
        stack.pop_frame(&mut heap);
        let again = stack
            .push_frameless(&[Zval::long(5), Zval::UNDEF], Head::default())
            .ok();
        assert_eq!(room(&stack, again), inner_room);
        assert_eq!(
            again.and_then(|f| stack.frame(f)).map(|f| f.slot(Slot(0))),
            Some(Zval::long(5)),
            "the operands are laid in the room's slots"
        );
        assert!(stack.spare.is_none());
        stack.pop_frame(&mut heap);
        assert!(stack.spare.is_some());
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        assert_eq!(heap.memory_usage(false), full);
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        assert_eq!(heap.memory_usage(false) - start, 2 * PAGE_SIZE);
        drop(stack.close(&mut heap));
        assert_eq!(heap.memory_usage(false), start);
    }

    /// The limit meets a page as it meets any allocation: at the chunk
    /// the heap would have to grow by, with php's message and the page's
    /// 262144 bytes in it.
    #[test]
    fn a_stack_past_the_memory_limit_is_refused_with_php_s_fatal() {
        let _alone = alone();
        let mut heap = Heap::new();
        heap.set_memory_limit(4 * 1024 * 1024);
        let mut stack = VmStack::<Head>::new();
        let refused = (0..100_000)
            .map(|_| stack.push_frame(&mut heap, 10, 0, Head::default))
            .find_map(Result::err);
        assert_eq!(
            refused,
            Some(Fatal::error(
                "Allowed memory size of 4194304 bytes exhausted (tried to allocate 262144 bytes)"
            ))
        );
        drop(stack.close(&mut heap));
    }

    /// A fiber's pages are 16 KiB, and its first holds the fiber
    /// function's frame below the first one pushed: 1024 slots less the
    /// header's two and that frame's five leave 1017, sixty-seven frames
    /// of fifteen.
    #[test]
    fn a_fiber_s_stack_grows_by_its_own_pages() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut stack = VmStack::<Head>::fiber();
        assert_eq!(stack.open(&mut heap), Ok(()));
        assert_eq!(heap.memory_usage(false) - start, FIBER_PAGE_SIZE);
        for _ in 0..67 {
            assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        }
        assert_eq!(heap.memory_usage(false) - start, FIBER_PAGE_SIZE);
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default).is_ok());
        assert_eq!(heap.memory_usage(false) - start, 2 * FIBER_PAGE_SIZE);
        drop(stack.close(&mut heap));
        assert_eq!(heap.memory_usage(false), start);
    }

    /// The header is the room's: written by the push, read and written
    /// through the frame, carried with the slots when the frame moves to
    /// an allocation of its own, and back on a stack where it resumes.
    #[test]
    fn a_frame_s_header_stands_in_its_room_and_moves_with_it() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut stack = VmStack::<Head>::new();
        let written = Head {
            function: 7,
            opline: 3,
            ..Head::default()
        };
        let f = stack
            .push_frame(&mut heap, 2, 0, || written.clone())
            .expect("a frame");
        assert_eq!(stack.header(f), Some(&written));
        if let Some(header) = stack.header_mut(f) {
            header.opline = 4;
        }
        if let Some(header) = stack.header(f) {
            header.seen.set(9);
        }
        if let Some(frame) = stack.frame_mut(f) {
            frame.set(Slot(1), Zval::long(5));
        }
        let moved = stack.move_to_heap(&mut heap);
        assert!(stack.is_empty());
        let moved = moved
            .expect("the move to the heap")
            .expect("the frame moves to the heap");
        assert_eq!(
            (
                moved.header().function,
                moved.header().opline,
                moved.header().seen.get()
            ),
            (7, 4, 9)
        );
        let back = stack.resume_frame(moved);
        assert_eq!(
            stack.frame(back).map(|f| f.slot(Slot(1))),
            Some(Zval::long(5))
        );
        assert_eq!(stack.header(back).map(|h| h.opline), Some(4));
        stack.pop_frame(&mut heap);
        drop(stack.close(&mut heap));
        assert_eq!(heap.memory_usage(false), start);
    }

    /// php's `zend_vm_stack_extend_call_frame`: the top frame takes more
    /// slots where its page has more than that left, its slots and
    /// header kept; where it has not, the frame is copied to a page of
    /// its own with what it held, and a page it leaves empty is given
    /// back (`zend_vm_stack_copy_call_frame`).
    #[test]
    fn the_top_frame_grows_in_place_or_moves_to_a_page_of_its_own() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut stack = VmStack::<Head>::new();
        let written = Head {
            function: 1,
            ..Head::default()
        };
        let f = stack
            .push_frame(&mut heap, 2, 0, || written.clone())
            .expect("a first frame fits");
        if let Some(frame) = stack.frame_mut(f) {
            frame.set(Slot(0), Zval::long(1));
        }
        assert_eq!(stack.grow_top(&mut heap, f, 3), Ok(()));
        assert_eq!(stack.frame(f).map(Frame::len), Some(5));
        assert_eq!(stack.frame(f).map(|f| f.slot(Slot(0))), Some(Zval::long(1)));
        assert_eq!(stack.frame(f).map(|f| f.slot(Slot(4))), Some(Zval::UNDEF));
        assert_eq!(heap.memory_usage(false) - start, PAGE_SIZE);
        // More than the page has left: a page of its own, rounded up to
        // the page size, and the first page, now empty, given back.
        assert_eq!(stack.grow_top(&mut heap, f, PAGE_SIZE / SLOT), Ok(()));
        assert_eq!(stack.header(f), Some(&written));
        assert_eq!(stack.frame(f).map(|f| f.slot(Slot(0))), Some(Zval::long(1)));
        assert_eq!(stack.frame(f).map(Frame::len), Some(5 + PAGE_SIZE / SLOT));
        assert_eq!(heap.memory_usage(false) - start, 2 * PAGE_SIZE);
        let g = stack
            .push_frame(&mut heap, 1, 0, Head::default)
            .expect("a frame above fits on a fresh page");
        assert!(
            stack.grow_top(&mut heap, f, 1).is_err(),
            "only the top frame grows"
        );
        stack.pop_frame(&mut heap);
        assert_ne!(g, f);
        stack.pop_frame(&mut heap);
        drop(stack.close(&mut heap));
        assert_eq!(heap.memory_usage(false), start);
    }

    /// A call a generator froze goes back onto the pages, header and
    /// slots, where it can be grown as any call on the stack, and the
    /// allocation it was kept in goes back to the heap.
    #[test]
    fn a_frozen_frame_is_restored_onto_the_pages() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let mut stack = VmStack::<Head>::new();
        let written = Head {
            function: 3,
            ..Head::default()
        };
        let f = stack
            .push_frame(&mut heap, 2, 0, || written.clone())
            .expect("a frame");
        if let Some(frame) = stack.frame_mut(f) {
            frame.set(Slot(1), Zval::long(8));
        }
        let moved = stack
            .move_to_heap(&mut heap)
            .expect("the move to the heap")
            .expect("a frame to move");
        let held = heap.memory_usage(false);
        let back = stack
            .restore_frame(&mut heap, moved)
            .expect("the restored frame");
        assert!(
            heap.memory_usage(false) < held,
            "the frame's own allocation is given back"
        );
        assert_eq!(stack.header(back), Some(&written));
        assert_eq!(
            stack.frame(back).map(|f| f.slot(Slot(1))),
            Some(Zval::long(8))
        );
        assert_eq!(stack.grow_top(&mut heap, back, 2), Ok(()));
        stack.pop_frame(&mut heap);
        drop(stack.close(&mut heap));
        assert_eq!(heap.memory_usage(false), start);
    }

    /// The rooms the word reaches, innermost first, read as another
    /// process reads them: the word's address, then each room's link.
    fn chain<H>(stack: &VmStack<H>) -> Vec<usize> {
        // SAFETY: the word's address is the live stack's field, exposed.
        let mut at =
            unsafe { std::ptr::with_exposed_provenance::<usize>(stack.link_word()).read() };
        let mut rooms = Vec::new();
        while at != 0 && rooms.len() < 64 {
            rooms.push(at);
            // SAFETY: a linked room stands while its frame does, and its
            // address was exposed by the push.
            at = unsafe { std::ptr::with_exposed_provenance::<usize>(at).read() };
        }
        rooms
    }

    /// The rooms of the frames standing, innermost first, an unobserved
    /// frameless call's left out.
    fn rooms(stack: &VmStack<Head>) -> Vec<usize> {
        stack
            .frames
            .iter()
            .rev()
            .filter(|frame| frame.linked())
            .map(Frame::room_address)
            .collect()
    }

    /// Every push links its room above the innermost one and every pop
    /// moves the word back, a frameless call's room never on the chain,
    /// a frame moved to the heap off it and back on where it resumes, a
    /// frame grown onto a page of its own followed there, and frames set
    /// aside off it until they are put back (design §5.6, revision 59).
    #[test]
    fn the_word_names_a_chain_of_rooms_as_the_stack_stands() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut stack = VmStack::<Head>::new();
        assert_eq!(stack.innermost(), 0);
        assert!(chain(&stack).is_empty());
        for _ in 0..3 {
            assert!(stack.push_frame(&mut heap, 2, 0, Head::default).is_ok());
        }
        assert_eq!(chain(&stack), rooms(&stack));
        assert_eq!(chain(&stack).len(), 3);
        assert!(
            stack
                .push_frameless(&[Zval::UNDEF; 2], Head::default())
                .is_ok()
        );
        assert_eq!(chain(&stack).len(), 3, "a frameless call is on no chain");
        assert!(stack.push_frame(&mut heap, 1, 0, Head::default).is_ok());
        assert_eq!(chain(&stack), rooms(&stack));
        assert_eq!(chain(&stack).len(), 4);
        stack.pop_frame(&mut heap);
        stack.pop_frame(&mut heap);
        assert_eq!(chain(&stack), rooms(&stack));
        let moved = stack
            .move_to_heap(&mut heap)
            .expect("the move to the heap")
            .expect("a frame to move");
        assert_eq!(chain(&stack).len(), 2);
        stack.resume_frame(moved);
        assert_eq!(chain(&stack), rooms(&stack));
        let grown = stack
            .push_frame(&mut heap, 1, 0, Head::default)
            .expect("a frame");
        assert_eq!(stack.grow_top(&mut heap, grown, PAGE_SIZE / SLOT), Ok(()));
        assert_eq!(chain(&stack), rooms(&stack));
        stack.pop_frame(&mut heap);
        let aside = stack.set_aside();
        assert!(chain(&stack).is_empty());
        assert!(stack.push_frame(&mut heap, 1, 0, Head::default).is_ok());
        assert_eq!(chain(&stack).len(), 1);
        drop(stack.put_back(&mut heap, aside));
        assert_eq!(chain(&stack), rooms(&stack));
        assert_eq!(chain(&stack).len(), 3);
        drop(stack.close(&mut heap));
        assert_eq!(stack.innermost(), 0);
    }

    /// A header that fills the room past its link, as the engine's does,
    /// which a [`FakeRoom`] asks of its header.
    #[derive(Debug, Default)]
    #[expect(
        dead_code,
        reason = "a room's bytes are read as another process reads them"
    )]
    struct Wide([u64; 9]);

    /// A room off the pages linked as php links `execute_fake`: the next
    /// push links above it and it links the resumer's room, and the word
    /// is back on the resumer's once the frame above it has left and it
    /// is unlinked.
    #[test]
    fn a_fake_room_stands_between_the_frame_above_it_and_the_resumer() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut stack = VmStack::<Wide>::new();
        assert!(stack.push_frame(&mut heap, 1, 0, Wide::default).is_ok());
        let resumer = stack.innermost();
        let mut room = FakeRoom::new(Wide::default());
        stack.link_room(&mut room);
        assert_eq!(stack.innermost(), room.address());
        assert!(stack.push_frame(&mut heap, 1, 0, Wide::default).is_ok());
        let walked = chain(&stack);
        assert_eq!(walked.len(), 3);
        assert_eq!(walked.get(1..), Some(&[room.address(), resumer][..]));
        stack.pop_frame(&mut heap);
        assert_eq!(stack.innermost(), room.address());
        stack.unlink_room(&room);
        assert_eq!(stack.innermost(), resumer);
        assert_eq!(chain(&stack), [resumer]);
        drop(stack.close(&mut heap));
    }

    /// A fiber's stack linked below the stack that resumed it: its first
    /// room's link, or, before its first push, the word itself.
    #[test]
    fn a_fiber_s_bottom_links_to_its_resumer() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut stack = VmStack::<Head>::new();
        assert!(stack.push_frame(&mut heap, 1, 0, Head::default).is_ok());
        let resumer = stack.innermost();
        let mut fiber = VmStack::<Head>::fiber();
        fiber.link_below(resumer);
        assert!(fiber.push_frame(&mut heap, 1, 0, Head::default).is_ok());
        assert!(fiber.push_frame(&mut heap, 1, 0, Head::default).is_ok());
        let walked = chain(&fiber);
        assert_eq!(walked.len(), 3);
        assert_eq!(walked.last(), Some(&resumer));
        fiber.link_below(0);
        assert_eq!(chain(&fiber).len(), 2);
        drop(fiber.close(&mut heap));
        drop(stack.close(&mut heap));
    }

    /// The word at `at` bytes into `base`, read as compiled code reads it:
    /// as a pointer, keeping what the word points into.
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "the layout's word offsets are word-aligned"
    )]
    fn word_at(base: *const u8, at: usize) -> *mut u8 {
        let word = base.wrapping_add(at).cast::<*mut u8>();
        // SAFETY: every caller names a word of a live stack or of a record
        // or room it names, at an offset its layout gives.
        unsafe { word.read() }
    }

    /// `value` written as the word at `at` bytes into `base`.
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "the layout's word offsets are word-aligned"
    )]
    fn put_word(base: *mut u8, at: usize, value: *mut u8) {
        let word = base.wrapping_add(at).cast::<*mut u8>();
        // SAFETY: as `word_at`, and the caller holds the stack mutably.
        unsafe { word.write(value) };
    }

    /// A count the layout names at `at` bytes into `base`.
    fn count_at(base: *const u8, at: usize) -> usize {
        word_at(base, at).addr()
    }

    /// `value` written as the count at `at` bytes into `base`.
    fn put_count(base: *mut u8, at: usize, value: usize) {
        put_word(base, at, std::ptr::without_provenance_mut(value));
    }

    /// A frame of `slots` slots pushed with nothing but the words
    /// [`VmStack::LAYOUT`] names, as compiled code pushes a plain call's
    /// (`jit-design.md` §5.3): the room checked against the page's end and
    /// the table's against its capacity, the link, the header and the slots
    /// written, the word moved, the record laid and the top bumped. None
    /// where either check refuses, nothing written.
    #[allow(
        clippy::cast_ptr_alignment,
        reason = "a room and its slots are slot-aligned"
    )]
    fn pushed_in_place(stack: &mut VmStack<Head>, slots: usize, header: Head) -> Option<()> {
        let layout = VmStack::<Head>::LAYOUT;
        let base = std::ptr::from_mut(stack).cast::<u8>();
        let (room, end) = (word_at(base, layout.top), word_at(base, layout.end));
        let used = (FRAME_HEADER_SLOTS + slots) * SLOT;
        let table = base.wrapping_add(layout.frames);
        let (at, len, cap) = (
            word_at(table, layout.table.at),
            count_at(table, layout.table.len),
            count_at(table, layout.table.cap),
        );
        if end.addr() - room.addr() < used || len == cap {
            return None;
        }
        put_count(room, 0, count_at(base, layout.innermost));
        let head = room.wrapping_add(HEADER_AT).cast::<Head>();
        // SAFETY: the header's place is past the room's link, inside the
        // room the check found free.
        unsafe { head.write(header) };
        for slot in 0..slots {
            let cell = room.wrapping_add(ROOM_BYTES + slot * SLOT).cast::<Zval>();
            // SAFETY: the slot is inside the room the check found free.
            unsafe { cell.write(Zval::UNDEF) };
        }
        put_count(base, layout.innermost, room.expose_provenance());
        let record = at.wrapping_add(len * layout.table.record);
        put_word(
            record,
            layout.table.record_at,
            room.wrapping_add(ROOM_BYTES),
        );
        put_count(record, layout.table.record_len, slots);
        let tag = record.wrapping_add(layout.table.record_placed);
        // SAFETY: the tag is the placement's first byte, inside the record
        // the capacity check found free.
        unsafe { tag.write(layout.table.on_page) };
        put_count(table, layout.table.len, len + 1);
        put_word(base, layout.top, room.wrapping_add(used));
        Some(())
    }

    /// The top frame popped with nothing but the words the layout names,
    /// as compiled code leaves a plain frame: its record taken off the
    /// table, the word moved back to its link and the top back to its
    /// room. None where the top record stands off the top page's slots.
    fn popped_in_place(stack: &mut VmStack<Head>) -> Option<()> {
        let layout = VmStack::<Head>::LAYOUT;
        let base = std::ptr::from_mut(stack).cast::<u8>();
        let table = base.wrapping_add(layout.frames);
        let (at, len) = (
            word_at(table, layout.table.at),
            count_at(table, layout.table.len),
        );
        let last = len.checked_sub(1)?;
        let record = at.wrapping_add(last * layout.table.record);
        let tag = record.wrapping_add(layout.table.record_placed);
        // SAFETY: the tag is the live last record's placement's first byte.
        let tag = unsafe { tag.read() };
        if tag != layout.table.on_page {
            return None;
        }
        let room = word_at(record, layout.table.record_at).wrapping_sub(ROOM_BYTES);
        put_count(table, layout.table.len, last);
        put_count(base, layout.innermost, count_at(room, 0));
        put_word(base, layout.top, room);
        Some(())
    }

    /// A frame pushed and popped through the layout's words alone leaves
    /// the stack as `push_frame` and `pop_frame` leave it: the same room,
    /// header, slots, chain and room left, across the table's growth and
    /// the first page; past the page's end, or a record that opened a
    /// page, the in-place push and pop refuse and leave nothing written.
    #[test]
    fn a_frame_pushed_in_place_stands_as_a_pushed_one() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut by_push = VmStack::<Head>::new();
        let mut in_place = VmStack::<Head>::new();
        assert!(pushed_in_place(&mut in_place, 2, Head::default()).is_none());
        for stack in [&mut by_push, &mut in_place] {
            assert!(stack.push_frame(&mut heap, 3, 0, Head::default).is_ok());
        }
        let made = |n| Head {
            function: n,
            opline: 2 * n,
            ..Head::default()
        };
        for n in 0..9 {
            assert!(by_push.push_frame(&mut heap, 2, 0, || made(n)).is_ok());
            if pushed_in_place(&mut in_place, 2, made(n)).is_none() {
                // The table is full: the engine's push grows it.
                assert!(in_place.push_frame(&mut heap, 2, 0, || made(n)).is_ok());
            }
            assert_eq!(in_place.len(), by_push.len());
            assert_eq!(in_place.room(), by_push.room());
            assert_eq!(in_place.header(in_place.current()), Some(&made(n)));
            assert_eq!(
                in_place.frame(in_place.current()).map(Frame::slots),
                Some(&[Zval::UNDEF; 2][..])
            );
            assert_eq!(chain(&in_place), rooms(&in_place));
        }
        for _ in 0..9 {
            by_push.pop_frame(&mut heap);
            assert_eq!(popped_in_place(&mut in_place), Some(()));
            assert_eq!(in_place.len(), by_push.len());
            assert_eq!(in_place.room(), by_push.room());
            assert_eq!(chain(&in_place), rooms(&in_place));
        }
        // A frame that opens a page is refused in place, and is popped by
        // the engine, which gives the page back.
        let start = heap.memory_usage(false);
        assert!(pushed_in_place(&mut in_place, PAGE_SIZE / SLOT, Head::default()).is_none());
        assert!(
            in_place
                .push_frame(&mut heap, PAGE_SIZE / SLOT, 0, Head::default)
                .is_ok()
        );
        assert!(heap.memory_usage(false) > start);
        assert!(popped_in_place(&mut in_place).is_none());
        in_place.pop_frame(&mut heap);
        assert_eq!(heap.memory_usage(false), start);
        assert!(pushed_in_place(&mut in_place, 2, made(1)).is_some());
        assert_eq!(chain(&in_place), rooms(&in_place));
        for stack in [&mut by_push, &mut in_place] {
            drop(stack.close(&mut heap));
        }
    }
}
