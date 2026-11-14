//! The engine-owned form of the units an epoch's cache holds
//! (`jit-design.md` §3.1, §3.2; design §5.14 *Identities*, §6.3): one
//! [`Func`] per op array of each held unit, beside the unit, with one
//! [`Word`] per entry of the op array's `safepoints`, laid as the tier
//! said the unit's words are ([`super::Laid`]), and
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
//!
//! Compiled code reads the landing words too, where a trace escapes
//! (`jit-design.md` §4.9): through the epoch's [`LandingIndex`], which
//! names each op array's landing words by its offset, the epoch's own
//! and the preload's alike ([`Landings`]), as php's escape reads the
//! handler of the opline the frame stands at in the op array itself.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use php_unit::decl::{FunctionDecl, Name, Parameter};
use php_unit::frozen::Frozen;
use php_unit::{OpArray, OplineIdx, PublishedUnit};

use super::{Laid, SafepointIdx, Word};
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
#[derive(php_macros::EngineState)]
pub(crate) struct Func {
    unit: Frozen<PublishedUnit>,
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

    /// Whether the op array is a property hook's body, php's
    /// `op_array.prop_info` set.
    pub(crate) fn is_hook(&self) -> bool {
        matches!(self.body, Body::Hook(_))
    }

    /// Whether a frame that runs the op array may have a receiver,
    /// `EX(This)` an object: every op array but a function's, whose frame
    /// never has one.
    pub(crate) fn receives(&self) -> bool {
        !matches!(self.body, Body::Function(_))
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
        let (decl, _) = self.declaration()?;
        Some(decl.map_or(&[][..], |decl| &decl.parameters))
    }

    /// The declaration the op array is the body of, none for a file's own
    /// code, and the name of the class a method's, a hook's or a closure
    /// written in either is declared in, php's `function_name` and
    /// `scope`.
    pub(crate) fn declaration(&self) -> Option<(Option<&FunctionDecl>, Option<&Name>)> {
        let unit = &self.unit;
        let (decl, class) = match self.body {
            Body::Main => return Some((None, None)),
            Body::Function(at) => (&unit.functions.get(at as usize)?.0, None),
            Body::Dynamic(at) => (&unit.dynamic.get(at as usize)?.0, dynamic_scope(unit, at)),
            Body::Method(at) => {
                let method = unit.methods.get(at as usize)?;
                let class = unit.classes.get(method.class as usize)?;
                let method = &class.methods.get(method.method as usize)?.function;
                (method, Some(&class.name))
            }
            Body::Hook(at) => {
                let hook = unit.hooks.get(at as usize)?;
                let class = unit.classes.get(hook.class as usize)?;
                let property = class.properties.get(hook.property as usize)?;
                (
                    &property.hooks.as_ref()?.of(hook.kind)?.function,
                    Some(&class.name),
                )
            }
        };
        Some((Some(decl), class))
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
        self.laid_landing(at).unwrap_or(&SEALED)
    }

    /// The landing word of opline `at` where the op array's landing words
    /// are laid down; none where they are not, and past the last opline.
    pub(crate) fn laid_landing(&self, at: OplineIdx) -> Option<&Word> {
        self.landings.get()?.get(usize::try_from(at.0).ok()?)
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
#[derive(php_macros::EngineState)]
pub(crate) struct Funcs {
    first: u32,
    rows: Column<Func>,
}

/// How many op arrays one chunk of a [`LandingIndex`] names: a number
/// past the epoch's first offset names its chunk by its bits above
/// these, and its place in the chunk by these.
pub const LANDING_CHUNK_BITS: u32 = 12;
const LANDING_CHUNK: usize = 1 << LANDING_CHUNK_BITS;

/// Where compiled code finds the landing words of an epoch's op arrays
/// (`jit-design.md` §4.9), as php's escape finds the handler of the
/// opline the frame stands at in the op array itself: at `chunks`, one
/// address per chunk of [`LANDING_CHUNK_BITS`] places, zero until a
/// landing of an op array named in it is laid down, and at that address
/// two words per place, the address of the op array's landing words,
/// one [`Word`] per opline, zero until a tier laid them down, and how
/// many there are. An op array is named at a place by its offset, which
/// [`LandingIndex::place`] says: the epoch's own rows first, then the
/// rows the epoch lays for the preload's op arrays
/// (`crate::arena::Base`), as `Arena::func` reads a row; one a request
/// compiled is named by neither, and reads as no compiled landing.
/// Each address is written once, with release, after what it names, and
/// stands as long as the epoch, which every trace whose code reads it
/// holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandingIndex {
    /// The address of the chunks' addresses.
    pub chunks: usize,
    /// The epoch's own op arrays, named from the index's first place.
    pub own: LandingRows,
    /// The preload's op arrays, named past the epoch's own; none where
    /// nothing was preloaded.
    pub preloaded: Option<LandingRows>,
}

/// One stretch of op arrays a [`LandingIndex`] names: the op array at offset
/// `first + n`, for `n` below `held`, at place `place + n`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
pub struct LandingRows {
    /// The offset the stretch's first op array stands at.
    pub first: u32,
    /// How many op arrays past `first` the stretch can name: as many as
    /// a [`Column`] numbers for the epoch's own, and as many as the epoch
    /// laid rows for, all as it was made, for the preload's.
    pub held: u32,
    /// The place the stretch's first op array is named at.
    pub place: u32,
}

impl LandingRows {
    /// The place of the op array at `offset`, where the stretch names it.
    fn place(self, offset: u32) -> Option<u32> {
        let past = offset
            .checked_sub(self.first)
            .filter(|past| *past < self.held)?;
        self.place.checked_add(past)
    }
}

impl LandingIndex {
    /// The place the op array at `offset` is named at: in the epoch's own
    /// stretch where that names it, in the preload's otherwise, and none past
    /// both; the one rule the escape's code follows too.
    #[must_use]
    pub fn place(&self, offset: u32) -> Option<u32> {
        place_among(self.own, self.preloaded, offset)
    }
}

/// [`LandingIndex::place`] over the stretches `own` and `preloaded`.
fn place_among(own: LandingRows, preloaded: Option<LandingRows>, offset: u32) -> Option<u32> {
    own.place(offset).or_else(|| preloaded?.place(offset))
}

/// An epoch's landing index ([`LandingIndex`]), over its own [`Funcs`]
/// and the ones it lays for the preload's op arrays, laid down the
/// first time a tier arms a landing or asks for it.
#[derive(php_macros::EngineState)]
pub(crate) struct Landings {
    own: LandingRows,
    preloaded: Option<LandingRows>,
    index: OnceLock<Index>,
}

impl Landings {
    /// The index over `own`, the epoch's own rows, which a [`Column`]
    /// numbers at most [`crate::arena::COLUMN_ROWS`] of, and `preloaded`,
    /// the epoch's rows of the preload's op arrays, every one of which
    /// it laid as the epoch was made ([`Funcs::preloaded`]).
    pub(crate) fn over(own: &Funcs, preloaded: Option<&Funcs>) -> Landings {
        let held = u32::try_from(crate::arena::COLUMN_ROWS).unwrap_or(u32::MAX);
        let own = LandingRows {
            first: own.first,
            held,
            place: 0,
        };
        let preloaded = preloaded.map(|funcs| LandingRows {
            first: funcs.first,
            held: funcs.rows.reserved(),
            place: held,
        });
        Landings {
            own,
            preloaded,
            index: OnceLock::new(),
        }
    }

    /// The index as compiled code reads it, laid down where it was not.
    pub(crate) fn index(&self) -> LandingIndex {
        LandingIndex {
            chunks: self.laid().addresses.as_ptr().expose_provenance(),
            own: self.own,
            preloaded: self.preloaded,
        }
    }

    /// The landing word of opline `at` of `row`, the op array at
    /// `offset`, for a tier to arm ([`Func::lay_landing`]), its op
    /// array's landing words named in the index; none past its last
    /// opline.
    pub(crate) fn lay<'f>(&self, row: &'f Func, offset: u32, at: OplineIdx) -> Option<&'f Word> {
        let place = place_among(self.own, self.preloaded, offset)?;
        let word = row.lay_landing(at)?;
        let words = row.landings.get()?;
        self.laid().name(place, words);
        Some(word)
    }

    /// The address of the landing words the index names the op array at
    /// `offset` by, where it names any.
    #[cfg(test)]
    pub(crate) fn named(&self, offset: u32) -> Option<usize> {
        let place = place_among(self.own, self.preloaded, offset)?;
        let place = usize::try_from(place).ok()?;
        let chunk = self
            .index
            .get()?
            .chunks
            .get(place >> LANDING_CHUNK_BITS)?
            .get()?;
        let words = chunk.get(2 * (place & (LANDING_CHUNK - 1)))?;
        Some(words.load(Ordering::Acquire)).filter(|words| *words != 0)
    }

    fn laid(&self) -> &Index {
        let places =
            (self.preloaded).map_or(self.own.held, |rows| rows.place.saturating_add(rows.held));
        let chunks = places.div_ceil(1 << LANDING_CHUNK_BITS);
        (self.index).get_or_init(|| Index::new(usize::try_from(chunks).unwrap_or(0)))
    }
}

/// The index's own: each chunk, owned, two words per place, and the
/// address of each chunk, which is what compiled code reads, as a
/// request's rows of its traces are kept (`php_jit`'s `enter::Rows`).
#[derive(php_macros::EngineState)]
struct Index {
    chunks: Box<[OnceLock<Box<[AtomicUsize]>>]>,
    addresses: Box<[AtomicUsize]>,
}

impl Index {
    /// `chunks` chunks, none laid.
    fn new(chunks: usize) -> Index {
        Index {
            chunks: (0..chunks).map(|_| OnceLock::new()).collect(),
            addresses: (0..chunks).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    /// `words`, the landing words of the op array named at place `at`,
    /// named in its chunk, the chunk laid down and named first where it
    /// is not; nothing past the places the index has.
    fn name(&self, at: u32, words: &[Word]) {
        let chunk = (at >> LANDING_CHUNK_BITS) as usize;
        let (Some(cell), Some(address)) = (self.chunks.get(chunk), self.addresses.get(chunk))
        else {
            return;
        };
        let laid = cell.get_or_init(|| {
            (0..2 * LANDING_CHUNK)
                .map(|_| AtomicUsize::new(0))
                .collect()
        });
        let place = 2 * (at as usize & (LANDING_CHUNK - 1));
        if let (Some(slot), Some(count)) = (laid.get(place), laid.get(place + 1)) {
            count.store(words.len(), Ordering::Relaxed);
            slot.store(words.as_ptr().expose_provenance(), Ordering::Release);
        }
        address.store(laid.as_ptr().expose_provenance(), Ordering::Release);
    }
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

    /// The rows of the preload's op arrays as one epoch over it runs
    /// them: one per op array of `units`, the preload's, laid as `laid`
    /// says, numbered from the least offset among them. Laid afresh for
    /// every epoch (`crate::arena::Base::again`), so a restart's
    /// epoch starts them as php's `zend_jit_restart` leaves a preloaded
    /// op array (`zend_jit_restart_preloaded_script`,
    /// `Z/ext/opcache/jit/zend_jit.c:4010-4034`): its entries and loops
    /// counting again, no landing armed, nothing blacklisted, and no word
    /// naming a trace of the table the restart dropped.
    pub(crate) fn preloaded(units: &[Frozen<PublishedUnit>], laid: Laid) -> Funcs {
        let bodies = units.iter().flat_map(|unit| bodies(unit));
        let least = bodies.filter_map(|(_, code)| engine_offset(code)).min();
        let funcs = Funcs::numbered_from(least.unwrap_or(0));
        for unit in units {
            funcs.publish(unit, laid);
        }
        funcs
    }

    /// One row per op array of `unit`, which the epoch's cache holds,
    /// each at its offset with a word per safepoint, laid as `laid`
    /// says: at the halves the tier gave where the unit's words count,
    /// and sealed where they do not, which no visit acts on, as php's op
    /// array persisted while its tier was off keeps php's own handlers
    /// ([`super::Tier::laid`]). An op array whose offset is the
    /// request's, or below the epoch's first, takes none and reads the
    /// sealed word.
    pub(crate) fn publish(&self, unit: &Frozen<PublishedUnit>, laid: Laid) {
        for (body, code) in bodies(unit) {
            let Some(offset) = engine_offset(code) else {
                continue;
            };
            let Some(at) = offset.checked_sub(self.first) else {
                continue;
            };
            let word = || match laid {
                Laid::Counting(halves) => Word::laid(halves),
                Laid::Sealed => Word::sealed(),
            };
            let words = code.safepoints.iter().map(|_| word()).collect();
            let row = Func {
                unit: Frozen::clone(unit),
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

/// The engine offset `code` took as its unit was published; none for an
/// op array compiled for one request.
fn engine_offset(code: &OpArray) -> Option<u32> {
    super::EngineFuncRef::of(super::FuncRef::at(code.runtime_cache))
        .map(super::EngineFuncRef::offset)
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

/// The class a function written inside another body is declared in, as
/// php's compiler sets `op_array->scope` to `CG(active_class_entry)`: the
/// class of the method or hook whose body, or a body inside it, declares
/// it; none inside a function or a file's own code.
fn dynamic_scope(unit: &php_unit::Unit, at: u32) -> Option<&Name> {
    let mut at = at;
    for _ in 0..=unit.dynamic.len() {
        let declares = |body: &OpArray| body.dynamic_func_defs.contains(&at);
        let class = (unit
            .methods
            .iter()
            .find(|m| declares(&m.body))
            .map(|m| m.class))
        .or_else(|| {
            unit.hooks
                .iter()
                .find(|h| declares(&h.body))
                .map(|h| h.class)
        });
        if let Some(class) = class {
            return unit.classes.get(class as usize).map(|c| &c.name);
        }
        let outer = unit.dynamic.iter().position(|(_, body)| declares(body))?;
        at = u32::try_from(outer).ok()?;
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::Ordering;

    use php_unit::{OpArray, Opcode, Opline, OplineIdx, Unit};

    use super::{Funcs, Laid, SEALED};
    use crate::tier::SafepointIdx;

    /// A unit whose main op array has `safepoints` safepoints and as many
    /// oplines, the last a `RETURN`, published at offset `first`.
    pub(crate) fn published(
        first: u32,
        safepoints: u32,
    ) -> php_unit::frozen::Frozen<php_unit::PublishedUnit> {
        let opline = |at| Opline {
            opcode: if at + 1 == safepoints {
                Opcode::Return
            } else {
                Opcode::Nop
            },
            ..Opline::default()
        };
        let mut unit = Unit {
            main: OpArray {
                safepoints: (0..safepoints).map(OplineIdx).collect::<Vec<_>>().into(),
                oplines: (0..safepoints).map(opline).collect(),
                ..OpArray::default()
            },
            ..Unit::default()
        };
        let numbers = std::sync::atomic::AtomicU32::new(first);
        crate::engine::assign_map_ptrs(&mut unit, &numbers);
        php_unit::frozen::Frozen::new(unit.published(None)).expect("a unit of the engine's values")
    }

    /// Each op array of a held unit has a row at its offset with a zero
    /// word per safepoint and the sealed word past them; the next epoch
    /// numbers its rows past this one's, and holds none of them.
    #[test]
    fn a_held_units_op_arrays_have_rows_at_their_offsets_and_the_next_epoch_starts_past_them() {
        let funcs = Funcs::numbered_from(10);
        funcs.publish(&published(10, 3), Laid::COLD);
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
        next.publish(&published(10, 1), Laid::COLD);
        assert!(next.get(10).is_none(), "below the epoch's first: sealed");
        next.publish(&published(11, 1), Laid::COLD);
        assert!(next.get(11).is_some());
    }

    /// An op array's landing words read sealed until a tier lays them
    /// down, one per opline, each sealed until armed, and the same word
    /// is read where it was laid; past the last opline there is none.
    #[test]
    fn landing_words_are_sealed_until_laid_and_one_per_opline() {
        let funcs = Funcs::numbered_from(0);
        funcs.publish(&published(0, 3), Laid::COLD);
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
        uncounted.publish(&published(0, 2), Laid::Sealed);
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

    /// The landing index names an op array's landing words once a tier
    /// lays them, at the address of its chunk, at its place past the
    /// epoch's first offset, and names none for an op array no landing of
    /// which is laid: what a trace's escape reads in code.
    #[test]
    fn the_landing_index_names_the_words_a_tier_laid() {
        use super::{LANDING_CHUNK_BITS, LandingIndex, LandingRows, Landings};
        let funcs = Funcs::numbered_from(4);
        funcs.publish(&published(4, 3), Laid::COLD);
        let landings = Landings::over(&funcs, None);
        let LandingIndex {
            chunks,
            own,
            preloaded,
        } = landings.index();
        let rows = LandingRows {
            first: 4,
            held: 1 << 23,
            place: 0,
        };
        assert_eq!((own, preloaded), (rows, None));
        let index = landings.index.get().expect("asked for, so laid");
        assert_eq!(index.addresses.len(), 1 << (23 - LANDING_CHUNK_BITS));
        assert_eq!(chunks, index.addresses.as_ptr().addr());
        assert_eq!(index.addresses[0].load(Ordering::Acquire), 0);
        let main = funcs.get(4).expect("the main op array has a row");
        let word = (landings.lay(main, 4, OplineIdx(2))).expect("a landing within the op array");
        let chunk = index.chunks[0]
            .get()
            .expect("the chunk laid with the words");
        assert_eq!(
            index.addresses[0].load(Ordering::Acquire),
            chunk.as_ptr().addr()
        );
        let words = chunk[0].load(Ordering::Acquire);
        assert_eq!(chunk[1].load(Ordering::Acquire), 3, "one word per opline");
        assert!(std::ptr::addr_eq(main.landing(OplineIdx(2)), word));
        assert_eq!(
            words + 2 * size_of::<crate::tier::Word>(),
            std::ptr::from_ref(word).addr()
        );
        assert!(
            chunk[2..]
                .iter()
                .all(|slot| slot.load(Ordering::Acquire) == 0)
        );
        assert!(landings.index().place(3).is_none());
        assert_eq!(2usize << LANDING_CHUNK_BITS, chunk.len());
    }

    /// An index over the preload's rows names them past the epoch's own,
    /// each by its offset in the stretch it is of, and an offset in
    /// neither stretch at no place (#3568).
    #[test]
    fn the_landing_index_names_the_preloads_rows_past_the_epochs_own() {
        use super::{LandingRows, Landings};
        let preloaded = Funcs::preloaded(&[published(5, 2), published(6, 1)], Laid::COLD);
        let own = Funcs::numbered_from(100);
        let index = Landings::over(&own, Some(&preloaded)).index();
        let rows = LandingRows {
            first: 5,
            held: 2,
            place: 1 << 23,
        };
        assert_eq!(index.preloaded, Some(rows));
        assert_eq!(index.place(100), Some(0));
        assert_eq!(index.place(5), Some(1 << 23));
        assert_eq!(index.place(6), Some((1 << 23) + 1));
        assert_eq!((index.place(4), index.place(7)), (None, None));
        assert_eq!(index.place(100 + (1 << 23)), None);
    }
}
