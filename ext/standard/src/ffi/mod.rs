//! The C library calls `filestat.c` makes that Rust's `std` has no
//! answer for (design §4.1, `unsafe.homes`' `ext/*/src/ffi/`):
//! `access(2)`, which is what `file_exists`, `is_readable`,
//! `is_writable` and `is_executable` ask of a plain file; `getuid(2)`,
//! `getgid(2)` and `getgroups(2)`, which choose the permission bits those
//! read of any other wrapper's file; `getpwnam_r(3)` and `getgrnam_r(3)`
//! for `chown` and `chgrp` by name; and `utime(2)`, which `touch` sets
//! the times with. Each is one call, and a path reaches it as the bytes
//! the host has. Beside them are the two `streamsfuncs.c` makes of a
//! stream's host fd: `select(2)`, which `php_select` is on every
//! platform php builds for here, over `fd_set`s built with `FD_SET`, and
//! `isatty(3)`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

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
        tv_sec: libc::time_t::try_from(sec).unwrap_or(libc::time_t::MAX),
        tv_usec: libc::suseconds_t::try_from(usec).unwrap_or(0),
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
