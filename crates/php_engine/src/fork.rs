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
//! through [`host_thread`], and so does the thread a host moves its
//! `main` onto, through [`main_thread`], which is the process's main
//! thread to a fork and so never beside one it asks. A thread started
//! any other way than [`spawn`] is one no fork waits for, and a thread
//! started through it that a fork
//! does not join first leaves the fork refused rather than a child that
//! hangs. What the engine cannot see is a host's own thread that holds
//! no request, such as the accept loop `php -S` kept beside its
//! connections' threads; no host that forks or carries pcntl may keep
//! one, which is why `php -S` serves in php's one-threaded event loop
//! (#1884).
//!
//! Two ways in, as the ruling has them, and one way through. pcntl asks
//! through the request,
//! [`Limits::fork`](crate::api::request::Limits::fork). A host asks through
//! the host face, [`fork`], with no request in flight, as php-fpm's master
//! makes a child or daemonizes. Both fork through `fork_as`, which joins
//! every engine's timer thread first, parked between requests as it is,
//! and starts each again in the parent over the deadlines it had, and in
//! the child restores what php's `php_child_init` restores, which php
//! calls in the child of pcntl's forks, opcache's, php-fpm's, the CGI's,
//! the CLI server's and every other host that forks.
//! Registering a timer, a realpath cache or an [`Inherited`] entry counts
//! as standing inside, so no fork copies the list of them held.
//!
//! What php's `php_child_init` (`Z/main/main.c:1897`) restores in the
//! child is three things. `refresh_memory_manager`, a fresh key for the
//! heap's shadow pointers, which this engine's heap does not have and no
//! program can observe. `zend_max_execution_timer_init`, a timer of the
//! child's own where a build times with POSIX timers, which here is the
//! engine's timer forgetting every deadline, as a child inherits no
//! interval timer. And `php_clear_stat_cache(true, NULL, 0)`, which here
//! is every engine's realpath cache emptied, the engine's own, which the
//! fork holds locked across the copy ([`RealpathCaches`]); the stat cache
//! is the request's, above the engine in `php_streams`, and is emptied by
//! the native that asked for the fork, as `pcntl_fork` does.
//!
//! Beside what php restores, a child makes its own what it would
//! otherwise share with its parent ([`Inherited`]): the compiled tier's
//! code arena on Linux, whose pages one shared mapping of a `memfd`
//! holds, so the two processes would write each other's code
//! (`jit-design.md` §4.4). php keeps its one buffer shared by every
//! process on one opcache segment, each allocation under
//! `zend_shared_alloc_lock`; this engine shares an arena among one
//! process's threads alone.

use std::convert::Infallible;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError, Weak};
use std::thread::{self, JoinHandle, ThreadId};

pub use php_process::{Forked, Pid};

use crate::interrupt::Timers;
use crate::virtual_cwd::RealpathCaches;

/// How many things stand inside the engine in this process, each an
/// [`Inside`], with [`FORKING`] set while an [`Alone`] is held.
static INSIDE: AtomicUsize = AtomicUsize::new(0);

/// The bit of [`INSIDE`] an [`Alone`] holds: nothing enters while it is
/// set, so nothing starts between the first reading and the fork.
const FORKING: usize = 1 << (usize::BITS - 1);

/// `EBUSY`, the same number on every platform the engine builds for: the
/// error a fork is refused with while something else runs inside the
/// engine, which `pcntl_fork` renders as php renders an `errno` it has
/// no text of its own for.
pub(crate) const EBUSY: i32 = 16;

/// One thing running inside the engine: a request in flight, or a thread
/// the engine started, from before it starts to after it is joined.
/// While one stands beside the caller's own, nothing forks.
#[derive(Debug)]
pub(crate) struct Inside(());

impl Inside {
    /// Entered, once no fork is under way: a fork holds the bit from its
    /// reading to its return, which takes the joins of the parked threads,
    /// a [`Worker`]'s once the job under way is done, and `fork(2)`.
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
/// before the thread exists to after it is joined; `T` is what its body
/// answers.
#[derive(Debug)]
pub(crate) struct Thread<T = ()> {
    handle: JoinHandle<T>,
    _inside: Inside,
}

/// `body` over `input` on a thread apart named `name`, with a stack of
/// `stack_size` bytes, counted inside the engine while it stands, and
/// joined before this returns: what `body` answered, or the panic it
/// ended in, for the caller to resume. A thread apart may take ids other
/// than the process's, and a thread inherits its creator's on Linux and
/// on macOS (xnu's `thread_create` copies the creating thread's
/// credential), so the caller starts what a request on it could start
/// before it, the engine's timer thread ([`crate::interrupt::Timer::stand`]),
/// and the thread's ids end with it. `input` is handed over once the
/// thread stands, so where the system refuses the thread it comes back
/// with the error, and `body` never runs.
pub(crate) fn run_apart<I: Send + 'static, T: Send + 'static>(
    name: &str,
    stack_size: usize,
    input: I,
    body: impl FnOnce(I) -> T + Send + 'static,
) -> Result<thread::Result<T>, (io::Error, I)> {
    // A rendezvous: the send returns once the thread holds `input`, and
    // hands it back where the thread ended without taking it.
    let (hand, handed) = std::sync::mpsc::sync_channel::<I>(0);
    let builder = thread::Builder::new()
        .name(name.to_owned())
        .stack_size(stack_size);
    let thread = match spawn(builder, move || handed.recv().ok().map(body)) {
        Ok(thread) => thread,
        Err(refused) => return Err((refused, input)),
    };
    let sent = hand.send(input);
    let ended = thread.handle.join();
    match (sent, ended) {
        (Err(std::sync::mpsc::SendError(input)), _) => Err((
            io::Error::other("the thread ended before it was handed its work"),
            input,
        )),
        (Ok(()), Ok(Some(answer))) => Ok(Ok(answer)),
        (Ok(()), Err(panicked)) => Ok(Err(panicked)),
        // A send that returned was received, so the body ran and
        // answered: no thread ends here, and one that did ended abnormally.
        (Ok(()), Ok(None)) => Ok(Err(Box::new(
            "the thread took its work and ended without an answer",
        ))),
    }
}

/// `body` on the thread `builder` describes, counted inside the engine
/// from before it exists to after it is joined: the one home every thread
/// of the engine's process starts through, the engine's own (its timer,
/// a [`Worker`]'s, a thread apart) and a host's ([`host_thread`],
/// [`main_thread`]) alike.
pub(crate) fn spawn<T: Send + 'static>(
    builder: thread::Builder,
    body: impl FnOnce() -> T + Send + 'static,
) -> io::Result<Thread<T>> {
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

/// `body` on the thread `builder` describes, as [`spawn`] starts one, out
/// of every signal's way from its first instruction
/// ([`php_process::started_aside`]): a thread the engine starts for
/// itself that runs no PHP, its timer and a [`Worker`]'s, so a signal sent
/// to the process is delivered to the thread doing the request's work and
/// interrupts its blocking call, as php's one thread takes it (#3861).
///
/// ```php
/// set_time_limit(30);
/// pcntl_signal(SIGUSR1, function ($signo) { echo "handled\n"; });
/// $parent = getmypid();
/// if (($child = pcntl_fork()) === 0) {
///     usleep(200000);
///     posix_kill($parent, SIGUSR1);
///     exit(0);
/// }
/// [$here, $there] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
/// $read = [$here];
/// $write = $except = null;
/// $start = microtime(true);
/// var_dump(@stream_select($read, $write, $except, 5));
/// var_dump(microtime(true) - $start < 4);
/// pcntl_signal_dispatch();
/// pcntl_waitpid($child, $status);
/// ```
/// ```expect process
/// bool(false)
/// bool(true)
/// handled
/// ```
pub(crate) fn spawn_aside<T: Send + 'static>(
    builder: thread::Builder,
    body: impl FnOnce() -> T + Send + 'static,
) -> io::Result<Thread<T>> {
    php_process::started_aside(|| spawn(builder, body))
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
    spawn(builder, body).map(HostThread)
}

/// The thread [`main_thread`] moved the process's work onto, once it
/// stands: the process's main thread as a fork sees it.
static MAIN: OnceLock<ThreadId> = OnceLock::new();

/// Whether the calling thread is the one [`main_thread`] started, which a
/// fork it asks copies alone, so it is never beside its own fork.
fn on_main_thread() -> bool {
    MAIN.get() == Some(&thread::current().id())
}

/// `body`, the whole of a host's `main`, as the process's main thread on
/// a stack of `stack_size` bytes, which `body` ends the process from
/// (design §5.6, #3043): the stack a request served on it has, whose
/// bounds the engine reads for php's "Maximum call stack size", so a host
/// whose process serves on its main thread, as php's CLI does, sizes the
/// stack its recursion through a native's callback needs, where the
/// platform's main thread is sized by the shell.
///
/// The thread starts through the engine's one home, so it stands inside
/// the engine until the process ends, and is the process's main thread to
/// a fork: a fork it asks, the host's or its request's, is not refused
/// for it, and the child holds it alone, as php's child holds its one
/// thread. It starts under the caller's signal mask, and the caller then
/// stands out of every signal's way ([`php_process::stand_aside`]), so a
/// signal reaches `body`'s thread as it reached the main thread, and one
/// it blocks waits for it. A panic in `body` ends the process with the
/// status a `main` that panicked ends it with, 101, once the panic hook
/// has written it. Where the system refuses the thread, `body` runs here,
/// on the stack this thread has, and the limit is what that stack
/// allows, as a host thread's is (§5.6).
pub fn main_thread<F>(stack_size: usize, body: F) -> !
where
    F: FnOnce() -> Infallible + Send + 'static,
{
    // A rendezvous: the send returns once the thread holds `body`, and
    // hands it back where the thread ended without taking it.
    let (hand, handed) = std::sync::mpsc::sync_channel::<F>(0);
    let builder = thread::Builder::new()
        .name("main".to_owned())
        .stack_size(stack_size);
    let serve = move || {
        if let Ok(body) = handed.recv() {
            let Err(_panicked) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            std::process::exit(101);
        }
    };
    let Ok(main) = spawn(builder, serve) else {
        match body() {}
    };
    MAIN.set(main.handle.thread().id()).ok();
    if let Err(std::sync::mpsc::SendError(body)) = hand.send(body) {
        match body() {}
    }
    // `main` is never dropped: the thread stands inside the engine until
    // the process ends, and nothing joins it.
    php_process::stand_aside()
}

/// What runs on a [`Worker`]'s thread (ADR-0010, Threads): the compiled
/// tier's compile queue (`jit-design.md` §4.8). The engine owns the
/// thread; the tier owns the body and the word that ends it.
pub trait Parked: Send + Sync + 'static {
    /// The thread's body: it serves until [`Parked::end`] asks it to
    /// return, which it does between two of its jobs, keeping the ones
    /// not yet served for the next body.
    fn run(&self);

    /// Asks the body that runs, or the next to start, to return. Called
    /// once before each join, so each ask ends exactly one body.
    fn end(&self);
}

/// The one thread a [`Parked`] body runs on, an engine's thread as
/// ADR-0010's Threads has them: started on demand through the engine's
/// one home, out of every signal's way ([`spawn_aside`]), so a fork
/// counts it, parked in its body between jobs, joined before every fork,
/// a host's and a request's alike, and started again after it in the
/// process that asked, as the engine's timer thread is (#2541), and
/// joined when the worker drops.
/// A child forked meanwhile has no thread for it until it wakes one.
pub struct Worker {
    name: String,
    parked: Arc<dyn Parked>,
    thread: Mutex<Option<Thread>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Every worker in the process, by a reference that does not keep it:
/// what a fork joins first, whoever asks for it.
static WORKERS: Mutex<Vec<Weak<Worker>>> = Mutex::new(Vec::new());

/// One of the process's lists of what a fork stops first, held, with
/// what has dropped let go: [`WORKERS`], the timers, the realpath
/// caches a fork's child empties ([`RealpathCaches`]), and what it
/// makes its own ([`INHERITED`]).
pub(crate) fn living<T: ?Sized>(
    list: &'static Mutex<Vec<Weak<T>>>,
) -> MutexGuard<'static, Vec<Weak<T>>> {
    let mut held = list.lock().unwrap_or_else(PoisonError::into_inner);
    held.retain(|entry| entry.strong_count() > 0);
    held
}

impl Worker {
    /// A worker for `parked`, its thread to be named `name`, with no
    /// thread yet, entered among the process's workers so a fork finds
    /// it. The entry stands inside the engine, so no fork copies the list
    /// held.
    #[must_use]
    pub fn new(name: &str, parked: Arc<dyn Parked>) -> Arc<Worker> {
        let worker = Arc::new(Worker {
            name: name.to_owned(),
            parked,
            thread: Mutex::new(None),
        });
        let _inside = Inside::enter();
        living(&WORKERS).push(Arc::downgrade(&worker));
        worker
    }

    /// Its thread started where none stands; whether one stands after.
    /// `false` only where the system refused a thread.
    pub fn wake(&self) -> bool {
        let mut thread = self.thread();
        if thread.is_none() {
            let parked = Arc::clone(&self.parked);
            let builder = thread::Builder::new().name(self.name.clone());
            *thread = spawn_aside(builder, move || parked.run()).ok();
        }
        thread.is_some()
    }

    /// Its thread, if one stands, asked to end and joined: what was
    /// joined.
    fn stop(&self) -> bool {
        let mut thread = self.thread();
        let Some(running) = thread.take() else {
            return false;
        };
        self.parked.end();
        running.join();
        true
    }

    /// The thread's slot, held as a thread that panicked holding it left it.
    fn thread(&self) -> MutexGuard<'_, Option<Thread>> {
        self.thread.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Worker {
    /// Its thread asked to end and joined: the body returns between jobs,
    /// so a job under way finishes first.
    fn drop(&mut self) {
        self.stop();
    }
}

/// Every worker in the process, as a fork finds them: each one's thread
/// joined before the fork and started again after it.
#[derive(Debug)]
struct Workers(Vec<Arc<Worker>>);

impl Workers {
    /// The workers that stand in the process.
    fn live() -> Workers {
        Workers(living(&WORKERS).iter().filter_map(Weak::upgrade).collect())
    }

    /// How many of them have a thread: what stands inside the engine for
    /// them, which a fork joins rather than refuses. `None` where one's
    /// slot is held, by a request waking it, which a fork does not wait
    /// for: the request may itself be waiting for the fork to end.
    fn threads(&self) -> Option<usize> {
        self.0.iter().try_fold(0, |threads, worker| {
            let thread = match worker.thread.try_lock() {
                Ok(thread) => thread,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => return None,
            };
            Some(threads + usize::from(thread.is_some()))
        })
    }

    /// Each one's thread asked to end and joined: what is stopped is the
    /// workers that had one.
    fn stop(self) -> Joined {
        let joined = self.0.iter().filter(|worker| worker.stop());
        Joined(joined.map(Arc::downgrade).collect())
    }
}

/// The workers whose threads a fork joined, for the length of the fork,
/// by a reference that does not keep them: the process that asked gives
/// each that still stands its thread again, so a worker whose engine
/// ended meanwhile is not started for nothing; a child forked meanwhile
/// drops them, and wakes each when it next has a job.
#[derive(Debug)]
#[must_use]
struct Joined(Vec<Weak<Worker>>);

impl Joined {
    /// Each one's thread started again. The restart stands inside the
    /// engine, as a timer's does ([`crate::interrupt::Stopped::resume`]).
    fn resume(self) {
        let _inside = Inside::enter();
        for worker in self.0.iter().filter_map(Weak::upgrade) {
            worker.wake();
        }
    }
}

/// What a forked child would share with its parent and makes its own
/// before anything else of it runs: the compiled tier's code arena on
/// Linux, two views of one `memfd` mapped shared (`jit-design.md` §4.4).
/// The engine owns the fork and its order; the tier owns what it holds
/// and how its child takes it, as a [`Parked`] body is the tier's on an
/// engine's thread.
pub trait Inherited: Send + Sync + 'static {
    /// Held for a fork, once every thread the engine started is joined
    /// and before the process is copied: whatever the child will need is
    /// taken and made here, so the child copies nothing half made and
    /// takes nothing another thread may hold. What it answers runs in the
    /// child alone, before the child does anything else, and is dropped
    /// unrun in the process that asked, which lets the hold go unchanged.
    fn hold(&self) -> Box<dyn FnOnce() + '_>;
}

/// Everything in the process a fork's child makes its own, by a
/// reference that does not keep it: what a fork holds last, whoever asks.
static INHERITED: Mutex<Vec<Weak<dyn Inherited>>> = Mutex::new(Vec::new());

/// `entry` entered among what every fork holds and every fork's child
/// makes its own ([`Inherited`]), until it drops. The entry stands inside
/// the engine, so no fork copies the list held.
pub fn inherit(entry: Weak<dyn Inherited>) {
    let _inside = Inside::enter();
    living(&INHERITED).push(entry);
}

/// What the process holds that a fork's child makes its own, as a fork
/// finds it.
struct Heritable(Vec<Arc<dyn Inherited>>);

impl Heritable {
    /// Every entry that stands, in the order each was entered.
    fn live() -> Heritable {
        Heritable(
            living(&INHERITED)
                .iter()
                .filter_map(Weak::upgrade)
                .collect(),
        )
    }

    /// Each one held, in [`Heritable::live`]'s order, which is every
    /// fork's.
    fn hold(&self) -> Kept<'_> {
        Kept(self.0.iter().map(|entry| entry.hold()).collect())
    }
}

/// What a fork holds for its child to make its own: let go unchanged in
/// the process that asked, taken by the child ([`Kept::take`]).
#[must_use]
struct Kept<'e>(Vec<Box<dyn FnOnce() + 'e>>);

impl Kept<'_> {
    /// In a fork's child, each entry made the child's own, in order.
    fn take(self) {
        for entry in self.0 {
            entry();
        }
    }
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

/// A fork under way, from its first reading of what stands inside the
/// engine to the fork's return: the one type that forks, and the only
/// caller of `php_process::fork`. While it is held [`FORKING`] is set, so
/// nothing enters the engine and what stands inside can only leave; a
/// second fork waits for it to be let go.
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
    /// Held, once no other fork is under way. Nothing is read here: what
    /// stands inside is [`Alone::beside`]'s to say.
    fn take() -> Alone {
        while INSIDE
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |inside| {
                (inside & FORKING == 0).then_some(inside | FORKING)
            })
            .is_err()
        {
            thread::yield_now();
        }
        Alone(())
    }

    /// How many things stand inside the engine beside `asker`, and beside
    /// the thread it asks on where that is the process's main thread
    /// [`main_thread`] started. With the fork held the number can only
    /// fall.
    #[expect(
        clippy::unused_self,
        reason = "the hold is what makes the number one that only falls"
    )]
    fn beside(&self, asker: Asker) -> usize {
        let own = asker as usize + usize::from(on_main_thread());
        (INSIDE.load(Ordering::Acquire) & !FORKING).saturating_sub(own)
    }

    /// `php_process::fork`, where nothing stands inside the engine beside
    /// `asker`, and `EBUSY` without a fork where anything does. In both
    /// processes the hold is let go once it returns, the child's copy of
    /// the number being the caller's own.
    fn fork(self, asker: Asker) -> io::Result<Forked> {
        if self.beside(asker) != 0 {
            return Err(busy());
        }
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

/// The error a fork is refused with while something else stands inside
/// the engine.
fn busy() -> io::Error {
    io::Error::from_raw_os_error(EBUSY)
}

/// Every fork, a request's and a host's (ADR-0010, Threads): the fork
/// held, so nothing enters the engine from here; refused with `EBUSY`,
/// touching no timer, where anything stands inside beside `asker` other
/// than the timer threads of the process's engines, each parked between
/// requests while its engine lives (#2541), and the threads of the
/// process's [`Worker`]s, each parked between its jobs; those threads
/// joined, a worker's once the job under way is done; then
/// [`Alone::fork`], which reads the number again; and, the hold let go,
/// each timer that had a thread given one again in the process that
/// asked, over the deadlines it had, and each worker that had one, or
/// where the fork was refused or failed. A child drops its copies
/// unstarted, forgets each schedule as it reaches it (see
/// [`Timer`](crate::interrupt::Timer)), and wakes a worker as it next
/// has a job for it.
///
/// Every engine's realpath cache is locked once the threads are joined,
/// and held across the copy: let go unchanged in the process that asked,
/// and emptied before it is let go in the child, as php's
/// `php_child_init` empties the process's. Whatever holds one, a request
/// still on its way out of the engine or a thread outside it as a test
/// is, holds it for one lookup, entry or clean and lets it go without
/// waiting on the fork ([`RealpathCaches::hold`]), so the fork waits for
/// it and the child never copies one held; a request that still stands
/// inside once it is let go refuses the fork at [`Alone::fork`]'s reading,
/// and the caches are let go unchanged.
///
/// Last, each [`Inherited`] entry is held, the tier's code arena among
/// them, its compile mutex taken and a copy of its code made for the
/// child; the child maps the copy over its arena before the caches are
/// let go, and the process that asked lets both go unchanged.
///
/// What stands inside is read before the threads are counted, and only
/// falls while the fork is held: a count that covers the reading means
/// nothing but those threads stood inside, so nothing holds a timer's
/// mutex or a worker's slot as its thread is joined, nor as the process
/// is copied. Counting takes no timer's mutex nor worker's slot that is
/// held: one held is a request at work on it, and the fork is refused.
fn fork_as(asker: Asker) -> io::Result<Forked> {
    let alone = Alone::take();
    let beside = alone.beside(asker);
    let timers = Timers::live();
    let workers = Workers::live();
    let standing = (timers.threads()).and_then(|t| workers.threads().map(|w| t + w));
    if standing.is_none_or(|standing| beside > standing) {
        return Err(busy());
    }
    let stopped = timers.stop();
    let joined = workers.stop();
    let caches = RealpathCaches::live();
    let held = caches.hold();
    let heritable = Heritable::live();
    let kept = heritable.hold();
    let forked = alone.fork(asker);
    if matches!(forked, Ok(Forked::Child)) {
        kept.take();
        held.clean();
    } else {
        drop(kept);
        drop(held);
        stopped.resume();
        joined.resume();
    }
    forked
}

/// The fork `Limits::fork` asks for, with that request alone inside
/// the engine beside the timers' threads it joins.
pub(crate) fn fork_request() -> io::Result<Forked> {
    fork_as(Asker::Request)
}

/// `fork(2)` for a host (#1807): the process copied with no request in
/// flight, as php-fpm's master makes a child (`fpm_children_make`) and
/// daemonizes (`fpm_unix_init_main`), returning in both as
/// [`Forked::Parent`] with the child's id and as [`Forked::Child`]. In
/// the child what php's `php_child_init` restores, which php-fpm calls in
/// each child it makes (`fpm_php_init_child`, `Z/sapi/fpm/fpm/fpm_php.c:223`)
/// and the CGI in each it forks (`Z/sapi/cgi/cgi_main.c:2044`), is
/// restored as for a request's fork: the realpath cache is the engine's,
/// not a request's, so the child starts with every engine's empty; the
/// deadlines and the stat cache are a request's, and none is in flight.
/// A daemonizing master's child, which php does not empty, is emptied
/// too, and runs no program before the children it makes empty theirs.
///
/// The engine's timer thread, which stays parked between requests once
/// a request has set a deadline, the startup request's among them, is
/// joined first and started again in the parent, so a host's fork after
/// a timed request is not refused (#2541).
///
/// # Errors
///
/// `EBUSY` where a request is in flight or a thread the engine started
/// other than its timer's and the caller's own [`main_thread`] stands, as
/// on a host that serves on threads, which no host that forks does;
/// otherwise the OS error `fork(2)` failed with, and no process was
/// made.
pub fn fork() -> io::Result<Forked> {
    fork_as(Asker::Host)
}

/// The tests' one rule for forks (#2715). A fork takes every engine's
/// timer and every [`Worker`] in the process and holds the inside
/// count's [`FORKING`] bit, all for the length of the fork. Every test in
/// the binary shares the process, so a test that forks, that takes the
/// process's timers or workers, or that reads what a fork holds while
/// it runs holds this guard. Reading
/// what a fork holds means a timer's end at its last drop, which a
/// fork's clone delays, or the [`FORKING`] bit. No sibling's fork is
/// then under way while the test reads.
#[cfg(test)]
pub(crate) fn forks_held() -> std::sync::MutexGuard<'static, ()> {
    static FORKS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    FORKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{
        EBUSY, FORKING, INSIDE, Inside, Parked, Worker, Workers, fork, forks_held, host_thread,
        run_apart, spawn,
    };
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, PoisonError};

    /// A thread started through the home is inside the engine while it
    /// waits; the harness's other tests move the number too, so what is
    /// read is a floor.
    #[test]
    fn a_thread_is_inside_while_it_stands() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let builder = std::thread::Builder::new().name("php-test".to_owned());
        let thread = spawn(builder, move || {
            rx.recv().ok();
        })
        .expect("the system starts the thread");
        assert!(INSIDE.load(Ordering::Relaxed) & !FORKING >= 1);
        drop(tx);
        thread.join();
    }

    /// A host's thread stands inside the engine while it runs, so a fork
    /// asked meanwhile is refused.
    #[test]
    fn a_host_thread_is_inside_while_it_stands() {
        let _forks = forks_held();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let thread = host_thread("php-host", 3 << 20, move || {
            rx.recv().ok();
        })
        .expect("the system starts the host thread");
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
        let _forks = forks_held();
        let inside = Inside::enter();
        let refused = fork().err().and_then(|e| e.raw_os_error());
        drop(inside);
        assert_eq!(refused, Some(EBUSY));
        assert_eq!(INSIDE.load(Ordering::Relaxed) & FORKING, 0);
    }

    /// A thread apart answers its body over the input it was handed,
    /// joined before the call returns.
    #[test]
    fn a_thread_apart_answers_its_body_joined() {
        let ran = run_apart("php-test-apart", 1 << 20, 20, |input: u32| input + 1);
        assert!(matches!(ran, Ok(Ok(21))));
    }

    /// A body that waits until it is asked to end, counting how often it
    /// ran and returned.
    #[derive(Default)]
    struct Waits {
        ending: std::sync::Mutex<bool>,
        asked: std::sync::Condvar,
        ran: std::sync::atomic::AtomicUsize,
        returned: std::sync::atomic::AtomicUsize,
    }

    impl Parked for Waits {
        fn run(&self) {
            self.ran.fetch_add(1, Ordering::SeqCst);
            let mut ending = self.ending.lock().unwrap_or_else(PoisonError::into_inner);
            while !*ending {
                ending = self
                    .asked
                    .wait(ending)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            *ending = false;
            self.returned.fetch_add(1, Ordering::SeqCst);
        }

        fn end(&self) {
            *self.ending.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.asked.notify_all();
        }
    }

    /// A worker has no thread until it is woken, one thread however
    /// often, which a fork counts, joins once its body returned and
    /// starts again after; a dropped worker's thread is joined.
    #[test]
    fn a_worker_is_joined_before_a_fork_and_woken_after() {
        let _forks = forks_held();
        let waits = Arc::new(Waits::default());
        let worker = Worker::new("php-test-worker", Arc::clone(&waits) as Arc<dyn Parked>);
        assert_eq!(Workers(vec![Arc::clone(&worker)]).threads(), Some(0));
        assert!(worker.wake() && worker.wake());
        assert_eq!(Workers(vec![Arc::clone(&worker)]).threads(), Some(1));
        assert!(
            Workers::live().0.iter().any(|w| Arc::ptr_eq(w, &worker)),
            "the fork finds the worker"
        );
        let joined = Workers(vec![Arc::clone(&worker)]).stop();
        assert_eq!(
            waits.returned.load(Ordering::SeqCst),
            1,
            "joined after its body returned"
        );
        assert_eq!(
            Workers(vec![Arc::clone(&worker)]).threads(),
            Some(0),
            "no thread at the fork"
        );
        joined.resume();
        assert_eq!(
            Workers(vec![Arc::clone(&worker)]).threads(),
            Some(1),
            "woken after it"
        );
        let gone = Arc::downgrade(&worker);
        drop(worker);
        assert!(gone.upgrade().is_none());
        assert_eq!(waits.ran.load(Ordering::SeqCst), 2);
        assert_eq!(
            waits.returned.load(Ordering::SeqCst),
            2,
            "the drop joined it"
        );
    }

    /// A worker a request is waking is not counted, so the fork is
    /// refused rather than waiting on a request that may wait on it.
    #[test]
    fn a_worker_being_woken_is_not_counted() {
        let _forks = forks_held();
        let worker = Worker::new(
            "php-test-worker",
            Arc::new(Waits::default()) as Arc<dyn Parked>,
        );
        let held = worker.thread();
        assert_eq!(Workers(vec![Arc::clone(&worker)]).threads(), None);
        drop(held);
        assert_eq!(Workers(vec![worker]).threads(), Some(0));
    }
}
