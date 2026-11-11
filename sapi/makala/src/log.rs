//! The server's two logs (#2345): the access log, a line for each
//! response as it ends, and the error log, what failed, a request's panic
//! among it, and the entries php logs where `error_log` names no file.
//!
//! The access line carries what Bandit's request-stop event does
//! (research/production-server.r2.md §4.8, M17): when the response ended,
//! in UTC, the peer, the request line, the status, the body's bytes in and
//! out, and how long the request took from its head to its response's end:
//!
//! ```text
//! 2026-10-10T08:22:40.123Z 127.0.0.1:50123 "GET /a.php?x=1 HTTP/1.1" 200 0 26 1.234ms
//! ```
//!
//! An error line is the time it was written for and the entry, whole.
//!
//! No request writes a line of its own into either log (#4355). The
//! request line cannot hold a `\r` or `\n`: the parser refuses a head
//! with one before its line is begun, and `Method` and `Uri` hold
//! neither, so a `%0D%0A` is written as the client sent it. An entry
//! php hands over is written whole, a message over several lines among
//! them, the file it names escaped by the engine; and a refusal escapes
//! the path it names, which the request's decoded path can carry
//! ([`php_sapi::log_field`]). A file the log thread makes is its
//! owner's alone, `0600`.
//!
//! No line is written where it is made. Each goes onto one bounded queue
//! that one log thread empties into the destinations, so a request thread
//! or the connection side never waits on a disk or a pipe, as the BEAM's
//! servers keep an event's handler, which runs in the request's own
//! process, from anything that blocks (research/production-server.r2.md
//! §4.8). Where the queue is full the line is
//! dropped and counted, never waited for, and the log thread writes the
//! count into the error log once it has room: a slow destination loses
//! lines and says how many, and never stalls PHP. The queue is std's
//! `sync_channel`, whose `try_send` refuses at once when full, so no
//! package is added; one thread and not a task of the connection side,
//! since a write to a file blocks and nothing that blocks runs there (M4).
//! The thread opens the destinations itself, so a FIFO nobody reads holds
//! only the log thread, even at its open.
//!
//! What php logs while it starts, before the log thread, goes straight to
//! stderr, so a startup php refuses, which ends the process at once, is
//! never lost in the queue.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyper::body::Incoming;
use hyper::{Method, Request, StatusCode, Uri, Version};
use php_sapi::HostThread;

/// Where one log goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// The process's stderr, the error log's where the line names none.
    Stderr,
    /// A file, created where it is not and appended to, which the log
    /// thread opens.
    File(PathBuf),
}

/// The log thread's stack: it formats nothing and writes lines.
const STACK: usize = 256 << 10;

/// How long the process's end waits for the log thread to write what is
/// queued ([`settle`]), a destination that never takes it losing it.
pub(crate) const SETTLE: Duration = Duration::from_secs(1);

/// One queued line, or the ask to say once every line before it is
/// written.
enum Queued {
    Access(Vec<u8>),
    Error(Vec<u8>),
    Settle(SyncSender<()>),
}

/// The queue's sending end, and whether an access log is kept.
struct Logs {
    queue: SyncSender<Queued>,
    access: bool,
}

/// The logs, once [`start`] has started their thread; until then an
/// error line goes straight to stderr and an access line nowhere.
static LOGS: OnceLock<Logs> = OnceLock::new();

/// The lines the queue refused since the log thread last said how many.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// The log thread, writing the access log to `access` where the line
/// names one and the error log to `errors`, with at most `lines` waiting
/// for it; once a process.
///
/// # Errors
///
/// The error the system refused the thread with, or the logs started
/// before.
pub(crate) fn start(
    access: Option<Destination>,
    errors: Destination,
    lines: usize,
) -> io::Result<HostThread> {
    let (queue, queued) = sync_channel(lines);
    let keeps_access = access.is_some();
    LOGS.set(Logs {
        queue,
        access: keeps_access,
    })
    .map_err(|_| io::Error::other("the logs were started already"))?;
    php_sapi::host_thread("makala-log", STACK, move || {
        Destinations::new(access, errors).write_out(&queued);
    })
}

/// What the access log says of one request, filled in as its response
/// goes and written when it has ended ([`Access::ended`]).
pub(crate) struct Access {
    pub(crate) peer: SocketAddr,
    pub(crate) method: Method,
    pub(crate) target: Uri,
    pub(crate) version: Version,
    /// The response's code, as its head was sent.
    pub(crate) status: StatusCode,
    /// When the head was parsed.
    pub(crate) started: Instant,
    /// The body's bytes read, as the connection side's pump counts them.
    pub(crate) body_in: Arc<AtomicU64>,
    /// The response body's bytes handed to the connection.
    pub(crate) body_out: u64,
}

impl Access {
    /// The record of a request whose head was just parsed, where an
    /// access log is kept.
    pub(crate) fn begun(peer: SocketAddr, request: &Request<Incoming>) -> Option<Access> {
        LOGS.get().filter(|logs| logs.access)?;
        Some(Access {
            peer,
            method: request.method().clone(),
            target: request.uri().clone(),
            version: request.version(),
            status: StatusCode::OK,
            started: Instant::now(),
            body_in: Arc::new(AtomicU64::new(0)),
            body_out: 0,
        })
    }

    /// The response has ended, whole or cut: its line queued, the time,
    /// the peer, the request line, the status, the bytes in and out and
    /// the time it took, as the line at this file's head has it.
    pub(crate) fn ended(&self) {
        let Some(logs) = LOGS.get() else {
            return;
        };
        let line = format!(
            "{} {} \"{} {} {:?}\" {} {} {} {:.3}ms\n",
            now(),
            self.peer,
            self.method,
            self.target,
            self.version,
            self.status.as_u16(),
            self.body_in.load(Ordering::Relaxed),
            self.body_out,
            self.started.elapsed().as_secs_f64() * 1000.0
        );
        queue(logs, Queued::Access(line.into_bytes()));
    }
}

/// One error entry, whole, as php's `log_message` hands it: the time,
/// then the entry, which may hold newlines, then one.
pub(crate) fn error(entry: &[u8]) {
    let mut line = format!("{} ", now()).into_bytes();
    line.extend_from_slice(entry);
    line.push(b'\n');
    match LOGS.get() {
        Some(logs) => queue(logs, Queued::Error(line)),
        None => {
            io::stderr().lock().write_all(&line).ok();
        }
    }
}

/// `queued` onto the queue where it has room, and counted as dropped
/// where it has none.
fn queue(logs: &Logs, queued: Queued) {
    if logs.queue.try_send(queued).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Waits until the log thread has written every line queued before, or
/// `limit` has passed: what the process's end does last. The ask waits
/// for room in a full queue, which only the end may do.
pub(crate) fn settle(limit: Duration) {
    let Some(logs) = LOGS.get() else {
        return;
    };
    let deadline = Instant::now() + limit;
    let (done, written) = sync_channel(1);
    let mut ask = Queued::Settle(done);
    loop {
        match logs.queue.try_send(ask) {
            Ok(()) => break,
            Err(TrySendError::Full(again)) if Instant::now() < deadline => {
                ask = again;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
    written
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok();
}

/// The log thread's destinations, each opened at its first line.
struct Destinations {
    access: Option<Log>,
    errors: Log,
}

/// One log: its destination, opened at its first line.
struct Log {
    to: Destination,
    out: Out,
}

/// What writes to a destination.
enum Out {
    /// Nothing yet: no line has come for it.
    Unopened,
    Open(Box<dyn Write + Send>),
    /// It would not open, which was said once on stderr; its lines go
    /// nowhere.
    Refused,
}

impl Log {
    fn new(to: Destination) -> Log {
        Log {
            to,
            out: Out::Unopened,
        }
    }

    fn write(&mut self, line: &[u8]) {
        if let Out::Unopened = self.out {
            self.out = open(&self.to);
        }
        if let Out::Open(out) = &mut self.out {
            out.write_all(line).ok();
        }
    }

    fn flush(&mut self) {
        if let Out::Open(out) = &mut self.out {
            out.flush().ok();
        }
    }
}

/// `to` opened for appending, or refused, said on stderr, where it will
/// not open. A file it makes is its owner's alone, `0600` whatever the
/// umask, since a line names a client's address and the path and query
/// it asked for (#4355); a file that stands already keeps its permissions.
fn open(to: &Destination) -> Out {
    match to {
        Destination::Stderr => Out::Open(Box::new(io::stderr())),
        Destination::File(path) => match File::options()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => Out::Open(Box::new(BufWriter::new(file))),
            Err(e) => {
                let refusal = format!("makala: cannot open the log {}: {e}\n", path.display());
                io::stderr().lock().write_all(refusal.as_bytes()).ok();
                Out::Refused
            }
        },
    }
}

impl Destinations {
    fn new(access: Option<Destination>, errors: Destination) -> Destinations {
        Destinations {
            access: access.map(Log::new),
            errors: Log::new(errors),
        }
    }

    /// Each queued line written as it comes, the destinations flushed
    /// whenever the queue is empty, and the count of lines dropped
    /// meanwhile written into the error log then, until the queue has no
    /// sender.
    fn write_out(mut self, queued: &Receiver<Queued>) {
        loop {
            let next = match queued.try_recv() {
                Ok(next) => next,
                Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {
                    self.idle();
                    match queued.recv() {
                        Ok(next) => next,
                        Err(_) => return,
                    }
                }
            };
            match next {
                Queued::Access(line) => {
                    if let Some(access) = &mut self.access {
                        access.write(&line);
                    }
                }
                Queued::Error(line) => self.errors.write(&line),
                Queued::Settle(done) => {
                    self.idle();
                    done.send(()).ok();
                }
            }
        }
    }

    /// The queue is empty: what was dropped said, and every destination
    /// flushed.
    fn idle(&mut self) {
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let line = format!(
                "{} makala: {dropped} log lines were dropped, the queue full while a destination was slower than the server\n",
                now()
            );
            self.errors.write(line.as_bytes());
        }
        if let Some(access) = &mut self.access {
            access.flush();
        }
        self.errors.flush();
    }
}

/// The time now in UTC, RFC 3339 to the millisecond:
/// `2026-10-10T08:22:40.123Z`.
fn now() -> String {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let at = php_proto::http::civil(since.as_secs());
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        at.year,
        at.month,
        at.day,
        at.hour,
        at.minute,
        at.second,
        since.subsec_millis()
    )
}
