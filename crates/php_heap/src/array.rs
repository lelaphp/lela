//! Arrays (design §5.3): php's `zend_hash`, because everything about it
//! is observable. The insertion order, `nNextFreeElement` with the 8.3
//! rule that a negative first key sets it, the internal pointer under
//! `current`, `next`, `reset` and `end`, the packed table that becomes a
//! hash at the first key that breaks it, the tombstone a deleted element
//! leaves and the rehash that compacts them and moves the pointer past
//! them, and `count()` after `unset` are each one algorithm here, copied
//! from `Z/Zend/zend_hash.c` at the tag.
//!
//! The layout is php's, to the byte, because `memory_get_usage()` reads
//! it: a 56-byte header out of the request's heap, php's
//! `sizeof(HashTable)`, uninitialised until the first write; then one
//! allocation for the data, the hash slots in front of the buckets
//! (`HT_SIZE_EX`, forty bytes a bucket) or of the zvals of a packed
//! table (`HT_PACKED_SIZE_EX`, sixteen a slot and two slots of hash);
//! sizes from eight, doubling, to `HT_MAX_SIZE`. An [`Arr`] is the
//! pointer php's `zend_array *` is, `Copy`, no `Drop`, the owners
//! counted by [`crate::Heap::addref`] and [`crate::Heap::release`], and
//! the writes are the heap's, because a write may grow the table, and
//! growing it is an allocation the request is held to.
//!
//! The iterators php keeps beside a table for `foreach` by reference
//! and over an object (`nIteratorsCount`, `EG(ht_iterators)`) are
//! [`crate::iterators`]'s, and every write here that moves a position,
//! the rehash that compacts, the delete that trims `nNumUsed`, the copy
//! and the destroy, moves theirs as php's does; the internal pointer is
//! here, and every move of it is php's.

// A position, a size and a mask are `uint32_t` in php and here, and a
// hash slot's index is the low word of the hash; the narrowing casts
// below are php's own. The data pointer is the allocator's, which hands
// out eight-aligned bins and whole pages, so a bucket or a zval cast
// from it is aligned as php's `arData` is.
#![allow(clippy::cast_possible_truncation, clippy::cast_ptr_alignment)]

use std::alloc::Layout;
use std::cell::Cell;
use std::cmp::Ordering;
use std::ptr::NonNull;
use std::sync::OnceLock;

use crate::gc::{self, Counted, Refcounted};
use crate::hooks::{Fatal, HeapHooks};
use crate::iterators::Iterators;
use crate::mm::Mm;
use crate::reference::Ref;
use crate::sort::zend_sort;
use crate::string::Str;
use crate::{GivenUp, Heap, Last, Zval, ZvalRef, ZvalType};

/// Where an array's header and data come from: the request's heap for
/// every array a request builds, which is what `memory_get_usage()`
/// counts, and the system allocator for a constant array of a published
/// unit, php's `pemalloc(size, 1)` under opcache (design §5.3). The
/// algorithm over the table is one whichever answers.
pub(crate) trait Allocate {
    /// `size` usable bytes, aligned for a bucket.
    fn alloc(&mut self, size: usize) -> Result<NonNull<u8>, Fatal>;
    /// Give an allocation back.
    ///
    /// # Safety
    ///
    /// `at` came from this allocator's `alloc` and nothing names it.
    unsafe fn free(&mut self, at: NonNull<u8>);
    /// `perealloc2`: `size` usable bytes with the first `copy` of the
    /// old ones in them, where the old allocation stood when the
    /// allocator can grow or cut it there, and moved otherwise.
    ///
    /// # Safety
    ///
    /// `at` came from this allocator's `alloc`, nothing else names it,
    /// and its first `copy` bytes are initialised.
    unsafe fn realloc(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Fatal>;
    /// `EG(ht_iterators)`, which a rehash moves: the request's list
    /// beside its heap, and none for a constant array, which no walk
    /// registers against.
    fn iterators(&mut self) -> Option<&mut Iterators> {
        None
    }
}

/// The request's allocator and its iterator list together, which is
/// what a write to a request's array is handed: the allocation a write
/// may make and the positions a rehash it causes moves.
pub(crate) struct Tables<'h> {
    mm: &'h mut Mm,
    iterators: &'h mut Iterators,
}

impl Allocate for Tables<'_> {
    fn alloc(&mut self, size: usize) -> Result<NonNull<u8>, Fatal> {
        self.mm.alloc(size)
    }

    unsafe fn realloc(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Fatal> {
        // SAFETY: the caller's, handed on to the heap the block is from.
        unsafe { self.mm.realloc(at, copy, size) }
    }

    unsafe fn free(&mut self, at: NonNull<u8>) {
        // SAFETY: the caller's, handed on to the heap the block is from.
        unsafe { self.mm.free(at) };
    }

    fn iterators(&mut self) -> Option<&mut Iterators> {
        Some(self.iterators)
    }
}

impl Allocate for Mm {
    fn alloc(&mut self, size: usize) -> Result<NonNull<u8>, Fatal> {
        Mm::alloc(self, size)
    }

    unsafe fn realloc(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Fatal> {
        // SAFETY: the caller's, handed on: the block is this heap's, its
        // first `copy` bytes are written and nothing else names it.
        unsafe { Mm::realloc(self, at, copy, size) }
    }

    unsafe fn free(&mut self, at: NonNull<u8>) {
        // SAFETY: the caller's, handed on: the block is this heap's and
        // nothing names it.
        unsafe { Mm::free(self, at) };
    }
}

/// The system allocator, php's `pemalloc(size, 1)`, for a constant
/// array and a persistent resource's payload: each allocation carries
/// its size in a sixteen-byte prefix, since `free` is handed the
/// address alone as `pefree` is.
pub(crate) struct System;

/// The prefix a [`System`] allocation carries.
const PREFIX: usize = 16;

impl System {
    fn layout(size: usize) -> Result<Layout, Fatal> {
        let whole = size.checked_add(PREFIX).ok_or(Fatal::out_of_memory(size))?;
        Layout::from_size_align(whole, PREFIX).map_err(|_| Fatal::out_of_memory(size))
    }
}

impl Allocate for System {
    fn alloc(&mut self, size: usize) -> Result<NonNull<u8>, Fatal> {
        let layout = System::layout(size)?;
        // SAFETY: the layout's size is the prefix and the bytes, never
        // zero.
        let at = unsafe { std::alloc::alloc(layout) };
        let Some(at) = NonNull::new(at) else {
            return Err(Fatal::out_of_memory(size));
        };
        // SAFETY: the allocation is at least the prefix, which holds the
        // size for `free` to read.
        unsafe { at.cast::<usize>().write(size) };
        // SAFETY: the allocation is the prefix and `size` bytes past it.
        Ok(unsafe { at.add(PREFIX) })
    }

    unsafe fn free(&mut self, at: NonNull<u8>) {
        // SAFETY: `at` is the address `alloc` handed out, the prefix
        // before it.
        let start = unsafe { at.sub(PREFIX) };
        // SAFETY: the prefix holds the size `alloc` wrote.
        let size = unsafe { start.cast::<usize>().read() };
        let Ok(layout) = System::layout(size) else {
            return;
        };
        // SAFETY: the allocation came from `alloc` under this layout.
        unsafe { std::alloc::dealloc(start.as_ptr(), layout) };
    }

    /// The system allocator has no page map to grow into, so a
    /// reallocation is the new bytes, the copy and the old given back;
    /// a constant array is built once and read, so nothing counts it.
    unsafe fn realloc(
        &mut self,
        at: NonNull<u8>,
        copy: usize,
        size: usize,
    ) -> Result<NonNull<u8>, Fatal> {
        let moved = self.alloc(size)?;
        // SAFETY: `at` holds `copy` readable bytes and `moved` is a fresh
        // allocation of `size`, so the shorter of the two is in both.
        unsafe { moved.copy_from_nonoverlapping(at, copy.min(size)) };
        // SAFETY: the caller's contract; the bytes are copied out.
        unsafe { self.free(at) };
        Ok(moved)
    }
}

/// A constant array (design §5.3), php's `IS_ARRAY_IMMUTABLE`, out of the
/// system allocator: the entries written in order as php's compiler
/// writes a jump table, then the immutable bit, after which no owner is
/// counted and no write reaches it. Its keys are persistent interned
/// strings and its values scalars, so freeing it releases nothing;
/// [`free_persistent_array`] gives its two allocations back. No entries
/// is the process's [`Arr::empty`], as `zend_try_ct_eval_array` answers
/// `ZVAL_EMPTY_ARRAY` for `[]`.
pub(crate) fn persistent_array(entries: &[(Key, Zval)]) -> Result<Arr, Fatal> {
    if entries.is_empty() {
        return Ok(Arr::empty());
    }
    let size = check_size(u32::try_from(entries.len()).unwrap_or(u32::MAX))?;
    let at = System.alloc(HEADER)?;
    // SAFETY: the allocation is the header's size and nothing else
    // holds it.
    let a = unsafe { Arr::init(at, size) };
    for (key, z) in entries {
        match *key {
            Key::Long(n) => write_long(&mut System, a, n, *z, Write::Update)?,
            Key::Str(s) => write_str(&mut System, a, s, *z)?,
        };
    }
    a.header().gc.add_flags(gc::IMMUTABLE);
    Ok(a)
}

/// `_zend_hash_init(ht, size, NULL, ZSTR_HASH_LOOKUP, 0)` over a header
/// the system allocator holds: what `zend_init_interned_strings_ht` does
/// to `CG(interned_strings)` (`Z/Zend/zend_string.c:78-84`), whose
/// `HashTable` is a field of `zend_compiler_globals` and not the request's
/// memory, while the data it lays down on the first add is the request's
/// own. [`free_table_header`] gives the header back; the data goes with
/// the heap whose allocator handed it out.
pub(crate) fn table_header(size: u32) -> Result<Arr, Fatal> {
    let size = check_size(size)?;
    let at = System.alloc(HEADER)?;
    // SAFETY: the allocation is the header's size and nothing else
    // holds it.
    Ok(unsafe { Arr::init(at, size) })
}

/// The header [`table_header`] laid down, back to the system allocator.
/// Its data is not touched: that is the request heap's, freed with the
/// heap's chunks, so this runs after the heap or over an array whose
/// data was never laid down.
pub(crate) fn free_table_header(a: Arr) {
    // SAFETY: the header came from `table_header` and nothing names it.
    unsafe { System.free(a.address()) };
}

/// The data a [`table_header`]'s array laid down on `mm`, given back
/// before [`free_table_header`] gives back the header: the end
/// of `zend_hash_destroy` over a table whose elements the caller has
/// freed already. A debug build's teardown is the one caller (design
/// §5.1, the request leak sensor).
#[cfg(debug_assertions)]
pub(crate) fn free_table_data(mm: &mut Mm, a: Arr) {
    if let Some(data) = a.data_addr() {
        // SAFETY: the data was laid down out of `mm` and the caller has
        // freed every element it held, so nothing names it.
        unsafe { mm.free(data) };
    }
}

/// `zend_hash_index_add_new(ht, index, &zv)` over a request's `mm` for a
/// key the caller knows is not there, which is what `zend_list_insert`
/// (`Z/Zend/zend_list.c:32-44`) knows of the id `nNextFreeElement` gave
/// it: the data the first add lays down, packed and sized as the header
/// says, and the doubling on the add past its end, are `mm`'s.
pub(crate) fn index_add_new(mm: &mut Mm, a: Arr, index: i64, z: Zval) -> Result<(), Fatal> {
    write_long(mm, a, index, z, Write::Update)?;
    Ok(())
}

/// `zend_hash_index_del(ht, index)` with no destructor: the element
/// gone and its value handed back to the caller, who is the entry's
/// destructor, as `list_entry_destructor` is `EG(regular_list)`'s
/// (`Z/Zend/zend_list.c:174`). `None` for an index that is not there.
pub(crate) fn index_del(a: Arr, index: i64) -> Option<Zval> {
    let pos = a.find(Key::Long(index))?;
    if let Pos::Bucket(idx) = pos {
        a.unlink(idx);
    }
    Some(a.del_at(pos.index(), None))
}

/// `pefree` over a [`persistent_array`]: the data, then the header.
/// The process's empty array, which a home hands out for a constant
/// array of no entries, is no home's to free.
pub(crate) fn free_persistent_array(a: Arr) {
    if a.is_same(Arr::empty()) {
        return;
    }
    if let Some(data) = a.data_addr() {
        // SAFETY: the data was `System`'s and nothing in it is owned.
        unsafe { System.free(data) };
    }
    // SAFETY: the header came from `persistent_array` and the unit that
    // named it is gone.
    unsafe { System.free(a.address()) };
}

/// `HT_MIN_SIZE`.
const MIN_SIZE: u32 = 8;
/// `HT_MAX_SIZE` on a 64-bit machine.
const MAX_SIZE: u32 = 0x4000_0000;
/// `HT_MIN_MASK`: the two hash slots a packed table keeps.
const MIN_MASK: u32 = u32::MAX - 1;
/// `HT_INVALID_IDX`: the end of a hash chain.
const INVALID: u32 = u32::MAX;
/// `sizeof(HashTable)`.
const HEADER: usize = size_of::<ArrHeader>();
/// `sizeof(Bucket)`.
const BUCKET: usize = size_of::<Bucket>();
/// `sizeof(zval)`, a packed table's slot.
const SLOT: usize = size_of::<Zval>();
/// `sizeof(uint32_t)`, a hash slot.
const HASH: usize = size_of::<u32>();

/// `HASH_FLAG_PACKED`.
const PACKED: u32 = 1 << 2;
/// `HASH_FLAG_UNINITIALIZED`: the header stands, the data does not.
const UNINITIALIZED: u32 = 1 << 3;
/// `HASH_FLAG_STATIC_KEYS`: every key is an integer or an interned
/// string, so destroying the table releases none.
const STATIC_KEYS: u32 = 1 << 4;
/// `HASH_FLAG_HAS_EMPTY_IND`: an object's property table, one of whose
/// declared slots may be empty, so `zend_array_count` counts again.
const HAS_EMPTY_IND: u32 = 1 << 5;
/// [`DeclaredRows::Copied`], in the byte php leaves unused beside
/// `u.v.flags` (`_unused`), which `set_flag_byte` leaves standing: php
/// has no bit for it, since its table says it by holding plain values
/// where it held an `IS_INDIRECT` per slot, and this table holds no row
/// for a slot at all.
const COPIED_DECLARED: u32 = 1 << 8;

/// What an object's property table holds of the object's declared
/// properties, the one fact php's table says by the type of the rows it
/// holds under their names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredRows {
    /// The slots stand for them, as php's table does with one
    /// `IS_INDIRECT` per slot (`rebuild_object_properties`): the table
    /// holds the dynamic properties alone, and a reader reads the
    /// declared ones off the object.
    Slots,
    /// Copies of them, as they stood when a table shared while it stood
    /// for the slots was separated, since `zend_array_dup` copies what
    /// each `IS_INDIRECT` points at and drops the indirection
    /// (`Z/Zend/zend_hash.c:2504-2505`): the table holds every row
    /// itself, the declared ones under their mangled names, first where
    /// a separation made the copy and wherever a sort put them where
    /// `spl_array_method` laid its sorted copy down, and a reader reads
    /// the table alone while the slots go on apart from it.
    Copied,
}

/// `zend_array`: fifty-six bytes, php's own order, out of the heap.
#[repr(C)]
pub(crate) struct ArrHeader {
    /// `zend_refcounted_h gc`.
    gc: Refcounted,
    /// `u.flags`.
    flags: Cell<u32>,
    /// `nTableMask`.
    table_mask: Cell<u32>,
    /// `arData` or `arPacked`: the buckets or the slots, which sit after
    /// the hash slots in the one data allocation. Null while
    /// uninitialised.
    data: Cell<*mut u8>,
    /// `nNumUsed`: buckets written, tombstones included.
    num_used: Cell<u32>,
    /// `nNumOfElements`: `count()`.
    num_elements: Cell<u32>,
    /// `nTableSize`.
    table_size: Cell<u32>,
    /// `nInternalPointer`.
    internal_pointer: Cell<u32>,
    /// `nNextFreeElement`.
    next_free: Cell<i64>,
    /// The declared rows the internal pointer of an object's property
    /// table has passed ([`crate::PropertyTable`]), in the half of
    /// php's `pDestructor` word this heap does not need, since its one
    /// destructor is `ZVAL_PTR_DTOR` for every array: php's table holds
    /// an `IS_INDIRECT` per declared slot before the dynamic rows and
    /// points into either with `nInternalPointer`, and this one holds
    /// the dynamic rows alone, so where php's pointer stands among the
    /// slots is kept here, as `HtIter::lead` keeps a walk's. Zero for
    /// every other table.
    lead: Cell<u32>,
    /// The rest of `pDestructor`, kept for the size.
    _dtor: u32,
}

/// `Bucket`: a zval, the hash or the integer key, and the string key or
/// none. Thirty-two bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct Bucket {
    val: Zval,
    h: u64,
    key: Option<Str>,
}

/// Where an element stands in its table, by the layout the table has:
/// a packed table's slot or a hash table's bucket, each at its position
/// below `nNumUsed`. What a lookup answers, so the layout is tested once
/// and every use of the answer knows which it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pos {
    Slot(u32),
    Bucket(u32),
}

impl Pos {
    /// The position, `idx` in php's `arData[idx]` or `arPacked[idx]`.
    const fn index(self) -> u32 {
        match self {
            Pos::Slot(idx) | Pos::Bucket(idx) => idx,
        }
    }
}

/// An array key after php's normalisation (`ZEND_HANDLE_NUMERIC_STR`
/// is the engine's to apply): an integer, or a string that is not a
/// canonical decimal integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Long(i64),
    Str(Str),
}

/// One element as `zend_hash_sort` hands it to the comparison: php's
/// `Bucket`, its key and value, with `Z_EXTRA(val)` set to the position
/// the element held before the sort, which is what the stable
/// comparators of `array.c` fall back to when two elements compare
/// equal (`RETURN_STABLE_SORT`, `stable_sort_fallback`). A view: naming
/// the value takes no owner.
#[derive(Clone, Copy, Debug)]
pub struct SortEntry {
    /// The key, as [`Arr::iter`] reads it.
    pub key: Key,
    /// The value, a view of it.
    pub val: Zval,
    /// `Z_EXTRA`: where the element stood, holes compacted, before the sort.
    pub position: u32,
}

/// `ZEND_HANDLE_NUMERIC_STR` itself (`_zend_handle_numeric_str_ex`,
/// `Z/Zend/zend_hash.h:377`): the integer a string key stands for where
/// its bytes are a canonical decimal integer in range, a `-` and no
/// leading zero, and nothing for any other string. The rule in one
/// place: the engine reads it for a key while a program is served and
/// the compiler for a constant dimension (`zend_handle_numeric_dim`).
#[must_use]
pub fn canonical_int(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, bytes),
    };
    if digits.is_empty()
        || digits.len() > 19
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits[0] == b'0')
        || (negative && digits == b"0")
    {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    text.parse().ok()
}

/// One array: php's `zend_array *`. Naming one does not keep it alive,
/// exactly as php's pointer does not; the engine's kernels take and give
/// up owners through [`crate::Heap::addref`] and [`crate::Heap::release`].
#[derive(Clone, Copy)]
pub struct Arr(NonNull<ArrHeader>);

// SAFETY: an `Arr` is the pointer php's `zend_array *` is: a request's
// array moves with the request that owns it, whole, to the thread that
// serves it (design §5.14), and an immutable one is read by every thread
// and written by none. The refcount `Cell` is what keeps it off `Sync`.
unsafe impl Send for Arr {}

/// The process's empty array, php's `zend_empty_array`
/// (`Z/Zend/zend_hash.c:260-271`): a header and no data, uninitialised
/// with `HT_MIN_MASK` and `HT_MIN_SIZE`, `nNextFreeElement` at
/// `ZEND_LONG_MIN`, and `GC_IMMUTABLE`, laid down once for the life of
/// the process beside the frozen strings and never freed.
struct FrozenEmpty(Arr);

// SAFETY: the array is written once, before the `OnceLock` in
// [`Arr::empty`] publishes it, and is immutable afterwards: every write
// to a table separates an immutable one first (`SEPARATE_ARRAY`), and
// its owners are not counted, so every thread reads it with nothing
// taken, as every thread reads php's.
unsafe impl Sync for FrozenEmpty {}
// SAFETY: see the `Sync` above.
unsafe impl Send for FrozenEmpty {}

impl Arr {
    /// Where in `zend_array` its count is, `GC_REFCOUNT`, the word whose
    /// pin sits above the count: one with nothing else in it names an
    /// array its one holder may write in place, where its zval says it is
    /// refcounted, as an immutable array's does not. What the compiled
    /// tier reads its packed arrays through (`jit-design.md` §4.3).
    pub const REFCOUNT_AT: usize = std::mem::offset_of!(ArrHeader, gc) + crate::gc::REFCOUNT_AT;
    /// Where `u.flags` is, `HT_FLAGS`.
    pub const FLAGS_AT: usize = std::mem::offset_of!(ArrHeader, flags);
    /// php's `HASH_FLAG_PACKED`, a bit of the flags at [`Arr::FLAGS_AT`].
    pub const PACKED: u32 = PACKED;
    /// Where `arPacked` is: a packed array's slots, sixteen bytes each.
    pub const DATA_AT: usize = std::mem::offset_of!(ArrHeader, data);
    /// Where `nNumUsed` is: past it a packed array has no slot.
    pub const USED_AT: usize = std::mem::offset_of!(ArrHeader, num_used);

    fn header(&self) -> &ArrHeader {
        // SAFETY: an `Arr` names an array `init` laid down, live while
        // its owner holds it, which is php's `zend_array *` contract.
        unsafe { self.0.as_ref() }
    }

    /// Where the header sits, for the heap that frees it.
    pub(crate) const fn address(self) -> NonNull<u8> {
        self.0.cast()
    }

    /// The payload word of a zval naming this array.
    pub(crate) fn payload(self) -> crate::zval::Word {
        crate::zval::Word::pointer(self.0)
    }

    /// The array a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Arr::payload`] for an array still live.
    pub(crate) unsafe fn from_payload(payload: crate::zval::Word) -> Option<Arr> {
        // SAFETY: the word holds the pointer `payload` stored, by the
        // contract.
        unsafe { payload.as_pointer::<ArrHeader>() }.map(Arr)
    }

    /// `_zend_hash_init_int` over a fresh 56-byte allocation: one owner,
    /// uninitialised, `nTableSize` from `size` through
    /// `zend_hash_check_size`, and `nNextFreeElement` at `ZEND_LONG_MIN`,
    /// which is the 8.3 rule: the first integer key, whatever its sign,
    /// sets it.
    ///
    /// # Safety
    ///
    /// `at` is writable for [`HEADER`] bytes and holds nothing else.
    unsafe fn init(at: NonNull<u8>, size: u32) -> Arr {
        let header: NonNull<ArrHeader> = at.cast();
        // SAFETY: the caller's allocation is the header and nothing else
        // holds it.
        unsafe {
            header.as_ptr().write(ArrHeader {
                gc: Refcounted::new(ZvalType::Array, 0),
                flags: Cell::new(UNINITIALIZED),
                table_mask: Cell::new(MIN_MASK),
                data: Cell::new(std::ptr::null_mut()),
                num_used: Cell::new(0),
                num_elements: Cell::new(0),
                table_size: Cell::new(size),
                internal_pointer: Cell::new(0),
                next_free: Cell::new(i64::MIN),
                lead: Cell::new(0),
                _dtor: 0,
            });
        }
        Arr(header)
    }

    /// A table destroyed under a pin left as an empty one of its kind,
    /// what `zend_array_destroy` leaves behind once its data is freed:
    /// no data, no rows and no recursion mark, the type `IS_ARRAY` again and the
    /// count and the pin kept. So what names it through its pin reads an
    /// empty table and never freed memory, and an owner taken of it
    /// since gives it up as any table (design §4.4, #2230).
    pub(crate) fn emptied(self) {
        let header = self.header();
        header.gc.set_kind(ZvalType::Array);
        header.gc.del_flags(gc::PROTECTED);
        header.flags.set(UNINITIALIZED);
        header.table_mask.set(MIN_MASK);
        header.data.set(std::ptr::null_mut());
        header.num_used.set(0);
        header.num_elements.set(0);
        header.internal_pointer.set(0);
        header.next_free.set(i64::MIN);
        header.lead.set(0);
    }

    /// The end of `zend_hash_clean`: in one pass over the positions used,
    /// every string key given up, then the hash slots reset and the
    /// counts, the internal pointer and `nNextFreeElement` back to an
    /// empty table's, the data and its size kept. What the rows still
    /// held that is counted is handed back to be given up once the table
    /// is empty, so a destructor it serves reads an empty table.
    fn emptied_in_place(self, heap: &mut Heap) -> Vec<Zval> {
        let mut left = Vec::new();
        let used = self.num_used();
        if self.is_packed() {
            for idx in 0..used {
                let z = self.read_slot(idx);
                if z.is_refcounted() {
                    left.push(z);
                }
            }
        } else if !self.is_uninitialized() {
            for idx in 0..used {
                let b = self.read_bucket(idx);
                if b.val.is_undef() {
                    continue;
                }
                if let Some(k) = b.key {
                    heap.release_string(k);
                }
                if b.val.is_refcounted() {
                    left.push(b.val);
                }
            }
            self.hash_reset();
        }
        let header = self.header();
        header.num_used.set(0);
        header.num_elements.set(0);
        header.internal_pointer.set(0);
        header.next_free.set(i64::MIN);
        header.lead.set(0);
        left
    }

    /// `ZVAL_EMPTY_ARRAY`: the process's one immutable empty array,
    /// php's `zend_empty_array`, which costs a request nothing and is
    /// what php answers wherever an array has nothing in it
    /// (`RETURN_EMPTY_ARRAY`). No owner is counted on it, and a write
    /// separates it as it separates any immutable array, into a table
    /// of the request's own.
    #[must_use]
    pub fn empty() -> Arr {
        static EMPTY: OnceLock<FrozenEmpty> = OnceLock::new();
        EMPTY
            .get_or_init(|| {
                // The header for the life of the process: leaked, as
                // php's is a `static const` in the binary.
                let at = NonNull::from(Box::leak(Box::new(
                    std::mem::MaybeUninit::<ArrHeader>::uninit(),
                )));
                // SAFETY: the allocation is a header's size and alignment
                // and nothing else holds it.
                let a = unsafe { Arr::init(at.cast(), MIN_SIZE) };
                a.header().gc.add_flags(gc::IMMUTABLE);
                FrozenEmpty(a)
            })
            .0
    }

    /// `zend_hash_num_elements`, `count()`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.header().num_elements.get() as usize
    }

    /// Whether `count()` is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.header().num_elements.get() == 0
    }

    /// `nNextFreeElement`: where `$a[] = v` writes next, or
    /// `ZEND_LONG_MIN` while no integer key has set it.
    #[must_use]
    pub fn next_free(&self) -> i64 {
        self.header().next_free.get()
    }

    /// `nTableSize`: the buckets or slots allocated, once any are.
    #[must_use]
    pub fn table_size(&self) -> u32 {
        self.header().table_size.get()
    }

    /// `HT_FLAGS(ht)`, the whole `u.flags` word: php's `u.v.flags` in
    /// its low byte, and the iterator count in its third byte.
    pub(crate) fn flags(self) -> u32 {
        self.header().flags.get()
    }

    pub(crate) fn set_flags(self, flags: u32) {
        self.header().flags.set(flags);
    }

    /// `ht->u.v.flags = flags`: the low byte written and the iterator
    /// count beside it left standing ("Don't overwrite iterator count",
    /// `Z/Zend/zend_hash.c:161`).
    fn set_flag_byte(self, flags: u32) {
        self.set_flags((self.flags() & !0xff) | flags);
    }

    /// `HT_IS_PACKED`: the slots are the integer keys `0..nNumUsed`.
    #[must_use]
    pub fn is_packed(&self) -> bool {
        self.header().flags.get() & PACKED != 0
    }

    /// `HT_FLAGS(ht) & HASH_FLAG_HAS_EMPTY_IND`: whether an object's
    /// property table may stand for a declared slot that is empty, which
    /// `rebuild_object_properties` sets where it finds one and
    /// `zend_std_unset_property` sets once the value it emptied a slot
    /// of is given up.
    #[must_use]
    pub fn has_empty_ind(&self) -> bool {
        self.header().flags.get() & HAS_EMPTY_IND != 0
    }

    /// `HASH_FLAG_HAS_EMPTY_IND` set, or cleared as `zend_array_count`
    /// clears it once a count finds no slot empty.
    pub fn set_has_empty_ind(&self, on: bool) {
        let flags = self.header().flags.get();
        self.header().flags.set(if on {
            flags | HAS_EMPTY_IND
        } else {
            flags & !HAS_EMPTY_IND
        });
    }

    /// What an object's property table holds of the declared
    /// properties: [`DeclaredRows::Slots`] for every table but the copy
    /// [`crate::Heap::object_properties_copy`] makes and the copies made
    /// of that.
    #[must_use]
    pub fn declared_rows(&self) -> DeclaredRows {
        if self.header().flags.get() & COPIED_DECLARED == 0 {
            DeclaredRows::Slots
        } else {
            DeclaredRows::Copied
        }
    }

    /// The table marked as holding `rows`, what the separation of an
    /// object's table carries onto the copy it makes of one that holds
    /// its declared rows already.
    pub fn set_declared_rows(&self, rows: DeclaredRows) {
        let flags = self.header().flags.get();
        self.header().flags.set(match rows {
            DeclaredRows::Slots => flags & !COPIED_DECLARED,
            DeclaredRows::Copied => flags | COPIED_DECLARED,
        });
    }

    fn is_uninitialized(self) -> bool {
        self.header().flags.get() & UNINITIALIZED != 0
    }

    /// `zend_refcounted_h gc`: the header the cycle collector colours.
    pub(crate) fn gc(self) -> Counted {
        // SAFETY: the block starts with its header, and stands while
        // its owner holds the handle, as `header` reads it.
        unsafe { Counted::new(self.address()) }
    }

    /// What the collector walks of a table (`Z/Zend/zend_gc.c:1161-1199`):
    /// the value of every position below `nNumUsed`, in order, an
    /// `IS_INDIRECT` one followed to the zval it names; a tombstone is
    /// `UNDEF`, which is never collectable.
    pub(crate) fn gc_values(self, out: &mut Vec<Zval>) {
        if self.is_uninitialized() {
            return;
        }
        for i in 0..self.num_used() {
            let z = self.value_at(i);
            out.push(z.as_indirect().map_or(z, ZvalRef::read));
        }
    }

    /// `IS_ARRAY_IMMUTABLE`: a compiled program's constant array, never
    /// refcounted, separated before any write.
    #[must_use]
    pub fn is_immutable(&self) -> bool {
        self.header().gc.flags() & gc::IMMUTABLE != 0
    }

    /// `zend_array_refcount`: one for an immutable array whatever its
    /// header says.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        if self.is_immutable() {
            return 1;
        }
        self.header().gc.refcount()
    }

    /// `GC_ADDREF`, and nothing for an immutable array.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(&self) {
        if !self.is_immutable() {
            self.gc().addref();
        }
    }

    /// `GC_IS_RECURSIVE`: a walk that protects itself is inside this
    /// array already, which is how `count()`, `var_dump()` and their
    /// kin see an array that holds itself.
    #[must_use]
    pub fn is_recursive(&self) -> bool {
        self.header().gc.flags() & gc::PROTECTED != 0
    }

    /// `GC_PROTECT_RECURSION`: the walk is inside. php does not mark an
    /// immutable array, since nothing can make one hold itself, and
    /// neither does this.
    pub fn protect(&self) {
        if !self.is_immutable() {
            self.header().gc.add_flags(gc::PROTECTED);
        }
    }

    /// `GC_UNPROTECT_RECURSION`: the walk is out.
    pub fn unprotect(&self) {
        if !self.is_immutable() {
            self.header().gc.del_flags(gc::PROTECTED);
        }
    }

    /// `GC_DELREF`, and what is left; an immutable array keeps its one.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn delref(self) -> u32 {
        if self.is_immutable() {
            return 1;
        }
        self.gc().delref()
    }

    // ------------------------------------------------------ the data

    /// `nNumUsed`: the positions written, tombstones included, which
    /// is where `zend_close_rsrc_list` starts its walk down and where
    /// `ZEND_HASH_FOREACH` fixes its end.
    #[must_use]
    pub fn num_used(self) -> u32 {
        self.header().num_used.get()
    }

    /// `ht->nNextFreeElement = n`: what `zend_init_rsrc_list`
    /// (`Z/Zend/zend_list.c:204-208`) writes over the `ZEND_LONG_MIN` the
    /// init left, so the first resource's id is one and not the 8.3
    /// rule's, and what `array_pop()` writes one back
    /// (`Z/ext/standard/array.c:3549`) where the element it took was the
    /// last one appended.
    pub fn set_next_free(self, n: i64) {
        self.header().next_free.set(n);
    }

    fn mask(self) -> u32 {
        self.header().table_mask.get()
    }

    /// `HT_HASH_SIZE(mask)`: the bytes the hash slots take.
    const fn hash_size(mask: u32) -> usize {
        mask.wrapping_neg() as usize * HASH
    }

    /// `HT_SIZE_TO_MASK`.
    const fn size_to_mask(size: u32) -> u32 {
        size.wrapping_add(size).wrapping_neg()
    }

    /// `HT_SIZE_EX(size, HT_SIZE_TO_MASK(size))`: a hash table's data.
    const fn data_size(size: u32) -> usize {
        size as usize * BUCKET + Arr::hash_size(Arr::size_to_mask(size))
    }

    /// `HT_PACKED_SIZE_EX(size, HT_MIN_MASK)`: a packed table's data.
    const fn packed_size(size: u32) -> usize {
        size as usize * SLOT + Arr::hash_size(MIN_MASK)
    }

    /// `HT_GET_DATA_ADDR`: the allocation, which starts at the hash slots.
    fn data_addr(self) -> Option<NonNull<u8>> {
        let data = NonNull::new(self.header().data.get())?;
        let hash = Arr::hash_size(self.mask());
        // SAFETY: the data pointer sits `hash` bytes into its allocation.
        Some(unsafe { data.sub(hash) })
    }

    /// `HT_SET_DATA_ADDR`: the allocation `at`, the buckets past its hash
    /// slots.
    fn set_data_addr(self, at: NonNull<u8>) {
        let hash = Arr::hash_size(self.mask());
        // SAFETY: the allocation holds the hash slots and then the data.
        self.header().data.set(unsafe { at.add(hash) }.as_ptr());
    }

    /// The bucket at `idx` of a hash table, which is below `nTableSize`.
    fn bucket(self, idx: u32) -> NonNull<Bucket> {
        let data = self.header().data.get().cast::<Bucket>();
        // SAFETY: `idx` is below the table size, so the bucket is inside
        // the data allocation.
        let at = unsafe { data.add(idx as usize) };
        // SAFETY: a table with buckets is initialised, and an initialised
        // table's data pointer is its allocation, never null; only an
        // uninitialised one's is, and it has no bucket to name.
        unsafe { NonNull::new_unchecked(at) }
    }

    fn read_bucket(self, idx: u32) -> Bucket {
        // SAFETY: the bucket is inside the allocation and was written by
        // an insert or a rehash, or is an `UNDEF` tombstone.
        unsafe { self.bucket(idx).read() }
    }

    fn write_bucket(self, idx: u32, b: Bucket) {
        // SAFETY: the bucket is inside the allocation; nothing else
        // borrows it, since every read copies.
        unsafe { self.bucket(idx).write(b) };
    }

    /// The slot at `idx` of a packed table, which is below `nTableSize`.
    fn slot(self, idx: u32) -> NonNull<Zval> {
        let data = self.header().data.get().cast::<Zval>();
        // SAFETY: `idx` is below the table size, so the slot is inside
        // the data allocation.
        let at = unsafe { data.add(idx as usize) };
        // SAFETY: a packed table is initialised (`zend_hash_real_init_packed`
        // lays its data down before it sets the flag), and an initialised
        // table's data pointer is its allocation, never null.
        unsafe { NonNull::new_unchecked(at) }
    }

    fn read_slot(self, idx: u32) -> Zval {
        // SAFETY: the slot is inside the allocation and was written, or
        // is an `UNDEF` hole.
        unsafe { self.slot(idx).read() }
    }

    fn write_slot(self, idx: u32, z: Zval) {
        // SAFETY: the slot is inside the allocation; nothing else borrows
        // it, since every read copies.
        unsafe { self.slot(idx).write(z) };
    }

    /// `HT_HASH(ht, n_index)`: the slot at the signed index `n_index`,
    /// which is negative and counts back from the buckets.
    fn hash_slot(self, n_index: u32) -> NonNull<u32> {
        let data = self.header().data.get().cast::<u32>();
        // SAFETY: `n_index | mask` is in `mask..=u32::MAX`, so as a signed
        // offset it stays inside the hash slots before the buckets.
        let at = unsafe { data.offset(n_index.cast_signed() as isize) };
        NonNull::new(at).unwrap_or(NonNull::dangling())
    }

    fn read_hash(self, n_index: u32) -> u32 {
        // SAFETY: the slot is inside the allocation and was reset.
        unsafe { self.hash_slot(n_index).read() }
    }

    fn write_hash(self, n_index: u32, idx: u32) {
        // SAFETY: the slot is inside the allocation; nothing else borrows
        // it.
        unsafe { self.hash_slot(n_index).write(idx) };
    }

    /// `HT_HASH_RESET`: every hash slot to `HT_INVALID_IDX`.
    fn hash_reset(self) {
        if let Some(at) = self.data_addr() {
            let slots = self.mask().wrapping_neg() as usize;
            // SAFETY: the allocation starts with `slots` hash words.
            unsafe { at.cast::<u32>().write_bytes(0xff, slots) };
        }
    }

    /// The value of bucket or slot `idx`, whichever layout holds it.
    fn value_at(self, idx: u32) -> Zval {
        if self.is_packed() {
            self.read_slot(idx)
        } else {
            self.read_bucket(idx).val
        }
    }

    /// The key of bucket or slot `idx`.
    fn key_at(self, idx: u32) -> Key {
        if self.is_packed() {
            Key::Long(i64::from(idx))
        } else {
            let b = self.read_bucket(idx);
            match b.key {
                Some(s) => Key::Str(s),
                None => Key::Long(b.h.cast_signed()),
            }
        }
    }

    /// `zend_hash_find_bucket`: the bucket of a string key, by hash
    /// then bytes, php's chain walk. The table is a hash table.
    fn find_str(self, key: Str) -> Option<u32> {
        let h = key.hash();
        let mut idx = self.read_hash((h as u32) | self.mask());
        while idx != INVALID {
            let b = self.read_bucket(idx);
            if b.h == h
                && b.key
                    .is_some_and(|k| k.is_same(key) || k.bytes() == key.bytes())
            {
                return Some(idx);
            }
            idx = b.val.next();
        }
        None
    }

    /// `zend_hash_str_find_bucket`: the bucket of the string key whose
    /// hash is `h` and whose bytes are `bytes`, none in a packed or an
    /// uninitialised table, whose two hash slots name no bucket.
    fn find_bucket_bytes(self, h: u64, bytes: &[u8]) -> Option<u32> {
        if self.is_uninitialized() || self.is_packed() {
            return None;
        }
        let mut idx = self.read_hash((h as u32) | self.mask());
        while idx != INVALID {
            let b = self.read_bucket(idx);
            if b.h == h && b.key.is_some_and(|k| k.bytes() == bytes) {
                return Some(idx);
            }
            idx = b.val.next();
        }
        None
    }

    /// `zend_hash_str_find(ht, str, len)` (`Z/Zend/zend_hash.c`): the
    /// value under the string key of these bytes, copied, no owner
    /// taken, found by the bytes' hash and a walk of its chain, so the
    /// lookup allocates nothing. The bytes are a key as written: no
    /// integer is read out of them, as `zend_symtable_str_find` would.
    #[must_use]
    pub fn str_find(&self, bytes: &[u8]) -> Option<Zval> {
        let idx = self.find_bucket_bytes(crate::string::hash_func(bytes), bytes)?;
        Some(self.read_bucket(idx).val)
    }

    /// `zend_hash_index_find_bucket`: the bucket of an integer key. The
    /// table is a hash table.
    fn find_long(self, h: u64) -> Option<u32> {
        let mut idx = self.read_hash((h as u32) | self.mask());
        while idx != INVALID {
            let b = self.read_bucket(idx);
            if b.h == h && b.key.is_none() {
                return Some(idx);
            }
            idx = b.val.next();
        }
        None
    }

    /// Where `key` stands, `ZEND_HASH_INDEX_FIND`'s shape
    /// (`Z/Zend/zend_execute.c`, `zend_fetch_dimension_address_inner`)
    /// and `zend_hash_find`'s: the layout is tested once. A packed table
    /// is `HASH_FLAG_PACKED`, the bound against `nNumUsed` and the slot's
    /// `UNDEF` test; an uninitialised table is not marked packed and has
    /// `nNumUsed` at zero, so the bound alone answers for it under an
    /// integer key, as php's `uninitialized_bucket` answers for it under
    /// any. A hash table is its chain walk.
    ///
    /// ```php
    /// $p = [1, 2, 3]; unset($p[1]); $h = ["k" => 1, 7 => 2]; $u = [];
    /// var_dump(array_key_exists(0, $p), array_key_exists(1, $p), array_key_exists(5, $p), array_key_exists("0", $p), array_key_exists("k", $p), array_key_exists("k", $h), array_key_exists(7, $h), array_key_exists(8, $h), array_key_exists(0, $u), array_key_exists("k", $u));
    /// unset($p[2], $h["k"], $h[9], $u[0]);
    /// var_dump($p, $h, $u);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// array(1) {
    ///   [7]=>
    ///   int(2)
    /// }
    /// array(0) {
    /// }
    /// ```
    #[inline(always)] // the packed test and its slot, at every element access
    #[allow(clippy::inline_always)]
    fn find(self, key: Key) -> Option<Pos> {
        if let Key::Long(n) = key
            && self.flags() & PACKED != 0
        {
            let h = n.cast_unsigned();
            if h >= u64::from(self.num_used()) {
                return None;
            }
            let idx = h as u32;
            return (!self.read_slot(idx).is_undef()).then_some(Pos::Slot(idx));
        }
        self.find_hashed(key)
    }

    /// [`Arr::find`] past a packed table's integer key, out of line as
    /// `zend_hash_find` and `zend_hash_index_find` are, so the packed
    /// test and its slot stay small enough to inline at every element
    /// access: a string key over a packed table, an uninitialised table,
    /// and a hash table's chain walk.
    #[inline(never)]
    fn find_hashed(self, key: Key) -> Option<Pos> {
        let flags = self.flags();
        match key {
            _ if flags & (UNINITIALIZED | PACKED) != 0 => None,
            Key::Long(n) => self.find_long(n.cast_unsigned()).map(Pos::Bucket),
            Key::Str(s) => self.find_str(s).map(Pos::Bucket),
        }
    }

    /// The value at `pos` in place: the slot itself, or the bucket's
    /// `val`, its first field, so `&p->val` is the bucket's address.
    #[inline]
    fn value_ptr(self, pos: Pos) -> NonNull<Zval> {
        match pos {
            Pos::Slot(idx) => self.slot(idx),
            Pos::Bucket(idx) => self.bucket(idx).cast::<Zval>(),
        }
    }

    /// `zend_hash_find` and `zend_hash_index_find`: the value at `key`,
    /// copied, no owner taken.
    #[must_use]
    #[inline]
    pub fn get(&self, key: Key) -> Option<Zval> {
        self.value_ref(key).map(ZvalRef::read)
    }

    /// `zend_hash_exists`, `array_key_exists`.
    #[must_use]
    #[inline]
    pub fn contains(&self, key: Key) -> bool {
        self.find(key).is_some()
    }

    /// The `zval *` `zend_hash_find` hands back, the entry in place:
    /// the bucket's `val` in a hash table and the slot in a packed one,
    /// which is what `zend_std_get_property_ptr_ptr` returns for a
    /// dynamic property and what `FETCH_OBJ_W` leaves in its `VAR`.
    /// None where the key is not there.
    ///
    /// **A name into a table is a name until the table is written.**
    /// Every insert may rehash or move the data allocation, as php's
    /// may, so a `ZvalRef` from here is held across the one opline that
    /// asked for it and the `ASSIGN*` beside it and no further; php
    /// holds `zval *` under the same rule.
    #[must_use]
    #[inline(always)] // as `find`: a dimension handler's inline path reads through it
    #[allow(clippy::inline_always)]
    pub fn value_ref(&self, key: Key) -> Option<ZvalRef> {
        Some(ZvalRef::new(self.value_ptr(self.find(key)?)))
    }

    /// Where the value at position `pos` stands, php's
    /// `(zval *)((char *)ht->arData + offset)`, which is how
    /// `ZEND_BIND_STATIC` reaches a slot of a `static` table by the
    /// offset the compiler recorded. The bucket is answered whatever it
    /// holds, `UNDEF` among it, which `zend_closure_bind_var_ex` writes
    /// for an arrow function's capture of a variable nothing defined.
    /// None past `nNumUsed`. The rule [`Arr::value_ref`] states holds.
    #[must_use]
    pub fn value_ref_at(&self, pos: u32) -> Option<ZvalRef> {
        if pos >= self.num_used() {
            return None;
        }
        let at = if self.is_packed() {
            self.slot(pos)
        } else {
            self.bucket(pos).cast::<Zval>()
        };
        Some(ZvalRef::new(at))
    }

    // ------------------------------------------- the internal pointer

    /// `_zend_hash_get_valid_pos`: `pos` or the first live position
    /// after it, `nNumUsed` when there is none.
    fn valid_pos(self, mut pos: u32) -> u32 {
        let used = self.num_used();
        while pos < used && self.value_at(pos).is_undef() {
            pos += 1;
        }
        pos
    }

    /// `nInternalPointer` as it was written, no tombstone stepped over.
    pub(crate) fn raw_pointer(self) -> u32 {
        self.header().internal_pointer.get()
    }

    /// `nInternalPointer = pos`.
    pub(crate) fn set_raw_pointer(self, pos: u32) {
        self.header().internal_pointer.set(pos);
    }

    /// The declared rows an object table's pointer has passed
    /// (`ArrHeader::lead`).
    pub(crate) fn lead(self) -> u32 {
        self.header().lead.get()
    }

    /// The pointer of an object's table set among its declared rows.
    pub(crate) fn set_lead(self, lead: u32) {
        self.header().lead.set(lead);
    }

    /// Whether position `pos` holds a bucket, php's
    /// `Z_TYPE(p->val) != IS_UNDEF`: below `nNumUsed` and no tombstone.
    pub(crate) fn is_live(self, pos: u32) -> bool {
        pos < self.num_used() && !self.value_at(pos).is_undef()
    }

    /// `zend_hash_get_current_pos`: where the internal pointer stands,
    /// past any tombstone.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.valid_pos(self.header().internal_pointer.get())
    }

    /// The key and the value at `pos`, if `pos` is a live position.
    #[must_use]
    pub fn entry_at(&self, pos: u32) -> Option<(Key, Zval)> {
        if pos >= self.num_used() {
            return None;
        }
        let z = self.value_at(pos);
        if z.is_undef() {
            return None;
        }
        Some((self.key_at(pos), z))
    }

    /// The scan `ZEND_FE_FETCH_R` runs from a position: the first live
    /// position at or after `pos`, with its key and value, or `None`
    /// when only tombstones stand between `pos` and `nNumUsed`. The
    /// value is copied and no owner taken. The key of a packed array's
    /// element is its position, as `ZVAL_LONG(result, pos)` reads it.
    #[must_use]
    pub fn live_from(&self, mut pos: u32) -> Option<(u32, Key, Zval)> {
        let used = self.num_used();
        while pos < used {
            let z = self.value_at(pos);
            if !z.is_undef() {
                return Some((pos, self.key_at(pos), z));
            }
            pos += 1;
        }
        None
    }

    /// `zend_hash_internal_pointer_reset`, `reset()`.
    pub fn reset(&self) {
        self.header().internal_pointer.set(self.valid_pos(0));
    }

    /// `zend_hash_internal_pointer_end`, `end()`: the last live position,
    /// or `nNumUsed` when there is none.
    pub fn end(&self) {
        let mut idx = self.num_used();
        while idx > 0 {
            idx -= 1;
            if !self.value_at(idx).is_undef() {
                self.header().internal_pointer.set(idx);
                return;
            }
        }
        self.header().internal_pointer.set(self.num_used());
    }

    /// `zend_hash_move_forward`, `next()`: one live position on, or past
    /// the end; `false` when the pointer was already past it.
    pub fn forward(&self) -> bool {
        let used = self.num_used();
        let mut idx = self.position();
        if idx >= used {
            return false;
        }
        loop {
            idx += 1;
            if idx >= used {
                self.header().internal_pointer.set(used);
                return true;
            }
            if !self.value_at(idx).is_undef() {
                self.header().internal_pointer.set(idx);
                return true;
            }
        }
    }

    /// `zend_hash_move_backwards`, `prev()`: one live position back, or
    /// past the end when there is none before it; `false` when the
    /// pointer was already past the end.
    pub fn backward(&self) -> bool {
        let used = self.num_used();
        let mut idx = self.header().internal_pointer.get();
        if idx >= used {
            return false;
        }
        while idx > 0 {
            idx -= 1;
            if !self.value_at(idx).is_undef() {
                self.header().internal_pointer.set(idx);
                return true;
            }
        }
        self.header().internal_pointer.set(used);
        true
    }

    /// The elements in order, `ZEND_HASH_FOREACH_KEY_VAL`: every live
    /// position, tombstones skipped, values copied and no owner taken.
    #[must_use]
    pub fn iter(&self) -> ArrIter {
        ArrIter { arr: *self, pos: 0 }
    }

    // ------------------------------------------------------- writes

    /// `zend_hash_real_init_packed_ex`.
    fn real_init_packed(self, mm: &mut dyn Allocate) -> Result<(), Fatal> {
        let at = mm.alloc(Arr::packed_size(self.table_size()))?;
        self.header().table_mask.set(MIN_MASK);
        self.set_data_addr(at);
        // php real-inits an object's table where it rebuilds it, before
        // it sets the flag; this table is initialised at its first
        // dynamic property, so the flag a rebuild set stands.
        self.set_flag_byte(PACKED | STATIC_KEYS | (self.flags() & HAS_EMPTY_IND));
        self.hash_reset();
        Ok(())
    }

    /// `zend_hash_real_init_mixed_ex`.
    fn real_init_mixed(self, mm: &mut dyn Allocate) -> Result<(), Fatal> {
        let size = self.table_size();
        let at = mm.alloc(Arr::data_size(size))?;
        self.header().table_mask.set(Arr::size_to_mask(size));
        self.set_data_addr(at);
        self.set_flag_byte(STATIC_KEYS | (self.flags() & HAS_EMPTY_IND));
        self.hash_reset();
        Ok(())
    }

    /// `zend_hash_packed_to_hash`: the same elements as buckets, the
    /// larger allocation taken before the smaller goes back, then the
    /// rehash that links them.
    fn packed_to_hash(self, mm: &mut dyn Allocate) -> Result<(), Fatal> {
        let size = self.table_size();
        let old = self.data_addr();
        let new = mm.alloc(Arr::data_size(size))?;
        let used = self.num_used();
        let slots: Vec<Zval> = (0..used).map(|i| self.read_slot(i)).collect();
        self.header().flags.set(self.header().flags.get() & !PACKED);
        self.header().table_mask.set(Arr::size_to_mask(size));
        self.set_data_addr(new);
        for (i, val) in slots.into_iter().enumerate() {
            self.write_bucket(
                i as u32,
                Bucket {
                    val,
                    h: i as u64,
                    key: None,
                },
            );
        }
        if let Some(old) = old {
            // SAFETY: the old data was this heap's and nothing names it
            // now that the buckets hold the elements.
            unsafe { mm.free(old) };
        }
        self.rehash(mm.iterators());
        Ok(())
    }

    /// `zend_hash_packed_grow` (`Z/Zend/zend_hash.c:310`): twice the
    /// slots, php's `perealloc2` over the used part of the data, so a
    /// table whose data is a `run` of pages grows where it stands when
    /// the pages after it are free and `memory_get_peak_usage()` never
    /// holds the two sizes at once (#278).
    ///
    /// A list appended to from empty moves `memory_get_usage()` at its
    /// first element and at each doubling past eight, by php's numbers
    /// (#2661): the 56-byte header and data of eight slots of sixteen
    /// bytes and two of hash, each size rounded up to its bin to 3072
    /// bytes, past that to a `run` of whole pages, and past the 2 MB chunk
    /// to a huge block. The program writes its first line before it
    /// measures, since the first byte a request writes holds the
    /// thirty-two of the default mimetype.
    ///
    /// ```php
    /// echo "appended, moved\n"; $a = []; $d = $p = 0; $m = memory_get_usage();
    /// for ($i = 1; $i <= 1100; $i++) { $a[] = $i; $d = memory_get_usage() - $m; if ($d !== $p) echo $i, ' ', $d, "\n"; $p = $d; }
    /// $m = memory_get_usage(); $b = []; for ($i = 0; $i < 100000; $i++) $b[] = $i; echo 100000, ' ', memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect capture
    /// appended, moved
    /// 1 216
    /// 9 376
    /// 17 696
    /// 33 1336
    /// 65 2616
    /// 129 8248
    /// 257 12344
    /// 513 20536
    /// 1025 36920
    /// 100000 2113616
    /// ```
    fn packed_grow(self, mm: &mut dyn Allocate) -> Result<(), Fatal> {
        let size = self.table_size();
        if size >= MAX_SIZE {
            return Err(overflow(size.wrapping_mul(2), BUCKET));
        }
        let grown = size * 2;
        let used_bytes = Arr::hash_size(MIN_MASK) + self.num_used() as usize * SLOT;
        let new = match self.data_addr() {
            // SAFETY: the data is this allocator's, its first `used_bytes`
            // are the hash and the used slots, and the header is its only
            // name, replaced below.
            Some(old) => unsafe { mm.realloc(old, used_bytes, Arr::packed_size(grown)) }?,
            None => mm.alloc(Arr::packed_size(grown))?,
        };
        self.set_data_addr(new);
        self.header().table_size.set(grown);
        Ok(())
    }

    /// `zend_hash_do_resize`: compact when a thirty-second of the table
    /// is tombstones, else double it, the used buckets copied and
    /// relinked.
    ///
    /// A hash appended to from empty moves `memory_get_usage()` at its
    /// first element and at each doubling past eight (#2661): the header
    /// and data of `HT_SIZE_EX`, forty bytes a bucket and four of hash
    /// for each of twice as many slots, rounded up as a list's data is
    /// ([`Arr::packed_grow`]). Under negative integer keys nothing else is
    /// allocated; under string keys each key's string is counted beside
    /// its bucket.
    ///
    /// ```php
    /// echo "inserted, moved\n"; $h = []; $d = $p = 0; $m = memory_get_usage();
    /// for ($i = 1; $i <= 1100; $i++) { $h[-$i] = $i; $d = memory_get_usage() - $m; if ($d !== $p) echo $i, ' ', $d, "\n"; $p = $d; }
    /// foreach ([1, 8, 9, 16, 17, 64, 65, 1000, 100000] as $n) {
    ///     $m = memory_get_usage(); $k = []; for ($i = 0; $i < $n; $i++) $k["k$i"] = $i;
    ///     echo "k$n ", memory_get_usage() - $m, "\n"; unset($k);
    /// }
    /// ```
    /// ```expect capture
    /// inserted, moved
    /// 1 376
    /// 9 696
    /// 17 1336
    /// 33 2616
    /// 65 8248
    /// 129 12344
    /// 257 20536
    /// 513 41016
    /// 1025 81976
    /// k1 408
    /// k8 632
    /// k9 984
    /// k16 1208
    /// k17 1880
    /// k64 4664
    /// k65 10328
    /// k1000 73016
    /// k100000 8442960
    /// ```
    fn do_resize(self, mm: &mut dyn Allocate) -> Result<(), Fatal> {
        let used = self.num_used();
        let elements = self.header().num_elements.get();
        let size = self.table_size();
        if used > elements + (elements >> 5) {
            self.rehash(mm.iterators());
        } else if size < MAX_SIZE {
            let grown = size + size;
            let old = self.data_addr();
            let new = mm.alloc(Arr::data_size(grown))?;
            let buckets: Vec<Bucket> = (0..used).map(|i| self.read_bucket(i)).collect();
            self.header().table_size.set(grown);
            self.header().table_mask.set(Arr::size_to_mask(grown));
            self.set_data_addr(new);
            for (i, b) in buckets.into_iter().enumerate() {
                self.write_bucket(i as u32, b);
            }
            if let Some(old) = old {
                // SAFETY: the old data was this heap's and its buckets
                // are copied.
                unsafe { mm.free(old) };
            }
            self.rehash(mm.iterators());
        } else {
            return Err(overflow(size.wrapping_mul(2), BUCKET + HASH));
        }
        Ok(())
    }

    /// Link bucket `idx` at the head of its hash chain.
    fn link(self, idx: u32) {
        let mut b = self.read_bucket(idx);
        let n_index = (b.h as u32) | self.mask();
        b.val.set_next(self.read_hash(n_index));
        self.write_bucket(idx, b);
        self.write_hash(n_index, idx);
    }

    /// `zend_hash_rehash` (`Z/Zend/zend_hash.c:1334`): the chains rebuilt
    /// over the buckets, and the tombstones compacted out with the
    /// internal pointer moved past the ones it stood on or after; every
    /// iterator on the table moved with the bucket it stood at, and the
    /// ones past the end moved to the new end, so an append after the
    /// rehash is walked.
    fn rehash(self, mut iterators: Option<&mut Iterators>) {
        let header = self.header();
        if header.num_elements.get() == 0 {
            if !self.is_uninitialized() {
                header.num_used.set(0);
                self.hash_reset();
                header.internal_pointer.set(0);
                if let Some(iterators) = iterators {
                    iterators.reset(self);
                }
            }
            return;
        }
        self.hash_reset();
        let used = self.num_used();
        let mut i = 0;
        while i < used {
            if self.read_bucket(i).val.is_undef() {
                let mut j = i;
                let walked = self.has_iterators();
                let mut iter_pos = match iterators.as_deref_mut() {
                    Some(iterators) if walked => iterators.lower_pos(self, i + 1),
                    _ => u32::MAX,
                };
                i += 1;
                while i < used {
                    let b = self.read_bucket(i);
                    if !b.val.is_undef() {
                        self.write_bucket(j, b);
                        self.link(j);
                        let pointer = header.internal_pointer.get();
                        if pointer > j && pointer <= i {
                            header.internal_pointer.set(j);
                        }
                        if walked
                            && i >= iter_pos
                            && let Some(iterators) = iterators.as_deref_mut()
                        {
                            loop {
                                iterators.update(self, iter_pos, j);
                                iter_pos = iterators.lower_pos(self, iter_pos + 1);
                                if iter_pos >= i {
                                    break;
                                }
                            }
                        }
                        j += 1;
                    }
                    i += 1;
                }
                header.num_used.set(j);
                if let Some(iterators) = iterators {
                    iterators.update(self, used, j);
                }
                return;
            }
            self.link(i);
            i += 1;
        }
    }

    /// `_zend_hash_del_el_ex` after the unlink: one element fewer, the
    /// trailing tombstones dropped off `nNumUsed`, the internal pointer
    /// clamped, and the value handed back for release.
    fn del_at(self, idx: u32, iterators: Option<&mut Iterators>) -> Zval {
        let header = self.header();
        header.num_elements.set(header.num_elements.get() - 1);
        let z = self.value_at(idx);
        if self.is_packed() {
            self.write_slot(idx, Zval::UNDEF);
        } else {
            let mut b = self.read_bucket(idx);
            b.val = Zval::UNDEF;
            b.key = None;
            self.write_bucket(idx, b);
        }
        if header.num_used.get() - 1 == idx {
            loop {
                header.num_used.set(header.num_used.get() - 1);
                let used = header.num_used.get();
                if used == 0 || !self.value_at(used - 1).is_undef() {
                    break;
                }
            }
            let used = header.num_used.get();
            header
                .internal_pointer
                .set(header.internal_pointer.get().min(used));
            if let Some(iterators) = iterators {
                iterators.clamp_max(self, used);
            }
        }
        z
    }

    /// The `add_to_packed` arm: the slots between `nNumUsed` and `h`
    /// become holes, and `h + 1` is both the used count and the next
    /// free element.
    fn add_to_packed(self, h: u32, z: Zval) {
        let header = self.header();
        let used = self.num_used();
        for hole in used..h {
            self.write_slot(hole, Zval::UNDEF);
        }
        self.write_slot(h, z);
        header.num_used.set(h + 1);
        header.next_free.set(i64::from(h) + 1);
        header.num_elements.set(header.num_elements.get() + 1);
    }

    /// Whether this is the one array `other` is, php's `ht1 == ht2`.
    #[must_use]
    pub fn is_same(self, other: Arr) -> bool {
        self.0 == other.0
    }

    /// `_zend_hash_del_el`: unlink bucket `idx` from its chain.
    fn unlink(self, idx: u32) {
        let b = self.read_bucket(idx);
        let n_index = (b.h as u32) | self.mask();
        let mut i = self.read_hash(n_index);
        if i == idx {
            self.write_hash(n_index, b.val.next());
            return;
        }
        while i != INVALID {
            let mut prev = self.read_bucket(i);
            if prev.val.next() == idx {
                prev.val.set_next(b.val.next());
                self.write_bucket(i, prev);
                return;
            }
            i = prev.val.next();
        }
    }
}

impl std::fmt::Debug for Arr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Arr({} of {}, {})",
            self.len(),
            self.table_size(),
            if self.is_uninitialized() {
                "uninitialised"
            } else if self.is_packed() {
                "packed"
            } else {
                "hash"
            }
        )
    }
}

/// `zend_hash_check_size`: eight at least, a power of two, and php's
/// fatal past `HT_MAX_SIZE`.
fn check_size(size: u32) -> Result<u32, Fatal> {
    if size <= MIN_SIZE {
        Ok(MIN_SIZE)
    } else if size > MAX_SIZE {
        Err(overflow(size, BUCKET))
    } else {
        Ok(size.next_power_of_two())
    }
}

/// php's `Possible integer overflow in memory allocation (%u * %zu +
/// %zu)`, the fatal the table's growth raises at `HT_MAX_SIZE`.
fn overflow(size: u32, each: usize) -> Fatal {
    Fatal::error(format!(
        "Possible integer overflow in memory allocation ({size} * {each} + {BUCKET})"
    ))
}

/// A position in an [`Arr`], walking the live elements in order.
#[derive(Debug)]
pub struct ArrIter {
    arr: Arr,
    pos: u32,
}

impl IntoIterator for &Arr {
    type Item = (Key, Zval);
    type IntoIter = ArrIter;

    fn into_iter(self) -> ArrIter {
        self.iter()
    }
}

impl Iterator for ArrIter {
    type Item = (Key, Zval);

    fn next(&mut self) -> Option<(Key, Zval)> {
        let pos = self.arr.valid_pos(self.pos);
        let entry = self.arr.entry_at(pos)?;
        self.pos = pos + 1;
        Some(entry)
    }
}

/// What a write asks for: `zend_hash_update` and `zend_hash_index_update`
/// write `key` whether or not it is there; `zend_hash_next_index_insert`
/// writes at `nNextFreeElement` and refuses an occupied one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    Update,
    Next,
    /// `HASH_ADD_NEW`: the caller knows the key is not there, so a hash
    /// table is not searched for it.
    New,
}

impl crate::Heap {
    /// `zend_new_array(0)`: a 56-byte header out of this heap, one owner,
    /// nothing allocated for its elements until the first write.
    pub fn array(&mut self) -> Result<Arr, Fatal> {
        self.array_sized(0)
    }

    /// `zend_new_array(size)`, `array_init_size`: the header, with the
    /// first data allocation sized for `size` elements through
    /// `zend_hash_check_size`.
    pub fn array_sized(&mut self, size: u32) -> Result<Arr, Fatal> {
        let size = check_size(size)?;
        let at = self.mm.alloc(HEADER)?;
        // SAFETY: the allocation is the header's size and nothing else
        // holds it.
        Ok(unsafe { Arr::init(at, size) })
    }

    /// `zend_new_array(size)` then `zend_hash_real_init_mixed`, which is
    /// what `INIT_ARRAY` does under `ZEND_ARRAY_NOT_PACKED`: the header
    /// and the bucket table laid down at once, so the first write finds
    /// a hash and never a packed list to convert. What `memory_get_usage()`
    /// reads after `[$x, 'k' => 1]` is this table's size, not the packed one's.
    pub fn array_mixed(&mut self, size: u32) -> Result<Arr, Fatal> {
        let a = self.array_sized(size)?;
        a.real_init_mixed(&mut self.tables())?;
        Ok(a)
    }

    /// `zend_hash_extend(a, size, HT_IS_PACKED(a))`: room for `size`
    /// elements made at once, where php makes it before it writes a
    /// known number of them. An uninitialised table is laid down at
    /// `size` checked as `zend_hash_check_size` checks it, a hash; a
    /// packed one grows in place as `perealloc2` grows it; a hash moves
    /// to a table of that size and is rehashed. A table already that
    /// large is left as it stands. The table has one owner.
    pub fn array_extend(&mut self, a: Arr, size: u32) -> Result<(), Fatal> {
        if size == 0 {
            return Ok(());
        }
        if a.is_uninitialized() {
            if size > a.table_size() {
                a.header().table_size.set(check_size(size)?);
            }
            return a.real_init_mixed(&mut self.tables());
        }
        if size <= a.table_size() {
            return Ok(());
        }
        let grown = check_size(size)?;
        let mut tables = self.tables();
        let old = a.data_addr();
        if a.is_packed() {
            let used_bytes = Arr::hash_size(MIN_MASK) + a.num_used() as usize * SLOT;
            let new = match old {
                // SAFETY: the data is this allocator's, its first
                // `used_bytes` are the hash and the used slots, and the
                // header is its only name, replaced below.
                Some(old) => unsafe { tables.realloc(old, used_bytes, Arr::packed_size(grown)) }?,
                None => tables.alloc(Arr::packed_size(grown))?,
            };
            a.set_data_addr(new);
            a.header().table_size.set(grown);
            return Ok(());
        }
        let new = tables.alloc(Arr::data_size(grown))?;
        let used = a.num_used();
        let buckets: Vec<Bucket> = (0..used).map(|i| a.read_bucket(i)).collect();
        a.header().table_size.set(grown);
        a.header().table_mask.set(Arr::size_to_mask(grown));
        a.set_data_addr(new);
        for (i, b) in buckets.into_iter().enumerate() {
            a.write_bucket(i as u32, b);
        }
        if let Some(old) = old {
            // SAFETY: the old data was this heap's and its buckets are
            // copied.
            unsafe { tables.free(old) };
        }
        a.rehash(tables.iterators());
        Ok(())
    }

    /// `zend_new_array(size)` then `zend_hash_real_init_packed`: the
    /// header and the packed slots laid down at once, so a table php
    /// real-inits and leaves empty holds its slots and reads `packed` to
    /// `debug_zval_dump`, and the first write grows nothing.
    pub fn array_packed(&mut self, size: u32) -> Result<Arr, Fatal> {
        let a = self.array_sized(size)?;
        a.real_init_packed(&mut self.tables())?;
        Ok(a)
    }

    /// `zend_hash_update` or `zend_hash_index_update` over `a`: `z` is
    /// the array's from here, and what `key` held is released last, after
    /// the write, through `hooks`, which is where a destructor is served
    /// and why the heap is theirs (seam 1). The array has one owner,
    /// which the engine's `SEPARATE_ARRAY` made sure of.
    pub fn array_update(hooks: &mut dyn HeapHooks, a: Arr, key: Key, z: Zval) -> Result<(), Fatal> {
        let written = match key {
            Key::Long(n) => hooks.heap().array_write_long(a, n, z, Write::Update)?,
            Key::Str(s) => hooks.heap().array_write_str(a, s, z)?,
        };
        if let Written::Replaced(old) = written {
            Heap::release(hooks, old);
        }
        Ok(())
    }

    /// `zend_hash_str_update` over `a`: `z` at the string key of these
    /// bytes, as written, no integer read out of them. A key the table
    /// holds keeps its bucket and its string, and what it held is
    /// released last, through `hooks`, as [`Heap::array_update`] does.
    /// A key it does not hold is made only after the table has room, as
    /// `_zend_hash_str_add_or_update_i` makes its `zend_string_init` past
    /// `ZEND_HASH_IF_FULL_DO_RESIZE`: so no string stands beside the old
    /// data and the new while the table doubles, which
    /// `memory_get_peak_usage()` reads. The array has one owner, which
    /// the engine's `SEPARATE_ARRAY` made sure of.
    pub fn array_str_update(
        hooks: &mut dyn HeapHooks,
        a: Arr,
        bytes: &[u8],
        z: Zval,
    ) -> Result<(), Fatal> {
        let heap = hooks.heap();
        if let Some(idx) = a.find_bucket_bytes(crate::string::hash_func(bytes), bytes) {
            let old = a.replace_at(idx, z);
            Heap::release(hooks, old);
            return Ok(());
        }
        a.make_room_for_str(&mut heap.tables())?;
        let key = heap.string(bytes)?;
        a.place_str(key, z);
        Ok(())
    }

    /// `zend_hash_next_index_insert`, `$a[] = z`: `false`, and `z` still
    /// the caller's, when `nNextFreeElement` is `ZEND_LONG_MAX` and taken.
    /// Nothing is replaced, so nothing here can reach user code.
    pub fn array_append(&mut self, a: Arr, z: Zval) -> Result<bool, Fatal> {
        Ok(self.array_write_long(a, a.next_free(), z, Write::Next)? != Written::Refused)
    }

    /// `zend_hash_lookup` and `zend_hash_index_lookup`
    /// (`Z/Zend/zend_hash.c:1004`, `:1196`), what
    /// `zend_fetch_dimension_address_inner` under `BP_VAR_W` reads: the
    /// entry at `key` in place, laid down holding `null` where the table
    /// has none, which is why an unwritten `$a['k']` is `NULL` after
    /// `$a['k'][0] = 1`. One walk: the entry found is the one answered,
    /// and a key not found is added where it goes with no second search
    /// (`HASH_LOOKUP`, `HASH_ADD_NEW`). The array has one owner, which
    /// the engine's `SEPARATE_ARRAY` made sure of, and the name is one
    /// until the next write, as [`Arr::value_ref`] says.
    ///
    /// ```php
    /// $p = [[1], [2], [3]]; $h = ["k" => [1], 5 => [2]]; $e = [];
    /// $p[1][] = 4; unset($p[0]); $p[0][] = 5; $p[4][] = 6;
    /// $h["k"][] = 7; $h[5][] = 8; $h["n"][] = 9; $h[6][] = 10;
    /// $e[0][] = 11; $f = []; $f["s"][] = 12;
    /// echo json_encode([$p, $h, $e, $f]), "\n";
    /// ```
    /// ```expect
    /// [{"1":[2,4],"2":[3],"0":[5],"4":[6]},{"k":[1,7],"5":[2,8],"n":[9],"6":[10]},[[11]],{"s":[12]}]
    /// ```
    #[inline]
    pub fn array_lookup(&mut self, a: Arr, key: Key) -> Result<ZvalRef, Fatal> {
        if let Some(pos) = a.find(key) {
            return Ok(ZvalRef::new(a.value_ptr(pos)));
        }
        self.array_add_null(a, key)
    }

    /// The miss of [`Heap::array_lookup`]: `null` added under a key the
    /// table has not got, and where it went.
    #[inline(never)]
    fn array_add_null(&mut self, a: Arr, key: Key) -> Result<ZvalRef, Fatal> {
        let pos = match key {
            Key::Long(n) => self.array_write_long(a, n, Zval::null(), Write::New)?,
            Key::Str(s) => Written::Added(append_str(&mut self.tables(), a, s, Zval::null())?),
        };
        match pos {
            Written::Added(pos) => Ok(ZvalRef::new(a.value_ptr(pos))),
            Written::Replaced(_) | Written::Refused => Err(Fatal::error(
                "InternalError: zend_hash_lookup added a key and found it already there",
            )),
        }
    }

    /// `zend_hash_next_index_insert` of `EG(uninitialized_zval)`, what
    /// `zend_fetch_dimension_address` does for `$a[]` in a write: the
    /// entry appended, in place, holding `null`; `None` where
    /// `nNextFreeElement` is `ZEND_LONG_MAX` and taken, which is
    /// `zend_cannot_add_element`'s `Error` at the site.
    pub fn array_next_entry(&mut self, a: Arr) -> Result<Option<ZvalRef>, Fatal> {
        let index = a.next_free();
        let index = if index == i64::MIN { 0 } else { index };
        Ok(
            match self.array_write_long(a, index, Zval::null(), Write::Next)? {
                Written::Added(pos) => Some(ZvalRef::new(a.value_ptr(pos))),
                Written::Replaced(_) | Written::Refused => None,
            },
        )
    }

    /// `_zend_hash_index_add_or_update_i`: what it did, the value it
    /// replaced handed back for the caller to release once the heap is
    /// let go of.
    fn array_write_long(
        &mut self,
        a: Arr,
        index: i64,
        z: Zval,
        write: Write,
    ) -> Result<Written, Fatal> {
        write_long(&mut self.tables(), a, index, z, write)
    }

    /// `_zend_hash_add_or_update_i` with `HASH_UPDATE`: the key is the
    /// bucket's own from here, one more owner unless it is interned; the
    /// value replaced, if one was, handed back to release.
    pub(crate) fn array_write_str(&mut self, a: Arr, key: Str, z: Zval) -> Result<Written, Fatal> {
        write_str(&mut self.tables(), a, key, z)
    }

    /// The allocator and the iterator list a write to a request's
    /// array is handed.
    fn tables(&mut self) -> Tables<'_> {
        Tables {
            mm: &mut self.mm,
            iterators: &mut self.iterators,
        }
    }
}

/// `_zend_hash_index_add_or_update_i` over whichever allocator `a`'s
/// data is from: the request's heap for a request's array, the system
/// allocator for a constant one.
fn write_long(
    mm: &mut dyn Allocate,
    a: Arr,
    index: i64,
    z: Zval,
    write: Write,
) -> Result<Written, Fatal> {
    let index = if write == Write::Next && index == i64::MIN {
        0
    } else {
        index
    };
    let h = index.cast_unsigned();
    let header = a.header();
    if a.is_packed() {
        if h < u64::from(a.num_used()) {
            let idx = h as u32;
            let old = a.read_slot(idx);
            if !old.is_undef() {
                if write == Write::Next {
                    return Ok(Written::Refused);
                }
                a.write_slot(idx, z);
                return Ok(Written::Replaced(old));
            }
            a.packed_to_hash(mm)?;
        } else if h < u64::from(a.table_size()) {
            a.add_to_packed(h as u32, z);
            return Ok(Written::Added(Pos::Slot(h as u32)));
        } else if (h >> 1) < u64::from(a.table_size())
            && (a.table_size() >> 1) < header.num_elements.get()
        {
            a.packed_grow(mm)?;
            a.add_to_packed(h as u32, z);
            return Ok(Written::Added(Pos::Slot(h as u32)));
        } else {
            if a.num_used() >= a.table_size() {
                header.table_size.set(a.table_size() + a.table_size());
            }
            a.packed_to_hash(mm)?;
        }
    } else if a.is_uninitialized() {
        if h < u64::from(a.table_size()) {
            a.real_init_packed(mm)?;
            a.add_to_packed(h as u32, z);
            return Ok(Written::Added(Pos::Slot(h as u32)));
        }
        a.real_init_mixed(mm)?;
    } else {
        if write != Write::New
            && let Some(idx) = a.find_long(h)
        {
            if write == Write::Next {
                return Ok(Written::Refused);
            }
            let mut b = a.read_bucket(idx);
            let old = b.val;
            b.val = z;
            b.val.set_next(old.next());
            a.write_bucket(idx, b);
            return Ok(Written::Replaced(old));
        }
        if a.num_used() >= a.table_size() {
            a.do_resize(mm)?;
        }
    }
    let idx = a.num_used();
    header.num_used.set(idx + 1);
    a.write_bucket(
        idx,
        Bucket {
            val: z,
            h,
            key: None,
        },
    );
    a.link(idx);
    if index >= header.next_free.get() {
        header.next_free.set(if index < i64::MAX {
            index + 1
        } else {
            i64::MAX
        });
    }
    header.num_elements.set(header.num_elements.get() + 1);
    Ok(Written::Added(Pos::Bucket(idx)))
}

/// `_zend_hash_add_or_update_i` with `HASH_UPDATE` over whichever
/// allocator `a`'s data is from: the key is the bucket's own from here,
/// one more owner unless it is interned; the value replaced, if one was,
/// handed back to release.
fn write_str(mm: &mut dyn Allocate, a: Arr, key: Str, z: Zval) -> Result<Written, Fatal> {
    if !a.is_uninitialized()
        && !a.is_packed()
        && let Some(idx) = a.find_str(key)
    {
        return Ok(Written::Replaced(a.replace_at(idx, z)));
    }
    Ok(Written::Added(append_str(mm, a, key, z)?))
}

/// `_zend_hash_add_or_update_i` with `HASH_ADD_NEW`, past the lookup a
/// release build leaves out: a bucket appended under `key` and linked at
/// the head of its chain, so a table that held the key already holds it
/// twice and a lookup finds this one, as php's does.
fn append_str(mm: &mut dyn Allocate, a: Arr, key: Str, z: Zval) -> Result<Pos, Fatal> {
    a.make_room_for_str(mm)?;
    if !key.is_interned() {
        key.addref();
    }
    Ok(a.place_str(key, z))
}

impl Arr {
    /// The value at bucket `idx` replaced by `z`, which keeps the chain
    /// link; the old value handed back to release.
    fn replace_at(self, idx: u32, z: Zval) -> Zval {
        let mut b = self.read_bucket(idx);
        let old = b.val;
        b.val = z;
        b.val.set_next(old.next());
        self.write_bucket(idx, b);
        old
    }

    /// What `_zend_hash_add_or_update_i` and `_zend_hash_str_add_or_update_i`
    /// do to the table before a string key's bucket goes in: lay it down
    /// where it is uninitialised; else turn a packed table into a hash of
    /// the same size, and then, `ZEND_HASH_IF_FULL_DO_RESIZE`, grow it
    /// where it is full, a packed table just turned among them.
    ///
    /// ```php
    /// $m = memory_get_usage(); $c = []; for ($i = 0; $i < 8; $i++) $c[] = $i;
    /// $c['k'] = 1; $c['l'] = 2;
    /// echo memory_get_usage() - $m, ' ', count($c), ' ', $c['k'] + $c['l'] + $c[7], "\n";
    /// ```
    /// ```expect capture
    /// 696 10 10
    /// ```
    fn make_room_for_str(self, mm: &mut dyn Allocate) -> Result<(), Fatal> {
        if self.is_uninitialized() {
            return self.real_init_mixed(mm);
        }
        if self.is_packed() {
            self.packed_to_hash(mm)?;
        }
        if self.num_used() >= self.table_size() {
            self.do_resize(mm)?;
        }
        Ok(())
    }

    /// The bucket of `key` and `z` appended to a table with room for it
    /// and linked at the head of its chain. The bucket takes the owner of
    /// `key` it is handed. Where it went is answered.
    fn place_str(self, key: Str, z: Zval) -> Pos {
        let h = key.hash();
        let header = self.header();
        if !key.is_interned() {
            header.flags.set(header.flags.get() & !STATIC_KEYS);
        }
        let idx = self.num_used();
        header.num_used.set(idx + 1);
        header.num_elements.set(header.num_elements.get() + 1);
        self.write_bucket(
            idx,
            Bucket {
                val: z,
                h,
                key: Some(key),
            },
        );
        self.link(idx);
        Pos::Bucket(idx)
    }
}

impl crate::Heap {
    /// `zend_hash_del` and `zend_hash_index_del`: the element at `key`
    /// gone, its value released through `hooks` and its string key given
    /// up; `false` for a key that is not there.
    pub fn array_del(hooks: &mut dyn HeapHooks, a: Arr, key: Key) -> bool {
        let Some(pos) = a.find(key) else {
            return false;
        };
        if let Pos::Bucket(idx) = pos {
            a.unlink(idx);
            if let Some(k) = a.read_bucket(idx).key {
                hooks.heap().release_string(k);
            }
        }
        let old = a.del_at(pos.index(), Some(&mut hooks.heap().iterators));
        Heap::release(hooks, old);
        true
    }

    /// `zend_hash_clean` (`Z/Zend/zend_hash.c:1875`) over `a`, which has
    /// one owner: every row's value destroyed in place, in order, its
    /// destructor served as it goes, with every key standing until the
    /// last value is destroyed; then in one pass over the buckets the
    /// keys given up, and the table emptied where it stands, its data
    /// kept, `nNumUsed`, `nNumOfElements` and `nInternalPointer` back to
    /// zero, `nNextFreeElement` to `ZEND_LONG_MIN` and the hash slots
    /// reset. A destroyed row reads as `null`. The walk stops at the rows
    /// the table held as it began, as php's `end` does, and ends where a
    /// destructor destroys the table; a hash iterator registered at the
    /// table says so, and carries the walk's position past a rehash a
    /// destructor's write sets off. Rows a destructor wrote are given up
    /// once the table is empty. Other iterators on the table are left
    /// where they stand, as php leaves them.
    pub fn array_clean(hooks: &mut dyn HeapHooks, a: Arr) {
        let watch = hooks.heap().iterator_add(a, 0);
        let end = a.num_used();
        while hooks.heap().iterator_on(watch, a) {
            let pos = hooks.heap().iterator_pos(watch, a);
            if pos >= end {
                break;
            }
            hooks.heap().iterator_set_pos(watch, pos + 1);
            if let Some(place) = a.value_ref_at(pos) {
                let old = place.read();
                if !old.is_undef() {
                    place.write(Zval::null());
                    Heap::release(hooks, old);
                }
            }
        }
        let standing = hooks.heap().iterator_on(watch, a);
        hooks.heap().iterator_del(watch);
        if standing {
            for left in a.emptied_in_place(hooks.heap()) {
                Heap::release(hooks, left);
            }
        }
    }

    /// `zend_array_dup`: a new array of one owner holding what `a`
    /// holds, every refcounted element one more owner and every string
    /// key too, the tombstones left behind and the internal pointer
    /// moved with the element it stood on.
    pub fn array_dup(&mut self, a: Arr) -> Result<Arr, Fatal> {
        let source = a.header();
        let at = self.mm.alloc(HEADER)?;
        // SAFETY: the allocation is the header's size and nothing else
        // holds it.
        let target = unsafe { Arr::init(at, MIN_SIZE) };
        let header = target.header();
        header.next_free.set(source.next_free.get());
        if source.num_elements.get() == 0 {
            return Ok(target);
        }
        header.table_size.set(source.table_size.get());
        let used = a.num_used();
        let pointer = source.internal_pointer.get();
        if a.is_packed() {
            target.real_init_packed(&mut self.mm)?;
            header.num_used.set(used);
            header.num_elements.set(source.num_elements.get());
            header
                .internal_pointer
                .set(if pointer < used { pointer } else { 0 });
            for i in 0..used {
                let z = self.dup_value(a.read_slot(i), Some(a));
                target.write_slot(i, z);
            }
            self.iterators.dup(a, target);
            return Ok(target);
        }
        target.real_init_mixed(&mut self.mm)?;
        target.set_flag_byte(source.flags.get() & (PACKED | UNINITIALIZED | STATIC_KEYS));
        self.iterators.dup(a, target);
        let walked = target.has_iterators();
        if walked {
            // `target->nNumUsed = source->nNumUsed`, so the positions the
            // copies were given read against the source's bounds while
            // the walk below compacts.
            header.num_used.set(used);
        }
        // `zend_array_dup_elements` past the first hole: `iter_pos` is the
        // lowest copy at or past the source position being moved, and
        // `None` until a hole has been met, since nothing before one
        // moves.
        let mut iter_pos: Option<u32> = None;
        header
            .internal_pointer
            .set(if pointer < used { pointer } else { 0 });
        let mut idx = 0;
        for i in 0..used {
            let b = a.read_bucket(i);
            if b.val.is_undef() {
                if walked && iter_pos.is_none() {
                    iter_pos = Some(self.iterators.lower_pos(target, i + 1));
                }
                continue;
            }
            let b = Bucket {
                val: self.dup_value(b.val, Some(a)),
                ..b
            };
            if let Some(k) = b.key {
                k.addref();
            }
            target.write_bucket(idx, b);
            target.link(idx);
            if pointer == i {
                header.internal_pointer.set(idx);
            }
            // An iterator copy at a position the compaction moves is
            // moved with its bucket.
            if let Some(mut lowest) = iter_pos
                && i >= lowest
            {
                loop {
                    self.iterators.update(target, lowest, idx);
                    lowest = self.iterators.lower_pos(target, lowest + 1);
                    if lowest >= i {
                        break;
                    }
                }
                iter_pos = Some(lowest);
            }
            idx += 1;
        }
        header.num_used.set(idx);
        header.num_elements.set(idx);
        Ok(target)
    }

    /// `zend_hash_add_new` and `zend_hash_index_add_new` of `z` under
    /// `key` into `a`, which has one owner: the key one more owner unless
    /// it is interned, `z` the array's from here. A string key is
    /// appended without the lookup, as php's release build appends it, so
    /// a key `a` holds already is held twice and a lookup finds the later
    /// row; an integer key is written as `zend_hash_index_update` writes
    /// it, the same bucket wherever the key is new, which is the one case
    /// php's add admits.
    pub fn array_add_new(&mut self, a: Arr, key: Key, z: Zval) -> Result<(), Fatal> {
        match key {
            Key::Str(name) => {
                append_str(&mut self.tables(), a, name, z)?;
            }
            Key::Long(n) => {
                self.array_write_long(a, n, z, Write::Update)?;
            }
        }
        Ok(())
    }

    /// `SEPARATE_ARRAY` (`Z/Zend/zend_execute.c`): `a` itself while it
    /// has one owner, else a copy with one owner and `a` with one fewer,
    /// which is what a write to a shared array does before it writes
    /// (design §5.3).
    ///
    /// php's macro gives up an owner only where the zval it separates is
    /// refcounted, `if (Z_REFCOUNTED_P(__zv)) GC_DELREF(_arr)`, and an
    /// immutable array is the one that is not: [`Zval::array`] gives it
    /// neither `IS_TYPE_REFCOUNTED` nor `IS_TYPE_COLLECTABLE`, because a
    /// constant array of a published unit is shared by every thread and
    /// nobody owns it. The guard is one level down here, in
    /// [`Arr::delref`] and [`Arr::addref`], where `zend_array_refcount`
    /// puts it: an immutable array reads as one owner, takes none and
    /// gives none up, so no request writes the `Cell` inside a value the
    /// unit published, and this copies where php copies without the
    /// second test. `a_separated_immutable_array_keeps_the_count_it_was_published_with`
    /// holds it.
    pub fn separate(&mut self, a: Arr) -> Result<Arr, Fatal> {
        if a.refcount() == 1 && !a.is_immutable() {
            return Ok(a);
        }
        let copy = self.array_dup(a)?;
        let _ = a.delref();
        Ok(copy)
    }

    /// `zend_hash_sort_ex` (`Z/Zend/zend_hash.c:3077`) with
    /// [`zend_sort`] as its sort, the one every sort native runs: a
    /// packed table made a hash table first, the holes compacted and
    /// each element handed to `compare` with the position it held, the
    /// buckets ordered as php's hybrid orders them for any comparison,
    /// the internal pointer reset, and then the chains rebuilt where the
    /// keys are kept, or under `renumber` the keys dropped, `nNextFreeElement`
    /// set past the last, and the table laid down packed in a fresh
    /// allocation with the hash table's given back. Nothing here is
    /// stable of itself: php's stable order is the caller's comparison
    /// falling back to [`SortEntry::position`], `RETURN_STABLE_SORT`, and
    /// a caller that wants `zend_sort`'s own order leaves it out. The
    /// hooks carry the comparison to the code it may serve, a `usort`
    /// callback, and are the heap after it. An empty table is left as it
    /// is; one element is renumbered where php renumbers it.
    ///
    /// The renumbered array is packed, so its keys are gone and the
    /// next append lands past the last; a hole and a string key vanish
    /// with the rest.
    ///
    /// ```php
    /// $a = ["x" => 3, 5 => 1, "y" => 2, 9 => 1]; unset($a[5]); sort($a); $a[] = 7; var_dump($a);
    /// $one = [4 => "a"]; sort($one); var_dump($one);
    /// ```
    /// ```expect
    /// array(4) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    ///   [2]=>
    ///   int(3)
    ///   [3]=>
    ///   int(7)
    /// }
    /// array(1) {
    ///   [0]=>
    ///   string(1) "a"
    /// }
    /// ```
    ///
    /// While the sort runs the table's hash index is empty, as php's
    /// `HT_HASH_RESET` leaves it: an element that reaches the table
    /// being sorted through a reference finds none of its keys there,
    /// so `['k' => 1, 'r' => 1]` orders after it, the left side's key
    /// missing from the right.
    ///
    /// ```php
    /// $a = ['k' => ['k' => 1, 'r' => 1], 'r' => null];
    /// $a['r'] = &$a;
    /// asort($a); echo implode(",", array_keys($a)), "\n";
    /// ```
    /// ```expect
    /// r,k
    /// ```
    pub fn array_sort<H: HeapHooks + ?Sized>(
        hooks: &mut H,
        a: Arr,
        renumber: bool,
        compare: &mut dyn FnMut(&mut H, &SortEntry, &SortEntry) -> Ordering,
    ) -> Result<(), Fatal> {
        // `!(nNumOfElements > 1) && !(renumber && nNumOfElements > 0)`.
        if a.len() <= 1 && (!renumber || a.is_empty()) {
            return Ok(());
        }
        if a.is_packed() {
            a.packed_to_hash(&mut hooks.heap().tables())?;
        }
        // `HT_HASH_RESET`: php's sort overwrites each bucket's chain
        // link with its position, and empties the hash index so that a
        // comparison reaching this table through a reference finds no
        // key in it rather than a broken chain (`Z/Zend/zend_hash.c:3021`,
        // for `bug63882_2.phpt`). The index is rebuilt below.
        a.hash_reset();
        let mut entries: Vec<SortEntry> = (0..a.num_used())
            .map(|i| a.read_bucket(i))
            .filter(|b| !b.val.is_undef())
            .enumerate()
            .map(|(position, b)| SortEntry {
                key: match b.key {
                    Some(s) => Key::Str(s),
                    None => Key::Long(b.h.cast_signed()),
                },
                val: b.val,
                position: position as u32,
            })
            .collect();
        zend_sort(&mut entries, &mut |x, y| compare(hooks, x, y));
        let header = a.header();
        header.num_used.set(entries.len() as u32);
        let used = entries.len() as u32;
        let heap = hooks.heap();
        for (i, entry) in entries.into_iter().enumerate() {
            let (h, key) = match entry.key {
                Key::Str(s) if renumber => {
                    heap.release_string(s);
                    (i as u64, None)
                }
                Key::Long(_) if renumber => (i as u64, None),
                Key::Str(s) => (s.hash(), Some(s)),
                Key::Long(n) => (n.cast_unsigned(), None),
            };
            a.write_bucket(
                i as u32,
                Bucket {
                    val: entry.val,
                    h,
                    key,
                },
            );
        }
        header.internal_pointer.set(0);
        if !renumber {
            a.rehash(Some(&mut hooks.heap().iterators));
            return Ok(());
        }
        header.next_free.set(i64::from(used));
        let old = a.data_addr();
        let new = heap.mm.alloc(Arr::packed_size(a.table_size()))?;
        let slots: Vec<Zval> = (0..used).map(|i| a.read_bucket(i).val).collect();
        a.set_flag_byte((header.flags.get() & 0xff) | PACKED | STATIC_KEYS);
        header.table_mask.set(MIN_MASK);
        a.set_data_addr(new);
        for (i, val) in slots.into_iter().enumerate() {
            a.write_slot(i as u32, val);
        }
        if let Some(old) = old {
            // SAFETY: the old data was this heap's and the slots hold the
            // elements now.
            unsafe { heap.mm.free(old) };
        }
        a.hash_reset();
        Ok(())
    }

    /// `zend_array_destroy` (`Z/Zend/zend_hash.c:1815`) over a table
    /// its last owner has let go of, through [`Heap::destroy`].
    pub(crate) fn array_destroy(hooks: &mut dyn HeapHooks, a: Arr) {
        Heap::destroy(hooks, Last::Table(a));
    }

    /// `rc_dtor_func` over a table or a reference cell no owner is left
    /// of, and over every one its contents were the last owner of, in
    /// php's order and without recursing on the machine stack.
    ///
    /// php's `zend_array_destroy` releases each element with
    /// `i_zval_ptr_dtor`, which calls `rc_dtor_func` again for an
    /// element that was the last owner of a table or a cell, so a nest
    /// is freed depth first: an element's own contents, destructors and
    /// all, are given up before the next element is read, and a table's
    /// string key after its value. The walk here is that recursion with
    /// its frames on a list: one entry per table being emptied, at the
    /// element it has reached and holding the key it has yet to give
    /// up, and one per cell whose value is being given up and whose
    /// bytes go back after it. So a nest php frees in its small frames is
    /// freed here at any depth.
    ///
    /// A table is `zend_array_destroy`: nothing for one the cycle
    /// collector is already destroying, whose type it turned to
    /// `IS_NULL` (`rc_dtor_func`'s `zend_empty_destroy`); otherwise the
    /// table taken out of the root buffer and turned to `IS_NULL`
    /// itself, every element released through `hooks`, in order, with
    /// the table standing around each, every string key given up, then
    /// the iterators unregistered and the data and the header back to
    /// the heap. A cell is `zend_reference_destroy`: the typed-property
    /// sources given up first, as `zend_free_source_list` does, then the
    /// value held released and the cell's bytes given back after it.
    ///
    /// The order is php's, destructors and all:
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n; } }
    /// $r = [new D(3)];
    /// $a = [[new D(1), [new D(2)], &$r], 'k' => [new D(4)], new D(5)];
    /// unset($r);
    /// $a = null;
    /// echo "\n";
    /// ```
    /// ```expect
    /// 12345
    /// ```
    ///
    /// A nest built through references and a plain one, each 25,000
    /// deep, which the witness lane's 2 MiB threads ran out on while
    /// the walk recursed:
    ///
    /// ```php
    /// $a = []; $c = &$a;
    /// for ($i = 0; $i < 25000; $i++) { $c[] = []; $c = &$c[0]; }
    /// unset($c);
    /// $a = null;
    /// $b = [];
    /// for ($i = 0; $i < 25000; $i++) { $b = [$b]; }
    /// $b = null;
    /// echo "released\n";
    /// ```
    /// ```expect
    /// released
    /// ```
    pub(crate) fn destroy(hooks: &mut dyn HeapHooks, first: Last) {
        // The heap's room for the walk, taken for its length and put back
        // emptied, so a walk allocates nothing after the request's first.
        let mut walk = std::mem::take(&mut hooks.heap().destroying);
        Heap::destroy_into(hooks, first, &mut walk);
        walk.clear();
        hooks.heap().destroying = walk;
    }

    /// [`Heap::destroy`]'s walk over `walk`, empty on entry and on the
    /// way out.
    fn destroy_into(hooks: &mut dyn HeapHooks, first: Last, walk: &mut Vec<Emptying>) {
        let mut next = Some(first);
        loop {
            match next.take() {
                // A table the cycle collector is destroying already.
                Some(Last::Table(a)) if a.gc().kind() == ZvalType::Null as u32 => {}
                Some(Last::Table(a)) => {
                    hooks
                        .heap()
                        .gc
                        .remove_from_buffer(crate::collector::Node::Arr(a));
                    a.gc().set_kind(ZvalType::Null);
                    walk.push(Emptying::Table {
                        a,
                        at: 0,
                        key: None,
                    });
                }
                Some(Last::Cell(r)) => {
                    if r.sources() != 0 {
                        hooks.free_reference(r);
                        r.set_sources(0);
                    }
                    let held = r.val();
                    r.write_val(Zval::UNDEF);
                    walk.push(Emptying::Cell(r));
                    next = Heap::release_within(hooks, held);
                    continue;
                }
                None => {}
            }
            let Some(top) = walk.last_mut() else {
                return;
            };
            match top {
                Emptying::Cell(r) => {
                    let r = *r;
                    walk.pop();
                    // SAFETY: the reference came from `reference`, its
                    // last owner has given it up and what it held is
                    // released, so only a pin names it.
                    unsafe { hooks.heap().give_back(r.address(), r.gc().pinned()) };
                }
                Emptying::Table { a, at, key } => {
                    if let Some(k) = key.take() {
                        hooks.heap().release_string(k);
                    }
                    let a = *a;
                    let Some((val, k)) = a.element_from(at) else {
                        walk.pop();
                        Heap::array_free_data(hooks.heap(), a);
                        let pinned = a.gc().pinned();
                        if pinned {
                            a.emptied();
                        }
                        // SAFETY: the header came from `array_sized`, its
                        // last owner has given it up and its contents are
                        // freed, so only a pin names it.
                        unsafe { hooks.heap().give_back(a.address(), pinned) };
                        continue;
                    };
                    next = Heap::release_within(hooks, val);
                    match (next.is_some(), k) {
                        (true, Some(k)) => *key = Some(k),
                        (false, Some(k)) => hooks.heap().release_string(k),
                        (_, None) => {}
                    }
                }
            }
        }
    }

    /// [`Heap::release`] of one element inside [`Heap::destroy`]: what it
    /// would destroy is handed back for the walk to take next.
    fn release_within(hooks: &mut dyn HeapHooks, z: Zval) -> Option<Last> {
        match Heap::give_up(hooks, z) {
            GivenUp::Owned => {
                Heap::gc_check_possible_root(hooks, z);
                None
            }
            GivenUp::Freed => None,
            GivenUp::Last(last) => Some(last),
        }
    }

    /// `zend_hash_destroy`: every element released through `hooks`, in
    /// order, every string key given up, the iterators unregistered and
    /// the data back to the heap, the header left for the caller, which
    /// is what the cycle collector's free phase does to a garbage table
    /// before it frees the headers in a second pass.
    pub(crate) fn array_free_contents(hooks: &mut dyn HeapHooks, a: Arr) {
        let mut at = 0;
        while let Some((val, key)) = a.element_from(&mut at) {
            Heap::release(hooks, val);
            if let Some(k) = key {
                hooks.heap().release_string(k);
            }
        }
        Heap::array_free_data(hooks.heap(), a);
    }

    /// The end of `zend_array_destroy` and `zend_hash_destroy` once
    /// every element is released: the iterators unregistered and the
    /// data back to the heap.
    fn array_free_data(heap: &mut Heap, a: Arr) {
        heap.iterators.remove(a);
        if let Some(data) = a.data_addr() {
            // SAFETY: the data was this heap's and every element in it is
            // released.
            unsafe { heap.mm.free(data) };
        }
    }
}

/// One frame of [`Heap::destroy`]'s walk: a table being emptied, at the
/// position of the next element to release and holding the key of the
/// element whose value is being given up, or a reference cell whose
/// value is being given up.
#[derive(Debug)]
pub(crate) enum Emptying {
    Table { a: Arr, at: u32, key: Option<Str> },
    Cell(Ref),
}

impl Arr {
    /// The next element of a table being destroyed at or past `*at`,
    /// its value and its string key, and `*at` moved past it: a packed
    /// table's slots, a hash's buckets less the tombstones, and nothing
    /// of an uninitialised table.
    fn element_from(self, at: &mut u32) -> Option<(Zval, Option<Str>)> {
        let used = self.num_used();
        if self.is_packed() {
            if *at >= used {
                return None;
            }
            let val = self.read_slot(*at);
            *at += 1;
            return Some((val, None));
        }
        if self.is_uninitialized() {
            return None;
        }
        while *at < used {
            let b = self.read_bucket(*at);
            *at += 1;
            if !b.val.is_undef() {
                return Some((b.val, b.key));
            }
        }
        None
    }
}

/// What a write did: the element added, and where it went, an
/// element's value replaced and handed back for the release that may
/// reach user code, or the next-index insert refused because
/// `ZEND_LONG_MAX` is taken.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Written {
    Added(Pos),
    Replaced(Zval),
    Refused,
}

#[cfg(test)]
mod tests {
    use super::canonical_int;

    /// `ZEND_HANDLE_NUMERIC_STR`: a canonical decimal integer in range
    /// and nothing else, which the engine reads for a key and the
    /// compiler for a constant dimension.
    #[test]
    fn keys_normalise_as_php_does() {
        assert_eq!(canonical_int(b"12"), Some(12));
        assert_eq!(canonical_int(b"-1"), Some(-1));
        assert_eq!(canonical_int(b"012"), None);
        assert_eq!(canonical_int(b"-0"), None);
        assert_eq!(canonical_int(b""), None);
        assert_eq!(canonical_int(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(canonical_int(b"9223372036854775808"), None);
    }

    #[test]
    fn a_constant_array_is_immutable_and_finds_its_keys() {
        use crate::{Intern, Strings};
        let mut strings = Strings::new();
        let Ok(s) = strings.intern(b"s") else {
            return;
        };
        let entries = [
            (Key::Long(1), Zval::long(12)),
            (Key::Str(s), Zval::long(14)),
            (Key::Long(-3), Zval::long(16)),
        ];
        let Ok(a) = strings.constant_array(&entries) else {
            return;
        };
        assert!(a.is_immutable());
        assert_eq!(a.refcount(), 1);
        assert!(!Zval::array(a).is_refcounted());
        assert_eq!(a.len(), 3);
        assert_eq!(a.get(Key::Long(1)).and_then(Zval::as_long), Some(12));
        assert_eq!(a.get(Key::Str(s)).and_then(Zval::as_long), Some(14));
        assert_eq!(a.get(Key::Long(-3)).and_then(Zval::as_long), Some(16));
        assert_eq!(a.get(Key::Long(2)), None);
        let keys: Vec<Key> = a.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, entries.map(|(k, _)| k));
        drop(strings);
    }

    use super::*;
    use crate::{Heap, Intern};

    use crate::serial::alone;

    /// The hooks the array's tests pass: a heap and nothing to serve,
    /// since no element here is an object, and a table the keys are
    /// interned into, off the heap as the engine's are.
    struct Nothing {
        heap: Heap,
        strings: crate::Strings,
    }

    impl Nothing {
        fn new() -> Nothing {
            Nothing {
                heap: Heap::new(),
                strings: crate::Strings::new(),
            }
        }
    }

    impl HeapHooks for Nothing {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn destruct(&mut self, _obj: crate::Obj) {}
        fn free_native(&mut self, _obj: crate::Obj) {}
        fn gc_children(&mut self, _obj: crate::Obj, _: &mut dyn FnMut(Zval)) -> crate::GcTable {
            crate::GcTable::Properties
        }
        fn free_resource(&mut self, _: crate::Res, _: crate::ResourceType) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::out_of_memory(bytes)
        }
    }

    /// `zend_empty_array`: one array for the process, immutable, empty
    /// and uninitialised, what the engine's table answers for a constant
    /// array of no entries, which nothing frees, and which a write separates
    /// into a table of the request's own.
    #[test]
    fn the_empty_array_is_the_process_s_one_and_a_write_separates_it() {
        let _alone = alone();
        let e = Arr::empty();
        assert!(e.is_same(Arr::empty()));
        assert!(e.is_immutable() && e.is_empty() && e.is_uninitialized());
        assert_eq!(e.next_free(), i64::MIN);
        assert!(!Zval::array(e).is_refcounted());
        let mut strings = crate::Strings::new();
        assert!(strings.constant_array(&[]).is_ok_and(|a| a.is_same(e)));
        drop(strings);
        let mut nothing = Nothing::new();
        let before = nothing.heap.memory_usage(false);
        let Ok(copy) = nothing.heap.separate(e) else {
            return;
        };
        assert!(!copy.is_same(e) && !copy.is_immutable());
        assert_eq!(nothing.heap.memory_usage(false) - before, HEADER);
        assert!(e.is_empty() && e.is_immutable());
    }

    /// `SEPARATE_ARRAY` over an immutable array, the constant array a
    /// published unit shares with every thread (design §5.3, §5.14): a
    /// copy, and the array itself untouched. php's macro gives up an
    /// owner only under `Z_REFCOUNTED_P`, and an immutable array is not
    /// refcounted; here [`Arr::delref`] is what refuses to write, so the
    /// header's own count reads after what it read before and the
    /// `Cell`s of a value the unit published are never written. #442
    /// asked whether the count went to zero; it does not.
    #[test]
    fn a_separated_immutable_array_keeps_the_count_it_was_published_with() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Ok(a) = nothing
            .strings
            .constant_array(&[(Key::Long(0), Zval::long(1))])
        else {
            return;
        };
        assert!(a.is_immutable());
        assert!(!Zval::array(a).is_refcounted());
        let published = a.header().gc.refcount();
        for _ in 0..3 {
            let Ok(copy) = nothing.heap.separate(a) else {
                return;
            };
            assert!(
                !copy.is_same(a),
                "an immutable array is copied, never written"
            );
            assert!(!copy.is_immutable());
            assert_eq!(copy.refcount(), 1);
            assert_eq!(copy.get(Key::Long(0)).and_then(Zval::as_long), Some(1));
            assert_eq!(
                a.header().gc.refcount(),
                published,
                "the published array's count is as it was found"
            );
            Heap::release(&mut nothing, Zval::array(copy));
        }
    }

    /// `zend_hash_add_new` appends without the lookup: a key the table
    /// holds already is held twice, both rows walked in order and
    /// counted, and a lookup finds the later one, across a resize too.
    #[test]
    fn an_add_new_of_a_held_key_is_a_second_row_the_lookup_finds() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let (Ok(a), Ok(b), Ok(c)) = (
            nothing.strings.intern(b"a"),
            nothing.strings.intern(b"b"),
            nothing.heap.array(),
        ) else {
            return;
        };
        for (key, n) in [(a, 1), (b, 2), (a, 3)] {
            assert!(
                nothing
                    .heap
                    .array_add_new(c, Key::Str(key), Zval::long(n))
                    .is_ok()
            );
        }
        assert_eq!(c.len(), 3);
        assert_eq!(c.get(Key::Str(a)).and_then(Zval::as_long), Some(3));
        for n in 0..8 {
            assert!(
                nothing
                    .heap
                    .array_add_new(c, Key::Long(n), Zval::long(n))
                    .is_ok()
            );
        }
        assert_eq!(c.len(), 11);
        assert_eq!(c.get(Key::Str(a)).and_then(Zval::as_long), Some(3));
        let values: Vec<i64> = c.iter().take(3).filter_map(|(_, z)| z.as_long()).collect();
        assert_eq!(values, [1, 2, 3]);
        Heap::release(&mut nothing, Zval::array(c));
    }

    fn keys(a: Arr) -> Vec<i64> {
        a.iter()
            .map(|(k, _)| match k {
                Key::Long(n) => n,
                Key::Str(_) => -1,
            })
            .collect()
    }

    /// **Every number here is php's own**, read off the oracle with
    /// `memory_get_usage()` and `memory_get_peak_usage()` around the same
    /// appends. The 257th element takes a packed table of 512 slots,
    /// 8200 bytes, past the bins and into three pages, and the two-page
    /// table of 256 grows into them where it stands, the pages after it
    /// being free: usage and peak both read 56 + 12288 = 12344 at 300
    /// elements, and the address did not move. A block of 5000 bytes
    /// laid down after it takes the two pages that follow, so the growth
    /// to 1024 slots, five pages, cannot stay: it moves, and the peak is
    /// what `zend_mm_realloc_slow` restores, 56 + 20480 + 8192 = 28728,
    /// never the old `run` beside the new one.
    #[test]
    fn a_packed_table_past_the_bins_grows_where_it_stands_and_moves_when_it_cannot() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let start = nothing.heap.memory_usage(false);
        let Ok(a) = nothing.heap.array() else { return };
        for i in 0..300 {
            assert!(matches!(
                nothing.heap.array_append(a, Zval::long(i)),
                Ok(true)
            ));
        }
        assert_eq!(a.table_size(), 512);
        assert_eq!(nothing.heap.memory_usage(false) - start, 12344);
        assert_eq!(nothing.heap.memory_peak() - start, 12344);
        let stood = a.data_addr();
        let Ok(block) = nothing.heap.block(5000) else {
            return;
        };
        assert_eq!(nothing.heap.memory_usage(false) - start, 12344 + 8192);
        for i in 300..900 {
            assert!(matches!(
                nothing.heap.array_append(a, Zval::long(i)),
                Ok(true)
            ));
        }
        assert_eq!(a.table_size(), 1024);
        assert_ne!(
            a.data_addr(),
            stood,
            "the pages after the run were the block's"
        );
        assert_eq!(nothing.heap.memory_usage(false) - start, 28728);
        assert_eq!(nothing.heap.memory_peak() - start, 28728);
        assert_eq!(a.get(Key::Long(299)).and_then(Zval::as_long), Some(299));
        assert_eq!(a.get(Key::Long(899)).and_then(Zval::as_long), Some(899));
        nothing.heap.free_block(block);
        Heap::release(&mut nothing, Zval::array(a));
        assert_eq!(nothing.heap.memory_usage(false), start);
    }

    /// The layout is php's: a 56-byte header, thirty-two to a bucket.
    #[test]
    fn the_header_and_the_bucket_are_php_s_sizes() {
        assert_eq!(HEADER, 56);
        assert_eq!(BUCKET, 32);
        assert_eq!(Arr::data_size(8), 320);
        assert_eq!(Arr::packed_size(8), 136);
        assert_eq!(Arr::size_to_mask(8), 0xffff_fff0);
    }

    /// **Every number here is php's own.** `zend_new_array` is the 56-byte
    /// bin; the first `$a[] = v` lays a packed table of eight down,
    /// 136 bytes in the 160-byte bin; the ninth element doubles it, 264
    /// in the 320-byte bin, and the peak reads the larger alone, since
    /// `zend_mm_realloc_heap`'s small extension puts the peak back where
    /// the larger footprint had it (the oracle reads 376 for the two);
    /// and a string key turns it into a hash table of the same size,
    /// `HT_SIZE_EX(16, -32)`, 640 bytes.
    #[test]
    fn an_array_takes_the_bytes_php_s_takes() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let start = nothing.heap.memory_usage(false);
        let a = nothing.heap.array();
        assert!(a.is_ok(), "the heap would not allocate a header");
        let Ok(a) = a else { return };
        assert_eq!(nothing.heap.memory_usage(false), start + 56);
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(1)),
            Ok(true)
        ));
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 160);
        assert!(a.is_packed());
        for i in 2..=8 {
            assert!(matches!(
                nothing.heap.array_append(a, Zval::long(i)),
                Ok(true)
            ));
        }
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 160);
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(9)),
            Ok(true)
        ));
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 320);
        assert_eq!(nothing.heap.memory_peak(), start + 56 + 320);
        assert_eq!(a.table_size(), 16);
        assert_eq!(a.len(), 9);
        assert_eq!(a.next_free(), 9);
        let key = nothing.heap.string(b"name");
        let Ok(key) = key else { return };
        assert!(Heap::array_update(&mut nothing, a, Key::Str(key), Zval::long(10)).is_ok());
        assert!(!a.is_packed());
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 640 + 32);
        assert_eq!(key.refcount(), 2, "the bucket holds the key");
        assert_eq!(a.get(Key::Str(key)).and_then(Zval::as_long), Some(10));
        assert_eq!(a.get(Key::Long(3)).and_then(Zval::as_long), Some(4));
        nothing.heap.release_string(key);
        Heap::release(&mut nothing, Zval::array(a));
        assert_eq!(nothing.heap.memory_usage(false), start);
    }

    /// `zend_hash_str_update` makes the key after the table has room:
    /// at the ninth key the old data and the new stand together, 56 +
    /// 320 + 640 and eight 32-byte keys, and the ninth key comes after
    /// the old data is freed, so the peak never holds all three. A key
    /// written again keeps its bucket and its string and makes none.
    #[test]
    fn a_byte_key_is_made_after_the_table_has_room() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let start = nothing.heap.memory_usage(false);
        let Ok(a) = nothing.heap.array() else { return };
        let names: Vec<Vec<u8>> = (0..9).map(|i| format!("k{i}").into_bytes()).collect();
        for (i, name) in (0..8).zip(&names) {
            assert!(Heap::array_str_update(&mut nothing, a, name, Zval::long(i)).is_ok());
        }
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 320 + 8 * 32);
        nothing.heap.reset_peak();
        assert!(Heap::array_str_update(&mut nothing, a, &names[8], Zval::long(8)).is_ok());
        assert_eq!(nothing.heap.memory_peak(), start + 56 + 320 + 640 + 8 * 32);
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 640 + 9 * 32);
        assert!(Heap::array_str_update(&mut nothing, a, b"k3", Zval::long(30)).is_ok());
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 640 + 9 * 32);
        assert_eq!(a.len(), 9);
        assert_eq!(a.str_find(b"k3").and_then(Zval::as_long), Some(30));
        assert_eq!(a.str_find(b"k8").and_then(Zval::as_long), Some(8));
        Heap::release(&mut nothing, Zval::array(a));
        assert_eq!(nothing.heap.memory_usage(false), start);
    }

    /// A string key added to a packed table that is full turns it into a
    /// hash of the same size and then, as `_zend_hash_add_or_update_i`
    /// does past `zend_hash_packed_to_hash`, grows it before the bucket
    /// goes in: php's `memory_get_usage()` reads 56 + 640 + 32 after
    /// eight appends and `$a['name'] = 9`, and the ninth bucket is inside
    /// the table, not past its end.
    #[test]
    fn a_string_key_on_a_full_packed_table_grows_it_after_the_conversion() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let start = nothing.heap.memory_usage(false);
        let Ok(a) = nothing.heap.array() else { return };
        for i in 0..8 {
            assert!(matches!(
                nothing.heap.array_append(a, Zval::long(i)),
                Ok(true)
            ));
        }
        assert_eq!(a.table_size(), 8);
        let Ok(key) = nothing.heap.string(b"name") else {
            return;
        };
        assert!(Heap::array_update(&mut nothing, a, Key::Str(key), Zval::long(9)).is_ok());
        assert!(!a.is_packed());
        assert_eq!(a.table_size(), 16);
        assert_eq!(nothing.heap.memory_usage(false), start + 56 + 640 + 32);
        assert_eq!(a.get(Key::Str(key)).and_then(Zval::as_long), Some(9));
        assert_eq!(a.get(Key::Long(7)).and_then(Zval::as_long), Some(7));
        nothing.heap.release_string(key);
        Heap::release(&mut nothing, Zval::array(a));
        assert_eq!(nothing.heap.memory_usage(false), start);
    }

    /// `nNextFreeElement` under the 8.3 rule: a negative first key sets
    /// it, so `$a[-5] = 1; $a[] = 2` writes at `-4`; a key past it moves
    /// it; a string key leaves it; and a deleted last key leaves it too.
    #[test]
    fn the_next_free_element_is_php_s() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Ok(a) = nothing.heap.array() else { return };
        assert_eq!(a.next_free(), i64::MIN);
        assert!(Heap::array_update(&mut nothing, a, Key::Long(-5), Zval::long(1)).is_ok());
        assert_eq!(a.next_free(), -4);
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(2)),
            Ok(true)
        ));
        assert_eq!(keys(a), vec![-5, -4]);
        assert!(Heap::array_update(&mut nothing, a, Key::Long(10), Zval::long(3)).is_ok());
        assert_eq!(a.next_free(), 11);
        assert!(Heap::array_del(&mut nothing, a, Key::Long(10)));
        assert_eq!(a.next_free(), 11);
        assert_eq!(a.len(), 2);
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(4)),
            Ok(true)
        ));
        assert_eq!(keys(a), vec![-5, -4, 11]);
        // `ZEND_LONG_MAX` taken: the next insert is refused.
        assert!(Heap::array_update(&mut nothing, a, Key::Long(i64::MAX), Zval::null()).is_ok());
        assert_eq!(a.next_free(), i64::MAX);
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(5)),
            Ok(false)
        ));
        Heap::release(&mut nothing, Zval::array(a));
    }

    /// A deleted element is a tombstone until the rehash compacts it:
    /// `count()` falls at once, the order holds across it, and the
    /// internal pointer stands on the element it stood on. A string key
    /// first leaves `nNextFreeElement` at `ZEND_LONG_MIN`, so the first
    /// `$a[] =` after it writes at zero.
    #[test]
    fn a_deleted_element_is_a_tombstone_and_the_pointer_survives_the_rehash() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let start = nothing.heap.memory_usage(false);
        let Ok(a) = nothing.heap.array() else { return };
        let Ok(key) = nothing.strings.intern(b"k") else {
            return;
        };
        assert!(Heap::array_update(&mut nothing, a, Key::Str(key), Zval::long(0)).is_ok());
        for i in 1..8 {
            assert!(matches!(
                nothing.heap.array_append(a, Zval::long(i)),
                Ok(true)
            ));
        }
        assert_eq!(a.table_size(), 8);
        assert_eq!(keys(a), vec![-1, 0, 1, 2, 3, 4, 5, 6]);
        a.reset();
        assert!(a.forward() && a.forward() && a.forward());
        assert_eq!(
            a.entry_at(a.position()).and_then(|(_, z)| z.as_long()),
            Some(3)
        );
        assert!(Heap::array_del(&mut nothing, a, Key::Long(0)));
        assert!(Heap::array_del(&mut nothing, a, Key::Long(1)));
        assert_eq!(a.len(), 6);
        assert_eq!(keys(a), vec![-1, 2, 3, 4, 5, 6]);
        assert_eq!(a.position(), 3, "the pointer stands where it stood");
        // The ninth write finds the table full, and with two of eight
        // buckets tombstones, more than a thirty-second, it compacts in
        // place rather than doubling.
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(8)),
            Ok(true)
        ));
        assert_eq!(a.table_size(), 8);
        assert_eq!(keys(a), vec![-1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(a.position(), 1, "and moved with its element");
        assert_eq!(
            a.entry_at(a.position()).and_then(|(_, z)| z.as_long()),
            Some(3)
        );
        a.end();
        assert_eq!(
            a.entry_at(a.position()).and_then(|(_, z)| z.as_long()),
            Some(8)
        );
        assert!(a.forward());
        assert_eq!(a.entry_at(a.position()), None, "past the end");
        assert!(!a.forward(), "and no further");
        assert!(!a.backward(), "prev() past the end fails as php's does");
        Heap::release(&mut nothing, Zval::array(a));
        assert_eq!(nothing.heap.memory_usage(false), start);
    }

    /// A packed table stays packed under holes and `unset` of its last
    /// element trims `nNumUsed`, so a `$a[] =` after it writes there.
    #[test]
    fn a_packed_table_with_holes_and_its_trimmed_end() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Ok(a) = nothing.heap.array() else { return };
        assert!(Heap::array_update(&mut nothing, a, Key::Long(0), Zval::long(0)).is_ok());
        assert!(Heap::array_update(&mut nothing, a, Key::Long(3), Zval::long(3)).is_ok());
        assert!(a.is_packed(), "a key below the table size keeps it packed");
        assert_eq!(a.len(), 2);
        assert_eq!(keys(a), vec![0, 3]);
        assert!(!a.contains(Key::Long(1)));
        assert!(Heap::array_del(&mut nothing, a, Key::Long(3)));
        assert_eq!(a.next_free(), 4);
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(4)),
            Ok(true)
        ));
        assert_eq!(keys(a), vec![0, 4]);
        // Writing into a hole keeps the order, which packed cannot, so
        // the table becomes a hash.
        assert!(Heap::array_update(&mut nothing, a, Key::Long(2), Zval::long(2)).is_ok());
        assert!(!a.is_packed());
        assert_eq!(keys(a), vec![0, 4, 2]);
        Heap::release(&mut nothing, Zval::array(a));
    }

    /// `zend_array_dup` is `SEPARATE_ARRAY`'s copy: the elements one
    /// more owner each, the source one owner fewer, and a write to the
    /// copy leaves the source as it was.
    #[test]
    fn separation_copies_once_and_counts_the_elements() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let start = nothing.heap.memory_usage(false);
        let Ok(a) = nothing.heap.array() else { return };
        let Ok(s) = nothing.heap.string(b"shared") else {
            return;
        };
        assert!(matches!(
            nothing.heap.array_append(a, Zval::string(s)),
            Ok(true)
        ));
        let z = Zval::array(a);
        assert!(z.is_refcounted());
        nothing.heap.addref(z);
        assert_eq!(a.refcount(), 2);
        let before = nothing.heap.memory_usage(false);
        let Ok(b) = nothing.heap.separate(a) else {
            return;
        };
        assert!(!b.is_same(a), "two owners: a copy");
        assert_eq!(a.refcount(), 1);
        assert_eq!(b.refcount(), 1);
        assert_eq!(s.refcount(), 2, "the string is held by both arrays");
        assert_eq!(nothing.heap.memory_usage(false), before + 56 + 160);
        assert!(matches!(
            nothing.heap.array_append(b, Zval::long(2)),
            Ok(true)
        ));
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 2);
        assert!(
            nothing.heap.separate(b).is_ok_and(|c| c.is_same(b)),
            "one owner: itself"
        );
        Heap::release(&mut nothing, Zval::array(b));
        assert_eq!(s.refcount(), 1, "and by the one that is left");
        Heap::release(&mut nothing, z);
        assert_eq!(
            nothing.heap.memory_usage(false),
            start,
            "which took the string with it"
        );
    }

    /// A nested array is released with its holder, and so is a
    /// resource's payload when the array held its last owner, or by the
    /// sweep when nothing released it.
    #[test]
    fn what_an_array_holds_goes_with_it() {
        struct Tracked(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _alone = alone();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let mut nothing = Nothing::new();
            let start = nothing.heap.memory_usage(false);
            let Ok(outer) = nothing.heap.array() else {
                return;
            };
            let Ok(inner) = nothing.heap.array() else {
                return;
            };
            let Ok(held) = nothing
                .heap
                .resource_with(crate::ResourceType(1), Tracked(dropped.clone()))
            else {
                return;
            };
            let held = Zval::resource(held);
            assert!(held.as_resource().is_some());
            assert!(matches!(nothing.heap.array_append(inner, held), Ok(true)));
            assert!(matches!(
                nothing.heap.array_append(outer, Zval::array(inner)),
                Ok(true)
            ));
            let Ok(swept) = nothing
                .heap
                .resource_with(crate::ResourceType(1), Tracked(dropped.clone()))
            else {
                return;
            };
            assert!(nothing.heap.resource_native::<Tracked>(swept).is_some());
            Heap::release(&mut nothing, Zval::array(outer));
            assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
            // The swept resource's thirty-two bytes and its payload, and
            // the list's own packed table, 160, which the first resource
            // laid down and nothing gives back before the request ends.
            assert_eq!(nothing.heap.memory_usage(false), start + 160 + 32 + 16);
        }
        assert_eq!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "swept"
        );
    }

    /// A sort keeps the keys, compacts the holes, is stable, and leaves
    /// the internal pointer at the start.
    #[test]
    fn a_sort_keeps_keys_and_is_stable() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Ok(a) = nothing.heap.array() else { return };
        for v in [3, 1, 2, 1] {
            assert!(matches!(
                nothing.heap.array_append(a, Zval::long(v)),
                Ok(true)
            ));
        }
        assert!(Heap::array_del(&mut nothing, a, Key::Long(2)));
        a.end();
        let sorted = Heap::array_sort(&mut nothing, a, false, &mut |_, x, y| {
            x.val
                .as_long()
                .cmp(&y.val.as_long())
                .then(x.position.cmp(&y.position))
        });
        assert!(sorted.is_ok());
        assert_eq!(keys(a), vec![1, 3, 0]);
        assert_eq!(a.position(), 0);
        assert!(!a.is_packed());
        Heap::release(&mut nothing, Zval::array(a));
    }

    /// A renumbered sort drops the keys, packs the table and sets the
    /// next free element past the last, as `sort()` leaves an array.
    #[test]
    fn a_renumbered_sort_packs_the_table() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Ok(a) = nothing.heap.array() else { return };
        for v in [3, 1, 2] {
            assert!(matches!(
                nothing.heap.array_append(a, Zval::long(v)),
                Ok(true)
            ));
        }
        assert!(Heap::array_del(&mut nothing, a, Key::Long(1)));
        let sorted = Heap::array_sort(&mut nothing, a, true, &mut |_, x, y| {
            x.val.as_long().cmp(&y.val.as_long())
        });
        assert!(sorted.is_ok());
        assert_eq!(keys(a), vec![0, 1]);
        assert_eq!(a.get(Key::Long(0)).and_then(Zval::as_long), Some(2));
        assert_eq!(a.next_free(), 2);
        assert!(a.is_packed());
        Heap::release(&mut nothing, Zval::array(a));
    }

    /// `zend_hash_clean` over a hash of 100,000 rows, each key and value
    /// a string of the request's own: one pass, not the n²/2 bucket reads
    /// of a delete per row from position 0 that #2284 measured, which at
    /// this size is five billion reads and takes minutes; this test holds
    /// the clean to five seconds where one pass takes milliseconds. The
    /// table is left empty where it stands, its data and size kept, every
    /// key and value given back, and it takes rows again from the start.
    #[test]
    #[cfg_attr(
        miri,
        ignore = "a bound of five seconds over 100,000 rows, which Miri interprets in half an hour"
    )]
    fn a_clean_empties_a_hash_in_one_pass_and_keeps_its_data() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let start = nothing.heap.memory_usage(false);
        let Ok(a) = nothing.heap.array() else {
            return;
        };
        let rows = 100_000;
        for i in 0..rows {
            let (Ok(key), Ok(value)) = (
                nothing.heap.string(format!("key{i}").as_bytes()),
                nothing.heap.string(format!("value{i}").as_bytes()),
            ) else {
                return;
            };
            assert!(
                Heap::array_update(&mut nothing, a, Key::Str(key), Zval::string(value)).is_ok()
            );
            nothing.heap.release_string(key);
        }
        let Ok(three) = nothing.heap.string(b"key3") else {
            return;
        };
        assert!(Heap::array_del(&mut nothing, a, Key::Str(three)));
        let size = a.table_size();
        let held = a.len();
        assert_eq!(held, rows - 1);
        let began = std::time::Instant::now();
        Heap::array_clean(&mut nothing, a);
        let took = began.elapsed();
        assert!(
            took < std::time::Duration::from_secs(5),
            "a clean of {held} rows took {took:?}"
        );
        assert!(a.is_empty() && a.num_used() == 0 && a.position() == 0);
        assert_eq!(a.next_free(), i64::MIN);
        assert_eq!(a.table_size(), size, "the data is kept");
        assert!(a.get(Key::Str(three)).is_none());
        nothing.heap.release_string(three);
        assert!(Heap::array_update(&mut nothing, a, Key::Long(7), Zval::long(1)).is_ok());
        assert_eq!(a.live_from(0).map(|(pos, _, _)| pos), Some(0));
        assert_eq!(a.next_free(), 8);
        Heap::release(&mut nothing, Zval::array(a));
        assert_eq!(nothing.heap.memory_usage(false), start);
    }

    /// A hash of `n` string-keyed elements `k0`, `k1`, … holding their
    /// index.
    fn hash_of(nothing: &mut Nothing, n: i64) -> Option<Arr> {
        let a = nothing.heap.array().ok()?;
        for i in 0..n {
            let key = nothing.strings.intern(format!("k{i}").as_bytes()).ok()?;
            Heap::array_update(nothing, a, Key::Str(key), Zval::long(i)).ok()?;
        }
        Some(a)
    }

    /// `zend_hash_rehash` with iterators (`Z/Zend/zend_hash.c:1395-1434`):
    /// the compaction a full table makes moves every registered
    /// position with the bucket it stood at, and one past the end to the
    /// new end, while the table's count says how many name it.
    #[test]
    fn a_rehash_moves_every_iterator_with_its_bucket() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Some(a) = hash_of(&mut nothing, 8) else {
            return;
        };
        let at_five = nothing.heap.iterator_add(a, 5);
        let at_end = nothing.heap.iterator_add(a, 8);
        assert_eq!(a.iterators_count(), 2);
        for k in [b"k1", b"k2"] {
            let Ok(key) = nothing.strings.intern(k) else {
                return;
            };
            assert!(Heap::array_del(&mut nothing, a, Key::Str(key)));
        }
        assert_eq!(
            nothing.heap.iterator_pos(at_five, a),
            5,
            "a delete moves no position"
        );
        assert!(matches!(
            nothing.heap.array_append(a, Zval::long(9)),
            Ok(true)
        ));
        assert_eq!(
            nothing.heap.iterator_pos(at_five, a),
            3,
            "k5 now stands at 3"
        );
        assert_eq!(
            a.entry_at(3).and_then(|(_, z)| z.as_long()),
            Some(5),
            "the bucket the iterator names is the one it stood at"
        );
        assert_eq!(
            nothing.heap.iterator_pos(at_end, a),
            6,
            "the old end is the new end"
        );
        nothing.heap.iterator_del(at_five);
        nothing.heap.iterator_del(at_end);
        assert_eq!(a.iterators_count(), 0);
        Heap::release(&mut nothing, Zval::array(a));
    }

    /// `_zend_hash_del_el_ex`'s trim of `nNumUsed` clamps every iterator
    /// to the new end (`zend_hash_iterators_clamp_max`), and an emptied
    /// table's rehash puts them all at zero.
    #[test]
    fn a_trimmed_end_clamps_the_iterators_past_it() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Some(a) = hash_of(&mut nothing, 3) else {
            return;
        };
        let past = nothing.heap.iterator_add(a, 3);
        let Ok(key) = nothing.strings.intern(b"k2") else {
            return;
        };
        assert!(Heap::array_del(&mut nothing, a, Key::Str(key)));
        assert_eq!(nothing.heap.iterator_pos(past, a), 2);
        nothing.heap.iterator_del(past);
        Heap::release(&mut nothing, Zval::array(a));
    }

    /// `zend_array_dup_ht_iterators`: a copy on write carries a copy of
    /// every iterator on the source, on the iterator's ring, so the walk
    /// that finds the copy in its variable goes on at the copy's
    /// position (`zend_hash_iterator_find_copy_pos`), compacted with the
    /// copy's buckets; and a table destroyed under an iterator poisons
    /// it rather than leave it naming freed memory.
    #[test]
    fn a_copy_carries_the_iterators_and_a_destroy_poisons_them() {
        let _alone = alone();
        let mut nothing = Nothing::new();
        let Some(a) = hash_of(&mut nothing, 4) else {
            return;
        };
        let walk = nothing.heap.iterator_add(a, 3);
        let Ok(key) = nothing.strings.intern(b"k0") else {
            return;
        };
        assert!(Heap::array_del(&mut nothing, a, Key::Str(key)));
        a.addref();
        let Ok(copy) = nothing.heap.separate(a) else {
            return;
        };
        assert!(!copy.is_same(a));
        assert_eq!(copy.iterators_count(), 1, "the copy is walked too");
        assert_eq!(nothing.heap.iterator_pos(walk, copy), 2, "k3, compacted");
        assert_eq!(a.iterators_count(), 0, "and the source no longer");
        let Ok((pos, held)) = nothing.heap.iterator_pos_ex(walk, copy) else {
            return;
        };
        assert!(held.is_same(copy) && pos == 2);
        Heap::release(&mut nothing, Zval::array(a));
        Heap::release(&mut nothing, Zval::array(copy));
        nothing.heap.iterator_del(walk);
        let Some(b) = hash_of(&mut nothing, 1) else {
            return;
        };
        let again = nothing.heap.iterator_add(b, 0);
        assert_eq!(again, walk, "the freed slot is the next one given");
        nothing.heap.iterator_del(again);
        Heap::release(&mut nothing, Zval::array(b));
    }
}
