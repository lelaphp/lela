//! `ReflectionAttribute` over the declaration model's
//! [`Attribute`](php_ext::decl::Attribute), and `reflect_attributes`,
//! the one walk behind every `getAttributes` (`php_reflection.c`'s
//! `reflect_attributes` and `read_attributes`). php keeps a
//! `zend_attribute*` into the subject's attribute table; the table is
//! part of an immutable unit here, so the reflector keeps a copy of the
//! attribute as written beside the three facts php reads off the
//! reference: the scope its constant expressions are evaluated in, the
//! target bit of the subject it sits on, and whether the subject
//! carries another of the same name.
//!
//! What php validates and this does not: `#[\DelayedTargetValidation]`
//! and the `validation_error` an internal attribute's own validator
//! leaves, neither of which the declaration model carries.

use std::fmt::Write as _;

use php_ext::call::{call_named, eval_const_expr, instantiate, lookup_class};
use php_ext::coerce::{to_callable, to_long, value_name};
use php_ext::decl::Attribute;
use php_ext::raising::{Thrown, arg_value_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use super::{
    at_most_arguments, class_is, class_named, default_value_string, exactly_arguments, is_internal,
    keep_attribute, new_object, set_name, string_argument, string_value, this_attribute,
};

/// `ZEND_ATTRIBUTE_TARGET_*` (`Z/Zend/zend_attributes.h:26-33`), the
/// bits the `Attribute::TARGET_*` constants name, in the order
/// `target_names` prints them.
pub(super) const TARGET_CLASS: i64 = 1;
pub(super) const TARGET_FUNCTION: i64 = 2;
pub(super) const TARGET_METHOD: i64 = 4;
pub(super) const TARGET_PROPERTY: i64 = 8;
pub(super) const TARGET_CLASS_CONST: i64 = 16;
pub(super) const TARGET_PARAMETER: i64 = 32;
pub(super) const TARGET_CONST: i64 = 64;
const TARGET_ALL: i64 = 127;

/// `ZEND_ATTRIBUTE_IS_REPEATABLE`.
const IS_REPEATABLE: i64 = 128;

/// `REFLECTION_ATTRIBUTE_IS_INSTANCEOF`, the one bit
/// `getAttributes` takes in its `$flags` argument.
const IS_INSTANCEOF: i64 = 2;

/// `target_names` (`Z/Zend/zend_attributes.c:423`), what
/// `zend_get_attribute_target_names` joins with `, `.
const TARGET_NAMES: [&str; 7] = [
    "class",
    "function",
    "method",
    "property",
    "class constant",
    "parameter",
    "constant",
];

/// php's `attribute_reference` (`php_reflection.c:196-201`), kept by
/// value: the attribute as written, the class its constant expressions
/// are evaluated in, the target bit of the subject it sits on, and
/// `zend_is_attribute_repeated` answered where the sibling list was
/// still at hand.
#[derive(Clone)]
pub(super) struct AttributeRef {
    pub(super) attribute: Attribute,
    pub(super) scope: Option<ClassId>,
    pub(super) target: i64,
    pub(super) repeated: bool,
}

/// `zend_get_attribute_target_names`: the bits set, in bit order,
/// joined with `, `.
fn target_names(flags: i64) -> String {
    let mut out = String::new();
    for (bit, name) in TARGET_NAMES.iter().enumerate() {
        if flags & (1 << bit) == 0 {
            continue;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(name);
    }
    out
}

/// `reflect_attributes` (`php_reflection.c`) with `read_attributes`
/// under it: the subject's attributes as `ReflectionAttribute`s, held
/// to the optional `$name` and the `IS_INSTANCEOF` bit. `$name` alone
/// matches the attribute's own name, case-insensitively as
/// `zend_string_equals_ci` does; with that bit set it is a class every
/// match must be an instance of, and a name no class has is php's
/// `Error`.
pub(super) fn reflect_attributes(
    req: &mut Request,
    attributes: &[Attribute],
    scope: Option<ClassId>,
    target: i64,
) -> Result<Value, Thrown> {
    at_most_arguments(req, 2)?;
    let wanted = match req.argument(1).map(|given| given.deref()) {
        Some(given) if !matches!(given.as_ref(), ValueRef::Null | ValueRef::Undef) => {
            Some(string_argument(req, 1)?)
        }
        _ => None,
    };
    let flags = match req.argument(2).map(|given| given.deref()) {
        Some(given) => to_long(req, &given)?,
        None => 0,
    };
    if flags & !IS_INSTANCEOF != 0 {
        return Err(arg_value_error(req, 2, "be a valid attribute filter flag"));
    }
    let base = match (&wanted, flags & IS_INSTANCEOF != 0) {
        (Some(name), true) => match lookup_class(req, name)? {
            Some(class) => Some(class),
            None => {
                return Err(throw(
                    req,
                    "Error",
                    &format!("Class \"{}\" not found", String::from_utf8_lossy(name)),
                ));
            }
        },
        _ => None,
    };
    let repeated: Vec<bool> = attributes
        .iter()
        .map(|one| {
            attributes
                .iter()
                .filter(|other| other.name.0.eq_ignore_ascii_case(&one.name.0))
                .count()
                > 1
        })
        .collect();
    let mut out = PhpArray::new(req)?;
    for (at, attribute) in attributes.iter().enumerate() {
        if !matches(req, attribute, wanted.as_deref(), base) {
            continue;
        }
        let kept = AttributeRef {
            attribute: attribute.clone(),
            scope,
            target,
            repeated: repeated.get(at).copied().unwrap_or(false),
        };
        let object = attribute_factory(req, &kept)?;
        out.append(req, object.as_value())?;
    }
    Ok(Value::array(out))
}

/// `read_attributes`'s two filters: the name as written, or the class
/// every match must be an instance of.
fn matches(
    req: &mut Request,
    attribute: &Attribute,
    wanted: Option<&[u8]>,
    base: Option<ClassId>,
) -> bool {
    let written = bare_name(&attribute.name.0);
    if let Some(base) = base {
        return match lookup_class(req, written) {
            Ok(Some(class)) => class_is(req, class, base, 0),
            _ => false,
        };
    }
    match wanted {
        Some(asked) => written.eq_ignore_ascii_case(bare_name(asked)),
        None => true,
    }
}

/// A class name with the leading `\` a written one may carry dropped,
/// as `zend_lookup_class` drops it.
fn bare_name(name: &[u8]) -> &[u8] {
    name.strip_prefix(b"\\").unwrap_or(name)
}

/// `reflection_attribute_factory`: one `ReflectionAttribute` with the
/// reference on its payload and `$name` written, as php writes the
/// property beside `intern->ptr`.
fn attribute_factory(req: &mut Request, kept: &AttributeRef) -> Result<PhpObject, Thrown> {
    let class = class_named(req, b"ReflectionAttribute")?;
    let object = new_object(req, class)?;
    let name = kept.attribute.name.0.to_vec();
    set_name(req, object, &name)?;
    keep_attribute(req, object, kept.clone());
    Ok(object)
}

/// `ReflectionAttribute::__construct()`: private in the stub and
/// reachable through reflection, where php refuses it.
pub(super) fn reflection_attribute_construct(req: &mut Request) -> Result<Value, Thrown> {
    Err(throw(
        req,
        "ReflectionException",
        "Cannot directly instantiate ReflectionAttribute",
    ))
}

/// `ReflectionAttribute::getName()`: `attr->data->name`, the attribute
/// as it was written.
///
/// The attribute the stub's `deprecated` puts on
/// `ReflectionFunction::isDisabled` is the one attribute this engine's
/// declaration model carries, since the compiler reads no `#[...]`
/// where a program writes it yet (#465), so every witness here reads
/// that one.
///
/// ```php
/// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
/// var_dump($a[0]->getName());
/// ```
/// ```expect
/// string(10) "Deprecated"
/// ```
pub(super) fn reflection_attribute_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let kept = this_attribute(req)?;
    let name = kept.attribute.name.0.to_vec();
    string_value(req, &name)
}

/// `ReflectionAttribute::getTarget()`: `attr->target`, the one
/// `Attribute::TARGET_*` bit of the subject the attribute sits on.
///
/// ```php
/// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
/// var_dump($a[0]->getTarget());
/// ```
/// ```expect
/// int(4)
/// ```
pub(super) fn reflection_attribute_get_target(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let kept = this_attribute(req)?;
    Ok(Value::int(kept.target))
}

/// `ReflectionAttribute::isRepeated()`:
/// `zend_is_attribute_repeated`, whether the subject carries another
/// attribute of the same name at the same offset.
///
/// ```php
/// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
/// var_dump($a[0]->isRepeated());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_attribute_is_repeated(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let kept = this_attribute(req)?;
    Ok(Value::bool(kept.repeated))
}

/// `ReflectionAttribute::getArguments()`: each argument's constant
/// expression evaluated in the attribute's scope, the positional ones
/// appended and the named ones at their name, as
/// `zend_get_attribute_value` and `zend_hash_add_new` place them.
///
/// ```php
/// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
/// var_dump($a[0]->getArguments());
/// ```
/// ```expect
/// array(2) {
///   ["since"]=>
///   string(3) "8.0"
///   ["message"]=>
///   string(73) "as ReflectionFunction can no longer be constructed for disabled functions"
/// }
/// ```
pub(super) fn reflection_attribute_get_arguments(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let kept = this_attribute(req)?;
    let mut out = PhpArray::new(req)?;
    for argument in &kept.attribute.arguments {
        let value = eval_const_expr(req, &argument.value, kept.scope)?;
        match &argument.name {
            Some(name) => {
                let key = PhpString::new(req, &name.0)?;
                out.set(req, Key::Str(key), value)?;
            }
            None => out.append(req, value)?,
        }
    }
    Ok(Value::array(out))
}

/// `ReflectionAttribute::__toString()`: `Attribute [ name ]` alone for
/// one with no argument, and the arguments block for one with some,
/// each argument as `format_default_value` writes it, which is the
/// expression as written and not its value.
///
/// ```php
/// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
/// echo $a[0];
/// ```
/// ```expect
/// Attribute [ Deprecated ] {
///   - Arguments [2] {
///     Argument #0 [ since = '8.0' ]
///     Argument #1 [ message = 'as ReflectionFunction can no longer be constructed for disabled functions' ]
///   }
/// }
/// ```
pub(super) fn reflection_attribute_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let kept = this_attribute(req)?;
    let mut out = format!(
        "Attribute [ {} ]",
        String::from_utf8_lossy(&kept.attribute.name.0)
    );
    if kept.attribute.arguments.is_empty() {
        out.push('\n');
        return string_value(req, out.as_bytes());
    }
    out.push_str(" {\n");
    let _ = writeln!(out, "  - Arguments [{}] {{", kept.attribute.arguments.len());
    for (at, argument) in kept.attribute.arguments.iter().enumerate() {
        let _ = write!(out, "    Argument #{at} [ ");
        if let Some(name) = &argument.name {
            out.push_str(&String::from_utf8_lossy(&name.0));
            out.push_str(" = ");
        }
        let written = default_value_string(req, &argument.value);
        out.push_str(&written);
        out.push_str(" ]\n");
    }
    out.push_str("  }\n}\n");
    string_value(req, out.as_bytes())
}

/// `ReflectionAttribute::newInstance()`: the attribute's class
/// instantiated with the arguments, php's four refusals before it —
/// the class no table has, a class carrying no `#[Attribute]` marker,
/// a marker whose flags do not name this target, and a user attribute
/// repeated where its marker did not allow it.
///
/// No witness: the one attribute this engine's declaration model
/// carries is `Deprecated`, whose class php registers in
/// `zend_attributes.c` and this engine declares nowhere (#464), so
/// serving it here answers php's `Attribute class "Deprecated" not
/// found` where php answers the object. The witness lands with the
/// class.
pub(super) fn reflection_attribute_new_instance(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let kept = this_attribute(req)?;
    let written = String::from_utf8_lossy(&kept.attribute.name.0).into_owned();
    let looked_up = bare_name(&kept.attribute.name.0).to_vec();
    let Some(class) = lookup_class(req, &looked_up)? else {
        return Err(throw(
            req,
            "Error",
            &format!("Attribute class \"{written}\" not found"),
        ));
    };
    let flags = marker_flags(req, class, &written)?;
    if kept.target & flags == 0 {
        return Err(throw(
            req,
            "Error",
            &format!(
                "Attribute \"{written}\" cannot target {} (allowed targets: {})",
                target_names(kept.target),
                target_names(flags & TARGET_ALL)
            ),
        ));
    }
    if !is_internal(req, class) && flags & IS_REPEATABLE == 0 && kept.repeated {
        return Err(throw(
            req,
            "Error",
            &format!("Attribute \"{written}\" must not be repeated"),
        ));
    }
    let object = construct(req, class, &kept)?;
    Ok(object.as_value())
}

/// `zend_attribute_attribute_get_flags`: the first argument of the
/// class's own `#[Attribute]` marker, `TARGET_ALL` where the marker
/// takes none, and php's `Error` for a class carrying no marker.
fn marker_flags(req: &mut Request, class: ClassId, written: &str) -> Result<i64, Thrown> {
    let marker = req.class_decl(class).and_then(|decl| {
        decl.attributes
            .iter()
            .find(|one| one.name.0.eq_ignore_ascii_case(b"Attribute"))
            .cloned()
    });
    let Some(marker) = marker else {
        return Err(throw(
            req,
            "Error",
            &format!("Attempting to use non-attribute class \"{written}\" as attribute"),
        ));
    };
    let Some(first) = marker.arguments.first() else {
        return Ok(TARGET_ALL);
    };
    let value = eval_const_expr(req, &first.value, Some(class))?;
    let Some(flags) = value.as_int() else {
        // `zend_zval_value_name(&flags)`
        // (`Z/Zend/zend_attributes.c:54`): the value's name, so `true`
        // stands where the type alone would read `bool`.
        let given = value_name(req, &value);
        return Err(throw(
            req,
            "Error",
            &format!(
                "Attribute::__construct(): Argument #1 ($flags) must be of type int, {given} given"
            ),
        ));
    };
    Ok(flags)
}

/// `zend_get_attribute_object`: the instance, its constructor served
/// with the attribute's arguments, the named ones by their names as
/// `zend_call_function`'s walk of `named_params` places them.
fn construct(req: &mut Request, class: ClassId, kept: &AttributeRef) -> Result<PhpObject, Thrown> {
    let mut positional = Vec::new();
    let mut named: Vec<(PhpString, Value)> = Vec::new();
    for argument in &kept.attribute.arguments {
        let value = eval_const_expr(req, &argument.value, kept.scope)?;
        match &argument.name {
            Some(name) => named.push((PhpString::new(req, &name.0)?, value)),
            None => positional.push(value),
        }
    }
    if named.is_empty() {
        return instantiate(req, class, &positional);
    }
    let object = new_object(req, class)?;
    let pair = constructor_callable(req, object)?;
    let served = to_callable(req, &pair)
        .and_then(|callable| call_named(req, &callable, &positional, &named));
    req.release(pair);
    match served {
        Ok(value) => {
            req.release(value);
            Ok(object)
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `[$object, '__construct']`, the callable `call_named` takes for a
/// constructor served with named arguments; a class with no
/// constructor takes the arguments and serves nothing, so the pair is
/// only made where there is one.
fn constructor_callable(req: &mut Request, object: PhpObject) -> Result<Value, Thrown> {
    let value = object.as_value();
    req.addref(&value);
    let method = string_value(req, b"__construct")?;
    let mut pair = PhpArray::new(req)?;
    pair.set(req, Key::Int(0), value)?;
    pair.set(req, Key::Int(1), method)?;
    Ok(Value::array(pair))
}
