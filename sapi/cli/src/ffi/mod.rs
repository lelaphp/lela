//! What the CLI host asks of the C library that Rust's standard library
//! does not answer, the host's one `unsafe` home (ADR-0008): the
//! process's own fds 0, 1 and 2, which `do_cli` registers its standard
//! streams over, the area `ps` shows its title from ([`title`]), and the
//! local time the built-in server's log writes.
//! Each `unsafe` block is one operation with its `SAFETY` comment.

mod title;

pub(crate) use title::save_ps_args;

use std::mem::MaybeUninit;
use std::os::fd::{FromRawFd, OwnedFd};

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
    let seconds = libc::time_t::try_from(now.as_secs()).ok()?;
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
