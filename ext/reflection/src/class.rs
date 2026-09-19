//! The accessors `php_reflection.c` answers off one
//! `zend_class_entry`: the modifier bits, the parent, the interfaces
//! and traits, the function, property and constant tables, and the
//! three ways php makes an instance. Each reads the linked tables the
//! reflector's `$name` looks up again, the same walk
//! `_class_string` renders (`linked_methods`, `linked_interfaces`,
//! `linked_constants`, `linked_properties`).
//!
//! What stands beside `stub::unbuilt` still: `getTraitAliases`, whose
//! `use ... as ...` list the declaration model does not carry; and the
//! eight lazy-object entries, each of which takes
//! a `Closure` (#415).

use php_ext::call::{eval_const_expr, instantiate, read_class_prop_silent, write_static_prop};
use php_ext::coerce::value_name;
use php_ext::decl::{ClassKind, MethodDecl, Visibility};
use php_ext::raising::{Thrown, arg_count_error, arg_type_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use super::{
    at_most_arguments, class_chain, class_constant_named, class_is, class_name_of, class_named,
    exactly_arguments, is_constructor, linked_constants, linked_interfaces, linked_methods,
    linked_properties, lookup_class, new_object, one_string_argument, property_named,
    reflection_class_factory, set_name, set_string_prop, string_argument, string_value, this_class,
};

/// `ZEND_ACC_INTERFACE`, `ZEND_ACC_TRAIT` and `ZEND_ACC_ENUM`.
fn kind_of(req: &mut Request, class: ClassId) -> ClassKind {
    req.class_decl(class).map_or(ClassKind::Class, |d| d.kind)
}

/// `ZEND_ACC_EXPLICIT_ABSTRACT_CLASS | ZEND_ACC_IMPLICIT_ABSTRACT_CLASS`:
/// a class declared `abstract`, and one with an abstract method php
/// marks implicitly abstract at link time.
pub(super) fn class_is_abstract(req: &mut Request, class: ClassId) -> bool {
    let explicit = req
        .class_decl(class)
        .is_some_and(|decl| decl.flags.is_abstract);
    explicit
        || linked_methods(req, class)
            .iter()
            .any(|m| m.decl.is_abstract)
}

/// php's `ce_flags & (INTERFACE | TRAIT | ABSTRACT | ENUM)` test, which
/// `isInstantiable` and `isCloneable` both open with.
fn cannot_instantiate(req: &mut Request, class: ClassId) -> bool {
    !matches!(kind_of(req, class), ClassKind::Class) || class_is_abstract(req, class)
}

/// `ce->constructor`: the linked `__construct`, with the scope that
/// declared it.
fn constructor_of(req: &mut Request, class: ClassId) -> Option<(MethodDecl, ClassId)> {
    linked_methods(req, class)
        .into_iter()
        .find(|m| is_constructor(&m.decl))
        .map(|m| (m.decl, m.scope))
}

/// `reflection_method_factory`: a `ReflectionMethod` with `$name` the
/// method's and `$class` the scope that declared it.
fn method_factory(req: &mut Request, name: &[u8], scope: ClassId) -> Result<PhpObject, Thrown> {
    let class = class_named(req, b"ReflectionMethod")?;
    let object = new_object(req, class)?;
    let scope = class_name_of(req, scope);
    set_name(req, object, name)?;
    set_string_prop(req, object, b"class", &scope)?;
    Ok(object)
}

/// `reflection_property_factory`: a `ReflectionProperty` with `$name`
/// the property's and `$class` the class that declared it.
fn property_factory(req: &mut Request, name: &[u8], scope: ClassId) -> Result<PhpObject, Thrown> {
    let class = class_named(req, b"ReflectionProperty")?;
    let object = new_object(req, class)?;
    let scope = class_name_of(req, scope);
    set_name(req, object, name)?;
    set_string_prop(req, object, b"class", &scope)?;
    Ok(object)
}

/// `reflection_class_constant_factory`: a `ReflectionClassConstant`
/// with `$name` the constant's and `$class` the class that declared it.
fn class_constant_factory(
    req: &mut Request,
    name: &[u8],
    scope: ClassId,
) -> Result<PhpObject, Thrown> {
    let class = class_named(req, b"ReflectionClassConstant")?;
    let object = new_object(req, class)?;
    let scope = class_name_of(req, scope);
    set_name(req, object, name)?;
    set_string_prop(req, object, b"class", &scope)?;
    Ok(object)
}

/// `ReflectionClass::isInterface()`: `ZEND_ACC_INTERFACE`.
///
/// ```php
/// var_dump((new ReflectionClass('JsonSerializable'))->isInterface());
/// var_dump((new ReflectionClass('Exception'))->isInterface());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_interface(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(Value::bool(kind_of(req, class) == ClassKind::Interface))
}

/// `ReflectionClass::isTrait()`: `ZEND_ACC_TRAIT`.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->isTrait());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_is_trait(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(Value::bool(kind_of(req, class) == ClassKind::Trait))
}

/// `ReflectionClass::isEnum()`: `ZEND_ACC_ENUM`.
///
/// ```php
/// var_dump((new ReflectionClass('PropertyHookType'))->isEnum());
/// var_dump((new ReflectionClass('Exception'))->isEnum());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_enum(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(Value::bool(kind_of(req, class) == ClassKind::Enum))
}

/// `ReflectionClass::isFinal()`: `ZEND_ACC_FINAL`.
///
/// ```php
/// var_dump((new ReflectionClass('ReflectionGenerator'))->isFinal());
/// var_dump((new ReflectionClass('Exception'))->isFinal());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_final(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let final_ = req
        .class_decl(class)
        .is_some_and(|decl| decl.flags.is_final);
    Ok(Value::bool(final_))
}

/// `ReflectionClass::isReadOnly()`: `ZEND_ACC_READONLY_CLASS`.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->isReadOnly());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_is_read_only(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let readonly = req
        .class_decl(class)
        .is_some_and(|decl| decl.flags.is_readonly);
    Ok(Value::bool(readonly))
}

/// `ReflectionClass::isAnonymous()`: `ZEND_ACC_ANON_CLASS`. A class
/// the compiler has read is always named here, so the answer is php's
/// for every class that is not anonymous.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->isAnonymous());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_is_anonymous(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_class(req)?;
    Ok(Value::bool(false))
}

/// `ReflectionClass::getModifiers()`: the `ZEND_ACC_FINAL`,
/// `ZEND_ACC_EXPLICIT_ABSTRACT_CLASS` and `ZEND_ACC_READONLY_CLASS`
/// bits of `ce_flags`, the numbers the stub's `IS_*` constants name.
///
/// ```php
/// var_dump((new ReflectionClass('ReflectionGenerator'))->getModifiers());
/// var_dump((new ReflectionClass('ReflectionFunctionAbstract'))->getModifiers());
/// ```
/// ```expect
/// int(32)
/// int(64)
/// ```
pub(super) fn reflection_class_get_modifiers(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let Some(flags) = req.class_decl(class).map(|decl| decl.flags) else {
        return Ok(Value::int(0));
    };
    let mut bits = 0;
    if flags.is_final {
        bits |= 32;
    }
    if flags.is_abstract {
        bits |= 64;
    }
    if flags.is_readonly {
        bits |= 65536;
    }
    Ok(Value::int(bits))
}

/// `ReflectionClass::isInstantiable()`: an interface, a trait, an enum
/// or an abstract class is not; anything else is, unless its
/// constructor is not public.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->isInstantiable());
/// var_dump((new ReflectionClass('JsonSerializable'))->isInstantiable());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_instantiable(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    if cannot_instantiate(req, class) {
        return Ok(Value::bool(false));
    }
    let public =
        constructor_of(req, class).is_none_or(|(decl, _)| decl.visibility == Visibility::Public);
    Ok(Value::bool(public))
}

/// `ReflectionClass::isCloneable()`: an interface, a trait, an enum or
/// an abstract class is not; a class with a `__clone` is cloneable
/// while that method is public; anything else is, since every object
/// here has php's `clone_obj`.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->isCloneable());
/// var_dump((new ReflectionClass('ReflectionClass'))->isCloneable());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_cloneable(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    if cannot_instantiate(req, class) {
        return Ok(Value::bool(false));
    }
    let clone = linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.eq_ignore_ascii_case(b"__clone"));
    Ok(Value::bool(
        clone.is_none_or(|m| m.decl.visibility == Visibility::Public),
    ))
}

/// `ReflectionClass::isIterable()` and its old spelling
/// `isIterateable()`: whether the class implements `Traversable` and
/// can be instantiated.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->isIterable());
/// var_dump((new ReflectionClass('Exception'))->isIterateable());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_iterable(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    if cannot_instantiate(req, class) {
        return Ok(Value::bool(false));
    }
    let Some(traversable) = lookup_class(req, b"Traversable")? else {
        return Ok(Value::bool(false));
    };
    Ok(Value::bool(class_is(req, class, traversable, 0)))
}

/// `ReflectionClass::getDocComment()`: `ce->doc_comment`, and `false`
/// where there is none.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->getDocComment());
/// ```
/// ```expect
/// bool(false)
/// ```
pub(super) fn reflection_class_get_doc_comment(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let Some(doc) = req.class_decl(class).and_then(|d| d.doc_comment.clone()) else {
        return Ok(Value::bool(false));
    };
    string_value(req, &doc)
}

/// `ReflectionClass::getParentClass()`:
/// `zend_reflection_class_factory` over `ce->parent`, and `false`
/// where the class extends nothing.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->getParentClass()->getName());
/// var_dump((new ReflectionClass('Exception'))->getParentClass());
/// ```
/// ```expect
/// string(9) "Exception"
/// bool(false)
/// ```
pub(super) fn reflection_class_get_parent_class(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let Some(parent) = class_chain(req, class).get(1).copied() else {
        return Ok(Value::bool(false));
    };
    let object = reflection_class_factory(req, parent)?;
    Ok(object.as_value())
}

/// `ReflectionClass::isInstance(object $object)`:
/// `instanceof_function` over the object's class.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->isInstance(new JsonException));
/// var_dump((new ReflectionClass('JsonException'))->isInstance(new Exception));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_class_is_instance(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let ValueRef::Object(object) = given.as_ref() else {
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type object, {} given", value_name(req, &given)),
        ));
    };
    let of = object.class();
    let class = this_class(req)?;
    Ok(Value::bool(class_is(req, of, class, 0)))
}

/// `ReflectionClass::implementsInterface(string|ReflectionClass
/// $interface)`: `instanceof_function` again, with php's `ReflectionException` for
/// a name that is not an interface.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->implementsInterface('Throwable'));
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// ```php
/// (new ReflectionClass('JsonException'))->implementsInterface('Exception');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Exception is not an interface in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->implementsInterface('Exception')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_class_implements_interface(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let interface = reflector_argument(req, 1, "Interface")?;
    if kind_of(req, interface) != ClassKind::Interface {
        let name = class_name_of(req, interface);
        return Err(throw(
            req,
            "ReflectionException",
            &format!("{} is not an interface", String::from_utf8_lossy(&name)),
        ));
    }
    let class = this_class(req)?;
    Ok(Value::bool(class_is(req, class, interface, 0)))
}

/// `Z_PARAM_OBJ_OF_CLASS_OR_STR(reflection_class_ptr)`: a
/// `ReflectionClass` is the class it names, a string is
/// `zend_lookup_class`, and a miss is php's `ReflectionException`,
/// `Interface` or `Class` naming which it wanted.
fn reflector_argument(req: &mut Request, position: u32, what: &str) -> Result<ClassId, Thrown> {
    if let Some(given) = req.argument(position)
        && let ValueRef::Object(object) = given.deref().as_ref()
    {
        let object = *object;
        let Some(name) = object
            .prop(req, b"name")
            .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
        else {
            return Err(throw(
                req,
                "Error",
                "Internal error: Failed to retrieve the argument's reflection object",
            ));
        };
        return class_named(req, &name);
    }
    let name = string_argument(req, position)?;
    let bare = name.strip_prefix(b"\\").unwrap_or(&name);
    match lookup_class(req, bare)? {
        Some(class) => Ok(class),
        None => Err(throw(
            req,
            "ReflectionException",
            &format!(
                "{what} \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        )),
    }
}

/// `ReflectionClass::getInterfaceNames()`: `ce->interfaces`, the
/// linked list in php's order.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->getInterfaceNames());
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(9) "Throwable"
///   [1]=>
///   string(10) "Stringable"
/// }
/// ```
pub(super) fn reflection_class_get_interface_names(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let mut names = PhpArray::new(req)?;
    for interface in linked_interfaces(req, class) {
        let name = class_name_of(req, interface);
        let value = string_value(req, &name)?;
        names.append(req, value)?;
    }
    Ok(Value::array(names))
}

/// `ReflectionClass::getInterfaces()`: the same list, each name a
/// `ReflectionClass`, keyed by the interface's name.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->getInterfaces());
/// ```
/// ```expect
/// array(2) {
///   ["Throwable"]=>
///   object(ReflectionClass)#2 (1) {
///     ["name"]=>
///     string(9) "Throwable"
///   }
///   ["Stringable"]=>
///   object(ReflectionClass)#3 (1) {
///     ["name"]=>
///     string(10) "Stringable"
///   }
/// }
/// ```
pub(super) fn reflection_class_get_interfaces(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let mut interfaces = PhpArray::new(req)?;
    for interface in linked_interfaces(req, class) {
        let name = class_name_of(req, interface);
        let object = reflection_class_factory(req, interface)?;
        let key = PhpString::new(req, &name)?;
        interfaces.set(req, Key::Str(key), object.as_value())?;
    }
    Ok(Value::array(interfaces))
}

/// `ReflectionClass::getTraitNames()`: `ce->trait_names`, the traits
/// the class itself uses.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->getTraitNames());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
pub(super) fn reflection_class_get_trait_names(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let traits = req
        .class_decl(class)
        .map(|decl| decl.traits.clone())
        .unwrap_or_default();
    let mut names = PhpArray::new(req)?;
    for trait_ in traits {
        let found = lookup_class(req, &trait_.0)?;
        let name = found.map_or_else(|| trait_.0.to_vec(), |found| class_name_of(req, found));
        let value = string_value(req, &name)?;
        names.append(req, value)?;
    }
    Ok(Value::array(names))
}

/// `ReflectionClass::getTraits()`: the same list, each name a
/// `ReflectionClass`, keyed by the trait's name.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->getTraits());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
pub(super) fn reflection_class_get_traits(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let traits = req
        .class_decl(class)
        .map(|decl| decl.traits.clone())
        .unwrap_or_default();
    let mut found = PhpArray::new(req)?;
    for trait_ in traits {
        let Some(at) = lookup_class(req, &trait_.0)? else {
            continue;
        };
        let name = class_name_of(req, at);
        let object = reflection_class_factory(req, at)?;
        let key = PhpString::new(req, &name)?;
        found.set(req, Key::Str(key), object.as_value())?;
    }
    Ok(Value::array(found))
}

// -------------------------------------------------------- the tables

/// `ReflectionClass::hasMethod(string $name)`:
/// `zend_hash_exists(&ce->function_table, lc_name)`.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->hasMethod('getCode'));
/// var_dump((new ReflectionClass('Exception'))->hasMethod('nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub(super) fn reflection_class_has_method(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    let lowered = name.to_ascii_lowercase();
    let found = linked_methods(req, class)
        .into_iter()
        .any(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered);
    Ok(Value::bool(found))
}

/// `ReflectionClass::getMethod(string $name)`:
/// `reflection_method_factory` over the function table's entry, and
/// php's `ReflectionException` for a name it has not.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->getMethod('getCode')->getName());
/// ```
/// ```expect
/// string(7) "getCode"
/// ```
///
/// ```php
/// (new ReflectionClass('Exception'))->getMethod('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Method Exception::nope() does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getMethod('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_class_get_method(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    let lowered = name.to_ascii_lowercase();
    let Some(found) = linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
    else {
        let named = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Method {}::{}() does not exist",
                String::from_utf8_lossy(&named),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let object = method_factory(req, &found.decl.function.name.0, found.scope)?;
    Ok(object.as_value())
}

/// `ReflectionClass::getConstructor()`:
/// `reflection_method_factory` over `ce->constructor`, and `null`
/// where the class has none.
///
/// ```php
/// var_dump((new ReflectionClass('Exception'))->getConstructor()->getName());
/// var_dump((new ReflectionClass('JsonSerializable'))->getConstructor());
/// ```
/// ```expect
/// string(11) "__construct"
/// NULL
/// ```
pub(super) fn reflection_class_get_constructor(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let Some((decl, scope)) = constructor_of(req, class) else {
        return Ok(Value::null());
    };
    let object = method_factory(req, &decl.function.name.0, scope)?;
    Ok(object.as_value())
}

/// `_addmethod`'s filter: the default is
/// `ZEND_ACC_PPP_MASK | ABSTRACT | FINAL | STATIC`, which every method
/// carries at least one bit of, so `null` takes them all.
fn method_bits(decl: &MethodDecl) -> i64 {
    let mut bits = match decl.visibility {
        Visibility::Public => 1,
        Visibility::Protected => 2,
        Visibility::Private => 4,
    };
    if !decl.is_instance {
        bits |= 16;
    }
    if decl.is_final {
        bits |= 32;
    }
    if decl.is_abstract {
        bits |= 64;
    }
    bits
}

/// The `?int $filter` argument the three table readers share, `null`
/// being the default mask php substitutes.
fn filter_argument(req: &mut Request, default: i64) -> Result<i64, Thrown> {
    let given = req.argument(1).unwrap_or_default().deref();
    if matches!(given.as_ref(), ValueRef::Null | ValueRef::Undef) {
        return Ok(default);
    }
    php_ext::coerce::to_long(req, &given)
}

/// `ReflectionClass::getMethods(?int $filter = null)`: one
/// `ReflectionMethod` per row of the function table whose flags the
/// filter keeps, a parent's private method skipped as `_addmethod`
/// skips it.
///
/// ```php
/// var_dump(count((new ReflectionClass('JsonSerializable'))->getMethods()));
/// ```
/// ```expect
/// int(1)
/// ```
pub(super) fn reflection_class_get_methods(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 1)?;
    let filter = filter_argument(req, 1 | 2 | 4 | 16 | 32 | 64)?;
    let class = this_class(req)?;
    let mut methods = PhpArray::new(req)?;
    for found in linked_methods(req, class) {
        if found.decl.visibility == Visibility::Private && found.scope != class {
            continue;
        }
        if method_bits(&found.decl) & filter == 0 {
            continue;
        }
        let object = method_factory(req, &found.decl.function.name.0, found.scope)?;
        methods.append(req, object.as_value())?;
    }
    Ok(Value::array(methods))
}

/// `ReflectionClass::getProperty(string $name)`:
/// `reflection_property_factory` over the property info, and php's
/// `ReflectionException` for a name the class has not.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->getProperty('message'));
/// ```
/// ```expect
/// object(ReflectionProperty)#2 (2) {
///   ["name"]=>
///   string(7) "message"
///   ["class"]=>
///   string(9) "Exception"
/// }
/// ```
///
/// ```php
/// (new ReflectionClass('Exception'))->getProperty('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Property Exception::$nope does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getProperty('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_class_get_property(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    let Some((scope, _)) = property_named(req, class, &name) else {
        let named = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Property {}::${} does not exist",
                String::from_utf8_lossy(&named),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let object = property_factory(req, &name, scope)?;
    Ok(object.as_value())
}

/// `ReflectionClass::getProperties(?int $filter = null)`: one
/// `ReflectionProperty` per row of `ce->properties_info` whose flags
/// the filter keeps, a parent's private property skipped as
/// `_addproperty` skips it.
///
/// ```php
/// var_dump(count((new ReflectionClass('Exception'))->getProperties()));
/// ```
/// ```expect
/// int(7)
/// ```
pub(super) fn reflection_class_get_properties(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 1)?;
    let filter = filter_argument(req, 1 | 2 | 4 | 16)?;
    let class = this_class(req)?;
    let mut properties = PhpArray::new(req)?;
    for (decl, scope) in linked_properties(req, class) {
        if decl.visibility == Visibility::Private && scope != class {
            continue;
        }
        let mut bits = match decl.visibility {
            Visibility::Public => 1,
            Visibility::Protected => 2,
            Visibility::Private => 4,
        };
        if !decl.is_instance {
            bits |= 16;
        }
        if bits & filter == 0 {
            continue;
        }
        let object = property_factory(req, &decl.name.0, scope)?;
        properties.append(req, object.as_value())?;
    }
    Ok(Value::array(properties))
}

/// `ReflectionClass::getConstants(?int $filter = null)`: the constants
/// table as an array keyed by name, each value the constant
/// expression evaluated in its declaring class.
///
/// ```php
/// var_dump((new ReflectionClass('ReflectionFunction'))->getConstants());
/// ```
/// ```expect
/// array(1) {
///   ["IS_DEPRECATED"]=>
///   int(2048)
/// }
/// ```
pub(super) fn reflection_class_get_constants(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 1)?;
    let filter = filter_argument(req, 1 | 2 | 4)?;
    let class = this_class(req)?;
    let mut constants = PhpArray::new(req)?;
    for (decl, scope) in linked_constants(req, class) {
        let bits = match decl.visibility {
            Visibility::Public => 1,
            Visibility::Protected => 2,
            Visibility::Private => 4,
        };
        if bits & filter == 0 {
            continue;
        }
        let value = eval_const_expr(req, &decl.value, Some(scope))?;
        let key = PhpString::new(req, &decl.name.0)?;
        constants.set(req, Key::Str(key), value)?;
    }
    Ok(Value::array(constants))
}

/// `ReflectionClass::getReflectionConstants(?int $filter = null)`: the
/// same table, each row a `ReflectionClassConstant`.
///
/// ```php
/// var_dump(count((new ReflectionClass('ReflectionFunction'))->getReflectionConstants()));
/// ```
/// ```expect
/// int(1)
/// ```
pub(super) fn reflection_class_get_reflection_constants(
    req: &mut Request,
) -> Result<Value, Thrown> {
    at_most_arguments(req, 1)?;
    let filter = filter_argument(req, 1 | 2 | 4)?;
    let class = this_class(req)?;
    let mut constants = PhpArray::new(req)?;
    for (decl, scope) in linked_constants(req, class) {
        let bits = match decl.visibility {
            Visibility::Public => 1,
            Visibility::Protected => 2,
            Visibility::Private => 4,
        };
        if bits & filter == 0 {
            continue;
        }
        let object = class_constant_factory(req, &decl.name.0, scope)?;
        constants.append(req, object.as_value())?;
    }
    Ok(Value::array(constants))
}

/// `ReflectionClass::getReflectionConstant(string $name)`: one
/// `ReflectionClassConstant`, and `false` for a name the table has
/// not.
///
/// ```php
/// var_dump((new ReflectionClass('ReflectionFunction'))->getReflectionConstant('IS_DEPRECATED'));
/// var_dump((new ReflectionClass('ReflectionFunction'))->getReflectionConstant('nope'));
/// ```
/// ```expect
/// object(ReflectionClassConstant)#2 (2) {
///   ["name"]=>
///   string(13) "IS_DEPRECATED"
///   ["class"]=>
///   string(18) "ReflectionFunction"
/// }
/// bool(false)
/// ```
pub(super) fn reflection_class_get_reflection_constant(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    let Some((scope, decl)) = class_constant_named(req, class, &name) else {
        return Ok(Value::bool(false));
    };
    let object = class_constant_factory(req, &decl.name.0, scope)?;
    Ok(object.as_value())
}

// ------------------------------------------------------ making one

/// `ReflectionClass::newInstance(mixed ...$args)`: `object_init_ex`
/// and the constructor served over the arguments, php's
/// `ReflectionException` for a class that cannot be instantiated.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->newInstance('x')->getMessage());
/// ```
/// ```expect
/// string(1) "x"
/// ```
pub(super) fn reflection_class_new_instance(req: &mut Request) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    let mut arguments = Vec::new();
    for position in 1..=req.argument_count() {
        let at = u32::try_from(position).unwrap_or(u32::MAX);
        arguments.push(req.argument(at).unwrap_or_default());
    }
    instantiable(req, class)?;
    let object = instantiate(req, class, &arguments)?;
    Ok(object.as_value())
}

/// `ReflectionClass::newInstanceArgs(array $args = [])`: the same,
/// over the array's values in order.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->newInstanceArgs(['x'])->getMessage());
/// ```
/// ```expect
/// string(1) "x"
/// ```
///
/// What is no array is php's `TypeError`, whose tail names the value as
/// `zend_zval_value_name` does and not as `zend_zval_type_name` does, so
/// `true` stands where the type alone would read `bool` (#462):
///
/// ```php
/// (new ReflectionClass('JsonException'))->newInstanceArgs(true);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionClass::newInstanceArgs(): Argument #1 ($args) must be of type array, true given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->newInstanceArgs(true)
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_class_new_instance_args(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 1)?;
    let class = this_class(req)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let mut arguments = Vec::new();
    if let ValueRef::Array(array) = given.as_ref() {
        let mut cursor = array.iter();
        while cursor.advance() {
            if let Some(value) = cursor.value() {
                arguments.push(*value);
            }
        }
    } else if !matches!(given.as_ref(), ValueRef::Null | ValueRef::Undef) {
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type array, {} given", value_name(req, &given)),
        ));
    }
    instantiable(req, class)?;
    let object = instantiate(req, class, &arguments)?;
    Ok(object.as_value())
}

/// `ReflectionClass::newInstanceWithoutConstructor()`:
/// `object_init_ex` alone, refused for a native class php gives a
/// `create_object` handler of its own. The object's inherited
/// properties hold no default until the linker places them (#419), so
/// a read of one is `null` here where php's is the declared default.
///
/// ```php
/// var_dump((new ReflectionClass('JsonException'))->newInstanceWithoutConstructor() instanceof JsonException);
/// ```
/// ```expect
/// bool(true)
/// ```
pub(super) fn reflection_class_new_instance_without_constructor(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    instantiable(req, class)?;
    let object = new_object(req, class)?;
    Ok(object.as_value())
}

/// php's `zend_throw_exception_ex(reflection_exception_ptr, ...)` for
/// a class `new` refuses, the message `ReflectionClass` gives before
/// the engine's own.
fn instantiable(req: &mut Request, class: ClassId) -> Result<(), Thrown> {
    if !cannot_instantiate(req, class) {
        return Ok(());
    }
    let name = class_name_of(req, class);
    let what = match kind_of(req, class) {
        ClassKind::Interface => "interface",
        ClassKind::Trait => "trait",
        ClassKind::Enum => "enum",
        ClassKind::Class => "abstract class",
    };
    Err(throw(
        req,
        "ReflectionException",
        &format!("Class {} is an {what}", String::from_utf8_lossy(&name)),
    ))
}

/// `ReflectionClass::getStaticProperties()`: `CE_STATIC_MEMBERS(ce)` as
/// an array, one slot per `static` property of `properties_info` under
/// its own name, in that table's order, the slots laid down from their
/// defaults on first use (`zend_class_init_statics`). php skips a
/// property a class above declared private, one declared as an
/// instance property, and a typed one nothing has written, whose slot
/// stands `IS_UNDEF`; each value is handed back deref'd, with one more
/// owner, which is what "enforce read only access" comes to.
///
/// ```php
/// class A { public static int $n; public static $p = 1; private static $q = 2; protected static $r = 3; public $i = 4; }
/// class B extends A { private static $q = 5; }
/// var_dump((new ReflectionClass('A'))->getStaticProperties(), (new ReflectionClass('B'))->getStaticProperties());
/// ```
/// ```expect
/// array(3) {
///   ["p"]=>
///   int(1)
///   ["q"]=>
///   int(2)
///   ["r"]=>
///   int(3)
/// }
/// array(3) {
///   ["q"]=>
///   int(5)
///   ["p"]=>
///   int(1)
///   ["r"]=>
///   int(3)
/// }
/// ```
///
/// ```php
/// (new ReflectionClass('Exception'))->getStaticProperties(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getStaticProperties() expects exactly 0 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getStaticProperties(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_class_get_static_properties(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let mut properties = PhpArray::new(req)?;
    for (property, declaring) in linked_properties(req, class) {
        if property.is_instance
            || (property.visibility == Visibility::Private && declaring != class)
        {
            continue;
        }
        let Some(value) = read_class_prop_silent(req, class, &property.name.0)? else {
            continue;
        };
        if matches!(value.as_ref(), ValueRef::Undef) {
            continue;
        }
        let key = Key::Str(PhpString::new(req, &property.name.0)?);
        properties.set(req, key, value)?;
    }
    Ok(Value::array(properties))
}

/// `ReflectionClass::getStaticPropertyValue(string $name, mixed
/// $default = UNKNOWN)`: the slot addressed under `BP_VAR_IS` with the
/// fake scope set to the class ([`read_class_prop_silent`]), so a
/// property the class itself declared private is reached and one a
/// class above did is not. A slot holding a value is that value, one
/// more owner taken (`RETURN_COPY_DEREF`); otherwise `$default` where
/// the call gave one, which php hands back before it looks at why the
/// slot was empty; otherwise php's `Error` for a typed property
/// nothing has written, whose text here is `Typed property` and not
/// the engine's `Typed static property`, and php's
/// `ReflectionException` for a name the class does not have.
///
/// ```php
/// class A { private static $q = 'a'; protected static $r = 'b'; public static $p = 'c'; }
/// class B extends A { private static $q = 'd'; }
/// $a = new ReflectionClass('A'); $b = new ReflectionClass('B');
/// var_dump($a->getStaticPropertyValue('q'), $a->getStaticPropertyValue('r'), $a->getStaticPropertyValue('p'));
/// var_dump($b->getStaticPropertyValue('q'), $b->getStaticPropertyValue('r'), $b->getStaticPropertyValue('nope', 'default value'));
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// string(1) "c"
/// string(1) "d"
/// string(1) "b"
/// string(13) "default value"
/// ```
///
/// A typed property nothing has written is the `Error`, and a default
/// stands before it.
///
/// ```php
/// class A { public static int $n; }
/// var_dump((new ReflectionClass('A'))->getStaticPropertyValue('n', 'd'));
/// (new ReflectionClass('A'))->getStaticPropertyValue('n');
/// ```
/// ```expect exit=255
/// string(1) "d"
///
/// Fatal error: Uncaught Error: Typed property A::$n must not be accessed before initialization in %s:4
/// Stack trace:
/// #0 %s(4): ReflectionClass->getStaticPropertyValue('n')
/// #1 {main}
///   thrown in %s on line 4
/// ```
///
/// An instance property is a name the class does not have, as php's
/// `BP_VAR_IS` hands back nothing for one.
///
/// ```php
/// class A { public $i = 1; }
/// (new ReflectionClass('A'))->getStaticPropertyValue('i');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Property A::$i does not exist in %s:3
/// Stack trace:
/// #0 %s(3): ReflectionClass->getStaticPropertyValue('i')
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// (new ReflectionClass('Exception'))->getStaticPropertyValue();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getStaticPropertyValue() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getStaticPropertyValue()
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_class_get_static_property_value(
    req: &mut Request,
) -> Result<Value, Thrown> {
    at_most_arguments(req, 2)?;
    if req.argument_count() == 0 {
        return Err(arg_count_error(req, "at least 1 argument, 0 given"));
    }
    let name = string_argument(req, 1)?;
    let class = this_class(req)?;
    let held = read_class_prop_silent(req, class, &name)?;
    if let Some(value) = held
        && !matches!(value.as_ref(), ValueRef::Undef)
    {
        return Ok(value);
    }
    if let Some(default) = req.argument(2) {
        let default = default.deref();
        req.addref(&default);
        return Ok(default);
    }
    let shown = class_name_of(req, class);
    let shown = String::from_utf8_lossy(&shown).into_owned();
    let name = String::from_utf8_lossy(&name).into_owned();
    if held.is_some() {
        return Err(throw(
            req,
            "Error",
            &format!("Typed property {shown}::${name} must not be accessed before initialization"),
        ));
    }
    Err(throw(
        req,
        "ReflectionException",
        &format!("Property {shown}::${name} does not exist"),
    ))
}

/// `ReflectionClass::setStaticPropertyValue(string $name, mixed
/// $value)`: the slot addressed under `BP_VAR_W` with the fake scope
/// set to the class, which is [`read_class_prop_silent`]'s test with
/// the typed-and-unwritten slot standing as a slot; a name the class
/// does not have is php's `ReflectionException` in words of its own,
/// `Class C does not have a property named p`, and never the engine's
/// `Access to undeclared static property`. The write is
/// `zend_update_static_property_ex` ([`write_static_prop`]), which
/// writes through a slot holding a reference.
///
/// php verifies the value against a typed property
/// (`zend_verify_property_type`) and this engine does not yet, here or
/// on `ReflectionProperty::setValue`; a value of the property's own
/// type is written the same either way.
///
/// ```php
/// class A { private static $q = 'a'; public static $p = 'b'; }
/// class B extends A { private static $q = 'c'; }
/// $a = new ReflectionClass('A'); $b = new ReflectionClass('B');
/// $a->setStaticPropertyValue('q', 'one'); $a->setStaticPropertyValue('p', 'two'); $b->setStaticPropertyValue('q', 'three');
/// var_dump($a->getStaticProperties(), $b->getStaticProperties());
/// ```
/// ```expect
/// array(2) {
///   ["q"]=>
///   string(3) "one"
///   ["p"]=>
///   string(3) "two"
/// }
/// array(2) {
///   ["q"]=>
///   string(5) "three"
///   ["p"]=>
///   string(3) "two"
/// }
/// ```
///
/// ```php
/// class A { public $i = 1; }
/// (new ReflectionClass('A'))->setStaticPropertyValue('i', 2);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class A does not have a property named i in %s:3
/// Stack trace:
/// #0 %s(3): ReflectionClass->setStaticPropertyValue('i', 2)
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// (new ReflectionClass('Exception'))->setStaticPropertyValue('p');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::setStaticPropertyValue() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->setStaticPropertyValue('p')
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(super) fn reflection_class_set_static_property_value(
    req: &mut Request,
) -> Result<Value, Thrown> {
    exactly_arguments(req, 2)?;
    let name = string_argument(req, 1)?;
    let class = this_class(req)?;
    let held = read_class_prop_silent(req, class, &name)?;
    let Some(held) = held else {
        let shown = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Class {} does not have a property named {}",
                String::from_utf8_lossy(&shown),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    req.release(held);
    let value = req.argument(2).unwrap_or_default().deref();
    req.addref(&value);
    write_static_prop(req, class, &name, value)?;
    Ok(Value::null())
}
