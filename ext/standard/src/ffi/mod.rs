//! The C library calls `filestat.c` makes that Rust's `std` has no
//! answer for (design §4.1, `unsafe.homes`' `ext/*/src/ffi/`):
//! `access(2)`, which is what `file_exists`, `is_readable`,
//! `is_writable` and `is_executable` ask of a plain file; `getuid(2)`,
//! `getgid(2)` and `getgroups(2)`, which choose the permission bits those
//! read of any other wrapper's file; `getpwnam_r(3)` and `getgrnam_r(3)`
//! for `chown` and `chgrp` by name; and `utime(2)`, which `touch` sets
//! the times with. Each is one call, and a path reaches it as the bytes
//! the host has.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{self, NonNull};

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

/// `getuid()`.
pub(super) fn uid() -> u32 {
    // SAFETY: `getuid` takes nothing and cannot fail.
    unsafe { libc::getuid() }
}

/// `getgid()`.
pub(super) fn gid() -> u32 {
    // SAFETY: `getgid` takes nothing and cannot fail.
    unsafe { libc::getgid() }
}

/// `getgroups()`: the supplementary groups, asked for their count first
/// and then into room for that many, as `php_stat` does.
pub(super) fn groups() -> Vec<u32> {
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
/// bytes. It cannot fail.
pub(super) fn random_bytes(buf: &mut [u8]) {
    // SAFETY: `buf` is live, writable storage of exactly the length
    // given, which is all the call writes.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) }
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
    let errno = unsafe { libc::__error() };
    // SAFETY: that location is live and the thread's own for its life;
    // writing zero to it is what C's `errno = 0` does.
    unsafe { *errno = 0 };
    // SAFETY: `nice` takes a number and changes the process's priority,
    // nothing in this program's memory.
    unsafe { libc::nice(increment) };
    std::io::Error::last_os_error().raw_os_error() == Some(0)
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

/// `syslog(priority, "%.*s", len, message)`: one message, its bytes as
/// they are, NUL ones included in the length.
pub(super) fn syslog(priority: i32, message: &[u8]) {
    let len = libc::c_int::try_from(message.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: the format is NUL-terminated and asks for one `int` and one
    // pointer to at most `len` bytes, which `message` holds and outlives
    // the call.
    unsafe { libc::syslog(priority, c"%.*s".as_ptr(), len, message.as_ptr()) };
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

/// `strptime(s, format, &tm)` over a zeroed `tm`: the eight fields
/// `strptime()` answers, `tm_sec` to `tm_yday`, and what was left of the
/// string, or `None` where the call answered null.
pub(super) fn strptime(s: &[u8], format: &[u8]) -> Option<([i32; 8], Vec<u8>)> {
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
    let tm = unsafe { tm.assume_init() };
    let rest = owned(rest)?;
    Some((
        [
            tm.tm_sec, tm.tm_min, tm.tm_hour, tm.tm_mday, tm.tm_mon, tm.tm_year, tm.tm_wday,
            tm.tm_yday,
        ],
        rest,
    ))
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
        let broadcast = flags & u32::try_from(libc::IFF_BROADCAST).unwrap_or(0) != 0;
        let ptp = flags & u32::try_from(libc::IFF_POINTOPOINT).unwrap_or(0) != 0;
        found.push(InterfaceAddress {
            name: owned(entry.ifa_name).unwrap_or_default(),
            flags,
            family,
            address: address_text(entry.ifa_addr),
            netmask: address_text(entry.ifa_netmask),
            broadcast: if broadcast {
                address_text(entry.ifa_dstaddr)
            } else {
                None
            },
            ptp: if ptp {
                address_text(entry.ifa_dstaddr)
            } else {
                None
            },
        });
        at = entry.ifa_next;
    }
    // SAFETY: `head` is the list `getifaddrs` made, freed once, and
    // nothing read from it is used after.
    unsafe { libc::freeifaddrs(head) };
    Ok(found)
}

// ---- dns.c: the darwin resolver of `<dns.h>`, `dn_expand` and
// `dn_skipname` from libresolv, `getaddrinfo`, `getnameinfo`,
// `inet_pton` and `gethostname`.

/// The opaque `dns_handle_t` of `<dns.h>`, `Z/ext/standard/dns.c:819`.
type DnsHandleT = *const c_void;

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
    fn res_9_dn_expand(
        msg: *const u8,
        eom: *const u8,
        src: *const u8,
        dst: *mut c_char,
        dstsiz: c_int,
    ) -> c_int;
    /// `dn_skipname`, which `<resolv.h>` renames `res_9_dn_skipname` on
    /// darwin; called at `Z/ext/standard/dns.c:996`.
    fn res_9_dn_skipname(ptr: *const u8, eom: *const u8) -> c_int;
}

unsafe extern "C" {
    /// `h_errno`, `<netdb.h>`, read by `php_dns_errno`
    /// (`Z/ext/standard/php_dns.h:27`): a plain global on darwin, set by
    /// `dns_search`.
    static mut h_errno: c_int;
    /// `inet_pton`, `<arpa/inet.h>`, called at `Z/ext/standard/dns.c:181`.
    fn inet_pton(af: c_int, src: *const c_char, dst: *mut c_void) -> c_int;
}

/// The `from`/`fromsize` pair php keeps for `dns_search` across one call of
/// a native (`Z/ext/standard/dns.c:817`): `fromsize` is set once and
/// `dns_search` may shrink it.
pub(super) struct DnsFrom {
    /// `struct sockaddr_storage from`.
    storage: libc::sockaddr_storage,
    /// `uint32_t fromsize = sizeof(from)`.
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
pub(super) struct DnsHandle(NonNull<c_void>);

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

impl Drop for DnsHandle {
    /// `dns_free`, `Z/ext/standard/php_dns.h:25`.
    fn drop(&mut self) {
        // SAFETY: the pointer came from `dns_open` and is freed once, here.
        unsafe { dns_free(self.0.as_ptr()) }
    }
}

/// The value of `h_errno`, `Z/ext/standard/php_dns.h:27`.
fn last_h_errno() -> c_int {
    let p = &raw const h_errno;
    // SAFETY: `h_errno` is a C `int` that lives for the whole process; it
    // is read by value, with no reference kept.
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
        res_9_dn_expand(
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
    unsafe { res_9_dn_skipname(range.start, range.end) }
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
