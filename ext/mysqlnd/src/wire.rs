//! `mysqlnd_wireprotocol.c`: the packets, each read as php reads it, with
//! php's warnings where one is malformed. What a packet's body says is
//! decoded once, into the type here, from the bytes the frame codec
//! handed over; `mysql_common` serves the scrambles (`auth`).

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;

use crate::conn::ConnData;
use crate::consts::{ConnState, NULL_LENGTH, UNKNOWN_SQLSTATE, cr};
use crate::error::{ErrorInfo, SERVER_GONE};
use crate::net::HEADER_SIZE;
use crate::stats::Stat;

/// `ERROR_MARKER`.
pub(crate) const ERROR_MARKER: u8 = 0xFF;
/// `EODATA_MARKER`.
pub(crate) const EODATA_MARKER: u8 = 0xFE;

/// `enum mysqlnd_packet_type`, as far as it picks the statistics a read
/// counts under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prot {
    Greet,
    Ok,
    Eof,
    RsetHeader,
    RsetField,
    PrepareResponse,
    ChangeUser,
    Stats,
    Sha256PkResponse,
    CachedSha2,
}

impl Prot {
    /// `packet_type_to_statistic_byte_count` and `..._packet_count`.
    fn stats(self) -> Option<(Stat, Stat)> {
        Some(match self {
            Prot::Ok => (Stat::BytesReceivedOk, Stat::PacketsReceivedOk),
            Prot::Eof => (Stat::BytesReceivedEof, Stat::PacketsReceivedEof),
            Prot::RsetHeader => (
                Stat::BytesReceivedRsetHeader,
                Stat::PacketsReceivedRsetHeader,
            ),
            Prot::RsetField => (
                Stat::BytesReceivedRsetFieldMeta,
                Stat::PacketsReceivedRsetFieldMeta,
            ),
            Prot::PrepareResponse => (
                Stat::BytesReceivedPrepareResponse,
                Stat::PacketsReceivedPrepareResponse,
            ),
            Prot::ChangeUser => (
                Stat::BytesReceivedChangeUser,
                Stat::PacketsReceivedChangeUser,
            ),
            _ => return None,
        })
    }

    /// The name php's premature-end warning gives the packet.
    fn name(self) -> &'static str {
        match self {
            Prot::Greet => "GREET",
            Prot::Ok => "OK",
            Prot::Eof => "EOF",
            Prot::RsetHeader => "RSET_HEADER",
            Prot::PrepareResponse => "PREPARE",
            Prot::ChangeUser => "CHANGE_USER",
            Prot::Sha256PkResponse | Prot::CachedSha2 => "SHA256_PK_REQUEST_RESPONSE",
            _ => "",
        }
    }
}

/// A body read past its end: how many bytes short it was, for php's
/// "%s packet %zu bytes shorter than expected".
pub(crate) struct Short(usize);

/// A cursor over one packet's body, php's `p` against `begin` and the
/// header's size.
pub(crate) struct Body<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Body<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Body<'a> {
        Body { bytes, at: 0 }
    }

    /// `BAIL_IF_NO_MORE_DATA`: whether the cursor stands past the body.
    pub(crate) fn check(&self) -> Result<(), Short> {
        if self.at > self.bytes.len() {
            return Err(Short(self.at - self.bytes.len()));
        }
        Ok(())
    }

    /// php's `p - begin`.
    pub(crate) fn position(&self) -> usize {
        self.at
    }

    pub(crate) fn left(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.at..).unwrap_or_default()
    }

    pub(crate) fn skip(&mut self, n: usize) {
        self.at += n;
    }

    pub(crate) fn u8(&mut self) -> u8 {
        let b = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }

    pub(crate) fn u16(&mut self) -> u16 {
        let lo = u16::from(self.u8());
        let hi = u16::from(self.u8());
        lo | (hi << 8)
    }

    pub(crate) fn u32(&mut self) -> u32 {
        let lo = u32::from(self.u16());
        let hi = u32::from(self.u16());
        lo | (hi << 16)
    }

    pub(crate) fn take(&mut self, n: usize) -> &'a [u8] {
        let end = (self.at + n).min(self.bytes.len());
        let out = self.bytes.get(self.at.min(end)..end).unwrap_or_default();
        self.at += n;
        out
    }

    /// A NUL-terminated string and its NUL.
    pub(crate) fn cstr(&mut self) -> &'a [u8] {
        let rest = self.rest();
        let len = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        let out = &rest[..len];
        self.at += len + 1;
        out
    }

    /// `php_mysqlnd_net_field_length_ll`: a length-encoded integer,
    /// [`NULL_LENGTH`] for `251`.
    pub(crate) fn lenenc(&mut self) -> u64 {
        let first = self.u8();
        match first {
            0..=250 => u64::from(first),
            251 => NULL_LENGTH,
            252 => u64::from(self.u16()),
            253 => {
                let lo = u64::from(self.u16());
                lo | (u64::from(self.u8()) << 16)
            }
            _ => {
                let lo = u64::from(self.u32());
                lo | (u64::from(self.u32()) << 32)
            }
        }
    }

    /// `php_mysqlnd_net_field_length`: as [`Body::lenenc`], where php's
    /// `zend_ulong` value of the nine-byte form reads four bytes and
    /// skips eight.
    pub(crate) fn field_length(&mut self) -> u64 {
        match self.bytes.get(self.at) {
            Some(254) => {
                self.at += 1;
                let n = u64::from(self.u32());
                self.at += 4;
                n
            }
            _ => self.lenenc(),
        }
    }
}

/// `php_mysqlnd_net_store_length`.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_EMULATE_PREPARES => false]);
/// $st = $db->prepare('SELECT LENGTH(?), LENGTH(?), LENGTH(?)');
/// $st->execute(['x', str_repeat('y', 300), str_repeat('z', 70000)]);
/// var_dump($st->fetch(PDO::FETCH_NUM));
/// ```
/// ```expect mysql
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(300)
///   [2]=>
///   int(70000)
/// }
/// ```
pub(crate) fn store_length(out: &mut Vec<u8>, length: u64) {
    let bytes = length.to_le_bytes();
    if length < 251 {
        out.push(bytes[0]);
    } else if length < 65_536 {
        out.push(252);
        out.extend_from_slice(&bytes[..2]);
    } else if length < 16_777_216 {
        out.push(253);
        out.extend_from_slice(&bytes[..3]);
    } else {
        out.push(254);
        out.extend_from_slice(&length.to_le_bytes());
    }
}

/// `php_mysqlnd_read_error_from_line`: the number, the SQLSTATE after a
/// `#`, `HY000` without one, and the text; `CR_UNKNOWN_ERROR` for a body
/// of two bytes or fewer.
///
/// ```php
/// $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// try { $db->query('SELECT * FROM no_such_table'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; var_dump($e->errorInfo); }
/// ```
/// ```expect mysql
/// SQLSTATE[42S02]: Base table or view not found: 1146 Table 'test.no_such_table' doesn't exist
/// array(3) {
///   [0]=>
///   string(5) "42S02"
///   [1]=>
///   int(1146)
///   [2]=>
///   string(40) "Table 'test.no_such_table' doesn't exist"
/// }
/// ```
pub(crate) fn error_from_line(buf: &[u8]) -> (u32, [u8; 5], Vec<u8>) {
    let mut state = *UNKNOWN_SQLSTATE;
    if buf.len() <= 2 {
        return (cr::UNKNOWN_ERROR, state, Vec::new());
    }
    let errno = u32::from(u16::from_le_bytes([buf[0], buf[1]]));
    let mut p = 2;
    if buf.get(p) == Some(&b'#') {
        p += 1;
        match buf.get(p..p + 5) {
            Some(s) => {
                state.copy_from_slice(s);
                p += 5;
            }
            None => return (errno, state, Vec::new()),
        }
    }
    (errno, state, buf.get(p..).unwrap_or_default().to_vec())
}

/// An error packet's three parts, as php keeps them on the packet.
#[derive(Debug, Clone, Default)]
pub(crate) struct PacketError {
    pub(crate) errno: u32,
    pub(crate) sqlstate: [u8; 5],
    pub(crate) error: Vec<u8>,
}

impl PacketError {
    pub(crate) fn read_from(buf: &[u8]) -> PacketError {
        let (errno, sqlstate, error) = error_from_line(buf);
        PacketError {
            errno,
            sqlstate,
            error,
        }
    }

    /// `SET_CLIENT_ERROR(info, errno, sqlstate, error)`; the `Err` is
    /// the memory limit's fatal.
    pub(crate) fn set_on(&self, req: &mut Request, info: &mut ErrorInfo) -> Result<(), Thrown> {
        info.set(req, self.errno, &self.sqlstate, &self.error)
    }
}

/// `MYSQLND_PACKET_GREET`.
#[derive(Debug, Default)]
pub(crate) struct Greet {
    pub(crate) protocol_version: u8,
    pub(crate) server_version: Vec<u8>,
    pub(crate) thread_id: u32,
    pub(crate) auth_plugin_data: Vec<u8>,
    pub(crate) server_capabilities: u32,
    pub(crate) charset_no: u8,
    pub(crate) server_status: u16,
    pub(crate) auth_protocol: Option<Vec<u8>>,
    pub(crate) pre41: bool,
    pub(crate) error: Option<PacketError>,
}

/// `php_mysqlnd_greet_read`'s parse.
fn greet(body: &[u8]) -> Result<Greet, Short> {
    let mut p = Body::new(body);
    let mut g = Greet::default();
    p.check()?;
    g.protocol_version = p.u8();
    p.check()?;
    if g.protocol_version == ERROR_MARKER {
        let mut e = PacketError::read_from(p.rest());
        // Bug #26426: "Too many connections" comes without its state.
        if e.errno == 1040 {
            e.sqlstate = *b"08004";
        }
        g.error = Some(e);
        return Ok(g);
    }
    if p.rest().starts_with(b"5.5.5-") {
        p.skip(6);
    }
    g.server_version = p.cstr().to_vec();
    p.check()?;
    g.thread_id = p.u32();
    p.check()?;
    let mut scramble = p.take(8).to_vec();
    p.check()?;
    p.skip(1);
    p.check()?;
    g.server_capabilities = u32::from(p.u16());
    p.check()?;
    g.charset_no = p.u8();
    p.check()?;
    g.server_status = p.u16();
    p.check()?;
    let pad_start = p.at;
    p.skip(13);
    p.check()?;
    let mut data_len = 20usize;
    if p.left() > 0 {
        scramble.extend_from_slice(p.take(12));
        p.skip(1);
    } else {
        g.pre41 = true;
    }
    if p.left() > 0 {
        // A 5.5+ server: back over the NUL that ended the scramble.
        p.at -= 1;
        let pad = body.get(pad_start..).unwrap_or_default();
        let high = u32::from(u16::from_le_bytes([
            pad.first().copied().unwrap_or(0),
            pad.get(1).copied().unwrap_or(0),
        ]));
        g.server_capabilities |= high << 16;
        data_len = usize::from(pad.get(2).copied().unwrap_or(0));
        if data_len > 20 {
            scramble.resize(20, 0);
            scramble.extend_from_slice(p.take(data_len - 20));
        }
    }
    if data_len <= 20 {
        scramble.resize(20, 0);
        scramble.truncate(data_len);
    }
    g.auth_plugin_data = scramble;
    if g.server_capabilities & crate::consts::client::PLUGIN_AUTH != 0 {
        p.check()?;
        let rest = p.rest();
        g.auth_protocol = Some(php_ext::coerce::c_str(rest).to_vec());
    }
    Ok(g)
}

/// The answer to an auth packet, `MYSQLND_PACKET_AUTH_RESPONSE`, and to
/// a change-user one, `MYSQLND_PACKET_CHG_USER_RESPONSE`.
#[derive(Debug, Default)]
pub(crate) struct AuthResponse {
    pub(crate) response_code: u8,
    pub(crate) error: Option<PacketError>,
    pub(crate) new_auth_protocol: Option<Vec<u8>>,
    pub(crate) new_auth_protocol_data: Vec<u8>,
    pub(crate) message: Option<Vec<u8>>,
    /// The change-user response's `server_asked_323_auth`.
    pub(crate) asked_323: bool,
}

/// `php_mysqlnd_auth_response_read`'s parse, which carries the switch
/// request's authentication method and data where the code is `0xFE`.
fn auth_response(req: &mut Request, body: &[u8]) -> Result<AuthResponse, Short> {
    let mut p = Body::new(body);
    let mut r = AuthResponse::default();
    p.check()?;
    r.response_code = p.u8();
    p.check()?;
    if r.response_code == ERROR_MARKER {
        r.error = Some(PacketError::read_from(p.rest()));
        return Ok(r);
    }
    if r.response_code == EODATA_MARKER {
        if p.left() > 0 {
            r.new_auth_protocol = Some(p.cstr().to_vec());
            r.new_auth_protocol_data = p.rest().to_vec();
        }
        return Ok(r);
    }
    let _affected = p.lenenc();
    p.check()?;
    let _insert_id = p.lenenc();
    p.check()?;
    let _status = p.u16();
    p.check()?;
    let _warnings = p.u16();
    p.check()?;
    if p.left() > 0 {
        let len = p.field_length();
        if len > 0 {
            if p.at > body.len() || (body.len() - p.at) < usize::try_from(len).unwrap_or(usize::MAX)
            {
                docref(
                    req,
                    Level::Warning,
                    "OK packet message length is past the packet size",
                );
                return Err(Short(0));
            }
            r.message = Some(p.take(usize::try_from(len).unwrap_or(usize::MAX)).to_vec());
        }
    }
    Ok(r)
}

/// `MYSQLND_PACKET_OK`.
#[derive(Debug, Default)]
pub(crate) struct Ok {
    pub(crate) field_count: u8,
    pub(crate) error: Option<PacketError>,
    pub(crate) affected_rows: u64,
    pub(crate) last_insert_id: u64,
    pub(crate) server_status: u32,
    pub(crate) warning_count: u32,
    pub(crate) message: Option<Vec<u8>>,
}

/// `php_mysqlnd_ok_read`'s parse.
fn ok(body: &[u8], cmd_buffer: usize) -> Result<Ok, Short> {
    let mut p = Body::new(body);
    let mut o = Ok::default();
    p.check()?;
    o.field_count = p.u8();
    p.check()?;
    if o.field_count == ERROR_MARKER {
        o.error = Some(PacketError::read_from(p.rest()));
        return Ok(o);
    }
    o.affected_rows = p.lenenc();
    p.check()?;
    o.last_insert_id = p.lenenc();
    p.check()?;
    o.server_status = u32::from(p.u16());
    p.check()?;
    o.warning_count = u32::from(p.u16());
    p.check()?;
    if p.left() > 0 {
        let len = p.field_length();
        if len > 0 {
            let keep =
                (usize::try_from(len).unwrap_or(usize::MAX)).min(cmd_buffer.saturating_sub(p.at));
            o.message = Some(p.take(keep).to_vec());
        }
    }
    p.check()?;
    Ok(o)
}

/// `MYSQLND_PACKET_EOF`.
#[derive(Debug, Default)]
pub(crate) struct Eof {
    pub(crate) field_count: u8,
    pub(crate) error: Option<PacketError>,
    pub(crate) warning_count: u32,
    pub(crate) server_status: u32,
}

/// `php_mysqlnd_eof_read`'s parse: a one-byte EOF after a prepared
/// statement's column definitions carries no status.
fn eof(body: &[u8]) -> Result<Eof, Short> {
    let mut p = Body::new(body);
    let mut e = Eof::default();
    p.check()?;
    e.field_count = p.u8();
    p.check()?;
    if e.field_count == ERROR_MARKER {
        e.error = Some(PacketError::read_from(p.rest()));
        return Ok(e);
    }
    if body.len() > 1 {
        e.warning_count = u32::from(p.u16());
        p.check()?;
        e.server_status = u32::from(p.u16());
        p.check()?;
    }
    p.check()?;
    Ok(e)
}

/// What a query's first answer is, `MYSQLND_PACKET_RSET_HEADER`.
#[derive(Debug)]
pub(crate) enum RsetHeader {
    Error(PacketError),
    /// `MYSQLND_NULL_LENGTH`: the server asks for a local file.
    LocalInfile(Vec<u8>),
    /// `0`: an OK.
    Upsert {
        affected_rows: u64,
        last_insert_id: u64,
        server_status: u32,
        warning_count: u32,
        info: Option<Vec<u8>>,
    },
    /// Rows of this many columns.
    Fields(u64),
}

/// `php_mysqlnd_rset_header_read`'s parse.
fn rset_header(req: &mut Request, body: &[u8]) -> Result<RsetHeader, Short> {
    let mut p = Body::new(body);
    p.check()?;
    if body.first() == Some(&ERROR_MARKER) {
        p.skip(1);
        p.check()?;
        return Ok(RsetHeader::Error(PacketError::read_from(p.rest())));
    }
    let count = p.field_length();
    p.check()?;
    match count {
        NULL_LENGTH => Ok(RsetHeader::LocalInfile(p.rest().to_vec())),
        0 => {
            let affected_rows = p.lenenc();
            p.check()?;
            let last_insert_id = p.lenenc();
            p.check()?;
            let server_status = u32::from(p.u16());
            p.check()?;
            let warning_count = u32::from(p.u16());
            p.check()?;
            let mut info = None;
            if p.left() > 0 {
                let len = p.field_length();
                if len > 0 {
                    if p.at > body.len()
                        || (body.len() - p.at) < usize::try_from(len).unwrap_or(usize::MAX)
                    {
                        let over = (p.at + usize::try_from(len).unwrap_or(usize::MAX))
                            .saturating_sub(body.len());
                        docref(
                            req,
                            Level::Warning,
                            &format!(
                                "RSET_HEADER packet additional data length is past {over} bytes the packet size"
                            ),
                        );
                        return Err(Short(usize::MAX));
                    }
                    info = Some(p.take(usize::try_from(len).unwrap_or(usize::MAX)).to_vec());
                }
            }
            p.check()?;
            Ok(RsetHeader::Upsert {
                affected_rows,
                last_insert_id,
                server_status,
                warning_count,
                info,
            })
        }
        n => {
            p.check()?;
            Ok(RsetHeader::Fields(n))
        }
    }
}

/// `MYSQLND_PACKET_PREPARE_RESPONSE`.
#[derive(Debug, Default)]
pub(crate) struct PrepareResponse {
    pub(crate) error: Option<PacketError>,
    pub(crate) stmt_id: u32,
    pub(crate) field_count: u32,
    pub(crate) param_count: u32,
    pub(crate) warning_count: u32,
}

/// The packet readers: each answers the packet it read and parsed, or
/// `None` for php's `FAIL`, after whatever php set or warned; the `Err`
/// is the memory limit's fatal, met by the error list's element.
impl ConnData {
    /// `mysqlnd_read_packet_header_and_body`: one packet's body, counted
    /// under its kind, the connection marked gone where the server is.
    pub(crate) fn read_packet(
        &mut self,
        req: &mut Request,
        kind: Prot,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let Some(size) = self.net.read_header(req, &mut self.stats, &mut self.error) else {
            self.state = ConnState::QuitSent;
            if self.error.errno == 0 {
                self.error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
            }
            return Ok(None);
        };
        if self.net.cmd_buffer_len() < size {
            self.error.client(
                req,
                cr::INVALID_BUFFER_USE,
                "Packet buffer wasn't big enough; as a workaround consider increasing value of net_cmd_buffer_size",
            )?;
            return Ok(None);
        }
        let Some(body) = self.net.receive(req, size, &mut self.stats) else {
            self.state = ConnState::QuitSent;
            self.error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
            return Ok(None);
        };
        if let Some((bytes, packets)) = kind.stats() {
            self.stats.add(bytes, (HEADER_SIZE + size) as u64);
            self.stats.inc(packets);
        }
        Ok(Some(body))
    }

    /// A packet read and parsed, the premature end warned as php warns
    /// it.
    fn parsed<T>(
        &mut self,
        req: &mut Request,
        kind: Prot,
        parse: impl FnOnce(&mut Request, &[u8], usize) -> Result<T, Short>,
    ) -> Result<Option<T>, Thrown> {
        let Some(body) = self.read_packet(req, kind)? else {
            return Ok(None);
        };
        Ok(match parse(req, &body, self.net.cmd_buffer_len()) {
            Ok(packet) => Some(packet),
            Err(Short(usize::MAX | 0)) => None,
            Err(Short(short)) => {
                docref(
                    req,
                    Level::Warning,
                    &format!("{} packet {short} bytes shorter than expected", kind.name()),
                );
                None
            }
        })
    }

    /// `php_mysqlnd_greet_read`.
    pub(crate) fn read_greet(&mut self, req: &mut Request) -> Result<Option<Greet>, Thrown> {
        self.parsed(req, Prot::Greet, |_, body, _| greet(body))
    }

    /// `php_mysqlnd_auth_response_read`.
    pub(crate) fn read_auth_response(
        &mut self,
        req: &mut Request,
    ) -> Result<Option<AuthResponse>, Thrown> {
        self.parsed(req, Prot::Ok, |req, body, _| auth_response(req, body))
    }

    /// `php_mysqlnd_ok_read`.
    pub(crate) fn read_ok(&mut self, req: &mut Request) -> Result<Option<Ok>, Thrown> {
        self.parsed(req, Prot::Ok, |_, body, cmd| ok(body, cmd))
    }

    /// `php_mysqlnd_eof_read`.
    pub(crate) fn read_eof(&mut self, req: &mut Request) -> Result<Option<Eof>, Thrown> {
        self.parsed(req, Prot::Eof, |_, body, _| eof(body))
    }

    /// `php_mysqlnd_rset_header_read`.
    pub(crate) fn read_rset_header(
        &mut self,
        req: &mut Request,
    ) -> Result<Option<RsetHeader>, Thrown> {
        self.parsed(req, Prot::RsetHeader, |req, body, _| rset_header(req, body))
    }

    /// `php_mysqlnd_stats_read`: the server's text as it came.
    pub(crate) fn read_stats(&mut self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        self.read_packet(req, Prot::Stats)
    }

    /// `php_mysqlnd_prepare_read`.
    pub(crate) fn read_prepare_response(
        &mut self,
        req: &mut Request,
    ) -> Result<Option<PrepareResponse>, Thrown> {
        let Some(body) = self.read_packet(req, Prot::PrepareResponse)? else {
            return Ok(None);
        };
        let mut p = Body::new(&body);
        let mut r = PrepareResponse::default();
        if body.is_empty() {
            docref(
                req,
                Level::Warning,
                "PREPARE packet 1 bytes shorter than expected",
            );
            return Ok(None);
        }
        let code = p.u8();
        if code == ERROR_MARKER {
            r.error = Some(PacketError::read_from(p.rest()));
            return Ok(Some(r));
        }
        let size = body.len();
        if size != 9 && size < 12 {
            php_ext::raising::raise(
                req,
                Level::Warning,
                &format!("Wrong COM_STMT_PREPARE response size. Received {size}"),
            );
            return Ok(None);
        }
        r.stmt_id = p.u32();
        r.field_count = u32::from(p.u16());
        r.param_count = u32::from(p.u16());
        if size > 9 {
            p.skip(1);
            r.warning_count = u32::from(p.u16());
        }
        Ok(Some(r))
    }

    /// `php_mysqlnd_chg_user_read`.
    pub(crate) fn read_change_user_response(
        &mut self,
        req: &mut Request,
    ) -> Result<Option<AuthResponse>, Thrown> {
        let capabilities = self.server_capabilities;
        self.parsed(req, Prot::ChangeUser, move |_, body, _| {
            let mut p = Body::new(body);
            let mut r = AuthResponse::default();
            p.check()?;
            r.response_code = p.u8();
            if body.len() == 1
                && body.first() == Some(&EODATA_MARKER)
                && capabilities & crate::consts::client::SECURE_CONNECTION != 0
            {
                r.asked_323 = true;
                return Ok(r);
            }
            if r.response_code == ERROR_MARKER {
                r.error = Some(PacketError::read_from(p.rest()));
            }
            p.check()?;
            if r.response_code == EODATA_MARKER && p.left() > 0 {
                r.new_auth_protocol = Some(p.cstr().to_vec());
                r.new_auth_protocol_data = p.rest().to_vec();
            }
            Ok(r)
        })
    }

    /// `php_mysqlnd_sha256_pk_request_response_read`: the PEM after the
    /// first byte.
    pub(crate) fn read_public_key(&mut self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        self.parsed(req, Prot::Sha256PkResponse, |_, body, _| {
            let mut p = Body::new(body);
            p.check()?;
            p.skip(1);
            p.check()?;
            Ok(p.rest().to_vec())
        })
    }

    /// `php_mysqlnd_cached_sha2_result_read`: the code, `0xFF` with the
    /// error, `0xFE` with the switch request, else `0x01` followed by the
    /// fast-auth answer (3) or the full-auth request (4).
    pub(crate) fn read_cached_sha2(
        &mut self,
        req: &mut Request,
    ) -> Result<Option<AuthResponse>, Thrown> {
        self.parsed(req, Prot::CachedSha2, |_, body, _| {
            let mut p = Body::new(body);
            let mut r = AuthResponse::default();
            p.check()?;
            r.response_code = p.u8();
            p.check()?;
            if r.response_code == ERROR_MARKER {
                r.error = Some(PacketError::read_from(p.rest()));
                return Ok(r);
            }
            if r.response_code == EODATA_MARKER {
                if p.left() > 0 {
                    r.new_auth_protocol = Some(p.cstr().to_vec());
                    r.new_auth_protocol_data = p.rest().to_vec();
                }
                return Ok(r);
            }
            r.response_code = p.u8();
            p.check()?;
            Ok(r)
        })
    }
}
