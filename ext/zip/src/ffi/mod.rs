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

/// What one `deflate` call came to, as `zip_algorithm_deflate.c`'s
/// `process` reads its return.
pub(crate) enum Deflated {
    /// `Z_OK`: more to do.
    More,
    /// `Z_BUF_ERROR` with no input left: the next input is wanted.
    NeedData,
    /// `Z_STREAM_END`.
    End,
}

/// `zip_algorithm_deflate.c`'s compressing state: a `z_stream` after
/// `deflateInit2(level, Z_DEFLATED, -MAX_WBITS, mem_level,
/// Z_DEFAULT_STRATEGY)`, boxed, since zlib's state names the stream it
/// was made for, and the input it is reading, which it owns while zlib
/// reads it; ended with its owner.
pub(crate) struct Deflate {
    stream: Box<z::z_stream>,
    input: Vec<u8>,
}

impl Drop for Deflate {
    fn drop(&mut self) {
        // SAFETY: the stream was initialised by `deflateInit2_` and is
        // ended once, here.
        unsafe { z::deflateEnd(&raw mut *self.stream) };
    }
}

impl Deflate {
    /// `start`: the stream at `level` and `mem_level`, or the code zlib
    /// refused it with.
    pub(crate) fn new(level: c_int, mem_level: c_int) -> Result<Deflate, c_int> {
        let mut stream = Box::new(z::z_stream {
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
        });
        // The header's version, as libzip's `deflateInit2` macro hands it,
        // which zlib's compatibility test holds to the library it loads.
        let version = &raw const php_zip_zlib_version;
        let size = c_int::try_from(std::mem::size_of::<z::z_stream>()).unwrap_or(c_int::MAX);
        // SAFETY: a fresh stream with its allocator set, the header's
        // NUL-terminated version string, which lives as long as the
        // program, and the struct's size.
        let code = unsafe {
            z::deflateInit2_(
                &raw mut *stream,
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
            // `deflateInit2_` freed what it took, and the stream is not
            // handed out, so no `deflateEnd` follows.
            return Err(code);
        }
        Ok(Deflate {
            stream,
            input: Vec::new(),
        })
    }

    /// `input`: `data` the next input, read from a copy the stream owns;
    /// zlib has read what it was given before, as `process` asks for more
    /// only then.
    pub(crate) fn input(&mut self, data: &[u8]) {
        self.input = data.to_vec();
        self.stream.next_in = self.input.as_mut_ptr();
        self.stream.avail_in = c_uint::try_from(self.input.len()).unwrap_or(c_uint::MAX);
    }

    /// `process`: one `deflate` into `out`, `Z_FINISH` once the input has
    /// ended, and how much it wrote; the code of any other return.
    pub(crate) fn process(
        &mut self,
        out: &mut [u8],
        finish: bool,
    ) -> Result<(Deflated, usize), c_int> {
        let room = c_uint::try_from(out.len()).unwrap_or(c_uint::MAX);
        self.stream.next_out = out.as_mut_ptr();
        self.stream.avail_out = room;
        let flush = if finish { z::Z_FINISH } else { z::Z_NO_FLUSH };
        // SAFETY: an initialised stream, reading the rest of `self.input`,
        // which zlib's `next_in` points into and which is not changed
        // while `avail_in` is not zero, and writing into `out`, which
        // outlives the call.
        let code = unsafe { z::deflate(&raw mut *self.stream, flush) };
        let written = (room - self.stream.avail_out) as usize;
        self.stream.next_out = std::ptr::null_mut();
        match code {
            z::Z_OK => Ok((Deflated::More, written)),
            z::Z_STREAM_END => Ok((Deflated::End, written)),
            z::Z_BUF_ERROR if self.stream.avail_in == 0 => Ok((Deflated::NeedData, written)),
            code => Err(code),
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
