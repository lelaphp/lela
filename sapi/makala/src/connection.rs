//! The connection side (#2337): hyper's HTTP/1.1 server on a tokio
//! runtime of one thread, which owns every socket, reads each request's
//! framing as RFC 9112 has it and never runs PHP, as the BEAM keeps
//! anything that blocks off the schedulers that own sockets
//! (research/production-server.r2.md §4.10, M2 and M4). A request goes to
//! the request side whole, on the bounded queue: while the queue is full
//! its connection waits for room. That and the connection cap are the
//! backpressure (#2342), Ranch's (research/production-server.r2.md M7):
//! at the cap the accept loop stops accepting, so new clients wait in
//! the kernel's listen queue and none is refused. The body follows on a
//! bounded channel as hyper reads it, and the response comes back on
//! two: the head once, then the body piece by piece, which hyper frames
//! chunked on HTTP/1.1 and
//! close-delimited on HTTP/1.0 where PHP set no length. A `HEAD`
//! response has none: PHP writes no body for one (php's `headers_only`),
//! and hyper sends none.
//!
//! Every limit of [`Limits`] (#2341) is applied on this side: the head's
//! request line, field bytes and declared body length where hyper has
//! parsed it ([`over_limit`]), the field count by hyper's parser, the
//! keep-alive and header clocks and the response's minimum rate by the
//! connection's socket ([`Socket`]), a chunked body's length and the
//! body's minimum rate as the body is read ([`pump`]), and the cap on
//! requests per connection as each request begins ([`respond`]); the
//! cap on connections is the accept loop's (#2342). hyper
//! answers `100 Continue` when the body is first read, which is after
//! the head has passed its limits.
//!
//! SIGTERM ends the accept loop and begins the drain
//! ([`serve_until_drained`], #2344),
//! bounded by [`Limits::shutdown_timeout`].

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::body::{Body as _, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{CONNECTION, CONTENT_LENGTH, HeaderName, HeaderValue};
use hyper::http::request::Parts;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Version};
use hyper_util::rt::TokioIo;
use php_sapi::ResponseHead;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot, watch};

use crate::framing;
use crate::options::Limits;
use crate::request::{Head, Job, Sink, Written};
use crate::socket::{Socket, Watch};

/// How many pieces of a request's body wait for the request to read them.
const BODY_PIECES: usize = 8;

/// How many pieces of a response wait for the client to take them.
const RESPONSE_PIECES: usize = 8;

/// The least hyper's read buffer may hold, which its builder asserts.
const LEAST_BUFFER: usize = 8192;

/// The connection side: its runtime, and SIGTERM, which begins the drain.
pub(crate) struct ConnectionSide {
    runtime: tokio::runtime::Runtime,
    terminate: Signal,
}

/// How the drain ended.
pub(crate) struct Drained {
    /// When the shutdown timeout ends, counted from the SIGTERM: what the
    /// request threads still run then is left to the process's end.
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
    /// serves: every connection accepted on `listener` served on its own
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
        listener: std::net::TcpListener,
        queue: mpsc::Sender<Job>,
    ) -> std::io::Result<Drained> {
        let ConnectionSide {
            runtime,
            mut terminate,
        } = self;
        runtime.block_on(serve_until_drained(limits, listener, queue, &mut terminate))
    }
}

/// The accept loop until SIGTERM, then the drain, Ranch's and Kestrel's
/// (research/production-server.r2.md M8, M10), in the order a supervisor
/// stops what it started, the last started first: the listener is
/// closed, so no connection is accepted after the signal and a client
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
    listener: std::net::TcpListener,
    queue: mpsc::Sender<Job>,
    terminate: &mut Signal,
) -> std::io::Result<Drained> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let slots = Arc::new(Semaphore::new(
        usize::try_from(limits.connections)
            .map_or(Semaphore::MAX_PERMITS, |n| n.min(Semaphore::MAX_PERMITS)),
    ));
    let (drain, draining) = watch::channel(false);
    let (open, mut all_ended) = mpsc::channel::<Infallible>(1);
    let accepting = accept(&listener, limits, &slots, queue, draining, open);
    if let Some(failed) = first_of(accepting, terminate.recv()).await {
        return Err(failed);
    }
    let deadline = tokio::time::Instant::now() + limits.shutdown_timeout;
    drop(listener);
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

/// The accept loop: each connection on a task of its own, its requests
/// served in order, as HTTP/1.1 keeps them. A connection holds one of
/// [`Limits::connections`] slots until its socket has lingered and
/// closed; with none free the loop waits for one before it accepts
/// again, as Ranch's acceptor sleeps at its cap, and the kernel's listen
/// queue holds the clients that come meanwhile. It returns only with the
/// slots closed, which nothing does; the drain drops it unfinished.
async fn accept(
    listener: &tokio::net::TcpListener,
    limits: Limits,
    slots: &Arc<Semaphore>,
    queue: mpsc::Sender<Job>,
    draining: watch::Receiver<bool>,
    open: mpsc::Sender<Infallible>,
) -> std::io::Error {
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
        let Ok((stream, peer)) = listener.accept().await else {
            drop(slot);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            continue;
        };
        let connection = Arc::new(Connection {
            peer,
            local,
            limits,
            queue: queue.clone(),
            watch: Arc::new(Watch::new()),
            begun: AtomicU32::new(0),
            cut: Notify::new(),
        });
        let socket = Socket::new(stream, Arc::clone(&connection.watch), limits, slot);
        tokio::spawn(drive(connection, socket, draining.clone(), open.clone()));
    }
}

/// What one connection's requests share.
struct Connection {
    peer: SocketAddr,
    local: SocketAddr,
    limits: Limits,
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
    // A connection's end, the client's close among them, is nothing the
    // server acts on until its logs (#2345).
    std::future::poll_fn(|cx| {
        if cut.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        if !shutting_down && drain.as_mut().poll(cx).is_ready() {
            served.as_mut().graceful_shutdown();
            shutting_down = true;
        }
        served.as_mut().poll(cx).map(drop)
    })
    .await;
}

/// The first of two futures to end: `a`'s output, or `None` where `b`
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
    connection.watch.request_began();
    let begun = connection.begun.fetch_add(1, Ordering::Relaxed) + 1;
    let mut response = hand_over(&connection, request).await;
    if begun >= connection.limits.requests_per_connection {
        response
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }
    Ok(response)
}

/// One request: refused where it is past a limit ([`over_limit`]) or its
/// framing does not hold ([`framing`]); else handed to the request side
/// with its body following it, and answered with the head the request
/// sends and the body it writes after it. The server answers on its own
/// with a 503 where it is stopping and takes no request; with the refusal
/// of a body that faulted before the request sent its head; and with a
/// 500 where the request ended without a head, which only a panic
/// contained at its edge leaves.
async fn hand_over(connection: &Arc<Connection>, request: Request<Incoming>) -> Response<Reply> {
    let (parts, incoming) = request.into_parts();
    if let Some(refused) =
        over_limit(&parts, &connection.limits).or_else(|| framing::refusal(&parts))
    {
        return closing(connection, refused);
    }
    let (feed, body) = mpsc::channel(BODY_PIECES);
    let (head_sent, head) = oneshot::channel();
    let (out, pieces) = mpsc::channel(RESPONSE_PIECES);
    let (whole_sent, whole) = oneshot::channel();
    let head_read = Head {
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
    };
    let job = Job {
        head: head_read,
        body,
        sink: Sink::new(head_sent, out, whole_sent),
    };
    if connection.queue.send(job).await.is_err() {
        return plain(connection, StatusCode::SERVICE_UNAVAILABLE);
    }
    let (broke, mut fault) = oneshot::channel();
    tokio::spawn(pump(incoming, feed, broke, Arc::clone(connection)));
    match answer(head, &mut fault).await {
        Answer::Head(head) => {
            // A fault that came with the head is no longer the answer's:
            // closed, the channel takes no later one, which the pump then
            // cuts the connection for, and one already sent is cut here.
            fault.close();
            if fault.try_recv().is_ok() {
                connection.cut.notify_one();
            }
            response(connection, &head, Written { pieces, whole })
        }
        Answer::Broke(fault) => closing(connection, fault.status()),
        Answer::Headless => plain(connection, StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// The limits a parsed head is held to (#2341): its request line, 414
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
    if broke.send(fault).is_err() {
        connection.cut.notify_one();
    }
}

/// The response for the head a request sent: its code, 200 where it set
/// none, and its header lines in order, each one hyper cannot carry left
/// out; the body what the request writes after it.
fn response(connection: &Connection, head: &ResponseHead, written: Written) -> Response<Reply> {
    let mut response = Response::new(Reply {
        written: Some(written),
        watch: Arc::clone(&connection.watch),
    });
    let code = u16::try_from(head.code).unwrap_or(500);
    *response.status_mut() = match code {
        0 => StatusCode::OK,
        code => StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let headers = response.headers_mut();
    for line in &head.lines {
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

/// A response of the server's own, with no body.
fn plain(connection: &Connection, status: StatusCode) -> Response<Reply> {
    let mut response = Response::new(Reply {
        written: None,
        watch: Arc::clone(&connection.watch),
    });
    *response.status_mut() = status;
    response
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

/// A response's body: what a request writes, piece by piece as it writes
/// it, ending when its sink is dropped with the request, and failing
/// where the request ended in a panic, so hyper drops the connection
/// rather than end a chunked body cut short as a whole one (on HTTP/1.0
/// with no length the close ends the body either way); or none, the
/// server's own answer. hyper lets go of it when the response has ended, and the
/// connection waits for its next request from then.
pub(crate) struct Reply {
    written: Option<Written>,
    watch: Arc<Watch>,
}

/// A response body that a request's panic left unfinished.
#[derive(Debug)]
pub(crate) struct Unfinished;

impl std::fmt::Display for Unfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the request panicked while its response was under way")
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
        let Some(written) = &mut self.get_mut().written else {
            return Poll::Ready(None);
        };
        if let Some(piece) = std::task::ready!(written.pieces.poll_recv(cx)) {
            return Poll::Ready(Some(Ok(Frame::data(piece))));
        }
        Pin::new(&mut written.whole)
            .poll(cx)
            .map(|whole| whole.err().map(|_| Err(Unfinished)))
    }

    fn is_end_stream(&self) -> bool {
        self.written.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        match self.written {
            Some(_) => SizeHint::default(),
            None => SizeHint::with_exact(0),
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.watch.response_ended();
    }
}
