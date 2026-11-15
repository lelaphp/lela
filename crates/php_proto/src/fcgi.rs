//! The `FastCGI` record protocol as php's `main/fastcgi.c` speaks it on the
//! application side: a record's 8-byte header decoded once into a
//! [`Header`] of a [`Kind`], a request read as `fcgi_read_request` reads
//! it ([`read_request`]), its body read as `fcgi_read` reads the
//! `FCGI_STDIN` records ([`Input`]), and what the request writes framed as
//! `fcgi_write` frames it, into one buffer of `out_buf`'s size, each
//! record padded to 8 bytes ([`Outgoing`]). Bytes and `std::io` streams
//! alone, no socket and no engine, so it tests in process.

use std::io::{self, Write};

use crate::cgi::Environment;

/// `FCGI_VERSION_1`.
pub const VERSION_1: u8 = 1;

/// `FCGI_MAX_LENGTH`: the most a record's content and padding hold.
pub const MAX_LENGTH: usize = 0xffff;

/// The size of `fcgi_request.out_buf`, what `fcgi_write` gathers records
/// in before one write.
const OUT_BUF: usize = 1024 * 8;

/// A record's type, `fcgi_request_type`, decoded once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    BeginRequest,
    AbortRequest,
    EndRequest,
    Params,
    Stdin,
    Stdout,
    Stderr,
    Data,
    GetValues,
    GetValuesResult,
    UnknownType,
    /// A type the specification does not name.
    Other(u8),
}

impl Kind {
    /// The type byte read.
    #[must_use]
    pub fn of(byte: u8) -> Kind {
        match byte {
            1 => Kind::BeginRequest,
            2 => Kind::AbortRequest,
            3 => Kind::EndRequest,
            4 => Kind::Params,
            5 => Kind::Stdin,
            6 => Kind::Stdout,
            7 => Kind::Stderr,
            8 => Kind::Data,
            9 => Kind::GetValues,
            10 => Kind::GetValuesResult,
            11 => Kind::UnknownType,
            other => Kind::Other(other),
        }
    }

    /// The type byte written.
    #[must_use]
    pub fn byte(self) -> u8 {
        match self {
            Kind::BeginRequest => 1,
            Kind::AbortRequest => 2,
            Kind::EndRequest => 3,
            Kind::Params => 4,
            Kind::Stdin => 5,
            Kind::Stdout => 6,
            Kind::Stderr => 7,
            Kind::Data => 8,
            Kind::GetValues => 9,
            Kind::GetValuesResult => 10,
            Kind::UnknownType => 11,
            Kind::Other(byte) => byte,
        }
    }
}

/// `fcgi_header`, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub kind: Kind,
    pub request_id: u16,
    pub content_length: u16,
    pub padding_length: u8,
}

impl Header {
    /// The header `bytes` hold.
    #[must_use]
    pub fn decode(bytes: [u8; 8]) -> Header {
        Header {
            version: bytes[0],
            kind: Kind::of(bytes[1]),
            request_id: u16::from_be_bytes([bytes[2], bytes[3]]),
            content_length: u16::from_be_bytes([bytes[4], bytes[5]]),
            padding_length: bytes[6],
        }
    }

    /// `fcgi_make_header`: the header of a record of `len` bytes, its
    /// padding what rounds the record up to a multiple of 8.
    #[must_use]
    pub fn make(kind: Kind, request_id: u16, len: usize) -> Header {
        let len = u16::try_from(len).unwrap_or(u16::MAX);
        let padding = (len.wrapping_add(7) & !7).wrapping_sub(len);
        Header {
            version: VERSION_1,
            kind,
            request_id,
            content_length: len,
            padding_length: u8::try_from(padding).unwrap_or(0),
        }
    }

    /// The eight bytes.
    #[must_use]
    pub fn encode(self) -> [u8; 8] {
        let id = self.request_id.to_be_bytes();
        let len = self.content_length.to_be_bytes();
        [
            self.version,
            self.kind.byte(),
            id[0],
            id[1],
            len[0],
            len[1],
            self.padding_length,
            0,
        ]
    }

    /// Content and padding, what follows the header.
    fn body_len(self) -> usize {
        usize::from(self.content_length) + usize::from(self.padding_length)
    }
}

/// The role a `FCGI_BEGIN_REQUEST` names, as `FCGI_ROLE` carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Responder,
    Authorizer,
    Filter,
}

impl Role {
    /// What `FCGI_ROLE` holds.
    #[must_use]
    pub fn name(self) -> &'static [u8] {
        match self {
            Role::Responder => b"RESPONDER",
            Role::Authorizer => b"AUTHORIZER",
            Role::Filter => b"FILTER",
        }
    }
}

/// `fcgi_hash_apply`'s walk over a request's variables, which `$_SERVER`
/// and `apache_request_headers` take: the newest first, as `fcgi_hash`
/// links each new name at the head of its list.
pub fn newest_first(params: &Environment) -> impl Iterator<Item = &(Vec<u8>, Vec<u8>)> {
    params.0.iter().rev()
}

/// `fcgi_get_params`: the name-value pairs of `data` set into `params`,
/// each length one byte under 128 and four with the high bit masked
/// otherwise; `false` for a pair that runs past the end, the pairs before
/// it kept.
pub fn get_params(params: &mut Environment, data: &[u8]) -> bool {
    let mut at = 0;
    while at < data.len() {
        let Some(name_len) = take_len(data, &mut at) else {
            return false;
        };
        if at >= data.len() {
            return false;
        }
        let Some(value_len) = take_len(data, &mut at) else {
            return false;
        };
        if name_len.saturating_add(value_len) > data.len() - at {
            return false;
        }
        let name = &data[at..at + name_len];
        let value = &data[at + name_len..at + name_len + value_len];
        params.put(name, Some(value));
        at += name_len + value_len;
    }
    true
}

/// One length of a pair, from `data[*at..]`.
fn take_len(data: &[u8], at: &mut usize) -> Option<usize> {
    let first = *data.get(*at)?;
    *at += 1;
    if first < 128 {
        return Some(usize::from(first));
    }
    if *at + 3 > data.len() {
        return None;
    }
    let len = u32::from_be_bytes([first & 0x7f, data[*at], data[*at + 1], data[*at + 2]]);
    *at += 3;
    usize::try_from(len).ok()
}

/// One name-value pair appended as `fcgi_read_request` writes the answer
/// to `FCGI_GET_VALUES`, and as a client writes `FCGI_PARAMS`.
pub fn put_pair(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    for len in [name.len(), value.len()] {
        match u8::try_from(len) {
            Ok(short) if short < 0x80 => out.push(short),
            _ => {
                let long = u32::try_from(len).unwrap_or(u32::MAX) | 0x8000_0000;
                out.extend_from_slice(&long.to_be_bytes());
            }
        }
    }
    out.extend_from_slice(name);
    out.extend_from_slice(value);
}

/// A request `fcgi_read_request` read up to its body: its id, whether
/// the connection stays open after it, its role, and its variables with
/// `FCGI_ROLE` among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Begun {
    pub id: u16,
    pub keep: bool,
    pub role: Role,
    pub params: Environment,
}

/// What `fcgi_read_request` came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// A request, its variables read (1).
    Request(Begun),
    /// A `FCGI_GET_VALUES` answered (2).
    Answered,
    /// Nothing to serve (0): the stream ended, or a record was malformed
    /// or of a type that opens nothing; the connection is closed.
    Refused,
}

/// `safe_read` of a whole header, `None` where the stream ends first or
/// the version is below `FCGI_VERSION_1`.
fn read_header(stream: &mut impl io::Read) -> Option<Header> {
    let mut bytes = [0u8; 8];
    stream.read_exact(&mut bytes).ok()?;
    let header = Header::decode(bytes);
    (header.version >= VERSION_1).then_some(header)
}

/// `safe_read` of `len` bytes.
fn read_body(stream: &mut impl io::Read, len: usize) -> Option<Vec<u8>> {
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(body)
}

/// `fcgi_read_request` (`Z/main/fastcgi.c:1036`) over `stream`: empty
/// `FCGI_STDIN` records before the request passed over; a
/// `FCGI_BEGIN_REQUEST` of eight bytes with a role php knows, then each
/// non-empty `FCGI_PARAMS` record's pairs until the empty one; or a
/// `FCGI_GET_VALUES`, whose names found in `values` are answered, newest
/// first, in one `FCGI_GET_VALUES_RESULT`.
pub fn read_request(stream: &mut (impl io::Read + Write), values: &[(Vec<u8>, Vec<u8>)]) -> Read {
    let Some(mut header) = read_header(stream) else {
        return Read::Refused;
    };
    while header.kind == Kind::Stdin && header.content_length == 0 {
        let Some(next) = read_header(stream) else {
            return Read::Refused;
        };
        header = next;
    }
    if header.body_len() > MAX_LENGTH {
        return Read::Refused;
    }
    match header.kind {
        Kind::BeginRequest if header.content_length == 8 => begin(stream, header),
        Kind::GetValues => answer(stream, header, values),
        _ => Read::Refused,
    }
}

/// `fcgi_read_request`'s `FCGI_BEGIN_REQUEST` half.
fn begin(stream: &mut impl io::Read, header: Header) -> Read {
    let Some(body) = read_body(stream, header.body_len()) else {
        return Read::Refused;
    };
    let role = match u16::from_be_bytes([body[0], body[1]]) {
        1 => Role::Responder,
        2 => Role::Authorizer,
        3 => Role::Filter,
        _ => return Read::Refused,
    };
    let mut params = Environment::default();
    params.put(b"FCGI_ROLE", Some(role.name()));
    let Some(mut next) = read_header(stream) else {
        return Read::Refused;
    };
    while next.kind == Kind::Params && next.content_length > 0 {
        if next.body_len() > MAX_LENGTH {
            return Read::Refused;
        }
        let Some(body) = read_body(stream, next.body_len()) else {
            return Read::Refused;
        };
        if !get_params(&mut params, &body[..usize::from(next.content_length)]) {
            return Read::Refused;
        }
        let Some(after) = read_header(stream) else {
            return Read::Refused;
        };
        next = after;
    }
    Read::Request(Begun {
        id: header.request_id,
        keep: body[2] & 1 != 0,
        role,
        params,
    })
}

/// `fcgi_read_request`'s `FCGI_GET_VALUES` half: `fcgi_mgmt_vars`'
/// answer to the names asked, newest first, while it fits one record.
fn answer(
    stream: &mut (impl io::Read + Write),
    header: Header,
    values: &[(Vec<u8>, Vec<u8>)],
) -> Read {
    let Some(body) = read_body(stream, header.body_len()) else {
        return Read::Refused;
    };
    let mut asked = Environment::default();
    if !get_params(&mut asked, &body[..usize::from(header.content_length)]) {
        return Read::Refused;
    }
    let mut pairs = Vec::new();
    for (name, _) in newest_first(&asked) {
        if let Some((_, value)) = values.iter().find(|(known, _)| known == name) {
            if 8 + pairs.len() + 8 + name.len() + value.len() >= MAX_LENGTH + 8 {
                break;
            }
            put_pair(&mut pairs, name, value);
        }
    }
    let header = Header::make(Kind::GetValuesResult, 0, pairs.len());
    let mut record = header.encode().to_vec();
    record.extend_from_slice(&pairs);
    record.resize(record.len() + usize::from(header.padding_length), 0);
    if stream.write_all(&record).is_err() {
        return Read::Refused;
    }
    Read::Answered
}

/// `fcgi_read`'s state: what is left of the `FCGI_STDIN` record being
/// read, its padding, and whether the empty record that ends the body has
/// been read.
#[derive(Debug, Default)]
pub struct Input {
    left: usize,
    padding: usize,
    ended: bool,
}

impl Input {
    /// `fcgi_read` (`Z/main/fastcgi.c:1181`): up to `buf.len()` bytes of
    /// the body, reading the next `FCGI_STDIN` record where the last is
    /// used up; fewer where the stream ends, or an empty record ends the
    /// body, and nothing past that record. php's `fcgi_read` would read the
    /// next record's header there, where no length bounds the body;
    /// the end is the end.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidData`] for a record that is not
    /// `FCGI_STDIN`, after which the connection does not stay, and the
    /// stream's own errors.
    pub fn read(&mut self, stream: &mut impl io::Read, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() && !self.ended {
            if self.left == 0 {
                match read_header(stream) {
                    Some(header) if header.kind == Kind::Stdin => {
                        self.left = usize::from(header.content_length);
                        self.padding = usize::from(header.padding_length);
                        if self.left == 0 {
                            self.ended = true;
                            return Ok(n);
                        }
                    }
                    _ => return Err(io::ErrorKind::InvalidData.into()),
                }
            }
            let want = self.left.min(buf.len() - n);
            let got = stream.read(&mut buf[n..n + want])?;
            if got == 0 {
                return Ok(n);
            }
            self.left -= got;
            n += got;
            if self.left > 0 {
                return Ok(n);
            }
            if self.padding > 0 {
                let mut pad = vec![0u8; self.padding];
                stream.read_exact(&mut pad)?;
                self.padding = 0;
            }
        }
        Ok(n)
    }
}

/// A request's body as a `FastCGI` host's `read_post` hands it out
/// (`sapi_fcgi_read_post`, `Z/sapi/cgi/cgi_main.c:504`, and php-fpm's
/// `sapi_cgi_read_post`): its `FCGI_STDIN` records ([`Input`]) and what
/// the request's length leaves of them
/// ([`RequestInfo::body_length`](crate::cgi::RequestInfo::body_length)).
#[derive(Debug, Default)]
pub struct Stdin {
    input: Input,
    unread: u64,
}

/// What one [`Stdin::read`] handed out, and whether the stream failed,
/// after which the connection is not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handed {
    pub n: usize,
    pub failed: bool,
}

impl Stdin {
    /// The body of a request `body_length` long.
    #[must_use]
    pub fn new(body_length: u64) -> Stdin {
        Stdin {
            input: Input::default(),
            unread: body_length,
        }
    }

    /// Up to `buf.len()` bytes of the body, no more than the length leaves,
    /// read until the buffer is full, the records end or the stream fails,
    /// as the hosts' loops over `fcgi_read` read them.
    pub fn read(&mut self, stream: &mut impl io::Read, buf: &mut [u8]) -> Handed {
        let want = usize::try_from(self.unread).map_or(buf.len(), |unread| unread.min(buf.len()));
        let mut read = Handed {
            n: 0,
            failed: false,
        };
        while read.n < want {
            match self.input.read(stream, &mut buf[read.n..want]) {
                Ok(0) => break,
                Ok(n) => read.n += n,
                Err(_) => {
                    read.failed = true;
                    break;
                }
            }
        }
        self.unread = self
            .unread
            .saturating_sub(u64::try_from(read.n).unwrap_or(u64::MAX));
        read
    }
}

/// `fcgi_request`'s half that writes: the buffer records are gathered in, the
/// open record's header position and type, and whether
/// `FCGI_END_REQUEST` went.
#[derive(Debug)]
pub struct Outgoing {
    id: u16,
    buf: Vec<u8>,
    open: Option<(usize, Kind)>,
    ended: bool,
}

impl Outgoing {
    /// What request `id` sends.
    #[must_use]
    pub fn new(id: u16) -> Outgoing {
        Outgoing {
            id,
            buf: Vec::with_capacity(OUT_BUF),
            open: None,
            ended: false,
        }
    }

    /// `open_packet`: a header's room at the buffer's end.
    fn open_packet(&mut self, kind: Kind) {
        self.open = Some((self.buf.len(), kind));
        self.buf.extend_from_slice(&[0; 8]);
    }

    /// `close_packet`: the open record's header written over its room,
    /// and its padding after its content.
    fn close_packet(&mut self) {
        if let Some((at, kind)) = self.open.take() {
            let len = self.buf.len() - at - 8;
            let header = Header::make(kind, self.id, len);
            self.buf[at..at + 8].copy_from_slice(&header.encode());
            self.buf
                .resize(self.buf.len() + usize::from(header.padding_length), 0);
        }
    }

    /// `fcgi_flush`: the open record closed and the buffer written, with
    /// `FCGI_END_REQUEST` (status 0, `FCGI_REQUEST_COMPLETE`) after it
    /// where `end` says.
    ///
    /// # Errors
    ///
    /// The stream's; the buffer is emptied whatever it wrote.
    pub fn flush(&mut self, stream: &mut impl Write, end: bool) -> io::Result<()> {
        self.close_packet();
        if end {
            let header = Header::make(Kind::EndRequest, self.id, 8);
            self.buf.extend_from_slice(&header.encode());
            self.buf.extend_from_slice(&[0; 8]);
        }
        let written = stream.write_all(&self.buf);
        self.buf.clear();
        written
    }

    /// `fcgi_end`: `FCGI_END_REQUEST` flushed, once.
    ///
    /// # Errors
    ///
    /// The stream's.
    pub fn end(&mut self, stream: &mut impl Write) -> io::Result<()> {
        if self.ended {
            return Ok(());
        }
        self.ended = true;
        self.flush(stream, true)
    }

    /// Whether `FCGI_END_REQUEST` went.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// `fcgi_write` (`Z/main/fastcgi.c:1540`): `bytes` as records of
    /// `kind`, gathered in the buffer and written as it fills, a write
    /// longer than a buffer sent in records of `0xfff8` bytes.
    ///
    /// # Errors
    ///
    /// The stream's.
    pub fn write(&mut self, stream: &mut impl Write, kind: Kind, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.open.is_some_and(|(_, open)| open != kind) {
            self.close_packet();
        }
        let len = bytes.len();
        let mut limit = OUT_BUF.saturating_sub(self.buf.len());
        if self.open.is_none() {
            limit = limit.saturating_sub(8);
        }
        if len < limit {
            if self.open.is_none() {
                self.open_packet(kind);
            }
            self.buf.extend_from_slice(bytes);
        } else if len - limit < OUT_BUF - 8 {
            if limit > 0 {
                if self.open.is_none() {
                    self.open_packet(kind);
                }
                self.buf.extend_from_slice(&bytes[..limit]);
            }
            self.flush(stream, false)?;
            if len > limit {
                self.open_packet(kind);
                self.buf.extend_from_slice(&bytes[limit..]);
            }
        } else {
            self.close_packet();
            let mut pos = 0;
            while len - pos > 0xffff {
                self.buf
                    .extend_from_slice(&Header::make(kind, self.id, 0xfff8).encode());
                self.flush(stream, false)?;
                stream.write_all(&bytes[pos..pos + 0xfff8])?;
                pos += 0xfff8;
            }
            let pad = ((len - pos + 7) & !7) - (len - pos);
            let rest = if pad > 0 { 8 - pad } else { 0 };
            self.buf
                .extend_from_slice(&Header::make(kind, self.id, len - pos - rest).encode());
            self.flush(stream, false)?;
            stream.write_all(&bytes[pos..len - rest])?;
            if pad > 0 {
                self.open_packet(kind);
                self.buf.extend_from_slice(&bytes[len - rest..]);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
