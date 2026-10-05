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
    Request, SocketOptions, Stream, StreamCtx, StreamOption, Streams as _, Xport,
};
use php_ext::values::{PhpString, Value};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SslPeer {
    #[default]
    Default,
    Verify,
    DontVerify,
}

/// `MYSQLND_VIO_DATA`'s options.
#[derive(Debug, Clone, Default)]
pub(crate) struct VioOptions {
    pub(crate) timeout_connect: u32,
    pub(crate) timeout_read: u32,
    pub(crate) net_read_buffer_size: u32,
    pub(crate) ssl_key: Option<Vec<u8>>,
    pub(crate) ssl_cert: Option<Vec<u8>>,
    pub(crate) ssl_ca: Option<Vec<u8>>,
    pub(crate) ssl_capath: Option<Vec<u8>>,
    pub(crate) ssl_cipher: Option<Vec<u8>>,
    pub(crate) ssl_passphrase: Option<Vec<u8>>,
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

/// `MYSQLND_VIO` and `MYSQLND_PFC` together: the stream, the frame
/// codec's sequence numbers and the compressed protocol's read buffer.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // php's flags, `vio->data->ssl` and the frame codec's two
pub(crate) struct Net {
    stream: Option<Stream>,
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
    /// `pfc->data->uncompressed_data`: what an envelope held that no read
    /// took yet.
    uncompressed: Vec<u8>,
    /// `pfc->cmd_buffer.length`.
    pub(crate) cmd_buffer: usize,
    /// `pfc->data->sha256_server_public_key`.
    pub(crate) sha256_server_public_key: Option<Vec<u8>>,
    /// The `persistent_id` a persistent connection's stream is opened
    /// under, php's `"%p"` of the `MYSQLND_VIO`: the key the engine keeps
    /// the socket by between the requests that take the connection up.
    pub(crate) persistent_id: Option<Vec<u8>>,
}

impl Net {
    /// Whether a stream is open, `has_valid_stream`.
    pub(crate) fn open(&self) -> bool {
        self.stream.is_some()
    }

    /// The stream, for what reaches it directly: `LOCAL INFILE`'s and a
    /// connection's resource.
    pub(crate) fn stream(&self) -> Option<Stream> {
        self.stream
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
            error.client(cr::CONNECTION_ERROR, "No handler for this scheme");
            return Ok(false);
        }
        let timeout = (self.options.timeout_connect > 0)
            .then(|| Duration::from_secs(u64::from(self.options.timeout_connect)));
        let made = req.xport_create(
            scheme,
            Xport::Client {
                timeout,
                connect: true,
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
                let text = if failure.text.is_empty() {
                    "Unknown error while connecting".to_owned()
                } else {
                    failure.text
                };
                error.client(cr::CONNECTION_ERROR, &text);
                return Ok(false);
            }
        };
        self.stream = Some(stream);
        self.unix = unix;
        self.post_connect(req, stream);
        Ok(true)
    }

    /// The stream of a persistent connection taken up by a request: the
    /// socket the engine kept under the connection's `persistent_id`, a
    /// live one, with the options `post_connect_set_opt` gave it; a stream
    /// holding no socket where none was kept, over which every command
    /// fails as one over a dead link does.
    pub(crate) fn take_up(&mut self, req: &mut Request, scheme: &[u8]) -> Result<(), Thrown> {
        self.stream = None;
        let made = req.xport_create(
            scheme,
            Xport::Client {
                timeout: None,
                connect: false,
                persistent: self.persistent_id.clone(),
                socket: SocketOptions::default(),
                context: None,
            },
        )?;
        if let Ok(stream) = made {
            self.stream = Some(stream);
            self.post_connect(req, stream);
        }
        Ok(())
    }

    /// The stream of a persistent connection let go as the request gives
    /// the connection back: the request's end keeps its socket under the
    /// connection's `persistent_id`, and nothing of the request's is left.
    pub(crate) fn give_back(&mut self) {
        self.stream = None;
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

    /// `mysqlnd_vio::close_stream`.
    pub(crate) fn close(&mut self, req: &mut Request) {
        if let Some(stream) = self.stream.take() {
            stream.free(req);
        }
        self.ssl = false;
        self.uncompressed.clear();
    }

    /// `mysqlnd_vio::network_read`: exactly `count` bytes, or none where
    /// the stream ends or fails first.
    fn network_read(
        &mut self,
        req: &mut Request,
        count: usize,
        stats: &mut Stats,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let Some(stream) = self.stream else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            match stream.read(req, count - out.len())? {
                Some(chunk) if !chunk.is_empty() => out.extend_from_slice(chunk.as_bytes()),
                _ => {
                    stats.add(Stat::BytesReceived, out.len() as u64);
                    return Ok(None);
                }
            }
        }
        stats.add(Stat::BytesReceived, count as u64);
        Ok(Some(out))
    }

    /// `mysqlnd_vio::network_write`: how many bytes the stream took.
    fn network_write(&mut self, req: &mut Request, bytes: &[u8]) -> usize {
        match self.stream {
            Some(stream) => stream.write(req, bytes).unwrap_or(0),
            None => 0,
        }
    }

    /// `mysqlnd_pfc::receive`: `count` bytes of the packet stream, out of
    /// the compressed envelopes where the protocol is compressed.
    pub(crate) fn receive(
        &mut self,
        req: &mut Request,
        count: usize,
        stats: &mut Stats,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        if !self.compressed {
            return self.network_read(req, count, stats);
        }
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            if self.uncompressed.is_empty() && !self.fill_envelope(req, stats)? {
                return Ok(None);
            }
            let take = (count - out.len()).min(self.uncompressed.len());
            out.extend(self.uncompressed.drain(..take));
        }
        Ok(Some(out))
    }

    /// The compressed half of `mysqlnd_pfc::receive` and
    /// `read_compressed_packet_from_stream_and_fill_read_buffer`: one
    /// envelope read, its number checked as php checks it, its payload
    /// inflated where its header says it was deflated.
    fn fill_envelope(&mut self, req: &mut Request, stats: &mut Stats) -> Result<bool, Thrown> {
        let Some(header) = self.network_read(req, HEADER_SIZE, stats)? else {
            return Ok(false);
        };
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
        let Some(comp_header) = self.network_read(req, COMPRESSED_HEADER_SIZE, stats)? else {
            return Ok(false);
        };
        let decompressed = uint3(&comp_header);
        let Some(payload) = self.network_read(req, size, stats)? else {
            return Ok(false);
        };
        if decompressed == 0 {
            self.uncompressed = payload;
            return Ok(true);
        }
        let mut out = Vec::with_capacity(decompressed);
        let inflated = flate2::read::ZlibDecoder::new(payload.as_slice()).read_to_end(&mut out);
        if inflated.is_err() || out.len() != decompressed {
            return Ok(false);
        }
        self.uncompressed = out;
        Ok(true)
    }

    /// `mysqlnd_pfc::send`: `payload` cut into frames of at most
    /// `MYSQLND_MAX_PACKET_SIZE`, an empty one after a frame of exactly
    /// that size, each numbered, each in an envelope of its own where the
    /// protocol is compressed; the bytes sent, `0` where the stream took
    /// none, with php's `CR_SERVER_GONE_ERROR`.
    pub(crate) fn send(
        &mut self,
        req: &mut Request,
        payload: &[u8],
        stats: &mut Stats,
        error: &mut ErrorInfo,
    ) -> usize {
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
            error.client(cr::SERVER_GONE_ERROR, SERVER_GONE);
            return 0;
        }
        sent
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
    ) -> Result<Option<usize>, Thrown> {
        let Some(header) = self.receive(req, HEADER_SIZE, stats)? else {
            return Ok(None);
        };
        let size = uint3(&header);
        let number = header.get(3).copied().unwrap_or(0);
        stats.add(Stat::ProtocolOverheadIn, HEADER_SIZE as u64);
        stats.inc(Stat::PacketsReceived);
        if self.compressed || self.packet_no == number {
            self.packet_no = self.packet_no.wrapping_add(1);
            return Ok(Some(size));
        }
        if size > 0
            && let Some(body) = self.receive(req, size, stats)?
            && body.first() == Some(&0xFF)
        {
            let (errno, sqlstate, text) =
                crate::wire::error_from_line(body.get(1..).unwrap_or_default());
            error.raw(errno, sqlstate, &text);
            return Ok(None);
        }
        php_ext::raising::raise(
            req,
            php_ext::raising::Level::Warning,
            &format!(
                "Packets out of order. Expected {} received {number}. Packet size={size}",
                self.packet_no
            ),
        );
        Ok(None)
    }

    /// `mysqlnd_vio::enable_ssl`: a `php_stream_context` holding the TLS
    /// options set on the connection, TLS set up and its handshake made on
    /// the stream, the `php_stream_context` dropped again, the read timeout set anew.
    pub(crate) fn enable_ssl(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let Some(stream) = self.stream else {
            return Ok(false);
        };
        let o = self.options.clone();
        let context = StreamCtx::alloc(req)?;
        let set = ssl_options(req, context, &o);
        let peer = match set {
            Ok(peer) => peer,
            Err(thrown) => {
                req.release(context.resource().as_value());
                return Err(thrown);
            }
        };
        self.options.ssl_verify_peer = peer;
        stream.set_ctx(req, Some(context));
        req.release(context.resource().as_value());
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
/// where none is given), the CA file and path, the passphrase and the
/// ciphers, then `verify_peer` and `verify_peer_name` as the peer check
/// it settles on, which it answers, and `allow_self_signed` where it
/// checks none.
fn ssl_options(req: &mut Request, context: StreamCtx, o: &VioOptions) -> Result<SslPeer, Thrown> {
    let mut any = false;
    let string_options: [(&[u8], &Option<Vec<u8>>); 6] = [
        (b"local_pk", &o.ssl_key),
        (b"local_cert", &o.ssl_cert),
        (b"cafile", &o.ssl_ca),
        (b"capath", &o.ssl_capath),
        (b"passphrase", &o.ssl_passphrase),
        (b"ciphers", &o.ssl_cipher),
    ];
    let put = |req: &mut Request, name: &[u8], value: &[u8]| {
        let text = Value::string(PhpString::new(req, value)?);
        let set = context.set_option(req, b"ssl", name, &text);
        req.release(text);
        set
    };
    for (name, value) in string_options {
        if let Some(value) = value {
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
