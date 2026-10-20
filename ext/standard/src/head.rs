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

use std::ops::ControlFlow;

use php_ext::abi::{Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{to_long, truthy, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value};

/// `header(string $header, bool $replace = true, int $response_code = 0): void`
/// (`Z/ext/standard/head.c:32`): `sapi_header_op` under `REPLACE` or
/// `ADD`, whose answer the native drops. After the first byte has
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
/// nothing taken, the first of the two it holds named; the response code
/// the third argument carries is taken with the line.
///
/// ```php
/// ob_start(); header("X-A: 1", true, 404); $code = http_response_code(); header("X-A: a\nb");
/// header("X-B: e\0f\ng"); header("X-B: e\rf\0g"); ob_end_flush(); var_dump($code);
/// ```
/// ```expect
///
/// Warning: Header may not contain more than a single header, new line detected in %s on line 2
///
/// Warning: Header may not contain NUL bytes in %s on line 3
///
/// Warning: Header may not contain more than a single header, new line detected in %s on line 3
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
#[php_function]
fn header(
    req: &mut Request,
    header: PhpString,
    #[php(default = true)] replace: bool,
    #[php(default = 0)] response_code: i64,
) {
    req.response_mut()
        .header_add(header.as_bytes(), replace, response_code);
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
#[php_function]
fn header_remove(req: &mut Request, #[php(default = null)] name: Option<PhpString>) {
    req.response_mut()
        .header_remove(name.as_ref().map(PhpString::as_bytes));
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
#[php_function]
fn http_response_code(
    req: &mut Request,
    #[php(default = 0)] response_code: i64,
) -> Union<i64, bool> {
    let code = response_code;
    if code != 0 {
        if req.response().headers_sent().is_some() {
            let message = match req.response().started_at() {
                Some((file, line)) => format!(
                    "Cannot set response code - headers already sent (output started at {}:{line})",
                    String::from_utf8_lossy(&file)
                ),
                None => String::from("Cannot set response code - headers already sent"),
            };
            docref(req, Level::Warning, &message);
            return Union::B(false);
        }
        if req.response().status_line().is_some() {
            docref(
                req,
                Level::Warning,
                "Calling http_response_code() after header('HTTP/...') has no effect",
            );
        }
        let before = req.response().response_code();
        req.response_mut().set_response_code(code);
        return if before != 0 {
            Union::A(before)
        } else {
            Union::B(true)
        };
    }
    let current = req.response().response_code();
    if current == 0 {
        Union::B(false)
    } else {
        Union::A(current)
    }
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
/// echo "x"; headers_sent($f, $l); var_dump(basename($f), $l);
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
#[php_function]
fn headers_sent(
    req: &mut Request,
    #[php(default = null)] filename: Ref<Untyped<Value>>,
    #[php(default = null)] line: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let sent = req.response().headers_sent();
    let (file, at) = sent.clone().unwrap_or_default();
    line.set(req, Untyped(Value::int(i64::from(at))))?;
    if filename.is_given() {
        let file = PhpString::new(req, &file)?;
        filename.set(req, Untyped(file.into_value()))?;
    }
    Ok(sent.is_some())
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
#[php_function]
fn headers_list(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for line in req.response().headers_list() {
        let value = PhpString::new(req, &line)?.into_value();
        array.append(req, value)?;
    }
    Ok(array)
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
    let refuse = |req: &mut Request, text: String| Err(throw_error(req, "ValueError", &text));
    let function = if url_encode {
        "setcookie"
    } else {
        "setrawcookie"
    };
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if strpbrk(name, b"=,; \t\r\n\x0b\x0c") {
        let text = format!("cannot contain \"=\", {ILLEGAL_COOKIE_CHARACTER}");
        return Err(argument_error(req, "ValueError", 1, &text));
    }
    if !url_encode && value.is_some_and(|v| strpbrk(v, b",; \t\r\n\x0b\x0c")) {
        let text = format!("cannot contain {ILLEGAL_COOKIE_CHARACTER}");
        return Err(argument_error(req, "ValueError", 2, &text));
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
    Ok(req.response_mut().header_add(&line, false, 0))
}

/// `php_head_parse_cookie_options_array` (`Z/ext/standard/head.c:205`).
fn cookie_options(
    req: &mut Request,
    options: PhpArray,
    cookie: &mut Cookie,
    function: &str,
) -> Result<(), Thrown> {
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(options, key, value)`
    // (`Z/ext/standard/head.c:204`), no owner of the table taken: the
    // argument holds it while `__toString` and a `ValueError` reach the
    // program.
    walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        let Key::Str(key) = key else {
            let text = format!("{function}(): option array cannot have numeric keys");
            return Err(throw_error(req, "ValueError", &text));
        };
        let lower = key.as_bytes().to_ascii_lowercase();
        let string = |req: &mut Request| with_string(req, &value, <[u8]>::to_vec);
        match lower.as_slice() {
            b"expires" => cookie.expires = to_long(req, &value)?,
            b"path" => cookie.path = Some(string(req)?),
            b"domain" => cookie.domain = Some(string(req)?),
            b"secure" => cookie.secure = truthy(req, &value.deref())?,
            b"httponly" => cookie.httponly = truthy(req, &value.deref())?,
            b"samesite" => cookie.samesite = Some(string(req)?),
            b"partitioned" => cookie.partitioned = truthy(req, &value.deref())?,
            _ => {
                let text = format!(
                    "{function}(): option \"{}\" is invalid",
                    String::from_utf8_lossy(key.as_bytes())
                );
                return Err(throw_error(req, "ValueError", &text));
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `php_setcookie_common` (`Z/ext/standard/head.c:246`): the name, the
/// value, then either the expiry and the four after it or, alone, an
/// array of options.
/// `setcookie` and `setrawcookie`'s seven parameters as they were held.
#[derive(Clone, Copy)]
struct CookieArguments {
    name: PhpString,
    value: PhpString,
    expires_or_options: Union<PhpArray, i64>,
    path: PhpString,
    domain: PhpString,
    secure: bool,
    httponly: bool,
}

fn setcookie_common(
    req: &mut Request,
    arguments: CookieArguments,
    raw: bool,
) -> Result<bool, Thrown> {
    let function = if raw { "setrawcookie" } else { "setcookie" };
    let name = arguments.name.as_bytes().to_vec();
    let value = Some(arguments.value.as_bytes().to_vec());
    let mut cookie = Cookie {
        path: Some(arguments.path.as_bytes().to_vec()),
        domain: Some(arguments.domain.as_bytes().to_vec()),
        secure: arguments.secure,
        httponly: arguments.httponly,
        ..Cookie::default()
    };
    match arguments.expires_or_options {
        Union::A(options) => {
            if req.frames().argument_count() > 3 {
                let text = format!(
                    "{function}(): Expects exactly 3 arguments when argument #3 ($expires_or_options) is an array"
                );
                return Err(throw_error(req, "ArgumentCountError", &text));
            }
            cookie_options(req, options, &mut cookie, function)?;
        }
        Union::B(expires) => cookie.expires = expires,
    }
    php_setcookie(req, &name, value.as_deref(), &cookie, !raw)
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
#[php_function]
fn setcookie(
    req: &mut Request,
    name: PhpString,
    #[php(default = "")] value: PhpString,
    #[php(default = 0)] expires_or_options: Union<PhpArray, i64>,
    #[php(default = "")] path: PhpString,
    #[php(default = "")] domain: PhpString,
    #[php(default = false)] secure: bool,
    #[php(default = false)] httponly: bool,
) -> Result<bool, Thrown> {
    let arguments = CookieArguments {
        name,
        value,
        expires_or_options,
        path,
        domain,
        secure,
        httponly,
    };
    setcookie_common(req, arguments, false)
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
#[php_function]
fn setrawcookie(
    req: &mut Request,
    name: PhpString,
    #[php(default = "")] value: PhpString,
    #[php(default = 0)] expires_or_options: Union<PhpArray, i64>,
    #[php(default = "")] path: PhpString,
    #[php(default = "")] domain: PhpString,
    #[php(default = false)] secure: bool,
    #[php(default = false)] httponly: bool,
) -> Result<bool, Thrown> {
    let arguments = CookieArguments {
        name,
        value,
        expires_or_options,
        path,
        domain,
        secure,
        httponly,
    };
    setcookie_common(req, arguments, true)
}
