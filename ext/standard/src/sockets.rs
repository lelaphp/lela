//! The socket natives of `streamsfuncs.c` (`Z/ext/standard/streamsfuncs.c:49-431`,
//! `:567`, `:1497`, `:1690`) and `fsock.c`'s `fsockopen` and
//! `pfsockopen`, over the engine's socket kinds and its
//! `php_stream_xport_create`: a client connected or a server bound
//! through a `transport`, accepted, named, sent to and received from, shut
//! down, and a pair made by `socketpair(2)`, which is `src/ffi/`'s. Beside
//! them is `default_socket_timeout`, `file.c`'s ini entry, which every
//! socket's read timeout and every connect without one of its own read;
//! the constants `file.stub.php` declares for these natives are
//! `file::constants`.
//!
//! `stream_socket_client` and `stream_socket_server` resolve their
//! `$context` as `php_stream_context_from_zval` does, the default one for
//! `null`, and hand `php_stream_xport_create` its `socket` options, which
//! `xp_socket.c` and `transports.c` read; `fsockopen` opens with none.
//!
//! `pfsockopen` and `STREAM_CLIENT_PERSISTENT` hand the engine php's
//! persistent key, and the engine keeps the socket in its pool between
//! requests (design §4.10). What is not here: crypto, which is
//! `ext/openssl`'s.
//!
//! The natives, as `basic_functions.stub.php` declares them.
//!
//! ```php
//! foreach (['fsockopen', 'stream_socket_client', 'stream_socket_server', 'stream_socket_accept', 'stream_socket_get_name', 'stream_socket_recvfrom', 'stream_socket_sendto', 'stream_socket_enable_crypto', 'stream_socket_shutdown', 'stream_socket_pair', 'stream_get_transports'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function fsockopen ] {
//!
//!   - Parameters [5] {
//!     Parameter #0 [ <required> string $hostname ]
//!     Parameter #1 [ <optional> int $port = -1 ]
//!     Parameter #2 [ <optional> &$error_code = null ]
//!     Parameter #3 [ <optional> &$error_message = null ]
//!     Parameter #4 [ <optional> ?float $timeout = null ]
//!   }
//! }
//! Function [ <internal:standard> function stream_socket_client ] {
//!
//!   - Parameters [6] {
//!     Parameter #0 [ <required> string $address ]
//!     Parameter #1 [ <optional> &$error_code = null ]
//!     Parameter #2 [ <optional> &$error_message = null ]
//!     Parameter #3 [ <optional> ?float $timeout = null ]
//!     Parameter #4 [ <optional> int $flags = STREAM_CLIENT_CONNECT ]
//!     Parameter #5 [ <optional> $context = null ]
//!   }
//! }
//! Function [ <internal:standard> function stream_socket_server ] {
//!
//!   - Parameters [5] {
//!     Parameter #0 [ <required> string $address ]
//!     Parameter #1 [ <optional> &$error_code = null ]
//!     Parameter #2 [ <optional> &$error_message = null ]
//!     Parameter #3 [ <optional> int $flags = STREAM_SERVER_BIND | STREAM_SERVER_LISTEN ]
//!     Parameter #4 [ <optional> $context = null ]
//!   }
//! }
//! Function [ <internal:standard> function stream_socket_accept ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> $socket ]
//!     Parameter #1 [ <optional> ?float $timeout = null ]
//!     Parameter #2 [ <optional> &$peer_name = null ]
//!   }
//! }
//! Function [ <internal:standard> function stream_socket_get_name ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> $socket ]
//!     Parameter #1 [ <required> bool $remote ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function stream_socket_recvfrom ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> $socket ]
//!     Parameter #1 [ <required> int $length ]
//!     Parameter #2 [ <optional> int $flags = 0 ]
//!     Parameter #3 [ <optional> &$address = null ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function stream_socket_sendto ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> $socket ]
//!     Parameter #1 [ <required> string $data ]
//!     Parameter #2 [ <optional> int $flags = 0 ]
//!     Parameter #3 [ <optional> string $address = "" ]
//!   }
//!   - Return [ int|false ]
//! }
//! Function [ <internal:standard> function stream_socket_enable_crypto ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> bool $enable ]
//!     Parameter #2 [ <optional> ?int $crypto_method = null ]
//!     Parameter #3 [ <optional> $session_stream = null ]
//!   }
//!   - Return [ int|bool ]
//! }
//! Function [ <internal:standard> function stream_socket_shutdown ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> int $mode ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function stream_socket_pair ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> int $domain ]
//!     Parameter #1 [ <required> int $type ]
//!     Parameter #2 [ <required> int $protocol ]
//!   }
//!   - Return [ array|false ]
//! }
//! Function [ <internal:standard> function stream_get_transports ] {
//!
//!   - Parameters [0] {
//!   }
//!   - Return [ array ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::net::Shutdown;
use std::time::Duration;

use php_ext::abi::{OrFalse, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref};
use php_ext::request::{
    FdStream, Handshake, Request, SocketOptions, Stream, Streams as _, Xport, XportFailure,
};
use php_ext::values::{Owner, PhpArray, PhpString, Resource, Value, ValueRef};

use crate::file::constants::{
    STREAM_CLIENT_ASYNC_CONNECT, STREAM_CLIENT_CONNECT, STREAM_CLIENT_PERSISTENT,
    STREAM_SERVER_LISTEN, STREAM_SHUT_RD, STREAM_SHUT_RDWR, STREAM_SHUT_WR,
};
use crate::streamsfuncs::{NullContext, context_of};

/// An untyped by-reference out-parameter, `&$error_code = null`.
type Out = Ref<Untyped<Value>>;

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
        default: Some(b"60".as_slice().into()),
        user: true,
        per_dir: true,
        system: true,
    }]
}

/// A `?float $timeout` given as `null` or not at all: `FG(default_socket_timeout)`.
fn timeout_or_default(req: &Request, timeout: Option<f64>) -> f64 {
    timeout.unwrap_or_else(|| default_timeout(req))
}

/// `FG(default_socket_timeout)` as a double of seconds, what a timeout
/// of `null` is: the quantity the entry's `OnUpdateLong` read.
pub(crate) fn default_timeout(req: &Request) -> f64 {
    #[allow(clippy::cast_precision_loss)] // php's `(double)FG(default_socket_timeout)`
    let secs = req.ini().long(b"default_socket_timeout") as f64;
    secs
}

/// `PHP_TIMEOUT_ULL_MAX / 1000000.0`, the most seconds a timeout holds.
const TIMEOUT_MAX: f64 = 18_446_744_073_709.55;
/// The same bound as the unsigned integer `ZEND_ULONG_FMT` names, which
/// `fsockopen()`'s argument #6 error prints; php passes the double to it
/// and prints what a register held (`trace:fsockopen-timeout-bound`).
const TIMEOUT_MAX_SHOWN: u64 = 18_446_744_073_709;

/// A timeout of seconds as the natives turn it into a `timeval`: `None`,
/// no limit, below zero and at or past the most a timeout holds, and the
/// microseconds `(php_timeout_ull)(timeout * 1000000.0)` keeps otherwise.
pub(crate) fn timeval(timeout: f64) -> Option<Duration> {
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
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `ZEND_TRY_ASSIGN_REF_LONG` and `ZEND_TRY_ASSIGN_REF_STR` of the two
/// by-reference slots `$error_code` and `$error_message`, where the call
/// gave them.
fn assign_error(
    req: &mut Request,
    (error_code, error_message): (&Out, &Out),
    code: i64,
    text: Option<&[u8]>,
) -> Result<(), Thrown> {
    error_code.set(req, Untyped(Value::int(code)))?;
    if let Some(text) = text
        && error_message.is_given()
    {
        let value = string_value(req, text)?;
        error_message.set(req, Untyped(value))?;
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
/// var_dump(fsockopen("127.0.0.1\0.example.com", $port));
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
///
/// Warning: fsockopen(): Unable to connect to 127.0.0.1:%d (The hostname must not contain null bytes) in %s on line 8
/// bool(false)
/// ```
///
/// A timeout outside `-1` and the bound is argument #6's `ValueError`,
/// the bound printed as the integer php's format names; the pinned php
/// prints whatever its integer register holds there, so the line holds
/// the digits and nothing after them (`trace:fsockopen-timeout-bound`).
///
/// ```php
/// foreach ([1e20, -2.0, NAN] as $t) {
///     try { fsockopen('127.0.0.1', 1, $code, $message, $t); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
///     try { pfsockopen('udp://127.0.0.1', 9, $code, $message, $t); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// fsockopen(): Argument #6 must be -1 or between 0 and %d
/// pfsockopen(): Argument #6 must be -1 or between 0 and %d
/// fsockopen(): Argument #6 must be -1 or between 0 and %d
/// pfsockopen(): Argument #6 must be -1 or between 0 and %d
/// fsockopen(): Argument #6 must be -1 or between 0 and %d
/// pfsockopen(): Argument #6 must be -1 or between 0 and %d
/// ```
///
/// The digits are `PHP_TIMEOUT_ULL_MAX / 1000000`, which the pinned php
/// does not print; the row's `covers` holds this fence to the engine
/// alone (#1553, #1556).
///
/// ```php
/// try { fsockopen('127.0.0.1', 1, $code, $message, 1e20); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pfsockopen('udp://127.0.0.1', 9, $code, $message, 1e20); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// fsockopen(): Argument #6 must be -1 or between 0 and 18446744073709
/// pfsockopen(): Argument #6 must be -1 or between 0 and 18446744073709
/// ```
#[php_function]
fn fsockopen(
    req: &mut Request,
    hostname: PhpString,
    #[php(default = -1)] port: i64,
    #[php(default = null)] error_code: Out,
    #[php(default = null)] error_message: Out,
    #[php(default = null)] timeout: Option<f64>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let error = (&error_code, &error_message);
    fsockopen_stream(req, "fsockopen", (hostname, port, timeout), error)
}

/// `pfsockopen(...)`, `fsockopen`'s persistent twin: the socket kept
/// under `pfsockopen__<host>:<port>`, the host escaped as
/// `php_stream_escape_persistent_key` escapes it, a resource of the
/// `persistent stream` type; a second ask by the key answers the same
/// resource, and `fclose` of one holder closes it for every holder.
///
/// ```php
/// $c = pfsockopen('udp://127.0.0.1', 9, $code, $message);
/// $d = pfsockopen('udp://127.0.0.1', 9);
/// var_dump($c, $code, $message, $c === $d);
/// fclose($d);
/// var_dump($c);
/// ```
/// ```expect
/// resource(%d) of type (persistent stream)
/// int(0)
/// string(0) ""
/// bool(true)
/// resource(%d) of type (Unknown)
/// ```
#[php_function]
fn pfsockopen(
    req: &mut Request,
    hostname: PhpString,
    #[php(default = -1)] port: i64,
    #[php(default = null)] error_code: Out,
    #[php(default = null)] error_message: Out,
    #[php(default = null)] timeout: Option<f64>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let error = (&error_code, &error_message);
    fsockopen_stream(req, "pfsockopen", (hostname, port, timeout), error)
}

/// `php_stream_escape_persistent_key` (`Z/main/streams/streams.c:144`):
/// the host whole, a NUL written `\0` and a backslash `\\`, so two
/// hosts that differ past a NUL key two persistent streams (GH-22617).
fn persistent_key(host: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(host.len());
    for &b in host {
        match b {
            0 => escaped.extend_from_slice(b"\\0"),
            b'\\' => escaped.extend_from_slice(b"\\\\"),
            b => escaped.push(b),
        }
    }
    escaped
}

/// `php_fsockopen_stream` for `native`, the name its `ValueError` prints.
fn fsockopen_stream(
    req: &mut Request,
    native: &str,
    (host, port, timeout): (PhpString, i64, Option<f64>),
    error: (&Out, &Out),
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let host = host.as_bytes().to_vec();
    let timeout = timeout_or_default(req, timeout);
    #[allow(clippy::float_cmp)] // php's `timeout != -1.0`
    let limited = timeout != -1.0;
    if limited && !(0.0..=TIMEOUT_MAX).contains(&timeout) {
        let text = format!("must be -1 or between 0 and {TIMEOUT_MAX_SHOWN}");
        return Err(argument_error(req, "ValueError", 6, &text));
    }
    let mut hostname = host.clone();
    if port > 0 {
        hostname.extend_from_slice(format!(":{port}").as_bytes());
    }
    let timeout = if limited { timeval(timeout) } else { None };
    // `pfsockopen__%s:%ld`, the host escaped and the port as given.
    let persistent = (native == "pfsockopen").then(|| {
        let mut key = b"pfsockopen__".to_vec();
        key.extend_from_slice(&persistent_key(&host));
        key.extend_from_slice(format!(":{port}").as_bytes());
        key
    });
    let xport = Xport::Client {
        timeout,
        connect: true,
        persistent,
        socket: SocketOptions::default(),
        context: None,
    };
    match req.xport_create(&hostname, xport)? {
        Ok(stream) => {
            if let Err(thrown) = assign_error(req, error, 0, Some(b"")) {
                stream.give_up(req);
                return Err(thrown);
            }
            Ok(Untyped(OrFalse::Value(stream.into_resource())))
        }
        Err(XportFailure { code, text, .. }) => {
            // php prints the host as the C string it holds.
            let shown = format!(
                "Unable to connect to {}:{port} ({})",
                String::from_utf8_lossy(php_ext::coerce::c_str(&host)),
                text.as_deref().unwrap_or("Unknown error")
            );
            docref(req, Level::Warning, &shown);
            assign_error(
                req,
                error,
                i64::from(code),
                text.as_deref().map(str::as_bytes),
            )?;
            Ok(Untyped(OrFalse::False))
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
/// $path = '/tmp/' . 'wc' . bin2hex(random_bytes(8)) . '.sock';
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
///
/// The `socket` options of its `context`: a client binds where `bindto` says,
/// and a value that is no string refuses the connect.
///
/// ```php
/// $t = stream_socket_server('tcp://127.0.0.1:0');
/// $port = explode(':', stream_socket_get_name($t, false))[1];
/// fclose($t);
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $ctx = stream_context_create(['socket' => ['bindto' => "127.0.0.1:$port", 'tcp_nodelay' => true]]);
/// $c = stream_socket_client('tcp://' . stream_socket_get_name($s, false), $code, $message, 1, STREAM_CLIENT_CONNECT, $ctx);
/// var_dump(stream_socket_get_name($c, false) === "127.0.0.1:$port", stream_socket_get_name(stream_socket_accept($s), true) === "127.0.0.1:$port");
/// $ctx = stream_context_create(['socket' => ['bindto' => 12]]);
/// var_dump(stream_socket_client('tcp://' . stream_socket_get_name($s, false), $code, $message, 1, STREAM_CLIENT_CONNECT, $ctx), $message);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// Warning: stream_socket_client(): Unable to connect to tcp://127.0.0.1:%d (local_addr context option is not a string.) in %s on line 10
/// bool(false)
/// string(42) "local_addr context option is not a string."
/// ```
///
/// `STREAM_CLIENT_PERSISTENT` keeps the client under
/// `stream_socket_client__<address>`, a `persistent stream`, which the
/// next ask by the address answers while it is alive.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $at = 'tcp://' . stream_socket_get_name($s, false);
/// $a = stream_socket_client($at, $code, $message, 1, STREAM_CLIENT_CONNECT | STREAM_CLIENT_PERSISTENT);
/// $b = stream_socket_client($at, $code, $message, 1, STREAM_CLIENT_CONNECT | STREAM_CLIENT_PERSISTENT);
/// var_dump(get_resource_type($a), $a === $b, $a === stream_socket_client($at));
/// ```
/// ```expect
/// string(17) "persistent stream"
/// bool(true)
/// bool(false)
/// ```
///
/// A client whose `$flags` carry neither `STREAM_CLIENT_CONNECT` nor
/// `STREAM_CLIENT_ASYNC_CONNECT` connects nowhere and parses nothing: the
/// stream holds no socket, a write takes nothing, a read fails and it is
/// at its end.
///
/// ```php
/// $c = stream_socket_client('abc', $code, $message, 0, 0);
/// var_dump(get_resource_type($c), $code, $message, fwrite($c, "x"), fread($c, 10), feof($c));
/// ```
/// ```expect
/// string(6) "stream"
/// int(0)
/// string(0) ""
/// int(0)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn stream_socket_client(
    req: &mut Request,
    address: PhpString,
    #[php(default = null)] error_code: Out,
    #[php(default = null)] error_message: Out,
    #[php(default = null)] timeout: Option<f64>,
    #[php(default = STREAM_CLIENT_CONNECT)] flags: i64,
    #[php(default = null)] context: Option<Resource>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let context = context_of(req, context, NullContext::Default)?;
    let host = address.as_bytes().to_vec();
    let timeout = match timeout {
        Some(timeout) if !timeout.is_finite() => {
            return Err(arg_value_error(req, 4, "be a finite value"));
        }
        timeout => timeout_or_default(req, timeout),
    };
    let error = (&error_code, &error_message);
    assign_error(req, error, 0, Some(b""))?;
    match req.xport_create(
        &host,
        Xport::Client {
            timeout: timeval(timeout),
            connect: flags & (STREAM_CLIENT_CONNECT | STREAM_CLIENT_ASYNC_CONNECT) != 0,
            // `stream_socket_client__%s`, the address escaped.
            persistent: (flags & STREAM_CLIENT_PERSISTENT != 0).then(|| {
                let mut key = b"stream_socket_client__".to_vec();
                key.extend_from_slice(&persistent_key(&host));
                key
            }),
            socket: SocketOptions::default(),
            context,
        },
    )? {
        Ok(stream) => Ok(Untyped(OrFalse::Value(stream.into_resource()))),
        Err(XportFailure { code, text, .. }) => {
            let shown = format!(
                "Unable to connect to {} ({})",
                String::from_utf8_lossy(&addslashes(&host)),
                text.as_deref().unwrap_or("Unknown error")
            );
            docref(req, Level::Warning, &shown);
            assign_error(
                req,
                error,
                i64::from(code),
                text.as_deref().map(str::as_bytes),
            )?;
            Ok(Untyped(OrFalse::False))
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
/// $path = '/tmp/' . 'ws' . bin2hex(random_bytes(8)) . '.sock';
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
///
/// A server listens with the `backlog` its `context` names, `32` where
/// it names none, and the default `context`'s options apply where
/// `$context` is `null`: with `backlog` at one, one client of three gets
/// in before the server accepts.
///
/// ```php
/// foreach ([['backlog' => 1], []] as $socket) {
///     $s = stream_socket_server('tcp://127.0.0.1:0', $code, $message, STREAM_SERVER_BIND | STREAM_SERVER_LISTEN, stream_context_create(['socket' => $socket]));
///     $in = 0;
///     for ($i = 0; $i < 3; $i++) $in += (bool) @stream_socket_client('tcp://' . stream_socket_get_name($s, false), $code, $message, 0.2);
///     echo $in, "\n";
/// }
/// stream_context_set_default(['socket' => ['backlog' => 1]]);
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $in = 0;
/// for ($i = 0; $i < 3; $i++) $in += (bool) @stream_socket_client('tcp://' . stream_socket_get_name($s, false), $code, $message, 0.2);
/// echo $in, "\n";
/// ```
/// ```expect
/// 1
/// 3
/// 1
/// ```
///
/// Its warning prints the address as the C string php holds:
///
/// ```php
/// var_dump(stream_socket_server("tcp://127.0.0.1\0x:1"));
/// ```
/// ```expect
/// Warning: stream_socket_server(): Unable to connect to tcp://127.0.0.1 (The hostname must not contain null bytes) in %s on line 2
/// bool(false)
/// ```
#[php_function]
fn stream_socket_server(
    req: &mut Request,
    address: PhpString,
    #[php(default = null)] error_code: Out,
    #[php(default = null)] error_message: Out,
    #[php(default = STREAM_SERVER_BIND | STREAM_SERVER_LISTEN)] flags: i64,
    #[php(default = null)] context: Option<Resource>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let context = context_of(req, context, NullContext::Default)?;
    let host = address.as_bytes().to_vec();
    let error = (&error_code, &error_message);
    assign_error(req, error, 0, Some(b""))?;
    let xport = Xport::Server {
        listen: flags & STREAM_SERVER_LISTEN != 0,
        socket: SocketOptions::default(),
        context,
    };
    match req.xport_create(&host, xport)? {
        Ok(stream) => Ok(Untyped(OrFalse::Value(stream.into_resource()))),
        Err(XportFailure { code, text, .. }) => {
            let shown = format!(
                "Unable to connect to {} ({})",
                String::from_utf8_lossy(php_ext::coerce::c_str(&host)),
                text.as_deref().unwrap_or("Unknown error")
            );
            docref(req, Level::Warning, &shown);
            assign_error(
                req,
                error,
                i64::from(code),
                text.as_deref().map(str::as_bytes),
            )?;
            Ok(Untyped(OrFalse::False))
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
#[php_function]
fn stream_socket_accept(
    req: &mut Request,
    socket: Stream,
    #[php(default = null)] timeout: Option<f64>,
    #[php(default = null)] peer_name: Out,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let timeout = match timeout {
        Some(timeout) if !timeout.is_finite() => {
            return Err(arg_value_error(req, 2, "be a finite value"));
        }
        timeout => timeout_or_default(req, timeout),
    };
    match socket.accept(req, timeval(timeout), peer_name.is_given())? {
        Ok((client, peer)) => {
            if let Some(peer) = peer {
                peer_name.set(req, Untyped(peer.into_value()))?;
            }
            Ok(Untyped(OrFalse::Value(client.into_resource())))
        }
        Err(text) => {
            let text = format!(
                "Accept failed: {}",
                text.as_deref().unwrap_or("Unknown error")
            );
            docref(req, Level::Warning, &text);
            Ok(Untyped(OrFalse::False))
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
#[php_function]
fn stream_socket_get_name(
    req: &mut Request,
    socket: Stream,
    remote: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    Ok(socket
        .socket_name(req, remote)?
        .map_or(OrFalse::False, OrFalse::Value))
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
#[php_function]
fn stream_socket_recvfrom(
    req: &mut Request,
    socket: Stream,
    length: i64,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] address: Out,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let want_addr = address.is_given();
    address.set(req, Untyped(Value::null()))?;
    let Ok(len) = usize::try_from(length) else {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    };
    if len == 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    match socket.recvfrom(req, len, flags, want_addr)? {
        Some((bytes, from)) => {
            if let Some(from) = from
                && let Err(thrown) = address.set(req, Untyped(from.into_value()))
            {
                bytes.give_up(req);
                return Err(thrown);
            }
            Ok(OrFalse::Value(bytes))
        }
        None => Ok(OrFalse::False),
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
#[php_function]
fn stream_socket_sendto(
    req: &mut Request,
    socket: Stream,
    data: PhpString,
    #[php(default = 0)] flags: i64,
    #[php(default = "")] address: PhpString,
) -> OrFalse<i64> {
    let data = data.as_bytes().to_vec();
    let address = address.as_bytes().to_vec();
    let to = (!address.is_empty()).then_some(address.as_slice());
    socket
        .sendto(req, &data, flags, to)
        .map_or(OrFalse::False, OrFalse::Value)
}

/// `stream_socket_enable_crypto($stream, bool $enable, ?int
/// $crypto_method = null, $session_stream = null): int|bool`,
/// `PHP_FUNCTION(stream_socket_enable_crypto)`
/// (`Z/ext/standard/streamsfuncs.c:1497`): enabling takes the method the
/// call gives or the `ssl` `crypto_method` of the stream's own `context`,
/// and with neither is php's `ValueError`; the stream's crypto layer set
/// up with it, `false` where that fails, then enabled or shut: `false`
/// for a handshake that failed, `0` for one a socket that does not block
/// has more of, `true` otherwise. A stream with no crypto layer is
/// php's warning each time it is asked, and `true` for turning it off.
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
#[php_function]
fn stream_socket_enable_crypto(
    req: &mut Request,
    stream: Stream,
    enable: bool,
    #[php(default = null)] crypto_method: Option<i64>,
    #[php(default = null)] session_stream: Option<Stream>,
) -> Result<Union<i64, bool>, Thrown> {
    if enable {
        let method = if let Some(method) = crypto_method {
            method
        } else {
            // `GET_CTX_OPT(stream, "ssl", "crypto_method", val)`, then
            // `Z_LVAL_P(val)`: the integer it holds, read unconverted.
            let found = stream
                .ctx(req)
                .and_then(|context| context.option(req, b"ssl", b"crypto_method"));
            let Some(value) = found else {
                return Err(arg_value_error(
                    req,
                    3,
                    "be specified when enabling encryption",
                ));
            };
            match value.as_ref() {
                ValueRef::Int(method) => method,
                _ => 0,
            }
        };
        if !stream.crypto_setup(req, method, session_stream) {
            return Ok(Union::B(false));
        }
    }
    Ok(match stream.crypto_enable(req, enable) {
        Handshake::Failed => Union::B(false),
        Handshake::Pending => Union::A(0),
        Handshake::Done | Handshake::Unsupported => Union::B(true),
    })
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
#[php_function]
fn stream_socket_shutdown(req: &mut Request, stream: Stream, mode: i64) -> Result<bool, Thrown> {
    let how = match mode {
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
    Ok(stream.shutdown(req, how))
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
#[php_function]
fn stream_socket_pair(
    req: &mut Request,
    domain: i64,
    r#type: i64,
    protocol: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let as_int = |n: i64| i32::try_from(n).unwrap_or(-1);
    let (first, second) =
        match crate::ffi::socketpair(as_int(domain), as_int(r#type), as_int(protocol)) {
            Ok(pair) => pair,
            Err(error) => {
                let code = error.raw_os_error().unwrap_or(0);
                let text = req.environment().os_error_text(code);
                let text = text
                    .split_once(" (os error")
                    .map_or(text.as_str(), |(t, _)| t);
                let text = format!("Failed to create sockets: [{code}]: {text}");
                docref(req, Level::Warning, &text);
                return Ok(OrFalse::False);
            }
        };
    let s1 = req.stream_from_fd(first, FdStream::Socket)?;
    let s2 = req.stream_from_fd(second, FdStream::Socket)?;
    let mut pair = PhpArray::new(req)?;
    pair.append(req, s1.into_value())?;
    pair.append(req, s2.into_value())?;
    Ok(OrFalse::Value(pair))
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
#[php_function]
fn stream_get_transports(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for name in req.transports() {
        let value = string_value(req, name.as_bytes())?;
        out.append(req, value)?;
    }
    Ok(out)
}
