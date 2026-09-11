//! The memory model (design §4.1, §5.1–§5.6, §5.13, §5.14): the zval and
//! its refcounted header, the heap that allocates and accounts for every
//! PHP-visible value, the VM stack with its entry slot, the map table
//! that gives an immutable shared structure its per-request state, and
//! the one upward seam, [`HeapHooks`], through which a refcount reaching
//! zero reaches user code without this crate knowing what a VM is.
//!
//! The crate is the first of the `unsafe` homes the manifest names, and
//! this skeleton uses none: every layout here is a safe stand-in whose
//! public shape is `heap.lock`, and the copy of `zend_alloc`,
//! `zend_hash` and `zend_vm_stack` lands behind it (design §11, days 3–7).
//! Nothing here panics: a slot that is not there reads as `UNDEF`, as
//! php's uninitialised zval does.
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

mod hooks;
mod map;
mod stack;
mod string;
mod zval;

pub use hooks::{Fatal, HeapHooks};
pub use map::{MapPtr, MapTable};
pub use stack::{EntryGuard, Frame, FrameRef, Slot, VmStack};
pub use string::Str;
pub use zval::{ObjectId, Zval, ZvalType};

/// The per-request heap (design §5.13): the allocator every string, array,
/// object and frame passes through, and the only accountant of memory.
/// `memory_get_usage()` reads it; no other item lets a native tell the
/// engine about memory.
#[derive(Debug, Default)]
pub struct Heap {
    used: usize,
    peak: usize,
}

impl Heap {
    /// A heap with nothing allocated.
    #[must_use]
    pub fn new() -> Heap {
        Heap::default()
    }

    /// Bytes in use: the bin accounting for `real == false`, whole chunks
    /// for `real == true` (design §5.13). This stand-in allocates nothing,
    /// so both read as zero.
    #[must_use]
    pub fn memory_usage(&self, real: bool) -> usize {
        let _ = real;
        self.used
    }

    /// The high-water mark of [`Heap::memory_usage`] since the request
    /// began or the peak was last reset.
    #[must_use]
    pub fn memory_peak(&self) -> usize {
        self.peak
    }

    /// `memory_reset_peak_usage()`: the peak becomes the current usage.
    pub fn reset_peak(&mut self) {
        self.peak = self.used;
    }

    /// One more owner of `z`: a refcount increment for a refcounted value,
    /// nothing for a scalar or an immutable value (`ZVAL_COPY`).
    pub fn addref(&self, z: Zval) {
        let _ = z;
    }

    /// One owner fewer: a refcount decrement, and when it reaches zero the
    /// value is freed through `hooks`, which may serve a destructor
    /// (`zval_ptr_dtor`). The parameter is what says this may re-enter.
    pub fn release(&mut self, z: Zval, hooks: &mut dyn HeapHooks) {
        let _ = (z, hooks);
    }
}
