// The compile-fail fixture of the `engine-state` rule (ADR-0005, #4477)
// at a class identity, compiled as `handle.rs` is: an extension's engine
// state derived over a field holding a `ClassId`, refused at the field. A
// `ClassId` may name a class of one request's own, numbered among that
// request's rows, or a row of a published arena a restart replaces; what
// engine state holds is the `EngineClassId` its start resolves, which the
// field beside it holds and passes.

use php_ext::abi::{EngineState, ExtensionBuilder, Refusal, Start};
use php_ext::attr::php_class;
use php_ext::values::{ClassId, EngineClassId};

/// Core's `Exception`.
#[php_class(extern)]
struct Exception;

/// The class a request last set to make its statements as, kept for
/// the next one, beside the class the extension raises.
#[derive(EngineState)]
struct Kept {
    raised: EngineClassId,
    statement: Option<ClassId>,
}

fn start(start: &Start) -> Result<Kept, Refusal> {
    Ok(Kept {
        raised: start.class::<Exception>()?,
        statement: None,
    })
}

pub fn extension() -> php_ext::abi::Extension {
    ExtensionBuilder::default()
        .engine_state(start)
        .build("kept")
}
