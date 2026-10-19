//! The heaps the engine keeps between requests ([`ChunkCache`]): what a
//! php-fpm child keeps of its heap from one request to the next, held
//! where one engine serves many threads.
//!
//! php's `zend_mm_shutdown(heap, full=0, silent)` (`Z/Zend/zend_alloc.c:2434`)
//! ends a request without giving its heap back. The huge blocks are
//! unmapped; every chunk but the main one joins `heap->cached_chunks`;
//! `avg_chunks_count` becomes the mean of itself and the request's
//! `peak_chunks_count`, and cached chunks are unmapped while there are
//! more than it keeps (`cached_chunks_count + 0.9 > avg_chunks_count`); and
//! the main chunk is laid down again. The next request starts holding the
//! main chunk and what was cached, and `memory_get_usage(true)` reads them
//! all: a child's request after one that peaked at five chunks starts at
//! three, then two, then one (`zend_mm_shutdown`'s `real_size =
//! (cached_chunks_count + 1) * ZEND_MM_CHUNK_SIZE`).
//!
//! Here a heap is its request's and is dropped with it (design §3.4), and
//! the state that outlives a request is the engine's (§5.14), so what
//! `zend_mm_shutdown(full=0)` keeps of a heap, its main chunk, its cached
//! chunks and its average, is handed at the heap's drop to the engine's
//! cache, and the next heap made in it starts from the newest kept: one
//! kept heap for each heap that stood at once, as a pool of php-fpm
//! children keeps one heap each, and a host that serves a request after
//! another on one thread reads at each first line what one child reads.
//! php zeroes nothing of a kept chunk but its header, laid down again
//! before a heap hands out its pages, and nothing here reads one before.
//! A heap reaches the cache once as it starts and once as it ends, and
//! never on an allocation.

use std::ptr::NonNull;
use std::sync::Mutex;

use super::{CHUNK_SIZE, Chunk, chunk_free};

/// The heaps the engine keeps between requests, which every heap made
/// with [`crate::Heap::new_in`] starts from and is kept in at its end,
/// as a php-fpm child keeps its heap (`zend_mm_shutdown(full=0)`).
/// Dropping it unmaps every chunk it keeps.
#[derive(Debug, Default)]
pub struct ChunkCache {
    kept: Mutex<Vec<Kept>>,
}

/// What `zend_mm_shutdown(full=0)` leaves of one heap: its main chunk,
/// the chunks it caches, the newest last, and its `avg_chunks_count`.
#[derive(Debug)]
pub(super) struct Kept {
    pub(super) main: NonNull<Chunk>,
    pub(super) cached: Vec<NonNull<Chunk>>,
    pub(super) avg_chunks_count: f64,
}

// SAFETY: a kept heap's chunks are mappings no heap names and nothing
// reads; the cache's mutex hands them to one thread's heap at a time.
unsafe impl Send for Kept {}

impl ChunkCache {
    /// An empty cache: the first heap maps its main chunk.
    #[must_use]
    pub fn new() -> ChunkCache {
        ChunkCache::default()
    }

    fn kept(&self) -> std::sync::MutexGuard<'_, Vec<Kept>> {
        self.kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The heap kept last, for a heap that starts.
    pub(super) fn take(&self) -> Option<Kept> {
        self.kept().pop()
    }

    /// A heap that ended, kept for the next.
    pub(super) fn keep(&self, kept: Kept) {
        self.kept().push(kept);
    }
}

impl Drop for ChunkCache {
    fn drop(&mut self) {
        let kept = self
            .kept
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for heap in kept.drain(..) {
            chunk_free(heap.main, CHUNK_SIZE);
            for chunk in heap.cached {
                chunk_free(chunk, CHUNK_SIZE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Relaxed;

    use super::ChunkCache;
    use crate::Heap;
    use crate::mm::{CHUNK_SIZE, MAPPINGS};
    use crate::serial::alone;

    /// What a request reads at its first line: `memory_get_usage()`,
    /// `memory_get_usage(true)`, both peaks and the limit.
    fn first_line(heap: &Heap) -> [usize; 5] {
        [
            heap.memory_usage(false),
            heap.memory_usage(true),
            heap.memory_peak(),
            heap.memory_real_peak(),
            heap.memory_limit(),
        ]
    }

    /// Whole pages held until the heap holds `chunks` chunks.
    fn grow_to(heap: &mut Heap, chunks: usize) -> Vec<crate::Str> {
        let mut held = Vec::new();
        while heap.memory_usage(true) < chunks * CHUNK_SIZE {
            let allocated = heap.string(&[b'x'; 4000]);
            assert!(allocated.is_ok(), "the heap would not allocate a run");
            if let Ok(s) = allocated {
                held.push(s);
            }
        }
        held
    }

    /// php-fpm's child maps its heap once (`zend_mm_shutdown(full=0)`):
    /// the second request of a host that serves many maps nothing, and
    /// reads at its first line what the first read.
    #[test]
    fn a_second_request_maps_no_chunk_and_reads_what_the_first_read() {
        let _alone = alone();
        let before = MAPPINGS.load(Relaxed);
        let cache = Arc::new(ChunkCache::new());
        let mut read = Vec::new();
        for request in 0..2 {
            let heap = Heap::new_in(Arc::clone(&cache));
            read.push(first_line(&heap));
            assert_eq!(
                MAPPINGS.load(Relaxed),
                before + 1,
                "request {request} holds the one mapping"
            );
            drop(heap);
            assert_eq!(MAPPINGS.load(Relaxed), before + 1, "and the cache keeps it");
        }
        assert_eq!(read[0], read[1]);
        assert_eq!(read[0][1], CHUNK_SIZE);
        drop(cache);
        assert_eq!(MAPPINGS.load(Relaxed), before, "the cache's end unmaps it");
    }

    /// The numbers a php-fpm child reads at the first line of each request
    /// after one that peaked at five chunks: `memory_get_usage(true)` and
    /// its peak at three chunks, then two, then one, as
    /// `zend_mm_shutdown(full=0)` keeps cached chunks against an average
    /// it halves towards each request's peak; a kept chunk is taken before
    /// a mapping as the heap grows again.
    #[test]
    fn a_request_after_one_that_peaked_at_five_chunks_starts_at_three_then_two_then_one() {
        let _alone = alone();
        let before = MAPPINGS.load(Relaxed);
        let cache = Arc::new(ChunkCache::new());
        let mut heap = Heap::new_in(Arc::clone(&cache));
        // php's fast shutdown frees none of what the request holds before
        // `zend_mm_shutdown`, so what the engine's teardown frees after
        // its start leaves every chunk to be kept or unmapped there.
        let held = grow_to(&mut heap, 5);
        assert_eq!(heap.memory_usage(true), 5 * CHUNK_SIZE);
        heap.fast_shutdown();
        for s in held {
            heap.release_string(s);
        }
        drop(heap);
        assert_eq!(MAPPINGS.load(Relaxed) - before, 3, "three chunks are kept");
        let mut started = Vec::new();
        for _ in 0..3 {
            let heap = Heap::new_in(Arc::clone(&cache));
            started.push((heap.memory_usage(true), heap.memory_real_peak()));
        }
        let chunks = |n| (n * CHUNK_SIZE, n * CHUNK_SIZE);
        assert_eq!(started, vec![chunks(3), chunks(2), chunks(1)]);
        let mut heap = Heap::new_in(Arc::clone(&cache));
        let mapped = MAPPINGS.load(Relaxed);
        let held = grow_to(&mut heap, 2);
        assert_eq!(
            MAPPINGS.load(Relaxed),
            mapped + 1,
            "a heap of one maps its second"
        );
        for s in held {
            heap.release_string(s);
        }
        drop(heap);
        drop(cache);
        assert_eq!(MAPPINGS.load(Relaxed), before);
    }

    /// Each heap that stood at once is kept, as each php-fpm child keeps
    /// its own, and a request after them takes one and maps nothing.
    #[test]
    fn the_cache_keeps_a_heap_for_each_that_stood_at_once() {
        let _alone = alone();
        let before = MAPPINGS.load(Relaxed);
        let cache = Arc::new(ChunkCache::new());
        let three: Vec<Heap> = (0..3).map(|_| Heap::new_in(Arc::clone(&cache))).collect();
        drop(three);
        assert_eq!(MAPPINGS.load(Relaxed) - before, 3);
        let heap = Heap::new_in(Arc::clone(&cache));
        assert_eq!(
            MAPPINGS.load(Relaxed) - before,
            3,
            "the fourth mapped nothing"
        );
        assert_eq!(heap.memory_usage(true), CHUNK_SIZE);
        drop(heap);
        drop(cache);
        assert_eq!(MAPPINGS.load(Relaxed), before);
    }
}
