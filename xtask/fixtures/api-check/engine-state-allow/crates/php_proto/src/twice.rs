//! The module an entry names, with two hand impls, where an entry admits
//! one: both held.

use php_heap::EngineState;

pub(crate) struct First(u8);

impl EngineState for First {}

pub(crate) struct Second(u8);

impl EngineState for Second {}
