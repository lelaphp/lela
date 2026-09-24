//! `php_fgetcsv` (`Z/ext/standard/file.c:1865`), the one CSV reader
//! behind `str_getcsv` here and `fgetcsv` in `file`,
//! with `php_csv_handle_escape_argument` beside it. The reader walks a
//! byte at a time: the delimiter, the enclosure and the escape are
//! single bytes, and under the C locale `php_mblen` never joins one of
//! them into a longer character.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value};

use crate::zpp::{given, optional_string, string_value};
use crate::{argument_count_between, string_argument};

/// `PHP_CSV_NO_ESCAPE`: the empty `$escape`.
pub(super) const NO_ESCAPE: Option<u8> = None;

/// `php_csv_handle_escape_argument(escape_str, arg_num)`: the escape
/// byte, `None` for the empty string, the 8.4 deprecation and `\` when
/// the call gave none, and the `ValueError` for more than one byte.
pub(super) fn escape_argument(req: &mut Request, position: u32) -> Result<Option<u8>, Thrown> {
    if given(req, position).is_none() {
        docref(
            req,
            Level::Deprecated,
            "the $escape parameter must be provided as its default value will change",
        );
        return Ok(Some(b'\\'));
    }
    let escape = string_argument(req, position)?;
    match escape.as_bytes() {
        [] => Ok(NO_ESCAPE),
        [c] => Ok(Some(*c)),
        _ => Err(arg_value_error(
            req,
            position,
            "be empty or a single character",
        )),
    }
}

/// `php_fgetcsv_lookup_trailing_spaces(ptr, len)`: where the line ends
/// once a trailing `\n`, `\r` or `\r\n` is set aside.
fn trailing(buf: &[u8], len: usize) -> usize {
    match buf[..len] {
        [.., b'\r', b'\n'] => len - 2,
        [.., b'\n' | b'\r'] => len - 1,
        _ => len,
    }
}

/// C's `isspace` under the C locale.
const fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `php_fgetcsv(stream, delimiter, enclosure, escape, len, buf)`
/// (`Z/ext/standard/file.c:1865`): the fields of the line in `first`,
/// or `None` for a line with nothing on it. Where an enclosed field runs
/// past the line's end, `more` is asked for the next line, php's
/// `php_stream_get_line(stream, NULL, 0, &new_len)`; `None` from it is
/// the stream at its end, and for `str_getcsv`, which has no stream,
/// the answer every time, so the field keeps what the line had.
#[allow(clippy::too_many_lines)]
pub(super) fn fgetcsv(
    first: &[u8],
    delimiter: u8,
    enclosure: u8,
    escape: Option<u8>,
    more: &mut dyn FnMut() -> Option<Vec<u8>>,
) -> Option<Vec<Vec<u8>>> {
    // `buf` is read as php reads its NUL-terminated copy.
    let at = |buf: &[u8], i: usize| buf.get(i).copied().unwrap_or(0);
    // A copy reads the terminator too, as php's `memcpy` does where an
    // escape stepped onto it.
    let span = |buf: &[u8], from: usize, to: usize| -> Vec<u8> {
        (from..to).map(|i| at(buf, i)).collect()
    };
    let mut buf = first.to_vec();
    let mut limit = trailing(&buf, buf.len());
    let mut line_end = limit;
    let mut values = Vec::new();
    let mut bptr = 0usize;
    let mut first_field = true;
    loop {
        let step = |bptr: usize, limit: usize| usize::from(bptr < limit);
        let mut field: Vec<u8> = Vec::new();
        let mut inc_len = step(bptr, limit);
        if inc_len == 1 {
            let mut tmp = bptr;
            while at(&buf, tmp) != delimiter && is_space(at(&buf, tmp)) {
                tmp += 1;
            }
            if at(&buf, tmp) == enclosure && tmp < limit {
                bptr = tmp;
            }
        }
        if first_field && bptr == line_end {
            return None;
        }
        first_field = false;
        if inc_len != 0 && at(&buf, bptr) == enclosure {
            let mut state = 0u8;
            bptr += 1;
            let mut hunk = bptr;
            // 2A: the enclosed field. The first turn reads the
            // enclosure's `inc_len`, as php's does, so a line that ends
            // on the enclosure steps once past its end.
            loop {
                if inc_len == 0 {
                    if state == 2 {
                        field.extend_from_slice(&span(&buf, hunk, bptr.saturating_sub(1)));
                        hunk = bptr;
                        break;
                    }
                    if state == 1 {
                        field.extend_from_slice(&span(&buf, hunk, bptr));
                        hunk = bptr;
                    }
                    if hunk != line_end {
                        field.extend_from_slice(&span(&buf, hunk, bptr));
                        hunk = bptr;
                    }
                    field.extend_from_slice(buf.get(line_end..).unwrap_or_default());
                    if let Some(next) = more() {
                        buf = next;
                        bptr = 0;
                        hunk = 0;
                        limit = trailing(&buf, buf.len());
                        line_end = limit;
                        state = 0;
                        inc_len = step(bptr, limit);
                        continue;
                    }
                    // The enclosure is unterminated.
                    if bptr > limit {
                        if hunk == bptr {
                            hunk -= 1;
                        }
                        bptr -= 1;
                    }
                    break;
                }
                match state {
                    1 => {
                        bptr += 1;
                        state = 0;
                    }
                    2 => {
                        if at(&buf, bptr) != enclosure {
                            field.extend_from_slice(&span(&buf, hunk, bptr.saturating_sub(1)));
                            hunk = bptr;
                            break;
                        }
                        field.extend_from_slice(&span(&buf, hunk, bptr));
                        bptr += 1;
                        hunk = bptr;
                        state = 0;
                    }
                    _ => {
                        if at(&buf, bptr) == enclosure {
                            state = 2;
                        } else if escape.is_some_and(|e| at(&buf, bptr) == e) {
                            state = 1;
                        }
                        bptr += 1;
                    }
                }
                inc_len = step(bptr, limit);
            }
            // Up to the delimiter.
            loop {
                if inc_len == 0 || at(&buf, bptr) == delimiter {
                    break;
                }
                bptr += 1;
                inc_len = step(bptr, limit);
            }
            field.extend_from_slice(&span(&buf, hunk, bptr));
            bptr += inc_len;
        } else {
            // 2B: the bare field.
            let hunk = bptr;
            while inc_len != 0 && at(&buf, bptr) != delimiter {
                bptr += 1;
                inc_len = step(bptr, limit);
            }
            field.extend_from_slice(&span(&buf, hunk, bptr));
            let end = trailing(&field, field.len());
            field.truncate(end);
            if at(&buf, bptr) == delimiter {
                bptr += 1;
            }
        }
        values.push(field);
        if inc_len == 0 {
            break;
        }
    }
    Some(values)
}

/// `str_getcsv(string $string, string $separator = ",", string
/// $enclosure = "\"", string $escape = "\\"): array`: the one line's
/// fields, and the one `null` of `php_bc_fgetcsv_empty_line` for an
/// empty line.
///
/// ```php
/// var_dump(str_getcsv('a,"b ""c""",d', ",", '"', ""), str_getcsv("x;'y;z'", ";", "'", "\\"), str_getcsv("", ",", '"', ""), str_getcsv('a,"b\\"c",d', ",", '"', "\\"), str_getcsv("  \"q\" ,r\n", ",", '"', ""), str_getcsv('"unterminated', ",", '"', ""), str_getcsv("a, b ,c\r\n", ",", '"', ""));
/// var_dump(str_getcsv("a,b"));
/// foreach ([["a", ""], ["a", ",", "xx"], ["a", ",", '"', "ab"]] as $args) { try { call_user_func_array("str_getcsv", $args); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(5) "b "c""
///   [2]=>
///   string(1) "d"
/// }
/// array(2) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(3) "y;z"
/// }
/// array(1) {
///   [0]=>
///   NULL
/// }
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(4) "b\"c"
///   [2]=>
///   string(1) "d"
/// }
/// array(2) {
///   [0]=>
///   string(2) "q "
///   [1]=>
///   string(1) "r"
/// }
/// array(1) {
///   [0]=>
///   string(12) "unterminated"
/// }
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(3) " b "
///   [2]=>
///   string(1) "c"
/// }
///
/// Deprecated: str_getcsv(): the $escape parameter must be provided as its default value will change in %s on line 3
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// str_getcsv(): Argument #2 ($separator) must be a single character
/// str_getcsv(): Argument #3 ($enclosure) must be a single character
/// str_getcsv(): Argument #4 ($escape) must be empty or a single character
/// ```
pub(super) fn str_getcsv(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 4)?;
    let string = string_argument(req, 1)?;
    let delimiter = optional_string(req, 2)?;
    let enclosure = optional_string(req, 3)?;
    let delimiter = match delimiter.as_ref().map(PhpString::as_bytes) {
        None => b',',
        Some([c]) => *c,
        Some(_) => return Err(arg_value_error(req, 2, "be a single character")),
    };
    let enclosure = match enclosure.as_ref().map(PhpString::as_bytes) {
        None => b'"',
        Some([c]) => *c,
        Some(_) => return Err(arg_value_error(req, 3, "be a single character")),
    };
    let escape = escape_argument(req, 4)?;
    let mut array = PhpArray::new(req)?;
    match fgetcsv(string.as_bytes(), delimiter, enclosure, escape, &mut || {
        None
    }) {
        None => array.append(req, Value::null())?,
        Some(fields) => {
            for field in fields {
                let value = string_value(req, &field)?;
                array.append(req, value)?;
            }
        }
    }
    Ok(Value::array(array))
}
