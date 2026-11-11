//! Outside every crate directory, reached by ext/gettext's `#[path]`.

use php_ext::abi::EngineState;

impl EngineState for super::Kept {}
