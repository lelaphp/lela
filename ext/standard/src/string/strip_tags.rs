//! `strip_tags` over `php_strip_tags_ex` (`Z/ext/standard/string.c:5118`),
//! php's five-state machine copied state for state: 0 the text, 1 inside
//! an HTML tag, 2 inside a PHP tag, 3 inside a `<!` declaration, 4
//! inside a comment; with the quote, bracket and nesting counters and
//! the tag buffer an allowed-tags set is matched against by
//! `php_tag_find`.

use std::ops::ControlFlow;

use php_ext::abi::Union;
use php_ext::attr::php_function;
use php_ext::coerce::{is_space, with_string};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpString};

/// `strip_tags(string $string, array|string|null $allowed_tags = null):
/// string`: the allowed set as the string, or each array element
/// wrapped in `<` and `>`.
///
/// ```php
/// $t = '<p>Test paragraph.</p><!-- Comment --> <a href="#fragment">Other text</a>';
/// var_dump(strip_tags($t), strip_tags($t, '<p><a>'), strip_tags($t, ['p']), strip_tags("a < b > c"), strip_tags("<?php echo 1; ?>x<? y ?>z"), strip_tags("<br/>a<br />b", "<br>"), strip_tags('<a title=">">x</a>'), strip_tags("<!DOCTYPE html><b>x</b>"), strip_tags("<?xml version='1.0'?><b>y</b>"), strip_tags("1 <2"), strip_tags("<<b>>x"), strip_tags("a\0b"));
/// ```
/// ```expect
/// string(26) "Test paragraph. Other text"
/// string(57) "<p>Test paragraph.</p> <a href="#fragment">Other text</a>"
/// string(33) "<p>Test paragraph.</p> Other text"
/// string(9) "a < b > c"
/// string(2) "xz"
/// string(13) "<br/>a<br />b"
/// string(1) "x"
/// string(1) "x"
/// string(1) "y"
/// string(2) "1 "
/// string(1) "x"
/// string(2) "ab"
/// ```
///
/// A `<` before C's `isspace` under the request's `LC_CTYPE` opens no tag,
/// and macOS's `C.UTF-8`, php's default, counts 0xA0 among it.
///
/// ```php
/// var_dump(bin2hex(strip_tags("a<\xa0b>c")));
/// setlocale(LC_CTYPE, "C"); var_dump(bin2hex(strip_tags("a<\xa0b>c")));
/// ```
/// ```expect
/// string(12) "613ca0623e63"
/// string(4) "6163"
/// ```
#[php_function]
pub(super) fn strip_tags(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] allowed_tags: Option<Union<PhpArray, PhpString>>,
) -> Result<Owner<PhpString>, Thrown> {
    let allow = allowed_tags;
    let allowed: Option<Vec<u8>> = match allow {
        None => None,
        Some(Union::B(s)) => Some(s.as_bytes().to_vec()),
        Some(Union::A(array)) => {
            let mut set = Vec::new();
            // `ZEND_HASH_FOREACH_VAL(allow_ht, tmp)`
            // (`Z/ext/standard/string.c:4842`), no owner of the table
            // taken: the argument holds it while `__toString` runs.
            walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
                with_string(req, row.value(), |tag| {
                    set.push(b'<');
                    set.extend_from_slice(tag);
                    set.push(b'>');
                })?;
                Ok(ControlFlow::Continue(()))
            })?;
            if set.is_empty() { None } else { Some(set) }
        }
    };
    let out = strip_tags_ex(string.as_bytes(), allowed.as_deref(), false);
    PhpString::new(req, &out)
}

/// `php_strip_tags_ex(rbuf, len, allow, allow_len, allow_tag_spaces)`,
/// `PHPAPI` in `php_string.h`: ext/filter's `FILTER_SANITIZE_STRING`
/// strips with `allow_tag_spaces` set, so `< b>` opens a tag there where
/// `strip_tags()` keeps it as text.
///
/// ```php
/// var_dump(strip_tags("a< b>c"), filter_var("a< b>c", FILTER_UNSAFE_RAW) === "a< b>c", @filter_var("a< b>c", FILTER_SANITIZE_STRING));
/// ```
/// ```expect
/// string(6) "a< b>c"
/// bool(true)
/// string(2) "ac"
/// ```
#[allow(clippy::too_many_lines)]
pub fn strip_tags_ex(buf: &[u8], allow: Option<&[u8]>, allow_tag_spaces: bool) -> Vec<u8> {
    let allow: Option<Vec<u8>> = allow.map(|a| a.iter().map(u8::to_ascii_lowercase).collect());
    let at = |i: usize| buf.get(i).copied().unwrap_or(0);
    let mut out = Vec::with_capacity(buf.len());
    let mut tag: Vec<u8> = Vec::new();
    let (mut lc, mut br, mut depth, mut in_q, mut is_xml) = (0u8, 0i32, 0i32, 0u8, false);
    let mut state = 0u8;
    let mut p = 0usize;
    while p < buf.len() {
        let c = buf[p];
        match state {
            0 => match c {
                0 => {}
                b'<' => {
                    if in_q != 0 {
                    } else if is_space(at(p + 1)) && !allow_tag_spaces {
                        out.push(c);
                    } else {
                        lc = b'<';
                        state = 1;
                        if allow.is_some() {
                            tag.push(b'<');
                        }
                    }
                }
                b'>' => {
                    if depth != 0 {
                        depth -= 1;
                    } else if in_q == 0 {
                        out.push(c);
                    }
                }
                _ => out.push(c),
            },
            1 => match c {
                0 => {}
                b'<' => {
                    if in_q != 0 {
                    } else if is_space(at(p + 1)) && !allow_tag_spaces {
                        if allow.is_some() {
                            tag.push(c);
                        }
                    } else {
                        depth += 1;
                    }
                }
                b'>' => {
                    if depth != 0 {
                        depth -= 1;
                    } else if in_q != 0 {
                    } else {
                        lc = b'>';
                        if !(is_xml && p >= 1 && buf[p - 1] == b'-') {
                            in_q = 0;
                            state = 0;
                            is_xml = false;
                            if let Some(set) = &allow {
                                tag.push(b'>');
                                if tag_find(&tag, set) {
                                    out.extend_from_slice(&tag);
                                }
                                tag.clear();
                            }
                        }
                    }
                }
                b'"' | b'\'' => {
                    if p != 0 && (in_q == 0 || c == in_q) {
                        in_q = if in_q != 0 { 0 } else { c };
                    }
                    if allow.is_some() {
                        tag.push(c);
                    }
                }
                b'!' if p >= 1 && buf[p - 1] == b'<' => {
                    state = 3;
                    lc = c;
                }
                b'?' if p >= 1 && buf[p - 1] == b'<' => {
                    br = 0;
                    state = 2;
                }
                _ => {
                    if allow.is_some() {
                        tag.push(c);
                    }
                }
            },
            2 => match c {
                b'(' if lc != b'"' && lc != b'\'' => {
                    lc = b'(';
                    br += 1;
                }
                b')' if lc != b'"' && lc != b'\'' => {
                    lc = b')';
                    br -= 1;
                }
                b'>' => {
                    if depth != 0 {
                        depth -= 1;
                    } else if in_q != 0 {
                    } else if br == 0 && p >= 1 && lc != b'"' && buf[p - 1] == b'?' {
                        in_q = 0;
                        state = 0;
                        tag.clear();
                    }
                }
                b'"' | b'\'' if p >= 1 && buf[p - 1] != b'\\' => {
                    if lc == c {
                        lc = 0;
                    } else if lc != b'\\' {
                        lc = c;
                    }
                    if p != 0 && (in_q == 0 || c == in_q) {
                        in_q = if in_q != 0 { 0 } else { c };
                    }
                }
                b'l' | b'L'
                    if p > 4
                        && buf[p - 1].eq_ignore_ascii_case(&b'm')
                        && buf[p - 2].eq_ignore_ascii_case(&b'x')
                        && buf[p - 3] == b'?'
                        && buf[p - 4] == b'<' =>
                {
                    state = 1;
                    is_xml = true;
                }
                _ => {}
            },
            3 => match c {
                b'>' => {
                    if depth != 0 {
                        depth -= 1;
                    } else if in_q == 0 {
                        in_q = 0;
                        state = 0;
                        tag.clear();
                    }
                }
                b'"' | b'\'' if p != 0 && buf[p - 1] != b'\\' && (in_q == 0 || c == in_q) => {
                    in_q = if in_q != 0 { 0 } else { c };
                }
                b'-' if p >= 2 && buf[p - 1] == b'-' && buf[p - 2] == b'!' => {
                    state = 4;
                }
                b'E' | b'e' if p > 6 && buf[p - 6..p].eq_ignore_ascii_case(b"doctyp") => {
                    state = 1;
                }
                _ => {}
            },
            _ => {
                if c == b'>' && in_q == 0 && p >= 2 && buf[p - 1] == b'-' && buf[p - 2] == b'-' {
                    in_q = 0;
                    state = 0;
                    tag.clear();
                }
            }
        }
        p += 1;
    }
    out
}

/// `php_tag_find(tag, len, set)`: the tag normalised, lowercased, to
/// `<name>` (whitespace ends the name, and a `/` right after `<` or
/// right before `>` is dropped), and whether the set holds it.
fn tag_find(tag: &[u8], set: &[u8]) -> bool {
    if tag.is_empty() {
        return false;
    }
    let at = |i: usize| tag.get(i).copied().unwrap_or(0);
    let mut norm = Vec::with_capacity(tag.len() + 1);
    let mut t = 0usize;
    let mut started = false;
    loop {
        let c = at(t).to_ascii_lowercase();
        match c {
            b'<' => norm.push(c),
            b'>' => break,
            _ => {
                if !is_space(c) {
                    started = true;
                    let before = if t == 0 { 0 } else { at(t - 1) };
                    if c != b'/' || (before != b'<' && at(t + 1) != b'>') {
                        norm.push(c);
                    }
                } else if started {
                    break;
                }
            }
        }
        t += 1;
        if t > tag.len() {
            break;
        }
    }
    norm.push(b'>');
    let set = set.split(|c| *c == 0).next().unwrap_or(&[]);
    super::memnstr(set, &norm, 0).is_some()
}
