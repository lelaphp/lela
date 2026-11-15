//! The C library calls `filestat.c` makes that Rust's `std` has no
//! answer for (design §4.1, `unsafe.homes`' `ext/*/src/ffi/`):
//! `access(2)`, which is what `file_exists`, `is_readable`,
//! `is_writable` and `is_executable` ask of a plain file; `getpwnam_r(3)`
//! and `getgrnam_r(3)` for `chown` and `chgrp` by name; and `utime(2)`,
//! which `touch` sets the times with. The process's user and groups,
//! which choose the permission bits an access check reads of any other
//! wrapper's file, are the engine's (`php_ext::process`). Each is one call, and a path reaches it as the bytes
//! the host has. Beside them are the two `streamsfuncs.c` makes of a
//! stream's host fd: `select(2)`, which `php_select` is on every
//! platform php builds for here, over `fd_set`s built with `FD_SET`, and
//! `isatty(3)`. The request's own locale, the `locale_t` `setlocale()`
//! builds and installs on the thread serving it, is in [`locale`].
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

pub(crate) mod locale;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
#[cfg(target_os = "macos")]
use std::ptr::NonNull;

/// `F_OK`, `R_OK`, `W_OK` and `X_OK`: what `access(2)` is asked.
pub(super) const F_OK: i32 = libc::F_OK;
pub(super) const R_OK: i32 = libc::R_OK;
pub(super) const W_OK: i32 = libc::W_OK;
pub(super) const X_OK: i32 = libc::X_OK;

/// The path as a C string; a NUL inside it is a path no call can be
/// handed, which `Z_PARAM_PATH` refused before it got here.
fn c_path(path: &Path) -> Option<CString> {
    CString::new(path.as_os_str().as_bytes()).ok()
}

/// `VCWD_ACCESS(path, mode) == 0`.
pub(super) fn access(path: &Path, mode: i32) -> bool {
    let Some(path) = c_path(path) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call,
    // which reads it and nothing else.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

/// `memchr(3)`: the offset of the first `byte` in `haystack`, the C
/// library's search, which `zend_memnstr` and `php_char_to_str_ex` call
/// as they are.
pub(super) fn memchr(haystack: &[u8], byte: u8) -> Option<usize> {
    // SAFETY: the pointer and the length are `haystack`'s, which the call
    // reads within and nothing else; the answer is null or a pointer into
    // it.
    let found =
        unsafe { libc::memchr(haystack.as_ptr().cast(), c_int::from(byte), haystack.len()) };
    (!found.is_null()).then(|| found.addr() - haystack.as_ptr().addr())
}

/// `php_get_uid_by_name` (`Z/ext/standard/filestat.c:416`) as ZTS builds
/// it, over `getpwnam_r(3)` with its buffer grown while it is too small.
pub(super) fn uid_by_name(name: &[u8]) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to live storage of the size given,
        // `name` is NUL-terminated, and the call writes only `entry`,
        // `buf` up to its length and `found`.
        let err = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut found,
            )
        };
        if err == libc::ERANGE || err == libc::EAGAIN {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if err != 0 || found.is_null() {
            return None;
        }
        // SAFETY: the call returned 0 with `found` set, so it filled
        // `entry`.
        let entry = unsafe { entry.assume_init() };
        return Some(entry.pw_uid);
    }
}

/// `php_get_gid_by_name` (`Z/ext/standard/filestat.c:275`) as ZTS builds
/// it, over `getgrnam_r(3)` with its buffer grown while it is too small.
pub(super) fn gid_by_name(name: &[u8]) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as `uid_by_name`'s call, over a `group`.
        let err = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut found,
            )
        };
        if err == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if err != 0 || found.is_null() {
            return None;
        }
        // SAFETY: the call returned 0 with `found` set, so it filled
        // `entry`.
        let entry = unsafe { entry.assume_init() };
        return Some(entry.gr_gid);
    }
}

/// `VCWD_UTIME(path, newtime)`: the modification and access times, or
/// the current time for `None`, which `utime(2)` checks against write
/// access where a time given is checked against ownership.
pub(super) fn utime(path: &Path, times: Option<(i64, i64)>) -> std::io::Result<()> {
    let Some(path) = c_path(path) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    let buf = times.map(|(modtime, actime)| libc::utimbuf { actime, modtime });
    let at = buf.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
    // SAFETY: `path` is NUL-terminated and `at` is null or points at a
    // `utimbuf` that outlives the call, which only reads them.
    let ret = unsafe { libc::utime(path.as_ptr(), at) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `fnmatch(pattern, string, flags) == 0`, the host's `fnmatch(3)`,
/// which `fnmatch()` calls as it is; a NUL inside either is refused by
/// `Z_PARAM_PATH` before it gets here.
pub(super) fn fnmatch(pattern: &[u8], string: &[u8], flags: i32) -> bool {
    let (Ok(pattern), Ok(string)) = (CString::new(pattern), CString::new(string)) else {
        return false;
    };
    // SAFETY: both are NUL-terminated strings that outlive the call,
    // which reads them and nothing else.
    unsafe { libc::fnmatch(pattern.as_ptr(), string.as_ptr(), flags) == 0 }
}

/// `arc4random_buf(3)`, the host's CSPRNG, which is what
/// `php_random_bytes` reads on this host: `buf` filled with random
/// bytes. It cannot fail, so the answer is always `true`.
#[cfg(not(target_os = "linux"))]
pub(super) fn random_bytes(buf: &mut [u8]) -> bool {
    // SAFETY: `buf` is live, writable storage of exactly the length
    // given, which is all the call writes.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) };
    true
}

/// `getrandom(2)` until `buf` is full, retried on `EINTR`, and
/// `/dev/urandom` for the rest where it fails otherwise: the arm
/// `php_random_bytes` takes on Linux (`Z/ext/random/csprng.c:100`).
/// `false` where both fail, which php answers with its `FAILURE`.
#[cfg(target_os = "linux")]
pub(super) fn random_bytes(buf: &mut [u8]) -> bool {
    use std::io::{ErrorKind, Read};
    let mut filled = 0;
    while let Some(rest) = buf.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        // SAFETY: `rest` is live, writable storage of exactly the length
        // given, which is all the call writes.
        let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        match usize::try_from(n) {
            Ok(n) => filled += n,
            Err(_) if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted => {}
            Err(_) => {
                return std::fs::File::open("/dev/urandom")
                    .and_then(|mut urandom| urandom.read_exact(rest))
                    .is_ok();
            }
        }
    }
    true
}

/// `sysconf(_SC_ARG_MAX)`, what `PHP_MINIT_FUNCTION(exec)` sets
/// `cmd_max_len` to, and `_POSIX_ARG_MAX`'s 4096 where it answers `-1`.
pub(super) fn arg_max() -> usize {
    // SAFETY: `sysconf` reads a name and writes nothing.
    let max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    usize::try_from(max).unwrap_or(4096)
}

/// `errno = 0; nice(increment); errno == 0`: whether `nice(2)` took the
/// increment, which is how `proc_nice` tells, since `-1` is also a
/// niceness.
pub(super) fn nice(increment: i32) -> bool {
    // SAFETY: `__error` answers the calling thread's `errno` location
    // and nothing else.
    #[cfg(not(target_os = "linux"))]
    let errno = unsafe { libc::__error() };
    // SAFETY: `__errno_location` is Linux's name for the same location.
    #[cfg(target_os = "linux")]
    let errno = unsafe { libc::__errno_location() };
    // SAFETY: that location is live and the thread's own for its life;
    // writing zero to it is what C's `errno = 0` does.
    unsafe { *errno = 0 };
    // SAFETY: `nice` takes a number and changes the process's priority,
    // nothing in this program's memory.
    unsafe { libc::nice(increment) };
    std::io::Error::last_os_error().raw_os_error() == Some(0)
}

/// `sleep(3)`, called once, as `php_sleep` is: the seconds left
/// unslept, which a signal with a handler leaves, and zero when nothing
/// woke it.
pub(super) fn sleep(seconds: u32) -> u32 {
    // SAFETY: `sleep` takes a number and suspends the calling thread,
    // touching nothing in this program's memory.
    unsafe { libc::sleep(seconds) }
}

/// `usleep(3)`, called once; php discards its answer, so a signal with
/// a handler ends the wait early and nothing says so.
pub(super) fn usleep(micros: u32) {
    // SAFETY: `usleep` takes a number and suspends the calling thread,
    // touching nothing in this program's memory.
    unsafe { libc::usleep(micros) };
}

/// What one `nanosleep(2)` answered.
pub(super) enum Nap {
    /// The whole time slept.
    Slept,
    /// `EINTR`: a signal with a handler woke it, this much left.
    Woken { seconds: i64, nanoseconds: i64 },
    /// `EINVAL`: nanoseconds past 999 999 999, or seconds below zero.
    Invalid,
    /// Any other failure.
    Failed,
}

/// `nanosleep(2)`, called once, as `PHP_FUNCTION(time_nanosleep)` calls
/// it, with what is left read only where it answers `EINTR`.
pub(super) fn nanosleep(seconds: i64, nanoseconds: i64) -> Nap {
    // `(time_t) tv_sec` and `(long) tv_nsec`, each 64 bits as `zend_long`
    // is on every host php builds for here.
    let wanted = libc::timespec {
        tv_sec: seconds,
        tv_nsec: nanoseconds,
    };
    let mut left = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: both pointers are to live `timespec`s of this frame; the
    // call reads the first and writes only the second.
    if unsafe { libc::nanosleep(&raw const wanted, &raw mut left) } == 0 {
        return Nap::Slept;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EINTR) => Nap::Woken {
            seconds: left.tv_sec,
            nanoseconds: left.tv_nsec,
        },
        Some(libc::EINVAL) => Nap::Invalid,
        _ => Nap::Failed,
    }
}

/// `openlog(3)` with `ident`, which the caller keeps alive until the
/// matching [`closelog`], since the C library keeps the pointer.
pub(super) fn openlog(ident: &CString, option: i32, facility: i32) {
    // SAFETY: `ident` is NUL-terminated and outlives the log it names,
    // which is closed before the caller lets it go.
    unsafe { libc::openlog(ident.as_ptr(), option, facility) };
}

/// `closelog(3)`.
pub(super) fn closelog() {
    // SAFETY: `closelog` takes nothing and closes the log if one is open.
    unsafe { libc::closelog() };
}

/// `syslog(priority, "%s", message)`: one message. It is a C string
/// because the host's `syslog(3)` may read its argument past what a
/// precision allows, even an empty one's first byte.
pub(super) fn syslog(priority: i32, message: &CStr) {
    // SAFETY: the format is NUL-terminated and asks for one pointer to a
    // NUL-terminated string, which `message` is and outlives the call.
    unsafe { libc::syslog(priority, c"%s".as_ptr(), message.as_ptr()) };
}

/// `strsignal(3)`: the host's name for a signal.
pub(super) fn strsignal(signal: i32) -> String {
    // SAFETY: `strsignal` answers a pointer to a NUL-terminated string
    // the C library owns, valid until the next call on this thread.
    let text = unsafe { libc::strsignal(signal) };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: `text` is the non-null, NUL-terminated string just
    // answered, read before any other call could replace it.
    let text = unsafe { std::ffi::CStr::from_ptr(text) };
    text.to_string_lossy().into_owned()
}

/// A C string the C library owns, copied out, or `None` for null.
fn owned(text: *const libc::c_char) -> Option<Vec<u8>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: `text` is a non-null, NUL-terminated string the call just
    // answered, read before any other call could replace it.
    let text = unsafe { std::ffi::CStr::from_ptr(text) };
    Some(text.to_bytes().to_vec())
}

/// `ntohs(getservbyname(name, proto)->s_port)`.
pub(super) fn service_port(name: &[u8], proto: &[u8]) -> Option<u16> {
    let (name, proto) = (CString::new(name).ok()?, CString::new(proto).ok()?);
    // SAFETY: both strings are NUL-terminated and outlive the call, which
    // answers null or a pointer to the C library's own `servent`.
    let entry = unsafe { libc::getservbyname(name.as_ptr(), proto.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `servent` just answered.
    let port = unsafe { (*entry).s_port };
    let port = u16::from_ne_bytes([port.to_ne_bytes()[0], port.to_ne_bytes()[1]]);
    Some(u16::from_be(port))
}

/// `getservbyport(htons(port), proto)->s_name`.
pub(super) fn service_name(port: u16, proto: &[u8]) -> Option<Vec<u8>> {
    let proto = CString::new(proto).ok()?;
    let port = i32::from(port.to_be());
    // SAFETY: `proto` is NUL-terminated and outlives the call, which
    // answers null or a pointer to the C library's own `servent`.
    let entry = unsafe { libc::getservbyport(port, proto.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `servent` just answered.
    owned(unsafe { (*entry).s_name })
}

/// `getprotobyname(name)->p_proto`.
pub(super) fn protocol_number(name: &[u8]) -> Option<i32> {
    let name = CString::new(name).ok()?;
    // SAFETY: `name` is NUL-terminated and outlives the call, which
    // answers null or a pointer to the C library's own `protoent`.
    let entry = unsafe { libc::getprotobyname(name.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `protoent` just answered.
    Some(unsafe { (*entry).p_proto })
}

/// `getprotobynumber(number)->p_name`.
pub(super) fn protocol_name(number: i32) -> Option<Vec<u8>> {
    // SAFETY: the call takes a number and answers null or a pointer to
    // the C library's own `protoent`.
    let entry = unsafe { libc::getprotobynumber(number) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-null `protoent` just answered.
    owned(unsafe { (*entry).p_name })
}

/// C's `time_t`, the type the libc crate binds `localtime_r`, `gmtime_r`
/// and `struct timeval` with. The crate deprecates the alias on musl,
/// whose 1.2.0 made `time_t` 64 bits on every target, a width the crate
/// has yet to follow on 32-bit ones (rust-lang/libc#1848). Every value
/// reaches it through `try_from`, which is right at either width, so the
/// deprecation is expected on musl and nowhere else; once the crate
/// lifts it, the unmet expectation fails the build.
#[cfg_attr(
    target_env = "musl",
    expect(deprecated, reason = "every value reaches it through try_from")
)]
type TimeT = libc::time_t;

/// C's `suseconds_t`, the type of `struct timeval`'s `tv_usec`,
/// deprecated on musl as [`TimeT`] is.
#[cfg_attr(
    target_env = "musl",
    expect(deprecated, reason = "every value reaches it through try_from")
)]
type SusecondsT = libc::suseconds_t;

/// `php_localtime_r(&now)`'s `tm_mon` and `tm_mday`, the month counted
/// from zero, in the process's own zone: what phpinfo's page asks to
/// show the first of April's logo.
pub(super) fn local_month_day() -> Option<(i32, i32)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let now = TimeT::try_from(now).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `now` is a live `time_t` and `tm` live, zeroed storage the
    // call writes; it answers null or a pointer to `tm`.
    let filled = unsafe { libc::localtime_r(&raw const now, tm.as_mut_ptr()) };
    if filled.is_null() {
        return None;
    }
    // SAFETY: a zeroed `tm` is a valid one, and the call wrote only fields.
    let tm = unsafe { tm.assume_init() };
    Some((tm.tm_mon, tm.tm_mday))
}

/// What `strptime(s, format, &tm)` over a zeroed `tm` came to: the eight
/// fields `strptime()` answers, `tm_sec` to `tm_yday`, and what was left
/// of the string.
pub(super) struct Parsed {
    pub(super) fields: [i32; 8],
    pub(super) rest: Vec<u8>,
    /// Where the C library turned the fields into its local time, as it
    /// does after a `%Z` of GMT or UTC and a `%z` (`timegm`, then
    /// `localtime_r`), the moment that was: the fields back through
    /// `timegm`, less the offset `localtime_r` wrote beside them. The
    /// local time is the process's, and php's is the request's.
    pub(super) moment: Option<i64>,
}

/// `strptime(s, format, &tm)` over a zeroed `tm`, `None` where the call
/// answered null.
pub(super) fn strptime(s: &[u8], format: &[u8]) -> Option<Parsed> {
    let (text, format) = (CString::new(s).ok()?, CString::new(format).ok()?);
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both strings are NUL-terminated and outlive the call, and
    // `tm` is live, zeroed storage the call writes; it answers null or a
    // pointer into `text`.
    let rest = unsafe { libc::strptime(text.as_ptr(), format.as_ptr(), tm.as_mut_ptr()) };
    if rest.is_null() {
        return None;
    }
    // SAFETY: a zeroed `tm` is a valid one, and the call wrote only fields.
    let mut tm = unsafe { tm.assume_init() };
    let rest = owned(rest)?;
    let fields = [
        tm.tm_sec, tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon, tm.tm_year, tm.tm_wday, tm.tm_yday,
    ];
    // Only `localtime_r` names a zone in the `tm`, which was zeroed.
    let moment = (!tm.tm_zone.is_null()).then(|| {
        let offset: i64 = tm.tm_gmtoff;
        // SAFETY: `tm` is a live, initialised `tm`; `timegm` reads its
        // fields and normalises them in place.
        let at = unsafe { libc::timegm(&raw mut tm) };
        at - offset
    });
    Some(Parsed {
        fields,
        rest,
        moment,
    })
}

/// `gmtime_r(t)`'s eight fields, `tm_sec` to `tm_yday`: the civil time of
/// `t` read as UTC, which a local time is once its offset is added.
pub(super) fn civil(t: i64) -> Option<[i32; 8]> {
    let t = TimeT::try_from(t).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `t` is a live `time_t` and `tm` live, zeroed storage the
    // call writes; it answers null or a pointer to `tm`.
    let filled = unsafe { libc::gmtime_r(&raw const t, tm.as_mut_ptr()) };
    if filled.is_null() {
        return None;
    }
    // SAFETY: a zeroed `tm` is a valid one, and the call wrote only fields.
    let tm = unsafe { tm.assume_init() };
    Some([
        tm.tm_sec, tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon, tm.tm_year, tm.tm_wday, tm.tm_yday,
    ])
}

/// One `ifaddrs` entry as `net_get_interfaces` reads it: the name, the
/// flags, the address's family, and the four addresses `php_inet_ntop`
/// renders, the last two only where `IFF_BROADCAST` or
/// `IFF_POINTOPOINT` says they are there.
pub(super) struct InterfaceAddress {
    pub(super) name: Vec<u8>,
    pub(super) flags: u32,
    pub(super) family: Option<i64>,
    pub(super) address: Option<Vec<u8>>,
    pub(super) netmask: Option<Vec<u8>>,
    pub(super) broadcast: Option<Vec<u8>>,
    pub(super) ptp: Option<Vec<u8>>,
}

unsafe extern "C" {
    /// `inet_ntop(3)`, which the `libc` crate does not declare.
    fn inet_ntop(
        af: libc::c_int,
        src: *const libc::c_void,
        dst: *mut libc::c_char,
        size: libc::socklen_t,
    ) -> *const libc::c_char;
}

/// `php_inet_ntop` (`Z/ext/standard/net.c:44`) over `inet_ntop(3)`: an
/// IPv4 or IPv6 address as text, `None` for any other family.
fn address_text(addr: *const libc::sockaddr) -> Option<Vec<u8>> {
    if addr.is_null() {
        return None;
    }
    // SAFETY: `addr` is a non-null `sockaddr` from `getifaddrs`, whose
    // family field every family has.
    let family = i32::from(unsafe { (*addr).sa_family });
    let mut out = [0 as libc::c_char; 64];
    let len = libc::socklen_t::try_from(out.len()).unwrap_or(0);
    // The address field's place inside the family's structure, reached
    // by offset so no pointer is cast to a stricter alignment.
    let offset = match family {
        libc::AF_INET => std::mem::offset_of!(libc::sockaddr_in, sin_addr),
        libc::AF_INET6 => std::mem::offset_of!(libc::sockaddr_in6, sin6_addr),
        _ => return None,
    };
    let src = addr
        .cast::<u8>()
        .wrapping_add(offset)
        .cast::<libc::c_void>();
    // SAFETY: `src` points at an address of `family`, and `out` has room
    // for `len` bytes, the most the call writes.
    let text = unsafe { inet_ntop(family, src, out.as_mut_ptr(), len) };
    owned(text)
}

/// `getifaddrs(3)` walked and freed: each entry, in its order, or the
/// error the call left.
pub(super) fn interfaces() -> std::io::Result<Vec<InterfaceAddress>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a live pointer the call writes the list to.
    if unsafe { libc::getifaddrs(&raw mut head) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut found = Vec::new();
    let mut at = head;
    while !at.is_null() {
        // SAFETY: `at` is a non-null entry of the list `getifaddrs` made,
        // which lives until `freeifaddrs` below.
        let entry = unsafe { &*at };
        let flags = entry.ifa_flags;
        let family = if entry.ifa_addr.is_null() {
            None
        } else {
            // SAFETY: `ifa_addr` is non-null and the list's own.
            Some(i64::from(unsafe { (*entry.ifa_addr).sa_family }))
        };
        // `ifa_dstaddr`, which glibc names `ifa_ifu`, the union it shares
        // with `ifa_broadaddr` there as it does here.
        #[cfg(not(target_os = "linux"))]
        let dstaddr = entry.ifa_dstaddr;
        #[cfg(target_os = "linux")]
        let dstaddr = entry.ifa_ifu;
        let broadcast = flags & u32::try_from(libc::IFF_BROADCAST).unwrap_or(0) != 0;
        let ptp = flags & u32::try_from(libc::IFF_POINTOPOINT).unwrap_or(0) != 0;
        found.push(InterfaceAddress {
            name: owned(entry.ifa_name).unwrap_or_default(),
            flags,
            family,
            address: address_text(entry.ifa_addr),
            netmask: address_text(entry.ifa_netmask),
            broadcast: if broadcast {
                address_text(dstaddr)
            } else {
                None
            },
            ptp: if ptp { address_text(dstaddr) } else { None },
        });
        at = entry.ifa_next;
    }
    // SAFETY: `head` is the list `getifaddrs` made, freed once, and
    // nothing read from it is used after.
    unsafe { libc::freeifaddrs(head) };
    Ok(found)
}

// ---- dns.c: the resolver `Z/ext/standard/php_dns.h` picks per host,
// darwin's `<dns.h>` (`HAVE_DNS_SEARCH`) and libresolv's `res_search`
// elsewhere, `dn_expand` and `dn_skipname` from libresolv, `getaddrinfo`,
// `getnameinfo`, `inet_pton` and `gethostname`.

/// The opaque `dns_handle_t` of `<dns.h>`, `Z/ext/standard/dns.c:819`.
#[cfg(target_os = "macos")]
type DnsHandleT = *const c_void;

#[cfg(target_os = "macos")]
#[link(name = "resolv")]
unsafe extern "C" {
    /// `dns_open`, `<dns.h>`, called at `Z/ext/standard/dns.c:416`.
    fn dns_open(name: *const c_char) -> DnsHandleT;
    /// `dns_free`, `<dns.h>`, called by `php_dns_free_handle`
    /// (`Z/ext/standard/php_dns.h:25`).
    fn dns_free(dns: DnsHandleT);
    /// `dns_search`, `<dns.h>`, called by `php_dns_search`
    /// (`Z/ext/standard/php_dns.h:23`).
    fn dns_search(
        dns: DnsHandleT,
        name: *const c_char,
        dnsclass: u32,
        dnstype: u32,
        buf: *mut c_char,
        len: u32,
        from: *mut libc::sockaddr,
        fromlen: *mut u32,
    ) -> i32;
    /// `dn_expand`, which `<resolv.h>` renames `res_9_dn_expand` on darwin;
    /// called at `Z/ext/standard/dns.c:461`.
    #[link_name = "res_9_dn_expand"]
    fn dn_expand_c(
        msg: *const u8,
        eom: *const u8,
        src: *const u8,
        dst: *mut c_char,
        dstsiz: c_int,
    ) -> c_int;
    /// `dn_skipname`, which `<resolv.h>` renames `res_9_dn_skipname` on
    /// darwin; called at `Z/ext/standard/dns.c:996`.
    #[link_name = "res_9_dn_skipname"]
    fn dn_skipname_c(ptr: *const u8, eom: *const u8) -> c_int;
    /// `h_errno`, `<netdb.h>`, read by `php_dns_errno`
    /// (`Z/ext/standard/php_dns.h:27`): a plain global on darwin, set by
    /// `dns_search`.
    static mut h_errno: c_int;
}

#[cfg(target_os = "linux")]
#[link(name = "resolv")]
unsafe extern "C" {
    /// `res_search`, `<resolv.h>`: `php_dns_search` where there is no
    /// `<dns.h>`. glibc exports it from libc under this name since 2.34,
    /// and `__res_search` only as a compatibility symbol a link cannot
    /// bind. php takes `res_nsearch` over a state of its own; glibc keeps
    /// `_res` per thread, so the calling thread's state is as private.
    fn res_search(
        dname: *const c_char,
        class: c_int,
        ty: c_int,
        answer: *mut u8,
        anslen: c_int,
    ) -> c_int;
    /// `dn_expand`, `<resolv.h>`; called at `Z/ext/standard/dns.c:461`.
    #[link_name = "dn_expand"]
    fn dn_expand_c(
        msg: *const u8,
        eom: *const u8,
        src: *const u8,
        dst: *mut c_char,
        dstsiz: c_int,
    ) -> c_int;
    /// `dn_skipname`, `<resolv.h>`; called at `Z/ext/standard/dns.c:996`.
    #[link_name = "dn_skipname"]
    fn dn_skipname_c(ptr: *const u8, eom: *const u8) -> c_int;
    /// The calling thread's `h_errno`, which `<netdb.h>` defines as
    /// `(*__h_errno_location ())` on glibc.
    fn __h_errno_location() -> *mut c_int;
}

unsafe extern "C" {
    /// `inet_pton`, `<arpa/inet.h>`, called at `Z/ext/standard/dns.c:181`.
    fn inet_pton(af: c_int, src: *const c_char, dst: *mut c_void) -> c_int;
}

/// The `from`/`fromsize` pair php keeps for `dns_search` across one call of
/// a native (`Z/ext/standard/dns.c:817`): `fromsize` is set once and
/// `dns_search` may shrink it. `res_search` takes neither.
pub(super) struct DnsFrom {
    /// `struct sockaddr_storage from`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    storage: libc::sockaddr_storage,
    /// `uint32_t fromsize = sizeof(from)`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    len: u32,
}

impl DnsFrom {
    /// A zeroed `sockaddr_storage` and its size, as `Z/ext/standard/dns.c:817`.
    pub(super) fn new() -> DnsFrom {
        // SAFETY: `sockaddr_storage` is a C struct of integers and padding
        // bytes, for which all-zero is a valid value.
        let storage = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
        let len = u32::try_from(size_of::<libc::sockaddr_storage>()).unwrap_or(0);
        DnsFrom { storage, len }
    }
}

/// An open `dns_handle_t`, freed with `dns_free` when dropped
/// (`php_dns_free_handle`, `Z/ext/standard/php_dns.h:25`).
#[cfg(target_os = "macos")]
pub(super) struct DnsHandle(NonNull<c_void>);

/// The resolver `res_search` reads, the calling thread's own `_res`; there
/// is nothing to open or to free.
#[cfg(target_os = "linux")]
pub(super) struct DnsHandle(());

#[cfg(target_os = "macos")]
impl DnsHandle {
    /// `dns_open(NULL)`, `Z/ext/standard/dns.c:416`; `None` where it gives
    /// `NULL`.
    pub(super) fn open() -> Option<DnsHandle> {
        // SAFETY: `dns_open` takes `NULL` for the default resolver setup and
        // returns an owned pointer or `NULL`.
        let h = unsafe { dns_open(ptr::null()) };
        NonNull::new(h.cast_mut()).map(DnsHandle)
    }

    /// `php_dns_search`, `Z/ext/standard/php_dns.h:23`: `dns_search` into
    /// `buf`. `Ok` is the length of the answer; `Err` is `h_errno`, read
    /// right after the call as `php_dns_errno` does.
    pub(super) fn search(
        &self,
        name: &CStr,
        class: u32,
        ty: u32,
        buf: &mut [u8],
        from: &mut DnsFrom,
    ) -> Result<usize, c_int> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let from_ptr = (&raw mut from.storage).cast::<libc::sockaddr>();
        // SAFETY: the pointer came from `dns_open` and is not yet freed;
        // `name` is NUL-terminated; `buf` is writable for `len` bytes, which
        // is at most its length; `from`/`fromlen` point at a live
        // `sockaddr_storage` and its size.
        let n = unsafe {
            dns_search(
                self.0.as_ptr(),
                name.as_ptr(),
                class,
                ty,
                buf.as_mut_ptr().cast::<c_char>(),
                len,
                from_ptr,
                &raw mut from.len,
            )
        };
        if n < 0 {
            return Err(last_h_errno());
        }
        Ok(usize::try_from(n).unwrap_or(0).min(buf.len()))
    }
}

#[cfg(target_os = "linux")]
impl DnsHandle {
    /// The thread's resolver, which `res_search` initialises on first use.
    #[allow(clippy::unnecessary_wraps)] // darwin's `dns_open` can give `NULL`
    pub(super) fn open() -> Option<DnsHandle> {
        Some(DnsHandle(()))
    }

    /// `php_dns_search` without `<dns.h>`: `res_search` into `buf`. `Ok` is
    /// the length of the answer; `Err` is the thread's `h_errno`, read right
    /// after the call as `php_dns_errno` does.
    #[allow(clippy::unused_self)] // the handle darwin's `dns_search` reads
    pub(super) fn search(
        &self,
        name: &CStr,
        class: u32,
        ty: u32,
        buf: &mut [u8],
        _from: &mut DnsFrom,
    ) -> Result<usize, c_int> {
        let len = c_int::try_from(buf.len()).unwrap_or(c_int::MAX);
        let class = c_int::try_from(class).unwrap_or(c_int::MAX);
        let ty = c_int::try_from(ty).unwrap_or(c_int::MAX);
        // SAFETY: `name` is NUL-terminated and `buf` is writable for `len`
        // bytes, which is at most its length.
        let n = unsafe { res_search(name.as_ptr(), class, ty, buf.as_mut_ptr(), len) };
        if n < 0 {
            return Err(last_h_errno());
        }
        Ok(usize::try_from(n).unwrap_or(0).min(buf.len()))
    }
}

#[cfg(target_os = "macos")]
impl Drop for DnsHandle {
    /// `dns_free`, `Z/ext/standard/php_dns.h:25`.
    fn drop(&mut self) {
        // SAFETY: the pointer came from `dns_open` and is freed once, here.
        unsafe { dns_free(self.0.as_ptr()) }
    }
}

#[cfg(target_os = "linux")]
impl Drop for DnsHandle {
    /// `php_dns_free_handle`'s place: nothing to free, since `res_search`
    /// reads the thread's own state.
    fn drop(&mut self) {}
}

/// The value of `h_errno`, `Z/ext/standard/php_dns.h:27`.
#[cfg(target_os = "macos")]
fn last_h_errno() -> c_int {
    let p = &raw const h_errno;
    // SAFETY: `h_errno` is a C `int` that lives for the whole process; it
    // is read by value, with no reference kept.
    unsafe { p.read() }
}

/// The calling thread's `h_errno`, `Z/ext/standard/php_dns.h:27`.
#[cfg(target_os = "linux")]
fn last_h_errno() -> c_int {
    // SAFETY: `__h_errno_location` returns the calling thread's `h_errno`,
    // valid for the thread's life.
    let p = unsafe { __h_errno_location() };
    // SAFETY: `p` points at a live, aligned `int` of this thread.
    unsafe { p.read() }
}

/// `dn_expand(msg, msg + msg.len(), msg + pos, name, cap)`, as
/// `Z/ext/standard/dns.c:461` calls it: the length of
/// the compressed name at `pos` and the name, or `None` where libresolv
/// gives -1. A `pos` at or past the end is -1, as `ns_name_unpack` checks.
pub(super) fn dn_expand(msg: &[u8], pos: usize, cap: usize) -> Option<(usize, Vec<u8>)> {
    let tail = msg.get(pos..)?;
    if tail.is_empty() {
        return None;
    }
    let mut dst = vec![0u8; cap.max(1)];
    let size = c_int::try_from(cap).unwrap_or(c_int::MAX);
    let range = msg.as_ptr_range();
    // SAFETY: `msg..eom` is one live slice and `src` lies inside it;
    // libresolv reads only inside it and writes at most `size` bytes, no
    // more than `dst` holds, NUL included.
    let n = unsafe {
        dn_expand_c(
            range.start,
            range.end,
            tail.as_ptr(),
            dst.as_mut_ptr().cast::<c_char>(),
            size,
        )
    };
    let n = usize::try_from(n).ok()?;
    let len = dst.iter().position(|&b| b == 0).unwrap_or(dst.len());
    dst.truncate(len);
    Some((n, dst))
}

/// `dn_skipname(msg + pos, msg + msg.len())`, as
/// `Z/ext/standard/dns.c:996` calls it: the length of the compressed
/// name at `pos`, or -1. A `pos` past the end is -1 and a `pos` at the end
/// is 0, as `ns_name_skip` gives.
pub(super) fn dn_skipname(msg: &[u8], pos: usize) -> c_int {
    let Some(tail) = msg.get(pos..) else {
        return -1;
    };
    let range = tail.as_ptr_range();
    // SAFETY: `ptr..eom` is one live slice; libresolv reads only below
    // `eom`.
    unsafe { dn_skipname_c(range.start, range.end) }
}

/// `gethostname(buf, sizeof(buf))` into a buffer of `size` bytes: the name
/// up to its NUL, or the `errno` and `strerror(errno)` of a failure
/// (`Z/ext/standard/dns.c:131`).
pub(super) fn gethostname(size: usize) -> Result<Vec<u8>, (c_int, Vec<u8>)> {
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is writable for `buf.len()` bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err((errno, strerror(errno)));
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    Ok(buf)
}

/// `strerror(errnum)` of `Z/ext/standard/dns.c:132`, through `strerror_r`.
fn strerror(errnum: c_int) -> Vec<u8> {
    let mut buf = vec![0u8; 256];
    // SAFETY: `buf` is writable for `buf.len()` bytes; `strerror_r` writes a
    // NUL-terminated text no longer than that.
    let rc = unsafe { libc::strerror_r(errnum, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    if rc != 0 && buf.first() == Some(&0) {
        return format!("Unknown error: {errnum}").into_bytes();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    buf
}

/// `inet_pton(AF_INET6, ip, &sa6.sin6_addr)` of `Z/ext/standard/dns.c:181`;
/// `None` where it gives 0.
pub(super) fn inet_pton6(ip: &CStr) -> Option<[u8; 16]> {
    let mut addr = libc::in6_addr { s6_addr: [0; 16] };
    // SAFETY: `ip` is NUL-terminated and `addr` is a live `in6_addr`, the
    // size `AF_INET6` writes.
    let rc = unsafe {
        inet_pton(
            libc::AF_INET6,
            ip.as_ptr(),
            (&raw mut addr).cast::<c_void>(),
        )
    };
    (rc != 0).then_some(addr.s6_addr)
}

/// `inet_pton(AF_INET, ip, &sa4.sin_addr)` of `Z/ext/standard/dns.c:188`;
/// `None` where it gives 0. The address is in network order.
pub(super) fn inet_pton4(ip: &CStr) -> Option<[u8; 4]> {
    let mut addr = libc::in_addr { s_addr: 0 };
    // SAFETY: `ip` is NUL-terminated and `addr` is a live `in_addr`, the
    // size `AF_INET` writes.
    let rc = unsafe { inet_pton(libc::AF_INET, ip.as_ptr(), (&raw mut addr).cast::<c_void>()) };
    (rc != 0).then_some(addr.s_addr.to_ne_bytes())
}

/// `NI_MAXHOST`, `<netdb.h>`: the size of `out`, `Z/ext/standard/dns.c:177`.
const NI_MAXHOST: usize = 1025;

/// `getnameinfo(sa, len, out, NI_MAXHOST, NULL, 0, NI_NAMEREQD)` of
/// `Z/ext/standard/dns.c:184`: the name,
/// or `None` where it does not give 0.
fn getnameinfo_namereqd(sa: *const libc::sockaddr, len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; NI_MAXHOST];
    let salen = libc::socklen_t::try_from(len).ok()?;
    let hostlen = libc::socklen_t::try_from(out.len()).ok()?;
    // SAFETY: the callers pass a pointer to a live socket address of `len`
    // bytes; `out` is writable for `hostlen` bytes; no service is asked for.
    let rc = unsafe {
        libc::getnameinfo(
            sa,
            salen,
            out.as_mut_ptr().cast::<c_char>(),
            hostlen,
            ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        return None;
    }
    let n = out.iter().position(|&b| b == 0).unwrap_or(out.len());
    out.truncate(n);
    Some(out)
}

/// The `getnameinfo` of `Z/ext/standard/dns.c:184`: a zeroed
/// `sockaddr_in6` with only its family and address set, as php leaves it.
pub(super) fn name_of_v6(addr: [u8; 16]) -> Option<Vec<u8>> {
    let sa6 = libc::sockaddr_in6 {
        #[cfg(not(target_os = "linux"))]
        sin6_len: 0,
        sin6_family: libc::sa_family_t::try_from(libc::AF_INET6).unwrap_or(0),
        sin6_port: 0,
        sin6_flowinfo: 0,
        sin6_addr: libc::in6_addr { s6_addr: addr },
        sin6_scope_id: 0,
    };
    getnameinfo_namereqd(
        (&raw const sa6).cast::<libc::sockaddr>(),
        size_of::<libc::sockaddr_in6>(),
    )
}

/// The `getnameinfo` of `Z/ext/standard/dns.c:191`: a zeroed `sockaddr_in`
/// with only its family and address set, as php leaves it.
pub(super) fn name_of_v4(addr: [u8; 4]) -> Option<Vec<u8>> {
    let sa4 = libc::sockaddr_in {
        #[cfg(not(target_os = "linux"))]
        sin_len: 0,
        sin_family: libc::sa_family_t::try_from(libc::AF_INET).unwrap_or(0),
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(addr),
        },
        sin_zero: [0; 8],
    };
    getnameinfo_namereqd(
        (&raw const sa4).cast::<libc::sockaddr>(),
        size_of::<libc::sockaddr_in>(),
    )
}

/// The list `getaddrinfo` gives, freed with `freeaddrinfo` when dropped; it
/// stands in for the `hostent` of `Z/main/network.c:1410`.
struct AddrList(*mut libc::addrinfo);

impl Drop for AddrList {
    /// `freeaddrinfo`, for the list of `Z/main/network.c:1410`.
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the list came from a `getaddrinfo` that gave 0 and is
            // freed once, here.
            unsafe { libc::freeaddrinfo(self.0) }
        }
    }
}

/// The IPv4 addresses of `name`, in the order `getaddrinfo` lists them, for
/// `php_network_gethostbyname` (`Z/main/network.c:1410`), which is
/// `gethostbyname(3)` on darwin. `None` where the lookup fails. One entry
/// per address: the hints ask for `SOCK_STREAM` only.
pub(super) fn ipv4_addresses(name: &CStr) -> Option<Vec<[u8; 4]>> {
    let hints = libc::addrinfo {
        ai_flags: 0,
        ai_family: libc::AF_INET,
        ai_socktype: libc::SOCK_STREAM,
        ai_protocol: 0,
        ai_addrlen: 0,
        ai_canonname: ptr::null_mut(),
        ai_addr: ptr::null_mut(),
        ai_next: ptr::null_mut(),
    };
    let mut res: *mut libc::addrinfo = ptr::null_mut();
    // SAFETY: `name` is NUL-terminated, no service is asked for, `hints` is
    // a live `addrinfo` with null pointers and `res` is a live out pointer.
    let rc =
        unsafe { libc::getaddrinfo(name.as_ptr(), ptr::null(), &raw const hints, &raw mut res) };
    if rc != 0 {
        return None;
    }
    let list = AddrList(res);
    let mut out = Vec::new();
    let mut cur = list.0.cast_const();
    while !cur.is_null() {
        // SAFETY: `cur` is a non-null node of the list `getaddrinfo` gave,
        // alive until `list` drops.
        let ai = unsafe { &*cur };
        let wide = usize::try_from(ai.ai_addrlen).unwrap_or(0) >= size_of::<libc::sockaddr_in>();
        if ai.ai_family == libc::AF_INET && !ai.ai_addr.is_null() && wide {
            // SAFETY: `ai_addr` is non-null and points at `ai_addrlen`
            // bytes, at least a `sockaddr_in`, of the same list.
            let sa = unsafe { ptr::read_unaligned(ai.ai_addr.cast::<libc::sockaddr_in>()) };
            out.push(sa.sin_addr.s_addr.to_ne_bytes());
        }
        cur = ai.ai_next.cast_const();
    }
    Some(out)
}

/// `uname(2)`, what `php_get_uname` reads (`Z/ext/standard/info.c:655`):
/// the system's name, node name, release, version and machine, in that
/// order; `None` where the call fails.
pub(super) fn uname() -> Option<[Vec<u8>; 5]> {
    // SAFETY: `utsname` is plain C data, for which all zeroes is a value.
    let mut buf: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `buf` is a live `utsname` the call writes into.
    if unsafe { libc::uname(&raw mut buf) } != 0 {
        return None;
    }
    let field = |chars: &[c_char]| -> Vec<u8> {
        chars
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| c.to_ne_bytes()[0])
            .collect()
    };
    Some([
        field(&buf.sysname),
        field(&buf.nodename),
        field(&buf.release),
        field(&buf.version),
        field(&buf.machine),
    ])
}

// ----------------------------------------------------- streamsfuncs.c

/// `FD_SETSIZE`: the first fd an `fd_set` has no bit for, which
/// `PHP_SAFE_MAX_FD` refuses before `select(2)` is asked.
pub(super) const FD_SETSIZE: RawFd = 1024;

/// An empty `fd_set` with the bit of each fd of `fds` set; each
/// is below [`FD_SETSIZE`], which the caller has checked.
fn fd_set(fds: &[RawFd]) -> libc::fd_set {
    let mut set = std::mem::MaybeUninit::<libc::fd_set>::uninit();
    // SAFETY: `FD_ZERO` writes every byte of the set it is handed, which
    // is live storage of an `fd_set`'s size.
    unsafe { libc::FD_ZERO(set.as_mut_ptr()) };
    // SAFETY: `FD_ZERO` initialised every byte of the set.
    let mut set = unsafe { set.assume_init() };
    for &fd in fds {
        if (0..FD_SETSIZE).contains(&fd) {
            // SAFETY: `fd` is in `0..FD_SETSIZE`, a bit the set has.
            unsafe { libc::FD_SET(fd, &raw mut set) };
        }
    }
    set
}

/// The fds of `fds` whose bit `set` holds.
fn set_in(set: &libc::fd_set, fds: &[RawFd]) -> Vec<RawFd> {
    fds.iter()
        .copied()
        .filter(|&fd| {
            // SAFETY: `fd` is in `0..FD_SETSIZE`, a bit the set has, and
            // `FD_ISSET` only reads it.
            (0..FD_SETSIZE).contains(&fd) && unsafe { libc::FD_ISSET(fd, set) }
        })
        .collect()
}

/// `php_select(max_fd + 1, &rfds, &wfds, &efds, tv_p)`: the three sets
/// of fds, `timeout` as seconds and microseconds or `None` to
/// wait for ever; the count `select(2)` answered, one per bit, and the
/// fds of each set it left set, or the `errno` of its `-1`.
pub(super) fn select(
    sets: [&[RawFd]; 3],
    timeout: Option<(i64, i64)>,
) -> std::io::Result<(i32, [Vec<RawFd>; 3])> {
    let [read, write, except] = sets;
    let max_fd = read
        .iter()
        .chain(write)
        .chain(except)
        .copied()
        .max()
        .unwrap_or(0);
    let (mut rfds, mut wfds, mut efds) = (fd_set(read), fd_set(write), fd_set(except));
    let mut tv = timeout.map(|(sec, usec)| libc::timeval {
        tv_sec: TimeT::try_from(sec).unwrap_or(TimeT::MAX),
        tv_usec: SusecondsT::try_from(usec).unwrap_or(0),
    });
    let tv_p = tv.as_mut().map_or(std::ptr::null_mut(), std::ptr::from_mut);
    // SAFETY: the three sets and the `timeval`, where there is one, are
    // live storage the call reads and writes and nothing else holds.
    let ret = unsafe {
        libc::select(
            max_fd + 1,
            &raw mut rfds,
            &raw mut wfds,
            &raw mut efds,
            tv_p,
        )
    };
    if ret == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        ret,
        [
            set_in(&rfds, read),
            set_in(&wfds, write),
            set_in(&efds, except),
        ],
    ))
}

/// `isatty(fileno)`: whether the fd is a terminal; any number is
/// a fd the call may be asked about.
pub(super) fn isatty(fd: RawFd) -> bool {
    // SAFETY: `isatty` reads nothing through its argument, a number.
    unsafe { libc::isatty(fd) == 1 }
}

// ------------------------------------------------------------ proc_open.c

/// `WNOHANG` and `WUNTRACED`, what `waitpid(2)` is given.
pub(super) const WNOHANG: i32 = libc::WNOHANG;
pub(super) const WUNTRACED: i32 = libc::WUNTRACED;

/// `waitpid(pid, &status, options)`: the pid it answered, `0` for a
/// child still running under `WNOHANG`, and the wait status, or the
/// `errno` of its `-1`.
pub(super) fn waitpid(pid: i32, options: i32) -> std::io::Result<(i32, i32)> {
    let mut status = 0;
    // SAFETY: `status` is live storage of an `int`, the one thing the
    // call writes.
    let ret = unsafe { libc::waitpid(pid, &raw mut status, options) };
    if ret == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok((ret, status))
    }
}

/// `kill(pid, sig) == 0`.
pub(super) fn kill(pid: i32, sig: i32) -> bool {
    // SAFETY: `kill` reads nothing through its arguments, two numbers.
    unsafe { libc::kill(pid, sig) == 0 }
}

/// `dup(fd)`: a new fd of the same open file, close-on-exec as every fd
/// this process makes, or the `errno` of its `-1`.
pub(super) fn dup(fd: RawFd) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: `fcntl` with `F_DUPFD_CLOEXEC` reads nothing through its
    // arguments and makes a new fd or fails.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if new == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `new` is the fd `fcntl` just made, open and owned by nothing
    // else.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(new) })
}

/// `openpty(&master, &slave, NULL, NULL, NULL)`: the two ends of a new
/// pseudoterminal, or the `errno` of its `-1`.
pub(super) fn openpty() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: `master` and `slave` are live `int`s the call writes; the
    // three null pointers are the name, the termios and the window size
    // the call is allowed not to be given.
    let ret = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openpty` answered 0, so `master` is an open fd it made and
    // nothing else owns.
    let master = unsafe { std::os::fd::OwnedFd::from_raw_fd(master) };
    // SAFETY: as `master`.
    let slave = unsafe { std::os::fd::OwnedFd::from_raw_fd(slave) };
    Ok((master, slave))
}

/// `close_parentends_of_pipes` (`Z/ext/standard/proc_open.c:1144`) in the
/// child, between `fork(2)` and `exec(2)`: each `(from, to)` pair is
/// `dup2(from, to)`, and a pair already in place has its close-on-exec
/// bit cleared, since every fd this process makes carries one. The fds
/// named must stay open until the command is spawned.
pub(super) fn place_fds(command: &mut std::process::Command, moves: Vec<(RawFd, RawFd)>) {
    use std::os::unix::process::CommandExt;
    let placed = move || {
        for &(from, to) in &moves {
            let ret = if from == to {
                // SAFETY: `fcntl` with `F_SETFD` reads nothing through
                // its arguments; it is async-signal-safe, as the child
                // between `fork` and `exec` needs.
                unsafe { libc::fcntl(from, libc::F_SETFD, 0) }
            } else {
                // SAFETY: `dup2` reads nothing through its arguments and
                // is async-signal-safe.
                unsafe { libc::dup2(from, to) }
            };
            if ret == -1 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    };
    // SAFETY: the closure calls `dup2` and `fcntl` alone, both
    // async-signal-safe, touches no lock and allocates nothing, which is
    // what a closure run between `fork` and `exec` may do.
    unsafe { command.pre_exec(placed) };
}

/// The process's working directory, the one a child started with no
/// `chdir` inherits, as the kernel names it: `proc_pidinfo(2)`'s
/// `PROC_PIDVNODEPATHINFO` on macOS, whose `getcwd(3)` opens `.` to name
/// it and so fails where the caller may not read the directory, which
/// is the case a child that inherits it must still start in.
#[cfg(target_os = "macos")]
pub(super) fn process_dir() -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let pid = c_int::try_from(std::process::id()).ok()?;
    let size = c_int::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    // SAFETY: `info` is live storage of `size` bytes, which the call
    // writes within and nothing else.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote != size {
        return None;
    }
    // SAFETY: the struct is integers and arrays of them, valid zeroed,
    // and the call answered that it wrote all of it.
    let info = unsafe { info.assume_init() };
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .as_flattened()
        .iter()
        .map(|c| c.to_ne_bytes()[0])
        .take_while(|b| *b != 0)
        .collect();
    (!path.is_empty()).then(|| std::ffi::OsString::from_vec(path).into())
}

/// The process's working directory, the one a child started with no
/// `chdir` inherits: `getcwd(3)`, which the kernel answers without
/// reading the directory.
#[cfg(not(target_os = "macos"))]
pub(super) fn process_dir() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}

/// The `envp` a child is started with, each `name=value` in the order
/// given, which php hands `posix_spawn` and `execve` as it is: made before
/// the fork and made the child's `environ` between `fork` and `exec`,
/// since the standard library's own `envp` sorts the names. A later call
/// stands over an earlier one, its hook called after it.
pub(super) fn environment(command: &mut std::process::Command, pairs: &[(Vec<u8>, Vec<u8>)]) {
    use std::os::unix::process::CommandExt;
    let strings: Vec<std::ffi::CString> = pairs
        .iter()
        .filter_map(|(name, value)| {
            let mut pair = name.clone();
            pair.push(b'=');
            pair.extend_from_slice(value);
            std::ffi::CString::new(pair).ok()
        })
        .collect();
    let mut addresses: Vec<usize> = strings.iter().map(|s| s.as_ptr() as usize).collect();
    addresses.push(0);
    let envp = Envp { strings, addresses };
    let set = move || {
        envp.install();
        Ok(())
    };
    // SAFETY: the closure writes one pointer to `environ`, which is
    // async-signal-safe, touches no lock and allocates nothing, which is
    // what a closure run between `fork` and `exec` may do; the strings and
    // the array it points `environ` at are the closure's own, alive until
    // the child execs.
    unsafe { command.pre_exec(set) };
}

/// A child's `envp`: the strings, and their addresses ended by a null.
struct Envp {
    strings: Vec<std::ffi::CString>,
    addresses: Vec<usize>,
}

impl Envp {
    /// `environ` pointed at the array, which the strings back.
    fn install(&self) {
        debug_assert_eq!(self.strings.len() + 1, self.addresses.len());
        set_environ(
            self.addresses
                .as_ptr()
                .cast::<*mut libc::c_char>()
                .cast_mut(),
        );
    }
}

/// `environ = envp`.
#[cfg(target_os = "macos")]
fn set_environ(envp: *mut *mut libc::c_char) {
    // SAFETY: `_NSGetEnviron` answers the address of the C runtime's
    // `environ` and reads nothing.
    let at = unsafe { libc::_NSGetEnviron() };
    // SAFETY: `at` is `environ`'s address, which only this child's thread
    // reads before it execs.
    unsafe { *at = envp };
}

#[cfg(not(target_os = "macos"))]
unsafe extern "C" {
    static mut environ: *mut *mut libc::c_char;
}

/// `environ = envp`.
#[cfg(not(target_os = "macos"))]
fn set_environ(envp: *mut *mut libc::c_char) {
    // SAFETY: `environ` is the C library's, which only this child's thread
    // reads before it execs.
    unsafe { environ = envp };
}

/// `socketpair(domain, type, protocol, pair)`: the two ends of a new pair
/// of connected sockets, close-on-exec, or the `errno` of its `-1`.
pub(super) fn socketpair(
    domain: i32,
    kind: i32,
    protocol: i32,
) -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut pair = [-1; 2];
    // SAFETY: `pair` is room for the two `int`s the call writes.
    let ret = unsafe { libc::socketpair(domain, kind, protocol, pair.as_mut_ptr()) };
    if ret == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `socketpair` answered 0, so `pair[0]` is an open socket it
    // made and nothing else owns.
    let first = unsafe { std::os::fd::OwnedFd::from_raw_fd(pair[0]) };
    // SAFETY: as `first`, for `pair[1]`.
    let second = unsafe { std::os::fd::OwnedFd::from_raw_fd(pair[1]) };
    for fd in [&first, &second] {
        use std::os::fd::AsRawFd;
        // SAFETY: `fcntl` with `F_SETFD` reads nothing through its
        // arguments but a number.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((first, second))
}

// ---------------------------------------------------------------- host facts

// The calls the natives of `basic_functions.c` and its neighbours make
// for facts only the host has, each one call; each answer is the host's,
// which a witness pins by shape.

/// `getloadavg(load, 3)`: the three averages, or `None` where the call
/// answers `-1` (`PHP_FUNCTION(sys_getloadavg)`).
pub(super) fn loadavg() -> Option<[f64; 3]> {
    let mut load = [0.0f64; 3];
    // SAFETY: `load` has room for the three doubles the call is asked for.
    let got = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    (got != -1).then_some(load)
}

/// Whose usage `getrusage` reads, `RUSAGE_SELF` or `RUSAGE_CHILDREN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Who {
    Process,
    Children,
}

/// `struct rusage`'s fields in the order `PHP_FUNCTION(getrusage)` adds
/// them (`Z/ext/standard/microtime.c:115-140`), each under its C name.
// `tv_usec` is `suseconds_t`, `i32` on Darwin and `i64` on Linux, so its
// widening is a conversion on one host and the same type on the other.
#[allow(clippy::useless_conversion)]
pub(super) fn rusage(who: Who) -> Option<[(&'static str, i64); 17]> {
    let who = match who {
        Who::Process => libc::RUSAGE_SELF,
        Who::Children => libc::RUSAGE_CHILDREN,
    };
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is room for the one `struct rusage` the call writes.
    let rc = unsafe { libc::getrusage(who, usage.as_mut_ptr()) };
    if rc == -1 {
        return None;
    }
    // SAFETY: zeroed storage is a valid `rusage`, and the call succeeded
    // over it.
    let u = unsafe { usage.assume_init() };
    Some([
        ("ru_oublock", u.ru_oublock),
        ("ru_inblock", u.ru_inblock),
        ("ru_msgsnd", u.ru_msgsnd),
        ("ru_msgrcv", u.ru_msgrcv),
        ("ru_maxrss", u.ru_maxrss),
        ("ru_ixrss", u.ru_ixrss),
        ("ru_idrss", u.ru_idrss),
        ("ru_minflt", u.ru_minflt),
        ("ru_majflt", u.ru_majflt),
        ("ru_nsignals", u.ru_nsignals),
        ("ru_nvcsw", u.ru_nvcsw),
        ("ru_nivcsw", u.ru_nivcsw),
        ("ru_nswap", u.ru_nswap),
        ("ru_utime.tv_usec", i64::from(u.ru_utime.tv_usec)),
        ("ru_utime.tv_sec", u.ru_utime.tv_sec),
        ("ru_stime.tv_usec", i64::from(u.ru_stime.tv_usec)),
        ("ru_stime.tv_sec", u.ru_stime.tv_sec),
    ])
}

/// What `statvfs(path)` says of the filesystem a path is on, as
/// `php_disk_total_space` and `php_disk_free_space` read it: blocks
/// counted in `f_frsize`, or `f_bsize` where that is zero.
pub(super) struct Space {
    pub(super) total: f64,
    pub(super) available: f64,
}

/// `statvfs(3)`: the space, or the `errno` of its failure.
pub(super) fn statvfs(path: &[u8]) -> Result<Space, i32> {
    let path = CString::new(path).map_err(|_| libc::ENOENT)?;
    let mut buf = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `buf` is room for the one
    // `struct statvfs` the call writes.
    let rc = unsafe { libc::statvfs(path.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    // SAFETY: zeroed storage is a valid `statvfs`, and the call
    // succeeded over it.
    let buf = unsafe { buf.assume_init() };
    // php's own `(double)` casts, over counts `u32` on one host and
    // `u64` on another, so the widening is not the same on each.
    #[allow(clippy::cast_precision_loss)]
    let double = |n: u64| n as f64;
    #[allow(clippy::useless_conversion)]
    let (bsize, frsize, blocks, bavail) = (
        u64::from(buf.f_bsize),
        u64::from(buf.f_frsize),
        u64::from(buf.f_blocks),
        u64::from(buf.f_bavail),
    );
    let unit = if buf.f_frsize == 0 {
        double(bsize)
    } else {
        double(frsize)
    };
    Ok(Space {
        total: double(blocks) * unit,
        available: double(bavail) * unit,
    })
}

/// The login name of `uid`, `getpwuid_r(3)` with its buffer grown while
/// it is too small, as `php_get_current_user` reads it; `None` where
/// the host has no entry.
pub(super) fn user_name(uid: u32) -> Option<Vec<u8>> {
    let mut buf: Vec<c_char> = vec![0; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to live storage of the size given, and
        // the call writes only `entry`, `buf` up to its length and `found`.
        let err = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut found,
            )
        };
        if err == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if err != 0 || found.is_null() {
            return None;
        }
        // SAFETY: the call returned 0 with `found` set, so it filled
        // `entry`, whose `pw_name` points into `buf`.
        let entry = unsafe { entry.assume_init() };
        // SAFETY: `pw_name` is the NUL-terminated name the call wrote
        // into `buf`, which lives across this read.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
        return Some(name.to_bytes().to_vec());
    }
}

/// `ftok(pathname, proj)`: the key, or the `errno` of its `-1`; `proj`
/// is a C `char`, signed on the hosts php builds for here.
pub(super) fn ftok(path: &[u8], proj: u8) -> Result<i64, i32> {
    let path = CString::new(path).map_err(|_| libc::ENOENT)?;
    // SAFETY: `path` is NUL-terminated and outlives the call, which reads
    // it and nothing else.
    let key = unsafe { libc::ftok(path.as_ptr(), c_int::from(i8::from_ne_bytes([proj]))) };
    if key == -1 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(i64::from(key))
}

/// `inet_ntop(af, address, buffer, 40)` of `PHP_FUNCTION(inet_ntop)`:
/// four bytes as `AF_INET`, sixteen as `AF_INET6`; `None` where the call
/// answers `NULL`.
pub(super) fn inet_text(address: &[u8]) -> Option<Vec<u8>> {
    let af = match address.len() {
        4 => libc::AF_INET,
        16 => libc::AF_INET6,
        _ => return None,
    };
    let mut buffer = [0 as c_char; 40];
    let size = libc::socklen_t::try_from(buffer.len()).ok()?;
    // SAFETY: `address` holds the four or sixteen bytes `af` reads, and
    // `buffer` has room for `size` bytes, the most the call writes.
    let text = unsafe {
        inet_ntop(
            af,
            address.as_ptr().cast::<c_void>(),
            buffer.as_mut_ptr(),
            size,
        )
    };
    if text.is_null() {
        return None;
    }
    // SAFETY: a non-null answer is `buffer`, NUL-terminated by the call.
    let text = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(text.to_bytes().to_vec())
}

/// `inet_pton(af, address, buffer)` of `PHP_FUNCTION(inet_pton)`: the
/// four or sixteen bytes, `AF_INET6` where the text holds a `:`; `None`
/// where the call answers zero or less.
pub(super) fn inet_bytes(address: &[u8], six: bool) -> Option<Vec<u8>> {
    let text = CString::new(address).ok()?;
    let (af, len) = if six {
        (libc::AF_INET6, 16)
    } else {
        (libc::AF_INET, 4)
    };
    let mut buffer = [0u8; 17];
    // SAFETY: `text` is NUL-terminated and `buffer` has room for the
    // sixteen bytes the widest family writes.
    let rc = unsafe { inet_pton(af, text.as_ptr(), buffer.as_mut_ptr().cast::<c_void>()) };
    (rc > 0).then(|| buffer[..len].to_vec())
}
