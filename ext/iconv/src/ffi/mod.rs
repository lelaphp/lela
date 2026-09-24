//! The host's `iconv(3)` (design §7.2, "posix, pcntl, sockets, iconv:
//! `libc`, thin"; `unsafe.homes`' `ext/*/src/ffi/`): `iconv_open`, one
//! `iconv` call at a time and `iconv_close`, which `Drop` makes. A
//! converter is opened for one call of a native and closed before it
//! returns, never shared, so no request sees another's shift state. The
//! oracle's `--ri iconv` reads libiconv 1.11, the macOS system library,
//! which is the library linked here.
#![allow(unsafe_code)]
// `libc` marks its iconv bindings deprecated, meaning to drop them at 1.0.
#![allow(deprecated)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::CString;

/// `errno` as `iconv` leaves it: `EINVAL`, `EILSEQ` and `E2BIG`.
pub(super) const EINVAL: i32 = libc::EINVAL;
pub(super) const EILSEQ: i32 = libc::EILSEQ;
pub(super) const E2BIG: i32 = libc::E2BIG;

/// One `iconv_t` converter, closed when dropped.
pub(super) struct Cd(libc::iconv_t);

impl Cd {
    /// `iconv_open(to, from)`: the converter, or `errno`.
    pub(super) fn open(to: &[u8], from: &[u8]) -> Result<Cd, i32> {
        let (Ok(to), Ok(from)) = (CString::new(to), CString::new(from)) else {
            return Err(EINVAL);
        };
        // SAFETY: both are NUL-terminated strings that outlive the call,
        // which reads them and keeps neither.
        let cd = unsafe { libc::iconv_open(to.as_ptr(), from.as_ptr()) };
        if cd as isize == -1 {
            return Err(errno());
        }
        Ok(Cd(cd))
    }

    /// One `iconv(cd, &in, &in_left, &out, &out_left)`: `input` is
    /// advanced past what was read (`None` is php's `NULL`, the flush of
    /// the shift state) and the answer is the bytes written into `out`
    /// with `iconv`'s return, `Err(errno)` for its `(size_t)-1`.
    pub(super) fn step(
        &mut self,
        input: Option<&mut &[u8]>,
        out: &mut [u8],
    ) -> (usize, Result<usize, i32>) {
        prime();
        let mut out_p = out.as_mut_ptr().cast::<libc::c_char>();
        let mut out_left = out.len();
        let result = match input {
            Some(input) => {
                let mut in_p = input.as_ptr().cast::<libc::c_char>().cast_mut();
                let mut in_left = input.len();
                // SAFETY: `in_p` and `out_p` point at `in_left` readable and
                // `out_left` writable bytes, which `iconv` stays within and
                // advances the pointers and counts over.
                let result = unsafe {
                    libc::iconv(
                        self.0,
                        &raw mut in_p,
                        &raw mut in_left,
                        &raw mut out_p,
                        &raw mut out_left,
                    )
                };
                let read = input.len() - in_left;
                *input = &input[read..];
                result
            }
            None => {
                // SAFETY: null input asks `iconv` to write the shift state's
                // reset into the `out_left` writable bytes at `out_p`.
                unsafe {
                    libc::iconv(
                        self.0,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &raw mut out_p,
                        &raw mut out_left,
                    )
                }
            }
        };
        let written = out.len() - out_left;
        if result == usize::MAX {
            (written, Err(errno()))
        } else {
            (written, Ok(result))
        }
    }
}

impl Cd {
    /// `php_iconv_string`'s buffer protocol: the whole of `input`, room
    /// for `input.len() + 32` bytes grown by the input's length at each
    /// `E2BIG`, then the shift state flushed, growing by 16. The bytes
    /// converted come back beside the `errno` of the call that stopped.
    pub(super) fn convert(&mut self, input: &[u8]) -> (Vec<u8>, Result<(), i32>) {
        let mut out = vec![0u8; input.len() + 32];
        let (mut used, mut rest, mut result) = (0, input, Ok(0));
        while !rest.is_empty() {
            let (written, step) = self.step(Some(&mut rest), &mut out[used..]);
            used += written;
            result = step;
            if result == Err(E2BIG) && !rest.is_empty() {
                out.resize(out.len() + input.len(), 0);
                continue;
            }
            break;
        }
        while result.is_ok() {
            let (written, step) = self.step(None, &mut out[used..]);
            used += written;
            result = step;
            if result != Err(E2BIG) {
                break;
            }
            out.resize(out.len() + 16, 0);
        }
        out.truncate(used);
        (out, result.map(|_| ()))
    }

    /// `_php_iconv_appendl`: `input` onto `out` in room of 128 bytes and
    /// doubling, `None` flushing the shift state; the `errno` of a call
    /// that failed other than for room.
    pub(super) fn append(&mut self, input: Option<&[u8]>, out: &mut Vec<u8>) -> Result<(), i32> {
        let mut room = 128;
        let mut rest = input.unwrap_or_default();
        loop {
            let mut chunk = vec![0u8; room];
            let (written, result) = self.step(input.map(|_| &mut rest), &mut chunk);
            out.extend_from_slice(&chunk[..written]);
            match result {
                Err(E2BIG) => {}
                Err(errno) => return Err(errno),
                Ok(_) if input.is_none() => return Ok(()),
                Ok(_) => {}
            }
            if input.is_some() && rest.is_empty() {
                return Ok(());
            }
            room <<= 1;
        }
    }

    /// The walk of `_php_iconv_strlen`, `_php_iconv_substr` and
    /// `_php_iconv_strpos`: `input` out through room of `width` bytes a
    /// call, a last call with no input flushing the shift state, each
    /// call's bytes handed to `each` with the walk's `errno` until it
    /// answers `false` or a call writes nothing. The answer is the `errno`
    /// the last failing call left, which strlen and substr read, and that
    /// of the last failing call that wrote, which is all strpos checks.
    pub(super) fn walk(
        &mut self,
        input: &[u8],
        width: usize,
        mut each: impl FnMut(&[u8], &mut i32) -> bool,
    ) -> (i32, i32) {
        let (mut rest, mut more, mut errno, mut wrote) = (input, !input.is_empty(), 0, 0);
        while more {
            let mut buf = [0u8; 8];
            more = !rest.is_empty();
            let (written, result) = self.step(more.then_some(&mut rest), &mut buf[..width]);
            if let Err(e) = result {
                errno = e;
            }
            if written == 0 {
                break;
            }
            if let Err(e) = result {
                wrote = e;
            }
            if !each(&buf[..written], &mut errno) {
                break;
            }
        }
        (errno, wrote)
    }
}

impl Drop for Cd {
    /// `iconv_close`.
    fn drop(&mut self) {
        // SAFETY: the converter came from `iconv_open` and is closed once,
        // here.
        unsafe {
            libc::iconv_close(self.0);
        }
    }
}

/// `zend_reset_lc_ctype_locale`, which `php_module_startup` runs before
/// any extension starts: `LC_CTYPE` as `C.UTF-8`, or `C`. The host's
/// libiconv reads the locale when a `//TRANSLIT` converter first
/// converts, and answers `?` for all of it under a process that never set
/// one, so this runs as the engine starts, as php's does.
pub(super) fn reset_lc_ctype() {
    for name in [c"C.UTF-8", c"C"] {
        // SAFETY: `name` is a static NUL-terminated string; `setlocale`
        // reads it and keeps nothing of it.
        if !unsafe { libc::setlocale(libc::LC_CTYPE, name.as_ptr()) }.is_null() {
            return;
        }
    }
}

/// The stack below the caller filled with a nonzero byte. The host's
/// libiconv reads a word of its own frame it never wrote when it
/// transliterates: nonzero, as php's stack there holds, it answers
/// `a'eb"uEUR` for `aébü€` under `ASCII//TRANSLIT`; zero, as a fresh
/// stack holds, `?`. The next frame `iconv` pushes lands where this one
/// was.
#[inline(never)]
fn prime() {
    let mut room = [0u8; 2048];
    std::hint::black_box(&mut room).fill(0xAA);
    std::hint::black_box(&room);
}

/// The calling thread's `errno`, read right after the call that set it.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
