//! The marshalling vocabulary (design §4.4, §7.1): [`FromArgument`] for a
//! parameter and [`IntoReturn`] for a return, and the types a native's
//! signature is written in. Every fact a Rust type says about a parameter
//! or a return is an item of one of the two traits, so the macro matches
//! no type by name and the vocabulary widens by an `impl`.
//!
//! The entry the macro emits is php's ZPP: the call read once, its
//! arity held and its arguments taken as they stand on it, then each
//! argument left to right through [`FromArgument::hold`], an absent one
//! through [`FromArgument::from_default`], then the body, then
//! [`IntoReturn::into_return`]. A hold whose argument already has a type
//! the declaration's pure mask admits takes it as it is, as
//! `ZEND_TYPE_CONTAINS_CODE` does before anything else; any other is
//! [`coerce::argument`] over the declaration, whose text is php's.

use std::any::{Any, TypeId};
use std::marker::PhantomData;

use php_heap::{Bytes, Slot, ZvalSpan};

use php_unit::decl::{Bare, ConstExpr, Name, Type};

use crate::api::abi::Callable;
use crate::api::abi::class::Class;
use crate::api::call::Function;
use crate::api::coerce;
use crate::api::raising::{Thrown, arg_type_error, arg_value_error, fatal};
use crate::api::request::Request;
use crate::api::values::{
    Owner, PhpArray, PhpObject, PhpRef, PhpString, Resource, TypeTag, Value, ValueRef,
};

/// `MAY_BE_NULL`, `MAY_BE_FALSE` and the rest of `zend_type_info.h`'s
/// bits the const assertions read.
mod bits {
    pub(super) const NULL: u32 = 1 << 1;
    pub(super) const FALSE: u32 = 1 << 2;
    pub(super) const TRUE: u32 = 1 << 3;
    pub(super) const LONG: u32 = 1 << 4;
    pub(super) const DOUBLE: u32 = 1 << 5;
    pub(super) const STRING: u32 = 1 << 6;
    pub(super) const ARRAY: u32 = 1 << 7;
    pub(super) const OBJECT: u32 = 1 << 8;
    pub(super) const RESOURCE: u32 = 1 << 9;
    pub(super) const CALLABLE: u32 = 1 << 12;
    pub(super) const VOID: u32 = 1 << 14;
    pub(super) const STATIC: u32 = 1 << 15;
    pub(super) const NEVER: u32 = 1 << 17;
    pub(super) const ANY: u32 = 0x3fe;
}

/// The two members of a union, held to what php's compiler refuses in a
/// declared type: a member twice (`bool|false`, `int|int`), `mixed`
/// beside anything, and `void` or `never` inside one.
const fn union_mask(a: u32, b: u32) -> u32 {
    assert!(
        a & b == 0,
        "a union names one type twice, which php refuses"
    );
    assert!(
        a & bits::ANY != bits::ANY && b & bits::ANY != bits::ANY,
        "mixed can only be used as a standalone type"
    );
    assert!(
        (a | b) & (bits::VOID | bits::NEVER) == 0,
        "void and never can only be used as standalone types"
    );
    a | b
}

/// `?T`, held to what php refuses: `?mixed` and a nullable `null`.
const fn nullable_mask(inner: u32) -> u32 {
    assert!(
        inner & bits::NULL == 0,
        "?mixed and ?null are types php refuses"
    );
    assert!(
        inner & (bits::VOID | bits::NEVER) == 0,
        "void and never cannot be nullable"
    );
    inner | bits::NULL
}

/// The argument as the call gave it, none where it gave none.
#[inline]
fn given(given: Value) -> Option<Value> {
    (!given.to_zval().is_undef()).then_some(given)
}

/// Whether `mask` admits `value` as it is, `ZEND_TYPE_CONTAINS_CODE`.
#[inline]
fn admits(mask: u32, value: &Value) -> bool {
    mask & (1u32 << u32::from(value.type_code())) != 0
}

/// `value` held to what `T` declares, the text of a refusal naming
/// `position`: what a by-reference referent and a body's own `Raw` hold
/// are held to.
fn held_as<T: FromArgument>(req: &mut Request, value: Value, position: u32) -> Result<T, Thrown> {
    let value = value.deref();
    if admits(T::MASK, &value) {
        return T::from_value(req, value);
    }
    // The type the declaration carries, `T::declared()`, which the
    // native's arginfo is built from: not a type spelled again.
    let held = match T::declared() {
        Some(ty) => coerce::coerce_param(req, &value, &ty, position)?,
        None => value,
    };
    T::from_value(req, held)
}

/// [`FromArgument::hold`] past the argument its declaration admits as it
/// is: the coercion, out of line, so the hold a native's entry makes of
/// an argument of its type is a test of the type byte.
#[inline(never)]
fn coerced<T: FromArgument>(req: &mut Request, given: Value, position: u32) -> Result<T, Thrown> {
    let held = coerce::argument(req, given, position)?;
    T::from_value(req, held)
}

fn mismatch<T>(req: &mut Request, what: &str) -> Result<T, Thrown> {
    Err(fatal(
        req,
        &format!("InternalError: a held argument is not the {what} its declaration names"),
    ))
}

/// A type a native parameter may have (design §7.1). The declaration,
/// whether it is passed by reference, the hold and the absent path are
/// all the type's own.
pub trait FromArgument: Sized {
    /// `ZEND_SEND_BY_REF`.
    const BY_REF: bool = false;
    /// The trailing `...$name`.
    const VARIADIC: bool = false;
    /// [`Passed`]: optional, and absence is the body's to see.
    const PASSED: bool = false;
    /// `ZEND_TYPE_PURE_MASK` of the declared type, what the hold takes a
    /// value as it is by and what the const assertions of a union and a
    /// nullable read; zero for a class and for no type.
    const MASK: u32;
    /// The declared type; `None` declares no type.
    fn declared() -> Option<Type>;
    /// Whether a held value is this type, which picks a union's arm.
    fn accepts(value: &Value) -> bool;
    /// The value the engine held to the declaration, as `Self`.
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown>;
    /// The argument the call passed at `position`, `given` as it stands
    /// on the call (`UNDEF` where the call gave none), held to the
    /// declaration: as it is where its type is one
    /// [`FromArgument::MASK`] admits, an int where it takes a float
    /// converted ([`coerce::widened`]), and otherwise through
    /// [`coerce::argument`], with the 8.1 null deprecation and php's
    /// `TypeError`; `None` where the call gave none.
    #[inline(always)] // a type byte's test in every native's entry; the coercion is out of line
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let Some(given) = self::given(given) else {
            return Ok(None);
        };
        let plain = given.deref();
        if admits(Self::MASK, &plain) {
            return Self::from_value(req, plain).map(Some);
        }
        if let Some(widened) = coerce::widened(Self::MASK, &plain) {
            return Self::from_value(req, widened).map(Some);
        }
        coerced(req, given, position).map(Some)
    }
    /// A variadic tail held off a frameless call's operands, `given`
    /// those from the tail's `position` on, each held as [`Self::hold`]
    /// holds an argument, what the entry `#[php_function]` generates for
    /// a frameless arity reads in place of the call's count (design
    /// §7.1); an internal error for a type that is no tail.
    fn tail(req: &mut Request, given: &[Value], position: u32) -> Result<Self, Thrown> {
        let _ = (given, position);
        mismatch(req, "variadic tail")
    }
    /// The absent argument: the declaration's default, `absent` as the
    /// entry lays it down, evaluated in `scope`, the declaring class, and
    /// held to the declared type ([`Absent`]).
    fn from_default(
        req: &mut Request,
        absent: Absent,
        scope: Option<&[u8]>,
        position: u32,
    ) -> Result<Self, Thrown> {
        let value = default_value(req, absent, scope, position, (Self::MASK, Self::declared))?;
        Self::from_value(req, value)
    }
}

/// A parameter's default as the entry lays it down where the call gave
/// no argument, the declaration's expression in the form that costs the
/// call least: a scalar as written, a string as its bytes, as php's
/// native leaves its C default standing under `Z_PARAM_OPTIONAL`; and
/// anything else as the function that builds its expression, called
/// on the request's first use alone, the value it evaluates to kept by
/// the request after, as `ZEND_RECV_INIT` caches an evaluated default in
/// its `run_time_cache` slot and `zend_get_default_from_internal_arg_info`
/// evaluates a native's (design §4.7).
#[derive(Debug, Clone, Copy)]
pub enum Absent {
    /// `null`, a `bool`, an `int` or a `float` written as itself, php's
    /// C default: laid down as the value, nothing evaluated.
    Value(Value),
    /// A string written as itself, the engine's string made of it at
    /// startup.
    Str(&'static [u8]),
    /// Any other constant expression, built where the request evaluates
    /// it first and kept by the request after.
    Expr(fn() -> ConstExpr),
}

/// The default `from_default` takes: a scalar as written, a string the
/// stub writes as the string the engine made of it at startup, anything
/// else from the request's store, evaluated there on its first use. So
/// an omitted `string $version = "1.0"` costs the request nothing, as
/// php's C default does:
///
/// ```php
/// $m = memory_get_usage(); $d = new DOMDocument; unset($d); var_dump(memory_get_usage() - $m);
/// $m = memory_get_usage(); $x = Dom\XMLDocument::createEmpty(); unset($x); var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// int(0)
/// int(0)
/// ```
fn default_value(
    req: &mut Request,
    absent: Absent,
    scope: Option<&[u8]>,
    position: u32,
    (mask, declared): (u32, fn() -> Option<Type>),
) -> Result<Value, Thrown> {
    let value = match absent {
        Absent::Value(value) => value,
        // php's C default for an omitted string costs the request
        // nothing, and neither does the engine's string made at startup.
        Absent::Str(written) => {
            let Some(made) = req.shared.default_string(written) else {
                let expr = || ConstExpr::Str(written.into());
                return evaluated(req, expr, scope, position, declared);
            };
            Value::string(crate::api::values::PhpString::of(made))
        }
        Absent::Expr(built) => return evaluated(req, built, scope, position, declared),
    };
    if admits(mask, &value) {
        return Ok(value);
    }
    held_default(req, value, position, declared)
}

/// A default that is no value as written, its expression built and
/// evaluated in `scope` on the request's first use and held to the declared type, and the request's
/// kept value after.
fn evaluated(
    req: &mut Request,
    expr: impl FnOnce() -> ConstExpr,
    scope: Option<&[u8]>,
    position: u32,
    declared: fn() -> Option<Type>,
) -> Result<Value, Thrown> {
    let key = (
        req.active_native_function().unwrap_or(Function::PASS),
        position,
    );
    if let Some(kept) = req.default_argument(key) {
        return Ok(kept);
    }
    let expr = expr();
    let scope = scope.and_then(|name| req.class_named(Bare::resolved(name)));
    let value = crate::api::call::eval_const_expr_made(req, &expr, scope)?;
    let held = held_default(req, value, position, declared)?;
    req.keep_default_argument(key, held);
    Ok(held)
}

/// A default held to the declared type, as an argument is.
fn held_default(
    req: &mut Request,
    value: Value,
    position: u32,
    declared: fn() -> Option<Type>,
) -> Result<Value, Thrown> {
    match declared() {
        Some(ty) if !coerce::holds(req, &value, &ty, coerce::Scopes::default()) => {
            let held = coerce::coerce_param(req, &value, &ty, position)?;
            req.release(value);
            Ok(held)
        }
        _ => Ok(value),
    }
}

/// `mixed`: the value as passed.
impl FromArgument for Value {
    const MASK: u32 = bits::ANY;
    fn declared() -> Option<Type> {
        Some(Type::Mixed)
    }
    fn accepts(_: &Value) -> bool {
        true
    }
    fn from_value(_: &mut Request, value: Value) -> Result<Self, Thrown> {
        Ok(value.deref())
    }
}

macro_rules! scalar {
    ($t:ty, $ty:expr, $mask:expr, $read:ident, $what:literal) => {
        impl FromArgument for $t {
            const MASK: u32 = $mask;
            fn declared() -> Option<Type> {
                Some($ty)
            }
            fn accepts(value: &Value) -> bool {
                value.$read().is_some()
            }
            #[inline]
            fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
                match value.$read() {
                    Some(held) => Ok(held),
                    None => mismatch(req, $what),
                }
            }
        }
    };
}
scalar!(bool, Type::Bool, bits::FALSE | bits::TRUE, as_bool, "bool");
scalar!(i64, Type::Int, bits::LONG, as_int, "int");
scalar!(f64, Type::Float, bits::DOUBLE, as_float, "float");
scalar!(
    PhpString,
    Type::String,
    bits::STRING,
    as_php_string,
    "string"
);
scalar!(PhpArray, Type::Array, bits::ARRAY, as_array, "array");
scalar!(PhpObject, Type::Object, bits::OBJECT, as_object, "object");

/// No declared type, the resource `Z_PARAM_RESOURCE` holds: "must be of
/// type resource, X given".
impl FromArgument for Resource {
    const MASK: u32 = bits::RESOURCE;
    fn declared() -> Option<Type> {
        None
    }
    fn accepts(value: &Value) -> bool {
        matches!(value.as_ref(), ValueRef::Resource(_))
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        match value.as_ref() {
            ValueRef::Resource(held) => Ok(held),
            _ => mismatch(req, "resource"),
        }
    }
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let Some(given) = self::given(given) else {
            return Ok(None);
        };
        let given = given.deref();
        if let ValueRef::Resource(held) = given.as_ref() {
            return Ok(Some(held));
        }
        let name = coerce::value_name(req, &given);
        Err(arg_type_error(
            req,
            position,
            &bytes!("be of type resource, ", name, " given"),
        ))
    }
}

/// `string` as `Z_PARAM_PATH` holds it: a string with no NUL byte in it,
/// one there being php's "must not contain any null bytes".
#[derive(Debug, Clone, Copy)]
pub struct Path(pub PhpString);

impl FromArgument for Path {
    const MASK: u32 = bits::STRING;
    fn declared() -> Option<Type> {
        Some(Type::String)
    }
    fn accepts(value: &Value) -> bool {
        PhpString::accepts(value)
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        PhpString::from_value(req, value).map(Path)
    }
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let Some(string) = PhpString::hold(req, given, position)? else {
            return Ok(None);
        };
        if string.as_bytes().contains(&0) {
            return Err(arg_value_error(req, position, "not contain any null bytes"));
        }
        Ok(Some(Path(string)))
    }
}

/// `callable`, held as `Z_PARAM_FUNC` holds it.
impl FromArgument for Callable {
    const MASK: u32 = bits::CALLABLE;
    fn declared() -> Option<Type> {
        Some(Type::Callable)
    }
    fn accepts(_: &Value) -> bool {
        true
    }
    fn from_value(req: &mut Request, _: Value) -> Result<Self, Thrown> {
        mismatch(req, "callable held off its own slot")
    }
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        coerce::callable_argument(req, given, position)
    }
}

/// `inner` with `null` beside it: `T|null` over a union, `?T` else.
fn or_null(inner: Type) -> Type {
    match inner {
        Type::Union(mut members) => {
            members.push(Type::Null);
            Type::Union(members)
        }
        other => Type::Nullable(Box::new(other)),
    }
}

/// `?T`, or `T|null` over a union.
impl<T: FromArgument> FromArgument for Option<T> {
    const BY_REF: bool = T::BY_REF;
    const MASK: u32 = nullable_mask(T::MASK);
    fn declared() -> Option<Type> {
        T::declared().map(or_null)
    }
    fn accepts(value: &Value) -> bool {
        matches!(value.as_ref(), ValueRef::Null) || T::accepts(value)
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        match value.as_ref() {
            ValueRef::Null => Ok(None),
            _ => T::from_value(req, value).map(Some),
        }
    }
    #[inline(always)] // as `FromArgument::hold`
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let Some(given) = self::given(given) else {
            return Ok(None);
        };
        let plain = given.deref();
        if plain.tag() == TypeTag::Null {
            return Ok(Some(None));
        }
        // `Z_PARAM_RESOURCE_OR_NULL`: an untyped resource's refusal
        // names the `null` it also takes.
        if T::MASK == bits::RESOURCE && T::declared().is_none() && plain.as_resource().is_none() {
            return Err(resource_or_null(req, &plain, position));
        }
        T::hold(req, given, position).map(|held| held.map(Some))
    }
}

/// `Z_PARAM_RESOURCE_OR_NULL`'s refusal of what is neither.
#[cold]
fn resource_or_null(req: &mut Request, given: &Value, position: u32) -> Thrown {
    let name = coerce::value_name(req, given);
    arg_type_error(
        req,
        position,
        &bytes!("be of type resource or null, ", name, " given"),
    )
}

/// No declared type, held as `T`: php's untyped parameters whose ZPP
/// still checks (`@param resource`, `DOMNode|string`) and untyped
/// returns.
#[derive(Debug, Clone, Copy)]
pub struct Untyped<T>(pub T);

impl<T: FromArgument> FromArgument for Untyped<T> {
    const BY_REF: bool = T::BY_REF;
    const MASK: u32 = 0;
    fn declared() -> Option<Type> {
        None
    }
    fn accepts(value: &Value) -> bool {
        T::accepts(value)
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        T::from_value(req, value).map(Untyped)
    }
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        match self::given(given) {
            Some(given) => held_as::<T>(req, given, position).map(|held| Some(Untyped(held))),
            None => Ok(None),
        }
    }
}

/// A value of the class `C` names, `DateTimeInterface $object`: held by
/// `instanceof`, and returned as the object.
pub struct Instance<C: Class> {
    object: PhpObject,
    class: PhantomData<C>,
}

impl<C: Class> Clone for Instance<C> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<C: Class> Copy for Instance<C> {}

impl<C: Class> Instance<C> {
    /// An object the body vouches is a `C`, to return.
    #[must_use]
    pub fn of(object: PhpObject) -> Instance<C> {
        Instance {
            object,
            class: PhantomData,
        }
    }

    /// The object.
    #[must_use]
    pub fn object(&self) -> PhpObject {
        self.object
    }

    /// The object's payload, `C::Payload`, or the one `InternalError`
    /// ([`PhpObject::native_mut_or_fatal`]).
    pub fn payload_mut<'a>(&self, req: &'a mut Request) -> Result<&'a mut C::Payload, Thrown> {
        self.object.native_mut_or_fatal(req)
    }
}

fn named<C: Class>() -> Type {
    Type::Named(Name(Bytes::borrowed(C::NAME)))
}

impl<C: Class> FromArgument for Instance<C> {
    const MASK: u32 = 0;
    fn declared() -> Option<Type> {
        Some(named::<C>())
    }
    fn accepts(value: &Value) -> bool {
        matches!(value.as_ref(), ValueRef::Object(_))
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        match value.as_ref() {
            ValueRef::Object(object) => Ok(Instance::of(object)),
            _ => mismatch(req, "object"),
        }
    }
}

/// The receiver, `$this`; its presence makes a method an instance
/// method. It checks the payload, not `instanceof C`: an entry reached
/// through `implementation =` serves another class's objects.
pub struct This<C: Class> {
    object: PhpObject,
    class: PhantomData<C>,
}

impl<C: Class> Clone for This<C> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<C: Class> Copy for This<C> {}

impl<C: Class> This<C> {
    /// The receiver of the method being served, what the entry reads
    /// before its arguments.
    pub fn read(req: &mut Request) -> Result<This<C>, Thrown> {
        match req.frames().this() {
            Some(object) => Ok(This {
                object,
                class: PhantomData,
            }),
            None => Err(fatal(
                req,
                "InternalError: a method served without its receiver",
            )),
        }
    }

    /// The object.
    #[must_use]
    pub fn object(&self) -> PhpObject {
        self.object
    }

    /// The receiver's payload, `C::Payload`, or the one `InternalError`
    /// ([`PhpObject::native_mut_or_fatal`]).
    pub fn payload_mut<'a>(&self, req: &'a mut Request) -> Result<&'a mut C::Payload, Thrown> {
        self.object.native_mut_or_fatal(req)
    }
}

/// `A|B`, nested for a wider union; the declaration flattens
/// `Union<A, Union<B, C>>` to `A|B|C`. Class members are written in
/// php's order, since `zend_type_to_string` keeps it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Union<A, B> {
    A(A),
    B(B),
}

fn flatten(parts: [Option<Type>; 2]) -> Type {
    let mut members = Vec::new();
    for member in parts.into_iter().flatten() {
        match member {
            Type::Union(inner) => members.extend(inner),
            other => members.push(other),
        }
    }
    Type::Union(members)
}

impl<A: FromArgument, B: FromArgument> FromArgument for Union<A, B> {
    const MASK: u32 = union_mask(A::MASK, B::MASK);
    fn declared() -> Option<Type> {
        Some(flatten([A::declared(), B::declared()]))
    }
    fn accepts(value: &Value) -> bool {
        A::accepts(value) || B::accepts(value)
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        if A::accepts(&value) {
            A::from_value(req, value).map(Union::A)
        } else {
            B::from_value(req, value).map(Union::B)
        }
    }
}

/// `iterable`: an array, or an object whose class is `Traversable`,
/// declared as php 8.2 compiles `iterable`, `Traversable|array`.
#[derive(Debug, Clone, Copy)]
pub enum Iterable {
    Array(PhpArray),
    Traversable(PhpObject),
}

impl FromArgument for Iterable {
    const MASK: u32 = bits::ARRAY;
    fn declared() -> Option<Type> {
        Some(Type::Union(vec![
            Type::Named(Name(Bytes::borrowed(b"Traversable"))),
            Type::Array,
        ]))
    }
    fn accepts(value: &Value) -> bool {
        matches!(value.as_ref(), ValueRef::Array(_) | ValueRef::Object(_))
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        match value.as_ref() {
            ValueRef::Array(array) => Ok(Iterable::Array(array)),
            ValueRef::Object(object) => Ok(Iterable::Traversable(object)),
            _ => mismatch(req, "iterable"),
        }
    }
}

/// `Passed<T>`: `= UNKNOWN` without a default, or presence the body
/// observes beside a shown one. Absent is `Omitted`, which is only ever a
/// trailing omission: a skipped `Passed` with no default is php's "must
/// be passed explicitly".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Passed<T> {
    Given(T),
    Omitted,
}

impl<T: FromArgument> FromArgument for Passed<T> {
    const PASSED: bool = true;
    const BY_REF: bool = T::BY_REF;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn accepts(value: &Value) -> bool {
        T::accepts(value)
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        T::from_value(req, value).map(Passed::Given)
    }
    #[inline(always)] // as `FromArgument::hold`
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        Ok(Some(match T::hold(req, given, position)? {
            Some(held) => Passed::Given(held),
            None => Passed::Omitted,
        }))
    }
    fn from_default(_: &mut Request, _: Absent, _: Option<&[u8]>, _: u32) -> Result<Self, Thrown> {
        Ok(Passed::Omitted)
    }
}

/// `Raw<T>`: declares `T` and holds nothing, for php's
/// `Z_PARAM_ZVAL`-then-convert natives, whose body raises before the
/// coercion; the body holds the argument with [`Raw::hold`] when it
/// chooses. Its mask admits every value, so a referent under
/// [`Ref`] and a default are taken as they are too: `Ref<Raw<T>>` is a
/// by-reference parameter declared `T` whose refusal is the body's, as
/// `end()`'s `array|object &$array` refuses a string with "must be of
/// type array".
pub struct Raw<T> {
    value: Value,
    position: u32,
    held: PhantomData<fn() -> T>,
}

impl<T> Clone for Raw<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Raw<T> {}

impl<T> Raw<T> {
    /// `value` declared as `T`, passed at `position`; zero for a return.
    pub(crate) fn at(value: Value, position: u32) -> Raw<T> {
        Raw {
            value,
            position,
            held: PhantomData,
        }
    }
}

impl<T: FromArgument> Raw<T> {
    /// The argument as passed, a reference followed.
    #[must_use]
    pub fn value(&self) -> Value {
        self.value
    }

    /// The declared coercion, with the text the entry would raise: the
    /// argument held off its own slot as it stands now, as the entry
    /// holds one, so a type only a slot can serve (`callable`, which
    /// `zend_is_callable` resolves where the argument stands) holds here
    /// too, and a second hold reads the string a first one wrote back
    /// (`zend_parse_arg_str_weak`) rather than what it replaced; the
    /// value as passed where the slot holds none.
    pub fn hold(&self, req: &mut Request) -> Result<T, Thrown> {
        let given = req
            .frames()
            .argument(self.position)
            .unwrap_or(Value::undef());
        match T::hold(req, given, self.position)? {
            Some(held) => Ok(held),
            None => held_as(req, self.value, self.position),
        }
    }
}

impl<T: FromArgument> FromArgument for Raw<T> {
    const MASK: u32 = bits::ANY;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn accepts(_: &Value) -> bool {
        true
    }
    fn from_value(_: &mut Request, value: Value) -> Result<Self, Thrown> {
        Ok(Raw::at(value, 0))
    }
    fn hold(_: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        Ok(self::given(given).map(|given| Raw::at(given.deref(), position)))
    }
    /// The absent argument: the default evaluated and held to nothing, as
    /// a given one is, so an `arity = manual` body counts its arguments
    /// before any is coerced; [`Raw::hold`] holds it at its position.
    fn from_default(
        req: &mut Request,
        absent: Absent,
        scope: Option<&[u8]>,
        position: u32,
    ) -> Result<Self, Thrown> {
        let value = default_value(req, absent, scope, position, (bits::ANY, || None))?;
        Ok(Raw::at(value, position))
    }
}

/// `Ref<T>`: a parameter the caller passes by reference, declared as `T`
/// with `by_ref`, its referent held to `T` on entry as ZPP holds it, so
/// `sort($s)` with a string in `$s` is php's `TypeError` before the body
/// runs. Absent, [`Ref::set`] writes nothing, as `if (zv)
/// ZEND_TRY_ASSIGN_REF` does.
#[derive(Clone, Copy)]
pub struct Ref<T> {
    cell: Option<PhpRef>,
    held: Option<T>,
}

impl<T> Ref<T> {
    /// Whether the call gave the argument.
    #[must_use]
    pub fn is_given(&self) -> bool {
        self.cell.is_some()
    }

    /// The referent as held on entry.
    #[must_use]
    pub fn get(&self) -> Option<&T> {
        self.held.as_ref()
    }

    /// The caller's reference itself, for a body that reads the variable
    /// again after calling back into the program, as `array_walk` reloads
    /// the array its callback may have replaced ("Reload array and
    /// position -- both may have changed"); none where the argument is
    /// absent or came as a value.
    #[must_use]
    pub fn reference(&self) -> Option<PhpRef> {
        self.cell
    }

    /// Assign through the reference, `ZEND_TRY_ASSIGN_REF`; nothing where
    /// the argument is absent. The value is taken as a native's return
    /// is ([`IntoReturn`]): an [`Owner`](crate::api::values::Owner) is
    /// moved to the cell, and a view is one more owner, the cell's.
    pub fn set(&self, req: &mut Request, value: impl IntoReturn) -> Result<(), Thrown> {
        let value = value.into_return(req)?;
        if let Some(cell) = self.cell {
            return cell.set(req, value);
        }
        value.give_up(req);
        Ok(())
    }

    /// `Z_PARAM_ARRAY_EX(.., 0, 1)`: the caller's array separated in
    /// place ([`PhpRef::separate_array`]), so the body changes it with no
    /// copy; none where the argument is absent or its referent is no
    /// array. Over an untyped or union referent it is the array arm's
    /// `SEPARATE_ARRAY`, as `array_multisort`'s arguments and
    /// `array_walk`'s `array|object` take it.
    pub fn separated(&self, req: &mut Request) -> Result<Option<PhpArray>, Thrown> {
        match self.cell {
            Some(cell) => cell.separate_array(req),
            None => Ok(None),
        }
    }
}

impl<T: FromArgument> FromArgument for Ref<T> {
    const BY_REF: bool = true;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn accepts(_: &Value) -> bool {
        true
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        let cell = value.as_php_ref();
        let held = held_as::<T>(req, value, 0)?;
        Ok(Ref {
            cell,
            held: Some(held),
        })
    }
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let Some(given) = self::given(given) else {
            return Ok(None);
        };
        let cell = given.as_php_ref();
        let held = held_as::<T>(req, given, position)?;
        Ok(Some(Ref {
            cell,
            held: Some(held),
        }))
    }
    fn from_default(_: &mut Request, _: Absent, _: Option<&[u8]>, _: u32) -> Result<Self, Thrown> {
        Ok(Ref {
            cell: None,
            held: None,
        })
    }
}

/// The variadic tail of a native's parameters, each element held to
/// `T`, php's `Z_PARAM_VARIADIC` (`Z/Zend/zend_API.h:2169`): the `zval
/// *args` and the `argc` it hands the body, read from the front as an
/// iterator of its elements by value, which knows how many are left
/// ([`ExactSizeIterator`]) and reads any one of them without the ones
/// before it ([`Iterator::nth`]). A call that names an argument the
/// tail cannot take is php's "does not accept unknown named
/// parameters".
///
/// A tail of a type that takes each argument as it stands (`mixed`, an
/// untyped value, an `array`, an array the body holds itself) is read
/// where the call left its arguments: over the frame's slots
/// ([`ZvalSpan`]) on a framed call, over the operands on a frameless
/// one, each element read back off its slot as the body reaches it, so
/// it allocates nothing however long it is; an `array` tail is held to
/// its type on entry, each argument left to right, before the body
/// runs. A tail of a type that converts what it takes is held whole on
/// entry: none or one element in place, a longer one in one `Vec` sized
/// from the call's count.
///
/// The lifetime is the call's. The entry hands the body a tail of a
/// lifetime the body cannot name, so the body keeps no tail past its
/// return, and the slots a tail reads stand on their page until the
/// frame pops, which is after the body returns. A slot written while
/// the body runs, a weak coercion's string written back
/// ([`Raw::hold`]), is read as it stands, as php's `args[i]` is.
///
/// A tail passed by value and one passed by reference, one, two and
/// three elements long:
///
/// ```php
/// $a = []; array_push($a, 1); array_push($a, 2, 3); array_push($a, 4, 5, 6);
/// echo json_encode($a), " ", max(1, 7), max(1, 2, 8), max(1, 2, 3, 9), "\n";
/// var_dump(sscanf("1", "%d", $x), $x);
/// var_dump(sscanf("1 b", "%d %s", $x, $y), $x, $y);
/// var_dump(sscanf("1 b 2.5", "%d %s %f", $x, $y, $z), $x, $y, $z);
/// ```
/// ```expect
/// [1,2,3,4,5,6] 789
/// int(1)
/// int(1)
/// int(2)
/// int(1)
/// string(1) "b"
/// int(3)
/// int(1)
/// string(1) "b"
/// float(2.5)
/// ```
///
/// A tail read in place: long ones, one unpacked, one holding a
/// reference, which is read through, an `array` tail refused at the
/// argument that is none before the body runs, one read out of order by
/// position, and a frameless call's:
///
/// ```php
/// echo max(4, 9, 2, 8, 1, 7, 3, 6, 5), " ", min(...[5, 3, 4]), " ";
/// $x = 7; $r = [1, &$x]; echo max(...$r), "\n";
/// echo json_encode(array_merge([1], [2], [3], [4, 5])), "\n";
/// try { array_merge([1], [2], 3, [4]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// echo sprintf('%3$s-%1$s-%2$s', 'a', 'b', 'c'), " ", sprintf('%s', 'd'), "\n";
/// echo json_encode(array_diff_key(['a' => 1, 'b' => 2, 'c' => 3], ['a' => 0], ['c' => 0])), "\n";
/// ```
/// ```expect
/// 9 3 7
/// [1,2,3,4,5]
/// array_merge(): Argument #3 must be of type array, int given
/// c-a-b d
/// {"b":2}
/// ```
#[derive(Debug, Clone)]
pub struct Variadic<'c, T> {
    tail: Tail<T>,
    call: PhantomData<&'c ()>,
}

/// Where a [`Variadic`]'s elements stand: held, none or one in place
/// and more on the heap, or left where the call put them and read back
/// from `front` up to `back`.
#[derive(Debug, Clone)]
enum Tail<T> {
    One(std::option::IntoIter<T>),
    Many(std::vec::IntoIter<T>),
    InPlace {
        slots: Slots,
        position: u32,
        front: u32,
        back: u32,
    },
}

/// The arguments an in-place tail reads: the frame's slots, or a
/// frameless call's operands, of which there are at most three.
#[derive(Debug, Clone, Copy)]
enum Slots {
    Frame(ZvalSpan),
    Operands([Value; 3]),
}

impl Slots {
    fn at(self, index: u32) -> Value {
        match self {
            Slots::Frame(span) => span
                .read(index as usize)
                .map_or_else(Value::undef, Value::of_zval),
            Slots::Operands(given) => given
                .get(index as usize)
                .copied()
                .unwrap_or_else(Value::undef),
        }
    }
}

/// Whether a `T` is read back off the slot it stands in, which a type
/// that holds the argument as it is, with no conversion, is: and if so,
/// whether its hold may refuse what it is given, so the entry holds
/// each argument to it before the body runs. `TypeId` decides, as the
/// four types are named here and nowhere else.
fn in_place<T: 'static>() -> Option<bool> {
    let id = TypeId::of::<T>();
    if id == TypeId::of::<Value>()
        || id == TypeId::of::<Untyped<Value>>()
        || id == TypeId::of::<Raw<PhpArray>>()
    {
        return Some(false);
    }
    (id == TypeId::of::<PhpArray>()).then_some(true)
}

/// `slot`, the argument at `position`, read back as the `T`
/// [`in_place`] names: as [`FromArgument::hold`] holds it, a reference
/// followed; none for any other type.
fn read_back<T: 'static>(slot: Value, position: u32) -> Option<T> {
    fn cast<T: 'static, X: 'static>(held: X) -> Option<T> {
        let mut held = Some(held);
        (&mut held as &mut dyn Any)
            .downcast_mut::<Option<T>>()?
            .take()
    }
    let plain = slot.deref();
    let id = TypeId::of::<T>();
    if id == TypeId::of::<Value>() {
        cast(plain)
    } else if id == TypeId::of::<Untyped<Value>>() {
        cast(Untyped(plain))
    } else if id == TypeId::of::<Raw<PhpArray>>() {
        cast(Raw::<PhpArray>::at(plain, position))
    } else if id == TypeId::of::<PhpArray>() {
        cast(plain.as_array()?)
    } else {
        None
    }
}

impl<T> Variadic<'_, T> {
    fn of(tail: Tail<T>) -> Self {
        Variadic {
            tail,
            call: PhantomData,
        }
    }

    fn none() -> Self {
        Variadic::of(Tail::One(None.into_iter()))
    }
}

impl<T: 'static> Iterator for Variadic<'_, T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        match &mut self.tail {
            Tail::One(one) => one.next(),
            Tail::Many(many) => many.next(),
            Tail::InPlace {
                slots,
                position,
                front,
                back,
            } => {
                if front >= back {
                    return None;
                }
                let at = *front;
                *front += 1;
                read_back(slots.at(at), position.saturating_add(at))
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = match &self.tail {
            Tail::One(one) => one.len(),
            Tail::Many(many) => many.len(),
            Tail::InPlace { front, back, .. } => back.saturating_sub(*front) as usize,
        };
        (left, Some(left))
    }
    /// The element `n` past the front, the ones before it passed over
    /// unread where the tail stands in place.
    fn nth(&mut self, n: usize) -> Option<T> {
        if let Tail::InPlace { front, back, .. } = &mut self.tail {
            let skip = u32::try_from(n).unwrap_or(u32::MAX);
            *front = front.saturating_add(skip).min(*back);
        } else {
            for _ in 0..n {
                self.next()?;
            }
        }
        self.next()
    }
}

impl<T: 'static> DoubleEndedIterator for Variadic<'_, T> {
    fn next_back(&mut self) -> Option<T> {
        match &mut self.tail {
            Tail::One(one) => one.next_back(),
            Tail::Many(many) => many.next_back(),
            Tail::InPlace {
                slots,
                position,
                front,
                back,
            } => {
                if front >= back {
                    return None;
                }
                *back -= 1;
                read_back(slots.at(*back), position.saturating_add(*back))
            }
        }
    }
}

impl<T: 'static> ExactSizeIterator for Variadic<'_, T> {}

/// The tail read off a call of `count` arguments from `position` on,
/// `first` the argument there as the entry read it: in place over the
/// frame's slots where [`in_place`] names `T`, each held to it first
/// where its hold may refuse; otherwise each later one read off the
/// frame and held, none or one in place and a longer one on the heap at
/// once. An empty tail holds nothing, so no hold sees the `UNDEF` the
/// entry reads past the call's arguments.
fn tail<'c, T: FromArgument + 'static>(
    req: &mut Request,
    first: Value,
    position: u32,
    count: u32,
) -> Result<Variadic<'c, T>, Thrown> {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let mut held = match count.cmp(&position) {
        Less => return Ok(Variadic::none()),
        Equal => {
            return Ok(Variadic::of(Tail::One(
                T::hold(req, first, position)?.into_iter(),
            )));
        }
        Greater => {
            let len = count - position + 1;
            if let Some(checks) = in_place::<T>()
                && let Some(span) = req
                    .serving_call()
                    .and_then(|call| call.frame.span(Slot(position - 1), len as usize))
            {
                let slots = Slots::Frame(span);
                if checks {
                    for at in 0..len {
                        T::hold(req, slots.at(at), position.saturating_add(at))?;
                    }
                }
                return Ok(Variadic::of(Tail::InPlace {
                    slots,
                    position,
                    front: 0,
                    back: len,
                }));
            }
            Vec::with_capacity(len as usize)
        }
    };
    held.extend(T::hold(req, first, position)?);
    for at in position + 1..=count {
        let given = req.frames().argument(at).unwrap_or(Value::undef());
        if let Some(value) = T::hold(req, given, at)? {
            held.push(value);
        }
    }
    Ok(Variadic::of(Tail::Many(held.into_iter())))
}

impl<T: FromArgument + 'static> FromArgument for Variadic<'_, T> {
    const VARIADIC: bool = true;
    const BY_REF: bool = T::BY_REF;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn accepts(_: &Value) -> bool {
        true
    }
    fn from_value(_: &mut Request, _: Value) -> Result<Self, Thrown> {
        Ok(Variadic::none())
    }
    /// The tail is read off the call once, from `position` on, `given`
    /// standing first in it.
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let count = req.variadic_count()?;
        tail(req, given, position, count).map(Some)
    }
    /// A frameless call's operands from `position` on, read in place
    /// where a framed call's would be, and otherwise held, none or one
    /// in place and more on the heap, as a framed call's are.
    fn tail(req: &mut Request, given: &[Value], position: u32) -> Result<Self, Thrown> {
        let mut held = match given {
            [] => return Ok(Variadic::none()),
            &[one] => {
                return Ok(Variadic::of(Tail::One(
                    T::hold(req, one, position)?.into_iter(),
                )));
            }
            many => match (in_place::<T>(), many) {
                (Some(checks), &[a, b] | &[a, b, _]) => {
                    let operands = [a, b, many.get(2).copied().unwrap_or_else(Value::undef)];
                    let len = u32::try_from(many.len()).unwrap_or(3);
                    let slots = Slots::Operands(operands);
                    if checks {
                        for at in 0..len {
                            T::hold(req, slots.at(at), position.saturating_add(at))?;
                        }
                    }
                    return Ok(Variadic::of(Tail::InPlace {
                        slots,
                        position,
                        front: 0,
                        back: len,
                    }));
                }
                _ => Vec::with_capacity(many.len()),
            },
        };
        for (at, &value) in (position..).zip(given) {
            held.extend(T::hold(req, value, at)?);
        }
        Ok(Variadic::of(Tail::Many(held.into_iter())))
    }
}

/// `Z_PARAM_VARIADIC_WITH_NAMED`: the tail, and the extra named
/// arguments beside it, which `call_user_func` hands on. The list of
/// named ones is laid down only where the call names one, so a call
/// that names none allocates nothing past what its tail does:
///
/// ```php
/// function f(...$a) { return json_encode($a); }
/// echo call_user_func('f'), call_user_func('f', 1), call_user_func('f', 1, 2, 3), "\n";
/// echo call_user_func('f', 1, x: 2), call_user_func('f', x: 1, y: 2), "\n";
/// ```
/// ```expect
/// [][1][1,2,3]
/// {"0":1,"x":2}{"x":1,"y":2}
/// ```
#[derive(Debug, Clone)]
pub struct NamedVariadic<'c, T> {
    pub positional: Variadic<'c, T>,
    named: Vec<(PhpString, T)>,
}

impl<T> NamedVariadic<'_, T> {
    /// The extra named arguments, name and value in the order given.
    #[must_use]
    pub fn named(&self) -> &[(PhpString, T)] {
        &self.named
    }
}

impl<T: FromArgument + 'static> FromArgument for NamedVariadic<'_, T> {
    const VARIADIC: bool = true;
    const BY_REF: bool = T::BY_REF;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn accepts(_: &Value) -> bool {
        true
    }
    fn from_value(_: &mut Request, _: Value) -> Result<Self, Thrown> {
        Ok(NamedVariadic {
            positional: Variadic::none(),
            named: Vec::new(),
        })
    }
    /// The positional tail is read off the call from `position` on,
    /// `given` standing first in it, and the named arguments after it,
    /// their list laid down only where there are any.
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let count = u32::try_from(req.frames().argument_count()).unwrap_or(u32::MAX);
        let positional = tail(req, given, position, count)?;
        let passed = req.named_arguments();
        let mut named = Vec::with_capacity(passed.len());
        for (name, value) in passed {
            named.push((name, held_as::<T>(req, value, count.saturating_add(1))?));
        }
        Ok(Some(NamedVariadic { positional, named }))
    }
}

/// A type that admits `default = null`: the macro asks for this bound
/// where a parameter's default is `null`, so a `null` on a type that
/// cannot hold it is a build error.
pub trait NullDefault {}

impl<T> NullDefault for Option<T> {}
impl NullDefault for Value {}
impl<T: NullDefault> NullDefault for Untyped<T> {}
impl<T: NullDefault> NullDefault for Raw<T> {}
impl<T> NullDefault for Ref<T> {}
impl<T: NullDefault> NullDefault for Passed<T> {}

// ---------------------------------------------------------------- returns

/// A type a native may return (design §7.1): its declared type and its
/// conversion to the value the engine is handed. An [`Owner`] is moved
/// to the engine; a view is the engine's to own, so its conversion takes
/// an owner of its own, `RETURN_COPY` (`Z_TRY_ADDREF`): a native returns
/// an argument, an element of one or a property as it reads it, and a
/// value it made as the owner it was made with.
///
/// [`Owner`]: crate::api::values::Owner
pub trait IntoReturn: Sized {
    /// Whether this return declares nothing, which `#[php(tentative)]`
    /// refuses at build time.
    const UNDECLARED: bool = false;
    /// The declared type's pure mask, as [`FromArgument::MASK`].
    const MASK: u32;
    /// The declared return type; `None` declares none.
    fn declared() -> Option<Type>;
    /// The value the engine is handed, its owner the engine's: a view's
    /// taken, `RETURN_COPY`, and an owner's moved; an `Err` passes
    /// through.
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown>;
}

/// `void`, and nothing on `__construct` and `__destruct`.
impl IntoReturn for () {
    const MASK: u32 = bits::VOID;
    fn declared() -> Option<Type> {
        Some(Type::Void)
    }
    fn into_return(self, _: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::null())
    }
}

impl IntoReturn for Value {
    const MASK: u32 = bits::ANY;
    fn declared() -> Option<Type> {
        Some(Type::Mixed)
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::take(req, self))
    }
}

impl<T: IntoReturn> IntoReturn for Untyped<T> {
    const UNDECLARED: bool = true;
    const MASK: u32 = 0;
    fn declared() -> Option<Type> {
        None
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        self.0.into_return(req)
    }
}

/// A `Value` returned as it is under `T`'s declaration: php verifies an
/// internal return only in debug builds. A view, which the engine takes
/// an owner of.
impl<T: FromArgument> IntoReturn for Raw<T> {
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::take(req, self.value))
    }
}

/// A return of a scalar or a view, the value it makes owned by the
/// engine: `Z_TRY_ADDREF` takes nothing of a scalar.
macro_rules! plain_return {
    ($t:ty, $ty:expr, $mask:expr, $make:expr) => {
        impl IntoReturn for $t {
            const MASK: u32 = $mask;
            fn declared() -> Option<Type> {
                Some($ty)
            }
            fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
                Ok(Owner::take(req, ($make)(self)))
            }
        }
    };
}
plain_return!(bool, Type::Bool, bits::FALSE | bits::TRUE, Value::bool);
plain_return!(i64, Type::Int, bits::LONG, Value::int);
plain_return!(f64, Type::Float, bits::DOUBLE, Value::float);
plain_return!(PhpString, Type::String, bits::STRING, Value::string);
plain_return!(PhpArray, Type::Array, bits::ARRAY, Value::array);
plain_return!(PhpObject, Type::Object, bits::OBJECT, Value::object);

/// `true`.
#[derive(Debug, Clone, Copy)]
pub struct True;
plain_return!(True, Type::True, bits::TRUE, |_| Value::bool(true));

/// `false`.
#[derive(Debug, Clone, Copy)]
pub struct False;
plain_return!(False, Type::False, bits::FALSE, |_| Value::bool(false));

/// `false` as a member of a parameter's union, `dba_key_split`'s
/// `string|false|null $key`: held only by `false` itself.
impl FromArgument for False {
    const MASK: u32 = bits::FALSE;
    fn declared() -> Option<Type> {
        Some(Type::False)
    }
    fn accepts(value: &Value) -> bool {
        matches!(value.as_ref(), ValueRef::Bool(false))
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        match value.as_ref() {
            ValueRef::Bool(false) => Ok(False),
            _ => mismatch(req, "false"),
        }
    }
}

/// `null`.
#[derive(Debug, Clone, Copy)]
pub struct Null;
plain_return!(Null, Type::Null, bits::NULL, |_| Value::null());

/// `static`: an object of the called scope.
#[derive(Debug, Clone, Copy)]
pub struct Static(pub PhpObject);
plain_return!(Static, Type::CalledClass, bits::STATIC, |own: Static| {
    own.0.as_value()
});

/// `never`: the body can only return an `Err`.
#[derive(Debug, Clone, Copy)]
pub enum Never {}

impl IntoReturn for Never {
    const MASK: u32 = bits::NEVER;
    fn declared() -> Option<Type> {
        Some(Type::Never)
    }
    fn into_return(self, _: &mut Request) -> Result<Owner<Value>, Thrown> {
        match self {}
    }
}

impl IntoReturn for Resource {
    const UNDECLARED: bool = true;
    const MASK: u32 = bits::RESOURCE;
    fn declared() -> Option<Type> {
        None
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::take(req, Value::resource(self)))
    }
}

/// `callable` as a return, `RETURN_COPY` of the value the callable came
/// from: the callable holds it as a view, so the return takes an owner.
impl IntoReturn for Callable {
    const MASK: u32 = bits::CALLABLE;
    fn declared() -> Option<Type> {
        Some(Type::Callable)
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::take(req, self.value()))
    }
}

impl<C: Class> IntoReturn for Instance<C> {
    const MASK: u32 = 0;
    fn declared() -> Option<Type> {
        Some(named::<C>())
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::take(req, self.object.as_value()))
    }
}

impl<T: IntoReturn> IntoReturn for Option<T> {
    const MASK: u32 = nullable_mask(T::MASK);
    fn declared() -> Option<Type> {
        T::declared().map(or_null)
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        match self {
            Some(value) => value.into_return(req),
            None => Ok(Owner::null()),
        }
    }
}

/// A return of `T|false`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrFalse<T> {
    Value(T),
    False,
}

impl<T: IntoReturn> IntoReturn for OrFalse<T> {
    const MASK: u32 = union_mask(T::MASK, bits::FALSE);
    fn declared() -> Option<Type> {
        Some(flatten([T::declared(), Some(Type::False)]))
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        match self {
            OrFalse::Value(value) => value.into_return(req),
            OrFalse::False => Ok(Owner::bool(false)),
        }
    }
}

impl<A: IntoReturn, B: IntoReturn> IntoReturn for Union<A, B> {
    const MASK: u32 = union_mask(A::MASK, B::MASK);
    fn declared() -> Option<Type> {
        Some(flatten([A::declared(), B::declared()]))
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        match self {
            Union::A(a) => a.into_return(req),
            Union::B(b) => b.into_return(req),
        }
    }
}

/// `Result<T, Thrown>`: an `Err` passes through.
impl<T: IntoReturn> IntoReturn for Result<T, Thrown> {
    const UNDECLARED: bool = T::UNDECLARED;
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        self?.into_return(req)
    }
}
