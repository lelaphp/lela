//! The `filter` extension: `php-src/ext/filter` as one crate under `ext/`
//! (ADR-0004). Beside the facade, one dependency, ext/standard, for the
//! two helpers `sanitizing_filters.c` includes of it; one entry,
//! `extension()`; nothing held across a call, so its request state is
//! empty (ADR-0005). The phpt subset is `php-src/ext/filter/tests`,
//! through `xtask phpt --subset`.
//!
//! `filter.c`'s dispatch is here, rule for rule: `php_filter_call` reads
//! the filter, its flags and its options out of the arguments,
//! `php_zval_filter` converts the value to a string and runs one entry
//! of `filter_list`, and `RETURN_VALIDATION_FAILED` is
//! [`Outcome::Failed`], which [`zval_filter`] turns into `false`, `null`
//! or the `Filter\FilterFailedException` as the flags say. The
//! validators are `logical`'s, the sanitizers and the callback
//! `sanitizing`'s.
//!
//! The input arrays are `IF_G`'s, what `php_sapi_filter`, the engine's
//! input filter this extension registers ([`sapi_filter`]), kept raw of
//! each variable php registered from what the host described: the query
//! string's, the cookie header's, the form body's and the host's
//! `$_SERVER` rows; `INPUT_ENV` is the environment as `$_ENV` has it. An
//! array no variable reached was never initialised, and `filter_input`
//! over it answers the missing variable's `null`.
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

mod logical;
mod sanitizing;

use std::ops::ControlFlow;

use constants::{
    FILTER_CALLBACK, FILTER_DEFAULT, FILTER_FLAG_NO_ENCODE_QUOTES, FILTER_FORCE_ARRAY,
    FILTER_NULL_ON_FAILURE, FILTER_REQUIRE_ARRAY, FILTER_REQUIRE_SCALAR,
    FILTER_SANITIZE_ADD_SLASHES, FILTER_SANITIZE_EMAIL, FILTER_SANITIZE_ENCODED,
    FILTER_SANITIZE_FULL_SPECIAL_CHARS, FILTER_SANITIZE_NUMBER_FLOAT, FILTER_SANITIZE_NUMBER_INT,
    FILTER_SANITIZE_SPECIAL_CHARS, FILTER_SANITIZE_STRING, FILTER_SANITIZE_URL,
    FILTER_THROW_ON_FAILURE, FILTER_UNSAFE_RAW, FILTER_VALIDATE_BOOL, FILTER_VALIDATE_DOMAIN,
    FILTER_VALIDATE_EMAIL, FILTER_VALIDATE_FLOAT, FILTER_VALIDATE_INT, FILTER_VALIDATE_IP,
    FILTER_VALIDATE_MAC, FILTER_VALIDATE_REGEXP, FILTER_VALIDATE_URL, INPUT_COOKIE, INPUT_ENV,
    INPUT_GET, INPUT_POST, INPUT_SERVER,
};
use php_ext::abi::{Extension, ExtensionBuilder, OrFalse, Union};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::coerce::{atoi, to_long, value_name, with_string};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, call_stack_end, docref, new_throwable,
    throw_value,
};
use php_ext::request::{Input, Request};
use php_ext::values::walk::Hold;
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpString, Value, ValueRef, walk};

/// `IF_G`'s input arrays: each variable [`sapi_filter`] was handed, raw,
/// by where it came from; `None` where none came.
#[derive(Default)]
struct State {
    post: Option<Owner<PhpArray>>,
    get: Option<Owner<PhpArray>>,
    cookie: Option<Owner<PhpArray>>,
    server: Option<Owner<PhpArray>>,
}

impl State {
    /// The array a variable from `input` is kept in, `None` for
    /// `parse_str()`'s, which php keeps nowhere, and for the
    /// environment's, which `$_ENV` has.
    fn slot(&mut self, input: Input) -> Option<&mut Option<Owner<PhpArray>>> {
        match input {
            Input::Post => Some(&mut self.post),
            Input::Get => Some(&mut self.get),
            Input::Cookie => Some(&mut self.cookie),
            Input::Server => Some(&mut self.server),
            Input::Env | Input::String => None,
        }
    }
}

/// `php_sapi_filter` (`Z/ext/filter/filter.c:504`), the input filter:
/// the variable kept raw in `IF_G`'s array for where it came from, and
/// the value handed back through `filter.default` under
/// `filter.default_flags`, as it came under `unsafe_raw`, the default.
///
/// ```php
/// parse_str("a=<b>", $o); var_dump($o, filter_input(INPUT_GET, "a"));
/// ```
/// ```expect
/// array(1) {
///   ["a"]=>
///   string(3) "<b>"
/// }
/// NULL
/// ```
///
/// `filter.default_flags` is read as `OnUpdateFlags` reads it: with no
/// value the flags are `FILTER_FLAG_NO_ENCODE_QUOTES`, and any value is
/// its `atoi`, so the empty string is 0 and the quotes are encoded:
///
/// ```php
/// foreach ([[], ['-d', 'filter.default_flags='], ['-d', 'filter.default_flags=128']] as $flags) {
///     $p = proc_open([PHP_BINARY, '-n', '-d', 'filter.default=full_special_chars', ...$flags, '-r', 'parse_str("a=%27%22%3C", $o); var_dump($o["a"]);'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
///     echo stream_get_contents($pipes[1]); proc_close($p);
/// }
/// ```
/// ```expect process
///
/// Deprecated: The filter.default ini setting is deprecated in Unknown on line 0
/// string(6) "'"&lt;"
///
/// Deprecated: The filter.default ini setting is deprecated in Unknown on line 0
/// string(16) "&#039;&quot;&lt;"
///
/// Deprecated: The filter.default ini setting is deprecated in Unknown on line 0
/// string(6) "'"&lt;"
/// ```
fn sapi_filter(
    req: &mut Request,
    input: Input,
    name: &[u8],
    value: &[u8],
) -> Result<Option<Owner<Value>>, Thrown> {
    if req
        .state::<State>()
        .slot(input)
        .is_some_and(|slot| slot.is_none())
    {
        let made = PhpArray::new(req)?;
        match req.state::<State>().slot(input) {
            Some(slot) => *slot = Some(made),
            None => made.give_up(req),
        }
    }
    let kept = req
        .state::<State>()
        .slot(input)
        .and_then(|slot| slot.as_deref().copied());
    if let Some(array) = kept {
        let raw = string_value(req, value)?;
        req.register_variable(array, name, raw)?;
    }
    let raw = string_value(req, value)?;
    let default = req
        .ini_get(b"filter.default")
        .and_then(|named| {
            FILTERS
                .iter()
                .find(|(name, _, _)| name.as_bytes().eq_ignore_ascii_case(named))
        })
        .map_or(FILTER_DEFAULT, |(_, id, _)| *id);
    if value.is_empty() || default == FILTER_UNSAFE_RAW {
        return Ok(Some(raw));
    }
    // `OnUpdateFlags`: no value is `FILTER_FLAG_NO_ENCODE_QUOTES`, any
    // value its `atoi`, so the empty string is 0.
    let flags = req
        .ini_value(b"filter.default_flags")
        .map_or(FILTER_FLAG_NO_ENCODE_QUOTES, |value| i64::from(atoi(value)));
    let run = Run {
        flags,
        options: None,
        native: "filter",
    };
    zval_filter(req, raw, default, &run).map(Some)
}

/// `PHP_RSHUTDOWN_FUNCTION(filter)`: `IF_G`'s arrays given up.
fn rshutdown(req: &mut Request) {
    let state = std::mem::take(req.state::<State>());
    for array in [state.post, state.get, state.cookie, state.server]
        .into_iter()
        .flatten()
    {
        array.give_up(req);
    }
}

/// `register_filter_symbols`' constants, in its order: the `INPUT_*`
/// types, `filter_private.h`'s flags and the filter ids,
/// `FILTER_SANITIZE_STRING` and `FILTER_SANITIZE_STRIPPED` under the
/// stub's `#[\Deprecated]`. The dispatch reads the flags and ids by
/// these names.
///
/// ```php
/// var_dump(INPUT_POST, INPUT_SERVER, FILTER_REQUIRE_ARRAY, FILTER_THROW_ON_FAILURE, FILTER_VALIDATE_BOOLEAN === FILTER_VALIDATE_BOOL, FILTER_DEFAULT, FILTER_CALLBACK, FILTER_FLAG_GLOBAL_RANGE, FILTER_FLAG_HOSTNAME);
/// var_dump(FILTER_SANITIZE_STRIPPED);
/// ```
/// ```expect
/// int(0)
/// int(5)
/// int(16777216)
/// int(268435456)
/// bool(true)
/// int(516)
/// int(1024)
/// int(536870912)
/// int(1048576)
///
/// Deprecated: Constant FILTER_SANITIZE_STRIPPED is deprecated since 8.1, use htmlspecialchars() instead in %s on line 3
/// int(513)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const INPUT_POST: i64 = 0;
    pub(crate) const INPUT_GET: i64 = 1;
    pub(crate) const INPUT_COOKIE: i64 = 2;
    pub(crate) const INPUT_ENV: i64 = 4;
    pub(crate) const INPUT_SERVER: i64 = 5;
    pub(crate) const FILTER_FLAG_NONE: i64 = 0;
    pub(crate) const FILTER_REQUIRE_SCALAR: i64 = 0x200_0000;
    pub(crate) const FILTER_REQUIRE_ARRAY: i64 = 0x100_0000;
    pub(crate) const FILTER_FORCE_ARRAY: i64 = 0x400_0000;
    pub(crate) const FILTER_NULL_ON_FAILURE: i64 = 0x800_0000;
    pub(crate) const FILTER_THROW_ON_FAILURE: i64 = 0x1000_0000;
    pub(crate) const FILTER_VALIDATE_INT: i64 = 0x0101;
    pub(crate) const FILTER_VALIDATE_BOOLEAN: i64 = FILTER_VALIDATE_BOOL;
    pub(crate) const FILTER_VALIDATE_BOOL: i64 = 0x0102;
    pub(crate) const FILTER_VALIDATE_FLOAT: i64 = 0x0103;
    pub(crate) const FILTER_VALIDATE_REGEXP: i64 = 0x0110;
    pub(crate) const FILTER_VALIDATE_DOMAIN: i64 = 0x0115;
    pub(crate) const FILTER_VALIDATE_URL: i64 = 0x0111;
    pub(crate) const FILTER_VALIDATE_EMAIL: i64 = 0x0112;
    pub(crate) const FILTER_VALIDATE_IP: i64 = 0x0113;
    pub(crate) const FILTER_VALIDATE_MAC: i64 = 0x0114;
    pub(crate) const FILTER_DEFAULT: i64 = 0x0204;
    pub(crate) const FILTER_UNSAFE_RAW: i64 = FILTER_DEFAULT;
    #[php(deprecated(since = "8.1", message = "use htmlspecialchars() instead"))]
    pub(crate) const FILTER_SANITIZE_STRING: i64 = 0x0201;
    #[php(deprecated(since = "8.1", message = "use htmlspecialchars() instead"))]
    pub(crate) const FILTER_SANITIZE_STRIPPED: i64 = FILTER_SANITIZE_STRING;
    pub(crate) const FILTER_SANITIZE_ENCODED: i64 = 0x0202;
    pub(crate) const FILTER_SANITIZE_SPECIAL_CHARS: i64 = 0x0203;
    pub(crate) const FILTER_SANITIZE_FULL_SPECIAL_CHARS: i64 = 0x020a;
    pub(crate) const FILTER_SANITIZE_EMAIL: i64 = 0x0205;
    pub(crate) const FILTER_SANITIZE_URL: i64 = 0x0206;
    pub(crate) const FILTER_SANITIZE_NUMBER_INT: i64 = 0x0207;
    pub(crate) const FILTER_SANITIZE_NUMBER_FLOAT: i64 = 0x0208;
    pub(crate) const FILTER_SANITIZE_ADD_SLASHES: i64 = 0x020b;
    pub(crate) const FILTER_CALLBACK: i64 = 0x0400;
    pub(crate) const FILTER_FLAG_ALLOW_OCTAL: i64 = 0x0001;
    pub(crate) const FILTER_FLAG_ALLOW_HEX: i64 = 0x0002;
    pub(crate) const FILTER_FLAG_STRIP_LOW: i64 = 0x0004;
    pub(crate) const FILTER_FLAG_STRIP_HIGH: i64 = 0x0008;
    pub(crate) const FILTER_FLAG_STRIP_BACKTICK: i64 = 0x0200;
    pub(crate) const FILTER_FLAG_ENCODE_LOW: i64 = 0x0010;
    pub(crate) const FILTER_FLAG_ENCODE_HIGH: i64 = 0x0020;
    pub(crate) const FILTER_FLAG_ENCODE_AMP: i64 = 0x0040;
    pub(crate) const FILTER_FLAG_NO_ENCODE_QUOTES: i64 = 0x0080;
    pub(crate) const FILTER_FLAG_EMPTY_STRING_NULL: i64 = 0x0100;
    pub(crate) const FILTER_FLAG_ALLOW_FRACTION: i64 = 0x1000;
    pub(crate) const FILTER_FLAG_ALLOW_THOUSAND: i64 = 0x2000;
    pub(crate) const FILTER_FLAG_ALLOW_SCIENTIFIC: i64 = 0x4000;
    pub(crate) const FILTER_FLAG_PATH_REQUIRED: i64 = 0x04_0000;
    pub(crate) const FILTER_FLAG_QUERY_REQUIRED: i64 = 0x08_0000;
    pub(crate) const FILTER_FLAG_IPV4: i64 = 0x10_0000;
    pub(crate) const FILTER_FLAG_IPV6: i64 = 0x20_0000;
    pub(crate) const FILTER_FLAG_NO_RES_RANGE: i64 = 0x40_0000;
    pub(crate) const FILTER_FLAG_NO_PRIV_RANGE: i64 = 0x80_0000;
    pub(crate) const FILTER_FLAG_GLOBAL_RANGE: i64 = 0x2000_0000;
    pub(crate) const FILTER_FLAG_HOSTNAME: i64 = 0x10_0000;
    pub(crate) const FILTER_FLAG_EMAIL_UNICODE: i64 = 0x10_0000;
}

/// What one filter made of the string: its value, owned, or
/// `RETURN_VALIDATION_FAILED`, which the flags turn into `false`, `null`
/// or the throwable.
enum Outcome {
    Value(Owner<Value>),
    Failed,
}

/// One pass of a filter: the flags and options `php_filter_call` read,
/// and the native whose name `get_active_function_name()` answers.
struct Run {
    flags: i64,
    options: Option<Value>,
    native: &'static str,
}

/// `PHP_INPUT_FILTER_PARAM_DECL`: the string, and the pass.
type FilterFn = fn(&mut Request, &Run, &[u8]) -> Result<Outcome, Thrown>;

/// `filter_list`, in its order: the name `filter_list()` answers, the
/// id, and the entry.
const FILTERS: [(&str, i64, FilterFn); 21] = [
    ("int", FILTER_VALIDATE_INT, logical::int),
    ("boolean", FILTER_VALIDATE_BOOL, logical::boolean),
    ("float", FILTER_VALIDATE_FLOAT, logical::float),
    ("validate_regexp", FILTER_VALIDATE_REGEXP, logical::regexp),
    ("validate_domain", FILTER_VALIDATE_DOMAIN, logical::domain),
    ("validate_url", FILTER_VALIDATE_URL, logical::url),
    ("validate_email", FILTER_VALIDATE_EMAIL, logical::email),
    ("validate_ip", FILTER_VALIDATE_IP, logical::ip),
    ("validate_mac", FILTER_VALIDATE_MAC, logical::mac),
    ("string", FILTER_SANITIZE_STRING, sanitizing::string),
    ("stripped", FILTER_SANITIZE_STRING, sanitizing::string),
    ("encoded", FILTER_SANITIZE_ENCODED, sanitizing::encoded),
    (
        "special_chars",
        FILTER_SANITIZE_SPECIAL_CHARS,
        sanitizing::special_chars,
    ),
    (
        "full_special_chars",
        FILTER_SANITIZE_FULL_SPECIAL_CHARS,
        sanitizing::full_special_chars,
    ),
    ("unsafe_raw", FILTER_DEFAULT, sanitizing::unsafe_raw),
    ("email", FILTER_SANITIZE_EMAIL, sanitizing::email),
    ("url", FILTER_SANITIZE_URL, sanitizing::url),
    (
        "number_int",
        FILTER_SANITIZE_NUMBER_INT,
        sanitizing::number_int,
    ),
    (
        "number_float",
        FILTER_SANITIZE_NUMBER_FLOAT,
        sanitizing::number_float,
    ),
    (
        "add_slashes",
        FILTER_SANITIZE_ADD_SLASHES,
        sanitizing::add_slashes,
    ),
    ("callback", FILTER_CALLBACK, sanitizing::callback),
];

/// The warning for an id `PHP_FILTER_ID_EXISTS` refuses.
fn unknown_filter(req: &mut Request, id: i64) {
    docref(req, Level::Warning, &format!("Unknown filter with ID {id}"));
}

/// `PHP_FILTER_ID_EXISTS`.
const fn id_exists(id: i64) -> bool {
    matches!(id, 0x0200..=0x020b | 0x0100..=0x0115 | FILTER_CALLBACK)
}

/// `php_find_filter`: the entry with the id, and the default's where
/// none has it.
fn find_filter(id: i64) -> (&'static str, FilterFn) {
    FILTERS
        .iter()
        .find(|(_, of, _)| *of == id)
        .map_or(("unsafe_raw", sanitizing::unsafe_raw as FilterFn), |row| {
            (row.0, row.2)
        })
}

/// The extension value (design §7.1): the seven natives `filter.stub.php`
/// declares, each by `#[php_function]` in the stub's order, its constants
/// in `register_filter_symbols`' order, the two throwable classes and the
/// two ini entries.
///
/// ```php
/// var_dump(extension_loaded('filter'), ini_get('filter.default'), ini_get('filter.default_flags'));
/// echo new ReflectionFunction('filter_var'), new ReflectionFunction('filter_input_array');
/// ```
/// ```expect
/// bool(true)
/// string(10) "unsafe_raw"
/// string(0) ""
/// Function [ <internal:filter> function filter_var ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> int $filter = FILTER_DEFAULT ]
///     Parameter #2 [ <optional> array|int $options = 0 ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:filter> function filter_input_array ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> int $type ]
///     Parameter #1 [ <optional> array|int $options = FILTER_DEFAULT ]
///     Parameter #2 [ <optional> bool $add_empty = true ]
///   }
///   - Return [ array|false|null ]
/// }
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::declared())
        .functions(vec![
            filter_has_var::declared(),
            filter_input::declared(),
            filter_var::declared(),
            filter_input_array::declared(),
            filter_var_array::declared(),
            filter_list::declared(),
            filter_id::declared(),
        ])
        .class::<FilterException>()
        .class::<FilterFailedException>()
        .ini(vec![
            ini_entry(b"filter.default", Some(b"unsafe_raw")),
            ini_entry(b"filter.default_flags", None),
        ])
        .on_modify(b"filter.default", update_default_filter)
        .request_state::<State>()
        .rshutdown(rshutdown)
        .input_filter(sapi_filter)
        .info(filter_info)
        .build("filter")
}

/// `UpdateDefaultFilter`, the entry's on-modify rule: a name of
/// `filter_list` other than the default's, without case, is php's
/// deprecation, raised by `zend_error` with no origin; anything else
/// falls back to the default in silence, and the value is taken either
/// way. The entry is `PHP_INI_SYSTEM|PHP_INI_PERDIR`, so `ini_set()`
/// refuses it before the rule and the startup stage is the only one the
/// rule meets, over the value the host started the engine with, where
/// php displays what it raises before any request
/// ([`php_ext::raising::startup`]):
///
/// ```php
/// var_dump(ini_get('filter.default'));
/// ```
/// ```expect regime=filter-default
/// Deprecated: The filter.default ini setting is deprecated in Unknown on line 0
/// string(3) "int"
/// ```
///
/// What a program meets is the refusal:
///
/// ```php
/// var_dump(ini_set('filter.default', 'int'), ini_set('filter.default_flags', '0'), ini_get('filter.default'));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// string(10) "unsafe_raw"
/// ```
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature, which may throw
fn update_default_filter(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    let named = FILTERS
        .iter()
        .find(|(name, _, _)| name.as_bytes().eq_ignore_ascii_case(value));
    if named.is_some_and(|(_, id, _)| *id != FILTER_DEFAULT) {
        php_ext::raising::startup(
            req,
            Level::Deprecated,
            "The filter.default ini setting is deprecated",
        );
    }
    Ok(true)
}

/// `PHP_MINFO_FUNCTION(filter)`: the one row, then the ini entries.
///
/// ```php
/// (new ReflectionExtension('filter'))->info();
/// ```
/// ```expect
/// filter
///
/// Input Validation and Filtering => enabled
///
/// Directive => Local Value => Master Value
/// filter.default => unsafe_raw => unsafe_raw
/// filter.default_flags => no value => no value
/// ```
fn filter_info(req: &mut Request) {
    req.info_table(&[&["Input Validation and Filtering", "enabled"]]);
    let shown = |req: &Request, name: &[u8]| {
        String::from_utf8_lossy(req.ini_get(name).unwrap_or_default()).into_owned()
    };
    let default = shown(req, b"filter.default");
    let flags = shown(req, b"filter.default_flags");
    req.info_ini_table(&[
        ["filter.default", &default, &default],
        ["filter.default_flags", &flags, &flags],
    ]);
}

/// `STD_PHP_INI_ENTRY(..., PHP_INI_SYSTEM|PHP_INI_PERDIR, ...)`, its
/// default `None` for php's `NULL`.
fn ini_entry(name: &[u8], default: Option<&[u8]>) -> IniDecl {
    IniDecl {
        name: Name(name.into()),
        default: default.map(Into::into),
        user: false,
        per_dir: true,
        system: true,
    }
}

/// Core's `Exception`, named for `extends`.
#[php_class(extern)]
struct Exception;

/// `namespace Filter { class FilterException extends \Exception {} class
/// FilterFailedException extends FilterException {} }`.
///
/// ```php
/// try { filter_var("x", FILTER_VALIDATE_INT, FILTER_THROW_ON_FAILURE); } catch (Filter\FilterException $e) { var_dump(get_class($e), get_parent_class($e), $e->getMessage()); }
/// ```
/// ```expect
/// string(28) "Filter\FilterFailedException"
/// string(22) "Filter\FilterException"
/// string(57) "filter validation failed: filter int not satisfied by 'x'"
/// ```
#[php_class(namespace(Filter), extends(Exception))]
struct FilterException;

#[php_class]
impl FilterException {}

#[php_class(namespace(Filter), extends(FilterException))]
struct FilterFailedException;

#[php_class]
impl FilterFailedException {}

/// `zend_hash_str_find` over an options array: the element under a
/// string key, a view.
fn find(options: Option<&Value>, name: &[u8]) -> Option<Value> {
    options?.deref().as_array()?.find(name)
}

/// A new string of the request's as a value.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// The bytes as C's `%s` prints them, up to the first NUL.
fn c_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or(&[])).into_owned()
}

/// The sentence the two failure flags together are refused with.
const BOTH_FAILURES: &str = "cannot use both FILTER_NULL_ON_FAILURE and FILTER_THROW_ON_FAILURE";

/// `zend_argument_value_error(n, "...")` over the `$options` parameter
/// with a sentence that does not open with `must`, which
/// `arg_value_error` supplies.
fn options_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    php_ext::raising::argument_error(req, "ValueError", position, tail)
}

/// `zend_throw_exception(php_filter_failed_exception_ce, message, 0)`.
fn throw_failed(req: &mut Request, message: &str) -> Thrown {
    match new_throwable(req, "Filter\\FilterFailedException", message) {
        Ok(value) => {
            let thrown = throw_value(req, &value);
            value.give_up(req);
            thrown
        }
        Err(thrown) => thrown,
    }
}

/// Whether the class, or a class it extends, has `__toString`, php's
/// `ce->__tostring`.
fn has_to_string(req: &mut Request, class: ClassId) -> bool {
    req.method_row(class, b"__tostring").is_some()
}

// ------------------------------------------------------------ dispatch

/// `php_zval_filter(value, filter, flags, options, charset)`: `value`
/// owned, what it became handed back owned.
fn zval_filter(
    req: &mut Request,
    value: Owner<Value>,
    filter: i64,
    run: &Run,
) -> Result<Owner<Value>, Thrown> {
    let (name, function) = find_filter(filter);
    let flags = run.flags;
    let filtered = if let Some(object) = (*value).deref().as_object().copied()
        && !has_to_string(req, object.class())
    {
        // #49274: an object with no `__toString` fails nicely, and
        // #67167 makes the failure `false` or `null`.
        value.give_up(req);
        if flags & FILTER_THROW_ON_FAILURE != 0 {
            let class = req
                .class_decl(object.class())
                .map(|decl| c_string(&decl.name.0))
                .unwrap_or_default();
            return Err(throw_failed(
                req,
                &format!(
                    "filter validation failed: object of type {class} has no __toString() method"
                ),
            ));
        }
        Owner::take(req, failure(flags))
    } else {
        let bytes = with_string(req, &value, <[u8]>::to_vec);
        value.give_up(req);
        let bytes = bytes?;
        match function(req, run, &bytes)? {
            Outcome::Value(value) => value,
            Outcome::Failed if flags & FILTER_THROW_ON_FAILURE != 0 => {
                return Err(throw_failed(
                    req,
                    &format!(
                        "filter validation failed: filter {name} not satisfied by '{}'",
                        c_string(&bytes)
                    ),
                ));
            }
            Outcome::Failed => Owner::take(req, failure(flags)),
        }
    };
    // `handle_default`: the `default` option in place of the failure.
    let failed = if flags & FILTER_NULL_ON_FAILURE != 0 {
        matches!(filtered.as_ref(), ValueRef::Null)
    } else {
        matches!(filtered.as_ref(), ValueRef::Bool(false))
    };
    if failed
        && run
            .options
            .as_ref()
            .is_some_and(|o| o.deref().as_array().is_some())
        && let Some(default) = find(run.options.as_ref(), b"default")
    {
        let default = default.deref();
        filtered.give_up(req);
        return Ok(Owner::take(req, default));
    }
    Ok(filtered)
}

/// What `RETURN_VALIDATION_FAILED` leaves without the throwable.
fn failure(flags: i64) -> Value {
    if flags & FILTER_NULL_ON_FAILURE != 0 {
        Value::null()
    } else {
        Value::bool(false)
    }
}

/// `php_zval_filter_recursive` over what `filter_var` and
/// `php_filter_array_handler` hand it, `value` owned: a scalar through
/// the filter, and an array as php walks its `ZVAL_DUP`, [`filtered_copy`].
fn zval_filter_recursive(
    req: &mut Request,
    value: Owner<Value>,
    filter: i64,
    run: &Run,
) -> Result<Owner<Value>, Thrown> {
    let Some(array) = (*value).deref().as_array().copied() else {
        return zval_filter(req, value, filter, run);
    };
    let copied = filtered_copy(req, array, filter, run);
    value.give_up(req);
    copied.map(Owner::into_value)
}

/// `ZVAL_DUP` and then `php_zval_filter_recursive` over the copy: the
/// copy keeps a reference its elements share as that reference, so a
/// value behind one is filtered where it stands and the variable it
/// names reads what the filter made of it, as php's walk writes through
/// `ZVAL_DEREF`. An array reached again through a reference while it is
/// walked is walked once. The copy owned; `array` a view.
///
/// ```php
/// $inner = ["1", "x"];
/// $outer = [&$inner, "2", ["3"]];
/// var_dump(filter_var_array($outer, FILTER_VALIDATE_INT), $inner);
/// $n = "7";
/// var_dump(filter_var([&$n], FILTER_VALIDATE_INT, FILTER_REQUIRE_ARRAY), $n);
/// $a = ["5"];
/// $a[] = &$a;
/// var_dump(filter_var($a, FILTER_VALIDATE_INT, FILTER_REQUIRE_ARRAY));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   &array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     bool(false)
///   }
///   [1]=>
///   int(2)
///   [2]=>
///   array(1) {
///     [0]=>
///     int(3)
///   }
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   bool(false)
/// }
/// array(1) {
///   [0]=>
///   &int(7)
/// }
/// int(7)
/// array(2) {
///   [0]=>
///   int(5)
///   [1]=>
///   &array(2) {
///     [0]=>
///     int(5)
///     [1]=>
///     *RECURSION*
///   }
/// }
/// ```
fn filtered_copy(
    req: &mut Request,
    array: PhpArray,
    filter: i64,
    run: &Run,
) -> Result<Owner<PhpArray>, Thrown> {
    let copied = array.copy(req)?;
    // The copy has the one owner, so the walk's writes never separate it
    // and the view walked is the array the owner stands on.
    let mut walked = *copied;
    match filter_in_place(req, &mut walked, filter, run) {
        Ok(()) => Ok(copied),
        Err(thrown) => {
            copied.give_up(req);
            Err(thrown)
        }
    }
}

/// `php_zval_filter_recursive` over an array with one owner, each
/// element filtered where it stands; one already being walked, reached
/// again through a reference, is left as it is.
fn filter_in_place(
    req: &mut Request,
    array: &mut PhpArray,
    filter: i64,
    run: &Run,
) -> Result<(), Thrown> {
    if array.is_recursive() {
        return Ok(());
    }
    call_stack_end(req)?;
    array.protect();
    let walked = filter_elements(req, array, filter, run);
    array.unprotect();
    walked
}

/// The walk's body. Behind a reference, `ZVAL_DEREF`: an array
/// separated in the cell (`SEPARATE_ARRAY`) and walked there, and a
/// scalar filtered and written back into the cell. A plain element is
/// filtered and put back in its slot, a nested array as a filtered copy,
/// which is what `SEPARATE_ARRAY` makes of an element the dup gave a
/// second owner.
fn filter_elements(
    req: &mut Request,
    array: &mut PhpArray,
    filter: i64,
    run: &Run,
) -> Result<(), Thrown> {
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(value), element)`
    // (`Z/ext/filter/filter.c:409`), no owner of the table taken: the
    // separated array holds it while a filter's callback runs, and each
    // row is written in place as php writes it.
    walk::foreach(req, *array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let element = *row.value();
        if let ValueRef::Reference(cell) = element.as_ref() {
            let cell = *cell;
            if let Some(mut held) = cell.separate_array(req)? {
                filter_in_place(req, &mut held, filter, run)?;
            } else {
                let held = Owner::take(req, cell.get());
                let filtered = zval_filter(req, held, filter, run)?;
                cell.convert(req, filtered);
            }
            return Ok(ControlFlow::Continue(()));
        }
        let element = Owner::take(req, element);
        let filtered = zval_filter_recursive(req, element, filter, run)?;
        array.replace(req, &key, filtered)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `php_filter_call`: `value` owned, what it became owned, and
/// `options_position` the parameter a refusal names.
fn filter_call(
    req: &mut Request,
    native: &'static str,
    value: Owner<Value>,
    mut filter: i64,
    arguments: Union<PhpArray, i64>,
    mut flags: i64,
    options_position: u32,
) -> Result<Owner<Value>, Thrown> {
    let mut options = None;
    match arguments {
        Union::B(long) if filter != -1 => {
            flags = long;
            if flags & (FILTER_REQUIRE_ARRAY | FILTER_FORCE_ARRAY) == 0 {
                flags |= FILTER_REQUIRE_SCALAR;
            }
        }
        Union::B(long) => filter = long,
        Union::A(table) => {
            let table = Some(table.to_value());
            if let Some(given) = find(table.as_ref(), b"filter") {
                filter = match to_long(req, &given.deref()) {
                    Ok(filter) => filter,
                    Err(thrown) => {
                        value.give_up(req);
                        return Err(thrown);
                    }
                };
            }
            if let Some(given) = find(table.as_ref(), b"options") {
                let given = given.deref();
                if filter == FILTER_CALLBACK {
                    options = Some(given);
                    flags = 0;
                } else if given.as_array().is_some() {
                    options = Some(given);
                }
            }
            if let Some(given) = find(table.as_ref(), b"flags") {
                flags = match to_long(req, &given.deref()) {
                    Ok(flags) => flags,
                    Err(thrown) => {
                        value.give_up(req);
                        return Err(thrown);
                    }
                };
                if flags & (FILTER_REQUIRE_ARRAY | FILTER_FORCE_ARRAY) == 0 {
                    flags |= FILTER_REQUIRE_SCALAR;
                }
            }
        }
    }
    if flags & FILTER_NULL_ON_FAILURE != 0 && flags & FILTER_THROW_ON_FAILURE != 0 {
        value.give_up(req);
        return Err(options_error(req, options_position, BOTH_FAILURES));
    }
    let run = Run {
        flags,
        options,
        native,
    };
    if (*value).deref().as_array().is_some() {
        if flags & FILTER_REQUIRE_SCALAR != 0 {
            value.give_up(req);
            if flags & FILTER_THROW_ON_FAILURE != 0 {
                return Err(throw_failed(
                    req,
                    "filter validation failed: not a scalar value (got an array)",
                ));
            }
            return Ok(Owner::take(req, failure(flags)));
        }
        return zval_filter_recursive(req, value, filter, &run);
    }
    if flags & FILTER_REQUIRE_ARRAY != 0 {
        let name = value_name(req, &(*value).deref());
        value.give_up(req);
        if flags & FILTER_THROW_ON_FAILURE != 0 {
            return Err(throw_failed(
                req,
                &format!("filter validation failed: not an array (got {name})"),
            ));
        }
        return Ok(Owner::take(req, failure(flags)));
    }
    let filtered = zval_filter(req, value, filter, &run)?;
    if flags & FILTER_FORCE_ARRAY != 0 {
        let mut wrapped = PhpArray::new(req)?;
        wrapped.append(req, filtered)?;
        return Ok(wrapped.into_value());
    }
    Ok(filtered)
}

/// `php_filter_array_handler(input, op_ht, op_long, return_value,
/// add_empty)`. Without a definition it is `php_filter_call(input, -1,
/// NULL, op_long, return_value, FILTER_REQUIRE_ARRAY)`, which walks the
/// input array through the one filter `op_long` names.
fn array_handler(
    req: &mut Request,
    native: &'static str,
    input: PhpArray,
    definition: Union<PhpArray, i64>,
    add_empty: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let table = match definition {
        Union::A(table) => table,
        Union::B(filter) => {
            let run = Run {
                flags: FILTER_REQUIRE_ARRAY,
                options: None,
                native,
            };
            return filtered_copy(req, input, filter, &run);
        }
    };
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(op_ht, arg_key, arg_elm)`
    // (`Z/ext/filter/filter.c:612`), no owner of the table taken: the
    // argument holds it while a filter's callback runs. A throwable gives
    // the array made so far up.
    let walked = walk::foreach(req, table, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let element = row.value().deref();
        let name = match key {
            Key::Str(name) if !name.is_empty() => name,
            Key::Str(_) => {
                return Err(options_error(req, 2, "cannot contain empty keys"));
            }
            Key::Int(_) => {
                return Err(arg_type_error(req, 2, "contain only string keys"));
            }
        };
        let Some(found) = lookup(req, input, name.as_bytes(), false) else {
            if add_empty {
                let key = Owner::take(req, Key::Str(name));
                out.set(req, key, Owner::null())?;
            }
            return Ok(ControlFlow::Continue(()));
        };
        let element = element.deref();
        let arguments = if let Some(array) = element.as_array() {
            Union::A(*array)
        } else {
            let id = to_long(req, &element)?;
            if !id_exists(id) {
                unknown_filter(req, id);
            }
            Union::B(id)
        };
        let found = Owner::take(req, found.deref());
        let filtered = filter_call(req, native, found, -1, arguments, FILTER_REQUIRE_SCALAR, 2)?;
        let key = Owner::take(req, Key::Str(name));
        out.set(req, key, filtered)?;
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// `php_filter_get_storage` (`Z/ext/filter/filter.c:425`): the input
/// array a type names, `None` where no variable reached it, and php's
/// `ValueError` for a type that is not one; `$_SERVER` and `$_ENV` made
/// first where they are made when named, so their variables have
/// reached the filter. The array has one more owner, the caller's.
///
/// ```php
/// var_dump(filter_input_array(INPUT_GET), is_array(filter_input_array(INPUT_ENV)));
/// ```
/// ```expect
/// NULL
/// bool(true)
/// ```
fn storage(req: &mut Request, input: i64) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let kept = match input {
        INPUT_POST => req.state::<State>().post.as_deref().copied(),
        INPUT_GET => req.state::<State>().get.as_deref().copied(),
        INPUT_COOKIE => req.state::<State>().cookie.as_deref().copied(),
        INPUT_SERVER => {
            let _ = req.superglobal(b"_SERVER");
            req.state::<State>().server.as_deref().copied()
        }
        INPUT_ENV => req.superglobal(b"_ENV"),
        _ => return Err(arg_value_error(req, 1, "be an INPUT_* constant")),
    };
    Ok(kept.map(|array| Owner::take(req, array)))
}

/// `zend_hash_find` by a variable name, a view: a string key alone, or
/// with `numeric` the integer key the name is, as `zend_symtable_find`
/// reads it.
fn lookup(req: &Request, array: PhpArray, name: &[u8], numeric: bool) -> Option<Value> {
    walk::rows(req, array).find_map(|(key, value)| match key {
        Key::Str(s) if s.as_bytes() == name => Some(value.deref()),
        Key::Int(n) if numeric && n.to_string().as_bytes() == name => Some(value.deref()),
        _ => None,
    })
}

// ------------------------------------------------------------ natives

/// `filter_has_var(int $input_type, string $var_name): bool`.
///
/// ```php
/// var_dump(filter_has_var(INPUT_SERVER, "PHP_SELF"), filter_has_var(INPUT_SERVER, "nope"), filter_has_var(INPUT_GET, "a"), filter_has_var(INPUT_ENV, "surely_unset"));
/// try { filter_has_var(3, "a"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// filter_has_var(): Argument #1 ($input_type) must be an INPUT_* constant
/// ```
#[php_function]
fn filter_has_var(req: &mut Request, input_type: i64, var_name: PhpString) -> Result<bool, Thrown> {
    let Some(array) = storage(req, input_type)? else {
        return Ok(false);
    };
    let found = lookup(req, *array, var_name.as_bytes(), false).is_some();
    array.give_up(req);
    Ok(found)
}

/// `filter_input(int $type, string $var_name, int $filter =
/// FILTER_DEFAULT, array|int $options = 0): mixed`: the variable
/// filtered as `filter_var` filters it; a missing one is `null`, `false`
/// under `FILTER_NULL_ON_FAILURE`, the `default` option where there is
/// one, and the throwable under `FILTER_THROW_ON_FAILURE`.
///
/// ```php
/// var_dump(filter_input(INPUT_SERVER, "SCRIPT_NAME", FILTER_VALIDATE_INT), filter_input(INPUT_GET, "a"), filter_input(INPUT_GET, "a", FILTER_DEFAULT, FILTER_NULL_ON_FAILURE), filter_input(INPUT_POST, "a", FILTER_VALIDATE_INT, ["options" => ["default" => 7]]), filter_input(INPUT_ENV, "surely_unset"));
/// try { filter_input(INPUT_GET, "a", FILTER_DEFAULT, FILTER_THROW_ON_FAILURE); } catch (Filter\FilterFailedException $e) { echo $e->getMessage(), "\n"; }
/// var_dump(filter_input(INPUT_GET, "a", 9999));
/// ```
/// ```expect
/// bool(false)
/// NULL
/// bool(false)
/// int(7)
/// NULL
/// input value 'a' not found
///
/// Warning: filter_input(): Unknown filter with ID 9999 in %s on line 4
/// bool(false)
/// ```
#[php_function]
fn filter_input(
    req: &mut Request,
    r#type: i64,
    var_name: PhpString,
    #[php(default = FILTER_DEFAULT)] filter: i64,
    #[php(default = 0)] options: Union<PhpArray, i64>,
) -> Result<Owner<Value>, Thrown> {
    let name = var_name.as_bytes();
    if !id_exists(filter) {
        unknown_filter(req, filter);
        return Ok(Owner::bool(false));
    }
    let array = storage(req, r#type)?;
    let found = array
        .as_deref()
        .and_then(|array| lookup(req, *array, name, false).map(|v| v.deref()));
    let found = found.map(|found| Owner::take(req, found));
    if let Some(array) = array {
        array.give_up(req);
    }
    if let Some(found) = found {
        return filter_call(
            req,
            "filter_input",
            found,
            filter,
            options,
            FILTER_REQUIRE_SCALAR,
            4,
        );
    }
    let (table, flags) = match options {
        Union::B(flags) => (None, flags),
        Union::A(table) => {
            let table = Some(table.to_value());
            match find(table.as_ref(), b"flags") {
                Some(given) => (table, to_long(req, &given.deref())?),
                None => (table, 0),
            }
        }
    };
    if flags & FILTER_NULL_ON_FAILURE != 0 && flags & FILTER_THROW_ON_FAILURE != 0 {
        return Err(options_error(req, 4, BOTH_FAILURES));
    }
    if flags & FILTER_THROW_ON_FAILURE != 0 {
        return Err(throw_failed(
            req,
            &format!("input value '{}' not found", c_string(name)),
        ));
    }
    let options = find(table.as_ref(), b"options").map(|o| o.deref());
    if options.as_ref().is_some_and(|o| o.as_array().is_some())
        && let Some(default) = find(options.as_ref(), b"default")
    {
        return Ok(Owner::take(req, default.deref()));
    }
    // Inverted on purpose, as `filter.c` says: a missing variable is
    // `false` under the flag that makes a failure `null`.
    let missing = if flags & FILTER_NULL_ON_FAILURE != 0 {
        Value::bool(false)
    } else {
        Value::null()
    };
    Ok(Owner::take(req, missing))
}

/// `filter_var(mixed $value, int $filter = FILTER_DEFAULT, array|int
/// $options = 0): mixed`: the value converted to a string and run
/// through one filter; an array is refused unless the flags ask for one,
/// and then filtered element by element.
///
/// ```php
/// try { filter_var_array([], ""); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var("7", FILTER_VALIDATE_INT, "5abc"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var("7", FILTER_VALIDATE_INT, 9e30); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(filter_var("  42 ", FILTER_VALIDATE_INT), filter_var("abc"), filter_var(1.5), filter_var(true), filter_var(null), filter_var([1], FILTER_VALIDATE_INT), filter_var(["1", ["x", "3"]], FILTER_VALIDATE_INT, FILTER_REQUIRE_ARRAY), filter_var("5", FILTER_VALIDATE_INT, FILTER_FORCE_ARRAY), filter_var("5", FILTER_VALIDATE_INT, FILTER_REQUIRE_ARRAY));
/// ```
/// ```expect
/// filter_var_array(): Argument #2 ($options) must be of type array|int, string given
/// filter_var(): Argument #3 ($options) must be of type array|int, string given
/// filter_var(): Argument #3 ($options) must be of type array|int, float given
/// int(42)
/// string(3) "abc"
/// string(3) "1.5"
/// string(1) "1"
/// string(0) ""
/// bool(false)
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   array(2) {
///     [0]=>
///     bool(false)
///     [1]=>
///     int(3)
///   }
/// }
/// array(1) {
///   [0]=>
///   int(5)
/// }
/// bool(false)
/// ```
///
/// The options array names the filter, its flags and its options; a
/// failure is `false`, `null` under `FILTER_NULL_ON_FAILURE`, the
/// `default` option where one is given, and the throwable under
/// `FILTER_THROW_ON_FAILURE`, which the two together refuse.
///
/// ```php
/// var_dump(filter_var("x", FILTER_VALIDATE_INT, FILTER_NULL_ON_FAILURE), filter_var("x", FILTER_DEFAULT, ["filter" => FILTER_VALIDATE_INT, "options" => ["default" => -1]]), filter_var("9", FILTER_VALIDATE_INT, ["options" => ["min_range" => 1, "max_range" => 5], "flags" => FILTER_NULL_ON_FAILURE]));
/// try { filter_var("x", FILTER_VALIDATE_INT, FILTER_NULL_ON_FAILURE | FILTER_THROW_ON_FAILURE); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var([1], FILTER_VALIDATE_INT, FILTER_THROW_ON_FAILURE); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var(1, FILTER_VALIDATE_INT, FILTER_THROW_ON_FAILURE | FILTER_REQUIRE_ARRAY); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var(new stdClass, FILTER_DEFAULT, FILTER_THROW_ON_FAILURE); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump(filter_var(new stdClass), filter_var("x", 12345));
/// ```
/// ```expect
/// NULL
/// int(-1)
/// NULL
/// filter_var(): Argument #3 ($options) cannot use both FILTER_NULL_ON_FAILURE and FILTER_THROW_ON_FAILURE
/// filter validation failed: not a scalar value (got an array)
/// filter validation failed: not an array (got int)
/// filter validation failed: object of type stdClass has no __toString() method
///
/// Warning: filter_var(): Unknown filter with ID 12345 in %s on line 7
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn filter_var(
    req: &mut Request,
    value: Value,
    #[php(default = FILTER_DEFAULT)] filter: i64,
    #[php(default = 0)] options: Union<PhpArray, i64>,
) -> Result<Owner<Value>, Thrown> {
    if !id_exists(filter) {
        unknown_filter(req, filter);
        return Ok(Owner::bool(false));
    }
    let value = Owner::take(req, value);
    filter_call(
        req,
        "filter_var",
        value,
        filter,
        options,
        FILTER_REQUIRE_SCALAR,
        3,
    )
}

/// `filter_input_array(int $type, array|int $options = FILTER_DEFAULT,
/// bool $add_empty = true): array|false|null`: `filter_var_array` over
/// an input array, `null` for one the host never filled.
///
/// ```php
/// var_dump(filter_input_array(INPUT_GET), filter_input_array(INPUT_SERVER, ["PHP_SELF" => FILTER_VALIDATE_INT, "nope" => FILTER_DEFAULT]), filter_input_array(INPUT_SERVER, ["nope" => FILTER_DEFAULT], false));
/// ```
/// ```expect
/// NULL
/// array(2) {
///   ["PHP_SELF"]=>
///   bool(false)
///   ["nope"]=>
///   NULL
/// }
/// array(0) {
/// }
/// ```
#[php_function]
fn filter_input_array(
    req: &mut Request,
    r#type: i64,
    #[php(default = FILTER_DEFAULT)] options: Union<PhpArray, i64>,
    #[php(default = true)] add_empty: bool,
) -> Result<Option<OrFalse<Owner<PhpArray>>>, Thrown> {
    if let Union::B(filter) = options
        && !id_exists(filter)
    {
        unknown_filter(req, filter);
        return Ok(Some(OrFalse::False));
    }
    let Some(array) = storage(req, r#type)? else {
        return Ok(None);
    };
    let filtered = array_handler(req, "filter_input_array", *array, options, add_empty);
    array.give_up(req);
    Ok(Some(OrFalse::Value(filtered?)))
}

/// `filter_var_array(array $array, array|int $options = FILTER_DEFAULT,
/// bool $add_empty = true): array|false|null`: every element through one
/// filter, or each key the definition names through its own, a missing
/// one `null` unless `$add_empty` is false.
///
/// ```php
/// $data = ["id" => "42", "tags" => ["a", "7"], "age" => "x", "n" => "3"];
/// var_dump(filter_var_array($data, ["id" => FILTER_VALIDATE_INT, "tags" => ["filter" => FILTER_VALIDATE_INT, "flags" => FILTER_FORCE_ARRAY], "age" => ["filter" => FILTER_VALIDATE_INT, "options" => ["default" => 0]], "missing" => FILTER_DEFAULT]));
/// var_dump(filter_var_array(["a" => "1", "b" => ["2"]], FILTER_VALIDATE_INT), filter_var_array(["a" => 1], ["a" => 999]));
/// try { filter_var_array([], [1 => FILTER_DEFAULT]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { filter_var_array([], ["" => FILTER_DEFAULT]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(4) {
///   ["id"]=>
///   int(42)
///   ["tags"]=>
///   array(2) {
///     [0]=>
///     bool(false)
///     [1]=>
///     int(7)
///   }
///   ["age"]=>
///   int(0)
///   ["missing"]=>
///   NULL
/// }
///
/// Warning: filter_var_array(): Unknown filter with ID 999 in %s on line 4
/// array(2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   array(1) {
///     [0]=>
///     int(2)
///   }
/// }
/// array(1) {
///   ["a"]=>
///   string(1) "1"
/// }
/// filter_var_array(): Argument #2 ($options) must contain only string keys
/// filter_var_array(): Argument #2 ($options) cannot contain empty keys
/// ```
#[php_function]
fn filter_var_array(
    req: &mut Request,
    array: PhpArray,
    #[php(default = FILTER_DEFAULT)] options: Union<PhpArray, i64>,
    #[php(default = true)] add_empty: bool,
) -> Result<Option<OrFalse<Owner<PhpArray>>>, Thrown> {
    if let Union::B(filter) = options
        && !id_exists(filter)
    {
        unknown_filter(req, filter);
        return Ok(Some(OrFalse::False));
    }
    let filtered = array_handler(req, "filter_var_array", array, options, add_empty)?;
    Ok(Some(OrFalse::Value(filtered)))
}

/// `filter_list(): array`: the names of `filter_list`, in its order.
///
/// ```php
/// echo implode(",", filter_list()), "\n";
/// ```
/// ```expect
/// int,boolean,float,validate_regexp,validate_domain,validate_url,validate_email,validate_ip,validate_mac,string,stripped,encoded,special_chars,full_special_chars,unsafe_raw,email,url,number_int,number_float,add_slashes,callback
/// ```
#[php_function]
fn filter_list(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for (name, _, _) in FILTERS {
        let name = string_value(req, name.as_bytes())?;
        array.append(req, name)?;
    }
    Ok(array)
}

/// `filter_id(string $name): int|false`: the id of the filter so named,
/// compared as `strcmp` compares.
///
/// ```php
/// var_dump(filter_id("int"), filter_id("stripped"), filter_id("callback"), filter_id("INT"), filter_id(""));
/// ```
/// ```expect
/// int(257)
/// int(513)
/// int(1024)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn filter_id(name: PhpString) -> OrFalse<i64> {
    let name = name.as_bytes().split(|&b| b == 0).next().unwrap_or(&[]);
    FILTERS
        .iter()
        .find(|(n, _, _)| n.as_bytes() == name)
        .map_or(OrFalse::False, |(_, id, _)| OrFalse::Value(*id))
}
