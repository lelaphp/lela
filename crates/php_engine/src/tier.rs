//! Seam 8, the tier (design §4.3, `jit-design.md` §2, §3; ADR-0006): the
//! loop asks the tier at three safepoints and nowhere else, through one
//! method bound per engine. `Nop` is the interpreter's own adapter; the
//! compiled tier is `php_jit::Jit`. The word each safepoint carries is
//! two atomics whose bits only the tier interprets.
//!
//! [`execute`] and [`step`] are the loop's two entries: one dispatch over
//! `php_unit::Opcode` (design §5.8), generated from `ops.toml` in this
//! crate's `build.rs` and generic over the tier, so `Nop` folds the
//! safepoint away. `execute` raises `Site::Entry` for the frame it is
//! given, after that frame's prologue; `RETURN` raises `Site::Return` for
//! every other frame and for no entry frame, which the stack's entry slot
//! says.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use php_heap::{Fatal, FrameRef};
use php_unit::Unit;

use crate::api::abi::Extension;
use crate::api::request::Request;
use crate::exec;

/// The number of a safepoint within its op array; zero is the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SafepointIdx(pub u32);

impl SafepointIdx {
    /// The entry safepoint.
    pub const ENTRY: SafepointIdx = SafepointIdx(0);
}

/// Where the loop is asking: a loop header from its backward jump, the
/// activation of a frame after its prologue, or a return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    Loop(SafepointIdx),
    Entry,
    Return(SafepointIdx),
}

impl Site {
    /// The safepoint's number.
    #[must_use]
    pub fn idx(self) -> SafepointIdx {
        match self {
            Site::Loop(i) | Site::Return(i) => i,
            Site::Entry => SafepointIdx::ENTRY,
        }
    }
}

/// What the tier told the loop to do.
#[derive(Debug)]
pub enum Flow {
    /// Nothing happened; the loop continues at the frame's opline.
    Continue,
    /// The tier served code; the loop continues in this frame after the
    /// interrupt and pending-throwable tests.
    Resume(FrameRef),
    /// A fatal was raised inside tier code.
    Bail(Fatal),
}

/// The tier: one method at the safepoints, and the state it declares as
/// an extension would.
pub trait Tier: 'static {
    /// Asked at the three sites and nowhere else.
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow;

    /// The tier's engine state, request state and ini, under seam 3
    /// without being an extension; `None` declares nothing.
    #[must_use]
    fn extension() -> Option<Extension> {
        None
    }
}

/// The interpreter alone: one inlined constant, so the branch on `Flow`
/// folds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Nop;

impl Tier for Nop {
    #[inline(always)] // jit-design.md §2.2: one constant, so the branch on `Flow` folds
    #[allow(clippy::inline_always)]
    fn safepoint(_: &mut Request, _: FrameRef, _: Site) -> Flow {
        Flow::Continue
    }
}

/// One safepoint's word (`jit-design.md` §3.2): two atomics, zero at
/// publication, all ones when sealed.
#[derive(Debug, Default)]
pub struct Word {
    halves: [AtomicU32; 2],
}

impl Word {
    /// The one accessor; the meaning of the bits is the tier's.
    #[must_use]
    pub fn halves(&self) -> &[AtomicU32; 2] {
        &self.halves
    }

    /// A word no tier acts on: every bit set. Allocated for every
    /// safepoint of a request-scoped op array.
    #[must_use]
    pub fn sealed() -> Word {
        Word {
            halves: [AtomicU32::new(u32::MAX), AtomicU32::new(u32::MAX)],
        }
    }
}

/// What one `step` did to the frame chain (`jit-design.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stepped {
    Same(FrameRef),
    Pushed(FrameRef),
    Popped(FrameRef),
    Moved(FrameRef),
    Threw(FrameRef),
    Done,
}

/// The generated dispatch (design §4.3 seam 7): one `match` arm per
/// opcode `ops.toml` names, over the handlers beside it.
mod dispatch {
    use php_heap::{Fatal, FrameRef};
    use php_unit::{OpArray, Opcode, Opline};

    use crate::api::request::Request;
    use crate::tier::{Stepped, Tier};

    include!(concat!(env!("OUT_DIR"), "/dispatch.rs"));
}

/// What a handler does with the flow a safepoint answered. `Continue` is
/// the whole of it while `Nop` is the only adapter; a tier that served
/// code is `php_jit`'s and lands with it (ADR-0006).
pub(crate) fn continued(flow: Flow) -> Result<(), Fatal> {
    match flow {
        Flow::Continue => Ok(()),
        Flow::Bail(fatal) => Err(fatal),
        Flow::Resume(_) => Err(Fatal::Error(String::from(
            "InternalError: a tier served code and this loop has no resumption for it yet",
        ))),
    }
}

/// The dispatch loop over `frame` until the entry frame returns, asking
/// `T` at the safepoints. `Ok(())` exactly when the entry frame has
/// returned; `Err` carries the fatal.
pub fn execute<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    let guard = req.stack_mut().push_entry(frame);
    let outcome = serve::<T>(req, frame);
    req.stack_mut().pop_entry(guard);
    outcome
}

/// The loop itself, with the entry slot standing.
fn serve<T: Tier>(req: &mut Request, entry: FrameRef) -> Result<(), Fatal> {
    let mut frame = entry;
    // The activation of a frame, after its prologue has run without
    // raising; `execute` raises it for its own entry frame whatever the
    // caller is, as php's own counter fires (design §4.3 seam 8).
    continued(T::safepoint(req, frame, Site::Entry))?;
    loop {
        let unit = standing(req, frame)?;
        let code = exec::code_of(&unit);
        let op = *at_opline(req, frame, code)?;
        match dispatch::dispatch::<T>(req, frame, code, &op)? {
            Stepped::Same(at) | Stepped::Moved(at) | Stepped::Popped(at) => frame = at,
            Stepped::Pushed(at) => {
                frame = at;
                continued(T::safepoint(req, frame, Site::Entry))?;
            }
            Stepped::Threw(_) => return req.uncaught(),
            Stepped::Done => return Ok(()),
        }
    }
}

/// Exactly one handler, the one at the frame's opline, with `Nop` at the
/// safepoints; what it did to the frame chain is reported, and a
/// throwable it raised is left pending.
pub fn step(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    let unit = standing(req, frame)?;
    let code = exec::code_of(&unit);
    let op = *at_opline(req, frame, code)?;
    dispatch::dispatch::<Nop>(req, frame, code, &op)
}

/// What `frame` is serving, held by its own share of the unit so the
/// oplines can be read while the request is borrowed.
fn standing(req: &Request, frame: FrameRef) -> Result<Arc<Unit>, Fatal> {
    req.vm().serving(frame).ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: frame {} is serving no op array",
            frame.0
        ))
    })
}

/// The opline `frame` stands at.
fn at_opline<'a>(
    req: &Request,
    frame: FrameRef,
    code: &'a php_unit::OpArray,
) -> Result<&'a php_unit::Opline, Fatal> {
    let at = req.vm().opline(frame);
    exec::opline_at(code, at).ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: frame {} stands at opline {at}, past the end of its op array",
            frame.0
        ))
    })
}
