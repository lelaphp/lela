//! The `spl` extension: `php-src/ext/spl` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/spl/tests`, through `xtask phpt --subset`.
//!
//! Each class is declared off the text of its `*.stub.php` ([`stub`])
//! in the order `PHP_MINIT_FUNCTION(spl)` registers it, and where php
//! gives the class an object of its own (`spl_array_object`,
//! `spl_fixedarray_object`) the object carries it as the native
//! payload its class's `create_object` lays down (design §5.4), with
//! the `ObjectHandlers` php overrides beside it. What a payload holds
//! of the heap is given back by the table's `free_obj` and named to the
//! collector by its `get_gc`. The natives of `php_spl.c` are
//! [`functions`], over the engine's autoload stack.
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
mod directory;
mod dllist;
mod exceptions;
mod fixedarray;
mod functions;
mod heap;
mod iterators;
mod observer;
mod stub;

use php_ext::abi::{Extension, ExtensionBuilder, ObjectHandlers, Std};
use php_ext::call::{call_method, lookup_class};
use php_ext::coerce::{coerce_param, key_of, truthy};
use php_ext::decl::{ClassKind, Name, Type};
use php_ext::raising::{Thrown, arg_count_error, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

/// A native's entry, the shape `ExtensionBuilder::functions` pairs a
/// declaration with.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

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
pub fn extension() -> Extension {
    let mut builder = ExtensionBuilder::default()
        .functions(functions::natives())
        .classes(exceptions::classes());
    for ((decl, methods), payload) in iterators::classes() {
        builder = match payload {
            iterators::Payload::Dual => {
                builder.class_payload::<iterators::DualIt>(decl, methods, &iterators::DUAL)
            }
            iterators::Payload::Rec => {
                builder.class_payload::<iterators::RecIt>(decl, methods, &iterators::REC)
            }
            iterators::Payload::None => builder.classes(vec![(decl, methods)]),
        };
    }
    for (decl, methods) in array::classes() {
        builder = builder.class_payload::<array::SplArray>(decl, methods, &array::HANDLERS);
    }
    for (decl, methods) in directory::classes() {
        let handlers = directory::handlers(&decl);
        builder = builder.class_payload::<directory::Fs>(decl, methods, handlers);
    }
    for (decl, methods) in dllist::classes() {
        builder = builder.class_payload::<dllist::SplDllist>(decl, methods, &dllist::HANDLERS);
    }
    for (decl, methods) in heap::classes() {
        builder = builder.class_payload::<heap::SplHeap>(decl, methods, &heap::HANDLERS);
    }
    for (decl, methods) in fixedarray::classes() {
        builder = builder.class_payload::<fixedarray::SplFixedArray>(
            decl,
            methods,
            &fixedarray::HANDLERS,
        );
    }
    for (decl, methods) in observer::classes() {
        builder = if decl.kind == ClassKind::Interface {
            builder.classes(vec![(decl, methods)])
        } else {
            builder.class_payload::<observer::Storage>(decl, methods, &observer::HANDLERS)
        };
    }
    builder.request_state::<State>().info(spl_info).build("SPL")
}

/// `PHP_MINFO_FUNCTION(spl)`: `SPL support => enabled`.
fn spl_info(req: &mut Request) {
    req.info_table(&[&["SPL support", "enabled"]]);
}

/// `ce->name`.
fn class_name(req: &Request, class: ClassId) -> Vec<u8> {
    req.class_decl(class)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default()
}

/// The class a declaration names, looked up without the autoloaders,
/// as the linker found it.
fn named(req: &mut Request, name: &Name) -> Option<ClassId> {
    lookup_class(req, &name.0).ok().flatten()
}

/// `ce->parent`.
fn parent_of(req: &mut Request, class: ClassId) -> Option<ClassId> {
    let parent = req.class_decl(class)?.parent.clone()?;
    named(req, &parent)
}

/// Whether `class` is, extends or implements the class `name`,
/// `instanceof_function` over the declarations.
fn is_a(req: &mut Request, class: ClassId, name: &[u8]) -> bool {
    match lookup_class(req, name).ok().flatten() {
        Some(of) => class_is(req, class, of, 0),
        None => false,
    }
}

/// Whether `class` is `of`, extends it or implements it.
fn class_is(req: &mut Request, class: ClassId, of: ClassId, depth: u32) -> bool {
    if class == of {
        return true;
    }
    let Some(decl) = req.class_decl(class).cloned() else {
        return false;
    };
    if depth > 64 {
        return false;
    }
    for interface in &decl.interfaces {
        if let Some(found) = named(req, interface)
            && class_is(req, found, of, depth + 1)
        {
            return true;
        }
    }
    match decl.parent.as_ref().and_then(|p| named(req, p)) {
        Some(parent) => class_is(req, parent, of, depth + 1),
        None => false,
    }
}

/// `ce->interfaces` after linking, in php's order: an internal class's
/// own after its parent's, each followed by the interfaces it extends
/// in reverse (`zend_do_implement_interface`); a user class's own after
/// the parent's, then the parents of those
/// (`zend_do_implement_interfaces`); and a class that declares none
/// takes its parent's in reverse (`zend_do_inherit_interfaces` walks
/// `if_num--`). `Stringable`, which `zend_register_functions` adds to an
/// internal class declaring `__toString` and the compiler appends to a
/// user one, stands where each puts it.
fn interfaces_of(req: &mut Request, class: ClassId) -> Vec<ClassId> {
    interfaces_at(req, class, 0)
}

fn interfaces_at(req: &mut Request, class: ClassId, depth: u32) -> Vec<ClassId> {
    let mut list: Vec<ClassId> = Vec::new();
    let Some(decl) = req.class_decl(class).cloned() else {
        return list;
    };
    if depth > 64 {
        return list;
    }
    let internal = decl.file.0.is_empty();
    let stringable = lookup_class(req, b"Stringable").ok().flatten();
    let to_string = decl.kind != ClassKind::Trait
        && stringable != Some(class)
        && decl
            .methods
            .iter()
            .any(|m| m.function.name.0.eq_ignore_ascii_case(b"__toString"));
    let mut own: Vec<ClassId> = decl
        .interfaces
        .iter()
        .filter_map(|name| named(req, name))
        .collect();
    if to_string && let Some(stringable) = stringable {
        if internal {
            list.push(stringable);
        } else if !own.contains(&stringable) {
            own.push(stringable);
        }
    }
    let inherited = match decl.parent.as_ref().and_then(|name| named(req, name)) {
        Some(parent) => interfaces_at(req, parent, depth + 1),
        None => Vec::new(),
    };
    let push = |list: &mut Vec<ClassId>, id: ClassId| {
        if !list.contains(&id) {
            list.push(id);
        }
    };
    if internal || own.is_empty() {
        for id in inherited.into_iter().rev() {
            push(&mut list, id);
        }
        if internal {
            for interface in own {
                push(&mut list, interface);
                for id in interfaces_at(req, interface, depth + 1).into_iter().rev() {
                    push(&mut list, id);
                }
            }
        }
        return list;
    }
    for id in inherited {
        push(&mut list, id);
    }
    let mut added = Vec::new();
    for interface in own {
        if !list.contains(&interface) {
            list.push(interface);
            added.push(interface);
        }
    }
    for interface in added {
        for id in interfaces_at(req, interface, depth + 1).into_iter().rev() {
            push(&mut list, id);
        }
    }
    list
}

// ---- the `Z_PARAM_*` readers SPL's natives and methods share (`Z/Zend/zend_API.h`)

/// `zend_wrong_parameters_count_error`: the count `min..=max` checked,
/// php's `ArgumentCountError` worded as it words it.
fn arity(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (how, n) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let word = if n == 1 { "argument" } else { "arguments" };
    Err(arg_count_error(
        req,
        &format!("{how} {n} {word}, {given} given"),
    ))
}

/// The argument at `position` when the call gave one.
fn given(req: &Request, position: u32) -> Option<Value> {
    req.argument(position)
        .filter(|value| !matches!(value.as_ref(), ValueRef::Undef))
}

/// `Z_PARAM_ZVAL`: the value, a reference followed.
fn value(req: &Request, position: u32) -> Value {
    given(req, position).unwrap_or_default().deref()
}

fn scalar(req: &mut Request, position: u32, ty: &Type) -> Result<Value, Thrown> {
    let given = given(req, position).unwrap_or_default();
    coerce_param(req, &given, ty, position)
}

/// `Z_PARAM_LONG`.
fn long(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    Ok(scalar(req, position, &Type::Int)?.as_int().unwrap_or(0))
}

/// `Z_PARAM_LONG` after `Z_PARAM_OPTIONAL`.
fn long_or(req: &mut Request, position: u32, default: i64) -> Result<i64, Thrown> {
    if given(req, position).is_none() {
        return Ok(default);
    }
    long(req, position)
}

/// `Z_PARAM_LONG_OR_NULL`.
fn nullable_long(req: &mut Request, position: u32) -> Result<Option<i64>, Thrown> {
    match given(req, position) {
        Some(v) if !matches!(v.deref().as_ref(), ValueRef::Null) => long(req, position).map(Some),
        _ => Ok(None),
    }
}

/// `$this` of the method being served.
fn this(req: &mut Request) -> Result<PhpObject, Thrown> {
    match req.this() {
        Some(this) => Ok(this),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: an SPL method served without its receiver",
        )),
    }
}

/// `Z_PARAM_BOOL` after `Z_PARAM_OPTIONAL`.
fn bool_or(req: &mut Request, position: u32, default: bool) -> Result<bool, Thrown> {
    if given(req, position).is_none() {
        return Ok(default);
    }
    Ok(scalar(req, position, &Type::Bool)?
        .as_bool()
        .unwrap_or(false))
}

/// `Z_PARAM_STR`.
fn string(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let coerced = scalar(req, position, &Type::String)?;
    match coerced.as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// `Z_PARAM_STR_OR_NULL`.
fn nullable_string(req: &mut Request, position: u32) -> Result<Option<PhpString>, Thrown> {
    match given(req, position) {
        Some(v) if !matches!(v.deref().as_ref(), ValueRef::Null) => string(req, position).map(Some),
        _ => Ok(None),
    }
}

/// `Z_PARAM_ARRAY_HT`.
fn array(req: &mut Request, position: u32) -> Result<PhpArray, Thrown> {
    let v = given(req, position).unwrap_or_default();
    let held = coerce_param(req, &v, &Type::Array, position)?;
    match held.as_array() {
        Some(a) => Ok(*a),
        None => PhpArray::new(req),
    }
}

/// `Z_PARAM_OBJECT_OF_CLASS` and `Z_PARAM_OBJ`: an object, of `class`
/// where one is named, both `coerce_param`'s.
fn object(req: &mut Request, position: u32, class: Option<&str>) -> Result<PhpObject, Thrown> {
    let v = value(req, position);
    let ty = class.map_or(Type::Object, |c| Type::Named(Name(c.as_bytes().into())));
    let coerced = coerce_param(req, &v, &ty, position)?;
    match coerced.as_object() {
        Some(o) => Ok(*o),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: coerce_param held a value that is no object to an object type",
        )),
    }
}

/// `zend_zval_type_name`: an object's class name, any other value's
/// type.
fn zval_type_name(req: &Request, value: &Value) -> String {
    let value = value.deref();
    match value.as_object() {
        Some(object) => String::from_utf8_lossy(&class_name(req, object.class())).into_owned(),
        None => value.type_name().to_owned(),
    }
}

/// A string value made of `bytes`, one owner, the caller's.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// A value handed back with one more owner, `RETURN_COPY`.
fn copied(req: &Request, value: Value) -> Value {
    let value = value.deref();
    req.addref(&value);
    value
}

// ---- a `Traversable` walked from a native, as `spl_iterator_apply` walks one

/// One method served on `object` with no arguments; the return is the
/// caller's, one owner.
fn method(req: &mut Request, object: PhpObject, method: &[u8]) -> Result<Value, Thrown> {
    call_method(req, &object, method, &[])
}

/// A method served for its effect, its return given up.
fn effect(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    let returned = method(req, object, name)?;
    req.release(returned);
    Ok(())
}

/// `valid()`, its return read as `zend_is_true` reads it.
fn valid(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let answered = method(req, object, b"valid")?;
    let valid = truthy(req, &answered);
    req.release(answered);
    Ok(valid)
}

/// A value with a reference followed, the owner moved to what it holds.
fn owned(req: &mut Request, value: Value) -> Value {
    let bare = value.deref();
    req.addref(&bare);
    req.release(value);
    bare
}

/// `zend_user_it_get_new_iterator`: an `Iterator` is its own and an
/// `IteratorAggregate` is asked with `getIterator()` until what it
/// answers is one. The iterator comes back with one owner, the
/// caller's.
fn iterator_of(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let mut current = object;
    req.addref(&current.as_value());
    loop {
        if is_a(req, current.class(), b"Iterator") {
            return Ok(current);
        }
        let answered = method(req, current, b"getIterator");
        let class = current.class();
        req.release(current.as_value());
        let answered = answered?;
        match answered.as_object().copied() {
            Some(next) if is_a(req, next.class(), b"Traversable") => current = next,
            _ => {
                req.release(answered);
                let name = String::from_utf8_lossy(&class_name(req, class)).into_owned();
                return Err(throw(
                    req,
                    "Exception",
                    &format!(
                        "Objects returned by {name}::getIterator() must be traversable or implement interface Iterator"
                    ),
                ));
            }
        }
    }
}

/// `spl_iterator_apply`: `each` served per element with the iterator,
/// until it answers `false` or throws.
fn apply(
    req: &mut Request,
    object: PhpObject,
    mut each: impl FnMut(&mut Request, PhpObject) -> Result<bool, Thrown>,
) -> Result<(), Thrown> {
    let iterator = iterator_of(req, object)?;
    let walked = (|| {
        effect(req, iterator, b"rewind")?;
        while valid(req, iterator)? {
            if !each(req, iterator)? {
                break;
            }
            effect(req, iterator, b"next")?;
        }
        Ok(())
    })();
    req.release(iterator.as_value());
    walked
}

/// `array_set_zval_key`: the value under the key a value makes, php's
/// offset rules through [`key_of`]; the value is the array's, or given
/// up where the key is refused.
fn set_key(
    req: &mut Request,
    array: &mut PhpArray,
    key: &Value,
    value: Value,
) -> Result<(), Thrown> {
    let value = owned(req, value);
    let key: Key = match key_of(req, &key.deref()) {
        Ok(key) => key,
        Err(thrown) => {
            req.release(value);
            return Err(thrown);
        }
    };
    array.set(req, key, value)
}

// ---- what several classes share

/// `zend_dval_to_lval_safe`: the integer a float offset makes, with
/// php's deprecation where the float has a fraction the cast drops.
fn float_key(req: &mut Request, f: f64) -> Result<i64, Thrown> {
    let n = php_ext::coerce::float_to_long(f);
    // `zend_is_long_compatible` is `(double)l == d`, an exact test.
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    let compatible = (n as f64) == f;
    if !compatible && f.is_finite() && f.abs() < 9.2e18 {
        let shown = php_ext::coerce::to_string(req, &Value::float(f))?;
        let shown = String::from_utf8_lossy(shown.as_bytes()).into_owned();
        php_ext::raising::deprecated(
            req,
            &format!("Implicit conversion from float {shown} to int loses precision"),
        );
    }
    Ok(n)
}

/// Whether the class of `object` overrides `lowered` with a method of
/// its own, php's `fptr_*` pointers being set: the method the class
/// answers is not one this crate serves.
fn overridden(req: &mut Request, object: PhpObject, lowered: &[u8]) -> bool {
    Std.get_method(req, object, lowered)
        .and_then(|method| req.function_decl(method))
        .is_some_and(|decl| !decl.file.0.is_empty())
}

/// One value through `ext/standard`'s `php_var_serialize`, called
/// directly as `spl_array.c`, `spl_dllist.c` and `spl_observer.c` call
/// it, never `serialize()` through the function table.
fn serialized(req: &mut Request, value: Value) -> Result<Vec<u8>, Thrown> {
    let mut out = Vec::new();
    php_ext_standard::Serializer::default().write(req, &mut out, &value)?;
    Ok(out)
}

/// The members, `zend_std_get_properties` as an array of its own.
fn members(req: &mut Request, object: PhpObject) -> Result<PhpArray, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (name, value) in Std.get_properties(req, object)? {
        req.addref(&value);
        let key = owned_key(req, &Key::from_string(name));
        out.set(req, key, value)?;
    }
    Ok(out)
}

/// A view of a key made one the caller may hand to [`PhpArray::set`],
/// which takes a string key's owner: `zend_string_copy` over a string
/// key, nothing over an integer one. A dynamic property's name read
/// through a view survives the table built from it, so an object
/// serialized twice names it twice.
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
pub(crate) fn owned_key(req: &Request, key: &Key) -> Key {
    if let Key::Str(s) = key {
        req.addref(&Value::string(*s));
    }
    key.clone()
}

/// `object_properties_load`: each member written onto the object.
fn load_members(req: &mut Request, object: PhpObject, members: &Value) -> Result<(), Thrown> {
    if let Some(members) = members.as_array() {
        for (key, value) in members.snapshot() {
            let name = key.to_prop_name(req)?;
            req.addref(&value);
            Std.write_property(req, object, name, value)?;
        }
    }
    Ok(())
}
