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

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use php_heap::post::{Pool, Poster, Registered, WakeSet};

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
}
