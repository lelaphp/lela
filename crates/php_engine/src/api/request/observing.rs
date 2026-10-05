//! The observer's state on a request (design §4.3 seam 9,
//! [`crate::observer`]): the chain of frames and natives' calls begun
//! with an end handler and not yet ended, php's
//! `EG(current_observed_frame)`; the natives' observer slots, php's
//! internal runtime cache; and the number the next native's call
//! begun takes. All three are [`Observing`]'s, whose fields no file but
//! this one reads. An op array's observer slot stands in the request's
//! map table beside the runtime cache's other slots.

use crate::api::call::Function;
use crate::observer::{Observable, Slot};

use super::Request;

/// The observer's request state.
#[derive(Debug, Default)]
pub(super) struct Observing {
    /// The open observed calls, the innermost last: php's
    /// `EG(current_observed_frame)` and each frame's
    /// `prev_observed_frame`.
    open: Vec<Observable>,
    /// Each native's observer slot at its place among the natives
    /// ([`Request::native_place`]), php's internal runtime cache,
    /// which `zend_reset_internal_run_time_cache` empties as each request
    /// starts (`Z/Zend/zend.c:1329`): laid down as long as the place the
    /// first native observed asks for, and grown on demand, so a request
    /// with no observer declared lays nothing down.
    natives: Vec<Slot>,
    /// How many natives' calls were begun, the number the next one
    /// takes ([`crate::observer::NativeCall`]).
    natives_begun: u64,
}

impl Request {
    /// `ZEND_OBSERVER_DATA(EX(func))`: the observer slot of the op array
    /// a frame serves ([`exec::Vm::observer_slot`]), or of a native.
    ///
    /// [`exec::Vm::observer_slot`]: crate::exec::Vm::observer_slot
    pub(crate) fn observer_slot(&self, at: Observable) -> Slot {
        match at {
            Observable::Code(frame) => self.vm.observer_slot(&self.map, frame),
            Observable::Native(call) => self
                .native_place(call.function)
                .and_then(|place| self.observing.natives.get(place))
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// That slot resolved to `slot` (`zend_observer_fcall_install`).
    pub(crate) fn set_observer_slot(&mut self, at: Observable, slot: Slot) {
        match at {
            Observable::Code(frame) => self.vm.set_observer_slot(&mut self.map, frame, slot),
            Observable::Native(call) => {
                let Some(place) = self.native_place(call.function) else {
                    return;
                };
                let natives = &mut self.observing.natives;
                if natives.len() <= place {
                    natives.resize(place.saturating_add(1), Slot::Unresolved);
                }
                if let Some(held) = natives.get_mut(place) {
                    *held = slot;
                }
            }
        }
    }

    /// Whether `function` is a native with an observer slot: a native
    /// function or method, or an enum's `cases`, `from` or `tryFrom`;
    /// not a call's trampoline, nor `zend_pass_function`, which php
    /// gives no runtime cache.
    pub(crate) fn has_native_slot(&self, function: Function) -> bool {
        self.native_place(function).is_some()
    }

    /// The number of a native's call being begun: one more than the
    /// last one's.
    pub(crate) fn number_native_call(&mut self) -> u64 {
        let number = self.observing.natives_begun;
        self.observing.natives_begun = number.wrapping_add(1);
        number
    }

    /// `at` begun with an end handler: the innermost open call now.
    pub(crate) fn open_observed(&mut self, at: Observable) {
        self.observing.open.push(at);
    }

    /// `at` taken off the chain where it is the innermost open call, and
    /// whether it was: php's `execute_data == EG(current_observed_frame)`.
    pub(crate) fn close_observed(&mut self, at: Observable) -> bool {
        if self.observing.open.last() != Some(&at) {
            return false;
        }
        self.observing.open.pop();
        true
    }

    /// Every open call, the innermost first, the chain left empty:
    /// `zend_observer_fcall_end_all`'s walk.
    pub(crate) fn take_observed(&mut self) -> Vec<Observable> {
        let mut open = std::mem::take(&mut self.observing.open);
        open.reverse();
        open
    }
}
