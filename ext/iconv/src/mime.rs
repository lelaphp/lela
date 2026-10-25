//! `iconv.c`'s MIME header codecs, `_php_iconv_mime_encode` and
//! `_php_iconv_mime_decode` (`Z/ext/iconv/iconv.c:917-1776`), and the
//! three natives over them. An encoded word's text is ext/standard's
//! `php_base64_encode`, `php_base64_decode` and `php_quot_print_decode`,
//! which `iconv.c` includes; every conversion is a converter of the
//! call's own.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::coerce::{c_str, to_long, with_string};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, SmartStr, Value};
use php_ext_standard::{base64, quot_print};

use super::ffi::heap::Block;
use super::ffi::{self, Cd, Errno};
use super::{Error, Kind, MAX_NAME, charset, encoding, failed, opened, too_long};

/// `ICONV_ASCII_ENCODING`.
const ASCII: &[u8] = b"ASCII";

/// `php_iconv_enc_scheme_t`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Base64,
    Qprint,
}

/// `$mode`'s two bits, `ICONV_MIME_DECODE_STRICT` and
/// `ICONV_MIME_DECODE_CONTINUE_ON_ERROR`, read once.
#[derive(Clone, Copy)]
struct Mode {
    strict: bool,
    go_on: bool,
}

impl Mode {
    fn of(mode: i64) -> Mode {
        Mode {
            strict: mode & 1 != 0,
            go_on: mode & 2 != 0,
        }
    }

    /// Where a word that is passed through leaves the scan.
    fn after_word(self) -> Scan {
        if self.strict { Scan::Word } else { Scan::Any }
    }
}

/// `qp_table`: what one byte costs as quoted-printable, itself or `=XX`.
fn qp_cost(byte: u8) -> usize {
    match byte {
        b'!'..=b'<' | b'>' | b'@'..=b'^' | b'`'..=b'~' => 1,
        _ => 3,
    }
}

/// An error a converter's `iconv` left, as the encoder's switch reads it.
fn by_errno(errno: Errno) -> Error {
    match errno {
        Errno::Inval => Error::IllegalChar,
        Errno::Ilseq => Error::IllegalSeq,
        other => Error::Unknown(other),
    }
}

/// What stopped the encoder: php's error, or the request's end.
enum Fail {
    Error(Error),
    Thrown(Thrown),
}

impl From<Error> for Fail {
    fn from(error: Error) -> Fail {
        Fail::Error(error)
    }
}

impl From<Thrown> for Fail {
    fn from(thrown: Thrown) -> Fail {
        Fail::Thrown(thrown)
    }
}

/// `_php_iconv_mime_encode`: `name: ` and `value` as encoded words of
/// `out_charset` in `scheme` onto `out`, php's `smart_str`, each line at
/// most `max` bytes, folded with `lf` and a space. The word's buffer is
/// asked for fallibly since 8.5.12 (`safe_femalloc`), and the words stop
/// once the memory limit was met (`ZEND_HAS_OOM()`).
#[allow(clippy::too_many_arguments)]
fn encode(
    req: &mut Request,
    out: &mut SmartStr,
    name: &[u8],
    value: &[u8],
    max: usize,
    lf: &[u8],
    scheme: Scheme,
    out_charset: &[u8],
    enc: &[u8],
) -> Result<(), Fail> {
    if name.len().wrapping_add(2) >= max || out_charset.len().wrapping_add(12) >= max {
        return Err(Error::TooBig.into());
    }
    let mut cd_pl = opened(ASCII, enc)?;
    let mut cd = opened(out_charset, enc)?;
    let Some(size) = max.checked_add(5) else {
        let message = format!("Possible integer overflow in memory allocation (1 * {max} + 5)");
        return Err(php_ext::raising::fatal(req, &message).into());
    };
    let Some(mut buf) = Block::ask_fallibly(req, size)? else {
        return Err(Error::Alloc.into());
    };
    let words = words(
        req,
        out,
        (&mut cd_pl, &mut cd),
        buf.as_mut_slice(),
        name,
        value,
        max,
        lf,
        scheme,
        out_charset,
    );
    buf.give_back(req);
    words
}

/// [`encode`]'s words, written with `buf` as php's `buf`.
#[allow(clippy::too_many_arguments)]
fn words(
    req: &mut Request,
    out: &mut SmartStr,
    (cd_pl, cd): (&mut Cd, &mut Cd),
    buf: &mut [u8],
    name: &[u8],
    value: &[u8],
    max: usize,
    lf: &[u8],
    scheme: Scheme,
    out_charset: &[u8],
) -> Result<(), Fail> {
    let mut chars = max;
    if !name.is_empty() {
        // php ignores what the name's conversion answers.
        appendl(req, out, Some(name), cd_pl)?.ok();
    }
    chars = chars.wrapping_sub(name.len());
    out.append(req, b": ")?;
    chars = chars.wrapping_sub(2);
    let mut rest = value;
    loop {
        let letter = if scheme == Scheme::Base64 { 4 } else { 3 };
        let min = b"=??X??=".len() + out_charset.len() + letter;
        if chars < min + lf.len() + 1 {
            out.append(req, lf)?;
            out.append(req, b" ")?;
            chars = max.wrapping_sub(1);
        }
        out.append(req, b"=?")?;
        out.append(req, out_charset)?;
        out.append(req, b"?")?;
        chars = chars.wrapping_sub(3 + out_charset.len());
        match scheme {
            Scheme::Base64 => {
                out.append(req, b"B?")?;
                chars = chars.wrapping_sub(2);
                let (used, left) = base64_word(cd, rest, buf, chars.wrapping_sub(2) / 4 * 3)?;
                rest = left;
                let encoded = base64::encode(&buf[..used]);
                if chars < encoded.len() {
                    return Err(Error::Unknown(ffi::errno()).into());
                }
                out.append(req, &encoded)?;
                out.append(req, b"?=")?;
                chars = chars.wrapping_sub(encoded.len() + 2);
            }
            Scheme::Qprint => {
                out.append(req, b"Q?")?;
                chars = chars.wrapping_sub(2);
                let (used, left) = qprint_word(cd, rest, buf, chars.wrapping_sub(2))?;
                rest = left;
                for &byte in &buf[..used] {
                    if qp_cost(byte) == 1 {
                        out.append(req, &[byte])?;
                        chars = chars.wrapping_sub(1);
                    } else {
                        const HEX: &[u8; 16] = b"0123456789ABCDEF";
                        out.append(req, b"=")?;
                        out.append(req, &[HEX[usize::from(byte >> 4)]])?;
                        out.append(req, &[HEX[usize::from(byte & 15)]])?;
                        chars = chars.wrapping_sub(3);
                    }
                }
                out.append(req, b"?=")?;
                chars = chars.wrapping_sub(2);
                cd.reset().map_err(Error::Unknown)?;
            }
        }
        if rest.is_empty() || req.memory().has_oom() {
            return Ok(());
        }
    }
}

/// One base64 word's bytes: as much of `input` as fits `size` bytes of
/// `buf` with room kept for the shift state's reset, four bytes more
/// each time it did not fit. The bytes written and the input left.
fn base64_word<'a>(
    cd: &mut Cd,
    input: &'a [u8],
    buf: &mut [u8],
    size: usize,
) -> Result<(usize, &'a [u8]), Error> {
    let size = size.min(buf.len());
    let mut reserved = 4;
    loop {
        if size <= reserved {
            return Err(Error::TooBig);
        }
        let mut rest = input;
        let (written, result) = cd.step(Some(&mut rest), &mut buf[..size - reserved]);
        match result {
            Err(Errno::TooBig) if rest.len() == input.len() => return Err(Error::TooBig),
            Ok(_) | Err(Errno::TooBig) => {}
            Err(errno) => return Err(by_errno(errno)),
        }
        let (flushed, result) = cd.step(None, &mut buf[written..size]);
        match result {
            Ok(_) => return Ok((written + flushed, rest)),
            Err(Errno::TooBig) => {}
            Err(errno) => return Err(Error::Unknown(errno)),
        }
        cd.reset().map_err(Error::Unknown)?;
        reserved += 4;
    }
}

/// One quoted-printable word's bytes: as much of `input` as converts
/// into bytes whose `=XX` form fits `room`, the converter's room shrunk
/// by a third of the overshoot each time it did not. The bytes written
/// and the input left.
fn qprint_word<'a>(
    cd: &mut Cd,
    input: &'a [u8],
    buf: &mut [u8],
    room: usize,
) -> Result<(usize, &'a [u8]), Error> {
    let mut size = room;
    loop {
        // php's loop stops at a size of zero with the input rewound and
        // its `do while` round the words never ends; here that is the
        // line too short for any word.
        if size == 0 || size > buf.len() {
            return Err(Error::TooBig);
        }
        let mut rest = input;
        let (written, result) = cd.step(Some(&mut rest), &mut buf[..size]);
        match result {
            Err(Errno::TooBig) if rest.len() == input.len() => {
                return Err(Error::Unknown(Errno::TooBig));
            }
            Ok(_) | Err(Errno::TooBig) => {}
            Err(errno) => return Err(by_errno(errno)),
        }
        let (flushed, result) = cd.step(None, &mut buf[written..size]);
        if let Err(errno) = result
            && errno != Errno::TooBig
        {
            return Err(Error::Unknown(errno));
        }
        let used = written + flushed;
        let needed: usize = buf[..used].iter().map(|&b| qp_cost(b)).sum();
        if needed <= room {
            return Ok((used, rest));
        }
        size = size.wrapping_sub((needed - room).div_ceil(3));
    }
}

/// `_php_iconv_appendl` onto `out`, php's `smart_str`: room of 128 bytes
/// and doubling, each asked for fallibly since 8.5.12
/// (`smart_str_falloc`), a refusal php's `PHP_ICONV_ERR_ALLOC`; `None`
/// flushes the converter's shift state. The bytes a call that failed
/// other than for room wrote are never counted into the string.
fn appendl(
    req: &mut Request,
    out: &mut SmartStr,
    input: Option<&[u8]>,
    cd: &mut Cd,
) -> Result<Result<(), Error>, Thrown> {
    let mut growth = 128;
    let mut rest = input.unwrap_or_default();
    loop {
        if input.is_some() && rest.is_empty() {
            return Ok(Ok(()));
        }
        if req
            .memory_mut()
            .fallible(|req| out.reserve(req, growth))?
            .is_none()
        {
            return Ok(Err(Error::Alloc));
        }
        let mut chunk = vec![0u8; growth];
        let (written, result) = cd.step(input.map(|_| &mut rest), &mut chunk);
        match (input, result) {
            (Some(_), Err(Errno::Inval)) => return Ok(Err(Error::IllegalChar)),
            (Some(_), Err(Errno::Ilseq)) => return Ok(Err(Error::IllegalSeq)),
            (Some(_), Ok(_) | Err(Errno::TooBig)) | (None, Err(Errno::TooBig)) => {}
            (None, Ok(_)) => {
                out.append(req, &chunk[..written])?;
                return Ok(Ok(()));
            }
            (_, Err(errno)) => return Ok(Err(Error::Unknown(errno))),
        }
        out.append(req, &chunk[..written])?;
        growth <<= 1;
    }
}

/// php's `smart_str` for the decoded header, `None` until an append
/// allocates it, which `iconv_mime_decode_headers` reads as nothing
/// left; with the request it lives on, and what ended the request in an
/// append, after which nothing more is appended.
struct Out<'r> {
    req: &'r mut Request,
    header: Option<SmartStr>,
    thrown: Option<Thrown>,
}

/// `_php_iconv_appendl` onto the header: `None` flushes the converter's
/// shift state, and an empty span appends nothing and allocates nothing.
fn append(out: &mut Out<'_>, bytes: Option<&[u8]>, cd: &mut Cd) -> Result<(), Error> {
    if bytes.is_some_and(<[u8]>::is_empty) {
        return Ok(());
    }
    if out.thrown.is_some() {
        return Err(Error::Alloc);
    }
    let header = out.header.get_or_insert_with(SmartStr::default);
    match appendl(out.req, header, bytes, cd) {
        Ok(appended) => appended,
        Err(thrown) => {
            out.thrown = Some(thrown);
            Err(Error::Alloc)
        }
    }
}

/// `_php_iconv_mime_decode`: one header decoded into `enc` on the
/// request's heap, and where it ended, `*next_pos`; the header given
/// back where the decode failed.
fn decode(
    req: &mut Request,
    input: &[u8],
    enc: &[u8],
    mode: Mode,
) -> Result<Result<(Option<SmartStr>, usize), Error>, Thrown> {
    let mut out = Out {
        req,
        header: None,
        thrown: None,
    };
    let decoded = decode_into(&mut out, input, enc, mode);
    let Out {
        req,
        header,
        thrown,
    } = out;
    if let Some(thrown) = thrown {
        return Err(thrown);
    }
    match decoded {
        Ok(next) => Ok(Ok((header, next))),
        Err(error) => {
            if let Some(mut header) = header {
                header.free(req);
            }
            Ok(Err(error))
        }
    }
}

/// `_php_iconv_mime_decode`'s `scan_stat`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// 0: any character.
    Any,
    /// 1: after `=`, the `?` that opens a word.
    Open,
    /// 2: the charset's name.
    Charset,
    /// 3: the scheme's letter.
    Scheme,
    /// 4: the `?` after it.
    TextOpen,
    /// 5: the encoded text.
    Text,
    /// 6: the `=` after its closing `?`.
    Close,
    /// 7: after `\r`, a `\n`.
    Cr,
    /// 8: after a line end, whether the next line folds into this one.
    Fold,
    /// 9: after a word, what comes next.
    After,
    /// 10: the language after `*`, dismissed.
    Language,
    /// 11: blanks.
    Blanks,
    /// 12: a word that is not encoded.
    Word,
}

/// The bytes of `input` from `start` to `end`, php's `(p1 + 1) -
/// encoded_word`; `None` where the start is `NULL`, which
/// `_php_iconv_appendl` reads as a flush.
fn span(input: &[u8], start: Option<usize>, end: usize) -> Option<&[u8]> {
    start.map(|start| input.get(start..end.min(input.len())).unwrap_or_default())
}

/// [`decode`]'s body, the header written through `out`.
#[allow(clippy::too_many_lines)]
fn decode_into(out: &mut Out<'_>, input: &[u8], enc: &[u8], mode: Mode) -> Result<usize, Error> {
    let at = |k: usize| input.get(k).copied().unwrap_or(0);
    let mut cd_pl = opened(enc, ASCII)?;
    let mut cd: Option<Cd> = None;
    let mut err: Result<(), Error> = Ok(());
    let (mut scan, mut scheme) = (Scan::Any, Scheme::Base64);
    let (mut word, mut charset, mut text, mut blanks) =
        (None::<usize>, None::<usize>, 0, None::<usize>);
    let mut text_len = 0;
    let (mut p, mut left) = (0usize, input.len());
    while left > 0 {
        let c = at(p);
        let mut eos = false;
        let mut after = scan == Scan::After;
        'step: {
            match scan {
                Scan::Any => match c {
                    b'\r' => scan = Scan::Cr,
                    b'\n' => scan = Scan::Fold,
                    b'=' => {
                        word = Some(p);
                        scan = Scan::Open;
                    }
                    b' ' | b'\t' => {
                        blanks = Some(p);
                        scan = Scan::Blanks;
                    }
                    _ => {
                        err = append(out, Some(&[c]), &mut cd_pl);
                        if err.is_err() {
                            if mode.go_on {
                                err = Ok(());
                            } else {
                                err?;
                            }
                        }
                        word = None;
                        if mode.strict {
                            scan = Scan::Word;
                        }
                    }
                },
                Scan::Open => {
                    if c != b'?' {
                        if c == b'\r' || c == b'\n' {
                            p -= 1;
                        }
                        err = append(out, span(input, word, p + 1), &mut cd_pl);
                        err?;
                        word = None;
                        scan = mode.after_word();
                        break 'step;
                    }
                    charset = Some(p + 1);
                    scan = Scan::Charset;
                }
                Scan::Charset => {
                    match c {
                        b'?' => scan = Scan::Scheme,
                        b'*' => scan = Scan::Language,
                        b'\r' | b'\n' => {
                            p -= 1;
                            append(out, Some(b"="), &mut cd_pl).ok();
                            append(out, Some(b"?"), &mut cd_pl).ok();
                            err = append(out, span(input, charset, p + 1), &mut cd_pl);
                            err?;
                            charset = None;
                            scan = mode.after_word();
                            break 'step;
                        }
                        _ => {}
                    }
                    if scan == Scan::Charset {
                        break 'step;
                    }
                    let Some(from) = charset else {
                        return Err(Error::Malformed);
                    };
                    let name = input.get(from..p).unwrap_or_default();
                    if name.len() > 79 {
                        if !mode.go_on {
                            return Err(Error::Malformed);
                        }
                        err = append(out, span(input, word, p + 1), &mut cd_pl);
                        err?;
                        word = None;
                        scan = mode.after_word();
                        break 'step;
                    }
                    cd = None;
                    match Cd::open(enc, c_str(name)) {
                        Ok(opened) => cd = Some(opened),
                        Err(errno) if !mode.go_on => {
                            return Err(if errno == Errno::Inval {
                                Error::WrongCharset
                            } else {
                                Error::Converter
                            });
                        }
                        Err(_) => {
                            // The word is kept undecoded, its two `?` and
                            // the `=` after them skipped over.
                            let mut marks = 2;
                            while marks > 0 && left > 1 {
                                p += 1;
                                if at(p) == b'?' {
                                    marks -= 1;
                                }
                                left -= 1;
                            }
                            if at(p + 1) == b'=' {
                                p += 1;
                                if left > 1 {
                                    left -= 1;
                                }
                            }
                            err = append(out, span(input, word, p + 1), &mut cd_pl);
                            err?;
                            scan = Scan::Word;
                        }
                    }
                }
                Scan::Scheme => match c {
                    b'b' | b'B' => {
                        scheme = Scheme::Base64;
                        scan = Scan::TextOpen;
                    }
                    b'q' | b'Q' => {
                        scheme = Scheme::Qprint;
                        scan = Scan::TextOpen;
                    }
                    _ => {
                        if !mode.go_on {
                            return Err(Error::Malformed);
                        }
                        err = append(out, span(input, word, p + 1), &mut cd_pl);
                        err?;
                        word = None;
                        scan = mode.after_word();
                    }
                },
                Scan::TextOpen => {
                    if c != b'?' {
                        if !mode.go_on {
                            return Err(Error::Malformed);
                        }
                        err = append(out, span(input, word, p + 1), &mut cd_pl);
                        err?;
                        word = None;
                        scan = mode.after_word();
                        break 'step;
                    }
                    text = p + 1;
                    scan = Scan::Text;
                }
                Scan::Text => {
                    if c == b'?' {
                        text_len = p - text;
                        scan = Scan::Close;
                    }
                }
                Scan::Cr => {
                    if c == b'\n' {
                        scan = Scan::Fold;
                    } else {
                        append(out, Some(b"\r"), &mut cd_pl).ok();
                        append(out, Some(&[c]), &mut cd_pl).ok();
                        scan = Scan::Any;
                    }
                }
                Scan::Fold => {
                    if c != b' ' && c != b'\t' {
                        p -= 1;
                        left = 1;
                        break 'step;
                    }
                    if word.is_none() {
                        append(out, Some(b" "), &mut cd_pl).ok();
                    }
                    blanks = None;
                    scan = Scan::Blanks;
                }
                Scan::Close => {
                    if c != b'=' {
                        if !mode.go_on {
                            return Err(Error::Malformed);
                        }
                        err = append(out, span(input, word, p + 1), &mut cd_pl);
                        err?;
                        word = None;
                        scan = mode.after_word();
                        break 'step;
                    }
                    scan = Scan::After;
                    if left == 1 {
                        eos = true;
                        after = true;
                    }
                }
                Scan::After => {}
                Scan::Language => {
                    if c == b'?' {
                        scan = Scan::Scheme;
                    }
                }
                Scan::Blanks => match c {
                    b'\r' => scan = Scan::Cr,
                    b'\n' => scan = Scan::Fold,
                    b'=' => {
                        if word.is_none()
                            && let Some(from) = blanks.take()
                        {
                            append(out, input.get(from..p), &mut cd_pl).ok();
                        }
                        word = Some(p);
                        scan = Scan::Open;
                    }
                    b' ' | b'\t' => {}
                    _ => {
                        if let Some(from) = blanks.take() {
                            append(out, input.get(from..p), &mut cd_pl).ok();
                        }
                        append(out, Some(&[c]), &mut cd_pl).ok();
                        word = None;
                        scan = mode.after_word();
                    }
                },
                Scan::Word => match c {
                    b'\r' => scan = Scan::Cr,
                    b'\n' => scan = Scan::Fold,
                    b' ' | b'\t' => {
                        blanks = Some(p);
                        scan = Scan::Blanks;
                    }
                    b'=' if !mode.strict => {
                        word = Some(p);
                        scan = Scan::Open;
                    }
                    _ => {
                        append(out, Some(&[c]), &mut cd_pl).ok();
                    }
                },
            }
            if !after {
                break 'step;
            }
            // 9: the word's text decoded, then what follows it.
            if !eos && !matches!(c, b'\r' | b'\n' | b' ' | b'\t') && mode.strict {
                err = append(out, span(input, word, p + 1), &mut cd_pl);
                err?;
                scan = Scan::Word;
                break 'step;
            }
            let encoded = input.get(text..text + text_len).unwrap_or_default();
            let decoded = match scheme {
                Scheme::Base64 => base64::decode(encoded, false),
                Scheme::Qprint => quot_print::decode(encoded, true),
            };
            let Some(decoded) = decoded else {
                if !mode.go_on {
                    return Err(Error::Unknown(ffi::errno()));
                }
                err = append(out, span(input, word, p + 1), &mut cd_pl);
                err?;
                word = None;
                scan = mode.after_word();
                break 'step;
            };
            let Some(cd) = cd.as_mut() else {
                return Err(Error::Converter);
            };
            err = append(out, Some(&decoded), cd);
            if err.is_ok() {
                err = append(out, None, cd);
            }
            if err.is_err() {
                if !mode.go_on {
                    err?;
                }
                err = append(out, span(input, word, p), &mut cd_pl);
                word = None;
                if err.is_err() {
                    break 'step;
                }
            }
            if eos {
                scan = Scan::Any;
                break 'step;
            }
            match c {
                b'\r' => scan = Scan::Cr,
                b'\n' => scan = Scan::Fold,
                b'=' => scan = Scan::Open,
                b' ' | b'\t' => {
                    blanks = Some(p);
                    scan = Scan::Blanks;
                }
                _ => {
                    append(out, Some(&[c]), &mut cd_pl).ok();
                    scan = Scan::Word;
                }
            }
        }
        left -= 1;
        p += 1;
    }
    match scan {
        Scan::Any | Scan::Fold | Scan::Blanks | Scan::Word => {}
        _ if mode.go_on => {
            if scan == Scan::Open {
                append(out, Some(b"="), &mut cd_pl).ok();
            }
            err = Ok(());
        }
        _ => return Err(Error::Malformed),
    }
    err.map(|()| p)
}

/// One of the encoder's preferences, `zend_hash_str_find_deref`.
fn preference(prefs: PhpArray, name: &[u8]) -> Option<Value> {
    prefs.find(name).map(|value| value.deref())
}

/// `iconv_mime_encode(string $field_name, string $field_value, array $options = []): string|false`,
/// the `scheme`, `input-charset`, `output-charset`, `line-length` and
/// `line-break-chars` preferences as php reads them.
///
/// ```php
/// $p = ['input-charset' => 'UTF-8', 'output-charset' => 'UTF-8', 'line-length' => 30, 'line-break-chars' => "\n"];
/// echo iconv_mime_encode('Subject', 'Prüfung Prüfung Prüfung', $p), "\n";
/// echo iconv_mime_encode('Subject', 'Prüfung Prüfung', ['scheme' => 'q'] + $p), "\n";
/// var_dump(iconv_mime_encode('Subject', "\xff", $p), iconv_mime_encode(str_repeat('x', 40), 'a', $p), iconv_mime_encode('S', 'a', ['output-charset' => 'nope']));
/// var_dump(iconv_mime_encode('S', 'a', ['input-charset' => str_repeat('a', 64)]));
/// ```
/// ```expect
/// Subject: =?UTF-8?B?UHI=?=
///  =?UTF-8?B?w7xmdW5nIFA=?=
///  =?UTF-8?B?csO8ZnVuZyA=?=
///  =?UTF-8?B?UHLDvGZ1bmc=?=
/// Subject: =?UTF-8?Q?Pr=C3=BCf?=
///  =?UTF-8?Q?ung=20Pr=C3=BCfun?=
///  =?UTF-8?Q?g?=
///
/// Notice: iconv_mime_encode(): Detected an illegal character in input string in %s on line 5
///
/// Warning: iconv_mime_encode(): Buffer length exceeded in %s on line 5
///
/// Warning: iconv_mime_encode(): Wrong encoding, conversion from "UTF-8" to "nope" is not allowed in %s on line 5
/// bool(false)
/// bool(false)
/// bool(false)
///
/// Warning: iconv_mime_encode(): Encoding parameter exceeds the maximum allowed length of 64 characters in %s on line 6
/// bool(false)
/// ```
///
/// Since 8.5.12 the word's buffer is asked for fallibly, and the words stop
/// once the memory limit was met; either way the limit's fatal is raised as
/// the call returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(iconv_mime_encode('x', 'y', ['line-length' => PHP_INT_MAX]), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 9223372036854792192 bytes) in %s on line 3
/// ```
///
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(iconv_mime_encode("A", str_repeat("\xFF", 2 << 20), ["scheme" => "Q", "input-charset" => "ISO-8859-1", "output-charset" => "ISO-8859-1"]), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn iconv_mime_encode(
    req: &mut Request,
    field_name: PhpString,
    field_value: PhpString,
    #[php(default = [])] options: PhpArray,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let mut in_charset = encoding(req, Kind::Internal);
    let mut out_charset = in_charset.clone();
    let (mut max, mut lf, mut scheme) = (76i64, b"\r\n".to_vec(), Scheme::Base64);
    if let Some(given) = preference(options, b"scheme")
        && let Some(&letter) = given.as_str_bytes().and_then(<[u8]>::first)
    {
        match letter {
            b'B' | b'b' => scheme = Scheme::Base64,
            b'Q' | b'q' => scheme = Scheme::Qprint,
            _ => {}
        }
    }
    for (key, slot) in [
        (&b"input-charset"[..], &mut in_charset),
        (b"output-charset", &mut out_charset),
    ] {
        let given = preference(options, key);
        if let Some(bytes) = given.as_ref().and_then(Value::as_str_bytes) {
            if bytes.len() >= MAX_NAME {
                too_long(req);
                return Ok(OrFalse::False);
            }
            if !bytes.is_empty() {
                *slot = bytes.to_vec();
            }
        }
    }
    if let Some(given) = preference(options, b"line-length") {
        max = to_long(req, &given)?;
    }
    if let Some(given) = preference(options, b"line-break-chars") {
        lf = with_string(req, &given, <[u8]>::to_vec)?;
    }
    // `(size_t) line_len`.
    let max = usize::try_from(max.cast_unsigned()).unwrap_or(usize::MAX);
    let (out_charset, in_charset) = (c_str(&out_charset).to_vec(), c_str(&in_charset).to_vec());
    let mut out = SmartStr::default();
    let encoded = encode(
        req,
        &mut out,
        field_name.as_bytes(),
        field_value.as_bytes(),
        max,
        c_str(&lf),
        scheme,
        &out_charset,
        &in_charset,
    );
    match encoded {
        Ok(()) => Ok(OrFalse::Value(out.extract(req)?)),
        Err(Fail::Error(error)) => {
            let answer = failed(req, error, &out_charset, &in_charset);
            out.free(req);
            answer
        }
        Err(Fail::Thrown(thrown)) => Err(thrown),
    }
}

/// `iconv_mime_decode(string $string, int $mode = 0, ?string $encoding = null): string|false`.
///
/// ```php
/// $h = "Subject: =?UTF-8?B?UHLDvGZ1bmc=?=\r\n =?UTF-8?Q?_Pr=C3=BCfung?= x";
/// var_dump(iconv_mime_decode($h), iconv_mime_decode('=?UTF-8?B?w6k=?=abc'), iconv_mime_decode('=?UTF-8?B?w6k=?=abc', ICONV_MIME_DECODE_STRICT));
/// var_dump(iconv_mime_decode('a =?nope?B?w6k=?= b', ICONV_MIME_DECODE_CONTINUE_ON_ERROR), iconv_mime_decode('a =?nope?B?w6k=?= b'), iconv_mime_decode('=?UTF-8?X?a?='), iconv_mime_decode('=?UTF-8?X?a?=', 2));
/// var_dump(iconv_mime_decode('=?UTF-8?B?w6k=?=', 0, 'ISO-8859-1') === "\xe9", iconv_mime_decode('x', 0, str_repeat('a', 64)));
/// ```
/// ```expect
/// string(28) "Subject: Prüfung Prüfung x"
/// string(5) "éabc"
/// string(19) "=?UTF-8?B?w6k=?=abc"
///
/// Warning: iconv_mime_decode(): Wrong encoding, conversion from "???" to "UTF-8" is not allowed in %s on line 4
///
/// Warning: iconv_mime_decode(): Malformed string in %s on line 4
/// string(19) "a =?nope?B?w6k=?= b"
/// bool(false)
/// bool(false)
/// string(13) "=?UTF-8?X?a?="
///
/// Warning: iconv_mime_decode(): Encoding parameter exceeds the maximum allowed length of 64 characters in %s on line 5
/// bool(true)
/// bool(false)
/// ```
///
/// Since 8.5.12 the decoded header grows fallibly, so a header past the
/// memory limit is refused there and the limit's fatal is raised as the
/// call returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(iconv_mime_decode('x: ' . str_repeat('a', 2 << 20), 0, 'UTF-32'), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 3
/// ```
#[php_function]
fn iconv_mime_decode(
    req: &mut Request,
    string: PhpString,
    #[php(default = 0)] mode: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let mode = Mode::of(mode);
    let Some(charset) = charset(req, encoding) else {
        return Ok(OrFalse::False);
    };
    match decode(req, string.as_bytes(), c_str(&charset), mode)? {
        Ok((header, _)) => Ok(OrFalse::Value(header.unwrap_or_default().extract(req)?)),
        Err(error) => failed(req, error, &charset, b"???"),
    }
}

/// `iconv_mime_decode_headers(string $headers, int $mode = 0, ?string $encoding = null): array|false`:
/// header after header, each split at its first `:`, a name seen again
/// gathering its values into an array.
///
/// ```php
/// $h = "Subject: =?UTF-8?B?UHLDvGZ1bmc=?=\r\nTo: a@b\r\nTo: =?UTF-8?Q?c=40d?=\r\n  e\r\nX-Nothing\r\nX:\t y";
/// var_dump(iconv_mime_decode_headers($h), iconv_mime_decode_headers("A: =?UTF-8?X?a?=\r\nB: b"), iconv_mime_decode_headers(''));
/// ```
/// ```expect
/// Warning: iconv_mime_decode_headers(): Malformed string in %s on line 3
/// array(3) {
///   ["Subject"]=>
///   string(8) "Prüfung"
///   ["To"]=>
///   array(2) {
///     [0]=>
///     string(3) "a@b"
///     [1]=>
///     string(4) "c@de"
///   }
///   ["X"]=>
///   string(1) "y"
/// }
/// bool(false)
/// array(0) {
/// }
/// ```
///
/// Since 8.5.12 the headers stop once the memory limit was met, and the
/// limit's fatal is raised as the call returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(iconv_mime_decode_headers(str_repeat("a:b\n", 512 * 1024), 0, "ISO-8859-1"), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2113536 bytes) in %s on line 3
/// ```
#[php_function]
fn iconv_mime_decode_headers(
    req: &mut Request,
    headers: PhpString,
    #[php(default = 0)] mode: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let mode = Mode::of(mode);
    let Some(charset) = charset(req, encoding) else {
        return Ok(OrFalse::False);
    };
    let mut out = PhpArray::new(req)?;
    let mut rest = headers.as_bytes();
    // Since 8.5.12 the headers stop once the memory limit was met.
    while !rest.is_empty() && !req.memory().has_oom() {
        let (mut decoded, next) = match decode(req, rest, c_str(&charset), mode)? {
            Ok((Some(header), next)) => (header, next),
            Ok((None, _)) => break,
            Err(error) => {
                let answer = failed(req, error, &charset, b"???");
                out.give_up(req);
                return answer;
            }
        };
        let header = decoded.as_bytes();
        if let Some(colon) = header.iter().position(|&b| b == b':') {
            let at = header[colon + 1..]
                .iter()
                .position(|&b| b != b' ' && b != b'\t')
                .map_or(header.len(), |n| colon + 1 + n);
            let (name, value) = (header[..colon].to_vec(), header[at..].to_vec());
            add_header(req, &mut out, &name, &value)?;
        }
        decoded.free(req);
        rest = rest.get(next..).unwrap_or_default();
    }
    Ok(OrFalse::Value(out))
}

/// One header into `iconv_mime_decode_headers`' answer, as php's loop
/// adds it: under a name the answer holds as a string key
/// (`zend_hash_str_find`) the value is appended, the first value made a
/// list in its place; under any other, `add_assoc_stringl_ex`, whose
/// symbol-table key makes a numeric name an index, so a numeric name
/// seen again overwrites the row it left rather than gathering a list.
fn add_header(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    name: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let Some(previous) = out.find(name) else {
        let value = stringl(req, value)?.into_value();
        return out.set_symtable_str(req, name, value);
    };
    let mut list = if let Some(list) = previous.as_array() {
        list
    } else {
        let mut list = PhpArray::new(req)?;
        list.append(req, Owner::take(req, previous))?;
        let made = *list;
        out.set_str(req, name, list.into_value())?;
        made
    };
    let value = stringl(req, value)?.into_value();
    list.append(req, value)
}

/// `add_assoc_stringl_ex`'s and `add_next_index_stringl`'s string,
/// `ZVAL_STRINGL`: `zend_string_init`, the request's own whatever its
/// length, one byte among them.
fn stringl(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::filled(req, bytes.len(), |b| b.copy_from_slice(bytes))
}
