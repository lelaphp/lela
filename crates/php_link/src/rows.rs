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
    /// `common.prototype`'s scope for each row of `methods`, at the
    /// row's position: the class whose method the row's prototype is,
    /// none where the link set none.
    pub prototypes: Box<[Option<H>]>,
    /// `CE_CONSTANTS_TABLE` in php's order: the class's own constants, a
    /// trait's among them, then each of the parent's table it does not
    /// declare but the parent's private ones, then the interfaces'
    /// constants nothing above declared. A row, never a value: the
    /// value is the request's to evaluate.
    pub constants: Rows<Row<H>, Exact>,
    /// `ce->properties_info` in php's order: the class's own properties,
    /// a trait's among them, then each of the parent's table it does not
    /// declare, a private one among them, then the interfaces' properties
    /// nothing above declared (`do_inherit_property`). A row's holder is
    /// the property's `ce`, the class whose declaration it is.
    pub properties: Rows<Row<H>, Exact>,
    /// `ce->interfaces`: every interface the class implements, each once
    /// and in php's order, so `instanceof` scans one slice.
    pub interfaces: Box<[H]>,
    /// `ce->constructor`: the position of `__construct`'s row in
    /// `methods`, set once where the class is linked, as
    /// `zend_add_magic_method` sets it for the class's own and
    /// `zend_do_inheritance` copies the parent's; none for a class with
    /// none.
    pub constructor: Option<u32>,
    /// `ce->destructor`: the position of `__destruct`'s row in
    /// `methods`, set as [`LinkedClass::constructor`] is.
    pub destructor: Option<u32>,
}

impl<H> Default for LinkedClass<H> {
    fn default() -> Self {
        LinkedClass {
            methods: Rows::default(),
            prototypes: Box::default(),
            constants: Rows::default(),
            properties: Rows::default(),
            interfaces: Box::default(),
            constructor: None,
            destructor: None,
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
        self.find_at(name, named).map(|(_, row)| row)
    }

    /// The row under `name` beside its position among the rows.
    pub fn find_at<'n>(&self, name: &[u8], named: impl Fn(&R) -> &'n [u8]) -> Option<(usize, &R)> {
        let at = self.position(hash::<C>(name), |row| same::<C>(named(row), name))?;
        Some((at, self.entries.get(at)?))
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

    /// `do_inherit_method`, or `do_inherit_class_constant`, over each
    /// row of `from` in its order: the row `copy` makes of it at the
    /// end, where it makes one, unless one of its name stands already.
    /// A name is read only where two hashes are equal.
    pub(crate) fn inherit<'n, 'm, S, O: Into<Option<R>>>(
        &mut self,
        from: &Rows<S, C>,
        copy: impl Fn(&S) -> O,
        named: impl Fn(&R) -> &'n [u8],
        named_from: impl Fn(&S) -> &'m [u8],
    ) {
        self.reserve(from.entries.len());
        for (row, &hash) in from.entries.iter().zip(&from.hashes) {
            let name = named_from(row);
            if self
                .position(hash, |held| same::<C>(named(held), name))
                .is_none()
                && let Some(row) = copy(row).into()
            {
                self.push(row, hash);
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

/// A declaration that carries its own name, as a row of
/// `CG(function_table)`, `CG(class_table)` or `EG(zend_constants)` is
/// filed under it.
pub trait Named {
    /// The name the row is filed under, as declared.
    fn name(&self) -> &[u8];
}

impl Named for php_unit::decl::FunctionDecl {
    fn name(&self) -> &[u8] {
        &self.name.0
    }
}

impl Named for php_unit::decl::ClassDecl {
    fn name(&self) -> &[u8] {
        &self.name.0
    }
}

impl Named for php_unit::decl::ConstDecl {
    fn name(&self) -> &[u8] {
        &self.name.0
    }
}

/// An index of a table's positions by name, the `HashTable` php keeps
/// `CG(function_table)`, `CG(class_table)` and `EG(zend_constants)` in,
/// over rows its holder keeps in order beside other columns of the same
/// rows: the first row of a name is the one found, as `zend_hash_add`
/// keeps the first, and every row stays where it stands. No name is
/// held; each is read through the rows ([`Named`]).
#[derive(Debug, Clone)]
pub struct Names<C> {
    index: HashTable<u32>,
    case: PhantomData<C>,
}

impl<C> Names<C> {
    /// The index of no rows.
    #[must_use]
    pub const fn new() -> Names<C> {
        Names {
            index: HashTable::new(),
            case: PhantomData,
        }
    }
}

impl<C> Default for Names<C> {
    fn default() -> Self {
        Names::new()
    }
}

impl<C: Case> Names<C> {
    /// The index of `rows`, each under its name; a row past the
    /// positions a `u32` numbers is not filed.
    #[must_use]
    pub fn over<R: Named>(rows: &[R]) -> Names<C> {
        let hashes: Vec<u32> = rows.iter().map(|row| hash::<C>(row.name())).collect();
        let mut index = HashTable::with_capacity(rows.len());
        for (at, (row, &hash)) in rows.iter().zip(&hashes).enumerate() {
            let Ok(position) = u32::try_from(at) else {
                break;
            };
            let held = |&p: &u32| {
                let p = usize::try_from(p).unwrap_or(usize::MAX);
                rows.get(p)
                    .is_some_and(|held| same::<C>(held.name(), row.name()))
            };
            if index.find(wide(hash), held).is_none() {
                index.insert_unique(wide(hash), position, |&p| {
                    wide(
                        hashes
                            .get(usize::try_from(p).unwrap_or(usize::MAX))
                            .copied()
                            .unwrap_or(0),
                    )
                });
            }
        }
        Names {
            index,
            case: PhantomData,
        }
    }

    /// The position among `rows`, the rows the index was made over, of
    /// the first named `name`.
    pub fn find<R: Named>(&self, rows: &[R], name: &[u8]) -> Option<usize> {
        if self.index.is_empty() {
            return None;
        }
        let found = self.index.find(wide(hash::<C>(name)), |&at| {
            let at = usize::try_from(at).unwrap_or(usize::MAX);
            rows.get(at).is_some_and(|row| same::<C>(row.name(), name))
        })?;
        usize::try_from(*found).ok()
    }

    /// `zend_hash_str_del`: the name `name` no longer finds the row of
    /// `rows` it found, which stays where it stands; that row's position,
    /// where one was filed under the name.
    pub fn remove<R: Named>(&mut self, rows: &[R], name: &[u8]) -> Option<usize> {
        let found = self.index.find_entry(wide(hash::<C>(name)), |&at| {
            let at = usize::try_from(at).unwrap_or(usize::MAX);
            rows.get(at).is_some_and(|row| same::<C>(row.name(), name))
        });
        let (at, _) = found.ok()?.remove();
        usize::try_from(at).ok()
    }
}

/// A table's rows beside their [`Names`]: the view of one of the
/// engine's declaration tables the compiler and the engine read a name
/// through. A view of no rows has no index to read.
#[derive(Debug)]
pub struct Indexed<'a, R, C> {
    rows: &'a [R],
    names: Option<&'a Names<C>>,
}

impl<R, C> Clone for Indexed<'_, R, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R, C> Copy for Indexed<'_, R, C> {}

impl<R, C> Default for Indexed<'_, R, C> {
    /// No rows.
    fn default() -> Self {
        Indexed {
            rows: &[],
            names: None,
        }
    }
}

impl<'a, R: Named, C: Case> Indexed<'a, R, C> {
    /// `rows` read through `names`, which was made over them.
    #[must_use]
    pub fn new(rows: &'a [R], names: &'a Names<C>) -> Indexed<'a, R, C> {
        Indexed {
            rows,
            names: Some(names),
        }
    }

    /// The rows in order.
    #[must_use]
    pub fn rows(self) -> &'a [R] {
        self.rows
    }

    /// The first row named `name`, beside its position.
    #[must_use]
    pub fn find(self, name: &[u8]) -> Option<(usize, &'a R)> {
        let at = self.names?.find(self.rows, name)?;
        Some((at, self.rows.get(at)?))
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

/// A table that holds the name it files each value under, as php's
/// `HashTable` holds a bucket's key: `EG(class_table)`'s rows past the
/// engine's, a class's trampolines, any table whose rows do not carry
/// their own name ([`Names`] indexes rows that do). The name is kept as
/// it is handed in and found under `C`, so a folded table finds `Foo`
/// under `FOO` with neither lowered; its hash is kept beside it.
#[derive(Debug, Clone)]
pub struct NameMap<C, V> {
    table: HashTable<Filed<V>>,
    case: PhantomData<C>,
}

/// One row of a [`NameMap`]: its name's hash, the name and the value.
#[derive(Debug, Clone)]
struct Filed<V> {
    hash: u32,
    name: Box<[u8]>,
    value: V,
}

impl<C, V> NameMap<C, V> {
    /// The table of no rows.
    #[must_use]
    pub const fn new() -> NameMap<C, V> {
        NameMap {
            table: HashTable::new(),
            case: PhantomData,
        }
    }

    /// Every row given up, `zend_hash_clean`.
    pub fn clear(&mut self) {
        self.table.clear();
    }
}

impl<C, V> Default for NameMap<C, V> {
    fn default() -> Self {
        NameMap::new()
    }
}

impl<C: Case, V> NameMap<C, V> {
    /// `zend_hash_find`: the value filed under `name`.
    pub fn find(&self, name: &[u8]) -> Option<&V> {
        if self.table.is_empty() {
            return None;
        }
        let hash = hash::<C>(name);
        let found = self.table.find(wide(hash), |filed| {
            filed.hash == hash && same::<C>(&filed.name, name)
        })?;
        Some(&found.value)
    }

    /// `zend_hash_add`: `value` under `name`, unless a row of that name
    /// stands already, which is kept. Whether it was added.
    pub fn add(&mut self, name: &[u8], value: V) -> bool {
        let hash = hash::<C>(name);
        let entry = self.table.entry(
            wide(hash),
            |filed| filed.hash == hash && same::<C>(&filed.name, name),
            |filed| wide(filed.hash),
        );
        match entry {
            hashbrown::hash_table::Entry::Occupied(_) => false,
            hashbrown::hash_table::Entry::Vacant(vacant) => {
                vacant.insert(Filed {
                    hash,
                    name: name.into(),
                    value,
                });
                true
            }
        }
    }
}

/// The one hash of a name, as a `BuildHasher`, for a std `HashMap` whose
/// key is a name compared byte for byte: what every other table by name
/// here hashes with ([`Names`], [`Rows`], [`NameMap`]), so a name is
/// hashed one way wherever it is a key. An extension reaches it on the
/// face, which saves it a hash crate of its own (#2157).
#[derive(Debug, Clone, Copy, Default)]
pub struct NameHasher;

impl std::hash::BuildHasher for NameHasher {
    type Hasher = NameHash;

    fn build_hasher(&self) -> NameHash {
        NameHash(0)
    }
}

/// [`NameHasher`]'s state: the words written so far, mixed as a name's
/// eight-byte words are.
#[derive(Debug, Clone, Copy, Default)]
pub struct NameHash(u64);

impl std::hash::Hasher for NameHash {
    fn finish(&self) -> u64 {
        wide(top(self.0))
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0 = words(bytes).fold(self.0, mix);
    }

    fn write_usize(&mut self, n: usize) {
        self.0 = mix(self.0, n as u64);
    }
}

/// A name's hash under `C`, eight bytes at a time, each word folded
/// first where `C` folds: what the index files a row under and what the
/// row keeps. php hashes the lowered name with DJBX33A; the choice is
/// not observable (design §3 principle 1).
fn hash<C: Case>(name: &[u8]) -> u32 {
    let hash = words(name).fold(0, |hash, word| {
        mix(hash, if C::FOLDED { folded(word) } else { word })
    });
    top(hash)
}

/// `bytes` as little-endian words of eight, the last zero past its
/// bytes where they do not fill it.
fn words(bytes: &[u8]) -> impl Iterator<Item = u64> + '_ {
    let chunks = bytes.chunks_exact(8);
    let rest = chunks.remainder();
    let last = (!rest.is_empty())
        .then(|| (rest.iter().rev()).fold(0, |word, &byte| word << 8 | u64::from(byte)));
    chunks
        .map(|chunk| {
            let mut word = [0u8; 8];
            word.copy_from_slice(chunk);
            u64::from_le_bytes(word)
        })
        .chain(last)
}

/// `zend_str_tolower` over the eight bytes of `word` at once: each of
/// `A` to `Z` gains the bit that lowers it, and no other byte changes.
fn folded(word: u64) -> u64 {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let seven = word & (ONES * 0x7f);
    let from_a = seven + ONES * (0x80 - u64::from(b'A'));
    let past_z = seven + ONES * (0x80 - u64::from(b'Z') - 1);
    let upper = from_a & !past_z & !word & (ONES * 0x80);
    word | (upper >> 2)
}

/// One word mixed into a hash.
fn mix(hash: u64, word: u64) -> u64 {
    (hash.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// A hash's top half, its best mixed bits.
fn top(hash: u64) -> u32 {
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
    fn an_index_finds_the_first_row_of_a_name_and_keeps_every_row() {
        use super::{Indexed, Names};
        use php_unit::decl::{ConstDecl, ConstExpr, Name};
        let constant = |name: &str, value| ConstDecl {
            name: Name(name.as_bytes().into()),
            value: ConstExpr::long(value),
            attributes: Vec::new(),
        };
        let rows = [
            constant("A", 1),
            constant("b", 2),
            constant("A", 3),
            constant("B", 4),
        ];
        let exact: Names<Exact> = Names::over(&rows);
        let table = Indexed::new(&rows, &exact);
        assert_eq!(table.find(b"A").map(|(at, _)| at), Some(0));
        assert_eq!(table.find(b"B").map(|(at, _)| at), Some(3));
        assert_eq!(table.find(b"a"), None);
        let folded: Names<Folded> = Names::over(&rows);
        assert_eq!(folded.find(&rows, b"B"), Some(1));
        assert_eq!(Indexed::<ConstDecl, Exact>::default().find(b"A"), None);
    }

    #[test]
    fn a_removed_name_finds_nothing_and_its_row_stays() {
        use super::Names;
        use php_unit::decl::{ConstDecl, ConstExpr, Name};
        let constant = |name: &str| ConstDecl {
            name: Name(name.as_bytes().into()),
            value: ConstExpr::long(0),
            attributes: Vec::new(),
        };
        let rows = [constant("a"), constant("B")];
        let mut names: Names<Folded> = Names::over(&rows);
        assert_eq!(names.remove(&rows, b"b"), Some(1));
        assert_eq!(names.find(&rows, b"B"), None);
        assert_eq!(names.find(&rows, b"a"), Some(0));
        assert_eq!(names.remove(&rows, b"b"), None);
    }

    #[test]
    fn folding_a_word_lowers_each_byte_as_ascii_does() {
        for byte in 0..=u8::MAX {
            let word = u64::from_le_bytes([byte, b'Q', 0x80 | byte, b'@', b'[', b'z', 0xc1, byte]);
            let lowered = word.to_le_bytes().map(|b| b.to_ascii_lowercase());
            assert_eq!(
                super::folded(word),
                u64::from_le_bytes(lowered),
                "{byte:#x}"
            );
        }
    }

    #[test]
    fn a_name_map_keeps_the_first_row_of_a_name_and_finds_it_in_any_case() {
        use super::NameMap;
        let mut folded: NameMap<Folded, u32> = NameMap::new();
        assert!(folded.add(b"ArrayObject", 1));
        assert!(!folded.add(b"arrayobject", 2));
        assert!(folded.add(b"ArrayIterator", 3));
        assert_eq!(folded.find(b"ARRAYOBJECT"), Some(&1));
        assert_eq!(folded.find(b"arrayiterator"), Some(&3));
        assert_eq!(folded.find(b"Array"), None);
        let mut exact: NameMap<Exact, u32> = NameMap::default();
        exact.add(b"K", 1);
        assert_eq!(exact.find(b"k"), None);
        folded.clear();
        assert_eq!(folded.find(b"ArrayObject"), None);
    }

    #[test]
    fn a_std_map_under_the_name_hasher_finds_what_it_holds() {
        let mut map: std::collections::HashMap<Vec<u8>, u32, super::NameHasher> =
            std::collections::HashMap::default();
        map.insert(b"Europe/Amsterdam".to_vec(), 1);
        map.insert(b"UTC".to_vec(), 2);
        assert_eq!(map.get(&b"UTC"[..]), Some(&2));
        assert_eq!(map.get(&b"Europe/Amsterdam"[..]), Some(&1));
        assert_eq!(map.get(&b"utc"[..]), None);
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
