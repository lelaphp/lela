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
//!
//! So a kept chunk carries what its last request wrote into the next
//! request's pages, as a php-fpm child's does, and under makala into a
//! request on another thread. That is the default, php's ([`Residue`],
//! ruled 2026-11-09). A process started with [`ZEROED`] set to a number
//! that is not zero zeroes the pages each request touched before its
//! chunks are kept, at a cost that follows the request's writes and not
//! the chunk's size; nothing a request reads moves.
//!
//! It covers the chunks a heap keeps, and nothing else. Not kept, and so
//! not zeroed: huge blocks and chunks given back to the system, which a
//! mapping gets back zeroed (and the system allocator, where there is no
//! `mmap`, is asked for zeroed). Outside it: a heap of the tracked
//! allocator (`USE_ZEND_ALLOC=0` with `USE_TRACKED_ALLOC=1`), whose
//! blocks are the system allocator's; an allocation ADR-0014's lend
//! serves from the system allocator past the limit, and `emalloc` outside
//! a lend; and every buffer that lives in the system allocator, the
//! host's own and the compiler's among them.

use std::ffi::OsStr;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};

use super::{Chunk, chunk_free};

/// The heaps the engine keeps between requests, which every heap made
/// with [`crate::Heap::new_in`] starts from and is kept in at its end,
/// as a php-fpm child keeps its heap (`zend_mm_shutdown(full=0)`).
/// Dropping it unmaps every chunk it keeps.
#[derive(Debug)]
pub struct ChunkCache {
    kept: Mutex<Vec<Kept>>,
    /// What a kept chunk's touched pages carry to the next heap, the
    /// process's choice read as the cache is made ([`Residue::chosen`]).
    residue: Residue,
}

impl Default for ChunkCache {
    fn default() -> ChunkCache {
        ChunkCache::with(Residue::chosen())
    }
}

/// The variable that turns [`Residue::Zeroed`] on for the process: set
/// to a number that is not zero, as `ZEND_ATOL` reads one, as php's
/// `USE_ZEND_ALLOC` is read. It is the engine's own and not a php.ini
/// directive, since php has none, and `ini_get_all()` names nothing of
/// it (ruled 2026-11-09), so it carries the product's prefix and none of
/// php's (ruled 2026-11-11).
///
/// The name promises that the pages a request touched are zeroed before
/// its chunks are kept, and no more: it is no security boundary,
/// it covers nothing the system allocator hands out, and it says nothing
/// of a request still running.
pub(super) const ZEROED: &str = "LELA_ZERO_TOUCHED_PAGES";

/// What the pages a request touched carry into the next request that is
/// handed its kept chunks: the bytes the request left there, as php's
/// `zend_mm_shutdown(full=0)` leaves them, or zeros, written before the
/// chunk is kept and so before another thread's heap can take it. Chosen
/// once for every heap of the process ([`Residue::chosen`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Residue {
    /// php's: nothing is zeroed.
    Left,
    /// Each kept chunk's pages from its first after the header up to the
    /// last its requests were handed since it was last zeroed, written
    /// to zero as the chunk is kept ([`super::Mm::keep`]).
    Zeroed,
}

impl Residue {
    /// The process's choice: [`Residue::Zeroed`] where it started with
    /// [`ZEROED`] set to a number that is not zero, read once.
    fn chosen() -> Residue {
        static CHOSEN: OnceLock<Residue> = OnceLock::new();
        *CHOSEN.get_or_init(|| Residue::from_env(std::env::var_os(ZEROED).as_deref()))
    }

    /// [`Residue::chosen`]'s reading of the variable.
    fn from_env(value: Option<&OsStr>) -> Residue {
        match value.map(|value| super::tracked::atol(value.as_encoded_bytes())) {
            Some(number) if number != 0 => Residue::Zeroed,
            _ => Residue::Left,
        }
    }
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

    /// An empty cache whose kept chunks carry `residue`, whatever the
    /// process chose.
    fn with(residue: Residue) -> ChunkCache {
        ChunkCache {
            kept: Mutex::default(),
            residue,
        }
    }

    /// What a chunk kept here carries to the next heap.
    pub(super) fn residue(&self) -> Residue {
        self.residue
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
            chunk_free(heap.main);
            for chunk in heap.cached {
                chunk_free(chunk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Relaxed;

    use std::ffi::OsStr;
    use std::ptr::NonNull;

    use super::{ChunkCache, Residue};
    use crate::Heap;
    use crate::mm::{CHUNK_SIZE, MAPPINGS, MAX_LARGE_SIZE, Mm, PAGE_SIZE, PAGES};
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
            held.push(heap.string(&[b'x'; 4000]).expect("a run"));
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

    /// The bytes a request wrote, a pattern no allocation lays down.
    const WRITTEN: u8 = 0x5A;

    /// The first `bytes` of the block at `at` laid with [`WRITTEN`].
    fn write(at: NonNull<u8>, bytes: usize) {
        // SAFETY: `at` is a block of at least `bytes` bytes its heap
        // handed out and nothing else names.
        unsafe { at.write_bytes(WRITTEN, bytes) };
    }

    /// What the page at `at`, a block of at least a page its heap has just
    /// handed out, holds past the debug build's freed-block mark, which
    /// a hand-out clears: `Some(byte)` where every byte is that one, and
    /// `None` where they differ.
    fn read(at: NonNull<u8>) -> Option<u8> {
        // SAFETY: the block is a page of its heap's, read through the
        // pointer this heap's allocation gave, and every byte of it was
        // written by an earlier request or zeroed as its chunk was kept.
        let page = unsafe { std::slice::from_raw_parts(at.as_ptr(), PAGE_SIZE) };
        let past = &page[64..];
        past.iter().all(|b| *b == past[0]).then_some(past[0])
    }

    /// What a second request reads in the pages the first touched, kept
    /// in a cache of `residue`: the first takes two `runs` of two pages from
    /// the main chunk and writes both, gives the first back and ends with
    /// the second held, and the second takes one `run` of four pages, which
    /// lands where both stood. Each of the four pages is read.
    fn read_after_one_request(residue: Residue) -> Vec<Option<u8>> {
        let cache = Arc::new(ChunkCache::with(residue));
        let mut first = Mm::new_in(Arc::clone(&cache));
        let freed = first.alloc(2 * PAGE_SIZE).expect("a run");
        let held = first.alloc(2 * PAGE_SIZE).expect("a run");
        write(freed, 2 * PAGE_SIZE);
        write(held, 2 * PAGE_SIZE);
        // SAFETY: `freed` is a block of `first`'s that nothing names.
        unsafe { first.free(freed) };
        drop(first);
        let mut second = Mm::new_in(Arc::clone(&cache));
        let run = second.alloc(4 * PAGE_SIZE).expect("a run");
        assert_eq!(run, freed, "the run lands where the first request's stood");
        let pages = (0..4).map(|page| {
            // SAFETY: the page is inside the run of four just handed out.
            read(unsafe { run.add(page * PAGE_SIZE) })
        });
        let read = pages.collect();
        // SAFETY: `run` is a block of `second`'s that nothing names.
        unsafe { second.free(run) };
        read
    }

    /// php's `zend_mm_shutdown(full=0)` zeroes nothing, so by default the
    /// next request's pages hold what the last one wrote, the pages it
    /// gave back before its end among them (ruled 2026-11-09); with
    /// [`Residue::Zeroed`] every page it touched reads zero.
    #[test]
    fn the_pages_a_request_touched_read_zero_in_the_next_only_where_the_cache_zeroes() {
        let _alone = alone();
        assert_eq!(
            read_after_one_request(Residue::Left),
            vec![Some(WRITTEN); 4],
            "php's: what the first request wrote, freed and held alike"
        );
        assert_eq!(read_after_one_request(Residue::Zeroed), vec![Some(0); 4]);
    }

    /// A cached chunk is zeroed as the main one is, and the numbers a
    /// request reads at its first line are the same either way: a request
    /// that peaked at three chunks leaves one cached
    /// (`zend_mm_shutdown(full=0)`'s average), and the next one, past its
    /// main chunk, is handed a `run` in it.
    #[test]
    fn a_kept_cached_chunk_is_zeroed_too_and_no_number_php_reads_moves() {
        let _alone = alone();
        let mut seen = Vec::new();
        for residue in [Residue::Left, Residue::Zeroed] {
            let cache = Arc::new(ChunkCache::with(residue));
            let mut first = Mm::new_in(Arc::clone(&cache));
            for _ in 0..3 {
                let whole = first.alloc(MAX_LARGE_SIZE).expect("a chunk's run");
                write(whole, PAGE_SIZE);
            }
            drop(first);
            let mut second = Mm::new_in(Arc::clone(&cache));
            let numbers = [second.size(), second.real_size(), second.real_peak()];
            let main = second.alloc(MAX_LARGE_SIZE).expect("the main chunk's run");
            let cached = second
                .alloc(MAX_LARGE_SIZE)
                .expect("the cached chunk's run");
            assert_eq!(second.real_size(), 2 * CHUNK_SIZE, "nothing new mapped");
            seen.push((numbers, read(cached)));
            // SAFETY: both are blocks of `second`'s that nothing names.
            unsafe { second.free(cached) };
            // SAFETY: as above.
            unsafe { second.free(main) };
        }
        let numbers = [0, 2 * CHUNK_SIZE, 2 * CHUNK_SIZE];
        assert_eq!(seen, vec![(numbers, Some(WRITTEN)), (numbers, Some(0))]);
    }

    /// With the cache zeroing, the first page of each block a request is
    /// handed reads zero: in the main chunk and the cached one, zeroed as
    /// they were kept, and in a chunk mapped fresh, which stands where the
    /// one the first request's end gave back to the system stood. A mapping comes zeroed; where a chunk is the system
    /// allocator's, under Miri and on a platform with no `mmap`, it is
    /// asked for zeroed, and Miri reads the page as uninitialised where it
    /// is not.
    #[test]
    fn a_chunk_mapped_fresh_reads_zero_as_a_kept_one_does() {
        let _alone = alone();
        let cache = Arc::new(ChunkCache::with(Residue::Zeroed));
        let mut first = Mm::new_in(Arc::clone(&cache));
        for _ in 0..3 {
            let whole = first.alloc(MAX_LARGE_SIZE).expect("a chunk's run");
            write(whole, PAGE_SIZE);
        }
        // Peaking at three chunks keeps one cached and gives the third back.
        drop(first);
        let mut second = Mm::new_in(Arc::clone(&cache));
        let runs: Vec<NonNull<u8>> = (0..3)
            .map(|_| second.alloc(MAX_LARGE_SIZE).expect("a chunk's run"))
            .collect();
        let read: Vec<Option<u8>> = runs.iter().map(|run| read(*run)).collect();
        for run in runs {
            // SAFETY: `run` is a block of `second`'s that nothing names.
            unsafe { second.free(run) };
        }
        assert_eq!(read, vec![Some(0); 3]);
    }

    /// A chunk a request empties and is handed again keeps the mark of
    /// the pages it touched before, so the pages the request wrote there
    /// first are zeroed with it however few it takes the second time.
    #[test]
    fn a_chunk_emptied_and_laid_down_again_in_a_request_keeps_its_mark() {
        let _alone = alone();
        let cache = Arc::new(ChunkCache::with(Residue::Zeroed));
        let mut heap = Mm::new_in(Arc::clone(&cache));
        // Past the fast shutdown's start an emptied chunk is cached.
        heap.keep_emptied();
        let main = heap.alloc(MAX_LARGE_SIZE).expect("the main chunk's run");
        let whole = heap.alloc(MAX_LARGE_SIZE).expect("a second chunk's run");
        let chunk = heap.chunks[1];
        // SAFETY: `whole` is a block of `heap`'s that nothing names.
        unsafe { heap.free(whole) };
        assert_eq!(heap.chunks.len(), 1, "the emptied chunk left the list");
        let again = heap.alloc(PAGE_SIZE).expect("a page");
        assert_eq!(heap.chunks[1], chunk, "and was laid down again");
        // SAFETY: the chunk is on the heap's list, its header the one
        // `chunk_init` wrote, and nothing writes it during the read.
        let touched = unsafe { (*chunk.as_ptr()).touched };
        assert_eq!(touched, PAGES, "every page its first run took");
        // SAFETY: both are blocks of `heap`'s that nothing names.
        unsafe { heap.free(again) };
        // SAFETY: as above.
        unsafe { heap.free(main) };
    }

    /// The process's choice is php's unless the variable is a number that
    /// is not zero, read as php reads `USE_ZEND_ALLOC`.
    #[test]
    fn the_process_zeroes_only_where_the_variable_is_a_number_not_zero() {
        let of = |value: Option<&str>| Residue::from_env(value.map(OsStr::new));
        assert_eq!(of(None), Residue::Left);
        assert_eq!(of(Some("0")), Residue::Left);
        assert_eq!(of(Some("")), Residue::Left);
        assert_eq!(of(Some("yes")), Residue::Left);
        assert_eq!(of(Some("1")), Residue::Zeroed);
        assert_eq!(of(Some(" 2x")), Residue::Zeroed);
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
