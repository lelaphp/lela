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
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use super::fpm::{Pool, round, terminate};
use crate::oracle::{Ended, Mapping, Observation, Oracle, Piece, Recipe, Variable, normalise_in};
use crate::phpt::cgi;
use crate::scratch::Tree;

/// The request a fence describes, as a client sends it.
#[derive(Debug, Clone)]
pub(crate) struct Http {
    method: String,
    query: String,
    /// The header fields in the order they are sent.
    fields: Vec<(&'static str, String)>,
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
    /// cookie, content type and length, and body. A value is one line, as
    /// a header field carries it, so a section's line breaks, which an
    /// environment variable holds and a field may not, are each a space.
    pub(crate) fn described(described: &cgi::Request) -> Http {
        let var = |name: &str| {
            let entries = described.env.entries().iter().rev();
            let mut found = entries.filter(|(had, _)| had == name);
            found.next().map_or_else(String::new, |(_, value)| {
                value
                    .to_string_lossy()
                    .replace("\r\n", " ")
                    .replace(['\r', '\n'], " ")
            })
        };
        let fields = [
            ("Host", "localhost".to_owned()),
            ("Cookie", var("HTTP_COOKIE")),
            ("Content-Type", var("CONTENT_TYPE")),
            ("Content-Length", var("CONTENT_LENGTH")),
        ];
        let method = Some(var("REQUEST_METHOD")).filter(|m| !m.is_empty());
        Http {
            method: method.unwrap_or_else(|| "GET".to_owned()),
            query: var("QUERY_STRING"),
            fields: fields.into_iter().filter(|(_, v)| !v.is_empty()).collect(),
            body: described.stdin.clone().unwrap_or_default(),
        }
    }

    /// A field's value as sent, empty where it is not.
    fn field(&self, name: &str) -> String {
        let found = self.fields.iter().find(|(had, _)| *had == name);
        found.map_or_else(String::new, |(_, value)| value.clone())
    }

    /// The request target: the program's file, and the query after a `?`.
    fn target(&self, script: &str) -> String {
        match self.query.as_str() {
            "" => format!("/{script}"),
            query => format!("/{script}?{query}"),
        }
    }
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
    let recipe = &oracle.pin().recipe;
    let own = oracle.tree("witness-makala")?;
    let program = own.write(&recipe.script_name, format!("{}{snippet}", recipe.preamble))?;
    let http = Http::of(&own, &program, request)?;
    let started = Instant::now();
    let answered = by_makala(recipe, host, own.path(), &http, &[], env)?;
    Ok(Observation {
        stdout: normalise_in(&recipe.normalise, own.path(), from_http(&answered.response)),
        stderr: normalise_in(&recipe.normalise, own.path(), php_logged(&answered.logged)),
        exit: Ended::Status(0),
        wall: started.elapsed(),
    })
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

/// `http` served by a makala of its own at `host` over the program in
/// `dir` named by the recipe's `script_name`, under the recipe's ini and
/// `ini` after it, ended once its response is read.
pub(crate) fn by_makala(
    recipe: &Recipe,
    host: &Path,
    dir: &Path,
    http: &Http,
    ini: &[String],
    env: &[(String, OsString)],
) -> Result<Answered> {
    let log = dir.join("errors");
    let server = Server::start(recipe, host, dir, ini, Some(1), &log, env)?;
    let timeout = Duration::from_secs(recipe.limits.timeout_s);
    let (response, remote) = server.ask(http, &recipe.script_name, timeout)?;
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
        for ini in recipe.ini.iter().chain(ini) {
            server.arg("-d").arg(ini);
        }
        server.args(["--listen", "127.0.0.1:0"]);
        if let Some(threads) = threads {
            server.arg("--threads").arg(threads.to_string());
        }
        server.arg("--root").arg(root).arg("--error-log").arg(log);
        recipe
            .environ(&mut server)?
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
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
        let mut line = String::new();
        BufReader::new(stderr?).read_line(&mut line)?;
        let said = line
            .split_once(" listening on http://")
            .and_then(|(software, rest)| Some((software, rest.split_once(',')?.0)));
        let (software, addr) =
            said.ok_or_else(|| anyhow!("makala did not say where it listens: {line:?}"))?;
        server.port = (addr.rsplit(':').next().and_then(|p| p.parse().ok()))
            .ok_or_else(|| anyhow!("makala names no port it listens on: {line:?}"))?;
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
        let fields: String = (http.fields.iter())
            .flat_map(|(name, value)| [*name, ": ", value, "\r\n"])
            .collect();
        let target = http.target(script);
        write!(stream, "{} {target} HTTP/1.0\r\n{fields}\r\n", http.method)?;
        stream.write_all(&http.body)?;
        let mut answer = Vec::new();
        stream
            .read_to_end(&mut answer)
            .context("reading makala's response")?;
        Ok((answer, stream.local_addr()?.port()))
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
    let recipe = &oracle.pin().recipe;
    let own = oracle.tree("witness-makala-fpm")?;
    let program = own.write(&recipe.script_name, format!("{}{snippet}", recipe.preamble))?;
    let http = Http::of(&own, &program, request)?;
    let started = Instant::now();
    let (stdout, stderr) = by_fpm(oracle, host, own.path(), &http, &[], Ports::default())?;
    Ok(Observation {
        stdout: normalise_in(&recipe.normalise, own.path(), from_cgi(&stdout)),
        stderr: normalise_in(&recipe.normalise, own.path(), stderr),
        exit: Ended::Status(0),
        wall: started.elapsed(),
    })
}

/// `http` handed by a pool of its own of the php-fpm at `host` the program
/// in `dir`, under the pin's `[fastcgi]` mapping with `ports` as the
/// connection's and the recipe's ini and `ini` after it; what it wrote on
/// `FCGI_STDOUT` and `FCGI_STDERR`.
pub(crate) fn by_fpm(
    oracle: &Oracle<'_>,
    host: &Path,
    dir: &Path,
    http: &Http,
    ini: &[String],
    ports: Ports,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let pin = oracle.pin();
    let recipe = &pin.recipe;
    let root = dir.canonicalize()?.to_string_lossy().into_owned();
    let params = mapped(pin.fastcgi()?, http, &root, &recipe.script_name, ports);
    // The socket in a tree with a short name, since a socket's address
    // holds no longer path than `SUN_LEN`; and php-fpm's default
    // `clear_env`, so `$_SERVER` holds nothing of the process's
    // environment, as makala's holds nothing.
    let sockets = Tree::new("mkf")?;
    let mut pool = Pool::start(oracle, host, &sockets, &[], ini, true)?;
    let timeout = Duration::from_secs(recipe.limits.timeout_s);
    let stream = pool.connect()?;
    stream.set_read_timeout(Some(timeout))?;
    let pairs = params.iter().map(|(n, v)| (n.as_bytes(), v.as_bytes()));
    let answered = round(stream, pairs, Some(&http.body))?;
    pool.settle(timeout)?;
    Ok(answered)
}

/// The mapping's parameters filled for `http` to the program's file `script`
/// under `root`, in the order nginx sends them, and then each header
/// field as `HTTP_<NAME>`, as nginx's `fastcgi_pass_request_headers`
/// passes it. The ports are `ports`, a fence's 0 and no part of what it
/// pins, and a comparison's those makala's connection had.
fn mapped(
    mapping: &Mapping,
    http: &Http,
    root: &str,
    script: &str,
    ports: Ports,
) -> Vec<(String, String)> {
    let fill = |variable: Variable| -> String {
        match variable {
            Variable::DocumentRoot => root.to_owned(),
            Variable::FastcgiUri | Variable::DocumentUri => format!("/{script}"),
            Variable::QueryString => http.query.clone(),
            Variable::RequestMethod => http.method.clone(),
            Variable::ContentType => http.field("Content-Type"),
            Variable::ContentLength => http.field("Content-Length"),
            Variable::RequestUri => http.target(script),
            Variable::ServerProtocol => "HTTP/1.0".to_owned(),
            Variable::Scheme => "http".to_owned(),
            Variable::Https => String::new(),
            Variable::NginxVersion => mapping.nginx.clone(),
            Variable::RemoteAddr | Variable::ServerAddr => "127.0.0.1".to_owned(),
            Variable::RemotePort => ports.remote.to_string(),
            Variable::ServerPort => ports.server.to_string(),
            Variable::ServerName => "localhost".to_owned(),
        }
    };
    let mut params = Vec::new();
    for param in &mapping.params {
        let value: String = (param.value.iter())
            .map(|piece| match piece {
                Piece::Text(text) => text.clone(),
                Piece::Variable(variable) => fill(*variable),
            })
            .collect();
        if !(param.if_not_empty && value.is_empty()) {
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
fn split_head(bytes: &[u8]) -> (String, &[u8]) {
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
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::oracle::Pin;

    /// Every parameter of the pinned mapping is filled for a request, in
    /// the order nginx sends them, `HTTPS` left out on a plain connection,
    /// and the header fields after them as `HTTP_` ones.
    #[test]
    fn the_pinned_mapping_is_filled_in_nginx_s_order_with_the_fields_after() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = fs::read_to_string(workspace.join("../oracle.toml")).unwrap();
        let pin = Pin::parse(&text).unwrap();
        let http = Http {
            method: "POST".to_owned(),
            query: "a=1".to_owned(),
            fields: vec![
                ("Host", "localhost".to_owned()),
                ("Content-Length", "2".to_owned()),
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
        assert_eq!(names.first(), Some(&"SCRIPT_FILENAME"));
        assert!(!names.contains(&"HTTPS"));
        assert_eq!(
            names[names.len() - 2..],
            ["HTTP_HOST", "HTTP_CONTENT_LENGTH"]
        );
        let value = |name: &str| {
            params
                .iter()
                .find(|(had, _)| had == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("SCRIPT_FILENAME"), Some("/root/t.php"));
        assert_eq!(value("REQUEST_URI"), Some("/t.php?a=1"));
        assert_eq!(value("SERVER_SOFTWARE"), Some("nginx/1.30.5"));
        assert_eq!(value("CONTENT_LENGTH"), Some("2"));
        assert_eq!(value("CONTENT_TYPE"), Some(""));
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
