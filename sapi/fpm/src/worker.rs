//! A child: `fpm_child_init` for its pool, then `fpm_main.c`'s request
//! loop over `fcgi_accept_request` — each connection accepted from the
//! pool's socket where `listen.allowed_clients` lets it in, each request
//! read with `php_proto::fcgi`, its variables made a request description
//! as `init_request_info` makes them, the ping and status pages answered
//! by the host, and every other request served on the engine the child
//! built with its pool's `php_value`s, through a sink that writes
//! `FCGI_STDOUT` records; the scoreboard kept at each stage, the access
//! log written, and the child gone after `pm.max_requests`.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use php_proto::cgi::{Environment, Proxy, Settings};
use php_proto::fcgi::{self, Input, Kind, Outgoing};
use php_sapi::{
    Engine, HostExtension, HostValue, IniEvent, IniOrigin, OutputSink, RequestDescription,
    ResponseHead, WebRequest,
};

use crate::access::AccessLog;
use crate::conf::PoolConfig;
use crate::master::{EXIT_SOFTWARE, Fpm};
use crate::scoreboard::{Action, Info, Scoreboard, Stage, Update};
use crate::sockets::Domain;
use crate::zlog::Level;
use crate::{zlog, zlog_sys};

/// `sapi_module.name`: what `php_sapi_name()` answers.
pub(crate) const NAME: &str = "fpm-fcgi";

/// The pool's listening socket, as the child accepts from it.
enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

/// One connection.
enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            Conn::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Conn {
    fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        match self {
            Conn::Tcp(s) => s.as_fd(),
            Conn::Unix(s) => s.as_fd(),
        }
    }

    /// `fcgi_close(req, force, …)`: the connection shut and drained where
    /// not forced.
    fn close(self, force: bool) {
        if !force {
            let _ = match &self {
                Conn::Tcp(s) => s.shutdown(std::net::Shutdown::Write),
                Conn::Unix(s) => s.shutdown(std::net::Shutdown::Write),
            };
            let mut this = self;
            let mut buf = [0u8; 8];
            while matches!(this.read(&mut buf), Ok(n) if n > 0) {}
        }
    }
}

/// What a request writes through: the connection, the request's output
/// buffer, and what the access log reads of the response afterwards.
struct Wire {
    conn: Option<Conn>,
    out: Outgoing,
    input: Input,
    keep: bool,
    /// `CGIG(fcgi_logging_request_started)`.
    stderr_started: bool,
    head: Option<ResponseHead>,
}

impl Wire {
    /// `fcgi_write`, the connection kept no longer where it fails.
    fn write(&mut self, kind: Kind, bytes: &[u8]) {
        if let Some(conn) = &mut self.conn
            && self.out.write(conn, kind, bytes).is_err()
        {
            self.keep = false;
        }
    }

    /// `fcgi_finish_request(req, force)`: `FCGI_END_REQUEST` sent where the
    /// connection is open, and the connection closed unless kept.
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

/// The request's sink, which the request holds while it runs.
#[derive(Clone)]
struct Response {
    wire: Arc<Mutex<Wire>>,
    board: Option<Scoreboard>,
}

impl Response {
    fn wire(&self) -> std::sync::MutexGuard<'_, Wire> {
        self.wire
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl OutputSink for Response {
    fn write(&mut self, bytes: &[u8]) {
        self.wire().write(Kind::Stdout, bytes);
    }

    /// `sapi_cgibin_flush`.
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

    /// `sapi_cgi_send_headers`.
    fn send_headers(&mut self, head: &ResponseHead) {
        let block = php_sapi::cgi::head(head);
        let mut wire = self.wire();
        wire.head = Some(head.clone());
        wire.write(Kind::Stdout, &block);
    }

    /// `sapi_cgi_log_message`: `PHP message: ` and the entry, through the
    /// child's log.
    fn log(&mut self, entry: &[u8]) {
        crate::zlog::msg(Level::Notice, b"PHP message: ", entry);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        let _ = std::io::stderr().write_all(bytes);
    }

    fn held(&mut self) -> Option<Box<dyn OutputSink + Send>> {
        Some(Box::new(self.clone()))
    }

    /// `sapi_cgibin_flush` sends the head before it flushes.
    fn flush_sends_head(&self) -> bool {
        true
    }

    /// `fastcgi_finish_request`'s `fcgi_end` and `fcgi_close(req, 0, 0)`.
    fn finish(&mut self) -> bool {
        let mut wire = self.wire();
        wire.finish(false);
        wire.conn.is_none()
    }

    /// `fpm_status_export_to_zval`.
    fn status(&mut self) -> Option<HostValue> {
        self.board
            .map(|board| crate::status::export(&board.copy(true)))
    }
}

/// The request being served, which the child's log also writes to.
static CURRENT: Mutex<Option<Arc<Mutex<Wire>>>> = Mutex::new(None);
/// `CGIG(fcgi_logging)`.
static LOGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `sapi_cgi_log_fastcgi`: a message of the child's log to the request's
/// `FCGI_STDERR`, `; ` between two, while `fastcgi.logging` is on.
fn log_fastcgi(_level: Level, message: &[u8]) {
    if !LOGGING.load(std::sync::atomic::Ordering::Relaxed) || message.is_empty() {
        return;
    }
    let current = CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(wire) = current else {
        return;
    };
    let mut wire = wire
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if wire.stderr_started {
        wire.write(Kind::Stderr, b"; ");
    } else {
        wire.stderr_started = true;
    }
    wire.write(Kind::Stderr, message);
}

/// `fcgi_set_allowed_clients`: the addresses `listen.allowed_clients`
/// names, each wrong one logged; `None` lets every address in.
fn allowed_clients(list: Option<&[u8]>) -> Option<Vec<IpAddr>> {
    let list = list?;
    let mut allowed = Vec::new();
    for word in list.split(|b| *b == b',') {
        match std::str::from_utf8(word)
            .ok()
            .and_then(|w| w.parse::<IpAddr>().ok())
        {
            Some(ip) => allowed.push(ip),
            None => zlog!(
                Error,
                "Wrong IP address '{}' in listen.allowed_clients",
                String::from_utf8_lossy(word)
            ),
        }
    }
    if allowed.is_empty() {
        zlog!(Error, "There are no allowed addresses");
    }
    Some(allowed)
}

/// `fcgi_is_allowed`, and `fcgi_get_last_client_ip`'s text of the peer.
fn allowed(allowed: Option<&[IpAddr]>, peer: Option<IpAddr>) -> bool {
    let (Some(list), Some(peer)) = (allowed, peer) else {
        return true;
    };
    list.iter().any(|ip| match (ip, peer) {
        (IpAddr::V4(a), IpAddr::V4(p)) => *a == p,
        (IpAddr::V6(a), IpAddr::V6(p)) => a.octets()[..12] == p.octets()[..12],
        (IpAddr::V4(a), IpAddr::V6(p)) => p.to_ipv4_mapped() == Some(*a),
        (IpAddr::V6(_), IpAddr::V4(_)) => false,
    })
}

/// `fcgi_get_last_client_ip`: the peer, a mapped IPv4 as IPv4.
fn client_ip(peer: Option<IpAddr>) -> Option<String> {
    peer.map(|ip| match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
        IpAddr::V4(v4) => v4.to_string(),
    })
}

/// A `.user.ini` cache entry, `user_config_cache_entry`.
struct UserConfig {
    expires: i64,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
}

/// The child.
pub(crate) struct Worker {
    engine: Engine,
    scoreboard: Scoreboard,
    /// The scoreboard the status page shows: the pool's, or the one a
    /// status pool serves the page of.
    status_board: Scoreboard,
    slot: usize,
    listener: Listener,
    allowed: Option<Vec<IpAddr>>,
    access: Option<AccessLog>,
    status_uri: Option<Vec<u8>>,
    ping: Option<(Vec<u8>, Vec<u8>)>,
    limit_extensions: Vec<Vec<u8>>,
    max_requests: i32,
    /// What `PHP_VALUE` and `PHP_ADMIN_VALUE` set, which stays for the
    /// child's later requests as php's master value does.
    sticky: Vec<(Vec<u8>, Vec<u8>, IniOrigin)>,
    user_config: Vec<(Vec<u8>, UserConfig)>,
    get_values: Vec<(Vec<u8>, Vec<u8>)>,
}

/// `fpm_child_init`: the child made ready for pool `pool` of `fpm`, and
/// its engine built; the child exits with 70 where a step fails.
pub(crate) fn init(
    fpm: &mut Fpm,
    pool: usize,
    slot: usize,
    ini: &[(String, String)],
    started: &str,
) -> Worker {
    let failed = |name: &str| -> ! {
        zlog!(Error, "[pool {name}] child failed to initialize");
        std::process::exit(i32::from(EXIT_SOFTWARE));
    };
    let name = fpm.pools[pool].name();
    let config = fpm.pools[pool].config.clone();
    let listening = fpm.pools[pool]
        .listening
        .and_then(|fd| fpm.sockets.by_fd(fd))
        .and_then(|fd| fd.try_clone().ok());
    let Some(listening) = listening else {
        failed(&name)
    };
    crate::stdio::init_child();
    let access = AccessLog::new(
        fpm.pools[pool].access_log.take(),
        config
            .access_format
            .clone()
            .filter(|_| config.access_log.as_deref().is_some_and(|l| !l.is_empty())),
        &config.access_suppress_paths,
    );
    let ping = match (&config.ping_path, &config.ping_response) {
        (Some(path), Some(response)) => Some((path.clone(), response.clone())),
        (Some(path), None) => {
            zlog!(
                Error,
                "[pool {name}] ping is set ({}) but ping.response is not set.",
                String::from_utf8_lossy(path)
            );
            failed(&name);
        }
        _ => None,
    };
    if !crate::unix::init_child(&config, &fpm.pools[pool].credentials) {
        failed(&name);
    }
    fpm.close_signal_pipe();
    if crate::ffi::child_soft_quit(listening.as_raw_fd()).is_err() {
        zlog_sys!(
            Error,
            nix::errno::Errno::last(),
            "failed to init child signals: sigaction()"
        );
        failed(&name);
    }
    crate::master::unblock_all();
    crate::env::init_child(&fpm.pools[pool]);
    let engine = build(ini, &config, started);
    let domain = fpm.pools[pool].domain;
    let allowed = if domain == Domain::Inet {
        allowed_clients(config.listen_allowed_clients.as_deref())
    } else {
        None
    };
    let listener = match domain {
        Domain::Inet => Listener::Tcp(TcpListener::from(listening)),
        Domain::Unix => Listener::Unix(UnixListener::from(listening)),
    };
    LOGGING.store(
        engine.ini_get(b"fastcgi.logging").is_none_or(|v| truth(&v)),
        std::sync::atomic::Ordering::Relaxed,
    );
    crate::zlog::set_external(Some(log_fastcgi));
    let Some(scoreboard) = fpm.pools[pool].scoreboard else {
        failed(&name)
    };
    let status_board = fpm.pools[pool]
        .shared
        .and_then(|shared| fpm.pools[shared].scoreboard)
        .unwrap_or(scoreboard);
    Worker {
        engine,
        scoreboard,
        slot,
        listener,
        allowed,
        access,
        status_uri: config.pm_status_path.clone(),
        ping,
        limit_extensions: fpm.pools[pool].limit_extensions.clone(),
        max_requests: config.pm_max_requests,
        sticky: Vec::new(),
        user_config: Vec::new(),
        get_values: vec![(b"FCGI_MPXS_CONNS".to_vec(), b"0".to_vec())],
        status_board,
    }
}

/// `OnUpdateBool`'s reading of an entry.
fn truth(value: &[u8]) -> bool {
    let v = String::from_utf8_lossy(value).trim().to_ascii_lowercase();
    v == "on" || v == "yes" || v == "true" || v.parse::<i64>().is_ok_and(|n| n != 0)
}

/// The child's engine: the bundle, `-d`'s entries, and the pool's
/// `php_value`s and `php_admin_value`s as `fpm_php_apply_defines` sets
/// them, each one php does not know logged.
fn build(ini: &[(String, String)], config: &PoolConfig, started: &str) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let mut builder = php_sapi::prepare(
        Engine::builder()
            .extensions(&extensions)
            .tier::<php_bundle::Tier>(),
        ini,
        Some(HostExtension::FpmFcgi),
    );
    for (name, value) in &config.php_values {
        builder = builder.host_ini(name, value, IniOrigin::Value);
    }
    for (name, value) in &config.php_admin_values {
        builder = builder.host_ini(name, value, IniOrigin::Admin);
    }
    let engine = builder.build();
    // What the pool's entries wrote as they were laid over, past what the
    // master wrote as php started.
    let own = engine
        .startup_errors()
        .strip_prefix(started)
        .unwrap_or_default();
    let _ = std::io::stderr().write_all(own.as_bytes());
    for (kind, values) in [
        ("php_value", &config.php_values),
        ("php_admin_value", &config.php_admin_values),
    ] {
        for (name, _) in values {
            if name.as_slice() != b"extension" && engine.ini_get(name).is_none() {
                zlog!(
                    Error,
                    "Unable to set {kind} '{}'",
                    String::from_utf8_lossy(name)
                );
            }
        }
    }
    engine
}

impl Worker {
    /// The child's slot of its pool's scoreboard.
    fn proc(&self) -> Option<&'static crate::scoreboard::Proc> {
        self.scoreboard.proc(self.slot)
    }

    /// `fpm_request_accepting`.
    fn accepting(&self) {
        if let Some(proc) = self.proc() {
            proc.set_stage(Stage::Accepting, crate::master::clock_us());
        }
        self.scoreboard.update(
            Update {
                idle: 1,
                active: -1,
                ..Update::ZERO
            },
            Action::Inc,
        );
    }

    /// `fpm_request_reading_headers`.
    fn reading_headers(&self) {
        if let Some(proc) = self.proc() {
            proc.reading_headers(crate::master::clock_us(), cpu_us());
        }
        self.scoreboard.update(
            Update {
                idle: -1,
                active: 1,
                requests: 1,
                ..Update::ZERO
            },
            Action::Inc,
        );
    }

    /// `fcgi_accept_request`'s wait for a connection: `None` once the
    /// child is told to finish or the socket fails.
    fn accept(&mut self) -> Option<(Conn, Option<IpAddr>)> {
        loop {
            if crate::ffi::shutting_down() {
                return None;
            }
            self.accepting();
            let accepted = match &self.listener {
                Listener::Tcp(l) => l.accept().map(|(s, peer)| (Conn::Tcp(s), Some(peer.ip()))),
                Listener::Unix(l) => l.accept().map(|(s, _)| (Conn::Unix(s), None)),
            };
            let (conn, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    let transient =
                        matches!(e.raw_os_error(), Some(libc::EINTR | libc::ECONNABORTED));
                    if crate::ffi::shutting_down() || !transient {
                        return None;
                    }
                    continue;
                }
            };
            if !allowed(self.allowed.as_deref(), peer) {
                zlog!(
                    Error,
                    "Connection disallowed: IP address '{}' has been dropped.",
                    client_ip(peer).unwrap_or_default()
                );
                conn.close(true);
                continue;
            }
            let mut fds = [nix::poll::PollFd::new(
                conn.fd(),
                nix::poll::PollFlags::POLLIN,
            )];
            let ready = loop {
                match nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(5000u16)) {
                    Err(nix::errno::Errno::EINTR) => {}
                    other => break other,
                }
            };
            let readable = fds[0]
                .revents()
                .is_some_and(|r| r.contains(nix::poll::PollFlags::POLLIN));
            if matches!(ready, Ok(n) if n > 0) && readable {
                return Some((conn, peer));
            }
            conn.close(true);
        }
    }

    /// The request loop: requests served until the child is told to
    /// finish or has served `pm.max_requests`; the process's exit code.
    pub(crate) fn run(mut self) -> u8 {
        let mut requests = 0;
        let mut kept: Option<(Conn, Option<IpAddr>)> = None;
        loop {
            let (mut conn, peer) = match kept.take() {
                Some(kept) => {
                    if crate::ffi::shutting_down() {
                        return 0;
                    }
                    kept
                }
                None => match self.accept() {
                    Some(accepted) => accepted,
                    None => return 0,
                },
            };
            self.reading_headers();
            let begun = match fcgi::read_request(&mut conn, &self.get_values) {
                fcgi::Read::Request(begun) => begun,
                fcgi::Read::Answered => {
                    kept = Some((conn, peer));
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
                input: Input::default(),
                keep: begun.keep,
                stderr_started: false,
                head: None,
            }));
            *CURRENT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&wire));
            self.serve(begun.params, &wire, peer);
            *CURRENT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            requests += 1;
            let last = self.max_requests > 0 && requests == self.max_requests;
            let mut w = wire
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last {
                w.keep = false;
            }
            w.finish(false);
            let conn = w.conn.take();
            drop(w);
            crate::stdio::flush_child();
            if last {
                return 0;
            }
            if let Some(conn) = conn {
                kept = Some((conn, peer));
            }
        }
    }
}

/// The CPU the child has used, its own and its children's, in
/// microseconds, what `times()` gives php.
fn cpu_us() -> i64 {
    use nix::sys::resource::{UsageWho, getrusage};
    let micro = |t: nix::sys::time::TimeVal| t.tv_sec() * 1_000_000 + i64::from(t.tv_usec());
    [UsageWho::RUSAGE_SELF, UsageWho::RUSAGE_CHILDREN]
        .into_iter()
        .filter_map(|who| getrusage(who).ok())
        .map(|u| micro(u.user_time()) + micro(u.system_time()))
        .sum()
}

/// `$_GET`'s keys as `php_default_treat_data` registers them: each pair
/// split on `arg_separator.input`, its name decoded, leading spaces
/// dropped, cut at its first `[`, a `.` or ` ` read as `_`.
fn get_keys(query: &[u8], separators: &[u8]) -> Vec<Vec<u8>> {
    query
        .split(|b| separators.contains(b))
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let name = pair.split(|b| *b == b'=').next().unwrap_or_default();
            let name = url_decode(name);
            let name: Vec<u8> = name.into_iter().skip_while(|b| *b == b' ').collect();
            let cut = name.iter().position(|b| *b == b'[').unwrap_or(name.len());
            let name: Vec<u8> = name[..cut]
                .iter()
                .map(|&b| if b == b'.' || b == b' ' { b'_' } else { b })
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// `sapi_header_op`'s `Content-Type:` line: a `text/` type without a
/// charset gets `default_charset`'s, the line then `Content-type: `.
fn content_type(line: Vec<u8>, charset: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"Content-Type:";
    if line.len() < NAME.len() || !line[..NAME.len()].eq_ignore_ascii_case(NAME) {
        return line;
    }
    let mime = line[NAME.len()..].trim_ascii_start();
    if charset.is_empty()
        || !mime.starts_with(b"text/")
        || crate::conf::find(mime, b"charset=").is_some()
    {
        return line;
    }
    let mut out = b"Content-type: ".to_vec();
    out.extend_from_slice(mime);
    out.extend_from_slice(b";charset=");
    out.extend_from_slice(charset);
    out
}

/// `php_url_decode`.
fn url_decode(text: &[u8]) -> Vec<u8> {
    let hex = |c: u8| char::from(c).to_digit(16);
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < text.len() => match (hex(text[i + 1]), hex(text[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// The file system as `init_request_info` asks it.
struct Disk;

impl php_proto::cgi::Files for Disk {
    fn resolves(&self, path: &[u8]) -> bool {
        std::fs::canonicalize(crate::conf::path(path)).is_ok()
    }

    fn is_file(&self, path: &[u8]) -> bool {
        std::fs::metadata(crate::conf::path(path)).is_ok_and(|m| m.is_file())
    }
}

/// What a request is, past the variables: the program the engine
/// serves, a page the host answers, or php's refusal text.
enum Program {
    /// No `REQUEST_METHOD`: not a request of the web; the engine starts
    /// and ends it with nothing to serve.
    Nothing,
    /// The ping or the status page.
    Page(crate::status::Page),
    /// `File not found.`, `Access denied.` or `No input file
    /// specified.`, under its code.
    Refused(i64, &'static [u8]),
    /// The file.
    File(Vec<u8>),
}

impl Worker {
    /// One request, from its variables to its access log line.
    #[allow(clippy::too_many_lines)] // `fpm_main`'s loop body, kept as one
    fn serve(&mut self, params: Environment, wire: &Arc<Mutex<Wire>>, peer: Option<IpAddr>) {
        let mut env = params;
        let settings = Settings {
            fix_pathinfo: self.flag(b"cgi.fix_pathinfo", true),
            discard_path: self.flag(b"cgi.discard_path", false),
            proxy: Proxy::Read {
                encoded: self.flag(b"fastcgi.script_path_encoded", true),
            },
        };
        let doc_root = self.engine.ini_get(b"doc_root").filter(|r| !r.is_empty());
        let info = php_proto::cgi::request_info(&mut env, settings, doc_root.as_deref(), &Disk);
        self.request_values(&env);
        let now = crate::master::clock_us();
        if let Some(proc) = self.proc() {
            proc.info(
                &Info {
                    request_uri: info.request_uri.clone(),
                    request_method: info.method.clone(),
                    query_string: info.query.clone(),
                    auth_user: info.auth.as_ref().map(|(user, _)| user.clone()),
                    script_filename: info.path_translated.clone(),
                    content_length: u64::try_from(info.content_length).unwrap_or(0),
                },
                now,
            );
        }
        let head_only = info.method.as_deref() == Some(b"HEAD");
        let program = if info.method.is_none() {
            Program::Nothing
        } else if let Some(page) = self.page(&info, head_only, env.get(b"QUERY_STRING")) {
            Program::Page(page)
        } else if let Some(path) = &info.path_translated {
            if self.extension_allowed(path) {
                match std::fs::File::open(crate::conf::path(path)) {
                    Ok(_) => Program::File(path.clone()),
                    Err(e) => {
                        zlog!(
                            Error,
                            "Unable to open primary script: {} ({})",
                            String::from_utf8_lossy(path),
                            crate::conf::errno(&e).desc()
                        );
                        if e.kind() == std::io::ErrorKind::PermissionDenied {
                            Program::Refused(403, b"Access denied.\n")
                        } else {
                            Program::Refused(404, b"No input file specified.\n")
                        }
                    }
                }
            } else {
                Program::Refused(403, b"Access denied.\n")
            }
        } else {
            zlog!(Debug, "Primary script unknown");
            Program::Refused(404, b"File not found.\n")
        };
        let mut response = Response {
            wire: Arc::clone(wire),
            board: Some(self.board()),
        };
        let (status, memory) = match program {
            Program::Page(page) => {
                let mut lines = Vec::new();
                if self.flag(b"expose_php", true) {
                    lines.push(format!("X-Powered-By: PHP/{}", php_sapi::PHP_VERSION).into_bytes());
                }
                let charset = self.engine.ini_get(b"default_charset").unwrap_or_default();
                lines.extend(
                    page.headers
                        .into_iter()
                        .map(|line| content_type(line, &charset)),
                );
                let head = ResponseHead {
                    code: page.code,
                    status_line: None,
                    lines,
                };
                response.send_headers(&head);
                response.write(&page.body);
                (page.code, 0)
            }
            program => {
                let executing = matches!(program, Program::File(_));
                let description = self.describe(&env, &info, program, wire);
                if executing && let Some(proc) = self.proc() {
                    proc.set_stage(Stage::Executing, crate::master::clock_us());
                }
                let served = self.engine.serve(description, &mut response);
                let status = wire
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .head
                    .as_ref()
                    .map_or(200, |h| h.code);
                (status, served.memory_peak)
            }
        };
        let memory = u64::try_from(memory).unwrap_or(0);
        if let Some(proc) = self.proc() {
            proc.end(crate::master::clock_us(), cpu_us(), memory);
        }
        self.scoreboard.update(
            Update {
                memory_peak: memory,
                ..Update::NONE
            },
            Action::Set,
        );
        let headers = wire
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .head
            .as_ref()
            .map(|h| h.lines.clone())
            .unwrap_or_default();
        let board = self.scoreboard.copy(false);
        let proc = self
            .proc()
            .map(crate::scoreboard::Proc::view)
            .unwrap_or_default();
        if let Some(access) = &mut self.access {
            let lookup = |name: &[u8]| env.get(name).map(<[u8]>::to_vec);
            crate::access::write(
                access,
                &crate::access::Request {
                    pool: &board,
                    proc: &proc,
                    status,
                    headers: &headers,
                    content_length: info.content_length,
                    param: &lookup,
                    client_ip: client_ip(peer),
                },
            );
        }
    }

    /// An entry read as `OnUpdateBool` reads it.
    fn flag(&self, name: &[u8], default: bool) -> bool {
        self.engine.ini_get(name).map_or(default, |v| truth(&v))
    }

    /// The scoreboard the status page shows.
    fn board(&self) -> Scoreboard {
        self.status_board
    }

    /// `fpm_php_limit_extensions`.
    fn extension_allowed(&self, path: &[u8]) -> bool {
        if self.limit_extensions.is_empty() {
            return true;
        }
        let allowed = self
            .limit_extensions
            .iter()
            .any(|ext| path.len() > ext.len() && path.ends_with(ext));
        if !allowed {
            zlog!(
                Notice,
                "Access to the script '{}' has been denied (see security.limit_extensions)",
                String::from_utf8_lossy(path)
            );
        }
        allowed
    }

    /// `fpm_status_handle_request`: the ping or the status page, where
    /// the request's path names one.
    fn page(
        &self,
        info: &php_proto::cgi::RequestInfo,
        head_only: bool,
        query: Option<&[u8]>,
    ) -> Option<crate::status::Page> {
        let uri = info.request_uri.as_deref()?;
        let executing = || {
            if let Some(proc) = self.proc() {
                proc.set_stage(Stage::Executing, crate::master::clock_us());
            }
        };
        if let Some((path, response)) = &self.ping
            && path.as_slice() == uri
        {
            executing();
            return Some(crate::status::ping(response, head_only));
        }
        self.status_uri.as_deref().filter(|s| *s == uri)?;
        executing();
        let separators = self
            .engine
            .ini_get(b"arg_separator.input")
            .unwrap_or_else(|| b"&".to_vec());
        let keys = get_keys(query.unwrap_or_default(), &separators);
        let full = keys.iter().any(|k| k.as_slice() == b"full");
        let board = self.board().copy(full);
        Some(crate::status::status(&board, &keys, head_only))
    }

    /// `PHP_VALUE` and `PHP_ADMIN_VALUE`, read as `fastcgi_ini_parser`
    /// reads them and kept for the child's later requests.
    fn request_values(&mut self, env: &Environment) {
        for (var, mode) in [
            (&b"PHP_VALUE"[..], IniOrigin::Value),
            (b"PHP_ADMIN_VALUE", IniOrigin::Admin),
        ] {
            let Some(text) = env.get(var) else {
                continue;
            };
            let mut text = text.to_vec();
            text.push(b'\n');
            let (events, _) = self.engine.parse_ini(&text);
            for event in events {
                let (key, value) = if let IniEvent::Entry(key, value) = event {
                    (text_of(&key), value.as_ref().map(text_of))
                } else {
                    zlog!(
                        Error,
                        "Passing INI directive through FastCGI: only classic entries are allowed"
                    );
                    continue;
                };
                if key.is_empty() {
                    zlog!(Error, "Passing INI directive through FastCGI: empty key");
                    continue;
                }
                let Some(value) = value else {
                    zlog!(
                        Error,
                        "Passing INI directive through FastCGI: empty value for key '{}'",
                        String::from_utf8_lossy(&key)
                    );
                    continue;
                };
                if key.as_slice() != b"extension" && self.engine.ini_get(&key).is_none() {
                    zlog!(
                        Error,
                        "Passing INI directive through FastCGI: unable to set '{}'",
                        String::from_utf8_lossy(&key)
                    );
                    continue;
                }
                self.sticky.retain(|(k, _, _)| *k != key);
                self.sticky.push((key, value, mode));
            }
        }
    }

    /// `sapi_cgi_pre_request_init`'s `.user.ini` files: each from the
    /// document root down to the program's directory, cached for
    /// `user_ini.cache_ttl` seconds per directory.
    fn user_ini(&mut self, path_translated: &[u8], env: &Environment) -> Vec<(Vec<u8>, Vec<u8>)> {
        let filename = self
            .engine
            .ini_get(b"user_ini.filename")
            .unwrap_or_default();
        if filename.is_empty() {
            return Vec::new();
        }
        let Some(doc_root) = env.get(b"DOCUMENT_ROOT").map(<[u8]>::to_vec) else {
            return Vec::new();
        };
        let doc_root = doc_root.strip_suffix(b"/").unwrap_or(&doc_root).to_vec();
        let mut dir = match path_translated.iter().rposition(|b| *b == b'/') {
            Some(at) => path_translated[..at].to_vec(),
            None => b".".to_vec(),
        };
        dir.push(b'/');
        let now = crate::scoreboard::epoch();
        let ttl = crate::conf::atoi(
            &self
                .engine
                .ini_get(b"user_ini.cache_ttl")
                .unwrap_or_default(),
        );
        if let Some((_, cached)) = self.user_config.iter().find(|(d, _)| *d == dir)
            && now <= cached.expires
        {
            return cached.entries.clone();
        }
        let path = if dir.first() == Some(&b'/') {
            dir.clone()
        } else {
            match std::fs::canonicalize(crate::conf::path(&dir)) {
                Ok(real) => real.into_os_string().into_encoded_bytes(),
                Err(_) => return Vec::new(),
            }
        };
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let (shorter, longer) = if path.len() > doc_root.len() {
            (&doc_root, &path)
        } else {
            (&path, &doc_root)
        };
        let mut read = |at: &[u8]| {
            let mut file = at.to_vec();
            file.push(b'/');
            file.extend_from_slice(&filename);
            let Ok(text) = std::fs::read(crate::conf::path(&file)) else {
                return;
            };
            let (events, _) = self.engine.parse_ini(&text);
            for event in events {
                if let IniEvent::Entry(key, Some(value)) = event {
                    let key = text_of(&key);
                    entries.retain(|(k, _)| *k != key);
                    entries.push((key, text_of(&value)));
                }
            }
        };
        if longer.starts_with(shorter) {
            let mut at = doc_root.len();
            while let Some(slash) = path
                .get(at..)
                .and_then(|rest| rest.iter().position(|b| *b == b'/'))
            {
                read(&path[..at + slash]);
                at += slash + 1;
            }
        } else {
            read(path.strip_suffix(b"/").unwrap_or(&path));
        }
        self.user_config.retain(|(d, _)| *d != dir);
        self.user_config.push((
            dir,
            UserConfig {
                expires: now + i64::from(ttl),
                entries: entries.clone(),
            },
        ));
        entries
    }

    /// The request description of a request of the web.
    fn describe(
        &mut self,
        env: &Environment,
        info: &php_proto::cgi::RequestInfo,
        program: Program,
        wire: &Arc<Mutex<Wire>>,
    ) -> RequestDescription {
        let mut ini = self.sticky.clone();
        if let Some(path) = &info.path_translated {
            ini.extend(
                self.user_ini(path, env)
                    .into_iter()
                    .map(|(k, v)| (k, v, IniOrigin::PerDir)),
            );
        }
        let mut body = Vec::new();
        if info.content_length > 0 {
            body = read_body(env, info.content_length, wire);
        }
        let settings_fix = self.flag(b"cgi.fix_pathinfo", true);
        let settings = Settings {
            fix_pathinfo: settings_fix,
            ..Settings::default()
        };
        let rows = php_sapi::cgi::server_variables(env, info, settings);
        let process_env: Vec<(Vec<u8>, Vec<u8>)> = std::env::vars_os()
            .map(|(n, v)| (n.into_encoded_bytes(), v.into_encoded_bytes()))
            .collect();
        let newest = Environment(fcgi::newest_first(env).cloned().collect());
        let (code, source, script_name, cwd) = match program {
            Program::File(path) => {
                let file = crate::conf::path(&path);
                let cwd = file
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_default();
                (info.response_code, None, Some(path), cwd)
            }
            Program::Refused(code, text) => (
                code,
                Some(text.to_vec()),
                info.path_translated.clone(),
                std::env::current_dir().unwrap_or_default(),
            ),
            Program::Nothing | Program::Page(_) => (
                info.response_code,
                Some(Vec::new()),
                None,
                std::env::current_dir().unwrap_or_default(),
            ),
        };
        let web = WebRequest {
            body,
            response_code: code,
            headers: php_proto::cgi::request_headers(&Environment(
                fcgi::newest_first(env).cloned().collect(),
            )),
            ..php_sapi::cgi::web(env, info)
        };
        RequestDescription {
            env: process_env,
            params: newest.0,
            cwd,
            server: rows,
            sapi_name: String::from(NAME),
            web: Some(web),
            script_name,
            source,
            ini,
            ..RequestDescription::default()
        }
    }
}

/// An INI value's text, which the normal scanner always makes.
fn text_of(value: &php_sapi::IniValue) -> Vec<u8> {
    match value {
        php_sapi::IniValue::Str(s) => s.clone(),
        php_sapi::IniValue::Bool(true) => b"1".to_vec(),
        php_sapi::IniValue::Long(n) => n.to_string().into_bytes(),
        php_sapi::IniValue::Double(d) => d.to_string().into_bytes(),
        php_sapi::IniValue::Bool(false) | php_sapi::IniValue::Null => Vec::new(),
    }
}

/// `sapi_cgi_read_post`: the body, up to its length, from
/// `REQUEST_BODY_FILE` where the server names one and otherwise from
/// the request's `FCGI_STDIN` records.
fn read_body(env: &Environment, length: i64, wire: &Arc<Mutex<Wire>>) -> Vec<u8> {
    let length = usize::try_from(length).unwrap_or(0);
    let mut body = vec![0u8; length];
    let mut read = 0;
    if let Some(file) = env.get(b"REQUEST_BODY_FILE").filter(|f| !f.is_empty()) {
        match std::fs::File::open(crate::conf::path(file)) {
            Ok(mut file) => {
                while read < length {
                    match file.read(&mut body[read..]) {
                        Ok(n) if n > 0 => read += n,
                        _ => break,
                    }
                }
            }
            Err(e) => {
                zlog!(
                    Warning,
                    "REQUEST_BODY_FILE: open('{}') failed: {} ({})",
                    String::from_utf8_lossy(file),
                    crate::conf::errno(&e).desc(),
                    crate::conf::errno(&e) as i32
                );
            }
        }
        body.truncate(read);
        return body;
    }
    let mut guard = wire
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Wire {
        conn, input, keep, ..
    } = &mut *guard;
    if let Some(conn) = conn {
        while read < length {
            match input.read(conn, &mut body[read..]) {
                Ok(n) if n > 0 => read += n,
                Ok(_) => break,
                Err(_) => {
                    *keep = false;
                    break;
                }
            }
        }
    }
    body.truncate(read);
    body
}
