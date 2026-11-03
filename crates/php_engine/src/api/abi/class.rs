//! What a `#[php_class]` header and its member impl implement (design
//! §4.4, §7.1), and the values a property default and a constant are
//! declared by: php declares both by value, so they are Rust values here,
//! typed through [`PropertyValue`] and [`IntoConst`].

use std::any::Any;

use php_unit::decl::{ConstExpr, Type};

use crate::api::__private::{NativeClass, NativeClassConst, NativeMethod, NativeProperty};
use crate::api::abi::marshal::{FromArgument, Instance, Untyped};
use crate::api::values::{ClassId, PhpArray, PhpString, Value};

/// A named class: what `Instance<C>`, `This<C>`, `extends` and
/// `implements` read. A `#[php_class]` header implements it, and so does
/// `#[php_class(extern)]` for a class another extension or Core declares.
pub trait Class {
    /// php's spelling, `Random\Engine`.
    const NAME: &'static [u8];
    /// The payload a `This<Self>` reaches: the class's own, a type
    /// several classes share, the parent's, or `()`.
    type Payload: Any + Send;
    /// The payload `new` lays down for an object of `class`, php's
    /// `create_object`: the payload type's `Default`, or the header's
    /// `new = PATH` for a payload laid out per class.
    fn new_payload(class: ClassRef) -> Self::Payload;
}

/// The class an object is being laid down for, what `new = PATH` is
/// handed: the class `new` names, which may extend the one declaring the
/// payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassRef(pub ClassId);

/// A class header: its static entry, the members left to [`Members`],
/// and how its objects are made.
pub trait Header: Class {
    /// The header, none for a class another extension or Core declares,
    /// to which the member impl adds class constants and trampolines.
    const HEADER: Option<&'static NativeClass>;
}

/// The members a `#[php_class]` impl declares, in source order within
/// each kind. A class with none still writes an empty impl, so a
/// forgotten one is a build error at `.class::<C>()`.
pub trait Members {
    /// The class constants, each the static entry the attribute lays down.
    const CONSTANTS: &'static [NativeClassConst];
    /// The properties.
    const PROPERTIES: &'static [NativeProperty];
    /// The methods, php's method entry array.
    const METHODS: &'static [NativeMethod];
}

/// A `#[php_class]` enum's cases, each php name beside its Rust variant,
/// for the enum's `FromArgument` and `IntoReturn`.
pub trait Cases: Class + Sized + Copy + PartialEq + 'static {
    /// The cases in declaration order.
    const CASES: &'static [(&'static [u8], Self)];
}

/// The value a property default is written as, per property type (design
/// §7.1): rustc checks the default against it, so `string $x = 0` is a
/// type error.
pub trait PropertyValue {
    /// The Rust value a default of this type is written as.
    type Const;
    /// The default as the declaration keeps it.
    fn const_expr(value: Self::Const) -> ConstExpr;
}

impl PropertyValue for i64 {
    type Const = i64;
    fn const_expr(value: i64) -> ConstExpr {
        ConstExpr::long(value)
    }
}
impl PropertyValue for f64 {
    type Const = f64;
    fn const_expr(value: f64) -> ConstExpr {
        ConstExpr::Double(value)
    }
}
impl PropertyValue for bool {
    type Const = bool;
    fn const_expr(value: bool) -> ConstExpr {
        ConstExpr::Bool(value)
    }
}
impl PropertyValue for PhpString {
    type Const = &'static str;
    fn const_expr(value: &str) -> ConstExpr {
        ConstExpr::Str(value.as_bytes().into())
    }
}
impl PropertyValue for PhpArray {
    type Const = ConstArray;
    fn const_expr(value: ConstArray) -> ConstExpr {
        value.expr()
    }
}
impl<T: PropertyValue> PropertyValue for Option<T> {
    type Const = Option<T::Const>;
    fn const_expr(value: Option<T::Const>) -> ConstExpr {
        value.map_or(ConstExpr::Null, T::const_expr)
    }
}
/// A property typed as a class, whose one default php's stubs write is
/// `null`: `private ?Throwable $previous = null` is `Option<Instance<C>>`
/// with `None`, and no Rust value stands for an object default.
impl<C: Class> PropertyValue for Instance<C> {
    type Const = std::convert::Infallible;
    fn const_expr(value: std::convert::Infallible) -> ConstExpr {
        match value {}
    }
}
impl PropertyValue for Value {
    type Const = ConstValue;
    fn const_expr(value: ConstValue) -> ConstExpr {
        value.expr()
    }
}
impl<T> PropertyValue for Untyped<T> {
    type Const = ConstValue;
    fn const_expr(value: ConstValue) -> ConstExpr {
        value.expr()
    }
}

/// A plain php value written in Rust, for a `mixed` or untyped default
/// and a constant's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'static str),
}

impl ConstValue {
    fn expr(self) -> ConstExpr {
        match self {
            ConstValue::Null => ConstExpr::Null,
            ConstValue::Bool(b) => ConstExpr::Bool(b),
            ConstValue::Int(n) => ConstExpr::long(n),
            ConstValue::Float(f) => ConstExpr::Double(f),
            ConstValue::Str(s) => ConstExpr::Str(s.as_bytes().into()),
        }
    }
}

impl From<&'static str> for ConstValue {
    fn from(s: &'static str) -> ConstValue {
        ConstValue::Str(s)
    }
}
impl From<i64> for ConstValue {
    fn from(n: i64) -> ConstValue {
        ConstValue::Int(n)
    }
}
impl From<f64> for ConstValue {
    fn from(f: f64) -> ConstValue {
        ConstValue::Float(f)
    }
}
impl From<bool> for ConstValue {
    fn from(b: bool) -> ConstValue {
        ConstValue::Bool(b)
    }
}

/// An array written in Rust, each element with its key where it has one:
/// `ConstArray(&[])` for `[]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConstArray(pub &'static [(Option<ConstValue>, ConstValue)]);

impl ConstArray {
    fn expr(self) -> ConstExpr {
        ConstExpr::Array {
            elements: self
                .0
                .iter()
                .map(|(key, value)| (key.map(ConstValue::expr), value.expr()))
                .collect(),
            line: 0,
        }
    }
}

/// A class constant's or a global constant's type and value, by value,
/// as `zend_declare_typed_class_constant` declares one.
pub trait IntoConst {
    /// The declared type, `None` for none.
    fn declared() -> Option<Type>;
    /// The value.
    fn value(&self) -> ConstExpr;
}

macro_rules! into_const {
    ($t:ty, $ty:expr, |$v:ident| $make:expr) => {
        impl IntoConst for $t {
            fn declared() -> Option<Type> {
                Some($ty)
            }
            fn value(&self) -> ConstExpr {
                let $v = self;
                $make
            }
        }
    };
}
into_const!(i64, Type::Int, |n| ConstExpr::long(*n));
into_const!(i32, Type::Int, |n| ConstExpr::long(i64::from(*n)));
into_const!(f64, Type::Float, |f| ConstExpr::Double(*f));
into_const!(bool, Type::Bool, |b| ConstExpr::Bool(*b));
into_const!(&str, Type::String, |s| ConstExpr::Str(s.as_bytes().into()));
into_const!(String, Type::String, |s| ConstExpr::Str(
    s.as_bytes().into()
));
into_const!(Vec<u8>, Type::String, |bytes| ConstExpr::Str(
    bytes.as_slice().into()
));
into_const!(ConstArray, Type::Array, |array| array.expr());

/// A plain value with no declared type, for a constant php declares by
/// value alone, `NULL` among them.
impl IntoConst for ConstValue {
    fn declared() -> Option<Type> {
        None
    }
    fn value(&self) -> ConstExpr {
        self.expr()
    }
}

/// A constant whose type no plain Rust type gives (`int|array`, `mixed`,
/// `null`): `T` says the type and `V` holds the value, `const X:
/// Typed<Union<i64, PhpArray>, i64> = Typed::new(5);`.
#[derive(Debug, Clone, Copy)]
pub struct Typed<T, V> {
    value: V,
    ty: std::marker::PhantomData<fn() -> T>,
}

impl<T, V> Typed<T, V> {
    /// The value, declared as `T`.
    pub const fn new(value: V) -> Typed<T, V> {
        Typed {
            value,
            ty: std::marker::PhantomData,
        }
    }
}

impl<T: FromArgument, V: IntoConst> IntoConst for Typed<T, V> {
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn value(&self) -> ConstExpr {
        self.value.value()
    }
}
