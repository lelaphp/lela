//! Map pointers (design §5.14): an immutable shared structure holds a
//! [`MapPtr<T>`], an offset assigned when it was published, and never a
//! `T`; the request's [`MapTable`] holds the `T` and allocates it on first
//! touch. This is `ZEND_MAP_PTR`, and it is what makes a published unit
//! `Sync` by construction.

use std::marker::PhantomData;

/// An offset into a request's map table, typed by what lives there.
#[derive(Debug)]
pub struct MapPtr<T> {
    offset: u32,
    of: PhantomData<fn() -> T>,
}

impl<T> Clone for MapPtr<T> {
    fn clone(&self) -> MapPtr<T> {
        *self
    }
}

impl<T> Copy for MapPtr<T> {}

impl<T> MapPtr<T> {
    /// The pointer at `offset`, assigned by the engine at publication.
    #[must_use]
    pub const fn new(offset: u32) -> MapPtr<T> {
        MapPtr {
            offset,
            of: PhantomData,
        }
    }

    /// The offset, an index into the table.
    #[must_use]
    pub const fn offset(self) -> u32 {
        self.offset
    }
}

/// One request's per-request state for every published structure it has
/// touched, indexed by [`MapPtr`].
#[derive(Debug, Default)]
pub struct MapTable {
    entries: Vec<Option<Box<dyn std::any::Any + Send>>>,
}

impl MapTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> MapTable {
        MapTable::default()
    }

    /// The address compiled code adds a constant offset to: the base of
    /// the table's storage. Zero while nothing has been touched.
    #[must_use]
    pub fn base(&self) -> usize {
        self.entries.as_ptr() as usize
    }

    /// The `T` at `ptr`, built by `T::default()` on first touch.
    pub fn map<T: Default + Send + 'static>(&mut self, ptr: MapPtr<T>) -> &mut T {
        let at = ptr.offset() as usize;
        if self.entries.len() <= at {
            self.entries.resize_with(at + 1, || None);
        }
        let slot = &mut self.entries[at];
        if !slot.as_ref().is_some_and(|b| b.is::<T>()) {
            *slot = Some(Box::new(T::default()));
        }
        // Written as a `T` on the line above when it was not one, so the
        // `None` arm is the type system's, not a path a request takes. With
        // panics denied, a leak is the only safe terminal for a `&mut T`
        // the arm cannot produce from nothing; whether it stays or becomes
        // one `unsafe` block is #177's.
        match slot.as_mut().and_then(|b| b.downcast_mut::<T>()) {
            Some(value) => value,
            None => Box::leak(Box::new(T::default())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_pointer_allocates_on_first_touch_and_keeps_what_was_written() {
        let mut table = MapTable::new();
        let ptr: MapPtr<u32> = MapPtr::new(3);
        assert_eq!(*table.map(ptr), 0);
        *table.map(ptr) = 7;
        assert_eq!(*table.map(ptr), 7);
        assert_eq!(table.base(), table.entries.as_ptr() as usize);
    }
}
