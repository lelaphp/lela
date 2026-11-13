// The module an entry names, which `lib.rs` also `include!`s, so its impl
// stands in the crate root too: held.

pub(crate) struct Included(u8);

impl php_heap::EngineState for Included {}
