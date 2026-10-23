//! What `Z/Zend/zend_enum.c` gives every enum: the handler table
//! `zend_register_enum_ce` builds for enum objects, and the three
//! methods `zend_enum_register_funcs` adds, `cases`, and for a backed
//! enum `from` and `tryFrom`, served here for a native enum and a
//! program's alike.
//!
//! The declarations of the three are [`php_unit::enum_methods`], one
//! list the compiler appends to an enum a program declares and the
//! engine to an enum a native declares. A native enum's rows are served
//! as its other native methods are; a program's enum gets one function
//! number per method where its class is bound, the request's own or the
//! published arena's ([`crate::api::call::Function::CLASS_NATIVE`]), the scope
//! of each the enum itself, as php allocates a `zend_internal_function`
//! per method per enum.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{ClassKind, EnumBacking, Type};

use crate::api::abi::{Entry, Refusals};
use crate::api::raising::{Thrown, arity, error, throw_error};
use crate::api::request::{ByValue, Request};
use crate::api::values::{ClassId, Owner, Value, adopted};

/// Which of the three a function number of a program's enum serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnumMethod {
    Cases,
    From,
    TryFrom,
}

impl EnumMethod {
    /// The method's name as the stub writes it.
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            EnumMethod::Cases => b"cases",
            EnumMethod::From => b"from",
            EnumMethod::TryFrom => b"tryFrom",
        }
    }

    /// The method a lowered name is, for an enum of `backing`: a pure
    /// enum has `cases` alone.
    pub(crate) fn of(lowered: &[u8], backing: EnumBacking) -> Option<EnumMethod> {
        match lowered {
            b"cases" => Some(EnumMethod::Cases),
            b"from" if backing != EnumBacking::Pure => Some(EnumMethod::From),
            b"tryfrom" if backing != EnumBacking::Pure => Some(EnumMethod::TryFrom),
            _ => None,
        }
    }

    /// The entry that serves it.
    pub(crate) fn entry(self) -> Entry {
        match self {
            EnumMethod::Cases => cases,
            EnumMethod::From => from,
            EnumMethod::TryFrom => try_from,
        }
    }
}

/// `zend_enum_object_handlers`: `clone_obj` null, so a case is never
/// copied, and `compare` `zend_objects_not_comparable`, so two cases are
/// equal only as one object.
///
/// ```php
/// enum Foo { case A; case B; }
/// try { clone Foo::A; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(Foo::A == Foo::A, Foo::A == Foo::B, Foo::A < Foo::B, Foo::A > Foo::B);
/// ```
/// ```expect
/// Trying to clone an uncloneable object of class Foo
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(crate) static ENUM: Refusals = Refusals::NONE.cloning().comparison();

/// The enum a call was made on, which is the scope of the three
/// methods: an enum is final, so the called scope is the enum itself.
fn called_enum(req: &mut Request) -> Result<ClassId, Thrown> {
    req.frames()
        .called_scope()
        .filter(|class| req.class_is(*class, ClassKind::Enum))
        .ok_or_else(|| error(req, "InternalError: an enum method served on no enum"))
}

/// The names of an enum's cases in declaration order, the constants
/// `ZEND_CLASS_CONST_IS_CASE` marks.
pub(crate) fn case_names(req: &Request, class: ClassId) -> Vec<Vec<u8>> {
    req.symbols()
        .class_decl(class)
        .map_or_else(Vec::new, |decl| {
            decl.constants
                .iter()
                .filter(|constant| constant.is_case)
                .map(|constant| constant.name.0.to_vec())
                .collect()
        })
}

/// `UnitEnum::cases()`, `zend_enum_cases_func`: every case in
/// declaration order, each the one instance the request keeps for it;
/// the enum's own constants are left out.
///
/// ```php
/// enum Suit { case Hearts; case Spades; const Wild = self::Spades; }
/// var_dump(Suit::cases(), Suit::cases()[1] === Suit::Wild);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   enum(Suit::Hearts)
///   [1]=>
///   enum(Suit::Spades)
/// }
/// bool(true)
/// ```
fn cases(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    arity(req, 0, Some(0))?; // `ZEND_PARSE_PARAMETERS_NONE`
    let class = called_enum(req)?;
    let mut out = req.array()?;
    for name in case_names(req, class) {
        let case = req.enum_case(class, &name)?;
        out.append(req, Owner::adopt(case.as_value()))?;
    }
    Ok(Owner::adopt(Value::array(out)))
}

/// `BackedEnum::from()`, `zend_enum_from_base` with `try_from` clear:
/// the argument coerced to the backing type as `Z_PARAM_LONG` and
/// `Z_PARAM_STR` coerce it, and the case whose value it is.
///
/// ```php
/// enum Suit: string { case Hearts = 'H'; case Spades = 'S'; }
/// enum Level: int { case Low = 1; case High = 10; }
/// var_dump(Suit::from('S'), Level::from('10'), Level::tryFrom(3));
/// try { Suit::from('X'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { Level::from(7); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { Level::from('x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// enum(Suit::Spades)
/// enum(Level::High)
/// NULL
/// "X" is not a valid backing value for enum Suit
/// 7 is not a valid backing value for enum Level
/// Level::from(): Argument #1 ($value) must be of type int, string given
/// ```
fn from(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    adopted(by_value(req, false))
}

/// `BackedEnum::tryFrom()`: `null` where `from` raises its `ValueError`.
fn try_from(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    adopted(by_value(req, true))
}

/// The key `zend_enum_from_base` parses the argument into, one owner
/// the caller's: `Z_PARAM_LONG` for an int enum; for a string enum
/// `Z_PARAM_STR` under `strict_types` and otherwise
/// `Z_PARAM_STR_OR_LONG`, whose weak arm reads a long first, so a float
/// or a bool is the long it truncates to and that long's decimal, made
/// here and never written over the argument (`zend_long_to_str`), and
/// only what is neither is read as a string.
fn backing_key(req: &mut Request, given: &Value, backing: EnumBacking) -> Result<Value, Thrown> {
    use crate::api::coerce::coerce_param;
    use crate::api::values::ValueRef;
    if backing == EnumBacking::Int {
        return coerce_param(req, given, &Type::Int, 1);
    }
    if req.strict_types() {
        let key = coerce_param(req, given, &Type::String, 1)?;
        req.addref(&key);
        return Ok(key);
    }
    let long = match given.as_ref() {
        ValueRef::Str(_) => None,
        ValueRef::Int(n) => Some(n),
        ValueRef::Float(_) | ValueRef::Bool(_) | ValueRef::Null | ValueRef::Undef => {
            coerce_param(req, given, &Type::Int, 1)?.as_int()
        }
        _ => {
            let either = Type::Union(vec![Type::String, Type::Int]);
            let key = coerce_param(req, given, &either, 1)?;
            req.addref(&key);
            return Ok(key);
        }
    };
    if let Some(n) = long {
        return Ok(Value::string(
            req.string(crate::c_number::Long::of(n).as_bytes())?,
        ));
    }
    req.addref(given);
    Ok(*given)
}

/// `zend_enum_from_base` and `zend_enum_get_case_by_value`: the
/// constants updated first, which is where a case of the wrong type or
/// a duplicate is refused, then the case the value names. An int given
/// to a string enum is compared as its decimal and stays an int in the
/// frame, so the trace shows it as given:
///
/// ```php
/// enum S: string { case A = "42"; }
/// var_dump(S::from(42), S::tryFrom(43));
/// try { S::from(7); } catch (ValueError $e) { echo $e->getMessage(), "\n", $e->getTraceAsString(), "\n"; }
/// ```
/// ```expect
/// enum(S::A)
/// NULL
/// "7" is not a valid backing value for enum S
/// #0 %s(4): S::from(7)
/// #1 {main}
/// ```
fn by_value(req: &mut Request, try_from: bool) -> Result<Value, Thrown> {
    arity(req, 1, Some(1))?; // `ZEND_PARSE_PARAMETERS_START(1, 1)`
    let class = called_enum(req)?;
    let backing = req
        .symbols()
        .class_decl(class)
        .map_or(EnumBacking::Pure, |decl| decl.backing);
    if backing == EnumBacking::Pure {
        let name = req.symbols().class_name(class);
        return Err(error(req, &format!("Enum {name} is not backed")));
    }
    let given = req.frames().argument(1).unwrap_or_default().deref();
    let wanted = backing_key(req, &given, backing)?;
    if let Err(thrown) = req.update_enum_constants(class) {
        req.release(wanted);
        return Err(thrown);
    }
    match req.backed_case_named(class, &wanted) {
        ByValue::Named(spelled) => {
            req.release(wanted);
            let Some(name) = spelled.as_str_bytes() else {
                return Err(crate::api::raising::fatal(
                    req,
                    "InternalError: a backed enum's table names a case by no string",
                ));
            };
            return req.enum_case(class, name).map(|case| case.as_value());
        }
        ByValue::Missing => return not_backing(req, class, wanted, try_from),
        // A native enum, whose table is the stub's: its cases read in turn.
        ByValue::Unbuilt => {}
    }
    for name in case_names(req, class) {
        let case = match req.enum_case(class, &name) {
            Ok(case) => case,
            Err(thrown) => {
                req.release(wanted);
                return Err(thrown);
            }
        };
        let held = case.prop(req, b"value").unwrap_or_default();
        // A backing value is an int or a string, the key php's
        // `backed_enum_table` finds it by, so two are the same where
        // the values are equal.
        if held == wanted {
            req.release(wanted);
            return Ok(case.as_value());
        }
        req.release(case.as_value());
    }
    not_backing(req, class, wanted, try_from)
}

/// `zend_enum_get_case_by_value`'s `not_found`: `null` for `tryFrom`,
/// and php's `ValueError` naming `wanted` for `from`, which is given up.
fn not_backing(
    req: &mut Request,
    class: ClassId,
    wanted: Value,
    try_from: bool,
) -> Result<Value, Thrown> {
    if try_from {
        req.release(wanted);
        return Ok(Value::null());
    }
    let class_name = req.symbols().class_name(class);
    let shown = match wanted.as_int() {
        Some(n) => n.to_string(),
        None => format!(
            "\"{}\"",
            String::from_utf8_lossy(wanted.as_str_bytes().unwrap_or_default())
        ),
    };
    req.release(wanted);
    Err(throw_error(
        req,
        "ValueError",
        &format!("{shown} is not a valid backing value for enum {class_name}"),
    ))
}
