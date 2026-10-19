//! The owner of every published trace (`jit-design.md` §4.5), so far its
//! number, [`TraceId`], which `policy` stores in a word, the one space
//! of numbers, [`TraceTable`], and its code arena, `code`, the one place
//! a trace's bytes are written. The slots, the exits and the one mutex a
//! publish takes land with the first compiled trace (#2956); until then
//! `code` is built for its tests (#2741) and a number is all a published
//! trace has.

use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "seams.toml rule unsafe.homes: php_jit::table::code"
)]
mod code;

/// One published trace's number (`jit-design.md` §3.2, §4.5): one space
/// for roots and side traces, whichever word holds it, below `2^24`, the
/// width a word keeps it in. Zero is no trace, as php's trace numbers
/// count from one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TraceId(u32);

impl TraceId {
    /// The first number past the ones a word can hold.
    pub(crate) const LIMIT: u32 = 1 << 24;

    /// Trace number `n`; none for zero and past [`TraceId::LIMIT`].
    pub(crate) fn new(n: u32) -> Option<TraceId> {
        (n != 0 && n < TraceId::LIMIT).then_some(TraceId(n))
    }

    /// The number.
    pub(crate) fn get(self) -> u32 {
        self.0
    }
}

/// The epoch's traces (`jit-design.md` §4.5), so far their numbers: one
/// space for roots and side traces, counted from one as php's
/// `ZEND_JIT_TRACE_NUM` is, a number taken only by a trace published.
/// No trace has code yet, so a word that enters one finds none to enter,
/// which is "not compiled" to every reader (`jit-design.md` §3.2, §4.9).
#[derive(Debug)]
pub(crate) struct TraceTable {
    next: AtomicU32,
}

impl TraceTable {
    /// No trace.
    pub(crate) fn new() -> TraceTable {
        TraceTable {
            next: AtomicU32::new(1),
        }
    }

    /// The number the next trace published takes, which a `TRACE start`
    /// line names before the recording, as php's names
    /// `ZEND_JIT_TRACE_NUM`; none past the numbers a word holds.
    pub(crate) fn next(&self) -> Option<TraceId> {
        TraceId::new(self.next.load(Ordering::Relaxed))
    }

    /// One more trace published: its number, or none where the space is
    /// spent.
    pub(crate) fn publish(&self) -> Option<TraceId> {
        let taken = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < TraceId::LIMIT).then_some(n + 1)
            })
            .ok()?;
        TraceId::new(taken)
    }
}
