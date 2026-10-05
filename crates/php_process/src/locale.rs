//! The process's `LC_CTYPE`, the one piece of C library state the engine
//! sets for the whole process (design §4.1, `unsafe.homes`): what
//! `zend_reset_lc_ctype_locale` does in `php_module_startup`, once, before
//! any extension starts.

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
