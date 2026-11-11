//! Impls in files outside every crate directory, reached by `#[path]` and
//! by `include!`: the sensor follows both and fails the crate naming each
//! impl's line in the file it reached.

use php_ext::abi::EngineState;
use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

pub(crate) struct Also {
    last: Value,
}

#[path = "../../../elsewhere/kept.rs"]
mod kept;

include!("../../../elsewhere/also.rs");
