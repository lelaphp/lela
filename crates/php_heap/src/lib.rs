//! The memory model (design §4.1, §5.1–§5.6, §5.13, §5.14): the zval and
//! its refcounted header, the heap that allocates and accounts for every
//! PHP-visible value, the VM stack with its entry slot, the map table
//! that gives an immutable shared structure its per-request state, and
//! the one upward seam, [`HeapHooks`], through which a refcount reaching
//! zero reaches user code without this crate knowing what a VM is.
//!
//! The crate is the first of the `unsafe` homes the manifest names, and
//! the allocator of §5.13, the string of §5.2 and the array of §5.3 are
//! here in full: a request's memory is chunks, bins and `runs` php's
//! way, the numbers php reports come from that accounting and from
//! nowhere else, an array is `zend_hash` to the byte and an object is
//! `zend_object` in its store, `zend_objects_store`, the destructor
//! served through the seam with the heap handed back for it. The VM
//! stack is `zend_vm_stack`'s pages, taken from the same heap, so a
//! frame is memory the limit bounds as any value is. Nothing here
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

mod array;
mod block;
mod call_stack;
mod collector;
mod compare;
mod fiber;
mod gc;
mod hooks;
mod iterators;
mod lend;
mod map;
mod mm;
mod numeric;
mod object;
mod observe;
#[cfg(debug_assertions)]
pub mod owners;
mod payload;
mod property_table;
mod reference;
mod resource;
#[cfg(debug_assertions)]
mod sensed;
mod sort;
mod stack;
mod string;
mod table_copy;
mod zend_arena;
mod zval;

pub use array::DeclaredRows;
pub use array::{Arr, ArrIter, Key, SortEntry, canonical_int};
pub use block::Block;
pub use call_stack::{CallStack, call_stack_bounds, call_stack_default_size};
pub use collector::GcStatus;
pub use compare::{Scalar, compare, compare_constants, identical, identical_constants};
pub use fiber::{FiberStack, Host, StackRefused, Suspensions, Switched, suspend};
pub use hooks::{Fatal, FatalReason, GcTable, HeapHooks};
pub use lend::{ecalloc, efree, emalloc, erealloc};
pub use map::{MapPtr, MapTable};
pub use numeric::{
    Exponent, Number, double_to_string, numeric_prefix, power, shift_left, shift_right,
};
pub use object::{Laziness, Obj, ObjectId};
pub use observe::{Allocation, AllocationObserverId, ObserveAllocations};
pub use property_table::{Direction, PropertyTable, TableRow};
pub use reference::Ref;
pub use resource::{Res, ResourceType};
#[cfg(debug_assertions)]
pub use sensed::Origin;
pub use sort::zend_sort;
pub use stack::{
    EntryGuard, FakeRoom, Frame, FrameRef, FramesAside, HEADER_AT, ROOM_BYTES, Slot, VmStack,
};
pub use string::{FreshStr, Intern, Str, Strings};
pub use zval::{PropBit, Zval, ZvalRef, ZvalType};

use gc::Counted;
use std::ptr::NonNull;

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
    /// php's `EG(objects_store)`: the table of ids, whose buckets are
    /// the request's memory and whose sweep drops the payloads no free
    /// reached. Declared before `mm` so that it drops first, while the
    /// objects it walks are still mapped.
    store: object::Store,
    /// php's `EG(regular_list)`: the resources by id in one [`Arr`]
    /// whose data is the request's memory, laid down by the first
    /// resource, and whose sweep drops the payloads no close reached.
    /// Declared before `mm` for the same reason the store is.
    resources: resource::RegularList,
    mm: mm::Mm,
    /// php's `CG(arena)`, the compiler's arena: 64 KiB laid down before
    /// anything else of the request's ([`Heap::new`]), carved and never
    /// freed a piece at a time ([`Heap::carve`]).
    arena: zend_arena::ZendArena,
    /// The strings the units this request compiled for itself hold and
    /// the names its declarations are read by, each once and live until
    /// the request ends ([`Heap`]'s [`Intern`]): the request's own memory
    /// and counted as such, in a table of the system allocator's, which
    /// nothing counts ([`string::Table`]).
    interned: string::Table,
    /// php's `EG(ht_iterators)`: the positions the walks of this
    /// request's tables are registered at.
    iterators: iterators::Iterators,
    /// php's `zend_gc_globals`, the cycle collector's root buffer and
    /// numbers, the request's own (design §5.12).
    gc: collector::Collector,
    /// The walks' pins past the first over blocks other than objects
    /// ([`Heap::pin_block`]).
    nested: gc::Nested,
    /// The room [`Heap::destroy`]'s walk keeps its frames in, empty
    /// between walks and kept for the next, so a table's destruction
    /// allocates nothing off the request's heap; a walk inside a
    /// destructor a walk serves finds it taken and lays its own.
    destroying: Vec<array::Emptying>,
    /// The constant arrays of the units this request alone serves, out of
    /// this heap and immutable, so no release frees them: kept in a
    /// debug build for its teardown to free by name before the request
    /// leak sensor, as php's `destroy_op_array` frees a request's literals.
    #[cfg(debug_assertions)]
    constant_arrays: Vec<Arr>,
}

impl Default for Heap {
    fn default() -> Heap {
        Heap::new()
    }
}

impl Heap {
    /// A heap with php's main chunk mapped and laid down in it, in
    /// `zend_activate`'s order, the compiler's arena `init_compiler`
    /// creates and the object store's table `init_executor` allocates,
    /// and nothing else: what a request's heap holds of the engine's own
    /// before the host and the extensions start it.
    #[must_use]
    pub fn new() -> Heap {
        let mut mm = mm::Mm::new();
        let arena = zend_arena::ZendArena::new(&mut mm);
        let store = object::Store::new(&mut mm);
        let mut gc = collector::Collector::new();
        gc.enable(true);
        Heap {
            store,
            resources: resource::RegularList::new(),
            mm,
            arena,
            interned: string::Table::new(),
            iterators: iterators::Iterators::default(),
            gc,
            nested: gc::Nested::default(),
            destroying: Vec::new(),
            #[cfg(debug_assertions)]
            constant_arrays: Vec::new(),
        }
    }

    /// `zend_arena_alloc(&CG(arena), size)`: `size` bytes, rounded up to
    /// eight, carved from the compiler's arena, which takes a block of
    /// the request's heap only where the newest has not the room, and
    /// gives nothing back before the request ends. What php keeps in the
    /// bytes, a runtime cache's slots among them, the engine keeps
    /// typed and elsewhere, so the carve is the room php's takes and
    /// hands out no memory. The `Err` is the memory limit's fatal.
    pub fn carve(&mut self, size: usize) -> Result<(), Fatal> {
        self.arena.carve(&mut self.mm, size)
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

    /// `is_zend_ptr`: whether `address` lies inside one of this heap's
    /// chunks or huge blocks, what php asks before it frees a pointer
    /// it cannot tell the allocator of (`ext/ffi`'s owned data) and what
    /// `zend_test_is_zend_ptr` answers. Only the heap's own lists are
    /// read, so any address is a question it answers, including one no
    /// mapping holds.
    #[must_use]
    pub fn holds(&self, address: usize) -> bool {
        self.mm.holds(address)
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

    /// Hold the request to `limit` bytes, `zend_set_memory_limit`: the
    /// limit is met on chunk growth, with php's exact message as the
    /// [`Fatal`]. A limit under what the chunks hold is `false` unless
    /// giving back cached chunks brings them under it.
    pub fn set_memory_limit(&mut self, limit: usize) -> bool {
        self.mm.set_limit(limit)
    }

    /// `observer` told of every allocation, reallocation and free this
    /// heap serves from here on, and of what `gc_mem_caches()` gives
    /// back, after the observers put in before it ([`ObserveAllocations`],
    /// design §7.1): what stands in the place of php's
    /// `zend_mm_set_custom_handlers`. The heap still serves each one, so
    /// what `memory_get_usage()` reads is what it reads with none.
    pub fn observe_allocations(
        &mut self,
        observer: Box<dyn ObserveAllocations>,
    ) -> AllocationObserverId {
        self.mm.observe(observer)
    }

    /// The observer put in at `at` taken out, told of nothing more and
    /// handed back, `None` where it was taken out already.
    pub fn unobserve_allocations(
        &mut self,
        at: AllocationObserverId,
    ) -> Option<Box<dyn ObserveAllocations>> {
        self.mm.unobserve(at)
    }

    /// `zend_alloc_in_memory_limit_error_reporting`: whether the limit's
    /// fatal is being reported, the window in which the limit refuses
    /// nothing and php discards the buffers `ob_start()` laid down.
    #[must_use]
    pub fn in_memory_limit_error(&self) -> bool {
        self.mm.overflowed()
    }

    /// The fatal the heap raised when it refused an allocation is
    /// rendered: php's `heap->overflow = 0` in `zend_mm_safe_error`. From
    /// that refusal until here the limit refuses nothing, so the
    /// rendering, a backtrace of every frame of a recursion the limit
    /// stopped among it, is not refused by the limit it reports; from
    /// here the limit holds again, for the shutdown functions and the
    /// destructors after.
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// ini_set('fatal_error_backtraces', '1');
    /// function f() { f(); }
    /// f();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 262144 bytes) in %s on line 4
    /// Stack trace:
    /// #0 %s(4): f()
    /// %a
    /// #%d %s(5): f()
    /// #%d {main}
    /// ```
    pub fn overflow_rendered(&mut self) {
        self.mm.overflow_rendered();
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

    /// A request-scoped string of `len` bytes written by `fill`, php's
    /// `zend_string_alloc` and the `memcpy` or `memset` a native does
    /// into `ZSTR_VAL` after it: the allocation is asked for, and the
    /// limit met, before a byte of the string exists anywhere, which is
    /// what lets `str_repeat("x", 200000000)` be refused without two
    /// hundred megabytes being built first. `fill` is given the `len`
    /// bytes, zero, to write once.
    pub fn string_with(&mut self, len: usize, fill: impl FnOnce(&mut [u8])) -> Result<Str, Fatal> {
        let at = self.mm.alloc(Str::allocation_size(len))?;
        // SAFETY: the allocation is exactly the size `init_with` asks for
        // and nothing else holds it.
        Ok(unsafe { Str::init_with(at, len, 0, fill) })
    }

    /// `zend_string_alloc(len, 0)`: a string of `len` zero bytes the
    /// caller writes in place and cuts down before handing it out
    /// ([`FreshStr`]); the limit is met before it exists.
    pub fn string_fresh(&mut self, len: usize) -> Result<FreshStr, Fatal> {
        self.string_with(len, |_| {}).map(FreshStr)
    }

    /// `zend_string_truncate(s, len, 0)`: the fresh string cut to `len`
    /// bytes where it stands or moved to the smaller bin, as `erealloc`
    /// cuts it.
    pub fn string_truncate(&mut self, s: FreshStr, len: usize) -> Result<FreshStr, Fatal> {
        s.truncate(&mut self.mm, len)
    }

    /// `zend_string_extend(s, len, 0)`: the fresh string grown to `len`
    /// bytes where it stands or moved, as `erealloc` grows it, the new
    /// bytes zero.
    pub fn string_extend(&mut self, s: FreshStr, len: usize) -> Result<FreshStr, Fatal> {
        s.extend(&mut self.mm, len)
    }

    /// `s` with the bytes of `tail` after its own, php's
    /// `zend_string_extend` and the `memcpy` after it: the caller hands
    /// one owner of `s` over and holds one of the string handed back.
    /// Where that owner is the only one and `s` is the request's, the
    /// allocation is grown where it stands as `erealloc` grows it, so a
    /// loop of `.=` is linear; an interned or shared `s` is copied into
    /// a new string and left to its other holders as it was. `tail` is
    /// read and keeps its owners, and may be `s`.
    pub fn string_append(&mut self, s: Str, tail: Str) -> Result<Str, Fatal> {
        Str::append(&mut self.mm, s, tail)
    }

    /// `zend_string_safe_alloc(n, m, l, 0)`: a string of `n * m + l`
    /// bytes whose allocation is that product plus the aligned header of
    /// a string of `l` bytes, which is not the allocation of
    /// [`Heap::string_with`] for the same length: `str_repeat("ab", 3)`
    /// takes 38 bytes and the 40-byte bin where a six-byte string takes
    /// 31 and the 32-byte bin, and `memory_get_usage()` says so. A
    /// product that overflows is php's `Possible integer overflow in
    /// memory allocation` fatal (`safe_address_guarded`), raised before
    /// anything is asked for.
    pub fn string_safe_alloc(
        &mut self,
        n: usize,
        m: usize,
        l: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<Str, Fatal> {
        let header = Str::allocation_size(l);
        let Some(size) = n.checked_mul(m).and_then(|bytes| bytes.checked_add(header)) else {
            return Err(Fatal::error(format!(
                "Possible integer overflow in memory allocation ({n} * {m} + {header})"
            )));
        };
        let at = self.mm.alloc(size)?;
        // SAFETY: the allocation holds the header and `n * m + l` bytes
        // with the NUL after them, and nothing else holds it.
        Ok(unsafe { Str::init_with(at, size - header + l, 0, fill) })
    }

    /// How many strings this request's own table holds ([`Heap`]'s
    /// [`Intern`]).
    #[must_use]
    pub fn interned(&self) -> usize {
        self.interned.count()
    }

    /// `zend_string_release`: one owner fewer, and the string is freed
    /// when the last gives it up, its bytes kept while a pin names it
    /// ([`Heap::give_back`]). An interned string is never freed. A
    /// debug build's freed-use guard is the header's decrement, and names
    /// the caller.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release_string(&mut self, s: Str) {
        if s.is_interned() || s.gc().release() != gc::Release::Last {
            return;
        }
        if s.is_persistent() {
            // Outside the request's heap nothing is counted, so a pinned
            // one is freed whole at its pin's let-go.
            if !s.gc().pinned() {
                string::free_persistent(s);
            }
            return;
        }
        // SAFETY: the string came from this heap's `string` and its last
        // owner has given it up, so only a pin names it.
        unsafe { self.give_back(s.address(), s.gc().pinned()) };
    }

    /// The end of `rc_dtor_func` once the block at `at` is destroyed,
    /// php's `efree`: its bytes back to the heap, or, while a pin stands
    /// over it, the free counted now, where php counts it, and the bytes
    /// kept for the last pin's let-go ([`Heap::unpin_block`]), so a
    /// frame or a walk that still names the block reads it destroyed
    /// and never freed (design §4.4, #2230). A block whose free was
    /// counted already, given up again by an owner taken of it since,
    /// has its bytes back with nothing counted twice.
    ///
    /// # Safety
    ///
    /// `at` is a block of this heap whose destruction is done, and
    /// nothing but a pin names it, which `pinned` says.
    pub(crate) unsafe fn give_back(&mut self, at: NonNull<u8>, pinned: bool) {
        if pinned {
            self.mm.retire(at);
        } else if self.mm.is_retired(at) {
            // SAFETY: the caller's contract: no pin and no owner names it.
            unsafe { self.mm.free_retired(at) };
        } else {
            // SAFETY: the caller's contract.
            unsafe { self.mm.free(at) };
        }
    }

    /// What a debug build's sensors found in this heap since the
    /// engine last asked, as the `InternalError` fatal that ends the
    /// request: a refcount operation refused because the block it named
    /// was one this heap had freed, naming the block, the operation and
    /// the line that asked for it, or a write into a freed block found as
    /// it left its bin's quarantine, naming the block. The first only,
    /// and taken, so it is raised once; the refused operation itself did
    /// nothing, and the written block goes on to the free list, so the
    /// free list stands. The engine asks after every
    /// handler and once more at the request's end, so a refusal made
    /// outside the loop, in a request-end function or as the request's
    /// tables are freed, is raised too. A release build senses nothing
    /// and answers `None`.
    pub fn take_sensed(&mut self) -> Option<Fatal> {
        #[cfg(debug_assertions)]
        {
            self.mm.take_sensed()
        }
        #[cfg(not(debug_assertions))]
        {
            None
        }
    }

    /// Whether a fatal is leaving the request: the engine says so where
    /// `zend_bailout` begins and again where its `zend_try` catches it.
    /// While one is, an owner dropped unconsumed is not recorded, as
    /// php's jump leaves a native's locals for the heap to discard; a
    /// refused refcount change still is ([`owners::dropped`]).
    #[cfg(debug_assertions)]
    pub fn fatal_leaving(&self, leaving: bool) {
        self.mm.fatal_leaving(leaving);
    }

    /// php's debug `zend_mm_shutdown` (`Z/Zend/zend_alloc.c:2434`) once
    /// the engine has freed what the request held: the blocks the heap
    /// itself keeps for the request's lifetime freed by name, the
    /// constant arrays of the units this request alone served, the
    /// resource list (`zend_destroy_rsrc_list`), the object store's
    /// buckets (`zend_objects_store_destroy`), the strings the request
    /// interned with their table (`zend_interned_strings_deactivate`)
    /// and the compiler's arena (`shutdown_compiler`), then every block still held named, `zend_mm_check_leaks`. What it
    /// finds is recorded as the sensors record and raised by
    /// [`Heap::take_sensed`]: the blocks never freed, their bytes and the
    /// first of them, with what `name` calls the [`Origin`] that block was
    /// allocated for, and the frames that allocated it where allocations
    /// are traced ([`Heap::trace_allocations`]). The heap serves no
    /// allocation after it but the one that renders that fatal. A release
    /// build has none of it and keeps php's fast teardown, the chunks
    /// given back whole.
    #[cfg(debug_assertions)]
    pub fn sense_leaks(&mut self, name: &dyn Fn(Origin) -> String) {
        for a in std::mem::take(&mut self.constant_arrays) {
            array::free_table_data(&mut self.mm, a);
            // SAFETY: the header came from this heap's `mm`, and the
            // array is immutable, so no owner gave it up before this.
            unsafe { self.mm.free(a.address()) };
        }
        self.resources.destroy(&mut self.mm);
        self.store.destroy(&mut self.mm);
        self.interned.destroy(&mut self.mm);
        self.arena.destroy(&mut self.mm);
        self.mm.sense_leaks(name);
    }

    /// What a debug build allocates from here on is for `origin`, the
    /// extension whose native or lifecycle hook the engine is about to
    /// serve, and the origin it replaces, which the engine puts back
    /// where that code returns ([`Origin`]). A block keeps the origin it
    /// was handed out under, and a leak names it.
    #[cfg(debug_assertions)]
    pub fn set_origin(&mut self, origin: Origin) -> Origin {
        self.mm.set_origin(origin)
    }

    /// The origin a debug build tags what it allocates now with: the
    /// extension whose code the engine is serving ([`Heap::set_origin`]).
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn origin(&self) -> Origin {
        self.mm.origin()
    }

    /// A debug build's opt-in tracing: every allocation from here
    /// on keeps the backtrace that made it, and a leak names the frames
    /// that allocated its first block. Each allocation then pays a
    /// `Backtrace::force_capture`, which costs many times the allocation,
    /// so it is for digging locally and never on by default.
    #[cfg(debug_assertions)]
    pub fn trace_allocations(&mut self) {
        self.mm.trace_allocations();
    }

    /// One more owner of `z`: a refcount increment for a refcounted
    /// value, nothing for a scalar or an immutable value (`ZVAL_COPY`).
    /// A debug build's freed-use guard is the header's, which this
    /// reaches as each type's own addref does, and names the caller.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(&self, z: Zval) {
        if !z.is_refcounted() {
            return;
        }
        if let Some(s) = z.as_str() {
            s.addref();
        } else if let Some(a) = z.as_arr() {
            a.addref();
        } else if let Some(o) = z.as_obj() {
            o.addref();
        } else if let Some(r) = z.as_resource() {
            r.addref();
        } else if let Some(r) = z.as_reference() {
            r.addref();
        }
    }

    /// One owner fewer, `zval_ptr_dtor`: a refcount decrement, and when
    /// it reaches zero the value is freed, an array's elements released
    /// with it, an object's destructor served through `hooks` (seam 1),
    /// a resource's per-type destructor the same way and a reference's
    /// value released before its cell. A decrement that leaves owners is
    /// `gc_check_possible_root` (design §5.12): an array or an object,
    /// or what a reference holds, may be the last outside owner of a
    /// cycle, and goes in the root buffer. The hooks are what says this
    /// may re-enter, and the heap is theirs: it is asked for at each
    /// step and again after every call up, since the destructor may have
    /// grown it.
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release(hooks: &mut dyn HeapHooks, z: Zval) {
        if !z.is_refcounted() {
            return;
        }
        match Heap::give_up(hooks, z) {
            GivenUp::Owned => Heap::gc_check_possible_root(hooks, z),
            GivenUp::Freed => {}
            GivenUp::Last(last) => Heap::destroy(hooks, last),
        }
    }

    /// One owner fewer as `zval_ptr_dtor_nogc` gives it up: what
    /// [`Heap::release`] does, and never a possible root. php frees a
    /// temporary operand this way (`FREE_OP`), and an internal call's
    /// arguments, since neither is a place a cycle is left behind in.
    /// Whether owners are left.
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release_nogc(hooks: &mut dyn HeapHooks, z: Zval) -> bool {
        if !z.is_refcounted() {
            return false;
        }
        match Heap::give_up(hooks, z) {
            GivenUp::Owned => true,
            GivenUp::Freed => false,
            GivenUp::Last(last) => {
                Heap::destroy(hooks, last);
                false
            }
        }
    }

    /// `GC_DELREF` and `gc_check_possible_root`: one owner of `z` given
    /// up where the caller holds that another stands, so the value is
    /// offered to the collector as a possible root and never destroyed
    /// here, as php gives up the collector's fiber running the code that
    /// gives it up (`Z/Zend/zend_gc.c:2343`). Whether an owner was given
    /// up: not the last, which is the caller's broken invariant and is
    /// left standing, so nothing is freed under what names it.
    #[must_use = "the last owner kept is the caller's InternalError"]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release_held(hooks: &mut dyn HeapHooks, z: Zval) -> bool {
        if counted_header(z).is_some_and(|header| header.refcount() <= 1) {
            return false;
        }
        Heap::release(hooks, z);
        true
    }

    /// Whether giving up one owner of `z` serves nothing past the
    /// decrement: another owner stands, and the value is in the
    /// collector's buffer already or can be no possible root
    /// (`!GC_MAY_LEAK`), so `gc_check_possible_root` takes nothing and no
    /// collection follows ([`Heap::release`]). What a frame's leave asks
    /// of the receiver it owns before giving it up with no PHP code
    /// served, as `OBJ_RELEASE` in `zend_leave_helper` serves none for
    /// such an object.
    #[must_use]
    pub fn releases_quietly(z: Zval) -> bool {
        counted_header(z).is_some_and(|header| header.refcount() > 1 && !header.may_leak())
    }

    /// One owner of `z` given up where [`Heap::releases_quietly`] holds,
    /// `GC_DELREF` and nothing after it, since `gc_check_possible_root`
    /// would take nothing; whether it held. Where it does not, nothing
    /// is touched and the owner is the caller's to give up through
    /// [`Heap::release`]. What a frame's leave does with the receiver it
    /// owns once it has asked.
    #[must_use]
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release_quietly(z: Zval) -> bool {
        let Some(header) =
            counted_header(z).filter(|header| header.refcount() > 1 && !header.may_leak())
        else {
            return false;
        };
        header.delref();
        true
    }

    /// `i_zval_ptr_dtor` up to `rc_dtor_func`: the refcount decrement,
    /// and at zero a string, an object or a resource freed here, while a
    /// table or a reference cell is handed back undestroyed, since what
    /// it holds may be the last owner of another and [`Heap::destroy`]
    /// takes them apart without recursing.
    #[cfg_attr(debug_assertions, track_caller)]
    fn give_up(hooks: &mut dyn HeapHooks, z: Zval) -> GivenUp {
        if !z.is_refcounted() {
            return GivenUp::Freed;
        }
        // A release does more than the decrement: a count left over
        // makes the value a possible root and the last one destroys it,
        // and both read and write the header the free list's link is
        // written over. So the question the header's decrement asks is
        // asked first here, and a refused release does nothing at all.
        #[cfg(debug_assertions)]
        if let Some(at) = z.counted()
            && gc::refuses_freed(at, sensed::Use::Release, std::panic::Location::caller())
        {
            return GivenUp::Freed;
        }
        if let Some(s) = z.as_str() {
            hooks.heap().release_string(s);
            return GivenUp::Freed;
        }
        // At zero the block is destroyed whether or not a pin stands:
        // a pin keeps only its bytes ([`Heap::give_back`]).
        match counted_header(z).map(Counted::release) {
            Some(gc::Release::Owned) => GivenUp::Owned,
            Some(gc::Release::Last) => {
                if let Some(a) = z.as_arr() {
                    GivenUp::Last(Last::Table(a))
                } else if let Some(r) = z.as_reference() {
                    GivenUp::Last(Last::Cell(r))
                } else {
                    if let Some(o) = z.as_obj() {
                        Heap::store_del(hooks, o);
                    } else if let Some(r) = z.as_resource() {
                        Heap::resource_free(hooks, r);
                    }
                    GivenUp::Freed
                }
            }
            Some(gc::Release::Ownerless) | None => GivenUp::Freed,
        }
    }

    /// A pin over the block `z` names (design §4.4, #2197, #2230): an
    /// uncounted guard over the block's bytes. Where the block's last
    /// owner goes while the pin stands, it is destroyed then, as php
    /// destroys it, its destructor served, its rows or members released,
    /// an object's id freed and its bytes counted off
    /// `memory_get_usage()`, and only the bytes wait for
    /// [`Heap::unpin_block`], so what names the block reads it destroyed
    /// and never freed. The count is untouched, so every count php reads
    /// and every separation it makes stand as php's. The pins are
    /// counted, an object's in its header ([`Obj::pin`]), the pin a frame
    /// takes of its receiver among them, and any other block's by the
    /// header's bit and the heap's count of the pins past the first, so
    /// the pins over one block are let go in any order, as a walk's step
    /// suspended in a fiber and a walk's step outside it let go theirs
    /// (#2291), and the last let go gives the bytes back. A value that
    /// carries no count takes no pin.
    ///
    /// A row the program frees while a walk reads it is destroyed at the
    /// `unset`, as php destroys it, whether it is an object or a table,
    /// and php's ids and counted memory follow:
    ///
    /// ```php
    /// class X { public $c; function __destruct() { echo "X gone\n"; } }
    /// class C { function __serialize(): array { unset($GLOBALS['r'][0]); echo "after unset\n"; return []; } }
    /// $x = new X; $x->c = new C; $r = [$x, 1]; unset($x); $o = [&$r];
    /// echo serialize($o), "\n";
    /// ```
    /// ```expect
    /// X gone
    /// after unset
    /// a:1:{i:0;a:2:{i:0;O:1:"X":1:{s:1:"c";O:1:"C":0:{}}i:1;i:1;}}
    /// ```
    ///
    /// ```php
    /// class D { function __destruct() { echo "D gone\n"; } }
    /// class C { function __serialize(): array { unset($GLOBALS['r'][0]); echo "after unset\n"; return []; } }
    /// $r = [[new D, new C], 1]; $o = [&$r];
    /// echo serialize($o), "\n";
    /// ```
    /// ```expect
    /// D gone
    /// after unset
    /// a:1:{i:0;a:2:{i:0;a:2:{i:0;O:1:"D":0:{}i:1;O:1:"C":0:{}}i:1;i:1;}}
    /// ```
    ///
    /// ```php
    /// class X { public $c; function __destruct() { echo "X gone\n"; } }
    /// class C { function __serialize(): array { $m0 = memory_get_usage(); unset($GLOBALS['r'][0]); echo "after unset\n"; $n = new stdClass; echo "id ", spl_object_id($n), " freed-mem ", $m0 - memory_get_usage(), "\n"; return []; } }
    /// $x = new X; echo "x id ", spl_object_id($x), "\n"; $x->c = new C; $r = [$x, 1]; unset($x); $o = [&$r];
    /// echo serialize($o), "\n";
    /// ```
    /// ```expect
    /// x id 1
    /// X gone
    /// after unset
    /// id 1 freed-mem 16
    /// a:1:{i:0;a:2:{i:0;O:1:"X":1:{s:1:"c";O:1:"C":0:{}}i:1;i:1;}}
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn pin_block(&mut self, z: Zval) {
        if let Some(o) = z.as_obj() {
            o.pin();
        } else if let Some(header) = counted_header(z)
            && header.pin()
        {
            self.nested.add(header.block());
        }
    }

    /// One pin [`Heap::pin_block`] took over `z` let go: where it was the
    /// last, the block was destroyed while the pins stood, and no owner
    /// was taken of it since, its bytes go back now, nothing else, since
    /// its destruction was served at php's moment.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn unpin_block(&mut self, z: Zval) {
        if let Some(o) = z.as_obj() {
            self.unpin(o);
            return;
        }
        let Some(header) = counted_header(z) else {
            return;
        };
        if self.nested.let_go(header.block()) || header.unpin() != 0 {
            return;
        }
        if let Some(s) = z.as_str()
            && s.is_persistent()
        {
            string::free_persistent(s);
        } else if z.as_resource().is_some_and(|r| !r.is_destroyed()) {
            // A resource its owners took off the list has its bytes
            // retired while it lives ([`Heap::resource_unlist`]): they go
            // back with its last owner, not with a pin.
        } else {
            // SAFETY: where it is retired, the block's last owner went
            // while the pins stood, its destruction is done, and the last
            // pin was the last thing naming it.
            unsafe { self.mm.free_retired(header.block()) };
        }
    }
}

/// The refcounted header of the block `z` names, `Z_COUNTED`; none for
/// a value that carries no count.
fn counted_header(z: Zval) -> Option<Counted> {
    if !z.is_refcounted() {
        return None;
    }
    if let Some(s) = z.as_str() {
        (!s.is_interned()).then(|| s.gc())
    } else if let Some(a) = z.as_arr() {
        Some(a.gc())
    } else if let Some(o) = z.as_obj() {
        Some(o.gc())
    } else if let Some(r) = z.as_resource() {
        Some(r.gc())
    } else {
        z.as_reference().map(Ref::gc)
    }
}

/// What one owner fewer left: owners still, nothing to free or freed
/// already, or a table or a reference cell whose last owner it was.
enum GivenUp {
    Owned,
    Freed,
    Last(Last),
}

/// A table or a reference cell no owner is left of, whose contents are
/// still to be given up: what php's `rc_dtor_func` recurses into
/// (`zend_array_destroy`, `zend_reference_destroy`).
#[derive(Clone, Copy)]
pub(crate) enum Last {
    Table(Arr),
    Cell(Ref),
}

impl Intern for Heap {
    /// The home of the strings a declaration of this request holds: the
    /// constants and names of a unit compiled for this request alone, an
    /// `eval` or an uncached include, as its compile's
    /// `zend_new_interned_string` keeps them in its op arrays and class
    /// entries, and a name read off a declaration that its unit made no
    /// string for. The frozen table, then this request's own, then a
    /// string out of this request's memory at the size `zend_string_init`
    /// takes, which the request's end takes with everything else. It is
    /// flagged interned and never persistent, so no release frees it and
    /// no thread but this one can name it, and the table that finds it
    /// again costs nothing ([`string::Table`]).
    ///
    /// It is not php's `zend_string_init_interned`: under opcache that is
    /// `accel_init_interned_string_for_php`, a string of its caller's own
    /// where opcache's buffer has none, which a request makes with
    /// [`Heap::string`] and never finds again.
    fn intern_marked(&mut self, bytes: &[u8], valid_utf8: bool) -> Result<Str, Fatal> {
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
        if valid_utf8 {
            s.mark_valid_utf8();
        }
        self.interned.add(hash, s);
        Ok(s)
    }

    /// A constant array of a unit this request alone serves, an `eval`
    /// or an uncached include: out of this heap and gone with it, as
    /// its interned strings are, immutable as the published one is so
    /// that the engine reads one kind of constant array.
    fn constant_array(&mut self, entries: &[(Key, Zval)]) -> Result<Arr, Fatal> {
        let a = self.array_constant(entries)?;
        #[cfg(debug_assertions)]
        if !entries.is_empty() {
            self.constant_arrays.push(a);
        }
        Ok(a)
    }
}

/// What a fresh heap holds: the compiler's arena and the object
/// store's table ([`Heap::new`]).
#[cfg(test)]
pub(crate) const STARTED: usize = zend_arena::FIRST_BLOCK + object::INIT_BYTES;

// The mappings the allocator's tests count are the process's, and the
// test runner `runs` its tests beside each other, in every file at once,
// so a test that maps a chunk takes this first: one guard for the crate,
// since a guard per file holds nothing against the others.
#[cfg(test)]
pub(crate) mod serial {
    pub(crate) fn alone() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        match SERIAL.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use super::*;

    use crate::serial::alone;

    /// A request that allocates and frees reads back where it started,
    /// and every size lands in the bin, `run` or huge block php's
    /// arithmetic puts it in.
    #[test]
    fn what_a_request_allocates_and_frees_reads_back_where_it_started() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        let real = heap.memory_usage(true);
        // The compiler's arena and the object store's table, and nothing
        // else, before a line runs.
        assert_eq!(start, STARTED);
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
    /// counts them as php under opcache counts the plain strings it hands
    /// out in their place, and the second request reads what the first
    /// read before it, because the first request's end took every one of
    /// them.
    ///
    /// **Every number here is php's own**, the bin arithmetic of design
    /// §5.13 over php's sizes: 40 bytes for a string of eight, 32 for one
    /// of four, and nothing for the table that finds them again, over
    /// what every request starts with.
    #[test]
    fn what_a_request_interned_is_its_own_and_the_next_request_starts_where_it_did() {
        use std::sync::atomic::Ordering::Relaxed;
        let _alone = alone();
        let mappings = mm::MAPPINGS.load(Relaxed);
        let mut started_at = Vec::new();
        for _ in 0..2 {
            let mut heap = Heap::new();
            started_at.push(heap.memory_usage(false) - STARTED);
            let heap_start = STARTED;
            // The frozen table answers first and allocates nothing.
            let frozen = heap.intern(b"");
            assert!(matches!(frozen, Ok(s) if s.is_empty()));
            assert_eq!(heap.memory_usage(false), heap_start);
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
            // and nothing beside it.
            assert_eq!(heap.memory_usage(false), heap_start + 40);
            assert!(first.is_interned());
            assert!(
                !first.is_persistent(),
                "a request's interned string is out of the request's heap"
            );
            // A release never frees an interned string, as php's does not;
            // the request's end is what takes it.
            heap.release_string(first);
            assert_eq!(heap.memory_usage(false), heap_start + 40);
            // A string the request made and gave up goes back at once.
            let made = heap.string(b"made");
            assert!(made.is_ok());
            if let Ok(made) = made {
                assert_eq!(heap.memory_usage(false), heap_start + 72);
                heap.release_string(made);
            }
            assert_eq!(heap.memory_usage(false), heap_start + 40);
        }
        assert_eq!(started_at, vec![0, 0], "every request starts at zero");
        assert_eq!(
            mm::MAPPINGS.load(Relaxed),
            mappings,
            "and leaves no mapping behind"
        );
    }

    /// The table's own storage is the system's (design §5.13, #2863):
    /// under opcache php lays no `CG(interned_strings)` down on the
    /// request's heap, so a request that interns 1025 strings pays their
    /// bins and nothing else, where `php -n` pays a 40960-byte bucket
    /// array on the first and doubles it on the 1025th. The frozen table
    /// answers before this one and holds no string of the request's.
    #[test]
    fn the_interned_table_s_storage_is_off_the_request_s_heap() {
        let _alone = alone();
        let mut heap = Heap::new();
        let start = heap.memory_usage(false);
        assert!(matches!(heap.intern(b""), Ok(s) if s.is_empty()));
        assert!(matches!(heap.intern(b"a"), Ok(s) if s.len() == 1));
        assert_eq!(heap.memory_usage(false), start);
        for i in 0..1025u32 {
            let interned = heap.intern(format!("s{i}").as_bytes());
            assert!(interned.is_ok(), "the heap would not intern s{i}");
        }
        assert_eq!(heap.interned(), 1025);
        assert_eq!(heap.memory_usage(false), start + 1025 * 32);
        assert_eq!(heap.memory_peak(), start + 1025 * 32);
    }

    /// A string past 2 MiB is a huge block, and php rounds one to the
    /// page the machine reports, `REAL_PAGE_SIZE`
    /// (`Z/Zend/zend_alloc.c:1925`), not to the 4 KiB page its chunks are
    /// carved into. What php answers is pinned by the second witness
    /// beside `memory_get_usage` in `crates/php_engine/src/natives.rs`,
    /// which builds a 3 MB string with `str_repeat` and reads what
    /// `memory_get_usage()` and `memory_get_usage(true)` moved by, per
    /// commit, against the oracle: 3,014,680 and 3,014,656 on the
    /// machine the oracle is pinned on, whose page is 16 KiB. This test
    /// holds the same two numbers at the heap's own face, with the page
    /// asked of the machine rather than assumed.
    ///
    /// 3,000,000 bytes of string is 3,000,032 allocated, the header, the
    /// NUL and the alignment, and 3,014,656 is that rounded to 16 KiB;
    /// rounded to 4 KiB it would be 3,002,368, twelve KiB short. The 24
    /// bytes over it in php's first number are the block's
    /// `zend_mm_huge_list` node, which `zend_mm_add_huge_block`
    /// (`Z/Zend/zend_alloc.c:1839`) takes from the 24-byte bin of the
    /// same heap before the block's own bytes are counted; the snippet's
    /// variables are on the frame before its first line runs and move
    /// nothing. The peak is the node and then the block, so it reads the
    /// same sum, and freeing the block gives the node back with it.
    #[test]
    fn a_huge_block_rounds_to_the_page_the_machine_reports() {
        let _alone = alone();
        let page = mm::real_page_size();
        // The same page through a second door, so that what this crate
        // asks the machine is held to what the machine says and not to
        // itself: `getconf PAGESIZE` is POSIX's own reader of
        // `_SC_PAGESIZE`, which is the number php reads. Miri starts no
        // process, and the page it answers is its own.
        if !cfg!(miri)
            && let Ok(said) = std::process::Command::new("getconf")
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
        let start = heap.memory_usage(false);
        let real = heap.memory_usage(true);
        let allocated = heap.string(&vec![b'x'; 3_000_000]);
        assert!(allocated.is_ok(), "the heap would not allocate 3 MB");
        let Ok(s) = allocated else { return };
        let rounded = 3_000_032usize.next_multiple_of(page);
        let node = 24;
        assert_eq!(heap.memory_usage(false) - start, rounded + node);
        assert_eq!(heap.memory_peak() - start, rounded + node);
        assert_eq!(heap.memory_usage(true), real + rounded);
        // php's own numbers, where the machine's page is the oracle's.
        if page == 16 * 1024 {
            assert_eq!(heap.memory_usage(false) - start, 3_014_680);
            assert_eq!(heap.memory_usage(true) - real, 3_014_656);
        }
        heap.release_string(s);
        assert_eq!(heap.memory_usage(false), start);
        assert_eq!(heap.memory_usage(true), real);
    }

    /// The bin accounting is php's: a string of four bytes takes the
    /// thirty-two byte bin, header, NUL and alignment included.
    #[test]
    fn a_string_takes_the_bytes_php_s_takes() {
        let _alone = alone();
        let mut nothing = Recorder { heap: Heap::new() };
        let start = nothing.heap.memory_usage(false);
        let allocated = nothing.heap.string(b"demo");
        assert!(allocated.is_ok(), "the heap would not allocate four bytes");
        let Ok(s) = allocated else { return };
        assert_eq!(nothing.heap.memory_usage(false) - start, 32);
        let z = Zval::string(s);
        assert!(z.is_refcounted());
        nothing.heap.addref(z);
        assert_eq!(s.refcount(), 2);
        Heap::release(&mut nothing, z);
        assert_eq!(s.refcount(), 1);
        Heap::release(&mut nothing, z);
        assert_eq!(nothing.heap.memory_usage(false), start);
    }

    /// `zend_string_extend` over a string of one owner grows it where it
    /// stands while the bin holds it, and over a shared one copies it and
    /// leaves the other holder's bytes as they were.
    #[test]
    fn an_append_extends_the_one_owner_s_string_and_copies_a_shared_one() {
        let _alone = alone();
        let mut nothing = Recorder { heap: Heap::new() };
        let pair = (nothing.heap.string(b"abc"), nothing.heap.string(b"de"));
        assert!(
            pair.0.is_ok() && pair.1.is_ok(),
            "the heap would not allocate five bytes"
        );
        let (Ok(s), Ok(tail)) = pair else { return };
        let start = nothing.heap.memory_usage(false);
        // 24 + 3 + 1 and 24 + 5 + 1 both round into the 32-byte bin.
        let grown = nothing.heap.string_append(s, tail);
        assert!(grown.is_ok(), "the append was refused");
        let Ok(grown) = grown else { return };
        assert!(
            grown.is_same(s),
            "a string with one owner moved within its bin"
        );
        assert_eq!(grown.bytes(), b"abcde");
        assert_eq!(nothing.heap.memory_usage(false), start);
        // Ten bytes are the 40-byte bin: the slot moves up, once.
        let twice = nothing.heap.string_append(grown, grown);
        assert!(twice.is_ok(), "the append was refused");
        let Ok(twice) = twice else { return };
        assert_eq!(twice.bytes(), b"abcdeabcde");
        assert_eq!(nothing.heap.memory_usage(false), start - 32 + 40);
        twice.addref();
        let copy = nothing.heap.string_append(twice, tail);
        assert!(copy.is_ok(), "the append was refused");
        let Ok(copy) = copy else { return };
        assert!(!copy.is_same(twice), "a shared string was written into");
        assert_eq!(twice.bytes(), b"abcdeabcde");
        assert_eq!(twice.refcount(), 1);
        assert_eq!(copy.bytes(), b"abcdeabcdede");
        for z in [twice, copy, tail] {
            Heap::release(&mut nothing, Zval::string(z));
        }
        assert_eq!(nothing.heap.memory_usage(false), start - 32 - 32);
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
            Some(Fatal::error(
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
        // 511 free pages to a chunk, sixteen of them the compiler's
        // arena and two the object store's table, two to an allocation.
        assert_eq!(held.len(), 246);
        assert_eq!(
            fatal,
            Fatal::error(
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
        // `run` of three pages, so two chunks, less the sixteen pages of
        // the compiler's arena and the two of the object store's table,
        // hold 334 `runs` and the next one meets the limit.
        let mut held = Vec::new();
        let fatal = loop {
            match heap.string(&[b'x'; 3000]) {
                Ok(s) => held.push(s),
                Err(f) => break f,
            }
        };
        assert_eq!(held.len(), 334 * 4);
        assert_eq!(heap.memory_usage(true), 2 * mm::CHUNK_SIZE);
        assert_eq!(
            fatal,
            Fatal::error(
                "Allowed memory size of 4194304 bytes exhausted (tried to allocate 12288 bytes)"
                    .to_string()
            )
        );
        // The fatal is rendered, as the engine says after each, so the
        // limit holds for the next ask.
        heap.overflow_rendered();
        // A 1760-byte string is the 1792-byte bin, a `run` of seven
        // pages, which neither chunk has free. Nothing is on a free
        // list, so the sweep collects nothing and the limit stands.
        assert_eq!(
            heap.string(&[b'x'; 1760]).err(),
            Some(Fatal::error(
                "Allowed memory size of 4194304 bytes exhausted (tried to allocate 28672 bytes)"
                    .to_string()
            ))
        );
        heap.overflow_rendered();
        // Every slot back on its bin's free list. The pages are still
        // the request's until the sweep walks them, so the same
        // allocation still finds no seven free pages anywhere.
        for s in held.drain(..) {
            heap.release_string(s);
        }
        assert_eq!(heap.memory_usage(false), STARTED);
        assert_eq!(heap.memory_usage(true), 2 * mm::CHUNK_SIZE);
        let swept = heap.string(&[b'x'; 1760]);
        assert!(swept.is_ok(), "the sweep did not give the free runs back");
        // The 1002 pages of the 334 `runs`, and the chunk the sweep
        // emptied kept against the next growth, as php keeps one.
        assert_eq!(heap.memory_usage(true), 2 * mm::CHUNK_SIZE);
        assert_eq!(heap.memory_usage(false), STARTED + 1792);
        if let Ok(s) = swept {
            heap.release_string(s);
        }
    }

    /// A size the page rounding carries past the top of memory is php's
    /// `E_ERROR`, with the size and the page in its words, before the
    /// limit is asked (`zend_mm_alloc_huge`, `Z/Zend/zend_alloc.c:1929`),
    /// and the heap is left as it was.
    #[test]
    fn a_size_the_rounding_overflows_is_php_s_integer_overflow() {
        let _alone = alone();
        let mut heap = Heap::new();
        let before = heap.memory_usage(false);
        let page = mm::real_page_size();
        let refused = heap.block(usize::MAX).err();
        let said = format!(
            "Possible integer overflow in memory allocation ({} + {page})",
            usize::MAX
        );
        assert!(
            matches!(refused.map(Fatal::into_reason), Some(FatalReason::Error(text)) if text == said)
        );
        assert_eq!(heap.memory_usage(false), before);
        let small = heap.block(16);
        assert!(small.is_ok(), "the heap still allocates");
        if let Ok(small) = small {
            heap.free_block(small);
        }
    }

    /// `is_zend_ptr` answers for any address inside a chunk or a huge
    /// block, block or not, and no for addresses no list holds, the null
    /// one and the top of memory among them, without reading them
    /// (`Z/Zend/tests/gh14626.phpt`). A heap collected twice before it
    /// has given anything out keeps its main chunk, which GH-18756's
    /// `zend_mm_gc` deleted (`Z/Zend/tests/gh18756.phpt`).
    #[test]
    fn the_heap_holds_its_chunks_and_huge_blocks_and_nothing_else() {
        let _alone = alone();
        let mut heap = Heap::new();
        heap.gc_mem_caches();
        heap.gc_mem_caches();
        let (small, huge) = (heap.block(100), heap.block(3_000_000));
        assert!(small.is_ok() && huge.is_ok(), "the heap would not allocate");
        let (Ok(small), Ok(huge)) = (small, huge) else {
            return;
        };
        let small_at = small.bytes().as_ptr().addr();
        let huge_at = huge.bytes().as_ptr().addr();
        let huge_end = huge_at + 3_000_000usize.next_multiple_of(mm::real_page_size());
        assert!(heap.holds(small_at) && heap.holds(small_at + 1));
        assert!(heap.holds(small_at & !(mm::CHUNK_SIZE - 1)));
        assert!(heap.holds(huge_at) && heap.holds(huge_end - 1));
        assert!(!heap.holds(huge_end));
        assert!(!heap.holds(0) && !heap.holds(usize::MAX));
        heap.free_block(huge);
        assert!(!heap.holds(huge_at));
        heap.free_block(small);
        assert!(heap.holds(small_at), "the main chunk stays the heap's");
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
            assert_eq!(heap.memory_usage(false), STARTED);
        }
        assert_eq!(mm::MAPPINGS.load(Relaxed), before);
    }

    /// An owner dropped unconsumed is recorded through its block's chunk,
    /// with the line that made it, and raised once where the heap is next
    /// asked; where another owner of the block stands, its line is named
    /// beside it. An owner consumed leaves nothing in the table, and one
    /// of what no count owns is recorded nowhere (#2246).
    #[cfg(debug_assertions)]
    #[test]
    fn an_owner_dropped_unconsumed_is_recorded_with_the_line_that_made_it() {
        let _alone = alone();
        let mut heap = Heap::new();
        let made = heap.string(b"a string of its own block");
        assert!(made.is_ok(), "the heap would not allocate");
        let Ok(s) = made else {
            return;
        };
        let z = Zval::string(s);
        let at = s.gc().block();
        let first = std::panic::Location::caller();
        let second = std::panic::Location::caller();
        owners::made(z, first);
        owners::made(z, second);
        assert_eq!(owners::standing(at), 2);
        assert_eq!(owners::consumed(z), Some(second), "the latest is taken off");
        owners::made(z, second);
        owners::dropped(z);
        assert_eq!(
            heap.take_sensed(),
            Some(Fatal::error(format!(
                "InternalError: the owner made at {second} was dropped without being handed to a sink or given up, or one of the other owners of its block, made at {first}"
            )))
        );
        assert_eq!(heap.take_sensed(), None, "it is reported once");
        assert_eq!(owners::consumed(z), Some(first));
        assert_eq!(owners::standing(at), 0, "a consumed owner leaves nothing");
        owners::made(z, first);
        owners::dropped(z);
        assert_eq!(
            heap.take_sensed(),
            Some(Fatal::error(format!(
                "InternalError: the owner made at {first} was dropped without being handed to a sink or given up"
            )))
        );
        owners::made(Zval::long(7), first);
        owners::dropped(Zval::long(7));
        assert_eq!(heap.take_sensed(), None, "an integer leaks nothing");
    }

    /// While a fatal leaves the request, an owner dropped unconsumed is
    /// not recorded, and a refused refcount change still is; once the
    /// fatal is caught, the owner is a defect again (#2048).
    #[cfg(debug_assertions)]
    #[test]
    fn an_owner_dropped_while_a_fatal_leaves_is_not_recorded() {
        let _alone = alone();
        let mut hooks = Recorder { heap: Heap::new() };
        let site = std::panic::Location::caller();
        let held = hooks.heap.string(b"a string an owner holds");
        assert!(held.is_ok(), "the heap would not allocate");
        let Ok(held) = held else {
            return;
        };
        let owned = Zval::string(held);
        hooks.heap.fatal_leaving(true);
        owners::made(owned, site);
        owners::dropped(owned);
        assert_eq!(hooks.heap.take_sensed(), None);
        let made = hooks.heap.string(b"a string of its own block");
        assert!(made.is_ok(), "the heap would not allocate");
        let Ok(s) = made else {
            return;
        };
        let z = Zval::string(s);
        Heap::release(&mut hooks, z);
        Heap::release(&mut hooks, z);
        assert!(matches!(
            hooks.heap.take_sensed().map(Fatal::into_reason),
            Some(FatalReason::Error(message)) if message.starts_with("InternalError: a release of the block at ")
        ));
        hooks.heap.fatal_leaving(false);
        owners::made(owned, site);
        owners::dropped(owned);
        assert!(hooks.heap.take_sensed().is_some());
    }

    /// A debug build refuses a read of a freed string's bytes: the slot
    /// its bin's quarantine holds carries the freed-block mark over the
    /// header's hash and length, so the read answers no bytes rather
    /// than a slice of the mark's length, and is recorded as the
    /// `InternalError` naming the block and the line that read (#2051).
    /// Its length reads as zero the same way.
    #[cfg(debug_assertions)]
    #[test]
    fn a_read_of_a_freed_strings_bytes_is_refused_and_named() {
        let _alone = alone();
        let mut hooks = Recorder { heap: Heap::new() };
        let made = hooks.heap.string(b"a string of its own block");
        assert!(made.is_ok(), "the heap would not allocate");
        let Ok(s) = made else {
            return;
        };
        Heap::release(&mut hooks, Zval::string(s));
        assert_eq!(hooks.heap.take_sensed(), None);
        let read = s.bytes();
        let site = line!() - 1;
        assert_eq!(read, b"");
        let at = s.address().as_ptr().addr();
        assert_eq!(
            hooks.heap.take_sensed(),
            Some(Fatal::error(format!(
                "InternalError: a read of the block at {at:#x}, which the heap has freed, at {}:{site}:22",
                file!()
            )))
        );
        assert_eq!(s.len(), 0);
        assert!(hooks.heap.take_sensed().is_some(), "the length's read too");
    }

    /// A debug build stops at a second release of a block, #723's
    /// defect planted: the first release frees the string, the second
    /// finds the block the heap's own and is refused, recorded as the
    /// `InternalError` naming the block, the operation and this line.
    /// The refused release wrote nothing, so the block leaves the
    /// quarantine as it entered it when the heap's GC drains it, and
    /// nothing more is recorded. An addref of a freed block is refused
    /// the same way, and so is the string's own release.
    #[cfg(debug_assertions)]
    #[test]
    fn a_release_of_a_freed_block_is_refused_and_named_where_it_was_made() {
        let _alone = alone();
        let mut hooks = Recorder { heap: Heap::new() };
        let start = hooks.heap.memory_usage(false);
        let made = hooks.heap.string(b"a string of its own block");
        assert!(made.is_ok(), "the heap would not allocate");
        let Ok(s) = made else {
            return;
        };
        let z = Zval::string(s);
        Heap::release(&mut hooks, z);
        assert_eq!(hooks.heap.memory_usage(false), start);
        assert_eq!(hooks.heap.take_sensed(), None);

        Heap::release(&mut hooks, z);
        let site = line!() - 1;
        let at = s.address().as_ptr().addr();
        assert_eq!(
            hooks.heap.take_sensed(),
            Some(Fatal::error(format!(
                "InternalError: a release of the block at {at:#x}, which the heap has freed, at {}:{site}:9",
                file!()
            )))
        );
        assert_eq!(hooks.heap.take_sensed(), None, "it is reported once");
        assert_eq!(hooks.heap.memory_usage(false), start);

        hooks.heap.addref(z);
        assert!(matches!(
            hooks.heap.take_sensed().map(Fatal::into_reason),
            Some(FatalReason::Error(message)) if message.starts_with("InternalError: an addref of the block at ")
        ));

        // The string's own release, which decrements through the type
        // and not through `Heap::release`, is refused at the header the
        // same way.
        hooks.heap.release_string(s);
        let site = line!() - 1;
        assert_eq!(
            hooks.heap.take_sensed(),
            Some(Fatal::error(format!(
                "InternalError: a release of the block at {at:#x}, which the heap has freed, at {}:{site}:20",
                file!()
            )))
        );
        hooks.heap.gc_mem_caches();
        assert_eq!(
            hooks.heap.take_sensed(),
            None,
            "a refused operation wrote into the block"
        );
        assert_eq!(hooks.heap.memory_usage(false), start);
    }

    /// The bin of a block just freed hands out another slot, and the
    /// block keeps its mark until its bin's quarantine is full: a
    /// value used after its block is given back and one of its size made
    /// is refused the first time it happens, where a free list that handed the
    /// block straight back would have cleared the mark and counted the
    /// new value's owner one more. A slot freed into a full quarantine
    /// evicts the first, which its bin then hands out again.
    #[cfg(debug_assertions)]
    #[test]
    fn the_most_recently_freed_slot_of_a_bin_is_held_back_and_its_use_refused() {
        let _alone = alone();
        let mut hooks = Recorder { heap: Heap::new() };
        let made = hooks.heap.string(b"a string of its own block");
        assert!(made.is_ok(), "the heap would not allocate");
        let Ok(stale) = made else {
            return;
        };
        let at = stale.address().as_ptr().addr();
        hooks.heap.release_string(stale);
        let fresh = hooks.heap.string(b"a string of its own block");
        assert!(
            fresh
                .as_ref()
                .is_ok_and(|s| s.address().as_ptr().addr() != at),
            "the bin handed the block just freed straight back"
        );
        stale.addref();
        assert!(matches!(
            hooks.heap.take_sensed().map(Fatal::into_reason),
            Some(FatalReason::Error(message))
                if message.starts_with(&format!("InternalError: an addref of the block at {at:#x}, which the heap has freed"))
        ));

        let more: Vec<Str> = (0..mm::QUARANTINE_SLOTS)
            .filter_map(|_| hooks.heap.string(b"a string of its own block").ok())
            .collect();
        assert_eq!(more.len(), mm::QUARANTINE_SLOTS);
        assert!(more.iter().all(|s| s.address().as_ptr().addr() != at));
        for s in more {
            hooks.heap.release_string(s);
        }
        assert_eq!(hooks.heap.take_sensed(), None, "an eviction found a write");
        let again: Vec<Str> = (0..mm::QUARANTINE_SLOTS + 2)
            .filter_map(|_| hooks.heap.string(b"a string of its own block").ok())
            .collect();
        assert!(
            again.iter().any(|s| s.address().as_ptr().addr() == at),
            "the evicted block never came back"
        );
        for s in again.into_iter().chain(fresh) {
            hooks.heap.release_string(s);
        }
        assert_eq!(hooks.heap.take_sensed(), None);
    }

    /// A block freed a second time while its bin's quarantine holds it
    /// is not freed again, and the free is recorded as the
    /// `InternalError` naming the block: the bin hands the block out once.
    #[cfg(debug_assertions)]
    #[test]
    fn a_free_of_a_quarantined_slot_is_refused_and_named() {
        let _alone = alone();
        let mut heap = Heap::new();
        let Ok(at) = heap.mm.alloc(32) else {
            return;
        };
        let held = heap.memory_usage(false);
        // SAFETY: the block was allocated just above and nothing names it.
        unsafe { heap.mm.free(at) };
        // SAFETY: the defect planted: the block is freed and its bin's
        // quarantine holds it, which is what the sensor reads first.
        unsafe { heap.mm.free(at) };
        assert_eq!(held - heap.memory_usage(false), 32, "counted off once");
        assert_eq!(
            heap.take_sensed(),
            Some(Fatal::error(format!(
                "InternalError: a free of the 32-byte block at {:#x}, which the heap has freed",
                at.as_ptr().addr()
            )))
        );
    }

    /// A write into a block its bin's quarantine holds is found as the
    /// block leaves, recorded as the `InternalError` naming the block:
    /// over the poison in its first word or over the mark after it, and
    /// when the block leaves by eviction or by the heap's GC.
    #[cfg(debug_assertions)]
    #[test]
    fn a_write_into_a_quarantined_slot_is_found_as_it_leaves() {
        type Leave = fn(&mut Heap);
        let _alone = alone();
        let leaves: [(&str, Leave); 2] = [
            ("evicted", |heap| {
                let more: Vec<Str> = (0..mm::QUARANTINE_SLOTS)
                    .filter_map(|_| heap.string(b"a string of its own block").ok())
                    .collect();
                for s in more {
                    heap.release_string(s);
                }
            }),
            ("drained", |heap| {
                heap.gc_mem_caches();
            }),
        ];
        for (how, leave) in leaves {
            for word in [0, 1, 2] {
                let mut heap = Heap::new();
                let made = heap.string(b"a string of its own block");
                assert!(made.is_ok(), "the heap would not allocate");
                let Ok(s) = made else {
                    return;
                };
                let at = s.address();
                heap.release_string(s);
                let words: *mut u64 = at.as_ptr().cast();
                // SAFETY: the block is a freed slot of `heap`'s bin, its
                // chunk still mapped, and the three words are inside it;
                // the write is the stale one planted.
                unsafe { words.wrapping_add(word).write(7) };
                assert_eq!(heap.take_sensed(), None, "{how} {word}");
                leave(&mut heap);
                assert_eq!(
                    heap.take_sensed(),
                    Some(Fatal::error(format!(
                        "InternalError: a write into the 56-byte block at {:#x} after the heap freed it, found as it left the quarantine",
                        at.as_ptr().addr()
                    ))),
                    "{how} {word}"
                );
            }
        }
    }

    /// A bin whose free list is empty carves a new `run` rather than hand
    /// out a slot its quarantine holds, so a block freed from a full
    /// `run` is still held back: a `run` of the 3072-byte bin holds four
    /// slots, and four freed and four made are four others. Within a
    /// chunk of the limit, where one more `run` could end the request
    /// where php's reuses a slot, the bin hands out its quarantine's
    /// oldest slot instead, checked, with a write into it found as it
    /// leaves.
    #[cfg(debug_assertions)]
    #[test]
    fn a_bin_carves_a_run_before_it_hands_out_a_held_slot_unless_the_limit_is_near() {
        let _alone = alone();
        let make = |heap: &mut Heap| -> Vec<Str> {
            (0..4)
                .filter_map(|_| heap.string(&[b'x'; 3000]).ok())
                .collect()
        };
        for near in [false, true] {
            let mut heap = Heap::new();
            if near {
                assert!(heap.set_memory_limit(heap.memory_usage(true) + mm::CHUNK_SIZE / 2));
            }
            let first = make(&mut heap);
            assert_eq!(first.len(), 4);
            let at: Vec<usize> = first.iter().map(|s| s.address().as_ptr().addr()).collect();
            let written = first[0].address();
            for s in first {
                heap.release_string(s);
            }
            let first_word: *mut u64 = written.as_ptr().cast();
            // SAFETY: the block is a freed slot of `heap`'s 3072-byte bin,
            // its chunk still mapped; the write is the stale one planted.
            unsafe { first_word.write(7) };
            let second = make(&mut heap);
            let again: Vec<usize> = second.iter().map(|s| s.address().as_ptr().addr()).collect();
            if near {
                assert_eq!(again, at, "the oldest held slot first, and no new run");
                assert_eq!(
                    heap.take_sensed(),
                    Some(Fatal::error(format!(
                        "InternalError: a write into the 3072-byte block at {:#x} after the heap freed it, found as it left the quarantine",
                        written.as_ptr().addr()
                    )))
                );
            } else {
                assert!(
                    again.iter().all(|a| !at.contains(a)),
                    "a held slot was handed out"
                );
                assert_eq!(heap.take_sensed(), None);
            }
            for s in second {
                heap.release_string(s);
            }
        }
    }

    /// One more owner through the type's own addref, the path
    /// `zend_array_dup`'s key addref took past [`Heap::addref`] when it
    /// corrupted the free list: each of the five refcounted kinds.
    #[cfg(debug_assertions)]
    #[track_caller]
    fn addref_by_kind(z: Zval) {
        if let Some(s) = z.as_str() {
            s.addref();
        } else if let Some(a) = z.as_arr() {
            a.addref();
        } else if let Some(o) = z.as_obj() {
            o.addref();
        } else if let Some(r) = z.as_resource() {
            r.addref();
        } else if let Some(r) = z.as_reference() {
            r.addref();
        }
    }

    /// An addref of a freed block through each type's own addref, not
    /// the heap's, is refused at the refcounted header every path
    /// reaches, recorded as the `InternalError` naming the block, the
    /// operation and the line that asked, and writes nothing: the block
    /// leaves its bin's quarantine with the poison in its first word,
    /// where an increment would have moved it, when the heap's GC drains
    /// the quarantine.
    #[cfg(debug_assertions)]
    #[test]
    fn an_addref_of_a_freed_block_through_each_type_is_refused_at_the_header() {
        type Make = fn(&mut Heap) -> Result<Zval, Fatal>;
        let _alone = alone();
        let mut hooks = Recorder { heap: Heap::new() };
        let kinds: [(&str, Make); 5] = [
            ("string", |h| {
                h.string(b"a string of its own block").map(Zval::string)
            }),
            ("array", |h| h.array().map(Zval::array)),
            ("object", |h| h.object(1, 0, 0).map(Zval::object)),
            ("reference", |h| {
                h.reference(Zval::null()).map(Zval::reference)
            }),
            ("resource", |h| {
                h.resource(ResourceType(3)).map(Zval::resource)
            }),
        ];
        let at = |z: Zval| z.counted().map(|at| at.as_ptr().addr());
        for (kind, make) in kinds {
            let made = (make(&mut hooks.heap), make(&mut hooks.heap));
            assert!(
                made.0.is_ok() && made.1.is_ok(),
                "the heap would not allocate a {kind}"
            );
            let (Ok(stale), Ok(other)) = made else {
                return;
            };
            Heap::release(&mut hooks, other);
            Heap::release(&mut hooks, stale);
            assert_eq!(hooks.heap.take_sensed(), None, "{kind}");

            addref_by_kind(stale);
            let site = line!() - 1;
            assert!(stale.is_refcounted(), "a {kind} is refcounted");
            let Some(block) = at(stale) else {
                return;
            };
            assert_eq!(
                hooks.heap.take_sensed(),
                Some(Fatal::error(format!(
                    "InternalError: an addref of the block at {block:#x}, which the heap has freed, at {}:{site}:13",
                    file!()
                ))),
                "{kind}"
            );

            hooks.heap.gc_mem_caches();
            assert_eq!(
                hooks.heap.take_sensed(),
                None,
                "the refused addref of the {kind} wrote into its block"
            );
        }
    }

    /// php's `heap->overflow`: the allocation the limit refuses raises
    /// the fatal and lifts the limit, so what renders the fatal is not
    /// refused, and the limit holds again once the engine says the
    /// fatal is rendered.
    #[test]
    fn the_limit_is_lifted_while_its_fatal_is_rendered() {
        let _alone = alone();
        let mut heap = Heap::new();
        heap.set_memory_limit(4 * 1024 * 1024);
        let bytes = vec![b'x'; 1024 * 1024];
        let mut held = Vec::new();
        let refused = loop {
            match heap.string(&bytes) {
                Ok(s) => held.push(s),
                Err(fatal) => break fatal,
            }
        };
        assert!(
            matches!(refused.reason(), FatalReason::Error(m) if m.starts_with("Allowed memory size of 4194304 bytes exhausted"))
        );
        let rendering = heap.string(&bytes);
        assert!(rendering.is_ok(), "the rendering is not refused");
        assert!(heap.memory_usage(true) > 4 * 1024 * 1024);
        held.extend(rendering);
        heap.overflow_rendered();
        let after = (0..4).map(|_| heap.string(&bytes)).find_map(Result::err);
        assert!(after.is_some(), "the limit holds again");
        for s in held {
            heap.release_string(s);
        }
    }

    /// What the heap's tests pass where a request would serve a
    /// destructor: nothing to serve, because a string has none.
    struct Recorder {
        heap: Heap,
    }

    impl HeapHooks for Recorder {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn destruct(&mut self, _obj: Obj) {}
        fn free_native(&mut self, _obj: Obj) {}
        fn gc_children(&mut self, _obj: Obj, _: &mut dyn FnMut(Zval)) -> crate::GcTable {
            crate::GcTable::Properties
        }
        fn free_resource(&mut self, _: crate::Res, _: crate::ResourceType) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::out_of_memory(bytes)
        }
    }
}
