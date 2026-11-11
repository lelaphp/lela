// The compile-fail fixture of the `engine-state` rule (#4348 ruling 2,
// ADR-0005) at the epoch's tier slot (#4492): a type that is not engine
// state, a request's value in its field, handed to `tier::epoch_state`,
// refused by its bound. Compiled by `php_engine`'s doctest on
// `epoch_state`, under a `compile_fail,E0277` pin as `handle.rs` is.

use php_engine::api::request::Request;
use php_engine::api::values::Value;
use php_engine::tier::epoch_state;

/// What the first request of the epoch handed the tier, kept for every
/// other request of it.
pub struct Kept {
    last: Value,
}

pub fn keep(req: &Request, last: Value) -> Option<&Kept> {
    epoch_state(req, move || Kept { last })
}
