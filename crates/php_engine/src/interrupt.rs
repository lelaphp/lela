//! The interrupt (design §5.6, §5.8; `CONTEXT.md`, Interrupt): the
//! engine's half of the one thing on a request that the execution timer
//! and signal delivery raise and nothing else touches. The interrupt
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
//! and [`Timer`], the execution timer `max_execution_time` arms.
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

/// The engine's execution timer (design §5.8): what `zend_set_timeout`
/// arms at the start of a request whose `max_execution_time` is not
/// zero and again when `set_time_limit()` moves the entry, and what its
/// drop unsets, as `php_request_shutdown` and `OnUpdateTimeout` unset
/// php's. One thread per armed timer, which is the only timed wake the
/// standard library has; a request that is served before the seconds
/// elapse wakes it at once rather than leaving it to the deadline.
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
/// **Its thread is never caught starting or ending.** Arming returns
/// once the thread waits, and the drop returns once the thread has ended
/// (`JoinHandle::join`, `pthread_join`): so while PHP code runs, a
/// timer's thread is parked in its wait holding nothing, and a dropped
/// timer has no thread at all. A fork needs both halves (#1783). The
/// standard library takes one process-wide mutex as a thread starts and
/// again as it ends, its stack overflow handler's thread table
/// (`std/src/sys/pal/unix/stack_overflow/thread_info.rs`), and a child
/// forked while a timer's thread stood in either copied it taken, with
/// no thread to release it, and hung at exit, where its own main thread
/// takes it. The drop wakes the thread rather than waiting out its
/// deadline, so a join costs a wake and an exit, never the seconds left.
/// That is what the engine's fork home stands on (#1807): a request
/// whose timer is unset has no timer thread alive.
///
/// A timer dropped in a process forked from the one that armed it has no
/// thread there, since a child copies its calling thread alone, as a
/// child does not inherit php's interval timer either. It is forgotten,
/// neither woken nor joined: the mutex waking takes may have been copied
/// held, and there is nothing to join.
///
/// The stress below forks a child straight after each `set_time_limit()`
/// in eight workers at once, and counts as hung a child not gone within
/// five seconds, which it kills.
///
/// ```php
/// for ($w = 0; $w < 8; $w++) {
///     if (($workers[] = pcntl_fork()) !== 0) { continue; }
///     $hung = 0;
///     for ($forked = 0; $forked < 1875; $forked += 25) {
///         $children = [];
///         for ($i = 0; $i < 25; $i++) {
///             set_time_limit(30);
///             if (($child = pcntl_fork()) === 0) { exit(0); }
///             $children[$child] = true;
///         }
///         $deadline = microtime(true) + 5;
///         while ($children && microtime(true) < $deadline) {
///             $gone = pcntl_waitpid(-1, $status, WNOHANG);
///             if ($gone > 0) { unset($children[$gone]); } else { usleep(1000); }
///         }
///         foreach ($children as $child => $_) {
///             posix_kill($child, SIGKILL);
///             pcntl_waitpid($child, $status);
///             $hung++;
///         }
///     }
///     exit(min($hung, 255));
/// }
/// $hung = 0;
/// foreach ($workers as $worker) {
///     pcntl_waitpid($worker, $status);
///     $hung += pcntl_wexitstatus($status);
/// }
/// echo "$hung of 15000 children hung\n";
/// ```
/// ```expect process wall=30s
/// 0 of 15000 children hung
/// ```
#[derive(Debug)]
pub(crate) struct Timer {
    unset: Arc<Unset>,
    interrupt: Interrupt,
    /// The thread, which the drop takes to join or forget.
    thread: Option<JoinHandle<()>>,
    /// The process that armed it, the only one its thread is in.
    armed_in: u32,
}

impl Timer {
    /// Armed for `seconds` on `interrupt`, and again for `hard` seconds
    /// past each timeout where `hard` is above zero; nothing at all for
    /// a `seconds` that is zero or below, which is php's
    /// `max_execution_time = 0` and the CLI's own default. It returns
    /// once the thread waits.
    pub(crate) fn arm(interrupt: &Interrupt, seconds: i64, hard: i64) -> Option<Timer> {
        let elapses = Duration::from_secs(u64::try_from(seconds).ok().filter(|s| *s > 0)?);
        let hard = u64::try_from(hard).ok().filter(|h| *h > 0);
        let unset = Arc::new(Unset::default());
        let waited = Arc::clone(&unset);
        let raised = interrupt.clone();
        let thread = thread::Builder::new()
            .name(String::from("php-execution-timer"))
            .spawn(move || waited.elapse(&raised, elapses, seconds, hard))
            .ok()?;
        unset.started();
        Some(Timer {
            unset,
            interrupt: interrupt.clone(),
            thread: Some(thread),
            armed_in: std::process::id(),
        })
    }
}

impl Drop for Timer {
    /// `zend_unset_timeout`: the thread told to raise nothing and joined,
    /// and then the timer's word cleared, in that order, since the thread
    /// holds the mutex while it decides and raises, so what it raised
    /// before it ended is cleared with it. In a forked child the thread
    /// is forgotten instead (see [`Timer`]).
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            if std::process::id() == self.armed_in {
                self.unset.now();
                // A thread that panicked has ended all the same.
                let _ = thread.join();
            } else {
                std::mem::forget(thread);
            }
        }
        self.interrupt.timer_unset();
    }
}

/// Where the timer's thread stands, which only ever moves forward.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Spawned, and not yet waiting: arming waits this out.
    #[default]
    Starting,
    /// Waiting for its deadline or its drop.
    Waiting,
    /// Told by the drop to raise nothing and end.
    Unset,
}

/// What the timer's thread and its owner share: the phase, and the
/// variable each wakes the other through.
#[derive(Debug, Default)]
struct Unset {
    phase: Mutex<Phase>,
    woken: Condvar,
}

impl Unset {
    /// The thread's whole body: say it waits, then wait until the
    /// deadline or until the timer is dropped, and raise the interrupt
    /// when the deadline is what came first. `zend_timeout_handler` sets
    /// both words and returns (`Z/Zend/zend_execute_API.c:1516-1518`);
    /// so does this, and the loop's next check is what raises the fatal.
    /// Then, where there is a `hard` wait, the same again after it: a
    /// deadline that finds the last timeout still unread ends the process
    /// ([`terminate`]), and one that finds it taken is a timeout of its
    /// own (`:1482`).
    fn elapse(&self, interrupt: &Interrupt, elapses: Duration, seconds: i64, hard: Option<u64>) {
        let mut deadline = Instant::now() + elapses;
        let mut phase = self.held();
        *phase = Phase::Waiting;
        self.woken.notify_all();
        loop {
            while *phase != Phase::Unset {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                phase = match self.woken.wait_timeout(phase, left) {
                    Ok((phase, _)) => phase,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
            if *phase == Phase::Unset {
                return;
            }
            if let Some(hard) = hard
                && interrupt.timed_out()
            {
                terminate(seconds, hard);
            }
            interrupt.elapsed(seconds);
            let Some(hard) = hard else {
                return;
            };
            deadline = Instant::now() + Duration::from_secs(hard);
        }
    }

    /// Wait until the thread has started and waits: past that point the
    /// standard library's start is behind it.
    fn started(&self) {
        let mut phase = self.held();
        while *phase == Phase::Starting {
            phase = match self.woken.wait(phase) {
                Ok(phase) => phase,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    /// Wake the thread and tell it not to raise anything.
    fn now(&self) {
        *self.held() = Phase::Unset;
        self.woken.notify_all();
    }

    /// The phase, whatever a panic on another thread left behind: what
    /// this guards only ever moves forward, so a poisoned `MutexGuard` is
    /// as good as a clean one.
    fn held(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
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
        let timer = Timer::arm(&interrupt, 1, 0);
        let armed = Instant::now();
        while !interrupt.raised().load(Ordering::Relaxed) {
            assert!(
                armed.elapsed() < Duration::from_secs(10),
                "the timer never fired"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(timer);
        assert_eq!(
            interrupt.taken(),
            None,
            "raised, and the timer's word cleared"
        );
    }

    /// #1783: arming returns with the thread waiting, and dropping
    /// returns with it ended, its share of the timer released, however
    /// often `set_time_limit()` replaces one; and the drop wakes it
    /// rather than waiting out the thirty seconds it was armed for.
    #[test]
    fn no_thread_a_timer_started_outlives_its_drop() {
        let interrupt = Interrupt::default();
        let began = Instant::now();
        for _ in 0..200 {
            let timer = Timer::arm(&interrupt, 30, 2);
            assert_eq!(
                timer.as_ref().map(|timer| *timer.unset.held()),
                Some(Phase::Waiting),
                "armed and waiting"
            );
            let shared = timer.as_ref().map(|timer| Arc::downgrade(&timer.unset));
            drop(timer);
            assert!(
                shared.and_then(|shared| shared.upgrade()).is_none(),
                "the thread let go of the timer before the drop returned"
            );
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "each drop woke its thread rather than waiting out the deadline"
        );
    }

    /// php's `max_execution_time = 0`, the CLI's default: no timer at
    /// all, and an interrupt nothing raises.
    #[test]
    fn a_request_with_no_time_limit_arms_no_timer() {
        let interrupt = Interrupt::default();
        assert!(Timer::arm(&interrupt, 0, 2).is_none());
        assert!(Timer::arm(&interrupt, -1, 2).is_none());
        assert!(!interrupt.raised().load(Ordering::Relaxed));
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
