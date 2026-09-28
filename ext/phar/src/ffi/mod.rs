//! The C library calls phar makes that Rust's `std` has no answer for
//! (design §4.1, `unsafe.homes`' `ext/*/src/ffi/`): `localtime_r(3)`
//! and `mktime(3)`, which `zip.c`'s `phar_zip_u2d_time` and
//! `phar_zip_d2u_time` read and write an entry's DOS time and date
//! through, in the host's local time as `TZ` names it. php's `phar`
//! calls these from `<time.h>`, not through another extension's
//! `PHPAPI`, so they are bound here (design §4.10). Each is one call.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::mem::MaybeUninit;

/// `struct tm`, the broken-down time `localtime_r` fills and `mktime`
/// reads.
pub(crate) type Tm = libc::tm;

/// `php_localtime_r(&time, &tmbuf)`: `time` broken down in the host's
/// local time, or `None` where the C library cannot represent it.
pub(crate) fn local_time(time: i64) -> Option<Tm> {
    let time: libc::time_t = time;
    let mut tm = MaybeUninit::<Tm>::uninit();
    // SAFETY: both pointers are to live storage of their types; the call
    // reads `time` and writes the whole of `tm` when it answers non-null.
    let filled = unsafe { libc::localtime_r(&raw const time, tm.as_mut_ptr()) };
    if filled.is_null() {
        return None;
    }
    // SAFETY: `localtime_r` answered non-null, so it filled `tm`.
    Some(unsafe { tm.assume_init() })
}

/// `mktime(tm)`: the local time `tm` holds as seconds since the epoch,
/// its fields normalised in place as the C library normalises them, `-1`
/// where it cannot be represented.
pub(crate) fn make_time(tm: &mut Tm) -> i64 {
    // SAFETY: `tm` is a live, initialised `struct tm` the call reads and
    // normalises in place, and nothing else.
    unsafe { libc::mktime(tm) }
}
