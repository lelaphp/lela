//! The C library calls phar makes that Rust's `std` has no answer for
//! (design §4.1, `unsafe.homes`' `ext/*/src/ffi/`): `localtime_r(3)`
//! and `mktime(3)`, which `zip.c`'s `phar_zip_u2d_time` and
//! `phar_zip_d2u_time` read and write an entry's DOS time and date
//! through, in the host's local time as `TZ` names it; and `getuid(2)`,
//! `getgid(2)` and `getgroups(2)`, which `phar_fancy_stat` chooses the
//! permission bits an access check reads by. php's `phar` calls these
//! from `<time.h>` and `<unistd.h>`, not through another extension's
//! `PHPAPI`, so they are bound here and not taken from ext/standard's
//! or ext/posix's own (design §4.10). Each is one call.
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

/// `getuid()`.
pub(crate) fn uid() -> u32 {
    // SAFETY: `getuid` takes nothing and cannot fail.
    unsafe { libc::getuid() }
}

/// `getgid()`.
pub(crate) fn gid() -> u32 {
    // SAFETY: `getgid` takes nothing and cannot fail.
    unsafe { libc::getgid() }
}

/// `getgroups()`: the supplementary groups, asked for their count first
/// and then into room for that many, as `phar_fancy_stat` does; none
/// where either call fails.
pub(crate) fn groups() -> Vec<u32> {
    // SAFETY: a count of zero asks only for the number of groups; the
    // null pointer is not written through.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let Ok(len) = usize::try_from(count) else {
        return Vec::new();
    };
    if len == 0 {
        return Vec::new();
    }
    let mut gids = vec![0; len];
    // SAFETY: `gids` has room for `count` entries, which is the most the
    // call writes.
    let written = unsafe { libc::getgroups(count, gids.as_mut_ptr()) };
    gids.truncate(usize::try_from(written).unwrap_or(0));
    gids
}
