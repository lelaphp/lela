//! The engine-owned form of the units an epoch's cache holds
//! (`jit-design.md` §3.1, §3.2; design §5.14 *Identities*, §6.3): one
//! [`Func`] per op array of each held unit, beside the unit, with one
//! [`Word`] per entry of the op array's `safepoints`, all bits zero, or
//! sealed where the tier said the unit's words do not count, and
//! a landing word per opline, laid down sealed the first time a tier arms
//! one (`jit-design.md` §2.2). The words are the engine's and never the
//! unit's (ADR-0001).
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

use std::sync::OnceLock;

use php_unit::decl::Parameter;
use php_unit::{MapPtrOffset, OpArray, OplineIdx, PublishedUnit};

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
/// its place there, one word per safepoint, and the landing words.
pub(crate) struct Func {
    unit: PublishedUnit,
    body: Body,
    words: Box<[Word]>,
    /// One word per opline, php's `zend_op_trace_info` of the opline a
    /// return lands on, laid down sealed the first time a tier
    /// arms a landing of the op array ([`Func::lay_landing`]), so an op
    /// array no trace returns into costs no allocation.
    landings: OnceLock<Box<[Word]>>,
}

impl Func {
    /// The op array itself.
    pub(crate) fn op_array(&self) -> Option<&OpArray> {
        exec::body_of(&self.unit, self.body)
    }

    /// Whether the op array is its file's own code, the main op array.
    pub(crate) fn is_main(&self) -> bool {
        matches!(self.body, Body::Main)
    }

    /// Whether every frame that runs the op array runs it in one scope:
    /// a function's, whose scope is none, and a method's or a property
    /// hook's of a class that is no trait, whose scope is its class. A
    /// trait's method runs in each class that uses it, a closure in the
    /// scope it is bound to and a file's code in its includer's, so none
    /// of those does.
    pub(crate) fn fixes_scope(&self) -> bool {
        let unit = &self.unit;
        let class = match self.body {
            Body::Function(_) => return true,
            Body::Main | Body::Dynamic(_) => return false,
            Body::Method(at) => unit.methods.get(at as usize).map(|method| method.class),
            Body::Hook(at) => unit.hooks.get(at as usize).map(|hook| hook.class),
        };
        class
            .and_then(|class| unit.classes.get(class as usize))
            .is_some_and(|class| class.kind != php_unit::decl::ClassKind::Trait)
    }

    /// The parameters the op array's declaration lists: none for a
    /// file's own code.
    pub(crate) fn parameters(&self) -> Option<&[Parameter]> {
        let unit = &self.unit;
        let decl = match self.body {
            Body::Main => return Some(&[]),
            Body::Function(at) => &unit.functions.get(at as usize)?.0,
            Body::Dynamic(at) => &unit.dynamic.get(at as usize)?.0,
            Body::Method(at) => {
                let method = unit.methods.get(at as usize)?;
                let class = unit.classes.get(method.class as usize)?;
                &class.methods.get(method.method as usize)?.function
            }
            Body::Hook(at) => {
                let hook = unit.hooks.get(at as usize)?;
                let class = unit.classes.get(hook.class as usize)?;
                let property = class.properties.get(hook.property as usize)?;
                &property.hooks.as_ref()?.of(hook.kind)?.function
            }
        };
        Some(&decl.parameters)
    }

    /// The word of safepoint `at`; the sealed word past the last.
    pub(crate) fn word(&self, at: SafepointIdx) -> &Word {
        usize::try_from(at.0)
            .ok()
            .and_then(|at| self.words.get(at))
            .unwrap_or(&SEALED)
    }

    /// The landing word of opline `at`: sealed until a tier armed it, and
    /// past the last opline.
    pub(crate) fn landing(&self, at: OplineIdx) -> &Word {
        (self.landings.get())
            .and_then(|words| words.get(usize::try_from(at.0).ok()?))
            .unwrap_or(&SEALED)
    }

    /// The landing word of opline `at`, for a tier to arm: the op array's
    /// landing words laid down sealed the first time; none past its last
    /// opline.
    pub(crate) fn lay_landing(&self, at: OplineIdx) -> Option<&Word> {
        let oplines = self.op_array().map_or(0, |code| code.oplines.len());
        let words = (self.landings).get_or_init(|| (0..oplines).map(|_| Word::sealed()).collect());
        words.get(usize::try_from(at.0).ok()?)
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
    /// each at its offset with a word per safepoint: zero where the
    /// unit's words `count`, and sealed where they do not, which no
    /// visit acts on, as php's op array persisted while its tier was off
    /// keeps php's own handlers ([`super::Tier::counts`]). An op array
    /// whose offset is the request's, or below the epoch's first, takes
    /// none and reads the sealed word.
    pub(crate) fn publish(&self, unit: &PublishedUnit, count: bool) {
        for (body, code) in bodies(unit) {
            let MapPtrOffset::Engine(offset) = code.runtime_cache else {
                continue;
            };
            let Some(at) = offset.checked_sub(self.first) else {
                continue;
            };
            let word = || {
                if count {
                    Word::default()
                } else {
                    Word::sealed()
                }
            };
            let words = code.safepoints.iter().map(|_| word()).collect();
            let row = Func {
                unit: PublishedUnit::clone(unit),
                body,
                words,
                landings: OnceLock::new(),
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

    use php_unit::{OpArray, Opline, OplineIdx, Unit};

    use super::{Funcs, SEALED};
    use crate::tier::SafepointIdx;

    /// A unit whose main op array has `safepoints` safepoints and as many
    /// oplines, published at offset `first`.
    fn published(first: u32, safepoints: u32) -> php_unit::PublishedUnit {
        let mut unit = Unit {
            main: OpArray {
                safepoints: (0..safepoints).map(OplineIdx).collect::<Vec<_>>().into(),
                oplines: (0..safepoints).map(|_| Opline::default()).collect(),
                ..OpArray::default()
            },
            ..Unit::default()
        };
        let numbers = std::sync::atomic::AtomicU32::new(first);
        crate::engine::assign_map_ptrs(&mut unit, &numbers);
        unit.published(None)
    }

    /// Each op array of a held unit has a row at its offset with a zero
    /// word per safepoint and the sealed word past them; the next epoch
    /// numbers its rows past this one's, and holds none of them.
    #[test]
    fn a_held_units_op_arrays_have_rows_at_their_offsets_and_the_next_epoch_starts_past_them() {
        let funcs = Funcs::numbered_from(10);
        funcs.publish(&published(10, 3), true);
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
        next.publish(&published(10, 1), true);
        assert!(next.get(10).is_none(), "below the epoch's first: sealed");
        next.publish(&published(11, 1), true);
        assert!(next.get(11).is_some());
    }

    /// An op array's landing words read sealed until a tier lays them
    /// down, one per opline, each sealed until armed, and the same word
    /// is read where it was laid; past the last opline there is none.
    #[test]
    fn landing_words_are_sealed_until_laid_and_one_per_opline() {
        let funcs = Funcs::numbered_from(0);
        funcs.publish(&published(0, 3), true);
        let main = funcs.get(0).expect("the main op array has a row");
        let sealed = |word: &crate::tier::Word| {
            word.halves()
                .iter()
                .all(|half| half.load(Ordering::Relaxed) == u32::MAX)
        };
        assert!(std::ptr::addr_eq(
            main.landing(OplineIdx(1)),
            std::ptr::addr_of!(SEALED)
        ));
        let laid = main
            .lay_landing(OplineIdx(1))
            .expect("a landing within the op array");
        assert!(sealed(laid));
        assert!(!std::ptr::addr_eq(laid, std::ptr::addr_of!(SEALED)));
        laid.halves()[1].store(0, Ordering::Relaxed);
        assert_eq!(
            main.landing(OplineIdx(1)).halves()[1].load(Ordering::Relaxed),
            0
        );
        assert!(sealed(main.landing(OplineIdx(2))));
        assert!(main.lay_landing(OplineIdx(3)).is_none());
        // A unit whose words do not count has them sealed, each its own.
        let uncounted = Funcs::numbered_from(0);
        uncounted.publish(&published(0, 2), false);
        let main = uncounted.get(0).expect("an uncounted unit keeps its row");
        assert!(main.op_array().is_some() && sealed(main.word(SafepointIdx(1))));
        assert!(!std::ptr::addr_eq(
            main.word(SafepointIdx(1)),
            std::ptr::addr_of!(SEALED)
        ));
        assert!(std::ptr::addr_eq(
            main.landing(OplineIdx(3)),
            std::ptr::addr_of!(SEALED)
        ));
    }
}
