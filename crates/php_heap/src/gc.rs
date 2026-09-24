//! The refcounted header every PHP-visible heap value carries (design
//! §5.2–§5.5): php's `zend_refcounted_h`, eight bytes, a refcount and one
//! word that packs the type byte, the flags and the collector's colour
//! exactly as `Zend/zend_types.h:768-775` packs them.
//!
//! The header is shared: the string of §5.2 is the first value to carry
//! one, and the array, object, reference and resource carry the same one
//! when they land. The counts are `Cell`s because php mutates a refcount
//! through a shared pointer and so does every kernel above this crate.

use std::cell::Cell;

use crate::ZvalType;

/// `zend_refcounted_h`: the first eight bytes of every refcounted value.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct Refcounted {
    refcount: Cell<u32>,
    type_info: Cell<u32>,
}

/// `GC_FLAGS_MASK` (`Z/Zend/zend_types.h:769`); the type byte is the low
/// four bits below it and the collector's colour the bits above.
const FLAGS_MASK: u32 = 0x0000_03f0;

/// `GC_NOT_COLLECTABLE`: a string is never part of a cycle.
pub(crate) const NOT_COLLECTABLE: u32 = 1 << 4;
/// `GC_PROTECTED`: a walk is inside this value, php's recursion guard.
pub(crate) const PROTECTED: u32 = 1 << 5;
/// `GC_IMMUTABLE`, which strings spell `IS_STR_INTERNED`.
pub(crate) const IMMUTABLE: u32 = 1 << 6;
/// `GC_PERSISTENT`, which strings spell `IS_STR_PERSISTENT`: allocated
/// outside the request's heap, so the request's end does not free it.
pub(crate) const PERSISTENT: u32 = 1 << 7;
/// `IS_STR_PERMANENT`: relives the request boundary.
pub(crate) const PERMANENT: u32 = 1 << 8;
/// `IS_STR_VALID_UTF8`: the bytes were checked once and are valid UTF-8.
pub(crate) const VALID_UTF8: u32 = 1 << 9;
/// `IS_OBJ_WEAKLY_REFERENCED`, which is `GC_PERSISTENT` on an object
/// (`Z/Zend/zend_types.h:859`): a `WeakReference` or a `WeakMap` names
/// the object, so its free tells the engine before the members go.
pub(crate) const OBJ_WEAKLY_REFERENCED: u32 = PERSISTENT;
/// `IS_OBJ_DESTRUCTOR_CALLED`: `zend_objects_store_del` served the
/// destructor, and serves it no second time.
pub(crate) const OBJ_DESTRUCTOR_CALLED: u32 = 1 << 8;
/// `IS_OBJ_FREE_CALLED`: `free_obj` ran over the members, once.
pub(crate) const OBJ_FREE_CALLED: u32 = 1 << 9;
/// `IS_OBJ_LAZY_UNINITIALIZED`: above the flags, in the collector's
/// half of the word, where php keeps it (`Z/Zend/zend_types.h:867`).
pub(crate) const OBJ_LAZY_UNINITIALIZED: u32 = 1 << 31;

impl Refcounted {
    /// A header with one owner, of `kind`, with `flags` set.
    pub(crate) fn new(kind: ZvalType, flags: u32) -> Refcounted {
        Refcounted {
            refcount: Cell::new(1),
            type_info: Cell::new(kind as u32 | (flags & FLAGS_MASK)),
        }
    }

    /// `GC_REFCOUNT`.
    pub(crate) fn refcount(&self) -> u32 {
        self.refcount.get()
    }

    /// `GC_ADDREF`: one more owner, and the new count.
    pub(crate) fn addref(&self) -> u32 {
        let count = self.refcount.get().saturating_add(1);
        self.refcount.set(count);
        count
    }

    /// `GC_SET_REFCOUNT`: the count as `zend_objects_store_del` holds it
    /// at one across a destructor.
    pub(crate) fn set_refcount(&self, count: u32) {
        self.refcount.set(count);
    }

    /// `GC_DELREF`: one owner fewer, and the count that is left. A count
    /// already at zero stays there, so a double release cannot wrap.
    pub(crate) fn delref(&self) -> u32 {
        let count = self.refcount.get().saturating_sub(1);
        self.refcount.set(count);
        count
    }

    /// `GC_FLAGS`.
    pub(crate) fn flags(&self) -> u32 {
        self.type_info.get() & FLAGS_MASK
    }

    /// One bit of the whole word, for the bits php keeps above the flags.
    pub(crate) fn has_info(&self, bit: u32) -> bool {
        self.type_info.get() & bit != 0
    }

    /// `GC_ADD_FLAGS`.
    pub(crate) fn add_flags(&self, flags: u32) {
        self.type_info
            .set(self.type_info.get() | (flags & FLAGS_MASK));
    }

    /// `GC_DEL_FLAGS`.
    pub(crate) fn del_flags(&self, flags: u32) {
        self.type_info
            .set(self.type_info.get() & !(flags & FLAGS_MASK));
    }
}
