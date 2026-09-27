//! The process's signal post (design §5.8, §7.2 revision 35;
//! `unsafe.homes`): what a signal handler records and whom it raises,
//! in the one place of the engine where a signal handler may reach.
//!
//! php's pcntl keeps its queue and its `async_signals` word in
//! `pcntl_globals` and its handler writes them from whatever the process
//! was doing (`pcntl_signal_handler`, `ext/pcntl/pcntl.c:1333`). Signal
//! dispositions are the process's, so what a handler writes is the
//! process's too, and an extension holds no atomic (ADR-0005): the
//! record is here, engine-owned, and [`post`] is the handler an
//! extension installs. It does two things and nothing else, each one
//! async-signal-safe because each is a bounded sequence of atomic operations:
//!
//! - the signal and what its `siginfo_t` said go into one free slot of a
//!   fixed pool, stamped with the order they arrived in, as php appends
//!   to its queue; with every slot taken the signal is forgotten, as
//!   php forgets one when its spares are spent;
//! - every interrupt handed to [`receive`] is raised, which is php's
//!   `EG(vm_interrupt) = true` under `pcntl_async_signals(true)`.
//!
//! [`take`] is the other side, php's `pcntl_signal_dispatch` taking the
//! queue: the slots of the numbers the caller asks for, freed, in the
//! order they arrived. A caller asks for the numbers its own table
//! holds, so two requests of one process never take each other's.

use std::any::Any;
use std::ffi::{c_int, c_void};
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

/// How many signals the post holds between two takes: php's spares are
/// `NSIG` of them, allocated at the first `pcntl_signal`, and 64 is at
/// least that on every target the engine builds for.
const SLOTS: usize = 64;

/// How many interrupts one process raises at a signal: one per request
/// that turned asynchronous delivery on and has not turned it off.
const RECEIVERS: usize = 64;

/// A slot nothing holds.
const FREE: u8 = 0;
/// A slot a handler is filling.
const WRITING: u8 = 1;
/// A slot holding a signal no take has claimed.
const READY: u8 = 2;
/// A slot a take is reading.
const READING: u8 = 3;

/// One signal, as [`post`] stores it: each field its own atomic, so a
/// handler that interrupts another handler, or runs on another thread,
/// fills a slot of its own and none waits on another.
struct Slot {
    state: AtomicU8,
    arrived: AtomicU64,
    number: AtomicI32,
    errno: AtomicI32,
    code: AtomicI32,
    pid: AtomicI32,
    uid: AtomicU32,
    status: AtomicI32,
    addr: AtomicUsize,
}

impl Slot {
    const fn empty() -> Slot {
        Slot {
            state: AtomicU8::new(FREE),
            arrived: AtomicU64::new(0),
            number: AtomicI32::new(0),
            errno: AtomicI32::new(0),
            code: AtomicI32::new(0),
            pid: AtomicI32::new(0),
            uid: AtomicU32::new(0),
            status: AtomicI32::new(0),
            addr: AtomicUsize::new(0),
        }
    }
}

static POOL: [Slot; SLOTS] = [const { Slot::empty() }; SLOTS];

/// The order signals arrive in, what [`take`] sorts by.
static ARRIVALS: AtomicU64 = AtomicU64::new(0);

/// The interrupts [`post`] raises, each an `AtomicBool` a [`Receiving`]
/// keeps alive while it stands here.
static RAISED: [AtomicPtr<AtomicBool>; RECEIVERS] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; RECEIVERS];

/// How many [`post`]s are between reading [`RAISED`] and done with what
/// they read: what a [`Receiving`] waits out before its interrupt may go.
static RAISING: AtomicUsize = AtomicUsize::new(0);

/// One signal a [`take`] handed back: the number and what `siginfo_t`
/// said of it, which php's `pcntl_siginfo_to_zval` renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posted {
    pub number: i32,
    pub errno: i32,
    pub code: i32,
    pub pid: i32,
    pub uid: u32,
    pub status: i32,
    pub addr: usize,
}

/// The signal handler, installed with `SA_SIGINFO`: the signal into a
/// free slot and every receiver's interrupt raised.
pub extern "C" fn post(number: c_int, info: *mut libc::siginfo_t, _context: *mut c_void) {
    let arrived = ARRIVALS.fetch_add(1, Ordering::Relaxed);
    let posted = read_info(number, info);
    if let Some(slot) = POOL.iter().find(|slot| {
        slot.state
            .compare_exchange(FREE, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }) {
        slot.arrived.store(arrived, Ordering::Relaxed);
        slot.number.store(posted.number, Ordering::Relaxed);
        slot.errno.store(posted.errno, Ordering::Relaxed);
        slot.code.store(posted.code, Ordering::Relaxed);
        slot.pid.store(posted.pid, Ordering::Relaxed);
        slot.uid.store(posted.uid, Ordering::Relaxed);
        slot.status.store(posted.status, Ordering::Relaxed);
        slot.addr.store(posted.addr, Ordering::Relaxed);
        slot.state.store(READY, Ordering::Release);
    }
    RAISING.fetch_add(1, Ordering::SeqCst);
    for receiver in &RAISED {
        let raised = receiver.load(Ordering::SeqCst);
        if raised.is_null() {
            continue;
        }
        // SAFETY: a non-null entry points into the allocation a
        // `Receiving` holds an owner of, and its drop clears the entry
        // and then waits for `RAISING` to reach zero before letting the
        // owner go; this post counted itself in before reading the entry,
        // so the allocation outlives the store.
        let raised = unsafe { &*raised };
        raised.store(true, Ordering::Release);
    }
    RAISING.fetch_sub(1, Ordering::SeqCst);
}

/// What `siginfo_t` says, read as php's `pcntl_siginfo_to_zval` reads
/// it; nothing but the number where the kernel handed no structure.
fn read_info(number: c_int, info: *mut libc::siginfo_t) -> Posted {
    let mut posted = Posted {
        number,
        errno: 0,
        code: 0,
        pid: 0,
        uid: 0,
        status: 0,
        addr: 0,
    };
    if info.is_null() {
        return posted;
    }
    // SAFETY: the kernel hands an `SA_SIGINFO` handler a `siginfo_t` that
    // is valid for the handler's duration, and the pointer is not null.
    let info = unsafe { &*info };
    posted.errno = info.si_errno;
    posted.code = info.si_code;
    union_fields(info, &mut posted);
    posted
}

/// The fields Apple's `siginfo_t` carries as plain members.
#[cfg(target_vendor = "apple")]
fn union_fields(info: &libc::siginfo_t, posted: &mut Posted) {
    posted.pid = info.si_pid;
    posted.uid = info.si_uid;
    posted.status = info.si_status;
    posted.addr = info.si_addr as usize;
}

/// The fields Linux's `siginfo_t` keeps in its union, read through the
/// accessors `libc` gives it.
#[cfg(not(target_vendor = "apple"))]
fn union_fields(info: &libc::siginfo_t, posted: &mut Posted) {
    // SAFETY: the union is plain integers over the structure the kernel
    // filled; reading a member the signal did not set reads bytes, never
    // an invalid value, and php reads them as its switch on the number
    // picks.
    posted.pid = unsafe { info.si_pid() };
    // SAFETY: as above.
    posted.uid = unsafe { info.si_uid() };
    // SAFETY: as above.
    posted.status = unsafe { info.si_status() };
    // SAFETY: as above.
    posted.addr = unsafe { info.si_addr() } as usize;
}

/// The signals posted under a number `wanted` asks for, their slots
/// freed, in the order they arrived: php's dispatch taking its queue.
/// A slot a handler is still filling is left for the next take, which
/// the interrupt that handler raises asks for.
pub fn take(wanted: impl Fn(i32) -> bool) -> Vec<Posted> {
    let mut taken: Vec<(u64, Posted)> = Vec::new();
    for slot in &POOL {
        if slot.state.load(Ordering::Acquire) != READY
            || !wanted(slot.number.load(Ordering::Relaxed))
        {
            continue;
        }
        if slot
            .state
            .compare_exchange(READY, READING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        let posted = Posted {
            number: slot.number.load(Ordering::Relaxed),
            errno: slot.errno.load(Ordering::Relaxed),
            code: slot.code.load(Ordering::Relaxed),
            pid: slot.pid.load(Ordering::Relaxed),
            uid: slot.uid.load(Ordering::Relaxed),
            status: slot.status.load(Ordering::Relaxed),
            addr: slot.addr.load(Ordering::Relaxed),
        };
        taken.push((slot.arrived.load(Ordering::Relaxed), posted));
        slot.state.store(FREE, Ordering::Release);
    }
    taken.sort_by_key(|(arrived, _)| *arrived);
    taken.into_iter().map(|(_, posted)| posted).collect()
}

/// An interrupt [`post`] raises while this stands; dropping it takes
/// the interrupt off the post.
pub struct Receiving {
    entry: usize,
    /// What keeps the interrupt's word alive; read by nothing, dropped
    /// after the entry is cleared and the posts are waited out.
    _owner: Arc<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for Receiving {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiving")
            .field("entry", &self.entry)
            .finish_non_exhaustive()
    }
}

/// Hand `owner`'s interrupt to the post, which raises it at every signal
/// until the [`Receiving`] is dropped. `None` when every entry is taken,
/// which is more requests with asynchronous delivery on than one
/// process of php ever has.
pub fn receive<T: AsRef<AtomicBool> + Send + Sync + 'static>(owner: Arc<T>) -> Option<Receiving> {
    let raised = std::ptr::from_ref::<AtomicBool>((*owner).as_ref()).cast_mut();
    let entry = RAISED.iter().position(|receiver| {
        receiver
            .compare_exchange(
                std::ptr::null_mut(),
                raised,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    })?;
    Some(Receiving {
        entry,
        _owner: owner as Arc<dyn Any + Send + Sync>,
    })
}

impl Drop for Receiving {
    /// The entry cleared, then every post that may have read it waited
    /// out, and only then the owner let go.
    fn drop(&mut self) {
        if let Some(receiver) = RAISED.get(self.entry) {
            receiver.store(std::ptr::null_mut(), Ordering::SeqCst);
        }
        while RAISING.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Word(AtomicBool);

    impl AsRef<AtomicBool> for Word {
        fn as_ref(&self) -> &AtomicBool {
            &self.0
        }
    }

    /// A post fills a slot and raises every receiver; a take of its
    /// number frees the slot, and a take of another number leaves it.
    #[test]
    fn a_post_is_taken_by_its_number_and_raises_its_receivers() {
        let word = Arc::new(Word(AtomicBool::new(false)));
        let receiving = receive(Arc::clone(&word));
        assert!(receiving.is_some());
        post(1_000, std::ptr::null_mut(), std::ptr::null_mut());
        post(1_001, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(word.0.load(Ordering::Acquire));
        assert_eq!(take(|n| n == 1_002), vec![]);
        let taken = take(|n| n == 1_000 || n == 1_001);
        assert_eq!(
            taken.iter().map(|p| p.number).collect::<Vec<_>>(),
            vec![1_000, 1_001]
        );
        assert_eq!(take(|n| n == 1_000), vec![]);
        drop(receiving);
        word.0.store(false, Ordering::Relaxed);
        post(1_003, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(
            !word.0.load(Ordering::Acquire),
            "a dropped receiver is raised no more"
        );
        assert_eq!(take(|n| n == 1_003).len(), 1);
    }
}
