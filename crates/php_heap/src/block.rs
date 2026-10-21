//! A block of the request heap with no header of its own (design
//! §5.13): what php's `emalloc(size)` hands a caller that keeps the size
//! beside the pointer. php's `php_output` layer is one such caller, for its
//! handler records, their buffers and its stack's elements
//! (`Z/main/output.c:865-875`, `Z/Zend/zend_stack.c:33`), and so is the
//! `Content-type` the first write lays down (`Z/main/SAPI.c:299`). The
//! heap counts a block by the bin its size falls in, as it counts a
//! string, and the block is given back through the heap and by nothing
//! else.

use std::ptr::NonNull;

use crate::Heap;
use crate::hooks::Fatal;

/// One `emalloc` of `len` bytes with one owner: the type is neither
/// `Copy` nor `Clone`, so [`Heap::free_block`] takes the only name the
/// bytes have, and they are read and written through it alone. The
/// bytes are zero until written, which `ecalloc` promises and `emalloc`
/// leaves unsaid; nothing reads the difference.
#[derive(Debug)]
pub struct Block {
    at: NonNull<u8>,
    len: usize,
}

// SAFETY: a block is the request's, moved with it and read on its
// thread alone, as a `Str` is.
unsafe impl Send for Block {}

impl Block {
    /// The bytes asked for, which is not the bin's size.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes, as written.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the heap handed `len` initialised bytes at `at` to this
        // block alone, and holds them until it takes the block back.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    /// The first byte's address, for a C library that holds the block
    /// and writes it through its own pointer; making it makes no
    /// reference to the bytes.
    #[must_use]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.at.as_ptr()
    }

    /// The bytes, to write.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, and the borrow of `self` is the one writer.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }
}

impl Heap {
    /// `ecalloc(1, len)`: `len` zero bytes out of this heap, counted by
    /// the bin, `run` or huge block the size falls in, and the memory
    /// limit met before any is written.
    pub fn block(&mut self, len: usize) -> Result<Block, Fatal> {
        let at = self.mm.alloc(len)?;
        // SAFETY: the allocation is `len` bytes nothing else holds.
        unsafe { at.as_ptr().write_bytes(0, len) };
        Ok(Block { at, len })
    }

    /// `erealloc(block, len)`: the same bytes to `min(old, len)`, zero
    /// past them, in a block of `len`, with the accounting
    /// `zend_mm_realloc_heap` leaves: the old size out, the new in, and
    /// the peak untouched by the two standing together.
    // The block is taken by value so that no name of the old bytes
    // survives the call; clippy reads the `Copy` fields as an unconsumed
    // argument.
    #[allow(clippy::needless_pass_by_value)]
    pub fn block_resize(&mut self, block: Block, len: usize) -> Result<Block, Fatal> {
        // The block's one name, taken: what the heap says about the old
        // bytes after this is said through the new one.
        let Block { at, len: old } = block;
        // SAFETY: `at` came from this heap's `block` for `old` bytes and
        // `block` was its only name, given up above.
        let at = unsafe { self.mm.realloc(at, old, len) }?;
        let mut block = Block { at, len };
        if len > old {
            block.bytes_mut()[old..].fill(0);
        }
        Ok(block)
    }

    /// `efree(block)`: the bytes back to the bin they came from.
    // Taken by value for the reason `block_resize` is.
    #[allow(clippy::needless_pass_by_value)]
    pub fn free_block(&mut self, block: Block) {
        let Block { at, .. } = block;
        // SAFETY: `at` came from this heap's `block` and `block` was its
        // only name, given up above.
        unsafe { self.mm.free(at) };
    }
}

#[cfg(test)]
mod tests {
    use crate::Heap;
    use crate::serial::alone;

    /// A block is counted by the bin its size falls in, a resize keeps
    /// the bytes and moves the count by the difference of the two bins
    /// with the peak where the larger footprint put it, and a free reads
    /// back where the heap started.
    #[test]
    fn a_block_is_counted_by_its_bin_resized_in_place_of_itself_and_given_back() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let peak = heap.memory_peak();
        let mut block = heap.block(25).expect("a block");
        assert_eq!(block.len(), 25);
        assert!(block.bytes().iter().all(|b| *b == 0));
        assert_eq!(heap.memory_usage(false) - start, 32);
        block.bytes_mut()[..3].copy_from_slice(b"abc");
        // A size in the same bin is the same slot, `zend_mm_realloc_heap`'s
        // reallocation in place.
        let stood = block.at;
        let block = heap.block_resize(block, 30).expect("a resized block");
        assert_eq!(block.at, stood);
        assert_eq!(heap.memory_usage(false) - start, 32);
        let block = heap.block_resize(block, 16384).expect("a resized block");
        assert_eq!(&block.bytes()[..4], b"abc\0");
        assert_eq!(block.bytes()[16383], 0);
        assert_eq!(heap.memory_usage(false) - start, 16384);
        // php's `zend_mm_realloc_slow` restores the peak: the 32 and the
        // 16384 never count together.
        assert_eq!(heap.memory_peak(), peak.max(start + 16384));
        let block = heap.block_resize(block, 10).expect("a resized block");
        assert_eq!(&block.bytes()[..4], b"abc\0");
        assert_eq!(heap.memory_usage(false) - start, 16);
        heap.free_block(block);
        assert_eq!(heap.memory_usage(false), start);
    }
}
