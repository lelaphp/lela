//! HTTP/1.x as php's built-in web server reads it: `php_http_parser`
//! (`Z/sapi/cli/php_http_parser.c`, joyent's `http_parser` as php keeps
//! it, built without `HTTP_PARSER_STRICT`), the request half, byte for
//! byte, with each callback where php's fires it; the reason phrases
//! php's hosts write a status line with (`Z/main/http_status_codes.h`);
//! and the path a request names normalised as `normalize_vpath` does
//! (`Z/sapi/cli/php_cli_server.c:1538`).

/// `enum php_http_method`, in php's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Delete,
    Get,
    Head,
    Post,
    Put,
    Patch,
    Connect,
    Options,
    Trace,
    Copy,
    Lock,
    Mkcol,
    Move,
    Mkcalendar,
    Propfind,
    Proppatch,
    Search,
    Unlock,
    Report,
    Mkactivity,
    Checkout,
    Merge,
    Msearch,
    Notify,
    Subscribe,
    Unsubscribe,
    NotImplemented,
}

/// A request's method as `php_http_parser` read it, `enum
/// php_http_method`: the methods the server acts on by name, and every
/// other by the name `php_http_method_str` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Method(Verb);

impl Method {
    pub const DELETE: Method = Method(Verb::Delete);
    pub const GET: Method = Method(Verb::Get);
    pub const HEAD: Method = Method(Verb::Head);
    pub const POST: Method = Method(Verb::Post);
    pub const PUT: Method = Method(Verb::Put);
    pub const PATCH: Method = Method(Verb::Patch);
    /// `PHP_HTTP_NOT_IMPLEMENTED`: a method php's parser does not know.
    pub const NOT_IMPLEMENTED: Method = Method(Verb::NotImplemented);

    /// `php_http_method_str`: `method_strings` by the method.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name()
    }
}

impl Verb {
    /// `method_strings[m]`.
    fn name(self) -> &'static str {
        match self {
            Verb::Delete => "DELETE",
            Verb::Get => "GET",
            Verb::Head => "HEAD",
            Verb::Post => "POST",
            Verb::Put => "PUT",
            Verb::Patch => "PATCH",
            Verb::Connect => "CONNECT",
            Verb::Options => "OPTIONS",
            Verb::Trace => "TRACE",
            Verb::Copy => "COPY",
            Verb::Lock => "LOCK",
            Verb::Mkcol => "MKCOL",
            Verb::Move => "MOVE",
            Verb::Mkcalendar => "MKCALENDAR",
            Verb::Propfind => "PROPFIND",
            Verb::Proppatch => "PROPPATCH",
            Verb::Search => "SEARCH",
            Verb::Unlock => "UNLOCK",
            Verb::Report => "REPORT",
            Verb::Mkactivity => "MKACTIVITY",
            Verb::Checkout => "CHECKOUT",
            Verb::Merge => "MERGE",
            Verb::Msearch => "M-SEARCH",
            Verb::Notify => "NOTIFY",
            Verb::Subscribe => "SUBSCRIBE",
            Verb::Unsubscribe => "UNSUBSCRIBE",
            Verb::NotImplemented => "NOTIMPLEMENTED",
        }
    }
}

/// What the parser hands its caller, php's `php_http_parser_settings`:
/// each data callback with the bytes of this buffer it covers, called
/// again for the rest where a buffer ends inside one, and `false` from
/// any of them stops the parse where it stands, as php's nonzero does.
pub trait Events {
    fn path(&mut self, at: &[u8]) -> bool;
    fn query_string(&mut self, at: &[u8]) -> bool;
    /// The URL, with the method the request line named.
    fn url(&mut self, method: Method, at: &[u8]) -> bool;
    fn header_field(&mut self, at: &[u8]) -> bool;
    fn header_value(&mut self, at: &[u8]) -> bool;
    fn headers_complete(&mut self) -> bool;
    fn body(&mut self, at: &[u8]);
    /// The message read whole, with the version the request line named.
    fn message_complete(&mut self, major: u16, minor: u16) -> bool;
}

/// The parser's states, `enum state`'s request half in php's order; the
/// header states end at `HeadersAlmostDone`, which `nread`'s cap reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Dead,
    StartReq,
    ReqMethod,
    ReqSpacesBeforeUrl,
    ReqSchema,
    ReqSchemaSlash,
    ReqSchemaSlashSlash,
    ReqHost,
    ReqPort,
    ReqPath,
    ReqQueryStringStart,
    ReqQueryString,
    ReqFragmentStart,
    ReqFragment,
    ReqHttpStart,
    ReqHttpH,
    ReqHttpHT,
    ReqHttpHTT,
    ReqHttpHTTP,
    ReqFirstHttpMajor,
    ReqHttpMajor,
    ReqFirstHttpMinor,
    ReqHttpMinor,
    ReqLineAlmostDone,
    HeaderFieldStart,
    HeaderField,
    HeaderValueStart,
    HeaderValue,
    HeaderAlmostDone,
    HeadersAlmostDone,
    ChunkSizeStart,
    ChunkSize,
    ChunkSizeAlmostDone,
    ChunkParameters,
    ChunkData,
    ChunkDataAlmostDone,
    ChunkDataDone,
    BodyIdentity,
}

/// `enum header_states`: how far a header's name, and then its value,
/// has matched one of the names the parser acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    General,
    C,
    Co,
    Con,
    MatchingConnection,
    MatchingProxyConnection,
    MatchingContentLength,
    MatchingTransferEncoding,
    MatchingUpgrade,
    Connection,
    ContentLength,
    TransferEncoding,
    Upgrade,
    MatchingTransferEncodingChunked,
    MatchingConnectionKeepAlive,
    MatchingConnectionClose,
    TransferEncodingChunked,
    ConnectionKeepAlive,
    ConnectionClose,
}

const F_CHUNKED: u8 = 1 << 0;
const F_CONNECTION_KEEP_ALIVE: u8 = 1 << 1;
const F_CONNECTION_CLOSE: u8 = 1 << 2;
const F_TRAILING: u8 = 1 << 3;
const F_UPGRADE: u8 = 1 << 4;

/// `PHP_HTTP_MAX_HEADER_SIZE`: the most a request's head may take.
pub const MAX_HEADER_SIZE: u32 = 80 * 1024;

/// `php_http_parser` over a request: what persists between buffers.
#[derive(Debug, Clone)]
pub struct Parser {
    state: State,
    header: Header,
    index: u8,
    nread: u32,
    flags: u8,
    content_length: i64,
    major: u16,
    minor: u16,
    method: Verb,
}

impl Default for Parser {
    fn default() -> Self {
        Parser::new()
    }
}

/// Where a buffer's data callbacks started, `*_mark`.
#[derive(Default)]
struct Marks {
    header_field: Option<usize>,
    header_value: Option<usize>,
    fragment: Option<usize>,
    query_string: Option<usize>,
    path: Option<usize>,
    url: Option<usize>,
}

/// A callback that stopped the parse.
struct Stopped;

impl Parser {
    /// `php_http_parser_init(PHP_HTTP_REQUEST)`.
    #[must_use]
    pub fn new() -> Parser {
        Parser {
            state: State::StartReq,
            header: Header::General,
            index: 0,
            nread: 0,
            flags: 0,
            content_length: -1,
            major: 0,
            minor: 0,
            method: Verb::Delete,
        }
    }

    /// Whether the parser stands before any byte of a request, `s_start_req`.
    #[must_use]
    pub fn at_start(&self) -> bool {
        self.state == State::StartReq
    }

    /// The method the request line named.
    #[must_use]
    pub fn method(&self) -> Method {
        Method(self.method)
    }

    /// `php_http_parser_execute`: `data` fed through, and how much of it
    /// was taken; less than all of it is a request php refuses, or a
    /// callback that stopped it.
    pub fn execute(&mut self, events: &mut dyn Events, data: &[u8]) -> usize {
        let mut marks = Marks::default();
        match self.state {
            State::HeaderField => marks.header_field = Some(0),
            State::HeaderValue => marks.header_value = Some(0),
            State::ReqFragment => marks.fragment = Some(0),
            State::ReqQueryString => marks.query_string = Some(0),
            State::ReqPath => marks.path = Some(0),
            _ => {}
        }
        if matches!(
            self.state,
            State::ReqPath
                | State::ReqSchema
                | State::ReqSchemaSlash
                | State::ReqSchemaSlashSlash
                | State::ReqPort
                | State::ReqQueryStringStart
                | State::ReqQueryString
                | State::ReqHost
                | State::ReqFragmentStart
                | State::ReqFragment
        ) {
            marks.url = Some(0);
        }
        let mut p = 0;
        while p < data.len() {
            match self.step(events, data, &mut p, &mut marks) {
                Ok(()) => p += 1,
                Err(Stopped) => return p,
            }
        }
        // `CALLBACK_NOCLEAR` over what the buffer ended inside, in php's
        // order; one that stops the parse stops the rest, and php's
        // `return (p - data)` there is the whole buffer all the same.
        let _ = marks
            .header_field
            .is_none_or(|at| events.header_field(&data[at..]))
            && marks
                .header_value
                .is_none_or(|at| events.header_value(&data[at..]))
            && marks
                .query_string
                .is_none_or(|at| events.query_string(&data[at..]))
            && marks.path.is_none_or(|at| events.path(&data[at..]))
            && marks
                .url
                .is_none_or(|at| events.url(Method(self.method), &data[at..]));
        data.len()
    }

    /// One byte, `data[*p]`, through php's `switch (state)`; a bulk body
    /// read moves `*p` to the last byte it took.
    #[allow(clippy::too_many_lines)]
    fn step(
        &mut self,
        events: &mut dyn Events,
        data: &[u8],
        p: &mut usize,
        marks: &mut Marks,
    ) -> Result<(), Stopped> {
        let at = *p;
        let ch = data[at];
        if self.state <= State::HeadersAlmostDone && self.flags & F_TRAILING == 0 {
            self.nread += 1;
            if self.nread > MAX_HEADER_SIZE {
                return self.error();
            }
        }
        let lower = ch | 0x20;
        match self.state {
            State::Dead => return self.error(),
            State::StartReq => {
                if ch == b'\r' || ch == b'\n' {
                    return Ok(());
                }
                self.flags = 0;
                self.content_length = -1;
                if !ch.is_ascii_uppercase() {
                    return self.error();
                }
                self.method = match ch {
                    b'C' => Verb::Connect,
                    b'D' => Verb::Delete,
                    b'G' => Verb::Get,
                    b'H' => Verb::Head,
                    b'L' => Verb::Lock,
                    b'M' => Verb::Mkcol,
                    b'N' => Verb::Notify,
                    b'O' => Verb::Options,
                    b'P' => Verb::Post,
                    b'R' => Verb::Report,
                    b'S' => Verb::Subscribe,
                    b'T' => Verb::Trace,
                    b'U' => Verb::Unlock,
                    _ => Verb::NotImplemented,
                };
                self.index = 1;
                self.state = State::ReqMethod;
            }
            State::ReqMethod => self.method_byte(ch)?,
            State::ReqSpacesBeforeUrl => {
                if ch == b' ' {
                    return Ok(());
                }
                if ch == b'/' || ch == b'*' {
                    marks.url = Some(at);
                    marks.path = Some(at);
                    self.state = State::ReqPath;
                } else if lower.is_ascii_lowercase() {
                    marks.url = Some(at);
                    self.state = State::ReqSchema;
                } else {
                    return self.error();
                }
            }
            State::ReqSchema => {
                if lower.is_ascii_lowercase() {
                    return Ok(());
                }
                self.state = match ch {
                    b':' => State::ReqSchemaSlash,
                    b'.' | b'0'..=b'9' => State::ReqHost,
                    _ => return self.error(),
                };
            }
            State::ReqSchemaSlash => self.state = State::ReqSchemaSlashSlash,
            State::ReqSchemaSlashSlash => self.state = State::ReqHost,
            State::ReqHost | State::ReqPort => {
                let host = self.state == State::ReqHost;
                if ch.is_ascii_digit()
                    || (host && (lower.is_ascii_lowercase() || ch == b'.' || ch == b'-'))
                {
                    return Ok(());
                }
                match ch {
                    b':' if host => self.state = State::ReqPort,
                    b'/' => {
                        marks.path = Some(at);
                        self.state = State::ReqPath;
                    }
                    b' ' => {
                        self.callback(events, marks, Mark::Url, data, at)?;
                        self.state = State::ReqHttpStart;
                    }
                    _ => return self.error(),
                }
            }
            State::ReqPath => {
                if normal_url_char(ch) {
                    return Ok(());
                }
                match ch {
                    b' ' | b'\r' | b'\n' => {
                        self.callback(events, marks, Mark::Url, data, at)?;
                        self.callback(events, marks, Mark::Path, data, at)?;
                        self.end_of_url(ch);
                    }
                    b'?' => {
                        self.callback(events, marks, Mark::Path, data, at)?;
                        self.state = State::ReqQueryStringStart;
                    }
                    b'#' => {
                        self.callback(events, marks, Mark::Path, data, at)?;
                        self.state = State::ReqFragmentStart;
                    }
                    _ => return self.error(),
                }
            }
            State::ReqQueryStringStart => {
                if normal_url_char(ch) {
                    marks.query_string = Some(at);
                    self.state = State::ReqQueryString;
                    return Ok(());
                }
                match ch {
                    b'?' => {}
                    b' ' | b'\r' | b'\n' => {
                        self.callback(events, marks, Mark::Url, data, at)?;
                        self.end_of_url(ch);
                    }
                    b'#' => self.state = State::ReqFragmentStart,
                    _ => return self.error(),
                }
            }
            State::ReqQueryString => {
                if normal_url_char(ch) {
                    return Ok(());
                }
                match ch {
                    b'?' => {}
                    b' ' | b'\r' | b'\n' => {
                        self.callback(events, marks, Mark::Url, data, at)?;
                        self.callback(events, marks, Mark::QueryString, data, at)?;
                        self.end_of_url(ch);
                    }
                    b'#' => {
                        self.callback(events, marks, Mark::QueryString, data, at)?;
                        self.state = State::ReqFragmentStart;
                    }
                    _ => return self.error(),
                }
            }
            State::ReqFragmentStart => {
                if normal_url_char(ch) {
                    marks.fragment = Some(at);
                    self.state = State::ReqFragment;
                    return Ok(());
                }
                match ch {
                    b' ' | b'\r' | b'\n' => {
                        self.callback(events, marks, Mark::Url, data, at)?;
                        self.end_of_url(ch);
                    }
                    b'?' => {
                        marks.fragment = Some(at);
                        self.state = State::ReqFragment;
                    }
                    b'#' => {}
                    _ => return self.error(),
                }
            }
            State::ReqFragment => {
                if normal_url_char(ch) {
                    return Ok(());
                }
                match ch {
                    b' ' | b'\r' | b'\n' => {
                        self.callback(events, marks, Mark::Url, data, at)?;
                        marks.fragment = None;
                        self.end_of_url(ch);
                    }
                    b'?' | b'#' => {}
                    _ => return self.error(),
                }
            }
            State::ReqHttpStart => match ch {
                b'H' => self.state = State::ReqHttpH,
                b' ' => {}
                _ => return self.error(),
            },
            State::ReqHttpH => self.state = State::ReqHttpHT,
            State::ReqHttpHT => self.state = State::ReqHttpHTT,
            State::ReqHttpHTT => self.state = State::ReqHttpHTTP,
            State::ReqHttpHTTP => self.state = State::ReqFirstHttpMajor,
            State::ReqFirstHttpMajor => {
                if !(b'1'..=b'9').contains(&ch) {
                    return self.error();
                }
                self.major = u16::from(ch - b'0');
                self.state = State::ReqHttpMajor;
            }
            State::ReqHttpMajor => {
                if ch == b'.' {
                    self.state = State::ReqFirstHttpMinor;
                    return Ok(());
                }
                self.major = self.version_digit(self.major, ch)?;
            }
            State::ReqFirstHttpMinor => {
                if !ch.is_ascii_digit() {
                    return self.error();
                }
                self.minor = u16::from(ch - b'0');
                self.state = State::ReqHttpMinor;
            }
            State::ReqHttpMinor => match ch {
                b'\r' => self.state = State::ReqLineAlmostDone,
                b'\n' => self.state = State::HeaderFieldStart,
                _ => self.minor = self.version_digit(self.minor, ch)?,
            },
            State::ReqLineAlmostDone => {
                if ch != b'\n' {
                    return self.error();
                }
                self.state = State::HeaderFieldStart;
            }
            State::HeaderFieldStart => {
                if ch == b'\r' {
                    self.state = State::HeadersAlmostDone;
                    return Ok(());
                }
                if ch == b'\n' {
                    // "they might be just sending \n instead of \r\n so
                    // this would be the second \n to denote the end of
                    // headers"
                    return self.headers_done(events);
                }
                let c = token(ch);
                if c == 0 {
                    return self.error();
                }
                marks.header_field = Some(at);
                self.index = 0;
                self.state = State::HeaderField;
                self.header = match c {
                    b'c' => Header::C,
                    b'p' => Header::MatchingProxyConnection,
                    b't' => Header::MatchingTransferEncoding,
                    b'u' => Header::MatchingUpgrade,
                    _ => Header::General,
                };
            }
            State::HeaderField => {
                let c = token(ch);
                if c != 0 {
                    self.header_name_byte(c, ch);
                    return Ok(());
                }
                match ch {
                    b':' => {
                        self.callback(events, marks, Mark::HeaderField, data, at)?;
                        self.state = State::HeaderValueStart;
                    }
                    b'\r' => {
                        self.state = State::HeaderAlmostDone;
                        self.callback(events, marks, Mark::HeaderField, data, at)?;
                    }
                    b'\n' => {
                        self.callback(events, marks, Mark::HeaderField, data, at)?;
                        self.state = State::HeaderFieldStart;
                    }
                    _ => return self.error(),
                }
            }
            State::HeaderValueStart => {
                if ch == b' ' {
                    return Ok(());
                }
                marks.header_value = Some(at);
                self.state = State::HeaderValue;
                self.index = 0;
                if ch == b'\r' {
                    self.callback(events, marks, Mark::HeaderValue, data, at)?;
                    self.header = Header::General;
                    self.state = State::HeaderAlmostDone;
                    return Ok(());
                }
                if ch == b'\n' {
                    self.callback(events, marks, Mark::HeaderValue, data, at)?;
                    self.state = State::HeaderFieldStart;
                    return Ok(());
                }
                self.header = match self.header {
                    Header::Upgrade => {
                        self.flags |= F_UPGRADE;
                        Header::General
                    }
                    Header::TransferEncoding if lower == b'c' => {
                        Header::MatchingTransferEncodingChunked
                    }
                    Header::ContentLength => {
                        if !ch.is_ascii_digit() {
                            return self.error();
                        }
                        self.content_length = i64::from(ch - b'0');
                        Header::ContentLength
                    }
                    Header::Connection if lower == b'k' => Header::MatchingConnectionKeepAlive,
                    Header::Connection if lower == b'c' => Header::MatchingConnectionClose,
                    _ => Header::General,
                };
            }
            State::HeaderValue => {
                if ch == b'\r' {
                    self.callback(events, marks, Mark::HeaderValue, data, at)?;
                    self.state = State::HeaderAlmostDone;
                    return Ok(());
                }
                if ch == b'\n' {
                    self.callback(events, marks, Mark::HeaderValue, data, at)?;
                    self.header_done();
                    return Ok(());
                }
                self.header_value_byte(ch)?;
            }
            State::HeaderAlmostDone => self.header_done(),
            State::HeadersAlmostDone => return self.headers_done(events),
            State::BodyIdentity => {
                let left = usize::try_from(self.content_length).unwrap_or(0);
                let take = (data.len() - at).min(left);
                if take > 0 {
                    events.body(&data[at..at + take]);
                    *p = at + take - 1;
                    self.content_length -= i64::try_from(take).unwrap_or(0);
                    if self.content_length == 0 {
                        self.message_complete(events)?;
                    }
                }
            }
            State::ChunkSizeStart => {
                let Some(digit) = unhex(ch) else {
                    return self.error();
                };
                self.content_length = digit;
                self.state = State::ChunkSize;
            }
            State::ChunkSize => {
                if ch == b'\r' {
                    self.state = State::ChunkSizeAlmostDone;
                    return Ok(());
                }
                match unhex(ch) {
                    Some(digit) => {
                        self.content_length = self.content_length.wrapping_mul(16) + digit;
                    }
                    None if ch == b';' || ch == b' ' => self.state = State::ChunkParameters,
                    None => return self.error(),
                }
            }
            State::ChunkParameters => {
                if ch == b'\r' {
                    self.state = State::ChunkSizeAlmostDone;
                }
            }
            State::ChunkSizeAlmostDone => {
                if self.content_length == 0 {
                    self.flags |= F_TRAILING;
                    self.state = State::HeaderFieldStart;
                } else {
                    self.state = State::ChunkData;
                }
            }
            State::ChunkData => {
                let left = usize::try_from(self.content_length).unwrap_or(0);
                let take = (data.len() - at).min(left);
                if take > 0 {
                    events.body(&data[at..at + take]);
                    *p = at + take - 1;
                }
                if take == left {
                    self.state = State::ChunkDataAlmostDone;
                }
                self.content_length -= i64::try_from(take).unwrap_or(0);
            }
            State::ChunkDataAlmostDone => self.state = State::ChunkDataDone,
            State::ChunkDataDone => self.state = State::ChunkSizeStart,
        }
        Ok(())
    }

    /// `s_req_method`: the method narrowed as each byte of it comes.
    fn method_byte(&mut self, ch: u8) -> Result<(), Stopped> {
        if ch == 0 {
            return self.error();
        }
        let matcher = self.method.name().as_bytes();
        let expected = matcher.get(usize::from(self.index)).copied().unwrap_or(0);
        let index = self.index;
        if ch == b' ' {
            if self.method != Verb::NotImplemented && expected != 0 {
                self.method = Verb::NotImplemented;
            }
            self.state = State::ReqSpacesBeforeUrl;
        } else if self.method == Verb::NotImplemented || ch == expected {
        } else {
            self.method = match (self.method, index, ch) {
                (Verb::Connect, 1, b'H') => Verb::Checkout,
                (Verb::Connect, 2, b'P') => Verb::Copy,
                (Verb::Mkcol, 1, b'O') => Verb::Move,
                (Verb::Mkcol, 3, b'A') => Verb::Mkcalendar,
                (Verb::Mkcol, 1, b'E') => Verb::Merge,
                (Verb::Mkcol, 1, b'-') => Verb::Msearch,
                (Verb::Mkcol, 2, b'A') => Verb::Mkactivity,
                (Verb::Post, 1, b'R') => Verb::Propfind,
                (Verb::Post, 1, b'U') => Verb::Put,
                (Verb::Post, 1, b'A') => Verb::Patch,
                (Verb::Subscribe, 1, b'E') => Verb::Search,
                (Verb::Unlock, 2, b'S') => Verb::Unsubscribe,
                (Verb::Propfind, 4, b'P') => Verb::Proppatch,
                _ => Verb::NotImplemented,
            };
        }
        self.index = self.index.wrapping_add(1);
        Ok(())
    }

    /// A digit of the version, `http_major` or `http_minor`, up to 999.
    fn version_digit(&mut self, had: u16, ch: u8) -> Result<u16, Stopped> {
        if !ch.is_ascii_digit() {
            self.error()?;
        }
        let next = had * 10 + u16::from(ch - b'0');
        if next > 999 {
            self.error()?;
        }
        Ok(next)
    }

    /// The request line's end after the URL: `HTTP/0.9` where it ends at
    /// the line's end, the version to come after a space.
    fn end_of_url(&mut self, ch: u8) {
        match ch {
            b' ' => self.state = State::ReqHttpStart,
            b'\r' => {
                self.major = 0;
                self.minor = 9;
                self.state = State::ReqLineAlmostDone;
            }
            _ => {
                self.major = 0;
                self.minor = 9;
                self.state = State::HeaderFieldStart;
            }
        }
    }

    /// A byte of a header's name matched against the names the parser
    /// acts on.
    fn header_name_byte(&mut self, c: u8, ch: u8) {
        let follow = |name: &[u8], matched: Header, header: &mut Header, index: &mut u8| {
            *index = index.wrapping_add(1);
            let at = usize::from(*index);
            if at > name.len() || name.get(at) != Some(&c) {
                *header = Header::General;
            } else if at == name.len() - 1 {
                *header = matched;
            }
        };
        let (mut header, mut index) = (self.header, self.index);
        match header {
            Header::General => {}
            Header::C => {
                index = index.wrapping_add(1);
                header = if c == b'o' {
                    Header::Co
                } else {
                    Header::General
                };
            }
            Header::Co => {
                index = index.wrapping_add(1);
                header = if c == b'n' {
                    Header::Con
                } else {
                    Header::General
                };
            }
            Header::Con => {
                index = index.wrapping_add(1);
                header = match c {
                    b'n' => Header::MatchingConnection,
                    b't' => Header::MatchingContentLength,
                    _ => Header::General,
                };
            }
            Header::MatchingConnection => {
                follow(b"connection", Header::Connection, &mut header, &mut index);
            }
            Header::MatchingProxyConnection => {
                follow(
                    b"proxy-connection",
                    Header::Connection,
                    &mut header,
                    &mut index,
                );
            }
            Header::MatchingContentLength => {
                follow(
                    b"content-length",
                    Header::ContentLength,
                    &mut header,
                    &mut index,
                );
            }
            Header::MatchingTransferEncoding => follow(
                b"transfer-encoding",
                Header::TransferEncoding,
                &mut header,
                &mut index,
            ),
            Header::MatchingUpgrade => {
                follow(b"upgrade", Header::Upgrade, &mut header, &mut index);
            }
            Header::Connection
            | Header::ContentLength
            | Header::TransferEncoding
            | Header::Upgrade => {
                if ch != b' ' {
                    header = Header::General;
                }
            }
            _ => header = Header::General,
        }
        self.header = header;
        self.index = index;
    }

    /// A byte of a header's value: `Content-Length`'s digits, and the
    /// three values the parser acts on matched.
    fn header_value_byte(&mut self, ch: u8) -> Result<(), Stopped> {
        let c = ch | 0x20;
        let follow = |name: &[u8], matched: Header, parser: &mut Parser| {
            parser.index = parser.index.wrapping_add(1);
            let at = usize::from(parser.index);
            if at > name.len() || name.get(at) != Some(&c) {
                parser.header = Header::General;
            } else if at == name.len() - 1 {
                parser.header = matched;
            }
        };
        match self.header {
            Header::General => {}
            Header::ContentLength => {
                if ch == b' ' {
                    return Ok(());
                }
                if !ch.is_ascii_digit() {
                    return self.error();
                }
                self.content_length = self
                    .content_length
                    .wrapping_mul(10)
                    .wrapping_add(i64::from(ch - b'0'));
            }
            Header::MatchingTransferEncodingChunked => {
                follow(b"chunked", Header::TransferEncodingChunked, self);
            }
            Header::MatchingConnectionKeepAlive => {
                follow(b"keep-alive", Header::ConnectionKeepAlive, self);
            }
            Header::MatchingConnectionClose => {
                follow(b"close", Header::ConnectionClose, self);
            }
            Header::TransferEncodingChunked
            | Header::ConnectionKeepAlive
            | Header::ConnectionClose => {
                if ch != b' ' {
                    self.header = Header::General;
                }
            }
            _ => {
                self.state = State::HeaderValue;
                self.header = Header::General;
            }
        }
        Ok(())
    }

    /// `s_header_almost_done`: the header's line is over, and what its
    /// value matched is set among the parser's `flags`.
    fn header_done(&mut self) {
        self.state = State::HeaderFieldStart;
        match self.header {
            Header::ConnectionKeepAlive => self.flags |= F_CONNECTION_KEEP_ALIVE,
            Header::ConnectionClose => self.flags |= F_CONNECTION_CLOSE,
            Header::TransferEncodingChunked => self.flags |= F_CHUNKED,
            _ => {}
        }
    }

    /// `s_headers_almost_done`: the head is over; a chunked body's
    /// trailer ends the message, and otherwise `on_headers_complete` and
    /// then the body as the head framed it, a request with no length
    /// having none.
    fn headers_done(&mut self, events: &mut dyn Events) -> Result<(), Stopped> {
        if self.flags & F_TRAILING != 0 {
            return self.message_complete(events);
        }
        self.nread = 0;
        if !events.headers_complete() {
            return Err(Stopped);
        }
        if self.flags & F_CHUNKED != 0 {
            self.state = State::ChunkSizeStart;
        } else if self.content_length > 0 {
            self.state = State::BodyIdentity;
        } else {
            self.message_complete(events)?;
        }
        Ok(())
    }

    /// `on_message_complete`, and the next message's start.
    fn message_complete(&mut self, events: &mut dyn Events) -> Result<(), Stopped> {
        if !events.message_complete(self.major, self.minor) {
            return Err(Stopped);
        }
        self.state = State::StartReq;
        Ok(())
    }

    /// `CALLBACK(FOR)`: the bytes from the mark to here, and the mark
    /// cleared.
    fn callback(
        &self,
        events: &mut dyn Events,
        marks: &mut Marks,
        which: Mark,
        data: &[u8],
        here: usize,
    ) -> Result<(), Stopped> {
        let slot = match which {
            Mark::Url => &mut marks.url,
            Mark::Path => &mut marks.path,
            Mark::QueryString => &mut marks.query_string,
            Mark::HeaderField => &mut marks.header_field,
            Mark::HeaderValue => &mut marks.header_value,
        };
        let Some(from) = slot.take() else {
            return Ok(());
        };
        let bytes = &data[from..here];
        let went_on = match which {
            Mark::Url => events.url(Method(self.method), bytes),
            Mark::Path => events.path(bytes),
            Mark::QueryString => events.query_string(bytes),
            Mark::HeaderField => events.header_field(bytes),
            Mark::HeaderValue => events.header_value(bytes),
        };
        if went_on { Ok(()) } else { Err(Stopped) }
    }

    /// `goto error`: the parser dead from here.
    fn error<T>(&mut self) -> Result<T, Stopped> {
        self.state = State::Dead;
        Err(Stopped)
    }
}

/// Which mark a callback reads.
#[derive(Clone, Copy)]
enum Mark {
    Url,
    Path,
    QueryString,
    HeaderField,
    HeaderValue,
}

/// `tokens`: a header name's byte lowered, or 0 for one no name holds.
fn token(ch: u8) -> u8 {
    match ch {
        b'A'..=b'Z' => ch | 0x20,
        b' '
        | b'!'
        | b'"'
        | b'#'
        | b'$'
        | b'%'
        | b'&'
        | b'\''
        | b'*'
        | b'+'
        | b'-'
        | b'.'
        | b'/'
        | b'0'..=b'9'
        | b'^'
        | b'_'
        | b'`'
        | b'a'..=b'z'
        | b'|'
        | b'}'
        | b'~' => ch,
        _ => 0,
    }
}

/// `normal_url_char`: a byte a path, a query string or a fragment runs on
/// over, which is every printable byte but `#` and `?`.
fn normal_url_char(ch: u8) -> bool {
    (0x21..0x7f).contains(&ch) && ch != b'#' && ch != b'?'
}

/// `unhex`: a hexadecimal digit's value.
fn unhex(ch: u8) -> Option<i64> {
    char::from(ch).to_digit(16).map(i64::from)
}

/// `http_status_map` (`Z/main/http_status_codes.h`): the reason phrase
/// php knows a code by.
#[must_use]
pub fn reason(code: i64) -> Option<&'static str> {
    const MAP: [(i64, &str); 48] = [
        (100, "Continue"),
        (101, "Switching Protocols"),
        (200, "OK"),
        (201, "Created"),
        (202, "Accepted"),
        (203, "Non-Authoritative Information"),
        (204, "No Content"),
        (205, "Reset Content"),
        (206, "Partial Content"),
        (300, "Multiple Choices"),
        (301, "Moved Permanently"),
        (302, "Found"),
        (303, "See Other"),
        (304, "Not Modified"),
        (305, "Use Proxy"),
        (307, "Temporary Redirect"),
        (308, "Permanent Redirect"),
        (400, "Bad Request"),
        (401, "Unauthorized"),
        (402, "Payment Required"),
        (403, "Forbidden"),
        (404, "Not Found"),
        (405, "Method Not Allowed"),
        (406, "Not Acceptable"),
        (407, "Proxy Authentication Required"),
        (408, "Request Timeout"),
        (409, "Conflict"),
        (410, "Gone"),
        (411, "Length Required"),
        (412, "Precondition Failed"),
        (413, "Request Entity Too Large"),
        (414, "Request-URI Too Long"),
        (415, "Unsupported Media Type"),
        (416, "Requested Range Not Satisfiable"),
        (417, "Expectation Failed"),
        (426, "Upgrade Required"),
        (428, "Precondition Required"),
        (429, "Too Many Requests"),
        (431, "Request Header Fields Too Large"),
        (451, "Unavailable For Legal Reasons"),
        (500, "Internal Server Error"),
        (501, "Not Implemented"),
        (502, "Bad Gateway"),
        (503, "Service Unavailable"),
        (504, "Gateway Timeout"),
        (505, "HTTP Version Not Supported"),
        (506, "Variant Also Negotiates"),
        (511, "Network Authentication Required"),
    ];
    MAP.iter()
        .find(|(had, _)| *had == code)
        .map(|(_, text)| *text)
}

/// `normalize_vpath` (`Z/sapi/cli/php_cli_server.c:1538`): the path
/// percent-decoded as `php_raw_url_decode` decodes it, leading slashes
/// one slash, `.` segments dropped, each `..` taking the segment before
/// it, and the slashes after each segment one slash.
#[must_use]
pub fn normalize_vpath(vpath: &[u8]) -> Vec<u8> {
    let mut path = raw_url_decode(vpath);
    let mut p = 0;
    if path.first() == Some(&b'/') {
        let n = path.iter().take_while(|&&b| b == b'/').count();
        p = 1;
        path.drain(p..n);
    }
    while p < path.len() {
        let n = p + path[p..].iter().take_while(|&&b| b != b'/').count();
        let segment = &path[p..n];
        if segment == b".." {
            if p > 0 {
                p -= 1;
                loop {
                    if p == 0 {
                        if path[p] == b'/' {
                            p += 1;
                        }
                        break;
                    }
                    p -= 1;
                    if path[p] == b'/' {
                        p += 1;
                        break;
                    }
                }
            }
            let after = n + path[n..].iter().take_while(|&&b| b == b'/').count();
            path.drain(p..after);
        } else if segment == b"." {
            let after = n + path[n..].iter().take_while(|&&b| b == b'/').count();
            path.drain(p..after);
        } else if n < path.len() {
            let after = n + path[n..].iter().take_while(|&&b| b == b'/').count();
            path.drain(n + 1..after);
            p = n + 1;
        } else {
            p = n;
        }
    }
    path
}

/// `php_raw_url_decode`: each `%` with two hex digits after it the byte
/// they spell; `+` stays.
#[must_use]
pub fn raw_url_decode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|&b| unhex(b)),
                bytes.get(i + 2).and_then(|&b| unhex(b)),
            )
        {
            out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// A moment in UTC as the calendar reads it, Howard Hinnant's
/// `civil_from_days` over the Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 for Sunday to 6 for Saturday.
    pub weekday: u32,
}

/// `seconds` since the Unix epoch as the calendar reads them in UTC.
#[must_use]
pub fn civil(seconds: u64) -> Civil {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let clock = u32::try_from(seconds % 86_400).unwrap_or(0);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    Civil {
        year: year_of_era + era * 400 + i64::from(month <= 2),
        month: u32::try_from(month).unwrap_or(1),
        day: u32::try_from(day).unwrap_or(1),
        hour: clock / 3_600,
        minute: clock % 3_600 / 60,
        second: clock % 60,
        weekday: u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0),
    }
}

/// The three-letter month names, January first.
pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `php_format_date("D, d M Y H:i:s", seconds, 0)`, what the built-in
/// server's `Date` header carries before ` GMT`.
#[must_use]
pub fn date(seconds: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let at = civil(seconds);
    let index = |n: u32| usize::try_from(n).unwrap_or(0);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02}",
        DAYS.get(index(at.weekday)).copied().unwrap_or("Sun"),
        at.day,
        MONTHS.get(index(at.month) - 1).copied().unwrap_or("Jan"),
        at.year,
        at.hour,
        at.minute,
        at.second
    )
}

/// What an `Authorization` header carries, as `php_handle_auth_data`
/// reads it into `SG(request_info)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    /// `Basic`: the user before the first `:` of the decoded credentials,
    /// and the password after it where it is not empty.
    Basic {
        user: Vec<u8>,
        password: Option<Vec<u8>>,
    },
    /// `Digest`: what follows the word, `auth_digest`.
    Digest(Vec<u8>),
}

/// `php_handle_auth_data` (`Z/main/main.c:2735`): `Basic` credentials
/// where they decode and hold a `:`, else `Digest`'s rest, else none.
#[must_use]
pub fn credentials(auth: &[u8]) -> Option<Credentials> {
    if auth.len() >= 6
        && auth[..6].eq_ignore_ascii_case(b"Basic ")
        && let Some(decoded) = base64(&auth[6..])
    {
        let decoded = crate::c_str(&decoded);
        if let Some(colon) = decoded.iter().position(|b| *b == b':') {
            let password = &decoded[colon + 1..];
            return Some(Credentials::Basic {
                user: decoded[..colon].to_vec(),
                password: (!password.is_empty()).then(|| password.to_vec()),
            });
        }
    }
    (auth.len() >= 7 && auth[..7].eq_ignore_ascii_case(b"Digest "))
        .then(|| Credentials::Digest(auth[7..].to_vec()))
}

/// `php_base64_decode`, the lenient one: bytes outside the alphabet
/// skipped, and `None` for a lone digit left over.
fn base64(input: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    let mut digits = 0;
    for c in input {
        if *c == b'=' {
            break;
        }
        let Some(v) = value(*c) else { continue };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        digits += 1;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).unwrap_or(0));
        }
    }
    (digits % 4 != 1).then_some(out)
}

#[cfg(test)]
mod tests;
