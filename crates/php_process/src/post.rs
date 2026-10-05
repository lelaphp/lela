//! A post written from an asynchronous signal handler and read by safe code
//! (design §5.8, revision 38; `unsafe.homes`): a fixed pool of records
//! that a poster interrupting anything, another poster included, can
//! fill without allocating or taking a mutex, and a wake set that raises
//! every interrupt registered on it. The mechanism knows nothing of what
//! it carries or why: the record is the caller's `Copy` type, what a
//! poster puts in it is the caller's closure, and which records a reader
//! takes is the caller's predicate. `php_engine` holds the one instance
//! and its rules, and an extension's `src/ffi/` the handler that
//! writes; what runs in an asynchronous signal handler is [`Poster::post`]
//! and the poster's own closure, and nothing else.
//!
//! Each operation is a bounded sequence of atomic operations over static
//! storage, which is what makes it async-signal-safe: a poster claims a
//! free slot by a compare-and-swap, so two posters, or a poster that
//! interrupts a reader, never touch one slot; a reader claims a filled
//! slot the same way. A poster that finds every slot claimed forgets its
//! record, and the wake it raises still stands.

use std::any::Any;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};

/// What one slot holds, a state machine over one byte.
mod state {
    /// Nothing holds the slot.
    pub(super) const FREE: u8 = 0;
    /// A poster is filling it.
    pub(super) const WRITING: u8 = 1;
    /// It holds a record no reader has claimed.
    pub(super) const READY: u8 = 2;
    /// A reader is reading it.
    pub(super) const READING: u8 = 3;
}

/// One slot of a [`Pool`]: its state, when its record arrived, and the
/// record, written while the state is `WRITING` and read while it is
/// `READING`, by whoever moved it there and nobody else.
struct Slot<T> {
    state: AtomicU8,
    arrived: AtomicU64,
    record: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Slot<T> {
    const fn free() -> Slot<T> {
        Slot {
            state: AtomicU8::new(state::FREE),
            arrived: AtomicU64::new(0),
            record: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

/// `N` records of `T`, posted from anywhere, a signal handler included and taken, in the order
/// they arrived, from safe code.
pub struct Pool<T, const N: usize> {
    slots: [Slot<T>; N],
    /// The order records arrive in, what [`Pool::take`] hands them back by.
    arrivals: AtomicU64,
    /// What a poster's closure starts from.
    blank: T,
}

// SAFETY: a slot's record is reached only by the one caller whose
// compare-and-swap moved its state to `WRITING` or `READING`, and the
// release store that ends that access is acquired by the next claim; a
// `T` that is `Send` may be written on one thread and read on another.
unsafe impl<T: Copy + Send, const N: usize> Sync for Pool<T, N> {}

impl<T: Copy, const N: usize> std::fmt::Debug for Pool<T, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("slots", &N)
            .finish_non_exhaustive()
    }
}

impl<T: Copy, const N: usize> Pool<T, N> {
    /// Every slot free; a poster's closure is handed `blank` to fill.
    #[must_use]
    pub const fn new(blank: T) -> Pool<T, N> {
        Pool {
            slots: [const { Slot::free() }; N],
            arrivals: AtomicU64::new(0),
            blank,
        }
    }

    /// One record into a free slot, `fill` writing it over the blank:
    /// callable from an asynchronous signal handler, since it allocates
    /// nothing and waits on nothing. `false` where every slot was taken,
    /// and then the record is forgotten.
    pub fn post(&self, fill: impl FnOnce(&mut T)) -> bool {
        let arrived = self.arrivals.fetch_add(1, Ordering::Relaxed);
        let Some(slot) = self.slots.iter().find(|slot| {
            slot.state
                .compare_exchange(
                    state::FREE,
                    state::WRITING,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
        }) else {
            return false;
        };
        // SAFETY: this call moved the slot from `FREE` to `WRITING`, so
        // no other poster or reader reaches the record until the release
        // store below.
        let record = unsafe { &mut *slot.record.get() };
        fill(record.write(self.blank));
        slot.arrived.store(arrived, Ordering::Relaxed);
        slot.state.store(state::READY, Ordering::Release);
        true
    }

    /// Every record `want` accepts handed to `each` in the order it
    /// arrived, its slot freed first; a record `want` refuses stays for
    /// another reader. A slot a poster is still filling, or another
    /// reader holds, is left for the next take. Allocates nothing.
    pub fn take(&self, want: impl Fn(&T) -> bool, mut each: impl FnMut(T)) {
        let mut held = [false; N];
        for (slot, held) in self.slots.iter().zip(&mut held) {
            if slot
                .state
                .compare_exchange(
                    state::READY,
                    state::READING,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            // SAFETY: this call moved the slot from `READY` to `READING`,
            // so it alone reaches the record.
            let cell = unsafe { *slot.record.get() };
            // SAFETY: the poster that set `READY` initialised the record.
            let record = unsafe { cell.assume_init() };
            if want(&record) {
                *held = true;
            } else {
                slot.state.store(state::READY, Ordering::Release);
            }
        }
        while let Some(at) = (0..N)
            .filter(|&at| held[at])
            .min_by_key(|&at| self.slots[at].arrived.load(Ordering::Relaxed))
        {
            held[at] = false;
            let slot = &self.slots[at];
            // SAFETY: as above; the slot is still `READING` and this call's.
            let cell = unsafe { *slot.record.get() };
            // SAFETY: as above.
            let record = unsafe { cell.assume_init() };
            slot.state.store(state::FREE, Ordering::Release);
            each(record);
        }
    }
}

/// Up to `N` interrupts, each raised by [`WakeSet::raise`] while the
/// [`Registered`] that put it here stands.
pub struct WakeSet<const N: usize> {
    raised: [AtomicPtr<AtomicBool>; N],
    /// How many raises are between reading an entry and done with it:
    /// what a [`Registered`] waits out before its interrupt may go.
    raising: AtomicUsize,
}

impl<const N: usize> std::fmt::Debug for WakeSet<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WakeSet")
            .field("entries", &N)
            .finish_non_exhaustive()
    }
}

impl<const N: usize> Default for WakeSet<N> {
    fn default() -> Self {
        WakeSet::new()
    }
}

impl<const N: usize> WakeSet<N> {
    /// No interrupt registered.
    #[must_use]
    pub const fn new() -> WakeSet<N> {
        WakeSet {
            raised: [const { AtomicPtr::new(std::ptr::null_mut()) }; N],
            raising: AtomicUsize::new(0),
        }
    }

    /// Every registered interrupt raised: callable from an asynchronous
    /// signal handler, since it is atomic operations over the set alone.
    pub fn raise(&self) {
        self.raising.fetch_add(1, Ordering::SeqCst);
        for entry in &self.raised {
            let raised = entry.load(Ordering::SeqCst);
            if raised.is_null() {
                continue;
            }
            // SAFETY: a non-null entry points into the allocation a
            // `Registered` holds an owner of, and its drop clears the
            // entry and then waits for `raising` to reach zero before
            // letting the owner go; this raise counted itself in before
            // reading the entry, so the allocation outlives the store.
            let raised = unsafe { &*raised };
            raised.store(true, Ordering::Release);
        }
        self.raising.fetch_sub(1, Ordering::SeqCst);
    }

    /// `owner`'s interrupt raised at every [`WakeSet::raise`] until the
    /// [`Registered`] is dropped; `None` where every entry is taken.
    pub fn register<O>(&self, owner: Arc<O>) -> Option<Registered<'_>>
    where
        O: AsRef<AtomicBool> + Send + Sync + 'static,
    {
        let raised = std::ptr::from_ref::<AtomicBool>((*owner).as_ref()).cast_mut();
        let entry = self.raised.iter().find(|entry| {
            entry
                .compare_exchange(
                    std::ptr::null_mut(),
                    raised,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
        })?;
        Some(Registered {
            entry,
            raising: &self.raising,
            _owner: owner,
        })
    }
}

/// One interrupt on a [`WakeSet`]; dropping it takes the interrupt off.
pub struct Registered<'a> {
    entry: &'a AtomicPtr<AtomicBool>,
    raising: &'a AtomicUsize,
    /// What keeps the interrupt's word alive; read by nothing, dropped
    /// after the entry is cleared and the raises are waited out.
    _owner: Arc<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for Registered<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registered").finish_non_exhaustive()
    }
}

impl Drop for Registered<'_> {
    /// The entry cleared, then every raise that may have read it waited
    /// out, and only then the owner let go.
    fn drop(&mut self) {
        self.entry.store(std::ptr::null_mut(), Ordering::SeqCst);
        while self.raising.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
    }
}

/// The poster's face of one [`Pool`] and one [`WakeSet`]: a record
/// posted and every registered interrupt raised, and nothing a reader
/// does. What an asynchronous handler is handed, so the handler can do
/// nothing else to either.
pub struct Poster<'a, T, const N: usize, const W: usize> {
    pool: &'a Pool<T, N>,
    wake: &'a WakeSet<W>,
}

impl<T, const N: usize, const W: usize> std::fmt::Debug for Poster<'_, T, N, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Poster").finish_non_exhaustive()
    }
}

impl<'a, T: Copy, const N: usize, const W: usize> Poster<'a, T, N, W> {
    /// The face over `pool` and `wake`.
    #[must_use]
    pub const fn new(pool: &'a Pool<T, N>, wake: &'a WakeSet<W>) -> Poster<'a, T, N, W> {
        Poster { pool, wake }
    }

    /// [`Pool::post`], then [`WakeSet::raise`] whether or not the pool
    /// had room: async-signal-safe, as each of the two is.
    pub fn post(&self, fill: impl FnOnce(&mut T)) {
        self.pool.post(fill);
        self.wake.raise();
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

    fn taken<const N: usize>(pool: &Pool<u32, N>, want: impl Fn(&u32) -> bool) -> Vec<u32> {
        let mut out = Vec::new();
        pool.take(want, |record| out.push(record));
        out
    }

    /// Records come back in the order they arrived, a take of some leaves
    /// the rest, and a record taken is gone.
    #[test]
    fn a_take_hands_back_what_it_wants_in_arrival_order() {
        let pool: Pool<u32, 4> = Pool::new(0);
        for n in [30, 10, 20] {
            assert!(pool.post(|record| *record = n));
        }
        assert_eq!(taken(&pool, |&n| n != 10), vec![30, 20]);
        assert_eq!(taken(&pool, |_| true), vec![10]);
        assert_eq!(taken(&pool, |_| true), vec![]);
    }

    /// A full pool forgets the record it has no room for, and a take
    /// frees room for the next.
    #[test]
    fn a_full_pool_forgets_and_a_take_makes_room() {
        let pool: Pool<u32, 2> = Pool::new(7);
        assert!(pool.post(|_| {}));
        assert!(pool.post(|record| *record += 1));
        assert!(!pool.post(|record| *record = 99));
        assert_eq!(taken(&pool, |_| true), vec![7, 8]);
        assert!(pool.post(|record| *record = 9));
        assert_eq!(taken(&pool, |_| true), vec![9]);
    }

    /// A raise reaches every registered interrupt, and a dropped
    /// registration is raised no more.
    #[test]
    fn a_raise_reaches_what_is_registered_and_nothing_dropped() {
        let set: WakeSet<2> = WakeSet::new();
        let one = Arc::new(Word(AtomicBool::new(false)));
        let two = Arc::new(Word(AtomicBool::new(false)));
        let three = Arc::new(Word(AtomicBool::new(false)));
        let first = set.register(Arc::clone(&one));
        let second = set.register(Arc::clone(&two));
        assert!(first.is_some() && second.is_some());
        assert!(
            set.register(Arc::clone(&three)).is_none(),
            "the set is full"
        );
        set.raise();
        assert!(one.0.load(Ordering::Acquire) && two.0.load(Ordering::Acquire));
        drop(first);
        one.0.store(false, Ordering::Relaxed);
        set.raise();
        assert!(
            !one.0.load(Ordering::Acquire),
            "a dropped entry is raised no more"
        );
    }

    /// The poster's face posts and raises in one call.
    #[test]
    fn a_poster_posts_and_raises() {
        static POOL: Pool<u32, 2> = Pool::new(0);
        static WAKE: WakeSet<1> = WakeSet::new();
        static POSTER: Poster<'static, u32, 2, 1> = Poster::new(&POOL, &WAKE);
        let word = Arc::new(Word(AtomicBool::new(false)));
        let registered = WAKE.register(Arc::clone(&word));
        POSTER.post(|record| *record = 5);
        assert!(word.0.load(Ordering::Acquire));
        assert_eq!(taken(&POOL, |_| true), vec![5]);
        drop(registered);
    }
}
