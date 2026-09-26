//! The three calls on a host fd the engine's streams layer makes that
//! Rust's `std` has no answer for (design §4.1, `unsafe.homes`): `poll(2)`
//! of one fd with a timeout, which `php_pollfd_for` is and which a
//! socket's timed read and `accept` wait on, and `fcntl(2)`'s
//! `O_NONBLOCK`, which `PHP_STREAM_OPTION_BLOCKING` sets on a pipe; and
//! `recv(2)` under `MSG_PEEK`, which `STREAM_PEEK` asks of any socket;
//! and `dup(2)` of an fd named by its number with `getdtablesize(2)`,
//! which `php://fd/` opens. Each is one call over an fd the caller holds
//! open, or names.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
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
