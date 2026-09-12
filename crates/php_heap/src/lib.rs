//! The memory model (design §4.1, §5.1–§5.6, §5.13, §5.14): the zval and
//! its refcounted header, the heap that allocates and accounts for every
//! PHP-visible value, the VM stack with its entry slot, the map table
//! that gives an immutable shared structure its per-request state, and
//! the one upward seam, [`HeapHooks`], through which a refcount reaching
//! zero reaches user code without this crate knowing what a VM is.
//!
//! The crate is the first of the `unsafe` homes the manifest names, and
//! the allocator of §5.13 and the string of §5.2 are here in full: a
//! request's memory is chunks, bins and `runs` php's way, and the numbers
//! php reports come from that accounting and from nowhere else. The
//! array, the object and the VM stack are still safe stand-ins whose
//! public shape is `heap.lock`, and the copy of `zend_hash` and
//! `zend_vm_stack` lands behind it (design §11, days 3–7). Nothing here
//! panics: a slot that is not there reads as `UNDEF`, as php's
//! uninitialised zval does.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod gc;
mod hooks;
mod map;
mod mm;
mod stack;
mod string;
mod zval;

pub use hooks::{Fatal, HeapHooks};
pub use map::{MapPtr, MapTable};
pub use stack::{EntryGuard, Frame, FrameRef, Slot, VmStack};
pub use string::Str;
pub use zval::{ObjectId, Zval, ZvalType};

/// The per-request heap (design §5.13): the allocator every string,
/// array, object and frame passes through, and the only accountant of
/// memory. `memory_get_usage()` reads it; no other item lets a native
/// tell the engine about memory.
///
/// The heap owns what it handed out. Dropping it is php's mass release
/// at request end: every chunk and every huge block goes back, so
/// nothing a request allocated outlives the request.
#[derive(Debug)]
pub struct Heap {
    mm: mm::Mm,
}

impl Default for Heap {
    fn default() -> Heap {
        Heap::new()
    }
}

impl Heap {
    /// A heap with php's main chunk mapped and nothing allocated in it.
    #[must_use]
    pub fn new() -> Heap {
        Heap { mm: mm::Mm::new() }
    }

    /// Bytes in use: the bin accounting for `real == false`, the bytes
    /// the chunks hold for `real == true`, which is their count times
    /// 2 MiB (design §5.13).
    #[must_use]
    pub fn memory_usage(&self, real: bool) -> usize {
        if real {
            self.mm.real_size()
        } else {
            self.mm.size()
        }
    }

    /// The high-water mark of the bin accounting since the request began
    /// or the peak was last reset, `memory_get_peak_usage(false)`.
    #[must_use]
    pub fn memory_peak(&self) -> usize {
        self.mm.peak()
    }

    /// The high-water mark of the bytes the chunks hold,
    /// `memory_get_peak_usage(true)`. php asks for both through one
    /// argument; the two are apart here because what answers the
    /// argument is the native above, not the heap.
    #[must_use]
    pub fn memory_real_peak(&self) -> usize {
        self.mm.real_peak()
    }

    /// `memory_reset_peak_usage()`: the peak becomes the current usage.
    pub fn reset_peak(&mut self) {
        self.mm.reset_peak();
    }

    /// The `memory_limit` this request is held to, in bytes.
    #[must_use]
    pub fn memory_limit(&self) -> usize {
        self.mm.limit()
    }

    /// Hold the request to `limit` bytes, which the ini setting sets
    /// once at request start. The limit is met on chunk growth, with
    /// php's exact message as the [`Fatal`].
    pub fn set_memory_limit(&mut self, limit: usize) {
        self.mm.set_limit(limit);
    }

    /// A request-scoped string of `bytes`, php's `zend_string_init`:
    /// one allocation out of this heap, one owner, and the bytes as
    /// given.
    pub fn string(&mut self, bytes: &[u8]) -> Result<Str, Fatal> {
        let at = self.mm.alloc(Str::allocation_size(bytes.len()))?;
        // SAFETY: the allocation is exactly the size `init` asks for and
        // nothing else holds it.
        Ok(unsafe { Str::init(at, bytes, 0) })
    }

    /// `zend_string_release`: one owner fewer, and the string is freed
    /// when the last gives it up. An interned string is never freed.
    pub fn release_string(&mut self, s: Str) {
        if s.is_interned() || s.delref() != 0 {
            return;
        }
        if s.is_persistent() {
            string::free_persistent(s);
            return;
        }
        // SAFETY: the string came from this heap's `string` and its last
        // owner has given it up, so nothing names it.
        unsafe { self.mm.free(s.address()) };
    }

    /// One more owner of `z`: a refcount increment for a refcounted
    /// value, nothing for a scalar or an immutable value (`ZVAL_COPY`).
    pub fn addref(&self, z: Zval) {
        if !z.is_refcounted() {
            return;
        }
        if let Some(s) = z.as_str() {
            s.addref();
        }
    }

    /// One owner fewer: a refcount decrement, and when it reaches zero the
    /// value is freed through `hooks`, which may serve a destructor
    /// (`zval_ptr_dtor`). The parameter is what says this may re-enter.
    pub fn release(&mut self, z: Zval, hooks: &mut dyn HeapHooks) {
        let _ = hooks;
        if !z.is_refcounted() {
            return;
        }
        if let Some(s) = z.as_str() {
            self.release_string(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mappings the allocator's tests count are the process's, and
    /// the test runner `runs` its tests beside each other, so a test that
    /// maps a chunk takes this first.
    fn alone() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        match SERIAL.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// A request that allocates and frees reads back where it started,
    /// and every size lands in the bin, `run` or huge block php's
    /// arithmetic puts it in.
    #[test]
    fn what_a_request_allocates_and_frees_reads_back_where_it_started() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let real = heap.memory_usage(true);
        assert_eq!(start, 0);
        assert_eq!(real, mm::CHUNK_SIZE);
        let mut strings = Vec::new();
        for len in [0usize, 1, 7, 8, 100, 3000, 5000, 3_000_000] {
            let bytes = vec![b'x'; len];
            let allocated = heap.string(&bytes);
            assert!(allocated.is_ok(), "the heap would not allocate {len}");
            if let Ok(s) = allocated {
                assert_eq!(s.len(), len);
                assert_eq!(s.bytes(), bytes.as_slice());
                assert_eq!(s.refcount(), 1);
                strings.push(s);
            }
        }
        assert!(heap.memory_usage(false) > start);
        for s in strings {
            heap.release_string(s);
        }
        assert_eq!(heap.memory_usage(false), start);
        assert_eq!(heap.memory_usage(true), real);
        assert!(heap.memory_peak() > start);
        // The real peak held the huge block beside the chunk, rounded up
        // to a page as php rounds one.
        assert_eq!(
            heap.memory_real_peak(),
            real + 3_000_000usize.next_multiple_of(4096)
        );
    }

    /// The bin accounting is php's: a string of four bytes takes the
    /// thirty-two byte bin, header, NUL and alignment included.
    #[test]
    fn a_string_takes_the_bytes_php_s_takes() {
        let _alone = alone();
        let mut heap = Heap::new();
        let allocated = heap.string(b"demo");
        assert!(allocated.is_ok(), "the heap would not allocate four bytes");
        let Ok(s) = allocated else { return };
        assert_eq!(heap.memory_usage(false), 32);
        let z = Zval::string(s);
        assert!(z.is_refcounted());
        heap.addref(z);
        assert_eq!(s.refcount(), 2);
        let mut nothing = Recorder;
        heap.release(z, &mut nothing);
        assert_eq!(s.refcount(), 1);
        heap.release(z, &mut nothing);
        assert_eq!(heap.memory_usage(false), 0);
    }

    /// The limit is met on chunk growth, with php's own text.
    #[test]
    fn the_memory_limit_is_php_s_message() {
        let _alone = alone();
        let mut heap = Heap::new();
        heap.set_memory_limit(mm::CHUNK_SIZE);
        let mut held = Vec::new();
        let mut fatal = None;
        for _ in 0..4096 {
            match heap.string(&[b'x'; 3000]) {
                Ok(s) => held.push(s),
                Err(f) => {
                    fatal = Some(f);
                    break;
                }
            }
        }
        assert_eq!(
            fatal,
            Some(Fatal::Error(
                "Allowed memory size of 2097152 bytes exhausted (tried to allocate 12288 bytes)"
                    .to_string()
            ))
        );
    }

    /// php sweeps the wholly free bin `runs` before it declares the
    /// limit reached (`zend_mm_gc`), so the limit is met where php's is
    /// met and not before: a request holding every slot it took is
    /// stopped, and the same request having given them back goes on,
    /// out of the pages the sweep handed back and with no chunk more.
    #[test]
    fn the_limit_is_met_after_the_sweep_of_free_bin_runs_and_not_before() {
        let _alone = alone();
        let mut heap = Heap::new();
        heap.set_memory_limit(2 * mm::CHUNK_SIZE);
        // A string of 3000 bytes is the 3072-byte bin, four slots to a
        // `run` of three pages, so two chunks hold 340 `runs` and the
        // next one meets the limit.
        let mut held = Vec::new();
        let fatal = loop {
            match heap.string(&[b'x'; 3000]) {
                Ok(s) => held.push(s),
                Err(f) => break f,
            }
        };
        assert_eq!(held.len(), 340 * 4);
        assert_eq!(heap.memory_usage(true), 2 * mm::CHUNK_SIZE);
        assert_eq!(
            fatal,
            Fatal::Error(
                "Allowed memory size of 4194304 bytes exhausted (tried to allocate 12288 bytes)"
                    .to_string()
            )
        );
        // A 1760-byte string is the 1792-byte bin, a `run` of seven
        // pages, which neither chunk has free. Nothing is on a free
        // list, so the sweep collects nothing and the limit stands.
        assert_eq!(
            heap.string(&[b'x'; 1760]).err(),
            Some(Fatal::Error(
                "Allowed memory size of 4194304 bytes exhausted (tried to allocate 28672 bytes)"
                    .to_string()
            ))
        );
        // Every slot back on its bin's free list. The pages are still
        // the request's until the sweep walks them, so the same
        // allocation still finds no seven free pages anywhere.
        for s in held.drain(..) {
            heap.release_string(s);
        }
        assert_eq!(heap.memory_usage(false), 0);
        assert_eq!(heap.memory_usage(true), 2 * mm::CHUNK_SIZE);
        let swept = heap.string(&[b'x'; 1760]);
        assert!(swept.is_ok(), "the sweep did not give the free runs back");
        // The 1020 pages of the 340 `runs`, and the chunk the sweep
        // emptied kept against the next growth, as php keeps one.
        assert_eq!(heap.memory_usage(true), 2 * mm::CHUNK_SIZE);
        assert_eq!(heap.memory_usage(false), 1792);
        if let Ok(s) = swept {
            heap.release_string(s);
        }
    }

    /// A request that outgrows one chunk takes more of them, gives the
    /// pages back as it frees, and leaves no mapping behind when it ends,
    /// which is php's mass release at request end.
    #[test]
    fn a_request_that_ends_leaves_no_mapping_behind() {
        use std::sync::atomic::Ordering::Relaxed;
        let _alone = alone();
        let before = mm::MAPPINGS.load(Relaxed);
        {
            let mut heap = Heap::new();
            let mut held = Vec::new();
            // Runs of a whole page each, past what one chunk holds.
            for _ in 0..700 {
                let allocated = heap.string(&[b'x'; 4000]);
                assert!(allocated.is_ok(), "the heap would not allocate a run");
                if let Ok(s) = allocated {
                    held.push(s);
                }
            }
            assert!(heap.memory_usage(true) > mm::CHUNK_SIZE);
            assert!(mm::MAPPINGS.load(Relaxed) > before);
            for s in held {
                heap.release_string(s);
            }
            assert_eq!(heap.memory_usage(false), 0);
        }
        assert_eq!(mm::MAPPINGS.load(Relaxed), before);
    }

    /// What the heap's tests pass where a request would serve a
    /// destructor: nothing to serve, because a string has none.
    struct Recorder;

    impl HeapHooks for Recorder {
        fn destruct(&mut self, _obj: ObjectId) {}
        fn free_native(&mut self, _obj: ObjectId) {}
        fn gc_children(&mut self, _obj: ObjectId, _visit: &mut dyn FnMut(Zval)) {}
        fn free_resource(&mut self, _res: u32) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::OutOfMemory(bytes)
        }
    }
}
