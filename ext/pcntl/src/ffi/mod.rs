//! The binding to the process calls `nix` leaves unsafe or does not
//! wrap (design §7.2, revision 35; `unsafe.homes`' `ext/*/src/ffi/`), the
//! one home of this crate's `unsafe`:
//!
//! - [`install`], whose `nix` wrapper is an `unsafe fn`;
//! - the `extern "C"` handler [`install`] hands `sigaction`, which
//!   decodes the kernel's `siginfo_t` into the engine's record;
//! - [`wait`] and [`waitid`], because php hands the program the status
//!   word and the `siginfo_t` as the kernel wrote them, which `nix`
//!   decodes into its own `WaitStatus` and does not give back;
//! - [`priority`] and [`set_priority`], `getpriority(2)` and
//!   `setpriority(2)`, which `nix` does not wrap;
//! - [`qos_class`] and [`set_qos_class`], Apple's
//!   `pthread_get_qos_class_np` and `pthread_set_qos_class_self_np`,
//!   which neither `nix` nor `libc` declares.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use nix::errno::Errno;
use std::ffi::{c_int, c_void};
#[cfg(target_os = "linux")]
use std::mem::size_of;

use php_ext::request::{SIGNAL_POST, Siginfo};

/// What a signal's disposition becomes: php's `SIG_DFL`, `SIG_IGN`, or
/// [`handler`], which records it in the engine's signal record for the
/// dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Default,
    Ignore,
    Post,
}

/// `php_signal4(signo, func, restart, mask_all)` (`ext/pcntl/php_signal.c`):
/// `sigaction(2)` with `SA_ONSTACK | SA_SIGINFO`, `SA_RESTART` where
/// `restart`, and every signal blocked while the handler runs where
/// `mask_all`. The number goes to the C library as it is, so a real-time
/// signal, which `nix`'s `Signal` cannot name, is installed as php
/// installs it, and one the library keeps for itself is refused there.
pub(crate) fn install(
    number: i32,
    disposition: Disposition,
    restart: bool,
    mask_all: bool,
) -> Result<(), Errno> {
    let mask = if mask_all {
        SignalSet::full()
    } else {
        SignalSet::empty()
    };
    let mut flags = libc::SA_ONSTACK | libc::SA_SIGINFO;
    if restart {
        flags |= libc::SA_RESTART;
    }
    let posting: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) = handler;
    // SAFETY: `sigaction` is plain C data, for which all zeroes is a value.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = match disposition {
        Disposition::Default => libc::SIG_DFL,
        Disposition::Ignore => libc::SIG_IGN,
        Disposition::Post => posting as libc::sighandler_t,
    };
    action.sa_mask = mask.0;
    action.sa_flags = flags;
    // SAFETY: `action` is a live `sigaction` read and not kept, and the
    // only handler it may name is `handler`, which is async-signal-safe
    // as its own docs say; the old action is not asked for.
    let set = unsafe { libc::sigaction(number, &raw const action, std::ptr::null_mut()) };
    if set != 0 {
        return Err(Errno::last());
    }
    Ok(())
}

/// A `sigset_t` php fills number by number with `sigaddset(3)`, which
/// takes the real-time signals too.
pub(crate) struct SignalSet(libc::sigset_t);

impl SignalSet {
    /// `sigemptyset(3)`.
    pub(crate) fn empty() -> SignalSet {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: `sigemptyset` initialises the set it is handed.
        unsafe { libc::sigemptyset(set.as_mut_ptr()) };
        // SAFETY: initialised by the call above.
        SignalSet(unsafe { set.assume_init() })
    }

    /// `sigfillset(3)`.
    fn full() -> SignalSet {
        let mut set = SignalSet::empty();
        // SAFETY: `set` is a live, initialised set.
        unsafe { libc::sigfillset(&raw mut set.0) };
        set
    }

    /// `sigaddset(3)`, refused for a number the C library does not take.
    pub(crate) fn add(&mut self, number: i32) -> Result<(), Errno> {
        // SAFETY: `self` is a live, initialised set.
        if unsafe { libc::sigaddset(&raw mut self.0, number) } != 0 {
            return Err(Errno::last());
        }
        Ok(())
    }

    /// `sigismember(3)`.
    pub(crate) fn contains(&self, number: i32) -> bool {
        // SAFETY: `self` is a live, initialised set, read only.
        unsafe { libc::sigismember(&raw const self.0, number) == 1 }
    }
}

/// `sigprocmask(how, set, &old)`: the mask as it was.
pub(crate) fn mask(how: c_int, set: &SignalSet) -> Result<SignalSet, Errno> {
    let mut old = SignalSet::empty();
    // SAFETY: both sets are live and initialised; the call reads one and
    // writes the other.
    if unsafe { libc::sigprocmask(how, &raw const set.0, &raw mut old.0) } != 0 {
        return Err(Errno::last());
    }
    Ok(old)
}

/// `sigwaitinfo(2)`, or `sigtimedwait(2)` for `timeout`'s seconds and
/// nanoseconds: the signal taken and what its `siginfo_t` said. The
/// number is `si_signo` where the call answered `0`, as php reads it.
#[cfg(target_os = "linux")]
pub(crate) fn wait_info(
    set: &SignalSet,
    timeout: Option<(i64, i64)>,
) -> Result<(i32, Siginfo), Errno> {
    // SAFETY: `siginfo_t` is plain integers and pointers, for which all
    // zeroes is a value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let number = match timeout {
        // SAFETY: `set` is a live set read and `info` a live `siginfo_t`
        // the call writes.
        None => unsafe { libc::sigwaitinfo(&raw const set.0, &raw mut info) },
        Some((sec, nsec)) => {
            let at = libc::timespec {
                tv_sec: sec,
                tv_nsec: nsec,
            };
            // SAFETY: as `sigwaitinfo`, and `at` a live `timespec` read.
            unsafe { libc::sigtimedwait(&raw const set.0, &raw mut info, &raw const at) }
        }
    };
    if number == -1 {
        return Err(Errno::last());
    }
    let mut record = Siginfo::default();
    decode(&info, &mut record);
    let number = if number == 0 && info.si_signo != 0 {
        info.si_signo
    } else {
        number
    };
    Ok((number, record))
}

/// `pcntl_signal_handler` (`ext/pcntl/pcntl.c:1333`), installed with
/// `SA_SIGINFO`: the signal and what the kernel's `siginfo_t` said of it
/// posted to the engine's signal record, which raises the interrupt of
/// every request that asked for asynchronous delivery.
///
/// What runs in the signal handler is this function, [`decode`] and the
/// post and wake of `php_process::post` that [`SIGNAL_POST`] is, and nothing
/// of `php_engine`: the post claims a free slot by a compare-and-swap,
/// the decoder copies integers out of the kernel's structure, and the
/// wake stores to atomics, so nothing here allocates, locks or calls a
/// function that is not async-signal-safe (design §5.8, revision 38).
extern "C" fn handler(number: c_int, info: *mut libc::siginfo_t, _context: *mut c_void) {
    SIGNAL_POST.post(|record| {
        if !info.is_null() {
            // SAFETY: the kernel hands an `SA_SIGINFO` handler a
            // `siginfo_t` that is valid for the handler's duration, and
            // the pointer is not null.
            decode(unsafe { &*info }, record);
        }
        record.number = number;
    });
}

/// A child's resource usage, as `wait4(2)` reports it.
pub(crate) type Usage = libc::rusage;

/// An `rusage` of zeroes, php's `memset`. Made as php makes it, and not
/// field by field: musl's carries a private reserved tail that no struct
/// expression can fill.
pub(crate) fn zeroed_usage() -> Usage {
    // SAFETY: `rusage` is plain integers and `timeval`s, for which all
    // zeroes is a value.
    unsafe { std::mem::zeroed() }
}

/// `waitpid(2)`, or `wait4(2)` where `usage` asks for the child's
/// resource usage: the child's id and the status word as the kernel
/// wrote it. `status` goes in as the caller's word, as php's goes in.
pub(crate) fn wait(
    pid: i32,
    status: &mut i32,
    options: i32,
    usage: Option<&mut Usage>,
) -> Result<i32, Errno> {
    let usage = usage.map_or(std::ptr::null_mut(), std::ptr::from_mut);
    // SAFETY: `status` is a live `i32` and `usage` is null or a live
    // `rusage`, the two out-parameters `wait4` writes.
    let child = unsafe { libc::wait4(pid, status, options, usage) };
    if child < 0 {
        return Err(Errno::last());
    }
    Ok(child)
}

/// `waitid(2)`: what the kernel wrote into `siginfo_t`, in the shape
/// the engine's signal record hands a dispatch.
pub(crate) fn waitid(idtype: i64, id: i64, options: i32) -> Result<Siginfo, Errno> {
    // SAFETY: `siginfo_t` is plain integers and pointers, for which all
    // zeroes is a value; php `memset`s it the same way.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (idtype, id) = (idtype as libc::idtype_t, id as libc::id_t);
    // SAFETY: `info` is a live `siginfo_t` the call writes.
    let status = unsafe { libc::waitid(idtype, id, &raw mut info, options) };
    if status == -1 {
        return Err(Errno::last());
    }
    let mut record = Siginfo::default();
    decode(&info, &mut record);
    Ok(record)
}

/// The fields php reads out of a `siginfo_t` (`pcntl_siginfo_to_zval`),
/// written into `record`: plain copies of integers, async-signal-safe.
#[cfg(target_vendor = "apple")]
fn decode(info: &libc::siginfo_t, record: &mut Siginfo) {
    record.number = info.si_signo;
    record.errno = info.si_errno;
    record.code = info.si_code;
    record.pid = info.si_pid;
    record.uid = info.si_uid;
    record.status = info.si_status;
    record.addr = info.si_addr as usize;
}

/// The fields php reads out of a `siginfo_t`, through the accessors
/// `libc` gives Linux's union, written into `record`.
#[cfg(not(target_vendor = "apple"))]
fn decode(info: &libc::siginfo_t, record: &mut Siginfo) {
    record.number = info.si_signo;
    record.errno = info.si_errno;
    record.code = info.si_code;
    // SAFETY: the union is integers the kernel filled or zeroes.
    record.pid = unsafe { info.si_pid() };
    // SAFETY: as above.
    record.uid = unsafe { info.si_uid() };
    // SAFETY: as above.
    record.status = unsafe { info.si_status() };
    // SAFETY: as above.
    record.addr = unsafe { info.si_addr() } as usize;
    #[cfg(target_os = "linux")]
    linux_members(info, record);
}

/// glibc's `si_utime`, `si_stime`, `si_band` and `si_fd`, which
/// `pcntl_siginfo_to_zval` renders where `<signal.h>` defines them.
#[cfg(target_os = "linux")]
fn linux_members(info: &libc::siginfo_t, record: &mut Siginfo) {
    /// The union's `_sigpoll` arm, after the three `int`s every arm
    /// shares, aligned as the union is.
    #[repr(C)]
    struct SigPoll {
        _base: [c_int; 3],
        band: libc::c_long,
        fd: c_int,
    }
    // glibc's `si_band` and `si_fd` on `x86_64` and `aarch64`.
    const _: () = assert!(
        std::mem::offset_of!(SigPoll, band) == 16 && std::mem::offset_of!(SigPoll, fd) == 24
    );
    // SAFETY: the union is integers the kernel filled or zeroes.
    record.utime = unsafe { info.si_utime() };
    // SAFETY: as above.
    record.stime = unsafe { info.si_stime() };
    // SAFETY: `SigPoll` lays the union's `_sigpoll` arm out as glibc's
    // `siginfo_t` does, inside the structure's size and alignment.
    let poll = unsafe { &*std::ptr::from_ref(info).cast::<SigPoll>() };
    record.band = poll.band;
    record.fd = poll.fd;
}

/// `unshare(2)` with php's flags, the low 32 bits of the number given.
#[cfg(target_os = "linux")]
pub(crate) fn unshare(flags: i64) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation)]
    let flags = flags as c_int;
    // SAFETY: `unshare` takes an integer and touches no memory of the
    // caller's.
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(Errno::last());
    }
    Ok(())
}

/// `sysconf(_SC_NPROCESSORS_CONF)`: how many CPUs the system has
/// configured, the bound php holds a CPU id to.
#[cfg(target_os = "linux")]
pub(crate) fn cpus_configured() -> i64 {
    // SAFETY: `sysconf` takes an integer and touches no memory.
    unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) }
}

/// `CPU_SETSIZE`: the CPUs a `cpu_set_t` has a bit for.
#[cfg(target_os = "linux")]
const CPU_SETSIZE: usize = 8 * size_of::<libc::cpu_set_t>();

/// `sched_getaffinity(pid, sizeof(cpu_set_t), &mask)`: the CPUs below
/// `cpus` the process may be scheduled on, in order.
#[cfg(target_os = "linux")]
pub(crate) fn cpu_affinity(pid: i64, cpus: i64) -> Result<Vec<usize>, Errno> {
    // SAFETY: `cpu_set_t` is a plain bit array, for which all zeroes is
    // the empty set, as `CPU_ZERO` leaves it.
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    #[allow(clippy::cast_possible_truncation)]
    let pid = pid as libc::pid_t;
    let size = size_of::<libc::cpu_set_t>();
    // SAFETY: `mask` is a live `cpu_set_t` of `size` bytes the call writes.
    if unsafe { libc::sched_getaffinity(pid, size, &raw mut mask) } != 0 {
        return Err(Errno::last());
    }
    let cpus = usize::try_from(cpus).unwrap_or(0).min(CPU_SETSIZE);
    // SAFETY: `CPU_ISSET` reads the live set at a CPU below its size.
    Ok((0..cpus)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &mask) })
        .collect())
}

/// `sched_setaffinity(pid, sizeof(cpu_set_t), &mask)` over a mask with
/// each of `cpus` set.
#[cfg(target_os = "linux")]
pub(crate) fn set_cpu_affinity(pid: i64, cpus: &[usize]) -> Result<(), Errno> {
    // SAFETY: as in `cpu_affinity`, the empty set.
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // glibc's `CPU_SET` leaves a CPU past the set's size out.
    for &cpu in cpus.iter().filter(|&&cpu| cpu < CPU_SETSIZE) {
        // SAFETY: `CPU_SET` writes the live set at a CPU below its size.
        unsafe { libc::CPU_SET(cpu, &mut mask) };
    }
    #[allow(clippy::cast_possible_truncation)]
    let pid = pid as libc::pid_t;
    let size = size_of::<libc::cpu_set_t>();
    // SAFETY: `mask` is a live `cpu_set_t` of `size` bytes, read.
    if unsafe { libc::sched_setaffinity(pid, size, &raw const mask) } != 0 {
        return Err(Errno::last());
    }
    Ok(())
}

/// `sched_getcpu()`: the CPU the calling thread runs on, or `-1`.
#[cfg(target_os = "linux")]
pub(crate) fn current_cpu() -> i32 {
    // SAFETY: `sched_getcpu` takes nothing and touches no memory.
    unsafe { libc::sched_getcpu() }
}

/// `getpriority(2)`, whose `-1` is a priority as well as the failure, so
/// `errno` is cleared first and read after, as php reads it.
pub(crate) fn priority(which: i64, who: i64) -> Result<i32, Errno> {
    Errno::clear();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (which, who) = (which as _, who as libc::id_t);
    // SAFETY: `getpriority` takes two integers and touches no memory of
    // the caller's.
    let priority = unsafe { libc::getpriority(which, who) };
    match Errno::last_raw() {
        0 => Ok(priority),
        _ => Err(Errno::last()),
    }
}

/// `setpriority(2)`.
pub(crate) fn set_priority(which: i64, who: i64, priority: i64) -> Result<(), Errno> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (which, who, priority) = (which as _, who as libc::id_t, priority as libc::c_int);
    // SAFETY: `setpriority` takes three integers and touches no memory of
    // the caller's.
    let failed = unsafe { libc::setpriority(which, who, priority) };
    if failed != 0 {
        return Err(Errno::last());
    }
    Ok(())
}

/// Apple's `qos_class_t` values (`<sys/qos.h>`).
#[cfg(target_vendor = "apple")]
pub(crate) mod qos {
    pub(crate) const USER_INTERACTIVE: u32 = 0x21;
    pub(crate) const USER_INITIATED: u32 = 0x19;
    pub(crate) const DEFAULT: u32 = 0x15;
    pub(crate) const UTILITY: u32 = 0x11;
    pub(crate) const BACKGROUND: u32 = 0x09;
}

#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    fn pthread_get_qos_class_np(
        thread: libc::pthread_t,
        qos_class: *mut u32,
        relative_priority: *mut libc::c_int,
    ) -> libc::c_int;
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: libc::c_int)
    -> libc::c_int;
}

/// `pthread_get_qos_class_np(pthread_self(), &qos_class, NULL)`: the
/// calling thread's class, or the class read and the `errno` where the
/// call failed.
#[cfg(target_vendor = "apple")]
pub(crate) fn qos_class() -> Result<u32, (u32, Errno)> {
    let mut class = 0;
    // SAFETY: `pthread_self` takes nothing and cannot fail.
    let this = unsafe { libc::pthread_self() };
    // SAFETY: `class` is a live `u32` the call writes, and the relative
    // priority it may write too is asked for with a null pointer, which
    // the call reads as "not wanted".
    let failed = unsafe { pthread_get_qos_class_np(this, &raw mut class, std::ptr::null_mut()) };
    if failed != 0 {
        return Err((class, Errno::last()));
    }
    Ok(class)
}

/// `pthread_set_qos_class_self_np(class, 0)`.
#[cfg(target_vendor = "apple")]
pub(crate) fn set_qos_class(class: u32) -> Result<(), Errno> {
    // SAFETY: the call takes two integers and touches no memory of the
    // caller's.
    let failed = unsafe { pthread_set_qos_class_self_np(class, 0) };
    if failed != 0 {
        return Err(Errno::last());
    }
    Ok(())
}

/// What the kernel writes into a `siginfo_t` on Linux, read back as the
/// record php renders it from.
#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::{SignalSet, mask, wait_info, waitid};

    /// A child's exit through `waitid(2)`: `SIGCHLD`, `CLD_EXITED`, the
    /// child's id and its status, its clock ticks read where they are.
    #[test]
    fn waitid_reads_a_child_s_exit() -> std::io::Result<()> {
        let child = std::process::Command::new("true").spawn()?;
        let record = waitid(i64::from(libc::P_PID), i64::from(child.id()), libc::WEXITED)?;
        assert_eq!(record.number, libc::SIGCHLD);
        assert_eq!(record.code, libc::CLD_EXITED);
        assert_eq!(u32::try_from(record.pid).ok(), Some(child.id()));
        assert_eq!(record.status, 0);
        assert!(record.utime >= 0 && record.stime >= 0);
        Ok(())
    }

    /// A real-time signal sent to this thread and taken by
    /// `sigtimedwait(2)`: its number, a user's code and the sender, this
    /// process, as php's real-time arm renders them.
    #[test]
    fn sigtimedwait_takes_a_real_time_signal_with_its_sender() -> Result<(), nix::errno::Errno> {
        let number = libc::SIGRTMIN();
        let mut set = SignalSet::empty();
        set.add(number)?;
        let old = mask(libc::SIG_BLOCK, &set)?;
        // SAFETY: `pthread_self` takes nothing and cannot fail.
        let this = unsafe { libc::pthread_self() };
        // SAFETY: the signal goes to this thread, which blocks it, so it
        // stays pending until the wait below takes it.
        unsafe { libc::pthread_kill(this, number) };
        let taken = wait_info(&set, Some((1, 0)));
        mask(libc::SIG_SETMASK, &old)?;
        let (taken, record) = taken?;
        assert_eq!(taken, number);
        assert!([libc::SI_USER, libc::SI_TKILL].contains(&record.code));
        assert_eq!(u32::try_from(record.pid).ok(), Some(std::process::id()));
        Ok(())
    }
}
