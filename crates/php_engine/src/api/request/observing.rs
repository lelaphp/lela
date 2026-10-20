//! The observer's state on a request (design §4.3 seam 9,
//! [`crate::observer`]): the chain of frames and natives' calls begun
//! with an end handler and not yet ended, php's
//! `EG(current_observed_frame)`; the natives' observer slots, php's
//! internal runtime cache; and the number the next native's call
//! begun takes. All three are [`Observing`]'s, whose fields no file but
//! this one reads. An op array's observer slot stands in the request's
//! map table beside the runtime cache's other slots.
//!
//! The chain, [`ObservedChain`], is the stack's the request is served
//! on: a fiber's switch swaps it with the rest of the execution state
//! ([`Request::switch_execution`]), as php saves it into the
//! `zend_fiber_context` it leaves (`top_observed_frame`), and a serving
//! with nothing executing sets it aside with the frames, so its frames
//! are always of the one stack whose frames are pushed and popped beside
//! it, and a frame is never ended in place of another stack's at the
//! same height. A native's call on it is one of that stack's too: the
//! `Fiber::suspend` a fiber is suspended in is ended in the fiber, and
//! the `Fiber::start` that switched into it on its resumer's stack. The
//! natives' slots and the count of their calls are the request's.
//!
//! The handlers added to and removed from one function's slot while the
//! request is served, php's `zend_observer_add_begin_handler` and its
//! kin, write the request's slot and never the published unit; the slot
//! is the function's and not a stack's, so a fiber's switch leaves it.

use crate::api::call::Function;
use crate::exec;
use crate::observer::{BeginHandler, EndHandler, Left, Observable, Removed, Slot};

use super::Code;
use super::Request;

/// The open observed calls of one stack, the innermost last: php's
/// `EG(current_observed_frame)` and each frame's `prev_observed_frame`,
/// and a suspended fiber's `top_observed_frame`.
#[derive(Debug, Default)]
pub(crate) struct ObservedChain(Vec<OpenCall>);

/// One call on the open chain: one begun and standing, or a frame a
/// fatal left, kept as it stood ([`crate::observer::bailed_out`]).
#[derive(Debug)]
pub(crate) enum OpenCall {
    Call(Observable),
    Left(Box<Left>),
}

/// The observer's request state.
#[derive(Debug, Default)]
pub(super) struct Observing {
    /// The open observed calls of the stack the request is served on.
    open: ObservedChain,
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

    /// The begin handler at `next` of that slot as it stands now, read
    /// where it stands ([`Slot::begin_at`]).
    pub(crate) fn observer_begin_at(&self, at: Observable, next: usize) -> Option<BeginHandler> {
        self.observer_slot_held(at)?.begin_at(next)
    }

    /// The end handler at `next` of that slot as it stands now.
    pub(crate) fn observer_end_at(&self, at: Observable, next: usize) -> Option<EndHandler> {
        self.observer_slot_held(at)?.end_at(next)
    }

    /// [`Request::observer_slot`] borrowed where it stands.
    fn observer_slot_held(&self, at: Observable) -> Option<&Slot> {
        match at {
            Observable::Code(frame) => self.vm.observer_slot_held(&self.map, frame),
            Observable::Native(call) => self
                .native_place(call.function)
                .and_then(|place| self.observing.natives.get(place)),
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

    /// Whether the observers were asked of the native `function` and
    /// none answered with a handler, php's slot holding
    /// `ZEND_OBSERVER_NONE_OBSERVED`: false while none was asked yet, as
    /// `zend_observer_handler_is_unobserved` reads an unresolved slot.
    pub(crate) fn native_unobserved(&self, function: Function) -> bool {
        self.native_place(function)
            .and_then(|place| self.observing.natives.get(place))
            .is_some_and(|slot| matches!(slot, Slot::Declined))
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
        self.observing.open.0.push(OpenCall::Call(at));
    }

    /// `at` taken off the chain where it is the innermost open call, and
    /// whether it was: php's `execute_data == EG(current_observed_frame)`.
    /// A frame a fatal left is `at` where `at` is that very frame,
    /// still standing, and never one pushed since at the same place.
    pub(crate) fn close_observed(&mut self, at: Observable) -> bool {
        let innermost = match (self.observing.open.0.last(), at) {
            (Some(OpenCall::Call(open)), _) => *open == at,
            (Some(OpenCall::Left(left)), Observable::Code(frame)) => {
                self.frame_view(frame).id() == left.frame
            }
            (Some(OpenCall::Left(_)), Observable::Native(_)) | (None, _) => false,
        };
        if innermost {
            self.observing.open.0.pop();
        }
        innermost
    }

    /// Every frame on the chain kept as it stands, for a fatal that
    /// pops them before they are ended; a native's call is kept by its
    /// number already.
    pub(crate) fn keep_observed(&mut self) {
        let mut open = std::mem::take(&mut self.observing.open.0);
        for opened in &mut open {
            if let OpenCall::Call(Observable::Code(frame)) = *opened {
                *opened = OpenCall::Left(Box::new(Left::of(self, frame)));
            }
        }
        self.observing.open.0 = open;
    }

    /// The chain the request is served on and `other` change places, as
    /// a switch changes stacks: `zend_observer_fiber_switch_notify`'s
    /// `from->top_observed_frame = EG(current_observed_frame);
    /// EG(current_observed_frame) = to->top_observed_frame;`.
    pub(crate) fn swap_observing(&mut self, other: &mut ObservedChain) {
        std::mem::swap(&mut self.observing.open, other);
    }

    /// Every open call, the innermost first, the chain left empty:
    /// `zend_observer_fcall_end_all`'s walk.
    pub(crate) fn take_observed(&mut self) -> Vec<OpenCall> {
        let mut open = std::mem::take(&mut self.observing.open.0);
        open.reverse();
        open
    }
}

impl Request {
    /// How many handlers of each kind one slot has room for: one per
    /// observer declared, `zend_observers_fcall_list.count`.
    fn observer_room(&self) -> usize {
        usize::try_from(self.shared.observers).unwrap_or(usize::MAX)
    }

    /// `ZEND_OBSERVER_DATA(function)`: a native's slot in the request's
    /// table of natives' slots, or the slot in the runtime cache of the
    /// op array a function of the program serves; none where the table
    /// or the cache is not laid down that far yet.
    fn function_observer_slot(&mut self, function: Function) -> Option<&mut Slot> {
        if let Some(place) = self.native_place(function) {
            return self.observing.natives.get_mut(place);
        }
        let cache = self.user_function(function)?.shape.cache;
        exec::observer_slot_of(&mut self.map, cache)
    }
}

impl Code<&mut Request> {
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
    /// op array or in the request's table of natives' slots, so what one
    /// request adds no other request sees, and the unit the function was
    /// compiled into, shared by every request, is not written.
    ///
    /// [`ExtensionBuilder::observer`]: crate::api::abi::ExtensionBuilder::observer
    pub fn add_begin_handler(self, function: Function, begin: BeginHandler) -> bool {
        let room = self.0.observer_room();
        self.0
            .function_observer_slot(function)
            .is_some_and(|slot| slot.add_begin(begin, room))
    }

    /// `zend_observer_add_end_handler` (`Z/Zend/zend_observer.c:216`):
    /// `end` added to `function`'s slot in this request, before the end
    /// handlers it holds, so they stay in the reverse of the begin
    /// handlers' order; as [`Code::add_begin_handler`] otherwise. A
    /// frame of `function` begun while no end handler observed it is not
    /// ended by this one, as php put no frame on its open chain then; a
    /// frame begun with one is ended by every end handler its slot holds
    /// when it is left.
    pub fn add_end_handler(self, function: Function, end: EndHandler) -> bool {
        let room = self.0.observer_room();
        self.0
            .function_observer_slot(function)
            .is_some_and(|slot| slot.add_end(end, room))
    }

    /// `zend_observer_remove_begin_handler` (`Z/Zend/zend_observer.c:201`):
    /// `begin` taken out of `function`'s slot in this request, and the
    /// handler that moved into its place handed back ([`Removed`]); none
    /// where the slot does not hold it. A handler that removes itself
    /// while it is being called calls the one handed back itself, as
    /// php's contract asks, since the walk passes over it.
    pub fn remove_begin_handler(
        self,
        function: Function,
        begin: BeginHandler,
    ) -> Option<Removed<BeginHandler>> {
        self.0.function_observer_slot(function)?.remove_begin(begin)
    }

    /// `zend_observer_remove_end_handler` (`Z/Zend/zend_observer.c:232`):
    /// as [`Code::remove_begin_handler`], over the end handlers.
    pub fn remove_end_handler(
        self,
        function: Function,
        end: EndHandler,
    ) -> Option<Removed<EndHandler>> {
        self.0.function_observer_slot(function)?.remove_end(end)
    }
}
