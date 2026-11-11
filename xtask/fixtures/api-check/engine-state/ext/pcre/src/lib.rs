//! The seal attempt (#4348 ruling 2): an extension making its type engine
//! state by an impl written by hand, around the derive, which holds no
//! field to the bound, so a request's value rides into engine state. The
//! `engine-state` sensor fails the crate naming the impl's line.

use php_ext::abi::EngineState;
use php_ext::values::Value;

/// What a hand-written impl lets through: a request's value, kept
/// engine-wide.
pub(crate) struct Kept {
    last: Value,
}

impl EngineState for Kept {}
