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
mod numeric;
mod stack;
mod string;
mod zval;

pub use hooks::{Fatal, HeapHooks};
pub use map::{MapPtr, MapTable};
pub use numeric::{Number, double_to_string, numeric_prefix, power, shift_left, shift_right};
pub use stack::{EntryGuard, Frame, FrameRef, Slot, VmStack};
pub use string::{Intern, Str, Strings};
pub use zval::{ObjectId, Zval, ZvalType};

/// The per-request heap (design §5.13): the allocator every string,
/// array, object and frame passes through, and the only accountant of
/// memory. `memory_get_usage()` reads it; no other item lets a native
/// tell the engine about memory.
///
/// The heap owns what it handed out. Dropping it is php's mass release
/// at request end: every chunk and every huge block goes back, so
/// nothing a request allocated outlives the request. What the request
/// interned is here for the same reason (design §5.2): those strings are
/// allocated out of this heap and go back with everything else.
#[derive(Debug)]
pub struct Heap {
    mm: mm::Mm,
    /// php's `CG(interned_strings)`: the strings this request interned,
    /// live until the request ends (`Z/Zend/zend_string.c:346-353`).
    /// The strings are the request's own memory and are counted as such,
    /// and so is the table's own storage in [`string::Buckets`] beside
    /// it.
    interned: string::Table,
    /// The storage that table takes, php's bucket array for the same
    /// `HashTable`: 40960 bytes of the request's own memory on the first
    /// string it interns, and doubling where php's doubles.
    buckets: string::Buckets,
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
        Heap {
            mm: mm::Mm::new(),
            interned: string::Table::default(),
            buckets: string::Buckets::new(),
        }
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

    /// How many strings this request has interned, php's
    /// `zend_hash_num_elements(&CG(interned_strings))`.
    #[must_use]
    pub fn interned(&self) -> usize {
        self.interned.count()
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

impl Intern for Heap {
    /// php's `zend_string_init_interned_request`
    /// (`Z/Zend/zend_string.c:297-327`): the frozen table, then this
    /// request's own, then a short-living interned string out of this
    /// request's memory, which the request's end takes with everything
    /// else. It is flagged interned and never persistent, so no release
    /// frees it and no thread but this one can name it.
    ///
    /// The string is allocated before the table takes room for it, php's
    /// order: `zend_string_init` and then `zend_add_interned_string` over
    /// `zend_hash_add_new`, whose first call is what lays the bucket
    /// array down. So the first string a request interns costs its own
    /// bytes and the table's 40960 beside them, exactly as php's does.
    fn intern(&mut self, bytes: &[u8]) -> Result<Str, Fatal> {
        if let Some(frozen) = string::frozen_string(bytes) {
            return Ok(frozen);
        }
        let hash = string::hash_func(bytes);
        if let Some(held) = self.interned.find(hash, bytes) {
            return Ok(held);
        }
        let at = self.mm.alloc(Str::allocation_size(bytes.len()))?;
        // SAFETY: the allocation is exactly the size `init` asks for and
        // nothing else holds it.
        let s = unsafe { Str::init(at, bytes, string::INTERNED) };
        self.buckets.add_one(&mut self.mm)?;
        self.interned.add(hash, s);
        Ok(s)
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
        // to the page the machine reports as php rounds one.
        assert_eq!(
            heap.memory_real_peak(),
            real + 3_000_000usize.next_multiple_of(mm::real_page_size())
        );
    }

    /// What a request interns is the request's own memory (design §5.2):
    /// the strings are allocated out of this heap, so `memory_get_usage()`
    /// counts them as php counts `CG(interned_strings)`'s, and the second
    /// request reads what the first read before it, because the first
    /// request's end took every one of them.
    ///
    /// **Every number here is php's own**, the bin arithmetic of design
    /// §5.13 over php's sizes: 40 bytes for a string of eight, 32 for one
    /// of four, and 40960 for the table the first intern lays down.
    #[test]
    fn what_a_request_interned_is_its_own_and_the_next_request_starts_where_it_did() {
        use std::sync::atomic::Ordering::Relaxed;
        let _alone = alone();
        let mappings = mm::MAPPINGS.load(Relaxed);
        let mut started_at = Vec::new();
        for _ in 0..2 {
            let mut heap = Heap::new();
            started_at.push(heap.memory_usage(false));
            // The frozen table answers first and allocates nothing.
            let frozen = heap.intern(b"");
            assert!(matches!(frozen, Ok(s) if s.is_empty()));
            assert_eq!(heap.memory_usage(false), 0);
            assert_eq!(heap.interned(), 0);
            // Every other string is one allocation, once.
            let first = heap.intern(b"var_dump");
            let again = heap.intern(b"var_dump");
            assert!(first.is_ok() && again.is_ok());
            let (Ok(first), Ok(again)) = (first, again) else {
                return;
            };
            assert_eq!(first.address(), again.address());
            assert_eq!(heap.interned(), 1);
            // Eight bytes, the header and the NUL: php's forty-byte bin,
            // and the table's own 40960 beside it, laid down by this
            // first intern and by no earlier act.
            assert_eq!(heap.memory_usage(false), 40 + 40960);
            assert!(first.is_interned());
            assert!(
                !first.is_persistent(),
                "a request's interned string is out of the request's heap"
            );
            // A release never frees an interned string, as php's does not;
            // the request's end is what takes it.
            heap.release_string(first);
            assert_eq!(heap.memory_usage(false), 41000);
            // A string the request made and gave up goes back at once.
            let made = heap.string(b"made");
            assert!(made.is_ok());
            if let Ok(made) = made {
                assert_eq!(heap.memory_usage(false), 41032);
                heap.release_string(made);
            }
            assert_eq!(heap.memory_usage(false), 41000);
        }
        assert_eq!(started_at, vec![0, 0], "every request starts at zero");
        assert_eq!(
            mm::MAPPINGS.load(Relaxed),
            mappings,
            "and leaves no mapping behind"
        );
    }

    /// The table's own storage is accounted where php accounts it
    /// (design §5.13): nothing at request start, because
    /// `zend_interned_strings_activate` inits `CG(interned_strings)` with
    /// `persistent` 0 and no eager real-init, and one allocation of
    /// `HT_SIZE_EX(1024, HT_SIZE_TO_MASK(1024))` on the first string the
    /// request interns — 1024 buckets of 32 bytes and 2048 hash slots of
    /// 4, 40960 bytes, ten whole pages of the request's heap.
    ///
    /// **Every number here is php's own.** The table doubles where php's
    /// doubles, on the add that fills it, and php allocates the larger
    /// array before it frees the smaller, which is what the peak reads.
    #[test]
    fn the_interned_table_s_storage_is_php_s_and_doubles_where_php_s_doubles() {
        let _alone = alone();
        let mut heap = Heap::new();
        // The frozen table answers before this one and lays nothing down,
        // so a request that interns only what the process froze pays
        // nothing at all.
        assert!(matches!(heap.intern(b""), Ok(s) if s.is_empty()));
        assert!(matches!(heap.intern(b"a"), Ok(s) if s.len() == 1));
        assert_eq!(heap.memory_usage(false), 0);
        // The first string that is this table's brings the whole array
        // with it, beside the 32-byte bin the string itself takes.
        assert!(heap.intern(b"s0").is_ok(), "the heap would not intern");
        assert_eq!(heap.memory_usage(false), 40960 + 32);
        for i in 1..1024u32 {
            let interned = heap.intern(format!("s{i}").as_bytes());
            assert!(interned.is_ok(), "the heap would not intern s{i}");
        }
        assert_eq!(heap.interned(), 1024);
        // 1024 strings of the 32-byte bin, and the array they fill.
        assert_eq!(heap.memory_usage(false), 40960 + 1024 * 32);
        assert_eq!(heap.memory_peak(), 40960 + 1024 * 32);
        // The one that fills it doubles it: the 2048-bucket array is
        // allocated, what the smaller held is copied over, and the
        // smaller goes back, so the peak holds both for that long.
        assert!(heap.intern(b"s1024").is_ok(), "the heap would not grow");
        assert_eq!(heap.memory_usage(false), 81920 + 1025 * 32);
        assert_eq!(heap.memory_peak(), 81920 + 40960 + 1025 * 32);
    }

    /// A string past 2 MiB is a huge block, and php rounds one to the
    /// page the machine reports, `REAL_PAGE_SIZE`
    /// (`Z/Zend/zend_alloc.c:1925`), not to the 4 KiB page its chunks are
    /// carved into. What php answers on the machine the oracle is pinned
    /// on, whose page is 16 KiB:
    ///
    /// ```text
    /// $ php -n -d memory_limit=128M -r '$b = memory_get_usage(); $rb = memory_get_usage(true);
    ///   $s = str_repeat("x", 3000000); echo memory_get_usage() - $b, " ", memory_get_usage(true) - $rb;'
    /// 3014680 3014656
    /// ```
    ///
    /// 3,000,000 bytes of string is 3,000,032 allocated, the header, the
    /// NUL and the alignment, and 3,014,656 is that rounded to 16 KiB;
    /// rounded to 4 KiB it would be 3,002,368, twelve KiB short. The 24
    /// bytes over it in php's first number are the variable php's symbol
    /// table took, which no allocation here makes.
    ///
    /// The session above is still one a person typed. `memory_get_usage` is a
    /// native of `php_engine` now, and the witness beside it pins the
    /// other half of this accounting per commit: a fresh request's real
    /// usage is php's one 2 MiB chunk, and the oracle says the same.
    /// A huge block is past what a witness can reach while `php_compile`
    /// reads no variable and nothing declares `str_repeat()`, so no
    /// snippet can hold a 3 MB string alive across two calls; #174 is
    /// where this session becomes a witness too.
    #[test]
    fn a_huge_block_rounds_to_the_page_the_machine_reports() {
        let _alone = alone();
        let page = mm::real_page_size();
        // The same page through a second door, so that what this crate
        // asks the machine is held to what the machine says and not to
        // itself: `getconf PAGESIZE` is POSIX's own reader of
        // `_SC_PAGESIZE`, which is the number php reads.
        if let Ok(said) = std::process::Command::new("getconf")
            .arg("PAGESIZE")
            .output()
        {
            let said = String::from_utf8_lossy(&said.stdout)
                .trim()
                .parse::<usize>();
            if let Ok(said) = said {
                assert_eq!(page, said, "the machine reports a page of {said}");
            }
        }
        let mut heap = Heap::new();
        let real = heap.memory_usage(true);
        let allocated = heap.string(&vec![b'x'; 3_000_000]);
        assert!(allocated.is_ok(), "the heap would not allocate 3 MB");
        let Ok(s) = allocated else { return };
        let rounded = 3_000_032usize.next_multiple_of(page);
        assert_eq!(heap.memory_usage(false), rounded);
        assert_eq!(heap.memory_usage(true), real + rounded);
        // php's own numbers, where the machine's page is the oracle's.
        if page == 16 * 1024 {
            assert_eq!(heap.memory_usage(false), 3_014_656);
            assert_eq!(heap.memory_usage(true) - real, 3_014_656);
        }
        heap.release_string(s);
        assert_eq!(heap.memory_usage(false), 0);
        assert_eq!(heap.memory_usage(true), real);
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

    /// An allocation of the large range names the pages php names and
    /// not the bytes asked for: outside `ZEND_DEBUG` php hands
    /// `zend_mm_alloc_pages` the page count alone, so a string of 5000
    /// bytes, 5032 with its header, meets the limit naming the two pages
    /// it was about to take and not the 5032.
    #[test]
    fn the_limit_on_a_large_allocation_names_the_pages_php_names() {
        let _alone = alone();
        let mut heap = Heap::new();
        heap.set_memory_limit(mm::CHUNK_SIZE);
        let mut held = Vec::new();
        let fatal = loop {
            match heap.string(&[b'x'; 5000]) {
                Ok(s) => held.push(s),
                Err(f) => break f,
            }
        };
        // 511 free pages to a chunk, two to an allocation.
        assert_eq!(held.len(), 255);
        assert_eq!(
            fatal,
            Fatal::Error(
                "Allowed memory size of 2097152 bytes exhausted (tried to allocate 8192 bytes)"
                    .to_string()
            )
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
