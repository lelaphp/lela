//! `Z/ext/libxml/mime_sniff.c`: the charset of a `Content-Type` header
//! as the WHATWG MIME Sniffing standard parses a MIME type
//! (<https://mimesniff.spec.whatwg.org/#parsing-a-mime-type>), strictly,
//! and the header read off the headers an http stream carries as its
//! `wrapper_data`, which the input a URL gives libxml and
//! `Dom\HTMLDocument::createFromFile` decode by.

use php_ext::raising::Thrown;
use php_ext::request::{Request, Stream};
use php_ext::values::{Key, PhpArray, Value, ValueRef, walk};

/// <https://fetch.spec.whatwg.org/#http-tab-or-space>.
const fn is_http_tab_or_space(c: u8) -> bool {
    matches!(c, 0x09 | 0x20)
}

/// <https://fetch.spec.whatwg.org/#http-whitespace>.
const fn is_http_whitespace(c: u8) -> bool {
    matches!(c, 0x0a | 0x0d) || is_http_tab_or_space(c)
}

/// <https://mimesniff.spec.whatwg.org/#http-quoted-string-token-code-point>.
const fn is_http_quoted_string_token(c: u8) -> bool {
    c == 0x09 || (c >= 0x20 && c != 0x7f)
}

/// <https://mimesniff.spec.whatwg.org/#http-token-code-point>, over php's
/// `char`, so no byte past ASCII is one.
const fn is_http_token(c: u8) -> bool {
    matches!(
        c,
        0x21 | 0x23..=0x27 | 0x2a | 0x2b | 0x2d | 0x2e | 0x5e | 0x5f | 0x60 | 0x7c | 0x7e
    ) || c.is_ascii_alphanumeric()
}

/// <https://infra.spec.whatwg.org/#collect-a-sequence-of-code-points>:
/// how many bytes from `at` the condition holds for.
fn collect(s: &[u8], at: usize, condition: impl Fn(u8) -> bool) -> usize {
    s[at.min(s.len())..]
        .iter()
        .take_while(|c| condition(**c))
        .count()
}

/// Empty, or holding a byte that is no HTTP token.
fn not_a_token(s: &[u8]) -> bool {
    s.is_empty() || !s.iter().all(|c| is_http_token(*c))
}

/// <https://fetch.spec.whatwg.org/#collect-an-http-quoted-string> with
/// `extract-value` true, from the `"` at `at`: the value and where the
/// scan stopped.
fn quoted_string(s: &[u8], at: usize) -> (Vec<u8>, usize) {
    let mut value = Vec::new();
    let mut at = at + 1;
    loop {
        let length = collect(s, at, |c| c != b'"' && c != b'\\');
        value.extend_from_slice(&s[at..at + length]);
        at += length;
        let Some(&quote_or_backslash) = s.get(at) else {
            break;
        };
        at += 1;
        if quote_or_backslash != b'\\' {
            break;
        }
        let Some(&escaped) = s.get(at) else {
            value.push(b'\\');
            break;
        };
        value.push(escaped);
        at += 1;
    }
    (value, at)
}

/// `php_libxml_sniff_charset_from_string` (`Z/ext/libxml/mime_sniff.c:192`):
/// the value of the first `charset` parameter of a MIME type made of
/// quoted-string tokens alone, `None` where the type or the subtype is not
/// one or no such parameter follows.
pub(crate) fn sniff_charset_from_string(header: &[u8]) -> Option<Vec<u8>> {
    let start = header
        .iter()
        .position(|c| !is_http_whitespace(*c))
        .unwrap_or(header.len());
    let end = header
        .iter()
        .rposition(|c| !is_http_whitespace(*c))
        .map_or(start, |at| at + 1);
    let s = &header[start..end.max(start)];
    let type_length = collect(s, 0, |c| c != b'/');
    if not_a_token(&s[..type_length]) {
        return None;
    }
    let mut at = type_length;
    if at >= s.len() {
        return None;
    }
    at += 1;
    let subtype_length = collect(s, at, |c| c != b';');
    if not_a_token(&s[at..at + subtype_length]) {
        return None;
    }
    at += subtype_length;
    while at < s.len() {
        at += 1;
        at += collect(s, at, is_http_whitespace);
        let name_length = collect(s, at, |c| c != b';' && c != b'=');
        let name = &s[at.min(s.len())..(at + name_length).min(s.len())];
        at += name_length;
        if at >= s.len() {
            break;
        }
        if s[at] == b';' {
            continue;
        }
        at += 1;
        let value = if s.get(at) == Some(&b'"') {
            let (value, stopped) = quoted_string(s, at);
            at = stopped + collect(s, stopped, |c| c != b';');
            value
        } else {
            let length = collect(s, at, |c| c != b';');
            let mut value = s[at.min(s.len())..(at + length).min(s.len())].to_vec();
            at += length;
            while value.last().is_some_and(|c| is_http_whitespace(*c)) {
                value.pop();
            }
            if value.is_empty() {
                continue;
            }
            value
        };
        if name.eq_ignore_ascii_case(b"charset")
            && value.iter().all(|c| is_http_quoted_string_token(*c))
        {
            return Some(value);
        }
    }
    None
}

/// `php_libxml_sniff_charset_from_stream` (`Z/ext/libxml/mime_sniff.c:306`):
/// where the stream's `wrapper_data` is an array of header lines, the
/// last response's `Content-Type` found by walking it back to the first
/// line with no colon, or no blank after one, which php takes for a
/// status line, and its charset sniffed.
///
/// ```php
/// $srv = stream_socket_server('tcp://127.0.0.1:0');
/// $addr = stream_socket_get_name($srv, false);
/// if (pcntl_fork() === 0) {
///     $c = stream_socket_accept($srv);
///     fread($c, 1024);
///     fwrite($c, "HTTP/1.0 200 OK\r\nContent-Type: text/xml; charset=\"ISO-8859-1\" ; x=y\r\n\r\n<?xml version=\"1.0\"?>\n<r>\xE4</r>\n");
///     fclose($c);
///     exit(0);
/// }
/// $d = new DOMDocument;
/// var_dump($d->load("http://$addr/"), bin2hex($d->documentElement->textContent));
/// pcntl_wait($status);
/// ```
/// ```expect process
/// bool(true)
/// string(4) "c3a4"
/// ```
///
/// `stream->wrapperdata` is read off the stream ([`Stream::wrapper_data`]),
/// so no method of a user wrapper runs, its `stream_eof` among them: the
/// one warning below is the parse's own read.
///
/// ```php
/// class W { public $context; function stream_open($p,$m,$o,&$op){return true;} function stream_read($n){ static $d = 0; return $d++ ? '' : "<a>x</a>"; } function stream_close(){} function stream_stat(){return [];} function url_stat($p,$f){return [];} }
/// stream_wrapper_register('ww', 'W');
/// $d = new DOMDocument; var_dump($d->load('ww://x'));
/// ```
/// ```expect
/// Warning: DOMDocument::load(): W::stream_eof is not implemented! Assuming EOF in %SCRATCH%/witness.php on line 4
///
/// Warning: DOMDocument::load(): Document is empty in ww://x, line: 1 in %SCRATCH%/witness.php on line 4
/// bool(false)
/// ```
pub fn sniff_charset_from_stream(
    req: &mut Request,
    stream: Stream,
) -> Result<Option<Vec<u8>>, Thrown> {
    let headers = stream
        .wrapper_data(req)
        .and_then(|data| data.deref().as_array());
    Ok(headers.and_then(|headers| charset_of_headers(req, headers)))
}

/// The charset of the last `Content-Type:` line among the wrapper's
/// header lines, read from the last line back as php's loop reads them;
/// `None` where a line that is not a header comes first.
fn charset_of_headers(req: &Request, headers: PhpArray) -> Option<Vec<u8>> {
    let lines: Vec<(Key, Value)> = walk::rows(req, headers).collect();
    for (_, line) in lines.iter().rev() {
        let line = line.deref();
        let ValueRef::Str(line) = line.as_ref() else {
            continue;
        };
        let line = line.as_bytes();
        let colon = line.iter().position(|c| *c == b':')?;
        // `space < colon` with no blank at all is a null pointer, which
        // compares below any other: php bails out then too.
        if line
            .iter()
            .position(|c| *c == b' ')
            .is_none_or(|space| space < colon)
        {
            return None;
        }
        if line.len() >= 13 && line[..13].eq_ignore_ascii_case(b"content-type:") {
            return sniff_charset_from_string(&line[13..]);
        }
    }
    None
}
