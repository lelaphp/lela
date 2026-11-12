//! `mysqlnd_vio.c` and `mysqlnd_protocol_frame_codec.c`: the connection's
//! socket, one of the engine's streams as php's is a `php_stream`, and the
//! frames over it, the three-byte length and the sequence number of every
//! packet, and the compressed protocol's envelopes around them.
//!
//! Every read and write is the stream's, so `default_socket_timeout`,
//! a unix socket, TLS through the crypto layer `openssl` registers and the
//! read timeout behave as mysqlnd's do over `php_stream`.

use std::io::{Read, Write};
use std::time::Duration;

use php_ext::raising::Thrown;
use php_ext::request::{
    OwnedStream as _, Persisted, Request, SocketOptions, Stream, StreamCtx, StreamOption,
    Streams as _, Xport,
};
use php_ext::values::{Owner, PhpString, Value};

use crate::alloc::{Alloc, Pstr};
use crate::consts::{MAX_PACKET_SIZE, cr};
use crate::error::{ErrorInfo, SERVER_GONE};
use crate::stats::{Stat, Stats};

/// `MYSQLND_HEADER_SIZE`.
pub(crate) const HEADER_SIZE: usize = 4;
/// `COMPRESSED_HEADER_SIZE`.
const COMPRESSED_HEADER_SIZE: usize = 3;
/// `STREAM_CRYPTO_METHOD_TLS_CLIENT`: TLS 1.0 to 1.3 as a client.
const TLS_CLIENT: i64 = (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | 1;

/// `enum mysqlnd_ssl_peer`: whether TLS checks the server's certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, php_ext::abi::EngineState)]
pub(crate) enum SslPeer {
    #[default]
    Default,
    Verify,
    DontVerify,
}

/// `MYSQLND_VIO_DATA`'s options, the TLS paths and cipher each
/// `mnd_pestrdup`'d as the connection is persistent or not. php's
/// `ssl_passphrase` is not among them: `mysqlnd_conn_data::set_client_option`
/// refuses `MYSQLND_OPT_SSL_PASSPHRASE`, so nothing ever sets it.
#[derive(Debug, Default)]
pub(crate) struct VioOptions {
    pub(crate) timeout_connect: u32,
    pub(crate) timeout_read: u32,
    pub(crate) net_read_buffer_size: u32,
    pub(crate) ssl_key: Option<Pstr>,
    pub(crate) ssl_cert: Option<Pstr>,
    pub(crate) ssl_ca: Option<Pstr>,
    pub(crate) ssl_capath: Option<Pstr>,
    pub(crate) ssl_cipher: Option<Pstr>,
    pub(crate) ssl_verify_peer: SslPeer,
}

impl VioOptions {
    /// Whether a TLS option is set, which turns `CLIENT_SSL` on in
    /// `get_updated_connect_flags`.
    pub(crate) fn wants_ssl(&self) -> bool {
        self.ssl_key.is_some()
            || self.ssl_cert.is_some()
            || self.ssl_ca.is_some()
            || self.ssl_capath.is_some()
            || self.ssl_cipher.is_some()
    }
}

/// [`VioOptions`] while the engine's pool keeps the connection: the
/// strings `malloc`'s, as a persistent connection's are.
#[derive(Debug, php_ext::abi::EngineState)]
pub(crate) struct IdleVio {
    timeout_connect: u32,
    timeout_read: u32,
    net_read_buffer_size: u32,
    ssl_key: Option<Box<[u8]>>,
    ssl_cert: Option<Box<[u8]>>,
    ssl_ca: Option<Box<[u8]>>,
    ssl_capath: Option<Box<[u8]>>,
    ssl_cipher: Option<Box<[u8]>>,
    ssl_verify_peer: SslPeer,
}

impl VioOptions {
    /// What the pool keeps of the options.
    fn idle(self, req: &mut Request) -> IdleVio {
        IdleVio {
            timeout_connect: self.timeout_connect,
            timeout_read: self.timeout_read,
            net_read_buffer_size: self.net_read_buffer_size,
            ssl_key: Pstr::idle_field(req, self.ssl_key),
            ssl_cert: Pstr::idle_field(req, self.ssl_cert),
            ssl_ca: Pstr::idle_field(req, self.ssl_ca),
            ssl_capath: Pstr::idle_field(req, self.ssl_capath),
            ssl_cipher: Pstr::idle_field(req, self.ssl_cipher),
            ssl_verify_peer: self.ssl_verify_peer,
        }
    }
}

impl From<IdleVio> for VioOptions {
    fn from(idle: IdleVio) -> VioOptions {
        VioOptions {
            timeout_connect: idle.timeout_connect,
            timeout_read: idle.timeout_read,
            net_read_buffer_size: idle.net_read_buffer_size,
            ssl_key: idle.ssl_key.map(Pstr::Malloc),
            ssl_cert: idle.ssl_cert.map(Pstr::Malloc),
            ssl_ca: idle.ssl_ca.map(Pstr::Malloc),
            ssl_capath: idle.ssl_capath.map(Pstr::Malloc),
            ssl_cipher: idle.ssl_cipher.map(Pstr::Malloc),
            ssl_verify_peer: idle.ssl_verify_peer,
        }
    }
}

/// `MYSQLND_VIO` and `MYSQLND_PFC` together: the stream, the frame
/// codec's sequence numbers and the compressed protocol's read buffer.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // php's flags, `vio->data->ssl` and the frame codec's two
pub(crate) struct Net {
    stream: Option<Owner<Stream>>,
    /// Whether the stream is a unix socket, which `caching_sha2_password` counts as
    /// a secure stream (`is_secure_transport`).
    pub(crate) unix: bool,
    /// `vio->data->ssl`.
    pub(crate) ssl: bool,
    pub(crate) options: VioOptions,
    /// `pfc->data->packet_no`.
    packet_no: u8,
    /// `pfc->data->compressed_envelope_packet_no`.
    envelope_no: u8,
    /// `pfc->data->compressed`.
    pub(crate) compressed: bool,
    /// `MYSQLND_PROTOCOL_FLAG_USE_COMPRESSION`, the option.
    pub(crate) use_compression: bool,
    /// `pfc->data->uncompressed_data`: the envelope last read and how far
    /// the reads have taken it.
    uncompressed: ReadBuffer,
    /// `pfc->cmd_buffer`, its length `pfc->cmd_buffer.length`; none
    /// until the frame codec's `init` sets its size.
    pub(crate) cmd_buffer: Option<Alloc>,
    /// `pfc->data->sha256_server_public_key`.
    pub(crate) sha256_server_public_key: Option<Pstr>,
    /// The `persistent_id` a persistent connection's stream is opened
    /// under, php's `"%p"` of the `MYSQLND_VIO`, which makes the stream a
    /// persistent one; the stream leaves the persistent list as it opens,
    /// so nothing is ever held under it.
    pub(crate) persistent_id: Option<Vec<u8>>,
}

/// A persistent connection's [`Net`] while the engine's pool keeps it:
/// its socket out of every request and of the engine's persistent list,
/// the connection's alone, as php's mysqlnd owns its stream, and no
/// stream of a request's. No read buffer by type: an envelope a request
/// read and did not read through ends with that request, so the next
/// request lent the connection reads no envelope an earlier request
/// left. What the socket's crypto layer holds of its own travels with
/// the socket, as a persistent stream's does (#4602).
#[derive(php_ext::abi::EngineState)]
#[allow(clippy::struct_excessive_bools)] // `Net`'s flags, one each
pub(crate) struct IdleNet {
    kept: Option<Persisted>,
    unix: bool,
    ssl: bool,
    options: IdleVio,
    packet_no: u8,
    envelope_no: u8,
    compressed: bool,
    use_compression: bool,
    cmd_buffer: Option<usize>,
    sha256_server_public_key: Option<Box<[u8]>>,
    persistent_id: Option<Vec<u8>>,
}

impl Net {
    /// `pfc->cmd_buffer.length`.
    pub(crate) fn cmd_buffer_len(&self) -> usize {
        self.cmd_buffer.as_ref().map_or(0, Alloc::len)
    }

    /// `MYSQLND_OPT_NET_CMD_BUFFER_SIZE` on the frame codec: the buffer
    /// allocated at `len`, or reallocated to it; the `Err` is the memory
    /// limit's fatal.
    pub(crate) fn set_cmd_buffer(
        &mut self,
        req: &mut Request,
        len: usize,
        persistent: bool,
    ) -> Result<(), Thrown> {
        self.cmd_buffer = Some(match self.cmd_buffer.take() {
            Some(buffer) => buffer.resize(req, len)?,
            None => Alloc::new(req, len, persistent)?,
        });
        Ok(())
    }

    /// Whether a stream is open, `has_valid_stream`.
    pub(crate) fn open(&self) -> bool {
        self.stream.is_some()
    }

    /// The stream, for what reaches it directly: `LOCAL INFILE`'s and a
    /// connection's resource.
    pub(crate) fn stream(&self) -> Option<Stream> {
        self.stream.as_deref().copied()
    }

    /// `mysqlnd_pfc::reset`: both sequence numbers to zero, as every
    /// command and the handshake begin.
    pub(crate) fn reset(&mut self) {
        self.packet_no = 0;
        self.envelope_no = 0;
    }

    /// `mysqlnd_vio::connect`: the stream for `scheme` opened, its read
    /// timeout set, or the error php sets. The stream is marked
    /// `PHP_STREAM_FLAG_SUPPRESS_ERRORS`, so a send the server's close
    /// broke is the connection's error alone, with no socket notice
    /// before it:
    ///
    /// ```php
    /// $a = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_ERRMODE => PDO::ERRMODE_WARNING]);
    /// $b = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
    /// $b->exec('KILL ' . $a->query('SELECT CONNECTION_ID()')->fetchColumn());
    /// usleep(100000);
    /// var_dump($a->query("SELECT '" . str_repeat('x', 8 << 20) . "'"));
    /// ```
    /// ```expect mysql
    /// Warning: PDO::query(): SQLSTATE[HY000]: General error: 2006 MySQL server has gone away in %s on line 6
    /// bool(false)
    /// ```
    ///
    /// The stream is the connection's alone from its open, persistent or
    /// not: `get_resources()` lists none of the three, and the id each
    /// took stays spent.
    ///
    /// ```php
    /// $n = count(get_resources());
    /// $l = mysqli_connect('p:127.0.0.1', 'root', '', 'test');
    /// $p = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_PERSISTENT => true]);
    /// $q = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// var_dump(count(get_resources()) - $n);
    /// $f = fopen('php://memory', 'r');
    /// var_dump(get_resource_id($f));
    /// ```
    /// ```expect mysql
    /// int(0)
    /// int(8)
    /// ```
    pub(crate) fn connect(
        &mut self,
        req: &mut Request,
        scheme: &[u8],
        error: &mut ErrorInfo,
    ) -> Result<bool, Thrown> {
        self.close(req);
        let tcp = scheme.starts_with(b"tcp://") && scheme.len() > 6;
        let unix = scheme.starts_with(b"unix://") && scheme.len() > 7;
        if !tcp && !unix {
            error.client(req, cr::CONNECTION_ERROR, "No handler for this scheme")?;
            return Ok(false);
        }
        let timeout = (self.options.timeout_connect > 0)
            .then(|| Duration::from_secs(u64::from(self.options.timeout_connect)));
        let made = req.xport_create(
            scheme,
            Xport::Client {
                timeout,
                connect: true,
                asynchronous: false,
                persistent: self.persistent_id.clone(),
                // `post_connect_set_opt`'s `mysqlnd_set_sock_no_delay`
                // on a TCP stream, set as the socket connects.
                socket: SocketOptions {
                    tcp_nodelay: tcp,
                    ..SocketOptions::default()
                },
                context: None,
            },
        )?;
        let stream = match made {
            Ok(stream) => stream,
            Err(failure) => {
                let text = failure
                    .text
                    .unwrap_or_else(|| "Unknown error while connecting".to_owned());
                error.client(req, cr::CONNECTION_ERROR, &text)?;
                return Ok(false);
            }
        };
        // `open_tcp_or_unix`'s delete from `EG(persistent_list)` and
        // `mysqlnd_fixup_regular_list`: the stream is the connection's.
        stream.unlist(req);
        self.post_connect(req, *stream);
        self.stream = Some(stream);
        self.unix = unix;
        Ok(true)
    }

    /// The frames of a persistent connection taken up by a request, over
    /// the socket the connection kept, lent to the request in neither of
    /// its lists, with the options `post_connect_set_opt` gave it; a
    /// connection that kept none takes up holding no stream, over which
    /// every command fails as one over a dead link does.
    pub(crate) fn take_up(req: &mut Request, idle: IdleNet) -> Result<Net, Thrown> {
        let mut net = Net {
            stream: None,
            unix: idle.unix,
            ssl: idle.ssl,
            options: idle.options.into(),
            packet_no: idle.packet_no,
            envelope_no: idle.envelope_no,
            compressed: idle.compressed,
            use_compression: idle.use_compression,
            uncompressed: ReadBuffer::default(),
            cmd_buffer: idle.cmd_buffer.map(Alloc::Malloc),
            sha256_server_public_key: idle.sha256_server_public_key.map(Pstr::Malloc),
            persistent_id: idle.persistent_id,
        };
        if let Some(kept) = idle.kept {
            let stream = req.stream_lend(kept)?;
            net.post_connect(req, *stream);
            net.stream = Some(stream);
        }
        Ok(net)
    }

    /// The frames of a persistent connection let go as the request gives
    /// the connection back, nothing of the request's left on them: its
    /// socket kept by the connection ([`OwnedStream::keep`](php_ext::request::OwnedStream::keep)),
    /// out of the request, so the one list the connection waits in is its
    /// owner's, and the envelope it had not read through emptied.
    pub(crate) fn idle(self, req: &mut Request) -> IdleNet {
        IdleNet {
            kept: self.stream.and_then(|stream| stream.keep(req)),
            unix: self.unix,
            ssl: self.ssl,
            options: self.options.idle(req),
            packet_no: self.packet_no,
            envelope_no: self.envelope_no,
            compressed: self.compressed,
            use_compression: self.use_compression,
            cmd_buffer: self.cmd_buffer.map(|buffer| buffer.idle(req)),
            sha256_server_public_key: Pstr::idle_field(req, self.sha256_server_public_key),
            persistent_id: self.persistent_id,
        }
    }

    /// `post_connect_set_opt` and the error suppression php sets beside it.
    fn post_connect(&self, req: &mut Request, stream: Stream) {
        if self.options.timeout_read > 0 {
            let wait = Duration::from_secs(u64::from(self.options.timeout_read));
            stream.set_option(req, StreamOption::ReadTimeout(wait));
        }
        if self.options.net_read_buffer_size > 0 {
            let size = usize::try_from(self.options.net_read_buffer_size).unwrap_or(usize::MAX);
            stream.set_option(req, StreamOption::ChunkSize(size));
        }
        // `net_stream->flags |= PHP_STREAM_FLAG_SUPPRESS_ERRORS`: a send
        // that fails is the connection's error in mysqlnd's words, not
        // the socket's notice.
        stream.set_option(req, StreamOption::SuppressErrors(true));
    }

    /// `mysqlnd_pfc::free_contents` and `mysqlnd_vio::free_contents`:
    /// the public key's file name and the TLS options `mnd_pefree`'d.
    pub(crate) fn free_contents(&mut self, req: &mut Request) {
        Pstr::put(req, &mut self.sha256_server_public_key, None);
        let o = &mut self.options;
        for field in [
            &mut o.ssl_key,
            &mut o.ssl_cert,
            &mut o.ssl_ca,
            &mut o.ssl_capath,
            &mut o.ssl_cipher,
        ] {
            Pstr::put(req, field, None);
        }
    }

    /// `mysqlnd_vio::close_stream`.
    pub(crate) fn close(&mut self, req: &mut Request) {
        if let Some(stream) = self.stream.take() {
            stream.free(req);
        }
        self.ssl = false;
        self.uncompressed = ReadBuffer::default();
    }

    /// `mysqlnd_vio::network_read`: `out` filled, or `false` where the
    /// stream ends or fails first. Each `php_stream_read` reads into the
    /// caller's buffer, the packet's own, as php's does, so no string of
    /// the request's holds the bytes.
    fn network_read(&mut self, req: &mut Request, out: &mut [u8], stats: &mut Stats) -> bool {
        let Some(stream) = self.stream() else {
            return false;
        };
        let mut had = 0;
        while had < out.len() {
            match stream.read_into(req, &mut out[had..]) {
                Some(read) if read > 0 => had += read,
                _ => {
                    stats.add(Stat::BytesReceived, had as u64);
                    return false;
                }
            }
        }
        stats.add(Stat::BytesReceived, out.len() as u64);
        true
    }

    /// [`Net::network_read`] of `count` bytes into a buffer of their own.
    fn network_read_owned(
        &mut self,
        req: &mut Request,
        count: usize,
        stats: &mut Stats,
    ) -> Option<Vec<u8>> {
        let mut out = vec![0; count];
        self.network_read(req, &mut out, stats).then_some(out)
    }

    /// `mysqlnd_vio::network_write`: how many bytes the stream took.
    fn network_write(&mut self, req: &mut Request, bytes: &[u8]) -> usize {
        match self.stream() {
            Some(stream) => stream.write(req, bytes).unwrap_or(0),
            None => 0,
        }
    }

    /// `mysqlnd_pfc::receive`: `out` filled from the packet stream, out of
    /// the compressed envelopes where the protocol is compressed; `false`
    /// where the stream ended first. A read takes what it needs from the
    /// envelope's [`ReadBuffer`] and the next envelope as the one before
    /// runs out, so a row that straddles two envelopes, and a packet of
    /// more than `MYSQLND_MAX_PACKET_SIZE` cut into frames across many,
    /// read as php's do:
    ///
    /// ```php
    /// $l = mysqli_init();
    /// mysqli_real_connect($l, '127.0.0.1', 'root', '', 'test', 3306, null, MYSQLI_CLIENT_COMPRESS);
    /// $q = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 300) SELECT n, REPEAT(CHAR(65 + n % 26), 900 + n * 3) FROM t";
    /// $h = '';
    /// foreach (mysqli_query($l, $q)->fetch_all() as [$n, $s]) {
    ///     $h .= strlen($s) === 900 + $n * 3 ? $s[0] : '!';
    /// }
    /// $st = mysqli_prepare($l, $q);
    /// $st->execute();
    /// $st->bind_result($n, $s);
    /// $m = 0;
    /// while ($st->fetch()) {
    ///     $m += strlen($s) === 900 + $n * 3;
    /// }
    /// var_dump(strlen($h), md5($h), $m);
    /// $big = mysqli_query($l, "SELECT REPEAT('ab', 9 << 20), 1")->fetch_row();
    /// var_dump(strlen($big[0]), md5($big[0]), $big[1]);
    /// ```
    /// ```expect mysql
    /// int(300)
    /// string(32) "63105feeba001e5cf24b1a3685d5292c"
    /// int(300)
    /// int(18874368)
    /// string(32) "78a43b699de8fe639c3ece9276931286"
    /// string(1) "1"
    /// ```
    pub(crate) fn receive_into(
        &mut self,
        req: &mut Request,
        out: &mut [u8],
        stats: &mut Stats,
    ) -> bool {
        if !self.compressed {
            return self.network_read(req, out, stats);
        }
        let mut had = self.uncompressed.read(out);
        while had < out.len() {
            if !self.fill_envelope(req, stats) {
                return false;
            }
            had += self.uncompressed.read(&mut out[had..]);
        }
        true
    }

    /// [`Net::receive_into`] of `count` bytes into a buffer of their own,
    /// what php's `cmd_buffer` holds of a packet that is not a row.
    pub(crate) fn receive(
        &mut self,
        req: &mut Request,
        count: usize,
        stats: &mut Stats,
    ) -> Option<Vec<u8>> {
        let mut out = vec![0; count];
        self.receive_into(req, &mut out, stats).then_some(out)
    }

    /// The compressed half of `mysqlnd_pfc::receive` and
    /// `read_compressed_packet_from_stream_and_fill_read_buffer`: one
    /// envelope read, its number checked as php checks it, its payload
    /// inflated where its header says it was deflated.
    fn fill_envelope(&mut self, req: &mut Request, stats: &mut Stats) -> bool {
        let mut header = [0; HEADER_SIZE];
        if !self.network_read(req, &mut header, stats) {
            return false;
        }
        let size = uint3(&header);
        let number = header.get(3).copied().unwrap_or(0);
        if self.envelope_no != number {
            php_ext::raising::raise(
                req,
                php_ext::raising::Level::Warning,
                &format!(
                    "Packets out of order. Expected {} received {number}. Packet size={size}",
                    self.envelope_no
                ),
            );
        }
        self.envelope_no = self.envelope_no.wrapping_add(1);
        let mut comp_header = [0; COMPRESSED_HEADER_SIZE];
        if !self.network_read(req, &mut comp_header, stats) {
            return false;
        }
        let decompressed = uint3(&comp_header);
        let Some(payload) = self.network_read_owned(req, size, stats) else {
            return false;
        };
        if decompressed == 0 {
            self.uncompressed = ReadBuffer::new(payload);
            return true;
        }
        let mut out = Vec::with_capacity(decompressed);
        let inflated = flate2::read::ZlibDecoder::new(payload.as_slice()).read_to_end(&mut out);
        if inflated.is_err() || out.len() != decompressed {
            return false;
        }
        self.uncompressed = ReadBuffer::new(out);
        true
    }

    /// `mysqlnd_pfc::send`: `payload` cut into frames of at most
    /// `MYSQLND_MAX_PACKET_SIZE`, an empty one after a frame of exactly
    /// that size, each numbered, each in an envelope of its own where the
    /// protocol is compressed; the bytes sent, `0` where the stream took
    /// none, with php's `CR_SERVER_GONE_ERROR`; the `Err` is the memory limit's
    /// fatal, met by the error list's element.
    pub(crate) fn send(
        &mut self,
        req: &mut Request,
        payload: &[u8],
        stats: &mut Stats,
        error: &mut ErrorInfo,
    ) -> Result<usize, Thrown> {
        let mut left = payload;
        let mut packets = 1u64;
        let mut sent;
        loop {
            let take = left.len().min(MAX_PACKET_SIZE);
            let mut frame = Vec::with_capacity(take + HEADER_SIZE);
            frame.extend_from_slice(&uint3_bytes(take));
            frame.push(self.packet_no);
            frame.extend_from_slice(&left[..take]);
            if self.compressed {
                sent = if take <= MAX_PACKET_SIZE - HEADER_SIZE {
                    self.write_envelope(req, &frame)
                } else {
                    // The frame with its header would not fit one
                    // envelope: php splits it after 8192 bytes.
                    let (head, tail) = frame.split_at(8192);
                    match self.write_envelope(req, head) {
                        0 => 0,
                        _ => self.write_envelope(req, tail),
                    }
                };
            } else {
                sent = self.network_write(req, &frame);
                self.envelope_no = self.envelope_no.wrapping_add(1);
            }
            self.packet_no = self.packet_no.wrapping_add(1);
            left = &left[take..];
            packets += 1;
            if sent == 0 || (left.is_empty() && take != MAX_PACKET_SIZE) {
                break;
            }
        }
        let overhead = packets * HEADER_SIZE as u64;
        stats.add(Stat::BytesSent, payload.len() as u64 + overhead);
        stats.add(Stat::ProtocolOverheadOut, overhead);
        stats.add(Stat::PacketsSent, packets);
        if sent == 0 {
            error.client(req, cr::SERVER_GONE_ERROR, SERVER_GONE)?;
            return Ok(0);
        }
        Ok(sent)
    }

    /// `write_compressed_packet`: one envelope, deflated where zlib's
    /// `compress()` succeeds and sent as it is where it fails.
    fn write_envelope(&mut self, req: &mut Request, uncompressed: &[u8]) -> usize {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        let deflated = encoder
            .write_all(uncompressed)
            .and_then(|()| encoder.finish());
        let (body, original) = match deflated {
            Ok(body) => (body, uncompressed.len()),
            Err(_) => (uncompressed.to_vec(), 0),
        };
        let mut envelope = Vec::with_capacity(body.len() + HEADER_SIZE + COMPRESSED_HEADER_SIZE);
        envelope.extend_from_slice(&uint3_bytes(body.len()));
        envelope.push(self.envelope_no);
        envelope.extend_from_slice(&uint3_bytes(original));
        envelope.extend_from_slice(&body);
        let sent = self.network_write(req, &envelope);
        self.envelope_no = self.envelope_no.wrapping_add(1);
        sent
    }

    /// `mysqlnd_read_header`: the payload's size, with the sequence
    /// number checked; the codec's number advances where it matches (or
    /// always, under compression). One that does not match is the
    /// server's error sent outside the sequence (WL#12999, the idle
    /// timeout's `CR_CLIENT_INTERACTION_TIMEOUT`), read into `error` as
    /// php reads it, or else php's warning.
    pub(crate) fn read_header(
        &mut self,
        req: &mut Request,
        stats: &mut Stats,
        error: &mut ErrorInfo,
    ) -> Option<usize> {
        let mut header = [0; HEADER_SIZE];
        if !self.receive_into(req, &mut header, stats) {
            return None;
        }
        let size = uint3(&header);
        let number = header.get(3).copied().unwrap_or(0);
        stats.add(Stat::ProtocolOverheadIn, HEADER_SIZE as u64);
        stats.inc(Stat::PacketsReceived);
        if self.compressed || self.packet_no == number {
            self.packet_no = self.packet_no.wrapping_add(1);
            return Some(size);
        }
        if size > 0
            && let Some(body) = self.receive(req, size, stats)
            && body.first() == Some(&0xFF)
        {
            let (errno, sqlstate, text) =
                crate::wire::error_from_line(body.get(1..).unwrap_or_default());
            error.raw(errno, sqlstate, &text);
            return None;
        }
        php_ext::raising::raise(
            req,
            php_ext::raising::Level::Warning,
            &format!(
                "Packets out of order. Expected {} received {number}. Packet size={size}",
                self.packet_no
            ),
        );
        None
    }

    /// `mysqlnd_vio::enable_ssl`: a `php_stream_context` holding the TLS
    /// options set on the connection, TLS set up and its handshake made on
    /// the stream, the `php_stream_context` dropped again, the read timeout set anew.
    pub(crate) fn enable_ssl(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let Some(stream) = self.stream() else {
            return Ok(false);
        };
        let context = StreamCtx::alloc(req)?;
        let set = ssl_options(req, *context, &self.options);
        let peer = match set {
            Ok(peer) => peer,
            Err(thrown) => {
                context.give_up(req);
                return Err(thrown);
            }
        };
        self.options.ssl_verify_peer = peer;
        stream.set_ctx(req, Some(*context));
        context.give_up(req);
        let secured = stream.crypto_setup(req, TLS_CLIENT, None)
            && matches!(
                stream.crypto_enable(req, true),
                php_ext::request::Handshake::Done
            );
        stream.set_ctx(req, None);
        if !secured {
            return Ok(false);
        }
        self.ssl = true;
        if self.options.timeout_read > 0 {
            let wait = Duration::from_secs(u64::from(self.options.timeout_read));
            stream.set_option(req, StreamOption::ReadTimeout(wait));
        }
        Ok(true)
    }
}

/// The `ssl` options `mysqlnd_vio::enable_ssl` sets on the `context` it
/// allocates, in its order: the key, the certificate (and it as the key
/// where none is given), the CA file and path and the ciphers, then `verify_peer` and `verify_peer_name` as the peer check
/// it settles on, which it answers, and `allow_self_signed` where it
/// checks none.
fn ssl_options(req: &mut Request, context: StreamCtx, o: &VioOptions) -> Result<SslPeer, Thrown> {
    let mut any = false;
    let string_options: [(&[u8], &Option<Pstr>); 5] = [
        (b"local_pk", &o.ssl_key),
        (b"local_cert", &o.ssl_cert),
        (b"cafile", &o.ssl_ca),
        (b"capath", &o.ssl_capath),
        (b"ciphers", &o.ssl_cipher),
    ];
    let put = |req: &mut Request, name: &[u8], value: &[u8]| {
        let text = PhpString::new(req, value)?;
        let set = context.set_option(req, b"ssl", name, &Value::string(*text));
        text.give_up(req);
        set
    };
    for (name, value) in string_options {
        if let Some(value) = value.as_ref().map(Pstr::bytes) {
            any = true;
            put(req, name, value)?;
            if name == b"local_cert" && o.ssl_key.is_none() {
                put(req, b"local_pk", value)?;
            }
        }
    }
    let peer = match o.ssl_verify_peer {
        SslPeer::Default if any => SslPeer::Verify,
        SslPeer::Default => SslPeer::DontVerify,
        other => other,
    };
    let verify = Value::bool(peer == SslPeer::Verify);
    context.set_option(req, b"ssl", b"verify_peer", &verify)?;
    context.set_option(req, b"ssl", b"verify_peer_name", &verify)?;
    if peer == SslPeer::DontVerify {
        context.set_option(req, b"ssl", b"allow_self_signed", &Value::bool(true))?;
    }
    Ok(peer)
}

/// `MYSQLND_READ_BUFFER`: one envelope's payload as it was inflated, and
/// its `offset`, how much of it the reads have taken. A read copies what
/// it takes and moves the cursor, never the bytes after it; the envelope
/// is given back whole once read through, as `mysqlnd_pfc::receive` frees
/// php's.
#[derive(Default, php_ext::abi::EngineState)]
struct ReadBuffer {
    data: Vec<u8>,
    offset: usize,
}

impl ReadBuffer {
    /// `mysqlnd_create_read_buffer` over an envelope's bytes.
    fn new(data: Vec<u8>) -> Self {
        Self { data, offset: 0 }
    }

    /// `read` of what is left, up to `out`'s length, into `out`: how many
    /// bytes it took. The buffer read through is freed, `free_buffer`.
    fn read(&mut self, out: &mut [u8]) -> usize {
        let left = self.data.get(self.offset..).unwrap_or_default();
        let take = out.len().min(left.len());
        out[..take].copy_from_slice(&left[..take]);
        self.offset += take;
        if self.offset == self.data.len() {
            *self = Self::default();
        }
        take
    }
}

/// `uint3korr`.
fn uint3(bytes: &[u8]) -> usize {
    let b = |i: usize| usize::from(bytes.get(i).copied().unwrap_or(0));
    b(0) | (b(1) << 8) | (b(2) << 16)
}

/// `int3store`.
fn uint3_bytes(n: usize) -> [u8; 3] {
    let n = u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes();
    [n[0], n[1], n[2]]
}
