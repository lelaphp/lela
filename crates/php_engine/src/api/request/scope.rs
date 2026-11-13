//! The two scopes a row of a class or a function stands in (design
//! §5.14, *Identities*): a request's own, [`InRequest`], and the epoch's
//! arena, [`InArena`]. One parameter, [`Scope`], carries what differs
//! between them together: the unit a row's body is read from, the
//! declaration a class composed with its traits, the class and the
//! function a row names, the runtime cache its op array is found at and
//! the record of the op array another process reads. A row of one scope
//! names only what that scope admits, and the type says which: an arena
//! row holds a [`Frozen`] unit and declaration, an [`EpochClassId`], an
//! [`EpochFunction`] and an [`EngineFuncRef`], each engine state, so the
//! row is engine state by the derive; a request's row holds a
//! [`PublishedUnit`], a [`ClassId`], a [`Function`] and a [`CacheAt`],
//! none of which is, as a request's own unit, class and function are not.
//!
//! A request reads both scopes' rows through one borrowed view per kind
//! of row, a [`FunctionRow`], a [`ClassRow`], a [`ClassRecord`] and a
//! [`PlacedOf`] with its [`PlacedSlots`], which hand what a reader asks for in the request's
//! names: an arena row's identities widen to the ones a request names
//! them by, which is the same number, and its frozen unit is read
//! through. Nothing of an arena row is copied into the request.
//!
//! A row of the arena is never built here: a publisher builds the
//! request's row and moves it into the arena's scope through the arena's
//! one check of each class and function it names, and the frozen unit
//! it was compiled into ([`UserFunction::published`],
//! [`Placed::published`], and a class's row in `Request::publish`).

use std::cell::{Cell, OnceCell};
use std::fmt::Debug;
use std::hash::Hash;
use std::ops::Range;

use php_link::{Case, Exact, Folded, LinkedClass, Row, Rows};
use php_unit::decl::ClassDecl;
use php_unit::frozen::Frozen;
use php_unit::{MapPtrOffset, PublishedUnit};

use super::{Declared, FrameShape, Member, Placed, PropertyInfo, RowPlace, UserFunction};
use crate::api::call::Function;
use crate::api::values::ClassId;
use crate::arena::{EpochClassId, EpochFunction};
use crate::exec::{self, CacheAt};
use crate::readable::{CalleeAt, FrozenRecord, Record};
use crate::tier::{EngineFuncRef, FuncRef};

/// Where a row stands, and so what it may name: one parameter for
/// everything a row of a class or a function holds that differs between
/// a request's own rows and the arena's.
pub(crate) trait Scope: Copy + Default + Eq + Debug + Hash + Send + Sync + 'static {
    /// The unit a row's body and declaration are read from.
    type Unit: Clone + Debug;
    /// The declaration a class composed with its traits at its link.
    type Decl: Clone + Debug;
    /// A class a row names.
    type Class: Copy + Eq + Hash + Debug + Into<ClassId>;
    /// A function a row names, a method of the class among them.
    type Function: Copy + Eq + Debug + Into<Function>;
    /// Where an op array's runtime cache is found.
    type Cache: Copy + Eq + Debug;
    /// The record of a function's op array another process reads
    /// ([`crate::readable`]).
    type Record: Debug;

    /// The unit as every reader reads it.
    fn unit(unit: &Self::Unit) -> &PublishedUnit;

    /// The declaration as every reader reads it.
    fn decl(decl: &Self::Decl) -> &ClassDecl;

    /// The cache as a request finds it.
    fn cache(cache: Self::Cache) -> CacheAt;

    /// Where the record stands, which a frame serving it names.
    fn callee_at(record: &Self::Record) -> CalleeAt;
}

/// A request's own rows: what it declared and bound itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct InRequest;

/// The epoch's arena's rows, which every request of the epoch shares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct InArena;

impl Scope for InRequest {
    type Unit = PublishedUnit;
    type Decl = ClassDecl;
    type Class = ClassId;
    type Function = Function;
    type Cache = CacheAt;
    type Record = Record;

    fn unit(unit: &PublishedUnit) -> &PublishedUnit {
        unit
    }

    fn decl(decl: &ClassDecl) -> &ClassDecl {
        decl
    }

    #[inline]
    fn cache(cache: CacheAt) -> CacheAt {
        cache
    }

    #[inline]
    fn callee_at(record: &Record) -> CalleeAt {
        record.at()
    }
}

impl Scope for InArena {
    type Unit = Frozen<PublishedUnit>;
    type Decl = Frozen<ClassDecl>;
    type Class = EpochClassId;
    type Function = EpochFunction;
    type Cache = EngineFuncRef;
    type Record = FrozenRecord;

    fn unit(unit: &Frozen<PublishedUnit>) -> &PublishedUnit {
        unit
    }

    fn decl(decl: &Frozen<ClassDecl>) -> &ClassDecl {
        decl
    }

    #[inline]
    fn cache(cache: EngineFuncRef) -> CacheAt {
        CacheAt::of(MapPtrOffset::Engine(cache.offset()))
    }

    #[inline]
    fn callee_at(record: &FrozenRecord) -> CalleeAt {
        record.at()
    }
}

impl<S: Scope> FrameShape<S> {
    /// The shape as a request reads it, its cache where the request
    /// finds it.
    #[inline]
    pub(crate) fn widened(self) -> FrameShape {
        FrameShape {
            parameters: self.parameters,
            typed: self.typed,
            cvs: self.cvs,
            slots: self.slots,
            observed: self.observed,
            entry: self.entry,
            cache: S::cache(self.cache),
            cache_size: self.cache_size,
            by_ref: self.by_ref,
            sent: self.sent,
        }
    }
}

impl<S: Scope> Member<S> {
    /// The member as a request names it.
    pub(crate) fn widened(self) -> Member {
        Member {
            class: self.class.into(),
            at: self.at,
        }
    }
}

impl<S: Scope> PropertyInfo<S> {
    /// The property as a request names it.
    pub(crate) fn widened(self) -> PropertyInfo {
        PropertyInfo {
            declaring: self.declaring.into(),
            at: self.at,
        }
    }
}

impl UserFunction {
    /// The row moved into the arena's scope: its unit `unit`, which is
    /// the one it was built over, frozen; its member's class `class`,
    /// the identity the arena minted for the class it is a method of;
    /// and its cache the engine's. None where `unit` is another unit,
    /// where a member's class is not `class`, and for a cache of a
    /// request's own, so no row of the arena names what a request made.
    pub(crate) fn published(
        self,
        unit: Frozen<PublishedUnit>,
        class: Option<EpochClassId>,
    ) -> Option<UserFunction<InArena>> {
        if !PublishedUnit::ptr_eq(&self.unit, &unit) {
            return None;
        }
        let member = match (self.member, class) {
            (None, _) => None,
            (Some(member), Some(class)) if member.class == class => Some(Member {
                class,
                at: member.at,
            }),
            (Some(_), _) => return None,
        };
        let cache = EngineFuncRef::of(FuncRef::at(self.shape.cache.offset()))?;
        let shape = FrameShape {
            parameters: self.shape.parameters,
            typed: self.shape.typed,
            cvs: self.shape.cvs,
            slots: self.shape.slots,
            observed: self.shape.observed,
            entry: self.shape.entry,
            cache,
            cache_size: self.shape.cache_size,
            by_ref: self.shape.by_ref,
            sent: self.shape.sent,
        };
        let record = FrozenRecord::beside(self.record, &unit);
        Some(UserFunction {
            unit,
            body: self.body,
            member,
            flags: self.flags,
            shape,
            record,
        })
    }
}

/// A user function or method a request reads, wherever its row stands:
/// one of the request's own or one of the arena's.
#[derive(Clone, Copy)]
pub(crate) enum FunctionRow<'r> {
    Own(&'r UserFunction),
    Arena(&'r UserFunction<InArena>),
}

impl<'r> FunctionRow<'r> {
    /// The unit its body is read from.
    #[inline]
    pub(crate) fn unit(self) -> &'r PublishedUnit {
        match self {
            FunctionRow::Own(row) => &row.unit,
            FunctionRow::Arena(row) => &row.unit,
        }
    }

    /// Its body in the unit.
    #[inline]
    pub(crate) fn body(self) -> exec::Body {
        match self {
            FunctionRow::Own(row) => row.body,
            FunctionRow::Arena(row) => row.body,
        }
    }

    /// For a method, its class and its place there.
    #[inline]
    pub(crate) fn member(self) -> Option<Member> {
        match self {
            FunctionRow::Own(row) => row.member,
            FunctionRow::Arena(row) => row.member.map(Member::widened),
        }
    }

    /// [`UserFunction::flags`].
    #[inline]
    pub(crate) fn flags(self) -> crate::attributes::Flags {
        match self {
            FunctionRow::Own(row) => row.flags,
            FunctionRow::Arena(row) => row.flags,
        }
    }

    /// What every call's frame push and activation read of it.
    #[inline]
    pub(crate) fn shape(self) -> FrameShape {
        match self {
            FunctionRow::Own(row) => row.shape,
            FunctionRow::Arena(row) => row.shape.widened(),
        }
    }

    /// Where its record stands, which a frame serving it names.
    #[inline]
    pub(crate) fn callee_at(self) -> CalleeAt {
        match self {
            FunctionRow::Own(row) => row.callee_at(),
            FunctionRow::Arena(row) => row.callee_at(),
        }
    }
}

/// A class a request reads the row of, wherever it stands: one the
/// request declared or one the arena holds.
#[derive(Clone, Copy)]
pub(crate) enum ClassRow<'r> {
    Own(&'r Declared),
    Arena(&'r Declared<InArena>),
}

impl<'r> ClassRow<'r> {
    /// The unit that holds its declaration.
    pub(crate) fn unit(self) -> &'r PublishedUnit {
        match self {
            ClassRow::Own(row) => row.unit(),
            ClassRow::Arena(row) => row.unit(),
        }
    }

    /// The unit, where the arena holds it frozen.
    pub(crate) fn frozen(self) -> Option<&'r Frozen<PublishedUnit>> {
        match self {
            ClassRow::Own(_) => None,
            ClassRow::Arena(row) => Some(&row.unit),
        }
    }

    /// The declaration's index in its unit.
    pub(crate) fn at(self) -> u32 {
        match self {
            ClassRow::Own(row) => row.at,
            ClassRow::Arena(row) => row.at,
        }
    }

    /// The declaration the table holds for the class.
    pub(crate) fn decl(self) -> Option<&'r ClassDecl> {
        match self {
            ClassRow::Own(row) => row.decl(),
            ClassRow::Arena(row) => row.decl(),
        }
    }

    /// For a class that uses traits, its declaration composed with them.
    pub(crate) fn composed(self) -> Option<Composition<'r>> {
        match self {
            ClassRow::Own(row) => (row.composed.as_deref()).map(|composed| Composition {
                decl: &composed.decl,
                bodies: &composed.bodies,
                abstracts: composed.abstracts,
            }),
            ClassRow::Arena(row) => (row.composed.as_deref()).map(|composed| Composition {
                decl: &composed.decl,
                bodies: &composed.bodies,
                abstracts: composed.abstracts,
            }),
        }
    }

    /// Its linked record, once it bound.
    pub(crate) fn record(self) -> Option<ClassRecord<'r>> {
        match self {
            ClassRow::Own(row) => row.bound.record.as_ref().map(ClassRecord::Request),
            ClassRow::Arena(row) => row.bound.record.as_ref().map(ClassRecord::Arena),
        }
    }

    /// The function its own method at `at` is.
    pub(crate) fn function(self, at: usize) -> Option<Function> {
        match self {
            ClassRow::Own(row) => row.bound.functions.get(at).copied().flatten(),
            ClassRow::Arena(row) => {
                (row.bound.functions.get(at).copied().flatten()).map(Into::into)
            }
        }
    }

    /// The numbers its methods and hooks took as user functions.
    pub(crate) fn members(self) -> Range<u32> {
        match self {
            ClassRow::Own(row) => row.bound.members.clone(),
            ClassRow::Arena(row) => row.bound.members.clone(),
        }
    }

    /// `ce->parent`, where its name named a class as the class bound.
    pub(crate) fn parent(self) -> Option<ClassId> {
        match self {
            ClassRow::Own(row) => row.bound.parent,
            ClassRow::Arena(row) => row.bound.parent.map(Into::into),
        }
    }

    /// Its placed properties.
    pub(crate) fn placed(self) -> PlacedOf<'r> {
        match self {
            ClassRow::Own(row) => PlacedOf::Request(&row.bound.placed),
            ClassRow::Arena(row) => PlacedOf::Arena(&row.bound.placed),
        }
    }
}

/// A class's declaration composed with its traits, as a request reads
/// it ([`php_link::Composed`]).
#[derive(Clone, Copy)]
pub(crate) struct Composition<'r> {
    pub(crate) decl: &'r ClassDecl,
    pub(crate) bodies: &'r [php_link::Body],
    pub(crate) abstracts: u32,
}

/// A class's linked record a request reads: one keyed by the classes a
/// request names, a request's own class's or an engine's class's as the
/// engine linked it, or an arena row's, keyed by the epoch's.
#[derive(Clone, Copy)]
pub(crate) enum ClassRecord<'r> {
    Request(&'r LinkedClass<ClassId>),
    Arena(&'r LinkedClass<EpochClassId>),
}

/// `row` naming its holder as a request names it.
fn widened<H: Copy + Into<ClassId>>(row: &Row<H>) -> Row<ClassId> {
    Row {
        holder: row.holder.map(Into::into),
        at: row.at,
    }
}

impl<'r> ClassRecord<'r> {
    /// `ce->function_table`.
    pub(crate) fn methods(self) -> RowsOf<'r, Folded> {
        match self {
            ClassRecord::Request(record) => RowsOf::Request(&record.methods),
            ClassRecord::Arena(record) => RowsOf::Arena(&record.methods),
        }
    }

    /// `ce->constants_table`.
    pub(crate) fn constants(self) -> RowsOf<'r, Exact> {
        match self {
            ClassRecord::Request(record) => RowsOf::Request(&record.constants),
            ClassRecord::Arena(record) => RowsOf::Arena(&record.constants),
        }
    }

    /// `ce->properties_info`.
    pub(crate) fn properties(self) -> RowsOf<'r, Exact> {
        match self {
            ClassRecord::Request(record) => RowsOf::Request(&record.properties),
            ClassRecord::Arena(record) => RowsOf::Arena(&record.properties),
        }
    }

    /// The prototype's scope of the method at `at`, where it has one.
    pub(crate) fn prototype(self, at: usize) -> Option<ClassId> {
        match self {
            ClassRecord::Request(record) => record.prototypes.get(at).copied().flatten(),
            ClassRecord::Arena(record) => {
                (record.prototypes.get(at).copied().flatten()).map(Into::into)
            }
        }
    }

    /// `ce->interfaces`, in order.
    pub(crate) fn interfaces(
        self,
    ) -> impl DoubleEndedIterator<Item = ClassId> + ExactSizeIterator + 'r {
        let (request, arena): (&[ClassId], &[EpochClassId]) = match self {
            ClassRecord::Request(record) => (&record.interfaces, &[]),
            ClassRecord::Arena(record) => (&[], &record.interfaces),
        };
        let arena = arena.iter().map(|class| ClassId::from(*class));
        Either {
            request: request.iter().copied(),
            arena,
        }
    }

    /// `ce->constructor`, by its place among the methods.
    pub(crate) fn constructor(self) -> Option<u32> {
        match self {
            ClassRecord::Request(record) => record.constructor,
            ClassRecord::Arena(record) => record.constructor,
        }
    }

    /// `ce->destructor`, by its place among the methods.
    pub(crate) fn destructor(self) -> Option<u32> {
        match self {
            ClassRecord::Request(record) => record.destructor,
            ClassRecord::Arena(record) => record.destructor,
        }
    }

    /// `ce->__get` and its three neighbours.
    pub(crate) fn accessors(self) -> php_link::Accessors {
        match self {
            ClassRecord::Request(record) => record.accessors,
            ClassRecord::Arena(record) => record.accessors,
        }
    }

    /// What the class's property hooks ask of its handlers.
    pub(crate) fn hooks(self) -> php_link::ClassHooks {
        match self {
            ClassRecord::Request(record) => record.hooks,
            ClassRecord::Arena(record) => record.hooks,
        }
    }
}

/// How many records one chunk of [`LinkRecords`] holds.
const LINK_CHUNK: usize = 32;

/// Records of arena rows keyed as a request names its classes, for one
/// link (`php_link::link`), whose lookups read one key, [`ClassId`]: a
/// record a request keys so is lent as it stands, and an arena row's is
/// widened once per link the first time the link asks for its class,
/// each in a cell of a chunk laid on first use and never moved, so what
/// it lends lives as long as the link. A copy is made where a request
/// links a class over a published one, never where a request runs.
pub(crate) struct LinkRecords {
    chunks: [OnceCell<Box<LinkChunk>>; 32],
    filled: Cell<usize>,
}

/// One chunk of [`LinkRecords`]: a cell for each record widened.
type LinkChunk = [OnceCell<(ClassId, LinkedClass<ClassId>)>; LINK_CHUNK];

impl LinkRecords {
    /// None widened yet.
    pub(crate) fn new() -> LinkRecords {
        LinkRecords {
            chunks: std::array::from_fn(|_| OnceCell::new()),
            filled: Cell::new(0),
        }
    }

    /// `record`, `class`'s, keyed as a request names its classes. None
    /// past the thousand classes one link can widen, more than any link
    /// reads.
    pub(crate) fn keyed<'s>(
        &'s self,
        class: ClassId,
        record: ClassRecord<'s>,
    ) -> Option<&'s LinkedClass<ClassId>> {
        let record = match record {
            ClassRecord::Request(record) => return Some(record),
            ClassRecord::Arena(record) => record,
        };
        let filled = self.filled.get();
        for at in 0..filled {
            let (held, widened) = self.cell(at)?.get()?;
            if *held == class {
                return Some(widened);
            }
        }
        let cell = self.cell(filled)?;
        let widened = record.try_map(|class| Some(ClassId::from(class)))?;
        self.filled.set(filled + 1);
        Some(&cell.get_or_init(|| (class, widened)).1)
    }

    /// The cell at `at`, its chunk laid where none was.
    fn cell(&self, at: usize) -> Option<&OnceCell<(ClassId, LinkedClass<ClassId>)>> {
        let chunk = self
            .chunks
            .get(at / LINK_CHUNK)?
            .get_or_init(|| Box::new(std::array::from_fn(|_| OnceCell::new())));
        chunk.get(at % LINK_CHUNK)
    }
}

/// The interfaces of one of the two records, as one iterator.
struct Either<A, B> {
    request: A,
    arena: B,
}

impl<A: Iterator<Item = ClassId>, B: Iterator<Item = ClassId>> Iterator for Either<A, B> {
    type Item = ClassId;

    fn next(&mut self) -> Option<ClassId> {
        self.request.next().or_else(|| self.arena.next())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (a, b) = (self.request.size_hint(), self.arena.size_hint());
        (a.0 + b.0, a.1.zip(b.1).map(|(a, b)| a + b))
    }
}

impl<A, B> DoubleEndedIterator for Either<A, B>
where
    A: DoubleEndedIterator<Item = ClassId>,
    B: DoubleEndedIterator<Item = ClassId>,
{
    fn next_back(&mut self) -> Option<ClassId> {
        self.arena.next_back().or_else(|| self.request.next_back())
    }
}

impl<A, B> ExactSizeIterator for Either<A, B>
where
    A: ExactSizeIterator<Item = ClassId>,
    B: ExactSizeIterator<Item = ClassId>,
{
}

/// One table of a [`ClassRecord`], each row read as a request names its
/// holder.
pub(crate) enum RowsOf<'r, C> {
    Request(&'r Rows<Row<ClassId>, C>),
    Arena(&'r Rows<Row<EpochClassId>, C>),
}

impl<C> Clone for RowsOf<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for RowsOf<'_, C> {}

impl<'r, C: Case> RowsOf<'r, C> {
    /// The table's rows, read as a slice of them is: [`RowsOf::len`],
    /// [`RowsOf::get`] and [`RowsOf::iter`].
    pub(crate) fn rows(self) -> RowsOf<'r, C> {
        self
    }

    /// How many rows the table holds.
    pub(crate) fn len(self) -> usize {
        match self {
            RowsOf::Request(rows) => rows.rows().len(),
            RowsOf::Arena(rows) => rows.rows().len(),
        }
    }

    /// Whether the table holds no row.
    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The row at `at`.
    pub(crate) fn get(self, at: usize) -> Option<Row<ClassId>> {
        match self {
            RowsOf::Request(rows) => rows.rows().get(at).copied(),
            RowsOf::Arena(rows) => rows.rows().get(at).map(widened),
        }
    }

    /// Every row in order.
    pub(crate) fn iter(
        self,
    ) -> impl DoubleEndedIterator<Item = Row<ClassId>> + ExactSizeIterator + 'r {
        RowsOf::each(Some(self))
    }

    /// Every row of `rows` in order, none where there is no table.
    pub(crate) fn each(
        rows: Option<RowsOf<'r, C>>,
    ) -> impl DoubleEndedIterator<Item = Row<ClassId>> + ExactSizeIterator + 'r {
        let (request, arena): (&[Row<ClassId>], &[Row<EpochClassId>]) = match rows {
            Some(RowsOf::Request(rows)) => (rows.rows(), &[]),
            Some(RowsOf::Arena(rows)) => (&[], rows.rows()),
            None => (&[], &[]),
        };
        RowsIter {
            request: request.iter(),
            arena: arena.iter(),
        }
    }

    /// The row filed under `name`, each row's name read by `named`.
    pub(crate) fn find<'n>(
        self,
        name: &[u8],
        named: impl Fn(Row<ClassId>) -> &'n [u8],
    ) -> Option<Row<ClassId>> {
        self.find_at(name, named).map(|(_, row)| row)
    }

    /// [`RowsOf::find`], with the row's place.
    pub(crate) fn find_at<'n>(
        self,
        name: &[u8],
        named: impl Fn(Row<ClassId>) -> &'n [u8],
    ) -> Option<(usize, Row<ClassId>)> {
        match self {
            RowsOf::Request(rows) => rows
                .find_at(name, |row| named(*row))
                .map(|(at, row)| (at, *row)),
            RowsOf::Arena(rows) => {
                (rows.find_at(name, |row| named(widened(row)))).map(|(at, row)| (at, widened(row)))
            }
        }
    }
}

impl RowsOf<'_, Exact> {
    /// [`RowsOf::find_at`] by the name's hash, taken already.
    #[inline]
    pub(crate) fn find_hashed<'n>(
        self,
        hash: u64,
        name: &[u8],
        named: impl Fn(Row<ClassId>) -> &'n [u8],
    ) -> Option<(usize, Row<ClassId>)> {
        match self {
            RowsOf::Request(rows) => {
                (rows.find_hashed(hash, name, |row| named(*row))).map(|(at, row)| (at, *row))
            }
            RowsOf::Arena(rows) => (rows.find_hashed(hash, name, |row| named(widened(row))))
                .map(|(at, row)| (at, widened(row))),
        }
    }
}

/// The rows of one of the two tables, as one iterator.
struct RowsIter<'r> {
    request: std::slice::Iter<'r, Row<ClassId>>,
    arena: std::slice::Iter<'r, Row<EpochClassId>>,
}

impl Iterator for RowsIter<'_> {
    type Item = Row<ClassId>;

    fn next(&mut self) -> Option<Row<ClassId>> {
        (self.request.next().copied()).or_else(|| self.arena.next().map(widened))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.request.len() + self.arena.len();
        (len, Some(len))
    }
}

impl DoubleEndedIterator for RowsIter<'_> {
    fn next_back(&mut self) -> Option<Row<ClassId>> {
        (self.arena.next_back().map(widened)).or_else(|| self.request.next_back().copied())
    }
}

impl ExactSizeIterator for RowsIter<'_> {}

/// A class's placed properties a request reads, wherever the class's
/// row stands.
#[derive(Clone, Copy)]
pub(crate) enum PlacedOf<'r> {
    Request(&'r Placed),
    Arena(&'r Placed<InArena>),
}

impl<'r> PlacedOf<'r> {
    /// The declared instance properties in slot order.
    pub(crate) fn slots(self) -> PlacedSlots<'r> {
        match self {
            PlacedOf::Request(placed) => PlacedSlots::Request(&placed.slots),
            PlacedOf::Arena(placed) => PlacedSlots::Arena(&placed.slots),
        }
    }

    /// What each row of the class's `properties_info` places its name at.
    pub(crate) fn rows(self) -> &'r [RowPlace] {
        match self {
            PlacedOf::Request(placed) => &placed.rows,
            PlacedOf::Arena(placed) => &placed.rows,
        }
    }

    /// [`Placed::holes`].
    pub(crate) fn holes(self) -> u32 {
        match self {
            PlacedOf::Request(placed) => placed.holes,
            PlacedOf::Arena(placed) => placed.holes,
        }
    }
}

/// A class's declared instance properties in slot order, each named as
/// a request names its declaring class.
#[derive(Clone, Copy)]
pub(crate) enum PlacedSlots<'r> {
    Request(&'r [PropertyInfo]),
    Arena(&'r [PropertyInfo<InArena>]),
}

impl<'r> PlacedSlots<'r> {
    /// None: a class with no placed properties.
    pub(crate) const NONE: PlacedSlots<'static> = PlacedSlots::Request(&[]);

    /// How many slots.
    pub(crate) fn len(self) -> usize {
        match self {
            PlacedSlots::Request(slots) => slots.len(),
            PlacedSlots::Arena(slots) => slots.len(),
        }
    }

    /// Whether there is none.
    pub(crate) fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The property in slot `at`.
    #[inline]
    pub(crate) fn get(self, at: usize) -> Option<PropertyInfo> {
        match self {
            PlacedSlots::Request(slots) => slots.get(at).copied(),
            PlacedSlots::Arena(slots) => slots.get(at).map(|info| info.widened()),
        }
    }

    /// Each slot's property in order.
    pub(crate) fn iter(
        self,
    ) -> impl DoubleEndedIterator<Item = PropertyInfo> + ExactSizeIterator + 'r {
        let (request, arena): (&[PropertyInfo], &[PropertyInfo<InArena>]) = match self {
            PlacedSlots::Request(slots) => (slots, &[]),
            PlacedSlots::Arena(slots) => (&[], slots),
        };
        SlotsIter {
            request: request.iter(),
            arena: arena.iter(),
        }
    }

    /// Each slot's property, collected.
    pub(crate) fn to_vec(self) -> Vec<PropertyInfo> {
        self.iter().collect()
    }
}

/// The slots of one of the two placements, as one iterator.
struct SlotsIter<'r> {
    request: std::slice::Iter<'r, PropertyInfo>,
    arena: std::slice::Iter<'r, PropertyInfo<InArena>>,
}

impl Iterator for SlotsIter<'_> {
    type Item = PropertyInfo;

    fn next(&mut self) -> Option<PropertyInfo> {
        (self.request.next().copied()).or_else(|| self.arena.next().map(|info| info.widened()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.request.len() + self.arena.len();
        (len, Some(len))
    }
}

impl DoubleEndedIterator for SlotsIter<'_> {
    fn next_back(&mut self) -> Option<PropertyInfo> {
        (self.arena.next_back().map(|info| info.widened()))
            .or_else(|| self.request.next_back().copied())
    }
}

impl ExactSizeIterator for SlotsIter<'_> {}
