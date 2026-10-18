//! `Z/ext/standard/pack.c`: `pack` and `unpack`, Perl's two as php
//! carries them, every format code of the file (`a A Z h H c C s S n v
//! i I l L N V q Q J P f g G d e E x X @`) with its repeater, `*` and
//! the named `unpack` keys. The machine is the pinned build's: 64-bit
//! `zend_long`, four-byte `int`, little-endian, so the machine-order
//! codes are the little-endian ones.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::string::Decimal;
use php_ext::abi::{OrFalse, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{atoi, to_double, to_long, to_string, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value};

/// `INT_MAX`, the bound every count of the file is held to.
const INT_MAX: i64 = 2_147_483_647;

/// `php_pack(val, size, endianness, output)`: the long's low `size`
/// bytes, in the order asked.
fn put_long(out: &mut [u8], value: i64, size: usize, big: bool) {
    let bytes = value.to_le_bytes();
    for i in 0..size {
        out[i] = if big { bytes[size - 1 - i] } else { bytes[i] };
    }
}

/// `pack(string $format, mixed ...$values): string`: the values packed
/// by the format's codes, each with its repeater, `*` for every value
/// left or a string's length, the whole string sized first and the
/// refusals php's `ValueError`s.
///
/// ```php
/// var_dump(bin2hex(pack("nvc*", 0x1234, 0x5678, 65, 66)), bin2hex(pack("NVJPqQ", 1, 2, 3, 4, -1, 5)), bin2hex(pack("a5A5Z5a*Z*", "ab", "cd", "efghij", "k", "l")), bin2hex(pack("H*h3H2", "abc1", "123", "f")), bin2hex(pack("CcSsLlIi", 255, -1, 65535, -2, 4294967295, -3, 7, -8)));
/// var_dump(bin2hex(pack("gGeEfd", 1.5, 1.5, 0.1, 0.1, -2.25, 1e100)), bin2hex(pack("x3X@5a", "q")), bin2hex(pack("C2X3C", 1, 2, 3)), bin2hex(pack("a", "long")), bin2hex(pack("H", "z")), bin2hex(pack("N", "12abc")), bin2hex(pack("x*")));
/// var_dump(bin2hex(pack("C", 1, 2)));
/// foreach (["y", "C2", "a", "N*a"] as $f) { try { pack($f, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// Warning: pack(): Type H: not enough characters in string in %s on line 2
/// string(12) "123478564142"
/// string(80) "000000010200000000000000000000030400000000000000ffffffffffffffff0500000000000000"
/// string(36) "6162000000636420202065666768006b6c00"
/// string(10) "abc12103f0"
/// string(44) "fffffffffefffffffffffdffffff07000000f8ffffff"
///
/// Warning: pack(): Type X: outside of string in %s on line 3
///
/// Warning: pack(): Type H: illegal hex digit z in %s on line 3
///
/// Warning: pack(): Type x: '*' ignored in %s on line 3
/// string(72) "0000c03f3fc000009a9999999999b93f3fb999999999999a000010c07dc39425ad49b254"
/// string(12) "000000000071"
/// string(2) "03"
/// string(2) "6c"
/// string(2) "00"
/// string(8) "0000000c"
/// string(2) "00"
///
/// Warning: pack(): 1 arguments unused in %s on line 4
/// string(2) "01"
/// Type y: unknown format code
/// Type C: too few arguments
/// Type a: not enough arguments
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// echo new ReflectionFunction('pack'), new ReflectionFunction('unpack');
/// ```
/// ```expect
/// Function [ <internal:standard> function pack ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $format ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function unpack ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $format ]
///     Parameter #1 [ <required> string $string ]
///     Parameter #2 [ <optional> int $offset = 0 ]
///   }
///   - Return [ array|false ]
/// }
/// ```
#[php_function]
fn pack(
    req: &mut Request,
    format: PhpString,
    values: Variadic<Value>,
) -> Result<Owner<PhpString>, Thrown> {
    let mut argv = values.0;
    let mut converted = Vec::new();
    let format = format.as_bytes().to_vec();
    let packed = packed(req, &format, &mut argv, &mut converted);
    for string in converted {
        string.give_up(req);
    }
    packed
}

/// `pack`'s three passes over `argv`. An argument a `*` count reads
/// whole is converted where it stands, as php's `try_convert_to_string`
/// converts the frame's slot, and its owner is pushed onto `converted`:
/// the string is the native's to give up once the passes are done,
/// whichever way they end, as the frame's slot is php's.
#[allow(clippy::too_many_lines)]
fn packed(
    req: &mut Request,
    format: &[u8],
    argv: &mut [Value],
    converted: &mut Vec<Owner<PhpString>>,
) -> Result<Owner<PhpString>, Thrown> {
    let num_args = argv.len();
    // Pass one: the codes and their counts.
    let mut codes: Vec<(u8, i64)> = Vec::new();
    let mut currentarg = 0usize;
    let mut i = 0;
    while i < format.len() {
        let code = format[i];
        i += 1;
        let mut arg: i64 = 1;
        if i < format.len() {
            let c = format[i];
            if c == b'*' {
                arg = -1;
                i += 1;
            } else if c.is_ascii_digit() {
                arg = i64::from(atoi(&format[i..]));
                while i < format.len() && format[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }
        let ch = char::from(code);
        match code {
            b'x' | b'X' | b'@' => {
                if arg < 0 {
                    docref(req, Level::Warning, &format!("Type {ch}: '*' ignored"));
                    arg = 1;
                }
            }
            b'a' | b'A' | b'Z' | b'h' | b'H' => {
                if currentarg >= num_args {
                    return Err(throw_error(
                        req,
                        "ValueError",
                        &format!("Type {ch}: not enough arguments"),
                    ));
                }
                if arg < 0 {
                    // `try_convert_to_string`: the value converted where
                    // it stands, so the packing pass reads the string.
                    let s = to_string(req, &argv[currentarg].deref())?;
                    arg = i64::try_from(s.len()).unwrap_or(INT_MAX);
                    argv[currentarg] = Value::string(*s);
                    converted.push(s);
                    if code == b'Z' {
                        arg += 1;
                    }
                }
                currentarg += 1;
            }
            b'q' | b'Q' | b'J' | b'P' | b'c' | b'C' | b's' | b'S' | b'i' | b'I' | b'l' | b'L'
            | b'n' | b'N' | b'v' | b'V' | b'f' | b'g' | b'G' | b'd' | b'e' | b'E' => {
                if arg < 0 {
                    arg = i64::try_from(num_args - currentarg).unwrap_or(0);
                }
                let current = i64::try_from(currentarg).unwrap_or(INT_MAX);
                if current > INT_MAX - arg || current + arg > i64::try_from(num_args).unwrap_or(0) {
                    return Err(throw_error(
                        req,
                        "ValueError",
                        &format!("Type {ch}: too few arguments"),
                    ));
                }
                currentarg += usize::try_from(arg).unwrap_or(0);
            }
            _ => {
                return Err(throw_error(
                    req,
                    "ValueError",
                    &format!("Type {ch}: unknown format code"),
                ));
            }
        }
        codes.push((code, arg));
    }
    if currentarg < num_args {
        docref(
            req,
            Level::Warning,
            &format!("{} arguments unused", num_args - currentarg),
        );
    }
    // Pass two: the size, `INC_OUTPUTPOS`'s overflow guard on each step.
    let (mut outputpos, mut outputsize) = (0i64, 0i64);
    for &(code, arg) in &codes {
        let ch = char::from(code);
        let width: i64 = match code {
            b'h' | b'H' => {
                let bytes = arg / 2 + arg % 2;
                if bytes < 0 || (INT_MAX - outputpos) < bytes {
                    return Err(overflow(req, ch));
                }
                outputpos += bytes;
                0
            }
            b'a' | b'A' | b'Z' | b'c' | b'C' | b'x' => 1,
            b's' | b'S' | b'n' | b'v' => 2,
            b'i' | b'I' | b'l' | b'L' | b'N' | b'V' | b'f' | b'g' | b'G' => 4,
            b'q' | b'Q' | b'J' | b'P' | b'd' | b'e' | b'E' => 8,
            b'X' => {
                outputpos -= arg;
                if outputpos < 0 {
                    docref(
                        req,
                        Level::Warning,
                        &format!("Type {ch}: outside of string"),
                    );
                    outputpos = 0;
                }
                0
            }
            _ => {
                outputpos = arg;
                0
            }
        };
        if width > 0 {
            if arg < 0 || (INT_MAX - outputpos) / width < arg {
                return Err(overflow(req, ch));
            }
            outputpos += arg * width;
        }
        outputsize = outputsize.max(outputpos);
    }
    // Pass three: the bytes.
    let mut out = vec![0u8; usize::try_from(outputsize).unwrap_or(0)];
    let mut pos = 0usize;
    let mut current = 0usize;
    let next_value = |current: &mut usize| {
        let value = argv[*current].deref();
        *current += 1;
        value
    };
    for &(code, arg) in &codes {
        let ch = char::from(code);
        let count = usize::try_from(arg).unwrap_or(0);
        match code {
            b'a' | b'A' | b'Z' => {
                let copy = if code == b'Z' {
                    count.saturating_sub(1)
                } else {
                    count
                };
                let value = next_value(&mut current);
                let pad = if code == b'A' { b' ' } else { 0 };
                let field = &mut out[pos..pos + count];
                with_string(req, &value, |s| {
                    field.fill(pad);
                    let n = s.len().min(copy);
                    field[..n].copy_from_slice(&s[..n]);
                })?;
                pos += count;
            }
            b'h' | b'H' => {
                let value = next_value(&mut current);
                let digits = with_string(req, &value, <[u8]>::to_vec)?;
                let mut left = count;
                if left > digits.len() {
                    docref(
                        req,
                        Level::Warning,
                        &format!("Type {ch}: not enough characters in string"),
                    );
                    left = digits.len();
                }
                let mut shift = if code == b'h' { 0 } else { 4 };
                let mut first = true;
                // php starts one byte back and steps on at each pair.
                let mut at = pos.wrapping_sub(1);
                for &n in &digits[..left] {
                    let nibble = match n {
                        b'0'..=b'9' => n - b'0',
                        b'A'..=b'F' => n - b'A' + 10,
                        b'a'..=b'f' => n - b'a' + 10,
                        _ => {
                            docref(
                                req,
                                Level::Warning,
                                &format!("Type {ch}: illegal hex digit {}", char::from(n)),
                            );
                            0
                        }
                    };
                    if first {
                        at = at.wrapping_add(1);
                        out[at] = 0;
                        first = false;
                    } else {
                        first = true;
                    }
                    out[at] |= nibble << shift;
                    shift = (shift + 4) & 7;
                }
                pos = at.wrapping_add(1);
            }
            b'c' | b'C' | b's' | b'S' | b'n' | b'v' | b'i' | b'I' | b'l' | b'L' | b'N' | b'V'
            | b'q' | b'Q' | b'J' | b'P' => {
                let (size, big) = match code {
                    b'c' | b'C' => (1, false),
                    b's' | b'S' | b'v' => (2, false),
                    b'n' => (2, true),
                    b'i' | b'I' | b'l' | b'L' | b'V' => (4, false),
                    b'N' => (4, true),
                    b'J' => (8, true),
                    _ => (8, false),
                };
                for _ in 0..count {
                    let value = next_value(&mut current);
                    let n = to_long(req, &value)?;
                    put_long(&mut out[pos..], n, size, big);
                    pos += size;
                }
            }
            b'f' | b'g' | b'G' => {
                for _ in 0..count {
                    let value = next_value(&mut current);
                    // `(float) zval_get_double`.
                    #[allow(clippy::cast_possible_truncation)]
                    let f = to_double(req, &value)? as f32;
                    let bytes = if code == b'G' {
                        f.to_be_bytes()
                    } else {
                        f.to_le_bytes()
                    };
                    out[pos..pos + 4].copy_from_slice(&bytes);
                    pos += 4;
                }
            }
            b'd' | b'e' | b'E' => {
                for _ in 0..count {
                    let value = next_value(&mut current);
                    let d = to_double(req, &value)?;
                    let bytes = if code == b'E' {
                        d.to_be_bytes()
                    } else {
                        d.to_le_bytes()
                    };
                    out[pos..pos + 8].copy_from_slice(&bytes);
                    pos += 8;
                }
            }
            b'x' => {
                out[pos..pos + count].fill(0);
                pos += count;
            }
            b'X' => pos = pos.saturating_sub(count),
            _ => {
                if count > pos {
                    out[pos..count].fill(0);
                }
                pos = count;
            }
        }
    }
    out.truncate(pos);
    PhpString::new(req, &out)
}

/// `INC_OUTPUTPOS`'s refusal.
fn overflow(req: &mut Request, code: char) -> Thrown {
    throw_error(
        req,
        "ValueError",
        &format!("Type {code}: integer overflow in format string"),
    )
}

/// `unpack(string $format, string $string, int $offset = 0):
/// array|false`: the format's parts, `/` between, each a code, a
/// repeater or `*`, and the name its values are keyed by, numbered
/// from 1 where the part repeats; a part that wants more than is left
/// is php's warning and `false`, and `*` stops where the input does.
///
/// ```php
/// var_dump(unpack("nfirst/vsecond/c2chars", "\x12\x34\x78\x56\x41\xff"), unpack("C*", "abc"), unpack("a3x/A5y/Z*z", "ab cd   ef\0gh"), unpack("H*hex/h2low", "\xab\xcd"), unpack("q", pack("q", -5)), unpack("J", "\x00\x00\x00\x00\x00\x00\x01\x00"), unpack("N", "\xff\xff\xff\xff"), unpack("l", "\xff\xff\xff\xff"));
/// var_dump(unpack("g", pack("g", 1.5)), unpack("E", pack("E", 0.1)), unpack("Cx/@3/Cy", "\x01\x02\x03\x04"), unpack("C", "abc", 2), unpack("N", "ab"), unpack("a*", ""), unpack("C2/C", "\x01\x02\x03"), unpack("C0", "a"));
/// try { unpack("y", "a"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { unpack("C", "a", 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(4) {
///   ["first"]=>
///   int(4660)
///   ["second"]=>
///   int(22136)
///   ["chars1"]=>
///   int(65)
///   ["chars2"]=>
///   int(-1)
/// }
/// array(3) {
///   [1]=>
///   int(97)
///   [2]=>
///   int(98)
///   [3]=>
///   int(99)
/// }
/// array(3) {
///   ["x"]=>
///   string(3) "ab "
///   ["y"]=>
///   string(2) "cd"
///   ["z"]=>
///   string(2) "ef"
/// }
/// array(2) {
///   ["hex"]=>
///   string(4) "abcd"
///   ["low"]=>
///   string(2) "ba"
/// }
/// array(1) {
///   [1]=>
///   int(-5)
/// }
/// array(1) {
///   [1]=>
///   int(256)
/// }
/// array(1) {
///   [1]=>
///   int(4294967295)
/// }
/// array(1) {
///   [1]=>
///   int(-1)
/// }
///
/// Warning: unpack(): Type N: not enough input values, need 4 values but only 2 were provided in %s on line 3
/// array(1) {
///   [1]=>
///   float(1.5)
/// }
/// array(1) {
///   [1]=>
///   float(0.1)
/// }
/// array(2) {
///   ["x"]=>
///   int(1)
///   ["y"]=>
///   int(4)
/// }
/// array(1) {
///   [1]=>
///   int(99)
/// }
/// bool(false)
/// array(1) {
///   [1]=>
///   string(0) ""
/// }
/// array(2) {
///   [1]=>
///   int(3)
///   [2]=>
///   int(2)
/// }
/// array(0) {
/// }
/// Invalid format type y
/// unpack(): Argument #3 ($offset) must be contained in argument #2 ($data)
/// ```
#[allow(clippy::too_many_lines)]
#[php_function]
fn unpack(
    req: &mut Request,
    format: PhpString,
    string: PhpString,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let input = string;
    let full = input.as_bytes();
    if offset < 0 || offset > i64::try_from(full.len()).unwrap_or(i64::MAX) {
        return Err(arg_value_error(
            req,
            3,
            "be contained in argument #2 ($data)",
        ));
    }
    let input = &full[usize::try_from(offset).unwrap_or(0)..];
    let inputlen = i64::try_from(input.len()).unwrap_or(i64::MAX);
    let format = format.as_bytes().to_vec();
    let mut array = PhpArray::new(req)?;
    let mut f = 0usize;
    let mut inputpos: i64 = 0;
    while f < format.len() {
        let ty = format[f];
        f += 1;
        let ch = char::from(ty);
        let mut repetitions: i64 = 1;
        if f < format.len() {
            let c = format[f];
            if c.is_ascii_digit() {
                let digits = format[f..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit())
                    .count();
                let parsed = std::str::from_utf8(&format[f..f + digits])
                    .ok()
                    .and_then(|t| t.parse::<i64>().ok())
                    .filter(|v| *v <= INT_MAX);
                let Some(parsed) = parsed else {
                    docref(req, Level::Warning, &format!("Type {ch}: integer overflow"));
                    array.give_up(req);
                    return Ok(OrFalse::False);
                };
                repetitions = parsed;
                f += digits;
            } else if c == b'*' {
                repetitions = -1;
                f += 1;
            }
        }
        let name_start = f;
        let argb = repetitions;
        while f < format.len() && format[f] != b'/' {
            f += 1;
        }
        let name = format[name_start..f.min(name_start + 200)].to_vec();
        let mut size: i64 = match ty {
            b'X' => {
                if repetitions < 0 {
                    docref(req, Level::Warning, &format!("Type {ch}: '*' ignored"));
                    repetitions = 1;
                }
                -1
            }
            b'@' => 0,
            b'a' | b'A' | b'Z' => {
                let size = repetitions;
                repetitions = 1;
                size
            }
            b'h' | b'H' => {
                let size = if repetitions > 0 {
                    (repetitions + 1) / 2
                } else {
                    repetitions
                };
                repetitions = 1;
                size
            }
            b'c' | b'C' | b'x' => 1,
            b's' | b'S' | b'n' | b'v' => 2,
            b'i' | b'I' | b'l' | b'L' | b'N' | b'V' | b'f' | b'g' | b'G' => 4,
            b'q' | b'Q' | b'J' | b'P' | b'd' | b'e' | b'E' => 8,
            _ => {
                array.give_up(req);
                return Err(throw_error(
                    req,
                    "ValueError",
                    &format!("Invalid format type {ch}"),
                ));
            }
        };
        let mut i: i64 = 0;
        while i != repetitions {
            if size != 0 && size != -1 && INT_MAX - size + 1 < inputpos {
                docref(req, Level::Warning, &format!("Type {ch}: integer overflow"));
                array.give_up(req);
                return Ok(OrFalse::False);
            }
            if inputpos + size > inputlen {
                if repetitions < 0 {
                    break;
                }
                let left = inputlen - inputpos;
                let verb = if left == 1 { "was" } else { "were" };
                docref(
                    req,
                    Level::Warning,
                    &format!(
                        "Type {ch}: not enough input values, need {size} values but only {left} {verb} provided"
                    ),
                );
                array.give_up(req);
                return Ok(OrFalse::False);
            }
            let at = usize::try_from(inputpos).unwrap_or(0);
            let bytes = |n: usize| &input[at..at + n];
            let rest = inputlen - inputpos;
            let value: Option<Owner<Value>> = match ty {
                b'a' | b'A' | b'Z' => {
                    let mut len = if size >= 0 && rest > size { size } else { rest };
                    size = len;
                    let span = bytes(usize::try_from(len).unwrap_or(0));
                    let kept: &[u8] = match ty {
                        b'a' => span,
                        b'A' => {
                            while len > 0
                                && matches!(
                                    span[usize::try_from(len - 1).unwrap_or(0)],
                                    0 | b' ' | b'\t' | b'\r' | b'\n'
                                )
                            {
                                len -= 1;
                            }
                            &span[..usize::try_from(len).unwrap_or(0)]
                        }
                        _ => php_ext::coerce::c_str(span),
                    };
                    Some(PhpString::new(req, kept)?.into_value())
                }
                b'h' | b'H' => {
                    if size > INT_MAX / 2 {
                        array.give_up(req);
                        return Err(argument_error(
                            req,
                            "ValueError",
                            1,
                            &format!("repeater must be less than or equal to {}", INT_MAX / 2),
                        ));
                    }
                    let mut len = rest * 2;
                    if size >= 0 && len > size * 2 {
                        len = size * 2;
                    }
                    if len > 0 && argb > 0 {
                        len -= argb % 2;
                    }
                    let mut shift = if ty == b'h' { 0 } else { 4 };
                    let mut first = true;
                    let mut ipos = at;
                    let mut hex = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
                    for _ in 0..len {
                        let nibble = (input.get(ipos).copied().unwrap_or(0) >> shift) & 0xF;
                        hex.push(if nibble < 10 {
                            b'0' + nibble
                        } else {
                            b'a' + nibble - 10
                        });
                        shift = (shift + 4) & 7;
                        if first {
                            first = false;
                        } else {
                            ipos += 1;
                            first = true;
                        }
                    }
                    Some(PhpString::new(req, &hex)?.into_value())
                }
                b'c' => Some(Owner::int(i64::from(i8::from_ne_bytes([input[at]])))),
                b'C' => Some(Owner::int(i64::from(input[at]))),
                b's' | b'S' | b'n' | b'v' => {
                    let pair = [input[at], input[at + 1]];
                    Some(Owner::int(match ty {
                        b's' => i64::from(i16::from_le_bytes(pair)),
                        b'n' => i64::from(u16::from_be_bytes(pair)),
                        _ => i64::from(u16::from_le_bytes(pair)),
                    }))
                }
                b'i' | b'I' | b'l' | b'L' | b'N' | b'V' => {
                    let word = [input[at], input[at + 1], input[at + 2], input[at + 3]];
                    Some(Owner::int(match ty {
                        b'i' | b'l' => i64::from(i32::from_le_bytes(word)),
                        b'N' => i64::from(u32::from_be_bytes(word)),
                        _ => i64::from(u32::from_le_bytes(word)),
                    }))
                }
                b'q' | b'Q' | b'J' | b'P' => {
                    let mut word = [0u8; 8];
                    word.copy_from_slice(bytes(8));
                    Some(Owner::int(if ty == b'J' {
                        i64::from_be_bytes(word)
                    } else {
                        i64::from_le_bytes(word)
                    }))
                }
                b'f' | b'g' | b'G' => {
                    let word = [input[at], input[at + 1], input[at + 2], input[at + 3]];
                    let v = if ty == b'G' {
                        f32::from_be_bytes(word)
                    } else {
                        f32::from_le_bytes(word)
                    };
                    Some(Owner::float(f64::from(v)))
                }
                b'd' | b'e' | b'E' => {
                    let mut word = [0u8; 8];
                    word.copy_from_slice(bytes(8));
                    Some(Owner::float(if ty == b'E' {
                        f64::from_be_bytes(word)
                    } else {
                        f64::from_le_bytes(word)
                    }))
                }
                b'x' => None,
                b'X' => {
                    if inputpos < size {
                        inputpos = -size;
                        i = repetitions - 1;
                        if repetitions >= 0 {
                            docref(
                                req,
                                Level::Warning,
                                &format!("Type {ch}: outside of string"),
                            );
                        }
                    }
                    None
                }
                _ => {
                    if repetitions <= inputlen {
                        inputpos = repetitions;
                    } else {
                        docref(
                            req,
                            Level::Warning,
                            &format!("Type {ch}: outside of string"),
                        );
                    }
                    i = repetitions - 1;
                    None
                }
            };
            if let Some(value) = value {
                let key = if name.is_empty() {
                    Owner::index(i + 1)
                } else if repetitions == 1 {
                    let name = PhpString::new(req, &name)?;
                    Key::from_string(req, name)
                } else {
                    let mut numbered = name.clone();
                    numbered.extend_from_slice(Decimal::of(i + 1).as_bytes());
                    let numbered = PhpString::new(req, &numbered)?;
                    Key::from_string(req, numbered)
                };
                array.set(req, key, value)?;
            }
            inputpos += size;
            if inputpos < 0 {
                if size != -1 {
                    docref(
                        req,
                        Level::Warning,
                        &format!("Type {ch}: outside of string"),
                    );
                }
                inputpos = 0;
            }
            i += 1;
        }
        if f < format.len() {
            f += 1;
        }
    }
    Ok(OrFalse::Value(array))
}
