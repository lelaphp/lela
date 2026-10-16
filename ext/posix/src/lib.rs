//! The `posix` extension: `php-src/ext/posix` as one crate under `ext/`
//! (ADR-0004). One workspace dependency, the facade; the OS through `nix`,
//! and through `libc` in [`ffi`] alone for what `nix` does not wrap (design
//! §7.2, revision 35); one entry, `extension()`; its state on the value
//! (ADR-0005). The phpt subset is `php-src/ext/posix/tests`, through
//! `xtask phpt --subset`.
//!
//! Every native is `posix.c`'s at the tag, in `posix.stub.php`'s order,
//! and a call that fails leaves its errno in `POSIX_G(last_error)`, which
//! [`State`] is and `posix_get_last_error()` reads. The process's ids, its
//! limits and its session are the process's: a native here that sets one
//! sets it for every request the process serves, which is why the crate
//! is in the `php` binary and no server host (revision 35). A path is
//! read against the request's working directory, as php's
//! `expand_filepath` reads it; the engine has no `open_basedir`, so none
//! of php's checks against it stands here.
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

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::{Mode, SFlag, mknod};
use nix::unistd::{
    AccessFlags, Gid, Group, Pid, Uid, User, access, getpgid, getpgrp, getpid, getppid, getsid,
    mkfifo, setegid, seteuid, setgid, setpgid, setsid, setuid,
};

use php_ext::abi::{Extension, ExtensionBuilder, OrFalse, Untyped};
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::{coerce_param, to_long, value_name};
use php_ext::decl::Type;
use php_ext::process;
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, clear, docref, throw_error,
};
use php_ext::request::{Cast, Request, Resolve, Stream};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. php's `zend_posix_globals`.
#[derive(Default)]
struct State {
    /// `POSIX_G(last_error)`: the errno of the last call that failed.
    last_error: i32,
}

/// `POSIX_PID_MIN` and `POSIX_PID_MAX`: a C `int`'s range.
const PID_MIN: i64 = i32::MIN as i64;
const PID_MAX: i64 = i32::MAX as i64;

/// A `zend_long` handed to a C `int` parameter, as C converts it: the low
/// 32 bits, so `2**32 + 1` is `1`. php passes its longs to `kill`,
/// `setpgid`, `sysconf` and the rest this way, and a program sees it.
#[allow(clippy::cast_possible_truncation)]
pub(crate) const fn as_c_int(n: i64) -> libc::c_int {
    n as libc::c_int
}

/// A `zend_long` handed to a `uid_t` or `gid_t` parameter, as C converts
/// it: the low 32 bits, unsigned, so `-1` is `4294967295`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) const fn as_id(n: i64) -> u32 {
    n as u32
}

/// A `zend_long` handed to a `mode_t` parameter, as C converts it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn as_mode(n: i64) -> libc::mode_t {
    n as libc::mode_t
}

/// A C `int` bound printed through `ZEND_LONG_FMT`, as
/// `PHP_POSIX_CHECK_PID` prints it: the 32 bits read back as an unsigned
/// number, so `INT_MIN` prints `2147483648`.
#[allow(clippy::cast_sign_loss)]
const fn as_printed(bound: i64) -> u32 {
    as_c_int(bound) as u32
}

/// The constants `register_posix_symbols` registers, in the stub's
/// order, each under the stub's `#ifdef`: what the platform's headers
/// name.
///
/// ```php
/// var_dump(POSIX_F_OK, POSIX_R_OK | POSIX_W_OK | POSIX_X_OK, POSIX_S_IFIFO, POSIX_RLIMIT_NOFILE, POSIX_SC_PAGESIZE, POSIX_RLIMIT_INFINITY === PHP_INT_MAX);
/// ```
/// ```expect
/// int(0)
/// int(7)
/// int(4096)
/// int(8)
/// int(29)
/// bool(true)
/// ```
#[php_const]
mod constants {
    pub(crate) const POSIX_F_OK: i64 = libc::F_OK as i64;
    pub(crate) const POSIX_X_OK: i64 = libc::X_OK as i64;
    pub(crate) const POSIX_W_OK: i64 = libc::W_OK as i64;
    pub(crate) const POSIX_R_OK: i64 = libc::R_OK as i64;
    pub(crate) const POSIX_S_IFREG: i64 = libc::S_IFREG as i64;
    pub(crate) const POSIX_S_IFCHR: i64 = libc::S_IFCHR as i64;
    pub(crate) const POSIX_S_IFBLK: i64 = libc::S_IFBLK as i64;
    pub(crate) const POSIX_S_IFIFO: i64 = libc::S_IFIFO as i64;
    pub(crate) const POSIX_S_IFSOCK: i64 = libc::S_IFSOCK as i64;
    pub(crate) const POSIX_RLIMIT_AS: i64 = libc::RLIMIT_AS as i64;
    pub(crate) const POSIX_RLIMIT_CORE: i64 = libc::RLIMIT_CORE as i64;
    pub(crate) const POSIX_RLIMIT_CPU: i64 = libc::RLIMIT_CPU as i64;
    pub(crate) const POSIX_RLIMIT_DATA: i64 = libc::RLIMIT_DATA as i64;
    pub(crate) const POSIX_RLIMIT_FSIZE: i64 = libc::RLIMIT_FSIZE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const POSIX_RLIMIT_LOCKS: i64 = libc::RLIMIT_LOCKS as i64;
    pub(crate) const POSIX_RLIMIT_MEMLOCK: i64 = libc::RLIMIT_MEMLOCK as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const POSIX_RLIMIT_MSGQUEUE: i64 = libc::RLIMIT_MSGQUEUE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const POSIX_RLIMIT_NICE: i64 = libc::RLIMIT_NICE as i64;
    pub(crate) const POSIX_RLIMIT_NOFILE: i64 = libc::RLIMIT_NOFILE as i64;
    pub(crate) const POSIX_RLIMIT_NPROC: i64 = libc::RLIMIT_NPROC as i64;
    pub(crate) const POSIX_RLIMIT_RSS: i64 = libc::RLIMIT_RSS as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const POSIX_RLIMIT_RTPRIO: i64 = libc::RLIMIT_RTPRIO as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const POSIX_RLIMIT_RTTIME: i64 = libc::RLIMIT_RTTIME as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const POSIX_RLIMIT_SIGPENDING: i64 = libc::RLIMIT_SIGPENDING as i64;
    pub(crate) const POSIX_RLIMIT_STACK: i64 = libc::RLIMIT_STACK as i64;
    #[cfg(target_os = "freebsd")]
    pub(crate) const POSIX_RLIMIT_KQUEUES: i64 = libc::RLIMIT_KQUEUES as i64;
    #[cfg(target_os = "freebsd")]
    pub(crate) const POSIX_RLIMIT_NPTS: i64 = libc::RLIMIT_NPTS as i64;
    pub(crate) const POSIX_RLIMIT_INFINITY: i64 = libc::RLIM_INFINITY.cast_signed();
    pub(crate) const POSIX_SC_ARG_MAX: i64 = libc::_SC_ARG_MAX as i64;
    pub(crate) const POSIX_SC_CHILD_MAX: i64 = libc::_SC_CHILD_MAX as i64;
    pub(crate) const POSIX_SC_CLK_TCK: i64 = libc::_SC_CLK_TCK as i64;
    pub(crate) const POSIX_SC_PAGESIZE: i64 = libc::_SC_PAGESIZE as i64;
    pub(crate) const POSIX_SC_NPROCESSORS_CONF: i64 = libc::_SC_NPROCESSORS_CONF as i64;
    pub(crate) const POSIX_SC_NPROCESSORS_ONLN: i64 = libc::_SC_NPROCESSORS_ONLN as i64;
    pub(crate) const POSIX_PC_LINK_MAX: i64 = libc::_PC_LINK_MAX as i64;
    pub(crate) const POSIX_PC_MAX_CANON: i64 = libc::_PC_MAX_CANON as i64;
    pub(crate) const POSIX_PC_MAX_INPUT: i64 = libc::_PC_MAX_INPUT as i64;
    pub(crate) const POSIX_PC_NAME_MAX: i64 = libc::_PC_NAME_MAX as i64;
    pub(crate) const POSIX_PC_PATH_MAX: i64 = libc::_PC_PATH_MAX as i64;
    pub(crate) const POSIX_PC_PIPE_BUF: i64 = libc::_PC_PIPE_BUF as i64;
    pub(crate) const POSIX_PC_CHOWN_RESTRICTED: i64 = libc::_PC_CHOWN_RESTRICTED as i64;
    pub(crate) const POSIX_PC_NO_TRUNC: i64 = libc::_PC_NO_TRUNC as i64;
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    pub(crate) const POSIX_PC_ALLOC_SIZE_MIN: i64 = libc::_PC_ALLOC_SIZE_MIN as i64;
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    pub(crate) const POSIX_PC_SYMLINK_MAX: i64 = libc::_PC_SYMLINK_MAX as i64;
    pub(crate) const POSIX_SC_OPEN_MAX: i64 = libc::_SC_OPEN_MAX as i64;
}

/// The extension value (design §7.1): declarations paired with entries,
/// the constants and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('posix'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::declared())
        .functions(vec![
            posix_kill::declared(),
            posix_getpid::declared(),
            posix_getppid::declared(),
            posix_getuid::declared(),
            posix_setuid::declared(),
            posix_geteuid::declared(),
            posix_seteuid::declared(),
            posix_getgid::declared(),
            posix_setgid::declared(),
            posix_getegid::declared(),
            posix_setegid::declared(),
            posix_getgroups::declared(),
            posix_getlogin::declared(),
            posix_getpgrp::declared(),
            posix_setsid::declared(),
            posix_setpgid::declared(),
            posix_getpgid::declared(),
            posix_getsid::declared(),
            posix_uname::declared(),
            posix_times::declared(),
            posix_ctermid::declared(),
            posix_ttyname::declared(),
            posix_isatty::declared(),
            posix_getcwd::declared(),
            posix_mkfifo::declared(),
            posix_mknod::declared(),
            posix_access::declared(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            posix_eaccess::declared(),
            posix_getgrnam::declared(),
            posix_getgrgid::declared(),
            posix_getpwnam::declared(),
            posix_getpwuid::declared(),
            posix_getrlimit::declared(),
            posix_setrlimit::declared(),
            posix_get_last_error::declared(),
            posix_errno::declared(),
            posix_strerror::declared(),
            posix_initgroups::declared(),
            posix_sysconf::declared(),
            posix_pathconf::declared(),
            posix_fpathconf::declared(),
        ])
        .request_state::<State>()
        .info(posix_info)
        .build("posix")
}

/// `PHP_MINFO_FUNCTION(posix)`: the one-row table.
///
/// ```php
/// (new ReflectionExtension('posix'))->info();
/// ```
/// ```expect
/// posix
///
/// POSIX support => enabled
/// ```
fn posix_info(req: &mut Request) {
    req.info_table(&[&["POSIX support", "enabled"]]);
}

/// `POSIX_G(last_error) = errno` and the `false` every failed call
/// answers.
fn failed(req: &mut Request, errno: Errno) -> bool {
    req.state::<State>().last_error = errno as i32;
    false
}

/// [`failed`] for a native that answers `OrFalse`.
fn failed_or<T>(req: &mut Request, errno: Errno) -> OrFalse<T> {
    failed(req, errno);
    OrFalse::False
}

/// `PHP_POSIX_CHECK_PID`: the `ValueError` for a number outside
/// `lower..=upper`. The macro hands the bounds, C `int`s, to a
/// `ZEND_LONG_FMT` conversion, which reads `INT_MIN`'s 32 bits as a
/// positive 64-bit number: php prints `2147483648`, and so does this.
fn check_pid(
    req: &mut Request,
    pid: i64,
    position: u32,
    lower: i64,
    upper: i64,
) -> Result<(), Thrown> {
    if pid < lower || pid > upper {
        return Err(arg_value_error(
            req,
            position,
            &format!("be between {} and {}", as_printed(lower), as_printed(upper)),
        ));
    }
    Ok(())
}

/// `add_assoc_*`: one row under a string key.
fn set(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    key: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, key, value)
}

/// A string of the request's as a value.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `posix_kill(int $process_id, int $signal): bool`: `kill(2)`. Signal
/// zero asks whether the process may be signalled and sends nothing; a
/// number no signal has is `EINVAL`, as the kernel answers it.
///
/// ```php
/// var_dump(posix_kill(posix_getpid(), 0), posix_kill(posix_getpid(), 999), posix_get_last_error() === 22);
/// try { posix_kill(PHP_INT_MAX, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// posix_kill(): Argument #1 ($process_id) must be between 2147483648 and 2147483647
/// ```
#[php_function]
fn posix_kill(req: &mut Request, process_id: i64, signal: i64) -> Result<bool, Thrown> {
    check_pid(req, process_id, 1, PID_MIN, PID_MAX)?;
    let signal = match signal {
        0 => None,
        n => match i32::try_from(n).ok().and_then(|n| Signal::try_from(n).ok()) {
            Some(signal) => Some(signal),
            None => return Ok(failed(req, Errno::EINVAL)),
        },
    };
    Ok(match kill(Pid::from_raw(as_c_int(process_id)), signal) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    })
}

/// `posix_getpid(): int`: `getpid(2)`.
///
/// ```php
/// var_dump(posix_getpid() === getmypid());
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_getpid() -> i64 {
    i64::from(getpid().as_raw())
}

/// `posix_getppid(): int`: `getppid(2)`.
///
/// ```php
/// var_dump(posix_getppid() > 0, posix_getppid() !== posix_getpid());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn posix_getppid() -> i64 {
    i64::from(getppid().as_raw())
}

/// `posix_getuid(): int`: `getuid(2)`, the engine's (`php_ext::process`).
///
/// ```php
/// var_dump(posix_getuid() === posix_getpwnam(posix_getpwuid(posix_getuid())['name'])['uid']);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_getuid() -> i64 {
    i64::from(process::uid())
}

/// `posix_setuid(int $user_id): bool`: `setuid(2)`, the errno kept where
/// it refuses. The witness sets the id the process already has, and asks
/// for root where it is not root, so the harness's own ids never move.
///
/// ```php
/// var_dump(posix_setuid(posix_getuid()), posix_getuid() === 0 || !posix_setuid(0), posix_getuid() === 0 || posix_get_last_error() === 1);
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn posix_setuid(req: &mut Request, user_id: i64) -> bool {
    match setuid(Uid::from_raw(as_id(user_id))) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    }
}

/// `posix_geteuid(): int`: `geteuid(2)`, the engine's (`php_ext::process`).
///
/// ```php
/// var_dump(posix_geteuid() === posix_getuid());
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_geteuid() -> i64 {
    i64::from(process::euid())
}

/// `posix_seteuid(int $user_id): bool`: `seteuid(2)`.
///
/// ```php
/// var_dump(posix_seteuid(posix_geteuid()), posix_geteuid() === 0 || !posix_seteuid(0));
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn posix_seteuid(req: &mut Request, user_id: i64) -> bool {
    match seteuid(Uid::from_raw(as_id(user_id))) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    }
}

/// `posix_getgid(): int`: `getgid(2)`, the engine's (`php_ext::process`).
///
/// ```php
/// var_dump(posix_getgid() === posix_getpwuid(posix_getuid())['gid']);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_getgid() -> i64 {
    i64::from(process::gid())
}

/// `posix_setgid(int $group_id): bool`: `setgid(2)`.
///
/// ```php
/// var_dump(posix_setgid(posix_getgid()), posix_getuid() === 0 || !posix_setgid(0));
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn posix_setgid(req: &mut Request, group_id: i64) -> bool {
    match setgid(Gid::from_raw(as_id(group_id))) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    }
}

/// `posix_getegid(): int`: `getegid(2)`, the engine's (`php_ext::process`).
///
/// ```php
/// var_dump(posix_getegid() === posix_getgid());
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_getegid() -> i64 {
    i64::from(process::egid())
}

/// `posix_setegid(int $group_id): bool`: `setegid(2)`.
///
/// ```php
/// var_dump(posix_setegid(posix_getegid()));
/// ```
/// ```expect process
/// bool(true)
/// ```
#[php_function]
fn posix_setegid(req: &mut Request, group_id: i64) -> bool {
    match setegid(Gid::from_raw(as_id(group_id))) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    }
}

/// `posix_getgroups(): array|false`: the supplementary group ids, a list,
/// the engine's (`php_ext::process`); `false` with the errno kept where
/// the OS refuses.
///
/// The witness asks in a child and prints what the child printed: the
/// fence holds what both hosts print and leaves the child's ending
/// unread.
///
/// ```php
/// $child = '$g = posix_getgroups(); var_dump(array_is_list($g), in_array(posix_getegid(), $g, true));';
/// $p = proc_open([PHP_BINARY, '-n', '-r', $child], [1 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]);
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// ```
///
/// macOS's variant reads Open Directory, and the kernel's list
/// does not (#2772): with the lookup denied to the child by a sandbox,
/// the call fails with `EINVAL`, where the plain `getgroups` would still
/// answer.
///
/// ```php
/// $deny = '(version 1)(allow default)(deny mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))';
/// $child = 'var_dump(posix_getgroups(), posix_get_last_error());';
/// $p = proc_open(['/usr/bin/sandbox-exec', '-p', $deny, PHP_BINARY, '-n', '-r', $child], [1 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]);
/// ```
/// ```expect process
/// bool(false)
/// int(22)
/// ```
#[php_function]
fn posix_getgroups(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let groups = match process::groups() {
        Ok(groups) => groups,
        Err(error) => {
            let errno = Errno::from_raw(error.raw_os_error().unwrap_or(0));
            return Ok(failed_or(req, errno));
        }
    };
    let mut array = PhpArray::new(req)?;
    for gid in groups {
        array.append(req, Owner::int(i64::from(gid)))?;
    }
    Ok(OrFalse::Value(array))
}

/// `posix_getlogin(): string|false`: `getlogin(2)`, false where the
/// process has no login, as a process started with no terminal has not.
///
/// ```php
/// $l = posix_getlogin();
/// var_dump($l === false || is_string($l));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_getlogin(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match ffi::getlogin() {
        Ok(name) => Ok(OrFalse::Value(PhpString::new(req, &name)?)),
        Err(errno) => Ok(failed_or(req, errno)),
    }
}

/// `posix_getpgrp(): int`: `getpgrp(2)`.
///
/// ```php
/// var_dump(posix_getpgrp() === posix_getpgid(0));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_getpgrp() -> i64 {
    i64::from(getpgrp().as_raw())
}

/// `posix_setsid(): int`: `setsid(2)`, `-1` where it refuses, with no
/// errno kept, as php keeps none. A session set in the harness would be
/// the harness's own, so the witness sets one in a forked child, which
/// leads it and so is refused a second, and reads the answer from its
/// status; the parent only calls it wrongly.
///
/// ```php
/// $pid = pcntl_fork();
/// if ($pid === 0) { exit(posix_setsid() === posix_getpid() && posix_setsid() === -1 ? 7 : 1); }
/// pcntl_waitpid($pid, $status);
/// var_dump(pcntl_wexitstatus($status));
/// try { posix_setsid(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect process
/// int(7)
/// posix_setsid() expects exactly 0 arguments, 1 given
/// ```
#[php_function]
fn posix_setsid() -> i64 {
    setsid().map_or(-1, |pid| i64::from(pid.as_raw()))
}

/// `posix_setpgid(int $process_id, int $process_group_id): bool`:
/// `setpgid(2)`, each number held to `0..=INT_MAX` first.
///
/// ```php
/// var_dump(posix_setpgid(2147483647, 0), posix_get_last_error() === 3);
/// try { posix_setpgid(-1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { posix_setpgid(0, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// posix_setpgid(): Argument #1 ($process_id) must be between 0 and 2147483647
/// posix_setpgid(): Argument #2 ($process_group_id) must be between 0 and 2147483647
/// ```
#[php_function]
fn posix_setpgid(
    req: &mut Request,
    process_id: i64,
    process_group_id: i64,
) -> Result<bool, Thrown> {
    check_pid(req, process_id, 1, 0, PID_MAX)?;
    check_pid(req, process_group_id, 2, 0, PID_MAX)?;
    let pid = Pid::from_raw(as_c_int(process_id));
    let group = Pid::from_raw(as_c_int(process_group_id));
    Ok(match setpgid(pid, group) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    })
}

/// `posix_getpgid(int $process_id): int|false`: `getpgid(2)`.
///
/// ```php
/// var_dump(posix_getpgid(0) === posix_getpgrp(), posix_getpgid(2147483647), posix_get_last_error() === 3);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn posix_getpgid(req: &mut Request, process_id: i64) -> OrFalse<i64> {
    match getpgid(Some(Pid::from_raw(as_c_int(process_id)))) {
        Ok(pid) => OrFalse::Value(i64::from(pid.as_raw())),
        Err(errno) => failed_or(req, errno),
    }
}

/// `posix_getsid(int $process_id): int|false`: `getsid(2)`, the number
/// held to `0..=INT_MAX` first.
///
/// ```php
/// var_dump(posix_getsid(0) === posix_getsid(posix_getpid()), posix_getsid(2147483647));
/// try { posix_getsid(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// posix_getsid(): Argument #1 ($process_id) must be between 0 and 2147483647
/// ```
#[php_function]
fn posix_getsid(req: &mut Request, process_id: i64) -> Result<OrFalse<i64>, Thrown> {
    check_pid(req, process_id, 1, 0, PID_MAX)?;
    Ok(match getsid(Some(Pid::from_raw(as_c_int(process_id)))) {
        Ok(pid) => OrFalse::Value(i64::from(pid.as_raw())),
        Err(errno) => failed_or(req, errno),
    })
}

/// `posix_uname(): array|false`: `uname(2)`'s five fields, by php's keys.
///
/// ```php
/// $u = posix_uname();
/// var_dump(array_keys($u), $u['sysname'] === PHP_OS, $u['machine'] === php_uname('m'));
/// ```
/// ```expect
/// array(5) {
///   [0]=>
///   string(7) "sysname"
///   [1]=>
///   string(8) "nodename"
///   [2]=>
///   string(7) "release"
///   [3]=>
///   string(7) "version"
///   [4]=>
///   string(7) "machine"
/// }
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn posix_uname(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let u = match nix::sys::utsname::uname() {
        Ok(u) => u,
        Err(errno) => return Ok(failed_or(req, errno)),
    };
    let mut array = PhpArray::new(req)?;
    for (key, field) in [
        (&b"sysname"[..], u.sysname()),
        (b"nodename", u.nodename()),
        (b"release", u.release()),
        (b"version", u.version()),
        (b"machine", u.machine()),
    ] {
        let value = string(req, field.as_bytes())?;
        set(req, &mut array, key, value)?;
    }
    Ok(OrFalse::Value(array))
}

/// `posix_times(): array|false`: `times(2)`'s ticks and four times.
///
/// ```php
/// $t = posix_times();
/// var_dump(array_keys($t), count(array_filter($t, 'is_int')));
/// ```
/// ```expect
/// array(5) {
///   [0]=>
///   string(5) "ticks"
///   [1]=>
///   string(5) "utime"
///   [2]=>
///   string(5) "stime"
///   [3]=>
///   string(6) "cutime"
///   [4]=>
///   string(6) "cstime"
/// }
/// int(5)
/// ```
#[php_function]
fn posix_times(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let t = match ffi::times() {
        Ok(t) => t,
        Err(errno) => return Ok(failed_or(req, errno)),
    };
    let mut array = PhpArray::new(req)?;
    for (key, count) in [
        (&b"ticks"[..], t.ticks),
        (b"utime", t.utime),
        (b"stime", t.stime),
        (b"cutime", t.cutime),
        (b"cstime", t.cstime),
    ] {
        set(req, &mut array, key, Owner::int(count))?;
    }
    Ok(OrFalse::Value(array))
}

/// `posix_ctermid(): string|false`: `ctermid(3)`, the controlling
/// terminal's path, which is `/dev/tty` wherever there is one.
///
/// ```php
/// var_dump(posix_ctermid());
/// ```
/// ```expect
/// string(8) "/dev/tty"
/// ```
#[php_function]
fn posix_ctermid(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match ffi::controlling_terminal() {
        Ok(path) => Ok(OrFalse::Value(PhpString::new(req, &path)?)),
        Err(errno) => Ok(failed_or(req, errno)),
    }
}

/// Which native reads a `$file_descriptor`: the three read a value that
/// is no resource apart (`posix.c` at the tag).
#[derive(Clone, Copy, PartialEq, Eq)]
enum FdReader {
    /// `posix_ttyname`: the warning, then `zval_get_long`.
    Ttyname,
    /// `posix_isatty`: the warning, and `false`.
    Isatty,
    /// `posix_fpathconf`: the `TypeError`.
    Fpathconf,
}

/// `$file_descriptor` as the three natives read it: a stream resource's
/// fd (`php_posix_stream_get_fd`), or the value as `zend_parse_arg_long`
/// reads an `int` parameter, and then held to `0..=INT_MAX`. `None` is the
/// `false` the native answers, its warning raised.
fn fd_of(req: &mut Request, given: &Value, reader: FdReader) -> Result<Option<i32>, Thrown> {
    let given = given.deref();
    let fd = if let ValueRef::Resource(_) = given.as_ref() {
        let Some(stream) = Stream::from_value(req, &given) else {
            let name = req.active_function_name();
            return Err(throw_error(
                req,
                "TypeError",
                &format!("{name}(): supplied resource is not a valid stream resource"),
            ));
        };
        // `php_posix_stream_get_fd`: each cast asked first, and taken
        // only where the kind says it would answer; a cast that fails
        // after that leaves php's `-1`.
        let fd = if stream.can_cast(req, Cast::Select).is_ok() {
            stream.cast(req, Cast::Select, false)
        } else {
            match stream.can_cast(req, Cast::Internal) {
                Ok(()) => stream.cast(req, Cast::Internal, false),
                Err(label) => {
                    docref(
                        req,
                        Level::Warning,
                        &format!("Could not use stream of type '{label}'"),
                    );
                    return Ok(None);
                }
            }
        };
        fd.map_or(-1, i64::from)
    } else {
        // `Z_PARAM_LONG` past the stream arm, where the stub declares
        // the untyped `$file_descriptor` that admits a resource too.
        match coerce_param(req, &given, &Type::Int, 1) {
            Ok(value) => match value.as_ref() {
                ValueRef::Int(n) => n,
                _ => 0,
            },
            Err(thrown) => {
                clear(req, thrown)?;
                let tail = format!("be of type int|resource, {} given", value_name(req, &given));
                if reader == FdReader::Fpathconf {
                    return Err(arg_type_error(req, 1, &tail));
                }
                docref(
                    req,
                    Level::Warning,
                    &format!("Argument #1 ($file_descriptor) must {tail}"),
                );
                if reader == FdReader::Isatty {
                    return Ok(None);
                }
                to_long(req, &given)?
            }
        }
    };
    if let Ok(fd) = i32::try_from(fd)
        && fd >= 0
    {
        return Ok(Some(fd));
    }
    docref(
        req,
        Level::Warning,
        &format!(
            "Argument #1 ($file_descriptor) must be between 0 and {}",
            i32::MAX
        ),
    );
    req.state::<State>().last_error = Errno::EBADF as i32;
    Ok(None)
}

/// `posix_ttyname($file_descriptor): string|false`: `ttyname_r(3)` over
/// a stream's fd or a number.
///
/// ```php
/// var_dump(posix_ttyname(STDIN) === false || is_string(posix_ttyname(STDIN)), posix_ttyname(-1), posix_ttyname(fopen('php://memory', 'r')));
/// ```
/// ```expect
///
/// Warning: posix_ttyname(): Argument #1 ($file_descriptor) must be between 0 and 2147483647 in %s on line 2
///
/// Warning: posix_ttyname(): Could not use stream of type 'MEMORY' in %s on line 2
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn posix_ttyname(
    req: &mut Request,
    file_descriptor: Untyped<Value>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(fd) = fd_of(req, &file_descriptor.0, FdReader::Ttyname)? else {
        return Ok(OrFalse::False);
    };
    match ffi::ttyname(fd) {
        Ok(path) => Ok(OrFalse::Value(PhpString::new(req, &path)?)),
        Err(errno) => Ok(failed_or(req, errno)),
    }
}

/// `posix_isatty($file_descriptor): bool`: `isatty(3)` over a stream's
/// fd or a number, the errno kept where it is no terminal.
///
/// ```php
/// $f = fopen(__FILE__, 'r');
/// var_dump(posix_isatty($f), posix_get_last_error() === 25, posix_isatty(10024), posix_get_last_error() === 9, posix_isatty('x'));
/// ```
/// ```expect
///
/// Warning: posix_isatty(): Argument #1 ($file_descriptor) must be of type int|resource, string given in %s on line 3
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
///
/// A stream is asked whether it would cast before it is cast, as
/// `php_posix_stream_get_fd` asks, so a user-space stream whose
/// `stream_cast` refuses is neither sought nor asked its end, and the
/// warning names its kind.
///
/// ```php
/// class W { public $context;
///     function stream_open($p, $m, $o, &$op) { return true; }
///     function stream_seek($o, $w) { echo "sought\n"; return true; }
///     function stream_cast($as) { return false; } }
/// stream_wrapper_register('ww', 'W');
/// var_dump(posix_isatty(fopen('ww://x', 'r')));
/// ```
/// ```expect
/// Warning: posix_isatty(): Could not use stream of type 'user-space' in %s on line 7
/// bool(false)
/// ```
#[php_function]
fn posix_isatty(req: &mut Request, file_descriptor: Untyped<Value>) -> Result<bool, Thrown> {
    let Some(fd) = fd_of(req, &file_descriptor.0, FdReader::Isatty)? else {
        return Ok(false);
    };
    Ok(match ffi::isatty(fd) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    })
}

/// `posix_getcwd(): string|false`: the request's working directory,
/// `VCWD_GETCWD`.
///
/// ```php
/// var_dump(posix_getcwd() === getcwd());
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn posix_getcwd(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let cwd = req.cwd();
    Ok(OrFalse::Value(PhpString::new(
        req,
        cwd.as_os_str().as_bytes(),
    )?))
}

/// `expand_filepath(path, NULL)`: a path against the request's working
/// directory, `None` for the empty one and one that does not resolve.
fn expanded(req: &mut Request, path: &[u8]) -> Option<Vec<u8>> {
    if path.is_empty() {
        return None;
    }
    let cwd = req.cwd();
    let relative_to: &[u8] = if path.first() == Some(&b'/') {
        b""
    } else {
        cwd.as_os_str().as_bytes()
    };
    req.virtual_file_ex(relative_to, path, Resolve::FilePath)
}

/// `posix_mkfifo(string $filename, int $permissions): bool`:
/// `mkfifo(2)`, after the `open_basedir` guard, which refuses without a
/// word and leaves the last error as it stood, as `posix_mknod`'s does.
///
/// ```php
/// @unlink('fifo');
/// var_dump(posix_mkfifo('fifo', 0600), filetype('fifo'), posix_mkfifo('fifo', 0600), posix_get_last_error() === 17);
/// unlink('fifo');
/// ```
/// ```expect
/// bool(true)
/// string(4) "fifo"
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn posix_mkfifo(req: &mut Request, filename: PhpString, permissions: i64) -> bool {
    // The guard refuses without a word, and leaves the last error as it was.
    if req.check_open_basedir(filename.as_bytes(), false).is_err() {
        return false;
    }
    let Some(path) = expanded(req, filename.as_bytes()) else {
        return failed(req, Errno::ENOENT);
    };
    let mode = Mode::from_bits_retain(as_mode(permissions));
    match mkfifo(OsStr::from_bytes(&path), mode) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    }
}

/// `posix_mknod(string $filename, int $flags, int $major = 0, int $minor
/// = 0): bool`: `mknod(2)`; a character or block device needs a major
/// number, which is the `ValueError` otherwise.
///
/// ```php
/// @unlink('node');
/// var_dump(posix_mknod('node', POSIX_S_IFIFO | 0600), filetype('node'));
/// unlink('node');
/// try { posix_mknod('node', POSIX_S_IFCHR | 0600); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(4) "fifo"
/// posix_mknod(): Argument #3 ($major) cannot be 0 for the POSIX_S_IFCHR and POSIX_S_IFBLK modes
/// ```
#[php_function]
// `makedev` takes `int` majors and minors on macOS and unsigned ones on Linux; the cast is C's own conversion.
#[allow(clippy::cast_sign_loss)]
fn posix_mknod(
    req: &mut Request,
    filename: PhpString,
    flags: i64,
    #[php(default = 0)] major: i64,
    #[php(default = 0)] minor: i64,
) -> Result<bool, Thrown> {
    if req.check_open_basedir(filename.as_bytes(), false).is_err() {
        return Ok(false);
    }
    let Some(path) = expanded(req, filename.as_bytes()) else {
        return Ok(failed(req, Errno::ENOENT));
    };
    let device = i64::from(libc::S_IFCHR) | i64::from(libc::S_IFBLK);
    let mut dev = 0;
    if flags & device != 0 {
        if major == 0 {
            return Err(argument_error(
                req,
                "ValueError",
                3,
                "cannot be 0 for the POSIX_S_IFCHR and POSIX_S_IFBLK modes",
            ));
        }
        dev = libc::makedev(as_c_int(major) as _, as_c_int(minor) as _);
    }
    let kind = SFlag::from_bits_retain(as_mode(flags));
    Ok(
        match mknod(OsStr::from_bytes(&path), kind, Mode::empty(), dev) {
            Ok(()) => true,
            Err(errno) => failed(req, errno),
        },
    )
}

/// `posix_access(string $filename, int $flags = 0): bool`: `access(2)`,
/// the path expanded first; one that does not expand is `EIO`, and one
/// the `open_basedir` guard refuses, silently, `EPERM`.
///
/// ```php
/// var_dump(posix_access(__FILE__, POSIX_R_OK | POSIX_W_OK), posix_access('nope', POSIX_F_OK), posix_get_last_error() === 2, posix_access(''), posix_get_last_error() === 5);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn posix_access(req: &mut Request, filename: PhpString, #[php(default = 0)] flags: i64) -> bool {
    let Some(path) = expanded(req, filename.as_bytes()) else {
        return failed(req, Errno::EIO);
    };
    if req.check_open_basedir(&path, false).is_err() {
        return failed(req, Errno::EPERM);
    }
    let mode = AccessFlags::from_bits_retain(as_c_int(flags));
    match access(OsStr::from_bytes(&path), mode) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    }
}

/// `posix_eaccess(string $filename, int $flags = 0): bool`: `eaccess(3)`,
/// [`posix_access`] by the effective ids, declared where php's build finds
/// the call (`HAVE_EACCESS`: Linux and FreeBSD, not Darwin); the empty path
/// is the `ValueError`, where `posix_access` answers `EIO`. The fence holds
/// the declaration to that condition, and where the function is declared,
/// each of its answers.
///
/// ```php
/// $declared = function_exists('posix_eaccess');
/// var_dump($declared === in_array(PHP_OS, ['Linux', 'FreeBSD'], true));
/// $held = [];
/// if ($declared) {
///     $held[] = posix_eaccess(__FILE__, POSIX_R_OK) === posix_access(__FILE__, POSIX_R_OK);
///     $held[] = posix_eaccess('nope', POSIX_F_OK) === false && posix_get_last_error() === 2;
///     try { posix_eaccess(''); } catch (ValueError $e) { $held[] = $e->getMessage() === 'posix_eaccess(): Argument #1 ($filename) must not be empty'; }
///     $held[] = count($held) === 3;
/// }
/// var_dump(!in_array(false, $held, true));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[php_function]
fn posix_eaccess(
    req: &mut Request,
    filename: php_ext::abi::Path,
    #[php(default = 0)] flags: i64,
) -> Result<bool, Thrown> {
    let php_ext::abi::Path(filename) = filename;
    let Some(path) = expanded(req, filename.as_bytes()) else {
        return Err(arg_value_error(req, 1, "not be empty"));
    };
    if req.check_open_basedir(&path, false).is_err() {
        return Ok(failed(req, Errno::EPERM));
    }
    let Ok(path) = CString::new(path) else {
        return Ok(failed(req, Errno::ENOENT));
    };
    Ok(match ffi::eaccess(&path, flags) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    })
}

/// `php_posix_group_to_array`: name, passwd, members and gid.
fn group_array(req: &mut Request, group: &Group) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    let name = string(req, group.name.as_bytes())?;
    set(req, &mut array, b"name", name)?;
    let passwd = string(req, group.passwd.as_bytes())?;
    set(req, &mut array, b"passwd", passwd)?;
    let mut members = PhpArray::new(req)?;
    for member in &group.mem {
        let member = string(req, member.as_bytes())?;
        members.append(req, member)?;
    }
    set(req, &mut array, b"members", members.into_value())?;
    let gid = Owner::int(i64::from(group.gid.as_raw()));
    set(req, &mut array, b"gid", gid)?;
    Ok(array)
}

/// The group a lookup answered, as the array, or `false` with the errno
/// kept.
fn group_or(
    req: &mut Request,
    found: nix::Result<Option<Group>>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    match found {
        Ok(Some(group)) => Ok(OrFalse::Value(group_array(req, &group)?)),
        Ok(None) => Ok(failed_or(req, Errno::last())),
        Err(errno) => Ok(failed_or(req, errno)),
    }
}

/// `posix_getgrnam(string $name): array|false`: the group database by
/// name.
///
/// ```php
/// $g = posix_getgrgid(posix_getegid());
/// var_dump(posix_getgrnam($g['name']) === $g, array_keys($g), posix_getgrnam('no such group here'));
/// ```
/// ```expect
/// bool(true)
/// array(4) {
///   [0]=>
///   string(4) "name"
///   [1]=>
///   string(6) "passwd"
///   [2]=>
///   string(7) "members"
///   [3]=>
///   string(3) "gid"
/// }
/// bool(false)
/// ```
#[php_function]
fn posix_getgrnam(req: &mut Request, name: PhpString) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Ok(name) = std::str::from_utf8(name.as_bytes()) else {
        return Ok(failed_or(req, Errno::ENOENT));
    };
    let found = Group::from_name(name);
    group_or(req, found)
}

/// `posix_getgrgid(int $group_id): array|false`: the group database by
/// id.
///
/// ```php
/// var_dump(posix_getgrgid(posix_getegid())['gid'] === posix_getegid(), posix_getgrgid(2147483000));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn posix_getgrgid(req: &mut Request, group_id: i64) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let found = Group::from_gid(Gid::from_raw(as_id(group_id)));
    group_or(req, found)
}

/// `php_posix_passwd_to_array`: the seven fields.
fn user_array(req: &mut Request, user: &User) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    let name = string(req, user.name.as_bytes())?;
    set(req, &mut array, b"name", name)?;
    let passwd = string(req, user.passwd.as_bytes())?;
    set(req, &mut array, b"passwd", passwd)?;
    set(
        req,
        &mut array,
        b"uid",
        Owner::int(i64::from(user.uid.as_raw())),
    )?;
    set(
        req,
        &mut array,
        b"gid",
        Owner::int(i64::from(user.gid.as_raw())),
    )?;
    let gecos = string(req, user.gecos.as_bytes())?;
    set(req, &mut array, b"gecos", gecos)?;
    let dir = string(req, user.dir.as_os_str().as_bytes())?;
    set(req, &mut array, b"dir", dir)?;
    let shell = string(req, user.shell.as_os_str().as_bytes())?;
    set(req, &mut array, b"shell", shell)?;
    Ok(array)
}

/// The user a lookup answered, as the array, or `false` with the errno
/// kept.
fn user_or(
    req: &mut Request,
    found: nix::Result<Option<User>>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    match found {
        Ok(Some(user)) => Ok(OrFalse::Value(user_array(req, &user)?)),
        Ok(None) => Ok(failed_or(req, Errno::last())),
        Err(errno) => Ok(failed_or(req, errno)),
    }
}

/// `posix_getpwnam(string $username): array|false`: the user database
/// by name.
///
/// ```php
/// $u = posix_getpwuid(posix_geteuid());
/// var_dump(posix_getpwnam($u['name']) === $u, array_keys($u), posix_getpwnam('no such user here'));
/// ```
/// ```expect
/// bool(true)
/// array(7) {
///   [0]=>
///   string(4) "name"
///   [1]=>
///   string(6) "passwd"
///   [2]=>
///   string(3) "uid"
///   [3]=>
///   string(3) "gid"
///   [4]=>
///   string(5) "gecos"
///   [5]=>
///   string(3) "dir"
///   [6]=>
///   string(5) "shell"
/// }
/// bool(false)
/// ```
#[php_function]
fn posix_getpwnam(
    req: &mut Request,
    username: PhpString,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Ok(name) = std::str::from_utf8(username.as_bytes()) else {
        return Ok(failed_or(req, Errno::ENOENT));
    };
    let found = User::from_name(name);
    user_or(req, found)
}

/// `posix_getpwuid(int $user_id): array|false`: the user database by id.
///
/// ```php
/// var_dump(posix_getpwuid(posix_geteuid())['uid'] === posix_geteuid(), posix_getpwuid(-1));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn posix_getpwuid(req: &mut Request, user_id: i64) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let found = User::from_uid(Uid::from_raw(as_id(user_id)));
    user_or(req, found)
}

/// php's `limits[]`: the resources `posix_getrlimit()` lists, by the
/// names it lists them under, under the platform's `#ifdef`s.
const LIMITS: &[(i64, &str)] = &[
    (libc::RLIMIT_CORE as i64, "core"),
    (libc::RLIMIT_DATA as i64, "data"),
    (libc::RLIMIT_STACK as i64, "stack"),
    (libc::RLIMIT_AS as i64, "totalmem"),
    (libc::RLIMIT_RSS as i64, "rss"),
    (libc::RLIMIT_NPROC as i64, "maxproc"),
    (libc::RLIMIT_MEMLOCK as i64, "memlock"),
    (libc::RLIMIT_CPU as i64, "cpu"),
    (libc::RLIMIT_FSIZE as i64, "filesize"),
    (libc::RLIMIT_NOFILE as i64, "openfiles"),
    #[cfg(target_os = "freebsd")]
    (libc::RLIMIT_KQUEUES as i64, "kqueues"),
    #[cfg(target_os = "freebsd")]
    (libc::RLIMIT_NPTS as i64, "npts"),
];

/// One limit as php renders it: the number, or `"unlimited"`.
fn limit_value(req: &mut Request, limit: Option<i64>) -> Result<Owner<Value>, Thrown> {
    match limit {
        Some(n) => Ok(Owner::int(n)),
        None => string(req, b"unlimited"),
    }
}

/// `posix_getrlimit(?int $resource = null): array|false`: every limit
/// `limits[]` names as `soft <name>` and `hard <name>`, or the one asked
/// for as a list of two.
///
/// ```php
/// $all = posix_getrlimit();
/// var_dump(array_slice(array_keys($all), 0, 4), posix_getrlimit(POSIX_RLIMIT_NOFILE) === [$all['soft openfiles'], $all['hard openfiles']], posix_getrlimit(-1));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(9) "soft core"
///   [1]=>
///   string(9) "hard core"
///   [2]=>
///   string(9) "soft data"
///   [3]=>
///   string(9) "hard data"
/// }
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn posix_getrlimit(
    req: &mut Request,
    #[php(default = null)] resource: Option<i64>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let asked: Vec<(i64, Option<&str>)> = match resource {
        None => LIMITS.iter().map(|&(r, name)| (r, Some(name))).collect(),
        Some(resource) => vec![(resource, None)],
    };
    let mut array = PhpArray::new(req)?;
    for (resource, name) in asked {
        let limits = match ffi::getrlimit(resource) {
            Ok(limits) => limits,
            Err(errno) => {
                array.give_up(req);
                return Ok(failed_or(req, errno));
            }
        };
        let soft = limit_value(req, limits.soft)?;
        let hard = limit_value(req, limits.hard)?;
        if let Some(name) = name {
            set(req, &mut array, format!("soft {name}").as_bytes(), soft)?;
            set(req, &mut array, format!("hard {name}").as_bytes(), hard)?;
        } else {
            array.append(req, soft)?;
            array.append(req, hard)?;
        }
    }
    Ok(OrFalse::Value(array))
}

/// `posix_setrlimit(int $resource, int $soft_limit, int $hard_limit):
/// bool`: `setrlimit(2)`, each limit held to `-1` and up and the soft to
/// the hard first. A limit set in the harness would be its own, so the
/// witness asks what refuses before the call and a resource that does
/// not exist.
///
/// ```php
/// foreach ([[-2, 1], [1, -2], [5, 4]] as [$soft, $hard]) {
///     try { posix_setrlimit(POSIX_RLIMIT_CORE, $soft, $hard); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// var_dump(posix_setrlimit(-1, 1, 1), posix_get_last_error() === 22);
/// ```
/// ```expect
/// posix_setrlimit(): Argument #2 ($soft_limit) must be greater or equal to -1
/// posix_setrlimit(): Argument #3 ($hard_limit) must be greater or equal to -1
/// posix_setrlimit(): Argument #2 ($soft_limit) must be lower or equal to 4
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn posix_setrlimit(
    req: &mut Request,
    resource: i64,
    soft_limit: i64,
    hard_limit: i64,
) -> Result<bool, Thrown> {
    if soft_limit < -1 {
        return Err(arg_value_error(req, 2, "be greater or equal to -1"));
    }
    if hard_limit < -1 {
        return Err(arg_value_error(req, 3, "be greater or equal to -1"));
    }
    if hard_limit > -1 && soft_limit > hard_limit {
        return Err(arg_value_error(
            req,
            2,
            &format!("be lower or equal to {hard_limit}"),
        ));
    }
    Ok(match ffi::setrlimit(resource, soft_limit, hard_limit) {
        Ok(()) => true,
        Err(errno) => failed(req, errno),
    })
}

/// `posix_get_last_error(): int` and its alias `posix_errno`:
/// `POSIX_G(last_error)`, zero until a call fails.
///
/// ```php
/// var_dump(posix_get_last_error(), posix_kill(0x7fffffff, 0), posix_get_last_error(), posix_errno());
/// ```
/// ```expect
/// int(0)
/// bool(false)
/// int(3)
/// int(3)
/// ```
#[php_function(aliases(posix_errno))]
fn posix_get_last_error(req: &mut Request) -> i64 {
    i64::from(req.state::<State>().last_error)
}

/// `posix_strerror(int $error_code): string`: the C library's text.
///
/// ```php
/// var_dump(posix_strerror(1), posix_strerror(9), posix_strerror(0));
/// ```
/// ```expect
/// string(23) "Operation not permitted"
/// string(19) "Bad file descriptor"
/// string(18) "Undefined error: 0"
/// ```
#[php_function]
fn posix_strerror(req: &mut Request, error_code: i64) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, &ffi::strerror(error_code))
}

/// `posix_initgroups(string $username, int $group_id): bool`:
/// `initgroups(3)`; the empty name is `false` without a call, and a
/// process that is not root is refused.
///
/// ```php
/// var_dump(posix_initgroups('', 0), posix_getuid() === 0 || !posix_initgroups('nobody', 0));
/// ```
/// ```expect process
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn posix_initgroups(username: PhpString, group_id: i64) -> bool {
    if username.as_bytes().is_empty() {
        return false;
    }
    let Ok(name) = CString::new(username.as_bytes()) else {
        return false;
    };
    ffi::initgroups(&name, group_id)
}

/// `posix_sysconf(int $conf_id): int`: `sysconf(3)`, `-1` for a name the
/// platform has not.
///
/// ```php
/// var_dump(posix_sysconf(POSIX_SC_PAGESIZE) > 0, posix_sysconf(POSIX_SC_NPROCESSORS_ONLN) > 0, posix_sysconf(-1));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(-1)
/// ```
#[php_function]
fn posix_sysconf(conf_id: i64) -> i64 {
    ffi::sysconf(conf_id)
}

/// `posix_pathconf(string $path, int $name): int|false`: `pathconf(2)`;
/// the empty path is the `ValueError`, and one the `open_basedir` guard
/// refuses its warning and php's `Invalid path supplied`.
///
/// ```php
/// var_dump(posix_pathconf('.', POSIX_PC_NAME_MAX) > 0, posix_pathconf('nope', POSIX_PC_PATH_MAX), posix_get_last_error() === 2);
/// try { posix_pathconf('', POSIX_PC_PATH_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// posix_pathconf(): Argument #1 ($path) must not be empty
/// ```
#[php_function]
fn posix_pathconf(req: &mut Request, path: PhpString, name: i64) -> Result<OrFalse<i64>, Thrown> {
    if path.as_bytes().is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if req.check_open_basedir(path.as_bytes(), true).is_err() {
        let text = format!(
            "Invalid path supplied: {}",
            String::from_utf8_lossy(path.as_bytes())
        );
        docref(req, Level::Warning, &text);
        return Ok(OrFalse::False);
    }
    let resolved = expanded(req, path.as_bytes()).unwrap_or_else(|| path.as_bytes().to_vec());
    let Ok(resolved) = CString::new(resolved) else {
        return Ok(failed_or(req, Errno::ENOENT));
    };
    Ok(match ffi::pathconf(&resolved, name) {
        Ok(value) => OrFalse::Value(value),
        Err(errno) => failed_or(req, errno),
    })
}

/// `posix_fpathconf($file_descriptor, int $name): int|false`:
/// `fpathconf(2)` over a stream's fd or a number; a value neither is the
/// `TypeError`.
///
/// ```php
/// $d = fopen('.', 'r');
/// var_dump(posix_fpathconf($d, POSIX_PC_NAME_MAX) > 0, posix_fpathconf(10024, POSIX_PC_NAME_MAX), posix_get_last_error() === 9);
/// try { posix_fpathconf('x', POSIX_PC_NAME_MAX); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// posix_fpathconf(): Argument #1 ($file_descriptor) must be of type int|resource, string given
/// ```
#[php_function]
fn posix_fpathconf(
    req: &mut Request,
    file_descriptor: Untyped<Value>,
    name: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let Some(fd) = fd_of(req, &file_descriptor.0, FdReader::Fpathconf)? else {
        return Ok(OrFalse::False);
    };
    Ok(match ffi::fpathconf(fd, name) {
        Ok(value) => OrFalse::Value(value),
        Err(errno) => failed_or(req, errno),
    })
}
