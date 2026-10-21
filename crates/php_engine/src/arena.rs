//! The engine's published arena (design §5.14, *Identities*): the rows
//! every request shares of the classes and functions published past the
//! engine's own, the preload's first. A [`ClassId`] or a [`Function`]
//! whose top bit is clear names one of these rows, or one of the
//! engine's below them; one whose top bit is set names a row of the
//! request's own.
//!
//! Each kind of row is a [`Column`]: a directory of chunks of one
//! length, each laid down on first use and never moved, of slots that
//! are each written once. A publisher takes a number with
//! [`Column::reserve`] before it builds the row, so rows that name each
//! other (a class and its methods) are built knowing each other's
//! numbers, and fills it with [`Column::fill`]. A request reads a row
//! with [`Column::get`]: the number's high bits held to the directory,
//! its low bits a slot in the chunk with no bound to hold, two acquire
//! loads and nothing taken, as php reads a `zend_class_entry *` from
//! shared memory. A row is found once it is filled and only then, and a
//! publisher hands a number on only after its row is filled, so whatever
//! carried the number to a request (the preload's tables, the unit
//! cache) carried the row with it. A number reserved by a publication
//! that failed is never filled: a hole no request is handed, which the
//! restart counts as waste. No row is written after its number is
//! handed on, nor freed or reused while the arena stands: what a class
//! builds once and keeps (its placed properties) its publisher builds
//! before handing the number on.
//!
//! An opcache restart takes rows back by epochs ([`Epochs`]): it hands
//! every request that starts after it a new, empty arena at once, and
//! the arena it replaced is dropped when the last request holding it
//! ends, as design §5.14 clears an arena "once no request holds its
//! epoch". Nothing renumbers a row: an epoch's numbers name its rows
//! alone, and everything that carries a number from one request to
//! another (the unit cache's files, the inheritance cache) is the
//! epoch's own ([`Arena::files`], [`Arena::inheritance`]), so it reaches
//! only requests of the same epoch. The preload's rows are the one
//! thing every epoch shares: once the preload has published, its arena
//! is laid below every epoch after ([`Epochs::lay_base`]) and an epoch
//! numbers its own rows past it, so the preload's classes and functions
//! keep their numbers across every restart, as php keeps what the
//! preload put in shared memory (`zend_shared_alloc_save_state` after
//! preloading, restored to by `accel_activate`'s restart).
//!
//! Nothing here is `unsafe`: the slots are `OnceLock`s, so a row is
//! `Sync` where its type is, and the chunks never reallocate, so a
//! row borrowed from the arena lives as long as the arena.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use crate::api::call::{ClassNative, Function};
use crate::api::request::{Declared, UserFunction};
use crate::api::values::ClassId;
use php_link::LinkedClass;

/// The rows of one chunk, `1 << CHUNK_BITS` of them: a number's low
/// bits are its slot in the chunk, its high bits the chunk's place in
/// the directory.
const CHUNK_BITS: u32 = 9;
const CHUNK: usize = 1 << CHUNK_BITS;

/// The most chunks a column's directory holds, so the most rows it
/// holds is `CHUNKS * CHUNK`, eight million: past the classes and
/// methods any program publishes into one epoch, and past the map
/// pointers the engine numbers op arrays by (`tier::published::Funcs`),
/// which every publication of an epoch advances, a unit published again
/// after its file changed and a trait's copied method among them; a
/// column whose numbers could name more is held to it
/// ([`Column::below`]). The directory of such a column is laid whole
/// where the column is made, 16 KiB a thousand chunks.
const CHUNKS: usize = 1 << 14;

/// One chunk of a column, laid down on first use and never moved: a
/// fixed-length array, so a number's slot in it is its low bits with no
/// bound to test.
type Chunk<T> = Box<[OnceLock<T>; CHUNK]>;

/// One kind of row of the arena, numbered from zero in the order its
/// numbers were reserved.
pub(crate) struct Column<T> {
    /// The chunks, as many as the limit needs, each laid on first use.
    chunks: Box<[OnceLock<Chunk<T>>]>,
    /// How many numbers were reserved, which is the next one's.
    reserved: AtomicU32,
    /// The number no row reaches.
    limit: u32,
}

impl<T> Column<T> {
    /// An empty column whose numbers stay below `limit`, and below the
    /// rows its directory holds.
    pub(crate) fn below(limit: u32) -> Column<T> {
        let held = u32::try_from(CHUNKS * CHUNK).unwrap_or(u32::MAX);
        let limit = limit.min(held);
        let chunks = usize::try_from(limit).map_or(CHUNKS, |limit| limit.div_ceil(CHUNK));
        Column {
            chunks: (0..chunks).map(|_| OnceLock::new()).collect(),
            reserved: AtomicU32::new(0),
            limit,
        }
    }

    /// `count` numbers taken together, the first answered: theirs alone
    /// whichever thread reserves beside this one. None where the column
    /// has no room for them.
    pub(crate) fn reserve(&self, count: u32) -> Option<u32> {
        self.reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(count).filter(|end| *end <= self.limit)
            })
            .ok()
    }

    /// The row a reserved number names, written once: `Err` gives the row
    /// back where the number was not reserved or its row was filled.
    pub(crate) fn fill(&self, at: u32, row: T) -> Result<(), T> {
        if at >= self.reserved() {
            return Err(row);
        }
        let Some(laid) = self.chunks.get((at >> CHUNK_BITS) as usize) else {
            return Err(row);
        };
        let chunk = if let Some(chunk) = laid.get() {
            chunk
        } else {
            // Made through a `Vec` so the chunk is laid on the heap and
            // never built on the stack; its length is `CHUNK`, so the
            // conversion holds, and the row is handed back rather than a
            // panic where it would not. Made before it is offered: a
            // thread that laid the chunk first keeps its own, and this
            // one is dropped.
            let slots: Vec<OnceLock<T>> = (0..CHUNK).map(|_| OnceLock::new()).collect();
            let Ok(fresh) = Chunk::try_from(slots) else {
                return Err(row);
            };
            laid.get_or_init(|| fresh)
        };
        chunk[at as usize & (CHUNK - 1)].set(row)
    }

    /// The row at a number its publisher chose rather than reserved,
    /// written once: every number below it counts as reserved, its row
    /// unfilled where no publisher places one. `Err` gives the row back
    /// past the limit and where the slot was filled.
    pub(crate) fn place(&self, at: u32, row: T) -> Result<(), T> {
        let Some(end) = at.checked_add(1).filter(|end| *end <= self.limit) else {
            return Err(row);
        };
        self.reserved.fetch_max(end, Ordering::AcqRel);
        self.fill(at, row)
    }

    /// The row at `at`, where it was filled.
    pub(crate) fn get(&self, at: u32) -> Option<&T> {
        let chunk = self.chunks.get((at >> CHUNK_BITS) as usize)?.get()?;
        chunk[at as usize & (CHUNK - 1)].get()
    }

    /// How many numbers were reserved: every row a request can have been
    /// handed stands below it.
    pub(crate) fn reserved(&self) -> u32 {
        self.reserved.load(Ordering::Acquire)
    }
}

/// The arena of one epoch: the classes, the user functions and methods,
/// and the natives a class lays under its own scope published past the
/// engine's own and the preload's, each numbered past what is numbered
/// below them, with the caches that name them.
pub(crate) struct Arena {
    /// The preload's arena, which every epoch after it is laid over and
    /// shares at the same numbers ([`Epochs::lay_base`]); none in the
    /// preload's own and where nothing was preloaded.
    below: Option<Arc<Arena>>,
    classes: Column<Declared>,
    functions: Column<UserFunction>,
    class_natives: Column<(ClassId, ClassNative)>,
    /// The classes below, the engine's and the preload's, which the
    /// arena's are numbered past.
    first_class: u32,
    /// The functions below, the engine's natives and native methods and
    /// the preload's user functions, which the arena's are numbered past.
    first_function: u32,
    /// The preload's class natives, which the arena's are numbered past.
    first_class_native: u32,
    /// php's inheritance cache over the classes published here, dropped
    /// with them ([`crate::inheritance_cache`]).
    pub(crate) inheritance: crate::inheritance_cache::InheritanceCache,
    /// The unit cache's files, whose early-bound classes and functions
    /// are rows here, php's `ZCSG(hash)` and what is counted of the
    /// memory under it ([`crate::unit_cache::Files`]).
    pub(crate) files: crate::unit_cache::Files,
    /// The engine's own classes' linked records, which every epoch
    /// shares, so what holds an epoch reads a class of the engine's as it
    /// reads one the epoch published ([`Arena::linked_class`]).
    natives: Arc<[LinkedClass<ClassId>]>,
    /// The compiled tier's value for the epoch, its trace table
    /// (`jit-design.md` §4.5): made by the tier the first time it asks,
    /// read by every request of the epoch with nothing taken, and
    /// dropped with the epoch, so a restart's epoch starts with none
    /// ([`crate::tier::epoch_state`]).
    pub(crate) tier: OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// The engine's interned strings, which a held unit's constants and
    /// names and the engine's classes' names are: kept by the epoch so
    /// what holds it reads them after the engine that made them is
    /// dropped. Last, so the units go before the strings they name.
    interned: Interned,
}

/// The engine's two tables of interned strings (design §5.2): the
/// permanent names its startup interned, frozen after, and the table a
/// unit's constant strings enter as it is published, grown by one
/// publisher at a time. Shared by the engine and every epoch it hands out, so neither
/// outlives the strings the other's units name.
#[derive(Clone, Default)]
pub(crate) struct Interned {
    pub(crate) names: Arc<php_heap::Strings>,
    pub(crate) strings: Arc<Mutex<php_heap::Strings>>,
}

impl Interned {
    /// The table a unit's constant strings enter, its one publisher at a
    /// time. The table is append-only and every string in it was
    /// finished before it was published, so what a thread that panicked
    /// holding the `Mutex` left is consistent, and a poisoned guard is
    /// taken as it stands.
    pub(crate) fn strings(&self) -> MutexGuard<'_, php_heap::Strings> {
        self.strings.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Arena {
    /// An empty arena over an engine of `classes` classes and
    /// `functions` natives and native methods, holding `files`, for a
    /// test that links no class.
    #[cfg(test)]
    pub(crate) fn past(classes: usize, functions: usize, files: crate::unit_cache::Files) -> Arena {
        let first_class = u32::try_from(classes).unwrap_or(ClassId::LOCAL);
        let first_function = u32::try_from(functions).unwrap_or(Function::CLASS_NATIVE);
        let natives = Arc::from([]);
        Arena::numbered(
            None,
            [first_class, first_function, 0],
            files,
            natives,
            Interned::default(),
        )
    }

    /// An empty arena over an engine whose classes linked to `natives`,
    /// past them and past `functions` natives and native methods,
    /// holding `files` and keeping `interned`.
    pub(crate) fn of_engine(
        natives: Arc<[LinkedClass<ClassId>]>,
        functions: usize,
        files: crate::unit_cache::Files,
        interned: Interned,
    ) -> Arena {
        let first_class = u32::try_from(natives.len()).unwrap_or(ClassId::LOCAL);
        let first_function = u32::try_from(functions).unwrap_or(Function::CLASS_NATIVE);
        Arena::numbered(
            None,
            [first_class, first_function, 0],
            files,
            natives,
            interned,
        )
    }

    /// An empty arena numbered from `first` (its classes, functions and
    /// class natives) over `below`, holding `files`.
    fn numbered(
        below: Option<Arc<Arena>>,
        first: [u32; 3],
        files: crate::unit_cache::Files,
        natives: Arc<[LinkedClass<ClassId>]>,
        interned: Interned,
    ) -> Arena {
        let [first_class, first_function, first_class_native] = first;
        Arena {
            below,
            classes: Column::below(ClassId::LOCAL.saturating_sub(first_class)),
            functions: Column::below(Function::CLASS_NATIVE.saturating_sub(first_function)),
            class_natives: Column::below(Function::CLASS_NATIVE.saturating_sub(first_class_native)),
            first_class,
            first_function,
            first_class_native,
            inheritance: crate::inheritance_cache::InheritanceCache::default(),
            files,
            natives,
            tier: OnceLock::new(),
            interned,
        }
    }

    /// An empty arena laid over `base`, which is laid over nothing,
    /// numbered past every number it reserved.
    fn over(base: Arc<Arena>) -> Arena {
        let [classes, functions, class_natives] = base.counts();
        let first = [
            base.first_class.saturating_add(classes),
            base.first_function.saturating_add(functions),
            base.first_class_native.saturating_add(class_natives),
        ];
        let files = base.files.emptied();
        let natives = Arc::clone(&base.natives);
        let interned = base.interned.clone();
        Arena::numbered(Some(base), first, files, natives, interned)
    }

    /// The arena a restart hands on: empty, over what this one is laid
    /// over and numbered as it is, which no request of this epoch sees.
    fn successor(&self) -> Arena {
        let first = [
            self.first_class,
            self.first_function,
            self.first_class_native,
        ];
        let natives = Arc::clone(&self.natives);
        let interned = self.interned.clone();
        Arena::numbered(
            self.below.clone(),
            first,
            self.files.emptied(),
            natives,
            interned,
        )
    }

    /// The arena a number below this one's first names a row of: the
    /// base, which is laid over nothing ([`Epochs::lay_base`]), so a
    /// lookup goes down one arena at most.
    fn base(&self) -> Option<&Arena> {
        self.below.as_deref()
    }

    /// The published class `class` names; none for the engine's own and
    /// for a request's.
    pub(crate) fn class(&self, class: ClassId) -> Option<&Declared> {
        let number = class.published_number()?;
        if let Some(at) = number.checked_sub(self.first_class) {
            return self.classes.get(at);
        }
        let base = self.base()?;
        base.classes.get(number.checked_sub(base.first_class)?)
    }

    /// The linked record of a class every request shares: one of the
    /// engine's, or one the epoch published; none for a request's.
    pub(crate) fn linked_class(&self, class: ClassId) -> Option<&LinkedClass<ClassId>> {
        let number = usize::try_from(class.published_number()?).ok()?;
        match self.natives.get(number) {
            Some(native) => Some(native),
            None => self.class(class)?.record(),
        }
    }

    /// The held op array at engine map-pointer offset `offset`, this
    /// epoch's or the preload's below it ([`crate::tier::published`]).
    pub(crate) fn func(&self, offset: u32) -> Option<&crate::tier::published::Func> {
        self.files
            .funcs
            .get(offset)
            .or_else(|| self.base()?.files.funcs.get(offset))
    }

    /// The published user function or method `function` names; none for
    /// a native and for a request's.
    pub(crate) fn function(&self, function: Function) -> Option<&UserFunction> {
        let number = function.published_number()?;
        if let Some(at) = number.checked_sub(self.first_function) {
            return self.functions.get(at);
        }
        let base = self.base()?;
        base.functions.get(number.checked_sub(base.first_function)?)
    }

    /// The published class native `function` names, and its class.
    pub(crate) fn class_native(&self, function: Function) -> Option<(ClassId, ClassNative)> {
        let at = function.published_class_native_at()?;
        if let Some(own) = at.checked_sub(self.first_class_native) {
            return self.class_natives.get(own).copied();
        }
        let base = self.base()?;
        let row = base
            .class_natives
            .get(at.checked_sub(base.first_class_native)?);
        row.copied()
    }

    /// The number of a class to publish, its row to [`Arena::publish_class`].
    pub(crate) fn reserve_class(&self) -> Option<ClassId> {
        let at = self.classes.reserve(1)?;
        Some(ClassId(self.first_class.checked_add(at)?))
    }

    /// The numbers of `count` user functions to publish, in order.
    pub(crate) fn reserve_functions(&self, count: u32) -> Option<std::ops::Range<u32>> {
        let at = self.functions.reserve(count)?;
        let first = self.first_function.checked_add(at)?;
        Some(first..first.checked_add(count)?)
    }

    /// The number of a class native to publish.
    pub(crate) fn reserve_class_native(&self) -> Option<Function> {
        let at = self.class_natives.reserve(1)?;
        Function::published_class_native(self.first_class_native.checked_add(at)?)
    }

    /// The row of a reserved class, written once; `false` where the
    /// number was not reserved or its row was written.
    pub(crate) fn publish_class(&self, class: ClassId, row: Declared) -> bool {
        let Some(at) = class
            .published_number()
            .and_then(|number| number.checked_sub(self.first_class))
        else {
            return false;
        };
        self.classes.fill(at, row).is_ok()
    }

    /// The row of a reserved user function, written once.
    pub(crate) fn publish_function(&self, function: Function, row: UserFunction) -> bool {
        let Some(at) = function
            .published_number()
            .and_then(|number| number.checked_sub(self.first_function))
        else {
            return false;
        };
        self.functions.fill(at, row).is_ok()
    }

    /// The row of a reserved class native, written once.
    pub(crate) fn publish_class_native(
        &self,
        function: Function,
        row: (ClassId, ClassNative),
    ) -> bool {
        let Some(at) = function
            .published_class_native_at()
            .and_then(|at| at.checked_sub(self.first_class_native))
        else {
            return false;
        };
        self.class_natives.fill(at, row).is_ok()
    }

    /// How many classes, functions and class natives this arena reserved,
    /// what is below it not among them.
    pub(crate) fn counts(&self) -> [u32; 3] {
        [
            self.classes.reserved(),
            self.functions.reserved(),
            self.class_natives.reserved(),
        ]
    }
}

/// The arena's epochs (design §5.14, *Identities*): the arena a request
/// starting now is handed, which it holds until it ends. A restart
/// (`accel_activate`'s, under `ZCSG(restart_pending)`) replaces it at the
/// next request's start with an empty one over the same base, however
/// many requests of the old epoch are still served; the old arena, its
/// rows and its caches go with the last of them. php waits instead for a
/// moment no process is inside (`accel_is_inactive`) and kills the ones
/// that hold it past `opcache.force_restart_timeout`; a thread cannot be
/// killed, and an epoch per request makes the wait needless.
pub(crate) struct Epochs {
    current: Mutex<Arc<Arena>>,
}

impl Epochs {
    pub(crate) fn of(arena: Arena) -> Epochs {
        Epochs {
            current: Mutex::new(Arc::new(arena)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Arc<Arena>> {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The arena of the epoch a request starting now would enter, for
    /// what reads or fills the cache outside a request; no restart is
    /// made here.
    pub(crate) fn current(&self) -> Arc<Arena> {
        Arc::clone(&self.lock())
    }

    /// The arena for a request starting now: where `restart` finds a
    /// restart scheduled for the current one (and counts it), a new
    /// arena in its place, both while no other request can start, so
    /// one restart replaces an epoch once.
    pub(crate) fn enter(&self, restart: impl FnOnce(&Arena) -> bool) -> Arc<Arena> {
        let mut current = self.lock();
        if restart(&current) {
            let next = current.successor();
            *current = Arc::new(next);
        }
        Arc::clone(&current)
    }

    /// `zend_shared_alloc_save_state` after preloading: the preload's
    /// `arena`, which it has published into, laid below every epoch from
    /// now on; the first laid stands, and an arena laid over one is never
    /// laid. The preload is served before any other request, and
    /// publishes nothing into it after; a row it did would be numbered
    /// among the next epoch's own and reached by no request of it.
    pub(crate) fn lay_base(&self, arena: &Arc<Arena>) {
        let mut current = self.lock();
        if arena.below.is_some() || current.below.is_some() {
            return;
        }
        *current = Arc::new(Arena::over(Arc::clone(arena)));
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::Arc;

    use super::{Arena, CHUNK, CHUNKS, Column, Epochs};
    use crate::api::call::ClassNative;
    use crate::api::values::ClassId;
    use crate::enums::EnumMethod;

    /// The preload's rows laid below every epoch: each epoch numbers its
    /// own past them and reads them at their numbers, and a restart
    /// hands on an empty epoch that reads them still.
    #[test]
    fn the_preloads_rows_keep_their_numbers_across_restarts() {
        let epochs = Epochs::of(Arena::past(10, 20, crate::unit_cache::Files::new(16)));
        let preload = epochs.enter(|_| false);
        assert_eq!(preload.reserve_class(), Some(ClassId(10)));
        let method = preload.reserve_class_native();
        let row = (ClassId(10), ClassNative::Enum(EnumMethod::Cases));
        assert!(method.is_some_and(|method| preload.publish_class_native(method, row)));
        epochs.lay_base(&preload);
        let first = epochs.enter(|_| false);
        assert_eq!(
            first.reserve_class(),
            Some(ClassId(11)),
            "numbered past the preload's"
        );
        let own = first.reserve_class_native();
        let mine = (ClassId(11), ClassNative::Enum(EnumMethod::From));
        assert!(own.is_some_and(|own| first.publish_class_native(own, mine)));
        assert_ne!(own, method);
        let second = epochs.enter(|_| true);
        assert!(!Arc::ptr_eq(&first, &second), "a new epoch");
        assert_eq!(second.counts(), [0, 0, 0]);
        for epoch in [&first, &second] {
            assert_eq!(
                method.and_then(|method| epoch.class_native(method)),
                Some(row)
            );
        }
        assert_eq!(own.and_then(|own| first.class_native(own)), Some(mine));
        assert_eq!(
            own.and_then(|own| second.class_native(own)),
            None,
            "not the new epoch's"
        );
        assert_eq!(second.reserve_class(), Some(ClassId(11)));
        assert!(Arc::ptr_eq(&second, &epochs.enter(|_| false)));
    }

    /// The tier's value is the epoch's (`jit-design.md` §4.5): what the
    /// tier made for one epoch every request of it reads, a restart's
    /// epoch starts with none, and the old value goes with the old epoch.
    #[test]
    fn a_restarts_epoch_starts_with_no_tier_value() {
        let epochs = Epochs::of(Arena::past(0, 0, crate::unit_cache::Files::new(16)));
        let first = epochs.enter(|_| false);
        let made = first.tier.get_or_init(|| Box::new(Arc::new(7_u32)));
        let held = made.downcast_ref::<Arc<u32>>().map(Arc::downgrade);
        assert!(Arc::ptr_eq(&first, &epochs.enter(|_| false)));
        let second = epochs.enter(|_| true);
        assert!(second.tier.get().is_none(), "a new epoch's has none");
        assert!(held.as_ref().is_some_and(|held| held.upgrade().is_some()));
        drop(first);
        assert!(
            held.is_some_and(|held| held.upgrade().is_none()),
            "gone with its epoch"
        );
    }

    /// A number's low bits are its slot in a chunk and its high bits the
    /// chunk's: rows on either side of a chunk's edge are each their own,
    /// a column is held to the rows its directory holds, and a number
    /// past the directory names nothing.
    #[test]
    fn each_number_has_its_own_slot_across_the_chunks() {
        let column: Column<u32> = Column::below(u32::MAX);
        let held = u32::try_from(CHUNKS * CHUNK).unwrap_or(u32::MAX);
        assert_eq!(column.limit, held, "held to the directory");
        let edge = u32::try_from(CHUNK).unwrap_or(u32::MAX);
        assert_eq!(column.reserve(edge + 1), Some(0));
        for at in [0, edge - 1, edge] {
            assert_eq!(column.fill(at, at + 7), Ok(()));
        }
        for at in [0, edge - 1, edge] {
            assert_eq!(column.get(at), Some(&(at + 7)));
        }
        assert_eq!(column.get(1), None, "reserved, never filled");
        assert_eq!(column.get(held), None);
        assert_eq!(column.get(u32::MAX), None);
        let small: Column<u32> = Column::below(10);
        assert_eq!(small.chunks.len(), 1, "as many chunks as the limit needs");
    }

    #[test]
    fn a_row_is_found_once_filled_and_never_written_again() {
        let column: Column<u64> = Column::below(1000);
        let at = column.reserve(3).unwrap_or(u32::MAX);
        assert_eq!(at, 0);
        assert_eq!(column.get(1), None);
        assert_eq!(column.fill(1, 11), Ok(()));
        assert_eq!(column.get(1), Some(&11));
        assert_eq!(column.fill(1, 12), Err(12));
        assert_eq!(column.fill(3, 13), Err(13), "not reserved");
        assert_eq!(column.reserve(998), None, "past the limit");
        assert_eq!(column.reserved(), 3);
    }

    /// Two threads reserving and filling at once: each number is one
    /// thread's, each row is what that thread filled, and a row read
    /// while the other thread grows the column stays where it was.
    #[test]
    fn rows_filled_on_two_threads_stay_where_they_were_filled() {
        let column: Column<(u8, u32)> = Column::below(1 << 20);
        let taken: Vec<Vec<u32>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0u8..2)
                .map(|thread| {
                    let column = &column;
                    scope.spawn(move || {
                        let mut mine = Vec::new();
                        for n in 0..5000 {
                            let Some(at) = column.reserve(1) else { break };
                            assert_eq!(column.fill(at, (thread, n)), Ok(()));
                            mine.push(at);
                            let first = mine.first().copied().unwrap_or(0);
                            assert_eq!(column.get(first), Some(&(thread, 0)));
                        }
                        mine
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("a worker runs to its end"))
                .collect()
        });
        for (thread, numbers) in (0u8..).zip(&taken) {
            for (n, at) in (0u32..).zip(numbers) {
                assert_eq!(column.get(*at), Some(&(thread, n)));
            }
        }
        assert_eq!(column.reserved(), 10_000);
    }
}
