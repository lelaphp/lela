//! The process's signal record and the rules over it (design §5.8, revision 38;
//! #1062): what the engine keeps of the signals an extension's handler
//! posts, which of them a request takes, and how a request's interrupt
//! is raised by one. The mechanism is `php_heap::post`'s, which knows no
//! signal; the handler that writes is the extension's, in its `src/ffi/`,
//! beside `sigaction`. Nothing here runs in a signal handler: the handler
//! calls [`SIGNAL_POST`]'s `post`, which is `php_heap`'s, with a closure
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
//! callback: how a function C calls back reaches the request (#1638).
//! Its operations are atomic operations and `php_heap::post`'s, so
//! ext/ffi's `zend_ffi_callback_trampoline` may make them from a
//! signal handler too; what it defers runs at the interrupt check.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use php_heap::post::{Pool, Poster, Registered, WakeSet};

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
/// `waitid` hands the same fields back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Siginfo {
    pub number: i32,
    pub errno: i32,
    pub code: i32,
    pub pid: i32,
    pub uid: u32,
    pub status: i32,
    pub addr: usize,
}

static POOL: Pool<Siginfo, SLOTS> = Pool::new(Siginfo {
    number: 0,
    errno: 0,
    code: 0,
    pid: 0,
    uid: 0,
    status: 0,
    addr: 0,
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
/// `zend_ffi_callback_trampoline`'s `EG()` made sound (#1638): while
/// the request waits in a call into C it is lent here, as an address
/// one callback at a time takes and gives back; at any other time a
/// callback's call is deferred, posted beside the request and the
/// request's interrupt raised, so the interrupt check serves it as it
/// dispatches pcntl's signals. No second `&mut Request` is ever made:
/// the address is out exactly while the request's own thread holds it
/// in nothing but the call into C, and is taken by one callback alone.
/// Each operation is a bounded sequence of atomic operations, so a
/// signal handler may make it.
#[derive(Debug)]
pub struct Reentry {
    /// The address lent, or zero.
    lent: AtomicUsize,
    deferred: Pool<Deferred, DEFERRED>,
    interrupt: Interrupt,
}

impl Reentry {
    /// Nothing lent and nothing deferred; a deferral raises `interrupt`.
    #[must_use]
    pub fn new(interrupt: &Interrupt) -> Reentry {
        Reentry {
            lent: AtomicUsize::new(0),
            deferred: Pool::new(Deferred {
                callback: 0,
                words: [0; DEFERRED_WORDS],
            }),
            interrupt: interrupt.clone(),
        }
    }

    /// `at` lent for a call into C, answering what was lent before,
    /// which [`Reentry::reclaim`] puts back when C returns.
    pub fn lend(&self, at: NonZeroUsize) -> usize {
        self.lent.swap(at.get(), Ordering::AcqRel)
    }

    /// `at` taken back once C returned, `before` lent again: waiting,
    /// where a callback another thread called still holds it, until
    /// that callback gives it back, so the request is never reached
    /// from two places.
    pub fn reclaim(&self, at: NonZeroUsize, before: usize) {
        while self
            .lent
            .compare_exchange(at.get(), before, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            std::thread::yield_now();
        }
    }

    /// The address lent, taken by one callback: none while nothing is
    /// lent, or another callback holds it.
    pub fn take_lent(&self) -> Option<NonZeroUsize> {
        NonZeroUsize::new(self.lent.swap(0, Ordering::AcqRel))
    }

    /// What [`Reentry::take_lent`] took, given back as the callback
    /// returns to C.
    pub fn give_back(&self, at: NonZeroUsize) {
        self.lent.store(at.get(), Ordering::Release);
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
}

#[cfg(test)]
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

    /// A lent address is taken by one callback at a time and given
    /// back; a deferral raises the interrupt and is taken in order.
    #[test]
    fn a_lent_request_is_taken_once_and_a_deferral_raises() {
        let interrupt = Interrupt::default();
        let reentry = Reentry::new(&interrupt);
        assert_eq!(reentry.take_lent(), None);
        let at = NonZeroUsize::MIN.saturating_add(7);
        assert_eq!(reentry.lend(at), 0);
        assert_eq!(reentry.take_lent(), Some(at));
        assert_eq!(reentry.take_lent(), None);
        reentry.give_back(at);
        reentry.reclaim(at, 0);
        assert_eq!(reentry.take_lent(), None);
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
}
