//! The code arena (`jit-design.md` §4.4), sealed by construction: one
//! span of pages seen through a write view and an executable view, as
//! many bytes as php's buffer holds ([`capacity`]), carved into 64 KiB
//! chunks, the last of them as long as the span leaves it, into which
//! [`Arena::publish`] copies a body once.
//! Bytes are final: a [`Code`] has no way to be written again, and the
//! chunk it lives in returns to the arena when the last `Code` in it is
//! dropped, so a later body may reuse it.
//!
//! [`Pages`] is the seam under the arena, the one span and the cache
//! maintenance between its two views, with three adapters: [`HeapPages`],
//! the test adapter, one allocation of `UnsafeCell<u8>` with nothing to
//! maintain, which tests the arena's bookkeeping under Miri; `MapJit`,
//! macOS on aarch64, one `MAP_JIT` anonymous mapping whose two views are
//! one address and the per-thread `pthread_jit_write_protect_np` toggle
//! around each write, with `sys_icache_invalidate` over the bytes written;
//! and `Memfd`, Linux on x86-64 and aarch64, one sealed `memfd` file
//! mapped twice, written through the one view and executed through the
//! other, with §4.4's cache maintenance over the executable addresses on
//! aarch64. Where the build sets no `php_jit_arena` (`build.rs`),
//! [`Unmapped`] maps nothing, and a table over it compiles nothing. Each
//! mapping an adapter makes is one [`Mapping`], unmapped when it drops.
//!
//! Beside the arena, the system's page its size is rounded to, and the
//! three calls the `PERF_DUMP` record needs that only `unsafe` reaches:
//! the jitdump's one mapping, the monotonic clock its records are
//! stamped with, and the thread that wrote them.
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
    /// How many bytes the mapping holds: every chunk's but the last's
    /// are [`CHUNK`].
    len: usize,
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

    /// How many bytes the chunk holds in a mapping of `len` bytes: a
    /// whole chunk, or what the mapping leaves the last.
    fn len(self, len: usize) -> usize {
        len.saturating_sub(self.start()).min(CHUNK)
    }
}

/// How many bytes an arena holds under `opcache.jit_buffer_size` of
/// `size`: php's buffer, the size rounded up to the page
/// (`ZEND_MM_ALIGNED_SIZE_EX(jit_size, page_size)`,
/// `Z/ext/opcache/ZendAccelerator.c:3257-3263`); none for none, or where
/// the size is past what an address holds.
pub(crate) fn capacity(size: u64) -> usize {
    usize::try_from(size)
        .ok()
        .and_then(|size| size.checked_next_multiple_of(page_size()))
        .unwrap_or(0)
}

/// The system's page, as php's `zend_get_page_size` reads it; 4096 where
/// it cannot be read.
fn page_size() -> usize {
    // SAFETY: `sysconf` reads a system value and writes nothing.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page)
        .ok()
        .filter(|page| page.is_power_of_two())
        .unwrap_or(4096)
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the chunk is held, not read, so it is not reused while the body stands; the length is the disassembly's"
    )
)]
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
    #[cfg_attr(not(test), expect(dead_code, reason = "the disassembly's"))]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Which chunk of the arena the body is in.
    #[cfg(test)]
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
    /// An arena of `capacity` bytes, mapped through `P`, in chunks of
    /// [`CHUNK`] bytes but the last, which holds what is left; a buffer
    /// below one chunk is one chunk of its own length, held by the cursor
    /// inside it (#3112's ruling). A capacity of none is refused.
    pub(crate) fn new(capacity: usize) -> io::Result<Arena<P>> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a code arena holds at least one byte",
            ));
        }
        let pages = P::map_pair(capacity)?;
        let free = (0..capacity.div_ceil(CHUNK))
            .map(|_| AtomicBool::new(true))
            .collect();
        Ok(Arena {
            region: Arc::new(Region {
                pages,
                len: capacity,
                free,
            }),
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
        (at.checked_add(len)? <= filling.chunk.index.len(self.region.len)).then_some(at)
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

#[cfg(all(php_jit_arena, target_os = "macos"))]
pub(crate) use mac::MapJit;

#[cfg(all(php_jit_arena, target_os = "linux"))]
pub(crate) use linux::Memfd;

/// The adapter of a target with none (`build.rs` sets no `php_jit_arena`):
/// it maps nothing, so an arena over it is never made.
#[cfg(any(test, not(php_jit_arena)))]
pub(crate) struct Unmapped;

#[cfg(any(test, not(php_jit_arena)))]
impl Pages for Unmapped {
    fn map_pair(_: usize) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no code arena on this target",
        ))
    }

    unsafe fn write(&self, _: usize, _: &[u8]) {}

    unsafe fn sync(&self, _: Range<usize>) {}

    fn rx(&self, _: usize) -> NonNull<u8> {
        NonNull::dangling()
    }
}

/// One mapping an adapter owns, `len` bytes from `base`, unmapped when it
/// drops: what `MapJit`'s one view, `Memfd`'s two and the jitdump's
/// `mmap` event each are.
#[cfg(unix)]
struct Mapping {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: a mapping is the process's, valid on every thread, and a
// `Mapping` hands out addresses only; what reads or writes through them
// is the caller's `unsafe`, which the arena's one writer keeps.
#[cfg(unix)]
unsafe impl Send for Mapping {}
// SAFETY: the same.
#[cfg(unix)]
unsafe impl Sync for Mapping {}

#[cfg(unix)]
impl Mapping {
    /// `len` bytes mapped fresh, overlapping nothing, with `prot` and
    /// `flags`: of `file` from its start, or anonymous where it is none.
    fn new(
        len: usize,
        prot: libc::c_int,
        flags: libc::c_int,
        file: Option<std::os::fd::BorrowedFd<'_>>,
    ) -> io::Result<Mapping> {
        use std::os::fd::AsRawFd;
        let fd = file.map_or(-1, |file| file.as_raw_fd());
        // SAFETY: a fresh mapping at an address the system chooses, so it
        // overlaps nothing; `fd` is open for the call or `-1` with
        // `MAP_ANON`.
        let base = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, 0) };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(base.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
        Ok(Mapping { base, len })
    }

    /// The address of offset `at`, which lies inside the mapping; an
    /// address only, which reads and writes nothing.
    fn at(&self, at: usize) -> NonNull<u8> {
        self.base.map_addr(|addr| addr.saturating_add(at))
    }
}

#[cfg(unix)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping this owns, made by `new` or by the Linux
        // adapter's `view`, whose addresses nothing uses past its owner: an
        // arena's pages drop with their region, after the arena and the
        // last `Code`, each of which holds the region.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
    }
}

/// `file` mapped once, readable and executable, and unmapped: the
/// `mmap` event `perf inject` finds a jitdump by, which php's
/// `ir_perf_jitdump_open` makes.
#[cfg(unix)]
pub(crate) fn map_once(file: &std::fs::File) -> io::Result<()> {
    use std::os::fd::AsFd;
    let once = Mapping::new(
        page_size(),
        libc::PROT_READ | libc::PROT_EXEC,
        libc::MAP_PRIVATE,
        Some(file.as_fd()),
    )?;
    drop(once);
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
#[cfg(all(php_jit_arena, target_os = "macos"))]
mod mac {
    use std::io;
    use std::ops::Range;
    use std::ptr::NonNull;

    use super::{Mapping, Pages};

    unsafe extern "C" {
        /// libSystem's cache maintenance over freshly written code:
        /// `<libkern/OSCacheControl.h>`.
        fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
    }

    /// One `MAP_JIT` anonymous mapping.
    pub(crate) struct MapJit(Mapping);

    impl Pages for MapJit {
        fn map_pair(len: usize) -> io::Result<Self> {
            let map = Mapping::new(
                len,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                None,
            )?;
            Ok(MapJit(map))
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
            self.0.at(at)
        }
    }
}

/// The Linux adapter (`jit-design.md` §4.4, (B)): one `memfd` file of the
/// arena's length, sealed against shrinking and growing and against any
/// further seal, mapped twice as a whole with `MAP_SHARED`, once
/// `PROT_READ | PROT_WRITE`, written only by the thread holding the arena
/// mutably and never executed, and once `PROT_READ | PROT_EXEC`, never remapped. The
/// file is closed once both views stand; they keep its pages. The file,
/// its seals and the two mappings go through `rustix`, as #2945 ruled; a
/// view is a [`Mapping`], unmapped when it drops.
#[cfg(all(php_jit_arena, target_os = "linux"))]
mod linux {
    use std::io;
    use std::ops::Range;
    use std::os::fd::OwnedFd;
    use std::ptr::NonNull;

    use rustix::fs::{MemfdFlags, SealFlags};
    use rustix::mm::{MapFlags, ProtFlags};

    use super::{Mapping, Pages};

    /// One sealed `memfd`'s two views.
    pub(crate) struct Memfd {
        written: Mapping,
        executed: Mapping,
    }

    impl Pages for Memfd {
        fn map_pair(len: usize) -> io::Result<Self> {
            let file = sealed(len)?;
            Ok(Memfd {
                written: view(&file, len, ProtFlags::READ | ProtFlags::WRITE)?,
                executed: view(&file, len, ProtFlags::READ | ProtFlags::EXEC)?,
            })
        }

        unsafe fn write(&self, at: usize, bytes: &[u8]) {
            let to = self.written.at(at).as_ptr();
            // SAFETY: the caller hands a range inside the mapping that no
            // thread executes or reads, and `bytes` is not in it.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) };
        }

        #[cfg(target_arch = "aarch64")]
        unsafe fn sync(&self, range: Range<usize>) {
            let start = self.rx(range.start);
            // SAFETY: the caller hands a range inside the mapping, whose
            // executable view lives as long as `self`.
            unsafe { aarch64::synced(start, range.len()) };
        }

        /// Nothing: x86-64's instruction caches are coherent with stores,
        /// snooped by physical address, so a store through either alias is
        /// what the next fetch sees (`jit-design.md` §4.4).
        #[cfg(not(target_arch = "aarch64"))]
        unsafe fn sync(&self, _: Range<usize>) {}

        fn rx(&self, at: usize) -> NonNull<u8> {
            self.executed.at(at)
        }
    }

    #[cfg(test)]
    impl Memfd {
        /// The write view's address of offset `at`.
        pub(super) fn written(&self, at: usize) -> NonNull<u8> {
            self.written.at(at)
        }
    }

    /// A `memfd` of `len` bytes, sealed against shrinking, growing and any
    /// further seal; never an executable itself (`MFD_NOEXEC_SEAL`) where
    /// the kernel knows it, as Linux 6.3 and later do and as
    /// `vm.memfd_noexec=2` requires.
    pub(super) fn sealed(len: usize) -> io::Result<OwnedFd> {
        let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING;
        let file = match rustix::fs::memfd_create("php_jit", flags | MemfdFlags::NOEXEC_SEAL) {
            Err(rustix::io::Errno::INVAL) => rustix::fs::memfd_create("php_jit", flags)?,
            made => made?,
        };
        let len = u64::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        rustix::fs::ftruncate(&file, len)?;
        rustix::fs::fcntl_add_seals(&file, SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL)?;
        Ok(file)
    }

    /// The whole of `file`, `len` bytes, mapped shared with `prot`.
    fn view(file: &OwnedFd, len: usize, prot: ProtFlags) -> io::Result<Mapping> {
        // SAFETY: a fresh mapping at an address the system chooses, so it
        // overlaps nothing, of a file open for the call.
        let base = unsafe {
            rustix::mm::mmap(std::ptr::null_mut(), len, prot, MapFlags::SHARED, file, 0)
        }?;
        let base =
            NonNull::new(base.cast::<u8>()).ok_or_else(|| io::Error::other("a mapping at null"))?;
        Ok(Mapping { base, len })
    }

    /// aarch64's cache maintenance after a write through the other alias
    /// (`jit-design.md` §4.4): the data cache is physically tagged, so one
    /// sequence over the executable view's addresses suffices, each line
    /// as long as `CTR_EL0` says, which Linux lets user space read.
    #[cfg(target_arch = "aarch64")]
    mod aarch64 {
        use std::arch::asm;
        use std::ptr::NonNull;

        /// `dc cvau` per data-cache line, `dsb ish`, `ic ivau` per
        /// instruction-cache line, `dsb ish`, `isb`, over `len` bytes
        /// from `start`.
        ///
        /// # Safety
        ///
        /// `start..start + len` lies inside a live readable mapping.
        pub(super) unsafe fn synced(start: NonNull<u8>, len: usize) {
            let ctr: u64;
            // SAFETY: reads `CTR_EL0`, which touches no memory; Linux
            // enables it at EL0, or traps and emulates it.
            unsafe {
                asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
            }
            // Each field is log2 of a line's 4-byte words.
            let data = 4_usize << ((ctr >> 16) & 0xF);
            let instruction = 4_usize << (ctr & 0xF);
            let (start, end) = (start.as_ptr().addr(), start.as_ptr().addr() + len);
            for line in (start & !(data - 1)..end).step_by(data) {
                // SAFETY: cleans a line of the caller's mapping to the
                // point of unification; it changes no byte.
                unsafe { asm!("dc cvau, {}", in(reg) line, options(nostack, preserves_flags)) };
            }
            // SAFETY: a barrier, which touches no byte.
            unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
            for line in (start & !(instruction - 1)..end).step_by(instruction) {
                // SAFETY: invalidates a line of the caller's mapping in
                // every core's instruction cache; it changes no byte.
                unsafe { asm!("ic ivau, {}", in(reg) line, options(nostack, preserves_flags)) };
            }
            // SAFETY: a barrier, which touches no byte.
            unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
            // SAFETY: this thread's instruction synchronisation barrier,
            // which touches no byte.
            unsafe { asm!("isb", options(nostack, preserves_flags)) };
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

    /// An arena holds the bytes it was made with and no more: a capacity
    /// below one chunk is a chunk of that length, filled to its last byte
    /// and no further, one past whole chunks a last chunk of what is left,
    /// and none is refused; php's buffer is `opcache.jit_buffer_size`
    /// rounded up to the page.
    #[test]
    fn an_arena_holds_its_capacity_and_no_more() -> Result<(), String> {
        assert!(Arena::<HeapPages>::new(0).is_err());
        let page = page_size();
        assert_eq!(
            (capacity(16 << 10), capacity(1), capacity(0)),
            ((16_usize << 10).next_multiple_of(page), page, 0)
        );
        let mut small = Arena::<HeapPages>::new(16 << 10).map_err(|e| e.to_string())?;
        assert_eq!(small.publish(&vec![1; (16 << 10) + 1], 1).err(), Some(Full));
        let first = published(&mut small, &vec![1; 12 << 10], 1)?;
        let second = published(&mut small, &vec![2; 4 << 10], 1)?;
        assert_eq!(offset(&small, &second), 12 << 10);
        assert_eq!(small.publish(&[3], 1).err(), Some(Full));
        drop((first, second));
        let again = published(&mut small, &vec![4; 16 << 10], 1)?;
        assert_eq!(offset(&small, &again), 0, "the one chunk, reused whole");
        let mut odd = Arena::<HeapPages>::new(CHUNK + 100).map_err(|e| e.to_string())?;
        let whole = published(&mut odd, &vec![5; CHUNK], 1)?;
        assert_eq!(odd.publish(&[6; 101], 1).err(), Some(Full));
        let last = published(&mut odd, &[6; 100], 1)?;
        assert_eq!((whole.chunk(), last.chunk()), (ChunkIdx(0), ChunkIdx(1)));
        Ok(())
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
#[cfg(all(test, php_jit_arena, not(miri)))]
pub(crate) fn callable(entry: NonNull<u8>) -> extern "C" fn() -> u32 {
    // SAFETY: every entry a test hands here is the first byte of a
    // complete `mov; ret` body in executable memory a live `Code` keeps,
    // which follows the C calling convention for a function of no
    // arguments returning `u32`.
    unsafe { std::mem::transmute::<*mut u8, extern "C" fn() -> u32>(entry.as_ptr()) }
}

/// `mov w0, #value; ret`: a body that returns `value`, a 16-bit
/// immediate, on aarch64.
#[cfg(all(test, php_jit_arena, not(miri), target_arch = "aarch64"))]
pub(crate) fn returning(value: u16) -> [u8; 8] {
    let mov = 0x5280_0000_u32 | (u32::from(value) << 5);
    let ret = 0xD65F_03C0_u32;
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&mov.to_le_bytes());
    bytes[4..].copy_from_slice(&ret.to_le_bytes());
    bytes
}

/// `mov eax, value; ret`: a body that returns `value` on x86-64.
#[cfg(all(test, php_jit_arena, not(miri), target_arch = "x86_64"))]
pub(crate) fn returning(value: u16) -> [u8; 6] {
    let mut bytes = [0xB8, 0, 0, 0, 0, 0xC3];
    bytes[1..5].copy_from_slice(&u32::from(value).to_le_bytes());
    bytes
}

/// The tests on the target itself (`jit-design.md` §4.4, §7's `table` row),
/// over the adapter the table maps code through: a body published
/// through the write view and entered through the executable view, a
/// chunk published into while another thread runs code in it, and on
/// aarch64 a reused chunk entered from a thread that never ran code in
/// it, with and without that thread's own `isb`; on Linux, that the two
/// views are two addresses of one sealed file. Every loop is bounded, so
/// a failure cannot hang the binary.
#[cfg(all(test, php_jit_arena, not(miri)))]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod native {
    #[cfg(target_arch = "aarch64")]
    use std::ptr::NonNull;
    #[cfg(target_arch = "aarch64")]
    use std::sync::atomic::AtomicPtr;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    #[cfg(target_arch = "aarch64")]
    use std::time::{Duration, Instant};

    use super::{Arena, CHUNK, Code, Full, callable, returning};
    use crate::table::Native;

    fn arena(chunks: usize) -> Result<Arena<Native>, String> {
        Arena::new(chunks * CHUNK).map_err(|e| e.to_string())
    }

    fn published(arena: &mut Arena<Native>, value: u16) -> Result<Code<Native>, String> {
        arena
            .publish(&returning(value), 4)
            .map_err(|Full| format!("no room for the body returning {value}"))
    }

    /// The body as a function: what compiled code's caller does with an
    /// entry, until `enter` names the `Entry` type.
    fn entered(code: &Code<Native>) -> extern "C" fn() -> u32 {
        callable(code.entry())
    }

    /// This thread's own instruction synchronisation barrier, `isb`.
    #[cfg(target_arch = "aarch64")]
    fn isb() {
        // SAFETY: `isb` touches no memory and no register the compiler
        // allocates.
        unsafe { std::arch::asm!("isb", options(nostack, preserves_flags)) };
    }

    /// `mov w0, #7; ret` on aarch64, `mov eax, 7; ret` on x86-64,
    /// published through the write view and entered through the
    /// executable view, returns 7.
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
    #[cfg(target_arch = "aarch64")]
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
        let fill = |arena: &mut Arena<Native>| {
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

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn a_reused_chunk_entered_by_a_fresh_thread_with_its_own_isb_runs_the_new_body()
    -> Result<(), String> {
        assert_eq!(stale_entries(true)?, 0);
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn a_reused_chunk_entered_by_a_fresh_thread_without_an_isb_runs_the_new_body()
    -> Result<(), String> {
        assert_eq!(stale_entries(false)?, 0);
        Ok(())
    }

    /// On Linux the two views are two addresses of one sealed file: what
    /// the write view holds the executable view reads, `/proc/self/maps`
    /// shows the one shared and writable and the other shared and
    /// executable, both of the `memfd`, and the file refuses to shrink or
    /// grow.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_two_views_are_two_addresses_of_one_sealed_file() -> Result<(), String> {
        use super::Pages;
        let sealed = super::linux::sealed(CHUNK).map_err(|e| e.to_string())?;
        let whole = u64::try_from(CHUNK).map_err(|e| e.to_string())?;
        assert!(rustix::fs::ftruncate(&sealed, whole + 1).is_err(), "grown");
        assert!(rustix::fs::ftruncate(&sealed, whole - 1).is_err(), "shrunk");
        let mut arena = arena(1)?;
        let code = published(&mut arena, 7)?;
        let pages = &arena.region.pages;
        let at = code.entry().as_ptr().addr() - pages.rx(0).as_ptr().addr();
        let written = pages.written(at);
        assert_ne!(written, code.entry());
        let body = returning(7);
        // SAFETY: the write view's bytes of the body just published, which
        // nothing writes while the arena is held here.
        let seen = unsafe { std::slice::from_raw_parts(written.as_ptr(), body.len()) };
        assert_eq!(seen, body);
        let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| e.to_string())?;
        let mapped = |address: usize| {
            maps.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                let (low, high) = fields.next()?.split_once('-')?;
                let range =
                    usize::from_str_radix(low, 16).ok()?..usize::from_str_radix(high, 16).ok()?;
                let perms = fields.next()?;
                let file = fields.nth(3).unwrap_or("");
                range
                    .contains(&address)
                    .then(|| (perms.to_owned(), file.to_owned()))
            })
        };
        let file = "/memfd:php_jit".to_owned();
        assert_eq!(
            mapped(written.as_ptr().addr()),
            Some(("rw-s".to_owned(), file.clone()))
        );
        assert_eq!(
            mapped(code.entry().as_ptr().addr()),
            Some(("r-xs".to_owned(), file))
        );
        Ok(())
    }
}
