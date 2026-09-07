//! A heap with a public module, a private module re-exported from, an
//! inherent impl, and items that are not public.
pub mod stack;
mod hidden;

pub use hidden::Slot;

pub struct Arena;

impl Arena {
    pub fn new() -> Arena {
        Arena
    }
    fn private(&self) {}
}

pub(crate) fn not_public() {}
