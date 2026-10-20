//! The process's signal record and the rules over it (design §5.8, revision 38;
//! #1062): what the engine keeps of the signals an extension's handler
//! posts, which of them a request takes, and how a request's interrupt
//! is raised by one. The mechanism is `php_process::post`'s, which knows no
//! signal; the handler that writes is the extension's, in its `src/ffi/`,
//! beside `sigaction`. Nothing here runs in a signal handler: the handler
//! calls [`SIGNAL_POST`]'s `post`, which is `php_process`'s, with a closure
//! of its own, and what is here runs at a request's interrupt check or
//! at a dispatch the program called.
//!
//! php's pcntl keeps its queue and its `async_signals` word in
//! `pcntl_globals` and its handler writes them from whatever the process
//! was doing (`pcntl_signal_handler`, `ext/pcntl/pcntl.c:1333`). Signal
//! dispositions are the process's, so what a handler writes is the
//! process's too, and an extension holds no atomic (ADR-0005): the
//! record is here, engine-owned.
//!
//! Beside the record is [`Reentry`], one per request whose FFI made a
//! callback: how a function C calls back reaches the request (#1638,
//! #1640). What a signal handler may reach of it, taking the lent
//! request and a deferral, is atomic operations and `php_process::post`'s,
//! so ext/ffi's `zend_ffi_callback_trampoline` may make them there; what
//! it defers, and what another thread waits on, runs at the interrupt
//! check.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::Thread;
use std::time::Duration;

use php_process::post::{Pool, Poster, Registered, WakeSet};

use crate::api::request::Interrupt;

/// How many signals the pool holds between two takes: php's spares are
/// `NSIG` of them, allocated at the first `pcntl_signal`, and 64 is at
/// least that on every target the engine builds for.
const SLOTS: usize = 64;

/// How many interrupts one signal raises: one per request that turned
/// asynchronous delivery on and has not turned it off.
const RECEIVERS: usize = 64;

/// One signal as the process took it: the number and what its
/// `siginfo_t` said, which php's `pcntl_siginfo_to_zval` renders. An
/// extension's handler fills it from the kernel's structure, and
/// `waitid` hands the same fields back. `utime`, `stime`, `band` and
/// `fd` are Linux's members, `SIGCHLD`'s clock ticks and `SIGPOLL`'s
/// band and file number, which `<signal.h>` names there alone; elsewhere
/// they stay zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Siginfo {
    pub number: i32,
    pub errno: i32,
    pub code: i32,
    pub pid: i32,
    pub uid: u32,
    pub status: i32,
    pub addr: usize,
    pub utime: i64,
    pub stime: i64,
    pub band: i64,
    pub fd: i32,
}

static POOL: Pool<Siginfo, SLOTS> = Pool::new(Siginfo {
    number: 0,
    errno: 0,
    code: 0,
    pid: 0,
    uid: 0,
    status: 0,
    addr: 0,
    utime: 0,
    stime: 0,
    band: 0,
    fd: 0,
});

static WAKE: WakeSet<RECEIVERS> = WakeSet::new();

/// What an extension's signal handler posts through: the signal into a
/// free slot of the process's pool, filled by the handler's closure, and
/// every request's interrupt raised that asked for asynchronous delivery,
/// which is php's `EG(vm_interrupt) = true` under
/// `pcntl_async_signals(true)`. Async-signal-safe, and the only item here
/// a handler may reach.
pub static SIGNAL_POST: Poster<'static, Siginfo, SLOTS, RECEIVERS> = Poster::new(&POOL, &WAKE);

/// The signals posted under a number `wanted` asks for, taken off the
/// pool in the order they arrived: php's dispatch taking its queue. A
/// request asks for the numbers its own handler table holds, so two
/// requests of one process never take each other's; a slot a handler is
/// still filling is left for the next take, which the interrupt that
/// handler raises asks for.
pub fn take_signals(wanted: impl Fn(i32) -> bool) -> Vec<Siginfo> {
    let mut taken = Vec::new();
    POOL.take(|posted| wanted(posted.number), |posted| taken.push(posted));
    taken
}

/// A request's interrupt on the process's wake set, raised at every
/// signal posted while this stands; dropping it takes the interrupt off.
#[derive(Debug)]
pub struct Receiving {
    /// Held, and read by nothing: its drop is what takes the interrupt
    /// off the wake set.
    _registered: Registered<'static>,
}

/// `owner`'s interrupt put on the wake set; `None` when it holds as
/// many as it can.
pub(crate) fn receive<T>(owner: Arc<T>) -> Option<Receiving>
where
    T: AsRef<AtomicBool> + Send + Sync + 'static,
{
    WAKE.register(owner).map(|registered| Receiving {
        _registered: registered,
    })
}

/// How many deferred calls one request holds between two checks of
/// its interrupt; a call past them is forgotten, as a signal posted to
/// a full pool is.
const DEFERRED: usize = 8;

/// How many words of argument values a [`Deferred`] carries.
pub const DEFERRED_WORDS: usize = 16;

/// One call C made of a request's callback while the request could not
/// be lent to it: the callback's number and its argument values, each
/// copied from where C passed it to a word boundary of `words`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deferred {
    pub callback: usize,
    pub words: [u64; DEFERRED_WORDS],
}

/// How a function C calls back reaches its request, which is
/// `zend_ffi_callback_trampoline`'s `EG()` made sound (#1638, #1640):
/// while the request waits in a call into C or blocks in an engine
/// native ([`crate::api::request::Limits::blocking`]) it is lent here,
/// as an address one callback at a time takes and gives back; at any
/// other time a callback's call is deferred to the request's interrupt
/// check, which serves it as it dispatches pcntl's signals. A call C
/// makes on the request's own thread, from a signal handler, is posted
/// as a [`Deferred`] copy and C reads a zeroed return; one C makes from
/// another thread is posted as a [`Reentry::wait`], and that thread is
/// held until the request answers it, lends itself, or ends. No second
/// `&mut Request` is ever made: the address is out exactly while the
/// request's own thread holds it in nothing but the wait, and is taken
/// by one callback alone. What a signal handler may reach, taking the
/// lent address and a deferral, is a bounded sequence of atomic
/// operations.
#[derive(Debug)]
pub struct Reentry {
    /// The generation of the lend whose request may be taken, or zero
    /// while nothing is lent or a callback holds it.
    lent: AtomicUsize,
    /// The request's address, which the lend that publishes a
    /// generation writes first.
    address: AtomicUsize,
    /// The last generation a lend took.
    lends: AtomicUsize,
    deferred: Pool<Deferred, DEFERRED>,
    /// The calls threads other than the request's wait on.
    waits: [Wait; WAITS],
    /// Whether the request has ended: a call waiting is abandoned, and
    /// one made since returns at once.
    closed: AtomicBool,
    interrupt: Interrupt,
}

/// How many threads at once may wait on one request; a further one
/// waits for a slot to come free.
const WAITS: usize = 8;

/// How long a waiting thread sleeps before it looks again, where
/// nothing woke it: the bound on how late it finds the request lent.
const WAIT_POLL: Duration = Duration::from_millis(1);

/// Where one waited call stands, a state machine over one byte.
mod wait {
    /// No thread holds the slot.
    pub(super) const FREE: u8 = 0;
    /// One party alone holds it: the waiter filling or leaving it, or
    /// serving itself on the lent request.
    pub(super) const OWNED: u8 = 1;
    /// Posted, and nobody has taken it yet.
    pub(super) const POSTED: u8 = 2;
    /// The request's thread is serving it.
    pub(super) const SERVING: u8 = 3;
    /// Served: the return is where C reads it.
    pub(super) const ANSWERED: u8 = 4;
    /// The request ended with it posted.
    pub(super) const ABANDONED: u8 = 5;
}

/// One slot of [`Reentry::wait`]: where it stands, the waiter's call,
/// and the waiter's thread, which whoever answers it unparks.
#[derive(Debug, Default)]
struct Wait {
    state: AtomicU8,
    call: AtomicUsize,
    thread: Mutex<Option<Thread>>,
}

impl Wait {
    fn moved(&self, from: u8, to: u8) -> bool {
        self.state
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The waiter woken, where it is parked; a slot whose thread is
    /// being set or cleared is skipped, since the thread doing it is
    /// not parked.
    fn wake(&self) {
        if let Ok(thread) = self.thread.try_lock()
            && let Some(thread) = thread.as_ref()
        {
            thread.unpark();
        }
    }

    /// The slot written for this thread's `call` and posted.
    fn post(&self, call: NonZeroUsize) {
        self.call.store(call.get(), Ordering::Relaxed);
        *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(std::thread::current());
        self.state.store(wait::POSTED, Ordering::Release);
    }

    /// The slot given up by its waiter.
    fn leave(&self) {
        *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.state.store(wait::FREE, Ordering::Release);
    }
}

/// The request a callback took from [`Reentry::take_lent`], held until
/// [`Reentry::give_back`]: its address, and the lend it was taken from.
#[derive(Debug)]
#[must_use = "a request taken is given back, or the wait that lent it never ends"]
pub struct Lent {
    generation: usize,
    at: NonZeroUsize,
}

impl Lent {
    /// The request's address, as the lend exposed it.
    #[must_use]
    pub fn at(&self) -> NonZeroUsize {
        self.at
    }
}

/// One lend of the request ([`Reentry::lend`]), which its
/// [`Reentry::reclaim`] ends: its generation, and the one takeable
/// before it.
#[derive(Debug)]
#[must_use = "a lend is reclaimed before the request is used again"]
pub(crate) struct Lending {
    generation: usize,
    before: usize,
}

/// How a [`Reentry::wait`] ended for the thread that waited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// Served, by the request's thread at its interrupt check or by the
    /// waiter itself on the lent request: the return is written.
    Answered,
    /// The request ended first: nothing served it, and C reads the
    /// return the waiter left.
    Abandoned,
}

impl Reentry {
    /// Nothing lent and nothing deferred; a deferral raises `interrupt`.
    #[must_use]
    pub(crate) fn new(interrupt: &Interrupt) -> Reentry {
        Reentry {
            lent: AtomicUsize::new(0),
            address: AtomicUsize::new(0),
            lends: AtomicUsize::new(0),
            deferred: Pool::new(Deferred {
                callback: 0,
                words: [0; DEFERRED_WORDS],
            }),
            waits: Default::default(),
            closed: AtomicBool::new(false),
            interrupt: interrupt.clone(),
        }
    }

    /// The request at `at` lent while it waits, under a generation of
    /// its own, which [`Reentry::reclaim`] takes back when the wait
    /// ends; every thread waiting on the request is woken to take it. A
    /// callback holding the request may lend it again, and the newer
    /// lend is what a callback then takes.
    pub(crate) fn lend(&self, at: NonZeroUsize) -> Lending {
        let generation = self.lends.fetch_add(1, Ordering::Relaxed) + 1;
        self.address.store(at.get(), Ordering::Relaxed);
        let before = self.lent.swap(generation, Ordering::AcqRel);
        self.wake_posted();
        Lending { generation, before }
    }

    /// The request taken back once the wait ended, and the lend it
    /// stood under made takeable again: waiting, where a callback still
    /// holds it, until that callback gives it back, so the request is
    /// never reached from two places.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "taking the lend ends it, so it is reclaimed once"
    )]
    pub(crate) fn reclaim(&self, lending: Lending) {
        while self
            .lent
            .compare_exchange(
                lending.generation,
                lending.before,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            std::thread::yield_now();
        }
    }

    /// The request lent, taken by one callback: none while nothing is
    /// lent, or another callback holds it.
    pub fn take_lent(&self) -> Option<Lent> {
        let generation = self.lent.swap(0, Ordering::AcqRel);
        if generation == 0 {
            return None;
        }
        NonZeroUsize::new(self.address.load(Ordering::Relaxed)).map(|at| Lent { generation, at })
    }

    /// What [`Reentry::take_lent`] took, given back as the callback
    /// returns to C; every other thread waiting is woken to take it.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "taking the request ends the hold, so it is given back once"
    )]
    pub fn give_back(&self, lent: Lent) {
        self.lent.store(lent.generation, Ordering::Release);
        self.wake_posted();
    }

    /// One call posted, `fill` writing it, and the request's interrupt
    /// raised whether or not there was room: `false` where every slot
    /// was taken and the call is forgotten.
    pub fn defer(&self, fill: impl FnOnce(&mut Deferred)) -> bool {
        let posted = self.deferred.post(fill);
        self.interrupt.raise();
        posted
    }

    /// Every call deferred, in the order C made them.
    pub fn take_deferred(&self) -> Vec<Deferred> {
        let mut taken = Vec::new();
        self.deferred
            .take(|_| true, |deferred| taken.push(deferred));
        taken
    }

    /// `call`, made by a thread that is not the request's, posted and
    /// that thread held until it is served (#1640): by the request's
    /// thread at its next interrupt check ([`Reentry::serve_waiting`]),
    /// or by this thread through `lent` with the request's address,
    /// where the request lends itself first, so the request never waits
    /// on C while C waits on the request. A call made after the request
    /// ended, or still posted when it ends, is [`Waited::Abandoned`].
    /// `call` is the caller's own, which the server reads while it
    /// serves and the waiter keeps standing until this returns.
    pub fn wait(&self, call: NonZeroUsize, mut lent: impl FnMut(&Lent)) -> Waited {
        let slot = loop {
            if self.closed.load(Ordering::Acquire) {
                return Waited::Abandoned;
            }
            if let Some(slot) = self
                .waits
                .iter()
                .find(|slot| slot.moved(wait::FREE, wait::OWNED))
            {
                break slot;
            }
            std::thread::park_timeout(WAIT_POLL);
        };
        slot.post(call);
        self.interrupt.raise();
        let waited = loop {
            match slot.state.load(Ordering::Acquire) {
                wait::ANSWERED => break Waited::Answered,
                wait::ABANDONED => break Waited::Abandoned,
                wait::POSTED
                    if self.closed.load(Ordering::Acquire)
                        && slot.moved(wait::POSTED, wait::OWNED) =>
                {
                    break Waited::Abandoned;
                }
                wait::POSTED => {
                    if let Some(taken) = self.take_lent() {
                        let mine = slot.moved(wait::POSTED, wait::OWNED);
                        if mine {
                            lent(&taken);
                        }
                        self.give_back(taken);
                        if mine {
                            break Waited::Answered;
                        }
                    }
                }
                _ => {}
            }
            std::thread::park_timeout(WAIT_POLL);
        };
        slot.leave();
        waited
    }

    /// Every call another thread waits on handed to `serve`, which
    /// writes its return, and its thread woken: at the request's
    /// interrupt check, on the request's thread.
    pub fn serve_waiting(&self, mut serve: impl FnMut(NonZeroUsize)) {
        for slot in &self.waits {
            if !slot.moved(wait::POSTED, wait::SERVING) {
                continue;
            }
            if let Some(call) = NonZeroUsize::new(slot.call.load(Ordering::Relaxed)) {
                serve(call);
            }
            slot.state.store(wait::ANSWERED, Ordering::Release);
            slot.wake();
        }
    }

    /// The request ended: every call still posted abandoned and its
    /// thread woken, and any call made since returns at once.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for slot in &self.waits {
            if slot.moved(wait::POSTED, wait::ABANDONED) {
                slot.wake();
            }
        }
    }

    /// Each thread whose call is posted woken, to take the request now
    /// lent.
    fn wake_posted(&self) {
        for slot in &self.waits {
            if slot.state.load(Ordering::Acquire) == wait::POSTED {
                slot.wake();
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::api::request::Interrupt;

    /// A post fills a slot and raises every receiving interrupt; a take
    /// of its number frees the slot, a take of another number leaves it,
    /// and a dropped receiver is raised no more. The numbers are none a
    /// signal has, so no other test's handler takes them.
    #[test]
    fn a_post_is_taken_by_its_number_and_raises_what_receives() {
        let interrupt = Interrupt::default();
        let receiving = interrupt.receive_signals();
        assert!(receiving.is_some());
        SIGNAL_POST.post(|posted| posted.number = 1_000);
        SIGNAL_POST.post(|posted| {
            posted.number = 1_001;
            posted.pid = 7;
        });
        assert!(interrupt.raised().load(Ordering::Acquire));
        assert_eq!(take_signals(|n| n == 1_002), vec![]);
        let taken = take_signals(|n| n == 1_000 || n == 1_001);
        assert_eq!(
            taken.iter().map(|p| (p.number, p.pid)).collect::<Vec<_>>(),
            vec![(1_000, 0), (1_001, 7)]
        );
        assert_eq!(take_signals(|n| n == 1_000), vec![]);
        drop(receiving);
        interrupt.raised().store(false, Ordering::Relaxed);
        SIGNAL_POST.post(|posted| posted.number = 1_003);
        assert!(!interrupt.raised().load(Ordering::Acquire));
        assert_eq!(take_signals(|n| n == 1_003).len(), 1);
    }

    /// A lent request is taken by one callback at a time and given
    /// back; a deferral raises the interrupt and is taken in order.
    #[test]
    fn a_lent_request_is_taken_once_and_a_deferral_raises() {
        let interrupt = Interrupt::default();
        let reentry = Reentry::new(&interrupt);
        assert!(reentry.take_lent().is_none());
        let at = NonZeroUsize::MIN.saturating_add(7);
        let lending = reentry.lend(at);
        let taken = reentry.take_lent();
        assert_eq!(taken.as_ref().map(Lent::at), Some(at));
        assert!(reentry.take_lent().is_none());
        if let Some(taken) = taken {
            reentry.give_back(taken);
        }
        reentry.reclaim(lending);
        assert!(reentry.take_lent().is_none());
        assert!(reentry.defer(|deferred| deferred.callback = 1));
        assert!(reentry.defer(|deferred| deferred.callback = 2));
        assert!(interrupt.raised().load(Ordering::Acquire));
        let taken = reentry.take_deferred();
        assert_eq!(
            taken.iter().map(|d| d.callback).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(reentry.take_deferred().is_empty());
    }

    /// A callback holding the request lends it again: the newer lend is
    /// what the next callback takes, and the first lend's reclaim waits
    /// until the holder has reclaimed its own and given the request back.
    #[test]
    fn a_nested_lend_is_not_reclaimed_by_the_outer_one() {
        let reentry = Arc::new(Reentry::new(&Interrupt::default()));
        let at = NonZeroUsize::MIN.saturating_add(7);
        let outer = reentry.lend(at);
        let held = reentry.take_lent();
        assert!(held.is_some());
        let inner = reentry.lend(at);
        let reclaimed = Arc::new(AtomicBool::new(false));
        let waiting = {
            let (reentry, reclaimed) = (Arc::clone(&reentry), Arc::clone(&reclaimed));
            std::thread::spawn(move || {
                reentry.reclaim(outer);
                reclaimed.store(true, Ordering::Release);
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        assert!(!reclaimed.load(Ordering::Acquire), "the inner lend stands");
        reentry.reclaim(inner);
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !reclaimed.load(Ordering::Acquire),
            "a callback still holds it"
        );
        if let Some(held) = held {
            reentry.give_back(held);
        }
        assert!(waiting.join().is_ok());
        assert!(reclaimed.load(Ordering::Acquire));
        assert!(reentry.take_lent().is_none());
    }

    /// A call another thread waits on is posted, raises the interrupt,
    /// and returns once the request's thread has served it.
    #[test]
    fn a_waited_call_is_served_at_the_check() {
        let interrupt = Interrupt::default();
        let reentry = Arc::new(Reentry::new(&interrupt));
        let call = NonZeroUsize::MIN.saturating_add(41);
        let waiter = {
            let reentry = Arc::clone(&reentry);
            std::thread::spawn(move || reentry.wait(call, |_| {}))
        };
        while !interrupt.raised().swap(false, Ordering::AcqRel) {
            std::thread::yield_now();
        }
        let mut served = Vec::new();
        reentry.serve_waiting(|call| served.push(call));
        assert_eq!(served, vec![call]);
        assert_eq!(waiter.join().ok(), Some(Waited::Answered));
    }

    /// A call another thread waits on while the request is lent is
    /// served on that thread, with the request.
    #[test]
    fn a_waited_call_takes_the_lent_request() {
        let reentry = Arc::new(Reentry::new(&Interrupt::default()));
        let at = NonZeroUsize::MIN.saturating_add(7);
        let lending = reentry.lend(at);
        let waiter = {
            let reentry = Arc::clone(&reentry);
            std::thread::spawn(move || {
                let mut took = None;
                let waited = reentry.wait(NonZeroUsize::MIN, |lent| took = Some(lent.at()));
                (waited, took)
            })
        };
        assert_eq!(waiter.join().ok(), Some((Waited::Answered, Some(at))));
        reentry.reclaim(lending);
        let mut served = 0;
        reentry.serve_waiting(|_| served += 1);
        assert_eq!(served, 0, "nothing is left to serve");
    }

    /// A call still posted when the request ends is abandoned and its
    /// thread let go, and one made after returns at once.
    #[test]
    fn a_waited_call_is_abandoned_at_the_end() {
        let interrupt = Interrupt::default();
        let reentry = Arc::new(Reentry::new(&interrupt));
        let waiter = {
            let reentry = Arc::clone(&reentry);
            std::thread::spawn(move || reentry.wait(NonZeroUsize::MIN, |_| {}))
        };
        while !interrupt.raised().load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        reentry.close();
        assert_eq!(waiter.join().ok(), Some(Waited::Abandoned));
        assert_eq!(reentry.wait(NonZeroUsize::MIN, |_| {}), Waited::Abandoned);
    }
}
