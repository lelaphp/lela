//! Extension state as a request reaches it (ADR-0005): each bundled
//! extension's request state, built in bundle order as the request
//! starts and dropped in reverse, its engine state, and what the bundle
//! registered. The request states are [`RequestState`]'s, whose field no
//! file but this one reads.

use std::any::{Any, TypeId};

use crate::api::abi::States;

use super::Request;

/// The extensions' request states, whose field no file but this one
/// reads.
pub(super) struct RequestState {
    states: States<(TypeId, Box<dyn Any + Send>)>,
}

impl Default for RequestState {
    fn default() -> RequestState {
        RequestState {
            states: States::new(),
        }
    }
}

impl Request {
    /// Each extension's request state, built in bundle order (ADR-0005)
    /// as the request is laid.
    pub(super) fn build_request_states(&mut self) {
        for extension in &self.shared.extensions {
            if let Some((type_id, build)) = &extension.request_state {
                self.state.states.push((*type_id, build()));
            }
        }
    }

    /// Every extension's request state dropped, in reverse bundle order,
    /// before a debug build's request leak sensor reads the heap, so a
    /// state that kept a block of the request's past `rshutdown` leaves
    /// it held. A state touched after this is built afresh.
    #[cfg(debug_assertions)]
    pub(super) fn drop_request_states(&mut self) {
        drop(std::mem::take(&mut self.state));
    }

    /// The request state of the calling extension, `T` being the type it
    /// declared with `request_state::<T>()` (ADR-0005). A type no bundled
    /// extension declared is built on first touch here, where the design
    /// names a bug; the linker's slot table replaces this scan.
    pub fn state<T: Default + Send + 'static>(&mut self) -> &mut T {
        let wanted = TypeId::of::<T>();
        let at = if let Some(at) = self.state.states.iter().position(|(id, _)| *id == wanted) {
            at
        } else {
            self.state.states.push((wanted, Box::new(T::default())));
            self.state.states.len() - 1
        };
        // Found or held at `at`, and written as a `T` where it is not
        // one; each `else` is the type system's, not a path a request takes.
        let Some((_, slot)) = self.state.states.get_mut(at) else {
            return Box::leak(Box::new(T::default()));
        };
        if !slot.is::<T>() {
            *slot = Box::new(T::default());
        }
        let Some(state) = slot.downcast_mut::<T>() else {
            return Box::leak(Box::new(T::default()));
        };
        state
    }

    /// The request state of type `T` where it stands, read through a
    /// request held shared: what [`Request::state`] answers once built,
    /// and `None` before a type no extension declared is first touched.
    #[must_use]
    pub fn state_ref<T: Send + 'static>(&self) -> Option<&T> {
        let wanted = TypeId::of::<T>();
        self.state
            .states
            .iter()
            .find(|(id, _)| *id == wanted)
            .and_then(|(_, state)| state.downcast_ref::<T>())
    }

    /// The engine state of the calling extension, `E` being the type its
    /// `engine_state` built (ADR-0005). `None` for a type no extension
    /// built, where the design names a bug.
    #[must_use]
    pub fn engine<E: Send + Sync + 'static>(&self) -> Option<&E> {
        self.shared
            .engine_states
            .iter()
            .find_map(|state| state.downcast_ref::<E>())
    }

    /// Every value of type `T` an extension of the bundle handed over
    /// with [`ExtensionBuilder::register`], in bundle order: what
    /// `pdo_driver_hash` holds after every `MINIT` of the bundle has
    /// registered into it, immutable for the engine's life
    /// (ADR-0005).
    ///
    /// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
    pub fn registered<T: Send + Sync + 'static>(&self) -> impl Iterator<Item = &T> {
        self.shared
            .supplied
            .registered
            .under(TypeId::of::<T>())
            .filter_map(|value| value.downcast_ref::<T>())
    }
}
