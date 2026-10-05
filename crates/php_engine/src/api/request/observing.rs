//! The observer's state on a request (design §4.3 seam 9,
//! [`crate::observer`]): the chain of frames begun with an end handler
//! and not yet ended, php's `EG(current_observed_frame)`, which is
//! [`Observing`]'s and whose field no file but this one reads; the
//! observer slot of the op array a frame serves, which stands in the
//! request's map table beside the runtime cache's other slots; and the
//! handlers added to and removed from one function's slot while the
//! request is served, php's `zend_observer_add_begin_handler` and its
//! kin, which write the request's slot and never the published unit.

use php_heap::FrameRef;

use crate::api::call::Function;
use crate::exec;
use crate::observer::{BeginHandler, EndHandler, Removed, Slot};

use super::Request;

/// The open observed frames, the innermost last: php's
/// `EG(current_observed_frame)` and each frame's `prev_observed_frame`.
#[derive(Debug, Default)]
pub(super) struct Observing(Vec<FrameRef>);

impl Request {
    /// `ZEND_OBSERVER_DATA(EX(func))`: the observer slot of the op array
    /// `frame` serves ([`exec::Vm::observer_slot`]).
    pub(crate) fn observer_slot(&self, frame: FrameRef) -> Slot {
        self.vm.observer_slot(&self.map, frame)
    }

    /// That slot resolved to `slot` (`zend_observer_fcall_install`).
    pub(crate) fn set_observer_slot(&mut self, frame: FrameRef, slot: Slot) {
        self.vm.set_observer_slot(&mut self.map, frame, slot);
    }

    /// `frame` begun with an end handler: the innermost open frame now.
    pub(crate) fn open_observed(&mut self, frame: FrameRef) {
        self.observing.0.push(frame);
    }

    /// `frame` taken off the chain where it is the innermost open frame,
    /// and whether it was: php's `execute_data == EG(current_observed_frame)`.
    pub(crate) fn close_observed(&mut self, frame: FrameRef) -> bool {
        if self.observing.0.last() != Some(&frame) {
            return false;
        }
        self.observing.0.pop();
        true
    }

    /// Every open frame, the innermost first, the chain left empty:
    /// `zend_observer_fcall_end_all`'s walk.
    pub(crate) fn take_observed(&mut self) -> Vec<FrameRef> {
        let mut open = std::mem::take(&mut self.observing.0);
        open.reverse();
        open
    }
}

impl Request {
    /// `zend_observer_add_begin_handler` (`Z/Zend/zend_observer.c:184`):
    /// `begin` added to `function`'s slot in this request, after the begin
    /// handlers it holds, so each call from now on is begun by it too.
    /// php allows it only to an extension that declared an observer
    /// ([`ExtensionBuilder::observer`]), and a slot holds one begin handler
    /// per observer declared: false where it holds that many already, or
    /// where the slot is not resolved yet, a function not called in this
    /// request, whose observers' initialisers answer for it on its first
    /// call. A frame of `function` already begun is not begun again.
    ///
    /// The slot is the request's, in the runtime cache of the function's
    /// op array, so what one request adds no other request sees, and the
    /// unit the function was compiled into, shared by every request, is
    /// not written. A function of an extension has its slot where
    /// natives are observed; this reaches a function of the program.
    ///
    /// [`ExtensionBuilder::observer`]: crate::api::abi::ExtensionBuilder::observer
    pub fn add_begin_handler(&mut self, function: Function, begin: BeginHandler) -> bool {
        let room = self.observer_room();
        self.function_observer_slot(function)
            .is_some_and(|slot| slot.add_begin(begin, room))
    }

    /// `zend_observer_add_end_handler` (`Z/Zend/zend_observer.c:216`):
    /// `end` added to `function`'s slot in this request, before the end
    /// handlers it holds, so they stay in the reverse of the begin
    /// handlers' order; as [`Request::add_begin_handler`] otherwise. A
    /// frame of `function` begun while no end handler observed it is not
    /// ended by this one, as php put no frame on its open chain then; a
    /// frame begun with one is ended by every end handler its slot holds
    /// when it is left.
    pub fn add_end_handler(&mut self, function: Function, end: EndHandler) -> bool {
        let room = self.observer_room();
        self.function_observer_slot(function)
            .is_some_and(|slot| slot.add_end(end, room))
    }

    /// `zend_observer_remove_begin_handler` (`Z/Zend/zend_observer.c:201`):
    /// `begin` taken out of `function`'s slot in this request, and the
    /// handler that moved into its place handed back ([`Removed`]); none
    /// where the slot does not hold it. A handler that removes itself
    /// while it is being called calls the one handed back itself, as
    /// php's contract asks, since the walk passes over it.
    pub fn remove_begin_handler(
        &mut self,
        function: Function,
        begin: BeginHandler,
    ) -> Option<Removed<BeginHandler>> {
        self.function_observer_slot(function)?.remove_begin(begin)
    }

    /// `zend_observer_remove_end_handler` (`Z/Zend/zend_observer.c:232`):
    /// as [`Request::remove_begin_handler`], over the end handlers.
    pub fn remove_end_handler(
        &mut self,
        function: Function,
        end: EndHandler,
    ) -> Option<Removed<EndHandler>> {
        self.function_observer_slot(function)?.remove_end(end)
    }

    /// How many handlers of each kind one slot has room for: one per
    /// observer declared, `zend_observers_fcall_list.count`.
    fn observer_room(&self) -> usize {
        usize::try_from(self.shared.observers).unwrap_or(usize::MAX)
    }

    /// `ZEND_OBSERVER_DATA(function)`: the slot in the runtime cache of
    /// the op array a function of the program serves, none where the
    /// cache is not laid down yet or the function is an extension's.
    fn function_observer_slot(&mut self, function: Function) -> Option<&mut Slot> {
        let user = self.user_function(function)?;
        let (unit, body) = (std::sync::Arc::clone(&user.unit), user.body);
        let code = exec::body_of(&unit, body)?;
        exec::observer_slot_of(&mut self.map, code)
    }
}
