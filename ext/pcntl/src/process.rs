//! The process natives: `fork`, the waits and the status words, `exec`,
//! `alarm`, the priorities, the errors and Apple's `QoS` classes, each
//! `PHP_FUNCTION(pcntl_*)` of `pcntl.c` at the tag.
//!
//! A witness here runs in the harness's own process beside others, so
//! one that waits names the child it forked and never `-1`, and what
//! changes the process as a whole (an alarm that fires, an `exec` that
//! succeeds) happens in a forked child whose status the parent reads.

use std::ffi::CString;

use nix::errno::Errno;
use nix::unistd::ForkResult;
use php_ext::abi::{False, OrFalse, Passed, Path, Ref, Untyped};
#[cfg(target_vendor = "apple")]
use php_ext::attr::php_class;
use php_ext::attr::php_function;
use php_ext::coerce::{to_long, to_string};
#[cfg(target_vendor = "apple")]
use php_ext::raising::throw;
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, arity, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::{State, failed, ffi};

/// `pcntl_fork(): int`: the child's id in the parent, `0` in the child,
/// and `-1` with php's warning where `fork(2)` refused. The child ends
/// with the status its request ends with, which the parent reads.
///
/// ```php
/// $pid = pcntl_fork();
/// if ($pid === 0) { exit(3); }
/// var_dump($pid > 0, pcntl_waitpid($pid, $status) === $pid, pcntl_wexitstatus($status));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(3)
/// ```
#[php_function]
pub(crate) fn pcntl_fork(req: &mut Request) -> i64 {
    match ffi::fork() {
        Ok(ForkResult::Parent { child }) => child.as_raw().into(),
        Ok(ForkResult::Child) => 0,
        Err(errno) => {
            failed(req, errno);
            let n = errno as i32;
            let message = match errno {
                Errno::EAGAIN => {
                    format!("Error {n}: Reached the maximum limit of number of processes")
                }
                Errno::ENOMEM => format!("Error {n}: Insufficient memory"),
                Errno::ENOSYS => format!("Error {n}: Unimplemented"),
                Errno::EBADF => format!("Error {n}: File descriptor concurrency issue"),
                _ => format!("Error {n}"),
            };
            docref(req, Level::Warning, &message);
            -1
        }
    }
}

/// `pcntl_waitpid(int $process_id, &$status, int $flags = 0,
/// &$resource_usage = []): int`: `wait4(2)` on the id, the status word
/// written back as the kernel wrote it, and, where `$resource_usage` is
/// given, the child's usage as php's `PHP_RUSAGE_TO_ARRAY` lays it out.
/// A process that is not a child is `-1` and `ECHILD`.
///
/// ```php
/// $pid = pcntl_fork();
/// if ($pid === 0) { exit(5); }
/// var_dump(pcntl_waitpid($pid, $status, 0, $usage) === $pid, pcntl_wexitstatus($status));
/// echo implode(',', array_keys($usage)), "\n";
/// var_dump(pcntl_waitpid(1, $status), pcntl_get_last_error() === PCNTL_ECHILD);
/// ```
/// ```expect
/// bool(true)
/// int(5)
/// ru_oublock,ru_inblock,ru_msgsnd,ru_msgrcv,ru_maxrss,ru_ixrss,ru_idrss,ru_minflt,ru_majflt,ru_nsignals,ru_nvcsw,ru_nivcsw,ru_nswap,ru_utime.tv_usec,ru_utime.tv_sec,ru_stime.tv_usec,ru_stime.tv_sec
/// int(-1)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn pcntl_waitpid(
    req: &mut Request,
    process_id: i64,
    status: Ref<Untyped<Value>>,
    #[php(default = 0)] flags: i64,
    #[php(default = [])] resource_usage: Ref<Untyped<Value>>,
) -> Result<i64, Thrown> {
    let pid = i32::try_from(process_id).unwrap_or(i32::MAX);
    waited(req, pid, &status, flags, &resource_usage)
}

/// `pcntl_wait(&$status, int $flags = 0, &$resource_usage = []): int`:
/// `wait3(2)`, which is `wait4(2)` on any child. Here in a child the
/// witness forked, so the one child it waits for is its own; the parent
/// only calls it wrongly, since any child of the harness's is not the
/// witness's to reap.
///
/// ```php
/// $pid = pcntl_fork();
/// if ($pid === 0) {
///     $grandchild = pcntl_fork();
///     if ($grandchild === 0) { exit(4); }
///     exit(pcntl_wait($status) === $grandchild ? 10 + pcntl_wexitstatus($status) : 1);
/// }
/// pcntl_waitpid($pid, $status);
/// var_dump(pcntl_wexitstatus($status));
/// try { pcntl_wait(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(14)
/// pcntl_wait() expects at least 1 argument, 0 given
/// ```
#[php_function]
pub(crate) fn pcntl_wait(
    req: &mut Request,
    status: Ref<Untyped<Value>>,
    #[php(default = 0)] flags: i64,
    #[php(default = [])] resource_usage: Ref<Untyped<Value>>,
) -> Result<i64, Thrown> {
    waited(req, -1, &status, flags, &resource_usage)
}

/// The body `pcntl_waitpid` and `pcntl_wait` share.
fn waited(
    req: &mut Request,
    pid: i32,
    status: &Ref<Untyped<Value>>,
    flags: i64,
    resource_usage: &Ref<Untyped<Value>>,
) -> Result<i64, Thrown> {
    let given = status.get().map_or(Value::null(), |held| held.0);
    let mut word = i32::try_from(to_long(req, &given)?).unwrap_or_default();
    let options = i32::try_from(flags).unwrap_or_default();
    let mut usage = resource_usage.is_given().then(zeroed_usage);
    let child = match ffi::wait(pid, &mut word, options, usage.as_mut()) {
        Ok(child) => child,
        Err(errno) => {
            failed(req, errno);
            -1
        }
    };
    if let Some(usage) = usage {
        let mut array = PhpArray::new(req)?;
        if child > 0 {
            usage_into(req, &mut array, &usage)?;
        }
        resource_usage.set(req, Untyped(array.to_value()))?;
    }
    status.set(req, Untyped(Value::int(word.into())))?;
    Ok(child.into())
}

/// An `rusage` of zeroes, php's `memset`.
fn zeroed_usage() -> ffi::Usage {
    ffi::Usage {
        ru_utime: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_stime: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_maxrss: 0,
        ru_ixrss: 0,
        ru_idrss: 0,
        ru_isrss: 0,
        ru_minflt: 0,
        ru_majflt: 0,
        ru_nswap: 0,
        ru_inblock: 0,
        ru_oublock: 0,
        ru_msgsnd: 0,
        ru_msgrcv: 0,
        ru_nsignals: 0,
        ru_nvcsw: 0,
        ru_nivcsw: 0,
    }
}

/// One `rusage` member as php's `zend_long`, whatever C type the
/// platform gives it.
fn long(member: impl Into<i64>) -> i64 {
    member.into()
}

/// `PHP_RUSAGE_TO_ARRAY`: the special members, then the two times.
fn usage_into(req: &mut Request, array: &mut PhpArray, usage: &ffi::Usage) -> Result<(), Thrown> {
    let fields: [(&[u8], i64); 17] = [
        (b"ru_oublock", long(usage.ru_oublock)),
        (b"ru_inblock", long(usage.ru_inblock)),
        (b"ru_msgsnd", long(usage.ru_msgsnd)),
        (b"ru_msgrcv", long(usage.ru_msgrcv)),
        (b"ru_maxrss", long(usage.ru_maxrss)),
        (b"ru_ixrss", long(usage.ru_ixrss)),
        (b"ru_idrss", long(usage.ru_idrss)),
        (b"ru_minflt", long(usage.ru_minflt)),
        (b"ru_majflt", long(usage.ru_majflt)),
        (b"ru_nsignals", long(usage.ru_nsignals)),
        (b"ru_nvcsw", long(usage.ru_nvcsw)),
        (b"ru_nivcsw", long(usage.ru_nivcsw)),
        (b"ru_nswap", long(usage.ru_nswap)),
        (b"ru_utime.tv_usec", long(usage.ru_utime.tv_usec)),
        (b"ru_utime.tv_sec", long(usage.ru_utime.tv_sec)),
        (b"ru_stime.tv_usec", long(usage.ru_stime.tv_usec)),
        (b"ru_stime.tv_sec", long(usage.ru_stime.tv_sec)),
    ];
    for (name, value) in fields {
        let key = PhpString::new(req, name)?;
        array.set(req, Key::Str(key), Value::int(value))?;
    }
    Ok(())
}

/// `pcntl_waitid(int $idtype = P_ALL, ?int $id = null, &$info = [], int
/// $flags = WEXITED, &$resource_usage = []): bool`: `waitid(2)`, and
/// what it wrote into `siginfo_t` rendered as a `SIGCHLD`'s. Where the
/// platform has neither `wait6` nor Linux's raw `waitid`, which Apple's
/// has not, `$resource_usage` is left as it was, as php leaves it.
///
/// ```php
/// $pid = pcntl_fork();
/// if ($pid === 0) { exit(6); }
/// var_dump(pcntl_waitid(P_PID, $pid, $info, WEXITED));
/// var_dump($info['signo'] === SIGCHLD, $info['status'], $info['pid'] === $pid, array_keys($info));
/// var_dump(pcntl_waitid(P_PID, 1, $info, WEXITED), pcntl_get_last_error() === PCNTL_ECHILD);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(6)
/// bool(true)
/// array(6) {
///   [0]=>
///   string(5) "signo"
///   [1]=>
///   string(5) "errno"
///   [2]=>
///   string(4) "code"
///   [3]=>
///   string(6) "status"
///   [4]=>
///   string(3) "pid"
///   [5]=>
///   string(3) "uid"
/// }
/// bool(false)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn pcntl_waitid(
    req: &mut Request,
    #[php(default = P_ALL)] idtype: i64,
    #[php(default = null)] id: Option<i64>,
    #[php(default = [])] info: Ref<Untyped<Value>>,
    #[php(default = WEXITED)] flags: i64,
    #[php(default = [])] resource_usage: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let _ = resource_usage;
    let options = i32::try_from(flags).unwrap_or_default();
    let posted = match ffi::waitid(idtype, id.unwrap_or(0), options) {
        Ok(posted) => posted,
        Err(errno) => {
            failed(req, errno);
            return Ok(false);
        }
    };
    if info.is_given() {
        let array = crate::signals::siginfo(req, libc::SIGCHLD, &posted)?;
        info.set(req, Untyped(array.to_value()))?;
    }
    Ok(true)
}

/// Read a status word as the platform's `int`, which is how php hands
/// it to the `W*` macros.
fn word(status: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let word = status as i32;
    word
}

/// `pcntl_wifexited(int $status): bool`, `WIFEXITED`.
///
/// ```php
/// var_dump(pcntl_wifexited(0x300), pcntl_wifexited(9), pcntl_wifexited(0x137f));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn pcntl_wifexited(status: i64) -> bool {
    libc::WIFEXITED(word(status))
}

/// `pcntl_wifstopped(int $status): bool`, `WIFSTOPPED`.
///
/// ```php
/// var_dump(pcntl_wifstopped(0x117f), pcntl_wifstopped(0x300));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn pcntl_wifstopped(status: i64) -> bool {
    libc::WIFSTOPPED(word(status))
}

/// `pcntl_wifcontinued(int $status): bool`, `WIFCONTINUED`: a stopped
/// word whose signal is `SIGCONT` on Apple's, `0xffff` on Linux.
///
/// ```php
/// var_dump(pcntl_wifcontinued(0x137f), pcntl_wifcontinued(0x117f), pcntl_wifcontinued(0));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn pcntl_wifcontinued(status: i64) -> bool {
    libc::WIFCONTINUED(word(status))
}

/// `pcntl_wifsignaled(int $status): bool`, `WIFSIGNALED`.
///
/// ```php
/// var_dump(pcntl_wifsignaled(9), pcntl_wifsignaled(0x300), pcntl_wifsignaled(0x137f));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn pcntl_wifsignaled(status: i64) -> bool {
    libc::WIFSIGNALED(word(status))
}

/// `pcntl_wexitstatus(int $status): int|false`, `WEXITSTATUS`.
///
/// ```php
/// var_dump(pcntl_wexitstatus(0x300), pcntl_wexitstatus(0xff00), pcntl_wexitstatus(9));
/// ```
/// ```expect
/// int(3)
/// int(255)
/// int(0)
/// ```
#[php_function]
pub(crate) fn pcntl_wexitstatus(status: i64) -> OrFalse<i64> {
    OrFalse::Value(libc::WEXITSTATUS(word(status)).into())
}

/// `pcntl_wtermsig(int $status): int|false`, `WTERMSIG`.
///
/// ```php
/// var_dump(pcntl_wtermsig(9), pcntl_wtermsig(0x8f), pcntl_wtermsig(0x300));
/// ```
/// ```expect
/// int(9)
/// int(15)
/// int(0)
/// ```
#[php_function]
pub(crate) fn pcntl_wtermsig(status: i64) -> OrFalse<i64> {
    OrFalse::Value(libc::WTERMSIG(word(status)).into())
}

/// `pcntl_wstopsig(int $status): int|false`, `WSTOPSIG`.
///
/// ```php
/// var_dump(pcntl_wstopsig(0x117f), pcntl_wstopsig(0x137f));
/// ```
/// ```expect
/// int(17)
/// int(19)
/// ```
#[php_function]
pub(crate) fn pcntl_wstopsig(status: i64) -> OrFalse<i64> {
    OrFalse::Value(libc::WSTOPSIG(word(status)).into())
}

/// `pcntl_exec(string $path, array $args = [], array $env_vars = []):
/// false`: `execv(2)`, or `execve(2)` where `$env_vars` is given, with
/// the path as `argv[0]`; returning at all is the failure, with php's
/// warning. The program here runs in a forked child, whose status the
/// parent reads.
///
/// ```php
/// $pid = pcntl_fork();
/// if ($pid === 0) { pcntl_exec('/bin/sh', ['-c', 'exit $CODE'], ['CODE' => 7]); exit(1); }
/// pcntl_waitpid($pid, $status);
/// var_dump(pcntl_wexitstatus($status));
/// var_dump(pcntl_exec('/nonexistent/program'), pcntl_get_last_error() === PCNTL_ENOENT);
/// try { pcntl_exec('/bin/sh', ["a\0b"]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_exec('/bin/sh', [], ["A\0" => 'b']); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_exec('/bin/sh', [], ['A' => "b\0"]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(7)
///
/// Warning: pcntl_exec(): Error has occurred: (errno 2) No such file or directory in %s on line 6
/// bool(false)
/// bool(true)
/// pcntl_exec(): Argument #2 ($args) individual argument must not contain null bytes
/// pcntl_exec(): Argument #3 ($env_vars) name for environment variable must not contain null bytes
/// pcntl_exec(): Argument #3 ($env_vars) value for environment variable must not contain null bytes
/// ```
#[php_function]
pub(crate) fn pcntl_exec(
    req: &mut Request,
    path: Path,
    #[php(default = [])] args: PhpArray,
    #[php(default = [])] env_vars: Passed<PhpArray>,
) -> Result<False, Thrown> {
    let program = CString::new(path.0.as_bytes()).unwrap_or_default();
    let mut arguments = vec![program.clone()];
    for (_, element) in &args {
        let string = to_string(req, &element)?;
        let bytes = string.as_bytes().to_vec();
        req.release(Value::string(string));
        let Ok(argument) = CString::new(bytes) else {
            return Err(argument_error(
                req,
                "ValueError",
                2,
                "individual argument must not contain null bytes",
            ));
        };
        arguments.push(argument);
    }
    let executed = match env_vars {
        Passed::Given(env_vars) => {
            let mut envp = Vec::new();
            for (key, element) in &env_vars {
                let string = to_string(req, &element)?;
                let value = string.as_bytes().to_vec();
                req.release(Value::string(string));
                if value.contains(&0) {
                    return Err(argument_error(
                        req,
                        "ValueError",
                        3,
                        "value for environment variable must not contain null bytes",
                    ));
                }
                let mut pair = match key {
                    Key::Int(n) => n.to_string().into_bytes(),
                    Key::Str(name) => name.as_bytes().to_vec(),
                };
                if pair.contains(&0) {
                    return Err(argument_error(
                        req,
                        "ValueError",
                        3,
                        "name for environment variable must not contain null bytes",
                    ));
                }
                pair.push(b'=');
                pair.extend_from_slice(&value);
                envp.push(CString::new(pair).unwrap_or_default());
            }
            nix::unistd::execve(&program, &arguments, &envp)
        }
        Passed::Omitted => nix::unistd::execv(&program, &arguments),
    };
    let Err(errno) = executed;
    failed(req, errno);
    let n = errno as i32;
    docref(
        req,
        Level::Warning,
        &format!("Error has occurred: (errno {n}) {}", strerror(n)),
    );
    Ok(False)
}

/// `pcntl_alarm(int $seconds): int`: `alarm(2)` with the seconds read as
/// its `unsigned int`, answering the seconds left of the one it
/// replaced; zero cancels. The alarm here fires in a forked child, which
/// `SIGALRM` ends.
///
/// ```php
/// var_dump(pcntl_alarm(0));
/// $pid = pcntl_fork();
/// if ($pid === 0) { pcntl_signal(SIGALRM, SIG_DFL); pcntl_alarm(1); sleep(5); exit(0); }
/// pcntl_waitpid($pid, $status);
/// var_dump(pcntl_wifsignaled($status), pcntl_wtermsig($status) === SIGALRM);
/// ```
/// ```expect wall=1s
/// int(0)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn pcntl_alarm(seconds: i64) -> i64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let seconds = seconds as u32;
    let left = if seconds == 0 {
        nix::unistd::alarm::cancel()
    } else {
        nix::unistd::alarm::set(seconds)
    };
    left.unwrap_or(0).into()
}

/// `pcntl_get_last_error(): int`, and its alias `pcntl_errno(): int`:
/// the `errno` of the last pcntl function that failed in the request.
///
/// ```php
/// var_dump(pcntl_get_last_error(), pcntl_errno());
/// pcntl_waitpid(1, $status);
/// var_dump(pcntl_get_last_error() === PCNTL_ECHILD, pcntl_errno() === PCNTL_ECHILD);
/// ```
/// ```expect
/// int(0)
/// int(0)
/// bool(true)
/// bool(true)
/// ```
#[php_function(aliases(pcntl_errno))]
pub(crate) fn pcntl_get_last_error(req: &mut Request) -> i64 {
    req.state::<State>().last_error.into()
}

/// `pcntl_getpriority(?int $process_id = null, int $mode = PRIO_PROCESS):
/// int|false`: `getpriority(2)`, with php's warning for a process that
/// is not there and its `ValueError`s for Apple's `EINVAL`.
///
/// ```php
/// var_dump(is_int(pcntl_getpriority()), pcntl_getpriority() === pcntl_getpriority(getmypid()));
/// var_dump(pcntl_getpriority(99999999));
/// try { pcntl_getpriority(null, 42); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { pcntl_getpriority(1, PRIO_DARWIN_THREAD); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// Warning: pcntl_getpriority(): Error 3: No process was located using the given parameters in %s on line 3
/// bool(false)
/// pcntl_getpriority(): Argument #2 ($mode) must be one of PRIO_PGRP, PRIO_USER, PRIO_PROCESS or PRIO_DARWIN_THREAD
/// pcntl_getpriority(): Argument #1 ($process_id) must be 0 (zero) if PRIO_DARWIN_THREAD is provided as second parameter
/// ```
#[php_function]
pub(crate) fn pcntl_getpriority(
    req: &mut Request,
    #[php(default = null)] process_id: Option<i64>,
    #[php(default = PRIO_PROCESS)] mode: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let pid = process_id.unwrap_or(0);
    match ffi::priority(mode, pid) {
        Ok(priority) => Ok(OrFalse::Value(priority.into())),
        Err(errno) => {
            failed(req, errno);
            let n = errno as i32;
            match errno {
                Errno::ESRCH => docref(
                    req,
                    Level::Warning,
                    &format!("Error {n}: No process was located using the given parameters"),
                ),
                Errno::EINVAL => return Err(priority_refused(req, mode, pid, None, 2, 1)),
                _ => docref(
                    req,
                    Level::Warning,
                    &format!("Unknown error {n} has occurred"),
                ),
            }
            Ok(OrFalse::False)
        }
    }
}

/// `pcntl_setpriority(int $priority, ?int $process_id = null, int $mode =
/// PRIO_PROCESS): bool`: `setpriority(2)`, with php's warnings and its
/// `ValueError`s for Apple's `EINVAL`. Setting the priority the process
/// has is the one change a witness beside others may make.
///
/// ```php
/// var_dump(pcntl_setpriority(pcntl_getpriority()));
/// var_dump(pcntl_setpriority(0, 99999999));
/// try { pcntl_setpriority(0, null, 42); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
///
/// Warning: pcntl_setpriority(): Error 3: No process was located using the given parameters in %s on line 3
/// bool(false)
/// pcntl_setpriority(): Argument #3 ($mode) must be one of PRIO_PGRP, PRIO_USER, PRIO_PROCESS or PRIO_DARWIN_THREAD
/// ```
#[php_function]
pub(crate) fn pcntl_setpriority(
    req: &mut Request,
    priority: i64,
    #[php(default = null)] process_id: Option<i64>,
    #[php(default = PRIO_PROCESS)] mode: i64,
) -> Result<bool, Thrown> {
    let pid = process_id.unwrap_or(0);
    let Err(errno) = ffi::set_priority(mode, pid, priority) else {
        return Ok(true);
    };
    failed(req, errno);
    let n = errno as i32;
    let message = match errno {
        Errno::ESRCH => format!("Error {n}: No process was located using the given parameters"),
        Errno::EINVAL => return Err(priority_refused(req, mode, pid, Some(priority), 3, 2)),
        Errno::EPERM => format!(
            "Error {n}: A process was located, but neither its effective nor real user ID matched the effective user ID of the caller"
        ),
        Errno::EACCES => {
            format!("Error {n}: Only a super user may attempt to increase the process priority")
        }
        _ => format!("Unknown error {n} has occurred"),
    };
    docref(req, Level::Warning, &message);
    Ok(false)
}

/// The `ValueError` php raises for `EINVAL` from the two priority calls:
/// the `which`, then Apple's thread value's own two rules, then the id;
/// `mode_at` and `pid_at` are the two parameters' positions, and
/// `priority` is `setpriority`'s alone.
fn priority_refused(
    req: &mut Request,
    mode: i64,
    pid: i64,
    priority: Option<i64>,
    mode_at: u32,
    pid_at: u32,
) -> Thrown {
    #[cfg(target_vendor = "apple")]
    {
        let thread = i64::from(3_i32);
        let known = [
            i64::from(libc::PRIO_PGRP),
            i64::from(libc::PRIO_USER),
            i64::from(libc::PRIO_PROCESS),
            thread,
        ];
        if !known.contains(&mode) {
            return arg_value_error(
                req,
                mode_at,
                "be one of PRIO_PGRP, PRIO_USER, PRIO_PROCESS or PRIO_DARWIN_THREAD",
            );
        }
        if mode == thread && pid != 0 {
            return arg_value_error(
                req,
                pid_at,
                "be 0 (zero) if PRIO_DARWIN_THREAD is provided as second parameter",
            );
        }
        if let Some(priority) = priority
            && mode == thread
            && priority != 0
            && priority != 0x1000
        {
            return arg_value_error(
                req,
                1,
                "be either 0 (zero) or PRIO_DARWIN_BG, for mode PRIO_DARWIN_THREAD",
            );
        }
        argument_error(
            req,
            "ValueError",
            pid_at,
            "is not a valid process, process group, or user ID",
        )
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = (mode, pid, priority, pid_at);
        arg_value_error(
            req,
            mode_at,
            "be one of PRIO_PGRP, PRIO_USER, or PRIO_PROCESS",
        )
    }
}

/// `pcntl_strerror(int $error_code): string`: `strerror(3)`'s sentence.
///
/// ```php
/// var_dump(pcntl_strerror(PCNTL_ECHILD), pcntl_strerror(0), pcntl_strerror(12345));
/// ```
/// ```expect
/// string(18) "No child processes"
/// string(18) "Undefined error: 0"
/// string(20) "Unknown error: 12345"
/// ```
#[php_function]
pub(crate) fn pcntl_strerror(req: &mut Request, error_code: i64) -> Result<PhpString, Thrown> {
    #[allow(clippy::cast_possible_truncation)]
    let sentence = strerror(error_code as i32);
    PhpString::new(req, sentence.as_bytes())
}

/// `strerror(3)`, as the standard library reads it through
/// `strerror_r(3)`, less the `" (os error N)"` it appends.
fn strerror(errno: i32) -> String {
    let shown = std::io::Error::from_raw_os_error(errno).to_string();
    let suffix = format!(" (os error {errno})");
    shown
        .strip_suffix(&suffix)
        .map_or_else(|| shown.clone(), str::to_owned)
}

/// `enum Pcntl\QosClass`, Apple's `QoS` classes, a thread's scheduling class.
///
/// ```php
/// var_dump(Pcntl\QosClass::cases()[0], count(Pcntl\QosClass::cases()));
/// ```
/// ```expect
/// enum(Pcntl\QosClass::UserInteractive)
/// int(5)
/// ```
#[cfg(target_vendor = "apple")]
#[php_class(namespace(Pcntl))]
pub(crate) enum QosClass {
    UserInteractive,
    UserInitiated,
    Default,
    Utility,
    Background,
}

#[cfg(target_vendor = "apple")]
#[php_class]
impl QosClass {}

/// `pcntl_getqos_class(): Pcntl\QosClass`: the calling thread's class,
/// `Default` for one the enum does not name.
///
/// ```php
/// var_dump(pcntl_getqos_class() instanceof Pcntl\QosClass);
/// ```
/// ```expect
/// bool(true)
/// ```
#[cfg(target_vendor = "apple")]
#[php_function]
pub(crate) fn pcntl_getqos_class(req: &mut Request) -> Result<QosClass, Thrown> {
    match ffi::qos_class() {
        Ok(class) => Ok(match class {
            ffi::qos::USER_INTERACTIVE => QosClass::UserInteractive,
            ffi::qos::USER_INITIATED => QosClass::UserInitiated,
            ffi::qos::UTILITY => QosClass::Utility,
            ffi::qos::BACKGROUND => QosClass::Background,
            _ => QosClass::Default,
        }),
        Err((class, errno)) => {
            failed(req, errno);
            Err(throw(req, "Error", &format!("invalid QOS class {class}")))
        }
    }
}

/// `pcntl_setqos_class(Pcntl\QosClass $qos_class =
/// Pcntl\QosClass::Default): void`: the calling thread's class set. The
/// stub shows a default the body's `ZEND_PARSE_PARAMETERS_START(1, 1)`
/// refuses, so a call with none is php's `ArgumentCountError`.
///
/// ```php
/// pcntl_setqos_class(Pcntl\QosClass::UserInitiated);
/// var_dump(pcntl_getqos_class());
/// pcntl_setqos_class(Pcntl\QosClass::Default);
/// var_dump(pcntl_getqos_class());
/// try { pcntl_setqos_class(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// enum(Pcntl\QosClass::UserInitiated)
/// enum(Pcntl\QosClass::Default)
/// pcntl_setqos_class() expects exactly 1 argument, 0 given
/// ```
#[cfg(target_vendor = "apple")]
#[php_function(arity = manual)]
pub(crate) fn pcntl_setqos_class(
    req: &mut Request,
    #[php(default = Pcntl::QosClass::Default)] qos_class: QosClass,
) -> Result<(), Thrown> {
    arity(req, 1, Some(1))?;
    let class = match qos_class {
        QosClass::UserInteractive => ffi::qos::USER_INTERACTIVE,
        QosClass::UserInitiated => ffi::qos::USER_INITIATED,
        QosClass::Default => ffi::qos::DEFAULT,
        QosClass::Utility => ffi::qos::UTILITY,
        QosClass::Background => ffi::qos::BACKGROUND,
    };
    if let Err(errno) = ffi::set_qos_class(class) {
        failed(req, errno);
        return Err(throw(req, "Error", "pcntl_setqos_class failed"));
    }
    Ok(())
}
