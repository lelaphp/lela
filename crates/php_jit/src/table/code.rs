//! The code arena (`jit-design.md` §4.4), sealed by construction: one
//! span of pages seen through a write view and an executable view, carved
//! into 64 KiB chunks, into which [`Arena::publish`] copies a body once.
//! Bytes are final: a [`Code`] has no way to be written again, and the
//! chunk it lives in returns to the arena when the last `Code` in it is
//! dropped, so a later body may reuse it.
//!
//! [`Pages`] is the seam under the arena, the one span and the cache
//! maintenance between its two views, with two adapters: [`HeapPages`],
//! the test adapter, one allocation of `UnsafeCell<u8>` with nothing to
//! maintain, which tests the arena's bookkeeping under Miri; and
//! `MapJit`, macOS on aarch64, one `MAP_JIT` anonymous mapping whose two
//! views are one address and the per-thread `pthread_jit_write_protect_np`
//! toggle around each write, with `sys_icache_invalidate` over the bytes
//! written. Linux's `memfd` adapter, two mappings of one sealed file,
//! joins when the Linux targets are measured; until then [`Unmapped`]
//! maps nothing there, and a table over it compiles nothing.
//!
//! Beside the arena, the three calls the `PERF_DUMP` record needs that
//! only `unsafe` reaches: the jitdump's one mapping, the monotonic clock
//! its records are stamped with, and the thread that wrote them.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::io;
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The size of one chunk, the unit the arena hands out and takes back.
pub(crate) const CHUNK: usize = 64 * 1024;

/// One span of pages seen as a write view and an executable view
/// (`jit-design.md` §4.4): the internal seam the arena is built over.
pub(crate) trait Pages: Sized + Send + Sync {
    /// Maps one span of `len` bytes, both views of it.
    fn map_pair(len: usize) -> io::Result<Self>;

    /// Copies `bytes` to offset `at` through the write view.
    ///
    /// # Safety
    ///
    /// `at..at + bytes.len()` lies inside the mapping, and no thread
    /// executes or reads those bytes while the copy runs: the arena hands
    /// a range to one write before any `Code` names it.
    unsafe fn write(&self, at: usize, bytes: &[u8]);

    /// Makes what [`write`](Pages::write) put at `range` what the next
    /// instruction fetch of any thread at those executable addresses sees.
    ///
    /// # Safety
    ///
    /// `range` lies inside the mapping.
    unsafe fn sync(&self, range: Range<usize>);

    /// The executable view's address of offset `at`, which lies inside the
    /// mapping; an address only, which reads and writes nothing.
    fn rx(&self, at: usize) -> NonNull<u8>;
}

/// Why a body was not published: it is larger than a chunk, or every
/// chunk holds live code (php's `TOO_MANY_TRACES` for a full buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Full;

/// The mapping and which of its chunks are free, shared by the arena and
/// every chunk handed out, so the pages outlive the last `Code`.
struct Region<P> {
    pages: P,
    /// One atomic a chunk: true while the chunk is neither filling nor
    /// holding a live `Code`.
    free: Box<[AtomicBool]>,
}

/// Which chunk of its arena a chunk is: made only by the arena, from the
/// position of a chunk it has, so it always names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkIdx(usize);

impl ChunkIdx {
    /// The offset of the chunk's first byte in the mapping.
    fn start(self) -> usize {
        self.0 * CHUNK
    }
}

/// One chunk handed out: while any `Code` in it lives, or the arena is
/// filling it, nobody else is given it.
pub(crate) struct Chunk<P: Pages> {
    region: Arc<Region<P>>,
    index: ChunkIdx,
}

impl<P: Pages> Drop for Chunk<P> {
    /// The last holder gone: no thread executes in the chunk any more, so
    /// the arena may take it again; the release pairs with the acquire
    /// that takes it.
    fn drop(&mut self) {
        self.region.free[self.index.0].store(true, Ordering::Release);
    }
}

/// One published body (`jit-design.md` §4.4): its entry in the executable
/// view, and the chunk it keeps from reuse.
pub(crate) struct Code<P: Pages> {
    entry: NonNull<u8>,
    len: usize,
    chunk: Arc<Chunk<P>>,
}

impl<P: Pages> Code<P> {
    /// The address of the body's first byte in the executable view; the
    /// `Entry` type it is called as comes with `enter`'s trampoline.
    pub(crate) fn entry(&self) -> NonNull<u8> {
        self.entry
    }

    /// How many bytes the body is.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Which chunk of the arena the body is in.
    pub(crate) fn chunk(&self) -> ChunkIdx {
        self.chunk.index
    }
}

// SAFETY: a `Code` names bytes no one writes again until every `Code` in
// their chunk is dropped, and its chunk's `Arc` is itself `Send`.
unsafe impl<P: Pages> Send for Code<P> {}
// SAFETY: the same, and `&Code` hands out only the address and two counts.
unsafe impl<P: Pages> Sync for Code<P> {}

/// The chunk being filled, and where its next body may start.
struct Filling<P: Pages> {
    chunk: Arc<Chunk<P>>,
    cursor: usize,
}

/// The arena (`jit-design.md` §4.4): published into by the one thread
/// holding it mutably, under the table's one mutex in the tier, and executed
/// from any.
pub(crate) struct Arena<P: Pages> {
    region: Arc<Region<P>>,
    filling: Option<Filling<P>>,
}

impl<P: Pages> Arena<P> {
    /// An arena of `capacity` bytes, a whole number of chunks and at
    /// least one, mapped through `P`.
    pub(crate) fn new(capacity: usize) -> io::Result<Arena<P>> {
        if capacity == 0 || !capacity.is_multiple_of(CHUNK) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a code arena is a whole number of {CHUNK}-byte chunks, not {capacity} bytes"
                ),
            ));
        }
        let pages = P::map_pair(capacity)?;
        let free = (0..capacity / CHUNK)
            .map(|_| AtomicBool::new(true))
            .collect();
        Ok(Arena {
            region: Arc::new(Region { pages, free }),
            filling: None,
        })
    }

    /// Copies `bytes` into a chunk through the write view at an executable
    /// address aligned to `align`, a power of two (any other is `Full`, a
    /// body no chunk fits), performs the cache
    /// maintenance the target needs, and returns its `Code`. The chunk
    /// being filled takes it if it fits; otherwise the arena lets that
    /// chunk go, its tail unused, and takes a free one, which may be the
    /// same chunk when no `Code` in it lives.
    pub(crate) fn publish(&mut self, bytes: &[u8], align: usize) -> Result<Code<P>, Full> {
        if !align.is_power_of_two() || bytes.len() > CHUNK {
            return Err(Full);
        }
        // The filling chunk is let go first when the body does not fit it:
        // holding no live `Code`, it is free again, and may be the one taken.
        let fit = (self.filling.take()).and_then(|filling| {
            let at = self.fitted(&filling, bytes.len(), align)?;
            Some((filling, at))
        });
        let (mut filling, at) = if let Some(fit) = fit {
            fit
        } else {
            let filling = self.taken()?;
            let at = self.fitted(&filling, bytes.len(), align).ok_or(Full)?;
            (filling, at)
        };
        let start = filling.chunk.index.start() + at;
        // SAFETY: `start..start + len` is inside this arena's chunk, past
        // its cursor, so no `Code` names it and no thread executes it.
        unsafe { self.region.pages.write(start, bytes) };
        // SAFETY: the same range, inside the mapping.
        unsafe { self.region.pages.sync(start..start + bytes.len()) };
        filling.cursor = at + bytes.len();
        let code = Code {
            entry: self.region.pages.rx(start),
            len: bytes.len(),
            chunk: Arc::clone(&filling.chunk),
        };
        self.filling = Some(filling);
        Ok(code)
    }

    /// Where in `filling` a body of `len` bytes aligned to `align` starts,
    /// if it fits there.
    fn fitted(&self, filling: &Filling<P>, len: usize, align: usize) -> Option<usize> {
        let base = filling.chunk.index.start();
        let cursor = self.region.pages.rx(base).as_ptr().addr() + filling.cursor;
        let at = cursor.checked_next_multiple_of(align)? - cursor + filling.cursor;
        (at.checked_add(len)? <= CHUNK).then_some(at)
    }

    /// A free chunk, taken: its atomic cleared with an acquire that pairs
    /// with the release of the last `Code` that left it.
    fn taken(&self) -> Result<Filling<P>, Full> {
        let index = (self.region.free.iter())
            .position(|flag| {
                flag.compare_exchange(true, false, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            })
            .ok_or(Full)?;
        Ok(Filling {
            chunk: Arc::new(Chunk {
                region: Arc::clone(&self.region),
                index: ChunkIdx(index),
            }),
            cursor: 0,
        })
    }

    /// How many chunks are filling or hold a live `Code`.
    #[cfg(test)]
    pub(crate) fn chunks_in_use(&self) -> usize {
        self.region.in_use()
    }

    /// A watch on the arena's mapping that holds nothing of it.
    #[cfg(test)]
    pub(crate) fn watch(&self) -> Watch<P> {
        Watch(Arc::downgrade(&self.region))
    }
}

#[cfg(test)]
impl<P> Region<P> {
    /// How many chunks are filling or hold a live `Code`.
    fn in_use(&self) -> usize {
        (self.free.iter())
            .filter(|flag| !flag.load(Ordering::Acquire))
            .count()
    }
}

/// What a test sees of an arena's mapping without keeping it.
#[cfg(test)]
pub(crate) struct Watch<P>(std::sync::Weak<Region<P>>);

#[cfg(test)]
impl<P> Watch<P> {
    /// How many chunks are in use; none once the arena and every `Code`
    /// in it are gone, and the pages with them.
    pub(crate) fn chunks_in_use(&self) -> Option<usize> {
        Some(self.0.upgrade()?.in_use())
    }
}

/// The test adapter (`jit-design.md` §4.4): one allocation of
/// `UnsafeCell<u8>`, both views one pointer into it, nothing to maintain.
/// It tests the arena's bookkeeping, which is all a heap adapter can.
#[cfg(test)]
pub(crate) struct HeapPages {
    bytes: Box<[std::cell::UnsafeCell<u8>]>,
}

#[cfg(test)]
// SAFETY: the arena writes a range only before any `Code` names it, from
// the one thread holding the arena mutably, so no two threads touch one
// byte unsynchronised.
unsafe impl Sync for HeapPages {}

#[cfg(test)]
impl Pages for HeapPages {
    fn map_pair(len: usize) -> io::Result<Self> {
        Ok(HeapPages {
            bytes: (0..len).map(|_| std::cell::UnsafeCell::new(0)).collect(),
        })
    }

    unsafe fn write(&self, at: usize, bytes: &[u8]) {
        for (cell, byte) in self.bytes[at..at + bytes.len()].iter().zip(bytes) {
            // SAFETY: the caller hands a range nobody else reads or writes.
            unsafe { *cell.get() = *byte };
        }
    }

    unsafe fn sync(&self, _: Range<usize>) {}

    fn rx(&self, at: usize) -> NonNull<u8> {
        NonNull::from(&self.bytes[at]).cast()
    }
}

#[cfg(test)]
impl HeapPages {
    /// The byte at `at`, read through the executable view.
    fn read(&self, at: usize) -> u8 {
        // SAFETY: a test reads only bytes a publish has finished writing.
        unsafe { *self.bytes[at].get() }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use mac::MapJit;

/// The adapter of a target with none yet: it maps nothing, so an arena
/// over it is never made.
#[cfg(any(test, not(all(target_os = "macos", target_arch = "aarch64"))))]
pub(crate) struct Unmapped;

#[cfg(any(test, not(all(target_os = "macos", target_arch = "aarch64"))))]
impl Pages for Unmapped {
    fn map_pair(_: usize) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no code arena on this target yet",
        ))
    }

    unsafe fn write(&self, _: usize, _: &[u8]) {}

    unsafe fn sync(&self, _: Range<usize>) {}

    fn rx(&self, _: usize) -> NonNull<u8> {
        NonNull::dangling()
    }
}

/// `file` mapped once, readable and executable, and unmapped: the
/// `mmap` event `perf inject` finds a jitdump by, which php's
/// `ir_perf_jitdump_open` makes.
#[cfg(unix)]
pub(crate) fn map_once(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    const PAGE: usize = 4096;
    // SAFETY: a fresh private mapping of an open file, overlapping
    // nothing, read by nothing.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGE,
            libc::PROT_READ | libc::PROT_EXEC,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the mapping just made, which nothing else names.
    unsafe { libc::munmap(at, PAGE) };
    Ok(())
}

/// The monotonic clock in nanoseconds, the clock `perf record -k 1`
/// stamps with and php's `ir_perf_timestamp` reads; zero where it
/// cannot be read.
pub(crate) fn monotonic_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a live `timespec` the call writes.
    let read = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    if read != 0 {
        return 0;
    }
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// The calling thread's id as a jitdump record names it: `gettid` on
/// Linux, where php writes records; zero elsewhere, where it writes none.
pub(crate) fn thread_id() -> u32 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `gettid` reads the calling thread's id and nothing else.
        let tid = unsafe { libc::gettid() };
        u32::try_from(tid).unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// The macOS adapter (`jit-design.md` §4.4, `research/jit-research.r1.md`
/// §B.8): one `MAP_JIT` mapping, the one span the platform allows, its
/// two views one address. A thread writes it only between its own
/// `pthread_jit_write_protect_np(0)` and `(1)`; every other thread sees it
/// executable throughout, so there is no process-wide protection change
/// after `map_pair`.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod mac {
    use std::io;
    use std::ops::Range;
    use std::ptr::NonNull;

    use super::Pages;

    unsafe extern "C" {
        /// libSystem's cache maintenance over freshly written code:
        /// `<libkern/OSCacheControl.h>`.
        fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
    }

    /// One `MAP_JIT` anonymous mapping.
    pub(crate) struct MapJit {
        base: NonNull<u8>,
        len: usize,
    }

    // SAFETY: the mapping is the process's, valid on every thread, and the
    // arena writes a range only before any `Code` names it, from the one
    // thread holding the arena mutably.
    unsafe impl Send for MapJit {}
    // SAFETY: the same.
    unsafe impl Sync for MapJit {}

    impl Pages for MapJit {
        fn map_pair(len: usize) -> io::Result<Self> {
            // SAFETY: a fresh anonymous mapping, overlapping nothing.
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                    -1,
                    0,
                )
            };
            if base == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let base = NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
            Ok(MapJit { base, len })
        }

        unsafe fn write(&self, at: usize, bytes: &[u8]) {
            // SAFETY: opens the `MAP_JIT` region for writing on this thread
            // alone; every other thread still sees it executable.
            unsafe { libc::pthread_jit_write_protect_np(0) };
            let to = self.rx(at).as_ptr();
            // SAFETY: the caller hands a range inside the mapping that no
            // thread executes or reads, and `bytes` is not in it.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) };
            // SAFETY: closes it again, before anything on this thread runs
            // code in the region.
            unsafe { libc::pthread_jit_write_protect_np(1) };
        }

        unsafe fn sync(&self, range: Range<usize>) {
            let start = self.rx(range.start).as_ptr().cast::<libc::c_void>();
            // SAFETY: the caller hands a range inside the mapping, which
            // lives as long as `self`.
            unsafe { sys_icache_invalidate(start, range.len()) };
        }

        fn rx(&self, at: usize) -> NonNull<u8> {
            self.base.map_addr(|addr| addr.saturating_add(at))
        }
    }

    impl Drop for MapJit {
        fn drop(&mut self) {
            // SAFETY: the mapping `map_pair` made; the last `Code` and the
            // arena are gone, since each holds the region that holds this.
            unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arena(chunks: usize) -> Result<Arena<HeapPages>, String> {
        Arena::new(chunks * CHUNK).map_err(|e| e.to_string())
    }

    fn published(
        arena: &mut Arena<HeapPages>,
        bytes: &[u8],
        align: usize,
    ) -> Result<Code<HeapPages>, String> {
        arena
            .publish(bytes, align)
            .map_err(|Full| format!("no room for {} bytes", bytes.len()))
    }

    /// The offset of a code's first byte in the mapping.
    fn offset(arena: &Arena<HeapPages>, code: &Code<HeapPages>) -> usize {
        code.entry().as_ptr().addr() - arena.region.pages.rx(0).as_ptr().addr()
    }

    /// A capacity that is not a whole number of chunks is refused.
    #[test]
    fn an_arena_is_a_whole_number_of_chunks() {
        assert!(Arena::<HeapPages>::new(0).is_err());
        assert!(Arena::<HeapPages>::new(CHUNK + 1).is_err());
        assert!(Arena::<HeapPages>::new(2 * CHUNK).is_ok());
    }

    /// Bodies fill one chunk back to back, each at its alignment, and
    /// read back as written through the executable view.
    #[test]
    fn bodies_fill_a_chunk_in_order_each_at_its_alignment() -> Result<(), String> {
        let mut arena = arena(2)?;
        let a = published(&mut arena, &[1, 2, 3], 1)?;
        let b = published(&mut arena, &[4; 5], 16)?;
        let c = published(&mut arena, &[5; 8], 8)?;
        assert_eq!(
            (a.chunk(), b.chunk(), c.chunk()),
            (ChunkIdx(0), ChunkIdx(0), ChunkIdx(0))
        );
        assert_eq!(b.entry().as_ptr().addr() % 16, 0);
        assert_eq!(c.entry().as_ptr().addr() % 8, 0);
        assert!(offset(&arena, &b) >= 3 && offset(&arena, &c) >= offset(&arena, &b) + 5);
        let pages = &arena.region.pages;
        assert_eq!(
            (0..3)
                .map(|i| pages.read(offset(&arena, &a) + i))
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(pages.read(offset(&arena, &b) + 4), 4);
        assert_eq!((a.len(), b.len(), c.len()), (3, 5, 8));
        assert_eq!(arena.chunks_in_use(), 1);
        Ok(())
    }

    /// A body that does not fit what is left of the chunk goes to the next
    /// at its start; one larger than a chunk, an alignment that is not a
    /// power of two, and an arena whose chunks all hold code are `Full`.
    #[test]
    fn a_body_that_does_not_fit_takes_the_next_chunk_and_none_left_is_full() -> Result<(), String> {
        let mut arena = arena(2)?;
        let big = published(&mut arena, &vec![9; CHUNK - 8], 1)?;
        let next = published(&mut arena, &[1; 16], 1)?;
        assert_eq!((big.chunk(), next.chunk()), (ChunkIdx(0), ChunkIdx(1)));
        assert_eq!(offset(&arena, &next), CHUNK);
        assert_eq!(arena.chunks_in_use(), 2);
        assert_eq!(arena.publish(&vec![0; CHUNK - 8], 1).err(), Some(Full));
        assert_eq!(arena.publish(&vec![0; CHUNK + 1], 1).err(), Some(Full));
        assert_eq!(arena.publish(&[0], 3).err(), Some(Full));
        Ok(())
    }

    /// A chunk returns when the arena has moved past it and its last
    /// `Code` is dropped, and not before; the next body that needs a chunk
    /// reuses it from its start.
    #[test]
    fn a_code_dropped_last_frees_its_chunk_for_reuse() -> Result<(), String> {
        let mut arena = arena(2)?;
        let first = published(&mut arena, &vec![1; CHUNK], 1)?;
        let second = published(&mut arena, &[2; 8], 1)?;
        let kept = published(&mut arena, &vec![3; CHUNK - 8], 1)?;
        assert_eq!(
            (first.chunk(), second.chunk(), kept.chunk()),
            (ChunkIdx(0), ChunkIdx(1), ChunkIdx(1))
        );
        assert_eq!(arena.chunks_in_use(), 2);
        drop(first);
        assert_eq!(
            arena.chunks_in_use(),
            1,
            "chunk 0 holds nothing and the arena fills chunk 1"
        );
        drop(second);
        assert_eq!(
            arena.chunks_in_use(),
            1,
            "chunk 1 is still filling and holds a live code"
        );
        let reused = published(&mut arena, &[4; 8], 1)?;
        assert_eq!((reused.chunk(), offset(&arena, &reused)), (ChunkIdx(0), 0));
        assert_eq!(arena.region.pages.read(0), 4);
        drop(kept);
        assert_eq!(
            arena.chunks_in_use(),
            1,
            "chunk 1, let go by the arena, returns with its last code"
        );
        drop(reused);
        assert_eq!(arena.chunks_in_use(), 1, "chunk 0 is filling");
        Ok(())
    }

    /// The mapping outlives the arena while a `Code` holds it.
    #[test]
    fn a_code_keeps_its_region_past_the_arena() -> Result<(), String> {
        let mut arena = arena(1)?;
        let code = published(&mut arena, &[7; 4], 1)?;
        let region = Arc::clone(&arena.region);
        drop(arena);
        assert_eq!(region.pages.read(0), 7);
        assert!(!region.free[0].load(Ordering::Acquire));
        drop(code);
        assert!(region.free[0].load(Ordering::Acquire));
        Ok(())
    }
}

/// The body at `entry` as a function of no arguments returning `u32`: a
/// test's way into a `mov; ret` body it published, until `enter` names
/// the `Entry` type.
#[cfg(all(test, target_os = "macos", target_arch = "aarch64", not(miri)))]
pub(crate) fn callable(entry: NonNull<u8>) -> extern "C" fn() -> u32 {
    // SAFETY: every entry a test hands here is the first byte of a
    // complete `mov; ret` body in executable memory a live `Code` keeps,
    // which follows the C calling convention for a function of no
    // arguments returning `u32`.
    unsafe { std::mem::transmute::<*mut u8, extern "C" fn() -> u32>(entry.as_ptr()) }
}

/// The tests on the target itself (`jit-design.md` §4.4, §7's `table` row):
/// a body published through `MAP_JIT` and entered, a chunk published into
/// while another thread runs code in it, and a reused chunk entered from a
/// thread that never ran code in it, with and without that thread's own
/// `isb`. Every loop is bounded, so a failure cannot hang the binary.
#[cfg(all(test, target_os = "macos", target_arch = "aarch64", not(miri)))]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod native {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::{Arena, CHUNK, Code, Full, MapJit, callable};

    /// `mov w0, #value; ret`: a body that returns `value`, a 16-bit
    /// immediate.
    fn returning(value: u16) -> [u8; 8] {
        let mov = 0x5280_0000_u32 | (u32::from(value) << 5);
        let ret = 0xD65F_03C0_u32;
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&mov.to_le_bytes());
        bytes[4..].copy_from_slice(&ret.to_le_bytes());
        bytes
    }

    fn arena(chunks: usize) -> Result<Arena<MapJit>, String> {
        Arena::new(chunks * CHUNK).map_err(|e| e.to_string())
    }

    fn published(arena: &mut Arena<MapJit>, value: u16) -> Result<Code<MapJit>, String> {
        arena
            .publish(&returning(value), 4)
            .map_err(|Full| format!("no room for the body returning {value}"))
    }

    /// The body as a function: what compiled code's caller does with an
    /// entry, until `enter` names the `Entry` type.
    fn entered(code: &Code<MapJit>) -> extern "C" fn() -> u32 {
        callable(code.entry())
    }

    /// This thread's own instruction synchronisation barrier, `isb`.
    fn isb() {
        // SAFETY: `isb` touches no memory and no register the compiler
        // allocates.
        unsafe { std::arch::asm!("isb", options(nostack, preserves_flags)) };
    }

    /// `mov w0, #7; ret` published through the write view and entered
    /// through the executable view returns 7.
    #[test]
    fn mov_ret_published_and_entered_returns_seven() -> Result<(), String> {
        let mut arena = arena(1)?;
        let code = published(&mut arena, 7)?;
        assert_eq!(entered(&code)(), 7);
        Ok(())
    }

    /// One thread runs a published body in a loop while another publishes
    /// a hundred bodies into the same chunk, each entered as it lands; the
    /// first asserts its body's answer on every iteration.
    #[test]
    fn a_body_runs_unchanged_while_a_hundred_publish_into_its_chunk() -> Result<(), String> {
        const BODIES: u16 = 100;
        const MOST: u64 = 50_000_000;
        let mut arena = arena(1)?;
        let spun = published(&mut arena, 7)?;
        let (done, started, iterations) = (
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicU64::new(0),
        );
        let mut late = Vec::new();
        std::thread::scope(|scope| -> Result<(), String> {
            let spinner = scope.spawn(|| {
                let body = entered(&spun);
                let mut n = 0_u64;
                while n < MOST && (n < 1000 || !done.load(Ordering::Acquire)) {
                    assert_eq!(body(), 7, "iteration {n}");
                    n += 1;
                    started.store(true, Ordering::Release);
                }
                iterations.store(n, Ordering::Relaxed);
            });
            while !started.load(Ordering::Acquire) && !spinner.is_finished() {
                std::hint::spin_loop();
            }
            for value in 0..BODIES {
                let code = published(&mut arena, 1000 + value)?;
                assert_eq!(code.chunk(), spun.chunk());
                assert_eq!(entered(&code)(), u32::from(1000 + value));
                late.push(code);
            }
            done.store(true, Ordering::Release);
            spinner
                .join()
                .map_err(|_| "the spinning thread panicked".to_owned())
        })?;
        assert!(iterations.load(Ordering::Relaxed) >= 1000);
        assert_eq!(late.len(), usize::from(BODIES));
        Ok(())
    }

    /// The reused-chunk question of §4.4, over `ROUNDS` rounds: a body
    /// entered on as many threads as the machine has cores, and dropped; a thread started that has
    /// never entered the chunk, waiting on an atomic pointer; a different
    /// body published at the same address of the reused chunk; and its
    /// entry stored in the pointer, the waiting thread entering the new
    /// body as soon as it sees it, with or without its own `isb` first,
    /// and with no system call between the two. Answers how many entries
    /// returned the old body's value.
    fn stale_entries(own_isb: bool) -> Result<u32, String> {
        const ROUNDS: u16 = 500;
        // A core and not a thread holds instructions, and which core a
        // thread runs on is the system's to choose: one warming thread per
        // core, so the entering thread's core has likely run the old body.
        let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let mut arena = arena(1)?;
        let mut stale = 0;
        // Each round starts with the arena's one chunk full, or never
        // taken, so its first body lands at the chunk's start.
        let fill = |arena: &mut Arena<MapJit>| {
            (arena.publish(&vec![0; CHUNK - 8], 1))
                .map_err(|Full| "no room for the filler".to_owned())
        };
        for round in 0..ROUNDS {
            let (old, new) = (2 * round, 2 * round + 1);
            let first = published(&mut arena, old)?;
            let filler = fill(&mut arena)?;
            let address = first.entry();
            let entry = AtomicPtr::new(std::ptr::null_mut());
            let answer = std::thread::scope(|scope| -> Result<u32, String> {
                let entering = scope.spawn(|| {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        let at = entry.load(Ordering::Acquire);
                        if let Some(at) = NonNull::new(at) {
                            if own_isb {
                                isb();
                            }
                            return Some(callable(at)());
                        }
                        if Instant::now() > deadline {
                            return None;
                        }
                        std::hint::spin_loop();
                    }
                });
                std::thread::scope(|warm| {
                    for _ in 0..cores {
                        warm.spawn(|| {
                            assert!((0..10_000).all(|_| entered(&first)() == u32::from(old)));
                        });
                    }
                });
                drop((first, filler));
                let second = published(&mut arena, new)?;
                assert_eq!(
                    second.entry(),
                    address,
                    "round {round}: the reused chunk's first body is at its start"
                );
                entry.store(second.entry().as_ptr(), Ordering::Release);
                let answer = entering
                    .join()
                    .map_err(|_| "the entering thread panicked".to_owned())?;
                drop(second);
                answer.ok_or_else(|| {
                    format!("round {round}: the entering thread saw no body in ten seconds")
                })
            })?;
            drop(fill(&mut arena)?);
            match answer {
                a if a == u32::from(new) => {}
                a if a == u32::from(old) => stale += 1,
                a => {
                    return Err(format!(
                        "round {round}: the reused chunk answered {a}, neither {old} nor {new}"
                    ));
                }
            }
        }
        Ok(stale)
    }

    #[test]
    fn a_reused_chunk_entered_by_a_fresh_thread_with_its_own_isb_runs_the_new_body()
    -> Result<(), String> {
        assert_eq!(stale_entries(true)?, 0);
        Ok(())
    }

    #[test]
    fn a_reused_chunk_entered_by_a_fresh_thread_without_an_isb_runs_the_new_body()
    -> Result<(), String> {
        assert_eq!(stale_entries(false)?, 0);
        Ok(())
    }
}
