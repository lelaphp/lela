// The compile-fail fixture of the `engine-state` rule (#4348 ruling 2,
// ADR-0005) at the entry: a type that is not engine state, a request's
// value in its field and no derive, handed to the engine start, refused
// by `engine_state`'s bound. Compiled as `handle.rs` is.

use php_ext::abi::{ExtensionBuilder, Refusal, Start};
use php_ext::values::Value;

/// What a request last handed the extension, kept for the next one.
struct Kept {
    last: Value,
}

fn start(_: &Start) -> Result<Kept, Refusal> {
    Err(Refusal::new())
}

pub fn extension() -> php_ext::abi::Extension {
    ExtensionBuilder::default()
        .engine_state(start)
        .build("kept")
}
