// The compile-fail fixture of the `engine-state` rule (#4348 ruling 2,
// ADR-0005), compiled by `php_bundle`'s `refusals` under a
// `compile_fail,E0277` pin that `xtask test` holds: an extension's engine
// state derived over a field holding a request's value, a handle into a
// request's heap, refused at the field.

use php_ext::abi::{EngineState, ExtensionBuilder, Refusal, Start};
use php_ext::values::Value;

/// What a request last handed the extension, kept for the next one.
#[derive(EngineState)]
struct Kept {
    calls: u64,
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
