//! The process natives: `fork`, the waits and the status words, `exec`,
//! `alarm`, the priorities, the errors and Apple's `QoS` classes, each
//! `PHP_FUNCTION(pcntl_*)` of `pcntl.c` at the tag.
//!
//! A witness here runs in the harness's own process beside others, so
//! one that waits names the child it forked and never `-1`, and what
//! changes the process as a whole (an alarm that fires, an `exec` that
//! succeeds) happens in a forked child whose status the parent reads.

use std::ffi::CString;
use std::ops::ControlFlow;

use nix::errno::Errno;
use php_ext::abi::{False, OrFalse, Passed, Path, Ref, Untyped};
use php_ext::attr::php_class;
use php_ext::attr::php_function;
use php_ext::coerce::{to_long, with_string};
#[cfg(target_vendor = "apple")]
use php_ext::raising::throw_error;
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref};
use php_ext::request::{Forked, Realpaths, Request, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value};

use crate::throwables::Throwables;
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
/// ```expect process
/// bool(true)
/// bool(true)
/// int(3)
/// ```
///
/// The fork is the engine's ([`Limits::fork`], #1807): its timer thread
/// is joined before the fork and started again after it in the parent,
/// over the deadlines it had, so the parent's limit, counted from before
/// the fork, still stops it, and the child inherits none.
///
/// ```php
/// set_time_limit(1);
/// if (($child = pcntl_fork()) === 0) { exit(0); }
/// pcntl_waitpid($child, $status);
/// for (;;) {}
/// ```
/// ```expect exit=255 process wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 5
/// ```
///
/// The child starts with the stat cache and the realpath cache empty,
/// as php 8.5.12's `php_child_init` empties both
/// (`php_clear_stat_cache(true, NULL, 0)`, `main/main.c:1901`,
/// GH-23986), so a file the parent stat'ed and a link it resolved, each
/// since changed by another process, read as they are now, and
/// `/proc/self` is the child's. The realpath cache is the engine's, which
/// its fork empties in every child it makes; the stat cache is the
/// request's, above the engine, so it is emptied here.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'fork');
/// var_dump(filesize($f));
/// if (($pid = pcntl_fork()) === 0) { file_put_contents($f, 'grown'); exit(0); }
/// pcntl_waitpid($pid, $status);
/// if (($pid = pcntl_fork()) === 0) { var_dump(filesize($f)); exit(0); }
/// pcntl_waitpid($pid, $status);
/// var_dump(filesize($f));
/// unlink($f);
/// ```
/// ```expect process
/// int(0)
/// int(5)
/// int(0)
/// ```
///
/// ```php
/// $d = sys_get_temp_dir() . '/fork' . getmypid();
/// mkdir("$d/a", 0777, true); mkdir("$d/b"); symlink("$d/a", "$d/l");
/// echo basename(realpath("$d/l")), "\n";
/// if (($pid = pcntl_fork()) === 0) { unlink("$d/l"); symlink("$d/b", "$d/l"); exit(0); }
/// pcntl_waitpid($pid, $status);
/// if (($pid = pcntl_fork()) === 0) { echo basename(realpath("$d/l")), "\n"; exit(0); }
/// pcntl_waitpid($pid, $status);
/// echo basename(realpath("$d/l")), "\n";
/// unlink("$d/l"); rmdir("$d/a"); rmdir("$d/b"); rmdir($d);
/// ```
/// ```expect process
/// a
/// b
/// a
/// ```
#[php_function]
pub(crate) fn pcntl_fork(req: &mut Request) -> i64 {
    match req.limits_mut().fork() {
        Ok(Forked::Parent { child }) => child.as_raw().into(),
        Ok(Forked::Child) => {
            req.clear_stat_cache(Realpaths::Kept);
            0
        }
        Err(error) => {
            let errno = Errno::from_raw(error.raw_os_error().unwrap_or(0));
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
/// ```expect process
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
/// ```expect process
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
    let mut usage = resource_usage.is_given().then(ffi::zeroed_usage);
    // The wait lends the request ([`Limits::blocking`]), so a callback C
    // makes from another thread meanwhile is served in it (#2401).
    let waited = req
        .limits_mut()
        .blocking(|| ffi::wait(pid, &mut word, options, usage.as_mut()));
    let child = match waited {
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
        resource_usage.set(req, Untyped(array.into_value()))?;
    }
    status.set(req, Untyped(Value::int(word.into())))?;
    Ok(child.into())
}

/// One `rusage` member as php's `zend_long`, whatever C type the
/// platform gives it.
fn long(member: impl Into<i64>) -> i64 {
    member.into()
}

/// `PHP_RUSAGE_TO_ARRAY`: the special members, then the two times.
fn usage_into(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    usage: &ffi::Usage,
) -> Result<(), Thrown> {
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
        array.set_str(req, name, Owner::int(value))?;
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
/// ```expect process
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
    let id = id.unwrap_or(0);
    let posted = match req
        .limits_mut()
        .blocking(|| ffi::waitid(idtype, id, options))
    {
        Ok(posted) => posted,
        Err(errno) => {
            failed(req, errno);
            return Ok(false);
        }
    };
    if info.is_given() {
        let array = crate::signals::siginfo(req, libc::SIGCHLD, &posted)?;
        info.set(req, Untyped(array.into_value()))?;
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
/// ```expect process
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
/// ```expect process
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
/// ```expect process
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
/// ```expect process
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
/// ```expect process
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
/// ```expect process
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
/// ```expect process
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
/// ```expect process
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
    // `ZEND_HASH_FOREACH_VAL(args_hash, element)` and `zval_get_string`,
    // which may serve `__toString`: a foreach walk holding nothing, the
    // argument holding the array.
    let ce = Throwables::of(req)?;
    walk::foreach(req, args, Hold::None, |req, row| -> walk::Step {
        let Ok(argument) = with_string(req, row.value(), |bytes| CString::new(bytes))? else {
            return Err(argument_error(
                req,
                ce.value_error,
                2,
                "individual argument must not contain null bytes",
            ));
        };
        arguments.push(argument);
        Ok(ControlFlow::Continue(()))
    })?;
    let executed = match env_vars {
        Passed::Given(env_vars) => {
            let mut envp = Vec::new();
            walk::foreach(req, env_vars, Hold::None, |req, row| -> walk::Step {
                let value = with_string(req, row.value(), <[u8]>::to_vec)?;
                if value.contains(&0) {
                    return Err(argument_error(
                        req,
                        ce.value_error,
                        3,
                        "value for environment variable must not contain null bytes",
                    ));
                }
                let mut pair = match row.key() {
                    Key::Int(n) => n.to_string().into_bytes(),
                    Key::Str(name) => name.as_bytes().to_vec(),
                };
                if pair.contains(&0) {
                    return Err(argument_error(
                        req,
                        ce.value_error,
                        3,
                        "name for environment variable must not contain null bytes",
                    ));
                }
                pair.push(b'=');
                pair.extend_from_slice(&value);
                envp.push(CString::new(pair).unwrap_or_default());
                Ok(ControlFlow::Continue(()))
            })?;
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
/// `SIGALRM` ends. Its ordering is fixed by construction: the alarm is
/// the child's own, in a process of one thread that sleeps past it, and
/// the parent reads the status only once `pcntl_waitpid` has the child
/// ended.
///
/// ```php
/// var_dump(pcntl_alarm(0));
/// $pid = pcntl_fork();
/// if ($pid === 0) { pcntl_signal(SIGALRM, SIG_DFL); pcntl_alarm(1); sleep(5); exit(0); }
/// pcntl_waitpid($pid, $status);
/// var_dump(pcntl_wifsignaled($status), pcntl_wtermsig($status) === SIGALRM);
/// ```
/// ```expect wall=1s process
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
/// ```expect process
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
/// ```expect process
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
/// ```expect process
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
        let ce = match Throwables::of(req) {
            Ok(ce) => ce,
            Err(fatal) => return fatal,
        };
        argument_error(
            req,
            ce.value_error,
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
/// ```expect process
/// string(18) "No child processes"
/// string(18) "Undefined error: 0"
/// string(20) "Unknown error: 12345"
/// ```
#[php_function]
pub(crate) fn pcntl_strerror(
    req: &mut Request,
    error_code: i64,
) -> Result<Owner<PhpString>, Thrown> {
    #[allow(clippy::cast_possible_truncation)]
    let sentence = strerror(error_code as i32);
    PhpString::new(req, sentence.as_bytes())
}

/// `pcntl_unshare(int $flags): bool`: `unshare(2)`, with php's
/// `ValueError` for flags the kernel refuses and its warning for any
/// other failure.
#[cfg(target_os = "linux")]
#[php_function]
pub(crate) fn pcntl_unshare(req: &mut Request, flags: i64) -> Result<bool, Thrown> {
    let Err(errno) = ffi::unshare(flags) else {
        return Ok(true);
    };
    failed(req, errno);
    let n = errno as i32;
    let text = match errno {
        Errno::EINVAL => {
            let tail = "be a combination of CLONE_* flags, or at least one flag is unsupported by the kernel";
            return Err(arg_value_error(req, 1, tail));
        }
        Errno::ENOMEM => format!("Error {n}: Insufficient memory for unshare"),
        Errno::EPERM => format!("Error {n}: No privilege to use these flags"),
        Errno::ENOSPC => format!(
            "Error {n}: Reached the maximum nesting limit for one of the specified namespaces"
        ),
        Errno::EUSERS => {
            format!("Error {n}: Reached the maximum nesting limit for the user namespace")
        }
        _ => format!("Unknown error {n} has occurred"),
    };
    docref(req, Level::Warning, &text);
    Ok(false)
}

/// `pcntl_getcpuaffinity(?int $process_id = null): array|false`: the
/// CPUs the process may be scheduled on, the calling one for `null`.
#[cfg(target_os = "linux")]
#[php_function]
pub(crate) fn pcntl_getcpuaffinity(
    req: &mut Request,
    #[php(default = null)] process_id: Option<i64>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let pid = process_id.unwrap_or(0);
    let cpus = match ffi::cpu_affinity(pid, ffi::cpus_configured()) {
        Ok(cpus) => cpus,
        Err(errno) => {
            affinity_refused(req, errno, pid, None)?;
            return Ok(OrFalse::False);
        }
    };
    let mut out = PhpArray::new(req)?;
    for cpu in cpus {
        let cpu = i64::try_from(cpu).unwrap_or(i64::MAX);
        out.append(req, Owner::int(cpu))?;
    }
    Ok(OrFalse::Value(out))
}

/// `pcntl_setcpuaffinity(?int $process_id = null, array $cpu_ids = []):
/// bool`: the process held to the CPUs given, each an int or a string
/// holding one, below the CPUs the system has configured.
#[cfg(target_os = "linux")]
#[php_function]
pub(crate) fn pcntl_setcpuaffinity(
    req: &mut Request,
    #[php(default = null)] process_id: Option<i64>,
    #[php(default = [])] cpu_ids: PhpArray,
) -> Result<bool, Thrown> {
    if cpu_ids.is_empty() {
        return Err(arg_value_error(req, 2, "not be empty"));
    }
    let pid = process_id.unwrap_or(0);
    let configured = ffi::cpus_configured();
    let values: Vec<Value> = php_ext::values::walk::rows(req, cpu_ids)
        .map(|(_, value)| value.deref())
        .collect();
    let mut cpus = Vec::new();
    let ce = Throwables::of(req)?;
    for value in values {
        let cpu = match value.as_ref() {
            php_ext::values::ValueRef::Int(n) => n,
            php_ext::values::ValueRef::Str(s) => match Key::lookup(s) {
                Key::Int(n) => n,
                Key::Str(s) => {
                    let shown = String::from_utf8_lossy(s.as_bytes()).into_owned();
                    let tail = format!("cpu id invalid value ({shown})");
                    return Err(argument_error(req, ce.value_error, 2, &tail));
                }
            },
            _ => {
                let given = php_ext::coerce::value_name(req, &value);
                let tail = [
                    b"value must be of type int|string, ".as_slice(),
                    &given,
                    b" given",
                ]
                .concat();
                return Err(argument_error(req, ce.type_error, 2, &tail));
            }
        };
        if !(0..configured).contains(&cpu) {
            let last = configured - 1;
            let tail = format!("cpu id must be between 0 and {last} ({cpu})");
            return Err(argument_error(req, ce.value_error, 2, &tail));
        }
        cpus.push(usize::try_from(cpu).unwrap_or(0));
    }
    match ffi::set_cpu_affinity(pid, &cpus) {
        Ok(()) => Ok(true),
        Err(errno) => {
            affinity_refused(req, errno, pid, Some(2))?;
            Ok(false)
        }
    }
}

/// What `sched_getaffinity` and `sched_setaffinity` refused, as php says
/// it: a `ValueError` for a process that is not there and for a mask the
/// kernel will not take, at the CPU ids where the call was a set, and a
/// warning otherwise.
#[cfg(target_os = "linux")]
fn affinity_refused(
    req: &mut Request,
    errno: Errno,
    pid: i64,
    cpu_ids_at: Option<u32>,
) -> Result<(), Thrown> {
    failed(req, errno);
    let ce = Throwables::of(req)?;
    match errno {
        Errno::ESRCH => {
            let tail = format!("invalid process ({pid})");
            Err(argument_error(req, ce.value_error, 1, &tail))
        }
        Errno::EINVAL => Err(match cpu_ids_at {
            Some(at) => argument_error(
                req,
                ce.value_error,
                at,
                "invalid cpu affinity mask size or unmapped cpu id(s)",
            ),
            None => php_ext::raising::value_error(req, "invalid cpu affinity mask size"),
        }),
        Errno::EPERM => {
            let text = "Calling process not having the proper privileges";
            docref(req, Level::Warning, text);
            Ok(())
        }
        _ => {
            docref(req, Level::Warning, &format!("Error {}", errno as i32));
            Ok(())
        }
    }
}

/// `pcntl_getcpu(): int`: the CPU the calling thread runs on.
#[cfg(target_os = "linux")]
#[php_function]
pub(crate) fn pcntl_getcpu() -> i64 {
    ffi::current_cpu().into()
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
/// php's MINIT registers it on every platform, where the two functions
/// that take and give it are Apple's alone.
///
/// ```php
/// var_dump(Pcntl\QosClass::cases()[0], count(Pcntl\QosClass::cases()));
/// ```
/// ```expect process
/// enum(Pcntl\QosClass::UserInteractive)
/// int(5)
/// ```
#[php_class(namespace(Pcntl))]
pub(crate) enum QosClass {
    UserInteractive,
    UserInitiated,
    Default,
    Utility,
    Background,
}

#[php_class]
impl QosClass {}

/// `pcntl_getqos_class(): Pcntl\QosClass`: the calling thread's class,
/// `Default` for one the enum does not name.
///
/// ```php
/// var_dump(pcntl_getqos_class() instanceof Pcntl\QosClass);
/// ```
/// ```expect process
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
            let ce = Throwables::of(req)?;
            Err(throw_error(
                req,
                ce.error,
                &format!("invalid QOS class {class}"),
            ))
        }
    }
}

/// `pcntl_setqos_class(Pcntl\QosClass $qos_class =
/// Pcntl\QosClass::Default): void`: the calling thread's class set,
/// `Default` where none is given, as 8.5.12's
/// `ZEND_PARSE_PARAMETERS_START(0, 1)` takes it.
///
/// ```php
/// pcntl_setqos_class(Pcntl\QosClass::UserInitiated);
/// var_dump(pcntl_getqos_class());
/// pcntl_setqos_class(Pcntl\QosClass::Background);
/// var_dump(pcntl_getqos_class());
/// pcntl_setqos_class();
/// var_dump(pcntl_getqos_class());
/// ```
/// ```expect process
/// enum(Pcntl\QosClass::UserInitiated)
/// enum(Pcntl\QosClass::Background)
/// enum(Pcntl\QosClass::Default)
/// ```
#[cfg(target_vendor = "apple")]
#[php_function]
pub(crate) fn pcntl_setqos_class(
    req: &mut Request,
    #[php(default = Pcntl::QosClass::Default)] qos_class: QosClass,
) -> Result<(), Thrown> {
    let class = match qos_class {
        QosClass::UserInteractive => ffi::qos::USER_INTERACTIVE,
        QosClass::UserInitiated => ffi::qos::USER_INITIATED,
        QosClass::Default => ffi::qos::DEFAULT,
        QosClass::Utility => ffi::qos::UTILITY,
        QosClass::Background => ffi::qos::BACKGROUND,
    };
    if let Err(errno) = ffi::set_qos_class(class) {
        failed(req, errno);
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, "pcntl_setqos_class failed"));
    }
    Ok(())
}
