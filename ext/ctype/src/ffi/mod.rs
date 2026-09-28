//! `<ctype.h>`'s classes as the host's C library answers them
//! (`unsafe.homes`' `ext/*/src/ffi/`): `isalnum(3)` and the rest over
//! one byte, under the locale the calling thread runs under, which is
//! the request's own once `setlocale()` has installed one (design §5.14,
//! revision 40) and the process's, `C` with `LC_CTYPE` `C.UTF-8`,
//! before.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::c_int;

use crate::Class;

/// `iswhat(c)`: whether `byte` is in `class` under the thread's
/// `LC_CTYPE`.
pub(crate) fn is(class: Class, byte: u8) -> bool {
    let test: unsafe extern "C" fn(c_int) -> c_int = match class {
        Class::Alnum => libc::isalnum,
        Class::Alpha => libc::isalpha,
        Class::Cntrl => libc::iscntrl,
        Class::Digit => libc::isdigit,
        Class::Lower => libc::islower,
        Class::Graph => libc::isgraph,
        Class::Print => libc::isprint,
        Class::Punct => libc::ispunct,
        Class::Space => libc::isspace,
        Class::Upper => libc::isupper,
        Class::Xdigit => libc::isxdigit,
    };
    // SAFETY: every class function takes any value an `unsigned char`
    // holds and reads the thread's locale, nothing else.
    unsafe { test(c_int::from(byte)) != 0 }
}
