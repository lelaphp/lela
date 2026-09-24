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
//! The input arrays are what the CLI host fills, `php_sapi_filter` passed
//! over the variables php registers: `INPUT_SERVER` the five entries
//! `cli_register_variables` names off the served file, `INPUT_ENV` the
//! environment, and `INPUT_GET`, `INPUT_POST` and `INPUT_COOKIE` never
//! initialised, so `filter_input` over them answers the missing
//! variable's `null`.
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

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::coerce::{coerce_param, numeric_string, to_long, to_string, value_name};
use php_ext::decl::{
    Attribute, AttributeArgument, ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr,
    EnumBacking, FunctionDecl, IniDecl, Name, Parameter, Type,
};
use php_ext::raising::{
    Level, Thrown, arg_count_error, arg_null_deprecated, arg_type_error, arg_value_error, docref,
    new_throwable, throw, throw_value,
};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpString, Value, ValueRef};

/// Nothing is held across a call: `IF_G`'s input arrays are the host's,
/// read where they are asked for.
#[derive(Default)]
struct State {}

// `filter_private.h`: the flags.
const REQUIRE_ARRAY: i64 = 0x100_0000;
const REQUIRE_SCALAR: i64 = 0x200_0000;
const FORCE_ARRAY: i64 = 0x400_0000;
const NULL_ON_FAILURE: i64 = 0x800_0000;
const THROW_ON_FAILURE: i64 = 0x1000_0000;
const FLAG_ALLOW_OCTAL: i64 = 0x0001;
const FLAG_ALLOW_HEX: i64 = 0x0002;
const FLAG_STRIP_LOW: i64 = 0x0004;
const FLAG_STRIP_HIGH: i64 = 0x0008;
const FLAG_ENCODE_LOW: i64 = 0x0010;
const FLAG_ENCODE_HIGH: i64 = 0x0020;
const FLAG_ENCODE_AMP: i64 = 0x0040;
const FLAG_NO_ENCODE_QUOTES: i64 = 0x0080;
const FLAG_EMPTY_STRING_NULL: i64 = 0x0100;
const FLAG_STRIP_BACKTICK: i64 = 0x0200;
const FLAG_ALLOW_FRACTION: i64 = 0x1000;
const FLAG_ALLOW_THOUSAND: i64 = 0x2000;
const FLAG_ALLOW_SCIENTIFIC: i64 = 0x4000;
const FLAG_PATH_REQUIRED: i64 = 0x04_0000;
const FLAG_QUERY_REQUIRED: i64 = 0x08_0000;
const FLAG_IPV4: i64 = 0x10_0000;
const FLAG_IPV6: i64 = 0x20_0000;
const FLAG_NO_RES_RANGE: i64 = 0x40_0000;
const FLAG_NO_PRIV_RANGE: i64 = 0x80_0000;
const FLAG_GLOBAL_RANGE: i64 = 0x2000_0000;
const FLAG_HOSTNAME: i64 = 0x10_0000;
const FLAG_EMAIL_UNICODE: i64 = 0x10_0000;

// The filter ids.
const VALIDATE_INT: i64 = 0x0101;
const VALIDATE_BOOL: i64 = 0x0102;
const VALIDATE_FLOAT: i64 = 0x0103;
const VALIDATE_REGEXP: i64 = 0x0110;
const VALIDATE_URL: i64 = 0x0111;
const VALIDATE_EMAIL: i64 = 0x0112;
const VALIDATE_IP: i64 = 0x0113;
const VALIDATE_MAC: i64 = 0x0114;
const VALIDATE_DOMAIN: i64 = 0x0115;
const DEFAULT: i64 = 0x0204;
const SANITIZE_STRING: i64 = 0x0201;
const SANITIZE_ENCODED: i64 = 0x0202;
const SANITIZE_SPECIAL_CHARS: i64 = 0x0203;
const SANITIZE_EMAIL: i64 = 0x0205;
const SANITIZE_URL: i64 = 0x0206;
const SANITIZE_NUMBER_INT: i64 = 0x0207;
const SANITIZE_NUMBER_FLOAT: i64 = 0x0208;
const SANITIZE_FULL_SPECIAL_CHARS: i64 = 0x020a;
const SANITIZE_ADD_SLASHES: i64 = 0x020b;
const CALLBACK: i64 = 0x0400;

// `PARSE_POST`, `PARSE_GET`, `PARSE_COOKIE`, `PARSE_ENV` and
// `PARSE_SERVER`, the `INPUT_*` constants.
const INPUT_POST: i64 = 0;
const INPUT_GET: i64 = 1;
const INPUT_COOKIE: i64 = 2;
const INPUT_ENV: i64 = 4;
const INPUT_SERVER: i64 = 5;

/// What one filter made of the string: its value, owned, or
/// `RETURN_VALIDATION_FAILED`, which the flags turn into `false`, `null`
/// or the throwable.
enum Outcome {
    Value(Value),
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
    ("int", VALIDATE_INT, logical::int),
    ("boolean", VALIDATE_BOOL, logical::boolean),
    ("float", VALIDATE_FLOAT, logical::float),
    ("validate_regexp", VALIDATE_REGEXP, logical::regexp),
    ("validate_domain", VALIDATE_DOMAIN, logical::domain),
    ("validate_url", VALIDATE_URL, logical::url),
    ("validate_email", VALIDATE_EMAIL, logical::email),
    ("validate_ip", VALIDATE_IP, logical::ip),
    ("validate_mac", VALIDATE_MAC, logical::mac),
    ("string", SANITIZE_STRING, sanitizing::string),
    ("stripped", SANITIZE_STRING, sanitizing::string),
    ("encoded", SANITIZE_ENCODED, sanitizing::encoded),
    (
        "special_chars",
        SANITIZE_SPECIAL_CHARS,
        sanitizing::special_chars,
    ),
    (
        "full_special_chars",
        SANITIZE_FULL_SPECIAL_CHARS,
        sanitizing::full_special_chars,
    ),
    ("unsafe_raw", DEFAULT, sanitizing::unsafe_raw),
    ("email", SANITIZE_EMAIL, sanitizing::email),
    ("url", SANITIZE_URL, sanitizing::url),
    ("number_int", SANITIZE_NUMBER_INT, sanitizing::number_int),
    (
        "number_float",
        SANITIZE_NUMBER_FLOAT,
        sanitizing::number_float,
    ),
    ("add_slashes", SANITIZE_ADD_SLASHES, sanitizing::add_slashes),
    ("callback", CALLBACK, sanitizing::callback),
];

/// `PHP_FILTER_ID_EXISTS`.
const fn id_exists(id: i64) -> bool {
    matches!(id, 0x0200..=0x020b | 0x0100..=0x0115 | CALLBACK)
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
/// declares, its constants in `register_filter_symbols`' order, the two
/// throwable classes and the two ini entries.
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
    let int = |name: &[u8]| parameter(name, Type::Int, None);
    let string = |name: &[u8]| parameter(name, Type::String, None);
    let constant = || {
        Some(ConstExpr::Constant(Name(
            b"FILTER_DEFAULT".as_slice().into(),
        )))
    };
    let options = |default: Option<ConstExpr>| {
        parameter(
            b"options",
            Type::Union(vec![Type::Array, Type::Int]),
            default,
        )
    };
    let add_empty = || parameter(b"add_empty", Type::Bool, Some(ConstExpr::Bool(true)));
    let array_or = || Type::Union(vec![Type::Array, Type::False, Type::Null]);
    let natives: Vec<(FunctionDecl, Entry)> = vec![
        (
            declared(
                b"filter_has_var",
                vec![int(b"input_type"), string(b"var_name")],
                Type::Bool,
            ),
            filter_has_var,
        ),
        (
            declared(
                b"filter_input",
                vec![
                    int(b"type"),
                    string(b"var_name"),
                    parameter(b"filter", Type::Int, constant()),
                    options(Some(ConstExpr::Long(0))),
                ],
                Type::Mixed,
            ),
            filter_input,
        ),
        (
            declared(
                b"filter_var",
                vec![
                    parameter(b"value", Type::Mixed, None),
                    parameter(b"filter", Type::Int, constant()),
                    options(Some(ConstExpr::Long(0))),
                ],
                Type::Mixed,
            ),
            filter_var,
        ),
        (
            declared(
                b"filter_input_array",
                vec![int(b"type"), options(constant()), add_empty()],
                array_or(),
            ),
            filter_input_array,
        ),
        (
            declared(
                b"filter_var_array",
                vec![
                    parameter(b"array", Type::Array, None),
                    options(constant()),
                    add_empty(),
                ],
                array_or(),
            ),
            filter_var_array,
        ),
        (
            declared(b"filter_list", Vec::new(), Type::Array),
            filter_list,
        ),
        (
            declared(
                b"filter_id",
                vec![string(b"name")],
                Type::Union(vec![Type::Int, Type::False]),
            ),
            filter_id,
        ),
    ];
    ExtensionBuilder::default()
        .constants(constants())
        .functions(natives)
        .classes(vec![
            (class(b"Filter\\FilterException", b"Exception"), Vec::new()),
            (
                class(b"Filter\\FilterFailedException", b"Filter\\FilterException"),
                Vec::new(),
            ),
        ])
        .ini(vec![
            ini_entry(b"filter.default", b"unsafe_raw"),
            ini_entry(b"filter.default_flags", b""),
        ])
        .request_state::<State>()
        .info(filter_info)
        .build("filter")
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
    let shown = |req: &Request, name: &[u8]| match req.ini_get(name) {
        Some(value) if !value.is_empty() => String::from_utf8_lossy(value).into_owned(),
        _ => String::from("no value"),
    };
    let default = shown(req, b"filter.default");
    let flags = shown(req, b"filter.default_flags");
    req.info_table(&[
        &["Directive", "Local Value", "Master Value"],
        &["filter.default", &default, &default],
        &["filter.default_flags", &flags, &flags],
    ]);
}

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// `STD_PHP_INI_ENTRY(..., PHP_INI_SYSTEM|PHP_INI_PERDIR, ...)`.
fn ini_entry(name: &[u8], default: &[u8]) -> IniDecl {
    IniDecl {
        name: Name(name.into()),
        default: default.into(),
        user: false,
        per_dir: true,
        system: true,
    }
}

/// The constants `register_filter_symbols` registers, in its order,
/// `FILTER_SANITIZE_STRING` and `FILTER_SANITIZE_STRIPPED` under the
/// stub's `#[\Deprecated]`.
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
fn constants() -> Vec<ConstDecl> {
    let rows: [(&[u8], i64); 56] = [
        (b"INPUT_POST", INPUT_POST),
        (b"INPUT_GET", INPUT_GET),
        (b"INPUT_COOKIE", INPUT_COOKIE),
        (b"INPUT_ENV", INPUT_ENV),
        (b"INPUT_SERVER", INPUT_SERVER),
        (b"FILTER_FLAG_NONE", 0),
        (b"FILTER_REQUIRE_SCALAR", REQUIRE_SCALAR),
        (b"FILTER_REQUIRE_ARRAY", REQUIRE_ARRAY),
        (b"FILTER_FORCE_ARRAY", FORCE_ARRAY),
        (b"FILTER_NULL_ON_FAILURE", NULL_ON_FAILURE),
        (b"FILTER_THROW_ON_FAILURE", THROW_ON_FAILURE),
        (b"FILTER_VALIDATE_INT", VALIDATE_INT),
        (b"FILTER_VALIDATE_BOOLEAN", VALIDATE_BOOL),
        (b"FILTER_VALIDATE_BOOL", VALIDATE_BOOL),
        (b"FILTER_VALIDATE_FLOAT", VALIDATE_FLOAT),
        (b"FILTER_VALIDATE_REGEXP", VALIDATE_REGEXP),
        (b"FILTER_VALIDATE_DOMAIN", VALIDATE_DOMAIN),
        (b"FILTER_VALIDATE_URL", VALIDATE_URL),
        (b"FILTER_VALIDATE_EMAIL", VALIDATE_EMAIL),
        (b"FILTER_VALIDATE_IP", VALIDATE_IP),
        (b"FILTER_VALIDATE_MAC", VALIDATE_MAC),
        (b"FILTER_DEFAULT", DEFAULT),
        (b"FILTER_UNSAFE_RAW", DEFAULT),
        (b"FILTER_SANITIZE_STRING", SANITIZE_STRING),
        (b"FILTER_SANITIZE_STRIPPED", SANITIZE_STRING),
        (b"FILTER_SANITIZE_ENCODED", SANITIZE_ENCODED),
        (b"FILTER_SANITIZE_SPECIAL_CHARS", SANITIZE_SPECIAL_CHARS),
        (
            b"FILTER_SANITIZE_FULL_SPECIAL_CHARS",
            SANITIZE_FULL_SPECIAL_CHARS,
        ),
        (b"FILTER_SANITIZE_EMAIL", SANITIZE_EMAIL),
        (b"FILTER_SANITIZE_URL", SANITIZE_URL),
        (b"FILTER_SANITIZE_NUMBER_INT", SANITIZE_NUMBER_INT),
        (b"FILTER_SANITIZE_NUMBER_FLOAT", SANITIZE_NUMBER_FLOAT),
        (b"FILTER_SANITIZE_ADD_SLASHES", SANITIZE_ADD_SLASHES),
        (b"FILTER_CALLBACK", CALLBACK),
        (b"FILTER_FLAG_ALLOW_OCTAL", FLAG_ALLOW_OCTAL),
        (b"FILTER_FLAG_ALLOW_HEX", FLAG_ALLOW_HEX),
        (b"FILTER_FLAG_STRIP_LOW", FLAG_STRIP_LOW),
        (b"FILTER_FLAG_STRIP_HIGH", FLAG_STRIP_HIGH),
        (b"FILTER_FLAG_STRIP_BACKTICK", FLAG_STRIP_BACKTICK),
        (b"FILTER_FLAG_ENCODE_LOW", FLAG_ENCODE_LOW),
        (b"FILTER_FLAG_ENCODE_HIGH", FLAG_ENCODE_HIGH),
        (b"FILTER_FLAG_ENCODE_AMP", FLAG_ENCODE_AMP),
        (b"FILTER_FLAG_NO_ENCODE_QUOTES", FLAG_NO_ENCODE_QUOTES),
        (b"FILTER_FLAG_EMPTY_STRING_NULL", FLAG_EMPTY_STRING_NULL),
        (b"FILTER_FLAG_ALLOW_FRACTION", FLAG_ALLOW_FRACTION),
        (b"FILTER_FLAG_ALLOW_THOUSAND", FLAG_ALLOW_THOUSAND),
        (b"FILTER_FLAG_ALLOW_SCIENTIFIC", FLAG_ALLOW_SCIENTIFIC),
        (b"FILTER_FLAG_PATH_REQUIRED", FLAG_PATH_REQUIRED),
        (b"FILTER_FLAG_QUERY_REQUIRED", FLAG_QUERY_REQUIRED),
        (b"FILTER_FLAG_IPV4", FLAG_IPV4),
        (b"FILTER_FLAG_IPV6", FLAG_IPV6),
        (b"FILTER_FLAG_NO_RES_RANGE", FLAG_NO_RES_RANGE),
        (b"FILTER_FLAG_NO_PRIV_RANGE", FLAG_NO_PRIV_RANGE),
        (b"FILTER_FLAG_GLOBAL_RANGE", FLAG_GLOBAL_RANGE),
        (b"FILTER_FLAG_HOSTNAME", FLAG_HOSTNAME),
        (b"FILTER_FLAG_EMAIL_UNICODE", FLAG_EMAIL_UNICODE),
    ];
    rows.into_iter()
        .map(|(name, value)| ConstDecl {
            name: Name(name.into()),
            value: ConstExpr::Long(value),
            attributes: if name.starts_with(b"FILTER_SANITIZE_STR") {
                vec![deprecated_since(b"8.1", b"use htmlspecialchars() instead")]
            } else {
                Vec::new()
            },
        })
        .collect()
}

/// `#[\Deprecated(since: ..., message: ...)]`.
fn deprecated_since(since: &[u8], message: &[u8]) -> Attribute {
    let argument = |name: &[u8], value: &[u8]| AttributeArgument {
        name: Some(Name(name.into())),
        value: ConstExpr::Str(value.into()),
    };
    Attribute {
        name: Name(b"Deprecated".as_slice().into()),
        arguments: vec![argument(b"since", since), argument(b"message", message)],
    }
}

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
fn class(name: &[u8], parent: &[u8]) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind: ClassKind::Class,
        flags: ClassFlags::default(),
        parent: Some(Name(parent.into())),
        interfaces: Vec::new(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// One function as `filter.stub.php` declares it.
fn declared(name: &[u8], parameters: Vec<Parameter>, returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters,
        return_type: Some(returns),
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

fn parameter(name: &[u8], ty: Type, default: Option<ConstExpr>) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default,
        attributes: Vec::new(),
    }
}

// ------------------------------------------------------------ arguments

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
fn arguments_between(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (word, bound) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}

/// The argument at `position` when the call gave one.
fn given(req: &Request, position: u32) -> Option<Value> {
    req.argument(position)
        .filter(|value| !matches!(value.as_ref(), ValueRef::Undef))
}

/// `Z_PARAM_LONG`, `absent` where `Z_PARAM_OPTIONAL` left it.
fn long_argument(req: &mut Request, position: u32, absent: i64) -> Result<i64, Thrown> {
    let Some(value) = given(req, position) else {
        return Ok(absent);
    };
    let coerced = coerce_param(req, &value, &Type::Int, position)?;
    Ok(coerced.deref().as_int().unwrap_or(0))
}

/// `Z_PARAM_BOOL`, `absent` where `Z_PARAM_OPTIONAL` left it.
fn bool_argument(req: &mut Request, position: u32, absent: bool) -> Result<bool, Thrown> {
    let Some(value) = given(req, position) else {
        return Ok(absent);
    };
    let coerced = coerce_param(req, &value, &Type::Bool, position)?;
    Ok(coerced.deref().as_bool().unwrap_or(false))
}

/// `Z_PARAM_STR`.
fn string_argument(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &value, &Type::String, position)?;
    Ok(coerced.deref().as_str_bytes().unwrap_or_default().to_vec())
}

/// `Z_PARAM_ARRAY_HT_OR_LONG` after `Z_PARAM_OPTIONAL`: the array, or
/// the long, `absent` where the call gave none.
fn array_or_long(
    req: &mut Request,
    position: u32,
    absent: i64,
) -> Result<(Option<PhpArray>, i64), Thrown> {
    let Some(value) = given(req, position) else {
        return Ok((None, absent));
    };
    let value = value.deref();
    if let Some(array) = value.as_array() {
        return Ok((Some(*array), 0));
    }
    // The weak long's refusals name the union, as
    // `zend_parse_arg_array_ht_or_long` reports them.
    let refused = match value.as_ref() {
        ValueRef::Null => {
            arg_null_deprecated(req, position, "array|int");
            return Ok((None, 0));
        }
        ValueRef::Str(s) => {
            let text = s.as_bytes().trim_ascii_start();
            let text = text
                .strip_prefix(b"-")
                .or_else(|| text.strip_prefix(b"+"))
                .unwrap_or(text);
            numeric_string(s.as_bytes()).is_none() && !text.first().is_some_and(u8::is_ascii_digit)
        }
        ValueRef::Object(_) | ValueRef::Resource(_) => true,
        _ => false,
    };
    if refused {
        let name = value_name(req, &value);
        return Err(arg_type_error(
            req,
            position,
            &format!("be of type array|int, {name} given"),
        ));
    }
    let coerced = coerce_param(req, &value, &Type::Int, position)?;
    Ok((None, coerced.deref().as_int().unwrap_or(0)))
}

/// `zend_hash_str_find` over an options array: the element under a
/// string key, a view.
fn find(options: Option<&Value>, name: &[u8]) -> Option<Value> {
    let array = *options?.deref().as_array()?;
    array
        .snapshot_refs()
        .into_iter()
        .find_map(|(key, value)| match key {
            Key::Str(s) if s.as_bytes() == name => Some(value),
            _ => None,
        })
}

/// A new string of the request's as a value.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
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
fn options_error(req: &mut Request, native: &str, position: u32, tail: &str) -> Thrown {
    throw(
        req,
        "ValueError",
        &format!("{native}(): Argument #{position} ($options) {tail}"),
    )
}

/// `zend_throw_exception(php_filter_failed_exception_ce, message, 0)`.
fn throw_failed(req: &mut Request, message: &str) -> Thrown {
    match new_throwable(req, "Filter\\FilterFailedException", message) {
        Ok(value) => {
            let thrown = throw_value(req, &value);
            req.release(value);
            thrown
        }
        Err(thrown) => thrown,
    }
}

/// Whether the class, or a class it extends, has `__toString`, php's
/// `ce->__tostring`.
fn has_to_string(req: &mut Request, class: ClassId) -> bool {
    let mut at = Some(class);
    for _ in 0..64 {
        let Some(here) = at else { return false };
        let Some(decl) = req.class_decl(here) else {
            return false;
        };
        if decl
            .methods
            .iter()
            .any(|m| m.function.name.0.eq_ignore_ascii_case(b"__tostring"))
        {
            return true;
        }
        let parent = decl.parent.as_ref().map(|p| p.0.to_vec());
        at = parent.and_then(|p| php_ext::call::lookup_class(req, &p).ok().flatten());
    }
    false
}

// ------------------------------------------------------------ dispatch

/// `php_zval_filter(value, filter, flags, options, charset)`: `value`
/// owned, what it became handed back owned.
fn zval_filter(req: &mut Request, value: Value, filter: i64, run: &Run) -> Result<Value, Thrown> {
    let (name, function) = find_filter(filter);
    let flags = run.flags;
    let filtered = if let Some(object) = value.deref().as_object().copied()
        && !has_to_string(req, object.class())
    {
        // #49274: an object with no `__toString` fails nicely, and
        // #67167 makes the failure `false` or `null`.
        req.release(value);
        if flags & THROW_ON_FAILURE != 0 {
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
        failure(flags)
    } else {
        let string = to_string(req, &value.deref());
        req.release(value);
        let string = string?;
        let bytes = string.as_bytes().to_vec();
        req.release(Value::string(string));
        match function(req, run, &bytes)? {
            Outcome::Value(value) => value,
            Outcome::Failed if flags & THROW_ON_FAILURE != 0 => {
                return Err(throw_failed(
                    req,
                    &format!(
                        "filter validation failed: filter {name} not satisfied by '{}'",
                        c_string(&bytes)
                    ),
                ));
            }
            Outcome::Failed => failure(flags),
        }
    };
    // `handle_default`: the `default` option in place of the failure.
    let failed = if flags & NULL_ON_FAILURE != 0 {
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
        req.addref(&default);
        req.release(filtered);
        return Ok(default);
    }
    Ok(filtered)
}

/// What `RETURN_VALIDATION_FAILED` leaves without the throwable.
fn failure(flags: i64) -> Value {
    if flags & NULL_ON_FAILURE != 0 {
        Value::null()
    } else {
        Value::bool(false)
    }
}

/// `php_zval_filter_recursive`: an array filtered element by element,
/// into a new array with the same keys; one already being walked left as
/// it is.
fn zval_filter_recursive(
    req: &mut Request,
    value: Value,
    filter: i64,
    run: &Run,
) -> Result<Value, Thrown> {
    let Some(array) = value.deref().as_array().copied() else {
        return zval_filter(req, value, filter, run);
    };
    if array.is_recursive() {
        return Ok(value);
    }
    array.protect();
    let built = filter_elements(req, array, filter, run);
    array.unprotect();
    req.release(value);
    built
}

fn filter_elements(
    req: &mut Request,
    array: PhpArray,
    filter: i64,
    run: &Run,
) -> Result<Value, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (key, element) in array.snapshot() {
        req.addref(&element);
        match zval_filter_recursive(req, element, filter, run) {
            Ok(filtered) => {
                if let Key::Str(s) = &key {
                    req.addref(&Value::string(*s));
                }
                out.set(req, key, filtered)?;
            }
            Err(thrown) => {
                req.release(out.to_value());
                return Err(thrown);
            }
        }
    }
    Ok(out.to_value())
}

/// `php_filter_call`: `value` owned, what it became owned, and
/// `options_position` the parameter a refusal names.
fn filter_call(
    req: &mut Request,
    native: &'static str,
    value: Value,
    mut filter: i64,
    arguments: (Option<PhpArray>, i64),
    mut flags: i64,
    options_position: u32,
) -> Result<Value, Thrown> {
    let mut options = None;
    match arguments {
        (None, long) if filter != -1 => {
            flags = long;
            if flags & (REQUIRE_ARRAY | FORCE_ARRAY) == 0 {
                flags |= REQUIRE_SCALAR;
            }
        }
        (None, long) => filter = long,
        (Some(table), _) => {
            let table = Some(table.to_value());
            if let Some(given) = find(table.as_ref(), b"filter") {
                filter = to_long(req, &given.deref())?;
            }
            if let Some(given) = find(table.as_ref(), b"options") {
                let given = given.deref();
                if filter == CALLBACK {
                    options = Some(given);
                    flags = 0;
                } else if given.as_array().is_some() {
                    options = Some(given);
                }
            }
            if let Some(given) = find(table.as_ref(), b"flags") {
                flags = to_long(req, &given.deref())?;
                if flags & (REQUIRE_ARRAY | FORCE_ARRAY) == 0 {
                    flags |= REQUIRE_SCALAR;
                }
            }
        }
    }
    if flags & NULL_ON_FAILURE != 0 && flags & THROW_ON_FAILURE != 0 {
        req.release(value);
        return Err(options_error(req, native, options_position, BOTH_FAILURES));
    }
    let run = Run {
        flags,
        options,
        native,
    };
    if value.deref().as_array().is_some() {
        if flags & REQUIRE_SCALAR != 0 {
            req.release(value);
            if flags & THROW_ON_FAILURE != 0 {
                return Err(throw_failed(
                    req,
                    "filter validation failed: not a scalar value (got an array)",
                ));
            }
            return Ok(failure(flags));
        }
        return zval_filter_recursive(req, value, filter, &run);
    }
    if flags & REQUIRE_ARRAY != 0 {
        let name = value_name(req, &value.deref());
        req.release(value);
        if flags & THROW_ON_FAILURE != 0 {
            return Err(throw_failed(
                req,
                &format!("filter validation failed: not an array (got {name})"),
            ));
        }
        return Ok(failure(flags));
    }
    let filtered = zval_filter(req, value, filter, &run)?;
    if flags & FORCE_ARRAY != 0 {
        let mut wrapped = PhpArray::new(req)?;
        wrapped.append(req, filtered)?;
        return Ok(wrapped.to_value());
    }
    Ok(filtered)
}

/// `php_filter_array_handler(input, op_ht, op_long, return_value,
/// add_empty)`.
fn array_handler(
    req: &mut Request,
    native: &'static str,
    input: PhpArray,
    definition: (Option<PhpArray>, i64),
    add_empty: bool,
) -> Result<Value, Thrown> {
    let (Some(table), _) = definition else {
        let copy = input.to_value();
        req.addref(&copy);
        return filter_call(req, native, copy, -1, definition, REQUIRE_ARRAY, 2);
    };
    let mut out = PhpArray::new(req)?;
    for (key, element) in table.snapshot() {
        let name = match key {
            Key::Str(name) if !name.is_empty() => name,
            Key::Str(_) => {
                req.release(out.to_value());
                return Err(options_error(req, native, 2, "cannot contain empty keys"));
            }
            Key::Int(_) => {
                req.release(out.to_value());
                return Err(arg_type_error(req, 2, "contain only string keys"));
            }
        };
        let Some(found) = lookup(input, name.as_bytes(), false) else {
            if add_empty {
                req.addref(&Value::string(name));
                out.set(req, Key::Str(name), Value::null())?;
            }
            continue;
        };
        let element = element.deref();
        let arguments = if let Some(array) = element.as_array() {
            (Some(*array), 0)
        } else {
            let id = to_long(req, &element)?;
            if !id_exists(id) {
                docref(req, Level::Warning, &format!("Unknown filter with ID {id}"));
            }
            (None, id)
        };
        let found = found.deref();
        req.addref(&found);
        match filter_call(req, native, found, -1, arguments, REQUIRE_SCALAR, 2) {
            Ok(filtered) => {
                req.addref(&Value::string(name));
                out.set(req, Key::Str(name), filtered)?;
            }
            Err(thrown) => {
                req.release(out.to_value());
                return Err(thrown);
            }
        }
    }
    Ok(out.to_value())
}

/// `php_filter_get_storage`: the input array a type names, `None` where
/// the host never initialised it, and php's `ValueError` for a type that
/// is not one. The array is the caller's, one owner.
fn storage(req: &mut Request, input: i64) -> Result<Option<PhpArray>, Thrown> {
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = match input {
        INPUT_GET | INPUT_POST | INPUT_COOKIE => {
            let name: &[u8] = match input {
                INPUT_GET => b"_GET",
                INPUT_POST => b"_POST",
                _ => b"_COOKIE",
            };
            return match req.superglobal(name).copied() {
                Some(array) => Ok(Some(array.copy(req)?)),
                None => Ok(None),
            };
        }
        INPUT_ENV => req.env_pairs().to_vec(),
        INPUT_SERVER => {
            if let Some(array) = req.superglobal(b"_SERVER").copied() {
                return Ok(Some(array.copy(req)?));
            }
            // `cli_register_variables`: what php registers off the
            // script, each through `php_sapi_filter` into the raw array.
            let script = req.script_name().map(<[u8]>::to_vec).unwrap_or_default();
            let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = [
                b"PHP_SELF".as_slice(),
                b"SCRIPT_NAME",
                b"SCRIPT_FILENAME",
                b"PATH_TRANSLATED",
            ]
            .iter()
            .map(|name| (name.to_vec(), script.clone()))
            .collect();
            pairs.push((b"DOCUMENT_ROOT".to_vec(), Vec::new()));
            pairs
        }
        _ => return Err(arg_value_error(req, 1, "be an INPUT_* constant")),
    };
    let mut array = PhpArray::new(req)?;
    for (name, value) in pairs {
        let name = PhpString::new(req, &name)?;
        let value = string_value(req, &value)?;
        array.set(req, Key::from_string(name), value)?;
    }
    Ok(Some(array))
}

/// `zend_hash_find` by a variable name, a view: a string key alone, or
/// with `numeric` the integer key the name is, as `zend_symtable_find`
/// reads it.
fn lookup(array: PhpArray, name: &[u8], numeric: bool) -> Option<Value> {
    array
        .snapshot()
        .into_iter()
        .find_map(|(key, value)| match key {
            Key::Str(s) if s.as_bytes() == name => Some(value),
            Key::Int(n) if numeric && n.to_string().as_bytes() == name => Some(value),
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
fn filter_has_var(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 2, 2)?;
    let input = long_argument(req, 1, 0)?;
    let name = string_argument(req, 2)?;
    let Some(array) = storage(req, input)? else {
        return Ok(Value::bool(false));
    };
    let found = lookup(array, &name, false).is_some();
    req.release(array.to_value());
    Ok(Value::bool(found))
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
fn filter_input(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 2, 4)?;
    let input = long_argument(req, 1, 0)?;
    let name = string_argument(req, 2)?;
    let filter = long_argument(req, 3, DEFAULT)?;
    let arguments = array_or_long(req, 4, 0)?;
    if !id_exists(filter) {
        docref(
            req,
            Level::Warning,
            &format!("Unknown filter with ID {filter}"),
        );
        return Ok(Value::bool(false));
    }
    let array = storage(req, input)?;
    let found = array.and_then(|array| lookup(array, &name, false).map(|v| v.deref()));
    if let Some(found) = &found {
        req.addref(found);
    }
    if let Some(array) = array {
        req.release(array.to_value());
    }
    if let Some(found) = found {
        return filter_call(
            req,
            "filter_input",
            found,
            filter,
            arguments,
            REQUIRE_SCALAR,
            4,
        );
    }
    let table = arguments.0.map(PhpArray::to_value);
    let flags = match (&table, find(table.as_ref(), b"flags")) {
        (None, _) => arguments.1,
        (Some(_), Some(given)) => to_long(req, &given.deref())?,
        (Some(_), None) => 0,
    };
    if flags & NULL_ON_FAILURE != 0 && flags & THROW_ON_FAILURE != 0 {
        return Err(options_error(req, "filter_input", 4, BOTH_FAILURES));
    }
    if flags & THROW_ON_FAILURE != 0 {
        return Err(throw_failed(
            req,
            &format!("input value '{}' not found", c_string(&name)),
        ));
    }
    let options = find(table.as_ref(), b"options").map(|o| o.deref());
    if options.as_ref().is_some_and(|o| o.as_array().is_some())
        && let Some(default) = find(options.as_ref(), b"default")
    {
        let default = default.deref();
        req.addref(&default);
        return Ok(default);
    }
    // Inverted on purpose, as `filter.c` says: a missing variable is
    // `false` under the flag that makes a failure `null`.
    Ok(if flags & NULL_ON_FAILURE != 0 {
        Value::bool(false)
    } else {
        Value::null()
    })
}

/// `filter_var(mixed $value, int $filter = FILTER_DEFAULT, array|int
/// $options = 0): mixed`: the value converted to a string and run
/// through one filter; an array is refused unless the flags ask for one,
/// and then filtered element by element.
///
/// ```php
/// try { filter_var_array([], ""); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(filter_var("  42 ", FILTER_VALIDATE_INT), filter_var("abc"), filter_var(1.5), filter_var(true), filter_var(null), filter_var([1], FILTER_VALIDATE_INT), filter_var(["1", ["x", "3"]], FILTER_VALIDATE_INT, FILTER_REQUIRE_ARRAY), filter_var("5", FILTER_VALIDATE_INT, FILTER_FORCE_ARRAY), filter_var("5", FILTER_VALIDATE_INT, FILTER_REQUIRE_ARRAY));
/// ```
/// ```expect
/// filter_var_array(): Argument #2 ($options) must be of type array|int, string given
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
fn filter_var(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 3)?;
    let value = req.argument(1).unwrap_or_default().deref();
    let filter = long_argument(req, 2, DEFAULT)?;
    let arguments = array_or_long(req, 3, 0)?;
    if !id_exists(filter) {
        docref(
            req,
            Level::Warning,
            &format!("Unknown filter with ID {filter}"),
        );
        return Ok(Value::bool(false));
    }
    req.addref(&value);
    filter_call(
        req,
        "filter_var",
        value,
        filter,
        arguments,
        REQUIRE_SCALAR,
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
fn filter_input_array(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 3)?;
    let input = long_argument(req, 1, 0)?;
    let definition = array_or_long(req, 2, DEFAULT)?;
    let add_empty = bool_argument(req, 3, true)?;
    if definition.0.is_none() && !id_exists(definition.1) {
        docref(
            req,
            Level::Warning,
            &format!("Unknown filter with ID {}", definition.1),
        );
        return Ok(Value::bool(false));
    }
    let Some(array) = storage(req, input)? else {
        return Ok(Value::null());
    };
    let filtered = array_handler(req, "filter_input_array", array, definition, add_empty);
    req.release(array.to_value());
    filtered
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
fn filter_var_array(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 3)?;
    let value = req.argument(1).unwrap_or_default();
    let coerced = coerce_param(req, &value, &Type::Array, 1)?;
    let array = coerced.deref().as_array().copied();
    let definition = array_or_long(req, 2, DEFAULT)?;
    let add_empty = bool_argument(req, 3, true)?;
    if definition.0.is_none() && !id_exists(definition.1) {
        docref(
            req,
            Level::Warning,
            &format!("Unknown filter with ID {}", definition.1),
        );
        return Ok(Value::bool(false));
    }
    let Some(array) = array else {
        return Ok(Value::null());
    };
    array_handler(req, "filter_var_array", array, definition, add_empty)
}

/// `filter_list(): array`: the names of `filter_list`, in its order.
///
/// ```php
/// echo implode(",", filter_list()), "\n";
/// ```
/// ```expect
/// int,boolean,float,validate_regexp,validate_domain,validate_url,validate_email,validate_ip,validate_mac,string,stripped,encoded,special_chars,full_special_chars,unsafe_raw,email,url,number_int,number_float,add_slashes,callback
/// ```
fn filter_list(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let mut array = PhpArray::new(req)?;
    for (name, _, _) in FILTERS {
        let name = string_value(req, name.as_bytes())?;
        array.append(req, name)?;
    }
    Ok(array.to_value())
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
fn filter_id(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 1)?;
    let name = string_argument(req, 1)?;
    let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
    Ok(FILTERS
        .iter()
        .find(|(n, _, _)| n.as_bytes() == name)
        .map_or(Value::bool(false), |(_, id, _)| Value::int(*id)))
}
