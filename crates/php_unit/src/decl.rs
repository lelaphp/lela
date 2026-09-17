//! The declaration model (design §4.7): the unlinked description of a
//! class, function, constant or attribute as its producer wrote it, with
//! constant expressions still unevaluated. Produced by the compiler for
//! user code and by the native macros for natives; consumed by the linker,
//! Reflection, the dumpers and the stub cross-check, which read one model.
//! Data only: no method here, since the seventeen types are re-exported
//! by `php_engine::api::decl` and their surface is `api.lock`'s.

/// A name as written: bytes, interned by the engine at publication.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Name(pub Box<[u8]>);

impl From<&str> for Name {
    fn from(s: &str) -> Name {
        Name(s.as_bytes().into())
    }
}

/// php's three member visibilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Visibility {
    #[default]
    Public,
    Protected,
    Private,
}

/// What a class declaration declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClassKind {
    #[default]
    Class,
    Interface,
    Trait,
    Enum,
}

/// The modifiers of a class (`ZEND_ACC_*` on the class entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ClassFlags {
    pub is_abstract: bool,
    pub is_final: bool,
    pub is_readonly: bool,
}

/// The backing type of an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EnumBacking {
    #[default]
    Pure,
    Int,
    String,
}

/// A declared type: scalar names, class names, `?T`, unions,
/// intersections and DNF, as `zend_type` holds them. The tentative bit of
/// a native method's return lives on the method (design §4.7).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Mixed,
    Void,
    Never,
    Null,
    Bool,
    False,
    True,
    Int,
    Float,
    String,
    Array,
    Object,
    Iterable,
    Callable,
    /// The `static` type: the called class.
    CalledClass,
    /// `self` and `parent`, resolved at link time.
    SelfClass,
    ParentClass,
    Named(Name),
    Nullable(Box<Type>),
    Union(Vec<Type>),
    Intersection(Vec<Type>),
}

/// An unevaluated constant expression: Zend's `zend_ast` for constant
/// expressions, evaluated by the engine on first use and kept as written
/// for Reflection. A closure or a first-class callable is an index into
/// the owning unit's function table.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstExpr {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    Str(Box<[u8]>),
    Constant(Name),
    ClassConstant {
        class: Name,
        name: Name,
    },
    EnumCase {
        class: Name,
        case: Name,
    },
    Unary {
        op: Name,
        operand: Box<ConstExpr>,
    },
    Binary {
        op: Name,
        lhs: Box<ConstExpr>,
        rhs: Box<ConstExpr>,
    },
    Array(Vec<(Option<ConstExpr>, ConstExpr)>),
    New {
        class: Name,
        arguments: Vec<ConstExpr>,
    },
    Closure(u32),
}

/// One argument of an attribute, positional or named.
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeArgument {
    pub name: Option<Name>,
    pub value: ConstExpr,
}

/// An attribute as written, its arguments unevaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: Name,
    pub arguments: Vec<AttributeArgument>,
}

/// One declared parameter slot.
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: Name,
    pub ty: Option<Type>,
    pub by_ref: bool,
    pub variadic: bool,
    pub default: Option<ConstExpr>,
    pub attributes: Vec<Attribute>,
}

/// A function's declaration: its signature, doc comment and site; the
/// body is the unit's op array of the same index.
///
/// `frameless` is a native's `zend_frameless_function_info` list
/// (`Z/Zend/zend_frameless_function.h`): the arities it serves without
/// a call frame, in the order php's `frameless_function_infos_*` table
/// lists them, `trim`'s being `[1, 2]`. php pairs each arity with a
/// handler of its own; here the handler is the native's one entry,
/// served over the arguments the opline carries, so the arity is the
/// whole row. Empty on a user function and on a native with no such
/// entry, as php's `frameless_function_infos` is null there. The
/// compiler emits `FRAMELESS_ICALL_<n>` for a call whose count an
/// arity here covers, and [`crate::frameless_table`] numbers the rows.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionDecl {
    pub name: Name,
    pub parameters: Vec<Parameter>,
    pub return_type: Option<Type>,
    pub returns_by_ref: bool,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
    pub file: Name,
    pub line: u32,
    pub frameless: Vec<u32>,
}

/// A method: a function with a visibility, whether it is an instance
/// member (`false` for a `static` method), and, on a native, the tentative
/// bit of its return type.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits of a method, each one fact
pub struct MethodDecl {
    pub function: FunctionDecl,
    pub visibility: Visibility,
    pub is_instance: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    pub tentative_return: bool,
}

/// A declared property, with its default still an expression.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDecl {
    pub name: Name,
    pub visibility: Visibility,
    pub is_instance: bool,
    pub is_readonly: bool,
    pub ty: Option<Type>,
    pub default: Option<ConstExpr>,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
}

/// A class constant, or an enum case when the class is an enum.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassConstDecl {
    pub name: Name,
    pub visibility: Visibility,
    pub is_final: bool,
    pub ty: Option<Type>,
    pub value: ConstExpr,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
}

/// A class, interface, trait or enum, unlinked: names, not identities.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub name: Name,
    pub kind: ClassKind,
    pub flags: ClassFlags,
    pub parent: Option<Name>,
    pub interfaces: Vec<Name>,
    pub traits: Vec<Name>,
    pub backing: EnumBacking,
    pub constants: Vec<ClassConstDecl>,
    pub properties: Vec<PropertyDecl>,
    pub methods: Vec<MethodDecl>,
    pub attributes: Vec<Attribute>,
    pub doc_comment: Option<Box<[u8]>>,
    pub file: Name,
    pub line: u32,
}

/// A global constant, `const X = ...;` or `define()` folded at compile time.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstDecl {
    pub name: Name,
    pub value: ConstExpr,
}

/// An ini entry an extension declares: its default and where it may be
/// changed (`PHP_INI_USER`, `PHP_INI_PERDIR`, `PHP_INI_SYSTEM`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniDecl {
    pub name: Name,
    pub default: Box<[u8]>,
    pub user: bool,
    pub per_dir: bool,
    pub system: bool,
}
