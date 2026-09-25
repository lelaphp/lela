//! The header and cookie natives of `Z/ext/standard/head.c`,
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

use php_ext::coerce::{coerce_param, to_long, to_string, truthy};
use php_ext::decl::{ConstExpr, FunctionDecl, Parameter, ParameterDefault, Type};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{ArrayOr, array_or_long, given, optional_string};
use crate::{argument_count_between, declared, long_argument, parameter, string_argument};

/// The seven, in the stub's order but the two cookie natives last.
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
        default: ParameterDefault::Expr(ConstExpr::Null),
        ..parameter(name, Type::Mixed)
    };
    vec![
        (
            declared(
                b"header",
                &[
                    parameter(b"header", Type::String),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Bool(true)),
                        ..parameter(b"replace", Type::Bool)
                    },
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Long(0)),
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
                    default: ParameterDefault::Expr(ConstExpr::Null),
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
                    default: ParameterDefault::Expr(ConstExpr::Long(0)),
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
        (
            stub(
                "setcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
            ),
            setcookie as Entry,
        ),
        (
            stub(
                "setrawcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
            ),
            setrawcookie as Entry,
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

/// `ILLEGAL_COOKIE_CHARACTER` (`Z/ext/standard/head.c:81`), the list the
/// refusals name.
const ILLEGAL_COOKIE_CHARACTER: &str =
    "\",\", \";\", \" \", \"\\t\", \"\\r\", \"\\n\", \"\\013\", or \"\\014\"";

/// Whether the C string `s` holds a byte of `set`, `strpbrk`'s answer.
fn strpbrk(s: &[u8], set: &[u8]) -> bool {
    s.iter().take_while(|c| **c != 0).any(|c| set.contains(c))
}

/// `php_format_date("D, d M Y H:i:s \G\M\T", t, 0)`: the time in UTC as
/// a cookie's `expires` writes it.
fn cookie_date(t: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = DAYS
        .get(usize::try_from(days.rem_euclid(7)).unwrap_or(0))
        .copied()
        .unwrap_or("Thu");
    let month = MONTHS
        .get(usize::try_from(month - 1).unwrap_or(0))
        .copied()
        .unwrap_or("Jan");
    format!(
        "{weekday}, {day:02} {month} {year:04} {:02}:{:02}:{:02} GMT",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// What `php_setcookie` is given past the name and value.
#[derive(Default)]
struct Cookie {
    expires: i64,
    path: Option<Vec<u8>>,
    domain: Option<Vec<u8>>,
    secure: bool,
    httponly: bool,
    samesite: Option<Vec<u8>>,
    partitioned: bool,
}

/// `php_setcookie` (`Z/ext/standard/head.c:83`): php's refusals, then the
/// `Set-Cookie` line, a deleted cookie for an empty value, the value
/// `rawurlencode`d unless raw, the expiry and `Max-Age`, and the
/// attributes, added to the headers as `sapi_header_op` adds one.
fn php_setcookie(
    req: &mut Request,
    name: &[u8],
    value: Option<&[u8]>,
    cookie: &Cookie,
    url_encode: bool,
) -> Result<bool, Thrown> {
    let refuse = |req: &mut Request, text: String| Err(throw(req, "ValueError", &text));
    let function = if url_encode {
        "setcookie"
    } else {
        "setrawcookie"
    };
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if strpbrk(name, b"=,; \t\r\n\x0b\x0c") {
        let text = format!(
            "{function}(): Argument #1 ($name) cannot contain \"=\", {ILLEGAL_COOKIE_CHARACTER}"
        );
        return refuse(req, text);
    }
    if !url_encode && value.is_some_and(|v| strpbrk(v, b",; \t\r\n\x0b\x0c")) {
        let text =
            format!("{function}(): Argument #2 ($value) cannot contain {ILLEGAL_COOKIE_CHARACTER}");
        return refuse(req, text);
    }
    for (what, field) in [("path", &cookie.path), ("domain", &cookie.domain)] {
        if field
            .as_deref()
            .is_some_and(|f| strpbrk(f, b",; \t\r\n\x0b\x0c"))
        {
            let text = format!(
                "{function}(): \"{what}\" option cannot contain {ILLEGAL_COOKIE_CHARACTER}"
            );
            return refuse(req, text);
        }
    }
    if cookie.expires >= 253_402_300_800 {
        let text = format!("{function}(): \"expires\" option cannot have a year greater than 9999");
        return refuse(req, text);
    }
    if cookie.partitioned && !cookie.secure {
        let text = format!(
            "{function}(): \"partitioned\" option cannot be used without \"secure\" option"
        );
        return refuse(req, text);
    }
    let mut line = b"Set-Cookie: ".to_vec();
    line.extend_from_slice(name);
    match value.filter(|v| !v.is_empty()) {
        None => {
            line.extend_from_slice(b"=deleted; expires=Thu, 01 Jan 1970 00:00:01 GMT; Max-Age=0");
        }
        Some(value) => {
            line.push(b'=');
            if url_encode {
                crate::url::url_encode(value, true, &mut line);
            } else {
                line.extend_from_slice(value);
            }
            if cookie.expires > 0 {
                line.extend_from_slice(b"; expires=");
                line.extend_from_slice(cookie_date(cookie.expires).as_bytes());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
                let diff = cookie.expires.saturating_sub(now).max(0);
                line.extend_from_slice(format!("; Max-Age={diff}").as_bytes());
            }
        }
    }
    let attributes: [(&[u8], Option<&[u8]>); 2] = [
        (b"; path=", cookie.path.as_deref()),
        (b"; domain=", cookie.domain.as_deref()),
    ];
    for (label, field) in attributes {
        if let Some(field) = field.filter(|f| !f.is_empty()) {
            line.extend_from_slice(label);
            line.extend_from_slice(field);
        }
    }
    if cookie.secure {
        line.extend_from_slice(b"; secure");
    }
    if cookie.httponly {
        line.extend_from_slice(b"; HttpOnly");
    }
    if let Some(samesite) = cookie.samesite.as_deref().filter(|s| !s.is_empty()) {
        line.extend_from_slice(b"; SameSite=");
        line.extend_from_slice(samesite);
    }
    if cookie.partitioned {
        line.extend_from_slice(b"; Partitioned");
    }
    Ok(req.header_add(&line, false, 0))
}

/// `php_head_parse_cookie_options_array` (`Z/ext/standard/head.c:205`).
fn cookie_options(
    req: &mut Request,
    options: PhpArray,
    cookie: &mut Cookie,
    function: &str,
) -> Result<(), Thrown> {
    for (key, value) in options.snapshot() {
        let Key::Str(key) = key else {
            let text = format!("{function}(): option array cannot have numeric keys");
            return Err(throw(req, "ValueError", &text));
        };
        let lower = key.as_bytes().to_ascii_lowercase();
        let string = |req: &mut Request| -> Result<Vec<u8>, Thrown> {
            let text = to_string(req, &value)?;
            let bytes = text.as_bytes().to_vec();
            req.release(Value::string(text));
            Ok(bytes)
        };
        match lower.as_slice() {
            b"expires" => cookie.expires = to_long(req, &value)?,
            b"path" => cookie.path = Some(string(req)?),
            b"domain" => cookie.domain = Some(string(req)?),
            b"secure" => cookie.secure = truthy(req, &value.deref()),
            b"httponly" => cookie.httponly = truthy(req, &value.deref()),
            b"samesite" => cookie.samesite = Some(string(req)?),
            b"partitioned" => cookie.partitioned = truthy(req, &value.deref()),
            _ => {
                let text = format!(
                    "{function}(): option \"{}\" is invalid",
                    String::from_utf8_lossy(key.as_bytes())
                );
                return Err(throw(req, "ValueError", &text));
            }
        }
    }
    Ok(())
}

/// `php_setcookie_common` (`Z/ext/standard/head.c:246`): the name, the
/// value, then either the expiry and the four after it or, alone, an
/// array of options.
fn setcookie_common(req: &mut Request, raw: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 7)?;
    let function = if raw { "setrawcookie" } else { "setcookie" };
    let name = string_argument(req, 1)?.as_bytes().to_vec();
    let value = optional_string(req, 2)?.map(|v| v.as_bytes().to_vec());
    let mut cookie = Cookie::default();
    let options = match given(req, 3) {
        Some(_) => array_or_long(req, 3, false)?,
        None => None,
    };
    let path = optional_string(req, 4)?.map(|v| v.as_bytes().to_vec());
    let domain = optional_string(req, 5)?.map(|v| v.as_bytes().to_vec());
    cookie.path = path;
    cookie.domain = domain;
    if given(req, 6).is_some() {
        cookie.secure = bool_argument(req, 6)?;
    }
    if given(req, 7).is_some() {
        cookie.httponly = bool_argument(req, 7)?;
    }
    match options {
        Some(ArrayOr::Array(options)) => {
            if req.argument_count() > 3 {
                let text = format!(
                    "{function}(): Expects exactly 3 arguments when argument #3 ($expires_or_options) is an array"
                );
                return Err(throw(req, "ArgumentCountError", &text));
            }
            cookie_options(req, options, &mut cookie, function)?;
        }
        Some(ArrayOr::Other(expires)) => cookie.expires = expires,
        None => {}
    }
    let sent = php_setcookie(req, &name, value.as_deref(), &cookie, !raw)?;
    Ok(Value::bool(sent))
}

/// `setcookie(string $name, string $value = "", array|int $expires_or_options = 0, string $path = "", string $domain = "", bool $secure = false, bool $httponly = false): bool`,
/// `PHP_FUNCTION(setcookie)` (`Z/ext/standard/head.c:294`): the value
/// `rawurlencode`d. Under the CLI the line goes nowhere and the answer
/// is `true`; each refusal is php's.
///
/// ```php
/// var_dump(setcookie('a', 'b c'), setcookie('a', '', ['expires' => 5, 'path' => '/', 'SameSite' => 'Lax', 'secure' => true, 'partitioned' => true]));
/// foreach ([[''], ['a=b'], ['a', 'b', ['path' => 'x;y']], ['a', 'b', ['domain' => "x\n"]], ['a', 'b', ['expires' => 253402300800]], ['a', 'b', ['partitioned' => true]], ['a', 'b', ['nope' => 1]], ['a', 'b', [1]]] as $args) {
///     try { call_user_func_array('setcookie', $args); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// try { setcookie('a', 'b', [], '/'); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// setcookie(): Argument #1 ($name) must not be empty
/// setcookie(): Argument #1 ($name) cannot contain "=", ",", ";", " ", "\t", "\r", "\n", "\013", or "\014"
/// setcookie(): "path" option cannot contain ",", ";", " ", "\t", "\r", "\n", "\013", or "\014"
/// setcookie(): "domain" option cannot contain ",", ";", " ", "\t", "\r", "\n", "\013", or "\014"
/// setcookie(): "expires" option cannot have a year greater than 9999
/// setcookie(): "partitioned" option cannot be used without "secure" option
/// setcookie(): option "nope" is invalid
/// setcookie(): option array cannot have numeric keys
/// setcookie(): Expects exactly 3 arguments when argument #3 ($expires_or_options) is an array
/// ```
fn setcookie(req: &mut Request) -> Result<Value, Thrown> {
    setcookie_common(req, false)
}

/// `setrawcookie(...)`, `PHP_FUNCTION(setrawcookie)`
/// (`Z/ext/standard/head.c:302`): the value as it is, and refused where
/// it holds a byte a cookie may not.
///
/// ```php
/// var_dump(setrawcookie('a', 'b%20c'));
/// try { setrawcookie('a', 'b c'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// setrawcookie(): Argument #2 ($value) cannot contain ",", ";", " ", "\t", "\r", "\n", "\013", or "\014"
/// ```
fn setrawcookie(req: &mut Request) -> Result<Value, Thrown> {
    setcookie_common(req, true)
}
