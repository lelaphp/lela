//! The `shmop` extension: `php-src/ext/shmop` as one crate under `ext/`
//! (ADR-0004). One workspace dependency, the facade; System V shared
//! memory through `libc` in [`ffi`] alone, since nix binds none of it
//! (design §7.2); one entry, `extension()`. The phpt subset is
//! `php-src/ext/shmop/tests`, through `xtask phpt --subset`.
//!
//! Every native is `shmop.c`'s at the tag, in `shmop.stub.php`'s order.
//! A segment is the process's, as php's is: a `Shmop` object holds one
//! attachment of it, detached when the object is freed, and the segment
//! outlives the request until `shmop_delete` marks it.
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
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_error, docref, fatal, throw_error,
};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use crate::ffi::Segment;
use crate::throwables::Throwables;

/// The extension value (design §7.1): the six natives and the class.
///
/// ```php
/// var_dump(extension_loaded('shmop'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            shmop_open::DECLARED,
            shmop_read::DECLARED,
            shmop_close::DECLARED,
            shmop_size::DECLARED,
            shmop_write::DECLARED,
            shmop_delete::DECLARED,
        ])
        .class::<Shmop>()
        .engine_state(Throwables::start)
        .info(shmop_info)
        .build("shmop")
}

/// `PHP_MINFO_FUNCTION(shmop)`: the one-row table.
///
/// ```php
/// (new ReflectionExtension('shmop'))->info();
/// ```
/// ```expect
/// shmop
///
/// shmop support => enabled
/// ```
fn shmop_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["shmop support", "enabled"]]);
}

/// `final class Shmop`, `@strict-properties`, `@not-serializable`: the
/// payload is the attached segment, `None` only on an object no native
/// made, which none reaches.
///
/// ```php
/// try { new Shmop; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $s = shmop_open(0, "c", 0600, 16);
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $s->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $t = shmop_open(0, "c", 0600, 16);
/// var_dump($s, $s == $t);
/// shmop_delete($s); shmop_delete($t);
/// ```
/// ```expect
/// Cannot directly construct Shmop, use shmop_open() instead
/// Trying to clone an uncloneable object of class Shmop
/// Serialization of 'Shmop' is not allowed
/// Cannot create dynamic property Shmop::$x
/// object(Shmop)#1 (0) {
/// }
/// bool(false)
/// ```
#[php_class(
    final,
    not_serializable,
    size = 88, payload = Option<Segment>,
    handlers = SHMOP,
    dynamic_properties = forbidden
)]
pub(crate) struct Shmop;

#[php_class]
impl Shmop {}

/// `shmop_object_handlers`: `std_object_handlers` with `get_constructor`
/// refusing, `clone_obj` php's null slot and `compare`
/// `zend_objects_not_comparable`; `free_obj`'s `shmdt` is the payload's
/// drop.
const SHMOP: Refusals = Refusals::NONE
    .construction(Some("shmop_open()"))
    .cloning()
    .comparison();

/// What `f` answers of a `Shmop`'s segment, or the `InternalError` for
/// one no native opened.
fn with<R>(
    req: &mut Request,
    shmop: Instance<Shmop>,
    f: impl FnOnce(&Segment) -> R,
) -> Result<R, Thrown> {
    match shmop.payload_mut(req)?.as_ref().map(f) {
        Some(answer) => Ok(answer),
        None => Err(fatal(
            req,
            "InternalError: a Shmop shmop_open() did not open",
        )),
    }
}

/// A count of bytes as a `zend_long`.
fn long(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// How `shmop_open` opens, the one letter of `$mode`.
#[derive(Clone, Copy)]
enum Access {
    /// `"a"`: attach an existing segment read-only.
    Read,
    /// `"c"`: create the segment, or attach the one there.
    Create,
    /// `"n"`: create the segment, failing where one is there.
    New,
    /// `"w"`: attach an existing segment to read and write.
    Write,
}

impl Access {
    /// The letter, `None` for any other string.
    fn of(mode: &[u8]) -> Option<Access> {
        match mode {
            b"a" => Some(Access::Read),
            b"c" => Some(Access::Create),
            b"n" => Some(Access::New),
            b"w" => Some(Access::Write),
            _ => None,
        }
    }

    /// The `shmget` flags it adds to the permissions.
    const fn flags(self) -> i32 {
        match self {
            Access::Read | Access::Write => 0,
            Access::Create => libc::IPC_CREAT,
            Access::New => libc::IPC_CREAT | libc::IPC_EXCL,
        }
    }
}

/// The call of `shmop_open` that failed, which its warning names.
#[derive(Clone, Copy)]
enum Step {
    /// `shmget`.
    Get,
    /// `shmctl(IPC_STAT)`.
    Stat,
    /// `shmat`.
    Attach,
}

impl Step {
    /// The warning `shmop_open` raises for it, with `strerror(errno)`,
    /// and no segment.
    fn failed(self, req: &mut Request, errno: i32) -> Option<Segment> {
        let what = match self {
            Step::Get => "attach or create shared memory segment",
            Step::Stat => "get shared memory segment information",
            Step::Attach => "attach to shared memory segment",
        };
        let text = req.environment().os_error_text(errno);
        docref(req, Level::Warning, &format!("Unable to {what} \"{text}\""));
        None
    }
}

/// `shmop_open(int $key, string $mode, int $permissions, int $size):
/// Shmop|false`: `shmget`, then `shmat`, the segment's size read back
/// with `IPC_STAT`. `$size` is read only where `$mode` creates, and
/// `$key`, `$permissions` and the flags are C `int`s, the low 32 bits.
///
/// ```php
/// $s = shmop_open(0, "c", 0600, 100);
/// var_dump(shmop_size($s));
/// var_dump(shmop_open(0, "a", 0600, 100));
/// try { shmop_open(0, "x", 0600, 100); } catch (ValueError $e) { echo $e->getMessage(), " ", spl_object_id($e), "\n"; }
/// try { shmop_open(0, "c", 0600, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { shmop_open(0, "w", 01000, 10); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// shmop_delete($s);
/// ```
/// ```expect
/// int(100)
///
/// Warning: shmop_open(): Unable to attach or create shared memory segment "Invalid argument" in %s on line 4
/// bool(false)
/// shmop_open(): Argument #2 ($mode) must be a valid access mode 3
/// shmop_open(): Argument #4 ($size) must be greater than 0 for the "c" and "n" access modes
/// shmop_open(): Argument #4 ($size) must be greater than 0 for the "c" and "n" access modes
/// ```
#[php_function]
fn shmop_open(
    req: &mut Request,
    key: i64,
    mode: PhpString,
    permissions: i64,
    size: i64,
) -> Result<OrFalse<Owner<Instance<Shmop>>>, Thrown> {
    if mode.len() != 1 {
        return Err(arg_value_error(req, 2, "be a valid access mode"));
    }
    // `object_init_ex` before the mode is read, so the object takes its
    // id before any `ValueError` does, as php's does.
    let Some(class) = lookup_class(req, b"Shmop")? else {
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, "Class \"Shmop\" not found"));
    };
    let shmop = new_object(req, class)?.into_instance::<Shmop>();
    let opened = open(req, key, mode.as_bytes(), permissions, size);
    if let Ok(Some(segment)) = opened {
        *shmop.payload_mut(req)? = Some(segment);
        return Ok(OrFalse::Value(shmop));
    }
    shmop.give_up(req);
    opened.map(|_| OrFalse::False)
}

/// The body of `shmop_open` after `object_init_ex`: the segment, `None`
/// after the warning a failed call raises.
#[allow(clippy::cast_possible_truncation)]
fn open(
    req: &mut Request,
    key: i64,
    mode: &[u8],
    permissions: i64,
    size: i64,
) -> Result<Option<Segment>, Thrown> {
    let Some(access) = Access::of(mode) else {
        return Err(arg_value_error(req, 2, "be a valid access mode"));
    };
    let flags = permissions as i32 | access.flags();
    let size = match access {
        Access::Create | Access::New => size,
        Access::Read | Access::Write => 0,
    };
    if flags & libc::IPC_CREAT != 0 && size < 1 {
        return Err(arg_value_error(
            req,
            4,
            "be greater than 0 for the \"c\" and \"n\" access modes",
        ));
    }
    let size = usize::try_from(size).unwrap_or(0);
    let id = match ffi::get(key as i32, size, flags) {
        Ok(id) => id,
        Err(errno) => return Ok(Step::Get.failed(req, errno)),
    };
    let len = match ffi::size(id) {
        Ok(len) => len,
        Err(errno) => return Ok(Step::Stat.failed(req, errno)),
    };
    if i64::try_from(len).is_err() {
        let ce = Throwables::of(req)?;
        return Err(argument_error(req, ce.value_error, 4, "is too large"));
    }
    match Segment::attach(id, matches!(access, Access::Read), len) {
        Ok(segment) => Ok(Some(segment)),
        Err(errno) => Ok(Step::Attach.failed(req, errno)),
    }
}

/// `shmop_read(Shmop $shmop, int $offset, int $size): string`: `$size`
/// bytes from `$offset`, or to the end where `$size` is zero.
///
/// ```php
/// $s = shmop_open(0, "c", 0600, 8);
/// shmop_write($s, "abcdefgh", 0);
/// var_dump(shmop_read($s, 2, 3), shmop_read($s, 5, 0), shmop_read($s, 8, 0));
/// try { shmop_read($s, 9, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { shmop_read($s, 4, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { shmop_read($s, 1, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// shmop_delete($s);
/// ```
/// ```expect
/// string(3) "cde"
/// string(3) "fgh"
/// string(0) ""
/// shmop_read(): Argument #2 ($offset) must be between 0 and the segment size
/// shmop_read(): Argument #3 ($size) is out of range
/// shmop_read(): Argument #3 ($size) is out of range
/// ```
///
/// The string is asked for fallibly since 8.5.12: a span the memory
/// limit has no room for is refused, and the limit's fatal is raised as
/// the call returns.
///
/// ```php
/// ini_set('memory_limit', '2M');
/// $s = shmop_open(0, "c", 0600, 5 << 19);
/// shmop_delete($s);
/// $r = shmop_read($s, 0, 0);
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 2637824 bytes) in %s on line 5
/// ```
#[php_function]
fn shmop_read(
    req: &mut Request,
    shmop: Instance<Shmop>,
    offset: i64,
    size: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let len = long(with(req, shmop, Segment::len)?);
    if offset < 0 || offset > len {
        return Err(arg_value_error(req, 2, "be between 0 and the segment size"));
    }
    if size < 0 || offset > i64::MAX - size || offset + size > len {
        let ce = Throwables::of(req)?;
        return Err(argument_error(req, ce.value_error, 3, "is out of range"));
    }
    let count = if size == 0 { len - offset } else { size };
    let (offset, count) = (
        usize::try_from(offset).unwrap_or(0),
        usize::try_from(count).unwrap_or(0),
    );
    let bytes = with(req, shmop, |segment| segment.read(offset, count))?;
    // `zend_string_finit(..., fallible: true)` of the span
    // (`Z/ext/shmop/shmop.c:248` at 8.5.12): where the memory limit
    // refused it, `RETURN_OOM()`, the limit's fatal raised as the call
    // returns.
    match req
        .memory_mut()
        .fallible(|req| PhpString::finit(req, &bytes))?
    {
        Some(read) => Ok(read),
        None => PhpString::new(req, b""), // RETURN_OOM()
    }
}

/// `shmop_close(Shmop $shmop): void`, which does nothing: the object
/// detaches when it is freed.
///
/// ```php
/// $s = shmop_open(0, "c", 0600, 8);
/// shmop_close($s);
/// var_dump(shmop_size($s));
/// shmop_delete($s);
/// ```
/// ```expect
/// Deprecated: Function shmop_close() is deprecated since 8.0, as Shmop objects are freed automatically in %s on line 3
/// int(8)
/// ```
#[php_function(deprecated(since = "8.0", message = "as Shmop objects are freed automatically"))]
fn shmop_close(shmop: Instance<Shmop>) {
    let _ = shmop;
}

/// `shmop_size(Shmop $shmop): int`: the size `IPC_STAT` answered when it
/// was opened, which for `"a"` and `"w"` is the segment's own.
///
/// ```php
/// $s = shmop_open(0, "c", 0600, 1000);
/// var_dump(shmop_size($s));
/// shmop_delete($s);
/// ```
/// ```expect
/// int(1000)
/// ```
#[php_function]
fn shmop_size(req: &mut Request, shmop: Instance<Shmop>) -> Result<i64, Thrown> {
    Ok(long(with(req, shmop, Segment::len)?))
}

/// `shmop_write(Shmop $shmop, string $data, int $offset): int`: as much
/// of `$data` as fits before the segment's end, and the count written.
///
/// ```php
/// $s = shmop_open(0, "c", 0600, 8);
/// var_dump(shmop_write($s, "abc", 0), shmop_write($s, "wxyz", 6), bin2hex(shmop_read($s, 0, 0)));
/// try { shmop_write($s, "a", 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $key = ftok(__FILE__, "w");
/// $c = shmop_open($key, "c", 0600, 8);
/// $r = shmop_open($key, "a", 0, 0);
/// try { shmop_write($r, "a", 0); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// shmop_delete($s); shmop_delete($c);
/// ```
/// ```expect
/// int(3)
/// int(2)
/// string(16) "6162630000007778"
/// shmop_write(): Argument #3 ($offset) is out of range
/// Read-only segment cannot be written
/// ```
#[php_function]
fn shmop_write(
    req: &mut Request,
    shmop: Instance<Shmop>,
    data: PhpString,
    offset: i64,
) -> Result<i64, Thrown> {
    let (read_only, len) = with(req, shmop, |segment| {
        (segment.read_only(), long(segment.len()))
    })?;
    let ce = Throwables::of(req)?;
    if read_only {
        return Err(throw_error(
            req,
            ce.error,
            "Read-only segment cannot be written",
        ));
    }
    if offset < 0 || offset > len {
        return Err(argument_error(req, ce.value_error, 3, "is out of range"));
    }
    let offset = usize::try_from(offset).unwrap_or(0);
    let written = with(req, shmop, |segment| segment.write(offset, data.as_bytes()))?;
    Ok(long(written))
}

/// `shmop_delete(Shmop $shmop): bool`: `IPC_RMID`, the segment removed
/// once the last process detaches.
///
/// ```php
/// $s = shmop_open(0, "c", 0600, 8);
/// var_dump(shmop_delete($s));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn shmop_delete(req: &mut Request, shmop: Instance<Shmop>) -> Result<bool, Thrown> {
    if with(req, shmop, Segment::remove)?.is_err() {
        docref(
            req,
            Level::Warning,
            "Can't mark segment for deletion (are you the owner?)",
        );
        return Ok(false);
    }
    Ok(true)
}
