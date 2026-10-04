//! The interrupt (design §5.6, §5.8; `CONTEXT.md`, Interrupt): the
//! engine's half of the one thing on a request that the execution timer,
//! a periodic interrupt and signal delivery raise and nothing else
//! touches. The interrupt
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
//! and [`Timer`], the request's one timer thread, which serves the
//! execution timer `max_execution_time` arms and every periodic
//! interrupt an extension starts (#1806).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
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
        return Err(Fatal::Error(format!(
            "Maximum execution time of {seconds} second{} exceeded",
            if seconds == 1 { "" } else { "s" }
        )));
    }
    req.serve_interrupt_hooks();
    Ok(req.is_pending())
}

/// The request's timer (design §5.8): every deadline the request has,
/// and the one thread the engine runs to wait for them and raise the
/// request's interrupt at each. The execution timer is one deadline:
/// what `zend_set_timeout` arms at the start of a request whose
/// `max_execution_time` is not zero and again when `set_time_limit()`
/// moves the entry, and what `zend_unset_timeout` unsets, as
/// `php_request_shutdown` and `OnUpdateTimeout` unset php's. A thread is
/// the only timed wake the standard library has, and one serves every
/// deadline: moving one is a write under the timer's mutex and a wake,
/// with no thread spawned (#1924); the thread is started when the first
/// deadline is set and joined when none is left; and a request that ends
/// before a deadline wakes it at once rather than leaving it to the
/// deadline. What is timed later is one more deadline in the
/// [`Schedule`] the thread reads, never a second thread.
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
/// timer's thread is parked in its wait or raising the interrupt, and a
/// timer with no deadline left has no thread at all. A fork needs both
/// halves (#1783). The standard library takes one process-wide mutex as
/// a thread starts and again as it ends, its stack overflow handler's
/// thread table (`std/src/sys/pal/unix/stack_overflow/thread_info.rs`),
/// and a child forked while a timer's thread stood in either copied it
/// taken, with no thread to release it, and hung at exit, where its own
/// main thread takes it. Ending wakes the thread rather than waiting out
/// its deadline, so a join costs a wake and an exit, never the seconds
/// left. That is what the engine's fork home stands on (#1807): a
/// request whose timer has no deadline has no timer thread alive, and
/// one that keeps its deadlines across a fork joins the thread and
/// starts it again over the same schedule, whose deadlines are instants.
///
/// A timer in a process forked from the one that started its thread has
/// no thread there, since a child copies its calling thread alone, as a
/// child does not inherit php's interval timer either. Its thread and
/// its schedule are forgotten, neither woken nor joined: the mutex may
/// have been copied held, and there is nothing to join. So the child
/// runs untimed, as php's does, until it sets a limit of its own.
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
/// in eight workers at once. A child not gone within five seconds is
/// hung, and killed; a fork that failed, or a child or a worker that did
/// not end with status zero, is counted apart, so neither passes as a
/// child that did not hang.
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
///         $deadline = microtime(true) + 5;
///         while ($children && microtime(true) < $deadline) {
///             $gone = pcntl_waitpid(-1, $status, WNOHANG);
///             if ($gone <= 0) { usleep(1000); continue; }
///             unset($children[$gone]);
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
#[derive(Debug, Default)]
pub(crate) struct Timer {
    shared: Arc<Shared>,
    /// The thread, while a deadline is set.
    thread: Option<JoinHandle<()>>,
    /// The process that started the thread, the only one it is in.
    started_in: Process,
    /// How many periodic interrupts were started, which numbers the next.
    periodics: u32,
}

impl Timer {
    /// `zend_unset_timeout` then `zend_set_timeout`: the execution
    /// deadline `seconds` from now, and again `hard` seconds past each
    /// timeout where `hard` is above zero; none at all for a `seconds`
    /// that is zero or below, which is php's `max_execution_time = 0`
    /// and the CLI's own default. It returns with the thread waiting
    /// where a deadline is left, and with no thread where none is.
    pub(crate) fn arm(&mut self, interrupt: &Interrupt, seconds: i64, hard: i64) {
        let execution = u64::try_from(seconds)
            .ok()
            .filter(|s| *s > 0)
            .map(|s| Execution {
                deadline: Instant::now() + Duration::from_secs(s),
                seconds,
                hard: u64::try_from(hard).ok().filter(|h| *h > 0),
            });
        self.change(interrupt, |schedule| schedule.execution = execution);
        // The old deadline is gone from the schedule, so whatever it
        // raised is raised already, and is cleared here.
        interrupt.timer_unset();
    }

    /// [`Request::start_periodic`]: a deadline `first` from now and every
    /// `every` after it, once where `every` is zero. `None` where no
    /// thread could be started to serve it.
    pub(crate) fn start_periodic(
        &mut self,
        interrupt: &Interrupt,
        first: Duration,
        every: Duration,
    ) -> Option<Periodic> {
        let of = Periodic(self.periodics);
        self.periodics = self.periodics.checked_add(1)?;
        let next = Some(Instant::now().checked_add(first)?);
        let period = Period {
            of,
            next,
            every,
            elapsed: 0,
        };
        self.change(interrupt, |schedule| schedule.periodic.push(period));
        if self.thread.is_none() {
            self.stop_periodic(interrupt, of);
            return None;
        }
        Some(of)
    }

    /// [`Request::periods_elapsed`]: the periods elapsed since the last
    /// time this was asked, and none counted from here.
    pub(crate) fn periods_elapsed(&mut self, of: Periodic) -> u64 {
        self.here();
        let mut schedule = self.shared.held();
        let period = schedule.periodic.iter_mut().find(|period| period.of == of);
        period.map_or(0, |period| std::mem::take(&mut period.elapsed))
    }

    /// [`Request::stop_periodic`]: its deadline gone and its count with it.
    pub(crate) fn stop_periodic(&mut self, interrupt: &Interrupt, of: Periodic) {
        self.change(interrupt, |schedule| {
            schedule.periodic.retain(|period| period.of != of);
        });
    }

    /// `f` over the schedule, under the mutex, and the thread woken to
    /// read it again: started where it had no thread and now has a
    /// deadline, ended where it now has none.
    fn change(&mut self, interrupt: &Interrupt, f: impl FnOnce(&mut Schedule)) {
        self.here();
        let idle = {
            let mut schedule = self.shared.held();
            f(&mut schedule);
            self.shared.woken.notify_all();
            schedule.next().is_none()
        };
        if idle {
            self.end();
        } else if self.thread.is_none() {
            self.start(interrupt);
        }
    }

    /// Spawn the thread and return once it waits; a thread the system
    /// refuses leaves the request untimed, as before any deadline.
    fn start(&mut self, interrupt: &Interrupt) {
        self.shared.held().phase = Phase::Starting;
        let shared = Arc::clone(&self.shared);
        let raised = interrupt.clone();
        let Ok(thread) = thread::Builder::new()
            .name(String::from("php-request-timer"))
            .spawn(move || shared.serve(&raised))
        else {
            return;
        };
        self.shared.started();
        self.thread = Some(thread);
        self.started_in = Process::current();
    }

    /// Tell the thread to raise nothing more, and join it.
    fn end(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.shared.held().phase = Phase::Unset;
            self.shared.woken.notify_all();
            // A thread that panicked has ended all the same.
            let _ = thread.join();
        }
    }

    /// In a process forked from the one that started the thread, the
    /// thread and its schedule forgotten (see [`Timer`]), so what follows
    /// starts afresh.
    fn here(&mut self) {
        if self.thread.is_some() && !self.started_in.is_current() {
            std::mem::forget(self.thread.take());
            std::mem::forget(std::mem::take(&mut self.shared));
        }
    }
}

impl Drop for Timer {
    /// The request's end: every deadline unset and the thread joined, or
    /// forgotten in a forked child.
    fn drop(&mut self) {
        self.here();
        self.end();
    }
}

/// A process, by its id, `getpid`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
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

/// Every deadline the request has, and where the thread stands: what the
/// thread and the request share under the timer's mutex.
#[derive(Debug, Default)]
struct Schedule {
    phase: Phase,
    /// The execution timer's deadline, while one is set.
    execution: Option<Execution>,
    /// Every periodic interrupt started and not yet stopped.
    periodic: Vec<Period>,
}

/// One periodic interrupt a request was asked for (#1806), by the number
/// its request's timer gave it: what [`Request::start_periodic`] answers
/// and the extension that asked keeps, in its request state, to read
/// its periods with and to stop it by. It names nothing outside the
/// request that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Periodic(u32);

/// A periodic interrupt, started.
#[derive(Debug)]
struct Period {
    of: Periodic,
    /// Its next deadline, `None` once a period of zero has raised its one.
    next: Option<Instant>,
    every: Duration,
    /// The periods elapsed since the extension last read them.
    elapsed: u64,
}

/// The execution timer, armed.
#[derive(Debug, Clone, Copy)]
struct Execution {
    deadline: Instant,
    /// What it was armed with, php's `EG(timeout_seconds)`.
    seconds: i64,
    /// `EG(hard_timeout)`, where it is above zero.
    hard: Option<u64>,
}

impl Schedule {
    /// The earliest deadline, `None` where none is set.
    fn next(&self) -> Option<Instant> {
        let periodic = self.periodic.iter().filter_map(|period| period.next);
        self.execution
            .map(|execution| execution.deadline)
            .into_iter()
            .chain(periodic)
            .min()
    }

    /// What each deadline at or before `now` raises.
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
    /// interrupt raised, as Excimer's handler stores `EG(vm_interrupt)`.
    /// The count waits for the extension's hook to read it.
    fn elapse(&mut self, interrupt: &Interrupt, now: Instant) {
        if let Some(execution) = &mut self.execution
            && execution.deadline <= now
        {
            if let Some(hard) = execution.hard
                && interrupt.timed_out()
            {
                terminate(execution.seconds, hard);
            }
            interrupt.elapsed(execution.seconds);
            match execution.hard {
                Some(hard) => execution.deadline = now + Duration::from_secs(hard),
                None => self.execution = None,
            }
        }
        for period in &mut self.periodic {
            let Some(next) = period.next.filter(|next| *next <= now) else {
                continue;
            };
            let late = (now - next).as_nanos().checked_div(period.every.as_nanos());
            let periods = late.map_or(1, |late| {
                u64::try_from(late).unwrap_or(u64::MAX).saturating_add(1)
            });
            period.elapsed = period.elapsed.saturating_add(periods);
            let ahead = period
                .every
                .saturating_mul(u32::try_from(periods).unwrap_or(u32::MAX));
            period.next = late.and_then(|_| next.checked_add(ahead));
            interrupt.raise();
        }
    }
}

/// What the timer's thread and the request share: the schedule, and the
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
    fn serve(&self, interrupt: &Interrupt) {
        let mut schedule = self.held();
        schedule.phase = Phase::Waiting;
        self.woken.notify_all();
        while schedule.phase != Phase::Unset {
            let now = Instant::now();
            schedule = match schedule.next() {
                Some(at) if at <= now => {
                    schedule.elapse(interrupt, now);
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
    let _ = std::io::stderr().write_all(line.as_bytes());
    std::process::exit(124)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use php_unit::{OpArray, Opcode, Opline, OplineIdx, Unit};

    use super::*;
    use crate::engine::{Engine, RequestDescription};
    use crate::probe::{Asked, Probe};
    use crate::tier::{Nop, Site, execute};

    /// php's text for a timer armed with one second
    /// (`Z/Zend/zend_execute_API.c:1441`).
    const TIMED_OUT: &str = "Maximum execution time of 1 second exceeded";

    /// A unit whose main op array is one `JMP` to itself: the back-edge
    /// of `for (;;) {}`, safepoint number one, which runs until the
    /// interrupt stops it.
    fn looping() -> Arc<Unit> {
        Arc::new(Unit {
            main: OpArray {
                oplines: vec![Opline {
                    opcode: Opcode::Jmp,
                    op1: 0,
                    out: 1,
                    ..Opline::default()
                }]
                .into_boxed_slice(),
                safepoints: vec![OplineIdx(0), OplineIdx(0)].into_boxed_slice(),
                ..OpArray::default()
            },
            ..Unit::default()
        })
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
            execute::<Probe>(&mut req, frame),
            Err(Fatal::Error(String::from(TIMED_OUT)))
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
            execute::<Probe>(&mut req, frame),
            Err(Fatal::Error(String::from(TIMED_OUT)))
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
        let mut req = Request::new(Arc::clone(&engine.shared), description());
        let frame = req.pushed(looping());
        let armed = Instant::now();

        assert_eq!(
            execute::<Nop>(&mut req, frame),
            Err(Fatal::Error(String::from(TIMED_OUT)))
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
        let mut timer = Timer::default();
        timer.arm(&interrupt, 1, 0);
        let armed = Instant::now();
        while !interrupt.raised().load(Ordering::Relaxed) {
            assert!(
                armed.elapsed() < Duration::from_secs(10),
                "the timer never fired"
            );
            thread::sleep(Duration::from_millis(10));
        }
        timer.arm(&interrupt, 0, 0);
        assert_eq!(
            interrupt.taken(),
            None,
            "raised, and the timer's word cleared"
        );
    }

    /// The thread serving a timer, by its id, while it has one.
    fn serving(timer: &Timer) -> Option<thread::ThreadId> {
        timer.thread.as_ref().map(|thread| thread.thread().id())
    }

    /// #1783: a deadline set returns with the thread waiting, and the
    /// last one unset, or the timer dropped, returns with the thread's
    /// body done and its share of the timer released; the unset wakes it
    /// rather than waiting out the thirty seconds it was armed for. That
    /// the thread has then ended, the standard library's exit behind it
    /// too, is the join's, which nothing in the process can observe
    /// short of counting its threads: the fork stress on [`Timer`] is
    /// what holds it.
    #[test]
    fn a_timers_thread_waits_once_armed_and_is_done_once_unset() {
        let interrupt = Interrupt::default();
        let began = Instant::now();
        for _ in 0..100 {
            let mut timer = Timer::default();
            timer.arm(&interrupt, 30, 2);
            assert_eq!(
                timer.shared.held().phase,
                Phase::Waiting,
                "armed and waiting"
            );
            timer.arm(&interrupt, 0, 2);
            assert_eq!(serving(&timer), None, "unset, and no thread");
            assert_eq!(
                Arc::strong_count(&timer.shared),
                1,
                "the thread let go of the timer before the unset returned"
            );
            timer.arm(&interrupt, 30, 2);
            let shared = Arc::downgrade(&timer.shared);
            drop(timer);
            assert!(
                shared.upgrade().is_none(),
                "the thread let go of the timer before the drop returned"
            );
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "each unset woke its thread rather than waiting out the deadline"
        );
    }

    /// #1924: `set_time_limit()` over an armed timer moves its deadline
    /// and spawns nothing, so the thread that served the first deadline
    /// serves every one after it.
    #[test]
    fn rearming_a_timer_keeps_its_thread() {
        let interrupt = Interrupt::default();
        let mut timer = Timer::default();
        timer.arm(&interrupt, 30, 2);
        let first = serving(&timer);
        assert!(first.is_some(), "armed, and served");
        for seconds in 1..=1000 {
            timer.arm(&interrupt, 30 + seconds, 2);
            assert_eq!(serving(&timer), first, "the same thread");
        }
    }

    /// php's `max_execution_time = 0`, the CLI's default: no deadline, no
    /// thread, and an interrupt nothing raises.
    #[test]
    fn a_request_with_no_time_limit_arms_no_timer() {
        let interrupt = Interrupt::default();
        let mut timer = Timer::default();
        timer.arm(&interrupt, 0, 2);
        timer.arm(&interrupt, -1, 2);
        assert_eq!(serving(&timer), None);
        assert!(!interrupt.raised().load(Ordering::Relaxed));
    }

    /// #1806: a periodic interrupt counts every period elapsed, the ones
    /// nobody read among them, and hands each count over once; the
    /// periods are laid on the grid `first` began, so a late read loses
    /// none.
    #[test]
    fn a_periodic_interrupt_counts_the_periods_nobody_read() {
        let interrupt = Interrupt::default();
        let mut timer = Timer::default();
        let every = Duration::from_millis(5);
        let began = Instant::now();
        let periodic = timer.start_periodic(&interrupt, every, every);
        thread::sleep(every * 20);
        let read = periodic.map_or(0, |periodic| timer.periods_elapsed(periodic));
        let at_most = u64::try_from(began.elapsed().as_millis() / every.as_millis()).unwrap_or(0);
        assert!(
            (15..=at_most).contains(&read),
            "{read} of at most {at_most}"
        );
        assert!(interrupt.raised().load(Ordering::Relaxed));
        let again = periodic.map_or(9, |periodic| timer.periods_elapsed(periodic));
        assert!(again <= 1, "each count is handed over once, not {again}");
    }

    /// A period of zero raises the interrupt once, `first` from now, as
    /// Excimer's one-shot timer does; stopping the last periodic
    /// interrupt ends the thread, and dropping a timer that still has
    /// one joins it.
    #[test]
    fn a_periodic_interrupt_of_no_period_raises_once_and_its_thread_ends() {
        let interrupt = Interrupt::default();
        let mut timer = Timer::default();
        let once = timer.start_periodic(&interrupt, Duration::from_millis(5), Duration::ZERO);
        assert!(serving(&timer).is_some(), "started, and served");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(once.map(|once| timer.periods_elapsed(once)), Some(1));
        if let Some(once) = once {
            timer.stop_periodic(&interrupt, once);
        }
        assert_eq!(serving(&timer), None, "stopped, and no thread");
        assert_eq!(Arc::strong_count(&timer.shared), 1);

        let every = timer.start_periodic(&interrupt, Duration::ZERO, Duration::from_secs(30));
        assert!(every.is_some() && serving(&timer).is_some());
        let shared = Arc::downgrade(&timer.shared);
        drop(timer);
        assert!(shared.upgrade().is_none(), "the drop joined the thread");
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
            Err(Fatal::Error(String::from(
                "Maximum execution time of 30 seconds exceeded"
            )))
        );
        assert!(!req.interrupt().raised().load(Ordering::Relaxed));
        // The second check finds nothing: php's helper clears both words.
        assert_eq!(check(&mut req), Ok(false));
    }
}
