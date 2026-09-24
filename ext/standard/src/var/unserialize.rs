//! `unserialize()`, php's `php_unserialize_with_options`
//! (`Z/ext/standard/var.c:1382`) over the scanner of
//! `var_unserializer.re`, copied token for token: the var hash of every
//! slot a value was written into, in the order php pushes them, which
//! `R:n;` turns into a reference and `r:n;` reads an object back from;
//! the delayed `__unserialize()` and `__wakeup()` calls, made in the
//! order their objects were finished once the whole string is read; and
//! php's warnings, with the byte offset where the scanner stood.

use php_ext::call::{call, call_method, class_constant, lookup_class, new_object};
use php_ext::coerce::{callable_checked, value_name};
use php_ext::raising::{Level, Thrown, deprecated, docref, throw, warning};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpRef, PhpString, Value, ValueRef};

use super::{INCOMPLETE_CLASS, MAGIC_MEMBER, has_method, is_enum_class};
use crate::argument_count_between;

/// `HT_MAX_SIZE` on a 64-bit build.
const HT_MAX_SIZE: i64 = 0x4000_0000;

/// Where one value of the string goes: the value `unserialize()`
/// returns, an element of an array being filled, or a property of an
/// object being filled, by the key the string carries.
#[derive(Clone)]
enum Slot {
    Root,
    Element(PhpArray, Key),
    Property(PhpObject, PhpString),
}

/// A call php delays until the whole string is read (`var_destroy`).
enum Delayed {
    Wakeup(PhpObject),
    Unserialize(PhpObject, PhpArray),
}

/// `php_unserialize_data`: the slots in push order, each with the value
/// last written into it, `None` once a failure has forbidden it; the
/// delayed calls; the options.
struct State<'a> {
    buf: &'a [u8],
    p: usize,
    slots: Vec<Option<(Slot, Value)>>,
    root: Value,
    delayed: Vec<Delayed>,
    allowed: Option<Vec<Vec<u8>>>,
    max_depth: i64,
    depth: i64,
}

/// `unserialize(string $data, array $options = []): mixed`, php's
/// `PHP_FUNCTION(unserialize)` (`Z/ext/standard/var.c:1510`).
///
/// ```php
/// var_dump(unserialize('a:3:{i:0;i:1;s:1:"k";d:1.5;i:1;a:1:{i:0;b:1;}}'), unserialize('N;'), unserialize('s:3:"a"b";'), unserialize('i:-7;'), unserialize('d:INF;'), unserialize(serialize(0.1)));
/// var_dump(unserialize(''), unserialize('b:0;'));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(1)
///   ["k"]=>
///   float(1.5)
///   [1]=>
///   array(1) {
///     [0]=>
///     bool(true)
///   }
/// }
/// NULL
/// string(3) "a"b"
/// int(-7)
/// float(INF)
/// float(0.1)
/// bool(false)
/// bool(false)
/// ```
///
/// Malformed data is `false` and php's warning with the offset the
/// scanner stood at; data after a whole value is warned about and the
/// value kept.
///
/// ```php
/// var_dump(unserialize('a:2:{i:0;i:1;}'), unserialize('i:1'), unserialize('s:5:"abc";'), unserialize('x'), unserialize('i:1;junk'), unserialize('a:1:{i:0;i:1;'), unserialize('i:99999999999999999999;'));
/// ```
/// ```expect
/// Warning: unserialize(): Unexpected end of serialized data in %s on line 2
///
/// Warning: unserialize(): Error at offset 13 of 14 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 3 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 10 of 10 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 1 bytes in %s on line 2
///
/// Warning: unserialize(): Extra data starting at offset 4 of 8 bytes in %s on line 2
///
/// Warning: unserialize(): Error at offset 13 of 13 bytes in %s on line 2
///
/// Warning: unserialize(): Numerical result out of range in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// int(1)
/// bool(false)
/// int(9223372036854775807)
/// ```
///
/// An object is made without its constructor and its properties
/// written as the string names them, mangled names placing protected
/// and private ones; `r:` is the same object again and `R:` a
/// reference to an earlier slot.
///
/// ```php
/// class P { public $a = 1; protected $b = 2; private $c = 3; function __construct() { echo "ctor\n"; } }
/// $p = unserialize(serialize(new P)); var_dump($p);
/// $o = unserialize('a:2:{i:0;O:8:"stdClass":1:{s:1:"x";i:1;}i:1;r:2;}'); var_dump($o[0] === $o[1]);
/// $r = unserialize('a:2:{i:0;i:5;i:1;R:2;}'); $r[1] = 9; var_dump($r);
/// ```
/// ```expect
/// ctor
/// object(P)#1 (3) {
///   ["a"]=>
///   int(1)
///   ["b":protected]=>
///   int(2)
///   ["c":"P":private]=>
///   int(3)
/// }
/// bool(true)
/// array(2) {
///   [0]=>
///   &int(9)
///   [1]=>
///   &int(9)
/// }
/// ```
///
/// `__unserialize()` is handed the data and `__wakeup()` is called with
/// none, each after the whole string is read, inner objects first; a
/// class that is not there, or not allowed, is `__PHP_Incomplete_Class`.
///
/// ```php
/// class U { public $d; function __unserialize(array $d): void { echo "unserialize ", json_encode($d), "\n"; $this->d = $d; } }
/// class W { public $i; function __wakeup() { echo "wakeup ", get_class($this->i ?? $this), "\n"; } }
/// $w = unserialize('O:1:"W":1:{s:1:"i";O:1:"W":1:{s:1:"i";N;}}');
/// $u = unserialize('O:1:"U":2:{s:1:"a";i:1;i:0;s:1:"b";}');
/// var_dump(unserialize('O:7:"Missing":1:{s:1:"x";i:1;}'), unserialize('O:1:"W":0:{}', ['allowed_classes' => false]));
/// ```
/// ```expect
/// wakeup W
/// wakeup W
/// unserialize {"a":1,"0":"b"}
/// object(__PHP_Incomplete_Class)#4 (2) {
///   ["__PHP_Incomplete_Class_Name"]=>
///   string(7) "Missing"
///   ["x"]=>
///   int(1)
/// }
/// object(__PHP_Incomplete_Class)#5 (1) {
///   ["__PHP_Incomplete_Class_Name"]=>
///   string(1) "W"
/// }
/// ```
///
/// The options: `allowed_classes` an array of names or a bool, and
/// `max_depth` an int, each held to its type.
///
/// ```php
/// var_dump(unserialize('O:1:"W":0:{}', ['allowed_classes' => ['w']]) instanceof W, unserialize('a:1:{i:0;a:1:{i:0;i:1;}}', ['max_depth' => 1]));
/// try { unserialize('i:1;', ['allowed_classes' => 1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { unserialize('i:1;', ['max_depth' => -1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// class W { function __wakeup() {} }
/// ```
/// ```expect
/// Warning: unserialize(): Maximum depth of 1 exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting in %s on line 2
///
/// Warning: unserialize(): Error at offset 14 of 24 bytes in %s on line 2
/// bool(true)
/// bool(false)
/// unserialize(): Option "allowed_classes" must be of type array|bool, int given
/// unserialize(): Option "max_depth" must be greater than or equal to 0
/// ```
///
/// An enum case is `E:` and its name, the case itself read back.
///
/// ```php
/// var_dump(unserialize('E:25:"RoundingMode:HalfEven";') === RoundingMode::HalfEven, unserialize('E:11:"Nope:Nobody";'), unserialize('E:8:"stdClass";'));
/// ```
/// ```expect
/// Warning: unserialize(): Error at offset 2 of 29 bytes in %s on line 2
///
/// Warning: unserialize(): Class 'Nope' not found in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 19 bytes in %s on line 2
///
/// Warning: unserialize(): Invalid enum name 'stdClass' (missing colon) in %s on line 2
///
/// Warning: unserialize(): Error at offset 0 of 15 bytes in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(super) fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let data = crate::string_argument(req, 1)?.as_bytes().to_vec();
    let (allowed, max_depth) = options(req)?;
    if data.is_empty() {
        return Ok(Value::bool(false));
    }
    let mut state = State {
        buf: &data,
        p: 0,
        slots: Vec::new(),
        root: Value::null(),
        delayed: Vec::new(),
        allowed,
        max_depth,
        depth: 0,
    };
    let parsed = state.value(req, Some(Slot::Root));
    let mut pending: Option<Thrown> = None;
    let ok = match parsed {
        Ok(ok) => ok,
        Err(thrown) => {
            pending = Some(thrown);
            false
        }
    };
    let total = data.len();
    if pending.is_none() {
        if !ok {
            docref(
                req,
                Level::Warning,
                &format!("Error at offset {} of {total} bytes", state.p),
            );
        } else if state.p < total {
            docref(
                req,
                Level::Warning,
                &format!("Extra data starting at offset {} of {total} bytes", state.p),
            );
        }
    }
    // `var_destroy`: the delayed calls, in the order they were queued,
    // until one of them fails.
    for delayed in std::mem::take(&mut state.delayed) {
        if pending.is_some() {
            break;
        }
        let called = match delayed {
            Delayed::Wakeup(object) => call_method(req, &object, b"__wakeup", &[]),
            Delayed::Unserialize(object, array) => {
                let answered = call_method(req, &object, b"__unserialize", &[array.to_value()]);
                req.release(array.to_value());
                answered
            }
        };
        match called {
            Ok(answer) => req.release(answer),
            Err(thrown) => pending = Some(thrown),
        }
    }
    if let Some(thrown) = pending {
        req.release(state.root);
        return Err(thrown);
    }
    if !ok {
        req.release(state.root);
        return Ok(Value::bool(false));
    }
    // A reference is unwrapped, per calling convention.
    let root = state.root;
    if let Some(cell) = root.as_php_ref() {
        let inner = cell.get();
        req.addref(&inner);
        req.release(root);
        return Ok(inner);
    }
    Ok(root)
}

/// `php_unserialize_data_t` with `php_var_unserialize`, `PHPAPI` in
/// `php_var.h`: one var hash across the values read out of one string,
/// each from where the last stopped, so an `R:n;` in a later value is a
/// reference to a slot an earlier one filled. What ext/session's decoders
/// read each session variable through into the session's array;
/// [`Unserializer::finish`] is `PHP_VAR_UNSERIALIZE_DESTROY`, the delayed
/// `__unserialize()` and `__wakeup()` calls.
pub struct Unserializer<'a> {
    state: State<'a>,
}

impl<'a> Unserializer<'a> {
    /// `PHP_VAR_UNSERIALIZE_INIT` over `data`, every class allowed and
    /// the depth `unserialize_max_depth` sets.
    #[must_use]
    pub fn new(req: &Request, data: &'a [u8]) -> Unserializer<'a> {
        let max_depth = req
            .ini_get(b"unserialize_max_depth")
            .and_then(|v| std::str::from_utf8(v).ok()?.trim().parse::<i64>().ok())
            .unwrap_or(4096);
        Unserializer {
            state: State {
                buf: data,
                p: 0,
                slots: Vec::new(),
                root: Value::null(),
                delayed: Vec::new(),
                allowed: None,
                max_depth,
                depth: 0,
            },
        }
    }

    /// Where the scanner stands, php's `*p` after the last value.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.state.p
    }

    /// The scanner moved to `at`, as a decoder steps over the name it
    /// read itself.
    pub const fn set_offset(&mut self, at: usize) {
        self.state.p = at;
    }

    /// `php_var_unserialize` of one value into `array[key]`, the slot
    /// the var hash numbers; `false` is the scanner's failure. The key
    /// is the array's from here, as a `set` takes it.
    pub fn element(
        &mut self,
        req: &mut Request,
        array: PhpArray,
        key: Key,
    ) -> Result<bool, Thrown> {
        self.state.value(req, Some(Slot::Element(array, key)))
    }

    /// `php_var_unserialize` of one value as a root, `None` for the
    /// scanner's failure; the value is the caller's, a reference
    /// unwrapped.
    pub fn value(&mut self, req: &mut Request) -> Result<Option<Value>, Thrown> {
        if !self.state.value(req, Some(Slot::Root))? {
            return Ok(None);
        }
        let root = std::mem::replace(&mut self.state.root, Value::null());
        if let Some(cell) = root.as_php_ref() {
            let inner = cell.get();
            req.addref(&inner);
            req.release(root);
            return Ok(Some(inner));
        }
        Ok(Some(root))
    }

    /// `PHP_VAR_UNSERIALIZE_DESTROY`: the delayed calls, in the order
    /// they were queued, until one of them throws.
    pub fn finish(mut self, req: &mut Request) -> Result<(), Thrown> {
        req.release(std::mem::replace(&mut self.state.root, Value::null()));
        for delayed in std::mem::take(&mut self.state.delayed) {
            let called = match delayed {
                Delayed::Wakeup(object) => call_method(req, &object, b"__wakeup", &[]),
                Delayed::Unserialize(object, array) => {
                    let answered = call_method(req, &object, b"__unserialize", &[array.to_value()]);
                    req.release(array.to_value());
                    answered
                }
            };
            req.release(called?);
        }
        Ok(())
    }
}

/// The allowed classes, lowered, `None` for all of them, and the depth
/// limit.
type Options = (Option<Vec<Vec<u8>>>, i64);

/// The `allowed_classes` and `max_depth` options, held to their types
/// as php holds them; the default depth is `unserialize_max_depth`.
fn options(req: &mut Request) -> Result<Options, Thrown> {
    let default_depth = req
        .ini_get(b"unserialize_max_depth")
        .and_then(|v| std::str::from_utf8(v).ok()?.trim().parse::<i64>().ok())
        .unwrap_or(4096);
    if req.argument_count() < 2 {
        return Ok((None, default_depth));
    }
    let given = req.argument(2).unwrap_or_default();
    let coerced = php_ext::coerce::coerce_param(req, &given, &php_ext::decl::Type::Array, 2)?;
    let Some(options) = coerced.deref().as_array().copied() else {
        return Ok((None, default_depth));
    };
    let mut allowed = None;
    if let Some(classes) = options.get(&Key::Str(PhpString::new(req, b"allowed_classes")?)) {
        let classes = classes.deref();
        match classes.as_ref() {
            ValueRef::Bool(true) => {}
            ValueRef::Bool(false) => allowed = Some(Vec::new()),
            ValueRef::Array(list) => {
                let mut names = Vec::new();
                for (_, entry) in list.snapshot() {
                    let entry = entry.deref();
                    if !matches!(entry.as_ref(), ValueRef::Str(_) | ValueRef::Object(_)) {
                        let name = value_name(req, &entry);
                        return Err(throw(
                            req,
                            "TypeError",
                            &format!(
                                "unserialize(): Option \"allowed_classes\" must be an array of class names, {name} given"
                            ),
                        ));
                    }
                    let name = php_ext::coerce::to_string(req, &entry)?.as_bytes().to_vec();
                    if !valid_class_name(&name) {
                        let shown = String::from_utf8_lossy(&name).into_owned();
                        return Err(throw(
                            req,
                            "ValueError",
                            &format!(
                                "unserialize(): Option \"allowed_classes\" must be an array of class names, \"{shown}\" given"
                            ),
                        ));
                    }
                    names.push(name.to_ascii_lowercase());
                }
                allowed = Some(names);
            }
            _ => {
                let name = value_name(req, &classes);
                return Err(throw(
                    req,
                    "TypeError",
                    &format!(
                        "unserialize(): Option \"allowed_classes\" must be of type array|bool, {name} given"
                    ),
                ));
            }
        }
    }
    let mut depth = default_depth;
    if let Some(max) = options.get(&Key::Str(PhpString::new(req, b"max_depth")?)) {
        let max = max.deref();
        let Some(n) = max.as_int() else {
            let name = value_name(req, &max);
            return Err(throw(
                req,
                "TypeError",
                &format!("unserialize(): Option \"max_depth\" must be of type int, {name} given"),
            ));
        };
        if n < 0 {
            return Err(throw(
                req,
                "ValueError",
                "unserialize(): Option \"max_depth\" must be greater than or equal to 0",
            ));
        }
        depth = n;
    }
    Ok((allowed, depth))
}

/// `zend_is_valid_class_name`: letters, digits, `_`, `\` and the bytes
/// past 0x7f.
fn valid_class_name(name: &[u8]) -> bool {
    name.iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'\\' || *c >= 0x80)
}

/// A scalar the key scanner read.
enum Scalar {
    Long(i64),
    Str(Vec<u8>),
    Other,
}

impl State<'_> {
    fn at(&self, at: usize) -> u8 {
        self.buf.get(at).copied().unwrap_or(0)
    }

    fn remaining(&self, from: usize) -> usize {
        self.buf.len().saturating_sub(from)
    }

    /// `uiv = [0-9]+` at `from`: its end, if one digit stands there.
    fn uiv(&self, from: usize) -> Option<usize> {
        let mut end = from;
        while self.at(end).is_ascii_digit() && end < self.buf.len() {
            end += 1;
        }
        (end > from).then_some(end)
    }

    /// `iv = [+-]? [0-9]+`.
    fn iv(&self, from: usize) -> Option<usize> {
        let start = if matches!(self.at(from), b'+' | b'-') {
            from + 1
        } else {
            from
        };
        self.uiv(start)
    }

    /// `nv = [+-]? ([0-9]* "." [0-9]+ | [0-9]+ "." [0-9]*)`.
    fn nv(&self, from: usize) -> Option<usize> {
        let start = if matches!(self.at(from), b'+' | b'-') {
            from + 1
        } else {
            from
        };
        let mut end = start;
        while self.at(end).is_ascii_digit() && end < self.buf.len() {
            end += 1;
        }
        let integral = end > start;
        if self.at(end) != b'.' || end >= self.buf.len() {
            return None;
        }
        let point = end;
        end += 1;
        while self.at(end).is_ascii_digit() && end < self.buf.len() {
            end += 1;
        }
        (integral || end > point + 1).then_some(end)
    }

    /// The longest of `iv`, `nv` and `nvexp` at `from`.
    fn number(&self, from: usize) -> Option<usize> {
        let base = [self.nv(from), self.iv(from)].into_iter().flatten().max()?;
        if matches!(self.at(base), b'e' | b'E')
            && base < self.buf.len()
            && let Some(exp) = self.iv(base + 1)
        {
            return Some(exp);
        }
        Some(base)
    }

    /// `parse_uiv`.
    fn parse_uiv(&self, from: usize) -> usize {
        let mut n: usize = 0;
        let mut at = from;
        while self.at(at).is_ascii_digit() && at < self.buf.len() {
            n = n
                .wrapping_mul(10)
                .wrapping_add(usize::from(self.at(at) - b'0'));
            at += 1;
        }
        n
    }

    /// `parse_iv2`: the number and where it ended, php's warning and the
    /// long's bound for one out of range.
    fn parse_iv2(&self, req: &mut Request, from: usize) -> (i64, usize) {
        let mut at = from;
        let mut negative = false;
        if self.at(at) == b'-' {
            negative = true;
            at += 1;
        } else if self.at(at) == b'+' {
            at += 1;
        }
        while self.at(at) == b'0' && at < self.buf.len() {
            at += 1;
        }
        let start = at;
        let mut result: u64 = 0;
        while self.at(at).is_ascii_digit() && at < self.buf.len() {
            result = result
                .wrapping_mul(10)
                .wrapping_add(u64::from(self.at(at) - b'0'));
            at += 1;
        }
        if at - start > 19 || result > i64::MAX.cast_unsigned() + u64::from(negative) {
            docref(req, Level::Warning, "Numerical result out of range");
            return (if negative { i64::MIN } else { i64::MAX }, at);
        }
        let value = if negative {
            result.wrapping_neg().cast_signed()
        } else {
            result.cast_signed()
        };
        (value, at)
    }

    /// `var_push`: the slot numbered for `R:` and `r:`.
    fn push(&mut self, slot: &Slot, value: Value) -> usize {
        self.slots.push(Some((slot.clone(), value)));
        self.slots.len() - 1
    }

    /// The value written into `slot`, which from here holds it, and the
    /// var hash entry updated.
    fn write(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        value: Value,
    ) -> Result<(), Thrown> {
        match slot {
            Slot::Root => {
                let previous = std::mem::replace(&mut self.root, value);
                req.release(previous);
            }
            Slot::Element(array, key) => {
                let mut array = *array;
                if let Key::Str(s) = key {
                    req.addref(&Value::string(*s));
                }
                array.set(req, key.clone(), value)?;
            }
            Slot::Property(object, name) => {
                req.addref(&Value::string(*name));
                object.unserialize_prop(req, *name, value)?;
            }
        }
        if let Some(at) = entry
            && let Some(Some((_, held))) = self.slots.get_mut(at)
        {
            *held = value;
        }
        Ok(())
    }

    /// `php_var_unserialize_internal` with a var hash: one value into
    /// `slot`. `false` is the scanner's failure, `self.p` where php's
    /// `*p` stands for the offset.
    // php's scanner is one function of rules; the arms stay together.
    #[allow(clippy::too_many_lines)]
    fn value(&mut self, req: &mut Request, slot: Option<Slot>) -> Result<bool, Thrown> {
        let Some(slot) = slot else {
            return Ok(false);
        };
        let start = self.p;
        if start >= self.buf.len() {
            return Ok(false);
        }
        let entry = if self.at(start) == b'R' {
            None
        } else {
            Some(self.push(&slot, Value::null()))
        };
        let c0 = self.at(start);
        let c1 = self.at(start + 1);
        match (c0, c1) {
            (b'R' | b'r', b':') => {
                let Some(end) = self.uiv(start + 2) else {
                    return Ok(false);
                };
                if self.at(end) != b';' || end >= self.buf.len() {
                    return Ok(false);
                }
                self.p = end + 1;
                let id = self.parse_uiv(start + 2);
                let Some(Some((held_slot, held))) =
                    id.checked_sub(1).and_then(|i| self.slots.get(i)).cloned()
                else {
                    return Ok(false);
                };
                if c0 == b'R' {
                    if slots_equal(&held_slot, &slot) {
                        return Ok(false);
                    }
                    let cell = if let Some(cell) = held.as_php_ref() {
                        *cell
                    } else {
                        req.addref(&held);
                        let cell = PhpRef::new(req, held)?;
                        let at = id - 1;
                        self.write(req, &held_slot, Some(at), cell.as_value())?;
                        cell
                    };
                    req.addref(&cell.as_value());
                    self.write(req, &slot, entry, cell.as_value())?;
                    return Ok(true);
                }
                if slots_equal(&held_slot, &slot) {
                    return Ok(false);
                }
                let object = held.deref();
                if object.as_object().is_none() {
                    return Ok(false);
                }
                req.addref(&object);
                self.write(req, &slot, entry, object)?;
                Ok(true)
            }
            (b'N', b';') => {
                self.p = start + 2;
                self.write(req, &slot, entry, Value::null())?;
                Ok(true)
            }
            (b'b', b':')
                if matches!(self.at(start + 2), b'0' | b'1')
                    && self.at(start + 3) == b';'
                    && start + 3 < self.buf.len() =>
            {
                self.p = start + 4;
                self.write(req, &slot, entry, Value::bool(self.at(start + 2) == b'1'))?;
                Ok(true)
            }
            (b'i', b':') => {
                let Some(end) = self.iv(start + 2) else {
                    return Ok(false);
                };
                if self.at(end) != b';' || end >= self.buf.len() {
                    return Ok(false);
                }
                self.p = end + 1;
                let (n, _) = self.parse_iv2(req, start + 2);
                self.write(req, &slot, entry, Value::int(n))?;
                Ok(true)
            }
            (b'd', b':') => self.double(req, &slot, entry, start),
            (b's' | b'S', b':') => {
                let Some(text) = self.string(req, start, c0 == b'S') else {
                    return Ok(false);
                };
                let value = Value::string(PhpString::new(req, &text)?);
                self.write(req, &slot, entry, value)?;
                Ok(true)
            }
            (b'a', b':') => self.array(req, &slot, entry, start),
            (b'O' | b'C', b':') => self.object(req, &slot, entry, start, c0 == b'C'),
            (b'E', b':') => self.enum_case(req, &slot, entry, start),
            (b'}', _) => {
                docref(req, Level::Warning, "Unexpected end of serialized data");
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// The scanner without a var hash, what reads a key.
    fn key(&mut self, req: &mut Request) -> Option<Scalar> {
        let start = self.p;
        if start >= self.buf.len() {
            return None;
        }
        match (self.at(start), self.at(start + 1)) {
            (b'i', b':') => {
                let end = self.iv(start + 2)?;
                if self.at(end) != b';' || end >= self.buf.len() {
                    return None;
                }
                self.p = end + 1;
                Some(Scalar::Long(self.parse_iv2(req, start + 2).0))
            }
            (b's' | b'S', b':') => {
                let upper = self.at(start) == b'S';
                self.string(req, start, upper).map(Scalar::Str)
            }
            (b'N', b';') => {
                self.p = start + 2;
                Some(Scalar::Other)
            }
            (b'b', b':')
                if matches!(self.at(start + 2), b'0' | b'1') && self.at(start + 3) == b';' =>
            {
                self.p = start + 4;
                Some(Scalar::Other)
            }
            (b'd', b':') => {
                let end = self.double_end(start)?;
                self.p = end;
                Some(Scalar::Other)
            }
            (b'a' | b'O' | b'C' | b'E', b':') => {
                // The token's header matched; with no var hash php
                // answers failure where the header ended.
                None
            }
            (b'}', _) => {
                docref(req, Level::Warning, "Unexpected end of serialized data");
                None
            }
            _ => None,
        }
    }

    /// Where a `d:` token ends, past its `;`.
    fn double_end(&self, start: usize) -> Option<usize> {
        let from = start + 2;
        for word in [&b"NAN"[..], b"INF", b"-INF"] {
            if self.buf.get(from..from + word.len()) == Some(word)
                && self.at(from + word.len()) == b';'
            {
                return Some(from + word.len() + 1);
            }
        }
        let end = self.number(from)?;
        (self.at(end) == b';' && end < self.buf.len()).then_some(end + 1)
    }

    fn double(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.double_end(start) else {
            return Ok(false);
        };
        self.p = end;
        let text = &self.buf[start + 2..end - 1];
        let value = match text {
            b"NAN" => f64::NAN,
            b"INF" => f64::INFINITY,
            b"-INF" => f64::NEG_INFINITY,
            _ => std::str::from_utf8(text)
                .ok()
                .and_then(|t| t.parse::<f64>().ok())
                .unwrap_or(0.0),
        };
        self.write(req, slot, entry, Value::float(value))?;
        Ok(true)
    }

    /// `"s:" uiv ":" ["]` and its `S:` twin: the bytes, `None` for the
    /// scanner's failure with `self.p` where php leaves `*p`.
    fn string(&mut self, req: &mut Request, start: usize, escaped: bool) -> Option<Vec<u8>> {
        let end = self.uiv(start + 2)?;
        if self.at(end) != b':' || self.at(end + 1) != b'"' || end + 1 >= self.buf.len() {
            return None;
        }
        let cursor = end + 2;
        let len = self.parse_uiv(start + 2);
        let maxlen = self.remaining(cursor);
        if maxlen < len {
            self.p = start + 2;
            return None;
        }
        let (text, after) = if escaped {
            let mut out = Vec::with_capacity(len);
            let mut at = cursor;
            for _ in 0..len {
                if at >= self.buf.len() {
                    return None;
                }
                if self.at(at) == b'\\' {
                    let hex = |c: u8| (c as char).to_digit(16);
                    let (Some(hi), Some(lo)) = (hex(self.at(at + 1)), hex(self.at(at + 2))) else {
                        return None;
                    };
                    out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                    at += 3;
                } else {
                    out.push(self.at(at));
                    at += 1;
                }
            }
            (out, at)
        } else {
            (self.buf[cursor..cursor + len].to_vec(), cursor + len)
        };
        if self.at(after) != b'"' || after >= self.buf.len() {
            self.p = after;
            return None;
        }
        if self.at(after + 1) != b';' || after + 1 >= self.buf.len() {
            self.p = after + 1;
            return None;
        }
        self.p = after + 2;
        if escaped {
            docref(
                req,
                Level::Deprecated,
                "Unserializing the 'S' format is deprecated",
            );
        }
        Some(text)
    }

    /// `finish_nested_data`.
    fn finish(&mut self) -> bool {
        if self.p >= self.buf.len() || self.at(self.p) != b'}' {
            return false;
        }
        self.p += 1;
        true
    }

    /// The depth check `process_nested_*_data` opens with.
    fn deeper(&mut self, req: &mut Request) -> bool {
        if self.max_depth > 0 && self.depth >= self.max_depth {
            let max = self.max_depth;
            docref(
                req,
                Level::Warning,
                &format!(
                    "Maximum depth of {max} exceeded. The depth limit can be changed using the max_depth unserialize() option or the unserialize_max_depth ini setting"
                ),
            );
            return false;
        }
        self.depth += 1;
        true
    }

    /// `"a:" uiv ":" "{"`.
    fn array(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.uiv(start + 2) else {
            return Ok(false);
        };
        if self.at(end) != b':' || self.at(end + 1) != b'{' || end + 1 >= self.buf.len() {
            return Ok(false);
        }
        let (elements, _) = self.parse_iv2(req, start + 2);
        self.p = end + 2;
        if !(0..HT_MAX_SIZE).contains(&elements)
            || usize::try_from(elements).unwrap_or(usize::MAX) > self.remaining(self.p) / 2
        {
            return Ok(false);
        }
        let array = PhpArray::new(req)?;
        self.write(req, slot, entry, Value::array(array))?;
        if elements == 0 {
            return Ok(self.finish());
        }
        if !self.array_elements(req, array, elements)? {
            return Ok(false);
        }
        Ok(self.finish())
    }

    /// `process_nested_array_data`.
    fn array_elements(
        &mut self,
        req: &mut Request,
        array: PhpArray,
        elements: i64,
    ) -> Result<bool, Thrown> {
        if !self.deeper(req) {
            return Ok(false);
        }
        let done = self.array_body(req, array, elements);
        self.depth -= 1;
        done
    }

    fn array_body(
        &mut self,
        req: &mut Request,
        array: PhpArray,
        mut elements: i64,
    ) -> Result<bool, Thrown> {
        let mut array = array;
        while elements > 0 {
            elements -= 1;
            let key = match self.key(req) {
                Some(Scalar::Long(n)) => Key::Int(n),
                Some(Scalar::Str(bytes)) => Key::from_string(PhpString::new(req, &bytes)?),
                _ => return Ok(false),
            };
            if let Key::Str(s) = &key {
                req.addref(&Value::string(*s));
            }
            array.set(req, key.clone(), Value::null())?;
            if !self.value(req, Some(Slot::Element(array, key)))? {
                return Ok(false);
            }
            if elements > 0 && !matches!(self.at(self.p - 1), b';' | b'}') {
                self.p -= 1;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `object ":" uiv ":" ["]`, `O:` and `C:`.
    // php's rule is one block; its checks stay in its order.
    #[allow(clippy::too_many_lines)]
    fn object(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
        custom: bool,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.uiv(start + 2) else {
            return Ok(false);
        };
        if self.at(end) != b':' || self.at(end + 1) != b'"' || end + 1 >= self.buf.len() {
            return Ok(false);
        }
        let cursor = end + 2;
        let len = self.parse_uiv(start + 2);
        if self.remaining(cursor) < len || len == 0 {
            self.p = start + 2;
            return Ok(false);
        }
        let name = self.buf[cursor..cursor + len].to_vec();
        let after = cursor + len;
        if self.at(after) != b'"' {
            self.p = after;
            return Ok(false);
        }
        if self.at(after + 1) != b':' {
            self.p = after + 1;
            return Ok(false);
        }
        if name[0] == 0 || name[0] == b'\\' {
            return Ok(false);
        }
        let lower = name.to_ascii_lowercase();
        let allowed = self
            .allowed
            .as_ref()
            .is_none_or(|list| list.contains(&lower));
        let mut incomplete = false;
        let class = if allowed {
            if let Some(class) = lookup_class(req, &name)? {
                Some(class)
            } else {
                if !valid_class_name(&name) {
                    return Ok(false);
                }
                incomplete = true;
                Self::callback(req, &name)?;
                if let Some(class) = lookup_class(req, &name)? {
                    incomplete = false;
                    Some(class)
                } else {
                    lookup_class(req, INCOMPLETE_CLASS)?
                }
            }
        } else {
            if !valid_class_name(&name) {
                return Ok(false);
            }
            incomplete = true;
            lookup_class(req, INCOMPLETE_CLASS)?
        };
        let Some(class) = class else {
            return Ok(false);
        };
        self.p = after;
        if custom {
            return self.custom(req, slot, entry, class, &name, incomplete);
        }
        if self.p + 2 >= self.buf.len() {
            warning(req, "Bad unserialize data");
            return Ok(false);
        }
        let (elements, at) = self.parse_iv2(req, self.p + 2);
        self.p = at;
        if elements < 0
            || usize::try_from(elements).unwrap_or(usize::MAX) > self.remaining(after) / 2
        {
            return Ok(false);
        }
        if self.at(self.p) != b':' {
            return Ok(false);
        }
        if self.at(self.p + 1) != b'{' {
            self.p += 1;
            return Ok(false);
        }
        self.p += 2;
        let has_unserialize = !incomplete && has_method(req, class, b"__unserialize");
        let object = new_object(req, class)?;
        self.write(req, slot, entry, object.as_value())?;
        if incomplete {
            let key = PhpString::new(req, MAGIC_MEMBER)?;
            let stored = Value::string(PhpString::new(req, &name)?);
            object.unserialize_prop(req, key, stored)?;
        }
        if has_unserialize {
            if elements >= HT_MAX_SIZE {
                return Ok(false);
            }
            let array = PhpArray::new(req)?;
            if !self.array_elements(req, array, elements)? {
                req.release(array.to_value());
                return Ok(false);
            }
            req.addref(&object.as_value());
            self.delayed.push(Delayed::Unserialize(object, array));
            return Ok(self.finish());
        }
        let has_wakeup = !incomplete && has_method(req, class, b"__wakeup");
        if !self.object_properties(req, object, elements)? {
            return Ok(false);
        }
        if has_wakeup {
            req.addref(&object.as_value());
            self.delayed.push(Delayed::Wakeup(object));
        }
        Ok(self.finish())
    }

    /// `unserialize_callback_func`: the function the ini names, handed the
    /// class name, and php's warning where it did not define it.
    fn callback(req: &mut Request, name: &[u8]) -> Result<(), Thrown> {
        let Some(function) = req
            .ini_get(b"unserialize_callback_func")
            .map(<[u8]>::to_vec)
            .filter(|f| !f.is_empty())
        else {
            return Ok(());
        };
        let callee = Value::string(PhpString::new(req, &function)?);
        let argument = Value::string(PhpString::new(req, name)?);
        if let Ok(callable) = callable_checked(req, &callee) {
            let answer = call(req, &callable, &[argument])?;
            req.release(answer);
        }
        if lookup_class(req, name)?.is_none() {
            let shown = String::from_utf8_lossy(&function).into_owned();
            docref(
                req,
                Level::Warning,
                &format!("Function {shown}() hasn't defined the class it was called for"),
            );
        }
        Ok(())
    }

    /// `object_custom`, `C:`: the data's length and braces checked, and,
    /// since no class here has a `Serializable` unserializer, php's
    /// warning and an object with nothing written.
    fn custom(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        class: ClassId,
        name: &[u8],
        incomplete: bool,
    ) -> Result<bool, Thrown> {
        let (datalen, at) = self.parse_iv2(req, self.p + 2);
        self.p = at;
        if self.remaining(self.p) < 2 || self.at(self.p) != b':' {
            return Ok(false);
        }
        if self.at(self.p + 1) != b'{' {
            self.p += 1;
            return Ok(false);
        }
        self.p += 2;
        let present = i64::try_from(self.remaining(self.p)).unwrap_or(i64::MAX);
        if datalen < 0 || present <= datalen {
            warning(
                req,
                &format!(
                    "Insufficient data for unserializing - {datalen} required, {present} present"
                ),
            );
            return Ok(false);
        }
        let datalen = usize::try_from(datalen).unwrap_or(0);
        if self.at(self.p + datalen) != b'}' {
            self.p += datalen;
            return Ok(false);
        }
        let shown = String::from_utf8_lossy(
            &req.class_decl(class)
                .map(|d| d.name.0.to_vec())
                .unwrap_or_default(),
        )
        .into_owned();
        warning(req, &format!("Class {shown} has no unserializer"));
        let object = new_object(req, class)?;
        self.write(req, slot, entry, object.as_value())?;
        if incomplete {
            let key = PhpString::new(req, MAGIC_MEMBER)?;
            let stored = Value::string(PhpString::new(req, name)?);
            object.unserialize_prop(req, key, stored)?;
        }
        self.p += datalen + 1;
        Ok(true)
    }

    /// `process_nested_object_data`: each key made a string, its
    /// property written `null` and then the value read into it.
    fn object_properties(
        &mut self,
        req: &mut Request,
        object: PhpObject,
        elements: i64,
    ) -> Result<bool, Thrown> {
        if !self.deeper(req) {
            return Ok(false);
        }
        let done = self.object_body(req, object, elements);
        self.depth -= 1;
        done
    }

    fn object_body(
        &mut self,
        req: &mut Request,
        object: PhpObject,
        mut elements: i64,
    ) -> Result<bool, Thrown> {
        while elements > 0 {
            elements -= 1;
            let key = match self.key(req) {
                Some(Scalar::Str(bytes)) => bytes,
                Some(Scalar::Long(n)) => n.to_string().into_bytes(),
                _ => return Ok(false),
            };
            let name = PhpString::new(req, &key)?;
            req.addref(&Value::string(name));
            object.unserialize_prop(req, name, Value::null())?;
            if !self.value(req, Some(Slot::Property(object, name)))? {
                return Ok(false);
            }
            if elements > 0 && !matches!(self.at(self.p - 1), b';' | b'}') {
                self.p -= 1;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `"E:" uiv ":" ["]`: `Enum:Case`, the case read back.
    fn enum_case(
        &mut self,
        req: &mut Request,
        slot: &Slot,
        entry: Option<usize>,
        start: usize,
    ) -> Result<bool, Thrown> {
        let Some(end) = self.uiv(start + 2) else {
            return Ok(false);
        };
        if self.at(end) != b':' || self.at(end + 1) != b'"' || end + 1 >= self.buf.len() {
            return Ok(false);
        }
        let cursor = end + 2;
        let len = self.parse_uiv(start + 2);
        if self.remaining(cursor) < len || len == 0 {
            self.p = start + 2;
            return Ok(false);
        }
        let text = self.buf[cursor..cursor + len].to_vec();
        let after = cursor + len;
        if self.at(after) != b'"' {
            self.p = after;
            return Ok(false);
        }
        if self.at(after + 1) != b';' {
            self.p = after + 1;
            return Ok(false);
        }
        let shown = String::from_utf8_lossy(&text).into_owned();
        let Some(colon) = text.iter().position(|b| *b == b':') else {
            docref(
                req,
                Level::Warning,
                &format!("Invalid enum name '{shown}' (missing colon)"),
            );
            return Ok(false);
        };
        let (enum_name, case) = (&text[..colon], &text[colon + 1..]);
        let enum_shown = String::from_utf8_lossy(enum_name).into_owned();
        let case_shown = String::from_utf8_lossy(case).into_owned();
        if !valid_class_name(enum_name) {
            return Ok(false);
        }
        let Some(class) = lookup_class(req, enum_name)? else {
            docref(
                req,
                Level::Warning,
                &format!("Class '{enum_shown}' not found"),
            );
            return Ok(false);
        };
        if !is_enum_class(req, class) {
            docref(
                req,
                Level::Warning,
                &format!("Class '{enum_shown}' is not an enum"),
            );
            return Ok(false);
        }
        self.p = after + 2;
        let known = req
            .class_decl(class)
            .is_some_and(|decl| decl.constants.iter().any(|c| *c.name.0 == *case));
        if !known {
            docref(
                req,
                Level::Warning,
                &format!("Undefined constant {enum_shown}::{case_shown}"),
            );
            return Ok(false);
        }
        let value = class_constant(req, class, case)?;
        self.write(req, slot, entry, value)?;
        Ok(true)
    }
}

/// Whether two slots name the same place, php's `rval_ref == rval`.
fn slots_equal(left: &Slot, right: &Slot) -> bool {
    match (left, right) {
        (Slot::Root, Slot::Root) => true,
        (Slot::Element(one, key), Slot::Element(other, key2)) => one == other && key == key2,
        (Slot::Property(one, name), Slot::Property(other, name2)) => one == other && name == name2,
        _ => false,
    }
}

#[allow(dead_code)]
fn unused(req: &mut Request) {
    deprecated(req, "");
}
