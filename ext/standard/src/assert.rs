//! `Z/ext/standard/assert.c`: `assert` and `assert_options`, the
//! `ASSERT_*` constants under their 8.3 deprecation, the five `assert.*`
//! ini entries, and `AssertionError`, which php registers with this
//! extension. `ASSERTG(active)` and its kin are the ini entries read where
//! php reads `ASSERTG`; `ASSERTG(callback)` is the request's assert
//! callback, the engine's.
//!
//! What waits elsewhere: php's compiler hands `assert` the assertion's
//! source as its description (`zend_compile_assert`) and skips the call
//! under `zend.assertions = -1`, which the compiler here does not do yet,
//! so an assertion with no description fails with an empty message; and
//! the entries' `OnUpdate*` handlers raise `assert.* INI setting is
//! deprecated` when a program or a startup ini changes one, which needs a
//! face the api has not got (#684).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::call::call;
use php_ext::coerce::{coerce_param, to_callable, to_string, truthy};
use php_ext::decl::{
    Attribute, AttributeArgument, ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr,
    EnumBacking, FunctionDecl, IniDecl, MethodDecl, Name, Type,
};
use php_ext::raising::{
    Level, Thrown, arg_value_error, deprecated, docref, new_throwable, throw_value,
};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value, ValueRef};

use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::given;
use crate::{argument_count_between, long_argument};

/// `PHP_ASSERT_*` (`Z/ext/standard/php_assert.h`), `assert_options`'s
/// `$option`.
const ASSERT_ACTIVE: i64 = 1;
const ASSERT_CALLBACK: i64 = 2;
const ASSERT_BAIL: i64 = 3;
const ASSERT_WARNING: i64 = 4;
const ASSERT_EXCEPTION: i64 = 5;

/// `E_ERROR`, the code `zend_throw_exception` gives the `AssertionError`.
const E_ERROR: i64 = 1;

/// The `#[\Deprecated(since: '8.3', ...)]` the stub puts on each constant
/// and, with no message, on `assert_options`.
fn deprecated_attribute(message: Option<&str>) -> Attribute {
    let argument = |name: &[u8], value: &str| AttributeArgument {
        name: Some(Name(name.into())),
        value: ConstExpr::Str(value.as_bytes().into()),
    };
    let mut arguments = vec![argument(b"since", "8.3")];
    arguments.extend(message.map(|message| argument(b"message", message)));
    Attribute {
        name: Name(b"Deprecated".as_slice().into()),
        arguments,
    }
}

/// The `ASSERT_*` constants, each deprecated as the stub marks it.
///
/// ```php
/// var_dump(ASSERT_ACTIVE, ASSERT_CALLBACK, ASSERT_BAIL, ASSERT_WARNING, ASSERT_EXCEPTION);
/// ```
/// ```expect
/// Deprecated: Constant ASSERT_ACTIVE is deprecated since 8.3, as assert_options() is deprecated in %s on line 2
///
/// Deprecated: Constant ASSERT_CALLBACK is deprecated since 8.3, as assert_options() is deprecated in %s on line 2
///
/// Deprecated: Constant ASSERT_BAIL is deprecated since 8.3, as assert_options() is deprecated in %s on line 2
///
/// Deprecated: Constant ASSERT_WARNING is deprecated since 8.3, as assert_options() is deprecated in %s on line 2
///
/// Deprecated: Constant ASSERT_EXCEPTION is deprecated since 8.3, as assert_options() is deprecated in %s on line 2
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(5)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    [
        ("ASSERT_ACTIVE", ASSERT_ACTIVE),
        ("ASSERT_CALLBACK", ASSERT_CALLBACK),
        ("ASSERT_BAIL", ASSERT_BAIL),
        ("ASSERT_WARNING", ASSERT_WARNING),
        ("ASSERT_EXCEPTION", ASSERT_EXCEPTION),
    ]
    .into_iter()
    .map(|(name, value)| ConstDecl {
        name: Name(name.as_bytes().into()),
        value: ConstExpr::Long(value),
        attributes: vec![deprecated_attribute(Some(
            "as assert_options() is deprecated",
        ))],
    })
    .collect()
}

/// `PHP_INI_BEGIN()` of `assert.c`: every entry `PHP_INI_ALL`.
///
/// ```php
/// var_dump(ini_get('assert.active'), ini_get('assert.bail'), ini_get('assert.warning'), ini_get('assert.callback'), ini_get('assert.exception'));
/// ```
/// ```expect
/// string(1) "1"
/// string(1) "0"
/// string(1) "1"
/// string(0) ""
/// string(1) "1"
/// ```
pub(crate) fn ini() -> Vec<IniDecl> {
    [
        ("assert.active", "1"),
        ("assert.bail", "0"),
        ("assert.warning", "1"),
        ("assert.callback", ""),
        ("assert.exception", "1"),
    ]
    .into_iter()
    .map(|(name, default)| IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.as_bytes().into(),
        user: true,
        per_dir: true,
        system: true,
    })
    .collect()
}

/// `AssertionError`, `register_class_AssertionError(zend_ce_error)`: a
/// class of its own over `Error` with nothing added.
///
/// ```php
/// $e = new AssertionError('m', 3);
/// var_dump($e instanceof Error, $e->getMessage(), $e->getCode(), (new ReflectionClass('AssertionError'))->getExtensionName());
/// ```
/// ```expect
/// bool(true)
/// string(1) "m"
/// int(3)
/// string(8) "standard"
/// ```
pub(crate) fn assertion_error_class() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let decl = ClassDecl {
        name: Name(b"AssertionError".as_slice().into()),
        kind: ClassKind::Class,
        flags: ClassFlags::default(),
        parent: Some(Name(b"Error".as_slice().into())),
        interfaces: Vec::new(),
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    };
    (decl, Vec::new())
}

/// The two natives.
///
/// ```php
/// echo new ReflectionFunction('assert'), new ReflectionFunction('assert_options');
/// ```
/// ```expect
/// Function [ <internal:standard> function assert ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $assertion ]
///     Parameter #1 [ <optional> Throwable|string|null $description = null ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal, deprecated:standard> function assert_options ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> int $option ]
///     Parameter #1 [ <optional> mixed $value = <default> ]
///   }
///   - Return [ mixed ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let options = FunctionDecl {
        attributes: vec![deprecated_attribute(None)],
        ..stub("assert_options(int $option, mixed $value = UNKNOWN): mixed")
    };
    vec![
        (
            stub("assert(mixed $assertion, Throwable|string|null $description = null): bool"),
            assert as Entry,
        ),
        (options, assert_options),
    ]
}

/// An `assert.*` entry as `zend_ini_parse_bool` reads it.
fn flag(req: &Request, name: &[u8]) -> bool {
    req.ini_get(name).is_some_and(Request::ini_parse_bool)
}

/// `assert(mixed $assertion, Throwable|string|null $description = null): bool`,
/// `PHP_FUNCTION(assert)` (`Z/ext/standard/assert.c:196`): `true` while
/// `assert.active` is off or `zend.assertions` at or below zero, and for
/// an assertion that holds; one that fails throws a `Throwable`
/// description, else serves the callback with the file, the line, `null`
/// and the description, then throws `AssertionError` with the description
/// and code `E_ERROR` under `assert.exception`, or warns `%s failed` under
/// `assert.warning`, and answers `false`; `assert.bail` ends the request.
///
/// ```php
/// var_dump(assert(true), assert(1, 'one'));
/// try { assert(false, 'it broke'); } catch (AssertionError $e) { var_dump($e->getMessage(), $e->getCode()); }
/// try { assert(0, new Exception('mine')); } catch (Exception $e) { var_dump($e->getMessage()); }
/// @assert_options(5, 0);
/// var_dump(assert(false, 'soft'));
/// @assert_options(2, 'report');
/// function report($file, $line, $null, $description = 'none') { echo basename($file), ":$line ", var_export($null, true), " $description\n"; }
/// var_dump(assert(false, 'called'));
/// @assert_options(4, 0); @assert_options(1, 0);
/// var_dump(assert(false, 'quiet'));
/// @assert_options(1, 1); @assert_options(3, 1);
/// assert(false, 'bail'); echo "not reached\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(8) "it broke"
/// int(1)
/// string(4) "mine"
///
/// Warning: assert(): soft failed in %s on line 6
/// bool(false)
/// witness.php:9 NULL called
///
/// Warning: assert(): called failed in %s on line 9
/// bool(false)
/// bool(true)
/// witness.php:13 NULL bail
/// ```
///
/// Under `assert.bail` without `assert.exception` the request ends there,
/// through `zend_throw_unwind_exit`, with status zero.
fn assert(req: &mut Request) -> Result<Value, Thrown> {
    let assertions = req.ini_get(b"zend.assertions").map_or(1, |value| {
        String::from_utf8_lossy(value)
            .trim()
            .parse::<i64>()
            .unwrap_or(0)
    });
    if !flag(req, b"assert.active") || assertions <= 0 {
        return Ok(Value::bool(true));
    }
    argument_count_between(req, 1, 2)?;
    let assertion = req.argument(1).unwrap_or_default();
    let description = match given(req, 2) {
        Some(value) => {
            let ty = Type::Union(vec![
                Type::Named(Name(b"Throwable".as_slice().into())),
                Type::String,
                Type::Null,
            ]);
            Some(coerce_param(req, &value, &ty, 2)?.deref())
        }
        None => None,
    };
    if truthy(req, &assertion.deref()) {
        return Ok(Value::bool(true));
    }
    let description = description.filter(|value| !matches!(value.as_ref(), ValueRef::Null));
    if let Some(object) = description
        .as_ref()
        .filter(|value| value.as_object().is_some())
    {
        return Err(throw_value(req, object));
    }
    let text = description
        .as_ref()
        .and_then(Value::as_str_bytes)
        .map(<[u8]>::to_vec);
    let callback = req.assert_callback().copied().or_else(|| {
        req.ini_get(b"assert.callback")
            .filter(|cb| !cb.is_empty())
            .map(<[u8]>::to_vec)
            .and_then(|cb| PhpString::new(req, &cb).ok().map(Value::string))
    });
    if let Some(callback) = callback {
        let callable = to_callable(req, &callback)?;
        let file = req.current_file().unwrap_or(b"Unknown").to_vec();
        let line = i64::from(req.current_line());
        let mut arguments = vec![
            Value::string(PhpString::new(req, &file)?),
            Value::int(line),
            Value::null(),
        ];
        if let Some(text) = &text {
            arguments.push(Value::string(PhpString::new(req, text)?));
        }
        let returned = call(req, &callable, &arguments);
        for argument in arguments {
            req.release(argument);
        }
        req.release(returned?);
    }
    let message = text.as_deref().map(String::from_utf8_lossy);
    if flag(req, b"assert.exception") {
        let error = new_throwable(req, "AssertionError", message.as_deref().unwrap_or(""))?;
        if let Some(object) = error.as_object() {
            let code = PhpString::new(req, b"code")?;
            object.set_prop(req, code, Value::int(E_ERROR))?;
        }
        let thrown = throw_value(req, &error);
        req.release(error);
        return Err(thrown);
    }
    if flag(req, b"assert.warning") {
        let what = message.as_deref().unwrap_or("Assertion");
        docref(req, Level::Warning, &format!("{what} failed"));
    }
    if flag(req, b"assert.bail") {
        return Err(php_ext::raising::exit(req, &Value::int(0)));
    }
    Ok(Value::bool(false))
}

/// `assert_options(int $option, mixed $value = UNKNOWN): mixed`,
/// `PHP_FUNCTION(assert_options)` (`Z/ext/standard/assert.c:275`),
/// deprecated since 8.3: the option's value as it stood, and with a
/// second argument the option set, the four switches through their ini
/// entries, the callback as it is given, `null` clearing it.
///
/// ```php
/// var_dump(assert_options(1), assert_options(4, 0), ini_get('assert.warning'), assert_options(2, 'strlen'), assert_options(2, null), assert_options(2));
/// try { assert_options(9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Function assert_options() is deprecated since 8.3 in %s on line 2
///
/// Deprecated: Function assert_options() is deprecated since 8.3 in %s on line 2
///
/// Deprecated: Function assert_options() is deprecated since 8.3 in %s on line 2
///
/// Deprecated: Function assert_options() is deprecated since 8.3 in %s on line 2
///
/// Deprecated: Function assert_options() is deprecated since 8.3 in %s on line 2
/// int(1)
/// int(1)
/// string(1) "0"
/// NULL
/// string(6) "strlen"
/// NULL
///
/// Deprecated: Function assert_options() is deprecated since 8.3 in %s on line 3
/// assert_options(): Argument #1 ($option) must be an ASSERT_* constant
/// ```
fn assert_options(req: &mut Request) -> Result<Value, Thrown> {
    deprecated(req, "Function assert_options() is deprecated since 8.3");
    argument_count_between(req, 1, 2)?;
    let what = long_argument(req, 1)?;
    let value = given(req, 2);
    let entry: &[u8] = match what {
        ASSERT_ACTIVE => b"assert.active",
        ASSERT_BAIL => b"assert.bail",
        ASSERT_WARNING => b"assert.warning",
        ASSERT_EXCEPTION => b"assert.exception",
        ASSERT_CALLBACK => {
            let old = match req.assert_callback().copied() {
                Some(callback) => {
                    req.addref(&callback);
                    callback
                }
                None => match req.ini_get(b"assert.callback").map(<[u8]>::to_vec) {
                    Some(cb) if !cb.is_empty() => Value::string(PhpString::new(req, &cb)?),
                    _ => Value::null(),
                },
            };
            if let Some(value) = value {
                let value = value.deref();
                if let Some(previous) = req.assert_callback().copied() {
                    req.release(previous);
                }
                if matches!(value.as_ref(), ValueRef::Null) {
                    req.set_assert_callback(None);
                } else {
                    req.addref(&value);
                    req.set_assert_callback(Some(value));
                }
            }
            return Ok(old);
        }
        _ => return Err(arg_value_error(req, 1, "be an ASSERT_* constant")),
    };
    let old = flag(req, entry);
    if let Some(value) = value {
        let text = to_string(req, &value)?;
        req.ini_set(entry, text.as_bytes())?;
        req.release(Value::string(text));
    }
    Ok(Value::int(i64::from(old)))
}
