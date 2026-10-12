//! The request side (#2337): a bounded pool of request threads, each
//! taking programs' whole requests off one bounded queue, as the
//! connection side routed them, and serving each through its own
//! `Engine::serve` on the one engine they share (design §5.14).
//! A request is described from its HTTP request as a web server describes
//! one to php-fpm, in the CGI environment, and read through what php-cgi
//! and php-fpm share (`php_sapi::cgi`); its body is a reader over the
//! channel its connection feeds, pulled as the request reads it (#2338);
//! and its sink writes onto the channel its connection's response reads.
//!
//! The environment is the mapping the oracle holds (#2334, #2347): the
//! stock `fastcgi_params` of the nginx release `oracle.toml`'s
//! `[fastcgi]` names, as it records it, with `SCRIPT_FILENAME` first and
//! the header fields after, and `SERVER_SOFTWARE` the host's own (#2335),
//! registered in php-fpm's order. The witnesses of `request makala` fences hold it to the pinned
//! php-fpm handed the same request under that recipe.
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::convert::Infallible;
use std::io::Read;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, PoisonError};

use hyper::body::Bytes;
use php_proto::cgi::{Environment, RequestInfo, Settings};
use php_proto::vpath::Translated;
use php_sapi::cgi::Disk;
use php_sapi::{
    Aborted, Body, Engine, HostPage, HostServes, HostThread, OutputSink, RequestDescription,
    ResponseHead, WebRequest,
};
use tokio::sync::{mpsc, oneshot};

use crate::options::Options;

/// One program's request as its connection hands it over: the head as
/// hyper read it, the program its route found, the body still arriving,
/// and where the response goes.
pub(crate) struct Job {
    pub(crate) head: Head,
    /// The program under the root, and the path past it.
    pub(crate) program: Translated,
    /// The body's pieces as the connection reads them; closed at its end.
    pub(crate) body: mpsc::Receiver<Bytes>,
    pub(crate) sink: Sink,
}

/// A request's head, as the connection read it.
pub(crate) struct Head {
    pub(crate) method: Vec<u8>,
    /// The request target: the path, and the query after a `?`.
    pub(crate) target: Vec<u8>,
    /// `HTTP/1.1` or `HTTP/1.0`.
    pub(crate) protocol: &'static str,
    /// The header fields in the order they came, each name lowercase.
    pub(crate) fields: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) peer: SocketAddr,
    pub(crate) local: SocketAddr,
    pub(crate) scheme: Scheme,
}

/// Whether a request came clear or under TLS (#2343).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    /// Clear, on the `--listen` port.
    Http,
    /// Under TLS, on the `--listen-tls` port.
    Https,
}

impl Scheme {
    /// Its name, as a URL and `REQUEST_SCHEME` spell it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

impl Head {
    /// The target's path, before a `?`.
    pub(crate) fn path(&self) -> &[u8] {
        self.target_parts().0
    }

    /// The target's query, after a `?`; empty where it has none.
    fn query(&self) -> &[u8] {
        self.target_parts().1
    }

    fn target_parts(&self) -> (&[u8], &[u8]) {
        match self.target.iter().position(|b| *b == b'?') {
            Some(at) => (&self.target[..at], &self.target[at + 1..]),
            None => (&self.target[..], &[][..]),
        }
    }
}

/// The request threads.
pub(crate) struct Pool {
    threads: Vec<HostThread>,
    /// Closed once every thread has ended: each holds a sender, which
    /// nothing is ever sent on, until it ends.
    ended: std::sync::mpsc::Receiver<Infallible>,
}

impl Pool {
    /// `options.request_threads` threads, each with `options.stack_size`
    /// of stack, each request the returned queue carries served by
    /// `serve`; the queue holds `options.request_queue` of them before its
    /// sender waits.
    pub(crate) fn start(
        options: &Options,
        serve: impl Fn(Job) + Clone + Send + 'static,
    ) -> std::io::Result<(mpsc::Sender<Job>, Pool)> {
        let (queue, jobs) = mpsc::channel(options.request_queue);
        let jobs = Arc::new(Mutex::new(jobs));
        let (alive, ended) = std::sync::mpsc::channel();
        let mut threads = Vec::with_capacity(options.request_threads);
        for n in 0..options.request_threads {
            let (jobs, serve, alive) = (Arc::clone(&jobs), serve.clone(), alive.clone());
            threads.push(php_sapi::host_thread(
                &format!("makala-request-{n}"),
                options.stack_size,
                move || {
                    serve_each(&jobs, &serve);
                    drop(alive);
                },
            )?);
        }
        Ok((queue, Pool { threads, ended }))
    }

    /// Every thread joined, once the queue's senders are gone and each
    /// has ended its request, where that is before `deadline`; whether it
    /// was. A thread still in a request at the deadline is left to the
    /// process's end, as php-fpm's master kills a child still serving
    /// when its `process_control_timeout` has passed.
    pub(crate) fn join_by(self, deadline: std::time::Instant) -> bool {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match self.ended.recv_timeout(left) {
            Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => return false,
            Ok(never) => match never {},
        }
        for thread in self.threads {
            thread.join();
        }
        true
    }
}

/// A request thread: the queue's next request served, until the queue is
/// closed. A panic is contained at the request's edge (design §5.14,
/// #2337's M9): the request is dropped whole, its connection answers a
/// head never sent with a 500 and cuts a response under way, and the
/// thread serves the next.
fn serve_each(jobs: &Mutex<mpsc::Receiver<Job>>, serve: &impl Fn(Job)) {
    loop {
        let next = jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .blocking_recv();
        let Some(job) = next else {
            return;
        };
        let who = (
            job.head.peer,
            job.head.method.clone(),
            job.head.target.clone(),
        );
        if let Err(panicked) = std::panic::catch_unwind(AssertUnwindSafe(|| serve(job))) {
            let (peer, method, target) = who;
            let why = panicked
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panicked.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("a panic with no message");
            crate::log::error(
                format!(
                    "makala: {peer} \"{} {}\" panicked: {why}; its connection is answered 500, or cut where its response was under way",
                    String::from_utf8_lossy(&method),
                    String::from_utf8_lossy(&target)
                )
                .as_bytes(),
            );
        }
    }
}

/// How the pool serves each request: through its own `Engine::serve` on
/// `engine`, under the document root `root`.
pub(crate) fn serving(engine: &Engine, root: &Arc<[u8]>) -> impl Fn(Job) + Clone + Send + 'static {
    let engine = engine.clone();
    let root = Arc::clone(root);
    move |job| serve(&engine, &root, job)
}

/// One program's request, through its own `Engine::serve`.
fn serve(engine: &Engine, root: &[u8], job: Job) {
    let Job {
        head,
        program,
        body,
        mut sink,
    } = job;
    let mut env = environment(&head, root, &program, head.query());
    // php-fpm's defaults of the entries `init_request_info` reads, which
    // this host declares none of: `cgi.fix_pathinfo` on, which finds the
    // program as named and leaves the split as it came.
    let settings = Settings::default();
    let info = php_proto::cgi::request_info(&mut env, settings, None, &Disk);
    let cwd = info
        .path_translated
        .as_deref()
        .and_then(|path| {
            php_sapi::cgi::path_of(path)
                .parent()
                .map(std::path::Path::to_path_buf)
        })
        .unwrap_or_else(|| php_sapi::cgi::path_of(root));
    // php-fpm registers the parameters newest first, as `fcgi_hash_apply`
    // walks them (`Z/main/fastcgi.c:399-407`): the header fields first and
    // `SCRIPT_FILENAME` last, in `$_SERVER` and `getallheaders()` alike.
    let newest = Environment(php_proto::fcgi::newest_first(&env).cloned().collect());
    let web = WebRequest {
        body: Body::Stream(Box::new(Arriving {
            pieces: body,
            held: Bytes::new(),
        })),
        head_ini: &[],
        headers: php_proto::cgi::request_headers(&newest),
        // `SG(request_info).request_uri`, which a host outside CGI fills
        // with the path the request asked for, as `php -S` does, and
        // where phar's front controller finds its entry outside CGI
        // (`Phar::webPhar`): the path as far as the program and past it,
        // so the entry is the one php-fpm's `SCRIPT_NAME` and `PATH_INFO`
        // give it, where php-fpm's `request_uri` is `SCRIPT_NAME` alone.
        request_uri: env.get(b"DOCUMENT_URI").map(<[u8]>::to_vec),
        ..php_sapi::cgi::web(&env, &info)
    };
    let description = RequestDescription {
        server: php_sapi::cgi::server_variables(&env, &info, settings),
        params: newest.0,
        cwd,
        sapi_name: crate::NAME.to_owned(),
        web: Some(web),
        script_name: info.path_translated.clone(),
        ..RequestDescription::default()
    };
    engine.serve_started(description, &mut sink, || look(&info));
}

/// What the request serves once it has started, in php-fpm's order
/// (`Z/sapi/fpm/fpm/fpm_main.c:1854-1888`): `File not found.` with a 404
/// where the path names no script; `Access denied.` with a 403 for a
/// script whose name does not end in `.php`, the default of its
/// `security.limit_extensions`, so no file of another kind runs as a
/// program, `T.PHP` among them; for one that does not open,
/// `Access denied.` with a 403 where it may not be read and `No input
/// file specified.` with a 404 otherwise; and else the program.
fn look(info: &RequestInfo) -> HostServes {
    let refused = |code, body: &[u8]| {
        HostServes::Page(HostPage {
            code,
            headers: Vec::new(),
            body: body.to_vec(),
        })
    };
    let Some(path) = &info.path_translated else {
        return refused(404, b"File not found.\n");
    };
    if !(path.len() > b".php".len() && path.ends_with(b".php")) {
        return refused(403, b"Access denied.\n");
    }
    match std::fs::File::open(php_sapi::cgi::path_of(path)) {
        Ok(_) => HostServes::Program,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            refused(403, b"Access denied.\n")
        }
        Err(_) => refused(404, b"No input file specified.\n"),
    }
}

/// The CGI environment a web server hands php-fpm for `head`, after
/// nginx's stock `fastcgi_params` with its documented split of the path
/// past the program (`oracle.toml`'s `[fastcgi]`, #2637): the path's info
/// first, the program the request found under the root, the request's
/// own variables, and each header field as an `HTTP_` variable. The
/// split is `php -S`'s walk ([`Translated`]), which routed the request:
/// `SCRIPT_NAME` is the path as far as the program, with the index file
/// where it named a directory, nginx's `$fastcgi_script_name`;
/// `PATH_INFO` the rest, empty where there is none, as nginx sends
/// `$fastcgi_path_info`; and `DOCUMENT_URI` the two, nginx's `$uri`
/// after the internal redirect its `index` makes of a directory. No
/// `PATH_TRANSLATED` is set: `php -S` sets none, nor does php-fpm for a
/// program found as named.
///
/// `$_SERVER` in php-fpm's order, held to the pinned php-fpm handed the
/// same request under the recipe with its default `clear_env`: every row
/// but php-fpm's own, `FCGI_ROLE`, the `FastCGI` role, which a host that speaks
/// HTTP has none of, and `USER` and `HOME`, which `fpm_env_init_child`
/// puts in a child's emptied environment; the ports, which are the run's
/// own, and `SERVER_SOFTWARE`, nginx's in the recipe and
/// `makala/<version>` here (#2335), masked.
///
/// ```php
/// foreach ($_SERVER as $name => $value) {
///     if (in_array($name, ['FCGI_ROLE', 'USER', 'HOME', 'REQUEST_TIME', 'REQUEST_TIME_FLOAT'])) continue;
///     echo $name, '=', in_array($name, ['REMOTE_PORT', 'SERVER_PORT', 'SERVER_SOFTWARE']) ? '(the run)' : $value, "\n";
/// }
/// ```
/// ```request makala
/// --GET--
/// a=1
/// --COOKIE--
/// c=v
/// --POST--
/// p=1
/// ```
/// ```expect capture
/// 200
/// x-powered-by: PHP/8.5.9
/// content-type: text/html; charset=UTF-8
///
/// HTTP_CONTENT_LENGTH=3
/// HTTP_CONTENT_TYPE=application/x-www-form-urlencoded
/// HTTP_COOKIE=c=v
/// HTTP_HOST=localhost
/// REDIRECT_STATUS=200
/// SERVER_NAME=localhost
/// SERVER_PORT=(the run)
/// SERVER_ADDR=127.0.0.1
/// REMOTE_PORT=(the run)
/// REMOTE_ADDR=127.0.0.1
/// SERVER_SOFTWARE=(the run)
/// GATEWAY_INTERFACE=CGI/1.1
/// REQUEST_SCHEME=http
/// SERVER_PROTOCOL=HTTP/1.0
/// DOCUMENT_ROOT=%SCRATCH%
/// DOCUMENT_URI=/witness.php
/// REQUEST_URI=/witness.php?a=1
/// SCRIPT_NAME=/witness.php
/// CONTENT_LENGTH=3
/// CONTENT_TYPE=application/x-www-form-urlencoded
/// REQUEST_METHOD=POST
/// QUERY_STRING=a=1
/// SCRIPT_FILENAME=%SCRATCH%/witness.php
/// PATH_INFO=
/// PHP_SELF=/witness.php
/// ```
///
/// The path past the program is its `PATH_INFO` under both, and php-fpm,
/// finding the program as named, sets no `PATH_TRANSLATED`:
///
/// ```php
/// foreach (['SCRIPT_NAME', 'PATH_INFO', 'PHP_SELF', 'DOCUMENT_URI', 'PATH_TRANSLATED'] as $name) {
///     echo $name, '=', $_SERVER[$name] ?? '(none)', "\n";
/// }
/// ```
/// ```request makala
/// --ENV--
/// REQUEST_URI=/witness.php/index.php
/// --GET--
/// x=1
/// ```
/// ```expect capture
/// 200
/// x-powered-by: PHP/8.5.9
/// content-type: text/html; charset=UTF-8
///
/// SCRIPT_NAME=/witness.php
/// PATH_INFO=/index.php
/// PHP_SELF=/witness.php/index.php
/// DOCUMENT_URI=/witness.php/index.php
/// PATH_TRANSLATED=(none)
/// ```
fn environment(head: &Head, root: &[u8], program: &Translated, query: &[u8]) -> Environment {
    let field = |name: &[u8]| {
        head.fields
            .iter()
            .find(|(had, _)| had == name)
            .map(|(_, value)| value.clone())
    };
    let host = field(b"host").map_or_else(
        || head.local.ip().to_string().into_bytes(),
        |host| match host.iter().rposition(|b| *b == b':') {
            Some(at) if !host.ends_with(b"]") => host[..at].to_vec(),
            _ => host,
        },
    );
    let path_info = program.path_info.clone().unwrap_or_default();
    let mut filename = root.to_vec();
    filename.extend_from_slice(&program.vpath);
    let mut env = vec![
        (b"PATH_INFO".to_vec(), path_info.clone()),
        (b"SCRIPT_FILENAME".to_vec(), filename),
        (b"QUERY_STRING".to_vec(), query.to_vec()),
        (b"REQUEST_METHOD".to_vec(), head.method.clone()),
        (
            b"CONTENT_TYPE".to_vec(),
            field(b"content-type").unwrap_or_default(),
        ),
        (
            b"CONTENT_LENGTH".to_vec(),
            field(b"content-length").unwrap_or_default(),
        ),
        (b"SCRIPT_NAME".to_vec(), program.vpath.clone()),
        (b"REQUEST_URI".to_vec(), head.target.clone()),
        (
            b"DOCUMENT_URI".to_vec(),
            [&program.vpath[..], &path_info].concat(),
        ),
        (b"DOCUMENT_ROOT".to_vec(), root.to_vec()),
        (
            b"SERVER_PROTOCOL".to_vec(),
            head.protocol.as_bytes().to_vec(),
        ),
        (
            b"REQUEST_SCHEME".to_vec(),
            head.scheme.name().as_bytes().to_vec(),
        ),
    ];
    // nginx's `HTTPS $https if_not_empty`: `on` under TLS, else none.
    if head.scheme == Scheme::Https {
        env.push((b"HTTPS".to_vec(), b"on".to_vec()));
    }
    env.extend([
        (b"GATEWAY_INTERFACE".to_vec(), b"CGI/1.1".to_vec()),
        (
            b"SERVER_SOFTWARE".to_vec(),
            crate::SERVER_SOFTWARE.as_bytes().to_vec(),
        ),
        (
            b"REMOTE_ADDR".to_vec(),
            head.peer.ip().to_string().into_bytes(),
        ),
        (
            b"REMOTE_PORT".to_vec(),
            head.peer.port().to_string().into_bytes(),
        ),
        (
            b"SERVER_ADDR".to_vec(),
            head.local.ip().to_string().into_bytes(),
        ),
        (
            b"SERVER_PORT".to_vec(),
            head.local.port().to_string().into_bytes(),
        ),
        (b"SERVER_NAME".to_vec(), host),
        (b"REDIRECT_STATUS".to_vec(), b"200".to_vec()),
    ]);
    // nginx's `fastcgi_pass_request_headers`: each field after the
    // parameters, a name sent twice joined as nginx joins it since 1.23,
    // and a name with `_` in it left out, as nginx ignores one by its
    // `underscores_in_headers off`, so no field can pose as another
    // whose `-` reads as the same `_`.
    for (name, value) in head.fields.iter().filter(|(name, _)| !name.contains(&b'_')) {
        let mut var = b"HTTP_".to_vec();
        var.extend(name.iter().map(|b| match b {
            b'-' => b'_',
            b => b.to_ascii_uppercase(),
        }));
        match env.iter_mut().find(|(had, _)| *had == var) {
            Some((_, joined)) => {
                joined.extend_from_slice(if name == b"cookie" { b"; " } else { b", " });
                joined.extend_from_slice(value);
            }
            None => env.push((var, value.clone())),
        }
    }
    Environment(env)
}

/// The body as the connection reads it: each read hands out what is left
/// of the piece it holds, and waits for the next where it holds none; the
/// channel closed, at the body's end or with its connection gone, is the
/// end (`Body::Stream`). A body cut short reads as short, as php-fpm's
/// `sapi_cgi_read_post` ends where `fcgi_read` answers less.
struct Arriving {
    pieces: mpsc::Receiver<Bytes>,
    held: Bytes,
}

impl Read for Arriving {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.held.is_empty() {
            match self.pieces.blocking_recv() {
                Some(piece) => self.held = piece,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.held.len());
        buf[..n].copy_from_slice(&self.held.split_to(n));
        Ok(n)
    }
}

/// The request's sink: the head once, onto the channel its connection
/// waits on, and the body piece by piece onto the one its response
/// reads, each write handed over as the request makes it. A write after
/// the connection dropped its response, its client gone, is
/// [`Aborted`], which is when `connection_aborted()` turns true, as php's
/// does on the write that finds the client gone and not before. Dropped
/// with the request, it says the body is whole, unless the request is
/// unwinding from a panic.
pub(crate) struct Sink {
    head: Option<oneshot::Sender<ResponseHead>>,
    body: Option<mpsc::Sender<Bytes>>,
    whole: Option<oneshot::Sender<()>>,
}

/// A response body as its connection reads it: the pieces the request
/// writes, and whether it ended whole, which a request that panicked
/// never says.
pub(crate) struct Written {
    /// The body's pieces, closed when the request drops its sink.
    pub(crate) pieces: mpsc::Receiver<Bytes>,
    /// Sent when the request drops its sink outside a panic.
    pub(crate) whole: oneshot::Receiver<()>,
}

impl Sink {
    /// The sink over a response's three channels: its head, its body's
    /// pieces, and its body's being whole.
    pub(crate) fn new(
        head: oneshot::Sender<ResponseHead>,
        body: mpsc::Sender<Bytes>,
        whole: oneshot::Sender<()>,
    ) -> Sink {
        Sink {
            head: Some(head),
            body: Some(body),
            whole: Some(whole),
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(whole) = self.whole.take()
            && !std::thread::panicking()
        {
            whole.send(()).ok();
        }
    }
}

impl OutputSink for Sink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        let gone = Aborted { exit_status: None };
        let body = self.body.as_ref().ok_or(gone)?;
        if body.blocking_send(Bytes::copy_from_slice(bytes)).is_err() {
            self.body = None;
            return Err(gone);
        }
        Ok(())
    }

    /// Every write is handed over as it is made.
    fn flush(&mut self) {}

    /// php-fpm's `sapi_cgibin_flush` sends the head first, so `flush()`
    /// before any output sends it here too.
    ///
    /// ```php
    /// var_dump(headers_sent());
    /// flush();
    /// var_dump(headers_sent());
    /// ```
    /// ```request makala
    /// --GET--
    /// q=1
    /// ```
    /// ```expect capture
    /// 200
    /// x-powered-by: PHP/8.5.9
    /// content-type: text/html; charset=UTF-8
    ///
    /// bool(false)
    /// bool(true)
    /// ```
    fn flush_sends_head(&self) -> bool {
        true
    }

    /// `fastcgi_finish_request()`: the body's channel closed and the body
    /// said whole, so the connection ends the response while the program
    /// runs on; the response is closed after, whatever came before.
    fn finish(&mut self) -> bool {
        self.body = None;
        if let Some(whole) = self.whole.take() {
            whole.send(()).ok();
        }
        true
    }

    fn send_headers(&mut self, head: &ResponseHead) {
        if let Some(waiting) = self.head.take() {
            waiting.send(head.clone()).ok();
        }
    }

    fn log(&mut self, entry: &[u8]) {
        crate::log::error(entry);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        use std::io::Write;
        std::io::stderr().lock().write_all(bytes).ok();
    }

    /// The request holds the sink while it runs, so each write reaches
    /// the connection as it is made: the channels move to the sink the
    /// request holds, which the engine asks for once, as the request
    /// begins.
    fn held(&mut self) -> Option<Box<dyn OutputSink + Send>> {
        Some(Box::new(Sink {
            head: self.head.take(),
            body: self.body.take(),
            whole: self.whole.take(),
        }))
    }
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::disallowed_methods,
    reason = "a test serves requests that panic on cue, and starts the connection side on a thread of its own (seams.toml rule fork.threads)"
)]
mod tests {
    //! The containment at the request's edge, held over a real socket: the
    //! connection side as the binary runs it, and a pool of one request
    //! thread whose serving panics where the request's target says, which
    //! stands for a defect in the engine without one to call on.

    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Condvar;
    use std::time::Duration;

    use super::*;

    /// Whether the client of `/cut` has read what was written before the
    /// panic, which the request waits for, so the panic comes with the
    /// response under way on the wire and not only in hyper's buffer.
    static READ: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

    /// A document root of this test binary's, holding the programs the
    /// tests ask for, so the connection side routes each to the request
    /// side.
    fn root() -> &'static std::path::Path {
        static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            let root = std::env::temp_dir().join(format!("makala-requests-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap_or_else(|e| panic!("{e}"));
            for program in ["panic.php", "cut.php", "next.php"] {
                std::fs::write(root.join(program), "").unwrap_or_else(|e| panic!("{e}"));
            }
            root
        })
    }

    /// The connection side on a thread of its own, its requests served by
    /// one request thread: `/panic.php` panics before a head, `/cut.php`
    /// after a head and a write, and any other program is answered with
    /// the serving thread's name.
    fn start() -> SocketAddr {
        // A head as the engine sends a request's: its code, 200 where
        // the program set none.
        fn ok() -> ResponseHead {
            ResponseHead {
                code: 200,
                ..ResponseHead::default()
            }
        }
        let root = root();
        let argv: Vec<std::ffi::OsString> = [
            "--listen",
            "127.0.0.1:0",
            "--threads",
            "1",
            "--root",
            root.to_str().unwrap_or("/"),
        ]
        .iter()
        .map(Into::into)
        .collect();
        let options = Options::parse(&argv).unwrap_or_else(|e| panic!("{e}"));
        let listen = options
            .listen
            .unwrap_or_else(|| panic!("a --listen address"));
        let listener = TcpListener::bind(listen).unwrap_or_else(|e| panic!("{e}"));
        let addr = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
        let (queue, pool) = Pool::start(&options, |job: Job| {
            let Job { head, mut sink, .. } = job;
            // The request holds its sink as the engine has it hold one,
            // so a panic unwinds through the sink the request held.
            let mut sink = sink.held().unwrap_or_else(|| panic!("a sink to hold"));
            match &head.target[..] {
                b"/panic.php" => panic!("a request panicked before its head, on cue"),
                b"/cut.php" => {
                    sink.send_headers(&ok());
                    sink.write(b"partial").ok();
                    let (read, seen) = &READ;
                    let guard = read.lock().unwrap_or_else(PoisonError::into_inner);
                    drop(
                        seen.wait_timeout_while(guard, Duration::from_secs(30), |read| !*read)
                            .unwrap_or_else(PoisonError::into_inner),
                    );
                    panic!("a request panicked mid-response, on cue");
                }
                _ => {
                    sink.send_headers(&ok());
                    let name = std::thread::current().name().unwrap_or("").to_owned();
                    sink.write(name.as_bytes()).ok();
                }
            }
        })
        .unwrap_or_else(|e| panic!("{e}"));
        let router = Arc::new(crate::route::Router::new(&options));
        std::thread::spawn(move || {
            let _pool = pool;
            if let Ok(side) = crate::connection::ConnectionSide::new() {
                let listeners = vec![crate::connection::Listener {
                    socket: listener,
                    tls: None,
                }];
                side.run(options.limits, router, listeners, queue).ok();
            }
        });
        addr
    }

    /// What one request on a connection of its own reads back until the
    /// server closes it.
    fn exchange(addr: SocketAddr, target: &str) -> String {
        let mut stream = send(addr, target);
        let mut read = Vec::new();
        stream.read_to_end(&mut read).ok();
        String::from_utf8_lossy(&read).into_owned()
    }

    /// One request sent on a connection of its own.
    fn send(addr: SocketAddr, target: &str) -> TcpStream {
        let mut stream = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap_or_else(|e| panic!("{e}"));
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n"
        )
        .unwrap_or_else(|e| panic!("{e}"));
        stream
    }

    /// The error log of this test binary, one file every test that
    /// starts the server shares, as the process's one log thread does.
    fn error_log() -> &'static std::path::Path {
        static LOG: std::sync::OnceLock<(std::path::PathBuf, HostThread)> =
            std::sync::OnceLock::new();
        let (path, _) = LOG.get_or_init(|| {
            let path = std::env::temp_dir().join(format!("makala-errors-{}", std::process::id()));
            std::fs::remove_file(&path).ok();
            let thread = crate::log::start(None, crate::log::Destination::File(path.clone()), 64)
                .unwrap_or_else(|e| panic!("{e}"));
            (path, thread)
        });
        path
    }

    /// A request that panics is answered 500, its thread serves the next,
    /// and the error log names the request and the panic's message.
    #[test]
    fn a_request_that_panics_is_answered_500_and_its_thread_serves_the_next() {
        let log = error_log();
        let addr = start();
        let panicked = exchange(addr, "/panic.php");
        assert!(
            panicked.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "{panicked}"
        );
        crate::log::settle(Duration::from_secs(5));
        let logged = std::fs::read_to_string(log).unwrap_or_default();
        assert!(
            logged.lines().any(|line| line.contains("makala: 127.0.0.1:")
                && line.contains(" \"GET /panic.php\" panicked: a request panicked before its head, on cue; its connection is answered 500")),
            "{logged}"
        );
        let next = exchange(addr, "/next.php");
        assert!(next.starts_with("HTTP/1.1 200 OK\r\n"), "{next}");
        assert!(next.contains("makala-request-0"), "{next}");
    }

    /// A request that panics once its response is under way has its
    /// connection cut, so the client never reads the body's end and takes
    /// the response cut short for a whole one.
    #[test]
    fn a_request_that_panics_mid_response_is_cut_and_its_thread_serves_the_next() {
        let addr = start();
        let mut stream = send(addr, "/cut.php");
        let mut read = Vec::new();
        let mut buf = [0u8; 1024];
        while !read.windows(7).any(|w| w == b"partial") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => read.extend_from_slice(&buf[..n]),
            }
        }
        *READ.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
        READ.1.notify_all();
        stream.read_to_end(&mut read).ok();
        let cut = String::from_utf8_lossy(&read).into_owned();
        assert!(cut.starts_with("HTTP/1.1 200 OK\r\n"), "{cut}");
        assert!(cut.contains("partial"), "{cut}");
        assert!(!cut.ends_with("0\r\n\r\n"), "{cut:?}");
        let next = exchange(addr, "/next.php");
        assert!(next.ends_with("0\r\n\r\n"), "{next:?}");
        assert!(next.contains("makala-request-0"), "{next}");
    }
}
