//! What the operator sets on the line: where the server listens, clear
//! and under TLS with its certificates (#2343), the document root, the
//! request pool and its queue, the php ini the engine starts with, and
//! the limits (#2337's Implementation Decisions, #2341):
//! one table of Kestrel's defaults, each overridden by `--limit
//! <name>=<value>`, and each applied by the connection side, the
//! connection cap by its accept loop (#2342) and the shutdown timeout by
//! the drain SIGTERM begins (#2344).
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::Duration;

use php_sapi::ini::Search;

pub use crate::log::Destination;

/// The line's grammar, printed with a refusal: the options, then each
/// limit `--limit` names, from [`LIMITS`].
#[must_use]
pub fn usage() -> String {
    let mut usage = "Usage: makala [-n] [-c <path>] [-d foo[=bar]]... [--listen <addr>:<port>] \
                     [--listen-tls <addr>:<port> --certificate [<host>=]<chain.pem>,<key.pem>...] \
                     --root <dir> [--threads <n>] [--queue <n>] [--serve-hidden] \
                     [--access-log <path>] [--error-log <path>] [--limit <name>=<value>]...\n\
                     one of --listen and --listen-tls at least; a --certificate with a host is \
                     that name's, through SNI, and one without is any other's\n\
                     limits, a time being <n>s or <n>ms and a rate <bytes a second>,<grace time> or off:\n"
        .to_owned();
    for (name, shape, _) in LIMITS {
        usage += "  ";
        usage += name;
        usage += "=";
        usage += shape;
        usage += "\n";
    }
    usage
}

/// The server as the operator's line set it.
#[derive(Debug, Clone)]
pub struct Options {
    /// The address the clear listening socket binds, where one does; `port`
    /// 0 takes one the system picks, which the started line names.
    pub listen: Option<SocketAddr>,
    /// The TLS listener, where one is.
    pub tls: Option<Tls>,
    /// The document root, made absolute: what a request's path is found
    /// under, and never left (`php -S`'s `normalize_vpath`).
    pub document_root: PathBuf,
    /// How many requests are served at once, each on a thread of its own.
    pub request_threads: usize,
    /// How many whole requests wait for a free request thread; a request
    /// past them waits on its connection for room.
    pub request_queue: usize,
    /// The stack of each request thread, [`php_sapi::REQUEST_STACK`].
    pub stack_size: usize,
    /// Whether dot segments and PHP's source are refused, or served as
    /// `php -S` serves them.
    pub hidden: Hidden,
    /// The `-d` entries, in the order given, over the ini files.
    pub ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// `-n` and `-c`, which steer the ini file search.
    pub ini_search: Search,
    /// Where the access log goes, a line for each response; none is kept
    /// without `--access-log` (#2345).
    pub access_log: Option<Destination>,
    /// Where the error log goes: stderr without `--error-log`.
    pub error_log: Destination,
    pub limits: Limits,
}

/// The TLS listener (#2343): TLS 1.2 and 1.3, ALPN `http/1.1`, and the
/// certificate each handshake gets, chosen by the name it asks for
/// (SNI). TLS is configured only with a certificate: a line that asks
/// for the listener without one is refused, as Kestrel requires one
/// explicitly (research/production-server.r2.md §1.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tls {
    /// The address the TLS listening socket binds; `port` 0 as `listen`'s.
    pub listen: SocketAddr,
    /// The certificate a handshake gets that names no host, or a host
    /// with none of its own; with none, such a handshake fails.
    pub default: Option<CertificateFiles>,
    /// The certificate each host gets, the host lowercase, as rustls
    /// hands it over.
    pub by_host: Vec<(String, CertificateFiles)>,
}

/// One certificate's files, PEM: its chain, the end entity first, and its
/// private key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateFiles {
    /// The certificates, the end entity first, then what signed it.
    pub chain: PathBuf,
    /// The end entity's private key.
    pub key: PathBuf,
}

/// What the document root shows that `php -S` serves and a server facing
/// the internet does not (#2586).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Hidden {
    /// Answered 404 as though absent: a path with a segment that starts
    /// with `.`, but under `/.well-known/` (RFC 8615), a static file whose
    /// extension is one of PHP's source (`route::SOURCE_EXTENSIONS`), in
    /// any case, and a static file with a path past it (#2642).
    #[default]
    Refused,
    /// Served by `php -S`'s rules exactly: `--serve-hidden`.
    Served,
}

/// The limits, with Kestrel's defaults, which are IIS's, Apache's and
/// nginx's (research/production-server.r2.md §1.3, §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The request line, in bytes, past which a request is answered 414:
    /// 8 KB.
    pub request_line: usize,
    /// How many header fields a request may carry, past which it is
    /// answered 431: 100.
    pub header_count: usize,
    /// The header fields, in bytes, past which a request is answered 431:
    /// 32 KB.
    pub header_bytes: usize,
    /// The body, in bytes, past which a request is answered 413, apart
    /// from php's `post_max_size`: 30,000,000.
    pub body_bytes: u64,
    /// How long a request's head may take from its first byte, past which
    /// it is answered 408: 30 s.
    pub header_timeout: Duration,
    /// How long a TLS handshake may take from the accept, past which the
    /// connection is closed: 10 s, Kestrel's `HandshakeTimeout`.
    pub handshake_timeout: Duration,
    /// How long a connection may wait for a request's first byte, from
    /// its accept or its last response, past which it is closed: 130 s.
    pub keep_alive_idle: Duration,
    /// The least a client must send of a body each second once the
    /// grace has passed, Kestrel's `MinRequestBodyDataRate`, or none.
    pub min_body_rate: Option<MinDataRate>,
    /// The least a client must take of a response each second once the
    /// grace has passed, Kestrel's `MinResponseDataRate`, or none.
    pub min_response_rate: Option<MinDataRate>,
    /// How many requests one connection may make: 1000, Cowboy's
    /// `max_keepalive`.
    pub requests_per_connection: u32,
    /// How many connections the server holds open at once, past which it
    /// stops accepting and the kernel's queue holds the rest: 10,000.
    pub connections: u32,
    /// How long the drain SIGTERM begins may take before what is still in
    /// flight is aborted and the process ends: 30 s, the default of
    /// Kestrel's host since .NET 6 (research/production-server.r2.md
    /// §1.4).
    pub shutdown_timeout: Duration,
    /// How many log lines wait for the log thread, past which a line is
    /// dropped and counted rather than waited for, so a slow destination
    /// never stalls a request: 4096 (#2345).
    pub log_lines: usize,
}

/// A minimum data rate, Kestrel's `MinDataRate`: so many bytes a second,
/// held once a grace has passed: 240 bytes after 5 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinDataRate {
    /// The bytes a second a client must at least move.
    pub bytes_per_second: NonZeroU64,
    /// How long the server waits before it holds the client to the rate.
    pub grace: Duration,
}

impl Default for MinDataRate {
    /// Kestrel's, which is IIS's `minBytesPerSecond`: 240 bytes a second
    /// after 5 s.
    fn default() -> MinDataRate {
        MinDataRate {
            bytes_per_second: NonZeroU64::new(240).unwrap_or(NonZeroU64::MIN),
            grace: Duration::from_secs(5),
        }
    }
}

impl MinDataRate {
    /// How long `bytes` take at the rate.
    pub(crate) fn time_for(self, bytes: u64) -> Duration {
        let nanos = u128::from(bytes) * 1_000_000_000 / u128::from(self.bytes_per_second.get());
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// How long a client may keep the server waiting in all once it has
    /// moved `bytes`: the grace, or the time those bytes take at the rate
    /// where that is longer.
    pub(crate) fn allowed(self, bytes: u64) -> Duration {
        self.grace.max(self.time_for(bytes))
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            request_line: 8 * 1024,
            header_count: 100,
            header_bytes: 32 * 1024,
            body_bytes: 30_000_000,
            header_timeout: Duration::from_secs(30),
            handshake_timeout: Duration::from_secs(10),
            keep_alive_idle: Duration::from_secs(130),
            min_body_rate: Some(MinDataRate::default()),
            min_response_rate: Some(MinDataRate::default()),
            requests_per_connection: 1000,
            connections: 10_000,
            shutdown_timeout: Duration::from_secs(30),
            log_lines: 4096,
        }
    }
}

/// How one limit's value is read into the table.
type SetLimit = fn(&mut Limits, &str) -> Result<(), String>;

/// Each limit as `--limit` names it, the shape of its value, and how the
/// value is read into the table: the one list the line reads and
/// [`usage`] prints.
const LIMITS: &[(&str, &str, SetLimit)] = &[
    ("request-line", "<bytes>", |l, v| {
        set(&mut l.request_line, count(v))
    }),
    ("header-count", "<n>", |l, v| {
        set(&mut l.header_count, count(v))
    }),
    ("header-bytes", "<bytes>", |l, v| {
        set(&mut l.header_bytes, count(v))
    }),
    ("body-bytes", "<bytes>", |l, v| {
        set(&mut l.body_bytes, number(v))
    }),
    ("header-timeout", "<time>", |l, v| {
        set(&mut l.header_timeout, time(v))
    }),
    ("handshake-timeout", "<time>", |l, v| {
        set(&mut l.handshake_timeout, time(v))
    }),
    ("keep-alive-idle", "<time>", |l, v| {
        set(&mut l.keep_alive_idle, time(v))
    }),
    ("body-data-rate", "<rate>", |l, v| {
        set(&mut l.min_body_rate, rate(v))
    }),
    ("response-data-rate", "<rate>", |l, v| {
        set(&mut l.min_response_rate, rate(v))
    }),
    ("requests-per-connection", "<n>", |l, v| {
        set(&mut l.requests_per_connection, count(v))
    }),
    ("connections", "<n>", |l, v| {
        set(&mut l.connections, count(v))
    }),
    ("shutdown-timeout", "<time>", |l, v| {
        set(&mut l.shutdown_timeout, time(v))
    }),
    ("log-lines", "<n>", |l, v| set(&mut l.log_lines, count(v))),
];

/// `read` into `field` where it was read.
fn set<T>(field: &mut T, read: Result<T, String>) -> Result<(), String> {
    *field = read?;
    Ok(())
}

impl Limits {
    /// One `--limit <name>=<value>` read into the table.
    fn set(&mut self, entry: &[u8]) -> Result<(), String> {
        let entry = std::str::from_utf8(entry).map_err(|_| "a --limit is not text".to_owned())?;
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| format!("--limit {entry} is not <name>=<value>"))?;
        let (_, _, set) = LIMITS
            .iter()
            .find(|(known, _, _)| *known == name)
            .ok_or_else(|| format!("--limit {name} is not a limit"))?;
        set(self, value).map_err(|why| format!("--limit {name}={value}: {why}"))
    }
}

/// A number, of bytes or of things.
fn number<T: std::str::FromStr>(value: &str) -> Result<T, String> {
    value.parse().map_err(|_| "not a number".to_owned())
}

/// A number of one or more.
fn count<T: std::str::FromStr + Default + PartialEq>(value: &str) -> Result<T, String> {
    at_least_one(number(value)?)
}

/// `n`, where it is not zero.
fn at_least_one<T: Default + PartialEq>(n: T) -> Result<T, String> {
    if n == T::default() {
        return Err("must be at least 1".to_owned());
    }
    Ok(n)
}

/// A time of more than none, `<n>s` or `<n>ms`.
fn time(value: &str) -> Result<Duration, String> {
    let time = if let Some(ms) = value.strip_suffix("ms") {
        Duration::from_millis(number(ms)?)
    } else if let Some(s) = value.strip_suffix('s') {
        Duration::from_secs(number(s)?)
    } else {
        return Err("a time is <n>s or <n>ms".to_owned());
    };
    at_least_one(time)
}

/// A minimum data rate, `<bytes a second>,<grace>`, or `off`.
fn rate(value: &str) -> Result<Option<MinDataRate>, String> {
    if value == "off" {
        return Ok(None);
    }
    let (bytes, grace) = value
        .split_once(',')
        .ok_or("a rate is <bytes a second>,<grace time> or off")?;
    Ok(Some(MinDataRate {
        bytes_per_second: NonZeroU64::new(number(bytes)?).ok_or("must be at least 1")?,
        grace: time(grace)?,
    }))
}

impl Options {
    /// The line, past the binary's name: php's `-n`, `-c` and `-d`, and
    /// the server's own options; a refusal says what was wrong.
    ///
    /// # Errors
    ///
    /// What the line got wrong: an unknown option, a value missing or not
    /// read, no `--listen` nor `--listen-tls`, a TLS listener without a
    /// certificate or a certificate without one, or no `--root`.
    pub fn parse(argv: &[OsString]) -> Result<Options, String> {
        let mut words = argv.iter();
        let mut listen = None;
        let mut listen_tls = None;
        let mut certificates = Vec::new();
        let mut root = None;
        let mut threads = None;
        let mut queue = None;
        let mut ini = Vec::new();
        let mut ini_search = Search::default();
        let mut limits = Limits::default();
        let mut access_log = None;
        let mut error_log = Destination::Stderr;
        let mut hidden = Hidden::Refused;
        while let Some(word) = words.next() {
            let mut value = |name: &str| {
                words
                    .next()
                    .ok_or_else(|| format!("{name} needs a value"))
                    .map(|value| value.clone().into_encoded_bytes())
            };
            match word.as_encoded_bytes() {
                b"-n" => ini_search.ignore = true,
                b"-c" => ini_search.path = Some(value("-c")?),
                b"-d" => ini.push(php_sapi::ini::define(&value("-d")?)),
                b"--listen" => listen = Some(read::<SocketAddr>("--listen", &value("--listen")?)?),
                b"--listen-tls" => {
                    listen_tls = Some(read::<SocketAddr>("--listen-tls", &value("--listen-tls")?)?);
                }
                b"--certificate" => certificates.push(certificate(&value("--certificate")?)?),
                b"--root" => root = Some(value("--root")?),
                b"--threads" => {
                    threads = Some(option_count("--threads", &value("--threads")?)?);
                }
                b"--queue" => queue = Some(option_count("--queue", &value("--queue")?)?),
                b"--serve-hidden" => hidden = Hidden::Served,
                b"--limit" => limits.set(&value("--limit")?)?,
                b"--access-log" => {
                    access_log = Some(Destination::File(path(&value("--access-log")?)));
                }
                b"--error-log" => error_log = Destination::File(path(&value("--error-log")?)),
                other => {
                    return Err(format!("unknown option {}", String::from_utf8_lossy(other)));
                }
            }
        }
        let tls = tls(listen_tls, certificates)?;
        if listen.is_none() && tls.is_none() {
            return Err("no --listen nor --listen-tls address".to_owned());
        }
        let root = root.ok_or("no --root directory")?;
        let document_root = std::fs::canonicalize(php_sapi::cgi::path_of(&root))
            .map_err(|e| format!("--root {}: {e}", String::from_utf8_lossy(&root)))?;
        let request_threads = threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
        });
        Ok(Options {
            listen,
            tls,
            document_root,
            request_threads,
            request_queue: queue.unwrap_or(request_threads.saturating_mul(4)),
            stack_size: php_sapi::REQUEST_STACK,
            hidden,
            ini,
            ini_search,
            access_log,
            error_log,
            limits,
        })
    }
}

/// One `--certificate [<host>=]<chain.pem>,<key.pem>`: the host, where
/// one names its own, and the files. A host holds no `/` nor `,`, so a
/// path with `=` in it reads as no host.
fn certificate(value: &[u8]) -> Result<(Option<String>, CertificateFiles), String> {
    let refused = || {
        format!(
            "--certificate {} is not [<host>=]<chain.pem>,<key.pem>",
            String::from_utf8_lossy(value)
        )
    };
    let (host, files) = match value.iter().position(|b| *b == b'=') {
        Some(at) if !value[..at].iter().any(|b| matches!(b, b'/' | b',')) => {
            let host = std::str::from_utf8(&value[..at]).map_err(|_| refused())?;
            // A name SNI can send: no wildcard, address or port, which no
            // handshake would match.
            if rustls::pki_types::DnsName::try_from(host).is_err() {
                return Err(format!(
                    "--certificate {host}=...: {host} is not a host name SNI sends"
                ));
            }
            (Some(host.to_ascii_lowercase()), &value[at + 1..])
        }
        _ => (None, value),
    };
    let comma = files.iter().position(|b| *b == b',').ok_or_else(refused)?;
    let (chain, key) = (&files[..comma], &files[comma + 1..]);
    if chain.is_empty() || key.is_empty() {
        return Err(refused());
    }
    Ok((
        host,
        CertificateFiles {
            chain: path(chain),
            key: path(key),
        },
    ))
}

/// The TLS listener the line asks for, with its certificates: one at
/// least, a host named once and one default at most.
fn tls(
    listen: Option<SocketAddr>,
    certificates: Vec<(Option<String>, CertificateFiles)>,
) -> Result<Option<Tls>, String> {
    let Some(listen) = listen else {
        return if certificates.is_empty() {
            Ok(None)
        } else {
            Err("--certificate without --listen-tls".to_owned())
        };
    };
    if certificates.is_empty() {
        return Err(
            "--listen-tls without a certificate: TLS needs at least one \
             --certificate [<host>=]<chain.pem>,<key.pem>"
                .to_owned(),
        );
    }
    let mut tls = Tls {
        listen,
        default: None,
        by_host: Vec::new(),
    };
    for (host, files) in certificates {
        match host {
            None if tls.default.is_some() => {
                return Err("a second --certificate without a host".to_owned());
            }
            None => tls.default = Some(files),
            Some(host) if tls.by_host.iter().any(|(had, _)| *had == host) => {
                return Err(format!("a second --certificate for {host}"));
            }
            Some(host) => tls.by_host.push((host, files)),
        }
    }
    Ok(Some(tls))
}

/// A path the line names, as its bytes are.
fn path(bytes: &[u8]) -> PathBuf {
    php_sapi::cgi::path_of(bytes)
}

/// An option's value read as a `T`.
fn read<T: std::str::FromStr>(name: &str, value: &[u8]) -> Result<T, String> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| format!("{name} {} is not read", String::from_utf8_lossy(value)))
}

/// An option's count of one or more.
fn option_count(name: &str, value: &[u8]) -> Result<usize, String> {
    at_least_one(read::<usize>(name, value)?).map_err(|why| format!("{name} {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Options, String> {
        Options::parse(&words.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn the_line_sets_the_server_and_php_s_ini() -> Result<(), String> {
        let root = std::env::temp_dir();
        let options = parse(&[
            "-n",
            "-d",
            "memory_limit=64M",
            "--listen",
            "127.0.0.1:0",
            "--root",
            root.to_str().unwrap_or("/"),
            "--threads",
            "3",
        ])?;
        assert!(options.ini_search.ignore);
        assert_eq!(options.ini, [(b"memory_limit".to_vec(), b"64M".to_vec())]);
        assert_eq!(options.request_threads, 3);
        assert_eq!(options.request_queue, 12);
        assert_eq!(options.stack_size, php_sapi::REQUEST_STACK);
        assert_eq!(options.hidden, Hidden::Refused);
        assert_eq!(options.limits, Limits::default());
        let served = parse(&["--listen", "127.0.0.1:0", "--root", "/", "--serve-hidden"])?;
        assert_eq!(served.hidden, Hidden::Served);
        Ok(())
    }

    #[test]
    fn each_limit_is_overridden_by_its_name() -> Result<(), String> {
        let root = std::env::temp_dir();
        let root = root.to_str().unwrap_or("/");
        let options = parse(&[
            "--listen",
            "127.0.0.1:0",
            "--root",
            root,
            "--limit",
            "request-line=100",
            "--limit",
            "header-count=3",
            "--limit",
            "header-bytes=200",
            "--limit",
            "body-bytes=10",
            "--limit",
            "header-timeout=500ms",
            "--limit",
            "handshake-timeout=700ms",
            "--limit",
            "keep-alive-idle=2s",
            "--limit",
            "body-data-rate=1000,1s",
            "--limit",
            "response-data-rate=2000,2s",
            "--limit",
            "requests-per-connection=2",
            "--limit",
            "connections=1",
            "--limit",
            "shutdown-timeout=3s",
            "--limit",
            "log-lines=7",
        ])?;
        assert_eq!(
            options.limits,
            Limits {
                request_line: 100,
                header_count: 3,
                header_bytes: 200,
                body_bytes: 10,
                header_timeout: Duration::from_millis(500),
                handshake_timeout: Duration::from_millis(700),
                keep_alive_idle: Duration::from_secs(2),
                min_body_rate: Some(MinDataRate {
                    bytes_per_second: NonZeroU64::new(1000).ok_or("1000")?,
                    grace: Duration::from_secs(1),
                }),
                min_response_rate: Some(MinDataRate {
                    bytes_per_second: NonZeroU64::new(2000).ok_or("2000")?,
                    grace: Duration::from_secs(2),
                }),
                requests_per_connection: 2,
                connections: 1,
                shutdown_timeout: Duration::from_secs(3),
                log_lines: 7,
            }
        );
        let off = parse(&[
            "--listen",
            "127.0.0.1:0",
            "--root",
            root,
            "--limit",
            "body-data-rate=off",
        ])?;
        assert_eq!(off.limits.min_body_rate, None);
        assert_eq!(off.limits.min_response_rate, Some(MinDataRate::default()));
        for (entry, refusal) in [
            ("nothing=1", "--limit nothing is not a limit"),
            (
                "header-count=0",
                "--limit header-count=0: must be at least 1",
            ),
            (
                "header-timeout=5",
                "--limit header-timeout=5: a time is <n>s or <n>ms",
            ),
            (
                "body-data-rate=240",
                "--limit body-data-rate=240: a rate is <bytes a second>,<grace time> or off",
            ),
            ("body-bytes", "--limit body-bytes is not <name>=<value>"),
            (
                "header-timeout=0ms",
                "--limit header-timeout=0ms: must be at least 1",
            ),
        ] {
            let refused = parse(&["--listen", "127.0.0.1:0", "--root", root, "--limit", entry]);
            assert_eq!(refused.err().as_deref(), Some(refusal));
        }
        Ok(())
    }

    #[test]
    fn a_data_rate_allows_its_grace_or_the_time_its_bytes_take() {
        let rate = MinDataRate {
            bytes_per_second: NonZeroU64::new(240).unwrap_or(NonZeroU64::MIN),
            grace: Duration::from_secs(5),
        };
        assert_eq!(rate.allowed(0), Duration::from_secs(5));
        assert_eq!(rate.allowed(2400), Duration::from_secs(10));
        assert_eq!(rate.time_for(120), Duration::from_millis(500));
    }

    #[test]
    fn a_line_without_an_address_or_with_no_thread_is_refused() {
        assert_eq!(
            parse(&["--root", "/"]).err().as_deref(),
            Some("no --listen nor --listen-tls address")
        );
        assert_eq!(
            parse(&["--listen", "127.0.0.1:0", "--root", "/", "--threads", "0"])
                .err()
                .as_deref(),
            Some("--threads must be at least 1")
        );
    }

    #[test]
    fn tls_listens_with_its_certificates_or_not_at_all() -> Result<(), String> {
        let tls = parse(&[
            "--listen-tls",
            "127.0.0.1:0",
            "--root",
            "/",
            "--certificate",
            "/c/default.pem,/c/default.key",
            "--certificate",
            "A.Test=/c/a.pem,/c/a.key",
        ])?;
        assert_eq!(tls.listen, None);
        let files = |name: &str| CertificateFiles {
            chain: PathBuf::from(format!("/c/{name}.pem")),
            key: PathBuf::from(format!("/c/{name}.key")),
        };
        assert_eq!(
            tls.tls,
            Some(Tls {
                listen: "127.0.0.1:0".parse().map_err(|_| "addr")?,
                default: Some(files("default")),
                by_host: vec![("a.test".to_owned(), files("a"))],
            })
        );
        for (line, refusal) in [
            (
                &["--listen-tls", "127.0.0.1:0"][..],
                "--listen-tls without a certificate: TLS needs at least one \
                 --certificate [<host>=]<chain.pem>,<key.pem>",
            ),
            (
                &[
                    "--listen",
                    "127.0.0.1:0",
                    "--certificate",
                    "/c/a.pem,/c/a.key",
                ][..],
                "--certificate without --listen-tls",
            ),
            (
                &["--listen-tls", "127.0.0.1:0", "--certificate", "/c/a.pem"][..],
                "--certificate /c/a.pem is not [<host>=]<chain.pem>,<key.pem>",
            ),
            (
                &[
                    "--listen-tls",
                    "127.0.0.1:0",
                    "--certificate",
                    "a.test=/c/a.pem,/c/a.key",
                    "--certificate",
                    "A.TEST=/c/b.pem,/c/b.key",
                ][..],
                "a second --certificate for a.test",
            ),
            (
                &[
                    "--listen-tls",
                    "127.0.0.1:0",
                    "--certificate",
                    "*.test=/c/a.pem,/c/a.key",
                ][..],
                "--certificate *.test=...: *.test is not a host name SNI sends",
            ),
        ] {
            let mut words = line.to_vec();
            words.extend(["--root", "/"]);
            assert_eq!(parse(&words).err().as_deref(), Some(refusal), "{line:?}");
        }
        Ok(())
    }
}
