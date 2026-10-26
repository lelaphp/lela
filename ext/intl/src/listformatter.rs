//! `listformatter/`: `IntlListFormatter` over ICU's `ulistfmt_*`.

use php_ext::abi::{OrFalse, Refusals, This};
use php_ext::attr::php_class;
use php_ext::coerce::with_string;
use php_ext::raising::{Thrown, argument_error, throw_error};
use php_ext::request::Request;
use std::ops::ControlFlow;

use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString};

use crate::error::{self, Carrier, IntlError};
use crate::ffi::formatter::iso3_language;
use crate::ffi::listformatter::ListFormat;
use crate::ffi::{self, Code};

/// `ListFormatter_object` past its `zend_object`: the error and the
/// format, `None` until the constructor opens one.
#[derive(Default)]
pub(crate) struct ListFormatterObject {
    error: IntlError,
    format: Option<ListFormat>,
}

impl Carrier for ListFormatterObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `listformatter_handlers`: `std_object_handlers` with no clone.
pub(crate) const HANDLERS: Refusals = Refusals::NONE.cloning();

/// `INTL_MAX_LOCALE_LEN`.
const MAX_LOCALE_LEN: usize = 156;

/// `PHP_METHOD(IntlListFormatter, __construct)`.
fn construct(
    req: &mut Request,
    object: PhpObject,
    locale: &[u8],
    kind: i64,
    width: i64,
) -> Result<(), Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    if object
        .native::<ListFormatterObject>(req)
        .is_some_and(|o| o.format.is_some())
    {
        return Err(throw_error(
            req,
            ce.error,
            "IntlListFormatter object is already constructed",
        ));
    }
    if locale.len() > MAX_LOCALE_LEN {
        let tail = format!("must be less than or equal to {MAX_LOCALE_LEN} characters");
        return Err(argument_error(req, ce.value_error, 1, &tail));
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
        return Err(argument_error(req, ce.value_error, 1, &tail));
    }
    if !(0..=2).contains(&kind) {
        let tail = "must be one of IntlListFormatter::TYPE_AND, IntlListFormatter::TYPE_OR, or IntlListFormatter::TYPE_UNITS";
        return Err(argument_error(req, ce.value_error, 2, tail));
    }
    if !(0..=2).contains(&width) {
        let tail = "must be one of IntlListFormatter::WIDTH_WIDE, IntlListFormatter::WIDTH_SHORT, or IntlListFormatter::WIDTH_NARROW";
        return Err(argument_error(req, ce.value_error, 3, tail));
    }
    let mut status = Code::ZERO;
    #[allow(clippy::cast_possible_truncation)]
    let format = ListFormat::open(&c_locale, kind as i32, width as i32, &mut status);
    if let Some(o) = object.native_mut::<ListFormatterObject>(req) {
        o.format = format;
    }
    if status.is_failure() {
        error::set(req, status, "Constructor failed")?;
        return Err(throw_error(req, ce.intl_exception, "Constructor failed"));
    }
    Ok(())
}

/// `PHP_METHOD(IntlListFormatter, format)`: the strings converted to
/// UTF-16, formatted with ICU's preflight, and converted back.
fn format(
    req: &mut Request,
    object: PhpObject,
    strings: PhpArray,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::errors_reset::<ListFormatterObject>(req, object);
    if strings.is_empty() {
        return PhpString::new(req, b"").map(Some);
    }
    let mut items = Vec::with_capacity(strings.len());
    // `ZEND_HASH_FOREACH_VAL(ht, val)` over the argument, which holds the
    // array while a conversion serves `__toString`.
    let failed = walk::foreach(req, strings, Hold::None, |req, row| -> walk::Step {
        match with_string(req, row.value(), ffi::to_utf16)? {
            Ok(units) => items.push(units),
            Err(code) => {
                let message = "Failed to convert string to UTF-16";
                error::errors_set::<ListFormatterObject>(req, object, code, message)?;
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if failed.is_some() {
        return Ok(None);
    }
    let taken = object
        .native_mut::<ListFormatterObject>(req)
        .and_then(|o| o.format.take());
    let Some(list) = taken else {
        return Err(throw_error(
            req,
            ce.error,
            "Found unconstructed IntlListFormatter",
        ));
    };
    let mut status = Code::ZERO;
    let needed = list.format(&items, &mut [], &mut status);
    let mut answer = None;
    if status.is_failure() && status != Code::BUFFER_OVERFLOW {
        answer = Some(status);
    }
    let mut out = vec![0u16; usize::try_from(needed).unwrap_or(0)];
    if answer.is_none() {
        status = Code::ZERO;
        list.format(&items, &mut out, &mut status);
        if status.is_failure() {
            answer = Some(status);
        }
    }
    if let Some(o) = object.native_mut::<ListFormatterObject>(req) {
        o.format = Some(list);
    }
    if let Some(code) = answer {
        error::errors_set::<ListFormatterObject>(req, object, code, "Failed to format list")?;
        return Ok(None);
    }
    match ffi::to_utf8(&out) {
        Ok(bytes) => PhpString::new(req, &bytes).map(Some),
        Err(code) => {
            let message = "Failed to convert result to UTF-8";
            error::errors_set::<ListFormatterObject>(req, object, code, message)?;
            Ok(None)
        }
    }
}

/// `IntlListFormatter`: a locale's list of items joined, as ICU's
/// `ulistfmt_*` joins them.
///
/// ```php
/// $f = new IntlListFormatter('en_US');
/// var_dump($f->format(['a', 'b', 3]), $f->format([]), (new IntlListFormatter('fr', IntlListFormatter::TYPE_OR, IntlListFormatter::WIDTH_SHORT))->format(['x', 'y']));
/// var_dump($f->getErrorCode(), $f->getErrorMessage());
/// try { new IntlListFormatter('en', 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { new IntlListFormatter('en', 0, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { new IntlListFormatter('root'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { new IntlListFormatter(str_repeat('x', 157)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $f->__construct('en'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone $f; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $f->format([new stdClass]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($f->format(["\xff"]), $f->getErrorCode() === U_INVALID_CHAR_FOUND, $f->getErrorMessage());
/// ```
/// ```expect
/// string(11) "a, b, and 3"
/// string(0) ""
/// string(6) "x ou y"
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// IntlListFormatter::__construct(): Argument #2 ($type) must be one of IntlListFormatter::TYPE_AND, IntlListFormatter::TYPE_OR, or IntlListFormatter::TYPE_UNITS
/// IntlListFormatter::__construct(): Argument #3 ($width) must be one of IntlListFormatter::WIDTH_WIDE, IntlListFormatter::WIDTH_SHORT, or IntlListFormatter::WIDTH_NARROW
/// IntlListFormatter::__construct(): Argument #1 ($locale) "root" is invalid
/// IntlListFormatter::__construct(): Argument #1 ($locale) must be less than or equal to 156 characters
/// IntlListFormatter object is already constructed
/// Trying to clone an uncloneable object of class IntlListFormatter
/// Object of class stdClass could not be converted to string
/// bool(false)
/// bool(true)
/// string(85) "IntlListFormatter::format(): Failed to convert string to UTF-16: U_INVALID_CHAR_FOUND"
/// ```
#[php_class(final, not_serializable, dynamic_properties = forbidden, size = 80, payload = ListFormatterObject, handlers = crate::listformatter::HANDLERS)]
pub(crate) struct IntlListFormatter;

#[php_class]
#[allow(non_snake_case)]
impl IntlListFormatter {
    const TYPE_AND: i64 = 0;
    const TYPE_OR: i64 = 1;
    const TYPE_UNITS: i64 = 2;
    const WIDTH_WIDE: i64 = 0;
    const WIDTH_SHORT: i64 = 1;
    const WIDTH_NARROW: i64 = 2;

    /// `__construct(string $locale, int $type = IntlListFormatter::TYPE_AND,
    /// int $width = IntlListFormatter::WIDTH_WIDE)`: the object's error and
    /// the request's reset before the arguments are read (php 8.5.12), so
    /// a stale failure is gone even where they are refused.
    ///
    /// ```php
    /// $f = new IntlListFormatter('en_US');
    /// $f->format(["\x80"]);
    /// new IntlListFormatter('en_US');
    /// var_dump(intl_get_error_code(), $f->getErrorCode());
    /// try { $f->__construct(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($f->getErrorCode());
    /// ```
    /// ```expect
    /// int(0)
    /// int(10)
    /// IntlListFormatter::__construct() expects at least 1 argument, 0 given
    /// int(0)
    /// ```
    #[php(before_args = error::errors_reset_this::<ListFormatterObject>)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        locale: PhpString,
        #[php(default = IntlListFormatter::TYPE_AND)] r#type: i64,
        #[php(default = IntlListFormatter::WIDTH_WIDE)] width: i64,
    ) -> Result<(), Thrown> {
        construct(req, this.object(), locale.as_bytes(), r#type, width)
    }

    fn format(
        req: &mut Request,
        this: This<Self>,
        strings: PhpArray,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        format(req, this.object(), strings).map(|s| s.map_or(OrFalse::False, OrFalse::Value))
    }

    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        i64::from(error::object_code::<ListFormatterObject>(req, this.object()).0)
    }

    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let message = error::object_message::<ListFormatterObject>(req, this.object());
        PhpString::new(req, &message)
    }
}
