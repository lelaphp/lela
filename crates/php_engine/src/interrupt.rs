//! The interrupt (design §5.6, §5.8; `CONTEXT.md`, Interrupt): the
//! engine's half of the one thing on a request that the execution timer
//! and signal delivery raise and nothing else touches. The interrupt
//! itself is [`crate::api::request::Interrupt`], on the request, because
//! signal delivery is an extension's and the compiled tier tests the
//! same atomic once per loop iteration through its `ExitCtx`
//! (`jit-design.md` §4.3).
//!
//! What is here is [`check`], the one the loop performs at a back-edge
//! `JMP*`, at a frame's activation and after every `Flow::Resume` and
//! nowhere else, [`interrupted`], what a check that finds it raised
//! does, and [`Timer`], the execution timer `max_execution_time` arms.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use php_heap::Fatal;

use crate::api::request::{Interrupt, Request};

/// The interrupt check (design §5.8): one relaxed load and a branch, at
/// the three sites and nowhere else. php's check costs the same load,
/// and an interrupt nobody raised costs nothing beyond it.
#[inline]
pub(crate) fn check(req: &mut Request) -> Result<(), Fatal> {
    if req.interrupt().raised().load(Ordering::Relaxed) {
        return interrupted(req);
    }
    Ok(())
}

/// What a check that found the interrupt raised does
/// (`Z/Zend/zend_vm_def.h:10526`): it clears the interrupt and then
/// either raises php's timeout fatal, when the execution timer was what
/// raised it, or dispatches the signals that were delivered to their
/// PHP handlers, which is php's `zend_interrupt_function`. Nothing
/// installs one until an extension delivers signals, and php does
/// nothing in that case, so neither does this.
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
pub(crate) fn interrupted(req: &mut Request) -> Result<(), Fatal> {
    match req.interrupt().taken() {
        Some(seconds) => Err(Fatal::Error(format!(
            "Maximum execution time of {seconds} second{} exceeded",
            if seconds == 1 { "" } else { "s" }
        ))),
        None => Ok(()),
    }
}

/// The engine's execution timer (design §5.8): what `zend_set_timeout`
/// arms at the start of a request whose `max_execution_time` is not
/// zero and again when `set_time_limit()` moves the entry, and what its
/// drop unsets, as `php_request_shutdown` and `OnUpdateTimeout` unset
/// php's. One thread per armed timer, which is the only timed wake the
/// standard library has; a request that is served before the seconds
/// elapse wakes it at once rather than leaving it to the deadline.
#[derive(Debug)]
pub(crate) struct Timer {
    unset: Arc<Unset>,
    interrupt: Interrupt,
}

impl Timer {
    /// Armed for `seconds` on `interrupt`, and nothing at all for a
    /// `seconds` that is zero or below, which is php's
    /// `max_execution_time = 0` and the CLI's own default.
    pub(crate) fn arm(interrupt: &Interrupt, seconds: i64) -> Option<Timer> {
        let elapses = Duration::from_secs(u64::try_from(seconds).ok().filter(|s| *s > 0)?);
        let unset = Arc::new(Unset::default());
        let waited = Arc::clone(&unset);
        let raised = interrupt.clone();
        thread::Builder::new()
            .name(String::from("php-execution-timer"))
            .spawn(move || waited.elapse(&raised, elapses, seconds))
            .ok()?;
        Some(Timer {
            unset,
            interrupt: interrupt.clone(),
        })
    }
}

impl Drop for Timer {
    /// `zend_unset_timeout`: the thread told to raise nothing, and then
    /// the timer's word cleared, in that order, since the thread holds
    /// the mutex while it decides and raises, so what it raised before
    /// the word is cleared is cleared with it.
    fn drop(&mut self) {
        self.unset.now();
        self.interrupt.timer_unset();
    }
}

/// What the timer's thread waits on: the word its drop sets, and the
/// variable it is woken through.
#[derive(Debug, Default)]
struct Unset {
    set: Mutex<bool>,
    woken: Condvar,
}

impl Unset {
    /// The thread's whole body: wait until the deadline or until the
    /// timer is dropped, and raise the interrupt when the deadline is
    /// what came first. `zend_timeout_handler` sets both words and
    /// returns (`Z/Zend/zend_execute_API.c:1516-1518`); so does this,
    /// and the loop's next check is what raises the fatal.
    fn elapse(&self, interrupt: &Interrupt, elapses: Duration, seconds: i64) {
        let deadline = Instant::now() + elapses;
        let mut set = self.held();
        while !*set {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                interrupt.elapsed(seconds);
                return;
            };
            set = match self.woken.wait_timeout(set, left) {
                Ok((set, _)) => set,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Wake the thread and tell it not to raise anything.
    fn now(&self) {
        *self.held() = true;
        self.woken.notify_all();
    }

    /// The word, whatever a panic on another thread left behind: what
    /// this guards is a `bool` only ever set to true, so a poisoned
    /// `MutexGuard` is as good as a clean one.
    fn held(&self) -> MutexGuard<'_, bool> {
        self.set.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use php_unit::{OpArray, Opcode, Opline, OplineIdx, Unit};

    use super::*;
    use crate::api::values::PhpString;
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
            script_name: Some(PhpString::from_bytes(b"witness.php")),
            ..RequestDescription::default()
        }
    }

    /// Design §5.8: the check at a frame's activation, before the site,
    /// so a request whose time is up raises the fatal without the tier
    /// being asked anything.
    #[test]
    fn the_interrupt_is_tested_at_an_activation() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description());
        let frame = req.push_frame(looping());
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
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description());
        let frame = req.push_frame(looping());
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
        let engine = Engine::builder()
            .ini("max_execution_time", "1")
            .tier::<Nop>()
            .build();
        let mut req = Request::new(Arc::clone(&engine.shared), description());
        let frame = req.push_frame(looping());
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
        let timer = Timer::arm(&interrupt, 1);
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

    /// php's `max_execution_time = 0`, the CLI's default: no timer at
    /// all, and an interrupt nothing raises.
    #[test]
    fn a_request_with_no_time_limit_arms_no_timer() {
        let interrupt = Interrupt::default();
        assert!(Timer::arm(&interrupt, 0).is_none());
        assert!(Timer::arm(&interrupt, -1).is_none());
        assert!(!interrupt.raised().load(Ordering::Relaxed));
    }

    /// The two arms of `Z/Zend/zend_vm_def.h:10526`: the timer's fatal,
    /// and the interrupt cleared with nothing raised when signal
    /// delivery was what raised it and no PHP handler is installed.
    #[test]
    fn the_helper_clears_the_flag_and_raises_only_what_the_timer_set() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description());

        req.interrupt().raise();
        assert_eq!(check(&mut req), Ok(()));
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
        assert_eq!(check(&mut req), Ok(()));
    }
}
