//! `socket_import_stream` and `socket_export_stream`: a socket and a
//! stream of the engine's over one fd, which the stream owns,
//! and of which the socket holds one owner as php's `zstream` does. No
//! second socket type: the stream is the engine's, cast with
//! `PHP_STREAM_AS_SOCKETD` or made over the socket by the `transport` its
//! type names.

use std::os::fd::AsRawFd;

use php_ext::abi::{Instance, OrFalse, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, throw};
use php_ext::request::{Cast, FdStream, Request, Stream, StreamOption};
use php_ext::values::{Resource, Value};

use crate::ffi;
use crate::socket::{Handle, Sock, Socket, made, sock, socket_error, valid};

/// `socket_import_stream($stream): Socket|false`: the stream's socket
/// as a `Socket` of the family its name has, sharing the fd
/// with the stream, whose read buffer is turned off; php's warning for
/// a stream of a kind that is no socket, and `php_stream_from_zval`'s
/// `TypeError` for a resource that is no open stream.
///
/// ```php
/// $srv = stream_socket_server('tcp://127.0.0.1:0');
/// $s = socket_import_stream($srv);
/// var_dump($s instanceof Socket, socket_get_option($s, SOL_SOCKET, SO_TYPE), socket_getsockname($s, $a), $a);
/// var_dump(socket_import_stream(fopen(__FILE__, 'r')));
/// var_dump(socket_import_stream(fopen('php://memory', 'r')));
/// unset($srv);
/// var_dump(socket_getsockname($s, $a));
/// $f = fopen(__FILE__, 'r'); fclose($f);
/// try { socket_import_stream($f); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// bool(true)
/// string(9) "127.0.0.1"
///
/// Warning: socket_import_stream(): Cannot represent a stream of type STDIO as a Socket Descriptor in %s on line 5
/// bool(false)
///
/// Warning: socket_import_stream(): Cannot represent a stream of type MEMORY as a Socket Descriptor in %s on line 6
/// bool(false)
/// bool(true)
/// socket_import_stream(): supplied resource is not a valid stream resource
/// ```
#[php_function]
fn socket_import_stream(
    req: &mut Request,
    stream: Resource,
) -> Result<OrFalse<Instance<Socket>>, Thrown> {
    let Some(stream) = Stream::from_value(req, &Value::resource(stream)) else {
        return Err(throw(
            req,
            "TypeError",
            "socket_import_stream(): supplied resource is not a valid stream resource",
        ));
    };
    let Some(fd) = stream.cast(req, Cast::Socket, true) else {
        return Ok(OrFalse::False);
    };
    let object = made(req, b"Socket")?;
    let family = match ffi::name(fd, false) {
        Ok(ffi::Addr::V4(_)) => libc::AF_INET,
        Ok(ffi::Addr::V6(_)) => libc::AF_INET6,
        Ok(ffi::Addr::Unix(_)) => libc::AF_UNIX,
        Ok(ffi::Addr::Other(family)) => family,
        Err(errno) => {
            socket_error(req, object, "Unable to obtain socket family", errno as i32);
            req.release(object.as_value());
            return Ok(OrFalse::False);
        }
    };
    let flags = match ffi::status_flags(fd) {
        Ok(flags) => flags,
        Err(errno) => {
            socket_error(req, object, "Unable to obtain blocking state", errno as i32);
            req.release(object.as_value());
            return Ok(OrFalse::False);
        }
    };
    let resource = stream.resource();
    req.addref(&Value::resource(resource));
    if let Some(payload) = object.native_mut::<Sock>(req) {
        *payload = Sock {
            handle: Handle::Stream {
                fd,
                stream: resource,
            },
            family,
            error: 0,
            blocking: !ffi::is_nonblocking(flags),
        };
    }
    stream.set_option(
        req,
        StreamOption::ReadBuffer {
            buffered: false,
            size: 0,
        },
    );
    Ok(OrFalse::Value(Instance::of(object)))
}

/// The `transport` `socket_export_stream` makes the stream with: `tcp`
/// or `udp` for an inet socket of that type, `unix` or `udg` for a unix
/// one, none for anything else.
fn transport(fd: i32, family: i32) -> Option<&'static str> {
    let mut buf = [0u8; 4];
    ffi::getsockopt(fd, libc::SOL_SOCKET, libc::SO_TYPE, &mut buf).ok()?;
    let ty = i32::from_ne_bytes(buf);
    match family {
        libc::AF_INET | libc::AF_INET6 => match ty {
            libc::SOCK_STREAM => {
                #[cfg(target_os = "linux")]
                {
                    let mut proto = [0u8; 4];
                    ffi::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PROTOCOL, &mut proto).ok()?;
                    if i32::from_ne_bytes(proto) != libc::IPPROTO_TCP {
                        return None;
                    }
                }
                Some("tcp")
            }
            libc::SOCK_DGRAM => Some("udp"),
            _ => None,
        },
        libc::AF_UNIX => match ty {
            libc::SOCK_STREAM => Some("unix"),
            libc::SOCK_DGRAM => Some("udg"),
            _ => None,
        },
        _ => None,
    }
}

/// `socket_export_stream(Socket $socket)`: the stream the socket came
/// from or went to already, or a new one of the `transport` its family
/// and type name, which takes the fd over.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP);
/// $st = socket_export_stream($s);
/// var_dump($st === socket_export_stream($s), stream_get_meta_data($st)['stream_type'], stream_get_meta_data($st)['uri']);
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// $a = socket_export_stream($p[0]);
/// var_dump(stream_get_meta_data($a)['stream_type'], fwrite($a, "hi"), socket_read($p[1], 10));
/// fclose($a);
/// var_dump(@socket_write($p[0], "x"), socket_last_error($p[0]) === SOCKET_EBADF);
/// ```
/// ```expect process
/// bool(true)
/// string(10) "udp_socket"
/// string(6) "udp://"
/// string(11) "unix_socket"
/// int(2)
/// string(2) "hi"
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn socket_export_stream(
    req: &mut Request,
    socket: Instance<Socket>,
) -> Result<Untyped<OrFalse<Resource>>, Thrown> {
    let fd = valid(req, socket)?;
    if let Some(stream) = sock(req, socket)?.stream() {
        req.addref(&Value::resource(stream));
        return Ok(Untyped(OrFalse::Value(stream)));
    }
    let payload = sock(req, socket)?;
    let (family, blocking) = (payload.family, payload.blocking);
    let Handle::Own(owned) = std::mem::take(&mut payload.handle) else {
        return Ok(Untyped(OrFalse::False));
    };
    let raw = owned.as_raw_fd();
    let kind = FdStream::Xport {
        name: transport(fd, family),
        blocking,
    };
    let stream = req.stream_from_fd(owned, kind)?;
    let resource = stream.resource();
    req.addref(&Value::resource(resource));
    sock(req, socket)?.handle = Handle::Stream {
        fd: raw,
        stream: resource,
    };
    Ok(Untyped(OrFalse::Value(resource)))
}
