//! The three calls on a host fd the engine's streams layer makes that
//! Rust's `std` has no answer for (design §4.1, `unsafe.homes`): `poll(2)`
//! of one fd with a timeout, which `php_pollfd_for` is and which a
//! socket's timed read and `accept` wait on, and of many, which
//! `php_select` is for `mysqlnd_poll`; and `fcntl(2)`'s
//! `O_NONBLOCK`, which `PHP_STREAM_OPTION_BLOCKING` sets on a pipe, and
//! `FD_CLOEXEC`, cleared on a file php opens for an exec'd process to
//! inherit; and
//! `recv(2)` under `MSG_PEEK`, which `STREAM_PEEK` asks of any socket;
//! and `fsync(2)` and `fdatasync(2)`, which `fsync()` asks of a file;
//! and `dup(2)` of an fd named by its number with `getdtablesize(2)`,
//! which `php://fd/` opens; and the socket calls `main/network.c` makes
//! where `std`'s sockets decide for it, `socket(2)`, `setsockopt(2)`,
//! `bind(2)`, `listen(2)` and a connect within a timeout, so a `tcp://`
//! client binds where `socket.bindto` says and a server listens with
//! `socket.backlog`. Each is one call over an fd the caller holds open, or
//! names.

use std::net::SocketAddr;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// What [`poll_fd`] waits for: `PHP_POLLREADABLE`, which is `POLLIN`
/// with `POLLERR` and `POLLHUP`, or `POLLOUT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Readable,
    Writable,
}

/// `php_pollfd_for(fd, events, timeout)` (`Z/main/php_network.h:193`):
/// whether the fd became ready within `timeout`, `None` to wait for ever;
/// `Ok(false)` is the timeout, and the `Err` the `errno` of a `-1`,
/// `EINTR` among them.
pub fn poll_fd(
    fd: BorrowedFd<'_>,
    want: Readiness,
    timeout: Option<Duration>,
) -> std::io::Result<bool> {
    let events = match want {
        Readiness::Readable => libc::POLLIN | libc::POLLERR | libc::POLLHUP,
        Readiness::Writable => libc::POLLOUT,
    };
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    // `php_tvtoto`: whole milliseconds, rounded up where a remainder is.
    let ms = timeout.map_or(-1, |t| {
        let whole = t.as_millis() + u128::from(t.subsec_nanos() % 1_000_000 != 0);
        i32::try_from(whole).unwrap_or(i32::MAX)
    });
    // SAFETY: `p` is one live `pollfd`, which the call reads and whose
    // `revents` it writes, and the count says one.
    let n = unsafe { libc::poll(&raw mut p, 1, ms) };
    match n {
        -1 => Err(std::io::Error::last_os_error()),
        0 => Ok(false),
        _ => Ok(true),
    }
}

/// `select(2)` over the three sets `php_select` hands it (`Z/main/php_network.h:83`),
/// made of one `poll(2)`: each set cut to the fds that came ready within
/// `timeout`, `None` to wait for ever — read for `POLLIN`, with the
/// `POLLERR` and `POLLHUP` that `select` reports as readable, write for
/// `POLLOUT`, except for `POLLPRI` — and the count `select` answers, one
/// per fd for each set it stays in. The `Err` is the `errno` of a `-1`,
/// `EINTR` among them.
pub fn select_fds(sets: [&mut Vec<RawFd>; 3], timeout: Option<Duration>) -> std::io::Result<usize> {
    const WANT: [libc::c_short; 3] = [
        libc::POLLIN | libc::POLLERR | libc::POLLHUP,
        libc::POLLOUT,
        libc::POLLPRI,
    ];
    let mut polled: Vec<libc::pollfd> = Vec::new();
    for (set, want) in sets.iter().zip(WANT) {
        for fd in set.iter() {
            match polled.iter_mut().find(|p| p.fd == *fd) {
                Some(p) => p.events |= want,
                None => polled.push(libc::pollfd {
                    fd: *fd,
                    events: want,
                    revents: 0,
                }),
            }
        }
    }
    let ms = timeout.map_or(-1, |t| {
        let whole = t.as_millis() + u128::from(t.subsec_nanos() % 1_000_000 != 0);
        i32::try_from(whole).unwrap_or(i32::MAX)
    });
    let count = libc::nfds_t::try_from(polled.len()).unwrap_or(libc::nfds_t::MAX);
    // SAFETY: `polled` is `count` live `pollfd`s, which the call reads and
    // whose `revents` it writes.
    let n = unsafe { libc::poll(polled.as_mut_ptr(), count, ms) };
    if n == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let mut ready = 0;
    for (set, want) in sets.into_iter().zip(WANT) {
        set.retain(|fd| polled.iter().any(|p| p.fd == *fd && p.revents & want != 0));
        ready += set.len();
    }
    Ok(ready)
}

/// `PHP_STREAM_OPTION_BLOCKING` as `php_stdiop_set_option` answers it
/// (`Z/main/streams/plain_wrapper.c:803`): `O_NONBLOCK` cleared for
/// `blocking` and set otherwise, and whether the fd was blocking before;
/// the `Err` is the `errno` of a `-1` from either `fcntl(2)`.
pub fn set_fd_blocking(fd: BorrowedFd<'_>, blocking: bool) -> std::io::Result<bool> {
    let raw = fd.as_raw_fd();
    // SAFETY: `fcntl` with `F_GETFL` reads nothing through its arguments
    // and answers the flags or `-1`.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL, 0) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let was_blocking = flags & libc::O_NONBLOCK == 0;
    let flags = if blocking {
        flags & !libc::O_NONBLOCK
    } else {
        flags | libc::O_NONBLOCK
    };
    // SAFETY: `fcntl` with `F_SETFL` reads nothing through its arguments
    // but the flags, a number.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(was_blocking)
}

/// `FD_CLOEXEC` cleared, so a process the request execs inherits the fd,
/// as it inherits each one php opens with `open(2)` and no `O_CLOEXEC`:
/// a plain file `fopen()` opens without `e` in its `$mode`
/// (`php_stream_parse_fopen_modes`, `Z/main/streams/plain_wrapper.c:108`)
/// and the primary file php holds while the program is served. `std`
/// opens every file with `O_CLOEXEC`. The `Err` is the `errno` of a `-1`
/// from either `fcntl(2)`.
pub fn inherit_fd(fd: BorrowedFd<'_>) -> std::io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: `fcntl` with `F_GETFD` reads nothing through its arguments
    // and answers the fd's flags or `-1`.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fcntl` with `F_SETFD` reads nothing through its arguments
    // but the flags, a number.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `fsync(2)` of the fd, or `fdatasync(2)` for `data_only`, the call
/// `php_stdiop_sync` (`Z/main/streams/plain_wrapper.c:595`) makes for
/// `PHP_STREAM_OPTION_SYNC_API`; the `Err` is the `errno` of a `-1`,
/// `EINVAL` for a pipe or a socket. `std`'s `sync_all` is
/// `F_FULLFSYNC` on macOS, which is not the call php makes.
pub fn sync_fd(fd: BorrowedFd<'_>, data_only: bool) -> std::io::Result<()> {
    let raw = fd.as_raw_fd();
    let n = if data_only {
        // SAFETY: `fdatasync` reads nothing through its argument, a number.
        unsafe { fdatasync(raw) }
    } else {
        // SAFETY: `fsync` reads nothing through its argument, a number.
        unsafe { libc::fsync(raw) }
    };
    if n == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// The symbol is in macOS's C library but not in its header, which is how
// php declares it there (`Z/main/streams/plain_wrapper.c:68`).
#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    fn fdatasync(fd: libc::c_int) -> libc::c_int;
}
#[cfg(not(target_vendor = "apple"))]
use libc::fdatasync;

/// `recv(fd, buf, len, MSG_PEEK)`: the bytes queued on a socket, left
/// queued; how many there were, or the `errno` of a `-1`.
pub fn peek_fd(fd: BorrowedFd<'_>, buf: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: `buf` is live storage of the length given, which is all the
    // call writes.
    let n = unsafe {
        libc::recv(
            fd.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_PEEK,
        )
    };
    usize::try_from(n).map_err(|_| std::io::Error::last_os_error())
}

/// `getdtablesize(2)`: one more than the highest fd number the process
/// may have open, which `php://fd/` holds a number below.
#[must_use]
pub fn fd_table_size() -> i32 {
    // SAFETY: `getdtablesize` reads and writes nothing and answers a
    // number.
    unsafe { libc::getdtablesize() }
}

/// `dup(2)` of the fd numbered `fd`, the copy owned by the caller; the
/// `Err` is the `errno` of a `-1`, `EBADF` for a number no fd has.
pub fn dup_fd(fd: RawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: `dup` reads nothing through its argument, a number, and
    // answers a new fd or `-1`.
    let copy = unsafe { libc::dup(fd) };
    if copy == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `copy` is the fd `dup` just made, open and owned by nobody
    // else, which the `OwnedFd` closes once.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// `BUFSIZ` of this host's `<stdio.h>`, the size php hands a user-space
/// stream's `stream_set_option` for a buffer option given none
/// (`user_stream_set_option`).
#[allow(clippy::cast_lossless)] // a `c_uint` on one host and a `c_int` on another
pub const BUFSIZ: i64 = libc::BUFSIZ as i64;

/// A socket option `main/network.c` sets on or off, `int val = 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockOpt {
    /// `SO_REUSEADDR`, which every bound socket gets.
    SoReuseaddr,
    /// `SO_REUSEPORT`, `socket.so_reuseport`.
    SoReuseport,
    /// `SO_BROADCAST`, `socket.so_broadcast`.
    SoBroadcast,
    /// `TCP_NODELAY`, `socket.tcp_nodelay`.
    TcpNodelay,
    /// `IPV6_V6ONLY`, `socket.ipv6_v6only`.
    Ipv6V6only,
}

/// `setsockopt(fd, level, name, &val, sizeof(val))` of an option set on
/// or off; the `Err` is the `errno` of a `-1`.
pub fn set_sockopt(fd: BorrowedFd<'_>, opt: SockOpt, on: bool) -> std::io::Result<()> {
    let (level, name) = match opt {
        SockOpt::SoReuseaddr => (libc::SOL_SOCKET, libc::SO_REUSEADDR),
        SockOpt::SoReuseport => (libc::SOL_SOCKET, libc::SO_REUSEPORT),
        SockOpt::SoBroadcast => (libc::SOL_SOCKET, libc::SO_BROADCAST),
        SockOpt::TcpNodelay => (libc::IPPROTO_TCP, libc::TCP_NODELAY),
        SockOpt::Ipv6V6only => (libc::IPPROTO_IPV6, libc::IPV6_V6ONLY),
    };
    let val = libc::c_int::from(on);
    let len = libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap_or(4);
    // SAFETY: `val` is one live `c_int`, which the call reads for `len`
    // bytes and does not keep.
    let n = unsafe { libc::setsockopt(fd.as_raw_fd(), level, name, (&raw const val).cast(), len) };
    if n == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `socket(family, socktype, 0)` for an address of `addr`'s family,
/// `SOCK_DGRAM` for `datagram`, closed on exec as `std` makes its own;
/// the `Err` is the `errno` of a `-1`.
pub fn new_socket(addr: &SocketAddr, datagram: bool) -> std::io::Result<OwnedFd> {
    let family = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let ty = if datagram {
        libc::SOCK_DGRAM
    } else {
        libc::SOCK_STREAM
    };
    // SAFETY: `socket` reads nothing through its arguments, three numbers,
    // and answers a new fd or `-1`.
    let raw = unsafe { libc::socket(family, ty, 0) };
    if raw == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is the fd `socket` just made, open and owned by nobody
    // else, which the `OwnedFd` closes once.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: `fcntl` with `F_SETFD` reads nothing through its arguments
    // but the flag, a number.
    if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// A `SocketAddr` as the `sockaddr_in` or `sockaddr_in6` the calls read.
enum RawAddr {
    V4(libc::sockaddr_in),
    V6(libc::sockaddr_in6),
}

impl RawAddr {
    fn of(addr: &SocketAddr) -> RawAddr {
        match addr {
            SocketAddr::V4(v4) => RawAddr::V4(libc::sockaddr_in {
                #[cfg(any(
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd"
                ))]
                sin_len: u8::try_from(size_of::<libc::sockaddr_in>()).unwrap_or(16),
                sin_family: libc::sa_family_t::try_from(libc::AF_INET).unwrap_or(2),
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            }),
            SocketAddr::V6(v6) => RawAddr::V6(libc::sockaddr_in6 {
                #[cfg(any(
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd"
                ))]
                sin6_len: u8::try_from(size_of::<libc::sockaddr_in6>()).unwrap_or(28),
                sin6_family: libc::sa_family_t::try_from(libc::AF_INET6).unwrap_or(30),
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            }),
        }
    }

    /// The address and its length, as `bind` and `connect` take them.
    fn parts(&self) -> (*const libc::sockaddr, libc::socklen_t) {
        match self {
            RawAddr::V4(a) => (
                (&raw const *a).cast(),
                libc::socklen_t::try_from(size_of::<libc::sockaddr_in>()).unwrap_or(16),
            ),
            RawAddr::V6(a) => (
                (&raw const *a).cast(),
                libc::socklen_t::try_from(size_of::<libc::sockaddr_in6>()).unwrap_or(28),
            ),
        }
    }
}

/// `bind(2)` of the socket to `addr`; the `Err` is the `errno` of a `-1`.
pub fn bind_fd(fd: BorrowedFd<'_>, addr: &SocketAddr) -> std::io::Result<()> {
    let raw = RawAddr::of(addr);
    let (at, len) = raw.parts();
    // SAFETY: `at` points into `raw`, a live address of `len` bytes, which
    // the call reads and does not keep.
    if unsafe { libc::bind(fd.as_raw_fd(), at, len) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `listen(2)` with `backlog`, `php_stream_xport_listen`'s; the `Err` is
/// the `errno` of a `-1`.
pub fn listen_fd(fd: BorrowedFd<'_>, backlog: i32) -> std::io::Result<()> {
    // SAFETY: `listen` reads nothing through its arguments, two numbers.
    if unsafe { libc::listen(fd.as_raw_fd(), backlog) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `php_network_connect_socket` (`Z/main/network.c:340`): the socket set
/// not to block and `connect(2)` begun; `asynchronous`, a connect still
/// in progress is left to finish and the socket not to block, the program
/// asking `stream_select` when it has; otherwise the socket polled for
/// writing within `timeout`, `None` to wait for ever, its `SO_ERROR` read,
/// and the socket set to block again. The `Ok` is what php leaves in
/// `*error_code`, `EINPROGRESS` over a connect left to finish and nothing
/// otherwise; the `Err` is the connect's `errno`, `ETIMEDOUT` where the
/// time ran out.
pub fn connect_fd(
    fd: BorrowedFd<'_>,
    addr: &SocketAddr,
    asynchronous: bool,
    timeout: Option<Duration>,
) -> std::io::Result<i32> {
    set_fd_blocking(fd, false)?;
    let raw = RawAddr::of(addr);
    let (at, len) = raw.parts();
    // SAFETY: `at` points into `raw`, a live address of `len` bytes, which
    // the call reads and does not keep.
    let n = unsafe { libc::connect(fd.as_raw_fd(), at, len) };
    if n == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        if asynchronous {
            return Ok(libc::EINPROGRESS);
        }
        if !poll_fd(fd.as_fd(), Readiness::Writable, timeout)? {
            return Err(std::io::Error::from_raw_os_error(libc::ETIMEDOUT));
        }
        let mut pending: libc::c_int = 0;
        let mut size = libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap_or(4);
        // SAFETY: `pending` and `size` are live, and the call writes at
        // most `size` bytes into `pending` and the length into `size`.
        let got = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut pending).cast(),
                &raw mut size,
            )
        };
        if got == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if pending != 0 {
            return Err(std::io::Error::from_raw_os_error(pending));
        }
    }
    if !asynchronous {
        set_fd_blocking(fd, true)?;
    }
    Ok(0)
}
