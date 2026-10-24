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

use std::any::Any;
use std::cell::Cell;
use std::ptr::NonNull;

use crate::array::{Arr, DeclaredRows};
use crate::gc::{self, Counted, Refcounted};
use crate::hooks::{Fatal, HeapHooks};
use crate::mm::Mm;
use crate::payload::{Payload, payload_of};
use crate::{Heap, PropBit, Zval, ZvalRef, ZvalType};

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
const BUCKET: usize = size_of::<Bucket>();
/// `OBJ_BUCKET_INVALID`: the low bit of a bucket that holds no object.
const BUCKET_INVALID: usize = 1;
/// One bucket of the store, php's `zend_object *`: an object's pointer,
/// or a word tagged [`BUCKET_INVALID`] in the same place ([`Store`]).
type Bucket = *mut ObjHeader;
/// The store's table at request start, what `memory_get_usage()` reads
/// before the request serves a line: 1024 buckets of a pointer each.
pub(crate) const INIT_BYTES: usize = INIT_SIZE as usize * BUCKET;

/// `zend_object`: forty bytes, php's own order, out of the heap, the
/// property slots after it.
#[repr(C)]
pub(crate) struct ObjHeader {
    /// `zend_refcounted_h gc`, whose flags carry
    /// `IS_OBJ_DESTRUCTOR_CALLED` and `IS_OBJ_FREE_CALLED`.
    gc: Refcounted,
    /// php's `handle`: the store's index, `spl_object_id()`.
    handle: Cell<u32>,
    /// `extra_flags`, `OBJ_EXTRA_FLAGS()`, where php keeps
    /// `IS_OBJ_LAZY_UNINITIALIZED` and `IS_OBJ_LAZY_PROXY`, read and
    /// written together as one [`Laziness`].
    extra_flags: Cell<u32>,
    /// php's `ce`: the class as the engine numbers it, which the heap
    /// reads nothing of, and the slots the class laid down, which php
    /// reads off `ce->default_properties_count`.
    class: Cell<u32>,
    slots: u32,
    /// php's `handlers`: what the heap needs of the table, the payload's
    /// type; none for a class with no native payload.
    payload: Cell<Option<&'static Payload>>,
    /// `properties`: the dynamic table, none until something
    /// materialises it (`rebuild_object_properties`).
    properties: Cell<Option<Arr>>,
}

// The header at the block's start, where `Zval::counted` reads it for
// every counted kind without asking which.
const _: () = assert!(std::mem::offset_of!(ObjHeader, gc) == 0);

/// What an object's two lazy bits say of it (`zend_lazy_objects.h`):
/// the strategy of an object nothing has initialised, or the proxy that
/// forwards to the instance its factory returned. What goes with each,
/// the initializer and the instance, is the engine's, kept aside by the
/// object's id as php keeps `zend_lazy_object_info` in
/// `EG(lazy_objects_store)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Laziness {
    /// Neither bit: an object that was never lazy, has been initialised
    /// in place, or is being initialised now.
    Eager,
    /// `IS_OBJ_LAZY_UNINITIALIZED`: a ghost, initialised in place.
    Ghost,
    /// `IS_OBJ_LAZY_UNINITIALIZED | IS_OBJ_LAZY_PROXY`: a proxy whose
    /// factory has not been served.
    Proxy,
    /// `IS_OBJ_LAZY_PROXY` alone: a proxy whose factory was served, every
    /// property access forwarded to the instance it returned.
    Forwarding,
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
    pub(crate) const fn payload(self) -> crate::zval::Word {
        crate::zval::Word::pointer(self.0)
    }

    /// The object a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Obj::payload`] for an object still live.
    pub(crate) unsafe fn from_payload(payload: crate::zval::Word) -> Option<Obj> {
        // SAFETY: the word holds the pointer `payload` stored, by the
        // contract.
        unsafe { payload.as_pointer::<ObjHeader>() }.map(Obj)
    }

    /// `zend_object_alloc`'s arithmetic: the header, a zval per slot and
    /// `custom`, the bytes php's struct lays beside the `zend_object`
    /// (its own fields, and the guard slot of a class that uses guards),
    /// which the payload sits in at its own alignment, the allocator's
    /// eight-byte alignment honouring up to eight. A payload larger than
    /// `custom` takes what it needs.
    fn allocation_size(
        slots: u32,
        payload: Option<&Payload>,
        custom: usize,
    ) -> Result<usize, Fatal> {
        let members = HEADER + SLOT * slots as usize;
        let Some(payload) = payload else {
            return Ok(members + custom);
        };
        if payload.layout.align() > 8 {
            return Err(Fatal::error(format!(
                "InternalError: a native payload aligned to {} cannot sit on the request heap",
                payload.layout.align()
            )));
        }
        let end = members.next_multiple_of(payload.layout.align()) + payload.layout.size();
        Ok(end.max(members + custom))
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
                extra_flags: Cell::new(0),
                class: Cell::new(class),
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
        self.header().class.get()
    }

    /// php's `zobj->ce = ce` where the object keeps its layout: `class`
    /// taken where it lays down `slots`, the slots the object has, and
    /// the object has no native payload; `false`, and nothing changed,
    /// otherwise.
    pub fn set_class(&self, class: u32, slots: u32) -> bool {
        if slots != self.slots() || self.has_native() {
            return false;
        }
        self.header().class.set(class);
        true
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(&self) {
        self.gc().addref();
    }

    /// `GC_DELREF`, and what is left.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn delref(self) -> u32 {
        self.gc().delref()
    }

    /// Whether two names name one object, php's pointer equality, which
    /// `===` is for objects.
    #[must_use]
    pub fn is_same(&self, other: Obj) -> bool {
        self.0 == other.0
    }

    /// `IS_OBJ_LAZY_UNINITIALIZED` and `IS_OBJ_LAZY_PROXY` read as the
    /// one state they encode (design §5.4). The bits are
    /// `extra_flags`'s, where php keeps them
    /// (`Z/Zend/zend_types.h:867-870`), since the header word's top bits
    /// are the collector's colour.
    #[must_use]
    pub fn laziness(&self) -> Laziness {
        let flags = self.header().extra_flags.get();
        match (
            flags & gc::OBJ_LAZY_UNINITIALIZED != 0,
            flags & gc::OBJ_LAZY_PROXY != 0,
        ) {
            (false, false) => Laziness::Eager,
            (true, false) => Laziness::Ghost,
            (true, true) => Laziness::Proxy,
            (false, true) => Laziness::Forwarding,
        }
    }

    /// How many frames and calls name the object as a receiver they hold
    /// no owner of, php's `EX(This)` without `ZEND_CALL_RELEASE_THIS`,
    /// kept in the bits of `extra_flags` php leaves unused. php frees
    /// such an object under the running frame when its last owner lets
    /// it go (`zend_objects_store_del`); here its destructor, its
    /// members and its id go at that moment and its bytes stand while a
    /// pin does ([`Heap::unpin`]), so no frame names freed memory.
    #[must_use]
    pub fn pins(&self) -> u32 {
        self.header().extra_flags.get() & gc::OBJ_PINS
    }

    /// One more frame names the object with no owner of it. The count
    /// cannot outgrow its thirty bits: every pin is a frame or a call
    /// on a stack the call-stack limit bounds long before.
    pub fn pin(&self) {
        let word = &self.header().extra_flags;
        let pins = (word.get() & gc::OBJ_PINS)
            .saturating_add(1)
            .min(gc::OBJ_PINS);
        word.set((word.get() & !gc::OBJ_PINS) | pins);
    }

    /// `OBJ_FLAGS(obj) & IS_OBJ_DESTRUCTOR_CALLED`: the destructor was
    /// served, or the object is marked as though it had been.
    #[must_use]
    pub fn destructor_called(&self) -> bool {
        self.has_flag(gc::OBJ_DESTRUCTOR_CALLED)
    }

    /// `GC_ADD_FLAGS` or `GC_DEL_FLAGS` of `IS_OBJ_DESTRUCTOR_CALLED`:
    /// what `zend_object_make_lazy` writes around the destructor it
    /// serves when it resets an object, so the object's own destructor
    /// is served again once it is initialised.
    pub fn set_destructor_called(&self, called: bool) {
        if called {
            self.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
        } else {
            self.header().gc.del_flags(gc::OBJ_DESTRUCTOR_CALLED);
        }
    }

    /// `OBJ_EXTRA_FLAGS(obj)`'s two lazy bits written as `state`
    /// encodes them, the other bits of the word left as they stand.
    pub fn set_laziness(&self, state: Laziness) {
        let bits = match state {
            Laziness::Eager => 0,
            Laziness::Ghost => gc::OBJ_LAZY_UNINITIALIZED,
            Laziness::Proxy => gc::OBJ_LAZY_UNINITIALIZED | gc::OBJ_LAZY_PROXY,
            Laziness::Forwarding => gc::OBJ_LAZY_PROXY,
        };
        let word = &self.header().extra_flags;
        let rest = word.get() & !(gc::OBJ_LAZY_UNINITIALIZED | gc::OBJ_LAZY_PROXY);
        word.set(rest | bits);
    }

    /// `Z_IS_RECURSIVE_P`: a walk that guards itself with
    /// [`Obj::protect`] is inside this object already, which is how
    /// `zend_std_compare_objects` sees an object whose property walk
    /// leads back to it.
    #[must_use]
    pub fn is_recursive(&self) -> bool {
        self.has_flag(gc::PROTECTED)
    }

    /// `IS_OBJ_WEAKLY_REFERENCED`: whether a weak reference or a weak
    /// map's key names the object, so that its free is told to the
    /// engine through [`HeapHooks::weakrefs_notify`].
    #[must_use]
    pub fn is_weakly_referenced(&self) -> bool {
        self.has_flag(gc::OBJ_WEAKLY_REFERENCED)
    }

    /// `GC_ADD_FLAGS(object, IS_OBJ_WEAKLY_REFERENCED)`, or
    /// `GC_DEL_FLAGS` where `weak` is false: what `zend_weakref_register`
    /// and `zend_weakref_unregister` write.
    pub fn set_weakly_referenced(&self, weak: bool) {
        if weak {
            self.add_flag(gc::OBJ_WEAKLY_REFERENCED);
        } else {
            self.header().gc.del_flags(gc::OBJ_WEAKLY_REFERENCED);
        }
    }

    /// `GC_ADD_FLAGS(object, GC_NOT_COLLECTABLE)`: the object can be part
    /// of no cycle, so a release that leaves it owners never offers it to
    /// the collector (`GC_MAY_LEAK`). php marks an enum case
    /// (`zend_enum_new`, `Z/Zend/zend_enum.c:42`) and a fake closure with
    /// no receiver (`zend_create_fake_closure`,
    /// `Z/Zend/zend_closures.c:870`), and nothing else of an object.
    pub fn set_uncollectable(&self) {
        self.add_flag(gc::NOT_COLLECTABLE);
    }

    /// `Z_PROTECT_RECURSION_P`: the walk is inside.
    pub fn protect(&self) {
        self.add_flag(gc::PROTECTED);
    }

    /// `Z_UNPROTECT_RECURSION_P`: the walk is out.
    pub fn unprotect(&self) {
        self.header().gc.del_flags(gc::PROTECTED);
    }

    /// The dynamic table, once something has materialised it.
    #[must_use]
    pub fn properties(&self) -> Option<Arr> {
        self.header().properties.get()
    }

    /// What the object's table holds of its declared properties, the
    /// slots standing for them while it has no table.
    #[must_use]
    pub fn declared_rows(&self) -> DeclaredRows {
        self.properties()
            .map_or(DeclaredRows::Slots, |table| table.declared_rows())
    }

    /// Whether the object carries a native payload.
    #[must_use]
    pub fn has_native(&self) -> bool {
        self.header().payload.get().is_some()
    }

    pub(crate) fn has_flag(self, flag: u32) -> bool {
        self.header().gc.flags() & flag != 0
    }

    pub(crate) fn add_flag(self, flag: u32) {
        self.header().gc.add_flags(flag);
    }

    /// `zend_refcounted_h gc`: the header the cycle collector colours.
    pub(crate) fn gc(self) -> Counted {
        // SAFETY: the block starts with its header, and stands while
        // its owner holds the handle, as `header` reads it.
        unsafe { Counted::new(self.address()) }
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

    /// `IS_PROP_UNINIT` over declared slot `i`: undefined, and nothing has
    /// written it since the object was made, so a read is the
    /// uninitialised-property error and a write, `isset` and `unset` pass
    /// the magic methods by, where a slot an `unset` emptied reaches them.
    #[must_use]
    pub fn slot_uninit(&self, i: u32) -> bool {
        let z = self.slot(i);
        z.is_undef() && z.prop_bit(PropBit::Uninit)
    }

    /// Whether declared slot `i` carries `flag`; a slot the class did not
    /// lay down carries none.
    #[must_use]
    pub fn slot_bit(&self, i: u32, flag: PropBit) -> bool {
        self.slot(i).prop_bit(flag)
    }

    /// Set or clear `flag` on declared slot `i`, the value it holds
    /// untouched; nothing for a slot the class did not lay down.
    pub fn set_slot_bit(&self, i: u32, flag: PropBit, on: bool) {
        if let Some(slot) = self.slot_ref(i) {
            slot.set_prop_bit(flag, on);
        }
    }

    /// `OBJ_PROP_NUM` in place: the declared slot as a name a handler
    /// keeps to the next opline, what `FETCH_OBJ_W` leaves in its `VAR`
    /// (`zend_std_get_property_ptr_ptr`); none for a slot the class did
    /// not lay down.
    #[must_use]
    pub fn slot_ref(&self, i: u32) -> Option<ZvalRef> {
        if i >= self.slots() {
            return None;
        }
        NonNull::new(self.slot_ptr(i)).map(ZvalRef::new)
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
        if !payload.is::<T>() {
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
/// never given, so an id is never false. The word is a pointer, as
/// php's is (#2529): a live bucket is the object's pointer with its
/// provenance, an invalidated one the same pointer tagged in place
/// (`map_addr`), and a free-list entry an address with no provenance
/// at all, so nothing is exposed and nothing is read back from an
/// integer. The growth's copy is the heap's untyped one, which carries
/// provenance with the bytes.
pub(crate) struct Store {
    buckets: Option<NonNull<Bucket>>,
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
        store.lay_down(mm).ok();
        store
    }

    fn lay_down(&mut self, mm: &mut Mm) -> Result<(), Fatal> {
        if self.buckets.is_some() {
            return Ok(());
        }
        let at = mm.alloc(INIT_BYTES)?.cast::<Bucket>();
        // SAFETY: the allocation is `INIT_SIZE` buckets and nothing else
        // holds it; php zeroes bucket zero and no other.
        unsafe { at.as_ptr().write(std::ptr::null_mut()) };
        self.buckets = Some(at);
        self.size = INIT_SIZE;
        Ok(())
    }

    fn bucket_ptr(&self, handle: u32) -> Option<*mut Bucket> {
        let buckets = self.buckets?;
        if handle >= self.size {
            return None;
        }
        // SAFETY: `handle` is below `size`, the buckets allocated.
        Some(unsafe { buckets.as_ptr().add(handle as usize) })
    }

    fn read(&self, handle: u32) -> Option<Bucket> {
        let at = self.bucket_ptr(handle)?;
        // SAFETY: the bucket is inside the allocation and below `top`
        // or being written, so it was written by `put`, `lay_down` or
        // the growth's copy.
        Some(unsafe { at.read() })
    }

    fn write(&self, handle: u32, word: Bucket) {
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
        if word.addr() & BUCKET_INVALID != 0 {
            return None;
        }
        NonNull::new(word).map(Obj)
    }

    /// `zend_objects_store_put` (`Z/Zend/zend_objects_API.c:142`): the
    /// head of the free list unless the shutdown walk forbids reuse,
    /// else `top`, and the table doubled by `erealloc` when `top` is at
    /// its size (`zend_objects_store_put_cold`).
    pub(crate) fn put(&mut self, mm: &mut Mm, o: Obj) -> Result<u32, Fatal> {
        self.lay_down(mm)?;
        let handle = match self.free_list_head {
            Some(handle) if !self.no_reuse => {
                let word = self.read(handle).map_or(BUCKET_INVALID, Bucket::addr);
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
        self.write(handle, o.0.as_ptr());
        Ok(handle)
    }

    /// `erealloc(object_buckets, 2 * size * sizeof(zend_object *))`:
    /// twice the buckets through the heap's `realloc`, so the table grows
    /// where it stands when the pages after it are free, as php's does,
    /// and moves otherwise.
    fn grow(&mut self, mm: &mut Mm) -> Result<(), Fatal> {
        let Some(old) = self.buckets else {
            return self.lay_down(mm);
        };
        let new_size = self.size * 2;
        // SAFETY: the old array came from this heap for `size` buckets,
        // every one of them written, and `buckets` is its only name,
        // replaced below.
        let at = unsafe {
            mm.realloc(
                old.cast(),
                self.size as usize * BUCKET,
                new_size as usize * BUCKET,
            )
        }?;
        self.buckets = Some(at.cast::<Bucket>());
        self.size = new_size;
        Ok(())
    }

    /// `SET_OBJ_INVALID`: the bucket no longer names a live object.
    pub(crate) fn invalidate(&self, handle: u32) {
        if let Some(word) = self.read(handle) {
            self.write(handle, word.map_addr(|a| a | BUCKET_INVALID));
        }
    }

    /// `ZEND_OBJECTS_STORE_ADD_TO_FREE_LIST`.
    pub(crate) fn add_to_free_list(&mut self, handle: u32) {
        let next = self.free_list_head.unwrap_or(u32::MAX) as usize;
        self.write(
            handle,
            std::ptr::without_provenance_mut((next << 1) | BUCKET_INVALID),
        );
        self.free_list_head = Some(handle);
    }

    /// How many buckets name a live object.
    pub(crate) fn live(&self) -> usize {
        (1..self.top).filter(|h| self.get(*h).is_some()).count()
    }
}

impl Store {
    /// `zend_objects_store_destroy` (`Z/Zend/zend_objects_API.c:37`): the
    /// buckets given back to `mm`, a debug build's teardown before the
    /// request leak sensor. An object still standing is the sensor's to
    /// name, as php leaves it for `zend_mm_check_leaks`; its payload is
    /// dropped here, as the sweep would drop it, since no bucket names
    /// it after this.
    #[cfg(debug_assertions)]
    pub(crate) fn destroy(&mut self, mm: &mut Mm) {
        for handle in 1..self.top {
            if let Some(o) = self.get(handle) {
                o.drop_payload();
            }
        }
        if let Some(buckets) = self.buckets.take() {
            // SAFETY: the buckets came from this heap's `mm` and
            // `buckets` was their only name.
            unsafe { mm.free(buckets.cast()) };
        }
        self.size = 0;
        self.top = 1;
        self.free_list_head = None;
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
    /// store under the id [`Obj::id`] reads, `custom` bytes beside the
    /// slots counted with it: the guard slot `zend_object_properties_size`
    /// adds for a class that uses guards, and the fields of an internal
    /// class's struct, which the engine reads off the class.
    pub fn object(&mut self, class: u32, slots: u32, custom: usize) -> Result<Obj, Fatal> {
        self.object_alloc(class, slots, None, custom)
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
        custom: usize,
        payload: T,
    ) -> Result<Obj, Fatal> {
        let table = payload_of::<T>();
        let o = self.object_alloc(class, slots, Some(table), custom)?;
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
        custom: usize,
    ) -> Result<Obj, Fatal> {
        let size = Obj::allocation_size(slots, payload, custom)?;
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
    /// alone until a separation copies them in
    /// ([`Heap::object_properties_copy`]). It is laid down at php's
    /// size, `zend_new_array(default_properties_count)` and, where the
    /// class declares a slot, `zend_hash_real_init_mixed`, so
    /// `memory_get_usage()` counts the rows php's holds for the slots. A
    /// declared slot empty as the table is built marks it
    /// `HASH_FLAG_HAS_EMPTY_IND`, as php's rebuild does. The table is
    /// the object's, one owner.
    ///
    /// ```php
    /// class Q { public $a = 1; } class N { public $a, $b, $c, $d, $e, $f, $g, $h, $i; } class E {}
    /// foreach ([new Q, new N, new E] as $o) { $m = memory_get_usage(); foreach ($o as $v) {} echo memory_get_usage() - $m, "\n"; }
    /// ```
    /// ```expect
    /// 376
    /// 696
    /// 56
    /// ```
    pub fn object_properties(&mut self, o: Obj) -> Result<Arr, Fatal> {
        if let Some(table) = o.properties() {
            return Ok(table);
        }
        let table = match o.slots() {
            0 => self.array()?,
            slots => self.array_mixed(slots)?,
        };
        if (0..o.slots()).any(|i| o.slot(i).is_undef()) {
            table.set_has_empty_ind(true);
        }
        o.header().properties.set(Some(table));
        Ok(table)
    }

    /// `zend_hash_extend(Z_OBJPROP_P(o), count + extra, ...)`: the
    /// object's table, laid down as [`Heap::object_properties`] lays
    /// it, made room in at once for `extra` rows past the ones php's
    /// holds, an `IS_INDIRECT` per declared slot and the dynamic rows,
    /// what php's unserializer does before it reads an object's
    /// properties (`Z/ext/standard/var_unserializer.re:824-829`).
    pub fn object_properties_extend(&mut self, o: Obj, extra: u32) -> Result<Arr, Fatal> {
        let table = self.object_properties(o)?;
        let held = match table.declared_rows() {
            DeclaredRows::Slots => o.slots(),
            DeclaredRows::Copied => 0,
        };
        let rows = u32::try_from(table.len()).unwrap_or(u32::MAX);
        let size = held.saturating_add(rows).saturating_add(extra);
        self.array_extend(table, size)?;
        Ok(table)
    }

    /// The object's table replaced, what `SEPARATE_ARRAY` over a shared
    /// one does for a write: the old name one owner fewer through the
    /// engine's release, never here.
    pub fn object_set_properties(&mut self, o: Obj, table: Arr) {
        o.header().properties.set(Some(table));
    }

    /// `obj->properties = NULL`: the object's table taken off it, its
    /// owner the caller's to give up, what `zend_lazy_object_init_proxy`
    /// and the rollback of a ghost's initializer do to a table the
    /// object must no longer read.
    pub fn object_take_properties(&mut self, o: Obj) -> Option<Arr> {
        o.header().properties.take()
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
    /// the old holds, and a copy of the dynamic table of its own. An
    /// object with a native payload is
    /// its class's `clone_obj` to copy through
    /// [`Heap::object_clone_with`] and is refused here, since the heap
    /// cannot copy a payload it only knows how to drop.
    pub fn object_clone(&mut self, o: Obj, custom: usize) -> Result<Option<Obj>, Fatal> {
        if o.has_native() {
            return Ok(None);
        }
        let new = self.object(o.class(), o.slots(), custom)?;
        self.clone_members(new, o)?;
        Ok(Some(new))
    }

    /// `zend_objects_clone_members` (`Z/Zend/zend_objects.c:197`) onto
    /// `new`, an object of the old one's class that a class's
    /// `clone_obj` made itself before it copies the members
    /// (`spl_object_storage_clone`), with no dynamic table yet: each slot
    /// one more owner of the old object's value and the dynamic table
    /// copied, as [`Heap::object_clone`] copies them, and what `new`'s
    /// slots held handed back, the caller's to give up
    /// (`i_zval_ptr_dtor(dst)`).
    pub fn object_clone_members_into(&mut self, new: Obj, old: Obj) -> Result<Vec<Zval>, Fatal> {
        let displaced = (0..new.slots()).map(|i| new.slot(i)).collect();
        self.clone_members(new, old)?;
        Ok(displaced)
    }

    /// A class's `clone_obj` over an object with a native payload
    /// (design §5.4), `zend_closure_clone` and its kin: the members
    /// copied as [`Heap::object_clone`] copies them, beside `payload`,
    /// the copy the class made of the old object's, in the new object's
    /// slot. What the copy names of the heap is the caller's to have
    /// taken an owner of, as `zend_create_closure` takes one of `$this`.
    pub fn object_clone_with<T: Any + Send>(
        &mut self,
        o: Obj,
        custom: usize,
        payload: T,
    ) -> Result<Obj, Fatal> {
        let new = self.object_with(o.class(), o.slots(), custom, payload)?;
        self.clone_members(new, o)?;
        Ok(new)
    }

    /// The loop of `zend_objects_clone_members`: each declared slot one
    /// more owner of the old object's value as `zval_add_ref` takes it,
    /// a reference nothing else owns copied as the value it holds, and a
    /// table of the new object's own beside the old one's wherever
    /// php's holds a row (`zend_hash_num_elements(old_object->properties)`,
    /// which counts a row per declared slot where the slots stand for
    /// them): its rows copied the same way, `HASH_FLAG_HAS_EMPTY_IND`
    /// carried, and what the old table holds of the declared properties
    /// with it, since php points each `IS_INDIRECT` at the new object's
    /// slot and copies a plain value as it is.
    fn clone_members(&mut self, new: Obj, old: Obj) -> Result<(), Fatal> {
        for i in 0..old.slots() {
            let z = self.dup_value(old.slot(i), None);
            new.write_slot(i, z);
        }
        let Some(table) = old.properties() else {
            return Ok(());
        };
        let rows = table.declared_rows();
        if table.is_empty() && (rows == DeclaredRows::Copied || old.slots() == 0) {
            return Ok(());
        }
        let copy = self.array_sized(u32::try_from(table.len()).unwrap_or(u32::MAX))?;
        new.header().properties.set(Some(copy));
        for (key, z) in &table {
            let z = self.dup_value(z, None);
            Heap::array_add_new(self, copy, key, z)?;
        }
        copy.set_has_empty_ind(table.has_empty_ind());
        copy.set_declared_rows(rows);
        Ok(())
    }

    /// `zend_objects_store_del` (`Z/Zend/zend_objects_API.c:162`):
    /// nothing for an object the cycle collector is already freeing,
    /// whose type it turned to `IS_NULL`; otherwise the destructor
    /// served once with the count held at one, and, when nothing took an
    /// owner meanwhile, the bucket invalidated, the members and the
    /// payload freed, the object taken out of the root buffer, the id put
    /// on the free list and the bytes given back, all at php's moment
    /// whether or not a pin stands, which keeps only the bytes
    /// ([`Heap::give_back`]). An object freed so once and owned again
    /// since, by what its pin let name it, gives its bytes back alone.
    /// The heap is reached through `hooks` across each call up, since
    /// the destructor may grow the store.
    pub(crate) fn store_del(hooks: &mut dyn HeapHooks, o: Obj) {
        if o.gc().kind() == ZvalType::Null as u32 {
            return;
        }
        if !o.has_flag(gc::OBJ_DESTRUCTOR_CALLED) {
            o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
            o.header().gc.set_refcount(1);
            hooks.destruct(o);
            o.delref();
        }
        if o.refcount() != 0 {
            return;
        }
        if !o.has_flag(gc::OBJ_FREE_CALLED) {
            let handle = o.id().0;
            hooks.heap().store.invalidate(handle);
            o.add_flag(gc::OBJ_FREE_CALLED);
            o.header().gc.set_refcount(1);
            Heap::object_free(hooks, o);
            o.header().gc.set_refcount(0);
            let heap = hooks.heap();
            heap.gc.remove_from_buffer(crate::collector::Node::Obj(o));
            heap.store.add_to_free_list(handle);
        }
        // SAFETY: the object came from `object_alloc`, its bucket is
        // invalid, its members are freed, its id is free and the root
        // buffer names it no more, so only a pin names it.
        unsafe { hooks.heap().give_back(o.address(), o.pins() > 0) };
    }

    /// One pin [`Obj::pin`] took let go. The last one over an object
    /// that was freed while it stood and that nothing owns since gives
    /// its bytes back, the one part of `zend_objects_store_del` the pin
    /// kept waiting: its destructor, its members, its id and the memory
    /// php counts for it went at php's moment. An autoloader that
    /// unregisters itself from inside its call, which
    /// `zend_call_function` makes with the object lent, is freed there:
    /// its destructor and its members' release are served before the
    /// method goes on, whether or not its class serves a destructor, and
    /// the ids it held are reused at once, as php's are.
    ///
    /// ```php
    /// class In { function __destruct() { echo "inner gone\n"; } }
    /// class A { public $in; function __construct() { $this->in = new In; }
    ///     function __invoke($c) { spl_autoload_unregister($this); echo "unregistered\n"; }
    ///     function __destruct() { echo "A gone\n"; } }
    /// class B { public $in; function __construct() { $this->in = new In; }
    ///     function __invoke($c) { spl_autoload_unregister($this); echo "B unregistered\n"; } }
    /// spl_autoload_register(new A);
    /// class_exists('X');
    /// spl_autoload_register(new B);
    /// class_exists('Y');
    /// echo "end\n";
    /// ```
    /// ```expect
    /// A gone
    /// inner gone
    /// unregistered
    /// inner gone
    /// B unregistered
    /// end
    /// ```
    ///
    /// ```php
    /// class B { public $in; function __construct() { $this->in = new stdClass; }
    ///   function __invoke($c) { echo "B id ", spl_object_id($this), "\n"; spl_autoload_unregister($this); $n = new stdClass; $m = new stdClass; echo "new ids ", spl_object_id($n), " ", spl_object_id($m), "\n"; }
    ///   function __destruct() { echo "B gone\n"; } }
    /// spl_autoload_register(new B);
    /// class_exists('Y');
    /// echo "end\n";
    /// ```
    /// ```expect
    /// B id 1
    /// B gone
    /// new ids 1 2
    /// end
    /// ```
    pub fn unpin(&mut self, o: Obj) {
        let pins = o.pins();
        let word = &o.header().extra_flags;
        word.set((word.get() & !gc::OBJ_PINS) | pins.saturating_sub(1));
        if pins == 1 && o.refcount() == 0 {
            // SAFETY: where it is retired, the object was freed while the
            // pin stood, nothing owns it since, and the pin was the last
            // thing naming it.
            unsafe { self.mm.free_retired(o.address()) };
        }
    }

    /// `free_obj`: for an internal class, the class's own handler first,
    /// served through [`HeapHooks::free_native`] with the members still
    /// standing, then [`Heap::object_std_dtor`], then the payload's drop,
    /// as php's handlers release what they hold and call
    /// `zend_object_std_dtor` last (`php_converter_free_object`,
    /// `Z/ext/intl/converter/converter.c:906-919`). A handler that frees
    /// the members before its own release, as
    /// `spl_array_object_free_storage` does, calls
    /// [`Heap::object_std_dtor`] itself, and the call here finds nothing
    /// left. An object of no payload is `zend_object_std_dtor` alone.
    pub(crate) fn object_free(hooks: &mut dyn HeapHooks, o: Obj) {
        let native = o.has_native();
        if native {
            hooks.free_native(o);
        }
        Heap::object_std_dtor(hooks, o);
        if native {
            o.drop_payload();
        }
    }

    /// `zend_object_std_dtor` (`Z/Zend/zend_objects.c:77`): the weak
    /// references to the object told first (`zend_weakrefs_notify`,
    /// [`HeapHooks::weakrefs_notify`]), then what the engine keeps aside
    /// for a lazy object ([`HeapHooks::forget_lazy`]), then the dynamic
    /// table and the slots. Once is all it does: a second call over the
    /// same object finds the table gone, each slot `UNDEF`, the object
    /// eager and no weak reference left to tell, which is what lets a
    /// class's `free_obj` free the members where php's handler calls it
    /// and [`Heap::object_free`] call it after regardless.
    ///
    /// The dynamic table is given up as `zend_object_std_dtor` gives it
    /// up, `GC_DELREF` and `zend_array_destroy` at zero and never
    /// `gc_possible_root`: a table still shared is not a root.
    pub fn object_std_dtor(hooks: &mut dyn HeapHooks, o: Obj) {
        if o.is_weakly_referenced() {
            hooks.weakrefs_notify(o);
        }
        if o.laziness() != Laziness::Eager {
            o.set_laziness(Laziness::Eager);
            hooks.forget_lazy(o);
        }
        if let Some(table) = o.header().properties.take() {
            Heap::release_nogc(hooks, Zval::array(table));
        }
        for i in 0..o.slots() {
            let z = o.slot(i);
            if z.is_undef() {
                continue;
            }
            o.write_slot(i, Zval::UNDEF);
            if let Some(r) = z.as_reference()
                && r.sources() != 0
            {
                hooks.slot_source_gone(o, i, r);
            }
            Heap::release(hooks, z);
        }
    }

    /// `zend_objects_store_mark_destructed`
    /// (`Z/Zend/zend_objects_API.c:69`): every live object's destructor
    /// flagged as served, what a fatal does before it bails out, so none
    /// of the objects that stand then is destructed, and an object made
    /// after it, in a shutdown function, still is.
    pub fn mark_destructed(&mut self) {
        for handle in 1..self.store.top {
            if let Some(o) = self.store.get(handle) {
                o.add_flag(gc::OBJ_DESTRUCTOR_CALLED);
            }
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

    use crate::serial::alone;

    /// What the heap's tests pass where a request would serve a
    /// destructor: the heap itself, and a log of what would have been
    /// served, in the order it would have, which is how `zend_objects_store_del`'s
    /// ordering is held without a VM (design §5.10).
    struct Recorder {
        heap: Heap,
        log: Vec<(ObjectId, &'static str)>,
        /// Whether `free_native` frees the members first, as a handler
        /// that calls `zend_object_std_dtor` before its own release does.
        members_first: bool,
    }

    impl Recorder {
        fn new() -> Recorder {
            Recorder {
                heap: Heap::new(),
                log: Vec::new(),
                members_first: false,
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
            if self.members_first {
                Heap::object_std_dtor(self, obj);
            }
            self.log.push((obj.id(), "free_native"));
        }
        fn gc_children(&mut self, _obj: Obj, _: &mut dyn FnMut(Zval)) -> crate::GcTable {
            crate::GcTable::Properties
        }
        fn free_resource(&mut self, _: crate::Res, _: crate::ResourceType) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::out_of_memory(bytes)
        }
    }

    /// `free_obj` is served while the members stand, and the members are
    /// freed after it, as php's handlers call `zend_object_std_dtor` last
    /// (`php_converter_free_object`); a handler that frees them first, as
    /// `spl_array_object_free_storage` does, finds nothing left after.
    #[test]
    fn free_obj_is_served_before_the_members_unless_it_frees_them() {
        let _alone = alone();
        for members_first in [false, true] {
            let mut r = Recorder::new();
            r.members_first = members_first;
            let outer = r.heap.object_with(3, 1, 0, 0u8).expect("an object");
            let inner = r.heap.object(3, 0, 0).expect("an object");
            outer.write_slot(0, Zval::object(inner));
            let (o, i) = (outer.id(), inner.id());
            Heap::release(&mut r, Zval::object(outer));
            let expected = if members_first {
                vec![(o, "destruct"), (i, "destruct"), (o, "free_native")]
            } else {
                vec![(o, "destruct"), (o, "free_native"), (i, "destruct")]
            };
            assert_eq!(r.log, expected, "members first: {members_first}");
            assert_eq!(r.heap.objects(), 0);
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
        assert_eq!(start, crate::STARTED);
        assert_eq!(std::mem::size_of::<ObjHeader>(), 40);
        let plain = r.heap.object(7, 0, 0).expect("an object");
        assert_eq!(r.heap.memory_usage(false) - start, 40);
        assert_eq!(plain.class(), 7);
        assert_eq!(plain.refcount(), 1);
        let three = r.heap.object(7, 3, 0).expect("an object");
        // Forty and forty-eight, the 88 of php's `zend_object_alloc` in
        // the 96-byte bin.
        assert_eq!(r.heap.memory_usage(false) - start, 40 + 96);
        assert_eq!(three.slots(), 3);
        assert!(
            !three.set_class(8, 0),
            "a class of another layout is refused"
        );
        assert!(three.set_class(8, 3));
        assert_eq!((plain.class(), three.class()), (7, 8));
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
        let [first, second, third] =
            std::array::from_fn(|_| recorder.heap.object(1, 0, 0).expect("an object"));
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
            .map(|_| recorder.heap.object(1, 0, 0).ok().map(|o| o.id()))
            .collect();
        assert_eq!(
            given,
            vec![Some(ObjectId(1)), Some(ObjectId(2)), Some(ObjectId(4))]
        );
        assert_eq!(recorder.heap.objects(), 4);
    }

    /// A receiver a running frame names with no owner is pinned: its
    /// last release serves its destructor, frees its members, puts its
    /// id on the free list and counts its bytes off at php's moment,
    /// while the bytes stand until the last pin is let go, and an owner
    /// taken of it meanwhile and given up frees nothing twice.
    #[test]
    fn a_pinned_object_keeps_only_its_bytes_until_the_pin_goes() {
        let _alone = alone();
        let mut r = Recorder::new();
        let outer = r.heap.object(1, 1, 0).expect("an object");
        let inner = r.heap.object(1, 0, 0).expect("an object");
        let (id, member) = (outer.id(), inner.id());
        outer.write_slot(0, Zval::object(inner));
        let start = r.heap.memory_usage(false);
        outer.pin();
        outer.pin();
        assert_eq!(outer.pins(), 2);
        Heap::release(&mut r, Zval::object(outer));
        assert_eq!(
            r.log,
            vec![(id, "destruct"), (member, "destruct")],
            "the destructor and the member's release are served at the release"
        );
        assert!(outer.slot(0).is_undef(), "the members are gone");
        assert!(r.heap.stored(outer.id()).is_none());
        assert_eq!(
            start - r.heap.memory_usage(false),
            96,
            "the member's forty bytes and the object's fifty-six are counted off"
        );
        let other = r.heap.object(1, 0, 0).expect("an object");
        assert_eq!(other.id(), id, "the id is reused at once");
        Heap::release(&mut r, Zval::object(other));
        let (held, served) = (r.heap.memory_usage(false), r.log.len());
        outer.addref();
        Heap::release(&mut r, Zval::object(outer));
        r.heap.unpin(outer);
        assert_eq!(outer.pins(), 1);
        assert!(
            r.heap.mm.is_retired(outer.address()),
            "one pin still stands"
        );
        r.heap.unpin(outer);
        assert!(
            !r.heap.mm.is_retired(outer.address()),
            "the last pin gives the bytes back"
        );
        assert_eq!(r.heap.memory_usage(false), held, "counted once");
        assert_eq!(r.log.len(), served, "nothing is served twice");
    }

    /// The table doubles where php's doubles, on the object past the
    /// 1024th, and the objects made before it keep their buckets.
    #[test]
    fn the_store_doubles_where_php_s_doubles() {
        let _alone = alone();
        let mut r = Recorder::new();
        let mut held = Vec::new();
        for _ in 1..INIT_SIZE {
            held.push(r.heap.object(1, 0, 0).expect("an object"));
        }
        let start = r.heap.memory_usage(false);
        let grown = r
            .heap
            .object(1, 0, 0)
            .expect("an object past the store's first table");
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
        let o = r.heap.object(1, 1, 0).expect("an object");
        let s = r
            .heap
            .string(b"held by the slot")
            .expect("the string \"held by the slot\"");
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
            crate::STARTED,
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
        fn gc_children(&mut self, _obj: Obj, _: &mut dyn FnMut(Zval)) -> crate::GcTable {
            crate::GcTable::Properties
        }
        fn free_resource(&mut self, _: crate::Res, _: crate::ResourceType) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::out_of_memory(bytes)
        }
    }

    #[test]
    fn a_destructor_that_takes_an_owner_keeps_the_object() {
        let _alone = alone();
        let mut k = Keeper {
            heap: Heap::new(),
            served: 0,
        };
        let o = k.heap.object(1, 0, 0).expect("an object");
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
        let a = r.heap.array().expect("an array");
        for _ in 0..3 {
            let o = r.heap.object(1, 0, 0).expect("an object");
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
        assert_eq!(r.heap.memory_usage(false), crate::STARTED);
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
        let o = r
            .heap
            .object_with(3, 2, 0, Counted(Arc::clone(&dropped), 42))
            .expect("an object and its payload");
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
            matches!(r.heap.object_clone(o, 0), Ok(None)),
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
        let kept = r
            .heap
            .object_with(3, 0, 0, Counted(Arc::clone(&dropped), 1))
            .expect("an object");
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
    /// table is a copy of its own.
    #[test]
    fn a_clone_shares_what_the_old_holds_with_one_more_owner() {
        let _alone = alone();
        let mut r = Recorder::new();
        let o = r.heap.object(2, 1, 0).expect("an object");
        let s = r
            .heap
            .string(b"in the slot")
            .expect("the string \"in the slot\"");
        o.write_slot(0, Zval::string(s));
        let table = r.heap.object_properties(o).expect("the properties table");
        let name = r.heap.string(b"dyn").expect("the string \"dyn\"");
        assert!(Heap::array_update(&mut r, table, crate::Key::Str(name), Zval::long(1)).is_ok());
        // The bucket took its own owner of the key.
        r.heap.release_string(name);
        let new = r
            .heap
            .object_clone(o, 0)
            .expect("a clone")
            .expect("an object with no payload clones");
        assert_eq!(new.id(), ObjectId(2));
        assert_eq!(new.class(), 2);
        assert_eq!(s.refcount(), 2);
        assert_eq!(table.refcount(), 1);
        let copy = new.properties();
        assert_eq!(copy.map(|t| t.is_same(table)), Some(false));
        assert_eq!(
            copy.and_then(|t| t.get(crate::Key::Str(name)))
                .and_then(Zval::as_long),
            Some(1)
        );
        Heap::release(&mut r, Zval::object(o));
        assert_eq!(s.refcount(), 1);
        Heap::release(&mut r, Zval::object(new));
        assert_eq!(r.heap.memory_usage(false), crate::STARTED);
    }

    /// A clone with a payload is the class's: the members copied as a
    /// clone copies them, the payload the class handed in the new slot,
    /// and each payload dropped once with its own object.
    #[test]
    fn a_clone_with_a_payload_carries_the_copy_the_class_made() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        let _alone = alone();
        let dropped = Arc::new(AtomicU32::new(0));
        let mut r = Recorder::new();
        let o = r
            .heap
            .object_with(3, 1, 0, Counted(Arc::clone(&dropped), 42))
            .expect("an object");
        let s = r
            .heap
            .string(b"in the slot")
            .expect("the string \"in the slot\"");
        o.write_slot(0, Zval::string(s));
        let copied = r.heap.native::<Counted>(o).map_or(0, |c| c.1);
        let new = r
            .heap
            .object_clone_with(o, 0, Counted(Arc::clone(&dropped), copied))
            .expect("a clone");
        assert_eq!(new.id(), ObjectId(2));
        assert_eq!(new.class(), 3);
        assert_eq!(s.refcount(), 2);
        assert_eq!(r.heap.native::<Counted>(new).map(|c| c.1), Some(42));
        Heap::release(&mut r, Zval::object(o));
        assert_eq!(dropped.load(Relaxed), 1);
        assert_eq!(s.refcount(), 1);
        Heap::release(&mut r, Zval::object(new));
        assert_eq!(dropped.load(Relaxed), 2);
        assert_eq!(r.heap.memory_usage(false), crate::STARTED);
    }

    /// The shutdown walks: destructors in id order for what is still
    /// live, then the members freed from the top down, each once, and
    /// the bytes left for the mass release.
    #[test]
    fn the_shutdown_walks_serve_each_once_in_php_s_orders() {
        let _alone = alone();
        let mut r = Recorder::new();
        let [_a, b, _c] = std::array::from_fn(|_| r.heap.object(1, 0, 0).expect("an object"));
        Heap::release(&mut r, Zval::object(b));
        r.log.clear();
        Heap::call_destructors(&mut r);
        assert_eq!(
            r.log,
            vec![(ObjectId(1), "destruct"), (ObjectId(3), "destruct")]
        );
        // With reuse off, an object made now is above `top`, not in
        // `b`'s bucket.
        let late = r.heap.object(1, 0, 0).expect("an object");
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

    /// The pointer of an object's table moves in php's positions: each
    /// declared slot a row whether or not it holds a value, then the
    /// dynamic rows, a tombstone stepped over; the seek steps over a
    /// slot nothing holds in the way it is asked; and the copy a
    /// separation makes carries the pointer to the row it stood at.
    #[test]
    fn the_pointer_of_an_object_s_table_walks_slots_then_rows() {
        use crate::{Direction, TableRow};
        let _alone = alone();
        let mut r = Recorder::new();
        let o = r.heap.object(2, 3, 0).expect("an object");
        o.write_slot(1, Zval::long(1));
        let table = r.heap.object_properties(o).expect("the properties table");
        let mut keys = Vec::new();
        for (name, n) in [(&b"x"[..], 5), (&b"y"[..], 6)] {
            let key = r.heap.string(name).expect("a string");
            assert!(Heap::array_update(&mut r, table, crate::Key::Str(key), Zval::long(n)).is_ok());
            keys.push(key);
        }
        let t = r.heap.object_property_table(o).expect("the property table");
        assert_eq!((t.len(), t.used()), (5, 5));
        t.reset();
        assert_eq!(t.position(), 0);
        assert_eq!(
            t.seek(Direction::Forward).map(|(row, _)| row),
            Some(TableRow::Slot(1))
        );
        assert!(t.forward());
        assert_eq!(
            t.seek(Direction::Forward).map(|(row, _)| row),
            Some(TableRow::Entry(crate::Key::Str(keys[0])))
        );
        t.end();
        assert_eq!(t.position(), 4);
        assert!(t.backward() && t.backward());
        assert_eq!(t.position(), 2);
        assert_eq!(
            t.seek(Direction::Backward).map(|(row, _)| row),
            Some(TableRow::Slot(1))
        );
        assert!(Heap::array_del(&mut r, table, crate::Key::Str(keys[0])));
        t.end();
        assert!(t.backward());
        assert_eq!(t.position(), 2, "the tombstone is no row to a move");
        t.end();
        table.addref();
        let copy = r
            .heap
            .object_properties_copy(o, &[keys[0], keys[0], keys[1]])
            .expect("a copy of the properties");
        let copied = o.property_table().expect("the copied property table");
        assert!(copied.table().is_same(copy));
        assert_eq!(
            copied.seek(Direction::Forward).map(|(row, _)| row),
            Some(TableRow::Entry(crate::Key::Str(keys[1]))),
            "the copy's pointer stands at the row the source's stood at"
        );
        Heap::release(&mut r, Zval::array(table));
        for key in keys {
            r.heap.release_string(key);
        }
        Heap::release(&mut r, Zval::object(o));
        assert_eq!(r.heap.memory_usage(false), crate::STARTED);
    }
}
