//! The lists and boxes of the declaration model, as [`php_heap::Bytes`]
//! is its names: a native's borrowed from its static entry for the life
//! of the process, a program's owned.
//!
//! php points a `zend_internal_function` at its `zend_internal_arg_info`
//! rows and a `zend_type` at a class name's `const char *`
//! (`zend_register_functions`, `Z/Zend/zend_API.c`), so registering a
//! native builds no tree of its types. A [`List`] or a [`Boxed`]
//! borrowed from a native's entry is that pointer: a native's union, its
//! `?T` and its attribute lists cost the process no allocation. One a
//! compile or a reader made is owned and goes with whatever holds it.
//! Either reads, compares, hashes and prints as what it holds.

use std::borrow::Cow;
use std::ops::{Deref, DerefMut};

use php_macros::EngineState;

/// A list of the declaration model: borrowed from a native's static
/// entry ([`List::borrowed`]) or a vector of its own (`From` a vector,
/// `collect`). Changing a borrowed one copies it into a vector first, so
/// the entry stays as the binary laid it.
#[derive(EngineState)]
pub struct List<T: Clone + 'static>(Cow<'static, [T]>);

impl<T: Clone> List<T> {
    /// `entries`, the binary's, pointed at and not copied.
    #[must_use]
    pub const fn borrowed(entries: &'static [T]) -> List<T> {
        List(Cow::Borrowed(entries))
    }

    /// The empty list, which holds no allocation.
    #[must_use]
    pub const fn new() -> List<T> {
        List(Cow::Owned(Vec::new()))
    }

    /// The entries a native's list borrows, the empty slice for an
    /// empty owned one, `None` for any other: what a `const` names the
    /// same entries again by, as no `Clone` runs in one.
    #[must_use]
    pub const fn as_borrowed(&self) -> Option<&'static [T]> {
        match &self.0 {
            Cow::Borrowed(entries) => Some(entries),
            Cow::Owned(owned) if owned.is_empty() => Some(&[]),
            Cow::Owned(_) => None,
        }
    }

    /// The entries as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// The vector to change: a borrowed list copied into one first.
    pub fn to_mut(&mut self) -> &mut Vec<T> {
        self.0.to_mut()
    }

    /// The entries as a vector of their own.
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.0.into_owned()
    }

    /// `entry` after the last.
    pub fn push(&mut self, entry: T) {
        self.to_mut().push(entry);
    }

    /// `entry` at `at`, those after it moved on by one.
    pub fn insert(&mut self, at: usize, entry: T) {
        self.to_mut().insert(at, entry);
    }

    /// The entry at `at`, taken out.
    pub fn remove(&mut self, at: usize) -> T {
        self.to_mut().remove(at)
    }

    /// The entries `keep` keeps, in order.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.to_mut().retain(keep);
    }

    /// Room for `more` entries, exactly.
    pub fn reserve_exact(&mut self, more: usize) {
        self.to_mut().reserve_exact(more);
    }

    /// An owned list's spare room given back; a borrowed one has none.
    pub fn shrink_to_fit(&mut self) {
        if let Cow::Owned(owned) = &mut self.0 {
            owned.shrink_to_fit();
        }
    }
}

impl<T: Clone> Deref for List<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

/// The entries to change, a borrowed list copied first.
impl<T: Clone> DerefMut for List<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.to_mut()
    }
}

impl<T: Clone> Default for List<T> {
    fn default() -> List<T> {
        List::new()
    }
}

/// The same entries where they are the binary's, a vector of copies
/// where they are owned.
impl<T: Clone> Clone for List<T> {
    fn clone(&self) -> List<T> {
        List(self.0.clone())
    }
}

impl<T: Clone> From<Vec<T>> for List<T> {
    fn from(owned: Vec<T>) -> List<T> {
        List(Cow::Owned(owned))
    }
}

impl<T: Clone> From<Box<[T]>> for List<T> {
    fn from(owned: Box<[T]>) -> List<T> {
        List(Cow::Owned(owned.into_vec()))
    }
}

impl<T: Clone> FromIterator<T> for List<T> {
    fn from_iter<I: IntoIterator<Item = T>>(entries: I) -> List<T> {
        List(Cow::Owned(entries.into_iter().collect()))
    }
}

impl<T: Clone> Extend<T> for List<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, entries: I) {
        self.to_mut().extend(entries);
    }
}

impl<T: Clone> IntoIterator for List<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> std::vec::IntoIter<T> {
        self.into_vec().into_iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a List<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> std::slice::Iter<'a, T> {
        self.iter()
    }
}

impl<'a, T: Clone> IntoIterator for &'a mut List<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> std::slice::IterMut<'a, T> {
        self.iter_mut()
    }
}

/// As the slice prints, whichever the list is.
impl<T: Clone + std::fmt::Debug> std::fmt::Debug for List<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <[T] as std::fmt::Debug>::fmt(self, f)
    }
}

impl<T: Clone + PartialEq> PartialEq for List<T> {
    fn eq(&self, other: &List<T>) -> bool {
        **self == **other
    }
}

impl<T: Clone + PartialEq> PartialEq<Vec<T>> for List<T> {
    fn eq(&self, other: &Vec<T>) -> bool {
        **self == **other
    }
}

impl<T: Clone + Eq> Eq for List<T> {}

/// As the slice hashes, as a vector does, so a borrowed and an owned
/// list of the same entries are one key.
impl<T: Clone + std::hash::Hash> std::hash::Hash for List<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

/// One value of the declaration model behind a pointer, `?T`'s `T`:
/// borrowed from a native's static entry ([`Boxed::borrowed`]) or a box
/// of its own (`From` a box or a value).
#[derive(EngineState)]
pub struct Boxed<T: 'static>(Pointed<T>);

#[derive(EngineState)]
enum Pointed<T: 'static> {
    Borrowed(&'static T),
    Owned(Box<T>),
}

impl<T> Boxed<T> {
    /// `value`, the binary's, pointed at and not copied.
    #[must_use]
    pub const fn borrowed(value: &'static T) -> Boxed<T> {
        Boxed(Pointed::Borrowed(value))
    }

    /// The value a native's box borrows, `None` for an owned one: what a
    /// `const` names the same value again by.
    #[must_use]
    pub const fn as_borrowed(&self) -> Option<&'static T> {
        match &self.0 {
            Pointed::Borrowed(value) => Some(value),
            Pointed::Owned(_) => None,
        }
    }
}

impl<T> Deref for Boxed<T> {
    type Target = T;

    fn deref(&self) -> &T {
        match &self.0 {
            Pointed::Borrowed(value) => value,
            Pointed::Owned(owned) => owned,
        }
    }
}

/// The value to change, a borrowed one copied into a box first.
impl<T: Clone> DerefMut for Boxed<T> {
    fn deref_mut(&mut self) -> &mut T {
        loop {
            match self.0 {
                Pointed::Borrowed(value) => self.0 = Pointed::Owned(Box::new(value.clone())),
                Pointed::Owned(ref mut owned) => return owned,
            }
        }
    }
}

impl<T> AsRef<T> for Boxed<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> From<Box<T>> for Boxed<T> {
    fn from(owned: Box<T>) -> Boxed<T> {
        Boxed(Pointed::Owned(owned))
    }
}

impl<T> From<T> for Boxed<T> {
    fn from(value: T) -> Boxed<T> {
        Boxed(Pointed::Owned(Box::new(value)))
    }
}

/// The same value where it is the binary's, a box of a copy where it is
/// owned.
impl<T: Clone> Clone for Boxed<T> {
    fn clone(&self) -> Boxed<T> {
        match &self.0 {
            Pointed::Borrowed(value) => Boxed::borrowed(value),
            Pointed::Owned(owned) => Boxed(Pointed::Owned(owned.clone())),
        }
    }
}

/// As the value prints, as a box does.
impl<T: std::fmt::Debug> std::fmt::Debug for Boxed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        T::fmt(self, f)
    }
}

impl<T: PartialEq> PartialEq for Boxed<T> {
    fn eq(&self, other: &Boxed<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for Boxed<T> {}

impl<T: std::hash::Hash> std::hash::Hash for Boxed<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        T::hash(self, state);
    }
}

#[cfg(test)]
mod tests {
    use super::{Boxed, List};

    const ENTRIES: &[u32] = &[1, 2];

    /// A borrowed list is the binary's entries themselves, and its clone
    /// too; changing one copies it and leaves the entries be; either
    /// compares and hashes as the slice it holds.
    #[test]
    fn a_borrowed_list_is_the_entries_until_changed() {
        let mut list = List::borrowed(ENTRIES);
        assert_eq!(list.as_ptr(), ENTRIES.as_ptr());
        assert_eq!(list.clone().as_ptr(), ENTRIES.as_ptr());
        assert_eq!(list.as_borrowed(), Some(ENTRIES));
        list.push(3);
        assert_ne!(list.as_ptr(), ENTRIES.as_ptr());
        assert_eq!(list, vec![1, 2, 3]);
        assert_eq!(list.as_borrowed(), None);
        assert_eq!(ENTRIES, &[1, 2]);
        assert_eq!(List::<u32>::new().as_borrowed(), Some(&[][..]));
        assert_eq!(List::borrowed(&[1u32, 2, 3]), list);
        assert_eq!(format!("{list:?}"), "[1, 2, 3]");
        let boxed = Boxed::borrowed(&7u32);
        assert_eq!(*boxed, 7);
        assert_eq!(boxed, Boxed::from(7));
        assert_eq!(Boxed::from(7).as_borrowed(), None);
        assert_eq!(format!("{boxed:?}"), "7");
    }
}
