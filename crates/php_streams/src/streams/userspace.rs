//! The user-space wrapper (design §4.3 seam 5): php's
//! `main/streams/userspace.c`, a wrapper a program registers with
//! `stream_wrapper_register` whose every operation is a method of an
//! instance of its class. The opener makes the instance, sets its
//! `context` and calls its constructor, then `stream_open`; the stream it
//! opens is a kind whose read, write, seek, flush, stat, `lock`, truncate,
//! option and close are `stream_read` and its siblings on that instance,
//! and a directory it opens is `dir_opendir`'s, read by `dir_readdir`.
//! The path operations, `url_stat`, `unlink`, `rename`, `mkdir`, `rmdir`
//! and `stream_metadata`, each make an instance of their own.
//!
//! A method is called as `zend_call_method_if_exists` calls it: on the
//! instance or its `__call`, and not at all where neither is, which is
//! each operation's `is not implemented!` warning; nothing is called
//! while a throwable a method raised is pending, and that throwable is
//! the native's to raise once it returns (`Request.stream_thrown`).
//!
//! The wrapper is the request's: its table, `FG(stream_wrappers)`, is
//! copied from the built-in one at the first change and dropped with
//! the request (ADR-0005).

use super::{CastTarget, MetaData, Ops, PhpStream, Ret, StatBuf};
use crate::api::decl::ClassKind;
use crate::api::raising::{Level, Thrown, docref};
use crate::api::request::Streams;
use crate::api::request::{Cast, OptionAnswer, Request, Stream, StreamOption, Whence, WrapperOp};
use crate::api::values::{ClassId, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, ValueRef};
use crate::state::Layer;
use crate::streams::StreamLayer;

/// `struct php_user_stream_wrapper` less the scheme, which is the row's:
/// the class every instance is of, and `wrapper.is_url`.
#[derive(Clone)]
pub(crate) struct UserWrapper {
    class: ClassId,
    pub(crate) is_url: bool,
    /// The scheme it was registered under, which tells two registrations
    /// apart as php's two wrapper structs are.
    scheme: Vec<u8>,
}

impl UserWrapper {
    pub(crate) const fn new(class: ClassId, is_url: bool, scheme: Vec<u8>) -> UserWrapper {
        UserWrapper {
            class,
            is_url,
            scheme,
        }
    }

    /// Whether two are one registration.
    pub(crate) fn same(&self, other: &UserWrapper) -> bool {
        self.scheme == other.scheme && self.class == other.class
    }
}

/// What one method call came to: the method is not there, it raised
/// (or was not called because a throwable was pending), or its value.
enum Called {
    Missing,
    Threw,
    Value(Value),
}

/// A throwable a method raised, kept for the native that asked.
fn defer(req: &mut Request, thrown: Thrown) {
    if req.stream_thrown().is_none() {
        *req.stream_thrown() = Some(thrown);
    } else if let Some(object) = thrown.exception() {
        req.release(object.as_value());
    }
}

/// `zend_call_method_if_exists` (`Z/Zend/zend_interfaces.c`): `name` on
/// the object, or the object's `__call` with the name and the arguments
/// as an array where the class has no such method.
fn call(req: &mut Request, object: PhpObject, name: &[u8], args: &[Value]) -> Called {
    if req.stream_thrown().is_some() || req.is_pending() {
        return Called::Threw;
    }
    let class = object.class();
    let lowered = name.to_ascii_lowercase();
    let served = if let Some(function) = req.method_named(class, &lowered) {
        req.serve_method(function, Some(object), class, args, &[])
            .map(Owner::hand_on_unmigrated)
    } else if let Some(function) = req.method_named(class, b"__call") {
        let Ok(called) = PhpString::new(req, name).map(Owner::hand_on_unmigrated) else {
            return Called::Threw;
        };
        let Ok(mut list) = PhpArray::new(req).map(Owner::hand_on_unmigrated) else {
            return Called::Threw;
        };
        for arg in args {
            if list.append(req, Owner::take(req, *arg)).is_err() {
                return Called::Threw;
            }
        }
        let magic = [Value::string(called), Value::array(list)];
        let served = req
            .serve_method(function, Some(object), class, &magic, &[])
            .map(Owner::hand_on_unmigrated);
        for value in magic {
            req.release(value);
        }
        served
    } else {
        return Called::Missing;
    };
    match served {
        // A method that ended the request, `exit` or a fatal error, is
        // php's bailout: nothing after it is served.
        Ok(value) if req.is_pending() => {
            req.release(value);
            Called::Threw
        }
        Ok(value) => Called::Value(value),
        // `zend_call_function`'s tail with no frame standing, as at
        // shutdown: the throwable rendered uncaught, or handed to the
        // exception handler, and not kept for a native that is not there.
        Err(thrown) if !req.executing() => match req.thrown_without_frame(&thrown) {
            Ok(value) => Called::Value(value),
            Err(_) => Called::Threw,
        },
        Err(thrown) => {
            defer(req, thrown);
            Called::Threw
        }
    }
}

/// A string argument of the request's.
fn string_arg(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(PhpString::new(req, bytes)?
        .into_value()
        .hand_on_unmigrated())
}

/// `zend_is_true`.
fn truthy(req: &mut Request, value: &Value) -> bool {
    crate::api::coerce::truthy(req, value)
}

/// `user_stream_create_object` (`Z/main/streams/userspace.c:242`): an
/// instance of the class, `None` for an interface, a trait or an
/// abstract class, which php's test of the class's kind refuses before
/// it makes anything and without a throwable; `object_init_ex`'s `Error` for
/// another class that cannot be instantiated, an enum; its `$context`
/// set to the `php_stream_context` the operation was handed, one owner
/// more of it, or to `null` for none, and its constructor called with
/// nothing.
///
/// ```php
/// abstract class A {} interface I {} trait T {} enum E { case X; }
/// foreach (['A', 'I', 'T', 'E'] as $c) {
///     stream_wrapper_register("w$c", $c);
///     try { var_dump(@fopen("w$c://x", 'r')); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(false)
/// Cannot instantiate enum E
/// ```
fn create_object(req: &mut Request, user: &UserWrapper) -> Result<Option<PhpObject>, Thrown> {
    let refused = req
        .class_decl(user.class)
        .is_some_and(|decl| match decl.kind {
            ClassKind::Interface | ClassKind::Trait => true,
            ClassKind::Class => decl.flags.is_abstract,
            ClassKind::Enum => false,
        });
    if refused {
        return Ok(None);
    }
    req.instantiable(user.class)?;
    let object = crate::api::call::new_object(req, user.class)?.hand_on_unmigrated();
    let context = match req.streams().stream_ctx {
        Some(context) => context.resource().as_value(),
        None => Value::null(),
    };
    let context = Owner::take(req, context);
    if let Err(thrown) = crate::api::call::write_prop(req, &object, b"context", context) {
        req.release(object.as_value());
        return Err(thrown);
    }
    if let Some(constructor) = req.method_named(user.class, b"__construct") {
        match req.serve_method(constructor, Some(object), user.class, &[], &[]) {
            Ok(value) => value.give_up(req),
            Err(thrown) => defer(req, thrown),
        }
    }
    Ok(Some(object))
}

/// The class's name, as the warnings print it.
fn class_name(req: &Request, object: PhpObject) -> String {
    req.class_name(object.class())
}

/// A method's `is not implemented!` warning.
fn not_implemented(req: &mut Request, object: PhpObject, method: &str) {
    let text = format!("{}::{method} is not implemented!", class_name(req, object));
    docref(req, Level::Warning, &text);
}

/// `user_wrapper_opener` (`Z/main/streams/userspace.c:275`): the same
/// path opened from inside its own `stream_open` refused as `infinite
/// recursion prevented`; the instance made; `stream_open` called with the
/// path, the `mode`, the options (`REPORT_ERRORS`) and `$opened_path` by
/// reference; a true answer opens a stream of the user kind, `mode` as
/// given. `options` is what `php_stream_open_wrapper_ex` hands on, its
/// own less `REPORT_ERRORS`: `STREAM_USE_PATH` where `fopen` asked for the
/// include path. A string `stream_open` left in `$opened_path` is the name
/// the open hands back, which an include compiles the file under and
/// lists among the included files. The `Err` inside is the text php's
/// wrapper logged, printed after `Failed to open stream: `.
///
/// ```php
/// file_put_contents(__DIR__ . '/opened.inc', '<?php function g() {}');
/// class W {
///     public $context; private $s;
///     function stream_open($path, $mode, $options, &$opened) {
///         $real = substr($path, strlen('wrap://'));
///         $this->s = fopen($real, $mode);
///         $opened = realpath($real);
///         return true;
///     }
///     function stream_read($n) { return fread($this->s, $n); }
///     function stream_eof() { return feof($this->s); }
///     function stream_stat() { return fstat($this->s); }
///     function stream_set_option($o, $a, $b) { return false; }
/// }
/// stream_wrapper_register('wrap', 'W');
/// require 'wrap://' . __DIR__ . '/opened.inc';
/// var_dump((new ReflectionFunction('g'))->getFileName() === realpath(__DIR__ . '/opened.inc'));
/// var_dump(in_array(realpath(__DIR__ . '/opened.inc'), get_included_files(), true));
/// unlink(__DIR__ . '/opened.inc');
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn open(
    req: &mut Request,
    user: &UserWrapper,
    path: &[u8],
    mode: &[u8],
    options: i64,
) -> Result<Result<Owner<Stream>, String>, Thrown> {
    if req.streams().user_stream_current.as_deref() == Some(path) {
        return Ok(Err(String::from("infinite recursion prevented")));
    }
    let outer = req.streams().user_stream_current.replace(path.to_vec());
    // A wrapper that is no URL wrapper, opening for an include under
    // `allow_url_include=0`, holds the opens it makes to
    // `allow_url_include` as an include's are (`PG(in_user_include)`).
    let in_user_include = req.streams().in_user_include;
    if !user.is_url
        && options & STREAM_OPEN_FOR_INCLUDE != 0
        && !req
            .ini_get(b"allow_url_include")
            .is_none_or(Request::ini_parse_bool)
    {
        req.streams().in_user_include = true;
    }
    let opened = open_with(req, user, path, mode, options);
    req.streams().in_user_include = in_user_include;
    req.streams().user_stream_current = outer;
    opened
}

/// `STREAM_OPEN_FOR_INCLUDE`.
const STREAM_OPEN_FOR_INCLUDE: i64 = 128;

/// `REPORT_ERRORS`, which `mkdir` and `rmdir` hand their methods.
const REPORT_ERRORS: i64 = 8;

fn open_with(
    req: &mut Request,
    user: &UserWrapper,
    path: &[u8],
    mode: &[u8],
    options: i64,
) -> Result<Result<Owner<Stream>, String>, Thrown> {
    let Some(object) = create_object(req, user)? else {
        return Ok(Err(String::from("operation failed")));
    };
    let null = Owner::take(req, Value::null());
    let opened_path = PhpRef::new(req, null)?.hand_on_unmigrated();
    let args = [
        string_arg(req, path)?,
        string_arg(req, mode)?,
        Value::int(options),
        opened_path.as_value(),
    ];
    let called = call(req, object, b"stream_open", &args);
    let named = opened_path.get().deref().as_str_bytes().map(<[u8]>::to_vec);
    for arg in args {
        req.release(arg);
    }
    let answer = match called {
        Called::Missing => Err(format!(
            "\"{}::stream_open\" is not implemented",
            class_name(req, object)
        )),
        Called::Threw => Err(String::from("operation failed")),
        Called::Value(value) => {
            let open = truthy(req, &value);
            req.release(value);
            if open {
                Ok(())
            } else {
                Err(format!(
                    "\"{}::stream_open\" call failed",
                    class_name(req, object)
                ))
            }
        }
    };
    if let Err(message) = answer {
        req.release(object.as_value());
        if let Some(thrown) = req.stream_thrown().take() {
            return Err(thrown);
        }
        return Ok(Err(message));
    }
    if let Some(named) = named {
        req.set_opened_path(&named);
    }
    let kind = UserStream {
        object,
        no_seek: false,
        own: None,
    };
    Ok(Ok(req.stream_alloc(PhpStream::new(Box::new(kind), mode))?))
}

/// `user_wrapper_opendir` (`Z/main/streams/userspace.c:377`): as the
/// opener, with `dir_opendir` and the options, a stream of the user
/// directory kind.
pub(crate) fn open_dir(
    req: &mut Request,
    user: &UserWrapper,
    path: &[u8],
) -> Result<Result<Owner<Stream>, String>, Thrown> {
    if req.streams().user_stream_current.as_deref() == Some(path) {
        return Ok(Err(String::from("infinite recursion prevented")));
    }
    let outer = req.streams().user_stream_current.replace(path.to_vec());
    let opened = open_dir_with(req, user, path);
    req.streams().user_stream_current = outer;
    opened
}

fn open_dir_with(
    req: &mut Request,
    user: &UserWrapper,
    path: &[u8],
) -> Result<Result<Owner<Stream>, String>, Thrown> {
    let Some(object) = create_object(req, user)? else {
        return Ok(Err(String::from("operation failed")));
    };
    let args = [string_arg(req, path)?, Value::int(0)];
    let called = call(req, object, b"dir_opendir", &args);
    for arg in args {
        req.release(arg);
    }
    let answer = match called {
        Called::Missing => Err(format!(
            "\"{}::dir_opendir\" is not implemented",
            class_name(req, object)
        )),
        Called::Threw => Err(String::from("operation failed")),
        Called::Value(value) => {
            let open = truthy(req, &value);
            req.release(value);
            if open {
                Ok(())
            } else {
                Err(format!(
                    "\"{}::dir_opendir\" call failed",
                    class_name(req, object)
                ))
            }
        }
    };
    if let Err(message) = answer {
        req.release(object.as_value());
        if let Some(thrown) = req.stream_thrown().take() {
            return Err(thrown);
        }
        return Ok(Err(message));
    }
    Ok(Ok(req.stream_alloc(PhpStream::new(
        Box::new(UserDir { object }),
        b"r",
    ))?))
}

/// `php_userstream_data_t` under `php_stream_userspace_ops`: the
/// instance, and whether its missing `stream_seek` said the stream
/// cannot seek.
struct UserStream {
    object: PhpObject,
    no_seek: bool,
    /// The stream the kind is under, which its methods hold against
    /// `fclose`.
    own: Option<Stream>,
}

impl UserStream {
    /// `f` with the stream held against `fclose`, php's
    /// `PHP_STREAM_FLAG_NO_FCLOSE` around `stream_read`, `stream_eof`,
    /// `stream_write`, `stream_seek` and `stream_cast`.
    fn held<R>(&mut self, req: &mut Request, f: impl FnOnce(&mut Self, &mut Request) -> R) -> R {
        let own = self.own.map(Stream::resource);
        if let Some(res) = own {
            req.streams().held_streams.push(res);
        }
        let answer = f(self, req);
        if own.is_some() {
            req.streams().held_streams.pop();
        }
        answer
    }

    /// `php_userstreamop_write` (`Z/main/streams/userspace.c:562`):
    /// `stream_write` with the bytes; `false` is `-1`, anything else an
    /// int, one past what was handed php's warning and cut to it.
    fn write_held(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let data = string_arg(req, buf).ok()?;
        let called = call(req, self.object, b"stream_write", &[data]);
        req.release(data);
        let value = match called {
            Called::Missing => {
                not_implemented(req, self.object, "stream_write");
                return None;
            }
            Called::Threw => return None,
            Called::Value(value) => value,
        };
        if matches!(value.as_ref(), ValueRef::Bool(false)) {
            return None;
        }
        let didwrite = crate::api::coerce::to_long(req, &value).unwrap_or(0);
        req.release(value);
        let count = i64::try_from(buf.len()).unwrap_or(i64::MAX);
        if didwrite > count {
            let text = format!(
                "{}::stream_write wrote {} bytes more data than requested ({didwrite} written, {count} max)",
                class_name(req, self.object),
                didwrite - count
            );
            docref(req, Level::Warning, &text);
            return Some(buf.len());
        }
        usize::try_from(didwrite).ok()
    }

    /// `php_userstreamop_read` (`Z/main/streams/userspace.c:614`):
    /// `stream_read` with the count, its string taken, one past the count
    /// php's warning and cut; then `stream_eof`, which is how the stream
    /// learns it is at eof, and whose absence is php's warning and eof.
    /// An answer that cannot be a string is `try_convert_to_string`'s
    /// `Error`, thrown from the native that read.
    ///
    /// ```php
    /// class M { public $context;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_stat() { return false; }
    ///     function stream_read($n) { return new stdClass; } }
    /// stream_wrapper_register('mm', 'M');
    /// try { var_dump(file_get_contents('mm://')); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Object of class stdClass could not be converted to string
    /// ```
    fn read_held(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let count = i64::try_from(buf.len()).unwrap_or(i64::MAX);
        let value = match call(req, self.object, b"stream_read", &[Value::int(count)]) {
            Called::Missing | Called::Threw => return None,
            Called::Value(value) => value,
        };
        if matches!(value.as_ref(), ValueRef::Bool(false)) {
            return None;
        }
        // `try_convert_to_string`: what cannot be a string throws, and the
        // read fails with it pending.
        let text = match crate::api::coerce::to_string(req, &value) {
            Ok(text) => text,
            Err(thrown) => {
                req.release(value);
                defer(req, thrown);
                return None;
            }
        };
        req.release(value);
        let mut didread = text.len();
        if didread > buf.len() {
            let text = format!(
                "{}::stream_read - read {} bytes more data than requested ({didread} read, {count} max) - excess data will be lost",
                class_name(req, self.object),
                didread - buf.len()
            );
            docref(req, Level::Warning, &text);
            didread = buf.len();
        }
        buf[..didread].copy_from_slice(&text.as_bytes()[..didread]);
        text.give_up(req);
        match call(req, self.object, b"stream_eof", &[]) {
            Called::Missing => {
                let text = format!(
                    "{}::stream_eof is not implemented! Assuming EOF",
                    class_name(req, self.object)
                );
                docref(req, Level::Warning, &text);
                *eof = true;
                return None;
            }
            Called::Threw => {
                *eof = true;
                return None;
            }
            Called::Value(value) => {
                if truthy(req, &value) {
                    *eof = true;
                }
                req.release(value);
            }
        }
        Some(didread)
    }

    /// `php_userstreamop_seek` (`Z/main/streams/userspace.c:741`):
    /// `stream_seek` with the offset and the whence; a class with none
    /// makes the stream one that cannot seek; a true answer is followed
    /// by `stream_tell`, whose int is the position.
    #[allow(clippy::unnecessary_wraps)] // `Ops::seek`'s answer, which it is
    fn seek_held(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let whence = match whence {
            Some(Whence::Set) => 0,
            Some(Whence::Cur) => 1,
            Some(Whence::End) => 2,
            None => 3,
        };
        let sought = match call(
            req,
            self.object,
            b"stream_seek",
            &[Value::int(offset), Value::int(whence)],
        ) {
            Called::Missing => {
                self.no_seek = true;
                return Some(false);
            }
            Called::Threw => false,
            Called::Value(value) => {
                let sought = truthy(req, &value);
                req.release(value);
                sought
            }
        };
        if !sought {
            return Some(false);
        }
        match call(req, self.object, b"stream_tell", &[]) {
            Called::Value(value) => {
                let at = value.as_int();
                req.release(value);
                match at {
                    Some(at) => {
                        *position = at;
                        Some(true)
                    }
                    None => Some(false),
                }
            }
            Called::Missing => {
                not_implemented(req, self.object, "stream_tell");
                Some(false)
            }
            Called::Threw => Some(false),
        }
    }

    /// `php_userstreamop_cast` (`Z/main/streams/userspace.c:1398`):
    /// `stream_cast` with `STREAM_CAST_FOR_SELECT` or
    /// `STREAM_CAST_AS_STREAM`, and the stream it answers cast in this
    /// one's place; a class without it, an answer that is no stream and
    /// the stream itself are php's warnings where a `ret` was handed and
    /// silent refusals when none was, and a false answer is refused in
    /// silence either way.
    fn cast_target_held(
        &mut self,
        req: &mut Request,
        select: bool,
        own: Option<Stream>,
        ret: Ret,
    ) -> Option<Stream> {
        let report = ret == Ret::Given;
        let castas = if select { 3 } else { 0 };
        let value = match call(req, self.object, b"stream_cast", &[Value::int(castas)]) {
            Called::Missing => {
                if report {
                    not_implemented(req, self.object, "stream_cast");
                }
                return None;
            }
            Called::Threw => return None,
            Called::Value(value) => value,
        };
        if !truthy(req, &value) {
            req.release(value);
            return None;
        }
        let inner = Stream::from_value(req, &value.deref());
        req.release(value);
        let complaint = match inner {
            None => "must return a stream resource",
            Some(inner) if own.is_some_and(|own| own.resource() == inner.resource()) => {
                "must not return itself"
            }
            Some(inner) => return Some(inner),
        };
        if !report {
            return None;
        }
        let text = format!("{}::stream_cast {complaint}", class_name(req, self.object));
        docref(req, Level::Warning, &text);
        None
    }

    /// `user_stream_set_check_liveliness` (`Z/main/streams/userspace.c:952`):
    /// `stream_eof`, `true` the stream's end and `false` its life; a class
    /// without it, and an answer that is no bool, are php's warnings and
    /// the end.
    fn check_liveness(&mut self, req: &mut Request) -> OptionAnswer {
        let value = match call(req, self.object, b"stream_eof", &[]) {
            Called::Missing => {
                let text = format!(
                    "{}::stream_eof is not implemented! Assuming EOF",
                    class_name(req, self.object)
                );
                docref(req, Level::Warning, &text);
                return OptionAnswer::Err;
            }
            Called::Threw => return OptionAnswer::Err,
            Called::Value(value) => value,
        };
        match value.as_ref() {
            ValueRef::Bool(true) => OptionAnswer::Err,
            ValueRef::Bool(false) => OptionAnswer::Ok,
            _ => {
                let name = crate::api::coerce::value_name(req, &value);
                let text = format!(
                    "{}::stream_eof value must be of type bool, {name} given",
                    class_name(req, self.object)
                );
                docref(req, Level::Warning, &text);
                req.release(value);
                OptionAnswer::Err
            }
        }
    }
}

/// `statbuf_from_array` (`Z/main/streams/userspace.c:799`): each of the
/// thirteen fields read by its name, zero where it is not there.
fn statbuf_from_array(req: &mut Request, array: PhpArray) -> Result<StatBuf, Thrown> {
    const NAMES: [&[u8]; 13] = [
        b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
        b"ctime", b"blksize", b"blocks",
    ];
    let mut sb = [0; 13];
    for (field, name) in sb.iter_mut().zip(NAMES) {
        if let Some(value) = array.find(name) {
            *field = crate::api::coerce::to_long(req, &value.deref())?;
        }
    }
    Ok(sb)
}

impl Ops for UserStream {
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        self.held(req, |this, req| this.write_held(req, buf))
    }

    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        self.held(req, |this, req| this.read_held(req, buf, eof))
    }

    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        self.held(req, |this, req| {
            this.seek_held(req, offset, whence, position)
        })
    }

    fn cast_target(
        &mut self,
        req: &mut Request,
        cast: Cast,
        own: Option<Stream>,
        ret: Ret,
    ) -> CastTarget<'_> {
        let select = cast == Cast::Select;
        self.held(req, |this, req| {
            this.cast_target_held(req, select, own, ret)
        })
        .map_or(CastTarget::Refused, CastTarget::Stream)
    }

    fn adopt(&mut self, own: Stream) {
        self.own = Some(own);
    }

    fn label(&self) -> &'static str {
        "user-space"
    }

    /// `php_userstreamop_close` (`Z/main/streams/userspace.c:699`):
    /// `stream_close` where it is there, then the instance given up.
    fn close(&mut self, req: &mut Request) -> i32 {
        if let Called::Value(value) = call(req, self.object, b"stream_close", &[]) {
            req.release(value);
        }
        req.release(self.object.as_value());
        0
    }

    /// `php_userstreamop_flush`: `stream_flush`, `true` its answer.
    fn flush(&mut self, req: &mut Request) -> bool {
        match call(req, self.object, b"stream_flush", &[]) {
            Called::Value(value) => {
                let flushed = truthy(req, &value);
                req.release(value);
                flushed
            }
            Called::Missing | Called::Threw => false,
        }
    }

    fn seekable(&self) -> bool {
        true
    }

    fn wrapper_data(&self) -> Option<Value> {
        Some(self.object.as_value())
    }

    fn lost_seek(&self) -> bool {
        self.no_seek
    }

    /// `php_userstreamop_stat` (`Z/main/streams/userspace.c:843`):
    /// `stream_stat`, an array of it read as a stat buffer.
    fn stat(&mut self, req: &mut Request) -> Option<StatBuf> {
        let value = match call(req, self.object, b"stream_stat", &[]) {
            Called::Missing => {
                not_implemented(req, self.object, "stream_stat");
                return None;
            }
            Called::Threw => return None,
            Called::Value(value) => value,
        };
        let sb = match value.as_array() {
            Some(array) => statbuf_from_array(req, *array).ok(),
            None => None,
        };
        req.release(value);
        sb
    }

    /// `user_stream_set_truncation` (`Z/main/streams/userspace.c:935`):
    /// unsupported where the class has no `stream_truncate`; otherwise
    /// the size handed to it, whose bool is the answer and whose other
    /// types are php's warning.
    fn truncate(&mut self, req: &mut Request, size: usize) -> Option<bool> {
        let class = self.object.class();
        let has = req.method_named(class, b"stream_truncate").is_some()
            || req.method_named(class, b"__call").is_some();
        if !has {
            return None;
        }
        let size = i64::try_from(size).ok()?;
        match call(req, self.object, b"stream_truncate", &[Value::int(size)]) {
            Called::Missing => {
                not_implemented(req, self.object, "stream_truncate");
                Some(false)
            }
            Called::Threw => Some(false),
            Called::Value(value) => {
                if let ValueRef::Bool(b) = value.as_ref() {
                    return Some(b);
                }
                let name = crate::api::coerce::value_name(req, &value);
                let text = format!(
                    "{}::stream_truncate value must be of type bool, {name} given",
                    class_name(req, self.object)
                );
                docref(req, Level::Warning, &text);
                req.release(value);
                Some(false)
            }
        }
    }

    /// `user_stream_set_locking` (`Z/main/streams/userspace.c:891`):
    /// `stream_lock` with `flock(2)`'s operation as `PHP_LOCK_*`; `true`
    /// takes the `lock`, and any answer but a bool fails it.
    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        let mut lock = 0;
        if operation & 4 != 0 {
            lock |= 4;
        }
        lock |= match operation & !4 {
            1 => 1,
            2 => 2,
            8 => 3,
            _ => 0,
        };
        match call(req, self.object, b"stream_lock", &[Value::int(lock)]) {
            Called::Missing => {
                if operation == 0 {
                    return Some(true);
                }
                not_implemented(req, self.object, "stream_lock");
                Some(false)
            }
            Called::Threw => Some(false),
            Called::Value(value) => {
                let taken = matches!(value.as_ref(), ValueRef::Bool(true));
                req.release(value);
                Some(taken)
            }
        }
    }

    fn meta_data(&self) -> MetaData {
        MetaData::Generic
    }

    /// `php_userstreamop_set_option` (`Z/main/streams/userspace.c:1024`)
    /// for the four it hands on, `user_stream_set_option`:
    /// `stream_set_option` with the option's number and its two values,
    /// whether to block and `null`, the timeout's seconds and
    /// microseconds, or `PHP_STREAM_BUFFER_*` and the size; its truth is
    /// `PHP_STREAM_OPTION_RETURN_OK`, and a class without the method is
    /// php's warning and `_ERR`. The chunk size is not one it hands on.
    fn set_option(&mut self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        const BUFFER_NONE: i64 = 0;
        const BUFFER_FULL: i64 = 2;
        let mode = |buffered: bool| Value::int(if buffered { BUFFER_FULL } else { BUFFER_NONE });
        let args = match option {
            StreamOption::Blocking(blocking) => [
                Value::int(1),
                Value::int(i64::from(blocking)),
                Value::null(),
            ],
            StreamOption::ReadBuffer { buffered, size: n } => {
                [Value::int(2), mode(buffered), Value::int(n)]
            }
            StreamOption::WriteBuffer { buffered, size: n } => {
                [Value::int(3), mode(buffered), Value::int(n)]
            }
            StreamOption::ReadTimeout(timeout) => [
                Value::int(4),
                Value::int(i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX)),
                Value::int(i64::from(timeout.subsec_micros())),
            ],
            StreamOption::ChunkSize(_)
            | StreamOption::Sync { .. }
            | StreamOption::SuppressErrors(_) => {
                return OptionAnswer::NotImplemented;
            }
            StreamOption::Liveness => return self.check_liveness(req),
        };
        match call(req, self.object, b"stream_set_option", &args) {
            Called::Missing => {
                not_implemented(req, self.object, "stream_set_option");
                OptionAnswer::Err
            }
            Called::Threw => OptionAnswer::Err,
            Called::Value(value) => {
                let set = truthy(req, &value);
                req.release(value);
                if set {
                    OptionAnswer::Ok
                } else {
                    OptionAnswer::Err
                }
            }
        }
    }

    fn is_userspace(&self) -> bool {
        true
    }
}

/// `php_userstreamop_*dir` under `php_stream_userspace_dir_ops`: the
/// instance `dir_opendir` was called on.
struct UserDir {
    object: PhpObject,
}

impl Ops for UserDir {
    fn label(&self) -> &'static str {
        "user-space-dir"
    }

    /// `php_stream_userspace_dir_ops` has no `write`.
    fn writable(&self) -> bool {
        false
    }

    fn write(&mut self, _req: &mut Request, _buf: &[u8]) -> Option<usize> {
        None
    }

    fn read(&mut self, _req: &mut Request, _buf: &mut [u8], _eof: &mut bool) -> Option<usize> {
        None
    }

    /// `php_userstreamop_closedir`: `dir_closedir`, then the instance
    /// given up.
    fn close(&mut self, req: &mut Request) -> i32 {
        if let Called::Value(value) = call(req, self.object, b"dir_closedir", &[]) {
            req.release(value);
        }
        req.release(self.object.as_value());
        0
    }

    /// `php_userstreamop_rewinddir`: `dir_rewinddir`, and `0` whatever it
    /// answered.
    fn seek(
        &mut self,
        req: &mut Request,
        _offset: i64,
        _whence: Option<Whence>,
        _position: &mut i64,
    ) -> Option<bool> {
        if let Called::Value(value) = call(req, self.object, b"dir_rewinddir", &[]) {
            req.release(value);
        }
        Some(true)
    }

    fn seekable(&self) -> bool {
        true
    }

    fn wrapper_data(&self) -> Option<Value> {
        Some(self.object.as_value())
    }

    /// `php_userstreamop_readdir` (`Z/main/streams/userspace.c:1344`):
    /// `dir_readdir`, a string of it the entry, a bool the end.
    fn read_dir(&mut self, req: &mut Request) -> Option<Vec<u8>> {
        let value = match call(req, self.object, b"dir_readdir", &[]) {
            Called::Missing => {
                not_implemented(req, self.object, "dir_readdir");
                return None;
            }
            Called::Threw => return None,
            Called::Value(value) => value,
        };
        if matches!(value.as_ref(), ValueRef::Bool(_)) {
            return None;
        }
        let name = match crate::api::coerce::to_string(req, &value) {
            Ok(text) => {
                let bytes = text.as_bytes().to_vec();
                text.give_up(req);
                Some(bytes)
            }
            Err(_) => None,
        };
        req.release(value);
        name
    }
}

/// One path operation: the instance made, the method called with the
/// arguments, and its answer; `None` where the class cannot be
/// instantiated or the method raised.
fn path_call(
    req: &mut Request,
    user: &UserWrapper,
    method: &[u8],
    args: &[Value],
) -> Result<Option<Called>, Thrown> {
    let Some(object) = create_object(req, user)? else {
        return Ok(None);
    };
    let called = call(req, object, method, args);
    if let Called::Missing = called {
        let text = format!(
            "{}::{} is not implemented!",
            class_name(req, object),
            String::from_utf8_lossy(method)
        );
        docref(req, Level::Warning, &text);
    }
    req.release(object.as_value());
    Ok(Some(called))
}

/// A path operation's bool: `true` only for a `true` answer.
fn bool_answer(req: &mut Request, called: Option<&Called>) -> Result<bool, Thrown> {
    let answer = match called {
        Some(Called::Value(value)) => {
            let yes = matches!(value.as_ref(), ValueRef::Bool(true));
            req.release(*value);
            yes
        }
        _ => false,
    };
    match req.stream_thrown().take() {
        Some(thrown) => Err(thrown),
        None => Ok(answer),
    }
}

/// `user_wrapper_stat_url` (`Z/main/streams/userspace.c:1301`): `url_stat`
/// with the path and the bits, `PHP_STREAM_URL_STAT_LINK`,
/// `PHP_STREAM_URL_STAT_QUIET` and `PHP_STREAM_URL_STAT_IGNORE_OPEN_BASEDIR`
/// as the caller set them, an array of it read as a stat buffer.
pub(crate) fn url_stat(
    req: &mut Request,
    user: &UserWrapper,
    url: &[u8],
    flags: i64,
) -> Result<Option<StatBuf>, Thrown> {
    let args = [string_arg(req, url)?, Value::int(flags)];
    let called = path_call(req, user, b"url_stat", &args)?;
    for arg in args {
        req.release(arg);
    }
    let sb = match called {
        Some(Called::Value(value)) => {
            let sb = match value.as_array() {
                Some(array) => Some(statbuf_from_array(req, *array)?),
                None => None,
            };
            req.release(value);
            sb
        }
        _ => None,
    };
    match req.stream_thrown().take() {
        Some(thrown) => Err(thrown),
        None => Ok(sb),
    }
}

/// `user_wrapper_unlink`, `user_wrapper_rename`, `user_wrapper_mkdir`,
/// `user_wrapper_rmdir` and `user_wrapper_metadata`
/// (`Z/main/streams/userspace.c:1037-1299`): the method with php's
/// arguments, `true` only for a `true` answer. `mkdir` hands its options
/// as `STREAM_MKDIR_RECURSIVE | REPORT_ERRORS`, `rmdir` as
/// `REPORT_ERRORS`, and `stream_metadata` its option with the times as
/// an array, the id or the permission bits.
pub(crate) fn path_op(
    req: &mut Request,
    user: &UserWrapper,
    url: &[u8],
    op: WrapperOp<'_>,
) -> Result<bool, Thrown> {
    let url_arg = string_arg(req, url)?;
    let (method, args): (&[u8], Vec<Value>) = match op {
        WrapperOp::Unlink => (b"unlink", vec![url_arg]),
        WrapperOp::Rename { to } => (b"rename", vec![url_arg, string_arg(req, to)?]),
        WrapperOp::Mkdir { mode, recursive } => (
            b"mkdir",
            vec![
                url_arg,
                Value::int(mode),
                Value::int(i64::from(recursive) | REPORT_ERRORS),
            ],
        ),
        WrapperOp::Rmdir => (b"rmdir", vec![url_arg, Value::int(REPORT_ERRORS)]),
        WrapperOp::Touch(times) => {
            let mut array = PhpArray::new(req)?.hand_on_unmigrated();
            if let Some((mtime, atime)) = times {
                array.append(req, Owner::take(req, Value::int(mtime)))?;
                array.append(req, Owner::take(req, Value::int(atime)))?;
            }
            (
                b"stream_metadata",
                vec![url_arg, Value::int(1), Value::array(array)],
            )
        }
        WrapperOp::Owner(uid) => (
            b"stream_metadata",
            vec![url_arg, Value::int(3), Value::int(uid)],
        ),
        WrapperOp::Group(gid) => (
            b"stream_metadata",
            vec![url_arg, Value::int(5), Value::int(gid)],
        ),
        WrapperOp::OwnerName(name) => (
            b"stream_metadata",
            vec![url_arg, Value::int(2), string_arg(req, name)?],
        ),
        WrapperOp::GroupName(name) => (
            b"stream_metadata",
            vec![url_arg, Value::int(4), string_arg(req, name)?],
        ),
        WrapperOp::Chmod(mode) => (
            b"stream_metadata",
            vec![url_arg, Value::int(6), Value::int(mode)],
        ),
    };
    let called = path_call(req, user, method, &args)?;
    for arg in args {
        req.release(arg);
    }
    bool_answer(req, called.as_ref())
}
