//! A fence with a `request makala` (#2347): the snippet served as a
//! request of the web by the production server, `sapi/makala`'s `makala`
//! over HTTP for the engine, and for the oracle by the pinned php-fpm
//! handed the same request under the HTTP-to-FastCGI mapping the pin
//! records (`[fastcgi]`, [`Mapping`], #2334), its variables filled as the
//! server fills them, so the server's own copy of the mapping, which it
//! may not read from the pin, is held to the pin. Two values differ by what they are and a snippet that
//! prints them masks them: the ports, which are the run's own, and
//! `SERVER_SOFTWARE`, nginx's in the recipe and the server's own name
//! under #2335's ruling, which makala's own test holds. The request is
//! the one the fence's sections describe (`cgi::request`): its method,
//! query string, cookie and body, an HTTP/1.0 one to `localhost` for the
//! file the recipe names, so the response ends at the close; a field
//! nginx would drop or join, which no section describes, is the server's
//! own test's. What php logs is held as each host hands it on, php-fpm's
//! `FCGI_STDERR` and the entries of makala's error log after their time.
//!
//! What each run is held to is the response as a client of nginx in
//! front of php-fpm reads it: the code, then each header line PHP set,
//! its name lowercased, then a blank line and the body. php-fpm's
//! `Status:` line is the code, 200 where it sends none; the server's own
//! `date` and `connection` are no line PHP set; and a name's case is
//! lowercased on both sides, since the server's HTTP stack writes no
//! other (#2570).

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use super::fpm::{Pool, round, terminate};
use super::process;
use crate::oracle::{Mapping, Observation, Oracle, Piece, Recipe, Sent, Variable};
use crate::phpt::cgi;
use crate::scratch::Tree;

/// The request a fence describes, as a client sends it. The query string,
/// the target and each field's value are the bytes the test wrote, which
/// need not be text: a cookie's or a query's bytes in a legacy encoding
/// reach the server as written.
#[derive(Debug, Clone)]
pub(crate) struct Http {
    method: String,
    query: Vec<u8>,
    /// The request target the test names as its `REQUEST_URI`, a path
    /// past the program's name among it, empty where it names none.
    uri: Vec<u8>,
    /// The header fields in the order they are sent.
    fields: Vec<(&'static str, Vec<u8>)>,
    body: Vec<u8>,
}

impl Http {
    /// The request the fence's sections describe, run-tests.php's CGI
    /// translation of them read back as HTTP.
    fn of(own: &Tree, program: &Path, request: &str) -> Result<Http> {
        Ok(Http::described(&super::process::described(
            own,
            program,
            request,
            &[],
        )?))
    }

    /// The request a CGI-shaped test's translation describes
    /// ([`cgi::request`]), read back as HTTP: its method, query string,
    /// cookie, the encodings it accepts, content type and length, target
    /// and body. A value is one line, as a header field carries it, so a
    /// section's line breaks, which an environment variable holds and a
    /// field may not, are each a space.
    pub(crate) fn described(described: &cgi::Request) -> Http {
        let var = |name: &str| {
            let entries = described.env.entries().iter().rev();
            let mut found = entries.filter(|(had, _)| had == name);
            found
                .next()
                .map_or_else(Vec::new, |(_, value)| one_line(value.as_encoded_bytes()))
        };
        let fields = [
            ("Host", b"localhost".to_vec()),
            ("Cookie", var("HTTP_COOKIE")),
            ("Accept-Encoding", var("HTTP_ACCEPT_ENCODING")),
            ("Content-Type", var("CONTENT_TYPE")),
            ("Content-Length", var("CONTENT_LENGTH")),
        ];
        let method = String::from_utf8_lossy(&var("REQUEST_METHOD")).into_owned();
        Http {
            method: Some(method)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "GET".to_owned()),
            query: var("QUERY_STRING"),
            uri: var("REQUEST_URI"),
            fields: fields.into_iter().filter(|(_, v)| !v.is_empty()).collect(),
            body: described.stdin.clone().unwrap_or_default(),
        }
    }

    /// A `GET` of the program with no query and no body.
    pub(crate) fn get() -> Http {
        Http {
            method: "GET".to_owned(),
            query: Vec::new(),
            uri: Vec::new(),
            fields: vec![("Host", b"localhost".to_vec())],
            body: Vec::new(),
        }
    }

    /// A field's value as sent, empty where it is not.
    pub(crate) fn field(&self, name: &str) -> Vec<u8> {
        let found = self.fields.iter().find(|(had, _)| *had == name);
        found.map_or_else(Vec::new, |(_, value)| value.clone())
    }

    /// The request target: the `REQUEST_URI` the test names or else the
    /// program's file, and the query after a `?` where the target has none.
    pub(crate) fn target(&self, script: &str) -> Vec<u8> {
        let mut target = match self.uri.as_slice() {
            [] => format!("/{script}").into_bytes(),
            uri => uri.to_vec(),
        };
        if !self.query.is_empty() && !target.contains(&b'?') {
            target.push(b'?');
            target.extend_from_slice(&self.query);
        }
        target
    }
}

/// A value as one line, as a header field carries it: each line break of
/// it, `\r\n`, `\r` or `\n`, a space.
fn one_line(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.iter().peekable();
    while let Some(&byte) = bytes.next() {
        if byte == b'\r' && bytes.peek() == Some(&&b'\n') {
            bytes.next();
        }
        out.push(if matches!(byte, b'\r' | b'\n') {
            b' '
        } else {
            byte
        });
    }
    out
}

/// `snippet` served as the request `request` describes by the makala at
/// `host`, with `env` over the recipe's environment.
pub(crate) fn served(
    oracle: &Oracle<'_>,
    host: &Path,
    snippet: &str,
    request: &str,
    env: &[(String, OsString)],
) -> Result<Observation> {
    let (own, program) = process::program(oracle, "witness-makala", snippet)?;
    let http = Http::of(&own, &program, request)?;
    let started = Instant::now();
    let by = by_makala(&oracle.pin().recipe, host, &program, &http, &[], env)?;
    let streams = (from_http(&by.response), php_logged(&by.logged));
    Ok(process::answered(oracle, &own, streams, started))
}

/// What one makala answered for one request: the response as it came, the
/// error log it wrote, the `SERVER_SOFTWARE` it named itself by and the
/// two ports of the connection, each the run's own.
pub(crate) struct Answered {
    pub(crate) response: Vec<u8>,
    pub(crate) logged: String,
    pub(crate) software: String,
    pub(crate) ports: Ports,
}

/// The two ports of one connection: the server's and the client's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Ports {
    pub(crate) server: u16,
    pub(crate) remote: u16,
}

/// The directory a program stands in, which a server serves, and its
/// name, which a request that names no target asks for.
pub(crate) fn placed(program: &Path) -> Result<(&Path, &str)> {
    let dir = program.parent().context("a program with no directory")?;
    let name = (program.file_name().and_then(|name| name.to_str()))
        .context("a program with no name of text")?;
    Ok((dir, name))
}

/// `http` served by a makala of its own at `host` over the directory of
/// `program`, under the recipe's ini and `ini` after it, ended once its
/// response is read.
pub(crate) fn by_makala(
    recipe: &Recipe,
    host: &Path,
    program: &Path,
    http: &Http,
    ini: &[String],
    env: &[(String, OsString)],
) -> Result<Answered> {
    let (dir, script) = placed(program)?;
    let log = dir.join("errors");
    let server = Server::start(recipe, host, dir, ini, Some(1), &log, env)?;
    let timeout = Duration::from_secs(recipe.limits.timeout_s);
    let (response, remote) = server.ask(http, script, timeout)?;
    let (software, server_port) = (server.software.clone(), server.port);
    drop(server);
    Ok(Answered {
        response,
        logged: std::fs::read_to_string(&log).unwrap_or_default(),
        software,
        ports: Ports {
            server: server_port,
            remote,
        },
    })
}

/// A makala serving a document root on a loopback port, ended as a service
/// manager ends it when dropped, so its drain lets a request run to its
/// end, and the trace of the natives it entered with it, before the
/// process ends.
pub(crate) struct Server {
    child: Child,
    addr: String,
    port: u16,
    /// What it names itself by, `makala/<version>` (#2335), as its first
    /// line on stderr says.
    pub(crate) software: String,
}

impl Server {
    /// The makala at `host` over `root` with `threads` request threads,
    /// or as many as it takes by default, its error log at `log`, under the recipe's ini and `ini` after it
    /// and the recipe's environment with `env` over it; started once it
    /// says where it listens.
    pub(crate) fn start(
        recipe: &Recipe,
        host: &Path,
        root: &Path,
        ini: &[String],
        threads: Option<usize>,
        log: &Path,
        env: &[(String, OsString)],
    ) -> Result<Server> {
        let mut server = Command::new(host);
        server.arg("-n");
        process::under_recipe(recipe, &mut server, ini, env)?.stderr(Stdio::piped());
        server.args(["--listen", "127.0.0.1:0"]);
        if let Some(threads) = threads {
            server.arg("--threads").arg(threads.to_string());
        }
        server.arg("--root").arg(root).arg("--error-log").arg(log);
        let mut child = server
            .spawn()
            .with_context(|| format!("running {}", host.display()))?;
        let stderr = child.stderr.take().context("makala's stderr");
        let mut server = Server {
            child,
            addr: String::new(),
            port: 0,
            software: String::new(),
        };
        // What php logs at startup, a deprecated ini entry's warning among
        // it, stands before the line that says where it listens.
        let (mut line, mut before) = (String::new(), String::new());
        let mut stderr = BufReader::new(stderr?);
        while stderr.read_line(&mut line)? > 0 && !line.contains(" listening on http://") {
            before.push_str(&line);
            line.clear();
        }
        let said = line
            .split_once(" listening on http://")
            .and_then(|(software, rest)| Some((software, rest.split_once(',')?.0)));
        let (software, addr) =
            said.ok_or_else(|| anyhow!("makala did not say where it listens: {before:?}"))?;
        server.port = (addr.rsplit(':').next().and_then(|p| p.parse().ok()))
            .ok_or_else(|| anyhow!("makala names no port it listens on: {before}{line:?}"))?;
        software.clone_into(&mut server.software);
        addr.clone_into(&mut server.addr);
        Ok(server)
    }

    /// `http` sent to the program `script` over a connection of its own as
    /// an HTTP/1.0 request, so the response ends at the close; the
    /// response as it came, and the client's port.
    pub(crate) fn ask(
        &self,
        http: &Http,
        script: &str,
        timeout: Duration,
    ) -> Result<(Vec<u8>, u16)> {
        let addr = &self.addr;
        let at: std::net::SocketAddr = addr.parse().with_context(|| format!("reading {addr}"))?;
        let mut stream = TcpStream::connect_timeout(&at, timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut sent = format!("{} ", http.method).into_bytes();
        sent.extend_from_slice(&http.target(script));
        sent.extend_from_slice(b" HTTP/1.0\r\n");
        for (name, value) in &http.fields {
            sent.extend_from_slice(format!("{name}: ").as_bytes());
            sent.extend_from_slice(value);
            sent.extend_from_slice(b"\r\n");
        }
        sent.extend_from_slice(b"\r\n");
        sent.extend_from_slice(&http.body);
        stream.write_all(&sent)?;
        let mut answer = Vec::new();
        stream
            .read_to_end(&mut answer)
            .context("reading makala's response")?;
        Ok((answer, stream.local_addr()?.port()))
    }

    /// The process, for what a soak samples of it.
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        terminate(&mut self.child);
    }
}

/// `snippet` served as the request `request` describes by the php-fpm at
/// `host` under the pin's `[fastcgi]` mapping, as the server would hand it
/// on, and its CGI response read as a client of nginx reads it: what a
/// `request makala` fence's oracle run is.
pub(crate) fn served_by_mapping(
    oracle: &Oracle<'_>,
    host: &Path,
    snippet: &str,
    request: &str,
) -> Result<Observation> {
    let (own, program) = process::program(oracle, "witness-makala-fpm", snippet)?;
    let http = Http::of(&own, &program, request)?;
    let started = Instant::now();
    let (stdout, stderr) = by_fpm(oracle, host, &program, &http, &[], Ports::default())?;
    Ok(process::answered(
        oracle,
        &own,
        (from_cgi(&stdout), stderr),
        started,
    ))
}

/// `http` handed by a pool of its own of the php-fpm at `host` to
/// `program`, under the pin's `[fastcgi]` mapping with `ports` as the
/// connection's and the recipe's ini and `ini` after it; what it wrote on
/// `FCGI_STDOUT` and `FCGI_STDERR`.
pub(crate) fn by_fpm(
    oracle: &Oracle<'_>,
    host: &Path,
    program: &Path,
    http: &Http,
    ini: &[String],
    ports: Ports,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut fpm = Fpm::start(oracle, host, program, http, (ini, ports), 1)?;
    let answered = fpm.ask()?;
    fpm.settle()?;
    Ok(answered)
}

/// A pool of its own of the php-fpm at `host`, kept to hand one request,
/// `http` to `program` under the pin's `[fastcgi]` mapping, as
/// many times as it is asked, by as many clients at once as there are: what
/// [`by_fpm`] asks once and a bench arm asks each rep (#2351, #2660).
/// Ended when dropped.
pub(crate) struct Fpm {
    /// Held only where a client cannot reach `address` at once,
    /// while [`Pool::connect`] waits for the master to listen.
    pool: Mutex<Pool>,
    /// Where the pool listens.
    address: SocketAddr,
    params: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
    timeout: Duration,
    /// The tree the pool's socket stands in, dropped after the pool.
    _sockets: Tree,
}

impl Fpm {
    /// The pool of `children` started, under the recipe's ini and `ini`
    /// after it, with `ports` as each connection's.
    pub(crate) fn start(
        oracle: &Oracle<'_>,
        host: &Path,
        program: &Path,
        http: &Http,
        (ini, ports): (&[String], Ports),
        children: usize,
    ) -> Result<Fpm> {
        let pin = oracle.pin();
        let recipe = &pin.recipe;
        let (dir, script) = placed(program)?;
        let root = dir.canonicalize()?.to_string_lossy().into_owned();
        // The socket in a tree with a short name, since a socket's address
        // holds no longer path than `SUN_LEN`; and php-fpm's default
        // `clear_env`, so `$_SERVER` holds nothing of the process's
        // environment, as makala's holds nothing.
        let sockets = Tree::new("mkf")?;
        let pool = Pool::start(oracle, host, &sockets, &[], ini, (true, children))?;
        Ok(Fpm {
            address: pool.address(),
            pool: Mutex::new(pool),
            params: mapped(pin.fastcgi()?, http, &root, script, ports),
            body: http.body.clone(),
            timeout: Duration::from_secs(recipe.limits.timeout_s),
            _sockets: sockets,
        })
    }

    /// The request handed once more: what it wrote on `FCGI_STDOUT` and
    /// `FCGI_STDERR`.
    pub(crate) fn ask(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        // Once the pool listens its clients reach it at once, without the
        // lock; before, or where it refuses, through `Pool::connect`, which
        // waits for it and says why a master ended.
        let stream = match UnixStream::connect_addr(&self.address) {
            Ok(stream) => stream,
            Err(_) => crate::held(&self.pool).connect()?,
        };
        stream.set_read_timeout(Some(self.timeout))?;
        let pairs = (self.params.iter()).map(|(n, v)| (n.as_bytes(), v.as_slice()));
        round(stream, pairs, Some(&self.body))
    }

    /// The pool's master, whose children answer its requests.
    pub(crate) fn pid(&self) -> u32 {
        crate::held(&self.pool).pid()
    }

    /// Once the request last asked has ended in the pool's child, as well
    /// as its response (`Pool::settle`).
    fn settle(&mut self) -> Result<()> {
        let pool = self.pool.get_mut().unwrap_or_else(PoisonError::into_inner);
        pool.settle(self.timeout)
    }
}

/// The mapping's parameters filled for `http` to the program's file `script`
/// under `root`, the request's path split by the mapping's pattern
/// ([`Mapping::split`]), in the order nginx sends them, and then each header
/// field as `HTTP_<NAME>`, as nginx's `fastcgi_pass_request_headers`
/// passes it. The ports are `ports`, a fence's 0 and no part of what it
/// pins, and a comparison's those makala's connection had.
fn mapped(
    mapping: &Mapping,
    http: &Http,
    root: &str,
    script: &str,
    ports: Ports,
) -> Vec<(String, Vec<u8>)> {
    let target = http.target(script);
    let uri = String::from_utf8_lossy(target.split(|b| *b == b'?').next().unwrap_or(&[]));
    let (name, info) = mapping.split(&uri);
    let fill = |variable: Variable| -> Vec<u8> {
        match variable {
            Variable::QueryString => http.query.clone(),
            Variable::ContentType => http.field("Content-Type"),
            Variable::ContentLength => http.field("Content-Length"),
            Variable::RequestUri => target.clone(),
            Variable::DocumentRoot => root.as_bytes().to_vec(),
            Variable::FastcgiUri => name.as_bytes().to_vec(),
            Variable::PathInfo => info.as_bytes().to_vec(),
            Variable::DocumentUri => uri.as_bytes().to_vec(),
            Variable::RequestMethod => http.method.clone().into_bytes(),
            Variable::ServerProtocol => b"HTTP/1.0".to_vec(),
            Variable::Scheme => b"http".to_vec(),
            Variable::Https => Vec::new(),
            Variable::NginxVersion => mapping.nginx.clone().into_bytes(),
            Variable::RemoteAddr | Variable::ServerAddr => b"127.0.0.1".to_vec(),
            Variable::RemotePort => ports.remote.to_string().into_bytes(),
            Variable::ServerPort => ports.server.to_string().into_bytes(),
            Variable::ServerName => b"localhost".to_vec(),
        }
    };
    let mut params = Vec::new();
    for param in &mapping.params {
        let value: Vec<u8> = (param.value.iter())
            .flat_map(|piece| match piece {
                Piece::Text(text) => text.clone().into_bytes(),
                Piece::Variable(variable) => fill(*variable),
            })
            .collect();
        let sent = match param.sent {
            Sent::Always => true,
            Sent::IfNotEmpty => !value.is_empty(),
            Sent::WithPathInfo => !info.is_empty(),
        };
        if sent {
            params.push((param.name.clone(), value));
        }
    }
    for (name, value) in &http.fields {
        let name = format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"));
        params.push((name, value.clone()));
    }
    params
}

/// makala's HTTP response as a client of nginx reads php-fpm's: the
/// status line's code, the lines PHP set, the server's `date` and
/// `connection` left out, and the body.
pub(crate) fn from_http(answer: &[u8]) -> Vec<u8> {
    let (head, body) = split_head(answer);
    let mut head = head.lines();
    let code = head.next().and_then(|status| status.split(' ').nth(1));
    let lines = head.filter(|line| !line.starts_with("date:") && !line.starts_with("connection:"));
    rendered(code.unwrap_or_default(), lines, body)
}

/// php-fpm's CGI response as nginx hands it on: its `Status:` line the
/// code, 200 without one, then the other lines, then the body.
pub(crate) fn from_cgi(out: &[u8]) -> Vec<u8> {
    let (head, body) = split_head(out);
    let status = head.lines().find_map(|line| line.strip_prefix("Status: "));
    let code = status.and_then(|status| status.split(' ').next());
    let lines = head.lines().filter(|line| !line.starts_with("Status: "));
    rendered(code.unwrap_or("200"), lines, body)
}

/// What php logged, from makala's error log: each entry's time taken off
/// and the server's own entries, `makala: `, left out.
pub(crate) fn php_logged(logged: &str) -> Vec<u8> {
    let entries = logged.lines().map(|line| match line.split_once(' ') {
        Some((time, entry)) if time.len() == 24 && time.ends_with('Z') => entry,
        _ => line,
    });
    let php = entries.filter(|entry| !entry.starts_with("makala: "));
    php.flat_map(|entry| [entry, "\n"])
        .collect::<String>()
        .into_bytes()
}

/// A head and its body, split at the first blank line.
pub(crate) fn split_head(bytes: &[u8]) -> (String, &[u8]) {
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n");
    let (head, body) = end.map_or((bytes, &[][..]), |end| (&bytes[..end], &bytes[end + 4..]));
    (String::from_utf8_lossy(head).into_owned(), body)
}

/// The code, each header line with its name lowercased, and the body
/// after a blank line.
fn rendered<'a>(code: &str, lines: impl Iterator<Item = &'a str>, body: &[u8]) -> Vec<u8> {
    let mut out = format!("{code}\n");
    for (name, value) in lines.filter_map(|line| line.split_once(':')) {
        out.push_str(&name.to_ascii_lowercase());
        out.push_str(": ");
        out.push_str(value.trim());
        out.push('\n');
    }
    out.push('\n');
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use std::fs;

    use super::*;
    use crate::oracle::Pin;

    /// Every parameter of the pinned mapping is filled for a request, in
    /// the order nginx sends them, `HTTPS` left out on a plain connection,
    /// and the header fields after them as `HTTP_` ones; the path past the
    /// program is `PATH_INFO`, empty where there is none (#2637), and
    /// `PATH_TRANSLATED` the root and it, none where there is none (#2650).
    #[test]
    fn the_pinned_mapping_is_filled_in_nginx_s_order_with_the_fields_after() {
        let workspace = crate::package_dir();
        let text = fs::read_to_string(workspace.join("../oracle.toml")).unwrap();
        let pin = Pin::parse(&text).unwrap();
        let http = Http {
            method: "POST".to_owned(),
            query: b"a=1".to_vec(),
            uri: Vec::new(),
            fields: vec![
                ("Host", b"localhost".to_vec()),
                ("Content-Length", b"2".to_vec()),
            ],
            body: b"hi".to_vec(),
        };
        let params = mapped(
            pin.fastcgi().unwrap(),
            &http,
            "/root",
            "t.php",
            Ports::default(),
        );
        let names: Vec<&str> = params.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names[..2], ["PATH_INFO", "SCRIPT_FILENAME"]);
        assert!(!names.contains(&"HTTPS"));
        assert_eq!(
            names[names.len() - 2..],
            ["HTTP_HOST", "HTTP_CONTENT_LENGTH"]
        );
        let value = |name: &str| {
            params
                .iter()
                .find(|(had, _)| had == name)
                .and_then(|(_, v)| std::str::from_utf8(v).ok())
        };
        assert_eq!(value("SCRIPT_FILENAME"), Some("/root/t.php"));
        assert_eq!(value("REQUEST_URI"), Some("/t.php?a=1"));
        assert_eq!(value("SERVER_SOFTWARE"), Some("nginx/1.30.5"));
        assert_eq!(value("CONTENT_LENGTH"), Some("2"));
        assert_eq!(value("CONTENT_TYPE"), Some(""));
        assert_eq!(value("PATH_INFO"), Some(""));
        let past = Http {
            uri: b"/t.php/x/y".to_vec(),
            ..http
        };
        let params = mapped(
            pin.fastcgi().unwrap(),
            &past,
            "/root",
            "t.php",
            Ports::default(),
        );
        let value = |name: &str| {
            let found = params.iter().find(|(had, _)| had == name);
            found.and_then(|(_, v)| std::str::from_utf8(v).ok())
        };
        assert_eq!(value("SCRIPT_NAME"), Some("/t.php"));
        assert_eq!(value("SCRIPT_FILENAME"), Some("/root/t.php"));
        assert_eq!(value("PATH_INFO"), Some("/x/y"));
        assert_eq!(value("PATH_TRANSLATED"), Some("/root/x/y"));
        assert_eq!(value("DOCUMENT_URI"), Some("/t.php/x/y"));
    }

    /// A test's `REQUEST_URI` is the target, its query after it where the
    /// target carries none, its `HTTP_ACCEPT_ENCODING` a field, and a
    /// cookie's bytes are sent as written.
    #[test]
    fn a_test_s_request_uri_is_the_target_and_the_encodings_it_accepts_a_field() {
        use std::os::unix::ffi::OsStringExt;
        let mut env = crate::phpt::environment::Environment::default();
        env.set("REQUEST_URI", "/f.php/a.jpg");
        env.set("QUERY_STRING", "x=1");
        env.set("HTTP_ACCEPT_ENCODING", "gzip");
        env.set(
            "HTTP_COOKIE",
            OsString::from_vec(b"a=\xc6\xfc\r\nb=1".to_vec()),
        );
        let http = Http::described(&cgi::Request {
            env,
            stdin: None,
            args: Vec::new(),
        });
        assert_eq!(http.target("f.php"), b"/f.php/a.jpg?x=1");
        assert_eq!(http.field("Accept-Encoding"), b"gzip");
        assert_eq!(
            http.field("Cookie"),
            b"a=\xc6\xfc b=1",
            "bytes as written, on one line"
        );
        assert_eq!(Http::get().target("t.php"), b"/t.php");
    }

    /// php-fpm's CGI response and makala's HTTP one read alike: the code,
    /// then each line PHP set with its name lowercased, then the body; and
    /// php's entries are read off makala's error log without their time.
    #[test]
    fn both_responses_read_as_a_client_of_nginx_reads_them() {
        let cgi = from_cgi(
            b"Status: 404 Not Found\r\nX-Powered-By: PHP\r\nContent-type: text/html\r\n\r\nbody",
        );
        let http = from_http(
            b"HTTP/1.0 404 Not Found\r\nx-powered-by: PHP\r\ndate: x\r\ncontent-type: text/html\r\nconnection: close\r\n\r\nbody",
        );
        let shown = "404\nx-powered-by: PHP\ncontent-type: text/html\n\nbody";
        assert_eq!(String::from_utf8_lossy(&cgi), shown);
        assert_eq!(String::from_utf8_lossy(&http), shown);
        let logged = "2026-10-10T08:22:40.123Z makala: drained\n2026-10-10T08:22:40.124Z x\n  y\n";
        assert_eq!(php_logged(logged), b"x\n  y\n");
    }
}
