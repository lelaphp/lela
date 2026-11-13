//! The module an entry names, which a `#[path]` in `lib.rs` also makes the
//! module `again`: held.

pub(crate) struct PathedTo(u8);

impl php_heap::EngineState for PathedTo {}
