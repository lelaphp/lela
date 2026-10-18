//! `mbstring.c`'s MIME: RFC 2047 encoded words written line by line
//! (`mb_mime_header_encode`) and read back (`mb_mime_header_decode`),
//! and `mb_send_mail`, which writes a message's headers and body in the
//! language's mail encodings and hands it to ext/standard's `php_mail`.

use crate::convert::guess;
use crate::ffi::mbfl::{Encoder, Encoding, Language, fast_convert};
use crate::settings::{Substitute, encoding, settings};
use crate::tables::{
    ENCODING_7BIT, ENCODING_8BIT, ENCODING_BASE64, MBFL_OUTPUTFILTER_ILLEGAL_MODE_CHAR,
    MIME_CHAR_NEEDS_QENCODE,
};
use crate::text::{new_string, same, value_error};
use php_ext::abi::{Path, Union};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString};

/// `MBSTRING_HEADER_ENC_WCHAR_BUFSIZE`.
const WCHAR_BUFSIZE: usize = 90;
/// `MBSTRING_MIN_WCHAR_BUFSIZE`.
const MIN_WCHAR_BUFSIZE: usize = 5;

const BASE64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn needs_qencode(c: u8) -> bool {
    c > 0x7F
        || c == b'='
        || MIME_CHAR_NEEDS_QENCODE
            .get(usize::from(c))
            .is_some_and(|&n| n != 0)
}

/// `transfer_encoded_size`.
fn encoded_size(bytes: &[u8], base64: bool) -> usize {
    if base64 {
        bytes.len().div_ceil(3) * 4
    } else {
        bytes
            .iter()
            .map(|&c| if needs_qencode(c) { 3 } else { 1 })
            .sum()
    }
}

/// `transfer_encode_mime_bytes`: the pending bytes Base64- or
/// Q-encoded onto `out`, and the pending buffer emptied.
fn transfer_encode(pending: &mut Encoder, out: &mut Encoder, base64: bool) {
    let bytes = pending.bytes().to_vec();
    let mut encoded = Vec::with_capacity(encoded_size(&bytes, base64));
    if base64 {
        for chunk in bytes.chunks(3) {
            let bits =
                chunk.iter().fold(0u32, |b, &c| (b << 8) | u32::from(c)) << (8 * (3 - chunk.len()));
            let sextet = |shift: u32| BASE64_TABLE[((bits >> shift) & 0x3F) as usize];
            encoded.push(sextet(18));
            encoded.push(sextet(12));
            encoded.push(if chunk.len() > 1 { sextet(6) } else { b'=' });
            encoded.push(if chunk.len() > 2 { sextet(0) } else { b'=' });
        }
    } else {
        for &c in &bytes {
            if needs_qencode(c) {
                encoded.extend_from_slice(format!("={c:02X}").as_bytes());
            } else {
                encoded.push(c);
            }
        }
    }
    out.append(&encoded);
    pending.truncate(0);
}

/// `mb_mime_header_encode`: ASCII words passed through while they fit
/// a line, then every codepoint from the first that needs it as encoded
/// words of at most 74 bytes a line.
#[allow(clippy::too_many_lines)] // one C function, kept whole
pub(crate) fn header_encode(
    input: &[u8],
    incode: Encoding,
    outcode: Encoding,
    base64: bool,
    linefeed: &[u8],
    indent: i64,
) -> Option<Vec<u8>> {
    if input.is_empty() {
        return Some(Vec::new());
    }
    let mut indent = if (0..74).contains(&indent) {
        usize::try_from(indent).unwrap_or(0)
    } else {
        0
    };
    let linefeed = &linefeed[..linefeed.len().min(8)];
    let linefeed = php_ext::coerce::c_str(linefeed);
    let mime_name = outcode.mime_name();
    let mut wchars = [0u32; WCHAR_BUFSIZE];
    let mut state = 0u32;
    // Pass through an all-ASCII string with no space past its leading
    // ones. php does not reset the decoder's state after this scan.
    let mut scan = input;
    let mut leading = true;
    let mut passthrough = true;
    'scan: while !scan.is_empty() {
        let n = incode.decode(&mut scan, &mut wchars, &mut state);
        for &c in &wchars[..n] {
            if leading {
                if c == u32::from(b' ') {
                    continue;
                }
                leading = false;
            }
            if !(0x21..=0x7E).contains(&c)
                || c == u32::from(b'=')
                || c == u32::from(b'?')
                || c == u32::from(b'_')
            {
                passthrough = false;
                break 'scan;
            }
        }
    }
    if passthrough {
        return None;
    }
    let mut buf = Encoder::new(
        Encoding::utf8(),
        input.len(),
        u32::from(b'?'),
        MBFL_OUTPUTFILTER_ILLEGAL_MODE_CHAR,
    );
    let mut offset = 0usize;
    let mut line_start = 0usize;
    let mut rest = input;
    let (mut p, mut e);
    let feed = |buf: &mut Encoder, line_start: &mut usize, indent: &mut usize| {
        buf.append(linefeed);
        buf.append(b" ");
        *indent = 0;
        *line_start = buf.len();
    };
    // ASCII words first.
    'ascii: loop {
        if rest.is_empty() {
            return Some(buf.into_vec());
        }
        let n = incode.decode(&mut rest, &mut wchars[offset..], &mut state);
        p = 0usize;
        e = offset + n;
        let mut word_start = 0usize;
        while p < e && wchars[p] == u32::from(b' ') && p - word_start <= 74 {
            p += 1;
        }
        let mut needs_mime = false;
        while p < e {
            let w = wchars[p];
            p += 1;
            if !(0x20..=0x7E).contains(&w)
                || w == u32::from(b'?')
                || w == u32::from(b'=')
                || w == u32::from(b'_')
                || (w == u32::from(b' ') && p - word_start > 74)
            {
                needs_mime = true;
                break;
            } else if w == u32::from(b' ') {
                if buf.len() - line_start + (p - word_start) + indent > 75 {
                    feed(&mut buf, &mut line_start, &mut indent);
                } else if buf.len() > 0 {
                    buf.append(b" ");
                }
                let word: Vec<u8> = wchars[word_start..p - 1]
                    .iter()
                    .map(|&c| (c & 0xFF) as u8)
                    .collect();
                buf.append(&word);
                word_start = p;
                while p < e && wchars[p] == u32::from(b' ') {
                    p += 1;
                }
            }
        }
        if !needs_mime && !rest.is_empty() {
            if word_start < MIN_WCHAR_BUFSIZE {
                needs_mime = true;
            } else {
                offset = e - word_start;
                wchars.copy_within(word_start..e, 0);
                continue 'ascii;
            }
        }
        if !needs_mime {
            if word_start < e && buf.len() > 0 {
                if buf.len() - line_start + (p - word_start) + indent > 74 {
                    buf.append(linefeed);
                    buf.append(b" ");
                } else {
                    buf.append(b" ");
                }
            }
            let word: Vec<u8> = wchars[word_start..e]
                .iter()
                .map(|&c| (c & 0xFF) as u8)
                .collect();
            buf.append(&word);
            continue 'ascii;
        }
        // `feed_and_mime_encode`.
        if buf.len() - line_start + indent + mime_name.len() > 55 {
            feed(&mut buf, &mut line_start, &mut indent);
        } else if buf.len() > 0 {
            buf.append(b" ");
        }
        p = word_start;
        break;
    }
    // Encoded words from `p` to the end.
    let mut pending = Encoder::new(
        outcode,
        rest.len(),
        u32::from(b'?'),
        MBFL_OUTPUTFILTER_ILLEGAL_MODE_CHAR,
    );
    offset = e - p;
    let mut refill = WCHAR_BUFSIZE - offset >= MIN_WCHAR_BUFSIZE;
    if refill {
        wchars.copy_within(p..e, 0);
    }
    loop {
        if refill || (p >= e && !rest.is_empty()) {
            let n = incode.decode(&mut rest, &mut wchars[offset..], &mut state);
            p = 0;
            e = offset + n;
        }
        if p >= e {
            buf.append(b"=?");
            buf.append(mime_name);
            buf.append(if base64 { b"?B??=" } else { b"?Q??=" });
            return Some(buf.into_vec());
        }
        // `start_new_line`.
        buf.append(b"=?");
        buf.append(mime_name);
        buf.append(if base64 { b"?B?" } else { b"?Q?" });
        let mut n = 12usize;
        let space = 73usize
            .wrapping_sub(indent)
            .wrapping_sub(buf.len() - line_start);
        loop {
            let tmppos = pending.len();
            let tmpstate = pending.state();
            n = n.min(e - p);
            pending.write(&wchars[p..p + n], false);
            let tmppos2 = pending.len();
            let tmpstate2 = pending.state();
            pending.write(&[], true);
            if encoded_size(pending.bytes(), base64) <= space || (n == 1 && tmppos == 0) {
                p += n;
                if p == e {
                    transfer_encode(&mut pending, &mut buf, base64);
                    buf.append(b"?=");
                    return Some(buf.into_vec());
                }
                pending.truncate(tmppos2);
                pending.set_state(tmpstate2);
            } else {
                pending.truncate(tmppos);
                pending.set_state(tmpstate);
                if n == 1 {
                    pending.write(&[], true);
                    transfer_encode(&mut pending, &mut buf, base64);
                    pending.set_state(0);
                    buf.append(b"?=");
                    indent = 0;
                    if !rest.is_empty() || p < e {
                        buf.append(linefeed);
                        buf.append(b" ");
                        line_start = buf.len();
                        offset = e - p;
                        refill = !rest.is_empty() && WCHAR_BUFSIZE - offset >= MIN_WCHAR_BUFSIZE;
                        if refill {
                            wchars.copy_within(p..e, 0);
                        }
                        break;
                    }
                    return Some(buf.into_vec());
                }
                n = (n >> 1).max(1);
            }
        }
    }
}

/// `mb_encode_mimeheader(string $string, ?string $charset = null, ?string $transfer_encoding = null, string $newline = "\r\n", int $indent = 0): string`.
///
/// ```php
/// var_dump(mb_encode_mimeheader("Hello"), mb_encode_mimeheader("Añb", "UTF-8"), mb_encode_mimeheader("Añb", "UTF-8", "Q"), mb_encode_mimeheader(str_repeat("ñ", 30), "UTF-8", "B", "\n"));
/// try { mb_encode_mimeheader("a", "pass"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "Hello"
/// string(20) "=?UTF-8?B?QcOxYg==?="
/// string(20) "=?UTF-8?Q?A=C3=B1b?="
/// string(110) "=?UTF-8?B?w7HDscOxw7HDscOxw7HDscOxw7HDscOxw7HDscOxw7HDscOxw7HDscOxw7E=?=
///  =?UTF-8?B?w7HDscOxw7HDscOxw7HDsQ==?="
/// mb_encode_mimeheader(): Argument #2 ($charset) must be a valid encoding, "pass" given
/// ```
#[php_function]
fn mb_encode_mimeheader(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] charset: Option<PhpString>,
    #[php(default = null)] transfer_encoding: Option<PhpString>,
    #[php(default = "\r\n")] newline: PhpString,
    #[php(default = 0)] indent: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let mut base64 = true;
    let charset = if let Some(name) = charset {
        let enc = self::encoding(req, Some(name), 2)?;
        if enc.mime_name().is_empty() || enc == Encoding::qprint() {
            let name = String::from_utf8_lossy(name.as_bytes()).into_owned();
            return Err(value_error(
                req,
                2,
                &format!("\"{name}\" cannot be used for MIME header encoding"),
            ));
        }
        enc
    } else {
        let language = settings(req).language;
        let [charset, header, _] = language.mail();
        if Encoding::numbered(header).is_some_and(|t| matches!(t.name().first(), Some(b'Q' | b'q')))
        {
            base64 = false;
        }
        Encoding::numbered(charset).unwrap_or_else(Encoding::pass)
    };
    if let Some(t) = transfer_encoding
        && matches!(t.as_bytes().first(), Some(b'Q' | b'q'))
    {
        base64 = false;
    }
    let internal = settings(req).current_internal;
    match header_encode(
        string.as_bytes(),
        internal,
        charset,
        base64,
        newline.as_bytes(),
        indent,
    ) {
        // `zend_empty_string` for an empty input, before anything is
        // encoded.
        Some(_) if string.is_empty() => crate::text::cut_string(req, b""),
        Some(out) => new_string(req, &out),
        None => Ok(same(req, string)),
    }
}

/// `decode_base64`.
fn base64_value(c: u8) -> Option<u32> {
    BASE64_TABLE
        .iter()
        .position(|&b| b == c)
        .and_then(|at| u32::try_from(at).ok())
}

/// `qprint_map`: a hex digit's value.
fn hex_value(c: u8) -> Option<u8> {
    char::from(c)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// `mime_header_decode_encoded_word`: the word at the start of `input`
/// decoded onto `out`, and the rest after it; `None` for no word.
fn decode_word<'a>(input: &'a [u8], out: &mut Encoder, state: &mut u32) -> Option<&'a [u8]> {
    if input.len() < 6 {
        return None;
    }
    let body = &input[2..];
    let charset_end = body.iter().position(|&b| b == b'?')?;
    let charset = &body[..charset_end];
    let method = *body.get(charset_end + 1)?;
    if body.get(charset_end + 2) != Some(&b'?') {
        return None;
    }
    let incode = Encoding::named(charset)?;
    let text_start = 2 + charset_end + 3;
    let mut end = input.len();
    let text_all = &input[text_start..];
    if let Some(marker) = text_all.windows(2).position(|w| w == b"?=") {
        end = text_start + marker;
    } else if text_start < end && input[end - 1] == b'?' {
        end -= 1;
    }
    let text = &input[text_start.min(end)..end];
    let mut bytes = Vec::with_capacity(text.len());
    match method {
        b'Q' | b'q' => {
            let mut i = 0;
            while i < text.len() {
                let c = text[i];
                i += 1;
                if c == b'_' {
                    bytes.push(b' ');
                    continue;
                }
                if c == b'=' && text.len() - i >= 2 {
                    let (c2, c3) = (text[i], text[i + 1]);
                    i += 2;
                    if let (Some(h), Some(l)) = (hex_value(c2), hex_value(c3)) {
                        bytes.push((h << 4) | l);
                        continue;
                    } else if c2 == b'\r' {
                        if c3 != b'\n' {
                            i -= 1;
                        }
                        continue;
                    } else if c2 == b'\n' {
                        i -= 1;
                        continue;
                    }
                }
                bytes.push(c);
            }
        }
        b'B' | b'b' => {
            let (mut bits, mut cache) = (0u32, 0u32);
            for &c in text {
                if matches!(c, b'\r' | b'\n' | b' ' | b'\t' | b'=') {
                    continue;
                }
                let Some(d) = base64_value(c) else {
                    bytes.push(b'?');
                    continue;
                };
                bits += 6;
                cache = (cache << 6) | d;
                if bits == 24 {
                    bytes.extend_from_slice(&cache.to_be_bytes()[1..]);
                    bits = 0;
                    cache = 0;
                }
            }
            let byte = |shift: u32| u8::try_from((cache >> shift) & 0xFF).unwrap_or(0);
            if bits == 18 {
                bytes.push(byte(10));
                bytes.push(byte(2));
            } else if bits == 12 {
                bytes.push(byte(4));
            }
        }
        _ => return None,
    }
    let mut buf = [0u32; 128];
    let mut rest = &bytes[..];
    while !rest.is_empty() {
        let n = incode.decode(&mut rest, &mut buf, state);
        out.write(&buf[..n], false);
    }
    Some(&input[(end + 2).min(input.len())..])
}

/// `mb_mime_header_decode`.
fn header_decode(input: &[u8], outcode: Encoding) -> Vec<u8> {
    let mut out = Encoder::new(
        outcode,
        input.len(),
        u32::from(b'?'),
        MBFL_OUTPUTFILTER_ILLEGAL_MODE_CHAR,
    );
    let mut state = 0u32;
    let mut space_pending = false;
    let mut p = input;
    let space = [u32::from(b' ')];
    let is_ws = |c: u8| matches!(c, b'\n' | b'\r' | b'\t' | b' ');
    while let Some(&c) = p.first() {
        if c == b'=' && p.get(1) == Some(&b'?') && p.len() >= 6 {
            let incode_end = p[2..].iter().position(|&b| b == b'?').map(|at| at + 2);
            if incode_end.is_some_and(|end| p.len() - end >= 3)
                && let Some(rest) = decode_word(p, &mut out, &mut state)
            {
                p = rest;
                if p.first().is_some_and(|&c| is_ws(c)) {
                    let skip = p.iter().position(|&c| !is_ws(c)).unwrap_or(p.len());
                    p = &p[skip..];
                    space_pending = true;
                } else {
                    space_pending = false;
                }
                continue;
            }
        }
        if space_pending {
            out.write(&space, false);
            space_pending = false;
        }
        if c != b'\n' && c != b'\r' {
            let end = 1 + p[1..]
                .iter()
                .position(|&b| matches!(b, b'=' | b'\n' | b'\r'))
                .unwrap_or(p.len() - 1);
            let mut run = &p[..end];
            let mut ascii_state = state;
            let mut buf = [0u32; 128];
            while !run.is_empty() {
                let n = Encoding::ascii().decode(&mut run, &mut buf, &mut ascii_state);
                out.write(&buf[..n], false);
            }
            state = ascii_state;
            p = &p[end..];
        }
        if p.first().is_some_and(|&c| c == b'\n' || c == b'\r') {
            let skip = p.iter().position(|&c| !is_ws(c)).unwrap_or(p.len());
            p = &p[skip..];
            if !p.is_empty() {
                out.write(&space, false);
            }
        }
    }
    out.write(&[], true);
    out.into_vec()
}

/// `mb_decode_mimeheader(string $string): string`.
///
/// ```php
/// var_dump(mb_decode_mimeheader("=?UTF-8?B?QcOxYg==?= =?UTF-8?Q?x=C3=B1_y?="), mb_decode_mimeheader("plain\r\n text"), mb_decode_mimeheader("=?nope?B?QQ==?="));
/// ```
/// ```expect
/// string(9) "Añbxñ y"
/// string(10) "plain text"
/// string(15) "=?nope?B?QQ==?="
/// ```
#[php_function]
fn mb_decode_mimeheader(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let internal = settings(req).current_internal;
    let out = header_decode(string.as_bytes(), internal);
    new_string(req, &out)
}

/// `_php_mbstr_parse_mail_headers`: each header's name lowered, with its
/// value, folded lines joined.
#[allow(clippy::too_many_lines)] // one C state machine, kept whole
fn parse_mail_headers(text: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut state = 0;
    let mut crlf_state: i32 = -1;
    let mut token: Option<usize> = None;
    let mut token_pos = 0usize;
    let mut name: Option<Vec<u8>> = None;
    let mut value: Option<Vec<u8>>;
    let put = |out: &mut Vec<(Vec<u8>, Vec<u8>)>, name: Vec<u8>, value: Vec<u8>| {
        let name = name.to_ascii_lowercase();
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = value,
            None => out.push((name, value)),
        }
    };
    let take = |token: Option<usize>, pos: usize| {
        token
            .filter(|_| pos > 0)
            .map(|t| text[t..(t + pos).min(text.len())].to_vec())
    };
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b':' => {
                if crlf_state == 1 {
                    token_pos += 1;
                }
                if state == 0 || state == 1 {
                    if let Some(n) = take(token, token_pos) {
                        name = Some(n);
                    }
                    state = 2;
                } else {
                    token_pos += 1;
                }
                crlf_state = 0;
            }
            b'\n' => {
                if crlf_state == -1 {
                    break;
                }
                crlf_state = -1;
            }
            b'\r' => {
                if crlf_state == 1 {
                    token_pos += 1;
                } else {
                    crlf_state = 1;
                }
            }
            b' ' | b'\t' => {
                if crlf_state == -1 {
                    state = if state == 3 { 4 } else { 5 };
                } else {
                    if crlf_state == 1 {
                        token_pos += 1;
                    }
                    if state == 1 || state == 3 {
                        token_pos += 1;
                    }
                }
                crlf_state = 0;
            }
            _ => {
                match state {
                    0 => {
                        token = Some(i);
                        token_pos = 0;
                        state = 1;
                    }
                    2 if crlf_state != -1 => {
                        token = Some(i);
                        token_pos = 0;
                        state = 3;
                    }
                    2 | 3 if crlf_state == -1 => {
                        value = take(token, token_pos);
                        if let (Some(n), Some(v)) = (name.take(), value) {
                            put(&mut out, n, v);
                        }
                        token = Some(i);
                        token_pos = 0;
                        state = 1;
                    }
                    4 => {
                        token_pos += 1;
                        state = 3;
                    }
                    _ => {}
                }
                if crlf_state == 1 {
                    token_pos += 1;
                }
                token_pos += 1;
                crlf_state = 0;
            }
        }
        i += 1;
    }
    if state == 2 {
        token = None;
        token_pos = 0;
        state = 3;
    }
    if state == 3 {
        value = take(token, token_pos);
        if let (Some(n), Some(v)) = (name.take(), value) {
            put(&mut out, n, v);
        }
    }
    out
}

fn header<'a>(headers: &'a [(Vec<u8>, Vec<u8>)], name: &[u8]) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_slice())
}

/// `mb_send_mail(string $to, string $subject, string $message, array|string $additional_headers = [], ?string $additional_params = null): bool`.
///
/// ```php
/// foreach ([["a\0b", "s", "m"], ["a", "s", "m", "x\0y"], ["a", "s", "m", ["To" => "b"]], ["a", "s", "m", [1 => "b"]]] as $args) {
///     try { mb_send_mail(...$args); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// ValueError: mb_send_mail(): Argument #1 ($to) must not contain any null bytes
/// ValueError: mb_send_mail(): Argument #4 ($additional_headers) must not contain any null bytes
/// ValueError: The additional headers cannot contain the "To" header
/// TypeError: Header name cannot be numeric, 1 given
/// ```
///
/// `mail.force_extra_parameters`, any value of it and the empty string
/// among them, stands in place of the program's parameters, as `mail()`
/// forces it:
///
/// ```php
/// foreach ([[], ['-d', 'mail.force_extra_parameters=']] as $force) {
///     $p = proc_open([PHP_BINARY, '-n', '-d', 'sendmail_path="cat >/dev/null; echo sent:"', ...$force, '-r', 'var_dump(mb_send_mail("a@example.com", "s", "m", "", "-fme"));'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
///     echo stream_get_contents($pipes[1]); proc_close($p);
/// }
/// ```
/// ```expect process
/// sent: -fme
/// bool(true)
/// sent:
/// bool(true)
/// ```
#[allow(clippy::too_many_lines)] // one C function, kept whole
#[php_function]
fn mb_send_mail(
    req: &mut Request,
    to: Path,
    subject: Path,
    message: Path,
    #[php(default = [])] additional_headers: Union<PhpArray, PhpString>,
    #[php(default = null)] additional_params: Option<Path>,
) -> Result<bool, Thrown> {
    let language = settings(req).language;
    let [charset, header_enc, body_enc] = language.mail();
    let mut tran_cs = Encoding::numbered(charset).unwrap_or_else(Encoding::utf8);
    let head_enc = Encoding::numbered(header_enc).unwrap_or_else(Encoding::base64);
    let mut body_enc = Encoding::numbered(body_enc).unwrap_or_else(Encoding::base64);
    let str_headers: Option<Vec<u8>> = match additional_headers {
        Union::B(text) => {
            if text.as_bytes().contains(&0) {
                return Err(value_error(req, 4, "must not contain any null bytes"));
            }
            let bytes = text.as_bytes();
            let end = bytes
                .iter()
                .rposition(|c| !b" \t\n\r\x0b\0".contains(c))
                .map_or(0, |at| at + 1);
            Some(bytes[..end].to_vec())
        }
        Union::A(array) => Some(php_ext_standard::php_mail_build_headers(req, array)?),
    };
    let headers = str_headers
        .as_deref()
        .map(parse_mail_headers)
        .unwrap_or_default();
    let mut suppress_content_type = false;
    if let Some(value) = header(&headers, b"content-type") {
        if let Some(semi) = value.iter().position(|&b| b == b';') {
            let params = &value[semi + 1..];
            let params = &params[params
                .iter()
                .position(|&b| b != b' ' && b != b'\t')
                .unwrap_or(params.len())..];
            let mut words = params
                .split(|b| b"= ".contains(b))
                .filter(|w| !w.is_empty());
            if words
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case(b"charset"))
            {
                let rest_start = params
                    .iter()
                    .position(|&b| b == b'=' || b == b' ')
                    .unwrap_or(params.len());
                let charset = params[rest_start..]
                    .split(|b| b"= \"".contains(b))
                    .find(|w| !w.is_empty());
                tran_cs = charset
                    .map_or(Some(tran_cs), Encoding::named)
                    .unwrap_or_else(|| {
                        let name =
                            String::from_utf8_lossy(charset.unwrap_or_default()).into_owned();
                        let text =
                            format!("Unsupported charset \"{name}\" - will be regarded as ascii");
                        docref(req, Level::Warning, &text);
                        Encoding::ascii()
                    });
            }
        }
        suppress_content_type = true;
    }
    let mut suppress_transfer_encoding = false;
    if let Some(value) = header(&headers, b"content-transfer-encoding") {
        match Encoding::named(value) {
            Some(enc) if [ENCODING_BASE64, ENCODING_7BIT, ENCODING_8BIT].contains(&enc.no()) => {
                body_enc = enc;
            }
            _ => {
                let name = String::from_utf8_lossy(value).into_owned();
                docref(
                    req,
                    Level::Warning,
                    &format!("Unsupported transfer encoding \"{name}\" - will be regarded as 8bit"),
                );
                body_enc = Encoding::eight_bit();
            }
        }
        suppress_transfer_encoding = true;
    }
    let to = clean_to(to.0.as_bytes());
    let (internal, detect, strict) = {
        let s = settings(req);
        (
            s.current_internal,
            s.detect_order.clone(),
            s.strict_detection,
        )
    };
    let pick = |text: &[u8]| {
        if internal == Encoding::pass() {
            guess(&[text], &detect, strict, false).unwrap_or(internal)
        } else {
            internal
        }
    };
    let mixed = req
        .ini_get(b"mail.mixed_lf_and_crlf")
        .is_some_and(Request::ini_parse_bool);
    let line_sep: &[u8] = if mixed { b"\n" } else { b"\r\n" };
    let subject_bytes = subject.0.as_bytes();
    let indent = i64::try_from("Subject: [PHP-jp nnnnnnnn]".len() + line_sep.len()).unwrap_or(0);
    let subject_enc = pick(subject_bytes);
    let subject = header_encode(
        subject_bytes,
        subject_enc,
        tran_cs,
        head_enc == Encoding::base64(),
        line_sep,
        indent,
    )
    .unwrap_or_else(|| subject_bytes.to_vec());
    let message_bytes = message.0.as_bytes();
    let msg_enc = pick(message_bytes);
    let sub = Substitute::QUESTION;
    let (tmp, _) = fast_convert(message_bytes, msg_enc, tran_cs, sub.char, sub.raw_mode());
    let (body, _) = fast_convert(
        &tmp,
        Encoding::eight_bit(),
        body_enc,
        sub.char,
        sub.raw_mode(),
    );
    let mut out: Vec<u8> = Vec::new();
    if let Some(given) = str_headers.as_deref().filter(|h| !h.is_empty()) {
        let mut len = given.len();
        if given[len - 1] == b'\n' {
            len -= 1;
        }
        if len > 0 && given[len - 1] == b'\r' {
            len -= 1;
        }
        out.extend_from_slice(&given[..len]);
    }
    let sep = |out: &mut Vec<u8>| {
        if !out.is_empty() {
            out.extend_from_slice(line_sep);
        }
    };
    if header(&headers, b"mime-version").is_none() {
        sep(&mut out);
        out.extend_from_slice(b"MIME-Version: 1.0");
    }
    if !suppress_content_type {
        sep(&mut out);
        out.extend_from_slice(b"Content-Type: text/plain");
        if let Some(name) = tran_cs.preferred_mime_name() {
            out.extend_from_slice(b"; charset=");
            out.extend_from_slice(name);
        }
    }
    if !suppress_transfer_encoding {
        sep(&mut out);
        out.extend_from_slice(b"Content-Transfer-Encoding: ");
        out.extend_from_slice(body_enc.preferred_mime_name().unwrap_or(b"7bit"));
    }
    // `zend_ini_str_ex`: a value, even the empty one, is forced over
    // the parameters, as `mail()` forces it.
    let forced = req
        .ini_value(b"mail.force_extra_parameters")
        .map(<[u8]>::to_vec);
    let extra = match forced.or_else(|| additional_params.map(|p| p.0.as_bytes().to_vec())) {
        Some(extra) => Some(php_ext_standard::php_escape_shell_cmd(req, &extra)?),
        None => None,
    };
    php_ext_standard::php_mail(req, &to, &subject, &body, Some(&out), extra.as_deref())
}

/// The `To:` cleaning of `mb_send_mail`: trailing space off, each
/// control byte a space but a folded CRLF and the blanks after it.
fn clean_to(to: &[u8]) -> Vec<u8> {
    let end = to
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(0, |at| at + 1);
    let mut out = to[..end].to_vec();
    let mut i = 0;
    while i < out.len() {
        if out[i].is_ascii_control() {
            if out[i] == b'\r'
                && out.get(i + 1) == Some(&b'\n')
                && matches!(out.get(i + 2), Some(b' ' | b'\t'))
            {
                i += 2;
                while matches!(out.get(i + 1), Some(b' ' | b'\t')) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            out[i] = b' ';
        }
        i += 1;
    }
    out
}

/// `mb_http_input`'s neighbour in `mb_get_info`: the language's mail
/// encodings by name.
pub(crate) fn mail_names(language: Language) -> [&'static [u8]; 3] {
    language
        .mail()
        .map(|no| Encoding::numbered(no).map_or(&b""[..], Encoding::name))
}
