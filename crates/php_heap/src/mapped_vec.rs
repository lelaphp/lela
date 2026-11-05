//! A stack on a mapping of its own (#4020's ruling, signed off on
//! #4064), for the stacks a parse lends itself between compiles: one
//! grown past what is kept goes back to the machine whole, rather than
//! to the system allocator, which keeps the pages it frees dirty in its
//! zone.

use std::alloc::Layout;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::bump::Chunk;
use crate::mm::real_page_size;

/// A stack whose room is a mapping of its own, given back to the machine
/// whole when it is dropped. What it holds is never dropped.
pub struct MappedVec<T> {
    /// The mapping, none until room is first asked for.
    room: Option<Chunk>,
    /// How many of `T` the mapping has room for.
    cap: usize,
    len: usize,
    held: PhantomData<T>,
}

impl<T> Default for MappedVec<T> {
    fn default() -> MappedVec<T> {
        MappedVec::new()
    }
}

impl<T> MappedVec<T> {
    /// The strictest alignment a page, the least a mapping is aligned
    /// to, serves on any machine this runs on.
    const PAGE_ALIGNED: usize = 4096;

    /// An empty stack, mapping nothing.
    #[must_use]
    pub const fn new() -> MappedVec<T> {
        const {
            assert!(
                !std::mem::needs_drop::<T>()
                    && size_of::<T>() != 0
                    && align_of::<T>() <= Self::PAGE_ALIGNED
            );
        };
        MappedVec {
            room: None,
            cap: 0,
            len: 0,
            held: PhantomData,
        }
    }

    /// How many it has room for.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Where its room stands: a dangling, aligned pointer with no room.
    fn at(&self) -> *mut T {
        (self.room.as_ref())
            .map_or(NonNull::dangling(), |room| room.at().cast())
            .as_ptr()
    }

    /// Room for `more` past the length, in a mapping twice the last at
    /// least, in whole pages.
    pub fn reserve(&mut self, more: usize) {
        let Some(wanted) = self.len.checked_add(more) else {
            std::alloc::handle_alloc_error(Layout::new::<T>())
        };
        if wanted <= self.cap {
            return;
        }
        let room = (wanted.max(self.cap.saturating_mul(2)))
            .checked_mul(size_of::<T>())
            .and_then(|bytes| bytes.checked_next_multiple_of(real_page_size()))
            .and_then(Chunk::map);
        let Some(room) = room else {
            std::alloc::handle_alloc_error(Layout::new::<T>())
        };
        let to = room.at().cast::<T>().as_ptr();
        // SAFETY: the old room holds `len` initialised elements, the new
        // mapping room for at least `wanted`, and the two do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.at(), to, self.len) };
        self.cap = room.len() / size_of::<T>();
        self.room = Some(room);
    }

    /// `item` on top.
    pub fn push(&mut self, item: T) {
        if self.len == self.cap {
            self.reserve(1);
        }
        // SAFETY: `len < cap`, so the slot is inside the mapping.
        unsafe { self.at().wrapping_add(self.len).write(item) };
        self.len += 1;
    }

    /// The top, taken off.
    pub fn pop(&mut self) -> Option<T> {
        self.len = self.len.checked_sub(1)?;
        // SAFETY: the slot at the old top was written, and is no longer
        // counted, so it is read out once.
        Some(unsafe { self.at().wrapping_add(self.len).read() })
    }

    /// Every element given up, the room kept.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// The first `len` kept.
    pub fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
}

impl<T: Copy> MappedVec<T> {
    /// `len` long, cut or filled with `value`.
    pub fn resize(&mut self, len: usize, value: T) {
        self.reserve(len.saturating_sub(self.len));
        while self.len < len {
            self.push(value);
        }
        self.truncate(len);
    }
}

impl<T> FromIterator<T> for MappedVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> MappedVec<T> {
        let mut vec = MappedVec::new();
        for item in items {
            vec.push(item);
        }
        vec
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for MappedVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T> std::ops::Deref for MappedVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: the first `len` elements are written; with no mapping
        // the pointer is dangling and aligned, at length zero.
        unsafe { std::slice::from_raw_parts(self.at(), self.len) }
    }
}

impl<T> std::ops::DerefMut for MappedVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`, and the stack is borrowed uniquely.
        unsafe { std::slice::from_raw_parts_mut(self.at(), self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_maps_nothing_until_it_holds_something() {
        let mut stack = MappedVec::<u32>::default();
        assert_eq!(stack.capacity(), 0);
        assert!(stack.is_empty());
        assert_eq!(stack.pop(), None);
        stack.reserve(0);
        assert_eq!(stack.capacity(), 0);
    }

    #[test]
    fn a_stack_pushes_pops_and_cuts_through_its_growth() {
        let mut stack: MappedVec<u32> = (0..10_000).collect();
        assert_eq!(stack.len(), 10_000);
        assert!(stack.iter().copied().eq(0..10_000));
        assert_eq!(stack.pop(), Some(9_999));
        stack[0] = 42;
        stack.truncate(2);
        assert_eq!(&*stack, &[42, 1]);
        stack.truncate(5);
        assert_eq!(stack.len(), 2);
        stack.clear();
        assert!(stack.is_empty());
        assert!(stack.capacity() >= 10_000, "the room is kept");
    }

    #[test]
    fn room_is_whole_pages_and_grows_to_twice_the_last() {
        let mut stack = MappedVec::<u8>::new();
        stack.push(1);
        let page = real_page_size();
        assert_eq!(stack.capacity(), page);
        stack.resize(page + 1, 2);
        assert_eq!(stack.capacity(), 2 * page);
        assert_eq!((stack[0], stack[page]), (1, 2));
    }

    #[test]
    fn a_resize_fills_and_cuts() {
        let mut stack = MappedVec::<u32>::new();
        stack.resize(3, 7);
        assert_eq!(&*stack, &[7, 7, 7]);
        stack.resize(1, 9);
        assert_eq!(&*stack, &[7]);
        stack.resize(2, 9);
        assert_eq!(&*stack, &[7, 9]);
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a test starts its own threads (seams.toml rule fork.threads)"
    )]
    fn a_stack_moves_to_another_thread_with_its_mapping() {
        let stack: MappedVec<u32> = (0..100).collect();
        let sum = std::thread::spawn(move || stack.iter().sum::<u32>()).join();
        assert_eq!(sum.ok(), Some(4950));
    }
}
