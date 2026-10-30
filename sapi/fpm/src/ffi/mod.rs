//! What php-fpm asks of the C library that neither Rust's standard library
//! nor `nix` answers safely, the host's one `unsafe` home (ADR-0008): the
//! signal handlers of the master and of a child, the anonymous shared
//! memory the `scoreboard` lives in, the fds a reload inherits by
//! number, the process's environment,
//! priority and title, the local time `strftime` writes, and the stop
//! and the Mach task right or the attachment a slow request's trace takes. Each
//! `unsafe` block is one operation with its `SAFETY` comment.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod signals;
mod title;
pub(crate) mod trace;

pub(crate) use signals::{Listening, child_soft_quit, master_signals, restore_defaults};

/// `in_shutdown`: the child was told to finish.
pub(crate) fn shutting_down() -> bool {
    signals::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
}
pub(crate) use title::{save_title_area, set_title};

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;

/// A value made of atomics alone, which every byte pattern of zeros is
/// valid for and which several processes may read and write at once.
///
/// # Safety
///
/// Every field, at every depth, is an atomic integer or an array of them.
pub(crate) unsafe trait Atomics: Sized {}

// SAFETY: a scoreboard's header is atomics alone (`scoreboard::Board`).
unsafe impl Atomics for crate::scoreboard::Board {}
// SAFETY: a scoreboard's process slot is atomics alone (`scoreboard::Proc`).
unsafe impl Atomics for crate::scoreboard::Proc {}

/// `fpm_shm_alloc`: `count` zeroed values in memory mapped shared and
/// anonymous, which a child forked after keeps shared with its master;
/// never unmapped, as the master keeps it until it exits.
pub(crate) fn shared<T: Atomics>(count: usize) -> Option<&'static [T]> {
    let len = std::mem::size_of::<T>().checked_mul(count.max(1))?;
    // SAFETY: an anonymous shared mapping of `len` bytes at an address the
    // kernel picks touches no memory the process holds.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return None;
    }
    let at = NonNull::new(at.cast::<T>())?;
    // SAFETY: the mapping is `count` values of `T` long, page-aligned so
    // aligned for `T`, zero-filled, which `Atomics` says is a valid `T`,
    // and never unmapped, so it lives as long as the process.
    Some(unsafe { std::slice::from_raw_parts(at.as_ptr(), count) })
}

/// A listening socket a reload inherited by the number `FPM_SOCKETS`
/// names (`fpm_sockets_init_main`).
pub(crate) fn inherited(fd: RawFd) -> Option<OwnedFd> {
    // SAFETY: `fcntl(F_GETFD)` reads the flags of the descriptor number it
    // is handed and touches no memory.
    let open = unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
    // SAFETY: the descriptor is open, and the master that exec'd this
    // process handed it over by this number in `FPM_SOCKETS`; nothing
    // else in the process owns it.
    open.then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `setenv(3)`.
pub(crate) fn set_env(name: &[u8], value: &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    let (name, value) = (
        std::ffi::OsStr::from_bytes(name),
        std::ffi::OsStr::from_bytes(value),
    );
    // SAFETY: the process is single-threaded here, the master reading its
    // configuration or a child setting its pool's environment before it
    // serves a request, so nothing reads the environment meanwhile.
    unsafe { std::env::set_var(name, value) };
}

/// `unsetenv(3)`.
pub(crate) fn unset_env(name: &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: the master is single-threaded as it execs.
    unsafe { std::env::remove_var(std::ffi::OsStr::from_bytes(name)) };
}

/// `clearenv(3)`, as `fpm_env_init_child` clears a pool's environment.
pub(crate) fn clear_env() {
    for (name, _) in std::env::vars_os() {
        // SAFETY: a child is single-threaded before its first request.
        unsafe { std::env::remove_var(name) };
    }
}

/// `initgroups(3)`, which `nix` leaves out on macOS.
pub(crate) fn initgroups(user: &std::ffi::CStr, group: u32) -> nix::Result<()> {
    #[allow(clippy::cast_possible_wrap)] // macOS takes the gid as an `int`
    // SAFETY: `initgroups` reads the NUL-ended name, live for the call,
    // and the group id.
    let done = unsafe { libc::initgroups(user.as_ptr(), group as _) };
    nix::errno::Errno::result(done).map(|_| ())
}

/// `setpriority(PRIO_PROCESS, 0, priority)`.
pub(crate) fn set_priority(priority: i32) -> bool {
    // SAFETY: `setpriority` reads its three integers alone.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, priority) == 0 }
}

/// C's `time_t`, the type the libc crate binds `localtime_r` with. The
/// crate deprecates the alias on musl, whose 1.2.0 made `time_t` 64 bits
/// on every target, a width the crate has yet to follow on 32-bit ones
/// (rust-lang/libc#1848). Every value reaches it through `try_from`,
/// which is right at either width, so the deprecation is expected on
/// musl and nowhere else; once the crate lifts it, the unmet expectation
/// fails the build.
#[cfg_attr(
    target_env = "musl",
    expect(deprecated, reason = "every value reaches it through try_from")
)]
type TimeT = libc::time_t;

/// The local time of `epoch` as `localtime_r` breaks it down.
fn local(epoch: i64) -> Option<libc::tm> {
    let seconds = TimeT::try_from(epoch).ok()?;
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
    Some(unsafe { tm.assume_init() })
}

/// `strftime(3)` of `format` at `epoch` in the process's local zone, as
/// php-fpm writes its times into a buffer of `size` bytes: nothing where
/// the format holds a NUL or what it writes does not fit.
pub(crate) fn strftime(format: &[u8], epoch: i64, size: usize) -> Vec<u8> {
    let (Some(tm), Ok(format)) = (local(epoch), CString::new(format)) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; size.max(1)];
    // SAFETY: `strftime` writes at most `buf.len()` bytes into `buf`,
    // reading the NUL-ended `format` and the `tm`, all live for the call.
    let written = unsafe {
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            format.as_ptr(),
            &raw const tm,
        )
    };
    buf.truncate(written);
    buf
}
