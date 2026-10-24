//! `collator/`: the `Collator` class and the `collator_*` functions, its
//! methods php aliases to them, over ICU's `UCollator`. The sorts are
//! php's: `zend_hash_sort` over a copy of the array with the comparison
//! `collator_sort.c` picks by its `$flags`, `collator_convert.c`'s conversions
//! and `collator_is_numeric.c`'s number test, and `zend_sort` over the
//! sort keys.

use std::cmp::Ordering;
use std::ops::ControlFlow;

use php_ext::abi::{Instance, OrFalse, Ref, Refusals, This, True};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{known_method, lookup_class, new_object};
use php_ext::coerce;
use php_ext::decl::Name;
use php_ext::raising::{Thrown, fatal, throw_error, warning};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, SortAlgorithm, Value, zend_sort};

use crate::error::{self, Carrier, IntlError};
use crate::ffi::collator::{Collator as Coll, is_space};
use crate::ffi::{Code, c_string, to_utf16};

/// `Collator_object`: the error and the ICU collator, `None` until a
/// constructor opens one.
#[derive(Default)]
pub(crate) struct CollatorObject {
    error: IntlError,
    coll: Option<Coll>,
}

impl Carrier for CollatorObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `Collator_handlers`: no clone, `ucol_cloneBinary` having nowhere to
/// keep its buffer.
pub(crate) const HANDLERS: Refusals = Refusals::NONE.cloning();

/// A `zend_long` handed to ICU's `int` or enum, C's truncation.
fn c_int(n: i64) -> i32 {
    let b = n.to_le_bytes();
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// `f` over the object's open collator, `None` where it has none.
fn with<T>(req: &Request, object: PhpObject, f: impl FnOnce(&Coll) -> T) -> Option<T> {
    object
        .native::<CollatorObject>(req)
        .and_then(|o| o.coll.as_ref())
        .map(f)
}

/// `f` over the object's open collator, mutably.
fn with_mut<T>(req: &mut Request, object: PhpObject, f: impl FnOnce(&mut Coll) -> T) -> Option<T> {
    object
        .native_mut::<CollatorObject>(req)
        .and_then(|o| o.coll.as_mut())
        .map(f)
}

/// `COLLATOR_METHOD_FETCH_OBJECT`: the object's error reset, the
/// request's having been reset before the arguments
/// (`COLLATOR_METHOD_INIT_VARS`, `before_args`); whether it has a
/// collator.
fn fetch(req: &mut Request, object: PhpObject) -> bool {
    error::reset_object::<CollatorObject>(req, object);
    with(req, object, |_| ()).is_some()
}

/// An unconstructed collator: where `noted`, the code and message set
/// as `collator_compare` sets them, then php's `Error`.
fn unconstructed(req: &mut Request, object: PhpObject, noted: bool) -> Thrown {
    if noted {
        let code = error::object_code::<CollatorObject>(req, object);
        error::set_code(req, code);
        if let Err(thrown) =
            error::errors_set_custom_msg::<CollatorObject>(req, object, "Object not initialized")
        {
            return thrown;
        }
    }
    throw_error(req, "Error", "Object not initialized")
}

/// `fetch`, then [`unconstructed`] for an object with no collator.
fn fetched(req: &mut Request, object: PhpObject, noted: bool) -> Result<(), Thrown> {
    if fetch(req, object) {
        Ok(())
    } else {
        Err(unconstructed(req, object, noted))
    }
}

/// `COLLATOR_CHECK_STATUS`: the code on the object and the request, and
/// where it is a failure the message and `false`.
fn check(
    req: &mut Request,
    object: PhpObject,
    status: Code,
    message: &str,
) -> Result<bool, Thrown> {
    error::errors_set_code::<CollatorObject>(req, object, status);
    if status.is_failure() {
        error::errors_set_custom_msg::<CollatorObject>(req, object, message)?;
        return Ok(false);
    }
    Ok(true)
}

/// `collator_ctor`: the collator opened for the locale, the default for
/// the empty string; `Ok(false)` where it failed and nothing was thrown.
fn ctor(req: &mut Request, object: PhpObject, locale: &[u8]) -> Result<bool, Thrown> {
    if locale.len() > 156 {
        error::set(
            req,
            Code::ILLEGAL_ARGUMENT,
            "Locale string too long, should be no longer than 156 characters",
        )?;
        return Ok(false);
    }
    if fetch(req, object) {
        return Err(throw_error(
            req,
            "Error",
            "Collator object is already constructed",
        ));
    }
    let locale = if locale.is_empty() {
        crate::default_locale(req)
    } else {
        locale.to_vec()
    };
    let mut status = Code::ZERO;
    let coll = Coll::open(&c_string(&locale), &mut status);
    if let Some(o) = object.native_mut::<CollatorObject>(req) {
        o.coll = coll;
        o.error.code = status;
    }
    error::set_code(req, status);
    if status.is_failure() {
        error::errors_set_custom_msg::<CollatorObject>(req, object, "unable to open ICU collator")?;
        return Ok(false);
    }
    Ok(true)
}

/// `collator_create`: a new `Collator`, `null` where it failed to open.
fn create(
    req: &mut Request,
    locale: PhpString,
) -> Result<Option<Owner<Instance<Collator>>>, Thrown> {
    let Some(class) = lookup_class(req, b"Collator")? else {
        return Err(fatal(req, "InternalError: Collator is not linked"));
    };
    let object = new_object(req, class)?;
    match ctor(req, *object, locale.as_bytes()) {
        Ok(true) => Ok(Some(object.into_instance())),
        answer => {
            object.give_up(req);
            answer.map(|_| None)
        }
    }
}

/// `collator_compare`: `ucol_strcoll` of the two strings in UTF-16.
fn compare(
    req: &mut Request,
    object: PhpObject,
    a: PhpString,
    b: PhpString,
) -> Result<OrFalse<i64>, Thrown> {
    fetched(req, object, true)?;
    let mut both = Vec::with_capacity(2);
    for (text, which) in [(&a, "first"), (&b, "second")] {
        match to_utf16(text.as_bytes()) {
            Ok(utf16) => both.push(utf16),
            Err(status) => {
                let message = format!("Error converting {which} argument to UTF-16");
                check(req, object, status, &message)?;
                return Ok(OrFalse::False);
            }
        }
    }
    let order = with(req, object, |c| c.strcoll(&both[0], &both[1])).unwrap_or(0);
    Ok(OrFalse::Value(i64::from(order)))
}

/// `collator_get_attribute`.
fn get_attribute(
    req: &mut Request,
    object: PhpObject,
    attribute: i64,
) -> Result<OrFalse<i64>, Thrown> {
    fetched(req, object, false)?;
    let mut status = Code::ZERO;
    let value = with(req, object, |c| c.attribute(c_int(attribute), &mut status)).unwrap_or(0);
    if !check(req, object, status, "Error getting attribute value")? {
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(i64::from(value)))
}

/// `collator_set_attribute`.
fn set_attribute(
    req: &mut Request,
    object: PhpObject,
    attribute: i64,
    value: i64,
) -> Result<bool, Thrown> {
    fetched(req, object, false)?;
    let mut status = Code::ZERO;
    with_mut(req, object, |c| {
        c.set_attribute(c_int(attribute), c_int(value), &mut status);
    });
    check(req, object, status, "Error setting attribute value")
}

/// `collator_get_strength`.
fn get_strength(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    fetched(req, object, false)?;
    Ok(with(req, object, Coll::strength).map_or(0, i64::from))
}

/// `collator_set_strength`.
fn set_strength(req: &mut Request, object: PhpObject, strength: i64) -> Result<True, Thrown> {
    fetched(req, object, false)?;
    with_mut(req, object, |c| c.set_strength(c_int(strength)));
    Ok(True)
}

/// `collator_get_locale`: the actual or the valid locale.
fn get_locale(
    req: &mut Request,
    object: PhpObject,
    kind: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    fetched(req, object, true)?;
    let mut status = Code::ZERO;
    let locale = with(req, object, |c| c.locale(c_int(kind), &mut status)).flatten();
    if !check(req, object, status, "Error getting locale by type")? {
        return Ok(OrFalse::False);
    }
    PhpString::new(req, locale.unwrap_or_default().as_bytes()).map(OrFalse::Value)
}

/// `collator_get_error_code`: the object's code, not reset.
fn get_error_code(req: &mut Request, object: PhpObject) -> OrFalse<i64> {
    OrFalse::Value(i64::from(
        error::object_code::<CollatorObject>(req, object).0,
    ))
}

/// `collator_get_error_message`: the object's message, not reset.
fn get_error_message(
    req: &mut Request,
    object: PhpObject,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let message = error::object_message::<CollatorObject>(req, object);
    PhpString::new(req, &message).map(OrFalse::Value)
}

/// `collator_get_sort_key`: `ucol_getSortKey` without its terminating NUL.
fn get_sort_key(
    req: &mut Request,
    object: PhpObject,
    text: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    fetched(req, object, true)?;
    let utf16 = match to_utf16(text.as_bytes()) {
        Ok(utf16) => utf16,
        Err(status) => {
            check(
                req,
                object,
                status,
                "Error converting first argument to UTF-16",
            )?;
            return Ok(OrFalse::False);
        }
    };
    let key = with(req, object, |c| c.sort_key(&utf16)).unwrap_or_default();
    let Some((_, key)) = key.split_last() else {
        return Ok(OrFalse::False);
    };
    PhpString::new(req, key).map(OrFalse::Value)
}

/// `collator_u_strtol` over base ten: the value, where it stopped, and
/// whether it overflowed (`ERANGE`).
fn u_strtol(s: &[u16]) -> (i64, usize, bool) {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let neg = at(i) == u16::from(b'-');
    if neg || at(i) == u16::from(b'+') {
        i += 1;
    }
    let cutoff: u64 = if neg {
        1 << 63
    } else {
        i64::MAX.unsigned_abs()
    };
    let (cutlim, cutoff) = (cutoff % 10, cutoff / 10);
    let (mut acc, mut any) = (0u64, 0i8);
    while let Some(d) = char::from_u32(u32::from(at(i))).and_then(|c| c.to_digit(10)) {
        let d = u64::from(d);
        if any < 0 || acc > cutoff || (acc == cutoff && d > cutlim) {
            any = -1;
        } else {
            any = 1;
            acc = acc * 10 + d;
        }
        i += 1;
    }
    let end = if any != 0 { i } else { 0 };
    if any < 0 {
        return (if neg { i64::MIN } else { i64::MAX }, end, true);
    }
    let value = i64::try_from(acc).unwrap_or(i64::MIN);
    (if neg { value.wrapping_neg() } else { value }, end, false)
}

/// `collator_u_strtod`: the value and where it stopped, the start for
/// no digits.
fn u_strtod(s: &[u16]) -> (f64, usize) {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let digit = |c: u16| (u16::from(b'0')..=u16::from(b'9')).contains(&c);
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let start = i;
    if at(i) == u16::from(b'-') || at(i) == u16::from(b'+') {
        i += 1;
    }
    let mut any = false;
    while digit(at(i)) {
        any = true;
        i += 1;
    }
    if at(i) == u16::from(b'.') {
        i += 1;
        while digit(at(i)) {
            any = true;
            i += 1;
        }
    }
    if (at(i) == u16::from(b'e') || at(i) == u16::from(b'E')) && any {
        let e = i;
        i += 1;
        if at(i) == u16::from(b'-') || at(i) == u16::from(b'+') {
            i += 1;
        }
        let mut any_exp = false;
        while digit(at(i)) {
            any_exp = true;
            i += 1;
        }
        if !any_exp {
            i = e;
        }
    }
    if !any {
        return (0.0, 0);
    }
    let text: String = s[start..i]
        .iter()
        .filter_map(|&c| char::from_u32(u32::from(c)))
        .collect();
    let text = text.strip_prefix('+').unwrap_or(&text);
    let text = if text.ends_with('.') {
        format!("{text}0")
    } else {
        text.to_owned()
    };
    (
        text.replace(".e", ".0e")
            .replace(".E", ".0E")
            .parse()
            .unwrap_or(0.0),
        i,
    )
}

/// `collator_is_numeric` without errors allowed: the number a UTF-16
/// string spells whole, surrounding white space allowed.
fn numeric(s: &[u16]) -> Option<Value> {
    if s.is_empty() {
        return None;
    }
    let skip = |mut i: usize| {
        while s.get(i).is_some_and(|&c| is_space(c)) {
            i += 1;
        }
        i
    };
    let (long, end, overflow) = u_strtol(s);
    if !overflow {
        if skip(end) == s.len() {
            return Some(Value::int(long));
        }
        let first = s.first().copied().unwrap_or(0);
        if end == 0 && first != 0 && first != u16::from(b'.') && first != u16::from(b'-') {
            return None;
        }
    }
    let (double, end) = u_strtod(s);
    if !(double == 0.0 && end == 0) && skip(end) == s.len() && double.is_finite() {
        return Some(Value::float(double));
    }
    None
}

/// A sort operand after `collator_convert_object_to_string`: a string's
/// UTF-8 text, or the value itself.
enum Operand {
    Text(Vec<u8>),
    Other(Value),
}

/// The text of a string, of an object through its `__toString` (empty
/// with php's warning where it is no UTF-8), or the value as it is.
fn operand(req: &mut Request, value: &Value) -> Result<Operand, Thrown> {
    if let Some(bytes) = value.as_str_bytes() {
        return Ok(Operand::Text(bytes.to_vec()));
    }
    let Some(object) = value.as_object() else {
        return Ok(Operand::Other(*value));
    };
    let name = Name(b"__tostring".as_slice().into());
    if known_method(req, object.class(), &name, Some(object)).is_none() {
        return Ok(Operand::Other(*value));
    }
    let bytes = coerce::with_string(req, value, <[u8]>::to_vec)?;
    if to_utf16(&bytes).is_err() {
        warning(
            req,
            "Error casting object to string in collator_convert_object_to_string()",
        );
        return Ok(Operand::Text(Vec::new()));
    }
    Ok(Operand::Text(bytes))
}

/// UTF-16 of text already held to be UTF-8.
fn utf16(text: &[u8]) -> Vec<u16> {
    to_utf16(text).unwrap_or_default()
}

/// `ucol_strcoll` with the object's collator as an ordering.
fn strcoll(req: &Request, object: PhpObject, a: &[u16], b: &[u16]) -> Ordering {
    with(req, object, |c| c.strcoll(a, b)).unwrap_or(0).cmp(&0)
}

/// `collator_normalize_sort_argument`: a string as its number where it
/// is numeric, else as itself; a new value the caller releases.
fn normalized(req: &mut Request, operand: Operand) -> Result<Owner<Value>, Thrown> {
    match operand {
        Operand::Text(text) => match numeric(&utf16(&text)) {
            Some(number) => Ok(Owner::take(req, number)),
            None => PhpString::new(req, &text).map(Owner::into_value),
        },
        Operand::Other(value) => Ok(Owner::take(req, value)),
    }
}

/// `collator_regular_compare_function`: two strings, one of them not
/// numeric, by the collator; anything else as php compares it, numeric
/// strings as their numbers.
fn regular(req: &mut Request, object: PhpObject, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
    let a = operand(req, a)?;
    let b = operand(req, b)?;
    if let (Operand::Text(x), Operand::Text(y)) = (&a, &b) {
        let (x, y) = (utf16(x), utf16(y));
        let numbers = numeric(&x).and_then(|nx| numeric(&y).map(|ny| (nx, ny)));
        let Some((nx, ny)) = numbers else {
            return Ok(strcoll(req, object, &x, &y));
        };
        return coerce::compare(req, &nx, &ny);
    }
    let a = normalized(req, a)?;
    let b = normalized(req, b)?;
    let order = coerce::compare(req, &a, &b);
    a.give_up(req);
    b.give_up(req);
    order
}

/// `collator_zval_to_string` in UTF-16: a string as it is, anything
/// else through `zval_get_string`.
fn string_of(req: &mut Request, value: &Value) -> Result<Vec<u16>, Thrown> {
    if let Some(bytes) = value.as_str_bytes() {
        return Ok(utf16(bytes));
    }
    let converted = coerce::with_string(req, value, to_utf16)?;
    converted.or_else(|_| {
        warning(
            req,
            "Error casting object to string in collator_convert_zstr_utf8_to_utf16()",
        );
        Ok(Vec::new())
    })
}

/// `collator_numeric_compare_function`: both read as doubles, a string
/// as its number or zero.
fn numerically(req: &mut Request, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
    let mut double = |value: &Value| -> Result<f64, Thrown> {
        match value.as_str_bytes() {
            Some(bytes) => match numeric(&utf16(bytes)) {
                Some(number) => coerce::to_double(req, &number),
                None => Ok(0.0),
            },
            None => coerce::to_double(req, value),
        }
    };
    let (x, y) = (double(a)?, double(b)?);
    Ok(x.partial_cmp(&y).unwrap_or(Ordering::Greater))
}

/// `collator_sort_internal`: a copy of the array sorted by the
/// comparison `$flags` picks, then put in the variable's place.
fn sort(
    req: &mut Request,
    object: PhpObject,
    array: &Ref<PhpArray>,
    flags: i64,
    renumber: bool,
) -> Result<bool, Thrown> {
    if !fetch(req, object) {
        return Err(throw_error(req, "Error", "Object not initialized"));
    }
    let Some(given) = array.get() else {
        return Ok(true);
    };
    let invalid = walk::rows(req, *given)
        .find_map(|(_, value)| value.as_str_bytes().and_then(|b| to_utf16(b).err()));
    if let Some(status) = invalid {
        check(
            req,
            object,
            status,
            "Error converting hash from UTF-8 to UTF-16",
        )?;
        return Ok(false);
    }
    let sorted = given.copy(req)?;
    // `zend_hash_sort` over a copy the native holds alone, which no
    // comparison here writes, so the view sorts the owner's table.
    let mut table = *sorted;
    let answer = table.sort_by(
        req,
        SortAlgorithm::Hash,
        renumber,
        |req, (_, a), (_, b)| match flags {
            2 => numerically(req, a, b),
            1 => {
                let (x, y) = (string_of(req, a)?, string_of(req, b)?);
                Ok(strcoll(req, object, &x, &y))
            }
            _ => regular(req, object, a, b),
        },
    );
    array.set(req, sorted)?;
    answer.map(|()| true)
}

/// `collator_sort_with_sort_keys`: the values ordered by their sort keys
/// with `zend_sort`, a non-string's key the empty string's, renumbered.
fn sort_with_sort_keys(
    req: &mut Request,
    object: PhpObject,
    array: &Ref<PhpArray>,
) -> Result<bool, Thrown> {
    fetched(req, object, true)?;
    let Some(given) = array.get().copied() else {
        return Ok(true);
    };
    if given.is_empty() {
        return Ok(true);
    }
    let mut keyed = Vec::with_capacity(given.len());
    // `ZEND_HASH_FOREACH_VAL(hash, hashData)` (`Z/ext/intl/collator/collator_sort.c:397`),
    // no owner of the table taken: the by-reference argument holds it while
    // a failure's warning reaches the program, after which the native
    // returns and reads no row it kept.
    let walked = walk::foreach(req, given, Hold::None, |req, row| -> walk::Step<bool> {
        let value = *row.value();
        let text = match value.as_str_bytes().map(to_utf16) {
            Some(Err(status)) => {
                check(req, object, status, "Sort with sort keys failed")?;
                return Ok(ControlFlow::Break(false));
            }
            Some(Ok(text)) => text,
            None => Vec::new(),
        };
        let mut key = with(req, object, |c| c.sort_key(&text)).unwrap_or_default();
        key.truncate(key.iter().position(|&b| b == 0).unwrap_or(key.len()));
        keyed.push((key, value));
        Ok(ControlFlow::Continue(()))
    })?;
    if let Some(answer) = walked {
        return Ok(answer);
    }
    zend_sort(&mut keyed, &mut |a, b| a.0.cmp(&b.0));
    let mut sorted = PhpArray::new(req)?;
    for (_, value) in keyed {
        sorted.append(req, Owner::take(req, value))?;
    }
    array.set(req, sorted)?;
    Ok(true)
}

/// The receiver of a procedural function, `Collator $object`.
type Object = Instance<Collator>;

/// `collator_create(string $locale): ?Collator`.
///
/// ```php
/// var_dump(get_class(collator_create('en_US')), collator_create('')->getLocale(ULOC_VALID_LOCALE) !== '', collator_create(str_repeat('x', 157)), intl_get_error_message());
/// ```
/// ```expect
/// string(8) "Collator"
/// bool(true)
/// NULL
/// string(108) "collator_create(): Locale string too long, should be no longer than 156 characters: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn collator_create(
    req: &mut Request,
    locale: PhpString,
) -> Result<Option<Owner<Instance<Collator>>>, Thrown> {
    create(req, locale)
}

/// `collator_compare(Collator $object, string $string1, string $string2): int|false`.
///
/// ```php
/// $c = collator_create('en_US');
/// var_dump(collator_compare($c, 'a', 'B'), collator_compare($c, 'b', 'a'), collator_compare($c, "\xff", 'a'), intl_get_error_message());
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// bool(false)
/// string(83) "collator_compare(): Error converting first argument to UTF-16: U_INVALID_CHAR_FOUND"
/// ```
#[php_function(before_args = error::reset)]
fn collator_compare(
    req: &mut Request,
    object: Object,
    string1: PhpString,
    string2: PhpString,
) -> Result<OrFalse<i64>, Thrown> {
    compare(req, object.object(), string1, string2)
}

/// `collator_get_attribute(Collator $object, int $attribute): int|false`.
///
/// ```php
/// $c = collator_create('en_US');
/// var_dump(collator_get_attribute($c, Collator::NUMERIC_COLLATION), collator_get_attribute($c, 99), intl_get_error_message());
/// ```
/// ```expect
/// int(16)
/// bool(false)
/// string(81) "collator_get_attribute(): Error getting attribute value: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn collator_get_attribute(
    req: &mut Request,
    object: Object,
    attribute: i64,
) -> Result<OrFalse<i64>, Thrown> {
    get_attribute(req, object.object(), attribute)
}

/// `collator_set_attribute(Collator $object, int $attribute, int $value): bool`.
///
/// ```php
/// $c = collator_create('en_US');
/// var_dump(collator_set_attribute($c, Collator::NUMERIC_COLLATION, Collator::ON), collator_compare($c, 'a10', 'a9'), collator_set_attribute($c, 99, 1));
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// bool(false)
/// ```
#[php_function(before_args = error::reset)]
fn collator_set_attribute(
    req: &mut Request,
    object: Object,
    attribute: i64,
    value: i64,
) -> Result<bool, Thrown> {
    set_attribute(req, object.object(), attribute, value)
}

/// `collator_get_strength(Collator $object): int`.
///
/// ```php
/// var_dump(collator_get_strength(collator_create('en_US')));
/// ```
/// ```expect
/// int(2)
/// ```
#[php_function(before_args = error::reset)]
fn collator_get_strength(req: &mut Request, object: Object) -> Result<i64, Thrown> {
    get_strength(req, object.object())
}

/// `collator_set_strength(Collator $object, int $strength): true`.
///
/// ```php
/// $c = collator_create('en_US');
/// var_dump(collator_set_strength($c, Collator::PRIMARY), collator_get_strength($c), collator_compare($c, 'a', 'A'));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn collator_set_strength(req: &mut Request, object: Object, strength: i64) -> Result<True, Thrown> {
    set_strength(req, object.object(), strength)
}

/// `collator_sort(Collator $object, array &$array, int $flags = Collator::SORT_REGULAR): bool`.
///
/// ```php
/// $c = collator_create('en_US');
/// $a = ['b', 'A', '10', '9', 'a', 1.5]; var_dump(collator_sort($c, $a)); echo json_encode($a), "\n";
/// $a = ['10', '9', '1e1', 'x']; collator_sort($c, $a, Collator::SORT_STRING); echo json_encode($a), "\n";
/// $a = ['10', '9', '1e1', 'x']; collator_sort($c, $a, Collator::SORT_NUMERIC); echo json_encode($a), "\n";
/// $a = ["\xff", 'a']; var_dump(collator_sort($c, $a), intl_get_error_message());
/// ```
/// ```expect
/// bool(true)
/// [1.5,"9","10","a","A","b"]
/// ["10","1e1","9","x"]
/// ["x","9","10","1e1"]
/// bool(false)
/// string(81) "collator_sort(): Error converting hash from UTF-8 to UTF-16: U_INVALID_CHAR_FOUND"
/// ```
#[php_function(before_args = error::reset)]
fn collator_sort(
    req: &mut Request,
    object: Object,
    array: Ref<PhpArray>,
    #[php(default = Collator::SORT_REGULAR)] flags: i64,
) -> Result<bool, Thrown> {
    sort(req, object.object(), &array, flags, true)
}

/// `collator_sort_with_sort_keys(Collator $object, array &$array): bool`.
///
/// ```php
/// $c = collator_create('en_US');
/// $a = ['x' => 'b', 'y' => 'A', 'z' => 'a', 3]; var_dump(collator_sort_with_sort_keys($c, $a)); echo json_encode($a), "\n";
/// ```
/// ```expect
/// bool(true)
/// [3,"a","A","b"]
/// ```
#[php_function(before_args = error::reset)]
fn collator_sort_with_sort_keys(
    req: &mut Request,
    object: Object,
    array: Ref<PhpArray>,
) -> Result<bool, Thrown> {
    sort_with_sort_keys(req, object.object(), &array)
}

/// `collator_asort(Collator $object, array &$array, int $flags = Collator::SORT_REGULAR): bool`.
///
/// ```php
/// $c = collator_create('en_US');
/// $a = ['x' => 'b', 'y' => 'A', 'z' => 'a']; var_dump(collator_asort($c, $a)); echo json_encode($a), "\n";
/// ```
/// ```expect
/// bool(true)
/// {"z":"a","y":"A","x":"b"}
/// ```
#[php_function(before_args = error::reset)]
fn collator_asort(
    req: &mut Request,
    object: Object,
    array: Ref<PhpArray>,
    #[php(default = Collator::SORT_REGULAR)] flags: i64,
) -> Result<bool, Thrown> {
    sort(req, object.object(), &array, flags, false)
}

/// `collator_get_locale(Collator $object, int $type): string|false`.
///
/// ```php
/// $c = collator_create('de_DE_PHONEBOOK');
/// var_dump(collator_get_locale($c, ULOC_VALID_LOCALE), collator_get_locale($c, 5), intl_get_error_message());
/// ```
/// ```expect
/// string(2) "de"
/// bool(false)
/// string(77) "collator_get_locale(): Error getting locale by type: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn collator_get_locale(
    req: &mut Request,
    object: Object,
    r#type: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_locale(req, object.object(), r#type)
}

/// `collator_get_error_code(Collator $object): int|false`.
///
/// ```php
/// $c = collator_create('xx'); var_dump(collator_get_error_code($c), collator_get_error_message($c));
/// ```
/// ```expect
/// int(-127)
/// string(23) "U_USING_DEFAULT_WARNING"
/// ```
#[php_function(before_args = error::reset)]
fn collator_get_error_code(req: &mut Request, object: Object) -> OrFalse<i64> {
    get_error_code(req, object.object())
}

/// `collator_get_error_message(Collator $object): string|false`.
///
/// ```php
/// $c = collator_create('en'); collator_get_attribute($c, 99); var_dump(collator_get_error_message($c));
/// ```
/// ```expect
/// string(81) "collator_get_attribute(): Error getting attribute value: U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn collator_get_error_message(
    req: &mut Request,
    object: Object,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_error_message(req, object.object())
}

/// `collator_get_sort_key(Collator $object, string $string): string|false`.
///
/// ```php
/// var_dump(bin2hex(collator_get_sort_key(collator_create('en_US'), 'ab')));
/// ```
/// ```expect
/// string(12) "2b2d01060106"
/// ```
#[php_function(before_args = error::reset)]
fn collator_get_sort_key(
    req: &mut Request,
    object: Object,
    string: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    get_sort_key(req, object.object(), string)
}

/// The `collator_*` block of `php_intl.stub.php`, in its order.
pub(crate) fn functions(b: php_ext::abi::ExtensionBuilder) -> php_ext::abi::ExtensionBuilder {
    b.functions(vec![
        collator_create::declared(),
        collator_compare::declared(),
        collator_get_attribute::declared(),
        collator_set_attribute::declared(),
        collator_get_strength::declared(),
        collator_set_strength::declared(),
        collator_sort::declared(),
        collator_sort_with_sort_keys::declared(),
        collator_asort::declared(),
        collator_get_locale::declared(),
        collator_get_error_code::declared(),
        collator_get_error_message::declared(),
        collator_get_sort_key::declared(),
    ])
}

/// `class Collator`, not serializable nor cloneable: its constructor,
/// which throws where `collator_create` answers `null`, and its methods,
/// each the `collator_*` function php aliases it to.
///
/// ```php
/// $c = new Collator('en_US');
/// var_dump(Collator::create('fr') instanceof Collator, $c->compare('a', 'b'), $c->getAttribute(Collator::STRENGTH), $c->setAttribute(Collator::STRENGTH, Collator::PRIMARY), $c->getStrength(), $c->setStrength(Collator::TERTIARY));
/// $a = ['b', 'a']; $c->sort($a); $k = ['x' => 'b', 'y' => 'a']; $c->asort($k); $w = ['b', 'a']; $c->sortWithSortKeys($w); echo json_encode([$a, $k, $w]), "\n";
/// var_dump($c->getLocale(ULOC_ACTUAL_LOCALE), $c->getErrorCode(), $c->getErrorMessage(), strlen($c->getSortKey('a')));
/// try { new Collator(str_repeat('x', 157)); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { $c->__construct('en'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone $c; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($c); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(-1)
/// int(2)
/// bool(true)
/// int(0)
/// bool(true)
/// [["a","b"],{"y":"a","x":"b"},["a","b"]]
/// string(4) "root"
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// int(5)
/// Collator::__construct(): Locale string too long, should be no longer than 156 characters
/// Collator object is already constructed
/// Trying to clone an uncloneable object of class Collator
/// Serialization of 'Collator' is not allowed
/// ```
#[php_class(not_serializable, size = 80, payload = CollatorObject, handlers = crate::collator::HANDLERS)]
pub(crate) struct Collator;

#[php_class]
#[allow(non_snake_case)]
impl Collator {
    const DEFAULT_VALUE: i64 = -1;
    const PRIMARY: i64 = 0;
    const SECONDARY: i64 = 1;
    const TERTIARY: i64 = 2;
    const DEFAULT_STRENGTH: i64 = 2;
    const QUATERNARY: i64 = 3;
    const IDENTICAL: i64 = 15;
    const OFF: i64 = 16;
    const ON: i64 = 17;
    const SHIFTED: i64 = 20;
    const NON_IGNORABLE: i64 = 21;
    const LOWER_FIRST: i64 = 24;
    const UPPER_FIRST: i64 = 25;
    const FRENCH_COLLATION: i64 = 0;
    const ALTERNATE_HANDLING: i64 = 1;
    const CASE_FIRST: i64 = 2;
    const CASE_LEVEL: i64 = 3;
    const NORMALIZATION_MODE: i64 = 4;
    const STRENGTH: i64 = 5;
    const HIRAGANA_QUATERNARY_MODE: i64 = 6;
    const NUMERIC_COLLATION: i64 = 7;
    const SORT_REGULAR: i64 = 0;
    const SORT_STRING: i64 = 1;
    const SORT_NUMERIC: i64 = 2;

    #[php(before_args = error::reset)]
    fn __construct(req: &mut Request, this: This<Self>, locale: PhpString) -> Result<(), Thrown> {
        let object = this.object();
        error::constructing(req, |req| ctor(req, object, locale.as_bytes())).map(|_| ())
    }

    #[php(tentative, before_args = error::reset)]
    fn create(
        req: &mut Request,
        locale: PhpString,
    ) -> Result<Option<Owner<Instance<Collator>>>, Thrown> {
        create(req, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn compare(
        req: &mut Request,
        this: This<Self>,
        string1: PhpString,
        string2: PhpString,
    ) -> Result<OrFalse<i64>, Thrown> {
        compare(req, this.object(), string1, string2)
    }

    #[php(tentative, before_args = error::reset)]
    fn sort(
        req: &mut Request,
        this: This<Self>,
        array: Ref<PhpArray>,
        #[php(default = Collator::SORT_REGULAR)] flags: i64,
    ) -> Result<bool, Thrown> {
        sort(req, this.object(), &array, flags, true)
    }

    #[php(tentative, before_args = error::reset)]
    fn sortWithSortKeys(
        req: &mut Request,
        this: This<Self>,
        array: Ref<PhpArray>,
    ) -> Result<bool, Thrown> {
        sort_with_sort_keys(req, this.object(), &array)
    }

    #[php(tentative, before_args = error::reset)]
    fn asort(
        req: &mut Request,
        this: This<Self>,
        array: Ref<PhpArray>,
        #[php(default = Collator::SORT_REGULAR)] flags: i64,
    ) -> Result<bool, Thrown> {
        sort(req, this.object(), &array, flags, false)
    }

    #[php(tentative, before_args = error::reset)]
    fn getAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
    ) -> Result<OrFalse<i64>, Thrown> {
        get_attribute(req, this.object(), attribute)
    }

    #[php(tentative, before_args = error::reset)]
    fn setAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
        value: i64,
    ) -> Result<bool, Thrown> {
        set_attribute(req, this.object(), attribute, value)
    }

    #[php(tentative, before_args = error::reset)]
    fn getStrength(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        get_strength(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn setStrength(req: &mut Request, this: This<Self>, strength: i64) -> Result<True, Thrown> {
        set_strength(req, this.object(), strength)
    }

    #[php(tentative, before_args = error::reset)]
    fn getLocale(
        req: &mut Request,
        this: This<Self>,
        r#type: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_locale(req, this.object(), r#type)
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> OrFalse<i64> {
        get_error_code(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorMessage(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_error_message(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getSortKey(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        get_sort_key(req, this.object(), string)
    }
}
