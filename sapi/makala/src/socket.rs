//! A connection's socket as hyper reads and writes it: the clocks that
//! bound how long a client may keep the server waiting, and how the
//! socket ends.
//!
//! Three clocks, Kestrel's (research/production-server.r2.md §1.3), each
//! running only while the server waits on the client for what it names,
//! which the connection's [`Watch`] says:
//!
//! - **Keep-alive idle**, from the accept or the last response until a
//!   request's first byte: past it the connection closes with nothing
//!   sent, as an end of input hyper closes on.
//! - **The header timeout**, from a head's first byte until hyper has
//!   parsed it: past it the server answers 408 and closes. A client that
//!   trickles bytes does not move it. The first byte is the first the
//!   socket reads while the connection idles; a head whose start hyper
//!   read with the request before it, which a pipelining client sends,
//!   is timed from that request's response's end ([`Boundary`]).
//! - **The minimum data rate** of the response, Kestrel's
//!   `MinResponseDataRate`, on each write the kernel would not take
//!   ([`WriteClock::wrote`]): past its deadline with a write stalled, the
//!   write fails, hyper drops the connection, and the request's next
//!   write meets `Aborted`. The body's rate is the pump's
//!   (`connection::pump`).
//!
//! Closing a socket that still holds bytes the client sent and the server
//! never read makes the kernel reset the connection, and a reset can
//! destroy the last response before the client reads it: the 400 that
//! refuses a request whose body the server will not read, or the answer
//! before a close the framing asks for. So a socket is not closed where
//! hyper lets go of it. It lingers, as Cowboy's `linger_timeout` has a
//! connection linger: its write side is shut, which the client reads as
//! the response's end, and what the client still sends is read and
//! dropped until it closes its side or [`LINGER`] has passed. A socket
//! let go outside the runtime, or as the runtime is dropped at the
//! drain's deadline, closes at once. A lingering socket holds its fd, and its connection's
//! slot under the cap, for that second beyond its connection. A TLS
//! connection's write side is shut by its `close_notify` first.
//!
//! A socket carries its bytes clear or under TLS ([`Wire`]), and
//! its clocks and the 408 count on what TLS carries. A clear connection
//! whose first byte is a TLS record's, `0x16`, a handshake sent to the
//! clear listener, is refused: its read fails, so hyper drops it unanswered,
//! as Bandit and Kestrel close one, and the error log says why
//! (research/production-server.r2.md M19).

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, Sleep};
use tokio_rustls::server::TlsStream;

use crate::boundary::{BodyLength, Boundary};
use crate::options::{Limits, MinDataRate};

/// How long a closing socket reads and drops what its client still
/// sends: Cowboy's `linger_timeout`, 1000 ms.
const LINGER: Duration = Duration::from_secs(1);

/// What a connection waits on, which says which clock of its socket
/// runs: set by its requests as each begins and as its response ends, and
/// by its socket as a head's first byte arrives, which the bytes it has
/// read, held against the requests they carry, say.
pub(crate) struct Watch {
    held: Mutex<Watched>,
}

/// What a [`Watch`] holds.
struct Watched {
    phase: Phase,
    /// How many responses have ended.
    responses: u64,
    /// The task whose read waits while a request is in hand, woken when
    /// the response ends: hyper polls its read again only when woken, and
    /// the keep-alive clock that starts then must be set.
    reader: Option<Waker>,
    /// Where the bytes read stand against the requests they carry.
    boundary: Boundary,
}

/// What a connection waits on.
#[derive(Debug, Clone, Copy)]
enum Phase {
    /// A request's first byte, since the accept or the last response.
    Idle(Instant),
    /// The rest of a head, since its first byte.
    Head(Instant),
    /// Nothing of the client's head: a request is in hand until its
    /// response ends.
    Busy,
}

impl Watch {
    /// A connection just accepted, waiting for its first request.
    pub(crate) fn new() -> Watch {
        Watch {
            held: Mutex::new(Watched {
                phase: Phase::Idle(Instant::now()),
                responses: 0,
                reader: None,
                boundary: Boundary::default(),
            }),
        }
    }

    /// A request's head is parsed and the request in hand, its body
    /// `length` long as hyper frames it.
    pub(crate) fn request_began(&self, length: BodyLength) {
        let mut held = self.held();
        held.phase = Phase::Busy;
        held.boundary.began(length);
    }

    /// A response has ended, and the connection waits for the next
    /// request, whose head is timed from now where any of it has been
    /// read: its read is woken to set the clock.
    pub(crate) fn response_ended(&self) {
        let reader = {
            let mut held = self.held();
            let now = Instant::now();
            held.phase = if held.boundary.ahead() {
                Phase::Head(now)
            } else {
                Phase::Idle(now)
            };
            held.responses += 1;
            held.reader.take()
        };
        if let Some(reader) = reader {
            reader.wake();
        }
    }

    fn held(&self) -> MutexGuard<'_, Watched> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn phase(&self) -> Phase {
        self.held().phase
    }

    fn responses(&self) -> u64 {
        self.held().responses
    }

    /// A read waits while a request is in hand, to be woken at its
    /// response's end.
    fn waits(&self, reader: &Waker) {
        self.held().reader = Some(reader.clone());
    }

    /// `bytes` read at `now`: a head's first where the connection idled
    /// and they hold the start of a request.
    fn arrived(&self, bytes: &[u8], now: Instant) {
        let mut held = self.held();
        if held.boundary.read(bytes)
            && let Phase::Idle(_) = held.phase
        {
            held.phase = Phase::Head(now);
        }
    }
}

/// The first byte of a TLS record of a handshake, which no HTTP request
/// begins with (RFC 8446 §5.1, `handshake(22)`).
const TLS_HANDSHAKE: u8 = 0x16;

/// What a connection's bytes travel over: the TCP stream itself, or TLS
/// over it.
pub(crate) enum Wire {
    /// The clear listener's: HTTP on the TCP stream.
    Clear(TcpStream),
    /// The TLS listener's, its handshake made: HTTP under TLS. rustls
    /// takes up to its buffer's 64 KiB of a response before a write waits
    /// on the client, so the minimum response rate starts once that has
    /// filled, as it starts once the kernel's send buffer has on a clear
    /// connection.
    Tls(Box<TlsStream<TcpStream>>),
}

impl Wire {
    /// The TCP stream under it.
    fn into_tcp(self) -> TcpStream {
        match self {
            Wire::Clear(stream) => stream,
            Wire::Tls(tls) => tls.into_inner().0,
        }
    }
}

/// The socket of one connection, with its clocks, which lingers when it
/// is let go.
pub(crate) struct Socket {
    /// What its bytes travel over, taken only when the socket is let go.
    stream: Option<Wire>,
    /// The peer of a clear connection whose first read has not come, which
    /// a TLS handshake must not begin: one that does is refused and logged
    /// under it.
    clear_peer: Option<SocketAddr>,
    watch: Arc<Watch>,
    limits: Limits,
    /// The keep-alive idle or header timeout's wake-up, while a read waits.
    read_timer: Option<Pin<Box<Sleep>>>,
    writes: WriteClock,
    /// The connection's slot under the cap, held until the socket has
    /// lingered and closed.
    slot: Option<OwnedSemaphorePermit>,
}

/// The response's minimum data rate over a connection's writes.
#[derive(Default)]
struct WriteClock {
    /// The wake-up, while a write stalls.
    timer: Option<Pin<Box<Sleep>>>,
    /// When a stalled write fails, moved on by each write the client
    /// takes.
    deadline: Option<Instant>,
    /// Whether the last write found the kernel would take nothing.
    stalled: bool,
    /// The response the deadline is for, by the count of responses
    /// ended: a new response starts with none, so no credit a fast
    /// reader earned on one response carries to the next.
    response: u64,
}

impl Socket {
    /// The socket of a connection just accepted, its clocks read from
    /// `watch` and `limits`.
    pub(crate) fn new(
        stream: Wire,
        peer: SocketAddr,
        watch: Arc<Watch>,
        limits: Limits,
        slot: OwnedSemaphorePermit,
    ) -> Socket {
        Socket {
            clear_peer: matches!(stream, Wire::Clear(_)).then_some(peer),
            stream: Some(stream),
            watch,
            limits,
            read_timer: None,
            writes: WriteClock::default(),
            slot: Some(slot),
        }
    }

    /// When the read clock that runs ends, and what the connection does
    /// then, where one runs.
    fn read_deadline(&self) -> Option<(Instant, Expiry)> {
        match self.watch.phase() {
            Phase::Idle(since) => Some((later(since, self.limits.keep_alive_idle), Expiry::Idle)),
            Phase::Head(since) => Some((later(since, self.limits.header_timeout), Expiry::Head)),
            Phase::Busy => None,
        }
    }

    /// The read clock passed: an idle connection reads as ended, and a
    /// head is answered 408, which hyper never sends, before the read
    /// fails and hyper drops the connection. The 408 goes straight to the
    /// wire, past hyper's buffer, which holds nothing then unless a write
    /// has stalled, when it is left unsent; what the wire does not take at
    /// once is left unsent too.
    fn expire(&mut self, expiry: Expiry, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match expiry {
            Expiry::Idle => Poll::Ready(Ok(())),
            Expiry::Head => {
                if let (Some(stream), false) = (&mut self.stream, self.writes.stalled) {
                    let answer = format!(
                        "HTTP/1.1 408 Request Timeout\r\nconnection: close\r\n\
                         content-length: 0\r\ndate: {}\r\n\r\n",
                        httpdate::fmt_http_date(SystemTime::now())
                    );
                    let mut stream = Pin::new(stream);
                    // The poll's outcome is dropped either way: the
                    // connection ends on the expiry whatever was sent.
                    let _sent = stream.as_mut().poll_write(cx, answer.as_bytes());
                    let _flushed = stream.poll_flush(cx);
                }
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the head did not arrive within the header timeout",
                )))
            }
        }
    }

    /// The stream, which is gone only once the socket is let go.
    fn stream(&mut self) -> io::Result<Pin<&mut Wire>> {
        self.stream
            .as_mut()
            .map(Pin::new)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))
    }
}

impl WriteClock {
    /// A write's outcome, held to `rate` for the connection's `response`:
    /// Kestrel's deadline, where each write the client takes moves it on
    /// by the time its bytes take at the rate, a write alone gets at
    /// least the grace, and a write that stalls past it fails. Taken is
    /// what the kernel's send buffer took, as Kestrel counts a flush
    /// done, so a client that stops reading holds its request thread for
    /// the grace or the time the bytes it was sent take at the rate,
    /// whichever is longer.
    fn wrote(
        &mut self,
        polled: Poll<io::Result<usize>>,
        rate: Option<MinDataRate>,
        response: u64,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<usize>> {
        if response != self.response {
            self.response = response;
            self.deadline = None;
        }
        let now = Instant::now();
        match polled {
            Poll::Ready(Ok(n)) => {
                self.stalled = false;
                if let Some(rate) = rate {
                    let bytes = u64::try_from(n).unwrap_or(u64::MAX);
                    let alone = later(now, rate.allowed(bytes));
                    self.deadline = Some(match self.deadline {
                        Some(deadline) => alone.max(later(deadline, rate.time_for(bytes))),
                        None => alone,
                    });
                }
                Poll::Ready(Ok(n))
            }
            Poll::Pending => {
                let fresh = !self.stalled;
                self.stalled = true;
                let Some(rate) = rate else {
                    return Poll::Pending;
                };
                let grace = later(now, rate.grace);
                let deadline = match self.deadline {
                    Some(deadline) if fresh => deadline.max(grace),
                    Some(deadline) => deadline,
                    None => grace,
                };
                self.deadline = Some(deadline);
                if wake_at(&mut self.timer, deadline, cx).is_ready() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the client takes the response slower than the minimum data rate",
                    )));
                }
                Poll::Pending
            }
            failed @ Poll::Ready(Err(_)) => failed,
        }
    }
}

/// `at` and `by` after it, or far off where that is past what an
/// `Instant` holds, as a limit the line set to years can be.
fn later(at: Instant, by: Duration) -> Instant {
    at.checked_add(by)
        .or_else(|| at.checked_add(FAR_OFF))
        .unwrap_or(at)
}

/// Far off: thirty years, as tokio's own far future is.
const FAR_OFF: Duration = Duration::from_hours(24 * 365 * 30);

/// What a read clock's end does.
#[derive(Debug, Clone, Copy)]
enum Expiry {
    /// Closes an idle connection.
    Idle,
    /// Answers a head 408.
    Head,
}

/// `timer` set for `deadline` and polled, so the task wakes then.
fn wake_at(
    timer: &mut Option<Pin<Box<Sleep>>>,
    deadline: Instant,
    cx: &mut Context<'_>,
) -> Poll<()> {
    let timer = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
    if timer.deadline() != deadline {
        timer.as_mut().reset(deadline);
    }
    timer.as_mut().poll(cx)
}

/// Each call to the stream under the wire, clear or under TLS.
macro_rules! on_wire {
    ($wire:expr, $stream:ident => $call:expr) => {
        match $wire {
            Wire::Clear($stream) => $call,
            Wire::Tls($stream) => $call,
        }
    };
}

impl AsyncRead for Wire {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        on_wire!(self.get_mut(), stream => Pin::new(stream).poll_read(cx, buf))
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        on_wire!(self.get_mut(), stream => Pin::new(stream).poll_write(cx, buf))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        on_wire!(self.get_mut(), stream => Pin::new(stream).poll_write_vectored(cx, bufs))
    }

    fn is_write_vectored(&self) -> bool {
        on_wire!(self, stream => stream.is_write_vectored())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        on_wire!(self.get_mut(), stream => Pin::new(stream).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        on_wire!(self.get_mut(), stream => Pin::new(stream).poll_shutdown(cx))
    }
}

impl AsyncRead for Socket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let now = Instant::now();
        let clock = this.read_deadline();
        if let Some((deadline, expiry)) = clock
            && deadline <= now
        {
            return this.expire(expiry, cx);
        }
        let filled = buf.filled().len();
        match this.stream()?.poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let read = &buf.filled()[filled..];
                if read.is_empty() {
                    return Poll::Ready(Ok(()));
                }
                if let Some(peer) = this.clear_peer.take()
                    && read.first() == Some(&TLS_HANDSHAKE)
                {
                    // hyper reads a failed first read as the client gone,
                    // so the refusal is logged here.
                    crate::log::error(
                        format!(
                            "makala: {peer}: a TLS handshake came to the clear port, which \
                             serves HTTP without TLS; the connection is closed unanswered"
                        )
                        .as_bytes(),
                    );
                    return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
                }
                this.watch.arrived(read, now);
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                let Some((deadline, expiry)) = clock else {
                    this.watch.waits(cx.waker());
                    return Poll::Pending;
                };
                if wake_at(&mut this.read_timer, deadline, cx).is_ready() {
                    this.expire(expiry, cx)
                } else {
                    Poll::Pending
                }
            }
            failed @ Poll::Ready(Err(_)) => failed,
        }
    }
}

impl AsyncWrite for Socket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = this.stream()?.poll_write(cx, buf);
        let response = this.watch.responses();
        this.writes
            .wrote(polled, this.limits.min_response_rate, response, cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = this.stream()?.poll_write_vectored(cx, bufs);
        let response = this.watch.responses();
        this.writes
            .wrote(polled, this.limits.min_response_rate, response, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(AsyncWrite::is_write_vectored)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().stream()?.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().stream()?.poll_shutdown(cx)
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let (Some(stream), Ok(runtime)) =
            (self.stream.take(), tokio::runtime::Handle::try_current())
        {
            runtime.spawn(linger(stream, self.slot.take()));
        }
    }
}

/// The write side shut, under TLS after its `close_notify`, then what the
/// client still sends read and dropped until it closes, and the socket
/// closed; all of it within [`LINGER`].
async fn linger(mut wire: Wire, slot: Option<OwnedSemaphorePermit>) {
    let lingered = async {
        std::future::poll_fn(|cx| Pin::new(&mut wire).poll_shutdown(cx))
            .await
            .ok();
        let stream = wire.into_tcp();
        let mut dropped = [0u8; 4096];
        loop {
            if stream.readable().await.is_err() {
                return;
            }
            match stream.try_read(&mut dropped) {
                Ok(0) => return,
                Err(e) if e.kind() != io::ErrorKind::WouldBlock => return,
                _ => {}
            }
        }
    };
    tokio::time::timeout(LINGER, lingered).await.ok();
    drop(slot);
}
