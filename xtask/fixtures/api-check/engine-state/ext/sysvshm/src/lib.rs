//! An impl in another crate's `src/ffi/`, reached by `#[path]`: a home
//! covers the crate's own files only, so the sensor fails this crate
//! naming the impl's line in the file it reached.

use php_ext::abi::EngineState;
use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

#[path = "../../json/src/ffi/borrowed.rs"]
mod borrowed;
