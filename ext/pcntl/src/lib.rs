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
//! thread alone, as php's does. The child goes on in its request's own
//! state, which is its copy: its values, frames, buffered bytes, the handler
//! table here. What it may not rely on is anything another thread of the
//! parent held: the execution timer, which is a thread and is gone (php's
//! is an interval timer, which a child does not inherit either), and a
//! mutex of the engine's shared tables another request took at the fork,
//! which stays taken. The `php` binary serves its one request on its one
//! thread, so neither arises there, which is why the extension is built
//! for it and no server host.
//!
//! **Its witnesses** are served by that binary for the same reason: every
//! fence here is marked `process`, and `xtask witness` runs it in
//! `php-witness`, the binary built with `witness-trace`, in a process of
//! its own, never in the engine linked into the harness, which carries no
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
mod process;
mod signals;

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
/// the constants, the request's end and the interrupt hook, php's
/// `zend_interrupt_function` as `PHP_MINIT(pcntl)` sets it.
///
/// ```php
/// var_dump(extension_loaded('pcntl'));
/// ```
/// ```expect process
/// bool(true)
/// ```
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default();
    #[cfg(target_vendor = "apple")]
    let builder = builder.class::<process::QosClass>();
    builder
        .constants(constants::declared())
        .functions(vec![
            process::pcntl_fork::declared(),
            process::pcntl_waitpid::declared(),
            process::pcntl_waitid::declared(),
            process::pcntl_wait::declared(),
            signals::pcntl_signal::declared(),
            signals::pcntl_signal_get_handler::declared(),
            signals::pcntl_signal_dispatch::declared(),
            signals::pcntl_sigprocmask::declared(),
            process::pcntl_wifexited::declared(),
            process::pcntl_wifstopped::declared(),
            process::pcntl_wifcontinued::declared(),
            process::pcntl_wifsignaled::declared(),
            process::pcntl_wexitstatus::declared(),
            process::pcntl_wtermsig::declared(),
            process::pcntl_wstopsig::declared(),
            process::pcntl_exec::declared(),
            process::pcntl_alarm::declared(),
            process::pcntl_get_last_error::declared(),
            process::pcntl_errno::declared(),
            process::pcntl_getpriority::declared(),
            process::pcntl_setpriority::declared(),
            process::pcntl_strerror::declared(),
            signals::pcntl_async_signals::declared(),
            #[cfg(target_vendor = "apple")]
            process::pcntl_getqos_class::declared(),
            #[cfg(target_vendor = "apple")]
            process::pcntl_setqos_class::declared(),
        ])
        .request_state::<State>()
        .rshutdown(signals::rshutdown)
        .interrupt(signals::dispatch)
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
    req.info_table(&[&["pcntl support", "enabled"]]);
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
