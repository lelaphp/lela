//! `php_json_encode_zval` and what it calls: `json_encoder.c` at the
//! tag, one `Vec<u8>` where php has a `smart_str`.
//!
//! What is copied exactly, because it is observable: the byte map that
//! decides which characters are escaped and the escape each is given;
//! the UTF-8 walk of `php_next_utf8_char` and its three
//! `INVALID_UTF8_*` arms; `zend_gcvt` at `serialize_precision`, with
//! the lowercase exponent json prints; the order the properties of an
//! object are written in and the private and protected ones skipped;
//! the recursion guard on an array, on an object and on a
//! `JsonSerializable`; and the depth compared *after* a bracket's
//! elements, which is why `json_encode($o, 0, 1)` is `{}` for an empty
//! object and `false` for one with anything in it.

use php_ext::call;
use php_ext::coerce::{Exponent, float_format, numeric_string};
use php_ext::decl::{ClassKind, EnumBacking};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpObject, Value, ValueRef};

use crate::{
    ERROR_INF_OR_NAN, ERROR_NON_BACKED_ENUM, ERROR_NONE, ERROR_RECURSION, ERROR_UNSUPPORTED_TYPE,
    ERROR_UTF8, FORCE_OBJECT, HEX_AMP, HEX_APOS, HEX_QUOT, HEX_TAG, INVALID_UTF8_IGNORE,
    INVALID_UTF8_SUBSTITUTE, NUMERIC_CHECK, PARTIAL_OUTPUT_ON_ERROR, PRESERVE_ZERO_FRACTION,
    PRETTY_PRINT, UNESCAPED_LINE_TERMINATORS, UNESCAPED_SLASHES, UNESCAPED_UNICODE,
};

/// `php_json_encoder` beside the two recursion guards php keeps on the
/// values themselves: this engine's face over an object has no bit to
/// set, so the walk carries the store entries it is inside.
pub(super) struct Encoder {
    /// `encoder->depth`.
    depth: i64,
    /// `encoder->max_depth`, `json_encode`'s third argument.
    max_depth: i64,
    /// `encoder->error_code`, what `json_last_error` is given after.
    pub(super) error_code: i64,
    /// `serialize_precision`, read once as php reads `PG()`.
    precision: i32,
    /// `GC_PROTECT_RECURSION` over the object being written out.
    inside: Vec<u32>,
    /// `ZEND_GUARD_PROTECT_RECURSION(guard, JSON)` over the object
    /// whose `jsonSerialize` is being served.
    serializing: Vec<u32>,
    /// `php_json_serializable_ce`, looked up once.
    serializable: Option<ClassId>,
}

impl Encoder {
    /// `php_json_encode_init` with the maximum depth `json_encode` was given.
    pub(super) fn new(req: &mut Request, max_depth: i64) -> Result<Encoder, Thrown> {
        Ok(Encoder {
            depth: 0,
            max_depth,
            error_code: ERROR_NONE,
            precision: serialize_precision(req),
            inside: Vec::new(),
            serializing: Vec::new(),
            serializable: call::lookup_class(req, b"JsonSerializable")?,
        })
    }
}

/// `PG(serialize_precision)`, the digits `zend_gcvt` is asked for.
fn serialize_precision(req: &Request) -> i32 {
    req.ini_get(b"serialize_precision")
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(-1)
}

/// php's `SUCCESS` and `FAILURE` from one of the encoder's functions.
/// A `FAILURE` is not the end of the call: `json_encode` reads
/// `error_code` and `JSON_PARTIAL_OUTPUT_ON_ERROR` to decide what to
/// answer, and only a PHP throwable ends the native, as the `Err` here.
type Wrote = Result<bool, Thrown>;

/// `php_json_encode_zval`: the value's arm, a reference followed first.
pub(super) fn encode(
    req: &mut Request,
    buf: &mut Vec<u8>,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => buf.extend_from_slice(b"null"),
        ValueRef::Bool(true) => buf.extend_from_slice(b"true"),
        ValueRef::Bool(false) => buf.extend_from_slice(b"false"),
        ValueRef::Int(n) => buf.extend_from_slice(n.to_string().as_bytes()),
        ValueRef::Float(f) => {
            if f.is_finite() {
                encode_double(buf, f, options, enc.precision);
            } else {
                enc.error_code = ERROR_INF_OR_NAN;
                buf.push(b'0');
            }
        }
        ValueRef::Str(s) => return Ok(escape_string(buf, s.as_bytes(), options, enc)),
        ValueRef::Object(o) => {
            let object = *o;
            if enc
                .serializable
                .is_some_and(|ce| object.instance_of(req, ce))
            {
                return serializable(req, buf, object, options, enc);
            }
            if let Some(kind) = req.class_decl(object.class()).map(|decl| decl.kind)
                && kind == ClassKind::Enum
            {
                return enum_case(req, buf, object, options, enc);
            }
            return held(req, buf, &value, options, enc);
        }
        ValueRef::Array(_) => return held(req, buf, &value, options, enc),
        ValueRef::Resource(_) | ValueRef::Reference(_) => {
            enc.error_code = ERROR_UNSUPPORTED_TYPE;
            if options & PARTIAL_OUTPUT_ON_ERROR != 0 {
                buf.extend_from_slice(b"null");
            }
            return Ok(false);
        }
    }
    Ok(true)
}

/// `php_json_encode_zval`'s `IS_ARRAY` and `IS_OBJECT` arms, which copy
/// the value before they walk it (`ZVAL_COPY`) so nothing a callback
/// does can free what is being written out.
fn held(
    req: &mut Request,
    buf: &mut Vec<u8>,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    req.addref(value);
    let wrote = encode_array(req, buf, value, options, enc);
    req.release(*value);
    wrote
}

/// `php_json_encode_double`: `zend_gcvt` at `serialize_precision`, with
/// `.0` appended under `JSON_PRESERVE_ZERO_FRACTION` where the text
/// carries no decimal point.
fn encode_double(buf: &mut Vec<u8>, value: f64, options: i64, precision: i32) {
    let text = float_format(value, precision, Exponent::Lower);
    let bare = !text.contains('.');
    buf.extend_from_slice(text.as_bytes());
    if options & PRESERVE_ZERO_FRACTION != 0 && bare {
        buf.extend_from_slice(b".0");
    }
}

/// `php_json_escape_string`: the quoted text of one string, or the
/// number itself under `JSON_NUMERIC_CHECK`.
fn escape_string(buf: &mut Vec<u8>, s: &[u8], options: i64, enc: &mut Encoder) -> bool {
    if s.is_empty() {
        buf.extend_from_slice(b"\"\"");
        return true;
    }
    if options & NUMERIC_CHECK != 0
        && let Some(number) = numeric_string(s)
    {
        if let Some(n) = number.as_int() {
            buf.extend_from_slice(n.to_string().as_bytes());
            return true;
        }
        if let Some(f) = number.as_float()
            && f.is_finite()
        {
            encode_double(buf, f, options, enc.precision);
            return true;
        }
    }
    let checkpoint = buf.len();
    buf.push(b'"');
    let mut at = 0;
    while at < s.len() {
        let byte = s.get(at).copied().unwrap_or(0);
        if !escaped(byte) {
            buf.push(byte);
            at += 1;
            continue;
        }
        if byte >= 0x80 {
            let Some(next) = escape_utf8(buf, s, at, options) else {
                buf.truncate(checkpoint);
                enc.error_code = ERROR_UTF8;
                if options & PARTIAL_OUTPUT_ON_ERROR != 0 {
                    buf.extend_from_slice(b"null");
                }
                return false;
            };
            at = next;
            continue;
        }
        escape_ascii(buf, byte, options);
        at += 1;
    }
    buf.push(b'"');
    true
}

/// `php_json_escape_string`'s `charmap`: the control characters, the six
/// characters an option may escape, the backslash, and every byte a
/// UTF-8 sequence may begin.
fn escaped(byte: u8) -> bool {
    !(0x20..0x80).contains(&byte)
        || matches!(byte, b'"' | b'&' | b'\'' | b'/' | b'<' | b'>' | b'\\')
}

/// The one-byte arm of the escape loop: php's `switch (us)`, where each
/// of the six characters is escaped only under its own option and a
/// control character is always `\u00XX`. The case of the hex digits is
/// php's own: `<` and `>` carry a capital where nothing else
/// does.
fn escape_ascii(buf: &mut Vec<u8>, byte: u8, options: i64) {
    let one_of = |buf: &mut Vec<u8>, on: bool, escape: &[u8], bare: u8| {
        if on {
            buf.extend_from_slice(escape);
        } else {
            buf.push(bare);
        }
    };
    match byte {
        b'"' => buf.extend_from_slice(if options & HEX_QUOT != 0 {
            &b"\\u0022"[..]
        } else {
            &b"\\\""[..]
        }),
        b'\\' => buf.extend_from_slice(b"\\\\"),
        b'/' => one_of(buf, options & UNESCAPED_SLASHES == 0, b"\\/", b'/'),
        0x08 => buf.extend_from_slice(b"\\b"),
        0x0c => buf.extend_from_slice(b"\\f"),
        b'\n' => buf.extend_from_slice(b"\\n"),
        b'\r' => buf.extend_from_slice(b"\\r"),
        b'\t' => buf.extend_from_slice(b"\\t"),
        b'<' => one_of(buf, options & HEX_TAG != 0, b"\\u003C", b'<'),
        b'>' => one_of(buf, options & HEX_TAG != 0, b"\\u003E", b'>'),
        b'&' => one_of(buf, options & HEX_AMP != 0, b"\\u0026", b'&'),
        b'\'' => one_of(buf, options & HEX_APOS != 0, b"\\u0027", b'\''),
        _ => push_hex(buf, u32::from(byte)),
    }
}

/// `php_next_utf8_char` and the three arms that follow it: the sequence
/// written as it stands under `JSON_UNESCAPED_UNICODE`, as `\uXXXX`
/// otherwise, and the two `INVALID_UTF8_*` answers for a malformed one.
/// `None` is the refusal the caller turns into `PHP_JSON_ERROR_UTF8`.
fn escape_utf8(buf: &mut Vec<u8>, s: &[u8], at: usize, options: i64) -> Option<usize> {
    let (code, next, ok) = next_utf8_char(s, at);
    if !ok {
        if options & INVALID_UTF8_IGNORE != 0 {
            return Some(next);
        }
        if options & INVALID_UTF8_SUBSTITUTE != 0 {
            if options & UNESCAPED_UNICODE != 0 {
                buf.extend_from_slice("\u{fffd}".as_bytes());
            } else {
                buf.extend_from_slice(b"\\ufffd");
            }
            return Some(next);
        }
        return None;
    }
    let bare = options & UNESCAPED_UNICODE != 0
        && (options & UNESCAPED_LINE_TERMINATORS != 0 || !(0x2028..=0x2029).contains(&code));
    if bare {
        buf.extend_from_slice(s.get(at..next).unwrap_or_default());
        return Some(next);
    }
    if code >= 0x10000 {
        let rest = code - 0x10000;
        push_hex(buf, ((rest >> 10) | 0xd800) & 0xffff);
        push_hex(buf, (rest & 0x3ff) | 0xdc00);
    } else {
        push_hex(buf, code);
    }
    Some(next)
}

/// One `\uXXXX` with php's lowercase hex digits.
fn push_hex(buf: &mut Vec<u8>, code: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    buf.extend_from_slice(b"\\u");
    for shift in [12, 8, 4, 0] {
        let nibble = usize::try_from((code >> shift) & 0xf).unwrap_or(0);
        buf.push(DIGITS.get(nibble).copied().unwrap_or(b'0'));
    }
}

/// `php_next_utf8_char` over `s` from `at`: the code point, where the
/// walk goes on, and whether the sequence was well formed. A malformed
/// one advances by what UTR #36's strategy 2 counts as the illegal
/// sequence, which is what php's `MB_FAILURE` does: never over a byte
/// that begins a sequence of its own.
fn next_utf8_char(s: &[u8], at: usize) -> (u32, usize, bool) {
    let byte = |i: usize| s.get(i).copied();
    let trail = |i: usize| byte(i).is_some_and(|b| (0x80..=0xbf).contains(&b));
    let failure = |advance: usize| (0, at + advance, false);
    let Some(first) = byte(at) else {
        return failure(1);
    };
    let six = |i: usize, shift: u32| u32::from(byte(i).unwrap_or(0) & 0x3f) << shift;
    let width = match first {
        0x00..=0x7f => return (u32::from(first), at + 1, true),
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return failure(1),
    };
    if !(1..width).all(|offset| trail(at + offset)) {
        return failure(illegal_run(s, at, width));
    }
    let code = match width {
        2 => u32::from(first & 0x1f) << 6 | six(at + 1, 0),
        3 => u32::from(first & 0x0f) << 12 | six(at + 1, 6) | six(at + 2, 0),
        _ => u32::from(first & 0x07) << 18 | six(at + 1, 12) | six(at + 2, 6) | six(at + 3, 0),
    };
    let shortest = match width {
        2 => code >= 0x80,
        3 => code >= 0x800 && !(0xd800..=0xdfff).contains(&code),
        _ => (0x1_0000..=0x10_ffff).contains(&code),
    };
    if !shortest {
        return failure(width);
    }
    (code, at + width, true)
}

/// `MB_FAILURE`'s length for a lead byte whose sequence did not
/// complete: the bytes up to the first that is absent or begins a
/// sequence of its own, and the whole width where none is.
fn illegal_run(s: &[u8], at: usize, width: usize) -> usize {
    let lead = |b: u8| b < 0x80 || (0xc2..=0xf4).contains(&b);
    for offset in 1..width {
        match s.get(at + offset) {
            None => return offset,
            Some(&b) if lead(b) => return offset,
            Some(_) => {}
        }
    }
    width
}

/// `php_json_encode_serializable_object`: `jsonSerialize` served on the
/// object, its answer written out, and `$this` from it taken straight
/// to the array arm so the method is not served again.
fn serializable(
    req: &mut Request,
    buf: &mut Vec<u8>,
    object: PhpObject,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    let index = object.index();
    if enc.serializing.contains(&index) {
        enc.error_code = ERROR_RECURSION;
        if options & PARTIAL_OUTPUT_ON_ERROR != 0 {
            buf.extend_from_slice(b"null");
        }
        return Ok(false);
    }
    enc.serializing.push(index);
    let served = call::call_method(req, &object, b"jsonSerialize", &[]);
    let answer = match served {
        Ok(answer) => answer,
        Err(thrown) => {
            enc.serializing.pop();
            if options & PARTIAL_OUTPUT_ON_ERROR != 0 {
                buf.extend_from_slice(b"null");
            }
            return Err(thrown);
        }
    };
    let itself = answer.as_object().is_some_and(|o| o.index() == index);
    if itself {
        enc.serializing.pop();
    }
    let wrote = if itself {
        held(req, buf, &answer, options, enc)
    } else {
        let wrote = encode(req, buf, &answer, options, enc);
        enc.serializing.pop();
        wrote
    };
    req.release(answer);
    wrote
}

/// `php_json_encode_serializable_enum`: a backed case is its value, and
/// a pure one the refusal php has no representation for.
fn enum_case(
    req: &mut Request,
    buf: &mut Vec<u8>,
    object: PhpObject,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    let backed = req
        .class_decl(object.class())
        .is_some_and(|decl| decl.backing != EnumBacking::Pure);
    if !backed {
        enc.error_code = ERROR_NON_BACKED_ENUM;
        buf.push(b'0');
        return Ok(false);
    }
    let case = object.prop(req, b"value").unwrap_or_default();
    encode(req, buf, &case, options, enc)
}

/// The rows an array or an object contributes, in php's order: an
/// array's own, and an object's declared properties and then its
/// dynamic ones with the private and protected of the class skipped,
/// which is `ZSTR_VAL(key)[0] == '\0'` over `zend_get_properties_for`.
/// A hooked property is what its `get` hook answers, and a virtual one
/// with none is no row; a class that answers `ZEND_PROP_PURPOSE_JSON`
/// itself is its answer.
///
/// ```php
/// class H { public $a = 1; public $v { get => 'v'; } protected $p = 2; public $w { set {} } }
/// class P { private $x = 1; } class C extends P { public $y = 2; }
/// echo json_encode(new H), json_encode(new C), "\n";
/// echo json_encode(new DateTimeImmutable('2000-01-01 UTC')), "\n";
/// ```
/// ```expect
/// {"a":1,"v":"v"}{"y":2}
/// {"date":"2000-01-01 00:00:00.000000","timezone_type":3,"timezone":"UTC"}
/// ```
fn rows(req: &mut Request, value: &Value) -> Result<Vec<(Key, Value)>, Thrown> {
    if let Some(array) = value.as_array() {
        return Ok(array.snapshot());
    }
    let Some(object) = value.as_object() else {
        return Ok(Vec::new());
    };
    // `zend_get_properties_for(val, ZEND_PROP_PURPOSE_JSON)`, which
    // initialises a lazy object and reads what that answers.
    let object = &php_ext::call::initialize_lazy(req, object)?;
    // The class's own table where it answers the purpose itself, as
    // `DateTime`'s and `DatePeriod`'s do, else its hooked properties'
    // or its members', every name mangled as the table holds it.
    Ok(object
        .props_for(req, php_ext::abi::PropertyPurpose::Json)?
        .into_iter()
        .filter(|(name, _)| name.as_bytes().first() != Some(&0))
        .map(|(name, value)| (Key::Str(name), value))
        .collect())
}

/// `php_json_encode_array`: the brackets, the rows between them, the
/// recursion guard around the walk and the depth read after it.
fn encode_array(
    req: &mut Request,
    buf: &mut Vec<u8>,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    let array = value.as_array().copied();
    let object = value.as_object().copied();
    let mut as_object = options & FORCE_OBJECT != 0;
    if let Some(array) = array {
        as_object = as_object || !array.is_list();
        if array.is_recursive() {
            enc.error_code = ERROR_RECURSION;
            buf.extend_from_slice(b"null");
            return Ok(false);
        }
        array.protect();
    }
    if let Some(object) = object {
        as_object = true;
        if enc.inside.contains(&object.index()) {
            enc.error_code = ERROR_RECURSION;
            buf.extend_from_slice(b"null");
            return Ok(false);
        }
        enc.inside.push(object.index());
    }
    let wrote = write_rows(req, buf, value, options, enc, as_object);
    if let Some(array) = array {
        array.unprotect();
    }
    if object.is_some() {
        enc.inside.pop();
    }
    wrote
}

/// The body of `php_json_encode_array` inside its guard.
fn write_rows(
    req: &mut Request,
    buf: &mut Vec<u8>,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
    as_object: bool,
) -> Wrote {
    buf.push(if as_object { b'{' } else { b'[' });
    enc.depth += 1;
    let mut comma = false;
    for (key, element) in rows(req, value)? {
        if comma {
            buf.push(b',');
        }
        comma = true;
        pretty_break(buf, options, enc);
        if as_object {
            write_key(buf, &key, options, enc);
            buf.push(b':');
            if options & PRETTY_PRINT != 0 {
                buf.push(b' ');
            }
        }
        if !encode(req, buf, &element, options, enc)? && options & PARTIAL_OUTPUT_ON_ERROR == 0 {
            return Ok(false);
        }
    }
    if enc.depth > enc.max_depth {
        enc.error_code = crate::ERROR_DEPTH;
        if options & PARTIAL_OUTPUT_ON_ERROR == 0 {
            return Ok(false);
        }
    }
    enc.depth -= 1;
    if comma {
        pretty_break(buf, options, enc);
    }
    buf.push(if as_object { b'}' } else { b']' });
    Ok(true)
}

/// One key of an object: the string escaped with `JSON_NUMERIC_CHECK`
/// taken off it, and an integer key quoted as php's
/// `smart_str_append_long` between two quotes.
fn write_key(buf: &mut Vec<u8>, key: &Key, options: i64, enc: &mut Encoder) {
    match key {
        Key::Str(name) => {
            let at = buf.len();
            let wrote = escape_string(buf, name.as_bytes(), options & !NUMERIC_CHECK, enc);
            if !wrote && options & PARTIAL_OUTPUT_ON_ERROR != 0 {
                buf.truncate(at);
                buf.extend_from_slice(b"\"\"");
            }
        }
        Key::Int(n) => {
            buf.push(b'"');
            buf.extend_from_slice(n.to_string().as_bytes());
            buf.push(b'"');
        }
    }
}

/// `php_json_pretty_print_char` and `php_json_pretty_print_indent`: the
/// newline and the four spaces a level, and nothing without the option.
fn pretty_break(buf: &mut Vec<u8>, options: i64, enc: &Encoder) {
    if options & PRETTY_PRINT == 0 {
        return;
    }
    buf.push(b'\n');
    for _ in 0..enc.depth {
        buf.extend_from_slice(b"    ");
    }
}
