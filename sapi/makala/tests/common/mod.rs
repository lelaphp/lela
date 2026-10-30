//! What every test of the binary over a socket shares: a document root of
//! its own, the server started on a `port` the system picks, and a response
//! read as a client reads it.

#![allow(
    dead_code,
    reason = "each test crate uses its own part of what is shared"
)]

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A document root of its own for `name`, holding `files`.
pub fn root(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("makala-{name}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for (file, text) in files {
        std::fs::write(dir.join(file), text).unwrap_or_else(|e| panic!("{file}: {e}"));
    }
    dir
}

/// The server, killed when the test lets go of it.
pub struct Server {
    child: Child,
    /// The clear listener's address.
    pub addr: SocketAddr,
    /// The TLS listener's address, where the line asked for one.
    pub tls: Option<SocketAddr>,
}

/// This crate's directory as cargo sets `CARGO_MANIFEST_DIR` when it runs
/// the test, never the path `env!` compiled in: cargo reuses a test binary
/// across two checkouts that share a target directory (#2790).
pub fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("cargo sets CARGO_MANIFEST_DIR for a test it runs")
}

impl Server {
    /// `makala -n` over `root` with two request threads, on a `port` of the
    /// system's.
    pub fn start(root: &Path) -> Server {
        Server::with_args(root, &["--threads", "2"])
    }

    /// `makala -n` over `root` with `args` after the address, once its
    /// started line names the addresses.
    pub fn with_args(root: &Path, args: &[&str]) -> Server {
        Server::in_dir(root, &manifest_dir(), args)
    }

    /// [`Server::with_args`], the process started in the working
    /// directory `dir`.
    pub fn in_dir(root: &Path, dir: &Path, args: &[&str]) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_makala"))
            .current_dir(dir)
            .args(["-n", "--listen", "127.0.0.1:0"])
            .args(args)
            .arg("--root")
            .arg(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("starting makala: {e}"));
        let stderr = child.stderr.take().unwrap_or_else(|| panic!("no stderr"));
        let mut lines = BufReader::new(stderr).lines();
        let line = lines
            .next()
            .and_then(Result::ok)
            .unwrap_or_else(|| panic!("makala wrote no started line"));
        let urls = line
            .split_once("listening on ")
            .and_then(|(_, rest)| rest.split_once(", document root "))
            .map_or_else(|| panic!("not a started line: {line}"), |(urls, _)| urls);
        let at = |scheme: &str| {
            urls.split(", ")
                .find_map(|url| url.strip_prefix(scheme)?.parse().ok())
        };
        let addr = at("http://").unwrap_or_else(|| panic!("no clear address: {line}"));
        let tls = at("https://");
        // What the server writes later is drained, so it never blocks on
        // a full pipe.
        std::thread::spawn(move || for _ in lines {});
        Server { child, addr, tls }
    }

    /// SIGTERM sent to the server, as `systemd` stops it, through
    /// `kill(1)`, so the test needs no `unsafe` of its own.
    pub fn terminate(&self) {
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap_or_else(|e| panic!("running kill: {e}"));
        assert!(sent.success(), "kill -TERM: {sent}");
    }

    /// How the server ended, where it did within `limit`.
    pub fn ended_within(&mut self, limit: Duration) -> Option<ExitStatus> {
        let mut ended = None;
        eventually(limit, || {
            ended = self.child.try_wait().ok().flatten();
            ended.is_some()
        });
        ended
    }

    pub fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(self.addr)
            .unwrap_or_else(|e| panic!("connecting to {}: {e}", self.addr));
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap_or_else(|e| panic!("{e}"));
        stream
    }

    /// `bytes` sent on a connection of their own, and what came back
    /// until the server closed it or `wait` passed with nothing more.
    pub fn exchange(&self, bytes: &[u8], wait: Duration) -> Exchange {
        let mut stream = self.connect();
        stream.write_all(bytes).unwrap_or_else(|e| panic!("{e}"));
        read_until_closed(&mut stream, wait)
    }

    /// `request` written on a connection of its own, left open for the
    /// test to read.
    pub fn send(&self, request: &Request) -> TcpStream {
        let mut stream = self.connect();
        stream
            .write_all(&request.bytes())
            .expect("write the request");
        stream
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// A well-formed HTTP/1.1 request, for a test whose subject is not the
/// framing: the request line, `Host: a` unless [`Request::host`] names
/// another, then the fields in the order they were added, and the body
/// with its `Content-Length`. A test of the framing writes its bytes.
#[derive(Debug, Clone)]
pub struct Request {
    method: String,
    target: String,
    host: String,
    fields: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    /// `method target`, no field but the host's and no body.
    pub fn new(method: &str, target: &str) -> Request {
        Request {
            method: method.to_owned(),
            target: target.to_owned(),
            host: "a".to_owned(),
            fields: Vec::new(),
            body: Vec::new(),
        }
    }

    /// `GET target`.
    pub fn get(target: &str) -> Request {
        Request::new("GET", target)
    }

    /// `POST target`.
    pub fn post(target: &str) -> Request {
        Request::new("POST", target)
    }

    /// The `Host` field's value.
    #[must_use]
    pub fn host(mut self, host: &str) -> Request {
        host.clone_into(&mut self.host);
        self
    }

    /// One more field, after those already added.
    #[must_use]
    pub fn field(mut self, name: &str, value: &str) -> Request {
        self.fields.push((name.to_owned(), value.to_owned()));
        self
    }

    /// `Connection: close`, after the fields already added.
    #[must_use]
    pub fn close(self) -> Request {
        self.field("Connection", "close")
    }

    /// The body, and its `Content-Length` after the fields already
    /// added.
    #[must_use]
    pub fn body(self, body: impl Into<Vec<u8>>) -> Request {
        let body = body.into();
        let mut request = self.field("Content-Length", &body.len().to_string());
        request.body = body;
        request
    }

    /// The request as the client writes it.
    pub fn bytes(&self) -> Vec<u8> {
        let mut bytes = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\n",
            self.method, self.target, self.host
        )
        .into_bytes();
        for (name, value) in &self.fields {
            bytes.extend_from_slice(&[name.as_bytes(), b": ", value.as_bytes(), b"\r\n"].concat());
        }
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// How a connection's reading ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The server closed it: the read saw the end.
    Closed,
    /// The server reset it.
    Reset,
    /// Still open when the wait passed.
    Open,
}

/// What came back on one connection.
#[derive(Debug)]
pub struct Exchange {
    pub bytes: Vec<u8>,
    pub ending: Ending,
}

impl Exchange {
    /// The status line of each response in the bytes, in order.
    pub fn statuses(&self) -> Vec<String> {
        let text = String::from_utf8_lossy(&self.bytes);
        let mut statuses = Vec::new();
        let mut at = 0;
        while let Some(found) = text[at..].find("HTTP/1.") {
            let start = at + found;
            let end = text[start..].find("\r\n").map_or(text.len(), |n| start + n);
            statuses.push(text[start..end].to_owned());
            at = end;
        }
        statuses
    }

    /// The code of each response in the bytes, in order.
    pub fn codes(&self) -> Vec<u16> {
        self.statuses()
            .iter()
            .filter_map(|line| line.split(' ').nth(1)?.parse().ok())
            .collect()
    }

    /// The first response, whole, its body unchunked.
    pub fn response(&self) -> Response {
        parse_response(&self.bytes)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// What `stream` reads until the server closes it, resets it, or `wait`
/// passes with nothing more.
pub fn read_until_closed(stream: &mut TcpStream, wait: Duration) -> Exchange {
    stream
        .set_read_timeout(Some(wait))
        .unwrap_or_else(|e| panic!("{e}"));
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 65536];
    let ending = loop {
        match stream.read(&mut buf) {
            Ok(0) => break Ending::Closed,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                break Ending::Open;
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => break Ending::Reset,
            Err(e) => panic!("reading: {e}"),
        }
    };
    Exchange { bytes, ending }
}

/// A response as a client reads it.
#[derive(Debug)]
pub struct Response {
    pub status: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(had, _)| had.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// The response the server wrote, read whole off a connection the request
/// asked to close: its head, and its body unchunked where it came
/// chunked.
pub fn read_response(stream: &mut TcpStream) -> Response {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .unwrap_or_else(|e| panic!("reading the response: {e}"));
    parse_response(&bytes)
}

/// One response in `bytes`, which hold it whole.
pub fn parse_response(bytes: &[u8]) -> Response {
    let end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(bytes)));
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let mut body = bytes[end + 4..].to_vec();
    let chunked = headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("transfer-encoding") && value == "chunked");
    if chunked {
        body = unchunked(&body);
    }
    Response {
        status,
        headers,
        body,
    }
}

/// A chunked body's data.
pub fn unchunked(mut body: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    loop {
        let line = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .unwrap_or_else(|| panic!("a chunk with no size line"));
        let size = usize::from_str_radix(&String::from_utf8_lossy(&body[..line]), 16)
            .unwrap_or_else(|e| panic!("a chunk size: {e}"));
        body = &body[line + 2..];
        if size == 0 {
            return data;
        }
        data.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

/// Waits until `done` holds or `limit` passes, and says whether it held.
pub fn eventually(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}
