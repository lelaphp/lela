//! What the CLI host asks of the C library that Rust's standard library
//! does not answer, the host's one `unsafe` home (ADR-0008): the local
//! time the built-in server's log writes. Each `unsafe` block is one
//! operation with its `SAFETY` comment.

use std::mem::MaybeUninit;

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
