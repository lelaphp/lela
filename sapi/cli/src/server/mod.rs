//! php's built-in web server, `php -S` (`Z/sapi/cli/php_cli_server.c`):
//! the document root and the address read as `do_cli_server` reads
//! them, a listening socket as `php_network_listen_socket` binds it, and
//! each connection it accepts served on a thread of its own through the
//! engine ([`client`]), where php serves them one after another in its
//! event loop, or forks workers for `PHP_CLI_SERVER_WORKERS`. The log is
//! php's: the startup line, one line as each connection is accepted and
//! closed, one per response, each after the local time `asctime` writes,
//! on stderr, as many as the level `-q` lowers leaves.

mod client;
mod log;

use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use php_sapi::{Engine, PHP_VERSION};

use crate::options::Server;
use log::{Log, Type};

/// `sapi_module.name` for the built-in server: what `php_sapi_name()`
/// answers under it.
pub(crate) const NAME: &str = "cli-server";

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
/// root and the router, the host and port the server listens on, and
/// the log.
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
/// parsed and listened on; the startup line; and then each connection
/// served until a `SIGINT` stops the loop, as
/// `php_cli_server_sigint_handler` stops it. The exit code: 0 once
/// stopped, what refused it where it does not start.
pub fn run(engine: Engine, server: &Server, color: bool) -> i32 {
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
    let log = Log::new(LOG_LEVEL.saturating_sub(server.quiet).max(1), color);
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
    serve_until_stopped(&site, &listener);
    0
}

/// `php_cli_server_do_event_loop` (`Z/sapi/cli/php_cli_server.c:2770-2797`)
/// with each connection served on a thread of its own: a wait of at most
/// a second for a connection, the flag a `SIGINT` sets read after each,
/// and once it is set no connection accepted any more, every one still
/// open told nothing more is read from it, as `php_cli_server_dtor`
/// closes the clients, and every thread joined, so the request a thread
/// was serving finishes as php's one loop finishes it. The engine is then
/// held by `site` alone, and the caller's drop of it ends the engine, as
/// the CLI's `php_module_shutdown` follows `do_cli_server`.
fn serve_until_stopped(site: &Arc<Site>, listener: &TcpListener) {
    let stop = crate::ffi::stop_on_sigint();
    let _ = listener.set_nonblocking(true);
    let mut clients: Vec<(Option<TcpStream>, JoinHandle<()>)> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        clients.retain(|(_, thread)| !thread.is_finished());
        if !crate::ffi::wait_readable(listener.as_raw_fd(), 1000) {
            continue;
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                let _ = stream.set_nonblocking(false);
                let held = stream.try_clone().ok();
                let site = Arc::clone(site);
                let name = peer_name(peer);
                site.log.write(Type::Message, &format!("{name} Accepted"));
                let thread = std::thread::spawn(move || client::serve(&site, stream, name));
                clients.push((held, thread));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => site.log.write(
                Type::Error,
                &format!("Failed to accept a client (reason: {})", strerror(&error)),
            ),
        }
    }
    for (held, _) in &clients {
        if let Some(stream) = held {
            let _ = stream.shutdown(Shutdown::Read);
        }
    }
    for (_, thread) in clients {
        let _ = thread.join();
    }
}

/// A line on stderr before the server starts, and exit 1.
fn refuse(line: &str) -> i32 {
    use std::io::Write;
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
    1
}

/// `php_cli_server_parse_addr` (`Z/sapi/cli/php_cli_server.c:2432`):
/// `host:port` or `[host]:port`, the port all digits `strtol` reads and
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
/// address in use or refused as invalid ending the search, and the port
/// the system chose where `0` was asked; `listen`. The reason where none
/// binds, `?` where php kept none.
fn listen(host: &str, port: u16) -> Result<(TcpListener, u16), String> {
    let addresses = (host, port).to_socket_addrs().map_err(|error| {
        let text = strerror(&error);
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
                return Err(strerror(&error));
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

/// `strerror(errno)`'s text for an error the system raised, without the
/// ` (os error N)` Rust adds.
fn strerror(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
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
