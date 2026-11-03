//! `intl_error.c`: the error state php keeps twice, once for the request
//! (`INTL_G(g_error)`, what `intl_get_error_code` and
//! `intl_get_error_message` answer) and once on each object (the
//! `intl_error` every `*_object` carries first, what its
//! `getErrorCode` and `getErrorMessage` answer). A message is prefixed
//! with the native that set it, `get_active_function_or_method_name`;
//! the request's is raised at `intl.error_level` and thrown as
//! `IntlException` under `intl.use_exceptions` when it is set.

use std::any::Any;
use std::fmt::Write as _;

use php_ext::abi::OrFalse;
use php_ext::call::new_object;
use php_ext::raising::{Level, Thrown, docref, throw_value, while_pending};
use php_ext::request::{Ini, Request};
use php_ext::values::{Owner, PhpObject, PhpString};

use crate::State;
use crate::ffi::Code;

/// `intl_error`: the last code and the message a native wrote for it,
/// sixteen bytes as php's, the message the `zend_string` php keeps on
/// the request's heap, so `memory_get_usage()` counts it where php's
/// does, from the native that set it until the next one replaces it,
/// a reset frees it, or its holder goes: the object's `free_obj`
/// ([`free`]), the request's `RSHUTDOWN`.
#[derive(Debug, Default)]
pub(crate) struct IntlError {
    pub(crate) code: Code,
    message: Option<Owner<PhpString>>,
}

/// A message an `intl_error` let go of, freed once the error is no longer
/// borrowed, `zend_string_release_ex`.
#[must_use = "the message is freed with Message::free"]
pub(crate) struct Message(Option<Owner<PhpString>>);

impl Message {
    /// `zend_string_release_ex(msg, false)`.
    pub(crate) fn free(self, req: &mut Request) {
        if let Some(message) = self.0 {
            message.give_up(req);
        }
    }
}

impl IntlError {
    /// `intl_error_reset`: the code zero and the message let go of.
    pub(crate) fn reset(&mut self) -> Message {
        self.code = Code::ZERO;
        Message(self.message.take())
    }

    /// `intl_error_set_custom_msg`'s last two steps: the message the
    /// error held let go of, and `text` its message.
    fn keep(&mut self, text: Owner<PhpString>) -> Message {
        Message(self.message.replace(text))
    }

    /// `intl_error_get_message`: `<message>: <code name>`, or the code's
    /// name alone.
    pub(crate) fn message(&self) -> Vec<u8> {
        let name = self.code.name().as_bytes();
        match &self.message {
            Some(message) => [message.as_bytes(), b": ", name].concat(),
            None => name.to_vec(),
        }
    }
}

/// An object's payload that carries php's `intl_error`, the first
/// member of each `*_object`.
pub(crate) trait Carrier: Any + Send {
    fn error(&mut self) -> &mut IntlError;
}

/// `INTL_G(g_error)`.
fn global(req: &mut Request) -> &mut IntlError {
    &mut req.state::<State>().error
}

/// The string `intl_error_set_custom_msg` keeps: `<native>(): <message>`,
/// `zend_string_concat3` over the name `get_active_function_or_method_name`
/// answers, which `zend_create_member_string` lays for a method and
/// `ZSTR_INIT_LITERAL` for `main` and which is given up after, and the
/// message alone, `zend_string_init`, while nothing executes
/// (`zend_is_executing`). The `Err` is the memory limit's fatal.
fn prefixed(req: &mut Request, message: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    if !req.frames().executing() {
        return PhpString::filled(req, message.len(), |b| b.copy_from_slice(message));
    }
    let name = req.frames().active_function_name();
    let name = name.as_bytes();
    let laid = if name == b"main" || name.windows(2).any(|w| w == b"::") {
        Some(PhpString::filled(req, name.len(), |b| {
            b.copy_from_slice(name);
        })?)
    } else {
        None
    };
    let text = PhpString::filled(req, name.len() + 4 + message.len(), |b| {
        let (head, rest) = b.split_at_mut(name.len());
        head.copy_from_slice(name);
        let (sep, tail) = rest.split_at_mut(4);
        sep.copy_from_slice(b"(): ");
        tail.copy_from_slice(message);
    });
    if let Some(laid) = laid {
        laid.give_up(req);
    }
    text
}

/// `intl_error_reset(err)` over the error `at` reaches, the message
/// freed once it is let go of.
fn reset_at(req: &mut Request, at: impl FnOnce(&mut Request) -> Option<&mut IntlError>) {
    if let Some(message) = at(req).map(IntlError::reset) {
        message.free(req);
    }
}

/// `intl_error_set_custom_msg(err, msg)` with `err` the error `at`
/// reaches: the string made, then the error's last message freed and
/// the string kept; nothing made where `at` reaches none.
fn keep_at(
    req: &mut Request,
    message: &[u8],
    at: impl Fn(&mut Request) -> Option<&mut IntlError>,
) -> Result<(), Thrown> {
    if at(req).is_none() {
        return Ok(());
    }
    let text = prefixed(req, message)?;
    match at(req) {
        Some(error) => error.keep(text).free(req),
        None => text.give_up(req),
    }
    Ok(())
}

/// `free_obj`'s `intl_error_reset(INTL_DATA_ERROR_P(obj))`: the object's
/// message freed, where php's handler frees it before or after
/// `zend_object_std_dtor` as each class's does. A clone starts with no
/// message, as its `create_object`'s `intl_error_init` leaves it, so it
/// frees only itself:
///
/// ```php
/// $f = new NumberFormatter('en', NumberFormatter::DECIMAL);
/// $f->parse('x');
/// $m = memory_get_usage();
/// $g = clone $f;
/// echo memory_get_usage() - $m, ' ', $g->getErrorMessage(), "\n";
/// $m = memory_get_usage(); unset($g); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($f); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 64 U_ZERO_ERROR
/// -64
/// -144
/// ```
pub(crate) fn free<P: Carrier>(req: &mut Request, object: PhpObject) {
    reset_at(req, |req| carried::<P>(req, object));
}

/// `intl_error_reset(NULL)`. A native whose php counterpart resets the
/// request's error ahead of its ZPP (`INTL_METHOD_INIT_VARS` and the
/// area macros built on it) names this as its `before_args`, so an
/// argument's `TypeError` or `ArgumentCountError` leaves no earlier
/// failure standing; `IntlListFormatter` and the `grapheme_*` functions
/// reset nothing there, and the failure stands.
///
/// ```php
/// $cal = IntlCalendar::createInstance('UTC'); $col = new Collator('en'); $bi = IntlBreakIterator::createWordInstance('en');
/// $nf = new NumberFormatter('en', NumberFormatter::DECIMAL); $tz = IntlTimeZone::createTimeZone('UTC'); $df = new IntlDateFormatter('en');
/// $lf = new IntlListFormatter('en'); $pg = new IntlDatePatternGenerator('en'); $x = $y = 0;
/// $calls = [
///     'calendar' => fn() => $cal->get([]), 'collator' => fn() => $col->compare([], 'a'),
///     'breakiterator' => fn() => $bi->following([]), 'formatter' => fn() => $nf->format([]),
///     'timezone' => fn() => $tz->getOffset([], true, $x, $y), 'normalizer' => fn() => normalizer_normalize([]),
///     'locale' => fn() => locale_get_display_name([]), 'transliterator' => fn() => transliterator_transliterate([], 'a'),
///     'dateformat' => fn() => $df->format([]), 'listformatter' => fn() => $lf->format(1),
///     'datepatterngenerator' => fn() => $pg->getBestPattern([]), 'idn' => fn() => idn_to_ascii([]),
///     'grapheme' => fn() => grapheme_strlen([]), 'iterator' => fn() => $bi->getPartsIterator()->key(1),
/// ];
/// foreach ($calls as $area => $f) {
///     IntlChar::isalpha(0x110000);
///     try { $f(); } catch (TypeError|ArgumentCountError $e) {}
///     echo $area, ' ', intl_get_error_code(), "\n";
/// }
/// ```
/// ```expect
/// calendar 0
/// collator 0
/// breakiterator 0
/// formatter 0
/// timezone 0
/// normalizer 0
/// locale 0
/// transliterator 0
/// dateformat 0
/// listformatter 1
/// datepatterngenerator 0
/// idn 0
/// grapheme 1
/// iterator 0
/// ```
pub(crate) fn reset(req: &mut Request) {
    reset_at(req, |req| Some(global(req)));
}

/// `intl_error_set_code(NULL, code)`.
pub(crate) fn set_code(req: &mut Request, code: Code) {
    global(req).code = code;
}

/// `intl_error_get_code(NULL)`.
pub(crate) fn code(req: &mut Request) -> Code {
    global(req).code
}

/// `INTL_G(error_level)`: `intl.error_level` as a quantity.
fn error_level(req: &mut Request) -> Option<Level> {
    if req.state::<State>().constructing {
        return None;
    }
    req.ini().get(b"intl.error_level")?;
    let bits = req.ini().long(b"intl.error_level");
    [
        Level::Error,
        Level::Warning,
        Level::Parse,
        Level::Notice,
        Level::CoreError,
        Level::CoreWarning,
        Level::CompileError,
        Level::CompileWarning,
        Level::UserError,
        Level::UserWarning,
        Level::UserNotice,
        Level::Strict,
        Level::RecoverableError,
        Level::Deprecated,
        Level::UserDeprecated,
    ]
    .into_iter()
    .find(|level| level.bit() == bits)
}

/// `INTL_G(use_exceptions)`.
pub(crate) fn use_exceptions(req: &mut Request) -> bool {
    if req.state::<State>().constructing {
        return true;
    }
    req.ini()
        .get(b"intl.use_exceptions")
        .is_some_and(Ini::parse_bool)
}

/// A constructor's body, as php's `__construct`s wrap theirs: errors
/// thrown as `IntlException` and raised at no level while it runs,
/// whatever the ini entries say, and the previous setting restored.
pub(crate) fn constructing<T>(req: &mut Request, f: impl FnOnce(&mut Request) -> T) -> T {
    let old = std::mem::replace(&mut req.state::<State>().constructing, true);
    let answer = f(req);
    req.state::<State>().constructing = old;
    answer
}

/// `intl_error_set_custom_msg(NULL, msg)`: raised at `intl.error_level`,
/// thrown as `IntlException` under `intl.use_exceptions`, and kept as
/// the request's message. The warning prints the message's bytes, as
/// php's `php_error_docref(NULL, level, "%s", msg)` does, a key that is
/// not UTF-8 included:
///
/// ```php
/// ini_set('intl.error_level', E_WARNING);
/// set_error_handler(function ($n, $m) { echo bin2hex($m), "\n"; return true; });
/// (new MessageFormatter('en', '{0}'))->format(["\xffk" => 1]);
/// ```
/// ```expect
/// Deprecated: ini_set(): Using a value different than 0 for intl.error_level is deprecated, as the intl.error_level INI setting is deprecated. Instead the intl.use_exceptions INI setting should be enabled to throw exceptions on errors or intl_get_error_code()/intl_get_error_message() should be used to manually deal with errors in %s on line 2
/// 4d657373616765466f726d61747465723a3a666f726d617428293a20496e76616c6964205554462d38206461746120696e20617267756d656e74206b65793a2027ff6b27
/// ```
pub(crate) fn set_custom_msg(req: &mut Request, message: impl AsRef<[u8]>) -> Result<(), Thrown> {
    custom_msg(req, None, message.as_ref())
}

/// [`set_custom_msg`] where `pending` may already be thrown, as php's C
/// goes on past a throw: the `IntlException` chained to it, and it
/// handed on where nothing more is thrown. What an error handler throws
/// from the level's warning is pending from there on, so the
/// `IntlException` chains over it as `zend_throw_exception_internal`
/// chains over `EG(exception)`:
///
/// ```php
/// ini_set('intl.error_level', E_WARNING);
/// ini_set('intl.use_exceptions', 1);
/// set_error_handler(function ($n, $m) { throw new RuntimeException($m); });
/// try { IntlChar::isalpha(0x110000); } catch (Throwable $e) { for (; $e; $e = $e->getPrevious()) echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ini_set('intl.use_exceptions', 0);
/// try { IntlChar::isalpha(0x110000); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: ini_set(): Using a value different than 0 for intl.error_level is deprecated, as the intl.error_level INI setting is deprecated. Instead the intl.use_exceptions INI setting should be enabled to throw exceptions on errors or intl_get_error_code()/intl_get_error_message() should be used to manually deal with errors in %s on line 2
/// IntlException: IntlChar::isalpha(): Codepoint out of range
/// RuntimeException: IntlChar::isalpha(): Codepoint out of range
/// RuntimeException: IntlChar::isalpha(): Codepoint out of range
/// ```
///
/// The later errors of a call that goes on past a throw hand no handler
/// their warning, and chain over what the first one's handler threw:
///
/// ```php
/// ini_set('intl.error_level', E_WARNING);
/// ini_set('intl.use_exceptions', 1);
/// set_error_handler(function ($n, $m) { echo "handler: $m\n"; throw new RuntimeException($m); });
/// $f = new IntlDateFormatter('en_US', IntlDateFormatter::SHORT, IntlDateFormatter::SHORT, 'UTC');
/// try { $f->format(['tm_year' => 'x', 'tm_mon' => 1.5]); } catch (Throwable $e) {
///     for (; $e; $e = $e->getPrevious()) echo get_class($e), ': ', $e->getMessage(), "\n";
/// }
/// ```
/// ```expect
/// Deprecated: ini_set(): Using a value different than 0 for intl.error_level is deprecated, as the intl.error_level INI setting is deprecated. Instead the intl.use_exceptions INI setting should be enabled to throw exceptions on errors or intl_get_error_code()/intl_get_error_message() should be used to manually deal with errors in %s on line 2
/// handler: IntlDateFormatter::format(): parameter array contains a non-integer element for key 'tm_year'
/// IntlException: IntlDateFormatter::format(): date formatting failed
/// IntlException: IntlDateFormatter::format(): error cloning calendar
/// IntlException: IntlDateFormatter::format(): parameter array contains a non-integer element for key 'tm_year'
/// RuntimeException: IntlDateFormatter::format(): parameter array contains a non-integer element for key 'tm_year'
/// ```
fn custom_msg(req: &mut Request, pending: Option<Thrown>, message: &[u8]) -> Result<(), Thrown> {
    let text = match prefixed(req, message) {
        Ok(text) => text,
        Err(fatal) => return Err(over(req, pending, fatal)),
    };
    let pending = match error_level(req) {
        Some(level) => raise(req, pending, level, message),
        None => pending,
    };
    let thrown = if use_exceptions(req) {
        Some(intl_exception(req, pending, *text))
    } else {
        pending
    };
    global(req).keep(text).free(req);
    thrown.map_or(Ok(()), Err)
}

/// `later` raised while `pending` stood, chained over it as
/// [`Thrown::chain`] chains it, or alone where nothing stood.
fn over(req: &mut Request, pending: Option<Thrown>, later: Thrown) -> Thrown {
    match pending {
        Some(pending) => pending.chain(req, later),
        None => later,
    }
}

/// `php_error_docref(NULL, level, "%s", msg)`: the warning raised, and
/// what an error handler threw from it pending from there on. Where a
/// throwable is pending already, php's `zend_call_function` hands no
/// handler the warning, and it is raised while that one stands.
fn raise(
    req: &mut Request,
    pending: Option<Thrown>,
    level: Level,
    message: &[u8],
) -> Option<Thrown> {
    if let Some(pending) = pending {
        while_pending(req, &pending, |req| docref(req, level, message));
        return Some(pending);
    }
    docref(req, level, message);
    php_ext::raising::pending(req).err()
}

/// `intl_error_set_custom_msg`'s throw, `zend_throw_error_exception`
/// over the string it made (`zend_string_concat3`): an `IntlException`
/// made, the string its message, then thrown, chained over what is
/// pending as `zend_throw_exception_internal` chains it. The throwable
/// shares the string the request keeps as its message, one allocation
/// for both, as php's `ZVAL_STR` hands the property its `zend_string`;
/// so a message's bytes that are not UTF-8 are the throwable's as they
/// are, and the request's next message frees nothing while the
/// throwable holds the last one:
///
/// ```php
/// ini_set('intl.use_exceptions', 1);
/// try { IntlChar::isalpha(0x110000); } catch (IntlException $e) {}
/// ini_set('intl.use_exceptions', 0);
/// $m = memory_get_usage();
/// IntlChar::isalpha(0x110001);
/// echo memory_get_usage() - $m, "\n";
/// ini_set('intl.use_exceptions', 1);
/// try { (new MessageFormatter('en', '{0}'))->format(["\xffk" => 1]); } catch (IntlException $e) { echo bin2hex($e->getMessage()), "\n"; }
/// ```
/// ```expect
/// 80
/// 4d657373616765466f726d61747465723a3a666f726d617428293a20496e76616c6964205554462d38206461746120696e20617267756d656e74206b65793a2027ff6b27
/// ```
fn intl_exception(req: &mut Request, pending: Option<Thrown>, text: PhpString) -> Thrown {
    let ce = match crate::throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return over(req, pending, fatal),
    };
    let object = match new_object(req, ce.intl_exception) {
        Ok(object) => object,
        Err(refused) => return over(req, pending, refused),
    };
    let view = *object;
    let written = PhpString::interned(req, b"message").and_then(|name| {
        let (name, message) = (Owner::take(req, name), Owner::take(req, text));
        view.set_prop(req, name, message.into_value())
    });
    if let Err(refused) = written {
        object.give_up(req);
        return over(req, pending, refused);
    }
    let value = view.as_value();
    let thrown = match pending {
        None => throw_value(req, &value),
        Some(pending) => match while_pending(req, &pending, |req| throw_value(req, &value)) {
            Some(later) => pending.chain(req, later),
            None => pending,
        },
    };
    object.give_up(req);
    thrown
}

/// `intl_error_set(NULL, code, msg)`.
pub(crate) fn set(req: &mut Request, code: Code, message: impl AsRef<[u8]>) -> Result<(), Thrown> {
    set_code(req, code);
    set_custom_msg(req, message)
}

/// `intl_error_get_message(NULL)`.
pub(crate) fn message(req: &mut Request) -> Vec<u8> {
    global(req).message()
}

/// The object's `intl_error`, or `None` for an object of another class.
fn carried<P: Carrier>(req: &mut Request, object: PhpObject) -> Option<&mut IntlError> {
    object.native_mut::<P>(req).map(Carrier::error)
}

/// `intl_error_reset(obj)`, as `INTL_METHOD_FETCH_OBJECT` resets it.
pub(crate) fn reset_object<P: Carrier>(req: &mut Request, object: PhpObject) {
    reset_at(req, |req| carried::<P>(req, object));
}

/// `intl_errors_reset(obj)`: the object's and the request's.
pub(crate) fn errors_reset<P: Carrier>(req: &mut Request, object: PhpObject) {
    reset_object::<P>(req, object);
    reset(req);
}

/// `intl_errors_reset(obj)` over the method's receiver, for a native that
/// resets both before its arguments are read (`before_args`), as
/// `IntlListFormatter::__construct` does.
pub(crate) fn errors_reset_this<P: Carrier>(req: &mut Request) {
    match req.frames().this() {
        Some(object) => errors_reset::<P>(req, object),
        None => reset(req),
    }
}

/// `intl_errors_set_code(obj, code)`: the object's and the request's.
pub(crate) fn errors_set_code<P: Carrier>(req: &mut Request, object: PhpObject, code: Code) {
    if let Some(error) = carried::<P>(req, object) {
        error.code = code;
    }
    set_code(req, code);
}

/// `intl_errors_set_custom_msg(obj, msg)`: the object's message, then
/// the request's, which raises. Each is a string of the request's heap,
/// so a warning handler counts both, the object's and the request's,
/// with the request's last one freed only after the handler returns;
/// the object's is freed with the object. A `UConverter` whose encoding
/// fails while it sizes what it writes is the witness, its UTF-16 copy held
/// across the warning as php holds it:
///
/// ```php
/// @ini_set('intl.error_level', E_WARNING);
/// class F extends UConverter {
///     function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
///         if ($reason !== self::REASON_UNASSIGNED) return null;
///         $error = U_INVALID_CHAR_FOUND;
///         return '?';
///     }
/// }
/// set_error_handler(function () use (&$u, &$p) { $u = memory_get_usage(); $p = memory_get_peak_usage(); return true; });
/// foreach ([10, 100000, 100000] as $n) {
///     $c = new F('ASCII', 'UTF-8');
///     $s = str_repeat('a', $n) . "\u{100}";
///     memory_reset_peak_usage(); $m = memory_get_usage();
///     $c->convert($s);
///     echo $u - $m, ' ', $p - $m, ' ', memory_get_usage() - $m;
///     $m = memory_get_usage(); unset($c); echo ' ', memory_get_usage() - $m, "\n";
/// }
/// ```
/// ```expect
/// 320 1168 224 -256
/// 200992 201840 192 -256
/// 200992 201840 192 -256
/// ```
pub(crate) fn errors_set_custom_msg<P: Carrier>(
    req: &mut Request,
    object: PhpObject,
    message: impl AsRef<[u8]>,
) -> Result<(), Thrown> {
    let message = message.as_ref();
    keep_at(req, message, |req| carried::<P>(req, object))?;
    set_custom_msg(req, message)
}

/// `intl_errors_set(obj, code, msg)`.
pub(crate) fn errors_set<P: Carrier>(
    req: &mut Request,
    object: PhpObject,
    code: Code,
    message: &str,
) -> Result<(), Thrown> {
    errors_set_code::<P>(req, object, code);
    errors_set_custom_msg::<P>(req, object, message)
}

/// `intl_error_get_code(obj)`.
pub(crate) fn object_code<P: Carrier>(req: &mut Request, object: PhpObject) -> Code {
    carried::<P>(req, object).map_or(Code::ZERO, |error| error.code)
}

/// `intl_error_get_message(obj)`.
pub(crate) fn object_message<P: Carrier>(req: &mut Request, object: PhpObject) -> Vec<u8> {
    carried::<P>(req, object).map_or_else(|| Code::ZERO.name().as_bytes().to_vec(), |e| e.message())
}

/// `INTL_METHOD_CHECK_STATUS(obj, msg)`: the object's code copied to
/// the request; where it is a failure, the message set on both and
/// `Err(None)`, which the native answers `false` (or null) for.
pub(crate) fn check_object<P: Carrier>(
    req: &mut Request,
    object: PhpObject,
    message: &str,
) -> Result<(), Option<Thrown>> {
    let code = object_code::<P>(req, object);
    set_code(req, code);
    if code.is_failure() {
        return Err(errors_set_custom_msg::<P>(req, object, message).err());
    }
    Ok(())
}

/// `INTL_CHECK_STATUS(err, msg)`: the code set on the request; where it
/// is a failure, the message too and `Err(None)`.
pub(crate) fn check(req: &mut Request, code: Code, message: &str) -> Result<(), Option<Thrown>> {
    set_code(req, code);
    if code.is_failure() {
        return Err(set_custom_msg(req, message).err());
    }
    Ok(())
}

/// `intl_parse_error_to_string`: where a pattern parse stopped, as php
/// words it.
pub(crate) fn parse_error_text(line: i32, offset: i32, pre: &[u8], post: &[u8]) -> String {
    let mut out = String::from("parse error ");
    let mut any = false;
    if line > 0 {
        write!(out, "on line {line}").ok();
        any = true;
    }
    if offset >= 0 {
        out.push_str(if any { ", " } else { "at " });
        write!(out, "offset {offset}").ok();
        any = true;
    }
    if !pre.is_empty() {
        if any {
            out.push_str(", ");
        }
        write!(out, "after \"{}\"", String::from_utf8_lossy(pre)).ok();
        any = true;
    }
    if !post.is_empty() {
        if any {
            out.push_str(", ");
        }
        write!(out, "before or at \"{}\"", String::from_utf8_lossy(post)).ok();
        any = true;
    }
    if !any {
        return "no parse error".to_owned();
    }
    out
}

/// `intl_parse_error_to_string` over ICU's `UParseError`, its contexts
/// read up to their NUL.
pub(crate) fn parse_error(pe: &crate::ffi::ParseError) -> String {
    let context = |c: &[u16; 16]| {
        let end = c.iter().position(|&u| u == 0).unwrap_or(c.len());
        crate::ffi::to_utf8(&c[..end]).unwrap_or_default()
    };
    parse_error_text(pe.line, pe.offset, &context(&pe.pre), &context(&pe.post))
}

/// The `intl_error *outside_error` php hands a function that may fail on
/// its caller's behalf (`common_date.cpp`, `timezone_class.cpp`): none,
/// so the request's error alone is set; a local one; or an object's.
pub(crate) enum Outside<'a> {
    Request,
    Local(&'a mut IntlError),
    Object(
        PhpObject,
        fn(&mut Request, PhpObject) -> Option<&mut IntlError>,
    ),
}

impl Outside<'_> {
    /// The error an object of payload `P` carries.
    pub(crate) fn of<P: Carrier>(object: PhpObject) -> Outside<'static> {
        Outside::Object(object, carried::<P>)
    }

    /// `intl_errors_set(err, code, msg)`: the outside error's code and
    /// message, then the request's, which raises.
    pub(crate) fn set(
        &mut self,
        req: &mut Request,
        code: Code,
        message: impl AsRef<[u8]>,
    ) -> Result<(), Thrown> {
        self.keep(req, code, message.as_ref())?;
        set(req, code, message)
    }

    /// [`Outside::set`] with what may already be thrown, as php's C runs
    /// on past a throw: the `IntlException` chained to it, or it handed
    /// on; `Ok` only where nothing was pending and nothing thrown.
    pub(crate) fn set_over(
        &mut self,
        req: &mut Request,
        pending: Option<Thrown>,
        code: Code,
        message: impl AsRef<[u8]>,
    ) -> Result<(), Thrown> {
        if let Err(fatal) = self.keep(req, code, message.as_ref()) {
            return Err(over(req, pending, fatal));
        }
        set_code(req, code);
        custom_msg(req, pending, message.as_ref())
    }

    /// The outside error's code and message, `intl_error_set_code` and
    /// `intl_error_set_custom_msg`; the `Err` is the memory limit's fatal.
    fn keep(&mut self, req: &mut Request, code: Code, message: &[u8]) -> Result<(), Thrown> {
        match self {
            Outside::Request => Ok(()),
            Outside::Local(error) => {
                error.code = code;
                let text = prefixed(req, message)?;
                error.keep(text).free(req);
                Ok(())
            }
            Outside::Object(object, carried) => {
                let (object, carried) = (*object, *carried);
                if let Some(error) = carried(req, object) {
                    error.code = code;
                }
                keep_at(req, message, |req| carried(req, object))
            }
        }
    }

    /// `intl_error_get_code(err)`: the outside error's, zero for none.
    pub(crate) fn code(&mut self, req: &mut Request) -> Code {
        match self {
            Outside::Request => Code::ZERO,
            Outside::Local(error) => error.code,
            Outside::Object(object, carried) => {
                carried(req, *object).map_or(Code::ZERO, |e| e.code)
            }
        }
    }
}

/// What a native answers for a check that failed: `false` for
/// `Err(None)`, where php's `INTL_CHECK_STATUS` and its kin
/// `RETURN_FALSE`, and the throwable for `Err(Some(..))`.
pub(crate) fn or_false<T>(answer: Result<T, Option<Thrown>>) -> Result<OrFalse<T>, Thrown> {
    match answer {
        Ok(value) => Ok(OrFalse::Value(value)),
        Err(None) => Ok(OrFalse::False),
        Err(Some(thrown)) => Err(thrown),
    }
}
