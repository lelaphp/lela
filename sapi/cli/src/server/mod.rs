//! php's built-in web server, `php -S` (`Z/sapi/cli/php_cli_server.c`):
//! the document root and the address read as `do_cli_server` reads
//! them, a listening socket as `php_network_listen_socket` binds it, and
//! php's event loop on the one thread the process has: every connection
//! waited on at once, each request read as its bytes arrive, so a client
//! that sends slowly holds no other back, and the requests served through
//! the engine one after another on that thread ([`client`]), as
//! `php_cli_server_do_event_loop` serves them (design §4.8, revision 122). No
//! request is ever in flight beside another, so a `pcntl_fork` from one
//! forks as under the CLI, and the child serves on in the loop it was
//! forked from, as php's does. `PHP_CLI_SERVER_WORKERS` forks the server
//! into worker processes before it serves, each through the engine's
//! one home for fork and each with a loop of its own on the same socket
//! ([`workers`]). The log is php's: the startup line, one line as
//! each connection is accepted and closed, one per response, each
//! after the local time `asctime` writes, on stderr, as many as the level
//! `-q` lowers leaves.

mod client;
mod log;
mod workers;

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use php_sapi::{Engine, PHP_VERSION};

use crate::ffi::{Interest, Polled};

use crate::options::Server;
use client::Connection;
use log::{Log, Type};

/// `sapi_module.name` for the built-in server: what `php_sapi_name()`
/// answers under it.
pub(crate) const NAME: &str = "cli-server";

/// How long the loop waits before it reads the atomic a `SIGINT` sets
/// again, in milliseconds: `php_cli_server_do_event_loop`'s `tv` of one
/// second.
const WAIT: i32 = 1000;

/// `php_cli_server_log_level`'s default (`Z/sapi/cli/php_cli_server.c:223`),
/// which each `-q` lowers.
const LOG_LEVEL: u8 = 3;

/// `sapi_cli_server_log_message` while php starts, before `-q` lowers the
/// level (`Z/sapi/cli/php_cli_server.c:797-800`): the entry as a message,
/// at the default level, which shows it.
pub(crate) fn log_message(entry: &[u8]) {
    Log::new(LOG_LEVEL, false).write(Type::Message, &String::from_utf8_lossy(entry));
}

/// What every connection of one server reads: the engine, the document
/// root and the router, the `host:port` the server listens on, and
/// the log. Shared with the sink a request holds, which the engine may
/// keep past the call that serves it.
struct Site {
    engine: Engine,
    document_root: Vec<u8>,
    router: Option<Vec<u8>>,
    host: String,
    port: u16,
    log: Log,
}

/// `do_cli_server` (`Z/sapi/cli/php_cli_server.c:2808`): the document root
/// `-t` names, which must be a directory and is served by its real path,
/// or the working directory; the router after the switches; the address
/// parsed and listened on; the workers `PHP_CLI_SERVER_WORKERS` asks
/// for forked; the startup line, in each process; and then each
/// connection served until a `SIGINT` stops the loop, as
/// `php_cli_server_sigint_handler` stops it, after which the listening
/// socket is closed and each worker this process forked waited for, as
/// `php_cli_server_dtor` closes and waits. The exit code: 0 once
/// stopped, 1 where the wait failed, what refused it where it does not
/// start.
pub fn serve(engine: Engine, server: &Server, color: bool) -> i32 {
    let document_root = match &server.document_root {
        Some(root) => {
            let shown = root.display();
            match std::fs::metadata(root) {
                Err(_) => return refuse(&format!("Directory {shown} does not exist.\n")),
                Ok(meta) if !meta.is_dir() => {
                    return refuse(&format!("{shown} is not a directory.\n"));
                }
                Ok(_) => std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()),
            }
        }
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let mut log = Log::new(LOG_LEVEL.saturating_sub(server.quiet).max(1), color);
    let address = String::from_utf8_lossy(&server.address).into_owned();
    let Some((host, port)) = parse_address(&address) else {
        return refuse(&format!("Invalid address: {address}\n"));
    };
    let (listener, port) = match listen(&host, port) {
        Ok(bound) => bound,
        Err(reason) => {
            let at = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            log.write(
                Type::Error,
                &format!("Failed to listen on {at} (reason: {reason})"),
            );
            return 1;
        }
    };
    // Non-blocking, so a connection another process accepted first leaves
    // this one's `accept` empty rather than stalled (`php_cli_server_ctor`,
    // `Z/sapi/cli/php_cli_server.c:2577-2584`).
    if listener.set_nonblocking(true).is_err() {
        log.write(Type::Error, "Failed to make server socket non-blocking");
        return 1;
    }
    let workers = workers::Workers::start(&log);
    if workers.many() {
        log.name_processes();
    }
    let (open, close) = if host.contains(':') {
        ("[", "]")
    } else {
        ("", "")
    };
    log.write(
        Type::Process,
        &format!(
            "PHP {PHP_VERSION} Development Server (http://{open}{host}{close}:{port}) started"
        ),
    );
    let site = Arc::new(Site {
        engine,
        document_root: document_root.into_os_string().into_encoded_bytes(),
        router: server
            .router
            .as_ref()
            .map(|router| router.as_os_str().as_encoded_bytes().to_vec()),
        host,
        port,
        log,
    });
    let status = serve_until_stopped(&site, &listener);
    drop(listener);
    workers.wait();
    status
}

/// `php_cli_server_do_event_loop` (`Z/sapi/cli/php_cli_server.c:2770-2797`):
/// a wait of at most a second on the listening socket and every open
/// connection, each for what it waits on, then each that is ready
/// answered in the order of its fd, as `php_cli_server_poller_iter_on_active`
/// (`Z/sapi/cli/php_cli_server.c:883`) walks them: the listening socket's one connection accepted, a
/// request read on and served once whole, a response sent on. The atomic a
/// `SIGINT` sets is read before each wait; once it is set every
/// connection still open is shut and dropped with nothing logged, as
/// `php_cli_server_dtor` destroys the clients. A request in flight when
/// the signal came is on this thread and finishes first, as in php's one
/// loop. The engine is then held by `site` alone, and the caller's drop
/// of it ends the engine, as the CLI's `php_module_shutdown` follows
/// `do_cli_server`. The exit code, `do_cli_server`'s `r`: 0, or 1 where
/// the wait failed, after php's line.
fn serve_until_stopped(site: &Arc<Site>, listener: &TcpListener) -> i32 {
    let stop = crate::ffi::stop_on_sigint();
    let listening = listener.as_raw_fd();
    let mut clients: BTreeMap<RawFd, Connection> = BTreeMap::new();
    let mut status = 0;
    while !stop.load(Ordering::Relaxed) {
        let mut polled: Vec<Polled> = std::iter::once(Polled::new(listening, Interest::Read))
            .chain(clients.values().filter_map(Connection::polled))
            .collect();
        polled.sort_by_key(Polled::fd);
        match crate::ffi::poll(&mut polled, WAIT) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                site.log.write(Type::Error, &php_sapi::strerror(&error));
                status = 1;
                break;
            }
        }
        let mut failed = false;
        for fd in polled.iter().filter(|fd| fd.ready()).map(Polled::fd) {
            if fd == listening {
                failed |= !accept(site, listener, &mut clients);
            } else if let Some(connection) = clients.remove(&fd) {
                let connection = connection.ready(site);
                if connection.is_open() {
                    clients.insert(fd, connection);
                }
            }
        }
        if failed {
            site.log.write(Type::Error, "Failed to poll event");
        }
    }
    for connection in clients.into_values() {
        connection.drop_unlogged();
    }
    status
}

/// `accept` on the listening socket, as
/// `php_cli_server_do_event_for_each_fd_callback`
/// (`Z/sapi/cli/php_cli_server.c:2704`) answers it: the
/// connection made non-blocking, `%s Accepted` logged and its request
/// waited on; nothing where another process took the connection first.
/// False where the accept failed, after php's line, or the connection
/// could not be made non-blocking, which the loop answers with
/// `Failed to poll event` once the pass is over, as
/// `php_cli_server_do_event_for_each_fd` (`:2755`) does.
fn accept(site: &Site, listener: &TcpListener, clients: &mut BTreeMap<RawFd, Connection>) -> bool {
    match listener.accept() {
        Ok((stream, peer)) => {
            if stream.set_nonblocking(true).is_err() {
                return false;
            }
            let name = peer_name(peer);
            site.log.write(Type::Message, &format!("{name} Accepted"));
            clients.insert(stream.as_raw_fd(), Connection::accepted(stream, name));
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(error) => {
            site.log.write(
                Type::Error,
                &format!(
                    "Failed to accept a client (reason: {})",
                    php_sapi::strerror(&error)
                ),
            );
            false
        }
    }
}

/// A line on stderr before the server starts, and exit 1.
fn refuse(line: &str) -> i32 {
    use std::io::Write;
    std::io::stderr().lock().write_all(line.as_bytes()).ok();
    1
}

/// `php_cli_server_parse_addr` (`Z/sapi/cli/php_cli_server.c:2432`):
/// `host:port` or `[host]:port`, the `port` all digits `strtol` reads and
/// at most 65535.
fn parse_address(address: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        (host, after.strip_prefix(':')?)
    } else {
        address.split_once(':')?
    };
    Some((host.to_owned(), strtol_port(port)?))
}

/// `strtol` over the whole of `digits`, base 10, with a sign and leading
/// blanks as `strtol` takes them, held to 0..=65535.
fn strtol_port(digits: &str) -> Option<u16> {
    let text = digits.trim_start();
    let (negative, number) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return (number.is_empty() && digits.is_empty()).then_some(0);
    }
    let value: u64 = number.parse().ok()?;
    if negative && value != 0 {
        return None;
    }
    u16::try_from(value).ok()
}

/// `php_network_listen_socket` (`Z/sapi/cli/php_cli_server.c:1283`): each
/// address the host resolves to bound in turn with `SO_REUSEADDR`, an
/// address in use or refused as invalid ending the search, and the `port`
/// the system chose where `0` was asked; `listen`. The reason where none
/// binds, `?` where php kept none.
fn listen(host: &str, port: u16) -> Result<(TcpListener, u16), String> {
    let addresses = (host, port).to_socket_addrs().map_err(|error| {
        let text = php_sapi::strerror(&error);
        let gai = text
            .strip_prefix("failed to lookup address information: ")
            .unwrap_or(&text);
        format!("php_network_getaddresses: getaddrinfo for {host} failed: {gai}")
    })?;
    for address in addresses {
        match TcpListener::bind(address) {
            Ok(listener) => {
                let port = listener.local_addr().map_or(port, |bound| bound.port());
                return Ok((listener, port));
            }
            Err(error) if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::EADDRINUSE)) => {
                return Err(php_sapi::strerror(&error));
            }
            Err(_) => {}
        }
    }
    Err(String::from("?"))
}

/// `php_network_populate_name_from_sockaddr`: `a.b.c.d:port`, or
/// `[v6]:port`.
fn peer_name(peer: SocketAddr) -> String {
    match peer {
        SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()),
        SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_a_host_and_a_port_as_php_parses_it() {
        assert_eq!(
            parse_address("localhost:0"),
            Some((String::from("localhost"), 0))
        );
        assert_eq!(
            parse_address("[::1]:8080"),
            Some((String::from("::1"), 8080))
        );
        assert_eq!(parse_address("localhost"), None);
        assert_eq!(parse_address("localhost:x"), None);
        assert_eq!(parse_address("localhost:65536"), None);
        assert_eq!(parse_address("[::1]8080"), None);
        assert_eq!(parse_address("[::1"), None);
    }
}
