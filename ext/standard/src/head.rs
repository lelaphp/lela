//! The header natives of `Z/ext/standard/head.c`,
//! `basic_functions.stub.php:2222-2242`, each over the request's
//! `SG(sapi_headers)`: the list, the response code and the status line
//! are the engine's, and `sapi_header_op`'s warnings are raised there,
//! at the site the request has reached. Under the CLI, which every
//! witness runs under, the host's `header_handler` answers zero
//! (`Z/sapi/cli/php_cli.c:376`), so a line `header()` is given is freed
//! and `headers_list()` stays empty; a web host's list is read through
//! the `Capture` sink in `php_engine`'s tests.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

use crate::output::Entry;
use crate::{argument_count_between, declared, long_argument, parameter, string_argument};

/// The five, in the stub's order less the two cookie natives.
///
/// ```php
/// header("X-A: 1"); var_dump(headers_list(), headers_sent(), http_response_code());
/// ```
/// ```expect
/// array(0) {
/// }
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let by_ref = |name: &[u8]| Parameter {
        ty: None,
        by_ref: true,
        default: Some(ConstExpr::Null),
        ..parameter(name, Type::Mixed)
    };
    vec![
        (
            declared(
                b"header",
                &[
                    parameter(b"header", Type::String),
                    Parameter {
                        default: Some(ConstExpr::Bool(true)),
                        ..parameter(b"replace", Type::Bool)
                    },
                    Parameter {
                        default: Some(ConstExpr::Long(0)),
                        ..parameter(b"response_code", Type::Int)
                    },
                ],
                Type::Void,
            ),
            header as Entry,
        ),
        (
            declared(
                b"header_remove",
                &[Parameter {
                    default: Some(ConstExpr::Null),
                    ..parameter(b"name", Type::Nullable(Box::new(Type::String)))
                }],
                Type::Void,
            ),
            header_remove as Entry,
        ),
        (
            declared(
                b"http_response_code",
                &[Parameter {
                    default: Some(ConstExpr::Long(0)),
                    ..parameter(b"response_code", Type::Int)
                }],
                Type::Union(vec![Type::Int, Type::Bool]),
            ),
            http_response_code as Entry,
        ),
        (
            declared(
                b"headers_sent",
                &[by_ref(b"filename"), by_ref(b"line")],
                Type::Bool,
            ),
            headers_sent as Entry,
        ),
        (
            declared(b"headers_list", &[], Type::Array),
            headers_list as Entry,
        ),
    ]
}

/// `Z_PARAM_BOOL` at `position`.
fn bool_argument(req: &mut Request, position: u32) -> Result<bool, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Bool, position)?;
    Ok(coerced.as_bool().unwrap_or(false))
}

/// `header(string $header, bool $replace = true, int $response_code = 0): void`
/// (`Z/ext/standard/head.c:32`): `sapi_header_op` under `REPLACE` or
/// `ADD`, whose answer the native drops. Once the first byte has
/// reached the host the line is refused with php's warning naming where
/// the body started, `SG(headers_sent)` and `OG(output_start_*)`.
///
/// ```php
/// echo "x"; header("X-A: 1"); var_dump(headers_sent());
/// ```
/// ```expect
/// x
/// Warning: Cannot modify header information - headers already sent by (output started at %s:2) in %s on line 2
/// bool(true)
/// ```
///
/// A line with a second line in it, or a NUL, is the warning and
/// nothing taken; the response code the third argument carries is
/// taken with the line.
///
/// ```php
/// header("X-A: 1", true, 404); $code = http_response_code(); header("X-A: a\nb"); var_dump($code);
/// ```
/// ```expect
///
/// Warning: Header may not contain more than a single header, new line detected in %s on line 2
/// int(404)
/// ```
///
/// What a line holds of the request heap under the CLI, where the line
/// itself is freed at once: a `Content-Type` holds its mimetype in the
/// thirty-two-byte slot the default one would take, and an `HTTP/`
/// line holds its bytes until the block is sent.
///
/// ```php
/// $b = memory_get_usage(); header("Content-Type: text/plain"); $a = memory_get_usage() - $b; header("HTTP/1.1 404 No"); $c = memory_get_usage() - $b; echo "x"; $d = memory_get_usage() - $b; var_dump($a, $c, $d);
/// ```
/// ```expect
/// xint(32)
/// int(48)
/// int(32)
/// ```
fn header(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let given = req.argument_count();
    let line = string_argument(req, 1)?;
    let replace = if given >= 2 {
        bool_argument(req, 2)?
    } else {
        true
    };
    let response_code = if given >= 3 {
        long_argument(req, 3)?
    } else {
        0
    };
    req.header_add(line.as_bytes(), replace, response_code);
    Ok(Value::null())
}

/// `header_remove(?string $name = null): void` (`Z/ext/standard/head.c:53`):
/// `sapi_header_op` under `DELETE`, or `DELETE_ALL` for a null name. A
/// name with a colon in it is `sapi_header_op`'s warning, and the
/// warning's display is the first byte to reach the host, so the next
/// header op is refused as sent.
///
/// ```php
/// header_remove("X-A: 1");
/// header_remove("X-A");
/// ```
/// ```expect
///
/// Warning: Header to delete may not contain colon. in %s on line 2
///
/// Warning: Cannot modify header information - headers already sent by (output started at %s:2) in %s on line 3
/// ```
fn header_remove(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 1)?;
    let name = if req.argument_count() == 1 {
        let given = req.argument(1).unwrap_or_default();
        if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
            None
        } else {
            let coerced = coerce_param(req, &given, &Type::Nullable(Box::new(Type::String)), 1)?;
            Some(coerced.as_str_bytes().unwrap_or_default().to_vec())
        }
    } else {
        None
    };
    req.header_remove(name.as_deref());
    Ok(Value::null())
}

/// `http_response_code(int $response_code = 0): int|bool`
/// (`Z/ext/standard/head.c:370`): with a code, the code written and the
/// one before it answered, `true` where there was none, which under the
/// CLI is the first write; refused with the docref warning once the
/// block is sent, and warned about with no effect on the status line
/// after a `header('HTTP/...')`. Without one, the code as it stands, and
/// `false` for none.
///
/// ```php
/// $a = http_response_code(); $b = http_response_code(404); $c = http_response_code(); $d = http_response_code(500); header("HTTP/1.1 201 Made"); $e = http_response_code(); $f = http_response_code(202); var_dump($a, $b, $c, $d, $e, $f);
/// var_dump(http_response_code(203));
/// ```
/// ```expect
///
/// Warning: http_response_code(): Calling http_response_code() after header('HTTP/...') has no effect in %s on line 2
/// bool(false)
/// bool(true)
/// int(404)
/// int(404)
/// int(201)
/// int(201)
///
/// Warning: http_response_code(): Cannot set response code - headers already sent (output started at %s:2) in %s on line 3
/// bool(false)
/// ```
fn http_response_code(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 1)?;
    let code = if req.argument_count() == 1 {
        long_argument(req, 1)?
    } else {
        0
    };
    if code != 0 {
        if req.headers_sent().is_some() {
            let message = match req.output_started_at() {
                Some((file, line)) => format!(
                    "Cannot set response code - headers already sent (output started at {}:{line})",
                    String::from_utf8_lossy(&file)
                ),
                None => String::from("Cannot set response code - headers already sent"),
            };
            docref(req, Level::Warning, &message);
            return Ok(Value::bool(false));
        }
        if req.status_line().is_some() {
            docref(
                req,
                Level::Warning,
                "Calling http_response_code() after header('HTTP/...') has no effect",
            );
        }
        let before = req.response_code();
        req.set_response_code(code);
        return Ok(if before != 0 {
            Value::int(before)
        } else {
            Value::bool(true)
        });
    }
    let current = req.response_code();
    Ok(if current == 0 {
        Value::bool(false)
    } else {
        Value::int(current)
    })
}

/// `headers_sent(&$filename = null, &$line = null): bool`
/// (`Z/ext/standard/head.c:310`): `SG(headers_sent)`, with where the
/// body started written through the two references, the line first as
/// php's `switch` falls through, and the empty string and zero while
/// nothing is sent. `SEND_REF` is what puts a reference in the two
/// places (#344); an argument that is no variable never reaches the
/// native at all, since the send raises php's `Error` over it first.
///
/// ```php
/// $before = headers_sent(); echo "x"; var_dump($before, headers_sent());
/// ```
/// ```expect
/// xbool(false)
/// bool(true)
/// ```
///
/// ```php
/// echo "x"; headers_sent($f, $l); var_dump($f, $l);
/// ```
/// ```expect
/// xstring(11) "witness.php"
/// int(2)
/// ```
///
/// ```php
/// headers_sent($f, $l); var_dump($f, $l);
/// ```
/// ```expect
/// string(0) ""
/// int(0)
/// ```
///
/// ```php
/// headers_sent(1, 2);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: headers_sent(): Argument #1 ($filename) could not be passed by reference in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
fn headers_sent(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 2)?;
    let given = req.argument_count();
    let sent = req.headers_sent();
    let (file, line) = sent.clone().unwrap_or_default();
    if given >= 2 {
        assign_ref(req, 2, |_| Ok(Value::int(i64::from(line))))?;
    }
    if given >= 1 {
        assign_ref(req, 1, |req| Ok(Value::string(PhpString::new(req, &file)?)))?;
    }
    Ok(Value::bool(sent.is_some()))
}

/// `ZEND_TRY_ASSIGN_REF_*` on the argument at `position`: `value` built
/// and written through the reference the argument is, and nothing built
/// where it is not one.
fn assign_ref(
    req: &mut Request,
    position: u32,
    value: impl FnOnce(&mut Request) -> Result<Value, Thrown>,
) -> Result<(), Thrown> {
    let Some(given) = req.argument(position) else {
        return Ok(());
    };
    let ValueRef::Reference(reference) = given.as_ref() else {
        return Ok(());
    };
    let reference = *reference;
    let value = value(req)?;
    reference.set(req, value);
    Ok(())
}

/// `headers_list(): array` (`Z/ext/standard/head.c:360`): the list, one
/// string each, in order. The CLI's `header_handler` takes no line, so
/// the list a `header()` leaves under it is empty, `Content-Type` and
/// all; the `text/plain;charset=UTF-8` a web host would list is read
/// off `Request::header_block` in `php_engine`.
///
/// ```php
/// header("X-A: 1"); header("Content-Type: text/plain"); var_dump(headers_list());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
fn headers_list(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    let mut array = PhpArray::new(req)?;
    for line in req.headers_list() {
        let value = Value::string(PhpString::new(req, &line)?);
        array.append(req, value)?;
    }
    Ok(Value::array(array))
}
