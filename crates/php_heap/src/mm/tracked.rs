//! php's tracked allocator, `USE_ZEND_ALLOC=0` with `USE_TRACKED_ALLOC=1`
//! (`Z/Zend/zend_alloc.c:3286-3413` and `:3613-3641` at 8.5.12): every
//! block the system's `malloc`, `realloc` and `free`, and the bytes asked
//! for each kept by its address, `tracked_allocs`, so the heap counts
//! what it has out, holds it to `memory_limit` and gives every block back
//! at the request's end. The heap's chunks, bins and runs are not used:
//! `memory_get_usage()` is the sum of the bytes asked for, exactly, and
//! `memory_get_usage(true)` the same number.
//!
//! It is a variant of the heap's allocator ([`super::Allocator`]), chosen
//! once per process from the environment as php's `alloc_globals_ctor`
//! chooses it ([`chosen`]); the limit, the delayed OOM and the fallible
//! scope are the heap's own, applied as `tracked_check_limit` applies
//! them, without the sweep.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::ptr::NonNull;
use std::sync::OnceLock;

/// Whether this process serves its requests' heaps from the tracked
/// allocator: `USE_ZEND_ALLOC` set to a number that is zero and
/// `USE_TRACKED_ALLOC` to one that is not, as `ZEND_ATOL` reads them.
/// Read once, from the environment the process started with, as php
/// reads it in `alloc_globals_ctor`; every heap of the
/// process is then tracked or none is.
///
/// `USE_ZEND_ALLOC=0` alone is php's plain system allocator, which keeps
/// no count and holds nothing to the limit; this heap has no such
/// variant and keeps its own allocator there.
///
/// A child served under the two variables counts a string's bytes to
/// the byte, has no chunk behind `memory_get_usage(true)` and no peak,
/// and is held to its limit by the bytes it asks for. The fatal's stack
/// trace is cut: past the limit every ask of a tracked heap is held to
/// it, so php 8.5.12's `zend_fetch_debug_backtrace`, which asks for its
/// frames fallibly, lists none, which the engine's does not do yet
/// (#3260).
///
/// ```php
/// $code = '$m = memory_get_usage(); $s = str_repeat("x", 5000);'
///     . ' var_dump(memory_get_usage() - $m, memory_get_usage() === memory_get_usage(true), memory_get_peak_usage());'
///     . ' ini_set("memory_limit", memory_get_usage() + 1000); $t = str_repeat("y", 3000); echo "after\n";';
/// $env = ['USE_ZEND_ALLOC' => '0', 'USE_TRACKED_ALLOC' => '1'];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $code], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes, null, $env);
/// $out = stream_get_contents($pipes[1]);
/// echo preg_replace(['/size of \d+ bytes/', '/\nStack trace:.*/s'], ['size of N bytes', "\n"], $out);
/// var_dump(proc_close($p));
/// ```
/// ```expect process
/// int(5032)
/// bool(true)
/// int(0)
///
/// Fatal error: Allowed memory size of N bytes exhausted (tried to allocate 3032 bytes) in Command line code on line 1
/// int(255)
/// ```
pub(crate) fn chosen() -> bool {
    static CHOSEN: OnceLock<bool> = OnceLock::new();
    *CHOSEN.get_or_init(|| {
        let zend = std::env::var_os("USE_ZEND_ALLOC");
        let tracked = std::env::var_os("USE_TRACKED_ALLOC");
        from_env(zend.as_deref(), tracked.as_deref())
    })
}

/// [`chosen`]'s reading of the two variables.
fn from_env(zend: Option<&OsStr>, tracked: Option<&OsStr>) -> bool {
    zend.is_some_and(|value| atol(value.as_encoded_bytes()) == 0)
        && tracked.is_some_and(|value| atol(value.as_encoded_bytes()) != 0)
}

/// `ZEND_ATOL`, `atoll`: blanks skipped, a sign, then the digits up to
/// the first byte that is not one; a string that opens with none is zero.
fn atol(text: &[u8]) -> i64 {
    let mut rest = text;
    while let [b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c, tail @ ..] = rest {
        rest = tail;
    }
    let negative = matches!(rest.first(), Some(b'-'));
    if matches!(rest.first(), Some(b'-' | b'+')) {
        rest = &rest[1..];
    }
    let mut value: i64 = 0;
    for digit in rest.iter().take_while(|b| b.is_ascii_digit()) {
        value = value.wrapping_mul(10).wrapping_add(i64::from(digit - b'0'));
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

/// `tracked_allocs`: each block the heap has out of the system, by its
/// address, with the bytes asked for it. php keys its table by the
/// address shifted past the alignment; a `HashMap` keyed by the address
/// itself is the same table. Its own memory is the process's, as php's
/// persistent table is, and counts toward nothing.
#[derive(Debug, Default)]
pub(crate) struct Tracked {
    blocks: HashMap<usize, Block>,
}

/// One block out: the system's pointer to it, and the bytes asked.
#[derive(Debug, Clone, Copy)]
struct Block {
    at: NonNull<u8>,
    size: usize,
}

impl Tracked {
    /// The bytes asked for the block at `at`, where it is one of these.
    pub(super) fn size_of(&self, at: NonNull<u8>) -> Option<usize> {
        self.blocks.get(&at.as_ptr().addr()).map(|block| block.size)
    }

    /// Whether `at` starts a block of these, what php's `is_zend_ptr` and
    /// `tracked_get_size_zv` answer.
    pub(super) fn holds(&self, address: usize) -> bool {
        self.blocks.contains_key(&address)
    }

    /// `malloc(size)` and `tracked_add`: `None` where the system has
    /// nothing to give. A block of no bytes is asked as one, so its
    /// address is the system's own and no other block's.
    pub(super) fn malloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        // SAFETY: `malloc` takes a byte count and touches nothing of ours.
        let at = NonNull::new(unsafe { libc::malloc(size.max(1)) }.cast::<u8>())?;
        self.blocks.insert(at.as_ptr().addr(), Block { at, size });
        Some(at)
    }

    /// `tracked_realloc` past its limit check: the old record dropped,
    /// `realloc`, and the new one added; `None` where the system has
    /// nothing to give, the block standing as it was and still recorded.
    ///
    /// # Safety
    ///
    /// `at` is a block of these that nothing reads or writes during the
    /// call; past a `Some`, only the answer names its bytes.
    pub(super) unsafe fn realloc(&mut self, at: NonNull<u8>, size: usize) -> Option<NonNull<u8>> {
        // SAFETY: the caller's contract: `at` is a live block `malloc` or
        // `realloc` gave these, so `realloc` may move or resize it.
        let to = unsafe { libc::realloc(at.as_ptr().cast(), size.max(1)) };
        let to = NonNull::new(to.cast::<u8>())?;
        self.blocks.remove(&at.as_ptr().addr());
        self.blocks
            .insert(to.as_ptr().addr(), Block { at: to, size });
        Some(to)
    }

    /// `tracked_free`: the block's record dropped and the block given back
    /// to the system; the bytes it was asked for are the answer, `None`
    /// for an address that is no block of these, which is left alone.
    ///
    /// # Safety
    ///
    /// Where `at` is a block of these, nothing names it after the call.
    pub(super) unsafe fn free(&mut self, at: NonNull<u8>) -> Option<usize> {
        let block = self.blocks.remove(&at.as_ptr().addr())?;
        // SAFETY: the caller's contract: the block is live, `malloc` or
        // `realloc` gave it these, and nothing names it from here on.
        unsafe { libc::free(block.at.as_ptr().cast()) };
        Some(block.size)
    }
}

impl Drop for Tracked {
    /// `tracked_free_all` as the heap ends: every block still out given
    /// back to the system.
    fn drop(&mut self) {
        for block in self.blocks.values() {
            // SAFETY: every block the table keeps is a live one `malloc`
            // or `realloc` gave it, and the heap that names them is
            // ending, so nothing names them after this.
            unsafe { libc::free(block.at.as_ptr().cast()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{atol, from_env};
    use crate::FatalReason;
    use crate::mm::{DelayedOom, Mm};
    use std::ffi::OsStr;

    #[test]
    fn a_tracked_heap_counts_the_bytes_asked_and_maps_no_chunk() {
        let mut mm = Mm::tracked();
        assert_eq!((mm.size(), mm.real_size()), (0, 0));
        let Ok(a) = mm.alloc(5) else {
            panic!("the system gives five bytes");
        };
        let Ok(b) = mm.alloc(3000) else {
            panic!("the system gives 3000 bytes");
        };
        assert_eq!((mm.size(), mm.real_size()), (3005, 3005));
        assert_eq!(
            (mm.peak(), mm.real_peak()),
            (0, 0),
            "php's tracked arm moves no peak"
        );
        assert!(mm.owns(a) && mm.holds(b.as_ptr().addr()));
        assert!(
            !mm.holds(b.as_ptr().addr() + 1),
            "is_zend_ptr asks of a block's start"
        );
        // SAFETY: `a` is live in this heap.
        assert_eq!(unsafe { mm.usable(a) }, 5);
        // SAFETY: `a` is live in this heap and holds no bytes read back.
        let Ok(a) = (unsafe { mm.realloc(a, 0, 100) }) else {
            panic!("the system grows the block");
        };
        assert_eq!(mm.size(), 3100);
        // SAFETY: `b` is live in this heap and nothing names it after.
        unsafe { mm.free(b) };
        assert_eq!((mm.size(), mm.real_size()), (100, 100));
        assert_eq!(mm.gc(), 0, "a custom heap with no sweep of its own");
        let _ = a; // given back as the heap ends
    }

    #[test]
    fn a_tracked_heap_is_held_to_the_limit_by_the_bytes_asked() {
        let mut mm = Mm::tracked();
        let Ok(a) = mm.alloc(1000) else {
            panic!("the system gives 1000 bytes");
        };
        assert!(!mm.set_limit(999), "under what is out");
        assert!(mm.set_limit(1500));
        let was = mm.set_fallible(true);
        let refused = mm.alloc(501).map_err(crate::Fatal::into_reason);
        assert_eq!(refused, Err(FatalReason::Refused));
        assert!(mm.take_refused());
        // SAFETY: `a` is live in this heap; a refused grow leaves it.
        let grown = unsafe { mm.realloc(a, 1000, 1600) }.map_err(crate::Fatal::into_reason);
        assert_eq!(grown, Err(FatalReason::Refused));
        assert_eq!(mm.size(), 1000, "a refusal moves nothing");
        mm.set_fallible(was);
        let Ok(b) = mm.alloc(501) else {
            panic!("an infallible ask past the soft limit is served");
        };
        assert_eq!(mm.size(), 1501);
        let DelayedOom::Pending(pending) = mm.delayed().clone() else {
            panic!("the limit leaves the delayed OOM");
        };
        assert_eq!(
            pending.message,
            "Allowed memory size of 1500 bytes exhausted (tried to allocate 501 bytes)"
        );
        // SAFETY: `b` is live in this heap and nothing names it after.
        unsafe { mm.free(b) };
        // SAFETY: as above, for `a`.
        unsafe { mm.free(a) };
        assert_eq!(mm.size(), 0);
    }

    #[test]
    fn the_environment_chooses_as_php_reads_it() {
        let on = |zend: Option<&str>, tracked: Option<&str>| {
            from_env(zend.map(OsStr::new), tracked.map(OsStr::new))
        };
        assert!(on(Some("0"), Some("1")));
        assert!(on(Some(" 0"), Some("2x")));
        assert!(on(Some("off"), Some("-1")), "atoll reads no digits as zero");
        assert!(!on(Some("1"), Some("1")));
        assert!(!on(Some("0"), Some("0")));
        assert!(!on(Some("0"), None), "the plain system allocator");
        assert!(!on(None, Some("1")));
        assert_eq!(atol(b"\t+42abc"), 42);
        assert_eq!(atol(b"-7"), -7);
    }
}
