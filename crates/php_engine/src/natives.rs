//! The natives the engine declares itself (design §13): the Zend
//! builtins that need frames or the engine's own tables, which php-src
//! keeps in `ext/standard` and this engine keeps here, so an engine with
//! no extensions serves a program that uses Zend alone (design §4.10).
//!
//! Two of them are here, the two the first witness of a loop needs.
//! Each is declared and paired with its entry exactly as an extension's
//! are, so there is one shape for a native and not two.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{FunctionDecl, Name, Parameter, Type};

use crate::api::abi::Entry;
use crate::api::raising::{Thrown, too_few_arguments};
use crate::api::request::Request;
use crate::api::values::{PhpString, Value};

/// The engine's own natives, in the order php declares them, each
/// declaration beside its entry.
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
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
                b"extension_loaded",
                &[parameter(b"extension", Some(Type::String), false)],
                Some(Type::Bool),
            ),
            extension_loaded as Entry,
        ),
    ]
}

/// `var_dump(mixed $value, mixed ...$values): void`, php's
/// `php_var_dump`: each value's type and contents, one line each.
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
fn var_dump(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    if arguments.is_empty() {
        return Err(too_few_arguments(req, 1, 0));
    }
    for value in arguments {
        let line = dumped(req, value);
        req.write(line.as_bytes());
    }
    Ok(Value::null())
}

/// One value as `php_var_dump` prints it, with its newline.
fn dumped(req: &mut Request, value: &Value) -> PhpString {
    let value = value.deref();
    if let Some(b) = value.as_bool() {
        return PhpString::from_bytes(format!("bool({b})\n").as_bytes());
    }
    if let Some(n) = value.as_int() {
        return PhpString::from_bytes(format!("int({n})\n").as_bytes());
    }
    if let Some(f) = value.as_float() {
        // `php_var_dump` prints a float with `%.*H` at `serialize_precision`.
        let text = crate::ops::double_to_string(f, req.serialize_precision());
        return PhpString::from_bytes(
            format!("float({})\n", String::from_utf8_lossy(text.as_bytes())).as_bytes(),
        );
    }
    if let Some(s) = value.as_php_string() {
        let mut line = PhpString::from_bytes(format!("string({}) \"", s.len()).as_bytes());
        line.append(s.as_bytes());
        line.append(b"\"\n");
        return line;
    }
    PhpString::from_bytes(b"NULL\n")
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
fn extension_loaded(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    let Some(wanted) = arguments.first().map(|v| crate::ops::to_string(req, v)) else {
        return Err(too_few_arguments(req, 1, 0));
    };
    let wanted = wanted.as_bytes().to_ascii_lowercase();
    Ok(Value::bool(req.loaded_extensions().iter().any(|name| {
        name.as_bytes().to_ascii_lowercase() == wanted
    })))
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
