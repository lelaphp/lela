//! The natives the engine declares itself (design §13): the Zend
//! builtins that need frames or the engine's own tables, which php-src
//! keeps in `ext/standard` and this engine keeps here, so an engine with
//! no extensions serves a program that uses Zend alone (design §4.10).
//!
//! Twelve of them are here: the two the first witness of a loop needs,
//! the three that read what `php_heap::mm` accounts, so the allocator's
//! numbers are php's numbers under a witness and not under a terminal
//! session a person typed once, `ini_get` and `ini_set`, which read and
//! move the engine's own ini table so a witness can reach what an entry
//! decides and what it kept, `set_time_limit`, which moves the one
//! entry the execution timer is armed from, `strlen` and `defined`,
//! the two of `zend_builtin_functions.c` whose calls php's compiler
//! serves itself (#219): the compiler emits `ZEND_STRLEN` and
//! `ZEND_DEFINED` over their kernels where it reads the name, and reads
//! it only where the bundle declares the native; and `class_exists` and
//! `get_class`, which read the linked table (#259). Each is declared and
//! paired with its entry exactly as an extension's are, so there is one
//! shape for a native and not two.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{ClassKind, ConstExpr, FunctionDecl, Name, Parameter, Type, Visibility};

use crate::api::abi::Entry;
use crate::api::coerce::{coerce_param, to_string};
use crate::api::raising::{
    Thrown, arg_count_error, arg_null_deprecated, arg_type_error, deprecated, throw,
    too_few_arguments,
};
use crate::api::request::Request;
use crate::api::values::{Key, PhpString, Value, ValueRef};

/// The engine's own natives, in the order php declares them, each
/// declaration beside its entry.
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let mut natives = zend_natives();
    natives.extend(class_natives());
    natives
}

/// The two that read the linked table (#259).
fn class_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"class_exists",
                &[
                    parameter(b"class", Some(Type::String), false),
                    Parameter {
                        default: Some(ConstExpr::Bool(true)),
                        ..parameter(b"autoload", Some(Type::Bool), false)
                    },
                ],
                Some(Type::Bool),
            ),
            class_exists as Entry,
        ),
        (
            declared(
                b"get_class",
                &[parameter(b"object", Some(Type::Object), false)],
                Some(Type::String),
            ),
            get_class as Entry,
        ),
    ]
}

/// The ten of `zend_builtin_functions.c` and `ext/standard` that need
/// the engine's frames, allocator and ini table.
fn zend_natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"var_dump",
                &[parameter(b"value", None, true)],
                Some(Type::Void),
            ),
            var_dump as Entry,
        ),
        (
            declared(
                b"memory_get_usage",
                &[real_usage_parameter()],
                Some(Type::Int),
            ),
            memory_get_usage as Entry,
        ),
        (
            declared(
                b"memory_get_peak_usage",
                &[real_usage_parameter()],
                Some(Type::Int),
            ),
            memory_get_peak_usage as Entry,
        ),
        (
            declared(b"memory_reset_peak_usage", &[], Some(Type::Void)),
            memory_reset_peak_usage as Entry,
        ),
        (
            declared(
                b"extension_loaded",
                &[parameter(b"extension", Some(Type::String), false)],
                Some(Type::Bool),
            ),
            extension_loaded as Entry,
        ),
        (
            declared(
                b"ini_get",
                &[parameter(b"option", Some(Type::String), false)],
                Some(Type::Union(vec![Type::String, Type::False])),
            ),
            ini_get as Entry,
        ),
        (
            declared(
                b"ini_set",
                &[
                    parameter(b"option", Some(Type::String), false),
                    parameter(
                        b"value",
                        Some(Type::Union(vec![
                            Type::String,
                            Type::Int,
                            Type::Float,
                            Type::Bool,
                            Type::Null,
                        ])),
                        false,
                    ),
                ],
                Some(Type::Union(vec![Type::String, Type::False])),
            ),
            ini_set as Entry,
        ),
        (
            declared(
                b"set_time_limit",
                &[parameter(b"seconds", Some(Type::Int), false)],
                Some(Type::Bool),
            ),
            set_time_limit as Entry,
        ),
        (
            declared(
                b"strlen",
                &[parameter(b"string", Some(Type::String), false)],
                Some(Type::Int),
            ),
            strlen as Entry,
        ),
        (
            declared(
                b"defined",
                &[parameter(b"constant_name", Some(Type::String), false)],
                Some(Type::Bool),
            ),
            defined as Entry,
        ),
    ]
}

/// `class_exists(string $class, bool $autoload = true): bool`, php's
/// `class_exists_impl` (`Z/Zend/zend_builtin_functions.c:1136`) with
/// `ZEND_ACC_LINKED` to have and `ZEND_ACC_INTERFACE | ZEND_ACC_TRAIT`
/// to lack: the class under the lowered name, a leading `\` dropped,
/// which is a class or an enum and not an interface or a trait. The
/// autoloaders are served for a miss under `$autoload`, and none is
/// registered yet.
///
/// ```php
/// class M {}
/// var_dump(class_exists('M'), class_exists('m'), class_exists('\M'), class_exists('Exception'), class_exists('Throwable'), class_exists('Nope'), class_exists('Nope', false));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn class_exists(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if !(1..=2).contains(&count) {
        return Err(wrong_argument_count(req, 1, 2, count));
    }
    let name = string_argument(req, 1)?;
    let autoload = if count == 2 {
        let given = req.argument(2).unwrap_or_default();
        coerce_param(req, &given, &Type::Bool, 2)?
            .as_bool()
            .unwrap_or(true)
    } else {
        true
    };
    let bytes = name.as_bytes();
    let bytes = bytes.strip_prefix(b"\\").unwrap_or(bytes);
    let found = match req.class_named(bytes) {
        Some(class) => Some(class),
        None if autoload => crate::api::call::autoload(req, bytes)?,
        None => None,
    };
    let exists = found
        .and_then(|class| req.class_decl(class))
        .is_some_and(|decl| !matches!(decl.kind, ClassKind::Interface | ClassKind::Trait));
    Ok(Value::bool(exists))
}

/// `get_class(object $object = UNKNOWN): string`, php's
/// `ZEND_FUNCTION(get_class)` (`Z/Zend/zend_builtin_functions.c:721`):
/// the declared name of the object's class, `ce->name`; without an
/// argument the executed scope's name, deprecated since 8.3, and php's
/// `Error` where there is no scope, which is everywhere a native can be
/// called from here.
///
/// ```php
/// class M {} $m = new M;
/// var_dump(get_class($m), get_class(new Exception));
/// get_class(1);
/// ```
/// ```expect exit=255
/// string(1) "M"
/// string(9) "Exception"
///
/// Fatal error: Uncaught TypeError: get_class(): Argument #1 ($object) must be of type object, int given in %s:4
/// Stack trace:
/// #0 %s(4): get_class(1)
/// #1 {main}
///   thrown in %s on line 4
/// ```
///
/// ```php
/// get_class();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: get_class() without arguments must be called from within a class in %s:2
/// Stack trace:
/// #0 %s(2): get_class()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn get_class(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let class = if count == 0 {
        match req.scope() {
            Some(scope) => {
                deprecated(req, "Calling get_class() without arguments is deprecated");
                scope
            }
            None => {
                return Err(throw(
                    req,
                    "Error",
                    "get_class() without arguments must be called from within a class",
                ));
            }
        }
    } else {
        let given = req.argument(1).unwrap_or_default();
        let object = coerce_param(req, &given, &Type::Object, 1)?;
        match object.as_object() {
            Some(object) => object.class(),
            None => return Ok(Value::null()),
        }
    };
    let name = req.class_name(class);
    Ok(Value::string(PhpString::new(req, name.as_bytes())?))
}

/// `strlen(string $string): int`, php's `ZEND_FUNCTION(strlen)`
/// (`Z/Zend/zend_builtin_functions.c:374`): one `Z_PARAM_STR` and its
/// byte length. A call of one argument never reaches here, since the
/// compiler emits `ZEND_STRLEN` over [`crate::ops::strlen`] for it
/// (`zend_compile_func_strlen`); what does is a call of any other
/// count, which `ZEND_PARSE_PARAMETERS_START(1, 1)` refuses, and the
/// native's frame is on the trace where the opcode's is not.
///
/// ```php
/// var_dump(strlen("a", "b"));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: strlen() expects exactly 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): strlen('a', 'b')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn strlen(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let string = string_argument(req, 1)?;
    Ok(Value::int(as_long(string.len())))
}

/// `defined(string $constant_name): bool`, php's `ZEND_FUNCTION(defined)`
/// (`Z/Zend/zend_builtin_functions.c:608`), which is
/// `zend_get_constant_ex` under `ZEND_FETCH_CLASS_SILENT`: a leading
/// `\` is dropped; a `::` names a class constant, which waits with the
/// classes (`zend_get_class_constant_ex`) and is `false` here; a `\`
/// left in the name is a namespaced constant, looked up with the
/// namespace lowered and the name as written, in the table alone; and
/// anything else is [`crate::ops::defined`], the table and then `true`,
/// `false` and `null` in any case. The compiler emits `ZEND_DEFINED`
/// where the name is written as a string with neither separator in it
/// (`zend_compile_func_defined`), so what reaches here is a name it
/// could not read, with a separator, or not a string, which `Z_PARAM_STR`
/// coerces as it coerces every string parameter.
///
/// ```php
/// var_dump(defined(null), defined("\\"), defined("Foo\\PHP_VERSION"), defined("\\PHP_VERSION"), defined("A::B"), defined("foo\\PHP_VERSION"), defined("\\Foo\\BAR"), defined("\\TRUE"), defined("x:y"), defined(":E_ALL"), defined("::E_ALL"));
/// ```
/// ```expect
/// Deprecated: defined(): Passing null to parameter #1 ($constant_name) of type string is deprecated in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// var_dump(defined(token_get_all("<?php 1;")));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: defined(): Argument #1 ($constant_name) must be of type string, array given in %s:2
/// Stack trace:
/// #0 %s(2): defined(Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn defined(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let name = string_argument(req, 1)?;
    let mut bytes = name.as_bytes();
    if let Some(rest) = bytes.strip_prefix(b"\\") {
        bytes = rest;
    }
    if let Some(colon) = bytes.iter().rposition(|b| *b == b':')
        && colon > 0
        && bytes.get(colon - 1) == Some(&b':')
    {
        return Ok(Value::bool(false));
    }
    if let Some(separator) = bytes.iter().rposition(|b| *b == b'\\') {
        let (namespace, constant) = bytes.split_at(separator);
        let mut lowered = namespace.to_ascii_lowercase();
        lowered.extend_from_slice(constant);
        return Ok(Value::bool(req.constant_named(&lowered).is_some()));
    }
    Ok(Value::bool(crate::ops::defined(req, bytes)))
}

/// `var_dump(mixed $value, mixed ...$values): void`, php's
/// `php_var_dump`: each value's type and contents, one line each, and an
/// array as `php_array_element_dump` lays it out, each element's key on
/// its own line and the value indented two more than the key, so a nested
/// array steps in by two each level. No user code builds an array yet; a
/// native's answer is one, and this is how a witness reads it
/// (`ext/tokenizer`'s `token_get_all` is the first).
///
/// ```php
/// var_dump(true, false, null, 1, -2, 2.5, 0.1, "demo", extension_loaded('Core'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// NULL
/// int(1)
/// int(-2)
/// float(2.5)
/// float(0.1)
/// string(4) "demo"
/// bool(true)
/// ```
///
/// A throwable a native leaves pending takes the native's call as its
/// throw site, which php reads off the caller's opline; uncaught, it
/// names that site twice and carries the native's own frame above
/// `{main}`.
///
/// ```php
/// var_dump();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: var_dump() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): var_dump()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// An object is `php_var_dump`'s `IS_OBJECT` arm: its class, its
/// store index, its properties in declaration order with the
/// visibility php mangles into a name, an untyped property nothing
/// wrote as `NULL`, and the dynamic ones after.
///
/// ```php
/// class M { public $x = 1; public $y; protected $p = 'p'; private $q = 2.5; }
/// class N {}
/// var_dump(new M, new N);
/// ```
/// ```expect
/// object(M)#1 (4) {
///   ["x"]=>
///   int(1)
///   ["y"]=>
///   NULL
///   ["p":protected]=>
///   string(1) "p"
///   ["q":"M":private]=>
///   float(2.5)
/// }
/// object(N)#2 (0) {
/// }
/// ```
fn var_dump(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Err(too_few_arguments(req, 1, 0));
    }
    for position in 1..=u32::try_from(count).unwrap_or(u32::MAX) {
        let value = req.argument(position).unwrap_or_default();
        let mut out = Vec::new();
        dump(req, &mut out, &value, 1);
        req.write(&out);
    }
    Ok(Value::null())
}

/// One value as `php_var_dump(zv, level)` prints it, with its newline:
/// `level - 1` blanks before it past the first level, and an array's
/// elements each through `php_array_element_dump`, the key at `level + 1`
/// and the value at `level + 2`.
fn dump(req: &mut Request, out: &mut Vec<u8>, value: &Value, level: usize) {
    let value = &value.deref();
    if level > 1 {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
    if let Some(b) = value.as_bool() {
        out.extend_from_slice(format!("bool({b})\n").as_bytes());
    } else if let Some(n) = value.as_int() {
        out.extend_from_slice(format!("int({n})\n").as_bytes());
    } else if let Some(f) = value.as_float() {
        // `php_var_dump` prints a float with `%.*H` at `serialize_precision`.
        let text = php_heap::double_to_string(f, req.serialize_precision());
        out.extend_from_slice(format!("float({text})\n").as_bytes());
    } else if let Some(s) = value.as_php_string() {
        out.extend_from_slice(format!("string({}) \"", s.len()).as_bytes());
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\"\n");
    } else if let Some(array) = value.as_array() {
        out.extend_from_slice(format!("array({}) {{\n", array.len()).as_bytes());
        for (key, element) in array {
            out.extend(std::iter::repeat_n(b' ', level + 1));
            match key {
                Key::Int(n) => out.extend_from_slice(format!("[{n}]=>\n").as_bytes()),
                Key::Str(s) => {
                    out.extend_from_slice(b"[\"");
                    out.extend_from_slice(s.as_bytes());
                    out.extend_from_slice(b"\"]=>\n");
                }
            }
            dump(req, out, &element, level + 2);
        }
        if level > 1 {
            out.extend(std::iter::repeat_n(b' ', level - 1));
        }
        out.extend_from_slice(b"}\n");
    } else if let Some(object) = value.as_object() {
        dump_object(req, out, *object, level);
    } else {
        out.extend_from_slice(b"NULL\n");
    }
}

/// `php_var_dump`'s `IS_OBJECT` arm: `object(Class)#handle (count) {`,
/// then each property as `php_object_property_dump` prints it, the
/// declared ones in declaration order with the visibility php mangles
/// into the name, `["p":protected]` and `["q":"Class":private]`, and a
/// typed one nothing has written as `uninitialized(type)`, which is in
/// the listing and not in the count (`zend_array_count` over a table
/// with an empty `IS_INDIRECT`); then the dynamic ones.
fn dump_object(
    req: &mut Request,
    out: &mut Vec<u8>,
    object: crate::api::values::PhpObject,
    level: usize,
) {
    let class = object.class();
    let declared: Vec<(Vec<u8>, Visibility, Option<Type>)> = req
        .class_decl(class)
        .map(|decl| {
            decl.properties
                .iter()
                .filter(|p| p.is_instance)
                .map(|p| (p.name.0.to_vec(), p.visibility, p.ty.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut rows: Vec<(Vec<u8>, Option<Value>, Option<Type>)> = declared
        .iter()
        .filter_map(|(name, visibility, ty)| {
            let value = object.prop(req, name);
            if value.is_none() && ty.is_none() {
                return None;
            }
            let shown = match visibility {
                Visibility::Public => name.clone(),
                Visibility::Protected => [&b"\""[..], name, b"\":protected"].concat(),
                Visibility::Private => {
                    let class = req.class_name(class);
                    [&b"\""[..], name, b"\":\"", class.as_bytes(), b"\":private"].concat()
                }
            };
            Some((shown, value, ty.clone()))
        })
        .collect();
    let dynamic: Vec<(PhpString, Value)> = object.props(req).unwrap_or_default();
    for (name, value) in dynamic {
        if declared
            .iter()
            .any(|(declared, _, _)| declared == name.as_bytes())
        {
            continue;
        }
        rows.push((name.as_bytes().to_vec(), Some(value), None));
    }
    let count = rows.iter().filter(|(_, value, _)| value.is_some()).count();
    let class_name = req.class_name(class);
    out.extend_from_slice(
        format!("object({class_name})#{} ({count}) {{\n", object.index()).as_bytes(),
    );
    for (shown, value, ty) in rows {
        out.extend(std::iter::repeat_n(b' ', level + 1));
        let quoted = shown.first() == Some(&b'"');
        if quoted {
            out.extend_from_slice(b"[");
            out.extend_from_slice(&shown);
            out.extend_from_slice(b"]=>\n");
        } else {
            out.extend_from_slice(b"[\"");
            out.extend_from_slice(&shown);
            out.extend_from_slice(b"\"]=>\n");
        }
        if let Some(value) = value {
            dump(req, out, &value, level + 2);
        } else {
            out.extend(std::iter::repeat_n(b' ', level + 1));
            let ty = ty.as_ref().map(type_text).unwrap_or_default();
            out.extend_from_slice(format!("uninitialized({ty})\n").as_bytes());
        }
    }
    if level > 1 {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
    out.extend_from_slice(b"}\n");
}

/// `zend_type_to_string` over a declared type: the scalar names, a
/// class name as written, `?T`, and `|` and `&` between the members of
/// a union and an intersection.
fn type_text(ty: &Type) -> String {
    match ty {
        Type::Mixed => String::from("mixed"),
        Type::Void => String::from("void"),
        Type::Never => String::from("never"),
        Type::Null => String::from("null"),
        Type::Bool => String::from("bool"),
        Type::False => String::from("false"),
        Type::True => String::from("true"),
        Type::Int => String::from("int"),
        Type::Float => String::from("float"),
        Type::String => String::from("string"),
        Type::Array => String::from("array"),
        Type::Object => String::from("object"),
        Type::Iterable => String::from("iterable"),
        Type::Callable => String::from("callable"),
        Type::CalledClass => String::from("static"),
        Type::SelfClass => String::from("self"),
        Type::ParentClass => String::from("parent"),
        Type::Named(name) => String::from_utf8_lossy(&name.0).into_owned(),
        Type::Nullable(inner) => format!("?{}", type_text(inner)),
        Type::Union(members) => members.iter().map(type_text).collect::<Vec<_>>().join("|"),
        Type::Intersection(members) => members.iter().map(type_text).collect::<Vec<_>>().join("&"),
    }
}

/// `memory_get_usage(bool $real_usage = false): int`, php's
/// `zend_memory_usage`: the bin accounting the request has out, or under
/// `$real_usage` the bytes its chunks hold.
///
/// A fresh request holds php's one 2 MiB chunk and nothing more, so the
/// second number is that chunk and the oracle says so. The first is each
/// engine's own bookkeeping and is not pinned here: what php's request
/// heap holds beside the interned strings is on the system allocator
/// in this engine, the op array, the compiler and the tree, which
/// design §5.13 states as the one approximation and the divergence row
/// on the witness of `Buckets` records.
///
/// ```php
/// var_dump(memory_get_usage(true), memory_get_peak_usage(true));
/// memory_reset_peak_usage();
/// var_dump(memory_get_peak_usage(true));
/// ```
/// ```expect
/// int(2097152)
/// int(2097152)
/// int(2097152)
/// ```
///
/// What one allocation moves both numbers by is a number both engines
/// answer, since every string a native builds is the request's heap's
/// (design §5.13, #174, #211). A string past 2 MiB is a huge block,
/// rounded to the page the machine reports, sixteen KiB where the
/// oracle is pinned, and `zend_mm_add_huge_block` takes the block's list
/// node from the 24-byte bin of the same heap before the block is
/// counted, so the bin accounting moves by the rounded block and the
/// node, and the chunks' by the block alone:
///
/// ```php
/// $b = memory_get_usage(); $rb = memory_get_usage(true); $s = str_repeat("x", 3000000); var_dump(memory_get_usage() - $b, memory_get_usage(true) - $rb);
/// ```
/// ```expect
/// int(3014680)
/// int(3014656)
/// ```
fn memory_get_usage(req: &mut Request) -> Result<Value, Thrown> {
    let real = real_usage(req)?;
    Ok(Value::int(as_long(req.memory_usage(real))))
}

/// `memory_get_peak_usage(bool $real_usage = false): int`, php's
/// `zend_memory_peak_usage`: the high-water mark of the same two
/// numbers since the request began or the peak was last reset.
fn memory_get_peak_usage(req: &mut Request) -> Result<Value, Thrown> {
    let real = real_usage(req)?;
    Ok(Value::int(as_long(req.memory_peak(real))))
}

/// `memory_reset_peak_usage(): void`, php's
/// `zend_memory_reset_peak_usage`: both peaks become what is in use now.
/// `ZEND_PARSE_PARAMETERS_NONE` takes no argument at all, so one is
/// php's `ArgumentCountError` and not an ignored extra.
fn memory_reset_peak_usage(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 0 {
        return Err(wrong_argument_count(req, 0, 0, count));
    }
    req.reset_memory_peak();
    Ok(Value::null())
}

/// php's `ZEND_PARSE_PARAMETERS_START(0, 1)` over one optional
/// `Z_PARAM_BOOL`: no argument is `false`, a second is php's
/// `ArgumentCountError`, and what is there is coerced as php coerces a
/// bool parameter of a native, `null` included, which is the
/// 8.1 deprecation and then `false`. The two shared helpers this reaches
/// for read `$real_usage` off the declaration, so their text is php's.
fn real_usage(req: &mut Request) -> Result<bool, Thrown> {
    let count = req.argument_count();
    if count > 1 {
        return Err(wrong_argument_count(req, 0, 1, count));
    }
    let Some(given) = req.argument(1) else {
        return Ok(false);
    };
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, 1, "bool");
        return Ok(false);
    }
    let coerced = coerce_param(req, &given, &Type::Bool, 1)?;
    Ok(coerced.as_bool().unwrap_or(false))
}

/// `zend_wrong_parameters_count_error` for a call outside a native's
/// arity: `exactly` where the maximum is also the minimum, else `at
/// least` under it and `at most` over it, the bound the call missed, and
/// that bound's own plural.
fn wrong_argument_count(req: &mut Request, least: usize, most: usize, given: usize) -> Thrown {
    let how = if least == most {
        "exactly"
    } else if given < least {
        "at least"
    } else {
        "at most"
    };
    let bound = if given < least { least } else { most };
    let word = if bound == 1 { "argument" } else { "arguments" };
    arg_count_error(req, &format!("{how} {bound} {word}, {given} given"))
}

/// A byte count as php's `RETURN_LONG` returns one.
fn as_long(bytes: usize) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// `extension_loaded(string $extension): bool`, php's
/// `zend_hash_exists` over the extension table, case-insensitively.
///
/// ```php
/// var_dump(extension_loaded('Core'), extension_loaded('CORE'), extension_loaded('nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
///
/// A diagnostic raised while a native's body runs stands where the call
/// does, since `zend_get_executed_lineno` answers off the caller's
/// opline and a native pushes no opline of its own.
///
/// ```php
/// var_dump(extension_loaded(NAN));
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to string in %s on line 2
/// bool(false)
/// ```
fn extension_loaded(req: &mut Request) -> Result<Value, Thrown> {
    let Some(given) = req.argument(1) else {
        return Err(too_few_arguments(req, 1, 0));
    };
    let wanted = crate::ops::to_string(req, &given)?;
    let lowered = wanted.as_bytes().to_ascii_lowercase();
    req.release(Value::string(wanted));
    Ok(Value::bool(req.loaded_extensions().iter().any(|name| {
        name.as_bytes().to_ascii_lowercase() == lowered
    })))
}

/// `ini_get(string $option): string|false`, php's `PHP_FUNCTION(ini_get)`
/// (`Z/ext/standard/basic_functions.c:1894`): the entry's current bytes
/// as a string of the request's, `zend_ini_get_value`, or `false` for a
/// name the table has no entry under. The option is coerced as a string
/// parameter of a native is, `null` being the 8.1 deprecation and then
/// no entry.
///
/// ```php
/// var_dump(ini_get('precision'), ini_get('nope'), ini_set('precision', '10'), ini_get('precision'), ini_get(null));
/// ```
/// ```expect
/// Deprecated: ini_get(): Passing null to parameter #1 ($option) of type string is deprecated in %s on line 2
/// string(2) "14"
/// bool(false)
/// string(2) "14"
/// string(2) "10"
/// bool(false)
/// ```
fn ini_get(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let option = string_argument(req, 1)?;
    let value = req.ini_get(option.as_bytes()).map(<[u8]>::to_vec);
    Ok(match value {
        Some(value) => Value::string(PhpString::new(req, &value)?),
        None => Value::bool(false),
    })
}

/// `ini_set(string $option, string|int|float|bool|null $value):
/// string|false`, php's `PHP_FUNCTION(ini_set)`: the entry's value
/// before the call, or `false` for a name the table has no entry under,
/// and the new value as `zval_get_string` reads the argument, so `10`
/// and `'10'` set the same bytes, `true` is `1` and `null` is empty. An
/// array, an object or a resource is php's `TypeError` before the table
/// is read. What a set entry moves is read where the entry is: here
/// `precision`, at the next float echoed.
///
/// ```php
/// var_dump(ini_set('precision', '10'), ini_set('precision', 10), ini_set('nope', '1'));
/// echo 1/3;
/// ```
/// ```expect
/// string(2) "14"
/// string(2) "10"
/// bool(false)
/// 0.3333333333
/// ```
///
/// `ZEND_PARSE_PARAMETERS_START(2, 2)`: fewer and more are one
/// `ArgumentCountError`, and the option is coerced as a string parameter
/// of a native is, `null` being the 8.1 deprecation and then no entry.
///
/// ```php
/// var_dump(ini_set('precision', null), ini_set('precision', true), ini_set('precision', 1.5));
/// ```
/// ```expect
/// string(2) "14"
/// string(0) ""
/// string(1) "1"
/// ```
fn ini_set(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 2 {
        return Err(wrong_argument_count(req, 2, 2, count));
    }
    let option = string_argument(req, 1)?;
    let value = req.argument(2).unwrap_or_default();
    let value = &value.deref();
    if matches!(
        value.as_ref(),
        ValueRef::Array(_) | ValueRef::Object(_) | ValueRef::Resource(_)
    ) {
        return Err(arg_type_error(
            req,
            2,
            "be of type string|int|float|bool|null",
        ));
    }
    let value = to_string(req, value)?;
    let set = req.ini_set(option.as_bytes(), value.as_bytes());
    req.release(Value::string(value));
    Ok(match set? {
        Some(previous) => Value::string(PhpString::new(req, &previous)?),
        None => Value::bool(false),
    })
}

/// `set_time_limit(int $seconds): bool`, php's `PHP_FUNCTION(set_time_limit)`
/// (`Z/main/main.c:1631`): `max_execution_time` moved to the seconds at
/// runtime, which unsets the execution timer and arms it again from
/// here, and `true` for the entry having taken the value. The seconds
/// are read as php's `"l"` reads them, so `'5'` and `5.0` are `5`, and
/// `null` is the 8.1 deprecation and then zero, which arms no timer.
///
/// ```php
/// var_dump(set_time_limit(5), ini_set('max_execution_time', '0'), set_time_limit('7'), ini_set('max_execution_time', 0));
/// ```
/// ```expect
/// bool(true)
/// string(1) "5"
/// bool(true)
/// string(1) "7"
/// ```
///
/// What the seconds do once armed is the witness beside
/// [`crate::interrupt::interrupted`].
fn set_time_limit(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, 1, count));
    }
    let seconds = long_argument(req, 1)?;
    Ok(Value::bool(req.set_time_limit(seconds)))
}

/// One `Z_PARAM_LONG` of a native, the argument at `position` read off
/// the call frame: `null` is the 8.1 deprecation and then zero, and what
/// is there is coerced as php coerces an int parameter.
fn long_argument(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "int");
        return Ok(0);
    }
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.as_int().unwrap_or(0))
}

/// One `Z_PARAM_STR` of a native, the argument at `position` read off
/// the call frame: `null` is the 8.1 deprecation and then the empty
/// string, and what is there is coerced as php coerces a string
/// parameter. The string is the frame's, a view the native holds while
/// the call stands.
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "string");
        return PhpString::new(req, b"");
    }
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    match coerced.as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// One declared parameter.
fn parameter(name: &[u8], ty: Option<Type>, variadic: bool) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
        variadic,
        default: None,
        attributes: Vec::new(),
    }
}

/// `bool $real_usage = false`, the one parameter the two memory readers
/// share.
fn real_usage_parameter() -> Parameter {
    Parameter {
        default: Some(ConstExpr::Bool(false)),
        ..parameter(b"real_usage", Some(Type::Bool), false)
    }
}

/// One native's declaration, as the macros derive an extension's from
/// its Rust signature.
fn declared(name: &[u8], parameters: &[Parameter], returns: Option<Type>) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: parameters.to_vec(),
        return_type: returns,
        returns_by_ref: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::Engine;
    use crate::test::Capture;

    /// What the engine wrote and what ended it, for one source.
    fn served(source: &str) -> (String, Option<String>) {
        let served = Engine::builder()
            .build()
            .serve_capture(source, Capture::default());
        (
            String::from_utf8_lossy(&served.stdout).into_owned(),
            served.fatal,
        )
    }

    /// php's `ZEND_PARSE_PARAMETERS_START(0, 1)`: `$real_usage` is
    /// optional and `false` without it, and a scalar is coerced as a
    /// bool parameter of a native is.
    ///
    /// The oracle, under the pin's recipe:
    ///
    /// ```text
    /// $ php -n -r 'var_dump(memory_get_usage(true) === memory_get_usage(1));'
    /// bool(true)
    /// ```
    ///
    /// **The flagged number is the oracle's; the unflagged one is this
    /// engine's own accounting.** 2097152 is php's one 2 MiB chunk,
    /// `ZEND_MM_CHUNK_SIZE` (`Z/Zend/zend_alloc_sizes.h:22`), which
    /// `zend_mm_init` maps into `real_size` before a request runs a line
    /// (`Z/Zend/zend_alloc.c:2047-2081`), and the oracle answers it too.
    ///
    /// 49240 is the object store's table and what this source's
    /// interning costs, and every byte of it is php's own arithmetic.
    /// `init_executor` lays `EG(objects_store)` down before the request
    /// serves a line, 1024 buckets of a pointer each, 8192 bytes of the
    /// request heap (`Z/Zend/zend_execute_API.c:171`, design §5.4), and
    /// so does this engine's request. php interns a request's constant operands
    /// into `CG(interned_strings)` out of the request heap
    /// (`zend_insert_literal` through `zend_new_interned_string_request`,
    /// `Z/Zend/zend_compile.c:563-582`, `Z/Zend/zend_string.c:227-265`),
    /// so its `memory_get_usage()` counts them and this engine's counts
    /// them the same way (design §5.2, §5.13): `var_dump` takes 40 bytes
    /// and `memory_get_usage` 48, php's
    /// `ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(len))` over a 24-byte
    /// header, the bytes and the NUL. The 40960 beside them is the
    /// table's own bucket array, which php `emalloc`s on the first string
    /// a request interns and not at request start: 1024 buckets of 32
    /// bytes and 2048 hash slots of 4, `HT_SIZE_EX(1024,
    /// HT_SIZE_TO_MASK(1024))` (`Z/Zend/zend_types.h:486-493`) over the
    /// `nTableSize` `zend_interned_strings_activate` sets
    /// (`Z/Zend/zend_string.c:78-84`, `:346-349`).
    ///
    /// The oracle's number for the same source, under the recipe and the
    /// harness's own scratch tree, is 463336, and the two are still not
    /// comparable: what php's request heap holds beside the interned
    /// strings and their table is here on the system allocator, the op
    /// array, the compiler and the tree, which design §5.13 states as an
    /// approximation. The witness on `php_heap::string::Buckets` holds the
    /// two numbers to each other under the divergence row that says so
    /// (#178). What replaces this pin with a number both answer is #174's
    /// witness, where a variable in `php_compile` and a `str_repeat()`
    /// native let a snippet measure what one huge block moves
    /// `memory_get_usage()` by.
    #[test]
    fn the_real_usage_flag_is_optional_and_coerced_as_php_coerces_a_bool() {
        let (unflagged, _) = served("<?php var_dump(memory_get_usage());");
        let (flagged, _) = served("<?php var_dump(memory_get_usage(true));");
        let (coerced, _) = served("<?php var_dump(memory_get_usage(1));");
        assert_eq!(
            unflagged, "int(49240)\n",
            "the object store's 8192, the interned table's 40960 and the two strings in it, 40 and 48"
        );
        assert_eq!(flagged, "int(2097152)\n");
        assert_eq!(coerced, flagged, "php's Z_PARAM_BOOL coerces 1 to true");
    }

    /// `zend_wrong_parameters_count_error` for a call outside the arity,
    /// held here without the oracle; the trace such a call prints, with
    /// its arguments, is the witness beside `Request::trace_arguments`.
    /// php's own text, from the pinned oracle:
    ///
    /// ```text
    /// ArgumentCountError: memory_get_usage() expects at most 1 argument, 2 given
    /// ArgumentCountError: memory_get_peak_usage() expects at most 1 argument, 2 given
    /// ArgumentCountError: memory_reset_peak_usage() expects exactly 0 arguments, 1 given
    /// ArgumentCountError: ini_set() expects exactly 2 arguments, 1 given
    /// ArgumentCountError: ini_set() expects exactly 2 arguments, 3 given
    /// ArgumentCountError: set_time_limit() expects exactly 1 argument, 0 given
    /// ```
    #[test]
    fn a_call_past_the_maximum_is_php_s_argument_count_error() {
        for (source, text) in [
            (
                "<?php ini_set('precision');",
                "ini_set() expects exactly 2 arguments, 1 given",
            ),
            (
                "<?php ini_set('precision', 1, 2);",
                "ini_set() expects exactly 2 arguments, 3 given",
            ),
            (
                "<?php set_time_limit();",
                "set_time_limit() expects exactly 1 argument, 0 given",
            ),
            (
                "<?php memory_get_usage(true, true);",
                "memory_get_usage() expects at most 1 argument, 2 given",
            ),
            (
                "<?php memory_get_peak_usage(true, true);",
                "memory_get_peak_usage() expects at most 1 argument, 2 given",
            ),
            (
                "<?php memory_reset_peak_usage(true);",
                "memory_reset_peak_usage() expects exactly 0 arguments, 1 given",
            ),
        ] {
            let (_, fatal) = served(source);
            assert_eq!(
                fatal.as_deref().map(|f| f.contains(text)),
                Some(true),
                "{source} came to {fatal:?}"
            );
        }
    }

    /// `memory_reset_peak_usage()` leaves both peaks at what the request
    /// has out. The witness beside `memory_get_usage` pins the flagged
    /// number against the oracle; this holds the reset when no oracle is
    /// on the machine, and holds the unflagged number beside it.
    ///
    /// **The second number is the oracle's; the first is this engine's
    /// own accounting**, for the reason the pin above states at length.
    /// 2097152 is php's one 2 MiB chunk. 49288 is the object store's
    /// 8192 and what this source's interning costs the request: the
    /// table's own 40960 bucket array on the first string it interns,
    /// and then
    /// `memory_reset_peak_usage` 48 bytes, `var_dump` 40 and
    /// `memory_get_peak_usage` 48. php counts its own interning the same
    /// way and then counts a request heap this engine does not have yet,
    /// so its number for this source, under the recipe as `witness.php`,
    /// is 463752 and not this one; #174's witness is where a number both
    /// answer comes back.
    #[test]
    fn a_reset_leaves_the_peak_at_what_the_request_has_out() {
        let (body, fatal) = served(
            "<?php memory_reset_peak_usage(); var_dump(memory_get_peak_usage(), memory_get_peak_usage(true));",
        );
        assert_eq!(fatal, None);
        assert_eq!(
            body, "int(49288)\nint(2097152)\n",
            "the object store's 8192, the interned table's 40960 and the three strings in it, 48, 40 and 48"
        );
    }
}
