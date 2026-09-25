//! The lazy-object entries of `php_reflection.c`: `ReflectionClass`'s
//! eight, which make an object lazy, initialise it, or ask what it
//! waits for, and `ReflectionProperty`'s three, which take one property
//! out of the set that waits. Each parses its arguments as php's
//! `ZEND_PARSE_PARAMETERS` block does and raises php's
//! `ReflectionException` for what the reflector refuses; the object
//! model is the engine's (`php_ext::call`'s lazy face, design §5.4).

use php_ext::call::{
    initialize_lazy, lookup_class, mark_lazy_initialized, new_lazy_ghost, new_lazy_proxy,
    reset_as_lazy_ghost, reset_as_lazy_proxy, skip_lazy_initialization,
};
use php_ext::coerce::{callable_argument, coerce_param, value_name};
use php_ext::decl::Type;
use php_ext::raising::{Thrown, arg_count_error, arg_type_error, argument_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, PhpObject, Value};

use super::{asked_class, class_chain, class_name_of, is_internal, this_class, this_property};

/// `ZEND_LAZY_OBJECT_SKIP_INITIALIZATION_ON_SERIALIZE`,
/// `ReflectionClass::SKIP_INITIALIZATION_ON_SERIALIZE`.
const SKIP_INITIALIZATION_ON_SERIALIZE: i64 = 1 << 3;
/// `ZEND_LAZY_OBJECT_SKIP_DESTRUCTOR`, `ReflectionClass::SKIP_DESTRUCTOR`.
const SKIP_DESTRUCTOR: i64 = 1 << 4;

/// `ZEND_PARSE_PARAMETERS_START(least, most)`'s count check.
fn arguments_between(req: &mut Request, least: usize, most: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    let (word, bound) = if count < least {
        (if least == most { "exactly" } else { "at least" }, least)
    } else if count > most {
        (if least == most { "exactly" } else { "at most" }, most)
    } else {
        return Ok(());
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {count} given"),
    ))
}

/// `Z_PARAM_OBJ_OF_CLASS(object, ce)`: an object of `class` or of a
/// class below it, and php's `TypeError` naming the class otherwise.
fn object_of(req: &mut Request, position: u32, class: ClassId) -> Result<PhpObject, Thrown> {
    let given = req.argument(position).unwrap_or_default().deref();
    if let Some(object) = given.as_object()
        && object.instance_of(req, class)
    {
        return Ok(*object);
    }
    let name = String::from_utf8_lossy(&class_name_of(req, class)).into_owned();
    let shown = value_name(req, &given);
    Err(arg_type_error(
        req,
        position,
        &format!("be of type {name}, {shown} given"),
    ))
}

/// `Z_PARAM_LONG` at `position`, zero where the argument is absent.
fn options_argument(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let Some(given) = req.argument(position) else {
        return Ok(0);
    };
    let held = coerce_param(req, &given, &Type::Int, position)?;
    Ok(held.as_int().unwrap_or(0))
}

/// Which of the two strategies `reflection_class_new_lazy` is asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strategy {
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
fn new_lazy(req: &mut Request, strategy: Strategy, reset: bool) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    let first = u32::from(reset);
    arguments_between(req, 1 + usize::from(reset), 2 + usize::from(reset))?;
    let object = if reset {
        Some(object_of(req, 1, class)?)
    } else {
        None
    };
    let Some(initializer) = callable_argument(req, first + 1)? else {
        return Err(arg_type_error(req, first + 1, "be a valid callback"));
    };
    let options = options_argument(req, first + 2)?;
    if options & !(SKIP_INITIALIZATION_ON_SERIALIZE | SKIP_DESTRUCTOR) != 0 {
        return Err(argument_error(
            req,
            "ReflectionException",
            first + 2,
            "contains invalid flags",
        ));
    }
    if !reset && options & SKIP_DESTRUCTOR != 0 {
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
            reset_as_lazy_ghost(req, &object, class, &initializer, options).map(|()| Value::null())
        }
        (Some(object), Strategy::Proxy) => {
            reset_as_lazy_proxy(req, &object, class, &initializer, options).map(|()| Value::null())
        }
        (None, Strategy::Ghost) => {
            new_lazy_ghost(req, class, &initializer, options).map(|o| o.as_value())
        }
        (None, Strategy::Proxy) => {
            new_lazy_proxy(req, class, &initializer, options).map(|o| o.as_value())
        }
    }
}

/// `ReflectionClass::newLazyGhost(callable $initializer, int $options = 0): object`:
/// an object of the class whose properties wait for the initializer,
/// served with the object in place the first time one is touched.
///
/// ```php
/// class C { public int $a; public $b = 2; public function __construct() { echo "init\n"; $this->a = 1; } }
/// $o = (new ReflectionClass('C'))->newLazyGhost(function (C $o) { $o->__construct(); });
/// var_dump($o);
/// var_dump($o->a);
/// var_dump($o);
/// ```
/// ```expect
/// lazy ghost object(C)#3 (0) {
///   ["a"]=>
///   uninitialized(int)
/// }
/// init
/// int(1)
/// object(C)#3 (2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   int(2)
/// }
/// ```
pub(super) fn reflection_class_new_lazy_ghost(req: &mut Request) -> Result<Value, Thrown> {
    new_lazy(req, Strategy::Ghost, false)
}

/// `ReflectionClass::newLazyProxy(callable $factory, int $options = 0): object`:
/// an object of the class whose accesses, once one is made, go to the
/// instance the factory returns.
///
/// ```php
/// class C { public $a = 1; }
/// $r = new ReflectionClass('C');
/// $p = $r->newLazyProxy(function () { echo "factory\n"; $c = new C; $c->a = 5; return $c; });
/// var_dump($p->a);
/// $i = $r->initializeLazyObject($p);
/// var_dump($i === $p, $i->a);
/// $p->a = 7;
/// var_dump($i->a, $p);
/// ```
/// ```expect
/// factory
/// int(5)
/// bool(false)
/// int(5)
/// int(7)
/// lazy proxy object(C)#3 (1) {
///   ["instance"]=>
///   object(C)#4 (1) {
///     ["a"]=>
///     int(7)
///   }
/// }
/// ```
pub(super) fn reflection_class_new_lazy_proxy(req: &mut Request) -> Result<Value, Thrown> {
    new_lazy(req, Strategy::Proxy, false)
}

/// `ReflectionClass::resetAsLazyGhost(object $object, callable $initializer, int $options = 0): void`:
/// the object's destructor served unless `SKIP_DESTRUCTOR`, its
/// properties unset, and the object left a ghost of the class.
///
/// ```php
/// class C { public $a = 1; function __destruct() { echo "destruct\n"; } }
/// $r = new ReflectionClass('C');
/// $o = new C;
/// $r->resetAsLazyGhost($o, function ($o) { echo "init\n"; $o->a = 3; });
/// var_dump($r->isUninitializedLazyObject($o));
/// try { $r->resetAsLazyGhost($o, fn () => null); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o->a);
/// $r->resetAsLazyGhost($o, function () {}, ReflectionClass::SKIP_DESTRUCTOR);
/// echo "end\n";
/// ```
/// ```expect
/// destruct
/// bool(true)
/// Object is already lazy
/// init
/// int(3)
/// end
/// ```
pub(super) fn reflection_class_reset_as_lazy_ghost(req: &mut Request) -> Result<Value, Thrown> {
    new_lazy(req, Strategy::Ghost, true)
}

/// `ReflectionClass::resetAsLazyProxy(object $object, callable $factory, int $options = 0): void`:
/// the same reset, the object left a proxy.
///
/// ```php
/// class C { public $a = 1; }
/// $r = new ReflectionClass('C');
/// $o = new C;
/// $r->resetAsLazyProxy($o, function ($o) { echo "factory\n"; return new C; });
/// var_dump($o);
/// $o->a = 2;
/// var_dump($o);
/// ```
/// ```expect
/// lazy proxy object(C)#2 (0) {
/// }
/// factory
/// lazy proxy object(C)#2 (1) {
///   ["instance"]=>
///   object(C)#4 (1) {
///     ["a"]=>
///     int(2)
///   }
/// }
/// ```
pub(super) fn reflection_class_reset_as_lazy_proxy(req: &mut Request) -> Result<Value, Thrown> {
    new_lazy(req, Strategy::Proxy, true)
}

/// `ReflectionClass::isUninitializedLazyObject(object $object): bool`.
///
/// ```php
/// class C { public $a = 1; }
/// $r = new ReflectionClass('C');
/// $o = $r->newLazyGhost(function () {});
/// var_dump($r->isUninitializedLazyObject($o), $r->isUninitializedLazyObject(new C));
/// $o->a;
/// var_dump($r->isUninitializedLazyObject($o));
/// try { $r->isUninitializedLazyObject(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ReflectionClass::isUninitializedLazyObject(): Argument #1 ($object) must be of type C, stdClass given
/// ```
pub(super) fn reflection_class_is_uninitialized_lazy_object(
    req: &mut Request,
) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    arguments_between(req, 1, 1)?;
    let object = object_of(req, 1, class)?;
    Ok(Value::bool(object.is_lazy()))
}

/// `ReflectionClass::initializeLazyObject(object $object): object`: a
/// lazy object initialised now, and the object its properties live on,
/// the ghost itself or the proxy's instance.
///
/// ```php
/// class C { public $a = 1; public int $b; }
/// $r = new ReflectionClass('C');
/// $o = $r->newLazyGhost(function ($o) { $o->a = 2; $o->b = 3; $o->dyn = 4; throw new Exception('no'); });
/// try { $r->initializeLazyObject($o); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump($r->isUninitializedLazyObject($o), $o);
/// ```
/// ```expect
///
/// Deprecated: Creation of dynamic property C::$dyn is deprecated in %s on line 4
/// no
/// bool(true)
/// lazy ghost object(C)#3 (0) {
///   ["b"]=>
///   uninitialized(int)
/// }
/// ```
pub(super) fn reflection_class_initialize_lazy_object(req: &mut Request) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    arguments_between(req, 1, 1)?;
    let object = object_of(req, 1, class)?;
    let instance = initialize_lazy(req, &object)?;
    req.addref(&instance.as_value());
    Ok(instance.as_value())
}

/// `ReflectionClass::markLazyObjectAsInitialized(object $object): object`:
/// each waiting property takes its default, the initializer is never
/// served, and the answer is `initializeLazyObject`'s.
///
/// ```php
/// class C { public $a = 1; public int $b; public $c; }
/// $r = new ReflectionClass('C');
/// $o = $r->newLazyGhost(function () { echo "never\n"; });
/// $r->getProperty('c')->setRawValueWithoutLazyInitialization($o, 3);
/// var_dump($r->markLazyObjectAsInitialized($o) === $o, $o);
/// ```
/// ```expect
/// bool(true)
/// object(C)#3 (2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   uninitialized(int)
///   ["c"]=>
///   int(3)
/// }
/// ```
pub(super) fn reflection_class_mark_lazy_object_as_initialized(
    req: &mut Request,
) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    arguments_between(req, 1, 1)?;
    let object = object_of(req, 1, class)?;
    let instance = mark_lazy_initialized(req, &object)?;
    req.addref(&instance.as_value());
    Ok(instance.as_value())
}

/// `ReflectionClass::getLazyInitializer(object $object): ?callable`:
/// the initializer as it was given, while nothing has initialised the
/// object, and `null` otherwise.
///
/// ```php
/// class C { public $a = 1; }
/// function init($o) {}
/// $r = new ReflectionClass('C');
/// $o = $r->newLazyGhost('init');
/// var_dump($r->getLazyInitializer($o));
/// $r->initializeLazyObject($o);
/// var_dump($r->getLazyInitializer($o), $r->getLazyInitializer(new C));
/// ```
/// ```expect
/// string(4) "init"
/// NULL
/// NULL
/// ```
pub(super) fn reflection_class_get_lazy_initializer(req: &mut Request) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    arguments_between(req, 1, 1)?;
    let object = object_of(req, 1, class)?;
    let Some(initializer) = object.lazy_initializer(req) else {
        return Ok(Value::null());
    };
    req.addref(&initializer);
    Ok(initializer)
}

/// `reflection_property_check_lazy_compatible`: the property is a
/// declared instance property, of an object whose class could be lazy.
fn lazy_compatible(
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

/// `ReflectionProperty::setRawValueWithoutLazyInitialization(object $object, mixed $value): void`:
/// the value written into the property past `__set` and hooks, on the
/// object the initialised proxies lead to, and the property no longer
/// waiting; the object is no longer lazy once none waits.
///
/// ```php
/// class C { public $a = 1; public int $b = 2; public static $s; public function __set($n, $v) { echo "set $n\n"; } }
/// $r = new ReflectionClass('C');
/// $o = $r->newLazyGhost(function () { echo "init\n"; });
/// $r->getProperty('a')->setRawValueWithoutLazyInitialization($o, 5);
/// var_dump($r->isUninitializedLazyObject($o));
/// try { $r->getProperty('b')->setRawValueWithoutLazyInitialization($o, 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($r->getProperty('b')->isLazy($o));
/// $r->getProperty('b')->setRawValueWithoutLazyInitialization($o, 3);
/// var_dump($r->isUninitializedLazyObject($o), $o);
/// try { $r->getProperty('s')->setRawValueWithoutLazyInitialization($o, 1); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// $d = new stdClass; $d->dyn = 1;
/// try { (new ReflectionProperty($d, 'dyn'))->setRawValueWithoutLazyInitialization($d, 2); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// Cannot assign string to property C::$b of type int
/// bool(true)
/// bool(false)
/// object(C)#3 (2) {
///   ["a"]=>
///   int(5)
///   ["b"]=>
///   int(3)
/// }
/// Can not use setRawValueWithoutLazyInitialization on static property C::$s
/// Can not use setRawValueWithoutLazyInitialization on dynamic property stdClass::$dyn
/// ```
pub(super) fn reflection_property_set_raw_value_without_lazy_initialization(
    req: &mut Request,
) -> Result<Value, Thrown> {
    let (class, _, _) = this_property(req)?;
    let asked = asked_class(req, class);
    arguments_between(req, 2, 2)?;
    let object = object_of(req, 1, asked)?;
    let object = object.lazy_real(req).unwrap_or(object);
    let (_, name) = lazy_compatible(req, object, "setRawValueWithoutLazyInitialization")?;
    let value = req.argument(2).unwrap_or_default().deref();
    req.addref(&value);
    skip_lazy_initialization(req, &object, &name, Some(value))?;
    Ok(Value::null())
}

/// `ReflectionProperty::skipLazyInitialization(object $object): void`:
/// the property takes its default and no longer waits; a property that
/// does not wait is left as it stands.
///
/// ```php
/// class C { public $a = 1; public int $b = 2; }
/// $r = new ReflectionClass('C');
/// $o = $r->newLazyGhost(function () { echo "init\n"; });
/// $r->getProperty('a')->skipLazyInitialization($o);
/// var_dump($o->a, $r->isUninitializedLazyObject($o));
/// $r->getProperty('b')->skipLazyInitialization($o);
/// var_dump($r->isUninitializedLazyObject($o), $o);
/// ```
/// ```expect
/// int(1)
/// bool(true)
/// bool(false)
/// object(C)#3 (2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   int(2)
/// }
/// ```
pub(super) fn reflection_property_skip_lazy_initialization(
    req: &mut Request,
) -> Result<Value, Thrown> {
    let (class, _, _) = this_property(req)?;
    let asked = asked_class(req, class);
    arguments_between(req, 1, 1)?;
    let object = object_of(req, 1, asked)?;
    let (_, name) = lazy_compatible(req, object, "skipLazyInitialization")?;
    skip_lazy_initialization(req, &object, &name, None)?;
    Ok(Value::null())
}

/// `ReflectionProperty::isLazy(object $object): bool`: whether the
/// property, on the object the initialised proxies lead to, still waits
/// for the initializer; `false` for a dynamic property and for a class
/// property (`ZEND_ACC_STATIC`).
///
/// ```php
/// class C { public $a = 1; public $b = 2; public static $s; }
/// $r = new ReflectionClass('C');
/// $p = $r->newLazyProxy(function () { return new C; });
/// $r->getProperty('a')->skipLazyInitialization($p);
/// var_dump($r->getProperty('a')->isLazy($p), $r->getProperty('b')->isLazy($p), $r->getProperty('s')->isLazy($p));
/// $r->initializeLazyObject($p);
/// var_dump($r->getProperty('a')->isLazy($p), $r->getProperty('b')->isLazy($p));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(super) fn reflection_property_is_lazy(req: &mut Request) -> Result<Value, Thrown> {
    let (class, name, declared) = this_property(req)?;
    let asked = asked_class(req, class);
    arguments_between(req, 1, 1)?;
    let object = object_of(req, 1, asked)?;
    if !declared.is_some_and(|decl| decl.is_instance) {
        return Ok(Value::bool(false));
    }
    Ok(Value::bool(object.is_lazy_prop(req, &name)))
}
