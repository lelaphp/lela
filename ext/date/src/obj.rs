//! The five classes' native payloads and their handler tables: php's
//! `php_date_obj`, `php_timezone_obj`, `php_interval_obj` and
//! `php_period_obj` as typed payloads on the object (design §5.4), and
//! the slots `date_register_classes` fills. What they share: the
//! uninitialised-object error, the class lookups, the objects made
//! around a payload, and the rows the property readers see.

use std::cmp::Ordering;
use std::sync::Arc;

use php_ext::abi::{
    Class, CloneObj, Instance, IteratorElement, ObjectHandlers, PropertiesFor, PropertyPlace,
    PropertyPurpose, PropertyRead, PropertyTest, Std,
};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{to_double, to_long, truthy};
use php_ext::raising::{Level, Thrown, raise, throw_error};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::ffi::{
    RelFields, RelTime, Time, UNSET, ZONETYPE_ABBR, ZONETYPE_ID, ZONETYPE_OFFSET, Zone,
};
use crate::format::date_format;
use crate::held::{Held, Shape};
use crate::interval::DateInterval;
use crate::lent::{Lent, lent, release};

/// `PHP_DATE_CIVIL` and `PHP_DATE_WALL`, the values of
/// `php_interval_obj`'s `int civil_or_wall`.
pub(super) const CIVIL: i32 = 1;
pub(super) const WALL: i32 = 2;

/// `php_date_obj`: the time, none until a constructor set it. timelib
/// made it on the request's heap ([`crate::lent`]), so its
/// `timelib_time` and its `tz_abbr` are counted where php counts them.
#[derive(Default)]
pub(crate) struct DateState {
    pub time: Option<Time>,
}

/// The payload: `php_date_obj` keeps its state behind pointers, and so the
/// payload is one, inside the `sizeof` php allocates (design §5.13).
pub(crate) type DateObj = Box<DateState>;

/// What a `DateTimeZone` holds, `php_timezone_obj`'s `type` and `tzi`:
/// a zone of the database, an offset from UTC in seconds, or an
/// abbreviation with its offset and whether it is daylight time. Two
/// words, so a `DateTimeZone`'s state lies inside the 32 bytes php's
/// `php_timezone_obj` holds past its `zend_object` and its `new` asks the
/// system allocator for nothing; the rare abbreviation is boxed.
#[derive(Clone)]
pub enum TzKind {
    Id(Arc<Zone>),
    Offset(i64),
    Abbr(Box<AbbrZone>),
}

/// `timelib_abbr_info`: an abbreviation zone's offset from UTC in
/// seconds, its name and whether it is daylight time.
#[derive(Clone)]
pub struct AbbrZone {
    pub utc_offset: i64,
    pub abbr: Vec<u8>,
    pub dst: i32,
}

impl TzKind {
    /// `TIMELIB_ZONETYPE_*`.
    pub(super) fn zone_type(&self) -> i64 {
        i64::from(match self {
            TzKind::Offset(_) => ZONETYPE_OFFSET,
            TzKind::Abbr(_) => ZONETYPE_ABBR,
            TzKind::Id(_) => ZONETYPE_ID,
        })
    }

    /// The zone given up. An identifier's `timelib_tzinfo` is dropped
    /// under a lend where this is its last owner, so what timelib
    /// allocated goes back to the request's heap (ADR-0014); while the
    /// request's cache keeps it, as it keeps every zone it parsed, and
    /// for an offset or an abbreviation, which hold none of timelib's
    /// memory, the drop frees nothing of timelib's and needs no lend.
    pub(super) fn release(self, req: &mut Request) -> Result<(), Thrown> {
        match self {
            TzKind::Id(zone) if Arc::strong_count(&zone) == 1 => release(req, zone),
            _ => Ok(()),
        }
    }

    /// `php_timezone_to_string`.
    pub(super) fn name(&self) -> Vec<u8> {
        match self {
            TzKind::Id(zone) => zone.name().to_vec(),
            TzKind::Offset(offset) => offset_string(*offset).into_bytes(),
            TzKind::Abbr(zone) => zone.abbr.clone(),
        }
    }
}

/// `php_timezone_obj`.
#[derive(Default)]
pub(crate) struct TzState {
    pub tz: Option<TzKind>,
    /// The `tzi.z.abbr` copy an abbreviation zone holds on the request's
    /// heap; kept by [`set_tz`].
    held: Option<Held>,
}

/// The payload, inside the `sizeof` php allocates (design §5.13): the
/// state itself, which fits it.
pub(crate) type TzObj = TzState;

const _: () = assert!(size_of::<TzState>() <= 32);

/// `php_interval_obj`: the relative time, none until a constructor set
/// it. timelib made it on the request's heap ([`crate::lent`]), where
/// php's `diff` points. Three words, so a `DateInterval`'s state lies
/// inside the 24 bytes php's `php_interval_obj` holds past its
/// `zend_object` and its `new` asks the system allocator for nothing.
#[derive(Default)]
pub(crate) struct IntervalState {
    pub diff: Option<RelTime>,
    pub civil_or_wall: i32,
    pub from_string: bool,
    /// The string a relative time was parsed from, the one the caller
    /// passed, shared as php's `zend_string_copy` shares it.
    pub date_string: Option<Owner<PhpString>>,
}

/// The payload, inside the `sizeof` php allocates (design §5.13): the
/// state itself, which fits it.
pub(crate) type IntervalObj = IntervalState;

const _: () = assert!(size_of::<IntervalState>() <= 24);

/// `php_period_obj`: its three times and its relative time are
/// timelib's, made on the request's heap ([`crate::lent`]) where php's
/// point.
#[derive(Default)]
pub(crate) struct PeriodState {
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
}

/// The payload: `php_period_obj` keeps its state behind pointers, and so the
/// payload is one, inside the `sizeof` php allocates (design §5.13).
pub(crate) type PeriodObj = Box<PeriodState>;

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
        None => Err(throw_error(
            req,
            "Error",
            &format!("Class \"{name}\" not found"),
        )),
    }
}

/// `ce->name`.
pub(super) fn class_name(req: &Request, class: ClassId) -> String {
    req.class_decl(class)
        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
        .unwrap_or_default()
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
        let base = req.lineage(class).find(|&at| is_internal(req, at));
        if let Some(at) = base {
            let base = class_name(req, at);
            format!("Object of type {name} (inheriting {base}) has not been {tail}")
        } else {
            format!("Object of type {name} not been {tail}")
        }
    };
    throw_error(req, "DateObjectError", &message)
}

/// The object's time, taken out of its payload for the length of `call`
/// and put back after: php's `dateobj->time`, read or changed where it
/// stands, with no copy made. `call` runs no PHP code, so nothing sees
/// the object without its time meanwhile. None where no constructor set
/// the time.
pub(super) fn with_time<R>(
    req: &mut Request,
    object: PhpObject,
    call: impl FnOnce(&mut Request, &mut Time) -> R,
) -> Option<R> {
    let mut time = object.native_mut::<DateObj>(req)?.time.take()?;
    let answered = call(req, &mut time);
    if let Some(payload) = object.native_mut::<DateObj>(req) {
        payload.time = Some(time);
    }
    Some(answered)
}

/// [`with_time`] with the request's heap lent for `call`, or the
/// uninitialised error: a body of `php_date.c` that works on
/// `dateobj->time` with timelib and nothing else.
pub(super) fn lent_time<R>(
    req: &mut Request,
    object: PhpObject,
    call: impl FnOnce(&mut Lent<'_>, &mut Time) -> R,
) -> Result<R, Thrown> {
    match with_time(req, object, |req, time| lent(req, |l| call(l, time))) {
        Some(answered) => answered,
        None => Err(uninitialized(req, object.class())),
    }
}

/// `DATE_CHECK_INITIALIZED(dateobj->time, ce)`: the uninitialised error
/// where no constructor set the object's time.
pub(super) fn check_time(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    if object
        .native::<DateObj>(req)
        .is_some_and(|d| d.time.is_some())
    {
        return Ok(());
    }
    Err(uninitialized(req, object.class()))
}

/// `timelib_time_clone(dateobj->time)`, or the uninitialised error.
pub(super) fn time_of(req: &mut Request, object: PhpObject) -> Result<Time, Thrown> {
    lent_time(req, object, |l, time| time.copy(l))
}

/// The object's time replaced, the one it had freed as
/// `timelib_time_dtor` frees it; none for none.
pub(super) fn set_time(
    req: &mut Request,
    object: PhpObject,
    time: Option<Time>,
) -> Result<(), Thrown> {
    let Some(payload) = object.native_mut::<DateObj>(req) else {
        return release(req, time);
    };
    let old = std::mem::replace(&mut payload.time, time);
    release(req, old)
}

/// `object_init_ex(zv, ce)` with its time set, one owner, the caller's.
pub(super) fn new_date(
    req: &mut Request,
    class: ClassId,
    time: Time,
) -> Result<Owner<PhpObject>, Thrown> {
    let object = match new_object(req, class) {
        Ok(object) => object,
        Err(thrown) => {
            release(req, time)?;
            return Err(thrown);
        }
    };
    set_time(req, *object, Some(time))?;
    Ok(object)
}

/// A new `DateTimeZone` holding `kind`, one owner, the caller's.
pub(super) fn new_timezone(req: &mut Request, kind: TzKind) -> Result<Owner<PhpObject>, Thrown> {
    let class = class_id(req, "DateTimeZone")?;
    let object = new_object(req, class)?;
    set_tz(req, *object, Some(kind))?;
    Ok(object)
}

/// The object's zone replaced, `set_timezone_from_timelib_time`: an
/// abbreviation zone's `tzi.z.abbr` freed and copied again with
/// `timelib_strdup`, the one block a `DateTimeZone` holds on the
/// request's heap; an identifier's `tzinfo` is the request's cache's
/// and an offset holds none. The `Err` is the memory limit.
pub(super) fn set_tz(
    req: &mut Request,
    object: PhpObject,
    kind: Option<TzKind>,
) -> Result<(), Thrown> {
    let shape = match &kind {
        Some(TzKind::Abbr(zone)) => Some(Shape::abbr(&zone.abbr)),
        _ => None,
    };
    let Some(payload) = object.native_mut::<TzObj>(req) else {
        return kind.map_or(Ok(()), |kind| kind.release(req));
    };
    let old = std::mem::replace(&mut payload.tz, kind);
    let held = payload.held.take();
    if let Some(old) = old {
        old.release(req)?;
    }
    let held = Held::hold(req, held, shape)?;
    if let Some(payload) = object.native_mut::<TzObj>(req) {
        payload.held = held;
    }
    Ok(())
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
        ZONETYPE_ABBR => Some(TzKind::Abbr(Box::new(AbbrZone {
            utc_offset: i64::from(t.z),
            abbr: t.tz_abbr().to_vec(),
            dst: t.dst,
        }))),
        _ => None,
    }
}

/// `php_date_instantiate(date_ce_interval, return_value)` and then its
/// `diff` set to what `diff` makes: the object first, as php makes it
/// before the relative time.
pub(super) fn new_interval(
    req: &mut Request,
    civil_or_wall: i32,
    date_string: Option<PhpString>,
    diff: impl FnOnce(&mut Request) -> Result<RelTime, Thrown>,
) -> Result<Owner<Instance<DateInterval>>, Thrown> {
    let class = class_id(req, "DateInterval")?;
    let object = new_object(req, class)?;
    let diff = match diff(req) {
        Ok(diff) => diff,
        Err(thrown) => {
            object.give_up(req);
            return Err(thrown);
        }
    };
    let date_string = date_string.map(|s| Owner::take(req, s));
    if let Some(payload) = object.native_mut::<IntervalObj>(req) {
        payload.civil_or_wall = civil_or_wall;
        payload.from_string = date_string.is_some();
        payload.date_string = date_string;
    }
    set_diff(req, *object, Some(diff))?;
    Ok(object.into_instance())
}

/// `create_date_period_interval` and `DatePeriod::getDateInterval`: a
/// new `DateInterval` over a `timelib_rel_time_clone` of a period's
/// interval, the object made first; its `civil_or_wall` is the zero
/// `ecalloc` leaves, neither `CIVIL` nor `WALL`, as php never sets it.
pub(super) fn period_interval(
    req: &mut Request,
    diff: &RelFields,
) -> Result<Owner<Instance<DateInterval>>, Thrown> {
    new_interval(req, 0, None, |req| lent(req, |l| RelTime::of(l, diff)))
}

/// The interval's relative time replaced, the one it had freed as
/// `timelib_rel_time_dtor` frees it; none for none.
pub(super) fn set_diff(
    req: &mut Request,
    object: PhpObject,
    diff: Option<RelTime>,
) -> Result<(), Thrown> {
    let Some(payload) = object.native_mut::<IntervalObj>(req) else {
        return release(req, diff);
    };
    match std::mem::replace(&mut payload.diff, diff) {
        Some(old) => release(req, old),
        None => Ok(()),
    }
}

/// The relative time a `DateInterval` holds, or the uninitialised error.
pub(super) fn interval_of(
    req: &mut Request,
    object: PhpObject,
) -> Result<(RelFields, i32), Thrown> {
    let diff = object
        .native::<IntervalObj>(req)
        .and_then(|i| i.diff.as_deref().map(|d| (*d, i.civil_or_wall)));
    match diff {
        Some(diff) => Ok(diff),
        None => Err(uninitialized(req, object.class())),
    }
}

/// `RETURN_OBJ_COPY`: the object answered as a `C`, a view the engine
/// takes its own owner of as it takes any returned view's.
pub(super) fn again<C: Class>(object: PhpObject) -> Instance<C> {
    Instance::of(object)
}

/// `add_assoc_*`: one row under a string key.
pub(super) fn set_str(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    key: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, key, value)
}

/// `add_assoc_string`.
pub(super) fn set_string(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    key: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let value = PhpString::new(req, value)?.into_value();
    set_str(req, array, key, value)
}

/// `zend_std_get_properties(object)`, each declared name mangled as a
/// reader keys it, an integer key as it stands: views, no owner taken.
pub(super) fn std_rows(req: &mut Request, object: PhpObject) -> Result<Vec<(Key, Value)>, Thrown> {
    let table = Std.get_properties(req, object)?;
    Ok(walk::rows(req, table).collect())
}

/// The fields a `*_object_to_hash` answers, each value one owner, which
/// the table or the row it is handed to takes.
pub(super) type Fields = Vec<(&'static [u8], Owner<Value>)>;

/// Fields handed nowhere, given up: the way out of a path that fails
/// while it holds them.
pub(super) fn give_up_fields(req: &mut Request, fields: Fields) {
    for (_, value) in fields {
        value.give_up(req);
    }
}

/// `made` pushed under `name`, or, where it failed, every field made
/// before it given up and the failure answered.
fn push_field(
    req: &mut Request,
    fields: &mut Fields,
    name: &'static [u8],
    made: Result<Owner<Value>, Thrown>,
) -> Result<(), Thrown> {
    match made {
        Ok(value) => {
            fields.push((name, value));
            Ok(())
        }
        Err(thrown) => {
            give_up_fields(req, std::mem::take(fields));
            Err(thrown)
        }
    }
}

/// The rows as an array with its own owners, each key and value one
/// more owner of what `own` views, the table `__serialize` returns and
/// `get_debug_info` hands a dumper: the fields first where
/// `first`, and the object's own rows added behind them only where no
/// field took their name, as `add_common_properties` adds them.
pub(super) fn rows_array(
    req: &mut Request,
    own: &[(Key, Value)],
    fields: Fields,
    first: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = match PhpArray::new(req) {
        Ok(array) => array,
        Err(thrown) => {
            give_up_fields(req, fields);
            return Err(thrown);
        }
    };
    if !first {
        for (key, value) in own {
            let (key, value) = (Owner::take(req, key.clone()), Owner::take(req, *value));
            if let Err(thrown) = array.set(req, key, value) {
                give_up_fields(req, fields);
                array.give_up(req);
                return Err(thrown);
            }
        }
    }
    let mut fields = fields.into_iter();
    while let Some((name, value)) = fields.next() {
        if let Err(thrown) = array.set_str(req, name, value) {
            give_up_fields(req, fields.collect());
            array.give_up(req);
            return Err(thrown);
        }
    }
    if first {
        for (key, value) in own {
            if !array.contains(key) {
                let (key, value) = (Owner::take(req, key.clone()), Owner::take(req, *value));
                array.set(req, key, value)?;
            }
        }
    }
    Ok(array)
}

/// `date_object_get_properties_for`'s table, a temporary one:
/// `zend_array_dup(zend_std_get_properties(object))`, the object's own
/// rows each one more owner, with `fields` updated into it where a row
/// of its name stands and appended where none does
/// (`zend_hash_str_update`).
fn fields_table(
    req: &mut Request,
    object: PhpObject,
    fields: impl FnOnce(&mut Request) -> Result<Fields, Thrown>,
) -> Result<PropertiesFor, Thrown> {
    let own = std_rows(req, object)?;
    let fields = fields(req)?;
    rows_array(req, &own, fields, false).map(PropertiesFor::Temporary)
}

/// Whether the class declares `__debugInfo`, php's `ce->__debugInfo`.
fn has_debug_info(req: &mut Request, object: PhpObject) -> bool {
    matches!(Std.get_method(req, object, b"__debuginfo"), Ok(Some(_)))
}

/// `date_object_to_hash`: `date`, and where the time is local its
/// `timezone_type` and `timezone`.
pub(super) fn date_fields(req: &mut Request, t: &Time) -> Result<Fields, Thrown> {
    let text = lent(req, |l| date_format(l, b"x-m-d H:i:s.u", t, true))?;
    let mut fields = vec![(&b"date"[..], PhpString::new(req, &text)?.into_value())];
    if t.is_localtime != 0 {
        let zone_type = Owner::int(i64::from(t.zone_type));
        fields.push((b"timezone_type", zone_type));
        let zone: Vec<u8> = match t.zone_type {
            ZONETYPE_ID => t.zone().map(|z| z.name().to_vec()).unwrap_or_default(),
            ZONETYPE_OFFSET => offset_string(i64::from(t.z)).into_bytes(),
            ZONETYPE_ABBR => t.tz_abbr().to_vec(),
            _ => Vec::new(),
        };
        let zone = PhpString::new(req, &zone).map(Owner::into_value);
        push_field(req, &mut fields, b"timezone", zone)?;
    }
    Ok(fields)
}

/// `date_timezone_object_to_hash`.
pub(super) fn tz_fields(req: &mut Request, kind: &TzKind) -> Result<Fields, Thrown> {
    let name = PhpString::new(req, &kind.name())?.into_value();
    let zone_type = Owner::int(kind.zone_type());
    Ok(vec![
        (&b"timezone_type"[..], zone_type),
        (b"timezone", name),
    ])
}

/// `date_interval_object_to_hash`.
pub(super) fn interval_fields(
    req: &mut Request,
    diff: &RelFields,
    date_string: Option<PhpString>,
) -> Fields {
    if let Some(text) = date_string {
        let text = Owner::take(req, text).into_value();
        let from_string = Owner::bool(true);
        return vec![(&b"from_string"[..], from_string), (b"date_string", text)];
    }
    #[allow(clippy::cast_precision_loss)]
    let f = diff.us as f64 / 1_000_000.0;
    let scalars = [
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
    ];
    scalars
        .into_iter()
        .map(|(name, value)| (name, Owner::take(req, value)))
        .collect()
}

/// One of a period's three times, `start`, `current` or `end`.
type PeriodTime = fn(&PeriodState) -> &Option<Time>;

/// `create_date_period_datetime`: a date of the period's class over a
/// `timelib_time_clone` of the time `which` reads, the object made
/// first, or `null` where the period has none.
fn period_date(
    req: &mut Request,
    object: PhpObject,
    which: PeriodTime,
) -> Result<Owner<Value>, Thrown> {
    let class = object
        .native::<PeriodObj>(req)
        .and_then(|p| p.start_ce.filter(|_| which(p).is_some()));
    let Some(class) = class else {
        return Ok(Owner::null());
    };
    let date = new_object(req, class)?;
    let copy = with_period(req, object, |req, p| {
        lent(req, |l| which(p).as_ref().map(|t| t.copy(l)))
    });
    if let Err(thrown) = copy
        .transpose()
        .and_then(|copy| set_time(req, *date, copy.flatten()))
    {
        date.give_up(req);
        return Err(thrown);
    }
    Ok(date.into_value())
}

/// The period's state, taken out of its payload for the length of
/// `call` and put back after, as php reads `period_obj` where it stands;
/// `call` runs no PHP code. None where the object has no payload.
pub(super) fn with_period<R>(
    req: &mut Request,
    object: PhpObject,
    call: impl FnOnce(&mut Request, &mut PeriodState) -> R,
) -> Option<R> {
    let mut state = std::mem::take(object.native_mut::<PeriodObj>(req)?);
    let answered = call(req, &mut state);
    if let Some(payload) = object.native_mut::<PeriodObj>(req) {
        *payload = state;
    }
    Some(answered)
}

/// The value of one of the seven names php's `date_period_read_property`
/// answers, made as `date_period_object_to_hash` makes its row: a date
/// over a clone of a time, an interval over a clone of the relative
/// time, the object made first and `null` for none, or a scalar.
pub(super) fn period_field(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let Some(p) = object.native::<PeriodObj>(req) else {
        return Ok(Owner::null());
    };
    let (interval, recurrences, include_start, include_end) = (
        p.interval.as_deref().copied(),
        p.recurrences,
        p.include_start_date,
        p.include_end_date,
    );
    let scalar = match name {
        b"start" => return period_date(req, object, |p| &p.start),
        b"current" => return period_date(req, object, |p| &p.current),
        b"end" => return period_date(req, object, |p| &p.end),
        b"interval" => {
            let Some(diff) = interval else {
                return Ok(Owner::null());
            };
            return period_interval(req, &diff).map(Owner::into_value);
        }
        b"recurrences" => Value::int(recurrences),
        b"include_start_date" => Value::bool(include_start),
        b"include_end_date" => Value::bool(include_end),
        _ => Value::null(),
    };
    Ok(Owner::take(req, scalar))
}

/// `date_period_object_to_hash`: the seven rows, each made in its turn.
pub(super) fn period_fields(req: &mut Request, object: PhpObject) -> Result<Fields, Thrown> {
    let mut fields = Vec::with_capacity(PERIOD_FIELDS.len());
    for name in PERIOD_FIELDS {
        let made = period_field(req, object, name);
        push_field(req, &mut fields, name, made)?;
    }
    Ok(fields)
}

/// The rows `DateTime`, `DateTimeImmutable` and their subclasses show
/// every reader of their properties but `get_object_vars`: the object's
/// own and the three of `date_object_to_hash`.
pub(crate) struct DateHandlers;

/// `date_object_handlers_date`, `DateTime`'s and `DateTimeImmutable`'s.
pub(crate) static DATE: DateHandlers = DateHandlers;

impl ObjectHandlers for DateHandlers {
    /// `date_object_free_storage_date`: `timelib_time_dtor`.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let time = object
            .native_mut::<DateObj>(req)
            .and_then(|d| d.time.take());
        // Freeing allocates nothing, so the limit refuses nothing.
        let _freed = release(req, time);
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
        if da.time.is_none() || db.time.is_none() {
            return Err(throw_error(
                req,
                "DateObjectError",
                "Trying to compare an incomplete DateTime or DateTimeImmutable object",
            ));
        }
        // `date_object_compare_date`: each time brought up to date where
        // it stands, then the two compared.
        for object in [a, b] {
            lent_time(req, object, |l, t| {
                if t.sse_uptodate == 0 {
                    let zone = t.zone().cloned();
                    t.update_ts(l, zone.as_ref());
                }
            })?;
        }
        let compared = with_time(req, a, |req, ta| {
            if a == b {
                return 0;
            }
            with_time(req, b, |_, tb| ta.compare(tb)).unwrap_or(0)
        });
        Ok(Some(compared.unwrap_or(0).cmp(&0)))
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        if has_debug_info(req, object) {
            return Std.get_debug_info(req, object);
        }
        let own = std_rows(req, object)?;
        let fields = with_time(req, object, |req, t| date_fields(req, t))
            .transpose()?
            .unwrap_or_default();
        rows_array(req, &own, fields, false).map(|t| Some(t.into()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        // `date_object_get_properties_for`'s `default:`, the standard
        // table, for the debug reader and for `get_object_vars()`.
        if matches!(
            purpose,
            PropertyPurpose::Debug | PropertyPurpose::GetObjectVars
        ) {
            return Std.get_properties_for(req, object, purpose);
        }
        fields_table(req, object, |req| {
            with_time(req, object, |req, t| date_fields(req, t))
                .transpose()
                .map(Option::unwrap_or_default)
        })
    }
}

/// `date_object_clone_date`: a fresh object of the class, its members
/// copied and `__clone` served, then the time copied into it.
pub(super) fn clone_date(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, DateObj::default())?;
    let time = with_time(req, object, |req, t| lent(req, |l| t.copy(l))).transpose();
    let set = time.and_then(|time| match time {
        Some(time) => set_time(req, *copy, Some(time)),
        None => Ok(()),
    });
    if let Err(thrown) = set {
        copy.give_up(req);
        return Err(thrown);
    }
    Ok(copy)
}

/// `DateTimeZone`'s table: its rows, its clone and its comparison.
pub(crate) struct TzHandlers;

/// `date_object_handlers_timezone`.
pub(crate) static TIMEZONE: TzHandlers = TzHandlers;

impl ObjectHandlers for TzHandlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let taken = object
            .native_mut::<TzObj>(req)
            .map(|t| (t.tz.take(), t.held.take()));
        let Some((kind, held)) = taken else {
            return;
        };
        if let Some(held) = held {
            held.free(req);
        }
        if let Some(kind) = kind {
            // Freeing allocates nothing, so the limit refuses nothing.
            let _freed = kind.release(req);
        }
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
            return Err(throw_error(
                req,
                "DateObjectError",
                "Trying to compare uninitialized DateTimeZone objects",
            ));
        };
        if ka.zone_type() != kb.zone_type() {
            return Err(throw_error(
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
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        if has_debug_info(req, object) {
            return Std.get_debug_info(req, object);
        }
        let own = std_rows(req, object)?;
        let kind = object.native::<TzObj>(req).and_then(|t| t.tz.clone());
        let fields = match &kind {
            Some(kind) => tz_fields(req, kind)?,
            None => Vec::new(),
        };
        rows_array(req, &own, fields, false).map(|t| Some(t.into()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        // `date_object_get_properties_for`'s `default:`, the standard
        // table, for the debug reader and for `get_object_vars()`.
        if matches!(
            purpose,
            PropertyPurpose::Debug | PropertyPurpose::GetObjectVars
        ) {
            return Std.get_properties_for(req, object, purpose);
        }
        let kind = object.native::<TzObj>(req).and_then(|t| t.tz.clone());
        fields_table(req, object, |req| match &kind {
            Some(kind) => tz_fields(req, kind),
            None => Ok(Vec::new()),
        })
    }
}

/// `date_object_clone_timezone`.
fn clone_timezone(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, TzObj::default())?;
    let kind = object.native::<TzObj>(req).and_then(|t| t.tz.clone());
    if let Err(thrown) = set_tz(req, *copy, kind) {
        copy.give_up(req);
        return Err(thrown);
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
        let diff = object.native::<IntervalObj>(req)?.diff.as_deref()?;
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
    /// `date_object_free_storage_interval`: the `date_string` released,
    /// then `timelib_rel_time_dtor`.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some((text, diff)) = object
            .native_mut::<IntervalObj>(req)
            .map(|i| (i.date_string.take(), i.diff.take()))
        else {
            return;
        };
        if let Some(text) = text {
            text.give_up(req);
        }
        // Freeing allocates nothing, so the limit refuses nothing.
        let _freed = release(req, diff);
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
    ) -> Result<Owner<Value>, Thrown> {
        match IntervalHandlers::field(req, object, name) {
            Some(value) => Ok(Owner::take(req, value)),
            None => Std.read_property(req, object, name, read),
        }
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let initialized = object
            .native::<IntervalObj>(req)
            .is_some_and(|i| i.diff.is_some());
        let field = name.as_bytes().to_vec();
        if !initialized || !INTERVAL_FIELDS[..8].contains(&field.as_slice()) {
            return Std.write_property(req, object, name, value);
        }
        name.give_up(req);
        let result = if field == b"f" {
            #[allow(clippy::cast_possible_truncation)]
            to_double(req, &value).map(|f| (f * 1_000_000.0) as i64)
        } else {
            to_long(req, &value)
        };
        value.give_up(req);
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
    ) -> Result<PropertyPlace, Thrown> {
        if INTERVAL_FIELDS.contains(&name) {
            return Ok(PropertyPlace::Handlers);
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
            PropertyTest::NotEmpty => truthy(req, &value)?,
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
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        if has_debug_info(req, object) {
            return Std.get_debug_info(req, object);
        }
        write_fields(req, object)?;
        let rows = std_rows(req, object)?;
        rows_array(req, &rows, Vec::new(), false).map(|t| Some(t.into()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    /// `date_object_get_properties_interval`, the class's
    /// `get_properties`: the interval's fields written into the object's
    /// own table and the table answered, which `ReflectionProperty`'s
    /// dynamic arm reads and the pointer natives move the pointer of.
    ///
    /// ```php
    /// $i = new DateInterval('P1Y2D');
    /// $p = new ReflectionProperty($i, 'y');
    /// var_dump($p->getValue($i), $p->isLazy($i), $p->isDefault());
    /// var_dump((new ReflectionObject($i))->hasProperty('days'));
    /// ```
    /// ```expect
    /// int(1)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// ```
    ///
    /// The fields live in the object's own table, so the pointer natives
    /// keep their pointer on it between calls, a field written through
    /// the interval is updated where its row stands, and a dynamic
    /// property added after the fields were written follows them.
    ///
    /// ```php
    /// $i = new DateInterval('P1Y2D');
    /// var_dump(@current($i), @next($i), @key($i));
    /// $i->m = 7;
    /// var_dump(@current($i), @key($i), @next($i), @end($i), @key($i), @prev($i));
    /// $j = new DateInterval('P1D'); @$j->foo = 1; var_dump(count((array) $j)); @$j->bar = 2;
    /// echo implode(' ', array_keys((array) $j)), "\n";
    /// ```
    /// ```expect
    /// int(1)
    /// int(0)
    /// string(1) "m"
    /// int(7)
    /// string(1) "m"
    /// int(2)
    /// bool(false)
    /// string(11) "from_string"
    /// bool(false)
    /// int(11)
    /// foo y m d h i s f invert days from_string bar
    /// ```
    ///
    /// php sets no `get_properties_for` on the class, so every reader
    /// reads this table as `zend_std_get_properties_for` hands it on, the
    /// object's own, where a declared slot is `IS_INDIRECT`: over a
    /// subclass, `get_object_vars()` refuses a protected or private
    /// declared row from outside and names it unmangled from inside, and
    /// `foreach` sees what `get_object_vars()` sees.
    ///
    /// ```php
    /// class I extends DateInterval { public $y = 'mine'; protected $p = 1; private $q = 2;
    ///     function inside() { return array_keys(get_object_vars($this)); } }
    /// $i = new I('P2Y');
    /// var_dump(get_object_vars($i));
    /// echo implode(' ', $i->inside()), "\n";
    /// $keys = []; foreach ($i as $k => $v) { $keys[] = $k; } echo implode(' ', $keys), "\n";
    /// ```
    /// ```expect
    /// array(10) {
    ///   ["y"]=>
    ///   int(2)
    ///   ["m"]=>
    ///   int(0)
    ///   ["d"]=>
    ///   int(0)
    ///   ["h"]=>
    ///   int(0)
    ///   ["i"]=>
    ///   int(0)
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
    /// y p q m d h i s f invert days from_string
    /// y m d h i s f invert days from_string
    /// ```
    fn get_properties(&self, req: &mut Request, object: PhpObject) -> Result<walk::Table, Thrown> {
        write_fields(req, object)?;
        Std.get_properties(req, object)
    }
}

/// `date_interval_object_to_hash`, as `date_object_get_properties_interval`
/// calls it: each field written into the object's own table with
/// `zend_hash_str_update`, a row it holds replaced where it stands and a
/// new one appended, so the table's internal pointer stays where it
/// stood.
pub(super) fn write_fields(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let mut fields = interval_rows(req, object).into_iter();
    while let Some((name, value)) = fields.next() {
        let written = match PhpString::new(req, name) {
            Ok(name) => {
                let written = object.set_row(req, &Key::Str(*name), value);
                name.give_up(req);
                written
            }
            Err(thrown) => {
                value.give_up(req);
                Err(thrown)
            }
        };
        if let Err(thrown) = written {
            give_up_fields(req, fields.collect());
            return Err(thrown);
        }
    }
    Ok(())
}

/// The interval's fields, none for one no constructor set.
fn interval_rows(req: &mut Request, object: PhpObject) -> Fields {
    let state = object.native::<IntervalObj>(req).and_then(|i| {
        i.diff
            .as_deref()
            .map(|d| (*d, i.date_string.as_deref().copied()))
    });
    match state {
        Some((diff, date_string)) => interval_fields(req, &diff, date_string),
        None => Vec::new(),
    }
}

/// `date_object_clone_interval`: a fresh object of the class, its
/// members copied and `__clone` served, then the relative time copied
/// into it by `timelib_rel_time_clone`.
fn clone_interval(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, IntervalObj::default())?;
    let state = object.native::<IntervalObj>(req).map(|i| {
        (
            i.civil_or_wall,
            i.from_string,
            i.date_string.as_deref().copied(),
        )
    });
    let state = state.map(|(civil, from, text)| (civil, from, text.map(|s| Owner::take(req, s))));
    if let (Some((civil, from, text)), Some(payload)) = (state, copy.native_mut::<IntervalObj>(req))
    {
        payload.civil_or_wall = civil;
        payload.from_string = from;
        payload.date_string = text;
    }
    let diff = object
        .native::<IntervalObj>(req)
        .and_then(|i| i.diff.as_deref().copied());
    let set = match diff {
        Some(diff) => {
            lent(req, |l| RelTime::of(l, &diff)).and_then(|d| set_diff(req, *copy, Some(d)))
        }
        None => Ok(()),
    };
    if let Err(thrown) = set {
        copy.give_up(req);
        return Err(thrown);
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
    throw_error(
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
    /// `date_period_it`: the iterator, `current`, the period and `current_index`.
    fn iterator_size(&self) -> usize {
        120
    }

    /// `date_object_free_storage_period`: the three times, then the
    /// relative time.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let taken = object.native_mut::<PeriodObj>(req).map(|p| {
            let times = [p.start.take(), p.current.take(), p.end.take()];
            (times, p.interval.take())
        });
        // Freeing allocates nothing, so the limit refuses nothing.
        let _freed = release(req, taken);
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_period)
    }

    /// `date_period_read_property`: the one field read, made alone, so a
    /// read of `interval` makes one interval and nothing beside it.
    ///
    /// ```php
    /// $p = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 2);
    /// $m = memory_get_usage(); $x = $p->interval->d; echo memory_get_usage() - $m, ' ', spl_object_id(new stdClass), "\n";
    /// $m = memory_get_usage(); $x = $p->start; echo memory_get_usage() - $m, ' ', spl_object_id(new stdClass), "\n";
    /// ```
    /// ```expect
    /// 0 3
    /// 320 2
    /// ```
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        if !PERIOD_FIELDS.contains(&name) {
            return Std.read_property(req, object, name, read);
        }
        if read == PropertyRead::Write {
            return Err(readonly(req, name));
        }
        period_field(req, object, name)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if PERIOD_FIELDS.contains(&name.as_bytes()) {
            value.give_up(req);
            let thrown = readonly(req, name.as_bytes());
            name.give_up(req);
            return Err(thrown);
        }
        Std.write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        if PERIOD_FIELDS.contains(&name) {
            return Ok(PropertyPlace::Handlers);
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
            _ => Ok(!matches!(value.as_ref(), ValueRef::Null)),
        };
        value.give_up(req);
        answer
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
            return Err(throw_error(
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
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
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
        rows_array(req, &own, fields, false).map(|t| Some(t.into()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<IteratorElement>, Thrown> {
        crate::period::iterator_element(req, object, index)
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        if purpose == PropertyPurpose::Debug {
            return Std.get_properties_for(req, object, purpose);
        }
        let initialized = object
            .native::<PeriodObj>(req)
            .is_some_and(|p| p.initialized);
        fields_table(req, object, |req| {
            if initialized {
                period_fields(req, object)
            } else {
                Ok(Vec::new())
            }
        })
    }
}

impl PeriodState {
    /// The period's state, copied, without the rows its last property
    /// read made and with no walk standing on `current`: each time by
    /// `timelib_time_clone` and then the interval by
    /// `timelib_rel_time_clone`, onto the request's heap.
    pub(super) fn copied(&self, lent: &Lent<'_>) -> PeriodState {
        let copy = |t: &Option<Time>| t.as_ref().map(|t| t.copy(lent));
        PeriodState {
            start: copy(&self.start),
            start_ce: self.start_ce,
            current: copy(&self.current),
            end: copy(&self.end),
            interval: self.interval.as_deref().map(|r| RelTime::of(lent, r)),
            recurrences: self.recurrences,
            include_start_date: self.include_start_date,
            include_end_date: self.include_end_date,
            initialized: self.initialized,
            cursor: None,
        }
    }
}

/// `date_object_clone_period`.
fn clone_period(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    // `date_object_new_*` then `zend_objects_clone_members`: the copy's
    // payload is fresh while `__clone` runs, as php's is, and is filled
    // after it.
    let copy = object.clone_with(req, PeriodObj::default())?;
    let state = with_period(req, object, |req, p| lent(req, |l| Box::new(p.copied(l))))
        .transpose()
        .and_then(|state| {
            let old = state.and_then(|state| {
                copy.native_mut::<PeriodObj>(req)
                    .map(|payload| std::mem::replace(payload, state))
            });
            release(req, old)
        });
    if let Err(thrown) = state {
        copy.give_up(req);
        return Err(thrown);
    }
    Ok(copy)
}
