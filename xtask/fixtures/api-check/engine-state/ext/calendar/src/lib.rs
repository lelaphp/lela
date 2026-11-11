//! The impl the build script writes, included from `OUT_DIR`.

use php_ext::abi::EngineState;
use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

include!(concat!(env!("OUT_DIR"), "/kept.rs"));
