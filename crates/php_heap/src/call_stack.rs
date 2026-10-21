//! The calling thread's native stack, as the platform describes it
//! (design §5.6), after `Zend/zend_call_stack.c`: `zend_call_stack_get`
//! asks for the thread's base and size, and the compiler's stack limit
//! and `execute`'s re-entry limit are both built from that pair less
//! `zend.reserved_stack_size`. php asks one platform at a time, an arm
//! per `#ifdef`, and answers `false` on a platform it has no arm for;
//! what is here is every arm php has save Windows, which is month two
//! (design §12), each copied from its function: macOS, Linux, FreeBSD
//! with `DragonFly`, OpenBSD, NetBSD, Haiku and Solaris with illumos.
//! `None` stands where php's own `zend_call_stack_get` answers `false`,
//! and leaves php's fallback standing where the caller builds the
//! limit: a local's address for the base and [`call_stack_default_size`]
//! for the size, `zend_call_stack_default_size` arm for arm. This is
//! the crate that may say `unsafe`, and each call into the platform is
//! one operation per block with what makes it sound beside it.

/// `zend_call_stack`: the high address of the thread's stack and how far
/// down from it the stack may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallStack {
    /// `base`: the high address, where the stack starts. Stacks grow
    /// down, so every live frame of the thread is below it.
    pub base: usize,
    /// `max_size`: the bytes the stack may take below `base`.
    pub max_size: usize,
}

/// `zend_call_stack_get`: the calling thread's bounds, from the platform
/// arm that answers, or `None` on a platform none does.
#[must_use]
pub fn call_stack_bounds() -> Option<CallStack> {
    platform::call_stack_bounds()
}

/// The `max_size` [`call_stack_bounds`] answers on the process's main
/// thread, asked from whichever thread: the stack php's CLI and each
/// php-fpm worker serve a request on, which the shell's `ulimit -s`
/// sizes, and so the budget php's `EG(stack_limit)` is worked out of
/// there (design §5.6). A host that serves on a thread of its own size
/// reads php's budget here (#3142). `None` where the platform's
/// main-thread arm cannot be asked from another thread or there is none,
/// Haiku and a platform php has no arm for, and where the rlimit is
/// `RLIM_INFINITY`, as php's arm answers `false` there.
#[must_use]
pub fn main_call_stack_size() -> Option<usize> {
    platform::main_call_stack_size()
}

/// `zend_call_stack_default_size`: the size php assumes for a thread on
/// this platform when nothing could ask, the rlimit's default: 8 MiB on
/// Linux, 4 MiB on FreeBSD and NetBSD, 2 MiB on `DragonFly`, 512 KiB on
/// OpenBSD, 256 KiB on Haiku, 32 KiB on Solaris and 2 MiB elsewhere; on
/// macOS 8 MiB for the main thread and 512 KiB for any other, the
/// platform's default pthread size.
#[must_use]
pub fn call_stack_default_size() -> usize {
    const MIB: usize = 1024 * 1024;
    match std::env::consts::OS {
        #[cfg(target_os = "macos")]
        "macos" if platform::is_main_thread() => 8 * MIB,
        #[cfg(target_os = "macos")]
        "macos" => 512 * 1024,
        "linux" => 8 * MIB,
        "freebsd" | "netbsd" => 4 * MIB,
        "dragonfly" => 2 * MIB,
        "openbsd" => 512 * 1024,
        "haiku" => 64 * 4096,
        "solaris" | "illumos" => 8 * 4096,
        _ => 2 * MIB,
    }
}

/// `zend_call_stack_position`: the address of a local of the calling
/// frame, php's `__builtin_frame_address(0)`.
#[cfg(any(
    target_os = "linux",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
))]
#[inline]
fn position() -> usize {
    let here = 0u8;
    std::ptr::addr_of!(here).addr()
}

/// `getrlimit(RLIMIT_STACK)`'s soft limit, which every arm that sizes
/// the main thread reads it from, or `None` where the call fails or the
/// limit is `RLIM_INFINITY`, where php answers `false`.
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
))]
fn stack_rlimit() -> Option<usize> {
    let mut rlim = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `rlim` is room for one `rlimit`, which the call fills
    // on success and leaves alone on failure.
    if unsafe { libc::getrlimit(libc::RLIMIT_STACK, rlim.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `getrlimit` returned 0, so `rlim` holds an initialised
    // `rlimit`.
    let rlim = unsafe { rlim.assume_init() };
    if rlim.rlim_cur == libc::RLIM_INFINITY {
        return None;
    }
    usize::try_from(rlim.rlim_cur).ok()
}

/// `sysconf(_SC_PAGESIZE)`, the page the BSD and Haiku arms deduct a
/// guard of, and the macOS arm the Rust runtime's; php spells it
/// `getpagesize()` on FreeBSD, which answers the same number, and Rust's
/// runtime reads it as this does.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "haiku"
))]
fn page_size() -> Option<usize> {
    // SAFETY: `sysconf` takes one name, reads and writes nothing of
    // ours and answers a plain number.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).ok()
}

/// `zend_call_stack_get_macos`: the base is `pthread_get_stackaddr_np`
/// and the size `pthread_get_stacksize_np`, both of the calling thread,
/// save the main thread's size, which ends above the page Rust's runtime
/// takes for its guard.
///
/// php's size is right for php's main thread and not for this one: a
/// Rust program's runtime, before `main`, remaps the lowest page of the
/// main thread's stack `PROT_NONE` (`std::sys::pal::unix::stack_overflow`,
/// `install_main_guard_default`, at the page-aligned address
/// `pthread_get_stacksize_np` below the base), so the last page php's
/// arithmetic counts faults here, and `zend.reserved_stack_size`'s 32
/// KiB above the end php reckons is one page less above the stack's real
/// end: 16 KiB on Apple silicon. php's Intel arm, which states 8 MiB for
/// a main thread because macOS 10.9 and 10.10 under-reported it, has no
/// place here either: the runtime places its guard by the platform's
/// size, and no macOS Rust supports under-reports it.
#[cfg(target_os = "macos")]
mod platform {
    use super::{CallStack, page_size};

    /// `pthread_main_np`, non-zero on the process's first thread.
    pub(super) fn is_main_thread() -> bool {
        // SAFETY: `pthread_main_np` takes no argument and reads nothing
        // of ours; it answers a plain number.
        unsafe { libc::pthread_main_np() != 0 }
    }

    unsafe extern "C" {
        /// `pthread_main_thread_np`, the process's first thread's
        /// `pthread_t`, which `libc` does not carry: exported by
        /// `libsystem_pthread` and declared in no public SDK header, so
        /// what follows is the open-source libpthread's, not a documented
        /// contract. That library keeps the first thread's record in
        /// static storage and answers `pthread_get_stackaddr_np` and
        /// `pthread_get_stacksize_np` for it from the record without
        /// looking the thread up (the `t == main_thread()` arms), so it
        /// reads the same after the first thread has ended, as it has in
        /// a host that moved its `main` to another thread
        /// (`php_process::stand_aside`); the stack itself stays mapped.
        fn pthread_main_thread_np() -> libc::pthread_t;
    }

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        // SAFETY: `pthread_self` takes no argument and answers the
        // calling thread's own handle, which stays valid while the
        // thread runs, and this thread is running it.
        let this = unsafe { libc::pthread_self() };
        let thread = if is_main_thread() {
            Thread::Main(this)
        } else {
            Thread::Other(this)
        };
        bounds(thread)
    }

    pub(super) fn main_call_stack_size() -> Option<usize> {
        // SAFETY: `pthread_main_thread_np` takes no argument, reads
        // nothing of ours and answers the first thread's `pthread_t`.
        let main = unsafe { pthread_main_thread_np() };
        bounds(Thread::Main(main)).map(|stack| stack.max_size)
    }

    /// The thread whose stack is read: the process's first, whose size
    /// ends above the runtime's guard page, or the calling thread when it
    /// is any other.
    #[derive(Clone, Copy)]
    enum Thread {
        Main(libc::pthread_t),
        Other(libc::pthread_t),
    }

    /// `zend_call_stack_get_macos` over `thread`.
    fn bounds(thread: Thread) -> Option<CallStack> {
        let (Thread::Main(id) | Thread::Other(id)) = thread;
        // SAFETY: `id` is the calling thread's own, which is running, or
        // the process's first, whose record the library keeps static; the
        // call reads that record and writes nothing.
        let base = unsafe { libc::pthread_get_stackaddr_np(id) }.addr();
        // SAFETY: as above, `id`'s record stands and the call reads it
        // and writes nothing.
        let size = unsafe { libc::pthread_get_stacksize_np(id) };
        let max_size = if matches!(thread, Thread::Main(_)) {
            let page = page_size()?;
            let guard = base.checked_sub(size)?.checked_next_multiple_of(page)?;
            base.checked_sub(guard.checked_add(page)?)?
        } else {
            size
        };
        Some(CallStack { base, max_size })
    }
}

/// `zend_call_stack_get_linux`: `/proc/self/maps` and `RLIMIT_STACK` on
/// the main thread, whose mapping grows on demand and whose size is the
/// rlimit's, and `pthread_getattr_np` on any other, whose mapping is
/// laid down whole when the thread starts.
#[cfg(target_os = "linux")]
mod platform {
    use std::ffi::c_void;
    use std::mem::MaybeUninit;

    use super::{CallStack, position, stack_rlimit};

    /// `zend_call_stack_is_main_thread`: the thread whose id is the
    /// process's.
    fn is_main_thread() -> bool {
        // SAFETY: `getpid` takes no argument, reads and writes nothing
        // of ours and answers a plain number.
        let pid = unsafe { libc::getpid() };
        // SAFETY: `SYS_gettid` takes no argument, reads and writes
        // nothing of ours and answers a plain number.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        libc::c_long::from(pid) == tid
    }

    /// `zend_call_stack_get_linux_pthread`: the attributes of the
    /// calling thread, which `pthread_getattr_np` answers wrongly for
    /// the main thread under musl and old glibc, so php never asks it
    /// there. The guard-page adjustment for glibc before 2.8 is not
    /// carried; Rust links no glibc that old.
    fn pthread_bounds() -> Option<CallStack> {
        let mut attr = MaybeUninit::<libc::pthread_attr_t>::uninit();
        // SAFETY: `pthread_self` takes no argument and answers the
        // calling thread's own handle, live while the thread runs.
        let this = unsafe { libc::pthread_self() };
        // SAFETY: `attr` is room for one `pthread_attr_t`, which the
        // call fills on success and leaves alone on failure; `this`
        // names a live thread.
        if unsafe { libc::pthread_getattr_np(this, attr.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: `pthread_getattr_np` returned 0, so `attr` holds an
        // initialised `pthread_attr_t`.
        let mut attr = unsafe { attr.assume_init() };
        let mut addr: *mut c_void = std::ptr::null_mut();
        let mut max_size: usize = 0;
        // SAFETY: `attr` is initialised, and `addr` and `max_size` are
        // locals of the right types the call writes through.
        let read = unsafe {
            libc::pthread_attr_getstack(&raw const attr, &raw mut addr, &raw mut max_size)
        };
        // SAFETY: `attr` is initialised and is not read after this.
        unsafe { libc::pthread_attr_destroy(&raw mut attr) };
        if read != 0 {
            return None;
        }
        Some(CallStack {
            base: addr.addr().wrapping_add(max_size),
            max_size,
        })
    }

    /// `zend_call_stack_get_linux_proc_maps`: the mapping that holds a
    /// local's address is the stack; its end is the base, and its size
    /// is `RLIMIT_STACK`, less what the mapping below would stop it
    /// growing past. Valgrind's last page is not deducted; php deducts
    /// it only in a build made for Valgrind.
    fn proc_maps_bounds() -> Option<CallStack> {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        let on_stack = position();
        let mut prev_end = 0usize;
        let mut found = None;
        for line in maps.lines() {
            let range = line.split(' ').next().unwrap_or("");
            let (start, end) = range.split_once('-')?;
            let start = usize::from_str_radix(start, 16).ok()?;
            let end = usize::from_str_radix(end, 16).ok()?;
            if start <= on_stack && end >= on_stack {
                found = Some(end);
                break;
            }
            prev_end = end;
        }
        let end = found?;
        let mut max_size = stack_rlimit()?;
        if end.wrapping_sub(max_size) < prev_end {
            max_size = prev_end.wrapping_sub(end);
        }
        Some(CallStack {
            base: end,
            max_size,
        })
    }

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        if is_main_thread() {
            return proc_maps_bounds();
        }
        pthread_bounds()
    }

    /// The main thread's arm sizes it by the rlimit alone, which any
    /// thread reads; the mapping below that would cap it is the main
    /// thread's own to find, and is not asked.
    pub(super) fn main_call_stack_size() -> Option<usize> {
        stack_rlimit()
    }
}

/// `zend_call_stack_get_freebsd`, which php runs on `DragonFly` too:
/// `KERN_USRSTACK` under `RLIMIT_STACK` less the guard pages
/// `security.bsd.stack_guard_page` counts on the main thread, and
/// `pthread_attr_get_np` on any other, whose attributes hold the stack
/// the thread was started on.
#[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
mod platform {
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::ptr::{null, null_mut};

    use super::{CallStack, page_size, stack_rlimit};

    /// `zend_call_stack_is_main_thread`: `pthread_main_np` answers 1 on
    /// the process's first thread and -1 before the threading library
    /// is up, which is the first thread too.
    fn is_main_thread() -> bool {
        // SAFETY: `pthread_main_np` takes no argument and reads nothing
        // of ours; it answers a plain number.
        let is_main = unsafe { libc::pthread_main_np() };
        is_main == -1 || is_main == 1
    }

    /// `zend_call_stack_get_freebsd_pthread`: the attributes of the
    /// calling thread, which answer bogus values for the main thread,
    /// so php never asks them there.
    fn pthread_bounds() -> Option<CallStack> {
        let mut attr = MaybeUninit::<libc::pthread_attr_t>::uninit();
        // SAFETY: `attr` is room for one `pthread_attr_t`, which the
        // call fills on success and leaves alone on failure.
        if unsafe { libc::pthread_attr_init(attr.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: `pthread_attr_init` returned 0, so `attr` holds an
        // initialised `pthread_attr_t`.
        let mut attr = unsafe { attr.assume_init() };
        // SAFETY: `pthread_self` takes no argument and answers the
        // calling thread's own handle, live while the thread runs.
        let this = unsafe { libc::pthread_self() };
        // SAFETY: `this` names a live thread and `attr` is initialised;
        // the call overwrites `attr` with the thread's attributes.
        let got = unsafe { libc::pthread_attr_get_np(this, &raw mut attr) };
        let mut addr: *mut c_void = null_mut();
        let mut max_size: usize = 0;
        let read = if got == 0 {
            // SAFETY: `attr` is initialised, and `addr` and `max_size`
            // are locals of the right types the call writes through.
            unsafe {
                libc::pthread_attr_getstack(&raw const attr, &raw mut addr, &raw mut max_size)
            }
        } else {
            got
        };
        // SAFETY: `attr` is initialised and is not read after this.
        unsafe { libc::pthread_attr_destroy(&raw mut attr) };
        if read != 0 {
            return None;
        }
        Some(CallStack {
            base: addr.addr().wrapping_add(max_size),
            max_size,
        })
    }

    /// `zend_call_stack_get_freebsd_sysctl`: the main thread's base is
    /// `kern.usrstack`, and its size the rlimit less the guard pages,
    /// one by default, that `security.bsd.stack_guard_page` counts.
    fn sysctl_bounds() -> Option<CallStack> {
        let mib = [libc::CTL_KERN, libc::KERN_USRSTACK];
        let mut stack_base: *mut c_void = null_mut();
        let mut len = size_of::<*mut c_void>();
        // SAFETY: `mib` is two names and `namelen` says two; `stack_base`
        // is `len` bytes the call writes through and shortens `len` to;
        // nothing is set, a null new value of length zero.
        let asked = unsafe {
            libc::sysctl(
                mib.as_ptr(),
                2,
                (&raw mut stack_base).cast(),
                &raw mut len,
                null(),
                0,
            )
        };
        if asked != 0 {
            return None;
        }
        let rlimit = stack_rlimit()?;
        let mut numguards: usize = 0;
        let mut len = size_of::<usize>();
        // SAFETY: the name is a NUL-terminated literal; `numguards` is
        // `len` bytes the call writes through and shortens `len` to;
        // nothing is set, a null new value of length zero.
        let asked = unsafe {
            libc::sysctlbyname(
                c"security.bsd.stack_guard_page".as_ptr(),
                (&raw mut numguards).cast(),
                &raw mut len,
                null(),
                0,
            )
        };
        if asked != 0 {
            return None;
        }
        let guard_size = numguards.wrapping_mul(page_size()?);
        Some(CallStack {
            base: stack_base.addr(),
            max_size: rlimit.wrapping_sub(guard_size),
        })
    }

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        if is_main_thread() {
            return sysctl_bounds();
        }
        pthread_bounds()
    }

    /// The main thread's arm asks the kernel, not the calling thread.
    pub(super) fn main_call_stack_size() -> Option<usize> {
        sysctl_bounds().map(|stack| stack.max_size)
    }
}

/// `zend_call_stack_get_openbsd`: `pthread_stackseg_np` on a thread the
/// library started, and `VM_PSSTRINGS` under `RLIMIT_STACK` on the
/// initial one, whose segment the library only estimates.
#[cfg(target_os = "openbsd")]
mod platform {
    use std::ffi::{c_int, c_void};
    use std::mem::MaybeUninit;
    use std::ptr::null_mut;

    use super::{CallStack, page_size, stack_rlimit};

    /// `VM_PSSTRINGS`, `sys/uvm/uvmexp.h`, which `libc` does not carry:
    /// the address of the process's argument strings, laid down at the
    /// top of the initial stack.
    const VM_PSSTRINGS: c_int = 3;

    /// `struct _ps_strings`, `sys/uvm/uvmexp.h`, what `VM_PSSTRINGS`
    /// answers.
    #[repr(C)]
    struct PsStrings {
        val: *mut c_void,
    }

    /// php reads the thread information block itself, `TIB_GET()`, and
    /// takes a thread with none or with `TIB_THREAD_INITIAL_STACK` set
    /// as the initial one; `pthread_main_np` answers 1 on exactly those
    /// two, before the library is up and on the flagged thread.
    fn is_main_thread() -> bool {
        // SAFETY: `pthread_main_np` takes no argument and reads nothing
        // of ours; it answers a plain number.
        unsafe { libc::pthread_main_np() != 0 }
    }

    /// `zend_call_stack_get_openbsd_pthread`: the thread's segment, its
    /// size less one page. `pthread_stackseg_np` answers the top of the
    /// stack in `ss_sp`, `pthread_stackseg_np(3)`, and the base here is
    /// that address; php's arm at 8.5.9 takes `ss_sp - ss_size`, the
    /// bottom, so its limit lies a whole stack below the frames and
    /// never fires, a defect and not a behaviour to copy: the accepted
    /// row `trace:heap-openbsd-stack-base` of `docs/divergence.toml`.
    fn pthread_bounds() -> Option<CallStack> {
        // SAFETY: `pthread_self` takes no argument and answers the
        // calling thread's own handle, live while the thread runs.
        let this = unsafe { libc::pthread_self() };
        let mut segment = MaybeUninit::<libc::stack_t>::uninit();
        // SAFETY: `this` names a live thread; `segment` is room for one
        // `stack_t`, which the call fills on success and leaves alone
        // on failure.
        if unsafe { libc::pthread_stackseg_np(this, segment.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: `pthread_stackseg_np` returned 0, so `segment` holds
        // an initialised `stack_t`.
        let segment = unsafe { segment.assume_init() };
        Some(CallStack {
            base: segment.ss_sp.addr(),
            max_size: segment.ss_size.wrapping_sub(page_size()?),
        })
    }

    /// `zend_call_stack_get_openbsd_vm`: the argument strings rounded up
    /// to a page are the initial stack's base, and its size the rlimit
    /// less one page.
    fn vm_bounds() -> Option<CallStack> {
        let mib = [libc::CTL_VM, VM_PSSTRINGS];
        let mut ps = PsStrings { val: null_mut() };
        let mut len = size_of::<PsStrings>();
        // SAFETY: `mib` is two names and `namelen` says two; `ps` is
        // `len` bytes the call writes through and shortens `len` to;
        // nothing is set, a null new value of length zero.
        let asked = unsafe {
            libc::sysctl(
                mib.as_ptr(),
                2,
                (&raw mut ps).cast(),
                &raw mut len,
                null_mut(),
                0,
            )
        };
        if asked != 0 {
            return None;
        }
        let rlimit = stack_rlimit()?;
        let page = page_size()?;
        let mask = page.wrapping_sub(1);
        Some(CallStack {
            base: ps.val.addr().wrapping_add(mask) & !mask,
            max_size: rlimit.wrapping_sub(page),
        })
    }

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        if is_main_thread() {
            return vm_bounds();
        }
        pthread_bounds()
    }

    /// The main thread's arm reads the process's argument strings, not
    /// the calling thread.
    pub(super) fn main_call_stack_size() -> Option<usize> {
        vm_bounds().map(|stack| stack.max_size)
    }
}

/// `zend_call_stack_get_netbsd`: the process's map under `RLIMIT_STACK`
/// on the first lwp, read by `sysctl` rather than procfs, which NetBSD
/// means to retire, and `pthread_getattr_np` less its guard on any
/// other.
#[cfg(target_os = "netbsd")]
mod platform {
    use std::ffi::{c_int, c_void};
    use std::mem::MaybeUninit;
    use std::ptr::{null, null_mut};

    use super::{CallStack, position, stack_rlimit};

    /// The lwp numbered 1 is the process's first.
    fn is_main_thread() -> bool {
        // SAFETY: `_lwp_self` takes no argument and reads nothing of
        // ours; it answers a plain number.
        unsafe { libc::_lwp_self() == 1 }
    }

    /// `zend_call_stack_get_netbsd_pthread`: the thread's stack from
    /// its attributes, less the guard at its low end. php leaves the
    /// attributes undestroyed; here they are destroyed, which no
    /// caller can see.
    fn pthread_bounds() -> Option<CallStack> {
        let mut attr = MaybeUninit::<libc::pthread_attr_t>::uninit();
        // SAFETY: `pthread_self` takes no argument and answers the
        // calling thread's own handle, live while the thread runs.
        let this = unsafe { libc::pthread_self() };
        // SAFETY: `attr` is room for one `pthread_attr_t`, which the
        // call fills on success and leaves alone on failure; `this`
        // names a live thread.
        if unsafe { libc::pthread_getattr_np(this, attr.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: `pthread_getattr_np` returned 0, so `attr` holds an
        // initialised `pthread_attr_t`.
        let mut attr = unsafe { attr.assume_init() };
        let mut addr: *mut c_void = null_mut();
        let mut max_size: usize = 0;
        let mut guard_size: usize = 0;
        // SAFETY: `attr` is initialised, and `addr` and `max_size` are
        // locals of the right types the call writes through.
        let mut read = unsafe {
            libc::pthread_attr_getstack(&raw const attr, &raw mut addr, &raw mut max_size)
        };
        if read == 0 {
            // SAFETY: `attr` is initialised and `guard_size` is a local
            // of the right type the call writes through.
            read = unsafe { libc::pthread_attr_getguardsize(&raw const attr, &raw mut guard_size) };
        }
        // SAFETY: `attr` is initialised and is not read after this.
        unsafe { libc::pthread_attr_destroy(&raw mut attr) };
        if read != 0 {
            return None;
        }
        let low = addr.addr().wrapping_add(guard_size);
        let max_size = max_size.wrapping_sub(guard_size);
        Some(CallStack {
            base: low.wrapping_add(max_size),
            max_size,
        })
    }

    /// `zend_call_stack_get_netbsd_vm`: `VM_PROC_MAP` sized once, taken
    /// with a third more room as `kinfo_getvmmap` takes it, and the
    /// entry that holds a local's address is the stack, its end the
    /// base and the rlimit its size.
    fn vm_bounds() -> Option<CallStack> {
        // SAFETY: `getpid` takes no argument, reads and writes nothing
        // of ours and answers a plain number.
        let pid = unsafe { libc::getpid() };
        let entry_size = size_of::<libc::kinfo_vmentry>();
        let mib = [
            libc::CTL_VM,
            libc::VM_PROC,
            libc::VM_PROC_MAP,
            pid,
            c_int::try_from(entry_size).ok()?,
        ];
        let mut len: usize = 0;
        // SAFETY: `mib` is five names and `namelen` says five; a null
        // old value asks for the size alone, written to `len`; nothing
        // is set, a null new value of length zero.
        let asked = unsafe { libc::sysctl(mib.as_ptr(), 5, null_mut(), &raw mut len, null(), 0) };
        if asked != 0 {
            return None;
        }
        let room = len.wrapping_mul(4) / 3 / entry_size + 1;
        let mut entries = vec![MaybeUninit::<libc::kinfo_vmentry>::uninit(); room];
        let mut len = room.wrapping_mul(entry_size);
        // SAFETY: `mib` is five names and `namelen` says five; `entries`
        // is `len` bytes the call writes through and shortens `len` to
        // what it wrote; nothing is set, a null new value of length zero.
        let asked = unsafe {
            libc::sysctl(
                mib.as_ptr(),
                5,
                entries.as_mut_ptr().cast(),
                &raw mut len,
                null(),
                0,
            )
        };
        if asked != 0 {
            return None;
        }
        let filled = len / entry_size;
        let on_stack = u64::try_from(position()).ok()?;
        let end = entries.get(..filled)?.iter().find_map(|entry| {
            // SAFETY: the call wrote `len` bytes, so the first `filled`
            // entries are whole `kinfo_vmentry` values.
            let entry = unsafe { entry.assume_init_ref() };
            (entry.kve_start <= on_stack && entry.kve_end >= on_stack).then_some(entry.kve_end)
        })?;
        Some(CallStack {
            base: usize::try_from(end).ok()?,
            max_size: stack_rlimit()?,
        })
    }

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        if is_main_thread() {
            return vm_bounds();
        }
        pthread_bounds()
    }

    /// The main thread's arm sizes it by the rlimit, which any thread
    /// reads.
    pub(super) fn main_call_stack_size() -> Option<usize> {
        stack_rlimit()
    }
}

/// `zend_call_stack_get_haiku`: the kernel's information on the calling
/// thread, `get_thread_info`, carries its stack's two ends, and the
/// four guard pages of `USER_STACK_GUARD_SIZE` come off the size.
#[cfg(target_os = "haiku")]
mod platform {
    use std::mem::MaybeUninit;
    use std::ptr::null;

    use super::{CallStack, page_size};

    /// Haiku's arm reads the calling thread's own information.
    pub(super) fn main_call_stack_size() -> Option<usize> {
        None
    }

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        // SAFETY: a null name asks for the calling thread's own id; the
        // call reads and writes nothing of ours.
        let id = unsafe { libc::find_thread(null()) };
        if id == libc::B_NAME_NOT_FOUND {
            return None;
        }
        let mut info = MaybeUninit::<libc::thread_info>::uninit();
        // SAFETY: `id` is the calling thread's, so it names a live
        // thread; `info` is room for one `thread_info`, which the call
        // fills on `B_OK` and leaves alone otherwise.
        if unsafe { libc::get_thread_info(id, info.as_mut_ptr()) } != libc::B_OK {
            return None;
        }
        // SAFETY: `get_thread_info` returned `B_OK`, so `info` holds an
        // initialised `thread_info`.
        let info = unsafe { info.assume_init() };
        let guard_size = page_size()?.wrapping_mul(4);
        let base = info.stack_end.addr();
        Some(CallStack {
            base,
            max_size: base
                .wrapping_sub(info.stack_base.addr())
                .wrapping_sub(guard_size),
        })
    }
}

/// `zend_call_stack_get_solaris`, which php runs on illumos too: the
/// process's map under `RLIMIT_STACK` on the first lwp, and
/// `thr_stksegment` on any other.
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
mod platform {
    use std::ffi::{c_int, c_uint};
    use std::mem::MaybeUninit;

    use super::{CallStack, position, stack_rlimit};

    unsafe extern "C" {
        /// `thread.h`: the calling thread's stack segment, `ss_sp` its
        /// top and `ss_size` its size; -1 on failure.
        fn thr_stksegment(segment: *mut libc::stack_t) -> c_int;
        /// `sys/lwp.h`: the calling lwp's id, `lwpid_t`, an unsigned int.
        fn _lwp_self() -> c_uint;
    }

    /// One `prmap_t` of `/proc/<pid>/map`, `sys/procfs.h`: `pr_vaddr`
    /// and `pr_size` as two words, `pr_mapname` of `PRMAPSZ` 64 bytes,
    /// `pr_offset` of eight, then `pr_mflags`, `pr_pagesize`, `pr_shmid`
    /// and one filler int. `libc` carries none of it, and only the two
    /// leading words are read.
    const PRMAP_SIZE: usize =
        2 * size_of::<usize>() + 64 + size_of::<i64>() + 4 * size_of::<c_int>();

    /// The lwp numbered 1 is the process's first.
    fn is_main_thread() -> bool {
        // SAFETY: `_lwp_self` takes no argument and reads nothing of
        // ours; it answers a plain number.
        unsafe { _lwp_self() == 1 }
    }

    /// `zend_call_stack_get_solaris_pthread`: the segment as the
    /// threading library describes it.
    fn pthread_bounds() -> Option<CallStack> {
        let mut segment = MaybeUninit::<libc::stack_t>::uninit();
        // SAFETY: `segment` is room for one `stack_t`, which the call
        // fills on success and leaves alone on failure.
        if unsafe { thr_stksegment(segment.as_mut_ptr()) } < 0 {
            return None;
        }
        // SAFETY: `thr_stksegment` returned 0, so `segment` holds an
        // initialised `stack_t`.
        let segment = unsafe { segment.assume_init() };
        Some(CallStack {
            base: segment.ss_sp.addr(),
            max_size: segment.ss_size,
        })
    }

    /// `zend_call_stack_get_solaris_proc_maps`: the mapping of
    /// `/proc/<pid>/map` that holds a local's address is the stack, its
    /// end the base and the rlimit its size. php reads the file the
    /// same way after a `Pgrab(PGRAB_RDONLY)` of its own process that it
    /// never reads through and releases at the end; the grab is not
    /// carried, which no caller can see.
    fn proc_map_bounds() -> Option<CallStack> {
        let map = std::fs::read(format!("/proc/{}/map", std::process::id())).ok()?;
        let on_stack = position();
        let word = size_of::<usize>();
        let end = map.chunks_exact(PRMAP_SIZE).find_map(|entry| {
            let vaddr = usize::from_ne_bytes(entry.get(..word)?.try_into().ok()?);
            let size = usize::from_ne_bytes(entry.get(word..2 * word)?.try_into().ok()?);
            let end = vaddr.wrapping_add(size);
            (vaddr <= on_stack && end >= on_stack).then_some(end)
        })?;
        Some(CallStack {
            base: end,
            max_size: stack_rlimit()?,
        })
    }

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        if is_main_thread() {
            return proc_map_bounds();
        }
        pthread_bounds()
    }

    /// The main thread's arm sizes it by the rlimit, which any thread
    /// reads.
    pub(super) fn main_call_stack_size() -> Option<usize> {
        stack_rlimit()
    }
}

/// A platform php has no arm for, or Windows, whose arm is month two:
/// nothing answers, and the fallback stands.
#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "haiku",
    target_os = "solaris",
    target_os = "illumos"
)))]
mod platform {
    use super::CallStack;

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        None
    }

    pub(super) fn main_call_stack_size() -> Option<usize> {
        None
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use super::*;

    /// Whether this platform has an arm above.
    const ARMED: bool = cfg!(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "haiku",
        target_os = "solaris",
        target_os = "illumos"
    ));

    /// A local of the asking frame lies inside the bounds the platform
    /// answers, on the thread the harness runs the test on and on one
    /// spawned at a size of this test's choosing, whose size is what
    /// was asked for, to the guard an arm deducts: one page on OpenBSD
    /// and NetBSD, four on Haiku.
    ///
    /// The size is above the 2 MiB the test harness spawns each test
    /// on: glibc keeps a finished thread's stack in a cache and hands
    /// it to the next thread that asks for one no larger, and
    /// `pthread_getattr_np` then answers the cached stack's size
    /// (`nptl/allocatestack.c`, `get_cached_stack`), which is how the
    /// twin of this test in `php_compile` read a 1 MiB thread as 2 MiB
    /// on the runner. A size no cached stack reaches is served fresh.
    #[test]
    #[cfg_attr(
        miri,
        ignore = "Miri keeps a frame's locals in allocations of its own, not on the machine stack whose bounds the probe answers"
    )]
    fn a_local_lies_within_the_bounds_the_platform_answers() {
        if ARMED {
            assert!(holds_a_local(call_stack_bounds()));
            let size = 4 * 1024 * 1024;
            let (bounds, held) = std::thread::Builder::new()
                .stack_size(size)
                .spawn(|| (call_stack_bounds(), holds_a_local(call_stack_bounds())))
                .expect("a thread of the size asked")
                .join()
                .expect("the thread runs to its end");
            assert!(held);
            let max_size = bounds.map_or(0, |stack| stack.max_size);
            let guard = 4 * 16 * 1024;
            assert!(
                max_size + guard >= size && max_size < 2 * size,
                "a thread spawned at {size} bytes reads as {max_size}"
            );
        } else {
            assert_eq!(call_stack_bounds(), None);
        }
    }

    /// The assumed size is a positive number of whole KiB on every
    /// platform, and the platform's default for the main thread where
    /// the design names one.
    #[test]
    #[cfg_attr(
        all(miri, target_os = "macos"),
        ignore = "the pinned nightly's Miri has no `pthread_main_np` on macOS, which the probe asks whether the thread is the main one"
    )]
    fn the_default_size_is_the_platforms() {
        let size = call_stack_default_size();
        assert!(size >= 32 * 1024 && size.is_multiple_of(1024));
    }

    fn holds_a_local(bounds: Option<CallStack>) -> bool {
        let here = 0u8;
        let local = std::ptr::addr_of!(here).addr();
        bounds.is_some_and(|stack| {
            local < stack.base && local >= stack.base.wrapping_sub(stack.max_size)
        })
    }
}
