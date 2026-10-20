//! What the loop holds across the frames it serves (design §5.8): a
//! share of every unit it entered while it serves ([`Units`]), and the
//! op array of each frame it entered and of each function it called
//! ([`Kept`]), so a call and a return take the op array up without
//! asking the function table, as php's `DO_UCALL` and
//! `zend_leave_helper` take `EX(func)` and `LOAD_OPLINE()`.
//!
//! php's `EX(func)` is a pointer the function table keeps alive; the
//! function table holds the unit here too, but the loop reads the op
//! array while it lends the request to a handler, so it keeps its own
//! share of the unit the op array stands in, one share per unit entered
//! and none per call. A share is never given back while the loop
//! serves, so the op arrays the loop keeps are borrowed from the shares
//! it holds, and a call or a return reads one with a load and a compare.

use std::cell::{Cell, OnceCell};

use php_heap::FrameRef;
use php_unit::{OpArray, PublishedUnit};

use crate::api::call::Function;
use crate::api::request::Request;
use crate::exec;

/// How many units one block of [`Units`] holds a share of.
const BLOCK: usize = 16;

/// How many blocks [`Units`] grows to: a serving whose frames enter more
/// units than all of them hold lets every share go and takes the units
/// up afresh ([`Entered::Full`]), so what it holds stays bounded.
const BLOCKS: usize = 16;

/// How many frames, and how many functions, the loop keeps the op array
/// of, one place per number modulo this: a return to a frame, or a call
/// to a function, whose place another took since is a lookup again,
/// which a recursion deeper than this pays once per return.
const KEPT: usize = 16;

/// The units the loop holds a share of while it serves, each kept from
/// the first frame that entered it until the loop lets them all go:
/// added to and never changed while a serving borrows them, so what is
/// borrowed from one stands as long as the loop holds it. A block is laid down where the one before
/// it is full. At most one is a file's unit the request compiled for
/// itself ([`Request::took_unit`]): a frame of a second lets them all go
/// ([`Entered::Full`]), and each is given back to the request
/// ([`Units::give_back`]), so an `eval` in a loop is compiled in the room
/// of the one before it.
#[derive(Default)]
pub(super) struct Units {
    blocks: [OnceCell<Box<[OnceCell<PublishedUnit>; BLOCK]>>; BLOCKS],
    len: Cell<usize>,
    /// Whether a unit the request compiled for itself is among them.
    took: Cell<bool>,
}

/// A frame the loop enters afresh, as [`Units::enter`] found it: its op
/// array, out of a held unit, or no place for its unit until the units
/// held are let go.
pub(super) enum Entered<'u> {
    Held(&'u OpArray),
    Full,
}

/// The op arrays of the frames the loop entered and of the functions it
/// called, out of the held [`Units`], each at its number modulo
/// [`KEPT`]; a place no number holds yet holds [`Kept::NONE`].
pub(super) struct Kept<'u> {
    frames: [(u32, Option<&'u OpArray>); KEPT],
    functions: [(u32, Option<&'u OpArray>); KEPT],
}

impl Default for Kept<'_> {
    fn default() -> Self {
        Kept {
            frames: [(Kept::NONE, None); KEPT],
            functions: [(Kept::NONE, None); KEPT],
        }
    }
}

impl Units {
    /// `frame` entered afresh: the op array it serves, its unit held, a
    /// function's found through the function table and a file's through
    /// the frame's annex ([`Request::unit_of`]), and kept for the return
    /// to the frame and for the next call to its function. None for a
    /// frame that stands nowhere or serves nothing.
    #[inline(never)]
    pub(super) fn enter<'u>(
        &'u self,
        req: &Request,
        kept: &mut Kept<'u>,
        frame: FrameRef,
    ) -> Option<Entered<'u>> {
        let header = req.vm().at(frame)?;
        let (unit, body) = req.unit_of(header)?;
        // A file's frame whose unit the request compiled for itself: an
        // `eval`'s, or a file no cache keeps. A function's unit is held by
        // the function table as long as the request is.
        let took = header.function.is_none() && req.took_unit(unit);
        let Some(held) = self.hold(unit, took) else {
            return Some(Entered::Full);
        };
        let code = exec::body_of(held, body)?;
        kept.keep(frame, code);
        if let Some(function) = header.function {
            kept.keep_called(function, code);
        }
        Some(Entered::Held(code))
    }

    /// The share of `unit` the loop holds, taken now where it holds none;
    /// none where every place is taken, or where `unit` is a second one
    /// the request compiled for itself (`took`).
    fn hold(&self, unit: &PublishedUnit, took: bool) -> Option<&PublishedUnit> {
        let len = self.len.get();
        // The unit entered last first: a program's frames mostly enter
        // the units they entered lately.
        let found = (0..len)
            .rev()
            .find_map(|at| self.at(at).filter(|held| PublishedUnit::ptr_eq(held, unit)));
        if found.is_some() {
            return found;
        }
        if took && self.took.get() {
            return None;
        }
        let block = self
            .blocks
            .get(len / BLOCK)?
            .get_or_init(|| Box::new(std::array::from_fn(|_| OnceCell::new())));
        let held = block
            .get(len % BLOCK)?
            .get_or_init(|| PublishedUnit::clone(unit));
        self.len.set(len + 1);
        self.took.set(self.took.get() || took);
        Some(held)
    }

    /// Every share given back to the request, the first taken first
    /// ([`Request::give_back`]), which keeps the room of a unit nothing
    /// else holds for its next compile, as php frees an `eval`'s op array
    /// to the request's heap. The places stay laid down for the next
    /// serving, which an `eval` in a loop starts once per `eval`, so it
    /// lays down no block of its own. It takes `&mut self`: the serving
    /// that held the units borrowed them shared for `'u`, and its
    /// [`Kept`] is a `Kept<'u>`, so no op array the loop kept outlives
    /// the share it was borrowed from, and the next serving starts with
    /// a `Kept` of its own.
    pub(super) fn give_back(&mut self, req: &mut Request) {
        for at in 0..self.len.replace(0) {
            let share = (self.blocks.get_mut(at / BLOCK))
                .and_then(OnceCell::get_mut)
                .and_then(|block| block.get_mut(at % BLOCK))
                .and_then(OnceCell::take);
            if let Some(share) = share {
                req.give_back(share);
            }
        }
        self.took.set(false);
    }

    /// The unit held at `at`.
    fn at(&self, at: usize) -> Option<&PublishedUnit> {
        self.blocks.get(at / BLOCK)?.get()?.get(at % BLOCK)?.get()
    }
}

impl<'u> Kept<'u> {
    /// The number no frame and no function has, which an empty place
    /// holds.
    const NONE: u32 = u32::MAX;

    /// The op array of a callee of `function`, pushed by a call to a
    /// function the loop has called before; none otherwise, and the loop
    /// enters it.
    #[inline]
    pub(super) fn called(&self, function: Function) -> Option<&'u OpArray> {
        let (kept, code) = *self.functions.get(function.0 as usize % KEPT)?;
        if kept == function.0 { code } else { None }
    }

    /// The op array of `frame`, returned to from a callee, as the loop
    /// entered it; none for a frame the loop did not enter last at its
    /// number, which it enters again.
    #[inline]
    pub(super) fn returned(&self, frame: FrameRef) -> Option<&'u OpArray> {
        let (entered, code) = *self.frames.get(frame.0 as usize % KEPT)?;
        if entered == frame.0 { code } else { None }
    }

    /// `frame` entered, serving `code`: kept for the return to it. A
    /// frame stands at its number until it is popped, and the frame
    /// pushed at that number after it is entered before anything it
    /// calls returns to it, so a frame's own entry is the one a return
    /// to its number reads.
    #[inline]
    pub(super) fn keep(&mut self, frame: FrameRef, code: &'u OpArray) {
        if let Some(entered) = self.frames.get_mut(frame.0 as usize % KEPT) {
            *entered = (frame.0, Some(code));
        }
    }

    /// `function`, called and serving `code`: kept for the next call.
    fn keep_called(&mut self, function: Function, code: &'u OpArray) {
        if let Some(called) = self.functions.get_mut(function.0 as usize % KEPT) {
            *called = (function.0, Some(code));
        }
    }

    /// Every frame the loop entered forgotten, where it goes on along an
    /// edge a tier may have answered (a `Same`, or a `Threw` the tier's
    /// `Resume` raised), which may have entered frames the loop did not:
    /// each is entered again on a return to it.
    pub(super) fn forget(&mut self) {
        self.frames = [(Kept::NONE, None); KEPT];
    }
}
