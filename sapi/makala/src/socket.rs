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
//!   socket reads while the connection idles: a head whose start came
//!   in with the request before it, sent before that request's response
//!   ended, is timed from the next read after that end, and by the
//!   keep-alive clock until then.
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
//! let go outside the runtime, which only the process's end does, closes
//! at once. A lingering socket holds its descriptor for that second
//! beyond its connection.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{Instant, Sleep};

use crate::options::{Limits, MinDataRate};

/// How long a closing socket reads and drops what its client still
/// sends: Cowboy's `linger_timeout`, 1000 ms.
const LINGER: Duration = Duration::from_secs(1);

/// What a connection waits on, which says which clock of its socket
/// runs: set by its requests as each begins and as its response ends, and
/// by its socket as a head's first byte arrives.
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
            }),
        }
    }

    /// A request's head is parsed and the request in hand.
    pub(crate) fn request_began(&self) {
        self.held().phase = Phase::Busy;
    }

    /// A response has ended, and the connection waits for the next
    /// request: its read is woken to set the clock.
    pub(crate) fn response_ended(&self) {
        let reader = {
            let mut held = self.held();
            held.phase = Phase::Idle(Instant::now());
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

    /// Bytes arrived at `now`: a head's first where the connection idled.
    fn arrived(&self, now: Instant) {
        let mut held = self.held();
        if let Phase::Idle(_) = held.phase {
            held.phase = Phase::Head(now);
        }
    }
}

/// The socket of one connection, with its clocks, which lingers when it
/// is let go.
pub(crate) struct Socket {
    /// Taken only when the socket is let go.
    stream: Option<TcpStream>,
    watch: Arc<Watch>,
    limits: Limits,
    /// The keep-alive idle or header timeout's wake-up, while a read waits.
    read_timer: Option<Pin<Box<Sleep>>>,
    writes: WriteClock,
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
    pub(crate) fn new(stream: TcpStream, watch: Arc<Watch>, limits: Limits) -> Socket {
        Socket {
            stream: Some(stream),
            watch,
            limits,
            read_timer: None,
            writes: WriteClock::default(),
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
    /// socket, past hyper's buffer, which holds nothing then unless a
    /// write has stalled, when it is left unsent.
    fn expire(&mut self, expiry: Expiry) -> Poll<io::Result<()>> {
        match expiry {
            Expiry::Idle => Poll::Ready(Ok(())),
            Expiry::Head => {
                if let (Some(stream), false) = (&self.stream, self.writes.stalled) {
                    let answer = format!(
                        "HTTP/1.1 408 Request Timeout\r\nconnection: close\r\n\
                         content-length: 0\r\ndate: {}\r\n\r\n",
                        httpdate::fmt_http_date(SystemTime::now())
                    );
                    stream.try_write(answer.as_bytes()).ok();
                }
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the head did not arrive within the header timeout",
                )))
            }
        }
    }

    /// The stream, which is gone only once the socket is let go.
    fn stream(&mut self) -> io::Result<Pin<&mut TcpStream>> {
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
            return this.expire(expiry);
        }
        let filled = buf.filled().len();
        match this.stream()?.poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > filled {
                    this.watch.arrived(now);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                let Some((deadline, expiry)) = clock else {
                    this.watch.waits(cx.waker());
                    return Poll::Pending;
                };
                if wake_at(&mut this.read_timer, deadline, cx).is_ready() {
                    this.expire(expiry)
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
            .is_some_and(TcpStream::is_write_vectored)
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
            runtime.spawn(linger(stream));
        }
    }
}

/// The write side shut, then what the client still sends read and
/// dropped until it closes or [`LINGER`] passes, and the socket closed.
async fn linger(mut stream: TcpStream) {
    std::future::poll_fn(|cx| Pin::new(&mut stream).poll_shutdown(cx))
        .await
        .ok();
    let drained = async {
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
    tokio::time::timeout(LINGER, drained).await.ok();
}
