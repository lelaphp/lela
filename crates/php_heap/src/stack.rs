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

use std::marker::PhantomData;
use std::mem::{MaybeUninit, align_of, needs_drop, size_of};
use std::ptr::NonNull;

use crate::hooks::Fatal;
use crate::{Heap, Zval, ZvalRef};

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

/// One `zend_vm_stack` page: `slots` slots at `at`, the first
/// [`PAGE_HEADER_SLOTS`] of them the header's, and `top` the first one
/// no frame holds.
#[derive(Debug)]
struct Page {
    at: NonNull<Zval>,
    slots: usize,
    top: usize,
}

/// Where a frame stands: on the page that was the top, from slot
/// `from`, at the start of a page it opened, php's
/// `ZEND_CALL_ALLOCATED`, in an allocation of its own beginning at
/// `block`, the generator's `emalloc(used_stack)`, or in a room off
/// the pages and off the request's heap ([`Loose`]).
#[derive(Debug)]
enum Placed {
    On { from: usize },
    Opened,
    Heap { block: NonNull<Zval> },
    Loose(Loose),
}

impl Placed {
    /// The slot of the page the frame's room starts at; none for a
    /// frame that stands on no page.
    fn page_from(&self) -> Option<usize> {
        match self {
            Placed::On { from } => Some(*from),
            Placed::Opened => Some(PAGE_HEADER_SLOTS),
            Placed::Heap { .. } | Placed::Loose(_) => None,
        }
    }
}

/// The room of a frame php lays down nowhere, an unobserved frameless
/// call's (`ZEND_FRAMELESS_ICALL_*` hands its native the operands where
/// they stand and pushes no frame): slots of the process's allocator
/// and not of the request's heap, so the frame opens no page, moves
/// neither `memory_get_usage()` nor its peak, and meets no
/// `memory_limit`, as php's call meets none. A room holds a header and
/// three operands and never more, and the stack keeps the rooms its
/// frames gave back for the next ([`VmStack::push_frameless`]), so it
/// holds one room per such call in flight; a room is freed when it is
/// dropped. The frame stands among the stack's frames, the chain a
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
/// it is dropped, as an uninitialised zval reads.
#[derive(Debug)]
pub struct Frame<H> {
    /// The frame's first CV, on its page; the header stands in the
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
    /// What the room holds: the header no larger than php's
    /// `zend_execute_data` and aligned no more strictly than a zval, so
    /// it sits in the slots before the frame's first CV, and owning
    /// nothing a drop would give back, since a pop frees the room
    /// without reading it.
    const FITS: () = assert!(
        size_of::<H>() <= FRAME_HEADER_SLOTS * SLOT
            && align_of::<H>() <= align_of::<Zval>()
            && !needs_drop::<H>(),
        "a frame header must fit php's zend_execute_data room and own nothing"
    );

    /// The header's place, the room before the first CV.
    fn header_at(&self) -> NonNull<H> {
        // SAFETY: every frame's `at` is `FRAME_HEADER_SLOTS` slots past
        // the start of its room, inside the page, the block or the room
        // off the pages it was laid in.
        unsafe { self.at.sub(FRAME_HEADER_SLOTS) }.cast::<H>()
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

/// The stack of one request, or of one fiber: pages of slots and the
/// entry slot.
#[derive(Debug)]
pub struct VmStack<H> {
    kind: Kind,
    /// php's `EG(vm_stack)` and every page its `prev` reaches, oldest
    /// first; frames are pushed on the last.
    pages: Vec<Page>,
    frames: Vec<Frame<H>>,
    entries: Vec<FrameRef>,
    /// The rooms off the pages the frameless calls' frames gave back,
    /// for the next ones.
    rooms: Vec<Loose>,
}

impl<H> Default for VmStack<H> {
    fn default() -> VmStack<H> {
        VmStack {
            kind: Kind::Request,
            pages: Vec::new(),
            frames: Vec::new(),
            entries: Vec::new(),
            rooms: Vec::new(),
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
        page.top += self.kind.bottom();
        self.pages.push(page);
        Ok(())
    }

    /// `zend_vm_stack_destroy`: every page given back to `heap`, the
    /// newest first, and every frame with them, the rooms off the pages
    /// with those. The headers of the
    /// frames still standing are handed back, the newest first, so what
    /// each names beyond its words is given up where the engine keeps
    /// it; a stack every frame was popped off hands back none.
    #[must_use = "a frame's header names what the engine keeps for it beyond the stack"]
    pub fn close(&mut self, heap: &mut Heap) -> Vec<H> {
        let dropped = self
            .frames
            .drain(..)
            .rev()
            .map(|frame| frame.header_copied())
            .collect();
        for page in self.pages.drain(..).rev() {
            page.give_back(heap);
        }
        self.rooms.clear();
        dropped
    }

    /// Push a frame of `slots` addressable slots, with the room of
    /// `extra_args` extra arguments after them, and return its position:
    /// `zend_vm_stack_push_call_frame_ex` over
    /// `zend_vm_calc_used_stack`. A frame the top page cannot hold takes
    /// a page of its own from `heap`, the stack's page size or, for a
    /// frame larger than that, the page size it rounds up to; a heap
    /// that cannot give it refuses the push with the fatal php raises.
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
    pub fn push_frame(
        &mut self,
        heap: &mut Heap,
        slots: usize,
        extra_args: usize,
        header: H,
    ) -> Result<FrameRef, Fatal> {
        self.open(heap)?;
        let used = FRAME_HEADER_SLOTS + slots + extra_args;
        let fits = self
            .pages
            .last()
            .is_some_and(|page| used <= page.slots - page.top);
        let placed = if fits {
            let from = self.pages.last().map_or(0, |page| page.top);
            Placed::On { from }
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
            self.pages.push(Page::take(heap, page)?);
            Placed::Opened
        };
        let Some(page) = self.pages.last_mut() else {
            return Err(Fatal::Error(String::from(
                "InternalError: a VM stack with no page after its push laid one down",
            )));
        };
        let from = page.top;
        page.top += used;
        let at = page.slot(from + FRAME_HEADER_SLOTS);
        Ok(self.lay(at, slots, placed, header))
    }

    /// A frame of `len` slots from `at` laid on top of the stack where
    /// `placed` says it stands, its header written in the room before
    /// `at` and every slot `UNDEF`, as `i_init_func_execute_data` finds
    /// them; the room is the push's, `FRAME_HEADER_SLOTS` and `len` slots
    /// long, which nothing else names.
    fn lay(&mut self, at: NonNull<Zval>, len: usize, placed: Placed, header: H) -> FrameRef {
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
        unsafe { frame.header_at().write(header) };
        frame.cells_mut().fill(Zval::UNDEF);
        self.resume_frame(frame)
    }

    /// Push the frame of a call php lays down nowhere, an unobserved
    /// frameless call's, of `slots` slots and the header's room before
    /// them: in a room off the pages and off `heap` ([`Loose`]), the
    /// one the last such frame gave back where one was. php's
    /// `ZEND_FRAMELESS_ICALL_*` pushes no frame unless an observer
    /// watches the function (`Z/Zend/zend_vm_def.h:9775`), so this one
    /// opens no page where the top page has no room for it, reads in
    /// neither `memory_get_usage()` nor its peak, and leaves the pages
    /// as they stood for what the native pushes while it is served.
    /// More than three slots, past what a frameless call passes, is the
    /// engine's error.
    pub fn push_frameless(&mut self, slots: usize, header: H) -> Result<FrameRef, Fatal> {
        if slots > LOOSE_OPERANDS {
            return Err(Fatal::Error(format!(
                "InternalError: a frameless call's frame of {slots} slots, past the {LOOSE_OPERANDS} operands its room holds"
            )));
        }
        let room = self.rooms.pop().unwrap_or_else(Loose::new);
        // SAFETY: the room holds `LOOSE_SLOTS` slots, so the first CV's
        // place, `FRAME_HEADER_SLOTS` slots in, is inside it.
        let at = unsafe { room.at().add(FRAME_HEADER_SLOTS) };
        Ok(self.lay(at, slots, Placed::Loose(room), header))
    }

    /// Pop the frame the loop is executing,
    /// `zend_vm_stack_free_call_frame`: the page it stood on keeps its
    /// room for the next push, and a page the frame opened goes back to
    /// `heap`.
    pub fn pop_frame(&mut self, heap: &mut Heap) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        self.leave(heap, frame);
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
        let Some((from, len, old)) = self
            .frames
            .last()
            .filter(|_| top == Some(f.0 as usize))
            .map(|frame| (frame.placed.page_from(), frame.len, frame.at))
        else {
            return Err(Fatal::Error(String::from(
                "InternalError: a VM stack frame grown that is not the top one",
            )));
        };
        let Some(from) = from else {
            return Err(Fatal::Error(String::from(
                "InternalError: a frame off the VM stack's pages grown on it",
            )));
        };
        let fits = self
            .pages
            .last()
            .is_some_and(|page| page.slots - page.top > more);
        if fits {
            if let Some(page) = self.pages.last_mut() {
                page.top += more;
            }
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
            let start = page.top;
            page.top += used;
            // SAFETY: the frame's room starts `FRAME_HEADER_SLOTS` slots
            // before its first CV, inside the page it stands on.
            let room = unsafe { old.sub(FRAME_HEADER_SLOTS) };
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
            let bottom = if self.pages.len() == 1 {
                self.kind.bottom()
            } else {
                0
            };
            if let Some(below) = self.pages.last_mut() {
                below.top = from;
            }
            if self
                .pages
                .last()
                .is_some_and(|below| below.top == PAGE_HEADER_SLOTS + bottom)
                && let Some(below) = self.pages.pop()
            {
                below.give_back(heap);
            }
            self.pages.push(page);
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
    /// room off the pages, kept for the next frameless call.
    fn leave(&mut self, heap: &mut Heap, frame: Frame<H>) {
        match frame.placed {
            Placed::Heap { .. } => frame.free(heap),
            Placed::Loose(room) => self.rooms.push(room),
            Placed::On { from } => {
                if let Some(page) = self.pages.last_mut() {
                    page.top = from;
                }
            }
            Placed::Opened => {
                if let Some(page) = self.pages.pop() {
                    page.give_back(heap);
                }
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
        let start = match frame.placed {
            Placed::Heap { .. } => return Ok(Some(frame)),
            Placed::On { from } => from,
            Placed::Opened => PAGE_HEADER_SLOTS,
            Placed::Loose(_) => {
                self.frames.push(frame);
                return Err(Fatal::Error(String::from(
                    "InternalError: a frameless call's frame moved off the VM stack",
                )));
            }
        };
        let Some(page) = self.pages.last() else {
            return Err(Fatal::Error(String::from(
                "InternalError: a frame moved off a VM stack with no page",
            )));
        };
        let used = page.top.saturating_sub(start);
        let block = match heap.mm.alloc(used * SLOT) {
            Ok(block) => block.cast::<Zval>(),
            Err(fatal) => {
                self.frames.push(frame);
                return Err(fatal);
            }
        };
        // SAFETY: the page's `used` slots from `start` are the frame's,
        // inside the page's allocation, and the block was just allocated
        // for `used` slots; the two do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(page.slot(start).as_ptr(), block.as_ptr(), used) };
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
        let at = u32::try_from(self.frames.len()).unwrap_or(u32::MAX);
        self.frames.push(frame);
        FrameRef(at)
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
        let at = self.push_frame(heap, len, 0, header)?;
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
            top: PAGE_HEADER_SLOTS,
        })
    }

    /// The page's slot `at`, which is inside it.
    fn slot(&self, at: usize) -> NonNull<Zval> {
        // SAFETY: every caller's `at` is under `slots`, the slots of the
        // one allocation `take` made, so the offset stays inside it.
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
    frames: Vec<Frame<H>>,
    entries: Vec<FrameRef>,
}

impl<H> VmStack<H> {
    /// The stack's frames and entry slots set aside, its pages kept with
    /// the room the frames hold: what is pushed next lands above them on
    /// the same page, as a frame pushed with no `execute_data` standing
    /// lands on php's one VM stack, and is numbered from the bottom.
    pub fn set_aside(&mut self) -> FramesAside<H> {
        FramesAside {
            frames: std::mem::take(&mut self.frames),
            entries: std::mem::take(&mut self.entries),
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
            dropped.push(frame.header_copied());
            self.leave(heap, frame);
        }
        self.frames = aside.frames;
        self.entries = aside.entries;
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
        let outer = stack.push_frame(&mut heap, 2, 0, Head::default());
        assert!(outer.is_ok(), "a first frame fits the first page");
        let Ok(outer) = outer else {
            return;
        };
        let guard = stack.push_entry(outer);
        let inner = stack.push_frame(&mut heap, 1, 0, Head::default());
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
        assert!(stack.push_frame(&mut heap, 2, 0, made(1)).is_ok());
        let aside = stack.set_aside();
        assert!(stack.push_frame(&mut heap, 2, 0, made(2)).is_ok());
        assert!(stack.push_frame(&mut heap, 2, 0, made(3)).is_ok());
        let dropped = stack.put_back(&mut heap, aside);
        assert_eq!(dropped, [made(3), made(2)]);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.header(FrameRef(0)), Some(&made(1)));
        let aside = stack.set_aside();
        assert_eq!(stack.put_back(&mut heap, aside), []);
        assert!(stack.push_frame(&mut heap, 2, 0, made(4)).is_ok());
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
            assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
        }
        assert_eq!(heap.memory_usage(false) - start, PAGE_SIZE);
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
        assert_eq!(heap.memory_usage(false) - start, 2 * PAGE_SIZE);
        stack.pop_frame(&mut heap);
        assert_eq!(heap.memory_usage(false) - start, PAGE_SIZE);
        // A frame larger than a page takes one of its own, rounded up
        // to the page size past the header.
        assert!(
            stack
                .push_frame(&mut heap, PAGE_SIZE / SLOT, 0, Head::default())
                .is_ok()
        );
        assert_eq!(heap.memory_usage(false) - start, 3 * PAGE_SIZE);
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
            assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
        }
        let full = heap.memory_usage(false);
        assert_eq!(full - start, PAGE_SIZE);
        let called = Head {
            function: 7,
            ..Head::default()
        };
        heap.reset_peak();
        let call = stack.push_frameless(3, called.clone());
        assert!(call.is_ok());
        let Ok(call) = call else {
            return;
        };
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
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
        assert_eq!(heap.memory_usage(false) - start, 2 * PAGE_SIZE);
        assert!(stack.push_frameless(1, Head::default()).is_ok());
        assert!(stack.push_frameless(4, Head::default()).is_err());
        assert_eq!(stack.current().0, 1094);
        stack.pop_frame(&mut heap);
        stack.pop_frame(&mut heap);
        assert_eq!(stack.rooms.len(), 1);
        assert_eq!(heap.memory_usage(false), full);
        stack.pop_frame(&mut heap);
        // On a page with room, as on a full one: the room is the one
        // given back, and the page's top stands where it stood.
        stack.pop_frame(&mut heap);
        assert_eq!(stack.rooms.len(), 2);
        let again = stack.push_frameless(2, Head::default());
        assert_eq!(
            again
                .ok()
                .and_then(|f| stack.frame(f))
                .map(|f| f.slot(Slot(0))),
            Some(Zval::UNDEF)
        );
        assert_eq!(stack.rooms.len(), 1);
        stack.pop_frame(&mut heap);
        assert_eq!(stack.rooms.len(), 2);
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
        assert_eq!(heap.memory_usage(false), full);
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
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
            .map(|_| stack.push_frame(&mut heap, 10, 0, Head::default()))
            .find_map(Result::err);
        assert_eq!(
            refused,
            Some(Fatal::Error(String::from(
                "Allowed memory size of 4194304 bytes exhausted (tried to allocate 262144 bytes)"
            )))
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
            assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
        }
        assert_eq!(heap.memory_usage(false) - start, FIBER_PAGE_SIZE);
        assert!(stack.push_frame(&mut heap, 10, 0, Head::default()).is_ok());
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
        let pushed = stack.push_frame(&mut heap, 2, 0, written.clone());
        assert!(pushed.is_ok());
        let Ok(f) = pushed else {
            return;
        };
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
        assert!(matches!(moved, Ok(Some(_))), "the frame moves to the heap");
        let Ok(Some(moved)) = moved else {
            return;
        };
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
        let pushed = stack.push_frame(&mut heap, 2, 0, written.clone());
        assert!(pushed.is_ok(), "a first frame fits");
        let Ok(f) = pushed else {
            return;
        };
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
        let pushed = stack.push_frame(&mut heap, 1, 0, Head::default());
        assert!(pushed.is_ok(), "a frame above fits on a fresh page");
        let Ok(g) = pushed else {
            return;
        };
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
        let pushed = stack.push_frame(&mut heap, 2, 0, written.clone());
        assert!(pushed.is_ok());
        let Ok(f) = pushed else {
            return;
        };
        if let Some(frame) = stack.frame_mut(f) {
            frame.set(Slot(1), Zval::long(8));
        }
        let moved = stack.move_to_heap(&mut heap);
        let Ok(Some(moved)) = moved else {
            return;
        };
        let held = heap.memory_usage(false);
        let back = stack.restore_frame(&mut heap, moved);
        assert!(back.is_ok());
        let Ok(back) = back else {
            return;
        };
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
}
