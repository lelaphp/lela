//! The socket natives of `streamsfuncs.c` (`Z/ext/standard/streamsfuncs.c:49-431`,
//! `:567`, `:1497`, `:1690`) and `fsock.c`'s `fsockopen` and
//! `pfsockopen`, over the engine's socket kinds and its
//! `php_stream_xport_create`: a client connected or a server bound
//! through a `transport`, accepted, named, sent to and received from, shut
//! down, and a pair made by `socketpair(2)`, which is `src/ffi/`'s. Beside
//! them are the constants `file.stub.php` declares for these natives and
//! `default_socket_timeout`, `file.c`'s ini entry, which every socket's
//! read timeout and every connect without one of its own read.
//!
//! What is not here: a stream `context`, which `t-opus-5/t680`'s
//! `streamsfuncs.rs` lays down, so `$context` is taken and not read;
//! persistent sockets, which a request-scoped engine has no store for,
//! so `pfsockopen` and `STREAM_CLIENT_PERSISTENT` open a socket of the
//! request's own; and crypto, which is `ext/openssl`'s.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::net::Shutdown;
use std::time::Duration;

use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstDecl, FunctionDecl, IniDecl, Name, Type};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw};
use php_ext::request::{FdStream, Request, Xport, XportFailure};
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

use crate::file::stream_argument;
use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{assign_ref, given, optional_long};
use crate::{argument_count_between, bool_argument, constant, long_argument, string_argument};

/// `PHP_STREAM_CLIENT_*` and `STREAM_XPORT_*` (`Z/main/php_streams.h`,
/// `Z/main/streams/php_stream_transport.h`): what `$flags` holds.
const STREAM_CLIENT_PERSISTENT: i64 = 1;
const STREAM_CLIENT_ASYNC_CONNECT: i64 = 2;
const STREAM_CLIENT_CONNECT: i64 = 4;
const STREAM_SERVER_BIND: i64 = 4;
const STREAM_SERVER_LISTEN: i64 = 8;

/// `STREAM_SHUT_*`, what `stream_socket_shutdown` takes.
const STREAM_SHUT_RD: i64 = 0;
const STREAM_SHUT_WR: i64 = 1;
const STREAM_SHUT_RDWR: i64 = 2;

/// The constants `file.stub.php` declares for the socket natives, with
/// the values this host's `<sys/socket.h>` and php's headers give them.
///
/// ```php
/// var_dump(STREAM_CLIENT_PERSISTENT, STREAM_CLIENT_ASYNC_CONNECT, STREAM_CLIENT_CONNECT, STREAM_CRYPTO_METHOD_ANY_CLIENT, STREAM_CRYPTO_METHOD_TLS_CLIENT, STREAM_CRYPTO_METHOD_TLS_SERVER, STREAM_CRYPTO_PROTO_TLSv1_3, STREAM_SHUT_RD, STREAM_SHUT_WR, STREAM_SHUT_RDWR, STREAM_PF_INET, STREAM_PF_INET6, STREAM_PF_UNIX, STREAM_IPPROTO_IP, STREAM_IPPROTO_TCP, STREAM_IPPROTO_UDP, STREAM_IPPROTO_ICMP, STREAM_IPPROTO_RAW, STREAM_SOCK_STREAM, STREAM_SOCK_DGRAM, STREAM_SOCK_RAW, STREAM_SOCK_SEQPACKET, STREAM_SOCK_RDM, STREAM_PEEK, STREAM_OOB, STREAM_SERVER_BIND, STREAM_SERVER_LISTEN, ini_get('default_socket_timeout'));
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(4)
/// int(127)
/// int(121)
/// int(120)
/// int(64)
/// int(0)
/// int(1)
/// int(2)
/// int(2)
/// int(30)
/// int(1)
/// int(0)
/// int(6)
/// int(17)
/// int(1)
/// int(255)
/// int(1)
/// int(2)
/// int(3)
/// int(5)
/// int(4)
/// int(2)
/// int(1)
/// int(4)
/// int(8)
/// string(2) "60"
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    vec![
        constant(b"STREAM_CLIENT_PERSISTENT", STREAM_CLIENT_PERSISTENT),
        constant(b"STREAM_CLIENT_ASYNC_CONNECT", STREAM_CLIENT_ASYNC_CONNECT),
        constant(b"STREAM_CLIENT_CONNECT", STREAM_CLIENT_CONNECT),
        constant(b"STREAM_CRYPTO_METHOD_ANY_CLIENT", 127),
        constant(b"STREAM_CRYPTO_METHOD_SSLv2_CLIENT", 3),
        constant(b"STREAM_CRYPTO_METHOD_SSLv3_CLIENT", 5),
        constant(b"STREAM_CRYPTO_METHOD_SSLv23_CLIENT", 57),
        constant(b"STREAM_CRYPTO_METHOD_TLS_CLIENT", 121),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_0_CLIENT", 9),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_1_CLIENT", 17),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_2_CLIENT", 33),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_3_CLIENT", 65),
        constant(b"STREAM_CRYPTO_METHOD_ANY_SERVER", 126),
        constant(b"STREAM_CRYPTO_METHOD_SSLv2_SERVER", 2),
        constant(b"STREAM_CRYPTO_METHOD_SSLv3_SERVER", 4),
        constant(b"STREAM_CRYPTO_METHOD_SSLv23_SERVER", 120),
        constant(b"STREAM_CRYPTO_METHOD_TLS_SERVER", 120),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_0_SERVER", 8),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_1_SERVER", 16),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_2_SERVER", 32),
        constant(b"STREAM_CRYPTO_METHOD_TLSv1_3_SERVER", 64),
        constant(b"STREAM_CRYPTO_PROTO_SSLv3", 4),
        constant(b"STREAM_CRYPTO_PROTO_TLSv1_0", 8),
        constant(b"STREAM_CRYPTO_PROTO_TLSv1_1", 16),
        constant(b"STREAM_CRYPTO_PROTO_TLSv1_2", 32),
        constant(b"STREAM_CRYPTO_PROTO_TLSv1_3", 64),
        constant(b"STREAM_SHUT_RD", STREAM_SHUT_RD),
        constant(b"STREAM_SHUT_WR", STREAM_SHUT_WR),
        constant(b"STREAM_SHUT_RDWR", STREAM_SHUT_RDWR),
        constant(b"STREAM_PF_INET", 2),
        constant(b"STREAM_PF_INET6", 30),
        constant(b"STREAM_PF_UNIX", 1),
        constant(b"STREAM_IPPROTO_IP", 0),
        constant(b"STREAM_IPPROTO_TCP", 6),
        constant(b"STREAM_IPPROTO_UDP", 17),
        constant(b"STREAM_IPPROTO_ICMP", 1),
        constant(b"STREAM_IPPROTO_RAW", 255),
        constant(b"STREAM_SOCK_STREAM", 1),
        constant(b"STREAM_SOCK_DGRAM", 2),
        constant(b"STREAM_SOCK_RAW", 3),
        constant(b"STREAM_SOCK_SEQPACKET", 5),
        constant(b"STREAM_SOCK_RDM", 4),
        constant(b"STREAM_PEEK", 2),
        constant(b"STREAM_OOB", 1),
        constant(b"STREAM_SERVER_BIND", STREAM_SERVER_BIND),
        constant(b"STREAM_SERVER_LISTEN", STREAM_SERVER_LISTEN),
    ]
}

/// `default_socket_timeout`, `PHP_INI_ALL` with php's sixty seconds
/// (`Z/ext/standard/file.c:139`), which a socket opened after it changes
/// takes as its read timeout.
///
/// ```php
/// var_dump(ini_get('default_socket_timeout'), ini_set('default_socket_timeout', '1'), ini_get('default_socket_timeout'));
/// $pair = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
/// var_dump(fread($pair[0], 1), stream_get_meta_data($pair[0])['timed_out']);
/// ```
/// ```expect
/// string(2) "60"
/// string(2) "60"
/// string(1) "1"
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn ini() -> Vec<IniDecl> {
    vec![IniDecl {
        name: Name(b"default_socket_timeout".as_slice().into()),
        default: b"60".as_slice().into(),
        user: true,
        per_dir: true,
        system: true,
    }]
}

/// The natives, as `basic_functions.stub.php` declares them.
///
/// ```php
/// foreach (['fsockopen', 'stream_socket_client', 'stream_socket_server', 'stream_socket_accept', 'stream_socket_get_name', 'stream_socket_recvfrom', 'stream_socket_sendto', 'stream_socket_enable_crypto', 'stream_socket_shutdown', 'stream_socket_pair', 'stream_get_transports'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function fsockopen ] {
///
///   - Parameters [5] {
///     Parameter #0 [ <required> string $hostname ]
///     Parameter #1 [ <optional> int $port = -1 ]
///     Parameter #2 [ <optional> &$error_code = null ]
///     Parameter #3 [ <optional> &$error_message = null ]
///     Parameter #4 [ <optional> ?float $timeout = null ]
///   }
/// }
/// Function [ <internal:standard> function stream_socket_client ] {
///
///   - Parameters [6] {
///     Parameter #0 [ <required> string $address ]
///     Parameter #1 [ <optional> &$error_code = null ]
///     Parameter #2 [ <optional> &$error_message = null ]
///     Parameter #3 [ <optional> ?float $timeout = null ]
///     Parameter #4 [ <optional> int $flags = STREAM_CLIENT_CONNECT ]
///     Parameter #5 [ <optional> $context = null ]
///   }
/// }
/// Function [ <internal:standard> function stream_socket_server ] {
///
///   - Parameters [5] {
///     Parameter #0 [ <required> string $address ]
///     Parameter #1 [ <optional> &$error_code = null ]
///     Parameter #2 [ <optional> &$error_message = null ]
///     Parameter #3 [ <optional> int $flags = STREAM_SERVER_BIND | STREAM_SERVER_LISTEN ]
///     Parameter #4 [ <optional> $context = null ]
///   }
/// }
/// Function [ <internal:standard> function stream_socket_accept ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> $socket ]
///     Parameter #1 [ <optional> ?float $timeout = null ]
///     Parameter #2 [ <optional> &$peer_name = null ]
///   }
/// }
/// Function [ <internal:standard> function stream_socket_get_name ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $socket ]
///     Parameter #1 [ <required> bool $remote ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function stream_socket_recvfrom ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> $socket ]
///     Parameter #1 [ <required> int $length ]
///     Parameter #2 [ <optional> int $flags = 0 ]
///     Parameter #3 [ <optional> &$address = null ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function stream_socket_sendto ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> $socket ]
///     Parameter #1 [ <required> string $data ]
///     Parameter #2 [ <optional> int $flags = 0 ]
///     Parameter #3 [ <optional> string $address = "" ]
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:standard> function stream_socket_enable_crypto ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> bool $enable ]
///     Parameter #2 [ <optional> ?int $crypto_method = null ]
///     Parameter #3 [ <optional> $session_stream = null ]
///   }
///   - Return [ int|bool ]
/// }
/// Function [ <internal:standard> function stream_socket_shutdown ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> int $mode ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function stream_socket_pair ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> int $domain ]
///     Parameter #1 [ <required> int $type ]
///     Parameter #2 [ <required> int $protocol ]
///   }
///   - Return [ array|false ]
/// }
/// Function [ <internal:standard> function stream_get_transports ] {
///
///   - Parameters [0] {
///   }
///   - Return [ array ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            stub(
                "fsockopen(string $hostname, int $port = -1, &$error_code = null, &$error_message = null, ?float $timeout = null)",
            ),
            fsockopen as Entry,
        ),
        (
            stub(
                "pfsockopen(string $hostname, int $port = -1, &$error_code = null, &$error_message = null, ?float $timeout = null)",
            ),
            pfsockopen,
        ),
        (
            stub(
                "stream_socket_client(string $address, &$error_code = null, &$error_message = null, ?float $timeout = null, int $flags = STREAM_CLIENT_CONNECT, $context = null)",
            ),
            stream_socket_client,
        ),
        (
            stub(
                "stream_socket_server(string $address, &$error_code = null, &$error_message = null, int $flags = STREAM_SERVER_BIND | STREAM_SERVER_LISTEN, $context = null)",
            ),
            stream_socket_server,
        ),
        (
            stub("stream_socket_accept($socket, ?float $timeout = null, &$peer_name = null)"),
            stream_socket_accept,
        ),
        (
            stub("stream_socket_get_name($socket, bool $remote): string|false"),
            stream_socket_get_name,
        ),
        (
            stub(
                "stream_socket_recvfrom($socket, int $length, int $flags = 0, &$address = null): string|false",
            ),
            stream_socket_recvfrom,
        ),
        (
            stub(
                "stream_socket_sendto($socket, string $data, int $flags = 0, string $address = \"\"): int|false",
            ),
            stream_socket_sendto,
        ),
        (
            stub(
                "stream_socket_enable_crypto($stream, bool $enable, ?int $crypto_method = null, $session_stream = null): int|bool",
            ),
            stream_socket_enable_crypto,
        ),
        (
            stub("stream_socket_shutdown($stream, int $mode): bool"),
            stream_socket_shutdown,
        ),
        (
            stub("stream_socket_pair(int $domain, int $type, int $protocol): array|false"),
            stream_socket_pair,
        ),
        (
            stub("stream_get_transports(): array"),
            stream_get_transports,
        ),
    ]
}

/// `Z_PARAM_DOUBLE_OR_NULL`: `None` for `null` or no argument.
fn nullable_double(req: &mut Request, position: u32) -> Result<Option<f64>, Thrown> {
    match given(req, position) {
        Some(value) if !matches!(value.deref().as_ref(), ValueRef::Null) => {
            let coerced = coerce_param(
                req,
                &value,
                &Type::Nullable(Box::new(Type::Float)),
                position,
            )?;
            Ok(coerced.as_float())
        }
        _ => Ok(None),
    }
}

/// `FG(default_socket_timeout)` as a double of seconds, what a timeout
/// of `null` is.
fn default_timeout(req: &Request) -> f64 {
    req.ini_get(b"default_socket_timeout")
        .and_then(|text| String::from_utf8_lossy(text).trim().parse::<i64>().ok())
        .map_or(60.0, |secs| {
            #[allow(clippy::cast_precision_loss)] // php's `(double)FG(default_socket_timeout)`
            let secs = secs as f64;
            secs
        })
}

/// `PHP_TIMEOUT_ULL_MAX / 1000000.0`, the most seconds a timeout holds.
const TIMEOUT_MAX: f64 = 18_446_744_073_709.55;

/// A timeout of seconds as the natives turn it into a `timeval`: `None`,
/// no limit, below zero and at or past the most a timeout holds, and the
/// microseconds `(php_timeout_ull)(timeout * 1000000.0)` keeps otherwise.
fn timeval(timeout: f64) -> Option<Duration> {
    if !(0.0..TIMEOUT_MAX).contains(&timeout) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // C's conversion to `unsigned long long`
    let micros = (timeout * 1_000_000.0) as u64;
    Some(Duration::from_micros(micros))
}

/// `php_addslashes`: `'`, `"`, `\` and the null byte escaped, as the
/// natives print a host that may hold binary bytes.
fn addslashes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            0 => out.extend_from_slice(b"\\0"),
            b'\'' | b'"' | b'\\' => {
                out.push(b'\\');
                out.push(b);
            }
            _ => out.push(b),
        }
    }
    out
}

/// A string of the request's as a value.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// `ZEND_TRY_ASSIGN_REF_LONG` and `ZEND_TRY_ASSIGN_REF_STR` of the two
/// by-reference slots `$error_code` and `$error_message`, where the call
/// gave them.
fn assign_error(
    req: &mut Request,
    at: (u32, u32),
    code: i64,
    text: Option<&[u8]>,
) -> Result<(), Thrown> {
    if req.argument_count() >= at.0 as usize {
        assign_ref(req, at.0, Value::int(code))?;
    }
    if let Some(text) = text
        && req.argument_count() >= at.1 as usize
    {
        let value = string_value(req, text)?;
        assign_ref(req, at.1, value)?;
    }
    Ok(())
}

/// `fsockopen(string $hostname, int $port = -1, &$error_code = null,
/// &$error_message = null, ?float $timeout = null)` and `pfsockopen`,
/// `php_fsockopen_stream` (`Z/ext/standard/fsock.c:50`): a `port` above
/// zero put after the host as `host:port`; the timeout, `-1` or one
/// between zero and the most a `timeval` holds; then a client connected
/// through the `transport` the name gives, `tcp` where it gives none. A
/// connect that fails is php's `Unable to connect to %s:%d (%s)`, with
/// the host and the `port` as they were given, and its code and text in the
/// two by-reference slots; one that succeeds clears them.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $port = (int) substr(stream_socket_get_name($s, false), strlen('127.0.0.1:'));
/// $c = fsockopen('127.0.0.1', $port, $code, $message, 1);
/// var_dump(is_resource($c), $code, $message, fwrite($c, "hi"), fread(stream_socket_accept($s), 2));
/// var_dump(fsockopen('127.0.0.1', 1, $code, $message, 1), $code, $message);
/// var_dump(is_resource(pfsockopen('udp://127.0.0.1', 9, $code)), $code);
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// string(0) ""
/// int(2)
/// string(2) "hi"
///
/// Warning: fsockopen(): Unable to connect to 127.0.0.1:1 (Connection refused) in %s on line 6
/// bool(false)
/// int(61)
/// string(18) "Connection refused"
/// bool(true)
/// int(0)
/// ```
fn fsockopen(req: &mut Request) -> Result<Value, Thrown> {
    fsockopen_stream(req, "fsockopen")
}

/// `pfsockopen(...)`, `fsockopen`'s persistent twin: a request here has
/// no store a socket outlives it in, so the socket is the request's own,
/// as `fsockopen`'s is.
fn pfsockopen(req: &mut Request) -> Result<Value, Thrown> {
    fsockopen_stream(req, "pfsockopen")
}

/// `php_fsockopen_stream` for `native`, the name its `ValueError` prints.
fn fsockopen_stream(req: &mut Request, native: &str) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 5)?;
    let host = string_argument(req, 1)?.as_bytes().to_vec();
    let port = optional_long(req, 2, -1)?;
    let timeout = match nullable_double(req, 5)? {
        Some(timeout) => timeout,
        None => default_timeout(req),
    };
    #[allow(clippy::float_cmp)] // php's `timeout != -1.0`
    let limited = timeout != -1.0;
    if limited && !(0.0..=TIMEOUT_MAX).contains(&timeout) {
        let text = format!("{native}(): Argument #6 must be -1 or between 0 and {TIMEOUT_MAX}");
        return Err(throw(req, "ValueError", &text));
    }
    let mut hostname = host.clone();
    if port > 0 {
        hostname.extend_from_slice(format!(":{port}").as_bytes());
    }
    let timeout = if limited { timeval(timeout) } else { None };
    match req.xport_create(&hostname, Xport::Client { timeout })? {
        Ok(stream) => {
            assign_error(req, (3, 4), 0, Some(b""))?;
            Ok(stream.as_value())
        }
        Err(XportFailure { code, text }) => {
            let shown = format!(
                "Unable to connect to {}:{port} ({})",
                String::from_utf8_lossy(&host),
                if text.is_empty() {
                    "Unknown error"
                } else {
                    &text
                }
            );
            docref(req, Level::Warning, &shown);
            let text = (!text.is_empty()).then_some(text.as_bytes());
            assign_error(req, (3, 4), i64::from(code), text)?;
            Ok(Value::bool(false))
        }
    }
}

/// `stream_socket_client(string $address, &$error_code = null,
/// &$error_message = null, ?float $timeout = null, int $bits =
/// STREAM_CLIENT_CONNECT, $`context` = null)`,
/// `PHP_FUNCTION(stream_socket_client)` (`Z/ext/standard/streamsfuncs.c:97`):
/// a timeout that is no finite number is php's `ValueError`, `null` is
/// `default_socket_timeout`; the two by-reference slots cleared, then a
/// client connected through the `transport` the address names, and a
/// connect that fails php's `Unable to connect to %s (%s)` with the
/// address escaped as `addslashes` escapes it, its code and text in the
/// slots.
///
/// ```php
/// $path = '/tmp/' . uniqid('w') . '.sock';
/// $s = stream_socket_server("unix://$path");
/// $c = stream_socket_client("unix://$path", $code, $message);
/// var_dump($code, $message, stream_get_meta_data($c)); unlink($path);
/// var_dump(stream_socket_client('foo://x', $code, $message), $message);
/// var_dump(stream_socket_client('tcp://127.0.0.1:1', $code, $message, 1), $code, $message);
/// try { stream_socket_client('tcp://127.0.0.1:1', $code, $message, INF); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// string(0) ""
/// array(8) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["stream_type"]=>
///   string(11) "unix_socket"
///   ["mode"]=>
///   string(2) "r+"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(false)
///   ["uri"]=>
///   string(%d) "unix:///tmp/%s.sock"
/// }
///
/// Warning: stream_socket_client(): Unable to connect to foo://x (Unable to find the socket transport "foo" - did you forget to enable it when you configured PHP?) in %s on line 6
/// bool(false)
/// string(96) "Unable to find the socket transport "foo" - did you forget to enable it when you configured PHP?"
///
/// Warning: stream_socket_client(): Unable to connect to tcp://127.0.0.1:1 (Connection refused) in %s on line 7
/// bool(false)
/// int(61)
/// string(18) "Connection refused"
/// stream_socket_client(): Argument #4 ($timeout) must be a finite value
/// ```
fn stream_socket_client(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 6)?;
    let host = string_argument(req, 1)?.as_bytes().to_vec();
    let timeout = match nullable_double(req, 4)? {
        Some(timeout) if !timeout.is_finite() => {
            return Err(arg_value_error(req, 4, "be a finite value"));
        }
        Some(timeout) => timeout,
        None => default_timeout(req),
    };
    optional_long(req, 5, STREAM_CLIENT_CONNECT)?;
    assign_error(req, (2, 3), 0, Some(b""))?;
    match req.xport_create(
        &host,
        Xport::Client {
            timeout: timeval(timeout),
        },
    )? {
        Ok(stream) => Ok(stream.as_value()),
        Err(XportFailure { code, text }) => {
            let shown = format!(
                "Unable to connect to {} ({})",
                String::from_utf8_lossy(&addslashes(&host)),
                if text.is_empty() {
                    "Unknown error"
                } else {
                    &text
                }
            );
            docref(req, Level::Warning, &shown);
            let text = (!text.is_empty()).then_some(text.as_bytes());
            assign_error(req, (2, 3), i64::from(code), text)?;
            Ok(Value::bool(false))
        }
    }
}

/// `stream_socket_server(string $address, &$error_code = null,
/// &$error_message = null, int $bits = STREAM_SERVER_BIND |
/// STREAM_SERVER_LISTEN, $`context` = null)`,
/// `PHP_FUNCTION(stream_socket_server)` (`Z/ext/standard/streamsfuncs.c:201`):
/// the slots cleared, then a server bound through the `transport` the
/// address names, listening where `$flags` says; one that fails is
/// php's `Unable to connect to %s (%s)`, `Unknown error` where the bind
/// left no text, as a unix socket's does.
///
/// ```php
/// $path = '/tmp/' . uniqid('w') . '.sock';
/// $s = stream_socket_server("udg://$path", $code, $message, STREAM_SERVER_BIND);
/// var_dump($code, $message, stream_get_meta_data($s)['stream_type']); unlink($path);
/// var_dump(stream_socket_server('tcp://127.0.0.1', $code, $message), $code, $message);
/// var_dump(stream_socket_server('unix:///nonexistent/dir/sock', $code, $message), $code, $message);
/// ```
/// ```expect
/// int(0)
/// string(0) ""
/// string(10) "udg_socket"
///
/// Warning: stream_socket_server(): Unable to connect to tcp://127.0.0.1 (Failed to parse address "127.0.0.1") in %s on line 5
/// bool(false)
/// int(0)
/// string(35) "Failed to parse address "127.0.0.1""
///
/// Warning: stream_socket_server(): Unable to connect to unix:///nonexistent/dir/sock (Unknown error) in %s on line 6
/// bool(false)
/// int(0)
/// string(0) ""
/// ```
fn stream_socket_server(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 5)?;
    let host = string_argument(req, 1)?.as_bytes().to_vec();
    let flags = optional_long(req, 4, STREAM_SERVER_BIND | STREAM_SERVER_LISTEN)?;
    assign_error(req, (2, 3), 0, Some(b""))?;
    let xport = Xport::Server {
        listen: flags & STREAM_SERVER_LISTEN != 0,
    };
    match req.xport_create(&host, xport)? {
        Ok(stream) => Ok(stream.as_value()),
        Err(XportFailure { code, text }) => {
            let shown = format!(
                "Unable to connect to {} ({})",
                String::from_utf8_lossy(&host),
                if text.is_empty() {
                    "Unknown error"
                } else {
                    &text
                }
            );
            docref(req, Level::Warning, &shown);
            let text = (!text.is_empty()).then_some(text.as_bytes());
            assign_error(req, (2, 3), i64::from(code), text)?;
            Ok(Value::bool(false))
        }
    }
}

/// `stream_socket_accept($socket, ?float $timeout = null, &$peer_name =
/// null)`, `PHP_FUNCTION(stream_socket_accept)`
/// (`Z/ext/standard/streamsfuncs.c:261`): the next client within the
/// timeout, `default_socket_timeout` for `null`, as a stream, and its
/// address in `$peer_name` where it is given; an accept that fails or
/// runs out of time is php's `Accept failed: %s`.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $c = stream_socket_client('tcp://' . stream_socket_get_name($s, false));
/// $a = stream_socket_accept($s, 1, $peer);
/// var_dump($peer === stream_socket_get_name($c, false), fwrite($c, "ping\n"), fgets($a));
/// var_dump(stream_socket_accept($s, 0.1), stream_socket_accept(fopen(__FILE__, 'r'), 0));
/// ```
/// ```expect
/// bool(true)
/// int(5)
/// string(5) "ping
/// "
///
/// Warning: stream_socket_accept(): Accept failed: Operation timed out in %s on line 6
///
/// Warning: stream_socket_accept(): Accept failed: Unknown error in %s on line 6
/// bool(false)
/// bool(false)
/// ```
fn stream_socket_accept(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let stream = stream_argument(req, 1)?;
    let timeout = match nullable_double(req, 2)? {
        Some(timeout) if !timeout.is_finite() => {
            return Err(arg_value_error(req, 2, "be a finite value"));
        }
        Some(timeout) => timeout,
        None => default_timeout(req),
    };
    let want_peer = req.argument_count() >= 3;
    match stream.accept(req, timeval(timeout), want_peer)? {
        Ok((client, peer)) => {
            if let Some(peer) = peer {
                assign_ref(req, 3, Value::string(peer))?;
            }
            Ok(client.as_value())
        }
        Err(text) => {
            let text = format!(
                "Accept failed: {}",
                text.as_deref().unwrap_or("Unknown error")
            );
            docref(req, Level::Warning, &text);
            Ok(Value::bool(false))
        }
    }
}

/// `stream_socket_get_name($socket, bool $remote): string|false`,
/// `PHP_FUNCTION(stream_socket_get_name)`
/// (`Z/ext/standard/streamsfuncs.c:326`): the socket's own address, or its
/// peer's, `ip:port` for an inet socket and the path for a unix one;
/// `false` where it has none, an unnamed socket's empty name among them,
/// and for a stream that is no socket.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// var_dump(str_starts_with(stream_socket_get_name($s, false), '127.0.0.1:'), stream_socket_get_name($s, true));
/// $pair = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
/// var_dump(stream_socket_get_name($pair[0], false), stream_socket_get_name(fopen(__FILE__, 'r'), false));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn stream_socket_get_name(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let peer = bool_argument(req, 2)?;
    Ok(stream
        .socket_name(req, peer)?
        .map_or(Value::bool(false), Value::string))
}

/// `stream_socket_recvfrom($socket, int $length, int $flags = 0,
/// &$address = null): string|false`,
/// `PHP_FUNCTION(stream_socket_recvfrom)`
/// (`Z/ext/standard/streamsfuncs.c:384`): `$address` cleared, a length
/// above zero, then the socket's `recvfrom(2)` past the read buffer, the
/// bytes left queued under `STREAM_PEEK`, and the sender's address in
/// `$address`.
///
/// ```php
/// $s = stream_socket_server('udp://127.0.0.1:0', $code, $message, STREAM_SERVER_BIND);
/// $c = stream_socket_client('udp://' . stream_socket_get_name($s, false));
/// var_dump(stream_socket_sendto($c, "one"), stream_socket_recvfrom($s, 10, STREAM_PEEK, $from), stream_socket_recvfrom($s, 10, 0, $from2));
/// var_dump($from === stream_socket_get_name($c, false), $from2 === $from);
/// try { stream_socket_recvfrom($s, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// string(3) "one"
/// string(3) "one"
/// bool(true)
/// bool(true)
/// stream_socket_recvfrom(): Argument #2 ($length) must be greater than 0
/// ```
fn stream_socket_recvfrom(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let stream = stream_argument(req, 1)?;
    let to_read = long_argument(req, 2)?;
    let flags = optional_long(req, 3, 0)?;
    let want_addr = req.argument_count() >= 4;
    if want_addr {
        assign_ref(req, 4, Value::null())?;
    }
    let Ok(len) = usize::try_from(to_read) else {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    };
    if len == 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    match stream.recvfrom(req, len, flags, want_addr)? {
        Some((bytes, from)) => {
            if let Some(from) = from {
                assign_ref(req, 4, Value::string(from))?;
            }
            Ok(Value::string(bytes))
        }
        None => Ok(Value::bool(false)),
    }
}

/// `stream_socket_sendto($socket, string $data, int $flags = 0, string
/// $address = ""): int|false`, `PHP_FUNCTION(stream_socket_sendto)`
/// (`Z/ext/standard/streamsfuncs.c:354`): an address that is given must be
/// `ip:port`, php's warning and `false` where it is not; then the
/// socket's `sendto(2)`, and the count.
///
/// ```php
/// $s = stream_socket_server('udp://127.0.0.1:0', $code, $message, STREAM_SERVER_BIND);
/// $c = stream_socket_server('udp://127.0.0.1:0', $code, $message, STREAM_SERVER_BIND);
/// var_dump(stream_socket_sendto($c, "to", 0, stream_socket_get_name($s, false)), fread($s, 10));
/// var_dump(stream_socket_sendto($c, "x", 0, 'nowhere'));
/// ```
/// ```expect
/// int(2)
/// string(2) "to"
///
/// Warning: stream_socket_sendto(): Failed to parse `nowhere' into a valid network address in %s on line 5
/// bool(false)
/// ```
fn stream_socket_sendto(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let stream = stream_argument(req, 1)?;
    let data = string_argument(req, 2)?.as_bytes().to_vec();
    let flags = optional_long(req, 3, 0)?;
    let address = if req.argument_count() >= 4 {
        string_argument(req, 4)?.as_bytes().to_vec()
    } else {
        Vec::new()
    };
    let to = (!address.is_empty()).then_some(address.as_slice());
    Ok(stream
        .sendto(req, &data, flags, to)
        .map_or(Value::bool(false), Value::int))
}

/// `stream_socket_enable_crypto($stream, bool $enable, ?int
/// $crypto_method = null, $session_stream = null): int|bool`,
/// `PHP_FUNCTION(stream_socket_enable_crypto)`
/// (`Z/ext/standard/streamsfuncs.c:1497`): enabling with no method is
/// php's `ValueError`, since no `context` gives one; a stream here has no
/// crypto layer, which is `ext/openssl`'s, so setting it up is php's
/// warning and `false`, and turning it off is the same warning and
/// `true`, php's answer for `PHP_STREAM_OPTION_RETURN_NOTIMPL`.
///
/// ```php
/// $f = fopen(__FILE__, 'r');
/// var_dump(stream_socket_enable_crypto($f, false), stream_socket_enable_crypto($f, true, STREAM_CRYPTO_METHOD_TLS_CLIENT));
/// try { stream_socket_enable_crypto($f, true); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: stream_socket_enable_crypto(): This stream does not support SSL/crypto in %s on line 3
///
/// Warning: stream_socket_enable_crypto(): This stream does not support SSL/crypto in %s on line 3
/// bool(true)
/// bool(false)
/// stream_socket_enable_crypto(): Argument #3 ($crypto_method) must be specified when enabling encryption
/// ```
fn stream_socket_enable_crypto(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    stream_argument(req, 1)?;
    let enable = bool_argument(req, 2)?;
    let method = crate::zpp::nullable_long(req, 3)?;
    if req.argument_count() >= 4
        && !matches!(
            req.argument(4).unwrap_or_default().deref().as_ref(),
            ValueRef::Null
        )
    {
        stream_argument(req, 4)?;
    }
    if enable && method.is_none() {
        return Err(arg_value_error(
            req,
            3,
            "be specified when enabling encryption",
        ));
    }
    docref(
        req,
        Level::Warning,
        "This stream does not support SSL/crypto",
    );
    Ok(Value::bool(!enable))
}

/// `stream_socket_shutdown($stream, int $mode): bool`,
/// `PHP_FUNCTION(stream_socket_shutdown)`
/// (`Z/ext/standard/streamsfuncs.c:1690`): one of the three
/// `STREAM_SHUT_*`, php's `ValueError` otherwise, and whether
/// `shutdown(2)` of the socket answered `0`.
///
/// ```php
/// $pair = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0); $a = $pair[0]; $b = $pair[1];
/// var_dump(fwrite($a, "last"), stream_socket_shutdown($a, STREAM_SHUT_WR), fread($b, 10), fread($b, 10), feof($b), stream_socket_shutdown(fopen(__FILE__, 'r'), STREAM_SHUT_RD));
/// try { stream_socket_shutdown($a, 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(4)
/// bool(true)
/// string(4) "last"
/// string(0) ""
/// bool(true)
/// bool(false)
/// stream_socket_shutdown(): Argument #2 ($mode) must be one of STREAM_SHUT_RD, STREAM_SHUT_WR, or STREAM_SHUT_RDWR
/// ```
fn stream_socket_shutdown(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let how = match long_argument(req, 2)? {
        STREAM_SHUT_RD => Shutdown::Read,
        STREAM_SHUT_WR => Shutdown::Write,
        STREAM_SHUT_RDWR => Shutdown::Both,
        _ => {
            return Err(arg_value_error(
                req,
                2,
                "be one of STREAM_SHUT_RD, STREAM_SHUT_WR, or STREAM_SHUT_RDWR",
            ));
        }
    };
    Ok(Value::bool(stream.shutdown(req, how)))
}

/// `stream_socket_pair(int $domain, int $type, int $protocol):
/// array|false`, `PHP_FUNCTION(stream_socket_pair)`
/// (`Z/ext/standard/streamsfuncs.c:49`): `socketpair(2)` and its two ends
/// as `generic_socket` streams, or php's `Failed to create sockets` with
/// `errno` and `false`.
///
/// ```php
/// $p = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
/// var_dump($p, fwrite($p[0], "x"), fread($p[1], 5), stream_get_meta_data($p[0]));
/// var_dump(stream_socket_pair(-1, STREAM_SOCK_STREAM, 0));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   resource(%d) of type (stream)
///   [1]=>
///   resource(%d) of type (stream)
/// }
/// int(1)
/// string(1) "x"
/// array(7) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["stream_type"]=>
///   string(14) "generic_socket"
///   ["mode"]=>
///   string(2) "r+"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(false)
/// }
///
/// Warning: stream_socket_pair(): Failed to create sockets: [47]: Address family not supported by protocol family in %s on line 4
/// bool(false)
/// ```
fn stream_socket_pair(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 3, 3)?;
    let domain = long_argument(req, 1)?;
    let kind = long_argument(req, 2)?;
    let protocol = long_argument(req, 3)?;
    let as_int = |n: i64| i32::try_from(n).unwrap_or(-1);
    let (first, second) =
        match crate::ffi::socketpair(as_int(domain), as_int(kind), as_int(protocol)) {
            Ok(pair) => pair,
            Err(error) => {
                let code = error.raw_os_error().unwrap_or(0);
                let text = req.os_error_text(code);
                let text = text
                    .split_once(" (os error")
                    .map_or(text.as_str(), |(t, _)| t);
                let text = format!("Failed to create sockets: [{code}]: {text}");
                docref(req, Level::Warning, &text);
                return Ok(Value::bool(false));
            }
        };
    let s1 = req.stream_from_fd(first, FdStream::Socket)?;
    let s2 = req.stream_from_fd(second, FdStream::Socket)?;
    let mut pair = PhpArray::new(req)?;
    pair.append(req, s1.as_value())?;
    pair.append(req, s2.as_value())?;
    Ok(Value::array(pair))
}

/// `stream_get_transports(): array`,
/// `PHP_FUNCTION(stream_get_transports)`
/// (`Z/ext/standard/streamsfuncs.c:567`): the registered `transports` in
/// their order. The oracle's list goes on with `ext/openssl`'s six,
/// which this engine does not have; the first four are `standard`'s.
///
/// ```php
/// var_dump(array_slice(stream_get_transports(), 0, 4));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(3) "tcp"
///   [1]=>
///   string(3) "udp"
///   [2]=>
///   string(4) "unix"
///   [3]=>
///   string(3) "udg"
/// }
/// ```
fn stream_get_transports(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    let mut out = PhpArray::new(req)?;
    for name in req.transports() {
        let value = string_value(req, name.as_bytes())?;
        out.append(req, value)?;
    }
    Ok(Value::array(out))
}
