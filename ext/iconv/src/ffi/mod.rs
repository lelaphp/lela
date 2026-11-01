//! The host's `iconv(3)` (design §7.2, "posix, pcntl, sockets, iconv:
//! `libc`, thin"; `unsafe.homes`' `ext/*/src/ffi/`): `iconv_open`, one
//! `iconv` call at a time and `iconv_close`, which `Drop` makes. A
//! converter is opened for one call of a native and closed before it
//! returns, or held by one stream filter of one request and closed when
//! the filter leaves its chain; never shared, so no request sees
//! another's shift state. Which `iconv(3)` that is, and its version,
//! [`linked`] says, as php's `configure` and `iconv.c` name it.
#![allow(unsafe_code)]
// `libc` marks its iconv bindings deprecated, meaning to drop them at 1.0.
#![allow(deprecated)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::CString;

pub(crate) mod heap;

/// The `iconv(3)` linked on macOS, the system's libiconv, which
/// `config.m4`'s `for iconv implementation` finds as `"GNU
/// libiconv"` by its `_libiconv_version`: `ICONV_IMPL` and `--ri`'s
/// implementation row are `PHP_ICONV_IMPL`, `"libiconv"`, and
/// `get_iconv_version()` prints the library's own `_libiconv_version`
/// as `"%d.%d"`, the high byte and the low, read when asked: `"1.11"`,
/// Apple's.
#[cfg(target_vendor = "apple")]
pub(super) mod linked {
    /// `PHP_ICONV_IMPL` for `"GNU libiconv"`.
    pub(crate) const IMPL: &str = "libiconv";

    // SAFETY: libiconv exports `_libiconv_version` as an `int` it sets
    // once and never writes, which `libc` links as `-liconv` here too.
    #[link(name = "iconv")]
    unsafe extern "C" {
        #[link_name = "_libiconv_version"]
        static LIBICONV_VERSION: libc::c_int;
    }

    /// `get_iconv_version()` under `HAVE_LIBICONV`.
    pub(crate) fn version() -> String {
        // SAFETY: the library's `int`, initialised before any code of
        // ours runs and never written after.
        let version = unsafe { LIBICONV_VERSION };
        format!("{}.{}", version >> 8, version & 0xff)
    }
}

/// The `iconv(3)` linked under glibc, glibc's own, which `config.m4`
/// finds first, by linking `gnu_get_libc_version()`: `PHP_ICONV_IMPL` is
/// `"glibc"`, and `get_iconv_version()` under `HAVE_GLIBC_ICONV` answers
/// `gnu_get_libc_version()`, the C library's version read when asked.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(super) mod linked {
    use std::ffi::CStr;

    /// `PHP_ICONV_IMPL` for `glibc`.
    pub(crate) const IMPL: &str = "glibc";

    /// `get_iconv_version()` under `HAVE_GLIBC_ICONV`.
    pub(crate) fn version() -> String {
        // SAFETY: takes nothing and answers a static NUL-terminated string.
        let version = unsafe { libc::gnu_get_libc_version() };
        // SAFETY: that string lives as long as the process and is never
        // written.
        let version = unsafe { CStr::from_ptr(version) };
        version.to_string_lossy().into_owned()
    }
}

/// Any other `iconv(3)`, musl's or a BSD's own, which `config.m4` finds
/// as none of glibc's, libiconv, the BSD iconv of `iconv_ccs_init` or
/// IBM's: `iconv.c`'s `PHP_ICONV_IMPL_VALUE` and `get_iconv_version()`
/// both fall back on `"unknown"`.
#[cfg(not(any(target_vendor = "apple", all(target_os = "linux", target_env = "gnu"))))]
pub(super) mod linked {
    /// `PHP_ICONV_IMPL_VALUE` with neither `PHP_ICONV_IMPL` nor
    /// `HAVE_LIBICONV`.
    pub(crate) const IMPL: &str = "unknown";

    /// `get_iconv_version()` with neither library.
    pub(crate) fn version() -> String {
        "unknown".to_owned()
    }
}

/// `errno` as `iconv_open` and `iconv` leave it, decoded once, here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Errno {
    /// `EINVAL`: `iconv`'s incomplete sequence at the input's end, and
    /// `iconv_open`'s conversion it does not know.
    Inval,
    /// `EILSEQ`: an illegal sequence in the input.
    Ilseq,
    /// `E2BIG`: no room left for what comes next.
    TooBig,
    /// Any other, which php prints by number.
    Other(i32),
}

impl Errno {
    /// The number, as php's `Unknown error (%d)` prints `errno`.
    pub(super) fn code(self) -> i32 {
        match self {
            Errno::Inval => libc::EINVAL,
            Errno::Ilseq => libc::EILSEQ,
            Errno::TooBig => libc::E2BIG,
            Errno::Other(n) => n,
        }
    }
}

/// One `iconv_t` converter, closed when dropped.
pub(super) struct Cd(libc::iconv_t);

// SAFETY: a converter is its state alone, which libiconv keeps behind the
// handle with no tie to the thread that opened it; `Cd` owns the handle
// and every call takes `&mut self`, so it is moved between threads and
// never used from two at once.
unsafe impl Send for Cd {}

impl Cd {
    /// `iconv_open(to, from)`: the converter, or `errno`.
    pub(super) fn open(to: &[u8], from: &[u8]) -> Result<Cd, Errno> {
        let (Ok(to), Ok(from)) = (CString::new(to), CString::new(from)) else {
            return Err(Errno::Inval);
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
    ) -> (usize, Result<usize, Errno>) {
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
    pub(super) fn convert(&mut self, input: &[u8]) -> (Vec<u8>, Result<(), Errno>) {
        let mut out = vec![0u8; input.len() + 32];
        let (mut used, mut rest, mut result) = (0, input, Ok(0));
        while !rest.is_empty() {
            let (written, step) = self.step(Some(&mut rest), &mut out[used..]);
            used += written;
            result = step;
            if result == Err(Errno::TooBig) && !rest.is_empty() {
                out.resize(out.len() + input.len(), 0);
                continue;
            }
            break;
        }
        while result.is_ok() {
            let (written, step) = self.step(None, &mut out[used..]);
            used += written;
            result = step;
            if result != Err(Errno::TooBig) {
                break;
            }
            out.resize(out.len() + 16, 0);
        }
        out.truncate(used);
        (out, result.map(|_| ()))
    }

    /// `_php_iconv_appendl`: `input` onto `out` in room of 128 bytes and
    /// doubling, `None` flushing the shift state; the `errno` of a call
    /// that failed other than for room, whose bytes php never counts
    /// into the string and which are dropped here.
    pub(super) fn append(&mut self, input: Option<&[u8]>, out: &mut Vec<u8>) -> Result<(), Errno> {
        let mut room = 128;
        let mut rest = input.unwrap_or_default();
        loop {
            let mut chunk = vec![0u8; room];
            let (written, result) = self.step(input.map(|_| &mut rest), &mut chunk);
            match result {
                Err(Errno::TooBig) | Ok(_) => out.extend_from_slice(&chunk[..written]),
                Err(errno) => return Err(errno),
            }
            if result.is_ok() && input.is_none() {
                return Ok(());
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
    /// of the last failing call that wrote, which is all strpos checks;
    /// `None` where no call failed.
    pub(super) fn walk(
        &mut self,
        input: &[u8],
        width: usize,
        mut each: impl FnMut(&[u8], &mut Option<Errno>) -> bool,
    ) -> (Option<Errno>, Option<Errno>) {
        let (mut rest, mut more, mut errno, mut wrote) = (input, !input.is_empty(), None, None);
        while more {
            let mut buf = [0u8; 8];
            more = !rest.is_empty();
            let (written, result) = self.step(more.then_some(&mut rest), &mut buf[..width]);
            if let Err(e) = result {
                errno = Some(e);
            }
            if written == 0 {
                break;
            }
            if let Err(e) = result {
                wrote = Some(e);
            }
            if !each(&buf[..written], &mut errno) {
                break;
            }
        }
        (errno, wrote)
    }
}

impl Cd {
    /// `iconv(cd, NULL, NULL, NULL, NULL)`: the shift state set back to
    /// its initial one, nothing written.
    pub(super) fn reset(&mut self) -> Result<(), Errno> {
        // SAFETY: with every pointer null `iconv` resets the converter's
        // state and reads or writes no buffer.
        let result = unsafe {
            libc::iconv(
                self.0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if result == usize::MAX {
            Err(errno())
        } else {
            Ok(())
        }
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

/// The calling thread's `errno`: read right after the call that set it,
/// or, where php prints `errno` without having just called anything, as
/// it stands.
pub(super) fn errno() -> Errno {
    match std::io::Error::last_os_error().raw_os_error().unwrap_or(0) {
        libc::EINVAL => Errno::Inval,
        libc::EILSEQ => Errno::Ilseq,
        libc::E2BIG => Errno::TooBig,
        other => Errno::Other(other),
    }
}
