//! The process's `LC_CTYPE`, the one piece of C library state the engine
//! sets for the whole process (design §4.1, `unsafe.homes`): what
//! `zend_reset_lc_ctype_locale` does in `php_module_startup`, once, before
//! any extension starts; and C's `isspace(3)`, which reads it.

use std::ffi::c_int;
use std::sync::Once;

/// `zend_reset_lc_ctype_locale` (`Z/Zend/zend_operators.c:3003`), which
/// `php_module_startup` runs once per process before any `MINIT`:
/// `LC_CTYPE` as `C.UTF-8`, or `C` where the platform has no `C.UTF-8`.
/// The host's libiconv reads the locale when a `//TRANSLIT` converter
/// first converts, and answers `?` for all of it under a process that
/// never set one.
///
/// Once per process however many engines it builds, and never on two
/// threads at once: the locale is the process's and `setlocale(3)` is not
/// safe to call from two threads at a time. Every caller after the first
/// returns once the first has set it.
pub fn reset_lc_ctype_locale() {
    static SET: Once = Once::new();
    SET.call_once(|| {
        for name in [c"C.UTF-8", c"C"] {
            // SAFETY: `name` is a static NUL-terminated string that
            // `setlocale` reads and keeps nothing of, and `SET` runs this
            // on one thread with every other caller waiting on it; no
            // other code of the engine calls `setlocale`.
            if !unsafe { libc::setlocale(libc::LC_CTYPE, name.as_ptr()) }.is_null() {
                return;
            }
        }
    });
}

/// C's `isspace(3)` under the calling thread's `LC_CTYPE`: the request's
/// locale where `setlocale()` installed one on the thread serving it, and
/// the process's that [`reset_lc_ctype_locale`] set otherwise. What php's
/// own `isspace((unsigned char) c)` answers, and what C's `strtol` and
/// `atoi` skip before a number: the six bytes space, `\t`, `\n`, `\v`,
/// `\f` and `\r` in the `C` locale, and under macOS's `C.UTF-8`, php's
/// default, `0xA0` besides, which glibc's `C.UTF-8` does not count.
#[must_use]
pub fn is_space(byte: u8) -> bool {
    // SAFETY: `isspace` takes any value an `unsigned char` holds and reads
    // the thread's locale and nothing else.
    let answer = unsafe { libc::isspace(c_int::from(byte)) };
    answer != 0
}

#[cfg(test)]
mod tests {
    use super::is_space;

    /// The `C` locale a test process runs under counts the six bytes and
    /// nothing else.
    #[test]
    fn is_space_reads_the_c_locale_six() {
        let spaces: Vec<u8> = (0..=u8::MAX).filter(|b| is_space(*b)).collect();
        assert_eq!(spaces, b"\t\n\x0b\x0c\r ");
    }

    /// Under `C.UTF-8`, php's default, the bytes past the six are the C
    /// library's: macOS's counts `0xA0` and glibc's nothing, so this test
    /// and not a witness, whose pin is one platform's oracle, holds that
    /// reading (#2796). The locale is the test thread's own, so the other
    /// tests of the process keep the `C` locale.
    #[test]
    fn is_space_reads_the_c_libraries_c_utf8() {
        // SAFETY: the name is a static NUL-terminated string `newlocale`
        // reads and keeps nothing of, and no base locale is handed over.
        let utf8 = unsafe {
            libc::newlocale(
                libc::LC_CTYPE_MASK,
                c"C.UTF-8".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        assert!(!utf8.is_null(), "the host has no C.UTF-8");
        // SAFETY: `utf8` is the live locale `newlocale` returned, made
        // this thread's until it is put back below.
        let before = unsafe { libc::uselocale(utf8) };
        let spaces: Vec<u8> = (0..=u8::MAX).filter(|b| is_space(*b)).collect();
        // SAFETY: `before` is what `uselocale` answered, the thread's
        // locale until the call above, so it is live.
        unsafe { libc::uselocale(before) };
        // SAFETY: `utf8` is no thread's locale any longer and is freed
        // once.
        unsafe { libc::freelocale(utf8) };
        let past_six: &[u8] = if cfg!(target_os = "macos") {
            b"\xa0"
        } else {
            b""
        };
        assert_eq!(spaces, [b"\t\n\x0b\x0c\r ".as_slice(), past_six].concat());
    }
}
