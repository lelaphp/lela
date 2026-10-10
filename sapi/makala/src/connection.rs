//! The connection side (#2337): hyper's HTTP/1.1 server on a tokio
//! runtime of one thread, which owns every socket, reads each request's
//! framing as RFC 9112 has it and never runs PHP, as the BEAM keeps
//! anything that blocks off the schedulers that own sockets
//! (research/production-server.r2.md §4.10, M2 and M4). A request goes to
//! the request side whole, on the bounded queue: while the queue is full
//! its connection waits for room, which is where the backpressure of
//! #2342 stands. The body follows on a bounded channel as hyper reads it,
//! and the response comes back on two: the head once, then the body
//! piece by piece, which hyper frames chunked on HTTP/1.1 and
//! close-delimited on HTTP/1.0 where PHP set no length.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper::body::{Body as _, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{CONNECTION, HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use php_sapi::ResponseHead;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::framing;
use crate::options::{Limits, Options};
use crate::request::{Head, Job, Sink};
use crate::socket::Socket;

/// How many pieces of a request's body wait for the request to read them.
const BODY_PIECES: usize = 8;

/// How many pieces of a response wait for the client to take them.
const RESPONSE_PIECES: usize = 8;

/// The connection side, on this thread for as long as the process serves:
/// every connection accepted on `listener` served on its own task, its
/// requests put on `queue`. It returns only where it could not start; the
/// drain that ends it is #2344's.
///
/// # Errors
///
/// What the runtime or the listener could not start with.
pub(crate) fn run(
    options: &Options,
    listener: std::net::TcpListener,
    queue: mpsc::Sender<Job>,
) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(accept(options.limits, listener, queue))
}

/// The accept loop: each connection on a task of its own, its requests
/// served in order, as HTTP/1.1 keeps them.
async fn accept(
    limits: Limits,
    listener: std::net::TcpListener,
    queue: mpsc::Sender<Job>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let local = listener.local_addr()?;
    loop {
        // A connection that failed before it was accepted, or a process
        // out of descriptors: the next is tried after a pause, so a full
        // table does not spin the loop.
        let Ok((stream, peer)) = listener.accept().await else {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            continue;
        };
        let connection = Arc::new(Connection {
            peer,
            local,
            limits,
            queue: queue.clone(),
            cut: Notify::new(),
        });
        tokio::spawn(drive(connection, stream));
    }
}

/// What one connection's requests share.
struct Connection {
    peer: SocketAddr,
    local: SocketAddr,
    limits: Limits,
    queue: mpsc::Sender<Job>,
    /// Rung where the connection must end while a response is under way,
    /// its body broken after the response began: the connection is
    /// dropped, its socket lingers, and the request's next write meets
    /// [`php_sapi::Aborted`].
    cut: Notify,
}

/// One connection, its requests served until it ends or is cut.
async fn drive(connection: Arc<Connection>, stream: tokio::net::TcpStream) {
    let limits = connection.limits;
    let service = {
        let connection = Arc::clone(&connection);
        service_fn(move |request| respond(Arc::clone(&connection), request))
    };
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout)
        .max_headers(limits.header_count)
        .max_buf_size(limits.request_line + limits.header_bytes);
    let served = builder.serve_connection(TokioIo::new(Socket::new(stream)), service);
    // A connection's end, the client's close among them, is nothing the
    // server acts on until its logs (#2345).
    first_of(served, connection.cut.notified()).await;
}

/// Whichever of two futures ends first, the other dropped unfinished.
async fn first_of<A: Future, B: Future>(a: A, b: B) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    std::future::poll_fn(|cx| {
        if a.as_mut().poll(cx).is_ready() || b.as_mut().poll(cx).is_ready() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// One request: refused where its framing does not hold ([`framing`]);
/// else handed to the request side with its body following it, and
/// answered with the head the request sends and the body it writes after
/// it. The server answers on its own with a 503 where it is stopping and
/// takes no request; with the refusal of a body that broke before the
/// request sent its head; and with a 500 where the request ended without
/// a head, which only a panic contained at its edge leaves.
async fn respond(
    connection: Arc<Connection>,
    request: Request<Incoming>,
) -> Result<Response<Reply>, Infallible> {
    let (parts, incoming) = request.into_parts();
    if let Some(refused) = framing::refusal(&parts) {
        return Ok(closing(refused));
    }
    let (feed, body) = mpsc::channel(BODY_PIECES);
    let (head_sent, head) = oneshot::channel();
    let (out, written) = mpsc::channel(RESPONSE_PIECES);
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
        sink: Sink::new(head_sent, out),
    };
    if connection.queue.send(job).await.is_err() {
        return Ok(plain(StatusCode::SERVICE_UNAVAILABLE));
    }
    let (broke, mut fault) = oneshot::channel();
    tokio::spawn(pump(incoming, feed, broke, Arc::clone(&connection)));
    Ok(match answer(head, &mut fault).await {
        Answer::Head(head) => {
            // A fault that came with the head is no longer the answer's:
            // closed, the channel takes no later one, which the pump then
            // cuts the connection for, and one already sent is cut here.
            fault.close();
            if fault.try_recv().is_ok() {
                connection.cut.notify_one();
            }
            response(&head, written)
        }
        Answer::Broke(fault) => closing(fault.status()),
        Answer::Headless => plain(StatusCode::INTERNAL_SERVER_ERROR),
    })
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
/// that ends whole leaves the head to wait for.
async fn answer(
    mut head: oneshot::Receiver<ResponseHead>,
    fault: &mut oneshot::Receiver<BodyFault>,
) -> Answer {
    let mut body_whole = false;
    std::future::poll_fn(|cx| {
        if let Poll::Ready(sent) = Pin::new(&mut head).poll(cx) {
            return Poll::Ready(sent.map_or(Answer::Headless, Answer::Head));
        }
        if !body_whole {
            match Pin::new(&mut *fault).poll(cx) {
                Poll::Ready(Ok(fault)) => return Poll::Ready(Answer::Broke(fault)),
                Poll::Ready(Err(_)) => body_whole = true,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    })
    .await
}

/// Why a request's body was not read to its end.
#[derive(Debug, Clone, Copy)]
enum BodyFault {
    /// It broke: a chunk hyper's decoder refused (§7.1), or the client
    /// failed or closed before the body's end.
    Broken,
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
        }
    }
}

/// The request's body onto its channel as hyper reads it, until it ends,
/// the request has gone and reads no more, or it breaks: a fault goes to
/// the request's answer while that waits, and cuts the connection once
/// the response is under way. The channel closed is the body's end, which
/// the request reads as short where the body broke.
async fn pump(
    mut incoming: Incoming,
    feed: mpsc::Sender<Bytes>,
    broke: oneshot::Sender<BodyFault>,
    connection: Arc<Connection>,
) {
    let fault = loop {
        let frame = std::future::poll_fn(|cx| Pin::new(&mut incoming).poll_frame(cx)).await;
        match frame {
            None => return,
            Some(Err(_)) => break BodyFault::Broken,
            Some(Ok(frame)) => match frame.into_data() {
                Ok(piece) => {
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
fn response(head: &ResponseHead, written: mpsc::Receiver<Bytes>) -> Response<Reply> {
    let mut response = Response::new(Reply::Written(written));
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
fn plain(status: StatusCode) -> Response<Reply> {
    let mut response = Response::new(Reply::Empty);
    *response.status_mut() = status;
    response
}

/// A response of the server's own after which the connection closes:
/// hyper reads its `Connection: close` and keeps nothing alive.
fn closing(status: StatusCode) -> Response<Reply> {
    let mut response = plain(status);
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

/// A response's body: what a request writes, piece by piece as it writes
/// it, ending when its sink is dropped with the request; or none, the
/// server's own answer.
pub(crate) enum Reply {
    Written(mpsc::Receiver<Bytes>),
    Empty,
}

impl hyper::body::Body for Reply {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Reply::Written(written) => written
                .poll_recv(cx)
                .map(|piece| piece.map(|piece| Ok(Frame::data(piece)))),
            Reply::Empty => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Reply::Empty)
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Reply::Written(_) => SizeHint::default(),
            Reply::Empty => SizeHint::with_exact(0),
        }
    }
}
