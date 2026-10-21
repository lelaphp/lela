//! The main thread's bounds, which only a test that is the process's
//! `main` can read: the harness runs every `#[test]` on a thread of its
//! own. On macOS the Rust runtime remaps the lowest page of the main
//! thread's stack `PROT_NONE` before `main`, so a walk down to the end
//! `php_heap::call_stack_bounds` answers faults there unless the bounds
//! end above that page (#1104). They end one page above the end php's
//! `zend_call_stack_get_macos` reckons, which `docs/divergence.toml`
//! records as `trace:macos-main-thread-guard-page` (#2583). The size
//! those bounds answer is what `php_heap::main_call_stack_size` answers
//! from any thread (#3142).

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

/// A frame that writes a KiB of its own and calls itself until the KiB
/// stands within 2 KiB of `end`: the deepest write lands in the last
/// 3 KiB the bounds call the thread's, and faults where they are wrong.
#[cfg(all(target_os = "macos", not(miri)))]
#[inline(never)]
fn descend(end: usize) -> usize {
    let mut pad = [0u8; 1024];
    std::hint::black_box(&mut pad);
    let here = pad.as_ptr().addr();
    if here < end + 2048 {
        return here;
    }
    let deepest = descend(end);
    std::hint::black_box(&pad);
    deepest
}

/// The end php reckons for the calling thread's stack: the base less
/// `pthread_get_stacksize_np`, with no page left for a guard.
#[cfg(all(target_os = "macos", not(miri)))]
fn php_end(base: usize) -> Option<usize> {
    // SAFETY: `pthread_self` takes no argument and answers the calling
    // thread's own handle, valid while the thread runs.
    let this = unsafe { libc::pthread_self() };
    // SAFETY: `this` names the calling thread, which is live; the call
    // reads its attributes and writes nothing.
    let size = unsafe { libc::pthread_get_stacksize_np(this) };
    base.checked_sub(size)
}

/// The page size, as the bounds read it.
#[cfg(all(target_os = "macos", not(miri)))]
fn page_size() -> Option<usize> {
    // SAFETY: `sysconf` takes one name, reads and writes nothing of ours
    // and answers a plain number.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).ok()
}

/// The main thread's bounds end one page above the end php reckons, the
/// page Rust's runtime guards, and the thread's stack is usable down to
/// them: the end `trace:macos-main-thread-guard-page` records. The guard
/// starts at the first page boundary at or above php's end, so the
/// distance is one page where the stack's size is a whole number of
/// pages and less than two where it is not.
#[cfg(all(target_os = "macos", not(miri)))]
fn the_main_threads_end_is_one_page_above_phps() -> Result<(), String> {
    let stack = php_heap::call_stack_bounds().ok_or("the main thread's bounds were not read")?;
    let end = stack.base - stack.max_size;
    let php = php_end(stack.base).ok_or("the thread's size was larger than its base")?;
    let page = page_size().ok_or("the page size was not read")?;
    let above = end
        .checked_sub(php)
        .ok_or("the bounds end below the end php reckons")?;
    if !(page..2 * page).contains(&above) {
        return Err(format!(
            "the bounds end {above} bytes above the end php reckons, not the one page of {page} the runtime guards"
        ));
    }
    let deepest = descend(end);
    println!(
        "the main thread's stack is usable down to {:#x}, {} bytes above the end the bounds answer, which is {} bytes above php's",
        deepest,
        deepest - end,
        above
    );
    Ok(())
}

/// The main thread's size as the main thread reads its own bounds, and
/// as a thread started at another size asks for it: one number, where
/// the platform's arm reads the main thread from anywhere.
#[cfg(all(any(target_os = "macos", target_os = "linux"), not(miri)))]
fn the_main_threads_size_reads_the_same_from_any_thread() -> Result<(), String> {
    let own = php_heap::call_stack_bounds()
        .map(|stack| stack.max_size)
        .ok_or("the main thread's bounds were not read")?;
    let here = php_heap::main_call_stack_size();
    let there = std::thread::Builder::new()
        .stack_size(3 * 1024 * 1024)
        .spawn(php_heap::main_call_stack_size)
        .map_err(|err| format!("spawning a thread: {err}"))?
        .join()
        .map_err(|_| "the spawned thread panicked")?;
    if here != Some(own) || there != Some(own) {
        return Err(format!(
            "the main thread's bounds answer {own} bytes, main_call_stack_size {here:?} on it and {there:?} on a thread of 3 MiB"
        ));
    }
    Ok(())
}

fn main() {
    #[cfg(all(target_os = "macos", not(miri)))]
    if let Err(message) = the_main_threads_end_is_one_page_above_phps() {
        eprintln!("{message}");
        std::process::exit(1);
    }
    #[cfg(all(any(target_os = "macos", target_os = "linux"), not(miri)))]
    if let Err(message) = the_main_threads_size_reads_the_same_from_any_thread() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
