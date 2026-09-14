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
//! (`nIteratorsCount`, `EG(ht_iterators)`) are not here yet; the
//! internal pointer is, and every move of it is php's.

// A position, a size and a mask are `uint32_t` in php and here, and a
// hash slot's index is the low word of the hash; the narrowing casts
// below are php's own. The data pointer is the allocator's, which hands
// out eight-aligned bins and whole pages, so a bucket or a zval cast
// from it is aligned as php's `arData` is.
#![allow(clippy::cast_possible_truncation, clippy::cast_ptr_alignment)]

use std::cell::Cell;
use std::cmp::Ordering;
use std::ptr::NonNull;

use crate::gc::{self, Refcounted};
use crate::hooks::{Fatal, HeapHooks};
use crate::mm::Mm;
use crate::string::Str;
use crate::{Zval, ZvalType};

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
    /// `pDestructor`: `ZVAL_PTR_DTOR` for every array here, kept for the
    /// size.
    _dtor: usize,
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

/// An array key after php's normalisation (`ZEND_HANDLE_NUMERIC_STR`
/// is the engine's to apply): an integer, or a string that is not a
/// canonical decimal integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Long(i64),
    Str(Str),
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

impl Arr {
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
    pub(crate) fn payload(self) -> u64 {
        self.0.as_ptr().expose_provenance() as u64
    }

    /// The array a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Arr::payload`] for an array still live.
    pub(crate) unsafe fn from_payload(payload: u64) -> Option<Arr> {
        let at: *mut ArrHeader =
            std::ptr::with_exposed_provenance_mut(usize::try_from(payload).unwrap_or(0));
        NonNull::new(at).map(Arr)
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
                _dtor: 0,
            });
        }
        Arr(header)
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

    /// `HT_IS_PACKED`: the slots are the integer keys `0..nNumUsed`.
    #[must_use]
    pub fn is_packed(&self) -> bool {
        self.header().flags.get() & PACKED != 0
    }

    fn is_uninitialized(self) -> bool {
        self.header().flags.get() & UNINITIALIZED != 0
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
    pub fn addref(&self) {
        if !self.is_immutable() {
            self.header().gc.addref();
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
    pub(crate) fn delref(self) -> u32 {
        if self.is_immutable() {
            return 1;
        }
        self.header().gc.delref()
    }

    // ------------------------------------------------------ the data

    fn num_used(self) -> u32 {
        self.header().num_used.get()
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

    /// The bucket at `idx`, which is below `nTableSize`.
    fn bucket(self, idx: u32) -> NonNull<Bucket> {
        let data = self.header().data.get().cast::<Bucket>();
        // SAFETY: `idx` is below the table size, so the bucket is inside
        // the data allocation.
        let at = unsafe { data.add(idx as usize) };
        NonNull::new(at).unwrap_or(NonNull::dangling())
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

    /// The packed slot at `idx`, which is below `nTableSize`.
    fn slot(self, idx: u32) -> NonNull<Zval> {
        let data = self.header().data.get().cast::<Zval>();
        // SAFETY: `idx` is below the table size, so the slot is inside
        // the data allocation.
        let at = unsafe { data.add(idx as usize) };
        NonNull::new(at).unwrap_or(NonNull::dangling())
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

    /// The bucket or slot index `key` is at, in either layout.
    fn find(self, key: Key) -> Option<u32> {
        if self.is_uninitialized() {
            return None;
        }
        match key {
            Key::Long(n) if self.is_packed() => {
                let h = n.cast_unsigned();
                (h < u64::from(self.num_used()) && !self.read_slot(h as u32).is_undef())
                    .then_some(h as u32)
            }
            Key::Long(n) => self.find_long(n.cast_unsigned()),
            Key::Str(_) if self.is_packed() => None,
            Key::Str(s) => self.find_str(s),
        }
    }

    /// `zend_hash_find` and `zend_hash_index_find`: the value at `key`,
    /// copied, no owner taken.
    #[must_use]
    pub fn get(&self, key: Key) -> Option<Zval> {
        self.find(key).map(|idx| self.value_at(idx))
    }

    /// `zend_hash_exists`, `array_key_exists`.
    #[must_use]
    pub fn contains(&self, key: Key) -> bool {
        self.find(key).is_some()
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
    fn real_init_packed(self, mm: &mut Mm) -> Result<(), Fatal> {
        let at = mm.alloc(Arr::packed_size(self.table_size()))?;
        self.header().table_mask.set(MIN_MASK);
        self.set_data_addr(at);
        self.header().flags.set(PACKED | STATIC_KEYS);
        self.hash_reset();
        Ok(())
    }

    /// `zend_hash_real_init_mixed_ex`.
    fn real_init_mixed(self, mm: &mut Mm) -> Result<(), Fatal> {
        let size = self.table_size();
        let at = mm.alloc(Arr::data_size(size))?;
        self.header().table_mask.set(Arr::size_to_mask(size));
        self.set_data_addr(at);
        self.header().flags.set(STATIC_KEYS);
        self.hash_reset();
        Ok(())
    }

    /// `zend_hash_packed_to_hash`: the same elements as buckets, the
    /// larger allocation taken before the smaller goes back, then the
    /// rehash that links them.
    fn packed_to_hash(self, mm: &mut Mm) -> Result<(), Fatal> {
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
        self.rehash();
        Ok(())
    }

    /// `zend_hash_packed_grow`: twice the slots, php's `perealloc2`
    /// taken as the larger allocation, the used part copied, and the
    /// smaller given back.
    fn packed_grow(self, mm: &mut Mm) -> Result<(), Fatal> {
        let size = self.table_size();
        if size >= MAX_SIZE {
            return Err(overflow(size.wrapping_mul(2), BUCKET));
        }
        let grown = size * 2;
        let old = self.data_addr();
        let new = mm.alloc(Arr::packed_size(grown))?;
        let used_bytes = Arr::hash_size(MIN_MASK) + self.num_used() as usize * SLOT;
        if let Some(old) = old {
            // SAFETY: both allocations are at least `used_bytes` and do
            // not overlap.
            unsafe { new.copy_from_nonoverlapping(old, used_bytes) };
            // SAFETY: the old data was this heap's and its used part is
            // copied.
            unsafe { mm.free(old) };
        }
        self.set_data_addr(new);
        self.header().table_size.set(grown);
        Ok(())
    }

    /// `zend_hash_do_resize`: compact when a thirty-second of the table
    /// is tombstones, else double it, the used buckets copied and
    /// relinked.
    fn do_resize(self, mm: &mut Mm) -> Result<(), Fatal> {
        let used = self.num_used();
        let elements = self.header().num_elements.get();
        let size = self.table_size();
        if used > elements + (elements >> 5) {
            self.rehash();
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
            self.rehash();
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

    /// `zend_hash_rehash`: the chains rebuilt over the buckets, and the
    /// tombstones compacted out with the internal pointer moved past the
    /// ones it stood on or after.
    fn rehash(self) {
        let header = self.header();
        if header.num_elements.get() == 0 {
            if !self.is_uninitialized() {
                header.num_used.set(0);
                self.hash_reset();
                header.internal_pointer.set(0);
            }
            return;
        }
        self.hash_reset();
        let used = self.num_used();
        let mut i = 0;
        while i < used {
            if self.read_bucket(i).val.is_undef() {
                let mut j = i;
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
                        j += 1;
                    }
                    i += 1;
                }
                header.num_used.set(j);
                return;
            }
            self.link(i);
            i += 1;
        }
    }

    /// `_zend_hash_del_el_ex` after the unlink: one element fewer, the
    /// trailing tombstones dropped off `nNumUsed`, the internal pointer
    /// clamped, and the value handed back for release.
    fn del_at(self, idx: u32) -> Zval {
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
    Fatal::Error(format!(
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

    /// `zend_hash_update` or `zend_hash_index_update` over `a`: `z` is
    /// the array's from here, and what `key` held is released. The array
    /// has one owner, which the engine's `SEPARATE_ARRAY` made sure of.
    pub fn array_update(
        &mut self,
        a: Arr,
        key: Key,
        z: Zval,
        hooks: &mut dyn HeapHooks,
    ) -> Result<(), Fatal> {
        match key {
            Key::Long(n) => {
                let _ = self.array_write_long(a, n, z, Write::Update, hooks)?;
            }
            Key::Str(s) => self.array_write_str(a, s, z, hooks)?,
        }
        Ok(())
    }

    /// `zend_hash_next_index_insert`, `$a[] = z`: `false`, and `z` still
    /// the caller's, when `nNextFreeElement` is `ZEND_LONG_MAX` and taken.
    pub fn array_append(&mut self, a: Arr, z: Zval) -> Result<bool, Fatal> {
        let mut nothing = Unhooked;
        self.array_write_long(a, a.next_free(), z, Write::Next, &mut nothing)
    }

    /// `_zend_hash_index_add_or_update_i`.
    fn array_write_long(
        &mut self,
        a: Arr,
        index: i64,
        z: Zval,
        write: Write,
        hooks: &mut dyn HeapHooks,
    ) -> Result<bool, Fatal> {
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
                        return Ok(false);
                    }
                    a.write_slot(idx, z);
                    self.release(old, hooks);
                    return Ok(true);
                }
                a.packed_to_hash(&mut self.mm)?;
            } else if h < u64::from(a.table_size()) {
                a.add_to_packed(h as u32, z);
                return Ok(true);
            } else if (h >> 1) < u64::from(a.table_size())
                && (a.table_size() >> 1) < header.num_elements.get()
            {
                a.packed_grow(&mut self.mm)?;
                a.add_to_packed(h as u32, z);
                return Ok(true);
            } else {
                if a.num_used() >= a.table_size() {
                    header.table_size.set(a.table_size() + a.table_size());
                }
                a.packed_to_hash(&mut self.mm)?;
            }
        } else if a.is_uninitialized() {
            if h < u64::from(a.table_size()) {
                a.real_init_packed(&mut self.mm)?;
                a.add_to_packed(h as u32, z);
                return Ok(true);
            }
            a.real_init_mixed(&mut self.mm)?;
        } else {
            if let Some(idx) = a.find_long(h) {
                if write == Write::Next {
                    return Ok(false);
                }
                let mut b = a.read_bucket(idx);
                let old = b.val;
                b.val = z;
                b.val.set_next(old.next());
                a.write_bucket(idx, b);
                self.release(old, hooks);
                return Ok(true);
            }
            if a.num_used() >= a.table_size() {
                a.do_resize(&mut self.mm)?;
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
        Ok(true)
    }

    /// `_zend_hash_add_or_update_i` with `HASH_UPDATE`: the key is the
    /// bucket's own from here, one more owner unless it is interned.
    fn array_write_str(
        &mut self,
        a: Arr,
        key: Str,
        z: Zval,
        hooks: &mut dyn HeapHooks,
    ) -> Result<(), Fatal> {
        let h = key.hash();
        let header = a.header();
        if a.is_uninitialized() {
            a.real_init_mixed(&mut self.mm)?;
        } else if a.is_packed() {
            a.packed_to_hash(&mut self.mm)?;
        } else {
            if let Some(idx) = a.find_str(key) {
                let mut b = a.read_bucket(idx);
                let old = b.val;
                b.val = z;
                b.val.set_next(old.next());
                a.write_bucket(idx, b);
                self.release(old, hooks);
                return Ok(());
            }
            if a.num_used() >= a.table_size() {
                a.do_resize(&mut self.mm)?;
            }
        }
        if !key.is_interned() {
            key.addref();
            header.flags.set(header.flags.get() & !STATIC_KEYS);
        }
        let idx = a.num_used();
        header.num_used.set(idx + 1);
        header.num_elements.set(header.num_elements.get() + 1);
        a.write_bucket(
            idx,
            Bucket {
                val: z,
                h,
                key: Some(key),
            },
        );
        a.link(idx);
        Ok(())
    }

    /// `zend_hash_del` and `zend_hash_index_del`: the element at `key`
    /// gone, its value released and its string key given up; `false` for
    /// a key that is not there.
    pub fn array_del(&mut self, a: Arr, key: Key, hooks: &mut dyn HeapHooks) -> bool {
        let Some(idx) = a.find(key) else {
            return false;
        };
        if !a.is_packed() {
            a.unlink(idx);
            if let Some(k) = a.read_bucket(idx).key {
                self.release_string(k);
            }
        }
        let old = a.del_at(idx);
        self.release(old, hooks);
        true
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
                let z = a.read_slot(i);
                self.addref(z);
                target.write_slot(i, z);
            }
            return Ok(target);
        }
        target.real_init_mixed(&mut self.mm)?;
        header
            .flags
            .set(source.flags.get() & (PACKED | UNINITIALIZED | STATIC_KEYS));
        header
            .internal_pointer
            .set(if pointer < used { pointer } else { 0 });
        let mut idx = 0;
        for i in 0..used {
            let b = a.read_bucket(i);
            if b.val.is_undef() {
                continue;
            }
            self.addref(b.val);
            if let Some(k) = b.key {
                k.addref();
            }
            target.write_bucket(idx, b);
            target.link(idx);
            if pointer == i {
                header.internal_pointer.set(idx);
            }
            idx += 1;
        }
        header.num_used.set(idx);
        header.num_elements.set(idx);
        Ok(target)
    }

    /// `SEPARATE_ARRAY`: `a` itself while it has one owner, else a copy
    /// with one owner and `a` with one fewer, which is what a write to a
    /// shared array does before it writes (design §5.3).
    pub fn separate(&mut self, a: Arr) -> Result<Arr, Fatal> {
        if a.refcount() == 1 && !a.is_immutable() {
            return Ok(a);
        }
        let copy = self.array_dup(a)?;
        let _ = a.delref();
        Ok(copy)
    }

    /// `zend_hash_sort` without renumbering: the holes compacted, the
    /// elements sorted by `compare`, stable as php's `sort()` is since
    /// 8.0, the internal pointer reset and the chains rebuilt. A packed
    /// table becomes a hash table first, as php's does.
    pub fn array_sort(
        &mut self,
        a: Arr,
        compare: &mut dyn FnMut(Zval, Zval) -> Ordering,
    ) -> Result<(), Fatal> {
        if a.len() <= 1 {
            return Ok(());
        }
        if a.is_packed() {
            a.packed_to_hash(&mut self.mm)?;
        }
        let mut buckets: Vec<Bucket> = (0..a.num_used())
            .map(|i| a.read_bucket(i))
            .filter(|b| !b.val.is_undef())
            .collect();
        buckets.sort_by(|x, y| compare(x.val, y.val));
        let header = a.header();
        header.num_used.set(buckets.len() as u32);
        for (i, b) in buckets.into_iter().enumerate() {
            a.write_bucket(i as u32, b);
        }
        header.internal_pointer.set(0);
        a.rehash();
        Ok(())
    }

    /// `zend_array_destroy`: every element released, every string key
    /// given up, the data and then the header back to this heap.
    pub(crate) fn array_destroy(&mut self, a: Arr, hooks: &mut dyn HeapHooks) {
        let used = a.num_used();
        if a.is_packed() {
            for i in 0..used {
                self.release(a.read_slot(i), hooks);
            }
        } else if !a.is_uninitialized() {
            for i in 0..used {
                let b = a.read_bucket(i);
                if b.val.is_undef() {
                    continue;
                }
                self.release(b.val, hooks);
                if let Some(k) = b.key {
                    self.release_string(k);
                }
            }
        }
        if let Some(data) = a.data_addr() {
            // SAFETY: the data was this heap's and every element in it is
            // released.
            unsafe { self.mm.free(data) };
        }
        // SAFETY: the header came from `array_sized` and its last owner
        // has given it up.
        unsafe { self.mm.free(a.address()) };
    }
}

/// What a write passes where nothing it releases can reach user code:
/// the next-index insert refuses an occupied slot and replaces nothing.
pub(crate) struct Unhooked;

impl HeapHooks for Unhooked {
    fn destruct(&mut self, _obj: crate::ObjectId) {}
    fn free_native(&mut self, _obj: crate::ObjectId) {}
    fn gc_children(&mut self, _obj: crate::ObjectId, _visit: &mut dyn FnMut(Zval)) {}
    fn free_resource(&mut self, _res: u32) {}
    fn out_of_memory(&mut self, bytes: usize) -> Fatal {
        Fatal::OutOfMemory(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Heap, Intern};

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

    use Unhooked as Nothing;

    fn keys(a: Arr) -> Vec<i64> {
        a.iter()
            .map(|(k, _)| match k {
                Key::Long(n) => n,
                Key::Str(_) => -1,
            })
            .collect()
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
    /// in the 320-byte bin, the larger taken before the smaller goes
    /// back; and a string key turns it into a hash table of the same
    /// size, `HT_SIZE_EX(16, -32)`, 640 bytes.
    #[test]
    fn an_array_takes_the_bytes_php_s_takes() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut nothing = Nothing;
        let a = heap.array();
        assert!(a.is_ok(), "the heap would not allocate a header");
        let Ok(a) = a else { return };
        assert_eq!(heap.memory_usage(false), 56);
        assert!(matches!(heap.array_append(a, Zval::long(1)), Ok(true)));
        assert_eq!(heap.memory_usage(false), 56 + 160);
        assert!(a.is_packed());
        for i in 2..=8 {
            assert!(matches!(heap.array_append(a, Zval::long(i)), Ok(true)));
        }
        assert_eq!(heap.memory_usage(false), 56 + 160);
        assert!(matches!(heap.array_append(a, Zval::long(9)), Ok(true)));
        assert_eq!(heap.memory_usage(false), 56 + 320);
        assert_eq!(heap.memory_peak(), 56 + 160 + 320);
        assert_eq!(a.table_size(), 16);
        assert_eq!(a.len(), 9);
        assert_eq!(a.next_free(), 9);
        let key = heap.string(b"name");
        let Ok(key) = key else { return };
        assert!(
            heap.array_update(a, Key::Str(key), Zval::long(10), &mut nothing)
                .is_ok()
        );
        assert!(!a.is_packed());
        assert_eq!(heap.memory_usage(false), 56 + 640 + 32);
        assert_eq!(key.refcount(), 2, "the bucket holds the key");
        assert_eq!(a.get(Key::Str(key)).and_then(Zval::as_long), Some(10));
        assert_eq!(a.get(Key::Long(3)).and_then(Zval::as_long), Some(4));
        heap.release_string(key);
        heap.release(Zval::array(a), &mut nothing);
        assert_eq!(heap.memory_usage(false), 0);
    }

    /// `nNextFreeElement` under the 8.3 rule: a negative first key sets
    /// it, so `$a[-5] = 1; $a[] = 2` writes at `-4`; a key past it moves
    /// it; a string key leaves it; and a deleted last key leaves it too.
    #[test]
    fn the_next_free_element_is_php_s() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut nothing = Nothing;
        let Ok(a) = heap.array() else { return };
        assert_eq!(a.next_free(), i64::MIN);
        assert!(
            heap.array_update(a, Key::Long(-5), Zval::long(1), &mut nothing)
                .is_ok()
        );
        assert_eq!(a.next_free(), -4);
        assert!(matches!(heap.array_append(a, Zval::long(2)), Ok(true)));
        assert_eq!(keys(a), vec![-5, -4]);
        assert!(
            heap.array_update(a, Key::Long(10), Zval::long(3), &mut nothing)
                .is_ok()
        );
        assert_eq!(a.next_free(), 11);
        assert!(heap.array_del(a, Key::Long(10), &mut nothing));
        assert_eq!(a.next_free(), 11);
        assert_eq!(a.len(), 2);
        assert!(matches!(heap.array_append(a, Zval::long(4)), Ok(true)));
        assert_eq!(keys(a), vec![-5, -4, 11]);
        // `ZEND_LONG_MAX` taken: the next insert is refused.
        assert!(
            heap.array_update(a, Key::Long(i64::MAX), Zval::null(), &mut nothing)
                .is_ok()
        );
        assert_eq!(a.next_free(), i64::MAX);
        assert!(matches!(heap.array_append(a, Zval::long(5)), Ok(false)));
        heap.release(Zval::array(a), &mut nothing);
    }

    /// A deleted element is a tombstone until the rehash compacts it:
    /// `count()` falls at once, the order holds across it, and the
    /// internal pointer stands on the element it stood on. A string key
    /// first leaves `nNextFreeElement` at `ZEND_LONG_MIN`, so the first
    /// `$a[] =` after it writes at zero.
    #[test]
    fn a_deleted_element_is_a_tombstone_and_the_pointer_survives_the_rehash() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut nothing = Nothing;
        let Ok(a) = heap.array() else { return };
        let Ok(key) = heap.intern(b"k") else { return };
        assert!(
            heap.array_update(a, Key::Str(key), Zval::long(0), &mut nothing)
                .is_ok()
        );
        for i in 1..8 {
            assert!(matches!(heap.array_append(a, Zval::long(i)), Ok(true)));
        }
        assert_eq!(a.table_size(), 8);
        assert_eq!(keys(a), vec![-1, 0, 1, 2, 3, 4, 5, 6]);
        a.reset();
        assert!(a.forward() && a.forward() && a.forward());
        assert_eq!(
            a.entry_at(a.position()).and_then(|(_, z)| z.as_long()),
            Some(3)
        );
        assert!(heap.array_del(a, Key::Long(0), &mut nothing));
        assert!(heap.array_del(a, Key::Long(1), &mut nothing));
        assert_eq!(a.len(), 6);
        assert_eq!(keys(a), vec![-1, 2, 3, 4, 5, 6]);
        assert_eq!(a.position(), 3, "the pointer stands where it stood");
        // The ninth write finds the table full, and with two of eight
        // buckets tombstones, more than a thirty-second, it compacts in
        // place rather than doubling.
        assert!(matches!(heap.array_append(a, Zval::long(8)), Ok(true)));
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
        heap.release(Zval::array(a), &mut nothing);
        assert_eq!(heap.memory_usage(false), 0);
    }

    /// A packed table stays packed under holes and `unset` of its last
    /// element trims `nNumUsed`, so a `$a[] =` after it writes there.
    #[test]
    fn a_packed_table_with_holes_and_its_trimmed_end() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut nothing = Nothing;
        let Ok(a) = heap.array() else { return };
        assert!(
            heap.array_update(a, Key::Long(0), Zval::long(0), &mut nothing)
                .is_ok()
        );
        assert!(
            heap.array_update(a, Key::Long(3), Zval::long(3), &mut nothing)
                .is_ok()
        );
        assert!(a.is_packed(), "a key below the table size keeps it packed");
        assert_eq!(a.len(), 2);
        assert_eq!(keys(a), vec![0, 3]);
        assert!(!a.contains(Key::Long(1)));
        assert!(heap.array_del(a, Key::Long(3), &mut nothing));
        assert_eq!(a.next_free(), 4);
        assert!(matches!(heap.array_append(a, Zval::long(4)), Ok(true)));
        assert_eq!(keys(a), vec![0, 4]);
        // Writing into a hole keeps the order, which packed cannot, so
        // the table becomes a hash.
        assert!(
            heap.array_update(a, Key::Long(2), Zval::long(2), &mut nothing)
                .is_ok()
        );
        assert!(!a.is_packed());
        assert_eq!(keys(a), vec![0, 4, 2]);
        heap.release(Zval::array(a), &mut nothing);
    }

    /// `zend_array_dup` is `SEPARATE_ARRAY`'s copy: the elements one
    /// more owner each, the source one owner fewer, and a write to the
    /// copy leaves the source as it was.
    #[test]
    fn separation_copies_once_and_counts_the_elements() {
        let _alone = alone();
        let mut heap = Heap::new();
        let mut nothing = Nothing;
        let Ok(a) = heap.array() else { return };
        let Ok(s) = heap.string(b"shared") else {
            return;
        };
        assert!(matches!(heap.array_append(a, Zval::string(s)), Ok(true)));
        let z = Zval::array(a);
        assert!(z.is_refcounted());
        heap.addref(z);
        assert_eq!(a.refcount(), 2);
        let before = heap.memory_usage(false);
        let Ok(b) = heap.separate(a) else { return };
        assert!(!b.is_same(a), "two owners: a copy");
        assert_eq!(a.refcount(), 1);
        assert_eq!(b.refcount(), 1);
        assert_eq!(s.refcount(), 2, "the string is held by both arrays");
        assert_eq!(heap.memory_usage(false), before + 56 + 160);
        assert!(matches!(heap.array_append(b, Zval::long(2)), Ok(true)));
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 2);
        assert!(
            heap.separate(b).is_ok_and(|c| c.is_same(b)),
            "one owner: itself"
        );
        heap.release(Zval::array(b), &mut nothing);
        assert_eq!(s.refcount(), 1, "and by the one that is left");
        heap.release(z, &mut nothing);
        assert_eq!(heap.memory_usage(false), 0, "which took the string with it");
    }

    /// A nested array is released with its holder, and a boxed stand-in
    /// is dropped when its last owner goes, or swept with the heap.
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
            let mut heap = Heap::new();
            let mut nothing = Nothing;
            let Ok(outer) = heap.array() else { return };
            let Ok(inner) = heap.array() else { return };
            let boxed = heap.boxed(ZvalType::Object, Box::new(Tracked(dropped.clone())));
            assert!(boxed.is_boxed());
            assert!(boxed.as_boxed::<Tracked>().is_some());
            assert!(matches!(heap.array_append(inner, boxed), Ok(true)));
            assert!(matches!(
                heap.array_append(outer, Zval::array(inner)),
                Ok(true)
            ));
            let swept = heap.boxed(ZvalType::Resource, Box::new(Tracked(dropped.clone())));
            assert!(swept.as_boxed::<Tracked>().is_some());
            heap.release(Zval::array(outer), &mut nothing);
            assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(heap.memory_usage(false), 0);
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
        let mut heap = Heap::new();
        let mut nothing = Nothing;
        let Ok(a) = heap.array() else { return };
        for v in [3, 1, 2, 1] {
            assert!(matches!(heap.array_append(a, Zval::long(v)), Ok(true)));
        }
        assert!(heap.array_del(a, Key::Long(2), &mut nothing));
        a.end();
        let sorted = heap.array_sort(a, &mut |x, y| x.as_long().cmp(&y.as_long()));
        assert!(sorted.is_ok());
        assert_eq!(keys(a), vec![1, 3, 0]);
        assert_eq!(a.position(), 0);
        assert!(!a.is_packed());
        heap.release(Zval::array(a), &mut nothing);
    }
}
