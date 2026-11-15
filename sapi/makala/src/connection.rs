//! The connection side: hyper's HTTP/1.1 server on a tokio
//! runtime of one thread, which owns every socket, reads each request's
//! framing as RFC 9112 has it and never runs PHP, as the BEAM keeps
//! anything that blocks off the schedulers that own sockets
//! (research/production-server.r2.md §4.10, M2 and M4). Each request is
//! routed on tokio's blocking pool, which may wait on the disk where this
//! thread may not, and a static file is sent from here
//! ([`crate::static_file`]), so no request thread waits on a
//! client taking one. A program's request goes to the request side
//! whole, on the bounded queue: while the queue is full its connection
//! waits for room. That and the connection cap are the
//! backpressure, Ranch's (research/production-server.r2.md M7):
//! at the cap the accept loop stops accepting, so new clients wait in
//! the kernel's listen queue and none is refused. The body follows on a
//! bounded channel as hyper reads it, and the response comes back on
//! two: the head once, then the body piece by piece, which hyper frames
//! chunked on HTTP/1.1 and
//! close-delimited on HTTP/1.0 where PHP set no length. A `HEAD`
//! response has none: PHP writes no body for one (php's `headers_only`),
//! and hyper sends none.
//!
//! Every limit of [`Limits`] is applied on this side: the head's
//! request line, field bytes and declared body length where hyper has
//! parsed it ([`over_limit`]), the field count by hyper's parser, the
//! keep-alive and header clocks and the response's minimum rate by the
//! connection's socket ([`Socket`]), a chunked body's length and the
//! body's minimum rate as the body is read ([`pump`]), and the cap on
//! requests per connection as each request begins ([`respond`]); the
//! cap on connections is the accept loop's. hyper
//! answers `100 Continue` when the body is first read, which is after
//! the head has passed its limits.
//!
//! A connection to the TLS listener is served once its handshake has
//! ended ([`open_connection`]), within
//! [`Limits::handshake_timeout`].
//!
//! SIGTERM ends the accept loop and begins the drain
//! ([`serve_until_drained`]),
//! bounded by [`Limits::shutdown_timeout`].

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::body::{Body as _, Bytes, Frame, Incoming, SizeHint};
use hyper::ext::ReasonPhrase;
use hyper::header::{CONNECTION, CONTENT_LENGTH, HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::TokioIo;
use php_proto::cgi::Line;
use php_sapi::{Ending, ResponseHead};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio_rustls::TlsAcceptor;

use crate::boundary::BodyLength;
use crate::framing;
use crate::log::Access;
use crate::options::Limits;
use crate::request::{Head, Job, Scheme, Sink, Written};
use crate::route::{Route, Router};
use crate::socket::{Socket, Watch, Wire};
use crate::static_file::Sending;

/// How many pieces of a request's body wait for the request to read them.
const BODY_PIECES: usize = 8;

/// How many pieces of a response wait for the client to take them.
const RESPONSE_PIECES: usize = 8;

/// The least hyper's read buffer may hold, which its builder asserts.
const LEAST_BUFFER: usize = 8192;

/// A listening socket, and the TLS its connections speak where they do.
pub(crate) struct Listener {
    /// The bound socket.
    pub(crate) socket: std::net::TcpListener,
    /// The handshakes its connections make, where they speak TLS.
    pub(crate) tls: Option<TlsAcceptor>,
}

impl Listener {
    /// What the started line names it by: its scheme and address.
    pub(crate) fn url(&self) -> std::io::Result<String> {
        Ok(format!(
            "{}://{}",
            scheme(self.tls.as_ref()).name(),
            self.socket.local_addr()?
        ))
    }
}

/// Whether a listener's connections come clear or under `tls`.
fn scheme(tls: Option<&TlsAcceptor>) -> Scheme {
    if tls.is_some() {
        Scheme::Https
    } else {
        Scheme::Http
    }
}

/// The connection side: its runtime, and SIGTERM, which begins the drain.
pub(crate) struct ConnectionSide {
    runtime: tokio::runtime::Runtime,
    terminate: Signal,
}

/// How the drain ended.
pub(crate) struct Drained {
    /// When the shutdown timeout ends, counted from the SIGTERM: what the
    /// request threads still serve then is left to the process's end.
    pub(crate) deadline: std::time::Instant,
    /// Whether every connection ended before the deadline; where one did
    /// not, it was aborted, its socket closed.
    pub(crate) whole: bool,
}

impl ConnectionSide {
    /// The runtime, of one thread, and SIGTERM taken from its default,
    /// which ends the process at once, before the server says it listens:
    /// a SIGTERM that comes after that line drains.
    ///
    /// # Errors
    ///
    /// What the runtime or the signal could not start with.
    pub(crate) fn new() -> std::io::Result<ConnectionSide> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        let terminate = {
            let _inside = runtime.enter();
            signal(SignalKind::terminate())?
        };
        Ok(ConnectionSide { runtime, terminate })
    }

    /// The connection side on this thread for as long as the process
    /// serves: every connection accepted on `listeners` served on its own
    /// task, its requests put on `queue`, until SIGTERM, and then the
    /// drain ([`serve_until_drained`]). What is left at the deadline is aborted here,
    /// as the runtime is dropped with every connection's task, which
    /// closes its socket and the channels its request writes to.
    ///
    /// # Errors
    ///
    /// What the listener could not start with, or the connection slots
    /// closed, which nothing does.
    pub(crate) fn run(
        self,
        limits: Limits,
        router: Arc<Router>,
        listeners: Vec<Listener>,
        queue: mpsc::Sender<Job>,
    ) -> std::io::Result<Drained> {
        let ConnectionSide {
            runtime,
            mut terminate,
        } = self;
        runtime.block_on(serve_until_drained(
            limits,
            router,
            listeners,
            queue,
            &mut terminate,
        ))
    }
}

/// The accept loops until SIGTERM, one a listener, then the drain,
/// Ranch's and Kestrel's (research/production-server.r2.md M8, M10), in
/// the order a supervisor stops what it started, the last started first:
/// the listeners are closed, so no connection is accepted after the signal and a client
/// that tries is refused by the kernel; every connection is told to end
/// ([`drive`]), an idle keep-alive one at once and a busy one once its
/// response has ended; and the connections are waited for until the
/// shutdown timeout ([`Limits::shutdown_timeout`]), or a second SIGTERM.
/// Each connection's task holds a sender of `open` while it runs, so the
/// channel closed is the connections all ended; the drain is whole then,
/// and the sockets still lingering after their last responses are given
/// until the deadline to close. The request threads are the last: with
/// every connection gone the queue has no sender, and each ends once its
/// request has ([`crate::request::Pool::join_by`]).
async fn serve_until_drained(
    limits: Limits,
    router: Arc<Router>,
    listeners: Vec<Listener>,
    queue: mpsc::Sender<Job>,
    terminate: &mut Signal,
) -> std::io::Result<Drained> {
    let listeners = listeners
        .into_iter()
        .map(|Listener { socket, tls }| {
            socket.set_nonblocking(true)?;
            Ok((tokio::net::TcpListener::from_std(socket)?, tls))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let slots = Arc::new(Semaphore::new(
        usize::try_from(limits.connections)
            .map_or(Semaphore::MAX_PERMITS, |n| n.min(Semaphore::MAX_PERMITS)),
    ));
    let (drain, draining) = watch::channel(false);
    let (open, mut all_ended) = mpsc::channel::<Infallible>(1);
    let mut loops: Vec<Pin<Box<dyn Future<Output = std::io::Error> + '_>>> = listeners
        .iter()
        .map(|(listener, tls)| {
            let accepting = Accepting {
                limits,
                router: &router,
                slots: &slots,
                queue: queue.clone(),
                draining: draining.clone(),
                open: open.clone(),
                tls: tls.clone(),
            };
            Box::pin(accept(listener, accepting)) as Pin<Box<dyn Future<Output = _>>>
        })
        .collect();
    drop((queue, draining, open));
    // The first loop to fail, which none does but with the slots closed.
    let accepting = std::future::poll_fn(|cx| {
        loops
            .iter_mut()
            .find_map(|accepting| match accepting.as_mut().poll(cx) {
                Poll::Ready(failed) => Some(failed),
                Poll::Pending => None,
            })
            .map_or(Poll::Pending, Poll::Ready)
    });
    if let Some(failed) = first_of(accepting, terminate.recv()).await {
        return Err(failed);
    }
    let deadline = tokio::time::Instant::now() + limits.shutdown_timeout;
    drop(loops);
    drop(listeners);
    crate::log::error(
        format!(
            "makala: SIGTERM: the listeners are closed, and the connections drain for at most {:?}",
            limits.shutdown_timeout
        )
        .as_bytes(),
    );
    drain.send_replace(true);
    // A second SIGTERM ends the wait at once, as the deadline would.
    let ended = tokio::time::timeout_at(deadline, all_ended.recv());
    let whole = matches!(first_of(ended, terminate.recv()).await, Some(Ok(None)));
    if whole {
        // Each socket still lingers for its last response, until the
        // deadline at most; none is cut short where the drain was whole.
        tokio::time::timeout_at(deadline, slots.acquire_many(limits.connections))
            .await
            .ok();
    }
    Ok(Drained {
        deadline: deadline.into_std(),
        whole,
    })
}

/// What an accept loop hands each connection it accepts.
struct Accepting<'a> {
    limits: Limits,
    router: &'a Arc<Router>,
    slots: &'a Arc<Semaphore>,
    queue: mpsc::Sender<Job>,
    draining: watch::Receiver<bool>,
    open: mpsc::Sender<Infallible>,
    /// The TLS the listener's connections speak, where they do.
    tls: Option<TlsAcceptor>,
}

/// One listener's accept loop: each connection on a task of its own, its
/// requests served in order, as HTTP/1.1 keeps them. A connection holds one of
/// [`Limits::connections`] slots until its socket has lingered and
/// closed; with none free the loop waits for one before it accepts
/// again, as Ranch's acceptor sleeps at its cap, and the kernel's listen
/// queue holds the clients that come meanwhile. It returns only with the
/// slots closed, which nothing does; the drain drops it unfinished.
async fn accept(listener: &tokio::net::TcpListener, accepting: Accepting<'_>) -> std::io::Error {
    let Accepting {
        limits,
        router,
        slots,
        queue,
        draining,
        open,
        tls,
    } = accepting;
    let local = match listener.local_addr() {
        Ok(local) => local,
        Err(e) => return e,
    };
    loop {
        let Ok(slot) = Arc::clone(slots).acquire_owned().await else {
            return std::io::Error::other("the connection slots were closed");
        };
        // A connection that failed before it was accepted, or a process
        // out of descriptors: the next is tried after a pause, so a full
        // table does not spin the loop.
        let Ok((stream, peer)) = accepted(listener).await else {
            drop(slot);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            continue;
        };
        let connection = Arc::new(Connection {
            peer,
            local,
            scheme: scheme(tls.as_ref()),
            limits,
            router: Arc::clone(router),
            queue: queue.clone(),
            watch: Arc::new(Watch::new()),
            begun: AtomicU32::new(0),
            cut: Notify::new(),
        });
        tokio::spawn(open_connection(
            connection,
            stream,
            slot,
            tls.clone(),
            draining.clone(),
            open.clone(),
        ));
    }
}

/// The next connection `listener` accepts, its socket set as a served
/// connection's is: no Nagle. hyper flushes a response's head alone when
/// the body is not ready yet, and on a kept-alive connection the kernel
/// would then hold the body until the client's delayed ACK of the head,
/// 40 ms on Linux; nginx sets the same for kept-alive connections. A
/// socket that refuses the option still serves, only slower.
async fn accepted(
    listener: &tokio::net::TcpListener,
) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
    let (stream, peer) = listener.accept().await?;
    stream.set_nodelay(true).ok();
    Ok((stream, peer))
}

/// A connection accepted, its TLS handshake made where its listener
/// speaks TLS, then served ([`drive`]); one whose handshake fails, or does
/// not end within the handshake timeout or before the drain begins, is
/// closed, the failure in the error log.
async fn open_connection(
    connection: Arc<Connection>,
    stream: tokio::net::TcpStream,
    slot: OwnedSemaphorePermit,
    tls: Option<TlsAcceptor>,
    mut draining: watch::Receiver<bool>,
    open: mpsc::Sender<Infallible>,
) {
    let wire = match tls {
        None => Wire::Clear(stream),
        Some(tls) => {
            let shaken =
                tokio::time::timeout(connection.limits.handshake_timeout, tls.accept(stream));
            match first_of(shaken, draining.wait_for(|draining| *draining)).await {
                Some(Ok(Ok(stream))) => Wire::Tls(Box::new(stream)),
                Some(Ok(Err(failed))) => {
                    handshake_failed(connection.peer, &failed);
                    return;
                }
                Some(Err(_)) => {
                    handshake_failed(connection.peer, &"it passed the handshake timeout");
                    return;
                }
                None => return,
            }
        }
    };
    let socket = Socket::new(
        wire,
        connection.peer,
        Arc::clone(&connection.watch),
        connection.limits,
        slot,
    );
    drive(connection, socket, draining, open).await;
}

/// A TLS handshake's failure in the error log.
fn handshake_failed(peer: SocketAddr, why: &dyn std::fmt::Display) {
    crate::log::error(format!("makala: {peer}: the TLS handshake failed: {why}").as_bytes());
}

/// What one connection's requests share.
struct Connection {
    peer: SocketAddr,
    local: SocketAddr,
    /// Whether it speaks TLS, which `$_SERVER` says.
    scheme: Scheme,
    limits: Limits,
    router: Arc<Router>,
    queue: mpsc::Sender<Job>,
    /// What the connection waits on, which its socket's clocks read.
    watch: Arc<Watch>,
    /// How many of its requests have begun.
    begun: AtomicU32,
    /// Rung where the connection must end while a response is under way,
    /// its body broken, too large or too slow after the response began,
    /// as `BodyFault` has it: the connection is
    /// dropped, its socket lingers, and the request's next write meets
    /// [`php_sapi::Aborted`].
    cut: Notify,
}

/// One connection, its requests served until it ends or is cut, or the
/// drain ends it: an idle one at once, and one with a request in hand
/// once that request's response has ended, as hyper's graceful shutdown
/// of an HTTP/1.1 connection has it.
async fn drive(
    connection: Arc<Connection>,
    socket: Socket,
    mut draining: watch::Receiver<bool>,
    _open: mpsc::Sender<Infallible>,
) {
    let limits = connection.limits;
    let service = {
        let connection = Arc::clone(&connection);
        service_fn(move |request| respond(Arc::clone(&connection), request))
    };
    // The head's limits are held from what hyper parsed ([`over_limit`]);
    // its buffer holds a head at both limits, and a head past it is
    // answered 431 by hyper, a request line among it. The count is
    // hyper's, the size of the array it parses into. The timeouts are the
    // socket's, since hyper's one header timeout runs from the end of a
    // response and answers nothing.
    let mut builder = http1::Builder::new();
    builder.max_headers(limits.header_count).max_buf_size(
        (limits.request_line + limits.header_bytes)
            .saturating_add(4)
            .max(LEAST_BUFFER),
    );
    let mut served = pin!(builder.serve_connection(TokioIo::new(socket), service));
    let mut cut = pin!(connection.cut.notified());
    let mut drain = pin!(draining.wait_for(|draining| *draining));
    let mut shutting_down = false;
    // A connection that ends on a failure says so in the error log, but
    // for a client gone between requests, the commonest end there is.
    let peer = connection.peer;
    std::future::poll_fn(|cx| {
        if cut.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        if !shutting_down && drain.as_mut().poll(cx).is_ready() {
            served.as_mut().graceful_shutdown();
            shutting_down = true;
        }
        served.as_mut().poll(cx).map(|ended| {
            if let Err(failed) = ended
                && !failed.is_incomplete_message()
            {
                failure(peer, &failed);
            }
        })
    })
    .await;
}

/// A connection's failure in the error log: the peer, then hyper's error
/// and the one under it, an I/O error or a limit's.
fn failure(peer: SocketAddr, failed: &hyper::Error) {
    let under = std::error::Error::source(failed)
        .map(|under| format!(": {under}"))
        .unwrap_or_default();
    crate::log::error(format!("makala: {peer}: {failed}{under}").as_bytes());
}

/// The first of two futures to end: what `a` yields, or `None` where `b`
/// ended first; the other is dropped unfinished.
async fn first_of<A: Future, B: Future>(a: A, b: B) -> Option<A::Output> {
    let (mut a, mut b) = (pin!(a), pin!(b));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = a.as_mut().poll(cx) {
            Poll::Ready(Some(output))
        } else if b.as_mut().poll(cx).is_ready() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await
}

/// One request, the connection's last where it reaches the cap on
/// requests per connection: its response then says `Connection: close`,
/// which hyper closes after.
async fn respond(
    connection: Arc<Connection>,
    request: Request<Incoming>,
) -> Result<Response<Reply>, Infallible> {
    // The body's length as hyper frames it: exact where it is sized, or
    // none, and chunked where it is not.
    connection
        .watch
        .request_began(match request.body().size_hint().exact() {
            Some(length) => BodyLength::Exact(length),
            None => BodyLength::Chunked,
        });
    let begun = connection.begun.fetch_add(1, Ordering::Relaxed) + 1;
    let mut access = Access::begun(connection.peer, &request);
    let body_in = access.as_ref().map(|access| Arc::clone(&access.body_in));
    let mut response = hand_over(&connection, request, body_in).await;
    if begun >= connection.limits.requests_per_connection {
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    if let Some(access) = &mut access {
        access.status = response.status();
    }
    response.body_mut().access = access;
    Ok(response)
}

/// One request: refused where it is past a limit ([`over_limit`]) or its
/// framing does not hold ([`framing`]); else routed on tokio's blocking
/// pool, which asks the disk ([`Router::route`]): a static file is sent
/// from here and a refusal answered, neither holding a request thread,
/// and a program's request is handed to the request side with
/// its body following it, and answered with the head the request sends
/// and the body it writes after it. The server answers on its own with a
/// 503 where the request side has stopped and takes no program; with the
/// refusal of a body that faulted before the request sent its head; where
/// the request ended without a head, as its ending answers it
/// ([`headless`]); and with a 500 where routing panicked, which nothing
/// that routes does.
async fn hand_over(
    connection: &Arc<Connection>,
    request: Request<Incoming>,
    body_in: Option<Arc<AtomicU64>>,
) -> Response<Reply> {
    let (parts, incoming) = request.into_parts();
    if let Some(refused) =
        over_limit(&parts, &connection.limits).or_else(|| framing::refusal(&parts))
    {
        return closing(connection, refused);
    }
    let head = Head {
        method: parts.method.as_str().as_bytes().to_vec(),
        target: parts.uri.path_and_query().map_or_else(
            || b"/".to_vec(),
            |target| target.as_str().as_bytes().to_vec(),
        ),
        protocol: if parts.version == Version::HTTP_10 {
            "HTTP/1.0"
        } else {
            "HTTP/1.1"
        },
        fields: parts
            .headers
            .iter()
            .map(|(name, value)| (name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()))
            .collect(),
        peer: connection.peer,
        local: connection.local,
        scheme: connection.scheme,
    };
    let router = Arc::clone(&connection.router);
    let asked = tokio::task::spawn_blocking(move || {
        let route = router.route(&head.method, head.path());
        (head, route)
    })
    .await;
    let (head, route) = match asked {
        Ok(asked) => asked,
        Err(failed) => {
            crate::log::error(
                format!(
                    "makala: {}: routing a request failed: {failed}",
                    connection.peer
                )
                .as_bytes(),
            );
            return plain(connection, StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let program = match route {
        Route::Program(program) => program,
        Route::Static(file) => {
            let head = file.head();
            let content = if parts.method == Method::HEAD {
                Content::Empty
            } else {
                Content::File(file.body())
            };
            return own_response(connection, &head, content);
        }
        Route::Refused(refusal) => {
            return own_response(connection, &refusal.head(), Content::Empty);
        }
    };
    let (feed, body) = mpsc::channel(BODY_PIECES);
    let (head_sent, answered) = oneshot::channel();
    let (out, pieces) = mpsc::channel(RESPONSE_PIECES);
    let (whole_sent, whole) = oneshot::channel();
    let (ended_sent, ended) = oneshot::channel();
    let job = Job {
        head,
        program,
        body,
        sink: Sink::new(head_sent, out, whole_sent),
        ended: ended_sent,
    };
    if connection.queue.send(job).await.is_err() {
        return plain(connection, StatusCode::SERVICE_UNAVAILABLE);
    }
    let (broke, mut fault) = oneshot::channel();
    tokio::spawn(pump(incoming, feed, broke, Arc::clone(connection), body_in));
    match answer(answered, &mut fault).await {
        Answer::Head(head) => {
            // A fault that came with the head is no longer the answer's:
            // closed, the channel takes no later one, which the pump then
            // cuts the connection for, and one already sent is cut here.
            fault.close();
            if let Ok(fault) = fault.try_recv() {
                cut(connection, fault);
            }
            php_response(
                connection,
                &head,
                Content::Written(Written { pieces, whole }),
            )
        }
        Answer::Broke(fault) => closing(connection, fault.status()),
        Answer::Headless => plain(connection, headless(ended.await.ok().flatten().as_ref())),
    }
}

/// The status a request that ended without sending a head is answered
/// with, read from how it ended: a 502 where php 8.5.12's allocator ended
/// its process, at the hard memory limit or its overflow panic, as nginx
/// answers Bad Gateway over the php-fpm worker that `_exit(137)`ed or
/// `abort()`ed there (`Z/Zend/zend_alloc.c:162`, `:394-405`) and wrote no
/// head, the request alone ended and the server serving on; a 500 for
/// any other, which a panic contained at the request's
/// edge leaves, its ending never sent. Neither the head the program set
/// nor what its buffer held reaches the client:
///
/// ```php
/// ob_start();
/// header('X-A: 1');
/// echo "body\n";
/// hash_pbkdf2('md5', 'p', 's', 1, PHP_INT_MAX);
/// ```
/// ```request makala
/// --GET--
/// q=1
/// ```
/// ```expect capture
/// 502
/// ```
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#[allow(rustdoc::invalid_codeblock_attributes)]
fn headless(ending: Option<&Ending>) -> StatusCode {
    match ending {
        Some(Ending::Terminated(_)) => StatusCode::BAD_GATEWAY,
        Some(Ending::Fatal(_) | Ending::Parse) | None => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The limits a parsed head is held to: its request line, 414
/// past [`Limits::request_line`]; its fields' bytes, each counted as its
/// name, value, colon, space and CRLF, 431 past [`Limits::header_bytes`];
/// and its body's length, 413 past [`Limits::body_bytes`] before a byte of
/// the body is read or `100 Continue` sent. A chunked body is counted as
/// it is read (`pump`).
fn over_limit(parts: &Parts, limits: &Limits) -> Option<StatusCode> {
    let line =
        parts.method.as_str().len() + 1 + parts.uri.to_string().len() + 1 + b"HTTP/1.1".len();
    let fields: usize = parts
        .headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len() + 4)
        .sum();
    let length = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<u64>().ok());
    if line > limits.request_line {
        Some(StatusCode::URI_TOO_LONG)
    } else if fields > limits.header_bytes {
        Some(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE)
    } else if length.is_some_and(|length| length > limits.body_bytes) {
        Some(StatusCode::PAYLOAD_TOO_LARGE)
    } else {
        None
    }
}

/// How a request is answered.
enum Answer {
    /// With the head it sent.
    Head(ResponseHead),
    /// With the refusal of its body, which broke first.
    Broke(BodyFault),
    /// With none: it ended without sending a head.
    Headless,
}

/// The request's head, or its body's fault where that comes first; a body
/// that ends whole leaves the head to wait for. A fault that has come is
/// read before a head that has: the pump sends it before it lets the
/// request read the body's end, so a request that answers once it has
/// read its body never answers ahead of its body's fault.
async fn answer(
    mut head: oneshot::Receiver<ResponseHead>,
    fault: &mut oneshot::Receiver<BodyFault>,
) -> Answer {
    let mut body_whole = false;
    std::future::poll_fn(|cx| {
        if !body_whole {
            match Pin::new(&mut *fault).poll(cx) {
                Poll::Ready(Ok(fault)) => return Poll::Ready(Answer::Broke(fault)),
                Poll::Ready(Err(_)) => body_whole = true,
                Poll::Pending => {}
            }
        }
        Pin::new(&mut head)
            .poll(cx)
            .map(|sent| sent.map_or(Answer::Headless, Answer::Head))
    })
    .await
}

/// Why a request's body was not read to its end.
#[derive(Debug, Clone, Copy)]
enum BodyFault {
    /// It broke: a chunk hyper's decoder refused (§7.1), or the client
    /// failed or closed before the body's end.
    Broken,
    /// A chunked body crossed [`Limits::body_bytes`].
    TooLarge,
    /// The client sent it slower than [`Limits::min_body_rate`], past the
    /// grace.
    TooSlow,
    /// It ended in a trailer section. hyper ends that section only at
    /// CRLF CRLF and parses it with a parser that takes a bare LF's blank
    /// line as its end and drops what follows, so a front end that ends
    /// it at the bare LF reads a request where this server read
    /// trailers; and PHP reads no trailer, as no web server hands one to
    /// php-fpm. The request is refused, and the connection with it.
    Trailers,
}

impl BodyFault {
    /// What the body did, as the error log says it.
    fn why(self) -> &'static str {
        match self {
            BodyFault::Broken => "broke",
            BodyFault::TooLarge => "passed the body limit",
            BodyFault::TooSlow => "came slower than the minimum data rate",
            BodyFault::Trailers => "ended in a trailer section",
        }
    }

    /// The answer to a request whose body broke before it sent a head.
    fn status(self) -> StatusCode {
        match self {
            BodyFault::Broken | BodyFault::Trailers => StatusCode::BAD_REQUEST,
            BodyFault::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            BodyFault::TooSlow => StatusCode::REQUEST_TIMEOUT,
        }
    }
}

/// The request's body onto its channel as hyper reads it, until it ends,
/// the request has gone and reads no more, or it faults: a fault goes to
/// the request's answer while that waits, and cuts the connection once
/// the response is under way. The channel closed is the body's end, which
/// the request reads as short where the body faulted, so a request thread
/// blocked reading a stalled client's body is freed at the minimum rate.
///
/// The rate is Kestrel's: only the time spent waiting on the client
/// counts, not the time the request leaves the channel full, and the body
/// faults once that time passes the grace, or the time what has arrived
/// takes at the rate where that is longer ([`MinDataRate::allowed`]).
///
/// [`MinDataRate::allowed`]: crate::options::MinDataRate::allowed
async fn pump(
    mut incoming: Incoming,
    feed: mpsc::Sender<Bytes>,
    broke: oneshot::Sender<BodyFault>,
    connection: Arc<Connection>,
    body_in: Option<Arc<AtomicU64>>,
) {
    let limits = connection.limits;
    let mut arrived = 0u64;
    let mut waited = Duration::ZERO;
    let fault = loop {
        let started = tokio::time::Instant::now();
        let next = std::future::poll_fn(|cx| Pin::new(&mut incoming).poll_frame(cx));
        let frame = match limits.min_body_rate {
            Some(rate) => {
                let left = rate.allowed(arrived).saturating_sub(waited);
                match tokio::time::timeout(left, next).await {
                    Ok(frame) => frame,
                    Err(_) => break BodyFault::TooSlow,
                }
            }
            None => next.await,
        };
        waited += started.elapsed();
        match frame {
            None => return,
            Some(Err(_)) => break BodyFault::Broken,
            Some(Ok(frame)) => match frame.into_data() {
                Ok(piece) => {
                    arrived += u64::try_from(piece.len()).unwrap_or(u64::MAX);
                    if let Some(body_in) = &body_in {
                        body_in.store(arrived, Ordering::Relaxed);
                    }
                    if arrived > limits.body_bytes {
                        break BodyFault::TooLarge;
                    }
                    if feed.send(piece).await.is_err() {
                        return;
                    }
                }
                Err(_) => break BodyFault::Trailers,
            },
        }
    };
    if let Err(fault) = broke.send(fault) {
        cut(&connection, fault);
    }
}

/// The connection cut for its request's body, which faulted once the
/// response was under way, and the cut in the error log.
fn cut(connection: &Connection, fault: BodyFault) {
    crate::log::error(
        format!(
            "makala: {}: the connection is cut, the request's body {}",
            connection.peer,
            fault.why()
        )
        .as_bytes(),
    );
    connection.cut.notify_one();
}

/// The response for the head a request sent, as php-fpm writes it and
/// the web server in front of it reads it: the lines
/// `sapi_cgi_send_headers` writes ([`php_sapi::cgi::lines`]), so a 304
/// carries no `Content-Type` of PHP's; the status their status line says,
/// 200 where there is none, with the reason phrase it carries, which is
/// the one PHP set (`header("HTTP/1.1 403 Access Denied")`) or php's for
/// the code; and each header line after it. A status whose code is not
/// three digits, which nginx refuses, is a 500: php-fpm's code is 200
/// from the request's start (`init_request_info`,
/// `Z/sapi/fpm/fpm/fpm_main.c:995`), as the engine's is, never 0.
fn php_response(connection: &Connection, head: &ResponseHead, content: Content) -> Response<Reply> {
    let lines = php_sapi::cgi::lines(head);
    let (mut status, mut reason) = (StatusCode::OK, &[][..]);
    let mut fields = Vec::with_capacity(lines.len());
    for line in &lines {
        match line {
            Line::Status(line) => {
                (status, reason) = line
                    .read()
                    .and_then(|(code, reason)| Some((StatusCode::from_u16(code).ok()?, reason)))
                    .unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, &[]));
            }
            Line::Field(field) => fields.push(*field),
        }
    }
    let mut response = response(connection, status, fields, content);
    if let Some(reason) = reason_phrase(status, reason) {
        response.extensions_mut().insert(reason);
    }
    response
}

/// The reason phrase hyper writes for `status` in place of its own:
/// `reason`, where it is one and not hyper's canonical text; an empty one
/// where there is none to write and the code has no canonical text, so
/// the line never reads `<none>`, hyper's text for that, as nginx writes
/// the code and a space; and none where hyper's own is the phrase. A
/// reason that is no phrase on the wire (a control byte) is not written.
fn reason_phrase(status: StatusCode, reason: &[u8]) -> Option<ReasonPhrase> {
    let canonical = status.canonical_reason().map(str::as_bytes);
    let set = (!reason.is_empty() && canonical != Some(reason))
        .then(|| ReasonPhrase::try_from(reason).ok())
        .flatten();
    match set {
        Some(set) => Some(set),
        None if canonical.is_none() => Some(ReasonPhrase::from_static(b"")),
        None => None,
    }
}

/// A response of `status` with the header lines `fields` in order, each
/// one hyper cannot carry left out; the body `content`.
fn response<'a>(
    connection: &Connection,
    status: StatusCode,
    fields: impl IntoIterator<Item = &'a [u8]>,
    content: Content,
) -> Response<Reply> {
    let mut response = Response::new(Reply {
        content,
        watch: Arc::clone(&connection.watch),
        access: None,
    });
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for line in fields {
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            continue;
        };
        let value = line[colon + 1..].trim_ascii_start();
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(&line[..colon]),
            HeaderValue::from_bytes(value),
        ) {
            headers.append(name, value);
        }
    }
    response
}

/// The response for a head of the server's own, a static file's or a
/// refusal's: its code and its header lines, with hyper's reason.
fn own_response(connection: &Connection, head: &ResponseHead, content: Content) -> Response<Reply> {
    let status = u16::try_from(head.code)
        .ok()
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response(
        connection,
        status,
        head.lines.iter().map(Vec::as_slice),
        content,
    )
}

/// A response of the server's own, with no body.
fn plain(connection: &Connection, status: StatusCode) -> Response<Reply> {
    response(connection, status, [], Content::Empty)
}

/// A response of the server's own after which the connection closes:
/// hyper reads its `Connection: close` and keeps nothing alive.
fn closing(connection: &Connection, status: StatusCode) -> Response<Reply> {
    let mut response = plain(connection, status);
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

/// A response's body. hyper lets go of it when the response has ended,
/// and the connection waits for its next request from then.
pub(crate) struct Reply {
    content: Content,
    watch: Arc<Watch>,
    /// What the access log says of it, where one is kept.
    access: Option<Access>,
}

/// Where a response's body comes from. A body that fails ends its
/// connection, so hyper never ends one cut short as a whole one (on
/// HTTP/1.0 with no length the close ends the body either way).
enum Content {
    /// None: the server's own answer, or a static file's to `HEAD`.
    Empty,
    /// What a request writes, piece by piece as it writes it, ending when
    /// its sink is dropped with the request, and failing where the
    /// request ended in a panic.
    Written(Written),
    /// A static file, read as the client takes it, failing where it ends
    /// short of its length.
    File(Sending),
}

/// A response body left unfinished.
#[derive(Debug)]
pub(crate) enum Unfinished {
    /// The request panicked while its response was under way.
    Panicked,
    /// A static file ended short of the length its head said.
    Shrank,
    /// A static file's read failed.
    ReadFailed(std::io::Error),
}

impl std::fmt::Display for Unfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unfinished::Panicked => {
                f.write_str("the request panicked while its response was under way")
            }
            Unfinished::Shrank => f.write_str("the static file shrank while it was sent"),
            Unfinished::ReadFailed(failed) => write!(f, "the static file's read failed: {failed}"),
        }
    }
}

impl std::error::Error for Unfinished {}

impl hyper::body::Body for Reply {
    type Data = Bytes;
    type Error = Unfinished;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Unfinished>>> {
        let reply = self.get_mut();
        let next = match &mut reply.content {
            Content::Empty => return Poll::Ready(None),
            Content::Written(written) => match std::task::ready!(written.pieces.poll_recv(cx)) {
                Some(piece) => Some(Ok(piece)),
                None => {
                    return Pin::new(&mut written.whole)
                        .poll(cx)
                        .map(|whole| whole.err().map(|_| Err(Unfinished::Panicked)));
                }
            },
            Content::File(file) => std::task::ready!(file.poll_piece(cx)),
        };
        if let (Some(Ok(piece)), Some(access)) = (&next, &mut reply.access) {
            access.body_out += u64::try_from(piece.len()).unwrap_or(u64::MAX);
        }
        Poll::Ready(next.map(|piece| piece.map(Frame::data)))
    }

    fn is_end_stream(&self) -> bool {
        match &self.content {
            Content::Empty => true,
            Content::Written(_) => false,
            Content::File(file) => file.left() == 0,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.content {
            Content::Empty => SizeHint::with_exact(0),
            Content::Written(_) => SizeHint::default(),
            Content::File(file) => SizeHint::with_exact(file.left()),
        }
    }
}

impl Drop for Reply {
    /// The response has ended, whole or cut: the connection waits for its
    /// next request, and the access log has its line.
    fn drop(&mut self) {
        self.watch.response_ended();
        if let Some(access) = &self.access {
            access.ended();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_accepted_socket_sets_tcp_nodelay() -> std::io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()?;
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let client = tokio::net::TcpStream::connect(listener.local_addr()?).await?;
            let (served, peer) = super::accepted(&listener).await?;
            assert_eq!(peer, client.local_addr()?);
            assert!(served.nodelay()?, "the accepted socket keeps Nagle on");
            Ok(())
        })
    }
}
