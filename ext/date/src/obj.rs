//! The five classes' native payloads and their handler tables: php's
//! `php_date_obj`, `php_timezone_obj`, `php_interval_obj` and
//! `php_period_obj` as typed payloads on the object (design §5.4), and
//! the slots `date_register_classes` fills. What they share: the
//! uninitialised-object error, the class lookups, the objects made
//! around a payload, and the rows the property readers see.

use std::cmp::Ordering;
use std::sync::Arc;

use php_ext::abi::{
    Class, CloneObj, Instance, ObjectHandlers, PropertyPlace, PropertyPurpose, PropertyRead,
    PropertyTest, Std,
};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{to_double, to_long, truthy};
use php_ext::decl::Visibility;
use php_ext::raising::{Level, Thrown, raise, throw};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::ffi::{RelTime, Time, UNSET, ZONETYPE_ABBR, ZONETYPE_ID, ZONETYPE_OFFSET, Zone};
use crate::format::date_format;
use crate::interval::DateInterval;
use crate::timezone::DateTimeZone;

/// `PHP_DATE_CIVIL` and `PHP_DATE_WALL`.
pub(super) const CIVIL: i64 = 1;
pub(super) const WALL: i64 = 2;

/// `php_date_obj`: the time, none until a constructor set it, and the
/// values the last property read made, which the rows it answered name.
#[derive(Default)]
pub(crate) struct DateObj {
    pub time: Option<Time>,
    rows: Vec<Value>,
}

/// What a `DateTimeZone` holds, `php_timezone_obj`'s `type` and `tzi`.
#[derive(Clone)]
pub(crate) enum TzKind {
    Id(Arc<Zone>),
    Offset(i64),
    Abbr {
        utc_offset: i64,
        abbr: Vec<u8>,
        dst: i32,
    },
}

impl TzKind {
    /// `TIMELIB_ZONETYPE_*`.
    pub(super) fn zone_type(&self) -> i64 {
        i64::from(match self {
            TzKind::Offset(_) => ZONETYPE_OFFSET,
            TzKind::Abbr { .. } => ZONETYPE_ABBR,
            TzKind::Id(_) => ZONETYPE_ID,
        })
    }

    /// `php_timezone_to_string`.
    pub(super) fn name(&self) -> Vec<u8> {
        match self {
            TzKind::Id(zone) => zone.name().to_vec(),
            TzKind::Offset(offset) => offset_string(*offset).into_bytes(),
            TzKind::Abbr { abbr, .. } => abbr.clone(),
        }
    }
}

/// `php_timezone_obj`.
#[derive(Default)]
pub(crate) struct TzObj {
    pub tz: Option<TzKind>,
    rows: Vec<Value>,
}

/// `php_interval_obj`.
#[derive(Default)]
pub(crate) struct IntervalObj {
    pub diff: Option<RelTime>,
    pub civil_or_wall: i64,
    pub from_string: bool,
    pub date_string: Option<Vec<u8>>,
    rows: Vec<Value>,
}

/// `php_period_obj`.
#[derive(Default)]
pub(crate) struct PeriodObj {
    pub start: Option<Time>,
    pub start_ce: Option<ClassId>,
    pub current: Option<Time>,
    pub end: Option<Time>,
    pub interval: Option<RelTime>,
    pub recurrences: i64,
    pub include_start_date: bool,
    pub include_end_date: bool,
    pub initialized: bool,
    /// How many steps past its rewind `current` stands, the index the
    /// last iterator over the period asked for; none before any walk.
    pub cursor: Option<usize>,
    rows: Vec<Value>,
}

/// `date_create_tz_offset_str`: `+05:00`, or `+05:00:01` where the
/// offset has seconds.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn offset_string(offset: i64) -> String {
    let seconds = offset % 60;
    let sign = if offset < 0 { '-' } else { '+' };
    let hours = ((offset / 3600) as i32).abs();
    let minutes = (((offset % 3600) as i32) / 60).abs();
    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{:02}", seconds.abs())
    }
}

/// A class by name, the ones this crate declares.
pub(super) fn class_id(req: &mut Request, name: &str) -> Result<ClassId, Thrown> {
    match lookup_class(req, name.as_bytes())? {
        Some(class) => Ok(class),
        None => Err(throw(req, "Error", &format!("Class \"{name}\" not found"))),
    }
}

/// `ce->name`.
pub(super) fn class_name(req: &Request, class: ClassId) -> String {
    req.class_decl(class)
        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
        .unwrap_or_default()
}

/// The class's parent, linked.
pub(super) fn parent_of(req: &mut Request, class: ClassId) -> Option<ClassId> {
    let parent = req.class_decl(class)?.parent.clone()?;
    lookup_class(req, &parent.0).ok().flatten()
}

/// Whether the class is an internal one, php's `ZEND_INTERNAL_CLASS`.
fn is_internal(req: &Request, class: ClassId) -> bool {
    req.class_extension(class).is_some()
}

/// `date_throw_uninitialized_error(ce)`: the `DateObjectError` for an
/// object whose constructor never ran, naming the internal class a
/// user class inherits.
pub(super) fn uninitialized(req: &mut Request, class: ClassId) -> Thrown {
    let name = class_name(req, class);
    let tail = "correctly initialized by calling parent::__construct() in its constructor";
    let message = if is_internal(req, class) {
        format!("Object of type {name} has not been {tail}")
    } else {
        let mut at = class;
        while !is_internal(req, at) {
            match parent_of(req, at) {
                Some(parent) => at = parent,
                None => break,
            }
        }
        if is_internal(req, at) {
            let base = class_name(req, at);
            format!("Object of type {name} (inheriting {base}) has not been {tail}")
        } else {
            format!("Object of type {name} not been {tail}")
        }
    };
    throw(req, "DateObjectError", &message)
}

/// A clone of the object's time, or the uninitialised error.
pub(super) fn time_of(req: &mut Request, object: PhpObject) -> Result<Time, Thrown> {
    let time = object.native::<DateObj>(req).and_then(|d| d.time.clone());
    match time {
        Some(time) => Ok(time),
        None => Err(uninitialized(req, object.class())),
    }
}

/// The object's time replaced.
pub(super) fn set_time(req: &mut Request, object: PhpObject, time: Option<Time>) {
    if let Some(payload) = object.native_mut::<DateObj>(req) {
        payload.time = time;
    }
}

/// `object_init_ex(zv, ce)` with its time set, one owner, the caller's.
pub(super) fn new_date(req: &mut Request, class: ClassId, time: Time) -> Result<PhpObject, Thrown> {
    let object = new_object(req, class)?;
    set_time(req, object, Some(time));
    Ok(object)
}

/// A new `DateTimeZone` holding `kind`, one owner, the caller's.
pub(super) fn new_timezone(
    req: &mut Request,
    kind: TzKind,
) -> Result<Instance<DateTimeZone>, Thrown> {
    let class = class_id(req, "DateTimeZone")?;
    let object = new_object(req, class)?;
    if let Some(payload) = object.native_mut::<TzObj>(req) {
        payload.tz = Some(kind);
    }
    Ok(Instance::of(object))
}

/// The zone a `DateTimeZone` holds, or the uninitialised error.
pub(super) fn tz_of(req: &mut Request, object: PhpObject) -> Result<TzKind, Thrown> {
    let kind = object.native::<TzObj>(req).and_then(|t| t.tz.clone());
    match kind {
        Some(kind) => Ok(kind),
        None => Err(uninitialized(req, object.class())),
    }
}

/// `set_timezone_from_timelib_time`: the zone a time carries.
pub(super) fn tz_from_time(t: &Time) -> Option<TzKind> {
    match t.zone_type {
        ZONETYPE_ID => t.zone().map(|z| TzKind::Id(Arc::clone(z))),
        ZONETYPE_OFFSET => Some(TzKind::Offset(i64::from(t.z))),
        ZONETYPE_ABBR => Some(TzKind::Abbr {
            utc_offset: i64::from(t.z),
            abbr: t.tz_abbr().to_vec(),
            dst: t.dst,
        }),
        _ => None,
    }
}

/// A new `DateInterval` over `diff`.
pub(super) fn new_interval(
    req: &mut Request,
    diff: RelTime,
    civil_or_wall: i64,
    date_string: Option<Vec<u8>>,
) -> Result<Instance<DateInterval>, Thrown> {
    let class = class_id(req, "DateInterval")?;
    let object = new_object(req, class)?;
    if let Some(payload) = object.native_mut::<IntervalObj>(req) {
        payload.diff = Some(diff);
        payload.civil_or_wall = civil_or_wall;
        payload.from_string = date_string.is_some();
        payload.date_string = date_string;
    }
    Ok(Instance::of(object))
}

/// The relative time a `DateInterval` holds, or the uninitialised error.
pub(super) fn interval_of(req: &mut Request, object: PhpObject) -> Result<(RelTime, i64), Thrown> {
    let diff = object
        .native::<IntervalObj>(req)
        .and_then(|i| i.diff.map(|d| (d, i.civil_or_wall)));
    match diff {
        Some(diff) => Ok(diff),
        None => Err(uninitialized(req, object.class())),
    }
}

/// `RETURN_OBJ_COPY`: the object with one more owner, answered as a `C`.
pub(super) fn again<C: Class>(req: &mut Request, object: PhpObject) -> Instance<C> {
    req.addref(&object.as_value());
    Instance::of(object)
}

/// `add_assoc_*`: one row under a string key.
pub(super) fn set_str(
    req: &mut Request,
    array: &mut PhpArray,
    key: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let key = Key::Str(PhpString::new(req, key)?);
    array.set(req, key, value)
}

/// `add_assoc_string`.
pub(super) fn set_string(
    req: &mut Request,
    array: &mut PhpArray,
    key: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let value = Value::string(PhpString::new(req, value)?);
    set_str(req, array, key, value)
}

/// A declared property's name as the readers key it: `\0*\0p` for a
/// protected one, `\0C\0p` for a private one of class `C`.
fn mangled(req: &mut Request, class: ClassId, name: PhpString) -> Result<PhpString, Thrown> {
    let mut at = Some(class);
    while let Some(c) = at {
        let found = req.class_decl(c).and_then(|decl| {
            decl.properties
                .iter()
                .find(|p| p.is_instance && *p.name.0 == *name.as_bytes())
                .map(|p| (p.visibility, decl.name.0.clone()))
        });
        if let Some((visibility, owner)) = found {
            let prefix: Vec<u8> = match visibility {
                Visibility::Public => return Ok(name),
                Visibility::Protected => b"\0*\0".to_vec(),
                Visibility::Private => [&b"\0"[..], &owner, b"\0"].concat(),
            };
            return PhpString::new(req, &[prefix.as_slice(), name.as_bytes()].concat());
        }
        at = parent_of(req, c);
    }
    Ok(name)
}

/// `zend_std_get_properties(object)` with each declared name mangled as
/// a reader keys it: views, no owner taken.
pub(super) fn std_rows(
    req: &mut Request,
    object: PhpObject,
) -> Result<Vec<(PhpString, Value)>, Thrown> {
    let rows = Std.get_properties(req, object)?;
    let mut out = Vec::with_capacity(rows.len());
    for (name, value) in rows {
        let key = mangled(req, object.class(), name)?;
        out.push((key, value));
    }
    Ok(out)
}

/// `zend_hash_str_update(props, name, value)` over rows: the value of a
/// row already named replaced where it stands, a new row appended.
fn update(rows: &mut Vec<(PhpString, Value)>, name: PhpString, value: Value) {
    match rows
        .iter_mut()
        .find(|(n, _)| n.as_bytes() == name.as_bytes())
    {
        Some(row) => row.1 = value,
        None => rows.push((name, value)),
    }
}

/// The rows as an array with its own owners, the table `__serialize`
/// returns and `get_debug_info` hands a dumper: the fields first where
/// `first`, and the object's own rows added behind them only where no
/// field took their name, as `add_common_properties` adds them.
pub(super) fn rows_array(
    req: &mut Request,
    own: &[(PhpString, Value)],
    fields: Vec<(&[u8], Value)>,
    first: bool,
) -> Result<PhpArray, Thrown> {
    let mut array = PhpArray::new(req)?;
    if !first {
        for (name, value) in own {
            req.addref(value);
            array.set(req, Key::Str(*name), *value)?;
        }
    }
    for (name, value) in fields {
        let key = Key::Str(PhpString::new(req, name)?);
        array.set(req, key, value)?;
    }
    if first {
        for (name, value) in own {
            let key = Key::Str(*name);
            if !array.contains(&key) {
                req.addref(value);
                array.set(req, key, *value)?;
            }
        }
    }
    Ok(array)
}

/// The rows a property reader sees: the object's own rows with the
/// fields updated into them. The values made for the fields are kept in
/// `keep`, which owns them until the next read or the object's release,
/// so the rows answered are views as the slot's contract asks.
fn rows_view(
    req: &mut Request,
    object: PhpObject,
    fields: Vec<(&[u8], Value)>,
    keep: fn(&mut Request, PhpObject) -> Option<&mut Vec<Value>>,
) -> Result<Vec<(PhpString, Value)>, Thrown> {
    let mut rows = std_rows(req, object)?;
    let mut made = Vec::with_capacity(fields.len());
    for (name, value) in fields {
        let name = PhpString::new(req, name)?;
        made.push(Value::string(name));
        made.push(value);
        update(&mut rows, name, value);
    }
    let old = keep(req, object).map(|rows| std::mem::replace(rows, made));
    for value in old.into_iter().flatten() {
        req.release(value);
    }
    Ok(rows)
}

/// What `free_obj` gives back of the kept rows.
fn release_rows(req: &mut Request, rows: Option<Vec<Value>>) {
    for value in rows.into_iter().flatten() {
        req.release(value);
    }
}

/// Whether the class declares `__debugInfo`, php's `ce->__debugInfo`.
fn has_debug_info(req: &mut Request, object: PhpObject) -> bool {
    Std.get_method(req, object, b"__debuginfo").is_some()
}

/// `date_object_to_hash`: `date`, and where the time is local its
/// `timezone_type` and `timezone`.
pub(super) fn date_fields(
    req: &mut Request,
    t: &Time,
) -> Result<Vec<(&'static [u8], Value)>, Thrown> {
    let text = date_format(b"x-m-d H:i:s.u", t, true);
    let mut fields = vec![(&b"date"[..], Value::string(PhpString::new(req, &text)?))];
    if t.is_localtime != 0 {
        fields.push((b"timezone_type", Value::int(i64::from(t.zone_type))));
        let zone: Vec<u8> = match t.zone_type {
            ZONETYPE_ID => t.zone().map(|z| z.name().to_vec()).unwrap_or_default(),
            ZONETYPE_OFFSET => offset_string(i64::from(t.z)).into_bytes(),
            ZONETYPE_ABBR => t.tz_abbr().to_vec(),
            _ => Vec::new(),
        };
        fields.push((b"timezone", Value::string(PhpString::new(req, &zone)?)));
    }
    Ok(fields)
}

/// `date_timezone_object_to_hash`.
pub(super) fn tz_fields(
    req: &mut Request,
    kind: &TzKind,
) -> Result<Vec<(&'static [u8], Value)>, Thrown> {
    Ok(vec![
        (&b"timezone_type"[..], Value::int(kind.zone_type())),
        (
            b"timezone",
            Value::string(PhpString::new(req, &kind.name())?),
        ),
    ])
}

/// `date_interval_object_to_hash`.
pub(super) fn interval_fields(
    req: &mut Request,
    diff: &RelTime,
    date_string: Option<&[u8]>,
) -> Result<Vec<(&'static [u8], Value)>, Thrown> {
    if let Some(text) = date_string {
        return Ok(vec![
            (&b"from_string"[..], Value::bool(true)),
            (b"date_string", Value::string(PhpString::new(req, text)?)),
        ]);
    }
    #[allow(clippy::cast_precision_loss)]
    let f = diff.us as f64 / 1_000_000.0;
    Ok(vec![
        (&b"y"[..], Value::int(diff.y)),
        (b"m", Value::int(diff.m)),
        (b"d", Value::int(diff.d)),
        (b"h", Value::int(diff.h)),
        (b"i", Value::int(diff.i)),
        (b"s", Value::int(diff.s)),
        (b"f", Value::float(f)),
        (b"invert", Value::int(i64::from(diff.invert))),
        (
            b"days",
            if diff.days == UNSET {
                Value::bool(false)
            } else {
                Value::int(diff.days)
            },
        ),
        (b"from_string", Value::bool(false)),
    ])
}

/// `create_date_period_datetime`: a date of the period's class over one
/// of its times, or `null`.
pub(super) fn period_date(
    req: &mut Request,
    t: Option<&Time>,
    class: Option<ClassId>,
) -> Result<Value, Thrown> {
    let (Some(t), Some(class)) = (t, class) else {
        return Ok(Value::null());
    };
    Ok(new_date(req, class, t.clone())?.as_value())
}

/// `date_period_object_to_hash`.
pub(super) fn period_fields(
    req: &mut Request,
    object: PhpObject,
) -> Result<Vec<(&'static [u8], Value)>, Thrown> {
    let Some(p) = object.native::<PeriodObj>(req) else {
        return Ok(Vec::new());
    };
    let (start, current, end) = (p.start.clone(), p.current.clone(), p.end.clone());
    let (class, interval) = (p.start_ce, p.interval);
    let (recurrences, include_start, include_end) =
        (p.recurrences, p.include_start_date, p.include_end_date);
    let start = period_date(req, start.as_ref(), class)?;
    let current = period_date(req, current.as_ref(), class)?;
    let end = period_date(req, end.as_ref(), class)?;
    let interval = match interval {
        Some(diff) => new_interval(req, diff, 0, None)?.object().as_value(),
        None => Value::null(),
    };
    Ok(vec![
        (&b"start"[..], start),
        (b"current", current),
        (b"end", end),
        (b"interval", interval),
        (b"recurrences", Value::int(recurrences)),
        (b"include_start_date", Value::bool(include_start)),
        (b"include_end_date", Value::bool(include_end)),
    ])
}

/// The rows `DateTime`, `DateTimeImmutable` and their subclasses show
/// every reader of their properties but `get_object_vars`: the object's
/// own and the three of `date_object_to_hash`.
pub(crate) struct DateHandlers;

/// `date_object_handlers_date`, `DateTime`'s and `DateTimeImmutable`'s.
pub(crate) static DATE: DateHandlers = DateHandlers;

impl ObjectHandlers for DateHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let rows = object
            .native_mut::<DateObj>(req)
            .map(|d| std::mem::take(&mut d.rows));
        release_rows(req, rows);
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_date)
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let (Some(a), Some(b)) = (left.as_object().copied(), right.as_object().copied()) else {
            return Std.compare(req, left, right);
        };
        let (Some(da), Some(db)) = (a.native::<DateObj>(req), b.native::<DateObj>(req)) else {
            return Std.compare(req, left, right);
        };
        let (Some(mut ta), Some(mut tb)) = (da.time.clone(), db.time.clone()) else {
            return Err(throw(
                req,
                "DateObjectError",
                "Trying to compare an incomplete DateTime or DateTimeImmutable object",
            ));
        };
        for t in [&mut ta, &mut tb] {
            if t.sse_uptodate == 0 {
                let zone = t.zone().cloned();
                t.update_ts(zone.as_ref());
            }
        }
        Ok(Some(ta.compare(&tb).cmp(&0)))
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        if has_debug_info(req, object) {
            return Std.get_debug_info(req, object);
        }
        let own = std_rows(req, object)?;
        let time = object.native::<DateObj>(req).and_then(|d| d.time.clone());
        let fields = match &time {
            Some(t) => date_fields(req, t)?,
            None => Vec::new(),
        };
        rows_array(req, &own, fields, false).map(Some)
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        // `date_object_get_properties_for`'s `default:`, the standard
        // table, for the debug reader and for `get_object_vars()`.
        if matches!(
            purpose,
            PropertyPurpose::Debug | PropertyPurpose::GetObjectVars
        ) {
            return Ok(None);
        }
        let time = object.native::<DateObj>(req).and_then(|d| d.time.clone());
        let fields = match &time {
            Some(t) => date_fields(req, t)?,
            None => Vec::new(),
        };
        rows_view(req, object, fields, |req, o| {
            o.native_mut::<DateObj>(req).map(|d| &mut d.rows)
        })
        .map(Some)
    }
}

/// `date_object_clone_date`: a fresh object of the class, its members
/// copied and `__clone` served, then the time copied into it.
pub(super) fn clone_date(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, DateObj::default())?;
    let time = object.native::<DateObj>(req).and_then(|d| d.time.clone());
    if time.is_some() {
        set_time(req, copy, time);
    }
    Ok(copy)
}

/// `DateTimeZone`'s table: its rows, its clone and its comparison.
pub(crate) struct TzHandlers;

/// `date_object_handlers_timezone`.
pub(crate) static TIMEZONE: TzHandlers = TzHandlers;

impl ObjectHandlers for TzHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let rows = object
            .native_mut::<TzObj>(req)
            .map(|t| std::mem::take(&mut t.rows));
        release_rows(req, rows);
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_timezone)
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let (Some(a), Some(b)) = (left.as_object().copied(), right.as_object().copied()) else {
            return Std.compare(req, left, right);
        };
        let (Some(ta), Some(tb)) = (a.native::<TzObj>(req), b.native::<TzObj>(req)) else {
            return Std.compare(req, left, right);
        };
        let (Some(ka), Some(kb)) = (ta.tz.clone(), tb.tz.clone()) else {
            return Err(throw(
                req,
                "DateObjectError",
                "Trying to compare uninitialized DateTimeZone objects",
            ));
        };
        if ka.zone_type() != kb.zone_type() {
            return Err(throw(
                req,
                "DateException",
                "Cannot compare two different kinds of DateTimeZone objects",
            ));
        }
        let same = match (&ka, &kb) {
            (TzKind::Offset(x), TzKind::Offset(y)) => x == y,
            _ => ka.name() == kb.name(),
        };
        // php answers 0 or 1, never -1: unequal zones are "greater".
        Ok(Some(if same {
            Ordering::Equal
        } else {
            Ordering::Greater
        }))
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        if has_debug_info(req, object) {
            return Std.get_debug_info(req, object);
        }
        let own = std_rows(req, object)?;
        let kind = object.native::<TzObj>(req).and_then(|t| t.tz.clone());
        let fields = match &kind {
            Some(kind) => tz_fields(req, kind)?,
            None => Vec::new(),
        };
        rows_array(req, &own, fields, false).map(Some)
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        // `date_object_get_properties_for`'s `default:`, the standard
        // table, for the debug reader and for `get_object_vars()`.
        if matches!(
            purpose,
            PropertyPurpose::Debug | PropertyPurpose::GetObjectVars
        ) {
            return Ok(None);
        }
        let kind = object.native::<TzObj>(req).and_then(|t| t.tz.clone());
        let fields = match &kind {
            Some(kind) => tz_fields(req, kind)?,
            None => Vec::new(),
        };
        rows_view(req, object, fields, |req, o| {
            o.native_mut::<TzObj>(req).map(|t| &mut t.rows)
        })
        .map(Some)
    }
}

/// `date_object_clone_timezone`.
fn clone_timezone(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, TzObj::default())?;
    let kind = object.native::<TzObj>(req).and_then(|t| t.tz.clone());
    if let Some(payload) = copy.native_mut::<TzObj>(req) {
        payload.tz = kind;
    }
    Ok(copy)
}

/// The names `DateInterval` answers from its relative time.
const INTERVAL_FIELDS: [&[u8]; 9] = [b"y", b"m", b"d", b"h", b"i", b"s", b"f", b"invert", b"days"];

/// `DateInterval`'s table: its fields read and written through the
/// relative time (`date_interval_read_property` and its kin), the rows
/// every reader sees, and php's refusal to compare two.
pub(crate) struct IntervalHandlers;

/// `date_object_handlers_interval`.
pub(crate) static INTERVAL: IntervalHandlers = IntervalHandlers;

impl IntervalHandlers {
    /// `date_interval_read_property` for one of its names: the field, or
    /// `None` for a name it does not answer.
    fn field(req: &Request, object: PhpObject, name: &[u8]) -> Option<Value> {
        let diff = object.native::<IntervalObj>(req)?.diff?;
        #[allow(clippy::cast_precision_loss)]
        let value = match name {
            b"y" => diff.y,
            b"m" => diff.m,
            b"d" => diff.d,
            b"h" => diff.h,
            b"i" => diff.i,
            b"s" => diff.s,
            b"f" => return Some(Value::float(diff.us as f64 / 1_000_000.0)),
            b"invert" => i64::from(diff.invert),
            b"days" => diff.days,
            _ => return None,
        };
        Some(if value == UNSET {
            Value::bool(false)
        } else {
            Value::int(value)
        })
    }
}

impl ObjectHandlers for IntervalHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let rows = object
            .native_mut::<IntervalObj>(req)
            .map(|i| std::mem::take(&mut i.rows));
        release_rows(req, rows);
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_interval)
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        match IntervalHandlers::field(req, object, name) {
            Some(value) => Ok(value),
            None => Std.read_property(req, object, name, read),
        }
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let initialized = object
            .native::<IntervalObj>(req)
            .is_some_and(|i| i.diff.is_some());
        let field = name.as_bytes().to_vec();
        if !initialized || !INTERVAL_FIELDS[..8].contains(&field.as_slice()) {
            return Std.write_property(req, object, name, value);
        }
        req.release(Value::string(name));
        let result = if field == b"f" {
            #[allow(clippy::cast_possible_truncation)]
            to_double(req, &value).map(|f| (f * 1_000_000.0) as i64)
        } else {
            to_long(req, &value)
        };
        req.release(value);
        let n = result?;
        if let Some(diff) = object
            .native_mut::<IntervalObj>(req)
            .and_then(|i| i.diff.as_mut())
        {
            #[allow(clippy::cast_possible_truncation)]
            match field.as_slice() {
                b"y" => diff.y = n,
                b"m" => diff.m = n,
                b"d" => diff.d = n,
                b"h" => diff.h = n,
                b"i" => diff.i = n,
                b"s" => diff.s = n,
                b"f" => diff.us = n,
                _ => diff.invert = n as i32,
            }
        }
        Ok(())
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> PropertyPlace {
        if INTERVAL_FIELDS.contains(&name) {
            return PropertyPlace::Handlers;
        }
        Std.get_property_ptr_ptr(req, object, name)
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let Some(value) = IntervalHandlers::field(req, object, name) else {
            return Std.has_property(req, object, name, test);
        };
        Ok(match test {
            PropertyTest::Exists => true,
            PropertyTest::NotEmpty => truthy(req, &value),
            PropertyTest::Isset => !matches!(value.as_ref(), ValueRef::Null),
        })
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let both = left
            .as_object()
            .is_some_and(|a| a.native::<IntervalObj>(req).is_some())
            && right
                .as_object()
                .is_some_and(|b| b.native::<IntervalObj>(req).is_some());
        if !both {
            return Std.compare(req, left, right);
        }
        raise(req, Level::Warning, "Cannot compare DateInterval objects");
        Ok(None)
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        if has_debug_info(req, object) {
            return Std.get_debug_info(req, object);
        }
        let own = std_rows(req, object)?;
        let fields = interval_rows(req, object)?;
        rows_array(req, &own, fields, false).map(Some)
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        if purpose == PropertyPurpose::Debug {
            return Ok(None);
        }
        let fields = interval_rows(req, object)?;
        rows_view(req, object, fields, |req, o| {
            o.native_mut::<IntervalObj>(req).map(|i| &mut i.rows)
        })
        .map(Some)
    }
}

/// The interval's fields, none for one no constructor set.
fn interval_rows(
    req: &mut Request,
    object: PhpObject,
) -> Result<Vec<(&'static [u8], Value)>, Thrown> {
    let state = object
        .native::<IntervalObj>(req)
        .and_then(|i| i.diff.map(|d| (d, i.date_string.clone())));
    match state {
        Some((diff, date_string)) => interval_fields(req, &diff, date_string.as_deref()),
        None => Ok(Vec::new()),
    }
}

/// `date_object_clone_interval`.
fn clone_interval(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, IntervalObj::default())?;
    let state = object.native::<IntervalObj>(req).map(|i| {
        (
            i.diff,
            i.civil_or_wall,
            i.from_string,
            i.date_string.clone(),
        )
    });
    if let (Some((diff, civil, from, text)), Some(payload)) =
        (state, copy.native_mut::<IntervalObj>(req))
    {
        payload.diff = diff;
        payload.civil_or_wall = civil;
        payload.from_string = from;
        payload.date_string = text;
    }
    Ok(copy)
}

/// The names `DatePeriod` answers itself, `date_period_is_internal_property`.
pub(super) const PERIOD_FIELDS: [&[u8]; 7] = [
    b"start",
    b"current",
    b"end",
    b"interval",
    b"recurrences",
    b"include_start_date",
    b"include_end_date",
];

/// `zend_readonly_property_modification_error_ex("DatePeriod", name)`,
/// what a write to one of the seven fields meets, and a read for a write
/// (`BP_VAR_W`, `BP_VAR_RW`), whatever the object's class.
fn readonly(req: &mut Request, name: &[u8]) -> Thrown {
    let field = String::from_utf8_lossy(name).into_owned();
    throw(
        req,
        "Error",
        &format!("Cannot modify readonly property DatePeriod::${field}"),
    )
}

/// `DatePeriod`'s table: its seven fields read from the period and
/// refused to writers, as php's virtual properties are.
///
/// ```php
/// $p = new DatePeriod(new DateTime('2000-01-01'), new DateInterval('P1D'), 1);
/// foreach (['recurrences', 'start', 'interval'] as $f) {
///     try { $p->$f[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
///     try { $r = &$p->$f; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Cannot modify readonly property DatePeriod::$recurrences
/// Cannot modify readonly property DatePeriod::$recurrences
/// Cannot modify readonly property DatePeriod::$start
/// Cannot modify readonly property DatePeriod::$start
/// Cannot modify readonly property DatePeriod::$interval
/// Cannot modify readonly property DatePeriod::$interval
/// ```
pub(crate) struct PeriodHandlers;

/// `date_object_handlers_period`.
pub(crate) static PERIOD: PeriodHandlers = PeriodHandlers;

impl ObjectHandlers for PeriodHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let rows = object
            .native_mut::<PeriodObj>(req)
            .map(|p| std::mem::take(&mut p.rows));
        release_rows(req, rows);
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_period)
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(p) = object.native::<PeriodObj>(req) {
            for value in &p.rows {
                visit(*value);
            }
        }
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        if !PERIOD_FIELDS.contains(&name) {
            return Std.read_property(req, object, name, read);
        }
        if read == PropertyRead::Write {
            return Err(readonly(req, name));
        }
        let fields = period_fields(req, object)?;
        let mut found = Value::null();
        for (field, value) in fields {
            if field == name {
                found = value;
            } else {
                req.release(value);
            }
        }
        Ok(found)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        if PERIOD_FIELDS.contains(&name.as_bytes()) {
            req.release(value);
            let thrown = readonly(req, name.as_bytes());
            req.release(Value::string(name));
            return Err(thrown);
        }
        Std.write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> PropertyPlace {
        if PERIOD_FIELDS.contains(&name) {
            return PropertyPlace::Handlers;
        }
        Std.get_property_ptr_ptr(req, object, name)
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        if !PERIOD_FIELDS.contains(&name) {
            return Std.has_property(req, object, name, test);
        }
        let initialized = object
            .native::<PeriodObj>(req)
            .is_some_and(|p| p.initialized);
        if test == PropertyTest::Exists {
            return Ok(true);
        }
        if !initialized {
            return Ok(false);
        }
        let value = self.read_property(req, object, name, PropertyRead::Silent)?;
        let answer = match test {
            PropertyTest::NotEmpty => truthy(req, &value),
            _ => !matches!(value.as_ref(), ValueRef::Null),
        };
        req.release(value);
        Ok(answer)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if PERIOD_FIELDS.contains(&name) {
            let class = class_name(req, object.class());
            let field = String::from_utf8_lossy(name).into_owned();
            return Err(throw(
                req,
                "Error",
                &format!("Cannot unset {class}::${field}"),
            ));
        }
        Std.unset_property(req, object, name)
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        if has_debug_info(req, object) {
            return Std.get_debug_info(req, object);
        }
        let own = std_rows(req, object)?;
        let initialized = object
            .native::<PeriodObj>(req)
            .is_some_and(|p| p.initialized);
        let fields = if initialized {
            period_fields(req, object)?
        } else {
            Vec::new()
        };
        rows_array(req, &own, fields, false).map(Some)
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<(Value, Value)>, Thrown> {
        crate::period::iterator_element(req, object, index)
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        if purpose == PropertyPurpose::Debug {
            return Ok(None);
        }
        let initialized = object
            .native::<PeriodObj>(req)
            .is_some_and(|p| p.initialized);
        let fields = if initialized {
            period_fields(req, object)?
        } else {
            Vec::new()
        };
        rows_view(req, object, fields, |req, o| {
            o.native_mut::<PeriodObj>(req).map(|p| &mut p.rows)
        })
        .map(Some)
    }
}

impl PeriodObj {
    /// The period's state, copied, without the rows its last property
    /// read made and with no walk standing on `current`.
    pub(super) fn copied(&self) -> PeriodObj {
        PeriodObj {
            start: self.start.clone(),
            start_ce: self.start_ce,
            current: self.current.clone(),
            end: self.end.clone(),
            interval: self.interval,
            recurrences: self.recurrences,
            include_start_date: self.include_start_date,
            include_end_date: self.include_end_date,
            initialized: self.initialized,
            cursor: None,
            rows: Vec::new(),
        }
    }

    /// `state` put in place of the period's, the rows its readers hold
    /// kept for `free_obj` to give back.
    pub(super) fn replace(&mut self, state: PeriodObj) {
        let rows = std::mem::take(&mut self.rows);
        *self = state;
        self.rows = rows;
    }
}

/// `date_object_clone_period`.
fn clone_period(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, PeriodObj::default())?;
    let state = object.native::<PeriodObj>(req).map(PeriodObj::copied);
    if let (Some(state), Some(payload)) = (state, copy.native_mut::<PeriodObj>(req)) {
        payload.replace(state);
    }
    Ok(copy)
}
