//! `php_json_encode_zval` and what it calls: `json_encoder.c` at the
//! tag, filling the `smart_str` php fills ([`SmartStr`]).
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

use std::ops::ControlFlow;

use php_ext::abi::PropertyPurpose;
use php_ext::call;
use php_ext::coerce::{Exponent, gcvt, numeric_string};
use php_ext::decl::{ClassKind, EnumBacking};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold, Unwritten};
use php_ext::values::{
    ClassId, HeldProperties, Key, Owner, PhpArray, PhpObject, RecursionGuard, SmartStr, Value,
    ValueRef,
};

use crate::constants::{
    JSON_ERROR_DEPTH, JSON_ERROR_INF_OR_NAN, JSON_ERROR_NON_BACKED_ENUM, JSON_ERROR_NONE,
    JSON_ERROR_RECURSION, JSON_ERROR_UNSUPPORTED_TYPE, JSON_ERROR_UTF8, JSON_FORCE_OBJECT,
    JSON_HEX_AMP, JSON_HEX_APOS, JSON_HEX_QUOT, JSON_HEX_TAG, JSON_INVALID_UTF8_IGNORE,
    JSON_INVALID_UTF8_SUBSTITUTE, JSON_NUMERIC_CHECK, JSON_PARTIAL_OUTPUT_ON_ERROR,
    JSON_PRESERVE_ZERO_FRACTION, JSON_PRETTY_PRINT, JSON_UNESCAPED_LINE_TERMINATORS,
    JSON_UNESCAPED_SLASHES, JSON_UNESCAPED_UNICODE,
};

/// `php_json_encoder`. The recursion guards are on the values
/// themselves, as php keeps them: an array's and an object's
/// `GC_PROTECTED`, and a `JsonSerializable`'s `JSON` guard, which a
/// `json_encode()` called from its own `jsonSerialize()` sees too.
pub(super) struct Encoder {
    /// `encoder->depth`.
    depth: i64,
    /// `encoder->max_depth`, `json_encode`'s third argument.
    max_depth: i64,
    /// `encoder->error_code`, what `json_last_error` is given after.
    pub(super) error_code: i64,
    /// `serialize_precision`, read once as php reads `PG()`.
    precision: i32,
    /// `php_json_serializable_ce`, looked up once.
    serializable: Option<ClassId>,
}

impl Encoder {
    /// `php_json_encode_init` with the maximum depth `json_encode` was given.
    pub(super) fn new(req: &mut Request, max_depth: i64) -> Result<Encoder, Thrown> {
        Ok(Encoder {
            depth: 0,
            max_depth,
            error_code: JSON_ERROR_NONE,
            precision: req.ini().serialize_precision(),
            serializable: call::lookup_class(req, b"JsonSerializable")?,
        })
    }
}

/// php's `SUCCESS` and `FAILURE` from one of the encoder's functions.
/// A `FAILURE` is not the end of the call: `json_encode` reads
/// `error_code` and `JSON_PARTIAL_OUTPUT_ON_ERROR` to decide what to
/// answer, and only a PHP throwable ends the native, as the `Err` here.
type Wrote = Result<bool, Thrown>;

/// `php_json_encode_zval`: the value's arm, a reference followed first.
pub(super) fn encode(
    req: &mut Request,
    buf: &mut SmartStr,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    // `FAILURE` once the memory limit was met, since 8.5.12, the limit's
    // fatal raised as the call returns.
    if req.memory().has_oom() {
        return Ok(false);
    }
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Object(object) => {
            if enc
                .serializable
                .is_some_and(|ce| object.instance_of(req, ce))
            {
                return serializable(req, buf, object, options, enc);
            }
            if let Some(kind) = req
                .symbols()
                .class_decl(object.class())
                .map(|decl| decl.kind)
                && kind == ClassKind::Enum
            {
                return enum_case(req, buf, object, options, enc);
            }
            held(req, buf, &value, options, enc)
        }
        ValueRef::Array(_) => held(req, buf, &value, options, enc),
        _ => encode_scalar(req, buf, &value, options, enc),
    }
}

/// [`encode`]'s arms over a value that holds no other, a call of their
/// own, so what writing one takes is off the stack of a nest encoded
/// level by level (#3159).
#[inline(never)]
fn encode_scalar(
    req: &mut Request,
    buf: &mut SmartStr,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => buf.append(req, b"null")?,
        ValueRef::Bool(true) => buf.append(req, b"true")?,
        ValueRef::Bool(false) => buf.append(req, b"false")?,
        ValueRef::Int(n) => buf.append(req, n.to_string().as_bytes())?,
        ValueRef::Float(f) => {
            if f.is_finite() {
                encode_double(req, buf, f, options, enc.precision)?;
            } else {
                enc.error_code = JSON_ERROR_INF_OR_NAN;
                buf.append(req, b"0")?;
            }
        }
        ValueRef::Str(s) => return escape_string(req, buf, s.as_bytes(), options, enc),
        ValueRef::Object(_) | ValueRef::Array(_) => {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: json_encode's scalar arm handed an array or an object",
            ));
        }
        ValueRef::Resource(_) | ValueRef::Reference(_) => {
            enc.error_code = JSON_ERROR_UNSUPPORTED_TYPE;
            partial_null(req, buf, options)?;
            return Ok(false);
        }
    }
    Ok(true)
}

/// `php_json_encode_zval`'s `IS_ARRAY` and `IS_OBJECT` arms, which copy
/// the value before they walk it (`ZVAL_COPY`) so nothing a callback
/// does can free what is being written out, and give the copy up with
/// `zval_ptr_dtor_nogc` (`Z/ext/json/json_encoder.c:672`), so an array
/// or object owners are left of is no possible root of the cycle
/// collector, as php's is not:
///
/// ```php
/// $a = [range(1, 2), range(3, 4)]; $b = [$a, new stdClass];
/// gc_collect_cycles(); $r = gc_status()['roots'];
/// json_encode($b);
/// echo gc_status()['roots'] - $r, "\n";
/// ```
/// ```expect
/// 0
/// ```
fn held(
    req: &mut Request,
    buf: &mut SmartStr,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    let held = Owner::take(req, *value);
    let wrote = encode_array(req, buf, value, options, enc);
    held.give_up_nogc(req);
    wrote
}

/// `php_json_encode_double`: `zend_gcvt` at `serialize_precision` as the
/// entry stands (`Z/ext/json/json_encoder.c:84`), with `.0` appended
/// under `JSON_PRESERVE_ZERO_FRACTION` where the text carries no decimal
/// point. The entry reaches `zend_gcvt` with none of the `precision ?
/// precision : 1` that `var_export` reads it through, so at `0` the one
/// digit is written exponentially once the point stands past it:
///
/// ```php
/// foreach ([0, 1, -1] as $p) {
///     ini_set('serialize_precision', $p);
///     echo $p, ": ", json_encode([10/3, 0.25, 1.5, 0.0, -1.5, 1e25, 0.1]), "|", json_encode(0.0, JSON_PRESERVE_ZERO_FRACTION), "|", var_export(10/3, true), "\n";
/// }
/// ```
/// ```expect
/// 0: [3.0e+0,0.2,2.0e+0,0.0e+0,-2.0e+0,1.0e+25,0.1]|0.0e+0|3.0
/// 1: [3,0.2,2,0,-2,1.0e+25,0.1]|0.0|3.0
/// -1: [3.3333333333333335,0.25,1.5,0,-1.5,1.0e+25,0.1]|0.0|3.3333333333333335
/// ```
fn encode_double(
    req: &mut Request,
    buf: &mut SmartStr,
    value: f64,
    options: i64,
    precision: i32,
) -> Result<(), Thrown> {
    let mut text = gcvt(value, precision, Exponent::Lower);
    if options & JSON_PRESERVE_ZERO_FRACTION != 0 && !text.contains('.') {
        text.push_str(".0");
    }
    buf.append(req, text.as_bytes())
}

/// The `null` written in a failed value's place under
/// `JSON_PARTIAL_OUTPUT_ON_ERROR`, and nothing without it.
fn partial_null(req: &mut Request, buf: &mut SmartStr, options: i64) -> Result<(), Thrown> {
    if options & JSON_PARTIAL_OUTPUT_ON_ERROR != 0 {
        buf.append(req, b"null")?;
    }
    Ok(())
}

/// `php_json_escape_string`: the quoted text of one string, or the
/// number itself under `JSON_NUMERIC_CHECK`; room for the string and
/// its quotes taken first (`smart_str_alloc`), and each stretch of bytes
/// that need no escape appended whole.
fn escape_string(
    req: &mut Request,
    buf: &mut SmartStr,
    s: &[u8],
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    if s.is_empty() {
        buf.append(req, b"\"\"")?;
        return Ok(true);
    }
    if options & JSON_NUMERIC_CHECK != 0
        && let Some(number) = numeric_string(s)
    {
        if let Some(n) = number.as_int() {
            buf.append(req, n.to_string().as_bytes())?;
            return Ok(true);
        }
        if let Some(f) = number.as_float()
            && f.is_finite()
        {
            encode_double(req, buf, f, options, enc.precision)?;
            return Ok(true);
        }
    }
    let checkpoint = buf.as_bytes().len();
    // `smart_str_falloc(buf, len + 2, 0, fallible: true)` since 8.5.12:
    // `FAILURE` where the memory limit refused the room.
    if req
        .memory_mut()
        .fallible(|req| buf.reserve(req, s.len().saturating_add(2)))?
        .is_none()
    {
        return Ok(false);
    }
    buf.append(req, b"\"")?;
    let (mut at, mut run) = (0, 0);
    while let Some(&byte) = s.get(at) {
        if !escaped(byte) {
            at += 1;
            continue;
        }
        // "An escaped byte can append six": `FAILURE` once the memory
        // limit was met, the buffer cut back to where the string began,
        // since 8.5.12.
        if req.memory().has_oom() {
            buf.remove(checkpoint..usize::MAX);
            return Ok(false);
        }
        buf.append(req, s.get(run..at).unwrap_or_default())?;
        if byte < 0x80 {
            escape_ascii(req, buf, byte, options)?;
            at += 1;
        } else if let Some(next) = escape_utf8(req, buf, s, at, options)? {
            at = next;
        } else {
            buf.remove(checkpoint..usize::MAX);
            enc.error_code = JSON_ERROR_UTF8;
            partial_null(req, buf, options)?;
            return Ok(false);
        }
        run = at;
    }
    buf.append(req, s.get(run..).unwrap_or_default())?;
    buf.append(req, b"\"")?;
    Ok(true)
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
fn escape_ascii(
    req: &mut Request,
    buf: &mut SmartStr,
    byte: u8,
    options: i64,
) -> Result<(), Thrown> {
    let one_of =
        |on: bool, escape: &'static [u8], bare: &'static [u8]| if on { escape } else { bare };
    let escape: &[u8] = match byte {
        b'"' => one_of(options & JSON_HEX_QUOT != 0, b"\\u0022", b"\\\""),
        b'\\' => b"\\\\",
        b'/' => one_of(options & JSON_UNESCAPED_SLASHES == 0, b"\\/", b"/"),
        0x08 => b"\\b",
        0x0c => b"\\f",
        b'\n' => b"\\n",
        b'\r' => b"\\r",
        b'\t' => b"\\t",
        b'<' => one_of(options & JSON_HEX_TAG != 0, b"\\u003C", b"<"),
        b'>' => one_of(options & JSON_HEX_TAG != 0, b"\\u003E", b">"),
        b'&' => one_of(options & JSON_HEX_AMP != 0, b"\\u0026", b"&"),
        b'\'' => one_of(options & JSON_HEX_APOS != 0, b"\\u0027", b"'"),
        _ => return push_hex(req, buf, u32::from(byte)),
    };
    buf.append(req, escape)
}

/// `php_next_utf8_char` and the three arms that follow it: the sequence
/// written as it stands under `JSON_UNESCAPED_UNICODE`, as `\uXXXX`
/// otherwise, and the two `INVALID_UTF8_*` answers for a malformed one.
/// `None` is the refusal the caller turns into `PHP_JSON_ERROR_UTF8`.
fn escape_utf8(
    req: &mut Request,
    buf: &mut SmartStr,
    s: &[u8],
    at: usize,
    options: i64,
) -> Result<Option<usize>, Thrown> {
    let (code, next, ok) = next_utf8_char(s, at);
    if !ok {
        if options & JSON_INVALID_UTF8_IGNORE != 0 {
            return Ok(Some(next));
        }
        if options & JSON_INVALID_UTF8_SUBSTITUTE != 0 {
            if options & JSON_UNESCAPED_UNICODE != 0 {
                buf.append(req, "\u{fffd}".as_bytes())?;
            } else {
                buf.append(req, b"\\ufffd")?;
            }
            return Ok(Some(next));
        }
        return Ok(None);
    }
    let bare = options & JSON_UNESCAPED_UNICODE != 0
        && (options & JSON_UNESCAPED_LINE_TERMINATORS != 0 || !(0x2028..=0x2029).contains(&code));
    if bare {
        buf.append(req, s.get(at..next).unwrap_or_default())?;
    } else if code >= 0x10000 {
        let rest = code - 0x10000;
        push_hex(req, buf, ((rest >> 10) | 0xd800) & 0xffff)?;
        push_hex(req, buf, (rest & 0x3ff) | 0xdc00)?;
    } else {
        push_hex(req, buf, code)?;
    }
    Ok(Some(next))
}

/// One `\uXXXX` with php's lowercase hex digits.
fn push_hex(req: &mut Request, buf: &mut SmartStr, code: u32) -> Result<(), Thrown> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = *b"\\u0000";
    for (digit, shift) in hex[2..].iter_mut().zip([12, 8, 4, 0]) {
        let nibble = usize::try_from((code >> shift) & 0xf).unwrap_or(0);
        *digit = DIGITS.get(nibble).copied().unwrap_or(b'0');
    }
    buf.append(req, &hex)
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
/// to the array arm so the method is not served again. The object's
/// `JSON` guard is held while the method runs, so a `json_encode()` of
/// it from inside is the recursion error, a second call's as much as
/// this one's.
///
/// ```php
/// class S implements JsonSerializable {
///     public $a = 1;
///     public function jsonSerialize(): mixed { var_dump(json_encode($this), json_last_error_msg()); return $this; }
/// }
/// var_dump(json_encode(new S));
/// ```
/// ```expect
/// bool(false)
/// string(18) "Recursion detected"
/// string(7) "{"a":1}"
/// ```
fn serializable(
    req: &mut Request,
    buf: &mut SmartStr,
    object: PhpObject,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    if object.is_recursive_in(req, RecursionGuard::Json) {
        enc.error_code = JSON_ERROR_RECURSION;
        partial_null(req, buf, options)?;
        return Ok(false);
    }
    object.protect_in(req, RecursionGuard::Json);
    let served = call::call_method(req, &object, b"jsonSerialize", &[]);
    let answer = match served {
        Ok(answer) => answer,
        Err(thrown) => {
            object.unprotect_in(req, RecursionGuard::Json);
            partial_null(req, buf, options)?;
            return Err(thrown);
        }
    };
    let itself = answer.as_object().is_some_and(|o| o == object);
    if itself {
        object.unprotect_in(req, RecursionGuard::Json);
    }
    let wrote = if itself {
        held(req, buf, &answer, options, enc)
    } else {
        let wrote = encode(req, buf, &answer, options, enc);
        object.unprotect_in(req, RecursionGuard::Json);
        wrote
    };
    answer.give_up(req);
    wrote
}

/// `php_json_encode_serializable_enum`: a backed case is its value, and
/// a pure one the refusal php has no representation for.
fn enum_case(
    req: &mut Request,
    buf: &mut SmartStr,
    object: PhpObject,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    let backed = req
        .symbols()
        .class_decl(object.class())
        .is_some_and(|decl| decl.backing != EnumBacking::Pure);
    if !backed {
        enc.error_code = JSON_ERROR_NON_BACKED_ENUM;
        buf.append(req, b"0")?;
        return Ok(false);
    }
    let case = object.prop(req, b"value").unwrap_or_default();
    encode(req, buf, &case, options, enc)
}

/// `php_json_encode_array`: the brackets, the rows between them, the
/// recursion guard around the walk and the depth read after it.
///
/// Before any of it, `php_json_check_stack_limit`: a nesting that has
/// come down to the thread's stack limit is `JSON_ERROR_DEPTH`, `null`
/// in its place under `JSON_PARTIAL_OUTPUT_ON_ERROR`, whatever depth
/// the caller allowed. Under the small-stack regime a chain of
/// objects and a nest of arrays both reach the ini's 256K in both
/// engines.
///
/// ```php
/// class Node { public $next; }
/// $first = $node = new Node;
/// for ($i = 0; $i < 25000; $i++) { $node->next = new Node; $node = $node->next; }
/// var_dump(json_encode($first, 0, 500000), json_last_error(), json_last_error_msg());
/// while ($next = $first->next) { $first->next = $next->next; }
/// $a = []; for ($i = 0; $i < 25000; $i++) { $a = [$a]; }
/// $partial = json_encode($a, JSON_PARTIAL_OUTPUT_ON_ERROR, 500000);
/// var_dump(json_encode($a, 0, 500000), json_last_error_msg(), str_contains($partial, '[null]'));
/// ```
/// ```expect regime=small-stack
/// bool(false)
/// int(1)
/// string(28) "Maximum stack depth exceeded"
/// bool(false)
/// string(28) "Maximum stack depth exceeded"
/// bool(true)
/// ```
fn encode_array(
    req: &mut Request,
    buf: &mut SmartStr,
    value: &Value,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    if req.limits().stack_overflowed() {
        enc.error_code = JSON_ERROR_DEPTH;
        partial_null(req, buf, options)?;
        return Ok(false);
    }
    if let Some(object) = value.as_object() {
        return encode_object(req, buf, object, options, enc);
    }
    let Some(array) = value.as_array() else {
        return Ok(true);
    };
    let as_object = options & JSON_FORCE_OBJECT != 0 || !array.is_list();
    let rows = Rows::Array(array);
    guarded(req, buf, Guard::Table(array), rows, options, enc, as_object)
}

/// What a guard of `php_json_encode_array` stands on, php's
/// `recursion_rc`: an object, or the table it walks.
#[derive(Clone, Copy)]
enum Guard {
    Object(PhpObject),
    Table(PhpArray),
}

impl Guard {
    fn is_recursive(self) -> bool {
        match self {
            Guard::Object(object) => object.is_recursive(),
            Guard::Table(table) => table.is_recursive(),
        }
    }

    fn protect(self) {
        match self {
            Guard::Object(object) => object.protect(),
            Guard::Table(table) => table.protect(),
        }
    }

    fn unprotect(self) {
        match self {
            Guard::Object(object) => object.unprotect(),
            Guard::Table(table) => table.unprotect(),
        }
    }
}

/// `php_json_encode_array` over an object: the rows it contributes, in
/// php's order, its declared properties and then its dynamic ones with
/// the private and protected of the class skipped, which is
/// `ZSTR_VAL(key)[0] == '\0'` over the table. An object whose table is
/// not laid down and whose class answers no table of its own is walked
/// slot by slot and guarded itself, the table never built
/// ([`PhpObject::slots_without_table`]); any other is read through
/// `zend_get_properties_for(val, ZEND_PROP_PURPOSE_JSON)`, which
/// initialises a lazy object, guarded on the table it answers, or on
/// the object where its class has hooked properties, whose table is
/// built for the encoder.
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
///
/// A hooked row is read through its hook only once the walk reaches
/// it and the row is not stepped over (`Z/ext/json/json_encoder.c:259`),
/// so a protected or private property's hook is never called, and a
/// public one's is called after the rows before it are written:
///
/// ```php
/// class S { private $p { get { echo "p read\n"; return 1; } } public $q { get { echo "q read\n"; return 2; } } }
/// echo json_encode(new S), "\n";
/// class J implements JsonSerializable { function jsonSerialize(): mixed { echo "js\n"; return 1; } }
/// class O { public $a; public $b { get { echo "b\n"; return 2; } } function __construct() { $this->a = new J; } }
/// echo json_encode(new O), "\n";
/// ```
/// ```expect
/// q read
/// {"q":2}
/// js
/// b
/// {"a":1,"b":2}
/// ```
///
/// A hook that returned and then threw, from a local's destructor at
/// its end, ends the encoding with its value given up before the
/// `catch` (`json_encode_hooked_property_throwing_getter.phpt`):
///
/// ```php
/// class V { function __destruct() { echo "V::__destruct\n"; } }
/// class T { function __destruct() { throw new Exception("t"); } }
/// class C { public $h { get { $l = new T; return new V; } } }
/// try { json_encode(new C); } catch (Exception $e) { echo "caught\n"; }
/// echo "done\n";
/// ```
/// ```expect
/// V::__destruct
/// caught
/// done
/// ```
///
/// A call of its own, so what reading an object's table takes is off
/// the stack of a nest of arrays encoded level by level (#3159).
#[inline(never)]
fn encode_object(
    req: &mut Request,
    buf: &mut SmartStr,
    object: PhpObject,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    let hooked = object.has_hooked_properties(req);
    let slots = if hooked {
        None
    } else {
        object.slots_without_table(req)?
    };
    if let Some(slots) = slots {
        return guarded(
            req,
            buf,
            Guard::Object(object),
            Rows::Slots(slots),
            options,
            enc,
            true,
        );
    }
    let held = object.properties_for(req, PropertyPurpose::Json)?;
    let guard = if hooked {
        Guard::Object(object)
    } else {
        Guard::Table(held.table().array())
    };
    let wrote = guarded(req, buf, guard, Rows::Held(&held), options, enc, true);
    held.release(req)?;
    wrote
}

/// The rows [`write_rows`] walks: an array, an object's slots read
/// where they stand, or the table `zend_get_properties_for` answered,
/// held until its release.
enum Rows<'h> {
    Array(PhpArray),
    Slots(walk::Table),
    Held(&'h HeldProperties),
}

/// `php_json_encode_array`'s guard around its walk: `null` and the
/// recursion error where a walk is inside what `guard` stands on already.
fn guarded(
    req: &mut Request,
    buf: &mut SmartStr,
    guard: Guard,
    rows: Rows<'_>,
    options: i64,
    enc: &mut Encoder,
    as_object: bool,
) -> Wrote {
    if guard.is_recursive() {
        enc.error_code = JSON_ERROR_RECURSION;
        buf.append(req, b"null")?;
        return Ok(false);
    }
    guard.protect();
    let wrote = write_rows(req, buf, rows, options, enc, as_object);
    guard.unprotect();
    wrote
}

/// The body of `php_json_encode_array` inside its guard.
///
/// An object's rows are the table `zend_get_properties_for` hands the
/// encoder, held from the first row to the last
/// (`ZEND_HASH_FOREACH_KEY_VAL_IND`, `Z/ext/json/json_encoder.c:243`):
/// a `jsonSerialize` that writes the object's dynamic properties
/// separates the table from the rows being written, and one that frees
/// a row's value frees none of what is still to be written.
///
/// ```php
/// class J implements JsonSerializable { public $o; function jsonSerialize(): mixed { $this->o->b = 'new'; $this->o->c = null; $x = [str_repeat('q', 30), str_repeat('r', 30)]; return 1; } }
/// $o = new stdClass; $j = new J; $j->o = $o;
/// $o->a = $j; $o->b = str_repeat('b', 30); $o->c = [str_repeat('c', 30)];
/// echo json_encode($o), "\n";
/// ```
/// ```expect
/// {"a":1,"b":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","c":["cccccccccccccccccccccccccccccc"]}
/// ```
fn write_rows(
    req: &mut Request,
    buf: &mut SmartStr,
    rows: Rows<'_>,
    options: i64,
    enc: &mut Encoder,
    as_object: bool,
) -> Wrote {
    buf.append(req, if as_object { b"{" } else { b"[" })?;
    enc.depth += 1;
    let mut comma = false;
    let object = !matches!(rows, Rows::Array(_));
    // `ZEND_HASH_FOREACH_KEY_VAL_IND(myht, …)`
    // (`Z/ext/json/json_encoder.c:243`): over an array the walk takes no
    // owner of its own, since `php_json_encode_zval`'s `ZVAL_COPY` holds
    // it ([`held`], so a `jsonSerialize` that assigns over what holds the
    // array leaves it standing, and one that writes it separates it from
    // the rows being written; over an object's table the owner
    // `zend_get_properties_for` took.
    let mut body = |req: &mut Request, row: &mut walk::Row| -> walk::Step {
        // "Skip protected and private members."
        if object && matches!(row.key(), Key::Str(name) if name.as_bytes().first() == Some(&0)) {
            return Ok(ControlFlow::Continue(()));
        }
        let element = row.read(req)?.deref();
        let row = (row.key(), &element);
        if write_row(req, buf, row, options, enc, as_object, &mut comma)? {
            Ok(ControlFlow::Continue(()))
        } else {
            Ok(ControlFlow::Break(()))
        }
    };
    let walked = match rows {
        Rows::Array(array) => walk::foreach(req, array, Hold::None, &mut body)?,
        Rows::Slots(slots) => walk::foreach(req, slots, Hold::None, &mut body)?,
        Rows::Held(held) => walk::properties(req, held, Unwritten::Skipped, &mut body)?,
    };
    if walked.is_some() {
        return Ok(false);
    }
    if enc.depth > enc.max_depth {
        enc.error_code = JSON_ERROR_DEPTH;
        if options & JSON_PARTIAL_OUTPUT_ON_ERROR == 0 {
            return Ok(false);
        }
    }
    enc.depth -= 1;
    if comma {
        pretty_break(req, buf, options, enc)?;
    }
    buf.append(req, if as_object { b"}" } else { b"]" })?;
    Ok(true)
}

/// One row of `php_json_encode_array`'s walk: the comma before all but
/// the first, the key where the rows are written as an object, then the
/// value; `false` where the value failed to encode and the walk stops
/// there, without `JSON_PARTIAL_OUTPUT_ON_ERROR`.
fn write_row(
    req: &mut Request,
    buf: &mut SmartStr,
    (key, element): (&Key, &Value),
    options: i64,
    enc: &mut Encoder,
    as_object: bool,
    comma: &mut bool,
) -> Wrote {
    if *comma {
        buf.append(req, b",")?;
    }
    *comma = true;
    pretty_break(req, buf, options, enc)?;
    if as_object {
        if !write_key(req, buf, key, options, enc)? {
            return Ok(false);
        }
        buf.append(
            req,
            if options & JSON_PRETTY_PRINT != 0 {
                b": "
            } else {
                b":"
            },
        )?;
    }
    Ok(encode(req, buf, element, options, enc)? || options & JSON_PARTIAL_OUTPUT_ON_ERROR != 0)
}

/// One key of an object: the string escaped with `JSON_NUMERIC_CHECK`
/// taken off it, and an integer key quoted as php's
/// `smart_str_append_long` between two quotes. A call of its own, as
/// [`encode_scalar`] is. `false` where the key failed to escape once the
/// memory limit was met, and the walk stops there, since 8.5.12.
#[inline(never)]
fn write_key(
    req: &mut Request,
    buf: &mut SmartStr,
    key: &Key,
    options: i64,
    enc: &mut Encoder,
) -> Wrote {
    match key {
        Key::Str(name) => {
            let at = buf.as_bytes().len();
            let wrote = escape_string(
                req,
                buf,
                name.as_bytes(),
                options & !JSON_NUMERIC_CHECK,
                enc,
            )?;
            if !wrote && req.memory().has_oom() {
                return Ok(false);
            }
            if !wrote && options & JSON_PARTIAL_OUTPUT_ON_ERROR != 0 {
                buf.remove(at..usize::MAX);
                buf.append(req, b"\"\"")?;
            }
            Ok(true)
        }
        Key::Int(n) => {
            buf.append(req, format!("\"{n}\"").as_bytes())?;
            Ok(true)
        }
    }
}

/// `php_json_pretty_print_char` and `php_json_pretty_print_indent`: the
/// newline and the four spaces a level, and nothing without the option.
fn pretty_break(
    req: &mut Request,
    buf: &mut SmartStr,
    options: i64,
    enc: &Encoder,
) -> Result<(), Thrown> {
    if options & JSON_PRETTY_PRINT == 0 {
        return Ok(());
    }
    buf.append(req, b"\n")?;
    for _ in 0..enc.depth {
        buf.append(req, b"    ")?;
    }
    Ok(())
}
