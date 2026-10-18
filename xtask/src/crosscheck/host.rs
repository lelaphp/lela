//! The host a module is rendered through (#1496). php registers most
//! modules in every host, and the lane renders those through the CLI's
//! `--re` and `--ri`; a host's own module exists only in that host:
//! `cgi-fcgi` in php-cgi and php-fpm (`sapi/cgi/cgi_main.c`,
//! `sapi/fpm/fpm/fpm_main.c` at the tag), `cli_server` in the CLI only
//! while it serves as the built-in server (`sapi/cli/php_cli_server.c`).
//! php-cgi and php-fpm have no `--re` flag, and the CLI's `--re
//! cli_server` runs no server, so the oracle's CLI renders neither. Such
//! a module is rendered on both sides by its host serving the column's
//! program as a request, as the witness lane serves a `request` fence:
//! the oracle's host from `[sapi]` beside the engine's witness build, the
//! response's head let go and its body held.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::Column;
use crate::oracle::{self, Ended, Observation, Oracle, normalise_in};
use crate::scratch::Tree;

/// What serves a module's rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    /// The CLI's `php --re` and `php --ri`: every module but a host's own.
    Cli,
    /// php-cgi, serving the program as a request of the web.
    Cgi,
    /// php-fpm, over a pool of one child, as a `request fpm` fence.
    Fpm,
    /// The CLI's built-in server, `php -S`, serving one request.
    Server,
}

/// The module php-cgi and php-fpm each register as their own.
const CGI_FCGI: &str = "cgi-fcgi";
/// The module the built-in server registers.
const CLI_SERVER: &str = "cli_server";

/// The request a CGI-shaped host is handed: a `GET` with nothing in it,
/// in the `.phpt` sections a `request` fence carries.
const REQUEST: &str = "--CGI--\n";

/// The hosts that render `module`, one reading pair each: both of
/// `cgi-fcgi`'s, the server for `cli_server`, the CLI for every other.
pub(crate) fn hosts(module: &str) -> &'static [Host] {
    match module {
        CGI_FCGI => &[Host::Cgi, Host::Fpm],
        CLI_SERVER => &[Host::Server],
        _ => &[Host::Cli],
    }
}

impl Host {
    /// The manifest row of the host whose sources the engine's side is:
    /// none for the CLI, whose reading stands over the extension's crate.
    pub(crate) fn krate(self) -> Option<&'static str> {
        match self {
            Host::Cli => None,
            Host::Cgi => Some("sapi/cgi"),
            Host::Fpm => Some("sapi/fpm"),
            Host::Server => Some("sapi/cli"),
        }
    }

    /// The reading's scope: [`Column::scope`] for the CLI, and the host's
    /// row with the module under it for a host's own module,
    /// `sapi/cgi/cgi-fcgi` and `sapi/cgi/cgi-fcgi/ri`.
    pub(crate) fn scope(self, module: &str, column: Column) -> String {
        match self.krate() {
            None => column.scope(module),
            Some(krate) => match column {
                Column::Re => format!("{krate}/{module}"),
                Column::Ri => format!("{krate}/{module}/ri"),
            },
        }
    }

    /// The divergence key of the reading [`Host::scope`] names:
    /// `crosscheck:ctype`, `crosscheck:ctype/ri`,
    /// `crosscheck:sapi/cgi/cgi-fcgi`.
    pub(crate) fn id(self, module: &str, column: Column) -> String {
        super::id_of(&self.scope(module, column))
    }

    /// The invocation as a reading names it.
    pub(crate) fn invocation(self) -> &'static str {
        match self {
            Host::Cli => "php",
            Host::Cgi => "php-cgi",
            Host::Fpm => "php-fpm",
            Host::Server => "php -S",
        }
    }
}

/// The oracle's rendering of `module` through `host`, which is not the
/// CLI: the pinned host from `[sapi]`, its digest asserted, or the pinned
/// `php` for the server.
pub(crate) fn oracle_side(
    oracle: &Oracle<'_>,
    host: Host,
    module: &str,
    column: Column,
) -> Result<Observation> {
    let program = column.program(module);
    let sapi = &oracle.pin().sapi;
    let pinned = |pinned: Option<&oracle::Host>, name: &str| {
        let pinned = pinned
            .ok_or_else(|| anyhow!("oracle.toml's [sapi] names no {name} to serve the module"))?;
        oracle.host_asserted(pinned)?;
        Ok::<_, anyhow::Error>(pinned.path.clone())
    };
    let served = match host {
        Host::Cli => bail!("the CLI renders {module} by its flags, not as a request"),
        Host::Cgi => {
            let path = pinned(sapi.cgi.as_ref(), "php-cgi")?;
            crate::witness::served_as_cgi(oracle, &path, &program, REQUEST, None, &[])?
        }
        Host::Fpm => {
            let path = pinned(sapi.fpm.as_ref(), "php-fpm")?;
            crate::witness::served_as_fpm(oracle, &path, &program, REQUEST, &[])?
        }
        Host::Server => served_by_server(oracle, &oracle.pin().oracle.path, &program)?,
    };
    Ok(body(oracle, served))
}

/// The engine's rendering through `host`: its witness build serving the
/// same request.
pub(crate) fn engine_side(
    oracle: &Oracle<'_>,
    binary: &crate::witness::Binary,
    host: Host,
    module: &str,
    column: Column,
) -> Result<Observation> {
    let program = column.program(module);
    let served = match host {
        Host::Cli => bail!("the CLI renders {module} by its flags, not as a request"),
        Host::Cgi => binary.serve_cgi(oracle, &program, REQUEST, None)?.0,
        Host::Fpm => binary.serve_fpm(oracle, &program, REQUEST)?.0,
        Host::Server => served_by_server(oracle, binary.cli()?, &program)?,
    };
    Ok(body(oracle, served))
}

/// The response's body: a CGI-shaped host writes its head first, and the
/// head is the host's and not the module's, so it is let go where every
/// line before the first empty one is a header.
fn body(oracle: &Oracle<'_>, mut served: Observation) -> Observation {
    served.stdout = without_head(&oracle.pin().recipe.normalise, served.stdout);
    served
}

/// `bytes`, normalised, less a head of `Name: value` lines and the empty
/// line after it; untouched where no such head opens it.
pub(crate) fn without_head(rules: &oracle::Normalise, bytes: Vec<u8>) -> Vec<u8> {
    let text = oracle::normalise(rules, bytes);
    let Some(end) = text.windows(2).position(|pair| pair == b"\n\n") else {
        return text;
    };
    let headed = text[..end].split(|b| *b == b'\n').all(|line| {
        line.iter().position(|b| *b == b':').is_some_and(|colon| {
            colon > 0
                && line[..colon]
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        })
    });
    if headed {
        oracle::normalise(rules, text[end + 2..].to_vec())
    } else {
        text
    }
}

/// How many ports the server is offered before the run gives up: a port
/// found free can be taken before the server binds it, and the server
/// then says so and exits.
const ATTEMPTS: usize = 3;

/// `snippet` served by the built-in server of the CLI at `php`: the
/// program written as the recipe's `script_name` in a tree of its own,
/// with the recipe's preamble; the server started on a loopback port
/// under the recipe's arguments, ini and environment with that tree as
/// its root; and one `GET` of the program once the server has said, on
/// its own second stream, that it listens there, so the response is this
/// server's and no other process's. The HTTP status stands for the exit:
/// 200 is 0, and any other is itself.
pub(crate) fn served_by_server(
    oracle: &Oracle<'_>,
    php: &Path,
    snippet: &str,
) -> Result<Observation> {
    let recipe = &oracle.pin().recipe;
    let own = Tree::new("crosscheck-server")?;
    let program = own.join(&recipe.script_name);
    crate::write(&program, format!("{}{snippet}", recipe.preamble))?;
    let timeout = Duration::from_secs(recipe.limits.timeout_s);
    let started = Instant::now();
    let mut refused = Vec::new();
    for _ in 0..ATTEMPTS {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| listener.local_addr())
            .context("finding a free loopback port")?
            .port();
        let mut server = Command::new(php);
        server.args(&recipe.args);
        for ini in &recipe.ini {
            server.arg("-d").arg(ini);
        }
        server
            .arg("-S")
            .arg(format!("127.0.0.1:{port}"))
            .arg("-t")
            .arg(own.path());
        recipe
            .environ(&mut server)?
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut server = Server(
            server
                .spawn()
                .with_context(|| format!("running {}", php.display()))?,
        );
        if let Err(said) = server.listening(port, timeout)? {
            refused.push(said);
            continue;
        }
        let (status, body) = get(port, &recipe.script_name, timeout)?;
        return Ok(Observation {
            stdout: normalise_in(&recipe.normalise, own.path(), body),
            stderr: Vec::new(),
            exit: Ended::Status(if status == 200 { 0 } else { i32::from(status) }),
            wall: started.elapsed(),
        });
    }
    bail!(
        "{} listened on none of {ATTEMPTS} ports: {}",
        php.display(),
        refused.join("; ")
    )
}

/// A server that is killed and reaped when it goes out of scope.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

impl Server {
    /// Whether the server says it listens on `port`: `Ok(Ok(()))` once its
    /// `Development Server (http://127.0.0.1:<port>) started` line is on
    /// its second stream, `Ok(Err(said))` where it ended first, with what
    /// it said.
    fn listening(&mut self, port: u16, timeout: Duration) -> Result<Result<(), String>> {
        let stderr = self
            .0
            .stderr
            .take()
            .ok_or_else(|| anyhow!("the server's second stream is not piped"))?;
        let (lines, said) = mpsc::channel();
        // The stream is read to its end, past the line waited for: a
        // server whose log has no reader dies of `SIGPIPE` at the next
        // request it logs.
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                lines.send(line).ok();
            }
        });
        let started = format!("(http://127.0.0.1:{port}) started");
        let deadline = Instant::now() + timeout;
        let mut heard = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match said.recv_timeout(left) {
                Ok(line) if line.contains(&started) => return Ok(Ok(())),
                Ok(line) => heard.push(line),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(Err(heard.join(" "))),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("the server did not listen on {port} within {timeout:?}")
                }
            }
        }
    }
}

/// `GET /<script>` over HTTP/1.0 to the server on `port`, and the
/// response's status and body.
fn get(port: u16, script: &str, timeout: Duration) -> Result<(u16, Vec<u8>)> {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).with_context(|| format!("connecting to {port}"))?;
    stream.set_read_timeout(Some(timeout))?;
    // One write: a request that reaches php's server in small pieces can
    // be refused as `Malformed HTTP request`, and the connection closed
    // with nothing written.
    let request = format!("GET /{script} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .context("writing the request")?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("reading the server's response")?;
    let end = response
        .windows(4)
        .position(|four| four == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("the server's response has no end of its head"))?;
    let status = response
        .split(|b| *b == b' ')
        .nth(1)
        .and_then(|code| std::str::from_utf8(code).ok())
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| anyhow!("the server's response has no status"))?;
    Ok((status, response[end + 4..].to_vec()))
}
