//! The observer's state on a request (design §4.3 seam 9,
//! [`crate::observer`]): the chain of frames begun with an end handler
//! and not yet ended, php's `EG(current_observed_frame)`, which is
//! [`Observing`]'s and whose field no file but this one reads; and the
//! observer slot of the op array a frame serves, which stands in the
//! request's map table beside the runtime cache's other slots.

use php_heap::FrameRef;

use crate::observer::Slot;

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
