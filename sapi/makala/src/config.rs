//! What the operator configures, read from the line: where the server
//! listens, the document root, the request pool and its queue, the php
//! ini the engine starts with, and the table of limits (#2337's
//! Implementation Decisions), each overridable. The connection side reads
//! the limits it applies; the rest of the table is what #2341 enforces.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use php_sapi::ini::Search;

/// The line's grammar, printed with a refusal.
pub const USAGE: &str = "Usage: makala [-n] [-c <path>] [-d foo[=bar]]... --listen <addr>:<port> --root <dir> [--threads <n>] [--queue <n>]\n";

/// The stack of each request thread, php-fpm's main thread's on the
/// reference machine (#2335): the engine reads the serving thread's
/// bounds for php's "Maximum call stack size", so a program sees this.
pub const REQUEST_STACK: usize = 8 << 20;

/// The server as the operator configured it.
#[derive(Debug, Clone)]
pub struct Config {
    /// The address the listening socket binds; port 0 takes one the
    /// system picks, which the started line names.
    pub listen: SocketAddr,
    /// The document root, made absolute: what a request's path is found
    /// under, and never left (`php -S`'s `normalize_vpath`).
    pub document_root: PathBuf,
    /// How many requests run at once, each on a thread of its own.
    pub request_threads: usize,
    /// How many whole requests wait for a free request thread; a request
    /// past them waits on its connection for room.
    pub request_queue: usize,
    /// The stack of each request thread, [`REQUEST_STACK`].
    pub stack_size: usize,
    /// The `-d` entries, in the order given, over the ini files.
    pub ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// `-n` and `-c`, which steer the ini file search.
    pub ini_search: Search,
    pub limits: Limits,
}

/// The limits, with Kestrel's defaults, which are IIS's, Apache's and
/// nginx's (research/production-server.r2.md §1.3, §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The request line, in bytes: 8 KB.
    pub request_line: usize,
    /// How many header fields a request may carry: 100.
    pub header_count: usize,
    /// The header block, in bytes: 32 KB.
    pub header_bytes: usize,
    /// The body, in bytes, apart from php's `post_max_size`: 30,000,000.
    pub body_bytes: u64,
    /// How long a request's head may take to arrive: 30 s.
    pub header_timeout: Duration,
    /// How long a kept-alive connection may wait for its next request:
    /// 130 s.
    pub keep_alive_idle: Duration,
    /// The least a client must send or take each second once the grace
    /// has passed: 240 bytes.
    pub min_data_rate: u64,
    /// The grace before [`Limits::min_data_rate`] holds: 5 s.
    pub min_data_rate_grace: Duration,
    /// How many requests one connection may make: 1000, Cowboy's
    /// `max_keepalive`.
    pub requests_per_connection: u32,
    /// How many connections the server holds open at once, past which it
    /// stops accepting and the kernel's queue holds the rest: 10,000.
    pub connections: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            request_line: 8 * 1024,
            header_count: 100,
            header_bytes: 32 * 1024,
            body_bytes: 30_000_000,
            header_timeout: Duration::from_secs(30),
            keep_alive_idle: Duration::from_secs(130),
            min_data_rate: 240,
            min_data_rate_grace: Duration::from_secs(5),
            requests_per_connection: 1000,
            connections: 10_000,
        }
    }
}

impl Config {
    /// The line, past the binary's name: php's `-n`, `-c` and `-d`, and
    /// the server's own options; a refusal says what was wrong.
    ///
    /// # Errors
    ///
    /// What the line got wrong: an unknown option, a value missing or not
    /// read, or no `--listen` or `--root`.
    pub fn parse(argv: &[OsString]) -> Result<Config, String> {
        let mut words = argv.iter();
        let mut listen = None;
        let mut root = None;
        let mut threads = None;
        let mut queue = None;
        let mut ini = Vec::new();
        let mut ini_search = Search::default();
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
                b"--root" => root = Some(value("--root")?),
                b"--threads" => threads = Some(count("--threads", &value("--threads")?)?),
                b"--queue" => queue = Some(count("--queue", &value("--queue")?)?),
                other => {
                    return Err(format!("unknown option {}", String::from_utf8_lossy(other)));
                }
            }
        }
        let listen = listen.ok_or("no --listen address")?;
        let root = root.ok_or("no --root directory")?;
        let document_root = std::fs::canonicalize(php_sapi::cgi::path_of(&root))
            .map_err(|e| format!("--root {}: {e}", String::from_utf8_lossy(&root)))?;
        let request_threads = threads.unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
        });
        Ok(Config {
            listen,
            document_root,
            request_threads,
            request_queue: queue.unwrap_or(request_threads.saturating_mul(4)),
            stack_size: REQUEST_STACK,
            ini,
            ini_search,
            limits: Limits::default(),
        })
    }
}

/// An option's value read as a `T`.
fn read<T: std::str::FromStr>(name: &str, value: &[u8]) -> Result<T, String> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| format!("{name} {} is not read", String::from_utf8_lossy(value)))
}

/// A count of one or more.
fn count(name: &str, value: &[u8]) -> Result<usize, String> {
    match read::<usize>(name, value)? {
        0 => Err(format!("{name} must be at least 1")),
        n => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Config, String> {
        Config::parse(&words.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn the_line_sets_the_server_and_php_s_ini() -> Result<(), String> {
        let root = std::env::temp_dir();
        let config = parse(&[
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
        assert!(config.ini_search.ignore);
        assert_eq!(config.ini, [(b"memory_limit".to_vec(), b"64M".to_vec())]);
        assert_eq!(config.request_threads, 3);
        assert_eq!(config.request_queue, 12);
        assert_eq!(config.stack_size, 8 << 20);
        assert_eq!(config.limits, Limits::default());
        Ok(())
    }

    #[test]
    fn a_line_without_an_address_or_with_no_thread_is_refused() {
        assert_eq!(
            parse(&["--root", "/"]).err().as_deref(),
            Some("no --listen address")
        );
        assert_eq!(
            parse(&["--listen", "127.0.0.1:0", "--root", "/", "--threads", "0"])
                .err()
                .as_deref(),
            Some("--threads must be at least 1")
        );
    }
}
