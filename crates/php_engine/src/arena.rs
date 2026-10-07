//! The engine's published arena (design §5.14, *Identities*): the rows
//! every request shares of the classes and functions published past the
//! engine's own, the preload's first. A [`ClassId`] or a [`Function`]
//! whose top bit is clear names one of these rows, or one of the
//! engine's below them; one whose top bit is set names a row of the
//! request's own.
//!
//! Each kind of row is a [`Column`]: segments that each double the one
//! before, laid down on first use and never moved, of slots that are
//! each written once. A publisher takes a number with [`Column::reserve`]
//! before it builds the row, so rows that name each other (a class and
//! its methods) are built knowing each other's numbers, and fills it
//! with [`Column::fill`]. A request reads a row with [`Column::get`]: two
//! acquire loads and nothing taken, as php reads a `zend_class_entry *` from
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
//! An opcache restart is the one thing that takes rows back ([`Epochs`]):
//! once no request holds the arena, each column is cut back to the rows
//! it kept, the preload's, which php keeps across a restart
//! (`zend_shared_alloc_save_state` after preloading, restored to by
//! `accel_activate`'s restart), and every number past them is free to
//! be reserved again. Each request holds the arena it started in, so no
//! number a live request holds is ever reused.
//!
//! Nothing here is `unsafe`: the slots are `OnceLock`s, so a row is
//! `Sync` where its type is, and the segments never reallocate, so a
//! row borrowed from the arena lives as long as the arena.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::api::call::Function;
use crate::api::request::{Declared, UserFunction};
use crate::api::values::ClassId;
use crate::enums::EnumMethod;

/// How many slots a column's first segment holds; each segment after
/// holds twice the one before.
const FIRST: usize = 64;

/// How many segments a column has room for: 24 hold
/// `FIRST * ((1 << 24) - 1)` rows, past the `1 << 29` a function's
/// number can name; a class's could name more, and its column is held
/// to what the segments hold ([`Column::below`]).
const SEGMENTS: usize = 24;

/// One kind of row of the arena, numbered from zero in the order its
/// numbers were reserved.
pub(crate) struct Column<T> {
    segments: [OnceLock<Box<[OnceLock<T>]>>; SEGMENTS],
    /// How many numbers were reserved, which is the next one's.
    reserved: AtomicU32,
    /// How many rows a restart keeps ([`Column::keep`]).
    kept: AtomicU32,
    /// The number no row reaches.
    limit: u32,
}

/// The segment and the slot in it of the row at `at`: segment `k`
/// holds the `FIRST << k` rows from `FIRST * ((1 << k) - 1)` on.
fn locate(at: u32) -> Option<(usize, usize)> {
    let biased = usize::try_from(at).ok()?.checked_add(FIRST)?;
    let top = usize::try_from(usize::BITS - 1 - biased.leading_zeros()).ok()?;
    let segment = top.checked_sub(usize::try_from(FIRST.trailing_zeros()).ok()?)?;
    let offset = biased.checked_sub(FIRST.checked_shl(u32::try_from(segment).ok()?)?)?;
    Some((segment, offset))
}

impl<T> Column<T> {
    /// An empty column whose numbers stay below `limit`, and below the
    /// rows its segments hold.
    fn below(limit: u32) -> Column<T> {
        let held = FIRST.saturating_mul((1 << SEGMENTS) - 1);
        let held = u32::try_from(held).unwrap_or(u32::MAX);
        Column {
            segments: std::array::from_fn(|_| OnceLock::new()),
            reserved: AtomicU32::new(0),
            kept: AtomicU32::new(0),
            limit: limit.min(held),
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
        let Some((segment, offset)) = locate(at) else {
            return Err(row);
        };
        let Some(segment) = self.segments.get(segment).map(|laid| {
            laid.get_or_init(|| {
                let slots = FIRST << segment;
                (0..slots).map(|_| OnceLock::new()).collect()
            })
        }) else {
            return Err(row);
        };
        match segment.get(offset) {
            Some(slot) => slot.set(row),
            None => Err(row),
        }
    }

    /// The row at `at`, where it was filled.
    pub(crate) fn get(&self, at: u32) -> Option<&T> {
        let (segment, offset) = locate(at)?;
        self.segments.get(segment)?.get()?.get(offset)?.get()
    }

    /// How many numbers were reserved: every row a request can have been
    /// handed stands below it.
    pub(crate) fn reserved(&self) -> u32 {
        self.reserved.load(Ordering::Acquire)
    }

    /// Every number reserved so far kept by a restart.
    fn keep(&self) {
        self.kept.store(self.reserved(), Ordering::Release);
    }

    /// The rows past the kept ones emptied, and their numbers free to
    /// be reserved again: what only the arena's one holder may do, so
    /// nothing reads a row as it goes. The segments stay laid down.
    fn truncate(&mut self) {
        let kept = *self.kept.get_mut();
        let reserved = *self.reserved.get_mut();
        for at in kept..reserved {
            let slot = locate(at).and_then(|(segment, offset)| {
                self.segments.get_mut(segment)?.get_mut()?.get_mut(offset)
            });
            if let Some(slot) = slot {
                slot.take();
            }
        }
        *self.reserved.get_mut() = kept;
    }
}

/// The arena: the classes, the user functions and methods, and the
/// enums' own `cases`, `from` and `tryFrom` published past the engine's
/// own, each numbered past what the engine numbers below them.
pub(crate) struct Arena {
    classes: Column<Declared>,
    functions: Column<UserFunction>,
    enum_methods: Column<(ClassId, EnumMethod)>,
    /// The engine's classes, which the arena's are numbered past.
    first_class: u32,
    /// The engine's natives and native methods, which the arena's user
    /// functions are numbered past.
    first_function: u32,
}

impl Arena {
    /// An empty arena over an engine of `classes` classes and
    /// `functions` natives and native methods.
    pub(crate) fn past(classes: usize, functions: usize) -> Arena {
        let first_class = u32::try_from(classes).unwrap_or(ClassId::LOCAL);
        let first_function = u32::try_from(functions).unwrap_or(Function::ENUM);
        Arena {
            classes: Column::below(ClassId::LOCAL.saturating_sub(first_class)),
            functions: Column::below(Function::ENUM.saturating_sub(first_function)),
            enum_methods: Column::below(Function::ENUM),
            first_class,
            first_function,
        }
    }

    /// The published class `class` names; none for the engine's own and
    /// for a request's.
    pub(crate) fn class(&self, class: ClassId) -> Option<&Declared> {
        self.classes
            .get(class.published_number()?.checked_sub(self.first_class)?)
    }

    /// The published user function or method `function` names; none for
    /// a native and for a request's.
    pub(crate) fn function(&self, function: Function) -> Option<&UserFunction> {
        self.functions.get(
            function
                .published_number()?
                .checked_sub(self.first_function)?,
        )
    }

    /// The published enum's method `function` names, and its enum.
    pub(crate) fn enum_method(&self, function: Function) -> Option<(ClassId, EnumMethod)> {
        self.enum_methods
            .get(function.published_enum_at()?)
            .copied()
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

    /// The number of an enum's method to publish.
    pub(crate) fn reserve_enum_method(&self) -> Option<Function> {
        Function::published_enum(self.enum_methods.reserve(1)?)
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

    /// The row of a reserved enum method, written once.
    pub(crate) fn publish_enum_method(
        &self,
        function: Function,
        row: (ClassId, EnumMethod),
    ) -> bool {
        let Some(at) = function.published_enum_at() else {
            return false;
        };
        self.enum_methods.fill(at, row).is_ok()
    }

    /// Every row published so far kept by a restart: the preload's, once
    /// it has published.
    pub(crate) fn keep(&self) {
        self.classes.keep();
        self.functions.keep();
        self.enum_methods.keep();
    }

    /// The arena as a restart leaves it: the kept rows alone.
    fn truncate(&mut self) {
        self.classes.truncate();
        self.functions.truncate();
        self.enum_methods.truncate();
    }

    /// How many classes, functions and enum methods are reserved.
    #[cfg(test)]
    pub(crate) fn counts(&self) -> [u32; 3] {
        [
            self.classes.reserved(),
            self.functions.reserved(),
            self.enum_methods.reserved(),
        ]
    }
}

/// The arena's epochs (design §5.14, *Identities*): the arena a request
/// starting now is handed, which it holds until it ends. A restart
/// (`accel_activate`'s, under `ZCSG(restart_pending)`) is made only once
/// no request holds the arena, `accel_is_inactive`, and empties it back
/// to the rows it kept; while one does, the restart stays pending and
/// is tried again at the next request's start.
pub(crate) struct Epochs {
    current: Mutex<Arc<Arena>>,
}

impl Epochs {
    pub(crate) fn of(arena: Arena) -> Epochs {
        Epochs {
            current: Mutex::new(Arc::new(arena)),
        }
    }

    /// The arena for a request starting now: where `pending` asks for a
    /// restart and no request holds the arena, the arena emptied back to
    /// the rows it kept and `restart` called, both while no other
    /// request can start.
    pub(crate) fn enter(
        &self,
        pending: impl FnOnce() -> bool,
        restart: impl FnOnce(),
    ) -> Arc<Arena> {
        let mut current = match self.current.lock() {
            Ok(current) => current,
            Err(poisoned) => poisoned.into_inner(),
        };
        if pending()
            && let Some(arena) = Arc::get_mut(&mut current)
        {
            arena.truncate();
            restart();
        }
        Arc::clone(&current)
    }
}

#[cfg(test)]
mod tests {
    use super::{Column, FIRST, locate};

    #[test]
    fn a_truncated_column_keeps_its_kept_rows_and_numbers_the_rest_again() {
        let mut column: Column<u32> = Column::below(1 << 20);
        let first = column.reserve(70).unwrap_or(u32::MAX);
        for at in first..70 {
            assert_eq!(column.fill(at, at), Ok(()));
        }
        column.keep();
        let past = column.reserve(100).unwrap_or(u32::MAX);
        assert_eq!(column.fill(past, 7), Ok(()));
        assert_eq!(column.fill(past + 99, 9), Ok(()));
        column.truncate();
        assert_eq!(column.reserved(), 70);
        assert_eq!(column.get(69), Some(&69), "a kept row stays");
        assert_eq!(column.get(past), None, "a row past them goes");
        assert_eq!(column.reserve(1), Some(70), "its number is reserved again");
        assert_eq!(column.fill(70, 1), Ok(()));
        assert_eq!(column.get(70), Some(&1));
    }

    #[test]
    fn each_number_has_its_own_slot_and_segments_double() {
        assert_eq!(locate(0), Some((0, 0)));
        assert_eq!(locate(63), Some((0, 63)));
        assert_eq!(locate(64), Some((1, 0)));
        assert_eq!(locate(191), Some((1, 127)));
        assert_eq!(locate(192), Some((2, 0)));
        let last = (1u32 << 29) - 1;
        let (segment, offset) = locate(last).unwrap_or((usize::MAX, 0));
        assert!(segment < super::SEGMENTS);
        assert!(offset < FIRST << segment);
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
                .map(|worker| worker.join().unwrap_or_default())
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
