//! The binding held to what the linked zlib allocates and answers: the
//! header version its init calls are handed, the window `inflate` defers
//! until the first call needs it, what a refused allocation leaves
//! behind, and `compress` into a bound no larger than its input, as
//! mysqlnd's `mysqlnd_deflate` calls it.

use std::alloc::{Layout, alloc, dealloc};
use std::collections::BTreeMap;
use std::ffi::{CStr, c_void};

use super::z;

/// The version handed to the init calls is the header's, as a C
/// string with one NUL, at its end.
#[test]
fn the_init_calls_are_handed_the_header_s_version() {
    let handed = CStr::from_bytes_with_nul(&super::HEADER_VERSION);
    assert_eq!(
        handed.map(CStr::to_bytes),
        Ok(crate::header::ZLIB_VERSION.as_bytes())
    );
}

/// What zlib asked of a stream's allocator: the blocks it holds, by
/// address, how many it asked for, the ask that is refused, counting
/// from one (zero refuses none), and whether it handed back a block it
/// was never given. The callbacks record rather than panic, since a
/// panic cannot unwind out of an `extern "C"` function.
#[derive(Default)]
struct Allocations {
    live: BTreeMap<usize, Layout>,
    attempts: usize,
    refuse: usize,
    stray_free: bool,
}

extern "C" fn allocate(opaque: *mut c_void, items: z::uInt, size: z::uInt) -> *mut c_void {
    // SAFETY: each test hands its own live Allocations as the opaque word
    // and zlib calls this synchronously, on the test's thread, during a
    // call the test makes.
    let allocations = unsafe { &mut *opaque.cast::<Allocations>() };
    allocations.attempts += 1;
    let bytes = items as usize * size as usize;
    let Ok(layout) = Layout::from_size_align(bytes, 16) else {
        return std::ptr::null_mut();
    };
    if allocations.attempts == allocations.refuse || bytes == 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: the layout is nonzero in size and aligned.
    let address = unsafe { alloc(layout) };
    if !address.is_null() {
        allocations.live.insert(address as usize, layout);
    }
    address.cast()
}

extern "C" fn release(opaque: *mut c_void, address: *mut c_void) {
    // SAFETY: the Allocations `allocate` was handed, as there.
    let allocations = unsafe { &mut *opaque.cast::<Allocations>() };
    let Some(layout) = allocations.live.remove(&(address as usize)) else {
        allocations.stray_free = true;
        return;
    };
    // SAFETY: `allocate` made this block with this layout, and its entry
    // is removed, so it is freed once.
    unsafe { dealloc(address.cast(), layout) };
}

/// A fresh stream whose allocator records into `allocations`.
#[allow(clippy::unnecessary_box_returns)]
fn stream(allocations: &mut Allocations) -> Box<z::z_stream> {
    let mut stream = super::fresh(false);
    stream.zalloc = allocate;
    stream.zfree = release;
    stream.opaque = std::ptr::from_mut(allocations).cast();
    stream
}

/// `inflateInit2` allocates the state alone; the first `inflate` that
/// writes bytes out allocates the window, `1 << windowBits` bytes, and
/// `inflateReset` keeps both, as `zlib_reset_inflate` relies on.
/// `inflateEnd` gives back everything.
#[test]
fn inflate_allocates_its_window_on_first_use_and_keeps_it_on_reset() {
    // A raw deflate stream, sync-flushed, that opens with `the qui`.
    let input = [
        0x2b, 0xc9, 0x48, 0x55, 0x28, 0x2c, 0xcd, 0x4c, 0xce, 0x56, 0x48, 0x2a, 0xca, 0x2f, 0xcf,
        0x53, 0x48, 0xcb, 0xaf, 0x50, 0xc8, 0x2a, 0xcd, 0xcd, 0x2d, 0x28, 0x56, 0xc8, 0x2f, 0x4b,
        0x2d, 0x52, 0x28, 0xc9, 0x48, 0x55, 0xc8, 0x49, 0xac, 0xaa, 0x54, 0x48, 0xc9, 0x4f, 0xe7,
        0x2a, 0x19, 0x15, 0x3d, 0x8a, 0xaa, 0x51, 0xd1, 0xa3, 0xa2, 0x47, 0x45, 0xcf, 0x4a, 0x34,
        0x00,
    ];
    for window in [8, 15] {
        let mut allocations = Allocations::default();
        let mut stream = stream(&mut allocations);
        // SAFETY: the stream is boxed, so its address is fixed, and its
        // allocator's Allocations lives to the end of the iteration.
        let code = unsafe {
            z::inflateInit2_(
                &raw mut *stream,
                -window,
                super::header_version(),
                super::stream_size(),
            )
        };
        assert_eq!(code, z::Z_OK);
        assert_eq!(allocations.live.len(), 1);
        let mut output = [0; 7];
        stream.next_in = input.as_ptr().cast_mut();
        stream.avail_in = super::avail(input.len());
        stream.next_out = output.as_mut_ptr();
        stream.avail_out = super::avail(output.len());
        // SAFETY: both buffers live through the call, at the stated lengths.
        let code = unsafe { z::inflate(&raw mut *stream, z::Z_SYNC_FLUSH) };
        assert_eq!(code, z::Z_OK);
        assert_eq!(&output, b"the qui");
        assert_eq!(allocations.live.len(), 2);
        assert!(
            allocations
                .live
                .values()
                .any(|layout| layout.size() == 1 << window)
        );
        let attempts = allocations.attempts;
        // SAFETY: an initialised stream, with no call outstanding.
        assert_eq!(unsafe { z::inflateReset(&raw mut *stream) }, z::Z_OK);
        assert_eq!(allocations.attempts, attempts);
        assert_eq!(allocations.live.len(), 2);
        // SAFETY: an initialised stream, ended once, its Allocations live.
        assert_eq!(unsafe { z::inflateEnd(&raw mut *stream) }, z::Z_OK);
        assert!(allocations.live.is_empty());
        assert!(!allocations.stray_free);
    }
}

/// `deflateInit2` makes five allocations; where any of them is refused
/// the call answers `Z_MEM_ERROR` and has given back the ones before it,
/// so a refused init leaves nothing for `deflateEnd`, as
/// `php_zlib_deflate_init` assumes when it frees its own struct alone.
#[test]
fn a_refused_deflate_init_gives_back_what_it_allocated() {
    for refuse in 0..=5 {
        let mut allocations = Allocations {
            refuse,
            ..Allocations::default()
        };
        let mut stream = stream(&mut allocations);
        // SAFETY: a fixed stream address, its allocator's Allocations live, the
        // header's version and size; a refused allocation answers null.
        let code = unsafe {
            z::deflateInit2_(
                &raw mut *stream,
                6,
                z::Z_DEFLATED,
                -15,
                8,
                0,
                super::header_version(),
                super::stream_size(),
            )
        };
        if refuse == 0 {
            assert_eq!(code, z::Z_OK);
            assert_eq!(allocations.live.len(), 5);
            // SAFETY: an initialised stream, ended once, its Allocations live.
            assert_eq!(unsafe { z::deflateEnd(&raw mut *stream) }, z::Z_OK);
        } else {
            assert_eq!(code, z::Z_MEM_ERROR);
        }
        assert!(allocations.live.is_empty());
        assert!(!allocations.stray_free);
    }
}

/// `compress` into a buffer as long as its input, as
/// `mysqlnd_deflate` bounds a packet's: text fits, answers `Z_OK` and
/// says it wrote fewer bytes than it read, and bytes that do not
/// compress answer `Z_BUF_ERROR` rather than write past the bound.
#[test]
fn compress_into_the_input_s_length_refuses_what_does_not_shrink() {
    let text = b"the quick brown fox jumps over the lazy dog\n".repeat(32);
    let mut word: u32 = 0x1234_5678;
    let noise: Vec<u8> = (0..512)
        .map(|_| {
            word = word.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            word.to_be_bytes()[0]
        })
        .collect();
    for (payload, expected) in [(text, z::Z_OK), (noise, z::Z_BUF_ERROR)] {
        // The four bytes of the packet's own header, which mysqlnd
        // compresses with its payload.
        let mut input = vec![0; 4];
        input.extend_from_slice(&payload);
        let mut output = vec![0; input.len()];
        let mut length = z::uLongf::try_from(input.len()).unwrap();
        // SAFETY: both buffers live through the call, at the stated
        // lengths; `compress` allocates on zlib's own allocator and frees
        // before it returns.
        let code = unsafe {
            z::compress(
                output.as_mut_ptr(),
                &raw mut length,
                input.as_ptr(),
                z::uLong::try_from(input.len()).unwrap(),
            )
        };
        assert_eq!(code, expected);
        if code == z::Z_OK {
            assert!(usize::try_from(length).unwrap() < input.len());
        }
    }
}
