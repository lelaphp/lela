//! The request side (#2337): a bounded pool of request threads, each
//! taking whole requests off one bounded queue and serving each through
//! its own `Engine::serve` on the one engine they share (design §5.14).
//! A request is described from its HTTP request as a web server describes
//! one to php-fpm, in the CGI environment, and read through what php-cgi
//! and php-fpm share (`php_sapi::cgi`); its body is a reader over the
//! channel its connection feeds, pulled as the request reads it (#2338);
//! and its sink writes onto the channel its connection's response reads.
//!
//! The environment is provisional: the mapping the oracle holds, nginx's
//! stock `fastcgi_params` against the pinned php-fpm, is #2334's and
//! arrives with #2347.

use std::io::Read;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};

use hyper::body::Bytes;
use php_proto::cgi::{Environment, Files, RequestInfo};
use php_sapi::cgi::Disk;
use php_sapi::{
    Aborted, Body, Engine, HostPage, HostServes, HostThread, OutputSink, RequestDescription,
    ResponseHead, WebRequest,
};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;

/// One request as its connection hands it over: the head as hyper read
/// it, the body still arriving, and where the response goes.
pub(crate) struct Job {
    pub(crate) head: Head,
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
}

/// The request threads.
pub(crate) struct Pool {
    threads: Vec<HostThread>,
}

impl Pool {
    /// `config.request_threads` threads, each with `config.stack_size`
    /// of stack, serving the requests the returned queue carries; the
    /// queue holds `config.request_queue` of them before its sender
    /// waits.
    pub(crate) fn start(
        engine: &Engine,
        config: &Config,
    ) -> std::io::Result<(mpsc::Sender<Job>, Pool)> {
        let (queue, jobs) = mpsc::channel(config.request_queue);
        let jobs = Arc::new(Mutex::new(jobs));
        let root = Arc::new(config.document_root.as_os_str().as_encoded_bytes().to_vec());
        let mut threads = Vec::with_capacity(config.request_threads);
        for n in 0..config.request_threads {
            let (engine, jobs, root) = (engine.clone(), Arc::clone(&jobs), Arc::clone(&root));
            threads.push(php_sapi::host_thread(
                &format!("makala-request-{n}"),
                config.stack_size,
                move || serve_each(&engine, &jobs, &root),
            )?);
        }
        Ok((queue, Pool { threads }))
    }

    /// Every thread joined, once the queue's senders are gone.
    pub(crate) fn join(self) {
        for thread in self.threads {
            thread.join();
        }
    }
}

/// A request thread: the queue's next request served, until the queue is
/// closed. A panic is contained at the request's edge (design §5.14,
/// #2337's M9): the request is dropped whole, its connection answers a
/// head never sent with a 500, and the thread serves the next.
fn serve_each(engine: &Engine, jobs: &Mutex<mpsc::Receiver<Job>>, root: &[u8]) {
    loop {
        let next = jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .blocking_recv();
        let Some(job) = next else {
            return;
        };
        if std::panic::catch_unwind(AssertUnwindSafe(|| serve(engine, root, job))).is_err() {
            crate::log(b"makala: a request panicked; its connection is answered 500");
        }
    }
}

/// One request through its own `Engine::serve`.
fn serve(engine: &Engine, root: &[u8], job: Job) {
    let Job {
        head,
        body,
        mut sink,
    } = job;
    let mut env = environment(&head, root);
    let settings = php_sapi::cgi::settings(engine);
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
    let web = WebRequest {
        body: Body::Stream(Box::new(Arriving {
            pieces: body,
            held: Bytes::new(),
        })),
        head_ini: &[],
        ..php_sapi::cgi::web(&env, &info)
    };
    let description = RequestDescription {
        server: php_sapi::cgi::server_variables(&env, &info, settings),
        params: env.0,
        cwd,
        sapi_name: crate::NAME.to_owned(),
        web: Some(web),
        script_name: info.path_translated.clone(),
        ..RequestDescription::default()
    };
    engine.serve_started(description, &mut sink, || look(&info));
}

/// What the request serves once it has started: the file its path names
/// where that is a `.php` file under the root, as php-fpm's
/// `security.limit_extensions` keeps it; php-fpm's `File not found.` where
/// the path names no file, and its `Access denied.` for a file of another
/// kind, which no server of PHP runs as a program (static files are
/// #2346's).
fn look(info: &RequestInfo) -> HostServes {
    let refused = |code, body: &[u8]| {
        HostServes::Page(HostPage {
            code,
            headers: Vec::new(),
            body: body.to_vec(),
        })
    };
    match &info.path_translated {
        Some(path) if Disk.is_file(path) && path.ends_with(b".php") => HostServes::Program,
        Some(path) if Disk.is_file(path) => refused(403, b"Access denied.\n"),
        _ => refused(404, b"File not found.\n"),
    }
}

/// The CGI environment a web server hands php-fpm for `head`, after
/// nginx's stock `fastcgi_params`: the script the path names under the
/// root, the request's own variables, and each header field as an
/// `HTTP_` variable. The path is `php -S`'s `normalize_vpath` of it,
/// percent-decoded with `..` held at the root, so no request names a file
/// outside it; a path that ends in `/` names its directory's `index.php`.
fn environment(head: &Head, root: &[u8]) -> Environment {
    let (path, query) = match head.target.iter().position(|b| *b == b'?') {
        Some(at) => (&head.target[..at], &head.target[at + 1..]),
        None => (&head.target[..], &[][..]),
    };
    let mut script = php_proto::http::normalize_vpath(path);
    if !script.starts_with(b"/") {
        script.insert(0, b'/');
    }
    if script.ends_with(b"/") {
        script.extend_from_slice(b"index.php");
    }
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
    let mut filename = root.to_vec();
    filename.extend_from_slice(&script);
    let mut env = vec![
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
        (b"SCRIPT_NAME".to_vec(), script.clone()),
        (b"REQUEST_URI".to_vec(), head.target.clone()),
        (b"DOCUMENT_URI".to_vec(), script),
        (b"DOCUMENT_ROOT".to_vec(), root.to_vec()),
        (
            b"SERVER_PROTOCOL".to_vec(),
            head.protocol.as_bytes().to_vec(),
        ),
        (b"REQUEST_SCHEME".to_vec(), b"http".to_vec()),
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
    ];
    for (name, value) in &head.fields {
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
/// end (`Body::Stream`).
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
/// does on the write that finds the client gone and not before.
pub(crate) struct Sink {
    head: Option<oneshot::Sender<ResponseHead>>,
    body: Option<mpsc::Sender<Bytes>>,
}

impl Sink {
    pub(crate) fn new(head: oneshot::Sender<ResponseHead>, body: mpsc::Sender<Bytes>) -> Sink {
        Sink {
            head: Some(head),
            body: Some(body),
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

    fn send_headers(&mut self, head: &ResponseHead) {
        if let Some(waiting) = self.head.take() {
            waiting.send(head.clone()).ok();
        }
    }

    fn log(&mut self, entry: &[u8]) {
        crate::log(entry);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        use std::io::Write;
        std::io::stderr().lock().write_all(bytes).ok();
    }

    /// The request holds the sink while it runs, so each write reaches
    /// the connection as it is made.
    fn held(&mut self) -> Option<Box<dyn OutputSink + Send>> {
        Some(Box::new(Sink {
            head: self.head.take(),
            body: self.body.take(),
        }))
    }
}
