//! `Z/ext/standard/scanf.c`: `php_sscanf_internal`, the Tcl-derived
//! scanner behind `sscanf` (and `fscanf` once `file.c`'s half lands),
//! with `ValidateFormat` read over the whole format first and
//! `BuildCharSet` for a `%[` set. The input and the format are C
//! strings, so each ends at its first NUL, as php's `char *` does.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::string::Decimal;
use php_ext::abi::{Ref, Union, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::is_space;
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

/// `SCAN_MAX_ARGS`.
const SCAN_MAX_ARGS: u64 = 0xff;
/// `SCAN_ERROR_EOF`.
const SCAN_ERROR_EOF: i64 = -1;

/// `ZEND_STRTOUL(s, &end, 10)` for the digits at `at`: the value and
/// the offset past them. Kept beside the face's `strtol`: php calls it on
/// a digit, so no space or sign is read, and the unsigned long it answers
/// saturates at `ULONG_MAX` with its end.
fn strtoul(s: &[u8], at: usize) -> (u64, usize) {
    let mut end = at;
    let mut value: u64 = 0;
    while end < s.len() && s[end].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add(u64::from(s[end] - b'0'));
        end += 1;
    }
    (value, end)
}

/// Why `ValidateFormat` refused the format: php's `ValueError`, one
/// sentence or the bad conversion byte. php's check that `%c` takes no
/// width is commented out of the file (a width on `%c` reads that many
/// bytes, which php keeps as its extension to Tcl's scanner).
enum Invalid {
    Value(&'static str),
    BadChar(u8),
}

/// `ValidateFormat(format, numVars, &totalSubs)`: the count of
/// variables the format assigns.
#[allow(clippy::too_many_lines)]
fn validate(format: &[u8], num_vars: usize) -> Result<usize, Invalid> {
    let at = |i: usize| format.get(i).copied().unwrap_or(0);
    let mut nassign: Vec<u32> = vec![0; num_vars.max(16)];
    let (mut xpg_size, mut obj_index) = (0usize, 0usize);
    let (mut got_xpg, mut got_sequential) = (false, false);
    let mut f = 0usize;
    let bad_index = |got_xpg: bool| {
        if got_xpg {
            Invalid::Value("\"%n$\" argument index out of range")
        } else {
            Invalid::Value("Different numbers of variable names and field specifiers")
        }
    };
    while at(f) != 0 {
        let mut ch = at(f);
        f += 1;
        let mut suppress = false;
        if ch != b'%' {
            continue;
        }
        ch = at(f);
        f += 1;
        if ch == b'%' {
            continue;
        }
        let mut xpg_checked = false;
        if ch == b'*' {
            suppress = true;
            ch = at(f);
            f += 1;
            xpg_checked = true;
        } else if ch.is_ascii_digit() {
            let (value, end) = strtoul(format, f - 1);
            if at(end) == b'$' {
                f = end + 1;
                ch = at(f);
                f += 1;
                got_xpg = true;
                if got_sequential {
                    return Err(Invalid::Value(
                        "cannot mix \"%\" and \"%n$\" conversion specifiers",
                    ));
                }
                let value_ok = value >= 1 && (num_vars == 0 || value <= num_vars as u64);
                if !value_ok {
                    return Err(bad_index(got_xpg));
                }
                if num_vars == 0 {
                    if value > SCAN_MAX_ARGS {
                        return Err(bad_index(got_xpg));
                    }
                    xpg_size = xpg_size.max(usize::try_from(value).unwrap_or(0));
                }
                obj_index = usize::try_from(value).unwrap_or(1) - 1;
                xpg_checked = true;
            }
        }
        if !xpg_checked {
            got_sequential = true;
            if got_xpg {
                return Err(Invalid::Value(
                    "cannot mix \"%\" and \"%n$\" conversion specifiers",
                ));
            }
        }
        if ch.is_ascii_digit() {
            let (_, end) = strtoul(format, f - 1);
            f = end;
            ch = at(f);
            f += 1;
        }
        if matches!(ch, b'l' | b'L' | b'h') {
            ch = at(f);
            f += 1;
        }
        if !suppress && num_vars > 0 && obj_index >= num_vars {
            return Err(bad_index(got_xpg));
        }
        match ch {
            b'n' | b'd' | b'D' | b'i' | b'o' | b'x' | b'X' | b'u' | b'f' | b'e' | b'E' | b'g'
            | b's' | b'c' => {}
            b'[' => {
                let bad_set = Invalid::Value("Unmatched [ in format string");
                if at(f) == 0 {
                    return Err(bad_set);
                }
                ch = at(f);
                f += 1;
                if ch == b'^' {
                    if at(f) == 0 {
                        return Err(bad_set);
                    }
                    ch = at(f);
                    f += 1;
                }
                if ch == b']' {
                    if at(f) == 0 {
                        return Err(bad_set);
                    }
                    ch = at(f);
                    f += 1;
                }
                while ch != b']' {
                    if at(f) == 0 {
                        return Err(bad_set);
                    }
                    ch = at(f);
                    f += 1;
                }
            }
            other => return Err(Invalid::BadChar(other)),
        }
        if !suppress {
            if obj_index >= nassign.len() {
                let grown = if xpg_size > 0 {
                    xpg_size.max(obj_index + 1)
                } else {
                    nassign.len() + 16
                };
                nassign.resize(grown.max(obj_index + 1), 0);
            }
            nassign[obj_index] += 1;
            obj_index += 1;
        }
    }
    let total = if num_vars == 0 {
        if xpg_size > 0 { xpg_size } else { obj_index }
    } else {
        num_vars
    };
    for slot in nassign.iter().take(total) {
        if *slot > 1 {
            return Err(Invalid::Value(
                "Variable is assigned by multiple \"%n$\" conversion specifiers",
            ));
        } else if xpg_size == 0 && *slot == 0 {
            return Err(Invalid::Value(
                "Variable is not assigned by any conversion specifiers",
            ));
        }
    }
    Ok(total)
}

/// `CharSet`: the bytes and ranges of a `%[` set, and whether it is
/// the complement.
struct CharSet {
    exclude: bool,
    chars: Vec<u8>,
    ranges: Vec<(i8, i8)>,
}

/// A byte as the pinned build's `char`, which Apple's arm64 ABI keeps
/// signed, so a range reaching past `0x7f` orders as php's does.
const fn signed(c: u8) -> i8 {
    i8::from_ne_bytes([c])
}

impl CharSet {
    /// `CharInSet(cset, ch)`.
    fn contains(&self, c: u8) -> bool {
        let ch = signed(c);
        let hit = self.chars.contains(&c)
            || self
                .ranges
                .iter()
                .any(|(start, end)| *start <= ch && ch <= *end);
        hit != self.exclude
    }
}

/// `BuildCharSet(cset, format)`: the set that starts at `f`, just past
/// the `[`, and the offset after its `]`.
fn build_char_set(format: &[u8], mut f: usize) -> (CharSet, usize) {
    let at = |i: usize| format.get(i).copied().unwrap_or(0);
    let mut set = CharSet {
        exclude: false,
        chars: Vec::new(),
        ranges: Vec::new(),
    };
    if at(f) == b'^' {
        set.exclude = true;
        f += 1;
    }
    let mut ch = at(f);
    f += 1;
    let mut start = ch;
    if ch == b']' || ch == b'-' {
        set.chars.push(ch);
        ch = at(f);
        f += 1;
    }
    while ch != b']' && ch != 0 {
        if at(f) == b'-' {
            start = ch;
        } else if ch == b'-' {
            if at(f) == b']' {
                set.chars.push(start);
                set.chars.push(ch);
            } else {
                ch = at(f);
                f += 1;
                let (a, b) = (signed(start), signed(ch));
                set.ranges.push(if a < b { (a, b) } else { (b, a) });
            }
        } else {
            set.chars.push(ch);
        }
        ch = at(f);
        f += 1;
    }
    (set, f)
}

/// Where a conversion's value goes: the variable at a position, or the
/// answered array at an index.
enum Target {
    Vars { first: u32, count: usize },
    Array(Owner<PhpArray>),
}

/// `ZEND_STRTOL(buf, NULL, base)` and `ZEND_STRTOUL`: the digits of
/// `buf` in `base`, saturated as `strtol` saturates, and the unsigned
/// read wrapping a sign as `strtoul` does. Kept beside the face's
/// `strtol`, which reads base 10 and signed alone.
fn strtol(buf: &[u8], base: u32, unsigned: bool) -> i64 {
    let mut i = 0;
    let negative = match buf.first() {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    if base == 16 && buf.get(i) == Some(&b'0') && matches!(buf.get(i + 1), Some(b'x' | b'X')) {
        i += 2;
    }
    let mut value: u128 = 0;
    for &c in &buf[i.min(buf.len())..] {
        let Some(d) = (c as char).to_digit(base) else {
            break;
        };
        value = (value * u128::from(base) + u128::from(d)).min(u128::from(u64::MAX) + 1);
    }
    if unsigned {
        let magnitude = u64::try_from(value).unwrap_or(u64::MAX);
        let wrapped = if negative {
            magnitude.wrapping_neg()
        } else {
            magnitude
        };
        return i64::from_ne_bytes(wrapped.to_ne_bytes());
    }
    if negative {
        i64::try_from(value).map_or(i64::MIN, |v| -v)
    } else {
        i64::try_from(value).unwrap_or(i64::MAX)
    }
}

/// `sscanf(string $string, string $format, mixed &...$vars):
/// array|int|null`: the conversions as an array, or, with variables
/// given, each written through its reference and the count of
/// conversions answered; `-1`, or `null` without variables, where the
/// input ran out before the first conversion.
///
/// ```php
/// var_dump(sscanf("age: 25 name: Bob", "age: %d name: %s"), sscanf("12 apples", "%d %s %s"), sscanf("0x1A 017 -5", "%i %i %u"), sscanf("3.5e2xyz", "%f%s"), sscanf("abc123", "%[a-z]%d"), sscanf("hello", "%2c%c"), sscanf("a b", "%s%n"), sscanf("", "%d"), sscanf("x", "%d"), sscanf("12", "%2\$s %1\$s"));
/// var_dump(sscanf("10 20", "%d %d", $a, $b), $a, $b, sscanf("", "%d", $c), $c, sscanf("7", "%*d%n", $n), $n);
/// foreach (["%d %d", "%q", "%", "%1\$s %s", "%[abc", "%3c"] as $f) { try { var_dump(sscanf("1 2", $f, $x)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(25)
///   [1]=>
///   string(3) "Bob"
/// }
/// array(3) {
///   [0]=>
///   int(12)
///   [1]=>
///   string(6) "apples"
///   [2]=>
///   NULL
/// }
/// array(3) {
///   [0]=>
///   int(26)
///   [1]=>
///   int(15)
///   [2]=>
///   string(20) "18446744073709551611"
/// }
/// array(2) {
///   [0]=>
///   float(350)
///   [1]=>
///   string(3) "xyz"
/// }
/// array(2) {
///   [0]=>
///   string(3) "abc"
///   [1]=>
///   int(123)
/// }
/// array(2) {
///   [0]=>
///   string(2) "he"
///   [1]=>
///   string(1) "l"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   int(1)
/// }
/// NULL
/// array(1) {
///   [0]=>
///   NULL
/// }
/// array(2) {
///   [0]=>
///   NULL
///   [1]=>
///   string(2) "12"
/// }
/// int(2)
/// int(10)
/// int(20)
/// int(-1)
/// NULL
/// int(2)
/// int(1)
/// Different numbers of variable names and field specifiers
/// Bad scan conversion character "q"
/// Bad scan conversion character "
/// cannot mix "%" and "%n$" conversion specifiers
/// Unmatched [ in format string
/// int(1)
/// ```
///
/// A conversion skips the white space C's `isspace` reads under the
/// request's `LC_CTYPE`, 0xA0 among it under macOS's `C.UTF-8`.
///
/// ```php
/// var_dump(sscanf("\xa012 x\xa0y", "%d %s%s"));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(12)
///   [1]=>
///   string(1) "x"
///   [2]=>
///   string(1) "y"
/// }
/// ```
#[php_function]
pub(super) fn sscanf(
    req: &mut Request,
    string: PhpString,
    format: PhpString,
    vars: Variadic<Ref<Value>>,
) -> Result<Option<Union<Owner<PhpArray>, i64>>, Thrown> {
    let scanned = php_sscanf_internal(req, string.as_bytes(), format.as_bytes(), 3, vars.0.len())?;
    let answer = match scanned.as_ref() {
        ValueRef::Array(array) => Some(Union::A(Owner::take(req, *array))),
        ValueRef::Int(n) => Some(Union::B(n)),
        _ => None,
    };
    scanned.give_up(req);
    Ok(answer)
}

/// `php_sscanf_internal(string, format, argCount, args, varStart,
/// return_value)`, `PHPAPI` in `scanf.h` (`Z/ext/standard/scanf.c:574`):
/// `string` read by `format`, each read as a C string up to its first
/// NUL, into the calling native's arguments at `first..first + num_vars`
/// where `num_vars` is not zero and into the returned array where it is.
/// What `sscanf()` and `fscanf()` scan through, and what
/// `SplFileObject::fscanf()` calls directly.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("7 apples\n3 pears\n"); $t->rewind();
/// var_dump($t->fscanf('%d %s'), $t->fscanf('%d %s', $n, $fruit), $n, $fruit);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(7)
///   [1]=>
///   string(6) "apples"
/// }
/// int(2)
/// int(3)
/// string(5) "pears"
/// ```
pub fn php_sscanf_internal(
    req: &mut Request,
    string: &[u8],
    format: &[u8],
    first: u32,
    num_vars: usize,
) -> Result<Owner<Value>, Thrown> {
    let c_string = |bytes: &[u8]| bytes.split(|c| *c == 0).next().unwrap_or(&[]).to_vec();
    let (input, format) = (c_string(string), c_string(format));
    scan(req, &input, &format, first, num_vars)
}

/// The scan itself, over the NUL-free `input` and `format`.
#[allow(clippy::too_many_lines)]
fn scan(
    req: &mut Request,
    input: &[u8],
    format: &[u8],
    first: u32,
    num_vars: usize,
) -> Result<Owner<Value>, Thrown> {
    let total = match validate(format, num_vars) {
        Ok(total) => total,
        Err(Invalid::Value(message)) => return Err(throw_error(req, "ValueError", message)),
        Err(Invalid::BadChar(c)) => {
            let text = format!("Bad scan conversion character \"{}\"", char::from(c));
            return Err(throw_error(req, "ValueError", &text));
        }
    };
    let mut target = if num_vars > 0 {
        Target::Vars {
            first,
            count: num_vars,
        }
    } else {
        let mut array = PhpArray::new(req)?;
        for _ in 0..total {
            array.append(req, Owner::null())?;
        }
        Target::Array(array)
    };
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let mut obj_index = 0usize;
    let (mut s, mut f) = (0usize, 0usize);
    let mut nconversions = 0usize;
    let mut underflow = false;
    'scan: while at(format, f) != 0 {
        let mut ch = at(format, f);
        f += 1;
        let mut suppress = false;
        let mut noskip = false;
        let mut unsigned = false;
        if is_space(ch) {
            while is_space(at(input, s)) {
                s += 1;
            }
            continue;
        }
        let literal = if ch == b'%' {
            ch = at(format, f);
            f += 1;
            ch == b'%'
        } else {
            true
        };
        if literal {
            if at(input, s) == 0 {
                underflow = true;
                break;
            }
            let sch = at(input, s);
            s += 1;
            if ch != sch {
                break;
            }
            continue;
        }
        if ch == b'*' {
            suppress = true;
            ch = at(format, f);
            f += 1;
        } else if ch.is_ascii_digit() {
            let (value, end) = strtoul(format, f - 1);
            if at(format, end) == b'$' {
                f = end + 1;
                ch = at(format, f);
                f += 1;
                obj_index = usize::try_from(value).unwrap_or(1).saturating_sub(1);
            }
        }
        let mut width = 0u64;
        if ch.is_ascii_digit() {
            let (value, end) = strtoul(format, f - 1);
            width = value;
            f = end;
            ch = at(format, f);
            f += 1;
        }
        if matches!(ch, b'l' | b'L' | b'h') {
            ch = at(format, f);
            f += 1;
        }
        let (op, base) = match ch {
            b'n' => {
                if !suppress {
                    put(
                        req,
                        &mut target,
                        &mut obj_index,
                        Owner::int(i64::try_from(s).unwrap_or(0)),
                    )?;
                }
                nconversions += 1;
                continue;
            }
            b'd' | b'D' => (b'i', 10),
            b'i' => (b'i', 0),
            b'o' => (b'i', 8),
            b'x' | b'X' => (b'i', 16),
            b'u' => {
                unsigned = true;
                (b'i', 10)
            }
            b'f' | b'e' | b'E' | b'g' => (b'f', 0),
            b's' => (b's', 0),
            b'c' => {
                noskip = true;
                if width == 0 {
                    width = 1;
                }
                (b's', 0)
            }
            _ => {
                noskip = true;
                (b'[', 0)
            }
        };
        if at(input, s) == 0 {
            underflow = true;
            break;
        }
        if !noskip {
            while at(input, s) != 0 && is_space(at(input, s)) {
                s += 1;
            }
            if at(input, s) == 0 {
                underflow = true;
                break;
            }
        }
        match op {
            b's' => {
                let mut left = if width == 0 { u64::MAX } else { width };
                let mut end = s;
                while at(input, end) != 0 {
                    if is_space(at(input, end)) {
                        break;
                    }
                    end += 1;
                    left -= 1;
                    if left == 0 {
                        break;
                    }
                }
                if !suppress {
                    let value = PhpString::new(req, &input[s..end])?.into_value();
                    put(req, &mut target, &mut obj_index, value)?;
                }
                s = end;
            }
            b'[' => {
                let mut left = if width == 0 { u64::MAX } else { width };
                let (set, next) = build_char_set(format, f);
                f = next;
                let mut end = s;
                while at(input, end) != 0 {
                    if !set.contains(at(input, end)) {
                        break;
                    }
                    end += 1;
                    left -= 1;
                    if left == 0 {
                        break;
                    }
                }
                if s == end {
                    break 'scan;
                }
                if !suppress {
                    let value = PhpString::new(req, &input[s..end])?.into_value();
                    put(req, &mut target, &mut obj_index, value)?;
                }
                s = end;
            }
            b'i' => {
                let mut base = base;
                let mut left = if width == 0 || width > 63 { 63 } else { width };
                let (mut signok, mut nodigits, mut nozero, mut xok) = (true, true, true, false);
                let mut buf: Vec<u8> = Vec::new();
                while left > 0 {
                    let c = at(input, s);
                    let take = match c {
                        b'0' => {
                            if base == 16 {
                                xok = true;
                            }
                            if base == 0 {
                                base = 8;
                                xok = true;
                            }
                            if nozero {
                                signok = false;
                                nodigits = false;
                                nozero = false;
                            } else {
                                signok = false;
                                xok = false;
                                nodigits = false;
                            }
                            true
                        }
                        b'1'..=b'7' => {
                            if base == 0 {
                                base = 10;
                            }
                            signok = false;
                            xok = false;
                            nodigits = false;
                            true
                        }
                        b'8' | b'9' => {
                            if base == 0 {
                                base = 10;
                            }
                            if base <= 8 {
                                false
                            } else {
                                signok = false;
                                xok = false;
                                nodigits = false;
                                true
                            }
                        }
                        b'A'..=b'F' | b'a'..=b'f' => {
                            if base <= 10 {
                                false
                            } else {
                                signok = false;
                                xok = false;
                                nodigits = false;
                                true
                            }
                        }
                        b'+' | b'-' if signok => {
                            signok = false;
                            true
                        }
                        b'x' | b'X' if xok && buf.len() == 1 => {
                            base = 16;
                            xok = false;
                            true
                        }
                        _ => false,
                    };
                    if !take {
                        break;
                    }
                    buf.push(c);
                    s += 1;
                    if at(input, s) == 0 {
                        break;
                    }
                    left -= 1;
                }
                if nodigits {
                    if at(input, s) == 0 {
                        underflow = true;
                    }
                    break 'scan;
                } else if matches!(buf.last(), Some(b'x' | b'X')) {
                    buf.pop();
                    s -= 1;
                }
                if !suppress {
                    let value = strtol(&buf, base, unsigned);
                    let value = if unsigned && value < 0 {
                        let text = Decimal::unsigned(u64::from_ne_bytes(value.to_ne_bytes()));
                        PhpString::new(req, text.as_bytes())?.into_value()
                    } else {
                        Owner::int(value)
                    };
                    put(req, &mut target, &mut obj_index, value)?;
                }
            }
            _ => {
                let mut left = if width == 0 || width > 63 { 63 } else { width };
                let (mut signok, mut nodigits, mut ptok, mut expok) = (true, true, true, true);
                let mut buf: Vec<u8> = Vec::new();
                while left > 0 {
                    let c = at(input, s);
                    let take = match c {
                        b'0'..=b'9' => {
                            signok = false;
                            nodigits = false;
                            true
                        }
                        b'+' | b'-' if signok => {
                            signok = false;
                            true
                        }
                        b'.' if ptok => {
                            signok = false;
                            ptok = false;
                            true
                        }
                        b'e' | b'E' if !nodigits && expok => {
                            expok = false;
                            ptok = false;
                            signok = true;
                            nodigits = true;
                            true
                        }
                        _ => false,
                    };
                    if !take {
                        break;
                    }
                    buf.push(c);
                    s += 1;
                    if at(input, s) == 0 {
                        break;
                    }
                    left -= 1;
                }
                if nodigits {
                    if expok {
                        if at(input, s) == 0 {
                            underflow = true;
                        }
                        break 'scan;
                    }
                    let last = buf.pop();
                    s -= 1;
                    if !matches!(last, Some(b'e' | b'E')) {
                        buf.pop();
                        s -= 1;
                    }
                }
                if !suppress {
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let value = strtod(&text);
                    put(req, &mut target, &mut obj_index, Owner::float(value))?;
                }
            }
        }
        nconversions += 1;
    }
    if underflow && nconversions == 0 {
        if let Target::Array(array) = target {
            array.give_up(req);
        }
        let answer = if num_vars > 0 {
            Value::int(SCAN_ERROR_EOF)
        } else {
            Value::null()
        };
        return Ok(Owner::take(req, answer));
    }
    Ok(match target {
        Target::Vars { .. } => Owner::int(i64::try_from(nconversions).unwrap_or(0)),
        Target::Array(array) => array.into_value(),
    })
}

/// One conversion's value, written where it goes; a slot past the
/// variables given is php's `break` out of the conversion.
fn put(
    req: &mut Request,
    target: &mut Target,
    index: &mut usize,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    match target {
        Target::Vars { first, count } => {
            if *index >= *count {
                value.give_up(req);
                return Ok(());
            }
            let position = *first + u32::try_from(*index).unwrap_or(0);
            *index += 1;
            assign_at(req, position, value)
        }
        Target::Array(array) => {
            let key = Key::Int(i64::try_from(*index).unwrap_or(0));
            *index += 1;
            array.set(req, Owner::take(req, key), value)
        }
    }
}

/// `ZEND_TRY_ASSIGN_REF_*` on `args + objIndex`, the calling native's
/// argument at `position`: `value` written through the reference the
/// argument is, held to the types of the properties that reference is
/// typed by, and given up where it is not one. The positions are the
/// caller's, `sscanf`'s, `fscanf`'s or `SplFileObject::fscanf`'s.
fn assign_at(req: &mut Request, position: u32, value: Owner<Value>) -> Result<(), Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if let ValueRef::Reference(reference) = given.as_ref() {
        return reference.set(req, value);
    }
    value.give_up(req);
    Ok(())
}

/// `zend_strtod(buf, NULL)` over what the float scan gathered: the
/// longest prefix Rust's reader takes, which for the digits, point,
/// sign and exponent the scan admits is all of it.
pub(super) fn strtod(text: &str) -> f64 {
    let mut end = text.len();
    while end > 0 {
        if let Ok(value) = text[..end].parse::<f64>() {
            return value;
        }
        end -= 1;
    }
    0.0
}
