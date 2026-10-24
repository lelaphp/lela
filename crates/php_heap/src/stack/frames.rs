//! The frame table of one stack: the [`Frame`] records a [`FrameRef`]
//! numbers, the oldest first, in one allocation of the process's
//! allocator. php has no such table, its frames being found through
//! `prev_execute_data`; the engine names a frame by its place here, so a
//! push writes one record at the end and a pop takes the last.
//!
//! Its three words are the table's whole state, laid out as
//! [`FramesLayout`] says, so compiled code that pushes a frame inline
//! writes the record at `at + len` and bumps `len` while `len < cap`,
//! and one that pops it takes `len` back by one (`jit-design.md` §5.3,
//! #3230). The invariant every method keeps, and what compiled code must
//! keep: the first `len` records are live, `len <= cap`, and `at` is the
//! start of an allocation of `cap` records, dangling while `cap` is zero.
//!
//! [`FrameRef`]: super::FrameRef

use std::alloc::{self, Layout};
use std::marker::PhantomData;
use std::mem::offset_of;
use std::ptr::NonNull;

use super::{Frame, Placed};
use crate::Zval;

/// The records of a stack's frames. `#[repr(C)]` so that the offsets of
/// its three words are fixed by its declaration ([`FramesLayout`]).
#[repr(C)]
pub(super) struct Frames<H> {
    /// The first record; dangling while `cap` is zero.
    at: NonNull<Frame<H>>,
    /// How many records are live, from `at` on.
    len: usize,
    /// How many records the allocation at `at` holds.
    cap: usize,
    /// The table owns its records.
    owns: PhantomData<Frame<H>>,
}

// SAFETY: the table owns its records, each `Send` where its header is,
// and is the request's or the fiber's, moved with it and read on its
// thread alone, as a `Vec` of them would be.
unsafe impl<H: Send> Send for Frames<H> {}

/// Where the table's three words stand in it, and a record's fields in a
/// record, for code that reads and writes them in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramesLayout {
    /// The first record's address.
    pub at: usize,
    /// How many records are live.
    pub len: usize,
    /// How many the allocation holds.
    pub cap: usize,
    /// One record's size, its stride in the table.
    pub record: usize,
    /// A record's first slot, the frame's first CV.
    pub record_at: usize,
    /// A record's slot count.
    pub record_len: usize,
    /// A record's placement, whose first byte is its tag.
    pub record_placed: usize,
    /// The tag of a frame on the top page, where its room's address is
    /// the page's top once it pops (`zend_vm_stack_free_call_frame`).
    pub on_page: u8,
}

impl<H> Frames<H> {
    /// The table's layout, the same for every header type.
    pub(super) const LAYOUT: FramesLayout = FramesLayout {
        at: offset_of!(Frames<H>, at),
        len: offset_of!(Frames<H>, len),
        cap: offset_of!(Frames<H>, cap),
        record: size_of::<Frame<H>>(),
        record_at: offset_of!(Frame<H>, at),
        record_len: offset_of!(Frame<H>, len),
        record_placed: offset_of!(Frame<H>, placed),
        on_page: super::ON_PAGE,
    };

    /// An empty table, which allocates nothing.
    pub(super) const fn new() -> Frames<H> {
        Frames {
            at: NonNull::dangling(),
            len: 0,
            cap: 0,
            owns: PhantomData,
        }
    }

    /// How many records are live.
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The live records.
    pub(super) fn as_slice(&self) -> &[Frame<H>] {
        // SAFETY: the first `len` records at `at` are live, and a borrow of
        // the table is a borrow of them; `at` is dangling only where `len`
        // is zero, which a slice of none allows.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    /// The live records, to write.
    pub(super) fn as_mut_slice(&mut self) -> &mut [Frame<H>] {
        // SAFETY: as `as_slice`, and the mutable borrow of the table is the
        // one writer.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }

    /// `frame` the last record. The allocation doubles where it is full,
    /// four records the first time; an allocator that cannot give it is
    /// the process's out of memory, as a `Vec`'s is.
    pub(super) fn push(&mut self, frame: Frame<H>) {
        if self.len == self.cap {
            self.grow();
        }
        let place = self.at.as_ptr().wrapping_add(self.len);
        // SAFETY: `len < cap` now, so the place past the last live record
        // is inside the allocation and holds no live record.
        unsafe { place.write(frame) };
        self.len += 1;
    }

    /// The last record taken out; none for an empty table.
    pub(super) fn pop(&mut self) -> Option<Frame<H>> {
        self.len = self.len.checked_sub(1)?;
        let place = self.at.as_ptr().wrapping_add(self.len);
        // SAFETY: the record at the old last place was live, and with `len`
        // taken back by one nothing names it but this read, which moves it
        // out once.
        Some(unsafe { place.read() })
    }

    /// The last record taken off where it stands on the top page
    /// ([`Placed::OnPage`]): the room it links and its own room, read in
    /// place, and the table shortened past it, so the pop of a call in a
    /// loop copies no record through the loop's stack. Such a record owns
    /// nothing, its placement no payload and its header nothing a drop
    /// would give back ([`Frame::FITS`]), so letting it go unread drops
    /// nothing; any other record, and an empty table, is none here and
    /// is [`Frames::pop`]'s.
    #[inline]
    pub(super) fn pop_on_page(&mut self) -> Option<(usize, NonNull<Zval>)> {
        let () = Frame::<H>::FITS;
        let last = self.as_slice().last()?;
        let Placed::OnPage = last.placed else {
            return None;
        };
        let taken = (last.linked_below(), last.room());
        self.len -= 1;
        Some(taken)
    }

    /// The whole table taken, an empty one left.
    pub(super) fn take(&mut self) -> Frames<H> {
        std::mem::take(self)
    }

    /// The allocation doubled, the live records moved with it.
    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        let cap = self.cap.saturating_mul(2).max(4);
        let Ok(layout) = Layout::array::<Frame<H>>(cap) else {
            alloc::handle_alloc_error(Layout::new::<Frame<H>>());
        };
        let at = if self.cap == 0 {
            // SAFETY: a record is no zero-sized type (it holds a pointer),
            // so the layout of four or more is not zero-sized.
            unsafe { alloc::alloc(layout) }
        } else {
            let Ok(old) = Layout::array::<Frame<H>>(self.cap) else {
                alloc::handle_alloc_error(layout);
            };
            // SAFETY: `at` came from this allocator with `old`, the layout of
            // `cap` records, and the new size is a valid one for the same
            // alignment, as `layout` is.
            unsafe { alloc::realloc(self.at.as_ptr().cast(), old, layout.size()) }
        };
        let Some(at) = NonNull::new(at).map(NonNull::cast::<Frame<H>>) else {
            alloc::handle_alloc_error(layout);
        };
        self.at = at;
        self.cap = cap;
    }
}

impl<H> Default for Frames<H> {
    fn default() -> Frames<H> {
        Frames::new()
    }
}

impl<H> Drop for Frames<H> {
    fn drop(&mut self) {
        while self.pop().is_some() {}
        if self.cap == 0 {
            return;
        }
        let Ok(layout) = Layout::array::<Frame<H>>(self.cap) else {
            return;
        };
        // SAFETY: `at` came from this allocator with `layout`, the layout of
        // `cap` records, and the table, being dropped, names it last.
        unsafe { alloc::dealloc(self.at.as_ptr().cast(), layout) };
    }
}

impl<H> std::ops::Deref for Frames<H> {
    type Target = [Frame<H>];

    fn deref(&self) -> &[Frame<H>] {
        self.as_slice()
    }
}

impl<H> std::ops::DerefMut for Frames<H> {
    fn deref_mut(&mut self) -> &mut [Frame<H>] {
        self.as_mut_slice()
    }
}

impl<H: std::fmt::Debug> std::fmt::Debug for Frames<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}
