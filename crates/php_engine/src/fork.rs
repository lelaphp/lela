//! The engine's one home for `fork` (#1807, as #1779 ruled): what the
//! engine decides around `php_process`'s one `fork`, which nothing else
//! calls. php forks with one thread in the process, since its timer
//! is an interval timer and its server serves on one thread; the engine
//! runs threads of its own (the request's timer, ADR-0010's Threads), so
//! before it forks it joins every one of them, and it forks only where
//! nothing else is inside the engine: POSIX lets the child of a process
//! with other threads call only async-signal-safe functions, and a child
//! that goes on running PHP code calls everything.
//!
//! The one facility is [`Inside`]: every request in flight and every
//! thread the engine starts holds one while it stands, so the process
//! knows how many things stand inside the engine, and [`Alone`] is the one
//! type that forks, which the engine can hold only while that number is
//! the caller's own. A thread started any other way than [`spawn`] is
//! one no fork waits for, and a thread started through it that a fork
//! does not join first leaves the fork refused rather than a child that
//! hangs. What the engine cannot see is a host's own thread that holds
//! no request, such as the accept loop `php -S` kept beside its
//! connections' threads; no host that forks or carries pcntl may keep
//! one, which is why `php -S` serves in php's one-threaded event loop
//! (#1884).
//!
//! Two ways in, as the ruling has them. pcntl asks through the request,
//! [`Request::fork`](crate::api::request::Request::fork): the request's
//! own threads joined first and started again in the parent over the
//! deadlines they had, and in the child what php's `php_child_init`
//! restores. A host asks through the host face, [`fork`], with no request
//! in flight, as php-fpm's master makes a child or daemonizes.
//!
//! What php's `php_child_init` (`Z/main/main.c:1865`) restores in the
//! child is two things: `zend_max_execution_timer_init`, a timer of the
//! child's own where a build times with POSIX timers, which here is the
//! request's timer forgetting its deadlines, as a child inherits no
//! interval timer; and `refresh_memory_manager`, a fresh key for the
//! heap's shadow pointers, which this engine's heap does not have and no
//! program can observe.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

pub use php_process::{Forked, Pid};

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
    let inside = Inside::enter();
    let handle = thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .ok()?;
    Some(Thread {
        handle,
        _inside: inside,
    })
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
/// it: the request it is, once its own threads are joined, or nothing at
/// all for a host.
#[derive(Debug, Clone, Copy)]
enum Asker {
    Host = 0,
    Request = 1,
}

impl Alone {
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

/// [`Alone`] held for a request and forked: what `Request::fork` calls
/// once the request's own threads are joined, so what runs inside the
/// engine is that request alone.
pub(crate) fn fork_request() -> io::Result<Forked> {
    Alone::hold(Asker::Request)?.fork()
}

/// `fork(2)` for a host (#1807): the process copied with no request in
/// flight, as php-fpm's master makes a child (`fpm_children_make`) and
/// daemonizes (`fpm_unix_init_main`), returning in both as
/// [`Forked::Parent`] with the child's id and as [`Forked::Child`]. What
/// php's `php_child_init` restores in the child is the request's, and no
/// request is in flight, so the child goes on as the parent would.
///
/// # Errors
///
/// `EBUSY` where a request is in flight or a thread the engine started
/// stands, as on a host that serves on threads, which no host that
/// forks does; otherwise the OS error `fork(2)` failed with, and no
/// process was made.
pub fn fork() -> io::Result<Forked> {
    Alone::hold(Asker::Host)?.fork()
}

#[cfg(test)]
mod tests {
    use super::{EBUSY, FORKING, INSIDE, Inside, fork, spawn};
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
