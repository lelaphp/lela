//! A module no entry names: its hand impl fails, with the entry or
//! without it.

use php_heap::EngineState;

/// A request's own value, which no check vouches for.
pub(crate) struct Thawed(u8);

impl EngineState for Thawed {}
