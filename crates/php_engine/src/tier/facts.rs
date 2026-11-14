//! How a recording names a class, and what a trace's build reads of one
//! (design §5.14, *Identities*; `jit-design.md` §4.1, *The owned view*;
//! #4653): [`RecordClass`], one the epoch shares by its [`EpochClassId`]
//! and any other by the index its view gave it; [`ClassFacts`], the part
//! of a declaration the build reads, copied by one projection that names
//! every field and keeps no value; and [`Linked`], a class's linked
//! record as the view reads it, its classes named by `RecordClass`.

use php_link::{Accessors, LinkedClass, Row, Rows};
use php_unit::decl::{
    Attribute, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, Name, PropertyDecl, Type,
    Visibility,
};

use super::EpochClassId;

/// A class as a recording names it (design §5.14, *Identities*): one
/// every request of the view's epoch shares, by the identity the arena's
/// one test of it mints ([`EpochClassId`]), or any other, a request's own
/// class, by the index its view gave it ([`OwnClass`]). No recording
/// names a class by a request's number, so a recording holds nothing of
/// its request and its job may wait on the worker after the request has
/// ended. Made at record time by
/// [`UnitView::record_class`](super::UnitView::record_class), which is
/// total over an object's class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, php_macros::EngineState)]
pub enum RecordClass {
    /// A class every request of the epoch shares.
    Shared(EpochClassId),
    /// A request's own class, by its index in the view that recorded it.
    Own(OwnClass),
}

impl RecordClass {
    /// The identity every request of the epoch names the class by; none
    /// for a request's own.
    #[must_use]
    pub fn shared(self) -> Option<EpochClassId> {
        match self {
            RecordClass::Shared(class) => Some(class),
            RecordClass::Own(_) => None,
        }
    }
}

impl From<EpochClassId> for RecordClass {
    fn from(class: EpochClassId) -> RecordClass {
        RecordClass::Shared(class)
    }
}

/// A request's own class as one recording names it: a dense index its
/// [`UnitView`](super::UnitView) mints as it first meets the class, the
/// position of the class's entry among the view's own classes, which
/// names nothing outside that view. The request numbers the class for
/// itself, and another request numbers another class the same, so the
/// view keeps no number of the request's: it finds the class again at
/// record time by its name, which one request declares for one class
/// alone.
///
/// The construction alone makes an index of one view meaningless in
/// another, and the type does not hold it: two views' indices compare
/// as the numbers they are, with no brand of the view that minted them.
/// Each is read back only through the view that minted it, which a
/// recording carries beside its record (`Recorded`), so no reader takes
/// an index to a view it was not minted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, php_macros::EngineState)]
pub struct OwnClass(u32);

impl OwnClass {
    /// The index its view minted, as a recorded trace's text form writes
    /// it (`jit-design.md` §4.1, `TraceRecord::dump`).
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    /// The own class at `index`, read back from a recorded trace's text
    /// form; whether the view the record is read through holds a class
    /// there is the view's to say.
    #[must_use]
    pub const fn at(index: u32) -> OwnClass {
        OwnClass(index)
    }

    /// The class at position `at` among a view's own classes.
    pub(super) fn of(at: usize) -> OwnClass {
        OwnClass(u32::try_from(at).unwrap_or(u32::MAX))
    }

    /// Its position among the view's own classes.
    pub(super) fn position(self) -> usize {
        self.0 as usize
    }
}

/// What a trace's build reads of a class's declaration (#4653): its
/// name, kind, flags and file, each property's name, visibility,
/// staticness and type, and each constant's name, type, visibility and
/// attribute names, which is what php's tracing tier reads. Made by one
/// projection, [`ClassFacts::of`], which names every field of the
/// declaration and keeps no `ConstExpr`, so no value of the request's
/// stands in it: a constant's value, a property's default, a method's
/// parameters and an attribute's arguments are left behind.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassFacts {
    pub name: Name,
    pub kind: ClassKind,
    pub flags: ClassFlags,
    /// The file it was declared in, empty for one of the engine's own.
    pub file: Name,
    pub properties: Box<[PropertyFacts]>,
    pub constants: Box<[ConstantFacts]>,
}

/// What a trace's build reads of one property's declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyFacts {
    pub name: Name,
    pub visibility: Visibility,
    /// Not `static`, php's `ZEND_ACC_STATIC` clear.
    pub is_instance: bool,
    pub ty: Option<Type>,
}

/// What a trace's build reads of one constant's declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstantFacts {
    pub name: Name,
    pub ty: Option<Type>,
    pub visibility: Visibility,
    /// The names of its attributes, as written.
    pub attributes: Box<[Name]>,
}

impl ClassFacts {
    /// What a trace's build reads of `decl`: the one projection, which
    /// names every field of the declaration, of each property, of each
    /// constant and of each attribute, so a field the unit format gains
    /// does not compile until this decides it.
    #[must_use]
    pub(crate) fn of(decl: &ClassDecl) -> ClassFacts {
        let ClassDecl {
            name,
            kind,
            flags,
            parent: _,
            interfaces: _,
            implementors: _,
            aliases: _,
            traits: _,
            adaptations: _,
            backing: _,
            constants,
            properties,
            methods: _,
            attributes: _,
            doc_comment: _,
            file,
            line: _,
            end_line: _,
            object_size: _,
            object_beside: _,
            attribute_validator: _,
        } = decl;
        ClassFacts {
            name: name.clone(),
            kind: *kind,
            flags: *flags,
            file: file.clone(),
            properties: properties.iter().map(PropertyFacts::of).collect(),
            constants: constants.iter().map(ConstantFacts::of).collect(),
        }
    }
}

impl PropertyFacts {
    fn of(decl: &PropertyDecl) -> PropertyFacts {
        let PropertyDecl {
            name,
            visibility,
            set_visibility: _,
            is_instance,
            is_readonly: _,
            is_final: _,
            is_promoted: _,
            is_virtual: _,
            ty,
            default: _,
            attributes: _,
            doc_comment: _,
            hooks: _,
        } = decl;
        PropertyFacts {
            name: name.clone(),
            visibility: *visibility,
            is_instance: *is_instance,
            ty: ty.clone(),
        }
    }
}

impl ConstantFacts {
    fn of(decl: &ClassConstDecl) -> ConstantFacts {
        let ClassConstDecl {
            name,
            visibility,
            is_final: _,
            is_case: _,
            ty,
            value: _,
            attributes,
            doc_comment: _,
        } = decl;
        ConstantFacts {
            name: name.clone(),
            ty: ty.clone(),
            visibility: *visibility,
            attributes: attributes.iter().map(attribute_name).collect(),
        }
    }
}

/// An attribute's name, its arguments left behind.
fn attribute_name(attribute: &Attribute) -> Name {
    let Attribute {
        name,
        arguments: _,
        line: _,
        strict: _,
        own_name: _,
    } = attribute;
    name.clone()
}

/// A class's linked record as a view reads it, each class it names as a
/// [`RecordClass`]: a request's own class's, copied as the request
/// linked it and keyed at record time, or one the epoch keeps, borrowed
/// and keyed as it is read, the engine's own and a published row's by
/// the identities the arena minted.
#[derive(Clone, Copy)]
pub struct Linked<'v> {
    class: RecordClass,
    record: Record<'v>,
}

impl std::fmt::Debug for Linked<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Linked").field(&self.class).finish()
    }
}

/// Where a [`Linked`] record stands.
#[derive(Clone, Copy)]
enum Record<'v> {
    Own(&'v LinkedClass<RecordClass>),
    Native(&'v LinkedClass<EpochClassId>),
    Published(&'v LinkedClass<EpochClassId>),
}

impl<'v> Linked<'v> {
    /// The record of `class`, a request's own, copied into the view.
    pub(super) fn own(class: RecordClass, record: &'v LinkedClass<RecordClass>) -> Linked<'v> {
        Linked {
            class,
            record: Record::Own(record),
        }
    }

    /// The record of `class`, one of the engine's own, as the epoch keeps
    /// it.
    pub(super) fn native(class: EpochClassId, record: &'v LinkedClass<EpochClassId>) -> Linked<'v> {
        Linked {
            class: RecordClass::Shared(class),
            record: Record::Native(record),
        }
    }

    /// The record of `class`, a row of the epoch's arena, which names
    /// every class by the identity the arena minted (#4652).
    pub(super) fn published(
        class: EpochClassId,
        record: &'v LinkedClass<EpochClassId>,
    ) -> Linked<'v> {
        Linked {
            class: RecordClass::Shared(class),
            record: Record::Published(record),
        }
    }

    /// `ce->constructor` is set.
    #[must_use]
    pub fn constructs(self) -> bool {
        match self.record {
            Record::Own(record) => record.constructor.is_some(),
            Record::Native(record) | Record::Published(record) => record.constructor.is_some(),
        }
    }

    /// `ce->__get`, `ce->__set`, `ce->__isset` and `ce->__unset`.
    #[must_use]
    pub fn accessors(self) -> Accessors {
        match self.record {
            Record::Own(record) => record.accessors,
            Record::Native(record) | Record::Published(record) => record.accessors,
        }
    }

    /// Whether `ce->interfaces` holds `of`.
    #[must_use]
    pub fn implements(self, of: RecordClass) -> bool {
        match self.record {
            Record::Own(record) => record.interfaces.contains(&of),
            Record::Native(record) | Record::Published(record) => of
                .shared()
                .is_some_and(|of| record.interfaces.contains(&of)),
        }
    }

    /// The row of `ce->properties_info` under `name`, by the class that
    /// declared it and its index among that class's properties, whose own
    /// name `named` reads; none where no row is.
    pub fn property<'n>(
        self,
        name: &[u8],
        named: impl Fn(RecordClass, u32) -> Option<&'n [u8]>,
    ) -> Option<(RecordClass, u32)> {
        match self.record {
            Record::Own(record) => found(&record.properties, self.class, Some, name, named),
            Record::Native(record) | Record::Published(record) => found(
                &record.properties,
                self.class,
                |c| Some(c.into()),
                name,
                named,
            ),
        }
    }

    /// The row of `CE_CONSTANTS_TABLE` under `name`, as
    /// [`Linked::property`] finds a property's.
    pub fn constant<'n>(
        self,
        name: &[u8],
        named: impl Fn(RecordClass, u32) -> Option<&'n [u8]>,
    ) -> Option<(RecordClass, u32)> {
        match self.record {
            Record::Own(record) => found(&record.constants, self.class, Some, name, named),
            Record::Native(record) | Record::Published(record) => found(
                &record.constants,
                self.class,
                |c| Some(c.into()),
                name,
                named,
            ),
        }
    }
}

/// The row of `rows`, a record of `class` whose holders `key` names as a
/// view does, under `name`, by its holder and its index.
fn found<'n, H: Copy>(
    rows: &Rows<Row<H>, php_link::Exact>,
    class: RecordClass,
    key: impl Fn(H) -> Option<RecordClass>,
    name: &[u8],
    named: impl Fn(RecordClass, u32) -> Option<&'n [u8]>,
) -> Option<(RecordClass, u32)> {
    let holder = |row: &Row<H>| match row.holder {
        Some(holder) => key(holder),
        None => Some(class),
    };
    let row = rows.find(name, |row| {
        holder(row)
            .and_then(|holder| named(holder, row.at))
            .unwrap_or_default()
    })?;
    Some((holder(row)?, row.at))
}
