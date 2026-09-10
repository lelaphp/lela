//! Seam 8, the tier (design §4.3, `jit-design.md` §2, §3; ADR-0006): the
//! loop asks the tier at three safepoints and nowhere else, through one
//! method bound per engine. `Nop` is the interpreter's own adapter; the
//! compiled tier is `php_jit::Jit`. The word each safepoint carries is
//! two atomics whose bits only the tier interprets. `execute` and `step`
//! are the loop's two entries; this skeleton has no loop, and each ends
//! the request in the internal-error fatal that names it.

use std::sync::atomic::AtomicU32;

use php_heap::{Fatal, FrameRef};

use crate::api::abi::Extension;
use crate::api::request::Request;

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

fn no_loop(what: &str) -> Fatal {
    Fatal::Error(format!(
        "InternalError: php_engine::tier::{what} has no dispatch loop yet; the engine serves nothing"
    ))
}

/// The dispatch loop over `frame` until the entry frame returns, asking
/// `T` at the safepoints. `Ok(())` exactly when the entry frame has
/// returned; `Err` carries the fatal.
pub fn execute<T: Tier>(req: &mut Request, frame: FrameRef) -> Result<(), Fatal> {
    let _ = (req, frame);
    Err(no_loop("execute"))
}

/// Exactly one handler, the one at the frame's opline, with `Nop` at the
/// safepoints; what it did to the frame chain is reported, and a
/// throwable it raised is left pending.
pub fn step(req: &mut Request, frame: FrameRef) -> Result<Stepped, Fatal> {
    let _ = (req, frame);
    Err(no_loop("step"))
}
