//! `Z/ext/openssl/xp_ssl.c`: the crypto layer of the `ssl`, `tls`,
//! `tlsv1.0` to `tlsv1.3` and `tcp` socket `transport`s openssl's `MINIT`
//! registers, over the engine's `tcp` socket (design §4.4's streams facet,
//! [`Crypto`]). The engine connects, accepts and waits; this file sets
//! up an `SSL_CTX` and an `SSL` over the socket as
//! `php_openssl_setup_crypto` does, runs the handshake, verifies the peer
//! as `php_openssl_apply_peer_verification_policy` does, and reads and
//! writes through the `SSL` once active, as `php_openssl_sockop_io` does.
//!
//! State is typed and decoded once: the crypto method a `transport` or a
//! call names is a [`CryptoMethod`], the `ssl` options of the stream's
//! `context` are one [`SslOptions`] read at setup, which every step after
//! reads, and whether the handshake ran, the `SSL` exists and the layer
//! is active is the layer's own.

use std::any::Any;
use std::ffi::{CString, c_int};
use std::ops::ControlFlow;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use php_ext::call::call;
use php_ext::coerce::{to_callable, to_long, truthy, with_string};
use php_ext::raising::{Level, Thrown, docref, leave_pending, raise, throw_error};
use php_ext::request::{
    Crypto, CryptoSocket, Handshake, OwnedStream as _, Readiness, Request, Stream, StreamCtx,
    Streams as _, XportFactory,
};
use php_ext::values::walk::Hold;
use php_ext::values::{Key, Owner, PhpArray, Value, ValueRef, walk};
use php_ext_standard::UriParser;

use crate::decode::{Origin, check_path};
use crate::ffi::ssl::{Ctx, Setup, Tls};
use crate::ffi::{self, Certificate};
use crate::x509::object_of;

/// `STREAM_CRYPTO_IS_CLIENT`, the low bit of a crypto method.
const IS_CLIENT: i64 = 1;

/// The protocol bits of `STREAM_CRYPTO_METHOD_*`, lowest first. `SSLv2`
/// is never built.
const SSL3: i64 = 1 << 2;
const TLS1_0: i64 = 1 << 3;
const TLS1_1: i64 = 1 << 4;
const TLS1_2: i64 = 1 << 5;
const TLS1_3: i64 = 1 << 6;

/// `HAVE_SSL3`: the headers this build compiled against leave
/// `OPENSSL_NO_SSL3` undefined, as OpenSSL 4's do and OpenSSL 3's do not.
const HAVE_SSL3: bool = crate::header::HAVE_SSL3;

/// The protocols from `PHP_OPENSSL_MIN_PROTO_VERSION` to
/// `PHP_OPENSSL_MAX_PROTO_VERSION`, lowest first: `SSLv3` first where the
/// build has it, else TLS 1.0.
const PROTOCOLS: &[i64] = if HAVE_SSL3 {
    &[SSL3, TLS1_0, TLS1_1, TLS1_2, TLS1_3]
} else {
    &[TLS1_0, TLS1_1, TLS1_2, TLS1_3]
};

/// `STREAM_CRYPTO_METHOD_TLS_ANY_CLIENT`.
const TLS_ANY_CLIENT: i64 = TLS1_0 | TLS1_1 | TLS1_2 | TLS1_3 | IS_CLIENT;

/// `SSL3_VERSION` and `TLS1_VERSION` to `TLS1_3_VERSION`, what
/// `SSL_version` answers.
const SSL3_VERSION: c_int = 0x300;
const TLS1_VERSION: c_int = 0x301;
const TLS1_1_VERSION: c_int = 0x302;
const TLS1_2_VERSION: c_int = 0x303;
const TLS1_3_VERSION: c_int = 0x304;

/// `SSL_ERROR_*`, as `SSL_get_error` answers.
const SSL_ERROR_NONE: c_int = 0;
const SSL_ERROR_WANT_READ: c_int = 2;
const SSL_ERROR_WANT_WRITE: c_int = 3;
const SSL_ERROR_SYSCALL: c_int = 5;
const SSL_ERROR_ZERO_RETURN: c_int = 6;

/// `SSL_OP_*` php sets and clears.
const SSL_OP_ALL: u64 = 0x8000_0850;
const SSL_OP_DONT_INSERT_EMPTY_FRAGMENTS: u64 = 0x0000_0800;
const SSL_OP_IGNORE_UNEXPECTED_EOF: u64 = 0x0000_0080;
const SSL_OP_NO_TICKET: u64 = 0x0000_4000;
const SSL_OP_NO_COMPRESSION: u64 = 0x0002_0000;
const SSL_OP_CIPHER_SERVER_PREFERENCE: u64 = 0x0040_0000;
/// `SSL_OP_SINGLE_DH_USE`, which OpenSSL 3 defines as nothing.
const SSL_OP_SINGLE_DH_USE: u64 = 0;

/// `SSL_MODE_*` php sets.
const SSL_MODE_ENABLE_PARTIAL_WRITE: std::ffi::c_long = 0x1;
const SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER: std::ffi::c_long = 0x2;
const SSL_MODE_RELEASE_BUFFERS: std::ffi::c_long = 0x10;

/// `X509_V_OK` and `X509_V_ERR_DEPTH_ZERO_SELF_SIGNED_CERT`.
const X509_V_OK: std::ffi::c_long = 0;
const X509_V_ERR_DEPTH_ZERO_SELF_SIGNED_CERT: std::ffi::c_long = 18;

/// `EAGAIN` on this host.
const EAGAIN: i32 = 35;

/// `SSL_R_NO_SHARED_CIPHER`, the reason `php_openssl_handle_ssl_error`
/// words apart.
const SSL_R_NO_SHARED_CIPHER: c_int = 193;

/// `OPENSSL_DEFAULT_STREAM_VERIFY_DEPTH`.
const DEFAULT_VERIFY_DEPTH: u64 = 9;

/// `X509_V_ERR_DEPTH_ZERO_SELF_SIGNED_CERT` as the verify callback reads
/// it.
const DEPTH_ZERO_SELF_SIGNED: c_int = 18;

/// `OPENSSL_DEFAULT_RENEG_LIMIT` and `OPENSSL_DEFAULT_RENEG_WINDOW`.
const DEFAULT_RENEG_LIMIT: i64 = 2;
const DEFAULT_RENEG_WINDOW: i64 = 300;

/// The `transports` openssl's `MINIT` registers, in its order: `ssl`,
/// `sslv3` where the build has it, `tls`, `tlsv1.0` to `tlsv1.3`, then
/// `tcp` over `standard`'s, each labelled as `php_openssl_socket_ops` is.
///
/// ```php
/// echo implode(',', stream_get_transports()), "\n";
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $c = stream_socket_client('tcp://' . stream_socket_get_name($s, false));
/// var_dump(stream_get_meta_data($c)['stream_type'], stream_socket_enable_crypto($c, false));
/// var_dump(stream_socket_client('udp://127.0.0.1:9') !== false);
/// ```
/// ```expect
/// tcp,udp,unix,udg,ssl,sslv3,tls,tlsv1.0,tlsv1.1,tlsv1.2,tlsv1.3
/// string(14) "tcp_socket/ssl"
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn transports() -> Vec<XportFactory> {
    [
        "ssl", "sslv3", "tls", "tlsv1.0", "tlsv1.1", "tlsv1.2", "tlsv1.3", "tcp",
    ]
    .into_iter()
    .filter(|&name| HAVE_SSL3 || name != "sslv3")
    .map(|name| XportFactory {
        name,
        label: "tcp_socket/ssl",
        layer: factory,
    })
    .collect()
}

/// A crypto method, `php_stream_xport_crypt_method_t` decoded: whether
/// the socket is the client, and the protocol bits it allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CryptoMethod {
    client: bool,
    protocols: i64,
}

impl CryptoMethod {
    fn of(bits: i64) -> CryptoMethod {
        CryptoMethod {
            client: bits & IS_CLIENT != 0,
            protocols: bits & !IS_CLIENT,
        }
    }

    /// php's bits again.
    fn bits(self) -> i64 {
        self.protocols | i64::from(self.client)
    }

    /// `php_openssl_get_proto_version_flags`: the protocols between
    /// `min` and `max`, each `0` for what the method's own bits say.
    fn bounded(self, min: i64, max: i64) -> i64 {
        let min = if min == 0 {
            min_flag(self.protocols)
        } else {
            min
        };
        let max = if max == 0 {
            max_flag(self.protocols)
        } else {
            max
        };
        let mut flags = self.protocols;
        for &version in PROTOCOLS {
            if version >= min && version <= max {
                flags |= version;
            } else {
                flags &= !version;
            }
        }
        flags
    }
}

/// `php_openssl_get_min_proto_version_flag`.
fn min_flag(flags: i64) -> i64 {
    PROTOCOLS
        .iter()
        .copied()
        .find(|v| flags & v != 0)
        .unwrap_or(TLS1_3)
}

/// `php_openssl_get_max_proto_version_flag`.
fn max_flag(flags: i64) -> i64 {
    PROTOCOLS
        .iter()
        .rev()
        .copied()
        .find(|v| flags & v != 0)
        .unwrap_or(TLS1_3)
}

/// `php_openssl_map_proto_version`.
fn proto_version(flag: i64) -> c_int {
    match flag {
        TLS1_3 => TLS1_3_VERSION,
        TLS1_1 => TLS1_1_VERSION,
        TLS1_0 => TLS1_VERSION,
        SSL3 if HAVE_SSL3 => SSL3_VERSION,
        _ => TLS1_2_VERSION,
    }
}

/// One call of `SSL_connect`, `SSL_accept`, `SSL_read`, `SSL_write` or
/// `SSL_peek`: what it answered and `errno` right after, which
/// `php_openssl_handle_ssl_error` reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Io {
    pub(crate) ret: c_int,
    pub(crate) errno: i32,
}

/// The cipher `SSL_get_current_cipher` names: its name, its bits and the
/// protocol version it came with.
pub(crate) struct Cipher {
    pub(crate) name: Vec<u8>,
    pub(crate) bits: c_int,
    pub(crate) version: Vec<u8>,
}

/// One name of a certificate's subjectAltName extension as
/// `php_openssl_matches_san_list` reads it: a DNS name, or an IP
/// address's bytes.
pub(crate) enum San {
    Dns(Vec<u8>),
    Ip(Vec<u8>),
}

/// The renegotiation bucket, `php_openssl_handshake_bucket_t`: when the
/// last handshake began, the limit and the window it is held to, the
/// tokens, whether the limit was passed (`should_close`), and the news of
/// a pass not read yet.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Reneg {
    prev_handshake: i64,
    limit: i64,
    window: i64,
    tokens: f32,
    should_close: bool,
    exceeded: bool,
}

/// What libssl's callbacks read, which [`Setup`] and [`Tls`] own for
/// them: `allow_self_signed` and `verify_depth`, the `passphrase`, a server's ALPN list
/// in wire form and SNI certificates, and the renegotiation bucket.
pub(crate) struct Hooks {
    allow_self_signed: bool,
    verify_depth: u64,
    pub(crate) passphrase: Option<Vec<u8>>,
    pub(crate) alpn: Vec<u8>,
    pub(crate) sni: Vec<(CString, Ctx)>,
    reneg: Option<Reneg>,
}

impl Default for Hooks {
    fn default() -> Hooks {
        Hooks {
            allow_self_signed: false,
            verify_depth: DEFAULT_VERIFY_DEPTH,
            passphrase: None,
            alpn: Vec::new(),
            sni: Vec::new(),
            reneg: None,
        }
    }
}

impl Hooks {
    /// `verify_callback`: a self-signed leaf passes where
    /// `allow_self_signed` says, and a certificate deeper than
    /// `verify_depth` fails; the verdict, and whether the chain is to be
    /// marked too long.
    pub(crate) fn verify(&self, preverify_ok: c_int, err: c_int, depth: c_int) -> (c_int, bool) {
        let mut ok = preverify_ok;
        if err == DEPTH_ZERO_SELF_SIGNED && self.allow_self_signed {
            ok = 1;
        }
        let too_deep = u64::from_ne_bytes(i64::from(depth).to_ne_bytes()) > self.verify_depth;
        (if too_deep { 0 } else { ok }, too_deep)
    }

    /// `php_openssl_limit_handshake_reneg`: the first handshake marks the
    /// time, and each one after spends a token the window gave back, the
    /// bucket passed once it holds more than the limit.
    pub(crate) fn handshake_started(&mut self) {
        let Some(bucket) = self.reneg.as_mut() else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        if bucket.prev_handshake == 0 {
            bucket.prev_handshake = now;
            return;
        }
        let elapsed = now - bucket.prev_handshake;
        bucket.prev_handshake = now;
        let rate = bucket.limit.checked_div(bucket.window).unwrap_or(0);
        #[allow(clippy::cast_precision_loss)] // php's `float tokens`
        let spent = elapsed.saturating_mul(rate) as f32;
        bucket.tokens = (bucket.tokens - spent).max(0.0) + 1.0;
        #[allow(clippy::cast_precision_loss)] // php compares the float to the long
        if bucket.tokens > bucket.limit as f32 {
            bucket.should_close = true;
            bucket.exceeded = true;
        }
    }
}

/// `peer_fingerprint` as `php_openssl_apply_peer_verification_policy`
/// sorts it: a string, an array of `algo => fingerprint` pairs (`None`
/// for one that is empty or holds another shape), or anything else.
enum Fingerprint {
    Text(Vec<u8>),
    Pairs(Option<Vec<(Vec<u8>, Vec<u8>)>>),
    Other,
}

/// One entry of `SNI_server_certs`: the host name its key gives, `None`
/// for an integer key, and what it maps to.
struct SniEntry {
    name: Option<Vec<u8>>,
    certs: SniCerts,
}

/// What an `SNI_server_certs` entry maps a host to: `local_cert` and
/// `local_pk`, each `None` where the array lacks it; a path to both; or
/// anything else.
enum SniCerts {
    Pair {
        local_cert: Option<Vec<u8>>,
        local_pk: Option<Vec<u8>>,
    },
    Path(Vec<u8>),
    Other,
}

/// `SNI_server_certs` as a server's setup reads it.
enum SniOption {
    NotArray,
    Entries(Vec<SniEntry>),
}

/// The `ssl` options of a stream's `context`, decoded once as setup
/// begins, each read as `xp_ssl.c` reads it: a yes or no by `zend_is_true`, a
/// number by `zval_get_long`, a string by `try_convert_to_string`, and
/// absent where the `context` has no such option. Every later step reads
/// this and not the `context`.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // php's options, one flag per name
struct SslOptions {
    verify_peer: Option<bool>,
    verify_peer_name: Option<bool>,
    allow_self_signed: bool,
    cafile: Option<Vec<u8>>,
    capath: Option<Vec<u8>>,
    local_cert: Option<Vec<u8>>,
    local_pk: Option<Vec<u8>>,
    passphrase: Option<Vec<u8>>,
    peer_name: Option<Vec<u8>>,
    verify_depth: Option<i64>,
    ciphers: Option<Vec<u8>>,
    capture_peer_cert: bool,
    capture_peer_cert_chain: bool,
    sni_enabled: Option<bool>,
    sni_server_certs: Option<SniOption>,
    disable_compression: Option<bool>,
    peer_fingerprint: Option<Fingerprint>,
    security_level: Option<i64>,
    min_proto_version: i64,
    max_proto_version: i64,
    no_ticket: bool,
    alpn_protocols: Option<Vec<u8>>,
    reneg_limit: Option<i64>,
    reneg_window: Option<i64>,
    dh_param: Option<Vec<u8>>,
    single_dh_use: Option<bool>,
    honor_cipher_order: Option<bool>,
    rsa_key_size: bool,
}

/// `php_stream_context_get_option(context, "ssl", name)`.
fn option(req: &Request, context: Option<StreamCtx>, name: &[u8]) -> Option<Value> {
    context.and_then(|context| context.option(req, b"ssl", name))
}

impl SslOptions {
    /// Every option the layer reads, off `context`.
    fn read(req: &mut Request, context: Option<StreamCtx>) -> Result<SslOptions, Thrown> {
        let mut o = SslOptions::default();
        let Some(context) = context else {
            return Ok(o);
        };
        let ctx = Some(context);
        let flag = |req: &mut Request, name: &[u8]| -> Result<Option<bool>, Thrown> {
            option(req, ctx, name).map(|v| truthy(req, &v)).transpose()
        };
        let long = |req: &mut Request, name: &[u8]| -> Result<Option<i64>, Thrown> {
            match option(req, ctx, name) {
                Some(v) => Ok(Some(to_long(req, &v)?)),
                None => Ok(None),
            }
        };
        let text = |req: &mut Request, name: &[u8]| -> Result<Option<Vec<u8>>, Thrown> {
            match option(req, ctx, name) {
                Some(v) => Ok(Some(with_string(req, &v, <[u8]>::to_vec)?)),
                None => Ok(None),
            }
        };
        o.min_proto_version = long(req, b"min_proto_version")?.unwrap_or(0);
        o.max_proto_version = long(req, b"max_proto_version")?.unwrap_or(0);
        o.no_ticket = flag(req, b"no_ticket")?.unwrap_or(false);
        o.disable_compression = flag(req, b"disable_compression")?;
        o.verify_peer = flag(req, b"verify_peer")?;
        o.verify_peer_name = flag(req, b"verify_peer_name")?;
        o.cafile = text(req, b"cafile")?;
        o.capath = text(req, b"capath")?;
        o.passphrase = text(req, b"passphrase")?;
        o.ciphers = text(req, b"ciphers")?;
        o.security_level = long(req, b"security_level")?;
        o.alpn_protocols = text(req, b"alpn_protocols")?;
        o.local_cert = text(req, b"local_cert")?;
        o.local_pk = text(req, b"local_pk")?;
        o.rsa_key_size = option(req, ctx, b"rsa_key_size").is_some();
        o.dh_param = text(req, b"dh_param")?;
        o.single_dh_use = flag(req, b"single_dh_use")?;
        o.honor_cipher_order = flag(req, b"honor_cipher_order")?;
        o.sni_enabled = flag(req, b"SNI_enabled")?;
        o.sni_server_certs = match option(req, ctx, b"SNI_server_certs") {
            Some(v) => Some(sni_option(req, &v)?),
            None => None,
        };
        o.reneg_limit = long(req, b"reneg_limit")?;
        o.reneg_window = long(req, b"reneg_window")?;
        o.peer_name = text(req, b"peer_name")?;
        o.capture_peer_cert = flag(req, b"capture_peer_cert")?.unwrap_or(false);
        o.capture_peer_cert_chain = flag(req, b"capture_peer_cert_chain")?.unwrap_or(false);
        o.peer_fingerprint = option(req, ctx, b"peer_fingerprint").map(|v| fingerprint(req, &v));
        o.allow_self_signed = flag(req, b"allow_self_signed")?.unwrap_or(false);
        o.verify_depth = long(req, b"verify_depth")?;
        Ok(o)
    }
}

/// `peer_fingerprint` sorted.
fn fingerprint(req: &Request, value: &Value) -> Fingerprint {
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Str(text) => Fingerprint::Text(text.as_bytes().to_vec()),
        ValueRef::Array(array) => {
            let mut pairs = Vec::new();
            for (key, item) in walk::rows(req, array) {
                let item = item.deref();
                match (key, item.as_ref()) {
                    (Key::Str(algo), ValueRef::Str(expected)) => {
                        pairs.push((algo.as_bytes().to_vec(), expected.as_bytes().to_vec()));
                    }
                    _ => return Fingerprint::Pairs(None),
                }
            }
            Fingerprint::Pairs((!pairs.is_empty()).then_some(pairs))
        }
        _ => Fingerprint::Other,
    }
}

/// `SNI_server_certs` sorted, each string converted as
/// `zval_try_get_string` converts it.
fn sni_option(req: &mut Request, value: &Value) -> Result<SniOption, Thrown> {
    let value = value.deref();
    let Some(array) = value.as_array() else {
        return Ok(SniOption::NotArray);
    };
    let mut entries = Vec::new();
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(val), key_index, key, current)`
    // (`Z/ext/openssl/xp_ssl.c:1374`), no owner of the table taken: the
    // stream context holds it while `zval_try_get_string` runs
    // `__toString`, as it does over each pair's rows.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let name = match row.key() {
            Key::Str(name) => Some(name.as_bytes().to_vec()),
            Key::Int(_) => None,
        };
        let item = row.value().deref();
        let certs = match item.as_ref() {
            ValueRef::Array(pair) => {
                let mut local_cert = None;
                let mut local_pk = None;
                walk::foreach(req, pair, Hold::None, |req, row| -> walk::Step {
                    match row.key() {
                        Key::Str(k) if k.as_bytes() == b"local_cert" => {
                            local_cert = Some(with_string(req, row.value(), <[u8]>::to_vec)?);
                        }
                        Key::Str(k) if k.as_bytes() == b"local_pk" => {
                            local_pk = Some(with_string(req, row.value(), <[u8]>::to_vec)?);
                        }
                        _ => {}
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
                SniCerts::Pair {
                    local_cert,
                    local_pk,
                }
            }
            ValueRef::Str(path) => SniCerts::Path(path.as_bytes().to_vec()),
            _ => SniCerts::Other,
        };
        entries.push(SniEntry { name, certs });
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(SniOption::Entries(entries))
}

/// How far a layer's handshake came: not begun; begun, the connect or
/// accept state set (`state_set`); or done, the layer active
/// (`ssl_active`), which shutting the layer takes back to begun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Fresh,
    Begun,
    Active,
}

/// `php_openssl_netstream_data_t` less the socket: the TLS once set up,
/// the options it was set up under, the handshake's own timeout, the
/// method a connect or an accept enables it with (`enable_on_connect`
/// and `method`), which side it is, how far its handshake came, and the
/// host a client verifies (`url_name`).
pub(crate) struct Layer {
    tls: Option<Tls>,
    options: SslOptions,
    connect_timeout: Option<Duration>,
    on_connect: Option<CryptoMethod>,
    is_client: bool,
    phase: Phase,
    url_name: Option<Vec<u8>>,
}

impl Drop for Layer {
    /// `php_openssl_sockop_close`: `SSL_shutdown` where active, then the
    /// `SSL` and its `context` freed.
    fn drop(&mut self) {
        if self.phase == Phase::Active
            && let Some(tls) = &self.tls
        {
            tls.shutdown();
        }
    }
}

/// `php_openssl_ssl_socket_factory` less the socket: the `transport`'s name
/// decides whether a connect enables crypto and with what method, the
/// `context`'s `crypto_method` standing in for `ssl`'s and `tls`'s; the
/// timeout is the handshake's; the host of the address, by the
/// `context`'s `ssl` URI parser, is what a client verifies.
fn factory(
    req: &mut Request,
    proto: &str,
    target: &[u8],
    timeout: Option<Duration>,
    context: Option<StreamCtx>,
) -> Result<Box<dyn Crypto>, Thrown> {
    let on_connect = match proto {
        "ssl" | "tls" => Some(crypto_method(req, context, TLS_ANY_CLIENT)?),
        "sslv3" => Some(SSL3 | IS_CLIENT),
        "tlsv1.0" => Some(TLS1_0 | IS_CLIENT),
        "tlsv1.1" => Some(TLS1_1 | IS_CLIENT),
        "tlsv1.2" => Some(TLS1_2 | IS_CLIENT),
        "tlsv1.3" => Some(TLS1_3 | IS_CLIENT),
        _ => None,
    };
    let url_name = url_name(req, target, context)?;
    Ok(Box::new(Layer {
        tls: None,
        options: SslOptions::default(),
        connect_timeout: timeout,
        on_connect: on_connect.map(CryptoMethod::of),
        is_client: false,
        phase: Phase::Fresh,
        url_name,
    }))
}

/// `php_openssl_get_crypto_method`: the `context`'s `crypto_method` with
/// the client bit, or the default.
fn crypto_method(
    req: &mut Request,
    context: Option<StreamCtx>,
    default: i64,
) -> Result<i64, Thrown> {
    match option(req, context, b"crypto_method") {
        Some(value) => Ok(to_long(req, &value)? | IS_CLIENT),
        None => Ok(default),
    }
}

/// `php_openssl_get_url_name`: the host of `target` as the `context`'s
/// `ssl` URI parser reads it, trailing dots taken off; a parser the
/// `context` names wrongly is php's `ValueError`.
fn url_name(
    req: &mut Request,
    target: &[u8],
    context: Option<StreamCtx>,
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(parser) = UriParser::of_ctx(req, context, b"ssl")? else {
        let text = format!(
            "{}(): Provided stream context has invalid value for the \"uri_parser_class\" option",
            req.frames().active_function_name()
        );
        return Err(throw_error(req, "ValueError", &text));
    };
    let Some(uri) = parser.parse(req, target)? else {
        return Ok(None);
    };
    let Some(mut host) = uri.host else {
        return Ok(None);
    };
    while host.last() == Some(&b'.') {
        host.pop();
    }
    Ok((!host.is_empty()).then_some(host))
}

/// `strerror` of an `errno`, as `php_socket_strerror` words it.
fn strerror(errno: i32) -> String {
    let text = std::io::Error::from_raw_os_error(errno).to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// Bytes as C's `%s` prints them: up to the first NUL.
fn c_str(bytes: &[u8]) -> String {
    String::from_utf8_lossy(php_ext::coerce::c_str(bytes)).into_owned()
}

/// A warning in `php_error_docref`'s words.
fn warn(req: &mut Request, text: &str) {
    docref(req, Level::Warning, text);
}

/// What `php_openssl_handle_ssl_error` decided: whether to try again,
/// and whether it left `errno` at `EAGAIN`, which its callers read.
#[derive(Debug, Clone, Copy)]
struct Handled {
    retry: bool,
    again: bool,
}

/// `php_openssl_handle_ssl_error`: what one call's failure means, with
/// php's warning for each failure it words. `retry_want` is the retry a
/// `WANT_READ` or `WANT_WRITE` gets: the handshake's `is_init`, or the
/// socket blocking. A peer that closed at a system call ends the stream,
/// which `eof` is set for.
fn handle_ssl_error(
    req: &mut Request,
    tls: &Tls,
    io: Io,
    retry_want: bool,
    eof: &mut bool,
) -> Handled {
    let err = tls.error(io.ret);
    match err {
        SSL_ERROR_ZERO_RETURN => Handled {
            retry: false,
            again: io.errno == EAGAIN,
        },
        SSL_ERROR_WANT_READ | SSL_ERROR_WANT_WRITE => Handled {
            retry: retry_want,
            again: true,
        },
        SSL_ERROR_SYSCALL if ffi::ssl::peek_error() == 0 => {
            if io.ret == 0 {
                // php's `SSL: fatal protocol error` asks for a queued code
                // just after finding none, so it is never raised.
                tls.set_shut_down();
                *eof = true;
            } else {
                warn(req, &format!("SSL: {}", strerror(io.errno)));
            }
            Handled {
                retry: false,
                again: io.errno == EAGAIN,
            }
        }
        _ => {
            let mut code = ffi::next_error().unwrap_or(0);
            if error_reason(code) == SSL_R_NO_SHARED_CIPHER {
                warn(
                    req,
                    "SSL_R_NO_SHARED_CIPHER: no suitable shared cipher could be used.  This could be because the server is missing an SSL certificate (local_cert context option)",
                );
            } else {
                let mut messages: Vec<u8> = Vec::new();
                loop {
                    if !messages.is_empty() {
                        messages.push(b'\n');
                    }
                    messages.extend_from_slice(&ffi::ssl::error_string(code));
                    match ffi::next_error() {
                        Some(next) => code = next,
                        None => break,
                    }
                }
                let text = format!(
                    "SSL operation failed with code {err}. OpenSSL Error messages:\n{}",
                    String::from_utf8_lossy(&messages)
                );
                warn(req, &text);
            }
            Handled {
                retry: false,
                again: false,
            }
        }
    }
}

/// `ERR_GET_REASON`: the reason of a code, the whole of a system error's
/// low bits.
fn error_reason(code: std::ffi::c_ulong) -> c_int {
    let reason = if code & 0x8000_0000 == 0 {
        code & 0x7F_FFFF
    } else {
        code & 0x7FFF_FFFF
    };
    c_int::try_from(reason).unwrap_or(0)
}

/// `php_openssl_matches_wildcard_name`: the names equal, ignoring case,
/// or the certificate's name with one `*` in its left-most label that
/// covers no dot of the subject.
pub(crate) fn matches_wildcard_name(subject: &[u8], cert: &[u8]) -> bool {
    if subject.eq_ignore_ascii_case(cert) {
        return true;
    }
    let Some(star) = cert.iter().position(|&b| b == b'*') else {
        return false;
    };
    if cert[..star].contains(&b'.') {
        return false;
    }
    if star > 0
        && !subject
            .get(..star)
            .is_some_and(|p| p.eq_ignore_ascii_case(&cert[..star]))
    {
        return false;
    }
    let suffix = &cert[star + 1..];
    if suffix.len() > subject.len() {
        return false;
    }
    let tail = &subject[subject.len() - suffix.len()..];
    let middle = subject
        .get(star..subject.len() - suffix.len())
        .unwrap_or(&[]);
    tail.eq_ignore_ascii_case(suffix) && !middle.contains(&b'.')
}

/// `EXPAND_IPV6_ADDRESS`: eight groups in upper-case hex, no leading
/// zeros.
fn expand_ipv6(bytes: &[u8]) -> String {
    bytes
        .chunks(2)
        .map(|pair| {
            let hi = u16::from(pair.first().copied().unwrap_or(0));
            let lo = u16::from(pair.get(1).copied().unwrap_or(0));
            format!("{:X}", (hi << 8) | lo)
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// `php_openssl_matches_san_list`.
fn matches_san_list(cert: &Certificate, subject: &[u8]) -> bool {
    let subject_v6 = std::str::from_utf8(subject)
        .ok()
        .and_then(|s| s.parse::<std::net::Ipv6Addr>().ok())
        .map(|ip| expand_ipv6(&ip.octets()));
    for san in ffi::ssl::san_names(cert) {
        match san {
            San::Dns(mut name) => {
                if name.last() == Some(&b'.') {
                    name.pop();
                }
                if matches_wildcard_name(subject, &name) {
                    return true;
                }
            }
            San::Ip(bytes) if bytes.len() == 4 => {
                let text = format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3]);
                if subject.eq_ignore_ascii_case(text.as_bytes()) {
                    return true;
                }
            }
            San::Ip(bytes) if bytes.len() == 16 => {
                if let Some(expanded) = &subject_v6
                    && expanded.eq_ignore_ascii_case(&expand_ipv6(&bytes))
                {
                    return true;
                }
            }
            San::Ip(_) => {}
        }
    }
    false
}

/// `php_openssl_matches_common_name`, with its warnings.
fn matches_common_name(req: &mut Request, cert: &Certificate, subject: &[u8]) -> bool {
    let Some((len, text)) = ffi::ssl::common_name(cert) else {
        warn(req, "Unable to locate peer certificate CN");
        return false;
    };
    let shown = String::from_utf8_lossy(&text[..len.min(text.len())]).into_owned();
    if len != text.len() {
        warn(req, &format!("Peer certificate CN=`{shown}' is malformed"));
        return false;
    }
    if matches_wildcard_name(subject, &text) {
        return true;
    }
    let expected = c_str(subject);
    warn(
        req,
        &format!("Peer certificate CN=`{shown}' did not match expected CN=`{expected}'"),
    );
    false
}

/// `php_openssl_x509_fingerprint_cmp`: the certificate's digest under
/// `method` in lower-case hex, compared ignoring case; the digest's own
/// warning where `method` names none.
fn fingerprint_matches(
    req: &mut Request,
    cert: &Certificate,
    method: &[u8],
    expected: &[u8],
) -> bool {
    let Some(md) = ffi::Digest::by_name(&crate::decode::c_string(method)) else {
        warn(req, "Unknown digest algorithm");
        return false;
    };
    let Some(digest) = cert.digest(&md) else {
        warn(req, "Could not generate signature");
        return false;
    };
    crate::hex(&digest).eq_ignore_ascii_case(expected)
}

/// `php_openssl_x509_fingerprint_match`.
fn fingerprint_match(req: &mut Request, cert: &Certificate, expected: &Fingerprint) -> bool {
    match expected {
        Fingerprint::Text(text) => {
            let method: &[u8] = match text.len() {
                32 => b"md5",
                40 => b"sha1",
                _ => return false,
            };
            fingerprint_matches(req, cert, method, text)
        }
        Fingerprint::Pairs(Some(pairs)) => pairs
            .iter()
            .all(|(algo, fp)| fingerprint_matches(req, cert, algo, fp)),
        Fingerprint::Pairs(None) => {
            warn(
                req,
                "Invalid peer_fingerprint array; [algo => fingerprint] form required",
            );
            false
        }
        Fingerprint::Other => false,
    }
}

/// `php_openssl_alpn_protos_parse`: a comma-separated list in wire form,
/// each name after its length; `None` for a list of 65,535 bytes or more
/// or a name over 255.
fn alpn_wire(list: &[u8]) -> Option<Vec<u8>> {
    if list.len() >= 65535 {
        return None;
    }
    let mut out = Vec::with_capacity(list.len() + 1);
    for name in list.split(|&b| b == b',') {
        out.push(u8::try_from(name.len()).ok()?);
        out.extend_from_slice(name);
    }
    Some(out)
}

impl Layer {
    /// `php_openssl_enable_peer_verification` over one `context`: the CA
    /// file and directory the options or the ini name, a server's CA
    /// names read from its file, the locations loaded or the file read as
    /// a stream, else the platform's defaults for a client; then
    /// `SSL_VERIFY_PEER`.
    fn enable_peer_verification(&self, req: &mut Request, ctx: &Ctx) -> bool {
        let ini = |req: &Request, name: &[u8]| {
            req.ini()
                .get(name)
                .filter(|v| !v.is_empty())
                .map(<[u8]>::to_vec)
        };
        let cafile = match &self.options.cafile {
            None => ini(req, b"openssl.cafile"),
            Some(cafile) => {
                if !self.is_client && !ctx.client_ca_file(&crate::decode::c_string(cafile)) {
                    raise(
                        req,
                        Level::Warning,
                        "SSL: failed loading CA names from cafile",
                    );
                    return false;
                }
                Some(cafile.clone())
            }
        };
        let capath = self
            .options
            .capath
            .clone()
            .or_else(|| ini(req, b"openssl.capath"));
        if cafile.is_some() || capath.is_some() {
            let file = cafile.as_deref().map(crate::decode::c_string);
            let dir = capath.as_deref().map(crate::decode::c_string);
            if !ctx.load_verify_locations(file.as_deref(), dir.as_deref()) {
                ffi::clear_errors();
                if let Some(cafile) = &cafile
                    && load_stream_cafile(req, ctx, cafile) == 0
                {
                    return false;
                }
            }
        } else if self.is_client && !ctx.default_verify_paths() {
            warn(
                req,
                "Unable to set default verify locations and no CA settings specified",
            );
            return false;
        }
        ctx.verify(true);
        true
    }

    /// `php_openssl_set_local_cert`: the certificate chain and its key
    /// from `local_cert` and `local_pk`, the key from the certificate's
    /// file where there is no `local_pk`.
    fn set_local_cert(&self, req: &mut Request, ctx: &Ctx) -> bool {
        let Some(certfile) = &self.options.local_cert else {
            return true;
        };
        let shown = c_str(certfile);
        let Some(cert_path) = check_path(
            req,
            certfile,
            Origin::Setting("local_cert in ssl stream context"),
            false,
        ) else {
            warn(
                req,
                &format!("Unable to get real path of certificate file `{shown}'"),
            );
            return false;
        };
        if !ctx.chain_file(&cert_path) {
            warn(
                req,
                &format!(
                    "Unable to set local cert chain file `{shown}'; Check that your cafile/capath settings include details of your certificate and its issuer"
                ),
            );
            return false;
        }
        let key_path = match &self.options.local_pk {
            Some(pk) => {
                let Some(path) = check_path(
                    req,
                    pk,
                    Origin::Setting("local_pk in ssl stream context"),
                    false,
                ) else {
                    let shown = c_str(pk);
                    warn(
                        req,
                        &format!("Unable to get real path of private key file `{shown}'"),
                    );
                    return false;
                };
                path
            }
            None => cert_path,
        };
        if !ctx.private_key_file(&key_path) {
            let shown = key_path.to_string_lossy();
            warn(req, &format!("Unable to set private key file `{shown}'"));
            return false;
        }
        if !ctx.check_private_key() {
            warn(req, "Private key does not match certificate!");
        }
        true
    }

    /// `php_openssl_set_server_specific_opts`.
    fn set_server_opts(&self, req: &mut Request, ctx: &Ctx) -> bool {
        let mut options = ctx.options();
        if self.options.rsa_key_size {
            warn(req, "rsa_key_size context option has been removed");
        }
        if let Some(path) = &self.options.dh_param
            && let Err(text) = ctx.dh_params(&crate::decode::c_string(path))
        {
            warn(req, text);
            return false;
        }
        if self.options.single_dh_use.unwrap_or(true) {
            options |= SSL_OP_SINGLE_DH_USE;
        }
        if self.options.honor_cipher_order.unwrap_or(true) {
            options |= SSL_OP_CIPHER_SERVER_PREFERENCE;
        }
        ctx.set_options(options);
        true
    }

    /// `php_openssl_enable_server_sni`: each host of `SNI_server_certs`
    /// given a `context` of its own, verified as the listener's is, and the
    /// callback that picks among them.
    fn enable_server_sni(&self, req: &mut Request, setup: &mut Setup, verify_peer: bool) -> bool {
        if self.options.sni_enabled == Some(false) {
            return true;
        }
        let Some(sni) = &self.options.sni_server_certs else {
            return true;
        };
        let entries = match sni {
            SniOption::NotArray => {
                warn(
                    req,
                    "SNI_server_certs requires an array mapping host names to cert paths",
                );
                return false;
            }
            SniOption::Entries(entries) if entries.is_empty() => {
                warn(req, "SNI_server_certs host cert array must not be empty");
                return false;
            }
            SniOption::Entries(entries) => entries,
        };
        for entry in entries {
            let Some(name) = &entry.name else {
                warn(req, "SNI_server_certs array requires string host name keys");
                return false;
            };
            let ctx = sni_entry_ctx(req, &entry.certs);
            let Some(ctx) = ctx else {
                return false;
            };
            if !verify_peer {
                ctx.verify(false);
            } else if !self.enable_peer_verification(req, &ctx) {
                return false;
            }
            setup.hooks().sni.push((crate::decode::c_string(name), ctx));
        }
        setup.sni_callback();
        true
    }

    /// The cipher list, the default where none is given, the security
    /// level and the ALPN protocols of `php_openssl_setup_crypto`; `false`
    /// where the list or the protocols are refused.
    fn set_ciphers(&self, req: &mut Request, setup: &mut Setup) -> bool {
        let ciphers = self.options.ciphers.clone().unwrap_or_else(|| {
            crate::constants::OPENSSL_DEFAULT_STREAM_CIPHERS
                .as_bytes()
                .to_vec()
        });
        if !setup.ctx().cipher_list(&crate::decode::c_string(&ciphers)) {
            return false;
        }
        if let Some(level) = self.options.security_level {
            if !(0..=5).contains(&level) {
                warn(req, "Security level must be between 0 and 5");
            }
            setup
                .ctx()
                .security_level(c_int::try_from(level).unwrap_or(c_int::MAX));
        }
        if let Some(list) = &self.options.alpn_protocols {
            let Some(wire) = alpn_wire(list) else {
                warn(
                    req,
                    "Failed parsing comma-separated TLS ALPN protocol string",
                );
                return false;
            };
            if self.is_client {
                setup.ctx().alpn_protos(&wire);
            } else {
                setup.hooks().alpn = wire;
                setup.alpn_select_callback();
            }
        }
        true
    }

    /// `php_openssl_setup_crypto`: the `SSL_CTX` made and configured as the
    /// `ssl` options say, the `SSL` made over the socket, and a session
    /// taken from `session` where one is given.
    fn setup_crypto(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        method: i64,
        session: Option<Stream>,
    ) -> Result<bool, Thrown> {
        if self.tls.is_some() {
            if socket.blocked {
                warn(req, "SSL/TLS already set-up for this stream");
                return Ok(false);
            }
            return Ok(true);
        }
        ffi::clear_errors();
        let method = CryptoMethod::of(method);
        self.is_client = method.client;
        let Some(ctx) = Ctx::new(self.is_client) else {
            warn(req, "SSL context creation failure");
            return Ok(false);
        };
        self.options = SslOptions::read(req, socket.ctx)?;
        let protocols = method.bounded(
            self.options.min_proto_version,
            self.options.max_proto_version,
        );
        let mut ctx_options = SSL_OP_ALL;
        if self.options.no_ticket {
            ctx_options |= SSL_OP_NO_TICKET;
        }
        ctx_options &= !SSL_OP_DONT_INSERT_EMPTY_FRAGMENTS;
        ctx_options |= SSL_OP_IGNORE_UNEXPECTED_EOF;
        if self.options.disable_compression.unwrap_or(true) {
            ctx_options |= SSL_OP_NO_COMPRESSION;
        }
        let mut setup = Setup::new(ctx);
        let verify_peer = self.options.verify_peer != Some(false);
        if !verify_peer {
            setup.ctx().verify(false);
        } else if !self.enable_peer_verification(req, setup.ctx()) {
            return Ok(false);
        }
        if let Some(passphrase) = &self.options.passphrase {
            setup.hooks().passphrase = Some(passphrase.clone());
            setup.passphrase_callback();
        }
        if !self.set_ciphers(req, &mut setup) || !self.set_local_cert(req, setup.ctx()) {
            return Ok(false);
        }
        setup.ctx().set_options(ctx_options);
        let (min, max) = (min_flag(protocols), max_flag(protocols));
        setup
            .ctx()
            .proto_versions(proto_version(min), proto_version(max));
        if !self.is_client && socket.ctx.is_some() && !self.set_server_opts(req, setup.ctx()) {
            return Ok(false);
        }
        let hooks = setup.hooks();
        hooks.allow_self_signed = self.options.allow_self_signed;
        if let Some(depth) = self.options.verify_depth {
            hooks.verify_depth = u64::from_ne_bytes(depth.to_ne_bytes());
        }
        if !self.is_client && !self.enable_server_sni(req, &mut setup, verify_peer) {
            return Ok(false);
        }
        let Some(mut tls) = setup.open() else {
            warn(req, "SSL handle creation failure");
            return Ok(false);
        };
        if !tls.set_fd(socket.fd.as_raw_fd()) {
            let io = Io { ret: 0, errno: 0 };
            let mut eof = false;
            handle_ssl_error(req, &tls, io, true, &mut eof);
        }
        if !self.is_client {
            let limit = self.options.reneg_limit.unwrap_or(DEFAULT_RENEG_LIMIT);
            if limit >= 0 {
                let window = self.options.reneg_window.unwrap_or(DEFAULT_RENEG_WINDOW);
                tls.hooks().reneg = Some(Reneg {
                    limit,
                    window,
                    ..Reneg::default()
                });
                tls.reneg_callback();
            }
        }
        tls.set_mode(SSL_MODE_RELEASE_BUFFERS);
        if let Some(session) = session {
            let copied = session.crypto(req, |other| {
                let other: &dyn Any = other;
                let from = other.downcast_ref::<Layer>()?.tls.as_ref()?;
                tls.copy_session_from(from).then_some(())
            });
            match copied {
                None => warn(req, "Supplied session stream must be an SSL enabled stream"),
                Some(None) => warn(req, "Supplied SSL session stream is not initialized"),
                Some(Some(())) => {}
            }
        }
        self.tls = Some(tls);
        Ok(true)
    }

    /// `php_openssl_enable_client_sni`: the host a client names, its
    /// `peer_name` over the address's, unless `SNI_enabled` is off.
    fn enable_client_sni(&self, tls: &Tls) {
        if self.options.sni_enabled == Some(false) {
            return;
        }
        let name = self.options.peer_name.as_ref().or(self.url_name.as_ref());
        if let Some(name) = name {
            tls.set_host_name(&crate::decode::c_string(name));
        }
    }

    /// `php_openssl_capture_peer_certs`: the peer's certificate, and its
    /// chain, kept in the stream's `context` where `capture_peer_cert` and
    /// `capture_peer_cert_chain` ask; whether the certificate was kept.
    fn capture_peer_certs(
        &self,
        req: &mut Request,
        context: StreamCtx,
        tls: &Tls,
        peer: &Certificate,
    ) -> Result<(), Thrown> {
        if self.options.capture_peer_cert {
            let object = object_of(req, peer.clone())?;
            let value = object.into_value();
            let set = context.set_option(req, b"ssl", b"peer_certificate", &value);
            value.give_up(req);
            set?;
        }
        if self.options.capture_peer_cert_chain {
            let value = match tls.peer_chain() {
                Some(chain) => {
                    let mut array = PhpArray::new(req)?;
                    for cert in chain {
                        let object = object_of(req, cert)?;
                        array.append(req, object.into_value())?;
                    }
                    array.into_value()
                }
                None => Owner::null(),
            };
            let set = context.set_option(req, b"ssl", b"peer_certificate_chain", &value);
            value.give_up(req);
            set?;
        }
        Ok(())
    }

    /// `php_openssl_apply_peer_verification_policy`, with its warnings.
    fn verify_peer(&self, req: &mut Request, tls: &Tls, peer: Option<&Certificate>) -> bool {
        let must_verify_peer = self.options.verify_peer.unwrap_or(self.is_client);
        let must_verify_peer_name = self.options.verify_peer_name.unwrap_or(self.is_client);
        let fingerprint = self.options.peer_fingerprint.as_ref();
        let Some(peer) = peer else {
            if must_verify_peer || must_verify_peer_name || fingerprint.is_some() {
                warn(req, "Could not get peer certificate");
                return false;
            }
            return true;
        };
        if must_verify_peer {
            let err = tls.verify_result();
            let allowed = err == X509_V_OK
                || (err == X509_V_ERR_DEPTH_ZERO_SELF_SIGNED_CERT
                    && self.options.allow_self_signed);
            if !allowed {
                let text = String::from_utf8_lossy(&ffi::ssl::verify_error_text(err)).into_owned();
                warn(req, &format!("Could not verify peer: code:{err} {text}"));
                return false;
            }
        }
        if let Some(expected) = fingerprint {
            if matches!(expected, Fingerprint::Other) {
                warn(
                    req,
                    "Expected peer fingerprint must be a string or an array",
                );
                return false;
            }
            if !fingerprint_match(req, peer, expected) {
                warn(req, "peer_fingerprint match failure");
                return false;
            }
        }
        if must_verify_peer_name {
            let name = match &self.options.peer_name {
                Some(name) => Some(name.clone()),
                None if self.is_client => self.url_name.clone(),
                None => None,
            };
            let Some(name) = name else {
                return false;
            };
            return matches_san_list(peer, &name) || matches_common_name(req, peer, &name);
        }
        true
    }

    /// `php_openssl_enable_crypto`: the handshake driven until it is done,
    /// fails or, on a socket that does not block, has more to wait for;
    /// the peer's certificate captured and verified; or the layer shut.
    fn enable_crypto(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        activate: bool,
    ) -> Result<Handshake, Thrown> {
        if activate && self.phase != Phase::Active {
            let Some(tls) = self.tls.take() else {
                return Ok(Handshake::Failed);
            };
            let done = self.handshake(req, socket, &tls);
            self.tls = Some(tls);
            return done;
        }
        if !activate
            && self.phase == Phase::Active
            && let Some(tls) = &self.tls
        {
            tls.shutdown();
            self.phase = Phase::Begun;
        }
        Ok(Handshake::Failed)
    }

    /// The handshake half of [`Layer::enable_crypto`].
    fn handshake(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        tls: &Tls,
    ) -> Result<Handshake, Thrown> {
        let blocked = socket.blocked;
        if self.is_client {
            self.enable_client_sni(tls);
        }
        if self.phase == Phase::Fresh {
            tls.set_state(self.is_client);
            self.phase = Phase::Begun;
        }
        if socket.set_blocking(false) {
            tls.set_mode(SSL_MODE_ENABLE_PARTIAL_WRITE | SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER);
        }
        let timeout = if self.is_client {
            self.connect_timeout
        } else {
            socket.timeout
        };
        let limit = timeout.filter(|t| !socket.blocked && !t.is_zero());
        let start = Instant::now();
        let mut eof = false;
        let (io, handled) = loop {
            ffi::clear_errors();
            let io = tls.handshake(self.is_client);
            let elapsed = start.elapsed();
            if let Some(limit) = limit
                && elapsed > limit
            {
                warn(req, "SSL: Handshake timed out");
                return Ok(Handshake::Failed);
            }
            if io.ret > 0 {
                break (io, None);
            }
            let err = tls.error(io.ret);
            let handled = handle_ssl_error(req, tls, io, blocked, &mut eof);
            if !handled.retry {
                break (io, Some(handled));
            }
            let want = if err == SSL_ERROR_WANT_READ {
                Readiness::Readable
            } else {
                Readiness::Writable
            };
            let _ready = socket.wait(want, limit.map(|l| l.saturating_sub(elapsed)));
        };
        if socket.blocked != blocked {
            socket.set_blocking(blocked);
        }
        let peer = tls.peer_certificate();
        if io.ret == 1 {
            if let (Some(peer), Some(context)) = (&peer, socket.ctx) {
                self.capture_peer_certs(req, context, tls, peer)?;
            }
            if !self.verify_peer(req, tls, peer.as_ref()) {
                tls.shutdown();
                return Ok(Handshake::Failed);
            }
            self.phase = Phase::Active;
            return Ok(Handshake::Done);
        }
        if handled.is_some_and(|h| h.again) {
            return Ok(Handshake::Pending);
        }
        if let (Some(peer), Some(context)) = (&peer, socket.ctx) {
            self.capture_peer_certs(req, context, tls, peer)?;
        }
        Ok(Handshake::Failed)
    }

    /// `php_openssl_limit_handshake_reneg`'s answer to a passed bucket,
    /// once the read that ran the handshake returned: the `context`'s
    /// `reneg_limit_callback` called with the stream, which keeps the
    /// connection by answering `true`, or php's warning.
    fn reneg_exceeded(
        req: &mut Request,
        socket: &CryptoSocket<'_>,
        tls: &mut Tls,
    ) -> Result<(), Thrown> {
        let callback = option(req, socket.ctx, b"reneg_limit_callback");
        let Some(callback) = callback else {
            warn(
                req,
                "SSL: client-initiated handshake rate limit exceeded by peer",
            );
            return Ok(());
        };
        let callable = to_callable(req, &callback)?;
        let argument = socket.stream.map_or(Value::null(), Stream::as_value);
        let returned = call(req, &callable, &[argument])?;
        if matches!(returned.as_ref(), ValueRef::Bool(true))
            && let Some(bucket) = tls.hooks().reneg.as_mut()
        {
            bucket.should_close = false;
        }
        returned.give_up(req);
        Ok(())
    }

    /// `php_openssl_sockop_io`'s active half: the read or write through the
    /// `SSL`, waiting as a blocking socket with a timeout waits, and never
    /// for a supplemental read.
    fn io(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        mut buf: IoBuf<'_>,
        eof: &mut bool,
    ) -> Result<Option<usize>, Thrown> {
        let Some(tls) = self.tls.as_mut() else {
            return Ok(None);
        };
        Layer::io_over(req, socket, tls, &mut buf, eof)
    }

    /// [`Layer::io`] over the layer's TLS.
    fn io_over(
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        tls: &mut Tls,
        buf: &mut IoBuf<'_>,
        eof: &mut bool,
    ) -> Result<Option<usize>, Thrown> {
        let read = matches!(buf, IoBuf::Read(_));
        let supplemental = socket.supplemental;
        let mut retry = !supplemental;
        let began_blocked = socket.blocked;
        let timeout = (began_blocked && !supplemental).then_some(socket.timeout);
        if timeout.is_some() || supplemental {
            socket.set_blocking(false);
        }
        let limit = timeout
            .flatten()
            .filter(|t| !socket.blocked && !t.is_zero());
        let start = Instant::now();
        let mut nr_bytes: c_int;
        loop {
            let elapsed = start.elapsed();
            if let Some(limit) = limit
                && elapsed > limit
            {
                if began_blocked {
                    socket.set_blocking(true);
                }
                socket.timed_out = true;
                return Ok(None);
            }
            ffi::clear_errors();
            let io = match buf {
                IoBuf::Read(b) => tls.read(b),
                IoBuf::Write(b) => tls.write(b),
            };
            nr_bytes = io.ret;
            let exceeded = tls
                .hooks()
                .reneg
                .as_mut()
                .is_some_and(|bucket| std::mem::take(&mut bucket.exceeded));
            if read && exceeded {
                Layer::reneg_exceeded(req, socket, tls)?;
            }
            if read && tls.hooks().reneg.is_some_and(|bucket| bucket.should_close) {
                let _ = socket.shutdown();
                nr_bytes = 0;
                *eof = true;
                break;
            }
            let left = limit.map(|l| l.saturating_sub(start.elapsed()));
            let err = tls.error(nr_bytes);
            let wait_for = |err: c_int| {
                let writable = if read {
                    err == SSL_ERROR_WANT_WRITE
                } else {
                    err != SSL_ERROR_WANT_READ
                };
                if writable {
                    Readiness::Writable
                } else {
                    Readiness::Readable
                }
            };
            if nr_bytes <= 0 {
                let handled = handle_ssl_error(req, tls, io, false, eof);
                retry = handled.retry;
                if handled.again && err == SSL_ERROR_WANT_READ && read {
                    retry = true;
                }
                if handled.again && err == SSL_ERROR_WANT_WRITE && !read {
                    retry = true;
                }
                if read {
                    *eof = !retry && !handled.again && tls.pending() == 0;
                }
                if !began_blocked || supplemental {
                    break;
                }
                if retry {
                    let _ready = socket.wait(wait_for(err), left);
                }
            } else {
                if err == SSL_ERROR_NONE {
                    break;
                }
                if began_blocked {
                    let _ready = socket.wait(wait_for(err), left);
                }
            }
            if !retry {
                break;
            }
        }
        if began_blocked {
            socket.set_blocking(true);
        }
        Ok(Some(usize::try_from(nr_bytes).unwrap_or(0)))
    }
}

/// The buffer of one [`Layer::io`], read into or written from.
enum IoBuf<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

/// `php_openssl_load_stream_cafile`: the certificates of a CA file read
/// as a stream, one `BEGIN`/`END CERTIFICATE` block at a time, each added
/// to the `context`'s store; how many were.
fn load_stream_cafile(req: &mut Request, ctx: &Ctx, cafile: &[u8]) -> usize {
    let shown = c_str(cafile);
    // `php_stream_open_wrapper(cafile, "rb", 0, NULL)`: options 0, so a
    // file that is not there warns only as the line below does.
    let opening = php_ext::request::Opening::default();
    let opened = match req.open_stream_with(cafile, b"rb", opening) {
        Ok(opened) => opened,
        // What a user wrapper threw stays in `EG(exception)`, and php
        // warns with it standing.
        Err(thrown) => {
            leave_pending(req, thrown);
            None
        }
    };
    let Some(stream) = opened else {
        raise(
            req,
            Level::Warning,
            &format!("failed loading cafile stream: `{shown}'"),
        );
        return 0;
    };
    let remote = stream.meta_data(req).is_ok_and(|meta| {
        let found = meta.find(b"wrapper_type").and_then(|v| {
            v.as_php_string()
                .map(|s| matches!(s.as_bytes(), b"http" | b"ftp"))
        });
        meta.give_up(req);
        found.unwrap_or(false)
    });
    if remote {
        stream.free(req);
        raise(
            req,
            Level::Warning,
            "remote cafile streams are disabled for security purposes",
        );
        return 0;
    }
    let mut added = 0;
    let mut block: Option<Vec<u8>> = None;
    while let Ok(Some(bytes)) = stream.get_line(req, None) {
        match &mut block {
            None => {
                if bytes == b"-----BEGIN CERTIFICATE-----\n"
                    || bytes == b"-----BEGIN CERTIFICATE-----\r\n"
                {
                    block = Some(bytes);
                }
            }
            Some(pem) => {
                pem.extend_from_slice(&bytes);
                let end = matches!(
                    bytes.as_slice(),
                    b"-----END CERTIFICATE-----"
                        | b"-----END CERTIFICATE-----\n"
                        | b"-----END CERTIFICATE-----\r\n"
                );
                if end {
                    let pem = block.take().unwrap_or_default();
                    if let Some(bio) = ffi::Bio::reading(&pem)
                        && let Some(cert) = Certificate::read_pem(&bio)
                        && ctx.add_store_cert(&cert)
                    {
                        added += 1;
                    }
                }
            }
        }
    }
    stream.free(req);
    if added == 0 {
        raise(
            req,
            Level::Warning,
            &format!("no valid certs found cafile stream: `{shown}'"),
        );
    }
    added
}

/// The `context` of one `SNI_server_certs` host, from its `local_cert` and
/// `local_pk` or its one path, each held to the paths guard; `None` after
/// php's warning where a piece is missing or refused.
fn sni_entry_ctx(req: &mut Request, certs: &SniCerts) -> Option<Ctx> {
    match certs {
        SniCerts::Pair {
            local_cert,
            local_pk,
        } => {
            let Some(cert) = local_cert else {
                warn(req, "local_cert not present in the array");
                return None;
            };
            let Some(cert_path) = check_path(
                req,
                cert,
                Origin::Setting("SNI_server_certs local_cert in ssl stream context"),
                false,
            ) else {
                let shown = c_str(cert);
                warn(
                    req,
                    &format!("Failed setting local cert chain file `{shown}'; could not open file"),
                );
                return None;
            };
            let Some(pk) = local_pk else {
                warn(req, "local_pk not present in the array");
                return None;
            };
            let Some(pk_path) = check_path(
                req,
                pk,
                Origin::Setting("SNI_server_certs local_pk in ssl stream context"),
                false,
            ) else {
                let shown = c_str(pk);
                warn(
                    req,
                    &format!(
                        "Failed setting local private key file `{shown}';  could not open file"
                    ),
                );
                return None;
            };
            sni_server_ctx(req, &cert_path, &pk_path)
        }
        SniCerts::Path(path) => {
            let resolved = check_path(
                req,
                path,
                Origin::Setting("SNI_server_certs in ssl stream context"),
                false,
            );
            if let Some(resolved) = resolved {
                sni_server_ctx(req, &resolved, &resolved)
            } else {
                let shown = c_str(path);
                warn(
                    req,
                    &format!("Failed setting local cert chain file `{shown}'; file not found"),
                );
                None
            }
        }
        SniCerts::Other => {
            warn(
                req,
                "SNI_server_certs options values must be of type array|string",
            );
            None
        }
    }
}

/// The `SSL_CTX` of one `SNI_server_certs` host,
/// `php_openssl_create_sni_server_ctx`.
fn sni_server_ctx(req: &mut Request, cert: &std::ffi::CStr, key: &std::ffi::CStr) -> Option<Ctx> {
    let Some(ctx) = Ctx::new(false) else {
        warn(req, "Failed to create the SSL context");
        return None;
    };
    if !ctx.chain_file(cert) {
        let shown = cert.to_string_lossy();
        warn(
            req,
            &format!(
                "Failed setting local cert chain file `{shown}'; check that your cafile/capath settings include details of your certificate and its issuer"
            ),
        );
        return None;
    }
    if !ctx.private_key_file(key) {
        let shown = key.to_string_lossy();
        warn(
            req,
            &format!("Failed setting private key from file `{shown}'"),
        );
        return None;
    }
    Some(ctx)
}

impl Crypto for Layer {
    fn setup(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        method: i64,
        session: Option<Stream>,
    ) -> Result<bool, Thrown> {
        self.setup_crypto(req, socket, method, session)
    }

    fn enable(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        activate: bool,
    ) -> Result<Handshake, Thrown> {
        self.enable_crypto(req, socket, activate)
    }

    fn active(&self) -> bool {
        self.phase == Phase::Active
    }

    fn read(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        buf: &mut [u8],
        eof: &mut bool,
    ) -> Result<Option<usize>, Thrown> {
        self.io(req, socket, IoBuf::Read(buf), eof)
    }

    fn write(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
        buf: &[u8],
    ) -> Option<usize> {
        let mut eof = false;
        self.io(req, socket, IoBuf::Write(buf), &mut eof)
            .unwrap_or(None)
    }

    fn pending(&self) -> usize {
        self.tls.as_ref().map_or(0, Tls::pending)
    }

    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` with an active layer, asked with
    /// php's zero: one `SSL_peek` that does not wait, a peer that is still
    /// there or has only not written yet being alive.
    fn alive(&mut self, req: &mut Request, socket: &mut CryptoSocket<'_>) -> bool {
        let _ = req;
        let Some(tls) = &self.tls else {
            return false;
        };
        let began_blocked = socket.blocked;
        if began_blocked {
            socket.set_blocking(false);
        }
        let io = tls.peek();
        let alive = io.ret > 0
            || match tls.error(io.ret) {
                SSL_ERROR_SYSCALL => io.errno == EAGAIN,
                SSL_ERROR_WANT_READ | SSL_ERROR_WANT_WRITE => true,
                _ => false,
            };
        if began_blocked && !socket.blocked {
            socket.set_blocking(true);
        }
        alive
    }

    /// `PHP_STREAM_OPTION_META_DATA_API`'s `crypto`: the protocol, the
    /// cipher's name, bits and version, and the ALPN protocol agreed.
    ///
    /// ```php
    /// $s = stream_socket_server('tcp://127.0.0.1:0');
    /// var_dump(array_key_exists('crypto', stream_get_meta_data($s)));
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn meta(&self, req: &mut Request) -> Result<Option<Owner<PhpArray>>, Thrown> {
        let Some(tls) = self.tls.as_ref().filter(|_| self.phase == Phase::Active) else {
            return Ok(None);
        };
        let protocol: &[u8] = match tls.version() {
            TLS1_3_VERSION => b"TLSv1.3",
            TLS1_2_VERSION => b"TLSv1.2",
            TLS1_1_VERSION => b"TLSv1.1",
            TLS1_VERSION => b"TLSv1",
            SSL3_VERSION if HAVE_SSL3 => b"SSLv3",
            _ => b"UNKNOWN",
        };
        let mut array = PhpArray::new(req)?;
        crate::put_bytes(req, &mut array, b"protocol", protocol)?;
        let cipher = tls.cipher();
        let (name, bits, version) =
            cipher.map_or((Vec::new(), 0, Vec::new()), |c| (c.name, c.bits, c.version));
        crate::put_bytes(req, &mut array, b"cipher_name", &name)?;
        crate::put(req, &mut array, b"cipher_bits", Owner::int(i64::from(bits)))?;
        crate::put_bytes(req, &mut array, b"cipher_version", &version)?;
        if let Some(alpn) = tls.alpn_selected() {
            crate::put_bytes(req, &mut array, b"alpn_protocol", &alpn)?;
        }
        Ok(Some(array))
    }

    /// `STREAM_XPORT_OP_CONNECT`'s tail and the accept's: where the
    /// `transport` enables crypto as it connects, set up and enabled, and
    /// php's `Failed to enable crypto` where that failed.
    fn connected(
        &mut self,
        req: &mut Request,
        socket: &mut CryptoSocket<'_>,
    ) -> Result<Result<(), String>, Thrown> {
        let Some(method) = self.on_connect else {
            return Ok(Ok(()));
        };
        let set_up = self.setup_crypto(req, socket, method.bits(), None)?;
        if !set_up || self.enable_crypto(req, socket, true)? == Handshake::Failed {
            warn(req, "Failed to enable crypto");
            return Ok(Err(String::from("Cannot enable crypto")));
        }
        Ok(Ok(()))
    }

    /// `sizeof(php_openssl_netstream_data_t)` on the pinned 64-bit
    /// build, the `php_netstream_data_t` it opens with, `ssl_handle` and
    /// `ctx`, the connect timeout, the four flags and the method, the
    /// renegotiation bucket, the SNI certificates and their count,
    /// `alpn_ctx`, `url_name` and its bit field
    /// (`Z/ext/openssl/xp_ssl.c:185-203`); then `url_name`'s
    /// `pestrndup`, the host and its NUL.
    ///
    /// ```php
    /// $s = stream_socket_server('tcp://127.0.0.1:0'); $name = stream_socket_get_name($s, false);
    /// $c = stream_socket_client("tcp://$name"); $a = stream_socket_accept($s); fclose($c); fclose($a); unset($c, $a);
    /// $m = memory_get_usage(); $c = stream_socket_client("tcp://$name"); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $a = stream_socket_accept($s); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); fclose($c); fclose($a); unset($c, $a); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 616
    /// 416
    /// -872
    /// ```
    fn allocations(&self) -> Vec<usize> {
        const NETSTREAM_DATA_SIZE: usize = 136;
        let mut sizes = vec![NETSTREAM_DATA_SIZE];
        if let Some(name) = &self.url_name {
            sizes.push(name.len() + 1);
        }
        sizes
    }

    /// `php_openssl_tcp_sockop_accept`'s client: nothing of the listener's
    /// but its `enable_on_connect` and its method, without the client bit.
    fn accepted(&self) -> Box<dyn Crypto> {
        Box::new(Layer {
            tls: None,
            options: SslOptions::default(),
            connect_timeout: None,
            on_connect: self.on_connect.map(|method| CryptoMethod {
                client: false,
                ..method
            }),
            is_client: false,
            phase: Phase::Fresh,
            url_name: None,
        })
    }
}
