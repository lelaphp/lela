//! The socket `transport`s an extension registers and the crypto layer
//! their sockets carry: php's `php_stream_xport_register`
//! (`Z/main/streams/transports.c:29`) and `PHP_STREAM_OPTION_CRYPTO_API`
//! (`Z/main/php_network.h`, `php_stream_xport_crypto_setup` and
//! `php_stream_xport_crypto_enable`), which `ext/openssl`'s `MINIT`
//! answers with `xp_ssl.c` for `ssl`, `tls`, `tlsv1.0` to `tlsv1.3` and,
//! over `standard`'s own, `tcp`.
//!
//! A registered `transport` is the stream layer's `tcp` socket kind with
//! a [`Crypto`] layer beside it: the layer connects, binds, listens,
//! accepts, waits, names and shuts the socket down as `xp_socket.c`
//! does, and hands a read or a write to the layer once the layer is
//! active, as `php_openssl_sockop_io` hands one to
//! `php_stream_socket_ops` while it is not. One stream kind, so
//! `stream_socket_enable_crypto`, the `https` and `ftps` wrappers and the
//! `transports` all reach the same socket.

use std::any::Any;
use std::os::fd::BorrowedFd;
use std::time::Duration;

pub use crate::api::layer::Readiness;

use crate::api::raising::Thrown;
use crate::api::request::{Request, Stream, StreamCtx};
use crate::api::values::{Owner, PhpArray};
use php_engine::api::abi::EngineState;

/// One socket `transport` an extension hands the engine with
/// [`ExtensionBuilder::register`], php's `php_stream_xport_register(name,
/// factory)`: `name` is what `stream_get_transports()` lists, and a name
/// the engine registers itself (`tcp`) is taken over in its place, as
/// openssl's `MINIT` takes `tcp` over; `label` is the kind's `stream_type`;
/// `layer` makes the crypto layer each socket carries, php's
/// `php_stream_transport_factory` less the socket, which the engine makes:
/// handed the `transport`'s name as the program wrote it, the rest of the
/// address (php's `resourcename`), the connect timeout (`None` to wait for
/// ever) and the stream's `context`, its `Err` what it threw. The socket is the
/// engine's `tcp` kind, since every factory php registers this way
/// (`php_openssl_ssl_socket_factory`) copies `php_stream_socket_ops`.
///
/// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
#[derive(Debug, Clone, Copy, EngineState)]
#[allow(clippy::type_complexity)] // php's factory, one function pointer
pub struct XportFactory {
    pub name: &'static str,
    pub label: &'static str,
    pub layer: fn(
        &mut Request,
        &str,
        &[u8],
        Option<Duration>,
        Option<StreamCtx>,
    ) -> Result<Box<dyn Crypto>, Thrown>,
}

/// How a crypto layer's handshake went, what
/// `php_stream_xport_crypto_enable` answers: `1`, done; `0`, a socket that
/// does not block has more to wait for; `-1`, failed; and, from the
/// stream alone, a stream with no crypto layer, which php warns
/// `This stream does not support SSL/crypto` for and answers
/// `PHP_STREAM_OPTION_RETURN_NOTIMPL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handshake {
    Done,
    Pending,
    Failed,
    Unsupported,
}

/// The socket a crypto layer runs over, lent for one call: the fields of
/// `php_netstream_data_t` that `xp_ssl.c` reads and writes, which the
/// engine takes back after the call. `blocked` is `is_blocked`, as
/// [`CryptoSocket::set_blocking`] leaves it, which a handshake's
/// `set_blocking` writes through to the stream at once, so PHP code a
/// warning of the handshake serves reads the socket as php's does; `timeout` is the read
/// timeout, `None` for php's `-1`; `timed_out` is `timeout_event`, which
/// a read that ran out of time sets; `supplemental` is the stream's
/// `has_buffered_data`, a read after the buffer already gave bytes, which
/// must not wait; `context` is `PHP_STREAM_CONTEXT(stream)`; `stream` is
/// the stream itself, which a callback of the layer's is handed.
#[allow(clippy::struct_excessive_bools)] // php's fields, one flag per name
pub struct CryptoSocket<'a> {
    pub fd: BorrowedFd<'a>,
    pub blocked: bool,
    pub timeout: Option<Duration>,
    pub timed_out: bool,
    pub supplemental: bool,
    pub ctx: Option<StreamCtx>,
    pub stream: Option<Stream>,
    /// The stream the socket stays in while the layer's call runs, which
    /// [`CryptoSocket::set_blocking`] writes `is_blocked` through to;
    /// `None` for a call the socket's own read or write makes.
    pub(crate) live: Option<Stream>,
}

impl CryptoSocket<'_> {
    /// `php_set_sock_blocking`: `O_NONBLOCK` cleared for `true` and set
    /// for `false`, and `blocked` with it where the call took; whether it
    /// took.
    pub fn set_blocking(&mut self, req: &mut Request, blocking: bool) -> bool {
        if crate::api::layer::set_fd_blocking(self.fd, blocking).is_err() {
            return false;
        }
        self.blocked = blocking;
        if let Some(stream) = self.live {
            crate::streams::sockets::show_blocking(req, stream, blocking);
        }
        true
    }

    /// `php_pollfd_for(socket, events, timeout) > 0`: whether the socket
    /// became ready within `timeout`, `None` to wait for ever; an
    /// interrupted wait is asked again, as php's callers loop on it.
    #[must_use]
    pub fn wait(&self, want: Readiness, timeout: Option<Duration>) -> bool {
        loop {
            match crate::api::layer::poll_fd(self.fd, want, timeout) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(ready) => return ready,
                Err(_) => return false,
            }
        }
    }

    /// `php_stream_xport_shutdown(stream, SHUT_RDWR)`: both directions of
    /// the socket shut, which a layer does to a peer that renegotiates
    /// past its limit; whether `shutdown(2)` answered `0`.
    pub fn shutdown(&self) -> bool {
        // A second fd for the same socket: `shutdown(2)` acts on the
        // socket, and closing the second fd leaves the first open.
        self.fd
            .try_clone_to_owned()
            .map(std::net::TcpStream::from)
            .is_ok_and(|socket| socket.shutdown(std::net::Shutdown::Both).is_ok())
    }
}

/// A socket's crypto layer, `xp_ssl.c`'s half of
/// `php_openssl_netstream_data_t` behind `PHP_STREAM_OPTION_CRYPTO_API`:
/// what `php_openssl_socket_ops` does beyond `php_stream_socket_ops`. The
/// engine asks it to set up and to enable, hands it every read and write
/// once it is [`Crypto::active`], asks it whether the peer is still there,
/// what `stream_get_meta_data` shows under `crypto`, and how much it holds
/// decrypted for `stream_select`; dropping it is its `SSL_shutdown` and
/// free, before the socket closes. What a call throws, php's
/// `EG(exception)` left standing while the call goes on to fail, the
/// engine keeps for the native to throw once it returns.
///
/// A layer is engine state ([`EngineState`]): a persistent stream's
/// socket keeps its layer as it outlives the request, in the persistent
/// list or in the extension that kept it ([`OwnedStream::keep`]), so the
/// layer holds nothing of a request's, which the bound says by type.
///
/// [`OwnedStream::keep`]: crate::api::request::OwnedStream::keep
pub trait Crypto: Any + EngineState {
    /// `STREAM_XPORT_CRYPTO_OP_SETUP` with php's crypto `method` bits and
    /// the `session_stream` to take a session from: `true` for php's `0`.
    fn setup(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        method: i64,
        session: Option<Stream>,
    ) -> Result<bool, Thrown>;

    /// `STREAM_XPORT_CRYPTO_OP_ENABLE`: the handshake made where
    /// `activate`, the layer shut where not; never
    /// [`Handshake::Unsupported`].
    fn enable(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        activate: bool,
    ) -> Result<Handshake, Thrown>;

    /// `ssl_active`: whether reads and writes go through the layer.
    fn active(&self) -> bool;

    /// `php_openssl_sockop_io` reading, asked only while active: how many
    /// bytes were given, `eof` set where the peer is done, `None` for
    /// php's `-1`.
    fn read(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        buf: &mut [u8],
        eof: &mut bool,
    ) -> Result<Option<usize>, Thrown>;

    /// `php_openssl_sockop_io` writing, asked only while active.
    fn write(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        buf: &[u8],
    ) -> Option<usize>;

    /// Whether the session the layer holds is one the peer issued and so
    /// can be resumed: exactly when php's FTP client would have stored it
    /// as `ftp->last_ssl_session` (`ftp_ssl_new_session_cb`,
    /// `Z/ext/ftp/ftp.c:219-228`, which OpenSSL calls for a session it
    /// may cache), `SSL_SESSION_is_resumable(SSL_get_session(ssl))` over
    /// the `SSL` `xp_ssl.c` keeps (`Z/ext/openssl/xp_ssl.c`'s
    /// `ssl_handle`). An FTPS data connection is refused where the control
    /// connection's is not (`data_accept`, `Z/ext/ftp/ftp.c:1776-1784`).
    fn resumable(&self) -> bool;

    /// `SSL_pending`: the decrypted bytes the layer holds, which the
    /// `PHP_STREAM_AS_FD_FOR_SELECT` cast moves into the read buffer so
    /// `stream_select` sees them.
    fn pending(&self) -> usize;

    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` while active, asked once the
    /// socket polled readable: whether the peer is still there.
    fn alive(&mut self, req: &mut Request, socket: &mut CryptoSocket<'_>) -> bool;

    /// What `PHP_STREAM_OPTION_META_DATA_API` adds under `crypto`, ahead
    /// of the socket's `timed_out`, `blocked` and `eof`, its one owner the
    /// caller's; `None` while the layer is not active.
    fn meta(&self, req: &mut Request) -> Result<Option<Owner<PhpArray>>, Thrown>;

    /// `STREAM_XPORT_OP_CONNECT`'s tail, and `php_openssl_tcp_sockop_accept`'s
    /// for a client: asked once the socket connected or was accepted,
    /// the crypto set up and enabled where the `transport` says so
    /// (`enable_on_connect`), with the layer's own warning where that
    /// failed. The inner `Err` is the text an accept reports, and fails
    /// the connect or the accept.
    fn connected(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
    ) -> Result<Result<(), String>, Thrown>;

    /// The layer of a client a server accepted, as
    /// `php_openssl_tcp_sockop_accept` makes it of what the listener's
    /// holds; the engine asks it [`Crypto::connected`] next.
    fn accepted(&self) -> Box<dyn Crypto>;

    /// What the `transport`'s factory `emalloc`s for the socket in
    /// place of `php_netstream_data_t`, each allocation's size in php's
    /// order, the struct first (`sizeof(php_openssl_netstream_data_t)`,
    /// then its `url_name`), which the engine lays down on the request's
    /// heap with the stream and gives back at its close, so
    /// `memory_get_usage()` counts what php's does.
    fn allocations(&self) -> Vec<usize>;
}
