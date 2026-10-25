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
//!
//! A file's unit the request compiled for itself, an `eval`'s or an
//! uncached include's, is not held so: php frees its op array at its
//! frame's return (`destroy_op_array` in `zend_leave_helper`), and the
//! next `eval` is compiled in its room. So the loop serves such a frame,
//! and what it calls, in a [`Scope`] of its own, which holds the one
//! share of that unit, keeps what it enters apart from what the loop
//! kept around it, and is over at the first edge that leaves the frame:
//! the share is given back there, and the loop around it goes on with
//! the op arrays it kept, as php's `zend_leave_helper` goes on in the
//! caller's `EX(func)`.

use std::cell::{Cell, OnceCell};

use php_compile::Reentered;
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

/// What the first [`Scope`] of a request is charged of the thread's
/// stack, before it is measured ([`Scope::measured`]): php's limit
/// moves this far below it ([`php_compile::StackLimit::nested`]), so a
/// scope costs a native's recursion below it none of php's budget. An
/// optimised loop's frame is about 1,800 bytes (#3051); an unoptimised
/// one's, with every handler in line, about 450 KiB.
const FIRST_SCOPE_CHARGE: usize = if cfg!(debug_assertions) {
    512 * 1024
} else {
    4 * 1024
};

/// The stack a frame served without a scope still has above the thread's
/// floor where the loop stops nesting scopes: room for the compile of
/// the next `eval`, which php's limit, at the floor there, holds.
const SCOPE_ROOM: usize = 256 * 1024;

/// How many frames, and how many functions, the loop keeps the op array
/// of, one place per number modulo this: a return to a frame, or a call
/// to a function, whose place another took since is a lookup again,
/// which a recursion deeper than this pays once per return.
const KEPT: usize = 16;

/// The units the loop holds a share of while it serves, each kept from
/// the first frame that entered it until the loop lets them all go:
/// added to and never changed while a serving borrows them, so what is
/// borrowed from one stands as long as the loop holds it. A block is
/// laid down where the one before it is full. A file's unit the request
/// compiled for itself ([`Request::took_unit`]) is held by the [`Scope`]
/// its frame is served in, not here, unless the machine stack has no
/// room for one more scope: then at most one such unit is held here, a
/// frame of a second lets them all go ([`Entered::Full`]), and each is
/// given back to the request ([`Units::give_back`]).
#[derive(Default)]
pub(super) struct Units {
    blocks: [OnceCell<Box<[OnceCell<PublishedUnit>; BLOCK]>>; BLOCKS],
    len: Cell<usize>,
    /// Whether a unit the request compiled for itself is among them.
    took: Cell<bool>,
}

/// How many [`Units`] a request keeps for its next servings
/// ([`UnitsLeft`]): one per serving standing at once, up to this, which a
/// native calling back into PHP from inside a callback nests.
const LEFT: usize = 16;

/// The [`Units`] a request's servings left, each laid down once and
/// taken up again by the next serving, as php's `execute_ex` lays
/// nothing down to serve a callback a native makes
/// (`zend_call_function`): a serving takes one with its blocks already
/// laid and the shares it held still held ([`UnitsLeft::take`],
/// [`Units::leave`]) and hands it back after ([`UnitsLeft::put`]), so a
/// callback's serving allocates nothing and takes no share of a unit
/// held before.
#[derive(Default)]
pub(crate) struct UnitsLeft(Vec<Units>);

impl UnitsLeft {
    /// The [`Units`] a serving left, where one is kept; an empty one
    /// otherwise, moved onto the serving's own stack, where the loop
    /// reads it as it read a [`Units`] it laid itself.
    #[inline]
    pub(super) fn take(&mut self) -> Units {
        self.0.pop().unwrap_or_default()
    }

    /// `units`, left by its serving ([`Units::leave`]), kept for the next
    /// serving while fewer than [`LEFT`] are; past that its shares go
    /// with it.
    #[inline]
    pub(super) fn put(&mut self, units: Units) {
        if self.0.len() < LEFT {
            self.0.push(units);
        }
    }
}

/// A frame the loop enters afresh, as [`Units::enter`] found it: its op
/// array, out of a held unit or the scope's own; a share of a file's
/// unit the request compiled for itself, which the frame is served in a
/// scope of its own over ([`Scope::took`]); or no place for its unit
/// until the units held are let go.
pub(super) enum Entered<'u> {
    Held(&'u OpArray),
    Took(PublishedUnit, Reentered),
    Full,
}

/// The frames one serving of the loop serves: every frame of the
/// serving, or a frame whose unit the request compiled for itself and
/// the frames it calls, while it stands. A frame pushed later stands at
/// a higher number (`FrameRef` is the position on the stack), so an
/// edge to a frame below the scope's own has left it.
#[derive(Clone, Copy)]
pub(super) enum Scope<'u> {
    /// Every frame of the serving, its units held in [`Units`].
    Whole,
    /// A scope of its own ([`Scope::took`]).
    Took {
        /// The frame the scope serves first, the lowest it serves.
        base: FrameRef,
        /// The share of the base frame's unit the scope holds.
        unit: &'u PublishedUnit,
        /// Where the stack stood at the call that began the scope.
        above: usize,
    },
}

/// Where the stack stands: the address of a local of the calling frame,
/// as php's `zend_call_stack_position` reads it.
#[inline]
pub(super) fn here() -> usize {
    let here = 0u8;
    std::ptr::addr_of!(here).addr()
}

impl<'u> Scope<'u> {
    /// `frame` and what it calls, its unit `unit`, held for the scope,
    /// begun by a call that stood at `above` on the stack.
    pub(super) fn took(frame: FrameRef, unit: &'u PublishedUnit, above: usize) -> Scope<'u> {
        Scope::Took {
            base: frame,
            unit,
            above,
        }
    }

    /// A scope's body begun: where it is the request's first, what it
    /// took of the stack since the call that began it noted as every
    /// next scope's charge ([`Request::note_scope_cost`]). Nothing for
    /// the whole serving.
    #[inline]
    pub(super) fn measured(self, req: &mut Request) {
        if let Scope::Took { above, .. } = self
            && let Some(cost) = req.scope_unmeasured().then(|| above.saturating_sub(here()))
            && let Some(cost) = std::num::NonZeroUsize::new(cost)
        {
            req.note_scope_cost(cost);
        }
    }

    /// The op array of `frame` where it is the scope's own, a file's
    /// whose unit the scope holds, kept for the return to it; none for
    /// any other, which [`Units::enter`] enters.
    #[inline]
    pub(super) fn own(self, kept: &mut Kept<'u>, frame: FrameRef) -> Option<&'u OpArray> {
        let Scope::Took { base, unit, .. } = self else {
            return None;
        };
        if frame != base {
            return None;
        }
        let code = exec::code_of(unit);
        kept.keep(frame, code);
        Some(code)
    }

    /// Whether an edge to `frame` leaves the scope: a frame below its
    /// own, which the scope around it serves.
    #[inline]
    pub(super) fn left(self, frame: FrameRef) -> bool {
        match self {
            Scope::Whole => false,
            Scope::Took { base, .. } => frame.0 < base.0,
        }
    }
}

/// The op arrays of the frames a serving entered and of the functions it
/// called, out of the held [`Units`] or its [`Scope`]'s share, each at
/// its number modulo [`KEPT`].
#[derive(Default)]
pub(super) struct Kept<'u> {
    frames: Places<'u>,
    functions: Places<'u>,
}

/// [`KEPT`] places, each a number and the op array kept for it: the
/// numbers apart from the op arrays, so that a new one, and one
/// forgotten, is written as zeroes end to end with no padding between,
/// as a scope lays one down for each `eval` it serves. A place that
/// keeps no op array keeps nothing, whatever its number.
#[derive(Default)]
struct Places<'u> {
    numbers: [u32; KEPT],
    codes: [Option<&'u OpArray>; KEPT],
}

impl<'u> Places<'u> {
    /// The op array kept for `number`, where its place keeps one for it.
    #[inline]
    fn get(&self, number: u32) -> Option<&'u OpArray> {
        let at = number as usize % KEPT;
        if *self.numbers.get(at)? == number {
            *self.codes.get(at)?
        } else {
            None
        }
    }

    /// `code` kept for `number`, in the place of whatever its place kept.
    #[inline]
    fn set(&mut self, number: u32, code: &'u OpArray) {
        let at = number as usize % KEPT;
        if let (Some(kept), Some(place)) = (self.numbers.get_mut(at), self.codes.get_mut(at)) {
            *kept = number;
            *place = Some(code);
        }
    }
}

impl Units {
    /// `frame` entered afresh: the op array it serves, its
    /// unit held, a function's found through the function table and a
    /// file's through the frame's annex ([`Request::unit_of`]), and kept
    /// for the return to the frame and for the next call to its
    /// function. A file's frame whose unit the request compiled for
    /// itself is served in a scope of its own ([`Entered::Took`]), where
    /// the machine stack has room for one. None for a frame that stands
    /// nowhere or serves nothing.
    #[inline(never)]
    pub(super) fn enter<'u>(
        &'u self,
        req: &mut Request,
        kept: &mut Kept<'u>,
        frame: FrameRef,
    ) -> Option<Entered<'u>> {
        let header = req.vm().at(frame)?;
        let (unit, body) = req.unit_of(header)?;
        // A file's frame whose unit the request compiled for itself: an
        // `eval`'s, or a file no cache keeps. A function's unit is held by
        // the function table as long as the request is.
        if header.function.is_none() && req.took_unit(unit) {
            let unit = PublishedUnit::clone(unit);
            return match req.stack_nested(FIRST_SCOPE_CHARGE, SCOPE_ROOM) {
                Some(moved) => Some(Entered::Took(unit, moved)),
                None => self.enter_held(kept, frame, &unit, (body, None), true),
            };
        }
        self.enter_held(kept, frame, unit, (body, header.function), false)
    }

    /// `frame`, serving `body` of `unit`, entered with its unit held
    /// here; `took` where the request compiled the unit for itself.
    fn enter_held<'u>(
        &'u self,
        kept: &mut Kept<'u>,
        frame: FrameRef,
        unit: &PublishedUnit,
        (body, function): (exec::Body, Option<Function>),
        took: bool,
    ) -> Option<Entered<'u>> {
        let Some(held) = self.hold(unit, took) else {
            return Some(Entered::Full);
        };
        let code = exec::body_of(held, body)?;
        kept.keep(frame, code);
        if let Some(function) = function {
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

    /// The serving over: where a unit the request compiled for itself is
    /// among the units held, or more than one block of them, every share
    /// given back ([`Units::give_back`]); otherwise each share kept for
    /// the next serving, since a function's unit and a cached file's are
    /// held by the request as long as it runs, so the next callback a
    /// native serves finds its unit held and takes no share of it, and
    /// [`Units::hold`]'s search of the units held walks one block at
    /// most.
    pub(super) fn leave(&mut self, req: &mut Request) {
        if self.took.get() || self.len.get() > BLOCK {
            self.give_back(req);
        }
    }

    /// The unit held at `at`.
    fn at(&self, at: usize) -> Option<&PublishedUnit> {
        self.blocks.get(at / BLOCK)?.get()?.get(at % BLOCK)?.get()
    }
}

impl<'u> Kept<'u> {
    /// The op array of a callee of `function`, pushed by a call to a
    /// function the loop has called before; none otherwise, and the loop
    /// enters it.
    #[inline]
    pub(super) fn called(&self, function: Function) -> Option<&'u OpArray> {
        self.functions.get(function.0)
    }

    /// The op array of `frame`, returned to from a callee, as the loop
    /// entered it; none for a frame the loop did not enter last at its
    /// number, which it enters again.
    #[inline]
    pub(super) fn returned(&self, frame: FrameRef) -> Option<&'u OpArray> {
        self.frames.get(frame.0)
    }

    /// `frame` entered, serving `code`: kept for the return to it. A
    /// frame stands at its number until it is popped, and the frame
    /// pushed at that number after it is entered before anything it
    /// calls returns to it, so a frame's own entry is the one a return
    /// to its number reads.
    #[inline]
    pub(super) fn keep(&mut self, frame: FrameRef, code: &'u OpArray) {
        self.frames.set(frame.0, code);
    }

    /// `function`, called and serving `code`: kept for the next call.
    fn keep_called(&mut self, function: Function, code: &'u OpArray) {
        self.functions.set(function.0, code);
    }

    /// Every frame the loop entered forgotten, where it goes on along an
    /// edge a tier may have answered (a `Same`, or a `Threw` the tier's
    /// `Resume` raised), which may have entered frames the loop did not:
    /// each is entered again on a return to it.
    pub(super) fn forget(&mut self) {
        self.frames = Places::default();
    }
}
