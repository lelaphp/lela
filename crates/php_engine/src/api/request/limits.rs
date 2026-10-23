//! The limits of a request: its interrupt (design §5.8), its deadlines
//! on the engine's timer, which raises it for `max_execution_time` and
//! for every periodic interrupt an extension started, `set_time_limit()`,
//! the interrupt hooks the bundle supplied, the thread's stack limit,
//! the running execution's trace number, php's `EG(jit_trace_num)`, and
//! `fork(2)`, which joins the timer's thread around it.
//! The interrupt, the deadlines, the stack limit and the trace number
//! are [`LimitsState`]'s, whose fields no file but this one reads.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use php_compile::{Reentered, StackLimit};

use super::{Forked, Periodic, Receiving, Reentry, Request};
use crate::api::abi::{Beneath, chain};
use crate::tier::TraceNum;

/// The request's limits facet (design §4.4): what bounds the request: its
/// interrupt and the periodic interrupts, the time limit, the machine
/// stack's bounds, a callback C makes from another thread, and `fork`.
/// [`Request::limits`] reads it and [`Request::limits_mut`] changes it;
/// either is the whole request, since nearly every operation can serve
/// PHP code.
pub struct Limits<R>(pub(crate) R);

impl Request {
    /// The limits facet, to read.
    #[must_use]
    pub fn limits(&self) -> Limits<&Request> {
        Limits(self)
    }

    /// The limits facet, to change.
    pub fn limits_mut(&mut self) -> Limits<&mut Request> {
        Limits(self)
    }
}

/// One request's interrupt (design §5.8; `CONTEXT.md`, Interrupt), as
/// the thread that raises it sees it: `Send` and `Sync`, and cloned so
/// that the engine's timer and the thread serving the request share
/// one. Raising is all a setter does; acting on it is the loop's,
/// at a back-edge, at a frame's activation, after every `Flow::Resume`
/// and after an internal call returns (design §5.8), and nothing else
/// touches it.
#[derive(Debug, Clone, Default)]
pub struct Interrupt(Arc<State>);

impl Interrupt {
    /// What signal delivery, a periodic interrupt and a deferred FFI
    /// callback ([`Reentry`]) raise it with: the loop serves each
    /// extension's interrupt hook at its next check, which is php's
    /// `zend_interrupt_function`; pcntl's dispatches what was delivered
    /// to its PHP handlers, and ffi's serves the callbacks deferred.
    pub fn raise(&self) {
        self.0.raised.store(true, Ordering::Release);
    }

    /// Whether it is raised: what the loop tests at its four sites and
    /// what compiled code tests once per loop iteration through the
    /// tier's `ExitCtx` (`jit-design.md` §4.3).
    #[must_use]
    pub fn raised(&self) -> &AtomicBool {
        &self.0.raised
    }

    /// What the engine's execution timer raises it with,
    /// `max_execution_time` having elapsed: the seconds it was armed
    /// with, then the two words `zend_timeout_handler` stores
    /// (`Z/Zend/zend_execute_API.c:1516`). The seconds go down first and
    /// the store that raises it releases them, because php's handler
    /// runs on the interrupted thread and this timer does not.
    pub(crate) fn elapsed(&self, seconds: i64) {
        self.0.seconds.store(seconds, Ordering::Relaxed);
        self.0.timed_out.store(true, Ordering::Relaxed);
        self.0.raised.store(true, Ordering::Release);
    }

    /// Whether the timer's timeout is still unread, php's
    /// `EG(timed_out)`: raised and not yet taken by a check.
    pub(crate) fn timed_out(&self) -> bool {
        self.0.timed_out.load(Ordering::Acquire)
    }

    /// What unsetting the execution timer does beside stopping it
    /// (`zend_unset_timeout`, `Z/Zend/zend_execute_API.c:1657-1685`): the
    /// timer's word is cleared, so a deadline the old timer reached as
    /// it was being unset is not the fatal the new one raises.
    pub(crate) fn timer_unset(&self) {
        self.0.timed_out.store(false, Ordering::Relaxed);
    }

    /// How many hold this interrupt: its request, the request's
    /// deadlines, the engine's schedule while the request has a deadline
    /// there, and whoever else cloned it.
    #[cfg(test)]
    pub(crate) fn holders(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// Hand this interrupt to the process's signal record, which raises
    /// it at every signal the process takes until the [`Receiving`] is
    /// dropped: php's `pcntl_signal_handler` storing `EG(vm_interrupt)`
    /// under `pcntl_async_signals(true)` (design §5.8). `None` when the
    /// record holds as many interrupts as it can.
    #[must_use]
    pub fn receive_signals(&self) -> Option<Receiving> {
        crate::signal::receive(Arc::clone(&self.0))
    }

    /// What a check that found it raised reads: it is cleared, and the
    /// seconds come back when the execution timer was what raised it,
    /// both as php clears them (`Z/Zend/zend_vm_def.h:10526`, `:10537`).
    /// Clearing it acquires what the setter released, so the seconds the
    /// text names are the ones the timer was armed with; the loop's own
    /// test stays a relaxed load, because this is where the two threads
    /// meet.
    pub(crate) fn taken(&self) -> Option<i64> {
        self.0.raised.swap(false, Ordering::Acquire);
        self.0
            .timed_out
            .swap(false, Ordering::Relaxed)
            .then(|| self.0.seconds.load(Ordering::Relaxed))
    }
}

/// What an [`Interrupt`] holds, one per request: the atomic the loop
/// tests, which of the two setters raised it, and the seconds php's
/// timeout text names.
#[derive(Debug, Default)]
struct State {
    raised: AtomicBool,
    /// php's `EG(timed_out)`.
    timed_out: AtomicBool,
    /// php's `EG(timeout_seconds)`.
    seconds: AtomicI64,
}

impl AsRef<AtomicBool> for State {
    /// The word a posted signal raises: the one the loop tests.
    fn as_ref(&self) -> &AtomicBool {
        &self.raised
    }
}

/// The time limit suspended while this stands
/// ([`Limits::suspend_time_limit`], #1814), over the request or over
/// what holds it, an observer's frame ([`Observed`]): what it derefs to,
/// so the handler that holds it keeps everything it had. Its drop is
/// the resume.
///
/// [`Observed`]: crate::api::abi::Observed
#[must_use = "the time limit is resumed when the guard is dropped, at once where it is not held"]
pub struct TimeLimitSuspended<'a, T: AsMut<Request> = Request> {
    held: &'a mut T,
}

impl<'a, T: AsMut<Request>> TimeLimitSuspended<'a, T> {
    /// The time limit of the request `held` reaches suspended, until the
    /// guard is dropped.
    pub(crate) fn over(held: &'a mut T) -> Self {
        held.as_mut().limits.deadlines.suspend();
        TimeLimitSuspended { held }
    }
}

impl<T: AsMut<Request>> Deref for TimeLimitSuspended<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.held
    }
}

impl<T: AsMut<Request>> DerefMut for TimeLimitSuspended<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.held
    }
}

impl<T: AsMut<Request>> Drop for TimeLimitSuspended<'_, T> {
    /// The resume: the time the limit had left counts on from here.
    fn drop(&mut self) {
        self.held.as_mut().limits.deadlines.resume();
    }
}

impl AsMut<Request> for Request {
    /// The request a [`TimeLimitSuspended`] over the request suspends.
    fn as_mut(&mut self) -> &mut Request {
        self
    }
}

/// The interrupt, the deadlines, the stack limit and the trace number,
/// whose fields no file but this one reads.
pub(super) struct LimitsState {
    /// The interrupt, shared with whatever thread sets it (design §5.8).
    interrupt: Interrupt,
    /// The request's deadlines on the engine's timer: the execution
    /// timer's while `max_execution_time` is not zero and every periodic
    /// interrupt's an extension started; dropping the request takes them
    /// out of the engine's schedule.
    deadlines: crate::interrupt::Deadlines,
    /// The thread's stack limit, php's `EG(stack_limit)`, worked out
    /// once at request start on the thread the request runs on
    /// (`zend_call_stack_init`) from the two `zend.*_stack_size`
    /// entries; what the compiler is held to (design §5.6).
    stack_limit: StackLimit,
    /// The stack the request's first scope of the loop was measured to
    /// take ([`Request::note_scope_cost`]), none before it.
    scope_cost: Option<std::num::NonZeroUsize>,
    /// php's `EG(jit_trace_num)`, the running execution's trace number
    /// ([`crate::tier::trace_num`]), kept here beside the stack limit
    /// because a fiber switch swaps both ([`Request::switch_execution`]).
    trace_num: TraceNum,
    /// How a function C calls back reaches the request
    /// ([`Limits::reentry`]), made at the first ask and closed as the
    /// request is dropped.
    reentry: Option<Arc<Reentry>>,
}

impl Drop for LimitsState {
    /// The request's end, for a thread waiting on it: what is still
    /// posted is abandoned and returns zero to C (#1640).
    fn drop(&mut self) {
        if let Some(reentry) = &self.reentry {
            reentry.close();
        }
    }
}

impl LimitsState {
    /// The limits as a request is laid on an engine whose timer is
    /// `timer`: a fresh interrupt, no deadline yet, and no stack limit
    /// until the request starts on its thread.
    pub(super) fn new(timer: &crate::interrupt::Timer) -> LimitsState {
        let interrupt = Interrupt::default();
        LimitsState {
            deadlines: crate::interrupt::Deadlines::new(timer, &interrupt),
            interrupt,
            stack_limit: StackLimit::UNCHECKED,
            scope_cost: None,
            trace_num: TraceNum::NONE,
            reentry: None,
        }
    }
}

impl Request {
    /// `zend_call_stack_init` at request start: the stack limit worked
    /// out on the thread the request runs on, from the engine's
    /// `zend.*_stack_size` entries.
    pub(super) fn start_stack_limit(&mut self) {
        self.limits.stack_limit = self.shared.stack_limit();
    }

    /// The stack limit and `other`'s change places, as the execution
    /// state does at a fiber switch ([`Request::switch_execution`]).
    pub(super) fn swap_stack_limit(&mut self, other: &mut StackLimit) {
        std::mem::swap(&mut self.limits.stack_limit, other);
    }

    /// The trace number and `other`'s change places, as the execution
    /// state does at a fiber switch ([`Request::switch_execution`]):
    /// `zend_fiber_capture_vm_state` and `zend_fiber_restore_vm_state`
    /// carry `EG(jit_trace_num)` (`Z/Zend/zend_fibers.c:131`, `:148`).
    pub(super) fn swap_trace_num(&mut self, other: &mut TraceNum) {
        std::mem::swap(&mut self.limits.trace_num, other);
    }

    /// The stack limit request start read on the thread, where the
    /// innermost native's call back into PHP has moved it under detect
    /// ([`Self::stack_reentered`]), for the compiler of whatever this
    /// request compiles and the raisers that ask it.
    pub(crate) fn stack_limit(&self) -> StackLimit {
        self.limits.stack_limit
    }

    /// A native's call back into PHP, or a generator's resume, entering
    /// the loop from this frame: the limit a native's own recursion is
    /// held to moves to php's budget below it
    /// ([`StackLimit::reentered`]), until [`Self::stack_left`] puts it
    /// back.
    pub(crate) fn stack_reentered(&mut self) -> Reentered {
        self.limits.stack_limit.reentered()
    }

    /// A scope of the loop about to serve a frame php serves in the same
    /// `execute_ex` ([`StackLimit::nested`]): the limit moved below it
    /// by what the scope takes, until [`Self::stack_left`] puts it back;
    /// none where the thread has no room for one, and on a fiber's
    /// stack, whose limit cannot move.
    /// The charge is what a scope was measured to take, `first` before
    /// one was; the room left below it at least `room` and twice the
    /// charge, for what the scope's frames call, a native's call back
    /// into the loop among them, whose frame is the size of a scope's.
    pub(crate) fn stack_nested(&mut self, first: usize, room: usize) -> Option<Reentered> {
        let charge = self
            .limits
            .scope_cost
            .map_or(first, std::num::NonZeroUsize::get);
        let room = room.max(charge.saturating_mul(2));
        self.limits.stack_limit.nested(charge, room)
    }

    /// Whether a scope of the loop is still to be measured: the first
    /// of the request.
    pub(crate) fn scope_unmeasured(&self) -> bool {
        self.limits.scope_cost.is_none()
    }

    /// `cost`, the stack the request's first scope of the loop took from
    /// the call that began it to its body: what every scope after it is
    /// charged: the loop's bodies are near one size, one scope's frame
    /// for each of the binary's tiers and observers (1,760 to 1,792
    /// bytes measured on #3051), and the room twice the charge.
    pub(crate) fn note_scope_cost(&mut self, cost: std::num::NonZeroUsize) {
        self.limits.scope_cost = Some(cost);
    }

    /// The serving [`Self::stack_reentered`] or the scope
    /// [`Self::stack_nested`] began is over.
    pub(crate) fn stack_left(&mut self, reentered: Reentered) {
        self.limits.stack_limit.left(reentered);
    }

    /// `zend_unset_timeout` then `zend_set_timeout` over
    /// `max_execution_time` as the ini table has it now: at request
    /// start, and again whenever the entry moves, which is php's
    /// `OnUpdateTimeout` at runtime. Whatever was armed is unset first,
    /// so the seconds count from here.
    pub(super) fn arm_timer(&mut self) {
        let (seconds, hard) = (self.max_execution_time(), self.ini().long(b"hard_timeout"));
        self.limits.deadlines.arm(seconds, hard);
    }

    /// `zend_unset_timeout` at `php_request_shutdown`'s fourth step,
    /// once the program's shutdown functions, destructors and buffer
    /// handlers are done: nothing the request does after it is timed,
    /// and every periodic interrupt's deadline goes with the execution
    /// timer's, so the engine's timer raises nothing more for the request
    /// from here, before any extension's `rshutdown`, whatever a host
    /// does with the request after it.
    pub(crate) fn unset_timer(&mut self) {
        self.limits.deadlines.unset();
    }

    /// The trace number of the execution the request runs on, which
    /// the tier reads through [`crate::tier::trace_num`] and the
    /// generator's resume sets aside around a body.
    pub(crate) fn trace_num(&self) -> TraceNum {
        self.limits.trace_num
    }

    /// The trace number of the execution the request runs on written
    /// ([`crate::tier::set_trace_num`]).
    pub(crate) fn set_trace_num(&mut self, trace_num: TraceNum) {
        self.limits.trace_num = trace_num;
    }

    /// The interrupt hooks' chain entered from the top: php's
    /// `zend_interrupt_function`, which the last `MINIT` to set it
    /// holds, each link handed the one it saved
    /// ([`crate::api::abi::chain::Interrupt`]); what the interrupt check
    /// serves when the execution timer was not what raised it. What the
    /// chain answers is left pending where the loop stands.
    pub(crate) fn serve_interrupt_hooks(&mut self) {
        let shared = Arc::clone(&self.shared);
        let top = Beneath::<chain::Interrupt>::top(shared.supplied.interrupt.links(), ());
        if let Err(thrown) = top.serve(self) {
            self.leave_pending(&thrown);
        }
    }

    /// `set_time_limit()`, which is `zend_alter_ini_entry_ex` over
    /// `max_execution_time` at runtime (`Z/main/main.c:1631`): the
    /// entry moved through [`Self::ini_set`], so the timer is re-armed
    /// as it is there, and `true` when the entry took the value.
    ///
    /// The value is `zend_long_to_str`'s, the interned string of one
    /// digit or a new one, which the entry keeps:
    ///
    /// ```php
    /// ini_set('from', 'x');
    /// $m = memory_get_usage(); set_time_limit(100); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); set_time_limit(5); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 32
    /// -32
    /// ```
    pub(crate) fn set_time_limit(&mut self, seconds: i64) -> bool {
        let Ok(string) = crate::api::coerce::int_to_string(self, seconds) else {
            return false;
        };
        let set = self.ini_mut().set_string(b"max_execution_time", *string);
        string.give_up(self);
        set.is_ok_and(|previous| previous.is_some())
    }

    /// `max_execution_time`, the seconds the engine's execution timer is
    /// armed for; zero, php's CLI default, arms none.
    pub(crate) fn max_execution_time(&self) -> i64 {
        i64::from(self.ini_number(b"max_execution_time", 0))
    }
}

impl<'r> Limits<&'r Request> {
    /// `zend_call_stack_overflowed(EG(stack_limit))`: whether the
    /// machine stack has come down to the limit request start worked
    /// out, `zend.max_allowed_stack_size` less `zend.reserved_stack_size`,
    /// and under detect php's own budget, the process's main thread's
    /// size, below the frame that last called back into PHP, however
    /// much larger the thread a host serves on is
    /// (`php_compile::StackLimit`, #3142).
    /// A native that walks a nest by recursing asks it at each level and
    /// raises its own refusal, as `php_url_encode_hash_ex` raises
    /// `Maximum call stack size reached.`; the one php's engine raises is
    /// [`crate::api::raising::call_stack_limit`].
    #[must_use]
    pub fn stack_overflowed(self) -> bool {
        self.0.limits.stack_limit.overflowed()
    }

    /// `EG(stack_limit)`: the address [`Limits::stack_overflowed`]
    /// compares a frame's to, the thread's or, inside a fiber, the
    /// fiber's own, and zero under `zend.max_allowed_stack_size=-1`,
    /// where nothing is checked. The thread's own bounds are
    /// [`Limits::call_stack_bounds`].
    #[must_use]
    pub fn stack_limit_address(self) -> usize {
        self.0.limits.stack_limit.limit()
    }

    /// `zend_call_stack_get`: the machine stack of the thread the request
    /// is served on, as the platform describes it, whose base and size
    /// request start works [`Limits::stack_limit_address`] out from;
    /// `None` on a thread the platform cannot describe. Inside a fiber it
    /// is still the thread's, as php's is. Read in `php_heap`'s `unsafe`
    /// home, never bound a second time in an extension.
    #[must_use]
    pub fn call_stack_bounds(self) -> Option<super::CallStack> {
        php_heap::call_stack_bounds()
    }

    /// This request's interrupt (design §5.8): what the loop tests at a
    /// back-edge, at a frame's activation, after every `Flow::Resume` and
    /// after an internal call returns, and what the engine's execution
    /// timer, signal delivery, a periodic interrupt and a deferred FFI
    /// callback raise. A `tier.lock` item, because compiled code tests
    /// the same atomic once per loop iteration through the tier's
    /// `ExitCtx` (`jit-design.md` §4.3).
    #[must_use]
    pub fn interrupt(self) -> &'r Interrupt {
        &self.0.limits.interrupt
    }
}

impl<'r> Limits<&'r mut Request> {
    /// A periodic interrupt (#1806): this request's interrupt raised
    /// `first` from now and every `every` after it, once where `every`
    /// is `None` (Excimer's one-shot `ExcimerTimer`), until
    /// [`Limits::stop_periodic`] stops it or the request reaches
    /// `php_request_shutdown`'s fourth step. A period shorter than a
    /// tenth of a millisecond is counted in tenths of a millisecond. What
    /// a sampling profiler's own timer thread does under php,
    /// Excimer's `timer_create` and its handler storing
    /// `EG(vm_interrupt)`, is the engine's here, since an extension owns
    /// no thread (ADR-0005, ADR-0010): one thread per engine serves every
    /// request's execution timer and periodic interrupts alike, parked
    /// between requests, and is joined before a fork ([`Limits::fork`])
    /// and at the engine's end.
    ///
    /// It is served through the interrupt hook the extension supplies
    /// ([`ExtensionBuilder::interrupt`]): at the loop's next check, every
    /// extension's hook is served, so a hook reads its own count with
    /// [`Limits::periods_elapsed`] and does nothing at zero, as Excimer's
    /// `excimer_timer_interrupt` serves only the timers that fired. The
    /// periods are wall-clock time, Excimer's `EXCIMER_REAL`; a child
    /// forked from the request inherits none of them, as a child inherits
    /// no POSIX timer. `None` where no thread could be started.
    ///
    /// [`ExtensionBuilder::interrupt`]: crate::api::abi::ExtensionBuilder::interrupt
    #[must_use]
    pub fn start_periodic(
        self,
        first: std::time::Duration,
        every: Option<std::time::Duration>,
    ) -> Option<Periodic> {
        self.0.limits.deadlines.start_periodic(first, every)
    }

    /// The time limit suspended (#1814) for as long as the guard this
    /// answers stands: the execution timer that `max_execution_time` and
    /// `set_time_limit()` arm stops counting, its `hard_timeout` stage
    /// with it, and the time it had left counts on from the guard's drop.
    /// What a handler that blocks takes first, a debugger waiting on its
    /// client above all: blocked past the limit, the timeout would be
    /// raised under it, and blocked `hard_timeout` seconds more, the
    /// process would end with no shutdown, where a person stepping
    /// through the program had only been slow. xdebug keeps the limit
    /// from biting by refusing `set_time_limit()` while a session is
    /// active (`zif_xdebug_set_time_limit`); here the handler says so,
    /// and the guard says when it stops.
    ///
    /// The guard derefs to the request, so the handler serves the
    /// request through it, PHP code it calls among what it does, all of
    /// it untimed; [`Observed::suspend_time_limit`] is the same guard
    /// over an observer's frame. Dropping it is the only resume, so a
    /// handler does not leave the limit suspended by returning early, nor
    /// by a fatal under it, since a `zend_bailout` here is a value the
    /// handler returns through, never a jump past its drop; a guard
    /// handed to `mem::forget` leaves the request untimed to its end.
    /// Guards nest, and the outermost one's drop is the resume.
    ///
    /// The deadline is taken out of the engine's schedule and its time
    /// left kept, so with nothing else scheduled the timer's thread waits
    /// parked until the resume, and a periodic interrupt an extension
    /// started counts on through it: a profiler's clock is its own. A
    /// limit set while it is suspended, `set_time_limit()` from PHP code
    /// the handler calls, is kept whole for the resume and, as any
    /// `set_time_limit()` does, clears a timeout raised and not yet
    /// taken, where xdebug refuses the call during a session; a timeout
    /// raised before the suspend and not cleared so is still raised at
    /// the loop's next check, since its time was up. The limit counts
    /// wall-clock time, as php's does on the oracle's platform
    /// (`ITIMER_REAL`) and in its threaded build (`CLOCK_BOOTTIME`); a
    /// handler that polls its client would spend even a CPU clock.
    ///
    /// [`Observed::suspend_time_limit`]: crate::api::abi::Observed::suspend_time_limit
    pub fn suspend_time_limit(self) -> TimeLimitSuspended<'r> {
        TimeLimitSuspended::over(self.0)
    }

    /// The periods of `periodic` elapsed since the last time this was
    /// asked, the ones the request was too busy to be interrupted for
    /// among them, as Excimer hands its callback the event count, POSIX's
    /// overruns plus one; zero for one stopped.
    pub fn periods_elapsed(self, periodic: Periodic) -> u64 {
        self.0.limits.deadlines.periods_elapsed(periodic)
    }

    /// `periodic` stopped, with the periods it had not been asked for,
    /// as Excimer's `excimer_timer_stop` deletes its timer.
    pub fn stop_periodic(self, periodic: Periodic) {
        self.0.limits.deadlines.stop_periodic(periodic);
    }

    /// `fork(2)` from this request, the engine's one home for it (#1807, as
    /// #1779 ruled), which `pcntl_fork` asks: the process copied with this
    /// request alone inside the engine and no thread of the engine's alive
    /// (ADR-0010, Threads), returning in both as [`Forked::Parent`] with the
    /// child's id and as [`Forked::Child`]. The engine's timer thread is joined
    /// before it, as a host's fork joins it, and after it started again in the
    /// parent over the same deadlines, which loses none of them, as php's
    /// interval timer counts on across the fork; in the child what php's
    /// `php_child_init` restores is restored: every deadline is dropped, the
    /// execution timer's and each periodic interrupt's, as a child inherits no
    /// interval timer and no POSIX timer, so it runs untimed until it sets a
    /// limit of its own, in its copy of this request.
    ///
    /// Two and a half seconds of a three-second limit spent before the fork
    /// leave a half after it. A limit counted again from the fork could end
    /// the program no sooner than three seconds after the fork, so the
    /// verdict asks only that, with the instant read before `pcntl_fork`:
    /// no delay can make a limit counted again pass as one counted from
    /// before, and one counted from before passes unless its timeout is
    /// taken two and a half seconds late. The limit counts wall-clock time
    /// here and in the oracle, whose build on this platform arms
    /// `ITIMER_REAL` (`Z/Zend/zend_execute_API.c:1611-1614`), so wall time
    /// is the clock it counts. The child is left to end on its own, so the
    /// parent waits on nothing a loaded machine stretches before its loop.
    /// The fatal's line is masked, as php's tests mask it. The time before
    /// the fork is spent in a loop that asks the clock once per hundred
    /// thousand passes, so the natives a witness records stay a few
    /// thousand however fast the loop runs.
    ///
    /// ```php
    /// $start = microtime(true);
    /// set_time_limit(3);
    /// register_shutdown_function(function () use (&$forked, &$child) {
    ///     if ($child === 0) { return; }
    ///     echo microtime(true) - $forked < 3 ? "counted from before the fork\n" : "counted again from the fork\n";
    /// });
    /// while (microtime(true) - $start < 2.5) { for ($i = 0; $i < 100000; $i++); }
    /// $forked = microtime(true);
    /// if (($child = pcntl_fork()) === 0) { exit(0); }
    /// for (;;) {}
    /// ```
    /// ```expect exit=255 process wall=3s
    /// Fatal error: Maximum execution time of 3 seconds exceeded in %s on line %d
    /// counted from before the fork
    /// ```
    ///
    /// # Errors
    ///
    /// `EBUSY` where another request is in flight in the process, on a
    /// host that serves on threads, whose other threads would leave the
    /// child what they held; otherwise the OS error `fork(2)` failed with,
    /// `EAGAIN` and `ENOMEM` among them, and no process was made.
    pub fn fork(self) -> std::io::Result<Forked> {
        crate::fork::fork_request()
    }

    /// How a function C calls back reaches this request (#1638, #1640):
    /// one per request, made at the first ask, which ext/ffi makes at
    /// its first call into C or callback, and closed when the request
    /// ends.
    pub fn reentry(self) -> Arc<Reentry> {
        let interrupt = &self.0.limits.interrupt;
        Arc::clone(
            self.0
                .limits
                .reentry
                .get_or_insert_with(|| Arc::new(Reentry::new(interrupt))),
        )
    }

    /// `wait` called with the request lent to a callback C makes from
    /// another thread meanwhile, which takes it, served on that thread,
    /// and gives it back before this returns (#1640): what a call into C
    /// and an engine native that blocks (`sleep`, `usleep`,
    /// `stream_select`, a blocking read, a child's wait, `flock`,
    /// `stream_socket_accept`, and
    /// ext/pcntl's and ext/sockets' waits, #2401) wait through, so the
    /// request never waits on C while C waits on the request. `wait`
    /// reaches nothing of the request, which is borrowed for its length;
    /// where no [`Reentry`] was made, nothing can call back and nothing
    /// is lent. `curl_exec` and `curl_multi_select` wait inside libcurl
    /// without it, as php's one thread runs one libcurl call of a request
    /// at a time, and a callback made meanwhile is served at the next
    /// interrupt check after they return (#2595).
    pub fn blocking<T>(self, wait: impl FnOnce() -> T) -> T {
        let Some(reentry) = self.0.limits.reentry.clone() else {
            return wait();
        };
        let Some(at) = std::num::NonZeroUsize::new(std::ptr::from_mut(self.0).expose_provenance())
        else {
            return wait();
        };
        let lending = reentry.lend(at);
        let waited = wait();
        reentry.reclaim(lending);
        waited
    }
}
