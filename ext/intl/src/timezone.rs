//! `timezone/`: `IntlTimeZone` over an `icu::TimeZone` (`timezone_class.cpp`,
//! `timezone_methods.cpp`) and the `intltz_*` functions its methods
//! alias. The zone is the shim's ([`crate::ffi::timezone`]); what php
//! does around it, its arguments, its errors and the objects it hands
//! back, is here.

use std::cmp::Ordering;
use std::ffi::CString;

use php_ext::abi::{
    CloneObj, DebugInfo, ExtensionBuilder, Instance, ObjectHandlers, OrFalse, Ref, Std, This,
    Union, Untyped,
};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{clone_with, lookup_class, new_object};
use php_ext::raising::{Thrown, arg_value_error, fatal, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::common_date::{DateTimeZone, convert_datetimezone, convert_to_datetimezone, now};
use crate::error::{self, Carrier, IntlError, Outside, or_false};
use crate::ffi::cpp::{CName, Text, UStr};
use crate::ffi::timezone::{self as tz, TimeZone, Zone};
use crate::ffi::{Code, c_string, to_utf16};
use crate::iterator;

/// `TimeZone_object`: the error and the zone, none until a factory lays
/// one down.
#[derive(Default)]
pub(crate) struct TimeZoneObject {
    error: IntlError,
    pub(crate) zone: Option<Zone>,
}

impl Carrier for TimeZoneObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `TimeZone_handlers`: the clone, the comparison and the debug table.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `IntlTimeZone`.
pub(crate) struct Handlers;

/// A native's answer where php may `RETURN_FALSE`.
type Answer<T> = Result<OrFalse<T>, Thrown>;

/// `timezone_object_construct`: a new `IntlTimeZone` holding `zone`, one
/// owner, the caller's.
pub(crate) fn construct(
    req: &mut Request,
    zone: Zone,
) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
    let Some(class) = lookup_class(req, b"IntlTimeZone")? else {
        return Err(fatal(req, "InternalError: IntlTimeZone is not linked"));
    };
    let object = new_object(req, class)?;
    if let Some(o) = object.native_mut::<TimeZoneObject>(req) {
        o.zone = Some(zone);
    }
    Ok(object.into_instance())
}

/// `TIMEZONE_METHOD_FETCH_OBJECT`: the object's error reset, the
/// request's having been reset before the arguments
/// (`TIMEZONE_METHOD_INIT_VARS`, `before_args`), and php's `Error` for an
/// object no factory made.
fn fetch(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::reset_object::<TimeZoneObject>(req, object);
    if zone(req, object).is_none() {
        return Err(throw_error(
            req,
            ce.error,
            "Found unconstructed IntlTimeZone",
        ));
    }
    Ok(())
}

/// The zone an `IntlTimeZone` holds.
fn zone(req: &Request, object: PhpObject) -> Option<&TimeZone> {
    object.native::<TimeZoneObject>(req)?.zone.as_deref()
}

/// What `f` reads of the zone a fetched object holds.
fn read<T>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&TimeZone) -> T,
) -> Result<T, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    fetch(req, object)?;
    match zone(req, object) {
        Some(zone) => Ok(f(zone)),
        None => Err(throw_error(
            req,
            ce.error,
            "Found unconstructed IntlTimeZone",
        )),
    }
}

/// `INTL_METHOD_CHECK_STATUS(to, msg)` over a code ICU wrote.
fn check(
    req: &mut Request,
    object: PhpObject,
    code: Code,
    message: &str,
) -> Result<(), Option<Thrown>> {
    error::errors_set_code::<TimeZoneObject>(req, object, code);
    error::check_object::<TimeZoneObject>(req, object, message)
}

/// `intl_stringFromChar` for an id, with php's message where the id is
/// not UTF-8 (`intl_error_set(NULL, status, ...)`).
fn id_units(req: &mut Request, id: &[u8]) -> Result<Vec<u16>, Option<Thrown>> {
    to_utf16(id)
        .map_err(|code| error::set(req, code, "could not convert time zone id to UTF-16").err())
}

/// A string ICU answered, in UTF-8, or `false` with `message` set on the
/// request (`INTL_CHECK_STATUS`).
fn answered(
    req: &mut Request,
    text: Option<UStr>,
    message: &str,
) -> Result<Owner<PhpString>, Option<Thrown>> {
    let utf8: Result<Vec<u8>, Code> = text.map_or(Ok(Vec::new()), |t| t.to_utf8());
    match utf8 {
        Ok(bytes) => {
            error::set_code(req, Code::ZERO);
            PhpString::new(req, &bytes).map_err(Some)
        }
        Err(code) => Err(error::check(req, code, message).err().flatten()),
    }
}

/// `INTL_CHECK_LOCALE_LEN`-free `Locale::createFromName`'s name: the
/// default locale where none is given.
fn locale_name(req: &Request, locale: Option<PhpString>) -> CString {
    match locale {
        Some(locale) => c_string(locale.as_bytes()),
        None => c_string(&crate::default_locale(req)),
    }
}

/// `TimeZone_compare_objects`, `TimeZone_get_debug_info` and the clone.
impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_timezone)
    }

    /// `TimeZone_compare_objects`: equal where ICU's `==` says so,
    /// uncomparable otherwise, and php's `Exception` where either
    /// operand holds no zone.
    ///
    /// ```php
    /// $a = IntlTimeZone::createTimeZone('Europe/Lisbon');
    /// var_dump($a == IntlTimeZone::createTimeZone('Europe/Lisbon'), $a == IntlTimeZone::getGMT(), $a < $a);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ```
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        let (ValueRef::Object(a), ValueRef::Object(b)) = (left.as_ref(), right.as_ref()) else {
            return Std.compare(req, left, right);
        };
        let (Some(x), Some(y)) = (
            a.native::<TimeZoneObject>(req),
            b.native::<TimeZoneObject>(req),
        ) else {
            return Std.compare(req, left, right);
        };
        if let (Some(x), Some(y)) = (x.zone.as_deref(), y.zone.as_deref()) {
            return Ok(tz::php_intl_tz_equals(x, y).then_some(Ordering::Equal));
        }
        let message = "Comparison with at least one unconstructed IntlTimeZone operand";
        Err(throw(req, ce.exception, message))
    }

    /// `TimeZone_get_debug_info`: whether the object holds a zone, its
    /// id, and its raw and current offsets.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        let rows = zone(req, object).map(debug_rows);
        debug_table(req, rows).map(|table| Some(table.into()))
    }

    /// `TimeZone_objects_free`: the error's message freed before
    /// `zend_object_std_dtor`.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        error::free::<TimeZoneObject>(req, object);
    }
}

/// What `TimeZone_get_debug_info` reads of a zone: its id, and its raw
/// and current offsets where ICU answers them.
pub(crate) type DebugRows = (Option<Vec<u8>>, Option<(i32, i32)>);

/// The id and offsets of `zone` now.
pub(crate) fn debug_rows(zone: &TimeZone) -> DebugRows {
    let id = tz::php_intl_tz_id(zone).and_then(|id| id.to_utf8().ok());
    let (mut raw, mut dst, mut status) = (0, 0, Code::ZERO);
    tz::php_intl_tz_offset(zone, now(), false, &mut raw, &mut dst, &mut status);
    (id, (!status.is_failure()).then_some((raw, raw + dst)))
}

/// `TimeZone_get_debug_info`'s table: `valid`, then `id`, `rawOffset`
/// and `currentOffset` as far as they were read.
pub(crate) fn debug_table(
    req: &mut Request,
    rows: Option<DebugRows>,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut table = PhpArray::new(req)?;
    set(req, &mut table, b"valid", Owner::bool(rows.is_some()))?;
    if let Some((Some(id), offsets)) = rows {
        let id = PhpString::new(req, &id)?;
        set(req, &mut table, b"id", id.into_value())?;
        if let Some((raw, current)) = offsets {
            set(req, &mut table, b"rawOffset", Owner::int(i64::from(raw)))?;
            set(
                req,
                &mut table,
                b"currentOffset",
                Owner::int(i64::from(current)),
            )?;
        }
    }
    Ok(table)
}

/// `zend_hash_str_update(table, name, value)`.
pub(crate) fn set(
    req: &mut Request,
    table: &mut Owner<PhpArray>,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    table.set_str(req, name, value)
}

/// `TimeZone_clone_obj`: the members copied, the zone cloned, and php's
/// `Error` for an object that holds none.
///
/// ```php
/// $a = IntlTimeZone::createTimeZone('Asia/Tokyo'); $b = clone $a;
/// var_dump($b->getID(), $a == $b, $a === $b);
/// ```
/// ```expect
/// string(10) "Asia/Tokyo"
/// bool(true)
/// bool(false)
/// ```
fn clone_timezone(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let zone = zone(req, object).map(|z| tz::php_intl_tz_clone(z));
    let constructed = zone.is_some();
    let payload = TimeZoneObject {
        error: IntlError::default(),
        zone: zone.flatten(),
    };
    let held = payload.zone.is_some();
    let copy = clone_with(req, &object, payload)?;
    if held {
        return Ok(copy);
    }
    copy.give_up(req);
    let message = if constructed {
        "Failed to clone IntlTimeZone"
    } else {
        "Cannot clone uninitialized IntlTimeZone"
    };
    Err(throw_error(req, ce.error, message))
}

/// `class IntlTimeZone`: one ICU zone, made by its static factories and
/// never by `new`, and its eleven display and type constants.
///
/// ```php
/// var_dump(IntlTimeZone::DISPLAY_LONG, IntlTimeZone::DISPLAY_GENERIC_LOCATION, IntlTimeZone::TYPE_CANONICAL_LOCATION);
/// try { new IntlTimeZone; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(IntlTimeZone::createTimeZone('Europe/Amsterdam'));
/// $r = new ReflectionClass('IntlTimeZone'); var_dump($r->newInstanceWithoutConstructor());
/// ```
/// ```expect
/// int(2)
/// int(8)
/// int(2)
/// Call to private IntlTimeZone::__construct() from global scope
/// object(IntlTimeZone)#1 (4) {
///   ["valid"]=>
///   bool(true)
///   ["id"]=>
///   string(16) "Europe/Amsterdam"
///   ["rawOffset"]=>
///   int(3600000)
///   ["currentOffset"]=>
///   int(%d)
/// }
/// object(IntlTimeZone)#3 (1) {
///   ["valid"]=>
///   bool(false)
/// }
/// ```
#[php_class(not_serializable, size = 88, payload = TimeZoneObject, handlers = crate::timezone::HANDLERS)]
pub(crate) struct IntlTimeZone;

/// The methods, each the `intltz_*` function it aliases.
///
/// ```php
/// $z = IntlTimeZone::createTimeZone('Europe/Lisbon');
/// var_dump(IntlTimeZone::countEquivalentIDs('Europe/Lisbon') > 1, get_class(IntlTimeZone::createDefault()), iterator_count(IntlTimeZone::createEnumeration('PT')));
/// var_dump(iterator_count(IntlTimeZone::createTimeZoneIDEnumeration(IntlTimeZone::TYPE_CANONICAL, 'PT')), IntlTimeZone::fromDateTimeZone(new DateTimeZone('Asia/Tokyo'))->getID());
/// var_dump(IntlTimeZone::getCanonicalID('Asia/Calcutta'), $z->getDisplayName(false, IntlTimeZone::DISPLAY_SHORT_GMT, 'en'), $z->getDSTSavings(), IntlTimeZone::getEquivalentID('Europe/Lisbon', 999));
/// var_dump($z->getErrorCode(), $z->getErrorMessage(), IntlTimeZone::getGMT()->getID(), IntlTimeZone::getIanaID('Asia/Calcutta'), $z->getOffset(1356998400000.0, false, $raw, $dst), $raw, $dst);
/// var_dump($z->getRawOffset(), IntlTimeZone::getRegion('Europe/Lisbon'), strlen(IntlTimeZone::getTZDataVersion()), IntlTimeZone::getUnknown()->getID(), IntlTimeZone::getWindowsID('Europe/Lisbon'));
/// var_dump(IntlTimeZone::getIDForWindowsID('GMT Standard Time'), $z->hasSameRules(IntlTimeZone::createTimeZone('Europe/London')), $z->toDateTimeZone()->getName(), $z->useDaylightTime());
/// ```
/// ```expect
/// bool(true)
/// string(12) "IntlTimeZone"
/// int(5)
/// int(3)
/// string(10) "Asia/Tokyo"
/// string(13) "Asia/Calcutta"
/// string(5) "+0000"
/// int(3600000)
/// string(0) ""
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// string(3) "GMT"
/// string(12) "Asia/Kolkata"
/// bool(true)
/// int(0)
/// int(0)
/// int(0)
/// string(2) "PT"
/// int(5)
/// string(11) "Etc/Unknown"
/// string(17) "GMT Standard Time"
/// string(13) "Europe/London"
/// bool(false)
/// string(13) "Europe/Lisbon"
/// bool(true)
/// ```
#[php_class]
impl IntlTimeZone {
    const DISPLAY_SHORT: i64 = 1;
    const DISPLAY_LONG: i64 = 2;
    const DISPLAY_SHORT_GENERIC: i64 = 3;
    const DISPLAY_LONG_GENERIC: i64 = 4;
    const DISPLAY_SHORT_GMT: i64 = 5;
    const DISPLAY_LONG_GMT: i64 = 6;
    const DISPLAY_SHORT_COMMONLY_USED: i64 = 7;
    const DISPLAY_GENERIC_LOCATION: i64 = 8;
    const TYPE_ANY: i64 = 0;
    const TYPE_CANONICAL: i64 = 1;
    const TYPE_CANONICAL_LOCATION: i64 = 2;

    /// `__construct()`: private, and php's `Exception` where reflection
    /// reaches it.
    ///
    /// ```php
    /// $m = new ReflectionMethod('IntlTimeZone', '__construct');
    /// try { $m->invoke((new ReflectionClass('IntlTimeZone'))->newInstanceWithoutConstructor()); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// An object of this type cannot be created with the new operator
    /// ```
    #[php(private)]
    fn __construct(req: &mut Request, _this: This<Self>) -> Result<(), Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        Err(throw(
            req,
            ce.exception,
            "An object of this type cannot be created with the new operator",
        ))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn countEquivalentIDs(req: &mut Request, timezoneId: PhpString) -> Answer<i64> {
        count_equivalent_ids(req, timezoneId)
    }

    #[php(tentative, before_args = error::reset)]
    fn createDefault(req: &mut Request) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
        special(req, 0)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn createEnumeration(
        req: &mut Request,
        #[php(default = null)] countryOrRawOffset: Option<Union<PhpString, i64>>,
    ) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
        create_enumeration(req, countryOrRawOffset)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn createTimeZone(
        req: &mut Request,
        timezoneId: PhpString,
    ) -> Result<Option<Owner<Instance<IntlTimeZone>>>, Thrown> {
        create_time_zone(req, timezoneId)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn createTimeZoneIDEnumeration(
        req: &mut Request,
        r#type: i64,
        #[php(default = null)] region: Option<PhpString>,
        #[php(default = null)] rawOffset: Option<i64>,
    ) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
        create_time_zone_id_enumeration(req, r#type, region, rawOffset)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn fromDateTimeZone(
        req: &mut Request,
        timezone: Instance<DateTimeZone>,
    ) -> Result<Option<Owner<Instance<IntlTimeZone>>>, Thrown> {
        from_date_time_zone(req, timezone)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getCanonicalID(
        req: &mut Request,
        timezoneId: PhpString,
        #[php(default = null)] isSystemId: Ref<Untyped<Value>>,
    ) -> Answer<Owner<PhpString>> {
        get_canonical_id(req, timezoneId, &isSystemId)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getDisplayName(
        req: &mut Request,
        this: This<Self>,
        #[php(default = false)] dst: bool,
        #[php(default = IntlTimeZone::DISPLAY_LONG)] style: i64,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Answer<Owner<PhpString>> {
        get_display_name(req, this.object(), dst, style, locale)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getDSTSavings(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        read(req, this.object(), |zone| {
            i64::from(tz::php_intl_tz_int(zone, 1))
        })
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getEquivalentID(
        req: &mut Request,
        timezoneId: PhpString,
        offset: i64,
    ) -> Answer<Owner<PhpString>> {
        get_equivalent_id(req, timezoneId, offset)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> OrFalse<i64> {
        OrFalse::Value(i64::from(
            error::object_code::<TimeZoneObject>(req, this.object()).0,
        ))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        let message = error::object_message::<TimeZoneObject>(req, this.object());
        PhpString::new(req, &message).map(OrFalse::Value)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getGMT(req: &mut Request) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
        special(req, 1)
    }

    #[allow(non_snake_case)]
    #[php(before_args = error::reset)]
    fn getIanaID(req: &mut Request, timezoneId: PhpString) -> Answer<Owner<PhpString>> {
        lookup(req, timezoneId, &Lookup::Iana)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getID(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        get_id(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getOffset(
        req: &mut Request,
        this: This<Self>,
        timestamp: f64,
        local: bool,
        rawOffset: Ref<Untyped<Value>>,
        dstOffset: Ref<Untyped<Value>>,
    ) -> Result<bool, Thrown> {
        get_offset(
            req,
            this.object(),
            timestamp,
            local,
            (&rawOffset, &dstOffset),
        )
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getRawOffset(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        read(req, this.object(), |zone| {
            i64::from(tz::php_intl_tz_int(zone, 0))
        })
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getRegion(req: &mut Request, timezoneId: PhpString) -> Answer<Owner<PhpString>> {
        get_region(req, timezoneId)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getTZDataVersion(req: &mut Request) -> Answer<Owner<PhpString>> {
        get_tz_data_version(req)
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn getUnknown(req: &mut Request) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
        special(req, 2)
    }

    #[php(tentative)]
    #[allow(non_snake_case)]
    fn getWindowsID(req: &mut Request, timezoneId: PhpString) -> Answer<Owner<PhpString>> {
        lookup(req, timezoneId, &Lookup::Windows)
    }

    #[php(tentative)]
    #[allow(non_snake_case)]
    fn getIDForWindowsID(
        req: &mut Request,
        timezoneId: PhpString,
        #[php(default = null)] region: Option<PhpString>,
    ) -> Answer<Owner<PhpString>> {
        lookup(req, timezoneId, &Lookup::ForWindows(region))
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn hasSameRules(
        req: &mut Request,
        this: This<Self>,
        other: Instance<IntlTimeZone>,
    ) -> Result<bool, Thrown> {
        has_same_rules(req, this.object(), other.object())
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn toDateTimeZone(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Owner<Instance<DateTimeZone>>> {
        to_date_time_zone(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    #[allow(non_snake_case)]
    fn useDaylightTime(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        read(req, this.object(), |zone| tz::php_intl_tz_int(zone, 2) != 0)
    }
}

/// `intltz_create_default`, `intltz_get_gmt` and `intltz_get_unknown`:
/// `createDefault()`, `getGMT()` or `getUnknown()`, as a zone of the
/// object's own.
fn special(req: &mut Request, which: i32) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
    match tz::php_intl_tz_special(which) {
        Some(zone) => construct(req, zone),
        None => Err(fatal(req, "InternalError: ICU answered no zone")),
    }
}

/// `intltz_create_time_zone`: GMT where ICU does not know the id.
fn create_time_zone(
    req: &mut Request,
    id: PhpString,
) -> Result<Option<Owner<Instance<IntlTimeZone>>>, Thrown> {
    let units = match id_units(req, id.as_bytes()) {
        Ok(units) => units,
        Err(thrown) => return thrown.map_or(Ok(None), Err),
    };
    match tz::php_intl_tz_create(Text::from(units.as_slice())) {
        Some(zone) => construct(req, zone).map(Some),
        None => Err(fatal(req, "InternalError: ICU answered no zone")),
    }
}

/// `intltz_from_date_time_zone`: php's `Error` for a `DateTimeZone` its
/// constructor never set, null where ICU knows no such zone.
fn from_date_time_zone(
    req: &mut Request,
    timezone: Instance<DateTimeZone>,
) -> Result<Option<Owner<Instance<IntlTimeZone>>>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let Some(kind) = php_ext_date::timezone_obj(req, timezone.object()) else {
        return Err(throw_error(
            req,
            ce.error,
            "DateTimeZone object is unconstructed",
        ));
    };
    match convert_datetimezone(req, Some(&kind), &mut Outside::Request)? {
        Some(zone) => construct(req, zone).map(Some),
        None => Ok(None),
    }
}

/// `intltz_create_enumeration`: every zone, a country's, or those at a
/// raw offset.
fn create_enumeration(
    req: &mut Request,
    filter: Option<Union<PhpString, i64>>,
) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
    let mut status = Code::ZERO;
    let enumeration = match filter {
        None => tz::enumeration(0, None, 0, None, &mut status),
        Some(Union::A(country)) => {
            let country = c_string(country.as_bytes());
            tz::enumeration(
                1,
                Some(CName::from(country.as_c_str())),
                0,
                None,
                &mut status,
            )
        }
        Some(Union::B(offset)) => {
            let Ok(offset) = i32::try_from(offset) else {
                return Err(arg_value_error(
                    req,
                    1,
                    &format!("be between {} and {}", i32::MIN, i32::MAX),
                ));
            };
            tz::enumeration(2, None, offset, None, &mut status)
        }
    };
    match enumeration {
        Some(enumeration) => {
            iterator::from_enumeration(req, enumeration).map(|o| OrFalse::Value(o.into_instance()))
        }
        None => or_false(Err(error::set(
            req,
            Code::ILLEGAL_ARGUMENT,
            "error obtaining enumeration",
        )
        .err())),
    }
}

/// `intltz_count_equivalent_ids`.
fn count_equivalent_ids(req: &mut Request, id: PhpString) -> Answer<i64> {
    or_false(id_units(req, id.as_bytes()).map(|units| {
        i64::from(tz::php_intl_tz_count_equivalent(Text::from(
            units.as_slice(),
        )))
    }))
}

/// `intltz_create_time_zone_id_enumeration`: the zones of a
/// `USystemTimeZoneType`, a country's and at an offset where given.
fn create_time_zone_id_enumeration(
    req: &mut Request,
    zone_type: i64,
    region: Option<PhpString>,
    offset: Option<i64>,
) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
    if !(0..=2).contains(&zone_type) {
        let message = "be one of IntlTimeZone::TYPE_ANY, IntlTimeZone::TYPE_CANONICAL, or IntlTimeZone::TYPE_CANONICAL_LOCATION";
        return Err(arg_value_error(req, 1, message));
    }
    let offset = match offset.map(i32::try_from) {
        Some(Err(_)) => {
            return Err(arg_value_error(
                req,
                3,
                &format!("be between {} and {}", i32::MIN, i32::MAX),
            ));
        }
        Some(Ok(offset)) => Some(offset),
        None => None,
    };
    let region = region.map(|r| c_string(r.as_bytes()));
    let mut status = Code::ZERO;
    #[allow(clippy::cast_possible_truncation, reason = "checked to 0..=2 above")]
    let enumeration = tz::enumeration(
        3,
        region.as_deref().map(CName::from),
        zone_type as i32,
        offset.as_ref(),
        &mut status,
    );
    if let Err(thrown) = error::check(req, status, "error obtaining time zone id enumeration") {
        return or_false(Err(thrown));
    }
    match enumeration {
        Some(enumeration) => {
            iterator::from_enumeration(req, enumeration).map(|o| OrFalse::Value(o.into_instance()))
        }
        None => Ok(OrFalse::False),
    }
}

/// `intltz_get_canonical_id`: the canonical id, and whether it is a
/// system id through the reference.
fn get_canonical_id(
    req: &mut Request,
    id: PhpString,
    is_system: &Ref<Untyped<Value>>,
) -> Answer<Owner<PhpString>> {
    let units = match id_units(req, id.as_bytes()) {
        Ok(units) => units,
        Err(thrown) => return or_false(Err(thrown)),
    };
    let (mut system, mut status) = (false, Code::ZERO);
    let text = tz::php_intl_tz_lookup(
        0,
        Text::from(units.as_slice()),
        0,
        None,
        &mut system,
        &mut status,
    );
    if let Err(thrown) = error::check(req, status, "error obtaining canonical ID") {
        return or_false(Err(thrown));
    }
    let answer = or_false(answered(
        req,
        text,
        "could not convert time zone id to UTF-16",
    ))?;
    if matches!(answer, OrFalse::Value(_))
        && let Err(thrown) = is_system.set(req, Untyped(Value::bool(system)))
    {
        if let OrFalse::Value(id) = answer {
            id.give_up(req);
        }
        return Err(thrown);
    }
    Ok(answer)
}

/// The lookups by id that answer an id: `getIanaID`, `getWindowsID` and
/// `getIDForWindowsID`.
enum Lookup {
    Iana,
    Windows,
    ForWindows(Option<PhpString>),
}

/// `intltz_get_iana_id`, `intltz_get_windows_id` and
/// `intltz_get_id_for_windows_id`.
fn lookup(req: &mut Request, id: PhpString, which: &Lookup) -> Answer<Owner<PhpString>> {
    let units = match id_units(req, id.as_bytes()) {
        Ok(units) => units,
        Err(thrown) => return or_false(Err(thrown)),
    };
    let (selector, region) = match which {
        Lookup::Iana => (2, None),
        Lookup::Windows => (3, None),
        Lookup::ForWindows(region) => (4, region.map(|r| c_string(r.as_bytes()))),
    };
    let (mut flag, mut status) = (false, Code::ZERO);
    let region = region.as_deref().map(CName::from);
    let text = tz::php_intl_tz_lookup(
        selector,
        Text::from(units.as_slice()),
        0,
        region,
        &mut flag,
        &mut status,
    );
    let (failed, unknown, converted) = match which {
        Lookup::Iana => (
            "error obtaining IANA ID",
            None,
            "could not convert time zone id to UTF-16",
        ),
        Lookup::Windows => (
            "Unable to get timezone from windows ID",
            Some("unknown system timezone"),
            "could not convert time zone id to UTF-8",
        ),
        Lookup::ForWindows(_) => (
            "unable to get windows ID for timezone",
            Some("unknown windows timezone"),
            "could not convert time zone id to UTF-8",
        ),
    };
    if let Err(thrown) = error::check(req, status, failed) {
        return or_false(Err(thrown));
    }
    if let Some(unknown) = unknown
        && text.as_ref().is_none_or(|t| t.units().is_empty())
    {
        return or_false(Err(error::set(req, Code::ILLEGAL_ARGUMENT, unknown).err()));
    }
    or_false(answered(req, text, converted))
}

/// `intltz_get_equivalent_id`.
fn get_equivalent_id(req: &mut Request, id: PhpString, index: i64) -> Answer<Owner<PhpString>> {
    let Ok(index) = i32::try_from(index) else {
        return Err(arg_value_error(
            req,
            2,
            &format!("be between {} and {}", i32::MIN, i32::MAX),
        ));
    };
    let units = match id_units(req, id.as_bytes()) {
        Ok(units) => units,
        Err(thrown) => return or_false(Err(thrown)),
    };
    let (mut flag, mut status) = (false, Code::ZERO);
    let text = tz::php_intl_tz_lookup(
        1,
        Text::from(units.as_slice()),
        index,
        None,
        &mut flag,
        &mut status,
    );
    or_false(answered(
        req,
        text,
        "could not convert resulting time zone id to UTF-16",
    ))
}

/// `intltz_get_region`: the two-letter country code of a zone.
fn get_region(req: &mut Request, id: PhpString) -> Answer<Owner<PhpString>> {
    let units = match id_units(req, id.as_bytes()) {
        Ok(units) => units,
        Err(thrown) => return or_false(Err(thrown)),
    };
    let (mut out, mut status) = ([0u8; 3], Code::ZERO);
    let len = tz::php_intl_tz_region(Text::from(units.as_slice()), &mut out, &mut status);
    if let Err(thrown) = error::check(req, status, "error obtaining region") {
        return or_false(Err(thrown));
    }
    let len = usize::try_from(len).unwrap_or(0).min(out.len());
    PhpString::new(req, &out[..len]).map(OrFalse::Value)
}

/// `intltz_get_tz_data_version`.
fn get_tz_data_version(req: &mut Request) -> Answer<Owner<PhpString>> {
    let mut status = Code::ZERO;
    let version = tz::data_version(&mut status);
    if let Err(thrown) = error::check(req, status, "error obtaining time zone data version") {
        return or_false(Err(thrown));
    }
    PhpString::new(req, version.as_bytes()).map(OrFalse::Value)
}

/// `intltz_get_id`.
fn get_id(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpString>> {
    let id = read(req, object, |z| tz::php_intl_tz_id(z))?;
    object_answer(req, object, id, "could not convert id to UTF-8")
}

/// A string ICU answered for an object, in UTF-8, or `false` with
/// `message` set on the object (`INTL_METHOD_CHECK_STATUS`).
fn object_answer(
    req: &mut Request,
    object: PhpObject,
    text: Option<UStr>,
    message: &str,
) -> Answer<Owner<PhpString>> {
    let utf8: Result<Vec<u8>, Code> = text.map_or(Ok(Vec::new()), |t| t.to_utf8());
    match utf8 {
        Ok(bytes) => PhpString::new(req, &bytes).map(OrFalse::Value),
        Err(code) => check(req, object, code, message)
            .err()
            .flatten()
            .map_or(Ok(OrFalse::False), Err),
    }
}

/// `intltz_get_offset`: the raw and the daylight offsets at an instant,
/// through the references.
fn get_offset(
    req: &mut Request,
    object: PhpObject,
    timestamp: f64,
    local: bool,
    (raw_out, dst_out): (&Ref<Untyped<Value>>, &Ref<Untyped<Value>>),
) -> Result<bool, Thrown> {
    let (raw, dst, status) = read(req, object, |zone| {
        let (mut raw, mut dst, mut status) = (0, 0, Code::ZERO);
        tz::php_intl_tz_offset(zone, timestamp, local, &mut raw, &mut dst, &mut status);
        (raw, dst, status)
    })?;
    if let Err(thrown) = check(req, object, status, "error obtaining offset") {
        return thrown.map_or(Ok(false), Err);
    }
    raw_out.set(req, Untyped(Value::int(i64::from(raw))))?;
    dst_out.set(req, Untyped(Value::int(i64::from(dst))))?;
    Ok(true)
}

/// `intltz_has_same_rules`.
fn has_same_rules(req: &mut Request, object: PhpObject, other: PhpObject) -> Result<bool, Thrown> {
    fetch(req, object)?;
    if let (Some(a), Some(b)) = (zone(req, object), zone(req, other)) {
        return Ok(tz::php_intl_tz_has_same_rules(a, b));
    }
    let message = "The second IntlTimeZone is unconstructed";
    error::errors_set::<TimeZoneObject>(req, object, Code::ILLEGAL_ARGUMENT, message)?;
    Ok(false)
}

/// `intltz_get_display_name`: the zone's name in a locale, in one of the
/// eight display styles.
fn get_display_name(
    req: &mut Request,
    object: PhpObject,
    dst: bool,
    style: i64,
    locale: Option<PhpString>,
) -> Answer<Owner<PhpString>> {
    fetch(req, object)?;
    let style = i32::try_from(style).ok().filter(|s| (1..=8).contains(s));
    let Some(style) = style else {
        error::errors_set::<TimeZoneObject>(
            req,
            object,
            Code::ILLEGAL_ARGUMENT,
            "wrong display type",
        )?;
        return Ok(OrFalse::False);
    };
    let locale = locale_name(req, locale);
    let name = zone(req, object).and_then(|zone| {
        tz::php_intl_tz_display_name(zone, dst, style, CName::from(locale.as_c_str()))
    });
    object_answer(
        req,
        object,
        name,
        "could not convert resulting time zone id to UTF-16",
    )
}

/// `intltz_to_date_time_zone`.
fn to_date_time_zone(
    req: &mut Request,
    object: PhpObject,
) -> Answer<Owner<Instance<DateTimeZone>>> {
    fetch(req, object)?;
    let Some(zone) = zone(req, object).and_then(|z| tz::php_intl_tz_clone(z)) else {
        return Ok(OrFalse::False);
    };
    let made = convert_to_datetimezone(req, &zone, &mut Outside::of::<TimeZoneObject>(object))?;
    Ok(made.map_or(OrFalse::False, |o| OrFalse::Value(o.into_instance())))
}

/// `intltz_count_equivalent_ids(string $timezoneId): int|false`.
///
/// ```php
/// var_dump(intltz_count_equivalent_ids('Europe/Lisbon') > 1, intltz_count_equivalent_ids("\xff"), intl_get_error_message());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(%d) "intltz_count_equivalent_ids(): could not convert time zone id to UTF-16: U_INVALID_CHAR_FOUND"
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_count_equivalent_ids(req: &mut Request, timezoneId: PhpString) -> Answer<i64> {
    count_equivalent_ids(req, timezoneId)
}

/// `intltz_create_default(): IntlTimeZone`.
///
/// ```php
/// ini_set('date.timezone', 'UTC'); var_dump(get_class(intltz_create_default()));
/// ```
/// ```expect
/// string(12) "IntlTimeZone"
/// ```
#[php_function(before_args = error::reset)]
fn intltz_create_default(req: &mut Request) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
    special(req, 0)
}

/// `intltz_create_enumeration(string|int|null $countryOrRawOffset = null): IntlIterator|false`.
///
/// ```php
/// var_dump(implode(' ', iterator_to_array(intltz_create_enumeration('PT'))), in_array('Asia/Kolkata', iterator_to_array(intltz_create_enumeration(19800000))), iterator_count(intltz_create_enumeration()) > 400);
/// try { intltz_create_enumeration(PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(59) "Atlantic/Azores Atlantic/Madeira Europe/Lisbon Portugal WET"
/// bool(true)
/// bool(true)
/// intltz_create_enumeration(): Argument #1 ($countryOrRawOffset) must be between -2147483648 and 2147483647
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_create_enumeration(
    req: &mut Request,
    #[php(default = null)] countryOrRawOffset: Option<Union<PhpString, i64>>,
) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
    create_enumeration(req, countryOrRawOffset)
}

/// `intltz_create_time_zone(string $timezoneId): ?IntlTimeZone`.
///
/// ```php
/// var_dump(intltz_create_time_zone('Nowhere/Special')->getID(), intltz_create_time_zone("\xff"), intl_get_error_code());
/// ```
/// ```expect
/// string(11) "Etc/Unknown"
/// NULL
/// int(10)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_create_time_zone(
    req: &mut Request,
    timezoneId: PhpString,
) -> Result<Option<Owner<Instance<IntlTimeZone>>>, Thrown> {
    create_time_zone(req, timezoneId)
}

/// `intltz_create_time_zone_id_enumeration(int $type, ?string $region = null, ?int $rawOffset = null): IntlIterator|false`.
///
/// ```php
/// var_dump(iterator_to_array(intltz_create_time_zone_id_enumeration(IntlTimeZone::TYPE_CANONICAL_LOCATION, 'PT', 0)));
/// try { intltz_create_time_zone_id_enumeration(7); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(16) "Atlantic/Madeira"
///   [1]=>
///   string(13) "Europe/Lisbon"
/// }
/// intltz_create_time_zone_id_enumeration(): Argument #1 ($type) must be one of IntlTimeZone::TYPE_ANY, IntlTimeZone::TYPE_CANONICAL, or IntlTimeZone::TYPE_CANONICAL_LOCATION
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_create_time_zone_id_enumeration(
    req: &mut Request,
    r#type: i64,
    #[php(default = null)] region: Option<PhpString>,
    #[php(default = null)] rawOffset: Option<i64>,
) -> Answer<Owner<Instance<iterator::IntlIterator>>> {
    create_time_zone_id_enumeration(req, r#type, region, rawOffset)
}

/// `intltz_from_date_time_zone(DateTimeZone $timezone): ?IntlTimeZone`.
///
/// ```php
/// foreach (['Europe/Rome', '+05:30', '-00:30', 'EST', '+30:00'] as $z) {
///     $t = intltz_from_date_time_zone(new DateTimeZone($z));
///     echo $t ? $t->getID() : intl_get_error_message(), "\n";
/// }
/// try { intltz_from_date_time_zone((new ReflectionClass('DateTimeZone'))->newInstanceWithoutConstructor()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Europe/Rome
/// GMT+05:30
/// GMT+00:30
/// EST
/// intltz_from_date_time_zone(): object has an time zone offset that's too large: U_ILLEGAL_ARGUMENT_ERROR
/// DateTimeZone object is unconstructed
/// ```
#[php_function(before_args = error::reset)]
fn intltz_from_date_time_zone(
    req: &mut Request,
    timezone: Instance<DateTimeZone>,
) -> Result<Option<Owner<Instance<IntlTimeZone>>>, Thrown> {
    from_date_time_zone(req, timezone)
}

/// `intltz_get_canonical_id(string $timezoneId, &$isSystemId = null): string|false`.
///
/// ```php
/// var_dump(intltz_get_canonical_id('Asia/Calcutta', $s), $s, intltz_get_canonical_id('GMT+05:30', $s), $s, intltz_get_canonical_id('Nowhere'));
/// ```
/// ```expect
/// string(13) "Asia/Calcutta"
/// bool(true)
/// string(9) "GMT+05:30"
/// bool(false)
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_get_canonical_id(
    req: &mut Request,
    timezoneId: PhpString,
    #[php(default = null)] isSystemId: Ref<Untyped<Value>>,
) -> Answer<Owner<PhpString>> {
    get_canonical_id(req, timezoneId, &isSystemId)
}

/// `intltz_get_display_name(IntlTimeZone $timezone, bool $dst = false, int $style = IntlTimeZone::DISPLAY_LONG, ?string $locale = null): string|false`.
///
/// ```php
/// $z = intltz_create_time_zone('Europe/Paris');
/// var_dump(intltz_get_display_name($z, false, IntlTimeZone::DISPLAY_LONG, 'en_US'), $z->getDisplayName(true, IntlTimeZone::DISPLAY_SHORT_GMT, 'fr_FR'), $z->getDisplayName(false, 99), intl_get_error_message());
/// ```
/// ```expect
/// string(30) "Central European Standard Time"
/// string(5) "+0200"
/// bool(false)
/// string(76) "IntlTimeZone::getDisplayName(): wrong display type: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_display_name(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
    #[php(default = false)] dst: bool,
    #[php(default = IntlTimeZone::DISPLAY_LONG)] style: i64,
    #[php(default = null)] locale: Option<PhpString>,
) -> Answer<Owner<PhpString>> {
    get_display_name(req, timezone.object(), dst, style, locale)
}

/// `intltz_get_dst_savings(IntlTimeZone $timezone): int`.
///
/// ```php
/// var_dump(intltz_get_dst_savings(intltz_create_time_zone('Europe/Paris')), intltz_create_time_zone('Asia/Tokyo')->getDSTSavings());
/// ```
/// ```expect
/// int(3600000)
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_dst_savings(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
) -> Result<i64, Thrown> {
    read(req, timezone.object(), |zone| {
        i64::from(tz::php_intl_tz_int(zone, 1))
    })
}

/// `intltz_get_equivalent_id(string $timezoneId, int $offset): string|false`.
///
/// ```php
/// var_dump(intltz_get_equivalent_id('Europe/Lisbon', 0) !== false, intltz_get_equivalent_id('Europe/Lisbon', 999));
/// try { intltz_get_equivalent_id('UTC', PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(0) ""
/// intltz_get_equivalent_id(): Argument #2 ($offset) must be between -2147483648 and 2147483647
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_get_equivalent_id(
    req: &mut Request,
    timezoneId: PhpString,
    offset: i64,
) -> Answer<Owner<PhpString>> {
    get_equivalent_id(req, timezoneId, offset)
}

/// `intltz_get_error_code(IntlTimeZone $timezone): int|false`.
///
/// ```php
/// $z = intltz_get_gmt(); $z->getDisplayName(false, 0);
/// var_dump(intltz_get_error_code($z), $z->getErrorCode());
/// ```
/// ```expect
/// int(1)
/// int(1)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_error_code(req: &mut Request, timezone: Instance<IntlTimeZone>) -> OrFalse<i64> {
    OrFalse::Value(i64::from(
        error::object_code::<TimeZoneObject>(req, timezone.object()).0,
    ))
}

/// `intltz_get_error_message(IntlTimeZone $timezone): string|false`.
///
/// ```php
/// $z = intltz_get_gmt(); var_dump(intltz_get_error_message($z)); $z->getDisplayName(false, 0);
/// var_dump(intltz_get_error_message($z), $z->getErrorMessage());
/// ```
/// ```expect
/// string(12) "U_ZERO_ERROR"
/// string(%d) "IntlTimeZone::getDisplayName(): wrong display type: U_ILLEGAL_ARGUMENT_ERROR"
/// string(%d) "IntlTimeZone::getDisplayName(): wrong display type: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_error_message(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
) -> Answer<Owner<PhpString>> {
    let message = error::object_message::<TimeZoneObject>(req, timezone.object());
    PhpString::new(req, &message).map(OrFalse::Value)
}

/// `intltz_get_gmt(): IntlTimeZone`.
///
/// ```php
/// var_dump(intltz_get_gmt()->getID(), IntlTimeZone::getGMT()->getRawOffset());
/// ```
/// ```expect
/// string(3) "GMT"
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_gmt(req: &mut Request) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
    special(req, 1)
}

/// `intltz_get_id(IntlTimeZone $timezone): string|false`.
///
/// ```php
/// var_dump(intltz_get_id(intltz_create_time_zone('America/New_York')));
/// try { intltz_get_id((new ReflectionClass('IntlTimeZone'))->newInstanceWithoutConstructor()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(16) "America/New_York"
/// Found unconstructed IntlTimeZone
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_id(req: &mut Request, timezone: Instance<IntlTimeZone>) -> Answer<Owner<PhpString>> {
    get_id(req, timezone.object())
}

/// `intltz_get_offset(IntlTimeZone $timezone, float $timestamp, bool $local, &$rawOffset, &$dstOffset): bool`.
///
/// ```php
/// $z = intltz_create_time_zone('Europe/Lisbon');
/// var_dump(intltz_get_offset($z, 1356998400000.0, false, $raw, $dst), $raw, $dst, $z->getOffset(1372636800000.0, true, $raw, $dst), $dst);
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// int(0)
/// bool(true)
/// int(3600000)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_get_offset(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
    timestamp: f64,
    local: bool,
    rawOffset: Ref<Untyped<Value>>,
    dstOffset: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    get_offset(
        req,
        timezone.object(),
        timestamp,
        local,
        (&rawOffset, &dstOffset),
    )
}

/// `intltz_get_raw_offset(IntlTimeZone $timezone): int`.
///
/// ```php
/// var_dump(intltz_get_raw_offset(intltz_create_time_zone('Asia/Kolkata')), intltz_create_time_zone('Asia/Kolkata')->getRawOffset());
/// ```
/// ```expect
/// int(19800000)
/// int(19800000)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_raw_offset(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
) -> Result<i64, Thrown> {
    read(req, timezone.object(), |zone| {
        i64::from(tz::php_intl_tz_int(zone, 0))
    })
}

/// `intltz_get_region(string $timezoneId): string|false`.
///
/// ```php
/// var_dump(intltz_get_region('Europe/Amsterdam'), IntlTimeZone::getRegion('Nowhere'), intl_get_error_message());
/// ```
/// ```expect
/// string(2) "NL"
/// bool(false)
/// string(%d) "IntlTimeZone::getRegion(): error obtaining region: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_get_region(req: &mut Request, timezoneId: PhpString) -> Answer<Owner<PhpString>> {
    get_region(req, timezoneId)
}

/// `intltz_get_tz_data_version(): string|false`.
///
/// ```php
/// var_dump(intltz_get_tz_data_version() === IntlTimeZone::getTZDataVersion(), (bool) preg_match('/^\d{4}[a-z]$/', intltz_get_tz_data_version()));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_tz_data_version(req: &mut Request) -> Answer<Owner<PhpString>> {
    get_tz_data_version(req)
}

/// `intltz_get_unknown(): IntlTimeZone`.
///
/// ```php
/// var_dump(intltz_get_unknown()->getID(), IntlTimeZone::getUnknown() == intltz_get_unknown());
/// ```
/// ```expect
/// string(11) "Etc/Unknown"
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_get_unknown(req: &mut Request) -> Result<Owner<Instance<IntlTimeZone>>, Thrown> {
    special(req, 2)
}

/// `intltz_get_windows_id(string $timezoneId): string|false`.
///
/// ```php
/// var_dump(intltz_get_windows_id('Europe/Amsterdam'), intltz_get_windows_id('Nowhere'), intl_get_error_message());
/// ```
/// ```expect
/// string(23) "W. Europe Standard Time"
/// bool(false)
/// string(74) "intltz_get_windows_id(): unknown system timezone: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function]
#[allow(non_snake_case)]
fn intltz_get_windows_id(req: &mut Request, timezoneId: PhpString) -> Answer<Owner<PhpString>> {
    lookup(req, timezoneId, &Lookup::Windows)
}

/// `intltz_get_id_for_windows_id(string $timezoneId, ?string $region = null): string|false`.
///
/// ```php
/// var_dump(intltz_get_id_for_windows_id('Pacific Standard Time'), intltz_get_id_for_windows_id('Pacific Standard Time', 'CA'), intltz_get_id_for_windows_id('Nowhere'));
/// ```
/// ```expect
/// string(19) "America/Los_Angeles"
/// string(17) "America/Vancouver"
/// bool(false)
/// ```
#[php_function]
#[allow(non_snake_case)]
fn intltz_get_id_for_windows_id(
    req: &mut Request,
    timezoneId: PhpString,
    #[php(default = null)] region: Option<PhpString>,
) -> Answer<Owner<PhpString>> {
    lookup(req, timezoneId, &Lookup::ForWindows(region))
}

/// `intltz_has_same_rules(IntlTimeZone $timezone, IntlTimeZone $other): bool`.
///
/// ```php
/// var_dump(intltz_has_same_rules(intltz_create_time_zone('Europe/Lisbon'), intltz_create_time_zone('Europe/Lisbon')), intltz_get_gmt()->hasSameRules(intltz_create_time_zone('Asia/Tokyo')));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_has_same_rules(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
    other: Instance<IntlTimeZone>,
) -> Result<bool, Thrown> {
    has_same_rules(req, timezone.object(), other.object())
}

/// `intltz_to_date_time_zone(IntlTimeZone $timezone): DateTimeZone|false`.
///
/// ```php
/// var_dump(intltz_to_date_time_zone(intltz_create_time_zone('Europe/Berlin'))->getName(), intltz_create_time_zone('GMT+05:30')->toDateTimeZone()->getName());
/// ```
/// ```expect
/// string(13) "Europe/Berlin"
/// string(6) "+05:30"
/// ```
#[php_function(before_args = error::reset)]
fn intltz_to_date_time_zone(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
) -> Answer<Owner<Instance<DateTimeZone>>> {
    to_date_time_zone(req, timezone.object())
}

/// `intltz_use_daylight_time(IntlTimeZone $timezone): bool`.
///
/// ```php
/// var_dump(intltz_use_daylight_time(intltz_create_time_zone('Europe/Paris')), intltz_create_time_zone('Asia/Tokyo')->useDaylightTime());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn intltz_use_daylight_time(
    req: &mut Request,
    timezone: Instance<IntlTimeZone>,
) -> Result<bool, Thrown> {
    read(req, timezone.object(), |zone| {
        tz::php_intl_tz_int(zone, 2) != 0
    })
}

/// `intltz_get_iana_id(string $timezoneId): string|false`.
///
/// ```php
/// var_dump(intltz_get_iana_id('Asia/Calcutta'), IntlTimeZone::getIanaID('Nowhere'));
/// ```
/// ```expect
/// string(12) "Asia/Kolkata"
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
#[allow(non_snake_case)]
fn intltz_get_iana_id(req: &mut Request, timezoneId: PhpString) -> Answer<Owner<PhpString>> {
    lookup(req, timezoneId, &Lookup::Iana)
}

/// The `intltz_*` block of `php_intl.stub.php`, in its order.
pub(crate) fn functions(b: ExtensionBuilder) -> ExtensionBuilder {
    b.functions(&[
        intltz_count_equivalent_ids::DECLARED,
        intltz_create_default::DECLARED,
        intltz_create_enumeration::DECLARED,
        intltz_create_time_zone::DECLARED,
        intltz_create_time_zone_id_enumeration::DECLARED,
        intltz_from_date_time_zone::DECLARED,
        intltz_get_canonical_id::DECLARED,
        intltz_get_display_name::DECLARED,
        intltz_get_dst_savings::DECLARED,
        intltz_get_equivalent_id::DECLARED,
        intltz_get_error_code::DECLARED,
        intltz_get_error_message::DECLARED,
        intltz_get_gmt::DECLARED,
        intltz_get_id::DECLARED,
        intltz_get_offset::DECLARED,
        intltz_get_raw_offset::DECLARED,
        intltz_get_region::DECLARED,
        intltz_get_tz_data_version::DECLARED,
        intltz_get_unknown::DECLARED,
        intltz_get_windows_id::DECLARED,
        intltz_get_id_for_windows_id::DECLARED,
        intltz_has_same_rules::DECLARED,
        intltz_to_date_time_zone::DECLARED,
        intltz_use_daylight_time::DECLARED,
        intltz_get_iana_id::DECLARED,
    ])
}
