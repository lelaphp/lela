//! A hand-written impl whose path is spaced around its `::`: the sensor
//! reads syntax, so the spacing changes nothing, and it fails the crate
//! naming the impl's line.

use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

impl php_ext :: abi :: EngineState for Kept {}
