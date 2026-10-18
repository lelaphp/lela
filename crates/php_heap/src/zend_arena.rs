//! `CG(arena)`, the compiler's arena (`Z/Zend/zend_arena.h`): request
//! memory php carves without ever freeing a piece of it, laid down by
//! `init_compiler` at 64 KiB on every request
//! (`Z/Zend/zend_compile.c:455`) and destroyed whole by
//! `shutdown_compiler`. The runtime caches of functions and of real
//! closures are carved from it (`init_func_run_time_cache_i`,
//! `zend_create_closure_ex`), which is why its blocks, and only its
//! blocks, are what `memory_get_usage()` sees of them (design §5.13,
//! #2863).
//!
//! It is not the compile's arena: `php_syntax::Arena` holds a compile's
//! syntax tree and temporaries on the system allocator, the recorded
//! approximation, and is reset after every compile. This one is the
//! request's, on its heap, and lives as long as the request.

use std::ptr::NonNull;

use crate::Fatal;
use crate::mm::Mm;

/// The bytes `zend_arena_create` asks for: `64 * 1024`.
pub(crate) const FIRST_BLOCK: usize = 64 * 1024;

/// `ZEND_MM_ALIGNED_SIZE(sizeof(zend_arena))`: the block's own header,
/// `ptr`, `end` and `prev`, which stands at the start of every block.
const HEADER: usize = 3 * std::mem::size_of::<usize>();

/// `ZEND_MM_ALIGNED_SIZE`: what `zend_arena_alloc` rounds a request to.
const fn aligned(size: usize) -> usize {
    (size + 7) & !7
}

/// The arena's blocks, each one `emalloc` of the request's heap, and the
/// bump of the newest: php's chain of `zend_arena` headers, `ptr` and
/// `end` read as offsets into the newest block.
#[derive(Debug, Default)]
pub(crate) struct ZendArena {
    /// Every block laid down, the newest last.
    blocks: Vec<NonNull<u8>>,
    /// `arena->ptr - (char *) arena`: the bytes of the newest block
    /// carved, its header among them.
    used: usize,
    /// `arena->end - (char *) arena`: the newest block's size.
    size: usize,
}

impl ZendArena {
    /// `zend_arena_create(64 * 1024)` out of `mm`. A machine that cannot
    /// give the block leaves the arena empty, and the first carve asks
    /// for one.
    pub(crate) fn new(mm: &mut Mm) -> ZendArena {
        let mut arena = ZendArena::default();
        arena.lay(mm, FIRST_BLOCK).ok();
        arena
    }

    /// One more block of `size` bytes, its header carved.
    fn lay(&mut self, mm: &mut Mm, size: usize) -> Result<(), Fatal> {
        let block = mm.alloc(size)?;
        self.blocks.push(block);
        self.used = HEADER;
        self.size = size;
        Ok(())
    }

    /// `zend_arena_alloc(&CG(arena), size)`: `size` rounded up to eight
    /// carved from the newest block where it has the room; otherwise a
    /// new block, the size of the newest or the request and a header
    /// where that is larger, laid down after it and carved from. The
    /// `Err` is the memory limit's fatal, with nothing carved.
    pub(crate) fn carve(&mut self, mm: &mut Mm, size: usize) -> Result<(), Fatal> {
        let size = aligned(size);
        if !self.blocks.is_empty() && size <= self.size - self.used {
            self.used += size;
            return Ok(());
        }
        let next = if self.blocks.is_empty() {
            FIRST_BLOCK.max(size + HEADER)
        } else if size + HEADER > self.size {
            size + HEADER
        } else {
            self.size
        };
        self.lay(mm, next)?;
        self.used += size;
        Ok(())
    }

    /// `zend_arena_destroy` in a debug build's teardown before the
    /// request leak sensor, as `shutdown_compiler` destroys it before
    /// `zend_mm_shutdown`: every block back to `mm`.
    #[cfg(debug_assertions)]
    pub(crate) fn destroy(&mut self, mm: &mut Mm) {
        for block in self.blocks.drain(..) {
            // SAFETY: the block came from this heap's `mm` and the arena
            // was its only name.
            unsafe { mm.free(block) };
        }
        self.used = 0;
        self.size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{FIRST_BLOCK, HEADER};
    use crate::Heap;
    use crate::serial::alone;

    /// The arena is the heap's first allocation, 64 KiB; a carve inside
    /// its room moves nothing, one past it lays a block of the same
    /// size, and one larger than a block lays a block of its own size
    /// and a header, which the next carve past its room copies.
    #[test]
    fn a_carve_is_counted_only_where_it_lays_a_block_of_the_arena_s_size_or_its_own() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        assert!(start >= FIRST_BLOCK);
        assert!(heap.carve(16).is_ok());
        assert!(heap.carve(FIRST_BLOCK - HEADER - 16).is_ok());
        assert_eq!(heap.memory_usage(false), start);
        assert!(heap.carve(1).is_ok());
        assert_eq!(heap.memory_usage(false), start + FIRST_BLOCK);
        assert!(heap.carve(FIRST_BLOCK).is_ok());
        let past = start + 2 * FIRST_BLOCK + 4096;
        assert_eq!(heap.memory_usage(false), past);
        assert!(heap.carve(FIRST_BLOCK).is_ok());
        assert_eq!(heap.memory_usage(false), past + FIRST_BLOCK + 4096);
    }

    /// A carve the memory limit refuses is the limit's fatal, and the
    /// arena carves on from the block it had.
    #[test]
    fn a_refused_block_is_the_limit_s_fatal_and_the_arena_keeps_its_block() {
        let _alone = alone();
        let mut heap = Heap::new();
        assert!(heap.set_memory_limit(2 * 1024 * 1024));
        assert!(heap.carve(4 * 1024 * 1024).is_err());
        heap.overflow_rendered();
        let start = heap.memory_usage(false);
        assert!(heap.carve(8).is_ok());
        assert_eq!(heap.memory_usage(false), start);
    }
}
