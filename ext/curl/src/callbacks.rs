//! What libcurl calls back into during a transfer: `curl_write`,
//! `curl_write_header`, `curl_read`, `curl_progress`, `curl_xferinfo`,
//! `curl_fnmatch`, `curl_debug`, `curl_prereqfunction` and
//! `curl_ssh_hostkeyfunction`, each over the `CurlHandle` the trampoline in
//! [`crate::ffi`] was handed, answering libcurl as php answers it.

use std::ffi::c_int;

use php_ext::coerce::to_long;
use php_ext::raising::throw_error;
use php_ext::request::{Request, Stream};
use php_ext::values::{Owner, PhpObject, PhpString, Value, ValueRef};

use crate::constants as c;
use crate::ffi::{Cx, Hook};
use crate::handle::{Curl, Handler, HeaderTo, ReadFrom, WriteTo, dup, verify_handlers};
use crate::throwables::Throwables;

/// The `CurlHandle`'s callable `pick` names, with `ch` first and `rest`
/// after, through [`call_held`]; the value returned, the `CurlHandle`'s
/// streams verified after it as php verifies them.
fn user(
    cx: &mut Cx<'_>,
    object: PhpObject,
    pick: impl Fn(&Curl) -> Option<&Handler>,
    rest: &[Value],
) -> Option<Owner<Value>> {
    let held = object
        .native::<Curl>(cx.req)
        .and_then(|curl| pick(curl).map(|handler| dup(cx.req, handler)))?;
    let mut arguments = vec![object.as_value()];
    arguments.extend_from_slice(rest);
    let returned = call_held(cx, object, held, &arguments);
    if returned.is_some()
        && let Some(thrown) = verify_handlers(cx.req, object, true)
    {
        cx.raise(thrown);
    }
    returned
}

/// `php_curl_call_callback` (php 8.5.12, `Z/ext/curl/interface.c:523`):
/// `zend_call_known_fcc` with `ch->in_callback` set, the callable's
/// objects held across the call, so a callback that replaces or clears
/// itself runs to its end on an object still alive, and given up while
/// the guard still stands, so a destructor that runs then is refused
/// `curl_reset` and `curl_close` as the callback is; then the guard as it
/// stood before, for a callback inside another.
///
/// ```php
/// class Callback {
///     public string $value = 'alive';
///     function __construct(private CurlHandle $h) {}
///     function write($h, $data) { curl_setopt($h, CURLOPT_WRITEFUNCTION, null); echo $this->value, "\n"; return strlen($data); }
///     function __destruct() {
///         try { curl_reset($this->h); } catch (Error $e) { echo $e->getMessage(), "\n"; }
///         curl_setopt($this->h, CURLOPT_WRITEFUNCTION, static function ($h, $data) { echo "installed\n"; return strlen($data); });
///     }
/// }
/// $f = tempnam(sys_get_temp_dir(), 'cb'); file_put_contents($f, 'x');
/// $h = curl_init("file://$f");
/// curl_setopt($h, CURLOPT_WRITEFUNCTION, [new Callback($h), 'write']);
/// var_dump(curl_exec($h), curl_exec($h));
/// unlink($f);
/// ```
/// ```expect
/// alive
/// curl_reset(): Attempt to reset cURL handle from a callback
/// installed
/// bool(true)
/// bool(true)
/// ```
fn call_held(
    cx: &mut Cx<'_>,
    object: PhpObject,
    held: Handler,
    arguments: &[Value],
) -> Option<Owner<Value>> {
    let was = set_in_callback(cx.req, object, true);
    let returned = cx.call(&held, arguments);
    held.give_up(cx.req);
    set_in_callback(cx.req, object, was);
    returned
}

/// `ch->in_callback = on`, answering what it was.
fn set_in_callback(req: &mut Request, object: PhpObject, on: bool) -> bool {
    object
        .native_mut::<Curl>(req)
        .is_some_and(|curl| std::mem::replace(&mut curl.in_callback, on))
}

/// A string argument of the request's, `ZVAL_STRINGL`; `None` where the
/// memory limit refused it, which is kept for the call to hand back.
fn string(cx: &mut Cx<'_>, bytes: &[u8]) -> Option<Owner<PhpString>> {
    match PhpString::new(cx.req, bytes) {
        Ok(string) => Some(string),
        Err(thrown) => {
            cx.raise(thrown);
            None
        }
    }
}

fn release_all(req: &mut Request, values: Vec<Owner<PhpString>>) {
    for value in values {
        value.give_up(req);
    }
}

/// `php_curl_get_long(&retval)`: the value as `zval_get_long` reads it,
/// the value let go.
fn long(cx: &mut Cx<'_>, value: Owner<Value>) -> i64 {
    let n = to_long(cx.req, &value).unwrap_or(0);
    value.give_up(cx.req);
    n
}

/// `bytes` written to the stream `value` names, as `fwrite` counts them.
fn to_stream(req: &mut Request, value: Option<Value>, bytes: &[u8]) -> usize {
    value
        .and_then(|value| Stream::from_value(req, &value))
        .and_then(|stream| stream.write(req, bytes))
        .unwrap_or(0)
}

/// A user callable handed `ch` and `data`, its answer as a length:
/// `curl_write`'s and `curl_write_header`'s `PHP_CURL_USER`.
fn user_write(cx: &mut Cx<'_>, object: PhpObject, header: bool, data: &[u8]) -> usize {
    let Some(text) = string(cx, data) else {
        return data.len();
    };
    let arguments = [Value::string(*text)];
    let returned = if header {
        user(cx, object, |curl| curl.header.handler.as_ref(), &arguments)
    } else {
        user(cx, object, |curl| curl.write.handler.as_ref(), &arguments)
    };
    text.give_up(cx.req);
    match returned {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Some(value) => long(cx, value) as usize,
        None => data.len(),
    }
}

/// `curl_write`.
pub(crate) fn write(cx: &mut Cx<'_>, object: PhpObject, data: &[u8]) -> usize {
    let Some(curl) = object.native_mut::<Curl>(cx.req) else {
        return data.len();
    };
    match curl.write.to {
        WriteTo::Stdout => {
            cx.req.response_mut().write(data);
            data.len()
        }
        WriteTo::File => {
            let stream = curl.write.stream();
            to_stream(cx.req, stream, data)
        }
        // `CURL_WRITEFUNC_ERROR` where the memory limit refused the
        // room, since 8.5.12; the transfer ends in `CURLE_WRITE_ERROR`.
        WriteTo::Return => match crate::handle::keep(cx.req, object, data) {
            Ok(true) => data.len(),
            Ok(false) => 0xFFFF_FFFF,
            Err(thrown) => {
                cx.raise(thrown);
                0xFFFF_FFFF
            }
        },
        WriteTo::User => user_write(cx, object, false, data),
    }
}

/// `curl_write_header`.
pub(crate) fn write_header(cx: &mut Cx<'_>, object: PhpObject, data: &[u8]) -> usize {
    let Some(curl) = object.native::<Curl>(cx.req) else {
        return data.len();
    };
    match curl.header.to {
        HeaderTo::Ignore => data.len(),
        HeaderTo::File => {
            let stream = curl.header.stream();
            to_stream(cx.req, stream, data)
        }
        HeaderTo::User => user_write(cx, object, true, data),
    }
}

/// `curl_read`: the `CURLOPT_INFILE` stream's bytes, or what the
/// `CURLOPT_READFUNCTION` callable returns, a string copied and an
/// integer answered as the length, `CURL_READFUNC_PAUSE` among them.
pub(crate) fn read(cx: &mut Cx<'_>, object: PhpObject, buffer: &mut [u8]) -> usize {
    let Some(curl) = object.native::<Curl>(cx.req) else {
        return 0;
    };
    let stream = curl.read.stream();
    match curl.read.to {
        ReadFrom::Direct => {
            let Some(stream) = stream.and_then(|value| Stream::from_value(cx.req, &value)) else {
                return 0;
            };
            match stream.read(cx.req, buffer.len()) {
                Ok(Some(read)) => {
                    let bytes = read.as_bytes();
                    let len = bytes.len().min(buffer.len());
                    buffer[..len].copy_from_slice(&bytes[..len]);
                    read.give_up(cx.req);
                    len
                }
                Ok(None) => 0,
                Err(thrown) => {
                    cx.raise(thrown);
                    0
                }
            }
        }
        ReadFrom::User => {
            let resource = stream.and_then(|value| value.as_resource().map(|v| v.as_value()));
            let size = i64::try_from(buffer.len()).unwrap_or(i64::MAX);
            let arguments = [resource.unwrap_or_else(Value::null), Value::int(size)];
            let Some(returned) = user(cx, object, |curl| curl.read.handler.as_ref(), &arguments)
            else {
                return 0;
            };
            let length = match returned.deref().as_ref() {
                ValueRef::Str(text) => {
                    let bytes = text.as_bytes();
                    let len = bytes.len().min(buffer.len());
                    buffer[..len].copy_from_slice(&bytes[..len]);
                    len
                }
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                ValueRef::Int(n) => n as usize,
                _ => 0,
            };
            returned.give_up(cx.req);
            length
        }
    }
}

/// A hook's callable handed `ch` and `rest`, `None` where it is unset or
/// served nothing.
fn hooked(cx: &mut Cx<'_>, object: PhpObject, hook: Hook, rest: &[Value]) -> Option<Owner<Value>> {
    user(cx, object, |curl| curl.hook(hook), rest)
}

/// `curl_progress` and `curl_xferinfo`: 1, which aborts, for any
/// answer but zero.
fn progressed(cx: &mut Cx<'_>, object: PhpObject, hook: Hook, counts: [i64; 4]) -> c_int {
    let arguments = counts.map(Value::int);
    match hooked(cx, object, hook, &arguments) {
        Some(value) => c_int::from(long(cx, value) != 0),
        None => 0,
    }
}

/// `curl_progress`: the four counts as php's `(zend_long)` casts them.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn progress(cx: &mut Cx<'_>, object: PhpObject, counts: [f64; 4]) -> c_int {
    progressed(cx, object, Hook::Progress, counts.map(|n| n as i64))
}

/// `curl_xferinfo`.
pub(crate) fn xferinfo(cx: &mut Cx<'_>, object: PhpObject, counts: [i64; 4]) -> c_int {
    progressed(cx, object, Hook::Xferinfo, counts)
}

/// `curl_fnmatch`: `CURL_FNMATCHFUNC_FAIL` where nothing answers.
pub(crate) fn fnmatch(cx: &mut Cx<'_>, object: PhpObject, pattern: &[u8], string_: &[u8]) -> c_int {
    const FAIL: c_int = 2;
    let (Some(pattern), Some(text)) = (string(cx, pattern), string(cx, string_)) else {
        return FAIL;
    };
    let arguments = [Value::string(*pattern), Value::string(*text)];
    let returned = hooked(cx, object, Hook::Fnmatch, &arguments);
    release_all(cx.req, vec![pattern, text]);
    match returned {
        #[allow(clippy::cast_possible_truncation)]
        Some(value) => long(cx, value) as c_int,
        None => FAIL,
    }
}

/// `curl_debug`: the headers sent kept for `CURLINFO_HEADER_OUT`, then
/// the callable where one is set, its answer dropped.
pub(crate) fn debug(cx: &mut Cx<'_>, object: PhpObject, kind: i64, data: &[u8]) -> c_int {
    let Some(curl) = object.native_mut::<Curl>(cx.req) else {
        return 0;
    };
    if kind == c::CURLINFO_HEADER_OUT {
        curl.header_out = Some(data.to_vec());
    }
    let held = object
        .native::<Curl>(cx.req)
        .and_then(|curl| curl.hook(Hook::Debug).map(|handler| dup(cx.req, handler)));
    let Some(held) = held else {
        return 0;
    };
    let Some(text) = string(cx, data) else {
        held.give_up(cx.req);
        return 0;
    };
    let arguments = [object.as_value(), Value::int(kind), Value::string(*text)];
    let returned = call_held(cx, object, held, &arguments);
    text.give_up(cx.req);
    if let Some(returned) = returned {
        returned.give_up(cx.req);
    }
    0
}

/// `curl_prereqfunction`: `CURL_PREREQFUNC_OK` or `CURL_PREREQFUNC_ABORT`
/// as the callable answers, php's `ValueError` or `TypeError` for
/// anything else.
pub(crate) fn prereq(
    cx: &mut Cx<'_>,
    object: PhpObject,
    (primary, local): (&[u8], &[u8]),
    ports: [c_int; 2],
) -> c_int {
    let (Some(primary), Some(local)) = (string(cx, primary), string(cx, local)) else {
        return 0;
    };
    let arguments = [
        Value::string(*primary),
        Value::string(*local),
        Value::int(i64::from(ports[0])),
        Value::int(i64::from(ports[1])),
    ];
    let returned = hooked(cx, object, Hook::Prereq, &arguments);
    release_all(cx.req, vec![primary, local]);
    let Some(returned) = returned else {
        return 0;
    };
    let text = "The CURLOPT_PREREQFUNCTION callback must return either CURL_PREREQFUNC_OK or CURL_PREREQFUNC_ABORT";
    let answer = match returned.as_ref() {
        #[allow(clippy::cast_possible_truncation)]
        ValueRef::Int(n) if n == c::CURL_PREREQFUNC_OK || n == c::CURL_PREREQFUNC_ABORT => {
            n as c_int
        }
        ValueRef::Int(_) => {
            let thrown = php_ext::raising::value_error(cx.req, text);
            cx.raise(thrown);
            0
        }
        _ => {
            let thrown = php_ext::raising::type_error(cx.req, text);
            cx.raise(thrown);
            0
        }
    };
    returned.give_up(cx.req);
    answer
}

/// `curl_ssh_hostkeyfunction`: `CURLKHMATCH_OK` or `CURLKHMATCH_MISMATCH`
/// as the callable answers, php's `Error` for anything else.
pub(crate) fn ssh_hostkey(cx: &mut Cx<'_>, object: PhpObject, keytype: c_int, key: &[u8]) -> c_int {
    const MISMATCH: c_int = 1;
    let Some(text) = string(cx, key) else {
        return MISMATCH;
    };
    let length = i64::try_from(key.len()).unwrap_or(i64::MAX);
    let arguments = [
        Value::int(i64::from(keytype)),
        Value::string(*text),
        Value::int(length),
    ];
    let returned = hooked(cx, object, Hook::SshHostkey, &arguments);
    text.give_up(cx.req);
    let Some(returned) = returned else {
        return MISMATCH;
    };
    let answer = match returned.as_ref() {
        #[allow(clippy::cast_possible_truncation)]
        ValueRef::Int(n) if n == c::CURLKHMATCH_OK || n == c::CURLKHMATCH_MISMATCH => n as c_int,
        _ => {
            let thrown = match Throwables::of(cx.req) {
                Ok(ce) => throw_error(
                    cx.req,
                    ce.error,
                    "The CURLOPT_SSH_HOSTKEYFUNCTION callback must return either CURLKHMATCH_OK or CURLKHMATCH_MISMATCH",
                ),
                Err(fatal) => fatal,
            };
            cx.raise(thrown);
            MISMATCH
        }
    };
    returned.give_up(cx.req);
    answer
}
