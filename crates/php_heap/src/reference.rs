//! References (design §5.5): php's `zend_reference`
//! (`Z/Zend/zend_types.h:608-612`), the one cell several holders share
//! when `&$x` binds them: the refcounted header, the zval held and the
//! typed-property sources list, thirty-two bytes out of the request's
//! heap, which is what `ZVAL_NEW_REF` (`Z/Zend/zend_types.h:1230`)
//! `emalloc`s and what `memory_get_usage()` moves by for one.
//!
//! A [`Ref`] is the pointer php's `zend_reference *` is, `Copy`, no
//! `Drop`, its owners counted by [`crate::Heap::addref`] and
//! [`crate::Heap::release`]; the release that reaches zero is
//! `zend_reference_destroy` (`Z/Zend/zend_variables.c:71`), one step of
//! [`crate::Heap`]'s `destroy`: the value held released first, through
//! the hooks since it may be an object, and the cell given back after
//! it. A write through a reference is
//! [`crate::Heap::reference_set`], the middle of `zend_assign_to_variable`
//! (`Z/Zend/zend_execute.h:171-183`): the value held is replaced first
//! and what it held released last, which is where a destructor is
//! served.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::gc::{self, Counted, Refcounted};
use crate::hooks::{Fatal, HeapHooks};
use crate::{Arr, Heap, Zval, ZvalRef, ZvalType};

/// `zend_reference`: thirty-two bytes, php's own order.
#[repr(C)]
pub(crate) struct RefHeader {
    /// `zend_refcounted_h gc`, `GC_REFERENCE`: not collectable on the
    /// header, as php marks it.
    gc: Refcounted,
    /// `val`: the zval held.
    val: Cell<Zval>,
    /// `sources`: php's typed-property sources list, a pointer-sized
    /// word. php hangs a `zend_property_info *` or a list of them here;
    /// the word is opaque to the heap, which reads it only to ask
    /// [`HeapHooks::free_reference`] to give up what it names, and the
    /// engine names its own source list by it
    /// (`Request::ref_add_type_source`).
    sources: Cell<usize>,
}

// The header at the block's start, where `Zval::counted` reads it for
// every counted kind without asking which.
const _: () = assert!(std::mem::offset_of!(RefHeader, gc) == 0);

/// One reference: php's `zend_reference *`. Naming one does not keep it
/// alive, exactly as php's pointer does not; the engine's kernels take
/// and give up owners through [`crate::Heap::addref`] and
/// [`crate::Heap::release`].
#[derive(Clone, Copy)]
pub struct Ref(NonNull<RefHeader>);

// SAFETY: a `Ref` is the pointer php's `zend_reference *` is: a
// request's reference moves with the request that owns it, whole, to the
// thread that serves it (design §5.14). The refcount `Cell` is what
// keeps it off `Sync`.
unsafe impl Send for Ref {}

impl Ref {
    /// Where in `zend_reference` the value it holds is, `val`: what the
    /// compiled tier reads a variable seen through a reference at
    /// (`jit-design.md` §4.3).
    pub const VALUE_AT: usize = std::mem::offset_of!(RefHeader, val);

    fn header(&self) -> &RefHeader {
        // SAFETY: a `Ref` names a reference `init` laid down, live while
        // its owner holds it, which is php's `zend_reference *` contract.
        unsafe { self.0.as_ref() }
    }

    /// Where the header sits, which is where the allocation starts, for
    /// the heap that frees it.
    pub(crate) const fn address(self) -> NonNull<u8> {
        self.0.cast()
    }

    /// The payload word of a zval naming this reference.
    pub(crate) fn payload(self) -> crate::zval::Word {
        crate::zval::Word::pointer(self.0)
    }

    /// The reference a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Ref::payload`] for a reference still live.
    pub(crate) unsafe fn from_payload(payload: crate::zval::Word) -> Option<Ref> {
        // SAFETY: the word holds the pointer `payload` stored, by the
        // contract.
        unsafe { payload.as_pointer::<RefHeader>() }.map(Ref)
    }

    /// `ZVAL_NEW_REF` over a fresh allocation: one owner, `z` the
    /// reference's own from here, no sources.
    ///
    /// # Safety
    ///
    /// `at` is writable for `size_of::<RefHeader>()` bytes and holds
    /// nothing else.
    unsafe fn init(at: NonNull<u8>, z: Zval) -> Ref {
        let header: NonNull<RefHeader> = at.cast();
        // SAFETY: the caller's allocation is the header and nothing else
        // holds it.
        unsafe {
            header.as_ptr().write(RefHeader {
                gc: Refcounted::new(ZvalType::Reference, gc::NOT_COLLECTABLE),
                val: Cell::new(z),
                sources: Cell::new(0),
            });
        }
        Ref(header)
    }

    /// `Z_REFVAL`: the zval held, copied, no owner taken.
    #[must_use]
    pub fn val(&self) -> Zval {
        self.header().val.get()
    }

    /// `Z_REFVAL_P`: the zval held in place, the `zval *` php writes a
    /// dimension through where a reference stands in the container
    /// (`zend_fetch_dimension_address`'s `IS_REFERENCE` arm). The cell
    /// keeps it alive, as php's cell keeps its `val`.
    #[must_use]
    pub fn val_ref(&self) -> ZvalRef {
        // `val` is a `Cell<Zval>`, which is `repr(transparent)` over the
        // zval it holds, so this is php's `&ref->val` and reads nothing.
        ZvalRef::new(NonNull::from(&self.header().val).cast())
    }

    /// `zend_refcounted_h gc`: the header the cycle collector colours
    /// as it walks through the cell, never a root of its own.
    pub(crate) fn gc(self) -> Counted {
        // SAFETY: the block starts with its header, and stands while
        // its owner holds the handle, as `header` reads it.
        unsafe { Counted::new(self.address()) }
    }

    /// Write the zval held as it stands, no owner touched.
    pub(crate) fn write_val(self, z: Zval) {
        self.header().val.set(z);
    }

    /// `ZEND_REF_HAS_TYPE_SOURCES` as one word: what the engine hung on
    /// this cell to type it, zero for a cell no typed property's slot
    /// holds. The heap never reads what it names.
    #[must_use]
    pub fn sources(&self) -> usize {
        self.header().sources.get()
    }

    /// `ZEND_REF_ADD_TYPE_SOURCE` and `ZEND_REF_DEL_TYPE_SOURCE` as one
    /// write: the engine's word, zero to say the cell is typed by
    /// nothing. The engine gives up what the old word named before it
    /// writes a new one, as php's `zend_ref_del_type_source` does.
    pub fn set_sources(&self, word: usize) {
        self.header().sources.set(word);
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

    /// Whether two names name one reference, php's pointer equality.
    #[must_use]
    pub fn is_same(&self, other: Ref) -> bool {
        self.0 == other.0
    }
}

/// A reference hashes as the one cell it names, by the address
/// [`Ref::is_same`] compares, so a table keyed by references keys them
/// by identity, as php's pointer would.
impl std::hash::Hash for Ref {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl std::fmt::Debug for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "reference({:?})", self.val().kind())
    }
}

impl Heap {
    /// `ZVAL_NEW_REF` (`Z/Zend/zend_types.h:1230`): a reference holding
    /// `z`, one owner, `z` the reference's own from here, as
    /// `ZVAL_COPY_VALUE` into `_ref->val` takes no owner of its own; the
    /// caller that keeps a second name of what `z` names takes it.
    pub fn reference(&mut self, z: Zval) -> Result<Ref, Fatal> {
        let at = self.mm.alloc(size_of::<RefHeader>())?;
        // SAFETY: the allocation is exactly the header's size and nothing
        // else holds it.
        Ok(unsafe { Ref::init(at, z) })
    }

    /// `SEPARATE_ZVAL_NOREF` on the zval a reference holds
    /// (`Z/Zend/zend_types.h`), which is `Z_PARAM_ARRAY_EX`'s separate on
    /// a by-reference parameter, what `sort(&$array)` writes through:
    /// the array held, itself where it has one owner, else a copy with
    /// one written into the cell and the original one owner fewer,
    /// `GC_TRY_DELREF`, and nothing released since an owner remains;
    /// `None` where the cell holds no array, which is the caller's
    /// `TypeError` to raise.
    pub fn reference_separate(&mut self, r: Ref) -> Result<Option<Arr>, Fatal> {
        let Some(a) = r.val().as_arr() else {
            return Ok(None);
        };
        let separated = self.separate(a)?;
        if !separated.is_same(a) {
            r.write_val(Zval::array(separated));
        }
        Ok(Some(separated))
    }

    /// The reference's half of `zend_assign_to_variable`
    /// (`Z/Zend/zend_execute.h:171-183`): `z` is what the reference
    /// holds from here, and what it held is released last, after the
    /// write, which is where a destructor is served. The typed-property
    /// sources, which `zend_assign_to_typed_ref` reads first, come with
    /// typed properties.
    pub fn reference_set(hooks: &mut dyn HeapHooks, r: Ref, z: Zval) {
        let old = r.val();
        r.write_val(z);
        Heap::release(hooks, old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Obj;

    use crate::serial::alone;

    /// The heap's tests pass this where a request would serve a
    /// destructor: the heap itself, and what would have been served.
    struct Recorder {
        heap: Heap,
        served: Vec<u32>,
    }

    impl HeapHooks for Recorder {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }
        fn destruct(&mut self, obj: Obj) {
            self.served.push(obj.id().0);
        }
        fn free_native(&mut self, _: Obj) {}
        fn gc_children(&mut self, _: Obj, _: &mut dyn FnMut(Zval)) -> crate::GcTable {
            crate::GcTable::Properties
        }
        fn free_resource(&mut self, _: crate::Res, _: crate::ResourceType) {}
        fn out_of_memory(&mut self, bytes: usize) -> Fatal {
            Fatal::out_of_memory(bytes)
        }
    }

    /// A reference is php's thirty-two bytes, the zval naming it is
    /// refcounted and collectable as `IS_REFERENCE_EX` is, a write
    /// through it releases what it held after the write, and the last
    /// release gives the value held back and then the cell.
    #[test]
    fn a_reference_is_thirty_two_bytes_and_writes_through() {
        let _alone = alone();
        assert_eq!(size_of::<RefHeader>(), 32);
        let mut rec = Recorder {
            heap: Heap::new(),
            served: Vec::new(),
        };
        let start = rec.heap.memory_usage(false);
        let Ok(s) = rec.heap.string(b"held by a ref") else {
            return;
        };
        let Ok(r) = rec.heap.reference(Zval::string(s)) else {
            return;
        };
        assert_eq!(rec.heap.memory_usage(false) - start, 32 + 40);
        let z = Zval::reference(r);
        assert!(z.is_refcounted());
        assert_eq!(z.kind(), ZvalType::Reference);
        assert_eq!(z.as_reference().map(|r| r.val()), Some(Zval::string(s)));
        assert_eq!(r.refcount(), 1);
        rec.heap.addref(z);
        assert_eq!(
            r.refcount(),
            2,
            "one more owner of the cell, not of the string"
        );
        assert_eq!(s.refcount(), 1);
        // A write through the reference: the string is released after
        // the write and the heap reads the cell and the long alone.
        let Ok(o) = rec.heap.object(7, 0, 0) else {
            return;
        };
        Heap::reference_set(&mut rec, r, Zval::object(o));
        assert_eq!(rec.heap.memory_usage(false) - start, 32 + 40);
        assert_eq!(r.val().as_obj().map(|o| o.id().0), Some(1));
        assert!(rec.served.is_empty());
        // The last release of the reference releases what it holds,
        // which serves the object's destructor, and gives the cell back.
        Heap::release(&mut rec, z);
        assert_eq!(r.refcount(), 1);
        assert!(rec.served.is_empty());
        Heap::release(&mut rec, z);
        assert_eq!(rec.served, vec![1]);
        assert_eq!(rec.heap.memory_usage(false), start);
        assert_eq!(rec.heap.objects(), 0);
    }
}
