//! `php_fgetcsv` (`Z/ext/standard/file.c:1865`), the one CSV reader
//! behind `str_getcsv` here and `fgetcsv` in `file`,
//! with `php_csv_handle_escape_argument` beside it. The reader walks a
//! character at a time by `php_mblen` under the thread's `LC_CTYPE`, so
//! a delimiter, enclosure or escape byte inside a multibyte character
//! is part of that character, and skips leading space by `isspace(3)`
//! under the same locale.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::Passed;
use php_ext::attr::php_function;
use php_ext::coerce::is_space;
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString};

use crate::ffi::locale::Mblen;

/// `PHP_CSV_NO_ESCAPE`: the empty `$escape`.
pub(super) const NO_ESCAPE: Option<u8> = None;

/// `php_csv_handle_escape_argument(escape_str, arg_num)`: the escape
/// byte of the `$escape` the entry held at `position`, `None` for the
/// empty string, the 8.4 deprecation and `\` when the call gave none,
/// and the `ValueError` for more than one byte.
pub(super) fn escape_of(
    req: &mut Request,
    escape: Passed<PhpString>,
    position: u32,
) -> Result<Option<u8>, Thrown> {
    let Passed::Given(escape) = escape else {
        docref(
            req,
            Level::Deprecated,
            "the $escape parameter must be provided as its default value will change",
        );
        return Ok(Some(b'\\'));
    };
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
/// once a trailing `\n`, `\r` or `\r\n` is set aside, read off the first
/// bytes of its last two valid characters.
fn trailing(mblen: &mut Mblen, buf: &[u8]) -> usize {
    let mut last = [0u8; 2];
    let mut at = 0;
    while at < buf.len() {
        let inc = if buf[at] == 0 {
            1
        } else {
            mblen.len(&buf[at..])
        };
        match usize::try_from(inc) {
            Ok(0) => break,
            Ok(inc) => {
                last = [last[1], buf[at]];
                at += inc;
            }
            Err(_) => {
                mblen.reset();
                at += 1;
            }
        }
    }
    match last {
        [b'\r', b'\n'] => at - 2,
        [_, b'\n' | b'\r'] => at - 1,
        _ => at,
    }
}

/// `php_fgetcsv(stream, delimiter, enclosure, escape, len, buf)`
/// (`Z/ext/standard/file.c:1865`): the fields of the line in `first`,
/// or `None` for a line with nothing on it. Where an enclosed field runs
/// past the line's end, `more` is asked for the next line, php's
/// `php_stream_get_line(stream, NULL, 0, &new_len)`; `None` from it is
/// the stream at its end, and for `str_getcsv`, which has no stream,
/// the answer every time, so the field keeps what the line had.
///
/// `PHPAPI` in `file.h`: what `SplFileObject`'s CSV reading calls
/// directly, the stream's next lines behind `more`.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("a,\"b\nc\",d\n\n"); $t->rewind();
/// var_dump($t->fgetcsv(',', '"', ''), $t->fgetcsv(',', '"', ''));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(3) "b
/// c"
///   [2]=>
///   string(1) "d"
/// }
/// array(1) {
///   [0]=>
///   NULL
/// }
/// ```
#[allow(clippy::too_many_lines)]
pub fn fgetcsv(
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
    // `inc_len`: the character at `bptr` before `limit`, 0 at `limit`.
    let step = |mblen: &mut Mblen, buf: &[u8], bptr: usize, limit: usize| -> isize {
        match buf.get(bptr..limit) {
            Some([0, ..]) => 1,
            Some(rest) if !rest.is_empty() => mblen.len(rest),
            _ => 0,
        }
    };
    let mut mblen = Mblen::new();
    let mut buf = first.to_vec();
    let mut limit = trailing(&mut mblen, &buf);
    let mut line_end = limit;
    let mut values = Vec::new();
    let mut bptr = 0usize;
    let mut first_field = true;
    loop {
        let mut field: Vec<u8> = Vec::new();
        let mut inc_len = step(&mut mblen, &buf, bptr, limit);
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
        let enclosed = inc_len != 0 && at(&buf, bptr) == enclosure;
        let hunk = if enclosed {
            let mut state = 0u8;
            bptr += 1;
            let mut hunk = bptr;
            // 2A: the enclosed field. The first turn reads the
            // enclosure's `inc_len`, as php's does, so a line that ends
            // on the enclosure steps once past its end.
            loop {
                match inc_len {
                    0 => {
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
                        let Some(next) = more() else {
                            // The enclosure is unterminated.
                            if bptr > limit {
                                if hunk == bptr {
                                    hunk -= 1;
                                }
                                bptr -= 1;
                            }
                            break;
                        };
                        buf = next;
                        bptr = 0;
                        hunk = 0;
                        limit = trailing(&mut mblen, &buf);
                        line_end = limit;
                        state = 0;
                    }
                    ..=1 => {
                        if inc_len < 0 {
                            mblen.reset();
                        }
                        match state {
                            1 => {
                                bptr += 1;
                                state = 0;
                            }
                            2 => {
                                if at(&buf, bptr) != enclosure {
                                    field.extend_from_slice(&span(
                                        &buf,
                                        hunk,
                                        bptr.saturating_sub(1),
                                    ));
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
                    }
                    wide => {
                        let wide = usize::try_from(wide).unwrap_or(1);
                        match state {
                            2 => {
                                field.extend_from_slice(&span(&buf, hunk, bptr.saturating_sub(1)));
                                hunk = bptr;
                                break;
                            }
                            1 => {
                                bptr += wide;
                                field.extend_from_slice(&span(&buf, hunk, bptr));
                                hunk = bptr;
                                state = 0;
                            }
                            _ => bptr += wide,
                        }
                    }
                }
                inc_len = step(&mut mblen, &buf, bptr, limit);
            }
            hunk
        } else {
            // 2B: the bare field.
            bptr
        };
        // Up to the delimiter.
        loop {
            match inc_len {
                0 => break,
                ..=1 => {
                    if inc_len < 0 {
                        inc_len = 1;
                        mblen.reset();
                    }
                    if at(&buf, bptr) == delimiter {
                        break;
                    }
                }
                _ => {}
            }
            bptr += usize::try_from(inc_len).unwrap_or(1);
            inc_len = step(&mut mblen, &buf, bptr, limit);
        }
        field.extend_from_slice(&span(&buf, hunk, bptr));
        if enclosed {
            bptr += usize::try_from(inc_len).unwrap_or(0);
        } else {
            let end = trailing(&mut mblen, &field);
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
///
/// The walk is by `php_mblen`'s characters under the request's
/// `LC_CTYPE`: a Shift-JIS character that ends in the delimiter's byte
/// does not split the field.
///
/// ```php
/// foreach (['C', 'ja_JP.SJIS'] as $l) { setlocale(LC_CTYPE, $l); var_dump(array_map('bin2hex', str_getcsv("\x95\\,x", "\\", '"', ""))); }
/// ```
/// ```expect locale=ja_JP.SJIS
/// array(2) {
///   [0]=>
///   string(2) "95"
///   [1]=>
///   string(4) "2c78"
/// }
/// array(1) {
///   [0]=>
///   string(8) "955c2c78"
/// }
/// ```
#[php_function]
pub(super) fn str_getcsv(
    req: &mut Request,
    string: PhpString,
    #[php(default = ",")] separator: PhpString,
    #[php(default = "\"")] enclosure: PhpString,
    #[php(default = "\\")] escape: Passed<PhpString>,
) -> Result<Owner<PhpArray>, Thrown> {
    let delimiter = match separator.as_bytes() {
        [c] => *c,
        _ => return Err(arg_value_error(req, 2, "be a single character")),
    };
    let enclosure = match enclosure.as_bytes() {
        [c] => *c,
        _ => return Err(arg_value_error(req, 3, "be a single character")),
    };
    let escape = escape_of(req, escape, 4)?;
    let mut array = PhpArray::new(req)?;
    match fgetcsv(string.as_bytes(), delimiter, enclosure, escape, &mut || {
        None
    }) {
        None => array.append(req, Owner::null())?,
        Some(fields) => {
            for field in fields {
                let value = PhpString::new(req, &field)?.into_value();
                array.append(req, value)?;
            }
        }
    }
    Ok(array)
}
