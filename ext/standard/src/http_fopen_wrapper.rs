//! `Z/ext/standard/http_fopen_wrapper.c`: the `http://` wrapper, php's
//! `php_stream_url_wrap_http_ex` over the engine's `tcp` socket
//! `transport`. The `context`'s `http` options are decoded once into
//! [`HttpOptions`]; each request of a chain of redirects is one call of
//! [`wrap_http_ex`], as php recurses, and each response head is read
//! into the one headers array `$http_response_header`,
//! `http_get_last_response_headers()` and the stream's `wrapper_data`
//! share. `https://` is the same wrapper over the `ssl` socket
//! `transport`, or a `CONNECT` through a `proxy` whose tunnel is then
//! secured, both `ext/openssl`'s, which registers the wrapper again under
//! `https`; without it, a redirect to one is refused as php without
//! `openssl` refuses it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::coerce::c_str;
use php_ext::coerce::{atoi, to_double, to_long, truthy};
use php_ext::decl::IniDecl;
use php_ext::raising::{Level, Thrown, docref, value_error};
use php_ext::request::{
    Handshake, Notification, NotifyCode, Opening, OptionAnswer, OwnedStream as _, Request,
    SocketOptions, Stream, StreamCtx, StreamOption, StreamWrapper, Streams as _, Xport,
    XportFailure,
};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef, walk};
use php_ext_uri::UriStruct;

use crate::State;
use crate::sockets::{default_timeout, timeval};
use crate::streamsfuncs::{notify, notify_error, notify_info};
use crate::url::{UriParser, php_url_decode};

/// `HTTP_HEADER_MAX_LOCATION_SIZE`: 8192 less the ten bytes of
/// `Location: `.
const HTTP_HEADER_MAX_LOCATION_SIZE: usize = 8182;

/// `PHP_URL_REDIRECT_MAX`, how many redirects a chain follows unless
/// `max_redirects` says otherwise.
const PHP_URL_REDIRECT_MAX: i64 = 20;

/// `PHP_TIMEOUT_ULL_MAX / 1000000.0`, the most seconds `timeout` holds,
/// and the whole seconds php prints of it.
const TIMEOUT_MAX: f64 = 18_446_744_073_709.55;
const TIMEOUT_MAX_SHOWN: u64 = 18_446_744_073_709;

/// `file.c`'s `user_agent` and `from`, `PHP_INI_ALL` and with no value
/// (`NULL`), which the http wrapper sends as `User-Agent:` and `From:`,
/// and the ftp wrapper as the anonymous password, in `PHP_INI_BEGIN`'s
/// order ahead of `default_socket_timeout`.
///
/// ```php
/// var_dump(ini_get('user_agent'), ini_get('from'), ini_set('from', 'me@example.com'), ini_get('from'));
/// ```
/// ```expect
/// string(0) ""
/// string(0) ""
/// string(0) ""
/// string(14) "me@example.com"
/// ```
pub(crate) fn ini() -> Vec<IniDecl> {
    [&b"user_agent"[..], b"from"]
        .into_iter()
        .map(|name| IniDecl {
            name,
            default: None,
            user: true,
            per_dir: true,
            system: true,
        })
        .collect()
}

/// `FG(from_address)` and `FG(user_agent)`: the entry's value, none where
/// it has none (`NULL`), where one set empty is the empty string.
pub(crate) fn file_ini(req: &Request, name: &[u8]) -> Option<Vec<u8>> {
    req.ini().value(name).map(<[u8]>::to_vec)
}

/// `php_stream_http_wrapper`: `http`, a URL wrapper that opens streams
/// and nothing else, whose streams no `fstat()` answers.
pub fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: "http".into(),
        label: "http",
        is_url: true,
        open: Some(open),
        open_dir: None,
        stat: None,
        unlink: None,
        rename: None,
        mkdir: None,
        rmdir: None,
        set_stat: None,
        stream_stat: Some(stream_stat),
    }
}

/// `php_stream_http_stream_stat`: `-1`, so the socket's own stat is not
/// taken for the resource's.
fn stream_stat(_req: &mut Request, _stream: Stream) -> Option<[i64; 13]> {
    None
}

/// `php_stream_url_wrap_http` as the `http` wrapper's opener.
fn open(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    url_wrap_http(req, "http", path, mode, opening)
}

/// `$result?` where the opener holds `$stream`: the stream closed and
/// its owner given up before the throwable is handed on.
macro_rules! held {
    ($req:expr, $stream:expr, $result:expr) => {
        match $result {
            Ok(answer) => answer,
            Err(thrown) => {
                $stream.free($req);
                return Err(thrown);
            }
        }
    };
}

/// `php_stream_url_wrap_http` (`Z/ext/standard/http_fopen_wrapper.c:1200`),
/// which the ftp wrapper calls too for a read through its `proxy`,
/// `label` the wrapper's: `BG(last_http_headers)` forgotten, the chain of
/// requests made, and the headers it read, where it sent one, kept as
/// the last response's and left in the caller's `$http_response_header`.
///
/// ```php
/// $srv = stream_socket_server('tcp://127.0.0.1:0');
/// $addr = stream_socket_get_name($srv, false);
/// if (pcntl_fork() === 0) {
///     $c = stream_socket_accept($srv);
///     fread($c, 1024);
///     fwrite($c, "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nhi");
///     fclose($c);
///     exit(0);
/// }
/// var_dump(in_array('http', stream_get_wrappers()), file_get_contents("http://$addr/x"));
/// var_dump(@$http_response_header, http_get_last_response_headers());
/// pcntl_wait($status);
/// ```
/// ```expect process
/// Deprecated: The predefined locally scoped $http_response_header variable is deprecated, call http_get_last_response_headers() instead in %s on line 12
/// bool(true)
/// string(2) "hi"
/// array(3) {
///   [0]=>
///   string(15) "HTTP/1.0 200 OK"
///   [1]=>
///   string(24) "Content-Type: text/plain"
///   [2]=>
///   string(17) "Content-Length: 2"
/// }
/// array(3) {
///   [0]=>
///   string(15) "HTTP/1.0 200 OK"
///   [1]=>
///   string(24) "Content-Type: text/plain"
///   [2]=>
///   string(17) "Content-Length: 2"
/// }
/// ```
pub(crate) fn url_wrap_http(
    req: &mut Request,
    label: &'static str,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    clear_last_headers(req);
    let ctx = req.stream_ctx();
    let options = HttpOptions::read(req, ctx, label)?;
    let mut chain = Chain {
        ctx,
        options,
        opening,
        mode: mode.to_vec(),
        headers: None,
        thrown: None,
    };
    let opened = wrap_http_ex(req, &mut chain, path, PHP_URL_REDIRECT_MAX, Hop::First);
    if let Some(headers) = chain.headers.take() {
        let value = Owner::take(req, Value::array(*headers));
        req.state::<State>().last_http_headers = Some(headers);
        req.frames_mut()
            .set_local_var(b"http_response_header", value);
    }
    let thrown = match opened {
        Ok(stream) => match chain.thrown {
            None => return Ok(stream),
            Some(thrown) => {
                if let Some(stream) = stream {
                    stream.free(req);
                }
                thrown
            }
        },
        Err(thrown) => thrown,
    };
    Err(thrown)
}

/// `zval_ptr_dtor(&BG(last_http_headers))`, what each request of the
/// wrapper and `http_clear_last_response_headers()` start with.
pub(crate) fn clear_last_headers(req: &mut Request) {
    if let Some(headers) = req.state::<State>().last_http_headers.take() {
        headers.give_up(req);
    }
}

/// `BG(last_http_headers)`: the headers the last request of the wrapper
/// read, a view of them.
pub(crate) fn last_headers(req: &mut Request) -> Option<PhpArray> {
    req.state::<State>().last_http_headers.as_deref().copied()
}

/// The `http` options of the `context` a chain of requests is made
/// under, each read once as its reader in `http_fopen_wrapper.c` reads
/// it; `None` is an option the `context` does not have, or one of a type
/// its reader passes over. `proxy` and `timeout` are read under the
/// wrapper's own label, which is `ftp` for a read the ftp wrapper hands
/// over.
struct HttpOptions {
    /// `method`, a string that is not empty.
    method: Option<Vec<u8>>,
    /// `header`: the strings of an array, each ended by `\r\n`, or a
    /// string that is not empty, trimmed as `php_trim(..., 3)` trims.
    header: Option<Vec<u8>>,
    /// `user_agent`, a string.
    user_agent: Option<Vec<u8>>,
    /// `content`, a string that is not empty.
    content: Option<Vec<u8>>,
    /// `proxy`, a string that is not empty: the `transport` connected to
    /// in place of the host.
    proxy: Option<Vec<u8>>,
    /// `request_fulluri`, `zend_is_true`.
    request_fulluri: Option<bool>,
    /// `follow_location`, `zval_is_true`.
    follow_location: Option<bool>,
    /// `max_redirects`, `zval_get_long`.
    max_redirects: Option<i64>,
    /// `protocol_version`, `zval_get_double`.
    protocol_version: Option<f64>,
    /// `timeout`, `zval_get_double`.
    timeout: Option<f64>,
    /// `ignore_errors`, `zend_is_true`.
    ignore_errors: bool,
    /// `auto_decode`, `zend_is_true`.
    auto_decode: Option<bool>,
    /// `uri_parser_class`, as `php_stream_context_get_uri_parser` reads
    /// it; `None` is one naming no parser.
    parser: Option<UriParser>,
}

impl HttpOptions {
    fn read(
        req: &mut Request,
        ctx: Option<StreamCtx>,
        label: &'static str,
    ) -> Result<HttpOptions, Thrown> {
        let parser = UriParser::of_ctx(req, ctx, b"http")?;
        let mut options = HttpOptions {
            method: None,
            header: None,
            user_agent: None,
            content: None,
            proxy: None,
            request_fulluri: None,
            follow_location: None,
            max_redirects: None,
            protocol_version: None,
            timeout: None,
            ignore_errors: false,
            auto_decode: None,
            parser,
        };
        let Some(ctx) = ctx else {
            return Ok(options);
        };
        let get = |req: &Request, wrapper: &[u8], name: &[u8]| ctx.option(req, wrapper, name);
        let string = |value: &Value| value.as_str_bytes().map(<[u8]>::to_vec);
        let filled = |value: &Value| string(value).filter(|s| !s.is_empty());
        options.method = get(req, b"http", b"method").as_ref().and_then(filled);
        options.header = get(req, b"http", b"header")
            .as_ref()
            .and_then(|value| header_of(req, value));
        options.user_agent = get(req, b"http", b"user_agent").as_ref().and_then(string);
        options.content = get(req, b"http", b"content").as_ref().and_then(filled);
        options.proxy = get(req, label.as_bytes(), b"proxy")
            .as_ref()
            .and_then(filled);
        if let Some(value) = get(req, b"http", b"request_fulluri") {
            options.request_fulluri = Some(truthy(req, &value)?);
        }
        if let Some(value) = get(req, b"http", b"follow_location") {
            options.follow_location = Some(truthy(req, &value)?);
        }
        if let Some(value) = get(req, b"http", b"max_redirects") {
            options.max_redirects = Some(to_long(req, &value)?);
        }
        if let Some(value) = get(req, b"http", b"protocol_version") {
            options.protocol_version = Some(to_double(req, &value)?);
        }
        if let Some(value) = get(req, label.as_bytes(), b"timeout") {
            options.timeout = Some(to_double(req, &value)?);
        }
        if let Some(value) = get(req, b"http", b"ignore_errors") {
            options.ignore_errors = truthy(req, &value)?;
        }
        if let Some(value) = get(req, b"http", b"auto_decode") {
            options.auto_decode = Some(truthy(req, &value)?);
        }
        Ok(options)
    }
}

/// `header` as `php_stream_url_wrap_http_ex` joins it: an array's string
/// members each followed by `\r\n`, or a string that is not empty; then
/// trimmed at both ends, and `None` where nothing is left.
fn header_of(req: &Request, value: &Value) -> Option<Vec<u8>> {
    let joined = match value.as_ref() {
        ValueRef::Array(lines) => {
            let mut joined = Vec::new();
            for (_, line) in walk::rows(req, lines) {
                if let Some(line) = line.deref().as_str_bytes() {
                    joined.extend_from_slice(line);
                    joined.extend_from_slice(b"\r\n");
                }
            }
            joined
        }
        ValueRef::Str(text) if !text.as_bytes().is_empty() => text.as_bytes().to_vec(),
        _ => return None,
    };
    let trimmed = php_trim(&joined);
    (!trimmed.is_empty()).then(|| trimmed.to_vec())
}

/// `php_trim(str, NULL, 0, 3)`: the default blanks, `" \t\n\r\0\x0B"`,
/// taken off both ends.
fn php_trim(s: &[u8]) -> &[u8] {
    let blank = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0 | 0x0b);
    let start = s.iter().position(|b| !blank(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !blank(b)).map_or(start, |at| at + 1);
    &s[start..end]
}

/// The flags one call of `php_stream_url_wrap_http_ex` is made with:
/// `HTTP_WRAPPER_HEADER_INIT` for the first request of a chain, or
/// `HTTP_WRAPPER_REDIRECTED` with `HTTP_WRAPPER_KEEP_METHOD` where a 307
/// or a 308 keeps the method, and `HTTP_WRAPPER_STRIP_AUTH` once the
/// chain has left its first origin (GHSA-fpwc-w8rq-cr92), which every
/// later hop keeps, back on the first origin as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hop {
    First,
    Redirected { keep_method: bool, strip_auth: bool },
}

impl Hop {
    const fn header_init(self) -> bool {
        matches!(self, Hop::First)
    }

    const fn redirected(self) -> bool {
        matches!(self, Hop::Redirected { .. })
    }

    const fn keep_method(self) -> bool {
        matches!(
            self,
            Hop::Redirected {
                keep_method: true,
                ..
            }
        )
    }

    /// `HTTP_WRAPPER_STRIP_AUTH`: the user's `Authorization`, `Cookie`
    /// and, off a proxy, `Proxy-Authorization` headers are not sent.
    const fn strip_auth(self) -> bool {
        matches!(
            self,
            Hop::Redirected {
                strip_auth: true,
                ..
            }
        )
    }

    /// Whether the request carries `content`: the first, and one a 307
    /// or a 308 redirected.
    const fn sends_content(self) -> bool {
        matches!(
            self,
            Hop::First
                | Hop::Redirected {
                    keep_method: true,
                    ..
                }
        )
    }
}

/// What every request of one chain shares: the `context`, the options
/// read off it, what the open was asked, the headers array read into
/// (`response_header`, undefined until a request is sent), and the first
/// throwable a notification's callback left, which the open raises once
/// it is done, as php finds it pending then.
struct Chain {
    ctx: Option<StreamCtx>,
    options: HttpOptions,
    opening: Opening,
    mode: Vec<u8>,
    headers: Option<Owner<PhpArray>>,
    thrown: Option<Thrown>,
}

impl Chain {
    /// A notification's answer: a throwable kept where none is yet, and
    /// the next callback not asked while one is, as `zend_call_function`
    /// refuses to call with a throwable pending.
    fn keep(&mut self, req: &mut Request, answered: Result<(), Thrown>) {
        if let Err(thrown) = answered {
            if self.thrown.is_none() {
                self.thrown = Some(thrown);
            } else {
                // A second throwable is given up; a fatal or an exit
                // holds none.
                match php_ext::raising::clear(req, thrown) {
                    Ok(()) | Err(_) => {}
                }
            }
        }
    }

    fn notify_info(
        &mut self,
        req: &mut Request,
        code: NotifyCode,
        message: Option<&[u8]>,
        message_code: i64,
    ) {
        if self.thrown.is_none() {
            let answered = notify_info(req, self.ctx, code, message, message_code);
            self.keep(req, answered);
        }
    }

    fn notify_error(
        &mut self,
        req: &mut Request,
        code: NotifyCode,
        message: &[u8],
        message_code: i64,
    ) {
        if self.thrown.is_none() {
            let answered = notify_error(req, self.ctx, code, Some(message), message_code);
            self.keep(req, answered);
        }
    }

    /// The headers array, made where no request made it yet.
    fn headers(&mut self, req: &mut Request) -> Result<&mut Owner<PhpArray>, Thrown> {
        let headers = match self.headers.take() {
            Some(headers) => headers,
            None => PhpArray::new(req)?,
        };
        Ok(self.headers.insert(headers))
    }

    /// One line appended to the headers array.
    fn add_header(&mut self, req: &mut Request, line: &[u8]) -> Result<(), Thrown> {
        let headers = self.headers(req)?;
        let line = PhpString::new(req, line)?.into_value();
        headers.append(req, line)
    }
}

/// `php_stream_http_response_header_info`: what the head of one
/// response said the wrapper is to do with it.
struct HeaderInfo {
    /// `Transfer-Encoding: chunked` to be decoded, the `dechunk` filter
    /// php made for it.
    chunked: bool,
    /// `Content-Length`, where it was all digits.
    file_size: usize,
    /// A header line the wrapper refused.
    error: bool,
    /// Whether a `Location` is followed.
    follow_location: bool,
    /// `Location`, its value.
    location: Option<Vec<u8>>,
}

impl HeaderInfo {
    /// `php_stream_http_response_header_info_init`.
    const fn new() -> HeaderInfo {
        HeaderInfo {
            chunked: false,
            file_size: 0,
            error: false,
            follow_location: true,
            location: None,
        }
    }
}

/// `php_stream_get_line(stream, NULL|buf, maxlen, &len)`: the next line
/// with its end, at most `maxlen - 1` bytes of it, as bytes.
fn get_line(
    req: &mut Request,
    stream: Stream,
    maxlen: Option<usize>,
) -> Result<Option<Vec<u8>>, Thrown> {
    stream.get_line(req, maxlen)
}

/// How one request reaches its host: over TLS (`use_ssl`) or not,
/// through the `proxy` (`use_proxy`) or straight to it, and with the
/// full URI on the request line (`request_fulluri`) or its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // php's three locals of the one function
struct Route {
    use_ssl: bool,
    use_proxy: bool,
    request_fulluri: bool,
}

impl Route {
    /// A `CONNECT` through the `proxy` whose tunnel then carries TLS,
    /// whose `Proxy-Authorization` the request to the host leaves out.
    const fn tunnel(self) -> bool {
        self.use_ssl && self.use_proxy
    }
}

/// `php_stream_handle_proxy_authorization_header`
/// (`Z/ext/standard/http_fopen_wrapper.c:121`): the first line of `lines`
/// whose name is `Proxy-Authorization:`, ignoring case, added to `header`
/// with `\r\n`; whether there was one.
fn proxy_authorization(lines: &[u8], header: &mut Vec<u8>) -> bool {
    let lines = c_str(lines);
    let mut s = 0;
    while s < lines.len() {
        while matches!(lines.get(s), Some(b' ' | b'\t')) {
            s += 1;
        }
        let mut p = s;
        while p < lines.len() && !matches!(lines[p], b':' | b'\r' | b'\n') {
            p += 1;
        }
        if lines.get(p) == Some(&b':') {
            p += 1;
            let name = &lines[s..p];
            while p < lines.len() && !matches!(lines[p], b'\r' | b'\n') {
                p += 1;
            }
            if name.eq_ignore_ascii_case(b"Proxy-Authorization:") {
                header.extend_from_slice(&lines[s..p]);
                header.extend_from_slice(b"\r\n");
                return true;
            }
        }
        s = p;
        while matches!(lines.get(s), Some(b'\r' | b'\n')) {
            s += 1;
        }
    }
    false
}

/// The `proxy-authorization:` line php takes out of the user's headers
/// for a request through a `CONNECT` tunnel, found in `lower` and cut
/// from `bag`, where it begins the headers or a line.
fn remove_proxy_authorization(bag: &mut Vec<u8>, lower: &[u8]) {
    const NAME: &[u8] = b"proxy-authorization:";
    let Some(found) = lower.windows(NAME.len()).position(|w| w == NAME) else {
        return;
    };
    if found != 0 && lower[found - 1] != b'\n' {
        return;
    }
    let mut s = found;
    let mut p = found + NAME.len();
    while s > 0 && matches!(lower[s - 1], b' ' | b'\t') {
        s -= 1;
    }
    while p < lower.len() && !matches!(lower[p], b'\r' | b'\n') {
        p += 1;
    }
    while matches!(lower.get(p), Some(b'\r' | b'\n')) {
        p += 1;
    }
    if p >= lower.len() {
        while s > 0 && matches!(lower[s - 1], b'\r' | b'\n') {
            s -= 1;
        }
        bag.truncate(s);
    } else {
        bag.drain(s..p);
    }
}

/// `HTTP_HEADER_BLOCK_SIZE`, the buffer the proxy's answer is read in.
const HTTP_HEADER_BLOCK_SIZE: usize = 1024;

/// `STREAM_CRYPTO_METHOD_SSLv23_CLIENT`, what the tunnel is set up with.
const SSLV23_CLIENT: i64 = (1 << 3) | (1 << 4) | (1 << 5) | 1;

/// The `CONNECT` of `php_stream_url_wrap_http_ex` through a `proxy` to an
/// `https` host (`Z/ext/standard/http_fopen_wrapper.c:506`): the host
/// kept as the `ssl` `peer_name` of the socket's `context` for the
/// handshake where none is given, the `CONNECT` written with the
/// `Proxy-Authorization` the `http` `header` holds, the proxy's answer
/// read to its blank line, then the tunnel's crypto set up and enabled,
/// and the `peer_name` taken back. `None` is a tunnel that failed, after
/// php's `Cannot connect to HTTPS server through proxy`.
fn tunnel(
    req: &mut Request,
    chain: &Chain,
    stream: Owner<Stream>,
    resource: &UriStruct,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let host = resource
        .host
        .as_deref()
        .map(c_str)
        .unwrap_or_default()
        .to_vec();
    let given = match chain.ctx {
        Some(ctx) => ctx.option(req, b"ssl", b"peer_name").is_some(),
        None => false,
    };
    let context = stream.ctx(req);
    let reset = match context {
        Some(context) if !given => {
            let set = PhpString::new(req, &host).and_then(|value| {
                let set = context.set_option(req, b"ssl", b"peer_name", &Value::string(*value));
                value.give_up(req);
                set
            });
            held!(req, stream, set);
            true
        }
        _ => false,
    };
    let mut header = b"CONNECT ".to_vec();
    header.extend_from_slice(&host);
    header.extend_from_slice(format!(":{} HTTP/1.0\r\n", resource.port.unwrap_or(0)).as_bytes());
    if let Some(ctx) = chain.ctx
        && let Some(value) = ctx.option(req, b"http", b"header")
    {
        match value.as_ref() {
            ValueRef::Array(lines) => {
                for (_, line) in walk::rows(req, lines) {
                    if let Some(line) = line.deref().as_str_bytes()
                        && proxy_authorization(line, &mut header)
                    {
                        break;
                    }
                }
            }
            ValueRef::Str(text) if !text.as_bytes().is_empty() => {
                proxy_authorization(text.as_bytes(), &mut header);
            }
            _ => {}
        }
    }
    header.extend_from_slice(b"\r\n");
    let unset = |req: &mut Request| -> Result<(), Thrown> {
        match context {
            Some(context) if reset => context.unset_option(req, b"ssl", b"peer_name"),
            _ => Ok(()),
        }
    };
    if stream.write(req, &header) != Some(header.len()) {
        let unsetting = unset(req);
        if unsetting.is_ok() {
            log(req, "Cannot connect to HTTPS server through proxy");
        }
        stream.free(req);
        return unsetting.map(|()| None);
    }
    while let Some(line) = held!(
        req,
        stream,
        get_line(req, *stream, Some(HTTP_HEADER_BLOCK_SIZE - 1))
    ) {
        if matches!(line.first(), None | Some(b'\n' | b'\r' | 0)) {
            break;
        }
    }
    let enabled = stream.crypto_setup(req, SSLV23_CLIENT, None)
        && !matches!(
            stream.crypto_enable(req, true),
            Handshake::Failed | Handshake::Unsupported
        );
    let stream = if enabled {
        Some(stream)
    } else {
        log(req, "Cannot connect to HTTPS server through proxy");
        stream.free(req);
        None
    };
    if let Err(thrown) = unset(req) {
        if let Some(stream) = stream {
            stream.free(req);
        }
        return Err(thrown);
    }
    Ok(stream)
}

/// `php_stream_wrapper_log_error` from the opener, which the open shows
/// where it fails.
fn log(req: &mut Request, message: &str) {
    req.wrapper_log_error(message);
}

/// [`log`] of a message that may hold any byte, as php's `%s` prints it.
fn log_bytes(req: &mut Request, head: &str, bytes: &[u8]) {
    log(
        req,
        &format!("{head}{}", String::from_utf8_lossy(c_str(bytes))),
    );
}

/// `php_stream_url_wrap_http_ex` (`Z/ext/standard/http_fopen_wrapper.c:357`),
/// one request of a chain: the URL parsed with the `context`'s parser, a
/// scheme other than `http` or `https` handed to its own wrapper where no
/// `proxy` says otherwise, the socket connected, the request written, the
/// head read into the chain's headers, and then the stream answered, a
/// redirect followed by one more call, or nothing. The `out:` block runs
/// on the way back up from every call, as php's does.
#[allow(clippy::too_many_lines)] // php's one function, in its order
fn wrap_http_ex(
    req: &mut Request,
    chain: &mut Chain,
    path: &[u8],
    redirect_max: i64,
    hop: Hop,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let mut redirect_max = redirect_max;
    if redirect_max < 1 {
        log(req, "Redirection limit reached, aborting");
        return Ok(None);
    }
    let Some(parser) = chain.options.parser else {
        let text = format!(
            "{}(): Provided stream context has invalid value for the \"uri_parser_class\" option",
            req.frames().active_function_name()
        );
        return Err(value_error(req, &text));
    };
    let Some(mut resource) = parser.parse(req, c_str(path))? else {
        return Ok(None);
    };
    let scheme = resource.scheme.clone().unwrap_or_default();
    let web = scheme.eq_ignore_ascii_case(b"http") || scheme.eq_ignore_ascii_case(b"https");
    let (route, transport) = if web {
        if chain
            .mode
            .iter()
            .any(|b| matches!(b, b'a' | b'w' | b'x' | b'+'))
        {
            log(req, "HTTP wrapper does not support writeable connections");
            return Ok(None);
        }
        let request_fulluri = chain.options.request_fulluri.unwrap_or(false);
        let use_ssl = scheme.len() > 4 && scheme[4] == b's';
        if resource.port.unwrap_or(0) == 0 {
            resource.port = Some(if use_ssl { 443 } else { 80 });
        }
        if let Some(proxy) = &chain.options.proxy {
            let route = Route {
                use_ssl,
                use_proxy: true,
                request_fulluri,
            };
            (route, proxy.clone())
        } else {
            let host = resource.host.as_deref().map(c_str).unwrap_or_default();
            let mut transport = if use_ssl {
                b"ssl://".to_vec()
            } else {
                b"tcp://".to_vec()
            };
            transport.extend_from_slice(host);
            transport.extend_from_slice(format!(":{}", resource.port.unwrap_or(0)).as_bytes());
            let route = Route {
                use_ssl,
                use_proxy: false,
                request_fulluri,
            };
            (route, transport)
        }
    } else {
        let Some(proxy) = chain.options.proxy.clone() else {
            let opening = Opening {
                warn: true,
                ..Opening::default()
            };
            let mode = chain.mode.clone();
            let ctx = chain.ctx;
            return req.with_stream_ctx(ctx, |req| req.open_stream_with(path, &mode, opening));
        };
        // Called from a non-http wrapper with http proxying asked for
        // (ftp).
        let route = Route {
            use_ssl: false,
            use_proxy: true,
            request_fulluri: true,
        };
        (route, proxy)
    };
    let use_ssl = route.use_ssl;
    if route.request_fulluri && c_str(path).iter().any(|b| matches!(b, b'\n' | b'\r')) {
        log(
            req,
            "HTTP wrapper full URI path does not allow CR or LF characters",
        );
        return Ok(None);
    }
    let timeout = match chain.options.timeout {
        Some(d) if d > TIMEOUT_MAX => {
            log(
                req,
                &format!("timeout must be lower than {TIMEOUT_MAX_SHOWN}"),
            );
            return Ok(None);
        }
        Some(d) => timeval(d),
        None => timeval(default_timeout(req).trunc()),
    };
    let xport = Xport::Client {
        timeout,
        connect: true,
        persistent: None,
        socket: SocketOptions::default(),
        context: chain.ctx,
    };
    let stream = match req.xport_create(&transport, xport)? {
        Ok(stream) => {
            if let Some(timeout) = timeout {
                stream.set_option(req, StreamOption::ReadTimeout(timeout));
            }
            Some(stream)
        }
        Err(XportFailure { text, .. }) => {
            if let Some(text) = text {
                log(req, &text);
            }
            None
        }
    };
    let stream = match stream {
        Some(stream) if route.tunnel() => tunnel(req, chain, stream, &resource)?,
        stream => stream,
    };
    let mut info = HeaderInfo::new();
    let Some(stream) = stream else {
        return Ok(None);
    };
    let chunk_size = if chain.opening.will_cast {
        match stream.set_option(req, StreamOption::ChunkSize(1)) {
            OptionAnswer::Previous(n) => Some(n),
            _ => None,
        }
    } else {
        None
    };
    stream.set_ctx(req, chain.ctx);
    chain.notify_info(req, NotifyCode::Connect, None, 0);
    if hop.header_init()
        && let Some(max) = chain.options.max_redirects
    {
        redirect_max = max;
    }
    let request = request_bytes(req, chain, path, &mut resource, route, hop);
    stream.write(req, &request);
    held!(req, stream, chain.headers(req));
    let status = held!(req, stream, read_status(req, chain, *stream));
    let Some(status) = status else {
        stream.free(req);
        log(req, "HTTP request failed!");
        return out(req, chain, None, hop, &info, chunk_size);
    };
    let mut last: Option<Vec<u8>> = None;
    while !stream.eof(req) {
        let Some(mut line) = held!(req, stream, get_line(req, *stream, None)) else {
            break;
        };
        let last_line = match line.first() {
            Some(b'\r') => {
                if line.get(1) != Some(&b'\n') {
                    stream.free(req);
                    log(
                        req,
                        "HTTP invalid header name (cannot start with CR character)!",
                    );
                    return out(req, chain, None, hop, &info, chunk_size);
                }
                true
            }
            Some(b'\n') => true,
            _ => false,
        };
        if let Some(previous) = last.take() {
            let parsed = headers_parse(
                req,
                chain,
                *stream,
                previous,
                Some(&mut line),
                status.code,
                &mut info,
            );
            match held!(req, stream, parsed) {
                None if info.error => {
                    stream.free(req);
                    return out(req, chain, None, hop, &info, chunk_size);
                }
                None => {}
                Some(folded) => {
                    last = Some(folded);
                    continue;
                }
            }
        } else if !last_line {
            if matches!(line.first(), Some(b' ' | b'\t')) {
                stream.free(req);
                log(
                    req,
                    "HTTP invalid response format (folding header at the start)!",
                );
                return out(req, chain, None, hop, &info, chunk_size);
            }
            header_trim(&mut line);
        }
        if last_line {
            break;
        }
        last = Some(line);
    }
    if let Some(previous) = last {
        let parsed = headers_parse(req, chain, *stream, previous, None, status.code, &mut info);
        held!(req, stream, parsed);
    }
    if !status.ok || (info.location.is_some() && info.follow_location) {
        let lenient = chain.opening.only_get_headers || chain.options.ignore_errors;
        if !info.follow_location || (lenient && redirect_max <= 1) {
            return out(req, chain, Some(stream), hop, &info, chunk_size);
        }
        if let Some(location) = info.location.clone() {
            chain.notify_info(req, NotifyCode::Redirected, Some(&location), 0);
        }
        stream.free(req);
        info.chunked = false;
        let Some(location) = info.location.take() else {
            log_bytes(req, "HTTP request failed! ", &status.line);
            return out(req, chain, None, hop, &info, chunk_size);
        };
        let new_path = redirect_path(&resource, &location, use_ssl);
        let Some(target) = parser.parse(req, c_str(&new_path))? else {
            log_bytes(req, "Invalid redirect URL! ", &new_path);
            return out(req, chain, None, hop, &info, chunk_size);
        };
        let same_origin = same_origin(&resource, &target, use_ssl);
        let controlled = [&target.user, &target.password, &target.path]
            .into_iter()
            .flatten()
            .any(|part| php_url_decode(part).iter().any(u8::is_ascii_control));
        if controlled {
            log_bytes(req, "Invalid redirect URL! ", &new_path);
            return out(req, chain, None, hop, &info, chunk_size);
        }
        let keep_method = matches!(status.code, 307 | 308);
        let strip_auth = hop.strip_auth() || !same_origin;
        let redirected = wrap_http_ex(
            req,
            chain,
            &new_path,
            redirect_max - 1,
            Hop::Redirected {
                keep_method,
                strip_auth,
            },
        )?;
        return out(req, chain, redirected, hop, &info, chunk_size);
    }
    out(req, chain, Some(stream), hop, &info, chunk_size)
}

/// The request line and the headers `php_stream_url_wrap_http_ex` sends,
/// then the `content` where the request carries it, each in php's order.
#[allow(clippy::too_many_lines)] // php's one stretch of its function
fn request_bytes(
    req: &mut Request,
    chain: &mut Chain,
    path: &[u8],
    resource: &mut UriStruct,
    route: Route,
    hop: Hop,
) -> Vec<u8> {
    let use_ssl = route.use_ssl;
    let options = &chain.options;
    let mut buf = Vec::new();
    let custom = options.method.as_ref().filter(|method| {
        !hop.redirected()
            || hop.keep_method()
            || method.as_slice() == b"GET"
            || method.as_slice() == b"HEAD"
    });
    match custom {
        Some(method) => {
            buf.extend_from_slice(method);
            buf.push(b' ');
        }
        None => buf.extend_from_slice(b"GET "),
    }
    if route.request_fulluri {
        buf.extend_from_slice(c_str(path));
    } else {
        match resource
            .path
            .as_deref()
            .map(c_str)
            .filter(|p| !p.is_empty())
        {
            Some(file) => buf.extend_from_slice(file),
            None => buf.push(b'/'),
        }
        if let Some(query) = &resource.query {
            buf.push(b'?');
            buf.extend_from_slice(c_str(query));
        }
    }
    match options.protocol_version {
        Some(version) => {
            buf.extend_from_slice(b" HTTP/");
            buf.extend_from_slice(one_decimal(version).as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        None => buf.extend_from_slice(b" HTTP/1.1\r\n"),
    }
    let mut have = Have::default();
    let mut user_headers = None;
    if let Some(header) = &options.header {
        // php's bag is a C string from here on: what follows a NUL is
        // neither searched, stripped nor sent.
        let mut bag = c_str(header).to_vec();
        let mut lower = bag.to_ascii_lowercase();
        if !hop.header_init() && !hop.keep_method() {
            strip_header(&mut bag, &mut lower, b"content-length");
            strip_header(&mut bag, &mut lower, b"content-type");
        }
        if hop.strip_auth() {
            strip_header(&mut bag, &mut lower, b"authorization");
            strip_header(&mut bag, &mut lower, b"cookie");
            if !route.use_proxy {
                strip_header(&mut bag, &mut lower, b"proxy-authorization");
            }
        }
        have.user_agent = check_has_header(&lower, b"user-agent:");
        have.host = check_has_header(&lower, b"host:");
        have.from = check_has_header(&lower, b"from:");
        have.auth = check_has_header(&lower, b"authorization:");
        have.content_length = check_has_header(&lower, b"content-length:");
        have.content_type = check_has_header(&lower, b"content-type:");
        have.connection = check_has_header(&lower, b"connection:");
        if route.tunnel() {
            remove_proxy_authorization(&mut bag, &lower);
        }
        user_headers = (!bag.is_empty()).then_some(bag);
    }
    if !have.auth
        && let Some(user) = &resource.user
    {
        let mut scratch = php_url_decode(user);
        scratch.push(b':');
        if let Some(password) = &resource.password {
            scratch.extend_from_slice(&php_url_decode(password));
        }
        buf.extend_from_slice(b"Authorization: Basic ");
        buf.extend_from_slice(c_str(&crate::base64::encode(&scratch)));
        buf.extend_from_slice(b"\r\n");
        chain.notify_info(req, NotifyCode::AuthRequired, None, 0);
    }
    let options = &chain.options;
    if !have.from
        && let Some(from) = file_ini(req, b"from")
    {
        buf.extend_from_slice(b"From: ");
        buf.extend_from_slice(c_str(&from));
        buf.extend_from_slice(b"\r\n");
    }
    if !have.host {
        buf.extend_from_slice(b"Host: ");
        buf.extend_from_slice(resource.host.as_deref().map(c_str).unwrap_or_default());
        let port = resource.port.unwrap_or(0);
        if port != 0 && ((use_ssl && port != 443) || (!use_ssl && port != 80)) {
            buf.extend_from_slice(format!(":{port}").as_bytes());
        }
        buf.extend_from_slice(b"\r\n");
    }
    if !have.connection {
        buf.extend_from_slice(b"Connection: close\r\n");
    }
    let agent = options
        .user_agent
        .clone()
        .or_else(|| file_ini(req, b"user_agent"));
    if !have.user_agent
        && let Some(agent) = agent
        && !c_str(&agent).is_empty()
    {
        buf.extend_from_slice(b"User-Agent: ");
        buf.extend_from_slice(c_str(&agent));
        buf.extend_from_slice(b"\r\n");
    }
    let content = options.content.as_ref().filter(|_| hop.sends_content());
    if let Some(user_headers) = user_headers {
        if let Some(content) = content
            && !have.content_length
        {
            buf.extend_from_slice(format!("Content-Length: {}\r\n", content.len()).as_bytes());
            have.content_length = true;
        }
        buf.extend_from_slice(c_str(&user_headers));
        buf.extend_from_slice(b"\r\n");
    }
    match content {
        Some(content) => {
            if !have.content_length {
                buf.extend_from_slice(format!("Content-Length: {}\r\n", content.len()).as_bytes());
            }
            if !have.content_type {
                buf.extend_from_slice(b"Content-Type: application/x-www-form-urlencoded\r\n");
                docref(
                    req,
                    Level::Notice,
                    "Content-type not specified assuming application/x-www-form-urlencoded",
                );
            }
            buf.extend_from_slice(b"\r\n");
            buf.extend_from_slice(content);
        }
        None => buf.extend_from_slice(b"\r\n"),
    }
    buf
}

/// `smart_str_append_printf(&req_buf, "%.1F", version)`.
fn one_decimal(version: f64) -> String {
    if version.is_nan() {
        return String::from("NAN");
    }
    if version.is_infinite() {
        return String::from(if version < 0.0 { "-INF" } else { "INF" });
    }
    format!("{version:.1}")
}

/// `HTTP_HEADER_*`, which of php's own headers the `header` option
/// already carries.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // php's bitset, one field per header
struct Have {
    user_agent: bool,
    host: bool,
    auth: bool,
    from: bool,
    content_length: bool,
    content_type: bool,
    connection: bool,
}

/// `next_header_line`: where the line after the one at `at` starts,
/// past its `\r`, its `\n` or its `\r\n`.
fn next_header_line(bag: &[u8], mut at: usize) -> usize {
    while at < bag.len() && !matches!(bag[at], b'\r' | b'\n') {
        at += 1;
    }
    if bag.get(at) == Some(&b'\r') {
        at += 1;
    }
    if bag.get(at) == Some(&b'\n') {
        at += 1;
    }
    at
}

/// `strip_header`: every line of the lowered bag that opens with `name`,
/// blanks allowed before its colon, taken out of both bags with the
/// folded lines that carry the rest of its value; where nothing follows
/// it, the line breaks before it too, so the `\r\n` sent after the bag
/// does not close the header block early. The first chain leaves its
/// origin, the second stays on it, and the third comes back to its
/// origin from another, withheld all the same, as every hop after the
/// one that left keeps `HTTP_WRAPPER_STRIP_AUTH`:
///
/// ```php
/// $a = stream_socket_server('tcp://127.0.0.1:0');
/// $b = stream_socket_server('tcp://127.0.0.1:0');
/// $addrA = stream_socket_get_name($a, false);
/// $addrB = stream_socket_get_name($b, false);
/// $echo = function ($c) {
///     preg_match_all('/^([A-Za-z-]+)[ \t]*:/m', fread($c, 4096), $m);
///     $body = implode(',', $m[1]);
///     fwrite($c, "HTTP/1.0 200 OK\r\nContent-Length: " . strlen($body) . "\r\n\r\n$body");
///     fclose($c);
/// };
/// if (pcntl_fork() === 0) {
///     $c = stream_socket_accept($a);
///     fread($c, 4096);
///     fwrite($c, "HTTP/1.0 302 Found\r\nLocation: http://$addrB/x\r\nContent-Length: 0\r\n\r\n");
///     fclose($c);
///     $echo(stream_socket_accept($b));
///     $c = stream_socket_accept($a);
///     fread($c, 4096);
///     fwrite($c, "HTTP/1.0 302 Found\r\nLocation: /y\r\nContent-Length: 0\r\n\r\n");
///     fclose($c);
///     $echo(stream_socket_accept($a));
///     $c = stream_socket_accept($a);
///     fread($c, 4096);
///     fwrite($c, "HTTP/1.0 302 Found\r\nLocation: http://$addrB/b\r\nContent-Length: 0\r\n\r\n");
///     fclose($c);
///     $c = stream_socket_accept($b);
///     fread($c, 4096);
///     fwrite($c, "HTTP/1.0 302 Found\r\nLocation: http://$addrA/back\r\nContent-Length: 0\r\n\r\n");
///     fclose($c);
///     $echo(stream_socket_accept($a));
///     exit(0);
/// }
/// $ctx = stream_context_create(['http' => ['header' => "Authorization: Bearer x\r\n"
///     . "Cookie : sid=1\r\n\tfolded\r\nX-Custom: 1\r\nProxy-Authorization: Basic eA=="]]);
/// var_dump(file_get_contents("http://$addrA/", false, $ctx));
/// var_dump(file_get_contents("http://$addrA/", false, $ctx));
/// var_dump(file_get_contents("http://$addrA/z", false, $ctx));
/// pcntl_wait($status);
/// ```
/// ```expect process
/// string(24) "Host,Connection,X-Custom"
/// string(65) "Host,Connection,Authorization,Cookie,X-Custom,Proxy-Authorization"
/// string(24) "Host,Connection,X-Custom"
/// ```
fn strip_header(bag: &mut Vec<u8>, lower: &mut Vec<u8>, name: &[u8]) {
    let mut line = 0;
    while line < lower.len() {
        if !lower[line..].starts_with(name) {
            line = next_header_line(lower, line);
            continue;
        }
        let mut colon = line + name.len();
        while matches!(lower.get(colon), Some(b' ' | b'\t')) {
            colon += 1;
        }
        if lower.get(colon) != Some(&b':') {
            line = next_header_line(lower, line);
            continue;
        }
        let mut next = next_header_line(lower, line);
        while matches!(lower.get(next), Some(b' ' | b'\t')) {
            next = next_header_line(lower, next);
        }
        if next == lower.len() {
            while line > 0 && matches!(lower[line - 1], b'\r' | b'\n') {
                line -= 1;
            }
        }
        bag.drain(line..next);
        lower.drain(line..next);
    }
}

/// Whether a redirect from `resource` to `target` stays on its origin:
/// the same scheme and host, ignoring case, and the same `port`, the
/// default of the request's own scheme standing in for one not given.
fn same_origin(resource: &UriStruct, target: &UriStruct, use_ssl: bool) -> bool {
    let default_port = if use_ssl { 443 } else { 80 };
    let port = |uri: &UriStruct| uri.port.filter(|port| *port != 0).unwrap_or(default_port);
    let same = |a: &Option<Vec<u8>>, b: &Option<Vec<u8>>| matches!((a, b), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b));
    same(&resource.scheme, &target.scheme)
        && same(&resource.host, &target.host)
        && port(resource) == port(target)
}

/// `check_has_header`: `name` anywhere in the lowered bag where it
/// starts the bag or follows a `\r\n`.
fn check_has_header(lower: &[u8], name: &[u8]) -> bool {
    lower
        .windows(name.len())
        .enumerate()
        .any(|(at, w)| w == name && (at == 0 || (at >= 2 && &lower[at - 2..at] == b"\r\n")))
}

/// A response's status line: the line as read, up to 126 bytes with its
/// end, which the failure message prints, its code, and whether the
/// request went well by it (`reqok`).
struct Status {
    line: Vec<u8>,
    code: i64,
    ok: bool,
}

/// The status line of `php_stream_url_wrap_http_ex`: read with a
/// 128-byte buffer, a `1xx` other than `101` skipped to the next line
/// that opens `HTTP/1`, `2xx` and `3xx` well and anything else told to
/// the notifier, and the line appended to the headers without its end,
/// the rest of a line longer than the buffer read and dropped. `None`
/// where the stream gave no line.
fn read_status(
    req: &mut Request,
    chain: &mut Chain,
    stream: Stream,
) -> Result<Option<Status>, Thrown> {
    if stream.eof(req) {
        return Ok(None);
    }
    let Some(mut line) = get_line(req, stream, Some(127))? else {
        return Ok(None);
    };
    let code_of = |line: &[u8]| {
        if line.len() > 9 {
            i64::from(atoi(&line[9..]))
        } else {
            0
        }
    };
    let mut code = code_of(&line);
    let mut ok = chain.opening.only_get_headers || chain.options.ignore_errors;
    if (100..200).contains(&code) && code != 101 {
        while !stream.eof(req) {
            let Some(next) = get_line(req, stream, Some(127))? else {
                break;
            };
            line = next;
            if line.len() >= 6 && line[..6].eq_ignore_ascii_case(b"HTTP/1") {
                break;
            }
        }
        code = code_of(&line);
    }
    if (200..400).contains(&code) {
        ok = true;
    } else if code == 403 {
        chain.notify_error(req, NotifyCode::AuthResult, &line, code);
    } else {
        chain.notify_error(req, NotifyCode::Failure, &line, code);
    }
    let mut shown = line.as_slice();
    if shown.last() == Some(&b'\n') {
        shown = &shown[..shown.len() - 1];
        if shown.last() == Some(&b'\r') {
            shown = &shown[..shown.len() - 1];
        }
    } else {
        get_line(req, stream, None)?;
    }
    let shown = shown.to_vec();
    chain.add_header(req, &shown)?;
    Ok(Some(Status { line, code, ok }))
}

/// `php_stream_http_response_header_trim`: the line's end and the blanks
/// before it taken off; whether there were blanks.
fn header_trim(line: &mut Vec<u8>) -> bool {
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    let space_trim = matches!(line.last(), Some(b' ' | b'\t'));
    while matches!(line.last(), Some(b' ' | b'\t')) {
        line.pop();
    }
    space_trim
}

/// `php_stream_http_response_headers_parse`
/// (`Z/ext/standard/http_fopen_wrapper.c:198`): a `line` that continues
/// the `last` one folded into it, which is handed back to be continued
/// further; otherwise the `last` header read, a name with a blank or a
/// line with no colon refused, `Location`, `Content-Type`,
/// `Content-Length` and `Transfer-Encoding: chunked` taken note of, and
/// the line appended to the headers but for a chunked encoding the
/// wrapper decodes.
fn headers_parse(
    req: &mut Request,
    chain: &mut Chain,
    _stream: Stream,
    last: Vec<u8>,
    line: Option<&mut Vec<u8>>,
    code: i64,
    info: &mut HeaderInfo,
) -> Result<Option<Vec<u8>>, Thrown> {
    let mut last = last;
    if let Some(line) = line
        && !matches!(line.first(), Some(b'\n' | b'\r'))
    {
        if header_trim(line) && line.is_empty() {
            return Ok(Some(last));
        }
        if matches!(line.first(), Some(b' ' | b'\t')) {
            let folded = line
                .iter()
                .position(|b| !matches!(b, b' ' | b'\t'))
                .unwrap_or(line.len());
            last.push(b' ');
            last.extend_from_slice(&line[folded..]);
            return Ok(Some(last));
        }
    }
    let Some(colon) = last.iter().position(|b| *b == b':') else {
        info.error = true;
        log(
            req,
            "HTTP invalid response format (no colon in header line)!",
        );
        return Ok(None);
    };
    if last
        .get(1..colon)
        .is_some_and(|name| name.iter().any(|b| matches!(b, b' ' | b'\t')))
    {
        info.error = true;
        log(req, "HTTP invalid response format (space in header name)!");
        return Ok(None);
    }
    let end = last.len().saturating_sub(1);
    let mut at = colon + 1;
    while at < end && matches!(last[at], b' ' | b'\t') {
        at += 1;
    }
    let value = c_str(last.get(at..).unwrap_or_default()).to_vec();
    let starts =
        |name: &[u8]| last.len() >= name.len() && last[..name.len()].eq_ignore_ascii_case(name);
    let mut store = true;
    if starts(b"Location:") {
        match chain.options.follow_location {
            Some(follow) => info.follow_location = follow,
            None if !((300..304).contains(&code) || code == 307 || code == 308) => {
                info.follow_location = false;
            }
            None => {}
        }
        if value.len() > HTTP_HEADER_MAX_LOCATION_SIZE {
            info.error = true;
            log(
                req,
                &format!(
                    "HTTP Location header size is over the limit of {HTTP_HEADER_MAX_LOCATION_SIZE} bytes"
                ),
            );
            return Ok(None);
        }
        info.location = Some(value);
    } else if starts(b"Content-Type:") {
        chain.notify_info(req, NotifyCode::MimeTypeIs, Some(&value), 0);
    } else if starts(b"Content-Length:") {
        if value.first().is_some_and(u8::is_ascii_digit) && value.iter().all(u8::is_ascii_digit) {
            let parsed = value.iter().fold(0u64, |n, d| {
                n.saturating_mul(10).saturating_add(u64::from(d - b'0'))
            });
            info.file_size = usize::try_from(parsed.min(i64::MAX as u64)).unwrap_or(usize::MAX);
            if chain.thrown.is_none() {
                let answered = notify(
                    req,
                    chain.ctx,
                    Notification::FileSize {
                        size: info.file_size,
                        message: c_str(&last),
                        message_code: 0,
                    },
                );
                chain.keep(req, answered);
            }
        }
    } else if starts(b"Transfer-Encoding:")
        && value.len() >= 7
        && value[..7].eq_ignore_ascii_case(b"Chunked")
        && !chain.opening.only_get_headers
        && chain.options.auto_decode.unwrap_or(true)
    {
        info.chunked = true;
        store = false;
    }
    if store {
        chain.add_header(req, &last)?;
    }
    Ok(None)
}

/// Where a `Location` points: itself where it is an absolute `http`,
/// `https`, `ftp` or `ftps` URL; otherwise a path on the same scheme,
/// host and `port`, one that does not open with `/` taken against the
/// directory of the request's path as php's `strrchr` finds it.
fn redirect_path(resource: &UriStruct, location: &[u8], use_ssl: bool) -> Vec<u8> {
    let absolute = location.len() >= 8
        && [&b"http://"[..], b"https://", b"ftp://", b"ftps://"]
            .into_iter()
            .any(|scheme| location[..scheme.len()].eq_ignore_ascii_case(scheme));
    if absolute {
        return location.to_vec();
    }
    let loc_path = if location.first() == Some(&b'/') {
        location.to_vec()
    } else if location.len() > 1
        && let Some(path) = resource.path.as_deref().map(c_str)
    {
        // `strrchr` finds the last `/` and the path is cut after it; a
        // path with none has its first byte made the `/`, or is `/`.
        let mut dir = match path.iter().rposition(|b| *b == b'/') {
            Some(slash) => path[..=slash].to_vec(),
            None => b"/".to_vec(),
        };
        if dir != b"/" {
            dir.push(b'/');
        }
        dir.extend_from_slice(location);
        dir
    } else {
        let mut loc_path = b"/".to_vec();
        loc_path.extend_from_slice(location);
        loc_path
    };
    let scheme = resource.scheme.as_deref().map(c_str).unwrap_or_default();
    let host = resource.host.as_deref().map(c_str).unwrap_or_default();
    let port = resource.port.unwrap_or(0);
    let mut new_path = scheme.to_vec();
    new_path.extend_from_slice(b"://");
    new_path.extend_from_slice(host);
    if (use_ssl && port != 443) || (!use_ssl && port != 80) {
        new_path.extend_from_slice(format!(":{port}").as_bytes());
    }
    new_path.extend_from_slice(&loc_path);
    new_path
}

/// The `out:` block of `php_stream_url_wrap_http_ex`, on the stream a
/// call answers, once per call on the way back up: the headers made the
/// stream's `wrapper_data` by the first request of the chain, the
/// progress counted from this response's `Content-Length`, the chunk
/// size put back, the stream at its start under the `mode` asked for,
/// chunks decoded where this response said so, and the bytes already
/// read past the head counted.
fn out(
    req: &mut Request,
    chain: &mut Chain,
    stream: Option<Owner<Stream>>,
    hop: Hop,
    info: &HeaderInfo,
    chunk_size: Option<usize>,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let Some(stream) = stream else {
        return Ok(None);
    };
    if hop.header_init()
        && let Some(headers) = chain.headers.as_deref().copied()
    {
        let value = Owner::take(req, Value::array(headers));
        stream.set_wrapper_data(req, value);
    }
    if chain.thrown.is_none() {
        let answered = notify(
            req,
            chain.ctx,
            Notification::ProgressInit {
                sofar: 0,
                max: info.file_size,
            },
        );
        chain.keep(req, answered);
    }
    if let Some(chunk_size) = chunk_size {
        stream.set_option(req, StreamOption::ChunkSize(chunk_size));
    }
    stream.rewind_as(req, &chain.mode);
    if info.chunked
        && let Some(filter) = held!(
            req,
            stream,
            stream.add_filter(req, b"dechunk", None, 1, true)
        )
    {
        filter.give_up(req);
    }
    let unread = held!(req, stream, unread_bytes(req, *stream));
    if unread > 0 && chain.thrown.is_none() {
        let answered = notify(
            req,
            chain.ctx,
            Notification::ProgressIncrement {
                dsofar: unread,
                dmax: 0,
            },
        );
        chain.keep(req, answered);
    }
    Ok(Some(stream))
}

/// `stream->writepos - stream->readpos`, what `stream_get_meta_data`
/// prints as `unread_bytes`.
fn unread_bytes(req: &mut Request, stream: Stream) -> Result<usize, Thrown> {
    let meta = stream.meta_data(req)?;
    let unread = meta
        .find(b"unread_bytes")
        .and_then(|v| v.as_int())
        .unwrap_or(0);
    meta.give_up(req);
    Ok(usize::try_from(unread).unwrap_or(0))
}
