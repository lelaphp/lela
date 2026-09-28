//! The signal natives (`pcntl.c`'s `pcntl_signal` to
//! `pcntl_async_signals`) and the dispatch the engine's interrupt check
//! serves. What a signal handler does is `ffi`'s, posting to the
//! engine's signal record (design §5.8, revision 38); what is here is
//! php's handler table, the dispatch that calls into it, and the switch
//! that hands this request's interrupt to the record.

use std::collections::BTreeMap;

use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
use php_ext::abi::{Passed, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::coerce::{is_callable, to_callable, value_name};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, docref, fatal,
};
use php_ext::request::{Receiving, Request, Siginfo, take_signals};
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::ffi::{self, Disposition};
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
    /// each number, each value held by one owner of the table's.
    handlers: BTreeMap<i32, Value>,
    /// `PCNTL_G(async_signals)`.
    delivery: Delivery,
    /// `PCNTL_G(processing_signal_queue)`: a dispatch is running, so a
    /// handler's own safepoints do not start another.
    dispatching: bool,
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
                &format!("be of type callable|int, {given} given"),
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
    req.addref(&handler);
    if let Some(previous) = req
        .state::<State>()
        .signals
        .handlers
        .insert(number, handler)
    {
        req.release(previous);
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
    let held = req.state::<State>().signals.handlers.get(&number).copied();
    Ok(Untyped(match held {
        Some(handler) => {
            req.addref(&handler);
            handler
        }
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

/// `pcntl_signal_dispatch()` the function (`pcntl.c:1366`): every signal
/// posted under a number the table holds, taken off the record in the
/// order it arrived, and its handler called with the number and the
/// `siginfo_t` array where the table holds a callable; one the table
/// holds `SIG_DFL` or `SIG_IGN` for is dropped. Every signal is blocked
/// while the queue plays, as php blocks them, and a handler that throws
/// ends the dispatch with what is left dropped. The engine's interrupt
/// check serves it as php's `pcntl_interrupt_function`. No fiber may
/// switch while the handlers play, `zend_fiber_switch_block`, so a
/// handler served inside a fiber cannot suspend it:
///
/// ```php
/// pcntl_signal(SIGUSR1, function () { try { Fiber::suspend(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; } });
/// $f = new Fiber(function () { posix_kill(posix_getpid(), SIGUSR1); pcntl_signal_dispatch(); echo "fiber end\n"; });
/// $f->start();
/// ```
/// ```expect process
/// Cannot switch fibers in current execution context
/// fiber end
/// ```
pub(crate) fn dispatch(req: &mut Request) -> Result<(), Thrown> {
    let table = &req.state::<State>().signals;
    if table.dispatching || table.handlers.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i32> = table.handlers.keys().copied().collect();
    let taken = take_signals(|number| numbers.contains(&number));
    if taken.is_empty() {
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
    // No fiber switches while the handlers play (`zend_fiber_switch_block`).
    let played = req.with_fiber_switch_blocked(|req| play(req, &taken));
    req.state::<State>().signals.dispatching = false;
    if masked {
        let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&unmasked), None);
    }
    played
}

/// Each signal's handler called in turn, until one throws.
fn play(req: &mut Request, taken: &[Siginfo]) -> Result<(), Thrown> {
    for posted in taken {
        let Some(handler) = req
            .state::<State>()
            .signals
            .handlers
            .get(&posted.number)
            .copied()
        else {
            continue;
        };
        if handler.as_int().is_some() {
            continue;
        }
        // The handler may replace itself; the call holds its own owner.
        req.addref(&handler);
        let called = call_handler(req, handler, posted);
        req.release(handler);
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
    req.release(info.to_value());
    req.release(called?);
    Ok(())
}

/// `pcntl_siginfo_to_zval(signo, siginfo, zv)` (`pcntl.c:1122`): `signo`,
/// `errno` and `code`, and the members the signal's number says the
/// kernel filled.
pub(crate) fn siginfo(
    req: &mut Request,
    number: i32,
    posted: &Siginfo,
) -> Result<PhpArray, Thrown> {
    let mut info = PhpArray::new(req)?;
    put(req, &mut info, b"signo", Value::int(posted.number.into()))?;
    put(req, &mut info, b"errno", Value::int(posted.errno.into()))?;
    put(req, &mut info, b"code", Value::int(posted.code.into()))?;
    let Ok(signal) = Signal::try_from(number) else {
        return Ok(info);
    };
    match signal {
        Signal::SIGCHLD => {
            put(req, &mut info, b"status", Value::int(posted.status.into()))?;
            put(req, &mut info, b"pid", Value::int(posted.pid.into()))?;
            put(req, &mut info, b"uid", Value::int(posted.uid.into()))?;
        }
        Signal::SIGUSR1 | Signal::SIGUSR2 => {
            put(req, &mut info, b"pid", Value::int(posted.pid.into()))?;
            put(req, &mut info, b"uid", Value::int(posted.uid.into()))?;
        }
        Signal::SIGILL | Signal::SIGFPE | Signal::SIGSEGV | Signal::SIGBUS => {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
            let addr = posted.addr as i64 as f64;
            put(req, &mut info, b"addr", Value::float(addr))?;
        }
        _ => {}
    }
    Ok(info)
}

/// `add_assoc_long_ex` and its kin: one string key.
fn put(req: &mut Request, array: &mut PhpArray, key: &[u8], value: Value) -> Result<(), Thrown> {
    let key = PhpString::new(req, key)?;
    array.set(req, Key::Str(key), value)
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
    let how = match i32::try_from(mode) {
        Ok(libc::SIG_BLOCK) => SigmaskHow::SIG_BLOCK,
        Ok(libc::SIG_UNBLOCK) => SigmaskHow::SIG_UNBLOCK,
        Ok(libc::SIG_SETMASK) => SigmaskHow::SIG_SETMASK,
        _ => {
            return Err(arg_value_error(
                req,
                1,
                "be one of SIG_BLOCK, SIG_UNBLOCK, or SIG_SETMASK",
            ));
        }
    };
    let set = signal_set(req, signals, 2, how == SigmaskHow::SIG_SETMASK)?;
    let mut old = SigSet::empty();
    if let Err(errno) = sigprocmask(how, Some(&set), Some(&mut old)) {
        failed(req, errno);
        docref(req, Level::Warning, errno.desc());
        return Ok(false);
    }
    if old_signals.is_given() {
        let mut numbers = PhpArray::new(req)?;
        for number in 1..NUMBERS {
            let member = i32::try_from(number)
                .ok()
                .and_then(|n| Signal::try_from(n).ok())
                .is_some_and(|signal| old.contains(signal));
            if member {
                numbers.append(req, Value::int(number))?;
            }
        }
        old_signals.set(req, Untyped(numbers.to_value()))?;
    }
    Ok(true)
}

/// `php_pcntl_set_user_signal_infos`: the array's values as a signal
/// set, each read as `zval_try_get_long` reads it and held to
/// `1..NUMBERS`.
fn signal_set(
    req: &mut Request,
    signals: PhpArray,
    position: u32,
    allow_empty: bool,
) -> Result<SigSet, Thrown> {
    if !allow_empty && signals.is_empty() {
        return Err(arg_value_error(req, position, "not be empty"));
    }
    let mut set = SigSet::empty();
    for (_, value) in &signals {
        let Some(number) = try_long(&value.deref()) else {
            let given = value_name(req, &value);
            return Err(argument_error(
                req,
                "TypeError",
                position,
                &format!("signals must be of type int, {given} given"),
            ));
        };
        let signal = (1..NUMBERS)
            .contains(&number)
            .then(|| i32::try_from(number).ok())
            .flatten()
            .and_then(|n| Signal::try_from(n).ok());
        let Some(signal) = signal else {
            return Err(argument_error(
                req,
                "ValueError",
                position,
                &format!("signals must be between 1 and {}", NUMBERS - 1),
            ));
        };
        set.add(signal);
    }
    Ok(set)
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
            let receiving = req.interrupt().receive_signals();
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
            let _ = ffi::install(number, Disposition::Default, false, false);
        }
        req.release(handler);
    }
    let _ = take_signals(|number| numbers.contains(&number));
}
