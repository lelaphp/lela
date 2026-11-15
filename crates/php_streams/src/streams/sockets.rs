//! The socket kinds and their `transports` (design §4.3 seam 5): php's
//! `main/streams/xp_socket.c` as one kind over the socket `std` opened,
//! labelled as the ops php picked for it (`tcp_socket`, `udp_socket`,
//! `unix_socket`, `udg_socket`, and `generic_socket` for a socket the
//! stream was handed), and `main/streams/transports.c`'s
//! `php_stream_xport_create`, which reads a `transport` off the front of a
//! name and connects or binds a socket of that kind. The four `transports`
//! are the four `PHP_MINIT(standard)` registers; an extension registers
//! more ([`XportFactory`]), each a `tcp` socket with the extension's crypto
//! layer beside it, which reads and writes in the socket's place once
//! active and is set up and enabled through `PHP_STREAM_OPTION_CRYPTO_API`
//! ([`Stream::crypto_setup`], [`Stream::crypto_enable`]).
//!
//! A read of a blocking socket with a timeout waits for the socket as
//! `php_sock_stream_wait_for_data` does, through `poll(2)`, which is
//! `php_heap`'s, and records the timeout `stream_get_meta_data` reports;
//! a read of a socket that does not block answers what is there, and the
//! kind never waits for a write.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::coerce::atoi;
use crate::api::layer::{Readiness, SockOpt};
use crate::api::values::{Emalloc, Owner};

use super::{CastTarget, MetaData, Ops, PhpStream, Ret, StatBuf, StreamOp, stat_buf, strerror};
use crate::api::raising::{Level, Thrown, raise, warning};
use crate::api::request::{
    BindTo, Cast, Crypto, CryptoSocket, Handshake, Ini, OptionAnswer, OwnedStream, Request,
    SocketOptions, Stream, StreamCtx, StreamOption, Xport, XportFactory, XportFailure, XportStep,
};
use crate::api::values::{PhpArray, PhpString};
use crate::streams::StreamLayer;
use crate::streams::persistent::Persistent;

/// `STREAM_OOB` and `STREAM_PEEK`, the bits of `stream_socket_sendto`'s
/// and `stream_socket_recvfrom`'s `$flags`.
/// `sizeof(php_netstream_data_t)` on the pinned 64-bit build: the
/// socket, `is_blocked` and `timeout_event` and their padding, the
/// `timeout` timeval and `ownsize` (`Z/main/php_network.h:319-325`).
pub(crate) const NETSTREAM_DATA_SIZE: usize = 32;

const STREAM_OOB: i64 = 1;
const STREAM_PEEK: i64 = 2;

/// `ETIMEDOUT`, `PHP_TIMEOUT_ERROR_VALUE`: what a connect or an accept
/// that ran out of time reports, as `strerror` names it on this host.
const ETIMEDOUT: i32 = 60;

/// `ENOTCONN`, what a read of a listening socket meets.
const ENOTCONN: i32 = 57;

/// `EMSGSIZE`, which `php_sockop_set_option`'s liveness check counts as
/// transient beside `EAGAIN`, as this host numbers it.
const EMSGSIZE: i32 = 40;

/// One operation of `PHP_STREAM_OPTION_XPORT_API` past connecting and
/// binding, which [`Request::xport_create`] does itself.
pub(crate) enum XportOp<'a> {
    /// `STREAM_XPORT_OP_ACCEPT`: the next client within `timeout`.
    Accept {
        timeout: Option<Duration>,
        want_peer: bool,
    },
    /// `STREAM_XPORT_OP_GET_NAME` or, with `peer`,
    /// `STREAM_XPORT_OP_GET_PEER_NAME`.
    Name { peer: bool },
    /// `STREAM_XPORT_OP_SEND`, to `to` where the socket takes one.
    Send {
        buf: &'a [u8],
        to: Option<SocketAddr>,
    },
    /// `STREAM_XPORT_OP_RECV`, leaving the bytes queued under `peek`.
    Recv {
        buf: &'a mut [u8],
        peek: bool,
        want_addr: bool,
    },
    /// `STREAM_XPORT_OP_SHUTDOWN`.
    Shutdown(Shutdown),
}

/// What a socket kind answered one [`XportOp`].
pub(crate) enum Xported {
    /// The client's kind and its address, or the error text.
    Accepted(Result<(Socket, Option<Vec<u8>>), String>),
    /// The address as `php_network_populate_name_from_sockaddr` writes
    /// it, `None` where the call failed.
    Name(Option<Vec<u8>>),
    /// How many bytes went, `None` for `-1` after php's warning.
    Sent(Option<usize>),
    /// How many bytes came and from where, `None` for `-1`.
    Received(Option<(usize, Option<Vec<u8>>)>),
    /// Whether `shutdown(2)` answered `0`.
    Shut(bool),
}

/// The socket `std` holds, one variant per shape php's `socket(2)` was
/// asked for, and the one a stream was handed.
#[derive(php_engine::api::abi::EngineState)]
pub(crate) enum Sock {
    Tcp(TcpStream),
    TcpServer(TcpListener),
    Udp(UdpSocket),
    Unix(UnixStream),
    UnixServer(UnixListener),
    Udg(UnixDatagram),
    /// A socket the stream was handed, whatever its domain and type;
    /// `UnixStream` is only the shape of its calls, `read(2)`,
    /// `write(2)` and `shutdown(2)`, which any socket takes.
    Generic(UnixStream),
}

impl Sock {
    fn fd(&self) -> BorrowedFd<'_> {
        match self {
            Sock::Tcp(s) => s.as_fd(),
            Sock::TcpServer(s) => s.as_fd(),
            Sock::Udp(s) => s.as_fd(),
            Sock::Unix(s) | Sock::Generic(s) => s.as_fd(),
            Sock::UnixServer(s) => s.as_fd(),
            Sock::Udg(s) => s.as_fd(),
        }
    }

    /// `recv(2)`.
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => (&*s).read(buf),
            Sock::Udp(s) => s.recv(buf),
            Sock::Unix(s) | Sock::Generic(s) => (&*s).read(buf),
            Sock::Udg(s) => s.recv(buf),
            Sock::TcpServer(_) | Sock::UnixServer(_) => {
                Err(std::io::Error::from_raw_os_error(ENOTCONN))
            }
        }
    }

    /// `send(2)`.
    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => (&*s).write(buf),
            Sock::Udp(s) => s.send(buf),
            Sock::Unix(s) | Sock::Generic(s) => (&*s).write(buf),
            Sock::Udg(s) => s.send(buf),
            Sock::TcpServer(_) | Sock::UnixServer(_) => {
                Err(std::io::Error::from_raw_os_error(ENOTCONN))
            }
        }
    }

    /// `php_set_sock_blocking`: `O_NONBLOCK` cleared or set.
    fn set_blocking(&self, blocking: bool) -> std::io::Result<()> {
        let nonblocking = !blocking;
        match self {
            Sock::Tcp(s) => s.set_nonblocking(nonblocking),
            Sock::TcpServer(s) => s.set_nonblocking(nonblocking),
            Sock::Udp(s) => s.set_nonblocking(nonblocking),
            Sock::Unix(s) | Sock::Generic(s) => s.set_nonblocking(nonblocking),
            Sock::UnixServer(s) => s.set_nonblocking(nonblocking),
            Sock::Udg(s) => s.set_nonblocking(nonblocking),
        }
    }
}

/// `php_netstream_data_t` under one of `xp_socket.c`'s ops, as a request
/// holds it: the [`Connection`], which outlives the request where the
/// stream is persistent, and beside it what of the request the socket
/// keeps, dropped as the connection leaves it ([`Socket::detach`]): the
/// allocations php makes for the struct, and `PHP_STREAM_CONTEXT(stream)`
/// and the stream itself, which the crypto layer is lent.
pub(crate) struct Socket {
    /// The `php_netstream_data_t` php `pemalloc`s, [`NETSTREAM_DATA_SIZE`]
    /// bytes, or what a registered `transport`'s factory allocates in its
    /// place ([`Crypto::allocations`]), on the request's heap from
    /// [`Ops::lay_down`] to the close; none for a persistent stream's,
    /// which php allocates persistent.
    data: Vec<Emalloc>,
    context: Option<StreamCtx>,
    own: Option<Stream>,
    conn: Connection,
}

/// What of a socket outlives the request that opened it: the socket,
/// `None` once closed; the ops' label; `is_blocked`; the read timeout,
/// `None` for php's `tv_sec == -1`; `timeout_event`; whether the stream
/// is a server's ([`Role`]), which only this kind reads; and a registered
/// `transport`'s crypto layer, dropped before the socket closes. Engine
/// state by type: a persistent stream's connection stands in the
/// engine's persistent list, or in the extension that kept it, between
/// requests ([`persistent::Entry`]), and no field of it can hold a
/// request's value.
///
/// [`persistent::Entry`]: super::persistent::Entry
#[derive(php_engine::api::abi::EngineState)]
pub(crate) struct Connection {
    crypto: CryptoLayer,
    /// The socket, shared with a crypto layer's call that runs over it
    /// while PHP code it serves reaches the stream.
    sock: Option<Arc<Sock>>,
    label: &'static str,
    is_blocked: bool,
    timeout: Option<Duration>,
    timeout_event: bool,
    role: Role,
    /// `PHP_STREAM_FLAG_SUPPRESS_ERRORS`: a failed send raises no notice.
    suppress_errors: bool,
}

/// A socket's crypto layer: none, the layer, or the layer out for a
/// call that runs with the request ([`StreamOp::with_crypto`]), which PHP code
/// the call serves finds in use.
#[derive(php_engine::api::abi::EngineState)]
enum CryptoLayer {
    Absent,
    On(Box<dyn Crypto>),
    Lent,
}

impl CryptoLayer {
    fn on(&self) -> Option<&dyn Crypto> {
        match self {
            CryptoLayer::On(layer) => Some(layer.as_ref()),
            CryptoLayer::Absent | CryptoLayer::Lent => None,
        }
    }

    fn on_mut(&mut self) -> Option<&mut dyn Crypto> {
        match self {
            CryptoLayer::On(layer) => Some(layer.as_mut()),
            CryptoLayer::Absent | CryptoLayer::Lent => None,
        }
    }

    /// The layer taken out for a call, its place marked in use; `None`
    /// for a socket with none or one out already.
    fn lend(&mut self) -> Option<Box<dyn Crypto>> {
        match std::mem::replace(self, CryptoLayer::Lent) {
            CryptoLayer::On(layer) => Some(layer),
            other => {
                *self = other;
                None
            }
        }
    }

    const fn is_lent(&self) -> bool {
        matches!(self, CryptoLayer::Lent)
    }
}

impl From<Option<Box<dyn Crypto>>> for CryptoLayer {
    fn from(layer: Option<Box<dyn Crypto>>) -> CryptoLayer {
        layer.map_or(CryptoLayer::Absent, CryptoLayer::On)
    }
}

/// Whether a socket stream is a server's: `PHP_STREAM_FLAG_NO_IO`, which
/// `php_stream_xport_create` sets on a stream it bound
/// (`Z/main/streams/transports.c:175`), with the `tcp_nodelay` option
/// the server was made with, which `php_tcp_sockop_accept` gives each
/// client it accepts; read when the server is made, where php reads it
/// off the server's `PHP_STREAM_CONTEXT` at each accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_engine::api::abi::EngineState)]
enum Role {
    Peer,
    Server { accept_nodelay: bool },
}

impl Connection {
    fn new(sock: Option<Sock>, label: &'static str, timeout: Option<Duration>) -> Connection {
        Connection {
            crypto: CryptoLayer::Absent,
            sock: sock.map(Arc::new),
            label,
            is_blocked: true,
            timeout,
            timeout_event: false,
            role: Role::Peer,
            suppress_errors: false,
        }
    }
}

impl From<Connection> for Socket {
    /// The connection under a socket a request holds, which keeps nothing
    /// of the request yet: the stream it serves adopts it
    /// ([`Ops::adopt`]), and a `context` set on that stream reaches it
    /// ([`Socket::set_ctx`]).
    fn from(conn: Connection) -> Socket {
        Socket {
            data: Vec::new(),
            context: None,
            own: None,
            conn,
        }
    }
}

impl Socket {
    fn new(sock: Option<Sock>, label: &'static str, timeout: Option<Duration>) -> Socket {
        Socket::from(Connection::new(sock, label, timeout))
    }

    /// Whether the crypto layer reads and writes in the socket's place,
    /// `ssl_active`.
    fn crypto_active(&self) -> bool {
        self.conn.crypto.on().is_some_and(Crypto::active)
    }

    /// The crypto layer and the socket it runs over, lent for one call,
    /// with `supplemental` the stream's `has_buffered_data`; the socket's
    /// `is_blocked` and `timeout_event` taken back after. `None` for a
    /// socket with no layer or one closed.
    fn with_layer<R>(
        &mut self,
        supplemental: bool,
        f: impl FnOnce(&mut dyn Crypto, &mut CryptoSocket<'_>) -> R,
    ) -> Option<R> {
        let Socket {
            context,
            own,
            conn:
                Connection {
                    crypto,
                    sock,
                    is_blocked,
                    timeout,
                    timeout_event,
                    ..
                },
            ..
        } = self;
        let layer = crypto.on_mut()?;
        let sock = sock.as_ref()?;
        let mut lent = CryptoSocket {
            fd: sock.fd(),
            blocked: *is_blocked,
            timeout: *timeout,
            timed_out: *timeout_event,
            supplemental,
            ctx: *context,
            stream: *own,
            live: None,
        };
        let out = f(layer, &mut lent);
        *is_blocked = lent.blocked;
        *timeout_event = lent.timed_out;
        Some(out)
    }

    /// The crypto layer, where the socket has one.
    pub(crate) fn crypto_layer(&mut self) -> Option<&mut dyn Crypto> {
        self.conn.crypto.on_mut()
    }

    /// `SSL_pending` of an active layer, `0` for every other socket.
    pub(crate) fn crypto_pending(&self) -> usize {
        self.conn
            .crypto
            .on()
            .filter(|layer| layer.active())
            .map_or(0, Crypto::pending)
    }

    /// `PHP_STREAM_CONTEXT(stream)`, as the stream set it, for the layer.
    pub(crate) fn set_ctx(&mut self, context: Option<StreamCtx>) {
        self.context = context;
    }

    /// The connection taken out of the stream it served, which is left
    /// holding no socket, and with it what of the request the socket
    /// kept, its allocations, its stream and its `stream->ctx`, to go
    /// with the stream (`forget_persistent_resource_id_numbers`): what a
    /// persistent stream's resource keeps in the persistent list when its
    /// last owner lets go, and what [`OwnedStream::keep`] keeps.
    pub(crate) fn detach(&mut self) -> Connection {
        let empty = Connection::new(None, self.conn.label, self.conn.timeout);
        std::mem::replace(&mut self.conn, empty)
    }

    /// `recv(2)` with `MSG_DONTWAIT` on a socket that blocks: the socket
    /// made non-blocking for the one call.
    fn read_now(sock: &Sock, buf: &mut [u8]) -> std::io::Result<usize> {
        sock.set_blocking(false)?;
        let read = sock.read(buf);
        sock.set_blocking(true).ok();
        read
    }
}

impl StreamOp<'_> {
    /// The socket's crypto layer lent out for `f` over the socket, which
    /// stays in the stream, so PHP code the layer serves, an error handler
    /// a warning of the handshake calls or a callback of the layer's,
    /// reaches the live socket: `is_blocked` as
    /// [`CryptoSocket::set_blocking`] leaves it, and the layer itself in
    /// use. `None` for a stream that is no socket, a socket with no
    /// layer, and a layer in use by a call further out.
    fn with_crypto<R>(
        &mut self,
        f: impl FnOnce(&mut Request, &mut dyn Crypto, &mut CryptoSocket<'_>) -> R,
    ) -> Option<R> {
        let (mut layer, sock, fields) = self.with(|s| {
            let socket = s.ops.as_socket()?;
            let sock = Arc::clone(socket.conn.sock.as_ref()?);
            let layer = socket.conn.crypto.lend()?;
            let fields = (
                socket.conn.is_blocked,
                socket.conn.timeout,
                socket.conn.timeout_event,
            );
            Some((layer, sock, (fields, socket.context, socket.own)))
        })??;
        let ((blocked, timeout, timed_out), ctx, stream) = fields;
        let live = self.stream();
        let mut lent = CryptoSocket {
            fd: sock.fd(),
            blocked,
            timeout,
            timed_out,
            supplemental: false,
            ctx,
            stream,
            live,
        };
        let out = f(self.req, layer.as_mut(), &mut lent);
        // `is_blocked` went through as the layer set it; `timeout_event`
        // goes back only where the layer set it, so what PHP code the call
        // served did to either stands.
        let set_timed_out = (lent.timed_out != timed_out).then_some(lent.timed_out);
        self.with(|s| {
            if let Some(socket) = s.ops.as_socket() {
                socket.conn.crypto = CryptoLayer::On(layer);
                if let Some(timed_out) = set_timed_out {
                    socket.conn.timeout_event = timed_out;
                }
            }
        });
        Some(out)
    }

    /// Whether the socket's crypto layer is out for a call further out,
    /// which PHP code that call serves asks to set up or enable again,
    /// and which fails as php's `in_free` answer does.
    fn crypto_in_use(&mut self) -> bool {
        self.with(|s| {
            s.ops
                .as_socket()
                .is_some_and(|socket| socket.conn.crypto.is_lent())
        }) == Some(true)
    }

    /// `STREAM_XPORT_CRYPTO_OP_SETUP`: `None` for a socket with no crypto
    /// layer, php's `PHP_STREAM_OPTION_RETURN_NOTIMPL`.
    fn crypto_setup(&mut self, method: i64, session: Option<Stream>) -> Option<bool> {
        if self.crypto_in_use() {
            return Some(false);
        }
        let setup = self.with_crypto(|req, layer, lent| layer.setup(req, lent, method, session))?;
        Some(setup.unwrap_or_else(|thrown| {
            super::filters::defer(self.req, thrown);
            false
        }))
    }

    /// `STREAM_XPORT_CRYPTO_OP_ENABLE`: `None` for a socket with no crypto
    /// layer.
    fn crypto_enable(&mut self, activate: bool) -> Option<Handshake> {
        if self.crypto_in_use() {
            return Some(Handshake::Failed);
        }
        let enabled = self.with_crypto(|req, layer, lent| layer.enable(req, lent, activate))?;
        Some(enabled.unwrap_or_else(|thrown| {
            super::filters::defer(self.req, thrown);
            Handshake::Failed
        }))
    }

    /// The layer's [`Crypto::connected`], after the connect or the accept
    /// that made the socket; nothing to do for a socket with no layer.
    fn crypto_connected(&mut self) -> Result<(), String> {
        match self.with_crypto(|req, layer, lent| layer.connected(req, lent)) {
            Some(Ok(connected)) => connected,
            Some(Err(thrown)) => {
                super::filters::defer(self.req, thrown);
                Err(String::new())
            }
            None => Ok(()),
        }
    }

    /// What the socket's crypto layer shows under `crypto`, `None` for a
    /// stream with no layer or an inactive one.
    pub(crate) fn crypto_meta(&mut self) -> Option<Result<Option<Owner<PhpArray>>, Thrown>> {
        self.with_crypto(|req, layer, _| layer.meta(req))
    }

    /// One `transport` operation of the socket's kind, the kind lent out
    /// for it.
    fn xport(&mut self, op: XportOp<'_>) -> Option<Xported> {
        self.kind(|req, ops| ops.xport(req, op)).flatten()
    }
}

/// [`CryptoSocket::set_blocking`] on the socket a crypto layer's call is
/// lent over, written through to the stream as it stands, so PHP code the
/// call serves next reads the socket's `is_blocked` as the layer left it.
pub(crate) fn show_blocking(req: &mut Request, stream: Stream, blocked: bool) {
    req.stream_with(stream, |s| {
        if let Some(socket) = s.ops.as_socket() {
            socket.conn.is_blocked = blocked;
        }
    });
}

/// A timeout of seconds as `default_socket_timeout` holds it: `None`, no
/// limit, for a negative one.
pub(crate) fn seconds(timeout: i64) -> Option<Duration> {
    u64::try_from(timeout).ok().map(Duration::from_secs)
}

/// `FG(default_socket_timeout)`, the ini entry's seconds, the quantity
/// its `OnUpdateLong` read; sixty where the bundle declares none.
fn default_timeout(req: &Request) -> Option<Duration> {
    let secs = (req.ini().get(b"default_socket_timeout")).map_or(60, Ini::parse_bytes);
    seconds(secs)
}

/// `php_network_populate_name_from_sockaddr` (`Z/main/network.c:645`):
/// an inet address as `ip:port`, `[ip]:port` for IPv6.
fn inet_text(addr: &SocketAddr) -> Vec<u8> {
    match addr {
        SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()).into_bytes(),
        SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()).into_bytes(),
    }
}

/// A unix socket's address as the same function writes it: the path,
/// and nothing for one with none.
fn unix_text(addr: &std::os::unix::net::SocketAddr) -> Vec<u8> {
    addr.as_pathname()
        .map(|path| path.as_os_str().as_bytes().to_vec())
        .unwrap_or_default()
}

/// `php_socket_error_str`: `strerror` of the `errno` an error carries,
/// and of `ETIMEDOUT` for `std`'s own timeout, which carries none.
fn socket_error(error: &std::io::Error) -> (i32, String) {
    if error.raw_os_error().is_none() && error.kind() == std::io::ErrorKind::TimedOut {
        let timed_out = std::io::Error::from_raw_os_error(ETIMEDOUT);
        return (ETIMEDOUT, strerror(&timed_out));
    }
    (error.raw_os_error().unwrap_or(0), strerror(error))
}

impl Ops for Socket {
    fn label(&self) -> &'static str {
        self.conn.label
    }

    fn counts_progress(&self) -> bool {
        true
    }

    /// `php_sockop_write` (`Z/main/streams/xp_socket.c:63`): one
    /// `send(2)`, a socket that does not block answering `0` where it
    /// would, and php's notice with `errno` where it failed, unless the
    /// stream was marked [`StreamOption::SuppressErrors`].
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        if self.crypto_active() {
            return self
                .with_layer(false, |layer, lent| layer.write(req, lent, buf))
                .flatten();
        }
        let Some(sock) = self.conn.sock.as_deref() else {
            return Some(0);
        };
        match sock.write(buf) {
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(0),
            Err(_) if self.conn.suppress_errors => None,
            Err(error) => {
                let (code, text) = socket_error(&error);
                let text = format!(
                    "{}(): Send of {} bytes failed with errno={code} {text}",
                    req.frames().active_function_name(),
                    buf.len()
                );
                raise(req, Level::Notice, &text);
                None
            }
        }
    }

    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        self.read_after_buffer(req, buf, eof, false)
    }

    /// `php_sockop_read` (`Z/main/streams/xp_socket.c:161`), or the crypto
    /// layer's read where it is active: a socket that blocks waits for data
    /// up to its timeout, the wait that runs out setting `timeout_event`
    /// and answering `-1`, and one with a zero timeout does not wait, nor
    /// does a read after the stream's buffer already gave bytes
    /// (`has_buffered_data`); then `recv(2)`, `0` bytes where the socket
    /// would block, eof at the peer's close and at an error.
    fn read_after_buffer(
        &mut self,
        req: &mut Request,
        buf: &mut [u8],
        eof: &mut bool,
        has_buffered_data: bool,
    ) -> Option<usize> {
        if self.crypto_active() {
            let read = self.with_layer(has_buffered_data, |layer, lent| {
                layer.read(req, lent, buf, eof)
            });
            return match read {
                Some(Ok(read)) => read,
                Some(Err(thrown)) => {
                    super::filters::defer(req, thrown);
                    None
                }
                None => None,
            };
        }
        let sock = self.conn.sock.as_deref()?;
        let mut dont_wait = false;
        if self.conn.is_blocked {
            dont_wait = has_buffered_data || self.conn.timeout == Some(Duration::ZERO);
            if !dont_wait {
                self.conn.timeout_event = false;
                let timeout = self.conn.timeout;
                loop {
                    let polled = req.limits_mut().blocking(|| {
                        crate::api::layer::poll_fd(sock.fd(), Readiness::Readable, timeout)
                    });
                    match polled {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Ok(true) | Err(_) => break,
                        Ok(false) => {
                            self.conn.timeout_event = true;
                            return None;
                        }
                    }
                }
            }
        }
        let read = if dont_wait {
            Socket::read_now(sock, buf)
        } else {
            req.limits_mut().blocking(|| sock.read(buf))
        };
        match read {
            Ok(0) => {
                *eof = true;
                Some(0)
            }
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(0),
            Err(_) => {
                *eof = true;
                None
            }
        }
    }

    /// `php_sockop_close`: `closesocket`, which the drop is, after the
    /// crypto layer's own close, `SSL_shutdown` and free, which its drop
    /// is.
    fn close(&mut self, req: &mut Request) -> i32 {
        self.conn.crypto = CryptoLayer::Absent;
        self.conn.sock = None;
        for data in std::mem::take(&mut self.data) {
            data.free(req);
        }
        0
    }

    /// `php_stream_generic_socket_factory`'s
    /// `sock = pemalloc(sizeof(php_netstream_data_t), 0)`, or what a
    /// registered `transport`'s factory allocates in its place.
    ///
    /// ```php
    /// [$x, $y] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0); fclose($x); fclose($y); unset($x, $y);
    /// $m = memory_get_usage(); [$x, $y] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0); echo memory_get_usage() - $m, "\n";
    /// fclose($x); fclose($y); unset($x, $y); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 576
    /// 32
    /// ```
    fn lay_down(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let sizes = self
            .conn
            .crypto
            .on()
            .map_or_else(|| vec![NETSTREAM_DATA_SIZE], Crypto::allocations);
        for size in sizes {
            self.data.push(Emalloc::new(req, size)?);
        }
        Ok(())
    }

    fn as_socket(&mut self) -> Option<&mut Socket> {
        Some(self)
    }

    fn adopt(&mut self, own: Stream) {
        self.own = Some(own);
    }

    /// `php_sockop_stat`: `fstat(2)` of the socket.
    fn stat(&mut self, _req: &mut Request) -> Option<StatBuf> {
        let fd = self.conn.sock.as_ref()?.fd().try_clone_to_owned().ok()?;
        let meta = std::fs::File::from(fd).metadata().ok()?;
        Some(stat_buf(&meta))
    }

    fn meta_data(&self) -> MetaData {
        MetaData::Socket {
            timed_out: self.conn.timeout_event,
            blocked: self.conn.is_blocked,
        }
    }

    /// `php_sockop_cast`: the socket, for any cast to a fd; with an active
    /// crypto layer, `php_openssl_sockop_cast` refuses every cast but the
    /// one `select(2)` asks for, since the bytes on the socket are not the
    /// stream's.
    fn cast_target(
        &mut self,
        _req: &mut Request,
        cast: Cast,
        _own: Option<Stream>,
        _ret: Ret,
    ) -> CastTarget<'_> {
        if cast != Cast::Select && self.crypto_active() {
            return CastTarget::Refused;
        }
        self.conn
            .sock
            .as_ref()
            .map_or(CastTarget::Refused, |sock| CastTarget::Fd(sock.fd()))
    }

    /// `php_sockop_set_option` (`Z/main/streams/xp_socket.c:333`):
    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` is [`Socket::alive`], or, with an
    /// active crypto layer, the layer's answer once the socket polls
    /// readable without waiting and alive where it does not
    /// (`php_openssl_sockop_set_option`),
    /// `PHP_STREAM_OPTION_BLOCKING` is `php_set_sock_blocking`, and
    /// `PHP_STREAM_OPTION_READ_TIMEOUT` the timeout a read waits for.
    fn set_option(&mut self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        match option {
            StreamOption::Blocking(blocking) => {
                let Some(sock) = self.conn.sock.as_ref() else {
                    return OptionAnswer::Err;
                };
                if sock.set_blocking(blocking).is_err() {
                    return OptionAnswer::Err;
                }
                self.conn.is_blocked = blocking;
                OptionAnswer::Ok
            }
            StreamOption::ReadTimeout(timeout) => {
                self.conn.timeout = Some(timeout);
                self.conn.timeout_event = false;
                OptionAnswer::Ok
            }
            StreamOption::SuppressErrors(suppress) => {
                self.conn.suppress_errors = suppress;
                OptionAnswer::Ok
            }
            StreamOption::Liveness if self.crypto_active() => {
                let readable = self.conn.sock.as_ref().is_some_and(|sock| {
                    matches!(
                        crate::api::layer::poll_fd(
                            sock.fd(),
                            Readiness::Readable,
                            Some(Duration::ZERO)
                        ),
                        Ok(true)
                    )
                });
                let alive = !readable
                    || self
                        .with_layer(false, |layer, lent| layer.alive(req, lent))
                        .unwrap_or(false);
                if alive {
                    OptionAnswer::Ok
                } else {
                    OptionAnswer::Err
                }
            }
            StreamOption::Liveness => {
                if self.alive() {
                    OptionAnswer::Ok
                } else {
                    OptionAnswer::Err
                }
            }
            _ => OptionAnswer::NotImplemented,
        }
    }

    fn xport(&mut self, req: &mut Request, op: XportOp<'_>) -> Option<Xported> {
        if let XportOp::Accept { timeout, want_peer } = op {
            // `stream_socket_accept`'s wait for a client lends the
            // request ([`Limits::blocking`]).
            let accepted = req
                .limits_mut()
                .blocking(|| self.accept(timeout, want_peer));
            return Some(Xported::Accepted(accepted));
        }
        let sock = self.conn.sock.as_deref()?;
        Some(match op {
            XportOp::Accept { .. } => return None,
            XportOp::Name { peer } => Xported::Name(name(sock, peer)),
            XportOp::Send { buf, to } => {
                let sent = match (sock, to) {
                    (Sock::Udp(s), Some(to)) => s.send_to(buf, to),
                    (sock, _) => sock.write(buf),
                };
                Xported::Sent(match sent {
                    Ok(n) => Some(n),
                    Err(error) => {
                        let text = format!(
                            "{}(): {}\n",
                            req.frames().active_function_name(),
                            strerror(&error)
                        );
                        warning(req, &text);
                        None
                    }
                })
            }
            XportOp::Recv {
                buf,
                peek,
                want_addr,
            } => Xported::Received(recv(sock, buf, peek, want_addr)),
            XportOp::Shutdown(how) => Xported::Shut(match sock {
                Sock::Tcp(s) => s.shutdown(how).is_ok(),
                Sock::Unix(s) | Sock::Generic(s) => s.shutdown(how).is_ok(),
                Sock::Udg(s) => s.shutdown(how).is_ok(),
                _ => false,
            }),
        })
    }
}

impl Socket {
    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` as `php_sockop_set_option`
    /// answers it (`Z/main/streams/xp_socket.c:342-386`) for the `0`
    /// that `php_stream_eof` and `php_stream_xport_create` ask with: a
    /// closed socket is dead; a server's stream (`PHP_STREAM_FLAG_NO_IO`)
    /// is polled for a readable socket without waiting and is alive where
    /// none is; then one byte is peeked without waiting, `MSG_PEEK |
    /// MSG_DONTWAIT`, and the peer's orderly close, which leaves none,
    /// is dead, as a failure other than `EAGAIN` or `EMSGSIZE` is. So
    /// `feof()` sees a peer that went away before any read finds the end,
    /// and a listening socket with a connection waiting, whose peek
    /// fails with `ENOTCONN`.
    ///
    /// Each `true` below is read after `stream_select` has seen the end or
    /// the connection arrive, since the check waits for nothing and the
    /// kernel hands a close or a connection to the other end when it
    /// does: read without the wait, a loaded machine answers `false` there
    /// now and then, under the oracle and this engine alike. Each
    /// `false` holds whether or not anything has arrived yet.
    ///
    /// ```php
    /// $ready = function ($s) { $r = [$s]; $w = $e = null; stream_select($r, $w, $e, 10); };
    /// [$a, $b] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
    /// var_dump(feof($b));
    /// fwrite($a, "x"); fclose($a);
    /// var_dump(feof($b), fread($b, 10));
    /// $ready($b);
    /// var_dump(feof($b));
    /// [$c, $d] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
    /// stream_set_blocking($d, false);
    /// fclose($c);
    /// $ready($d);
    /// var_dump(feof($d));
    /// $s = stream_socket_server('tcp://127.0.0.1:0');
    /// var_dump(feof($s));
    /// $client = stream_socket_client(stream_socket_get_name($s, false));
    /// $ready($s);
    /// var_dump(feof($s), feof($client));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(1) "x"
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    fn alive(&self) -> bool {
        let Some(sock) = self.conn.sock.as_ref() else {
            return false;
        };
        if matches!(self.conn.role, Role::Server { .. })
            && !matches!(
                crate::api::layer::poll_fd(sock.fd(), Readiness::Readable, Some(Duration::ZERO)),
                Ok(true)
            )
        {
            return true;
        }
        let mut byte = [0u8; 1];
        let peeked = if self.conn.is_blocked {
            // `MSG_DONTWAIT` on a socket that blocks: made non-blocking
            // for the one call, as [`Socket::read_now`] makes it.
            sock.set_blocking(false).ok();
            let peeked = crate::api::layer::peek_fd(sock.fd(), &mut byte);
            sock.set_blocking(true).ok();
            peeked
        } else {
            crate::api::layer::peek_fd(sock.fd(), &mut byte)
        };
        match peeked {
            Ok(n) => n > 0,
            Err(error) => {
                error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(EMSGSIZE)
            }
        }
    }

    /// `php_tcp_sockop_accept` over `php_network_accept_incoming`
    /// (`Z/main/network.c:772`): the listener waited for up to `timeout`,
    /// `ETIMEDOUT` where it ran out, then `accept(2)`; the client a socket
    /// of the listener's kind, blocking and timeout as the listener's,
    /// and its address where it was asked for.
    fn accept(
        &mut self,
        timeout: Option<Duration>,
        want_peer: bool,
    ) -> Result<(Socket, Option<Vec<u8>>), String> {
        let sock = self.conn.sock.as_deref().ok_or_else(String::new)?;
        match crate::api::layer::poll_fd(sock.fd(), Readiness::Readable, timeout) {
            Ok(true) => {}
            Ok(false) => {
                return Err(socket_error(&std::io::Error::from_raw_os_error(ETIMEDOUT)).1);
            }
            Err(error) => return Err(socket_error(&error).1),
        }
        let (client, peer) = match sock {
            Sock::TcpServer(listener) => {
                let (client, addr) = listener.accept().map_err(|e| socket_error(&e).1)?;
                if self.conn.role
                    == (Role::Server {
                        accept_nodelay: true,
                    })
                {
                    client.set_nodelay(true).ok();
                }
                (Sock::Tcp(client), inet_text(&addr))
            }
            Sock::UnixServer(listener) => {
                let (client, addr) = listener.accept().map_err(|e| socket_error(&e).1)?;
                (Sock::Unix(client), unix_text(&addr))
            }
            // A socket the stream was handed and given the `tcp` ops,
            // `php://stdin` over a listening socket: `accept(2)` of
            // whatever family it is, as `php_network_accept_incoming` takes
            // any, through a copy of the fd shaped as that family's listener.
            Sock::Generic(handed) if self.conn.label != GENERIC => {
                let copy = handed.as_fd().try_clone_to_owned();
                let copy = copy.map_err(|e| socket_error(&e).1)?;
                let inet = TcpListener::from(copy);
                if inet.local_addr().is_ok() {
                    let (client, addr) = inet.accept().map_err(|e| socket_error(&e).1)?;
                    (Sock::Tcp(client), inet_text(&addr))
                } else {
                    let unix = UnixListener::from(OwnedFd::from(inet));
                    let (client, addr) = unix.accept().map_err(|e| socket_error(&e).1)?;
                    (Sock::Unix(client), unix_text(&addr))
                }
            }
            _ => {
                let error = std::io::Error::from_raw_os_error(95);
                return Err(socket_error(&error).1);
            }
        };
        let mut accepted = Socket::new(Some(client), self.conn.label, self.conn.timeout);
        accepted.conn.is_blocked = self.conn.is_blocked;
        accepted.conn.crypto = self.conn.crypto.on().map(Crypto::accepted).into();
        Ok((accepted, want_peer.then_some(peer)))
    }
}

/// `php_network_get_sock_name` and `php_network_get_peer_name`, which ask
/// the fd of whatever family it is, a socket `php://fd/` was handed
/// among them. The fd is found by name, as the socket's stream reads it:
///
/// ```php
/// function handed($s, $name) {
///     foreach (scandir('/dev/fd') as $n) {
///         if (ctype_digit($n) && ($h = @fopen("php://fd/$n", 'r+')) && stream_socket_get_name($h, false) === $name) { return $h; }
///     }
/// }
/// $t = stream_socket_server('tcp://127.0.0.1:0');
/// $h = handed($t, stream_socket_get_name($t, false));
/// var_dump(stream_get_meta_data($h)['stream_type'], stream_socket_get_name($h, true));
/// $path = sys_get_temp_dir() . '/s' . bin2hex(random_bytes(6)) . '.sock';
/// $u = stream_socket_server("unix://$path");
/// $h = handed($u, $path);
/// var_dump(stream_get_meta_data($h)['stream_type'], stream_socket_get_name($h, true));
/// unlink($path);
/// ```
/// ```expect
/// string(10) "tcp_socket"
/// bool(false)
/// string(10) "tcp_socket"
/// bool(false)
/// ```
fn name(sock: &Sock, peer: bool) -> Option<Vec<u8>> {
    let inet = |addr: std::io::Result<SocketAddr>| addr.ok().map(|a| inet_text(&a));
    let unix =
        |addr: std::io::Result<std::os::unix::net::SocketAddr>| addr.ok().map(|a| unix_text(&a));
    match (sock, peer) {
        (Sock::Tcp(s), false) => inet(s.local_addr()),
        (Sock::Tcp(s), true) => inet(s.peer_addr()),
        (Sock::TcpServer(s), false) => inet(s.local_addr()),
        (Sock::Udp(s), false) => inet(s.local_addr()),
        (Sock::Udp(s), true) => inet(s.peer_addr()),
        (Sock::Unix(s), false) => unix(s.local_addr()),
        (Sock::Unix(s), true) => unix(s.peer_addr()),
        (Sock::UnixServer(s), false) => unix(s.local_addr()),
        (Sock::Udg(s), false) => unix(s.local_addr()),
        (Sock::Udg(s), true) => unix(s.peer_addr()),
        (Sock::Generic(s), peer) => handed_name(s, peer),
        _ => None,
    }
}

/// [`name`] over a [`Sock::Generic`]: the family's own read through a
/// copy of the fd, an inet socket's where the kernel answers one and
/// the unix one's otherwise, as `accept` tells them apart.
fn handed_name(handed: &UnixStream, peer: bool) -> Option<Vec<u8>> {
    let inet = TcpStream::from(handed.as_fd().try_clone_to_owned().ok()?);
    let addr = if peer {
        inet.peer_addr()
    } else {
        inet.local_addr()
    };
    if let Ok(addr) = addr {
        return Some(inet_text(&addr));
    }
    let addr = if peer {
        handed.peer_addr()
    } else {
        handed.local_addr()
    };
    addr.ok().map(|addr| unix_text(&addr))
}

/// `sock_recvfrom` (`Z/main/streams/xp_socket.c:305`): `recvfrom(2)`, or
/// `recv(2)` with `MSG_PEEK`, and the sender's address as text where it
/// was asked for, empty where the socket gives none.
fn recv(
    sock: &Sock,
    buf: &mut [u8],
    peek: bool,
    want_addr: bool,
) -> Option<(usize, Option<Vec<u8>>)> {
    if peek {
        if let Sock::Udp(s) = sock {
            let (n, from) = s.peek_from(buf).ok()?;
            return Some((n, want_addr.then(|| inet_text(&from))));
        }
        let n = crate::api::layer::peek_fd(sock.fd(), buf).ok()?;
        return Some((n, want_addr.then(Vec::new)));
    }
    match sock {
        Sock::Udp(s) => {
            let (n, from) = s.recv_from(buf).ok()?;
            Some((n, want_addr.then(|| inet_text(&from))))
        }
        Sock::Udg(s) => {
            let (n, from) = s.recv_from(buf).ok()?;
            Some((n, want_addr.then(|| unix_text(&from))))
        }
        sock => {
            let n = sock.read(buf).ok()?;
            Some((n, want_addr.then(Vec::new)))
        }
    }
}

/// `parse_ip_address_ex` (`Z/main/streams/xp_socket.c:651`): the host
/// and the `port` of `host:port`, `[v6]:port` for IPv6, the `port` as
/// `atoi` reads it; the error text where there is no `port`.
fn parse_ip_address(name: &[u8]) -> Result<(String, u16), String> {
    let shown = String::from_utf8_lossy(name);
    if name.contains(&0) {
        return Err(String::from("The hostname must not contain null bytes"));
    }
    if name.first() == Some(&b'[') && name.len() > 1 {
        let inner = &name[1..name.len() - 1];
        let Some(close) = inner.iter().position(|b| *b == b']') else {
            return Err(format!("Failed to parse IPv6 address \"{shown}\""));
        };
        if name.get(close + 2) != Some(&b':') {
            return Err(format!("Failed to parse IPv6 address \"{shown}\""));
        }
        let host = String::from_utf8_lossy(&name[1..=close]).into_owned();
        return Ok((host, atoi_port(&name[close + 3..])));
    }
    let head = &name[..name.len().saturating_sub(1)];
    let Some(colon) = head.iter().position(|b| *b == b':') else {
        return Err(format!("Failed to parse address \"{shown}\""));
    };
    let host = String::from_utf8_lossy(&name[..colon]).into_owned();
    Ok((host, atoi_port(&name[colon + 1..])))
}

/// C's `atoi` of a `port`, cut to the `unsigned short` php hands
/// `htons`: the int's low 16 bits.
fn atoi_port(text: &[u8]) -> u16 {
    u16::try_from(atoi(text) & 0xFFFF).unwrap_or(0)
}

/// `php_network_getaddresses` (`Z/main/network.c:148`): the addresses
/// `getaddrinfo(3)` gives `host`, each with `port`; a lookup that fails
/// is php's warning, and its text is the error.
fn addresses(req: &mut Request, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    match (host, port).to_socket_addrs() {
        Ok(addrs) => Ok(addrs.collect()),
        Err(error) => {
            let text = error.to_string();
            let gai = text
                .strip_prefix("failed to lookup address information: ")
                .unwrap_or(&text);
            let message = format!("php_network_getaddresses: getaddrinfo for {host} failed: {gai}");
            let shown = format!("{}(): {message}", req.frames().active_function_name());
            warning(req, &shown);
            Err(message)
        }
    }
}

/// `sun_path`'s room less its terminating NUL, the longest path
/// `parse_unix_address` keeps.
const UNIX_PATH_MAX: usize = 103;

/// `parse_unix_address` (`Z/main/streams/xp_socket.c:608`): the path, cut
/// to what `sun_path` holds after php's notice.
fn unix_path(req: &mut Request, name: &[u8]) -> std::path::PathBuf {
    let mut path = name;
    if path.len() > UNIX_PATH_MAX {
        path = &path[..UNIX_PATH_MAX];
        let text = format!(
            "{}(): socket path exceeded the maximum allowed length of {UNIX_PATH_MAX} bytes and was truncated",
            req.frames().active_function_name()
        );
        raise(req, Level::Notice, &text);
    }
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path))
}

/// An error of `std`'s as the failure `php_stream_xport_create` reports
/// of `step`.
fn failure(step: XportStep, error: &std::io::Error) -> XportFailure {
    let (code, text) = socket_error(error);
    XportFailure {
        step,
        code,
        text: Some(text),
    }
}

/// A failure of `step` with php's text and no `errno`.
fn text_only(step: XportStep, text: String) -> XportFailure {
    XportFailure {
        step,
        code: 0,
        text: Some(text),
    }
}

/// A failure of `step` that gave no text, php's `NULL` `error_text`.
const fn textless(step: XportStep) -> XportFailure {
    XportFailure {
        step,
        code: 0,
        text: None,
    }
}

/// What a kind's connect or bind made: the socket, and the `errno` a
/// connect left behind it, `EINPROGRESS` over one still under way, which
/// a crypto layer that then fails answers with.
pub(crate) struct Made {
    sock: Sock,
    error_code: i32,
}

impl From<Sock> for Made {
    fn from(sock: Sock) -> Made {
        Made {
            sock,
            error_code: 0,
        }
    }
}

/// `php_tcp_sockop_connect` (`Z/main/streams/xp_socket.c:747`) for
/// `tcp` and `udp`, over `php_network_connect_socket_to_host`
/// (`Z/main/network.c:818`): the `bindto` option read after the host,
/// a value that is no string refusing the connect; then each address the
/// host resolves to tried in turn within what is left of `timeout`: a
/// socket of its family, bound to `bindto` where it is an address of that
/// family (php's `Invalid IP Address` and `Failed to bind` warnings where
/// it is not one, or will not bind), `SO_BROADCAST` on a UDP one and
/// `TCP_NODELAY` on a TCP one as the options say, and connected, or under
/// `asynchronous`, `STREAM_XPORT_OP_CONNECT_ASYNC`, the connect begun and
/// left to finish on a socket that does not block. The last
/// failure is the error, and one with no text where no address was
/// tried, as php leaves `error_string` unwritten then.
fn connect_inet(
    req: &mut Request,
    name: &[u8],
    udp: bool,
    asynchronous: bool,
    timeout: Option<Duration>,
    options: &SocketOptions,
) -> Result<Made, XportFailure> {
    let refused = |text| text_only(XportStep::Connect, text);
    let (host, port) = parse_ip_address(name).map_err(refused)?;
    // A `bindto` that does not parse is php's error text, which the first
    // attempt clears, and no bind.
    let mut bindto = match &options.bindto {
        Some(BindTo::NotAString) => {
            return Err(refused(String::from(
                "local_addr context option is not a string.",
            )));
        }
        Some(BindTo::Address(text)) => parse_ip_address(text).ok(),
        None => None,
    };
    let addrs = addresses(req, &host, port).map_err(refused)?;
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut last = textless(XportStep::Connect);
    for addr in addrs {
        if let Some((local, _)) = &bindto {
            if addr.is_ipv6() && !local.contains(':') {
                continue;
            }
            if addr.is_ipv4() && (local.contains(':') || local == "0") {
                bindto = None;
            }
        }
        let Ok(fd) = crate::api::layer::new_socket(&addr, udp) else {
            continue;
        };
        if let Some((local, local_port)) = &bindto {
            bind_local(req, fd.as_fd(), &addr, local, *local_port);
        }
        if udp && options.so_broadcast {
            crate::api::layer::set_sockopt(fd.as_fd(), SockOpt::SoBroadcast, true).ok();
        }
        if !udp && options.tcp_nodelay {
            crate::api::layer::set_sockopt(fd.as_fd(), SockOpt::TcpNodelay, true).ok();
        }
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        match crate::api::layer::connect_fd(
            fd.as_fd(),
            &addr,
            asynchronous,
            left.map(|t| t.max(Duration::from_micros(1))),
        ) {
            Ok(error_code) => {
                let sock = if udp {
                    Sock::Udp(UdpSocket::from(fd))
                } else {
                    Sock::Tcp(TcpStream::from(fd))
                };
                return Ok(Made { sock, error_code });
            }
            Err(error) => last = failure(XportStep::Connect, &error),
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    Err(last)
}

/// The `bindto` half of `php_network_connect_socket_to_host`
/// (`Z/main/network.c:883`): the local address as `inet_pton` of the
/// socket's family reads it, php's `Invalid IP Address: %s` where it
/// does not, and `Failed to bind to '%s:%d', system said: %s` where the
/// bind fails; the connect goes on either way.
fn bind_local(req: &mut Request, fd: BorrowedFd<'_>, to: &SocketAddr, local: &str, port: u16) {
    let ip = if to.is_ipv4() {
        local
            .parse::<std::net::Ipv4Addr>()
            .ok()
            .map(std::net::IpAddr::V4)
    } else {
        local
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(std::net::IpAddr::V6)
    };
    let Some(ip) = ip else {
        let text = format!(
            "{}(): Invalid IP Address: {local}",
            req.frames().active_function_name()
        );
        warning(req, &text);
        return;
    };
    if let Err(error) = crate::api::layer::bind_fd(fd, &SocketAddr::new(ip, port)) {
        let text = format!(
            "{}(): Failed to bind to '{local}:{port}', system said: {}",
            req.frames().active_function_name(),
            socket_error(&error).1
        );
        warning(req, &text);
    }
}

/// `php_tcp_sockop_bind` (`Z/main/streams/xp_socket.c:678`) over
/// `php_network_bind_socket_to_local_addr` (`Z/main/network.c:487`) for
/// `tcp` and `udp`: the first address the host resolves to that binds,
/// each socket given `SO_REUSEADDR`, then `IPV6_V6ONLY`, `SO_REUSEPORT`
/// and, for UDP, `SO_BROADCAST` as the options say; then, for a TCP
/// server asked to, `listen(2)` with the `backlog` option, `32` where it
/// is absent (`Z/main/streams/transports.c:162`).
fn bind_inet(
    req: &mut Request,
    name: &[u8],
    udp: bool,
    listen: bool,
    options: &SocketOptions,
) -> Result<Sock, XportFailure> {
    let refused = |text| text_only(XportStep::Bind, text);
    let (host, port) = parse_ip_address(name).map_err(refused)?;
    let addrs = addresses(req, &host, port).map_err(refused)?;
    let mut last = textless(XportStep::Bind);
    for addr in addrs {
        let fd = match crate::api::layer::new_socket(&addr, udp) {
            Ok(fd) => fd,
            Err(error) => {
                last = failure(XportStep::Bind, &error);
                continue;
            }
        };
        crate::api::layer::set_sockopt(fd.as_fd(), SockOpt::SoReuseaddr, true).ok();
        if addr.is_ipv6()
            && let Some(only) = options.ipv6_v6only
        {
            crate::api::layer::set_sockopt(fd.as_fd(), SockOpt::Ipv6V6only, only).ok();
        }
        if options.so_reuseport {
            crate::api::layer::set_sockopt(fd.as_fd(), SockOpt::SoReuseport, true).ok();
        }
        if udp && options.so_broadcast {
            crate::api::layer::set_sockopt(fd.as_fd(), SockOpt::SoBroadcast, true).ok();
        }
        if let Err(error) = crate::api::layer::bind_fd(fd.as_fd(), &addr) {
            last = failure(XportStep::Bind, &error);
            continue;
        }
        if udp {
            return Ok(Sock::Udp(UdpSocket::from(fd)));
        }
        if listen {
            let backlog = options.listen_queue.unwrap_or(32);
            let backlog =
                i32::try_from(backlog).unwrap_or(if backlog < 0 { i32::MIN } else { i32::MAX });
            crate::api::layer::listen_fd(fd.as_fd(), backlog)
                .map_err(|error| failure(XportStep::Listen, &error))?;
        }
        return Ok(Sock::TcpServer(TcpListener::from(fd)));
    }
    Err(last)
}

/// `php_stream_generic_socket_ops.label`, the kind of a socket the stream
/// was handed, whose options take no `transport` operation but a name,
/// a send, a receive and a shutdown.
const GENERIC: &str = "generic_socket";

/// The `transports` `PHP_MINIT(standard)` registers
/// (`Z/ext/standard/basic_functions.c:315`), in its order, each with the
/// label of the ops `php_stream_generic_socket_factory` picks for it.
const TRANSPORTS: [(&str, &str); 4] = [
    ("tcp", "tcp_socket"),
    ("udp", "udp_socket"),
    ("unix", "unix_socket"),
    ("udg", "udg_socket"),
];

/// What a `transport`'s name finds in `xport_hash`: one of the engine's
/// four, by the label of its kind, or one an extension registered, which
/// is a `tcp` socket with the extension's crypto layer.
#[derive(Clone, Copy)]
pub(crate) enum Found {
    Engine(&'static str),
    Registered(XportFactory),
}

pub(crate) trait Transports {
    fn transport_named(&self, protocol: &[u8]) -> Option<Found>;
    fn make_sock(&mut self, label: &str, rest: &[u8], xport: Xport) -> Result<Made, XportFailure>;
    fn socket_stream(&mut self, fd: OwnedFd) -> Result<Owner<Stream>, Thrown>;
    fn php_socket_stream(&mut self, fd: OwnedFd) -> Result<Owner<Stream>, Thrown>;
    fn xport_stream(
        &mut self,
        fd: OwnedFd,
        name: Option<&str>,
        blocking: bool,
    ) -> Result<Owner<Stream>, Thrown>;
    fn transports(&self) -> Vec<&'static str>;
    fn xport_create(
        &mut self,
        name: &[u8],
        xport: Xport,
    ) -> Result<Result<Owner<Stream>, XportFailure>, Thrown>;
}

impl Transports for Request {
    /// `zend_hash_str_find_ptr(&xport_hash, protocol)`: the last
    /// registration of the name, an extension's over the engine's.
    fn transport_named(&self, protocol: &[u8]) -> Option<Found> {
        if let Some(transport) = self
            .registered_by::<XportFactory>()
            .map(|(_, t)| t)
            .filter(|t| t.name.as_bytes() == protocol)
            .last()
        {
            return Some(Found::Registered(*transport));
        }
        TRANSPORTS
            .iter()
            .find(|(registered, _)| registered.as_bytes() == protocol)
            .map(|&(_, label)| Found::Engine(label))
    }

    /// The socket of a `transport`'s kind, by its label: a client
    /// connected or a server bound, `php_stream_xport_connect` and
    /// `php_stream_xport_bind` under the kind's ops.
    fn make_sock(&mut self, label: &str, rest: &[u8], xport: Xport) -> Result<Made, XportFailure> {
        match (label, xport) {
            (
                "tcp_socket",
                Xport::Client {
                    timeout,
                    asynchronous,
                    socket,
                    ..
                },
            ) => connect_inet(self, rest, false, asynchronous, timeout, &socket),
            (
                "udp_socket",
                Xport::Client {
                    timeout,
                    asynchronous,
                    socket,
                    ..
                },
            ) => connect_inet(self, rest, true, asynchronous, timeout, &socket),
            ("tcp_socket", Xport::Server { listen, socket, .. }) => {
                bind_inet(self, rest, false, listen, &socket).map(Made::from)
            }
            ("udp_socket", Xport::Server { listen, socket, .. }) => {
                bind_inet(self, rest, true, listen, &socket).map(Made::from)
            }
            ("unix_socket", Xport::Client { .. }) => {
                let path = unix_path(self, rest);
                UnixStream::connect(path)
                    .map(|s| Sock::Unix(s).into())
                    .map_err(|e| failure(XportStep::Connect, &e))
            }
            ("unix_socket", Xport::Server { .. }) => {
                let path = unix_path(self, rest);
                UnixListener::bind(path)
                    .map(|s| Sock::UnixServer(s).into())
                    .map_err(|_| textless(XportStep::Bind))
            }
            ("udg_socket", Xport::Client { .. }) => {
                let path = unix_path(self, rest);
                UnixDatagram::unbound()
                    .and_then(|s| s.connect(path).map(|()| Sock::Udg(s).into()))
                    .map_err(|e| failure(XportStep::Connect, &e))
            }
            (_, _) => {
                let path = unix_path(self, rest);
                UnixDatagram::bind(path)
                    .map(|s| Sock::Udg(s).into())
                    .map_err(|_| textless(XportStep::Bind))
            }
        }
    }

    /// `php_stream_sock_open_from_socket` (`Z/main/network.c:1020`): a
    /// socket the stream is handed, of the `generic_socket` kind, `r+`,
    /// blocking, with `default_socket_timeout` as its read timeout.
    fn socket_stream(&mut self, fd: OwnedFd) -> Result<Owner<Stream>, Thrown> {
        self.xport_stream(fd, None, true)
    }

    /// What `php_stream_url_wrap_php` makes of a `php://` fd that is a
    /// socket (`Z/ext/standard/php_fopen_wrapper.c:361-372`):
    /// `php_stream_sock_open_from_socket`, then `stream->ops =
    /// &php_stream_socket_ops`, so the stream is `tcp_socket` whatever the
    /// socket's family and takes the `transport`'s operations, an accept
    /// among them, which is how a CLI handed a listening socket as its
    /// stdin accepts on it. Served by the binary (`process`), since the
    /// child is `PHP_BINARY`.
    ///
    /// ```php
    /// $path = sys_get_temp_dir() . '/' . 's' . bin2hex(random_bytes(8)) . '.sock';
    /// $server = stream_socket_server("unix://$path");
    /// $child = proc_open([PHP_BINARY, '-n', '-r', '$c = stream_socket_accept(fopen("php://stdin", "r")); echo stream_get_meta_data(STDIN)["stream_type"], " ", get_resource_type($c), "\n"; fwrite($c, "hi");'], [0 => $server, 1 => ['pipe', 'w']], $pipes);
    /// $client = stream_socket_client("unix://$path");
    /// var_dump(stream_get_contents($client), stream_get_contents($pipes[1]), proc_close($child));
    /// unlink($path);
    /// ```
    /// ```expect process
    /// string(2) "hi"
    /// string(18) "tcp_socket stream
    /// "
    /// int(0)
    /// ```
    fn php_socket_stream(&mut self, fd: OwnedFd) -> Result<Owner<Stream>, Thrown> {
        let stream = self.socket_stream(fd)?;
        self.stream_with(*stream, |s| {
            if let Some(socket) = s.ops.as_socket() {
                socket.conn.label = "tcp_socket";
            }
        });
        Ok(stream)
    }

    /// `php_stream_xport_create(name, 0 flags)` handed a socket
    /// afterwards, as `socket_export_stream` makes one
    /// (`Z/ext/sockets/sockets.c:2698`): the kind the registered xport
    /// `name` names, with `name://` as its `uri`, or
    /// `php_stream_sock_open_from_socket`'s `generic_socket` for none;
    /// `r+`, `blocking` as its `is_blocked` with the fd's own `O_NONBLOCK`
    /// left as it is, and `default_socket_timeout` as its read timeout.
    fn xport_stream(
        &mut self,
        fd: OwnedFd,
        name: Option<&str>,
        blocking: bool,
    ) -> Result<Owner<Stream>, Thrown> {
        let found = name.and_then(|name| Some((name, self.transport_named(name.as_bytes())?)));
        let mut crypto = None;
        let (sock, label) = match found {
            Some((_, Found::Registered(transport))) => {
                let layer = (transport.layer)(self, transport.name, b"", None, None)?;
                crypto = Some(layer);
                (Sock::Tcp(TcpStream::from(fd)), transport.label)
            }
            Some((_, Found::Engine("tcp_socket"))) => {
                (Sock::Tcp(TcpStream::from(fd)), "tcp_socket")
            }
            Some((_, Found::Engine("udp_socket"))) => {
                (Sock::Udp(UdpSocket::from(fd)), "udp_socket")
            }
            Some((_, Found::Engine("unix_socket"))) => {
                (Sock::Unix(UnixStream::from(fd)), "unix_socket")
            }
            Some((_, Found::Engine("udg_socket"))) => {
                (Sock::Udg(UnixDatagram::from(fd)), "udg_socket")
            }
            _ => (Sock::Generic(UnixStream::from(fd)), GENERIC),
        };
        let mut kind = Socket::new(Some(sock), label, default_timeout(self));
        kind.conn.is_blocked = blocking;
        kind.conn.crypto = crypto.into();
        let mut stream = PhpStream::new(Box::new(kind), b"r+");
        stream.flags.no_seek = true;
        let stream = self.stream_alloc(stream)?;
        let Some((name, _)) = found else {
            return Ok(stream);
        };
        super::orig_path_set(self, stream, format!("{name}://").as_bytes())
    }

    fn transports(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = TRANSPORTS.iter().map(|(name, _)| *name).collect();
        for (_, transport) in self.registered_by::<XportFactory>() {
            if !names.contains(&transport.name) {
                names.push(transport.name);
            }
        }
        names
    }

    fn xport_create(
        &mut self,
        name: &[u8],
        xport: Xport,
    ) -> Result<Result<Owner<Stream>, XportFailure>, Thrown> {
        let n = name
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            .count();
        let (protocol, rest) =
            if n > 1 && name.get(n) == Some(&b':') && name[n + 1..].starts_with(b"//") {
                (&name[..n], &name[n + 3..])
            } else {
                (&b"tcp"[..], name)
            };
        let persistent = match &xport {
            Xport::Client { persistent, .. } => persistent.clone(),
            Xport::Server { .. } => None,
        };
        // `php_stream_from_persistent_id` first, a live stream under the
        // key answered as it stands.
        if let Some(key) = &persistent
            && let Some(stream) = self.persistent_stream(key)?
        {
            return Ok(Ok(stream));
        }
        let Some(found) = self.transport_named(protocol) else {
            let shown = String::from_utf8_lossy(&protocol[..protocol.len().min(31)]);
            return Ok(Err(text_only(
                XportStep::Unregistered,
                format!(
                    "Unable to find the socket transport \"{shown}\" - did you forget to enable it when you configured PHP?"
                ),
            )));
        };
        let xport = context_options(self, xport)?;
        let (Xport::Client { context, .. } | Xport::Server { context, .. }) = xport;
        let role = match &xport {
            Xport::Client { .. } => Role::Peer,
            Xport::Server { socket, .. } => Role::Server {
                accept_nodelay: socket.tcp_nodelay,
            },
        };
        let (label, crypto) = match found {
            Found::Engine(label) => (label, None),
            Found::Registered(transport) => {
                let timeout = match &xport {
                    Xport::Client { timeout, .. } => *timeout,
                    Xport::Server { .. } => default_timeout(self),
                };
                let protocol = String::from_utf8_lossy(protocol);
                let layer = (transport.layer)(self, &protocol, rest, timeout, context)?;
                ("tcp_socket", Some((transport.label, layer)))
            }
        };
        // `php_stream_generic_socket_factory` makes the stream with
        // `sock->socket = -1`, and only a client asked to connect, or a
        // server, is handed a socket.
        let (sock, error_code) = match xport {
            Xport::Client { connect: false, .. } => (None, 0),
            xport => match self.make_sock(label, rest, xport) {
                Ok(Made { sock, error_code }) => (Some(sock), error_code),
                Err(failed) => return Ok(Err(failed)),
            },
        };
        let connected = sock.is_some();
        let (label, crypto) = match crypto {
            Some((label, layer)) => (label, Some(layer)),
            None => (label, None),
        };
        let mut kind = Socket::new(sock, label, default_timeout(self));
        kind.conn.role = role;
        kind.conn.crypto = crypto.into();
        let mut stream = PhpStream::new(Box::new(kind), b"r+");
        stream.flags.no_seek = true;
        // `pestrdup(name, persistent_id ? 1 : 0)`.
        if persistent.is_some() {
            stream.orig_path = Some(super::OrigPath::Persistent(name.into()));
        }
        let stream = self.stream_alloc_as(stream, persistent.is_some())?;
        let stream = match persistent {
            Some(_) => stream,
            None => super::orig_path_set(self, stream, name)?,
        };
        stream.set_ctx(self, context);
        if role == Role::Peer && connected {
            let connected = self
                .stream_op(*stream, StreamOp::crypto_connected)
                .unwrap_or(Ok(()));
            if connected.is_err() {
                stream.free(self);
                return Ok(Err(XportFailure {
                    code: error_code,
                    ..textless(XportStep::Connect)
                }));
            }
        }
        if let Some(key) = persistent {
            self.hold_persistent(key, *stream);
        }
        Ok(Ok(stream))
    }
}

/// The `socket` options `xp_socket.c` reads off `PHP_STREAM_CONTEXT(stream)`
/// as it connects or binds, laid under what the caller of
/// [`Request::xport_create`] sets on the socket itself.
fn context_options(req: &mut Request, mut xport: Xport) -> Result<Xport, Thrown> {
    let (Xport::Client {
        context, socket, ..
    }
    | Xport::Server {
        context, socket, ..
    }) = &mut xport;
    if let Some(context) = *context {
        let read = context.socket_options(req)?;
        *socket = std::mem::take(socket).under(read);
    }
    Ok(xport)
}

/// `php_network_parse_network_address_with_port` (`Z/main/network.c:560`):
/// `ip:port` or `[v6]:port` of a written-out address, no lookup.
fn parse_network_address(text: &[u8]) -> Option<SocketAddr> {
    let text = std::str::from_utf8(text).ok()?;
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr);
    }
    let (host, port) = text.rsplit_once(':')?;
    let port = atoi_port(port.as_bytes());
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some(SocketAddr::new(host.parse().ok()?, port))
}

/// What an accept came to: the client and, where it was asked for, its
/// address, or the error text, `None` for a stream that is no socket.
type Accepted = Result<(Owner<Stream>, Option<Owner<PhpString>>), Option<String>>;

/// What [`Stream::recvfrom`] answers: the bytes read, and the sender's
/// address where it was asked for, each a string of the request's.
type Received = (Owner<PhpString>, Option<Owner<PhpString>>);

impl Stream {
    /// `php_stream_xport_accept` (`Z/main/streams/transports.c:283`): the
    /// next client of a listening socket within `timeout`, `None` to wait
    /// for ever, as a stream of the listener's kind, `r+`, with the
    /// client's address where `want_peer` asks for it; the error text of
    /// an accept that failed or ran out of time, `None` for a stream
    /// that is no socket.
    pub fn accept(
        self,
        req: &mut Request,
        timeout: Option<Duration>,
        want_peer: bool,
    ) -> Result<Accepted, Thrown> {
        let answered = req
            .stream_op(self, |op| op.xport(XportOp::Accept { timeout, want_peer }))
            .flatten();
        let Some(Xported::Accepted(accepted)) = answered else {
            return Ok(Err(None));
        };
        let (client, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(text) => return Ok(Err(Some(text))),
        };
        let mut stream = PhpStream::new(Box::new(client), b"r+");
        stream.flags.no_seek = true;
        let client = req.stream_alloc(stream)?;
        // `xparam->outputs.client->ctx = stream->ctx`, one owner more, and
        // the client's crypto enabled where the listener's transport says.
        let context = self.ctx(req);
        client.set_ctx(req, context);
        let connected = req
            .stream_op(*client, StreamOp::crypto_connected)
            .unwrap_or(Ok(()));
        if let Err(text) = connected {
            client.free(req);
            return Ok(Err(Some(text)));
        }
        let peer = peer.map(|peer| PhpString::new(req, &peer)).transpose();
        Ok(Ok(super::freed_on_throw(req, client, peer)?))
    }

    /// `php_stream_xport_get_name` (`Z/main/streams/transports.c:327`):
    /// the socket's own address or, with `peer`, its peer's, as
    /// `php_network_populate_name_from_sockaddr` writes it; `None` where
    /// the stream is no socket, the call failed or the name is empty.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn socket_name(
        self,
        req: &mut Request,
        peer: bool,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let answered = req
            .stream_op(self, |op| op.xport(XportOp::Name { peer }))
            .flatten();
        match answered {
            Some(Xported::Name(Some(name))) if name.first().is_some_and(|b| *b != 0) => {
                Ok(Some(PhpString::new(req, &name)?))
            }
            _ => Ok(None),
        }
    }

    /// `php_stream_xport_sendto` (`Z/main/streams/transports.c:462`)
    /// after `stream_socket_sendto` parsed the address: `to` is `ip:port`
    /// or `[v6]:port`, and `None` back is php's warning that it is
    /// neither. Then the socket's `send`, to `to` where it is given,
    /// whatever the bits, since php's shortcut to `php_stream_write` is
    /// compiled out; `STREAM_OOB` is taken and not honoured. The count,
    /// or php's `-1`, which a stream that is no socket answers too.
    pub fn sendto(
        self,
        req: &mut Request,
        data: &[u8],
        flags: i64,
        to: Option<&[u8]>,
    ) -> Option<i64> {
        let addr = match to {
            Some(text) => {
                let Some(addr) = parse_network_address(text) else {
                    let shown = format!(
                        "{}(): Failed to parse `{}' into a valid network address",
                        req.frames().active_function_name(),
                        String::from_utf8_lossy(text)
                    );
                    warning(req, &shown);
                    return None;
                };
                Some(addr)
            }
            None => None,
        };
        // `MSG_OOB` has no spelling in `std`'s sockets; the bit is read
        // and goes no further.
        let _ = flags & STREAM_OOB;
        let answered = req
            .stream_op(self, |op| {
                op.xport(XportOp::Send {
                    buf: data,
                    to: addr,
                })
            })
            .flatten();
        Some(match answered {
            Some(Xported::Sent(Some(n))) => i64::try_from(n).unwrap_or(i64::MAX),
            _ => -1,
        })
    }

    /// `php_stream_xport_recvfrom` (`Z/main/streams/transports.c:398`):
    /// the socket's `recv` straight, past the read buffer, whatever the
    /// bits, since php's read through the buffer is compiled out; under
    /// `STREAM_PEEK` the bytes are left queued, and the sender's address
    /// comes back where `want_addr` asks for it, empty where the socket
    /// gives none. `None` is php's `-1`, which a stream that is no socket
    /// answers too.
    pub fn recvfrom(
        self,
        req: &mut Request,
        len: usize,
        flags: i64,
        want_addr: bool,
    ) -> Result<Option<Received>, Thrown> {
        let peek = flags & STREAM_PEEK != 0;
        let mut buf = vec![0u8; len];
        let answered = req
            .stream_op(self, |op| {
                op.xport(XportOp::Recv {
                    buf: &mut buf,
                    peek,
                    want_addr,
                })
            })
            .flatten();
        let Some(Xported::Received(Some((n, from)))) = answered else {
            return Ok(None);
        };
        let bytes = PhpString::new(req, &buf[..n])?;
        let from = match from {
            Some(from) => Some(PhpString::new(req, &from)?),
            None => None,
        };
        Ok(Some((bytes, from)))
    }

    /// `php_stream_xport_shutdown` (`Z/main/streams/transports.c:476`):
    /// `shutdown(2)` of one direction or both; whether it answered `0`.
    pub fn shutdown(self, req: &mut Request, how: Shutdown) -> bool {
        matches!(
            req.stream_op(self, |op| op.xport(XportOp::Shutdown(how)))
                .flatten(),
            Some(Xported::Shut(true))
        )
    }
}

impl Stream {
    /// `php_stream_xport_crypto_setup` (`Z/main/streams/transports.c:357`):
    /// `STREAM_XPORT_CRYPTO_OP_SETUP` of the stream's crypto layer with
    /// php's `method` bits and the `session_stream`; `true` for php's `0`.
    /// A stream with no layer is php's warning `This stream does not
    /// support SSL/crypto` and `false`.
    ///
    /// ```php
    /// [$a, $b] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
    /// var_dump(stream_socket_enable_crypto($a, true, STREAM_CRYPTO_METHOD_TLS_CLIENT));
    /// ```
    /// ```expect
    /// Warning: stream_socket_enable_crypto(): This stream does not support SSL/crypto in %s on line 3
    /// bool(false)
    /// ```
    pub fn crypto_setup(self, req: &mut Request, method: i64, session: Option<Stream>) -> bool {
        let answered = req
            .stream_op(self, |op| op.crypto_setup(method, session))
            .flatten();
        answered.unwrap_or_else(|| {
            no_crypto(req);
            false
        })
    }

    /// `php_stream_xport_crypto_enable` (`Z/main/streams/transports.c:378`):
    /// `STREAM_XPORT_CRYPTO_OP_ENABLE` of the stream's crypto layer, the
    /// handshake where `activate` and the layer shut where not; a stream
    /// with no layer is php's warning and [`Handshake::Unsupported`].
    pub fn crypto_enable(self, req: &mut Request, activate: bool) -> Handshake {
        let answered = req
            .stream_op(self, |op| op.crypto_enable(activate))
            .flatten();
        answered.unwrap_or_else(|| {
            no_crypto(req);
            Handshake::Unsupported
        })
    }

    /// The stream's crypto layer handed to `f`, as `xp_ssl.c` reads a
    /// `session_stream`'s `abstract` once its `ops` are
    /// `php_openssl_socket_ops`: `None` for a stream with no layer, one
    /// closed, and one inside a call already.
    pub fn crypto<R>(self, req: &mut Request, f: impl FnOnce(&mut dyn Crypto) -> R) -> Option<R> {
        req.stream_with(self, |s| {
            s.ops.as_socket().and_then(Socket::crypto_layer).map(f)
        })
        .flatten()
    }
}

/// `php_error_docref("streams.crypto", E_WARNING, ...)` for a stream whose
/// kind answers `PHP_STREAM_OPTION_CRYPTO_API` with `NOTIMPL`.
fn no_crypto(req: &mut Request) {
    let text = format!(
        "{}(): This stream does not support SSL/crypto",
        req.frames().active_function_name()
    );
    warning(req, &text);
}
