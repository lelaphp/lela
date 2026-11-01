//! php-cgi's `FastCGI` serving, `php-cgi -b <path>` (`Z/sapi/cgi/cgi_main.c:1931-2140`,
//! `:2412-2640`): the socket `fcgi_listen` binds, the children
//! `PHP_FCGI_CHILDREN` pre-forks and keeps, and in each process the loop
//! `fcgi_accept_request` drives, each request's variables read from its
//! `FCGI_PARAMS` as `init_request_info` reads them and the request served
//! through a sink of `FCGI_STDOUT` records, until `PHP_FCGI_MAX_REQUESTS`.

use std::io::Write as _;
use std::net::{IpAddr, TcpListener};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use php_proto::cgi::{Environment, RequestInfo};
use php_proto::fcgi::{self, Kind, Outgoing};
use php_sapi::cgi::{C_NUMBER, CachedDisk};
use php_sapi::fcgi::{Conn, allowed, allowed_clients};
use php_sapi::{
    Aborted, Body, Chdir, Engine, Forked, HostServes, IniOrigin, OutputSink, RequestDescription,
    ResponseHead, WebRequest,
};

use crate::{NAME, refusal};

/// `MIN(SOMAXCONN, 128)`, `fcgi_listen`'s `backlog` where
/// `PHP_FCGI_BACKLOG` names none.
const BACKLOG: i32 = 128;

/// `SOMAXCONN`, the most `PHP_FCGI_BACKLOG` may ask for.
const SOMAXCONN: i32 = nix::libc::SOMAXCONN;

/// `main`'s `max_requests` where `PHP_FCGI_MAX_REQUESTS` names none.
const MAX_REQUESTS: i32 = 500;

/// `main` returning `FAILURE`, which the process ends with.
const FAILURE: i32 = 255;

/// The socket `fcgi_listen` bound.
enum Listening {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listening {
    /// `fcgi_accept_request`'s `accept`: a connection and its peer, which
    /// a Unix socket's names none; `None` once a `SIGTERM` has asked the
    /// process to finish or the socket fails.
    ///
    /// A stop handled after the check and before `accept` has made the
    /// number name a socket listening on nothing
    /// ([`crate::ffi::accepting_on`]), so `accept` fails and the loop ends
    /// as the check would have ended it.
    fn accept(&self) -> Option<(Conn, Option<IpAddr>)> {
        loop {
            if crate::ffi::shutting_down() {
                return None;
            }
            match crate::ffi::accept(self.fd()) {
                Ok(fd) => match self {
                    Listening::Tcp(_) => {
                        let s = std::net::TcpStream::from(fd);
                        let Ok(peer) = s.peer_addr() else {
                            continue;
                        };
                        return Some((Conn::Tcp(s), Some(peer.ip())));
                    }
                    Listening::Unix(_) => {
                        return Some((Conn::Unix(std::os::unix::net::UnixStream::from(fd)), None));
                    }
                },
                Err(nix::errno::Errno::EINTR | nix::errno::Errno::ECONNABORTED) => {}
                Err(_) => return None,
            }
        }
    }

    fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        match self {
            Listening::Tcp(socket) => socket.as_fd(),
            Listening::Unix(socket) => socket.as_fd(),
        }
    }
}

impl Drop for Listening {
    /// The number handed back before it is closed, so a stop handled
    /// after it touches nothing the number names next.
    fn drop(&mut self) {
        crate::ffi::not_accepting_on(self.fd());
    }
}

/// `fcgi_listen(path, backlog)`: `host:port` and a bare `port` bind TCP,
/// every address where the host is empty or `*`, and anything else names
/// a Unix socket, which is opened to everyone (`chmod(path, 0777)`). A
/// path `sun_path` cannot hold is refused with `Listening socket's path
/// name is too long.` before a socket is made, as php's
/// `path_len >= sizeof(sa.sa_unix.sun_path)` refuses it. A path it holds
/// is unlinked before the socket is made, as php's `unlink(path)` is
/// (`Z/main/fastcgi.c:744`), so a file left there, a socket a killed
/// php-cgi left among them, is replaced by the one bound; a step of [`bound`] that
/// fails is said with its own line.
fn listen(path: &[u8], backlog: i32) -> Option<Listening> {
    use nix::sys::socket::{AddressFamily, Backlog, SockaddrIn, UnixAddr};
    let tcp = match path.iter().position(|b| *b == b':') {
        Some(at) => {
            let port = (C_NUMBER.atoi)(&path[at + 1..]);
            (port != 0).then(|| (&path[..at], port))
        }
        None if !path.is_empty() && path.iter().all(u8::is_ascii_digit) => {
            let port = (C_NUMBER.atoi)(path);
            (port != 0).then_some((&b""[..], port))
        }
        None => None,
    };
    let backlog = Backlog::new(backlog).ok()?;
    if let Some((host, port)) = tcp {
        let port = u16::try_from(port).ok()?;
        let ip = if host.is_empty() || host.starts_with(b"*") {
            std::net::Ipv4Addr::UNSPECIFIED
        } else {
            resolve(host)?
        };
        let address = SockaddrIn::from(std::net::SocketAddrV4::new(ip, port));
        let fd = bound(AddressFamily::Inet, &address, backlog)?;
        crate::ffi::shutdown_on_term();
        return Some(Listening::Tcp(TcpListener::from(fd)));
    }
    let name = php_sapi::cgi::path_of(path);
    // `UnixAddr::new` refuses a path of `sun_path`'s length or more, as
    // php's check does, and nothing else a command line can hold.
    let Ok(address) = UnixAddr::new(&name) else {
        eprintln!("Listening socket's path name is too long.");
        return None;
    };
    // php ignores `unlink`'s failure: what cannot be removed, a directory,
    // is left for `bind` to refuse.
    std::fs::remove_file(&name).ok();
    let fd = bound(AddressFamily::Unix, &address, backlog)?;
    std::fs::set_permissions(&name, std::os::unix::fs::PermissionsExt::from_mode(0o777)).ok();
    crate::ffi::shutdown_on_term();
    Some(Listening::Unix(UnixListener::from(fd)))
}

/// `fcgi_listen`'s `socket`, `setsockopt(SO_REUSEADDR)`, `bind` and
/// `listen`, in that order: the socket listening, or `Cannot bind/listen
/// socket - [<errno>] <strerror>.` where a step fails, as php writes it
/// after `close(listen_socket)`. That `close` leaves a failed `bind`'s or
/// `listen`'s errno standing, and where `socket` itself failed it is
/// `close(-1)`, so the errno php prints is its `EBADF`.
fn bound(
    family: nix::sys::socket::AddressFamily,
    address: &dyn nix::sys::socket::SockaddrLike,
    backlog: nix::sys::socket::Backlog,
) -> Option<std::os::fd::OwnedFd> {
    use nix::errno::Errno;
    use nix::sys::socket::{SockFlag, SockType, bind, setsockopt, socket, sockopt};
    let made = socket(family, SockType::Stream, SockFlag::empty(), None)
        .map_err(|_| Errno::EBADF)
        .and_then(|fd| {
            setsockopt(&fd, sockopt::ReuseAddr, &true)?;
            bind(fd.as_raw_fd(), address)?;
            nix::sys::socket::listen(&fd, backlog)?;
            Ok(fd)
        });
    match made {
        Ok(fd) => Some(fd),
        Err(errno) => {
            eprintln!(
                "Cannot bind/listen socket - [{}] {}.",
                errno as i32,
                php_sapi::strerror(&std::io::Error::from(errno))
            );
            None
        }
    }
}

/// `inet_addr(host)`, and `gethostbyname` where it is no address:
/// `fcgi_listen`'s `Cannot resolve host name` where neither answers.
fn resolve(host: &[u8]) -> Option<std::net::Ipv4Addr> {
    let text = std::str::from_utf8(host).ok()?;
    if let Ok(ip) = text.parse() {
        return Some(ip);
    }
    let found = std::net::ToSocketAddrs::to_socket_addrs(&(text, 0))
        .ok()
        .and_then(|mut addrs| {
            addrs.find_map(|addr| match addr.ip() {
                IpAddr::V4(ip) => Some(ip),
                IpAddr::V6(_) => None,
            })
        });
    if found.is_none() {
        eprintln!("Cannot resolve host name '{text}'!");
    }
    found
}

/// What one process serves through: the connection, the request's
/// `FCGI_STDOUT` and `FCGI_STDERR` records and its body's input records.
struct Wire {
    conn: Option<Conn>,
    out: Outgoing,
    input: fcgi::Stdin,
    keep: bool,
}

impl Wire {
    /// `sapi_fcgi_read_post`: up to `buf.len()` bytes of the body
    /// ([`fcgi::Stdin::read`]); none past a read that fails, after which
    /// the connection is not kept.
    fn read_body(&mut self, buf: &mut [u8]) -> usize {
        let Some(conn) = &mut self.conn else {
            return 0;
        };
        let read = self.input.read(conn, buf);
        if read.failed {
            self.keep = false;
        }
        read.n
    }

    /// `fcgi_write`: whether the bytes went, which a connection the web
    /// server closed refuses; none open takes them silently.
    fn write(&mut self, kind: Kind, bytes: &[u8]) -> bool {
        if let Some(conn) = &mut self.conn
            && self.out.write(conn, kind, bytes).is_err()
        {
            self.keep = false;
            return false;
        }
        true
    }

    /// `fcgi_finish_request(req, force)`: `FCGI_END_REQUEST` sent where
    /// the connection is open, and the connection closed unless kept.
    fn finish(&mut self, force: bool) {
        let Some(conn) = &mut self.conn else {
            return;
        };
        if self.out.end(conn).is_err() {
            self.keep = false;
        }
        if (force || !self.keep)
            && let Some(conn) = self.conn.take()
        {
            conn.close(force);
        }
    }
}

/// A request's sink: `sapi_cgi_ub_write` and its kin over `FCGI_STDOUT`.
struct Response {
    wire: Arc<Mutex<Wire>>,
    /// `CGIG(fcgi_logging)`, `fastcgi.logging`.
    logging: bool,
}

impl Response {
    fn wire(&self) -> MutexGuard<'_, Wire> {
        lock(&self.wire)
    }
}

impl OutputSink for Response {
    /// `sapi_cgi_ub_write`: a write the web server's connection refuses
    /// is `php_handle_aborted_connection`, with no status set.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        if self.wire().write(Kind::Stdout, bytes) {
            Ok(())
        } else {
            Err(Aborted { exit_status: None })
        }
    }

    /// `sapi_cgi_flush`: `fcgi_flush(request, 0)`.
    fn flush(&mut self) {
        let mut wire = self.wire();
        let Wire {
            conn, out, keep, ..
        } = &mut *wire;
        if let Some(conn) = conn
            && out.flush(conn, false).is_err()
        {
            *keep = false;
        }
    }

    /// `sapi_cgi_send_headers`, the head [`php_sapi::cgi::head`] writes.
    fn send_headers(&mut self, head: &ResponseHead) -> Result<(), Aborted> {
        let block = php_sapi::cgi::head(head);
        self.wire().write(Kind::Stdout, &block);
        Ok(())
    }

    /// `sapi_cgi_log_message`: the entry and `\n` as the request's
    /// `FCGI_STDERR` while `fastcgi.logging` is on, else on stderr.
    fn log(&mut self, entry: &[u8]) {
        if self.logging {
            let mut line = entry.to_vec();
            line.push(b'\n');
            self.wire().write(Kind::Stderr, &line);
        } else {
            let mut stderr = std::io::stderr().lock();
            stderr.write_all(entry).ok();
            stderr.write_all(b"\n").ok();
        }
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().lock().write_all(bytes).ok();
    }

    /// `sapi_cgi_flush` sends the head before it flushes.
    fn flush_sends_head(&self) -> bool {
        true
    }

    /// `fastcgi_finish_request`'s `fcgi_end` and `fcgi_close(req, 0, 0)`.
    fn finish(&mut self) -> bool {
        let mut wire = self.wire();
        wire.finish(false);
        wire.conn.is_none()
    }
}

/// The variables `main` reads before it serves, each refused as php
/// refuses it.
struct Limits {
    max_requests: i32,
    children: i32,
}

impl Limits {
    /// `PHP_FCGI_MAX_REQUESTS` and `PHP_FCGI_CHILDREN`, read with `atoi`
    /// as `main` reads them, a negative one refused with php's line.
    fn read(env: &Environment) -> Option<Limits> {
        let mut max_requests = MAX_REQUESTS;
        if let Some(value) = env.get(b"PHP_FCGI_MAX_REQUESTS") {
            max_requests = (C_NUMBER.atoi)(value);
            if max_requests < 0 {
                eprintln!("PHP_FCGI_MAX_REQUESTS is not valid");
                return None;
            }
        }
        let mut children = 0;
        if let Some(value) = env.get(b"PHP_FCGI_CHILDREN") {
            children = (C_NUMBER.atoi)(value);
            if children < 0 {
                eprintln!("PHP_FCGI_CHILDREN is not valid");
                return None;
            }
        }
        Some(Limits {
            max_requests,
            children,
        })
    }

    /// `fcgi_set_mgmt_var`'s `FCGI_MAX_CONNS` and `FCGI_MAX_REQS`, the
    /// children where there are some and else `1`, and `fcgi_init`'s
    /// `FCGI_MPXS_CONNS`, what an `FCGI_GET_VALUES` is answered with.
    fn values(env: &Environment) -> Vec<(Vec<u8>, Vec<u8>)> {
        let conns = env
            .get(b"PHP_FCGI_CHILDREN")
            .map_or_else(|| b"1".to_vec(), <[u8]>::to_vec);
        vec![
            (b"FCGI_MAX_CONNS".to_vec(), conns.clone()),
            (b"FCGI_MAX_REQS".to_vec(), conns),
            (b"FCGI_MPXS_CONNS".to_vec(), b"0".to_vec()),
        ]
    }
}

/// php-cgi `-b path`: the socket bound, the children forked where
/// `PHP_FCGI_CHILDREN` asks, and requests served; the process's exit code.
pub(crate) fn serve(engine: &Engine, path: &[u8], env: &Environment) -> i32 {
    let mut backlog = BACKLOG;
    if let Some(value) = env.get(b"PHP_FCGI_BACKLOG") {
        backlog = (C_NUMBER.atoi)(value);
    }
    if !(-1..=SOMAXCONN).contains(&backlog) {
        eprintln!("Invalid backlog {backlog}, needs to be between -1 and {SOMAXCONN}");
        return FAILURE;
    }
    let Some(listening) = listen(path, backlog) else {
        eprintln!(
            "Couldn't create FastCGI listen socket on port {}",
            String::from_utf8_lossy(path)
        );
        return FAILURE;
    };
    crate::ffi::accepting_on(listening.fd());
    let Some(limits) = Limits::read(env) else {
        return FAILURE;
    };
    let accepting = Accepting {
        listening,
        allowed: env
            .get(b"FCGI_WEB_SERVER_ADDRS")
            .map(|list| allowed_clients(list, |_| {})),
        values: Limits::values(env),
        max_requests: limits.max_requests,
    };
    if limits.children > 0 {
        return match keep_children(limits.children) {
            Some(()) => accepting.run(engine),
            None => 0,
        };
    }
    accepting.run(engine)
}

/// The parent's loop (`Z/sapi/cgi/cgi_main.c:2009-2085`): a process group
/// of its own, its stop signals killing it whole, and `children` children
/// forked and each one that ends forked again. `Some` in a child, which
/// serves; the parent never comes back but by its stop, which kills it.
fn keep_children(children: i32) -> Option<()> {
    nix::unistd::setsid().ok();
    if crate::ffi::kill_group_on_stop(nix::unistd::getpgrp()).is_err() {
        eprintln!("Can't set signals: {}", std::io::Error::last_os_error());
        php_sapi::exit(1);
    }
    let mut running = 0;
    loop {
        while running < children {
            match php_sapi::fork() {
                Ok(Forked::Child) => {
                    crate::ffi::child_stops();
                    return Some(());
                }
                Ok(Forked::Parent { .. }) => running += 1,
                Err(e) => {
                    eprintln!("php (pre-forking): {e}");
                    php_sapi::exit(1);
                }
            }
        }
        loop {
            match nix::sys::wait::wait() {
                Ok(_) => {
                    running -= 1;
                    break;
                }
                Err(nix::errno::Errno::EINTR) if crate::ffi::told_to_exit() => break,
                Err(nix::errno::Errno::EINTR) => {}
                Err(_) => return None,
            }
        }
        // `exit_signal`: the stop came, and the parent goes to
        // `parent_out` whether the wait ended on it or on a child the
        // stop ended, forking none in its place.
        if crate::ffi::told_to_exit() {
            return None;
        }
    }
}

/// One process's requests.
struct Accepting {
    listening: Listening,
    allowed: Option<Vec<IpAddr>>,
    values: Vec<(Vec<u8>, Vec<u8>)>,
    max_requests: i32,
}

impl Accepting {
    /// `while (fcgi_accept_request(request) >= 0)`: each request served,
    /// the connection kept where the web server asked, until
    /// `max_requests`; the exit code.
    fn run(&self, engine: &Engine) -> i32 {
        let mut requests = 0;
        let mut exit_status = 0;
        let mut kept: Option<Conn> = None;
        loop {
            let mut conn = match kept.take() {
                Some(_) if crate::ffi::shutting_down() => return exit_status,
                Some(conn) => conn,
                None => match self.accept() {
                    Some(conn) => conn,
                    None => return exit_status,
                },
            };
            let begun = match fcgi::read_request(&mut conn, &self.values) {
                fcgi::Read::Request(begun) => begun,
                fcgi::Read::Answered => {
                    kept = Some(conn);
                    continue;
                }
                fcgi::Read::Refused => {
                    conn.close(true);
                    continue;
                }
            };
            let wire = Arc::new(Mutex::new(Wire {
                conn: Some(conn),
                out: Outgoing::new(begun.id),
                input: fcgi::Stdin::default(),
                keep: begun.keep,
            }));
            let served = serve_one(engine, begun.params, &wire);
            let mut w = lock(&wire);
            let Some(code) = served else {
                // `php_request_startup` failed: `fcgi_finish_request(request,
                // 1)` and `main` returns `FAILURE`.
                w.finish(true);
                return FAILURE;
            };
            if exit_status == 0 {
                exit_status = code;
            }
            requests += 1;
            if self.max_requests != 0 && requests == self.max_requests {
                drain(&mut w);
                w.finish(true);
                return if self.max_requests == 1 {
                    exit_status
                } else {
                    0
                };
            }
            w.finish(false);
            kept = w.conn.take();
        }
    }

    /// A connection a web server opened that `FCGI_WEB_SERVER_ADDRS`
    /// lets in; one it does not is logged and closed, as
    /// `fcgi_accept_request` drops it.
    fn accept(&self) -> Option<Conn> {
        loop {
            let (conn, peer) = self.listening.accept()?;
            if allowed(self.allowed.as_deref(), peer) {
                return Some(conn);
            }
            eprintln!(
                "Connection disallowed: IP address '{}' has been dropped.",
                php_sapi::fcgi::client_ip(peer).unwrap_or_default()
            );
            conn.close(true);
        }
    }
}

/// One request (`Z/sapi/cgi/cgi_main.c:2207-2560` past the loop's
/// accept): its variables, each read up to a NUL as a `char *` reads it
/// (#2757), made a request as `init_request_info` makes it, its body
/// read from its `FCGI_STDIN`, and the program served, or php's page
/// where the file does not open. `None` where the request's start failed.
fn serve_one(engine: &Engine, params: Environment, wire: &Arc<Mutex<Wire>>) -> Option<i32> {
    let headers = php_proto::cgi::request_headers(&Environment(
        fcgi::newest_first(&params).cloned().collect(),
    ));
    let mut env = params.cut_at_nul();
    let settings = php_sapi::cgi::settings(engine);
    let doc_root = engine.ini_get(b"doc_root").filter(|root| !root.is_empty());
    let info: RequestInfo = php_proto::cgi::request_info(
        &mut env,
        settings,
        doc_root.as_deref(),
        &CachedDisk(engine),
        C_NUMBER,
    );
    let path = info.path_translated.clone();
    let mut ini = path.as_deref().map_or_else(Vec::new, |path| {
        engine.activated_ini(env.get(b"SERVER_NAME"), path)
    });
    if let (Some(path), Some(doc_root)) = (path.as_deref(), env.get(b"DOCUMENT_ROOT")) {
        let dir = php_sapi::cgi::user_ini_dir(path);
        let user = php_sapi::cgi::user_ini(engine, &dir, doc_root);
        ini.extend(user.into_iter().map(|(k, v)| (k, v, IniOrigin::PerDir)));
    }
    lock(wire).input = fcgi::Stdin::new(info.body_length());
    let body = Body::Stream(Box::new(Stdin(Arc::clone(wire))));
    let rows = php_sapi::cgi::server_variables(&env, &info, settings);
    let process_env: Vec<(Vec<u8>, Vec<u8>)> = std::env::vars_os()
        .map(|(n, v)| (n.into_encoded_bytes(), v.into_encoded_bytes()))
        .collect();
    let newest = Environment(fcgi::newest_first(&env).cloned().collect());
    let web = WebRequest {
        body,
        response_code: info.response_code,
        headers,
        ..php_sapi::cgi::web(&env, &info)
    };
    let description = RequestDescription {
        env: process_env,
        params: newest.0,
        cwd: std::env::current_dir().unwrap_or_default(),
        chdir: Chdir::IntoProgramDir,
        server: rows,
        sapi_name: String::from(NAME),
        web: Some(web),
        script_name: path.clone(),
        ini,
        ..RequestDescription::default()
    };
    let logging = php_sapi::ini::enabled(engine, "fastcgi.logging");
    let sink = Box::new(Response {
        wire: Arc::clone(wire),
        logging,
    });
    let (served, _) = engine.serve(description, sink, |_| {
        refusal(path.as_deref()).map_or(HostServes::Program, HostServes::Page)
    });
    (!served.start_failed).then_some(served.exit_code)
}

/// The wire, held for one read or write.
fn lock(wire: &Mutex<Wire>) -> MutexGuard<'_, Wire> {
    wire.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The request's body as the request pulls it, [`Wire::read_body`].
struct Stdin(Arc<Mutex<Wire>>);

impl std::io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(lock(&self.0).read_body(buf))
    }
}

/// What the web server sent that the request did not read, its empty
/// `FCGI_STDIN` among it, taken off the connection without waiting, so
/// a close that follows the last answer sends the answer and no reset
/// that could reach the web server before it.
fn drain(wire: &mut Wire) {
    use std::io::Read as _;
    let Some(conn) = &mut wire.conn else {
        return;
    };
    let nonblocking = match conn {
        Conn::Tcp(s) => s.set_nonblocking(true),
        Conn::Unix(s) => s.set_nonblocking(true),
    };
    if nonblocking.is_err() {
        return;
    }
    let mut buf = [0u8; 512];
    while matches!(conn.read(&mut buf), Ok(n) if n > 0) {}
    match conn {
        Conn::Tcp(s) => s.set_nonblocking(false),
        Conn::Unix(s) => s.set_nonblocking(false),
    }
    .ok();
}
