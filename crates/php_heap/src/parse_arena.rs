//! The parse arena (#4020's ruling, signed off on #4064): a compile's
//! syntax tree and temporaries bumped into chunks the machine maps
//! whole, and the chunks a compile grew given back to the machine when
//! it is reset, rather than to the system allocator, which keeps the
//! pages a compile frees dirty in its zone.
//!
//! It is not the request's heap and counts nothing: only its own pages
//! go back, and design §5.13's recorded approximation stands for what
//! it accounts. Its first chunk is 32 KiB, as `CG(ast_arena)` starts
//! (`Z/Zend/zend_language_scanner.l:599`), and each later one twice the
//! last. A reset keeps at most one chunk, the newest, and only while it
//! is within what the caller keeps.
//!
//! A block borrows the arena and a reset takes the arena by `&mut`, so
//! nothing bumped here is named past the chunk under it: a block held
//! across a reset is a compile error.
//!
//! ```compile_fail,E0502
//! let mut arena = php_heap::ParseArena::default();
//! let block = arena.alloc(1_u32);
//! arena.reset(0);
//! assert_eq!(*block, 1);
//! ```
//!
//! ```compile_fail,E0505
//! let arena = php_heap::ParseArena::default();
//! let block = arena.alloc_slice_copy(b"abc");
//! drop(arena);
//! assert_eq!(block, b"abc");
//! ```

use std::alloc::Layout;
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

use crate::bump::Chunk;
use crate::mm::real_page_size;

/// The first chunk's bytes: `zend_arena_create(1024 * 32)`.
const FIRST_CHUNK: usize = 32 * 1024;

/// Blocks bumped into mapped chunks and given up together at a
/// [`ParseArena::reset`]. Nothing in it is dropped: what is laid here has
/// no destructor that runs. A new one maps nothing until it is first
/// used.
#[derive(Default)]
pub struct ParseArena {
    /// Every chunk mapped; the newest, the last, is the one bumped into.
    chunks: RefCell<Vec<Chunk>>,
    /// The bytes of the newest chunk handed out.
    used: Cell<usize>,
}

impl std::fmt::Debug for ParseArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParseArena")
            .field("allocated", &self.allocated_bytes())
            .finish_non_exhaustive()
    }
}

impl ParseArena {
    /// The bytes of every chunk mapped.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.chunks.borrow().iter().map(Chunk::len).sum()
    }

    /// Give up every block at once, keeping the newest chunk for the next
    /// compile where it is `kept` bytes or fewer, and every other chunk
    /// back to the machine.
    pub fn reset(&mut self, kept: usize) {
        let chunks = self.chunks.get_mut();
        let newest = chunks.pop().filter(|newest| newest.len() <= kept);
        chunks.clear();
        chunks.extend(newest);
        self.used.set(0);
    }

    /// A block of `layout`, bumped into the newest chunk or into a chunk
    /// mapped for it. The machine refusing a chunk ends the process, as
    /// the system allocator's refusal does.
    fn block(&self, layout: Layout) -> NonNull<u8> {
        if layout.size() == 0 {
            return NonNull::new(std::ptr::without_provenance_mut(layout.align()))
                .unwrap_or(NonNull::dangling());
        }
        if let Some(at) = self.bumped(layout) {
            return at;
        }
        let Some(chunk) = self.next_chunk(layout).and_then(Chunk::map) else {
            std::alloc::handle_alloc_error(layout)
        };
        self.chunks.borrow_mut().push(chunk);
        self.used.set(0);
        match self.bumped(layout) {
            Some(at) => at,
            None => std::alloc::handle_alloc_error(layout),
        }
    }

    /// The bytes of the chunk mapped for `layout`: twice the newest, the
    /// first at [`FIRST_CHUNK`], and at least the block, in whole pages.
    fn next_chunk(&self, layout: Layout) -> Option<usize> {
        let newest = self.chunks.borrow().last().map_or(0, Chunk::len);
        let at_least = layout.size().checked_add(layout.align())?;
        (newest.saturating_mul(2).max(FIRST_CHUNK))
            .max(at_least)
            .checked_next_multiple_of(real_page_size())
    }

    /// A block of `layout` in the newest chunk, where it fits.
    fn bumped(&self, layout: Layout) -> Option<NonNull<u8>> {
        let chunks = self.chunks.borrow();
        let newest = chunks.last()?;
        let base = newest.at().as_ptr();
        let top = base.addr().checked_add(self.used.get())?;
        let start = top.checked_next_multiple_of(layout.align())? - base.addr();
        let end = start.checked_add(layout.size())?;
        if end > newest.len() {
            return None;
        }
        self.used.set(end);
        NonNull::new(base.wrapping_add(start))
    }

    /// Whether the block at `at`, `old` bytes, is the last one bumped,
    /// and the newest chunk has room for it to be `new` bytes; it is then
    /// grown in place.
    fn grown_in_place(&self, at: NonNull<u8>, old: usize, new: usize) -> bool {
        let chunks = self.chunks.borrow();
        let Some(newest) = chunks.last() else {
            return false;
        };
        let Some(start) = at.as_ptr().addr().checked_sub(newest.at().as_ptr().addr()) else {
            return false;
        };
        if start.checked_add(old) != Some(self.used.get()) {
            return false;
        }
        let end = start.saturating_add(new);
        if end > newest.len() {
            return false;
        }
        self.used.set(end);
        true
    }

    /// `value` in the arena.
    #[expect(
        clippy::mut_from_ref,
        reason = "each block is handed out once until a reset, which takes the arena by `&mut`"
    )]
    pub fn alloc<T>(&self, value: T) -> &mut T {
        let at = self.block(Layout::new::<T>()).cast::<T>();
        // SAFETY: `at` is aligned and sized for a `T`, in a chunk no
        // other block covers.
        unsafe { at.as_ptr().write(value) };
        // SAFETY: written just above; the block is this borrow's until a
        // reset, which takes the arena by `&mut`.
        unsafe { &mut *at.as_ptr() }
    }

    /// A copy of `items` in the arena.
    #[expect(
        clippy::mut_from_ref,
        reason = "each block is handed out once until a reset, which takes the arena by `&mut`"
    )]
    pub fn alloc_slice_copy<T: Copy>(&self, items: &[T]) -> &mut [T] {
        let Ok(layout) = Layout::array::<T>(items.len()) else {
            std::alloc::handle_alloc_error(Layout::new::<T>())
        };
        let at = self.block(layout).cast::<T>();
        // SAFETY: `at` is aligned and holds `items.len()` of `T`, in a
        // chunk no other block covers, so the two do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(items.as_ptr(), at.as_ptr(), items.len()) };
        // SAFETY: every element was written just above; the block is
        // this borrow's until a reset.
        unsafe { std::slice::from_raw_parts_mut(at.as_ptr(), items.len()) }
    }
}

/// A vector grown in a [`ParseArena`], in place while it is the last
/// block bumped, and frozen to a slice of it. What it holds is never
/// dropped.
pub struct ArenaVec<'a, T> {
    arena: &'a ParseArena,
    at: NonNull<T>,
    len: usize,
    cap: usize,
}

impl<'a, T> ArenaVec<'a, T> {
    /// An empty vector in `arena` with room for `capacity`.
    #[must_use]
    pub fn with_capacity_in(capacity: usize, arena: &'a ParseArena) -> ArenaVec<'a, T> {
        const { assert!(!std::mem::needs_drop::<T>() && size_of::<T>() != 0) };
        let mut vec = ArenaVec {
            arena,
            at: NonNull::dangling(),
            len: 0,
            cap: 0,
        };
        vec.reserve(capacity);
        vec
    }

    /// Room for `more` past the length.
    pub fn reserve(&mut self, more: usize) {
        let Some(wanted) = self.len.checked_add(more) else {
            std::alloc::handle_alloc_error(Layout::new::<T>())
        };
        if wanted <= self.cap {
            return;
        }
        let cap = wanted.max(self.cap.saturating_mul(2)).max(4);
        let (Ok(old), Ok(new)) = (Layout::array::<T>(self.cap), Layout::array::<T>(cap)) else {
            std::alloc::handle_alloc_error(Layout::new::<T>())
        };
        if self.cap != 0
            && self
                .arena
                .grown_in_place(self.at.cast(), old.size(), new.size())
        {
            self.cap = cap;
            return;
        }
        let at = self.arena.block(new).cast::<T>();
        // SAFETY: the old block holds `len` initialised elements and the
        // new one room for `cap`, two blocks of the arena that do not
        // overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.at.as_ptr(), at.as_ptr(), self.len) };
        self.at = at;
        self.cap = cap;
    }

    /// `item` at the end.
    pub fn push(&mut self, item: T) {
        if self.len == self.cap {
            self.reserve(1);
        }
        // SAFETY: `len < cap`, so the slot is inside the block and unwritten.
        unsafe { self.at.as_ptr().wrapping_add(self.len).write(item) };
        self.len += 1;
    }

    /// What it holds.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        self
    }

    /// What it holds, to change in place.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self
    }

    /// What it holds, as a slice of the arena.
    #[must_use]
    pub fn into_slice(self) -> &'a [T] {
        // SAFETY: the first `len` elements are written, and the block is
        // the arena's until a reset, which takes it by `&mut`.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }
}

impl<T: Copy> ArenaVec<'_, T> {
    /// `items` at the end.
    pub fn extend_from_slice(&mut self, items: &[T]) {
        self.reserve(items.len());
        // SAFETY: `reserve` made room for `items` past the length; the
        // arena's block and `items` do not overlap, a `&[T]` of this
        // vector being impossible while `&mut self` is held.
        unsafe {
            std::ptr::copy_nonoverlapping(
                items.as_ptr(),
                self.at.as_ptr().wrapping_add(self.len),
                items.len(),
            );
        }
        self.len += items.len();
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for ArenaVec<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: PartialEq> PartialEq for ArenaVec<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for ArenaVec<'_, T> {}

impl<T> std::ops::Deref for ArenaVec<'_, T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: the first `len` elements are written; a vector of no
        // room is a dangling, aligned pointer of length zero.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }
}

impl<T> std::ops::DerefMut for ArenaVec<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`, and the vector is borrowed uniquely.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_keeps_the_newest_chunk_within_what_is_kept_and_unmaps_the_rest() {
        let mut arena = ParseArena::default();
        assert_eq!(arena.allocated_bytes(), 0, "nothing is mapped until used");
        assert_eq!(*arena.alloc(7_u64), 7);
        assert_eq!(arena.alloc_slice_copy(b"abc"), b"abc");
        let first = arena.allocated_bytes();
        assert_eq!(first, FIRST_CHUNK.next_multiple_of(real_page_size()));
        arena.reset(first);
        assert_eq!(arena.allocated_bytes(), first, "the chunk is kept");
        arena.alloc_slice_copy(&vec![0_u8; first + 1]);
        assert!(arena.allocated_bytes() > first);
        arena.reset(first);
        assert_eq!(arena.allocated_bytes(), 0, "a newest chunk past it is not");
        arena.alloc(1_u8);
        arena.alloc_slice_copy(&vec![0_u8; first]);
        let newest = arena.allocated_bytes() - first;
        arena.reset(newest);
        assert_eq!(arena.allocated_bytes(), newest, "only the newest is kept");
    }

    #[test]
    fn chunks_double_from_the_first_and_a_large_block_takes_its_own() {
        let mut arena = ParseArena::default();
        let page = real_page_size();
        let first = FIRST_CHUNK.next_multiple_of(page);
        arena.alloc(1_u8);
        arena.alloc_slice_copy(&vec![0_u8; first - 1]);
        assert_eq!(arena.allocated_bytes(), first, "the first chunk is full");
        arena.alloc(1_u8);
        assert_eq!(arena.allocated_bytes(), first + 2 * first);
        arena.reset(0);
        let large = 3 * first;
        let block = arena.alloc_slice_copy(&vec![9_u8; large]);
        assert!(block.iter().all(|&byte| byte == 9));
        let mapped = arena.allocated_bytes();
        assert!(mapped >= large && mapped.is_multiple_of(page), "{mapped}");
    }

    #[test]
    fn blocks_are_aligned_to_what_they_hold_and_do_not_overlap() {
        #[derive(Clone, Copy)]
        #[repr(align(64))]
        struct Line(u8);
        let arena = ParseArena::default();
        let byte = arena.alloc(1_u8);
        let line = arena.alloc(Line(2));
        let word = arena.alloc(3_u64);
        let lines = arena.alloc_slice_copy(&[Line(4); 3]);
        assert_eq!(std::ptr::from_ref(line).addr() % 64, 0);
        assert_eq!(lines.as_ptr().addr() % 64, 0);
        assert_eq!(std::ptr::from_ref(word).addr() % 8, 0);
        assert_eq!((*byte, line.0, *word, lines[2].0), (1, 2, 3, 4));
    }

    #[test]
    fn nothing_of_no_size_takes_room() {
        let arena = ParseArena::default();
        let nothing: &mut [u64] = arena.alloc_slice_copy(&[]);
        assert!(nothing.is_empty());
        assert_eq!(arena.allocated_bytes(), 0);
    }

    #[test]
    fn a_vector_grows_in_place_while_it_is_the_last_block_and_moves_after() {
        let arena = ParseArena::default();
        let mut grown = ArenaVec::with_capacity_in(1, &arena);
        grown.push(0_u32);
        let at = grown.as_ptr();
        grown.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(grown.as_ptr(), at, "the last block grows where it stands");
        arena.alloc(9_u32);
        grown.reserve(100);
        assert_ne!(grown.as_ptr(), at, "a block after it moves it");
        for item in 7..100 {
            grown.push(item);
        }
        assert!(grown.iter().copied().eq(0..100));
        grown[0] = 7;
        let frozen = grown.into_slice();
        assert_eq!((frozen[0], frozen[99], frozen.len()), (7, 99, 100));
    }

    /// A vector past its chunk moves to the next, which the machine may
    /// lay just below or just above it; it is held by where it stands
    /// in the newest chunk, and never by an address that only abuts.
    #[test]
    fn a_vector_filling_its_chunk_moves_to_the_next_whole() {
        let arena = ParseArena::default();
        let items = FIRST_CHUNK.next_multiple_of(real_page_size()) / size_of::<u64>();
        let mut grown = ArenaVec::with_capacity_in(items, &arena);
        for item in 0..items {
            grown.push(item as u64);
        }
        grown.push(items as u64);
        assert!(arena.allocated_bytes() > items * size_of::<u64>());
        assert!(grown.iter().copied().eq(0..=items as u64));
        let other = arena.alloc(7_u64);
        grown.reserve(items);
        assert_eq!(*other, 7);
        assert!(grown.iter().copied().eq(0..=items as u64));
    }

    #[test]
    fn a_kept_chunk_serves_the_next_compile_from_its_start() {
        let mut arena = ParseArena::default();
        let first = std::ptr::from_ref(arena.alloc(1_u64)).addr();
        arena.reset(usize::MAX);
        let again = std::ptr::from_ref(arena.alloc(2_u64)).addr();
        assert_eq!(first, again);
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "a test starts its own threads (seams.toml rule fork.threads)"
    )]
    fn an_arena_moves_to_another_thread_with_its_chunks() {
        let mut arena = ParseArena::default();
        arena.alloc(1_u64);
        let room = arena.allocated_bytes();
        let moved = std::thread::spawn(move || {
            arena.reset(usize::MAX);
            arena.allocated_bytes()
        })
        .join();
        assert_eq!(moved.ok(), Some(room));
    }
}
