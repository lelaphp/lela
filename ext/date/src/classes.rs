//! The classes of `php_date.stub.php` that carry no payload of their own:
//! `DateTimeInterface`, with its fourteen formats and the methods both
//! date classes implement, and 8.3's `DateError` and `DateException`
//! families. The classes they extend and implement are Core's, declared
//! here as `extern` headers to name them.

use php_ext::abi::{Instance, OrFalse};
use php_ext::attr::php_class;
use php_ext::values::{PhpArray, PhpString};

use crate::constants;
use crate::interval::DateInterval;
use crate::timezone::DateTimeZone;

/// Core's `Error`, `DateError`'s parent.
#[php_class(extern)]
pub(crate) struct Error;

/// Core's `Exception`, `DateException`'s parent.
#[php_class(extern)]
pub(crate) struct Exception;

/// Core's `IteratorAggregate`, which `DatePeriod` implements.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

/// Core's `Iterator`, what `DatePeriod::getIterator` answers.
#[php_class(extern)]
pub(crate) struct Iterator;

/// `interface DateTimeInterface`: its fourteen formats, each the `DATE_*`
/// constant's value (`@cvalue DATE_FORMAT_*`, the format itself and no
/// read of the constant, whose own deprecation a read would raise), and
/// the nine methods both date classes implement.
///
/// ```php
/// var_dump(DateTimeInterface::ATOM, DateTimeInterface::RSS, (new ReflectionClass('DateTimeInterface'))->isInterface());
/// var_dump(DateTimeInterface::RFC7231);
/// ```
/// ```expect
/// string(13) "Y-m-d\TH:i:sP"
/// string(16) "D, d M Y H:i:s O"
/// bool(true)
///
/// Deprecated: Constant DateTimeInterface::RFC7231 is deprecated since 8.5, as this format ignores the associated timezone and always uses GMT in %s on line 3
/// string(21) "D, d M Y H:i:s \G\M\T"
/// ```
#[php_class(interface)]
pub(crate) struct DateTimeInterface;

#[php_class]
impl DateTimeInterface {
    const ATOM: &'static str = constants::DATE_ATOM;
    const COOKIE: &'static str = constants::DATE_COOKIE;
    const ISO8601: &'static str = constants::DATE_ISO8601;
    const ISO8601_EXPANDED: &'static str = constants::DATE_ISO8601_EXPANDED;
    const RFC822: &'static str = constants::DATE_RFC822;
    const RFC850: &'static str = constants::DATE_RFC850;
    const RFC1036: &'static str = constants::DATE_RFC1036;
    const RFC1123: &'static str = constants::DATE_RFC1123;
    #[php(deprecated(
        since = "8.5",
        message = "as this format ignores the associated timezone and always uses GMT"
    ))]
    const RFC7231: &'static str = constants::DATE_RFC7231;
    const RFC2822: &'static str = constants::DATE_RFC2822;
    const RFC3339: &'static str = constants::DATE_RFC3339;
    const RFC3339_EXTENDED: &'static str = constants::DATE_RFC3339_EXTENDED;
    const RSS: &'static str = constants::DATE_RSS;
    const W3C: &'static str = constants::DATE_W3C;

    #[php(tentative)]
    fn format(this: This<Self>, format: PhpString) -> PhpString;
    #[php(tentative)]
    fn getTimezone(this: This<Self>) -> OrFalse<Instance<DateTimeZone>>;
    #[php(tentative)]
    fn getOffset(this: This<Self>) -> i64;
    #[php(tentative)]
    fn getTimestamp(this: This<Self>) -> i64;
    fn getMicrosecond(this: This<Self>) -> i64;
    #[php(tentative)]
    fn diff(
        this: This<Self>,
        targetObject: Instance<DateTimeInterface>,
        #[php(default = false)] absolute: bool,
    ) -> Instance<DateInterval>;
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(this: This<Self>);
    fn __serialize(this: This<Self>) -> PhpArray;
    fn __unserialize(this: This<Self>, data: PhpArray);
}

/// `class DateError extends Error`, `@strict-properties`, with the two
/// errors below it, and `DateException extends Exception` with its five.
///
/// ```php
/// foreach (['DateError', 'DateObjectError', 'DateRangeError', 'DateException', 'DateInvalidTimeZoneException', 'DateInvalidOperationException', 'DateMalformedStringException', 'DateMalformedIntervalStringException', 'DateMalformedPeriodStringException'] as $c) {
///     echo $c, ' extends ', get_parent_class($c), "\n";
/// }
/// ```
/// ```expect
/// DateError extends Error
/// DateObjectError extends DateError
/// DateRangeError extends DateError
/// DateException extends Exception
/// DateInvalidTimeZoneException extends DateException
/// DateInvalidOperationException extends DateException
/// DateMalformedStringException extends DateException
/// DateMalformedIntervalStringException extends DateException
/// DateMalformedPeriodStringException extends DateException
/// ```
#[php_class(extends(Error), dynamic_properties = forbidden)]
pub(crate) struct DateError;

#[php_class]
impl DateError {}

/// `class DateObjectError extends DateError`, `@strict-properties`.
#[php_class(extends(DateError), dynamic_properties = forbidden)]
pub(crate) struct DateObjectError;

#[php_class]
impl DateObjectError {}

/// `class DateRangeError extends DateError`, `@strict-properties`.
#[php_class(extends(DateError), dynamic_properties = forbidden)]
pub(crate) struct DateRangeError;

#[php_class]
impl DateRangeError {}

/// `class DateException extends Exception`, `@strict-properties`.
#[php_class(extends(Exception), dynamic_properties = forbidden)]
pub(crate) struct DateException;

#[php_class]
impl DateException {}

/// `class DateInvalidTimeZoneException extends DateException`,
/// `@strict-properties`.
#[php_class(extends(DateException), dynamic_properties = forbidden)]
pub(crate) struct DateInvalidTimeZoneException;

#[php_class]
impl DateInvalidTimeZoneException {}

/// `class DateInvalidOperationException extends DateException`,
/// `@strict-properties`.
#[php_class(extends(DateException), dynamic_properties = forbidden)]
pub(crate) struct DateInvalidOperationException;

#[php_class]
impl DateInvalidOperationException {}

/// `class DateMalformedStringException extends DateException`,
/// `@strict-properties`.
#[php_class(extends(DateException), dynamic_properties = forbidden)]
pub(crate) struct DateMalformedStringException;

#[php_class]
impl DateMalformedStringException {}

/// `class DateMalformedIntervalStringException extends DateException`,
/// `@strict-properties`.
#[php_class(extends(DateException), dynamic_properties = forbidden)]
pub(crate) struct DateMalformedIntervalStringException;

#[php_class]
impl DateMalformedIntervalStringException {}

/// `class DateMalformedPeriodStringException extends DateException`,
/// `@strict-properties`.
#[php_class(extends(DateException), dynamic_properties = forbidden)]
pub(crate) struct DateMalformedPeriodStringException;

#[php_class]
impl DateMalformedPeriodStringException {}
