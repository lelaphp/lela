//! The `json` extension: `php-src/ext/json` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/json/tests`, through `xtask phpt --subset`.
//!
//! Declared by the attributes as `json.stub.php` at the tag declares it
//! and `php -n --re json` renders it (design §12, `xtask crosscheck`): the twenty-nine
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

use php_ext::abi::{Extension, ExtensionBuilder, OrFalse, Start};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::raising::{Thrown, arg_value_error, fatal, new_throwable, pending, throw_value};
use php_ext::request::Request;
use php_ext::values::{ClassId, Owner, PhpString, SmartStr, Value};

use constants::{
    JSON_ERROR_CTRL_CHAR, JSON_ERROR_DEPTH, JSON_ERROR_INF_OR_NAN,
    JSON_ERROR_INVALID_PROPERTY_NAME, JSON_ERROR_NON_BACKED_ENUM, JSON_ERROR_NONE,
    JSON_ERROR_RECURSION, JSON_ERROR_STATE_MISMATCH, JSON_ERROR_SYNTAX,
    JSON_ERROR_UNSUPPORTED_TYPE, JSON_ERROR_UTF8, JSON_ERROR_UTF16, JSON_INVALID_UTF8_IGNORE,
    JSON_OBJECT_AS_ARRAY, JSON_PARTIAL_OUTPUT_ON_ERROR, JSON_THROW_ON_ERROR,
};

/// The constants `PHP_MINIT_FUNCTION(json)` registers, in its order:
/// the encoder's flags (`php_json.h`), the decoder's two, the three both
/// read, and the twelve error codes of `php_json_error_code`. The bodies
/// read the values declared here.
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
#[php_const]
pub(crate) mod constants {
    pub(crate) const JSON_HEX_TAG: i64 = 1 << 0;
    pub(crate) const JSON_HEX_AMP: i64 = 1 << 1;
    pub(crate) const JSON_HEX_APOS: i64 = 1 << 2;
    pub(crate) const JSON_HEX_QUOT: i64 = 1 << 3;
    pub(crate) const JSON_FORCE_OBJECT: i64 = 1 << 4;
    pub(crate) const JSON_NUMERIC_CHECK: i64 = 1 << 5;
    pub(crate) const JSON_UNESCAPED_SLASHES: i64 = 1 << 6;
    pub(crate) const JSON_PRETTY_PRINT: i64 = 1 << 7;
    pub(crate) const JSON_UNESCAPED_UNICODE: i64 = 1 << 8;
    pub(crate) const JSON_PARTIAL_OUTPUT_ON_ERROR: i64 = 1 << 9;
    pub(crate) const JSON_PRESERVE_ZERO_FRACTION: i64 = 1 << 10;
    pub(crate) const JSON_UNESCAPED_LINE_TERMINATORS: i64 = 1 << 11;
    pub(crate) const JSON_OBJECT_AS_ARRAY: i64 = 1 << 0;
    pub(crate) const JSON_BIGINT_AS_STRING: i64 = 1 << 1;
    pub(crate) const JSON_INVALID_UTF8_IGNORE: i64 = 1 << 20;
    pub(crate) const JSON_INVALID_UTF8_SUBSTITUTE: i64 = 1 << 21;
    pub(crate) const JSON_THROW_ON_ERROR: i64 = 1 << 22;
    pub(crate) const JSON_ERROR_NONE: i64 = 0;
    pub(crate) const JSON_ERROR_DEPTH: i64 = 1;
    pub(crate) const JSON_ERROR_STATE_MISMATCH: i64 = 2;
    pub(crate) const JSON_ERROR_CTRL_CHAR: i64 = 3;
    pub(crate) const JSON_ERROR_SYNTAX: i64 = 4;
    pub(crate) const JSON_ERROR_UTF8: i64 = 5;
    pub(crate) const JSON_ERROR_RECURSION: i64 = 6;
    pub(crate) const JSON_ERROR_INF_OR_NAN: i64 = 7;
    pub(crate) const JSON_ERROR_UNSUPPORTED_TYPE: i64 = 8;
    pub(crate) const JSON_ERROR_INVALID_PROPERTY_NAME: i64 = 9;
    pub(crate) const JSON_ERROR_UTF16: i64 = 10;
    pub(crate) const JSON_ERROR_NON_BACKED_ENUM: i64 = 11;
}

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
/// `JSON_G(error_code)`: what the last `json_encode` or `json_decode`
/// of the request left, `PHP_JSON_ERROR_NONE` until one runs.
#[derive(Default)]
struct State {
    error_code: i64,
}

/// The extension value (design §7.1): the constants, the five natives
/// and the two classes in `json.stub.php`'s order, and the state
/// declared on it.
///
/// ```php
/// var_dump(extension_loaded('json'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::DECLARED)
        .functions(&[
            json_encode::DECLARED,
            json_decode::DECLARED,
            json_validate::DECLARED,
            json_last_error::DECLARED,
            json_last_error_msg::DECLARED,
        ])
        .class::<JsonSerializable>()
        .class::<JsonException>()
        .request_state::<State>()
        .engine_state(|start: &Start| start.class::<JsonException>().map(Raised))
        .info(json_info)
        .build("json")
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
#[php_class(interface, use_guards)]
pub(crate) struct JsonSerializable;

#[php_class]
impl JsonSerializable {
    #[php(tentative)]
    fn jsonSerialize(this: This<Self>) -> Value;
}

/// Core's `Exception`, named for `extends`.
#[php_class(extern)]
pub(crate) struct Exception;

/// `class JsonException extends Exception`, what `JSON_THROW_ON_ERROR`
/// throws.
#[php_class(extends(Exception))]
pub(crate) struct JsonException;

#[php_class]
impl JsonException {}

/// `php_json_get_error_msg`: the twelve messages of `json.c` and
/// `Unknown error` past them.
fn error_message(code: i64) -> &'static str {
    match code {
        JSON_ERROR_NONE => "No error",
        JSON_ERROR_DEPTH => "Maximum stack depth exceeded",
        JSON_ERROR_STATE_MISMATCH => "State mismatch (invalid or malformed JSON)",
        JSON_ERROR_CTRL_CHAR => "Control character error, possibly incorrectly encoded",
        JSON_ERROR_SYNTAX => "Syntax error",
        JSON_ERROR_UTF8 => "Malformed UTF-8 characters, possibly incorrectly encoded",
        JSON_ERROR_RECURSION => "Recursion detected",
        JSON_ERROR_INF_OR_NAN => "Inf and NaN cannot be JSON encoded",
        JSON_ERROR_UNSUPPORTED_TYPE => "Type is not supported",
        JSON_ERROR_INVALID_PROPERTY_NAME => "The decoded property name is invalid",
        JSON_ERROR_UTF16 => "Single unpaired UTF-16 surrogate in unicode escape",
        JSON_ERROR_NON_BACKED_ENUM => "Non-backed enums have no default serialization",
        _ => "Unknown error",
    }
}

/// `php_json_exception_ce`, resolved once as the engine starts
/// (ADR-0005: engine state, immutable), so a raise names the class by
/// its identity, never by a name looked up as it throws (#3403).
#[derive(Clone, Copy)]
struct Raised(ClassId);

/// `zend_throw_exception(php_json_exception_ce, msg, error_code)`: the
/// message `json_last_error_msg` would have answered, under the code
/// `json_last_error` would have, which is what `getCode()` reads.
fn throw_json(req: &mut Request, code: i64) -> Thrown {
    let Some(&Raised(class)) = req.engine::<Raised>() else {
        return fatal(req, "InternalError: json was not started");
    };
    let value = match new_throwable(req, class, error_message(code)) {
        Ok(value) => value,
        Err(thrown) => return thrown,
    };
    if let Some(object) = value.as_object()
        && let Err(thrown) = php_ext::call::write_prop(req, &object, b"code", Owner::int(code))
    {
        value.give_up(req);
        return thrown;
    }
    let thrown = throw_value(req, &value);
    value.give_up(req);
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
///
/// The text is written into a `smart_str` on the request's heap, laid
/// down at 231 bytes and grown to the page past what it holds, and
/// `smart_str_extract` trims it to the answer, so the peak counts the
/// buffer beside the string it hands back:
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); $p = memory_get_peak_usage(); $u = memory_get_usage(); echo $p - $s, ' ', $u - $s, "\n"; }
/// class N { public $a=1; public $b=1; }
/// $n = new N; $long = str_repeat('x', 300);
/// pk(fn () => json_encode($n));
/// pk(fn () => json_encode([1,2,3]));
/// pk(fn () => json_encode([$long, $long]));
/// ```
/// ```expect
/// 304 48
/// 296 40
/// 4312 640
/// ```
///
/// Since 8.5.12 a string's room is asked for fallibly, and the encoder
/// stops at its next value or escaped byte once the memory limit was
/// met, so a buffer the limit refused, or one an escaped string, a value
/// or a key has grown past it, ends the call with the limit's fatal as it
/// returns rather than growing on to the hard limit. Each case crosses the
/// limit where a new chunk is asked for, whose figure is the chunk's in any
/// layout; a huge block's growth in place names a figure the address space
/// decides, in php's process as in the engine's (#3442):
///
/// ```php
/// ini_set('memory_limit', '4M');
/// $a = [str_repeat('a', 1200000), str_repeat('b', 1200000), str_repeat('c', 1200000)];
/// $j = json_encode($a);
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
///
/// ```php
/// ini_set('memory_limit', '4M');
/// $j = json_encode(str_repeat("\0", 500000));
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
///
/// ```php
/// ini_set('memory_limit', '4M');
/// $j = json_encode([str_repeat("\x01", 500000) => 1]);
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn json_encode(
    req: &mut Request,
    value: Value,
    #[php(default = 0)] flags: i64,
    #[php(default = 512)] depth: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let mut encoder = encode::Encoder::new(req, depth)?;
    let mut buffer = SmartStr::default();
    let wrote = encode::encode(req, &mut buffer, &value, flags, &mut encoder);
    let partial = flags & JSON_PARTIAL_OUTPUT_ON_ERROR != 0;
    let failed = wrote.is_err() || encoder.error_code != JSON_ERROR_NONE && !partial;
    // `smart_str_free(&buf)` where no string is answered.
    if failed {
        buffer.free(req);
    }
    wrote?;
    if flags & JSON_THROW_ON_ERROR == 0 || partial {
        set_error(req, encoder.error_code);
        if failed {
            pending(req)?;
            return Ok(OrFalse::False);
        }
    } else if failed {
        return Err(throw_json(req, encoder.error_code));
    }
    // `smart_str_extract`, and what a reader of an object's properties
    // left pending, a lazy object's initializer that threw, thrown once
    // the encoder is done.
    let answer = buffer.extract(req)?;
    if let Err(thrown) = pending(req) {
        answer.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(answer))
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
///
/// `$json` is a `string` parameter, whose `null` is the 8.1 deprecation
/// and the empty string under the weak rule and the `TypeError` under
/// `strict_types`.
///
/// ```php
/// declare(strict_types=1);
/// try { json_decode(null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// json_decode(): Argument #1 ($json) must be of type string, null given
/// ```
///
/// Since 8.5.12 an array, an object and an array's growth are asked for
/// fallibly, as `array_finit`, `object_finit` and
/// `zend_hash_next_index_finsert`: one the memory limit refuses is the
/// parser's `YYERROR`, and the limit's fatal is raised as the call
/// returns, rather than the values growing on to the hard limit:
///
/// ```php
/// $j = '[' . str_repeat('[[]],', 1024 * 1024) . '[]]';
/// ini_set('memory_limit', '10M');
/// $a = json_decode($j);
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 10485760 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 4
/// ```
///
/// ```php
/// ini_set('memory_limit', '10M');
/// $a = json_decode('[{}' . str_repeat(',{}', 1024 * 1024) . ']');
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 10485760 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn json_decode(
    req: &mut Request,
    json: PhpString,
    #[php(default = null)] associative: Option<bool>,
    #[php(default = 512)] depth: i64,
    #[php(default = 0)] mut flags: i64,
) -> Result<Owner<Value>, Thrown> {
    let throwing = flags & JSON_THROW_ON_ERROR != 0;
    if !throwing {
        set_error(req, JSON_ERROR_NONE);
    }
    if json.is_empty() {
        if throwing {
            return Err(throw_json(req, JSON_ERROR_SYNTAX));
        }
        set_error(req, JSON_ERROR_SYNTAX);
        return Ok(Owner::null());
    }
    depth_within(req, depth, 3)?;
    // For BC reasons the bool overrides the option bit, either way.
    match associative {
        Some(true) => flags |= JSON_OBJECT_AS_ARRAY,
        Some(false) => flags &= !JSON_OBJECT_AS_ARRAY,
        None => {}
    }
    match decode::decode(req, json.as_bytes(), flags, depth)? {
        Ok(value) => Ok(value),
        Err(code) => {
            if throwing {
                return Err(throw_json(req, code));
            }
            set_error(req, code);
            Ok(Owner::null())
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
#[php_function]
fn json_validate(
    req: &mut Request,
    json: PhpString,
    #[php(default = 512)] depth: i64,
    #[php(default = 0)] flags: i64,
) -> Result<bool, Thrown> {
    if flags != 0 && flags != JSON_INVALID_UTF8_IGNORE {
        return Err(arg_value_error(
            req,
            3,
            "be a valid flag (allowed flags: JSON_INVALID_UTF8_IGNORE)",
        ));
    }
    if json.is_empty() {
        set_error(req, JSON_ERROR_SYNTAX);
        return Ok(false);
    }
    set_error(req, JSON_ERROR_NONE);
    depth_within(req, depth, 2)?;
    match decode::validate(req, json.as_bytes(), flags, depth)? {
        Ok(built) => {
            built.give_up(req);
            Ok(true)
        }
        Err(code) => {
            set_error(req, code);
            Ok(false)
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
#[php_function]
fn json_last_error(req: &mut Request) -> i64 {
    req.state::<State>().error_code
}

/// `PHP_FUNCTION(json_last_error_msg)`: `php_json_get_error_msg` over
/// `JSON_G(error_code)`: the message of the code the last call left.
///
/// ```php
/// json_decode('[');
/// var_dump(json_last_error_msg());
/// json_decode('[1]');
/// var_dump(json_last_error_msg());
/// ```
/// ```expect
/// string(12) "Syntax error"
/// string(8) "No error"
/// ```
#[php_function]
fn json_last_error_msg(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let code = req.state::<State>().error_code;
    PhpString::new(req, error_message(code).as_bytes())
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
    req.extensions_mut()
        .info_table(&[&["json support", "enabled"]]);
}
