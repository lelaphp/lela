//! The `pcntl` extension: `php-src/ext/pcntl` as one crate under `ext/`
//! (ADR-0004), built for the `php` binary and no server host (design
//! §7.2, revision 35). One dependency beside the facade, the process
//! through `nix`, with [`ffi`] the home of what `nix` leaves unsafe or
//! does not wrap; its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/pcntl/tests`, through `xtask phpt --subset`.
//!
//! Two groups of natives, declared in the stub's order: the process
//! ([`process`]: `fork`, the waits and the status words, `exec`, `alarm`,
//! the priorities, the errors and Apple's `QoS` classes) and the signals
//! ([`signals`]: the handler table, the dispatch, the mask and
//! asynchronous delivery). A signal reaches PHP code through the engine
//! alone (design §5.8): the handler this crate installs, in [`ffi`],
//! decodes the kernel's `siginfo_t` into the engine's signal record,
//! which keeps the signal and raises the interrupt of a request that
//! turned asynchronous delivery on, and does nothing else; the
//! PHP callback runs in [`signals::dispatch`], which the loop's interrupt
//! check serves as php's `zend_interrupt_function`, and which
//! `pcntl_signal_dispatch()` serves where asked.
//!
//! **A forked child.** `pcntl_fork` copies the process with its calling
//! thread alone, as php's does, through the engine's one home for fork
//! ([`Limits::fork`]): the engine joins every thread of its own
//! first, the engine's timer among them, and forks only while this
//! request is the one inside the engine, so nothing a thread of the
//! engine's held is left held in the child. The child goes on in its request's own
//! state, which is its copy: its values, frames, buffered bytes, the
//! handler table here, and no timer, as a child inherits no interval
//! timer. A host that serves on threads has requests on other threads,
//! and there the fork is refused rather than made, so the extension is
//! built for the hosts whose serving process is single-threaded: the
//! `php` binary and php-fpm's children.
//!
//! **Its witnesses** are served by that binary for the same reason: every
//! fence here is marked `process`, and `xtask witness` runs it in the
//! `php` binary, in a process of its own, never in the engine linked into the harness, which carries no
//! pcntl (harness-design.md §8, revision 61).
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ffi;
#[cfg(all(test, target_os = "linux"))]
mod linux_values;
mod process;
mod signals;
mod throwables;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_const;
use php_ext::request::Request;

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. php's `pcntl_globals`, the queue aside, which is the
/// engine's signal record.
#[derive(Default)]
struct State {
    /// `PCNTL_G(php_signal_table)` and how signals are delivered.
    signals: signals::Table,
    /// `PCNTL_G(last_error)`.
    last_error: i32,
}

/// The extension value (design §7.1): declarations paired with entries,
/// the constants, the request's end, the interrupt hook, php's
/// `zend_interrupt_function` as `PHP_MINIT(pcntl)` sets it, and the hook
/// `ZEND_TICKS` serves, the `pcntl_signal_dispatch_tick_function` its
/// `MINIT` adds with `php_add_tick_function`, which serves the signals
/// taken since the last dispatch after each statement of a
/// `declare(ticks)` scope.
///
/// ```php
/// var_dump(extension_loaded('pcntl'));
/// ```
/// ```expect process
/// bool(true)
/// ```
///
/// ```php
/// declare(ticks=1);
/// pcntl_signal(SIGUSR1, function ($signo) { echo "handled\n"; });
/// echo "start\n";
/// posix_kill(posix_getpid(), SIGUSR1);
/// $i = 0;
/// echo "done\n";
/// ```
/// ```expect process
/// start
/// handled
/// done
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .class::<process::QosClass>()
        .constants(constants::DECLARED)
        .functions(&[
            process::pcntl_fork::DECLARED,
            process::pcntl_waitpid::DECLARED,
            process::pcntl_waitid::DECLARED,
            process::pcntl_wait::DECLARED,
            signals::pcntl_signal::DECLARED,
            signals::pcntl_signal_get_handler::DECLARED,
            signals::pcntl_signal_dispatch::DECLARED,
            signals::pcntl_sigprocmask::DECLARED,
            #[cfg(target_os = "linux")]
            signals::pcntl_sigwaitinfo::DECLARED,
            #[cfg(target_os = "linux")]
            signals::pcntl_sigtimedwait::DECLARED,
            process::pcntl_wifexited::DECLARED,
            process::pcntl_wifstopped::DECLARED,
            process::pcntl_wifcontinued::DECLARED,
            process::pcntl_wifsignaled::DECLARED,
            process::pcntl_wexitstatus::DECLARED,
            process::pcntl_wtermsig::DECLARED,
            process::pcntl_wstopsig::DECLARED,
            process::pcntl_exec::DECLARED,
            process::pcntl_alarm::DECLARED,
            process::pcntl_get_last_error::DECLARED,
            process::pcntl_errno::DECLARED,
            process::pcntl_getpriority::DECLARED,
            process::pcntl_setpriority::DECLARED,
            process::pcntl_strerror::DECLARED,
            signals::pcntl_async_signals::DECLARED,
            #[cfg(target_os = "linux")]
            process::pcntl_unshare::DECLARED,
            #[cfg(target_os = "linux")]
            process::pcntl_getcpuaffinity::DECLARED,
            #[cfg(target_os = "linux")]
            process::pcntl_setcpuaffinity::DECLARED,
            #[cfg(target_os = "linux")]
            process::pcntl_getcpu::DECLARED,
            #[cfg(target_vendor = "apple")]
            process::pcntl_getqos_class::DECLARED,
            #[cfg(target_vendor = "apple")]
            process::pcntl_setqos_class::DECLARED,
        ])
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .rshutdown(signals::rshutdown)
        .interrupt(signals::interrupt)
        .ticks(signals::dispatch)
        .info(pcntl_info)
        .build("pcntl")
}

/// `PHP_MINFO_FUNCTION(pcntl)`: the one-row table.
///
/// ```php
/// (new ReflectionExtension('pcntl'))->info();
/// ```
/// ```expect process
/// pcntl
///
/// pcntl support => enabled
/// ```
fn pcntl_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["pcntl support", "enabled"]]);
}

/// `PCNTL_G(last_error) = errno`.
fn failed(req: &mut Request, errno: nix::errno::Errno) {
    req.state::<State>().last_error = errno as i32;
}

/// `pcntl.stub.php`'s constants in its order, with the values of the
/// platform's headers; one behind an `#ifdef` the target lacks is behind
/// the `#[cfg]` that says so.
#[php_const]
mod constants {
    pub(crate) const WNOHANG: i64 = libc::WNOHANG as i64;
    pub(crate) const WUNTRACED: i64 = libc::WUNTRACED as i64;
    pub(crate) const WCONTINUED: i64 = libc::WCONTINUED as i64;
    pub(crate) const WEXITED: i64 = libc::WEXITED as i64;
    pub(crate) const WSTOPPED: i64 = libc::WSTOPPED as i64;
    pub(crate) const WNOWAIT: i64 = libc::WNOWAIT as i64;
    pub(crate) const P_ALL: i64 = libc::P_ALL as i64;
    pub(crate) const P_PID: i64 = libc::P_PID as i64;
    pub(crate) const P_PGID: i64 = libc::P_PGID as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const P_PIDFD: i64 = libc::P_PIDFD as i64;
    pub(crate) const SIG_IGN: i64 = 1;
    pub(crate) const SIG_DFL: i64 = 0;
    pub(crate) const SIG_ERR: i64 = -1;
    pub(crate) const SIGHUP: i64 = libc::SIGHUP as i64;
    pub(crate) const SIGINT: i64 = libc::SIGINT as i64;
    pub(crate) const SIGQUIT: i64 = libc::SIGQUIT as i64;
    pub(crate) const SIGILL: i64 = libc::SIGILL as i64;
    pub(crate) const SIGTRAP: i64 = libc::SIGTRAP as i64;
    pub(crate) const SIGABRT: i64 = libc::SIGABRT as i64;
    pub(crate) const SIGIOT: i64 = libc::SIGIOT as i64;
    pub(crate) const SIGBUS: i64 = libc::SIGBUS as i64;
    pub(crate) const SIGFPE: i64 = libc::SIGFPE as i64;
    pub(crate) const SIGKILL: i64 = libc::SIGKILL as i64;
    pub(crate) const SIGUSR1: i64 = libc::SIGUSR1 as i64;
    pub(crate) const SIGSEGV: i64 = libc::SIGSEGV as i64;
    pub(crate) const SIGUSR2: i64 = libc::SIGUSR2 as i64;
    pub(crate) const SIGPIPE: i64 = libc::SIGPIPE as i64;
    pub(crate) const SIGALRM: i64 = libc::SIGALRM as i64;
    pub(crate) const SIGTERM: i64 = libc::SIGTERM as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SIGSTKFLT: i64 = libc::SIGSTKFLT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SIGCLD: i64 = libc::SIGCHLD as i64;
    pub(crate) const SIGCHLD: i64 = libc::SIGCHLD as i64;
    pub(crate) const SIGCONT: i64 = libc::SIGCONT as i64;
    pub(crate) const SIGSTOP: i64 = libc::SIGSTOP as i64;
    pub(crate) const SIGTSTP: i64 = libc::SIGTSTP as i64;
    pub(crate) const SIGTTIN: i64 = libc::SIGTTIN as i64;
    pub(crate) const SIGTTOU: i64 = libc::SIGTTOU as i64;
    pub(crate) const SIGURG: i64 = libc::SIGURG as i64;
    pub(crate) const SIGXCPU: i64 = libc::SIGXCPU as i64;
    pub(crate) const SIGXFSZ: i64 = libc::SIGXFSZ as i64;
    pub(crate) const SIGVTALRM: i64 = libc::SIGVTALRM as i64;
    pub(crate) const SIGPROF: i64 = libc::SIGPROF as i64;
    pub(crate) const SIGWINCH: i64 = libc::SIGWINCH as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SIGPOLL: i64 = libc::SIGPOLL as i64;
    pub(crate) const SIGIO: i64 = libc::SIGIO as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SIGPWR: i64 = libc::SIGPWR as i64;
    #[cfg(target_vendor = "apple")]
    pub(crate) const SIGINFO: i64 = libc::SIGINFO as i64;
    pub(crate) const SIGSYS: i64 = libc::SIGSYS as i64;
    pub(crate) const SIGBABY: i64 = libc::SIGSYS as i64;
    /// glibc's `SIGRTMIN`, a call: the first real-time signal past the
    /// ones the C library keeps for itself.
    #[cfg(target_os = "linux")]
    pub(crate) fn SIGRTMIN() -> i64 {
        i64::from(libc::SIGRTMIN())
    }
    /// glibc's `SIGRTMAX`, a call: the first real-time signal past the
    /// ones the C library keeps for itself.
    #[cfg(target_os = "linux")]
    pub(crate) fn SIGRTMAX() -> i64 {
        i64::from(libc::SIGRTMAX())
    }
    pub(crate) const PRIO_PGRP: i64 = libc::PRIO_PGRP as i64;
    pub(crate) const PRIO_USER: i64 = libc::PRIO_USER as i64;
    pub(crate) const PRIO_PROCESS: i64 = libc::PRIO_PROCESS as i64;
    #[cfg(target_vendor = "apple")]
    pub(crate) const PRIO_DARWIN_BG: i64 = 0x1000;
    #[cfg(target_vendor = "apple")]
    pub(crate) const PRIO_DARWIN_THREAD: i64 = 3;
    pub(crate) const SIG_BLOCK: i64 = libc::SIG_BLOCK as i64;
    pub(crate) const SIG_UNBLOCK: i64 = libc::SIG_UNBLOCK as i64;
    pub(crate) const SIG_SETMASK: i64 = libc::SIG_SETMASK as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_USER: i64 = libc::SI_USER as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_KERNEL: i64 = libc::SI_KERNEL as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_QUEUE: i64 = libc::SI_QUEUE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_TIMER: i64 = libc::SI_TIMER as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_MESGQ: i64 = libc::SI_MESGQ as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_ASYNCIO: i64 = libc::SI_ASYNCIO as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_SIGIO: i64 = libc::SI_SIGIO as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SI_TKILL: i64 = libc::SI_TKILL as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLD_EXITED: i64 = libc::CLD_EXITED as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLD_KILLED: i64 = libc::CLD_KILLED as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLD_DUMPED: i64 = libc::CLD_DUMPED as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLD_TRAPPED: i64 = libc::CLD_TRAPPED as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLD_STOPPED: i64 = libc::CLD_STOPPED as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLD_CONTINUED: i64 = libc::CLD_CONTINUED as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TRAP_BRKPT: i64 = libc::TRAP_BRKPT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TRAP_TRACE: i64 = libc::TRAP_TRACE as i64;
    /// `<asm-generic/siginfo.h>`'s `si_code`s, which the `libc` crate does
    /// not carry, here to `SEGV_ACCERR`.
    #[cfg(target_os = "linux")]
    pub(crate) const POLL_IN: i64 = 1;
    #[cfg(target_os = "linux")]
    pub(crate) const POLL_OUT: i64 = 2;
    #[cfg(target_os = "linux")]
    pub(crate) const POLL_MSG: i64 = 3;
    #[cfg(target_os = "linux")]
    pub(crate) const POLL_ERR: i64 = 4;
    #[cfg(target_os = "linux")]
    pub(crate) const POLL_PRI: i64 = 5;
    #[cfg(target_os = "linux")]
    pub(crate) const POLL_HUP: i64 = 6;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_ILLOPC: i64 = 1;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_ILLOPN: i64 = 2;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_ILLADR: i64 = 3;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_ILLTRP: i64 = 4;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_PRVOPC: i64 = 5;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_PRVREG: i64 = 6;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_COPROC: i64 = 7;
    #[cfg(target_os = "linux")]
    pub(crate) const ILL_BADSTK: i64 = 8;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_INTDIV: i64 = 1;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_INTOVF: i64 = 2;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_FLTDIV: i64 = 3;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_FLTOVF: i64 = 4;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_FLTUND: i64 = 5;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_FLTRES: i64 = 6;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_FLTINV: i64 = 7;
    #[cfg(target_os = "linux")]
    pub(crate) const FPE_FLTSUB: i64 = 8;
    #[cfg(target_os = "linux")]
    pub(crate) const SEGV_MAPERR: i64 = 1;
    #[cfg(target_os = "linux")]
    pub(crate) const SEGV_ACCERR: i64 = 2;
    #[cfg(target_os = "linux")]
    pub(crate) const BUS_ADRALN: i64 = libc::BUS_ADRALN as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const BUS_ADRERR: i64 = libc::BUS_ADRERR as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const BUS_OBJERR: i64 = libc::BUS_OBJERR as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLONE_NEWNS: i64 = libc::CLONE_NEWNS as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLONE_NEWIPC: i64 = libc::CLONE_NEWIPC as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLONE_NEWUTS: i64 = libc::CLONE_NEWUTS as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLONE_NEWNET: i64 = libc::CLONE_NEWNET as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLONE_NEWPID: i64 = libc::CLONE_NEWPID as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLONE_NEWUSER: i64 = libc::CLONE_NEWUSER as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const CLONE_NEWCGROUP: i64 = libc::CLONE_NEWCGROUP as i64;
    pub(crate) const PCNTL_EINTR: i64 = libc::EINTR as i64;
    pub(crate) const PCNTL_ECHILD: i64 = libc::ECHILD as i64;
    pub(crate) const PCNTL_EINVAL: i64 = libc::EINVAL as i64;
    pub(crate) const PCNTL_EAGAIN: i64 = libc::EAGAIN as i64;
    pub(crate) const PCNTL_ESRCH: i64 = libc::ESRCH as i64;
    pub(crate) const PCNTL_EACCES: i64 = libc::EACCES as i64;
    pub(crate) const PCNTL_EPERM: i64 = libc::EPERM as i64;
    pub(crate) const PCNTL_ENOMEM: i64 = libc::ENOMEM as i64;
    pub(crate) const PCNTL_E2BIG: i64 = libc::E2BIG as i64;
    pub(crate) const PCNTL_EFAULT: i64 = libc::EFAULT as i64;
    pub(crate) const PCNTL_EIO: i64 = libc::EIO as i64;
    pub(crate) const PCNTL_EISDIR: i64 = libc::EISDIR as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const PCNTL_ELIBBAD: i64 = libc::ELIBBAD as i64;
    pub(crate) const PCNTL_ELOOP: i64 = libc::ELOOP as i64;
    pub(crate) const PCNTL_EMFILE: i64 = libc::EMFILE as i64;
    pub(crate) const PCNTL_ENAMETOOLONG: i64 = libc::ENAMETOOLONG as i64;
    pub(crate) const PCNTL_ENFILE: i64 = libc::ENFILE as i64;
    pub(crate) const PCNTL_ENOENT: i64 = libc::ENOENT as i64;
    pub(crate) const PCNTL_ENOEXEC: i64 = libc::ENOEXEC as i64;
    pub(crate) const PCNTL_ENOTDIR: i64 = libc::ENOTDIR as i64;
    pub(crate) const PCNTL_ETXTBSY: i64 = libc::ETXTBSY as i64;
    pub(crate) const PCNTL_ENOSPC: i64 = libc::ENOSPC as i64;
    pub(crate) const PCNTL_EUSERS: i64 = libc::EUSERS as i64;
}
