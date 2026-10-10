//! The main thread's bounds, which only a test that is the process's
//! `main` can read: the harness runs every `#[test]` on a thread of its
//! own. On macOS the Rust runtime remaps the lowest page of the main
//! thread's stack `PROT_NONE` before `main`, so a walk down to the end
//! `php_heap::call_stack_bounds` answers faults there unless the bounds
//! end above that page (#1104).

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

fn main() {
    #[cfg(all(target_os = "macos", not(miri)))]
    {
        let Some(stack) = php_heap::call_stack_bounds() else {
            eprintln!("the main thread's bounds were not read");
            std::process::exit(1);
        };
        let end = stack.base - stack.max_size;
        let deepest = descend(end);
        println!(
            "the main thread's stack is usable down to {:#x}, {} bytes above the end the bounds answer",
            deepest,
            deepest - end
        );
    }
}
