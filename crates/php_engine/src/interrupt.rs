//! The interrupt (design §5.6, §5.8; `CONTEXT.md`, Interrupt): the
//! engine's half of the one thing on a request that the execution timer,
//! a periodic interrupt, signal delivery and a deferred FFI callback
//! raise and nothing else touches. The interrupt
//! itself is [`crate::api::request::Interrupt`], on the request, because
//! signal delivery is an extension's and the compiled tier tests the
//! same atomic once per loop iteration through its `ExitCtx`
//! (`jit-design.md` §4.3).
//!
//! What is here is [`check`], the one the loop performs at its four
//! sites and nowhere else (design §5.8, revision 37): at a back-edge
//! `JMP*`, at a frame's activation, after every `Flow::Resume`, and
//! after an internal call returns, which is php's
//! `ZEND_VM_FCALL_INTERRUPT_CHECK` in the call handlers; [`interrupted`],
//! what a check that finds it raised does, which `call_function` also
//! runs after a native a native called, as `zend_call_function` does;
//! and [`Timer`], the engine's one timer thread, which serves every
//! request's [`Deadlines`]: the execution timer `max_execution_time`
//! arms and every periodic interrupt an extension starts (#1806).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError, Weak};
use std::time::{Duration, Instant};

use php_heap::Fatal;

use crate::api::request::{Interrupt, Request};

/// The interrupt check (design §5.8): one relaxed load and a branch, at
/// the loop's four sites and nowhere else. php's check costs the same load,
/// and an interrupt nobody raised costs nothing beyond it. `Ok(true)`
/// is a check whose extension hooks left a throwable pending, which the
/// site hands to the loop as thrown where it stands, as `zend_interrupt_helper`
/// leaves `EG(exception)` for the opline it re-enters.
#[inline]
pub(crate) fn check(req: &mut Request) -> Result<bool, Fatal> {
    if req.interrupt().raised().load(Ordering::Relaxed) {
        return interrupted(req);
    }
    Ok(false)
}

/// What a check that found the interrupt raised does
/// (`Z/Zend/zend_vm_def.h:10526`): it clears the interrupt and then
/// either raises php's timeout fatal, when the execution timer was what
/// raised it, or serves each extension's interrupt hook from the top of
/// php's `zend_interrupt_function` chain, the last `MINIT` to set it
/// first and each link then the one it saved (pcntl's dispatches the
/// signals delivered to their PHP handlers). With no hook it does
/// nothing, as php does.
///
/// The text and the plural are `zend_timeout`'s
/// (`Z/Zend/zend_execute_API.c:1441`), over the seconds the timer was
/// armed with, which is php's `EG(timeout_seconds)`. The line is the
/// back-edge's target, since the frame stands there before the check
/// (`ZEND_VM_JMP_EX`, which sets the opline and then
/// `ZEND_VM_INTERRUPT_CHECK`), which for an empty body is the `JMPNZ`
/// itself; the tests below pin the plural.
///
/// ```php
/// set_time_limit(1); for (;;) {}
/// ```
/// ```expect exit=255 wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
/// ```
pub(crate) fn interrupted(req: &mut Request) -> Result<bool, Fatal> {
    if let Some(seconds) = req.interrupt().taken() {
        return Err(Fatal::error(format!(
            "Maximum execution time of {seconds} second{} exceeded",
            if seconds == 1 { "" } else { "s" }
        )));
    }
    req.serve_interrupt_hooks();
    Ok(req.is_pending())
}

/// The engine's execution timer (design §5.8, §5.14 **Threads**, #2252):
/// every deadline of every request the engine serves, and the one thread
/// the engine runs to wait for them and raise each request's interrupt at
/// its own. A request's deadlines are its [`Deadlines`]: the execution
/// timer, what `zend_set_timeout` arms at the start of a request whose
/// `max_execution_time` is not zero and again when `set_time_limit()`
/// moves the entry, and what `zend_unset_timeout` unsets, as
/// `php_request_shutdown` and `OnUpdateTimeout` unset php's; and every
/// periodic interrupt an extension starts (#1806). A thread is the only
/// timed wake the standard library has, and one serves every request:
/// the schedule keeps each request's earliest deadline in one ordered
/// set and the thread waits for the first, so moving one is a write
/// under the timer's mutex and, where it comes sooner, a wake, with no
/// thread spawned (#1924). The thread is started by the change that sets
/// the engine's first deadline, so an engine that never arms one runs
/// none, and stays parked in its wait between requests, so a host that
/// serves them one after another, a php-fpm child or php-cgi, spawns it
/// once rather than at every request (#2541). It is joined before every
/// fork, a host's and a request's alike ([`Timers`]), and at the engine's
/// end, the drop of the last clone. A request takes its deadlines out under
/// the mutex as it ends, so nothing the thread raises after reaches it,
/// nor the next request on its thread, which has an interrupt of its
/// own. A deadline the thread itself retires, a timeout with no hard
/// stage or a periodic interrupt that raises once, leaves it parked
/// until the next change. What is timed later is one more deadline in
/// the [`Schedule`] the thread reads, never a second thread.
///
/// ZTS php on Linux arms one POSIX timer per thread on `CLOCK_BOOTTIME`
/// (`Z/Zend/zend_max_execution_timer.c:40-76`), and the oracle's build an
/// interval timer, `ITIMER_REAL`: both wall-clock time, as this is. The
/// mechanism is not observable; the deadline and the fatal are.
///
/// Past a timeout the timer is armed again for `hard_timeout` seconds,
/// as `zend_timeout_handler` arms it (`Z/Zend/zend_execute_API.c:1520`),
/// and `zend_timeout` leaves it armed when the loop takes the fatal: so
/// a shutdown function that loops is stopped by a second timeout, the
/// same seconds in its text, and a program the loop does not reach a
/// check in within `hard_timeout` seconds ends the process, as php's
/// build without ZTS does. A `hard_timeout` of zero arms nothing past
/// the first.
///
/// While a handler holds the time limit suspended
/// ([`Request::suspend_time_limit`], #1814) the execution timer is no
/// deadline but the time it had left, which neither stage counts down,
/// and the resume makes it a deadline again that much from then.
///
/// ```php
/// set_time_limit(1);
/// register_shutdown_function(function () { echo "shutdown\n"; for (;;) {} });
/// for (;;) {}
/// ```
/// ```expect exit=255 wall=3s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 4
/// shutdown
///
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
/// ```
///
/// **Its thread is never caught starting or ending.** Starting it
/// returns once it waits, and ending it returns once it has ended
/// (`JoinHandle::join`, `pthread_join`): so while PHP code runs, the
/// timer's thread is parked in its wait or raising an interrupt, and a
/// fork or the engine's end leaves no thread. A fork needs both
/// halves (#1783). The standard library takes one process-wide mutex as
/// a thread starts and again as it ends, its stack overflow handler's
/// thread table (`std/src/sys/pal/unix/stack_overflow/thread_info.rs`),
/// and a child forked while a timer's thread stood in either copied it
/// taken, with no thread to release it, and hung at exit, where its own
/// main thread takes it. Ending wakes the thread rather than waiting out
/// its deadline, so a join costs a wake and an exit, never the seconds
/// left. That is what a fork stands on (ADR-0010, Threads): the thread
/// is one [`crate::fork::spawn`] started, which no fork happens beside,
/// and the engine's one home for fork (#1807) joins every engine's timer
/// thread first ([`Timers::stop`]) and starts each again after in the
/// parent over the same schedule ([`Stopped::resume`]), whose deadlines
/// are instants, so a periodic interrupt keeps its grid and the
/// execution timer its stage.
///
/// A timer in a process forked from the one its schedule is for has no
/// thread there, as a child does not inherit php's interval timer
/// either, and its schedule is forgotten as the child first reaches it;
/// a thread a fork outside the home left standing in the parent is
/// forgotten with it, neither woken nor joined: the mutex may have been
/// copied held, and there is nothing to join. So the child runs untimed,
/// as php's does, until it sets a limit of its own.
///
/// ```php
/// set_time_limit(1);
/// if (($child = pcntl_fork()) === 0) {
///     for ($until = microtime(true) + 2; microtime(true) < $until;) {}
///     echo "the child ran past its parent's limit\n";
///     exit(0);
/// }
/// set_time_limit(0);
/// pcntl_waitpid($child, $status);
/// var_dump(pcntl_wifexited($status), pcntl_wexitstatus($status));
/// ```
/// ```expect process wall=2s
/// the child ran past its parent's limit
/// bool(true)
/// int(0)
/// ```
///
/// The stress below forks a child straight after each `set_time_limit()`
/// in eight workers at once, 25 children at a time. A hung child never
/// ends and a slow one does, so a child is hung once ten seconds pass in
/// which no child of its 25 ends: each ending starts the ten seconds
/// again, and the clock is read only after a poll that found nothing to
/// reap, so a child that ended while its worker was not running is reaped
/// rather than counted. A hung child is killed. A fork that failed, or a
/// child or a worker that did not end with status zero, is counted apart,
/// so neither passes as a child that did not hang. Five seconds from the
/// batch's last fork, read before the reaping, was once crossed by 73
/// children of 15000 at a load of 60, and by none of 1,020,000 read
/// since at loads to 95, the slowest reaped 0.55 s after its fork (#2371).
///
/// ```php
/// $tally = [];
/// for ($w = 0; $w < 8; $w++) {
///     $tally[$w] = tempnam(sys_get_temp_dir(), 'timer');
///     if (($workers[$w] = pcntl_fork()) !== 0) { continue; }
///     $hung = $otherwise = 0;
///     for ($forked = 0; $forked < 1875; $forked += 25) {
///         $children = [];
///         for ($i = 0; $i < 25; $i++) {
///             set_time_limit(30);
///             $child = pcntl_fork();
///             if ($child === 0) { exit(0); }
///             if ($child < 0) { $otherwise++; } else { $children[$child] = true; }
///         }
///         $quiet = microtime(true) + 10;
///         while ($children) {
///             $gone = pcntl_waitpid(-1, $status, WNOHANG);
///             if ($gone <= 0) {
///                 if (microtime(true) >= $quiet) { break; }
///                 usleep(1000);
///                 continue;
///             }
///             unset($children[$gone]);
///             $quiet = microtime(true) + 10;
///             if (!pcntl_wifexited($status) || pcntl_wexitstatus($status) !== 0) { $otherwise++; }
///         }
///         foreach ($children as $child => $_) {
///             posix_kill($child, SIGKILL);
///             pcntl_waitpid($child, $status);
///             $hung++;
///         }
///     }
///     file_put_contents($tally[$w], "$hung $otherwise");
///     exit(0);
/// }
/// $hung = $otherwise = $workers_otherwise = 0;
/// foreach ($workers as $w => $worker) {
///     pcntl_waitpid($worker, $status);
///     if (!pcntl_wifexited($status) || pcntl_wexitstatus($status) !== 0) { $workers_otherwise++; }
///     [$h, $o] = explode(' ', file_get_contents($tally[$w])) + [0, 0];
///     $hung += (int) $h;
///     $otherwise += (int) $o;
///     unlink($tally[$w]);
/// }
/// echo "$hung of 15000 children hung, $otherwise did not fork or end with status 0\n";
/// echo "$workers_otherwise of 8 workers did not end with status 0\n";
/// ```
/// ```expect process wall=30s
/// 0 of 15000 children hung, 0 did not fork or end with status 0
/// 0 of 8 workers did not end with status 0
/// ```
#[derive(Debug, Clone)]
pub(crate) struct Timer(Arc<Mutex<Control>>);

/// Every engine's timer in the process, by a reference that does not keep
/// the engine alive: what a fork stops first, whoever asks for it.
static TIMERS: Mutex<Vec<Weak<Mutex<Control>>>> = Mutex::new(Vec::new());

/// [`TIMERS`], held, with the timers of engines that have ended let go.
fn registered() -> MutexGuard<'static, Vec<Weak<Mutex<Control>>>> {
    let mut timers = TIMERS.lock().unwrap_or_else(PoisonError::into_inner);
    timers.retain(|timer| timer.strong_count() > 0);
    timers
}

impl Default for Timer {
    /// A timer with no deadline and no thread, entered among the
    /// process's timers so a fork finds it. The entry stands inside the
    /// engine ([`crate::fork::Inside`]), so no fork copies the list held.
    fn default() -> Timer {
        let timer = Timer(Arc::default());
        let _inside = crate::fork::Inside::enter();
        registered().push(Arc::downgrade(&timer.0));
        timer
    }
}

impl Timer {
    /// Whether the timer's thread waits parked with no request in its
    /// schedule, for the tests that hold it to the requests in flight.
    #[cfg(test)]
    pub(crate) fn parks_with_no_request(&self) -> bool {
        let control = self.control();
        control.thread.is_some() && control.shared.held().requests.is_empty()
    }

    /// Its thread started where none stands, before any deadline: a
    /// thread started from a thread apart ([`crate::fork::run_apart`])
    /// would keep the ids it took, so the timer stands before one, and a
    /// deadline the preload arms there is served by a thread with the
    /// process's ids.
    pub(crate) fn stand(&self) {
        let mut control = self.control();
        if control.thread.is_none() {
            control.start();
        }
    }

    /// The thread and the schedule, held: in a process forked from the
    /// one the schedule is for, both forgotten first (see [`Timer`]).
    fn control(&self) -> MutexGuard<'_, Control> {
        let mut control = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        control.forget_if_forked();
        control
    }
}

/// Every engine's timer in the process, as a fork finds them
/// ([`crate::fork`], ADR-0010's Threads): each one's thread joined before
/// the fork and started again after it.
#[derive(Debug)]
pub(crate) struct Timers(Vec<Timer>);

impl Timers {
    /// The timers of every engine that stands in the process.
    pub(crate) fn live() -> Timers {
        Timers(
            registered()
                .iter()
                .filter_map(Weak::upgrade)
                .map(Timer)
                .collect(),
        )
    }

    /// How many of them have a thread: what stands inside the engine for
    /// them, which a fork joins rather than refuses. `None` where one's
    /// mutex is held, by a request at work on it, which a fork does not
    /// wait for: the request may itself be waiting for the fork to end.
    pub(crate) fn threads(&self) -> Option<usize> {
        self.0.iter().try_fold(0, |threads, timer| {
            let control = match timer.0.try_lock() {
                Ok(control) => control,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => return None,
            };
            Some(threads + usize::from(control.thread.is_some()))
        })
    }

    /// Each one's thread told to end and joined, its schedule kept whole:
    /// what is stopped is the timers that had one.
    pub(crate) fn stop(self) -> Stopped {
        let mut stopped = Vec::new();
        for timer in self.0 {
            let mut control = timer.control();
            if control.thread.is_some() {
                control.end();
                drop(control);
                stopped.push(timer);
            }
        }
        Stopped(stopped)
    }
}

/// The timers whose threads a fork joined, for the length of the fork:
/// the caller's process gives each its thread again, and a child forked
/// meanwhile drops them, since its copy of each schedule is for another
/// process and is forgotten as it is reached.
#[derive(Debug)]
#[must_use]
pub(crate) struct Stopped(Vec<Timer>);

impl Stopped {
    /// Each one's thread started again, over the same deadlines, which
    /// are instants, so a periodic interrupt keeps its grid and the
    /// execution timer its stage: the process is left as it was.
    ///
    /// The restart stands inside the engine, as a request does when it
    /// starts a timer's thread: a spawn waits out a fork under way while
    /// it holds the timer's mutex, so a second fork must count it, and
    /// refuse, rather than wait on that mutex itself or copy it held.
    pub(crate) fn resume(self) {
        let _inside = crate::fork::Inside::enter();
        for timer in &self.0 {
            let mut control = timer.control();
            if control.thread.is_none() {
                control.start();
            }
        }
    }
}

/// What the engine's timer holds under its first mutex, which every
/// change a request makes takes and the thread never does: the thread,
/// started and joined only under it, and the schedule the thread reads
/// under the second.
#[derive(Debug)]
struct Control {
    shared: Arc<Shared>,
    /// The thread, from the change that set the engine's first deadline
    /// to a fork or the engine's end, parked in between whenever no
    /// request has a deadline.
    thread: Option<crate::fork::Thread>,
    /// The process the schedule is for, the only one the thread is in.
    process: Process,
    /// How many requests the timer has numbered, which numbers the next.
    requests: u64,
    /// How many threads the timer has started, for the tests that hold
    /// it to one.
    #[cfg(test)]
    started: usize,
}

impl Default for Control {
    fn default() -> Control {
        Control {
            shared: Arc::default(),
            thread: None,
            process: Process::current(),
            requests: 0,
            #[cfg(test)]
            started: 0,
        }
    }
}

impl Control {
    /// `f` over the schedule, under its mutex, and the thread woken where
    /// the earliest deadline came sooner, or started where it had none
    /// and now has a deadline. A change that leaves no deadline leaves the
    /// thread parked in its wait, so the next request's costs a write and
    /// a wake rather than a spawn (#2541).
    fn change(&mut self, f: impl FnOnce(&mut Schedule)) {
        let due = {
            let mut schedule = self.shared.held();
            let before = schedule.next();
            f(&mut schedule);
            let next = schedule.next();
            if next.is_some_and(|next| before.is_none_or(|before| next < before)) {
                self.shared.woken.notify_all();
            }
            next.is_some()
        };
        if due && self.thread.is_none() {
            self.start();
        }
    }

    /// Spawn the thread and return once it waits; a thread the system
    /// refuses leaves the engine's requests untimed, as before any
    /// deadline, until a change starts one.
    fn start(&mut self) {
        self.shared.held().phase = Phase::Starting;
        let shared = Arc::clone(&self.shared);
        let Some(thread) = crate::fork::spawn("php-timer", move || shared.serve()) else {
            return;
        };
        self.shared.started();
        self.thread = Some(thread);
        #[cfg(test)]
        {
            self.started += 1;
        }
    }

    /// Tell the thread to raise nothing more, and join it.
    fn end(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.shared.held().phase = Phase::Unset;
            self.shared.woken.notify_all();
            thread.join();
        }
    }

    /// In a process forked from the one the schedule is for, the thread and the
    /// schedule forgotten (see [`Timer`]), so what follows starts afresh: a
    /// suspended time limit with them, so the child is untimed after the resume
    /// as it is with none suspended. With no thread, nothing could hold the
    /// schedule's mutex as the process was copied, and the copy is dropped.
    fn forget_if_forked(&mut self) {
        if self.process.is_current() {
            return;
        }
        let shared = std::mem::take(&mut self.shared);
        if let Some(thread) = self.thread.take() {
            thread.forget();
            #[expect(
                clippy::mem_forget,
                reason = "the forked child's copy of the schedule, whose mutex may have been copied held, is neither woken nor dropped"
            )]
            std::mem::forget(shared);
        }
        self.process = Process::current();
    }
}

impl Drop for Control {
    /// The engine's end: the thread joined, or forgotten in a forked
    /// child. A request holds the timer, so none is in flight here.
    fn drop(&mut self) {
        self.forget_if_forked();
        self.end();
    }
}

/// One request's deadlines on its engine's [`Timer`]: the execution
/// timer `max_execution_time` arms and every periodic interrupt an
/// extension starts, taken out of the schedule as the request ends.
#[derive(Debug)]
pub(crate) struct Deadlines {
    timer: Timer,
    /// The request, by the number the timer gave it.
    key: Key,
    /// The request's interrupt, which the thread raises.
    interrupt: Interrupt,
    /// How many periodic interrupts were started, which numbers the next.
    periodics: u32,
    /// How many [`Request::suspend_time_limit`] guards stand: while any
    /// does, the execution timer is held as the time it has left.
    suspended: u32,
}

/// The shortest period a periodic interrupt is counted in, which one
/// asked for shorter is: a period below what the thread takes to raise
/// one would keep it raising and never let the request at the schedule.
const SHORTEST: Duration = Duration::from_micros(100);

impl Deadlines {
    /// The deadlines of the request whose interrupt is `interrupt`, on the
    /// engine's `timer`: none yet.
    pub(crate) fn new(timer: &Timer, interrupt: &Interrupt) -> Deadlines {
        let key = {
            let mut control = timer.control();
            control.requests = control.requests.wrapping_add(1);
            Key(control.requests)
        };
        Deadlines {
            timer: timer.clone(),
            key,
            interrupt: interrupt.clone(),
            periodics: 0,
            suspended: 0,
        }
    }

    /// `zend_unset_timeout` then `zend_set_timeout`: the execution
    /// deadline `seconds` from now, and again `hard` seconds past each
    /// timeout where `hard` is above zero; none at all for a `seconds`
    /// that is zero or below, which is php's `max_execution_time = 0`
    /// and the CLI's own default, or so far off no clock reaches it.
    /// While the time limit is suspended the whole `seconds` are kept as
    /// the time left, and count from the resume.
    pub(crate) fn arm(&mut self, seconds: i64, hard: i64) {
        let left = u64::try_from(seconds)
            .ok()
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .filter(|left| Instant::now().checked_add(*left).is_some());
        let mut execution = left.map(|left| Execution {
            due: Due::Left(left),
            seconds,
            hard: u64::try_from(hard).ok().filter(|h| *h > 0),
        });
        if self.suspended == 0 {
            execution = execution.and_then(|execution| execution.resumed(Instant::now()));
        }
        self.change(|scheduled| scheduled.execution = execution);
        // The old deadline is gone from the schedule, so whatever it
        // raised is raised already, and is cleared here.
        self.interrupt.timer_unset();
    }

    /// `zend_unset_timeout` at `php_request_shutdown`'s fourth step, and
    /// every periodic interrupt's deadline with it, so the request leaves
    /// the thread nothing to raise before any extension's `rshutdown`;
    /// the periods counted stay for an `rshutdown` to read.
    pub(crate) fn unset(&mut self) {
        self.change(|scheduled| {
            scheduled.execution = None;
            for period in &mut scheduled.periodic {
                period.next = None;
            }
        });
        self.interrupt.timer_unset();
    }

    /// [`Request::suspend_time_limit`]: the execution timer's deadline
    /// taken out of the schedule and the time it had left kept, by the
    /// first of the guards that stand; the thread waits parked where
    /// nothing else is scheduled, and serves the rest on where something
    /// is. A timeout already raised stays raised.
    pub(crate) fn suspend(&mut self) {
        self.suspended = self.suspended.saturating_add(1);
        if self.suspended == 1 {
            let now = Instant::now();
            self.change(|scheduled| {
                scheduled.execution = scheduled
                    .execution
                    .map(|execution| execution.suspended(now));
            });
        }
    }

    /// The guard's drop: by the last of the guards that stand, the
    /// execution timer put back in the schedule at the time it had left
    /// from now, and the thread started where it had none.
    pub(crate) fn resume(&mut self) {
        self.suspended = self.suspended.saturating_sub(1);
        if self.suspended == 0 {
            let now = Instant::now();
            self.change(|scheduled| {
                scheduled.execution = scheduled
                    .execution
                    .and_then(|execution| execution.resumed(now));
            });
        }
    }

    /// [`Request::start_periodic`]. `None` where no thread could be
    /// started to serve it.
    pub(crate) fn start_periodic(
        &mut self,
        first: Duration,
        every: Option<Duration>,
    ) -> Option<Periodic> {
        let id = Periodic(self.periodics);
        self.periodics = self.periodics.checked_add(1)?;
        let period = Period {
            id,
            next: Some(Instant::now().checked_add(first)?),
            every: every.map(|every| every.max(SHORTEST)),
            elapsed: 0,
        };
        let mut control = self.timer.control();
        control.change(|schedule| {
            schedule.change(self.key, &self.interrupt, |scheduled| {
                scheduled.periodic.push(period);
            });
        });
        if control.thread.is_none() {
            control.change(|schedule| {
                schedule.change(self.key, &self.interrupt, |scheduled| {
                    scheduled.periodic.retain(|period| period.id != id);
                });
            });
            return None;
        }
        Some(id)
    }

    /// [`Request::periods_elapsed`]: the periods elapsed since the last
    /// time this was asked, and none counted from here.
    pub(crate) fn periods_elapsed(&mut self, id: Periodic) -> u64 {
        let control = self.timer.control();
        let mut schedule = control.shared.held();
        let period = schedule
            .requests
            .get_mut(&self.key)
            .and_then(|scheduled| scheduled.periodic.iter_mut().find(|period| period.id == id));
        period.map_or(0, |period| std::mem::take(&mut period.elapsed))
    }

    /// [`Request::stop_periodic`]: its deadline gone and its count with it.
    pub(crate) fn stop_periodic(&mut self, id: Periodic) {
        self.change(|scheduled| scheduled.periodic.retain(|period| period.id != id));
    }

    /// `f` over this request's deadlines, through [`Control::change`].
    fn change(&self, f: impl FnOnce(&mut Scheduled)) {
        self.timer
            .control()
            .change(|schedule| schedule.change(self.key, &self.interrupt, f));
    }
}

impl Drop for Deadlines {
    /// The request's end: its deadlines and its periods taken out of the
    /// schedule; the thread stays parked for the engine's next request.
    fn drop(&mut self) {
        self.timer
            .control()
            .change(|schedule| schedule.remove(self.key));
    }
}

/// A request, by the number its engine's timer gave it as it was laid,
/// which no other request of the engine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Key(u64);

/// A process, by its id, `getpid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Process(u32);

impl Process {
    /// The process this runs in.
    fn current() -> Process {
        Process(std::process::id())
    }

    /// Whether this is the process it runs in, which a child forked from
    /// it never is.
    fn is_current(self) -> bool {
        self == Process::current()
    }
}

/// Where the timer's thread stands: starting, then waiting, then told to
/// end, and starting again only once it has been joined.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Spawned, and not yet waiting: starting waits this out.
    #[default]
    Starting,
    /// Waiting for a deadline, a change or its end.
    Waiting,
    /// Told to raise nothing more and end.
    Unset,
}

/// Every deadline the engine's requests have, and where the thread
/// stands: what the thread and the requests share under the timer's
/// second mutex.
#[derive(Debug, Default)]
struct Schedule {
    phase: Phase,
    /// Each request with a deadline, a suspended time limit or a
    /// periodic interrupt, by its number.
    requests: HashMap<Key, Scheduled>,
    /// Each request's earliest deadline, the one [`Scheduled::next`]
    /// holds, in the order the thread meets them, with the interrupt it
    /// raises there: the schedule holds a request's interrupt while the
    /// request has a deadline and no longer.
    due: BTreeMap<(Instant, Key), Interrupt>,
}

/// One request's deadlines, in the schedule.
#[derive(Debug, Default)]
struct Scheduled {
    /// The execution timer, while one is armed.
    execution: Option<Execution>,
    /// Every periodic interrupt started and not yet stopped.
    periodic: Vec<Period>,
    /// The earliest of its deadlines, as [`Schedule::due`] holds it.
    next: Option<Instant>,
}

/// One periodic interrupt a request was asked for (#1806), by the number
/// its request's deadlines gave it: what [`Request::start_periodic`]
/// answers and the extension that asked keeps, in its request state, to
/// read its periods with and to stop it by. The number is its request's
/// alone: another request numbers its own from the same start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Periodic(u32);

/// A periodic interrupt, started.
#[derive(Debug)]
struct Period {
    id: Periodic,
    /// Its next deadline: `None` once one that raises once has raised, or
    /// once the request's end has unset it.
    next: Option<Instant>,
    /// The period, at least [`SHORTEST`]; `None` raises once.
    every: Option<Duration>,
    /// The periods elapsed since the extension last read them.
    elapsed: u64,
}

/// The execution timer, armed.
#[derive(Debug, Clone, Copy)]
struct Execution {
    due: Due,
    /// What it was armed with, php's `EG(timeout_seconds)`.
    seconds: i64,
    /// `EG(hard_timeout)`, where it is above zero.
    hard: Option<u64>,
}

/// When the execution timer is due: at a deadline the thread waits for,
/// or, while the time limit is suspended (#1814), never, with the time
/// it had left kept for the resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    At(Instant),
    Left(Duration),
}

impl Execution {
    /// The deadline the thread waits for, `None` while suspended.
    fn deadline(self) -> Option<Instant> {
        match self.due {
            Due::At(deadline) => Some(deadline),
            Due::Left(_) => None,
        }
    }

    /// Suspended at `now`: the deadline become the time it had left, none
    /// where it had passed, so a resume finds it due at once.
    fn suspended(self, now: Instant) -> Execution {
        match self.due {
            Due::At(deadline) => Execution {
                due: Due::Left(deadline.saturating_duration_since(now)),
                ..self
            },
            Due::Left(_) => self,
        }
    }

    /// Resumed at `now`: the time left become a deadline that far off,
    /// and `None` where no clock reaches it, as [`Deadlines::arm`] arms
    /// none.
    fn resumed(self, now: Instant) -> Option<Execution> {
        match self.due {
            Due::At(_) => Some(self),
            Due::Left(left) => Some(Execution {
                due: Due::At(now.checked_add(left)?),
                ..self
            }),
        }
    }
}

impl Schedule {
    /// The earliest deadline of any request, `None` where none is set.
    fn next(&self) -> Option<Instant> {
        self.due.first_key_value().map(|((at, _), _)| *at)
    }

    /// `f` over the deadlines of the request `key`, whose interrupt is
    /// `interrupt`, and its place in [`Schedule::due`] moved to match.
    fn change(&mut self, key: Key, interrupt: &Interrupt, f: impl FnOnce(&mut Scheduled)) {
        f(self.requests.entry(key).or_default());
        self.settle(key, interrupt);
    }

    /// The request `key` gone from the schedule, every deadline with it.
    fn remove(&mut self, key: Key) {
        if let Some(next) = self.requests.remove(&key).and_then(|gone| gone.next) {
            self.due.remove(&(next, key));
        }
    }

    /// The request `key`'s earliest deadline put in [`Schedule::due`] in
    /// place of the one it held, raising `interrupt`, and the request let
    /// go of where it has nothing left to time or to count.
    fn settle(&mut self, key: Key, interrupt: &Interrupt) {
        let Some(scheduled) = self.requests.get_mut(&key) else {
            return;
        };
        let next = scheduled.earliest();
        if next != scheduled.next {
            if let Some(held) = scheduled.next {
                self.due.remove(&(held, key));
            }
            if let Some(next) = next {
                self.due.insert((next, key), interrupt.clone());
            }
            scheduled.next = next;
        }
        if scheduled.execution.is_none() && scheduled.periodic.is_empty() {
            self.remove(key);
        }
    }

    /// What every deadline at or before `now` raises, request by request
    /// in the order they fall due ([`Scheduled::elapse`]).
    fn elapse(&mut self, now: Instant) {
        while let Some((&(at, _), _)) = self.due.first_key_value()
            && at <= now
        {
            let Some(((_, key), interrupt)) = self.due.pop_first() else {
                break;
            };
            if let Some(scheduled) = self.requests.get_mut(&key) {
                scheduled.next = None;
                scheduled.elapse(&interrupt, now);
            }
            self.settle(key, &interrupt);
        }
    }
}

impl Scheduled {
    /// The earliest of its deadlines, `None` where none is set.
    fn earliest(&self) -> Option<Instant> {
        let periodic = self.periodic.iter().filter_map(|period| period.next);
        self.execution
            .and_then(Execution::deadline)
            .into_iter()
            .chain(periodic)
            .min()
    }

    /// What each of its deadlines at or before `now` raises on
    /// `interrupt`, its request's.
    ///
    /// The execution timer's: `zend_timeout_handler` sets both words and
    /// returns (`Z/Zend/zend_execute_API.c:1516-1518`); so does this, and
    /// the loop's next check is what raises the fatal. Where there is a
    /// `hard` wait the deadline moves past it: a deadline that finds the
    /// last timeout still unread ends the process ([`terminate`]), and
    /// one that finds it taken is a timeout of its own (`:1482`).
    ///
    /// A periodic interrupt's: every period elapsed by `now` counted, the
    /// ones a late wake missed among them, as Excimer counts a POSIX
    /// timer's overruns into its event count (`excimer_timer_handle`),
    /// its next deadline kept on the grid `first` laid down, and the
    /// interrupt raised, as Excimer's handler stores php's interrupt word.
    /// The count waits for the extension's hook to read it.
    fn elapse(&mut self, interrupt: &Interrupt, now: Instant) {
        if let Some(execution) = &mut self.execution
            && execution.deadline().is_some_and(|deadline| deadline <= now)
        {
            if let Some(hard) = execution.hard
                && interrupt.timed_out()
            {
                terminate(execution.seconds, hard);
            }
            interrupt.elapsed(execution.seconds);
            match execution
                .hard
                .and_then(|hard| now.checked_add(Duration::from_secs(hard)))
            {
                Some(deadline) => execution.due = Due::At(deadline),
                None => self.execution = None,
            }
        }
        for period in &mut self.periodic {
            let Some(next) = period.next.filter(|next| *next <= now) else {
                continue;
            };
            let late = period
                .every
                .map(|every| (now - next).as_nanos() / every.as_nanos());
            let periods = late.map_or(1, |late| {
                u64::try_from(late).unwrap_or(u64::MAX).saturating_add(1)
            });
            period.elapsed = period.elapsed.saturating_add(periods);
            let times = u32::try_from(periods).unwrap_or(u32::MAX);
            period.next = period
                .every
                .and_then(|every| next.checked_add(every.saturating_mul(times)));
            interrupt.raise();
        }
    }
}

/// What the timer's thread and the requests share: the schedule, and the
/// variable each wakes the other through.
#[derive(Debug, Default)]
struct Shared {
    schedule: Mutex<Schedule>,
    woken: Condvar,
}

impl Shared {
    /// The thread's whole body: say it waits, then wait for the earliest
    /// deadline, or for ever where none is set, and raise what is due,
    /// reading the schedule again at each wake, until told to end.
    fn serve(&self) {
        let mut schedule = self.held();
        schedule.phase = Phase::Waiting;
        self.woken.notify_all();
        while schedule.phase != Phase::Unset {
            let now = Instant::now();
            schedule = match schedule.next() {
                Some(at) if at <= now => {
                    schedule.elapse(now);
                    schedule
                }
                Some(at) => match self.woken.wait_timeout(schedule, at - now) {
                    Ok((schedule, _)) => schedule,
                    Err(poisoned) => poisoned.into_inner().0,
                },
                None => self
                    .woken
                    .wait(schedule)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    /// Wait until the thread has started and waits: past that point the
    /// standard library's start is behind it. Saying so is the first
    /// thing the thread's body does, so nothing it runs can leave this
    /// waiting.
    fn started(&self) {
        let mut schedule = self.held();
        while schedule.phase == Phase::Starting {
            schedule = self
                .woken
                .wait(schedule)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The schedule, whatever a panic on another thread left behind:
    /// each write leaves it whole, so a poisoned `MutexGuard` is as good
    /// as a clean one.
    fn held(&self) -> MutexGuard<'_, Schedule> {
        self.schedule.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The hard timeout (`Z/Zend/zend_execute_API.c:1482-1510`): php's
/// line on the process's standard error and its status, 124, with no
/// shutdown of any kind, as `_exit` ends it. The timer's thread cannot
/// read the frame the request's thread is stuck under, so the place is
/// php's where nothing is executing.
fn terminate(seconds: i64, hard: u64) -> ! {
    use std::io::Write as _;
    let line = format!(
        "\nFatal error: Maximum execution time of {seconds}+{hard} seconds exceeded (terminated) in Unknown on line 0\n"
    );
    std::io::stderr().write_all(line.as_bytes()).ok();
    std::process::exit(124)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    use php_unit::{OpArray, Opcode, Opline, OplineIdx, PublishedUnit, Unit};

    use super::*;
    use crate::engine::{Engine, RequestDescription};
    use crate::observer::Untold;
    use crate::probe::{Asked, Probe};
    use crate::tier::{Nop, Site, execute};

    /// php's text for a timer armed with one second
    /// (`Z/Zend/zend_execute_API.c:1441`).
    const TIMED_OUT: &str = "Maximum execution time of 1 second exceeded";

    /// A unit whose main op array is one `JMP` to itself: the back-edge
    /// of `for (;;) {}`, safepoint number one, which runs until the
    /// interrupt stops it.
    fn looping() -> PublishedUnit {
        Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::Jmp,
                    op1: 0,
                    out: 1,
                    ..Opline::default()
                }],
                safepoints: vec![OplineIdx(0), OplineIdx(0)].into(),
                ..OpArray::default()
            },
            ..Unit::default()
        }
        .published(None)
    }

    fn description() -> RequestDescription {
        RequestDescription {
            script_name: Some(b"witness.php".to_vec()),
            ..RequestDescription::default()
        }
    }

    /// Design §5.8: the check at a frame's activation, before the site,
    /// so a request whose time is up raises the fatal without the tier
    /// being asked anything.
    #[test]
    fn the_interrupt_is_tested_at_an_activation() {
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description());
        let frame = req.pushed(looping());
        req.interrupt().elapsed(1);

        assert_eq!(
            execute::<Probe, Untold>(&mut req, frame),
            Err(Fatal::error(TIMED_OUT))
        );
        assert_eq!(
            req.state::<Asked>().sites,
            vec![],
            "the check is before the site, so nothing was asked"
        );
    }

    /// `jit-design.md` §2.2: after a `Flow::Resume` the loop performs the
    /// check it performs at a back-edge, so a timer that fired while the
    /// tier served code stops the loop at the exit.
    #[test]
    fn the_interrupt_is_tested_after_a_resume() {
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description());
        let frame = req.pushed(looping());
        let asked = req.state::<Asked>();
        asked.resume = Some(frame);
        asked.elapsed = Some(1);

        assert_eq!(
            execute::<Probe, Untold>(&mut req, frame),
            Err(Fatal::error(TIMED_OUT))
        );
        assert_eq!(req.state::<Asked>().sites, vec![(Site::Entry, frame)]);
    }

    /// Design §5.8: an interrupt raised on another thread stops the loop
    /// at the next back-edge, which for `for (;;) {}` is one iteration.
    /// The other thread is the engine's own execution timer, armed from
    /// `max_execution_time`.
    #[test]
    fn an_execution_timer_stops_a_loop_at_its_back_edge() {
        let engine = Engine::cli_builder()
            .ini("max_execution_time", "1")
            .tier::<Nop>()
            .build();
        // Before the request, whose start arms the timer.
        let armed = Instant::now();
        let mut req = Request::new(Arc::clone(&engine.shared), description());
        let frame = req.pushed(looping());

        assert_eq!(
            execute::<Nop, Untold>(&mut req, frame),
            Err(Fatal::error(TIMED_OUT))
        );
        assert!(
            armed.elapsed() >= Duration::from_secs(1),
            "the loop ran until the timer fired, not before"
        );
    }

    /// `zend_unset_timeout` clears `EG(timed_out)` beside stopping the
    /// timer, so a deadline the old timer reached as `set_time_limit()`
    /// replaced it raises no fatal at the next check.
    #[test]
    fn unsetting_a_timer_that_elapsed_clears_what_it_raised() {
        let interrupt = Interrupt::default();
        let mut deadlines = Deadlines::new(&Timer::default(), &interrupt);
        deadlines.arm(1, 0);
        let armed = Instant::now();
        while !interrupt.raised().load(Ordering::Relaxed) {
            assert!(
                armed.elapsed() < Duration::from_secs(10),
                "the timer never fired"
            );
            thread::sleep(Duration::from_millis(10));
        }
        deadlines.arm(0, 0);
        assert_eq!(
            interrupt.taken(),
            None,
            "raised, and the timer's word cleared"
        );
    }

    /// The thread serving a timer, by its id, while it has one.
    fn serving(timer: &Timer) -> Option<thread::ThreadId> {
        timer
            .control()
            .thread
            .as_ref()
            .map(|thread| thread.thread().id())
    }

    /// How many threads a timer has started.
    fn started(timer: &Timer) -> usize {
        timer.control().started
    }

    /// The schedule's share of a timer, which its thread holds while it
    /// runs.
    fn shared(timer: &Timer) -> Arc<Shared> {
        Arc::clone(&timer.control().shared)
    }

    /// #1783: a deadline set returns with the thread waiting; the last
    /// one unset, or the request's deadlines dropped, leaves it parked in
    /// its wait (#2541); and the engine's end, the drop of the timer's last
    /// clone, returns with the thread's body done and its share of the
    /// schedule released. The end wakes it rather than waiting out the
    /// thirty seconds it was armed for. That the thread has then ended,
    /// the standard library's exit behind it too, is the join's, which
    /// nothing in the process can observe short of counting its threads:
    /// the fork stress on [`Timer`] is what holds it.
    #[test]
    fn a_timers_thread_waits_once_armed_and_is_done_at_the_engines_end() {
        let _forks = crate::fork::forks_held();
        let interrupt = Interrupt::default();
        let began = Instant::now();
        for _ in 0..100 {
            let timer = Timer::default();
            let mut deadlines = Deadlines::new(&timer, &interrupt);
            deadlines.arm(30, 2);
            let thread = serving(&timer);
            assert_eq!(
                shared(&timer).held().phase,
                Phase::Waiting,
                "armed and waiting"
            );
            deadlines.arm(0, 2);
            assert_eq!(serving(&timer), thread, "unset, and parked");
            deadlines.arm(30, 2);
            drop(deadlines);
            assert_eq!(serving(&timer), thread, "dropped, and parked");
            let schedule = shared(&timer);
            drop(timer);
            assert_eq!(
                Arc::strong_count(&schedule),
                1,
                "the thread let go of the schedule before the engine's end returned"
            );
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "each end woke its thread rather than waiting out the deadline"
        );
    }

    /// #1924: `set_time_limit()` over an armed timer moves its deadline
    /// and spawns nothing, so the thread that served the first deadline
    /// serves every one after it.
    #[test]
    fn rearming_a_timer_keeps_its_thread() {
        let interrupt = Interrupt::default();
        let timer = Timer::default();
        let mut deadlines = Deadlines::new(&timer, &interrupt);
        deadlines.arm(30, 2);
        let first = serving(&timer);
        assert!(first.is_some(), "armed, and served");
        for seconds in 1..=1000 {
            deadlines.arm(30 + seconds, 2);
            assert_eq!(serving(&timer), first, "the same thread");
        }
        assert_eq!(started(&timer), 1);
    }

    /// php's `max_execution_time = 0`, the CLI's default: no deadline, no
    /// thread, and an interrupt nothing raises; and the same for a limit
    /// past what the clock can count to, `set_time_limit(PHP_INT_MAX)`.
    #[test]
    fn a_request_with_no_time_limit_arms_no_timer() {
        let interrupt = Interrupt::default();
        let timer = Timer::default();
        let mut deadlines = Deadlines::new(&timer, &interrupt);
        deadlines.arm(0, 2);
        deadlines.arm(-1, 2);
        deadlines.arm(i64::MAX, i64::MAX);
        assert_eq!(serving(&timer), None);
        assert_eq!(
            started(&timer),
            0,
            "an engine that arms nothing runs no thread"
        );
        assert!(!interrupt.raised().load(Ordering::Relaxed));
    }

    /// #2252: requests served on threads of their own over one engine,
    /// each with `max_execution_time` armed, are served by one thread,
    /// which the engine started once and which stays parked once the last
    /// of them ended (#2541).
    #[test]
    fn one_thread_serves_every_request_of_an_engine() {
        const REQUESTS: usize = 8;
        let engine = Engine::cli_builder()
            .ini("max_execution_time", "30")
            .build();
        let armed = Arc::new(std::sync::Barrier::new(REQUESTS + 1));
        let counted = Arc::new(std::sync::Barrier::new(REQUESTS + 1));
        let threads: Vec<_> = (0..REQUESTS)
            .map(|_| {
                let shared = Arc::clone(&engine.shared);
                let (armed, counted) = (Arc::clone(&armed), Arc::clone(&counted));
                thread::spawn(move || {
                    let req = Request::new(shared, description());
                    armed.wait();
                    counted.wait();
                    drop(req);
                })
            })
            .collect();
        armed.wait();
        let timer = &engine.shared.timer;
        let scheduled = shared(timer).held().requests.len();
        let (serving_then, started_then) = (serving(timer), started(timer));
        counted.wait();
        for thread in threads {
            assert!(thread.join().is_ok());
        }
        assert_eq!(
            scheduled, REQUESTS,
            "every request's deadline in one schedule"
        );
        assert!(serving_then.is_some());
        assert_eq!(
            started_then, 1,
            "one thread for the engine's start and {REQUESTS} requests"
        );
        assert_eq!(serving(timer), serving_then, "parked once the last ended");
        assert!(shared(timer).held().requests.is_empty());
    }

    /// #2541: requests served one after another over one engine, each with
    /// `max_execution_time` armed, as a php-fpm child or php-cgi serves
    /// them, are served by one thread, which stays parked between them
    /// rather than being joined at each request's end and spawned again
    /// at the next one's start.
    #[test]
    fn requests_served_one_after_another_start_one_thread() {
        let engine = Engine::cli_builder()
            .ini("max_execution_time", "30")
            .build();
        let timer = &engine.shared.timer;
        let first = Request::new(Arc::clone(&engine.shared), description());
        let thread = serving(timer);
        assert!(thread.is_some(), "armed, and served");
        drop(first);
        assert_eq!(serving(timer), thread, "parked between the requests");
        let second = Request::new(Arc::clone(&engine.shared), description());
        assert_eq!(serving(timer), thread, "the same thread serves the next");
        drop(second);
        assert_eq!(
            started(timer),
            1,
            "one thread for the engine's start and two requests"
        );
    }

    /// #2252: each of the requests one engine's thread serves meets its
    /// own deadline, the loop stopped at its back-edge with php's fatal.
    #[test]
    fn the_engines_thread_stops_each_requests_loop_at_its_deadline() {
        const REQUESTS: usize = 4;
        let engine = Engine::cli_builder()
            .ini("max_execution_time", "1")
            .ini("hard_timeout", "0")
            .tier::<Nop>()
            .build();
        let armed = Arc::new(std::sync::Barrier::new(REQUESTS));
        let began = Instant::now();
        let threads: Vec<_> = (0..REQUESTS)
            .map(|_| {
                let shared = Arc::clone(&engine.shared);
                let armed = Arc::clone(&armed);
                thread::spawn(move || {
                    let mut req = Request::new(shared, description());
                    let frame = req.pushed(looping());
                    armed.wait();
                    execute::<Nop, Untold>(&mut req, frame)
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().ok(), Some(Err(Fatal::error(TIMED_OUT))));
        }
        assert!(began.elapsed() >= Duration::from_secs(1));
        assert_eq!(started(&engine.shared.timer), 1);
    }

    /// A request that ends takes its deadlines out of the schedule, so
    /// the thread holds no interrupt of it and raises nothing for it,
    /// nor for the request laid next, whose interrupt is its own.
    #[test]
    fn a_request_that_ends_leaves_the_schedule_nothing_of_its_own() {
        let timer = Timer::default();
        let ended = Interrupt::default();
        let mut deadlines = Deadlines::new(&timer, &ended);
        deadlines.arm(1, 2);
        let _periodic = deadlines.start_periodic(Duration::from_secs(1), None);
        let other = Interrupt::default();
        let mut others = Deadlines::new(&timer, &other);
        others.arm(30, 2);
        drop(deadlines);
        assert_eq!(ended.holders(), 1, "the schedule let go of its interrupt");
        assert_eq!(shared(&timer).held().requests.len(), 1);
        assert!(
            serving(&timer).is_some(),
            "the other request is still timed"
        );
        let next = Deadlines::new(&timer, &Interrupt::default());
        thread::sleep(Duration::from_millis(1200));
        assert!(
            !ended.raised().load(Ordering::Relaxed),
            "nothing raised past its end"
        );
        assert!(!next.interrupt.raised().load(Ordering::Relaxed));
        assert!(!other.raised().load(Ordering::Relaxed));
    }

    /// #1806: a periodic interrupt counts every period elapsed, the ones
    /// nobody read among them, and hands each count over once; the
    /// periods are laid on the grid `first` began, so a late read loses
    /// none.
    #[test]
    fn a_periodic_interrupt_counts_the_periods_nobody_read() {
        let interrupt = Interrupt::default();
        let mut deadlines = Deadlines::new(&Timer::default(), &interrupt);
        let every = Duration::from_millis(5);
        let began = Instant::now();
        let periodic = deadlines.start_periodic(every, Some(every));
        thread::sleep(every * 20);
        let read = periodic.map_or(0, |periodic| deadlines.periods_elapsed(periodic));
        let at_most = u64::try_from(began.elapsed().as_millis() / every.as_millis()).unwrap_or(0);
        assert!(
            (15..=at_most).contains(&read),
            "{read} of at most {at_most}"
        );
        assert!(interrupt.raised().load(Ordering::Relaxed));
        let again = periodic.map_or(9, |periodic| deadlines.periods_elapsed(periodic));
        assert!(again <= 1, "each count is handed over once, not {again}");
    }

    /// A periodic interrupt with no period raises the interrupt once, `first` from now, as
    /// Excimer's one-shot timer does; stopping the last periodic
    /// interrupt, or dropping deadlines that still have one, leaves the
    /// thread parked with nothing of the request in the schedule.
    #[test]
    fn a_periodic_interrupt_of_no_period_raises_once_and_its_thread_parks() {
        let interrupt = Interrupt::default();
        let timer = Timer::default();
        let mut deadlines = Deadlines::new(&timer, &interrupt);
        let once = deadlines.start_periodic(Duration::from_millis(5), None);
        assert!(serving(&timer).is_some(), "started, and served");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(once.map(|once| deadlines.periods_elapsed(once)), Some(1));
        if let Some(once) = once {
            deadlines.stop_periodic(once);
        }
        let thread = serving(&timer);
        assert!(thread.is_some(), "stopped, and parked");
        assert_eq!(shared(&timer).held().next(), None);

        let every = deadlines.start_periodic(Duration::ZERO, Some(Duration::from_secs(30)));
        assert!(every.is_some());
        assert_eq!(serving(&timer), thread, "the parked thread serves it");
        drop(deadlines);
        assert_eq!(serving(&timer), thread, "dropped, and parked");
        assert!(shared(&timer).held().requests.is_empty());
        assert_eq!(started(&timer), 1);
    }

    /// ADR-0010, Threads: a fork is called with the timer's thread joined,
    /// parked as it was, and after it the parent's thread serves the same
    /// deadlines, the execution timer's and a periodic interrupt's,
    /// counted from where they stood rather than from the fork. The
    /// timers are this test's own, since stopping every one in the
    /// process would stop the other tests' too.
    #[test]
    fn a_fork_finds_no_thread_and_the_parent_keeps_its_deadlines() {
        let interrupt = Interrupt::default();
        let timer = Timer::default();
        let mut deadlines = Deadlines::new(&timer, &interrupt);
        deadlines.arm(30, 2);
        let periodic =
            deadlines.start_periodic(Duration::from_secs(20), Some(Duration::from_secs(20)));
        let before = shared(&timer).held().next();
        let timers = Timers(vec![timer.clone()]);
        assert_eq!(timers.threads(), Some(1));
        let stopped = timers.stop();
        assert_eq!(serving(&timer), None, "no thread at the fork");
        assert_eq!(Arc::strong_count(&shared(&timer)), 2, "and its body done");
        stopped.resume();
        assert!(serving(&timer).is_some(), "started again after the fork");
        assert_eq!(shared(&timer).held().next(), before, "the same deadlines");
        if let Some(periodic) = periodic {
            deadlines.stop_periodic(periodic);
        }
        assert!(serving(&timer).is_some(), "the execution timer is left");
    }

    /// #2541: a host's fork after a timed request finds the engine's timer
    /// parked, joins it, and in the parent starts it again, as it was:
    /// php-fpm's master forks each child after the startup request armed
    /// `max_execution_time`.
    #[test]
    fn a_fork_after_a_timed_request_joins_the_parked_thread() {
        let _forks = crate::fork::forks_held();
        let engine = Engine::cli_builder()
            .ini("max_execution_time", "30")
            .build();
        drop(Request::new(Arc::clone(&engine.shared), description()));
        let timer = &engine.shared.timer;
        assert!(serving(timer).is_some(), "parked after the request");
        let live = Timers::live();
        assert!(
            live.0.iter().any(|live| Arc::ptr_eq(&live.0, &timer.0)),
            "the fork finds the engine's timer"
        );
        let timers = Timers(vec![timer.clone()]);
        assert_eq!(timers.threads(), Some(1), "what the fork joins");
        let stopped = timers.stop();
        assert_eq!(serving(timer), None, "no thread at the fork");
        stopped.resume();
        assert!(serving(timer).is_some(), "parked again after the fork");
        assert_eq!(started(timer), 2);
    }

    /// A timer whose mutex a request holds is not counted, so the fork is
    /// refused rather than waiting on a request that may wait on it.
    #[test]
    fn a_timer_a_request_holds_is_not_counted() {
        let timer = Timer::default();
        let held = timer.control();
        assert_eq!(Timers(vec![timer.clone()]).threads(), None);
        drop(held);
        assert_eq!(Timers(vec![timer.clone()]).threads(), Some(0));
    }

    /// A host's fork refused while something else stands inside the
    /// engine touches no timer: the parked thread is the one that stood.
    #[test]
    fn a_refused_fork_leaves_the_parked_thread_standing() {
        let _forks = crate::fork::forks_held();
        let engine = Engine::cli_builder()
            .ini("max_execution_time", "30")
            .build();
        drop(Request::new(Arc::clone(&engine.shared), description()));
        let timer = &engine.shared.timer;
        let thread = serving(timer);
        assert!(thread.is_some());
        let inside = crate::fork::Inside::enter();
        let refused = crate::fork::fork().err().and_then(|e| e.raw_os_error());
        drop(inside);
        assert_eq!(refused, Some(crate::fork::EBUSY));
        assert_eq!(serving(timer), thread, "the same thread, never joined");
        assert_eq!(started(timer), 1);
    }

    /// A timer whose engine has ended is not one a fork finds.
    #[test]
    fn a_fork_does_not_find_an_ended_engines_timer() {
        let _forks = crate::fork::forks_held();
        let timer = Timer::default();
        let gone = Arc::downgrade(&timer.0);
        drop(timer);
        let live = Timers::live();
        assert!(gone.upgrade().is_none());
        assert!(
            !live
                .0
                .iter()
                .any(|live| std::ptr::eq(Arc::as_ptr(&live.0), gone.as_ptr()))
        );
    }

    /// The execution timer's due time, as the schedule holds it.
    fn due(deadlines: &Deadlines) -> Option<Due> {
        shared(&deadlines.timer)
            .held()
            .requests
            .get(&deadlines.key)
            .and_then(|scheduled| scheduled.execution)
            .map(|execution| execution.due)
    }

    /// #1814: a suspend takes the deadline out of the schedule and keeps
    /// the time it had left, joining the thread that served nothing else;
    /// guards nest, and the outermost resume puts the deadline back that
    /// far from then, on a thread started again.
    #[test]
    fn a_suspend_keeps_the_time_left_and_the_last_resume_puts_it_back() {
        let interrupt = Interrupt::default();
        let timer = Timer::default();
        let mut deadlines = Deadlines::new(&timer, &interrupt);
        deadlines.arm(30, 2);
        deadlines.suspend();
        let left = match due(&deadlines) {
            Some(Due::Left(left)) => left,
            _ => Duration::ZERO,
        };
        assert!(
            left > Duration::from_secs(29) && left <= Duration::from_secs(30),
            "suspended with the time left, not {:?}",
            due(&deadlines)
        );
        assert!(
            serving(&timer).is_some(),
            "nothing else scheduled, and the thread parked"
        );
        deadlines.suspend();
        deadlines.resume();
        assert_eq!(
            due(&deadlines),
            Some(Due::Left(left)),
            "an inner resume resumes nothing"
        );
        let resumed = Instant::now();
        deadlines.resume();
        let due_again = due(&deadlines);
        assert!(
            matches!(due_again, Some(Due::At(deadline))
                if deadline >= resumed + left && deadline <= Instant::now() + left),
            "resumed at the time left from now, not {due_again:?}"
        );
        assert!(serving(&timer).is_some(), "served again");
    }

    /// `set_time_limit()` while suspended keeps its seconds whole for the
    /// resume, and arms no deadline before it.
    #[test]
    fn a_limit_set_while_suspended_counts_from_the_resume() {
        let interrupt = Interrupt::default();
        let timer = Timer::default();
        let mut deadlines = Deadlines::new(&timer, &interrupt);
        deadlines.suspend();
        deadlines.arm(5, 2);
        assert_eq!(due(&deadlines), Some(Due::Left(Duration::from_secs(5))));
        assert_eq!(serving(&timer), None);
        deadlines.resume();
        assert!(matches!(due(&deadlines), Some(Due::At(_))));
        assert!(serving(&timer).is_some());
    }

    /// #1814's notes: with a periodic interrupt scheduled, a suspend and
    /// a resume are writes to the schedule the same thread serves on.
    #[test]
    fn a_suspend_beside_a_periodic_interrupt_keeps_its_thread() {
        let interrupt = Interrupt::default();
        let timer = Timer::default();
        let mut deadlines = Deadlines::new(&timer, &interrupt);
        let _periodic =
            deadlines.start_periodic(Duration::from_secs(30), Some(Duration::from_secs(30)));
        deadlines.arm(30, 2);
        let first = serving(&timer);
        deadlines.suspend();
        assert_eq!(serving(&timer), first);
        deadlines.resume();
        assert_eq!(serving(&timer), first);
    }

    /// Past the limit and past its hard stage, suspended, nothing is
    /// raised and the process goes on; resumed, the time left is what is
    /// waited before the timeout.
    #[test]
    fn a_suspended_limit_raises_nothing_until_its_time_left_after_the_resume() {
        let interrupt = Interrupt::default();
        let mut deadlines = Deadlines::new(&Timer::default(), &interrupt);
        deadlines.arm(1, 1);
        deadlines.suspend();
        thread::sleep(Duration::from_millis(2200));
        assert!(
            !interrupt.raised().load(Ordering::Relaxed),
            "suspended, nothing raised"
        );
        let resumed = Instant::now();
        deadlines.resume();
        while !interrupt.raised().load(Ordering::Relaxed) {
            assert!(
                resumed.elapsed() < Duration::from_secs(10),
                "the timer never fired"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            resumed.elapsed() >= Duration::from_millis(900),
            "{:?}",
            resumed.elapsed()
        );
        assert_eq!(interrupt.taken(), Some(1));
    }

    /// The two arms of `Z/Zend/zend_vm_def.h:10526`: the timer's fatal,
    /// and the interrupt cleared with nothing raised when signal
    /// delivery was what raised it and no PHP handler is installed.
    #[test]
    fn the_helper_clears_the_flag_and_raises_only_what_the_timer_set() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description());

        req.interrupt().raise();
        assert_eq!(check(&mut req), Ok(false));
        assert!(!req.interrupt().raised().load(Ordering::Relaxed));

        req.interrupt().elapsed(30);
        assert_eq!(
            check(&mut req),
            Err(Fatal::error(
                "Maximum execution time of 30 seconds exceeded"
            ))
        );
        assert!(!req.interrupt().raised().load(Ordering::Relaxed));
        // The second check finds nothing: php's helper clears both words.
        assert_eq!(check(&mut req), Ok(false));
    }
}
