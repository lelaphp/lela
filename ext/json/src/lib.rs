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
//! under `Exception`. `json_last_error` and `json_last_error_msg` read
//! the request state, `JSON_G(error_code)`; the bodies of `json_encode`,
//! `json_decode` and `json_validate`, `php_json_encode_zval` and
//! `php_json_parser`, are #358, and until it lands each throws `Error`
//! naming that, since a wrong value is worse than a loud gap.
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

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::decl::{
    ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr, EnumBacking, FunctionDecl, MethodDecl,
    Name, Parameter, Type, Visibility,
};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

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
                        int(b"depth", Type::Int, 512),
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
                        int(b"depth", Type::Int, 512),
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
                        int(b"depth", Type::Int, 512),
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
        (b"JSON_HEX_TAG", 1 << 0),
        (b"JSON_HEX_AMP", 1 << 1),
        (b"JSON_HEX_APOS", 1 << 2),
        (b"JSON_HEX_QUOT", 1 << 3),
        (b"JSON_FORCE_OBJECT", 1 << 4),
        (b"JSON_NUMERIC_CHECK", 1 << 5),
        (b"JSON_UNESCAPED_SLASHES", 1 << 6),
        (b"JSON_PRETTY_PRINT", 1 << 7),
        (b"JSON_UNESCAPED_UNICODE", 1 << 8),
        (b"JSON_PARTIAL_OUTPUT_ON_ERROR", 1 << 9),
        (b"JSON_PRESERVE_ZERO_FRACTION", 1 << 10),
        (b"JSON_UNESCAPED_LINE_TERMINATORS", 1 << 11),
        (b"JSON_OBJECT_AS_ARRAY", 1 << 0),
        (b"JSON_BIGINT_AS_STRING", 1 << 1),
        (b"JSON_INVALID_UTF8_IGNORE", 1 << 20),
        (b"JSON_INVALID_UTF8_SUBSTITUTE", 1 << 21),
        (b"JSON_THROW_ON_ERROR", 1 << 22),
        (b"JSON_ERROR_NONE", 0),
        (b"JSON_ERROR_DEPTH", 1),
        (b"JSON_ERROR_STATE_MISMATCH", 2),
        (b"JSON_ERROR_CTRL_CHAR", 3),
        (b"JSON_ERROR_SYNTAX", 4),
        (b"JSON_ERROR_UTF8", 5),
        (b"JSON_ERROR_RECURSION", 6),
        (b"JSON_ERROR_INF_OR_NAN", 7),
        (b"JSON_ERROR_UNSUPPORTED_TYPE", 8),
        (b"JSON_ERROR_INVALID_PROPERTY_NAME", 9),
        (b"JSON_ERROR_UTF16", 10),
        (b"JSON_ERROR_NON_BACKED_ENUM", 11),
    ];
    rows.into_iter()
        .map(|(name, value)| ConstDecl {
            name: Name(name.into()),
            value: ConstExpr::Long(value),
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
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        frameless: Vec::new(),
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

/// The `Error` a body #358 ports throws meanwhile: the native's name
/// and the number, never a value php would not answer.
fn not_ported(req: &mut Request, native: &str) -> Thrown {
    throw(
        req,
        "Error",
        &format!("{native}() is not served by this engine yet (#358)"),
    )
}

/// `json_encode(mixed $value, int $flags = 0, int $depth = 512):
/// string|false`, declared as the stub has it; the body is #358. The
/// witness pins the declaration, which is what this crate renders.
///
/// ```php
/// echo new ReflectionFunction('json_encode');
/// ```
/// ```expect
/// Function [ <internal:json> function json_encode ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> int $flags = 0 ]
///     Parameter #2 [ <optional> int $depth = 512 ]
///   }
///   - Return [ string|false ]
/// }
/// ```
fn json_encode(req: &mut Request) -> Result<Value, Thrown> {
    Err(not_ported(req, "json_encode"))
}

/// `json_decode(string $json, ?bool $associative = null, int $depth =
/// 512, int $flags = 0): mixed`, declared as the stub has it; the body
/// is #358.
///
/// ```php
/// echo new ReflectionFunction('json_decode');
/// ```
/// ```expect
/// Function [ <internal:json> function json_decode ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $json ]
///     Parameter #1 [ <optional> ?bool $associative = null ]
///     Parameter #2 [ <optional> int $depth = 512 ]
///     Parameter #3 [ <optional> int $flags = 0 ]
///   }
///   - Return [ mixed ]
/// }
/// ```
fn json_decode(req: &mut Request) -> Result<Value, Thrown> {
    Err(not_ported(req, "json_decode"))
}

/// `json_validate(string $json, int $depth = 512, int $flags = 0):
/// bool`, declared as the stub has it; the body is #358.
///
/// ```php
/// echo new ReflectionFunction('json_validate');
/// ```
/// ```expect
/// Function [ <internal:json> function json_validate ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $json ]
///     Parameter #1 [ <optional> int $depth = 512 ]
///     Parameter #2 [ <optional> int $flags = 0 ]
///   }
///   - Return [ bool ]
/// }
/// ```
fn json_validate(req: &mut Request) -> Result<Value, Thrown> {
    Err(not_ported(req, "json_validate"))
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
#[allow(clippy::unnecessary_wraps)] // the shape of an `Entry`
fn json_last_error(req: &mut Request) -> Result<Value, Thrown> {
    Ok(Value::int(req.state::<State>().error_code))
}

/// `PHP_FUNCTION(json_last_error_msg)`: `php_json_get_error_msg` over
/// `JSON_G(error_code)`, the twelve messages of `json.c` and `Unknown
/// error` past them. The witness is `json_last_error`'s.
fn json_last_error_msg(req: &mut Request) -> Result<Value, Thrown> {
    let code = req.state::<State>().error_code;
    let message = match code {
        0 => "No error",
        1 => "Maximum stack depth exceeded",
        2 => "State mismatch (invalid or malformed JSON)",
        3 => "Control character error, possibly incorrectly encoded",
        4 => "Syntax error",
        5 => "Malformed UTF-8 characters, possibly incorrectly encoded",
        6 => "Recursion detected",
        7 => "Inf and NaN cannot be JSON encoded",
        8 => "Type is not supported",
        9 => "The decoded property name is invalid",
        10 => "Single unpaired UTF-16 surrogate in unicode escape",
        11 => "Non-backed enums have no default serialization",
        _ => "Unknown error",
    };
    Ok(Value::string(PhpString::new(req, message.as_bytes())?))
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
