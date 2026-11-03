//! The `sysvmsg` extension: `php-src/ext/sysvmsg` as one crate under
//! `ext/` (ADR-0004). Two workspace dependencies, the facade and
//! `standard`, whose serializer `sysvmsg.c` calls through
//! `ext/standard/php_var.h` (design §4.10); System V message queues
//! through `libc` in [`ffi`] alone, since nix binds none of them (design
//! §7.2, #1323); one entry, `extension()`. The phpt subset is
//! `php-src/ext/sysvmsg/tests`, through `xtask phpt --subset`.
//!
//! Every native is `sysvmsg.c`'s at the tag, in `sysvmsg.stub.php`'s
//! order. A queue is the system's and outlives the request; a
//! `SysvMessageQueue` names it and holds nothing to let go.
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

use crate::throwables::Throwables;
use php_ext::abi::{Extension, ExtensionBuilder, Instance, OrFalse, Ref, Refusals, Untyped};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{to_long, value_name};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, docref, fatal, throw_error,
};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};
use php_ext_standard::{Serializer, Unserializer};

/// A by-reference parameter with no declared type.
type Out = Ref<Untyped<Value>>;

/// The constants `register_sysvmsg_symbols` registers: php's own bits
/// for `msg_receive`, and the two errnos it reports.
///
/// ```php
/// var_dump(MSG_IPC_NOWAIT, MSG_NOERROR, MSG_EXCEPT, MSG_EAGAIN === (PHP_OS_FAMILY === 'Darwin' ? 35 : 11));
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(4)
/// bool(true)
/// ```
#[php_const]
mod constants {
    pub(crate) const MSG_IPC_NOWAIT: i64 = 1;
    pub(crate) const MSG_EAGAIN: i64 = libc::EAGAIN as i64;
    pub(crate) const MSG_ENOMSG: i64 = libc::ENOMSG as i64;
    pub(crate) const MSG_NOERROR: i64 = 2;
    pub(crate) const MSG_EXCEPT: i64 = 4;
}

/// The extension value (design §7.1): the constants, the seven natives
/// and the class.
///
/// ```php
/// var_dump(extension_loaded('sysvmsg'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::DECLARED)
        .functions(&[
            msg_get_queue::DECLARED,
            msg_send::DECLARED,
            msg_receive::DECLARED,
            msg_remove_queue::DECLARED,
            msg_stat_queue::DECLARED,
            msg_set_queue::DECLARED,
            msg_queue_exists::DECLARED,
        ])
        .class::<SysvMessageQueue>()
        .info(sysvmsg_info)
        .engine_state(throwables::Throwables::start)
        .build("sysvmsg")
}

/// `PHP_MINFO_FUNCTION(sysvmsg)`: the one-row table.
///
/// ```php
/// (new ReflectionExtension('sysvmsg'))->info();
/// ```
/// ```expect
/// sysvmsg
///
/// sysvmsg support => enabled
/// ```
fn sysvmsg_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["sysvmsg support", "enabled"]]);
}

/// `sysvmsg_queue_t` less its `zend_object` and its key, which nothing
/// reads: the queue's id.
#[derive(Clone, Copy)]
pub(crate) struct Queue {
    id: i32,
}

/// `final class SysvMessageQueue`, `@strict-properties`,
/// `@not-serializable`: the payload is the queue, `None` only on an
/// object no native made, which none reaches.
///
/// ```php
/// try { new SysvMessageQueue; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $q = msg_get_queue(ftok(__FILE__, "a"));
/// try { clone $q; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($q); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $q->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($q, $q == $q);
/// msg_remove_queue($q);
/// ```
/// ```expect
/// Cannot directly construct SysvMessageQueue, use msg_get_queue() instead
/// Trying to clone an uncloneable object of class SysvMessageQueue
/// Serialization of 'SysvMessageQueue' is not allowed
/// Cannot create dynamic property SysvMessageQueue::$x
/// object(SysvMessageQueue)#1 (0) {
/// }
/// bool(true)
/// ```
#[php_class(
    final,
    not_serializable,
    size = 72, payload = Option<Queue>,
    handlers = SYSVMSG,
    dynamic_properties = forbidden
)]
pub(crate) struct SysvMessageQueue;

#[php_class]
impl SysvMessageQueue {}

/// `sysvmsg_queue_object_handlers`: `std_object_handlers` with
/// `get_constructor` refusing, `clone_obj` php's null slot and `compare`
/// `zend_objects_not_comparable`.
const SYSVMSG: Refusals = Refusals::NONE
    .construction(Some("msg_get_queue()"))
    .cloning()
    .comparison();

/// The queue's id, or the `InternalError` for an object no native made.
fn id(req: &mut Request, queue: Instance<SysvMessageQueue>) -> Result<i32, Thrown> {
    match *queue.payload_mut(req)? {
        Some(queue) => Ok(queue.id),
        None => Err(fatal(
            req,
            "InternalError: a SysvMessageQueue msg_get_queue() did not make",
        )),
    }
}

/// A `zend_long` handed to a C `int` parameter: the low 32 bits.
#[allow(clippy::cast_possible_truncation)]
const fn c_int(n: i64) -> i32 {
    n as i32
}

/// `msg_get_queue(int $key, int $permissions = 0666):
/// SysvMessageQueue|false`: the queue under `$key`, made with
/// `$permissions` where there is none.
///
/// ```php
/// $q = msg_get_queue(ftok(__FILE__, "b"), 0600);
/// var_dump(msg_stat_queue($q)["msg_perm.mode"] === 0600, msg_remove_queue($q));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn msg_get_queue(
    req: &mut Request,
    key: i64,
    #[php(default = 0666)] permissions: i64,
) -> Result<OrFalse<Owner<Instance<SysvMessageQueue>>>, Thrown> {
    let flags = libc::IPC_CREAT | libc::IPC_EXCL | c_int(permissions);
    let id = match ffi::get(c_int(key), 0).or_else(|_| ffi::get(c_int(key), flags)) {
        Ok(id) => id,
        Err(errno) => {
            let text = req.environment().os_error_text(errno);
            let hex = key.cast_unsigned();
            docref(
                req,
                Level::Warning,
                &format!("Failed for key 0x{hex:x}: {text}"),
            );
            return Ok(OrFalse::False);
        }
    };
    let Some(class) = lookup_class(req, b"SysvMessageQueue")? else {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "Class \"SysvMessageQueue\" not found",
        ));
    };
    let queue = new_object(req, class)?.into_instance::<SysvMessageQueue>();
    *queue.payload_mut(req)? = Some(Queue { id });
    Ok(OrFalse::Value(queue))
}

/// `msg_send(SysvMessageQueue $queue, int $message_type, $message, bool
/// $serialize = true, bool $blocking = true, &$error_code = null): bool`:
/// the message serialized, or as the string it is, sent with its type.
///
/// ```php
/// $q = msg_get_queue(ftok(__FILE__, "c"));
/// var_dump(msg_send($q, 1, 1.5, false), msg_send($q, 2, [1], true));
/// msg_receive($q, 1, $t, 64, $m, false); var_dump($m);
/// msg_receive($q, 2, $t, 64, $m, false); var_dump($m);
/// try { msg_send($q, 1, null, false); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(msg_send($q, 0, "x", true, true, $err), $err);
/// msg_remove_queue($q);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(8) "1.500000"
/// string(14) "a:1:{i:0;i:1;}"
/// msg_send(): Argument #3 ($message) must be of type string|int|float|bool, null given
///
/// Warning: msg_send(): msgsnd failed: Invalid argument in %s on line 7
/// bool(false)
/// int(22)
/// ```
#[php_function]
fn msg_send(
    req: &mut Request,
    queue: Instance<SysvMessageQueue>,
    message_type: i64,
    message: Untyped<Value>,
    #[php(default = true)] serialize: bool,
    #[php(default = true)] blocking: bool,
    #[php(default = null)] error_code: Out,
) -> Result<bool, Thrown> {
    let id = id(req, queue)?;
    let message = message.0;
    let text = if serialize {
        let mut out = Vec::new();
        let mut hash = Serializer::new(req);
        let written = hash.write(req, &mut out, &message);
        hash.finish(req);
        written?;
        out
    } else {
        match message.as_ref() {
            ValueRef::Str(s) => s.as_bytes().to_vec(),
            ValueRef::Int(n) => n.to_string().into_bytes(),
            ValueRef::Bool(b) => vec![if b { b'1' } else { b'0' }],
            ValueRef::Float(d) => percent_f(d).into_bytes(),
            _ => {
                let given = value_name(req, &message);
                let tail = [
                    b"be of type string|int|float|bool, ".as_slice(),
                    &given,
                    b" given",
                ]
                .concat();
                return Err(arg_type_error(req, 3, &tail));
            }
        }
    };
    if let Err(errno) = ffi::send(id, message_type, &text, !blocking) {
        let text = req.environment().os_error_text(errno);
        docref(req, Level::Warning, &format!("msgsnd failed: {text}"));
        error_code.set(req, Untyped(Value::int(i64::from(errno))))?;
        return Ok(false);
    }
    Ok(true)
}

/// `spprintf("%F", d)`: six places, no locale, and php's `NAN` and `INF`.
fn percent_f(d: f64) -> String {
    if d.is_nan() {
        return "NAN".to_owned();
    }
    if d.is_infinite() {
        return if d > 0.0 { "INF" } else { "-INF" }.to_owned();
    }
    format!("{d:.6}")
}

/// The flags of `msg_receive`, php's own bits.
const PHP_MSG_IPC_NOWAIT: i64 = 1;
const PHP_MSG_NOERROR: i64 = 2;
const PHP_MSG_EXCEPT: i64 = 4;

/// `msg_receive(SysvMessageQueue $queue, int $desired_message_type,
/// &$received_message_type, int $max_message_size, mixed &$message, bool
/// $unserialize = true, int $flags = 0, &$error_code = null): bool`: the
/// first message of the type (any type for zero, the lowest up to `-n`
/// for a negative), unserialized unless `$unserialize` is `false`.
///
/// ```php
/// $q = msg_get_queue(ftok(__FILE__, "d"));
/// msg_send($q, 3, ["a" => 1]);
/// var_dump(msg_receive($q, 0, $t, 64, $m), $t, $m);
/// var_dump(msg_receive($q, 0, $t, 64, $m, true, MSG_IPC_NOWAIT, $err), $t, $m, $err === MSG_ENOMSG);
/// msg_send($q, 1, "abcdef", false);
/// var_dump(msg_receive($q, 0, $t, 3, $m, false, MSG_IPC_NOWAIT, $err), $err === 7);
/// var_dump(msg_receive($q, 0, $t, 3, $m, false, MSG_NOERROR), $m);
/// msg_send($q, 1, "not serialized", false);
/// var_dump(msg_receive($q, 0, $t, 64, $m), $m);
/// try { msg_receive($q, 0, $t, 0, $m); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// msg_remove_queue($q);
/// ```
/// ```expect
/// bool(true)
/// int(3)
/// array(1) {
///   ["a"]=>
///   int(1)
/// }
/// bool(false)
/// int(0)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// string(3) "abc"
///
/// Warning: msg_receive(): Message corrupted in %s on line 10
/// bool(false)
/// bool(false)
/// msg_receive(): Argument #4 ($max_message_size) must be greater than 0
/// ```
#[php_function]
#[allow(clippy::too_many_arguments)]
fn msg_receive(
    req: &mut Request,
    queue: Instance<SysvMessageQueue>,
    desired_message_type: i64,
    received_message_type: Out,
    max_message_size: i64,
    message: Ref<Value>,
    #[php(default = true)] unserialize: bool,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] error_code: Out,
) -> Result<bool, Thrown> {
    let Ok(max) = usize::try_from(max_message_size) else {
        return Err(arg_value_error(req, 4, "be greater than 0"));
    };
    if max == 0 {
        return Err(arg_value_error(req, 4, "be greater than 0"));
    }
    if flags & PHP_MSG_EXCEPT != 0 && !ffi::HAS_EXCEPT {
        let text = "MSG_EXCEPT is not supported on your system";
        docref(req, Level::Warning, text);
        return Ok(false);
    }
    let how = ffi::Receive {
        except: flags & PHP_MSG_EXCEPT != 0,
        noerror: flags & PHP_MSG_NOERROR != 0,
        nowait: flags & PHP_MSG_IPC_NOWAIT != 0,
    };
    let id = id(req, queue)?;
    // `safe_emalloc(maxsize, 1, sizeof(struct php_msgbuf))`: the
    // request's memory limit met as php meets it, before the room.
    let asked = PhpString::product(req, max, 1, 16, |_| {})?;
    asked.give_up(req);
    let (kind, text) = match ffi::receive(id, max, desired_message_type, how) {
        Ok(got) => got,
        Err(errno) => {
            received_message_type.set(req, Untyped(Value::int(0)))?;
            message.set(req, Value::bool(false))?;
            error_code.set(req, Untyped(Value::int(i64::from(errno))))?;
            return Ok(false);
        }
    };
    received_message_type.set(req, Untyped(Value::int(kind)))?;
    error_code.set(req, Untyped(Value::int(0)))?;
    if !unserialize {
        let text = PhpString::new(req, &text)?;
        message.set(req, text)?;
        return Ok(true);
    }
    let mut reader = Unserializer::new(req, &text)?;
    // The value read is the caller's, kept beside the read's outcome.
    let (outcome, read) = match reader.local(req) {
        Ok(read) => (Ok(()), read),
        Err(thrown) => (Err(thrown), None),
    };
    if let Err(thrown) = reader.finish(req, outcome) {
        if let Some(value) = read {
            value.give_up(req);
        }
        return Err(thrown);
    }
    let Some(value) = read else {
        docref(req, Level::Warning, "Message corrupted");
        message.set(req, Value::bool(false))?;
        return Ok(false);
    };
    message.set(req, value)?;
    Ok(true)
}

/// `msg_remove_queue(SysvMessageQueue $queue): bool`: `IPC_RMID`.
///
/// ```php
/// $q = msg_get_queue(ftok(__FILE__, "f"));
/// var_dump(msg_remove_queue($q), msg_remove_queue($q));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn msg_remove_queue(req: &mut Request, queue: Instance<SysvMessageQueue>) -> Result<bool, Thrown> {
    Ok(ffi::remove(id(req, queue)?))
}

/// `msg_stat_queue(SysvMessageQueue $queue): array|false`: `IPC_STAT`'s
/// ten members, `false` where the queue is gone.
///
/// ```php
/// $q = msg_get_queue(ftok(__FILE__, "h"), 0640);
/// msg_send($q, 1, "x");
/// $s = msg_stat_queue($q);
/// var_dump(array_keys($s), $s["msg_perm.mode"] === 0640, $s["msg_qnum"], $s["msg_lspid"] === getmypid(), $s["msg_perm.uid"] === getmyuid());
/// msg_remove_queue($q);
/// var_dump(msg_stat_queue($q));
/// ```
/// ```expect
/// array(10) {
///   [0]=>
///   string(12) "msg_perm.uid"
///   [1]=>
///   string(12) "msg_perm.gid"
///   [2]=>
///   string(13) "msg_perm.mode"
///   [3]=>
///   string(9) "msg_stime"
///   [4]=>
///   string(9) "msg_rtime"
///   [5]=>
///   string(9) "msg_ctime"
///   [6]=>
///   string(8) "msg_qnum"
///   [7]=>
///   string(10) "msg_qbytes"
///   [8]=>
///   string(9) "msg_lspid"
///   [9]=>
///   string(9) "msg_lrpid"
/// }
/// bool(true)
/// int(1)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn msg_stat_queue(
    req: &mut Request,
    queue: Instance<SysvMessageQueue>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(stat) = ffi::stat(id(req, queue)?) else {
        return Ok(OrFalse::False);
    };
    let mut array = PhpArray::new(req)?;
    for (key, value) in [
        ("msg_perm.uid", stat.uid),
        ("msg_perm.gid", stat.gid),
        ("msg_perm.mode", stat.mode),
        ("msg_stime", stat.stime),
        ("msg_rtime", stat.rtime),
        ("msg_ctime", stat.ctime),
        ("msg_qnum", stat.qnum),
        ("msg_qbytes", stat.qbytes),
        ("msg_lspid", stat.lspid),
        ("msg_lrpid", stat.lrpid),
    ] {
        array.set_str(req, key.as_bytes(), Owner::int(value))?;
    }
    Ok(OrFalse::Value(array))
}

/// `msg_set_queue(SysvMessageQueue $queue, array $data): bool`: the
/// owner, group, permissions and byte limit `$data` gives, each read as
/// `zval_get_long` reads it, written with `IPC_SET` over what `IPC_STAT`
/// read.
///
/// ```php
/// $q = msg_get_queue(ftok(__FILE__, "i"), 0600);
/// var_dump(msg_set_queue($q, ["msg_perm.mode" => "400 owners", "msg_qbytes" => 1000]));
/// $s = msg_stat_queue($q);
/// var_dump($s["msg_perm.mode"], $s["msg_qbytes"]);
/// msg_remove_queue($q);
/// var_dump(msg_set_queue($q, []));
/// ```
/// ```expect
/// bool(true)
/// int(400)
/// int(1000)
/// bool(false)
/// ```
#[php_function]
fn msg_set_queue(
    req: &mut Request,
    queue: Instance<SysvMessageQueue>,
    data: PhpArray,
) -> Result<bool, Thrown> {
    let id = id(req, queue)?;
    let mut changes = ffi::Changes::default();
    for (key, slot) in [
        ("msg_perm.uid", &mut changes.uid),
        ("msg_perm.gid", &mut changes.gid),
        ("msg_perm.mode", &mut changes.mode),
        ("msg_qbytes", &mut changes.qbytes),
    ] {
        if let Some(item) = data.find(key.as_bytes()) {
            *slot = Some(to_long(req, &item.deref())?);
        }
    }
    Ok(ffi::set(id, &changes))
}

/// `msg_queue_exists(int $key): bool`: whether `msgget` finds a queue
/// under `$key` without making one.
///
/// ```php
/// $k = ftok(__FILE__, "e");
/// var_dump(msg_queue_exists($k));
/// $q = msg_get_queue($k);
/// var_dump(msg_queue_exists($k));
/// msg_remove_queue($q);
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn msg_queue_exists(key: i64) -> bool {
    ffi::get(c_int(key), 0).is_ok()
}
