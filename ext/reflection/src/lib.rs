//! The `reflection` extension: `php-src/ext/reflection` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/reflection/tests`, through `xtask phpt --subset`.
//!
//! What is here reads the linked tables through `api` and nothing else
//! (design §12): `ReflectionExtension` and `ReflectionFunction`, each
//! with its constructor and its `__toString`, rendering as
//! `_extension_string` and `_function_string` in `php_reflection.c` at
//! the tag render, in php's wording and order, since `xtask crosscheck`
//! holds the bytes to `php --re` (design §7.1, §8). A reflection object
//! keeps what php keeps on it, the `name` property its stub declares,
//! and looks the extension or function up again when it renders, which
//! for the linked tables is the same entry php's `intern->ptr` points
//! at. `ReflectionException` is declared here as `php_reflection.stub.php`
//! declares it, under `Exception`.
//!
//! Rendered so far: the header, and the `Functions` block with each
//! function's parameters and return. An extension's `INI`, `Constants`
//! and `Classes` blocks, `_extension_ini_string`, `_const_string` and
//! `_class_string`, are not rendered yet; `ctype` has none of the three.
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

use std::fmt::Write as _;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::call::{Function, lookup_function};
use php_ext::decl::{
    ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter,
    PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Thrown, arg_count_error, arg_null_deprecated, arg_type_error, throw};
use php_ext::request::{ExtensionId, PHP_VERSION, Request};
use php_ext::values::{PhpObject, PhpString, Value, ValueRef};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

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
    ExtensionBuilder::default()
        .classes(vec![
            (
                class(
                    b"ReflectionException",
                    ClassKind::Class,
                    Some(b"Exception"),
                    &[],
                ),
                Vec::new(),
            ),
            (
                class(b"Reflector", ClassKind::Interface, None, &[b"Stringable"]),
                Vec::new(),
            ),
            (
                ClassDecl {
                    flags: ClassFlags {
                        is_abstract: true,
                        ..ClassFlags::default()
                    },
                    properties: vec![name_property()],
                    ..class(
                        b"ReflectionFunctionAbstract",
                        ClassKind::Class,
                        None,
                        &[b"Reflector"],
                    )
                },
                Vec::new(),
            ),
            (
                class(
                    b"ReflectionFunction",
                    ClassKind::Class,
                    Some(b"ReflectionFunctionAbstract"),
                    &[],
                ),
                vec![
                    (
                        method(
                            b"__construct",
                            &[parameter(
                                b"function",
                                Type::Union(vec![Type::Named(Name::from("Closure")), Type::String]),
                            )],
                            None,
                        ),
                        reflection_function_construct as Entry,
                    ),
                    (
                        method(b"__toString", &[], Some(Type::String)),
                        reflection_function_to_string as Entry,
                    ),
                ],
            ),
            (
                ClassDecl {
                    properties: vec![name_property()],
                    ..class(
                        b"ReflectionExtension",
                        ClassKind::Class,
                        None,
                        &[b"Reflector"],
                    )
                },
                vec![
                    (
                        method(b"__construct", &[parameter(b"name", Type::String)], None),
                        reflection_extension_construct as Entry,
                    ),
                    (
                        method(b"__toString", &[], Some(Type::String)),
                        reflection_extension_to_string as Entry,
                    ),
                ],
            ),
        ])
        .request_state::<State>()
        .build("reflection")
}

/// A native's entry, the shape `ExtensionBuilder::classes` takes beside
/// each method.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

fn class(name: &[u8], kind: ClassKind, parent: Option<&[u8]>, interfaces: &[&[u8]]) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: ClassFlags::default(),
        parent: parent.map(|p| Name(p.into())),
        interfaces: interfaces.iter().map(|i| Name((*i).into())).collect(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
    }
}

/// `public string $name;`, as the stub declares it on each reflector.
fn name_property() -> PropertyDecl {
    PropertyDecl {
        name: Name::from("name"),
        visibility: Visibility::Public,
        is_instance: true,
        is_readonly: false,
        ty: Some(Type::String),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    }
}

fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

fn method(name: &[u8], parameters: &[Parameter], returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters: parameters.to_vec(),
            return_type: returns,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// `ZEND_PARSE_PARAMETERS_START(1, 1)` with one `string`: the count
/// held to exactly one, `null` the 8.1 deprecation and the empty
/// string, a scalar coerced as a string parameter is, and the
/// argument's bytes as a string of the request's.
fn one_string_argument(req: &mut Request, name: &str) -> Result<Vec<u8>, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(arg_count_error(
            req,
            &format!("exactly 1 argument, {count} given"),
        ));
    }
    let Some(given) = req.argument(1) else {
        return Err(arg_count_error(req, "exactly 1 argument, 0 given"));
    };
    let given = given.deref();
    if matches!(given.as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, 1, name);
        return Ok(Vec::new());
    }
    let coerced = php_ext::coerce::coerce_param(req, &given, &Type::String, 1)?;
    Ok(coerced.as_str_bytes().unwrap_or_default().to_vec())
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
    let key = PhpString::new(req, b"name")?;
    let value = PhpString::new(req, name)?;
    this.set_prop(req, key, Value::string(value))
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
    let name = one_string_argument(req, "string")?;
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
fn reflection_extension_to_string(req: &mut Request) -> Result<Value, Thrown> {
    let (_, name) = this_name(req, "ReflectionExtension")?;
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
    let text = extension_string(req, extension, "");
    Ok(Value::string(PhpString::new(req, text.as_bytes())?))
}

/// `ReflectionFunction::__construct(Closure|string $function)`: the
/// function under the lowered name, a leading `\` dropped, its declared
/// name kept as `$name`; a name the tables have not is php's
/// `ReflectionException`, the name as given. A `Closure` is not a
/// class here yet, so an object is the type error.
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
    let name = one_string_argument(req, "string")?;
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
    let text = function_string(req, function, "");
    Ok(Value::string(PhpString::new(req, text.as_bytes())?))
}

/// `_extension_string`: the header with the extension's number and
/// version, then the blocks it has something for. Every bundled
/// extension is `<persistent>`, as one compiled in is, and its version
/// is `PHP_VERSION`, as `PHP_CTYPE_VERSION` and its kin are.
fn extension_string(req: &Request, extension: ExtensionId, indent: &str) -> String {
    let mut out = String::new();
    let name = req.extension_name(extension).unwrap_or_default();
    let _ = writeln!(
        out,
        "{indent}Extension [ <persistent> extension #{} {name} version {PHP_VERSION} ] {{",
        req.extension_number(extension)
    );
    let functions = req.extension_functions(extension);
    if !functions.is_empty() {
        out.push_str("\n  - Functions {\n");
        for function in functions {
            out.push_str(&function_string(req, function, "    "));
        }
        let _ = writeln!(out, "{indent}  }}");
    }
    let _ = writeln!(out, "{indent}}}");
    out
}

/// `_function_string` with no scope: `Function [ <internal:ext> function
/// name ] {`, its parameters and its return row, each line under
/// `indent`. A function with the `Deprecated` attribute carries
/// `, deprecated` after `<internal`, as `ZEND_ACC_DEPRECATED` does.
fn function_string(req: &Request, function: Function, indent: &str) -> String {
    let mut out = String::new();
    let Some(decl) = req.function_decl(function) else {
        return out;
    };
    let _ = write!(out, "{indent}Function [ <internal");
    if decl
        .attributes
        .iter()
        .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated"))
    {
        out.push_str(", deprecated");
    }
    if let Some(extension) = req
        .function_extension(function)
        .and_then(|e| req.extension_name(e))
    {
        let _ = write!(out, ":{extension}");
    }
    out.push_str("> function ");
    if decl.returns_by_ref {
        out.push('&');
    }
    let _ = writeln!(out, "{} ] {{", String::from_utf8_lossy(&decl.name.0));
    let param_indent = format!("{indent}  ");
    function_parameter_string(&mut out, decl, &param_indent);
    if let Some(returns) = &decl.return_type {
        let _ = writeln!(out, "  {indent}- Return [ {} ]", type_to_string(returns));
    }
    let _ = writeln!(out, "{indent}}}");
    out
}

/// `_function_parameter_string`: `- Parameters [N] {` and one
/// `_parameter_string` line per parameter, a required one being every
/// parameter before the first with a default or the variadic one.
fn function_parameter_string(out: &mut String, decl: &FunctionDecl, indent: &str) {
    let required = decl
        .parameters
        .iter()
        .position(|p| p.default.is_some() || p.variadic)
        .unwrap_or(decl.parameters.len());
    out.push('\n');
    let _ = writeln!(out, "{indent}- Parameters [{}] {{", decl.parameters.len());
    for (offset, parameter) in decl.parameters.iter().enumerate() {
        let _ = write!(out, "{indent}  Parameter #{offset} [ ");
        out.push_str(if offset < required {
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
        if offset >= required && !parameter.variadic {
            out.push_str(" = ");
            out.push_str(
                &parameter
                    .default
                    .as_ref()
                    .map_or_else(|| String::from("<default>"), const_expr_string),
            );
        }
        out.push_str(" ]\n");
    }
    let _ = writeln!(out, "{indent}}}");
}

/// A native parameter's default as `gen_stub.php` writes it into
/// `zend_internal_arg_info.default_value`: the expression's source
/// text, which for a scalar is the scalar as written.
fn const_expr_string(expr: &php_ext::decl::ConstExpr) -> String {
    use php_ext::decl::ConstExpr;
    match expr {
        ConstExpr::Null => String::from("null"),
        ConstExpr::Bool(b) => String::from(if *b { "true" } else { "false" }),
        ConstExpr::Long(n) => n.to_string(),
        ConstExpr::Double(f) => f.to_string(),
        ConstExpr::Str(s) => format!("'{}'", String::from_utf8_lossy(s)),
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
