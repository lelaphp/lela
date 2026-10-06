//! A debug build's origin of every block the heap hands out
//! ([`crate::sensed::Origin`]), which the request leak sensor's walk
//! ([`super::leaks`]) names a block still held by.
//!
//! The origin is kept per block and not per `run` or page, because a bin
//! `run` serves every extension whose blocks fall in its bin, and the
//! leaks the sensor finds are mostly small strings. It is kept beside the
//! chunk and not in it: php's debug heap widens every block by its
//! `zend_mm_debug_info`, and the oracle is a release build, so a block
//! here takes the bytes a release build's does and `memory_get_usage()`
//! reads php's. Each chunk's header names one table of [`GRAINS`] origins,
//! one per eight bytes, which every block of the chunk starts on, since a
//! bin's size is a multiple of eight; a block's origin is one store where
//! it is handed out, and nothing where it is freed, since the walk reads
//! only blocks still held. The table is zeroed by the system's allocator
//! and so is touched only where blocks stand, and a huge block, which has
//! no chunk, keeps its origin in its own record.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;

use super::{CHUNK_SIZE, Chunk, Mm, chunk_of};
use crate::sensed::Origin;

/// The bytes one origin covers: every block of a chunk starts on one.
const GRAIN: usize = 8;
/// The origins of one chunk's table.
const GRAINS: usize = CHUNK_SIZE / GRAIN;

/// One chunk's table, zeroed, so every block reads as `Core`'s until it
/// is handed out; null where the system could not give it, and the
/// chunk's blocks then read as `Core`'s.
pub(super) fn table() -> *mut u16 {
    match Layout::array::<u16>(GRAINS) {
        // SAFETY: the layout has a non-zero size.
        Ok(layout) => unsafe { alloc_zeroed(layout) }.cast(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// The table of `chunk` given back, as the chunk leaves its heap's list.
pub(super) fn give_back(chunk: NonNull<Chunk>) {
    // SAFETY: the chunk is one its heap initialised, and nothing else
    // reads its header while the heap gives it up.
    let header = unsafe { &mut *chunk.as_ptr() };
    let origins = std::mem::replace(&mut header.origins, std::ptr::null_mut());
    if let (false, Ok(layout)) = (origins.is_null(), Layout::array::<u16>(GRAINS)) {
        // SAFETY: the table came from `table` with this layout, and the
        // header named it alone.
        unsafe { dealloc(origins.cast(), layout) };
    }
}

/// Where the origin of the block at `at`, in a chunk, stands.
fn slot(at: NonNull<u8>) -> Option<*mut u16> {
    let chunk = chunk_of(at);
    // SAFETY: `at` is a block of a chunk its heap holds, whose page zero
    // is the header `chunk_init` wrote.
    let origins = unsafe { (*chunk.as_ptr()).origins };
    let grain = (at.as_ptr().addr() & (CHUNK_SIZE - 1)) / GRAIN;
    // SAFETY: a block's offset in its chunk is under `CHUNK_SIZE`, so its
    // grain is inside the table.
    (!origins.is_null()).then(|| unsafe { origins.add(grain) })
}

/// The block at `at`, in a chunk, allocated for `origin`.
pub(super) fn write(at: NonNull<u8>, origin: Origin) {
    if let Some(slot) = slot(at) {
        // SAFETY: the slot is the block's in its chunk's table.
        unsafe { slot.write(origin.number()) };
    }
}

/// The origin the block at `at`, in a chunk, was allocated for.
pub(super) fn read(at: NonNull<u8>) -> Origin {
    // SAFETY: the slot is the block's in its chunk's table.
    slot(at).map_or_else(Origin::default, |slot| Origin::new(unsafe { slot.read() }))
}

impl Mm {
    /// What the heap allocates from here on is for `origin`, and the
    /// origin it replaces, for the engine to put back.
    pub(crate) fn set_origin(&mut self, origin: Origin) -> Origin {
        std::mem::replace(&mut self.origin, origin)
    }
}
