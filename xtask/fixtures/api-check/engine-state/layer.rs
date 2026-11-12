// The compile-fail fixture of the `engine-state` rule (#4348 ruling 2,
// ADR-0005) at the stream layer's state (#4475): a type that is not
// engine state, a request's value in its field, handed to
// `StateHook::of`, refused by its bound. Compiled by `php_engine`'s
// doctest on `StateHook`, under a `compile_fail,E0277` pin as `epoch.rs`
// is.

use php_engine::api::values::Value;
use php_engine::layer::StateHook;

/// What a request last handed the layer, kept for the next one.
#[derive(Default)]
pub struct Kept {
    last: Option<Value>,
}

pub const STATE: StateHook = StateHook::of::<Kept>();
