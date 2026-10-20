//! The objects of a foreign library the request keeps for an extension
//! (#2277, #2313, ADR-0013): a C library's converter, connection or
//! parser that an object's payload names and a call into the library
//! may need past PHP code freeing it. The request owns the object, the
//! payload owns a [`Foreign`] naming it, and [`Memory::holding`] defers
//! its drop to the end of the call that holds it. The request's end
//! drops whatever remains. The table is this file's and no other file
//! reads its fields.

use std::any::{Any, TypeId};
use std::marker::PhantomData;

use super::{Memory, Request};

/// The one owner of an object the request keeps ([`Memory::adopt`]):
/// what a payload stores. Neither `Copy` nor `Clone`, so the object is
/// given up in one place, [`Memory::disown`], which hands it back to be
/// closed; one dropped without that leaves the object to the request's
/// end.
#[must_use = "an adopted object is given up with Request::disown, or stays until the request ends"]
#[derive(Debug)]
pub struct Foreign<T> {
    id: ForeignId<T>,
}

impl<T> Foreign<T> {
    /// The name the object is reached and held by.
    #[must_use]
    pub fn id(&self) -> ForeignId<T> {
        self.id
    }
}

/// The name of an object the request keeps, copied freely: it reaches
/// the object while its owner has not given it up, or while a call holds
/// it, and nothing after.
pub struct ForeignId<T> {
    at: u32,
    generation: u32,
    kind: PhantomData<fn() -> T>,
}

impl<T> Clone for ForeignId<T> {
    fn clone(&self) -> ForeignId<T> {
        *self
    }
}

impl<T> Copy for ForeignId<T> {}

impl<T> PartialEq for ForeignId<T> {
    fn eq(&self, other: &ForeignId<T>) -> bool {
        (self.at, self.generation) == (other.at, other.generation)
    }
}

impl<T> Eq for ForeignId<T> {}

impl<T> std::fmt::Debug for ForeignId<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ForeignId({}#{})", self.at, self.generation)
    }
}

/// The span of one [`Memory::holding`]: while it lasts, the object it
/// names is not dropped, whatever the PHP code it serves gives up. The
/// lifetime is the span's own, so nothing made from a hold outlives it:
///
/// ```compile_fail,E0521
/// use php_engine::api::request::{ForeignId, Held, Request};
/// fn escape(req: &mut Request, id: ForeignId<u8>) {
///     let mut kept = None;
///     req.memory_mut().holding(id, |_, held| kept = Some(held));
/// }
/// ```
pub struct Held<'h, T> {
    id: ForeignId<T>,
    span: PhantomData<fn(&'h ()) -> &'h ()>,
}

impl<T: Send + 'static> Held<'_, T> {
    /// The object held, through the request: there for the whole span,
    /// including after its owner gave it up inside it.
    pub fn get<'r>(&self, req: &'r mut Request) -> Option<&'r mut T> {
        req.foreign.reach(self.id, true)
    }
}

/// One row of the table: the object, the holds on it, and whether its
/// owner gave it up while one stood.
struct Row {
    generation: u32,
    value: Option<Box<dyn Any + Send>>,
    kind: TypeId,
    holds: u32,
    disowned: bool,
}

/// The objects the request keeps, their rows reused under a new
/// generation once freed.
#[derive(Default)]
pub(super) struct Table {
    rows: Vec<Row>,
    free: Vec<u32>,
}

impl Table {
    fn adopt<T: Send + 'static>(&mut self, value: T) -> ForeignId<T> {
        let value: Box<dyn Any + Send> = Box::new(value);
        let kind = TypeId::of::<T>();
        let at = self.free.pop().unwrap_or_else(|| {
            self.rows.push(Row {
                generation: 0,
                value: None,
                kind,
                holds: 0,
                disowned: false,
            });
            u32::try_from(self.rows.len() - 1).unwrap_or(u32::MAX)
        });
        let generation = match self.rows.get_mut(at as usize) {
            Some(row) => {
                (row.value, row.kind, row.holds, row.disowned) = (Some(value), kind, 0, false);
                row.generation
            }
            None => 0,
        };
        ForeignId {
            at,
            generation,
            kind: PhantomData,
        }
    }

    /// The row `id` names, while its object stands and is a `T`.
    fn row<T: 'static>(&mut self, id: ForeignId<T>) -> Option<&mut Row> {
        let row = self.rows.get_mut(id.at as usize)?;
        let live = row.generation == id.generation && row.value.is_some();
        (live && row.kind == TypeId::of::<T>()).then_some(row)
    }

    /// The object, to its owner while not given up, and to a hold
    /// (`held`) until the hold ends.
    fn reach<T: Send + 'static>(&mut self, id: ForeignId<T>, held: bool) -> Option<&mut T> {
        let row = self.row(id)?;
        if row.disowned && !held {
            return None;
        }
        row.value.as_mut()?.downcast_mut::<T>()
    }

    /// One more hold of `id`, where its owner has not given it up.
    fn hold<T: 'static>(&mut self, id: ForeignId<T>) -> bool {
        let row = self.row(id).filter(|row| !row.disowned);
        row.map(|row| row.holds += 1).is_some()
    }

    /// One hold of `id` ended: the object dropped where its owner gave it
    /// up meanwhile and no other hold stands.
    fn end_hold<T: 'static>(&mut self, id: ForeignId<T>) {
        let Some(row) = self.row(id) else {
            return;
        };
        row.holds = row.holds.saturating_sub(1);
        if row.holds == 0 && row.disowned {
            self.free(id.at);
        }
    }

    /// `id` given up by its owner: its object handed back where no hold
    /// stands, kept for the last hold to drop otherwise.
    fn disown<T: 'static>(&mut self, id: ForeignId<T>) -> Option<T> {
        let row = self.row(id)?;
        if row.holds > 0 {
            row.disowned = true;
            return None;
        }
        let value = self.vacate(id.at)?;
        value.downcast::<T>().ok().map(|value| *value)
    }

    /// The row's object dropped, once the table is consistent again.
    fn free(&mut self, at: u32) {
        drop(self.vacate(at));
    }

    /// The row's object taken out and the row reused under its next
    /// generation.
    fn vacate(&mut self, at: u32) -> Option<Box<dyn Any + Send>> {
        let row = self.rows.get_mut(at as usize)?;
        let value = row.value.take();
        row.generation = row.generation.wrapping_add(1);
        row.disowned = false;
        self.free.push(at);
        value
    }
}

impl<'r> Memory<&'r mut Request> {
    /// `value` kept by the request until its owner, the answer, gives it
    /// up and no call holds it, or until the request ends. What an
    /// object's payload stores in place of a C library's pointer, when a
    /// call into the library can serve PHP code that frees the payload's.
    pub fn adopt<T: Send + 'static>(self, value: T) -> Foreign<T> {
        Foreign {
            id: self.0.foreign.adopt(value),
        }
    }

    /// The object `id` names, while its owner has not given it up.
    pub fn foreign<T: Send + 'static>(self, id: ForeignId<T>) -> Option<&'r mut T> {
        self.0.foreign.reach(id, false)
    }

    /// `body` served with the object `id` names held: an owner that gives
    /// it up meanwhile ([`Memory::disown`]) leaves its drop to the end
    /// of `body`, as php's `GC_ADDREF` keeps an object across a call.
    /// `None`, with `body` not served, where `id` names nothing its owner
    /// still keeps.
    pub fn holding<T: Send + 'static, R>(
        self,
        id: ForeignId<T>,
        body: impl for<'h> FnOnce(&mut Request, Held<'h, T>) -> R,
    ) -> Option<R> {
        if !self.0.foreign.hold(id) {
            return None;
        }
        let held = Held {
            id,
            span: PhantomData,
        };
        let answer = body(self.0, held);
        self.0.foreign.end_hold(id);
        Some(answer)
    }

    /// The object given up by its owner: handed back where no call holds
    /// it, for the owner to close with the request at hand, as a library
    /// whose close calls back into PHP code or answers a code is closed
    /// (`curl_easy_cleanup` serving a file part's `free_cb`,
    /// `sqlite3_close` answering `SQLITE_BUSY`); `None` where a call holds
    /// it, which drops it as the last such call ends.
    #[allow(clippy::needless_pass_by_value)] // the owner is taken, so the object is given up once
    pub fn disown<T: Send + 'static>(self, foreign: Foreign<T>) -> Option<T> {
        self.0.foreign.disown(foreign.id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Table;

    /// Counts its drops.
    struct Counted(Arc<AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counted() -> (Counted, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        (Counted(Arc::clone(&drops)), drops)
    }

    #[test]
    fn an_object_given_up_with_no_hold_is_handed_back_at_once() {
        let mut table = Table::default();
        let (value, drops) = counted();
        let id = table.adopt(value);
        assert!(table.reach(id, false).is_some());
        let back = table.disown(id);
        assert!(back.is_some(), "its owner closes it");
        assert!(table.reach(id, false).is_none());
        assert!(table.reach(id, true).is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(back);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_object_given_up_under_holds_is_dropped_as_the_last_ends() {
        let mut table = Table::default();
        let (value, drops) = counted();
        let id = table.adopt(value);
        assert!(table.hold(id));
        assert!(table.hold(id));
        assert!(
            table.disown(id).is_none(),
            "a hold stands, so the last hold drops it"
        );
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert!(table.reach(id, false).is_none(), "its owner gave it up");
        assert!(table.reach(id, true).is_some(), "a hold still reaches it");
        assert!(!table.hold(id), "no new hold is taken of what was given up");
        table.end_hold(id);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        table.end_hold(id);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_reused_row_answers_nothing_to_the_name_it_had() {
        let mut table = Table::default();
        let old = table.adopt(1u8);
        assert_eq!(table.disown(old), Some(1));
        let new = table.adopt(2u8);
        assert_eq!((old.at, old.generation + 1), (new.at, new.generation));
        assert!(table.reach(old, true).is_none());
        assert!(!table.hold(old));
        assert_eq!(table.disown(old), None);
        assert_eq!(table.reach(new, false).copied(), Some(2));
    }

    #[test]
    fn the_table_dropped_drops_what_remains() {
        let mut table = Table::default();
        let (value, drops) = counted();
        let id = table.adopt(value);
        assert!(table.hold(id));
        drop(table);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}
