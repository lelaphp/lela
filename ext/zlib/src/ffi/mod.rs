//! The binding to zlib (design §7.2, `unsafe.homes`' `ext/*/src/ffi/`):
//! the platform's library, linked through `libz-sys`, and the one home of
//! this crate's `unsafe`. php's tests observe the compressed bytes, and
//! those follow the linked library's algorithm, so the library is the
//! one the oracle links: macOS's `/usr/lib/libz.1.dylib`, 1.2.12, where
//! every level, strategy, window and memory level php exposes writes the
//! oracle's bytes (#950's measurement; zlib-rs and `miniz_oxide` do not).
//!
//! Three safe types over it: [`Deflate`] and [`Inflate`], one `z_stream`
//! each after `deflateInit2` or `inflateInit2`, and [`GzFile`], the
//! `gzFile` `php_stream_gzopen` reads and writes through. A `z_stream`
//! is boxed, since zlib's state names the stream it was made for, and
//! each call hands zlib the caller's slices for that call alone, which is
//! all zlib reads of `next_in` and `next_out`. zlib allocates through
//! [`alloc`] and [`free`], on the request's heap as `php_zlib_alloc`'s
//! `safe_emalloc` allocates (design §5.13), or for a persistent stream's
//! filter through [`palloc`] and [`pfree`], on the system's, as the
//! [`Heap`] a stream is made over says; a `gzFile` allocates through
//! zlib's own.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, c_char, c_int, c_uint};
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;

use std::marker::PhantomData;

use libz_sys as z;
use php_ext::abi::EngineState;
use php_ext::raising::Thrown;
use php_ext::request::{Request, efree, emalloc};
use php_ext::values::{Emalloc, Malloc, Pemalloc};

// SAFETY: zlib's own `zError`, which libz-sys does not declare, from the
// library it links: an `int` in, a pointer into a static table out.
unsafe extern "C" {
    fn zError(err: c_int) -> *const c_char;
}

/// What a zlib call answers, `Z_OK` through `Z_VERSION_ERROR`, the codes
/// `ZLIB_OK` and its kin name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Ok,
    StreamEnd,
    NeedDict,
    Errno,
    StreamError,
    DataError,
    MemError,
    BufError,
    VersionError,
}

impl Status {
    /// The code zlib answered; zlib answers none but these nine, and
    /// anything else reads as `Z_STREAM_ERROR`, what zlib says of a stream
    /// it cannot read.
    fn of(code: c_int) -> Status {
        match code {
            z::Z_OK => Status::Ok,
            z::Z_STREAM_END => Status::StreamEnd,
            z::Z_NEED_DICT => Status::NeedDict,
            z::Z_ERRNO => Status::Errno,
            z::Z_DATA_ERROR => Status::DataError,
            z::Z_MEM_ERROR => Status::MemError,
            z::Z_BUF_ERROR => Status::BufError,
            z::Z_VERSION_ERROR => Status::VersionError,
            _ => Status::StreamError,
        }
    }

    /// The code, as `inflate_get_status` answers it.
    pub(crate) fn code(self) -> i64 {
        i64::from(match self {
            Status::Ok => z::Z_OK,
            Status::StreamEnd => z::Z_STREAM_END,
            Status::NeedDict => z::Z_NEED_DICT,
            Status::Errno => z::Z_ERRNO,
            Status::StreamError => z::Z_STREAM_ERROR,
            Status::DataError => z::Z_DATA_ERROR,
            Status::MemError => z::Z_MEM_ERROR,
            Status::BufError => z::Z_BUF_ERROR,
            Status::VersionError => z::Z_VERSION_ERROR,
        })
    }

    /// `zError(code)`: the library's own text, `data error` and its kin.
    pub(crate) fn message(self) -> String {
        let code = c_int::try_from(self.code()).unwrap_or(z::Z_STREAM_ERROR);
        // SAFETY: zError takes any code of the nine and answers a pointer
        // into the library's static `z_errmsg` table.
        let text = unsafe { zError(code) };
        if text.is_null() {
            return String::new();
        }
        // SAFETY: the table's entries are NUL-terminated C strings that
        // live as long as the library.
        let text = unsafe { CStr::from_ptr(text) };
        text.to_string_lossy().into_owned()
    }
}

/// The flush a `deflate` or `inflate` call is made with, `Z_NO_FLUSH`
/// through `Z_BLOCK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flush {
    None,
    Partial,
    Sync,
    Full,
    Finish,
    Block,
}

impl Flush {
    /// The flush a program named by its `ZLIB_*` value, `None` for a
    /// number that is none of the six.
    pub(crate) fn of(mode: i64) -> Option<Flush> {
        Some(match c_int::try_from(mode).ok()? {
            z::Z_NO_FLUSH => Flush::None,
            z::Z_PARTIAL_FLUSH => Flush::Partial,
            z::Z_SYNC_FLUSH => Flush::Sync,
            z::Z_FULL_FLUSH => Flush::Full,
            z::Z_FINISH => Flush::Finish,
            z::Z_BLOCK => Flush::Block,
            _ => return None,
        })
    }

    fn code(self) -> c_int {
        match self {
            Flush::None => z::Z_NO_FLUSH,
            Flush::Partial => z::Z_PARTIAL_FLUSH,
            Flush::Sync => z::Z_SYNC_FLUSH,
            Flush::Full => z::Z_FULL_FLUSH,
            Flush::Finish => z::Z_FINISH,
            Flush::Block => z::Z_BLOCK,
        }
    }
}

/// What one call did: its status, how much of the input it read and how
/// much it wrote.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Step {
    pub(crate) status: Status,
    pub(crate) read: usize,
    pub(crate) written: usize,
}

/// `zlibVersion()`: the linked library's version, `1.2.12` on the
/// oracle's machine.
pub(crate) fn version() -> String {
    // SAFETY: zlibVersion answers a pointer to the library's static
    // ZLIB_VERSION string.
    let text = unsafe { z::zlibVersion() };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: that string is NUL-terminated and lives as long as the
    // library.
    let text = unsafe { CStr::from_ptr(text) };
    text.to_string_lossy().into_owned()
}

/// zlib's `zalloc`, `php_zlib_alloc` (`Z/ext/zlib/zlib.c:122-125`):
/// `safe_emalloc(items, size, 0)`, `items * size` bytes of the heap the
/// call is lent, left as the heap finds them, as php's `safe_emalloc`
/// leaves them. Two `uInt`s multiply inside a 64-bit `size_t`, so php's
/// overflow check never fires for them.
extern "C" fn alloc(_opaque: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    emalloc(items as usize * size as usize)
}

/// zlib's `zfree`, `php_zlib_free` (`Z/ext/zlib/zlib.c:127-130`):
/// `efree(address)`, under the lend of the call zlib is in.
extern "C" fn free(_opaque: z::voidpf, address: z::voidpf) {
    // SAFETY: zlib frees a block once, and only one [`alloc`] answered,
    // so a block of the lent heap or, where the limit refused one, of
    // the system allocator.
    unsafe { efree(address) }
}

/// The bytes a [`palloc`] block carries before zlib's: the block's whole
/// size, which [`pfree`] gives back the layout of, at zlib's alignment.
const PHEADER: usize = 16;

/// zlib's `zalloc` for a persistent stream's filter: `items * size`
/// bytes of the system allocator, which no request's heap counts or
/// frees, since the stream outlives the request that opened it.
#[allow(
    clippy::cast_ptr_alignment,
    reason = "the block's layout is `PHEADER`-aligned, past `usize`'s"
)]
extern "C" fn palloc(_opaque: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    let total = (items as usize * size as usize).saturating_add(PHEADER);
    let Ok(layout) = std::alloc::Layout::from_size_align(total, PHEADER) else {
        return std::ptr::null_mut();
    };
    // SAFETY: the layout is at least `PHEADER` bytes, never zero-sized.
    let at = unsafe { std::alloc::alloc(layout) };
    if at.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the block is `PHEADER`-aligned and holds `PHEADER` bytes.
    unsafe { at.cast::<usize>().write(total) };
    // SAFETY: `PHEADER` bytes into the block, which is longer.
    unsafe { at.add(PHEADER) }.cast()
}

/// zlib's `zfree` for what [`palloc`] answered.
#[allow(
    clippy::cast_ptr_alignment,
    reason = "the block's layout is `PHEADER`-aligned, past `usize`'s"
)]
extern "C" fn pfree(_opaque: z::voidpf, address: z::voidpf) {
    if address.is_null() {
        return;
    }
    // SAFETY: zlib frees only what `palloc` answered, `PHEADER` bytes past
    // the start of its block.
    let at = unsafe { address.cast::<u8>().sub(PHEADER) };
    // SAFETY: `palloc` wrote the block's size at its start.
    let total = unsafe { at.cast::<usize>().read() };
    // SAFETY: the layout `palloc` allocated the block under.
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(total, PHEADER) };
    // SAFETY: the block is live, zlib freeing it once.
    unsafe { std::alloc::dealloc(at, layout) };
}

/// Where zlib allocates a stream's state, as the allocations of the
/// filter that holds it are made ([`Pemalloc`]): the request's heap,
/// lent for each call, beside an [`Emalloc`], and the system's
/// ([`palloc`]) beside a [`Malloc`], a persistent stream's.
pub(crate) trait Heap: Pemalloc {
    /// `zalloc` and `zfree`.
    fn allocator() -> (z::alloc_func, z::free_func);

    /// `call`, the request's heap lent for its length where zlib
    /// allocates on it (ADR-0014); the `Err` is the memory limit's fatal
    /// an allocation met.
    fn serve(req: &mut Request, call: impl FnOnce() -> c_int) -> Result<c_int, Thrown>;
}

impl Heap for Emalloc {
    fn allocator() -> (z::alloc_func, z::free_func) {
        (alloc, free)
    }

    fn serve(req: &mut Request, call: impl FnOnce() -> c_int) -> Result<c_int, Thrown> {
        req.memory_mut().lend_heap(call)
    }
}

impl Heap for Malloc {
    fn allocator() -> (z::alloc_func, z::free_func) {
        (palloc, pfree)
    }

    fn serve(_req: &mut Request, call: impl FnOnce() -> c_int) -> Result<c_int, Thrown> {
        Ok(call())
    }
}

/// A `z_stream` before its init, with the allocator set and nothing
/// else, as `memset` then `Z.zalloc = php_zlib_alloc` leaves one; boxed,
/// since zlib's state keeps the stream's address. A persistent stream's
/// filter allocates on the system's ([`palloc`]).
#[allow(clippy::unnecessary_box_returns)]
fn fresh<A: Heap>() -> Box<z::z_stream> {
    let (zalloc, zfree) = A::allocator();
    Box::new(z::z_stream {
        next_in: std::ptr::null_mut(),
        avail_in: 0,
        total_in: 0,
        next_out: std::ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: std::ptr::null_mut(),
        state: std::ptr::null_mut(),
        zalloc,
        zfree,
        opaque: std::ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    })
}

/// `ZLIB_VERSION` of the header this build compiled against
/// ([`crate::header`]), NUL-terminated as the macro expands to a C string.
static HEADER_VERSION: [u8; crate::header::ZLIB_VERSION.len() + 1] =
    nul_terminated(crate::header::ZLIB_VERSION);

/// `text` and a NUL after it, in `N` bytes.
const fn nul_terminated<const N: usize>(text: &str) -> [u8; N] {
    let bytes = text.as_bytes();
    let mut out = [0; N];
    let mut at = 0;
    while at < bytes.len() && at + 1 < N {
        out[at] = bytes[at];
        at += 1;
    }
    out
}

/// `ZLIB_VERSION` as `zlib.h`'s `deflateInit2` and `inflateInit2` macros
/// pass it: the header's, which zlib's compatibility test compares with
/// its own, not the loaded library's `zlibVersion()` ([`version`]).
fn header_version() -> *const c_char {
    HEADER_VERSION.as_ptr().cast()
}

/// `sizeof(z_stream)`, which the init calls check against the library's.
fn stream_size() -> c_int {
    c_int::try_from(std::mem::size_of::<z::z_stream>()).unwrap_or(c_int::MAX)
}

/// A length zlib's `uInt` holds, the rest left for the next call.
fn avail(len: usize) -> c_uint {
    c_uint::try_from(len).unwrap_or(c_uint::MAX)
}

/// One `z_stream` and what zlib allocated for it on the request's heap
/// (design §5.13), or the system's where `A` is a persistent stream's.
/// Every call into zlib is made through [`Stream::call`], which lends the
/// request's heap for that call alone. A stream
/// is ended through [`Deflate::end`] or [`Inflate::end`], which gives its
/// state back; one dropped without is left to the request's end, whose
/// heap takes what zlib held, and zlib is not called again, since its
/// state is in memory the heap may already have given back.
///
/// ```php
/// $w1 = inflate_init(ZLIB_ENCODING_DEFLATE); $w2 = deflate_init(ZLIB_ENCODING_DEFLATE);
/// $m = memory_get_usage(); $i = inflate_init(ZLIB_ENCODING_DEFLATE); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $d = deflate_init(ZLIB_ENCODING_DEFLATE); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $s = deflate_init(ZLIB_ENCODING_RAW, ['level' => 9, 'memory' => 1, 'window' => 9]); echo memory_get_usage() - $m, "\n";
/// $c = deflate_add($d, str_repeat('abc', 1000), ZLIB_FINISH);
/// $m = memory_get_usage(); $o = inflate_add($i, $c); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($i, $d, $s); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $g = gzuncompress(gzcompress(str_repeat('x', 100))); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 8416
/// 270560
/// 11488
/// 3072
/// -290464
/// 128
/// ```
struct Stream<A> {
    z: Box<z::z_stream>,
    /// Where zlib allocates, so whether a call lends it the request's
    /// heap.
    heap: PhantomData<A>,
}

impl<A: Heap> Stream<A> {
    /// A stream before its init over `A`'s allocator.
    fn fresh() -> Stream<A> {
        Stream {
            z: fresh::<A>(),
            heap: PhantomData,
        }
    }

    /// `call` over the stream with `req`'s heap lent for its length
    /// (ADR-0014), so what zlib allocates and frees is the request's;
    /// the `Err` is the memory limit's fatal an allocation met, which
    /// ends the request as php's `emalloc` does.
    fn call(
        &mut self,
        req: &mut Request,
        call: impl FnOnce(*mut z::z_stream) -> c_int,
    ) -> Result<c_int, Thrown> {
        let stream = &raw mut *self.z;
        A::serve(req, || call(stream))
    }

    /// One step over `input` into `output` for their length alone.
    fn step(
        &mut self,
        req: &mut Request,
        input: &[u8],
        output: &mut [u8],
        call: impl FnOnce(*mut z::z_stream) -> c_int,
    ) -> Result<Step, Thrown> {
        let avail_in = avail(input.len());
        let avail_out = avail(output.len());
        self.z.next_in = input.as_ptr().cast_mut();
        self.z.avail_in = avail_in;
        self.z.next_out = output.as_mut_ptr();
        self.z.avail_out = avail_out;
        let code = self.call(req, call);
        let read = (avail_in - self.z.avail_in) as usize;
        let written = (avail_out - self.z.avail_out) as usize;
        self.z.next_in = std::ptr::null_mut();
        self.z.avail_in = 0;
        self.z.next_out = std::ptr::null_mut();
        self.z.avail_out = 0;
        Ok(Step {
            status: Status::of(code?),
            read,
            written,
        })
    }
}

/// A `z_stream` after `deflateInit2`, its state on the request's heap,
/// or the system's where `A` is a persistent stream's.
pub(crate) struct Deflate<A = Emalloc>(Stream<A>);

// SAFETY: the stream and the state zlib hangs off it are reached only
// through this value, which one request owns and serves on one thread at
// a time (ADR-0005); zlib keeps no thread-local state.
unsafe impl<A: Send> Send for Deflate<A> {}

// A persistent stream's filter's: zlib's state is on the system
// allocator ([`palloc`]), so no request's heap is reachable from it, and
// it is kept with the stream across requests, one at a time, as `Send`
// says.
impl EngineState for Deflate<Malloc> {}

impl<A: Heap> Deflate<A> {
    /// `deflateInit2(&Z, level, Z_DEFLATED, window, memory, strategy)`:
    /// the stream, or zlib's refusal; the outer `Err` is the memory
    /// limit's fatal. zlib is handed the header's `ZLIB_VERSION`
    /// ([`header_version`]) as the macro hands it `deflateInit2_`, so its
    /// compatibility test answers `Z_VERSION_ERROR` where the library
    /// loaded is of another major than the header compiled against.
    pub(crate) fn new(
        req: &mut Request,
        (level, window, memory, strategy): (i64, i64, i64, i64),
    ) -> Result<Result<Deflate<A>, Status>, Thrown> {
        let int = |n: i64| c_int::try_from(n).map_err(|_| Status::StreamError);
        let (Ok(level), Ok(window), Ok(memory), Ok(strategy)) =
            (int(level), int(window), int(memory), int(strategy))
        else {
            return Ok(Err(Status::StreamError));
        };
        let mut stream = Stream::fresh();
        let code = stream.call(req, |z| {
            // SAFETY: the stream is fresh with its allocator set, the
            // version is the header's static C string and the size the
            // struct's.
            unsafe {
                z::deflateInit2_(
                    z,
                    level,
                    z::Z_DEFLATED,
                    window,
                    memory,
                    strategy,
                    header_version(),
                    stream_size(),
                )
            }
        })?;
        Ok(match Status::of(code) {
            Status::Ok => Ok(Deflate(stream)),
            refused => Err(refused),
        })
    }

    /// `deflate(&Z, flush)` over `input` into `output`.
    pub(crate) fn deflate(
        &mut self,
        req: &mut Request,
        input: &[u8],
        output: &mut [u8],
        flush: Flush,
    ) -> Result<Step, Thrown> {
        let flush = flush.code();
        // SAFETY: the stream was initialised by deflateInit2, and step
        // points it at the two slices for this call alone.
        self.0
            .step(req, input, output, |z| unsafe { z::deflate(z, flush) })
    }

    /// `deflateSetDictionary`.
    pub(crate) fn set_dictionary(
        &mut self,
        req: &mut Request,
        dictionary: &[u8],
    ) -> Result<Status, Thrown> {
        // SAFETY: an initialised stream, and the dictionary read for the
        // length given during the call alone.
        let code = self.0.call(req, |z| unsafe {
            z::deflateSetDictionary(z, dictionary.as_ptr(), avail(dictionary.len()))
        })?;
        Ok(Status::of(code))
    }

    /// `deflateReset`.
    pub(crate) fn reset(&mut self, req: &mut Request) -> Result<(), Thrown> {
        // SAFETY: an initialised stream.
        self.0
            .call(req, |z| unsafe { z::deflateReset(z) })
            .map(|_| ())
    }

    /// `deflateEnd`: the state given back to the request's heap.
    pub(crate) fn end(mut self, req: &mut Request) {
        // SAFETY: an initialised stream, ended once, here, as `self` is
        // given up.
        self.0.call(req, |z| unsafe { z::deflateEnd(z) }).ok();
    }
}

/// A `z_stream` after `inflateInit2`, its state on the request's heap,
/// or the system's where `A` is a persistent stream's.
pub(crate) struct Inflate<A = Emalloc>(Stream<A>);

// SAFETY: as Deflate's: one owner, one thread at a time (ADR-0005).
unsafe impl<A: Send> Send for Inflate<A> {}

// As `Deflate<Malloc>`: zlib's state on the system allocator.
impl EngineState for Inflate<Malloc> {}

impl<A: Heap> Inflate<A> {
    /// `inflateInit2(&Z, window)`: the stream, or zlib's refusal; the
    /// outer `Err` is the memory limit's fatal. zlib is handed the
    /// header's `ZLIB_VERSION` ([`header_version`]) as the macro hands it
    /// `inflateInit2_`, and answers `Z_VERSION_ERROR` where the library
    /// loaded is of another major.
    pub(crate) fn new(
        req: &mut Request,
        window: i64,
    ) -> Result<Result<Inflate<A>, Status>, Thrown> {
        let Ok(window) = c_int::try_from(window) else {
            return Ok(Err(Status::StreamError));
        };
        let mut stream = Stream::fresh();
        // SAFETY: the stream is fresh with its allocator set, the version
        // is the header's static C string and the size the struct's.
        let code = stream.call(req, |z| unsafe {
            z::inflateInit2_(z, window, header_version(), stream_size())
        })?;
        Ok(match Status::of(code) {
            Status::Ok => Ok(Inflate(stream)),
            refused => Err(refused),
        })
    }

    /// `inflate(&Z, flush)` over `input` into `output`.
    pub(crate) fn inflate(
        &mut self,
        req: &mut Request,
        input: &[u8],
        output: &mut [u8],
        flush: Flush,
    ) -> Result<Step, Thrown> {
        let flush = flush.code();
        // SAFETY: the stream was initialised by inflateInit2, and step
        // points it at the two slices for this call alone.
        self.0
            .step(req, input, output, |z| unsafe { z::inflate(z, flush) })
    }

    /// `inflateSetDictionary`.
    pub(crate) fn set_dictionary(
        &mut self,
        req: &mut Request,
        dictionary: &[u8],
    ) -> Result<Status, Thrown> {
        // SAFETY: an initialised stream, and the dictionary read for the
        // length given during the call alone.
        let code = self.0.call(req, |z| unsafe {
            z::inflateSetDictionary(z, dictionary.as_ptr(), avail(dictionary.len()))
        })?;
        Ok(Status::of(code))
    }

    /// `inflateReset`.
    pub(crate) fn reset(&mut self, req: &mut Request) -> Result<(), Thrown> {
        // SAFETY: an initialised stream.
        self.0
            .call(req, |z| unsafe { z::inflateReset(z) })
            .map(|_| ())
    }

    /// `Z.total_in`: every byte read since the init or the last reset.
    pub(crate) fn total_in(&self) -> u64 {
        #[allow(clippy::useless_conversion)] // `uLong` is `u64` on the 64-bit targets
        u64::from(self.0.z.total_in)
    }

    /// `inflateEnd`: the state given back to the request's heap.
    pub(crate) fn end(mut self, req: &mut Request) {
        // SAFETY: an initialised stream, ended once, here, as `self` is
        // given up.
        self.0.call(req, |z| unsafe { z::inflateEnd(z) }).ok();
    }
}

/// A `gzFile` from `gzdopen`, closed by `gzclose` when dropped.
pub(crate) struct GzFile(NonNull<z::gzFile_s>);

// SAFETY: the gzFile and the descriptor it owns are reached only through
// this value, which one request owns (ADR-0005).
unsafe impl Send for GzFile {}

impl GzFile {
    /// `gzdopen(dup(fd), mode)`, the `mode` read up to its first NUL as the
    /// C string php hands on; `None` where zlib refused it.
    pub(crate) fn dopen(fd: RawFd, mode: &[u8]) -> Option<GzFile> {
        let mut mode = php_ext::coerce::c_str(mode).to_vec();
        mode.push(0);
        // SAFETY: the descriptor belongs to the open stream the caller
        // holds, which outlives this call.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let raw = borrowed.try_clone_to_owned().ok()?.into_raw_fd();
        // SAFETY: a descriptor this call owns and a NUL-terminated mode.
        let file = unsafe { z::gzdopen(raw, mode.as_ptr().cast()) };
        if let Some(file) = NonNull::new(file) {
            return Some(GzFile(file));
        }
        // SAFETY: gzdopen took nothing on failure, so the duplicate is
        // still this call's to close.
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
        None
    }

    /// `gzread`: how many bytes it gave, negative for an error.
    pub(crate) fn read(&mut self, buf: &mut [u8]) -> i64 {
        // SAFETY: an open gzFile, writing at most the length given.
        i64::from(unsafe { z::gzread(self.0.as_ptr(), buf.as_mut_ptr().cast(), avail(buf.len())) })
    }

    /// `gzwrite`: how many bytes it took, zero or less for an error.
    pub(crate) fn write(&mut self, buf: &[u8]) -> i64 {
        // SAFETY: an open gzFile, reading at most the length given.
        i64::from(unsafe { z::gzwrite(self.0.as_ptr(), buf.as_ptr().cast(), avail(buf.len())) })
    }

    /// `gzeof`.
    pub(crate) fn eof(&mut self) -> bool {
        // SAFETY: an open gzFile.
        unsafe { z::gzeof(self.0.as_ptr()) != 0 }
    }

    /// `gzseek`: the new offset, negative where it failed.
    #[allow(irrefutable_let_patterns, clippy::useless_conversion)] // `z_off_t` is `long`, `i64` on the 64-bit targets
    pub(crate) fn seek(&mut self, offset: i64, whence: i32) -> i64 {
        let Ok(offset) = z::z_off_t::try_from(offset) else {
            return -1;
        };
        // SAFETY: an open gzFile.
        i64::from(unsafe { z::gzseek(self.0.as_ptr(), offset, whence) })
    }

    /// `gzflush(file, Z_SYNC_FLUSH)`: `true` for `Z_OK`.
    pub(crate) fn flush(&mut self) -> bool {
        // SAFETY: an open gzFile.
        unsafe { z::gzflush(self.0.as_ptr(), z::Z_SYNC_FLUSH) == z::Z_OK }
    }

    /// `gzsetparams(file, level, Z_DEFAULT_STRATEGY)`: `true` for `Z_OK`.
    pub(crate) fn set_level(&mut self, level: i64) -> bool {
        let Ok(level) = c_int::try_from(level) else {
            return false;
        };
        // SAFETY: an open gzFile.
        unsafe { z::gzsetparams(self.0.as_ptr(), level, z::Z_DEFAULT_STRATEGY) == z::Z_OK }
    }

    /// `gzerror`'s number: whether the last error was `Z_ERRNO`, a
    /// system call's, which php reports with `errno`.
    pub(crate) fn failed_errno(&mut self) -> bool {
        let mut code: c_int = 0;
        // SAFETY: an open gzFile, and a number for gzerror to write.
        unsafe { z::gzerror(self.0.as_ptr(), &raw mut code) };
        code == z::Z_ERRNO
    }
}

impl Drop for GzFile {
    fn drop(&mut self) {
        // SAFETY: an open gzFile, closed once, here, with the duplicate
        // descriptor it owns.
        unsafe { z::gzclose(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests;
