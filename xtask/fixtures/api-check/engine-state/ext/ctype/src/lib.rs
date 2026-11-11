//! A hand-written impl through a renamed import: the impl's path does not
//! end in `EngineState`, but the `use` names what it renames. The
//! `engine-state` sensor fails the crate naming the impl's line.

use php_ext::abi::EngineState as Frozen;
use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

impl Frozen for Kept {}
