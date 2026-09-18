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

use std::sync::atomic::AtomicU32;

use php_heap::{Fatal, FrameRef};

use crate::api::abi::Extension;
use crate::api::request::Request;
use crate::{exec, interrupt};

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
/// the whole of it while `Nop` is the only adapter; a handler cannot
/// dispatch from another frame, so a tier that served code at a
/// handler's site lands with `php_jit` (ADR-0006). The loop's own sites
/// take a `Resume` through [`answered`].
pub(crate) fn continued(flow: Flow) -> Result<(), Fatal> {
    match flow {
        Flow::Continue => Ok(()),
        Flow::Bail(fatal) => Err(fatal),
        Flow::Resume(_) => Err(Fatal::Error(String::from(
            "InternalError: a tier served code and this loop has no resumption for it yet",
        ))),
    }
}

/// What the loop does with the flow a safepoint answered
/// (`jit-design.md` §2.2), as an edge it already knows: `Continue` goes
/// on in the same frame, and a `Resume` is the frame the tier left the
/// loop in, after the interrupt check the loop performs at a back-edge
/// and the pending test it performs after a `throw`-effect op. The two
/// are in that order, and an interrupt the tier found raised is raised
/// still, because consuming it is the loop's.
fn answered(req: &mut Request, flow: Flow, frame: FrameRef) -> Result<Stepped, Fatal> {
    let at = match flow {
        Flow::Continue => return Ok(Stepped::Same(frame)),
        Flow::Bail(fatal) => return Err(fatal),
        Flow::Resume(at) => at,
    };
    interrupt::check(req)?;
    if req.is_pending() {
        return Ok(Stepped::Threw(at));
    }
    Ok(Stepped::Same(at))
}

/// The activation of a frame, its prologue having raised nothing
/// (design §5.6, §4.3 seam 8): the interrupt check, then `Site::Entry`,
/// then the frame the loop goes on in.
fn activated<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    interrupt::check(req)?;
    let flow = T::safepoint(req, frame, Site::Entry);
    answered(req, flow, frame)
}

/// What a serving does with a throwable its entry frame did not catch:
/// the outermost serving renders it as the uncaught fatal, where php's
/// `zend_execute_scripts` calls `zend_exception_error`, and a serving
/// another serving is inside leaves it pending for its caller to read,
/// where php's `zend_call_function` returns with `EG(exception)` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unhandled {
    Render,
    Pending,
}

/// The dispatch loop over `frame` until the entry frame returns, asking
/// `T` at the safepoints. `Ok(())` exactly when the entry frame has
/// returned; `Err` carries the fatal.
pub fn execute<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    serving::<T>(req, frame, Unhandled::Render)
}

/// `zend_call_function` over a user function: the same loop on a frame
/// pushed inside a serving that is already standing, for a native or a
/// destructor calling back into PHP ([`Request::serve_user`]). What its
/// entry frame did not catch stays pending and `Ok(())` says the
/// serving is over, not that the frame returned; the caller reads the
/// pending slot, as php's caller reads `EG(exception)`.
pub(crate) fn reenter<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    serving::<T>(req, frame, Unhandled::Pending)
}

/// One serving: the entry slot pushed, the loop, the entry slot popped
/// on the fatal path as on the normal one.
fn serving<T: Tier>(req: &mut Request, frame: FrameRef, unhandled: Unhandled) -> Result<(), Fatal> {
    let guard = req.stack_mut().push_entry(frame);
    let outcome = serve::<T>(req, frame, unhandled);
    req.stack_mut().pop_entry(guard);
    outcome
}

/// The loop itself, with the entry slot standing. `execute` activates
/// its own entry frame whatever the caller is, where php installs its
/// own site (design §4.3 seam 8).
fn serve<T: Tier>(req: &mut Request, entry: FrameRef, unhandled: Unhandled) -> Result<(), Fatal> {
    let mut frame = entry;
    let mut stepped = activated::<T>(req, frame)?;
    loop {
        match stepped {
            Stepped::Same(at) | Stepped::Moved(at) | Stepped::Popped(at) => frame = at,
            Stepped::Pushed(at) => {
                frame = at;
                stepped = activated::<T>(req, frame)?;
                continue;
            }
            Stepped::Threw(_) => {
                return match unhandled {
                    Unhandled::Render => req.uncaught(),
                    Unhandled::Pending => Ok(()),
                };
            }
            Stepped::Done => return Ok(()),
        }
        let serving = standing(req, frame)?;
        let code = code_of(&serving, frame)?;
        let op = *at_opline(req, frame, code)?;
        stepped = dispatch::dispatch::<T>(req, frame, code, &op)?;
    }
}

/// Exactly one handler, the one at the frame's opline, with `Nop` at the
/// safepoints; what it did to the frame chain is reported, and a
/// throwable it raised is left pending.
pub fn step(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    let serving = standing(req, frame)?;
    let code = code_of(&serving, frame)?;
    let op = *at_opline(req, frame, code)?;
    dispatch::dispatch::<Nop>(req, frame, code, &op)
}

/// What `frame` is serving, held by its own share of the unit so the
/// oplines can be read while the request is borrowed.
fn standing(req: &Request, frame: FrameRef) -> Result<exec::Serving, Fatal> {
    req.vm().serving(frame).ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: frame {} is serving no op array",
            frame.0
        ))
    })
}

/// The op array `frame` is serving, out of its unit.
fn code_of(serving: &exec::Serving, frame: FrameRef) -> Result<&php_unit::OpArray, Fatal> {
    serving.code().ok_or_else(|| {
        Fatal::Error(format!(
            "InternalError: frame {} names a body its unit has not",
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
