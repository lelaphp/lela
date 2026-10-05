//! The memory area of a request: the heap reached through the
//! request, interned and request strings, references, arrays, the
//! memory limit and its verdict, `memory_get_usage()` and the garbage
//! collector's natives. Its state is the heap itself, which every area
//! reaches, so it stays on the request.

use std::sync::Arc;

pub use php_heap::{Allocation, AllocationObserver, ObserveAllocations};
use php_heap::{Arr, Fatal, Heap, Intern, Ref, SortEntry, ZvalRef};

use crate::api::raising::Thrown;
use crate::api::values::{Key, PhpArray, PhpObject, PhpRef, PhpString, Value};

use super::{Hooks, Request};

impl Request {
    /// The request's own memory, for the compilation whose unit is this
    /// request's alone (design §5.2): what a host handed the source for,
    /// an `eval` and an uncached include, whose constant strings are
    /// interned here and are gone when the request ends.
    pub(crate) fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// `zend_string_init_existing_interned` (`Z/Zend/zend_string.c`):
    /// the interned string for `bytes` where the engine or this request
    /// holds one already, and a new string of the request's otherwise.
    pub(crate) fn existing_interned(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        match self.interned_held(bytes) {
            Some(held) => Ok(held),
            None => self.string(bytes),
        }
    }

    /// `zend_new_interned_string`, what `zval_make_interned_string`
    /// makes of a declared value: the interned string for `bytes` where
    /// the engine or this request holds one already, a string a unit's
    /// compilation interned among them, and one interned into the
    /// request's otherwise, as [`Request::interned`] interns it.
    pub(crate) fn new_interned(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        match self.interned_held(bytes) {
            Some(held) => Ok(held),
            None => self.interned(bytes),
        }
    }

    /// The interned string for `bytes` the engine's table or this
    /// request's holds, `zend_interned_string_ht_lookup` over both.
    fn interned_held(&self, bytes: &[u8]) -> Option<PhpString> {
        self.shared
            .existing_interned(bytes)
            .or_else(|| self.heap.interned_find(bytes))
            .map(PhpString::of)
    }

    /// A string of `bytes` out of this request's heap, what
    /// [`PhpString::new`] is: php's `ZVAL_STRINGL_FAST`, the process's
    /// interned string for the empty string and a string of one byte
    /// (`ZSTR_EMPTY_ALLOC`, `ZSTR_CHAR`), `zend_string_init` otherwise.
    /// The memory limit is the fatal that ends the request, recorded
    /// here where `zend_mm` leaves the request, and the `Thrown` that
    /// carries no throwable ends the native.
    pub(crate) fn string(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        let allocated = if bytes.len() <= 1 {
            self.heap.intern(bytes)
        } else {
            self.heap.string(bytes)
        };
        self.allocated(allocated)
    }

    /// A string of `len` bytes written in place, what
    /// [`PhpString::filled`] is: `zend_string_alloc`, then the caller's
    /// write into `ZSTR_VAL`.
    pub(crate) fn string_with(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_with(len, fill);
        self.allocated(allocated)
    }

    /// A string of `n * m + l` bytes written in place, what
    /// [`PhpString::product`] is: `zend_string_safe_alloc`.
    pub(crate) fn string_product(
        &mut self,
        n: usize,
        m: usize,
        l: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_safe_alloc(n, m, l, fill);
        self.allocated(allocated)
    }

    /// `s` with `tail`'s bytes after its own, `zend_string_extend` and
    /// the `memcpy` after it ([`php_heap::Heap::string_append`]): one
    /// owner of `s` handed over, one of the longer string handed back, grown
    /// where it stands when that owner was the only one.
    pub(crate) fn string_append(
        &mut self,
        s: PhpString,
        tail: PhpString,
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_append(s.heap(), tail.heap());
        self.allocated(allocated)
    }

    /// What the heap answered, as the native sees it: the string, or the
    /// memory limit as php's fatal, which `zend_mm` raises where the
    /// allocation was asked for and which ends the request from there.
    fn allocated(&mut self, allocated: Result<php_heap::Str, Fatal>) -> Result<PhpString, Thrown> {
        self.fatal_of(allocated).map(PhpString::of)
    }

    /// One more owner of what `value` holds: `Z_TRY_ADDREF_P`, a refcount
    /// for a string, an array, an object, a resource or a reference of
    /// the request's, nothing for a scalar, an interned string or an
    /// immutable array. What a native calls where php's body copies a
    /// value it already holds into a second place (`zend_string_copy`,
    /// `ZVAL_COPY`): the clone of the `Value` is `ZVAL_COPY_VALUE`, and
    /// this beside it is the second owner, given up with whatever row or
    /// property the copy is put in. What a native does before it returns an argument
    /// as its value, `RETURN_COPY` and `RETURN_STR_COPY`: the frame gives
    /// its owner up with the call, and the caller's temporary holds the
    /// one taken here.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(&self, value: &Value) {
        self.heap.addref(value.to_zval());
    }

    /// One owner fewer of what `value` holds, `zval_ptr_dtor`: the
    /// string, the array, the object, the resource or the reference is
    /// freed when its last owner gives it up, an array's elements and a
    /// reference's value released with it, an object's destructor and a
    /// resource's per-type destructor served first through seam 1, and
    /// an interned string or an immutable array never is. The request
    /// is the hooks over itself, so the heap a destructor allocates from
    /// is this one, handed back for the duration ([`Hooks`]).
    // Taking the value is the point: the owner is given up here, and a
    // `Value` that is still in hand after this call names what may be
    // freed.
    #[allow(clippy::needless_pass_by_value)]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release(&mut self, value: Value) {
        Heap::release(&mut Hooks(self), value.to_zval());
    }

    /// One owner fewer as `zval_ptr_dtor_nogc` gives it up: what
    /// [`Request::release`] does, and never a possible root of the cycle
    /// collector (design §5.12). What `FREE_OP` does to a temporary
    /// operand and `zend_vm_stack_free_args` to an internal call's
    /// arguments.
    #[allow(clippy::needless_pass_by_value)]
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn release_nogc(&mut self, value: Value) {
        Heap::release_nogc(&mut Hooks(self), value.to_zval());
    }

    /// `ZVAL_NEW_REF`, what [`PhpRef::new`] is: a cell of the request's
    /// heap holding `value`, one owner of the cell, `value` the cell's
    /// from here, and the memory limit as the fatal that ends the
    /// request.
    pub(crate) fn reference(&mut self, value: Value) -> Result<PhpRef, Thrown> {
        let allocated = self.heap.reference(value.into_zval());
        self.fatal_of(allocated).map(PhpRef::of)
    }

    /// The reference's half of `zend_assign_to_variable`, what
    /// [`PhpRef::set`] and `ASSIGN` over a CV holding a reference are:
    /// `value` is the cell's from here, and what the cell held is
    /// released last, after the write.
    pub(crate) fn reference_set(&mut self, r: Ref, value: Value) {
        Heap::reference_set(&mut Hooks(self), r, value.into_zval());
    }

    /// An empty array out of this request's heap, what [`PhpArray::new`]
    /// is: php's `array_init`, and the memory limit as the fatal that
    /// ends the request.
    pub(crate) fn array(&mut self) -> Result<PhpArray, Thrown> {
        let allocated = self.heap.array();
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_new_array(size)`, what `INIT_ARRAY` lays down for the
    /// elements it was told to expect, and `zend_hash_real_init_mixed`
    /// over it where the opline says the array cannot be packed
    /// (`ZEND_ARRAY_NOT_PACKED`); the memory limit as the fatal that ends
    /// the request.
    pub(crate) fn array_sized(&mut self, size: u32, packed: bool) -> Result<PhpArray, Thrown> {
        let allocated = if packed {
            self.heap.array_sized(size)
        } else {
            self.heap.array_mixed(size)
        };
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_hash_update` or `zend_hash_index_update` over `a`, which
    /// has one owner: `value` is the array's from here, what `key` held
    /// is released, and a string key is one more owner.
    pub(crate) fn array_update(
        &mut self,
        a: Arr,
        key: php_heap::Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let z = value.into_zval();
        let written = Heap::array_update(&mut Hooks(self), a, key, z);
        self.fatal_of(written)
    }

    /// `zend_hash_add_new` or `zend_hash_index_add_new` over `a`, which
    /// has one owner: `value` is the array's from here and a string key
    /// one more owner, appended without the lookup.
    pub(crate) fn array_add_new(
        &mut self,
        a: Arr,
        key: php_heap::Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let added = self.heap.array_add_new(a, key, value.into_zval());
        self.fatal_of(added)
    }

    /// `zend_hash_next_index_insert` over `a`, which has one owner:
    /// `false`, and `value` given up, when the next element is taken.
    pub(crate) fn array_append(&mut self, a: Arr, value: Value) -> Result<bool, Thrown> {
        let z = value.into_zval();
        let appended = self.heap.array_append(a, z);
        if appended == Ok(false) {
            Heap::release(&mut Hooks(self), z);
        }
        self.fatal_of(appended)
    }

    /// `zend_hash_lookup` over `a`, which has one owner: the entry at
    /// `key` in place, laid down holding `null` where the table has none,
    /// which is what a `FETCH_DIM_W` and an `ASSIGN_DIM` write through.
    pub(crate) fn array_lookup(
        &mut self,
        a: Arr,
        key: php_heap::Key,
    ) -> Result<Option<ZvalRef>, Thrown> {
        let entry = self.heap.array_lookup(a, key);
        self.fatal_of(entry)
    }

    /// `zend_hash_next_index_insert` of `null` over `a`, which has one
    /// owner: the entry appended, in place; `None` when the next element
    /// is taken.
    pub(crate) fn array_next_entry(&mut self, a: Arr) -> Result<Option<ZvalRef>, Thrown> {
        let entry = self.heap.array_next_entry(a);
        self.fatal_of(entry)
    }

    /// `zend_hash_del` over `a`, which has one owner: `true` when the key
    /// was there, its value released.
    pub(crate) fn array_del(&mut self, a: Arr, key: php_heap::Key) -> bool {
        Heap::array_del(&mut Hooks(self), a, key)
    }

    /// `zend_array_dup`: a copy of `a` with one owner.
    pub(crate) fn array_dup(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let copied = self.heap.array_dup(a);
        self.fatal_of(copied)
    }

    /// `SEPARATE_ARRAY`: `a` itself with one owner, else a copy of it
    /// with one, `a` one owner fewer.
    pub(crate) fn array_separate(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let separated = self.heap.separate(a);
        self.fatal_of(separated)
    }

    /// `zend_hash_sort` over `a`, which has one owner: the comparison
    /// carried through seam 1, since a `usort` callback runs PHP code,
    /// and the keys dropped under `renumber`.
    pub(crate) fn array_sort(
        &mut self,
        a: Arr,
        renumber: bool,
        compare: &mut dyn FnMut(&mut Request, &SortEntry, &SortEntry) -> std::cmp::Ordering,
    ) -> Result<(), Thrown> {
        let sorted = Heap::array_sort(&mut Hooks(self), a, renumber, &mut |hooks, x, y| {
            compare(hooks.0, x, y)
        });
        self.fatal_of(sorted)
    }

    /// `SEPARATE_ZVAL_NOREF` on what `r` holds, what
    /// [`PhpRef::separate_array`] is: the array in the cell with one
    /// owner, a copy written into the cell where it had more.
    pub(crate) fn reference_separate(&mut self, r: Ref) -> Result<Option<Arr>, Thrown> {
        let separated = self.heap.reference_separate(r);
        self.fatal_of(separated)
    }

    /// What the heap answered, as the native sees it: the value, or the
    /// fatal the allocator raised, recorded here where `zend_mm` leaves
    /// the request, as the `Thrown` carrying no throwable that ends the
    /// native.
    pub(crate) fn fatal_of<T>(&mut self, answered: Result<T, Fatal>) -> Result<T, Thrown> {
        match answered {
            Ok(value) => Ok(value),
            Err(fatal) => {
                self.end_in(&fatal);
                Err(Thrown::of(None))
            }
        }
    }

    /// The limit the heap holds a request to from its start: what
    /// `php_module_startup` left, `memory_limit` where it is under
    /// `max_memory_limit` and the maximum's otherwise, since
    /// `OnChangeMaxMemoryLimit` runs first and set it. `-1` is no limit.
    pub(super) fn set_memory_limit(&mut self) {
        let limit = self
            .limit_bytes(b"memory_limit")
            .min(self.limit_bytes(b"max_memory_limit"));
        self.heap.set_memory_limit(limit.min(usize::MAX >> 1));
    }

    /// An entry `zend_ini_parse_uquantity` reads, `-1` the largest size.
    fn limit_bytes(&self, name: &[u8]) -> usize {
        let bytes = self.ini_get(name).map_or(0, Request::ini_parse_bytes);
        usize::try_from(bytes.cast_unsigned()).unwrap_or(usize::MAX)
    }

    /// `OnChangeMemoryLimit` (`Z/main/main.c:333-367`) over `value` at
    /// runtime or at startup: a value past `max_memory_limit` is php's
    /// warning (silent for `-1`) and the maximum's text in its place,
    /// the heap left as it was; otherwise `zend_set_memory_limit`, whose
    /// refusal of a limit under what the chunks hold is php's other
    /// warning and the entry kept. The text the entry takes is the
    /// answer.
    ///
    /// ```php
    /// $a = str_repeat("0", 5 * 1024 * 1024);
    /// var_dump(ini_set("memory_limit", "3M"), ini_get("memory_limit"));
    /// ```
    /// ```expect
    /// Warning: Failed to set memory limit to 3145728 bytes (Current memory usage is 7356416 bytes) in %s on line 3
    /// bool(false)
    /// string(4) "128M"
    /// ```
    pub(crate) fn memory_limit_verdict(&mut self, value: &[u8]) -> Option<Vec<u8>> {
        let wanted =
            usize::try_from(Request::ini_parse_bytes(value).cast_unsigned()).unwrap_or(usize::MAX);
        let max = self.limit_bytes(b"max_memory_limit");
        if wanted > max {
            if wanted != usize::MAX {
                crate::api::raising::warning(
                    self,
                    &format!(
                        "Failed to set memory_limit to {wanted} bytes. Setting to max_memory_limit instead (currently: {max} bytes)"
                    ),
                );
            }
            return self.ini_get(b"max_memory_limit").map(<[u8]>::to_vec);
        }
        if !self.heap.set_memory_limit(wanted.min(usize::MAX >> 1)) {
            let usage = self.heap.memory_usage(true);
            crate::api::raising::warning(
                self,
                &format!(
                    "Failed to set memory limit to {wanted} bytes (Current memory usage is {usage} bytes)"
                ),
            );
            return None;
        }
        Some(value.to_vec())
    }

    /// `memory_get_usage()`.
    #[must_use]
    pub fn memory_usage(&self, real: bool) -> usize {
        self.heap.memory_usage(real)
    }

    /// `memory_get_peak_usage()`: the chunks' high-water mark for
    /// `real == true`, the bin accounting's for `real == false`, the two
    /// the heap keeps apart (design §5.13).
    #[must_use]
    pub fn memory_peak(&self, real: bool) -> usize {
        if real {
            self.heap.memory_real_peak()
        } else {
            self.heap.memory_peak()
        }
    }

    /// `memory_reset_peak_usage()`.
    pub fn reset_memory_peak(&mut self) {
        self.heap.reset_peak();
    }

    /// `observer` told of every allocation, reallocation and free the
    /// request's heap serves from here on, and of what
    /// `gc_mem_caches()` gives back (design §7.1): what stands in the
    /// place of php's `zend_mm_set_custom_handlers`, which no extension
    /// is given. The heap serves each one still, so `memory_get_usage()`
    /// reads what it reads with no observer, and with none put in an
    /// allocation pays one test of an empty list, as php's pays its
    /// `use_custom_heap` test. An observer is told mid-allocation and is
    /// handed nothing of the request; what it keeps is its own, and it is
    /// dropped with the request's heap where it is not taken out before.
    pub fn observe_allocations(
        &mut self,
        observer: Box<dyn ObserveAllocations>,
    ) -> AllocationObserver {
        self.heap.observe_allocations(observer)
    }

    /// The observer put in at `at` taken out and handed back, so the
    /// extension that put it in reads what it kept; `None` where it was
    /// taken out already.
    pub fn unobserve_allocations(
        &mut self,
        at: AllocationObserver,
    ) -> Option<Box<dyn ObserveAllocations>> {
        self.heap.unobserve_allocations(at)
    }

    /// `gc_collect_cycles()` through php's pointer: the top link the
    /// bundle supplied ([`ExtensionBuilder::gc_collect`]), or with none
    /// `zend_gc_collect_cycles` over the request's heap (design §5.12);
    /// the values the collection freed, as php 8.5 counts them.
    ///
    /// [`ExtensionBuilder::gc_collect`]: crate::api::abi::ExtensionBuilder::gc_collect
    pub fn gc_collect(&mut self) -> i64 {
        let shared = Arc::clone(&self.shared);
        crate::supplied::GcBeneath(shared.supplied.gc_collect.links()).collect(self)
    }

    /// `zend_gc_collect_cycles`, the heap's own collection beneath every
    /// link of the pointer.
    pub(crate) fn gc_collect_own(&mut self) -> i64 {
        Heap::gc_collect_cycles(&mut Hooks(self))
    }

    /// One entry of the collector's destructor phase served
    /// ([`Heap::gc_call_destructor`]), for the fiber the phase is served
    /// in: the object whose destructor was served.
    pub(crate) fn gc_call_destructor(&mut self, idx: u32) -> Option<PhpObject> {
        Heap::gc_call_destructor(&mut Hooks(self), idx).map(PhpObject::of)
    }

    /// `gc_check_possible_root` over an object: a possible root again,
    /// where its release left it owners.
    pub(crate) fn gc_possible_root(&mut self, object: PhpObject) {
        let zval = object.as_value().to_zval();
        Heap::gc_check_possible_root(&mut Hooks(self), zval);
    }

    /// `gc_enable()` and `gc_disable()`: `zend.enable_gc` altered, whose
    /// `OnUpdateGCEnabled` is `gc_enable`, so `ini_get` reads what the
    /// call left.
    ///
    /// The value is `zend_alter_ini_entry_chars`'s new string of one
    /// byte (`Z/Zend/zend_builtin_functions.c:184-206`), which the entry
    /// keeps in place of the one it held:
    ///
    /// ```php
    /// ini_set('from', 'x');
    /// $m = memory_get_usage(); gc_disable(); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); gc_enable(); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 32
    /// 0
    /// ```
    pub fn gc_enable(&mut self, enabled: bool) {
        let value: &[u8] = if enabled { b"1" } else { b"0" };
        // The memory limit's fatal is recorded and ends the request.
        let string = self
            .string_with(value.len(), |bytes| bytes.copy_from_slice(value))
            .ok();
        self.ini_hold_string(b"zend.enable_gc", Some(value), string);
        self.heap.gc_enable(enabled);
    }

    /// `gc_enabled()`.
    #[must_use]
    pub fn gc_enabled(&self) -> bool {
        self.heap.gc_enabled()
    }

    /// `gc_status()`, `zend_gc_get_status` in php 8.5's twelve fields, the
    /// times in seconds as php divides its nanoseconds, the keys strings
    /// of the request's.
    pub fn gc_status(&mut self) -> Result<PhpArray, Thrown> {
        let status = self.heap.gc_status();
        let mut array = PhpArray::new(self)?;
        for (name, value) in [
            ("running", Value::bool(status.running)),
            ("protected", Value::bool(status.protected)),
            ("full", Value::bool(status.full)),
            ("runs", Value::int(i64::from(status.runs))),
            ("collected", Value::int(i64::from(status.collected))),
            ("threshold", Value::int(i64::from(status.threshold))),
            ("buffer_size", Value::int(i64::from(status.buffer_size))),
            ("roots", Value::int(i64::from(status.roots))),
            (
                "application_time",
                Value::float(seconds(status.application_time)),
            ),
            (
                "collector_time",
                Value::float(seconds(status.collector_time)),
            ),
            (
                "destructor_time",
                Value::float(seconds(status.destructor_time)),
            ),
            ("free_time", Value::float(seconds(status.free_time))),
        ] {
            let key = Key::Str(PhpString::new(self, name.as_bytes())?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// An interned string, never refcounted, for a name the engine holds
    /// as bytes, as `zend_string_init_interned_request` answers it: the
    /// engine's permanent one where a declaration made the name
    /// ([`crate::engine::Shared::permanent`]), which costs the request
    /// nothing, else this request's heap's; the `Err` is the memory
    /// limit refusing the intern.
    ///
    /// A class's and a method's names are `ce->name` and
    /// `function_name`, so only the array of names is counted:
    ///
    /// ```php
    /// $o = new ArrayObject; $i = new ArrayIterator;
    /// $m = memory_get_usage();
    /// $a = get_class($o); $b = $o::class; $c = get_parent_class($i); $d = get_class_methods('Countable');
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(216)
    /// ```
    pub(crate) fn interned(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        if let Some(permanent) = self.shared.permanent(bytes) {
            return Ok(PhpString::of(permanent));
        }
        let interned = self.heap.intern(bytes);
        self.fatal_of(interned).map(PhpString::of)
    }

    /// The heap's memory limit in bytes, `PG(memory_limit)` as
    /// `zend_set_memory_limit` set it.
    pub fn heap_memory_limit(&self) -> usize {
        self.heap.memory_limit()
    }
}

/// `(double) hrtime / ZEND_NANO_IN_SEC`, a collector time in seconds.
fn seconds(d: std::time::Duration) -> f64 {
    d.as_secs_f64()
}
