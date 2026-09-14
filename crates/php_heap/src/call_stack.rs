//! The calling thread's native stack, as the platform describes it
//! (design §5.6), after `Zend/zend_call_stack.c`: `zend_call_stack_get`
//! asks for the thread's base and size, and the compiler's stack limit
//! and `execute`'s re-entry limit are both built from that pair less
//! `zend.reserved_stack_size`. php asks one platform at a time, an arm
//! per `#ifdef`, and answers `false` on a platform it has no arm for;
//! what is here is the two arms the oracle runs on, macOS and Linux,
//! each copied from its function, and `None` elsewhere, which leaves
//! php's own fallback standing where the caller builds the limit: a
//! local's address for the base and [`call_stack_default_size`] for the
//! size, `zend_call_stack_default_size` arm for arm. This is the crate
//! that may say `unsafe`, and each call into the platform is one
//! operation per block with what makes it sound beside it.

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
#[cfg(target_os = "linux")]
#[inline]
fn position() -> usize {
    let here = 0u8;
    std::ptr::addr_of!(here).addr()
}

/// `zend_call_stack_get_macos`: the base is `pthread_get_stackaddr_np`
/// and the size `pthread_get_stacksize_np`, both of the calling thread,
/// save the main thread of an Intel Mac, whose size the platform
/// under-reports and php states as the 8 MiB default instead.
#[cfg(target_os = "macos")]
mod platform {
    use super::CallStack;

    /// `pthread_main_np`, non-zero on the process's first thread.
    pub(super) fn is_main_thread() -> bool {
        // SAFETY: `pthread_main_np` takes no argument and reads nothing
        // of ours; it answers a plain number.
        unsafe { libc::pthread_main_np() != 0 }
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "php's macOS arm always answers, and the arm's shape is every arm's"
    )]
    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        // SAFETY: `pthread_self` takes no argument and answers the
        // calling thread's own handle, which stays valid while the
        // thread runs, and this thread is running it.
        let this = unsafe { libc::pthread_self() };
        // SAFETY: `this` is the calling thread's handle, so it names a
        // live thread; the call reads its attributes and writes nothing.
        let base = unsafe { libc::pthread_get_stackaddr_np(this) };
        let max_size = if cfg!(not(target_arch = "aarch64")) && is_main_thread() {
            8 * 1024 * 1024
        } else {
            // SAFETY: as above, `this` names a live thread and the call
            // reads its attributes and writes nothing.
            unsafe { libc::pthread_get_stacksize_np(this) }
        };
        Some(CallStack {
            base: base.addr(),
            max_size,
        })
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

    use super::{CallStack, position};

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
        let mut rlim = MaybeUninit::<libc::rlimit>::uninit();
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
        let mut max_size = usize::try_from(rlim.rlim_cur).ok()?;
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
}

/// A platform php has an arm for that this crate does not lay down, or
/// one php has none for: nothing answers, and the fallback stands.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::CallStack;

    pub(super) fn call_stack_bounds() -> Option<CallStack> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local of the asking frame lies inside the bounds the platform
    /// answers, on the thread the harness runs the test on and on one
    /// spawned at a size of this test's choosing, whose size is what
    /// was asked for, to the page.
    #[test]
    fn a_local_lies_within_the_bounds_the_platform_answers() {
        if cfg!(not(any(target_os = "macos", target_os = "linux"))) {
            assert_eq!(call_stack_bounds(), None);
            return;
        }
        assert!(holds_a_local(call_stack_bounds()));
        let size = 1024 * 1024;
        let spawned = std::thread::Builder::new()
            .stack_size(size)
            .spawn(|| (call_stack_bounds(), holds_a_local(call_stack_bounds())))
            .ok()
            .and_then(|thread| thread.join().ok());
        let Some((bounds, held)) = spawned else {
            return;
        };
        assert!(held);
        let max_size = bounds.map_or(0, |stack| stack.max_size);
        assert!(
            max_size >= size && max_size < 2 * size,
            "a thread spawned at {size} bytes reads as {max_size}"
        );
    }

    /// The assumed size is a positive number of whole KiB on every
    /// platform, and the platform's default for the main thread where
    /// the design names one.
    #[test]
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
