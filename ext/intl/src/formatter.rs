//! `formatter/`: `NumberFormatter` and the `numfmt_*` functions over
//! ICU's `unum_*`. Each method php aliases to a function shares one body
//! here with it, handed the object and how many places the function's
//! arguments stand after the method's (one: the object).

use php_ext::abi::{CloneObj, Instance, ObjectHandlers, OrFalse, Raw, Ref, This, Union, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{clone_with, lookup_class, new_object};
use php_ext::coerce::{to_double, to_long};
use php_ext::raising::{Thrown, argument_error, arity, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};

use crate::error::{self, Carrier, IntlError};
use crate::ffi::formatter::{Number, NumberFormat, ParseError, Parsed, iso3_language};
use crate::ffi::{self, Code};

/// `int|float`.
type Num = Union<i64, f64>;
/// A native's answer before `false` is chosen: `None` is `false`, a
/// throw rides beside.
type Answer<T> = Result<Option<T>, Thrown>;

/// `NumberFormatter_object` past its `zend_object`: the error and the
/// format, `None` until a constructor opens one.
#[derive(Default)]
pub(crate) struct FormatterObject {
    error: IntlError,
    unum: Option<NumberFormat>,
}

impl Carrier for FormatterObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `NumberFormatter_handlers`: `std_object_handlers` with php's clone.
pub(crate) struct Handlers;
pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_formatter)
    }
}

/// `NumberFormatter_object_clone`: the members and `unum_clone`, or
/// php's `Error` once the copy is made.
fn clone_formatter(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let mut status = Code::ZERO;
    let copy = object
        .native::<FormatterObject>(req)
        .and_then(|o| o.unum.as_ref())
        .map(|unum| unum.try_clone(&mut status));
    let held = copy.is_some();
    let unum = copy.flatten();
    let new = clone_with(
        req,
        &object,
        FormatterObject {
            error: IntlError::default(),
            unum,
        },
    )?;
    let message = match held.then_some(()) {
        None => "Cannot clone uninitialized NumberFormatter",
        Some(()) if status.is_failure() => "Failed to clone NumberFormatter",
        Some(()) => return Ok(new),
    };
    new.give_up(req);
    Err(throw_error(req, "Error", message))
}

/// `INTL_METHOD_CHECK_STATUS`: the code on the object and the request;
/// a failure's message set and `None` answered.
fn check(req: &mut Request, object: PhpObject, code: Code, message: &str) -> Answer<()> {
    error::errors_set_code::<FormatterObject>(req, object, code);
    match error::check_object::<FormatterObject>(req, object, message) {
        Ok(()) => Ok(Some(())),
        Err(None) => Ok(None),
        Err(Some(thrown)) => Err(thrown),
    }
}

/// `FORMATTER_METHOD_FETCH_OBJECT` and `body` over the format, taken out
/// of the object for the call and put back after.
fn with<T>(
    req: &mut Request,
    object: PhpObject,
    body: impl FnOnce(&mut Request, &mut NumberFormat) -> Answer<T>,
) -> Answer<T> {
    error::reset_object::<FormatterObject>(req, object);
    let taken = object
        .native_mut::<FormatterObject>(req)
        .and_then(|o| o.unum.take());
    let Some(mut unum) = taken else {
        return Err(throw_error(
            req,
            "Error",
            "Found unconstructed NumberFormatter",
        ));
    };
    let answer = body(req, &mut unum);
    if let Some(o) = object.native_mut::<FormatterObject>(req) {
        o.unum = Some(unum);
    }
    answer
}

/// ICU's preflight as php sizes it: `first` `UChar`s, and on an overflow
/// the full length and `extra` more, the code reset; with no `extra`
/// the second call fills its buffer exactly and warns
/// `U_STRING_NOT_TERMINATED_WARNING`, as php's does.
fn fill(
    first: usize,
    extra: usize,
    mut call: impl FnMut(&mut [u16], &mut Code) -> i32,
) -> (Vec<u16>, Code) {
    let mut buffer = vec![0u16; first];
    let mut status = Code::ZERO;
    let mut len = call(&mut buffer, &mut status);
    let needed = usize::try_from(len).unwrap_or(0);
    if status == Code::BUFFER_OVERFLOW && (extra == 0 || needed >= first) {
        status = Code::ZERO;
        buffer = vec![0u16; needed + extra];
        let again = call(&mut buffer, &mut status);
        if extra > 0 {
            len = again;
        }
    }
    buffer.truncate(usize::try_from(len).unwrap_or(0));
    (buffer, status)
}

/// `INTL_METHOD_RETVAL_UTF8`: the UTF-16 checked under `message` and
/// converted, a failure of either answered `false`.
fn utf8(
    req: &mut Request,
    object: PhpObject,
    (text, code): (Vec<u16>, Code),
    message: &str,
) -> Answer<Owner<PhpString>> {
    if check(req, object, code, message)?.is_none() {
        return Ok(None);
    }
    let converted = ffi::to_utf8(&text);
    let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
    match (
        check(req, object, code, "Error converting value to UTF-8")?,
        converted,
    ) {
        (Some(()), Ok(bytes)) => Ok(Some(PhpString::new(req, &bytes)?)),
        _ => Ok(None),
    }
}

/// `intl_convert_utf8_to_utf16`, its code checked under `message`.
fn utf16(req: &mut Request, object: PhpObject, text: &[u8], message: &str) -> Answer<Vec<u16>> {
    let converted = ffi::to_utf16(text);
    let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
    Ok(check(req, object, code, message)?.and(converted.ok()))
}

/// A `zend_long` as C casts it to `int32_t`.
#[allow(clippy::cast_possible_truncation)]
fn int32(n: i64) -> i32 {
    n as i32
}

fn or_false<T>(answer: Option<T>) -> OrFalse<T> {
    answer.map_or(OrFalse::False, OrFalse::Value)
}

/// `INTL_CHECK_LOCALE_LEN_OR_FAILURE`.
pub(crate) fn locale_too_long(req: &mut Request, len: usize) -> Result<bool, Thrown> {
    if len > 156 {
        let message = "Locale string too long, should be no longer than 156 characters";
        error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(true);
    }
    Ok(false)
}

/// The constructor's ZPP, counted and held where the native says, the
/// constructor's inside its [`error::constructing`] span; the request's
/// error was reset before it (`FORMATTER_METHOD_INIT_VARS`,
/// `before_args`).
fn held(
    req: &mut Request,
    locale: &Raw<PhpString>,
    style: &Raw<i64>,
    pattern: &Raw<Option<PhpString>>,
) -> Result<(PhpString, i64, Option<PhpString>), Thrown> {
    arity(req, 2, Some(3))?;
    Ok((locale.hold(req)?, style.hold(req)?, pattern.hold(req)?))
}

/// `numfmt_ctor`: `Ok(false)` for php's `FAILURE` where nothing was
/// thrown.
fn construct(
    req: &mut Request,
    object: PhpObject,
    (locale, style, pattern): &(PhpString, i64, Option<PhpString>),
) -> Result<bool, Thrown> {
    let (locale, style, pattern) = (
        locale.as_bytes(),
        *style,
        pattern.as_ref().map(PhpString::as_bytes),
    );
    if locale_too_long(req, locale.len())? {
        return Ok(false);
    }
    error::reset_object::<FormatterObject>(req, object);
    if object
        .native::<FormatterObject>(req)
        .is_some_and(|o| o.unum.is_some())
    {
        return Err(throw_error(
            req,
            "Error",
            "NumberFormatter object is already constructed",
        ));
    }
    let mut units = None;
    if let Some(pattern) = pattern.filter(|p| !p.is_empty()) {
        match ffi::to_utf16(pattern) {
            Ok(u) => units = Some(u),
            Err(code) => {
                return ctor_failed(req, object, code, "error converting pattern to UTF-16");
            }
        }
    }
    let locale = if locale.is_empty() {
        crate::default_locale(req)
    } else {
        locale.to_vec()
    };
    let c_locale = ffi::c_string(&locale);
    if iso3_language(&c_locale).is_empty() {
        let tail = format!(
            "\"{}\" is invalid",
            String::from_utf8_lossy(c_locale.to_bytes())
        );
        return Err(argument_error(req, "ValueError", 1, &tail));
    }
    let canonical = ffi::canonicalize(c_locale.to_bytes()).map_or(c_locale, |c| ffi::c_string(&c));
    let mut status = Code::ZERO;
    let unum = NumberFormat::open(int32(style), units.as_deref(), &canonical, &mut status);
    if let Some(o) = object.native_mut::<FormatterObject>(req) {
        o.unum = unum;
    }
    ctor_failed(req, object, status, "number formatter creation failed")
}

/// `INTL_CTOR_CHECK_STATUS`.
fn ctor_failed(
    req: &mut Request,
    object: PhpObject,
    code: Code,
    message: &str,
) -> Result<bool, Thrown> {
    Ok(check(req, object, code, message)?.is_some())
}

/// `numfmt_create`: a fresh `NumberFormatter`, or null where its
/// constructor failed.
fn create(
    req: &mut Request,
    locale: &Raw<PhpString>,
    style: &Raw<i64>,
    pattern: &Raw<Option<PhpString>>,
) -> Result<Option<Owner<Instance<NumberFormatter>>>, Thrown> {
    let held = held(req, locale, style, pattern)?;
    let Some(class) = lookup_class(req, b"NumberFormatter")? else {
        return Err(fatal(req, "InternalError: NumberFormatter is not linked"));
    };
    let object = new_object(req, class)?;
    match construct(req, *object, &held) {
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

/// `numfmt_format`.
fn format(
    req: &mut Request,
    object: PhpObject,
    num: Num,
    kind: i64,
    at: u32,
) -> Answer<Owner<PhpString>> {
    with(req, object, |req, unum| {
        let kind = match (kind, &num) {
            (0, Union::A(_)) => 2,
            (0, Union::B(_)) => 3,
            (kind, _) => kind,
        };
        let value = match num {
            Union::A(n) => Value::int(n),
            Union::B(d) => Value::float(d),
        };
        #[allow(clippy::cast_possible_truncation)]
        let number = match kind {
            1 => Number::Int32(int32(to_long(req, &value)?)),
            2 => Number::Int64(match num {
                Union::A(n) => n,
                Union::B(d) => d as i64,
            }),
            3 => Number::Double(to_double(req, &value)?),
            4 => return Err(currency_type(req, at, "format")),
            _ => {
                return Err(argument_error(
                    req,
                    "ValueError",
                    2 + at,
                    "must be a NumberFormatter::TYPE_* constant",
                ));
            }
        };
        let filled = fill(32, 0, |out, status| unum.format(number, out, status));
        utf8(req, object, filled, "Number formatting failed")
    })
}

/// `NumberFormatter::TYPE_CURRENCY` refused, as `numfmt_format` and
/// `numfmt_parse` word it.
fn currency_type(req: &mut Request, at: u32, verb: &str) -> Thrown {
    let tail = if at == 0 {
        format!(
            "cannot be NumberFormatter::TYPE_CURRENCY constant, use NumberFormatter::{verb}Currency() method instead"
        )
    } else {
        format!(
            "cannot be NumberFormatter::TYPE_CURRENCY constant, use numfmt_{verb}_currency() function instead"
        )
    };
    argument_error(req, "ValueError", 2 + at, &tail)
}

/// The `&$offset` php reads: a given one not null, as `z!` holds it.
fn offset(req: &mut Request, given: &Ref<Untyped<Value>>) -> Result<Option<i32>, Thrown> {
    match given.get() {
        Some(Untyped(v)) if !matches!(v.as_ref(), php_ext::values::ValueRef::Null) => {
            Ok(Some(int32(to_long(req, v)?)))
        }
        _ => Ok(None),
    }
}

/// `intl_convert_utf8_offset_to_utf16` of a given offset, which ICU
/// reads in code units, refused with "Invalid UTF-8 offset" where it
/// falls inside a character (GH-23094); the outer `None` is that refusal.
fn utf16_position(
    req: &mut Request,
    object: PhpObject,
    text: &[u8],
    position: Option<i32>,
) -> Answer<Option<i32>> {
    let Some(p) = position else {
        return Ok(Some(None));
    };
    match ffi::utf8_offset_to_utf16(text, p) {
        Ok(units) => Ok(Some(Some(units))),
        Err(code) => check(req, object, code, "Invalid UTF-8 offset").map(|_| None),
    }
}

/// `numfmt_parse`.
fn parse(
    req: &mut Request,
    object: PhpObject,
    text: PhpString,
    kind: i64,
    at_offset: &Ref<Untyped<Value>>,
    at: u32,
) -> Answer<Union<i64, f64>> {
    let position = offset(req, at_offset)?;
    with(req, object, |req, unum| {
        let Some(units) = utf16(
            req,
            object,
            text.as_bytes(),
            "String conversion to UTF-16 failed",
        )?
        else {
            return Ok(None);
        };
        let Some(mut position) = utf16_position(req, object, text.as_bytes(), position)? else {
            return Ok(None);
        };
        let mut status = Code::ZERO;
        let parsed = match kind {
            1..=3 => unum.parse(kind, &units, position.as_mut(), &mut status),
            4 => return Err(currency_type(req, at, "parse")),
            _ => {
                return Err(argument_error(
                    req,
                    "ValueError",
                    2 + at,
                    "must be a NumberFormatter::TYPE_* constant",
                ));
            }
        };
        if let Some(p) = position {
            let p = ffi::utf16_offset_to_utf8(&units, p);
            at_offset.set(req, Untyped(Value::int(i64::from(p))))?;
        }
        check(req, object, status, "Number parsing failed")?.map_or(Ok(None), |()| {
            Ok(Some(match parsed {
                Parsed::Int32(n) => Union::A(i64::from(n)),
                Parsed::Int64(n) => Union::A(n),
                Parsed::Double(d) => Union::B(d),
            }))
        })
    })
}

/// `numfmt_format_currency`.
fn format_currency(
    req: &mut Request,
    object: PhpObject,
    amount: f64,
    currency: PhpString,
) -> Answer<Owner<PhpString>> {
    with(req, object, |req, unum| {
        let Some(mut code) = utf16(
            req,
            object,
            currency.as_bytes(),
            "Currency conversion to UTF-16 failed",
        )?
        else {
            return Ok(None);
        };
        code.push(0);
        let number = Number::Currency(amount, code.as_ptr());
        let filled = fill(32, 0, |out, status| unum.format(number, out, status));
        utf8(req, object, filled, "Number formatting failed")
    })
}

/// `numfmt_parse_currency`.
fn parse_currency(
    req: &mut Request,
    object: PhpObject,
    text: PhpString,
    currency: &Ref<Untyped<Value>>,
    at_offset: &Ref<Untyped<Value>>,
) -> Answer<f64> {
    with(req, object, |req, unum| {
        let Some(units) = utf16(
            req,
            object,
            text.as_bytes(),
            "String conversion to UTF-16 failed",
        )?
        else {
            return Ok(None);
        };
        let position = offset(req, at_offset)?;
        let Some(mut position) = utf16_position(req, object, text.as_bytes(), position)? else {
            return Ok(None);
        };
        let mut code = [0u16; 5];
        let mut status = Code::ZERO;
        let number = unum.parse_currency(&units, position.as_mut(), &mut code, &mut status);
        if let Some(p) = position {
            let p = ffi::utf16_offset_to_utf8(&units, p);
            at_offset.set(req, Untyped(Value::int(i64::from(p))))?;
        }
        if check(req, object, status, "Number parsing failed")?.is_none() {
            return Ok(None);
        }
        let end = code.iter().position(|&u| u == 0).unwrap_or(code.len());
        let Some(text) = utf8(
            req,
            object,
            (code[..end].to_vec(), status),
            "Currency conversion to UTF-8 failed",
        )?
        else {
            return Ok(None);
        };
        currency.set(req, Untyped(text.into_value()))?;
        Ok(Some(number))
    })
}

/// The attributes `unum_getAttribute` answers as an integer.
const INT_ATTRIBUTES: std::ops::RangeInclusive<i64> = 0..=19;
/// `UNUM_ROUNDING_INCREMENT`, the one answered as a double.
const ROUNDING_INCREMENT: i64 = 12;

/// `numfmt_get_attribute`.
fn get_attribute(req: &mut Request, object: PhpObject, attribute: i64) -> Answer<Union<i64, f64>> {
    with(req, object, |req, unum| {
        let value = match attribute {
            ROUNDING_INCREMENT => Some(unum.double_attribute(12))
                .filter(|&d| d.to_bits() != (-1.0f64).to_bits())
                .map(Union::B),
            a if INT_ATTRIBUTES.contains(&a) => Some(unum.attribute(int32(a)))
                .filter(|&n| n != -1)
                .map(|n| Union::A(i64::from(n))),
            _ => None,
        };
        let code = if value.is_some() {
            Code::ZERO
        } else {
            Code::UNSUPPORTED
        };
        Ok(check(req, object, code, "Error getting attribute value")?.and(value))
    })
}

/// `numfmt_set_attribute`.
fn set_attribute(req: &mut Request, object: PhpObject, attribute: i64, value: &Num) -> Answer<()> {
    let value = match value {
        Union::A(n) => Value::int(*n),
        Union::B(d) => Value::float(*d),
    };
    with(req, object, |req, unum| {
        let code = match attribute {
            ROUNDING_INCREMENT => {
                unum.set_double_attribute(12, to_double(req, &value)?);
                Code::ZERO
            }
            a if INT_ATTRIBUTES.contains(&a) => {
                unum.set_attribute(int32(a), int32(to_long(req, &value)?));
                Code::ZERO
            }
            _ => Code::UNSUPPORTED,
        };
        check(req, object, code, "Error setting attribute value")
    })
}

/// The three text getters: a text attribute (0), a symbol (1) or the
/// pattern (2), each in php's first buffer.
fn get_text(
    req: &mut Request,
    object: PhpObject,
    which: u8,
    key: i64,
    message: &str,
) -> Answer<Owner<PhpString>> {
    if which == 1 && !(0..29).contains(&key) {
        error::set(req, Code::ILLEGAL_ARGUMENT, "invalid symbol value")?;
        return Ok(None);
    }
    with(req, object, |req, unum| {
        let first = if which == 1 { 4 } else { 64 };
        let filled = fill(first, 1, |out, status| {
            unum.text(which, int32(key), out, status)
        });
        utf8(req, object, filled, message)
    })
}

/// The three text setters, as [`get_text`] names them.
fn set_text(
    req: &mut Request,
    object: PhpObject,
    which: u8,
    key: i64,
    value: PhpString,
    messages: [&str; 2],
) -> Answer<()> {
    if which == 1 && !(0..29).contains(&key) {
        error::set(req, Code::ILLEGAL_ARGUMENT, "invalid symbol value")?;
        return Ok(None);
    }
    with(req, object, |req, unum| {
        let Some(units) = utf16(req, object, value.as_bytes(), messages[0])? else {
            return Ok(None);
        };
        let mut status = Code::ZERO;
        let mut stopped = ParseError::default();
        unum.set_text(which, int32(key), &units, &mut stopped, &mut status);
        if which == 2 && status.is_failure() {
            if let Some(o) = object.native_mut::<FormatterObject>(req) {
                o.error.code = status;
            }
            let message = format!(
                "Error setting pattern value at line {}, offset {}",
                stopped.line, stopped.offset
            );
            error::errors_set_custom_msg::<FormatterObject>(req, object, &message)?;
            return Ok(None);
        }
        check(req, object, status, messages[1])
    })
}

/// `numfmt_get_locale`.
fn get_locale(req: &mut Request, object: PhpObject, kind: i64) -> Answer<Owner<PhpString>> {
    with(req, object, |req, unum| {
        let mut status = Code::ZERO;
        let locale = unum.locale(int32(kind), &mut status).unwrap_or("");
        if check(req, object, status, "Error getting locale")?.is_none() {
            return Ok(None);
        }
        Ok(Some(PhpString::new(req, locale.as_bytes())?))
    })
}

/// `numfmt_get_error_code`.
fn error_code(req: &mut Request, object: PhpObject) -> i64 {
    i64::from(error::object_code::<FormatterObject>(req, object).0)
}

/// `numfmt_get_error_message`.
fn error_message(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let message = error::object_message::<FormatterObject>(req, object);
    PhpString::new(req, &message)
}

/// `class NumberFormatter`: one `UNumberFormat` each, cloned with the
/// object; one `new` without a constructor has none.
///
/// ```php
/// $f = new NumberFormatter('en_US', NumberFormatter::DECIMAL);
/// $g = clone $f; $g->setAttribute(NumberFormatter::FRACTION_DIGITS, 2);
/// var_dump($f->format(1234.5), $g->format(1234.5), $f->getLocale());
/// $u = (new ReflectionClass('NumberFormatter'))->newInstanceWithoutConstructor();
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $u->format(1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $f->__construct('en', 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new NumberFormatter('en', 99); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { new NumberFormatter('root', 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(numfmt_create('en', 99), intl_get_error_message(), NumberFormatter::create('de', NumberFormatter::PERCENT)->format(0.5));
/// var_dump($f->parse('1,5'), $f->formatCurrency(2, 'USD'), $f->parseCurrency('$3.00', $c), $c, $f->getAttribute(NumberFormatter::GROUPING_SIZE));
/// var_dump($f->setTextAttribute(NumberFormatter::POSITIVE_PREFIX, '+'), $f->getTextAttribute(NumberFormatter::POSITIVE_PREFIX), $f->setSymbol(NumberFormatter::MINUS_SIGN_SYMBOL, '~'), $f->getSymbol(NumberFormatter::MINUS_SIGN_SYMBOL));
/// var_dump($f->setPattern('0.0'), $f->getPattern(), $f->format(-2), $f->getErrorCode(), $f->getErrorMessage());
/// ```
/// ```expect
/// string(7) "1,234.5"
/// string(8) "1,234.50"
/// string(5) "en_US"
/// Cannot clone uninitialized NumberFormatter
/// Found unconstructed NumberFormatter
/// NumberFormatter object is already constructed
/// NumberFormatter::__construct(): number formatter creation failed
/// NumberFormatter::__construct(): Argument #1 ($locale) "root" is invalid
/// NULL
/// string(70) "numfmt_create(): number formatter creation failed: U_UNSUPPORTED_ERROR"
/// string(5) "50 %"
/// bool(false)
/// string(1) "2"
/// float(3)
/// string(3) "USD"
/// int(3)
/// bool(true)
/// string(1) "+"
/// bool(true)
/// string(1) "~"
/// bool(true)
/// string(11) "'+'0.0;-0.0"
/// string(4) "~2.0"
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// ```
#[php_class(not_serializable, size = 80, payload = FormatterObject, handlers = crate::formatter::HANDLERS)]
pub(crate) struct NumberFormatter;

#[php_class]
#[allow(non_snake_case)]
impl NumberFormatter {
    const PATTERN_DECIMAL: i64 = 0;
    const DECIMAL: i64 = 1;
    const DECIMAL_COMPACT_SHORT: i64 = 14;
    const DECIMAL_COMPACT_LONG: i64 = 15;
    const CURRENCY: i64 = 2;
    const PERCENT: i64 = 3;
    const SCIENTIFIC: i64 = 4;
    const SPELLOUT: i64 = 5;
    const ORDINAL: i64 = 6;
    const DURATION: i64 = 7;
    const PATTERN_RULEBASED: i64 = 9;
    const IGNORE: i64 = 0;
    const CURRENCY_ISO: i64 = 10;
    const CURRENCY_PLURAL: i64 = 11;
    const CURRENCY_ACCOUNTING: i64 = 12;
    const CASH_CURRENCY: i64 = 13;
    const CURRENCY_STANDARD: i64 = 16;
    const DEFAULT_STYLE: i64 = 1;
    const ROUND_CEILING: i64 = 0;
    const ROUND_FLOOR: i64 = 1;
    const ROUND_DOWN: i64 = 2;
    const ROUND_UP: i64 = 3;
    const ROUND_TOWARD_ZERO: i64 = 2;
    const ROUND_AWAY_FROM_ZERO: i64 = 3;
    const ROUND_HALFEVEN: i64 = 4;
    const ROUND_HALFODD: i64 = 8;
    const ROUND_HALFDOWN: i64 = 5;
    const ROUND_HALFUP: i64 = 6;
    const PAD_BEFORE_PREFIX: i64 = 0;
    const PAD_AFTER_PREFIX: i64 = 1;
    const PAD_BEFORE_SUFFIX: i64 = 2;
    const PAD_AFTER_SUFFIX: i64 = 3;
    const PARSE_INT_ONLY: i64 = 0;
    const GROUPING_USED: i64 = 1;
    const DECIMAL_ALWAYS_SHOWN: i64 = 2;
    const MAX_INTEGER_DIGITS: i64 = 3;
    const MIN_INTEGER_DIGITS: i64 = 4;
    const INTEGER_DIGITS: i64 = 5;
    const MAX_FRACTION_DIGITS: i64 = 6;
    const MIN_FRACTION_DIGITS: i64 = 7;
    const FRACTION_DIGITS: i64 = 8;
    const MULTIPLIER: i64 = 9;
    const GROUPING_SIZE: i64 = 10;
    const ROUNDING_MODE: i64 = 11;
    const ROUNDING_INCREMENT: i64 = 12;
    const FORMAT_WIDTH: i64 = 13;
    const PADDING_POSITION: i64 = 14;
    const SECONDARY_GROUPING_SIZE: i64 = 15;
    const SIGNIFICANT_DIGITS_USED: i64 = 16;
    const MIN_SIGNIFICANT_DIGITS: i64 = 17;
    const MAX_SIGNIFICANT_DIGITS: i64 = 18;
    const LENIENT_PARSE: i64 = 19;
    const POSITIVE_PREFIX: i64 = 0;
    const POSITIVE_SUFFIX: i64 = 1;
    const NEGATIVE_PREFIX: i64 = 2;
    const NEGATIVE_SUFFIX: i64 = 3;
    const PADDING_CHARACTER: i64 = 4;
    const CURRENCY_CODE: i64 = 5;
    const DEFAULT_RULESET: i64 = 6;
    const PUBLIC_RULESETS: i64 = 7;
    const DECIMAL_SEPARATOR_SYMBOL: i64 = 0;
    const GROUPING_SEPARATOR_SYMBOL: i64 = 1;
    const PATTERN_SEPARATOR_SYMBOL: i64 = 2;
    const PERCENT_SYMBOL: i64 = 3;
    const ZERO_DIGIT_SYMBOL: i64 = 4;
    const DIGIT_SYMBOL: i64 = 5;
    const MINUS_SIGN_SYMBOL: i64 = 6;
    const PLUS_SIGN_SYMBOL: i64 = 7;
    const CURRENCY_SYMBOL: i64 = 8;
    const INTL_CURRENCY_SYMBOL: i64 = 9;
    const MONETARY_SEPARATOR_SYMBOL: i64 = 10;
    const EXPONENTIAL_SYMBOL: i64 = 11;
    const PERMILL_SYMBOL: i64 = 12;
    const PAD_ESCAPE_SYMBOL: i64 = 13;
    const INFINITY_SYMBOL: i64 = 14;
    const NAN_SYMBOL: i64 = 15;
    const SIGNIFICANT_DIGIT_SYMBOL: i64 = 16;
    const MONETARY_GROUPING_SEPARATOR_SYMBOL: i64 = 17;
    const TYPE_DEFAULT: i64 = 0;
    const TYPE_INT32: i64 = 1;
    const TYPE_INT64: i64 = 2;
    const TYPE_DOUBLE: i64 = 3;
    #[php(deprecated(since = "8.3"))]
    const TYPE_CURRENCY: i64 = 4;

    #[php(arity = manual, before_args = error::reset)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        locale: Raw<PhpString>,
        style: Raw<i64>,
        #[php(default = null)] pattern: Raw<Option<PhpString>>,
    ) -> Result<(), Thrown> {
        let object = this.object();
        error::constructing(req, |req| {
            let held = held(req, &locale, &style, &pattern)?;
            construct(req, object, &held).map(|_| ())
        })
    }

    #[php(tentative, arity = manual, before_args = error::reset)]
    fn create(
        req: &mut Request,
        locale: Raw<PhpString>,
        style: Raw<i64>,
        #[php(default = null)] pattern: Raw<Option<PhpString>>,
    ) -> Result<Option<Owner<Instance<NumberFormatter>>>, Thrown> {
        create(req, &locale, &style, &pattern)
    }

    #[php(tentative, before_args = error::reset)]
    fn format(
        req: &mut Request,
        this: This<Self>,
        num: Num,
        #[php(default = NumberFormatter::TYPE_DEFAULT)] r#type: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        format(req, this.object(), num, r#type, 0).map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn parse(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        #[php(default = NumberFormatter::TYPE_DOUBLE)] r#type: i64,
        #[php(default = null)] offset: Ref<Untyped<Value>>,
    ) -> Result<OrFalse<Union<i64, f64>>, Thrown> {
        parse(req, this.object(), string, r#type, &offset, 0).map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn formatCurrency(
        req: &mut Request,
        this: This<Self>,
        amount: f64,
        currency: PhpString,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        format_currency(req, this.object(), amount, currency).map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn parseCurrency(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        currency: Ref<Untyped<Value>>,
        #[php(default = null)] offset: Ref<Untyped<Value>>,
    ) -> Result<OrFalse<f64>, Thrown> {
        parse_currency(req, this.object(), string, &currency, &offset).map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn setAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
        value: Num,
    ) -> Result<bool, Thrown> {
        set_attribute(req, this.object(), attribute, &value).map(|a| a.is_some())
    }

    #[php(tentative, before_args = error::reset)]
    fn getAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
    ) -> Result<OrFalse<Union<i64, f64>>, Thrown> {
        get_attribute(req, this.object(), attribute).map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn setTextAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
        value: PhpString,
    ) -> Result<bool, Thrown> {
        set_text(req, this.object(), 0, attribute, value, TEXT_SETTER).map(|a| a.is_some())
    }

    #[php(tentative, before_args = error::reset)]
    fn getTextAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_text(
            req,
            this.object(),
            0,
            attribute,
            "Error getting attribute value",
        )
        .map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn setSymbol(
        req: &mut Request,
        this: This<Self>,
        symbol: i64,
        value: PhpString,
    ) -> Result<bool, Thrown> {
        set_text(req, this.object(), 1, symbol, value, SYMBOL_SETTER).map(|a| a.is_some())
    }

    #[php(tentative, before_args = error::reset)]
    fn getSymbol(
        req: &mut Request,
        this: This<Self>,
        symbol: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_text(req, this.object(), 1, symbol, "Error getting symbol value").map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn setPattern(req: &mut Request, this: This<Self>, pattern: PhpString) -> Result<bool, Thrown> {
        set_text(req, this.object(), 2, 0, pattern, PATTERN_SETTER).map(|a| a.is_some())
    }

    #[php(tentative, before_args = error::reset)]
    fn getPattern(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_text(req, this.object(), 2, 0, "Error getting formatter pattern").map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn getLocale(
        req: &mut Request,
        this: This<Self>,
        #[php(default = ULOC_ACTUAL_LOCALE)] r#type: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_locale(req, this.object(), r#type).map(or_false)
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        error_code(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        error_message(req, this.object())
    }
}

const TEXT_SETTER: [&str; 2] = [
    "Error converting attribute value to UTF-16",
    "Error setting text attribute",
];
const SYMBOL_SETTER: [&str; 2] = [
    "Error converting symbol value to UTF-16",
    "Error setting symbol value",
];
const PATTERN_SETTER: [&str; 2] = ["Error converting pattern to UTF-16", ""];

type Fmt = Instance<NumberFormatter>;

/// `numfmt_create(string $locale, int $style, ?string $pattern = null): ?NumberFormatter`.
/// `numfmt_create`: null where the constructor fails.
///
/// ```php
/// var_dump(numfmt_create('en', NumberFormatter::DECIMAL) instanceof NumberFormatter, numfmt_create(str_repeat('a', 157), 1), intl_get_error_message());
/// ```
/// ```expect
/// bool(true)
/// NULL
/// string(106) "numfmt_create(): Locale string too long, should be no longer than 156 characters: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(arity = manual, before_args = error::reset)]
fn numfmt_create(
    req: &mut Request,
    locale: Raw<PhpString>,
    style: Raw<i64>,
    #[php(default = null)] pattern: Raw<Option<PhpString>>,
) -> Result<Option<Owner<Fmt>>, Thrown> {
    create(req, &locale, &style, &pattern)
}

/// `numfmt_format`.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::DECIMAL);
/// var_dump(numfmt_format($f, 1234567.891), numfmt_format($f, 3000000000, NumberFormatter::TYPE_INT32), numfmt_format($f, 1.5, NumberFormatter::TYPE_INT64));
/// try { numfmt_format($f, 1, NumberFormatter::TYPE_CURRENCY); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $f->format(1, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $s = numfmt_create('en', NumberFormatter::SPELLOUT); var_dump(numfmt_format($s, 123456789), intl_get_error_code());
/// ```
/// ```expect
/// string(13) "1,234,567.891"
/// string(14) "-1,294,967,296"
/// string(1) "1"
///
/// Deprecated: Constant NumberFormatter::TYPE_CURRENCY is deprecated since 8.3 in %s on line 4
/// numfmt_format(): Argument #3 ($type) cannot be NumberFormatter::TYPE_CURRENCY constant, use numfmt_format_currency() function instead
/// NumberFormatter::format(): Argument #2 ($type) must be a NumberFormatter::TYPE_* constant
/// string(90) "one hundred twenty-three million four hundred fifty-six thousand seven hundred eighty-nine"
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_format(
    req: &mut Request,
    formatter: Fmt,
    num: Num,
    #[php(default = NumberFormatter::TYPE_DEFAULT)] r#type: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    format(req, formatter.object(), num, r#type, 1).map(or_false)
}

/// `numfmt_parse`, the offset read and written back where given.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::DECIMAL);
/// $p = 2; var_dump(numfmt_parse($f, 'x 1,234.5 y', NumberFormatter::TYPE_DOUBLE, $p), $p);
/// var_dump($f->parse('12.7', NumberFormatter::TYPE_INT32), $f->parse('abc'), intl_get_error_message());
/// ```
/// ```expect
/// float(1234.5)
/// int(9)
/// int(12)
/// bool(false)
/// string(62) "NumberFormatter::parse(): Number parsing failed: U_PARSE_ERROR"
/// ```
///
/// The offset is in bytes of the UTF-8 text, read into ICU's code units
/// and written back from them; one inside a character is refused and
/// left as given (GH-23094). `parseCurrency` reads it the same way.
///
/// ```php
/// $f = new NumberFormatter('en_US', NumberFormatter::DECIMAL);
/// $at = 4;
/// var_dump($f->parse("😀123", NumberFormatter::TYPE_INT32, $at), $at);
/// $at = 1;
/// var_dump($f->parse("é123", NumberFormatter::TYPE_INT32, $at), $at, $f->getErrorMessage());
/// $f = new NumberFormatter('en_US', NumberFormatter::CURRENCY);
/// $at = 4;
/// var_dump($f->parseCurrency("😀$1.5", $currency, $at), $currency, $at);
/// ```
/// ```expect
/// int(123)
/// int(7)
/// bool(false)
/// int(1)
/// string(68) "NumberFormatter::parse(): Invalid UTF-8 offset: U_INVALID_CHAR_FOUND"
/// float(1.5)
/// string(3) "USD"
/// int(8)
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_parse(
    req: &mut Request,
    formatter: Fmt,
    string: PhpString,
    #[php(default = NumberFormatter::TYPE_DOUBLE)] r#type: i64,
    #[php(default = null)] offset: Ref<Untyped<Value>>,
) -> Result<OrFalse<Union<i64, f64>>, Thrown> {
    parse(req, formatter.object(), string, r#type, &offset, 1).map(or_false)
}

/// `numfmt_format_currency`.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::CURRENCY);
/// var_dump(numfmt_format_currency($f, 1234.5, 'EUR'), $f->formatCurrency(-3, 'JPY'));
/// ```
/// ```expect
/// string(11) "€1,234.50"
/// string(4) "-¥3"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_format_currency(
    req: &mut Request,
    formatter: Fmt,
    amount: f64,
    currency: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    format_currency(req, formatter.object(), amount, currency).map(or_false)
}

/// `numfmt_parse_currency`, the currency written back.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::CURRENCY);
/// var_dump(numfmt_parse_currency($f, '€1,234.50', $c), $c, $f->parseCurrency('nope', $d), $d ?? 'unset');
/// ```
/// ```expect
/// float(1234.5)
/// string(3) "EUR"
/// bool(false)
/// string(5) "unset"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_parse_currency(
    req: &mut Request,
    formatter: Fmt,
    string: PhpString,
    currency: Ref<Untyped<Value>>,
    #[php(default = null)] offset: Ref<Untyped<Value>>,
) -> Result<OrFalse<f64>, Thrown> {
    parse_currency(req, formatter.object(), string, &currency, &offset).map(or_false)
}

/// `numfmt_set_attribute` and `numfmt_get_attribute`.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::DECIMAL);
/// var_dump(numfmt_set_attribute($f, NumberFormatter::MAX_FRACTION_DIGITS, 1.9), numfmt_get_attribute($f, NumberFormatter::MAX_FRACTION_DIGITS));
/// var_dump($f->setAttribute(NumberFormatter::ROUNDING_INCREMENT, 0.5), $f->getAttribute(NumberFormatter::ROUNDING_INCREMENT), $f->format(1.3));
/// var_dump($f->setAttribute(99, 1), $f->getAttribute(99), $f->getErrorMessage());
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// bool(true)
/// float(0.5)
/// string(3) "1.5"
/// bool(false)
/// bool(false)
/// string(83) "NumberFormatter::getAttribute(): Error getting attribute value: U_UNSUPPORTED_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_set_attribute(
    req: &mut Request,
    formatter: Fmt,
    attribute: i64,
    value: Num,
) -> Result<bool, Thrown> {
    set_attribute(req, formatter.object(), attribute, &value).map(|a| a.is_some())
}

/// `numfmt_get_attribute`.
///
/// ```php
/// var_dump(numfmt_get_attribute(numfmt_create('en', NumberFormatter::PERCENT), NumberFormatter::MAX_FRACTION_DIGITS));
/// ```
/// ```expect
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_get_attribute(
    req: &mut Request,
    formatter: Fmt,
    attribute: i64,
) -> Result<OrFalse<Union<i64, f64>>, Thrown> {
    get_attribute(req, formatter.object(), attribute).map(or_false)
}

/// `numfmt_set_text_attribute` and `numfmt_get_text_attribute`.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::DECIMAL);
/// var_dump(numfmt_set_text_attribute($f, NumberFormatter::NEGATIVE_PREFIX, '_'), numfmt_get_text_attribute($f, NumberFormatter::NEGATIVE_PREFIX), $f->format(-5));
/// var_dump($f->getTextAttribute(NumberFormatter::DEFAULT_RULESET), $f->getErrorCode());
/// ```
/// ```expect
/// bool(true)
/// string(1) "_"
/// string(2) "_5"
/// bool(false)
/// int(16)
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_set_text_attribute(
    req: &mut Request,
    formatter: Fmt,
    attribute: i64,
    value: PhpString,
) -> Result<bool, Thrown> {
    set_text(req, formatter.object(), 0, attribute, value, TEXT_SETTER).map(|a| a.is_some())
}

/// `numfmt_get_text_attribute`.
///
/// ```php
/// var_dump(numfmt_get_text_attribute(numfmt_create('en', NumberFormatter::CURRENCY), NumberFormatter::CURRENCY_CODE));
/// ```
/// ```expect
/// string(3) "XXX"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_get_text_attribute(
    req: &mut Request,
    formatter: Fmt,
    attribute: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_text(
        req,
        formatter.object(),
        0,
        attribute,
        "Error getting attribute value",
    )
    .map(or_false)
}

/// `numfmt_set_symbol` and `numfmt_get_symbol`, a symbol out of ICU's
/// range refused before the object is read.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::DECIMAL);
/// var_dump(numfmt_set_symbol($f, NumberFormatter::GROUPING_SEPARATOR_SYMBOL, '*'), numfmt_get_symbol($f, NumberFormatter::GROUPING_SEPARATOR_SYMBOL), $f->format(12345));
/// var_dump($f->getSymbol(NumberFormatter::INFINITY_SYMBOL), $f->setSymbol(-1, 'x'), intl_get_error_message());
/// ```
/// ```expect
/// bool(true)
/// string(1) "*"
/// string(6) "12*345"
/// string(3) "∞"
/// bool(false)
/// string(76) "NumberFormatter::setSymbol(): invalid symbol value: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_set_symbol(
    req: &mut Request,
    formatter: Fmt,
    symbol: i64,
    value: PhpString,
) -> Result<bool, Thrown> {
    set_text(req, formatter.object(), 1, symbol, value, SYMBOL_SETTER).map(|a| a.is_some())
}

/// `numfmt_get_symbol`.
///
/// ```php
/// var_dump(numfmt_get_symbol(numfmt_create('de', NumberFormatter::DECIMAL), NumberFormatter::DECIMAL_SEPARATOR_SYMBOL));
/// ```
/// ```expect
/// string(1) ","
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_get_symbol(
    req: &mut Request,
    formatter: Fmt,
    symbol: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_text(
        req,
        formatter.object(),
        1,
        symbol,
        "Error getting symbol value",
    )
    .map(or_false)
}

/// `numfmt_set_pattern` and `numfmt_get_pattern`.
///
/// ```php
/// $f = numfmt_create('en_US', NumberFormatter::DECIMAL);
/// var_dump(numfmt_get_pattern($f), numfmt_set_pattern($f, '#,##0.00 ¤'), $f->getPattern(), $f->format(3));
/// var_dump($f->setPattern("0'"), $f->getErrorMessage());
/// ```
/// ```expect
/// string(9) "#,##0.###"
/// bool(true)
/// string(11) "#,##0.00 ¤"
/// string(6) "3.00 $"
/// bool(false)
/// string(102) "NumberFormatter::setPattern(): Error setting pattern value at line 0, offset 0: U_PATTERN_SYNTAX_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_set_pattern(
    req: &mut Request,
    formatter: Fmt,
    pattern: PhpString,
) -> Result<bool, Thrown> {
    set_text(req, formatter.object(), 2, 0, pattern, PATTERN_SETTER).map(|a| a.is_some())
}

/// `numfmt_get_pattern`.
///
/// ```php
/// var_dump(numfmt_get_pattern(numfmt_create('en', NumberFormatter::PERCENT)));
/// ```
/// ```expect
/// string(6) "#,##0%"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_get_pattern(
    req: &mut Request,
    formatter: Fmt,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_text(
        req,
        formatter.object(),
        2,
        0,
        "Error getting formatter pattern",
    )
    .map(or_false)
}

/// `numfmt_get_locale`.
///
/// ```php
/// $f = numfmt_create('de-CH', NumberFormatter::DECIMAL);
/// var_dump(numfmt_get_locale($f), $f->getLocale(ULOC_VALID_LOCALE), $f->getLocale(9));
/// ```
/// ```expect
/// string(5) "de_CH"
/// string(5) "de_CH"
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_get_locale(
    req: &mut Request,
    formatter: Fmt,
    #[php(default = ULOC_ACTUAL_LOCALE)] r#type: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_locale(req, formatter.object(), r#type).map(or_false)
}

/// `numfmt_get_error_code` and `numfmt_get_error_message`: the object's.
///
/// ```php
/// $f = numfmt_create('en', NumberFormatter::DECIMAL); $f->parse('x');
/// var_dump(numfmt_get_error_code($f), numfmt_get_error_message($f));
/// ```
/// ```expect
/// int(9)
/// string(62) "NumberFormatter::parse(): Number parsing failed: U_PARSE_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_get_error_code(req: &mut Request, formatter: Fmt) -> i64 {
    error_code(req, formatter.object())
}

/// `numfmt_get_error_message`.
///
/// ```php
/// var_dump(numfmt_get_error_message(numfmt_create('en', NumberFormatter::DECIMAL)));
/// ```
/// ```expect
/// string(23) "U_USING_DEFAULT_WARNING"
/// ```
#[php_function(before_args = error::reset)]
fn numfmt_get_error_message(req: &mut Request, formatter: Fmt) -> Result<Owner<PhpString>, Thrown> {
    error_message(req, formatter.object())
}

/// The `numfmt_*` block of `php_intl.stub.php`.
pub(crate) fn functions(b: php_ext::abi::ExtensionBuilder) -> php_ext::abi::ExtensionBuilder {
    b.functions(vec![
        numfmt_create::declared(),
        numfmt_format::declared(),
        numfmt_parse::declared(),
        numfmt_format_currency::declared(),
        numfmt_parse_currency::declared(),
        numfmt_set_attribute::declared(),
        numfmt_get_attribute::declared(),
        numfmt_set_text_attribute::declared(),
        numfmt_get_text_attribute::declared(),
        numfmt_set_symbol::declared(),
        numfmt_get_symbol::declared(),
        numfmt_set_pattern::declared(),
        numfmt_get_pattern::declared(),
        numfmt_get_locale::declared(),
        numfmt_get_error_code::declared(),
        numfmt_get_error_message::declared(),
    ])
}
