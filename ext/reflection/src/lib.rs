//! The `reflection` extension: `php-src/ext/reflection` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/reflection/tests`, through `xtask phpt --subset`.
//!
//! What is here reads the linked tables through `api` and nothing else
//! (design §12): `ReflectionClass`, `ReflectionObject`,
//! `ReflectionProperty`, `ReflectionClassConstant` and
//! `ReflectionConstant` with the accessors
//! `php_reflection.c` answers off the class entry, the property info
//! and the constant (the section below `ReflectionClass`), and
//! `ReflectionExtension` and `ReflectionFunction`, each
//! with its constructor and its `__toString`, rendering as
//! `_extension_string` and `_function_string` in `php_reflection.c` at
//! the tag render, in php's wording and order, since `xtask crosscheck`
//! holds the bytes to `php --re` (design §7.1, §8), beside the
//! accessors `php_reflection.c` answers off the `zend_module_entry`,
//! `getName` to `isTemporary`, `info()` writing the extension's `MINFO`
//! table as `php_info_print_module` does, and `isDeprecated`. A reflection object
//! keeps what php keeps on it, the `name` property its stub declares,
//! and looks the extension or function up again when it renders, which
//! for the linked tables is the same entry php's `intern->ptr` points
//! at. `ReflectionException` is declared here as `php_reflection.stub.php`
//! declares it, under `Exception`.
//!
//! Rendered: `_extension_string`'s header, `Constants`, `Functions`
//! and `Classes` blocks, `_const_string`, `_class_string` with its
//! `_class_const_string`, `_property_string` and `_function_string`
//! rows over the linked tables, and `ReflectionClass::__toString` and
//! `ReflectionMethod::__toString` over the same two. What php reads off
//! one class entry after `zend_do_inheritance` and
//! `zend_do_implement_interfaces` ran, the function table with each
//! method's scope and prototype, the interface list in its order, the
//! constants and the properties with their shadows, is computed here
//! by the same rules over the declarations (`linked_methods`,
//! `linked_interfaces`, `linked_constants`, `linked_properties`), so
//! the words `inherits`, `overwrites`, `prototype` and `ctor` are php's.
//! Reflection's own twenty-six classes are declared in [`stub`] with
//! every method the stub has, so `php --re reflection` renders whole.
//! A reflector keeps php's `intern->ce` and `intern->obj` in the native
//! payload [`Reflected`], which its class's `create_object` lays down
//! (design §5.4), so the class it was asked about and the object it was
//! made from are what the renderings read. An extension's `INI` block,
//! `_extension_ini_string`, a closure's `Bound Variables` and
//! `<iterateable>` are not rendered yet (#375).
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

mod attr;
mod class;
mod enums;
mod func;
mod member;
mod signature;
mod stub;

use std::collections::HashMap;
use std::fmt::Write as _;

use php_ext::abi::{CloneObj, DependencyKind, Extension, ExtensionBuilder, ObjectHandlers};
use php_ext::call::{
    Function, constant, eval_const_expr, lookup_class, lookup_function, new_object,
    read_static_prop, write_static_prop,
};
use php_ext::coerce::value_name;
use php_ext::decl::{
    ClassConstDecl, ClassDecl, ClassKind, ConstExpr, EnumBacking, FunctionDecl, MethodDecl, Name,
    Parameter, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Thrown, arg_count_error, arg_type_error, deprecated, throw};
use php_ext::request::{ExtensionId, PHP_VERSION, Request};
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
///
/// The two tables are php's `intern->ptr` for the reflectors whose
/// subject is not a name they can look up again, a `ReflectionParameter`
/// and the four type reflectors, keyed by the object's store index as php
/// keys them by the object itself (see [`signature`]).
#[derive(Default)]
struct State {
    parameters: HashMap<u32, signature::ParameterRef>,
    types: HashMap<u32, signature::TypeRef>,
}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('reflection'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    let mut builder = ExtensionBuilder::default();
    for (decl, methods) in stub::classes() {
        builder = if REFLECTORS.iter().any(|name| *name == decl.name.0.as_ref()) {
            builder.class_payload::<Reflected>(decl, methods, &Reflector)
        } else {
            builder.classes(vec![(decl, methods)])
        };
    }
    builder
        .request_state::<State>()
        .info(reflection_info)
        // `zend_module_entry.name` is `Reflection`, the spelling
        // `ReflectionClass::getExtensionName` and `php --re` print;
        // `extension_loaded('reflection')` lowers both sides.
        .build("Reflection")
}

/// `PHP_MINFO_FUNCTION(reflection)`: the one-row table `Reflection =>
/// enabled`. The witness is `ReflectionExtension::info`'s.
fn reflection_info(req: &mut Request) {
    req.info_table(&[&["Reflection", "enabled"]]);
}

/// The classes php gives `reflection_objects_new` as their
/// `create_object` (`Z/ext/reflection/php_reflection.c:7990-8080`), of
/// the ones this crate's reflectors read a payload off: the class
/// reflector, the object reflector php keeps the instance on, and the
/// two member reflectors whose scope is the class they were asked
/// about. A subclass takes the payload from its parent, as php inherits
/// `create_object`, so `ReflectionEnum` and a user class extending
/// `ReflectionClass` carry it without a row here. The reflectors php
/// gives it that keep nothing yet stay `Std` until they do.
const REFLECTORS: &[&[u8]] = &[
    b"ReflectionAttribute",
    b"ReflectionClass",
    b"ReflectionObject",
    b"ReflectionMethod",
    b"ReflectionProperty",
    b"ReflectionFunction",
    b"ReflectionParameter",
];

/// php's `reflection_object` beside the `name` and `class` properties
/// the stub declares (`Z/ext/reflection/php_reflection.c:180-188`):
/// `intern->ce`, the class the reflector was asked about, which is not
/// always the one that declared what it reflects; and `intern->obj`,
/// the object a `ReflectionObject` was made from. `_function_string`
/// takes `intern->ce` as a method's scope, `_class_string` prints the
/// `Dynamic properties` of `intern->obj`, and
/// `ReflectionProperty::getValue` holds its argument to `intern->ce`.
/// The slot is laid down by `new` as the class's `create_object`
/// (design §5.4) and written by the constructor.
#[derive(Default)]
struct Reflected {
    ce: Option<ClassId>,
    object: Option<PhpObject>,
    /// php's `attribute_reference`, which `ReflectionAttribute` keeps
    /// where the other reflectors keep `intern->ce`.
    attribute: Option<attr::AttributeRef>,
}

/// `reflection_object_handlers`
/// (`Z/ext/reflection/php_reflection.c:7976-7981`): `free_obj` gives
/// back the object the reflector kept, `clone_obj` is php's null slot,
/// which is why `clone $reflector` is php's uncloneable `Error`, and
/// `get_gc` hands the collector what the payload holds. php's `offset`
/// and `write_property` are its own bookkeeping and the engine's here.
struct Reflector;

impl ObjectHandlers for Reflector {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(kept) = object
            .native_mut::<Reflected>(req)
            .and_then(|reflected| reflected.object.take())
        {
            req.release(kept.as_value());
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(kept) = object
            .native::<Reflected>(req)
            .and_then(|reflected| reflected.object)
        {
            visit(kept.as_value());
        }
    }
}

/// `intern->ce` and `intern->obj` written as php's constructors write
/// them, the object one more owner where there is one: what `new` laid
/// down is overwritten, so nothing the payload held before is lost.
fn keep(req: &mut Request, this: PhpObject, ce: ClassId, object: Option<PhpObject>) {
    if let Some(object) = object {
        req.addref(&object.as_value());
    }
    let released = match this.native_mut::<Reflected>(req) {
        Some(reflected) => {
            let released = reflected.object.take();
            reflected.ce = Some(ce);
            reflected.object = object;
            released
        }
        None => object,
    };
    if let Some(released) = released {
        req.release(released.as_value());
    }
}

/// `intern->obj` alone, as `ReflectionFunction::__construct` writes it
/// for a closure (`php_reflection.c`'s `is_closure` arm): the object
/// one more owner, the class left as it stands, since a closure names
/// none.
fn keep_closure(req: &mut Request, this: PhpObject, object: PhpObject) {
    req.addref(&object.as_value());
    let released = match this.native_mut::<Reflected>(req) {
        Some(reflected) => reflected.object.replace(object),
        None => Some(object),
    };
    if let Some(released) = released {
        req.release(released.as_value());
    }
}

/// `intern->ptr` as `reflection_attribute_factory` writes it: the
/// reference the six `ReflectionAttribute` accessors read.
fn keep_attribute(req: &mut Request, this: PhpObject, kept: attr::AttributeRef) {
    if let Some(reflected) = this.native_mut::<Reflected>(req) {
        reflected.attribute = Some(kept);
    }
}

/// `GET_REFLECTION_OBJECT_PTR(attr)`: the reference a
/// `ReflectionAttribute` keeps, or php's `Error` for a reflector read
/// before anything wrote one.
fn this_attribute(req: &mut Request) -> Result<attr::AttributeRef, Thrown> {
    let kept = req
        .this()
        .and_then(|this| this.native::<Reflected>(req))
        .and_then(|reflected| reflected.attribute.clone());
    match kept {
        Some(kept) => Ok(kept),
        None => Err(throw(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

/// `intern->obj` where it holds a `Closure`, which is what a reflector
/// over a closure keeps: none for every other reflector.
fn kept_closure(req: &Request, this: PhpObject) -> Option<PhpObject> {
    let kept = kept_object(req, this)?;
    kept.closure(req).map(|_| kept)
}

/// `intern->ce`, the class the reflector was asked about; `None` for a
/// reflector whose class carries no payload yet.
fn kept_class(req: &Request, this: PhpObject) -> Option<ClassId> {
    this.native::<Reflected>(req).and_then(|kept| kept.ce)
}

/// `intern->obj`, the object a `ReflectionObject` was made from.
fn kept_object(req: &Request, this: PhpObject) -> Option<PhpObject> {
    this.native::<Reflected>(req).and_then(|kept| kept.object)
}

/// A native's entry, the shape `ExtensionBuilder::classes` takes beside
/// each method.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// `ZEND_PARSE_PARAMETERS_START(1, 1)` with one `string`: the count
/// held to exactly one and the argument read as [`string_argument`].
fn one_string_argument(req: &mut Request) -> Result<Vec<u8>, Thrown> {
    exactly_arguments(req, 1)?;
    string_argument(req, 1)
}

/// `ZEND_PARSE_PARAMETERS_START(n, n)`'s count check: php's
/// `ArgumentCountError`, `expects exactly N argument(s), M given`.
fn exactly_arguments(req: &mut Request, expected: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count == expected {
        return Ok(());
    }
    let plural = if expected == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("exactly {expected} argument{plural}, {count} given"),
    ))
}

/// `ZEND_PARSE_PARAMETERS_START(0, n)`'s count check: `expects at most
/// N arguments, M given`.
fn at_most_arguments(req: &mut Request, most: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count <= most {
        return Ok(());
    }
    Err(arg_count_error(
        req,
        &format!("at most {most} arguments, {count} given"),
    ))
}

/// `Z_PARAM_STR` at `position`: a scalar is coerced as a string
/// parameter is, `null` the 8.1 deprecation naming the declared type
/// and the empty string written back as the argument, both
/// `coerce_param`'s; the argument's bytes are copied out of the frame.
fn string_argument(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    let Some(given) = req.argument(position) else {
        return Ok(Vec::new());
    };
    let coerced = php_ext::coerce::coerce_param(req, &given, &Type::String, position)?;
    Ok(coerced.as_str_bytes().unwrap_or_default().to_vec())
}

/// `Z_PARAM_OBJ_OR_STR` at `position` and the lookup both reflection
/// constructors do with it: an object is its class, kept beside it; a
/// string is `zend_lookup_class`, a leading `\` dropped, or php's
/// `ReflectionException` `Class "X" does not exist` with the name as
/// given; anything else is the `TypeError` naming `object|string`.
fn class_argument(
    req: &mut Request,
    position: u32,
) -> Result<(ClassId, Option<PhpObject>), Thrown> {
    if let Some(given) = req.argument(position) {
        match given.deref().as_ref() {
            ValueRef::Object(object) => return Ok((object.class(), Some(*object))),
            ValueRef::Array(_) | ValueRef::Resource(_) => {
                return Err(arg_type_error(
                    req,
                    position,
                    &format!(
                        "be of type object|string, {} given",
                        value_name(req, &given.deref())
                    ),
                ));
            }
            _ => {}
        }
    }
    let name = string_argument(req, position)?;
    Ok((class_named(req, &name)?, None))
}

/// `zend_lookup_class` or the `ReflectionException` every reflector
/// throws for a miss, `Class "X" does not exist`, the name as given.
fn class_named(req: &mut Request, name: &[u8]) -> Result<ClassId, Thrown> {
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    match lookup_class(req, bare)? {
        Some(class) => Ok(class),
        None => Err(throw(
            req,
            "ReflectionException",
            &format!("Class \"{}\" does not exist", String::from_utf8_lossy(name)),
        )),
    }
}

/// `ZEND_THIS` and its `name` property, or php's `Error` for a
/// reflector read before its constructor ran.
fn this_name(req: &mut Request, class: &str) -> Result<(PhpObject, Vec<u8>), Thrown> {
    let Some(this) = req.this() else {
        return Err(throw(
            req,
            "Error",
            &format!("Internal error: Failed to retrieve the reflection object of {class}"),
        ));
    };
    match this
        .prop(req, b"name")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    {
        Some(name) => Ok((this, name)),
        None => Err(throw(
            req,
            "Error",
            &format!("Typed property {class}::$name must not be accessed before initialization"),
        )),
    }
}

fn set_name(req: &mut Request, this: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    set_string_prop(req, this, b"name", name)
}

fn set_string_prop(
    req: &mut Request,
    this: PhpObject,
    prop: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let key = PhpString::new(req, prop)?;
    let value = PhpString::new(req, value)?;
    this.set_prop(req, key, Value::string(value))
}

/// A string of the request's as a native's return.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// `ReflectionExtension::__construct(string $name)`: the extension
/// under the lowered name in `module_registry`, its declared name kept
/// as `$name`; a name the bundle has not is php's `ReflectionException`,
/// the name as given.
///
/// ```php
/// $e = new ReflectionExtension('CTYPE'); $f = new ReflectionFunction('CTYPE_ALPHA'); echo $e->name, '|', $f->name, '|'; var_dump($e instanceof Reflector);
/// ```
/// ```expect
/// ctype|ctype_alpha|bool(true)
/// ```
///
/// ```php
/// new ReflectionExtension('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Extension "nope" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionExtension->__construct('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionExtension();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionExtension::__construct() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionExtension->__construct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_extension_construct(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let Some(extension) = req.extension_named(&name) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Extension \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let declared = req
        .extension_name(extension)
        .map(str::to_owned)
        .unwrap_or_default();
    if let Some(this) = req.this() {
        set_name(req, this, declared.as_bytes())?;
    }
    Ok(Value::null())
}

/// `ReflectionExtension::__toString()`: `_extension_string` over the
/// extension `$name` names, indent empty. The load number in the first
/// line is the oracle's build order there and the bundle's here, `#10`
/// against `#15` for ctype, a difference that is nobody's defect; the
/// pin's `[recipe.normalise] extension_number_token` (#317) replaces
/// the digits after `extension #` in both streams, so the fence below
/// holds every byte the oracle prints and the token stands where the
/// number was.
///
/// ```php
/// echo new ReflectionExtension('ctype');
/// ```
/// ```expect
/// Extension [ <persistent> extension #%N% ctype version 8.5.9 ] {
///
///   - Functions {
///     Function [ <internal:ctype> function ctype_alnum ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_alpha ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_cntrl ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_digit ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_lower ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_graph ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_print ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_punct ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_space ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_upper ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_xdigit ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///   }
/// }
/// ```
///
/// ```php
/// echo new ReflectionExtension('tokenizer');
/// ```
/// ```expect
/// Extension [ <persistent> extension #%N% tokenizer version 8.5.9 ] {
///
///   - Constants [154] {
///     Constant [ <persistent> int T_LNUMBER ] { 260 }
///     Constant [ <persistent> int T_DNUMBER ] { 261 }
///     Constant [ <persistent> int T_STRING ] { 262 }
///     Constant [ <persistent> int T_NAME_FULLY_QUALIFIED ] { 263 }
///     Constant [ <persistent> int T_NAME_RELATIVE ] { 264 }
///     Constant [ <persistent> int T_NAME_QUALIFIED ] { 265 }
///     Constant [ <persistent> int T_VARIABLE ] { 266 }
///     Constant [ <persistent> int T_INLINE_HTML ] { 267 }
///     Constant [ <persistent> int T_ENCAPSED_AND_WHITESPACE ] { 268 }
///     Constant [ <persistent> int T_CONSTANT_ENCAPSED_STRING ] { 269 }
///     Constant [ <persistent> int T_STRING_VARNAME ] { 270 }
///     Constant [ <persistent> int T_NUM_STRING ] { 271 }
///     Constant [ <persistent> int T_INCLUDE ] { 272 }
///     Constant [ <persistent> int T_INCLUDE_ONCE ] { 273 }
///     Constant [ <persistent> int T_EVAL ] { 274 }
///     Constant [ <persistent> int T_REQUIRE ] { 275 }
///     Constant [ <persistent> int T_REQUIRE_ONCE ] { 276 }
///     Constant [ <persistent> int T_LOGICAL_OR ] { 277 }
///     Constant [ <persistent> int T_LOGICAL_XOR ] { 278 }
///     Constant [ <persistent> int T_LOGICAL_AND ] { 279 }
///     Constant [ <persistent> int T_PRINT ] { 280 }
///     Constant [ <persistent> int T_YIELD ] { 281 }
///     Constant [ <persistent> int T_YIELD_FROM ] { 282 }
///     Constant [ <persistent> int T_INSTANCEOF ] { 283 }
///     Constant [ <persistent> int T_NEW ] { 284 }
///     Constant [ <persistent> int T_CLONE ] { 285 }
///     Constant [ <persistent> int T_EXIT ] { 286 }
///     Constant [ <persistent> int T_IF ] { 287 }
///     Constant [ <persistent> int T_ELSEIF ] { 288 }
///     Constant [ <persistent> int T_ELSE ] { 289 }
///     Constant [ <persistent> int T_ENDIF ] { 290 }
///     Constant [ <persistent> int T_ECHO ] { 291 }
///     Constant [ <persistent> int T_DO ] { 292 }
///     Constant [ <persistent> int T_WHILE ] { 293 }
///     Constant [ <persistent> int T_ENDWHILE ] { 294 }
///     Constant [ <persistent> int T_FOR ] { 295 }
///     Constant [ <persistent> int T_ENDFOR ] { 296 }
///     Constant [ <persistent> int T_FOREACH ] { 297 }
///     Constant [ <persistent> int T_ENDFOREACH ] { 298 }
///     Constant [ <persistent> int T_DECLARE ] { 299 }
///     Constant [ <persistent> int T_ENDDECLARE ] { 300 }
///     Constant [ <persistent> int T_AS ] { 301 }
///     Constant [ <persistent> int T_SWITCH ] { 302 }
///     Constant [ <persistent> int T_ENDSWITCH ] { 303 }
///     Constant [ <persistent> int T_CASE ] { 304 }
///     Constant [ <persistent> int T_DEFAULT ] { 305 }
///     Constant [ <persistent> int T_MATCH ] { 306 }
///     Constant [ <persistent> int T_BREAK ] { 307 }
///     Constant [ <persistent> int T_CONTINUE ] { 308 }
///     Constant [ <persistent> int T_GOTO ] { 309 }
///     Constant [ <persistent> int T_FUNCTION ] { 310 }
///     Constant [ <persistent> int T_FN ] { 311 }
///     Constant [ <persistent> int T_CONST ] { 312 }
///     Constant [ <persistent> int T_RETURN ] { 313 }
///     Constant [ <persistent> int T_TRY ] { 314 }
///     Constant [ <persistent> int T_CATCH ] { 315 }
///     Constant [ <persistent> int T_FINALLY ] { 316 }
///     Constant [ <persistent> int T_THROW ] { 317 }
///     Constant [ <persistent> int T_USE ] { 318 }
///     Constant [ <persistent> int T_INSTEADOF ] { 319 }
///     Constant [ <persistent> int T_GLOBAL ] { 320 }
///     Constant [ <persistent> int T_STATIC ] { 321 }
///     Constant [ <persistent> int T_ABSTRACT ] { 322 }
///     Constant [ <persistent> int T_FINAL ] { 323 }
///     Constant [ <persistent> int T_PRIVATE ] { 324 }
///     Constant [ <persistent> int T_PROTECTED ] { 325 }
///     Constant [ <persistent> int T_PUBLIC ] { 326 }
///     Constant [ <persistent> int T_PRIVATE_SET ] { 327 }
///     Constant [ <persistent> int T_PROTECTED_SET ] { 328 }
///     Constant [ <persistent> int T_PUBLIC_SET ] { 329 }
///     Constant [ <persistent> int T_READONLY ] { 330 }
///     Constant [ <persistent> int T_VAR ] { 331 }
///     Constant [ <persistent> int T_UNSET ] { 332 }
///     Constant [ <persistent> int T_ISSET ] { 333 }
///     Constant [ <persistent> int T_EMPTY ] { 334 }
///     Constant [ <persistent> int T_HALT_COMPILER ] { 335 }
///     Constant [ <persistent> int T_CLASS ] { 336 }
///     Constant [ <persistent> int T_TRAIT ] { 337 }
///     Constant [ <persistent> int T_INTERFACE ] { 338 }
///     Constant [ <persistent> int T_ENUM ] { 339 }
///     Constant [ <persistent> int T_EXTENDS ] { 340 }
///     Constant [ <persistent> int T_IMPLEMENTS ] { 341 }
///     Constant [ <persistent> int T_NAMESPACE ] { 342 }
///     Constant [ <persistent> int T_LIST ] { 343 }
///     Constant [ <persistent> int T_ARRAY ] { 344 }
///     Constant [ <persistent> int T_CALLABLE ] { 345 }
///     Constant [ <persistent> int T_LINE ] { 346 }
///     Constant [ <persistent> int T_FILE ] { 347 }
///     Constant [ <persistent> int T_DIR ] { 348 }
///     Constant [ <persistent> int T_CLASS_C ] { 349 }
///     Constant [ <persistent> int T_TRAIT_C ] { 350 }
///     Constant [ <persistent> int T_METHOD_C ] { 351 }
///     Constant [ <persistent> int T_FUNC_C ] { 352 }
///     Constant [ <persistent> int T_PROPERTY_C ] { 353 }
///     Constant [ <persistent> int T_NS_C ] { 354 }
///     Constant [ <persistent> int T_ATTRIBUTE ] { 355 }
///     Constant [ <persistent> int T_PLUS_EQUAL ] { 356 }
///     Constant [ <persistent> int T_MINUS_EQUAL ] { 357 }
///     Constant [ <persistent> int T_MUL_EQUAL ] { 358 }
///     Constant [ <persistent> int T_DIV_EQUAL ] { 359 }
///     Constant [ <persistent> int T_CONCAT_EQUAL ] { 360 }
///     Constant [ <persistent> int T_MOD_EQUAL ] { 361 }
///     Constant [ <persistent> int T_AND_EQUAL ] { 362 }
///     Constant [ <persistent> int T_OR_EQUAL ] { 363 }
///     Constant [ <persistent> int T_XOR_EQUAL ] { 364 }
///     Constant [ <persistent> int T_SL_EQUAL ] { 365 }
///     Constant [ <persistent> int T_SR_EQUAL ] { 366 }
///     Constant [ <persistent> int T_COALESCE_EQUAL ] { 367 }
///     Constant [ <persistent> int T_BOOLEAN_OR ] { 368 }
///     Constant [ <persistent> int T_BOOLEAN_AND ] { 369 }
///     Constant [ <persistent> int T_IS_EQUAL ] { 370 }
///     Constant [ <persistent> int T_IS_NOT_EQUAL ] { 371 }
///     Constant [ <persistent> int T_IS_IDENTICAL ] { 372 }
///     Constant [ <persistent> int T_IS_NOT_IDENTICAL ] { 373 }
///     Constant [ <persistent> int T_IS_SMALLER_OR_EQUAL ] { 374 }
///     Constant [ <persistent> int T_IS_GREATER_OR_EQUAL ] { 375 }
///     Constant [ <persistent> int T_SPACESHIP ] { 376 }
///     Constant [ <persistent> int T_SL ] { 377 }
///     Constant [ <persistent> int T_SR ] { 378 }
///     Constant [ <persistent> int T_INC ] { 379 }
///     Constant [ <persistent> int T_DEC ] { 380 }
///     Constant [ <persistent> int T_INT_CAST ] { 381 }
///     Constant [ <persistent> int T_DOUBLE_CAST ] { 382 }
///     Constant [ <persistent> int T_STRING_CAST ] { 383 }
///     Constant [ <persistent> int T_ARRAY_CAST ] { 384 }
///     Constant [ <persistent> int T_OBJECT_CAST ] { 385 }
///     Constant [ <persistent> int T_BOOL_CAST ] { 386 }
///     Constant [ <persistent> int T_UNSET_CAST ] { 387 }
///     Constant [ <persistent> int T_VOID_CAST ] { 388 }
///     Constant [ <persistent> int T_OBJECT_OPERATOR ] { 389 }
///     Constant [ <persistent> int T_NULLSAFE_OBJECT_OPERATOR ] { 390 }
///     Constant [ <persistent> int T_DOUBLE_ARROW ] { 391 }
///     Constant [ <persistent> int T_COMMENT ] { 392 }
///     Constant [ <persistent> int T_DOC_COMMENT ] { 393 }
///     Constant [ <persistent> int T_OPEN_TAG ] { 394 }
///     Constant [ <persistent> int T_OPEN_TAG_WITH_ECHO ] { 395 }
///     Constant [ <persistent> int T_CLOSE_TAG ] { 396 }
///     Constant [ <persistent> int T_WHITESPACE ] { 397 }
///     Constant [ <persistent> int T_START_HEREDOC ] { 398 }
///     Constant [ <persistent> int T_END_HEREDOC ] { 399 }
///     Constant [ <persistent> int T_DOLLAR_OPEN_CURLY_BRACES ] { 400 }
///     Constant [ <persistent> int T_CURLY_OPEN ] { 401 }
///     Constant [ <persistent> int T_PAAMAYIM_NEKUDOTAYIM ] { 402 }
///     Constant [ <persistent> int T_NS_SEPARATOR ] { 403 }
///     Constant [ <persistent> int T_ELLIPSIS ] { 404 }
///     Constant [ <persistent> int T_COALESCE ] { 405 }
///     Constant [ <persistent> int T_POW ] { 406 }
///     Constant [ <persistent> int T_POW_EQUAL ] { 407 }
///     Constant [ <persistent> int T_PIPE ] { 408 }
///     Constant [ <persistent> int T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG ] { 409 }
///     Constant [ <persistent> int T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG ] { 410 }
///     Constant [ <persistent> int T_BAD_CHARACTER ] { 411 }
///     Constant [ <persistent> int T_DOUBLE_COLON ] { 402 }
///     Constant [ <persistent> int TOKEN_PARSE ] { 1 }
///   }
///
///   - Functions {
///     Function [ <internal:tokenizer> function token_get_all ] {
///
///       - Parameters [2] {
///         Parameter #0 [ <required> string $code ]
///         Parameter #1 [ <optional> int $flags = 0 ]
///       }
///       - Return [ array ]
///     }
///     Function [ <internal:tokenizer> function token_name ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> int $id ]
///       }
///       - Return [ string ]
///     }
///   }
///
///   - Classes [1] {
///     Class [ <internal:tokenizer> class PhpToken implements Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [1] {
///         Method [ <internal:tokenizer> static public method tokenize ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> string $code ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///
///       - Properties [4] {
///         Property [ public int $id ]
///         Property [ public string $text ]
///         Property [ public int $line ]
///         Property [ public int $pos ]
///       }
///
///       - Methods [5] {
///         Method [ <internal:tokenizer, ctor> final public method __construct ] {
///
///           - Parameters [4] {
///             Parameter #0 [ <required> int $id ]
///             Parameter #1 [ <required> string $text ]
///             Parameter #2 [ <optional> int $line = -1 ]
///             Parameter #3 [ <optional> int $pos = -1 ]
///           }
///         }
///
///         Method [ <internal:tokenizer> public method is ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> $kind ]
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:tokenizer> public method isIgnorable ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:tokenizer> public method getTokenName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?string ]
///         }
///
///         Method [ <internal:tokenizer, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///   }
/// }
/// ```
fn reflection_extension_to_string(req: &mut Request) -> Result<Value, Thrown> {
    let extension = this_extension(req)?;
    let text = extension_string(req, extension, "");
    string_value(req, text.as_bytes())
}

/// `GET_REFLECTION_OBJECT_PTR(module)` for a `ReflectionExtension`: the
/// extension `$name` names, looked up again as the constructor found
/// it; a name the bundle has not is the constructor's
/// `ReflectionException`.
fn this_extension(req: &mut Request) -> Result<ExtensionId, Thrown> {
    let (_, name) = this_name(req, "ReflectionExtension")?;
    match req.extension_named(&name) {
        Some(extension) => Ok(extension),
        None => Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Extension \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        )),
    }
}

/// `ReflectionExtension::getName()`: `module->name`, the spelling the
/// `zend_module_entry` carries whatever the constructor was given.
///
/// ```php
/// var_dump((new ReflectionExtension('CTYPE'))->getName());
/// ```
/// ```expect
/// string(5) "ctype"
/// ```
fn reflection_extension_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let name = req
        .extension_name(extension)
        .map(str::to_owned)
        .unwrap_or_default();
    string_value(req, name.as_bytes())
}

/// `ReflectionExtension::getClasses()`: over `EG(class_table)` in its
/// order, every class whose `ce->info.internal.module` is this
/// extension, keyed by its name and holding the `ReflectionClass`
/// `zend_reflection_class_factory` makes, `object_init_ex` and the name
/// written with no constructor served, a `ReflectionEnum` for an enum.
/// The alias key php takes for an aliased class has no case here until
/// `class_alias` links one. `ctype` declares no class, so the fence
/// pins the empty walk; the walk over `Reflection`'s classes is
/// `ReflectionExtension_getClasses_basic.phpt`'s, which names php's
/// twenty-six where this crate declares a subset, and a fence over one
/// of them waits on the compiler reading a dimension fetch.
///
/// ```php
/// $e = new ReflectionExtension('ctype'); var_dump($e->getClasses(), $e->getClassNames());
/// ```
/// ```expect
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
fn reflection_extension_get_classes(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let mut classes = PhpArray::new(req)?;
    for (class, name) in extension_classes(req, extension) {
        let reflector = reflection_class_factory(req, class)?;
        let key = Key::Str(PhpString::new(req, &name)?);
        classes.set(req, key, Value::object(reflector))?;
    }
    Ok(Value::array(classes))
}

/// `ReflectionExtension::getClassNames()`: the same walk as
/// `getClasses`, the names appended in order. The witness is
/// `getClasses`'s.
fn reflection_extension_get_class_names(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let mut names = PhpArray::new(req)?;
    for (_, name) in extension_classes(req, extension) {
        let name = Value::string(PhpString::new(req, &name)?);
        names.append(req, name)?;
    }
    Ok(Value::array(names))
}

/// `add_extension_class`'s test over the linked table: each class the
/// extension declared, with its name, in table order.
fn extension_classes(req: &Request, extension: ExtensionId) -> Vec<(ClassId, Vec<u8>)> {
    req.declared_classes()
        .into_iter()
        .filter(|&class| req.class_extension(class) == Some(extension))
        .map(|class| (class, class_name_of(req, class)))
        .collect()
}

/// `zend_reflection_class_factory`: a `ReflectionClass`, or a
/// `ReflectionEnum` for an enum, with `$name` written and no
/// constructor served.
fn reflection_class_factory(req: &mut Request, class: ClassId) -> Result<PhpObject, Thrown> {
    let is_enum = req
        .class_decl(class)
        .is_some_and(|decl| decl.kind == ClassKind::Enum);
    let reflector = if is_enum {
        lookup_class(req, b"ReflectionEnum")?
    } else {
        None
    };
    let reflector = match reflector {
        Some(reflector) => reflector,
        None => class_named(req, b"ReflectionClass")?,
    };
    let object = new_object(req, reflector)?;
    let name = class_name_of(req, class);
    set_name(req, object, &name)?;
    Ok(object)
}

/// `ReflectionExtension::getVersion()`: `module->version`, and `null`
/// for `NO_VERSION_YET`. Every extension compiled in here carries
/// `PHP_VERSION`, as `PHP_CTYPE_VERSION` and its kin do, which is what
/// `_extension_string`'s header prints.
///
/// ```php
/// var_dump((new ReflectionExtension('json'))->getVersion());
/// ```
/// ```expect
/// string(5) "8.5.9"
/// ```
fn reflection_extension_get_version(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_extension(req)?;
    string_value(req, PHP_VERSION.as_bytes())
}

/// `ReflectionExtension::getFunctions()`: one `ReflectionFunction` per
/// native the extension declared, keyed by
/// `fptr->common.function_name`, which for a native is the lowered
/// name php registered it under.
///
/// ```php
/// var_dump((new ReflectionExtension('json'))->getFunctions());
/// ```
/// ```expect
/// array(5) {
///   ["json_encode"]=>
///   object(ReflectionFunction)#2 (1) {
///     ["name"]=>
///     string(11) "json_encode"
///   }
///   ["json_decode"]=>
///   object(ReflectionFunction)#3 (1) {
///     ["name"]=>
///     string(11) "json_decode"
///   }
///   ["json_validate"]=>
///   object(ReflectionFunction)#4 (1) {
///     ["name"]=>
///     string(13) "json_validate"
///   }
///   ["json_last_error"]=>
///   object(ReflectionFunction)#5 (1) {
///     ["name"]=>
///     string(15) "json_last_error"
///   }
///   ["json_last_error_msg"]=>
///   object(ReflectionFunction)#6 (1) {
///     ["name"]=>
///     string(19) "json_last_error_msg"
///   }
/// }
/// ```
fn reflection_extension_get_functions(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let reflector = class_named(req, b"ReflectionFunction")?;
    let mut functions = PhpArray::new(req)?;
    for function in req.extension_functions(extension) {
        let Some(name) = req.function_decl(function).map(|decl| decl.name.0.to_vec()) else {
            continue;
        };
        let object = new_object(req, reflector)?;
        set_name(req, object, &name)?;
        let key = Key::Str(PhpString::new(req, &name)?);
        functions.set(req, key, object.as_value())?;
    }
    Ok(Value::array(functions))
}

/// `ReflectionExtension::getConstants()`: the constants the extension
/// declared, keyed by name, each value the constant's.
///
/// ```php
/// var_dump((new ReflectionExtension('ctype'))->getConstants());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
fn reflection_extension_get_constants(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let mut constants = PhpArray::new(req)?;
    for declared in req.extension_constants(extension) {
        let name = declared.0.to_vec();
        let value = constant(req, &name)?.unwrap_or_default();
        let key = Key::Str(PhpString::new(req, &name)?);
        constants.set(req, key, value)?;
    }
    Ok(Value::array(constants))
}

/// `ReflectionExtension::getDependencies()`: `module->deps` walked into
/// an array keyed by the other extension's name, each value the kind,
/// `Required`, `Conflicts` or `Optional`, then the row's `rel` and
/// `version` each after one space where set; the empty array where the
/// pointer is null, as it is for `ctype`.
///
/// ```php
/// var_dump((new ReflectionExtension('ctype'))->getDependencies());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
fn reflection_extension_get_dependencies(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let mut dependencies = PhpArray::new(req)?;
    for dependency in req.extension_dependencies(extension) {
        let mut relation = String::from(match dependency.kind {
            DependencyKind::Required => "Required",
            DependencyKind::Conflicts => "Conflicts",
            DependencyKind::Optional => "Optional",
        });
        for part in [&dependency.rel, &dependency.version].into_iter().flatten() {
            relation.push(' ');
            relation.push_str(part);
        }
        let key = Key::Str(PhpString::new(req, dependency.name.as_bytes())?);
        let value = Value::string(PhpString::new(req, relation.as_bytes())?);
        dependencies.set(req, key, value)?;
    }
    Ok(Value::array(dependencies))
}

/// `ReflectionExtension::info()`: `php_info_print_module` over the
/// extension, the name then its `MINFO` table, written to the body as
/// the CLI prints it; `null` returned, as a `void` method is.
///
/// ```php
/// (new ReflectionExtension('ctype'))->info();
/// ```
/// ```expect
/// ctype
///
/// ctype functions => enabled
/// ```
fn reflection_extension_info(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    req.extension_info(extension);
    Ok(Value::null())
}

/// `ReflectionExtension::isPersistent()`:
/// `module->type == MODULE_PERSISTENT`, which every extension compiled
/// into the binary is; `MODULE_TEMPORARY` is `dl()`'s, which this
/// engine has none of.
///
/// ```php
/// $e = new ReflectionExtension('ctype'); var_dump($e->isPersistent(), $e->isTemporary());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn reflection_extension_is_persistent(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_extension(req)?;
    Ok(Value::bool(true))
}

/// `ReflectionExtension::isTemporary()`:
/// `module->type == MODULE_TEMPORARY`, the complement of
/// `isPersistent`, whose witness this shares.
fn reflection_extension_is_temporary(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_extension(req)?;
    Ok(Value::bool(false))
}

/// `ReflectionFunctionAbstract::isDeprecated()`:
/// `_function_check_flag(ZEND_ACC_DEPRECATED)`, the bit the
/// `#[\Deprecated]` attribute sets on a native's or a user function's
/// declaration, read from the attribute as `_function_string` reads it
/// for `, deprecated`. `ctype` deprecates none of its eleven.
///
/// ```php
/// var_dump((new ReflectionFunction('ctype_alpha'))->isDeprecated());
/// ```
/// ```expect
/// bool(false)
/// ```
fn reflection_function_is_deprecated(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, name) = this_name(req, "ReflectionFunction")?;
    let Some(function) = lookup_function(req, &name) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Function {}() does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let deprecated = req.function_decl(function).is_some_and(|decl| {
        decl.attributes
            .iter()
            .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated"))
    });
    Ok(Value::bool(deprecated))
}

/// `ReflectionFunction::__construct(Closure|string $function)`: the
/// function under the lowered name, a leading `\` dropped, its declared
/// name kept as `$name`; a name the tables have not is php's
/// `ReflectionException`, the name as given. A `Closure` is
/// `zend_get_closure_method_def` over the object, which the reflector
/// keeps as php keeps `intern->obj`, and `$name` is the
/// `{closure:file:line}` the compiler built for it; an object of any
/// other class is the type error.
///
/// ```php
/// $r = new ReflectionFunction(function ($a, $b = 1) {});
/// var_dump($r->isClosure(), $r->getNumberOfParameters(), $r->getNumberOfRequiredParameters(), $r->isUserDefined());
/// echo $r, "\n";
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// int(1)
/// bool(true)
/// Closure [ <user> function {closure:%s:2} ] {
///   @@ %s 2 - 2
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $a ]
///     Parameter #1 [ <optional> $b = 1 ]
///   }
/// }
/// ```
///
/// ```php
/// new ReflectionFunction(new stdClass);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionFunction::__construct(): Argument #1 ($function) must be of type Closure|string, stdClass given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionFunction->__construct(Object(stdClass))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionFunction('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Function nope() does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionFunction->__construct('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_function_construct(req: &mut Request) -> Result<Value, Thrown> {
    if let Some(given) = req.argument(1)
        && let ValueRef::Object(object) = given.deref().as_ref()
    {
        let object = *object;
        // php's `IS_OBJECT` arm: `zend_get_closure_method_def` and
        // `intern->obj` holding the closure, `$name` the declaration's
        // own, which is the `{closure:…}` the compiler built.
        if let Some(decl) = signature::closure_decl(req, object) {
            if let Some(this) = req.this() {
                set_name(req, this, &decl.name.0)?;
                keep_closure(req, this, object);
            }
            return Ok(Value::null());
        }
        let class = req.class_decl(object.class()).map_or_else(
            || String::from("object"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        );
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type Closure|string, {class} given"),
        ));
    }
    let name = one_string_argument(req)?;
    let bare = name.strip_prefix(b"\\").unwrap_or(&name);
    let Some(function) = lookup_function(req, bare) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Function {}() does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let declared = req
        .function_decl(function)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default();
    if let Some(this) = req.this() {
        set_name(req, this, &declared)?;
    }
    Ok(Value::null())
}

/// `ReflectionFunction::__toString()`: `_function_string` over the
/// function `$name` names, no scope, indent empty.
///
/// ```php
/// echo new ReflectionFunction('ctype_alpha');
/// ```
/// ```expect
/// Function [ <internal:ctype> function ctype_alpha ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $text ]
///   }
///   - Return [ bool ]
/// }
/// ```
fn reflection_function_to_string(req: &mut Request) -> Result<Value, Thrown> {
    let (this, name) = this_name(req, "ReflectionFunction")?;
    if let Some(closure) = kept_closure(req, this)
        && let Some(function) = closure.closure(req).map(|info| info.function)
    {
        let text = function_string(req, function, "");
        return Ok(Value::string(PhpString::new(req, text.as_bytes())?));
    }
    let Some(function) = lookup_function(req, &name) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Function {}() does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let text = function_string(req, function, "");
    Ok(Value::string(PhpString::new(req, text.as_bytes())?))
}

// -------------------------------------------------- ReflectionClass

/// `GET_REFLECTION_OBJECT_PTR(ce)` for a class reflector: the linked
/// class `$name` names on `ZEND_THIS`, looked up again since the
/// reflector keeps the name and not php's `intern->ptr`.
fn this_class(req: &mut Request) -> Result<ClassId, Thrown> {
    let (_, name) = this_name(req, "ReflectionClass")?;
    class_named(req, &name)
}

/// `ce->name` as bytes, empty for an identity the tables have no
/// declaration for.
fn class_name_of(req: &Request, class: ClassId) -> Vec<u8> {
    req.class_decl(class)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default()
}

/// The class and its parents, nearest first: what php has in one
/// class entry after `zend_do_inheritance` copied the parent's tables
/// into the child's, and what every walk below reads instead.
fn class_chain(req: &mut Request, class: ClassId) -> Vec<ClassId> {
    let mut chain = vec![class];
    let mut at = class;
    while chain.len() <= 64 {
        let Some(parent) = req
            .class_decl(at)
            .and_then(|decl| decl.parent.clone())
            .and_then(|name| lookup_class(req, &name.0).ok().flatten())
        else {
            break;
        };
        chain.push(parent);
        at = parent;
    }
    chain
}

/// `instanceof_function(ce, of)` over identities: `ce` is `of`,
/// extends it, or implements it, each class of the chain's interfaces
/// walked with it.
fn class_is(req: &mut Request, class: ClassId, of: ClassId, depth: u32) -> bool {
    if class == of {
        return true;
    }
    if depth > 64 {
        return false;
    }
    let Some(decl) = req.class_decl(class) else {
        return false;
    };
    let names: Vec<Name> = decl
        .interfaces
        .iter()
        .chain(decl.parent.iter())
        .cloned()
        .collect();
    names.iter().any(|name| {
        lookup_class(req, &name.0)
            .ok()
            .flatten()
            .is_some_and(|linked| class_is(req, linked, of, depth + 1))
    })
}

/// `zend_memrchr(name, '\\')`: where the namespace ends, if the name
/// has one.
fn namespace_end(name: &[u8]) -> Option<usize> {
    name.iter().rposition(|&b| b == b'\\')
}

/// `reflection_class_object_ctor` with `is_object` off,
/// `ReflectionClass::__construct(object|string $objectOrClass)`: an
/// object's class or the class named, its declared name kept as
/// `$name`.
///
/// ```php
/// $r = new ReflectionClass('reflectionexception'); var_dump($r, (new ReflectionClass(new ReflectionException))->name);
/// ```
/// ```expect
/// object(ReflectionClass)#1 (1) {
///   ["name"]=>
///   string(19) "ReflectionException"
/// }
/// string(19) "ReflectionException"
/// ```
///
/// ```php
/// new ReflectionClass('Nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->__construct('Nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// The `TypeError` for an array, `must be of type object|string, array
/// given`, has no fence until the compiler reads an array expression.
///
/// ```php
/// new ReflectionClass(null);
/// ```
/// ```expect exit=255
/// Deprecated: ReflectionClass::__construct(): Passing null to parameter #1 ($objectOrClass) of type object|string is deprecated in %s on line 2
///
/// Fatal error: Uncaught ReflectionException: Class "" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->__construct('')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionClass();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::__construct() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->__construct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_class_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let (class, _) = class_argument(req, 1)?;
    let name = class_name_of(req, class);
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
    }
    Ok(Value::null())
}

/// `reflection_class_object_ctor` with `is_object` on,
/// `ReflectionObject::__construct(object $object)`: the object's class
/// as `$name`, and the object itself as `intern->obj`, one more owner,
/// which the `Dynamic properties` block of `__toString` reads and
/// `free_obj` gives back.
///
/// ```php
/// $t = new stdClass; $t->b = 2;
/// echo new ReflectionObject($t);
/// ```
/// ```expect
/// Object of class [ <internal:Core> class stdClass ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [0] {
///   }
///
///   - Dynamic properties [1] {
///     Property [ <dynamic> public $b ]
///   }
///
///   - Methods [0] {
///   }
/// }
/// ```
///
/// ```php
/// var_dump(new ReflectionObject(new ReflectionException));
/// ```
/// ```expect
/// object(ReflectionObject)#1 (1) {
///   ["name"]=>
///   string(19) "ReflectionException"
/// }
/// ```
///
/// ```php
/// new ReflectionObject('x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionObject::__construct(): Argument #1 ($object) must be of type object, string given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionObject->__construct('x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_object_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let ValueRef::Object(object) = given.as_ref() else {
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type object, {} given", value_name(req, &given)),
        ));
    };
    let object = *object;
    let name = class_name_of(req, object.class());
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
        keep(req, this, object.class(), Some(object));
    }
    Ok(Value::null())
}

/// `ReflectionClass::getName()`: `ce->name`. `ReflectionObject` has no
/// `getName` of its own and answers with this one, as php's inherited
/// function table does.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('c'))->getName(), (new ReflectionObject(new C))->getName());
/// ```
/// ```expect
/// string(1) "C"
/// string(1) "C"
/// ```
///
/// ```php
/// ReflectionObject::getName();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Non-static method ReflectionClass::getName() cannot be called statically in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $r = new ReflectionClass('ReflectionException'); var_dump($r->getName(1));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getName() expects exactly 0 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getName(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_class_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    string_value(req, &name)
}

/// `ReflectionClass::isInternal()`: `ce->type == ZEND_INTERNAL_CLASS`,
/// which for the declaration model is a class with no file.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('C'))->isInternal(), (new ReflectionClass('ReflectionClass'))->isInternal(), (new ReflectionClass('C'))->isUserDefined(), (new ReflectionObject(new ReflectionProperty('Exception', 'message')))->isUserDefined());
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn reflection_class_is_internal(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(Value::bool(is_internal(req, class)))
}

/// `ReflectionClass::isUserDefined()`: `ce->type == ZEND_USER_CLASS`.
/// The witness is `isInternal`'s above.
fn reflection_class_is_user_defined(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(Value::bool(!is_internal(req, class)))
}

fn is_internal(req: &Request, class: ClassId) -> bool {
    req.class_decl(class)
        .is_none_or(|decl| decl.file.0.is_empty())
}

/// `ReflectionClass::getFileName()`: `ce->info.user.filename` for a
/// user class, `false` for an internal one. The three site accessors
/// share this witness; the name is the file's as the runner names
/// it, a path under the oracle and `witness.php` here.
///
/// ```php
/// $r = new ReflectionClass('C'); var_dump($r->getFileName(), $r->getStartLine(), $r->getEndLine()); $r = new ReflectionClass('Exception'); var_dump($r->getFileName(), $r->getStartLine(), $r->getEndLine()); class C { public $a; }
/// ```
/// ```expect
/// string(%d) "%s.php"
/// int(2)
/// int(2)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn reflection_class_get_file_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let file = req
        .class_decl(class)
        .filter(|decl| !decl.file.0.is_empty())
        .map(|decl| decl.file.0.to_vec());
    match file {
        Some(file) => string_value(req, &file),
        None => Ok(Value::bool(false)),
    }
}

/// `ReflectionClass::getStartLine()`: `ce->info.user.line_start`, or
/// `false` for an internal class. The witness is `getFileName`'s.
fn reflection_class_get_start_line(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(user_line(req, class, |decl| decl.line))
}

/// `ReflectionClass::getEndLine()`: `ce->info.user.line_end`, the line
/// of the closing brace, or `false` for an internal class. The witness
/// is `getFileName`'s.
fn reflection_class_get_end_line(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(user_line(req, class, |decl| decl.end_line))
}

fn user_line(req: &Request, class: ClassId, line: fn(&ClassDecl) -> u32) -> Value {
    req.class_decl(class)
        .filter(|decl| !decl.file.0.is_empty())
        .map_or(Value::bool(false), |decl| Value::int(i64::from(line(decl))))
}

/// `ReflectionClass::getExtension()`: `ce->info.internal.module` as a
/// `ReflectionExtension`, made as `reflection_extension_factory` makes
/// it, `object_init_ex` and the name written, no constructor served;
/// `null` for a user class. A class Zend declares itself is php's
/// `Core` `zend_module_entry`, which this engine has no `Extension` value for yet
/// (#379), so `Exception` answers `null` here and `Core` there; the
/// fence stays off that class until the bundle names it.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('ReflectionException'))->getExtension(), (new ReflectionClass('ReflectionException'))->getExtensionName(), (new ReflectionClass('C'))->getExtension(), (new ReflectionClass('C'))->getExtensionName());
/// ```
/// ```expect
/// object(ReflectionExtension)#2 (1) {
///   ["name"]=>
///   string(10) "Reflection"
/// }
/// string(10) "Reflection"
/// NULL
/// bool(false)
/// ```
fn reflection_class_get_extension(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let Some(name) = class_extension_name(req, class) else {
        return Ok(Value::null());
    };
    let Some(reflector) = lookup_class(req, b"ReflectionExtension")? else {
        return Ok(Value::null());
    };
    let object = new_object(req, reflector)?;
    set_name(req, object, name.as_bytes())?;
    Ok(Value::object(object))
}

/// `ReflectionClass::getExtensionName()`: the extension's name, or `false`
/// for a user class. The witness is `getExtension`'s.
fn reflection_class_get_extension_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    match class_extension_name(req, class) {
        Some(name) => string_value(req, name.as_bytes()),
        None => Ok(Value::bool(false)),
    }
}

fn class_extension_name(req: &Request, class: ClassId) -> Option<String> {
    let extension = req.class_extension(class)?;
    req.extension_name(extension).map(str::to_owned)
}

/// `ReflectionClass::hasConstant(string $name)`:
/// `zend_hash_exists(&ce->constants_table, name)`, the table php fills
/// with the class's own constants and every parent's but the private
/// ones (`do_inherit_class_constant`). A scalar is the string it
/// coerces to, so `hasConstant(1)` asks for `"1"`.
///
/// ```php
/// class Foo { const c1 = 1; } $r = new ReflectionClass('Foo'); var_dump($r->hasConstant('c1'), $r->hasConstant('c2'), $r->hasConstant(1), (new ReflectionClass('ReflectionClass'))->hasConstant('IS_FINAL'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
fn reflection_class_has_constant(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    Ok(Value::bool(
        class_constant_named(req, class, &name).is_some(),
    ))
}

/// The constant `name` names in `ce->constants_table`: the class's own or
/// a parent's non-private one, with the class that declares it.
fn class_constant_named(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Option<(ClassId, ClassConstDecl)> {
    for (depth, at) in class_chain(req, class).into_iter().enumerate() {
        let found =
            req.class_decl(at)?.constants.iter().find(|c| {
                &*c.name.0 == name && (depth == 0 || c.visibility != Visibility::Private)
            });
        if let Some(constant) = found {
            return Some((at, constant.clone()));
        }
    }
    None
}

/// `ReflectionClass::getConstant(string $name)`: the constant's value,
/// its expression evaluated in the declaring class
/// (`zend_update_class_constant`); a name the table has not is the 8.4
/// deprecation and `false`.
///
/// ```php
/// class Foo { const c1 = 1; } $r = new ReflectionClass('Foo'); var_dump($r->getConstant('c1'), (new ReflectionClass('ReflectionClass'))->getConstant('IS_FINAL')); var_dump($r->getConstant('c2'));
/// ```
/// ```expect
/// int(1)
/// int(32)
///
/// Deprecated: ReflectionClass::getConstant() for a non-existent constant is deprecated, use ReflectionClass::hasConstant() to check if the constant exists in %s on line 2
/// bool(false)
/// ```
fn reflection_class_get_constant(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    if let Some((declaring, constant)) = class_constant_named(req, class, &name) {
        return eval_const_expr(req, &constant.value, Some(declaring));
    }
    deprecated(
        req,
        "ReflectionClass::getConstant() for a non-existent constant is deprecated, use ReflectionClass::hasConstant() to check if the constant exists",
    );
    Ok(Value::bool(false))
}

/// `ReflectionClass::hasProperty(string $name)`: the name in
/// `ce->properties_info`, the class's own properties and every
/// parent's, a parent's private one excluded as php's
/// `property_info->ce == ce` test excludes it. php then asks a
/// `ReflectionObject`'s instance for a dynamic property of that name;
/// the reflector here keeps no instance, so that branch is `false`
/// until it does.
///
/// ```php
/// class C { public $a; protected $b; private $c; public static $d; } $r = new ReflectionClass('C'); $e = new ReflectionClass('ReflectionException'); var_dump($r->hasProperty('a'), $r->hasProperty('b'), $r->hasProperty('c'), $r->hasProperty('d'), $r->hasProperty('e'), $r->hasProperty(1.5), $e->hasProperty('message'), $e->hasProperty('string'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
fn reflection_class_has_property(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    Ok(Value::bool(property_named(req, class, &name).is_some()))
}

/// The property `name` names in the class's info table: the class's
/// own or a parent's non-private one, with the class that declares it.
fn property_named(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Option<(ClassId, PropertyDecl)> {
    for (depth, at) in class_chain(req, class).into_iter().enumerate() {
        let found =
            req.class_decl(at)?.properties.iter().find(|p| {
                &*p.name.0 == name && (depth == 0 || p.visibility != Visibility::Private)
            });
        if let Some(property) = found {
            return Some((at, property.clone()));
        }
    }
    None
}

/// `ReflectionClass::getDefaultProperties()`: `add_class_vars` twice,
/// the `static` properties then the instance ones, each in
/// `properties_info` order, the class's own before the inherited, a
/// parent's private ones skipped, and a typed property with no
/// default skipped as its `UNDEF` slot is; each default is the
/// expression it was written as, evaluated in the declaring class.
///
/// ```php
/// class Foo { protected static $s = 'foo'; protected $i = 'foo'; public $n; } var_dump((new ReflectionClass('Foo'))->getDefaultProperties(), (new ReflectionClass('ReflectionException'))->getDefaultProperties());
/// ```
/// ```expect
/// array(3) {
///   ["s"]=>
///   string(3) "foo"
///   ["i"]=>
///   string(3) "foo"
///   ["n"]=>
///   NULL
/// }
/// array(4) {
///   ["message"]=>
///   string(0) ""
///   ["code"]=>
///   int(0)
///   ["file"]=>
///   string(0) ""
///   ["line"]=>
///   int(0)
/// }
/// ```
fn reflection_class_get_default_properties(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let mut defaults = PhpArray::new(req)?;
    let chain = class_chain(req, class);
    for statics in [true, false] {
        for (depth, &at) in chain.iter().enumerate() {
            let properties: Vec<PropertyDecl> = req
                .class_decl(at)
                .map(|decl| decl.properties.clone())
                .unwrap_or_default();
            for property in properties {
                if property.is_instance == statics
                    || (depth > 0 && property.visibility == Visibility::Private)
                {
                    continue;
                }
                let value = match &property.default {
                    Some(expr) => eval_const_expr(req, expr, Some(at))?,
                    None if property.ty.is_some() => continue,
                    None => Value::null(),
                };
                let key = Key::Str(PhpString::new(req, &property.name.0)?);
                if defaults.contains(&key) {
                    continue;
                }
                defaults.set(req, key, value)?;
            }
        }
    }
    Ok(Value::array(defaults))
}

/// `ReflectionClass::isSubclassOf(ReflectionClass|string $class)`:
/// `ce != class_ce && instanceof_function(ce, class_ce)`, so a class
/// is not a subclass of itself and is one of every parent and every
/// interface up the chain. A `ReflectionClass` argument is the class
/// it reflects; a string is looked up, a miss php's `ReflectionException`.
///
/// ```php
/// class A {} $a = new ReflectionClass('A'); $e = new ReflectionClass('ReflectionException'); var_dump($a->isSubclassOf($a), $e->isSubclassOf('exception'), $e->isSubclassOf(new ReflectionClass('Throwable')), $e->isSubclassOf('Stringable'), $e->isSubclassOf('ReflectionException'), $a->isSubclassOf('ReflectionException'));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// class A {} (new ReflectionClass('A'))->isSubclassOf('X');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class "X" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->isSubclassOf('X')
/// #1 {main}
///   thrown in %s on line 2
/// ```
/// The `TypeError` for an array has no fence until the compiler reads
/// an array expression.
fn reflection_class_is_subclass_of(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let class = this_class(req)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let of = match given.as_ref() {
        ValueRef::Object(object) => {
            let reflector = lookup_class(req, b"ReflectionClass")?;
            if !reflector.is_some_and(|reflector| object.instance_of(req, reflector)) {
                let name = class_name_of(req, object.class());
                return Err(arg_type_error(
                    req,
                    1,
                    &format!(
                        "be of type ReflectionClass|string, {} given",
                        String::from_utf8_lossy(&name)
                    ),
                ));
            }
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
            class_named(req, &name)?
        }
        ValueRef::Array(_) | ValueRef::Resource(_) => {
            return Err(arg_type_error(
                req,
                1,
                &format!(
                    "be of type ReflectionClass|string, {} given",
                    value_name(req, &given)
                ),
            ));
        }
        _ => {
            let name = string_argument(req, 1)?;
            class_named(req, &name)?
        }
    };
    Ok(Value::bool(class != of && class_is(req, class, of, 0)))
}

/// `ReflectionClass::inNamespace()`: whether `ce->name` has a `\`.
///
/// ```php
/// $r = new ReflectionClass('ReflectionException'); var_dump($r->inNamespace(), $r->getNamespaceName(), $r->getShortName());
/// ```
/// ```expect
/// bool(false)
/// string(0) ""
/// string(19) "ReflectionException"
/// ```
fn reflection_class_in_namespace(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    Ok(Value::bool(namespace_end(&name).is_some()))
}

/// `ReflectionClass::getNamespaceName()`: the name up to its last `\`,
/// empty without one. The witness is `inNamespace`'s.
fn reflection_class_get_namespace_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    let end = namespace_end(&name).unwrap_or(0);
    string_value(req, &name[..end])
}

/// `ReflectionClass::getShortName()`: the name after its last `\`, the
/// whole name without one. The witness is `inNamespace`'s.
fn reflection_class_get_short_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    let start = namespace_end(&name).map_or(0, |end| end + 1);
    string_value(req, &name[start..])
}

/// `ReflectionClass::getAttributes(?string $name = null, int $flags =
/// 0)`: `reflect_attributes` over `ce->attributes` with
/// `ZEND_ATTRIBUTE_TARGET_CLASS`, an empty array for a class with none.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('C'))->getAttributes(), (new ReflectionClass('ReflectionClass'))->getAttributes());
/// try { (new ReflectionClass('C'))->getAttributes(null, 4); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { (new ReflectionClass('C'))->getAttributes('Nope', ReflectionAttribute::IS_INSTANCEOF); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(0) {
/// }
/// array(0) {
/// }
/// ValueError: ReflectionClass::getAttributes(): Argument #2 ($flags) must be a valid attribute filter flag
/// Error: Class "Nope" not found
/// ```
///
/// ```php
/// (new ReflectionClass('ReflectionException'))->getAttributes(1, 2, 3);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getAttributes() expects at most 2 arguments, 3 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getAttributes(1, 2, 3)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_class_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    let class = this_class(req)?;
    let attributes = req
        .class_decl(class)
        .map_or_else(Vec::new, |decl| decl.attributes.clone());
    attr::reflect_attributes(req, &attributes, Some(class), attr::TARGET_CLASS)
}

// -------------------------------------------------- ReflectionClass::__toString

/// `ReflectionClass::__toString()`: `_class_string` over the class
/// `$name` names, no object, indent empty. `JsonException` is the
/// first class of the bundle with inherited members: `Exception`'s
/// four protected properties, its three private ones shadowed and
/// skipped, and ten of its eleven methods, the private `__clone`
/// skipped, each `inherits Exception` with the prototype the interface
/// gave it and `ctor` on the constructor.
///
/// ```php
/// echo new ReflectionClass('JsonException');
/// ```
/// ```expect
/// Class [ <internal:json> class JsonException extends Exception implements Throwable, Stringable ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [4] {
///     Property [ protected $message = '' ]
///     Property [ protected $code = 0 ]
///     Property [ protected string $file = '' ]
///     Property [ protected int $line = 0 ]
///   }
///
///   - Methods [10] {
///     Method [ <internal:Core, inherits Exception, ctor> public method __construct ] {
///
///       - Parameters [3] {
///         Parameter #0 [ <optional> string $message = "" ]
///         Parameter #1 [ <optional> int $code = 0 ]
///         Parameter #2 [ <optional> ?Throwable $previous = null ]
///       }
///     }
///
///     Method [ <internal:Core, inherits Exception> public method __wakeup ] {
///
///       - Parameters [0] {
///       }
///       - Tentative return [ void ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getMessage ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getCode ] {
///
///       - Parameters [0] {
///       }
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getFile ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getLine ] {
///
///       - Parameters [0] {
///       }
///       - Return [ int ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getTrace ] {
///
///       - Parameters [0] {
///       }
///       - Return [ array ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getPrevious ] {
///
///       - Parameters [0] {
///       }
///       - Return [ ?Throwable ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getTraceAsString ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core, inherits Exception, prototype Stringable> public method __toString ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///   }
/// }
/// ```
///
/// ```php
/// echo new ReflectionClass('Throwable');
/// ```
/// ```expect
/// Interface [ <internal:Core> interface Throwable extends Stringable ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [0] {
///   }
///
///   - Methods [8] {
///     Method [ <internal:Core> abstract public method getMessage ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core> abstract public method getCode ] {
///
///       - Parameters [0] {
///       }
///     }
///
///     Method [ <internal:Core> abstract public method getFile ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core> abstract public method getLine ] {
///
///       - Parameters [0] {
///       }
///       - Return [ int ]
///     }
///
///     Method [ <internal:Core> abstract public method getTrace ] {
///
///       - Parameters [0] {
///       }
///       - Return [ array ]
///     }
///
///     Method [ <internal:Core> abstract public method getPrevious ] {
///
///       - Parameters [0] {
///       }
///       - Return [ ?Throwable ]
///     }
///
///     Method [ <internal:Core> abstract public method getTraceAsString ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///
///     Method [ <internal:Core, inherits Stringable> abstract public method __toString ] {
///
///       - Parameters [0] {
///       }
///       - Return [ string ]
///     }
///   }
/// }
/// ```
///
/// A user class with a parent and an interface renders the same way,
/// with no `getParentClass()` asked for it (#388): the header carries
/// `extends P implements I`, the class's own constants and properties
/// come before the ones it inherits, and an interface's constant is in
/// the table too.
///
/// ```php
/// interface I { const J = 3; }
/// class P { const A = 1; public $x = 1; }
/// class C extends P implements I { const B = 2; public $y = 2; }
/// echo new ReflectionClass('C');
/// ```
/// ```expect
/// Class [ <user> class C extends P implements I ] {
///   @@ %s 4-4
///
///   - Constants [3] {
///     Constant [ public int B ] { 2 }
///     Constant [ public int A ] { 1 }
///     Constant [ public int J ] { 3 }
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [2] {
///     Property [ public $y = 2 ]
///     Property [ public $x = 1 ]
///   }
///
///   - Methods [0] {
///   }
/// }
/// ```
fn reflection_class_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    // `_class_string(&str, ce, &intern->obj, "")`: the object reflector
    // hands the instance it kept, and the class reflector php's null
    // zval, which is what puts `Object of class` and the `Dynamic
    // properties` block on the one and not the other.
    let object = req.this().and_then(|this| kept_object(req, this));
    let text = class_string(req, class, object, "");
    string_value(req, text.as_bytes())
}

// -------------------------------------------------- ReflectionMethod

/// `instantiate_reflection_method` as the constructor,
/// `ReflectionMethod::__construct(object|string $objectOrMethod,
/// ?string $method = null)`: an object and a name, a class name and a
/// name, or one `Class::method` string, which 8.5 deprecates in favour
/// of `createFromMethodName`; the class is `zend_lookup_class` or the
/// `ReflectionException` every reflector throws, the method is the
/// linked table's entry under the lowered name or `Method C::m() does
/// not exist`. `$name` is the method's declared spelling and `$class`
/// its scope's name, `Exception` for a method `JsonException` inherits,
/// as `mptr->common.scope->name` is. php keeps the class it was asked
/// about too, `intern->ce`, which `__toString` renders against; the
/// reflector here keeps the two properties alone until a native
/// payload can hold it (#360), so it renders against the declaring
/// class.
///
/// ```php
/// $m = new ReflectionMethod('JsonException', 'getcode'); $n = new ReflectionMethod(new JsonException, 'getMessage'); var_dump($m->name, $m->class, $n->name, $n->class);
/// ```
/// ```expect
/// string(7) "getCode"
/// string(9) "Exception"
/// string(10) "getMessage"
/// string(9) "Exception"
/// ```
///
/// ```php
/// new ReflectionMethod('Exception::nope');
/// ```
/// ```expect exit=255
/// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
///
/// Fatal error: Uncaught ReflectionException: Method Exception::nope() does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionMethod->__construct('Exception::nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionMethod('nocolons');
/// ```
/// ```expect exit=255
/// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
///
/// Fatal error: Uncaught ReflectionException: ReflectionMethod::__construct(): Argument #1 ($objectOrMethod) must be a valid method name in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionMethod->__construct('nocolons')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionMethod(new Exception);
/// ```
/// ```expect exit=255
/// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
///
/// Fatal error: Uncaught ValueError: ReflectionMethod::__construct(): Argument #2 ($method) cannot be null when argument #1 ($objectOrMethod) is an object in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionMethod->__construct(Object(Exception))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionMethod();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionMethod::__construct() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionMethod->__construct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_method_construct(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Err(arg_count_error(req, "at least 1 argument, 0 given"));
    }
    at_most_arguments(req, 2)?;
    if count == 1 {
        deprecated(
            req,
            "Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead",
        );
    }
    let second = req
        .argument(2)
        .map(|v| v.deref())
        .filter(|v| !matches!(v.as_ref(), ValueRef::Null | ValueRef::Undef));
    let first = req.argument(1).unwrap_or_default().deref();
    let (class, method_name) = match (first.as_ref(), second) {
        (ValueRef::Object(object), Some(_)) => (object.class(), string_argument(req, 2)?),
        (ValueRef::Object(_), None) => {
            // `zend_argument_value_error` with no `must`.
            return Err(throw(
                req,
                "ValueError",
                "ReflectionMethod::__construct(): Argument #2 ($method) cannot be null when argument #1 ($objectOrMethod) is an object",
            ));
        }
        (ValueRef::Array(_) | ValueRef::Resource(_), _) => {
            return Err(arg_type_error(
                req,
                1,
                &format!(
                    "be of type object|string, {} given",
                    value_name(req, &first)
                ),
            ));
        }
        (_, Some(_)) => {
            let class = string_argument(req, 1)?;
            (class_named(req, &class)?, string_argument(req, 2)?)
        }
        (_, None) => {
            let given = string_argument(req, 1)?;
            let Some(at) = given.windows(2).position(|pair| pair == b"::") else {
                return Err(throw(
                    req,
                    "ReflectionException",
                    "ReflectionMethod::__construct(): Argument #1 ($objectOrMethod) must be a valid method name",
                ));
            };
            let class = class_named(req, &given[..at])?;
            (class, given[at + 2..].to_vec())
        }
    };
    let lowered = method_name.to_ascii_lowercase();
    let Some(found) = linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
    else {
        let class = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Method {}::{}() does not exist",
                String::from_utf8_lossy(&class),
                String::from_utf8_lossy(&method_name)
            ),
        ));
    };
    let scope = class_name_of(req, found.scope);
    if let Some(this) = req.this() {
        set_name(req, this, &found.decl.function.name.0)?;
        set_string_prop(req, this, b"class", &scope)?;
        // `intern->ce = ce`: the class the reflector was asked about,
        // which `_function_string` takes as the method's scope, where
        // `$class` is the class that declared the method.
        keep(req, this, class, None);
    }
    Ok(Value::null())
}

/// `ReflectionMethod::__toString()`: `_function_string` over the
/// method with `intern->ce`, the class the reflector was asked about,
/// as its scope, indent empty. The
/// tentative bit renders as `Tentative return`, a declared return
/// type as `Return`, and a method with neither has no row.
///
/// ```php
/// echo new ReflectionMethod('JsonException', 'getMessage');
/// ```
/// ```expect
/// Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getMessage ] {
///
///   - Parameters [0] {
///   }
///   - Return [ string ]
/// }
/// ```
///
/// ```php
/// echo new ReflectionMethod('JsonSerializable::jsonSerialize');
/// ```
/// ```expect
/// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 2
/// Method [ <internal:json> abstract public method jsonSerialize ] {
///
///   - Parameters [0] {
///   }
///   - Tentative return [ mixed ]
/// }
/// ```
///
/// ```php
/// echo new ReflectionMethod('Exception', 'getMessage'), new ReflectionMethod('Exception', 'getCode'), new ReflectionMethod('Exception', '__construct');
/// ```
/// ```expect
/// Method [ <internal:Core, prototype Throwable> final public method getMessage ] {
///
///   - Parameters [0] {
///   }
///   - Return [ string ]
/// }
/// Method [ <internal:Core, prototype Throwable> final public method getCode ] {
///
///   - Parameters [0] {
///   }
/// }
/// Method [ <internal:Core, ctor> public method __construct ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <optional> string $message = "" ]
///     Parameter #1 [ <optional> int $code = 0 ]
///     Parameter #2 [ <optional> ?Throwable $previous = null ]
///   }
/// }
/// ```
fn reflection_method_to_string(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (this, name) = this_name(req, "ReflectionMethod")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw(
            req,
            "Error",
            "Typed property ReflectionMethod::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let lowered = name.to_ascii_lowercase();
    let Some(found) = linked_methods(req, class)
        .into_iter()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
    else {
        let class = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Method {}::{}() does not exist",
                String::from_utf8_lossy(&class),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    // `_function_string(&str, mptr, intern->ce, "")`: the scope is the
    // class the reflector was asked about, so a method the class
    // inherited carries `inherits` and the declaring class's own
    // carries `overwrites`.
    let scope = kept_class(req, this).unwrap_or(class);
    let text = method_string(req, &found, scope, "");
    string_value(req, text.as_bytes())
}

// -------------------------------------------------- ReflectionProperty

/// `ReflectionProperty::__construct(object|string $class, string
/// $property)`: the property in the class's info table, or a dynamic
/// one on the object given, else php's `ReflectionException`; `$name` is the name
/// and `$class` the declaring class, `property_info->ce->name`.
///
/// ```php
/// var_dump(new ReflectionProperty('ReflectionException', 'message'));
/// ```
/// ```expect
/// object(ReflectionProperty)#1 (2) {
///   ["name"]=>
///   string(7) "message"
///   ["class"]=>
///   string(9) "Exception"
/// }
/// ```
///
/// ```php
/// new ReflectionProperty('ReflectionException', 'string');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Property ReflectionException::$string does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->__construct('ReflectionExcep...', 'string')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionProperty('Nope', 'x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->__construct('Nope', 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionProperty('ReflectionException');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::__construct() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->__construct('ReflectionExcep...')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_property_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 2)?;
    let (class, object) = class_argument(req, 1)?;
    let name = string_argument(req, 2)?;
    let declaring = match property_named(req, class, &name) {
        Some((declaring, _)) => declaring,
        None if object.is_some_and(|o| o.prop(req, &name).is_some()) => class,
        None => {
            let class = class_name_of(req, class);
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Property {}::${} does not exist",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(&name)
                ),
            ));
        }
    };
    let declaring = class_name_of(req, declaring);
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
        set_string_prop(req, this, b"class", &declaring)?;
        // `intern->ce = ce`: the class asked about, which `getValue`
        // holds its argument to, where `$class` is the declaring one.
        keep(req, this, class, None);
    }
    Ok(Value::null())
}

/// The property a `ReflectionProperty` reflects: its declaring class
/// (`$class`), its name, and its declaration where the class has one,
/// none for a dynamic property.
fn this_property(req: &mut Request) -> Result<(ClassId, Vec<u8>, Option<PropertyDecl>), Thrown> {
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
    let declared = property_named(req, class, &name).map(|(_, decl)| decl);
    Ok((class, name, declared))
}

/// `ReflectionProperty::getValue(?object $object = null)`: an instance
/// property read off the object given, which must be of the declaring
/// class; the read is the slot or the dynamic table by name, php's
/// `read_property` under `fake_scope`, so visibility is no bar. A
/// slot holds the declared default, which `object_properties_init` put
/// there when the object was made, an inherited property's included
/// (#419); a typed property with no default holds nothing and is php's
/// `Error` for a read before initialization. A `static` property is
/// `zend_read_static_property_ex` over the class the reflector was
/// asked about (`intern->ce`), whose fake scope is that class, and the
/// argument is neither read nor required; php parses the argument
/// before it looks at the property, so what is no object is a
/// `TypeError` either way.
///
/// ```php
/// class P { public static $s = 1; } class C extends P {}
/// $r = new ReflectionProperty('C', 's');
/// var_dump($r->class, $r->getValue(), $r->getValue(null));
/// ```
/// ```expect
/// string(1) "P"
/// int(1)
/// int(1)
/// ```
///
/// ```php
/// class T { protected $p = 2; } $o = new T; var_dump((new ReflectionProperty($o, 'p'))->getValue($o), (new ReflectionProperty('Exception', 'message'))->getValue(new ReflectionException), (new ReflectionProperty('Exception', 'code'))->getValue(new Exception));
/// ```
/// ```expect
/// int(2)
/// string(0) ""
/// int(0)
/// ```
///
/// ```php
/// (new ReflectionProperty('Exception', 'message'))->getValue();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionProperty::getValue(): Argument #1 ($object) must be provided for instance properties in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->getValue()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// class T { protected $p = 2; } (new ReflectionProperty('Exception', 'message'))->getValue(new T);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Given object is not an instance of the class this property was declared in in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->getValue(Object(T))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// (new ReflectionProperty('Exception', 'message'))->getValue(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionProperty::getValue(): Argument #1 ($object) must be of type ?object, int given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->getValue(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_property_get_value(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    if !matches!(
        given.as_ref(),
        ValueRef::Object(_) | ValueRef::Null | ValueRef::Undef
    ) {
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type ?object, {} given", value_name(req, &given)),
        ));
    }
    let (class, name, declared) = this_property(req)?;
    // `instanceof_function(Z_OBJCE_P(object), intern->ce)`: the object
    // is held to the class the reflector was asked about, which is the
    // subclass where the property was reached through one, and not to
    // the class that declared the property.
    let asked = req
        .this()
        .and_then(|this| kept_class(req, this))
        .unwrap_or(class);
    if declared.as_ref().is_some_and(|decl| !decl.is_instance) {
        return read_static_prop(req, asked, &name);
    }
    let ValueRef::Object(object) = given.as_ref() else {
        return Err(arg_type_error(
            req,
            1,
            "be provided for instance properties",
        ));
    };
    if !object.instance_of(req, asked) {
        return Err(throw(
            req,
            "ReflectionException",
            "Given object is not an instance of the class this property was declared in",
        ));
    }
    match object.prop(req, &name) {
        Some(value) => {
            let value = value.deref();
            req.addref(&value);
            Ok(value)
        }
        None if declared.as_ref().is_some_and(|decl| decl.ty.is_some()) => {
            let class = class_name_of(req, class);
            Err(throw(
                req,
                "Error",
                &format!(
                    "Typed property {}::${} must not be accessed before initialization",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(&name)
                ),
            ))
        }
        None => Ok(Value::null()),
    }
}

/// `ReflectionProperty::setValue(mixed $objectOrValue, mixed $value)`:
/// an instance property written on the object given through
/// `write_property` under `fake_scope`, one more owner of the value
/// taken for the slot. A `static` property takes php's two 8.3
/// deprecations, the one for a single argument and the one for a first
/// argument that is neither `null` nor an object, and is then
/// `zend_update_static_property_ex` over the class the reflector was
/// asked about; the value is the last argument either way.
///
/// ```php
/// class T { public static $s = 1; } $r = new ReflectionProperty('T', 's'); var_dump($r->getValue()); $r->setValue(null, 2); var_dump($r->getValue());
/// ```
/// ```expect
/// int(1)
/// int(2)
/// ```
///
/// The single-argument form, php's 8.3 deprecation, whose value is the
/// one argument:
///
/// ```php
/// class T { public static $s = 1; }
/// $r = new ReflectionProperty('T', 's');
/// $r->setValue(3);
/// var_dump(T::$s);
/// ```
/// ```expect
/// Deprecated: Calling ReflectionProperty::setValue() with a single argument is deprecated in %s on line 4
/// int(3)
/// ```
///
/// ```php
/// class T { public $p = 1; } $o = new T; $r = new ReflectionProperty($o, 'p'); $r->setValue($o, 'two'); var_dump($r->getValue($o), $o->p);
/// ```
/// ```expect
/// string(3) "two"
/// string(3) "two"
/// ```
///
/// ```php
/// class T { public $p = 1; } (new ReflectionProperty('T', 'p'))->setValue(new T);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::setValue() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->setValue(Object(T))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// (new ReflectionProperty('Exception', 'message'))->setValue(1, 2);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionProperty::setValue(): Argument #1 ($objectOrValue) must be of type object, int given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->setValue(1, 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_property_set_value(req: &mut Request) -> Result<Value, Thrown> {
    let (class, name, declared) = this_property(req)?;
    if declared.as_ref().is_some_and(|decl| !decl.is_instance) {
        let count = req.argument_count();
        let at = if count == 1 {
            deprecated(
                req,
                "Calling ReflectionProperty::setValue() with a single argument is deprecated",
            );
            1
        } else {
            exactly_arguments(req, 2)?;
            let first = req.argument(1).unwrap_or_default().deref();
            if !matches!(first.as_ref(), ValueRef::Null | ValueRef::Object(_)) {
                deprecated(
                    req,
                    "Calling ReflectionProperty::setValue() with a 1st argument which is not null or an object is deprecated",
                );
            }
            2
        };
        let asked = req
            .this()
            .and_then(|this| kept_class(req, this))
            .unwrap_or(class);
        let value = req.argument(at).unwrap_or_default().deref();
        req.addref(&value);
        write_static_prop(req, asked, &name, value)?;
        return Ok(Value::null());
    }
    exactly_arguments(req, 2)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let ValueRef::Object(object) = given.as_ref() else {
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type object, {} given", value_name(req, &given)),
        ));
    };
    let value = req.argument(2).unwrap_or_default().deref();
    req.addref(&value);
    let key = PhpString::new(req, &name)?;
    object.set_prop(req, key, value)?;
    Ok(Value::null())
}

/// `ReflectionProperty::getDocComment()`: `prop->doc_comment`, or
/// `false` where the declaration carries none and where there is no
/// declaration at all, php's `ref->prop && ref->prop->doc_comment`, a
/// dynamic property being the second case. A declarator takes the
/// comment that stands before the declaration only when it is the
/// first, `backup_doc_comment` reducing at each `property:`, so
/// `/** P */ public $x, $y;` gives it to `$x` alone.
///
/// ```php
/// class M { /** P */ public $x, $y; } var_dump((new ReflectionProperty('M', 'x'))->getDocComment(), (new ReflectionProperty('M', 'y'))->getDocComment(), (new ReflectionProperty('Exception', 'message'))->getDocComment());
/// ```
/// ```expect
/// string(8) "/** P */"
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// class M { public $x; } (new ReflectionProperty('M', 'x'))->getDocComment(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::getDocComment() expects exactly 0 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->getDocComment(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_property_get_doc_comment(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, _, declared) = this_property(req)?;
    match declared.and_then(|decl| decl.doc_comment) {
        Some(doc) => string_value(req, &doc),
        None => Ok(Value::bool(false)),
    }
}

// -------------------------------------------------- ReflectionClassConstant

/// `ReflectionClassConstant::__construct(object|string $class, string
/// $constant)`: the constant `zend_hash_find_ptr(CE_CONSTANTS_TABLE(ce),
/// constname)` finds, else php's `ReflectionException`; `$name` is the
/// name as given and `$class` is `constant->ce->name`, the class that
/// declared it, which for an inherited constant is the parent. A
/// parent's private constant is not in the table and so is a miss.
///
/// ```php
/// class M { /** K */ const A = 1, B = 2; } var_dump(new ReflectionClassConstant('M', 'A'), (new ReflectionClassConstant('ReflectionProperty', 'IS_PUBLIC'))->class);
/// ```
/// ```expect
/// object(ReflectionClassConstant)#1 (2) {
///   ["name"]=>
///   string(1) "A"
///   ["class"]=>
///   string(1) "M"
/// }
/// string(18) "ReflectionProperty"
/// ```
///
/// A parent's private constant is not in the child's table, so the
/// child is not where it is found (#257, #388):
///
/// ```php
/// class P { private const R = 2; }
/// class C extends P {}
/// new ReflectionClassConstant('C', 'R');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Constant C::R does not exist in %s:4
/// Stack trace:
/// #0 %s(4): ReflectionClassConstant->__construct('C', 'R')
/// #1 {main}
///   thrown in %s on line 4
/// ```
///
/// ```php
/// class M { const A = 1; } new ReflectionClassConstant('M', 'Z');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Constant M::Z does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClassConstant->__construct('M', 'Z')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionClassConstant('Nope', 'A');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClassConstant->__construct('Nope', 'A')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionClassConstant('Exception');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClassConstant::__construct() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClassConstant->__construct('Exception')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_class_constant_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 2)?;
    let (class, _) = class_argument(req, 1)?;
    let name = string_argument(req, 2)?;
    let Some((declaring, _)) = class_constant_named(req, class, &name) else {
        let class = class_name_of(req, class);
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Constant {}::{} does not exist",
                String::from_utf8_lossy(&class),
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let declaring = class_name_of(req, declaring);
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
        set_string_prop(req, this, b"class", &declaring)?;
    }
    Ok(Value::null())
}

/// The constant a `ReflectionClassConstant` reflects, looked up again
/// in the class `$class` names as php's `intern->ptr` points at the
/// entry it found: `None` for a name the table no longer has.
fn this_class_constant(req: &mut Request) -> Result<Option<ClassConstDecl>, Thrown> {
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
    Ok(class_constant_named(req, class, &name).map(|(_, decl)| decl))
}

/// `ReflectionClassConstant::getDocComment()`: `ref->doc_comment`, or
/// `false` where the declaration carries none. The constant
/// declarators take the comment as the property declarators do, the
/// first alone.
///
/// ```php
/// class M { /** K */ const A = 1, B = 2; } var_dump((new ReflectionClassConstant('M', 'A'))->getDocComment(), (new ReflectionClassConstant('M', 'B'))->getDocComment(), (new ReflectionClassConstant('ReflectionProperty', 'IS_PUBLIC'))->getDocComment());
/// ```
/// ```expect
/// string(8) "/** K */"
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// var_dump((new ReflectionClassConstant('ReflectionObject', 'IS_FINAL'))->class, (new ReflectionClassConstant('ReflectionObject', 'IS_FINAL'))->getDocComment());
/// ```
/// ```expect
/// string(15) "ReflectionClass"
/// bool(false)
/// ```
///
/// An inherited constant is the parent's declaration, so the comment is
/// the parent's and `$class` is the parent (#257, #388):
///
/// ```php
/// class P { /** R */ protected const R = 2; }
/// class C extends P {}
/// $r = new ReflectionClassConstant('C', 'R');
/// var_dump($r->class, $r->getDocComment());
/// ```
/// ```expect
/// string(1) "P"
/// string(8) "/** R */"
/// ```
fn reflection_class_constant_get_doc_comment(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    match this_class_constant(req)?.and_then(|decl| decl.doc_comment) {
        Some(doc) => string_value(req, &doc),
        None => Ok(Value::bool(false)),
    }
}

// -------------------------------------------------- ReflectionConstant

/// `ReflectionConstant::__construct(string $name)`: the constant under
/// the name with its namespace lowered and a leading `\` dropped,
/// `zend_get_constant_ptr`; a miss is php's `ReflectionException` with the name as
/// given, and `$name` keeps the spelling given too.
///
/// ```php
/// var_dump(new ReflectionConstant('\PHP_VERSION'), (new ReflectionConstant('PHP_EOL'))->name);
/// ```
/// ```expect
/// object(ReflectionConstant)#1 (1) {
///   ["name"]=>
///   string(12) "\PHP_VERSION"
/// }
/// string(7) "PHP_EOL"
/// ```
///
/// ```php
/// new ReflectionConstant('NOPE');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Constant "NOPE" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionConstant->__construct('NOPE')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionConstant();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionConstant::__construct() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionConstant->__construct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_constant_construct(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let bare = name.strip_prefix(b"\\").unwrap_or(&name);
    let mut lowered = bare.to_vec();
    if let Some(end) = namespace_end(bare) {
        lowered[..end].make_ascii_lowercase();
    }
    if constant(req, &lowered)?.is_none() {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Constant \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    }
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
    }
    Ok(Value::null())
}

/// `ReflectionConstant::getAttributes(?string $name = null, int $flags
/// = 0)`: `reflect_attributes` over `const_->attributes`. A constant
/// declaration (`ConstDecl`) carries no attributes, and no constant
/// this engine declares has one in php either (`E_STRICT`, the one
/// with `#[\Deprecated]`, is not declared), so the answer is the empty
/// array; `ReflectionConstant_getAttributes_internal.phpt` waits on
/// both.
///
/// ```php
/// var_dump((new ReflectionConstant('PHP_EOL'))->getAttributes());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
fn reflection_constant_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    this_name(req, "ReflectionConstant")?;
    attr::reflect_attributes(req, &[], None, attr::TARGET_CONST)
}

/// `_extension_string`: the header with the extension's number and
/// version, then the blocks it has something for: `Constants`, one
/// `_const_string` row per constant the extension declared;
/// `Functions`, one `_function_string` per native; `Classes`, one
/// `_class_string` per class, each preceded by a blank line as
/// `_extension_class_string` writes it. Every bundled extension is
/// `<persistent>`, as one compiled in is, and its version is
/// `PHP_VERSION`, as `PHP_CTYPE_VERSION` and its kin are. The `INI`
/// block is not rendered yet (#375); `json` has no ini entry.
fn extension_string(req: &mut Request, extension: ExtensionId, indent: &str) -> String {
    let mut out = String::new();
    let name = req
        .extension_name(extension)
        .map(str::to_owned)
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "{indent}Extension [ <persistent> extension #{} {name} version {PHP_VERSION} ] {{",
        req.extension_number(extension)
    );
    let constants = req.extension_constants(extension);
    if !constants.is_empty() {
        let _ = write!(out, "\n  - Constants [{}] {{\n", constants.len());
        for constant in &constants {
            out.push_str(&const_string(req, &constant.0, "    "));
        }
        let _ = writeln!(out, "{indent}  }}");
    }
    let functions = req.extension_functions(extension);
    if !functions.is_empty() {
        out.push_str("\n  - Functions {\n");
        for function in functions {
            out.push_str(&function_string(req, function, "    "));
        }
        let _ = writeln!(out, "{indent}  }}");
    }
    let classes = extension_classes(req, extension);
    if !classes.is_empty() {
        let _ = write!(out, "\n  - Classes [{}] {{", classes.len());
        let sub_indent = format!("{indent}    ");
        for (class, _) in classes {
            out.push('\n');
            out.push_str(&class_string(req, class, None, &sub_indent));
        }
        let _ = writeln!(out, "{indent}  }}");
    }
    let _ = writeln!(out, "{indent}}}");
    out
}

/// `_const_string`: `Constant [ <persistent> type NAME ] { value }`,
/// the flags every constant an extension registers at `MINIT` carries
/// (`CONST_PERSISTENT`; `no_file_cache` and `deprecated` are none of
/// the bundle's yet), the value's `zend_zval_type_name`, and the value
/// as `zval_get_string` prints it, `Array` for an array. The value is
/// the linked table's, read as `constant()` reads it.
fn const_string(req: &mut Request, name: &[u8], indent: &str) -> String {
    let value = constant(req, name).ok().flatten().unwrap_or_default();
    let shown = value_string(req, &value);
    let words = if constant_is_persistent(req, name) {
        "<persistent> "
    } else {
        ""
    };
    format!(
        "{indent}Constant [ {words}{} {} ] {{ {shown} }}\n",
        value.type_name(),
        String::from_utf8_lossy(name)
    )
}

/// `CONST_PERSISTENT`: the bit every constant an extension registered
/// at startup carries and one a running program defined does not,
/// which is `get_defined_constants()`'s own list here.
fn constant_is_persistent(req: &mut Request, name: &[u8]) -> bool {
    let Ok(key) = PhpString::new(req, name) else {
        return true;
    };
    let defined = req.constants().map(|table| table.contains(&Key::Str(key)));
    !defined.unwrap_or(false)
}

/// `zval_get_string` for a constant's value as `_const_string` and
/// `_class_const_string` print it: `Array` for an array, `Object` for
/// an object, and the string a scalar casts to.
fn value_string(req: &mut Request, value: &Value) -> String {
    match value.deref().as_ref() {
        ValueRef::Array(_) => String::from("Array"),
        ValueRef::Object(_) => String::from("Object"),
        _ => php_ext::coerce::to_string(req, value).map_or_else(
            |_| String::new(),
            |s| String::from_utf8_lossy(s.as_bytes()).into_owned(),
        ),
    }
}

/// `_class_string`: the header, then the `Constants`,
/// `Static properties`, `Static methods`, `Properties`,
/// `Dynamic properties` and `Methods` blocks in php's order, an enum's
/// `Enum cases` before
/// its `Constants`, each over the linked table with the counts php
/// prints, a parent's private member skipped as its shadow is
/// (`prop->ce != ce`, `mptr->common.scope != ce`). A user class carries
/// its `@@ file start-end` line. `object` is php's `obj` argument, the
/// instance an object reflector kept: it turns the header's word into
/// `Object of class` and is the only thing that puts a `Dynamic
/// properties` block there at all. `<iterateable>` is not rendered yet
/// (#375).
fn class_string(
    req: &mut Request,
    class: ClassId,
    object: Option<PhpObject>,
    indent: &str,
) -> String {
    let mut out = String::new();
    let Some(decl) = req.class_decl(class).cloned() else {
        return out;
    };
    let sub_indent = format!("{indent}    ");
    let methods = linked_methods(req, class);
    class_header(
        req,
        &mut out,
        class,
        &decl,
        &methods,
        object.is_some(),
        indent,
    );

    // `ZEND_CLASS_CONST_IS_CASE`: an enum's cases go first, under a
    // heading shown only when there are any; the model has no case bit
    // yet, so an enum's constants are its cases (#377).
    let (cases, constants): (Vec<_>, Vec<_>) = linked_constants(req, class)
        .into_iter()
        .partition(|_| decl.kind == ClassKind::Enum);
    if !cases.is_empty() {
        let _ = write!(out, "\n{indent}  - Enum cases [{}] {{\n", cases.len());
        for (case, _) in &cases {
            out.push_str(&enum_case_string(req, case, decl.backing, &sub_indent));
        }
        let _ = writeln!(out, "{indent}  }}");
    }
    let _ = write!(out, "\n{indent}  - Constants [{}] {{\n", constants.len());
    for (constant, _) in &constants {
        out.push_str(&class_const_string(req, constant, None, &sub_indent));
    }
    let _ = writeln!(out, "{indent}  }}");

    let properties: Vec<(PropertyDecl, ClassId)> = linked_properties(req, class)
        .into_iter()
        .filter(|(p, declaring)| p.visibility != Visibility::Private || *declaring == class)
        .collect();
    let visible: Vec<&LinkedMethod> = methods
        .iter()
        .filter(|m| m.decl.visibility != Visibility::Private || m.scope == class)
        .collect();
    for is_instance in [false, true] {
        let block = if is_instance {
            "Properties"
        } else {
            "Static properties"
        };
        let chosen: Vec<&PropertyDecl> = properties
            .iter()
            .filter(|(p, _)| p.is_instance == is_instance)
            .map(|(p, _)| p)
            .collect();
        let _ = write!(out, "\n{indent}  - {block} [{}] {{\n", chosen.len());
        for property in chosen {
            out.push_str(&property_string(req, property, &sub_indent));
        }
        let _ = writeln!(out, "{indent}  }}");

        // `zend_get_properties_no_lazy_init(Z_OBJ_P(obj))` less every
        // name `ce->properties_info` has and every mangled one, which
        // is how php skips a private or protected slot: what is left is
        // the object's own, and the block stands only for an object
        // reflector, after `Properties` and before `Methods`.
        if is_instance && let Some(object) = object {
            let dynamic = dynamic_properties(req, class, object);
            let _ = write!(
                out,
                "\n{indent}  - Dynamic properties [{}] {{\n",
                dynamic.len()
            );
            for name in &dynamic {
                let _ = writeln!(
                    out,
                    "{sub_indent}Property [ <dynamic> public ${} ]",
                    String::from_utf8_lossy(name)
                );
            }
            let _ = writeln!(out, "{indent}  }}");
        }

        let block = if is_instance {
            "Methods"
        } else {
            "Static methods"
        };
        let chosen: Vec<&&LinkedMethod> = visible
            .iter()
            .filter(|m| m.decl.is_instance == is_instance)
            .collect();
        let _ = write!(out, "\n{indent}  - {block} [{}] {{", chosen.len());
        if chosen.is_empty() {
            out.push('\n');
        }
        for method in chosen {
            out.push('\n');
            out.push_str(&method_string(req, method, class, &sub_indent));
        }
        let _ = writeln!(out, "{indent}  }}");
    }
    let _ = writeln!(out, "{indent}}}");
    out
}

/// The names in the object's property table the class did not declare,
/// in the table's order: php's `Dynamic properties` list, a mangled
/// name (a private or protected slot, whose key begins with a zero
/// byte) skipped as `ZSTR_VAL(prop_name)[0]` skips it.
fn dynamic_properties(req: &mut Request, class: ClassId, object: PhpObject) -> Vec<Vec<u8>> {
    let declared: Vec<Vec<u8>> = req
        .class_decl(class)
        .map(|decl| decl.properties.iter().map(|p| p.name.0.to_vec()).collect())
        .unwrap_or_default();
    let Ok(entries) = object.props(req) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .map(|(name, _)| name.as_bytes().to_vec())
        .filter(|name| name.first().is_some_and(|byte| *byte != 0))
        .filter(|name| !declared.iter().any(|shown| shown == name))
        .collect()
}

/// The first line of `_class_string`: the doc comment where there is
/// one, `Object of class` for an object reflector and the kind
/// otherwise, `<internal:ext>` or `<user>`, the modifiers, the name,
/// `extends`, an enum's backing type and the interface list, then a
/// user class's `@@ file start-end` line.
fn class_header(
    req: &mut Request,
    out: &mut String,
    class: ClassId,
    decl: &ClassDecl,
    methods: &[LinkedMethod],
    of_object: bool,
    indent: &str,
) {
    if let Some(doc) = &decl.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    let kind = match (of_object, decl.kind) {
        (true, _) => "Object of class",
        (false, ClassKind::Class) => "Class",
        (false, ClassKind::Interface) => "Interface",
        (false, ClassKind::Trait) => "Trait",
        (false, ClassKind::Enum) => "Enum",
    };
    let _ = write!(out, "{indent}{kind} [ ");
    let internal = decl.file.0.is_empty();
    out.push_str(if internal { "<internal" } else { "<user" });
    if internal && let Some(extension) = req.class_extension_name(class) {
        let _ = write!(out, ":{extension}");
    }
    out.push_str("> ");
    match decl.kind {
        ClassKind::Interface => out.push_str("interface "),
        ClassKind::Trait => out.push_str("trait "),
        ClassKind::Enum => out.push_str("enum "),
        ClassKind::Class => {
            // `ZEND_ACC_IMPLICIT_ABSTRACT_CLASS`: a class with an
            // abstract method it did not implement is abstract too.
            if decl.flags.is_abstract || methods.iter().any(|m| m.decl.is_abstract) {
                out.push_str("abstract ");
            }
            if decl.flags.is_final {
                out.push_str("final ");
            }
            if decl.flags.is_readonly {
                out.push_str("readonly ");
            }
            out.push_str("class ");
        }
    }
    out.push_str(&String::from_utf8_lossy(&decl.name.0));
    if let Some(parent) = &decl.parent {
        let _ = write!(out, " extends {}", String::from_utf8_lossy(&parent.0));
    }
    if decl.kind == ClassKind::Enum {
        match decl.backing {
            EnumBacking::Int => out.push_str(": int"),
            EnumBacking::String => out.push_str(": string"),
            EnumBacking::Pure => {}
        }
    }
    let interfaces = linked_interfaces(req, class);
    for (i, interface) in interfaces.iter().enumerate() {
        let name = class_name_of(req, *interface);
        let name = String::from_utf8_lossy(&name);
        if i == 0 {
            let word = if decl.kind == ClassKind::Interface {
                "extends"
            } else {
                "implements"
            };
            let _ = write!(out, " {word} {name}");
        } else {
            let _ = write!(out, ", {name}");
        }
    }
    out.push_str(" ] {\n");
    if !internal {
        let _ = writeln!(
            out,
            "{indent}  @@ {} {}-{}",
            String::from_utf8_lossy(&decl.file.0),
            decl.line,
            decl.end_line
        );
    }
}

/// `_class_const_string`: `Constant [ final? visibility type NAME ] {
/// value }`, the type the declaration's where it has one and the
/// value's `zend_zval_type_name` otherwise, the value as
/// `zval_get_string` prints it, `Array` for an array and `Object` for
/// an object; a doc comment on the line above.
fn class_const_string(
    req: &mut Request,
    constant: &ClassConstDecl,
    enum_of: Option<&[u8]>,
    indent: &str,
) -> String {
    let mut out = String::new();
    if let Some(doc) = &constant.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    // A case of an enum holds the enum's own instance, so
    // `zend_zval_type_name` names the enum and the `IS_OBJECT` arm
    // writes `Object` in place of the value.
    let value = eval_const_expr(req, &constant.value, None).unwrap_or_default();
    let ty = match (&constant.ty, enum_of) {
        (Some(ty), _) => type_to_string(ty),
        (None, Some(name)) => String::from_utf8_lossy(name).into_owned(),
        (None, None) => String::from(value.type_name()),
    };
    let shown = match enum_of {
        Some(_) => String::from("Object"),
        None => value_string(req, &value),
    };
    let _ = writeln!(
        out,
        "{indent}Constant [ {}{} {ty} {} ] {{ {shown} }}",
        if constant.is_final { "final " } else { "" },
        visibility_word(constant.visibility),
        String::from_utf8_lossy(&constant.name.0)
    );
    out
}

/// `_enum_case_string`: `Case NAME` for a pure enum, `Case NAME = value`
/// for a backed one, the value being the case's backing value as
/// `zval_get_string` prints it; a doc comment on the line above.
fn enum_case_string(
    req: &mut Request,
    case: &ClassConstDecl,
    backing: EnumBacking,
    indent: &str,
) -> String {
    let mut out = String::new();
    if let Some(doc) = &case.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    let _ = write!(
        out,
        "{indent}Case {}",
        String::from_utf8_lossy(&case.name.0)
    );
    if backing != EnumBacking::Pure {
        let value = eval_const_expr(req, &case.value, None).unwrap_or_default();
        let _ = write!(out, " = {}", value_string(req, &value));
    }
    out.push('\n');
    out
}

/// `zend_visibility_string`.
fn visibility_word(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Private => "private",
    }
}

/// `_property_string` for a declared property: `Property [ visibility
/// type $name = default ]` with the modifier words between where the
/// declaration has them, the default being the
/// slot of `default_properties_table` as `format_default_value` prints
/// it: an untyped property nothing initialised holds `NULL`, a typed
/// one is `UNDEF` and prints no default. Hooks, `abstract`, `final`
/// and the asymmetric `(set)` visibilities have no bit on the
/// declaration model yet and are not rendered.
fn property_string(req: &mut Request, property: &PropertyDecl, indent: &str) -> String {
    let mut out = String::new();
    if let Some(doc) = &property.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    out.push_str(indent);
    out.push_str("Property [ ");
    // `_property_string`'s order: `abstract`, `final`, the visibility,
    // the set visibility, `static`, `readonly`, `virtual`, the type.
    if property.is_final {
        out.push_str("final ");
    }
    out.push_str(visibility_word(property.visibility));
    out.push(' ');
    // `zend_declare_typed_property`: a readonly property with no set
    // visibility of its own is `ZEND_ACC_PROTECTED_SET`, which the
    // `(set)` words print; the implicit one waits on the readonly
    // modification error that php raises before it.
    match property.set_visibility {
        Some(Visibility::Private) => out.push_str("private(set) "),
        Some(_) => out.push_str("protected(set) "),
        None if property.is_readonly && property.visibility == Visibility::Public => {
            out.push_str("protected(set) ");
        }
        None => {}
    }
    if !property.is_instance {
        out.push_str("static ");
    }
    if property.is_readonly {
        out.push_str("readonly ");
    }
    if let Some(ty) = &property.ty {
        let _ = write!(out, "{} ", type_to_string(ty));
    }
    let _ = write!(out, "${}", String::from_utf8_lossy(&property.name.0));
    match (&property.default, &property.ty) {
        (Some(default), _) => {
            let _ = write!(out, " = {}", default_value_string(req, default));
        }
        (None, None) => out.push_str(" = NULL"),
        (None, Some(_)) => {}
    }
    out.push_str(" ]\n");
    out
}

/// `format_default_value` over a declared default: `smart_str_append_scalar`
/// for a scalar, `NULL`, `true`, `false`, the number, or the string in
/// single quotes with `smart_str_append_escaped`'s escapes; an array as
/// `[a, b]` or `['k' => v]`; a constant's name as `zend_ast_export`
/// writes an unevaluated expression.
fn default_value_string(req: &mut Request, expr: &ConstExpr) -> String {
    match expr {
        ConstExpr::Null => String::from("NULL"),
        ConstExpr::Bool(b) => String::from(if *b { "true" } else { "false" }),
        ConstExpr::Long(n) => n.to_string(),
        ConstExpr::Double(f) => php_ext::coerce::to_string(req, &Value::float(*f)).map_or_else(
            |_| String::new(),
            |s| String::from_utf8_lossy(s.as_bytes()).into_owned(),
        ),
        ConstExpr::Str(s) => format!("'{}'", escaped(s)),
        ConstExpr::Array(elements) => {
            let is_list = elements.iter().all(|(key, _)| key.is_none());
            let mut out = String::from("[");
            for (i, (key, value)) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if !is_list {
                    match key {
                        Some(ConstExpr::Str(s)) => {
                            let _ = write!(out, "'{}'", escaped(s));
                        }
                        Some(key) => out.push_str(&default_value_string(req, key)),
                        None => out.push_str(&i.to_string()),
                    }
                    out.push_str(" => ");
                }
                out.push_str(&default_value_string(req, value));
            }
            out.push(']');
            out
        }
        ConstExpr::Constant(name) => String::from_utf8_lossy(&name.0).into_owned(),
        ConstExpr::ClassConstant { class, name } => format!(
            "{}::{}",
            String::from_utf8_lossy(&class.0),
            String::from_utf8_lossy(&name.0)
        ),
        _ => String::from("<default>"),
    }
}

/// `smart_str_append_escaped`: a backslash, a byte below 32 and one
/// above 126 escaped, the C escapes for the seven that have one and
/// `\xHH` for the rest.
fn escaped(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0C => out.push_str("\\f"),
            0x0B => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            0x1B => out.push_str("\\e"),
            32..=126 => out.push(char::from(byte)),
            _ => {
                let _ = write!(out, "\\x{byte:02X}");
            }
        }
    }
    out
}

/// One row of a linked class's function table, `zend_function` as
/// `zend_do_inheritance` left it: the declaration, `common.scope`, the
/// class that declared it, and `common.prototype`'s scope, the
/// interface or the abstract parent method it implements, where
/// `do_inheritance_check_on_method` set one.
#[derive(Clone)]
struct LinkedMethod {
    decl: MethodDecl,
    scope: ClassId,
    prototype: Option<ClassId>,
}

/// `ZEND_ACC_CTOR`: the method named `__construct`.
fn is_constructor(method: &MethodDecl) -> bool {
    method.function.name.0.eq_ignore_ascii_case(b"__construct")
}

/// `ce->function_table` after linking: the class's own methods in
/// declaration order, then each of the parent's appended where the
/// class has none of that name (`do_inherit_method`), the private ones
/// too, then each new interface's abstract methods the same way
/// (`do_interface_implementation`). A method the class declares over a
/// parent's or an interface's takes the prototype
/// `do_inheritance_check_on_method` gives it: the parent method's
/// prototype, or the parent method itself, unless the parent's is
/// private and not abstract, or a constructor with no abstract
/// prototype, which leaves none.
fn linked_methods(req: &mut Request, class: ClassId) -> Vec<LinkedMethod> {
    linked_methods_at(req, class, 0)
}

fn linked_methods_at(req: &mut Request, class: ClassId, depth: u32) -> Vec<LinkedMethod> {
    let Some(decl) = req.class_decl(class).cloned() else {
        return Vec::new();
    };
    let mut table: Vec<LinkedMethod> = decl
        .methods
        .iter()
        .map(|method| LinkedMethod {
            decl: method.clone(),
            scope: class,
            prototype: None,
        })
        .collect();
    if depth > 64 {
        return table;
    }
    let parent = decl
        .parent
        .as_ref()
        .and_then(|name| lookup_class(req, &name.0).ok().flatten());
    if let Some(parent) = parent {
        for inherited in linked_methods_at(req, parent, depth + 1) {
            inherit_method(&mut table, inherited, false);
        }
    }
    let parent_interfaces = parent.map_or_else(Vec::new, |p| linked_interfaces(req, p));
    let own_interfaces: Vec<ClassId> = linked_interfaces(req, class)
        .into_iter()
        .filter(|i| !parent_interfaces.contains(i))
        .collect();
    for interface in own_interfaces {
        for inherited in linked_methods_at(req, interface, depth + 1) {
            inherit_method(&mut table, inherited, true);
        }
    }
    table
}

/// `do_inherit_method`: the child's entry of the same name takes the
/// prototype the check gives it, or the parent's row is appended.
fn inherit_method(table: &mut Vec<LinkedMethod>, parent: LinkedMethod, is_interface: bool) {
    let name = parent.decl.function.name.0.to_ascii_lowercase();
    let child = table
        .iter_mut()
        .find(|m| m.decl.function.name.0.to_ascii_lowercase() == name);
    let Some(child) = child else {
        table.push(parent);
        return;
    };
    if is_interface && child.scope == parent.scope {
        // The same method in an interface may be inherited a few times.
        return;
    }
    let flags_private_alone = parent.decl.visibility == Visibility::Private
        && !parent.decl.is_abstract
        && !is_constructor(&parent.decl);
    if flags_private_alone {
        return;
    }
    let proto_is_abstract = parent.prototype.is_some() || parent.decl.is_abstract;
    if is_constructor(&parent.decl) && !proto_is_abstract {
        return;
    }
    child.prototype = Some(parent.prototype.unwrap_or(parent.scope));
}

/// `ce->interfaces` after linking, in php's order. An internal class
/// that declares `__toString` has `Stringable` first, added by
/// `zend_register_functions` before `zend_class_implements` runs; a
/// user class has it last among its own, where the compiler appends
/// it. A class with no interfaces of its own takes its parent's in
/// reverse (`zend_do_inherit_interfaces` walks `if_num--`), which is
/// why `JsonException` implements `Throwable, Stringable` where
/// `Exception` implements `Stringable, Throwable`. An internal class
/// with interfaces of its own appends each after the parent's, followed
/// by that interface's own parents in reverse
/// (`zend_do_implement_interface`); a user class appends its own first
/// and their parents after all of them
/// (`zend_do_implement_interfaces`).
fn linked_interfaces(req: &mut Request, class: ClassId) -> Vec<ClassId> {
    linked_interfaces_at(req, class, 0)
}

fn linked_interfaces_at(req: &mut Request, class: ClassId, depth: u32) -> Vec<ClassId> {
    let Some(decl) = req.class_decl(class).cloned() else {
        return Vec::new();
    };
    let mut list: Vec<ClassId> = Vec::new();
    if depth > 64 {
        return list;
    }
    let internal = decl.file.0.is_empty();
    let stringable = lookup_class(req, b"Stringable").ok().flatten();
    let declares_to_string = decl.kind != ClassKind::Trait
        && stringable != Some(class)
        && decl
            .methods
            .iter()
            .any(|m| m.function.name.0.eq_ignore_ascii_case(b"__toString"));
    let mut own: Vec<ClassId> = decl
        .interfaces
        .iter()
        .filter_map(|name| lookup_class(req, &name.0).ok().flatten())
        .collect();
    if declares_to_string && let Some(stringable) = stringable {
        if internal {
            list.push(stringable);
        } else if !own.contains(&stringable) {
            own.push(stringable);
        }
    }
    let parent = decl
        .parent
        .as_ref()
        .and_then(|name| lookup_class(req, &name.0).ok().flatten());
    let inherited = parent.map_or_else(Vec::new, |p| linked_interfaces_at(req, p, depth + 1));
    let push = |list: &mut Vec<ClassId>, id: ClassId| {
        if !list.contains(&id) {
            list.push(id);
        }
    };
    if internal {
        for id in inherited.into_iter().rev() {
            push(&mut list, id);
        }
        for interface in own {
            push(&mut list, interface);
            for id in linked_interfaces_at(req, interface, depth + 1)
                .into_iter()
                .rev()
            {
                push(&mut list, id);
            }
        }
    } else if own.is_empty() {
        for id in inherited.into_iter().rev() {
            push(&mut list, id);
        }
    } else {
        let mut added: Vec<ClassId> = Vec::new();
        for id in inherited {
            push(&mut list, id);
        }
        for interface in own {
            if !list.contains(&interface) {
                list.push(interface);
                added.push(interface);
            }
        }
        for interface in added {
            for id in linked_interfaces_at(req, interface, depth + 1)
                .into_iter()
                .rev()
            {
                push(&mut list, id);
            }
        }
    }
    list
}

/// `ce->constants_table` after linking: the class's own constants, then
/// the parent's that are not private and not overridden
/// (`do_inherit_class_constant`), then each new interface's
/// (`do_inherit_iface_constant`), each with the class that declared it.
fn linked_constants(req: &mut Request, class: ClassId) -> Vec<(ClassConstDecl, ClassId)> {
    linked_constants_at(req, class, 0)
}

fn linked_constants_at(
    req: &mut Request,
    class: ClassId,
    depth: u32,
) -> Vec<(ClassConstDecl, ClassId)> {
    let Some(decl) = req.class_decl(class).cloned() else {
        return Vec::new();
    };
    let own: Vec<(ClassConstDecl, ClassId)> =
        decl.constants.iter().map(|c| (c.clone(), class)).collect();
    // An internal class inherits before `zend_declare_class_constant_ex`
    // adds its own, so the parent's come first; a user class compiles
    // its own into the table first.
    let internal = decl.file.0.is_empty();
    let mut table: Vec<(ClassConstDecl, ClassId)> = if internal { Vec::new() } else { own.clone() };
    if depth > 64 {
        return own;
    }
    let parent = decl
        .parent
        .as_ref()
        .and_then(|name| lookup_class(req, &name.0).ok().flatten());
    let mut sources: Vec<ClassId> = parent.into_iter().collect();
    let parent_interfaces = parent.map_or_else(Vec::new, |p| linked_interfaces(req, p));
    sources.extend(
        linked_interfaces(req, class)
            .into_iter()
            .filter(|i| !parent_interfaces.contains(i)),
    );
    for source in sources {
        for (constant, declaring) in linked_constants_at(req, source, depth + 1) {
            let present = table.iter().any(|(c, _)| c.name == constant.name)
                || (internal && own.iter().any(|(c, _)| c.name == constant.name));
            if !present && constant.visibility != Visibility::Private {
                table.push((constant, declaring));
            }
        }
    }
    if internal {
        table.extend(own);
    }
    table
}

/// `ce->properties_info` after linking: the class's own properties
/// and each of the parent's the class does not redeclare
/// (`do_inherit_property`), a private one among them as the shadow
/// `_class_string` skips, each with the class that declared it. A user
/// class has its own first, compiled into the table before
/// `zend_do_inheritance` appends the parent's; an internal class has
/// the parent's first, `zend_register_internal_class_ex` inheriting
/// before `zend_declare_typed_property` adds its own, which is why
/// `ReflectionMethod` lists `$name` before `$class`.
fn linked_properties(req: &mut Request, class: ClassId) -> Vec<(PropertyDecl, ClassId)> {
    let chain = class_chain(req, class);
    let internal = req
        .class_decl(class)
        .is_none_or(|decl| decl.file.0.is_empty());
    let mut table: Vec<(PropertyDecl, ClassId)> = Vec::new();
    for at in chain.iter().rev() {
        let Some(decl) = req.class_decl(*at) else {
            continue;
        };
        for property in &decl.properties {
            let at_index = table.iter().position(|(p, _)| p.name == property.name);
            match at_index {
                // The child's declaration stands where the parent's was
                // inherited; a user class puts its own before the rest.
                Some(i) if internal => table[i] = (property.clone(), *at),
                Some(i) => {
                    table.remove(i);
                    table.insert(0, (property.clone(), *at));
                }
                None if internal || *at != class => table.push((property.clone(), *at)),
                None => table.insert(
                    table.iter().filter(|(_, d)| *d == class).count(),
                    (property.clone(), *at),
                ),
            }
        }
    }
    table
}

/// `_function_string` with no scope: `Function [ <internal:ext>
/// function name ] {`, its parameters and its return row, each line
/// under `indent`. A function with the `Deprecated` attribute carries
/// `, deprecated` after `<internal`, as `ZEND_ACC_DEPRECATED` does.
fn function_string(req: &mut Request, function: Function, indent: &str) -> String {
    let Some(decl) = req.function_decl(function).cloned() else {
        return String::new();
    };
    let module = req
        .function_extension(function)
        .and_then(|e| req.extension_name(e))
        .map(str::to_owned);
    let mut out = String::new();
    function_header(&mut out, &decl, module.as_deref(), None, indent);
    function_body(req, &mut out, &decl, false, indent);
    out
}

/// `_function_string` with a scope, a method of `ce`: `Method [
/// <internal:ext, inherits S, overwrites O, prototype P, ctor> ... method
/// name ] {` with the modifier and visibility words between where the
/// declaration has them, then the
/// parameters and the return row, `Tentative return` where the
/// declaration carries the bit. `inherits` names the method's scope
/// when it is not `ce`; `overwrites` names the scope of the parent's
/// method of the same name when the method is `ce`'s own and the
/// parent's is not private; `prototype` names the prototype's scope.
fn method_string(req: &mut Request, method: &LinkedMethod, ce: ClassId, indent: &str) -> String {
    let module = req.class_extension_name(method.scope).map(str::to_owned);
    let mut words: Vec<String> = Vec::new();
    if method.scope != ce {
        let scope = class_name_of(req, method.scope);
        words.push(format!("inherits {}", String::from_utf8_lossy(&scope)));
    } else if let Some(parent) = class_chain(req, ce).get(1).copied() {
        let lowered = method.decl.function.name.0.to_ascii_lowercase();
        let overwrites = linked_methods(req, parent)
            .into_iter()
            .find(|m| m.decl.function.name.0.to_ascii_lowercase() == lowered)
            .filter(|m| m.scope != method.scope && m.decl.visibility != Visibility::Private);
        if let Some(overwritten) = overwrites {
            let scope = class_name_of(req, overwritten.scope);
            words.push(format!("overwrites {}", String::from_utf8_lossy(&scope)));
        }
    }
    if let Some(prototype) = method.prototype {
        let scope = class_name_of(req, prototype);
        words.push(format!("prototype {}", String::from_utf8_lossy(&scope)));
    }
    if is_constructor(&method.decl) {
        words.push(String::from("ctor"));
    }
    let mut out = String::new();
    function_header(
        &mut out,
        &method.decl.function,
        module.as_deref(),
        Some((&method.decl, &words)),
        indent,
    );
    function_body(
        req,
        &mut out,
        &method.decl.function,
        method.decl.tentative_return,
        indent,
    );
    out
}

/// The first line of `_function_string`: the doc comment where there
/// is one, `Function [` or `Method [`, `<internal:module` or `<user`
/// with `, deprecated` where the attribute is and a method's
/// inheritance words, then
/// the modifiers and the name.
fn function_header(
    out: &mut String,
    decl: &FunctionDecl,
    module: Option<&str>,
    method: Option<(&MethodDecl, &[String])>,
    indent: &str,
) {
    if let Some(doc) = &decl.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    out.push_str(indent);
    // `fptr->common.fn_flags & ZEND_ACC_CLOSURE ? "Closure [ " :
    // (fptr->common.scope ? "Method [ " : "Function [ ")`
    // (`Z/ext/reflection/php_reflection.c:905`). The flag is the
    // declaration's name here: `zend_begin_func_decl` names every
    // closure `{closure:…}` and no other name php compiles can be
    // written that way.
    out.push_str(if method.is_some() {
        "Method [ "
    } else if decl.name.0.starts_with(b"{closure:") {
        "Closure [ "
    } else {
        "Function [ "
    });
    let internal = decl.file.0.is_empty();
    out.push_str(if internal { "<internal" } else { "<user" });
    if decl
        .attributes
        .iter()
        .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated"))
    {
        out.push_str(", deprecated");
    }
    if internal && let Some(module) = module {
        let _ = write!(out, ":{module}");
    }
    if let Some((_, words)) = method {
        for word in words {
            let _ = write!(out, ", {word}");
        }
    }
    out.push_str("> ");
    if let Some((method, _)) = method {
        if method.is_abstract {
            out.push_str("abstract ");
        }
        if method.is_final {
            out.push_str("final ");
        }
        if !method.is_instance {
            out.push_str("static ");
        }
        let _ = write!(out, "{} method ", visibility_word(method.visibility));
    } else {
        out.push_str("function ");
    }
    if decl.returns_by_ref {
        out.push('&');
    }
    let _ = writeln!(out, "{} ] {{", String::from_utf8_lossy(&decl.name.0));
}

/// The rest of `_function_string`: a user function's `@@ file start -
/// end` line, the parameters, the return row and the closing brace.
/// php writes the `@@` row for a `ZEND_USER_FUNCTION` alone, off
/// `op_array.filename`, `line_start` and `line_end`, which are the file
/// and the lines of the `function` keyword and of the closing brace;
/// a native's file is empty and it carries no row, as
/// `_class_string` does for a class.
///
/// ```php
/// function f($a) {
///   return $a;
/// }
/// echo new ReflectionFunction('f');
/// ```
/// ```expect
/// Function [ <user> function f ] {
///   @@ %s 2 - 4
///
///   - Parameters [1] {
///     Parameter #0 [ <required> $a ]
///   }
/// }
/// ```
fn function_body(
    req: &mut Request,
    out: &mut String,
    decl: &FunctionDecl,
    tentative: bool,
    indent: &str,
) {
    if !decl.file.0.is_empty() {
        let _ = writeln!(
            out,
            "{indent}  @@ {} {} - {}",
            String::from_utf8_lossy(&decl.file.0),
            decl.line,
            decl.end_line
        );
    }
    let param_indent = format!("{indent}  ");
    function_parameter_string(req, out, decl, &param_indent);
    if let Some(returns) = &decl.return_type {
        let word = if tentative {
            "Tentative return"
        } else {
            "Return"
        };
        let _ = writeln!(out, "  {indent}- {word} [ {} ]", type_to_string(returns));
    }
    let _ = writeln!(out, "{indent}}}");
}

/// `required_num_args`: every parameter before the first with a
/// default or the variadic one.
fn required_parameters(decl: &FunctionDecl) -> usize {
    decl.parameters
        .iter()
        .position(|p| p.default.is_some() || p.variadic)
        .unwrap_or(decl.parameters.len())
}

/// `_parameter_string`: one `Parameter #N [ ... ]` row, the modifier
/// word, the type where the declaration has one, `&` for a by-reference
/// parameter, `...` for the variadic one, the name, and the default
/// after `=` where the row is optional and not variadic. php passes an
/// indent it does not write, so the row carries none.
fn parameter_string(
    req: &mut Request,
    parameter: &Parameter,
    offset: usize,
    required: bool,
    internal: bool,
) -> String {
    let mut out = format!("Parameter #{offset} [ ");
    out.push_str(if required {
        "<required> "
    } else {
        "<optional> "
    });
    if let Some(ty) = &parameter.ty {
        let _ = write!(out, "{} ", type_to_string(ty));
    }
    if parameter.by_ref {
        out.push('&');
    }
    if parameter.variadic {
        out.push_str("...");
    }
    let _ = write!(out, "${}", String::from_utf8_lossy(&parameter.name.0));
    // A native's default is the text `gen_stub.php` wrote into the arg
    // info; a user function's is the zval `RECV_INIT` holds, which
    // `format_default_value` prints and `NULL` is the spelling of.
    match (&parameter.default, internal) {
        (Some(expr), false) => {
            let _ = write!(out, " = {}", default_value_string(req, expr));
        }
        (default, _) if !required && !parameter.variadic => {
            out.push_str(" = ");
            out.push_str(
                &default
                    .as_ref()
                    .map_or_else(|| String::from("<default>"), const_expr_string),
            );
        }
        _ => {}
    }
    out.push_str(" ]");
    out
}

/// `_function_parameter_string`: `- Parameters [N] {` and one
/// [`parameter_string`] line per parameter.
fn function_parameter_string(
    req: &mut Request,
    out: &mut String,
    decl: &FunctionDecl,
    indent: &str,
) {
    let required = required_parameters(decl);
    let internal = decl.file.0.is_empty();
    out.push('\n');
    let _ = writeln!(out, "{indent}- Parameters [{}] {{", decl.parameters.len());
    for (offset, parameter) in decl.parameters.clone().iter().enumerate() {
        let row = parameter_string(req, parameter, offset, offset < required, internal);
        let _ = writeln!(out, "{indent}  {row}");
    }
    let _ = writeln!(out, "{indent}}}");
}

/// A native parameter's default as `gen_stub.php` writes it into
/// `zend_internal_arg_info.default_value`: the expression's source
/// text, which for a scalar is the scalar as written, and for a string
/// the stub's spelling, double-quoted as `zend_exceptions.stub.php` and
/// the rest of php-src write theirs, and `<default>` for the stub's
/// `UNKNOWN`, an optional parameter with nothing to show; a string the
/// stub spells with an escape needs the written text on the
/// declaration, which the model does not carry (#377).
fn const_expr_string(expr: &php_ext::decl::ConstExpr) -> String {
    use php_ext::decl::ConstExpr;
    match expr {
        ConstExpr::Null => String::from("null"),
        ConstExpr::Bool(b) => String::from(if *b { "true" } else { "false" }),
        ConstExpr::Long(n) => n.to_string(),
        ConstExpr::Double(f) => f.to_string(),
        ConstExpr::Str(s) => format!("\"{}\"", String::from_utf8_lossy(s)),
        expr if stub::is_unknown_default(expr) => String::from("<default>"),
        ConstExpr::Constant(name) => String::from_utf8_lossy(&name.0).into_owned(),
        ConstExpr::Array(elements) if elements.is_empty() => String::from("[]"),
        _ => String::from("<default>"),
    }
}

/// `zend_type_to_string`: class names first, then the scalar and
/// special types in `zend_type_to_string_resolved`'s order, `|` between; `mixed` alone
/// for the any-mask; a single type with `null` as `?T`, a union with it
/// as `|null` last; an intersection with `&`, bracketed inside a union.
fn type_to_string(ty: &Type) -> String {
    match ty {
        Type::Mixed => return String::from("mixed"),
        Type::Intersection(members) => {
            return members
                .iter()
                .map(type_to_string)
                .collect::<Vec<_>>()
                .join("&");
        }
        _ => {}
    }
    let members: Vec<&Type> = match ty {
        Type::Union(members) => members.iter().collect(),
        Type::Nullable(inner) => vec![inner.as_ref(), &Type::Null],
        other => vec![other],
    };
    let mut parts: Vec<String> = Vec::new();
    for member in &members {
        match member {
            Type::Named(name) => parts.push(String::from_utf8_lossy(&name.0).into_owned()),
            Type::SelfClass => parts.push(String::from("self")),
            Type::ParentClass => parts.push(String::from("parent")),
            Type::Intersection(inner) => parts.push(format!(
                "({})",
                inner
                    .iter()
                    .map(type_to_string)
                    .collect::<Vec<_>>()
                    .join("&")
            )),
            _ => {}
        }
    }
    let has = |wanted: &Type| members.contains(&wanted);
    if has(&Type::CalledClass) {
        parts.push(String::from("static"));
    }
    for (member, word) in [
        (Type::Callable, "callable"),
        (Type::Object, "object"),
        (Type::Array, "array"),
        (Type::String, "string"),
        (Type::Int, "int"),
        (Type::Float, "float"),
    ] {
        if has(&member) {
            parts.push(String::from(word));
        }
    }
    if has(&Type::Iterable) {
        parts.push(String::from("iterable"));
    }
    if has(&Type::Bool) || (has(&Type::False) && has(&Type::True)) {
        parts.push(String::from("bool"));
    } else if has(&Type::False) {
        parts.push(String::from("false"));
    } else if has(&Type::True) {
        parts.push(String::from("true"));
    }
    for (member, word) in [(Type::Void, "void"), (Type::Never, "never")] {
        if has(&member) {
            parts.push(String::from(word));
        }
    }
    if has(&Type::Null) {
        let joined = parts.join("|");
        if !parts.is_empty() && !joined.contains('|') && !joined.contains('&') {
            return format!("?{joined}");
        }
        parts.push(String::from("null"));
    }
    parts.join("|")
}
