//! `Z/ext/standard/assert.c`: `assert` and `assert_options`, the
//! `ASSERT_*` constants under their 8.3 deprecation, the five `assert.*`
//! ini entries, and `AssertionError`, which php registers with this
//! extension. `ASSERTG(active)` and its kin are the ini entries read where
//! php reads `ASSERTG`; `ASSERTG(callback)` is the request's assert
//! callback, the engine's.
//!
//! The compiler hands `assert` the assertion's source as its description
//! where none is written, and skips the call while `zend.assertions` is
//! at or below zero (`zend_compile_assert`, `ZEND_ASSERT_CHECK`), so
//! what reaches here under `zend.assertions <= 0` is a call by another
//! name, `$f = 'assert'; $f(false)`, which the native answers `true`.
//! The entries' `OnUpdate*` handlers raise `assert.* INI setting is
//! deprecated` for a value that turns an assertion's default off, from
//! `ini_set` and `ini_restore` as their `on_modify`, and for a startup
//! value at the request's first moment, where this engine has a request
//! to raise it on (as `date.timezone`'s is); `assert_options` sets them
//! under php's `ZEND_INI_STAGE_ASSERT_OPTIONS`, which raises nothing.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{IniStage, Instance, Passed, Raw, Union};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::call;
use php_ext::coerce::{to_callable, to_string, truthy};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{
    Level, Thrown, arg_value_error, arity, docref, new_throwable, throw_chained, throw_value,
    uncaught_at, while_pending,
};
use php_ext::request::{Ini, Request};
use php_ext::values::{Owner, PhpObject, PhpString, Value, ValueRef};

use constants::{ASSERT_ACTIVE, ASSERT_BAIL, ASSERT_CALLBACK, ASSERT_EXCEPTION, ASSERT_WARNING};

/// `E_ERROR`, the code `zend_throw_exception` gives the `AssertionError`.
const E_ERROR: i64 = 1;

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
#[php_const]
pub(crate) mod constants {
    #[php(deprecated(since = "8.3", message = "as assert_options() is deprecated"))]
    pub(crate) const ASSERT_ACTIVE: i64 = 1;
    #[php(deprecated(since = "8.3", message = "as assert_options() is deprecated"))]
    pub(crate) const ASSERT_CALLBACK: i64 = 2;
    #[php(deprecated(since = "8.3", message = "as assert_options() is deprecated"))]
    pub(crate) const ASSERT_BAIL: i64 = 3;
    #[php(deprecated(since = "8.3", message = "as assert_options() is deprecated"))]
    pub(crate) const ASSERT_WARNING: i64 = 4;
    #[php(deprecated(since = "8.3", message = "as assert_options() is deprecated"))]
    pub(crate) const ASSERT_EXCEPTION: i64 = 5;
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
        ("assert.active", Some("1")),
        ("assert.bail", Some("0")),
        ("assert.warning", Some("1")),
        ("assert.callback", None),
        ("assert.exception", Some("1")),
    ]
    .into_iter()
    .map(|(name, default)| IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.map(|default| default.as_bytes().into()),
        user: true,
        per_dir: true,
        system: true,
    })
    .collect()
}

/// Whether a value of an entry is one its handler deprecates.
type Deprecates = fn(&[u8]) -> bool;

/// The five entries with the value of each that php's handler deprecates:
/// `OnUpdateActiveBool`, `OnUpdateBailBool`, `OnUpdateWarningBool`,
/// `OnChangeCallback` and `OnUpdateExceptionBool`
/// (`Z/ext/standard/assert.c:79-118`), in `PHP_INI_BEGIN()`'s order.
const DEPRECATED: [(&[u8], Deprecates); 5] = [
    (b"assert.active", |v| !Ini::parse_bool(v)),
    (b"assert.bail", Ini::parse_bool),
    (b"assert.warning", |v| !Ini::parse_bool(v)),
    (b"assert.callback", |v| !v.is_empty()),
    (b"assert.exception", |v| !Ini::parse_bool(v)),
];

/// The request's assertion state: whether the entry being set is
/// `assert_options`'s, php's `ZEND_INI_STAGE_ASSERT_OPTIONS`, which
/// `php_must_emit_ini_deprecation` raises nothing under, and
/// `ASSERTG(callback)`, the callback's one owner.
#[derive(Default)]
pub(crate) struct Stage {
    assert_options: bool,
    callback: Option<Owner<Value>>,
}

/// `ASSERTG(callback)` given up and cleared.
fn drop_callback(req: &mut Request) {
    if let Some(callback) = req.state::<crate::State>().assert.callback.take() {
        callback.give_up(req);
    }
}

/// One entry's handler, the `on_modify` of all five: the deprecation for
/// a value that turns an assertion's default off, and for
/// `assert.callback` the callback `assert_options` set given up, since
/// php's `ASSERTG(callback)` is the entry's string from here. As the
/// request ends neither: `php_must_emit_ini_deprecation` is false at
/// `ZEND_INI_STAGE_DEACTIVATE`, and with no code executing
/// `OnChangeCallback` leaves `ASSERTG(callback)` alone.
///
/// ```php
/// ini_set('assert.warning', '0'); ini_set('assert.active', '1'); ini_set('assert.callback', 'strlen');
/// var_dump(assert_options(ASSERT_BAIL, 1), ini_get('assert.bail'));
/// ```
/// ```expect
/// Deprecated: ini_set(): assert.warning INI setting is deprecated in %s on line 2
///
/// Deprecated: ini_set(): assert.callback INI setting is deprecated in %s on line 2
///
/// Deprecated: Constant ASSERT_BAIL is deprecated since 8.3, as assert_options() is deprecated in %s on line 3
///
/// Deprecated: Function assert_options() is deprecated since 8.3 in %s on line 3
/// int(0)
/// string(1) "1"
/// ```
fn on_modify(req: &mut Request, name: &[u8], value: &[u8]) {
    if req.ini().stage() == IniStage::Deactivate {
        return;
    }
    if name == b"assert.callback" {
        drop_callback(req);
    }
    let deprecates = DEPRECATED
        .iter()
        .any(|(entry, deprecated)| *entry == name && deprecated(value));
    if deprecates && !req.state::<crate::State>().assert.assert_options {
        let text = format!(
            "{} INI setting is deprecated",
            String::from_utf8_lossy(name)
        );
        docref(req, Level::Deprecated, &text);
    }
}

/// `PHP_RSHUTDOWN_FUNCTION(assert)` (`Z/ext/standard/assert.c:157`):
/// `ASSERTG(callback)` given up as the request ends, the string the
/// entry was made into or the callable `assert_options` set.
pub(crate) fn rshutdown(req: &mut Request) {
    drop_callback(req);
}

macro_rules! handler {
    ($name:ident, $entry:literal) => {
        #[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::on_modify`'s signature
        pub(crate) fn $name(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
            let value = value.unwrap_or_default();
            on_modify(req, $entry, value);
            Ok(true)
        }
    };
}
handler!(on_active, b"assert.active");
handler!(on_bail, b"assert.bail");
handler!(on_warning, b"assert.warning");
handler!(on_callback, b"assert.callback");
handler!(on_exception, b"assert.exception");

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
#[php_class(extends(Error))]
pub(crate) struct AssertionError;

#[php_class]
impl AssertionError {}

/// Core's `Error`, `AssertionError`'s parent.
#[php_class(extern)]
pub(crate) struct Error;

/// Core's `Throwable`, what `assert`'s `$description` may be.
#[php_class(extern)]
pub(crate) struct Throwable;

/// An `assert.*` entry as `zend_ini_parse_bool` reads it.
fn flag(req: &Request, name: &[u8]) -> bool {
    req.ini().get(name).is_some_and(Ini::parse_bool)
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
/// What the callback throws, or its resolution does, stands pending
/// while the assertion goes on, as `call_user_function` leaves it, and
/// the `AssertionError` chains it as its `previous`:
///
/// ```php
/// function cb() { throw new Exception("from the callback"); }
/// ini_set("assert.callback", "cb");
/// try { assert(false); } catch (AssertionError $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: ini_set(): assert.callback INI setting is deprecated in %s on line 3
/// assert(false) / from the callback
/// ```
///
/// The warning is raised with that throwable pending, as php's
/// `php_error_docref` is with `EG(exception)` set: a user error handler
/// is handed nothing, since `zend_call_function` returns at once, and the
/// `UNDEF` it leaves serves nothing, so with a handler set the warning is
/// not shown; under `assert.bail` the throwable is then rendered uncaught
/// at `E_WARNING`.
///
/// ```php
/// @ini_set("assert.exception", 0);
/// @ini_set("assert.bail", 1);
/// @ini_set("assert.callback", "f1");
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// set_error_handler(function ($no, $str) { echo "handler($no): $str\n"; return false; });
/// assert(false);
/// echo "not reached\n";
/// ```
/// ```expect
/// Warning: Uncaught Error: Invalid callback f1, function "f1" not found or invalid function name in %s:7
/// Stack trace:
/// #0 %s(7): assert(false, 'assert(false)')
/// #1 {main}
///   thrown in %s on line 7
/// shutdown
/// ```
///
/// ```php
/// @ini_set("assert.exception", 0);
/// function cb() { throw new Exception("from the callback"); }
/// @ini_set("assert.callback", "cb");
/// set_error_handler(function ($no, $str) { echo "handler($no): $str\n"; return false; });
/// try { var_dump(assert(false)); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// from the callback
/// ```
///
/// The entry's callback is made into a string once and kept, as
/// `ASSERTG(callback)` keeps it, so an assertion that serves a callback
/// with no function behind it keeps and peaks at php's sizes:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// @ini_set("assert.callback", "nope");
/// pk(function () { try { assert(false); } catch (Error $e) {} });
/// ```
/// ```expect
/// 4480 32
/// ```
///
/// A callback that runs is handed the op array's file name and the
/// description argument itself, as php's `ZVAL_STR` hands them, so its
/// call peaks where php's does whatever the file's path:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// @ini_set("assert.callback", "strlen");
/// pk(function () { try { assert(false); } catch (Error $e) {} });
/// ```
/// ```expect
/// 5040 32
/// ```
///
/// Under `assert.bail` without `assert.exception` the request ends there,
/// through `zend_throw_unwind_exit`, with status zero.
///
/// ```php
/// echo new ReflectionFunction('assert');
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
/// ```
#[php_function(arity = manual)]
fn assert(
    req: &mut Request,
    assertion: Raw<Value>,
    #[php(default = null)] description: Raw<Option<Union<Instance<Throwable>, PhpString>>>,
) -> Result<bool, Thrown> {
    let assertions = req.ini().get(b"zend.assertions").map_or(1, |value| {
        String::from_utf8_lossy(value)
            .trim()
            .parse::<i64>()
            .unwrap_or(0)
    });
    if !flag(req, b"assert.active") || assertions <= 0 {
        return Ok(true);
    }
    // `ZEND_PARSE_PARAMETERS_START(1, 2)` after the active check, as php
    // reads them.
    arity(req, 1, Some(2))?;
    let assertion = assertion.value();
    let description = description.hold(req)?;
    if truthy(req, &assertion)? {
        return Ok(true);
    }
    let described = match description {
        Some(Union::A(object)) => return Err(throw_value(req, &object.object().as_value())),
        Some(Union::B(text)) => Some(text),
        None => None,
    };
    let text = described.map(|text| text.as_bytes().to_vec());
    let pending = callback(req, described)?;
    let message = text.as_deref().map(String::from_utf8_lossy);
    if flag(req, b"assert.exception") {
        let message = message.as_deref().unwrap_or("");
        // `zend_throw_exception(assertion_error_ce, ..., E_ERROR)`: the
        // code written before the throw, which `zend_throw_exception_hook`
        // is told of.
        let coded = |req: &mut Request, object: &PhpObject| -> Result<(), Thrown> {
            let code = Owner::int(E_ERROR);
            php_ext::call::write_prop(req, object, b"code", code)
        };
        let thrown = if let Some(previous) = pending {
            let thrown = throw_chained(req, previous, "AssertionError", message);
            if let Some(object) = thrown.exception() {
                coded(req, object)?;
            }
            thrown
        } else {
            let error = new_throwable(req, "AssertionError", message)?;
            if let Some(object) = error.as_object() {
                coded(req, &object)?;
            }
            let thrown = throw_value(req, &error);
            error.give_up(req);
            thrown
        };
        return Err(thrown);
    }
    if flag(req, b"assert.warning") {
        let what = message.as_deref().unwrap_or("Assertion");
        let warn = |req: &mut Request| docref(req, Level::Warning, &format!("{what} failed"));
        // `php_error_docref` with what the callback threw in
        // `EG(exception)`: served as php serves a diagnostic while a
        // throwable is pending, and nothing where a fatal or an exit left.
        match &pending {
            Some(thrown) => {
                while_pending(req, thrown, warn);
            }
            None => warn(req),
        }
    }
    if flag(req, b"assert.bail") {
        // "The callback might have thrown. Use E_WARNING to print the
        // exception so we can avoid bailout and use unwind_exit."
        if let Some(thrown) = pending {
            uncaught_at(req, thrown, Level::Warning)?;
        }
        return Err(php_ext::raising::exit(req, &Value::int(0)));
    }
    if let Some(thrown) = pending {
        return Err(thrown);
    }
    Ok(false)
}

/// `assert()`'s `ASSERTG(callback)`, called with the file, the line,
/// `null` and the description where one was given: what it threw, or
/// what resolving it threw, is the `Ok`, for the assertion to carry on
/// past as php carries on with it in `EG(exception)`.
fn callback(req: &mut Request, described: Option<PhpString>) -> Result<Option<Thrown>, Thrown> {
    // `ZVAL_STRING(&ASSERTG(callback), ASSERTG(cb))`: the entry's string
    // made once and kept as the callback until the entry changes.
    if req.state::<crate::State>().assert.callback.is_none()
        && let Some(cb) = req
            .ini()
            .get(b"assert.callback")
            .filter(|cb| !cb.is_empty())
    {
        let cb = cb.to_vec();
        let cb = PhpString::new(req, &cb)?.into_value();
        req.state::<crate::State>().assert.callback = Some(cb);
    }
    let callback = req
        .state::<crate::State>()
        .assert
        .callback
        .as_deref()
        .copied();
    // `call_user_function`: what the callback, or its resolution,
    // throws stands pending while the assertion goes on, and an
    // `AssertionError` chains it as its `previous`.
    let Some(callback) = callback else {
        return Ok(None);
    };
    // `ZVAL_STR(&args[0], filename)`, the op array's file name, held
    // here for the call, and `ZVAL_STR(&args[3], description_str)`, the
    // argument itself.
    let file = req.frames().current_file().unwrap_or(b"Unknown").to_vec();
    let line = i64::from(req.frames().current_line());
    let file = PhpString::declared(req, &file)?;
    let mut arguments = vec![Value::string(*file), Value::int(line), Value::null()];
    let described = described.map(|text| Owner::take(req, text));
    if let Some(text) = &described {
        arguments.push(Value::string(**text));
    }
    let returned =
        to_callable(req, &callback).and_then(|callable| call(req, &callable, &arguments));
    if let Some(text) = described {
        text.give_up(req);
    }
    file.give_up(req);
    match returned {
        Ok(returned) => {
            returned.give_up(req);
            Ok(None)
        }
        Err(thrown) => Ok(Some(thrown)),
    }
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
///
/// ```php
/// echo new ReflectionFunction('assert_options');
/// ```
/// ```expect
/// Function [ <internal, deprecated:standard> function assert_options ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> int $option ]
///     Parameter #1 [ <optional> mixed $value = <default> ]
///   }
///   - Return [ mixed ]
/// }
/// ```
#[php_function(deprecated(since = "8.3"))]
fn assert_options(
    req: &mut Request,
    option: i64,
    value: Passed<Value>,
) -> Result<Owner<Value>, Thrown> {
    let what = option;
    let value = match value {
        Passed::Given(value) => Some(value),
        Passed::Omitted => None,
    };
    let entry: &[u8] = match what {
        ASSERT_ACTIVE => b"assert.active",
        ASSERT_BAIL => b"assert.bail",
        ASSERT_WARNING => b"assert.warning",
        ASSERT_EXCEPTION => b"assert.exception",
        ASSERT_CALLBACK => {
            let old = match req
                .state::<crate::State>()
                .assert
                .callback
                .as_deref()
                .copied()
            {
                Some(callback) => Owner::take(req, callback),
                None => match req.ini().get(b"assert.callback").map(<[u8]>::to_vec) {
                    Some(cb) if !cb.is_empty() => PhpString::new(req, &cb)?.into_value(),
                    _ => Owner::null(),
                },
            };
            if let Some(value) = value {
                drop_callback(req);
                if !matches!(value.as_ref(), ValueRef::Null) {
                    let kept = Owner::take(req, value);
                    req.state::<crate::State>().assert.callback = Some(kept);
                }
            }
            return Ok(old);
        }
        _ => return Err(arg_value_error(req, 1, "be an ASSERT_* constant")),
    };
    let old = flag(req, entry);
    if let Some(value) = value {
        let text = to_string(req, &value)?;
        req.state::<crate::State>().assert.assert_options = true;
        let set = req.ini_mut().set_string(entry, *text);
        req.state::<crate::State>().assert.assert_options = false;
        text.give_up(req);
        set?;
    }
    Ok(Owner::int(i64::from(old)))
}
