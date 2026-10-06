//! `DateInterval` and its two procedural natives: the ISO 8601 duration
//! `timelib_strtointerval` reads, the relative part of a parse, the
//! format letters of `date_interval_format`, and the serialised form.

use php_ext::abi::{Instance, OrFalse, This};
use php_ext::attr::{php_class, php_function};
use php_ext::call::new_object;
use php_ext::coerce::{to_double, to_long, to_string};
use php_ext::raising::{Level, Thrown, docref, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::datetime::restore_properties;
use crate::ffi::{self, RelTime, Time, UNSET};
use crate::obj::{
    CIVIL, INTERVAL, IntervalObj, WALL, class_id, hold_diff, interval_fields, interval_of,
    new_interval, rows_array, std_rows,
};

/// `date_interval_initialize(&rt, format, len)`: the period of an ISO
/// 8601 duration, or the difference of its two dates, or php's
/// `DateMalformedIntervalStringException`.
fn initialize(req: &mut Request, text: &[u8]) -> Result<RelTime, Thrown> {
    let parsed = ffi::strtointerval(text);
    let shown = String::from_utf8_lossy(text);
    if !parsed.errors.errors.is_empty() {
        return Err(throw(
            req,
            "DateMalformedIntervalStringException",
            &format!("Unknown or bad format ({shown})"),
        ));
    }
    if let Some(period) = parsed.period {
        return Ok(period);
    }
    if let (Some(mut b), Some(mut e)) = (parsed.begin, parsed.end) {
        b.update_ts(None);
        e.update_ts(None);
        return Ok(b.diff(&e));
    }
    Err(throw(
        req,
        "DateMalformedIntervalStringException",
        &format!("Failed to parse interval ({shown})"),
    ))
}

/// The relative part of a parse of `text`, or its first error.
fn from_date_string(text: &[u8]) -> Result<(RelTime, bool), (i32, char, String)> {
    let (t, errors) = Time::strtotime(text);
    if let Some(e) = errors.errors.first() {
        let c = if e.character == 0 {
            ' '
        } else {
            char::from(e.character)
        };
        return Err((e.position, c, String::from_utf8_lossy(&e.text).into_owned()));
    }
    let absolute = t.have_date != 0 || t.have_time != 0 || t.have_zone != 0;
    Ok((t.relative, absolute))
}

/// `date_interval_create_from_date_string(string $datetime):
/// DateInterval|false`: php's warnings and `false` for a string that is
/// not a relative time.
///
/// ```php
/// var_dump(date_interval_create_from_date_string('3 days 4 hours'), date_interval_create_from_date_string('next weekday'));
/// var_dump(date_interval_create_from_date_string('foo'), date_interval_create_from_date_string('2020-01-01'));
/// ```
/// ```expect
/// object(DateInterval)#1 (2) {
///   ["from_string"]=>
///   bool(true)
///   ["date_string"]=>
///   string(14) "3 days 4 hours"
/// }
/// object(DateInterval)#2 (2) {
///   ["from_string"]=>
///   bool(true)
///   ["date_string"]=>
///   string(12) "next weekday"
/// }
///
/// Warning: date_interval_create_from_date_string(): Unknown or bad format (foo) at position 0 (f): The timezone could not be found in the database in %s on line 3
///
/// Warning: date_interval_create_from_date_string(): String '2020-01-01' contains non-relative elements in %s on line 3
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn date_interval_create_from_date_string(
    req: &mut Request,
    datetime: PhpString,
) -> Result<OrFalse<Owner<Instance<DateInterval>>>, Thrown> {
    let text = datetime.as_bytes();
    let shown = String::from_utf8_lossy(text).into_owned();
    match from_date_string(text) {
        Err((at, c, message)) => {
            docref(
                req,
                Level::Warning,
                &format!("Unknown or bad format ({shown}) at position {at} ({c}): {message}"),
            );
            Ok(OrFalse::False)
        }
        Ok((_, true)) => {
            docref(
                req,
                Level::Warning,
                &format!("String '{shown}' contains non-relative elements"),
            );
            Ok(OrFalse::False)
        }
        Ok((rel, false)) => new_interval(req, rel, CIVIL, Some(text.to_vec())).map(OrFalse::Value),
    }
}

/// `date_interval_format(format, len, t)`: `%` and a letter each
/// replaced by its field.
fn interval_format(format: &[u8], t: &RelTime) -> Vec<u8> {
    let mut out = Vec::new();
    let mut spec = false;
    #[allow(clippy::cast_possible_truncation)]
    let c_int = |n: i64| i64::from(n as i32);
    for &c in format {
        if !spec {
            if c == b'%' {
                spec = true;
            } else {
                out.push(c);
            }
            continue;
        }
        spec = false;
        let piece = match c {
            b'Y' => format!("{:02}", c_int(t.y)),
            b'y' => c_int(t.y).to_string(),
            b'M' => format!("{:02}", c_int(t.m)),
            b'm' => c_int(t.m).to_string(),
            b'D' => format!("{:02}", c_int(t.d)),
            b'd' => c_int(t.d).to_string(),
            b'H' => format!("{:02}", c_int(t.h)),
            b'h' => c_int(t.h).to_string(),
            b'I' => format!("{:02}", c_int(t.i)),
            b'i' => c_int(t.i).to_string(),
            b'S' => format!("{:02}", t.s),
            b's' => t.s.to_string(),
            b'F' => format!("{:06}", t.us),
            b'f' => t.us.to_string(),
            b'a' => {
                if c_int(t.days) == UNSET {
                    "(unknown)".to_owned()
                } else {
                    c_int(t.days).to_string()
                }
            }
            b'r' => (if t.invert != 0 { "-" } else { "" }).to_owned(),
            b'R' => (if t.invert != 0 { "-" } else { "+" }).to_owned(),
            b'%' => "%".to_owned(),
            other => {
                out.push(b'%');
                out.push(other);
                continue;
            }
        };
        out.extend_from_slice(piece.as_bytes());
    }
    out
}

/// `date_interval_format` and `DateInterval::format`.
fn format_of(
    req: &mut Request,
    object: PhpObject,
    format: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let (diff, _) = interval_of(req, object)?;
    PhpString::new(req, &interval_format(format.as_bytes(), &diff))
}

/// `date_interval_format(DateInterval $object, string $format): string`,
/// and `DateInterval::format`.
///
/// ```php
/// $i = new DateInterval('P1Y2M3DT4H5M6S');
/// var_dump(date_interval_format($i, '%Y-%M-%D %H:%I:%S %y %m %d %h %i %s %F %f %a %R%r %% %z'), $i->format(''));
/// $d = (new DateTime('2000-01-01'))->diff(new DateTime('1999-12-25 12:00:00.25'));
/// var_dump($d->format('%R%a days %H hours %F'), $d->format('%r%d'));
/// ```
/// ```expect
/// string(55) "01-02-03 04:05:06 1 2 3 4 5 6 000000 0 (unknown) + % %z"
/// string(0) ""
/// string(23) "-6 days 11 hours 750000"
/// string(2) "-6"
/// ```
#[php_function]
fn date_interval_format(
    req: &mut Request,
    object: Instance<DateInterval>,
    format: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    format_of(req, object.object(), format)
}

/// A hash row by name, dereferenced.
fn find(hash: PhpArray, name: &[u8]) -> Option<Value> {
    hash.find(name).map(|v| v.deref())
}

/// Whether a value's type is at most `IS_STRING`, what the hash readers
/// take.
fn scalar(value: &Value) -> bool {
    matches!(
        value.as_ref(),
        ValueRef::Null
            | ValueRef::Bool(_)
            | ValueRef::Int(_)
            | ValueRef::Float(_)
            | ValueRef::Str(_)
    )
}

/// `DATE_A64I` over the value's string: `strtoll`'s leading number.
fn a64i(req: &mut Request, value: &Value) -> Result<i64, Thrown> {
    let s = to_string(req, value)?;
    let text = String::from_utf8_lossy(s.as_bytes()).into_owned();
    s.give_up(req);
    let t = text.trim_start();
    let (sign, digits) = match t.as_bytes().first() {
        Some(b'-') => (-1i64, &t[1..]),
        Some(b'+') => (1, &t[1..]),
        _ => (1, t),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    let n = digits[..end].parse::<i64>().map_or(
        if end == 0 {
            0
        } else if sign < 0 {
            i64::MIN
        } else {
            i64::MAX
        },
        |n| n * sign,
    );
    Ok(n)
}

/// `php_date_interval_initialize_from_hash`: a `date_string` read again
/// through the parser, or every field read back with php's defaults.
///
/// ```php
/// $i = unserialize('O:12:"DateInterval":2:{s:1:"d";i:4;s:1:"f";i:9999999999990;}');
/// var_dump($i->d, $i->f);
/// ```
/// ```expect
/// Warning: The float 9.99999999999E+18 is not representable as an int, cast occurred in %s on line 2
/// int(4)
/// float(-8446744073719.552)
/// ```
fn from_hash(req: &mut Request, object: PhpObject, hash: PhpArray) -> Result<(), Thrown> {
    if let Some(text) =
        find(hash, b"date_string").and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    {
        let (t, errors) = Time::strtotime(&text);
        if let Some(e) = errors.errors.first() {
            let c = if e.character == 0 {
                ' '
            } else {
                char::from(e.character)
            };
            return Err(throw_error(
                req,
                "Error",
                &format!(
                    "Unknown or bad format ({}) at position {} ({c}) while unserializing: {}",
                    String::from_utf8_lossy(&text),
                    e.position,
                    String::from_utf8_lossy(&e.text)
                ),
            ));
        }
        if let Some(payload) = object.native_mut::<IntervalObj>(req) {
            payload.diff = Some(t.relative);
            payload.civil_or_wall = CIVIL;
            payload.from_string = true;
            payload.date_string = Some(text);
        }
        return hold_diff(req, object);
    }
    let mut diff = RelTime::default();
    let long_field = |req: &mut Request, name: &[u8], default: i64| -> Result<i64, Thrown> {
        match find(hash, name) {
            Some(v) if scalar(&v) => to_long(req, &v),
            _ => Ok(default),
        }
    };
    diff.y = long_field(req, b"y", -1)?;
    diff.m = long_field(req, b"m", -1)?;
    diff.d = long_field(req, b"d", -1)?;
    diff.h = long_field(req, b"h", -1)?;
    diff.i = long_field(req, b"i", -1)?;
    diff.s = long_field(req, b"s", -1)?;
    // `zend_dval_to_lval(zval_get_double(f) * 1000000.0)`: past the
    // long's range, php's warning and the modular long.
    if let Some(f) = find(hash, b"f") {
        let us = to_double(req, &f)? * 1_000_000.0;
        diff.us = to_long(req, &Value::float(us))?;
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        diff.weekday = long_field(req, b"weekday", -1)? as i32;
        diff.weekday_behavior = long_field(req, b"weekday_behavior", -1)? as i32;
        diff.first_last_day_of = long_field(req, b"first_last_day_of", -1)? as i32;
        diff.invert = long_field(req, b"invert", 0)? as i32;
    }
    diff.days = match find(hash, b"days") {
        Some(v) if matches!(v.as_ref(), ValueRef::Bool(false)) => UNSET,
        Some(v) if scalar(&v) => a64i(req, &v)?,
        _ => -1,
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        diff.special_type = long_field(req, b"special_type", 0)? as u32;
        diff.have_weekday_relative = long_field(req, b"have_weekday_relative", 0)? as u32;
        diff.have_special_relative = long_field(req, b"have_special_relative", 0)? as u32;
    }
    diff.special_amount = match find(hash, b"special_amount") {
        Some(v) if scalar(&v) => a64i(req, &v)?,
        _ => -1,
    };
    let civil = match find(hash, b"civil_or_wall") {
        Some(v) => to_long(req, &v)?,
        None => CIVIL,
    };
    if let Some(payload) = object.native_mut::<IntervalObj>(req) {
        payload.diff = Some(diff);
        payload.civil_or_wall = civil;
        payload.from_string = false;
        payload.date_string = None;
    }
    hold_diff(req, object)
}

/// The names `restore_custom_dateinterval_properties` passes over.
const INTERNAL: [&[u8]; 11] = [
    b"date_string",
    b"from_string",
    b"y",
    b"m",
    b"d",
    b"h",
    b"i",
    b"s",
    b"f",
    b"invert",
    b"days",
];

/// `class DateInterval`, over [`IntervalObj`] and the table that answers
/// its fields as properties.
///
/// ```php
/// var_dump((new DateInterval('P1Y2M3DT4H'))->format('%y-%m-%d %h %R %%'));
/// ```
/// ```expect
/// string(11) "1-2-3 4 + %"
/// ```
#[php_class(size = 80, payload = IntervalObj, handlers = INTERVAL)]
pub(crate) struct DateInterval;

#[php_class]
impl DateInterval {
    /// `DateInterval::__construct(string $duration)`.
    ///
    /// ```php
    /// var_dump(new DateInterval('P1Y2M3DT4H5M6S'), (new DateInterval('P2W'))->d, (new DateInterval('PT36H'))->h);
    /// var_dump((new DateInterval('2008-05-11T15:30:00Z/2008-06-12T16:31:01Z'))->format('%m %d %h %i %s %a'));
    /// try { new DateInterval('P1Q'); } catch (DateMalformedIntervalStringException $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// try { new DateInterval('2008-05-11T15:30:00Z'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(DateInterval)#1 (10) {
    ///   ["y"]=>
    ///   int(1)
    ///   ["m"]=>
    ///   int(2)
    ///   ["d"]=>
    ///   int(3)
    ///   ["h"]=>
    ///   int(4)
    ///   ["i"]=>
    ///   int(5)
    ///   ["s"]=>
    ///   int(6)
    ///   ["f"]=>
    ///   float(0)
    ///   ["invert"]=>
    ///   int(0)
    ///   ["days"]=>
    ///   bool(false)
    ///   ["from_string"]=>
    ///   bool(false)
    /// }
    /// int(14)
    /// int(36)
    /// string(12) "1 1 1 1 1 32"
    /// DateMalformedIntervalStringException: Unknown or bad format (P1Q)
    /// Failed to parse interval (2008-05-11T15:30:00Z)
    /// ```
    ///
    /// An interval holds its `timelib_rel_time` on the request's heap, as
    /// php's timelib allocates it, made by the constructor, a clone, a
    /// diff, `createFromDateString` and `__unserialize`, and freed with
    /// the object.
    ///
    /// ```php
    /// $i = new DateInterval('P1D'); $a = new DateTime('2020-01-01'); $b = new DateTime('2021-01-01');
    /// $w = [$a->diff($b), DateInterval::createFromDateString('3 days'), unserialize(serialize($i)), clone $i]; unset($w);
    /// $m = memory_get_usage(); $c = clone $i; $d = $a->diff($b); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $s = DateInterval::createFromDateString('3 days'); $u = unserialize(serialize($i)); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $n = new DateInterval('P2D'); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($c, $d, $s, $u, $n); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 352
    /// 352
    /// 176
    /// -880
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, duration: PhpString) -> Result<(), Thrown> {
        let diff = initialize(req, duration.as_bytes())?;
        if let Some(payload) = this.object().native_mut::<IntervalObj>(req) {
            payload.diff = Some(diff);
            payload.civil_or_wall = WALL;
        }
        hold_diff(req, this.object())
    }

    /// `DateInterval::createFromDateString(string $datetime): DateInterval`:
    /// the same, thrown as `DateMalformedIntervalStringException`.
    ///
    /// ```php
    /// var_dump(DateInterval::createFromDateString('last day of next month'));
    /// try { DateInterval::createFromDateString('tomorrow noon'); } catch (DateMalformedIntervalStringException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(DateInterval)#1 (2) {
    ///   ["from_string"]=>
    ///   bool(true)
    ///   ["date_string"]=>
    ///   string(22) "last day of next month"
    /// }
    /// String 'tomorrow noon' contains non-relative elements
    /// ```
    #[php(tentative)]
    fn createFromDateString(
        req: &mut Request,
        datetime: PhpString,
    ) -> Result<Owner<Instance<DateInterval>>, Thrown> {
        let text = datetime.as_bytes();
        let shown = String::from_utf8_lossy(text).into_owned();
        match from_date_string(text) {
            Err((at, c, message)) => Err(throw_error(
                req,
                "DateMalformedIntervalStringException",
                &format!("Unknown or bad format ({shown}) at position {at} ({c}): {message}"),
            )),
            Ok((_, true)) => Err(throw_error(
                req,
                "DateMalformedIntervalStringException",
                &format!("String '{shown}' contains non-relative elements"),
            )),
            Ok((rel, false)) => new_interval(req, rel, CIVIL, Some(text.to_vec())),
        }
    }

    /// `DateInterval::format(string $format): string`,
    /// `date_interval_format`'s body.
    #[php(tentative)]
    fn format(
        req: &mut Request,
        this: This<Self>,
        format: PhpString,
    ) -> Result<Owner<PhpString>, Thrown> {
        format_of(req, this.object(), format)
    }

    /// `DateInterval::__serialize(): array`.
    ///
    /// ```php
    /// var_dump((new DateInterval('PT1M'))->__serialize(), DateInterval::createFromDateString('next monday')->__serialize());
    /// ```
    /// ```expect
    /// array(10) {
    ///   ["y"]=>
    ///   int(0)
    ///   ["m"]=>
    ///   int(0)
    ///   ["d"]=>
    ///   int(0)
    ///   ["h"]=>
    ///   int(0)
    ///   ["i"]=>
    ///   int(1)
    ///   ["s"]=>
    ///   int(0)
    ///   ["f"]=>
    ///   float(0)
    ///   ["invert"]=>
    ///   int(0)
    ///   ["days"]=>
    ///   bool(false)
    ///   ["from_string"]=>
    ///   bool(false)
    /// }
    /// array(2) {
    ///   ["from_string"]=>
    ///   bool(true)
    ///   ["date_string"]=>
    ///   string(11) "next monday"
    /// }
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let object = this.object();
        let (diff, _) = interval_of(req, object)?;
        let text = object
            .native::<IntervalObj>(req)
            .and_then(|i| i.date_string.clone());
        let fields = interval_fields(req, &diff, text.as_deref())?;
        let own = std_rows(req, object)?;
        rows_array(req, &own, fields, true)
    }

    /// `DateInterval::__unserialize(array $data): void`.
    ///
    /// ```php
    /// $i = new DateInterval('P1D');
    /// $i->__unserialize(['y' => 5, 'invert' => 1, 'days' => 10, 'extra' => 'x']);
    /// var_dump($i);
    /// ```
    /// ```expect
    /// Deprecated: Creation of dynamic property DateInterval::$extra is deprecated in %s on line 3
    /// object(DateInterval)#1 (11) {
    ///   ["extra"]=>
    ///   string(1) "x"
    ///   ["y"]=>
    ///   int(5)
    ///   ["m"]=>
    ///   int(-1)
    ///   ["d"]=>
    ///   int(-1)
    ///   ["h"]=>
    ///   int(-1)
    ///   ["i"]=>
    ///   int(-1)
    ///   ["s"]=>
    ///   int(-1)
    ///   ["f"]=>
    ///   float(0)
    ///   ["invert"]=>
    ///   int(1)
    ///   ["days"]=>
    ///   int(10)
    ///   ["from_string"]=>
    ///   bool(false)
    /// }
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let object = this.object();
        from_hash(req, object, data)?;
        restore_properties(req, object, data, &INTERNAL)
    }

    /// `DateInterval::__wakeup(): void`, deprecated since 8.5.
    ///
    /// ```php
    /// $i = new DateInterval('P1D');
    /// @$i->__wakeup();
    /// var_dump($i->d);
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let object = this.object();
        // `Z_OBJPROP_P`, which `date_object_get_properties_interval` fills
        // with the fields before it answers.
        let own = std_rows(req, object)?;
        let state = object
            .native::<IntervalObj>(req)
            .and_then(|i| i.diff.map(|d| (d, i.date_string.clone())));
        let fields = match state {
            Some((diff, text)) => interval_fields(req, &diff, text.as_deref())?,
            None => Vec::new(),
        };
        let hash = rows_array(req, &own, fields, false)?;
        let done = from_hash(req, object, *hash);
        hash.give_up(req);
        done
    }

    /// `DateInterval::__set_state(array $array): DateInterval`.
    ///
    /// ```php
    /// var_dump(DateInterval::__set_state(['y' => 1, 'm' => '2', 'd' => 3.9, 'days' => false, 'f' => 0.5]));
    /// var_export(new DateInterval('P1D'));
    /// echo "\n";
    /// var_export(DateInterval::createFromDateString('+1 day'));
    /// ```
    /// ```expect
    /// object(DateInterval)#1 (10) {
    ///   ["y"]=>
    ///   int(1)
    ///   ["m"]=>
    ///   int(2)
    ///   ["d"]=>
    ///   int(3)
    ///   ["h"]=>
    ///   int(-1)
    ///   ["i"]=>
    ///   int(-1)
    ///   ["s"]=>
    ///   int(-1)
    ///   ["f"]=>
    ///   float(0.5)
    ///   ["invert"]=>
    ///   int(0)
    ///   ["days"]=>
    ///   bool(false)
    ///   ["from_string"]=>
    ///   bool(false)
    /// }
    /// \DateInterval::__set_state(array(
    ///    'y' => 0,
    ///    'm' => 0,
    ///    'd' => 1,
    ///    'h' => 0,
    ///    'i' => 0,
    ///    's' => 0,
    ///    'f' => 0.0,
    ///    'invert' => 0,
    ///    'days' => false,
    ///    'from_string' => false,
    /// ))
    /// \DateInterval::__set_state(array(
    ///    'from_string' => true,
    ///    'date_string' => '+1 day',
    /// ))
    /// ```
    #[php(tentative)]
    fn __set_state(
        req: &mut Request,
        array: PhpArray,
    ) -> Result<Owner<Instance<DateInterval>>, Thrown> {
        let class = class_id(req, "DateInterval")?;
        let object = new_object(req, class)?;
        if let Err(thrown) = from_hash(req, *object, array) {
            object.give_up(req);
            return Err(thrown);
        }
        Ok(object.into_instance())
    }
}
