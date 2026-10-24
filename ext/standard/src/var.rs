//! The dumpers of `var.c` and the renderer under them. `print_r` is
//! `ext/standard`'s native (`PHP_FUNCTION(print_r)`,
//! `Z/ext/standard/basic_functions.c:2093`); what it renders with is
//! Zend's, `zend_print_zval_r_to_buf` and `print_hash`
//! (`Z/Zend/zend.c:380-612`), where 8.5 keeps the renderer php's
//! `ReflectionExtension::info` and `var.c` both call. The rendering is
//! the same walk `php_var_dump` does over the same rows and lays them
//! out differently: `Array\n(\n    [k] => v\n)\n` and `Class Object\n`
//! over four spaces of indent per level, a property's visibility in its
//! name rather than beside it, and no type or length anywhere.

use std::ops::ControlFlow;

use php_ext::abi::{
    ObjectHandlers, PropertyPlace, PropertyPurpose, PropertyRead, PropertyTest, True, Union,
};
use php_ext::attr::{php_class, php_function};
use php_ext::coerce::{Exponent, float_format, with_string};
use php_ext::decl::{ClassKind, EnumBacking};
use php_ext::raising::{Level, Thrown, call_stack_end, docref, pending, throw_error, warning};
use php_ext::request::Request;
use php_ext::values::walk::{self, GiveUp, Guarded, Hold, Unwritten};
use php_ext::values::{Emalloc, Key, Owner, PhpString, RecursionGuard, SmartStr, Value};

use crate::string::Decimal;

pub(crate) mod debug;
pub(crate) mod serialize;
pub(crate) mod unserialize;

pub use serialize::Serializer;
pub use unserialize::{Unserializer, php_unserialize_with_options};

/// `BG(serialize_lock)`, `BG(serialize)` and `BG(unserialize)`
/// (`Z/ext/standard/basic_functions.h`): the var hash every serializer
/// and every unserializer opened while one is open shares, so a
/// `Serializable::serialize()` that serializes its parts numbers them
/// in the string around it, and an `R:` after a `C:` record names a value
/// read inside it; and `BG(serialize_lock)`, taken around the magic
/// methods, under which each opens a var hash of its own.
#[derive(Default)]
pub(crate) struct Nesting {
    /// `BG(serialize_lock)`: how many `__serialize()`, `__sleep()`,
    /// `__unserialize()` and `__wakeup()` calls are under way.
    lock: u32,
    serialize: Shared<serialize::VarHash>,
    unserialize: Shared<unserialize::VarHash>,
}

/// One of `BG(serialize)` and `BG(unserialize)`: how many holders share
/// the var hash, `level`, and the var hash itself while its holder has
/// set it down for a call that may open another holder.
struct Shared<T> {
    level: u32,
    parked: Option<T>,
}

impl<T> Default for Shared<T> {
    fn default() -> Shared<T> {
        Shared {
            level: 0,
            parked: None,
        }
    }
}

/// How a serializer or an unserializer holds its var hash.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Holder {
    /// A var hash of its own, opened while `BG(serialize_lock)` is held.
    Own,
    /// The shared var hash, which it opened: `BG(...).level` was zero.
    First,
    /// The shared var hash another holder opened and set down for it.
    Joined,
}

impl<T> Shared<T> {
    /// `PHP_VAR_SERIALIZE_INIT` and its twin: `fresh` as a var hash of its
    /// own while `BG(serialize_lock)` is held, `fresh` as the shared one
    /// where none is open, and the open one otherwise.
    fn open(&mut self, lock: u32, fresh: T) -> (T, Holder) {
        if lock > 0 {
            return (fresh, Holder::Own);
        }
        if self.level == 0 {
            self.level = 1;
            return (fresh, Holder::First);
        }
        match self.parked.take() {
            Some(hash) => {
                self.level += 1;
                (hash, Holder::Joined)
            }
            // A holder that did not set its var hash down: this one keeps
            // a var hash of its own, as while the lock is held.
            None => (fresh, Holder::Own),
        }
    }

    /// `PHP_VAR_SERIALIZE_DESTROY` and its twin: the var hash to destroy
    /// where this was its only or its last holder, `None` where it goes
    /// back to the holder that set it down.
    fn close(&mut self, hash: T, holder: Holder) -> Option<T> {
        if holder == Holder::Own {
            return Some(hash);
        }
        self.level = self.level.saturating_sub(1);
        if self.level == 0 {
            return Some(hash);
        }
        self.parked = Some(hash);
        None
    }

    /// The var hash set down for a call that may open another holder,
    /// and taken up again after it, `f`'s answer beside it; a var hash
    /// of its own is kept through the call.
    fn set_down<R>(
        req: &mut Request,
        home: fn(&mut Nesting) -> &mut Shared<T>,
        hash: T,
        holder: Holder,
        f: impl FnOnce(&mut Request) -> R,
    ) -> (T, R)
    where
        T: Default,
    {
        if holder == Holder::Own {
            return (hash, f(req));
        }
        home(&mut req.state::<crate::State>().var).parked = Some(hash);
        let answered = f(req);
        let hash = home(&mut req.state::<crate::State>().var)
            .parked
            .take()
            .unwrap_or_default();
        (hash, answered)
    }
}

/// `f` with `BG(serialize_lock)` held, as php holds it around the magic
/// methods a serializer or an unserializer calls.
fn locked<R>(req: &mut Request, f: impl FnOnce(&mut Request) -> R) -> R {
    req.state::<crate::State>().var.lock += 1;
    let answered = f(req);
    let nesting = &mut req.state::<crate::State>().var;
    nesting.lock = nesting.lock.saturating_sub(1);
    answered
}

/// `PHP_IC_ENTRY`'s name, the class an object of a class `unserialize()`
/// could not load becomes.
pub(crate) const INCOMPLETE_CLASS: &[u8] = b"__PHP_Incomplete_Class";

/// `MAGIC_MEMBER`, the property an incomplete object keeps its class's
/// name in.
pub(crate) const MAGIC_MEMBER: &[u8] = b"__PHP_Incomplete_Class_Name";

/// The object's class name as the linked table spells it.
fn class_name(req: &Request, object: php_ext::values::PhpObject) -> Vec<u8> {
    req.symbols()
        .class_decl(object.class())
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default()
}

/// Whether the class, or a class it extends, declares `method`, compared
/// without case as php's function table is keyed.
fn has_method(req: &mut Request, class: php_ext::values::ClassId, method: &[u8]) -> bool {
    req.symbols()
        .method_row(class, method)
        .and_then(|row| req.symbols().row_method(row))
        .is_some_and(|method| !method.is_abstract)
}

/// Whether the class is `Serializable`, php's `ce->serialize` and
/// `ce->unserialize` being set: `zend_implement_serializable` gives them
/// to every class that implements the interface, through an interface
/// that extends it or a class it extends. `unserialize()` refuses an
/// `O:` record of such a class that has no `__unserialize()`.
///
/// ```php
/// class N implements Serializable { function serialize() { return ""; } function unserialize($s) {} }
/// class B extends N { function __serialize(): array { return []; } function __unserialize(array $a): void { echo "both\n"; } }
/// var_dump(unserialize('O:1:"N":0:{}'), unserialize('O:1:"B":0:{}') instanceof B);
/// ```
/// ```expect
/// Deprecated: N implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary) in %s on line 2
///
/// Warning: Erroneous data format for unserializing 'N' in %s on line 4
///
/// Warning: unserialize(): Error at offset 11 of 12 bytes in %s on line 4
/// both
/// bool(false)
/// bool(true)
/// ```
fn is_serializable(req: &mut Request, class: php_ext::values::ClassId) -> bool {
    php_ext::call::lookup_class(req, b"Serializable")
        .ok()
        .flatten()
        .is_some_and(|serializable| req.symbols().instance_of(class, serializable))
}

/// `ce->ce_flags & ZEND_ACC_NOT_SERIALIZABLE`, the refusal
/// `php_var_serialize_intern` and `object_common` raise before anything
/// else of an object: the class, or a class it extends, declares
/// `not_serializable`, which `zend_do_inheritance_ex` passes down and
/// `Symbols::class_flags` answers.
/// `Some` of the class's name as php's `%s` prints `ce->name`, up to
/// its first NUL, so an anonymous class is `class@anonymous`.
///
/// ```php
/// foreach ([(function () { yield 1; })(), new WeakMap, new class extends ArrayObject {}] as $o) {
///     try { serialize($o); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// try { unserialize('a:1:{i:0;O:7:"WeakMap":0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// echo serialize(new ArrayObject([1])), "\n";
/// ```
/// ```expect
/// Serialization of 'Generator' is not allowed
/// Serialization of 'WeakMap' is not allowed
/// Serialization of 'ArrayObject@anonymous' is not allowed
/// Unserialization of 'WeakMap' is not allowed
/// O:11:"ArrayObject":4:{i:0;i:0;i:1;a:1:{i:0;i:1;}i:2;a:0:{}i:3;N;}
/// ```
fn not_serializable(req: &mut Request, class: php_ext::values::ClassId) -> Option<String> {
    if !req.symbols().class_flags(class)?.not_serializable {
        return None;
    }
    let name = &req.symbols().class_decl(class)?.name.0;
    Some(String::from_utf8_lossy(php_ext::coerce::c_str(name)).into_owned())
}

/// Whether the object is an enum case.
fn is_enum(req: &Request, object: php_ext::values::PhpObject) -> bool {
    is_enum_class(req, object.class())
}

fn is_enum_class(req: &Request, class: php_ext::values::ClassId) -> bool {
    req.symbols()
        .class_decl(class)
        .is_some_and(|decl| matches!(decl.kind, ClassKind::Enum))
}

/// `__PHP_Incomplete_Class`, what `unserialize()` makes an object of a
/// class it cannot load: no member, every property dynamic.
///
/// ```php
/// $o = unserialize('O:3:"Foo":1:{s:1:"a";i:1;}'); var_dump($o, get_class($o), serialize($o));
/// ```
/// ```expect
/// object(__PHP_Incomplete_Class)#1 (2) {
///   ["__PHP_Incomplete_Class_Name"]=>
///   string(3) "Foo"
///   ["a"]=>
///   int(1)
/// }
/// string(22) "__PHP_Incomplete_Class"
/// string(26) "O:3:"Foo":1:{s:1:"a";i:1;}"
/// ```
///
/// Its handlers are php's `php_incomplete_object_handlers`
/// (`Z/ext/standard/incomplete_class.c`): a property read or an `isset`
/// warns and finds nothing, and a write or an `unset` is php's `Error`,
/// each naming the class the object stands for.
///
/// ```php
/// $o = unserialize('O:3:"Foo":1:{s:1:"a";i:1;}');
/// var_dump($o->a, isset($o->a));
/// try { $o->b = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { unset($o->a); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: main(): The script tried to access a property on an incomplete object. Please ensure that the class definition "Foo" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition in %s on line 3
///
/// Warning: main(): The script tried to access a property on an incomplete object. Please ensure that the class definition "Foo" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition in %s on line 3
/// NULL
/// bool(false)
/// The script tried to modify a property on an incomplete object. Please ensure that the class definition "Foo" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition
/// The script tried to modify a property on an incomplete object. Please ensure that the class definition "Foo" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition
/// ```
#[php_class(final, dynamic_properties = allowed, handlers = INCOMPLETE)]
pub(crate) struct __PHP_Incomplete_Class;

#[php_class]
impl __PHP_Incomplete_Class {}

/// `php_incomplete_object_handlers`.
pub(crate) struct Incomplete;

pub(crate) static INCOMPLETE: Incomplete = Incomplete;

/// `INCOMPLETE_CLASS_MSG` over what the program tried and the class the
/// object stands for, `php_lookup_class_name`'s, `unknown` without one.
fn incomplete_message(req: &Request, object: php_ext::values::PhpObject, what: &str) -> String {
    let name = object
        .prop(req, MAGIC_MEMBER)
        .and_then(|v| {
            v.as_str_bytes()
                .map(|b| String::from_utf8_lossy(b).into_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    format!(
        "The script tried to {what} on an incomplete object. Please ensure that the class definition \"{name}\" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition"
    )
}

impl ObjectHandlers for Incomplete {
    fn read_property(
        &self,
        req: &mut Request,
        object: php_ext::values::PhpObject,
        _name: &[u8],
        _read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        let message = incomplete_message(req, object, "access a property");
        docref(req, Level::Warning, &message);
        Ok(Owner::null())
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: php_ext::values::PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        value.give_up(req);
        name.give_up(req);
        let message = incomplete_message(req, object, "modify a property");
        Err(throw_error(req, "Error", &message))
    }

    /// `incomplete_class_get_property_ptr_ptr`: every place refused with
    /// the "modify a property" `Error`, so a write by reference, a
    /// compound write and an increment each meet it, as a plain write
    /// does.
    ///
    /// ```php
    /// $o = unserialize('O:1:"C":1:{s:1:"p";i:1;}');
    /// $x = 1;
    /// foreach ([fn () => $o->p = &$x, fn () => $o->p .= 'x', fn () => $o->p++] as $f) {
    ///     try { $f(); } catch (Error $e) { echo substr($e->getMessage(), 0, 50), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// The script tried to modify a property on an incomp
    /// The script tried to modify a property on an incomp
    /// The script tried to modify a property on an incomp
    /// ```
    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: php_ext::values::PhpObject,
        _name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        let message = incomplete_message(req, object, "modify a property");
        Err(throw_error(req, "Error", &message))
    }

    /// `incomplete_class_get_method`: every method refused with the
    /// "call a method" `Error`, before any lookup.
    ///
    /// ```php
    /// $o = unserialize('O:1:"C":0:{}');
    /// try { $o->f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// The script tried to call a method on an incomplete object. Please ensure that the class definition "C" of the object you are trying to operate on was loaded _before_ unserialize() gets called or provide an autoloader to load the class definition
    /// ```
    fn get_method(
        &self,
        req: &mut Request,
        object: php_ext::values::PhpObject,
        _lowered: &[u8],
    ) -> Result<Option<php_ext::call::Function>, Thrown> {
        let message = incomplete_message(req, object, "call a method");
        Err(throw_error(req, "Error", &message))
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: php_ext::values::PhpObject,
        _name: &[u8],
        _test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let message = incomplete_message(req, object, "access a property");
        docref(req, Level::Warning, &message);
        Ok(false)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: php_ext::values::PhpObject,
        _name: &[u8],
    ) -> Result<(), Thrown> {
        let message = incomplete_message(req, object, "modify a property");
        Err(throw_error(req, "Error", &message))
    }
}

/// The ini entries the unserializer reads, `basic_functions.c`'s
/// `unserialize_callback_func` and `unserialize_max_depth`.
///
/// `var.c`'s one entry; `unserialize_callback_func`, which
/// `unserialize()` reads too, is `main.c`'s.
///
/// ```php
/// var_dump(ini_get("unserialize_max_depth"), ini_get("unserialize_callback_func"));
/// ```
/// ```expect
/// string(4) "4096"
/// string(0) ""
/// ```
pub(crate) fn ini() -> Vec<php_ext::decl::IniDecl> {
    vec![php_ext::decl::IniDecl {
        name: php_ext::decl::Name(b"unserialize_max_depth".as_slice().into()),
        default: Some(b"4096".as_slice().into()),
        user: true,
        per_dir: true,
        system: true,
    }]
}

/// `PRINT_ZVAL_INDENT` (`Z/Zend/zend.c:80`): the four spaces a level of
/// the rendering steps in by, taken twice between a key and the array
/// its value opens.
const INDENT: usize = 4;

/// `print_r(mixed $value, bool $return = false): string|true`, php's
/// `PHP_FUNCTION(print_r)`: the rendering written where `echo` writes it
/// and `true` answered, or, under `$return`, the rendering itself and
/// nothing written. A scalar is what `zval_get_string` makes of it, so
/// `null` and `false` render as nothing at all and a float renders at
/// `precision` rather than at `serialize_precision`.
///
/// ```php
/// print_r("s"); print_r(1); print_r(1.5); print_r(true); print_r(false); print_r(null); echo "|\n";
/// var_dump(print_r(0.1, true), print_r("x", false));
/// ```
/// ```expect
/// s11.51|
/// xstring(3) "0.1"
/// bool(true)
/// ```
///
/// An array is `Array` and its elements under it, each key in brackets
/// and each value four spaces further in, and an element that opens an
/// array of its own leaves the blank line php's `print_hash` writes
/// after the `)` of a nested one.
///
/// ```php
/// print_r([1, "two" => [3, [], "k" => "v"]]);
/// ```
/// ```expect
/// Array
/// (
///     [0] => 1
///     [two] => Array
///         (
///             [0] => 3
///             [1] => Array
///                 (
///                 )
///
///             [k] => v
///         )
///
/// )
/// ```
///
/// An object is its class and `Object`, its properties under it with the
/// visibility php mangles into the name unmangled back out,
/// `[p:protected]` and `[p:C:private]` naming the class that declared
/// the private one, an inherited property before the class's own, and
/// the dynamic ones after. A value that holds itself is ` *RECURSION*`
/// and not an unbounded walk.
///
/// ```php
/// class A { public $pub = 1; protected $prot = "p"; private $priv = 2.5; }
/// class B extends A { public $own = [1]; }
/// $b = new B; $b->dyn = "d"; $b->self = $b;
/// print_r($b);
/// print_r(new stdClass);
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property B::$dyn is deprecated in %s on line 4
///
/// Deprecated: Creation of dynamic property B::$self is deprecated in %s on line 4
/// B Object
/// (
///     [pub] => 1
///     [prot:protected] => p
///     [priv:A:private] => 2.5
///     [own] => Array
///         (
///             [0] => 1
///         )
///
///     [dyn] => d
///     [self] => B Object
///  *RECURSION*
/// )
/// stdClass Object
/// (
/// )
/// ```
///
/// The array half of the guard is `GC_PROTECTED` on the array itself,
/// which a program reaches by binding an entry to the variable that
/// holds the array (`$a[] = &$a`): the entry is the cell the CV holds,
/// the cell holds the array, and the array is the one the walk is
/// already inside. php writes the marker after the `Array` it has
/// already printed, so it lands on the next line with one space before
/// it and the rows never open.
///
/// ```php
/// $a = []; $a[] = &$a; print_r($a);
/// $b = [1]; $b["self"] = &$b; print_r($b);
/// ```
/// ```expect
/// Array
/// (
///     [0] => Array
///  *RECURSION*
/// )
/// Array
/// (
///     [0] => 1
///     [self] => Array
///  *RECURSION*
/// )
/// ```
///
/// The object half is the same mark on the object itself, which the one
/// line `$a->prop = $a` sets up, and the rows of the second visit never
/// open.
///
/// ```php
/// $a = new stdClass; $a->prop = $a; print_r($a);
/// ```
/// ```expect
/// stdClass Object
/// (
///     [prop] => stdClass Object
///  *RECURSION*
/// )
/// ```
///
/// `$return = true` hands the same bytes back as a string rather than
/// writing them, which is what a caller that renders into a page does.
///
/// ```php
/// class C { private $p = 1; }
/// var_dump(print_r(new C, true), print_r([1 => [2]], true));
/// $v = ['k' => [1, 2]];
/// print_r($v);
/// var_dump(print_r($v, true) === "Array\n(\n    [k] => Array\n        (\n            [0] => 1\n            [1] => 2\n        )\n\n)\n");
/// ```
/// ```expect
/// string(36) "C Object
/// (
///     [p:C:private] => 1
/// )
/// "
/// string(69) "Array
/// (
///     [1] => Array
///         (
///             [0] => 2
///         )
///
/// )
/// "
/// Array
/// (
///     [k] => Array
///         (
///             [0] => 1
///             [1] => 2
///         )
///
/// )
/// bool(true)
/// ```
///
/// ```php
/// print_r();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: print_r() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): print_r()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// echo new ReflectionFunction('print_r');
/// ```
/// ```expect
/// Function [ <internal:standard> function print_r ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> bool $return = false ]
///   }
///   - Return [ string|true ]
/// }
/// ```
///
/// A class's debug table that leaves a throwable pending is printed
/// whole with the rows after it, and the throwable is thrown after:
///
/// ```php
/// $n = (new ReflectionClass('DOMNode'))->newInstanceWithoutConstructor();
/// try { print_r([$n, 1]); } catch (DOMException $e) { for ($i = 0; $e; $e = $e->getPrevious()) $i++; echo $i, "\n"; }
/// ```
/// ```expect
/// Array
/// (
///     [0] => DOMNode Object
///         (
///         )
///
///     [1] => 1
/// )
/// 18
/// ```
///
/// An object's rows are walked as `print_hash` walks the table
/// `zend_get_properties_for` holds: a nested `__debugInfo` that unsets
/// a dynamic row ahead separates the table, and the walk prints the row
/// it began with, never the freed string:
///
/// ```php
/// #[AllowDynamicProperties]
/// class O { public int $u; public $b = 'b'; }
/// class N { function __debugInfo() { global $o; unset($o->d); $o->b = str_repeat('c', 3); $GLOBALS['x'] = str_repeat('x', 3); return []; } }
/// $o = new O; $o->a = new N; $o->d = str_repeat('d', 3); $o->e = 'e';
/// print_r($o);
/// ```
/// ```expect
/// O Object
/// (
///     [b] => b
///     [a] => N Object
///         (
///         )
///
///     [d] => ddd
///     [e] => e
/// )
/// ```
///
/// An array's rows are walked as `print_hash` walks them, holding no
/// owner of a row: a count a `__debugInfo` reads of its own object is
/// php's, the array's and the call's (#2159).
///
/// ```php
/// class F { public $x = 1; function __debugInfo() { debug_zval_dump($this); return []; } }
/// $s = [new F];
/// print_r($s);
/// ```
/// ```expect
/// *RECURSION*
/// object(F)#1 (0) refcount(2){
/// }
/// Array
/// (
///     [0] => F Object
///         (
///         )
///
/// )
/// ```
///
/// The object's guard is cleared before the table is released, so a
/// destructor the release serves prints the object whole:
///
/// ```php
/// class D { function __destruct() { global $o, $n; if ($n++ < 1) { echo "dtor:"; print_r($o); } } }
/// class O { public $x = 1; function __debugInfo() { return ['d' => new D]; } }
/// $n = 0; $o = new O;
/// print_r($o);
/// ```
/// ```expect
/// dtor:O Object
/// (
///     [d] => D Object
///         (
///         )
///
/// )
/// O Object
/// (
///     [d] => D Object
///         (
///         )
///
/// )
/// ```
///
/// The string `print_r($v, true)` returns is the `smart_str` php built
/// it in, `RETURN_STR(zend_print_zval_r_to_str(var, 0))`, whose
/// `smart_str_0` keeps the buffer's allocation as it stands: 256 bytes
/// for anything shorter than a page's first fill, a scalar's empty
/// string among them, and the page past it for more:
///
/// ```php
/// $m = memory_get_usage(); $x = print_r([1], true); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $z = print_r(1, true); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $n = print_r(null, true); echo memory_get_usage() - $m, "\n";
/// $s = str_repeat('s', 231); $t = str_repeat('t', 232);
/// $m = memory_get_usage(); $y = print_r($s, true); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $w = print_r($t, true); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $v = print_r(array_fill(0, 30, 1), true); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 256
/// 256
/// 256
/// 256
/// 4096
/// 4096
/// ```
#[php_function]
fn print_r(
    req: &mut Request,
    value: Value,
    #[php(default = false)] r#return: bool,
) -> Result<Union<Owner<PhpString>, True>, Thrown> {
    let returned = r#return;
    let mut out = Vec::new();
    // A throwable a class's debug table left pending is thrown once the
    // whole value is printed, as `php_print_zval_r` leaves it in
    // `EG(exception)`; a fatal or an exit prints nothing.
    if let Err(thrown) = render(req, &mut out, &value, 0) {
        if thrown.exception().is_some() && !returned {
            req.response_mut().write(&out);
        }
        return Err(thrown);
    }
    if returned {
        return returned_string(req, &value, &out).map(Union::A);
    }
    req.response_mut().write(&out);
    Ok(Union::B(True))
}

/// The rendering as the `smart_str` `zend_print_zval_r_to_buf` appends
/// it into: a scalar's string in one `smart_str_append`, a table's in
/// many. What the allocation comes to depends on the length at the end
/// and on whether the first append was all of it, since each growth is
/// to the page past the length it is asked at, so a table's rendering is
/// appended as its first byte and then the rest, which grows the
/// buffer where php's many appends grow it.
fn returned_string(
    req: &mut Request,
    value: &Value,
    out: &[u8],
) -> Result<Owner<PhpString>, Thrown> {
    let table = matches!(
        value.deref().tag(),
        php_ext::values::TypeTag::Array | php_ext::values::TypeTag::Object
    );
    let pieces: &[&[u8]] = match out.split_first() {
        Some((_, rest)) if table && !rest.is_empty() => &[&out[..1], rest],
        _ => &[out],
    };
    let mut buf = SmartStr::default();
    for piece in pieces {
        if let Err(thrown) = buf.append(req, piece) {
            buf.free(req);
            return Err(thrown);
        }
    }
    buf.finish(req)
}

/// One value as `zend_print_zval_r_to_buf(buf, expr, indent)` renders
/// it: an array and an object open their name and print their rows as
/// `print_hash` does ([`Printed`]), each walked with a foreach walk, a
/// reference is what the cell holds, a float is
/// `smart_str_append_double` at `precision`, which raises nothing for a
/// NAN since 8.5.12 (`Z/Zend/zend.c:600-602`, GH-23662), and everything
/// else is
/// `zval_get_string`, which is `1` for `true` and nothing at all for
/// `false` and `null`.
///
/// ```php
/// print_r(NAN); print_r([NAN, INF, -0.0, 0.1 + 0.2, 1e100]);
/// ini_set('precision', '0'); print_r(1.5); echo "\n";
/// ini_set('precision', '-1'); print_r(0.1 + 0.2); echo "\n";
/// ```
/// ```expect
/// NANArray
/// (
///     [0] => NAN
///     [1] => INF
///     [2] => -0
///     [3] => 0.3
///     [4] => 1.0E+100
/// )
/// 2
/// 0.30000000000000004
/// ```
///
/// The array and the object arms carry php's recursion guard, the mark
/// set on what the walk is inside and cleared on the way out, and a
/// value already marked is ` *RECURSION*` where its rows would have
/// stood — after the `Array` or the `Object` php has already written,
/// which is why the marker lands on the next line with one space before
/// it. Without the guard a value that holds itself is unbounded
/// recursion, which is a panic reaching a request (design §10).
///
/// php asks no stack limit here, so a nest deeper than a small
/// `zend.max_allowed_stack_size` is printed whole, and this walk is
/// held to the thread's floor instead
/// ([`php_ext::raising::call_stack_end`]), which the small-stack
/// regime's 256K does not move.
///
/// ```php
/// $a = []; for ($i = 0; $i < 600; $i++) { $a = [$a]; }
/// echo strlen(print_r($a, true)), "\n";
/// ```
/// ```expect regime=small-stack
/// 4335610
/// ```
pub(super) fn render(
    req: &mut Request,
    out: &mut Vec<u8>,
    value: &Value,
    indent: usize,
) -> Result<(), Thrown> {
    // `case IS_REFERENCE`: php renders what the cell holds, at this
    // level and with no mark of its own.
    let value = value.deref();
    // php asks no stack limit here and runs to the thread's end; this
    // walk is held to the floor short of it.
    call_stack_end(req)?;
    if let Some(array) = value.as_array() {
        out.extend_from_slice(b"Array\n");
        // `GC_IS_RECURSIVE` and `GC_PROTECT_RECURSION`
        // (`Z/Zend/zend.c:554`), which php takes only where the array is
        // refcounted, and a walk that holds nothing: where the program
        // gives the array up under the walk, the walk ends there, and the
        // `)` is written after the rows printed. The `(` is laid down
        // before the walk and taken back where the walk found the table
        // marked, since that walk wrote nothing after it.
        let opened = out.len();
        let mut printed = Printed::open(out, indent, false);
        let walked = walk::foreach_guarded(req, array, Hold::None, |req, row| {
            printed.row(req, out, row.key(), row.value())?;
            Ok(ControlFlow::<()>::Continue(()))
        })?;
        if walked == Guarded::Recursive {
            out.truncate(opened);
            out.extend_from_slice(b" *RECURSION*");
            return Ok(());
        }
        return printed.close(out);
    }
    if let Some(object) = value.as_object() {
        let Some(decl) = req.symbols().class_decl(object.class()) else {
            return Ok(());
        };
        let (kind, backing) = (decl.kind, decl.backing);
        // `Z_OBJ_HANDLER_P(expr, get_class_name)`.
        let name = object.handlers(req).get_class_name(req, object);
        // `smart_str_appends(buf, ZSTR_VAL(class_name))`: up to the NUL
        // of an anonymous class's name.
        out.extend_from_slice(php_ext::coerce::c_str(&name));
        // `ZEND_ACC_ENUM`: a case is `Suit Enum:string` and its `name`
        // and `value` under it, where an object is `C Object`.
        if matches!(kind, ClassKind::Enum) {
            out.extend_from_slice(b" Enum");
            match backing {
                EnumBacking::Int => out.extend_from_slice(b":int"),
                EnumBacking::String => out.extend_from_slice(b":string"),
                EnumBacking::Pure => {}
            }
            out.push(b'\n');
        } else {
            out.extend_from_slice(b" Object\n");
        }
        if object.is_recursive_in(req, RecursionGuard::Debug) {
            out.extend_from_slice(b" *RECURSION*");
            return Ok(());
        }
        // `zend_get_properties_for(struc, ZEND_PROP_PURPOSE_DEBUG)`: the
        // table a class shows in place of its properties, which is what
        // `__debugInfo` and a `Closure` show, and the object's own table
        // otherwise, held until its rows are rendered, php's
        // `zend_release_properties`. A typed property nothing has
        // written is no row of php's walk (`ZEND_HASH_FOREACH_KEY_VAL_IND`).
        let held = object.properties_for(req, PropertyPurpose::Debug)?;
        object.protect_in(req, RecursionGuard::Debug);
        let mut printed = Printed::open(out, indent, true);
        let walked = walk::properties(req, &held, Unwritten::Skipped, |req, row| {
            printed.row(req, out, row.key(), row.value())?;
            Ok(ControlFlow::<()>::Continue(()))
        });
        // The guard cleared before the release, as php's
        // `zend_release_properties` follows its unprotect, so a
        // destructor the release serves prints the object whole.
        object.unprotect_in(req, RecursionGuard::Debug);
        let released = held.release(req);
        // What the table left pending was raised before any row's, so a
        // row's is chained onto it; a fatal a row raised ends the print
        // where it stands.
        let rendered = walked.and_then(|_| printed.close(out));
        return match (released, rendered) {
            (Err(earlier), Err(later)) => Err(earlier.chain(req, later)),
            (Err(thrown), Ok(())) | (Ok(()), Err(thrown)) => Err(thrown),
            (Ok(()), Ok(())) => Ok(()),
        };
    }
    // `case IS_DOUBLE`: `smart_str_append_double(buf, d, EG(precision),
    // false)`, the text `zend_double_to_str` makes without its warning.
    if let Some(f) = value.as_float() {
        let text = float_format(f, req.ini().precision(), Exponent::Upper);
        out.extend_from_slice(text.as_bytes());
        return Ok(());
    }
    // `zval_get_string_func`, whose string is the caller's to release:
    // the decimal of an int, the bytes of a string, `1` for `true`, the
    // empty string for `false` and for `null`, and a resource's
    // `Resource id #N`.
    with_string(req, &value, |string| out.extend_from_slice(string))
}

/// One `print_hash` as it is written: its indent, whether its keys are
/// an object's, and the throwable a row left pending.
struct Printed {
    indent: usize,
    is_object: bool,
    /// php checks no `EG(exception)` between rows, so a throwable an
    /// element's debug table left pending waits while the rest print, a
    /// later one chained onto it.
    pending: Option<Thrown>,
}

impl Printed {
    /// The `(` and the newline after it.
    fn open(out: &mut Vec<u8>, indent: usize, is_object: bool) -> Printed {
        out.extend(std::iter::repeat_n(b' ', indent));
        out.extend_from_slice(b"(\n");
        Printed {
            indent,
            is_object,
            pending: None,
        }
    }

    /// One row, `[key] => ` and its value, and the newline after it; the
    /// `Err` is a fatal, which ends the walk.
    fn row(
        &mut self,
        req: &mut Request,
        out: &mut Vec<u8>,
        key: &Key,
        element: &Value,
    ) -> Result<(), Thrown> {
        let indent = self.indent;
        out.extend(std::iter::repeat_n(b' ', indent + INDENT));
        out.push(b'[');
        match key {
            Key::Int(n) => out.extend_from_slice(Decimal::of(*n).as_bytes()),
            Key::Str(s) if self.is_object => unmangled(out, s.as_bytes()),
            Key::Str(s) => out.extend_from_slice(s.as_bytes()),
        }
        out.extend_from_slice(b"] => ");
        if let Err(thrown) = render(req, out, element, indent + INDENT + INDENT) {
            let thrown = match self.pending.take() {
                Some(earlier) => earlier.chain(req, thrown),
                None => thrown,
            };
            if thrown.exception().is_none() {
                return Err(thrown);
            }
            self.pending = Some(thrown);
        }
        out.push(b'\n');
        Ok(())
    }

    /// The `)` and the newline after it, and what a row left pending.
    fn close(self, out: &mut Vec<u8>) -> Result<(), Thrown> {
        out.extend(std::iter::repeat_n(b' ', self.indent));
        out.extend_from_slice(b")\n");
        self.pending.map_or(Ok(()), Err)
    }
}

/// `zend_unmangle_property_name_ex`: the class a mangled name carries
/// and the name itself, `*` for the class of a protected property and
/// the declaring class for a private one; a name that is not mangled,
/// a public property's and a dynamic one's, carries no class. A private
/// property of an anonymous class is mangled with a name that holds a
/// NUL of its own, so where a third NUL follows, the class runs through
/// the second (`anonclass_src_len`).
fn unmangle(key: &[u8]) -> (Option<&[u8]>, &[u8]) {
    let Some(rest) = key.strip_prefix(b"\0") else {
        return (None, key);
    };
    let Some(end) = rest.iter().position(|b| *b == 0) else {
        return (None, key);
    };
    let end = match rest[end + 1..].iter().position(|b| *b == 0) {
        Some(source) => end + 1 + source,
        None => end,
    };
    (Some(&rest[..end]), &rest[end + 1..])
}

/// The name `print_hash` prints for an object's row: the unmangled name,
/// and the visibility php spells after it where the key carried a class,
/// the class up to its NUL (`smart_str_appends`).
///
/// ```php
/// print_r(new class { private $p = 1; protected $q = 2; });
/// ```
/// ```expect
/// class@anonymous Object
/// (
///     [p:class@anonymous:private] => 1
///     [q:protected] => 2
/// )
/// ```
fn unmangled(out: &mut Vec<u8>, key: &[u8]) {
    let (class, name) = unmangle(key);
    out.extend_from_slice(name);
    match class {
        None => {}
        Some(b"*") => out.extend_from_slice(b":protected"),
        Some(class) => {
            out.push(b':');
            out.extend_from_slice(php_ext::coerce::c_str(class));
            out.extend_from_slice(b":private");
        }
    }
}

/// `var_export(mixed $value, bool $return = false): ?string`, php's
/// `PHP_FUNCTION(var_export)` over `php_var_export_ex`
/// (`Z/ext/standard/var.c:516`): the value as PHP source that evaluates
/// back to it, written where `echo` writes it and `null` answered, or,
/// under `$return`, the source itself. A string is quoted with `'` and
/// `\` escaped as `php_addcslashes` escapes them and a NUL spliced out
/// as `' . "\0" . '`; a float is `serialize_precision` with `.0` after a
/// whole one, so what is printed reads back as the same double;
/// `PHP_INT_MIN` is `-9223372036854775807-1`, since the number written
/// whole would read back as a float.
///
/// ```php
/// var_export(1); echo "|"; var_export(1.0); echo "|"; var_export(0.1); echo "|"; var_export(true); echo "|"; var_export(false); echo "|"; var_export(null); echo "|"; var_export("it's\\a"); echo "|"; var_export(PHP_INT_MIN); echo "|"; var_export("a\0b");
/// ```
/// ```expect
/// 1|1.0|0.1|true|false|NULL|'it\'s\\a'|-9223372036854775807-1|'a' . "\0" . 'b'
/// ```
///
/// An array is `array (` and its elements, each key two spaces in and
/// each nested array opening on a line of its own; an object is
/// `\Class::__set_state(array(` with its properties three spaces in
/// under their unmangled names, and a `stdClass` is `(object) array(`,
/// which is what php writes for the one class with no `__set_state`.
///
/// ```php
/// var_export([1, 'k' => [2, 3], 'x' => 'y']); echo "\n";
/// class A { public $a = 1; protected $b = 2; private $c = 3; }
/// var_export(new A); echo "\n";
/// var_export(new stdClass);
/// ```
/// ```expect
/// array (
///   0 => 1,
///   'k' =>%w
///   array (
///     0 => 2,
///     1 => 3,
///   ),
///   'x' => 'y',
/// )
/// \A::__set_state(array(
///    'a' => 1,
///    'b' => 2,
///    'c' => 3,
/// ))
/// (object) array(
/// )
/// ```
///
/// A value that holds itself is where the two dumpers part, and the
/// difference is php's: `print_r` prints ` *RECURSION*` and goes on,
/// `var_export` prints `NULL` in its place and raises the warning
/// `php_var_export_ex` raises there, pinned below, since what it writes
/// has to be source that evaluates.
///
/// ```php
/// $o = new stdClass; $o->p = $o; var_export($o); echo "\n";
/// $a = [1]; $a[] = &$a; var_export($a); echo "\n";
/// var_dump(var_export([1], true), var_export(1, true));
/// ```
/// ```expect
/// Warning: var_export does not handle circular references in %s on line 2
/// (object) array(
///    'p' => NULL,
/// )
///
/// Warning: var_export does not handle circular references in %s on line 3
/// array (
///   0 => 1,
///   1 => NULL,
/// )
/// string(19) "array (
///   0 => 1,
/// )"
/// string(1) "1"
/// ```
///
/// ```php
/// echo new ReflectionFunction('var_export');
/// ```
/// ```expect
/// Function [ <internal:standard> function var_export ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> bool $return = false ]
///   }
///   - Return [ ?string ]
/// }
/// ```
///
/// The export is built in one `smart_str` on the request's heap and handed
/// over trimmed, `smart_str_extract`, and what php allocates on the way,
/// each indent's `spprintf` and a string's `php_addcslashes` and
/// `php_str_to_str`, is held while it is appended, so the peak and what
/// stays are php's:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $r = $f(); return [memory_get_peak_usage() - $s, memory_get_usage() - $s]; }
/// foreach ([[1, 'a' => 2], range(1, 40), str_repeat("x'\\\0", 100), ['k' => str_repeat('y', 3000)], (object) ['p' => 1.5, 'q' => [true, null]], 1.0, "plain"] as $i => $v) {
///     [$p, $u] = pk(fn() => var_export($v, true));
///     echo $i, ' ', $p, ' ', $u, "\n";
/// }
/// ob_start(); $s = memory_get_usage(); memory_reset_peak_usage(); var_export(range(1, 40)); $p = memory_get_peak_usage() - $s; ob_end_clean(); echo "print ", $p, "\n";
/// ```
/// ```expect
/// 0 584 280
/// 1 4352 728
/// 2 6528 2008
/// 3 12544 3288
/// 4 512 344
/// 5 296 256
/// 6 328 256
/// print 5688
/// ```
#[php_function]
fn var_export(
    req: &mut Request,
    value: Value,
    #[php(default = false)] r#return: bool,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let returned = r#return;
    // One `smart_str` for the whole export, on the request's heap.
    let mut out = SmartStr::default();
    if let Err(thrown) = export(req, &mut out, &value, 1) {
        out.free(req);
        return Err(thrown);
    }
    // What a reader of an object's properties left pending, a lazy
    // object's initializer that threw, is thrown once what was read is
    // written, as php's `EG(exception)` is after `PHPWRITE`.
    if returned {
        // `RETURN_STR(smart_str_extract(&buf))`: the buffer is the string.
        let string = out.extract(req)?;
        if let Err(thrown) = pending(req) {
            string.give_up(req);
            return Err(thrown);
        }
        return Ok(Some(string));
    }
    req.response_mut().write(out.as_bytes());
    out.free(req);
    pending(req)?;
    Ok(None)
}

/// One value as `php_var_export_ex(struc, level, buf)` writes it, and a
/// reference as what the cell holds, php's `goto again`. `level` is
/// php's: the top level is one, an array's elements are exported at two
/// more than their array's, and what is written before the `array (` or
/// the `(object) array(` of a nested one is the newline and the indent
/// php only writes past level one.
///
/// Both compound arms carry php's `EXPORT` guard rather than the
/// `DEBUG` one `print_r` takes, and answer a value already inside the
/// walk with `NULL` and the warning, which is what keeps the source it
/// writes evaluable.
///
/// The array arm gives up the owner it held for its walk with
/// `GC_DELREF(myht)`, which leaves no possible root of the cycle
/// collector, where `var_dump`'s `GC_DTOR_NO_REF` leaves one:
///
/// ```php
/// foreach (['var_dump', 'debug_zval_dump', 'var_export'] as $f) {
///     $a = [range(1, 2), range(3, 4)]; $b = [$a];
///     gc_collect_cycles(); $r = gc_status()['roots'];
///     ob_start(); $f($b); ob_end_clean();
///     echo $f, ' ', gc_status()['roots'] - $r, "\n";
///     unset($a, $b); gc_collect_cycles();
/// }
/// ```
/// ```expect
/// var_dump 4
/// debug_zval_dump 4
/// var_export 0
/// ```
///
/// # Errors
///
/// What a property read or a lazy object's initializer threw.
pub fn export(
    req: &mut Request,
    out: &mut impl ExportOut,
    value: &Value,
    level: usize,
) -> Result<(), Thrown> {
    // Unguarded in php, held to the thread's floor here.
    call_stack_end(req)?;
    let value = value.deref();
    if let Some(b) = value.as_bool() {
        out.put(req, if b { b"true" } else { b"false" })?;
    } else if let Some(n) = value.as_int() {
        // `ZEND_LONG_MIN` written whole would read back as a float.
        if n == i64::MIN {
            put_long(req, out, i64::MIN + 1)?;
            out.put(req, b"-1")?;
        } else {
            put_long(req, out, n)?;
        }
    } else if let Some(f) = value.as_float() {
        // `smart_str_append_double` with `zero_fraction`: the text, and
        // `.0` after a finite one that carries no point of its own.
        let text = float_format(f, req.ini().serialize_precision(), Exponent::Upper);
        out.put(req, text.as_bytes())?;
        if f.is_finite() && !text.contains('.') {
            out.put(req, b".0")?;
        }
    } else if let Some(s) = value.as_php_string() {
        let temps = Escaped::new(req, s.as_bytes())?;
        let put = quoted(req, out, s.as_bytes());
        temps.free(req);
        put?;
    } else if let Some(array) = value.as_array() {
        // `GC_IS_RECURSIVE`, then `GC_ADDREF(myht)` beside
        // `GC_PROTECT_RECURSION(myht)` and `ZEND_HASH_FOREACH_KEY_VAL`
        // (`Z/ext/standard/var.c:560`): a foreach walk holding the array and
        // guarding its recursion, so a user error handler the circular
        // warning or an object's export reaches that assigns over the
        // array's holder leaves it standing until the walk is done, its
        // owner given up as `GC_DELREF(myht)` (`:587`), no root. The
        // text is laid down before the walk and taken back where the walk
        // found the array marked, since php writes `NULL` in its place.
        let opened = out.written();
        if level > 1 {
            out.put(req, b"\n")?;
            spaces(req, out, level - 1)?;
        }
        out.put(req, b"array (\n")?;
        let held = Hold::Owner(GiveUp::DtorNogc);
        let walked = walk::foreach_guarded(req, array, held, |req, row| -> walk::Step {
            // `php_array_element_export`: a string key escaped before
            // the indent is written.
            match row.key() {
                Key::Int(n) => {
                    spaces(req, out, level + 1)?;
                    put_long(req, out, *n)?;
                }
                Key::Str(s) => {
                    let temps = Escaped::new(req, s.as_bytes())?;
                    let put =
                        spaces(req, out, level + 1).and_then(|()| quoted(req, out, s.as_bytes()));
                    temps.free(req);
                    put?;
                }
            }
            out.put(req, b" => ")?;
            export(req, out, row.value(), level + 2)?;
            out.put(req, b",\n")?;
            Ok(ControlFlow::Continue(()))
        })?;
        if let Guarded::Recursive = walked {
            out.truncate(opened);
            out.put(req, b"NULL")?;
            circular(req);
            return Ok(());
        }
        if level > 1 {
            spaces(req, out, level - 1)?;
        }
        out.put(req, b")")?;
    } else if let Some(object) = value.as_object() {
        return export_object(req, out, object, level);
    } else {
        out.put(req, b"NULL")?;
    }
    Ok(())
}

/// `php_var_export_ex`'s `IS_OBJECT` arm: the guard first, so a class
/// whose rows are built on the way out is not asked for them inside its
/// own cycle; then `\Class::__set_state(array(` and each property under
/// its unmangled name, `(object) array(` for the one class with no
/// `__set_state`, and `\Suit::Hearts` for an enum case, which carries
/// neither its properties nor the closing pair.
///
/// The table is `zend_get_properties_for`'s, laid down where the object
/// has none, as `rebuild_object_properties` lays php's, and
/// `memory_get_usage()` counts it:
///
/// ```php
/// class K { public $d = 1; }
/// $k = new K; $m = memory_get_usage(); $v = var_export($k, true); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 456
/// ```
fn export_object(
    req: &mut Request,
    out: &mut impl ExportOut,
    object: php_ext::values::PhpObject,
    level: usize,
) -> Result<(), Thrown> {
    if object.is_recursive_in(req, RecursionGuard::Export) {
        out.put(req, b"NULL")?;
        circular(req);
        return Ok(());
    }
    object.protect_in(req, RecursionGuard::Export);
    let held = match object.properties_for(req, PropertyPurpose::VarExport) {
        Ok(held) => held,
        Err(thrown) => {
            object.unprotect_in(req, RecursionGuard::Export);
            return Err(thrown);
        }
    };
    let Some(decl) = req.symbols().class_decl(object.class()) else {
        object.unprotect_in(req, RecursionGuard::Export);
        return held.release(req);
    };
    let (name, is_enum) = (decl.name.0.to_vec(), matches!(decl.kind, ClassKind::Enum));
    let standard = name == b"stdClass";
    if level > 1 {
        out.put(req, b"\n")?;
        spaces(req, out, level - 1)?;
    }
    if standard {
        out.put(req, b"(object) array(\n")?;
    } else {
        out.put(req, b"\\")?;
        out.put(req, &name)?;
        if is_enum {
            out.put(req, b"::")?;
            let case = object
                .prop(req, b"name")
                .and_then(|name| name.as_str_bytes().map(<[u8]>::to_vec))
                .unwrap_or_default();
            out.put(req, &case)?;
        } else {
            out.put(req, b"::__set_state(array(\n")?;
        }
    }
    // `ZEND_HASH_FOREACH_KEY_VAL_IND(myht, index, key, val)` over the
    // table held until `zend_release_properties`.
    let written = if is_enum {
        Ok(None)
    } else {
        walk::properties(req, &held, Unwritten::Skipped, |req, row| {
            // A hooked row is read through its hook before it is written.
            row.read(req)?;
            spaces(req, out, level + 2)?;
            // `php_object_element_export`: a name quoted, unmangled; an
            // integer key as it stands.
            match row.key() {
                Key::Str(key) => {
                    let (_, name) = unmangle(key.as_bytes());
                    // `php_addcslashes_str`'s string, held while it is
                    // appended.
                    let (slashed, _) = addcslashed(req, name)?;
                    let put = out
                        .put(req, b"'")
                        .and_then(|()| escaped(req, out, name))
                        .and_then(|()| out.put(req, b"'"));
                    slashed.free(req);
                    put?;
                }
                Key::Int(n) => put_long(req, out, *n)?,
            }
            out.put(req, b" => ")?;
            export(req, out, row.value(), level + 2)?;
            out.put(req, b",\n")?;
            Ok(ControlFlow::<()>::Continue(()))
        })
    };
    object.unprotect_in(req, RecursionGuard::Export);
    let released = held.release(req);
    written?;
    released?;
    if level > 1 && !is_enum {
        spaces(req, out, level - 1)?;
    }
    if standard {
        out.put(req, b")")?;
    } else if !is_enum {
        out.put(req, b"))")?;
    }
    Ok(())
}

/// Where `php_var_export_ex` writes: php's `smart_str`, a [`SmartStr`] on
/// the request's heap, which `var_export` hands over as its string, or a
/// buffer of the caller's (`zend_test`'s observer).
pub trait ExportOut {
    /// `smart_str_appendl`; the `Err` is the memory limit's fatal.
    ///
    /// # Errors
    ///
    /// The memory limit's fatal.
    fn put(&mut self, req: &mut Request, bytes: &[u8]) -> Result<(), Thrown>;
    /// The bytes written so far, `ZSTR_LEN(buf->s)`.
    fn written(&self) -> usize;
    /// What was written past `len` taken back.
    fn truncate(&mut self, len: usize);
}

impl ExportOut for SmartStr {
    fn put(&mut self, req: &mut Request, bytes: &[u8]) -> Result<(), Thrown> {
        self.append(req, bytes)
    }

    fn written(&self) -> usize {
        self.as_bytes().len()
    }

    fn truncate(&mut self, len: usize) {
        let written = self.written();
        self.remove(len..written);
    }
}

impl ExportOut for Vec<u8> {
    fn put(&mut self, _: &mut Request, bytes: &[u8]) -> Result<(), Thrown> {
        self.extend_from_slice(bytes);
        Ok(())
    }

    fn written(&self) -> usize {
        self.len()
    }

    fn truncate(&mut self, len: usize) {
        Vec::truncate(self, len);
    }
}

/// `buffer_append_spaces` (`Z/ext/standard/var.c:450`): `n` spaces,
/// made as php makes them, `spprintf("%*c")` into a buffer of its own,
/// appended and then freed.
fn spaces(req: &mut Request, out: &mut impl ExportOut, n: usize) -> Result<(), Thrown> {
    const SPACES: [u8; 32] = [b' '; 32];
    let printed = Emalloc::printf_buffer(req, n)?;
    let mut left = n;
    let mut put = Ok(());
    while left > 0 && put.is_ok() {
        let take = left.min(SPACES.len());
        put = out.put(req, &SPACES[..take]);
        left -= take;
    }
    printed.free(req);
    put
}

/// `_ZSTR_STRUCT_SIZE(len)`, what a `zend_string` of `len` bytes asks
/// `emalloc` for on the oracle's LP64.
const fn string_size(len: usize) -> usize {
    (24 + len + 1).next_multiple_of(8)
}

/// `php_addcslashes(s, "'\\", 2)`'s string: `zend_string_safe_alloc(4,
/// len, 0, 0)`, cut to what it wrote where that is shorter, which
/// php-8.5.12's `zend_string_truncate` asks as the bytes past the
/// empty string's header (`Z/Zend/zend_string.h:343`).
fn addcslashed(req: &mut Request, bytes: &[u8]) -> Result<(Emalloc, usize), Thrown> {
    let len = bytes.len();
    let wrote = len + bytes.iter().filter(|b| matches!(b, b'\'' | b'\\')).count();
    let mut block = Emalloc::new(req, 4 * len + string_size(0))?;
    if wrote < 4 * len {
        block = block.resize(req, string_size(0) + wrote)?;
    }
    Ok((block, wrote))
}

/// The two strings php's export makes of a string it writes, held while
/// it is appended: `php_addcslashes` over `'` and `\\`, then
/// `php_str_to_str` of that over a NUL, a copy where it holds none.
struct Escaped {
    slashed: Emalloc,
    spliced: Emalloc,
}

impl Escaped {
    fn new(req: &mut Request, bytes: &[u8]) -> Result<Escaped, Thrown> {
        let (slashed, len) = addcslashed(req, bytes)?;
        #[allow(clippy::naive_bytecount)] // a string's NULs, one loop, not a crate
        let nuls = bytes.iter().filter(|b| **b == 0).count();
        let spliced = if len > 1 && nuls > 0 {
            // `zend_string_safe_alloc(count, str_len - needle_len, length)`,
            // then `zend_string_truncate` to what it wrote.
            let made = Emalloc::new(req, nuls * 11 + string_size(len));
            match made.and_then(|made| made.resize(req, string_size(0) + len + nuls * 11)) {
                Ok(made) => made,
                Err(thrown) => {
                    slashed.free(req);
                    return Err(thrown);
                }
            }
        } else {
            // A copy where no NUL stands, or the splice itself for a
            // string that is one NUL.
            let copied = if len == 1 && nuls == 1 { 12 } else { len };
            match Emalloc::new(req, string_size(copied)) {
                Ok(made) => made,
                Err(thrown) => {
                    slashed.free(req);
                    return Err(thrown);
                }
            }
        };
        Ok(Escaped { slashed, spliced })
    }

    /// `zend_string_free` of both, in php's order.
    fn free(self, req: &mut Request) {
        self.slashed.free(req);
        self.spliced.free(req);
    }
}

/// `smart_str_append_long`: `n` in decimal, written into the buffer.
fn put_long(req: &mut Request, out: &mut impl ExportOut, n: i64) -> Result<(), Thrown> {
    out.put(req, Decimal::of(n).as_bytes())
}

/// `var_export does not handle circular references`, the warning php
/// raises where the walk meets a value it is already inside.
fn circular(req: &mut Request) {
    warning(req, "var_export does not handle circular references");
}

/// A string as php writes it into exported source: `php_addcslashes`
/// over `'` and `\`, then `php_str_to_str` splicing a NUL out of the
/// quoted text as `' . "\0" . '`, between the quotes php puts around it.
fn quoted(req: &mut Request, out: &mut impl ExportOut, bytes: &[u8]) -> Result<(), Thrown> {
    out.put(req, b"'")?;
    escaped(req, out, bytes)?;
    out.put(req, b"'")
}

/// The escaping itself, which a property name takes without the NUL
/// splice php leaves to a value's string: each stretch of bytes that needs
/// none written as it stands.
fn escaped(req: &mut Request, out: &mut impl ExportOut, bytes: &[u8]) -> Result<(), Thrown> {
    let mut rest = bytes;
    while let Some(at) = rest.iter().position(|b| matches!(b, b'\'' | b'\\' | 0)) {
        out.put(req, &rest[..at])?;
        match rest[at] {
            0 => out.put(req, b"' . \"\\0\" . '")?,
            byte => out.put(req, &[b'\\', byte])?,
        }
        rest = &rest[at + 1..];
    }
    out.put(req, rest)
}
