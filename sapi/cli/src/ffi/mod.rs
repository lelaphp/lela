//! What the CLI host asks of the C library that Rust's standard library
//! does not answer, the host's one `unsafe` home (ADR-0008): the
//! process's own fds 0, 1 and 2, which `do_cli` registers its standard
//! streams over, the area `ps` shows its title from ([`title`]), the
//! local time the built-in server's log writes, the `SIGINT` and the
//! wait its loop serves between, and the fds above 2 `php-linked` closes
//! before it serves (`close_inherited_fds`).
//! Each `unsafe` block is one operation with its `SAFETY` comment.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod title;

pub(crate) use title::save_ps_args;

use std::mem::MaybeUninit;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// fds 0, 1 and 2 as the request's own, each where it is open: what
/// `cli_register_file_handles` opens `php://stdin`, `stdout` and `stderr`
/// over, which an `fclose()` of the constant closes. The engine leaves
/// them open when the request ends ([`php_sapi::FileHandles::process`]),
/// so the next request of `--repeat` takes them again.
pub(crate) fn standard_fds() -> [Option<OwnedFd>; 3] {
    [0, 1, 2].map(|fd| {
        // SAFETY: `fcntl(F_GETFD)` reads the flags of the descriptor
        // number it is handed and touches no memory.
        let open = unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
        // SAFETY: the descriptor is open, and nothing in the process owns
        // fds 0, 1 and 2 but the request this is handed to: the standard
        // library's handles borrow them, and the request before this one
        // released its ownership without closing them.
        open.then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    })
}

/// `sapi_cli_single_write` over fd 1 until every byte went
/// (`Z/sapi/cli/php_cli.c:239`): `write(2)`, again where it was
/// interrupted, and after a wait for the fd where it would block; the
/// error of one the fd refused. What the standard library's stdout
/// hides, since it answers a closed fd 1 as though the bytes went.
pub(crate) fn write_stdout(mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: `write(2)` reads `bytes.len()` bytes from the slice,
        // which is live and that long for the call, and touches nothing
        // else.
        let wrote = unsafe { libc::write(1, bytes.as_ptr().cast(), bytes.len()) };
        if let Ok(wrote) = usize::try_from(wrote) {
            bytes = bytes.get(wrote..).unwrap_or_default();
            continue;
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock => wait_writable(),
            _ => return Err(error),
        }
    }
    Ok(())
}

/// `sapi_cli_select(STDOUT_FILENO)`: fd 1 waited for until it takes a
/// write, a minute at most, `default_socket_timeout` at its default, and
/// the write tried again either way, as php's loop tries it.
fn wait_writable() {
    let _ = writable_within(1, 60_000);
}

/// `php_pollfd_for(fd, POLLOUT, timeout)` (`Z/main/php_network.h:180`): whether `fd` takes a write
/// within `millis`; false where the wait ran out, failed or a signal cut
/// it short.
pub(crate) fn writable_within(fd: RawFd, millis: i32) -> bool {
    let mut polled = [Polled::new(fd, Interest::Write)];
    poll(&mut polled, millis).is_ok_and(|ready| ready > 0)
}

/// Set by the built-in server's `SIGINT` handler ([`stop_on_sigint`]).
static SIGINT_RAISED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `php_cli_server_sigint_handler`: the atomic the event loop reads.
extern "C" fn on_sigint(_: libc::c_int) {
    SIGINT_RAISED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// `signal(SIGINT, php_cli_server_sigint_handler)`
/// (`Z/sapi/cli/php_cli_server.c:2900`): from here a `SIGINT` sets
/// the atomic answered, which the built-in server's loop reads between
/// waits, where it would have ended the process.
pub(crate) fn stop_on_sigint() -> &'static std::sync::atomic::AtomicBool {
    let handler = on_sigint as extern "C" fn(libc::c_int);
    // SAFETY: `signal(2)` installs a handler that only stores to an
    // atomic, which is async-signal-safe, and touches no memory of ours.
    let _ = unsafe { libc::signal(libc::SIGINT, handler as libc::sighandler_t) };
    &SIGINT_RAISED
}

/// What a [`Polled`] fd is waited for: a read, as the built-in server's
/// poller watches a listening socket and a request being read, or a
/// write, as it watches a response it sends itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interest {
    Read,
    Write,
}

/// One fd as `poll(2)` takes it, `php_cli_server_poller`'s `rfds` or
/// `wfds` for one fd, with what the wait found.
#[repr(transparent)]
pub(crate) struct Polled(libc::pollfd);

impl Polled {
    pub(crate) fn new(fd: RawFd, interest: Interest) -> Polled {
        let events = match interest {
            Interest::Read => libc::POLLIN,
            Interest::Write => libc::POLLOUT,
        };
        Polled(libc::pollfd {
            fd,
            events,
            revents: 0,
        })
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.0.fd
    }

    /// Whether the wait found the fd ready for what it was waited for:
    /// as `select(2)` reports it, a peer that hung up or an error on the
    /// socket counted as ready, so the read or write that follows says
    /// which.
    pub(crate) fn ready(&self) -> bool {
        self.0.revents & (self.0.events | libc::POLLHUP | libc::POLLERR) != 0
    }
}

/// `php_cli_server_poller_poll` (`Z/sapi/cli/php_cli_server.c:876`): a
/// wait of at most `millis` for any of `fds` to be ready, and how many
/// are; the error where the wait failed, `EINTR` where a signal cut it
/// short, and `EBADF` where one of `fds` is not open, as `select(2)`
/// fails the whole wait over it.
pub(crate) fn poll(fds: &mut [Polled], millis: i32) -> std::io::Result<usize> {
    let Ok(count) = libc::nfds_t::try_from(fds.len()) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    // SAFETY: `poll(2)` reads and writes `count` `pollfd`s from the
    // pointer, the slice's own, live and that long for the call: a
    // `Polled` is a `pollfd` by `repr(transparent)`.
    let ready = unsafe { libc::poll(fds.as_mut_ptr().cast::<libc::pollfd>(), count, millis) };
    let ready = usize::try_from(ready).map_err(|_| std::io::Error::last_os_error())?;
    if fds.iter().any(|fd| fd.0.revents & libc::POLLNVAL != 0) {
        return Err(std::io::Error::from_raw_os_error(libc::EBADF));
    }
    Ok(ready)
}

/// Every fd above 2 the process was started holding, closed, as
/// a server that outlives its caller's other children does before it
/// serves. On macOS the standard library opens a pipe with
/// `pipe(2)` and marks it close-on-exec after, so a process another
/// thread spawns in between inherits that pipe, and holding it keeps the
/// pipe from reaching its end for whoever reads it. `/dev/fd` lists what
/// is open; called first in `main`, with no thread started and nothing
/// of this process's own open beyond the three, so each one closed is
/// one nothing here owns.
#[cfg(feature = "linked")]
pub fn close_inherited_fds() {
    let Ok(open) = std::fs::read_dir("/dev/fd") else {
        return;
    };
    let inherited: Vec<RawFd> = open
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect();
    // The listing's own fd is among them and closed with the
    // listing already; its close here answers `EBADF`, which is no error.
    for fd in inherited {
        // SAFETY: `close(2)` reads its one integer argument; the fd is
        // owned by no value of this process (see above).
        unsafe { libc::close(fd) };
    }
}

/// `php_cli_server_worker_install_pdeathsig`
/// (`Z/sapi/cli/php_cli_server.c:2486`) in a worker the server forked
/// from `parent`: `SIGTERM` asked for once the parent dies, where the
/// system offers it (`PR_SET_PDEATHSIG`), and the worker ended at once
/// where the parent already died between the fork and the ask. Nothing
/// where the system offers no such signal, as php's build without
/// `prctl` or `procctl` does nothing.
pub(crate) fn end_with_parent(parent: u32) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `prctl(PR_SET_PDEATHSIG)` reads its two integer
        // arguments and touches no memory of ours.
        let _ = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
        // SAFETY: `getppid(2)` takes nothing and touches no memory.
        let now = unsafe { libc::getppid() };
        if u32::try_from(now).ok() != Some(parent) {
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = parent;
}

/// The loop of `php_cli_server_dtor` over one worker
/// (`Z/sapi/cli/php_cli_server.c:2395-2405`): `waitpid` until the worker
/// `pid` exited or a signal ended it, given up where the wait fails.
pub(crate) fn wait_for_end(pid: i32) {
    let mut status = 0;
    loop {
        // SAFETY: `waitpid(2)` writes the one `c_int` it is handed, live
        // for the call.
        if unsafe { libc::waitpid(pid, &raw mut status, 0) } == -1 {
            return;
        }
        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            return;
        }
    }
}

/// `php_cli_server_get_system_time` (`Z/sapi/cli/php_cli_server.c:267`):
/// the time now in the process's local zone, `php_localtime_r` and
/// `php_asctime_r`, as `Wed Jun  1 21:49:08 2026` without the line end
/// `asctime` writes; `None` where the C library cannot place it.
pub(crate) fn asctime_now() -> Option<String> {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    // `time_t`, read off the call: the libc crate deprecates its alias on
    // musl, whose 1.2.0 made it 64 bits everywhere (rust-lang/libc#1848).
    let seconds = now.as_secs().try_into().ok()?;
    let mut tm = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_r` reads the one `time_t` it is handed and writes
    // the whole of the `tm` it is handed, both live for the call; it
    // answers null where it wrote nothing.
    let written = unsafe { libc::localtime_r(&raw const seconds, tm.as_mut_ptr()) };
    if written.is_null() {
        return None;
    }
    // SAFETY: `localtime_r` answered the pointer it was handed, which it
    // does only once every field of the `tm` is written.
    let tm = unsafe { tm.assume_init() };
    let day = DAYS.get(usize::try_from(tm.tm_wday).ok()?)?;
    let month = MONTHS.get(usize::try_from(tm.tm_mon).ok()?)?;
    Some(format!(
        "{day} {month}{:>3} {:02}:{:02}:{:02} {}",
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        i64::from(tm.tm_year) + 1900
    ))
}
