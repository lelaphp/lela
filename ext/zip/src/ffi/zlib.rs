//! zlib's deflate, the platform's library linked through the `libz-sys`
//! ext/zlib binds. php's archives carry libzip's deflate bytes, which
//! follow the linked zlib's algorithm at the memLevel libzip passes to
//! `deflateInit2`, `MAX_MEM_LEVEL`, 9.

use std::ffi::{c_char, c_int, c_uint, c_void};

use libz_sys as z;

use crate::codec::Turn;

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

/// `zip_algorithm_deflate.c`'s compressing state: a `z_stream` after
/// `deflateInit2(level, Z_DEFLATED, -MAX_WBITS, mem_level,
/// Z_DEFAULT_STRATEGY)`, boxed, since zlib's state names the stream it
/// was made for; ended with its owner.
pub(crate) struct Deflate {
    stream: Box<z::z_stream>,
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
        Ok(Deflate { stream })
    }

    /// `process`: one `deflate` of `input` into `out`, `Z_FINISH` once
    /// the input has ended: what it came to, how much it took and how
    /// much it made; the code of any other return.
    pub(crate) fn process(
        &mut self,
        input: &[u8],
        out: &mut [u8],
        finish: bool,
    ) -> Result<(Turn, usize, usize), c_int> {
        let given = c_uint::try_from(input.len()).unwrap_or(c_uint::MAX);
        let room = c_uint::try_from(out.len()).unwrap_or(c_uint::MAX);
        // zlib reads through `next_in` and never writes it.
        self.stream.next_in = input.as_ptr().cast_mut();
        self.stream.avail_in = given;
        self.stream.next_out = out.as_mut_ptr();
        self.stream.avail_out = room;
        let flush = if finish { z::Z_FINISH } else { z::Z_NO_FLUSH };
        // SAFETY: an initialised stream, reading `input` and writing
        // `out`, which outlive the call; the pointers are cleared after
        // it, so zlib keeps none.
        let code = unsafe { z::deflate(&raw mut *self.stream, flush) };
        let used = (given - self.stream.avail_in) as usize;
        let written = (room - self.stream.avail_out) as usize;
        self.stream.next_in = std::ptr::null_mut();
        self.stream.avail_in = 0;
        self.stream.next_out = std::ptr::null_mut();
        self.stream.avail_out = 0;
        match code {
            z::Z_OK => Ok((Turn::More, used, written)),
            z::Z_STREAM_END => Ok((Turn::End, used, written)),
            // `Z_BUF_ERROR` with no input left: the next input is wanted.
            z::Z_BUF_ERROR if used == input.len() => Ok((Turn::NeedData, used, written)),
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
