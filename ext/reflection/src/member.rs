//! The accessors `php_reflection.c` answers off one property, one
//! class constant and one global constant, and the one method
//! `Reflection` itself declares. Each reads the declaration the reflector's `$name`,
//! and a member's `$class`, look up again.
//!
//! A property's hooks are a list `PropertyDecl` has no field for, since
//! the compiler refuses one (#427), so `hasHooks`, `getHooks`, `hasHook`
//! and `getHook` answer as php does for a property without hooks.
//!
//! What stands beside `stub::unbuilt` still: `isAbstract`, `isVirtual`
//! and `isFinal`, which are hooks proper (#427), and the four lazy entries;
//! and `setAccessible`, which the stub marks `#[\\Deprecated]` (#412).

use php_ext::call::{constant, eval_const_expr};
use php_ext::decl::{ClassConstDecl, ClassKind, Name, PropertyDecl, Type, Visibility};
use php_ext::raising::{Thrown, deprecated, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, PhpArray, Value};

use super::signature::type_factory;
use super::{
    class_const_string, class_constant_named, class_name_of, class_named, const_string,
    exactly_arguments, namespace_end, property_named, property_string, reflection_class_factory,
    string_value, this_name,
};

/// `Reflection::getModifierNames(int $modifiers)`: the words php
/// prints for the bits, in `add_next_index`'s order, the three
/// visibilities mutually exclusive and so are the two `(set)` ones.
///
/// ```php
/// var_dump(Reflection::getModifierNames(33));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(5) "final"
///   [1]=>
///   string(6) "public"
/// }
/// ```
pub(super) fn reflection_get_modifier_names(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let modifiers = php_ext::coerce::to_long(req, &given)?;
    let mut words: Vec<&str> = Vec::new();
    if modifiers & 64 != 0 {
        words.push("abstract");
    }
    if modifiers & 32 != 0 {
        words.push("final");
    }
    if modifiers & 512 != 0 {
        words.push("virtual");
    }
    match modifiers & 7 {
        1 => words.push("public"),
        4 => words.push("private"),
        2 => words.push("protected"),
        _ => {}
    }
    match modifiers & (2048 | 4096) {
        2048 => words.push("protected(set)"),
        4096 => words.push("private(set)"),
        _ => {}
    }
    if modifiers & 16 != 0 {
        words.push("static");
    }
    if modifiers & (128 | 65536) != 0 {
        words.push("readonly");
    }
    let mut names = PhpArray::new(req)?;
    for word in words {
        let value = string_value(req, word.as_bytes())?;
        names.append(req, value)?;
    }
    Ok(Value::array(names))
}

// ----------------------------------------------- ReflectionProperty

/// php's `property_reference`: the class the reflector names, the
/// property's name, and the declaration with the class that declared
/// it, `None` for php's dynamic property.
type Reflected = (ClassId, Vec<u8>, Option<(ClassId, PropertyDecl)>);

/// `GET_REFLECTION_OBJECT_PTR(ref)` for a property reflector, with the
/// class that declared it.
fn this_declared(req: &mut Request) -> Result<Reflected, Thrown> {
    let (this, name) = this_name(req, "ReflectionProperty")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw(
            req,
            "Error",
            "Typed property ReflectionProperty::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let found = property_named(req, class, &name);
    Ok((class, name, found))
}

/// `ReflectionProperty::hasHooks()`: `ref->prop && ref->prop->hooks`.
/// `PropertyDecl` carries no list of hooks, since the compiler refuses one
/// (#427), so every property here, declared or dynamic, is one without
/// hooks and the answer is php's for it.
///
/// ```php
/// class C { public $p; } $o = new C; $o->d = 1;
/// var_dump((new ReflectionProperty('C', 'p'))->hasHooks(), (new ReflectionProperty($o, 'd'))->hasHooks());
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property C::$d is deprecated in %s on line 2
/// bool(false)
/// bool(false)
/// ```
pub(super) fn reflection_property_has_hooks(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_declared(req)?;
    Ok(Value::bool(false))
}

/// `ReflectionProperty::getHooks()`: the `get` and `set` hooks keyed by
/// kind, and the empty array for a property without hooks, which every
/// property here is ([`reflection_property_has_hooks`]).
///
/// ```php
/// class C { public $p; }
/// var_dump((new ReflectionProperty('C', 'p'))->getHooks());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
pub(super) fn reflection_property_get_hooks(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_declared(req)?;
    Ok(Value::array(PhpArray::new(req)?))
}

/// `Z_PARAM_OBJ_OF_CLASS(type, reflection_property_hook_type_ptr)`, the
/// one argument `hasHook` and `getHook` take, held to its count and to
/// `PropertyHookType`.
fn hook_type_argument(req: &mut Request) -> Result<(), Thrown> {
    exactly_arguments(req, 1)?;
    let given = req.argument(1).unwrap_or_default();
    let ty = Type::Named(Name(b"PropertyHookType".as_slice().into()));
    php_ext::coerce::coerce_param(req, &given, &ty, 1)?;
    Ok(())
}

/// `ReflectionProperty::hasHook(PropertyHookType $type)`: whether the
/// property's hooks include one of that kind, `false` for a property without
/// hooks, which every property here is
/// ([`reflection_property_has_hooks`]).
///
/// ```php
/// class C { public $p; } $r = new ReflectionProperty('C', 'p');
/// var_dump($r->hasHook(PropertyHookType::Get), $r->hasHook(PropertyHookType::Set));
/// try { $r->hasHook('get'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// ReflectionProperty::hasHook(): Argument #1 ($type) must be of type PropertyHookType, string given
/// ```
pub(super) fn reflection_property_has_hook(req: &mut Request) -> Result<Value, Thrown> {
    hook_type_argument(req)?;
    this_declared(req)?;
    Ok(Value::bool(false))
}

/// `ReflectionProperty::getHook(PropertyHookType $type)`: a
/// `ReflectionMethod` over the one of the property's hooks of that
/// kind, `null` for a property without hooks, which every
/// property here is ([`reflection_property_has_hooks`]).
///
/// ```php
/// class C { public $p; }
/// var_dump((new ReflectionProperty('C', 'p'))->getHook(PropertyHookType::Get));
/// ```
/// ```expect
/// NULL
/// ```
pub(super) fn reflection_property_get_hook(req: &mut Request) -> Result<Value, Thrown> {
    hook_type_argument(req)?;
    this_declared(req)?;
    Ok(Value::null())
}

/// `ReflectionProperty::getMangledName()`: `prop->name`, the key the
/// property has in the object's table, as `zend_mangle_property_name`
/// writes it: the name for a public property, `\0*\0name` for a
/// protected one and `\0Class\0name`, the declaring class, for a
/// private one; a dynamic property has no info and answers
/// `ref->unmangled_name`.
///
/// ```php
/// class P { public $a; protected $b; private $c; }
/// $o = new P; $o->d = 1;
/// foreach (['a', 'b', 'c'] as $n) { var_dump((new ReflectionProperty('P', $n))->getMangledName()); }
/// var_dump((new ReflectionProperty($o, 'd'))->getMangledName(), array_keys((array) $o));
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property P::$d is deprecated in %s on line 3
/// string(1) "a"
/// string(4) "%0*%0b"
/// string(4) "%0P%0c"
/// string(1) "d"
/// array(4) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(4) "%0*%0b"
///   [2]=>
///   string(4) "%0P%0c"
///   [3]=>
///   string(1) "d"
/// }
/// ```
pub(super) fn reflection_property_get_mangled_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, name, found) = this_declared(req)?;
    let mangled = match found {
        Some((scope, decl)) => match decl.visibility {
            Visibility::Public => name,
            Visibility::Protected => [b"\0*\0".as_slice(), &name].concat(),
            Visibility::Private => {
                let class = class_name_of(req, scope);
                [b"\0".as_slice(), &class, b"\0", &name].concat()
            }
        },
        None => name,
    };
    string_value(req, &mangled)
}

/// `ReflectionProperty::getName()`: `ref->unmangled_name`.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->getName());
/// ```
/// ```expect
/// string(7) "message"
/// ```
pub(super) fn reflection_property_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, name, _) = this_declared(req)?;
    string_value(req, &name)
}

/// `ReflectionProperty::__toString()`: `_property_string` over the
/// property info, the row `_class_string` prints inside its
/// `Properties` block.
///
/// ```php
/// echo new ReflectionProperty('ReflectionClass', 'name');
/// echo new ReflectionProperty('Exception', 'message');
/// ```
/// ```expect
/// Property [ public string $name ]
/// Property [ protected $message = '' ]
/// ```
///
/// ```php
/// $o = new stdClass; $o->x = 1;
/// echo new ReflectionProperty($o, 'x');
/// ```
/// ```expect
/// Property [ <dynamic> public $x ]
/// ```
pub(super) fn reflection_property_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, name, found) = this_declared(req)?;
    let Some((_, decl)) = found else {
        // `_property_string` with no property info: php's dynamic row,
        // which carries no modifier but `public` and no default.
        let text = format!(
            "Property [ <dynamic> public ${} ]\n",
            String::from_utf8_lossy(&name)
        );
        return string_value(req, text.as_bytes());
    };
    let text = property_string(req, &decl, "");
    string_value(req, text.as_bytes())
}

/// The visibility, set visibility, `final`, `static` and `readonly`
/// bits of a property's `flags`, the numbers the stub's `IS_*`
/// constants name.
fn property_bits(decl: &PropertyDecl) -> i64 {
    let mut bits = match decl.visibility {
        Visibility::Public => 1,
        Visibility::Protected => 2,
        Visibility::Private => 4,
    };
    match decl.set_visibility {
        Some(Visibility::Private) => bits |= 4096,
        Some(_) => bits |= 2048,
        None => {}
    }
    if decl.is_final {
        bits |= 32;
    }
    if !decl.is_instance {
        bits |= 16;
    }
    if decl.is_readonly {
        bits |= 128;
    }
    bits
}

/// `ReflectionProperty::isPrivateSet()`: `ZEND_ACC_PRIVATE_SET`, the
/// set visibility a `private(set)` property carries.
///
/// ```php
/// class Foo { public private(set) int $bar; }
/// var_dump((new ReflectionProperty('Foo', 'bar'))->isPrivateSet());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_property_is_private_set(req: &mut Request) -> Result<Value, Thrown> {
    set_visibility_is(req, Visibility::Private)
}

/// `ReflectionProperty::isProtectedSet()`: `ZEND_ACC_PROTECTED_SET`.
///
/// ```php
/// class Foo { public protected(set) int $bar; }
/// var_dump((new ReflectionProperty('Foo', 'bar'))->isProtectedSet());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_property_is_protected_set(req: &mut Request) -> Result<Value, Thrown> {
    set_visibility_is(req, Visibility::Protected)
}

/// The two `(set)` accessors' body: a dynamic property carries no set
/// visibility, so it is `false` for either.
fn set_visibility_is(req: &mut Request, wanted: Visibility) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(found.is_some_and(|(_, decl)| {
        decl.set_visibility == Some(wanted)
    })))
}

/// `ReflectionProperty::getModifiers()`: `prop_get_flags(ref)` kept to
/// the `PPP | PPP_SET | STATIC | READONLY | ABSTRACT | VIRTUAL |
/// FINAL` mask. The `(set)` visibilities, `abstract`, `virtual` and
/// `final` are bits `PropertyDecl` has no field for, so the answer is
/// php's for a property that carries none of them. A dynamic property
/// has no info, and `prop_get_flags` answers `ZEND_ACC_PUBLIC` for it.
///
/// ```php
/// $o = new stdClass; $o->d = 1;
/// var_dump((new ReflectionProperty('Exception', 'message'))->getModifiers(), (new ReflectionProperty($o, 'd'))->getModifiers());
/// ```
/// ```expect
/// int(2)
/// int(1)
/// ```
pub(super) fn reflection_property_get_modifiers(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::int(
        found.map_or(1, |(_, decl)| property_bits(&decl)),
    ))
}

/// `ReflectionProperty::isPublic()`: `ZEND_ACC_PUBLIC`, which a
/// dynamic property carries too.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->isPublic());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_property_is_public(req: &mut Request) -> Result<Value, Thrown> {
    visibility_is(req, Visibility::Public, true)
}

/// `ReflectionProperty::isProtected()`: `ZEND_ACC_PROTECTED`.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->isProtected());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_property_is_protected(req: &mut Request) -> Result<Value, Thrown> {
    visibility_is(req, Visibility::Protected, false)
}

/// `ReflectionProperty::isPrivate()`: `ZEND_ACC_PRIVATE`.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'string'))->isPrivate());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_property_is_private(req: &mut Request) -> Result<Value, Thrown> {
    visibility_is(req, Visibility::Private, false)
}

fn visibility_is(req: &mut Request, wanted: Visibility, dynamic: bool) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(
        found.map_or(dynamic, |(_, decl)| decl.visibility == wanted),
    ))
}

/// `ReflectionProperty::isStatic()`: `ZEND_ACC_STATIC`.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->isStatic());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_property_is_static(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(
        found.is_some_and(|(_, decl)| !decl.is_instance),
    ))
}

/// `ReflectionProperty::isReadOnly()`: `ZEND_ACC_READONLY`.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->isReadOnly());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_property_is_read_only(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(found.is_some_and(|(_, decl)| decl.is_readonly)))
}

/// `ReflectionProperty::isDefault()`: `_property_check_dynamic`, which
/// is whether the class declares the property at all.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->isDefault());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_property_is_default(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(found.is_some()))
}

/// `ReflectionProperty::isDynamic()`: the other half of
/// `_property_check_dynamic`.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->isDynamic());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_property_is_dynamic(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(found.is_none()))
}

/// `ReflectionProperty::isPromoted()`: `ZEND_ACC_PROMOTED`, the bit on
/// the property a promoted constructor parameter declares; a dynamic
/// property has no info and answers `false`.
///
/// ```php
/// class P { function __construct(public int $x, /** D */ public $z, $y) {} public $w; }
/// var_dump((new ReflectionProperty('P', 'x'))->isPromoted(), (new ReflectionProperty('P', 'w'))->isPromoted(), (new ReflectionProperty('Exception', 'message'))->isPromoted());
/// var_dump((new ReflectionProperty('P', 'z'))->getDocComment());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// string(8) "/** D */"
/// ```
pub(super) fn reflection_property_is_promoted(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(found.is_some_and(|(_, decl)| decl.is_promoted)))
}

/// `ReflectionProperty::getDeclaringClass()`:
/// `zend_reflection_class_factory` over `prop->ce`, and the class the
/// reflector names for a dynamic property.
///
/// ```php
/// var_dump((new ReflectionProperty('JsonException', 'message'))->getDeclaringClass()->getName());
/// ```
/// ```expect
/// string(9) "Exception"
/// ```
pub(super) fn reflection_property_get_declaring_class(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (class, _, found) = this_declared(req)?;
    let scope = found.map_or(class, |(scope, _)| scope);
    let object = reflection_class_factory(req, scope)?;
    Ok(object.as_value())
}

/// `ReflectionProperty::hasType()`: `ZEND_TYPE_IS_SET(prop->type)`.
///
/// ```php
/// var_dump((new ReflectionProperty('ReflectionClass', 'name'))->hasType());
/// var_dump((new ReflectionProperty('Exception', 'message'))->hasType());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_property_has_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(
        found.is_some_and(|(_, decl)| decl.ty.is_some()),
    ))
}

/// `ReflectionProperty::getType()`: `reflection_type_factory` over
/// `prop->type` with `legacy_behavior` set, and `null` where the
/// property is untyped or dynamic.
///
/// ```php
/// echo (new ReflectionProperty('ReflectionClass', 'name'))->getType(), "\n";
/// ```
/// ```expect
/// string
/// ```
pub(super) fn reflection_property_get_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    let Some(ty) = found.and_then(|(_, decl)| decl.ty) else {
        return Ok(Value::null());
    };
    let object = type_factory(req, &ty, true)?;
    Ok(object.as_value())
}

/// `ReflectionProperty::getSettableType()`: the type a write must
/// satisfy, which for a property with no hooks is `getType()`'s.
///
/// ```php
/// echo (new ReflectionProperty('ReflectionClass', 'name'))->getSettableType(), "\n";
/// ```
/// ```expect
/// string
/// ```
pub(super) fn reflection_property_get_settable_type(req: &mut Request) -> Result<Value, Thrown> {
    reflection_property_get_type(req)
}

/// `ReflectionProperty::hasDefaultValue()`: whether
/// `property_get_default` gives a value that is not `UNDEF`; an
/// untyped property with nothing written has php's implicit `null` and
/// a typed one has none.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->hasDefaultValue());
/// var_dump((new ReflectionProperty('ReflectionClass', 'name'))->hasDefaultValue());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_property_has_default_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    Ok(Value::bool(found.is_some_and(|(_, decl)| {
        decl.default.is_some() || decl.ty.is_none()
    })))
}

/// `property_get_default`'s three answers: a written default, the
/// implicit `null` of an untyped property, and nothing, which a typed
/// property without one and a dynamic property both give.
enum Default {
    Written(php_ext::decl::ConstExpr, ClassId),
    ImplicitNull,
    Absent,
}

/// `ReflectionProperty::getDefaultValue()`: the default expression
/// evaluated in the declaring class, php's implicit `null` for an
/// untyped property with nothing written, and php's own
/// `E_DEPRECATED` and `null` where there is no default at all, a
/// dynamic property among them, which has no info to hold one.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->getDefaultValue());
/// ```
/// ```expect
/// string(0) ""
/// ```
///
/// ```php
/// var_dump((new ReflectionProperty('ReflectionClass', 'name'))->getDefaultValue());
/// ```
/// ```expect
/// Deprecated: ReflectionProperty::getDefaultValue() for a property without a default value is deprecated, use ReflectionProperty::hasDefaultValue() to check if the default value exists in %s on line 2
/// NULL
/// ```
///
/// ```php
/// $o = new stdClass; $o->d = 1;
/// var_dump((new ReflectionProperty($o, 'd'))->getDefaultValue());
/// ```
/// ```expect
/// Deprecated: ReflectionProperty::getDefaultValue() for a property without a default value is deprecated, use ReflectionProperty::hasDefaultValue() to check if the default value exists in %s on line 3
/// NULL
/// ```
pub(super) fn reflection_property_get_default_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, found) = this_declared(req)?;
    let default = match found {
        Some((scope, decl)) => match (decl.default, decl.ty) {
            (Some(expr), _) => Default::Written(expr, scope),
            (None, None) => Default::ImplicitNull,
            (None, Some(_)) => Default::Absent,
        },
        None => Default::Absent,
    };
    match default {
        Default::Written(expr, scope) => eval_const_expr(req, &expr, Some(scope)),
        Default::ImplicitNull => Ok(Value::null()),
        Default::Absent => {
            deprecated(
                req,
                "ReflectionProperty::getDefaultValue() for a property without a default value is deprecated, use ReflectionProperty::hasDefaultValue() to check if the default value exists",
            );
            Ok(Value::null())
        }
    }
}

/// `ReflectionProperty::getAttributes(?string $name = null, int $flags
/// = 0)`: `reflect_attributes` over `prop->attributes` with
/// `ZEND_ATTRIBUTE_TARGET_PROPERTY`.
///
/// ```php
/// var_dump((new ReflectionProperty('Exception', 'message'))->getAttributes());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
pub(super) fn reflection_property_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    let (class, _, found) = this_declared(req)?;
    let scope = found.as_ref().map_or(class, |(scope, _)| *scope);
    let attributes = found.map_or_else(Vec::new, |(_, decl)| decl.attributes.clone());
    super::attr::reflect_attributes(req, &attributes, Some(scope), super::attr::TARGET_PROPERTY)
}

// ------------------------------------------ ReflectionClassConstant

/// `GET_REFLECTION_OBJECT_PTR(ref)` for a class-constant reflector,
/// with the class that declared it.
fn this_constant(req: &mut Request) -> Result<(Vec<u8>, ClassId, ClassConstDecl), Thrown> {
    let (this, name) = this_name(req, "ReflectionClassConstant")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw(
            req,
            "Error",
            "Typed property ReflectionClassConstant::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let Some((scope, decl)) = class_constant_named(req, class, &name) else {
        let named = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Constant {}::{} does not exist",
                String::from_utf8_lossy(&named),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    Ok((name, scope, decl))
}

/// `ReflectionClassConstant::getName()`: the constant's name.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getName());
/// ```
/// ```expect
/// string(13) "IS_DEPRECATED"
/// ```
pub(super) fn reflection_class_constant_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (name, _, _) = this_constant(req)?;
    string_value(req, &name)
}

/// `ReflectionClassConstant::getValue()`: the constant expression
/// evaluated in its declaring class, `zend_update_class_constant`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getValue());
/// ```
/// ```expect
/// int(2048)
/// ```
pub(super) fn reflection_class_constant_get_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, scope, decl) = this_constant(req)?;
    super::constant_value(req, &decl, scope)
}

/// `ReflectionClassConstant::__toString()`: `_class_const_string`, the
/// row `_class_string` prints inside its `Constants` block.
///
/// ```php
/// echo new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED');
/// echo new ReflectionClassConstant('PropertyHookType', 'Get');
/// ```
/// ```expect
/// Constant [ public int IS_DEPRECATED ] { 2048 }
/// Constant [ public PropertyHookType Get ] { Object }
/// ```
pub(super) fn reflection_class_constant_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, scope, decl) = this_constant(req)?;
    let held = class_name_of(req, scope);
    let enum_of = req
        .class_decl(scope)
        .is_some_and(|decl| decl.kind == ClassKind::Enum)
        .then_some(held.as_slice());
    let text = class_const_string(req, &decl, scope, enum_of, "")?;
    string_value(req, text.as_bytes())
}

/// `ReflectionClassConstant::getModifiers()`: the visibility and
/// `ZEND_ACC_FINAL` bits of `ZEND_CLASS_CONST_FLAGS`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getModifiers());
/// ```
/// ```expect
/// int(1)
/// ```
pub(super) fn reflection_class_constant_get_modifiers(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, decl) = this_constant(req)?;
    let mut bits = match decl.visibility {
        Visibility::Public => 1,
        Visibility::Protected => 2,
        Visibility::Private => 4,
    };
    if decl.is_final {
        bits |= 32;
    }
    Ok(Value::int(bits))
}

/// `ReflectionClassConstant::isPublic()`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isPublic());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_class_constant_is_public(req: &mut Request) -> Result<Value, Thrown> {
    constant_visibility_is(req, Visibility::Public)
}

/// `ReflectionClassConstant::isProtected()`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isProtected());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_constant_is_protected(req: &mut Request) -> Result<Value, Thrown> {
    constant_visibility_is(req, Visibility::Protected)
}

/// `ReflectionClassConstant::isPrivate()`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isPrivate());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_constant_is_private(req: &mut Request) -> Result<Value, Thrown> {
    constant_visibility_is(req, Visibility::Private)
}

fn constant_visibility_is(req: &mut Request, wanted: Visibility) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, decl) = this_constant(req)?;
    Ok(Value::bool(decl.visibility == wanted))
}

/// `ReflectionClassConstant::isFinal()`: `ZEND_ACC_FINAL`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isFinal());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_constant_is_final(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, decl) = this_constant(req)?;
    Ok(Value::bool(decl.is_final))
}

/// `ReflectionClassConstant::isEnumCase()`: `ZEND_CLASS_CONST_IS_CASE`,
/// which every constant of an enum carries and no other does.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isEnumCase());
/// var_dump((new ReflectionClassConstant('PropertyHookType', 'Get'))->isEnumCase());
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// ```
pub(super) fn reflection_class_constant_is_enum_case(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, decl) = this_constant(req)?;
    Ok(Value::bool(decl.is_case))
}

/// `ReflectionClassConstant::isDeprecated()`:
/// `ZEND_ACC_DEPRECATED`, which the `#[\Deprecated]` attribute sets,
/// read from the attribute as `_function_string` reads a function's.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->isDeprecated());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_constant_is_deprecated(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, decl) = this_constant(req)?;
    Ok(Value::bool(
        decl.attributes
            .iter()
            .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated")),
    ))
}

/// `ReflectionClassConstant::hasType()`: whether the constant was
/// declared with a type, which php 8.3 allows.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->hasType());
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_class_constant_has_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, decl) = this_constant(req)?;
    Ok(Value::bool(decl.ty.is_some()))
}

/// `ReflectionClassConstant::getType()`: `reflection_type_factory` over
/// the declared type, and `null` where the constant has none.
///
/// ```php
/// echo (new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getType(), "\n";
/// ```
/// ```expect
/// int
/// ```
pub(super) fn reflection_class_constant_get_type(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, decl) = this_constant(req)?;
    let Some(ty) = decl.ty else {
        return Ok(Value::null());
    };
    let object = type_factory(req, &ty, true)?;
    Ok(object.as_value())
}

/// `ReflectionClassConstant::getDeclaringClass()`:
/// `zend_reflection_class_factory` over `constant->ce`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getDeclaringClass()->getName());
/// ```
/// ```expect
/// string(18) "ReflectionFunction"
/// ```
pub(super) fn reflection_class_constant_get_declaring_class(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, scope, _) = this_constant(req)?;
    let object = reflection_class_factory(req, scope)?;
    Ok(object.as_value())
}

/// `ReflectionClassConstant::getAttributes(?string $name = null, int
/// $flags = 0)`: `reflect_attributes` over the constant's attributes
/// with `ZEND_ATTRIBUTE_TARGET_CLASS_CONST`.
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionFunction', 'IS_DEPRECATED'))->getAttributes());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
pub(super) fn reflection_class_constant_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    let (_, scope, decl) = this_constant(req)?;
    let attributes = decl.attributes.clone();
    super::attr::reflect_attributes(
        req,
        &attributes,
        Some(scope),
        super::attr::TARGET_CLASS_CONST,
    )
}

// ----------------------------------------------- ReflectionConstant

/// The name a `ReflectionConstant` reflects, with the namespace part
/// lowered as its constructor lowers it for the lookup.
fn this_global_constant(req: &mut Request) -> Result<Vec<u8>, Thrown> {
    let (_, name) = this_name(req, "ReflectionConstant")?;
    Ok(name.strip_prefix(b"\\").unwrap_or(&name).to_vec())
}

/// `ReflectionConstant::getName()`: `const_->name`.
///
/// ```php
/// var_dump((new ReflectionConstant('PHP_EOL'))->getName());
/// ```
/// ```expect
/// string(7) "PHP_EOL"
/// ```
pub(super) fn reflection_constant_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let name = this_global_constant(req)?;
    string_value(req, &name)
}

/// `ReflectionConstant::getFileName()`: `const_->filename`, the file
/// executing when `define` or `const` wrote the constant, and `false`
/// for one an extension or the engine registered.
///
/// ```php
/// define('D', 1);
/// var_dump(basename((new ReflectionConstant('D'))->getFileName()), (new ReflectionConstant('PHP_VERSION'))->getFileName(), (new ReflectionConstant('STDIN'))->getFileName());
/// ```
/// ```expect
/// string(11) "witness.php"
/// bool(false)
/// bool(false)
/// ```
pub(super) fn reflection_constant_get_file_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let name = this_global_constant(req)?;
    match req.constant_file(&name).map(<[u8]>::to_vec) {
        Some(file) => string_value(req, &file),
        None => Ok(Value::bool(false)),
    }
}

/// `ReflectionConstant::getShortName()`: the name after the last `\`.
///
/// ```php
/// var_dump((new ReflectionConstant('PHP_EOL'))->getShortName());
/// ```
/// ```expect
/// string(7) "PHP_EOL"
/// ```
pub(super) fn reflection_constant_get_short_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let name = this_global_constant(req)?;
    let start = namespace_end(&name).map_or(0, |end| end + 1);
    let short = name[start..].to_vec();
    string_value(req, &short)
}

/// `ReflectionConstant::getNamespaceName()`: the name before the last
/// `\`, and the empty string where there is none.
///
/// ```php
/// var_dump((new ReflectionConstant('PHP_EOL'))->getNamespaceName());
/// ```
/// ```expect
/// string(0) ""
/// ```
pub(super) fn reflection_constant_get_namespace_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let name = this_global_constant(req)?;
    let space = namespace_end(&name).map_or_else(Vec::new, |end| name[..end].to_vec());
    string_value(req, &space)
}

/// `ReflectionConstant::getValue()`: `const_->value`.
///
/// ```php
/// var_dump((new ReflectionConstant('PHP_INT_SIZE'))->getValue());
/// ```
/// ```expect
/// int(8)
/// ```
pub(super) fn reflection_constant_get_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let name = this_global_constant(req)?;
    Ok(constant(req, &name)?.unwrap_or_default())
}

/// `ReflectionConstant::isDeprecated()`: `CONST_DEPRECATED`, which is
/// the `#[\Deprecated]` attribute on the declaration, and `E_STRICT`
/// is the one constant carrying it.
///
/// ```php
/// var_dump((new ReflectionConstant('E_STRICT'))->isDeprecated());
/// var_dump((new ReflectionConstant('PHP_EOL'))->isDeprecated());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_constant_is_deprecated(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let name = this_global_constant(req)?;
    Ok(Value::bool(super::constant_is_deprecated(req, &name)))
}

/// `ReflectionConstant::getExtensionName()`: the extension that
/// declared the constant, and `false` for one a program defined.
///
/// ```php
/// var_dump((new ReflectionConstant('JSON_ERROR_NONE'))->getExtensionName());
/// ```
/// ```expect
/// string(4) "json"
/// ```
pub(super) fn reflection_constant_get_extension_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let Some(name) = constant_extension_name(req)? else {
        return Ok(Value::bool(false));
    };
    string_value(req, name.as_bytes())
}

/// `ReflectionConstant::getExtension()`: the same name as a
/// `ReflectionExtension`, and `null` where the name is `false`.
///
/// ```php
/// var_dump((new ReflectionConstant('JSON_ERROR_NONE'))->getExtension());
/// ```
/// ```expect
/// object(ReflectionExtension)#2 (1) {
///   ["name"]=>
///   string(4) "json"
/// }
/// ```
pub(super) fn reflection_constant_get_extension(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let Some(name) = constant_extension_name(req)? else {
        return Ok(Value::null());
    };
    let class = class_named(req, b"ReflectionExtension")?;
    let object = super::new_object(req, class)?;
    super::set_name(req, object, name.as_bytes())?;
    Ok(object.as_value())
}

/// `reflection_constant_find_ext`: the extension whose constants
/// table the name is in, php's walk of `EG(zend_constants)` by the
/// number that says which extension declared it.
fn constant_extension_name(req: &mut Request) -> Result<Option<String>, Thrown> {
    let name = this_global_constant(req)?;
    for named in req.loaded_extensions() {
        let Some(extension) = req.extension_named(named.as_bytes()) else {
            continue;
        };
        if req
            .extension_constants(extension)
            .iter()
            .any(|declared| declared.0.as_ref() == name.as_slice())
        {
            return Ok(req.extension_name(extension).map(str::to_owned));
        }
    }
    Ok(None)
}

/// `ReflectionConstant::__toString()`: `_const_string` over the name
/// and the value, the row `_extension_string` prints inside its
/// `Constants` block.
///
/// ```php
/// echo new ReflectionConstant('PHP_INT_SIZE');
/// echo new ReflectionConstant('E_STRICT');
/// ```
/// ```expect
/// Constant [ <persistent> int PHP_INT_SIZE ] { 8 }
/// Constant [ <persistent, deprecated> int E_STRICT ] { 2048 }
/// ```
///
/// ```php
/// define('DEMO', 1);
/// echo new ReflectionConstant('DEMO');
/// ```
/// ```expect
/// Constant [ int DEMO ] { 1 }
/// ```
pub(super) fn reflection_constant_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let name = this_global_constant(req)?;
    let text = const_string(req, &name, "");
    string_value(req, text.as_bytes())
}

/// `ReflectionClass::isAbstract()`: `ZEND_ACC_EXPLICIT_ABSTRACT_CLASS`
/// or `ZEND_ACC_IMPLICIT_ABSTRACT_CLASS`, so an interface and a class
/// with an abstract method both read `true`.
///
/// ```php
/// var_dump((new ReflectionClass('ReflectionFunctionAbstract'))->isAbstract());
/// var_dump((new ReflectionClass('Exception'))->isAbstract());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_abstract(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = super::this_class(req)?;
    Ok(Value::bool(super::class::class_is_abstract(req, class)))
}
