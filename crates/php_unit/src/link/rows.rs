//! The linked class record, design §6.2's immutable linked class:
//! what `zend_do_link_class` leaves in a `zend_class_entry` for a lookup
//! to read, built once by the link and never written after. Each of its
//! tables is [`Rows`], rows in php's order with an index by name, so a
//! lookup is one hash lookup and a walk reads the rows in order.

use std::marker::PhantomData;

use hashbrown::HashTable;

/// A linked class's tables, generic over the key `H` its reader
/// names a class by: the engine's class number, or whatever the caller
/// of the link holds its classes under. Plain data, with no cell and no
/// per-request value, so it is `Sync` where `H` is.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkedClass<H> {
    /// `ce->function_table` in php's order: the class's own methods, a
    /// trait's among them, then each of the parent's table it does not
    /// declare, then the traits' abstract methods it took, then the
    /// interfaces' methods nothing above declared.
    pub methods: Rows<Row<H>, Folded>,
}

impl<H> Default for LinkedClass<H> {
    fn default() -> Self {
        LinkedClass {
            methods: Rows::default(),
        }
    }
}

/// One member of a linked class's rows: the class that declared it,
/// `common.scope`, none for the class's own; and its index among that
/// class's declared members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row<H> {
    pub holder: Option<H>,
    pub at: u32,
}

impl<H: Copy> Row<H> {
    /// The class that declared the member, `class` where the row is the
    /// class's own.
    pub fn holder_in(self, class: H) -> H {
        self.holder.unwrap_or(class)
    }
}

/// How a table's names compare and hash: a method's whatever their
/// case, a constant's or a property's byte for byte.
pub trait Case {
    /// Whether one character's case is folded.
    const FOLDED: bool;
}

/// Names compared whatever their ASCII case, `zend_string_tolower`'s
/// keys: a method's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Folded;

/// Names compared byte for byte: a constant's or a property's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exact;

impl Case for Folded {
    const FOLDED: bool = true;
}

impl Case for Exact {
    const FOLDED: bool = false;
}

/// Rows in the order they were added, and an index of their positions by
/// name: a `HashTable` php's `HashTable` is both of. The name is read
/// through the row by the caller's `named`, so no key is held or
/// allocated; each row's hash is kept beside it, so the index grows,
/// and another table's rows are copied in, without reading a name.
#[derive(Debug, Clone)]
pub struct Rows<R, C> {
    entries: Vec<R>,
    hashes: Vec<u32>,
    index: HashTable<u32>,
    case: PhantomData<C>,
}

impl<R, C> Default for Rows<R, C> {
    fn default() -> Self {
        Rows {
            entries: Vec::new(),
            hashes: Vec::new(),
            index: HashTable::new(),
            case: PhantomData,
        }
    }
}

impl<R: PartialEq, C> PartialEq for Rows<R, C> {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl<R, C: Case> Rows<R, C> {
    /// The rows in order.
    pub fn rows(&self) -> &[R] {
        &self.entries
    }

    /// The row under `name`, whose own name `named` reads.
    pub fn find<'n>(&self, name: &[u8], named: impl Fn(&R) -> &'n [u8]) -> Option<&R> {
        self.position(hash::<C>(name), |row| same::<C>(named(row), name))
            .and_then(|at| self.entries.get(at))
    }

    /// `zend_hash_add`: `row` under `name` at the end, unless a row of
    /// that name stands already. Whether it was added.
    pub(crate) fn add<'n>(&mut self, row: R, name: &[u8], named: impl Fn(&R) -> &'n [u8]) -> bool {
        let hash = hash::<C>(name);
        if self
            .position(hash, |held| same::<C>(named(held), name))
            .is_some()
        {
            return false;
        }
        self.push(row, hash)
    }

    /// `do_inherit_method` over each row of `from` in its order: the row
    /// `copy` makes of it at the end, unless one of its name stands
    /// already. A name is read only where two hashes are equal.
    pub(crate) fn inherit<'n, 'm, S>(
        &mut self,
        from: &Rows<S, C>,
        copy: impl Fn(&S) -> R,
        named: impl Fn(&R) -> &'n [u8],
        named_from: impl Fn(&S) -> &'m [u8],
    ) {
        self.reserve(from.entries.len());
        for (row, &hash) in from.entries.iter().zip(&from.hashes) {
            let name = named_from(row);
            if self
                .position(hash, |held| same::<C>(named(held), name))
                .is_none()
            {
                self.push(copy(row), hash);
            }
        }
    }

    fn reserve(&mut self, more: usize) {
        self.entries.reserve(more);
        self.hashes.reserve(more);
    }

    /// The position of the row of `hash` that `matches` takes.
    fn position(&self, hash: u32, matches: impl Fn(&R) -> bool) -> Option<usize> {
        let found = self.index.find(wide(hash), |&at| {
            let at = usize::try_from(at).unwrap_or(usize::MAX);
            self.hashes.get(at) == Some(&hash) && self.entries.get(at).is_some_and(&matches)
        })?;
        usize::try_from(*found).ok()
    }

    /// `row` at the end, unless the table holds as many rows as a
    /// position can number. Whether it was added.
    fn push(&mut self, row: R, hash: u32) -> bool {
        let Ok(at) = u32::try_from(self.entries.len()) else {
            return false;
        };
        self.entries.push(row);
        self.hashes.push(hash);
        let hashes = &self.hashes;
        self.index.insert_unique(wide(hash), at, |&at| {
            wide(
                hashes
                    .get(usize::try_from(at).unwrap_or(usize::MAX))
                    .copied()
                    .unwrap_or(0),
            )
        });
        true
    }
}

/// Two names the same under `C`.
fn same<C: Case>(a: &[u8], b: &[u8]) -> bool {
    if C::FOLDED {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// A name's hash under `C`, eight bytes at a time, each folded first
/// where `C` folds: what the index files a row under and what the row
/// keeps. php hashes the lowered name with DJBX33A; the choice is not
/// observable (design §3 principle 1).
fn hash<C: Case>(name: &[u8]) -> u32 {
    let mut hash: u64 = 0;
    for chunk in name.chunks(8) {
        let mut word = [0u8; 8];
        for (to, &from) in word.iter_mut().zip(chunk) {
            *to = if C::FOLDED {
                from.to_ascii_lowercase()
            } else {
                from
            };
        }
        hash = (hash.rotate_left(5) ^ u64::from_le_bytes(word)).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    u32::try_from(hash >> 32).unwrap_or_default()
}

/// A kept hash spread over the 64 bits the index reads, its top seven
/// among them.
fn wide(hash: u32) -> u64 {
    u64::from(hash).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

#[cfg(test)]
mod tests {
    use super::{Exact, Folded, Rows};

    fn named(row: &&'static [u8]) -> &'static [u8] {
        row
    }

    #[test]
    fn a_folded_table_finds_a_name_in_any_case_and_keeps_the_first() {
        let mut rows: Rows<&'static [u8], Folded> = Rows::default();
        for name in [&b"Own"[..], b"other", b"OWN"] {
            rows.add(name, name, named);
        }
        assert_eq!(rows.rows(), [&b"Own"[..], b"other"]);
        assert_eq!(rows.find(b"oWn", named), Some(&&b"Own"[..]));
        assert_eq!(rows.find(b"none", named), None);
    }

    #[test]
    fn an_exact_table_tells_case_apart() {
        let mut rows: Rows<&'static [u8], Exact> = Rows::default();
        rows.add(b"K", b"K", named);
        rows.add(b"k", b"k", named);
        assert_eq!(rows.rows().len(), 2);
        assert_eq!(rows.find(b"k", named), Some(&&b"k"[..]));
    }

    #[test]
    fn inheriting_appends_what_the_table_lacks_in_the_other_tables_order() {
        let mut parent: Rows<&'static [u8], Folded> = Rows::default();
        for name in [&b"a"[..], b"B", b"c"] {
            parent.add(name, name, named);
        }
        let mut child: Rows<&'static [u8], Folded> = Rows::default();
        child.add(b"b", b"b", named);
        child.inherit(&parent, |&row| row, named, named);
        assert_eq!(child.rows(), [&b"b"[..], b"a", b"c"]);
        assert_eq!(child.find(b"C", named), Some(&&b"c"[..]));
    }
}
