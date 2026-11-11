//! The library's compiled table, read by every thread once made.

use php_ext::abi::EngineState;

pub(crate) struct Table(Box<[u8]>);

// Engine state (ADR-0005): the library's bytes, never a request's heap.
impl EngineState for Table {}
