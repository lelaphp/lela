//! The engine-owned form of the units an epoch's cache holds
//! (`jit-design.md` §3.1, §3.2; design §5.14 *Identities*, §6.3): one
//! [`Func`] per op array of each held unit, beside the unit, with one
//! [`Word`] per entry of the op array's `safepoints`, all bits zero. The
//! words are the engine's and never the unit's (ADR-0001).
//!
//! A held unit's op arrays took the engine's map-pointer offsets as it
//! was published (`ZEND_MAP_PTR_NEW`), each its own and never another's,
//! so an op array is found by its offset: [`Funcs`] keeps the epoch's
//! rows in a column numbered by it, each written once as its unit is
//! held and kept until the epoch is dropped, so a unit the cache
//! replaces stays with every op array of it, as design §6.3 keeps an
//! evicted unit until the restart. An op array compiled for one request
//! alone, an `eval`'s or an include no cache holds, took the request's
//! offsets, and no row stands for it: it reads the sealed word.

use php_unit::{MapPtrOffset, OpArray, PublishedUnit};

use super::{SafepointIdx, Word};
use crate::arena::Column;
use crate::exec::{self, Body};

/// The one sealed word: what every safepoint of an op array no epoch
/// holds reads, and what a number past an op array's safepoints reads.
/// No tier writes it, since a sealed word decodes as one no tier acts
/// on, so one word stands for all of them and an op array of the
/// request's costs no allocation.
pub(super) static SEALED: Word = Word::sealed();

/// One op array of a held unit, design §3.1's `Func`: the unit it is of,
/// its place there, and one word per safepoint.
pub(crate) struct Func {
    unit: PublishedUnit,
    body: Body,
    words: Box<[Word]>,
}

impl Func {
    /// The op array itself.
    pub(crate) fn op_array(&self) -> Option<&OpArray> {
        exec::body_of(&self.unit, self.body)
    }

    /// The word of safepoint `at`; the sealed word past the last.
    pub(crate) fn word(&self, at: SafepointIdx) -> &Word {
        usize::try_from(at.0)
            .ok()
            .and_then(|at| self.words.get(at))
            .unwrap_or(&SEALED)
    }
}

/// The rows of one epoch's held op arrays, by map-pointer offset: the
/// offset less `first`, the first offset the epoch's rows are numbered
/// from.
pub(crate) struct Funcs {
    first: u32,
    rows: Column<Func>,
}

impl Funcs {
    /// No row, numbered from offset `first`.
    pub(crate) fn numbered_from(first: u32) -> Funcs {
        Funcs {
            first,
            rows: Column::below(u32::MAX),
        }
    }

    /// The next epoch's: numbered from past every offset this one holds
    /// a row at, which is below every offset the engine hands out after
    /// the epoch is replaced.
    pub(crate) fn after(&self) -> Funcs {
        Funcs::numbered_from(self.first.saturating_add(self.rows.reserved()))
    }

    /// One row per op array of `unit`, which the epoch's cache holds,
    /// each at its offset with a zero word per safepoint. An op array
    /// whose offset is the request's, or below the epoch's first, takes
    /// none and reads the sealed word.
    pub(crate) fn publish(&self, unit: &PublishedUnit) {
        for (body, code) in bodies(unit) {
            let MapPtrOffset::Engine(offset) = code.runtime_cache else {
                continue;
            };
            let Some(at) = offset.checked_sub(self.first) else {
                continue;
            };
            let words = code.safepoints.iter().map(|_| Word::default()).collect();
            let row = Func {
                unit: PublishedUnit::clone(unit),
                body,
                words,
            };
            // A slot already written is another publication of the same
            // offset, which no publisher makes: the first stands.
            self.rows.place(at, row).ok();
        }
    }

    /// The row of the op array at engine offset `offset`, where this
    /// epoch holds it.
    pub(crate) fn get(&self, offset: u32) -> Option<&Func> {
        self.rows.get(offset.checked_sub(self.first)?)
    }
}

/// Every op array of `unit` with its place: the main one, its
/// functions', the ones no name binds, its methods' and its hooks', the
/// order `assign_map_ptrs` gives them their offsets in.
fn bodies(unit: &PublishedUnit) -> impl Iterator<Item = (Body, &OpArray)> {
    let numbered = |at: usize| u32::try_from(at).unwrap_or(u32::MAX);
    std::iter::once((Body::Main, &unit.main))
        .chain(
            unit.functions
                .iter()
                .enumerate()
                .map(move |(at, (_, code))| (Body::Function(numbered(at)), code)),
        )
        .chain(
            unit.dynamic
                .iter()
                .enumerate()
                .map(move |(at, (_, code))| (Body::Dynamic(numbered(at)), code)),
        )
        .chain(
            unit.methods
                .iter()
                .enumerate()
                .map(move |(at, method)| (Body::Method(numbered(at)), &method.body)),
        )
        .chain(
            unit.hooks
                .iter()
                .enumerate()
                .map(move |(at, hook)| (Body::Hook(numbered(at)), &hook.body)),
        )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use php_unit::{OpArray, OplineIdx, Unit};

    use super::{Funcs, SEALED};
    use crate::tier::SafepointIdx;

    /// A unit whose main op array has `safepoints` safepoints, published
    /// at offset `first`.
    fn published(first: u32, safepoints: u32) -> php_unit::PublishedUnit {
        let mut unit = Unit {
            main: OpArray {
                safepoints: (0..safepoints).map(OplineIdx).collect::<Vec<_>>().into(),
                ..OpArray::default()
            },
            ..Unit::default()
        };
        let numbers = std::sync::atomic::AtomicU32::new(first);
        crate::engine::assign_map_ptrs(&mut unit, &numbers);
        unit.published()
    }

    /// Each op array of a held unit has a row at its offset with a zero
    /// word per safepoint and the sealed word past them; the next epoch
    /// numbers its rows past this one's, and holds none of them.
    #[test]
    fn a_held_units_op_arrays_have_rows_at_their_offsets_and_the_next_epoch_starts_past_them() {
        let funcs = Funcs::numbered_from(10);
        funcs.publish(&published(10, 3));
        let main = funcs.get(10);
        assert!(main.is_some_and(|main| main.op_array().is_some()));
        let word = main.map(|main| main.word(SafepointIdx(2)));
        let zero = |half: &std::sync::atomic::AtomicU32| half.load(Ordering::Relaxed) == 0;
        assert!(word.is_some_and(|word| word.halves().iter().all(zero)));
        let past = main.map(|main| main.word(SafepointIdx(3)));
        assert!(past.is_some_and(|past| std::ptr::addr_eq(past, std::ptr::addr_of!(SEALED))));
        assert!(funcs.get(9).is_none() && funcs.get(11).is_none());
        let next = funcs.after();
        // 11, past the one offset the epoch held a row at.
        assert!(next.get(10).is_none());
        next.publish(&published(10, 1));
        assert!(next.get(10).is_none(), "below the epoch's first: sealed");
        next.publish(&published(11, 1));
        assert!(next.get(11).is_some());
    }
}
