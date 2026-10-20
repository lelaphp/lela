//! What the loop holds across the frames it serves (design §5.8): a
//! share of the last [`HELD`] units it entered ([`Units`]), and which of
//! those units and which body the op array of each frame it entered and
//! of each function it called is ([`Kept`]), so a call and a return take
//! the op array up without asking the function table, as php's
//! `DO_UCALL` and `zend_leave_helper` take `EX(func)` and
//! `LOAD_OPLINE()`.
//!
//! php's `EX(func)` is a pointer the function table keeps alive; the
//! function table holds the unit here too, but the loop reads the op
//! array while it lends the request to a handler, so it keeps its own
//! share of the unit the op array stands in, one share per unit entered
//! and none per call.

use php_heap::FrameRef;
use php_unit::{OpArray, PublishedUnit};

use crate::api::call::Function;
use crate::api::request::Request;
use crate::exec::{self, Body};

/// How many units the loop holds a share of at once: enough for a
/// program's main file and the files of the functions it calls in turn
/// to take no share per call.
const HELD: usize = 4;

/// How many frames, and how many functions, the loop keeps the op array
/// of, one place per number modulo this: a return to a frame, or a call
/// to a function, whose place another took since is a lookup again,
/// which a recursion deeper than this pays once per return.
const KEPT: usize = 16;

/// One op array the loop holds: the place among the held units of its
/// unit, and its body there.
#[derive(Debug, Clone, Copy)]
pub(super) struct Code {
    place: usize,
    body: Body,
}

/// The units the loop holds a share of. An op array is read out of
/// them while the loop serves it, so they change only where the loop
/// enters a frame afresh.
#[derive(Default)]
pub(super) struct Units {
    units: [Option<PublishedUnit>; HELD],
    oldest: usize,
}

/// The op arrays of the frames the loop entered and of the functions it
/// called, by their place among the held [`Units`].
#[derive(Default)]
pub(super) struct Kept {
    frames: [Option<(FrameRef, Code)>; KEPT],
    functions: [Option<(Function, Code)>; KEPT],
}

impl Units {
    /// `frame` entered afresh: the op array it serves, its unit held, a
    /// function's found through the function table and a file's through
    /// the frame's annex ([`Request::unit_of`]), and kept for the return
    /// to the frame and for the next call to its function. None for a
    /// frame that stands nowhere or serves nothing.
    /// The share a place held is given back to the request
    /// ([`Request::give_back`]), which keeps the tables of a unit nothing
    /// else holds for its next compile.
    #[inline(never)]
    pub(super) fn enter(
        &mut self,
        req: &mut Request,
        kept: &mut Kept,
        frame: FrameRef,
    ) -> Option<Code> {
        let header = req.vm().at(frame)?;
        let function = header.function;
        let (unit, body) = req.unit_of(header)?;
        let found = self.units.iter().position(|held| {
            held.as_ref()
                .is_some_and(|held| PublishedUnit::ptr_eq(held, unit))
        });
        let place = if let Some(place) = found {
            place
        } else {
            let place = self.oldest;
            self.oldest = (self.oldest + 1) % HELD;
            let given_up = self
                .units
                .get_mut(place)?
                .replace(PublishedUnit::clone(unit));
            kept.lost(place);
            if let Some(given_up) = given_up {
                req.give_back(given_up);
            }
            place
        };
        let code = Code { place, body };
        kept.keep(frame, code);
        if let Some(function) = function {
            kept.keep_called(function, code);
        }
        Some(code)
    }

    /// The op array `code` names.
    #[inline]
    pub(super) fn code(&self, code: Code) -> Option<&OpArray> {
        exec::body_of(self.units.get(code.place)?.as_deref()?, code.body)
    }
}

impl Kept {
    /// The op array of a callee of `function`, pushed by a call to a
    /// function the loop has called before; none otherwise, and the loop
    /// enters it.
    #[inline]
    pub(super) fn called(&self, function: Function) -> Option<Code> {
        match *self.functions.get(function.0 as usize % KEPT)? {
            Some((kept, code)) if kept == function => Some(code),
            _ => None,
        }
    }

    /// The op array of `frame`, returned to from a callee, as the loop
    /// entered it; none for a frame the loop did not enter last at its
    /// number, which it enters again.
    #[inline]
    pub(super) fn returned(&self, frame: FrameRef) -> Option<Code> {
        match *self.frames.get(frame.0 as usize % KEPT)? {
            Some((entered, code)) if entered == frame => Some(code),
            _ => None,
        }
    }

    /// `frame` entered, serving `code`: kept for the return to it. A
    /// frame stands at its number until it is popped, and the frame
    /// pushed at that number after it is entered before anything it
    /// calls returns to it, so a frame's own entry is the one a return
    /// to its number reads.
    #[inline]
    pub(super) fn keep(&mut self, frame: FrameRef, code: Code) {
        if let Some(entered) = self.frames.get_mut(frame.0 as usize % KEPT) {
            *entered = Some((frame, code));
        }
    }

    /// `function`, called and serving `code`: kept for the next call.
    fn keep_called(&mut self, function: Function, code: Code) {
        if let Some(called) = self.functions.get_mut(function.0 as usize % KEPT) {
            *called = Some((function, code));
        }
    }

    /// Every frame the loop entered forgotten, where it goes on along an
    /// edge a tier may have answered (a `Same`, or a `Threw` the tier's
    /// `Resume` raised), which may have entered frames the loop did not:
    /// each is entered again on a return to it.
    pub(super) fn forget(&mut self) {
        self.frames = [None; KEPT];
    }

    /// What stood at `place` among the held units is no longer in hand.
    fn lost(&mut self, place: usize) {
        for entered in &mut self.frames {
            if entered.is_some_and(|(_, code)| code.place == place) {
                *entered = None;
            }
        }
        for called in &mut self.functions {
            if called.is_some_and(|(_, code)| code.place == place) {
                *called = None;
            }
        }
    }
}
