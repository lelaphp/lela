//! The engine's free chunks ([`ChunkCache`]): what php's worker keeps
//! across requests, held where one engine serves many threads.
//!
//! php's `zend_mm_shutdown(heap, full=0, silent)` (`Z/Zend/zend_alloc.c:2434`)
//! ends a request without giving its heap back: the main chunk is laid
//! down again where it stands and the other chunks join
//! `heap->cached_chunks`, so a php-fpm child maps nothing for a request
//! that fits in what it already holds. Here a heap is the request's and
//! is dropped with it (design §3.4), and the state that outlives a
//! request is the engine's (§5.14), so the chunks a heap gives up at its
//! end go to the engine's cache, and the next heap of any thread takes
//! its main chunk, and every chunk it grows by, from there before it maps
//! one.
//!
//! What a heap reads is untouched by where its chunks came from: a chunk
//! taken from the cache is laid down by `zend_mm_chunk_init` as a fresh
//! one is, and counted as a fresh one is, so `memory_get_usage(true)` at
//! a request's first line is the one chunk php's second request reads.
//! php zeroes nothing of a cached chunk but its header (`memset(p, 0,
//! sizeof(zend_mm_chunk))`), and nothing here reads a chunk's pages before
//! a heap hands them out.
//!
//! The cache keeps as many chunks as heaps have stood at once, the most
//! main chunks it could be asked for together; a chunk given back past
//! that is unmapped. A heap reaches the cache once as it starts and once
//! as it ends, and never on an allocation.

use std::ptr::NonNull;
use std::sync::Mutex;

use super::{CHUNK_SIZE, Chunk, chunk_alloc, chunk_free};

/// The engine's cache of free chunks, which every heap made with
/// [`crate::Heap::new_in`] takes its chunks from and gives them back to
/// at its end, as a php-fpm child's heap keeps its own across requests
/// (`zend_mm_shutdown(full=0)`). Dropping it unmaps what it holds.
#[derive(Debug, Default)]
pub struct ChunkCache {
    kept: Mutex<Kept>,
}

/// What the cache holds and how many heaps it serves.
#[derive(Debug, Default)]
struct Kept {
    /// The free chunks, the newest given back last.
    free: Vec<Free>,
    /// The heaps standing now, and the most that have stood at once,
    /// which bounds `free`.
    heaps: usize,
    peak_heaps: usize,
}

/// One chunk no heap holds: a mapping of [`CHUNK_SIZE`] bytes aligned to
/// its size, whose header the next heap to take it writes again.
#[derive(Debug)]
struct Free(NonNull<Chunk>);

// SAFETY: a free chunk is a mapping no heap names and nothing reads; the
// cache's lock is what hands it to one thread's heap at a time.
unsafe impl Send for Free {}

impl ChunkCache {
    /// An empty cache: the first heap maps its main chunk.
    #[must_use]
    pub fn new() -> ChunkCache {
        ChunkCache::default()
    }

    fn kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One heap more, and its main chunk: the newest the cache holds, or
    /// a fresh mapping where it holds none.
    pub(super) fn enter(&self) -> Option<NonNull<Chunk>> {
        let taken = {
            let mut kept = self.kept();
            kept.heaps += 1;
            kept.peak_heaps = kept.peak_heaps.max(kept.heaps);
            kept.free.pop()
        };
        taken.map_or_else(|| chunk_alloc(CHUNK_SIZE), |Free(chunk)| Some(chunk))
    }

    /// One more chunk for a heap that grows: the cache's newest, or a
    /// fresh mapping.
    pub(super) fn take(&self) -> Option<NonNull<Chunk>> {
        let taken = self.kept().free.pop();
        taken.map_or_else(|| chunk_alloc(CHUNK_SIZE), |Free(chunk)| Some(chunk))
    }

    /// A chunk a standing heap gave up, kept while the cache is under its
    /// bound and unmapped past it.
    pub(super) fn give(&self, chunk: NonNull<Chunk>) {
        let past = {
            let mut kept = self.kept();
            if kept.free.len() < kept.peak_heaps {
                kept.free.push(Free(chunk));
                None
            } else {
                Some(chunk)
            }
        };
        if let Some(chunk) = past {
            chunk_free(chunk, CHUNK_SIZE);
        }
    }

    /// A heap's end: every chunk it held, kept while the cache is under
    /// its bound and unmapped past it, and one heap fewer.
    pub(super) fn leave(&self, chunks: impl Iterator<Item = NonNull<Chunk>>) {
        let mut past = Vec::new();
        {
            let mut kept = self.kept();
            for chunk in chunks {
                if kept.free.len() < kept.peak_heaps {
                    kept.free.push(Free(chunk));
                } else {
                    past.push(chunk);
                }
            }
            kept.heaps = kept.heaps.saturating_sub(1);
        }
        for chunk in past {
            chunk_free(chunk, CHUNK_SIZE);
        }
    }
}

impl Drop for ChunkCache {
    fn drop(&mut self) {
        let kept = self
            .kept
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for Free(chunk) in kept.free.drain(..) {
            chunk_free(chunk, CHUNK_SIZE);
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
            assert_eq!(
                MAPPINGS.load(Relaxed),
                before + 1,
                "and leaves it to the cache"
            );
        }
        assert_eq!(read[0], read[1]);
        assert_eq!(read[0][1], CHUNK_SIZE);
        drop(cache);
        assert_eq!(MAPPINGS.load(Relaxed), before, "the cache's end unmaps it");
    }

    /// A request that grew past its main chunk gives every chunk back,
    /// and the next one grows into them without a mapping; what it reads
    /// at its first line is still the one chunk, since a chunk counts
    /// where a heap holds it.
    #[test]
    fn a_heap_grows_into_the_chunks_an_earlier_one_gave_back() {
        let _alone = alone();
        let before = MAPPINGS.load(Relaxed);
        let cache = Arc::new(ChunkCache::new());
        // Two heaps at once, so the cache keeps two chunks.
        let other = Heap::new_in(Arc::clone(&cache));
        let mut mapped = Vec::new();
        for _ in 0..2 {
            let mut heap = Heap::new_in(Arc::clone(&cache));
            assert_eq!(heap.memory_usage(true), CHUNK_SIZE);
            let mut held = Vec::new();
            // Runs of a whole page each, past what one chunk holds.
            for _ in 0..600 {
                let allocated = heap.string(&[b'x'; 4000]);
                assert!(allocated.is_ok(), "the heap would not allocate a run");
                if let Ok(s) = allocated {
                    held.push(s);
                }
            }
            assert_eq!(heap.memory_usage(true), 2 * CHUNK_SIZE);
            mapped.push(MAPPINGS.load(Relaxed) - before);
            for s in held {
                heap.release_string(s);
            }
        }
        assert_eq!(mapped, vec![3, 3], "the second request mapped nothing");
        drop(other);
        drop(cache);
        assert_eq!(MAPPINGS.load(Relaxed), before);
    }

    /// The cache keeps as many chunks as heaps have stood at once and
    /// unmaps what is given back past that.
    #[test]
    fn the_cache_keeps_a_chunk_for_each_heap_that_stood_at_once() {
        let _alone = alone();
        let before = MAPPINGS.load(Relaxed);
        let cache = Arc::new(ChunkCache::new());
        let three: Vec<Heap> = (0..3).map(|_| Heap::new_in(Arc::clone(&cache))).collect();
        let mut big = Heap::new_in(Arc::clone(&cache));
        let mut held = Vec::new();
        for _ in 0..1200 {
            if let Ok(s) = big.string(&[b'x'; 4000]) {
                held.push(s);
            }
        }
        assert_eq!(big.memory_usage(true), 3 * CHUNK_SIZE);
        for s in held {
            big.release_string(s);
        }
        drop(three);
        drop(big);
        assert_eq!(
            MAPPINGS.load(Relaxed) - before,
            4,
            "four heaps stood at once, so four chunks are kept"
        );
        drop(cache);
        assert_eq!(MAPPINGS.load(Relaxed), before);
    }
}
