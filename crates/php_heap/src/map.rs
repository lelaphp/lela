//! Map pointers (design §5.14): an immutable shared structure holds a
//! [`MapPtr<T>`], an offset assigned when it was published, and never a
//! `T`; the request's [`MapTable`] holds the `T` and allocates it on first
//! touch. This is `ZEND_MAP_PTR`, and it is what makes a published unit
//! `Sync` by construction.

use std::any::Any;
use std::marker::PhantomData;
use std::ptr;

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

    /// `ZEND_MAP_PTR_GET`: the `T` at `ptr` where the request has touched
    /// it, and none where it has not.
    #[must_use]
    pub fn get<T: 'static>(&self, ptr: MapPtr<T>) -> Option<&T> {
        self.entries
            .get(ptr.offset() as usize)?
            .as_ref()?
            .downcast_ref::<T>()
    }

    /// The `T` at `ptr`, built by `T::default()` on first touch.
    pub fn map<T: Default + Send + 'static>(&mut self, ptr: MapPtr<T>) -> &mut T {
        let at = ptr.offset() as usize;
        if self.entries.len() <= at {
            self.entries.resize_with(at + 1, || None);
        }
        let boxed = match &mut self.entries[at] {
            Some(boxed) if boxed.is::<T>() => boxed,
            slot => slot.insert(Box::new(T::default())),
        };
        let raw = ptr::from_mut::<dyn Any + Send>(boxed.as_mut()).cast::<T>();
        // SAFETY: the box holds a `T`: the first arm checked it and the
        // second wrote one, so the cast names the `T` the box owns and the
        // borrow lives as long as the table's.
        unsafe { &mut *raw }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_pointer_allocates_on_first_touch_and_keeps_what_was_written() {
        let mut table = MapTable::new();
        let ptr: MapPtr<u32> = MapPtr::new(3);
        assert_eq!(table.get(ptr), None, "untouched, there is nothing to read");
        assert_eq!(*table.map(ptr), 0);
        *table.map(ptr) = 7;
        assert_eq!(*table.map(ptr), 7);
        assert_eq!(table.get(ptr), Some(&7));
        assert_eq!(table.base(), table.entries.as_ptr() as usize);
    }
}
