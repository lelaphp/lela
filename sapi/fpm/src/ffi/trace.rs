//! `fpm_trace_*`, the half of the slow log that stops a child and opens
//! its memory: `fpm_trace_mach.c` on macOS, which stops the child with
//! `SIGSTOP` and asks the kernel for its Mach task right, and
//! `fpm_trace_ptrace.c` elsewhere, which attaches to it. Once it is open
//! the master reads bytes out of it ([`Memory::read`]), the request's
//! frames as the engine lays them (design §5.6, revision 59): through
//! `mach_vm_read_overwrite` on the task right, and `process_vm_readv` on
//! Linux, which the attachment permits.

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

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
    fn mach_vm_read_overwrite(
        target_task: libc::mach_port_t,
        address: u64,
        size: u64,
        data: u64,
        outsize: *mut u64,
    ) -> libc::c_int;
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::c_int;
}

/// A stopped child's memory, open for the master to read: its task right
/// on macOS, its pid where the master is attached to it. Dropping it is
/// `fpm_trace_close`, so it closes on every path out of the read.
pub(crate) struct Memory {
    #[cfg(target_os = "macos")]
    task: libc::mach_port_t,
    #[cfg(not(target_os = "macos"))]
    pid: i32,
}

impl Memory {
    /// `fpm_trace_get_long` and `fpm_trace_get_strz` underneath: the
    /// bytes at `address` in the child, every one of `into`, or nothing
    /// and `false`.
    #[cfg(target_os = "macos")]
    pub(crate) fn read(&self, address: u64, into: &mut [u8]) -> bool {
        let mut got = 0u64;
        let len = into.len() as u64;
        let local = into.as_mut_ptr().expose_provenance() as u64;
        // SAFETY: `into` is a live buffer of `len` bytes the kernel writes
        // at most `len` into, and `got` a live local.
        let kr = unsafe { mach_vm_read_overwrite(self.task, address, len, local, &raw mut got) };
        kr == KERN_SUCCESS && got == len
    }

    /// As on macOS, through `process_vm_readv`, which the attachment
    /// `fpm_trace_signal` made permits.
    #[cfg(target_os = "linux")]
    pub(crate) fn read(&self, address: u64, into: &mut [u8]) -> bool {
        let len = into.len();
        let local = libc::iovec {
            iov_base: into.as_mut_ptr().cast(),
            iov_len: len,
        };
        let Ok(remote) = usize::try_from(address) else {
            return false;
        };
        let remote = libc::iovec {
            iov_base: std::ptr::with_exposed_provenance_mut(remote),
            iov_len: len,
        };
        // SAFETY: `local` names `into`, live and `len` bytes long, which the
        // kernel writes at most `len` into; `remote` is the child's, which
        // the kernel reads on its side.
        let read = unsafe {
            libc::process_vm_readv(self.pid, &raw const local, 1, &raw const remote, 1, 0)
        };
        usize::try_from(read).is_ok_and(|read| read == len)
    }

    /// Elsewhere no read is made: the frames are not written.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn read(&self, _address: u64, _into: &mut [u8]) -> bool {
        false
    }
}

/// `fpm_trace_close`, once the read is done, which on macOS lets go of
/// what the read held and forgets the task right
/// (`fpm_mach_vm_deallocate`, `target = 0`): here the reads hold nothing
/// past their call, and the send right goes back, which php leaves
/// to the process's end.
#[cfg(target_os = "macos")]
impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: `mach_task_self_` is the process's own task port.
        let own = unsafe { mach_task_self_ };
        // SAFETY: the task port is the send right `task_for_pid` gave this
        // value, given back once.
        unsafe { mach_port_deallocate(own, self.task) };
    }
}

/// `fpm_trace_ready`: the child's task right, where the kernel gives it.
#[cfg(target_os = "macos")]
pub(crate) fn ready(pid: i32) -> Option<Memory> {
    let mut target: libc::mach_port_t = 0;
    // SAFETY: `mach_task_self_` is the process's own task port, which the
    // C library sets at startup and nothing writes after.
    let own = unsafe { mach_task_self_ };
    // SAFETY: `target` is a live local the call writes one port name to.
    let kr = unsafe { task_for_pid(own, pid, &raw mut target) };
    if kr == KERN_SUCCESS {
        return Some(Memory { task: target });
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
    crate::zlog!(Error @ fpm_trace_ready:65, "task_for_pid() failed: {text} ({kr}){hint}");
    None
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature on every platform: macOS's kernel can refuse the task port"
)]
pub(crate) fn ready(pid: i32) -> Option<Memory> {
    Some(Memory { pid })
}

/// `fpm_trace_close`, once the read is done: the master detached, which
/// `fpm_php_trace` goes on from whether or not it could.
#[cfg(not(target_os = "macos"))]
impl Drop for Memory {
    fn drop(&mut self) {
        let pid = self.pid;
        if let Err(e) = nix::sys::ptrace::detach(Pid::from_raw(pid), None) {
            crate::zlog_sys!(Error @ fpm_trace_close:47, e, "failed to ptrace(DETACH) child {pid}");
        }
    }
}

/// `fpm_pctl_kill(pid, FPM_PCTL_CONT)`: the child let go on.
pub(crate) fn resume(pid: i32) {
    kill(Pid::from_raw(pid), Signal::SIGCONT).ok();
}
