//! The linked class record, design §6.2's immutable linked class:
//! what `zend_do_link_class` leaves in a `zend_class_entry` for a lookup
//! to read, built once by the link and never written after. Each of its
//! tables is [`Rows`], rows in php's order with an index by name, so a
//! lookup is one hash lookup and a walk reads the rows in order.

use std::marker::PhantomData;
use std::sync::Arc;

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
    /// none where the link set none, and none for each row past the
    /// column's length, so a class whose rows have none holds none.
    pub prototypes: Column<Option<H>>,
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
    pub interfaces: Column<H>,
    /// `ce->constructor`: the position of `__construct`'s row in
    /// `methods`, set once where the class is linked, as
    /// `zend_add_magic_method` sets it for the class's own and
    /// `zend_do_inheritance` copies the parent's; none for a class with
    /// none.
    pub constructor: Option<u32>,
    /// `ce->destructor`: the position of `__destruct`'s row in
    /// `methods`, set as [`LinkedClass::constructor`] is.
    pub destructor: Option<u32>,
    /// `ce->__get`, `ce->__set`, `ce->__isset` and `ce->__unset`, set as
    /// [`LinkedClass::constructor`] is, so a property access asks whether
    /// the class serves one by reading a position, never by a lookup.
    pub accessors: Accessors,
    /// Where the class's property hooks are: `ce->num_hooked_props != 0`,
    /// or a parent's private hooked property only that parent's scope
    /// reaches, so a property access on a class with neither asks no
    /// hook.
    pub hooks: ClassHooks,
}

/// The positions in a linked class's `methods` of the four magic
/// methods a property access is served by, `zend_add_magic_method`'s
/// `ce->__get`, `ce->__set`, `ce->__isset` and `ce->__unset`, which
/// `zend_do_inheritance` copies down; none for a class without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Accessors {
    pub get: Option<u32>,
    pub set: Option<u32>,
    pub isset: Option<u32>,
    pub unset: Option<u32>,
}

/// One of the four magic methods a property access is served by, which
/// names its position among [`Accessors`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accessor {
    Get,
    Set,
    Isset,
    Unset,
}

impl Accessors {
    /// The position of `accessor`'s row among the record's methods.
    #[must_use]
    pub fn of(&self, accessor: Accessor) -> Option<u32> {
        match accessor {
            Accessor::Get => self.get,
            Accessor::Set => self.set,
            Accessor::Isset => self.isset,
            Accessor::Unset => self.unset,
        }
    }
}

/// Where a linked class's property hooks are, as `do_inherit_property`
/// counts `ce->num_hooked_props` and as a property access finds a
/// `prop_info->hooks` under its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClassHooks {
    /// No property of the class's chain has hooks.
    #[default]
    Absent,
    /// No row of `properties` has hooks, `ce->num_hooked_props == 0`, but
    /// a parent's private hooked property the class redeclares is still
    /// read through its hooks under that parent's scope
    /// (`zend_get_parent_private_property`).
    ParentPrivate,
    /// A row of `properties` has hooks, `ce->num_hooked_props != 0`:
    /// `get_object_vars`, the encoders and `foreach` read the object
    /// through the hooks' table and iterator.
    Rows,
}

impl<H> Default for LinkedClass<H> {
    fn default() -> Self {
        LinkedClass {
            methods: Rows::default(),
            prototypes: Column::default(),
            constants: Rows::default(),
            properties: Rows::default(),
            interfaces: Column::default(),
            constructor: None,
            destructor: None,
            accessors: Accessors::default(),
            hooks: ClassHooks::Absent,
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

/// One column of a linked record: its own values, or a range of one
/// table laid for many records ([`Column::lay`]), as opcache persists a
/// class's tables into one buffer. It reads as the slice either way.
#[derive(Debug, Clone)]
pub struct Column<T>(Store<T>);

/// Where a [`Column`]'s values are.
#[derive(Debug, Clone)]
enum Store<T> {
    Own(Vec<T>),
    Laid {
        table: Arc<[T]>,
        start: u32,
        end: u32,
    },
}

impl<T> Default for Column<T> {
    fn default() -> Self {
        Column(Store::Own(Vec::new()))
    }
}

impl<T> From<Vec<T>> for Column<T> {
    fn from(values: Vec<T>) -> Self {
        Column(Store::Own(values))
    }
}

impl<T> FromIterator<T> for Column<T> {
    fn from_iter<I: IntoIterator<Item = T>>(values: I) -> Self {
        Column::from(values.into_iter().collect::<Vec<T>>())
    }
}

impl<T> std::ops::Deref for Column<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match &self.0 {
            Store::Own(values) => values,
            Store::Laid { table, start, end } => table
                .get(wide_at(*start)..wide_at(*end))
                .unwrap_or_default(),
        }
    }
}

impl<T: PartialEq> PartialEq for Column<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Clone> Column<T> {
    /// Each of `columns` laid as a range of one table, in their order, so
    /// they hold one allocation between them rather than one each; an
    /// empty one holds none. Columns past the rows a `u32` numbers are
    /// left as they stand.
    pub fn lay<'c>(columns: impl IntoIterator<Item = &'c mut Column<T>>)
    where
        T: 'c,
    {
        let mut columns: Vec<&mut Column<T>> = columns.into_iter().collect();
        let len: usize = columns.iter().map(|column| column.len()).sum();
        let Some(first) = columns.iter().find_map(|column| column.first().cloned()) else {
            for column in &mut columns {
                column.0 = Store::Own(Vec::new());
            }
            return;
        };
        if u32::try_from(len).is_err() {
            return;
        }
        // The table allocated once at its length, then each column copied
        // in and its own values given up, so no second copy stands.
        let mut table: Arc<[T]> = std::iter::repeat_n(first, len).collect();
        let Some(cells) = Arc::get_mut(&mut table) else {
            return;
        };
        let mut ends = Vec::with_capacity(columns.len());
        let mut start = 0;
        for column in &mut columns {
            let end = start + column.len();
            if let Some(cells) = cells.get_mut(start..end) {
                cells.clone_from_slice(column);
            }
            column.0 = Store::Own(Vec::new());
            ends.push(end);
            start = end;
        }
        let mut start = 0;
        for (column, end) in columns.iter_mut().zip(ends) {
            let end = u32::try_from(end).unwrap_or(start);
            if end != start {
                column.0 = Store::Laid {
                    table: Arc::clone(&table),
                    start,
                    end,
                };
            }
            start = end;
        }
    }

    /// The values to change, a laid column's copied out of its table
    /// first; none only where that copy did not take.
    fn own(&mut self) -> Option<&mut Vec<T>> {
        if let Store::Laid { .. } = &self.0 {
            self.0 = Store::Own(self.to_vec());
        }
        match &mut self.0 {
            Store::Own(values) => Some(values),
            Store::Laid { .. } => None,
        }
    }
}

impl<H: Clone> LinkedClass<H> {
    /// Each column of `records` laid as a range of one table of its kind:
    /// every record's rows in one, their indexes and hashes in another,
    /// the prototypes and the interfaces in one each, so the records hold
    /// four allocations between them rather than about ten each, as
    /// opcache persists a class's tables into one buffer
    /// (`zend_persist_class_entry`). What a lookup reads is unchanged.
    pub fn lay(records: &mut [LinkedClass<H>]) {
        let mut rows = Vec::with_capacity(records.len().saturating_mul(3));
        let mut words = Vec::with_capacity(records.len().saturating_mul(3));
        let mut prototypes = Vec::with_capacity(records.len());
        let mut interfaces = Vec::with_capacity(records.len());
        for record in records {
            rows.extend([
                &mut record.methods.entries,
                &mut record.constants.entries,
                &mut record.properties.entries,
            ]);
            words.extend([
                &mut record.methods.words,
                &mut record.constants.words,
                &mut record.properties.words,
            ]);
            prototypes.push(&mut record.prototypes);
            interfaces.push(&mut record.interfaces);
        }
        Column::lay(rows);
        Column::lay(words);
        Column::lay(prototypes);
        Column::lay(interfaces);
    }
}

/// A table of at most this many rows has no index: its hashes are
/// scanned, which at that length reads no more than a lookup in an index does.
const SCANNED: usize = 8;

/// Rows in the order they were added, and an index of their positions by
/// name: a `HashTable` php's `HashTable` is both of. The name is read
/// through the row by the caller's `named`, so no key is held or
/// allocated; each row's hash is kept beside the index in one column, as
/// `zend_hash` lays its hash part before its buckets in one allocation
/// (`HT_SET_DATA_ADDR`), so the index grows, and another table's rows
/// are copied in, without reading a name.
#[derive(Debug, Clone)]
pub struct Rows<R, C> {
    entries: Column<R>,
    /// The index's `slots` slots, each the position of a row plus one
    /// or zero where none is filed, read in turn from where the hash
    /// lands, packed into as few words as the positions fit
    /// ([`slot_bits`]); then each row's hash, by its position.
    words: Column<u32>,
    /// How many slots the index has: none for a table of at most
    /// [`SCANNED`] rows, otherwise a power of two at least twice its
    /// rows, so a lookup ends at an empty slot.
    slots: u32,
    case: PhantomData<C>,
}

impl<R, C> Default for Rows<R, C> {
    fn default() -> Self {
        Rows {
            entries: Column::default(),
            words: Column::default(),
            slots: 0,
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

    /// Each row's hash, by its position.
    fn hashes(&self) -> &[u32] {
        self.words
            .get(index_words(self.slots)..)
            .unwrap_or_default()
    }

    /// The row under `name`, whose own name `named` reads.
    pub fn find<'n>(&self, name: &[u8], named: impl Fn(&R) -> &'n [u8]) -> Option<&R> {
        self.find_at(name, named).map(|(_, row)| row)
    }

    /// The row under `name` beside its position among the rows.
    pub fn find_at<'n>(&self, name: &[u8], named: impl Fn(&R) -> &'n [u8]) -> Option<(usize, &R)> {
        self.found_at(hash::<C>(name), name, named)
    }

    fn found_at<'n>(
        &self,
        hash: u32,
        name: &[u8],
        named: impl Fn(&R) -> &'n [u8],
    ) -> Option<(usize, &R)> {
        let at = self.position(hash, |row| same::<C>(named(row), name))?;
        Some((at, self.entries.get(at)?))
    }

    /// The position of the first row of `hash` that `matches` takes.
    fn position(&self, hash: u32, matches: impl Fn(&R) -> bool) -> Option<usize> {
        let (index, hashes) = self.words.split_at_checked(index_words(self.slots))?;
        let hit =
            |at: usize| hashes.get(at) == Some(&hash) && self.entries.get(at).is_some_and(&matches);
        let slots = wide_at(self.slots);
        let Some(mask) = slots.checked_sub(1) else {
            return (0..hashes.len()).find(|&at| hit(at));
        };
        let bits = slot_bits(self.slots);
        let mut slot = landing(hash, mask);
        for _ in 0..slots {
            let at = wide_at(slot_in(index, bits, slot)?.checked_sub(1)?);
            if hit(at) {
                return Some(at);
            }
            slot = (slot + 1) & mask;
        }
        None
    }
}

impl<R: Clone, C: Case> Rows<R, C> {
    /// A table laid for `rows` rows, its index among them, so filling it
    /// that far grows nothing: what `zend_hash_extend` does to a class's
    /// table before `zend_do_inheritance` copies the parent's in.
    pub(crate) fn with_capacity(rows: usize) -> Self {
        let slots = slots_for(rows);
        let mut words = Vec::with_capacity(index_words(slots).saturating_add(rows));
        words.resize(index_words(slots), 0);
        Rows {
            entries: Vec::with_capacity(rows).into(),
            words: words.into(),
            slots,
            case: PhantomData,
        }
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

    /// `zend_hash_del` of the row under `name`, where one stands, then
    /// `row` at the end: what `zend_declare_typed_property` makes of an
    /// internal class's redeclaration of a property it inherited. Whether
    /// it was added.
    pub(crate) fn replace<'n>(
        &mut self,
        row: R,
        name: &[u8],
        named: impl Fn(&R) -> &'n [u8],
    ) -> bool {
        let hash = hash::<C>(name);
        if let Some(at) = self.position(hash, |held| same::<C>(named(held), name)) {
            if let Some(entries) = self.entries.own() {
                entries.remove(at);
            }
            let hashed = index_words(self.slots).saturating_add(at);
            if let Some(words) = self.words.own() {
                words.remove(hashed);
            }
            // Every row past it moved down one: the index filed again
            // over the rows as they stand, in the room it has.
            self.refile(self.slots);
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
        for (row, &hash) in from.entries.iter().zip(from.hashes()) {
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
        let slots = slots_for(self.entries.len().saturating_add(more));
        if slots > self.slots {
            self.refile(slots);
        }
        if let Some(entries) = self.entries.own() {
            entries.reserve(more);
        }
        if let Some(words) = self.words.own() {
            words.reserve(more);
        }
    }

    /// `row` at the end, unless the table holds as many rows as a
    /// position can number. Whether it was added.
    fn push(&mut self, row: R, hash: u32) -> bool {
        let Some(filed) = u32::try_from(self.entries.len())
            .ok()
            .and_then(|at| at.checked_add(1))
        else {
            return false;
        };
        let slots = slots_for(wide_at(filed));
        if slots > self.slots {
            self.refile(slots);
        }
        let (Some(entries), Some(words)) = (self.entries.own(), self.words.own()) else {
            return false;
        };
        entries.push(row);
        words.push(hash);
        self.file(hash, filed);
        true
    }

    /// The index laid again at `slots` slots, every row filed in it in
    /// order, so the first of a name is the first a lookup meets.
    fn refile(&mut self, slots: u32) {
        let hashes = self.hashes().to_vec();
        let mut words = Vec::with_capacity(index_words(slots).saturating_add(hashes.len()));
        words.resize(index_words(slots), 0);
        words.extend_from_slice(&hashes);
        self.words = words.into();
        self.slots = slots;
        for (filed, &hash) in (1u32..).zip(&hashes) {
            self.file(hash, filed);
        }
    }

    /// `filed`, a row's position plus one, into the first empty word of
    /// the index from where `hash` lands.
    fn file(&mut self, hash: u32, filed: u32) {
        let (slots, bits) = (wide_at(self.slots), slot_bits(self.slots));
        let Some(mask) = slots.checked_sub(1) else {
            return;
        };
        let words = index_words(self.slots);
        let Some(index) = (self.words.own()).and_then(|own| own.get_mut(..words)) else {
            return;
        };
        let mut slot = landing(hash, mask);
        for _ in 0..slots {
            if slot_in(index, bits, slot) == Some(0) {
                let per = 32 / bits;
                let shift = slot % per * bits;
                if let Some(word) = index.get_mut(slot / per) {
                    let cleared = u64::from(*word) & !(slot_mask(bits) << shift);
                    let set = cleared | (u64::from(filed) & slot_mask(bits)) << shift;
                    *word = u32::try_from(set).unwrap_or(*word);
                }
                return;
            }
            slot = (slot + 1) & mask;
        }
    }
}

/// The bits one slot of an index of `slots` slots is laid in: the
/// fewest of 8, 16 and 32 that number its rows plus one, an index
/// filing at most half as many rows as it has slots. php's `HT_HASH`
/// slot is 32 bits whatever the table's size; the width is not
/// observable, so a class's index is laid in the fewest.
fn slot_bits(slots: u32) -> usize {
    match slots {
        0..=0x100 => 8,
        0x101..=0x2_0000 => 16,
        _ => 32,
    }
}

/// The words an index of `slots` slots fills.
fn index_words(slots: u32) -> usize {
    wide_at(slots).saturating_mul(slot_bits(slots)) / 32
}

/// The low `bits` bits of a word.
fn slot_mask(bits: usize) -> u64 {
    (1u64 << bits) - 1
}

/// What slot `slot` of an index laid at `bits` bits a slot files: a
/// row's position plus one, zero where it files none.
fn slot_in(index: &[u32], bits: usize, slot: usize) -> Option<u32> {
    let per = 32 / bits;
    let word = u64::from(*index.get(slot / per)?);
    u32::try_from(word >> (slot % per * bits) & slot_mask(bits)).ok()
}

/// The words of index a table of `rows` rows is laid with: none up to
/// [`SCANNED`], then the power of two at least twice the rows.
fn slots_for(rows: usize) -> u32 {
    if rows <= SCANNED {
        return 0;
    }
    (rows.checked_mul(2))
        .and_then(usize::checked_next_power_of_two)
        .and_then(|slots| u32::try_from(slots).ok())
        .unwrap_or(0)
}

/// Where a lookup of `hash` starts in an index of `mask + 1` words: the
/// spread hash's top half, its best mixed bits.
fn landing(hash: u32, mask: usize) -> usize {
    usize::try_from(wide(hash) >> 32).unwrap_or(0) & mask
}

/// A position or a length kept as a `u32`, as an index into a slice.
fn wide_at(at: u32) -> usize {
    usize::try_from(at).unwrap_or(usize::MAX)
}

impl<R> Rows<R, Exact> {
    /// [`Rows::find_at`] by `hash`, the name's `zend_inline_hash_func`
    /// as a string caches it ([`php_heap::Str::hash`]): `zend_hash_find`
    /// taking `ZSTR_H(key)`, so the name is hashed once whatever the
    /// number of tables it is looked up in. Only a table filed by exact
    /// bytes takes a string's hash, since a folded one files the name
    /// lowered.
    pub fn find_hashed<'n>(
        &self,
        hash: u64,
        name: &[u8],
        named: impl Fn(&R) -> &'n [u8],
    ) -> Option<(usize, &R)> {
        self.found_at(filed(hash), name, named)
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

/// The hash a table files `name` under: over a folded table's name the
/// words mixed with each byte lowered, over an exact one php's
/// `zend_inline_hash_func`, the hash a string caches, so
/// [`Rows::find_hashed`] can take a string's hash in place of hashing it.
fn hash<C: Case>(name: &[u8]) -> u32 {
    if C::FOLDED {
        top(words(name).fold(0, |hash, word| mix(hash, folded(word))))
    } else {
        filed(php_heap::Str::hash_of(name))
    }
}

/// The word of a string's hash a table keeps: its low half, which
/// differs between short names where the high half holds little past
/// the bit php forces.
fn filed(hash: u64) -> u32 {
    u32::try_from(hash & u64::from(u32::MAX)).unwrap_or_default()
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
    fn an_exact_table_finds_a_name_by_the_hash_a_string_caches() {
        let mut rows: Rows<&'static [u8], Exact> = Rows::default();
        for name in [&b"d"[..], b"p", b"properties"] {
            rows.add(name, name, named);
        }
        let hash = |name: &[u8]| php_heap::Str::hash_of(name);
        assert_eq!(
            rows.find_hashed(hash(b"properties"), b"properties", named),
            Some((2, &&b"properties"[..]))
        );
        assert_eq!(rows.find_hashed(hash(b"q"), b"q", named), None);
        // A hash that is not the name's finds nothing, as php's bucket
        // compare reads the hash before the key.
        assert_eq!(rows.find_hashed(hash(b"p"), b"d", named), None);
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
    fn a_replaced_row_moves_to_the_end_and_each_row_is_found_where_it_stands() {
        let mut rows: Rows<&'static [u8], Exact> = Rows::with_capacity(4);
        for name in [&b"a"[..], b"b", b"c"] {
            rows.add(name, name, named);
        }
        assert!(rows.replace(b"a", b"a", named));
        assert!(rows.replace(b"d", b"d", named));
        assert_eq!(rows.rows(), [&b"b"[..], b"c", b"a", b"d"]);
        let found = [&b"a"[..], b"b", b"c", b"d"].map(|name| rows.find_at(name, named));
        let at = found.map(|found| found.map(|(at, _)| at));
        assert_eq!(at, [Some(2), Some(0), Some(1), Some(3)]);
    }

    #[test]
    fn a_table_past_the_scanned_length_finds_each_row_where_it_stands() {
        let names: Vec<&'static [u8]> = (0..40u8)
            .map(|n| &*Box::leak(format!("m{n}").into_bytes().into_boxed_slice()))
            .collect();
        let mut rows: Rows<&'static [u8], Folded> = Rows::default();
        for &name in &names {
            assert!(rows.add(name, name, named));
        }
        assert!(!rows.add(b"M7", b"M7", named));
        assert!(rows.replace(names[3], names[3], named));
        let at = |name: &[u8], rows: &Rows<&'static [u8], Folded>| {
            rows.find_at(name, named).map(|(at, _)| at)
        };
        assert_eq!(at(b"M3", &rows), Some(39));
        assert_eq!(at(b"m4", &rows), Some(3));
        assert_eq!(at(b"m40", &rows), None);
        let mut child: Rows<&'static [u8], Folded> = Rows::with_capacity(2);
        child.add(b"own", b"own", named);
        child.inherit(&rows, |&row| row, named, named);
        assert_eq!(child.rows().len(), 41);
        assert_eq!(at(b"m39", &child), Some(39));
        assert_eq!(at(b"m3", &child), Some(40));
    }

    #[test]
    fn laid_records_read_as_their_own_columns_did() {
        use super::{Column, LinkedClass, Row};
        let record = |rows: u32| {
            let mut record: LinkedClass<u32> = LinkedClass::default();
            for at in 0..rows {
                let name = format!("p{at}");
                record
                    .properties
                    .add(Row { holder: None, at }, name.as_bytes(), |_| b"");
            }
            record.interfaces = Column::from(vec![rows]);
            record
        };
        let named = |row: &Row<u32>| match row.at {
            0 => &b"p0"[..],
            1 => b"p1",
            11 => b"p11",
            _ => b"",
        };
        let mut records = vec![record(2), record(0), record(12)];
        let own = records.clone();
        LinkedClass::lay(&mut records);
        assert_eq!(records, own);
        assert_eq!(
            records[2]
                .properties
                .find_at(b"p11", named)
                .map(|(at, _)| at),
            Some(11)
        );
        assert_eq!(
            records[0]
                .properties
                .find_at(b"p1", named)
                .map(|(at, _)| at),
            Some(1)
        );
        assert_eq!(records[1].properties.find(b"p0", named), None);
        assert_eq!(*records[2].interfaces, [12]);
        let mut child: Rows<Row<u32>, super::Exact> = Rows::default();
        child.inherit(&records[2].properties, |&row| row, named, named);
        assert_eq!(child.find_at(b"p11", named).map(|(at, _)| at), Some(11));
    }

    #[test]
    fn an_index_of_each_slot_width_finds_each_row_where_it_stands() {
        // 100 rows file in 8-bit slots, 300 in 16-bit ones and 70,000 in
        // 32-bit ones.
        for (count, words) in [(100u32, 64), (300, 512), (70_000, 262_144)] {
            let names: Vec<&'static [u8]> = (0..count)
                .map(|n| &*Box::leak(format!("r{n}").into_bytes().into_boxed_slice()))
                .collect();
            let mut rows: Rows<&'static [u8], Exact> = Rows::default();
            for &name in &names {
                assert!(rows.add(name, name, named));
            }
            assert_eq!(super::index_words(rows.slots), words, "{count}");
            for (at, &name) in names.iter().enumerate() {
                assert_eq!(rows.find_at(name, named).map(|(at, _)| at), Some(at));
            }
            assert_eq!(rows.find(b"r-1", named), None);
        }
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
