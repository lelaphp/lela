//! Engine state whose type is erased (#4492): a slot the engine keeps
//! for a value it does not name, such as the compiled tier's value for an
//! epoch (`php_engine`'s `Arena::tier`), which every request of the epoch
//! reads. `Box<dyn Any + Send + Sync>` says nothing of what it holds, so
//! a slot of it would take a value holding a pointer into a request's
//! heap; [`AnyState`] is made from an [`EngineState`] alone, and is one.

use std::any::Any;

use crate::EngineState;

/// A value of engine state behind its erased type: built only from a
/// `T: EngineState`, read back as the `T` it was built from.
pub struct AnyState(Box<dyn Any + Send + Sync>);

impl AnyState {
    /// `value`, its type erased.
    pub fn new<T: EngineState + Any + Send + Sync>(value: T) -> AnyState {
        AnyState(Box::new(value))
    }

    /// The value, where it is a `T`.
    #[must_use]
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}

// Its one constructor takes a value of engine state, and nothing reaches
// the box but through it.
impl EngineState for AnyState {}

#[cfg(test)]
mod tests {
    use super::AnyState;

    #[test]
    fn a_value_is_read_back_as_its_own_type_and_no_other() {
        let state = AnyState::new(std::sync::Arc::new(7_u32));
        assert_eq!(
            state.downcast_ref::<std::sync::Arc<u32>>().map(|n| **n),
            Some(7)
        );
        assert!(state.downcast_ref::<u32>().is_none());
    }
}
