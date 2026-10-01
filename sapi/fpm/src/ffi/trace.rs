//! `fpm_trace_*`, the half of the slow log that stops a child and opens
//! its memory: `fpm_trace_mach.c` on macOS, which stops the child with
//! `SIGSTOP` and asks the kernel for its task port, and
//! `fpm_trace_ptrace.c` elsewhere, which attaches to it. What the master
//! reads there once it is open, the frames of the request, is not read
//! (the crate's own doc).

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::zlog;

/// `fpm_trace_signal`: the child stopped for the master to read.
#[cfg(target_os = "macos")]
pub(crate) fn signal(pid: i32) -> bool {
    if let Err(e) = kill(Pid::from_raw(pid), Signal::SIGSTOP) {
        crate::zlog_sys!(Error @ fpm_trace_signal:47, e, "failed to send SIGSTOP to {pid}");
        return false;
    }
    true
}

/// `KERN_SUCCESS` and `KERN_FAILURE`, `<mach/kern_return.h>`.
#[cfg(target_os = "macos")]
const KERN_SUCCESS: libc::c_int = 0;
#[cfg(target_os = "macos")]
const KERN_FAILURE: libc::c_int = 5;

// The Mach calls `fpm_trace_mach.c` makes, as `<mach/mach.h>` declares them.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn task_for_pid(
        target_tport: libc::mach_port_t,
        pid: libc::c_int,
        task: *mut libc::mach_port_t,
    ) -> libc::c_int;
    fn mach_error_string(error_value: libc::c_int) -> *const libc::c_char;
}

/// `fpm_trace_ready`: the child's task port, where the kernel gives it.
#[cfg(target_os = "macos")]
pub(crate) fn ready(pid: i32) -> bool {
    let mut target: libc::mach_port_t = 0;
    // SAFETY: `mach_task_self_` is the process's own task port, which the
    // C library sets at startup and nothing writes after.
    let own = unsafe { mach_task_self_ };
    // SAFETY: `target` is a live local the call writes one port name to.
    let kr = unsafe { task_for_pid(own, pid, &raw mut target) };
    if kr == KERN_SUCCESS {
        return true;
    }
    // SAFETY: `mach_error_string` returns a static C string for any code.
    let text = unsafe { mach_error_string(kr) };
    let text = if text.is_null() {
        String::new()
    } else {
        // SAFETY: a non-null result is a NUL-terminated static string.
        unsafe { std::ffi::CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    };
    let hint = if kr == KERN_FAILURE {
        " It seems that master process does not have enough privileges to trace processes."
    } else {
        ""
    };
    zlog!(Error @ fpm_trace_ready:65, "task_for_pid() failed: {text} ({kr}){hint}");
    false
}

/// `fpm_trace_close`: nothing held past the read.
#[cfg(target_os = "macos")]
pub(crate) fn close(_pid: i32) -> bool {
    true
}

/// `fpm_trace_signal`: the master attached to the child, which stops it.
#[cfg(not(target_os = "macos"))]
pub(crate) fn signal(pid: i32) -> bool {
    if let Err(e) = nix::sys::ptrace::attach(Pid::from_raw(pid)) {
        crate::zlog_sys!(Error @ fpm_trace_signal:30, e, "failed to ptrace(ATTACH) child {pid}");
        return false;
    }
    true
}

/// `fpm_trace_ready`: the attached child is ready as it stands.
#[cfg(not(target_os = "macos"))]
pub(crate) fn ready(_pid: i32) -> bool {
    true
}

/// `fpm_trace_close`: the master detached.
#[cfg(not(target_os = "macos"))]
pub(crate) fn close(pid: i32) -> bool {
    if let Err(e) = nix::sys::ptrace::detach(Pid::from_raw(pid), None) {
        crate::zlog_sys!(Error @ fpm_trace_close:47, e, "failed to ptrace(DETACH) child {pid}");
        return false;
    }
    true
}

/// `fpm_pctl_kill(pid, FPM_PCTL_CONT)`: the child let go on.
pub(crate) fn resume(pid: i32) {
    let _ = kill(Pid::from_raw(pid), Signal::SIGCONT);
}
