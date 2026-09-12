//! The allocator (design §5.13): a copy of `zend_alloc`, the one
//! accountant of PHP-visible memory. 2 MiB chunks carved into 4 KiB
//! pages, the thirty small-size bins of `Zend/zend_alloc_sizes.h`, large
//! `runs` of whole pages, huge allocations of their own, per-request
//! ownership and mass release when the request's heap drops.
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
//! an address a mask away, and the header is zeroed on the way in.
//!
//! Before it declares the limit reached or the system out of memory, php
//! sweeps: `zend_mm_gc` (`Z/Zend/zend_alloc.c:2101`) counts the free slots
//! of every bin `run` into the page map word that holds that count,
//! gives back the `runs` whose slots are all free, and the caller asks
//! again. [`Mm::gc`] is that function, called where php calls it, so a
//! request that filled a bin and gave it back goes on exactly as far as
//! php's does.

// A page number, a page count and a bin number are all under 512 here and
// a chunk count is a handful: the narrowing casts below hold what php
// holds in a `uint32_t` for the same reason it does.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::alloc::{Layout, alloc, dealloc};
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use crate::hooks::Fatal;

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
/// `ZEND_MM_MAX_LARGE_SIZE`: above this an allocation is huge.
const MAX_LARGE_SIZE: usize = CHUNK_SIZE - PAGE_SIZE * FIRST_PAGE as usize;
/// php's default `memory_limit` before any ini is read
/// (`Z/Zend/zend_alloc.c:2085`).
pub(crate) const NO_LIMIT: usize = usize::MAX >> 1;

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

/// One huge allocation and the bytes it was rounded to.
struct Huge {
    at: NonNull<u8>,
    size: usize,
}

/// The per-request heap: its chunks, its thirty free lists, its huge
/// blocks and the four numbers php answers `memory_get_usage()` and
/// `memory_get_peak_usage()` with.
pub(crate) struct Mm {
    /// The chunks in php's walk order; the first is php's `main_chunk`
    /// and is never moved or deleted.
    chunks: Vec<NonNull<Chunk>>,
    /// `cached_chunks`, kept mapped against the next growth, newest
    /// first.
    cached: Vec<NonNull<Chunk>>,
    /// `free_slot`: the head of each bin's free list.
    free_slot: [*mut FreeSlot; BINS],
    /// `huge_list`.
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
}

impl Mm {
    /// A heap with php's main chunk mapped, as `zend_mm_init` leaves one
    /// (`Z/Zend/zend_alloc.c:2047`): one chunk of real size, nothing
    /// allocated in it, no limit until the ini sets one. A machine that
    /// cannot give the chunk leaves the heap empty and the next
    /// allocation asks again.
    pub(crate) fn new() -> Mm {
        let mut mm = Mm {
            chunks: Vec::new(),
            cached: Vec::new(),
            free_slot: [std::ptr::null_mut(); BINS],
            huge: Vec::new(),
            peak_chunks_count: 0,
            avg_chunks_count: 1.0,
            last_delete_boundary: 0,
            last_delete_count: 0,
            size: 0,
            peak: 0,
            real_size: 0,
            real_peak: 0,
            limit: NO_LIMIT,
        };
        if let Some(chunk) = chunk_alloc(CHUNK_SIZE) {
            chunk_init(chunk, 0);
            mm.chunks.push(chunk);
            mm.peak_chunks_count = 1;
            mm.real_size = CHUNK_SIZE;
            mm.real_peak = CHUNK_SIZE;
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

    /// The `memory_limit` this heap is held to.
    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    /// Set the `memory_limit`.
    pub(crate) fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    /// `_emalloc`: `size` usable bytes, from the bin its size falls in,
    /// from a `run` of pages, or from a huge block of its own.
    pub(crate) fn alloc(&mut self, size: usize) -> Result<NonNull<u8>, Fatal> {
        if size <= MAX_SMALL_SIZE {
            self.alloc_small(size_to_bin(size))
        } else if size <= MAX_LARGE_SIZE {
            self.alloc_large(size)
        } else {
            self.alloc_huge(size)
        }
    }

    /// `_efree`: give `at` back to the bin, `run` or huge block it came
    /// from. php reads what it is from the chunk's page map, and so does
    /// this, so a pointer carries no header of its own.
    ///
    /// # Safety
    ///
    /// `at` is a pointer this heap returned from [`Mm::alloc`] and has
    /// not been freed since.
    pub(crate) unsafe fn free(&mut self, at: NonNull<u8>) {
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
            self.free_pages(chunk, page_num as u32, pages_count, true);
        }
    }

    /// `zend_mm_alloc_small` (`Z/Zend/zend_alloc.c:1392`): the head of the
    /// bin's free list, or a fresh `run` when the list is empty.
    fn alloc_small(&mut self, bin_num: usize) -> Result<NonNull<u8>, Fatal> {
        self.size += BIN_DATA_SIZE[bin_num];
        self.peak = self.peak.max(self.size);
        if let Some(slot) = NonNull::new(self.free_slot[bin_num]) {
            // SAFETY: a slot on the free list is free memory of this
            // heap, whose first word is the next free slot.
            self.free_slot[bin_num] = unsafe { slot.as_ref() }.next;
            return Ok(slot.cast());
        }
        self.alloc_small_slow(bin_num)
    }

    /// `zend_mm_alloc_small_slow` (`Z/Zend/zend_alloc.c:1338`): a `run` of
    /// pages for this bin, threaded into a free list of its elements, and
    /// the first element returned. The accounting was done by the caller,
    /// as php's is, so a failure here undoes it.
    fn alloc_small_slow(&mut self, bin_num: usize) -> Result<NonNull<u8>, Fatal> {
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
    /// back on the head of its bin's free list.
    ///
    /// # Safety
    ///
    /// `at` is a live slot of bin `bin_num` in this heap.
    unsafe fn free_small(&mut self, at: NonNull<u8>, bin_num: usize) {
        self.size = self.size.saturating_sub(BIN_DATA_SIZE[bin_num]);
        let slot: *mut FreeSlot = at.as_ptr().cast();
        // SAFETY: the caller has given the slot up, so its bytes are the
        // heap's again and the link word may be written over them.
        unsafe {
            slot.write(FreeSlot {
                next: self.free_slot[bin_num],
            });
        };
        self.free_slot[bin_num] = slot;
    }

    /// `zend_mm_alloc_large` (`Z/Zend/zend_alloc.c:1114`): whole pages,
    /// and the accounting is the pages and not the bytes asked for.
    fn alloc_large(&mut self, size: usize) -> Result<NonNull<u8>, Fatal> {
        let pages_count = size.div_ceil(PAGE_SIZE) as u32;
        let at = self.alloc_pages(pages_count, size)?;
        self.size += pages_count as usize * PAGE_SIZE;
        self.peak = self.peak.max(self.size);
        Ok(at)
    }

    /// `zend_mm_alloc_pages` (`Z/Zend/zend_alloc.c:908`): the best-fit `run`
    /// of `pages_count` free pages in the first chunk that has one, else a
    /// chunk from the cache, else a new chunk. `asked` is the number php
    /// names in the limit's message at this site.
    fn alloc_pages(&mut self, pages_count: u32, asked: usize) -> Result<NonNull<u8>, Fatal> {
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
    /// `memory_limit` (`Z/Zend/zend_alloc.c:1038-1076`). Both ways of
    /// failing here sweep first: php's `goto get_chunk` is the loop, so a
    /// sweep that emptied a chunk into the cache is taken from the cache
    /// on the way round and the limit is never reached.
    fn grow(&mut self, asked: usize) -> Result<NonNull<Chunk>, Fatal> {
        let chunk = loop {
            if let Some(chunk) = self.cached.pop() {
                break chunk;
            }
            if CHUNK_SIZE > self.limit - self.real_size.min(self.limit) {
                if self.gc() != 0 {
                    continue;
                }
                return Err(limit_exhausted(self.limit, asked));
            }
            let mapped = match chunk_alloc(CHUNK_SIZE) {
                Some(chunk) => Some(chunk),
                None if self.gc() != 0 => chunk_alloc(CHUNK_SIZE),
                None => None,
            };
            let Some(chunk) = mapped else {
                return Err(out_of_memory(self.real_size, asked));
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
        chunk_init(chunk, num);
        self.chunks.push(chunk);
        self.peak_chunks_count = self.peak_chunks_count.max(self.chunks.len());
        Ok(chunk)
    }

    /// `zend_mm_free_pages_ex` (`Z/Zend/zend_alloc.c:1178`): the `run` goes
    /// back to the chunk, and with `free_chunk` a chunk that is wholly
    /// free and is not the main one leaves the list. The sweep passes
    /// `false`, because it deletes the chunk itself once it has walked
    /// the whole of it.
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

    /// `zend_mm_delete_chunk` (`Z/Zend/zend_alloc.c:1140`): a chunk that
    /// emptied is kept mapped while the request has been using about that
    /// many, and given back otherwise.
    fn delete_chunk(&mut self, chunk: NonNull<Chunk>) {
        let Some(at) = self.chunks.iter().position(|c| *c == chunk) else {
            return;
        };
        self.chunks.remove(at);
        let count = self.chunks.len();
        // php compares the chunks a request holds against a running
        // average of what it has been holding; a heap here serves one
        // request and is dropped, so the average stays the 1.0 that
        // `zend_mm_init` leaves.
        let held = (count + self.cached.len()) as f64;
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
    /// starts from zero.
    fn gc(&mut self) -> usize {
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

    /// `zend_mm_alloc_huge` (`Z/Zend/zend_alloc.c:1916`): a mapping of its
    /// own, rounded up to a page, counted in both the bin accounting and
    /// the real size.
    fn alloc_huge(&mut self, size: usize) -> Result<NonNull<u8>, Fatal> {
        let new_size = size.next_multiple_of(PAGE_SIZE);
        if new_size > self.limit - self.real_size.min(self.limit)
            && (self.gc() == 0 || new_size > self.limit - self.real_size.min(self.limit))
        {
            return Err(limit_exhausted(self.limit, size));
        }
        let mapped = match chunk_alloc(new_size) {
            Some(at) => Some(at),
            None if self.gc() != 0 => chunk_alloc(new_size),
            None => None,
        };
        let Some(at) = mapped else {
            return Err(out_of_memory(self.real_size, size));
        };
        let at = at.cast::<u8>();
        self.huge.push(Huge { at, size: new_size });
        self.real_size += new_size;
        self.real_peak = self.real_peak.max(self.real_size);
        self.size += new_size;
        self.peak = self.peak.max(self.size);
        Ok(at)
    }

    /// `zend_mm_free_huge` (`Z/Zend/zend_alloc.c:1988`).
    fn free_huge(&mut self, at: NonNull<u8>) {
        let Some(index) = self.huge.iter().position(|block| block.at == at) else {
            return;
        };
        let block = self.huge.swap_remove(index);
        self.real_size = self.real_size.saturating_sub(block.size);
        self.size = self.size.saturating_sub(block.size);
        chunk_free(block.at.cast(), block.size);
    }
}

impl std::fmt::Debug for Mm {
    /// What php's `memory_get_usage()` answers, and the chunks behind
    /// it; the addresses are the machine's and say nothing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mm")
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
    /// Mass release at request end (design §5.13): every chunk, cached
    /// chunk and huge block this request mapped is given back, so nothing
    /// a request allocated outlives it.
    fn drop(&mut self) {
        for chunk in self.chunks.drain(..).chain(self.cached.drain(..)) {
            chunk_free(chunk, CHUNK_SIZE);
        }
        for block in self.huge.drain(..) {
            chunk_free(block.at.cast(), block.size);
        }
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

/// `zend_mm_chunk_init` (`Z/Zend/zend_alloc.c:870`): page zero is the
/// header and every other page is free.
fn chunk_init(chunk: NonNull<Chunk>, num: u32) {
    let mut header = Chunk {
        free_pages: PAGES - FIRST_PAGE,
        free_tail: FIRST_PAGE,
        num,
        free_map: [0; PAGE_MAP_LEN],
        map: [0; PAGES as usize],
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

/// php's exact text when a chunk would take the request past its
/// `memory_limit` (`Z/Zend/zend_alloc.c:1045`).
fn limit_exhausted(limit: usize, size: usize) -> Fatal {
    Fatal::Error(format!(
        "Allowed memory size of {limit} bytes exhausted (tried to allocate {size} bytes)"
    ))
}

/// php's exact text when the system cannot give the mapping
/// (`Z/Zend/zend_alloc.c:1063`).
fn out_of_memory(allocated: usize, size: usize) -> Fatal {
    Fatal::Error(format!(
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
