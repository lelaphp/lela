//! What a debug build's sensors in this crate found (design §10): the
//! one place a defect the heap senses waits until the engine asks, at
//! its next sensing point and at the request's end
//! ([`crate::Heap::take_sensed`]). A sensor records here and goes on;
//! it never decides how the request ends, and nothing it records is
//! dropped unasked.
//!
//! The record is the request's heap's, a [`Record`] its allocator owns,
//! and not the thread's: no engine state is per thread (§5.14). The
//! sensor that writes it most is the refcounted header's increment and
//! decrement, which every path reaches and which cannot name its heap,
//! so the allocator writes the record's address into the header of
//! every chunk it lays down, and a sensor reaches it from the block it
//! found through the block's chunk (`mm::freed::record`), as php reaches
//! a block's heap through `chunk->heap` (`Z/Zend/zend_alloc.c:1454`).
//!
//! The first defect is kept and a later one is not: the first ends the
//! request where the engine next asks, and what follows it is read
//! through memory that defect has already made untrustworthy. A release
//! build senses nothing and keeps nothing, and compiles none of this.

use crate::Fatal;

/// What a refcount operation was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefOp {
    Addref,
    Release,
}

/// One defect a debug build sensed.
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

/// The first defect a debug build sensed in one heap and the engine has
/// not yet taken. Written through a shared pointer, from a chunk's
/// header, so the defect sits in a `Cell`.
#[derive(Default)]
pub(crate) struct Record(std::cell::Cell<Option<Sensed>>);

impl std::fmt::Debug for Record {
    /// Nothing of the defect: reading it would take it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record").finish_non_exhaustive()
    }
}

impl Record {
    /// `sensed` kept for the engine's next question, unless a defect is
    /// kept already.
    pub(crate) fn record(&self, sensed: Sensed) {
        let kept = self.0.take();
        self.0.set(Some(kept.unwrap_or(sensed)));
    }

    /// The defect kept, as the `InternalError` fatal that ends the
    /// request, and taken, so it is raised once.
    pub(crate) fn take(&self) -> Option<Fatal> {
        self.0.take().map(Sensed::into_fatal)
    }
}
