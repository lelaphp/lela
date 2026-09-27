//! What the lazy-object methods of `php_reflection.c` share:
//! `ReflectionClass`'s eight, which make an object lazy, initialise it,
//! or ask what it waits for, and `ReflectionProperty`'s three, which
//! take one property out of the set that waits. Their arguments are
//! held by their declarations; what is here holds the object to the
//! reflected class and raises php's `ReflectionException` for what the
//! reflector refuses; the object model is the engine's (`php_ext::call`'s
//! lazy face, design §5.4).

use php_ext::abi::Callable;
use php_ext::call::{
    lookup_class, new_lazy_ghost, new_lazy_proxy, reset_as_lazy_ghost, reset_as_lazy_proxy,
};
use php_ext::coerce::value_name;
use php_ext::raising::{Thrown, arg_type_error, argument_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, PhpObject, Value};

use super::{asked_class, class_chain, class_name_of, is_internal, this_class, this_property};

/// `ZEND_LAZY_OBJECT_SKIP_INITIALIZATION_ON_SERIALIZE`,
/// `ReflectionClass::SKIP_INITIALIZATION_ON_SERIALIZE`.
const SKIP_INITIALIZATION_ON_SERIALIZE: i64 = 1 << 3;
/// `ZEND_LAZY_OBJECT_SKIP_DESTRUCTOR`, `ReflectionClass::SKIP_DESTRUCTOR`.
const SKIP_DESTRUCTOR: i64 = 1 << 4;

/// `Z_PARAM_OBJ_OF_CLASS(object, ce)` past `object`: an object of
/// `class` or of a class below it, and php's `TypeError` naming the
/// class otherwise.
pub(super) fn object_of(
    req: &mut Request,
    object: PhpObject,
    class: ClassId,
    position: u32,
) -> Result<PhpObject, Thrown> {
    if object.instance_of(req, class) {
        return Ok(object);
    }
    let name = String::from_utf8_lossy(&class_name_of(req, class)).into_owned();
    let shown = value_name(req, &object.as_value());
    Err(arg_type_error(
        req,
        position,
        &format!("be of type {name}, {shown} given"),
    ))
}

/// Which of the two strategies `reflection_class_new_lazy` is asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Strategy {
    Ghost,
    Proxy,
}

/// `reflection_class_new_lazy` (`Z/ext/reflection/php_reflection.c:5187`),
/// the body the four makers share: the arguments parsed, the options
/// held to the user mask, `SKIP_DESTRUCTOR` refused where nothing is
/// reset and an object already lazy refused where one is, then the
/// engine's `zend_object_make_lazy`.
///
/// ```php
/// class C { public $a = 1; }
/// class E extends ArrayObject {}
/// $r = new ReflectionClass('C');
/// try { $r->newLazyGhost(fn () => null, 1); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// try { $r->newLazyGhost(fn () => null, ReflectionClass::SKIP_DESTRUCTOR); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionClass('ArrayObject'))->newLazyGhost(fn () => null); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionClass('E'))->newLazyProxy(fn () => null); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump((new ReflectionClass('stdClass'))->newLazyGhost(fn () => null));
/// ```
/// ```expect
/// ReflectionClass::newLazyGhost(): Argument #2 ($options) contains invalid flags
/// ReflectionClass::newLazyGhost(): Argument #2 ($options) does not accept ReflectionClass::SKIP_DESTRUCTOR
/// Cannot make instance of internal class lazy: ArrayObject is internal
/// Cannot make instance of internal class lazy: E inherits internal class ArrayObject
/// object(stdClass)#%d (0) {
/// }
/// ```
pub(super) fn new_lazy(
    req: &mut Request,
    strategy: Strategy,
    object: Option<PhpObject>,
    initializer: &Callable,
    options: i64,
) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    let first = u32::from(object.is_some());
    let object = match object {
        Some(object) => Some(object_of(req, object, class, 1)?),
        None => None,
    };
    if options & !(SKIP_INITIALIZATION_ON_SERIALIZE | SKIP_DESTRUCTOR) != 0 {
        return Err(argument_error(
            req,
            "ReflectionException",
            first + 2,
            "contains invalid flags",
        ));
    }
    if object.is_none() && options & SKIP_DESTRUCTOR != 0 {
        return Err(argument_error(
            req,
            "ReflectionException",
            2,
            "does not accept ReflectionClass::SKIP_DESTRUCTOR",
        ));
    }
    match (object, strategy) {
        (Some(object), _) if object.is_lazy() => {
            Err(throw(req, "ReflectionException", "Object is already lazy"))
        }
        (Some(object), Strategy::Ghost) => {
            reset_as_lazy_ghost(req, &object, class, initializer, options).map(|()| Value::null())
        }
        (Some(object), Strategy::Proxy) => {
            reset_as_lazy_proxy(req, &object, class, initializer, options).map(|()| Value::null())
        }
        (None, Strategy::Ghost) => {
            new_lazy_ghost(req, class, initializer, options).map(|o| o.as_value())
        }
        (None, Strategy::Proxy) => {
            new_lazy_proxy(req, class, initializer, options).map(|o| o.as_value())
        }
    }
}

/// `reflection_property_check_lazy_compatible`: the property is a
/// declared instance property, of an object whose class could be lazy.
pub(super) fn lazy_compatible(
    req: &mut Request,
    object: PhpObject,
    method: &str,
) -> Result<(ClassId, Vec<u8>), Thrown> {
    let (class, name, declared) = this_property(req)?;
    let asked = asked_class(req, class);
    let shown = String::from_utf8_lossy(&name).into_owned();
    let Some(declared) = declared else {
        let of = String::from_utf8_lossy(&class_name_of(req, asked)).into_owned();
        return Err(throw(
            req,
            "ReflectionException",
            &format!("Can not use {method} on dynamic property {of}::${shown}"),
        ));
    };
    if !declared.is_instance {
        let of = String::from_utf8_lossy(&class_name_of(req, class)).into_owned();
        return Err(throw(
            req,
            "ReflectionException",
            &format!("Can not use {method} on static property {of}::${shown}"),
        ));
    }
    // `ZEND_ACC_VIRTUAL`: a property with no slot has none to write.
    if php_ext::call::property_is_virtual(req, class, &name) {
        let of = String::from_utf8_lossy(&class_name_of(req, class)).into_owned();
        return Err(throw(
            req,
            "ReflectionException",
            &format!("Can not use {method} on virtual property {of}::${shown}"),
        ));
    }
    // `zend_class_can_be_lazy`: no internal class but `stdClass` on the
    // object's chain.
    let std_class = lookup_class(req, b"stdClass")?;
    let internal = class_chain(req, object.class())
        .into_iter()
        .any(|at| is_internal(req, at) && Some(at) != std_class);
    if internal {
        let of = String::from_utf8_lossy(&class_name_of(req, object.class())).into_owned();
        return Err(throw(
            req,
            "ReflectionException",
            &format!("Can not use {method} on internal class {of}"),
        ));
    }
    Ok((asked, name))
}
