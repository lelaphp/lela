//! The stand-in for the kinds not yet on the heap: a resource or a
//! reference written into an array is owned Rust data still (design
//! §11, days 3–7), and an array's bucket is sixteen bytes that must name
//! it. So the value is boxed, the box carries the refcounted header
//! every heap value carries, and the heap counts it as it counts a
//! string: one owner per `addref`, dropped when the last gives it up,
//! and swept with the request when none did, as the object store is
//! swept at shutdown. What is boxed is on the system allocator and is
//! not in `memory_get_usage()`; the reference and the resource of
//! design §5.5 replace it.

use std::any::Any;
use std::ptr::NonNull;

use crate::ZvalType;
use crate::gc::Refcounted;

/// One boxed value: the header, the value and where the sweep list
/// holds it.
#[repr(C)]
pub(crate) struct Boxed {
    gc: Refcounted,
    slot: u32,
    value: Box<dyn Any + Send>,
}

impl Boxed {
    /// The value.
    pub(crate) fn value(&self) -> &(dyn Any + Send) {
        self.value.as_ref()
    }

    /// `GC_ADDREF`.
    pub(crate) fn addref(&self) {
        self.gc.addref();
    }

    /// `GC_DELREF`, and what is left.
    pub(crate) fn delref(&self) -> u32 {
        self.gc.delref()
    }
}

/// The boxes one heap holds, so that the request's end drops what no
/// release did: php's `zend_objects_store` walked by
/// `zend_objects_store_free_object_storage`, in the one shape it needs
/// here, a slot per box and a free list of the slots given back.
#[derive(Default)]
pub(crate) struct Boxes {
    slots: Vec<Option<NonNull<Boxed>>>,
    free: Vec<u32>,
}

impl Boxes {
    /// Box `value` as a zval of `kind`, with one owner.
    pub(crate) fn add(&mut self, kind: ZvalType, value: Box<dyn Any + Send>) -> crate::Zval {
        let slot = self.free.pop().unwrap_or_else(|| {
            self.slots.push(None);
            u32::try_from(self.slots.len() - 1).unwrap_or(u32::MAX)
        });
        let boxed = Box::new(Boxed {
            gc: Refcounted::new(kind, 0),
            slot,
            value,
        });
        let at = NonNull::from(Box::leak(boxed));
        if let Some(cell) = self.slots.get_mut(slot as usize) {
            *cell = Some(at);
        }
        crate::Zval::boxed(kind, at)
    }

    /// Drop the box `at`, whose last owner gave it up.
    ///
    /// # Safety
    ///
    /// `at` came from [`Boxes::add`] on this same list and has not been
    /// dropped since.
    pub(crate) unsafe fn drop_box(&mut self, at: NonNull<Boxed>) {
        // SAFETY: the caller's contract: the box is live and this list's.
        let boxed = unsafe { Box::from_raw(at.as_ptr()) };
        if let Some(cell) = self.slots.get_mut(boxed.slot as usize) {
            *cell = None;
        }
        self.free.push(boxed.slot);
        drop(boxed);
    }
}

impl Drop for Boxes {
    /// The sweep: every box no release dropped goes with the request.
    fn drop(&mut self) {
        for at in self.slots.drain(..).flatten() {
            // SAFETY: a slot names a box `add` leaked and nothing dropped,
            // since `drop_box` clears the slot it drops.
            drop(unsafe { Box::from_raw(at.as_ptr()) });
        }
    }
}

impl std::fmt::Debug for Boxes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self.slots.iter().filter(|s| s.is_some()).count();
        write!(f, "Boxes({held} boxed)")
    }
}
