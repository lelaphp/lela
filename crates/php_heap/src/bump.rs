//! The bump (design §5.2, §5.14): where an engine's permanent strings
//! are laid, one after another in large chunks, rather than one system
//! allocation each.
//!
//! php with opcache copies every permanent interned string into one
//! contiguous buffer as it starts, `accel_copy_permanent_strings`
//! (`Z/ext/opcache/ZendAccelerator.c:646`) over
//! `accel_new_interned_string`, so that its startup vocabulary is one
//! sequence of bytes in one mapping and not thousands of `pemalloc`
//! blocks, each with its rounding, scattered among what startup freed.
//! A [`Bump`] is that buffer: blocks handed out one after another through
//! chunks the machine maps whole, the first sized from the count its
//! table was laid for, a later one only where that count was short.
//! Nothing laid here is freed alone; the chunks go back with the bump,
//! which is the table's, as php's buffer goes back with its shared
//! memory.
//!
//! A chunk is a mapping of its own where the platform maps, so a page
//! the strings never reach is never touched and costs the process
//! nothing. Miri cannot model a mapping (the note atop `mm.rs`), so
//! under it, and where there is no `mmap`, a chunk is the system
//! allocator's, aligned to a page, and Miri sees its whole life.

use std::ptr::NonNull;

use crate::mm::real_page_size;

/// Every block a bump hands out is aligned to this, a string's header
/// being words.
const ALIGN: usize = 8;

/// The least a chunk is laid at, so a table sized for nothing lays its
/// first strings in one chunk and not one each.
const MIN_CHUNK: usize = 64 * 1024;

/// Blocks handed out through chunks laid down whole, each after the
/// last, and freed only with the bump.
pub(crate) struct Bump {
    /// Each chunk, the last the one laid into.
    chunks: Vec<Chunk>,
    /// The bytes of the last chunk handed out.
    used: usize,
    /// What the next chunk is laid at, at least: the size asked for as
    /// the bump was made, until the first chunk takes it.
    next: usize,
}

impl Bump {
    /// A bump that lays nothing down until its first block is asked
    /// for, and then a chunk of at least `bytes`.
    pub(crate) fn with_capacity(bytes: usize) -> Bump {
        Bump {
            chunks: Vec::new(),
            used: 0,
            next: bytes,
        }
    }

    /// `size` bytes, aligned to [`ALIGN`] and written by nobody yet: the
    /// last chunk's next, or the first of a chunk laid down for them.
    /// `None` where the machine refuses a chunk.
    pub(crate) fn alloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        let size = size.checked_next_multiple_of(ALIGN)?;
        if let Some(chunk) = self.chunks.last()
            && chunk.len() - self.used >= size
        {
            let at = chunk.at().as_ptr().wrapping_add(self.used);
            self.used += size;
            return NonNull::new(at);
        }
        let len = (self.next.max(size).max(MIN_CHUNK)).checked_next_multiple_of(real_page_size())?;
        let chunk = Chunk::map(len)?;
        let at = chunk.at();
        self.chunks.push(chunk);
        self.used = size;
        self.next = MIN_CHUNK;
        Some(at)
    }

    /// Whether `at` is a block this bump handed out, which says the
    /// bump frees it and nothing else does.
    pub(crate) fn holds(&self, at: NonNull<u8>) -> bool {
        let at = at.as_ptr().addr();
        (self.chunks.iter()).any(|chunk| {
            let start = chunk.at().as_ptr().addr();
            start <= at && at < start + chunk.len()
        })
    }

    /// How many chunks the bump has laid down: the allocations its
    /// blocks took between them.
    #[cfg(test)]
    pub(crate) fn chunks(&self) -> usize {
        self.chunks.len()
    }
}

impl std::fmt::Debug for Bump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bump({} chunks)", self.chunks.len())
    }
}

/// Whole pages laid down fresh, given back to the machine as it drops.
/// Whatever was laid in it goes with it, so whoever lays there holds it
/// past every name into it.
pub(crate) struct Chunk {
    at: NonNull<u8>,
    len: usize,
}

// SAFETY: a chunk is its owner's alone, and nothing about a mapping is
// the thread's that made it.
unsafe impl Send for Chunk {}

impl Chunk {
    /// `len` bytes, a whole number of pages, mapped fresh; `None` where
    /// the machine refuses.
    pub(crate) fn map(len: usize) -> Option<Chunk> {
        Some(Chunk { at: map(len)?, len })
    }

    /// Its first byte, aligned to a page.
    pub(crate) fn at(&self) -> NonNull<u8> {
        self.at
    }

    /// Its bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        unmap(self.at, self.len);
    }
}

/// `len` bytes, a whole number of pages, mapped fresh.
#[cfg(all(unix, not(miri)))]
fn map(len: usize) -> Option<NonNull<u8>> {
    crate::mm::mmap(std::ptr::null_mut(), len)
}

/// The chunk [`map`] laid down, unmapped whole.
#[cfg(all(unix, not(miri)))]
fn unmap(chunk: NonNull<u8>, len: usize) {
    crate::mm::munmap(chunk.as_ptr(), len);
}

/// Where there is no mapping to make, and under Miri, a chunk is the
/// system allocator's, aligned to a page.
#[cfg(any(not(unix), miri))]
fn map(len: usize) -> Option<NonNull<u8>> {
    let layout = std::alloc::Layout::from_size_align(len, real_page_size()).ok()?;
    // SAFETY: the layout's size is at least a page, never zero.
    NonNull::new(unsafe { std::alloc::alloc(layout) })
}

/// The system allocator's chunk back with the layout [`map`] asked for.
#[cfg(any(not(unix), miri))]
fn unmap(chunk: NonNull<u8>, len: usize) {
    let Ok(layout) = std::alloc::Layout::from_size_align(len, real_page_size()) else {
        return;
    };
    // SAFETY: the chunk came from `map` with this layout, and its
    // `Chunk` is dropping, so nothing names it.
    unsafe { std::alloc::dealloc(chunk.as_ptr(), layout) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks follow one another in one chunk until it is full, a block
    /// that does not fit laying the next chunk at its size, and every
    /// block, written, is one the bump holds.
    #[test]
    fn blocks_are_laid_one_after_another_and_a_chunk_only_when_the_last_is_full() {
        let mut bump = Bump::with_capacity(1);
        assert_eq!(bump.chunks(), 0, "nothing laid until a block is asked for");
        let first = bump.alloc(20).expect("a block");
        let second = bump.alloc(8).expect("a block");
        assert_eq!(second.as_ptr().addr(), first.as_ptr().addr() + 24);
        assert_eq!(bump.chunks(), 1);
        let rest = MIN_CHUNK.next_multiple_of(real_page_size()) - 32;
        let last = bump.alloc(rest).expect("the chunk's last block");
        assert_eq!(bump.chunks(), 1);
        let past = bump.alloc(MIN_CHUNK * 2).expect("a chunk of its own");
        assert_eq!(bump.chunks(), 2);
        for at in [first, second, last, past] {
            assert!(bump.holds(at));
            // SAFETY: each block is at least 8 bytes the bump handed out
            // and nobody else holds.
            unsafe { at.as_ptr().write_bytes(7, 8) };
        }
        let mut other = Bump::with_capacity(0);
        let elsewhere = other.alloc(8).expect("a block");
        assert!(!bump.holds(elsewhere));
    }
}
