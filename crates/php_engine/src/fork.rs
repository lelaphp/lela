//! The engine's one home for `fork` (#1807, as #1779 ruled): what the
//! engine decides around `php_process`'s one `fork`, which nothing else
//! calls. php forks with one thread in the process, since its timer
//! is an interval timer and its server serves on one thread; the engine
//! runs threads of its own (its timer, ADR-0010's Threads), so
//! before it forks it joins every one of them, and it forks only where
//! nothing else is inside the engine: POSIX lets the child of a process
//! with other threads call only async-signal-safe functions, and a child
//! that goes on running PHP code calls everything.
//!
//! The one facility is [`Inside`]: every request in flight and every
//! thread the engine starts holds one while it stands, so the process
//! knows how many things stand inside the engine, and [`Alone`] is the one
//! type that forks, which the engine can hold only while that number is
//! the caller's own. A host's thread that serves requests starts here too,
//! through [`host_thread`]. A thread started any other way than [`spawn`] is
//! one no fork waits for, and a thread started through it that a fork
//! does not join first leaves the fork refused rather than a child that
//! hangs. What the engine cannot see is a host's own thread that holds
//! no request, such as the accept loop `php -S` kept beside its
//! connections' threads; no host that forks or carries pcntl may keep
//! one, which is why `php -S` serves in php's one-threaded event loop
//! (#1884).
//!
//! Two ways in, as the ruling has them, and one way through. pcntl asks
//! through the request,
//! [`Request::fork`](crate::api::request::Request::fork), and in the child
//! what php's `php_child_init` restores is restored. A host asks through
//! the host face, [`fork`], with no request in flight, as php-fpm's master
//! makes a child or daemonizes. Both fork through `fork_as`, which joins
//! every engine's timer thread first, parked between requests as it is,
//! and starts each again in the parent over the deadlines it had.
//!
//! What php's `php_child_init` (`Z/main/main.c:1865`) restores in the
//! child is two things: `zend_max_execution_timer_init`, a timer of the
//! child's own where a build times with POSIX timers, which here is the
//! engine's timer forgetting every deadline, as a child inherits no
//! interval timer; and `refresh_memory_manager`, a fresh key for the
//! heap's shadow pointers, which this engine's heap does not have and no
//! program can observe.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

pub use php_process::{Forked, Pid};

use crate::interrupt::Timers;

/// How many things stand inside the engine in this process, each an
/// [`Inside`], with [`FORKING`] set while an [`Alone`] is held.
static INSIDE: AtomicUsize = AtomicUsize::new(0);

/// The bit of [`INSIDE`] an [`Alone`] holds: nothing enters while it is
/// set, so nothing starts between the reading and the fork.
const FORKING: usize = 1 << (usize::BITS - 1);

/// `EBUSY`, the same number on every platform the engine builds for: the
/// error a fork is refused with while something else runs inside the
/// engine, which `pcntl_fork` renders as php renders an `errno` it has
/// no text of its own for.
const EBUSY: i32 = 16;

/// One thing running inside the engine: a request in flight, or a thread
/// the engine started, from before it starts to after it is joined.
/// While one stands beside the caller's own, nothing forks.
#[derive(Debug)]
pub(crate) struct Inside(());

impl Inside {
    /// Entered, once no fork is under way: a fork holds the bit from its
    /// reading to its return, which takes no longer than `fork(2)`.
    pub(crate) fn enter() -> Inside {
        while INSIDE
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |inside| {
                (inside & FORKING == 0).then_some(inside + 1)
            })
            .is_err()
        {
            thread::yield_now();
        }
        Inside(())
    }
}

impl Drop for Inside {
    /// Left. In a forked child too, whose copy of the number counted it.
    fn drop(&mut self) {
        INSIDE.fetch_sub(1, Ordering::Release);
    }
}

/// A thread the engine started, which a fork waits for: [`Inside`] from
/// before the thread exists to after it is joined.
#[derive(Debug)]
pub(crate) struct Thread {
    handle: JoinHandle<()>,
    _inside: Inside,
}

/// `body` on a thread of its own named `name`, counted inside the engine
/// until it is joined; `None` where the system refuses a thread.
pub(crate) fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Option<Thread> {
    start(thread::Builder::new().name(name.to_owned()), body).ok()
}

/// `body` on the thread `builder` describes, counted inside the engine
/// from before it exists to after it is joined.
fn start(builder: thread::Builder, body: impl FnOnce() + Send + 'static) -> io::Result<Thread> {
    let inside = Inside::enter();
    #[expect(
        clippy::disallowed_methods,
        reason = "the home: every thread of the engine's process starts here, counted until it is joined"
    )]
    let handle = builder.spawn(body)?;
    Ok(Thread {
        handle,
        _inside: inside,
    })
}

/// A thread a host serves requests on, each through its own
/// [`Engine::serve`](crate::Engine::serve), as a server that keeps a pool
/// of request threads does (design §5.14): started through the engine's
/// one home, so it stands inside the engine from before it exists to
/// after it is joined, as a thread the engine starts does, and a fork
/// asked while it stands is refused.
#[derive(Debug)]
pub struct HostThread(Thread);

impl HostThread {
    /// Joined, once the host has told it to end.
    pub fn join(self) {
        self.0.join();
    }
}

/// `body` on a host's thread named `name` with a stack of `stack_size`
/// bytes: the stack a request served on it has, whose bounds the engine
/// reads for php's "Maximum call stack size" (design §12), so a host
/// sizes it as the process it stands in for serves on.
///
/// # Errors
///
/// The error the system refused the thread with.
pub fn host_thread(
    name: &str,
    stack_size: usize,
    body: impl FnOnce() + Send + 'static,
) -> io::Result<HostThread> {
    let builder = thread::Builder::new()
        .name(name.to_owned())
        .stack_size(stack_size);
    start(builder, body).map(HostThread)
}

impl Thread {
    /// Joined, once its owner has told it to end.
    pub(crate) fn join(self) {
        // A thread that panicked has ended all the same.
        self.handle.join().ok();
    }

    /// Forgotten, in a process forked from the one that started it, which
    /// only a fork outside this home makes: the thread is not there, so
    /// there is nothing to join, and the child's copy of the number
    /// counted it.
    pub(crate) fn forget(self) {
        #[expect(
            clippy::mem_forget,
            reason = "a forked child's copy of a thread that is not there: there is nothing to join"
        )]
        std::mem::forget(self.handle);
    }

    /// The thread, for a test that tells one from another.
    #[cfg(test)]
    pub(crate) fn thread(&self) -> &thread::Thread {
        self.handle.thread()
    }
}

/// The process with nothing inside the engine but the caller: the one
/// type that forks, and the only caller of `php_process::fork`. It is
/// held from the reading to the fork's return, so nothing enters between.
#[derive(Debug)]
pub(crate) struct Alone(());

/// Who asks for a fork, and so what may stand inside the engine beside
/// it once the engine's timer threads are joined: the request it is, or
/// nothing at all for a host, which forks with no request in flight.
#[derive(Debug, Clone, Copy)]
enum Asker {
    Host = 0,
    Request = 1,
}

impl Alone {
    /// Refused with `EBUSY`, before any thread is joined, where more
    /// stands inside the engine than the asker and the `parked` timer
    /// threads the fork would join: so a fork refused leaves every
    /// engine's timer serving as it was. [`Alone::hold`] reads the number
    /// again once they are joined, which is what decides.
    fn refused_beside(asker: Asker, parked: usize) -> io::Result<()> {
        let inside = INSIDE.load(Ordering::Acquire);
        if inside & FORKING == 0 && inside > asker as usize + parked {
            return Err(io::Error::from_raw_os_error(EBUSY));
        }
        Ok(())
    }

    /// Held, where what stands inside the engine is the asker alone.
    /// Refused with `EBUSY` where anything else does, a request another
    /// thread serves or a thread the engine started; a second fork under
    /// way is waited out.
    fn hold(asker: Asker) -> io::Result<Alone> {
        let own = asker as usize;
        loop {
            match INSIDE.compare_exchange_weak(
                own,
                own | FORKING,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(Alone(())),
                Err(inside) if inside & FORKING != 0 || inside == own => thread::yield_now(),
                Err(_) => return Err(io::Error::from_raw_os_error(EBUSY)),
            }
        }
    }

    /// `php_process::fork`: in both processes the hold is let go once it
    /// returns, the child's copy of the number being the caller's own.
    fn fork(self) -> io::Result<Forked> {
        #[expect(
            clippy::disallowed_methods,
            reason = "the home: Alone is php_process::fork's one caller"
        )]
        let forked = php_process::fork();
        drop(self);
        forked
    }
}

impl Drop for Alone {
    fn drop(&mut self) {
        INSIDE.fetch_and(!FORKING, Ordering::Release);
    }
}

/// Every fork, a request's and a host's: the timer thread of every engine
/// in the process joined (ADR-0010, Threads), each of which stays parked
/// between requests while its engine lives (#2541), [`Alone`] held for
/// `asker` and forked, and each timer started again after where it has
/// a deadline, in the process that asked, or where the fork was refused
/// or failed. A child drops its copies unstarted, which it forgets as it
/// reaches them (see [`Timer`](crate::interrupt::Timer)).
fn fork_as(asker: Asker) -> io::Result<Forked> {
    let timers = Timers::live();
    Alone::refused_beside(asker, timers.threads())?;
    let stopped = timers.stop();
    let forked = Alone::hold(asker).and_then(Alone::fork);
    if !matches!(forked, Ok(Forked::Child)) {
        stopped.resume();
    }
    forked
}

/// The fork `Request::fork` asks for, with that request alone inside
/// the engine beside the timers' threads it joins.
pub(crate) fn fork_request() -> io::Result<Forked> {
    fork_as(Asker::Request)
}

/// `fork(2)` for a host (#1807): the process copied with no request in
/// flight, as php-fpm's master makes a child (`fpm_children_make`) and
/// daemonizes (`fpm_unix_init_main`), returning in both as
/// [`Forked::Parent`] with the child's id and as [`Forked::Child`]. What
/// php's `php_child_init` restores in the child is the request's, and no
/// request is in flight, so the child goes on as the parent would.
///
/// The engine's timer thread, which stays parked between requests once a
/// request has set a deadline, startup's and engine end's among them, is
/// joined first and started again in the parent where it has a deadline,
/// so a host's fork after a timed request is not refused (#2541).
///
/// # Errors
///
/// `EBUSY` where a request is in flight or a thread the engine started
/// other than its timer's stands, as on a host that serves on threads,
/// which no host that forks does; otherwise the OS error `fork(2)` failed
/// with, and no process was made.
pub fn fork() -> io::Result<Forked> {
    fork_as(Asker::Host)
}

#[cfg(test)]
mod tests {
    use super::{EBUSY, FORKING, INSIDE, Inside, fork, host_thread, spawn};
    use std::sync::atomic::Ordering;

    /// A thread started through the home is inside the engine while it
    /// waits; the harness's other tests move the number too, so what is
    /// read is a floor.
    #[test]
    fn a_thread_is_inside_while_it_stands() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let Some(thread) = spawn("php-test", move || {
            rx.recv().ok();
        }) else {
            return;
        };
        assert!(INSIDE.load(Ordering::Relaxed) & !FORKING >= 1);
        drop(tx);
        thread.join();
    }

    /// A host's thread stands inside the engine while it runs, so a fork
    /// asked meanwhile is refused.
    #[test]
    fn a_host_thread_is_inside_while_it_stands() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let started = host_thread("php-host", 3 << 20, move || {
            rx.recv().ok();
        });
        assert!(
            started.is_ok(),
            "the system refused the thread: {started:?}"
        );
        let Ok(thread) = started else {
            return;
        };
        assert!(INSIDE.load(Ordering::Relaxed) & !FORKING >= 1);
        assert_eq!(fork().err().and_then(|e| e.raw_os_error()), Some(EBUSY));
        drop(tx);
        thread.join();
    }

    /// A host's fork is refused, with no process made, while anything
    /// runs inside the engine: here what the test holds, beside whatever
    /// requests the harness's other tests serve.
    #[test]
    fn a_hosts_fork_is_refused_while_anything_is_inside() {
        let inside = Inside::enter();
        let refused = fork().err().and_then(|e| e.raw_os_error());
        drop(inside);
        assert_eq!(refused, Some(EBUSY));
        assert_eq!(INSIDE.load(Ordering::Relaxed) & FORKING, 0);
    }
}
