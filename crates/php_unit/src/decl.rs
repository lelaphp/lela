//! The declaration model (design §4.7): the unlinked description of a
//! class, function, constant or attribute as its producer wrote it, with
//! constant expressions still unevaluated. Produced by the compiler for
//! user code and by the native macros for natives; consumed by the linker,
//! Reflection, the dumpers and the stub cross-check, which read one model.
//! Data only: no method here, since the eighteen types are re-exported
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
    pub dynamic_properties: DynamicProperties,
}

/// What a class does with a write to a name it did not declare, php's
/// two `ZEND_ACC_*` bits read as the one question they answer
/// (`Z/Zend/zend_object_handlers.c:1288`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DynamicProperties {
    /// Neither bit, which every class carries unless it says
    /// otherwise: creating one is php's 8.2 deprecation.
    #[default]
    Deprecated,
    /// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES`, what
    /// `#[\AllowDynamicProperties]` sets and what `stdClass` is
    /// registered with: creating one raises nothing.
    Allowed,
    /// `ZEND_ACC_NO_DYNAMIC_PROPERTIES`, which an enum carries and
    /// `zend_register_closure_ce` sets on `Closure`: creating one is
    /// php's `Error`.
    Forbidden,
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

impl Type {
    /// `MAY_BE_ANY` (`Z/Zend/zend_type_info.h:35`): the bit of every
    /// `IS_*` a value can be, which is the mask a parameter with no
    /// declared type carries in a `RECV`'s `op2`.
    pub const MAY_BE_ANY: u32 = 0x3fe;

    /// `ZEND_TYPE_PURE_MASK` (`Z/Zend/zend_types.h:284`): the `1 <<
    /// IS_*` of every builtin this type accepts, which a `RECV`'s `op2`
    /// carries so the handler's fast path is one test against the
    /// argument's type. A class name contributes no bit, so an object
    /// argument takes the slow path and is checked against the name;
    /// `mixed` is every bit and `iterable` is `array` beside the bit of
    /// `Traversable`'s name.
    #[must_use]
    pub fn mask(&self) -> u32 {
        match self {
            Type::Mixed => Type::MAY_BE_ANY,
            Type::Void => 1 << 14,
            Type::Never => 1 << 17,
            Type::Null => 1 << 1,
            Type::Bool => (1 << 2) | (1 << 3),
            Type::False => 1 << 2,
            Type::True => 1 << 3,
            Type::Int => 1 << 4,
            Type::Float => 1 << 5,
            Type::String => 1 << 6,
            // `iterable` is `array` beside `Traversable`, whose bit is
            // the name's and not the mask's.
            Type::Array | Type::Iterable => 1 << 7,
            Type::Object => 1 << 8,
            Type::Callable => 1 << 12,
            Type::CalledClass => 1 << 15,
            // A class name, and an intersection, which is names alone,
            // contribute no bit: the check reads the name.
            Type::SelfClass | Type::ParentClass | Type::Named(_) | Type::Intersection(_) => 0,
            Type::Nullable(inner) => inner.mask() | (1 << 1),
            Type::Union(members) => members.iter().fold(0, |mask, ty| mask | ty.mask()),
        }
    }

    /// Whether a value of this `IS_*` code satisfies the type without a
    /// coercion: `ZEND_TYPE_CONTAINS_CODE` over the pure mask, which
    /// `zend_check_type` tries before anything else.
    #[must_use]
    pub fn contains_code(&self, code: u8) -> bool {
        self.mask() & (1u32 << u32::from(code)) != 0
    }
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
/// `line` and `end_line` are `op_array.line_start` and `line_end`, the
/// lines of the `function` keyword and of the closing brace, and zero
/// on a native, whose `file` is empty. Reflection's `_function_string`
/// prints the pair as the `@@ file start - end` row it writes for a
/// user function alone.
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
    pub end_line: u32,
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
/// `line` and `end_line` are `ce->info.user.line_start` and `line_end`,
/// the lines of the keyword and of the closing brace, and zero on a
/// native, whose `file` is empty.
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
    pub end_line: u32,
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
