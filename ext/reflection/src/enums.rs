//! The three entries `zend_enum_register_funcs` gives every enum —
//! `UnitEnum::cases`, `BackedEnum::from` and `tryFrom` — and the three
//! enum reflectors over them: `ReflectionEnum`, whose cases are the
//! class's constants, and `ReflectionEnumUnitCase` with
//! `ReflectionEnumBackedCase` under it, each of which names one case
//! and hands back the instance the engine keeps for it
//! (`zend_enum_get_case`, served here through `call::class_constant`).
//!
//! The one enum this engine can be asked about is the stub's own
//! `PropertyHookType`, since the compiler reads no `enum` a program
//! writes (#466). The three entries hang off the stub rather than off
//! every linked enum for the same reason: php's `zend_enum_register_funcs`
//! is served at link time and would have nothing else to reach, and the
//! entries read [`Request::called_scope`], so they serve whichever enum
//! they were called on once there is more than one.

use php_ext::call::class_constant;
use php_ext::coerce::{coerce_param, to_string};
use php_ext::decl::{ClassKind, EnumBacking, Type};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, PhpArray, PhpObject, Value};

use super::signature::type_factory;
use super::{
    class_argument, class_constant_named, class_name_of, class_named, exactly_arguments,
    new_object, one_string_argument, set_name, set_string_prop, string_argument, this_name,
};

/// `ce->enum_backing_type`: the type of a backed enum's `value`, and
/// `None` for a pure one.
fn backing_of(req: &Request, class: ClassId) -> Option<Type> {
    match req.class_decl(class).map(|decl| decl.backing) {
        Some(EnumBacking::Int) => Some(Type::Int),
        Some(EnumBacking::String) => Some(Type::String),
        _ => None,
    }
}

/// `ZEND_ACC_ENUM` on the class a `ReflectionEnum` names, or php's
/// `ReflectionException` for one that is no enum.
fn this_enum(req: &mut Request) -> Result<ClassId, Thrown> {
    let (_, name) = this_name(req, "ReflectionEnum")?;
    let class = class_named(req, &name)?;
    held_to_enum(req, class)?;
    Ok(class)
}

/// `ReflectionEnum::__construct`'s own refusal, after
/// `reflection_class_object_ctor` has done its half.
fn held_to_enum(req: &mut Request, class: ClassId) -> Result<(), Thrown> {
    if req
        .class_decl(class)
        .is_some_and(|decl| decl.kind == ClassKind::Enum)
    {
        return Ok(());
    }
    let name = class_name_of(req, class);
    Err(throw(
        req,
        "ReflectionException",
        &format!(
            "Class \"{}\" is not an enum",
            String::from_utf8_lossy(&name)
        ),
    ))
}

/// The case names of an enum, in declaration order: the class's own
/// constants, which a `ClassDecl` of kind `Enum` carries as its cases.
fn case_names(req: &Request, class: ClassId) -> Vec<Vec<u8>> {
    req.class_decl(class).map_or_else(Vec::new, |decl| {
        decl.constants
            .iter()
            .map(|constant| constant.name.0.to_vec())
            .collect()
    })
}

/// The enum the call was made on, `zend_get_called_scope`, which is
/// what a case entry serves.
fn called_enum(req: &mut Request) -> Result<ClassId, Thrown> {
    let Some(class) = req.called_scope() else {
        return Err(throw(
            req,
            "Error",
            "Internal error: an enum entry served with no called scope",
        ));
    };
    held_to_enum(req, class)?;
    Ok(class)
}

/// `UnitEnum::cases(): array`, `zend_enum_cases`: every case of the
/// enum, in declaration order, each the one instance the engine keeps
/// for it.
///
/// ```php
/// $cases = PropertyHookType::cases();
/// var_dump($cases);
/// var_dump($cases[0] === PropertyHookType::Get);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   enum(PropertyHookType::Get)
///   [1]=>
///   enum(PropertyHookType::Set)
/// }
/// bool(true)
/// ```
pub(super) fn enum_cases(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = called_enum(req)?;
    let mut out = PhpArray::new(req)?;
    for name in case_names(req, class) {
        let case = class_constant(req, class, &name)?;
        out.append(req, case)?;
    }
    Ok(Value::array(out))
}

/// `BackedEnum::from(int|string $value): static`, `zend_enum_from_base`
/// with `try_from` clear: the case whose backing value is `$value`, and
/// php's `ValueError` for one no case carries.
///
/// ```php
/// var_dump(PropertyHookType::from('set'));
/// try { PropertyHookType::from('nope'); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
/// try { PropertyHookType::from(1); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
/// ```
/// ```expect
/// enum(PropertyHookType::Set)
/// ValueError: "nope" is not a valid backing value for enum PropertyHookType
/// ValueError: "1" is not a valid backing value for enum PropertyHookType
/// ```
pub(super) fn enum_from(req: &mut Request) -> Result<Value, Thrown> {
    from_backing(req, false)
}

/// `BackedEnum::tryFrom(int|string $value): ?static`, the same with
/// `try_from` set: `null` where `from` raises.
///
/// ```php
/// var_dump(PropertyHookType::tryFrom('get'), PropertyHookType::tryFrom('nope'));
/// ```
/// ```expect
/// enum(PropertyHookType::Get)
/// NULL
/// ```
pub(super) fn enum_try_from(req: &mut Request) -> Result<Value, Thrown> {
    from_backing(req, true)
}

/// `zend_enum_from_base`: the argument coerced to the enum's backing
/// type as the parameter's declared type coerces it, then the case
/// whose backing value it equals.
fn from_backing(req: &mut Request, try_from: bool) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let class = called_enum(req)?;
    let Some(backing) = backing_of(req, class) else {
        let name = class_name_of(req, class);
        return Err(throw(
            req,
            "Error",
            &format!("Enum {} is not backed", String::from_utf8_lossy(&name)),
        ));
    };
    let given = req.argument(1).unwrap_or_default().deref();
    let wanted = coerce_param(req, &given, &backing, 1)?;
    for name in case_names(req, class) {
        let case = class_constant(req, class, &name)?;
        let held = case
            .as_object()
            .and_then(|object| object.prop(req, b"value"))
            .unwrap_or_default();
        if php_ext::coerce::identical(&held, &wanted) {
            return Ok(case);
        }
        req.release(case);
    }
    if try_from {
        return Ok(Value::null());
    }
    let name = class_name_of(req, class);
    let shown = match backing {
        Type::Int => to_string(req, &wanted)
            .map(|s| String::from_utf8_lossy(s.as_bytes()).into_owned())
            .unwrap_or_default(),
        _ => format!(
            "\"{}\"",
            to_string(req, &wanted)
                .map(|s| String::from_utf8_lossy(s.as_bytes()).into_owned())
                .unwrap_or_default()
        ),
    };
    Err(throw(
        req,
        "ValueError",
        &format!(
            "{shown} is not a valid backing value for enum {}",
            String::from_utf8_lossy(&name)
        ),
    ))
}

/// `ReflectionEnum::__construct(object|string $objectOrClass)`:
/// `reflection_class_object_ctor` and then php's own refusal for a
/// class that is no enum.
///
/// ```php
/// var_dump(new ReflectionEnum('PropertyHookType'));
/// try { new ReflectionEnum('Exception'); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(ReflectionEnum)#1 (1) {
///   ["name"]=>
///   string(16) "PropertyHookType"
/// }
/// ReflectionException: Class "Exception" is not an enum
/// ```
pub(super) fn reflection_enum_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let (class, _) = class_argument(req, 1)?;
    held_to_enum(req, class)?;
    let name = class_name_of(req, class);
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
    }
    Ok(Value::null())
}

/// `ReflectionEnum::isBacked(): bool`: `ce->enum_backing_type != IS_UNDEF`.
///
/// ```php
/// var_dump((new ReflectionEnum('PropertyHookType'))->isBacked());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_enum_is_backed(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_enum(req)?;
    Ok(Value::bool(backing_of(req, class).is_some()))
}

/// `ReflectionEnum::getBackingType(): ?ReflectionNamedType`: the
/// backing type as a type reflector, and `null` for a pure enum.
///
/// ```php
/// var_dump((string) (new ReflectionEnum('PropertyHookType'))->getBackingType());
/// ```
/// ```expect
/// string(6) "string"
/// ```
pub(super) fn reflection_enum_get_backing_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_enum(req)?;
    match backing_of(req, class) {
        Some(backing) => {
            let object = type_factory(req, &backing, false)?;
            Ok(object.as_value())
        }
        None => Ok(Value::null()),
    }
}

/// `ReflectionEnum::hasCase(string $name): bool`: whether the
/// constants table holds a case of that name.
///
/// ```php
/// $e = new ReflectionEnum('PropertyHookType');
/// var_dump($e->hasCase('Get'), $e->hasCase('Nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_enum_has_case(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_enum(req)?;
    Ok(Value::bool(case_names(req, class).contains(&name)))
}

/// `ReflectionEnum::getCase(string $name): ReflectionEnumUnitCase`: the
/// case reflector for that name, php's `ReflectionException` for a name
/// the enum has no case under.
///
/// ```php
/// $e = new ReflectionEnum('PropertyHookType');
/// var_dump($e->getCase('Get'));
/// try { $e->getCase('Nope'); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(ReflectionEnumBackedCase)#2 (2) {
///   ["name"]=>
///   string(3) "Get"
///   ["class"]=>
///   string(16) "PropertyHookType"
/// }
/// ReflectionException: Case PropertyHookType::Nope does not exist
/// ```
pub(super) fn reflection_enum_get_case(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_enum(req)?;
    if !case_names(req, class).contains(&name) {
        let held = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Case {}::{} does not exist",
                String::from_utf8_lossy(&held),
                String::from_utf8_lossy(&name)
            ),
        ));
    }
    let object = case_factory(req, class, &name)?;
    Ok(object.as_value())
}

/// `ReflectionEnum::getCases(): array`: one case reflector per case, in
/// declaration order.
///
/// ```php
/// var_dump(count((new ReflectionEnum('PropertyHookType'))->getCases()));
/// ```
/// ```expect
/// int(2)
/// ```
pub(super) fn reflection_enum_get_cases(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_enum(req)?;
    let mut out = PhpArray::new(req)?;
    for name in case_names(req, class) {
        let object = case_factory(req, class, &name)?;
        out.append(req, object.as_value())?;
    }
    Ok(Value::array(out))
}

/// php's `reflection_enum_case_factory`: a `ReflectionEnumBackedCase`
/// for a backed enum and a `ReflectionEnumUnitCase` for a pure one,
/// with `$name` and `$class` written as
/// `reflection_class_constant_factory` writes them.
fn case_factory(req: &mut Request, class: ClassId, name: &[u8]) -> Result<PhpObject, Thrown> {
    let reflector = if backing_of(req, class).is_some() {
        class_named(req, b"ReflectionEnumBackedCase")?
    } else {
        class_named(req, b"ReflectionEnumUnitCase")?
    };
    let object = new_object(req, reflector)?;
    let held = class_name_of(req, class);
    set_name(req, object, name)?;
    set_string_prop(req, object, b"class", &held)?;
    Ok(object)
}

/// The enum and the case a `ReflectionEnumUnitCase` names, php's
/// `GET_REFLECTION_OBJECT_PTR(ref)` over `intern->ptr`.
fn this_case(req: &mut Request) -> Result<(ClassId, Vec<u8>), Thrown> {
    let (this, name) = this_name(req, "ReflectionEnumUnitCase")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw(
            req,
            "Error",
            "Typed property ReflectionClassConstant::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    held_to_enum(req, class)?;
    Ok((class, name))
}

/// `ReflectionEnumUnitCase::__construct(object|string $class, string
/// $constant)`: `ReflectionClassConstant`'s constructor and then php's
/// refusals for a class that is no enum and a constant that is no case.
///
/// ```php
/// var_dump(new ReflectionEnumUnitCase('PropertyHookType', 'Set'));
/// try { new ReflectionEnumUnitCase('Nope', 'Set'); } catch (Throwable $t) { echo get_class($t), ': ', $t->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(ReflectionEnumUnitCase)#1 (2) {
///   ["name"]=>
///   string(3) "Set"
///   ["class"]=>
///   string(16) "PropertyHookType"
/// }
/// ReflectionException: Class "Nope" does not exist
/// ```
pub(super) fn reflection_enum_case_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 2)?;
    let (class, _) = class_argument(req, 1)?;
    let name = string_argument(req, 2)?;
    held_to_enum(req, class)?;
    if class_constant_named(req, class, &name).is_none() {
        let held = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Case {}::{} does not exist",
                String::from_utf8_lossy(&held),
                String::from_utf8_lossy(&name)
            ),
        ));
    }
    let held = class_name_of(req, class);
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
        set_string_prop(req, this, b"class", &held)?;
    }
    Ok(Value::null())
}

/// `ReflectionEnumUnitCase::getEnum(): ReflectionEnum`: the enum the
/// case belongs to.
///
/// ```php
/// $c = (new ReflectionEnum('PropertyHookType'))->getCase('Get');
/// var_dump($c->getEnum());
/// ```
/// ```expect
/// object(ReflectionEnum)#1 (1) {
///   ["name"]=>
///   string(16) "PropertyHookType"
/// }
/// ```
pub(super) fn reflection_enum_case_get_enum(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (class, _) = this_case(req)?;
    let reflector = class_named(req, b"ReflectionEnum")?;
    let object = new_object(req, reflector)?;
    let name = class_name_of(req, class);
    set_name(req, object, &name)?;
    Ok(object.as_value())
}

/// `ReflectionEnumUnitCase::getValue(): UnitEnum`: the one instance the
/// engine keeps for the case, `zend_enum_get_case`.
///
/// ```php
/// $c = (new ReflectionEnum('PropertyHookType'))->getCase('Get');
/// var_dump($c->getValue(), $c->getValue() === PropertyHookType::Get);
/// ```
/// ```expect
/// enum(PropertyHookType::Get)
/// bool(true)
/// ```
pub(super) fn reflection_enum_case_get_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (class, name) = this_case(req)?;
    class_constant(req, class, &name)
}

/// `ReflectionEnumBackedCase::getBackingValue(): int|string`: the
/// case's `value`, `zend_enum_fetch_case_value`.
///
/// ```php
/// $c = (new ReflectionEnum('PropertyHookType'))->getCase('Set');
/// var_dump($c->getBackingValue());
/// ```
/// ```expect
/// string(3) "set"
/// ```
pub(super) fn reflection_enum_case_get_backing_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (class, name) = this_case(req)?;
    let case = class_constant(req, class, &name)?;
    let value = case
        .as_object()
        .and_then(|object| object.prop(req, b"value"))
        .unwrap_or_default();
    req.addref(&value);
    req.release(case);
    Ok(value)
}
