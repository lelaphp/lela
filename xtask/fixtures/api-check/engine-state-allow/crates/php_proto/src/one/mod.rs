//! The module an entry names, by its `mod.rs`: its one hand impl passes.

mod child;

use php_heap::EngineState;

pub(crate) struct One(u8);

impl EngineState for One {}
