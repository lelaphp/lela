//! `php_http.c`: the HTTP client a `SoapClient` sends its requests with.
//! The headers its credentials and the `http.header` option of its
//! stream add, which the WSDL load shares; the connection, through a
//! proxy and over TLS, kept open in the client's `httpsocket` and
//! `httpurl` between calls; the request line and the headers in php's
//! order, the cookies and the digest credentials the server asked for;
//! the response's head and body, redirects followed, and every failure
//! a fault on the client, worded as php words it.

use std::time::Duration;

use php_ext::abi::Callable;
use php_ext::call::{call, lookup_function, new_object};
use php_ext::coerce::{atoi, c_str, is_space, numeric_string, to_callable};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw};
use php_ext::request::{
    Handshake, OwnedStream as _, PHP_VERSION, Request, SocketOptions, Stream, StreamCtx,
    Streams as _, Xport,
};
use php_ext::values::{
    Emalloc, FreshString, Key, Owner, PhpArray, PhpObject, PhpString, SmartStr, Value, ValueRef,
    walk,
};
use php_ext_standard::md5::Md5;
use php_ext_standard::{UriParser, base64};
use php_ext_uri::{Parser, UriStruct};

use crate::Res;
use crate::classes::{HeldUri, UrlPayload};
use crate::constants::{SOAP_1_2, SOAP_COMPRESSION_ACCEPT, SOAP_COMPRESSION_DEFLATE};
use crate::shared::{
    ClientSlot, add_soap_fault, class_id, client_slot, instance_of, set_client_slot,
};
use crate::throwables::Throwables;

type R<T> = Result<T, Thrown>;

/// `Authorization: Basic` or `Proxy-Authorization: Basic` over `login`
/// and an optional password.
fn basic(soap_headers: &mut Vec<u8>, header: &[u8], login: &[u8], password: Option<&[u8]>) {
    let mut auth = login.to_vec();
    auth.push(b':');
    if let Some(p) = password {
        auth.extend_from_slice(p);
    }
    soap_headers.extend_from_slice(header);
    soap_headers.extend_from_slice(b": Basic ");
    soap_headers.extend_from_slice(&base64::encode(&auth));
    soap_headers.extend_from_slice(b"\r\n");
}

/// `proxy_authentication`: the client's `proxy_login` and
/// `proxy_password` as `Proxy-Authorization: Basic`, where it has a
/// login.
pub(crate) fn proxy_authentication(this: PhpObject, soap_headers: &mut Vec<u8>) -> bool {
    let login = client_slot(this, ClientSlot::ProxyLogin);
    let Some(login) = login.as_str_bytes() else {
        return false;
    };
    let password = client_slot(this, ClientSlot::ProxyPassword);
    basic(
        soap_headers,
        b"Proxy-Authorization",
        login,
        password.as_str_bytes(),
    );
    true
}

/// `basic_authentication`: the client's `login` and `password` as
/// `Authorization: Basic`, where it has a login and does not use digest.
pub(crate) fn basic_authentication(this: PhpObject, soap_headers: &mut Vec<u8>) -> bool {
    let login = client_slot(this, ClientSlot::Login);
    let digest = client_slot(this, ClientSlot::UseDigest);
    let Some(login) = login.as_str_bytes() else {
        return false;
    };
    if matches!(digest.as_ref(), ValueRef::Bool(true)) {
        return false;
    }
    let password = client_slot(this, ClientSlot::Password);
    basic(
        soap_headers,
        b"Authorization",
        login,
        password.as_str_bytes(),
    );
    true
}

/// `http_context_add_header`: each `name: value` line of `s` appended,
/// but the headers the client writes itself.
fn http_context_add_header(
    s: &[u8],
    has_authorization: bool,
    has_proxy_authorization: bool,
    has_cookies: bool,
    soap_headers: &mut Vec<u8>,
) {
    let mut at = 0;
    while at < s.len() {
        while at < s.len() && matches!(s[at], b' ' | b'\t' | b'\r' | b'\n') {
            at += 1;
        }
        let start = at;
        let mut p = at;
        let mut name_len: Option<usize> = None;
        while p < s.len() {
            match s[p] {
                b':' => {
                    if name_len.is_none() {
                        name_len = Some(p - start);
                    }
                    break;
                }
                b' ' | b'\t' if name_len.is_none() => name_len = Some(p - start),
                b'\r' | b'\n' => break,
                _ => {}
            }
            p += 1;
        }
        if p < s.len() && s[p] == b':' {
            while p < s.len() && s[p] != b'\r' && s[p] != b'\n' {
                p += 1;
            }
            let name = &s[start..start + name_len.unwrap_or(0)];
            let is = |n: &[u8]| name.eq_ignore_ascii_case(n);
            let skipped = is(b"host")
                || is(b"connection")
                || is(b"user-agent")
                || is(b"content-length")
                || is(b"content-type")
                || (has_cookies && is(b"cookie"))
                || (has_authorization && is(b"authorization"))
                || (has_proxy_authorization && is(b"proxy-authorization"));
            if !skipped {
                soap_headers.extend_from_slice(&s[start..p]);
                soap_headers.extend_from_slice(b"\r\n");
            }
        }
        at = if p < s.len() { p + 1 } else { p };
    }
}

/// `http_context_headers`: the `http.header` option of `context`, a
/// string or an array of strings, through [`http_context_add_header`].
pub(crate) fn http_context_headers(
    req: &Request,
    context: Option<StreamCtx>,
    has_authorization: bool,
    has_proxy_authorization: bool,
    has_cookies: bool,
    soap_headers: &mut Vec<u8>,
) {
    let Some(context) = context else {
        return;
    };
    let Some(header) = context.option(req, b"http", b"header") else {
        return;
    };
    let flags = (has_authorization, has_proxy_authorization, has_cookies);
    match header.as_ref() {
        ValueRef::Str(s) if !s.is_empty() => {
            http_context_add_header(s.as_bytes(), flags.0, flags.1, flags.2, soap_headers);
        }
        ValueRef::Array(a) => {
            for (_, value) in walk::rows(req, a) {
                if let ValueRef::Str(s) = value.deref().as_ref()
                    && !s.is_empty()
                {
                    http_context_add_header(s.as_bytes(), flags.0, flags.1, flags.2, soap_headers);
                }
            }
        }
        _ => {}
    }
}

// ---- the C string reads php_http.c makes

/// `STREAM_CRYPTO_METHOD_SSLv2_CLIENT` and its kin, the methods the
/// tunnel through a proxy is secured with.
const SSLV2_CLIENT: i64 = (1 << 1) | 1;
const SSLV3_CLIENT: i64 = (1 << 2) | 1;
const SSLV23_CLIENT: i64 = (1 << 3) | (1 << 4) | (1 << 5) | 1;
const TLS_CLIENT: i64 = (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | 1;

/// `SOAP_SSL_METHOD_TLS` and its kin, the client's `ssl_method`.
const SSL_METHOD_TLS: i64 = crate::constants::SOAP_SSL_METHOD_TLS;
const SSL_METHOD_SSLV2: i64 = crate::constants::SOAP_SSL_METHOD_SSLv2;
const SSL_METHOD_SSLV3: i64 = crate::constants::SOAP_SSL_METHOD_SSLv3;
const SSL_METHOD_SSLV23: i64 = crate::constants::SOAP_SSL_METHOD_SSLv23;

/// `char headerbuf[8192]`: what one `php_stream_gets` fills, the line
/// cut at 8191 bytes.
const HEADER_BUF: usize = 8192;

/// The byte at `i` of a NUL-terminated string, its NUL past the end.
fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// `strncmp(left, right, n) == 0` over two NUL-terminated strings.
fn strncmp_eq(left: &[u8], right: &[u8], n: usize) -> bool {
    compare_n(left, right, n, false)
}

/// `strncasecmp(left, right, n) == 0` over two NUL-terminated strings.
fn strncasecmp_eq(left: &[u8], right: &[u8], n: usize) -> bool {
    compare_n(left, right, n, true)
}

/// The first `n` bytes of two NUL-terminated strings equal, up to the
/// first NUL, ASCII case folded where `fold` asks.
fn compare_n(left: &[u8], right: &[u8], n: usize, fold: bool) -> bool {
    let (left, right) = (c_str(left), c_str(right));
    for i in 0..n {
        let (mut x, mut y) = (at(left, i), at(right, i));
        if fold {
            x = x.to_ascii_lowercase();
            y = y.to_ascii_lowercase();
        }
        if x != y {
            return false;
        }
        if x == 0 {
            break;
        }
    }
    true
}

/// Where `needle` first stands in `hay`, `strchr`.
fn find(hay: &[u8], needle: u8) -> Option<usize> {
    hay.iter().position(|c| *c == needle)
}

/// C's `(int)` of a `long`, as php stores a `zend_long` `port` and a
/// length in an `int`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "C's conversion to `int`, which php's `int port` and `int buf_size` make"
)]
fn int_of(n: i64) -> i32 {
    n as i32
}

/// C's conversion of a `zend_long` to `zend_ulong`, as
/// `smart_str_append_unsigned` prints a `port`.
#[expect(
    clippy::cast_sign_loss,
    reason = "`smart_str_append_unsigned` of a `zend_long` port"
)]
fn unsigned_of(n: i64) -> u64 {
    n as u64
}

/// `sscanf(line, "%x", &buf_size) > 0`: the hex number at the start of
/// a chunk's size line, its blanks, a sign and a `0x` read past, into the
/// `unsigned int` php 8.5.12 hands it; none where no digit stands there.
fn scan_hex(line: &[u8]) -> Option<u32> {
    let b = c_str(line);
    let mut i = 0;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let negative = match b.get(i) {
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
    if at(b, i) == b'0' && matches!(at(b, i + 1), b'x' | b'X') && at(b, i + 2).is_ascii_hexdigit() {
        i += 2;
    }
    let start = i;
    let mut n: u64 = 0;
    while let Some(d) = b.get(i).and_then(|d| char::from(*d).to_digit(16)) {
        n = n.saturating_mul(16).saturating_add(u64::from(d));
        i += 1;
    }
    if i == start {
        return None;
    }
    let n = if negative { n.wrapping_neg() } else { n };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`%x` stores into the `unsigned int` php hands it"
    )]
    let n = n as u32;
    Some(n)
}

/// `is_numeric_string(s, len, &lval, NULL, 1)` answering `IS_LONG`: the
/// integer the string's numeric prefix is, none for a float or no
/// number.
fn numeric_long(s: &[u8]) -> Option<i64> {
    (1..=s.len())
        .rev()
        .find_map(|end| numeric_string(&s[..end]))
        .and_then(|v| v.as_int())
}

/// `Z_DVAL_P(tmp) == 1.0`.
#[expect(clippy::float_cmp, reason = "php compares the double exactly")]
fn is_one(v: f64) -> bool {
    v == 1.0
}

/// The lowercase hex of a digest, `php_hash_bin2hex` and `make_digest`.
fn hex(digest: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    digest
        .iter()
        .flat_map(|b| [DIGITS[usize::from(b >> 4)], DIGITS[usize::from(b & 15)]])
        .collect()
}

/// The md5 of the parts in order, its hex, as `PHP_MD5Init`, the
/// updates, `PHP_MD5Final` and `make_digest` make it.
fn md5_hex(parts: &[&[u8]]) -> Vec<u8> {
    let mut ctx = Md5::new();
    for part in parts {
        ctx.update(part);
    }
    hex(&ctx.finish())
}

// ---- the parsed URL

/// `uri->port` as php's `php_uri` holds it, `0` for none.
fn port_of(uri: &UriStruct) -> i64 {
    uri.port.unwrap_or(0)
}

/// `ZSTR_VAL(uri->host)`, the empty string for none.
fn host_of(uri: &UriStruct) -> &[u8] {
    uri.host.as_deref().unwrap_or_default()
}

/// `php_uri_get_parser`: `parse_url` where no class is named, else the
/// parser registered under the name, `parse_url`'s own among them.
fn uri_parser(name: Option<&[u8]>) -> Option<UriParser> {
    match name {
        None | Some(b"parse_url") => Some(UriParser::ParseUrl),
        Some(name) => Parser::named(name).map(UriParser::Uri),
    }
}

/// The request target: the path, `/` for none, then the query and the
/// fragment, as the request line and the digest's `uri` write it.
fn target(uri: &UriStruct, out: &mut Vec<u8>) {
    match &uri.path {
        Some(path) => out.extend_from_slice(c_str(path)),
        None => out.push(b'/'),
    }
    if let Some(query) = &uri.query {
        out.push(b'?');
        out.extend_from_slice(c_str(query));
    }
    if let Some(fragment) = &uri.fragment {
        out.push(b'#');
        out.extend_from_slice(c_str(fragment));
    }
}

// ---- the stream

/// `php_stream_gets(stream, headerbuf, sizeof(headerbuf))`: the next
/// line, at most 8191 bytes of it, none at the end.
fn gets(req: &mut Request, stream: Stream) -> R<Option<Vec<u8>>> {
    stream.get_line(req, Some(HEADER_BUF))
}

/// `php_stream_getc`: one byte, none for php's `EOF`.
fn getc(req: &mut Request, stream: Stream) -> Option<u8> {
    let mut byte = [0];
    match stream.read_into(req, &mut byte) {
        Some(1) => Some(byte[0]),
        _ => None,
    }
}

/// Whether a header line is the blank one that ends a head.
fn is_blank_line(line: &[u8]) -> bool {
    line.starts_with(b"\r\n") || line.first() == Some(&b'\n')
}

/// `get_http_headers`: the response's head, each line up to the blank
/// one as the stream gave it, gathered in a `smart_str` on the request's
/// heap and handed out as that string, `tmp_response.s`, at the room its
/// appends grew it to, which the caller holds until php releases its
/// `http_headers`; none where the stream ended first, where the blank
/// line came before any, or, since 8.5.12, once the memory limit was met
/// after a line (`ZEND_HAS_OOM()`).
fn get_http_headers(req: &mut Request, stream: Stream) -> R<Option<Owner<PhpString>>> {
    // `smart_str` holds no string until the first append.
    let mut response = SmartStr::default();
    let mut any = false;
    loop {
        let line = match gets(req, stream) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(thrown) => {
                response.free(req);
                return Err(thrown);
            }
        };
        if is_blank_line(&line) {
            if !any {
                response.free(req);
                return Ok(None);
            }
            return response.finish(req).map(Some);
        }
        if let Err(thrown) = response.append(req, c_str(&line)) {
            response.free(req);
            return Err(thrown);
        }
        any = true;
        if req.memory().has_oom() {
            break;
        }
    }
    response.free(req);
    Ok(None)
}

/// `get_http_header_value_nodup`: where the value of the first line of
/// `headers` that starts with `name`, in any case, begins and how long
/// it is, its leading blanks left out, and its `\r` and trailing blanks
/// where a `\n` ends it.
fn header_value_at(headers: &[u8], name: &[u8]) -> Option<(usize, usize)> {
    let headers = c_str(headers);
    let mut pos = 0;
    loop {
        if strncasecmp_eq(&headers[pos..], name, name.len()) {
            let mut value = pos + name.len();
            while matches!(headers.get(value), Some(b' ' | b'\t')) {
                value += 1;
            }
            let eol = match find(&headers[value..], b'\n') {
                None => headers.len(),
                Some(n) => {
                    let mut eol = value + n;
                    if eol > value {
                        if headers[eol - 1] == b'\r' {
                            eol -= 1;
                        }
                        while eol > value && matches!(headers[eol - 1], b' ' | b'\t') {
                            eol -= 1;
                        }
                    }
                    eol
                }
            };
            return Some((value, eol - value));
        }
        pos += find(&headers[pos..], b'\n')? + 1;
    }
}

/// `get_http_header_value`: the value [`header_value_at`] finds, copied.
fn get_http_header_value(headers: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    header_value_at(headers, name).map(|(at, len)| headers[at..at + len].to_vec())
}

/// `ZSTR_MAX_LEN` (`Z/Zend/zend_string.h:109`): `SIZE_MAX` less a
/// string's aligned header and terminator.
const ZSTR_MAX_LEN: usize = usize::MAX - 32;

/// `http_buf` and `http_buf_size`: the string php 8.5.12 reads a body
/// into, on the request's heap, asked for and grown fallibly where php's
/// is, and how many bytes were read into it. The string is the body php
/// hands on, at the room its growths left it.
#[derive(Default)]
struct HttpBuf {
    string: Option<FreshString>,
    size: usize,
}

impl HttpBuf {
    /// The buffer grown to `len` bytes, `zend_string_falloc` or
    /// `zend_string_safe_frealloc` where `fallibly`, else
    /// `zend_string_alloc`; `false` where the memory limit refused it, the
    /// buffer then gone as php's `zend_string_release` lets it go.
    fn grow(&mut self, req: &mut Request, len: usize, fallibly: bool) -> R<bool> {
        let held = self.string.take();
        let ask = |req: &mut Request| match held {
            None => FreshString::new(req, len),
            Some(held) => held.extend(req, len),
        };
        let grown = if fallibly {
            req.memory_mut().fallible(ask)?
        } else {
            Some(ask(req)?)
        };
        let refused = grown.is_none();
        self.string = grown;
        Ok(!refused)
    }

    /// `php_stream_read(stream, http_buf->val + http_buf_size, len)`, at
    /// most `len` bytes into the room past those read: how many it took,
    /// none for a read that took nothing.
    fn read(&mut self, req: &mut Request, stream: Stream, len: usize) -> Option<usize> {
        let string = self.string.as_mut()?;
        let room = string.bytes_mut().get_mut(self.size..self.size + len)?;
        let read = stream.read_into(req, room).filter(|n| *n > 0)?;
        self.size += read;
        Some(read)
    }

    /// `http_buf->len = http_buf_size` and the string handed on, its room
    /// kept; `ZSTR_EMPTY_ALLOC()` where none was asked for.
    fn finish(self, req: &mut Request) -> R<Owner<PhpString>> {
        match self.string {
            Some(mut string) => {
                string.set_len(self.size);
                Ok(string.finish())
            }
            None => PhpString::new(req, b""),
        }
    }

    /// The buffer let go, `zend_string_release`.
    fn free(self, req: &mut Request) {
        if let Some(held) = self.string {
            held.free(req);
        }
    }
}

/// `get_http_body`: the response's body, chunked, of its
/// `Content-Length` or to the end of a connection that closes, read into
/// `http_buf` and handed on as that string; none where it can be read
/// none of these ways, its chunks are broken, or, since 8.5.12, the
/// memory limit refused its buffer's room.
///
/// The bytes a call holds at its peak and after it are php's: the body's
/// string, the head's `smart_str` and the status's words held through
/// the read, and the client's `Soap\Url`:
///
/// ```php
/// $srv = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; $c = stream_socket_accept($s, 10); $n = 0;'
///     . ' while (($l = fgets($c)) !== false && rtrim($l) !== "") if (preg_match("/^Content-Length: (\d+)/i", $l, $m)) $n = (int) $m[1];'
///     . ' if ($n) fread($c, $n); fwrite($c, $argv[1]); for ($i = 0; $i < (int) $argv[3]; $i++) fwrite($c, $argv[2]); fwrite($c, $argv[4]); fclose($c);';
/// foreach ([["Content-Length: 10", str_repeat("x", 10), "1", ""], ["Content-Length: 1000", str_repeat("x", 1000), "1", ""], ["Content-Length: 300000", str_repeat("x", 1000), "300", ""], ["Transfer-Encoding: chunked", "1000\r\n" . str_repeat("x", 0x1000) . "\r\n", "73", "0\r\n\r\n"]] as [$head, $piece, $count, $tail]) {
///     $p = proc_open([PHP_BINARY, '-n', '-r', $srv, "HTTP/1.1 200 OK\r\n$head\r\n\r\n", $piece, $count, $tail], [1 => ['pipe', 'w']], $pipes);
///     $uri = 'http://' . trim(fgets($pipes[1]));
///     $c = new SoapClient(null, ['location' => $uri, 'uri' => 'u']);
///     memory_reset_peak_usage(); $m = memory_get_usage();
///     $r = $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1);
///     echo strlen($r), ' ', memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
///     unset($r, $c); proc_close($p);
/// }
/// ```
/// ```expect process
/// 10 9144 8904
/// 1000 10384 10112
/// 300000 312208 311936
/// 299008 312208 311936
/// ```
///
/// A body read to the close grows 4096 bytes past what it read until
/// `php_stream_eof` says the peer closed, which asks without waiting:
/// where the peer's close came before the last read, no growth follows
/// it; where the peer holds the connection open past its last byte, one
/// more does, and the string handed on keeps that room. Which of the two
/// a peer that closes as it writes meets is the scheduler's, under the
/// oracle and this engine alike, so each peer here orders its close:
///
/// ```php
/// $srv = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; $c = stream_socket_accept($s, 10);'
///     . ' while (($l = fgets($c)) !== false && rtrim($l) !== "") {} fread($c, 4); $close = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";'
///     . ' if ($argv[1] === "first") { fwrite($c, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok" . $close . str_repeat("x", 12272)); stream_socket_shutdown($c, STREAM_SHUT_WR); while (fgets($c) !== false); }'
///     . ' else { fwrite($c, $close); for ($i = 0; $i < 300; $i++) fwrite($c, str_repeat("x", 1000)); usleep(1000000); }'
///     . ' fclose($c);';
/// foreach (["first", "held"] as $peer) {
///     $p = proc_open([PHP_BINARY, '-n', '-r', $srv, $peer], [1 => ['pipe', 'w']], $pipes);
///     $uri = 'http://' . trim(fgets($pipes[1]));
///     $c = new SoapClient(null, ['location' => $uri, 'uri' => 'u']);
///     if ($peer === 'first') { $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1); usleep(100000); }
///     memory_reset_peak_usage(); $m = memory_get_usage();
///     $r = $c->__doRequest('<E/>', $uri, 'f', SOAP_1_1);
///     echo $peer, ' ', strlen($r), ' ', memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
///     unset($r, $c); proc_close($p);
/// }
/// ```
/// ```expect process
/// first 12272 16656 7768
/// held 300000 316304 307384
/// ```
fn get_http_body(
    req: &mut Request,
    stream: Stream,
    close: bool,
    headers: &[u8],
) -> R<Option<Owner<PhpString>>> {
    let mut header_close = close;
    if !close
        && let Some(header) = get_http_header_value(headers, b"Connection:")
        && strncasecmp_eq(&header, b"close", 5)
    {
        header_close = true;
    }
    let chunked = get_http_header_value(headers, b"Transfer-Encoding:")
        .is_some_and(|header| strncasecmp_eq(&header, b"chunked", 7));
    let mut header_length = 0;
    if let Some(header) = get_http_header_value(headers, b"Content-Length:") {
        header_length = atoi(&header);
        if header_length == 0 && !chunked {
            // Empty response.
            return PhpString::new(req, b"").map(Some);
        }
    }
    if chunked {
        return chunked_body(req, stream);
    }
    if header_length != 0 {
        // `header_length >= ZSTR_MAX_LEN` since 8.5.12, which an `int`
        // never reaches here.
        let Ok(want) = usize::try_from(header_length) else {
            return Ok(None);
        };
        let mut buf = HttpBuf::default();
        if !buf.grow(req, want, true)? {
            return Ok(None);
        }
        while buf.size < want {
            let left = want - buf.size;
            if buf.read(req, stream, left).is_none() {
                break;
            }
        }
        return buf.finish(req).map(Some);
    }
    if header_close {
        let mut buf = HttpBuf::default();
        loop {
            // The first room is `zend_string_alloc(4096)`, each one after
            // it grown by 4096 fallibly and bounded, since 8.5.12.
            let first = buf.string.is_none();
            if !first && buf.size >= ZSTR_MAX_LEN - 4096 {
                buf.free(req);
                return Ok(None);
            }
            if !buf.grow(req, buf.size + 4096, !first)? {
                return Ok(None);
            }
            buf.read(req, stream, 4096);
            if stream.eof(req) {
                break;
            }
        }
        return buf.finish(req).map(Some);
    }
    Ok(None)
}

/// The chunked arm of [`get_http_body`]: each chunk's size line read as
/// `%x`, the chunk and its `\r\n`, to the chunk of size zero, then the
/// trailer headers read past. A size line that does not parse is read
/// again from what the last `gets` left, as php's buffer keeps it. Since
/// 8.5.12 the size is unsigned, refused past `ZSTR_MAX_LEN`, and each
/// chunk's room asked for fallibly.
fn chunked_body(req: &mut Request, stream: Stream) -> R<Option<Owner<PhpString>>> {
    let mut buf = HttpBuf::default();
    match chunks(req, stream, &mut buf) {
        Ok(true) => buf.finish(req).map(Some),
        Ok(false) => {
            buf.free(req);
            Ok(None)
        }
        Err(thrown) => {
            buf.free(req);
            Err(thrown)
        }
    }
}

/// [`chunked_body`]'s reads, into `buf`'s room: `false` where php
/// answers none.
fn chunks(req: &mut Request, stream: Stream, buf: &mut HttpBuf) -> R<bool> {
    let mut size_line: Vec<u8> = Vec::new();
    let mut done = false;
    while !done {
        if let Some(line) = gets(req, stream)? {
            size_line = line;
        }
        let Some(buf_size) = scan_hex(&size_line) else {
            // Something wrong in chunked encoding.
            return Ok(false);
        };
        if buf_size > 0 {
            let want = usize::try_from(buf_size).unwrap_or(usize::MAX);
            if want >= ZSTR_MAX_LEN - buf.size {
                return Ok(false);
            }
            if !buf.grow(req, buf.size + want, true)? {
                return Ok(false);
            }
            let mut len_size = 0;
            while len_size < want {
                let Some(read) = buf.read(req, stream, want - len_size) else {
                    // Error or EOF.
                    done = true;
                    break;
                };
                len_size += read;
            }
            // Eat up '\r' '\n'.
            let mut ch = getc(req, stream);
            if ch == Some(b'\r') {
                ch = getc(req, stream);
            }
            if ch != Some(b'\n') {
                // Something wrong in chunked encoding.
                return Ok(false);
            }
        }
        if buf_size == 0 {
            done = true;
        }
    }
    // Ignore trailer headers.
    while let Some(line) = gets(req, stream)? {
        if is_blank_line(&line) {
            break;
        }
    }
    Ok(true)
}

/// `FG(default_socket_timeout)` as `php_stream_xport_create` makes it
/// the connect's timeout where none is given: none, no deadline, for
/// one below zero.
fn default_timeout(req: &Request) -> Option<Duration> {
    let secs = req.ini().long(b"default_socket_timeout");
    u64::try_from(secs).ok().map(Duration::from_secs)
}

/// `http_connect`: a stream to the host `uri` names, or to the client's
/// proxy, which `use_proxy` is then set for; over the `ssl_method`'s TLS
/// where `use_ssl` asks and no proxy stands between, or through a
/// `CONNECT` the proxy answers and the method php picks for the tunnel.
/// None where the connect, the `CONNECT` or the handshake failed.
///
/// `error_reporting` is lowered by `E_WARNING`, `E_NOTICE`,
/// `E_USER_WARNING` and `E_USER_NOTICE` around the whole of it
/// (`php_http.c:192-193`, `:323`), so the warning a refused connect
/// raises is not shown and still reaches `error_get_last()` and a handler;
/// the client's `stream_context` is handed to the connect, which reads
/// its `socket` options (`bindto`) itself.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0'); $at = stream_socket_get_name($s, false); fclose($s);
/// $c = new SoapClient(null, ['location' => "http://$at/", 'uri' => 'urn:x']);
/// try { $c->f(); } catch (SoapFault $e) { echo $e->getMessage(), "\n"; }
/// echo str_replace($at, 'ADDR', error_get_last()['message']), "\n";
/// set_error_handler(function ($no, $str) { echo "$no $str\n"; return true; });
/// try { $c->f(); } catch (SoapFault $e) { echo $e->getMessage(), "\n"; }
/// var_dump(error_reporting() === E_ALL);
/// ini_set('default_socket_timeout', 1);
/// $s = stream_socket_server('tcp://127.0.0.1:0'); $at = stream_socket_get_name($s, false);
/// $f = stream_socket_server('tcp://127.0.0.1:0'); $from = stream_socket_get_name($f, false); fclose($f);
/// $c = new SoapClient(null, ['location' => "http://$at/", 'uri' => 'urn:x', 'stream_context' => stream_context_create(['socket' => ['bindto' => $from]])]);
/// try { $c->f(); } catch (SoapFault $e) { echo $e->getMessage(), "\n"; }
/// var_dump(stream_socket_get_name(stream_socket_accept($s, 1), true) === $from);
/// ```
/// ```expect
/// Could not connect to host
/// SoapClient::__doRequest(): connect() failed: Connection refused
/// 2 SoapClient::__doRequest(): connect() failed: Connection refused
/// Could not connect to host
/// bool(true)
/// Error Fetching http headers
/// bool(true)
/// ```
///
/// The connect asks for no `error_string`, so the warning is
/// `php_stream_xport_create`'s own ([`XportFailure::warning`]): an
/// xport no extension registers, as `SOAP_SSL_METHOD_SSLv2` names over
/// `https`, is php's text bare, with no `connect() failed: ` before
/// it.
///
/// ```php
/// $c = @new SoapClient(null, ['location' => 'https://127.0.0.1:1/', 'uri' => 'urn:x', 'ssl_method' => SOAP_SSL_METHOD_SSLv2]);
/// try { $c->f(); } catch (SoapFault $e) { echo $e->getMessage(), "\n"; }
/// echo error_get_last()['message'], "\n";
/// ```
/// ```expect
/// Could not connect to host
/// SoapClient::__doRequest(): Unable to find the socket transport "sslv2" - did you forget to enable it when you configured PHP?
/// ```
///
/// [`XportFailure::warning`]: php_ext::request::XportFailure::warning
fn http_connect(
    req: &mut Request,
    this: PhpObject,
    uri: &UriStruct,
    use_ssl: bool,
    context: Option<StreamCtx>,
    use_proxy: &mut bool,
) -> R<Option<Owner<Stream>>> {
    const MASKED: [Level; 4] = [
        Level::Warning,
        Level::Notice,
        Level::UserWarning,
        Level::UserNotice,
    ];
    req.diagnostics_mut().reporting_without(&MASKED, |req| {
        connect(req, this, uri, use_ssl, context, use_proxy)
    })
}

/// [`http_connect`] under its lowered `error_reporting`.
fn connect(
    req: &mut Request,
    this: PhpObject,
    uri: &UriStruct,
    use_ssl: bool,
    context: Option<StreamCtx>,
    use_proxy: &mut bool,
) -> R<Option<Owner<Stream>>> {
    let proxy_host = client_slot(this, ClientSlot::ProxyHost);
    let proxy_port = client_slot(this, ClientSlot::ProxyPort);
    let (host, port) = match (proxy_host.as_str_bytes(), proxy_port.as_int()) {
        (Some(host), Some(port)) => {
            *use_proxy = true;
            (c_str(host).to_vec(), int_of(port))
        }
        _ => (c_str(host_of(uri)).to_vec(), int_of(port_of(uri))),
    };
    let timeout = match client_slot(this, ClientSlot::ConnectionTimeout).as_int() {
        Some(secs) if secs > 0 => Some(Duration::from_secs(secs.unsigned_abs())),
        _ => default_timeout(req),
    };
    let ssl_method = client_slot(this, ClientSlot::SslMethod).as_int();
    let protocol = if use_ssl && !*use_proxy {
        match ssl_method {
            Some(SSL_METHOD_TLS) => "tls",
            Some(SSL_METHOD_SSLV2) => "sslv2",
            Some(SSL_METHOD_SSLV3) => "sslv3",
            _ => "ssl",
        }
    } else {
        "tcp"
    };
    let mut name = format!("{protocol}://").into_bytes();
    name.extend_from_slice(&host);
    name.extend_from_slice(format!(":{port}").as_bytes());
    let xport = Xport::Client {
        timeout,
        connect: true,
        persistent: None,
        socket: SocketOptions::default(),
        context,
    };
    let stream = match req.xport_create(&name, xport)? {
        Ok(stream) => stream,
        // `php_stream_xport_create` asked for no `error_string`.
        Err(failed) => {
            docref(req, Level::Warning, &failed.warning());
            return Ok(None);
        }
    };
    if !(*use_proxy && use_ssl) {
        return Ok(Some(stream));
    }
    // Set `peer_name` or name verification will try to use the proxy
    // server's name.
    let given = match context {
        Some(context) => context.option(req, b"ssl", b"peer_name").is_some(),
        None => false,
    };
    if !given && let Some(own) = stream.ctx(req) {
        let peer_name = PhpString::new(req, host_of(uri))?.into_value();
        let set = own.set_option(req, b"ssl", b"peer_name", &peer_name);
        peer_name.give_up(req);
        set?;
    }
    let target_port = port_of(uri);
    let mut head = b"CONNECT ".to_vec();
    head.extend_from_slice(c_str(host_of(uri)));
    head.extend_from_slice(format!(":{} HTTP/1.1\r\nHost: ", unsigned_of(target_port)).as_bytes());
    head.extend_from_slice(c_str(host_of(uri)));
    if target_port != 80 {
        head.extend_from_slice(format!(":{}", unsigned_of(target_port)).as_bytes());
    }
    head.extend_from_slice(b"\r\n");
    proxy_authentication(this, &mut head);
    head.extend_from_slice(b"\r\n");
    let connected = stream.write(req, &head) == Some(head.len())
        && match get_http_headers(req, *stream) {
            // `zend_string_free(http_headers)`.
            Ok(Some(headers)) => {
                headers.give_up(req);
                true
            }
            Ok(None) => false,
            Err(thrown) => {
                stream.free(req);
                return Err(thrown);
            }
        };
    if !connected {
        stream.free(req);
        return Ok(None);
    }
    // Enable the TLS layer: the method the `ssl_method` names, TLS for
    // one php does not know, `SSLv23` for none.
    let crypto_method = match ssl_method {
        None | Some(SSL_METHOD_SSLV23) => SSLV23_CLIENT,
        Some(SSL_METHOD_SSLV2) => SSLV2_CLIENT,
        Some(SSL_METHOD_SSLV3) => SSLV3_CLIENT,
        Some(_) => TLS_CLIENT,
    };
    if !stream.crypto_setup(req, crypto_method, None)
        || matches!(
            stream.crypto_enable(req, true),
            Handshake::Failed | Handshake::Unsupported
        )
    {
        stream.free(req);
        return Ok(None);
    }
    Ok(Some(stream))
}

/// `in_domain`: whether a cookie's `domain` covers `host`, a leading
/// `.` matching the host's tail and any other only the host itself.
fn in_domain(host: &[u8], domain: &[u8]) -> bool {
    if domain.first() == Some(&b'.') {
        host.len() > domain.len() && c_str(&host[host.len() - domain.len()..]) == c_str(domain)
    } else {
        host == domain
    }
}

// ---- the client's slots

/// `add_soap_fault(this_ptr, "HTTP", text, NULL, NULL, soap_lang_en)`.
fn fault(req: &mut Request, this: PhpObject, text: &[u8]) -> R<()> {
    add_soap_fault(req, this, b"HTTP", text, None, None, b"en")
}

/// `ZVAL_NULL(Z_CLIENT_HTTPSOCKET_P(this_ptr)); php_stream_close(stream);`
/// and `ZVAL_NULL(Z_CLIENT_USE_PROXY_P(this_ptr))`: the kept connection
/// closed and forgotten, its URL kept or not as `forget_url` says
/// (`convert_to_null(Z_CLIENT_HTTPURL_P(this_ptr))`).
fn drop_connection(req: &mut Request, this: PhpObject, stream: Stream, forget_url: bool) -> R<()> {
    stream.close(req);
    let value = Owner::null();
    set_client_slot(req, this, ClientSlot::HttpSocket, value)?;
    if forget_url {
        set_client_slot(req, this, ClientSlot::HttpUrl, Owner::null())?;
    }
    let value = Owner::null();
    set_client_slot(req, this, ClientSlot::UseProxy, value)
}

/// An array slot of the client separated for a write, `SEPARATE_ARRAY`:
/// the array with one owner, the caller's, to write back with
/// [`set_client_slot`]; a fresh one where the slot holds none.
fn slot_array(req: &mut Request, this: PhpObject, which: ClientSlot) -> R<Owner<PhpArray>> {
    match client_slot(this, which).as_array() {
        Some(array) => {
            let mut array = Owner::take(req, array);
            array.separate(req)?;
            Ok(array)
        }
        None => PhpArray::new(req),
    }
}

/// A string slot of the client written, `ZVAL_STRINGL`.
fn set_string_slot(req: &mut Request, this: PhpObject, which: ClientSlot, bytes: &[u8]) -> R<()> {
    let value = PhpString::new(req, bytes)?;
    set_client_slot(req, this, which, value.into_value())
}

/// `bytes` as a string value with one owner.
fn string(req: &mut Request, bytes: &[u8]) -> R<Owner<Value>> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// The function named, as `call_user_function` finds it: a name no
/// function has is php's `Error` "Invalid callback".
fn callable_named(req: &mut Request, name: &[u8]) -> R<Callable> {
    let named = string(req, name)?;
    let callable = to_callable(req, &named);
    named.give_up(req);
    callable
}

/// `call_user_function` of the function named, over a string and
/// integers: the string it answers, none for anything else.
fn call_by_name(req: &mut Request, name: &[u8], data: &[u8], ints: &[i64]) -> R<Option<Vec<u8>>> {
    let callable = callable_named(req, name)?;
    let text = string(req, data)?;
    let mut args = vec![*text];
    args.extend(ints.iter().map(|n| Value::int(*n)));
    let answered = call(req, &callable, &args);
    text.give_up(req);
    let answered = answered?;
    let out = answered.as_str_bytes().map(<[u8]>::to_vec);
    answered.give_up(req);
    Ok(out)
}

/// Whether the connection kept in the client's `httpsocket` goes where
/// `uri` does: through the same proxy for plain HTTP, or to the same
/// host and `port` with the same security, as its `httpurl` says.
fn same_target(
    req: &mut Request,
    this: PhpObject,
    uri: &UriStruct,
    use_ssl: bool,
    use_proxy: bool,
) -> bool {
    let url = client_slot(this, ClientSlot::HttpUrl);
    let Some(object) = url.as_object() else {
        return false;
    };
    if !instance_of(req, object, "Soap\\Url") {
        return false;
    }
    let Some(orig) = object
        .native::<UrlPayload>(req)
        .and_then(|payload| payload.uri.as_ref())
        .map(|held| &held.uri)
    else {
        return false;
    };
    // php's `(use_ssl && https) || (!use_ssl && !scheme) || (!use_ssl &&
    // !https)`, which is security alike.
    let https = orig.scheme.as_deref() == Some(b"https");
    (use_proxy && !use_ssl)
        || (use_ssl == https && orig.host == uri.host && port_of(orig) == port_of(uri))
}

/// What one exchange ended in: php's `FALSE`, its `TRUE` with no
/// response asked for, or the response's body.
enum Sent {
    Failed,
    Done,
    Body(Owner<PhpString>),
}

/// What an exchange holds on the request's heap where php holds it: the
/// response's head, `http_headers`; the words after its status,
/// `http_msg`; and its body, `http_body`. Each is let go where php lets
/// it go, and whatever is held as the exchange ends with it.
#[derive(Default)]
struct Held {
    headers: Option<Owner<PhpString>>,
    msg: Option<Emalloc>,
    body: Option<Owner<PhpString>>,
}

impl Held {
    /// `zend_string_release_ex(http_headers, 0)`.
    fn release_headers(&mut self, req: &mut Request) {
        if let Some(headers) = self.headers.take() {
            headers.give_up(req);
        }
    }

    /// `zend_string_release_ex(http_body, 0)`.
    fn release_body(&mut self, req: &mut Request) {
        if let Some(body) = self.body.take() {
            body.give_up(req);
        }
    }

    /// `efree(http_msg); http_msg = estrdup(text)`.
    fn set_msg(&mut self, req: &mut Request, text: &[u8]) -> R<()> {
        if let Some(msg) = self.msg.take() {
            msg.free(req);
        }
        let mut msg = Emalloc::new(req, text.len() + 1)?;
        msg.bytes_mut()[..text.len()].copy_from_slice(text);
        self.msg = Some(msg);
        Ok(())
    }

    /// `http_msg`'s text, without its NUL.
    fn msg(&self) -> &[u8] {
        self.msg.as_ref().map_or(&[][..], |msg| c_str(msg.bytes()))
    }

    /// All of it let go.
    fn release_all(&mut self, req: &mut Request) {
        self.release_headers(req);
        self.release_body(req);
        if let Some(msg) = self.msg.take() {
            msg.free(req);
        }
    }
}

/// `make_http_soap_request`: the response body, or none where php
/// answers false (a fault already added to the client through
/// `shared::add_soap_fault`).
pub(crate) fn make_http_soap_request(
    req: &mut Request,
    this: PhpObject,
    buf: &[u8],
    location: &[u8],
    soapaction: Option<&[u8]>,
    soap_version: i64,
    uri_parser_class: Option<&[u8]>,
) -> Res<Option<Owner<PhpString>>> {
    let call = Call {
        buf,
        location,
        soapaction,
        soap_version,
        uri_parser_class,
        want_body: true,
    };
    Ok(match send(req, this, &call)? {
        Sent::Body(body) => Some(body),
        Sent::Failed | Sent::Done => None,
    })
}

/// `make_http_soap_request` with no `return_value`, `__doRequest`'s
/// `oneWay`: the request sent and the response's head read only where
/// `trace` keeps it, the connection then closed; `false` where php
/// answers false (a fault already added to the client).
pub(crate) fn make_http_soap_request_one_way(
    req: &mut Request,
    this: PhpObject,
    buf: &[u8],
    location: &[u8],
    soapaction: Option<&[u8]>,
    soap_version: i64,
    uri_parser_class: Option<&[u8]>,
) -> Res<bool> {
    let call = Call {
        buf,
        location,
        soapaction,
        soap_version,
        uri_parser_class,
        want_body: false,
    };
    Ok(!matches!(send(req, this, &call)?, Sent::Failed))
}

/// What `make_http_soap_request` is called with.
struct Call<'a> {
    buf: &'a [u8],
    location: &'a [u8],
    soapaction: Option<&'a [u8]>,
    soap_version: i64,
    uri_parser_class: Option<&'a [u8]>,
    /// Whether php's `return_value` is given.
    want_body: bool,
}

/// The request compressed as the client's `compression` asks, with the
/// headers that say so: `Accept-Encoding` where it accepts, and the
/// body through `gzcompress` or `gzencode`, `Content-Encoding` naming
/// which. None where the function answered no string.
fn compress(
    req: &mut Request,
    this: PhpObject,
    buf: &[u8],
    soap_headers_z: &mut Vec<u8>,
) -> R<Option<Vec<u8>>> {
    let Some(compression) = client_slot(this, ClientSlot::Compression).as_int() else {
        return Ok(Some(buf.to_vec()));
    };
    let level = (compression & 0x0f).min(9);
    let kind = compression & SOAP_COMPRESSION_DEFLATE;
    if compression & SOAP_COMPRESSION_ACCEPT != 0 {
        soap_headers_z.extend_from_slice(b"Accept-Encoding: gzip, deflate\r\n");
    }
    if level == 0 {
        return Ok(Some(buf.to_vec()));
    }
    if kind == SOAP_COMPRESSION_DEFLATE {
        soap_headers_z.extend_from_slice(b"Content-Encoding: deflate\r\n");
        call_by_name(req, b"gzcompress", buf, &[level])
    } else {
        soap_headers_z.extend_from_slice(b"Content-Encoding: gzip\r\n");
        call_by_name(req, b"gzencode", buf, &[level, 0x1f])
    }
}

/// `make_http_soap_request` over what its exchange holds, all of it let
/// go however the exchange ends.
fn send(req: &mut Request, this: PhpObject, call: &Call<'_>) -> Res<Sent> {
    let mut held = Held::default();
    let sent = exchange(req, this, call, &mut held);
    held.release_all(req);
    sent
}

/// The body of `make_http_soap_request`, its `try_again` the loop: one
/// exchange per turn, a redirect or a digest challenge the next.
#[allow(clippy::too_many_lines)] // php's one function, its stretches kept in order
fn exchange(req: &mut Request, this: PhpObject, call: &Call<'_>, held: &mut Held) -> Res<Sent> {
    let mut soap_headers_z = Vec::new();
    let Some(request) = compress(req, this, call.buf, &mut soap_headers_z)? else {
        return Ok(Sent::Failed);
    };
    let mut stream = None;
    let mut use_proxy = false;
    let socket = client_slot(this, ClientSlot::HttpSocket);
    if matches!(socket.as_ref(), ValueRef::Resource(_)) {
        stream = Stream::from_value(req, &socket);
        if let Some(n) = client_slot(this, ClientSlot::UseProxy).as_int() {
            use_proxy = n != 0;
        }
    }
    let mut uri = None;
    if call.location.first().is_some_and(|c| *c != 0) {
        let Some(parser) = uri_parser(call.uri_parser_class) else {
            return Err(arg_value_error(req, 6, "be a valid URI parser name").into());
        };
        uri = parser.parse(req, call.location)?;
    }
    // `php_stream_context_from_zval(Z_CLIENT_STREAM_CONTEXT_P(this_ptr), 0)`
    // of a resource the slot holds.
    let context = client_slot(this, ClientSlot::StreamCtx)
        .as_resource()
        .map(|res| StreamCtx::fetch(req, res))
        .transpose()?;
    let mut redirect_max: i64 = 20;
    if let Some(context) = context
        && let Some(max) = context.option(req, b"http", b"max_redirects")
    {
        match max.as_ref() {
            ValueRef::Str(s) => {
                if let Some(n) = numeric_long(s.as_bytes()) {
                    redirect_max = n;
                }
            }
            ValueRef::Int(n) => redirect_max = n,
            _ => {}
        }
    }
    loop {
        let Some(mut uri_now) = uri.take().filter(|u: &UriStruct| u.host.is_some()) else {
            fault(req, this, b"Unable to parse URL")?;
            return Ok(Sent::Failed);
        };
        let use_ssl = match uri_now.scheme.as_deref() {
            Some(b"https") => true,
            Some(b"http") => false,
            _ => {
                fault(
                    req,
                    this,
                    b"Unknown protocol. Only http and https are allowed.",
                )?;
                return Ok(Sent::Failed);
            }
        };
        if use_ssl && !req.stream_wrappers().iter().any(|w| w == "https") {
            fault(req, this, b"SSL support is not available in this build")?;
            return Ok(Sent::Failed);
        }
        if port_of(&uri_now) == 0 {
            uri_now.port = Some(if use_ssl { 443 } else { 80 });
        }
        // Check if request to the same host.
        if let Some(s) = stream
            && !same_target(req, this, &uri_now, use_ssl, use_proxy)
        {
            drop_connection(req, this, s, true)?;
            stream = None;
            use_proxy = false;
        }
        // Check if keep-alive connection is still opened.
        if let Some(s) = stream
            && s.eof(req)
        {
            drop_connection(req, this, s, true)?;
            stream = None;
            use_proxy = false;
        }
        if stream.is_none() {
            let Some(s) = http_connect(req, this, &uri_now, use_ssl, context, &mut use_proxy)?
            else {
                fault(req, this, b"Could not connect to host")?;
                return Ok(Sent::Failed);
            };
            let view = *s;
            // The client's slot keeps the connection.
            let kept = s.into_resource().into_value();
            set_client_slot(req, this, ClientSlot::HttpSocket, kept)?;
            set_client_slot(
                req,
                this,
                ClientSlot::UseProxy,
                Owner::int(i64::from(use_proxy)),
            )?;
            stream = Some(view);
        }
        let client_trace = matches!(
            client_slot(this, ClientSlot::Trace).as_ref(),
            ValueRef::Bool(true)
        );
        let Some(s) = stream else {
            fault(req, this, b"Failed to create stream??")?;
            return Ok(Sent::Failed);
        };
        let request_head = Head {
            uri: &uri_now,
            use_ssl,
            use_proxy,
            context,
            soap_headers_z: &soap_headers_z,
            request_len: request.len(),
        };
        let mut soap_headers = request_headers(req, this, call, &request_head)?;
        if client_trace {
            set_string_slot(req, this, ClientSlot::LastRequestHeaders, &soap_headers)?;
        }
        soap_headers.extend_from_slice(&request);
        if s.write(req, &soap_headers) != Some(soap_headers.len()) {
            drop_connection(req, this, s, true)?;
            fault(req, this, b"Failed Sending HTTP SOAP request")?;
            return Ok(Sent::Failed);
        }
        // `http_headers`, held in `held` until php releases it.
        let mut head: Option<PhpString> = None;
        let mut http_1_1 = false;
        let mut http_status = 0;
        if call.want_body || client_trace {
            loop {
                let Some(headers) = get_http_headers(req, s)? else {
                    drop_connection(req, this, s, false)?;
                    fault(req, this, b"Error Fetching http headers")?;
                    return Ok(Sent::Failed);
                };
                let view = **held.headers.insert(headers);
                if client_trace {
                    // `ZVAL_STR_COPY`: the trace shares the head.
                    let kept = Owner::take(req, Value::string(view));
                    set_client_slot(req, this, ClientSlot::LastResponseHeaders, kept)?;
                }
                // Check to see what HTTP status was sent.
                http_1_1 = false;
                http_status = 0;
                if let Some(version) = get_http_header_value(view.as_bytes(), b"HTTP/") {
                    http_1_1 = strncmp_eq(&version, b"1.1", 3);
                    if let Some(sp) = find(&version, b' ') {
                        let rest = &version[sp + 1..];
                        http_status = atoi(rest);
                        if let Some(sp) = find(rest, b' ') {
                            held.set_msg(req, c_str(&rest[sp + 1..]))?;
                        }
                    }
                }
                if http_status != 100 {
                    head = Some(view);
                    break;
                }
                // Try and get headers again.
                held.release_headers(req);
            }
        }
        if !call.want_body {
            // The headers were only fetched because the trace keeps them.
            drop_connection(req, this, s, false)?;
            return Ok(Sent::Done);
        }
        // A call that wants the body has read a head above.
        let Some(head) = head else {
            return Ok(Sent::Failed);
        };
        let http_headers = head.as_bytes();
        // Grab and send back every cookie.
        let mut from = 0;
        while let Some((value, len)) = header_value_at(&http_headers[from..], b"Set-Cookie:") {
            let value = from + value;
            set_cookie(req, this, &http_headers[value..value + len], &uri_now)?;
            from = value + len;
        }
        // See if the server requested a close.
        let proxied = use_proxy && !use_ssl;
        let says = |name: &[u8], word: &[u8]| {
            get_http_header_value(http_headers, name)
                .is_some_and(|value| strncasecmp_eq(&value, word, word.len()))
        };
        let http_close = if http_1_1 {
            (proxied && says(b"Proxy-Connection:", b"close")) || says(b"Connection:", b"close")
        } else {
            !((proxied && says(b"Proxy-Connection:", b"Keep-Alive"))
                || says(b"Connection:", b"Keep-Alive"))
        };
        let Some(http_body) = get_http_body(req, s, http_close, http_headers)? else {
            drop_connection(req, this, s, false)?;
            held.release_headers(req);
            fault(
                req,
                this,
                b"Error Fetching http body, No Content-Length, connection closed or chunked data",
            )?;
            return Ok(Sent::Failed);
        };
        held.body = Some(http_body);
        if http_close {
            drop_connection(req, this, s, false)?;
            stream = None;
        }
        // Process HTTP status codes.
        if (300..400).contains(&http_status) {
            if let Some(location) = get_http_header_value(http_headers, b"Location:") {
                let Some(parser) = uri_parser(call.uri_parser_class) else {
                    return Err(arg_value_error(req, 6, "be a valid URI parser name").into());
                };
                if let Some(new_uri) = parser.parse(req, c_str(&location))? {
                    held.release_headers(req);
                    held.release_body(req);
                    uri = Some(redirected(new_uri, &uri_now));
                    redirect_max -= 1;
                    if redirect_max < 1 {
                        fault(req, this, b"Redirection limit reached, aborting")?;
                        return Ok(Sent::Failed);
                    }
                    continue;
                }
            }
        } else if http_status == 401 && digest_challenge(req, this, http_headers)? {
            held.release_headers(req);
            held.release_body(req);
            uri = Some(uri_now);
            continue;
        }
        return answer(req, this, held, head, http_status);
    }
}

/// What one request's head is written from: the URL, how the
/// connection goes, the client's `stream_context`, the compression's
/// headers and the length of the body.
struct Head<'a> {
    uri: &'a UriStruct,
    use_ssl: bool,
    use_proxy: bool,
    context: Option<StreamCtx>,
    soap_headers_z: &'a [u8],
    request_len: usize,
}

/// The head of one request, as `make_http_soap_request` writes it into
/// `soap_headers`: the request line, `Host`, `Connection`, `User-Agent`,
/// the compression's headers, `Content-Type` with the action, or
/// `SOAPAction`, `Content-Length`, the credentials, the cookies, the
/// stream's own `http.header` lines and the blank line: what `trace`
/// keeps as `__last_request_headers`. Err where the digest's nonce could
/// not be drawn, php's `RandomException` thrown.
#[allow(clippy::too_many_lines)] // php's one stretch of its function
fn request_headers(
    req: &mut Request,
    this: PhpObject,
    call: &Call<'_>,
    head: &Head<'_>,
) -> R<Vec<u8>> {
    let uri = head.uri;
    let context = head.context;
    let set_url = client_slot(this, ClientSlot::HttpUrl);
    if set_url.as_object().is_some() {
        set_client_slot(req, this, ClientSlot::HttpUrl, Owner::null())?;
    }
    let class = class_id(req, "Soap\\Url")?;
    let url = new_object(req, class)?;
    // `url_obj->uri = uri`: the `php_uri` is the object's from here.
    let held = match HeldUri::new(req, uri.clone()) {
        Ok(held) => held,
        Err(thrown) => {
            url.give_up(req);
            return Err(thrown);
        }
    };
    match url.native_mut::<UrlPayload>(req) {
        Some(payload) => payload.uri = Some(held),
        None => held.free(req),
    }
    set_client_slot(req, this, ClientSlot::HttpUrl, url.into_value())?;
    let http_1_1 = match context {
        Some(context) => !matches!(
            context.option(req, b"http", b"protocol_version")
                .as_ref()
                .map(Value::as_ref),
            Some(ValueRef::Float(v)) if is_one(v)
        ),
        None => true,
    };
    let port = port_of(uri);
    let mut h = b"POST ".to_vec();
    if head.use_proxy && !head.use_ssl {
        h.extend_from_slice(c_str(uri.scheme.as_deref().unwrap_or_default()));
        h.extend_from_slice(b"://");
        h.extend_from_slice(c_str(host_of(uri)));
        h.extend_from_slice(format!(":{}", unsigned_of(port)).as_bytes());
    }
    target(uri, &mut h);
    h.extend_from_slice(if http_1_1 {
        b" HTTP/1.1\r\n"
    } else {
        b" HTTP/1.0\r\n"
    });
    h.extend_from_slice(b"Host: ");
    h.extend_from_slice(c_str(host_of(uri)));
    if port != if head.use_ssl { 443 } else { 80 } {
        h.extend_from_slice(format!(":{}", unsigned_of(port)).as_bytes());
    }
    let keep_alive = client_slot(this, ClientSlot::KeepAlive);
    if !http_1_1 || matches!(keep_alive.as_ref(), ValueRef::Bool(false)) {
        h.extend_from_slice(b"\r\nConnection: close\r\n");
    } else {
        h.extend_from_slice(b"\r\nConnection: Keep-Alive\r\n");
    }
    user_agent(req, this, context, &mut h);
    h.extend_from_slice(head.soap_headers_z);
    let content_type = match context {
        Some(context) => context
            .option(req, b"http", b"content_type")
            .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
            .filter(|v| !v.is_empty()),
        None => None,
    };
    if call.soap_version == SOAP_1_2 {
        match &content_type {
            Some(content_type) => {
                h.extend_from_slice(b"Content-Type: ");
                h.extend_from_slice(content_type);
            }
            None => h.extend_from_slice(b"Content-Type: application/soap+xml; charset=utf-8"),
        }
        if let Some(action) = call.soapaction {
            h.extend_from_slice(b"; action=\"");
            h.extend_from_slice(c_str(action));
            h.push(b'"');
        }
        h.extend_from_slice(b"\r\n");
    } else {
        match &content_type {
            Some(content_type) => {
                h.extend_from_slice(b"Content-Type: ");
                h.extend_from_slice(content_type);
                h.extend_from_slice(b"\r\n");
            }
            None => h.extend_from_slice(b"Content-Type: text/xml; charset=utf-8\r\n"),
        }
        if let Some(action) = call.soapaction {
            h.extend_from_slice(b"SOAPAction: \"");
            h.extend_from_slice(c_str(action));
            h.extend_from_slice(b"\"\r\n");
        }
    }
    h.extend_from_slice(format!("Content-Length: {}\r\n", head.request_len).as_bytes());
    // HTTP Authentication.
    let mut has_authorization = false;
    let login = client_slot(this, ClientSlot::Login);
    if let Some(login) = login.as_str_bytes() {
        has_authorization = true;
        if client_slot(this, ClientSlot::Digest).as_array().is_none() {
            let password = client_slot(this, ClientSlot::Password);
            basic(&mut h, b"Authorization", login, password.as_str_bytes());
        } else {
            digest_authorization(req, this, uri, login, &mut h)?;
        }
    }
    // Proxy HTTP Authentication.
    let mut has_proxy_authorization = false;
    if head.use_proxy && !head.use_ssl {
        has_proxy_authorization = proxy_authentication(this, &mut h);
    }
    // Send cookies along with request.
    let has_cookies = cookie_header(req, this, uri, head.use_ssl, &mut h);
    http_context_headers(
        req,
        context,
        has_authorization,
        has_proxy_authorization,
        has_cookies,
        &mut h,
    );
    h.extend_from_slice(b"\r\n");
    Ok(h)
}

/// `User-Agent`: the client's `user_agent`, none where it is empty; else
/// the stream's `http.user_agent`, the same; else the `user_agent` ini
/// entry where it has a value; else `PHP-SOAP/` and php's version.
fn user_agent(req: &Request, this: PhpObject, context: Option<StreamCtx>, h: &mut Vec<u8>) {
    let line = |h: &mut Vec<u8>, agent: &[u8]| {
        h.extend_from_slice(b"User-Agent: ");
        h.extend_from_slice(agent);
        h.extend_from_slice(b"\r\n");
    };
    if let Some(agent) = client_slot(this, ClientSlot::UserAgent).as_str_bytes() {
        if !agent.is_empty() {
            line(h, agent);
        }
        return;
    }
    if let Some(context) = context
        && let Some(agent) = context.option(req, b"http", b"user_agent")
        && let Some(agent) = agent.as_str_bytes()
    {
        if !agent.is_empty() {
            line(h, agent);
        }
        return;
    }
    match req.ini().value(b"user_agent") {
        Some(agent) => line(h, c_str(agent)),
        None => line(h, format!("PHP-SOAP/{PHP_VERSION}").as_bytes()),
    }
}

/// `Authorization: Digest` over the challenge kept in the client's
/// `_digest`: a fresh `cnonce`, the `nc` counted up in the array, and
/// the response hashed as RFC 2617 says, `md5-sess` and `qop=auth`
/// heeded. Where the nonce could not be drawn, the kept connection is
/// dropped and php's `RandomException` thrown.
fn digest_authorization(
    req: &mut Request,
    this: PhpObject,
    uri: &UriStruct,
    login: &[u8],
    h: &mut Vec<u8>,
) -> R<()> {
    let mut nonce = [0u8; 16];
    if let Err(why) = getrandom::fill(&mut nonce) {
        let ce = Throwables::of(req)?;
        let thrown = throw(
            req,
            ce.random_exception,
            &format!("Failed to retrieve randomness from the operating system ({why})"),
        );
        let socket = client_slot(this, ClientSlot::HttpSocket);
        if let Some(stream) = Stream::from_value(req, &socket) {
            drop_connection(req, this, stream, true)?;
        }
        return Err(thrown);
    }
    let cnonce = hex(&nonce);
    let mut digest = slot_array(req, this, ClientSlot::Digest)?;
    let count = digest
        .find(b"nc")
        .and_then(|v| v.as_int())
        .map_or(1, |n| n.wrapping_add(1));
    digest.set_str(req, b"nc", Owner::int(count))?;
    // `snprintf(nc, sizeof(nc), "%08" ZEND_LONG_FMT_SPEC, ...)`, or
    // `"00000001"` for a count it adds.
    let mut nc = format!("{count:08}").into_bytes();
    nc.truncate(8);
    let field = |name: &[u8]| -> Option<Vec<u8>> {
        digest
            .find(name)
            .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    };
    let realm = field(b"realm");
    let nonce = field(b"nonce");
    let qop = field(b"qop");
    let opaque = field(b"opaque");
    let algorithm = field(b"algorithm");
    let kept = digest.into_value();
    set_client_slot(req, this, ClientSlot::Digest, kept)?;
    let password = client_slot(this, ClientSlot::Password);
    let mut ha1 = md5_hex(&[
        login,
        b":",
        realm.as_deref().unwrap_or_default(),
        b":",
        password.as_str_bytes().unwrap_or_default(),
    ]);
    if algorithm
        .as_deref()
        .is_some_and(|a| a.eq_ignore_ascii_case(b"md5-sess"))
    {
        ha1 = md5_hex(&[
            &ha1,
            b":",
            nonce.as_deref().unwrap_or_default(),
            b":",
            &cnonce[..8],
        ]);
    }
    let mut ha2_parts: Vec<&[u8]> = vec![b"POST:", uri.path.as_deref().unwrap_or(b"/")];
    if let Some(query) = &uri.query {
        ha2_parts.push(b"?");
        ha2_parts.push(query);
    }
    let ha2 = md5_hex(&ha2_parts);
    let mut parts: Vec<&[u8]> = vec![&ha1, b":", nonce.as_deref().unwrap_or_default(), b":"];
    if qop.is_some() {
        // TODO in php: support for qop=auth-int.
        parts.extend_from_slice(&[&nc, b":", &cnonce[..8], b":", b"auth", b":"]);
    }
    parts.push(&ha2);
    let response = md5_hex(&parts);
    h.extend_from_slice(b"Authorization: Digest username=\"");
    h.extend_from_slice(login);
    if let Some(realm) = &realm {
        h.extend_from_slice(b"\", realm=\"");
        h.extend_from_slice(realm);
    }
    if let Some(nonce) = &nonce {
        h.extend_from_slice(b"\", nonce=\"");
        h.extend_from_slice(nonce);
    }
    h.extend_from_slice(b"\", uri=\"");
    target(uri, h);
    if qop.is_some() {
        h.extend_from_slice(b"\", qop=auth, nc=");
        h.extend_from_slice(&nc);
        h.extend_from_slice(b", cnonce=\"");
        h.extend_from_slice(&cnonce[..8]);
    }
    h.extend_from_slice(b"\", response=\"");
    h.extend_from_slice(&response);
    if let Some(opaque) = &opaque {
        h.extend_from_slice(b"\", opaque=\"");
        h.extend_from_slice(opaque);
    }
    if let Some(algorithm) = &algorithm {
        h.extend_from_slice(b"\", algorithm=\"");
        h.extend_from_slice(algorithm);
    }
    h.extend_from_slice(b"\"\r\n");
    Ok(())
}

/// `Cookie:`, each of the client's `_cookies` whose `path` the request's
/// path starts with, whose `domain` covers the host and which is not
/// `secure` or goes over TLS, as `name=value` joined by `; `; whether
/// the header was written, which a cookie array that is a list or empty
/// writes none of.
fn cookie_header(
    req: &Request,
    this: PhpObject,
    uri: &UriStruct,
    use_ssl: bool,
    h: &mut Vec<u8>,
) -> bool {
    let cookies = client_slot(this, ClientSlot::Cookies);
    let Some(cookies) = cookies.as_array() else {
        return false;
    };
    if cookies.is_empty() || cookies.is_packed() {
        return false;
    }
    let path = uri.path.as_deref().unwrap_or(b"/");
    let host = host_of(uri);
    let mut first = true;
    h.extend_from_slice(b"Cookie: ");
    for (key, data) in walk::rows(req, cookies) {
        let data = data.deref();
        let (Key::Str(name), Some(data)) = (key, data.as_array()) else {
            continue;
        };
        let Some(value) = data.get(&Key::Int(0)) else {
            continue;
        };
        let Some(value) = value.as_str_bytes() else {
            continue;
        };
        let path_ok = match data
            .get(&Key::Int(1))
            .as_ref()
            .and_then(Value::as_str_bytes)
        {
            Some(cookie_path) => strncmp_eq(path, cookie_path, cookie_path.len()),
            None => true,
        };
        let domain_ok = match data
            .get(&Key::Int(2))
            .as_ref()
            .and_then(Value::as_str_bytes)
        {
            Some(domain) => in_domain(host, domain),
            None => true,
        };
        if path_ok && domain_ok && (use_ssl || data.get(&Key::Int(3)).is_none()) {
            if !first {
                h.extend_from_slice(b"; ");
            }
            first = false;
            h.extend_from_slice(name.as_bytes());
            h.push(b'=');
            h.extend_from_slice(value);
        }
    }
    h.extend_from_slice(b"\r\n");
    true
}

/// One `Set-Cookie:` value kept in the client's `_cookies` under its
/// name: `[value, path, domain, secure]` by index as its options came,
/// the path defaulting to the request's directory and the domain to its
/// host. A value with no `=` before any `;` is dropped.
fn set_cookie(req: &mut Request, this: PhpObject, cookie: &[u8], uri: &UriStruct) -> R<()> {
    let mut cookies = slot_array(req, this, ClientSlot::Cookies)?;
    let cookie = c_str(cookie);
    let eqpos = find(cookie, b'=');
    let sempos = find(cookie, b';');
    let stored = match eqpos {
        Some(eqpos) if sempos.is_none_or(|sempos| sempos > eqpos) => {
            store_cookie(req, &mut cookies, cookie, eqpos, sempos, uri)
        }
        _ => Ok(()),
    };
    let kept = cookies.into_value();
    set_client_slot(req, this, ClientSlot::Cookies, kept)?;
    stored
}

/// The body of [`set_cookie`] once the name is known.
fn store_cookie(
    req: &mut Request,
    cookies: &mut Owner<PhpArray>,
    cookie: &[u8],
    eqpos: usize,
    sempos: Option<usize>,
    uri: &UriStruct,
) -> R<()> {
    let mut zcookie = PhpArray::new(req)?;
    let value = string(req, &cookie[eqpos + 1..sempos.unwrap_or(cookie.len())])?;
    zcookie.set(req, Owner::index(0), value)?;
    if let Some(sempos) = sempos {
        let mut options = sempos + 1;
        while options < cookie.len() {
            while cookie.get(options) == Some(&b' ') {
                options += 1;
            }
            let rest = &cookie[options..];
            let next = find(rest, b';');
            let end = next.unwrap_or(rest.len());
            if let Some(path) = rest.strip_prefix(b"path=") {
                let value = string(req, &path[..end - 5])?;
                zcookie.set(req, Owner::index(1), value)?;
            } else if let Some(domain) = rest.strip_prefix(b"domain=") {
                let value = string(req, &domain[..end - 7])?;
                zcookie.set(req, Owner::index(2), value)?;
            } else if rest.starts_with(b"secure") {
                zcookie.set(req, Owner::index(3), Owner::bool(true))?;
            }
            match next {
                Some(next) => options += next + 1,
                None => break,
            }
        }
    }
    if !zcookie.contains(&Key::Int(1)) {
        let t = c_str(uri.path.as_deref().unwrap_or(b"/"));
        if let Some(c) = t.iter().rposition(|c| *c == b'/') {
            let value = string(req, &t[..c])?;
            zcookie.set(req, Owner::index(1), value)?;
        }
    }
    if !zcookie.contains(&Key::Int(2)) {
        let value = string(req, host_of(uri))?;
        zcookie.set(req, Owner::index(2), value)?;
    }
    let name = PhpString::new(req, &cookie[..eqpos])?;
    let name = Key::from_string(req, name);
    cookies.set(req, name, zcookie.into_value())
}

/// The URL a `Location` sends the next request to: a relative one takes
/// the scheme, and where it names none the host and `port`, of the one
/// answered, and a path not starting with `/` is put under the answered
/// path's directory. The path so joined carries the NUL php's
/// `strcat` leaves inside the length it allocated, which only the
/// digest's hash reads.
fn redirected(mut new_uri: UriStruct, uri: &UriStruct) -> UriStruct {
    if new_uri.scheme.is_some() {
        return new_uri;
    }
    let Some(new_path) = new_uri.path.clone() else {
        return new_uri;
    };
    new_uri.scheme.clone_from(&uri.scheme);
    if new_uri.host.is_none() {
        new_uri.host.clone_from(&uri.host);
        new_uri.port = uri.port;
    }
    if new_path.first() != Some(&b'/') {
        let base = uri.path.as_deref().map_or(Some(&b"/"[..]), |path| {
            let t = c_str(path);
            t.iter().rposition(|c| *c == b'/').map(|p| &t[..=p])
        });
        let joined = base.map(|base| {
            let mut s = base.to_vec();
            s.extend_from_slice(c_str(&new_path));
            s
        });
        if let Some(mut joined) = joined {
            joined.push(0);
            new_uri.path = Some(joined);
        }
    }
    new_uri
}

/// A `401` with a `WWW-Authenticate: Digest` challenge, the client
/// having a login and a password and no digest yet: the challenge's
/// `name=value` pairs kept in the client's `_digest`, which the next
/// try answers; `true` where it was kept.
fn digest_challenge(req: &mut Request, this: PhpObject, headers: &[u8]) -> R<bool> {
    let Some(auth) = get_http_header_value(headers, b"WWW-Authenticate:") else {
        return Ok(false);
    };
    let auth = c_str(&auth);
    if !auth.starts_with(b"Digest")
        || client_slot(this, ClientSlot::Digest).as_array().is_some()
        || client_slot(this, ClientSlot::Login)
            .as_str_bytes()
            .is_none()
        || client_slot(this, ClientSlot::Password)
            .as_str_bytes()
            .is_none()
    {
        return Ok(false);
    }
    let mut digest: Option<Owner<PhpArray>> = None;
    let mut s = b"Digest".len();
    while s < auth.len() {
        while at(auth, s) == b' ' {
            s += 1;
        }
        let name = s;
        while s < auth.len() && auth[s] != b'=' {
            s += 1;
        }
        if s >= auth.len() {
            continue;
        }
        let name = &auth[name..s];
        s += 1;
        let val = if at(auth, s) == b'"' {
            s += 1;
            let start = s;
            while s < auth.len() && auth[s] != b'"' {
                s += 1;
            }
            &auth[start..s]
        } else {
            let start = s;
            while s < auth.len() && !matches!(auth[s], b' ' | b',') {
                s += 1;
            }
            &auth[start..s]
        };
        if s < auth.len() {
            if auth[s] == b',' {
                s += 1;
            } else {
                s += 1;
                while s < auth.len() && auth[s] != b',' {
                    s += 1;
                }
                if s < auth.len() {
                    s += 1;
                }
            }
        }
        let array = match &mut digest {
            Some(array) => array,
            None => digest.insert(PhpArray::new(req)?),
        };
        let value = string(req, val)?;
        array.set_symtable_str(req, name, value)?;
    }
    let Some(digest) = digest else {
        return Ok(false);
    };
    let kept = digest.into_value();
    set_client_slot(req, this, ClientSlot::Digest, kept)?;
    Ok(true)
}

/// The tail of `make_http_soap_request`: the body decompressed as its
/// `Content-Encoding` says, through `gzdecode` or `gzuncompress`; and an
/// error status whose body is empty, or no XML where the `Content-Type`
/// does not say XML, a fault with the status line's message.
fn answer(
    req: &mut Request,
    this: PhpObject,
    held: &mut Held,
    head: PhpString,
    http_status: i32,
) -> Res<Sent> {
    let http_headers = head.as_bytes();
    // Check and see if the server even sent a xml document.
    let content_type_xml =
        get_http_header_value(http_headers, b"Content-Type:").is_some_and(|ct| {
            let ct = c_str(&ct);
            let cmplen = find(ct, b';').unwrap_or(ct.len());
            strncmp_eq(ct, b"text/xml", cmplen) || strncmp_eq(ct, b"application/soap+xml", cmplen)
        });
    let Some(http_body) = held.body.take() else {
        return Ok(Sent::Failed);
    };
    // Decompress response.
    let body = match get_http_header_value(http_headers, b"Content-Encoding:") {
        Some(encoding) => {
            let encoding = c_str(&encoding);
            let function: &[u8] = if matches!(encoding, b"gzip" | b"x-gzip")
                && lookup_function(req, b"gzdecode").is_some()
            {
                b"gzdecode"
            } else if encoding == b"deflate" && lookup_function(req, b"gzuncompress").is_some() {
                b"gzuncompress"
            } else {
                http_body.give_up(req);
                held.release_all(req);
                fault(req, this, b"Unknown Content-Encoding")?;
                return Ok(Sent::Failed);
            };
            // The body string itself is the argument, `ZVAL_STR`, and
            // the string answered is the body handed on.
            let answered = callable_named(req, function)
                .and_then(|callable| call(req, &callable, &[Value::string(*http_body)]));
            let decompressed = answered.map(|answered| {
                let kept = match answered.as_ref() {
                    ValueRef::Str(s) => Some(Owner::take(req, s)),
                    _ => None,
                };
                answered.give_up(req);
                kept
            });
            http_body.give_up(req);
            let Some(body) = decompressed? else {
                held.release_headers(req);
                fault(req, this, b"Can't uncompress compressed response")?;
                return Ok(Sent::Failed);
            };
            body
        }
        None => http_body,
    };
    // `zend_string_release_ex(http_headers, 0)`.
    held.release_headers(req);
    if http_status >= 400 {
        let text = body.as_bytes();
        let error = text.is_empty() || (!content_type_xml && !starts_xml(text));
        if error {
            body.give_up(req);
            fault(req, this, held.msg())?;
            return Ok(Sent::Failed);
        }
    }
    Ok(Sent::Body(body))
}

/// Whether a body starts `<?xml` past its control bytes, read as
/// `char`, which is signed where the pinned php runs, so bytes past
/// `0x7f` are passed over too.
fn starts_xml(body: &[u8]) -> bool {
    let s = c_str(body);
    #[expect(
        clippy::cast_possible_wrap,
        reason = "C's signed `char`, as `*s < ' '` reads a byte"
    )]
    let skip = s.iter().take_while(|c| (**c as i8) < 0x20).count();
    s[skip..].starts_with(b"<?xml")
}
