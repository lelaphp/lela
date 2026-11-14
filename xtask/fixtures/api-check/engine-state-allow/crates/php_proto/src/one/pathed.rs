//! A file under the directory of the module an entry names, which a
//! `#[path]` in `lib.rs` makes another module: held.

use php_heap::EngineState;

pub(crate) struct Pathed(u8);

impl EngineState for Pathed {}
