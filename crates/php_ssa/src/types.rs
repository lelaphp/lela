//! What inference knows of a value: `zend_ssa_var_info`'s type mask, the
//! `MAY_BE_*` bits of `Z/Zend/zend_type_info.h` and
//! `Z/Zend/Optimizer/zend_inference.h`, the class of an object, and the
//! range of an integer (`zend_ssa_range`), with the reading of a constant
//! and of a declared type that `zend_inference.c` makes.

use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};

use php_heap::{Key, Zval, ZvalType};
use php_macros::EngineState;
use php_unit::decl::Type;
use php_unit::may_be;

use crate::LinkedTable;

/// A `MAY_BE_*` mask: the types a value may have, and for an array its
/// keys and the types of its elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, EngineState)]
pub struct Types(pub u32);

impl Types {
    pub const NONE: Types = Types(0);
    pub const UNDEF: Types = Types(may_be::UNDEF);
    pub const NULL: Types = Types(may_be::NULL);
    pub const FALSE: Types = Types(may_be::FALSE);
    pub const TRUE: Types = Types(may_be::TRUE);
    pub const BOOL: Types = Types(may_be::FALSE | may_be::TRUE);
    pub const LONG: Types = Types(may_be::LONG);
    pub const DOUBLE: Types = Types(may_be::DOUBLE);
    pub const STRING: Types = Types(may_be::STRING);
    pub const ARRAY: Types = Types(may_be::ARRAY);
    pub const OBJECT: Types = Types(may_be::OBJECT);
    pub const RESOURCE: Types = Types(may_be::RESOURCE);
    pub const ANY: Types = Types(may_be::ANY);
    pub const REF: Types = Types(may_be::REF);
    pub const OF_NULL: Types = Types(may_be::NULL << may_be::ARRAY_SHIFT);
    pub const OF_FALSE: Types = Types(may_be::FALSE << may_be::ARRAY_SHIFT);
    pub const OF_TRUE: Types = Types(may_be::TRUE << may_be::ARRAY_SHIFT);
    pub const OF_LONG: Types = Types(may_be::LONG << may_be::ARRAY_SHIFT);
    pub const OF_DOUBLE: Types = Types(may_be::DOUBLE << may_be::ARRAY_SHIFT);
    pub const OF_STRING: Types = Types(may_be::STRING << may_be::ARRAY_SHIFT);
    pub const OF_ARRAY: Types = Types(may_be::ARRAY << may_be::ARRAY_SHIFT);
    pub const OF_ANY: Types = Types(may_be::OF_ANY);
    pub const OF_REF: Types = Types(may_be::OF_REF);
    pub const PACKED: Types = Types(may_be::PACKED);
    pub const NUMERIC_HASH: Types = Types(may_be::NUMERIC_HASH);
    pub const STRING_HASH: Types = Types(may_be::STRING_HASH);
    pub const EMPTY: Types = Types(may_be::EMPTY);
    /// `MAY_BE_ARRAY_KEY_LONG`.
    pub const KEY_LONG: Types = Types(may_be::PACKED | may_be::NUMERIC_HASH);
    /// `MAY_BE_ARRAY_KEY_STRING`.
    pub const KEY_STRING: Types = Types(may_be::STRING_HASH);
    pub const KEY_ANY: Types = Types(may_be::KEY_ANY);
    pub const CLASS: Types = Types(may_be::CLASS);
    pub const INDIRECT: Types = Types(may_be::INDIRECT);
    pub const GUARD: Types = Types(may_be::GUARD);
    pub const RC1: Types = Types(may_be::RC1);
    pub const RCN: Types = Types(may_be::RCN);
    /// Both reference counts.
    pub const RC: Types = Types(may_be::RC1 | may_be::RCN);
    /// An array of anything: every key, element and element reference.
    pub const ARRAY_ANY: Types = Types(may_be::KEY_ANY | may_be::OF_ANY | may_be::OF_REF);
    /// What php knows of a value it knows nothing of, but that it is no
    /// reference: `MAY_BE_ANY | MAY_BE_ARRAY_KEY_ANY | MAY_BE_ARRAY_OF_ANY
    /// | MAY_BE_ARRAY_OF_REF` with both counts.
    pub const UNKNOWN: Types = Types(
        may_be::ANY | may_be::KEY_ANY | may_be::OF_ANY | may_be::OF_REF | may_be::RC1 | may_be::RCN,
    );
    /// What a reference may hold, `UPDATE_SSA_TYPE`'s widening of one.
    pub const REF_ANY: Types = Types(Types::UNKNOWN.0 | may_be::REF);

    /// Whether any bit of `other` is set.
    #[must_use]
    pub const fn has(self, other: Types) -> bool {
        self.0 & other.0 != 0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The bits but those of `other`.
    #[must_use]
    pub const fn without(self, other: Types) -> Types {
        Types(self.0 & !other.0)
    }

    /// The types of its elements as a value's: php's
    /// `(t & MAY_BE_ARRAY_OF_ANY) >> MAY_BE_ARRAY_SHIFT`.
    #[must_use]
    pub const fn elements(self) -> Types {
        Types((self.0 & may_be::OF_ANY) >> may_be::ARRAY_SHIFT)
    }

    /// Its value types as an array's elements, `(t & MAY_BE_ANY) <<
    /// MAY_BE_ARRAY_SHIFT`.
    #[must_use]
    pub const fn as_elements(self) -> Types {
        Types((self.0 & may_be::ANY) << may_be::ARRAY_SHIFT)
    }

    /// `MAY_BE_HASH_ONLY`: an array with hash keys and never packed or
    /// empty.
    #[must_use]
    pub const fn hash_only(self) -> bool {
        self.has(Types(may_be::NUMERIC_HASH | may_be::STRING_HASH))
            && !self.has(Types(may_be::PACKED | may_be::EMPTY))
    }
}

impl BitOr for Types {
    type Output = Types;
    fn bitor(self, other: Types) -> Types {
        Types(self.0 | other.0)
    }
}

impl BitOrAssign for Types {
    fn bitor_assign(&mut self, other: Types) {
        self.0 |= other.0;
    }
}

impl BitAnd for Types {
    type Output = Types;
    fn bitand(self, other: Types) -> Types {
        Types(self.0 & other.0)
    }
}

impl BitAndAssign for Types {
    fn bitand_assign(&mut self, other: Types) {
        self.0 &= other.0;
    }
}

impl Not for Types {
    type Output = Types;
    fn not(self) -> Types {
        Types(!self.0)
    }
}

/// `1 << IS_*`: the bit of one value type, as php writes it from a
/// zval's type or an `extended_value`'s.
#[must_use]
pub(crate) fn bit(code: u32) -> Types {
    Types(1u32.checked_shl(code).unwrap_or(0))
}

/// `zend_ssa_range`: the integers a value lies between, `underflow` and
/// `overflow` where it may pass a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EngineState)]
#[allow(clippy::struct_field_names)]
pub struct Range {
    pub min: i64,
    pub max: i64,
    pub underflow: bool,
    pub overflow: bool,
}

impl Range {
    /// Every integer, with both bounds passable.
    pub const ANY: Range = Range {
        min: i64::MIN,
        max: i64::MAX,
        underflow: true,
        overflow: true,
    };

    /// Every integer, with neither bound passed: what an `int`
    /// parameter holds.
    pub const LONG: Range = Range {
        min: i64::MIN,
        max: i64::MAX,
        underflow: false,
        overflow: false,
    };
}

/// An object's class as inference knows it: the class, and whether the
/// value may be of a class below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub struct ClassOf<C> {
    pub class: C,
    pub instanceof: bool,
}

/// `zend_ssa_var_info`, as php's tier reads it: its types, its class where
/// one is known, and its range where it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub struct VarInfo<C> {
    pub types: Types,
    pub class: Option<ClassOf<C>>,
    pub range: Option<Range>,
}

impl<C> Default for VarInfo<C> {
    fn default() -> VarInfo<C> {
        VarInfo {
            types: Types::NONE,
            class: None,
            range: None,
        }
    }
}

/// `func_info->return_info`: what a function returns, its range printed
/// whether or not php holds it, as `zend_dump_op_array` prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub struct Returns<C> {
    pub types: Types,
    pub class: Option<ClassOf<C>>,
    pub range: Range,
    pub has_range: bool,
}

impl<C> Default for Returns<C> {
    fn default() -> Returns<C> {
        Returns {
            types: Types::NONE,
            class: None,
            range: Range::default(),
            has_range: false,
        }
    }
}

/// `IS_CONSTANT_AST`, past every zval type php's masks name.
const CONSTANT_AST: u32 = 11;

/// `_const_op_type`: the types of an op array's constant. A string or
/// an array there is interned or immutable, never counted, so a string
/// may be shared and an array is (`zend_array_type_info`).
#[must_use]
pub(crate) fn of_constant(value: &Zval) -> Types {
    match value.kind() {
        ZvalType::ConstExpr => Types::RC | Types::ANY | Types::KEY_ANY | Types::OF_ANY,
        ZvalType::Array => value.as_arr().map_or(Types::ARRAY | Types::RCN, |arr| {
            let mut t = Types::ARRAY | Types::RCN;
            if arr.is_empty() {
                t |= Types::EMPTY;
            } else if arr.is_packed() {
                t |= Types::PACKED;
                for (_, element) in &arr {
                    t |= element_bit(&element);
                }
            } else {
                for (key, element) in &arr {
                    t |= match key {
                        Key::Str(_) => Types::STRING_HASH,
                        Key::Long(_) => Types::NUMERIC_HASH,
                    };
                    t |= element_bit(&element);
                }
            }
            t
        }),
        ZvalType::String => Types::STRING | Types::RCN,
        kind => bit(kind as u32),
    }
}

/// `1 << (Z_TYPE_P(val) + MAY_BE_ARRAY_SHIFT)`, as php writes it even
/// for an element of a type past the masks.
fn element_bit(element: &Zval) -> Types {
    let code = match element.kind() {
        ZvalType::ConstExpr => CONSTANT_AST,
        kind => kind as u32,
    };
    bit(code + may_be::ARRAY_SHIFT)
}

/// `MAY_BE_CALLABLE`, `MAY_BE_VOID` and `MAY_BE_STATIC`: the declaration
/// bits past the value types.
const DECL_CALLABLE: u32 = 1 << 12;
const DECL_VOID: u32 = 1 << 14;
const DECL_STATIC: u32 = 1 << 15;

/// The one class a declared type names alone, `ZEND_TYPE_HAS_NAME`,
/// nullable or not; `iterable` names `Traversable`. None for a list of
/// names, `ZEND_TYPE_HAS_LIST`.
fn single_name(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Named(_) | Type::SelfClass | Type::ParentClass | Type::Iterable => Some(ty),
        Type::Nullable(inner) => single_name(inner),
        Type::Union(members) => {
            let mut names = members.iter().filter(|m| names_class(m));
            let first = names.next()?;
            if names.next().is_some() {
                return None;
            }
            single_name(first)
        }
        _ => None,
    }
}

/// Whether a declared type names a class, `ZEND_TYPE_IS_COMPLEX`.
fn names_class(ty: &Type) -> bool {
    match ty {
        Type::Named(_)
        | Type::SelfClass
        | Type::ParentClass
        | Type::Iterable
        | Type::Intersection(_) => true,
        Type::Nullable(inner) => names_class(inner),
        Type::Union(members) => members.iter().any(names_class),
        _ => false,
    }
}

/// `zend_convert_type`: what a value of declared type `ty` may be, and
/// the class it names alone; any value where none is declared. php looks
/// the class up with no op array (`zend_optimizer_get_class_entry(script,
/// NULL, lcname)`), which binds no program's class: only one of the
/// engine's own, and never `self` or `parent`, which the compiler wrote
/// as a program's class's name.
pub(crate) fn of_declared<T: LinkedTable>(
    ty: Option<&Type>,
    table: &T,
) -> (Types, Option<T::Class>) {
    let Some(ty) = ty else {
        return (Types::UNKNOWN, None);
    };
    let mask = ty.mask();
    let mut t = Types(mask) & Types::ANY;
    if mask & DECL_VOID != 0 {
        t |= Types::NULL;
    }
    if mask & DECL_CALLABLE != 0 {
        t |= Types::STRING | Types::OBJECT | Types::ARRAY | Types::ARRAY_ANY;
    }
    if mask & DECL_STATIC != 0 {
        t |= Types::OBJECT;
    }
    if t.has(Types::ARRAY) {
        t |= Types::ARRAY_ANY;
    }
    let mut class = None;
    if names_class(ty) {
        t |= Types::OBJECT;
        class = match single_name(ty) {
            Some(Type::Named(name)) => table.engine_class(&name.0.to_ascii_lowercase()),
            Some(Type::Iterable) => table.engine_class(b"traversable"),
            _ => None,
        };
    }
    if t.has(Types::STRING | Types::ARRAY | Types::OBJECT | Types::RESOURCE) {
        t |= Types::RC;
    }
    (t, class)
}

/// `get_ssa_alias_types`.
#[must_use]
pub(crate) fn of_alias(alias: crate::Alias) -> Types {
    match alias {
        crate::Alias::HttpResponseHeader => {
            Types::ARRAY | Types::KEY_LONG | Types::OF_STRING | Types::RC
        }
        _ => Types::UNDEF | Types::REF_ANY,
    }
}
