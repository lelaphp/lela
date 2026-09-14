//! Objects (design §5.4): php's `zend_object` and the store it stands in,
//! `zend_objects_store`, copied from `Z/Zend/zend_objects.c` and
//! `Z/Zend/zend_objects_API.c` at the tag. The store is a table of ids
//! with a free list, which is what `spl_object_id()`, a weak reference
//! and the collector's enumeration read; an object is one allocation of
//! the request's heap, the forty-byte header php's `sizeof(zend_object)`
//! is before its one property slot, then a zval per declared property,
//! then, for an internal class, the native payload as one typed slot.
//!
//! The layout is php's in size, because `memory_get_usage()` reads it:
//! `zend_object_alloc` (`Z/Zend/zend_objects_API.h:93`) takes
//! `sizeof(zend_object)` and `sizeof(zval)` per declared property less
//! the one the struct already holds, so a class of no properties is
//! forty bytes and each property sixteen more. Where php puts the
//! payload before the header and reaches the header through
//! `handlers->offset`, this puts it after the slots and reaches it
//! through the header; the bytes are the same and the pointer arithmetic
//! goes one way.
//!
//! An [`Obj`] is the pointer php's `zend_object *` is, `Copy`, no `Drop`,
//! its owners counted by [`crate::Heap::addref`] and
//! [`crate::Heap::release`]; the release that reaches zero is
//! `zend_objects_store_del`, which serves the destructor and the native
//! payload's release through [`HeapHooks`] with the heap handed back for
//! the duration, because a destructor is PHP code and PHP code allocates.
//!
//! **What the payload's type gives the heap.** php's `free_obj` is the
//! class's, written by hand per internal class; here the slot's drop is
//! derived from the payload's type, one table per `T` promoted from a
//! constant, and the values of the request's heap a payload names are
//! given back through [`HeapHooks::free_native`], by the engine that knows
//! the type, before the drop runs. No extension body downcasts through
//! `Any` to reach its own state: [`crate::Heap::native`] is the one door,
//! and it asks the type.

// A handle, a slot count and a table size are `uint32_t` in php and here;
// the narrowing casts below are php's own. The pointer is the allocator's,
// eight-aligned, and the header's alignment is eight.
#![allow(clippy::cast_possible_truncation)]

use std::alloc::Layout;
use std::any::{Any, TypeId};
use std::cell::Cell;
use std::ptr::NonNull;

use crate::array::Arr;
use crate::gc::{self, Refcounted};
use crate::hooks::{Fatal, HeapHooks};
use crate::mm::Mm;
use crate::{Heap, Zval, ZvalType};

/// The identity of an object in the store (design §5.4): the index php
/// reports as `spl_object_id()`, and what weak references, `WeakMap`
/// keys and the garbage collector enumerate by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u32);

/// `sizeof(zend_object) - sizeof(zval)`: the header before the slots.
const HEADER: usize = size_of::<ObjHeader>();
/// `sizeof(zval)`, one property slot.
const SLOT: usize = size_of::<Zval>();
/// The store's `init_size`: `zend_objects_store_init` is given 1024 by
/// `init_executor` (`Z/Zend/zend_execute_API.c:171`).
const INIT_SIZE: u32 = 1024;
/// `sizeof(zend_object *)`, one bucket of the store.
const BUCKET: usize = size_of::<usize>();
/// `OBJ_BUCKET_INVALID`: the low bit of a bucket that holds no object.
const BUCKET_INVALID: usize = 1;
/// The store's table at request start, what `memory_get_usage()` reads
/// before the request serves a line: 1024 buckets of a pointer each.
pub(crate) const INIT_BYTES: usize = INIT_SIZE as usize * BUCKET;

/// What the heap needs of a payload's type, php's `handlers->offset` and
/// `free_obj` for the slot alone: its layout, its type and its drop. One
/// per type, promoted to `'static` from a constant, so the header holds
/// a thin pointer where php holds `handlers`.
struct Payload {
    layout: Layout,
    type_id: fn() -> TypeId,
    drop: unsafe fn(NonNull<u8>),
}

/// `ptr::drop_in_place` over the payload as its type.
///
/// # Safety
///
/// `at` is a live, initialised `T` nothing will read again.
unsafe fn drop_payload<T>(at: NonNull<u8>) {
    // SAFETY: the caller's contract: a live `T` at `at`.
    unsafe { std::ptr::drop_in_place(at.cast::<T>().as_ptr()) };
}

/// The one table for `T`.
fn payload_of<T: Any + Send>() -> &'static Payload {
    const {
        &Payload {
            layout: Layout::new::<T>(),
            type_id: TypeId::of::<T>,
            drop: drop_payload::<T>,
        }
    }
}

/// `zend_object`: forty bytes, php's own order, out of the heap, the
/// property slots after it.
#[repr(C)]
pub(crate) struct ObjHeader {
    /// `zend_refcounted_h gc`, whose flags carry
    /// `IS_OBJ_DESTRUCTOR_CALLED` and `IS_OBJ_FREE_CALLED`.
    gc: Refcounted,
    /// php's `handle`: the store's index, `spl_object_id()`.
    handle: Cell<u32>,
    /// `extra_flags`, `OBJ_EXTRA_FLAGS()`: kept for the size.
    _extra_flags: u32,
    /// php's `ce`: the class as the engine numbers it, which the heap
    /// reads nothing of, and the slots the class laid down, which php
    /// reads off `ce->default_properties_count`.
    class: u32,
    slots: u32,
    /// php's `handlers`: what the heap needs of the table, the payload's
    /// type; none for a class with no native payload.
    payload: Cell<Option<&'static Payload>>,
    /// `properties`: the dynamic table, none until something
    /// materialises it (`rebuild_object_properties`).
    properties: Cell<Option<Arr>>,
}

/// One object: php's `zend_object *`. Naming one does not keep it alive,
/// exactly as php's pointer does not; the engine's kernels take and give
/// up owners through [`crate::Heap::addref`] and [`crate::Heap::release`].
#[derive(Clone, Copy)]
pub struct Obj(NonNull<ObjHeader>);

// SAFETY: an `Obj` is the pointer php's `zend_object *` is: a request's
// object moves with the request that owns it, whole, to the thread that
// serves it (design §5.14), and its payload is `Send` by the bound on
// `Heap::object_with`. The refcount `Cell` is what keeps it off `Sync`.
unsafe impl Send for Obj {}

impl Obj {
    fn header(&self) -> &ObjHeader {
        // SAFETY: an `Obj` names an object `init` laid down, live while
        // its owner holds it, which is php's `zend_object *` contract.
        unsafe { self.0.as_ref() }
    }

    /// Where the header sits, which is where the allocation starts, for
    /// the heap that frees it.
    pub(crate) const fn address(self) -> NonNull<u8> {
        self.0.cast()
    }

    /// The payload word of a zval naming this object.
    pub(crate) fn payload(self) -> u64 {
        self.0.as_ptr().expose_provenance() as u64
    }

    /// The object a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Obj::payload`] for an object still live.
    pub(crate) unsafe fn from_payload(payload: u64) -> Option<Obj> {
        let at: *mut ObjHeader =
            std::ptr::with_exposed_provenance_mut(usize::try_from(payload).unwrap_or(0));
        NonNull::new(at).map(Obj)
    }

    /// `zend_object_alloc`'s arithmetic: the header, a zval per slot and
    /// the payload after them at its own alignment, which the allocator's
    /// eight-byte alignment can honour up to eight.
    fn allocation_size(slots: u32, payload: Option<&Payload>) -> Result<usize, Fatal> {
        let members = HEADER + SLOT * slots as usize;
        let Some(payload) = payload else {
            return Ok(members);
        };
        if payload.layout.align() > 8 {
            return Err(Fatal::Error(format!(
                "InternalError: a native payload aligned to {} cannot sit on the request heap",
                payload.layout.align()
            )));
        }
        Ok(members.next_multiple_of(payload.layout.align()) + payload.layout.size())
    }

    /// `_zend_object_std_init` over a fresh allocation: one owner, the
    /// slots `IS_UNDEF` until `object_properties_init` lays the class's
    /// defaults down, no dynamic table, and no id until the store gives
    /// one.
    ///
    /// # Safety
    ///
    /// `at` is writable for [`Obj::allocation_size`] bytes and holds
    /// nothing else.
    unsafe fn init(
        at: NonNull<u8>,
        class: u32,
        slots: u32,
        payload: Option<&'static Payload>,
    ) -> Obj {
        let header: NonNull<ObjHeader> = at.cast();
        // SAFETY: the caller's allocation is the header and the slots and
        // nothing else holds it.
        unsafe {
            header.as_ptr().write(ObjHeader {
                gc: Refcounted::new(ZvalType::Object, 0),
                handle: Cell::new(0),
                _extra_flags: 0,
                class,
                slots,
                payload: Cell::new(payload),
                properties: Cell::new(None),
            });
        }
        let o = Obj(header);
        for i in 0..slots {
            o.write_slot(i, Zval::UNDEF);
        }
        o
    }

    /// `spl_object_id()`: the index the store gave.
    #[must_use]
    pub fn id(&self) -> ObjectId {
        ObjectId(self.header().handle.get())
    }

    /// The class, as the engine numbered it when it built the object.
    #[must_use]
    pub fn class(&self) -> u32 {
        self.header().class
    }

    /// The declared property slots, `ce->default_properties_count`.
    #[must_use]
    pub fn slots(&self) -> u32 {
        self.header().slots
    }

    /// `GC_REFCOUNT`.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        self.header().gc.refcount()
    }

    /// `GC_ADDREF`.
    pub fn addref(&self) {
        self.header().gc.addref();
    }

    /// `GC_DELREF`, and what is left.
    pub(crate) fn delref(self) -> u32 {
        self.header().gc.delref()
    }

    /// Whether two names name one object, php's pointer equality, which
    /// `===` is for objects.
    #[must_use]
    pub fn is_same(&self, other: Obj) -> bool {
        self.0 == other.0
    }

    /// `IS_OBJ_LAZY_UNINITIALIZED`: a lazy ghost or proxy nothing has
    /// initialised (design §5.4). The bit sits above the flags, where
    /// php keeps it (`Z/Zend/zend_types.h:867`).
    #[must_use]
    pub fn is_lazy(&self) -> bool {
        self.header().gc.has_info(gc::OBJ_LAZY_UNINITIALIZED)
    }

    /// The dynamic table, once something has materialised it.
    #[must_use]
    pub fn properties(&self) -> Option<Arr> {
        self.header().properties.get()
    }

    /// Whether the object carries a native payload.
    #[must_use]
    pub fn has_native(&self) -> bool {
        self.header().payload.get().is_some()
    }

    fn has_flag(self, flag: u32) -> bool {
        self.header().gc.flags() & flag != 0
    }

    fn add_flag(self, flag: u32) {
        self.header().gc.add_flags(flag);
    }

    fn slot_ptr(self, i: u32) -> *mut Zval {
        // SAFETY: `i` is below `slots`, which the callers hold, so the
        // slot is inside the allocation `init` laid down.
        unsafe { self.address().as_ptr().add(HEADER + SLOT * i as usize) }.cast()
    }

    /// `OBJ_PROP_NUM`: the slot's zval, and `UNDEF` for a slot the class
    /// did not lay down, which reads as an uninitialised property does.
    #[must_use]
    pub fn slot(&self, i: u32) -> Zval {
        if i >= self.slots() {
            return Zval::UNDEF;
        }
        // SAFETY: the slot is inside the allocation, and every slot was
        // written by `init`.
        unsafe { self.slot_ptr(i).read() }
    }

    /// Write the slot as it stands, no owner touched; a slot the class
    /// did not lay down is not written.
    pub(crate) fn write_slot(self, i: u32, z: Zval) {
        if i >= self.slots() {
            return;
        }
        // SAFETY: the slot is inside the allocation.
        unsafe { self.slot_ptr(i).write(z) };
    }

    /// Where the payload sits, after the slots at its own alignment.
    fn payload_addr(self, payload: &Payload) -> NonNull<u8> {
        let offset =
            (HEADER + SLOT * self.slots() as usize).next_multiple_of(payload.layout.align());
        // SAFETY: the allocation was sized by `allocation_size` to hold
        // the payload at this offset.
        unsafe { self.address().add(offset) }
    }

    /// The payload as `T`, when the object carries one of that type.
    fn native_ptr<T: Any + Send>(self) -> Option<NonNull<T>> {
        let payload = self.header().payload.get()?;
        if (payload.type_id)() != TypeId::of::<T>() {
            return None;
        }
        Some(self.payload_addr(payload).cast())
    }

    /// Drop the payload once, and never again: the table is taken off
    /// the header first, so the store's sweep and `free_obj` cannot both
    /// drop it.
    fn drop_payload(self) {
        let Some(payload) = self.header().payload.take() else {
            return;
        };
        let at = self.payload_addr(payload);
        // SAFETY: the payload was written by `object_with` at this
        // address, has not been dropped, since the table was still on
        // the header, and nothing reads it after this.
        unsafe { (payload.drop)(at) };
    }
}

impl std::fmt::Debug for Obj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "object(#{} of class {})", self.id().0, self.class())
    }
}

/// `zend_objects_store`: the bucket array, php's `object_buckets`, out
/// of the request's heap and sized 1024 at request start, `top` the
/// next id never given, and the free list threaded through the buckets
/// given back, its head an index or none.
///
/// A bucket holds the object's address, or, with `OBJ_BUCKET_INVALID`
/// set in its low bit, the next free id shifted up one
/// (`SET_OBJ_BUCKET_NUMBER`), exactly as php packs it; bucket zero is
/// never given, so an id is never false.
pub(crate) struct Store {
    buckets: Option<NonNull<usize>>,
    size: u32,
    top: u32,
    free_list_head: Option<u32>,
    /// `EG_FLAGS_OBJECT_STORE_NO_REUSE`: set by the shutdown walk, so an
    /// object a destructor makes is above `top` and the walk reaches it.
    no_reuse: bool,
}

impl Store {
    /// `zend_objects_store_init` (`Z/Zend/zend_objects_API.c:28`): the
    /// buckets allocated, `top` at one, no free list. A machine that
    /// cannot give the chunk leaves the table for the first `put`.
    pub(crate) fn new(mm: &mut Mm) -> Store {
        let mut store = Store {
            buckets: None,
            size: 0,
            top: 1,
            free_list_head: None,
            no_reuse: false,
        };
        let _ = store.lay_down(mm);
        store
    }

    fn lay_down(&mut self, mm: &mut Mm) -> Result<(), Fatal> {
        if self.buckets.is_some() {
            return Ok(());
        }
        let at = mm.alloc(INIT_BYTES)?.cast::<usize>();
        // SAFETY: the allocation is `INIT_SIZE` buckets and nothing else
        // holds it; php zeroes bucket zero and no other.
        unsafe { at.as_ptr().write(0) };
        self.buckets = Some(at);
        self.size = INIT_SIZE;
        Ok(())
    }

    fn bucket_ptr(&self, handle: u32) -> Option<*mut usize> {
        let buckets = self.buckets?;
        if handle >= self.size {
            return None;
        }
        // SAFETY: `handle` is below `size`, the buckets allocated.
        Some(unsafe { buckets.as_ptr().add(handle as usize) })
    }

    fn read(&self, handle: u32) -> Option<usize> {
        let at = self.bucket_ptr(handle)?;
        // SAFETY: the bucket is inside the allocation and below `top`
        // or being written, so it was written by `put`, `lay_down` or
        // the growth's copy.
        Some(unsafe { at.read() })
    }

    fn write(&self, handle: u32, word: usize) {
        if let Some(at) = self.bucket_ptr(handle) {
            // SAFETY: the bucket is inside the allocation.
            unsafe { at.write(word) };
        }
    }

    /// `EG(objects_store).object_buckets[handle]` when `IS_OBJ_VALID`.
    pub(crate) fn get(&self, handle: u32) -> Option<Obj> {
        if handle == 0 || handle >= self.top {
            return None;
        }
        let word = self.read(handle)?;
        if word & BUCKET_INVALID != 0 {
            return None;
        }
        NonNull::new(std::ptr::with_exposed_provenance_mut(word)).map(Obj)
    }

    /// `zend_objects_store_put` (`Z/Zend/zend_objects_API.c:142`): the
    /// head of the free list unless the shutdown walk forbids reuse,
    /// else `top`, and the table doubled by `erealloc` when `top` is at
    /// its size (`zend_objects_store_put_cold`).
    pub(crate) fn put(&mut self, mm: &mut Mm, o: Obj) -> Result<u32, Fatal> {
        self.lay_down(mm)?;
        let handle = match self.free_list_head {
            Some(handle) if !self.no_reuse => {
                let word = self.read(handle).unwrap_or(BUCKET_INVALID);
                let next = (word >> 1) as u32;
                self.free_list_head = if next == u32::MAX { None } else { Some(next) };
                handle
            }
            _ => {
                if self.top == self.size {
                    self.grow(mm)?;
                }
                let handle = self.top;
                self.top += 1;
                handle
            }
        };
        o.header().handle.set(handle);
        self.write(handle, o.address().as_ptr().expose_provenance());
        Ok(handle)
    }

    /// `erealloc(object_buckets, 2 * size * sizeof(zend_object *))`: a
    /// new array of twice the buckets, the old ones copied over and the
    /// old array given back. php's `erealloc` grows a large block in
    /// place when the pages after it are free, which #278 covers for a
    /// packed table and this shares.
    fn grow(&mut self, mm: &mut Mm) -> Result<(), Fatal> {
        let Some(old) = self.buckets else {
            return self.lay_down(mm);
        };
        let new_size = self.size * 2;
        let at = mm.alloc(new_size as usize * BUCKET)?.cast::<usize>();
        // SAFETY: the old array holds `size` buckets, the new holds twice
        // that, and the two do not overlap.
        unsafe {
            at.as_ptr()
                .copy_from_nonoverlapping(old.as_ptr(), self.size as usize);
        }
        // SAFETY: the old array came from this heap and every bucket in
        // it has been copied out.
        unsafe { mm.free(old.cast()) };
        self.buckets = Some(at);
        self.size = new_size;
        Ok(())
    }

    /// `SET_OBJ_INVALID`: the bucket no longer names a live object.
    fn invalidate(&self, handle: u32) {
        if let Some(word) = self.read(handle) {
            self.write(handle, word | BUCKET_INVALID);
        }
    }

    /// `ZEND_OBJECTS_STORE_ADD_TO_FREE_LIST`.
    fn add_to_free_list(&mut self, handle: u32) {
        let next = self.free_list_head.unwrap_or(u32::MAX) as usize;
        self.write(handle, (next << 1) | BUCKET_INVALID);
        self.free_list_head = Some(handle);
    }

    /// How many buckets name a live object.
    pub(crate) fn live(&self) -> usize {
        (1..self.top).filter(|h| self.get(*h).is_some()).count()
    }
}

impl Drop for Store {
    /// The sweep of what no free reached: every payload still on an
    /// object is dropped, so a payload's own resources go with the
    /// request; the objects' bytes are the heap's and go with its
    /// chunks. Declared before `mm` on the heap, so this runs first.
    fn drop(&mut self) {
        for handle in 1..self.top {
            if let Some(o) = self.get(handle) {
                o.drop_payload();
            }
        }
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Store({} live, top {})", self.live(), self.top)
    }
}

impl Heap {
    /// `zend_objects_new` (`Z/Zend/zend_objects.c:189`): an object of
    /// `class` with `slots` declared property slots, one owner, in the
    /// store under the id [`Obj::id`] reads. The
    /// engine computes `slots` as php's `zend_object_properties_size`
    /// does, the declared count plus one for a class that uses guards.
    pub fn object(&mut self, class: u32, slots: u32) -> Result<Obj, Fatal> {
        self.object_alloc(class, slots, None)
    }

    /// The same object carrying `payload` as its native slot (design
    /// §5.4): php's `zend_object_alloc(sizeof(custom), ce)`, the custom
    /// struct's bytes counted with the object's. `T` is `Send` because
    /// the request is; what it names of the heap is given back through
    /// [`HeapHooks::free_native`] before it is dropped.
    pub fn object_with<T: Any + Send>(
        &mut self,
        class: u32,
        slots: u32,
        payload: T,
    ) -> Result<Obj, Fatal> {
        let table = payload_of::<T>();
        let o = self.object_alloc(class, slots, Some(table))?;
        // SAFETY: the allocation was sized for the payload at this
        // address, which nothing has written.
        unsafe { o.payload_addr(table).cast::<T>().as_ptr().write(payload) };
        Ok(o)
    }

    fn object_alloc(
        &mut self,
        class: u32,
        slots: u32,
        payload: Option<&'static Payload>,
    ) -> Result<Obj, Fatal> {
        let size = Obj::allocation_size(slots, payload)?;
        let at = self.mm.alloc(size)?;
        // SAFETY: the allocation is exactly the size `init` lays out and
        // nothing else holds it.
        let o = unsafe { Obj::init(at, class, slots, payload) };
        if let Err(fatal) = self.store.put(&mut self.mm, o) {
            // SAFETY: the object was allocated just above and is in no
            // bucket, so nothing names it.
            unsafe { self.mm.free(at) };
            return Err(fatal);
        }
        Ok(o)
    }

    /// The object under `id`, while it is live: what the seam is handed
    /// and what `spl_object_id()`'s inverse, a weak reference, reads.
    #[must_use]
    pub fn stored(&self, id: ObjectId) -> Option<Obj> {
        self.store.get(id.0)
    }

    /// How many objects the store holds.
    #[must_use]
    pub fn objects(&self) -> usize {
        self.store.live()
    }

    /// `zend_std_get_properties`: the dynamic table, materialised on the
    /// first ask (`rebuild_object_properties`). php appends an
    /// `IS_INDIRECT` per declared slot there; those slots are read off
    /// the object here, and the table holds the dynamic properties
    /// alone. The table is the object's, one owner.
    pub fn object_properties(&mut self, o: Obj) -> Result<Arr, Fatal> {
        if let Some(table) = o.properties() {
            return Ok(table);
        }
        let table = self.array()?;
        o.header().properties.set(Some(table));
        Ok(table)
    }

    /// The object's table replaced, what `SEPARATE_ARRAY` over a shared
    /// one does for a write: the old name one owner fewer through the
    /// engine's release, never here.
    pub fn object_set_properties(&mut self, o: Obj, table: Arr) {
        o.header().properties.set(Some(table));
    }

    /// `zend_assign_to_variable` over a declared slot: `z` is the slot's
    /// from here, and what it held is released last, after the write,
    /// which is where a destructor is served.
    pub fn object_set_slot(hooks: &mut dyn HeapHooks, o: Obj, i: u32, z: Zval) {
        if i >= o.slots() {
            return;
        }
        let old = o.slot(i);
        o.write_slot(i, z);
        Heap::release(hooks, old);
    }

    /// The native payload as `T`, `None` for an object of another type
    /// or of none. Borrowed for as long as the heap is, which is what
    /// keeps a `Copy` name from handing out two.
    #[must_use]
    pub fn native<T: Any + Send>(&self, o: Obj) -> Option<&T> {
        let at = o.native_ptr::<T>()?;
        // SAFETY: the payload was written by `object_with` as a `T` and is
        // live while the heap is borrowed.
        Some(unsafe { at.as_ref() })
    }

    /// The native payload as `T`, mutably; the heap is held for as long
    /// as the borrow is, so no second name reaches it meanwhile.
    pub fn native_mut<T: Any + Send>(&mut self, o: Obj) -> Option<&mut T> {
        let mut at = o.native_ptr::<T>()?;
        // SAFETY: as `native`, and the heap is held mutably.
        Some(unsafe { at.as_mut() })
    }

    /// `zend_objects_clone_members` (`Z/Zend/zend_objects.c:197`) over a
    /// fresh object of the same class: every slot one more owner of what
    /// the old holds, and the dynamic table shared with one more owner,
    /// separated by the first write. An object with a native payload is
    /// its class's `clone_obj` to copy and is refused here, since the
    /// heap cannot copy a payload it only knows how to drop.
    pub fn object_clone(&mut self, o: Obj) -> Result<Option<Obj>, Fatal> {
        if o.has_native() {
            return Ok(None);
        }
        let new = self.object(o.class(), o.slots())?;
        for i in 0..o.slots() {
            let z = o.slot(i);
            self.addref(z);
            new.write_slot(i, z);
        }
        if let Some(table) = o.properties() {
            table.addref();
            new.header().properties.set(Some(table));
        }
        Ok(Some(new))
    }

    /// `zend_objects_store_del` (`Z/Zend/zend_objects_API.c:162`): the
    /// destructor served once with the count held at one, and, when
    /// nothing took an owner meanwhile, the bucket invalidated, the
    /// members and the payload freed once, the bytes given back and the
    /// id put on the free list. The heap is reached through `hooks`
    /// across each call up, since the destructor may grow the store.
    pub(crate) fn store_del(hooks: &mut dyn HeapHooks, o: Obj) {
        if !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED) {
            o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
            o.header().gc.set_refcount(1);
            hooks.destruct(o);
            o.delref();
        }
        if o.refcount() != 0 {
            return;
        }
        let handle = o.id().0;
        hooks.heap().store.invalidate(handle);
        if !o.has_flag(gc::OBJ_FREE_CALLED) {
            o.add_flag(gc::OBJ_FREE_CALLED);
            o.header().gc.set_refcount(1);
            Heap::object_free(hooks, o);
        }
        let heap = hooks.heap();
        // SAFETY: the object came from `object_alloc`, its bucket is
        // invalid and its members are freed, so nothing names it.
        unsafe { heap.mm.free(o.address()) };
        heap.store.add_to_free_list(handle);
    }

    /// `free_obj`: `zend_object_std_dtor` (`Z/Zend/zend_objects.c:77`),
    /// the dynamic table then the slots, and for an internal class the
    /// payload, what it names of the heap given back by the engine
    /// through [`HeapHooks::free_native`] and the rest by its drop.
    fn object_free(hooks: &mut dyn HeapHooks, o: Obj) {
        if let Some(table) = o.header().properties.take() {
            Heap::release(hooks, Zval::array(table));
        }
        for i in 0..o.slots() {
            let z = o.slot(i);
            o.write_slot(i, Zval::UNDEF);
            Heap::release(hooks, z);
        }
        if o.has_native() {
            hooks.free_native(o);
            o.drop_payload();
        }
    }

    /// `zend_objects_store_call_destructors`
    /// (`Z/Zend/zend_objects_API.c:43`), the shutdown walk: every live
    /// object's destructor not yet served, in id order, with reuse of a
    /// freed id off from here on so an object a destructor makes lands
    /// above `top` and the walk reaches it.
    pub fn call_destructors(hooks: &mut dyn HeapHooks) {
        hooks.heap().store.no_reuse = true;
        let mut handle = 1;
        while handle < hooks.heap().store.top {
            if let Some(o) = hooks.heap().store.get(handle)
                && !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED)
            {
                o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
                o.addref();
                hooks.destruct(o);
                o.delref();
            }
            handle += 1;
        }
    }

    /// `zend_objects_store_free_object_storage`
    /// (`Z/Zend/zend_objects_API.c:83`), the shutdown free: every live
    /// object's members and payload freed once, from the top down, each
    /// held at one more owner so nothing frees it twice; the bytes stay
    /// where they are, as php leaves them for the mass release.
    pub fn free_object_storage(hooks: &mut dyn HeapHooks) {
        let mut handle = hooks.heap().store.top;
        while handle > 1 {
            handle -= 1;
            if let Some(o) = hooks.heap().store.get(handle)
                && !o.has_flag(gc::OBJ_FREE_CALLED)
            {
                o.add_flag(gc::OBJ_FREE_CALLED);
                o.addref();
                Heap::object_free(hooks, o);
            }
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

    /// What the heap's tests pass where a request would serve a
    /// destructor: the heap itself, and a log of what would have been
    /// served, in the order it would have, which is how `zend_objects_store_del`'s
    /// ordering is held without a VM (design §5.10).
    struct Recorder {
        heap: Heap,
        log: Vec<(ObjectId, &'static str)>,
    }

    impl Recorder {
        fn new() -> Recorder {
            Recorder {
                heap: Heap::new(),
                log: Vec::new(),
            }
        }
    }

    impl HeapHooks for Recorder {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn destruct(&mut self, obj: Obj) {
            self.log.push((obj.id(), "destruct"));
        }
        fn free_native(&mut self, obj: Obj) {
            self.log.push((obj.id(), "free_native"));
        }
        fn gc_children(&mut self, _obj: Obj, _visit: &mut dyn FnMut(Zval)) {}
        fn free_resource(&mut self, _res: u32) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::OutOfMemory(bytes)
        }
    }

    /// The store's table is php's at request start, eight kilobytes of
    /// the request's memory, and an object is php's forty bytes plus
    /// sixteen a slot: `sizeof(zend_object)` less the slot the struct
    /// holds, then `sizeof(zval)` per declared property.
    ///
    /// **Every number here is php's own.**
    #[test]
    fn an_object_takes_the_bytes_php_s_takes() {
        let _alone = alone();
        let mut r = Recorder::new();
        let start = r.heap.memory_usage(false);
        assert_eq!(start, INIT_BYTES);
        assert_eq!(std::mem::size_of::<ObjHeader>(), 40);
        let plain = r.heap.object(7, 0);
        assert!(plain.is_ok(), "the heap would not allocate an object");
        let Ok(plain) = plain else { return };
        assert_eq!(r.heap.memory_usage(false) - start, 40);
        assert_eq!(plain.class(), 7);
        assert_eq!(plain.refcount(), 1);
        let three = r.heap.object(7, 3);
        assert!(three.is_ok(), "the heap would not allocate an object");
        let Ok(three) = three else { return };
        // Forty and forty-eight, the 88 of php's `zend_object_alloc` in
        // the 96-byte bin.
        assert_eq!(r.heap.memory_usage(false) - start, 40 + 96);
        assert_eq!(three.slots(), 3);
        assert!(three.slot(0).is_undef());
        assert!(
            three.slot(3).is_undef(),
            "a slot past the class's reads as UNDEF"
        );
        Heap::release(&mut r, Zval::object(plain));
        Heap::release(&mut r, Zval::object(three));
        assert_eq!(r.heap.memory_usage(false), start);
        assert_eq!(r.heap.objects(), 0);
    }

    /// Ids start at one, count up, and a freed id is the next one
    /// given, off the free list, which is what `spl_object_id()` reads
    /// after `unset`.
    #[test]
    fn a_freed_handle_is_the_next_one_given() {
        let _alone = alone();
        let mut recorder = Recorder::new();
        let ids: Vec<Option<Obj>> = (0..3).map(|_| recorder.heap.object(1, 0).ok()).collect();
        let [Some(first), Some(second), Some(third)] = ids[..] else {
            return;
        };
        assert_eq!(
            (first.id(), second.id(), third.id()),
            (ObjectId(1), ObjectId(2), ObjectId(3))
        );
        assert_eq!(
            recorder.heap.stored(ObjectId(2)).map(|o| o.is_same(second)),
            Some(true)
        );
        Heap::release(&mut recorder, Zval::object(second));
        assert!(recorder.heap.stored(ObjectId(2)).is_none());
        Heap::release(&mut recorder, Zval::object(first));
        // The list is last freed first: the first's id, then the
        // second's, then a fresh one past the third.
        let given: Vec<Option<ObjectId>> = (0..3)
            .map(|_| recorder.heap.object(1, 0).ok().map(|o| o.id()))
            .collect();
        assert_eq!(
            given,
            vec![Some(ObjectId(1)), Some(ObjectId(2)), Some(ObjectId(4))]
        );
        assert_eq!(recorder.heap.objects(), 4);
    }

    /// The table doubles where php's doubles, on the object past the
    /// 1024th, and the objects made before it keep their buckets.
    #[test]
    fn the_store_doubles_where_php_s_doubles() {
        let _alone = alone();
        let mut r = Recorder::new();
        let mut held = Vec::new();
        for _ in 1..INIT_SIZE {
            let o = r.heap.object(1, 0);
            assert!(o.is_ok(), "the heap would not allocate an object");
            if let Ok(o) = o {
                held.push(o);
            }
        }
        let start = r.heap.memory_usage(false);
        let grown = r.heap.object(1, 0);
        assert!(grown.is_ok(), "the heap would not grow the store");
        let Ok(grown) = grown else { return };
        assert_eq!(grown.id(), ObjectId(INIT_SIZE));
        // Sixteen kilobytes for the new table, the old eight back, and
        // the object itself.
        assert_eq!(r.heap.memory_usage(false) - start, 8192 + 40);
        assert_eq!(
            r.heap.stored(ObjectId(1)).map(|o| o.is_same(held[0])),
            Some(true)
        );
        assert_eq!(r.heap.objects(), INIT_SIZE as usize);
    }

    /// `zend_objects_store_del`: the destructor is served once, with the
    /// count held at one, before the members are freed; a release that
    /// finds the destructor already served frees without it; and an
    /// object a destructor keeps alive is not freed until its new owner
    /// lets go, its destructor never served again.
    #[test]
    fn a_release_to_zero_serves_the_destructor_once_and_frees_after_it() {
        let _alone = alone();
        let mut r = Recorder::new();
        let Ok(o) = r.heap.object(1, 1) else { return };
        let Ok(s) = r.heap.string(b"held by the slot") else {
            return;
        };
        o.write_slot(0, Zval::string(s));
        let z = Zval::object(o);
        r.heap.addref(z);
        assert_eq!(o.refcount(), 2);
        Heap::release(&mut r, z);
        assert_eq!(o.refcount(), 1);
        assert!(
            r.log.is_empty(),
            "a release that leaves an owner serves nothing"
        );
        Heap::release(&mut r, z);
        assert_eq!(r.log, vec![(ObjectId(1), "destruct")]);
        assert_eq!(r.heap.objects(), 0);
        assert_eq!(
            r.heap.memory_usage(false),
            INIT_BYTES,
            "the slot's string went with it"
        );
    }

    /// A destructor that takes an owner resurrects the object: the free
    /// half waits for that owner, and the second release to zero does
    /// not serve the destructor again.
    /// The hooks whose destructor takes an owner of what it is served.
    struct Keeper {
        heap: Heap,
        served: u32,
    }

    impl HeapHooks for Keeper {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn destruct(&mut self, obj: Obj) {
            self.served += 1;
            obj.addref();
        }
        fn free_native(&mut self, _obj: Obj) {}
        fn gc_children(&mut self, _obj: Obj, _visit: &mut dyn FnMut(Zval)) {}
        fn free_resource(&mut self, _res: u32) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::OutOfMemory(bytes)
        }
    }

    #[test]
    fn a_destructor_that_takes_an_owner_keeps_the_object() {
        let _alone = alone();
        let mut k = Keeper {
            heap: Heap::new(),
            served: 0,
        };
        let Ok(o) = k.heap.object(1, 0) else { return };
        Heap::release(&mut k, Zval::object(o));
        assert_eq!(k.served, 1);
        assert_eq!(k.heap.objects(), 1, "the destructor's owner keeps it");
        assert_eq!(o.refcount(), 1);
        Heap::release(&mut k, Zval::object(o));
        assert_eq!(k.served, 1, "a destructor is served once");
        assert_eq!(k.heap.objects(), 0);
    }

    /// An array of objects destroyed serves each destructor in element
    /// order, with the array still standing around it, as
    /// `zend_array_destroy` runs `i_zval_ptr_dtor` over each element
    /// before it frees the data.
    #[test]
    fn an_array_of_objects_serves_the_destructors_in_order() {
        let _alone = alone();
        let mut r = Recorder::new();
        let Ok(a) = r.heap.array() else { return };
        for _ in 0..3 {
            let Ok(o) = r.heap.object(1, 0) else { return };
            assert_eq!(r.heap.array_append(a, Zval::object(o)), Ok(true));
        }
        assert_eq!(r.heap.objects(), 3);
        Heap::release(&mut r, Zval::array(a));
        assert_eq!(
            r.log,
            vec![
                (ObjectId(1), "destruct"),
                (ObjectId(2), "destruct"),
                (ObjectId(3), "destruct")
            ]
        );
        assert_eq!(r.heap.objects(), 0);
        assert_eq!(r.heap.memory_usage(false), INIT_BYTES);
    }

    /// A native payload sits after the slots in the same allocation, is
    /// read back as its type and as no other, and its release is
    /// `free_native` for what it names of the heap and then its own drop.
    /// A payload that counts its drops.
    struct Counted(std::sync::Arc<std::sync::atomic::AtomicU32>, u64);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn a_native_payload_is_one_typed_slot_dropped_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        let _alone = alone();
        let dropped = Arc::new(AtomicU32::new(0));
        let mut r = Recorder::new();
        let start = r.heap.memory_usage(false);
        let o = r.heap.object_with(3, 2, Counted(Arc::clone(&dropped), 42));
        assert!(o.is_ok(), "the heap would not allocate the payload");
        let Ok(o) = o else { return };
        // Forty, two slots and sixteen of payload: the 88 of a 96 bin.
        assert_eq!(r.heap.memory_usage(false) - start, 96);
        assert_eq!(r.heap.native::<Counted>(o).map(|c| c.1), Some(42));
        assert!(
            r.heap.native::<u64>(o).is_none(),
            "another type reads nothing"
        );
        if let Some(c) = r.heap.native_mut::<Counted>(o) {
            c.1 = 7;
        }
        assert_eq!(r.heap.native::<Counted>(o).map(|c| c.1), Some(7));
        assert!(
            matches!(r.heap.object_clone(o), Ok(None)),
            "a payload is its class's to clone"
        );
        Heap::release(&mut r, Zval::object(o));
        assert_eq!(
            r.log,
            vec![(ObjectId(1), "destruct"), (ObjectId(1), "free_native")]
        );
        assert_eq!(dropped.load(Relaxed), 1);
        assert_eq!(r.heap.memory_usage(false), start);
        // What no release reached is dropped with the heap, once.
        let Ok(kept) = r.heap.object_with(3, 0, Counted(Arc::clone(&dropped), 1)) else {
            return;
        };
        assert_eq!(
            kept.id(),
            ObjectId(1),
            "the handle came back off the free list"
        );
        drop(r);
        assert_eq!(dropped.load(Relaxed), 2);
    }

    /// A clone is a fresh entry of the same class whose slots name what
    /// the old ones name with one more owner each, and whose dynamic
    /// table is the same table shared until a write separates it.
    #[test]
    fn a_clone_shares_what_the_old_holds_with_one_more_owner() {
        let _alone = alone();
        let mut r = Recorder::new();
        let Ok(o) = r.heap.object(2, 1) else { return };
        let Ok(s) = r.heap.string(b"in the slot") else {
            return;
        };
        o.write_slot(0, Zval::string(s));
        let Ok(table) = r.heap.object_properties(o) else {
            return;
        };
        let Ok(name) = r.heap.string(b"dyn") else {
            return;
        };
        assert!(Heap::array_update(&mut r, table, crate::Key::Str(name), Zval::long(1)).is_ok());
        // The bucket took its own owner of the key.
        r.heap.release_string(name);
        let Ok(Some(new)) = r.heap.object_clone(o) else {
            return;
        };
        assert_eq!(new.id(), ObjectId(2));
        assert_eq!(new.class(), 2);
        assert_eq!(s.refcount(), 2);
        assert_eq!(table.refcount(), 2);
        assert_eq!(new.properties().map(|t| t.is_same(table)), Some(true));
        Heap::release(&mut r, Zval::object(o));
        assert_eq!(s.refcount(), 1);
        assert_eq!(table.refcount(), 1);
        Heap::release(&mut r, Zval::object(new));
        assert_eq!(r.heap.memory_usage(false), INIT_BYTES);
    }

    /// The shutdown walks: destructors in id order for what is still
    /// live, then the members freed from the top down, each once, and
    /// the bytes left for the mass release.
    #[test]
    fn the_shutdown_walks_serve_each_once_in_php_s_orders() {
        let _alone = alone();
        let mut r = Recorder::new();
        let ids: Vec<Option<Obj>> = (0..3).map(|_| r.heap.object(1, 0).ok()).collect();
        let [Some(_a), Some(b), Some(_c)] = ids[..] else {
            return;
        };
        Heap::release(&mut r, Zval::object(b));
        r.log.clear();
        Heap::call_destructors(&mut r);
        assert_eq!(
            r.log,
            vec![(ObjectId(1), "destruct"), (ObjectId(3), "destruct")]
        );
        // With reuse off, an object made now is above `top`, not in
        // `b`'s bucket.
        let Ok(late) = r.heap.object(1, 0) else {
            return;
        };
        assert_eq!(late.id(), ObjectId(4));
        r.log.clear();
        Heap::free_object_storage(&mut r);
        assert!(r.log.is_empty(), "no payload, so nothing to hand up");
        assert_eq!(r.heap.objects(), 3, "the bytes stay for the mass release");
        Heap::free_object_storage(&mut r);
        Heap::call_destructors(&mut r);
        assert_eq!(
            r.log,
            vec![(ObjectId(4), "destruct")],
            "each walk serves each object once"
        );
    }
}
