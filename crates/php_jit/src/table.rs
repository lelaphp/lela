//! The owner of every published trace (`jit-design.md` §4.5), so far its
//! number, [`TraceId`], which `policy` stores in a word, and its code
//! arena, `code`, the one place a trace's bytes are written. The table,
//! its slots and the one mutex a publish takes land with the first
//! trace; until then `code` is built for its tests (#2741).
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
