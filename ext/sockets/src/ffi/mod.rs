//! The calls `nix` does not wrap in the form php makes them (design §7.2,
//! revision 36; `unsafe.homes`' `ext/*/src/ffi/`), each one `libc` call
//! behind a safe function, and the one home of this crate's `unsafe`.
//!
//! A `Socket` holds its fd as php's `bsd_socket` does, a number
//! that may belong to a stream the socket was imported from or exported
//! to, and that stays the socket's number after that stream was closed;
//! so every call here takes the number, which `nix`'s `AsFd` forms could
//! only borrow from an owner the socket may not have. `nix`'s forms that
//! take a number take its enums for the domain, the type, the protocol
//! and the flags, which cannot carry a number a program passed and the
//! platform does not name, and its `recvfrom` passes no flags. So
//! `socket(2)`, `socketpair(2)`, `accept(2)`, `listen(2)`, `bind(2)`,
//! `connect(2)`, `write(2)`, `send(2)`, `recv(2)`, `sendto(2)`,
//! `recvfrom(2)`, `getsockname(2)`, `getpeername(2)`, `getsockopt(2)`,
//! `setsockopt(2)`, `fcntl(2)`, `select(2)`, `sockatmark(3)`,
//! `sendmsg(2)`, `recvmsg(2)` and `getaddrinfo(3)` are here, with the
//! one codec between a `sockaddr` and [`Addr`]; `h_errno` and
//! `hstrerror(3)` are the resolver's.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, c_char, c_int};
use std::mem::size_of;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::ptr;

use nix::errno::Errno;

/// A socket address as php reads one out of and writes one into a
/// `sockaddr`: the three families the extension speaks, and the family
/// number of any other, `0` for none at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Addr {
    V4(SocketAddrV4),
    V6(SocketAddrV6),
    /// `sun_path` up to its first NUL, as php reads it with `%s`.
    Unix(Vec<u8>),
    Other(i32),
}

/// `sizeof(((struct sockaddr_un *)0)->sun_path)`: what a path must be
/// shorter than.
pub(crate) const SUN_PATH: usize = {
    let probe = libc::sockaddr_un {
        #[cfg(target_os = "macos")]
        sun_len: 0,
        sun_family: 0,
        sun_path: [0; _],
    };
    probe.sun_path.len()
};

/// `offsetof(struct sockaddr_un, sun_path)`.
const SUN_PATH_AT: usize = size_of::<libc::sockaddr_un>() - SUN_PATH;

/// A family number as a `sa_family_t`.
fn sa_family(family: c_int) -> libc::sa_family_t {
    libc::sa_family_t::try_from(family).unwrap_or(0)
}

fn result(ret: c_int) -> Result<c_int, Errno> {
    if ret == -1 {
        Err(Errno::last())
    } else {
        Ok(ret)
    }
}

fn sized(ret: isize) -> Result<usize, Errno> {
    usize::try_from(ret).map_err(|_| Errno::last())
}

fn len_of<T>() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<T>()).unwrap_or(libc::socklen_t::MAX)
}

/// An empty `sockaddr_storage`.
fn storage() -> libc::sockaddr_storage {
    // SAFETY: `sockaddr_storage` is plain C data, for which all zeroes is a
    // value (the unspecified family).
    unsafe { std::mem::zeroed() }
}

/// `addr` written into a `sockaddr_storage` as php's callers fill theirs,
/// zeroed first, and its length: `sizeof` the family's struct for an inet
/// address, and for a path `offsetof(sun_path)` plus its bytes, the
/// length `socket_bind`, `socket_connect` and `SUN_LEN` give. A family
/// the extension does not speak is written as the family alone.
fn encode(addr: &Addr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut ss = storage();
    let len = match addr {
        Addr::V4(v4) => {
            let sin = libc::sockaddr_in {
                #[cfg(target_os = "macos")]
                sin_len: 0,
                sin_family: sa_family(libc::AF_INET),
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; _],
            };
            // SAFETY: a `sockaddr_in` fits in the storage, which is aligned
            // for every `sockaddr`.
            unsafe { ptr::write((&raw mut ss).cast::<libc::sockaddr_in>(), sin) };
            len_of::<libc::sockaddr_in>()
        }
        Addr::V6(v6) => {
            let sin6 = libc::sockaddr_in6 {
                #[cfg(target_os = "macos")]
                sin6_len: 0,
                sin6_family: sa_family(libc::AF_INET6),
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            };
            // SAFETY: a `sockaddr_in6` fits in the storage, which is aligned
            // for every `sockaddr`.
            unsafe { ptr::write((&raw mut ss).cast::<libc::sockaddr_in6>(), sin6) };
            len_of::<libc::sockaddr_in6>()
        }
        Addr::Unix(path) => {
            let mut sun = libc::sockaddr_un {
                #[cfg(target_os = "macos")]
                sun_len: 0,
                sun_family: sa_family(libc::AF_UNIX),
                sun_path: [0; _],
            };
            let n = path.len().min(SUN_PATH);
            for (slot, byte) in sun.sun_path.iter_mut().zip(&path[..n]) {
                *slot = c_char::from_ne_bytes([*byte]);
            }
            // SAFETY: a `sockaddr_un` fits in the storage, which is aligned
            // for every `sockaddr`.
            unsafe { ptr::write((&raw mut ss).cast::<libc::sockaddr_un>(), sun) };
            libc::socklen_t::try_from(SUN_PATH_AT + n).unwrap_or(0)
        }
        Addr::Other(family) => {
            ss.ss_family = sa_family(*family);
            len_of::<libc::sa_family_t>()
        }
    };
    (ss, len)
}

/// A `sockaddr` the kernel wrote, read as php reads each family.
fn decode(ss: &libc::sockaddr_storage) -> Addr {
    let at = ptr::from_ref(ss);
    match c_int::from(ss.ss_family) {
        libc::AF_INET => {
            // SAFETY: the storage holds a `sockaddr_in` where the family says
            // so, and is aligned for it.
            let sin = unsafe { ptr::read(at.cast::<libc::sockaddr_in>()) };
            Addr::V4(SocketAddrV4::new(
                Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes()),
                u16::from_be(sin.sin_port),
            ))
        }
        libc::AF_INET6 => {
            // SAFETY: the storage holds a `sockaddr_in6` where the family
            // says so, and is aligned for it.
            let sin6 = unsafe { ptr::read(at.cast::<libc::sockaddr_in6>()) };
            Addr::V6(SocketAddrV6::new(
                Ipv6Addr::from(sin6.sin6_addr.s6_addr),
                u16::from_be(sin6.sin6_port),
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            ))
        }
        libc::AF_UNIX => {
            // SAFETY: the storage holds a `sockaddr_un` where the family says
            // so, and is aligned for it.
            let sun = unsafe { ptr::read(at.cast::<libc::sockaddr_un>()) };
            Addr::Unix(
                sun.sun_path
                    .iter()
                    .take_while(|c| **c != 0)
                    .map(|c| c.to_ne_bytes()[0])
                    .collect(),
            )
        }
        family => Addr::Other(family),
    }
}

/// `socket(domain, type, protocol)`, whatever numbers the program passed.
pub(crate) fn socket(domain: i32, ty: i32, protocol: i32) -> Result<OwnedFd, Errno> {
    // SAFETY: `socket` reads nothing through its arguments, numbers.
    let fd = result(unsafe { libc::socket(domain, ty, protocol) })?;
    // SAFETY: `socket` answered a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `socketpair(domain, type, protocol, fds)`.
pub(crate) fn socketpair(domain: i32, ty: i32, protocol: i32) -> Result<[OwnedFd; 2], Errno> {
    let mut fds: [c_int; 2] = [-1, -1];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    result(unsafe { libc::socketpair(domain, ty, protocol, fds.as_mut_ptr()) })?;
    // SAFETY: `socketpair` answered two new descriptors nothing else owns.
    let first = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: as the first.
    let second = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok([first, second])
}

/// `accept(fd, &la, &la_len)`: the new fd and `la->sa_family`.
pub(crate) fn accept(fd: RawFd) -> Result<(OwnedFd, i32), Errno> {
    let mut ss = storage();
    let mut len = len_of::<libc::sockaddr_storage>();
    // SAFETY: `ss` is a live `sockaddr_storage` of `len` bytes, which the
    // call writes the peer's address into.
    let new = result(unsafe { libc::accept(fd, (&raw mut ss).cast(), &raw mut len) })?;
    // SAFETY: `accept` answered a new descriptor nothing else owns.
    let owned = unsafe { OwnedFd::from_raw_fd(new) };
    Ok((owned, c_int::from(ss.ss_family)))
}

/// `listen(fd, backlog)`.
pub(crate) fn listen(fd: RawFd, backlog: i32) -> Result<(), Errno> {
    // SAFETY: `listen` reads nothing through its arguments, numbers.
    result(unsafe { libc::listen(fd, backlog) }).map(drop)
}

/// `bind(fd, addr, len)`.
pub(crate) fn bind(fd: RawFd, addr: &Addr) -> Result<(), Errno> {
    let (ss, len) = encode(addr);
    // SAFETY: `ss` is a live `sockaddr` of at least `len` bytes, read and
    // not kept.
    result(unsafe { libc::bind(fd, (&raw const ss).cast(), len) }).map(drop)
}

/// `connect(fd, addr, len)`.
pub(crate) fn connect(fd: RawFd, addr: &Addr) -> Result<(), Errno> {
    let (ss, len) = encode(addr);
    // SAFETY: `ss` is a live `sockaddr` of at least `len` bytes, read and
    // not kept.
    result(unsafe { libc::connect(fd, (&raw const ss).cast(), len) }).map(drop)
}

/// `write(fd, buf, len)`.
pub(crate) fn write(fd: RawFd, buf: &[u8]) -> Result<usize, Errno> {
    // SAFETY: `buf` is `buf.len()` readable bytes.
    sized(unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) })
}

/// `send(fd, buf, len, flags)`.
pub(crate) fn send(fd: RawFd, buf: &[u8], flags: i32) -> Result<usize, Errno> {
    // SAFETY: `buf` is `buf.len()` readable bytes.
    sized(unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), flags) })
}

/// `recv(fd, buf, len, flags)`.
pub(crate) fn recv(fd: RawFd, buf: &mut [u8], flags: i32) -> Result<usize, Errno> {
    // SAFETY: `buf` is `buf.len()` writable bytes.
    sized(unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), flags) })
}

/// `sendto(fd, buf, len, flags, addr, addr_len)`.
pub(crate) fn sendto(fd: RawFd, buf: &[u8], flags: i32, to: &Addr) -> Result<usize, Errno> {
    let (ss, len) = encode(to);
    // SAFETY: `buf` is `buf.len()` readable bytes and `ss` a live `sockaddr`
    // of at least `len` bytes, both read and not kept.
    let sent = unsafe {
        libc::sendto(
            fd,
            buf.as_ptr().cast(),
            buf.len(),
            flags,
            (&raw const ss).cast(),
            len,
        )
    };
    sized(sent)
}

/// `recvfrom(fd, buf, len, flags, &from, &from_len)` with `from` of
/// `family`'s size, its family set first as php sets it: the count and
/// the address the call left.
pub(crate) fn recvfrom(
    fd: RawFd,
    buf: &mut [u8],
    flags: i32,
    family: i32,
) -> Result<(usize, Addr), Errno> {
    let mut ss = storage();
    ss.ss_family = sa_family(family);
    let mut len = match family {
        libc::AF_INET => len_of::<libc::sockaddr_in>(),
        libc::AF_INET6 => len_of::<libc::sockaddr_in6>(),
        _ => len_of::<libc::sockaddr_un>(),
    };
    // SAFETY: `buf` is `buf.len()` writable bytes and `ss` a live
    // `sockaddr_storage` of at least `len` bytes, which the call writes.
    let got = unsafe {
        libc::recvfrom(
            fd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            flags,
            (&raw mut ss).cast(),
            &raw mut len,
        )
    };
    let n = sized(got)?;
    Ok((n, decode(&ss)))
}

/// `getsockname(fd, sa, &len)` or, with `peer`, `getpeername`.
pub(crate) fn name(fd: RawFd, peer: bool) -> Result<Addr, Errno> {
    let mut ss = storage();
    let mut len = len_of::<libc::sockaddr_storage>();
    let at = (&raw mut ss).cast::<libc::sockaddr>();
    let ret = if peer {
        // SAFETY: `ss` is a live `sockaddr_storage` of `len` bytes.
        unsafe { libc::getpeername(fd, at, &raw mut len) }
    } else {
        // SAFETY: as `getpeername`.
        unsafe { libc::getsockname(fd, at, &raw mut len) }
    };
    result(ret)?;
    Ok(decode(&ss))
}

/// `getsockopt(fd, level, name, buf, &len)`: how many bytes of `buf` the
/// option filled.
pub(crate) fn getsockopt(fd: RawFd, level: i32, name: i32, buf: &mut [u8]) -> Result<usize, Errno> {
    let mut len = libc::socklen_t::try_from(buf.len()).unwrap_or(libc::socklen_t::MAX);
    // SAFETY: `buf` is `len` writable bytes.
    result(unsafe { libc::getsockopt(fd, level, name, buf.as_mut_ptr().cast(), &raw mut len) })?;
    Ok(usize::try_from(len).unwrap_or(0))
}

/// `setsockopt(fd, level, name, value, len)`.
pub(crate) fn setsockopt(fd: RawFd, level: i32, name: i32, value: &[u8]) -> Result<(), Errno> {
    let len = libc::socklen_t::try_from(value.len()).unwrap_or(libc::socklen_t::MAX);
    // SAFETY: `value` is `len` readable bytes, read and not kept.
    result(unsafe { libc::setsockopt(fd, level, name, value.as_ptr().cast(), len) }).map(drop)
}

/// `fcntl(fd, F_GETFL)`.
pub(crate) fn status_flags(fd: RawFd) -> Result<i32, Errno> {
    // SAFETY: `F_GETFL` reads nothing through its arguments.
    result(unsafe { libc::fcntl(fd, libc::F_GETFL) })
}

/// Whether `fcntl(fd, F_GETFL)` has `O_NONBLOCK`.
pub(crate) const fn is_nonblocking(flags: i32) -> bool {
    flags & libc::O_NONBLOCK != 0
}

/// `php_set_sock_blocking` (`Z/main/network.c:1080`): `O_NONBLOCK`
/// cleared or set over the flags the fd has.
pub(crate) fn set_blocking(fd: RawFd, block: bool) -> Result<(), Errno> {
    let flags = status_flags(fd)?;
    let flags = if block {
        flags & !libc::O_NONBLOCK
    } else {
        flags | libc::O_NONBLOCK
    };
    // SAFETY: `F_SETFL` reads nothing through its arguments, numbers.
    result(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }).map(drop)
}

/// `FD_CLOEXEC` set on an fd `accept` gave, where `accept4` is
/// not, as `php_accept_connect` sets it.
pub(crate) fn set_cloexec(fd: RawFd) -> Result<(), Errno> {
    // SAFETY: `F_GETFD` reads nothing through its arguments.
    let mode = result(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
    if mode & libc::FD_CLOEXEC == 0 {
        // SAFETY: `F_SETFD` reads nothing through its arguments, numbers.
        result(unsafe { libc::fcntl(fd, libc::F_SETFD, mode | libc::FD_CLOEXEC) })?;
    }
    Ok(())
}

unsafe extern "C" {
    /// `<sys/socket.h>`'s `sockatmark`, which the `libc` crate does not
    /// declare for every target.
    fn sockatmark(fd: c_int) -> c_int;
}

/// `sockatmark(fd)`.
pub(crate) fn at_mark(fd: RawFd) -> Result<bool, Errno> {
    // SAFETY: `sockatmark` reads nothing through its argument, a number.
    result(unsafe { sockatmark(fd) }).map(|r| r != 0)
}

/// `FD_SETSIZE`, the first fd an `fd_set` has no bit for.
pub(crate) const FD_SETSIZE: RawFd = 1024;

fn fd_set(fds: &[RawFd]) -> libc::fd_set {
    let mut set = std::mem::MaybeUninit::<libc::fd_set>::uninit();
    // SAFETY: `FD_ZERO` writes every byte of the set it is handed, live
    // storage of an `fd_set`'s size.
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

fn is_set(set: &libc::fd_set, fd: RawFd) -> bool {
    // SAFETY: `fd` is in `0..FD_SETSIZE`, a bit the set has, and `FD_ISSET`
    // only reads it.
    (0..FD_SETSIZE).contains(&fd) && unsafe { libc::FD_ISSET(fd, set) }
}

/// `select(max_fd + 1, &rfds, &wfds, &efds, tv_p)`: the count, and for
/// each set whether each of its fds is left set.
pub(crate) fn select(
    sets: [&[RawFd]; 3],
    timeout: Option<(i64, i64)>,
) -> Result<(i32, [Vec<bool>; 3]), Errno> {
    let max_fd = sets.iter().copied().flatten().copied().max().unwrap_or(0);
    let mut built = sets.map(fd_set);
    let mut tv = timeout.map(|(sec, usec)| libc::timeval {
        tv_sec: libc::time_t::try_from(sec).unwrap_or(libc::time_t::MAX),
        tv_usec: libc::suseconds_t::try_from(usec).unwrap_or(0),
    });
    let tv_p = tv.as_mut().map_or(ptr::null_mut(), ptr::from_mut);
    let [r, w, e] = &mut built;
    // SAFETY: the three sets and the `timeval`, where there is one, are
    // live storage the call reads and writes and nothing else holds.
    let ret = result(unsafe { libc::select(max_fd + 1, r, w, e, tv_p) })?;
    let mut ready: [Vec<bool>; 3] = Default::default();
    for ((out, set), fds) in ready.iter_mut().zip(&built).zip(sets) {
        *out = fds.iter().map(|&fd| is_set(set, fd)).collect();
    }
    Ok((ret, ready))
}

/// The ancillary data's layout, `CMSG_ALIGN` and the header
/// `CMSG_DATA` skips, which the `libc` crate gives only as `unsafe`
/// functions.
pub(crate) mod cmsg {
    use std::mem::size_of;

    /// `CMSG_ALIGN`'s unit: a `uint32_t` on darwin, a `size_t` elsewhere.
    #[cfg(target_os = "macos")]
    const UNIT: usize = size_of::<u32>();
    #[cfg(not(target_os = "macos"))]
    const UNIT: usize = size_of::<usize>();

    /// `CMSG_ALIGN(len)`.
    pub(crate) const fn align(len: usize) -> usize {
        (len + UNIT - 1) & !(UNIT - 1)
    }

    /// `sizeof(struct cmsghdr)`.
    pub(crate) const HEADER: usize = size_of::<libc::cmsghdr>();

    /// `CMSG_DATA(cmsg) - cmsg`.
    pub(crate) const DATA_AT: usize = align(HEADER);

    /// `CMSG_SPACE(len)`.
    pub(crate) const fn space(len: usize) -> usize {
        align(HEADER) + align(len)
    }

    /// `CMSG_LEN(len)`.
    pub(crate) const fn len(len: usize) -> usize {
        align(HEADER) + len
    }

    /// A header written at the front of `out`: `cmsg_len`, `cmsg_level`
    /// and `cmsg_type` in the platform's widths.
    pub(crate) fn put_header(out: &mut [u8], length: usize, level: i32, ty: i32) {
        let len_width = size_of::<libc::socklen_t>().max(HEADER - 2 * size_of::<i32>());
        let length = length.to_ne_bytes();
        #[cfg(target_endian = "little")]
        let length = &length[..len_width];
        #[cfg(target_endian = "big")]
        let length = &length[length.len() - len_width..];
        out[..len_width].copy_from_slice(length);
        out[len_width..len_width + 4].copy_from_slice(&level.to_ne_bytes());
        out[len_width + 4..len_width + 8].copy_from_slice(&ty.to_ne_bytes());
    }

    /// The header at the front of `at`: `cmsg_len`, `cmsg_level` and
    /// `cmsg_type`.
    pub(crate) fn header(at: &[u8]) -> (usize, i32, i32) {
        let len_width = HEADER - 2 * size_of::<i32>();
        let mut length = [0u8; size_of::<usize>()];
        #[cfg(target_endian = "little")]
        length[..len_width].copy_from_slice(&at[..len_width]);
        #[cfg(target_endian = "big")]
        length[size_of::<usize>() - len_width..].copy_from_slice(&at[..len_width]);
        let word = |from: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&at[from..from + 4]);
            i32::from_ne_bytes(b)
        };
        (
            usize::from_ne_bytes(length),
            word(len_width),
            word(len_width + 4),
        )
    }
}

/// What one `recvmsg(2)` came to: the count, the name where one was
/// asked for, the one buffer's bytes, the ancillary data the kernel
/// left, `msg_controllen` of it, and `msg_flags`.
pub(crate) struct Received {
    pub(crate) count: usize,
    pub(crate) name: Option<Addr>,
    pub(crate) data: Vec<u8>,
    pub(crate) control: Vec<u8>,
    pub(crate) flags: i32,
}

/// `sendmsg(fd, &msghdr, flags)` over the name, the buffers and the
/// ancillary data php's conversions laid out.
// `msg_iovlen` and `msg_controllen` are `size_t` on Linux and `int`/`socklen_t` on macOS: the conversion does work on one platform and none on the other.
#[allow(clippy::useless_conversion)]
pub(crate) fn sendmsg(
    fd: RawFd,
    name: Option<&Addr>,
    iov: &[Vec<u8>],
    control: &[u8],
    flags: i32,
) -> Result<usize, Errno> {
    let named = name.map(encode);
    let mut slices: Vec<libc::iovec> = iov
        .iter()
        .map(|buf| libc::iovec {
            iov_base: buf.as_ptr().cast_mut().cast(),
            iov_len: buf.len(),
        })
        .collect();
    let mut control = control.to_vec();
    let mut hdr = msghdr();
    if let Some((ss, len)) = &named {
        hdr.msg_name = ptr::from_ref(ss).cast_mut().cast();
        hdr.msg_namelen = *len;
    }
    if !slices.is_empty() {
        hdr.msg_iov = slices.as_mut_ptr();
        hdr.msg_iovlen = slices.len().try_into().unwrap_or(0);
    }
    if !control.is_empty() {
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = control.len().try_into().unwrap_or(0);
    }
    // SAFETY: every pointer the header holds is live storage of the length
    // beside it for the call, and `sendmsg` only reads through them.
    sized(unsafe { libc::sendmsg(fd, &raw const hdr, flags) })
}

fn msghdr() -> libc::msghdr {
    // SAFETY: `msghdr` is plain C data, for which all zeroes is the empty
    // header: no name, no buffers, no ancillary data.
    unsafe { std::mem::zeroed() }
}

/// `recvmsg(fd, &msghdr, flags)` into a name of `name_len` bytes where
/// one is asked for, one buffer of `size` bytes and `controllen` bytes
/// of ancillary room where some is.
// `msg_iovlen` and `msg_controllen` are `size_t` on Linux and `int`/`socklen_t` on macOS: the conversion does work on one platform and none on the other.
#[allow(clippy::useless_conversion)]
pub(crate) fn recvmsg(
    fd: RawFd,
    name_len: Option<usize>,
    size: usize,
    controllen: usize,
    flags: i32,
) -> Result<Received, Errno> {
    let mut ss = storage();
    let mut data = vec![0u8; size];
    let mut control = vec![0u8; controllen];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut hdr = msghdr();
    if let Some(len) = name_len {
        hdr.msg_name = (&raw mut ss).cast();
        hdr.msg_namelen = libc::socklen_t::try_from(len).unwrap_or(0);
    }
    hdr.msg_iov = &raw mut iov;
    hdr.msg_iovlen = 1;
    if controllen > 0 {
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = controllen.try_into().unwrap_or(0);
    }
    // SAFETY: every pointer the header holds is live storage of the length
    // beside it, which the call writes and nothing else holds meanwhile.
    let count = sized(unsafe { libc::recvmsg(fd, &raw mut hdr, flags) })?;
    let kept = usize::try_from(hdr.msg_controllen)
        .unwrap_or(0)
        .min(controllen);
    control.truncate(kept);
    Ok(Received {
        count,
        name: name_len.map(|_| decode(&ss)),
        data,
        control,
        flags: hdr.msg_flags,
    })
}

/// An fd `SCM_RIGHTS` handed this process, now owned here.
pub(crate) fn adopt(fd: RawFd) -> OwnedFd {
    // SAFETY: a descriptor the kernel installed for this process by
    // `recvmsg`, which nothing else in it owns yet.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// One `struct addrinfo` of a `getaddrinfo` list, copied out.
#[derive(Clone)]
pub(crate) struct AddrInfo {
    pub(crate) flags: i32,
    pub(crate) family: i32,
    pub(crate) socktype: i32,
    pub(crate) protocol: i32,
    pub(crate) canonname: Option<Vec<u8>>,
    pub(crate) addr: Addr,
}

/// The list `getaddrinfo` answers, freed when this drops.
struct List(*mut libc::addrinfo);

impl Drop for List {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the list `getaddrinfo` answered, freed once.
            unsafe { libc::freeaddrinfo(self.0) };
        }
    }
}

/// `getaddrinfo(host, service, &hints, &result)` with the four hints
/// php sets, `[flags, family, socktype, protocol]`: every entry of the
/// list copied, or the call's non-zero answer.
pub(crate) fn getaddrinfo(
    host: &CStr,
    service: Option<&CStr>,
    hints: [i32; 4],
) -> Result<Vec<AddrInfo>, i32> {
    let mut want = empty_hints();
    want.ai_flags = hints[0];
    want.ai_family = hints[1];
    want.ai_socktype = hints[2];
    want.ai_protocol = hints[3];
    let mut res: *mut libc::addrinfo = ptr::null_mut();
    let service = service.map_or(ptr::null(), CStr::as_ptr);
    // SAFETY: `host` and `service`, where there is one, are NUL-terminated,
    // `want` is a live `addrinfo` with null pointers and `res` a live out
    // pointer.
    let rc = unsafe { libc::getaddrinfo(host.as_ptr(), service, &raw const want, &raw mut res) };
    let list = List(res);
    if rc != 0 {
        return Err(rc);
    }
    let mut out = Vec::new();
    let mut cur = list.0.cast_const();
    while !cur.is_null() {
        // SAFETY: `cur` is a non-null node of the list, alive until `list`
        // drops.
        let ai = unsafe { &*cur };
        let canonname = (!ai.ai_canonname.is_null()).then(|| {
            // SAFETY: a non-null `ai_canonname` is a NUL-terminated string of
            // the list.
            unsafe { CStr::from_ptr(ai.ai_canonname) }
                .to_bytes()
                .to_vec()
        });
        let mut ss = storage();
        let len = usize::try_from(ai.ai_addrlen)
            .unwrap_or(0)
            .min(size_of::<libc::sockaddr_storage>());
        if !ai.ai_addr.is_null() {
            // SAFETY: `ai_addr` points at `ai_addrlen` bytes of the list and
            // `ss` has room for `len` of them; the two do not overlap.
            unsafe {
                ptr::copy_nonoverlapping(ai.ai_addr.cast::<u8>(), (&raw mut ss).cast::<u8>(), len);
            }
        }
        out.push(AddrInfo {
            flags: ai.ai_flags,
            family: ai.ai_family,
            socktype: ai.ai_socktype,
            protocol: ai.ai_protocol,
            canonname,
            addr: decode(&ss),
        });
        cur = ai.ai_next.cast_const();
    }
    Ok(out)
}

fn empty_hints() -> libc::addrinfo {
    // SAFETY: `addrinfo` is plain C data, for which all zeroes is the hints
    // of none: no flags, any family, null pointers.
    unsafe { std::mem::zeroed() }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `h_errno`, `<netdb.h>`: a plain global on darwin.
    static mut h_errno: c_int;
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    /// The calling thread's `h_errno`, `(*__h_errno_location ())` on glibc.
    fn __h_errno_location() -> *mut c_int;
}

/// `h_errno`, as `PHP_SOCKET_ERROR(.., -10000 - h_errno)` reads it.
#[cfg(target_os = "macos")]
pub(crate) fn h_errno_now() -> i32 {
    let at = &raw const h_errno;
    // SAFETY: `h_errno` is a C `int` that lives for the whole process, read
    // by value with no reference kept.
    unsafe { at.read() }
}

/// `h_errno`, as `PHP_SOCKET_ERROR(.., -10000 - h_errno)` reads it.
#[cfg(target_os = "linux")]
pub(crate) fn h_errno_now() -> i32 {
    // SAFETY: `__h_errno_location` answers the calling thread's `h_errno`,
    // valid for the thread's life.
    let at = unsafe { __h_errno_location() };
    // SAFETY: `at` is that live `int`, read by value.
    unsafe { at.read() }
}

/// `hstrerror(error)`: the resolver's text for the number.
pub(crate) fn hstrerror(error: i32) -> Vec<u8> {
    // SAFETY: `hstrerror` reads nothing through its argument and answers a
    // static string or null.
    let text = unsafe { libc::hstrerror(error) };
    if text.is_null() {
        return Vec::new();
    }
    // SAFETY: a non-null answer is a NUL-terminated static string.
    unsafe { CStr::from_ptr(text) }.to_bytes().to_vec()
}
