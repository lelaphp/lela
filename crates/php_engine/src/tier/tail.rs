//! The dispatch threaded, as php's tailcall VM threads it (design §5.8,
//! `cfg(dispatch = "tailcall")`): each handler a function of its own
//! that, where its answer goes on in the frame, finds the next opline
//! and becomes that opline's handler, `ZEND_VM_DISPATCH_TO_HANDLER` as a
//! tail call, so the loop's fetch and answer are each handler's tail and
//! no arm meets another. The chain comes back to [`super::serve_frame`]
//! only with an edge of the frame chain, which the loop takes as it
//! takes one from the `match`; the safepoints are where they are, in
//! the handlers, and the frames the compiled tier shares are the same
//! (ADR-0006).
//!
//! Rust's explicit tail calls (`become`) are nightly's alone, so this is
//! the variant the design leaves to be measured there, and the `match`
//! loop is the one shipped.

use std::ops::ControlFlow;

use php_heap::{Fatal, FrameRef};
use php_unit::{Handler, OpArray, Opline};

use super::dispatch::tails::{HANDLERS, Tails};
use super::{Next, Tier, Way, past_the_end};
use crate::api::request::Request;
use crate::observer::Telling;

/// A handler threaded: the function that serves the opline it is handed
/// at `at` and becomes the handler of the opline its answer names, or
/// hands back an answer that leaves the frame. It saves no register of
/// its caller's, as php's tailcall VM's handlers are `preserve_none`, so
/// what the chain carries stays in the registers it is passed in across
/// the calls a handler makes, and a handler that makes none saves
/// nothing. `oplines` is `code`'s, handed down beside it so that finding
/// the next opline loads neither their base nor their length, and the
/// table is handed down too, so that finding its handler is one load.
pub(crate) type Tail<D> = extern "rust-preserve-none" fn(
    &mut Request,
    FrameRef,
    &OpArray,
    &[Opline],
    &Opline,
    u32,
    &D,
    &Table<D>,
) -> Result<Next, Fatal>;

/// Every handler threaded, by its `php_unit::Handler`.
pub(crate) struct Table<D>(pub(crate) [Tail<D>; HANDLERS]);

impl<D> Table<D> {
    /// The function `handler` is served by.
    #[inline(always)] // each handler's tail
    #[allow(clippy::inline_always)]
    pub(crate) fn of(&self, handler: Handler) -> Tail<D> {
        self.0[handler as usize]
    }
}

/// The chain from the opline at `at`: its handler called, and the answer
/// the last handler of the chain left the frame with, which is always
/// [`Way::Left`], since every other answer goes on in the chain.
#[inline]
pub(super) fn chain<T: Tier, D: Telling>(
    req: &mut Request,
    frame: FrameRef,
    code: &OpArray,
    oplines: &[Opline],
    at: u32,
    told: &D,
) -> Result<Next, Fatal> {
    let Some(op) = oplines.get(at as usize) else {
        return Err(past_the_end(frame, at));
    };
    told.before(req, frame, at, op.opcode);
    let table = Tails::<T, D>::TABLE;
    (table.of(op.handler))(req, frame, code, oplines, op, at, told, table)
}

/// Where a handler's answer sends the chain: on at the opline it names,
/// told to the observers of a declared opcode as the loop tells them,
/// or out of the chain with the answer, which leaves the frame.
#[inline(always)] // each handler's tail
#[allow(clippy::inline_always)]
pub(crate) fn onward<'a, D: Telling>(
    req: &mut Request,
    frame: FrameRef,
    oplines: &'a [Opline],
    at: u32,
    next: Next,
    told: &D,
) -> Result<ControlFlow<Next, (&'a Opline, u32)>, Fatal> {
    // The way on tested first, as a branch and not a select, so that a
    // handler whose answer is known on its path goes on from that path:
    // as the loop's, the add does not wrap.
    let to = if next == Next::ADVANCE {
        at.wrapping_add(1)
    } else {
        match next.way() {
            Way::On(places) => at.wrapping_add(places),
            Way::Jump(to) => to,
            Way::Left(_) => return Ok(ControlFlow::Break(next)),
        }
    };
    let Some(op) = oplines.get(to as usize) else {
        return Err(past_the_end(frame, to));
    };
    told.before(req, frame, to, op.opcode);
    Ok(ControlFlow::Continue((op, to)))
}
