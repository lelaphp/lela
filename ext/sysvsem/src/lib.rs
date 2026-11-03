//! The `sysvsem` extension: `php-src/ext/sysvsem` as one crate under
//! `ext/` (ADR-0004). One workspace dependency, the facade; System V
//! semaphores through `libc` in [`ffi`] alone, since nix binds none of
//! them (design §7.2, #1323); one entry, `extension()`. The phpt subset
//! is `php-src/ext/sysvsem/tests`, through `xtask phpt --subset`.
//!
//! Every native is `sysvsem.c`'s at the tag, in `sysvsem.stub.php`'s
//! order. A `SysvSemaphore` is php's set of three under one key:
//! semaphore 0 is the semaphore itself, set to `max_acquire` by its
//! first user; 1 counts the users; 2 serialises reading 1 and setting
//! 0. The set is the system's and outlives the request; what one object
//! acquired is given back when it is freed, where it auto-releases.
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
mod throwables;

use php_ext::abi::{Extension, ExtensionBuilder, Instance, OrFalse, Refusals};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Level, Thrown, docref, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::Owner;

use crate::ffi::Op;

/// `SYSVSEM_SEM`, the semaphore itself.
const SEM: u16 = 0;
/// `SYSVSEM_USAGE`, the count of its users.
const USAGE: u16 = 1;
/// `SYSVSEM_SETVAL`, which serialises reading the count and setting the
/// semaphore.
const SETVAL: u16 = 2;
/// `SEM_UNDO` as `sem_flg` carries it.
#[allow(clippy::cast_possible_truncation)]
const UNDO: i16 = libc::SEM_UNDO as i16;
/// `IPC_NOWAIT` as `sem_flg` carries it.
#[allow(clippy::cast_possible_truncation)]
const NOWAIT: i16 = libc::IPC_NOWAIT as i16;

/// The extension value (design §7.1): the four natives and the class.
///
/// ```php
/// var_dump(extension_loaded('sysvsem'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            sem_get::DECLARED,
            sem_acquire::DECLARED,
            sem_release::DECLARED,
            sem_remove::DECLARED,
        ])
        .class::<SysvSemaphore>()
        .engine_state(throwables::Throwables::start)
        .info(sysvsem_info)
        .build("sysvsem")
}

/// `PHP_MINFO_FUNCTION(sysvsem)`: the one-row table.
///
/// ```php
/// (new ReflectionExtension('sysvsem'))->info();
/// ```
/// ```expect
/// sysvsem
///
/// sysvsem support => enabled
/// ```
fn sysvsem_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["sysvsem support", "enabled"]]);
}

/// What an object holds of its set: the count it acquired and whether it
/// gives it back when freed, or nothing once `sem_remove` removed the
/// set (php's `count == -1`).
#[derive(Clone, Copy)]
enum Hold {
    Held { count: i32, auto_release: bool },
    Removed,
}

/// `sysvsem_sem` less its `zend_object`: the key, for the warnings, the
/// set's id, and what the object holds of it.
pub(crate) struct Sem {
    key: i32,
    id: i32,
    hold: Hold,
}

impl Drop for Sem {
    /// `sysvsem_free_obj`: where the object auto-releases and the set is
    /// not removed, one user less, and what it acquired given back.
    fn drop(&mut self) {
        let Hold::Held {
            count,
            auto_release: true,
        } = self.hold
        else {
            return;
        };
        let mut ops = vec![Op {
            num: USAGE,
            delta: -1,
            flags: UNDO,
        }];
        if count != 0 {
            #[allow(clippy::cast_possible_truncation)]
            ops.push(Op {
                num: SEM,
                delta: count as i16,
                flags: UNDO,
            });
        }
        ffi::op(self.id, &ops).ok();
    }
}

/// `final class SysvSemaphore`, `@strict-properties`,
/// `@not-serializable`: the payload is the set, `None` only on an object
/// no native made, which none reaches.
///
/// ```php
/// try { new SysvSemaphore; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $s = sem_get(0);
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $s->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($s, $s == $s);
/// sem_remove($s);
/// ```
/// ```expect
/// Cannot directly construct SysvSemaphore, use sem_get() instead
/// Trying to clone an uncloneable object of class SysvSemaphore
/// Serialization of 'SysvSemaphore' is not allowed
/// Cannot create dynamic property SysvSemaphore::$x
/// object(SysvSemaphore)#1 (0) {
/// }
/// bool(true)
/// ```
#[php_class(
    final,
    not_serializable,
    size = 72, payload = Option<Sem>,
    handlers = SYSVSEM,
    dynamic_properties = forbidden
)]
pub(crate) struct SysvSemaphore;

#[php_class]
impl SysvSemaphore {}

/// `sysvsem_object_handlers`: `std_object_handlers` with
/// `get_constructor` refusing, `clone_obj` php's null slot and `compare`
/// `zend_objects_not_comparable`; `free_obj` is the payload's drop.
const SYSVSEM: Refusals = Refusals::NONE
    .construction(Some("sem_get()"))
    .cloning()
    .comparison();

/// `semop` retried while a signal interrupts it, as every loop of
/// `sysvsem.c` retries on `EINTR`.
fn semop(id: i32, ops: &[Op]) -> Result<(), i32> {
    loop {
        match ffi::op(id, ops) {
            Err(libc::EINTR) => {}
            done => return done,
        }
    }
}

/// `sem_get(int $key, int $max_acquire = 1, int $permissions = 0666,
/// bool $auto_release = true): SysvSemaphore|false`: the set of three
/// under `$key`, made where there is none, one user more, and semaphore
/// 0 set to `$max_acquire` where this is its only user. `$key` and
/// `$permissions` are C `int`s, the low 32 bits; the warnings print the
/// key as the `zend_long` it was given.
///
/// ```php
/// $k = ftok(__FILE__, "g");
/// $a = sem_get($k, 2);
/// var_dump(sem_acquire($a, true), sem_acquire($a, true), sem_acquire($a, true));
/// $b = sem_get($k, 5);
/// var_dump(sem_release($a), sem_acquire($b, true));
/// sem_remove($a);
/// $r = sem_get(PHP_INT_MIN, 1, 0400);
/// var_dump(sem_remove($r));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
///
/// Warning: sem_get(): Failed acquiring SYSVSEM_SETVAL for key 0x8000000000000000: Permission denied in %s on line 8
///
/// Warning: sem_get(): Failed releasing SYSVSEM_SETVAL for key 0x8000000000000000: Permission denied in %s on line 8
/// bool(true)
/// ```
#[php_function]
#[allow(clippy::cast_possible_truncation)]
fn sem_get(
    req: &mut Request,
    key: i64,
    #[php(default = 1)] max_acquire: i64,
    #[php(default = 0666)] permissions: i64,
    #[php(default = true)] auto_release: bool,
) -> Result<OrFalse<Owner<Instance<SysvSemaphore>>>, Thrown> {
    let warn = |req: &mut Request, what: &str, errno: i32| {
        let text = req.environment().os_error_text(errno);
        docref(
            req,
            Level::Warning,
            &format!("{what} for key 0x{:x}: {text}", key.cast_unsigned()),
        );
    };
    let id = match ffi::get(key as i32, 3, permissions as i32 | libc::IPC_CREAT) {
        Ok(id) => id,
        Err(errno) => {
            warn(req, "Failed", errno);
            return Ok(OrFalse::False);
        }
    };
    let enter = [
        Op {
            num: SETVAL,
            delta: 0,
            flags: 0,
        },
        Op {
            num: SETVAL,
            delta: 1,
            flags: UNDO,
        },
        Op {
            num: USAGE,
            delta: 1,
            flags: UNDO,
        },
    ];
    if let Err(errno) = semop(id, &enter) {
        warn(req, "Failed acquiring SYSVSEM_SETVAL", errno);
    }
    match ffi::value(id, i32::from(USAGE)) {
        Ok(1) => {
            if let Err(errno) = ffi::set_value(id, i32::from(SEM), max_acquire as i32) {
                warn(req, "Failed", errno);
            }
        }
        Ok(_) => {}
        Err(errno) => warn(req, "Failed", errno),
    }
    let leave = [Op {
        num: SETVAL,
        delta: -1,
        flags: UNDO,
    }];
    if let Err(errno) = semop(id, &leave) {
        warn(req, "Failed releasing SYSVSEM_SETVAL", errno);
    }
    let Some(class) = lookup_class(req, b"SysvSemaphore")? else {
        let ce = throwables::Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "Class \"SysvSemaphore\" not found",
        ));
    };
    let semaphore = new_object(req, class)?.into_instance::<SysvSemaphore>();
    *semaphore.payload_mut(req)? = Some(Sem {
        key: key as i32,
        id,
        hold: Hold::Held {
            count: 0,
            auto_release,
        },
    });
    Ok(OrFalse::Value(semaphore))
}

/// The key, set and hold of a `SysvSemaphore`, or the `InternalError`
/// for one no native made.
fn sem(req: &mut Request, semaphore: Instance<SysvSemaphore>) -> Result<(i32, i32, Hold), Thrown> {
    match semaphore.payload_mut(req)?.as_ref() {
        Some(sem) => Ok((sem.key, sem.id, sem.hold)),
        None => Err(fatal(
            req,
            "InternalError: a SysvSemaphore sem_get() did not make",
        )),
    }
}

/// `php_sysvsem_semop`: semaphore 0 taken one down or given one back,
/// and the object's count moved with it where it succeeds. A set
/// `sem_remove` removed stays removed.
fn php_sysvsem_semop(
    req: &mut Request,
    semaphore: Instance<SysvSemaphore>,
    acquire: bool,
    nowait: bool,
) -> Result<bool, Thrown> {
    let (key, id, hold) = sem(req, semaphore)?;
    let hex = key.cast_unsigned();
    if !acquire && matches!(hold, Hold::Held { count: 0, .. }) {
        let text = format!("SysV semaphore for key 0x{hex:x} is not currently acquired");
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    let op = Op {
        num: SEM,
        delta: if acquire { -1 } else { 1 },
        flags: UNDO | if nowait { NOWAIT } else { 0 },
    };
    if let Err(errno) = semop(id, &[op]) {
        if errno != libc::EAGAIN {
            let what = if acquire { "acquire" } else { "release" };
            let text = req.environment().os_error_text(errno);
            docref(
                req,
                Level::Warning,
                &format!("Failed to {what} key 0x{hex:x}: {text}"),
            );
        }
        return Ok(false);
    }
    if let (
        Some(sem),
        Hold::Held {
            count,
            auto_release,
        },
    ) = (semaphore.payload_mut(req)?.as_mut(), hold)
    {
        let count = if acquire { count + 1 } else { count - 1 };
        sem.hold = Hold::Held {
            count,
            auto_release,
        };
    }
    Ok(true)
}

/// `sem_acquire(SysvSemaphore $semaphore, bool $non_blocking = false):
/// bool`: semaphore 0 taken one down, waiting for it unless
/// `$non_blocking`, which answers `false` without a warning where it
/// would wait.
///
/// ```php
/// $s = sem_get(0, 1);
/// var_dump(sem_acquire($s), sem_acquire($s, true));
/// sem_remove($s);
/// var_dump(sem_acquire($s));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
///
/// Warning: sem_acquire(): Failed to acquire key 0x0: %s in %s on line 5
/// bool(false)
/// ```
#[php_function]
fn sem_acquire(
    req: &mut Request,
    semaphore: Instance<SysvSemaphore>,
    #[php(default = false)] non_blocking: bool,
) -> Result<bool, Thrown> {
    php_sysvsem_semop(req, semaphore, true, non_blocking)
}

/// `sem_release(SysvSemaphore $semaphore): bool`: semaphore 0 given one
/// back, refused with a warning where this object acquired none.
///
/// ```php
/// $s = sem_get(0);
/// var_dump(sem_release($s));
/// sem_acquire($s);
/// var_dump(sem_release($s));
/// sem_remove($s);
/// ```
/// ```expect
/// Warning: sem_release(): SysV semaphore for key 0x0 is not currently acquired in %s on line 3
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn sem_release(req: &mut Request, semaphore: Instance<SysvSemaphore>) -> Result<bool, Thrown> {
    php_sysvsem_semop(req, semaphore, false, false)
}

/// `sem_remove(SysvSemaphore $semaphore): bool`: the set removed from
/// the system, after which freeing the object gives nothing back.
///
/// ```php
/// $k = ftok(__FILE__, "r");
/// $a = sem_get($k);
/// $b = sem_get($k);
/// sem_acquire($a);
/// var_dump(sem_acquire($b, true));
/// unset($a);
/// var_dump(sem_acquire($b, true), sem_remove($b), sem_remove($b));
/// ```
/// ```expect
/// bool(false)
///
/// Warning: sem_remove(): SysV semaphore for key 0x%x does not (any longer) exist in %s on line 8
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn sem_remove(req: &mut Request, semaphore: Instance<SysvSemaphore>) -> Result<bool, Thrown> {
    let (key, id, _) = sem(req, semaphore)?;
    let hex = key.cast_unsigned();
    if !ffi::exists(id) {
        let text = format!("SysV semaphore for key 0x{hex:x} does not (any longer) exist");
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    if let Err(errno) = ffi::remove(id) {
        let text = req.environment().os_error_text(errno);
        docref(
            req,
            Level::Warning,
            &format!("Failed for SysV semaphore for key 0x{hex:x}: {text}"),
        );
        return Ok(false);
    }
    if let Some(sem) = semaphore.payload_mut(req)?.as_mut() {
        sem.hold = Hold::Removed;
    }
    Ok(true)
}
