//! `msgformat/`: `MessageFormatter` and the `msgfmt_*` functions over
//! ICU's `umsg_*`, with `msgformat_helpers.cpp`'s typing of the
//! arguments over `icu::MessageFormat` through `shim/msgformat.cpp`.
//! Each method php aliases to a function shares one body here with it.
//!
//! A format runs PHP code between reading the formatter and formatting
//! with it (an argument's `__toString`, a `DateTime`'s `getTimestamp`),
//! so no borrow of the formatter is held across a conversion: the
//! argument types are copied out first and the formatter is read again
//! for the call.

use std::collections::HashMap;
use std::ops::ControlFlow;

use php_ext::abi::{CloneObj, Instance, ObjectHandlers, OrFalse, Raw, This, Union, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{clone_with, lookup_class, new_object};
use php_ext::coerce::{to_double, to_long, to_string};
use php_ext::raising::{Carrying, Thrown, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Layout, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::common_date::{process_timezone_argument, zval_to_millis};
use crate::error::{self, Carrier, IntlError, Outside};
use crate::ffi::msgformat::{self as umsg, ArgKind, ArgType, Formattable, MessageFormat, PartKind};
use crate::ffi::{self, Code, ParseError, c_string};
use crate::formatter::locale_too_long;

/// `U_INVALID_FORMAT_ERROR`.
const INVALID_FORMAT: Code = Code(3);
/// `U_PARSE_ERROR`.
const PARSE: Code = Code(9);
/// `U_PATTERN_SYNTAX_ERROR`.
const PATTERN_SYNTAX: Code = Code(65799);
/// `U_ARGUMENT_TYPE_MISMATCH`.
const ARGUMENT_TYPE_MISMATCH: Code = Code(65804);

/// A native's answer before `false` is chosen: `None` is `false`, a
/// throw rides beside.
type Answer<T> = Result<Option<T>, Thrown>;

/// `string|false` as declared, which `INTL_CHECK_LOCALE_LEN` answers
/// `null` outside of, as php's internal returns go unverified.
type OrNull<T> = Union<OrFalse<T>, Untyped<()>>;

/// The key an argument's type is kept under: its number, or its name in
/// UTF-16, as php keys `arg_types` by index or by the name's bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ArgKey {
    Number(i64),
    Name(Vec<u16>),
}

/// `arg_types`: what each argument the pattern names is formatted as.
type ArgTypes = HashMap<ArgKey, ArgType>;

/// `umsgf` and `arg_types`: the format and the argument types php
/// caches on it, boxed as php's `HashTable *` is, so the payload fits
/// php's struct.
pub(crate) struct Formatter {
    umsgf: MessageFormat,
    types: Option<Box<ArgTypes>>,
}

impl Formatter {
    fn new(umsgf: MessageFormat) -> Formatter {
        Formatter { umsgf, types: None }
    }
}

/// `MessageFormatter_object` past its `zend_object`: the error, the
/// formatter a constructor opened, the pattern it was given, and whether
/// date subformats were handed the default zone (`tz_set`), which a
/// second constructor leaves set, as `msgformat_data_free` does; a clone
/// carries both (php 8.5.12).
#[derive(Default)]
pub(crate) struct MessageFormatterObject {
    error: IntlError,
    formatter: Option<Formatter>,
    pattern: Option<Box<[u8]>>,
    zoned: bool,
}

impl Carrier for MessageFormatterObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// Where one call's formatter stands: an object's payload, or the
/// `MessageFormatter_object` php lays on the stack for
/// `msgfmt_format_message` and `msgfmt_parse_message`.
enum Site<'a> {
    Object(PhpObject),
    Local(&'a mut MessageFormatterObject),
}

impl Site<'_> {
    fn data<'s>(&'s mut self, req: &'s mut Request) -> Option<&'s mut MessageFormatterObject> {
        match self {
            Site::Object(object) => object.native_mut::<MessageFormatterObject>(req),
            Site::Local(local) => Some(local),
        }
    }

    fn formatter<'s>(&'s mut self, req: &'s mut Request) -> Option<&'s mut Formatter> {
        self.data(req)?.formatter.as_mut()
    }

    /// `INTL_DATA_ERROR_P(mfo)`, as `msgformat_helpers.cpp` hands it on.
    fn outside(&mut self) -> Outside<'_> {
        match self {
            Site::Object(object) => Outside::of::<MessageFormatterObject>(*object),
            Site::Local(local) => Outside::Local(&mut local.error),
        }
    }

    /// `INTL_DATA_ERROR_CODE(mfo)`.
    fn code(&mut self, req: &mut Request) -> Code {
        self.outside().code(req)
    }

    /// `INTL_DATA_ERROR_CODE(mfo) = code`, as ICU writes it.
    fn set_code(&mut self, req: &mut Request, code: Code) {
        if let Some(data) = self.data(req) {
            data.error.code = code;
        }
    }

    /// `intl_errors_set(INTL_DATA_ERROR_P(mfo), code, message)`.
    fn set(
        &mut self,
        req: &mut Request,
        code: Code,
        message: impl AsRef<[u8]>,
    ) -> Result<(), Thrown> {
        self.outside().set(req, code, message)
    }

    /// `INTL_METHOD_CHECK_STATUS(mfo, message)`: the code copied to the
    /// request, and where it is a failure its message set and `false`.
    fn check(&mut self, req: &mut Request, message: &str) -> Result<bool, Thrown> {
        let code = self.code(req);
        error::set_code(req, code);
        if code.is_failure() {
            self.set(req, code, message)?;
            return Ok(false);
        }
        Ok(true)
    }
}

/// `MessageFormatter_handlers`: `std_object_handlers` with php's clone.
pub(crate) struct Handlers;
pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_formatter)
    }
}

/// `MessageFormatter_object_clone`: the members, the pattern and
/// `tz_set` copied, as php 8.5.12 copies them, and `umsg_clone`, the
/// cached types left behind, or php's `Error` once the copy is made.
fn clone_formatter(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let mut status = Code::ZERO;
    let (copy, pattern, zoned) = match object.native::<MessageFormatterObject>(req) {
        Some(o) => (
            o.formatter.as_ref().map(|f| f.umsgf.try_clone(&mut status)),
            o.pattern.clone(),
            o.zoned,
        ),
        None => (None, None, false),
    };
    let opened = copy.is_some();
    let payload = MessageFormatterObject {
        formatter: copy.flatten().map(Formatter::new),
        pattern,
        zoned,
        ..MessageFormatterObject::default()
    };
    let new = clone_with(req, &object, payload)?;
    let message = if !opened {
        "Cannot clone uninitialized MessageFormatter"
    } else if status.is_failure() {
        "Failed to clone MessageFormatter"
    } else {
        return Ok(new);
    };
    new.give_up(req);
    Err(throw_error(req, ce.error, message))
}

/// `MSG_FORMAT_METHOD_FETCH_OBJECT`: the object's error reset, and
/// php's `Error` where no constructor opened a formatter. The request's
/// was reset before the arguments were held (`MSG_FORMAT_METHOD_INIT_VARS`,
/// `before_args = error::reset`), so a `TypeError` leaves no earlier failure
/// standing.
///
/// ```php
/// $f = new MessageFormatter('en', '{0}');
/// $f->format([-1 => 1]);
/// try { $f->format(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// $f->format([-1 => 1]);
/// try { $f->setPattern([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// $f->format([-1 => 1]);
/// try { $f->getLocale(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// msgfmt_format($f, [-1 => 1]);
/// try { msgfmt_parse($f, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// msgfmt_format($f, [-1 => 1]);
/// try { msgfmt_get_pattern(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// ```
/// ```expect
/// MessageFormatter::format(): Argument #1 ($values) must be of type array, int given
/// int(0)
/// MessageFormatter::setPattern(): Argument #1 ($pattern) must be of type string, array given
/// int(0)
/// MessageFormatter::getLocale() expects exactly 0 arguments, 1 given
/// int(0)
/// msgfmt_parse(): Argument #2 ($string) must be of type string, array given
/// int(0)
/// msgfmt_get_pattern(): Argument #1 ($formatter) must be of type MessageFormatter, stdClass given
/// int(0)
/// ```
fn fetch(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::reset_object::<MessageFormatterObject>(req, object);
    let opened = object
        .native::<MessageFormatterObject>(req)
        .is_some_and(|o| o.formatter.is_some());
    if !opened {
        return Err(throw_error(
            req,
            ce.error,
            "Found unconstructed MessageFormatter",
        ));
    }
    Ok(())
}

/// `msgfmt_ctor`'s arguments, counted and held where the native says,
/// the constructor's inside its [`error::constructing`] span; the
/// request's error was reset before them (`before_args`).
fn arguments(
    req: &mut Request,
    locale: &Raw<PhpString>,
    pattern: &Raw<PhpString>,
) -> Result<(PhpString, PhpString), Thrown> {
    php_ext::raising::arity(req, 2, Some(2))?;
    Ok((locale.hold(req)?, pattern.hold(req)?))
}

/// `intl_convert_utf8_to_utf16` of a pattern php converts only where it
/// is not empty: none for an empty one, the code for a failure.
fn pattern_units(pattern: &[u8]) -> Result<Option<Vec<u16>>, Code> {
    if pattern.is_empty() {
        return Ok(None);
    }
    ffi::to_utf16(pattern).map(Some)
}

/// `intl_locale_get_default()` where the locale is empty.
fn locale_or_default(req: &Request, locale: &[u8]) -> Vec<u8> {
    if locale.is_empty() {
        crate::default_locale(req)
    } else {
        locale.to_vec()
    }
}

/// `pattern syntax error (...)`, as the constructors word a parse that
/// stopped.
fn syntax_error(parse_error: &ParseError) -> String {
    format!("pattern syntax error ({})", error::parse_error(parse_error))
}

/// `msgfmt_ctor`: `Ok(false)` for php's `FAILURE` where nothing was
/// thrown.
fn construct(
    req: &mut Request,
    object: PhpObject,
    (locale, pattern): &(PhpString, PhpString),
) -> Result<bool, Thrown> {
    let (locale, pattern) = (locale.as_bytes(), pattern.as_bytes());
    if locale_too_long(req, locale.len())? {
        return Ok(false);
    }
    error::reset_object::<MessageFormatterObject>(req, object);
    let mut site = Site::Object(object);
    let units = match pattern_units(pattern) {
        Ok(units) => units,
        Err(code) => {
            site.set_code(req, code);
            return site.check(req, "error converting pattern to UTF-16");
        }
    };
    let locale = c_string(&locale_or_default(req, locale));
    if let Some(data) = site.data(req) {
        // `msgformat_data_free` where a constructor ran before.
        if data.pattern.is_some() {
            data.formatter = None;
            data.error.reset();
        }
        data.pattern = Some(pattern.into());
    }
    let mut parse_error = ParseError::default();
    let mut status = Code::ZERO;
    let umsgf = umsg::open(
        units.as_deref(),
        &locale,
        Some(&mut parse_error),
        &mut status,
    );
    if let Some(data) = site.data(req) {
        data.formatter = umsgf.map(Formatter::new);
        data.error.code = status;
    }
    if status == PATTERN_SYNTAX {
        error::set_code(req, status);
        let message = syntax_error(&parse_error);
        error::errors_set_custom_msg::<MessageFormatterObject>(req, object, &message)?;
        return Ok(false);
    }
    site.check(req, "message formatter creation failed")
}

/// `msgfmt_create`: a fresh `MessageFormatter`, or null where its
/// constructor failed.
fn create(
    req: &mut Request,
    locale: &Raw<PhpString>,
    pattern: &Raw<PhpString>,
) -> Result<Option<Owner<Instance<MessageFormatter>>>, Thrown> {
    let given = arguments(req, locale, pattern)?;
    let Some(class) = lookup_class(req, b"MessageFormatter")? else {
        return Err(fatal(req, "InternalError: MessageFormatter is not linked"));
    };
    let object = new_object(req, class)?;
    match construct(req, *object, &given) {
        Ok(true) => Ok(Some(object.into_instance())),
        Ok(false) => {
            object.give_up(req);
            Ok(None)
        }
        Err(thrown) => {
            object.give_up(req);
            Err(thrown)
        }
    }
}

/// `umsg_get_numeric_types`: the types `getArgTypeList` keeps, by number.
fn numbered_types(umsgf: &MessageFormat) -> ArgTypes {
    (0..)
        .zip(umsgf.arg_types())
        .map(|(n, ty)| (ArgKey::Number(n), ty))
        .collect()
}

/// `umsg_parse_format` over a pattern that names its arguments: each
/// `ARG_START`'s name or number, and the type its argument type and
/// style say, as `MessageFormat::cacheExplicitFormats` reads them; the
/// first inconsistency's code and message otherwise.
fn named_types(umsgf: &MessageFormat) -> Result<ArgTypes, (Code, &'static str)> {
    let mut types = ArgTypes::new();
    let count = umsgf.part_count();
    let text_of = |i: i32| umsgf.part(i).map(|p| (p.kind, p.text));
    // The last two parts are at most ARG_LIMIT and MSG_LIMIT.
    let mut i = 0;
    while i < count - 2 {
        let Some((PartKind::ArgStart(arg), _)) = text_of(i) else {
            i += 1;
            continue;
        };
        i += 1;
        let key = match text_of(i) {
            Some((PartKind::ArgName, name)) => ArgKey::Name(name.to_vec()),
            Some((PartKind::ArgNumber(n), _)) if n < 0 => {
                return Err((INVALID_FORMAT, "Found part with negative number"));
            }
            Some((PartKind::ArgNumber(n), _)) => ArgKey::Number(i64::from(n)),
            _ => return Err((INVALID_FORMAT, "Invalid part type encountered")),
        };
        let ty = match arg {
            Some(ArgKind::Simple) => {
                i += 1;
                let Some((PartKind::ArgType, name)) = text_of(i) else {
                    return Err((
                        PARSE,
                        "Expected UMSGPAT_PART_TYPE_ARG_TYPE part following UMSGPAT_ARG_TYPE_SIMPLE part",
                    ));
                };
                simple_type(name, text_of(i + 1))
            }
            Some(ArgKind::Plural | ArgKind::Choice | ArgKind::SelectOrdinal) => {
                Some(ArgType::Double)
            }
            Some(ArgKind::Untyped | ArgKind::Select) | None => Some(ArgType::String),
        };
        let stored = types.entry(key).or_insert(ArgType::Object);
        // ICU refuses a simple type it does not know when the pattern is
        // applied, so a type is always found; php's would be unset.
        if let Some(ty) = ty {
            if *stored != ArgType::Object && *stored != ty {
                return Err((
                    ARGUMENT_TYPE_MISMATCH,
                    "Inconsistent types declared for an argument",
                ));
            }
            *stored = ty;
        }
        i += 1;
    }
    Ok(types)
}

/// A simple argument's type, by its type name and the style part after
/// it.
fn simple_type(name: &[u16], style: Option<(PartKind, &[u16])>) -> Option<ArgType> {
    let is = |text: &[u16], word: &str| text.iter().copied().eq(word.encode_utf16());
    if is(name, "number") {
        return Some(match style {
            Some((PartKind::ArgStyle, style)) if is(style, "integer") => ArgType::Int64,
            _ => ArgType::Double,
        });
    }
    if is(name, "date") || is(name, "time") {
        return Some(ArgType::Date);
    }
    ["spellout", "ordinal", "duration"]
        .iter()
        .any(|word| is(name, word))
        .then_some(ArgType::Double)
}

/// `umsg_get_types`: the argument types, cached on the formatter; none
/// where reading them failed, the error set on the site.
fn arg_types(req: &mut Request, site: &mut Site) -> Result<Option<ArgTypes>, Thrown> {
    if site.code(req).is_failure() {
        return Ok(None);
    }
    let Some(formatter) = site.formatter(req) else {
        return Ok(None);
    };
    if let Some(types) = &formatter.types {
        return Ok(Some(ArgTypes::clone(types)));
    }
    let read = if formatter.umsgf.named() {
        named_types(&formatter.umsgf)
    } else {
        Ok(numbered_types(&formatter.umsgf))
    };
    match read {
        Ok(types) => {
            formatter.types = Some(Box::new(types.clone()));
            Ok(Some(types))
        }
        Err((code, message)) => {
            site.set(req, code, message)?;
            Ok(None)
        }
    }
}

/// `umsg_set_timezone`: the date subformats handed the request's
/// default zone, once per formatter. A date argument inside a complex
/// argument is not reached, as `getFormats()` does not answer it.
fn set_timezone(req: &mut Request, site: &mut Site) -> Result<(), Thrown> {
    let Some(data) = site.data(req).filter(|d| !d.zoned) else {
        return Ok(());
    };
    let Some(formatter) = data.formatter.as_mut() else {
        return Ok(());
    };
    match formatter.umsgf.date_formats(None) {
        None => {
            let message = "Out of memory retrieving subformats";
            site.set(req, Code::MEMORY_ALLOCATION, message)?;
        }
        Some(dates) if dates > 0 && !site.code(req).is_failure() => {
            let zone = process_timezone_argument(req, None, &mut site.outside())?;
            if let (Some(zone), Some(formatter)) = (zone, site.formatter(req)) {
                formatter.umsgf.date_formats(Some(&zone));
            }
        }
        Some(_) => {}
    }
    if !site.code(req).is_failure()
        && let Some(data) = site.data(req)
    {
        data.zoned = true;
    }
    Ok(())
}

/// A key or value php prints with `%s`: up to its first NUL.
fn shown(bytes: &[u8]) -> &[u8] {
    php_ext::coerce::c_str(bytes)
}

/// `'<shown>'` within a message.
fn quoted(head: &str, bytes: &[u8], tail: &str) -> Vec<u8> {
    [head.as_bytes(), b"'", shown(bytes), b"'", tail.as_bytes()].concat()
}

/// `umsg_format_helper`'s walk over the arguments: the site its errors
/// are set on, and what a conversion threw, php's `EG(exception)`, which
/// the walk goes on past as php's loop does: `zval_get_tmp_string`
/// leaves the throwable pending and answers the empty string, and the
/// later conversions are served as php serves them with one pending
/// ([`Carrying`]). An argument whose `__toString` took an `exit` leaves
/// the unwind exit pending, and the walk goes on past it so too: a later
/// `__toString` is not called, and an array's warning is rendered:
///
/// ```php
/// class X { function __toString(): string { exit("exiting\n"); } }
/// class Y { function __toString(): string { echo "Y called\n"; return "y"; } }
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// $f = new MessageFormatter('en', '{0} {1} {2}');
/// var_dump($f->format([new X, new Y, []]));
/// echo "unreached\n";
/// ```
/// ```expect
/// exiting
///
/// Warning: Array to string conversion in %SCRATCH%/witness.php on line 6
/// shutdown
/// ```
struct Converting<'s, 'a> {
    site: &'s mut Site<'a>,
    pending: &'s mut Carrying,
}

impl Converting<'_, '_> {
    /// A conversion that can call PHP code: its value, or `None` where it
    /// threw, the throwable pending from there on ([`Carrying::serve`]).
    fn serve<T>(
        &mut self,
        req: &mut Request,
        step: impl FnOnce(&mut Request) -> Result<T, Thrown>,
    ) -> Answer<T> {
        self.pending.serve(req, step)
    }

    /// What a step threw, chained over what is pending
    /// ([`Carrying::keep`]).
    fn keep(&mut self, req: &mut Request, thrown: Thrown) -> Result<(), Thrown> {
        self.pending.keep(req, thrown)
    }

    /// `intl_errors_set(&err, code, message)`, its `IntlException`
    /// chained over what is pending; `Err` where anything is pending
    /// after it, which ends the walk, as the failure ends php's at its
    /// next row.
    fn set(
        &mut self,
        req: &mut Request,
        code: Code,
        message: impl AsRef<[u8]>,
    ) -> Result<(), Thrown> {
        match self.pending.take() {
            None => self.site.set(req, code, message),
            pending => self.site.outside().set_over(req, pending, code, message),
        }
    }

    fn code(&mut self, req: &mut Request) -> Code {
        self.site.code(req)
    }

    /// One argument's value as a string `Formattable`, its `__toString`
    /// served for an object, the empty string where that threw; a
    /// failure's message set with `None`.
    fn string(&mut self, req: &mut Request, value: &Value) -> Answer<Formattable> {
        let Some(text) = self.serve(req, |req| to_string(req, value))? else {
            return Ok(Some(Formattable::String(Vec::new())));
        };
        let converted = ffi::to_utf16(text.as_bytes());
        let answer = match converted {
            Ok(units) => Ok(Some(Formattable::String(units))),
            Err(code) => {
                let message = quoted(
                    "Invalid UTF-8 data in string argument: ",
                    text.as_bytes(),
                    "",
                );
                self.set(req, code, message).map(|()| None)
            }
        };
        text.give_up(req);
        answer
    }

    /// `zval_get_double`.
    fn double(&mut self, req: &mut Request, value: &Value) -> Result<Formattable, Thrown> {
        let d = self.serve(req, |req| to_double(req, value))?;
        Ok(Formattable::Double(d.unwrap_or(0.0)))
    }

    /// A `zend_long` or `double` held to the range of a 32-bit argument,
    /// php's `int32_t` cast of anything else.
    fn long(&mut self, req: &mut Request, value: &Value) -> Answer<Formattable> {
        let (low, high) = (i32::MIN, i32::MAX);
        #[allow(clippy::cast_possible_truncation, reason = "php's (int32_t) casts")]
        let n = match value.as_ref() {
            ValueRef::Float(d) if d > f64::from(high) || d < f64::from(low) => {
                let message =
                    "Found PHP float with absolute value too large for 32 bit integer argument";
                return self
                    .set(req, Code::ILLEGAL_ARGUMENT, message)
                    .map(|()| None);
            }
            ValueRef::Float(d) => d as i32,
            ValueRef::Int(n) => {
                let Ok(n) = i32::try_from(n) else {
                    let message = "Found PHP integer with absolute value too large for 32 bit integer argument";
                    return self
                        .set(req, Code::ILLEGAL_ARGUMENT, message)
                        .map(|()| None);
                };
                n
            }
            _ => self.serve(req, |req| to_long(req, value))?.unwrap_or(0) as i32,
        };
        Ok(Some(Formattable::Long(n)))
    }

    /// A `double` held to the range of a 64-bit argument; an integer as
    /// it is, anything else through `zval_get_long`.
    fn int64(&mut self, req: &mut Request, value: &Value) -> Answer<Formattable> {
        #[allow(clippy::cast_precision_loss, reason = "php's (double)U_INT64_MAX")]
        let (low, high) = (i64::MIN as f64, i64::MAX as f64);
        #[allow(clippy::cast_possible_truncation, reason = "php's (int64_t) cast")]
        let n = match value.as_ref() {
            ValueRef::Float(d) if d > high || d < low => {
                let message =
                    "Found PHP float with absolute value too large for 64 bit integer argument";
                return self
                    .set(req, Code::ILLEGAL_ARGUMENT, message)
                    .map(|()| None);
            }
            ValueRef::Float(d) => d as i64,
            ValueRef::Int(n) => n,
            _ => self.serve(req, |req| to_long(req, value))?.unwrap_or(0),
        };
        Ok(Some(Formattable::Int64(n)))
    }

    /// A date argument's milliseconds, `NAN` where `getTimestamp()`
    /// threw, as `intl_datetime_decompose` leaves them; where the value is
    /// no date, `umsg_format_helper`'s message set over the one
    /// `intl_zval_to_millis` set. The conversion is served as the others
    /// are ([`Carrying::serve`]), so past a throwable an earlier argument
    /// left pending a `DateTimeInterface`'s `getTimestamp()` is not
    /// called, as `zend_call_known_function` returns at once
    /// (`Z/Zend/zend_execute_API.c:822`), and the date is `NAN`:
    ///
    /// ```php
    /// class T { function __toString(): string { throw new Exception('t'); } }
    /// class D extends DateTime { function getTimestamp(): int { echo "D::getTimestamp\n"; return 0; } }
    /// class E extends DateTime { function getTimestamp(): int { throw new Exception('e'); } }
    /// try { var_dump(MessageFormatter::formatMessage('en', '{0} {1,date}', [new T, new D('@0')])); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// try { var_dump(MessageFormatter::formatMessage('en', '{0} {1,date} {2}', [new T, new E('@0'), new T])); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), ' ', var_export($e->getPrevious(), true), "\n"; }
    /// try { var_dump(MessageFormatter::formatMessage('en', '{0,date} {1}', [new E('@0'), new D('@0')])); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// var_dump(MessageFormatter::formatMessage('en', '{0,date,yyyy}', [new D('@0')]));
    /// ini_set('intl.use_exceptions', 1);
    /// try { MessageFormatter::formatMessage('en', '{0} {1,date}', [new T, 'x']); } catch (Exception $e) { for (; $e; $e = $e->getPrevious()) echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Exception: t
    /// Exception: t NULL
    /// Exception: e
    /// D::getTimestamp
    /// string(4) "1970"
    /// IntlException: MessageFormatter::formatMessage(): The argument for key '1' cannot be used as a date or time
    /// IntlException: MessageFormatter::formatMessage(): string 'x' is not numeric, which would be required for it to be a valid date
    /// Exception: t
    /// ```
    fn date(&mut self, req: &mut Request, value: Value, name: &[u16]) -> Answer<Formattable> {
        let site = &mut *self.site;
        let millis = self
            .pending
            .serve(req, |req| zval_to_millis(req, value, &mut site.outside()))?;
        let code = self.code(req);
        if !code.is_failure() {
            return Ok(Some(Formattable::Date(millis.unwrap_or(f64::NAN))));
        }
        let Ok(key) = ffi::to_utf8(name) else {
            return Ok(None);
        };
        let message = quoted(
            "The argument for key ",
            &key,
            " cannot be used as a date or time",
        );
        self.set(req, code, message).map(|()| None)
    }

    /// An argument the pattern does not type, by what the value is: a
    /// number as itself, `null` and the booleans as integers, a string
    /// or an object as a string; php's message for anything else.
    fn untyped(&mut self, req: &mut Request, value: &Value, name: &[u16]) -> Answer<Formattable> {
        Ok(Some(match value.as_ref() {
            ValueRef::Float(d) => Formattable::Double(d),
            ValueRef::Int(n) => Formattable::Int64(n),
            ValueRef::Null | ValueRef::Bool(false) => Formattable::Int64(0),
            ValueRef::Bool(true) => Formattable::Int64(1),
            ValueRef::Str(_) | ValueRef::Object(_) => return self.string(req, value),
            _ => {
                if let Ok(key) = ffi::to_utf8(name) {
                    let message = quoted(
                        "No strategy to convert the value given for the argument with key ",
                        &key,
                        " is available",
                    );
                    self.set(req, Code::ILLEGAL_ARGUMENT, message)?;
                }
                return Ok(None);
            }
        }))
    }
}

/// `umsg_format_helper`: each argument named and typed by the pattern
/// (or by its value where the pattern does not say), then
/// `MessageFormat::format`; none where any step failed, its error set
/// on the site. A conversion that throws leaves its throwable in
/// `pending` while the walk goes on and `MessageFormat::format` runs,
/// and the native returns into it ([`do_format`]), so what the later
/// arguments raise is raised:
///
/// ```php
/// class T { function __toString(): string { throw new Exception('t'); } }
/// class U { function __toString(): string { echo "U::__toString\n"; return 'u'; } }
/// class V { function __toString(): string { throw new Exception('v'); } }
/// try { MessageFormatter::formatMessage('en', '{0} {1}', [new T, []]); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump(MessageFormatter::formatMessage('en', '{0} {1} {2} {3,number,integer} {4}', [new T, new U, new V, [], "\xff"])); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), ' ', var_export($e->getPrevious(), true), "\n"; }
/// var_dump(intl_get_error_code());
/// ini_set('intl.use_exceptions', 1);
/// try { MessageFormatter::formatMessage('en', '{0} {1}', [new T, "\xff"]); } catch (Exception $e) { for (; $e; $e = $e->getPrevious()) echo get_class($e), ': ', bin2hex($e->getMessage()), "\n"; }
/// ```
/// ```expect
/// Warning: Array to string conversion in %s on line 5
/// t
/// Exception: t NULL
/// int(10)
/// IntlException: 4d657373616765466f726d61747465723a3a666f726d61744d65737361676528293a20496e76616c6964205554462d38206461746120696e20737472696e6720617267756d656e743a2027ff27
/// Exception: 74
/// ```
fn format_helper(
    req: &mut Request,
    site: &mut Site,
    pending: &mut Carrying,
    values: PhpArray,
) -> Answer<Vec<u16>> {
    if site.code(req).is_failure() {
        return Ok(None);
    }
    let types = arg_types(req, site)?.unwrap_or_default();
    set_timezone(req, site)?;
    let mut args = Vec::with_capacity(values.len());
    let mut converting = Converting { site, pending };
    // `ZEND_HASH_FOREACH_KEY_VAL(args, num_index, str_index, elem)` over
    // the argument, which holds the array while a conversion serves
    // `__toString` or a date argument's methods.
    let walked = walk::foreach(req, values, Hold::None, |req, row| -> walk::Step {
        if converting.code(req).is_failure() {
            return Ok(ControlFlow::Break(()));
        }
        let value = row.value().deref();
        let (name, stored) = match row.key() {
            Key::Int(n) if !(0..=i64::from(i32::MAX)).contains(n) => {
                let message = "Found negative or too large array key";
                converting.set(req, Code::ILLEGAL_ARGUMENT, message)?;
                return Ok(ControlFlow::Continue(()));
            }
            Key::Int(n) => (
                n.to_string().encode_utf16().collect(),
                types.get(&ArgKey::Number(*n)),
            ),
            Key::Str(s) => match ffi::to_utf16(s.as_bytes()) {
                Ok(units) => {
                    let stored = types.get(&ArgKey::Name(units.clone()));
                    (units, stored)
                }
                Err(code) => {
                    let message = quoted("Invalid UTF-8 data in argument key: ", s.as_bytes(), "");
                    converting.set(req, code, message)?;
                    return Ok(ControlFlow::Continue(()));
                }
            },
        };
        let arg = match stored.copied().unwrap_or(ArgType::Object) {
            ArgType::String => converting.string(req, &value)?,
            ArgType::Double => Some(converting.double(req, &value)?),
            ArgType::Long => converting.long(req, &value)?,
            ArgType::Int64 => converting.int64(req, &value)?,
            ArgType::Date => converting.date(req, value, &name)?,
            ArgType::Object => converting.untyped(req, &value, &name)?,
            ArgType::Array => {
                converting.set(
                    req,
                    Code::ILLEGAL_ARGUMENT,
                    "Found unsupported argument type",
                )?;
                None
            }
        };
        if let Some(arg) = arg {
            args.push((name, arg));
        }
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        converting.keep(req, thrown)?;
    }
    formatted(req, &mut converting, &args)
}

/// `umsg_format_helper` after its walk: nothing where a row failed, else
/// `MessageFormat::format` over the arguments, which runs with a
/// throwable pending as php's does, its failure set over that one.
fn formatted(
    req: &mut Request,
    converting: &mut Converting,
    args: &[(Vec<u16>, Formattable)],
) -> Answer<Vec<u16>> {
    let ce = crate::throwables::Throwables::of(req)?;
    if converting.code(req).is_failure() {
        return Ok(None);
    }
    // The formatter is read again once the arguments' PHP code has run:
    // an object that code left unconstructed is the `Error`.
    let Some(formatter) = converting.site.formatter(req) else {
        let message = "Found unconstructed MessageFormatter";
        let thrown = throw_error(req, ce.error, message);
        return converting.keep(req, thrown).map(|()| None);
    };
    let mut status = Code::ZERO;
    let text = formatter.umsgf.format(args, &mut status);
    if status.is_failure() {
        let message = "Call to ICU MessageFormat::format() has failed";
        converting.set(req, status, message)?;
        return Ok(None);
    }
    Ok(Some(text.map(|t| t.units().to_vec()).unwrap_or_default()))
}

/// `msgfmt_do_format`: `umsg_format_helper`'s text in UTF-8, `false`
/// where it or the conversion failed. A throwable a conversion left
/// pending is returned into after the text is converted, so where the
/// walk set no failure the request's code is set as
/// `INTL_METHOD_RETVAL_UTF8` sets it with the throwable in
/// `EG(exception)`, a stale failure reset to `U_ZERO_ERROR` and its
/// message left standing as `intl_error_set_code` leaves it:
///
/// ```php
/// class T { function __toString(): string { throw new Exception('t'); } }
/// var_dump(MessageFormatter::formatMessage('en', '{0,date}', ['x']));
/// var_dump(intl_get_error_code());
/// try { var_dump(MessageFormatter::formatMessage('en', '{0}', [new T])); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code(), intl_get_error_message());
/// var_dump(MessageFormatter::formatMessage('en', '{0,date}', ['x']));
/// try { var_dump(MessageFormatter::formatMessage('en', '{0} {1,date}', [new T, 'x'])); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code(), intl_get_error_message());
/// $f = new MessageFormatter('en', '{0}');
/// var_dump(MessageFormatter::formatMessage('en', '{0,date}', ['x']));
/// try { var_dump($f->format([new T])); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code(), $f->getErrorCode(), $f->getErrorMessage());
/// ```
/// ```expect
/// bool(false)
/// int(1)
/// Exception: t
/// int(0)
/// string(106) "MessageFormatter::formatMessage(): The argument for key '0' cannot be used as a date or time: U_ZERO_ERROR"
/// bool(false)
/// Exception: t
/// int(1)
/// string(118) "MessageFormatter::formatMessage(): The argument for key '1' cannot be used as a date or time: U_ILLEGAL_ARGUMENT_ERROR"
/// bool(false)
/// Exception: t
/// int(0)
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// ```
fn do_format(req: &mut Request, site: &mut Site, values: PhpArray) -> Answer<Owner<PhpString>> {
    let mut pending = Carrying::default();
    let text = match format_helper(req, site, &mut pending, values)? {
        // `INTL_METHOD_RETVAL_UTF8`, served past what is pending.
        Some(units) => pending
            .serve(req, |req| {
                let converted = ffi::to_utf8(&units);
                site.set_code(req, converted.as_ref().err().copied().unwrap_or(Code::ZERO));
                if !site.check(req, "Error converting value to UTF-8")? {
                    return Ok(None);
                }
                match converted {
                    Ok(bytes) => PhpString::new(req, &bytes).map(Some),
                    Err(_) => Ok(None),
                }
            })?
            .flatten(),
        None => None,
    };
    match pending.take() {
        Some(pending) => Err(pending),
        None => Ok(text),
    }
}

/// `msgfmt_format`.
fn format(req: &mut Request, object: PhpObject, values: PhpArray) -> Answer<Owner<PhpString>> {
    fetch(req, object)?;
    do_format(req, &mut Site::Object(object), values)
}

/// A formatter of one call's own, as `msgfmt_format_message` and
/// `msgfmt_parse_message` open one; `None` where the pattern did not
/// convert, its message set on the request.
fn local_pattern(
    req: &mut Request,
    pattern: &[u8],
) -> Result<Result<Option<Vec<u16>>, ()>, Thrown> {
    let Ok(units) = pattern_units(pattern) else {
        let message = "error converting pattern to UTF-16";
        error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(Err(()));
    };
    Ok(Ok(units))
}

/// `msgfmt_format_message`: a formatter opened for the call, null for a
/// locale too long.
fn format_message(
    req: &mut Request,
    locale: &[u8],
    pattern: &[u8],
    values: PhpArray,
) -> Result<OrNull<Owner<PhpString>>, Thrown> {
    if locale_too_long(req, locale.len())? {
        return Ok(Union::B(Untyped(())));
    }
    let answer = |a: Option<Owner<PhpString>>| Union::A(a.map_or(OrFalse::False, OrFalse::Value));
    let Ok(units) = local_pattern(req, pattern)? else {
        return Ok(answer(None));
    };
    let locale = c_string(&locale_or_default(req, locale));
    let mut local = MessageFormatterObject::default();
    let mut parse_error = ParseError::default();
    let mut status = Code::ZERO;
    let umsgf = umsg::open(
        units.as_deref(),
        &locale,
        Some(&mut parse_error),
        &mut status,
    );
    local.error.code = status;
    if status.is_failure() {
        if status == PATTERN_SYNTAX {
            error::set_code(req, status);
            error::set_custom_msg(req, syntax_error(&parse_error))?;
        } else {
            error::set_custom_msg(req, "Creating message formatter failed")?;
        }
        return Ok(answer(None));
    }
    local.formatter = umsgf.map(Formatter::new);
    do_format(req, &mut Site::Local(&mut local), values).map(answer)
}

/// One parsed value as `umsg_parse_helper` answers it: a date in
/// seconds, an integer past `ZEND_LONG_MAX` as a float.
#[allow(clippy::cast_precision_loss, reason = "php's (double) casts")]
fn parsed_value(
    req: &mut Request,
    value: Formattable,
) -> Result<Result<Owner<Value>, Code>, Thrown> {
    let value = match value {
        Formattable::Date(ms) => Value::float(ms / 1000.0),
        Formattable::Double(d) => Value::float(d),
        Formattable::Long(n) => Value::int(i64::from(n)),
        Formattable::Int64(n) if n == i64::MIN => Value::float(n as f64),
        Formattable::Int64(n) => Value::int(n),
        Formattable::String(units) => match ffi::to_utf8(&units) {
            Ok(bytes) => return Ok(Ok(PhpString::new(req, &bytes)?.into_value())),
            Err(code) => return Ok(Err(code)),
        },
    };
    Ok(Ok(Owner::take(req, value)))
}

/// `msgfmt_do_parse`: the source in UTF-16, `MessageFormat::parse`, and
/// the values as a list; `false` where a step failed.
fn do_parse(req: &mut Request, site: &mut Site, source: &[u8]) -> Answer<Owner<PhpArray>> {
    let units = ffi::to_utf16(source);
    site.set_code(req, units.as_ref().err().copied().unwrap_or(Code::ZERO));
    if !site.check(req, "Converting parse string failed")? {
        return Ok(None);
    }
    let units = units.unwrap_or_default();
    let mut status = site.code(req);
    let parsed = site
        .formatter(req)
        .and_then(|f| f.umsgf.parse(&units, &mut status));
    let mut values = Vec::new();
    for value in parsed.into_iter().flatten() {
        let Some(value) = value else {
            status = Code::ILLEGAL_ARGUMENT;
            continue;
        };
        match parsed_value(req, value)? {
            Ok(value) => values.push(value),
            Err(code) => {
                status = code;
                break;
            }
        }
    }
    site.set_code(req, status);
    if !site.check(req, "Parsing failed")? {
        for value in values {
            value.give_up(req);
        }
        return Ok(None);
    }
    let count = u32::try_from(values.len()).unwrap_or(u32::MAX);
    let mut array = PhpArray::sized(req, count, Layout::Packed)?;
    for value in values {
        array.append(req, value)?;
    }
    Ok(Some(array))
}

/// `msgfmt_parse`.
fn parse(req: &mut Request, object: PhpObject, source: PhpString) -> Answer<Owner<PhpArray>> {
    fetch(req, object)?;
    do_parse(req, &mut Site::Object(object), source.as_bytes())
}

/// `msgfmt_parse_message`: a formatter opened for the call, null for a
/// locale too long.
fn parse_message(
    req: &mut Request,
    locale: &[u8],
    pattern: &[u8],
    source: &[u8],
) -> Result<OrNull<Owner<PhpArray>>, Thrown> {
    if locale_too_long(req, locale.len())? {
        return Ok(Union::B(Untyped(())));
    }
    let answer = |a: Option<Owner<PhpArray>>| Union::A(a.map_or(OrFalse::False, OrFalse::Value));
    let Ok(units) = local_pattern(req, pattern)? else {
        return Ok(answer(None));
    };
    let locale = c_string(&locale_or_default(req, locale));
    let mut local = MessageFormatterObject::default();
    let mut status = Code::ZERO;
    local.formatter = umsg::open(units.as_deref(), &locale, None, &mut status).map(Formatter::new);
    local.error.code = status;
    let mut site = Site::Local(&mut local);
    if !site.check(req, "Creating message formatter failed")? {
        return Ok(answer(None));
    }
    do_parse(req, &mut site, source).map(answer)
}

/// `msgfmt_set_pattern`: the pattern applied, kept as given and the
/// cached types dropped; `false` where it does not convert or apply.
fn set_pattern(req: &mut Request, object: PhpObject, pattern: PhpString) -> Answer<()> {
    fetch(req, object)?;
    let mut site = Site::Object(object);
    let units = ffi::to_utf16(pattern.as_bytes());
    site.set_code(req, units.as_ref().err().copied().unwrap_or(Code::ZERO));
    if !site.check(req, "Error converting pattern to UTF-16")? {
        return Ok(None);
    }
    let units = units.unwrap_or_default();
    let mut parse_error = ParseError::default();
    let mut status = site.code(req);
    if let Some(formatter) = site.formatter(req) {
        formatter
            .umsgf
            .apply_pattern(&units, &mut parse_error, &mut status);
    }
    site.set_code(req, status);
    if status.is_failure() {
        let message = format!(
            "Error setting symbol value at line {}, offset {}",
            parse_error.line, parse_error.offset
        );
        error::errors_set_custom_msg::<MessageFormatterObject>(req, object, &message)?;
        return Ok(None);
    }
    if let Some(data) = site.data(req) {
        data.pattern = Some(pattern.as_bytes().into());
        if let Some(formatter) = data.formatter.as_mut() {
            formatter.types = None;
        }
    }
    Ok(Some(()))
}

/// `msgfmt_get_pattern`: the pattern as given, a clone's copied with it.
fn get_pattern(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpString>> {
    fetch(req, object)?;
    let pattern = object
        .native::<MessageFormatterObject>(req)
        .and_then(|o| o.pattern.clone());
    pattern.map(|p| PhpString::new(req, &p)).transpose()
}

/// `msgfmt_get_locale`: `umsg_getLocale`.
fn get_locale(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    fetch(req, object)?;
    let locale = object
        .native::<MessageFormatterObject>(req)
        .and_then(|o| o.formatter.as_ref())
        .map(|f| f.umsgf.locale())
        .unwrap_or_default();
    PhpString::new(req, &locale)
}

/// `msgfmt_get_error_code`: the object's code, the request's left as it
/// is.
fn error_code(req: &mut Request, object: PhpObject) -> i64 {
    i64::from(error::object_code::<MessageFormatterObject>(req, object).0)
}

/// `msgfmt_get_error_message`.
fn error_message(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let message = error::object_message::<MessageFormatterObject>(req, object);
    PhpString::new(req, &message)
}

fn or_false<T>(answer: Option<T>) -> OrFalse<T> {
    answer.map_or(OrFalse::False, OrFalse::Value)
}

/// `class MessageFormatter`: one `UMessageFormat` each, cloned with the
/// object and its pattern; one `new` without a constructor has none.
///
/// ```php
/// $m = new MessageFormatter('en_US', '{0,number} monkeys on {1,number} trees');
/// var_dump($m->format([4560, 123]), $m->getPattern(), $m->getLocale(), $m->getErrorCode(), $m->getErrorMessage());
/// $c = clone $m; var_dump($c->getPattern(), $c->format([1, 2]));
/// var_dump($m->setPattern('{0} {1,number}'), $m->format(['x', 1970]), $m->parse('a 1,970'));
/// var_dump($m->setPattern('{0,'), $m->getErrorCode(), $m->getErrorMessage());
/// var_dump(MessageFormatter::create('en', '{n, plural, one{# cat} other{# cats}}')->format(['n' => 3]));
/// var_dump(MessageFormatter::formatMessage('de', '{0,number,integer}', [12345.7]), MessageFormatter::parseMessage('en', '{0} and {1,number}', 'x and 4.5'));
/// try { new MessageFormatter('en', '{0,choice}'); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// $u = (new ReflectionClass('MessageFormatter'))->newInstanceWithoutConstructor();
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $u->format([]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(26) "4,560 monkeys on 123 trees"
/// string(38) "{0,number} monkeys on {1,number} trees"
/// string(5) "en_US"
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// string(38) "{0,number} monkeys on {1,number} trees"
/// string(20) "1 monkeys on 2 trees"
/// bool(true)
/// string(7) "x 1,970"
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   int(1970)
/// }
/// bool(false)
/// int(65801)
/// string(98) "MessageFormatter::setPattern(): Error setting symbol value at line 0, offset 0: U_UNMATCHED_BRACES"
/// string(6) "3 cats"
/// string(6) "12.345"
/// array(2) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   float(4.5)
/// }
/// MessageFormatter::__construct(): pattern syntax error (parse error at offset 1, after "{", before or at "0,choice}")
/// Cannot clone uninitialized MessageFormatter
/// Found unconstructed MessageFormatter
/// ```
///
/// A clone keeps `tz_set` too, so its date subformats keep the zone the
/// original was handed rather than taking today's default (php 8.5.12).
///
/// ```php
/// date_default_timezone_set('UTC');
/// $t = new MessageFormatter('en_US', '{0,time,short}');
/// $at = new DateTimeImmutable('2024-01-02 03:04:05', new DateTimeZone('UTC'));
/// $t->format([$at]);
/// $c = clone $t;
/// date_default_timezone_set('America/New_York');
/// echo str_replace("\u{202F}", ' ', $c->format([$at])), "\n";
/// ```
/// ```expect
/// 3:04 AM
/// ```
#[php_class(not_serializable, size = 112, payload = MessageFormatterObject, handlers = crate::msgformat::HANDLERS)]
pub(crate) struct MessageFormatter;

#[php_class]
#[allow(non_snake_case)]
impl MessageFormatter {
    #[php(arity = manual, before_args = error::reset)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        locale: Raw<PhpString>,
        pattern: Raw<PhpString>,
    ) -> Result<(), Thrown> {
        let object = this.object();
        error::constructing(req, |req| {
            let given = arguments(req, &locale, &pattern)?;
            construct(req, object, &given).map(|_| ())
        })
    }

    #[php(tentative, arity = manual, before_args = error::reset)]
    fn create(
        req: &mut Request,
        locale: Raw<PhpString>,
        pattern: Raw<PhpString>,
    ) -> Result<Option<Owner<Instance<MessageFormatter>>>, Thrown> {
        create(req, &locale, &pattern)
    }

    #[php(tentative, before_args = error::reset)]
    fn format(
        req: &mut Request,
        this: This<Self>,
        values: PhpArray,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        format(req, this.object(), values).map(or_false)
    }

    #[php(tentative)]
    fn formatMessage(
        req: &mut Request,
        locale: PhpString,
        pattern: PhpString,
        values: PhpArray,
    ) -> Result<OrNull<Owner<PhpString>>, Thrown> {
        format_message(req, locale.as_bytes(), pattern.as_bytes(), values)
    }

    #[php(tentative, before_args = error::reset)]
    fn parse(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        parse(req, this.object(), string).map(or_false)
    }

    #[php(tentative)]
    fn parseMessage(
        req: &mut Request,
        locale: PhpString,
        pattern: PhpString,
        message: PhpString,
    ) -> Result<OrNull<Owner<PhpArray>>, Thrown> {
        parse_message(
            req,
            locale.as_bytes(),
            pattern.as_bytes(),
            message.as_bytes(),
        )
    }

    #[php(tentative, before_args = error::reset)]
    fn setPattern(req: &mut Request, this: This<Self>, pattern: PhpString) -> Result<bool, Thrown> {
        set_pattern(req, this.object(), pattern).map(|a| a.is_some())
    }

    #[php(tentative, before_args = error::reset)]
    fn getPattern(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_pattern(req, this.object()).map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn getLocale(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        get_locale(req, this.object())
    }

    #[php(tentative)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        error_code(req, this.object())
    }

    #[php(tentative)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        error_message(req, this.object())
    }
}

type Fmt = Instance<MessageFormatter>;

/// `msgfmt_create`: null where the constructor fails, its message the
/// request's.
///
/// ```php
/// var_dump(msgfmt_create('en', '{0}') instanceof MessageFormatter, msgfmt_create('en', '{0,'), intl_get_error_message(), msgfmt_create('en', ''), intl_get_error_message());
/// ```
/// ```expect
/// bool(true)
/// NULL
/// string(70) "msgfmt_create(): message formatter creation failed: U_UNMATCHED_BRACES"
/// NULL
/// string(76) "msgfmt_create(): message formatter creation failed: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(arity = manual, before_args = error::reset)]
fn msgfmt_create(
    req: &mut Request,
    locale: Raw<PhpString>,
    pattern: Raw<PhpString>,
) -> Result<Option<Owner<Fmt>>, Thrown> {
    create(req, &locale, &pattern)
}

/// `msgfmt_format`: each argument typed by the pattern, or by its value
/// where the pattern names no type.
///
/// ```php
/// $f = msgfmt_create('en_US', '{0,number,integer} {1} {name} {2,date,yyyy-MM-dd} {3}');
/// var_dump(msgfmt_format($f, [3.7, 'x', 'name' => true, 2 => 86400 * 365, 3 => null]));
/// var_dump(msgfmt_format($f, [-1 => 1]), intl_get_error_message());
/// var_dump(msgfmt_format($f, [2 => 'no']), intl_get_error_message(), msgfmt_format($f, [5 => []]), intl_get_error_message());
/// var_dump(msgfmt_format(msgfmt_create('en', '{a,number} {a}'), ['a' => 1]), intl_get_error_message());
/// ```
/// ```expect
/// string(17) "3 x 1 1971-01-01 "
/// bool(false)
/// string(80) "msgfmt_format(): Found negative or too large array key: U_ILLEGAL_ARGUMENT_ERROR"
/// bool(false)
/// string(100) "msgfmt_format(): The argument for key '2' cannot be used as a date or time: U_ILLEGAL_ARGUMENT_ERROR"
/// bool(false)
/// string(124) "msgfmt_format(): No strategy to convert the value given for the argument with key '5' is available: U_ILLEGAL_ARGUMENT_ERROR"
/// bool(false)
/// string(86) "msgfmt_format(): Inconsistent types declared for an argument: U_ARGUMENT_TYPE_MISMATCH"
/// ```
#[php_function(before_args = error::reset)]
fn msgfmt_format(
    req: &mut Request,
    formatter: Fmt,
    values: PhpArray,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let object = formatter.object();
    format(req, object, values).map(or_false)
}

/// `msgfmt_format_message`: the pattern opened for one call.
///
/// ```php
/// var_dump(msgfmt_format_message('en_US', '{0} has {1,number,percent}', ['x', 0.25]), msgfmt_format_message('en', 'some {wrong.format}', []), intl_get_error_message());
/// var_dump(msgfmt_format_message(str_repeat('a', 157), '{0}', []), msgfmt_format_message('en', "\x80", []), intl_get_error_message());
/// ```
/// ```expect
/// string(9) "x has 25%"
/// bool(false)
/// string(141) "msgfmt_format_message(): pattern syntax error (parse error at offset 6, after "some {", before or at "wrong.format}"): U_PATTERN_SYNTAX_ERROR"
/// NULL
/// bool(false)
/// string(85) "msgfmt_format_message(): error converting pattern to UTF-16: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function]
fn msgfmt_format_message(
    req: &mut Request,
    locale: PhpString,
    pattern: PhpString,
    values: PhpArray,
) -> Result<OrNull<Owner<PhpString>>, Thrown> {
    format_message(req, locale.as_bytes(), pattern.as_bytes(), values)
}

/// `msgfmt_parse`: the values a message holds, as a list.
///
/// ```php
/// $f = msgfmt_create('en_US', '{0,number,integer} monkeys on {1,number} trees');
/// var_dump(msgfmt_parse($f, '4,560 monkeys on 1.5 trees'), msgfmt_parse($f, 'nothing'), intl_get_error_message());
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(4560)
///   [1]=>
///   float(1.5)
/// }
/// bool(false)
/// string(53) "msgfmt_parse(): Parsing failed: U_MESSAGE_PARSE_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn msgfmt_parse(
    req: &mut Request,
    formatter: Fmt,
    string: PhpString,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let object = formatter.object();
    parse(req, object, string).map(or_false)
}

/// `msgfmt_parse_message`: the pattern opened for one call.
///
/// ```php
/// var_dump(msgfmt_parse_message('en', '{0} and {1}', 'a and b'), msgfmt_parse_message('en', '{0,whatever}', 'x'), intl_get_error_message());
/// var_dump(msgfmt_parse_message('en', '{a}', 'x'), intl_get_error_message());
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// bool(false)
/// string(83) "msgfmt_parse_message(): Creating message formatter failed: U_ILLEGAL_ARGUMENT_ERROR"
/// bool(false)
/// string(64) "msgfmt_parse_message(): Parsing failed: U_ARGUMENT_TYPE_MISMATCH"
/// ```
#[php_function]
fn msgfmt_parse_message(
    req: &mut Request,
    locale: PhpString,
    pattern: PhpString,
    message: PhpString,
) -> Result<OrNull<Owner<PhpArray>>, Thrown> {
    parse_message(
        req,
        locale.as_bytes(),
        pattern.as_bytes(),
        message.as_bytes(),
    )
}

/// `msgfmt_set_pattern`.
///
/// ```php
/// $f = msgfmt_create('en', '{0}');
/// var_dump(msgfmt_set_pattern($f, '{0} and {1}'), msgfmt_format($f, [1, 2]), msgfmt_set_pattern($f, '{0,number monkeys'), msgfmt_get_error_message($f), msgfmt_get_pattern($f));
/// ```
/// ```expect
/// bool(true)
/// string(7) "1 and 2"
/// bool(false)
/// string(92) "msgfmt_set_pattern(): Error setting symbol value at line 0, offset 1: U_PATTERN_SYNTAX_ERROR"
/// string(11) "{0} and {1}"
/// ```
#[php_function(before_args = error::reset)]
fn msgfmt_set_pattern(
    req: &mut Request,
    formatter: Fmt,
    pattern: PhpString,
) -> Result<bool, Thrown> {
    let object = formatter.object();
    set_pattern(req, object, pattern).map(|a| a.is_some())
}

/// `msgfmt_get_pattern`.
///
/// ```php
/// $f = msgfmt_create('en', '{0} x'); var_dump(msgfmt_get_pattern($f), msgfmt_get_pattern(clone $f));
/// ```
/// ```expect
/// string(5) "{0} x"
/// string(5) "{0} x"
/// ```
#[php_function(before_args = error::reset)]
fn msgfmt_get_pattern(
    req: &mut Request,
    formatter: Fmt,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let object = formatter.object();
    get_pattern(req, object).map(or_false)
}

/// `msgfmt_get_locale`, the request's error reset as every method's is.
///
/// ```php
/// $f = msgfmt_create('de_DE@currency=EUR', '{0}'); msgfmt_format($f, [-1 => 1]);
/// var_dump(intl_get_error_code(), msgfmt_get_locale($f), intl_get_error_code());
/// ```
/// ```expect
/// int(1)
/// string(18) "de_DE@currency=EUR"
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn msgfmt_get_locale(req: &mut Request, formatter: Fmt) -> Result<Owner<PhpString>, Thrown> {
    let object = formatter.object();
    get_locale(req, object)
}

/// `msgfmt_get_error_code`.
///
/// ```php
/// $f = msgfmt_create('en', '{0,date}'); var_dump(msgfmt_format($f, [[]]), msgfmt_get_error_code($f), intl_get_error_code());
/// ```
/// ```expect
/// bool(false)
/// int(1)
/// int(1)
/// ```
#[php_function]
fn msgfmt_get_error_code(req: &mut Request, formatter: Fmt) -> i64 {
    error_code(req, formatter.object())
}

/// `msgfmt_get_error_message`.
///
/// ```php
/// $f = msgfmt_create('en', '{0,date}'); msgfmt_format($f, ['x']); var_dump(msgfmt_get_error_message($f));
/// ```
/// ```expect
/// string(100) "msgfmt_format(): The argument for key '0' cannot be used as a date or time: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function]
fn msgfmt_get_error_message(req: &mut Request, formatter: Fmt) -> Result<Owner<PhpString>, Thrown> {
    error_message(req, formatter.object())
}

/// The `msgfmt_*` functions, in the stub's order.
pub(crate) fn functions(builder: php_ext::abi::ExtensionBuilder) -> php_ext::abi::ExtensionBuilder {
    builder.functions(vec![
        msgfmt_create::declared(),
        msgfmt_format::declared(),
        msgfmt_format_message::declared(),
        msgfmt_parse::declared(),
        msgfmt_parse_message::declared(),
        msgfmt_set_pattern::declared(),
        msgfmt_get_pattern::declared(),
        msgfmt_get_locale::declared(),
        msgfmt_get_error_code::declared(),
        msgfmt_get_error_message::declared(),
    ])
}
