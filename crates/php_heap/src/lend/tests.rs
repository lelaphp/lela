//! The lend as C sees it: the six entries called as a library compiled
//! against them calls them, under a lend and outside one.

use std::ffi::{CStr, c_void};

use super::{ecalloc, efree, emalloc, erealloc, estrdup, estrndup};
use crate::Heap;
use crate::hooks::{Fatal, FatalReason};
use crate::serial::alone;

/// Under a lend a block is the heap's, counted at its bin and in the
/// peak, and its free gives the bytes back; outside one the heap sees
/// nothing of either.
#[test]
fn a_lent_block_is_counted_and_given_back_and_an_unlent_one_is_not_seen() {
    let _alone = alone();
    let mut heap = Heap::new();
    let start = heap.memory_usage(false);
    let Ok(at) = heap.lend(|| emalloc(240)) else {
        return;
    };
    assert_eq!(heap.memory_usage(false), start + 256, "the 256 bin");
    // SAFETY: a block the lend answered, freed under a lend of its heap.
    let freed = heap.lend(|| unsafe { efree(at) });
    assert!(freed.is_ok());
    assert_eq!(heap.memory_usage(false), start);
    assert_eq!(heap.memory_peak(), start + 256);

    let outside = emalloc(240);
    assert!(!outside.is_null());
    assert_eq!(
        heap.memory_usage(false),
        start,
        "the system's, not the heap's"
    );
    // A system block freed under a lend goes back to the system.
    // SAFETY: a live block `emalloc` answered outside a lend.
    let freed = heap.lend(|| unsafe { efree(outside) });
    assert!(freed.is_ok());
    assert_eq!(heap.memory_usage(false), start);
}

/// The lend ends with the call: an entry called after it is the system
/// allocator's, and a lend inside another puts the outer one back.
#[test]
fn the_lend_ends_with_the_call_and_an_inner_lend_puts_the_outer_back() {
    let _alone = alone();
    let mut outer = Heap::new();
    let mut inner = Heap::new();
    let outer_start = outer.memory_usage(false);
    let inner_start = inner.memory_usage(false);
    let lent = outer.lend(|| {
        let a = inner.lend(|| emalloc(64));
        let b = emalloc(64);
        (a, b)
    });
    let Ok((Ok(a), b)) = lent else {
        return;
    };
    assert_eq!(inner.memory_usage(false), inner_start + 64);
    assert_eq!(outer.memory_usage(false), outer_start + 64);
    let after = emalloc(64);
    assert_eq!(outer.memory_usage(false), outer_start + 64);
    // SAFETY: the block freed under a lend of the heap that made it.
    let freed = outer.lend(|| unsafe { efree(b) });
    assert!(freed.is_ok());
    let freed = inner.lend(|| {
        for block in [a, after] {
            // SAFETY: a block of this heap, and the system's, each freed once.
            unsafe { efree(block) };
        }
    });
    assert!(freed.is_ok());
    assert_eq!(outer.memory_usage(false), outer_start);
    assert_eq!(inner.memory_usage(false), inner_start);
}

/// `ecalloc` zeroes, `erealloc` keeps the bytes as it moves a block to
/// a larger bin, `estrdup` and `estrndup` copy with a terminator.
#[test]
fn the_entries_keep_their_c_contracts_under_a_lend() {
    let _alone = alone();
    let mut heap = Heap::new();
    let start = heap.memory_usage(false);
    let lent = heap.lend(|| {
        let zeroed = ecalloc(3, 8).cast::<u8>();
        // SAFETY: 24 bytes `ecalloc` answered.
        let all_zero = unsafe { std::slice::from_raw_parts(zeroed, 24) }
            .iter()
            .all(|b| *b == 0);
        // SAFETY: the block is live and has 24 bytes.
        unsafe { zeroed.write_bytes(7, 24) };
        // SAFETY: a live block of these entries.
        let grown = unsafe { erealloc(zeroed.cast(), 100) }.cast::<u8>();
        // SAFETY: the first 24 bytes moved with it.
        let kept = unsafe { std::slice::from_raw_parts(grown, 24) }
            .iter()
            .all(|b| *b == 7);
        // SAFETY: a NUL-terminated literal.
        let dup = unsafe { estrdup(c"Europe/Amsterdam".as_ptr()) };
        // SAFETY: six readable bytes.
        let ndup = unsafe { estrndup(c"Europe/Amsterdam".as_ptr(), 6) };
        // SAFETY: terminated by `estrdup`.
        let whole = unsafe { CStr::from_ptr(dup) }.to_owned();
        // SAFETY: terminated by `estrndup`.
        let part = unsafe { CStr::from_ptr(ndup) }.to_owned();
        let texts = (whole, part);
        for block in [grown.cast::<c_void>(), dup.cast(), ndup.cast()] {
            // SAFETY: a live block of these entries, freed once.
            unsafe { efree(block) };
        }
        (all_zero, kept, texts)
    });
    let Ok((all_zero, kept, (dup, ndup))) = lent else {
        return;
    };
    assert!(all_zero && kept);
    assert_eq!(dup.as_bytes(), b"Europe/Amsterdam");
    assert_eq!(ndup.as_bytes(), b"Europe");
    assert_eq!(heap.memory_usage(false), start);
}

/// The limit refused under a lend: the block is the system's, the call
/// runs to its end, and the lend answers the limit's fatal. The block
/// goes back to the system under a later lend.
#[test]
fn a_refusal_of_the_limit_is_served_by_the_system_and_answered_after_the_call() {
    let _alone = alone();
    let mut heap = Heap::new();
    let start = heap.memory_usage(false);
    assert!(heap.set_memory_limit(2 * 1024 * 1024));
    let mut kept: *mut c_void = std::ptr::null_mut();
    let lent = heap.lend(|| {
        kept = emalloc(4 * 1024 * 1024);
        !kept.is_null()
    });
    assert!(
        matches!(lent.as_ref().map_err(Fatal::reason), Err(FatalReason::Error(text)) if text.starts_with("Allowed memory size of 2097152 bytes exhausted")),
        "{lent:?}"
    );
    assert!(!kept.is_null());
    assert_eq!(heap.memory_usage(false), start);
    // SAFETY: the system's block the refusal was served with.
    let freed = heap.lend(|| unsafe { efree(kept) });
    assert!(freed.is_ok());
}
