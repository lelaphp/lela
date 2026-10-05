//! What a debug build's sensors in this crate found (design §10): the
//! one place a defect the heap senses waits until the engine asks, at
//! its next sensing point and at the request's end
//! ([`crate::Heap::take_sensed`]). A sensor records here and goes on;
//! it never decides how the request ends, and nothing it records is
//! dropped unasked.
//!
//! The record is the calling thread's and not the heap's, because the
//! sensor that writes it most is the refcounted header's increment and
//! decrement, which every path reaches and none of which can name the
//! heap its block came from. php keeps its allocator's state per thread
//! for the same reason (`AG(mm_heap)` under ZTS), and a request runs to
//! completion on the one thread it started on (§5.14), its fibers
//! included, so what one request's sensors record is that request's,
//! taken at its end before the thread serves the next.
//!
//! The first defect is kept and a later one is not: the first ends the
//! request where the engine next asks, and what follows it is read
//! through memory that defect has already made untrustworthy. A release
//! build senses nothing and keeps nothing.

use crate::Fatal;

/// What a refcount operation was asked to do.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefOp {
    Addref,
    Release,
}

/// One defect a debug build sensed.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sensed {
    /// A refcount operation refused because the block it named was one
    /// the heap had freed: the block, the operation and the line that
    /// asked for it.
    FreedUse {
        at: usize,
        op: RefOp,
        site: &'static std::panic::Location<'static>,
    },
}

#[cfg(debug_assertions)]
impl Sensed {
    /// The `InternalError` fatal that ends the request, naming what was
    /// sensed and where.
    fn into_fatal(self) -> Fatal {
        match self {
            Sensed::FreedUse { at, op, site } => {
                let op = match op {
                    RefOp::Addref => "an addref",
                    RefOp::Release => "a release",
                };
                Fatal::Error(format!(
                    "InternalError: {op} of the block at {at:#x}, which the heap has freed, at {site}"
                ))
            }
        }
    }
}

#[cfg(debug_assertions)]
std::thread_local! {
    /// The first defect sensed on this thread and not yet taken.
    static FIRST: std::cell::Cell<Option<Sensed>> = const { std::cell::Cell::new(None) };
}

/// `sensed` kept for the engine's next question, unless a defect is
/// kept already.
#[cfg(debug_assertions)]
pub(crate) fn record(sensed: Sensed) {
    FIRST.with(|first| {
        let kept = first.take();
        first.set(Some(kept.unwrap_or(sensed)));
    });
}

/// The defect kept, as the `InternalError` fatal that ends the request,
/// and taken, so it is raised once. `None` where none was sensed, and
/// always in a release build.
pub(crate) fn take() -> Option<Fatal> {
    #[cfg(debug_assertions)]
    {
        FIRST.with(std::cell::Cell::take).map(Sensed::into_fatal)
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}
