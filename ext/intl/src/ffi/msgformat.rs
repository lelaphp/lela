//! `umsg.h`: the message format php's `MessageFormatter` holds, one
//! `UMessageFormat` per object; and `shim/msgformat.cpp`, where php's
//! `msgformat_helpers.cpp` reads the same format as an
//! `icu::MessageFormat`: its argument types, its pattern's parts, its
//! date subformats, `format()` over typed arguments and `parse()`.

use std::ffi::{CStr, c_char};
use std::ptr::NonNull;

use super::cpp::{Text, UStr, opaque, owner};
use super::timezone::TimeZone;
use super::{Code, ParseError, length};

opaque!(
    /// `UMessageFormat`, an `icu::MessageFormat`.
    UMessageFormat,
    /// What `MessageFormat::parse` answers: its `Formattable`s and
    /// their count.
    ParsedList
);

owner!(
    /// One `UMessageFormat`, from `umsg_open` to `umsg_close`.
    MessageFormat(UMessageFormat),
    umsg_close
);

owner!(
    /// The `Formattable`s one `parse()` answered.
    Parsed(ParsedList),
    php_intl_msgfmt_parsed_delete
);

use sys::umsg_close;

mod sys {
    use std::ffi::c_char;
    use std::ptr::NonNull;

    use super::super::{Code, ParseError};
    use super::{MessageFormat, UMessageFormat};

    icu! {
        fn umsg_open(
            pattern: *const u16,
            len: i32,
            locale: *const c_char,
            parse_error: Option<&mut ParseError>,
            status: &mut Code,
        ) -> Option<MessageFormat>;
        fn umsg_close(fmt: NonNull<UMessageFormat>);
        fn umsg_clone(fmt: &UMessageFormat, status: &mut Code) -> Option<MessageFormat>;
        fn umsg_applyPattern(
            fmt: &mut UMessageFormat,
            pattern: *const u16,
            len: i32,
            parse_error: &mut ParseError,
            status: &mut Code,
        );
        fn umsg_getLocale(fmt: &UMessageFormat) -> *const c_char;
    }
}

/// `php_intl_msgfmt_part`.
#[repr(C)]
#[derive(Default)]
struct RawPart {
    kind: i32,
    arg_type: i32,
    value: i32,
    index: i32,
    length: i32,
}

/// `php_intl_msgfmt_arg`.
#[repr(C)]
struct RawArg<'a> {
    name: Text<'a>,
    kind: i32,
    number: f64,
    integer: i64,
    text: Text<'a>,
}

/// `php_intl_msgfmt_value`.
#[repr(C)]
struct RawValue {
    kind: i32,
    number: f64,
    integer: i64,
    units: *const u16,
    length: i32,
}

impl Default for RawValue {
    fn default() -> RawValue {
        RawValue {
            kind: 0,
            number: 0.0,
            integer: 0,
            units: std::ptr::null(),
            length: 0,
        }
    }
}

unsafe extern "C" {
    safe fn php_intl_msgfmt_named(fmt: &UMessageFormat) -> bool;
    safe fn php_intl_msgfmt_arg_type(fmt: &UMessageFormat, i: i32) -> i32;
    safe fn php_intl_msgfmt_part_count(fmt: &UMessageFormat) -> i32;
    safe fn php_intl_msgfmt_part(fmt: &UMessageFormat, i: i32, out: &mut RawPart) -> bool;
    safe fn php_intl_msgfmt_pattern(fmt: &UMessageFormat, length: &mut i32) -> *const u16;
    safe fn php_intl_msgfmt_date_formats(fmt: &mut UMessageFormat, zone: Option<&TimeZone>) -> i32;
    fn php_intl_msgfmt_format(
        fmt: &UMessageFormat,
        args: *const RawArg,
        count: i32,
        status: &mut Code,
    ) -> Option<UStr>;
    safe fn php_intl_msgfmt_parse(
        fmt: &UMessageFormat,
        source: Text,
        status: &mut Code,
    ) -> Option<Parsed>;
    safe fn php_intl_msgfmt_parsed_count(parsed: &ParsedList) -> i32;
    safe fn php_intl_msgfmt_parsed_at(parsed: &ParsedList, i: i32, out: &mut RawValue) -> bool;
    fn php_intl_msgfmt_parsed_delete(parsed: NonNull<ParsedList>);
}

/// `Formattable::Type`, what an argument is formatted as.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArgType {
    Date = 0,
    Double = 1,
    Long = 2,
    String = 3,
    Array = 4,
    Int64 = 5,
    /// `kObject`, which php reads as a type the pattern does not say.
    Object = 6,
}

impl ArgType {
    fn of(kind: i32) -> Option<ArgType> {
        [
            ArgType::Date,
            ArgType::Double,
            ArgType::Long,
            ArgType::String,
            ArgType::Array,
            ArgType::Int64,
            ArgType::Object,
        ]
        .into_iter()
        .find(|t| *t as i32 == kind)
    }
}

/// `UMessagePatternArgType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArgKind {
    /// `UMSGPAT_ARG_TYPE_NONE`: `{name}` alone.
    Untyped,
    Simple,
    Choice,
    Plural,
    Select,
    SelectOrdinal,
}

/// `UMessagePatternPartType`, the kinds `umsg_parse_format` tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartKind {
    ArgStart(Option<ArgKind>),
    ArgNumber(i32),
    ArgName,
    ArgType,
    ArgStyle,
    Other,
}

/// One part of a format's pattern and the substring it covers.
pub(crate) struct Part<'f> {
    pub(crate) kind: PartKind,
    pub(crate) text: &'f [u16],
}

/// One argument as `format()` takes it, or one value as `parse()`
/// answers it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Formattable {
    /// Milliseconds since the epoch.
    Date(f64),
    Double(f64),
    Long(i32),
    Int64(i64),
    String(Vec<u16>),
}

/// `umsg_open`: the pattern, or null where none is given, as php hands
/// ICU a pattern it never converted. ICU answers the format it made
/// whether or not the pattern parsed, its failure in `status`; a format
/// beside a failure is closed here ([`Code::built`]).
pub(crate) fn open(
    pattern: Option<&[u16]>,
    locale: &CStr,
    parse_error: Option<&mut ParseError>,
    status: &mut Code,
) -> Option<MessageFormat> {
    let (ptr, len) = pattern.map_or((std::ptr::null(), 0), |p| {
        (p.as_ptr(), length(p.len()).unwrap_or(0))
    });
    // SAFETY: the pattern is len UChars or null with zero, the locale
    // NUL-terminated, the parse error a live struct or none.
    let made = unsafe { sys::umsg_open(ptr, len, locale.as_ptr(), parse_error, status) };
    status.built(made)
}

impl MessageFormat {
    /// `umsg_clone`.
    pub(crate) fn try_clone(&self, status: &mut Code) -> Option<MessageFormat> {
        // SAFETY: the format is open.
        let made = unsafe { sys::umsg_clone(self, status) };
        status.built(made)
    }

    /// `umsg_applyPattern`.
    pub(crate) fn apply_pattern(
        &mut self,
        pattern: &[u16],
        parse_error: &mut ParseError,
        status: &mut Code,
    ) {
        let len = length(pattern.len()).unwrap_or(0);
        // SAFETY: the pattern is len UChars of a live slice.
        unsafe { sys::umsg_applyPattern(self, pattern.as_ptr(), len, parse_error, status) };
    }

    /// `umsg_getLocale`, copied out of the format that keeps it.
    pub(crate) fn locale(&self) -> Vec<u8> {
        // SAFETY: the format is open; ICU answers its locale's name.
        let name: *const c_char = unsafe { sys::umsg_getLocale(self) };
        if name.is_null() {
            return Vec::new();
        }
        // SAFETY: a NUL-terminated name the format holds while `self` is
        // borrowed, copied before the borrow ends.
        unsafe { CStr::from_ptr(name) }.to_bytes().to_vec()
    }

    /// `MessageFormat::usesNamedArguments`.
    pub(crate) fn named(&self) -> bool {
        php_intl_msgfmt_named(self)
    }

    /// `getArgTypeList`, as php's `MessageFormatAdapter` reads it.
    pub(crate) fn arg_types(&self) -> impl Iterator<Item = ArgType> {
        (0..).map_while(|i| ArgType::of(php_intl_msgfmt_arg_type(self, i)))
    }

    /// `MessagePattern::countParts`.
    pub(crate) fn part_count(&self) -> i32 {
        php_intl_msgfmt_part_count(self)
    }

    /// `MessagePattern::getPart(i)` and its substring; none past the
    /// last part.
    pub(crate) fn part(&self, i: i32) -> Option<Part<'_>> {
        let mut raw = RawPart::default();
        if !php_intl_msgfmt_part(self, i, &mut raw) {
            return None;
        }
        let arg_kind = match raw.arg_type {
            0 => Some(ArgKind::Untyped),
            1 => Some(ArgKind::Simple),
            2 => Some(ArgKind::Choice),
            3 => Some(ArgKind::Plural),
            4 => Some(ArgKind::Select),
            5 => Some(ArgKind::SelectOrdinal),
            _ => None,
        };
        let kind = match raw.kind {
            5 => PartKind::ArgStart(arg_kind),
            7 => PartKind::ArgNumber(raw.value),
            8 => PartKind::ArgName,
            9 => PartKind::ArgType,
            10 => PartKind::ArgStyle,
            _ => PartKind::Other,
        };
        let start = usize::try_from(raw.index).unwrap_or(0);
        let end = start + usize::try_from(raw.length).unwrap_or(0);
        let text = self.pattern().get(start..end).unwrap_or_default();
        Some(Part { kind, text })
    }

    /// The pattern string the parts index.
    fn pattern(&self) -> &[u16] {
        let mut len = 0;
        let ptr = php_intl_msgfmt_pattern(self, &mut len);
        let len = usize::try_from(len).unwrap_or(0);
        if ptr.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: the pattern's buffer holds `len` units and lives as
        // long as the format, which `self` borrows.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    /// How many of the subformats are date formats, each handed a clone
    /// of `zone` where one is given; none where ICU has no subformats to
    /// answer.
    pub(crate) fn date_formats(&mut self, zone: Option<&TimeZone>) -> Option<u32> {
        u32::try_from(php_intl_msgfmt_date_formats(self, zone)).ok()
    }

    /// `MessageFormat::format` over the arguments, each under its name;
    /// none where it fails, the failure in `status`.
    pub(crate) fn format(
        &self,
        args: &[(Vec<u16>, Formattable)],
        status: &mut Code,
    ) -> Option<UStr> {
        let raw: Vec<RawArg> = args
            .iter()
            .map(|(name, value)| {
                let (kind, number, integer, text) = match value {
                    Formattable::Date(d) => (ArgType::Date, *d, 0, &[][..]),
                    Formattable::Double(d) => (ArgType::Double, *d, 0, &[][..]),
                    Formattable::Long(n) => (ArgType::Long, 0.0, i64::from(*n), &[][..]),
                    Formattable::Int64(n) => (ArgType::Int64, 0.0, *n, &[][..]),
                    Formattable::String(s) => (ArgType::String, 0.0, 0, s.as_slice()),
                };
                RawArg {
                    name: Text::from(name.as_slice()),
                    kind: kind as i32,
                    number,
                    integer,
                    text: Text::from(text),
                }
            })
            .collect();
        let count = length(raw.len()).unwrap_or(0);
        // SAFETY: `raw` holds `count` arguments, each reading slices that
        // `args` keeps alive across the call.
        unsafe { php_intl_msgfmt_format(self, raw.as_ptr(), count, status) }
    }

    /// `MessageFormat::parse`: each value, `None` for one of a type php
    /// cannot answer (`kObject`, `kArray`); none at all where the parse
    /// fails, the failure in `status`.
    pub(crate) fn parse(
        &self,
        source: &[u16],
        status: &mut Code,
    ) -> Option<Vec<Option<Formattable>>> {
        let parsed = php_intl_msgfmt_parse(self, Text::from(source), status)?;
        let count = php_intl_msgfmt_parsed_count(&parsed);
        let values = (0..count)
            .map(|i| {
                let mut raw = RawValue::default();
                if !php_intl_msgfmt_parsed_at(&parsed, i, &mut raw) {
                    return None;
                }
                Some(match ArgType::of(raw.kind)? {
                    ArgType::Date => Formattable::Date(raw.number),
                    ArgType::Double => Formattable::Double(raw.number),
                    #[allow(clippy::cast_possible_truncation, reason = "a kLong is an int32_t")]
                    ArgType::Long => Formattable::Long(raw.integer as i32),
                    ArgType::Int64 => Formattable::Int64(raw.integer),
                    ArgType::String => Formattable::String(units(&parsed, &raw).to_vec()),
                    ArgType::Array | ArgType::Object => return None,
                })
            })
            .collect();
        Some(values)
    }
}

/// A parsed string's units, borrowed from the list that holds it.
fn units<'p>(_list: &'p Parsed, raw: &RawValue) -> &'p [u16] {
    let len = usize::try_from(raw.length).unwrap_or(0);
    if raw.units.is_null() || len == 0 {
        return &[];
    }
    // SAFETY: the shim pointed at the buffer of a `UnicodeString` inside
    // the list, `length` units long, which lives as long as the list.
    unsafe { std::slice::from_raw_parts(raw.units, len) }
}
