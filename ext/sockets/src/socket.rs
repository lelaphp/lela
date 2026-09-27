//! The `Socket` class and its payload, php's `php_socket`
//! (`Z/ext/sockets/php_sockets.h:69`): the fd, the family it was
//! made for, its last error and whether it blocks; with
//! `socket_object_handlers` refusing `new`, `clone` and comparison, and
//! `ENSURE_SOCKET_VALID` and `PHP_SOCKET_ERROR`, which every native
//! reads through.

use std::cmp::Ordering;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use php_ext::abi::{CloneObj, Instance, ObjectHandlers};
use php_ext::attr::php_class;
use php_ext::call::{Function, lookup_class, new_object};
use php_ext::raising::{Level, Thrown, argument_error, docref, throw};
use php_ext::request::{Request, Stream};
use php_ext::values::{PhpObject, Resource, Value};

use crate::State;

/// Where a socket's fd lives, `bsd_socket` and `zstream`
/// together.
#[derive(Default)]
pub(crate) enum Handle {
    /// `bsd_socket == -1`: laid down and not yet opened, or closed by
    /// `socket_close`.
    #[default]
    Closed,
    /// An fd the socket owns and closes, `zstream` undefined.
    Own(OwnedFd),
    /// An fd a stream owns, the socket imported from it or
    /// exported to it: the number, and the stream, of which the socket
    /// holds one owner. The number stays the socket's after the stream
    /// is closed, as php's `bsd_socket` does.
    Stream { fd: RawFd, stream: Resource },
}

/// `php_socket` less its `zend_object`.
pub(crate) struct Sock {
    pub(crate) handle: Handle,
    /// `type`, the family: `PF_UNSPEC` until opened.
    pub(crate) family: i32,
    /// `error`, the errno of the socket's last failed call.
    pub(crate) error: i32,
    /// `blocking`.
    pub(crate) blocking: bool,
}

impl Default for Sock {
    /// `socket_create_object`.
    fn default() -> Sock {
        Sock {
            handle: Handle::Closed,
            family: libc::AF_UNSPEC,
            error: 0,
            blocking: true,
        }
    }
}

impl Sock {
    /// A socket opened over `fd` for `family`, blocking and with no
    /// error.
    pub(crate) fn owning(fd: OwnedFd, family: i32) -> Sock {
        Sock {
            handle: Handle::Own(fd),
            family,
            error: 0,
            blocking: true,
        }
    }

    /// `bsd_socket`, `None` for `-1`.
    pub(crate) fn fd(&self) -> Option<RawFd> {
        match &self.handle {
            Handle::Closed => None,
            Handle::Own(fd) => Some(fd.as_raw_fd()),
            Handle::Stream { fd, .. } => Some(*fd),
        }
    }

    /// `zstream`, where it is defined.
    pub(crate) fn stream(&self) -> Option<Resource> {
        match &self.handle {
            Handle::Stream { stream, .. } => Some(*stream),
            _ => None,
        }
    }
}

/// `final class Socket`, `@strict-properties`, `@not-serializable`.
///
/// ```php
/// try { new Socket; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $s->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($s, $s == socket_create(AF_INET, SOCK_STREAM, SOL_TCP));
/// ```
/// ```expect
/// Cannot directly construct Socket, use socket_create() instead
/// Trying to clone an uncloneable object of class Socket
/// Serialization of 'Socket' is not allowed
/// Cannot create dynamic property Socket::$x
/// object(Socket)#1 (0) {
/// }
/// bool(false)
/// ```
#[php_class(
    final,
    not_serializable,
    payload = Sock,
    handlers = SOCKET,
    dynamic_properties = forbidden
)]
pub(crate) struct Socket;

#[php_class]
impl Socket {}

/// `socket_object_handlers`: `std_object_handlers` with `free_obj`
/// closing what the socket owns, `get_constructor` refusing,
/// `clone_obj` php's null slot, `get_gc` the stream and `compare`
/// `zend_objects_not_comparable`.
pub(crate) struct Handlers;

pub(crate) static SOCKET: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    /// `socket_free_obj`: the fd closed where the socket owns it,
    /// its owner of the stream let go where a stream does.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let handle = object
            .native_mut::<Sock>(req)
            .map(|sock| std::mem::take(&mut sock.handle));
        if let Some(Handle::Stream { stream, .. }) = handle {
            req.release(Value::resource(stream));
        }
    }

    fn get_constructor(
        &self,
        req: &mut Request,
        _object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        Err(throw(
            req,
            "Error",
            "Cannot directly construct Socket, use socket_create() instead",
        ))
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(stream) = object.native::<Sock>(req).and_then(Sock::stream) {
            visit(Value::resource(stream));
        }
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let _ = (req, left, right);
        Ok(None)
    }
}

/// `object_init_ex(return_value, socket_ce)` with the payload laid:
/// a new `Socket` holding `sock`.
pub(crate) fn new_socket(req: &mut Request, sock: Sock) -> Result<Instance<Socket>, Thrown> {
    let object = made(req, b"Socket")?;
    if let Some(payload) = object.native_mut::<Sock>(req) {
        *payload = sock;
    }
    Ok(Instance::of(object))
}

/// A new object of a class this extension declares.
pub(crate) fn made(req: &mut Request, class: &[u8]) -> Result<PhpObject, Thrown> {
    let Some(id) = lookup_class(req, class)? else {
        let text = format!("Class \"{}\" not found", String::from_utf8_lossy(class));
        return Err(throw(req, "Error", &text));
    };
    new_object(req, id)
}

/// The payload of a socket argument.
pub(crate) fn sock(req: &mut Request, socket: Instance<Socket>) -> Result<&mut Sock, Thrown> {
    socket.payload_mut(req)
}

/// `ENSURE_SOCKET_VALID`: the fd of the socket at argument
/// `position`, and php's `Error` for one closed.
pub(crate) fn valid_at(
    req: &mut Request,
    socket: Instance<Socket>,
    position: u32,
) -> Result<RawFd, Thrown> {
    match sock(req, socket)?.fd() {
        Some(fd) => Ok(fd),
        None => Err(argument_error(
            req,
            "Error",
            position,
            "has already been closed",
        )),
    }
}

/// [`valid_at`] for the first argument, where every native but
/// `socket_select`'s arrays takes its socket.
pub(crate) fn valid(req: &mut Request, socket: Instance<Socket>) -> Result<RawFd, Thrown> {
    valid_at(req, socket, 1)
}

/// `sockets_strerror` (`Z/ext/sockets/sockets.c:418`): a number below
/// `-10000` is a resolver error, `-10000 - h_errno`, named by
/// `hstrerror`; any other is `strerror`'s.
pub(crate) fn strerror(req: &Request, error: i32) -> String {
    if error < -10000 {
        let h = if error == i32::MIN {
            2_147_473_648
        } else {
            -error - 10000
        };
        return String::from_utf8_lossy(&crate::ffi::hstrerror(h)).into_owned();
    }
    req.os_error_text(error)
}

/// `EAGAIN`, `EWOULDBLOCK` and `EINPROGRESS`: what a socket that does
/// not block meets, which `PHP_SOCKET_ERROR` records without a warning.
pub(crate) const fn is_transient(error: i32) -> bool {
    error == libc::EAGAIN || error == libc::EWOULDBLOCK || error == libc::EINPROGRESS
}

/// `PHP_SOCKET_ERROR(socket, msg, errn)`: the errno recorded on the
/// socket and as `SOCKETS_G(last_error)`, and php's warning
/// `msg [errno]: text` unless the socket would only have blocked.
pub(crate) fn socket_error(req: &mut Request, socket: PhpObject, msg: &str, error: i32) {
    if let Some(sock) = socket.native_mut::<Sock>(req) {
        sock.error = error;
    }
    req.state::<State>().last_error = error;
    if !is_transient(error) {
        let text = format!("{msg} [{error}]: {}", strerror(req, error));
        docref(req, Level::Warning, &text);
    }
}

/// `SOCKETS_G(last_error) = errno` and php's warning `msg [errno]:
/// text`, for a call that made no socket.
pub(crate) fn global_error(req: &mut Request, msg: &str, error: i32) {
    req.state::<State>().last_error = error;
    let text = format!("{msg} [{error}]: {}", strerror(req, error));
    docref(req, Level::Warning, &text);
}

/// The stream a socket's `zstream` names, where it is still one.
pub(crate) fn live_stream(req: &Request, stream: Resource) -> Option<Stream> {
    Stream::from_value(req, &Value::resource(stream))
}
