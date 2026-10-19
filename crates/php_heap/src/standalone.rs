//! A heap apart from the request's ([`StandaloneHeap`]): php's
//! `zend_mm_startup`, `zend_mm_gc` and `zend_mm_shutdown(heap, full=1,
//! silent=0)` over a `zend_mm_heap` no request allocates from, what
//! `zend_test_gh18756` drives (`Z/Zend/tests/gh18756.phpt`).
//!
//! It is the allocator of [`crate::Heap`] alone, its main chunk and its
//! sweep, with none of what a request's heap lays in it: no object
//! store, no arena, no collector and no cache it is kept in. It hands
//! nothing out, so the only thing it holds is its main chunk, which
//! GH-18756's `zend_mm_gc` deleted from a heap that had given nothing
//! out; [`StandaloneHeap::gc`] keeps it, as the request's sweep does.

use crate::mm::Mm;

/// A `zend_mm_heap` of its own, apart from the request's: its main chunk
/// mapped at [`StandaloneHeap::startup`], swept by
/// [`StandaloneHeap::gc`] and unmapped whole by
/// [`StandaloneHeap::shutdown`] or its drop.
#[derive(Debug)]
pub struct StandaloneHeap {
    mm: Mm,
}

impl StandaloneHeap {
    /// `zend_mm_startup` (`Z/Zend/zend_alloc.c:3138`): a heap with its main
    /// chunk mapped and nothing allocated in it. A machine that cannot
    /// give the chunk leaves the heap empty, as [`crate::Heap::new`]
    /// does.
    #[must_use]
    pub fn startup() -> StandaloneHeap {
        StandaloneHeap { mm: Mm::new() }
    }

    /// `zend_mm_gc` (`Z/Zend/zend_alloc.c:2101`): the sweep of wholly free
    /// bin `runs` and of the chunks they leave empty, the main chunk
    /// kept; the bytes it collected.
    pub fn gc(&mut self) -> usize {
        self.mm.gc()
    }

    /// `zend_mm_shutdown(heap, full=1, silent=0)`
    /// (`Z/Zend/zend_alloc.c:2434`): every chunk and every huge block
    /// unmapped, the main chunk among them, and the heap gone.
    pub fn shutdown(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::StandaloneHeap;
    use crate::mm::{CHUNK_SIZE, MAPPINGS};
    use crate::serial::alone;
    use std::sync::atomic::Ordering::Relaxed;

    /// `zend_test_gh18756`'s sequence: a heap started, collected twice
    /// with nothing allocated, and shut down. Each sweep keeps the main
    /// chunk, so the heap still holds one chunk of real size, and the
    /// shutdown leaves no mapping behind.
    #[test]
    fn a_heap_apart_collected_twice_keeps_its_main_chunk_and_shuts_down_whole() {
        let _alone = alone();
        let before = MAPPINGS.load(Relaxed);
        let mut heap = StandaloneHeap::startup();
        assert_eq!(
            heap.mm.real_size(),
            CHUNK_SIZE,
            "startup maps the main chunk"
        );
        assert_eq!(heap.gc(), 0, "an empty heap has nothing to collect");
        assert_eq!(heap.gc(), 0, "an empty heap has nothing to collect");
        assert_eq!(heap.mm.real_size(), CHUNK_SIZE, "the main chunk stays");
        heap.shutdown();
        assert_eq!(
            MAPPINGS.load(Relaxed),
            before,
            "shutdown unmaps the main chunk"
        );
    }
}
