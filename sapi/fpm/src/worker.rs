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
use std::net::{IpAddr, TcpStream};
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use php_proto::cgi::{Environment, Proxy, Settings};
use php_proto::fcgi::{self, Input, Kind, Outgoing};
use php_sapi::cgi::{C_NUMBER, Disk, ini_text};
use php_sapi::fcgi::{Conn, allowed, client_ip};
use php_sapi::{
    Aborted, Body, Chdir, Engine, HostPage, HostServes, HostValue, IniEvent, IniOrigin, NotAlone,
    OutputSink, ProgramFile, RequestDescription, ResponseHead, WebRequest,
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
    /// `fcgi_write`: whether the bytes went, which a connection the
    /// web server closed refuses; none open takes them silently.
    fn write(&mut self, kind: Kind, bytes: &[u8]) -> bool {
        if let Some(conn) = &mut self.conn
            && self.out.write(conn, kind, bytes).is_err()
        {
            self.keep = false;
            return false;
        }
        true
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

/// The request's sink, which the request owns while it runs.
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
    /// `sapi_cgibin_ub_write` (`Z/sapi/fpm/fpm/fpm_main.c:265`): a write
    /// the web server's connection refuses is
    /// `php_handle_aborted_connection`, with no status set.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        if self.wire().write(Kind::Stdout, bytes) {
            Ok(())
        } else {
            Err(Aborted { exit_status: None })
        }
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

    /// `sapi_cgi_send_headers`, the head [`php_sapi::cgi::head`] writes.
    ///
    /// `cgi.nph` read at the send, so an `ini_set` before the first byte
    /// opens the head with `Status: 200 OK`:
    ///
    /// ```php
    /// ini_set('cgi.nph', '1');
    /// echo "x";
    /// ```
    /// ```request fpm
    /// --GET--
    /// q=1
    /// ```
    /// ```expect
    /// Status: 200 OK
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// x
    /// ```
    ///
    /// and `cgi.rfc2616_headers` writes the status line `header()` was
    /// given whole:
    ///
    /// ```php
    /// ini_set('cgi.rfc2616_headers', '1');
    /// header('HTTP/1.1 404 Not Found');
    /// echo "x";
    /// ```
    /// ```request fpm
    /// --GET--
    /// q=1
    /// ```
    /// ```expect
    /// HTTP/1.1 404 Not Found
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// x
    /// ```
    fn send_headers(&mut self, head: &ResponseHead) {
        let block = php_sapi::cgi::head(head);
        let mut wire = self.wire();
        wire.head = Some(head.clone());
        let _ = wire.write(Kind::Stdout, &block);
    }

    /// `sapi_cgi_log_message`: `PHP message: ` and the entry, through the
    /// child's log.
    fn log(&mut self, entry: &[u8]) {
        crate::zlog::msg(
            Level::Notice,
            crate::zlog::Site::at("sapi_cgi_log_message", 631),
            b"PHP message: ",
            entry,
        );
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().write_all(bytes).ok();
    }

    /// fd 1, the pipe the master logs, or `/dev/null` where the pool does
    /// not catch its workers' output.
    fn write_stdout(&mut self, bytes: &[u8]) -> bool {
        std::io::stdout().lock().write_all(bytes).ok();
        std::io::stdout().lock().flush().ok();
        true
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
            .and_then(|board| board.copy(true))
            .map(|board| crate::status::export(&board))
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
        let _ = wire.write(Kind::Stderr, b"; ");
    } else {
        wire.stderr_started = true;
    }
    let _ = wire.write(Kind::Stderr, message);
}

/// `fcgi_set_allowed_clients` over `listen.allowed_clients`, each wrong
/// address logged as the child logs it; `None` lets every address in.
fn allowed_clients(list: Option<&[u8]>) -> Option<Vec<IpAddr>> {
    let allowed = php_sapi::fcgi::allowed_clients(list?, |word| {
        zlog!(
            Error @ (crate::zlog::Site::FCGI),
            "Wrong IP address '{}' in listen.allowed_clients",
            String::from_utf8_lossy(word)
        );
    });
    if allowed.is_empty() {
        zlog!(Error @ (crate::zlog::Site::FCGI), "There are no allowed addresses");
    }
    Some(allowed)
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
    /// The pool's listening socket, which the `SIGQUIT` handler closes.
    listening: crate::ffi::Listening,
    /// Whether it is a TCP socket or a Unix one.
    domain: Domain,
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

/// `fpm_child_init`: the child made ready for pool `pool` of `fpm`, on
/// the master's engine with the pool's values laid over it ([`lay`]);
/// the child exits with 70 where a step fails.
pub(crate) fn init(fpm: &mut Fpm, pool: usize, slot: usize) -> Worker {
    let failed = |name: &str| -> ! {
        zlog!(Error @ fpm_child_init:202, "[pool {name}] child failed to initialize");
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
    // `fd_stderr_original`, the master's stderr as
    // `fpm_stdio_save_original_stderr` dup'd it, with no `FD_CLOEXEC`: php's
    // child never closes it, so a process a request execs holds it, and
    // this child lets go of it unclosed (#2428).
    if let Some(saved) = fpm.stderr_original.take() {
        let _unclosed = saved.into_raw_fd();
    }
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
                Error @ fpm_status_init_child:37,
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
    let Ok(listening) = crate::ffi::child_soft_quit(listening) else {
        zlog_sys!(
            Error @ fpm_signals_init_child:248,
            nix::errno::Errno::last(),
            "failed to init child signals: sigaction()"
        );
        failed(&name);
    };
    crate::master::unblock_all();
    crate::env::init_child(&fpm.pools[pool]);
    let Some(Ok(engine)) = fpm.engine.take().map(|engine| lay(engine, &config)) else {
        failed(&name)
    };
    let domain = fpm.pools[pool].domain;
    let allowed = if domain == Domain::Inet {
        allowed_clients(config.listen_allowed_clients.as_deref())
    } else {
        None
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
        listening,
        domain,
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

/// `fpm_php_init_child`'s `fpm_php_apply_defines`: the pool's
/// `php_value`s and then its `php_admin_value`s laid over the engine the
/// master started and the fork copied, which the child keeps, as php's
/// child keeps what its master's `php_module_startup` built and
/// preloaded; what laying them wrote to stderr written there, and each
/// one php does not know logged; nothing laid where another clone of
/// the engine stands.
fn lay(mut engine: Engine, config: &PoolConfig) -> Result<Engine, NotAlone> {
    let entries = (config.php_values.iter())
        .map(|(name, value)| (name.clone(), value.clone(), IniOrigin::Value))
        .chain(
            (config.php_admin_values.iter())
                .map(|(name, value)| (name.clone(), value.clone(), IniOrigin::Admin)),
        )
        .collect();
    let written = engine.alter_master_ini(entries)?;
    std::io::stderr().write_all(&written).ok();
    for (kind, values, line) in [
        ("php_value", &config.php_values, 110),
        ("php_admin_value", &config.php_admin_values, 119),
    ] {
        for (name, _) in values {
            if name.as_slice() != b"extension" && engine.ini_get(name).is_none() {
                zlog!(
                    Error @ (crate::zlog::Site::at("fpm_php_apply_defines", line)),
                    "Unable to set {kind} '{}'",
                    String::from_utf8_lossy(name)
                );
            }
        }
    }
    Ok(engine)
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
            let (conn, peer) = match self.listening.accept() {
                Ok(fd) => match self.domain {
                    Domain::Inet => {
                        let s = TcpStream::from(fd);
                        // A peer gone before its address is read is let go,
                        // so `listen.allowed_clients` never lets in a
                        // connection it could not judge.
                        let Ok(peer) = s.peer_addr() else {
                            continue;
                        };
                        (Conn::Tcp(s), Some(peer.ip()))
                    }
                    Domain::Unix => (Conn::Unix(UnixStream::from(fd)), None),
                },
                Err(e) => {
                    let transient = matches!(
                        e,
                        nix::errno::Errno::EINTR | nix::errno::Errno::ECONNABORTED
                    );
                    if crate::ffi::shutting_down() || !transient {
                        return None;
                    }
                    continue;
                }
            };
            if !allowed(self.allowed.as_deref(), peer) {
                zlog!(
                    Error @ (crate::zlog::Site::FCGI),
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
            let served = self.serve(begun.params, &wire, peer);
            *CURRENT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            if served == Served::StartFailed {
                // `fcgi_finish_request(request, 1)`, then
                // `php_module_shutdown` and `FPM_EXIT_SOFTWARE` out of
                // `main` (`Z/sapi/fpm/fpm/fpm_main.c:1838-1843`): what the
                // failure wrote ends the request, the connection is closed
                // whatever the web server asked to keep, and the child ends.
                wire.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .finish(true);
                crate::stdio::flush_child();
                return EXIT_SOFTWARE;
            }
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
    use nix::sys::time::TimeValLike as _;
    [UsageWho::RUSAGE_SELF, UsageWho::RUSAGE_CHILDREN]
        .into_iter()
        .filter_map(|who| getrusage(who).ok())
        .map(|u| u.user_time().num_microseconds() + u.system_time().num_microseconds())
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

/// How a request the child served ended.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Served {
    /// It started, and ended with its access log line written.
    Ended,
    /// Its start failed, an extension's `rinit` having raised (#1775):
    /// it ends before `fpm_request_end` writes the scoreboard and the
    /// access log, and the child ends after it
    /// (`Z/sapi/fpm/fpm/fpm_main.c:1838-1843`).
    StartFailed,
}

impl Worker {
    /// One request, from its variables to its access log line, and how
    /// it ended.
    fn serve(
        &mut self,
        params: Environment,
        wire: &Arc<Mutex<Wire>>,
        peer: Option<IpAddr>,
    ) -> Served {
        // `apache_request_headers` reads each parameter whole
        // (`sapi_add_request_header`); every other reading php makes of
        // one, `init_request_info`'s, the loop's, `getenv()`'s and
        // `$_SERVER`'s, goes through a `char *` and stops at a NUL (#2757).
        let headers = php_proto::cgi::request_headers(&Environment(
            fcgi::newest_first(&params).cloned().collect(),
        ));
        let mut env = params.cut_at_nul();
        let settings = Settings {
            fix_pathinfo: self.flag(b"cgi.fix_pathinfo", true),
            discard_path: self.flag(b"cgi.discard_path", false),
            proxy: Proxy::Read {
                encoded: self.flag(b"fastcgi.script_path_encoded", true),
            },
        };
        let doc_root = self.engine.ini_get(b"doc_root").filter(|r| !r.is_empty());
        let info =
            php_proto::cgi::request_info(&mut env, settings, doc_root.as_deref(), &Disk, C_NUMBER);
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
        let description = self.describe(&env, &info, headers, wire);
        let response = Box::new(Response {
            wire: Arc::clone(wire),
            board: Some(self.board()),
        });
        let query = env.get(b"QUERY_STRING");
        let (served, _) = self.engine.serve(description, response, |innermost| {
            if let Some(proc) = self.proc() {
                proc.set_chain_word(innermost.address());
            }
            self.look(&info, head_only, query)
        });
        if served.start_failed {
            return Served::StartFailed;
        }
        let (status, headers) = wire
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .head
            .as_ref()
            .map_or((200, Vec::new()), |h| (h.code, h.lines.clone()));
        let memory = u64::try_from(served.memory_peak).unwrap_or(0);
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
        let board = self.scoreboard.header();
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
        Served::Ended
    }

    /// What the loop serves once `php_request_startup` has run
    /// (`Z/sapi/fpm/fpm/fpm_main.c:1845-1903`): nothing for a request
    /// with no `REQUEST_METHOD`; the ping or the status page; `File not
    /// found.` for a request with no path translated, `Access denied.`
    /// past `security.limit_extensions`, and `Access denied.` or `No
    /// input file specified.` for a file that does not open, each logged
    /// as php logs it; and otherwise the file, the child's stage then
    /// `Executing`.
    fn look(
        &self,
        info: &php_proto::cgi::RequestInfo,
        head_only: bool,
        query: Option<&[u8]>,
    ) -> HostServes {
        let refused = |code, body: &[u8]| {
            HostServes::Page(HostPage {
                code,
                headers: Vec::new(),
                body: body.to_vec(),
            })
        };
        if info.method.is_none() {
            return HostServes::Nothing;
        }
        if let Some(page) = self.page(info, head_only, query) {
            return HostServes::Page(page);
        }
        let Some(path) = &info.path_translated else {
            zlog!(Debug @ main:1858, "Primary script unknown");
            return refused(404, b"File not found.\n");
        };
        if !self.extension_allowed(path) {
            return refused(403, b"Access denied.\n");
        }
        match std::fs::File::open(crate::conf::path(path)) {
            Ok(_) => {
                if let Some(proc) = self.proc() {
                    proc.set_stage(Stage::Executing, crate::master::clock_us());
                }
                HostServes::Program
            }
            Err(e) => HostServes::Page(unopened(path, &e)),
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
                Notice @ fpm_php_limit_extensions:252,
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
    ) -> Option<HostPage> {
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
        Some(crate::status::status(board.as_ref(), &keys, head_only))
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
                    (ini_text(&key), value.as_ref().map(ini_text))
                } else {
                    zlog!(
                        Error @ fastcgi_ini_parser:1404,
                        "Passing INI directive through FastCGI: only classic entries are allowed"
                    );
                    continue;
                };
                if key.is_empty() {
                    zlog!(Error @ fastcgi_ini_parser:1411, "Passing INI directive through FastCGI: empty key");
                    continue;
                }
                let Some(value) = value else {
                    zlog!(
                        Error @ fastcgi_ini_parser:1420,
                        "Passing INI directive through FastCGI: empty value for key '{}'",
                        String::from_utf8_lossy(&key)
                    );
                    continue;
                };
                if key.as_slice() != b"extension" && self.engine.ini_get(&key).is_none() {
                    zlog!(
                        Error @ fastcgi_ini_parser:1428,
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

    /// `sapi_cgi_pre_request_init`'s `.user.ini` files
    /// ([`php_sapi::cgi::user_ini`]), cached for `user_ini.cache_ttl`
    /// seconds per directory: `PG(user_ini_cache_ttl)`, the entry as its
    /// `OnUpdateLong` read it, `zend_ini_parse_quantity`'s value, so `1K`
    /// is 1024 seconds.
    fn user_ini(&mut self, path_translated: &[u8], env: &Environment) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Some(doc_root) = env.get(b"DOCUMENT_ROOT") else {
            return Vec::new();
        };
        let dir = php_sapi::cgi::user_ini_dir(path_translated);
        let now = crate::scoreboard::epoch();
        let ttl = php_ext::request::Request::ini_parse_bytes(
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
        let entries = php_sapi::cgi::user_ini(&self.engine, &dir, doc_root);
        self.user_config.retain(|(d, _)| *d != dir);
        self.user_config.push((
            dir,
            UserConfig {
                expires: now.saturating_add(ttl),
                entries: entries.clone(),
            },
        ));
        entries
    }

    /// The request description of a request of the web: its entries the
    /// `PHP_VALUE`s kept, then the php.ini's `[HOST=]` table of its
    /// `SERVER_NAME` and `[PATH=]` tables along its directory, which
    /// `sapi_cgi_activate` sets ([`Engine::activated_ini`]), then its
    /// `.user.ini` lines; `headers` are `apache_request_headers`', read
    /// off the parameters as they came.
    fn describe(
        &mut self,
        env: &Environment,
        info: &php_proto::cgi::RequestInfo,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        wire: &Arc<Mutex<Wire>>,
    ) -> RequestDescription {
        let mut ini = self.sticky.clone();
        if let Some(path) = &info.path_translated {
            ini.extend(self.engine.activated_ini(env.get(b"SERVER_NAME"), path));
            ini.extend(
                self.user_ini(path, env)
                    .into_iter()
                    .map(|(k, v)| (k, v, IniOrigin::PerDir)),
            );
        }
        let mut body = Body::default();
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
        let web = WebRequest {
            body,
            response_code: info.response_code,
            headers,
            ..php_sapi::cgi::web(env, info)
        };
        RequestDescription {
            env: process_env,
            params: newest.0,
            // The child's directory, the pool's `chdir` where it names
            // one (`fpm_unix_init_child`, `Z/sapi/fpm/fpm/fpm_unix.c:457`);
            // `php_execute_script` enters the program's and comes back
            // before the request shuts down.
            cwd: std::env::current_dir().unwrap_or_default(),
            chdir: Chdir::IntoProgramDir,
            server: rows,
            sapi_name: String::from(NAME),
            web: Some(web),
            script_name: info.path_translated.clone(),
            ini,
            program_file: ProgramFile::Listed,
            ..RequestDescription::default()
        }
    }
}

/// `sapi_cgi_read_post`: the body, up to its length, from
/// `REQUEST_BODY_FILE` where the server names one and otherwise from
/// the request's `FCGI_STDIN` records; a file that does not open is
/// refused with php's warning, which the request raises where php reads
/// the body.
///
/// ```php
/// var_dump(file_get_contents("php://input"), error_get_last()["message"]);
/// ```
/// ```request fpm
/// --ENV--
/// REQUEST_BODY_FILE=/nonexistent/body
/// --POST--
/// a=1
/// ```
/// ```expect
/// Content-type: text/html; charset=UTF-8
///
///
/// Warning: REQUEST_BODY_FILE: open('/nonexistent/body') failed: No such file or directory (2) in Unknown on line 0
/// string(0) ""
/// string(82) "REQUEST_BODY_FILE: open('/nonexistent/body') failed: No such file or directory (2)"
/// ```
fn read_body(env: &Environment, length: i64, wire: &Arc<Mutex<Wire>>) -> Body {
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
                return Body::Refused(format!(
                    "REQUEST_BODY_FILE: open('{}') failed: {} ({})",
                    String::from_utf8_lossy(file),
                    crate::conf::errno(&e).desc(),
                    crate::conf::errno(&e) as i32
                ));
            }
        }
        body.truncate(read);
        return Body::from(body);
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
    Body::from(body)
}

/// A program that did not open with `error`
/// (`Z/sapi/fpm/fpm/fpm_main.c:1879-1888`): the line php-fpm logs for it,
/// then its page, `Access denied.` for `EACCES` alone
/// ([`php_sapi::cgi::unopened`]).
fn unopened(path: &[u8], error: &std::io::Error) -> HostPage {
    zlog!(
        Error @ main:1881,
        "Unable to open primary script: {} ({})",
        String::from_utf8_lossy(path),
        crate::conf::errno(error).desc()
    );
    php_sapi::cgi::unopened(Some(error))
}

#[cfg(test)]
mod tests {
    /// php-fpm denies a script whose open met `EACCES` and answers every
    /// other errno as no input file, `EPERM` among them, which macOS
    /// answers for a file under a directory the process may not read
    /// (#2876); `EACCES` itself is held against the pinned php-fpm by
    /// tests/lines.toml's script of mode 0.
    #[test]
    fn a_script_that_does_not_open_with_eperm_is_no_input_file() {
        let page = |errno: i32| {
            let page = super::unopened(b"/x.php", &std::io::Error::from_raw_os_error(errno));
            (page.code, page.body)
        };
        assert_eq!(
            page(libc::EPERM),
            (404, b"No input file specified.\n".to_vec())
        );
        assert_eq!(
            page(libc::ENOENT),
            (404, b"No input file specified.\n".to_vec())
        );
        assert_eq!(page(libc::EACCES), (403, b"Access denied.\n".to_vec()));
    }
}
