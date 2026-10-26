//! The `spl` extension: `php-src/ext/spl` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/spl/tests`, through `xtask phpt --subset`.
//!
//! Each class is a `#[php_class]` header and member impl, registered
//! in the order `PHP_MINIT_FUNCTION(spl)` registers it, and where php
//! gives the class an object of its own (`spl_array_object`,
//! `spl_fixedarray_object`) the object carries it as the native
//! payload its class's `create_object` lays down (design §5.4), with
//! the `ObjectHandlers` php overrides beside it. What a payload holds
//! of the heap is given back by the table's `free_obj` and named to the
//! collector by its `get_gc`. The natives of `php_spl.c` are
//! [`functions`]' `#[php_function]`s, over the engine's autoload stack.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod array;
pub mod directory;
mod dllist;
mod exceptions;
mod fixedarray;
mod functions;
mod heap;
mod iterators;
mod observer;
mod throwables;

pub(crate) use iterators::{RecursiveIterator, SeekableIterator};

use std::ops::ControlFlow;

use crate::throwables::Throwables;
use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, FromArgument, ObjectHandlers, Raw, Std,
};
use php_ext::attr::php_class;
use php_ext::call::{Iter, call_method, iterate, lookup_class};
use php_ext::coerce::{key_of, truthy};
use php_ext::decl::ClassKind;
use php_ext::raising::{Thrown, throw_error, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::walk::Hold;
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, View};
use php_ext_standard::{Serializer, Unserializer};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {
    /// `SPL_G(autoload_extensions)`: what `spl_autoload_extensions` set,
    /// `None` for php's default.
    autoload_extensions: Option<Vec<u8>>,
}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it, the classes in the order
/// `PHP_MINIT_FUNCTION(spl)` registers them.
///
/// ```php
/// var_dump(extension_loaded('spl'));
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// The natives are `php_spl.stub.php`'s, in its order.
///
/// ```php
/// var_dump(get_extension_funcs("spl"));
/// ```
/// ```expect
/// array(15) {
///   [0]=>
///   string(16) "class_implements"
///   [1]=>
///   string(13) "class_parents"
///   [2]=>
///   string(10) "class_uses"
///   [3]=>
///   string(12) "spl_autoload"
///   [4]=>
///   string(17) "spl_autoload_call"
///   [5]=>
///   string(23) "spl_autoload_extensions"
///   [6]=>
///   string(22) "spl_autoload_functions"
///   [7]=>
///   string(21) "spl_autoload_register"
///   [8]=>
///   string(23) "spl_autoload_unregister"
///   [9]=>
///   string(11) "spl_classes"
///   [10]=>
///   string(15) "spl_object_hash"
///   [11]=>
///   string(13) "spl_object_id"
///   [12]=>
///   string(14) "iterator_apply"
///   [13]=>
///   string(14) "iterator_count"
///   [14]=>
///   string(17) "iterator_to_array"
/// }
/// ```
///
/// Its `deps` table is `spl_deps` (`Z/ext/spl/php_spl.c:760-763`), `json`
/// required, so json starts before it:
///
/// ```php
/// $loaded = get_loaded_extensions();
/// var_dump((new ReflectionExtension('SPL'))->getDependencies(), array_search('json', $loaded) < array_search('SPL', $loaded));
/// ```
/// ```expect
/// array(1) {
///   ["json"]=>
///   string(8) "Required"
/// }
/// bool(true)
/// ```
pub fn extension() -> Extension {
    let mut builder = exceptions::register(ExtensionBuilder::default().functions(vec![
        functions::class_implements::declared(),
        functions::class_parents::declared(),
        functions::class_uses::declared(),
        functions::spl_autoload::declared(),
        functions::spl_autoload_call::declared(),
        functions::spl_autoload_extensions::declared(),
        functions::spl_autoload_functions::declared(),
        functions::spl_autoload_register::declared(),
        functions::spl_autoload_unregister::declared(),
        functions::spl_classes::declared(),
        functions::spl_object_hash::declared(),
        functions::spl_object_id::declared(),
        functions::iterator_apply::declared(),
        functions::iterator_count::declared(),
        functions::iterator_to_array::declared(),
    ]));
    builder = iterators::register(builder);
    builder = array::register(builder);
    builder = directory::register(builder);
    builder = dllist::register(builder);
    builder = heap::register(builder);
    builder = builder.class::<fixedarray::SplFixedArray>();
    observer::register(builder)
        .dependencies(vec![Dependency {
            name: String::from("json"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .info(spl_info)
        .build("SPL")
}

// ---- Core's classes, named for `extends` and `implements`

/// Core's `Exception`.
#[php_class(extern)]
pub(crate) struct Exception;

/// Core's `Traversable`.
#[php_class(extern)]
pub(crate) struct Traversable;

/// Core's `Stringable`.
#[php_class(extern)]
pub(crate) struct Stringable;

/// Core's `Iterator`.
#[php_class(extern)]
pub(crate) struct Iterator;

/// Core's `Countable`.
#[php_class(extern)]
pub(crate) struct Countable;

/// Core's `IteratorAggregate`.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

/// Core's `ArrayAccess`.
#[php_class(extern)]
pub(crate) struct ArrayAccess;

/// Core's `Serializable`.
#[php_class(extern)]
pub(crate) struct Serializable;

/// ext/json's `JsonSerializable`.
#[php_class(extern)]
pub(crate) struct JsonSerializable;

/// `PHP_MINFO_FUNCTION(spl)`: `SPL support => enabled`, then
/// `SPL_LIST_CLASSES` twice, its interfaces and then the rest, each
/// joined by `, ` in the list's order (`spl_build_class_list_string`).
///
/// ```php
/// $e = new ReflectionExtension('spl'); ob_start(); $e->info(); $rows = explode("\n", ob_get_clean());
/// echo $rows[4], "\n", substr($rows[5], 0, 70), "\n", count(explode(', ', $rows[5])), "\n";
/// ```
/// ```expect
/// Interfaces => OuterIterator, RecursiveIterator, SeekableIterator, SplObserver, SplSubject
/// Classes => AppendIterator, ArrayIterator, ArrayObject, BadFunctionCall
/// 50
/// ```
fn spl_info(req: &mut Request) {
    let (interfaces, classes): (Vec<&str>, Vec<&str>) =
        functions::SPL_CLASSES.iter().partition(|name| {
            lookup_class(req, name.as_bytes())
                .ok()
                .flatten()
                .and_then(|class| req.symbols().class_decl(class))
                .is_some_and(|decl| decl.kind == ClassKind::Interface)
        });
    let (interfaces, classes) = (interfaces.join(", "), classes.join(", "));
    req.extensions_mut().info_table(&[
        &["SPL support", "enabled"],
        &["Interfaces", &interfaces],
        &["Classes", &classes],
    ]);
}

/// `ce->name`.
fn class_name(req: &Request, class: ClassId) -> Vec<u8> {
    req.symbols()
        .class_decl(class)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default()
}

/// Whether `class` is, extends or implements the class `name`,
/// `instanceof_function` over the linked record
/// ([`Symbols::instance_of`]).
fn is_a(req: &mut Request, class: ClassId, name: &[u8]) -> bool {
    lookup_class(req, name)
        .ok()
        .flatten()
        .is_some_and(|of| req.symbols().instance_of(class, of))
}

/// A string value made of `bytes`, one owner, the caller's.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// A value a method hands back as it came, under the declared type `T`:
/// php's `RETURN_COPY_VALUE` of what an inner iterator or a callback
/// answered, which an internal method's return is not verified against.
fn answered<T: FromArgument>(value: Owner<Value>) -> Owner<Raw<T>> {
    value.into_raw()
}

// ---- a `Traversable` walked from a native, as `spl_iterator_apply` walks one

/// One method served on `object` with no arguments; the return is the
/// caller's, one owner.
#[cfg_attr(debug_assertions, track_caller)]
fn method(req: &mut Request, object: PhpObject, method: &[u8]) -> Result<Owner<Value>, Thrown> {
    call_method(req, &object, method, &[])
}

/// A method served for its effect, its return given up.
fn effect(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    let returned = method(req, object, name)?;
    returned.give_up(req);
    Ok(())
}

/// `valid()`, its return read as `zend_is_true` reads it.
fn valid(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let answered = method(req, object, b"valid")?;
    let valid = truthy(req, &answered);
    answered.give_up(req);
    let valid = valid?;
    Ok(valid)
}

/// The owner of a value with a reference followed, `ZVAL_DEREF` then
/// `ZVAL_COPY` of what an iterator answered: an owner of what the
/// reference holds, the reference's given up.
#[cfg_attr(debug_assertions, track_caller)]
pub(crate) fn dereferenced(req: &mut Request, value: Owner<Value>) -> Owner<Value> {
    if value.as_php_ref().is_none() {
        return value;
    }
    let bare = Owner::take(req, value.deref());
    value.give_up(req);
    bare
}

/// What a slot of a native's state held before it was written, given up,
/// `zval_ptr_dtor`.
pub(crate) fn give_up<T: View>(req: &mut Request, old: Option<Owner<T>>) {
    give_up_all(req, old);
}

/// The object a call answered where it is a `class`, as the owner the
/// call answered, the one the native keeps; anything else given up.
pub(crate) fn object_answered(
    req: &mut Request,
    answered: Owner<Value>,
    class: &[u8],
) -> Option<Owner<PhpObject>> {
    match answered.into_object() {
        Ok(object) if is_a(req, object.class(), class) => Some(object),
        Ok(object) => {
            object.give_up(req);
            None
        }
        Err(other) => {
            other.give_up(req);
            None
        }
    }
}

/// Each owner given up, in order.
pub(crate) fn give_up_all<T: View>(req: &mut Request, owners: impl IntoIterator<Item = Owner<T>>) {
    for owner in owners {
        owner.give_up(req);
    }
}

/// `spl_iterator_apply`, exported to ext/phar for `phar_build` (design
/// §4.10, the `spl` → `phar` row): [`apply`].
pub fn iterator_apply(
    req: &mut Request,
    object: PhpObject,
    each: impl FnMut(&mut Request, Iter) -> Result<bool, Thrown>,
) -> Result<(), Thrown> {
    apply(req, object, each)
}

/// `spl_iterator_apply`: the object's iterator (`ce->get_iterator`)
/// rewound, then `each` served per element with it, the iterator moved
/// on after each, until `valid` says no, `each` answers `false` or
/// something throws; the iterator given up either way.
fn apply(
    req: &mut Request,
    object: PhpObject,
    mut each: impl FnMut(&mut Request, Iter) -> Result<bool, Thrown>,
) -> Result<(), Thrown> {
    let held = iterate(req, &object)?;
    let iter = *held;
    let walked = (|| {
        iter.rewind(req)?;
        while iter.valid(req)? {
            if !each(req, iter)? {
                break;
            }
            iter.next(req)?;
        }
        Ok(())
    })();
    held.give_up(req);
    walked
}

/// `array_set_zval_key` (`Z/Zend/zend_API.c:2256`): the value under the
/// key a value makes, a numeric string as its integer, a bool as `0` or
/// `1`, a float through [`float_key`], a resource by its number with
/// php's warning, `null` as `""` with its deprecation, and php's
/// `TypeError` for an array or an object; the value is the array's, or
/// given up where the key is refused.
///
/// ```php
/// function g() { yield "1" => 'a'; yield true => 'b'; yield 2.5 => 'c'; yield null => 'd'; yield [] => 'e'; }
/// try { var_dump(iterator_to_array(g())); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// function h() { yield "1" => 'a'; yield true => 'b'; yield 2.0 => 'c'; }
/// var_dump(iterator_to_array(h()));
/// ```
/// ```expect
/// Deprecated: Implicit conversion from float 2.5 to int loses precision in %s on line 3
///
/// Deprecated: Using null as an array offset is deprecated, use an empty string instead in %s on line 3
/// Cannot access offset of type array on array
/// array(2) {
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "c"
/// }
/// ```
fn set_key(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    key: &Value,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    let (key, value) = keyed(req, key, value)?;
    array.set(req, key, value)
}

/// [`set_key`]'s key and value, each one owner, the value's reference
/// followed, for an array the caller writes itself.
fn keyed(
    req: &mut Request,
    key: &Value,
    value: Owner<Value>,
) -> Result<(Owner<Key>, Owner<Value>), Thrown> {
    let value = dereferenced(req, value);
    let key = match zval_key(req, &key.deref()) {
        Ok(key) => key,
        Err(thrown) => {
            value.give_up(req);
            return Err(thrown);
        }
    };
    Ok((key, value))
}

/// The key [`set_key`] places a value under, one owner of a string key.
fn zval_key(req: &mut Request, key: &Value) -> Result<Owner<Key>, Thrown> {
    if let Some(s) = key.as_php_string() {
        return key_of(req, &Value::string(s));
    }
    if let Some(n) = key.as_int() {
        return Ok(Owner::index(n));
    }
    if let Some(b) = key.as_bool() {
        return Ok(Owner::index(i64::from(b)));
    }
    if let Some(f) = key.as_float() {
        let n = float_key(req, f)?;
        return Ok(Owner::index(n));
    }
    if let Some(resource) = key.as_resource() {
        let id = resource.id();
        php_ext::raising::warning(
            req,
            &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
        );
        php_ext::raising::pending(req)?;
        return Ok(Owner::index(id));
    }
    if key.as_array().is_none() && key.as_object().is_none() {
        php_ext::raising::deprecated(
            req,
            "Using null as an array offset is deprecated, use an empty string instead",
        );
        php_ext::raising::pending(req)?;
        return Ok(PhpString::new(req, b"")?.into_key());
    }
    Err(php_ext::raising::illegal_container_offset(
        req,
        "array",
        key,
        php_ext::abi::DimensionRead::Write,
    ))
}

// ---- what several classes share

/// `zend_dval_to_lval_safe`: the integer a float offset makes, with
/// php's warning where no long represents it and its deprecation where
/// the float has a fraction the cast drops.
fn float_key(req: &mut Request, f: f64) -> Result<i64, Thrown> {
    // An array's own offset reading of a float is this one, 8.5's
    // warning for a float no long represents included.
    let key = php_ext::coerce::key_of(req, &Value::float(f))?;
    let n = match &*key {
        Key::Int(n) => *n,
        Key::Str(_) => 0,
    };
    key.give_up(req);
    Ok(n)
}

/// Whether the class of `object` overrides `lowered` with a method of
/// its own, php's `fptr_*` pointers being set: the method the class
/// answers is not one this crate serves.
fn overridden(req: &mut Request, object: PhpObject, lowered: &[u8]) -> bool {
    Std.get_method(req, object, lowered)
        .ok()
        .flatten()
        .and_then(|method| req.symbols().function_decl(method))
        .is_some_and(|decl| !decl.file.0.is_empty())
}

/// A string of parts `write` lays down through one of `ext/standard`'s
/// var hashes, `PHP_VAR_SERIALIZE_INIT` to `PHP_VAR_SERIALIZE_DESTROY`,
/// each part through `php_var_serialize`, called directly as
/// `spl_array.c`, `spl_dllist.c` and `spl_observer.c` call it, never
/// `serialize()` through the function table. One var hash numbers every
/// part, so an object a later part shows again is its `r:n;`, and inside
/// a `serialize()` the var hash is the one that serializer has open.
///
/// ```php
/// $s = new SplObjectStorage; $x = new stdClass; $a = new stdClass;
/// $s[$a] = $x; $s[$x] = $a;
/// echo $s->serialize(), "\n";
/// $o = new ArrayObject([$x]); $o->p = $x;
/// echo $o->serialize(), "\n";
/// $l = new SplDoublyLinkedList; $l->push($x); $l->push($x);
/// echo $l->serialize(), "\n", serialize([$x, $s]), "\n";
/// ```
/// ```expect
/// x:i:2;O:8:"stdClass":0:{},O:8:"stdClass":0:{};r:3;,r:2;;m:a:0:{}
///
/// Deprecated: Creation of dynamic property ArrayObject::$p is deprecated in %s on line 5
/// x:i:0;a:1:{i:0;O:8:"stdClass":0:{}};m:a:1:{s:1:"p";r:3;}
/// i:0;:O:8:"stdClass":0:{}:r:2;
/// a:2:{i:0;O:8:"stdClass":0:{}i:1;O:16:"SplObjectStorage":2:{i:0;a:4:{i:0;O:8:"stdClass":0:{}i:1;r:2;i:2;r:2;i:3;r:5;}i:1;a:0:{}}}
/// ```
fn serialized_parts(
    req: &mut Request,
    write: impl FnOnce(&mut Request, &mut Serializer, &mut Vec<u8>) -> Result<(), Thrown>,
) -> Result<Owner<PhpString>, Thrown> {
    let mut serializer = Serializer::new(req);
    let mut out = Vec::new();
    let written = write(req, &mut serializer, &mut out);
    serializer.finish(req);
    written?;
    PhpString::new(req, &out)
}

/// `zend_std_get_properties`: the object's own table, the declared
/// properties that hold a value and then the dynamic ones, each under
/// the key the table holds it by, mangled for a protected or a private
/// property; views, no owner taken.
fn std_properties(req: &mut Request, object: PhpObject) -> Result<Vec<(Key, Value)>, Thrown> {
    let table = object.property_table(req)?;
    Ok(walk::rows(req, table).collect())
}

/// The members, `zend_std_get_properties` as an array of its own, a
/// protected or a private property under its mangled name, which
/// [`load_members`] places back in its slot.
///
/// ```php
/// class A extends ArrayObject { public $a = 1; protected $b = 2; private $c = 3; }
/// class H extends SplMinHeap { protected $b = 2; private $c = 3; }
/// echo str_replace("\0", '~', (new A([1]))->serialize()), "\n", str_replace("\0", '~', serialize(new H)), "\n";
/// $h = unserialize('O:1:"H":2:{i:0;a:2:{s:4:"' . "\0*\0" . 'b";i:5;s:4:"' . "\0H\0" . 'c";i:6;}i:1;a:2:{s:5:"flags";i:0;s:13:"heap_elements";a:0:{}}}');
/// var_dump($h);
/// ```
/// ```expect
/// x:i:0;a:1:{i:0;i:1;};m:a:3:{s:1:"a";i:1;s:4:"~*~b";i:2;s:4:"~A~c";i:3;}
/// O:1:"H":2:{i:0;a:2:{s:4:"~*~b";i:2;s:4:"~H~c";i:3;}i:1;a:2:{s:5:"flags";i:0;s:13:"heap_elements";a:0:{}}}
/// object(H)#1 (5) {
///   ["b":protected]=>
///   int(5)
///   ["c":"H":private]=>
///   int(6)
///   ["flags":"SplHeap":private]=>
///   int(0)
///   ["isCorrupted":"SplHeap":private]=>
///   bool(false)
///   ["heap":"SplHeap":private]=>
///   array(0) {
///   }
/// }
/// ```
///
/// Each key is an owner of its own, taken of the view the table holds,
/// so a dynamic property's name survives the table built from it and an
/// object serialized twice names it twice.
///
/// ```php
/// #[AllowDynamicProperties] class S extends SplObjectStorage {}
/// $s = new S; $s->{str_repeat('ab', 3)} = 1;
/// echo $s->serialize(), "\n", $s->serialize(), "\n";
/// ```
/// ```expect
/// x:i:0;m:a:1:{s:6:"ababab";i:1;}
/// x:i:0;m:a:1:{s:6:"ababab";i:1;}
/// ```
fn members(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (key, value) in std_properties(req, object)? {
        let (key, value) = (Owner::take(req, key), Owner::take(req, value));
        out.set(req, key, value)?;
    }
    Ok(out)
}

/// `object_properties_load`: each member written onto the object, a
/// mangled name placing a protected or a private property in its slot,
/// and an integer key a row of its own (`zend_hash_index_update`) once
/// the class's dynamic-property rule has had its say: the `Error` that
/// ends the load, or the deprecation, the integer printed in either.
///
/// ```php
/// var_dump(unserialize('O:11:"ArrayObject":4:{i:0;i:0;i:1;a:0:{}i:2;a:1:{i:5;i:1;}i:3;N;}'));
/// class D extends ArrayObject {}
/// var_dump(unserialize('O:1:"D":4:{i:0;i:0;i:1;a:0:{}i:2;a:1:{i:7;s:1:"x";}i:3;N;}'));
/// ```
/// ```expect
///
/// Deprecated: Creation of dynamic property ArrayObject::$5 is deprecated in %s on line 2
/// object(ArrayObject)#1 (2) {
///   [5]=>
///   int(1)
///   ["storage":"ArrayObject":private]=>
///   array(0) {
///   }
/// }
///
/// Deprecated: Creation of dynamic property D::$7 is deprecated in %s on line 4
/// object(D)#1 (2) {
///   [7]=>
///   string(1) "x"
///   ["storage":"ArrayObject":private]=>
///   array(0) {
///   }
/// }
/// ```
fn load_members(req: &mut Request, object: PhpObject, members: &Value) -> Result<(), Thrown> {
    if let Some(members) = members.as_array() {
        // `object_properties_load`'s `ZEND_HASH_FOREACH_KEY_VAL(properties,
        // …)` (`Z/Zend/zend_API.c:1752`), no owner of the table taken: the
        // argument holds it while a write runs a hook or a deprecation
        // reaches the program.
        walk::foreach(req, members, Hold::None, |req, row| -> walk::Step {
            let key = row.key().clone();
            let value = row.value().deref();
            if let Key::Int(h) = key {
                integer_member(req, object, h)?;
                let value = Owner::take(req, value);
                object.set_row(req, &key, value)?;
                return Ok(ControlFlow::Continue(()));
            }
            let name = key.to_prop_name(req)?;
            let value = Owner::take(req, value);
            object.unserialize_prop(req, name, value)?;
            Ok(ControlFlow::Continue(()))
        })?;
    }
    Ok(())
}

/// How the read of an SPL record, the data `SplObjectStorage`'s,
/// `SplDoublyLinkedList`'s and `ArrayObject`'s `unserialize` take, ended
/// short of its var hash's destruction. A read that threw is the
/// `Err` beside it, which broke the record too.
#[derive(Clone, Copy)]
pub(crate) enum Record {
    /// Read to its end, with the members `object_properties_load`
    /// writes where the record carries them.
    Read(Option<Value>),
    /// php's `goto outexcept` (the list's `error`) with `p` at this
    /// offset.
    Broke(usize),
}

/// What a record's read hands `PHP_VAR_UNSERIALIZE_DESTROY`: the members
/// written where it was read whole, and where it broke off, the offset
/// php's `UnexpectedValueException` names. A read that threw broke it
/// where the reader stands, as php's `p` stands after the failed
/// `php_var_unserialize`, and goes to `outexcept` as any failed read
/// does.
pub(crate) fn record_read(
    req: &mut Request,
    object: PhpObject,
    reader: &Unserializer<'_>,
    read: Result<Record, Thrown>,
) -> (Result<(), Thrown>, Option<usize>) {
    match read {
        Ok(Record::Read(None)) => (Ok(()), None),
        Ok(Record::Read(Some(members))) => (load_members(req, object, &members), None),
        Ok(Record::Broke(at)) => (Ok(()), Some(at)),
        Err(thrown) => (Err(thrown), Some(reader.offset())),
    }
}

/// php's `outexcept` after `PHP_VAR_UNSERIALIZE_DESTROY`, whose answer is
/// `finished`: where the record broke off at `broke_at`, the
/// `UnexpectedValueException` naming the offset of the `len` bytes,
/// chained onto whatever throwable stands, as `zend_throw_exception_ex`
/// chains onto `EG(exception)`.
pub(crate) fn record_end(
    req: &mut Request,
    finished: Result<(), Thrown>,
    broke_at: Option<usize>,
    len: usize,
) -> Result<(), Thrown> {
    let Some(at) = broke_at else {
        return finished;
    };
    let ce = Throwables::of(req)?;
    let broke = throw_exception_ex(
        req,
        ce.unexpected_value_exception,
        &format!("Error at offset {at} of {len} bytes"),
    );
    Err(match finished {
        Ok(()) => broke,
        Err(stands) => stands.chain(req, broke),
    })
}

/// `object_properties_load`'s rule for an integer member key
/// (`Z/Zend/zend_API.c:1806`): the class that forbids dynamic
/// properties throws, one that does not allow them deprecates.
fn integer_member(req: &mut Request, object: PhpObject, h: i64) -> Result<(), Thrown> {
    use php_ext::decl::DynamicProperties;
    let Some(flags) = req.symbols().class_flags(object.class()) else {
        return Ok(());
    };
    let class = String::from_utf8_lossy(&class_name(req, object.class())).into_owned();
    match flags.dynamic_properties {
        DynamicProperties::Allowed => {}
        DynamicProperties::Forbidden => {
            let message = format!("Cannot create dynamic property {class}::${h}");
            let ce = Throwables::of(req)?;
            return Err(throw_error(req, ce.error, &message));
        }
        DynamicProperties::Deprecated => {
            let message = format!("Creation of dynamic property {class}::${h} is deprecated");
            php_ext::raising::deprecated(req, &message);
        }
    }
    Ok(())
}
