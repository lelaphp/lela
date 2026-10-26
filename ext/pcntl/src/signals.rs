//! The signal natives (`pcntl.c`'s `pcntl_signal` to
//! `pcntl_async_signals`) and the dispatch the engine's interrupt check
//! serves. What a signal handler does is `ffi`'s, posting to the
//! engine's signal record (design §5.8, revision 38); what is here is
//! php's handler table, the dispatch that calls into it, and the switch
//! that hands this request's interrupt to the record.

use std::collections::{BTreeMap, VecDeque};

use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
#[cfg(target_os = "linux")]
use php_ext::abi::OrFalse;
use php_ext::abi::{Beneath, Passed, Ref, Untyped, chain};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::coerce::{is_callable, to_callable, value_name};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, docref, fatal, leave_pending,
};
use php_ext::request::{Receiving, Request, Siginfo, take_signals};
use php_ext::values::{Owner, PhpArray, Value, ValueRef, walk};

use crate::ffi::{self, Disposition, SignalSet};
use crate::throwables::Throwables;
use crate::{State, failed};

/// `PCNTL_G(num_signals)`: one past the last signal number, `NSIG` where
/// the platform has no real-time signals and `SIGRTMAX + 1` where it does.
#[cfg(target_vendor = "apple")]
const NUMBERS: i64 = 32;
#[cfg(not(target_vendor = "apple"))]
const NUMBERS: i64 = 65;

/// php's handler table and how this request takes its signals.
#[derive(Default)]
pub(crate) struct Table {
    /// `PCNTL_G(php_signal_table)`: what `pcntl_signal` was handed for
    /// each number, an owner of each the table's.
    handlers: BTreeMap<i32, Owner<Value>>,
    /// `PCNTL_G(async_signals)`.
    delivery: Delivery,
    /// `PCNTL_G(processing_signal_queue)`: a dispatch is running, so a
    /// handler's own safepoints do not start another.
    dispatching: bool,
    /// What `PCNTL_G(head)` holds between two dispatches: the signals a
    /// dispatch took and a throwing handler kept it from playing, put
    /// back to be played first by the next one (8.5.12). The record
    /// keeps the rest.
    held: VecDeque<Siginfo>,
}

/// How the signals the record took reach this request's handlers.
#[derive(Default)]
enum Delivery {
    /// At `pcntl_signal_dispatch()` alone.
    #[default]
    Explicit,
    /// At the loop's next interrupt check as well, the record raising this
    /// request's interrupt while the [`Receiving`] stands; `None` where
    /// the record had no room for one more. Held, and read by nothing: its
    /// drop is what takes the interrupt off the record.
    Asynchronous { _receiving: Option<Receiving> },
}

/// `pcntl_signal(int $signal, callable|int $handler, bool
/// $restart_syscalls = true): bool`: the handler recorded for the
/// signal, and the signal's disposition set, `SIG_DFL` and `SIG_IGN`
/// as given and anything callable as `ffi`'s handler with
/// every signal blocked while it runs (`php_signal4(.., 1)`).
/// `SIGALRM` defaults to interrupting system calls, as it always did.
///
/// ```php
/// var_dump(pcntl_signal(SIGWINCH, function () {}), pcntl_signal(SIGWINCH, SIG_DFL));
/// try { pcntl_signal(0, SIG_DFL); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_signal(32, SIG_DFL); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_signal(SIGWINCH, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_signal(SIGWINCH, 'nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(pcntl_get_last_error());
/// pcntl_signal(SIGKILL, SIG_IGN);
/// ```
/// ```expect exit=255 process
/// bool(true)
/// bool(true)
/// pcntl_signal(): Argument #1 ($signal) must be greater than or equal to 1
/// pcntl_signal(): Argument #1 ($signal) must be less than 32
/// pcntl_signal(): Argument #2 ($handler) must be either SIG_DFL or SIG_IGN when an integer value is given
/// pcntl_signal(): Argument #2 ($handler) must be of type callable|int, string given
/// int(22)
///
/// Fatal error: Error installing signal handler for 9 in %s on line 8
/// ```
#[php_function]
pub(crate) fn pcntl_signal(
    req: &mut Request,
    signal: i64,
    handler: Untyped<Value>,
    #[php(default = true)] restart_syscalls: Passed<bool>,
) -> Result<bool, Thrown> {
    if signal < 1 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 1"));
    }
    if signal >= NUMBERS {
        return Err(arg_value_error(req, 1, &format!("be less than {NUMBERS}")));
    }
    let number = i32::try_from(signal).unwrap_or(i32::MAX);
    let restart = match restart_syscalls {
        Passed::Given(restart) => restart,
        Passed::Omitted => number != Signal::SIGALRM as i32,
    };
    let handler = handler.0.deref();
    let disposition = match handler.as_int() {
        Some(0) => Disposition::Default,
        Some(1) => Disposition::Ignore,
        Some(_) => {
            return Err(arg_value_error(
                req,
                2,
                "be either SIG_DFL or SIG_IGN when an integer value is given",
            ));
        }
        None if is_callable(req, &handler) => Disposition::Post,
        None => {
            req.state::<State>().last_error = libc::EINVAL;
            let given = value_name(req, &handler);
            return Err(arg_type_error(
                req,
                2,
                &[b"be of type callable|int, ".as_slice(), &given, b" given"].concat(),
            ));
        }
    };
    let mask_all = disposition == Disposition::Post;
    // php's `php_signal` goes through `zend_sigaction`, whose refusal is
    // the fatal (`Z/Zend/zend_signal.c:258`); pcntl's own "Error assigning
    // signal" warning is behind it and never reached.
    if ffi::install(number, disposition, restart, mask_all).is_err() {
        return Err(fatal(
            req,
            &format!("Error installing signal handler for {number}"),
        ));
    }
    let handler = Owner::take(req, handler);
    if let Some(previous) = req
        .state::<State>()
        .signals
        .handlers
        .insert(number, handler)
    {
        previous.give_up(req);
    }
    Ok(true)
}

/// `pcntl_signal_get_handler(int $signal)`: what `pcntl_signal` was
/// last handed for the signal, or `SIG_DFL` where it was handed nothing.
///
/// ```php
/// var_dump(pcntl_signal_get_handler(SIGURG));
/// pcntl_signal(SIGURG, SIG_IGN);
/// var_dump(pcntl_signal_get_handler(SIGURG));
/// pcntl_signal(SIGURG, 'strlen');
/// var_dump(pcntl_signal_get_handler(SIGURG));
/// try { pcntl_signal_get_handler(32); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect process
/// int(0)
/// int(1)
/// string(6) "strlen"
/// pcntl_signal_get_handler(): Argument #1 ($signal) must be between 1 and 31
/// ```
#[php_function]
pub(crate) fn pcntl_signal_get_handler(
    req: &mut Request,
    signal: i64,
) -> Result<Untyped<Value>, Thrown> {
    let highest = NUMBERS - 1;
    if !(1..=highest).contains(&signal) {
        return Err(arg_value_error(
            req,
            1,
            &format!("be between 1 and {highest}"),
        ));
    }
    let number = i32::try_from(signal).unwrap_or(i32::MAX);
    let held = req
        .state::<State>()
        .signals
        .handlers
        .get(&number)
        .map(|handler| **handler);
    Ok(Untyped(match held {
        Some(handler) => handler,
        None => Value::int(0),
    }))
}

/// `pcntl_signal_dispatch(): bool`: the handlers of every signal the
/// process took since the last dispatch, called in the order the signals
/// arrived.
///
/// The signal here is sent by a forked child, and its ordering is fixed
/// by construction: nothing but a dispatch calls the handler, and the
/// witness dispatches until the handler has been called, so a signal the
/// kernel hands to another thread of the process, and whose handler
/// posts it late, is taken by a later dispatch rather than missed.
///
/// ```php
/// $handled = false;
/// pcntl_signal(SIGUSR1, function ($signo, $info) {
///     echo "handled ", $signo === SIGUSR1 ? 'SIGUSR1' : $signo, " from ", $info['pid'] === $GLOBALS['child'] ? 'the child' : 'elsewhere', "\n";
///     $GLOBALS['handled'] = true;
/// });
/// $child = pcntl_fork();
/// if ($child === 0) { posix_kill(posix_getppid(), SIGUSR1); exit(0); }
/// pcntl_waitpid($child, $status);
/// echo "waited\n";
/// while (true) { $dispatched = pcntl_signal_dispatch(); if ($handled) break; usleep(1000); }
/// var_dump($dispatched, pcntl_signal_dispatch());
/// ```
/// ```expect process
/// waited
/// handled SIGUSR1 from the child
/// bool(true)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn pcntl_signal_dispatch(req: &mut Request) -> Result<bool, Thrown> {
    dispatch(req)?;
    Ok(true)
}

/// `pcntl_interrupt_function` (`pcntl.c:1922-1927`), pcntl's link of
/// `zend_interrupt_function`: the signals taken dispatched
/// ([`dispatch`]), what a handler threw left pending, and then the one it
/// saved served.
pub(crate) fn interrupt(
    req: &mut Request,
    beneath: Beneath<'_, chain::Interrupt>,
) -> Result<(), Thrown> {
    if let Err(thrown) = dispatch(req) {
        leave_pending(req, thrown);
    }
    beneath.serve(req)
}

/// `pcntl_signal_dispatch()` the function (`pcntl.c:1366`): every signal
/// posted under a number the table holds, taken off the record in the
/// order it arrived, and its handler called with the number and the
/// `siginfo_t` array where the table holds a callable; one the table
/// holds `SIG_DFL` or `SIG_IGN` for is dropped. Every signal is blocked
/// while the queue plays, as php blocks them, and a handler that throws
/// ends the dispatch with what is left put back, played first by the
/// next dispatch, which under `pcntl_async_signals(true)` the interrupt
/// raised again asks for once the throwable is handled (8.5.12). The
/// engine's interrupt check serves it through [`interrupt`].
///
/// ```php
/// $called = [];
/// pcntl_signal(SIGUSR1, function () use (&$called) { $called[] = 'USR1'; throw new Exception('thrown'); });
/// pcntl_signal(SIGUSR2, function () use (&$called) { $called[] = 'USR2'; });
/// pcntl_signal(SIGHUP, function () use (&$called) { $called[] = 'HUP'; });
/// posix_kill(posix_getpid(), SIGUSR1); posix_kill(posix_getpid(), SIGUSR2); posix_kill(posix_getpid(), SIGHUP);
/// try { pcntl_signal_dispatch(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// echo implode(', ', $called), "\n";
/// pcntl_signal_dispatch();
/// echo implode(', ', $called), "\n";
/// $called = [];
/// posix_kill(posix_getpid(), SIGUSR1); posix_kill(posix_getpid(), SIGUSR2);
/// pcntl_async_signals(true);
/// try { posix_kill(posix_getpid(), SIGHUP); echo "not reached\n"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// usleep(1000);
/// echo implode(', ', $called), "\n";
/// ```
/// ```expect process
/// thrown
/// USR1
/// USR1, USR2, HUP
/// thrown
/// USR1, USR2, HUP
/// ```
///
/// No fiber may switch while the handlers play,
/// `zend_fiber_switch_block`, so a handler served inside a fiber cannot
/// suspend it:
///
/// ```php
/// pcntl_signal(SIGUSR1, function () {
///     if (Fiber::getCurrent()) {
///         try { Fiber::suspend(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
///     }
/// });
/// $f = new Fiber(function () { posix_kill(posix_getpid(), SIGUSR1); pcntl_signal_dispatch(); echo "end\n"; });
/// $f->start();
/// ```
/// ```expect process
/// Cannot switch fibers in current execution context
/// end
/// ```
pub(crate) fn dispatch(req: &mut Request) -> Result<(), Thrown> {
    let table = &mut req.state::<State>().signals;
    if table.dispatching || table.handlers.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i32> = table.handlers.keys().copied().collect();
    let mut queue = std::mem::take(&mut table.held);
    queue.extend(take_signals(|number| numbers.contains(&number)));
    if queue.is_empty() {
        return Ok(());
    }
    let mut unmasked = SigSet::empty();
    let masked = sigprocmask(
        SigmaskHow::SIG_BLOCK,
        Some(&SigSet::all()),
        Some(&mut unmasked),
    )
    .is_ok();
    req.state::<State>().signals.dispatching = true;
    // "Prevent switching fibers when handling signals".
    let played = req
        .code_mut()
        .with_fiber_switch_blocked(|req| play(req, &mut queue));
    let table = &mut req.state::<State>().signals;
    table.dispatching = false;
    // "Put back what the throwing handler did not get to, instead of
    // dropping it, and ask the engine to come back once the exception
    // has been handled."
    let come_back = !queue.is_empty() && matches!(table.delivery, Delivery::Asynchronous { .. });
    table.held = queue;
    if come_back {
        req.limits().interrupt().raise();
    }
    if masked {
        sigprocmask(SigmaskHow::SIG_SETMASK, Some(&unmasked), None).ok();
    }
    played
}

/// Each signal's handler called in turn, each taken off the queue before
/// its handler is called, until one throws; what is left stays queued.
fn play(req: &mut Request, queue: &mut VecDeque<Siginfo>) -> Result<(), Thrown> {
    while let Some(posted) = queue.pop_front() {
        let Some(handler) = req
            .state::<State>()
            .signals
            .handlers
            .get(&posted.number)
            .map(|handler| **handler)
        else {
            continue;
        };
        if handler.as_int().is_some() {
            continue;
        }
        // The handler may replace itself; the call holds its own owner.
        let held = Owner::take(req, handler);
        let called = call_handler(req, *held, &posted);
        held.give_up(req);
        called?;
    }
    Ok(())
}

/// `call_user_function(NULL, NULL, handle, &retval, 2, params)`, the
/// value it answers given up.
fn call_handler(req: &mut Request, handler: Value, posted: &Siginfo) -> Result<(), Thrown> {
    let info = siginfo(req, posted.number, posted)?;
    let called = to_callable(req, &handler).and_then(|callable| {
        call(
            req,
            &callable,
            &[Value::int(posted.number.into()), info.to_value()],
        )
    });
    info.give_up(req);
    called?.give_up(req);
    Ok(())
}

/// `pcntl_siginfo_to_zval(signo, siginfo, zv)` (`pcntl.c:1122`): `signo`,
/// `errno` and `code`, and the members the signal's number says the
/// kernel filled.
pub(crate) fn siginfo(
    req: &mut Request,
    number: i32,
    posted: &Siginfo,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut info = PhpArray::new(req)?;
    put(req, &mut info, b"signo", Owner::int(posted.number.into()))?;
    put(req, &mut info, b"errno", Owner::int(posted.errno.into()))?;
    put(req, &mut info, b"code", Owner::int(posted.code.into()))?;
    let Ok(signal) = Signal::try_from(number) else {
        realtime(req, &mut info, number, posted)?;
        return Ok(info);
    };
    match signal {
        Signal::SIGCHLD => {
            put(req, &mut info, b"status", Owner::int(posted.status.into()))?;
            #[cfg(target_os = "linux")]
            for (key, ticks) in [(&b"utime"[..], posted.utime), (b"stime", posted.stime)] {
                #[allow(clippy::cast_precision_loss)]
                let ticks = Value::float(ticks as f64);
                put(req, &mut info, key, Owner::take(req, ticks))?;
            }
            put(req, &mut info, b"pid", Owner::int(posted.pid.into()))?;
            put(req, &mut info, b"uid", Owner::int(posted.uid.into()))?;
        }
        Signal::SIGUSR1 | Signal::SIGUSR2 => {
            put(req, &mut info, b"pid", Owner::int(posted.pid.into()))?;
            put(req, &mut info, b"uid", Owner::int(posted.uid.into()))?;
        }
        Signal::SIGILL | Signal::SIGFPE | Signal::SIGSEGV | Signal::SIGBUS => {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
            let addr = posted.addr as i64 as f64;
            put(req, &mut info, b"addr", Owner::float(addr))?;
        }
        #[cfg(target_os = "linux")]
        Signal::SIGIO => {
            let band = Value::int(posted.band);
            put(req, &mut info, b"band", Owner::take(req, band))?;
            let fd = Value::int(posted.fd.into());
            put(req, &mut info, b"fd", Owner::take(req, fd))?;
        }
        _ => {}
    }
    Ok(info)
}

/// The members a real-time signal carries, the sender's `pid` and
/// `uid`, where the platform has real-time signals: php's
/// `SIGRTMIN <= signo && signo <= SIGRTMAX` after its switch.
#[cfg(target_os = "linux")]
fn realtime(
    req: &mut Request,
    info: &mut Owner<PhpArray>,
    number: i32,
    posted: &Siginfo,
) -> Result<(), Thrown> {
    if (libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&number) {
        put(req, info, b"pid", Owner::int(posted.pid.into()))?;
        put(req, info, b"uid", Owner::int(posted.uid.into()))?;
    }
    Ok(())
}

/// [`realtime`] where there are no real-time signals.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn realtime(_: &mut Request, _: &mut Owner<PhpArray>, _: i32, _: &Siginfo) -> Result<(), Thrown> {
    Ok(())
}

/// `add_assoc_long_ex` and its kin: one string key.
fn put(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    key: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, key, value)
}

/// `pcntl_sigprocmask(int $mode, array $signals, &$old_signals = null):
/// bool`: the calling thread's mask changed, and what it was, as the
/// numbers in it in order, written into `$old_signals`.
///
/// ```php
/// var_dump(pcntl_sigprocmask(SIG_BLOCK, [SIGURG], $old));
/// var_dump(pcntl_sigprocmask(SIG_SETMASK, $old, $was), in_array(SIGURG, $was), in_array(SIGURG, $old));
/// try { pcntl_sigprocmask(7, []); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_sigprocmask(SIG_BLOCK, []); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_sigprocmask(SIG_BLOCK, [0]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_sigprocmask(SIG_BLOCK, [[]]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// pcntl_sigprocmask(): Argument #1 ($mode) must be one of SIG_BLOCK, SIG_UNBLOCK, or SIG_SETMASK
/// pcntl_sigprocmask(): Argument #2 ($signals) must not be empty
/// pcntl_sigprocmask(): Argument #2 ($signals) signals must be between 1 and 31
/// pcntl_sigprocmask(): Argument #2 ($signals) signals must be of type int, array given
/// ```
#[php_function]
pub(crate) fn pcntl_sigprocmask(
    req: &mut Request,
    mode: i64,
    signals: PhpArray,
    #[php(default = null)] old_signals: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let Ok(how @ (libc::SIG_BLOCK | libc::SIG_UNBLOCK | libc::SIG_SETMASK)) = i32::try_from(mode)
    else {
        return Err(arg_value_error(
            req,
            1,
            "be one of SIG_BLOCK, SIG_UNBLOCK, or SIG_SETMASK",
        ));
    };
    let Some(set) = signal_set(req, signals, 2, how == libc::SIG_SETMASK)? else {
        return Ok(false);
    };
    let old = match ffi::mask(how, &set) {
        Ok(old) => old,
        Err(errno) => {
            failed(req, errno);
            docref(req, Level::Warning, errno.desc());
            return Ok(false);
        }
    };
    if old_signals.is_given() {
        let mut numbers = PhpArray::new(req)?;
        for number in 1..NUMBERS {
            let member = i32::try_from(number).is_ok_and(|n| old.contains(n));
            if member {
                numbers.append(req, Owner::int(number))?;
            }
        }
        old_signals.set(req, Untyped(numbers.into_value()))?;
    }
    Ok(true)
}

/// `pcntl_sigwaitinfo(array $signals, &$info = []): int|false`: the
/// calling thread waits for one of the signals, which it should have
/// blocked, and takes it without a handler; `$info` is what its
/// `siginfo_t` said.
#[cfg(target_os = "linux")]
#[php_function]
pub(crate) fn pcntl_sigwaitinfo(
    req: &mut Request,
    signals: PhpArray,
    #[php(default = [])] info: Ref<Untyped<Value>>,
) -> Result<OrFalse<i64>, Thrown> {
    waited(req, signals, &info, None)
}

/// `pcntl_sigtimedwait(array $signals, &$info = [], int $seconds = 0, int
/// $nanoseconds = 0): int|false`: [`pcntl_sigwaitinfo`] for at most the
/// time given, `false` with no warning where none came.
#[cfg(target_os = "linux")]
#[php_function]
pub(crate) fn pcntl_sigtimedwait(
    req: &mut Request,
    signals: PhpArray,
    #[php(default = [])] info: Ref<Untyped<Value>>,
    #[php(default = 0)] seconds: i64,
    #[php(default = 0)] nanoseconds: i64,
) -> Result<OrFalse<i64>, Thrown> {
    waited(req, signals, &info, Some((seconds, nanoseconds)))
}

/// The two waits: the set read and checked as php checks it, then the
/// timeout, then the wait, its signal number answered and its
/// `siginfo_t` written into `$info` where it was passed.
#[cfg(target_os = "linux")]
fn waited(
    req: &mut Request,
    signals: PhpArray,
    info: &Ref<Untyped<Value>>,
    timeout: Option<(i64, i64)>,
) -> Result<OrFalse<i64>, Thrown> {
    let Some(set) = signal_set(req, signals, 1, false)? else {
        return Ok(OrFalse::False);
    };
    if let Some((seconds, nanoseconds)) = timeout {
        if seconds < 0 {
            return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
        }
        if !(0..1_000_000_000).contains(&nanoseconds) {
            return Err(arg_value_error(req, 4, "be between 0 and 1e9"));
        }
        if seconds == 0 && nanoseconds == 0 {
            return Err(php_ext::raising::value_error(
                req,
                "pcntl_sigtimedwait(): At least one of argument #3 ($seconds) or argument #4 ($nanoseconds) must be greater than 0",
            ));
        }
    }
    // The wait lends the request ([`Limits::blocking`], #2401).
    let (number, posted) = match req.limits_mut().blocking(|| ffi::wait_info(&set, timeout)) {
        Ok(taken) => taken,
        Err(nix::errno::Errno::EAGAIN) if timeout.is_some() => return Ok(OrFalse::False),
        Err(errno) => {
            failed(req, errno);
            docref(req, Level::Warning, errno.desc());
            return Ok(OrFalse::False);
        }
    };
    if number > 0 && info.is_given() {
        let array = siginfo(req, number, &posted)?;
        info.set(req, Untyped(array.into_value()))?;
    }
    Ok(OrFalse::Value(number.into()))
}

/// `php_pcntl_set_user_signal_infos`: the array's values as a signal
/// set, each read as `zval_try_get_long` reads it and held to
/// `1..NUMBERS`, and added with `sigaddset(3)`; `None` after php's
/// warning where the C library refused a number.
fn signal_set(
    req: &mut Request,
    signals: PhpArray,
    position: u32,
    allow_empty: bool,
) -> Result<Option<SignalSet>, Thrown> {
    if !allow_empty && signals.is_empty() {
        return Err(arg_value_error(req, position, "not be empty"));
    }
    let mut set = SignalSet::empty();
    // Read first under the rows walk; what is refused after takes the
    // request mutably.
    let values: Vec<Value> = walk::rows(req, signals).map(|(_, value)| value).collect();
    let ce = Throwables::of(req)?;
    for value in values {
        let Some(number) = try_long(&value.deref()) else {
            let given = value_name(req, &value);
            return Err(argument_error(
                req,
                ce.type_error,
                position,
                &[
                    b"signals must be of type int, ".as_slice(),
                    &given,
                    b" given",
                ]
                .concat(),
            ));
        };
        let number = (1..NUMBERS)
            .contains(&number)
            .then(|| i32::try_from(number).ok())
            .flatten();
        let Some(number) = number else {
            return Err(argument_error(
                req,
                ce.value_error,
                position,
                &format!("signals must be between 1 and {}", NUMBERS - 1),
            ));
        };
        if let Err(errno) = set.add(number) {
            failed(req, errno);
            docref(req, Level::Warning, errno.desc());
            return Ok(None);
        }
    }
    Ok(Some(set))
}

/// `zval_try_get_long`: an integer, a float that fits, a bool or null,
/// or a numeric string; nothing for anything else.
fn try_long(value: &Value) -> Option<i64> {
    match value.as_ref() {
        ValueRef::Int(n) => Some(n),
        ValueRef::Null => Some(0),
        ValueRef::Bool(b) => Some(i64::from(b)),
        #[allow(clippy::cast_possible_truncation)]
        ValueRef::Float(f) if f.is_finite() => Some(f as i64),
        ValueRef::Str(s) => std::str::from_utf8(s.as_bytes())
            .ok()
            .and_then(|text| text.trim().parse().ok()),
        _ => None,
    }
}

/// `pcntl_async_signals(?bool $enable = null): bool`: whether signals
/// are delivered at the loop's next interrupt check as well as at
/// `pcntl_signal_dispatch()`, and, where `$enable` is given, the answer
/// before it was set. An asynchronous signal runs its handler at the
/// next check after a native call (`ZEND_VM_FCALL_INTERRUPT_CHECK`)
/// with no dispatch called.
///
/// The signal here is sent by a forked child, and its ordering is fixed
/// by construction: the witness calls no dispatch and waits, calling
/// `usleep`, until the handler has been called, so whether it is called
/// as `pcntl_fork` or `pcntl_waitpid` returns, or after a later `usleep`
/// because another thread of the process took the signal late, it is
/// called before "waited" is written.
///
/// ```php
/// var_dump(pcntl_async_signals(), pcntl_async_signals(true), pcntl_async_signals());
/// $handled = false;
/// pcntl_signal(SIGUSR2, function ($signo) { echo "handled\n"; $GLOBALS['handled'] = true; });
/// $child = pcntl_fork();
/// if ($child === 0) { posix_kill(posix_getppid(), SIGUSR2); exit(0); }
/// pcntl_waitpid($child, $status);
/// while (!$handled) { usleep(1000); }
/// echo "waited\n";
/// var_dump(pcntl_async_signals(false), pcntl_async_signals());
/// ```
/// ```expect process
/// bool(false)
/// bool(false)
/// bool(true)
/// handled
/// waited
/// bool(true)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn pcntl_async_signals(
    req: &mut Request,
    #[php(default = null)] enable: Option<bool>,
) -> bool {
    let was = matches!(
        req.state::<State>().signals.delivery,
        Delivery::Asynchronous { .. }
    );
    match enable {
        Some(true) if !was => {
            let receiving = req.limits().interrupt().receive_signals();
            req.state::<State>().signals.delivery = Delivery::Asynchronous {
                _receiving: receiving,
            };
        }
        Some(false) => req.state::<State>().signals.delivery = Delivery::Explicit,
        _ => {}
    }
    was
}

/// `PHP_RSHUTDOWN_FUNCTION(pcntl)`: every signal the table set a
/// disposition for back to `SIG_DFL`, the table's owners given up, the
/// signals still posted under its numbers dropped, and asynchronous
/// delivery off.
pub(crate) fn rshutdown(req: &mut Request) {
    let table = std::mem::take(&mut req.state::<State>().signals);
    let numbers: Vec<i32> = table.handlers.keys().copied().collect();
    for (number, handler) in table.handlers {
        if handler.as_int() != Some(0) {
            ffi::install(number, Disposition::Default, false, false).ok();
        }
        handler.give_up(req);
    }
    drop(take_signals(|number| numbers.contains(&number)));
}
