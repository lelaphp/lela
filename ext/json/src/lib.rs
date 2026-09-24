//! The `json` extension: `php-src/ext/json` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/json/tests`, through `xtask phpt --subset`.
//!
//! Declared as `json.stub.php` at the tag declares it and `php -n --re
//! json` renders it (design §12, `xtask crosscheck`): the twenty-nine
//! `JSON_*` constants `PHP_MINIT_FUNCTION(json)` registers, in its
//! order, the five functions, the `JsonSerializable` interface whose
//! `jsonSerialize` carries the tentative `mixed`, and `JsonException`
//! under `Exception`.
//!
//! The bodies are `json.c`'s: `encode` is `php_json_encode_zval` over
//! `json_encoder.c` and `decode` is `php_json_parser` over
//! `json_parser.y` and `json_scanner.re`, each with what it copies
//! exactly written at its head. What they leave behind is the request
//! state's `error_code`, `JSON_G(error_code)`, which `json_last_error`
//! and `json_last_error_msg` answer and `JSON_THROW_ON_ERROR` raises as
//! `JsonException` instead.
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

mod decode;
mod encode;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::coerce::coerce_param;
use php_ext::decl::{
    ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr, EnumBacking, FunctionDecl, MethodDecl,
    Name, Parameter, Type, Visibility,
};
use php_ext::raising::{Thrown, arg_count_error, arg_value_error, new_throwable, throw_value};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

/// `json_encode()` options, `php_json.h`.
const HEX_TAG: i64 = 1 << 0;
const HEX_AMP: i64 = 1 << 1;
const HEX_APOS: i64 = 1 << 2;
const HEX_QUOT: i64 = 1 << 3;
const FORCE_OBJECT: i64 = 1 << 4;
const NUMERIC_CHECK: i64 = 1 << 5;
const UNESCAPED_SLASHES: i64 = 1 << 6;
const PRETTY_PRINT: i64 = 1 << 7;
const UNESCAPED_UNICODE: i64 = 1 << 8;
const PARTIAL_OUTPUT_ON_ERROR: i64 = 1 << 9;
const PRESERVE_ZERO_FRACTION: i64 = 1 << 10;
const UNESCAPED_LINE_TERMINATORS: i64 = 1 << 11;

/// `json_decode()` options.
const OBJECT_AS_ARRAY: i64 = 1 << 0;
const BIGINT_AS_STRING: i64 = 1 << 1;

/// What more than one of the three reads.
const INVALID_UTF8_IGNORE: i64 = 1 << 20;
const INVALID_UTF8_SUBSTITUTE: i64 = 1 << 21;
const THROW_ON_ERROR: i64 = 1 << 22;

/// `php_json_error_code`, in its order.
const ERROR_NONE: i64 = 0;
const ERROR_DEPTH: i64 = 1;
const ERROR_STATE_MISMATCH: i64 = 2;
const ERROR_CTRL_CHAR: i64 = 3;
const ERROR_SYNTAX: i64 = 4;
const ERROR_UTF8: i64 = 5;
const ERROR_RECURSION: i64 = 6;
const ERROR_INF_OR_NAN: i64 = 7;
const ERROR_UNSUPPORTED_TYPE: i64 = 8;
const ERROR_INVALID_PROPERTY_NAME: i64 = 9;
const ERROR_UTF16: i64 = 10;
const ERROR_NON_BACKED_ENUM: i64 = 11;

/// `PHP_JSON_PARSER_DEFAULT_DEPTH`.
const DEFAULT_DEPTH: i64 = 512;

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
/// `JSON_G(error_code)`: what the last `json_encode` or `json_decode`
/// of the request left, `PHP_JSON_ERROR_NONE` until one runs.
#[derive(Default)]
struct State {
    error_code: i64,
}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('json'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    let int = |name: &[u8], ty: Type, default: i64| Parameter {
        default: Some(ConstExpr::Long(default)),
        ..parameter(name, ty)
    };
    ExtensionBuilder::default()
        .constants(constants())
        .functions(vec![
            (
                declared(
                    b"json_encode",
                    &[
                        parameter(b"value", Type::Mixed),
                        int(b"flags", Type::Int, 0),
                        int(b"depth", Type::Int, DEFAULT_DEPTH),
                    ],
                    Type::Union(vec![Type::String, Type::False]),
                ),
                json_encode as Entry,
            ),
            (
                declared(
                    b"json_decode",
                    &[
                        parameter(b"json", Type::String),
                        Parameter {
                            default: Some(ConstExpr::Null),
                            ..parameter(b"associative", Type::Nullable(Box::new(Type::Bool)))
                        },
                        int(b"depth", Type::Int, DEFAULT_DEPTH),
                        int(b"flags", Type::Int, 0),
                    ],
                    Type::Mixed,
                ),
                json_decode as Entry,
            ),
            (
                declared(
                    b"json_validate",
                    &[
                        parameter(b"json", Type::String),
                        int(b"depth", Type::Int, DEFAULT_DEPTH),
                        int(b"flags", Type::Int, 0),
                    ],
                    Type::Bool,
                ),
                json_validate as Entry,
            ),
            (
                declared(b"json_last_error", &[], Type::Int),
                json_last_error as Entry,
            ),
            (
                declared(b"json_last_error_msg", &[], Type::String),
                json_last_error_msg as Entry,
            ),
        ])
        .classes(vec![
            (json_serializable(), Vec::new()),
            (
                class(b"JsonException", ClassKind::Class, Some(b"Exception")),
                Vec::new(),
            ),
        ])
        .request_state::<State>()
        .info(json_info)
        .build("json")
}

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The constants `PHP_MINIT_FUNCTION(json)` registers, in its order:
/// the encoder's flags, the decoder's two, the three both read, and the
/// twelve error codes of `php_json_error_code`.
///
/// ```php
/// var_dump(JSON_HEX_TAG, JSON_PRETTY_PRINT, JSON_OBJECT_AS_ARRAY, JSON_THROW_ON_ERROR, JSON_ERROR_NONE, JSON_ERROR_NON_BACKED_ENUM);
/// ```
/// ```expect
/// int(1)
/// int(128)
/// int(1)
/// int(4194304)
/// int(0)
/// int(11)
/// ```
fn constants() -> Vec<ConstDecl> {
    let rows: [(&[u8], i64); 29] = [
        (b"JSON_HEX_TAG", HEX_TAG),
        (b"JSON_HEX_AMP", HEX_AMP),
        (b"JSON_HEX_APOS", HEX_APOS),
        (b"JSON_HEX_QUOT", HEX_QUOT),
        (b"JSON_FORCE_OBJECT", FORCE_OBJECT),
        (b"JSON_NUMERIC_CHECK", NUMERIC_CHECK),
        (b"JSON_UNESCAPED_SLASHES", UNESCAPED_SLASHES),
        (b"JSON_PRETTY_PRINT", PRETTY_PRINT),
        (b"JSON_UNESCAPED_UNICODE", UNESCAPED_UNICODE),
        (b"JSON_PARTIAL_OUTPUT_ON_ERROR", PARTIAL_OUTPUT_ON_ERROR),
        (b"JSON_PRESERVE_ZERO_FRACTION", PRESERVE_ZERO_FRACTION),
        (
            b"JSON_UNESCAPED_LINE_TERMINATORS",
            UNESCAPED_LINE_TERMINATORS,
        ),
        (b"JSON_OBJECT_AS_ARRAY", OBJECT_AS_ARRAY),
        (b"JSON_BIGINT_AS_STRING", BIGINT_AS_STRING),
        (b"JSON_INVALID_UTF8_IGNORE", INVALID_UTF8_IGNORE),
        (b"JSON_INVALID_UTF8_SUBSTITUTE", INVALID_UTF8_SUBSTITUTE),
        (b"JSON_THROW_ON_ERROR", THROW_ON_ERROR),
        (b"JSON_ERROR_NONE", ERROR_NONE),
        (b"JSON_ERROR_DEPTH", ERROR_DEPTH),
        (b"JSON_ERROR_STATE_MISMATCH", ERROR_STATE_MISMATCH),
        (b"JSON_ERROR_CTRL_CHAR", ERROR_CTRL_CHAR),
        (b"JSON_ERROR_SYNTAX", ERROR_SYNTAX),
        (b"JSON_ERROR_UTF8", ERROR_UTF8),
        (b"JSON_ERROR_RECURSION", ERROR_RECURSION),
        (b"JSON_ERROR_INF_OR_NAN", ERROR_INF_OR_NAN),
        (b"JSON_ERROR_UNSUPPORTED_TYPE", ERROR_UNSUPPORTED_TYPE),
        (
            b"JSON_ERROR_INVALID_PROPERTY_NAME",
            ERROR_INVALID_PROPERTY_NAME,
        ),
        (b"JSON_ERROR_UTF16", ERROR_UTF16),
        (b"JSON_ERROR_NON_BACKED_ENUM", ERROR_NON_BACKED_ENUM),
    ];
    rows.into_iter()
        .map(|(name, value)| ConstDecl {
            name: Name(name.into()),
            value: ConstExpr::Long(value),
            attributes: Vec::new(),
        })
        .collect()
}

/// `interface JsonSerializable { public function jsonSerialize():
/// mixed; }`, the return `@tentative-return-type` in the stub, so the
/// linker holds a user implementation to it with the deprecation and
/// not the fatal (design §6, §12).
///
/// ```php
/// echo new ReflectionClass('JsonSerializable');
/// ```
/// ```expect
/// Interface [ <internal:json> interface JsonSerializable ] {
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
///   - Methods [1] {
///     Method [ <internal:json> abstract public method jsonSerialize ] {
///
///       - Parameters [0] {
///       }
///       - Tentative return [ mixed ]
///     }
///   }
/// }
/// ```
fn json_serializable() -> ClassDecl {
    ClassDecl {
        methods: vec![MethodDecl {
            function: declared(b"jsonSerialize", &[], Type::Mixed),
            visibility: Visibility::Public,
            is_instance: true,
            is_abstract: true,
            is_final: false,
            tentative_return: true,
        }],
        ..class(b"JsonSerializable", ClassKind::Interface, None)
    }
}

fn class(name: &[u8], kind: ClassKind, parent: Option<&[u8]>) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: ClassFlags::default(),
        parent: parent.map(|p| Name(p.into())),
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

/// One function as `json.stub.php` declares it.
fn declared(name: &[u8], parameters: &[Parameter], returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: parameters.to_vec(),
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

fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check:
/// `zend_wrong_parameters_count_error`'s sentence for a count outside
/// the range.
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

/// One `Z_PARAM_STRING`: `coerce_param` over `string`, whose `null` is
/// the 8.1 deprecation and the empty string under the weak rule and the
/// `TypeError` under `strict_types`.
///
/// ```php
/// declare(strict_types=1);
/// try { json_decode(null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// json_decode(): Argument #1 ($json) must be of type string, null given
/// ```
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    match coerced.deref().as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// One `Z_PARAM_OPTIONAL Z_PARAM_LONG`: the argument coerced as php
/// coerces an int parameter, and `absent` where the call gave none.
fn long_argument(req: &mut Request, position: u32, absent: i64) -> Result<i64, Thrown> {
    let Some(given) = req.argument(position) else {
        return Ok(absent);
    };
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.deref().as_int().unwrap_or(0))
}

/// `php_json_get_error_msg`: the twelve messages of `json.c` and
/// `Unknown error` past them.
fn error_message(code: i64) -> &'static str {
    match code {
        ERROR_NONE => "No error",
        ERROR_DEPTH => "Maximum stack depth exceeded",
        ERROR_STATE_MISMATCH => "State mismatch (invalid or malformed JSON)",
        ERROR_CTRL_CHAR => "Control character error, possibly incorrectly encoded",
        ERROR_SYNTAX => "Syntax error",
        ERROR_UTF8 => "Malformed UTF-8 characters, possibly incorrectly encoded",
        ERROR_RECURSION => "Recursion detected",
        ERROR_INF_OR_NAN => "Inf and NaN cannot be JSON encoded",
        ERROR_UNSUPPORTED_TYPE => "Type is not supported",
        ERROR_INVALID_PROPERTY_NAME => "The decoded property name is invalid",
        ERROR_UTF16 => "Single unpaired UTF-16 surrogate in unicode escape",
        ERROR_NON_BACKED_ENUM => "Non-backed enums have no default serialization",
        _ => "Unknown error",
    }
}

/// `zend_throw_exception(php_json_exception_ce, msg, error_code)`: the
/// message `json_last_error_msg` would have answered, under the code
/// `json_last_error` would have, which is what `getCode()` reads.
fn throw_json(req: &mut Request, code: i64) -> Thrown {
    let made = new_throwable(req, "JsonException", error_message(code));
    let value = match made {
        Ok(value) => value,
        Err(thrown) => return thrown,
    };
    if let Some(object) = value.as_object().copied() {
        let written = PhpString::new(req, b"code")
            .and_then(|name| object.set_prop(req, name, Value::int(code)));
        if let Err(thrown) = written {
            req.release(value);
            return thrown;
        }
    }
    let thrown = throw_value(req, &value);
    req.release(value);
    thrown
}

/// The request's `JSON_G(error_code)`, written where php writes it.
fn set_error(req: &mut Request, code: i64) {
    req.state::<State>().error_code = code;
}

/// The two `zend_argument_value_error`s the decoders raise over their
/// depth, whose parameter number differs between them.
fn depth_within(req: &mut Request, depth: i64, position: u32) -> Result<(), Thrown> {
    if depth <= 0 {
        return Err(arg_value_error(req, position, "be greater than 0"));
    }
    if depth > i64::from(i32::MAX) {
        return Err(arg_value_error(
            req,
            position,
            &format!("be less than {}", i32::MAX),
        ));
    }
    Ok(())
}

/// `json_encode(mixed $value, int $flags = 0, int $depth = 512):
/// string|false`, `PHP_FUNCTION(json_encode)` over
/// `php_json_encode_zval`: the text, or `false` with the code in
/// `json_last_error` once anything refused it.
///
/// ```php
/// var_dump(json_encode([1, 2, 3]), json_encode(['a' => 1]), json_encode([1 => 'x']), json_encode([]), json_encode(null), json_encode(1.5), json_encode("a/b<&'\">"), json_encode("\xc3\xa9"));
/// ```
/// ```expect
/// string(7) "[1,2,3]"
/// string(7) "{"a":1}"
/// string(9) "{"1":"x"}"
/// string(2) "[]"
/// string(4) "null"
/// string(3) "1.5"
/// string(12) ""a\/b<&'\">""
/// string(8) ""\u00e9""
/// ```
///
/// ```php
/// var_dump(json_encode([], JSON_FORCE_OBJECT), json_encode("a/b", JSON_UNESCAPED_SLASHES), json_encode("<>", JSON_HEX_TAG), json_encode("&", JSON_HEX_AMP), json_encode("'", JSON_HEX_APOS), json_encode('"', JSON_HEX_QUOT), json_encode("\xc3\xa9", JSON_UNESCAPED_UNICODE), json_encode(1.0, JSON_PRESERVE_ZERO_FRACTION), json_encode("12", JSON_NUMERIC_CHECK), json_encode([1, [2]], JSON_PRETTY_PRINT));
/// ```
/// ```expect
/// string(2) "{}"
/// string(5) ""a/b""
/// string(14) ""\u003C\u003E""
/// string(8) ""\u0026""
/// string(8) ""\u0027""
/// string(8) ""\u0022""
/// string(4) ""é""
/// string(3) "1.0"
/// string(2) "12"
/// string(32) "[
///     1,
///     [
///         2
///     ]
/// ]"
/// ```
///
/// ```php
/// var_dump(json_encode(INF), json_encode(INF, JSON_PARTIAL_OUTPUT_ON_ERROR), json_last_error(), json_last_error_msg(), json_encode("\x9f"), json_last_error(), json_encode([[1]], 0, 1), json_last_error(), json_last_error_msg());
/// ```
/// ```expect
/// bool(false)
/// string(1) "0"
/// int(7)
/// string(34) "Inf and NaN cannot be JSON encoded"
/// bool(false)
/// int(5)
/// bool(false)
/// int(1)
/// string(28) "Maximum stack depth exceeded"
/// ```
/// The three options that need a second one beside them, `%s` standing
/// for the bitwise or the compiler does not read yet: 2304 is
/// `JSON_UNESCAPED_UNICODE` with `JSON_UNESCAPED_LINE_TERMINATORS`, the
/// pair that leaves U+2028 and U+2029 as they stand.
///
/// ```php
/// var_dump(json_encode("a\x9fb", JSON_INVALID_UTF8_SUBSTITUTE), json_encode("a\x9fb", JSON_INVALID_UTF8_IGNORE), json_encode("\xe2\x80\xa8", JSON_UNESCAPED_UNICODE), json_encode("\xe2\x80\xa8", 2304));
/// ```
/// ```expect
/// string(10) ""a\ufffdb""
/// string(4) ""ab""
/// string(8) ""\u2028""
/// string(5) "" ""
/// ```
///
/// An object that holds itself is `JSON_ERROR_RECURSION`, and `null`
/// in its place under `JSON_PARTIAL_OUTPUT_ON_ERROR`.
///
/// ```php
/// $a = new stdClass; $a->prop = $a; var_dump(json_encode($a), json_last_error(), json_last_error_msg(), json_encode($a, JSON_PARTIAL_OUTPUT_ON_ERROR));
/// ```
/// ```expect
/// bool(false)
/// int(6)
/// string(18) "Recursion detected"
/// string(13) "{"prop":null}"
/// ```
fn json_encode(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 3)?;
    let value = req.argument(1).unwrap_or_default();
    let options = long_argument(req, 2, 0)?;
    let depth = long_argument(req, 3, DEFAULT_DEPTH)?;
    let mut encoder = encode::Encoder::new(req, depth)?;
    let mut buffer = Vec::new();
    encode::encode(req, &mut buffer, &value, options, &mut encoder)?;
    let partial = options & PARTIAL_OUTPUT_ON_ERROR != 0;
    if options & THROW_ON_ERROR == 0 || partial {
        set_error(req, encoder.error_code);
        if encoder.error_code != ERROR_NONE && !partial {
            return Ok(Value::bool(false));
        }
    } else if encoder.error_code != ERROR_NONE {
        return Err(throw_json(req, encoder.error_code));
    }
    Ok(Value::string(PhpString::new(req, &buffer)?))
}

/// `json_decode(string $json, ?bool $associative = null, int $depth =
/// 512, int $flags = 0): mixed`, `PHP_FUNCTION(json_decode)` over
/// `php_json_decode_ex`: the value, or `null` with the code in
/// `json_last_error`. The `bool` overrides `JSON_OBJECT_AS_ARRAY` where
/// it is given, and the empty string is a syntax error before the depth
/// is read at all.
///
/// ```php
/// var_dump(json_decode('{"a":[1,2],"b":null}'), json_decode('{"a":1}', true), json_decode('"\u00e9\ud834\udd1e"'), json_decode('123456789012345678901234567890'), json_decode('123456789012345678901234567890', false, 512, JSON_BIGINT_AS_STRING));
/// ```
/// ```expect
/// object(stdClass)#1 (2) {
///   ["a"]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(2)
///   }
///   ["b"]=>
///   NULL
/// }
/// array(1) {
///   ["a"]=>
///   int(1)
/// }
/// string(6) "é𝄞"
/// float(1.2345678901234568E+29)
/// string(30) "123456789012345678901234567890"
/// ```
///
/// ```php
/// foreach (['', '.', '[1}', '[[1]]', '"\ud834"', "\"\x01\"", '{"\u0000":1}'] as $text) { var_dump(json_decode($text, false, 2), json_last_error(), json_last_error_msg()); }
/// ```
/// ```expect
/// NULL
/// int(4)
/// string(12) "Syntax error"
/// NULL
/// int(4)
/// string(12) "Syntax error"
/// NULL
/// int(2)
/// string(42) "State mismatch (invalid or malformed JSON)"
/// NULL
/// int(1)
/// string(28) "Maximum stack depth exceeded"
/// NULL
/// int(10)
/// string(50) "Single unpaired UTF-16 surrogate in unicode escape"
/// NULL
/// int(3)
/// string(53) "Control character error, possibly incorrectly encoded"
/// NULL
/// int(9)
/// string(36) "The decoded property name is invalid"
/// ```
///
/// ```php
/// json_decode('[1', false, 512, JSON_THROW_ON_ERROR);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught JsonException: Syntax error in %s:2
/// Stack trace:
/// #0 %s(2): json_decode('[1', false, 512, 4194304)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// Caught, the throwable answers its class, php's message and the
/// `JSON_ERROR_*` code the parser stopped with, which is the whole of
/// what `JSON_THROW_ON_ERROR` carries.
///
/// ```php
/// try { json_decode('[1', false, 512, JSON_THROW_ON_ERROR); } catch (JsonException $e) { var_dump(get_class($e), $e->getMessage(), $e->getCode()); }
/// ```
/// ```expect
/// string(13) "JsonException"
/// string(12) "Syntax error"
/// int(4)
/// ```
///
/// The two `INVALID_UTF8_*` options inside a string, which the scanner
/// answers where the encoder answers them outside one.
///
/// ```php
/// var_dump(json_decode("\"a\x9fb\"", false, 512, JSON_INVALID_UTF8_SUBSTITUTE), json_decode("\"a\x9fb\"", false, 512, JSON_INVALID_UTF8_IGNORE));
/// ```
/// ```expect
/// string(5) "a�b"
/// string(2) "ab"
/// ```
fn json_decode(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 4)?;
    let text = string_argument(req, 1)?;
    let associative = match req.argument(2) {
        None => None,
        Some(given) => {
            let ty = Type::Nullable(Box::new(Type::Bool));
            coerce_param(req, &given, &ty, 2)?.deref().as_bool()
        }
    };
    let depth = long_argument(req, 3, DEFAULT_DEPTH)?;
    let mut options = long_argument(req, 4, 0)?;
    let throwing = options & THROW_ON_ERROR != 0;
    if !throwing {
        set_error(req, ERROR_NONE);
    }
    if text.is_empty() {
        if throwing {
            return Err(throw_json(req, ERROR_SYNTAX));
        }
        set_error(req, ERROR_SYNTAX);
        return Ok(Value::null());
    }
    depth_within(req, depth, 3)?;
    // For BC reasons the bool overrides the option bit, either way.
    match associative {
        Some(true) => options |= OBJECT_AS_ARRAY,
        Some(false) => options &= !OBJECT_AS_ARRAY,
        None => {}
    }
    match decode::decode(req, text.as_bytes(), options, depth)? {
        Ok(value) => Ok(value),
        Err(code) => {
            if throwing {
                return Err(throw_json(req, code));
            }
            set_error(req, code);
            Ok(Value::null())
        }
    }
}

/// `json_validate(string $json, int $depth = 512, int $flags = 0):
/// bool`, `PHP_FUNCTION(json_validate)` over `php_json_validate_ex`:
/// the scanner and the grammar with nothing built under them, so the
/// answer is what `json_decode` would have managed and the code it
/// leaves is the same. `JSON_INVALID_UTF8_IGNORE` is the one option it takes.
///
/// ```php
/// foreach (['{"a":1}', '[]', '"x"', '1', '-', '', '[1}'] as $text) { var_dump(json_validate($text), json_last_error()); }
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// bool(true)
/// int(0)
/// bool(true)
/// int(0)
/// bool(true)
/// int(0)
/// bool(false)
/// int(4)
/// bool(false)
/// int(4)
/// bool(false)
/// int(2)
/// ```
///
/// ```php
/// var_dump(json_validate("\"a\xb0b\""), json_last_error(), json_validate("\"a\xb0b\"", 512, JSON_INVALID_UTF8_IGNORE), json_validate('[[1]]', 2), json_last_error_msg());
/// ```
/// ```expect
/// bool(false)
/// int(5)
/// bool(true)
/// bool(false)
/// string(28) "Maximum stack depth exceeded"
/// ```
///
/// ```php
/// json_validate('1', 512, JSON_THROW_ON_ERROR);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: json_validate(): Argument #3 ($flags) must be a valid flag (allowed flags: JSON_INVALID_UTF8_IGNORE) in %s:2
/// Stack trace:
/// #0 %s(2): json_validate('1', 512, 4194304)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn json_validate(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 3)?;
    let text = string_argument(req, 1)?;
    let depth = long_argument(req, 2, DEFAULT_DEPTH)?;
    let options = long_argument(req, 3, 0)?;
    if options != 0 && options != INVALID_UTF8_IGNORE {
        return Err(arg_value_error(
            req,
            3,
            "be a valid flag (allowed flags: JSON_INVALID_UTF8_IGNORE)",
        ));
    }
    if text.is_empty() {
        set_error(req, ERROR_SYNTAX);
        return Ok(Value::bool(false));
    }
    set_error(req, ERROR_NONE);
    depth_within(req, depth, 2)?;
    match decode::validate(req, text.as_bytes(), options, depth)? {
        Ok(_) => Ok(Value::bool(true)),
        Err(code) => {
            set_error(req, code);
            Ok(Value::bool(false))
        }
    }
}

/// `PHP_FUNCTION(json_last_error)`: `JSON_G(error_code)`,
/// `JSON_ERROR_NONE` while nothing in the request encoded or decoded.
///
/// ```php
/// var_dump(json_last_error(), json_last_error_msg());
/// ```
/// ```expect
/// int(0)
/// string(8) "No error"
/// ```
fn json_last_error(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    Ok(Value::int(req.state::<State>().error_code))
}

/// `PHP_FUNCTION(json_last_error_msg)`: `php_json_get_error_msg` over
/// `JSON_G(error_code)`. The witness is `json_last_error`'s.
fn json_last_error_msg(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let code = req.state::<State>().error_code;
    Ok(Value::string(PhpString::new(
        req,
        error_message(code).as_bytes(),
    )?))
}

/// `PHP_MINFO_FUNCTION(json)`: the one-row table `json support =>
/// enabled`, what `phpinfo()`, `php --ri json` and
/// `ReflectionExtension::info` print for the extension.
///
/// ```php
/// (new ReflectionExtension('json'))->info();
/// ```
/// ```expect
/// json
///
/// json support => enabled
/// ```
fn json_info(req: &mut Request) {
    req.info_table(&[&["json support", "enabled"]]);
}
