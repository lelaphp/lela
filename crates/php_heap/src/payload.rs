//! The native payload's table (design §5.4): what the heap needs of a
//! payload's type and nothing more, its layout, its type and its drop,
//! one table per `T` promoted to `'static` from a constant, so an object
//! header holds a thin pointer where php holds `handlers` and a resource
//! carries the same table beside its `ptr`. The drop is derived from the
//! type; what a payload names of the heap is given back by the engine
//! through the hooks before the drop runs.

use std::alloc::Layout;
use std::any::{Any, TypeId};
use std::ptr::NonNull;

/// What the heap needs of a payload's type, php's `handlers->offset` and
/// `free_obj` for the slot alone: its layout, its type and its drop.
pub(crate) struct Payload {
    pub(crate) layout: Layout,
    pub(crate) type_id: TypeId,
    pub(crate) drop: unsafe fn(NonNull<u8>),
}

impl Payload {
    /// Whether the table is `T`'s.
    pub(crate) fn is<T: Any>(&self) -> bool {
        self.type_id == TypeId::of::<T>()
    }
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
pub(crate) fn payload_of<T: Any + Send>() -> &'static Payload {
    const {
        &Payload {
            layout: Layout::new::<T>(),
            type_id: TypeId::of::<T>(),
            drop: drop_payload::<T>,
        }
    }
}
