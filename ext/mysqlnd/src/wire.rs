//! `mysqlnd_wireprotocol.c`: the packets, each read as php reads it, with
//! php's warnings where one is malformed. What a packet's body says is
//! decoded once, into the type here, from the bytes the frame codec
//! handed over; `mysql_common` serves the scrambles (`auth`).
//!
//! Every read is bounded by what is left of the body, as php 8.5.11's
//! fix of GHSA-r6x9-5r99-36j7 bounds it: a parse asks [`Body::need`] for
//! the bytes its next fields take, at the line of php's
//! `BAIL_IF_NOT_ENOUGH_DATA_EX`, and a length-encoded integer or a
//! NUL-terminated string is read only where it ends inside the body.

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
    /// The answer to an auth packet, an OK packet as php counts it.
    AuthResponse,
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
            Prot::Ok | Prot::AuthResponse => (Stat::BytesReceivedOk, Stat::PacketsReceivedOk),
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

    /// The name the packet's `premature_end:` warns under, "%s packet
    /// shorter than expected"; the statistics' read never bails.
    fn name(self) -> Option<&'static str> {
        Some(match self {
            Prot::Greet => "GREET",
            Prot::Ok => "OK",
            Prot::AuthResponse => "AUTH_RESPONSE",
            Prot::Eof => "EOF",
            Prot::RsetHeader => "RSET_HEADER",
            Prot::RsetField => "Result set field",
            Prot::PrepareResponse => "PREPARE",
            Prot::ChangeUser => "CHANGE_USER",
            Prot::Sha256PkResponse | Prot::CachedSha2 => "SHA256_PK_REQUEST_RESPONSE",
            Prot::Stats => return None,
        })
    }
}

/// A line of php's `mysqlnd_wireprotocol.c` at the pinned php-src: the
/// `__LINE__` of one bail, which its "Premature end of data" warning
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Line(pub(crate) u32);

/// Why a parse failed, php's `FAIL` out of a packet's read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Malformed {
    /// `goto premature_end` from the bail at this line: "Premature end
    /// of data (`mysqlnd_wireprotocol.c:%u`)", then the packet's "%s
    /// packet shorter than expected".
    Premature(Line),
    /// A `DBG_RETURN(FAIL)` past the parse's own warning, or with none.
    Failed,
}

impl Malformed {
    /// What php warns of the failure out of a `kind` packet: a premature
    /// end's two warnings, `BAIL_PREMATURE_END`'s and `premature_end:`'s;
    /// nothing more for a parse that warned on its own.
    pub(crate) fn warn(self, req: &mut Request, kind: Prot) {
        let Malformed::Premature(Line(line)) = self else {
            return;
        };
        docref(
            req,
            Level::Warning,
            &format!("Premature end of data (mysqlnd_wireprotocol.c:{line})"),
        );
        if let Some(name) = kind.name() {
            docref(
                req,
                Level::Warning,
                &format!("{name} packet shorter than expected"),
            );
        }
    }
}

/// What `PACKET_READ` left: the packet, filled as far as its parse went,
/// and whether the read passed (php's `PASS`) or failed past it.
#[derive(Debug)]
pub(crate) enum Read<T> {
    Passed(T),
    Failed(T),
}

impl<T> Read<T> {
    /// The packet where the read passed.
    pub(crate) fn passed(self) -> Option<T> {
        match self {
            Read::Passed(packet) => Some(packet),
            Read::Failed(_) => None,
        }
    }
}

/// `php_mysqlnd_net_field_length`'s answer, its two sentinels decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Length {
    /// The length the bytes say.
    Of(u64),
    /// `251`, `MYSQLND_NULL_LENGTH`.
    Null,
    /// Fewer bytes left than the form takes,
    /// `MYSQLND_INVALID_NET_FIELD_LENGTH`; the cursor stays where it was.
    Short,
}

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

    /// `BAIL_IF_NOT_ENOUGH_DATA_EX(n)` at `line`: whether `n` more bytes
    /// are left of the body.
    pub(crate) fn need(&self, n: usize, line: u32) -> Result<(), Malformed> {
        if self.at.saturating_add(n) > self.bytes.len() {
            return Err(Malformed::Premature(Line(line)));
        }
        Ok(())
    }

    /// php's `p - begin`.
    pub(crate) fn position(&self) -> usize {
        self.at
    }

    /// php's `packet->header.size - (p - begin)`, none once past it.
    pub(crate) fn left(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.at..).unwrap_or_default()
    }

    pub(crate) fn skip(&mut self, n: usize) {
        self.at += n;
    }

    /// One byte, read where [`Body::need`] said it stands; a byte past
    /// the body reads as zero.
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

    fn u64(&mut self) -> u64 {
        let lo = u64::from(self.u32());
        lo | (u64::from(self.u32()) << 32)
    }

    pub(crate) fn take(&mut self, n: usize) -> &'a [u8] {
        let end = (self.at + n).min(self.bytes.len());
        let out = self.bytes.get(self.at.min(end)..end).unwrap_or_default();
        self.at += n;
        out
    }

    /// A NUL-terminated string and its NUL, the NUL looked for in what is
    /// left (`memchr`); none there is the bail at `line`.
    pub(crate) fn cstr(&mut self, line: u32) -> Result<&'a [u8], Malformed> {
        let rest = self.rest();
        let Some(len) = rest.iter().position(|b| *b == 0) else {
            return Err(Malformed::Premature(Line(line)));
        };
        self.at += len + 1;
        Ok(&rest[..len])
    }

    /// A length-encoded integer, bounded by what is left: [`Length::Short`]
    /// where the form its first byte says is longer, the cursor left
    /// where it was. `wide` reads all eight bytes of the nine-byte form
    /// (`php_mysqlnd_net_field_length_ll`); without it four are read and
    /// eight skipped, php's `zend_ulong` of `php_mysqlnd_net_field_length`.
    fn length(&mut self, wide: bool) -> Length {
        let Some(&first) = self.bytes.get(self.at) else {
            return Length::Short;
        };
        let width = match first {
            0..=251 => 1,
            252 => 3,
            253 => 4,
            _ => 9,
        };
        if width > self.left() {
            return Length::Short;
        }
        let start = self.at;
        self.at += 1;
        let length = match first {
            0..=250 => Length::Of(u64::from(first)),
            251 => Length::Null,
            252 => Length::Of(u64::from(self.u16())),
            253 => {
                let lo = u64::from(self.u16());
                Length::Of(lo | (u64::from(self.u8()) << 16))
            }
            _ if wide => Length::Of(self.u64()),
            _ => Length::Of(u64::from(self.u32())),
        };
        self.at = start + width;
        length
    }

    /// `php_mysqlnd_net_field_length_ll`: a count, [`NULL_LENGTH`] for
    /// `251` as php's `uint64_t` holds it; too few bytes left is the bail
    /// at `line`.
    pub(crate) fn lenenc(&mut self, line: u32) -> Result<u64, Malformed> {
        match self.length(true) {
            Length::Of(n) => Ok(n),
            Length::Null => Ok(NULL_LENGTH),
            Length::Short => Err(Malformed::Premature(Line(line))),
        }
    }

    /// `php_mysqlnd_net_field_length` with what is left as its
    /// `remaining_size`.
    pub(crate) fn field_length(&mut self) -> Length {
        self.length(false)
    }

    /// The `len` bytes a length says, where they are all left of the
    /// body; `None` for a `NULL` or short length and one past the end.
    pub(crate) fn within(&mut self, len: Length) -> Option<&'a [u8]> {
        let Length::Of(len) = len else {
            return None;
        };
        let len = usize::try_from(len)
            .ok()
            .filter(|len| *len <= self.left())?;
        Some(self.take(len))
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

/// `php_mysqlnd_greet_read`'s parse: the version NUL-terminated inside
/// the packet, the 31 bytes after it whole, and a scramble longer than
/// 20 bytes only as long as what is left.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $talks = ['00000000', '010000000a', '080000000a352e3503030303473e3f6047257c67', '110000000a382e300003000000473e3f6047257c67', '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81ff0000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264'];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:402)
/// mysqli::__construct(): GREET packet shorter than expected
/// mysqli::__construct(): Error while reading greeting packet. PID
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:409)
/// mysqli::__construct(): GREET packet shorter than expected
/// mysqli::__construct(): Error while reading greeting packet. PID
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:436)
/// mysqli::__construct(): GREET packet shorter than expected
/// mysqli::__construct(): Error while reading greeting packet. PID
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:441)
/// mysqli::__construct(): GREET packet shorter than expected
/// mysqli::__construct(): Error while reading greeting packet. PID
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:487)
/// mysqli::__construct(): GREET packet shorter than expected
/// mysqli::__construct(): Error while reading greeting packet. PID
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// ```
fn greet(body: &[u8], g: &mut Greet) -> Result<(), Malformed> {
    let mut p = Body::new(body);
    p.need(1, 402)?;
    g.protocol_version = p.u8();
    p.need(1, 409)?;
    if g.protocol_version == ERROR_MARKER {
        let mut e = PacketError::read_from(p.rest());
        // Bug #26426: "Too many connections" comes without its state.
        if e.errno == 1040 {
            e.sqlstate = *b"08004";
        }
        g.error = Some(e);
        return Ok(());
    }
    if p.rest().starts_with(b"5.5.5-") {
        p.skip(6);
    }
    g.server_version = p.cstr(436)?.to_vec();
    p.need(4 + 8 + 1 + 2 + 1 + 2 + 13, 441)?;
    g.thread_id = p.u32();
    let mut scramble = p.take(8).to_vec();
    p.skip(1);
    g.server_capabilities = u32::from(p.u16());
    g.charset_no = p.u8();
    g.server_status = p.u16();
    let pad_start = p.at;
    p.skip(13);
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
            p.need(data_len - 20, 487)?;
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
        p.need(1, 504)?;
        g.auth_protocol = Some(php_ext::coerce::c_str(p.rest()).to_vec());
    }
    Ok(())
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

/// `0xFE`'s switch request: the method's name NUL-terminated inside the
/// packet (the bail at `line` where it is not), its data the rest.
fn switch_request(p: &mut Body<'_>, r: &mut AuthResponse, line: u32) -> Result<(), Malformed> {
    r.new_auth_protocol = Some(p.cstr(line)?.to_vec());
    r.new_auth_protocol_data = p.rest().to_vec();
    Ok(())
}

/// `php_mysqlnd_auth_response_read`'s parse, which carries the switch
/// request's authentication method and data where the code is `0xFE`;
/// the code is kept where the parse fails past it, which the handshake
/// reads.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264';
/// $talks = ["$greet,00000002", "$greet,0100000200", "$greet,0200000200fe", "$greet,020000020000", "$greet,06000002000000020000", "$greet,16000002fe6d7973716c5f6e61746976655f70617373776f7264"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:738)
/// mysqli::__construct(): AUTH_RESPONSE packet shorter than expected
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:743)
/// mysqli::__construct(): AUTH_RESPONSE packet shorter than expected
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:779)
/// mysqli::__construct(): AUTH_RESPONSE packet shorter than expected
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:785)
/// mysqli::__construct(): AUTH_RESPONSE packet shorter than expected
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:789)
/// mysqli::__construct(): AUTH_RESPONSE packet shorter than expected
/// mysqli::__construct(): (HY000/2002): Unknown error while trying to connect via tcp://127.0.0.1
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:758)
/// mysqli::__construct(): AUTH_RESPONSE packet shorter than expected
/// mysqli::__construct(): (HY000/2000): mysqlnd cannot connect to MySQL 4.1+ using the old insecure authentication. Please use an administration tool to reset your password with the command SET PASSWORD = PASSWORD('your_existing_password'). This will store a new, and more secure, hash value in mysql.user. If this user is used in other scripts executed by PHP 5.2 or earlier you might need to remove the old-passwords flag from your my.cnf file
/// ```
fn auth_response(req: &mut Request, body: &[u8], r: &mut AuthResponse) -> Result<(), Malformed> {
    let mut p = Body::new(body);
    p.need(1, 738)?;
    r.response_code = p.u8();
    p.need(1, 743)?;
    if r.response_code == ERROR_MARKER {
        r.error = Some(PacketError::read_from(p.rest()));
        return Ok(());
    }
    if r.response_code == EODATA_MARKER {
        if p.left() > 0 {
            switch_request(&mut p, r, 758)?;
        }
        return Ok(());
    }
    let _affected = p.lenenc(779)?;
    let _insert_id = p.lenenc(785)?;
    p.need(4, 789)?;
    let _status = p.u16();
    let _warnings = p.u16();
    if p.left() > 0 {
        let len = p.field_length();
        if len == Length::Of(0) {
            return Ok(());
        }
        let Some(message) = p.within(len) else {
            docref(
                req,
                Level::Warning,
                "OK packet message length is past the packet size",
            );
            return Err(Malformed::Failed);
        };
        r.message = Some(message.to_vec());
    }
    Ok(())
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

/// `php_mysqlnd_ok_read`'s parse: the message as long as its length
/// says, cut at the command buffer's end (`MIN(net_len, buf_len - (p -
/// begin))`), a `NULL` length reading to that end.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264,0700000200000002000000';
/// $talks = ["$greet,00000001", "$greet,0200000100fe", "$greet,030000010000fd", "$greet,06000001000000020000", "$greet,0800000100000002000000fd", "$greet,0100000100"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); var_dump($m->select_db('php_test')); echo $m->error, "\n"; $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::select_db(): Premature end of data (mysqlnd_wireprotocol.c:912)
/// mysqli::select_db(): OK packet shorter than expected
/// mysqli::select_db(): Error while reading INIT_DB's response packet. PID
/// bool(false)
/// Malformed packet
/// mysqli::select_db(): Premature end of data (mysqlnd_wireprotocol.c:931)
/// mysqli::select_db(): OK packet shorter than expected
/// mysqli::select_db(): Error while reading INIT_DB's response packet. PID
/// bool(false)
/// Malformed packet
/// mysqli::select_db(): Premature end of data (mysqlnd_wireprotocol.c:937)
/// mysqli::select_db(): OK packet shorter than expected
/// mysqli::select_db(): Error while reading INIT_DB's response packet. PID
/// bool(false)
/// Malformed packet
/// mysqli::select_db(): Premature end of data (mysqlnd_wireprotocol.c:941)
/// mysqli::select_db(): OK packet shorter than expected
/// mysqli::select_db(): Error while reading INIT_DB's response packet. PID
/// bool(false)
/// Malformed packet
/// mysqli::select_db(): Premature end of data (mysqlnd_wireprotocol.c:952)
/// mysqli::select_db(): OK packet shorter than expected
/// mysqli::select_db(): Error while reading INIT_DB's response packet. PID
/// bool(false)
/// Malformed packet
/// mysqli::select_db(): Premature end of data (mysqlnd_wireprotocol.c:917)
/// mysqli::select_db(): OK packet shorter than expected
/// mysqli::select_db(): Error while reading INIT_DB's response packet. PID
/// bool(false)
/// Malformed packet
/// ```
fn ok(body: &[u8], cmd_buffer: usize, o: &mut Ok) -> Result<(), Malformed> {
    let mut p = Body::new(body);
    p.need(1, 912)?;
    o.field_count = p.u8();
    p.need(1, 917)?;
    if o.field_count == ERROR_MARKER {
        o.error = Some(PacketError::read_from(p.rest()));
        return Ok(());
    }
    o.affected_rows = p.lenenc(931)?;
    o.last_insert_id = p.lenenc(937)?;
    p.need(4, 941)?;
    o.server_status = u32::from(p.u16());
    o.warning_count = u32::from(p.u16());
    if p.left() > 0 {
        let len = p.field_length();
        let room = cmd_buffer.saturating_sub(p.at);
        let keep = match len {
            Length::Of(0) => None,
            Length::Of(len) => Some(usize::try_from(len).unwrap_or(usize::MAX).min(room)),
            Length::Null => Some(room),
            Length::Short => return Err(Malformed::Premature(Line(952))),
        };
        o.message = keep.map(|keep| p.take(keep).to_vec());
    }
    Ok(())
}

/// `MYSQLND_PACKET_EOF`.
#[derive(Debug, Default)]
pub(crate) struct Eof {
    pub(crate) field_count: u8,
    pub(crate) error: Option<PacketError>,
    pub(crate) warning_count: u32,
    pub(crate) server_status: u32,
}

/// `php_mysqlnd_eof_read`'s parse: the marker, then the warnings and the
/// status whole.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264,0700000200000002000000';
/// $talks = ["$greet,00000001", "$greet,04000001fe000200", "$greet,01000001fe"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); var_dump($m->multi_query('SELECT 1; SELECT 2')); echo $m->error, "\n"; $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::multi_query(): Premature end of data (mysqlnd_wireprotocol.c:1013)
/// mysqli::multi_query(): EOF packet shorter than expected
/// mysqli::multi_query(): Error while reading SET_OPTION's response packet. PID
/// bool(false)
/// MySQL server has gone away
/// mysqli::multi_query(): Premature end of data (mysqlnd_wireprotocol.c:1034)
/// mysqli::multi_query(): EOF packet shorter than expected
/// mysqli::multi_query(): Error while reading SET_OPTION's response packet. PID
/// bool(false)
/// MySQL server has gone away
/// mysqli::multi_query(): Premature end of data (mysqlnd_wireprotocol.c:1018)
/// mysqli::multi_query(): EOF packet shorter than expected
/// mysqli::multi_query(): Error while reading SET_OPTION's response packet. PID
/// bool(false)
/// MySQL server has gone away
/// ```
fn eof(body: &[u8], e: &mut Eof) -> Result<(), Malformed> {
    let mut p = Body::new(body);
    p.need(1, 1013)?;
    e.field_count = p.u8();
    p.need(1, 1018)?;
    if e.field_count == ERROR_MARKER {
        e.error = Some(PacketError::read_from(p.rest()));
        return Ok(());
    }
    if body.len() > 1 {
        p.need(4, 1034)?;
        e.warning_count = u32::from(p.u16());
        e.server_status = u32::from(p.u16());
    }
    Ok(())
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

/// `php_mysqlnd_rset_header_read`'s parse: the column count, or an OK
/// whose text is read only as long as what is left.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264,0700000200000002000000';
/// $talks = ["$greet,00000001", "$greet,01000001ff", "$greet,030000010000fd", "$greet,01000001fd", "$greet,0200000100fd", "$greet,0400000100000000", "$greet,080000010000000200000005"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); var_dump($m->query('SELECT 1')); echo $m->error, "\n"; $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::query(): Premature end of data (mysqlnd_wireprotocol.c:1140)
/// mysqli::query(): RSET_HEADER packet shorter than expected
/// mysqli::query(): Error reading result set's header
/// bool(false)
///
/// mysqli::query(): Premature end of data (mysqlnd_wireprotocol.c:1149)
/// mysqli::query(): RSET_HEADER packet shorter than expected
/// mysqli::query(): Error reading result set's header
/// bool(false)
///
/// mysqli::query(): Premature end of data (mysqlnd_wireprotocol.c:1190)
/// mysqli::query(): RSET_HEADER packet shorter than expected
/// mysqli::query(): Error reading result set's header
/// bool(false)
///
/// mysqli::query(): Premature end of data (mysqlnd_wireprotocol.c:1159)
/// mysqli::query(): RSET_HEADER packet shorter than expected
/// mysqli::query(): Error reading result set's header
/// bool(false)
///
/// mysqli::query(): Premature end of data (mysqlnd_wireprotocol.c:1184)
/// mysqli::query(): RSET_HEADER packet shorter than expected
/// mysqli::query(): Error reading result set's header
/// bool(false)
///
/// mysqli::query(): Premature end of data (mysqlnd_wireprotocol.c:1194)
/// mysqli::query(): RSET_HEADER packet shorter than expected
/// mysqli::query(): Error reading result set's header
/// bool(false)
///
/// mysqli::query(): RSET_HEADER packet additional data length is past 5 bytes the packet size
/// mysqli::query(): Error reading result set's header
/// bool(false)
/// ```
fn rset_header(req: &mut Request, body: &[u8]) -> Result<RsetHeader, Malformed> {
    let mut p = Body::new(body);
    p.need(1, 1140)?;
    if body.first() == Some(&ERROR_MARKER) {
        p.skip(1);
        p.need(1, 1149)?;
        return Ok(RsetHeader::Error(PacketError::read_from(p.rest())));
    }
    match p.field_length() {
        Length::Short => Err(Malformed::Premature(Line(1159))),
        Length::Null => Ok(RsetHeader::LocalInfile(p.rest().to_vec())),
        Length::Of(0) => {
            let affected_rows = p.lenenc(1184)?;
            let last_insert_id = p.lenenc(1190)?;
            p.need(4, 1194)?;
            let server_status = u32::from(p.u16());
            let warning_count = u32::from(p.u16());
            // php's `len` as its `size_t` arithmetic reads it, a `NULL`
            // one `(size_t) -1`.
            let len = match p.field_length() {
                Length::Of(0) | Length::Short => None,
                Length::Of(len) => Some(usize::try_from(len).unwrap_or(usize::MAX)),
                Length::Null => Some(usize::MAX),
            };
            let mut info = None;
            if let Some(len) = len {
                if p.left() < len {
                    // `((p - buf) - packet->header.size) + len`.
                    let over = p.at.wrapping_sub(body.len()).wrapping_add(len);
                    docref(
                        req,
                        Level::Warning,
                        &format!(
                            "RSET_HEADER packet additional data length is past {over} bytes the packet size"
                        ),
                    );
                    return Err(Malformed::Failed);
                }
                info = Some(p.take(len).to_vec());
            }
            Ok(RsetHeader::Upsert {
                affected_rows,
                last_insert_id,
                server_status,
                warning_count,
                info,
            })
        }
        Length::Of(n) => Ok(RsetHeader::Fields(n)),
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

/// `php_mysqlnd_prepare_read`'s parse: a body of 9 bytes, or of 12 and
/// more with the warnings.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264,0700000200000002000000';
/// $talks = ["$greet,00000001", "$greet,0100000100", "$greet,0a00000100010000000000000000"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); var_dump($m->prepare('SELECT 1')); echo $m->error, "\n"; $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::prepare(): Premature end of data (mysqlnd_wireprotocol.c:1924)
/// mysqli::prepare(): PREPARE packet shorter than expected
/// bool(false)
///
/// mysqli::prepare(): Premature end of data (mysqlnd_wireprotocol.c:1929)
/// mysqli::prepare(): PREPARE packet shorter than expected
/// bool(false)
///
/// Wrong COM_STMT_PREPARE response size. Received 10
/// bool(false)
/// ```
fn prepare_response(req: &mut Request, body: &[u8]) -> Result<PrepareResponse, Malformed> {
    let mut p = Body::new(body);
    let mut r = PrepareResponse::default();
    p.need(1, 1924)?;
    let code = p.u8();
    p.need(1, 1929)?;
    if code == ERROR_MARKER {
        r.error = Some(PacketError::read_from(p.rest()));
        return Ok(r);
    }
    let size = body.len();
    if size != 9 && size < 12 {
        php_ext::raising::raise(
            req,
            Level::Warning,
            &format!("Wrong COM_STMT_PREPARE response size. Received {size}"),
        );
        return Err(Malformed::Failed);
    }
    p.need(8, 1949)?;
    r.stmt_id = p.u32();
    r.field_count = u32::from(p.u16());
    r.param_count = u32::from(p.u16());
    if size > 9 {
        p.need(3, 1962)?;
        p.skip(1);
        r.warning_count = u32::from(p.u16());
    }
    Ok(r)
}

/// `php_mysqlnd_chg_user_read`'s parse: the code kept where the parse
/// fails past it, and an error packet's error kept where the bail after
/// it is taken, as `mysqlnd_auth_change_user` reads both.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264,0700000200000002000000';
/// $talks = ["$greet,00000001", "$greet,0100000100", "$greet,16000001fe6d7973716c5f6e61746976655f70617373776f7264", "$greet,01000001ff"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); var_dump($m->change_user('root2', '', '')); echo $m->errno, ' [', $m->error, "]\n"; $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::change_user(): Premature end of data (mysqlnd_wireprotocol.c:2003)
/// mysqli::change_user(): CHANGE_USER packet shorter than expected
/// bool(false)
/// 0 []
/// mysqli::change_user(): Premature end of data (mysqlnd_wireprotocol.c:2028)
/// mysqli::change_user(): CHANGE_USER packet shorter than expected
/// bool(false)
/// 0 []
/// mysqli::change_user(): Premature end of data (mysqlnd_wireprotocol.c:2033)
/// mysqli::change_user(): CHANGE_USER packet shorter than expected
/// bool(false)
/// 2000 [mysqlnd cannot connect to MySQL 4.1+ using the old insecure authentication. Please use an administration tool to reset your password with the command SET PASSWORD = PASSWORD('your_existing_password'). This will store a new, and more secure, hash value in mysql.user. If this user is used in other scripts executed by PHP 5.2 or earlier you might need to remove the old-passwords flag from your my.cnf file]
/// mysqli::change_user(): Premature end of data (mysqlnd_wireprotocol.c:2028)
/// mysqli::change_user(): CHANGE_USER packet shorter than expected
/// bool(false)
/// 2000 []
/// ```
fn change_user_response(
    body: &[u8],
    capabilities: u32,
    r: &mut AuthResponse,
) -> Result<(), Malformed> {
    let mut p = Body::new(body);
    p.need(1, 2003)?;
    r.response_code = p.u8();
    if body.len() == 1
        && r.response_code == EODATA_MARKER
        && capabilities & crate::consts::client::SECURE_CONNECTION != 0
    {
        r.asked_323 = true;
        return Err(Malformed::Failed);
    }
    if r.response_code == ERROR_MARKER {
        r.error = Some(PacketError::read_from(p.rest()));
    }
    p.need(1, 2028)?;
    if r.response_code == EODATA_MARKER && p.left() > 0 {
        switch_request(&mut p, r, 2033)?;
    }
    Ok(())
}

/// `php_mysqlnd_sha256_pk_request_response_read`'s parse: the PEM after
/// the first byte, at least one byte of it.
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '520000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431007368613235365f70617373776f7264';
/// $talks = ["$greet,00000002,0700000400000002000000", "$greet,0100000200,0700000400000002000000"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); echo $m->connect_errno, "\n"; $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:2119)
/// mysqli::__construct(): SHA256_PK_REQUEST_RESPONSE packet shorter than expected
/// Error while receiving public key. PID
/// 0
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:2122)
/// mysqli::__construct(): SHA256_PK_REQUEST_RESPONSE packet shorter than expected
/// Error while receiving public key. PID
/// 0
/// ```
fn public_key(body: &[u8]) -> Result<Vec<u8>, Malformed> {
    let mut p = Body::new(body);
    p.need(1, 2119)?;
    p.skip(1);
    p.need(1, 2122)?;
    Ok(p.rest().to_vec())
}

/// `php_mysqlnd_cached_sha2_result_read`'s parse: the code, `0xFF` with
/// the error, `0xFE` with the switch request, else `0x01` followed by the
/// fast-auth answer (3) or the full-auth request (4).
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c64310063616368696e675f736861325f70617373776f7264';
/// $ok = '0700000300000002000000';
/// $talks = ["$greet,00000002$ok", "$greet,0100000201$ok", "$greet,16000002fe6d7973716c5f6e61746976655f70617373776f7264$ok", "$greet,020000020103$ok"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); echo $m->connect_errno, "\n"; $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:2198)
/// mysqli::__construct(): SHA256_PK_REQUEST_RESPONSE packet shorter than expected
/// 0
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:2202)
/// mysqli::__construct(): SHA256_PK_REQUEST_RESPONSE packet shorter than expected
/// 0
/// mysqli::__construct(): Premature end of data (mysqlnd_wireprotocol.c:2217)
/// mysqli::__construct(): SHA256_PK_REQUEST_RESPONSE packet shorter than expected
/// 0
/// 0
/// ```
fn cached_sha2(body: &[u8], r: &mut AuthResponse) -> Result<(), Malformed> {
    let mut p = Body::new(body);
    p.need(1, 2198)?;
    r.response_code = p.u8();
    p.need(1, 2202)?;
    if r.response_code == ERROR_MARKER {
        r.error = Some(PacketError::read_from(p.rest()));
        return Ok(());
    }
    if r.response_code == EODATA_MARKER {
        if p.left() > 0 {
            switch_request(&mut p, r, 2217)?;
        }
        return Ok(());
    }
    p.need(1, 2238)?;
    r.response_code = p.u8();
    Ok(())
}

/// The packet readers: each answers the packet it read and whether its
/// parse passed, php's `PACKET_READ`, after whatever php set or warned;
/// the `Err` is the memory limit's fatal, met by the error list's
/// element.
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

    /// `PACKET_READ`: a packet read and parsed into a new `T`, filled as
    /// far as the parse went; a failure warned as php warns it.
    fn packet_read<T: Default>(
        &mut self,
        req: &mut Request,
        kind: Prot,
        parse: impl FnOnce(&mut Request, &[u8], usize, &mut T) -> Result<(), Malformed>,
    ) -> Result<Read<T>, Thrown> {
        let mut packet = T::default();
        let Some(body) = self.read_packet(req, kind)? else {
            return Ok(Read::Failed(packet));
        };
        Ok(
            match parse(req, &body, self.net.cmd_buffer_len(), &mut packet) {
                Ok(()) => Read::Passed(packet),
                Err(malformed) => {
                    malformed.warn(req, kind);
                    Read::Failed(packet)
                }
            },
        )
    }

    /// [`ConnData::packet_read`] of a packet the parse makes whole, `None`
    /// where it failed.
    fn read_whole<T>(
        &mut self,
        req: &mut Request,
        kind: Prot,
        parse: impl FnOnce(&mut Request, &[u8]) -> Result<T, Malformed>,
    ) -> Result<Option<T>, Thrown> {
        let read = self.packet_read(req, kind, |req, body, _, packet: &mut Option<T>| {
            *packet = Some(parse(req, body)?);
            Ok(())
        })?;
        Ok(read.passed().flatten())
    }

    /// `php_mysqlnd_greet_read`.
    pub(crate) fn read_greet(&mut self, req: &mut Request) -> Result<Option<Greet>, Thrown> {
        let read = self.packet_read(req, Prot::Greet, |_, body, _, g| greet(body, g))?;
        Ok(read.passed())
    }

    /// `php_mysqlnd_auth_response_read`: the packet whether or not it
    /// passed, as the handshake reads its code either way.
    pub(crate) fn read_auth_response(
        &mut self,
        req: &mut Request,
    ) -> Result<Read<AuthResponse>, Thrown> {
        self.packet_read(req, Prot::AuthResponse, |req, body, _, r| {
            auth_response(req, body, r)
        })
    }

    /// `php_mysqlnd_ok_read`.
    pub(crate) fn read_ok(&mut self, req: &mut Request) -> Result<Option<Ok>, Thrown> {
        let read = self.packet_read(req, Prot::Ok, |_, body, cmd, o| ok(body, cmd, o))?;
        Ok(read.passed())
    }

    /// `php_mysqlnd_eof_read`.
    pub(crate) fn read_eof(&mut self, req: &mut Request) -> Result<Option<Eof>, Thrown> {
        let read = self.packet_read(req, Prot::Eof, |_, body, _, e| eof(body, e))?;
        Ok(read.passed())
    }

    /// `php_mysqlnd_rset_header_read`.
    pub(crate) fn read_rset_header(
        &mut self,
        req: &mut Request,
    ) -> Result<Option<RsetHeader>, Thrown> {
        self.read_whole(req, Prot::RsetHeader, rset_header)
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
        self.read_whole(req, Prot::PrepareResponse, prepare_response)
    }

    /// `php_mysqlnd_chg_user_read`: the packet whether or not it passed,
    /// as `mysqlnd_auth_change_user` reads its code and error either way.
    pub(crate) fn read_change_user_response(
        &mut self,
        req: &mut Request,
    ) -> Result<Read<AuthResponse>, Thrown> {
        let capabilities = self.server_capabilities;
        self.packet_read(req, Prot::ChangeUser, |_, body, _, r| {
            change_user_response(body, capabilities, r)
        })
    }

    /// `php_mysqlnd_sha256_pk_request_response_read`.
    pub(crate) fn read_public_key(&mut self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        self.read_whole(req, Prot::Sha256PkResponse, |_, body| public_key(body))
    }

    /// `php_mysqlnd_cached_sha2_result_read`.
    pub(crate) fn read_cached_sha2(
        &mut self,
        req: &mut Request,
    ) -> Result<Option<AuthResponse>, Thrown> {
        let read = self.packet_read(req, Prot::CachedSha2, |_, body, _, r| cached_sha2(body, r))?;
        Ok(read.passed())
    }
}
