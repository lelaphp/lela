//! The classes of `php_date.stub.php`, declared as `php_date_arginfo.h`
//! registers them: their constants, properties and methods in the
//! stub's order, each method's return tentative where the stub says
//! `@tentative-return-type`, and the handler table and payload
//! `date_register_classes` gives each.

use php_ext::abi::ExtensionBuilder;
use php_ext::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstExpr,
    EnumBacking, MethodDecl, Name, Parameter, PropertyDecl, Type, Visibility,
};

use crate::args::{class, d, method, nullable, p, str_default, union, untyped};
use crate::obj::{
    DateHandlers, DateObj, IntervalHandlers, IntervalObj, PeriodHandlers, PeriodObj, TzHandlers,
    TzObj,
};
use crate::period::{EXCLUDE_START_DATE, INCLUDE_END_DATE};
use crate::tzdb::GROUPS;
use crate::{
    Entry, FORMATS, datetime, deprecated, interval, period, rfc7231_deprecation, timezone,
};

static DATE: DateHandlers = DateHandlers;
static TIMEZONE: TzHandlers = TzHandlers;
static INTERVAL: IntervalHandlers = IntervalHandlers;
static PERIOD: PeriodHandlers = PeriodHandlers;

/// One class with nothing declared on it yet.
fn class_decl(name: &str, kind: ClassKind, parent: Option<&str>, interfaces: &[&str]) -> ClassDecl {
    ClassDecl {
        name: Name(name.as_bytes().into()),
        kind,
        flags: ClassFlags::default(),
        parent: parent.map(|p| Name(p.as_bytes().into())),
        interfaces: interfaces
            .iter()
            .map(|i| Name(i.as_bytes().into()))
            .collect(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// A typed public class constant.
fn constant(name: &str, ty: Type, value: ConstExpr) -> ClassConstDecl {
    ClassConstDecl {
        name: Name(name.as_bytes().into()),
        visibility: Visibility::Public,
        is_final: false,
        ty: Some(ty),
        value,
        attributes: Vec::new(),
        doc_comment: None,
    }
}

/// An instance method, its return tentative.
fn m(name: &str, params: Vec<Parameter>, returns: Type) -> MethodDecl {
    method(name, params, Some(returns), true, true)
}

/// An instance method whose return is real.
fn real(name: &str, params: Vec<Parameter>, returns: Type) -> MethodDecl {
    method(name, params, Some(returns), true, false)
}

/// A method of the class (`ZEND_ACC_STATIC`), its return tentative.
fn st(name: &str, params: Vec<Parameter>, returns: Type) -> MethodDecl {
    method(name, params, Some(returns), false, true)
}

/// A method of the class (`ZEND_ACC_STATIC`) whose return is real.
fn st_real(name: &str, params: Vec<Parameter>, returns: Type) -> MethodDecl {
    method(name, params, Some(returns), false, false)
}

/// A constructor, which declares no return.
fn ctor(params: Vec<Parameter>) -> MethodDecl {
    method("__construct", params, None, true, false)
}

/// `__wakeup(): void`, deprecated since 8.5.
fn wakeup() -> MethodDecl {
    let mut decl = m("__wakeup", vec![], Type::Void);
    decl.function.attributes = deprecated(
        "8.5",
        "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()",
    );
    decl
}

/// `#[\NoDiscard(message: ...)]` of a `DateTimeImmutable` method.
fn no_discard(mut decl: MethodDecl) -> MethodDecl {
    let name = String::from_utf8_lossy(&decl.function.name.0).into_owned();
    decl.function.attributes = vec![Attribute {
        name: Name(b"NoDiscard".as_slice().into()),
        arguments: vec![AttributeArgument {
            name: Some(Name(b"message".as_slice().into())),
            value: ConstExpr::Str(
                format!("as DateTimeImmutable::{name}() does not modify the object itself")
                    .into_bytes()
                    .into(),
            ),
        }],
    }];
    decl
}

fn serialize() -> MethodDecl {
    real("__serialize", vec![], Type::Array)
}

fn unserialize() -> MethodDecl {
    real("__unserialize", vec![p("data", Type::Array)], Type::Void)
}

fn set_state(returns: &str) -> MethodDecl {
    st("__set_state", vec![p("array", Type::Array)], class(returns))
}

fn target() -> Vec<Parameter> {
    vec![
        p("targetObject", class("DateTimeInterface")),
        d("absolute", Type::Bool, ConstExpr::Bool(false)),
    ]
}

fn date_ctor() -> MethodDecl {
    ctor(vec![
        d("datetime", Type::String, str_default("now")),
        d("timezone", nullable(class("DateTimeZone")), ConstExpr::Null),
    ])
}

fn from_format(returns: &str) -> MethodDecl {
    st(
        "createFromFormat",
        vec![
            p("format", Type::String),
            p("datetime", Type::String),
            d("timezone", nullable(class("DateTimeZone")), ConstExpr::Null),
        ],
        union(vec![class(returns), Type::False]),
    )
}

fn from_timestamp() -> MethodDecl {
    st(
        "createFromTimestamp",
        vec![p("timestamp", union(vec![Type::Int, Type::Float]))],
        Type::CalledClass,
    )
}

fn last_errors() -> MethodDecl {
    st(
        "getLastErrors",
        vec![],
        union(vec![Type::Array, Type::False]),
    )
}

fn set_time(returns: &str) -> MethodDecl {
    m(
        "setTime",
        vec![
            p("hour", Type::Int),
            p("minute", Type::Int),
            d("second", Type::Int, ConstExpr::Long(0)),
            d("microsecond", Type::Int, ConstExpr::Long(0)),
        ],
        class(returns),
    )
}

fn set_date(returns: &str) -> MethodDecl {
    m(
        "setDate",
        vec![
            p("year", Type::Int),
            p("month", Type::Int),
            p("day", Type::Int),
        ],
        class(returns),
    )
}

fn set_isodate(returns: &str) -> MethodDecl {
    m(
        "setISODate",
        vec![
            p("year", Type::Int),
            p("week", Type::Int),
            d("dayOfWeek", Type::Int, ConstExpr::Long(1)),
        ],
        class(returns),
    )
}

fn tz_or_false() -> Type {
    union(vec![class("DateTimeZone"), Type::False])
}

/// `interface DateTimeInterface`: its fourteen formats and nine methods.
fn interface() -> ClassDecl {
    let mut decl = class_decl("DateTimeInterface", ClassKind::Interface, None, &[]);
    decl.constants = FORMATS
        .iter()
        .map(|(name, _)| ClassConstDecl {
            attributes: if *name == "RFC7231" {
                rfc7231_deprecation()
            } else {
                Vec::new()
            },
            ..constant(
                name,
                Type::String,
                ConstExpr::Constant(Name(format!("DATE_{name}").into_bytes().into())),
            )
        })
        .collect();
    let abstract_ = |mut decl: MethodDecl| {
        decl.is_abstract = true;
        decl
    };
    decl.methods = vec![
        m("format", vec![p("format", Type::String)], Type::String),
        m("getTimezone", vec![], tz_or_false()),
        m("getOffset", vec![], Type::Int),
        m("getTimestamp", vec![], Type::Int),
        real("getMicrosecond", vec![], Type::Int),
        m("diff", target(), class("DateInterval")),
        wakeup(),
        serialize(),
        unserialize(),
    ]
    .into_iter()
    .map(abstract_)
    .collect();
    decl
}

/// `class DateTime implements DateTimeInterface`.
fn date_time() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let returns = || class("DateTime");
    let interval = || vec![p("interval", class("DateInterval"))];
    let methods: Vec<(MethodDecl, Entry)> = vec![
        (date_ctor(), datetime::construct as Entry),
        (serialize(), datetime::serialize),
        (unserialize(), datetime::datetime_unserialize),
        (wakeup(), datetime::datetime_wakeup),
        (set_state("DateTime"), datetime::datetime_set_state),
        (
            st(
                "createFromImmutable",
                vec![p("object", class("DateTimeImmutable"))],
                Type::CalledClass,
            ),
            datetime::create_from_immutable,
        ),
        (
            st_real(
                "createFromInterface",
                vec![p("object", class("DateTimeInterface"))],
                returns(),
            ),
            datetime::datetime_create_from_interface,
        ),
        (
            from_format("DateTime"),
            datetime::datetime_create_from_format,
        ),
        (from_timestamp(), datetime::datetime_create_from_timestamp),
        (last_errors(), datetime::date_get_last_errors),
        (
            m("format", vec![p("format", Type::String)], Type::String),
            datetime::format_method,
        ),
        (
            m("modify", vec![p("modifier", Type::String)], returns()),
            datetime::datetime_modify,
        ),
        (m("add", interval(), returns()), datetime::date_add),
        (m("sub", interval(), returns()), datetime::datetime_sub),
        (
            m("getTimezone", vec![], tz_or_false()),
            datetime::date_timezone_get,
        ),
        (
            m(
                "setTimezone",
                vec![p("timezone", class("DateTimeZone"))],
                returns(),
            ),
            datetime::date_timezone_set,
        ),
        (m("getOffset", vec![], Type::Int), datetime::date_offset_get),
        (
            real("getMicrosecond", vec![], Type::Int),
            datetime::get_microsecond,
        ),
        (set_time("DateTime"), datetime::date_time_set),
        (set_date("DateTime"), datetime::date_date_set),
        (set_isodate("DateTime"), datetime::date_isodate_set),
        (
            m("setTimestamp", vec![p("timestamp", Type::Int)], returns()),
            datetime::date_timestamp_set,
        ),
        (
            real(
                "setMicrosecond",
                vec![p("microsecond", Type::Int)],
                Type::CalledClass,
            ),
            datetime::datetime_set_microsecond,
        ),
        (
            m("getTimestamp", vec![], Type::Int),
            datetime::date_timestamp_get,
        ),
        (
            m("diff", target(), class("DateInterval")),
            datetime::date_diff,
        ),
    ];
    (
        class_decl("DateTime", ClassKind::Class, None, &["DateTimeInterface"]),
        methods,
    )
}

/// `class DateTimeImmutable implements DateTimeInterface`.
#[allow(clippy::too_many_lines)] // the stub's method table, one row per method
fn date_time_immutable() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let returns = || class("DateTimeImmutable");
    let interval = || vec![p("interval", class("DateInterval"))];
    let methods: Vec<(MethodDecl, Entry)> = vec![
        (date_ctor(), datetime::construct as Entry),
        (serialize(), datetime::serialize),
        (unserialize(), datetime::immutable_unserialize),
        (wakeup(), datetime::immutable_wakeup),
        (
            set_state("DateTimeImmutable"),
            datetime::immutable_set_state,
        ),
        (
            from_format("DateTimeImmutable"),
            datetime::immutable_create_from_format,
        ),
        (from_timestamp(), datetime::immutable_create_from_timestamp),
        (last_errors(), datetime::date_get_last_errors),
        (
            m("format", vec![p("format", Type::String)], Type::String),
            datetime::format_method,
        ),
        (
            m("getTimezone", vec![], tz_or_false()),
            datetime::date_timezone_get,
        ),
        (m("getOffset", vec![], Type::Int), datetime::date_offset_get),
        (
            m("getTimestamp", vec![], Type::Int),
            datetime::date_timestamp_get,
        ),
        (
            real("getMicrosecond", vec![], Type::Int),
            datetime::get_microsecond,
        ),
        (
            m("diff", target(), class("DateInterval")),
            datetime::date_diff,
        ),
        (
            no_discard(m("modify", vec![p("modifier", Type::String)], returns())),
            datetime::immutable_modify,
        ),
        (
            no_discard(m("add", interval(), returns())),
            datetime::immutable_add,
        ),
        (
            no_discard(m("sub", interval(), returns())),
            datetime::immutable_sub,
        ),
        (
            no_discard(m(
                "setTimezone",
                vec![p("timezone", class("DateTimeZone"))],
                returns(),
            )),
            datetime::immutable_set_timezone,
        ),
        (
            no_discard(set_time("DateTimeImmutable")),
            datetime::immutable_set_time,
        ),
        (
            no_discard(set_date("DateTimeImmutable")),
            datetime::immutable_set_date,
        ),
        (
            no_discard(set_isodate("DateTimeImmutable")),
            datetime::immutable_set_isodate,
        ),
        (
            no_discard(m(
                "setTimestamp",
                vec![p("timestamp", Type::Int)],
                returns(),
            )),
            datetime::immutable_set_timestamp,
        ),
        (
            no_discard(real(
                "setMicrosecond",
                vec![p("microsecond", Type::Int)],
                Type::CalledClass,
            )),
            datetime::immutable_set_microsecond,
        ),
        (
            st(
                "createFromMutable",
                vec![p("object", class("DateTime"))],
                Type::CalledClass,
            ),
            datetime::create_from_mutable,
        ),
        (
            st_real(
                "createFromInterface",
                vec![p("object", class("DateTimeInterface"))],
                returns(),
            ),
            datetime::immutable_create_from_interface,
        ),
    ];
    (
        class_decl(
            "DateTimeImmutable",
            ClassKind::Class,
            None,
            &["DateTimeInterface"],
        ),
        methods,
    )
}

/// `class DateTimeZone`: its fourteen groups and eleven methods.
fn date_time_zone() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let mut decl = class_decl("DateTimeZone", ClassKind::Class, None, &[]);
    decl.constants = GROUPS
        .iter()
        .map(|(name, value)| constant(name, Type::Int, ConstExpr::Long(*value)))
        .collect();
    let methods: Vec<(MethodDecl, Entry)> = vec![
        (
            ctor(vec![p("timezone", Type::String)]),
            timezone::construct as Entry,
        ),
        (
            m("getName", vec![], Type::String),
            timezone::timezone_name_get,
        ),
        (
            m(
                "getOffset",
                vec![p("datetime", class("DateTimeInterface"))],
                Type::Int,
            ),
            timezone::timezone_offset_get,
        ),
        (
            m(
                "getTransitions",
                vec![
                    d(
                        "timestampBegin",
                        Type::Int,
                        ConstExpr::Constant(Name(b"PHP_INT_MIN".as_slice().into())),
                    ),
                    d("timestampEnd", Type::Int, ConstExpr::Long(2_147_483_647)),
                ],
                union(vec![Type::Array, Type::False]),
            ),
            timezone::timezone_transitions_get,
        ),
        (
            m("getLocation", vec![], union(vec![Type::Array, Type::False])),
            timezone::timezone_location_get,
        ),
        (
            st("listAbbreviations", vec![], Type::Array),
            timezone::list_abbreviations,
        ),
        (
            st(
                "listIdentifiers",
                vec![
                    d(
                        "timezoneGroup",
                        Type::Int,
                        ConstExpr::ClassConstant {
                            class: Name(b"DateTimeZone".as_slice().into()),
                            name: Name(b"ALL".as_slice().into()),
                        },
                    ),
                    d("countryCode", nullable(Type::String), ConstExpr::Null),
                ],
                Type::Array,
            ),
            timezone::list_identifiers,
        ),
        (serialize(), timezone::serialize),
        (unserialize(), timezone::unserialize),
        (wakeup(), timezone::wakeup),
        (set_state("DateTimeZone"), timezone::set_state),
    ];
    (decl, methods)
}

/// `class DateInterval`.
fn date_interval() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let methods: Vec<(MethodDecl, Entry)> = vec![
        (
            ctor(vec![p("duration", Type::String)]),
            interval::construct as Entry,
        ),
        (
            st(
                "createFromDateString",
                vec![p("datetime", Type::String)],
                class("DateInterval"),
            ),
            interval::create_from_date_string,
        ),
        (
            m("format", vec![p("format", Type::String)], Type::String),
            interval::date_interval_format,
        ),
        (serialize(), interval::serialize),
        (unserialize(), interval::unserialize),
        (wakeup(), interval::wakeup),
        (set_state("DateInterval"), interval::set_state),
    ];
    (
        class_decl("DateInterval", ClassKind::Class, None, &[]),
        methods,
    )
}

/// An untyped parameter whose stub default is `UNKNOWN`: optional, with
/// no value Reflection can show.
fn unknown(name: &str) -> Parameter {
    Parameter {
        default: Some(ConstExpr::Constant(Name(b"UNKNOWN".as_slice().into()))),
        ..untyped(name)
    }
}

/// One of `DatePeriod`'s seven properties, typed and without a default,
/// which its handlers answer in place of a slot (php's `ZEND_ACC_VIRTUAL`).
fn period_property(name: &str, ty: Type) -> PropertyDecl {
    PropertyDecl {
        name: Name(name.as_bytes().into()),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
        ty: Some(ty),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    }
}

/// `class DatePeriod`. php declares it `implements IteratorAggregate`
/// with `getIterator()`; the engine declares neither `Traversable` nor
/// `InternalIterator` yet, so both wait for them (#714).
fn date_period() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let mut decl = class_decl("DatePeriod", ClassKind::Class, None, &[]);
    decl.constants = vec![
        constant(
            "EXCLUDE_START_DATE",
            Type::Int,
            ConstExpr::Long(EXCLUDE_START_DATE),
        ),
        constant(
            "INCLUDE_END_DATE",
            Type::Int,
            ConstExpr::Long(INCLUDE_END_DATE),
        ),
    ];
    let date = || nullable(class("DateTimeInterface"));
    decl.properties = vec![
        period_property("start", date()),
        period_property("current", date()),
        period_property("end", date()),
        period_property("interval", nullable(class("DateInterval"))),
        period_property("recurrences", Type::Int),
        period_property("include_start_date", Type::Bool),
        period_property("include_end_date", Type::Bool),
    ];
    let methods: Vec<(MethodDecl, Entry)> = vec![
        (
            st_real(
                "createFromISO8601String",
                vec![
                    p("specification", Type::String),
                    d("options", Type::Int, ConstExpr::Long(0)),
                ],
                Type::CalledClass,
            ),
            period::create_from_iso8601_string as Entry,
        ),
        (
            ctor(vec![
                untyped("start"),
                unknown("interval"),
                unknown("end"),
                unknown("options"),
            ]),
            period::construct,
        ),
        (
            m("getStartDate", vec![], class("DateTimeInterface")),
            period::get_start_date,
        ),
        (
            m("getEndDate", vec![], nullable(class("DateTimeInterface"))),
            period::get_end_date,
        ),
        (
            m("getDateInterval", vec![], class("DateInterval")),
            period::get_date_interval,
        ),
        (
            m("getRecurrences", vec![], nullable(Type::Int)),
            period::get_recurrences,
        ),
        (serialize(), period::serialize),
        (unserialize(), period::unserialize),
        (wakeup(), period::wakeup),
        (set_state("DatePeriod"), period::set_state),
    ];
    (decl, methods)
}

/// `date_register_classes`: the interface, the five classes with their
/// payloads and tables, and 8.3's `DateError` and `DateException` classes.
pub(super) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    let (mutable, mutable_methods) = date_time();
    let (immutable, immutable_methods) = date_time_immutable();
    let (tz, tz_methods) = date_time_zone();
    let (iv, iv_methods) = date_interval();
    let (pe, pe_methods) = date_period();
    let errors = [
        ("DateError", "Error"),
        ("DateObjectError", "DateError"),
        ("DateRangeError", "DateError"),
        ("DateException", "Exception"),
        ("DateInvalidTimeZoneException", "DateException"),
        ("DateInvalidOperationException", "DateException"),
        ("DateMalformedStringException", "DateException"),
        ("DateMalformedIntervalStringException", "DateException"),
        ("DateMalformedPeriodStringException", "DateException"),
    ]
    .into_iter()
    .map(|(name, parent)| {
        (
            class_decl(name, ClassKind::Class, Some(parent), &[]),
            Vec::new(),
        )
    })
    .collect();
    builder
        .classes(vec![(interface(), Vec::new())])
        .class_payload::<DateObj>(mutable, mutable_methods, &DATE)
        .class_payload::<DateObj>(immutable, immutable_methods, &DATE)
        .class_payload::<TzObj>(tz, tz_methods, &TIMEZONE)
        .class_payload::<IntervalObj>(iv, iv_methods, &INTERVAL)
        .class_payload::<PeriodObj>(pe, pe_methods, &PERIOD)
        .classes(errors)
}
