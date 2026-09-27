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
//! number per method per request when its class is declared
//! ([`ENUM_FUNCTIONS`]), the scope of each the enum itself, as php
//! allocates a `zend_internal_function` per method per enum.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use php_unit::decl::{ClassKind, EnumBacking, Type};

use crate::api::abi::{CloneObj, Entry, ObjectHandlers};
use crate::api::raising::{Thrown, arg_count_error, throw};
use crate::api::request::Request;
use crate::api::values::{ClassId, PhpArray, PhpString, Value};

/// Where the function numbers of a program's enum methods begin: past
/// every native, native method and user function a request can number,
/// so the three spaces never meet.
pub(crate) const ENUM_FUNCTIONS: u32 = 1 << 30;

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
pub(crate) struct EnumHandlers;

impl ObjectHandlers for EnumHandlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn compare(
        &self,
        _: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let same = match (left.as_object(), right.as_object()) {
            (Some(left), Some(right)) => left.index() == right.index(),
            _ => false,
        };
        Ok(same.then_some(Ordering::Equal))
    }
}

pub(crate) static ENUM: EnumHandlers = EnumHandlers;

/// The enum a call was made on, which is the scope of the three
/// methods: an enum is final, so the called scope is the enum itself.
fn called_enum(req: &mut Request) -> Result<ClassId, Thrown> {
    req.called_scope()
        .filter(|class| {
            req.class_decl(*class)
                .is_some_and(|decl| decl.kind == ClassKind::Enum)
        })
        .ok_or_else(|| {
            throw(
                req,
                "Error",
                "InternalError: an enum method served on no enum",
            )
        })
}

/// `ZEND_PARSE_PARAMETERS_NONE` and `ZEND_PARSE_PARAMETERS_START(1, 1)`.
fn exactly(req: &mut Request, wanted: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count == wanted {
        return Ok(());
    }
    let noun = if wanted == 1 { "argument" } else { "arguments" };
    Err(arg_count_error(
        req,
        &format!("exactly {wanted} {noun}, {count} given"),
    ))
}

/// The names of an enum's cases in declaration order, the constants
/// `ZEND_CLASS_CONST_IS_CASE` marks.
pub(crate) fn case_names(req: &Request, class: ClassId) -> Vec<Vec<u8>> {
    req.class_decl(class).map_or_else(Vec::new, |decl| {
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
fn cases(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 0)?;
    let class = called_enum(req)?;
    let mut out = PhpArray::new(req)?;
    for name in case_names(req, class) {
        let case = req.enum_case(class, &name)?;
        out.append(req, case.as_value())?;
    }
    Ok(Value::array(out))
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
fn from(req: &mut Request) -> Result<Value, Thrown> {
    by_value(req, false)
}

/// `BackedEnum::tryFrom()`: `null` where `from` raises its `ValueError`.
fn try_from(req: &mut Request) -> Result<Value, Thrown> {
    by_value(req, true)
}

/// `zend_enum_from_base` and `zend_enum_get_case_by_value`: the
/// constants updated first, which is where a case of the wrong type or
/// a duplicate is refused, then the case the value names.
///
/// A string-backed enum outside `strict_types` takes its argument as
/// `Z_PARAM_STR_OR_LONG` takes it and converts an int itself
/// (`zend_long_to_str`), never in the frame, so a trace prints the int
/// that was passed; the value [`crate::api::coerce::coerce_param`]
/// hands back is the frame's, a view nothing here releases.
///
/// ```php
/// enum Suit: string { case Hearts = 'H'; case Spades = 'H'; }
/// enum N: string { case A = '42'; }
/// var_dump(N::from(42));
/// try { Suit::from(42); } catch (Error $e) { echo $e->getMessage(), "\n"; var_dump($e->getTrace()[0]['args']); }
/// try { Suit::tryFrom('bar'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// enum(N::A)
/// Duplicate value in enum Suit for cases Hearts and Spades
/// array(1) {
///   [0]=>
///   int(42)
/// }
/// Duplicate value in enum Suit for cases Hearts and Spades
/// ```
fn by_value(req: &mut Request, try_from: bool) -> Result<Value, Thrown> {
    exactly(req, 1)?;
    let class = called_enum(req)?;
    let backing = req
        .class_decl(class)
        .map_or(EnumBacking::Pure, |decl| decl.backing);
    let ty = match backing {
        EnumBacking::Int => Type::Int,
        EnumBacking::String => Type::String,
        EnumBacking::Pure => {
            let name = req.class_name(class);
            return Err(throw(req, "Error", &format!("Enum {name} is not backed")));
        }
    };
    let given = req.argument(1).unwrap_or_default().deref();
    let own = match given.as_int() {
        Some(n) if backing == EnumBacking::String && !req.strict_types() => Some(Value::string(
            PhpString::new(req, n.to_string().as_bytes())?,
        )),
        _ => None,
    };
    let wanted = match own {
        Some(own) => own,
        None => crate::api::coerce::coerce_param(req, &given, &ty, 1)?,
    };
    let found = case_by_value(req, class, &wanted, try_from);
    if let Some(own) = own {
        req.release(own);
    }
    found
}

/// `zend_enum_get_case_by_value` over a value already held to the
/// backing type: the case whose value is `wanted`, `null` for none under
/// `tryFrom`, and php's `ValueError` for none under `from`. `wanted` is
/// a view.
fn case_by_value(
    req: &mut Request,
    class: ClassId,
    wanted: &Value,
    try_from: bool,
) -> Result<Value, Thrown> {
    req.update_enum_constants(class)?;
    for name in case_names(req, class) {
        let case = req.enum_case(class, &name)?;
        let held = case.prop(req, b"value").unwrap_or_default();
        if crate::api::coerce::identical(&held, wanted) {
            return Ok(case.as_value());
        }
        req.release(case.as_value());
    }
    if try_from {
        return Ok(Value::null());
    }
    let class_name = req.class_name(class);
    let shown = match wanted.as_int() {
        Some(n) => n.to_string(),
        None => format!(
            "\"{}\"",
            String::from_utf8_lossy(wanted.as_str_bytes().unwrap_or_default())
        ),
    };
    Err(throw(
        req,
        "ValueError",
        &format!("{shown} is not a valid backing value for enum {class_name}"),
    ))
}
