//! The allocator (design §5.13): a copy of `zend_alloc`, the one
//! accountant of PHP-visible memory. 2 MiB chunks carved into 4 KiB
//! pages, the thirty small-size bins of `Zend/zend_alloc_sizes.h`, large
//! `runs` of whole pages, huge allocations of their own, per-request
//! ownership and mass release when the request's heap drops. A huge
//! allocation is rounded to the page the machine reports, which is php's
//! `REAL_PAGE_SIZE` and on this machine is not the 4 KiB page a chunk is
//! carved into.
//!
//! The numbers php reports come from here and from nowhere else:
//! `memory_get_usage(false)` is [`Mm::size`], the bin accounting, and
//! `memory_get_usage(true)` is [`Mm::real_size`], the bytes the chunks
//! take. Both follow php's own arithmetic, so an array or a string of a
//! given length moves them by the same bytes php moves them by.
//!
//! Two departures, neither observable from PHP. php threads its chunks
//! on a circular list inside the chunks themselves, because its heap
//! struct lives in the first one; here the list is a `Vec` beside them,
//! walked in the same order, so the chunk a `run` lands in is the chunk
//! php's walk would have landed in. And php maps a chunk with `mmap`,
//! which hands back zeroed pages; here a chunk comes from the system
//! allocator aligned to its own size, which is what makes the chunk of
//! an address a mask away, and the header is zeroed on the way in. A
//! huge block is php's own mapping on a unix, aligned as php aligns it,
//! because its tail is grown and cut where it stands.
//!
//! Miri cannot model that mapping. Its `munmap` is a deallocation of the
//! whole allocation an `mmap` made, so php's unmapping of the slack
//! around an aligned block, and of a cut tail, both of which the machine
//! defines, is to Miri a deallocation of the wrong size, reported as
//! undefined behaviour at the call. Under Miri a huge block therefore
//! takes the arm of a platform with no `mmap`: the system allocator,
//! aligned to a chunk, never grown or cut in place. Miri then sees each
//! huge block's whole life, a use of one after its free included, and
//! what it never checks is the mapping itself: the slack trim and the
//! in-place grow and cut, with the accounting of the in-place arms of
//! `realloc_huge`, which the native test lane covers.
//!
//! Before it declares the limit reached or the system out of memory, php
//! sweeps: `zend_mm_gc` (`Z/Zend/zend_alloc.c:2101`) counts the free slots
//! of every bin `run` into the page map word that holds that count,
//! gives back the `runs` whose slots are all free, and the caller asks
//! again. [`Mm::gc_heap`] is that function, called where php calls it, so a
//! request that filled a bin and gave it back goes on exactly as far as
//! php's does.

// A page number, a page count and a bin number are all under 512 here and
// a chunk count is a handful: the narrowing casts below hold what php
// holds in a `uint32_t` for the same reason it does.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::alloc::{Layout, alloc, dealloc};
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

use crate::hooks::Fatal;
use crate::observe::{Allocation, AllocationObserverId, ObserveAllocations, Observers};

mod cache;
#[cfg(debug_assertions)]
mod leaks;
mod oom;
#[cfg(debug_assertions)]
mod origins;
pub(crate) mod tracked;

pub use cache::ChunkCache;
pub use oom::{DelayedOom, PendingOom};

use oom::{HardLimitPercent, Wake};
use tracked::Tracked;

/// Which allocator a heap serves its blocks from, php's
/// `use_custom_heap` and the handlers it names: Zend's own chunks, bins
/// and runs, or the system's allocator with each block's size kept
/// ([`tracked`]), chosen once for every heap of the process
/// ([`tracked::chosen`]).
#[derive(Debug)]
enum Allocator {
    Zend,
    Tracked(Box<Tracked>),
}

/// The arm `alloc`, `realloc` and `free` take, php's `use_custom_heap`:
/// Zend's own, or the custom one that serves the tracked allocator and
/// tells the observers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entries {
    Zend,
    Custom,
}

/// `ZEND_MM_CHUNK_SIZE`: 2 MiB, and the alignment of every chunk, so the
/// chunk an allocation belongs to is its address with the low bits cut.
pub(crate) const CHUNK_SIZE: usize = 2 * 1024 * 1024;
/// `ZEND_MM_PAGE_SIZE`.
const PAGE_SIZE: usize = 4096;
/// `ZEND_MM_PAGES`: the pages of one chunk.
const PAGES: u32 = (CHUNK_SIZE / PAGE_SIZE) as u32;
/// `ZEND_MM_FIRST_PAGE`: page zero holds the chunk's own header.
const FIRST_PAGE: u32 = 1;
/// `ZEND_MM_PAGE_MAP_LEN`: the words of the free-page bitset.
const PAGE_MAP_LEN: usize = PAGES as usize / 64;
/// `ZEND_MM_BINS`.
const BINS: usize = 30;
/// `ZEND_MM_MAX_SMALL_SIZE`: above this an allocation is a `run` of pages.
const MAX_SMALL_SIZE: usize = 3072;
/// `ZEND_MM_MIN_USEABLE_BIN_SIZE` under `ZEND_MM_HEAP_PROTECTION`, which
/// the oracle's build has: two pointers, the free slot's next pointer
/// and its shadow, so an allocation of fewer bytes is sixteen and bin 0
/// is never handed out (`Z/Zend/zend_alloc.c:153`). A crc32 state is
/// four bytes php `ecalloc`s, and counts sixteen:
///
/// ```php
/// $a = hash_init('crc32b');
/// $m = memory_get_usage();
/// $b = hash_init('crc32b');
/// var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// int(112)
/// ```
const MIN_USEABLE_BIN_SIZE: usize = 16;
/// `ZEND_MM_MAX_LARGE_SIZE`: above this an allocation is huge.
const MAX_LARGE_SIZE: usize = CHUNK_SIZE - PAGE_SIZE * FIRST_PAGE as usize;
/// php's default `memory_limit` before any ini is read
/// (`Z/Zend/zend_alloc.c:2085`).
pub(crate) const NO_LIMIT: usize = usize::MAX >> 1;

/// `REAL_PAGE_SIZE` (`Z/Zend/zend_alloc.c:104`): the page the machine
/// reports, which is what php rounds a huge allocation to
/// (`Z/Zend/zend_alloc.c:1925`) and is not [`PAGE_SIZE`], its own chunk
/// page. php reads it once, at startup
/// (`Z/Zend/zend_alloc.c:3334`); this reads it once too, on the first
/// huge allocation of the process, there being no startup here and the
/// number not moving under a running machine. On macOS on Apple Silicon
/// it is 16 KiB, so a 3 MB string moves `memory_get_usage(true)` by
/// 3,014,656 bytes where a 4 KiB rounding would move it by 3,002,368.
pub(crate) fn real_page_size() -> usize {
    static ASKED: OnceLock<usize> = OnceLock::new();
    *ASKED.get_or_init(machine_page_size)
}

/// What the machine answers: `sysconf(_SC_PAGESIZE)`, as php asks it
/// (`Z/Zend/zend_alloc.c:3334`). A machine that answers nothing usable
/// leaves php's own default standing, the `#ifndef REAL_PAGE_SIZE` arm of
/// `Z/Zend/zend_alloc.c:112`.
#[cfg(unix)]
fn machine_page_size() -> usize {
    // SAFETY: `sysconf` takes one name, reads and writes nothing of ours
    // and answers a plain number.
    let answered = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    match usize::try_from(answered) {
        Ok(size) if size.is_power_of_two() => size,
        _ => PAGE_SIZE,
    }
}

/// A platform without `_SC_PAGESIZE` keeps php's default, as php keeps it
/// (`Z/Zend/zend_alloc.c:112`).
#[cfg(not(unix))]
fn machine_page_size() -> usize {
    PAGE_SIZE
}

/// `bin_data_size`, the usable bytes of each bin
/// (`Z/Zend/zend_alloc_sizes.h:32-62`).
const BIN_DATA_SIZE: [usize; BINS] = [
    8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640,
    768, 896, 1024, 1280, 1536, 1792, 2048, 2560, 3072,
];
/// `bin_elements`, how many of each bin fit in its `run`.
const BIN_ELEMENTS: [usize; BINS] = [
    512, 256, 170, 128, 102, 85, 73, 64, 51, 42, 36, 32, 25, 21, 18, 16, 64, 32, 9, 8, 32, 16, 9,
    8, 16, 8, 16, 8, 8, 4,
];
/// `bin_pages`, the pages one `run` of each bin takes.
const BIN_PAGES: [u32; BINS] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 5, 3, 1, 1, 5, 3, 2, 2, 5, 3, 7, 4, 5, 3,
];

/// `ZEND_MM_IS_LRUN`, `ZEND_MM_IS_SRUN` and the fields packed beside them
/// in a page's map word (`Z/Zend/zend_alloc.c:191-215`).
const IS_LRUN: u32 = 0x4000_0000;
const IS_SRUN: u32 = 0x8000_0000;
const LRUN_PAGES_MASK: u32 = 0x0000_03ff;
const SRUN_BIN_NUM_MASK: u32 = 0x0000_001f;
const SRUN_FREE_COUNTER_MASK: u32 = 0x01ff_0000;
const SRUN_FREE_COUNTER_SHIFT: u32 = 16;
const NRUN_OFFSET_MASK: u32 = 0x01ff_0000;
const NRUN_OFFSET_SHIFT: u32 = 16;

/// One chunk's header, in its page zero: which pages are taken, how far
/// the taken ones reach, and what each `run` at a page is.
#[repr(C)]
struct Chunk {
    /// `free_pages`: pages of this chunk nothing holds.
    free_pages: u32,
    /// `free_tail`: the first page of the free tail, kept as php keeps
    /// it, which is to say not always accurate and never over-stated.
    free_tail: u32,
    /// `num`: the order the chunk was mapped in, which decides which of
    /// two chunks the cache keeps.
    num: u32,
    /// `free_map`: one bit per page, set while the page is taken.
    free_map: [u64; PAGE_MAP_LEN],
    /// `map`: what starts at each page, `ZEND_MM_FRUN`, `ZEND_MM_LRUN`,
    /// `ZEND_MM_SRUN` or `ZEND_MM_NRUN`.
    map: [u32; PAGES as usize],
    /// A debug build's way from a block to its heap's sensed record,
    /// what php's `heap` field is to its checks: the [`Mm::sensed`] of
    /// the heap that laid the chunk down.
    #[cfg(debug_assertions)]
    sensed: *const crate::sensed::Record,
    /// A debug build's table of the origins of the chunk's blocks
    /// ([`origins`]), null where the system could not give it.
    #[cfg(debug_assertions)]
    origins: Option<NonNull<crate::sensed::Origin>>,
}

/// The header takes page zero and nothing more, which is what
/// `ZEND_MM_FIRST_PAGE` says.
const _: () = assert!(size_of::<Chunk>() <= PAGE_SIZE * FIRST_PAGE as usize);

/// A free slot of a bin `run`: while it is free its first word is the next
/// free slot of the same bin, which is php's free list exactly.
#[repr(C)]
struct FreeSlot {
    next: *mut FreeSlot,
}

/// `sizeof(zend_mm_huge_list)` (`Z/Zend/zend_alloc.c:355`): `ptr`, `size`
/// and `next`, three words, so the node falls in the 24-byte bin.
const HUGE_LIST_SIZE: usize = 3 * size_of::<usize>();

/// One huge allocation, the bytes it was rounded to, and the slot of the
/// 24-byte bin that is its `zend_mm_huge_list` node. php threads the
/// blocks through the nodes; here the `Vec` holds the blocks and the
/// node is taken and given back for the bytes it counts, since a bin
/// allocation of this heap is what `memory_get_usage()` reads.
struct Huge {
    at: NonNull<u8>,
    size: usize,
    node: NonNull<u8>,
    /// A debug build's origin of the block ([`origins`]).
    #[cfg(debug_assertions)]
    origin: crate::sensed::Origin,
}

/// The per-request heap: its chunks, its thirty free lists, its huge
/// blocks and the four numbers php answers `memory_get_usage()` and
/// `memory_get_peak_usage()` with.
pub(crate) struct Mm {
    /// The allocator its blocks come from.
    allocator: Allocator,
    /// php's `heap->use_custom_heap`: whether the entries take their cold
    /// arm, for the tracked allocator or for the observers, the one test
    /// an allocation pays for either ([`Mm::reckon_custom`]).
    entries: Entries,
    /// The chunks in php's walk order; the first is php's `main_chunk`
    /// and is never moved or deleted.
    chunks: Vec<NonNull<Chunk>>,
    /// `cached_chunks`, kept mapped against the next growth, newest
    /// first.
    cached: Vec<NonNull<Chunk>>,
    /// `free_slot`: the head of each bin's free list.
    free_slot: [*mut FreeSlot; BINS],
    /// `huge_list`, as a `Vec` beside the nodes rather than a list
    /// through them.
    huge: Vec<Huge>,
    /// `peak_chunks_count`, `avg_chunks_count`,
    /// `last_chunks_delete_boundary` and `last_chunks_delete_count`: what
    /// decides whether a chunk is cached or given back.
    peak_chunks_count: usize,
    avg_chunks_count: f64,
    last_delete_boundary: usize,
    last_delete_count: usize,
    /// `size`, `peak`, `real_size`, `real_peak`, `limit`.
    size: usize,
    peak: usize,
    real_size: usize,
    real_peak: usize,
    limit: usize,
    /// `hard_limit` and `hard_limit_percent`: past the hard limit an
    /// allocation ends the process ([`Mm::check_limit`]).
    hard_limit: usize,
    hard_limit_percent: HardLimitPercent,
    /// `fallible`: whether an allocation past the soft limit is refused,
    /// php's `NULL`, rather than served ([`Mm::set_fallible`]).
    fallible: Asks,
    /// Whether a fallible ask was refused since [`Mm::take_refused`]
    /// last read it.
    refused: bool,
    /// `overflow`: set while the limit's fatal is raised
    /// ([`Mm::take_pending_oom`]), the window php discards the buffers
    /// `ob_start()` laid down in (`zend_alloc_in_memory_limit_error_reporting`).
    overflow: bool,
    /// php's `EG(delayed_oom)`, which the limit records and the engine
    /// raises ([`DelayedOom`]).
    delayed: DelayedOom,
    /// The request's interrupt, raised as a delayed OOM is recorded.
    wake: Option<Wake>,
    /// The profilers told of what the entries serve, `alloc`, `realloc`,
    /// `free` and `gc`, and of nothing the heap does on its own behalf.
    observers: Observers,
    /// The blocks whose free php's accounting has seen while a pin keeps
    /// their bytes ([`Mm::retire`]): few, since each is a block destroyed
    /// under a pin that still stands.
    retired: Vec<NonNull<u8>>,
    /// A debug build's record of what its sensors found
    /// ([`crate::sensed`]), held apart so the address every chunk's
    /// header holds stands while the heap moves, and dropped after the
    /// chunks are given back.
    #[cfg(debug_assertions)]
    sensed: crate::sensed::Sensor,
    /// A debug build's quarantine of each bin's last freed slots
    /// ([`Quarantine`]), held back from the free list.
    #[cfg(debug_assertions)]
    quarantine: [Quarantine; BINS],
    /// A debug build's origin of what it allocates from here on
    /// ([`Mm::set_origin`]).
    #[cfg(debug_assertions)]
    origin: crate::sensed::Origin,
    /// The engine's cache this heap started from and is kept in at its
    /// end ([`ChunkCache`]); none for a heap that unmaps everything at its
    /// end.
    cache: Option<Arc<ChunkCache>>,
    /// A debug build's backtrace of every block it has out, where
    /// allocations are traced ([`Mm::trace_allocations`]).
    #[cfg(debug_assertions)]
    traces: Option<std::collections::HashMap<usize, std::backtrace::Backtrace>>,
    /// php's fast shutdown has begun ([`Mm::keep_emptied`]).
    keep_emptied: bool,
    /// A test's arming of the limit ([`Mm::refuse_after`]), off unless a
    /// test arms it.
    #[cfg(test)]
    armed: Armed,
}

/// Where a test's arming of the limit stands ([`Mm::refuse_after`]).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Armed {
    Off,
    /// This many more asks served before the one refused.
    After(usize),
    /// The ask it was armed for refused.
    Fired,
}

impl Mm {
    /// A heap with php's main chunk mapped, as `zend_mm_init` leaves one
    /// (`Z/Zend/zend_alloc.c:2047`): one chunk of real size, nothing
    /// allocated in it, no limit until the ini sets one. A machine that
    /// cannot give the chunk leaves the heap empty and the next
    /// allocation asks again.
    pub(crate) fn new() -> Mm {
        Mm::over(None)
    }

    /// A heap that starts from what `cache` kept of the last heap to end
    /// in it, as `zend_mm_shutdown(full=0)` leaves php's heap for the next
    /// request: its main chunk laid down again, its cached chunks counted
    /// in `memory_get_usage(true)`, and its average; [`Mm::new`]'s where
    /// the cache keeps none.
    pub(crate) fn new_in(cache: Arc<ChunkCache>) -> Mm {
        Mm::over(Some(cache))
    }

    /// A heap of the allocator the process chose: Zend's, or, where the
    /// environment asked for it, the tracked one, which maps no chunk and
    /// takes nothing from the cache.
    fn over(cache: Option<Arc<ChunkCache>>) -> Mm {
        let allocator = if tracked::chosen() {
            Allocator::Tracked(Box::default())
        } else {
            Allocator::Zend
        };
        Mm::of(allocator, cache)
    }

    /// A tracked heap, whatever the process chose, for the tests. Its
    /// blocks are asked for raw: the debug build's freed-block mark and
    /// owner table read the process's choice ([`tracked::chosen`]), not
    /// the heap's, since they reach a block with no heap in hand, so a
    /// counted value laid on this heap in a process that chose Zend's
    /// would be read as a chunk's.
    #[cfg(test)]
    pub(crate) fn tracked() -> Mm {
        Mm::of(Allocator::Tracked(Box::default()), None)
    }

    fn of(allocator: Allocator, cache: Option<Arc<ChunkCache>>) -> Mm {
        let zend = matches!(allocator, Allocator::Zend);
        let kept = cache
            .as_ref()
            .filter(|_| zend)
            .and_then(|cache| cache.take());
        let (main, cached, avg_chunks_count) = match kept {
            Some(kept) => (Some(kept.main), kept.cached, kept.avg_chunks_count),
            None if zend => (chunk_alloc(CHUNK_SIZE), Vec::new(), 1.0),
            None => (None, Vec::new(), 1.0),
        };
        let mut mm = Mm {
            allocator,
            entries: if zend { Entries::Zend } else { Entries::Custom },
            chunks: Vec::new(),
            cached,
            free_slot: [std::ptr::null_mut(); BINS],
            huge: Vec::new(),
            peak_chunks_count: 0,
            avg_chunks_count,
            last_delete_boundary: 0,
            last_delete_count: 0,
            size: 0,
            peak: 0,
            real_size: 0,
            real_peak: 0,
            limit: NO_LIMIT,
            hard_limit: NO_LIMIT,
            hard_limit_percent: HardLimitPercent::Default,
            fallible: Asks::Infallibly,
            refused: false,
            overflow: false,
            delayed: DelayedOom::None,
            wake: None,
            observers: Observers::default(),
            retired: Vec::new(),
            #[cfg(debug_assertions)]
            sensed: crate::sensed::Sensor::new(),
            #[cfg(debug_assertions)]
            quarantine: [Quarantine::EMPTY; BINS],
            #[cfg(debug_assertions)]
            origin: crate::sensed::Origin::default(),
            cache,
            #[cfg(debug_assertions)]
            traces: None,
            keep_emptied: false,
            #[cfg(test)]
            armed: Armed::Off,
        };
        // A debug build's sensor of the lend's slot (ADR-0014): a heap is
        // made where its request starts, and no lend stands there.
        #[cfg(debug_assertions)]
        if crate::lend::standing() {
            mm.sensed.record(crate::sensed::Sensed::MadeUnderLend);
        }
        if let Some(chunk) = main {
            mm.chunk_init(chunk, 0);
            mm.chunks.push(chunk);
            mm.peak_chunks_count = 1;
            mm.real_size = (mm.cached.len() + 1) * CHUNK_SIZE;
            mm.real_peak = mm.real_size;
        }
        mm
    }

    /// `memory_get_usage(false)`: the bin, `run` and huge accounting.
    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// `memory_get_usage(true)`: the bytes the chunks and the huge blocks
    /// hold, which for the chunks is their count times 2 MiB.
    pub(crate) fn real_size(&self) -> usize {
        self.real_size
    }

    /// `memory_get_peak_usage(false)`.
    pub(crate) fn peak(&self) -> usize {
        self.peak
    }

    /// `memory_get_peak_usage(true)`.
    pub(crate) fn real_peak(&self) -> usize {
        self.real_peak
    }

    /// `memory_reset_peak_usage()` (`Z/Zend/zend_alloc.c:2957`).
    pub(crate) fn reset_peak(&mut self) {
        self.peak = self.size;
        self.real_peak = self.real_size;
    }

    /// [`crate::Heap::uncount`]: the blocks of `blocks` this heap holds
    /// left out of the bin accounting, and their bytes.
    pub(crate) fn uncount(&mut self, blocks: impl Iterator<Item = NonNull<u8>>) -> usize {
        let bytes = blocks
            .filter(|at| self.holds(at.as_ptr().addr()))
            .map(|at| self.accounted(at))
            .sum();
        self.size = self.size.saturating_sub(bytes);
        if matches!(self.allocator, Allocator::Tracked(_)) {
            self.real_size = self.size;
        }
        bytes
    }

    /// [`crate::Heap::recount`]: `bytes` counted again, the peak raised
    /// to them as an allocation raises it.
    pub(crate) fn recount(&mut self, bytes: usize) {
        self.size += bytes;
        if matches!(self.allocator, Allocator::Tracked(_)) {
            self.real_size = self.size;
        } else {
            self.peak = self.peak.max(self.size);
        }
    }

    /// The `memory_limit` this heap is held to.
    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    /// `zend_set_memory_limit` (`Z/Zend/zend_alloc.c:3201` at 8.5.12): a
    /// limit past php's `ZEND_MM_LIMIT_MAX` is that, and one under what
    /// the chunks hold is met by giving cached chunks back where that is
    /// enough and refused where it is not; the hard limit follows the
    /// soft one it is held to.
    pub(crate) fn set_limit(&mut self, limit: usize) -> bool {
        let limit = limit.min(NO_LIMIT);
        if limit < self.real_size {
            if limit < self.real_size - self.cached.len() * CHUNK_SIZE {
                return false;
            }
            while limit < self.real_size {
                let Some(chunk) = self.cached.pop() else {
                    break;
                };
                chunk_free(chunk, CHUNK_SIZE);
                self.real_size -= CHUNK_SIZE;
            }
        }
        self.limit = limit;
        self.hard_limit = self.hard_limit_percent.hard_limit(limit);
        true
    }

    /// `zend_set_memory_hard_limit_percent` (`Z/Zend/zend_alloc.c:3215`):
    /// php's `-1` or a percent of 100 and above, refused while the soft
    /// limit is exceeded, since the hard limit must stay above what the
    /// chunks hold.
    pub(crate) fn set_hard_limit_percent(&mut self, percent: i64) -> bool {
        let Some(percent) = HardLimitPercent::from_php(percent) else {
            return false;
        };
        if self.real_size > self.limit {
            return false;
        }
        self.hard_limit_percent = percent;
        self.hard_limit = percent.hard_limit(self.limit);
        true
    }

    /// `heap->overflow`, `zend_alloc_in_memory_limit_error_reporting`:
    /// whether the limit's fatal is being raised.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflow
    }

    /// php's `EG(delayed_oom)`.
    #[inline]
    pub(crate) fn delayed(&self) -> &DelayedOom {
        &self.delayed
    }

    /// The request's interrupt, raised from here on as a delayed OOM is
    /// recorded.
    pub(crate) fn wake_on_oom(&mut self, wake: Wake) {
        self.wake = Some(wake);
    }

    /// The site the engine read for a delayed OOM recorded and not yet
    /// named, as php reads it in `zend_mm_delayed_verror`; one named
    /// already keeps its own.
    pub(crate) fn locate_oom(&mut self, file: &[u8], line: u32) {
        if let DelayedOom::Pending(pending) = &mut self.delayed
            && pending.site.is_none()
        {
            pending.site = Some((file.into(), line));
        }
    }

    /// The opening of `zend_handle_delayed_oom` (`Z/Zend/zend_alloc.c:3891`):
    /// a pending OOM taken out to be raised, the state `HANDLING` and the
    /// limit's fatal being reported, `heap->overflow = 1`; `None` where
    /// none is pending.
    pub(crate) fn take_pending_oom(&mut self) -> Option<PendingOom> {
        if !matches!(self.delayed, DelayedOom::Pending(_)) {
            return None;
        }
        let DelayedOom::Pending(pending) =
            std::mem::replace(&mut self.delayed, DelayedOom::Handling)
        else {
            return None;
        };
        self.overflow = true;
        Some(pending)
    }

    /// The close of `zend_handle_delayed_oom`: the fatal raised,
    /// `heap->overflow = 0` and the state `HANDLED`.
    pub(crate) fn oom_handled(&mut self) {
        self.overflow = false;
        self.delayed = DelayedOom::Handled;
    }

    /// `EG(delayed_oom).state = ZEND_OOM_STATE_NONE`, which
    /// `php_request_shutdown` writes as it begins and again once the heap
    /// is shut down (`Z/main/main.c:2000`, `:2095`).
    pub(crate) fn reset_oom(&mut self) {
        self.delayed = DelayedOom::None;
    }

    /// `zend_mm_check_limit` (`Z/Zend/zend_alloc.c:206` at 8.5.12), at the
    /// three places a heap grows: a chunk, a huge block, a huge block's
    /// tail. Past the soft limit the heap sweeps where `gc` says so;
    /// past the hard limit an allocation that is not fallible ends the
    /// process; still past the soft limit, the delayed OOM is recorded,
    /// once, and the allocation is served, or refused where it is
    /// fallible. Whether the sweep ran and freed anything is the answer,
    /// for a chunk's growth to look in the cache again.
    fn check_limit(&mut self, add: usize, gc: bool) -> Result<bool, Refused> {
        if !self.past_limit(add) {
            return Ok(false);
        }
        let did_gc = gc && self.gc_heap() != 0;
        if self.fallible == Asks::Infallibly && add > self.hard_limit.saturating_sub(self.real_size)
        {
            return Err(Refused::Fatal(Fatal::hard_limit(self.hard_limit, add)));
        }
        if !gc || self.past_limit(add) {
            self.delay(add);
            if self.fallible == Asks::Fallibly {
                return Err(Refused::Fallible);
            }
        }
        Ok(did_gc)
    }

    /// Whether `add` more bytes would take the chunks past the soft
    /// limit, or they are past it already.
    fn past_limit(&self, add: usize) -> bool {
        self.limit < self.real_size || add > self.limit - self.real_size
    }

    /// `zend_mm_delayed_error` over the limit's text, the limit and the
    /// bytes asked for, where no delayed OOM stands.
    #[cold]
    fn delay(&mut self, add: usize) {
        if self.delayed == DelayedOom::None {
            let limit = self.limit;
            self.delay_message(format!(
                "Allowed memory size of {limit} bytes exhausted (tried to allocate {add} bytes)"
            ));
        }
    }

    /// `zend_mm_delayed_verror` (`Z/Zend/zend_alloc.c:409`): the delayed
    /// OOM recorded with `message`, where none stands, and the request's
    /// interrupt raised.
    #[cold]
    fn delay_message(&mut self, message: String) {
        if self.delayed != DelayedOom::None {
            return;
        }
        self.delayed = DelayedOom::Pending(PendingOom {
            message,
            site: None,
        });
        if let Some(wake) = &self.wake {
            (**wake)
                .as_ref()
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// A test's arming of the limit (`crate::refusals`): `after` more
    /// allocations and reallocations served, and the one after them
    /// refused as the hard limit refuses it. The heap's own arms of the
    /// limit are reached only at chunk growth and a huge block, so a test
    /// cannot fill a heap until exactly the `n`th ask of an operation is
    /// refused; this refuses it where it is asked.
    #[cfg(test)]
    pub(crate) fn refuse_after(&mut self, after: usize) {
        self.armed = Armed::After(after);
    }

    /// The arming taken off, and whether it refused the ask it was armed
    /// for.
    #[cfg(test)]
    pub(crate) fn disarm(&mut self) -> bool {
        std::mem::replace(&mut self.armed, Armed::Off) == Armed::Fired
    }

    /// One ask counted against the arming: whether it is the one refused.
    #[cfg(test)]
    fn counted_out(&mut self) -> bool {
        match self.armed {
            Armed::After(0) => {
                self.armed = Armed::Fired;
                true
            }
            Armed::After(n) => {
                self.armed = Armed::After(n - 1);
                false
            }
            Armed::Off | Armed::Fired => false,
        }
    }

    /// `zend_mm_set_fallible` (`Z/Zend/zend_alloc.c:3696`): whether an
    /// ask past the soft limit is refused from here on, its refusal the
    /// [`FatalReason::Refused`] its caller reads as php's `NULL`, rather
    /// than served; the fallibility it had is the answer, for the caller
    /// to put back, as `ZEND_MM_FALLIBLE_FORWARD` puts it back.
    ///
    /// [`FatalReason::Refused`]: crate::FatalReason::Refused
    pub(crate) fn set_fallible(&mut self, fallible: bool) -> bool {
        std::mem::replace(&mut self.fallible, Asks::of(fallible)) == Asks::Fallibly
    }

    /// Whether a fallible ask was refused since this was last read.
    pub(crate) fn take_refused(&mut self) -> bool {
        std::mem::take(&mut self.refused)
    }

    /// What an ask the allocator refused hands its caller: a fallible
    /// refusal noted and carried as [`FatalReason::Refused`], anything
    /// else the request's end.
    ///
    /// [`FatalReason::Refused`]: crate::FatalReason::Refused
    #[cold]
    fn refusal(&mut self, refused: Refused) -> Fatal {
        match refused {
            Refused::Fatal(fatal) => fatal,
            Refused::Fallible => {
                self.refused = true;
                Fatal::refused()
            }
        }
    }

    /// `zend_mm_delayed_error_or_panic(heap->fallible, ...)`
    /// (`Z/Zend/zend_alloc.c:454-468` at 8.5.12), where every size php's
    /// allocator and its tables find overflowing goes, `message` its
    /// sentence: a fallible ask's refusal with the delayed OOM recorded,
    /// and otherwise `zend_mm_panic`'s end of the process
    /// ([`FatalReason::Panic`]).
    ///
    /// [`FatalReason::Panic`]: crate::FatalReason::Panic
    #[cold]
    pub(crate) fn overflow_refusal(&mut self, message: String) -> Fatal {
        if self.fallible == Asks::Fallibly {
            self.delay_message(message);
            return self.refusal(Refused::Fallible);
        }
        Fatal::panic(message)
    }

    /// `_emalloc`: [`Mm::alloc_heap`], and the observers told of what it
    /// answered, where there are any ([`crate::observe`]).
    #[inline]
    pub(crate) fn alloc(&mut self, size: usize) -> Result<NonNull<u8>, Fatal> {
        self.alloc_as(size).map_err(|refused| self.refusal(refused))
    }

    /// `_femalloc` (`Z/Zend/zend_alloc.c:3068`): [`Mm::alloc`] with the
    /// heap fallible for the one call, `ZEND_MM_FALLIBLE_FORWARD`, so a
    /// block the soft limit has no room for is `None` and leaves the
    /// delayed OOM behind.
    #[cfg(test)]
    pub(crate) fn try_alloc(&mut self, size: usize) -> Result<Option<NonNull<u8>>, Fatal> {
        let was = std::mem::replace(&mut self.fallible, Asks::Fallibly);
        let answered = self.alloc_as(size);
        self.fallible = was;
        Refused::fallible(answered)
    }

    /// [`Mm::alloc`] as the heap's fallibility stands.
    #[inline]
    fn alloc_as(&mut self, size: usize) -> Result<NonNull<u8>, Refused> {
        #[cfg(test)]
        if self.counted_out() {
            return Err(Refused::Fatal(Fatal::hard_limit(self.hard_limit, size)));
        }
        if self.entries != Entries::Zend {
            return self.alloc_custom(size);
        }
        let at = self.alloc_heap(size)?;
        #[cfg(debug_assertions)]
        self.trace(None, at);
        Ok(at)
    }

    /// `_malloc_custom`: the tracked allocator's block or Zend's, and the
    /// observers told of it.
    #[cold]
    #[inline(never)]
    fn alloc_custom(&mut self, size: usize) -> Result<NonNull<u8>, Refused> {
        let at = match self.allocator {
            Allocator::Tracked(_) => self.tracked_malloc(size)?,
            Allocator::Zend => self.alloc_heap(size)?,
        };
        #[cfg(debug_assertions)]
        self.trace(None, at);
        if !self.observers.is_empty() {
            self.observers.tell(Allocation::Allocated {
                at: at.as_ptr().addr(),
                size,
            });
        }
        Ok(at)
    }

    /// `_erealloc2`: [`Mm::realloc_heap`], and the observers told of what
    /// it answered, where there are any.
    ///
    /// # Safety
    ///
    /// As [`Mm::realloc_heap`]'s.
    #[inline]
    pub(crate) unsafe fn realloc(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Fatal> {
        // SAFETY: the caller's contract, handed on.
        unsafe { self.realloc_as(at, copy, size) }.map_err(|refused| self.refusal(refused))
    }

    /// `_ferealloc2` (`Z/Zend/zend_alloc.c:3084`): [`Mm::realloc`] with
    /// the heap fallible for the one call, so a grow the soft limit has
    /// no room for is `None`, the block left as it stood, php's "original
    /// block is left untouched".
    ///
    /// # Safety
    ///
    /// As [`Mm::realloc_heap`]'s.
    #[cfg(test)]
    pub(crate) unsafe fn try_realloc(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<Option<NonNull<u8>>, Fatal> {
        let was = std::mem::replace(&mut self.fallible, Asks::Fallibly);
        // SAFETY: the caller's contract, handed on.
        let answered = unsafe { self.realloc_as(at, copy, size) };
        self.fallible = was;
        Refused::fallible(answered)
    }

    /// [`Mm::realloc`] as the heap's fallibility stands.
    ///
    /// # Safety
    ///
    /// As [`Mm::realloc_heap`]'s.
    #[inline]
    unsafe fn realloc_as(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Refused> {
        #[cfg(test)]
        if self.counted_out() {
            return Err(Refused::Fatal(Fatal::hard_limit(self.hard_limit, size)));
        }
        if self.entries != Entries::Zend {
            // SAFETY: the caller's contract, handed on.
            return unsafe { self.realloc_custom(at, copy, size) };
        }
        // SAFETY: the caller's contract, handed on.
        let to = unsafe { self.realloc_heap(at, copy, size) }?;
        #[cfg(debug_assertions)]
        self.trace(Some(at), to);
        Ok(to)
    }

    /// `_realloc_custom`: the tracked allocator's `realloc` or Zend's,
    /// and the observers told of it.
    ///
    /// # Safety
    ///
    /// As [`Mm::realloc_heap`]'s.
    #[cold]
    #[inline(never)]
    unsafe fn realloc_custom(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Refused> {
        let to = match self.allocator {
            // SAFETY: the caller's contract, handed on.
            Allocator::Tracked(_) => unsafe { self.tracked_realloc(at, size) }?,
            // SAFETY: the caller's contract, handed on.
            Allocator::Zend => unsafe { self.realloc_heap(at, copy, size) }?,
        };
        #[cfg(debug_assertions)]
        self.trace(Some(at), to);
        if !self.observers.is_empty() {
            self.observers.tell(Allocation::Reallocated {
                from: at.as_ptr().addr(),
                to: to.as_ptr().addr(),
                size,
            });
        }
        Ok(to)
    }
    /// `_efree`: [`Mm::free_heap`], and the observers told of it, where
    /// there are any.
    ///
    /// # Safety
    ///
    /// As [`Mm::free_heap`]'s.
    #[inline]
    pub(crate) unsafe fn free(&mut self, at: NonNull<u8>) {
        if self.entries != Entries::Zend {
            // SAFETY: the caller's contract, handed on.
            unsafe { self.free_custom(at) };
            return;
        }
        // SAFETY: the caller's contract, handed on.
        unsafe { self.free_heap(at) };
        #[cfg(debug_assertions)]
        if let Some(traces) = &mut self.traces {
            traces.remove(&at.as_ptr().addr());
        }
    }

    /// `_free_custom`: the block back to the tracked allocator or to
    /// Zend's, and the observers told of it.
    ///
    /// # Safety
    ///
    /// As [`Mm::free_heap`]'s.
    #[cold]
    #[inline(never)]
    unsafe fn free_custom(&mut self, at: NonNull<u8>) {
        // SAFETY: the caller's contract, handed on.
        unsafe { self.free_block(at) };
        #[cfg(debug_assertions)]
        if let Some(traces) = &mut self.traces {
            traces.remove(&at.as_ptr().addr());
        }
        if !self.observers.is_empty() {
            self.observers.tell(Allocation::Freed {
                at: at.as_ptr().addr(),
            });
        }
    }

    /// The block at `at` given back to the allocator it came from.
    ///
    /// # Safety
    ///
    /// As [`Mm::free_heap`]'s.
    unsafe fn free_block(&mut self, at: NonNull<u8>) {
        match self.allocator {
            // SAFETY: the caller's contract, handed on.
            Allocator::Tracked(_) => unsafe { self.tracked_free(at) },
            // SAFETY: the caller's contract, handed on.
            Allocator::Zend => unsafe { self.free_heap(at) },
        }
    }

    /// `_efree` as `memory_get_usage()` and the observers see it, the
    /// bytes kept: what a pinned block's destruction does at php's
    /// moment, so the request's counts fall where php's do while a pin
    /// still names the block (design §4.4, #2230). Its bytes go back
    /// with [`Mm::free_retired`]; a block retired already is left as it
    /// stands.
    #[cold]
    #[inline(never)]
    pub(crate) fn retire(&mut self, at: NonNull<u8>) {
        if self.is_retired(at) {
            return;
        }
        // A block freed already is no block to retire: a debug build
        // names it as it names a second free ([`Mm::free_small`]).
        #[cfg(debug_assertions)]
        if matches!(self.allocator, Allocator::Zend) && freed::is_freed(at) {
            self.sensed.record(crate::sensed::Sensed::FreedTwice {
                at: at.as_ptr().addr(),
                size: self.accounted(at),
            });
            return;
        }
        self.size = self.size.saturating_sub(self.accounted(at));
        // A tracked heap's two usages are one number, as php's
        // `tracked_free` keeps them.
        if matches!(self.allocator, Allocator::Tracked(_)) {
            self.real_size = self.size;
        }
        self.retired.push(at);
        if !self.observers.is_empty() {
            self.observers.tell(Allocation::Freed {
                at: at.as_ptr().addr(),
            });
        }
    }

    /// Whether `at` is a block [`Mm::retire`] took the free of and whose
    /// bytes have not gone back since.
    pub(crate) fn is_retired(&self, at: NonNull<u8>) -> bool {
        !self.retired.is_empty() && self.retired.contains(&at)
    }

    /// The bytes of a block [`Mm::retire`] took the free of given back,
    /// with nothing counted or told twice; nothing for a block not
    /// retired.
    ///
    /// # Safety
    ///
    /// Where `at` is retired, nothing names it any more.
    #[cold]
    #[inline(never)]
    pub(crate) unsafe fn free_retired(&mut self, at: NonNull<u8>) {
        let Some(index) = self.retired.iter().position(|block| *block == at) else {
            return;
        };
        self.retired.swap_remove(index);
        self.size += self.accounted(at);
        // SAFETY: the caller's contract: the block is this heap's, its
        // free was only counted, and nothing names it.
        unsafe { self.free_block(at) };
        #[cfg(debug_assertions)]
        if let Some(traces) = &mut self.traces {
            traces.remove(&at.as_ptr().addr());
        }
    }

    /// The bytes [`Mm::free_heap`] takes off `memory_get_usage()` for the
    /// block at `at`: its bin's size, its pages, or a huge block's bytes
    /// and the bin slot of its list node.
    fn accounted(&self, at: NonNull<u8>) -> usize {
        if let Allocator::Tracked(tracked) = &self.allocator {
            return tracked.size_of(at).unwrap_or(0);
        }
        let offset = at.as_ptr().addr() & (CHUNK_SIZE - 1);
        if offset == 0 {
            return self
                .huge
                .iter()
                .find(|block| block.at == at)
                .map_or(0, |block| {
                    block.size + BIN_DATA_SIZE[size_to_bin(HUGE_LIST_SIZE)]
                });
        }
        // SAFETY: `at` is live in this heap, so the chunk it sits in is
        // one of ours and its header is initialised.
        let info = unsafe { &*chunk_of(at).as_ptr() }.map[offset / PAGE_SIZE];
        if info & IS_SRUN != 0 {
            BIN_DATA_SIZE[(info & SRUN_BIN_NUM_MASK) as usize]
        } else {
            (info & LRUN_PAGES_MASK) as usize * PAGE_SIZE
        }
    }

    /// Whether `at` is a block this heap handed out, what php's
    /// `zend_mm_free_heap` checks as `chunk->heap == heap`: a chunk-aligned
    /// address one of its huge blocks, any other one whose chunk is one of
    /// its chunks. A chunk is the heap's own 2 MiB and a huge block its
    /// own mapping, so no block of the system allocator lies inside
    /// either, and the answer reads only the heap's own lists, never the
    /// memory at `at`. A slot the debug quarantine holds is still in its
    /// chunk, so it answers yes and its second free is sensed as one.
    pub(crate) fn owns(&self, at: NonNull<u8>) -> bool {
        if let Allocator::Tracked(tracked) = &self.allocator {
            return tracked.holds(at.as_ptr().addr());
        }
        if at.as_ptr().addr() & (CHUNK_SIZE - 1) == 0 {
            return self.huge.iter().any(|block| block.at == at);
        }
        self.chunks.contains(&chunk_of(at))
    }

    /// Whether `address` lies anywhere inside one of this heap's chunks
    /// or huge blocks, what php's `is_zend_ptr` answers: its chunks in
    /// their walk order, then its huge blocks, each a range of addresses
    /// compared and nothing at `address` read, so any number is a
    /// question it can answer. Unlike [`Mm::owns`] it asks of an
    /// address, not of a block the heap handed out.
    pub(crate) fn holds(&self, address: usize) -> bool {
        if let Allocator::Tracked(tracked) = &self.allocator {
            return tracked.holds(address);
        }
        let within =
            |start: usize, size: usize| (start..start.saturating_add(size)).contains(&address);
        self.chunks
            .iter()
            .any(|chunk| within(chunk.as_ptr().addr(), CHUNK_SIZE))
            || self
                .huge
                .iter()
                .any(|block| within(block.at.as_ptr().addr(), block.size))
    }

    /// The bytes the block at `at` can hold, what `zend_mm_size` answers:
    /// its bin's size, its pages, or a huge block's rounded size.
    ///
    /// # Safety
    ///
    /// `at` is live in this heap ([`Mm::owns`]).
    pub(crate) unsafe fn usable(&self, at: NonNull<u8>) -> usize {
        if let Allocator::Tracked(tracked) = &self.allocator {
            return tracked.size_of(at).unwrap_or(0);
        }
        let offset = at.as_ptr().addr() & (CHUNK_SIZE - 1);
        if offset == 0 {
            return self
                .huge
                .iter()
                .find(|block| block.at == at)
                .map_or(0, |block| block.size);
        }
        // SAFETY: the caller's contract: `at` is live in this heap, so the
        // chunk it sits in is one of ours and its header is initialised.
        let info = unsafe { &*chunk_of(at).as_ptr() }.map[offset / PAGE_SIZE];
        if info & IS_SRUN != 0 {
            BIN_DATA_SIZE[(info & SRUN_BIN_NUM_MASK) as usize]
        } else {
            (info & LRUN_PAGES_MASK) as usize * PAGE_SIZE
        }
    }

    /// `zend_mm_gc` as `gc_mem_caches()` calls it: [`Mm::gc_heap`], and
    /// the observers told of the bytes it gave back, where there are
    /// any.
    pub(crate) fn gc(&mut self) -> usize {
        let freed = self.gc_heap();
        if !self.observers.is_empty() {
            self.observers.tell(Allocation::Collected { freed });
        }
        freed
    }

    /// `observer` put in last in this heap's list, and its place.
    pub(crate) fn observe(
        &mut self,
        observer: Box<dyn ObserveAllocations>,
    ) -> AllocationObserverId {
        let at = self.observers.add(observer);
        self.reckon_custom();
        at
    }

    /// The observer at `at` taken out of this heap's list, where it is
    /// still in.
    pub(crate) fn unobserve(
        &mut self,
        at: AllocationObserverId,
    ) -> Option<Box<dyn ObserveAllocations>> {
        let removed = self.observers.remove(at);
        self.reckon_custom();
        removed
    }

    /// [`Mm::entries`] set again as the allocator and the observers stand.
    fn reckon_custom(&mut self) {
        let custom = matches!(self.allocator, Allocator::Tracked(_)) || !self.observers.is_empty();
        self.entries = if custom {
            Entries::Custom
        } else {
            Entries::Zend
        };
    }

    /// `zend_mm_alloc_heap`: `size` usable bytes, from the bin its size
    /// falls in, sixteen at least as it raises it, from a `run` of pages,
    /// or from a huge block of its own.
    #[inline] // `zend_always_inline`, its large and huge arms kept out of line
    fn alloc_heap(&mut self, size: usize) -> Result<NonNull<u8>, Refused> {
        let size = size.max(MIN_USEABLE_BIN_SIZE);
        if size <= MAX_SMALL_SIZE {
            self.alloc_small(size_to_bin(size))
        } else if size <= MAX_LARGE_SIZE {
            self.alloc_large(size)
        } else {
            self.alloc_huge(size)
        }
    }

    /// `zend_mm_realloc_heap` (`Z/Zend/zend_alloc.c:1674`):
    /// `size` usable bytes with the first `copy` of the old ones in them,
    /// grown or cut where the block stands when php's would be, and moved
    /// otherwise. What a block's arm does is what php's does, because the
    /// pages the `run` takes and the chunk a moved block lands in are read
    /// back by `memory_get_usage(true)` and the limit:
    ///
    /// - A bin slot whose new size falls in the same bin is the same
    ///   slot; one that falls a bin lower is moved down; one that grows
    ///   within the bins is moved up, the peak restored past the two
    ///   standing together.
    /// - A `run` of pages that keeps its page count is the same `run`;
    ///   one that shrinks gives its tail pages back; one that grows takes
    ///   the pages after it when they are free, and moves otherwise.
    /// - A huge block that keeps its rounded size is the same block, and
    ///   one that shrinks is cut to the new rounding as `zend_mm_chunk_truncate`
    ///   cuts one. One that grows asks the machine for the pages after
    ///   it, `zend_mm_chunk_extend`, and moves where the answer is no.
    ///
    /// A move is `zend_mm_realloc_slow` (`:1569`): alloc, copy, free, and
    /// the peak put back where the larger of the two footprints put it,
    /// not where both stood at once.
    ///
    /// # Safety
    ///
    /// `at` is a pointer this heap returned from [`Mm::alloc`], has not
    /// been freed since, and its first `copy` bytes are initialised.
    unsafe fn realloc_heap(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Refused> {
        let offset = at.as_ptr().addr() & (CHUNK_SIZE - 1);
        if offset == 0 {
            // SAFETY: the caller's contract; a chunk-aligned pointer of
            // this heap is a huge block.
            return unsafe { self.realloc_huge(at, copy, size) };
        }
        let size = size.max(MIN_USEABLE_BIN_SIZE);
        let chunk = chunk_of(at);
        let page_num = offset / PAGE_SIZE;
        // SAFETY: `at` is live in this heap, so the chunk it sits in is
        // one of ours and its header is initialised.
        let info = unsafe { &*chunk.as_ptr() }.map[page_num];
        let old_size = if info & IS_SRUN != 0 {
            let old_bin = (info & SRUN_BIN_NUM_MASK) as usize;
            let old_size = BIN_DATA_SIZE[old_bin];
            if size <= old_size {
                if old_bin > 0 && size < BIN_DATA_SIZE[old_bin - 1] {
                    // Truncation: down to the bin the size falls in.
                    let moved = self.alloc_small(size_to_bin(size))?;
                    // SAFETY: `at` holds `copy` readable bytes and
                    // `moved` is a fresh slot of at least `size`.
                    unsafe { copy_into(at, moved, copy.min(size)) };
                    // SAFETY: the caller's contract, and the page map says
                    // the slot is bin `old_bin`'s.
                    unsafe { self.free_small(at, old_bin) };
                    return Ok(moved);
                }
                // Reallocation in place: the same slot.
                return Ok(at);
            }
            if size <= MAX_SMALL_SIZE {
                // A small extension, with the peak restored as the slow
                // path restores it.
                let peak = self.peak;
                let moved = self.alloc_small(size_to_bin(size))?;
                // SAFETY: `at` holds `copy` readable bytes of a slot of
                // `old_size`, and `moved` is a fresh slot past that.
                unsafe { copy_into(at, moved, copy.min(old_size)) };
                // SAFETY: the caller's contract, and the page map says
                // the slot is bin `old_bin`'s.
                unsafe { self.free_small(at, old_bin) };
                self.peak = peak.max(self.size);
                return Ok(moved);
            }
            old_size
        } else {
            let old_pages = info & LRUN_PAGES_MASK;
            let old_size = old_pages as usize * PAGE_SIZE;
            if size > MAX_SMALL_SIZE && size <= MAX_LARGE_SIZE {
                let new_size = size.next_multiple_of(PAGE_SIZE);
                let new_pages = (new_size / PAGE_SIZE) as u32;
                // SAFETY: the run is in this chunk, so its header is ours
                // to write and nothing else holds it.
                let header = unsafe { &mut *chunk.as_ptr() };
                let page_num = page_num as u32;
                if new_size == old_size {
                    return Ok(at);
                }
                if new_size < old_size {
                    // The tail pages go back.
                    let rest = old_pages - new_pages;
                    self.size = self.size.saturating_sub(rest as usize * PAGE_SIZE);
                    header.map[page_num as usize] = IS_LRUN | new_pages;
                    header.free_pages += rest;
                    bitset_reset_range(&mut header.free_map, page_num + new_pages, rest);
                    return Ok(at);
                }
                // The pages after the run, when they are free.
                let more = new_pages - old_pages;
                if page_num + new_pages <= PAGES
                    && bitset_is_free_range(&header.free_map, page_num + old_pages, more)
                {
                    self.size += more as usize * PAGE_SIZE;
                    self.peak = self.peak.max(self.size);
                    header.free_pages -= more;
                    bitset_set_range(&mut header.free_map, page_num + old_pages, more);
                    header.map[page_num as usize] = IS_LRUN | new_pages;
                    return Ok(at);
                }
            }
            old_size
        };
        // SAFETY: the caller's contract, and `old_size` is what the page
        // map says the block holds.
        unsafe { self.realloc_slow(at, copy.min(old_size), size) }
    }

    /// `zend_mm_realloc_slow` (`Z/Zend/zend_alloc.c:1569`): alloc, copy,
    /// free, and the peak where the larger of the two footprints put it.
    ///
    /// # Safety
    ///
    /// `at` is live in this heap and holds `copy` readable bytes, no more
    /// than `size`.
    unsafe fn realloc_slow(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Refused> {
        let peak = self.peak;
        let moved = self.alloc_heap(size)?;
        // SAFETY: `at` holds `copy` readable bytes and `moved` is a fresh
        // allocation of `size`, so the shorter of the two is in both.
        unsafe { copy_into(at, moved, copy.min(size)) };
        // SAFETY: the caller's contract, `at` is live in this heap; its
        // bytes are copied and nothing reads it after this.
        unsafe { self.free_heap(at) };
        self.peak = peak.max(self.size);
        Ok(moved)
    }

    /// `zend_mm_realloc_huge` (`Z/Zend/zend_alloc.c:1587`): the block
    /// stands when its rounding does not move; a shorter rounding cuts
    /// its tail where it stands, `zend_mm_chunk_truncate`, and the two
    /// sizes move by the difference; a longer one is held to the limit by
    /// the difference alone and then asks the machine for the pages
    /// after the block, `zend_mm_chunk_extend`, the two sizes and their
    /// peaks moving by the difference where it answers yes. Where it
    /// answers no, or the size is no longer huge, the block moves, the
    /// slow path. So a string grown by appends past 2 MiB is copied only
    /// where the machine has mapped something after it, as php's is.
    ///
    /// The real usage moves by what the block gained whether it grew
    /// where it stands or moved. The real peak moves by that alone while
    /// the machine lets the block grow where it stands, and by one copy
    /// of the block more where it does not, which is the machine's
    /// mapping at that moment, in php's process as in this one (#1410);
    /// so the fence holds the peak to that bound and no number:
    ///
    /// ```php
    /// $s = str_repeat('x', 3000000); $t = str_repeat('y', 10000); memory_reset_peak_usage(); $r = memory_get_usage(true);
    /// for ($i = 0; $i < 100; $i++) { $s .= $t; }
    /// $grown = memory_get_usage(true) - $r;
    /// var_dump(strlen($s), $grown, memory_get_peak_usage(true) - $r <= $grown + strlen($s));
    /// ```
    /// ```expect
    /// int(4000000)
    /// int(999424)
    /// bool(true)
    /// ```
    ///
    /// A longer rounding the limit has no room for is held to it by the
    /// difference alone, and grows: the delayed OOM names the difference,
    /// the program runs on to its end, where the fatal is raised, and the
    /// shutdown functions see the grown block.
    ///
    /// ```php
    /// ini_set('memory_limit', '7M');
    /// $s = str_repeat('x', 3000000);
    /// register_shutdown_function(function () { echo "shutdown sees ", strlen($GLOBALS['s']), "\n"; });
    /// $s .= $s;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 7340032 bytes exhausted (tried to allocate 2998272 bytes) in %s on line 5
    /// shutdown sees 6000000
    /// ```
    ///
    /// A size the rounding would carry past the top of memory is refused
    /// as [`Mm::alloc_huge`] refuses a fresh block of it, before the limit
    /// is asked, and the block stands where it stood with its bytes and
    /// its count ([`Mm::huge_rounding`]). php rounds here without that
    /// check (`Z/Zend/zend_alloc.c:1757`): its rounding wraps to zero,
    /// `zend_mm_chunk_truncate` unmaps the whole block, and the block is
    /// returned unmapped (#3602).
    ///
    /// # Safety
    ///
    /// `at` is a huge block of this heap holding `copy` readable bytes.
    unsafe fn realloc_huge(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Refused> {
        let Some(index) = self.huge.iter().position(|block| block.at == at) else {
            return Err(Refused::Fatal(Fatal::error("zend_mm_heap corrupted")));
        };
        let old_size = self.huge[index].size;
        if size > MAX_LARGE_SIZE {
            let new_size = self.huge_rounding(size)?;
            if new_size == old_size {
                return Ok(at);
            }
            if new_size < old_size {
                if huge_truncate(at, old_size, new_size) {
                    self.huge[index].size = new_size;
                    self.real_size = self.real_size.saturating_sub(old_size - new_size);
                    self.size = self.size.saturating_sub(old_size - new_size);
                    return Ok(at);
                }
            } else {
                let more = new_size - old_size;
                self.check_limit(more, true)?;
                if huge_extend(at, old_size, new_size) {
                    self.huge[index].size = new_size;
                    self.real_size += more;
                    self.real_peak = self.real_peak.max(self.real_size);
                    self.size += more;
                    self.peak = self.peak.max(self.size);
                    return Ok(at);
                }
            }
        }
        // SAFETY: the caller's contract, and `old_size` is the block's.
        unsafe { self.realloc_slow(at, copy.min(old_size), size) }
    }

    /// `zend_mm_free_heap`: give `at` back to the bin, `run` or huge
    /// block it came from. php reads what it is from the chunk's page
    /// map, and so does this, so a pointer carries no header of its own.
    ///
    /// # Safety
    ///
    /// `at` is a pointer this heap returned from [`Mm::alloc`] and has
    /// not been freed since.
    unsafe fn free_heap(&mut self, at: NonNull<u8>) {
        let offset = at.as_ptr().addr() & (CHUNK_SIZE - 1);
        if offset == 0 {
            self.free_huge(at);
            return;
        }
        let chunk = chunk_of(at);
        let page_num = offset / PAGE_SIZE;
        // SAFETY: `at` is live in this heap, so the chunk it sits in is
        // one of ours and its header is initialised.
        let info = unsafe { &*chunk.as_ptr() }.map[page_num];
        if info & IS_SRUN != 0 {
            // SAFETY: the page map says a bin run starts here, so `at` is
            // one of that bin's slots.
            unsafe { self.free_small(at, (info & SRUN_BIN_NUM_MASK) as usize) };
        } else {
            let pages_count = info & LRUN_PAGES_MASK;
            self.size = self.size.saturating_sub(pages_count as usize * PAGE_SIZE);
            #[cfg(debug_assertions)]
            freed::given_back(at, pages_count as usize * PAGE_SIZE);
            self.free_pages(chunk, page_num as u32, pages_count, true);
        }
    }

    /// `zend_mm_alloc_small` (`Z/Zend/zend_alloc.c:1392`): the head of the
    /// bin's free list, or a fresh `run` when the list is empty.
    #[inline] // `zend_always_inline`, its slow arm out of line
    fn alloc_small(&mut self, bin_num: usize) -> Result<NonNull<u8>, Refused> {
        self.size += BIN_DATA_SIZE[bin_num];
        self.peak = self.peak.max(self.size);
        #[cfg(debug_assertions)]
        if self.free_slot[bin_num].is_null() && self.limit_is_near() {
            self.evict_oldest(bin_num);
        }
        let slot = match NonNull::new(self.free_slot[bin_num]) {
            Some(slot) => {
                // SAFETY: a slot on the free list is free memory of this
                // heap, whose first word is the next free slot.
                self.free_slot[bin_num] = unsafe { slot.as_ref() }.next;
                slot.cast()
            }
            None => self.alloc_small_slow(bin_num)?,
        };
        #[cfg(debug_assertions)]
        {
            freed::handed_out(slot, BIN_DATA_SIZE[bin_num]);
            origins::write(slot, self.origin);
        }
        Ok(slot)
    }

    /// `zend_mm_alloc_small_slow` (`Z/Zend/zend_alloc.c:1338`): a `run` of
    /// pages for this bin, threaded into a free list of its elements, and
    /// the first element returned. The accounting was done by the caller,
    /// as php's is, so a failure here undoes it.
    #[cold]
    #[inline(never)] // `zend_never_inline`, as php keeps it off the bin's fast path
    fn alloc_small_slow(&mut self, bin_num: usize) -> Result<NonNull<u8>, Refused> {
        let pages = BIN_PAGES[bin_num];
        let run = match self.alloc_pages(pages, PAGE_SIZE * pages as usize) {
            Ok(run) => run,
            Err(fatal) => {
                self.size = self.size.saturating_sub(BIN_DATA_SIZE[bin_num]);
                return Err(fatal);
            }
        };
        let chunk = chunk_of(run);
        let page_num = (run.as_ptr().addr() & (CHUNK_SIZE - 1)) / PAGE_SIZE;
        // SAFETY: the run was just carved out of this chunk, so its
        // header is ours to write and nothing else holds it.
        let header = unsafe { &mut *chunk.as_ptr() };
        header.map[page_num] = IS_SRUN | bin_num as u32;
        for i in 1..pages as usize {
            header.map[page_num + i] =
                IS_SRUN | IS_LRUN | bin_num as u32 | ((i as u32) << NRUN_OFFSET_SHIFT);
        }
        let data_size = BIN_DATA_SIZE[bin_num];
        let mut next = std::ptr::null_mut();
        for i in (1..BIN_ELEMENTS[bin_num]).rev() {
            // SAFETY: element `i` of the run is inside the pages just
            // allocated, and a free slot's first word is its link.
            let slot: *mut FreeSlot = unsafe { run.as_ptr().add(i * data_size) }.cast();
            // SAFETY: the slot is free memory of this heap, so writing
            // its link word writes nothing a caller holds.
            unsafe { slot.write(FreeSlot { next }) };
            next = slot;
        }
        self.free_slot[bin_num] = next;
        Ok(run)
    }

    /// `zend_mm_free_small` (`Z/Zend/zend_alloc.c:1417`): the slot goes
    /// back on the head of its bin's free list; in a debug build, into its
    /// bin's [`Quarantine`] first, and a slot the quarantine holds already
    /// is a free of a freed block, recorded as the `InternalError` naming
    /// it and not freed again.
    ///
    /// # Safety
    ///
    /// `at` is a live slot of bin `bin_num` in this heap.
    unsafe fn free_small(&mut self, at: NonNull<u8>, bin_num: usize) {
        #[cfg(debug_assertions)]
        if self.quarantine[bin_num].holds(at.cast()) {
            self.sensed.record(crate::sensed::Sensed::FreedTwice {
                at: at.as_ptr().addr(),
                size: BIN_DATA_SIZE[bin_num],
            });
            return;
        }
        self.size = self.size.saturating_sub(BIN_DATA_SIZE[bin_num]);
        #[cfg(debug_assertions)]
        {
            freed::given_back(at, BIN_DATA_SIZE[bin_num]);
            self.hold_back(at.cast(), bin_num);
        }
        #[cfg(not(debug_assertions))]
        // SAFETY: the caller's contract, handed on.
        unsafe {
            self.push_free(at.cast(), bin_num);
        }
    }

    /// The free slot at `slot` on the head of its bin's free list.
    ///
    /// # Safety
    ///
    /// `slot` is a slot of bin `bin_num` in this heap that nothing holds
    /// and no free list or quarantine names.
    unsafe fn push_free(&mut self, slot: NonNull<FreeSlot>, bin_num: usize) {
        // SAFETY: the caller's contract: the slot's bytes are the heap's
        // and the link word may be written over them.
        unsafe {
            slot.write(FreeSlot {
                next: self.free_slot[bin_num],
            });
        };
        self.free_slot[bin_num] = slot.as_ptr();
    }

    /// `zend_mm_alloc_large` (`Z/Zend/zend_alloc.c:1114`): whole pages,
    /// and the accounting is the pages and not the bytes asked for. So is
    /// the number a message names: outside `ZEND_DEBUG` php hands
    /// `zend_mm_alloc_pages` the page count alone, so both messages it
    /// raises name `ZEND_MM_PAGE_SIZE * pages_count`.
    #[inline(never)] // off the bins' path, so a small block's needs no frame of its own
    fn alloc_large(&mut self, size: usize) -> Result<NonNull<u8>, Refused> {
        let pages_count = size.div_ceil(PAGE_SIZE) as u32;
        let at = self.alloc_pages(pages_count, PAGE_SIZE * pages_count as usize)?;
        self.size += pages_count as usize * PAGE_SIZE;
        self.peak = self.peak.max(self.size);
        #[cfg(debug_assertions)]
        {
            freed::handed_out(at, pages_count as usize * PAGE_SIZE);
            origins::write(at, self.origin);
        }
        Ok(at)
    }

    /// `zend_mm_alloc_pages` (`Z/Zend/zend_alloc.c:908`): the best-fit `run`
    /// of `pages_count` free pages in the first chunk that has one, else a
    /// chunk from the cache, else a new chunk. `asked` is the number php
    /// names in the limit's message at this site.
    fn alloc_pages(&mut self, pages_count: u32, asked: usize) -> Result<NonNull<u8>, Refused> {
        let mut found = None;
        for (steps, chunk) in self.chunks.iter().copied().enumerate() {
            // SAFETY: every chunk on the list is one this heap mapped and
            // initialised, and no other reference to its header is live.
            let header = unsafe { &mut *chunk.as_ptr() };
            if header.free_pages < pages_count {
                continue;
            }
            if let Some(page_num) = best_fit(header, pages_count) {
                found = Some((steps, chunk, page_num));
                break;
            }
        }
        let (page_num, chunk) = match found {
            Some((steps, chunk, page_num)) => {
                // php moves a chunk it had to walk for into the head of
                // the list, so the next walk is shorter.
                if steps > 2 && pages_count < 8 {
                    self.chunks.remove(steps);
                    self.chunks.insert(1, chunk);
                }
                (page_num, chunk)
            }
            None => (FIRST_PAGE, self.grow(asked)?),
        };
        // SAFETY: the run was just chosen in this chunk, so its header is
        // ours to write and nothing else holds it.
        let header = unsafe { &mut *chunk.as_ptr() };
        header.free_pages -= pages_count;
        bitset_set_range(&mut header.free_map, page_num, pages_count);
        header.map[page_num as usize] = IS_LRUN | pages_count;
        if page_num == header.free_tail {
            header.free_tail = page_num + pages_count;
        }
        // SAFETY: page `page_num` is inside the chunk, which is one
        // allocation of `CHUNK_SIZE` bytes.
        let at = unsafe { chunk.cast::<u8>().add(page_num as usize * PAGE_SIZE) };
        Ok(at)
    }

    /// One more chunk on the list: the cache's newest if it has one, else
    /// a fresh mapping, which is where php holds a request to its
    /// `memory_limit` (`Z/Zend/zend_alloc.c:1175-1194` at 8.5.12), for the
    /// chunk's whole 2 MiB, which is the number the limit's message names
    /// here. A sweep that freed anything sends it round again, php's
    /// `goto get_chunk`, so a chunk the sweep emptied into the cache is
    /// taken from the cache; past the soft limit with nothing swept the
    /// chunk is mapped all the same, the delayed OOM recorded.
    fn grow(&mut self, asked: usize) -> Result<NonNull<Chunk>, Refused> {
        let chunk = loop {
            if let Some(chunk) = self.cached.pop() {
                break chunk;
            }
            if self.check_limit(CHUNK_SIZE, true)? {
                continue;
            }
            let mapped = match chunk_alloc(CHUNK_SIZE) {
                Some(chunk) => Some(chunk),
                None if self.gc_heap() != 0 => chunk_alloc(CHUNK_SIZE),
                None => None,
            };
            let Some(chunk) = mapped else {
                return Err(Refused::Fatal(out_of_memory(self.real_size, asked)));
            };
            self.real_size += CHUNK_SIZE;
            self.real_peak = self.real_peak.max(self.real_size);
            break chunk;
        };
        // SAFETY: the tail of the list is a chunk this heap initialised.
        let num = self
            .chunks
            .last()
            .map_or(0, |last| unsafe { &*last.as_ptr() }.num + 1);
        self.chunk_init(chunk, num);
        self.chunks.push(chunk);
        self.peak_chunks_count = self.peak_chunks_count.max(self.chunks.len());
        Ok(chunk)
    }

    /// `zend_mm_free_pages_ex` (`Z/Zend/zend_alloc.c:1178`): the `run` goes
    /// back to the chunk, and with `free_chunk` a chunk that is wholly
    /// free and is not the main one leaves the list. The sweep passes
    /// `false`, because it deletes the chunk itself once it has walked
    /// the whole of it.
    #[inline(never)] // off the bins' path, as `alloc_large` is
    fn free_pages(
        &mut self,
        chunk: NonNull<Chunk>,
        page_num: u32,
        pages_count: u32,
        free_chunk: bool,
    ) {
        // SAFETY: the run being freed is in this chunk, so its header is
        // ours to write and nothing else holds it.
        let header = unsafe { &mut *chunk.as_ptr() };
        header.free_pages += pages_count;
        bitset_reset_range(&mut header.free_map, page_num, pages_count);
        header.map[page_num as usize] = 0;
        if header.free_tail == page_num + pages_count {
            header.free_tail = page_num;
        }
        let empty = header.free_pages == PAGES - FIRST_PAGE;
        if free_chunk && empty && self.chunks.first() != Some(&chunk) {
            self.delete_chunk(chunk);
        }
    }

    /// php's fast shutdown has begun, `zend_shutdown_executor_values(1)`,
    /// which frees none of what the request held and leaves every chunk
    /// on the list for `zend_mm_shutdown`: a chunk the engine's teardown
    /// empties from here on is cached, never unmapped, so what the heap
    /// keeps for the next request ([`Mm::keep`]) is what php's keeps.
    pub(crate) fn keep_emptied(&mut self) {
        self.keep_emptied = true;
    }

    /// `zend_mm_delete_chunk` (`Z/Zend/zend_alloc.c:1140`): a chunk that
    /// emptied is kept mapped while the request has been using about that
    /// many, and given back otherwise; past the fast shutdown's start
    /// ([`Mm::keep_emptied`]) it is kept.
    fn delete_chunk(&mut self, chunk: NonNull<Chunk>) {
        let Some(at) = self.chunks.iter().position(|c| *c == chunk) else {
            return;
        };
        self.chunks.remove(at);
        #[cfg(debug_assertions)]
        origins::give_back(chunk);
        let count = self.chunks.len();
        // php compares the chunks a request holds against a running
        // average of what its heap has been holding, which a heap kept
        // between requests carries ([`ChunkCache`]) and a new one starts
        // at the 1.0 `zend_mm_init` leaves.
        let held = (count + self.cached.len()) as f64;
        if self.keep_emptied {
            self.cached.push(chunk);
            return;
        }
        if held < self.avg_chunks_count + 0.1
            || (count == self.last_delete_boundary && self.last_delete_count >= 4)
        {
            self.cached.push(chunk);
            return;
        }
        self.real_size = self.real_size.saturating_sub(CHUNK_SIZE);
        if self.cached.is_empty() {
            if count == self.last_delete_boundary {
                self.last_delete_count += 1;
            } else {
                self.last_delete_boundary = count;
                self.last_delete_count = 0;
            }
        }
        // php keeps whichever of the two chunks was mapped first.
        let newer = self.cached.last().is_none_or(|cached| {
            // SAFETY: a chunk on the cache is one this heap mapped and
            // initialised.
            let cached_num = unsafe { &*cached.as_ptr() }.num;
            // SAFETY: the chunk has left the list and holds nothing, so
            // nothing else reads its header.
            let num = unsafe { &*chunk.as_ptr() }.num;
            num > cached_num
        });
        if newer {
            chunk_free(chunk, CHUNK_SIZE);
        } else if let Some(cached) = self.cached.pop() {
            chunk_free(cached, CHUNK_SIZE);
            self.cached.push(chunk);
        }
    }

    /// `zend_mm_gc` (`Z/Zend/zend_alloc.c:2101`): the sweep of wholly free
    /// bin `runs` php makes before it declares the limit reached, and the
    /// bytes it collected.
    ///
    /// Two passes over each of the thirty free lists, then one over the
    /// chunks. The first pass counts the free slots of each `run` it
    /// meets into the free-slot count of that `run`'s page map word; the
    /// second unlinks every slot of a `run` whose count reached the
    /// `run`'s element count; the walk of the chunks gives those `runs`
    /// back and resets the count of every other, so the next sweep
    /// starts from zero. A debug build drains its [`Quarantine`] first,
    /// so the sweep counts every slot php's would count free.
    fn gc_heap(&mut self) -> usize {
        // A custom heap's sweep is its handler's, and the tracked one
        // names none (`Z/Zend/zend_alloc.c:2242`).
        if let Allocator::Tracked(_) = self.allocator {
            return 0;
        }
        #[cfg(debug_assertions)]
        self.drain_quarantine();
        for (bin_num, &elements) in BIN_ELEMENTS.iter().enumerate() {
            let elements = elements as u32;
            let mut has_free_pages = false;
            let mut next = NonNull::new(self.free_slot[bin_num]);
            while let Some(slot) = next {
                let (chunk, page_num) = run_of(slot);
                // SAFETY: a slot on the free list is free memory of this
                // heap, so its chunk is one of ours, its header is
                // initialised and no other reference to it is live.
                let header = unsafe { &mut *chunk.as_ptr() };
                let free_counter = srun_free_counter(header.map[page_num]) + 1;
                if free_counter == elements {
                    has_free_pages = true;
                }
                // php's `ZEND_MM_SRUN_EX`, which does not mask the count
                // it lays in: bin zero's `run` holds 512 slots and the
                // field is nine bits, so the count of a wholly free `run`
                // of that bin reads back as zero and the passes below
                // leave it alone, as php's leave it alone.
                header.map[page_num] =
                    IS_SRUN | bin_num as u32 | (free_counter << SRUN_FREE_COUNTER_SHIFT);
                // SAFETY: the slot is free memory of this heap, whose
                // first word is the next free slot of the same bin.
                next = NonNull::new(unsafe { slot.as_ref() }.next);
            }
            if !has_free_pages {
                continue;
            }
            let mut prev: Option<NonNull<FreeSlot>> = None;
            let mut next = NonNull::new(self.free_slot[bin_num]);
            while let Some(slot) = next {
                // SAFETY: the slot is free memory of this heap, whose
                // first word is the next free slot of the same bin.
                let after = unsafe { slot.as_ref() }.next;
                let (chunk, page_num) = run_of(slot);
                // SAFETY: the chunk the slot sits in is one this heap
                // mapped and initialised.
                let info = unsafe { &*chunk.as_ptr() }.map[page_num];
                if srun_free_counter(info) == elements {
                    match prev {
                        // SAFETY: the previous slot is free memory of
                        // this heap and its whole content is its link.
                        Some(prev) => unsafe { prev.as_ptr().write(FreeSlot { next: after }) },
                        None => self.free_slot[bin_num] = after,
                    }
                } else {
                    prev = Some(slot);
                }
                next = NonNull::new(after);
            }
        }
        let main = self.chunks.first().copied();
        let mut collected = 0;
        for chunk in self.chunks.clone() {
            let mut page_num = FIRST_PAGE;
            loop {
                // SAFETY: every chunk on the list is one this heap mapped
                // and initialised, and no other reference to its header
                // is live.
                let header = unsafe { &*chunk.as_ptr() };
                // php re-reads the tail each time round, because freeing
                // a `run` at the tail moves it.
                if page_num >= header.free_tail {
                    break;
                }
                let taken = bitset_is_set(&header.free_map, page_num);
                let info = header.map[page_num as usize];
                if !taken {
                    page_num += 1;
                    continue;
                }
                if info & IS_SRUN == 0 {
                    page_num += info & LRUN_PAGES_MASK;
                    continue;
                }
                let bin_num = (info & SRUN_BIN_NUM_MASK) as usize;
                let pages_count = BIN_PAGES[bin_num];
                if srun_free_counter(info) == BIN_ELEMENTS[bin_num] as u32 {
                    self.free_pages(chunk, page_num, pages_count, false);
                    collected += pages_count;
                } else {
                    // SAFETY: the chunk is one this heap mapped and
                    // initialised, and no other reference to it is live.
                    let header = unsafe { &mut *chunk.as_ptr() };
                    header.map[page_num as usize] = IS_SRUN | bin_num as u32;
                }
                page_num += pages_count;
            }
            // SAFETY: the chunk is one this heap mapped and initialised.
            let free_pages = unsafe { &*chunk.as_ptr() }.free_pages;
            if free_pages == PAGES - FIRST_PAGE && main != Some(chunk) {
                self.delete_chunk(chunk);
            }
        }
        collected as usize * PAGE_SIZE
    }

    /// `zend_mm_alloc_huge` (`Z/Zend/zend_alloc.c:2071` at 8.5.12): a
    /// mapping of its own, rounded up to the page the machine reports and
    /// not to the chunk's own page, counted in both the bin accounting
    /// and the real size, and held to the limit for that rounding, the
    /// number its message names. Between the mapping and the count,
    /// `zend_mm_add_huge_block` takes the block's list node from the
    /// 24-byte bin of the same heap, never fallibly, so the bin accounting
    /// moves by 24 and then by the rounded block, and the peak is the sum.
    /// A node the bin cannot give ends php's request there; here the
    /// mapping goes back and the fatal is the bin's. A size the rounding
    /// would carry past the top of memory is php's error before any of
    /// that, the limit not consulted ([`Mm::huge_rounding`]).
    #[cold]
    #[inline(never)] // off the bins' path, as `alloc_large` is
    fn alloc_huge(&mut self, size: usize) -> Result<NonNull<u8>, Refused> {
        let new_size = self.huge_rounding(size)?;
        self.check_limit(new_size, true)?;
        let mapped = match huge_map(new_size) {
            Some(at) => Some(at),
            None if self.gc_heap() != 0 => huge_map(new_size),
            None => None,
        };
        let Some(at) = mapped else {
            return Err(Refused::Fatal(out_of_memory(self.real_size, size)));
        };
        let was = std::mem::replace(&mut self.fallible, Asks::Infallibly);
        let node = self.alloc_small(size_to_bin(HUGE_LIST_SIZE));
        self.fallible = was;
        let node = match node {
            Ok(node) => node,
            Err(refused) => {
                huge_unmap(at, new_size);
                return Err(refused);
            }
        };
        self.huge.push(Huge {
            at,
            size: new_size,
            node,
            #[cfg(debug_assertions)]
            origin: self.origin,
        });
        self.real_size += new_size;
        self.real_peak = self.real_peak.max(self.real_size);
        self.size += new_size;
        self.peak = self.peak.max(self.size);
        Ok(at)
    }

    /// `ZEND_MM_ALIGNED_SIZE_EX(size, REAL_PAGE_SIZE)`, a huge block's
    /// size rounded up to the page the machine reports, with
    /// `zend_mm_alloc_huge`'s check on it (`Z/Zend/zend_alloc.c:2079`): a
    /// size the rounding would carry past the top of memory is php's
    /// `Possible integer overflow` with the size and the page in its
    /// words ([`Mm::overflow_refusal`]). [`Mm::alloc_huge`] and
    /// [`Mm::realloc_huge`] both round through this, so neither wraps;
    /// php checks only the first.
    fn huge_rounding(&mut self, size: usize) -> Result<usize, Refused> {
        let page = real_page_size();
        if let Some(rounded) = size.checked_next_multiple_of(page) {
            return Ok(rounded);
        }
        let message = format!("Possible integer overflow in memory allocation ({size} + {page})");
        Err(Refused::Fatal(self.overflow_refusal(message)))
    }

    /// `zend_mm_free_huge` (`Z/Zend/zend_alloc.c:1988`): the node goes
    /// back to its bin in `zend_mm_del_huge_block`
    /// (`Z/Zend/zend_alloc.c:1856`), then the mapping and its bytes.
    #[cold]
    #[inline(never)] // as `alloc_huge`
    fn free_huge(&mut self, at: NonNull<u8>) {
        let Some(index) = self.huge.iter().position(|block| block.at == at) else {
            return;
        };
        let block = self.huge.swap_remove(index);
        // SAFETY: the node is a slot `alloc_huge` took from the 24-byte
        // bin for this block and nothing else has freed it.
        unsafe { self.free_small(block.node, size_to_bin(HUGE_LIST_SIZE)) };
        self.real_size = self.real_size.saturating_sub(block.size);
        self.size = self.size.saturating_sub(block.size);
        huge_unmap(block.at, block.size);
    }
}

impl Mm {
    /// `tracked_malloc` (`Z/Zend/zend_alloc.c:3335`): the limit asked for
    /// the bytes, with no sweep, then the system's block, the two usages
    /// moved by exactly the bytes asked and the peaks left where they
    /// stand, as php's `ZEND_MM_STAT` arm leaves them.
    #[cold]
    #[inline(never)]
    fn tracked_malloc(&mut self, size: usize) -> Result<NonNull<u8>, Refused> {
        self.check_limit(size, false)?;
        let Allocator::Tracked(tracked) = &mut self.allocator else {
            return Err(Refused::Fatal(Fatal::error("zend_mm_heap corrupted")));
        };
        let Some(at) = tracked.malloc(size) else {
            return Err(Refused::Fatal(system_out_of_memory()));
        };
        self.size += size;
        self.real_size = self.size;
        Ok(at)
    }

    /// `tracked_realloc` (`Z/Zend/zend_alloc.c:3374`): a grow asks the
    /// limit for the difference, its refusal leaving the block as it
    /// stood; then the system's `realloc`, and the usages moved by the
    /// difference.
    ///
    /// # Safety
    ///
    /// `at` is a live block of this heap that nothing reads or writes
    /// during the call.
    #[cold]
    #[inline(never)]
    unsafe fn tracked_realloc(
        &mut self,
        at: NonNull<u8>,
        size: usize,
    ) -> Result<NonNull<u8>, Refused> {
        let old_size = match &self.allocator {
            Allocator::Tracked(tracked) => tracked.size_of(at),
            Allocator::Zend => None,
        };
        let Some(old_size) = old_size else {
            return Err(Refused::Fatal(Fatal::error("zend_mm_heap corrupted")));
        };
        if size > old_size {
            self.check_limit(size - old_size, false)?;
        }
        let Allocator::Tracked(tracked) = &mut self.allocator else {
            return Err(Refused::Fatal(Fatal::error("zend_mm_heap corrupted")));
        };
        // SAFETY: the caller's contract: `at` is a live block of this
        // heap's table and nothing touches it during the call.
        let Some(to) = (unsafe { tracked.realloc(at, size) }) else {
            return Err(Refused::Fatal(system_out_of_memory()));
        };
        self.size = self.size - old_size + size;
        self.real_size = self.size;
        Ok(to)
    }

    /// `tracked_free` (`Z/Zend/zend_alloc.c:3359`): the block back to the
    /// system and the usages down by the bytes it was asked for.
    ///
    /// # Safety
    ///
    /// `at` is a live block of this heap that nothing names after the
    /// call.
    #[cold]
    #[inline(never)]
    unsafe fn tracked_free(&mut self, at: NonNull<u8>) {
        let Allocator::Tracked(tracked) = &mut self.allocator else {
            return;
        };
        // SAFETY: the caller's contract, handed on.
        let freed = unsafe { tracked.free(at) };
        // php asserts the block is the table's (`tracked_get_size_zv`); a
        // debug build names the free of one it is not, a second free
        // among them.
        let Some(size) = freed else {
            #[cfg(debug_assertions)]
            self.sensed.record(crate::sensed::Sensed::FreedTwice {
                at: at.as_ptr().addr(),
                size: 0,
            });
            return;
        };
        self.size -= size;
        self.real_size = self.size;
    }
}

/// `zend_out_of_memory` (`Z/Zend/zend_alloc.c:3279`): the system's
/// allocator had nothing to give a tracked heap.
#[cold]
fn system_out_of_memory() -> Fatal {
    Fatal::error("Out of memory")
}

impl std::fmt::Debug for Mm {
    /// What php's `memory_get_usage()` answers, and the chunks behind
    /// it; the addresses are the machine's and say nothing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mm")
            .field("tracked", &matches!(self.allocator, Allocator::Tracked(_)))
            .field("size", &self.size)
            .field("real_size", &self.real_size)
            .field("peak", &self.peak)
            .field("real_peak", &self.real_peak)
            .field("chunks", &self.chunks.len())
            .field("cached", &self.cached.len())
            .field("huge", &self.huge.len())
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl Drop for Mm {
    /// Mass release at request end (design §5.13), so nothing a request
    /// allocated outlives it: every huge block is unmapped, and where the
    /// heap has a cache what `zend_mm_shutdown(full=0)` keeps of it is
    /// kept there for the next heap ([`Mm::keep`]); every chunk is
    /// unmapped where it has none.
    fn drop(&mut self) {
        for block in self.huge.drain(..) {
            huge_unmap(block.at, block.size);
        }
        #[cfg(debug_assertions)]
        for chunk in &self.chunks {
            origins::give_back(*chunk);
        }
        if let Some(cache) = self.cache.take() {
            self.keep(&cache);
            return;
        }
        for chunk in self.chunks.drain(..).chain(self.cached.drain(..)) {
            chunk_free(chunk, CHUNK_SIZE);
        }
    }
}

impl Mm {
    /// `zend_mm_shutdown(heap, full=0, silent)` (`Z/Zend/zend_alloc.c:2434`)
    /// past its huge blocks: every chunk but the main one moved into the
    /// cached chunks, each to the head, the average taken with the
    /// request's peak, the cached chunks past what it keeps unmapped from
    /// the head, and what is left kept in `cache` for the next heap.
    fn keep(&mut self, cache: &ChunkCache) {
        let mut chunks = self.chunks.drain(..);
        let Some(main) = chunks.next() else {
            for chunk in self.cached.drain(..) {
                chunk_free(chunk, CHUNK_SIZE);
            }
            return;
        };
        self.cached.extend(chunks);
        #[allow(clippy::cast_precision_loss)] // a count of chunks
        let peak = self.peak_chunks_count as f64;
        let avg_chunks_count = f64::midpoint(self.avg_chunks_count, peak);
        #[allow(clippy::cast_precision_loss)]
        while self.cached.len() as f64 + 0.9 > avg_chunks_count {
            let Some(chunk) = self.cached.pop() else {
                break;
            };
            chunk_free(chunk, CHUNK_SIZE);
        }
        // php clears a cached chunk's header, `memset(p, 0,
        // sizeof(zend_mm_chunk))`; its `num` is the one field read before
        // `chunk_init` lays it down again.
        for chunk in &self.cached {
            // SAFETY: a cached chunk is a mapping of this heap's that no
            // list names, so nothing else reads or writes its header.
            unsafe { (*chunk.as_ptr()).num = 0 };
        }
        cache.keep(cache::Kept {
            main,
            cached: std::mem::take(&mut self.cached),
            avg_chunks_count,
        });
    }
}

/// `zend_mm_small_size_to_bin` (`Z/Zend/zend_alloc.c:1242`), php's
/// branchless form: the bins are eight apart to 64 and then four to a
/// power of two.
fn size_to_bin(size: usize) -> usize {
    if size <= 64 {
        return (size - usize::from(size != 0)) >> 3;
    }
    let t1 = size - 1;
    // `zend_mm_small_size_to_bit`: the place of the highest set bit.
    let t2 = (usize::BITS - t1.leading_zeros()) as usize - 3;
    (t1 >> t2) + ((t2 - 3) << 2)
}

impl Mm {
    /// What a debug build's sensors found in this heap since the engine
    /// last asked, taken ([`crate::sensed`]).
    #[cfg(debug_assertions)]
    pub(crate) fn take_sensed(&mut self) -> Option<crate::Fatal> {
        self.sensed.take()
    }

    /// Whether a fatal is leaving the request ([`crate::sensed::Record`]).
    #[cfg(debug_assertions)]
    pub(crate) fn fatal_leaving(&self, leaving: bool) {
        self.sensed.fatal_leaving(leaving);
    }

    /// `chunk` laid down as one of this heap's ([`chunk_init`]), its
    /// header naming this heap's sensed record in a debug build; a
    /// release build's header names nothing of the heap.
    #[cfg_attr(
        not(debug_assertions),
        expect(
            clippy::unused_self,
            reason = "only a debug build's chunk header names its heap's sensed record"
        )
    )]
    fn chunk_init(&self, chunk: NonNull<Chunk>, num: u32) {
        #[cfg(debug_assertions)]
        chunk_init(chunk, num, self.sensed.record_at(), origins::table());
        #[cfg(not(debug_assertions))]
        chunk_init(chunk, num);
    }
}

/// `zend_mm_chunk_init` (`Z/Zend/zend_alloc.c:870`): page zero is the
/// header and every other page is free.
fn chunk_init(
    chunk: NonNull<Chunk>,
    num: u32,
    #[cfg(debug_assertions)] sensed: *const crate::sensed::Record,
    #[cfg(debug_assertions)] origins: Option<NonNull<crate::sensed::Origin>>,
) {
    let mut header = Chunk {
        free_pages: PAGES - FIRST_PAGE,
        free_tail: FIRST_PAGE,
        num,
        free_map: [0; PAGE_MAP_LEN],
        map: [0; PAGES as usize],
        #[cfg(debug_assertions)]
        sensed,
        #[cfg(debug_assertions)]
        origins,
    };
    header.free_map[0] = (1 << FIRST_PAGE) - 1;
    header.map[0] = IS_LRUN | FIRST_PAGE;
    // SAFETY: the chunk is a mapping of `CHUNK_SIZE` bytes this heap owns
    // and nothing has been carved out of it, so page zero is free to take
    // the header.
    unsafe { chunk.as_ptr().write(header) };
}

/// The chunk an allocation belongs to: its address with the chunk's own
/// alignment masked off, which is what php's `ZEND_MM_ALIGNED_BASE` does
/// and why chunks are aligned to their size.
fn chunk_of(at: NonNull<u8>) -> NonNull<Chunk> {
    // The pointer keeps the provenance of the mapping it came from, which
    // covers the whole chunk, so the header it lands on is in range.
    at.map_addr(|addr| NonZeroUsize::new(addr.get() & !(CHUNK_SIZE - 1)).unwrap_or(addr))
        .cast()
}

/// The chunk and the first page of the bin `run` a free slot sits in:
/// php's step back over the `NRUN` pages that stand before the `run`'s
/// head (`Z/Zend/zend_alloc.c:2126-2136`).
fn run_of(slot: NonNull<FreeSlot>) -> (NonNull<Chunk>, usize) {
    let at = slot.cast::<u8>();
    let chunk = chunk_of(at);
    let page_num = (at.as_ptr().addr() & (CHUNK_SIZE - 1)) / PAGE_SIZE;
    // SAFETY: the slot is inside this chunk, so the chunk is one this
    // heap mapped and its header is initialised.
    let info = unsafe { &*chunk.as_ptr() }.map[page_num];
    if info & IS_LRUN == 0 {
        return (chunk, page_num);
    }
    let offset = ((info & NRUN_OFFSET_MASK) >> NRUN_OFFSET_SHIFT) as usize;
    (chunk, page_num - offset)
}

/// `ZEND_MM_SRUN_FREE_COUNTER`: the free slots the sweep has counted in
/// the `run` this page map word heads.
fn srun_free_counter(info: u32) -> u32 {
    (info & SRUN_FREE_COUNTER_MASK) >> SRUN_FREE_COUNTER_SHIFT
}

/// The mappings this process holds, which the tests read to see that a
/// heap that dropped left nothing behind.
#[cfg(test)]
pub(crate) static MAPPINGS: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// `zend_mm_chunk_alloc`: `size` bytes aligned to a chunk.
fn chunk_alloc(size: usize) -> Option<NonNull<Chunk>> {
    let layout = Layout::from_size_align(size, CHUNK_SIZE).ok()?;
    // SAFETY: the layout has a non-zero size, which is what `alloc` asks.
    let at = unsafe { alloc(layout) };
    let at = NonNull::new(at).map(NonNull::cast);
    #[cfg(test)]
    if at.is_some() {
        MAPPINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    at
}

/// `zend_mm_chunk_free`: `size` is what the mapping was asked for, a
/// whole chunk or the rounded size of one huge block.
fn chunk_free(chunk: NonNull<Chunk>, size: usize) {
    let Ok(layout) = Layout::from_size_align(size, CHUNK_SIZE) else {
        return;
    };
    #[cfg(test)]
    MAPPINGS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: the chunk came from `chunk_alloc` with this layout and has
    // left every list that named it.
    unsafe { dealloc(chunk.as_ptr().cast(), layout) };
}

/// `zend_mm_chunk_alloc_int(size, ZEND_MM_CHUNK_SIZE)`
/// (`Z/Zend/zend_alloc.c:752`) for a huge block: a mapping of its own,
/// as php's is, so that its tail can later be grown or cut where it
/// stands. One the machine hands back off a chunk boundary is given up
/// and asked for again a chunk wider, and the slack either side of the
/// aligned block unmapped, php's arithmetic exactly.
#[cfg(all(unix, not(miri)))]
fn huge_map(size: usize) -> Option<NonNull<u8>> {
    let at = mmap(std::ptr::null_mut(), size)?;
    let at = if at.as_ptr().addr() & (CHUNK_SIZE - 1) == 0 {
        at
    } else {
        munmap(at.as_ptr(), size);
        let page = real_page_size();
        let wide = mmap(std::ptr::null_mut(), size + CHUNK_SIZE - page)?;
        let mut alignment = CHUNK_SIZE;
        let mut offset = wide.as_ptr().addr() & (CHUNK_SIZE - 1);
        let mut at = wide.as_ptr();
        if offset != 0 {
            offset = CHUNK_SIZE - offset;
            munmap(at, offset);
            at = at.wrapping_add(offset);
            alignment -= offset;
        }
        if alignment > page {
            munmap(at.wrapping_add(size), alignment - page);
        }
        NonNull::new(at)?
    };
    #[cfg(test)]
    MAPPINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(at)
}

/// A platform with no `mmap`, and Miri, whose `munmap` takes a mapping
/// only whole (the note at the top of this file), keep the huge block on the system
/// allocator, aligned to a chunk, and never grow or cut it in place.
#[cfg(any(not(unix), miri))]
fn huge_map(size: usize) -> Option<NonNull<u8>> {
    chunk_alloc(size).map(NonNull::cast)
}

/// `zend_mm_chunk_free` over a huge block: its whole mapping goes back.
#[cfg(all(unix, not(miri)))]
fn huge_unmap(at: NonNull<u8>, size: usize) {
    #[cfg(test)]
    MAPPINGS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    munmap(at.as_ptr(), size);
}

/// The system allocator's block goes back, as [`chunk_free`] does.
#[cfg(any(not(unix), miri))]
fn huge_unmap(at: NonNull<u8>, size: usize) {
    chunk_free(at.cast(), size);
}

/// `zend_mm_chunk_extend` (`Z/Zend/zend_alloc.c:843`): the pages after a
/// huge block asked for where they stand, `mremap` without
/// `MREMAP_MAYMOVE` where the platform has it, and elsewhere
/// `zend_mm_mmap_fixed`, a mapping hinted at the block's end and given
/// back when the machine put it anywhere else. `false` is the machine
/// saying no, and the caller moves the block as php does.
#[cfg(all(target_os = "linux", not(miri)))]
fn huge_extend(at: NonNull<u8>, old_size: usize, new_size: usize) -> bool {
    // SAFETY: `at` is a mapping of `old_size` bytes this heap made, and
    // without `MREMAP_MAYMOVE` the call grows it in place or fails.
    let grown = unsafe { libc::mremap(at.as_ptr().cast(), old_size, new_size, 0) };
    grown != libc::MAP_FAILED
}

/// `zend_mm_mmap_fixed((char*)addr + old_size, new_size - old_size)`, the
/// arm of `zend_mm_chunk_extend` for a platform with no `mremap`.
#[cfg(all(unix, not(target_os = "linux"), not(miri)))]
fn huge_extend(at: NonNull<u8>, old_size: usize, new_size: usize) -> bool {
    let end = at.as_ptr().wrapping_add(old_size);
    let Some(tail) = mmap(end, new_size - old_size) else {
        return false;
    };
    if tail.as_ptr() == end {
        return true;
    }
    munmap(tail.as_ptr(), new_size - old_size);
    false
}

/// No platform mapping to grow: the block moves.
#[cfg(any(not(unix), miri))]
fn huge_extend(_at: NonNull<u8>, _old_size: usize, _new_size: usize) -> bool {
    false
}

/// `zend_mm_chunk_truncate` (`Z/Zend/zend_alloc.c:822`): the tail past
/// `new_size` unmapped, the block standing where it is.
#[cfg(all(unix, not(miri)))]
fn huge_truncate(at: NonNull<u8>, old_size: usize, new_size: usize) -> bool {
    munmap(at.as_ptr().wrapping_add(new_size), old_size - new_size);
    true
}

/// No platform mapping to cut: the block moves.
#[cfg(any(not(unix), miri))]
fn huge_truncate(_at: NonNull<u8>, _old_size: usize, _new_size: usize) -> bool {
    false
}

/// `zend_mm_mmap` (`Z/Zend/zend_alloc.c:522`), with `hint` php's
/// `zend_mm_mmap_fixed` address or null: `size` fresh, private,
/// anonymous bytes, or `None` where the machine refuses them.
#[cfg(all(unix, not(miri)))]
fn mmap(hint: *mut u8, size: usize) -> Option<NonNull<u8>> {
    // SAFETY: an anonymous private mapping names no file and, without
    // `MAP_FIXED`, replaces nothing already mapped; `hint` is a hint.
    let at = unsafe {
        libc::mmap(
            hint.cast(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return None;
    }
    NonNull::new(at.cast())
}

/// `zend_mm_munmap`: `size` bytes at `at`, which this heap mapped and
/// nothing names any more, given back to the machine.
#[cfg(all(unix, not(miri)))]
fn munmap(at: *mut u8, size: usize) {
    // SAFETY: the range is part of a mapping `mmap` made for this heap,
    // and the caller holds no name into it.
    unsafe { libc::munmap(at.cast(), size) };
}

/// php's `heap->fallible`: how the heap answers an ask past the soft
/// limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asks {
    /// Served, the delayed OOM recorded, and held to the hard limit.
    Infallibly,
    /// Refused, php's `NULL`, with the delayed OOM recorded.
    Fallibly,
}

impl Asks {
    /// php's `bool`.
    fn of(fallible: bool) -> Asks {
        if fallible {
            Asks::Fallibly
        } else {
            Asks::Infallibly
        }
    }
}

/// Why the allocator handed out no block.
#[derive(Debug)]
pub(crate) enum Refused {
    /// A fallible ask past the soft limit, php's `NULL` from `_femalloc`,
    /// with the delayed OOM left behind.
    Fallible,
    /// What ends the request: the hard limit, the system's own refusal,
    /// or a size past the top of memory.
    Fatal(Fatal),
}

impl Refused {
    /// A fallible ask's answer: the block, `None` where the soft limit
    /// refused it, or the request's end.
    #[cfg(test)]
    fn fallible<T>(answered: Result<T, Refused>) -> Result<Option<T>, Fatal> {
        match answered {
            Ok(at) => Ok(Some(at)),
            Err(Refused::Fallible) => Ok(None),
            Err(Refused::Fatal(fatal)) => Err(fatal),
        }
    }
}

impl From<Fatal> for Refused {
    fn from(fatal: Fatal) -> Refused {
        Refused::Fatal(fatal)
    }
}

/// php's exact text when the system cannot give the mapping
/// (`Z/Zend/zend_alloc.c:1063`).
fn out_of_memory(allocated: usize, size: usize) -> Fatal {
    Fatal::error(format!(
        "Out of memory (allocated {allocated} bytes) (tried to allocate {size} bytes)"
    ))
}

/// php's best-fit search over one chunk's free map
/// (`Z/Zend/zend_alloc.c:967-1030`): the shortest free `run` that fits, and
/// the free tail corrected on the way past it.
fn best_fit(chunk: &mut Chunk, pages_count: u32) -> Option<u32> {
    let free_tail = chunk.free_tail;
    let mut best = None;
    let mut best_len = PAGES;
    let mut word = 0;
    let mut tmp = chunk.free_map[0];
    let mut i = 0;
    word += 1;
    loop {
        // Skip the words whose every page is taken.
        while tmp == u64::MAX {
            i += 64;
            if i == PAGES {
                return best;
            }
            tmp = chunk.free_map[word];
            word += 1;
        }
        let page_num = i + (!tmp).trailing_zeros();
        tmp &= tmp.wrapping_add(1);
        // Skip the words whose every page is free.
        while tmp == 0 {
            i += 64;
            if i >= free_tail || i == PAGES {
                let len = PAGES - page_num;
                if len >= pages_count && len < best_len {
                    chunk.free_tail = page_num + pages_count;
                    return Some(page_num);
                }
                chunk.free_tail = page_num;
                return best;
            }
            tmp = chunk.free_map[word];
            word += 1;
        }
        let len = i + tmp.trailing_zeros() - page_num;
        if len >= pages_count {
            if len == pages_count {
                return Some(page_num);
            }
            if len < best_len {
                best_len = len;
                best = Some(page_num);
            }
        }
        tmp |= tmp.wrapping_sub(1);
    }
}

/// `zend_mm_bitset_is_set`: the bit stands while the page is taken.
fn bitset_is_set(map: &[u64; PAGE_MAP_LEN], page: u32) -> bool {
    map[page as usize / 64] & (1 << (page % 64)) != 0
}

/// `zend_mm_bitset_set_range`.
fn bitset_set_range(map: &mut [u64; PAGE_MAP_LEN], start: u32, len: u32) {
    for page in start..start + len {
        map[page as usize / 64] |= 1 << (page % 64);
    }
}

/// `zend_mm_bitset_reset_range`.
fn bitset_reset_range(map: &mut [u64; PAGE_MAP_LEN], start: u32, len: u32) {
    for page in start..start + len {
        map[page as usize / 64] &= !(1 << (page % 64));
    }
}

/// `zend_mm_bitset_is_free_range` (`Z/Zend/zend_alloc.c:689`): no page
/// of the range is taken.
fn bitset_is_free_range(map: &[u64; PAGE_MAP_LEN], start: u32, len: u32) -> bool {
    (start..start + len).all(|page| !bitset_is_set(map, page))
}

/// The `memcpy` of a realloc: the first `len` bytes of `from` into `to`.
///
/// # Safety
///
/// `from` holds `len` readable bytes, `to` holds `len` writable ones, and
/// the two do not overlap.
unsafe fn copy_into(from: NonNull<u8>, to: NonNull<u8>, len: usize) {
    // SAFETY: the caller's contract, handed on.
    unsafe { std::ptr::copy_nonoverlapping(from.as_ptr(), to.as_ptr(), len) };
}

/// The freed slots a debug build holds back per bin (#1991).
#[cfg(debug_assertions)]
pub(crate) const QUARANTINE_SLOTS: usize = 16;

/// A debug build's quarantine of one bin (#1991): the bin's last
/// [`QUARANTINE_SLOTS`] freed slots, oldest first, held back from the
/// free list. php's free list is last in, first out, so the slot a block
/// was just freed from is the next one its bin hands out, and the handing
/// out clears the mark the freed-use guard reads ([`freed`]) before a
/// stale use of the block can meet it: the guard would find the
/// commonest use-after-free, a value used after its block is given back
/// and one of its size made, only when nothing of that size was made in
/// between. Held back, the slot keeps its mark until its bin's ring is
/// full and pushes it out, and a stale use of it is found the first time.
///
/// A slot leaves by eviction, onto the head of the free list, after its
/// mark and the poison in its first word are found as they were laid
/// ([`freed::untouched`]); one written over is recorded as the
/// `InternalError` naming the block. It leaves when the ring is full and
/// another slot of its bin is freed, and at the start of the heap's GC
/// (`zend_mm_gc`), the sweep that runs before the limit refuses anything,
/// so the sweep sees every free slot php's would. A bin whose free list is
/// empty carves a new `run` rather than hand out a held slot, so a debug
/// build can hold a few more pages than php's; within a chunk of the
/// limit it hands out its oldest held slot instead, as php's would hand
/// out its newest, so the limit refuses where php's does
/// ([`Mm::limit_is_near`]). `memory_get_usage()` counts a slot freed when
/// it enters, as php counts it freed in `zend_mm_free_small`. A release
/// build has none of it.
#[cfg(debug_assertions)]
#[derive(Clone, Copy)]
struct Quarantine {
    /// A ring of the held slots: [`Quarantine::len`] of them, from
    /// [`Quarantine::oldest`] on, are `Some`.
    slots: [Option<NonNull<FreeSlot>>; QUARANTINE_SLOTS],
    oldest: usize,
    len: usize,
}

#[cfg(debug_assertions)]
impl Quarantine {
    const EMPTY: Quarantine = Quarantine {
        slots: [None; QUARANTINE_SLOTS],
        oldest: 0,
        len: 0,
    };

    /// Whether the ring holds `slot`.
    fn holds(&self, slot: NonNull<FreeSlot>) -> bool {
        (0..self.len).any(|i| self.slots[(self.oldest + i) % QUARANTINE_SLOTS] == Some(slot))
    }

    /// `slot` held as the newest, and the oldest given up where the ring
    /// was full.
    fn hold(&mut self, slot: NonNull<FreeSlot>) -> Option<NonNull<FreeSlot>> {
        let evicted = if self.len == QUARANTINE_SLOTS {
            self.take_oldest()
        } else {
            None
        };
        self.slots[(self.oldest + self.len) % QUARANTINE_SLOTS] = Some(slot);
        self.len += 1;
        evicted
    }

    /// The slot held longest, given up.
    fn take_oldest(&mut self) -> Option<NonNull<FreeSlot>> {
        if self.len == 0 {
            return None;
        }
        let slot = self.slots[self.oldest].take();
        self.oldest = (self.oldest + 1) % QUARANTINE_SLOTS;
        self.len -= 1;
        slot
    }

    /// The slot held shortest, given up.
    fn take_newest(&mut self) -> Option<NonNull<FreeSlot>> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        self.slots[(self.oldest + self.len) % QUARANTINE_SLOTS].take()
    }
}

#[cfg(debug_assertions)]
impl Mm {
    /// The slot at `slot` of bin `bin_num`, freed, poisoned and held in
    /// its bin's quarantine, and the slot it pushes out evicted.
    fn hold_back(&mut self, slot: NonNull<FreeSlot>, bin_num: usize) {
        // SAFETY: the slot is one of bin `bin_num`'s its owner has just
        // given up, which nothing else names.
        unsafe { freed::poison(slot.cast()) };
        if let Some(evicted) = self.quarantine[bin_num].hold(slot) {
            self.evict(evicted, bin_num);
        }
    }

    /// Whether one more chunk would take the heap past its limit, the
    /// test `zend_mm_alloc_pages` makes before it refuses
    /// (`Z/Zend/zend_alloc.c:1038`, [`Mm::grow`]): only there can a `run`
    /// a debug build carves where php's reused a slot end the request.
    fn limit_is_near(&self) -> bool {
        CHUNK_SIZE > self.limit - self.real_size.min(self.limit)
    }

    /// Bin `bin_num`'s oldest held slot evicted, where it holds one.
    fn evict_oldest(&mut self, bin_num: usize) {
        if let Some(slot) = self.quarantine[bin_num].take_oldest() {
            self.evict(slot, bin_num);
        }
    }

    /// Every held slot evicted, the newest of each bin first, so its free
    /// list hands them out oldest first.
    fn drain_quarantine(&mut self) {
        for bin_num in 0..BINS {
            while let Some(slot) = self.quarantine[bin_num].take_newest() {
                self.evict(slot, bin_num);
            }
        }
    }

    /// A slot out of bin `bin_num`'s quarantine checked and put on its
    /// free list: one whose mark or poison was written over is recorded.
    fn evict(&mut self, slot: NonNull<FreeSlot>, bin_num: usize) {
        let size = BIN_DATA_SIZE[bin_num];
        // SAFETY: the slot was freed into the quarantine, which held the
        // only name of it, and is a slot of `size` bytes.
        if !unsafe { freed::untouched(slot.cast(), size) } {
            self.sensed.record(crate::sensed::Sensed::FreedWrite {
                at: slot.as_ptr().addr(),
                size,
            });
        }
        // SAFETY: as above; the quarantine left it on the way here.
        unsafe { self.push_free(slot, bin_num) };
    }
}

/// A debug build's mark on a block the heap has freed (design §10), so
/// that a refcount operation on one is found where it is made and not
/// several allocations later, where a write into a freed bin slot's link
/// word surfaces (#723, #750). A release build compiles none of it.
///
/// The mark is two words, the second and third of the block, laid when a
/// bin slot or a `run` is given back and cleared when one is handed out,
/// so a block the heap has out never carries it. The first word of a bin
/// slot is the poison while the slot is in its bin's [`Quarantine`] and
/// the free list's link once it leaves. The pattern is no live value's:
/// where every refcounted value keeps a hash and a length, a flags word
/// and a pointer, a store index and a class number, a zval's payload and
/// its type byte, or a list index and a type number, the pair of odd
/// words below is none of them. A huge block goes back to the system and carries no
/// mark; nor does a slot of the 8- and 16-byte bins, which no refcounted
/// value is small enough for.
#[cfg(debug_assertions)]
pub(crate) mod freed {
    use std::ptr::NonNull;

    /// The two words a freed block carries.
    const MARK: [u64; 2] = [0xA5A5_A5A5_A5A5_A5A5; 2];

    /// Where the mark stands in a block: its second word, which every
    /// block of this heap is aligned for, being at least eight-aligned.
    fn mark_at(at: NonNull<u8>) -> *mut [u64; 2] {
        let mark: *mut [u64; 2] = at.as_ptr().wrapping_add(8).cast();
        mark
    }

    /// Where the mark stands in a block, when the block has room for it.
    fn mark_of(at: NonNull<u8>, size: usize) -> Option<*mut [u64; 2]> {
        (size >= 8 + size_of::<[u64; 2]>()).then(|| mark_at(at))
    }

    /// The block of `size` bytes at `at` is the heap's again.
    pub(crate) fn given_back(at: NonNull<u8>, size: usize) {
        if let Some(mark) = mark_of(at, size) {
            // SAFETY: the block is `size` bytes of this heap its owner has
            // given up, and the mark is inside it past the link word.
            unsafe { mark.write(MARK) };
        }
    }

    /// The first word of a slot in the quarantine, read as the header's
    /// two halves: its type word is zero, so a stale use reads the block
    /// as a counted, mutable value, as it read the free list's link, and
    /// meets the guard; its count is one no live value holds.
    const POISON: u64 = if cfg!(target_endian = "little") {
        0x0000_0000_A5A5_A5A5
    } else {
        0xA5A5_A5A5_0000_0000
    };

    /// The slot at `at` poisoned as it enters the quarantine.
    ///
    /// # Safety
    ///
    /// `at` is a bin slot of this heap its owner has given up.
    pub(crate) unsafe fn poison(at: NonNull<u8>) {
        let first: *mut u64 = at.as_ptr().cast();
        // SAFETY: the caller's contract; a bin slot is eight-aligned and
        // at least eight bytes.
        unsafe { first.write(POISON) };
    }

    /// Whether the slot of `size` bytes at `at` still carries the poison
    /// and the mark laid as it entered the quarantine.
    ///
    /// # Safety
    ///
    /// `at` is a bin slot of `size` bytes this heap's quarantine holds.
    pub(crate) unsafe fn untouched(at: NonNull<u8>, size: usize) -> bool {
        let first: *const u64 = at.as_ptr().cast();
        // SAFETY: the caller's contract: the first word is the slot's.
        let poisoned = unsafe { first.read() } == POISON;
        poisoned
            && mark_of(at, size).is_none_or(|mark| {
                // SAFETY: as above, and the mark is inside the slot.
                let mark = unsafe { mark.read() };
                mark == MARK
            })
    }

    /// The block of `size` bytes at `at` is its caller's from here.
    pub(crate) fn handed_out(at: NonNull<u8>, size: usize) {
        if let Some(mark) = mark_of(at, size) {
            // SAFETY: the block is `size` bytes of this heap nothing else
            // holds yet, and the mark is inside it.
            unsafe { mark.write([0; 2]) };
        }
    }

    /// Whether the refcounted block at `at` is one the heap has freed and
    /// not handed out since. Every refcounted value is at least 24 bytes,
    /// so the two words are inside it, and no header leaves padding in
    /// them, so a live block's are initialised. `at` is the block's own
    /// pointer, never one derived from a reference to its header, whose
    /// provenance ends at the header's eight bytes (#2414).
    pub(crate) fn is_freed(at: NonNull<u8>) -> bool {
        // A tracked heap's blocks are the system's, which carry no mark
        // and sit in no chunk ([`super::tracked`]).
        if super::tracked::chosen() {
            return false;
        }
        let mark = mark_at(at);
        // SAFETY: `at` names a refcounted value whose count is about to
        // move: a block of this heap, live or freed into a bin or a `run`
        // of a chunk still mapped, or a live persistent string out of the
        // process's allocator, so its second and third words are
        // readable; a block freed back to the system is the defect this
        // read exists to name, and reading it is the refcount
        // operation's own read, made one step earlier.
        unsafe { mark.read() == MARK }
    }

    /// `sensed` kept in the record of the heap the freed block at `at`
    /// belongs to, reached through the header of its chunk, php's
    /// `chunk->heap`. Only a block [`is_freed`] found marked is passed,
    /// and only a slot of a chunk carries the mark.
    pub(crate) fn record(at: NonNull<u8>, sensed: crate::sensed::Sensed) {
        with_record(at, |record| record.record(sensed));
    }

    /// `f` handed the record of the heap the block at `at` belongs to,
    /// reached through the header of its chunk, php's `chunk->heap`. `at`
    /// is a block of a chunk, never a huge block, which heads a mapping of
    /// its own, and the chunk is mapped: a block the caller holds an owner
    /// of, or one [`is_freed`] found marked, whose chunk its heap still
    /// holds, since a slot is marked only while its chunk is the heap's.
    pub(crate) fn with_record<R>(
        at: NonNull<u8>,
        f: impl FnOnce(&crate::sensed::Record) -> R,
    ) -> R {
        let chunk = super::chunk_of(at);
        // SAFETY: the caller's contract: the chunk is one its heap holds
        // mapped, whose page zero is the header `chunk_init` wrote.
        let record = unsafe { (*chunk.as_ptr()).sensed };
        // SAFETY: the header names the record its heap shares, which the
        // heap lets go of only after it has given every chunk back, so the
        // record stands while the chunk is mapped.
        f(unsafe { &*record })
    }
}
