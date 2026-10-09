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
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::{Body as _, Bytes, Frame, Incoming, SizeHint};
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use php_sapi::ResponseHead;
use tokio::sync::{mpsc, oneshot};

use crate::config::{Config, Limits};
use crate::request::{Head, Job, Sink};

/// How many pieces of a request's body wait for the request to read them.
const BODY_PIECES: usize = 8;

/// How many pieces of a response wait for the client to take them.
const RESPONSE_PIECES: usize = 8;

/// The connection side, on this thread until the listener fails: every
/// connection accepted on `listener` served on its own task, its requests
/// put on `queue`.
///
/// # Errors
///
/// What the runtime or the listener failed with.
pub(crate) fn run(
    config: &Config,
    listener: std::net::TcpListener,
    queue: mpsc::Sender<Job>,
) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(accept(config.limits, listener, queue))
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
        let queue = queue.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| respond(queue.clone(), peer, local, request));
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_timeout)
                .max_headers(limits.header_count)
                .max_buf_size(limits.request_line + limits.header_bytes);
            // A connection's end, the client's close among them, is
            // nothing the server acts on until its logs (#2345).
            builder
                .serve_connection(TokioIo::new(stream), service)
                .await
                .ok();
        });
    }
}

/// One request: handed to the request side with its body following it,
/// and answered with the head the request sends and the body it writes
/// after it; a 503 where the server is stopping and takes no request,
/// and a 500 where the request ended without a head, which only a panic
/// contained at its edge leaves.
async fn respond(
    queue: mpsc::Sender<Job>,
    peer: SocketAddr,
    local: SocketAddr,
    request: Request<Incoming>,
) -> Result<Response<Reply>, Infallible> {
    let (parts, incoming) = request.into_parts();
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
        peer,
        local,
    };
    let job = Job {
        head: head_read,
        body,
        sink: Sink::new(head_sent, out),
    };
    if queue.send(job).await.is_err() {
        return Ok(plain(StatusCode::SERVICE_UNAVAILABLE));
    }
    tokio::spawn(pump(incoming, feed));
    Ok(match head.await {
        Ok(head) => response(&head, written),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR),
    })
}

/// The request's body onto its channel as hyper reads it, until it ends,
/// fails, or the request has gone and reads no more; the channel closed
/// is the body's end. Trailers are not PHP's to read.
async fn pump(mut incoming: Incoming, feed: mpsc::Sender<Bytes>) {
    loop {
        let frame = std::future::poll_fn(|cx| Pin::new(&mut incoming).poll_frame(cx)).await;
        let Some(Ok(frame)) = frame else {
            return;
        };
        if let Ok(piece) = frame.into_data()
            && feed.send(piece).await.is_err()
        {
            return;
        }
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
    let mut response = Response::new(Reply::Whole(None));
    *response.status_mut() = status;
    response
}

/// A response's body: what a request writes, piece by piece as it writes
/// it, ending when its sink is dropped with the request; or the server's
/// own, whole.
pub(crate) enum Reply {
    Written(mpsc::Receiver<Bytes>),
    Whole(Option<Bytes>),
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
            Reply::Whole(whole) => Poll::Ready(whole.take().map(|whole| Ok(Frame::data(whole)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self, Reply::Whole(None))
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Reply::Written(_) => SizeHint::default(),
            Reply::Whole(whole) => SizeHint::with_exact(
                whole
                    .as_ref()
                    .map_or(0, |whole| u64::try_from(whole.len()).unwrap_or(u64::MAX)),
            ),
        }
    }
}
