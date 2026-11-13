//! The module the entry names: its hand impl passes under the entry, and
//! the trait handed to a macro whose body no source holds fails here as
//! anywhere, since the entry admits an impl and nothing else.

use php_heap::EngineState;

/// A value its checked constructor vouches for.
pub(crate) struct Frozen<T>(T);

impl<T> EngineState for Frozen<T> {}

php_heap::vouched!(EngineState for Frozen<u8>);
