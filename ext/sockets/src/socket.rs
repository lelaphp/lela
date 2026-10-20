//! The `Socket` class and its payload, php's `php_socket`
//! (`Z/ext/sockets/php_sockets.h:69`): the fd, the family it was
//! made for, its last error and whether it blocks; with
//! `socket_object_handlers` refusing `new`, `clone` and comparison, and
//! `ENSURE_SOCKET_VALID` and `PHP_SOCKET_ERROR`, which every native
//! reads through.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use php_ext::abi::{GcTable, Instance, ObjectHandlers, Refusals};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Level, Thrown, argument_error, docref, throw_error};
use php_ext::request::{Request, Stream, Streams as _};
use php_ext::values::{Owner, PhpObject, Resource, Value};

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
    Stream { fd: RawFd, stream: Owner<Resource> },
    /// Out of the socket for a wait that stands on its fd ([`wait_on`],
    /// #2601), which holds what stood here and gives it back when it
    /// returns: the fd, and the stream the socket holds an owner of
    /// where it holds one, as a callback served in the wait reaches them.
    Waiting { fd: RawFd, stream: Option<Resource> },
    /// Closed by `socket_close` while a wait held it: closed to the
    /// program at once, and what the wait holds let go when it returns.
    Closing,
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
            Handle::Closed | Handle::Closing => None,
            Handle::Own(fd) => Some(fd.as_raw_fd()),
            Handle::Stream { fd, .. } | Handle::Waiting { fd, .. } => Some(*fd),
        }
    }

    /// `zstream`, where it is defined.
    pub(crate) fn stream(&self) -> Option<Resource> {
        match &self.handle {
            Handle::Stream { stream, .. } => Some(**stream),
            Handle::Waiting { stream, .. } => *stream,
            _ => None,
        }
    }

    /// The socket closed, `bsd_socket = -1`: what it held, for the
    /// caller to let go ([`release`]), or `None` where a wait holds it,
    /// which lets it go when it returns.
    pub(crate) fn close(&mut self) -> Option<Handle> {
        if let Handle::Waiting { .. } = self.handle {
            self.handle = Handle::Closing;
            return None;
        }
        Some(std::mem::take(&mut self.handle))
    }
}

/// What a closed socket held let go: the stream it came from or went to
/// closed, and its owner of it given up, or the fd it owns closed.
pub(crate) fn release(req: &mut Request, handle: Handle) {
    if let Handle::Stream { stream, .. } = handle {
        if let Some(open) = live_stream(req, *stream) {
            open.close(req);
        }
        stream.give_up(req);
    }
}

/// What a wait on sockets answered ([`wait_on`]).
pub(crate) enum Waited<R> {
    /// What the wait answered, every socket open when it returned.
    Answered(R),
    /// The socket at index `at` of the list was closed, before the wait
    /// or in it, and the native answers as php's closed-socket path does:
    /// what the wait answered, where it waited, is the native's to let go.
    Closed { at: usize, answered: Option<R> },
}

/// `wait` on the fds of `sockets`, in their order, with the request lent
/// to a callback C makes from another thread meanwhile
/// ([`Limits::blocking`], #2401), and each socket's [`Handle`] out of
/// the socket for the wait's length, as the streams layer takes a stream
/// out of its resource for an op (#2601). A callback served in the wait
/// that closes one of them closes it to the program at once, and the fd
/// is let go only when the wait returns, so the fd the wait stands on is
/// the socket's until then and nothing opened meanwhile takes its
/// number through a `socket_close` (a stream that owns the fd, closed by
/// `fclose`, is #2664's). php shows none of this, since no code of the program is
/// served during its wait. The close is deferred rather than refused
/// because the streams layer defers an `fclose` of a stream that is out
/// the same way, and a refusal would raise in the callback what php
/// never raises for an open socket. The close does not wake the wait: it
/// returns as it would have, and the native then answers as for a socket
/// closed before it, the order php's one thread would have taken the two
/// in. A socket a wait holds already, the same socket twice in one list
/// or one a callback waits on inside an outer wait, is waited on where
/// it stands and let go by the wait that holds it. Each socket is kept
/// alive for the wait, whatever the callback does to the variables that
/// name it.
pub(crate) fn wait_on<R>(
    req: &mut Request,
    sockets: &[PhpObject],
    wait: impl FnOnce(&[RawFd]) -> R,
) -> Waited<R> {
    let mut fds = Vec::with_capacity(sockets.len());
    let mut held: Vec<(Owner<PhpObject>, Handle)> = Vec::new();
    for (at, object) in sockets.iter().enumerate() {
        let taken = object.native_mut::<Sock>(req).and_then(|sock| {
            let fd = sock.fd()?;
            let stream = sock.stream();
            let handle = match sock.handle {
                Handle::Waiting { .. } => None,
                _ => Some(std::mem::replace(
                    &mut sock.handle,
                    Handle::Waiting { fd, stream },
                )),
            };
            Some((fd, handle))
        });
        let Some((fd, handle)) = taken else {
            give_back(req, held);
            return Waited::Closed { at, answered: None };
        };
        if let Some(handle) = handle {
            held.push((Owner::take(req, *object), handle));
        }
        fds.push(fd);
    }
    // A socket over a stream's fd: the stream held out of its resource
    // for the wait too, so a callback's `fclose` of it closes no fd the
    // wait stands on until it returns (#2664).
    let streams: Vec<Stream> = held
        .iter()
        .filter_map(|(_, handle)| match handle {
            Handle::Stream { stream, .. } => live_stream(req, **stream),
            _ => None,
        })
        .collect();
    let answered = req.holding_streams(&streams, |req| req.limits_mut().blocking(|| wait(&fds)));
    let closed = sockets
        .iter()
        .position(|object| object.native::<Sock>(req).and_then(Sock::fd).is_none());
    give_back(req, held);
    match closed {
        Some(at) => Waited::Closed {
            at,
            answered: Some(answered),
        },
        None => Waited::Answered(answered),
    }
}

/// [`wait_on`] the one socket of a native's first argument, php's
/// closed-socket `Error` where it was closed in the wait, its answer
/// dropped.
pub(crate) fn wait_on_one<R>(
    req: &mut Request,
    socket: Instance<Socket>,
    wait: impl FnOnce(RawFd) -> R,
) -> Result<R, Thrown> {
    match wait_on_socket(req, socket, wait) {
        Waited::Answered(answered) => Ok(answered),
        Waited::Closed { .. } => Err(closed(req, 1)),
    }
}

/// [`wait_on`] the one socket of a native's first argument.
pub(crate) fn wait_on_socket<R>(
    req: &mut Request,
    socket: Instance<Socket>,
    wait: impl FnOnce(RawFd) -> R,
) -> Waited<R> {
    let mut wait = Some(wait);
    let waited = wait_on(req, &[socket.object()], |fds| {
        fds.first().zip(wait.take()).map(|(fd, wait)| wait(*fd))
    });
    match waited {
        Waited::Answered(Some(answered)) => Waited::Answered(answered),
        Waited::Answered(None) => Waited::Closed {
            at: 0,
            answered: None,
        },
        Waited::Closed { at, answered } => Waited::Closed {
            at,
            answered: answered.flatten(),
        },
    }
}

/// The handles a wait held put back where each socket is still open, and
/// let go where a callback closed it in the wait.
fn give_back(req: &mut Request, held: Vec<(Owner<PhpObject>, Handle)>) {
    for (object, handle) in held {
        let closed = match object.native_mut::<Sock>(req) {
            Some(sock) => match std::mem::take(&mut sock.handle) {
                Handle::Waiting { .. } => {
                    sock.handle = handle;
                    None
                }
                _ => Some(handle),
            },
            None => Some(handle),
        };
        if let Some(handle) = closed {
            release(req, handle);
        }
        object.give_up(req);
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
///
/// `socket_create_object` allocates a `php_socket`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// $m = memory_get_usage();
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 80
/// ```
#[php_class(
    final,
    not_serializable,
    size = 88, payload = Sock,
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

/// What the table refuses: construction, cloning and comparison.
const REFUSALS: Refusals = Refusals::NONE
    .construction(Some("socket_create()"))
    .cloning()
    .comparison();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `socket_free_obj`: the fd closed where the socket owns it,
    /// its owner of the stream let go where a stream does.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let handle = object
            .native_mut::<Sock>(req)
            .map(|sock| std::mem::take(&mut sock.handle));
        if let Some(Handle::Stream { stream, .. }) = handle {
            stream.give_up(req);
        }
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(stream) = object.native::<Sock>(req).and_then(Sock::stream) {
            visit(Value::resource(stream));
        }
        GcTable::Properties
    }
}

/// `object_init_ex(return_value, socket_ce)` with the payload laid:
/// a new `Socket` holding `sock`.
pub(crate) fn new_socket(req: &mut Request, sock: Sock) -> Result<Owner<Instance<Socket>>, Thrown> {
    let object = made(req, b"Socket")?;
    if let Some(payload) = object.native_mut::<Sock>(req) {
        *payload = sock;
    }
    Ok(object.into_instance())
}

/// A new object of a class this extension declares.
pub(crate) fn made(req: &mut Request, class: &[u8]) -> Result<Owner<PhpObject>, Thrown> {
    let Some(id) = lookup_class(req, class)? else {
        let text = format!("Class \"{}\" not found", String::from_utf8_lossy(class));
        return Err(throw_error(req, "Error", &text));
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
        None => Err(closed(req, position)),
    }
}

/// `ENSURE_SOCKET_VALID`'s `Error` for the socket at argument
/// `position`.
pub(crate) fn closed(req: &mut Request, position: u32) -> Thrown {
    argument_error(req, "Error", position, "has already been closed")
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
    req.environment().os_error_text(error)
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
