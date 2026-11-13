//! A child module's file, under the directory of the module an entry
//! names: held.

use php_heap::EngineState;

pub(crate) struct Child(u8);

impl EngineState for Child {}
