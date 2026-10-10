//! The binding to zlib's deflate (design §7.2, `unsafe.homes`'
//! `ext/*/src/ffi/`), the one this crate has: the platform's library,
//! linked through the `libz-sys` ext/zlib binds, and the one home of this
//! crate's `unsafe`. php's archives carry libzip's deflate bytes, which
//! follow the linked zlib's algorithm at the memLevel libzip asks for,
//! 9, where the zip crate's encoder fixes 8 (#1657, #1651). Everything
//! else the crate reads and writes stays pure Rust.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{c_char, c_int, c_uint, c_void};

use libz_sys as z;

unsafe extern "C" {
    /// `ZLIB_VERSION` of the `zlib.h` this crate compiled against, the C
    /// constant `build.rs` compiles, NUL-terminated.
    static php_zip_zlib_version: c_char;
}

/// `BUFSIZE`, what `zip_source_compress` reads its source in and
/// `copy_data` reads the compressed stream out in.
const BUFSIZE: usize = 8192;

/// `-MAX_WBITS`: a raw deflate stream, no zlib header.
const RAW_WINDOW: c_int = -15;

/// zlib's allocator as libzip leaves it, `Z_NULL`: `zcalloc`, which is
/// `malloc(items * size)` wherever `uInt` is wider than two bytes and
/// zeroes nothing (zlib's `zutil.c:300-304`, as libz-sys vendors it and
/// the oracle's 1.2.12 has it). Two `uInt`s multiply inside a 64-bit
/// `size_t`.
extern "C" fn alloc(_opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void {
    // SAFETY: `malloc` takes any size and answers null or memory of its
    // own, which zlib frees through `free`.
    unsafe { libc::malloc(items as usize * size as usize) }
}

/// `zcfree`, `free` of what [`alloc`] gave.
extern "C" fn free(_opaque: *mut c_void, address: *mut c_void) {
    // SAFETY: `address` is one [`alloc`] answered, which zlib frees once.
    unsafe { libc::free(address) }
}

/// A `z_stream` after `deflateInit2`, ended with its owner; boxed, since
/// zlib's state names the stream it was made for.
struct Stream(Box<z::z_stream>);

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream was initialised by `deflateInit2_` and is
        // ended once, here.
        unsafe { z::deflateEnd(&raw mut *self.0) };
    }
}

/// `zip_algorithm_deflate.c` over `data`: `deflateInit2(level,
/// Z_DEFLATED, -MAX_WBITS, mem_level, Z_DEFAULT_STRATEGY)`, the input
/// handed over `BUFSIZE` bytes at a time, as `zip_source_compress` reads
/// it, and deflated with `Z_NO_FLUSH` until the input ends, then
/// `Z_FINISH`; the raw stream, or `None` where zlib refused.
pub(crate) fn deflate(data: &[u8], level: c_int, mem_level: c_int) -> Option<Vec<u8>> {
    let mut stream = Stream(Box::new(z::z_stream {
        next_in: std::ptr::null_mut(),
        avail_in: 0,
        total_in: 0,
        next_out: std::ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: std::ptr::null_mut(),
        state: std::ptr::null_mut(),
        zalloc: alloc,
        zfree: free,
        opaque: std::ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    }));
    // The header's version, as libzip's `deflateInit2` macro hands it,
    // which zlib's compatibility test holds to the library it loads.
    let version = &raw const php_zip_zlib_version;
    let size = c_int::try_from(std::mem::size_of::<z::z_stream>()).ok()?;
    // SAFETY: a fresh stream with its allocator set, the header's
    // NUL-terminated version string, which lives as long as the
    // program, and the struct's size.
    let code = unsafe {
        z::deflateInit2_(
            &raw mut *stream.0,
            level,
            z::Z_DEFLATED,
            RAW_WINDOW,
            mem_level,
            z::Z_DEFAULT_STRATEGY,
            version,
            size,
        )
    };
    if code != z::Z_OK {
        // `deflateInit2_` freed what it took; the `deflateEnd` the drop
        // makes of a stream with no state is zlib's `Z_STREAM_ERROR`.
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 2 + BUFSIZE);
    let mut buffer = [0u8; BUFSIZE];
    let mut chunks = data.chunks(BUFSIZE);
    let mut ended = data.is_empty();
    loop {
        if stream.0.avail_in == 0 && !ended {
            match chunks.next() {
                Some(chunk) => {
                    stream.0.next_in = chunk.as_ptr().cast_mut();
                    stream.0.avail_in = c_uint::try_from(chunk.len()).ok()?;
                }
                None => ended = true,
            }
            if stream.0.avail_in == 0 {
                continue;
            }
        }
        stream.0.next_out = buffer.as_mut_ptr();
        stream.0.avail_out = c_uint::try_from(BUFSIZE).ok()?;
        let flush = if ended { z::Z_FINISH } else { z::Z_NO_FLUSH };
        // SAFETY: an initialised stream, reading the chunk of `data` set
        // above, which outlives the call, and writing into `buffer`.
        let code = unsafe { z::deflate(&raw mut *stream.0, flush) };
        let written = BUFSIZE - stream.0.avail_out as usize;
        out.extend_from_slice(buffer.get(..written)?);
        match code {
            z::Z_STREAM_END => return Some(out),
            z::Z_OK => {}
            z::Z_BUF_ERROR if stream.0.avail_in == 0 => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::{php_zip_zlib_version, z};

    /// What `deflateInit2_` is handed is the header's `ZLIB_VERSION`, a
    /// dotted version of the major the loaded library answers, the one
    /// byte zlib's compatibility test compares.
    #[test]
    fn deflate_init_is_handed_the_header_s_version() {
        // SAFETY: the constant `build.rs` compiled, a NUL-terminated C
        // string that lives as long as the program.
        let header = unsafe { CStr::from_ptr(&raw const php_zip_zlib_version) };
        // SAFETY: `zlibVersion` answers the library's static string.
        let loaded = unsafe { z::zlibVersion() };
        // SAFETY: that string is NUL-terminated and lives as long as the
        // library.
        let loaded = unsafe { CStr::from_ptr(loaded) };
        let header = header.to_bytes();
        let dotted = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-');
        assert!(
            !header.is_empty() && header.iter().all(dotted),
            "{header:?}"
        );
        assert_eq!(header.first(), loaded.to_bytes().first());
    }
}
