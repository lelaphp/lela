//! The CLI's built-in server, `php -S`, as the harness drives it: started
//! on a free loopback port, held until it says on its second stream that
//! it listens there, so a response is this server's and no other
//! process's, asked over HTTP/1.0, and its log read line by line after.
//! `crosscheck` renders `cli_server` through it (#1496), and
//! `dispatch-census` serves the bench pages through it (#4367).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::witness;

/// What the server's log says as a connection it served ends, after the
/// request's own lines: `[<date>] 127.0.0.1:<port> Closing`.
const CLOSING: &str = " Closing";

/// A server that is killed and reaped when it goes out of scope, with its
/// log's lines as they come.
pub(crate) struct Server {
    child: Child,
    port: u16,
    lines: mpsc::Receiver<String>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

impl Server {
    /// `command`, the CLI with what it runs under, made anew for each
    /// port it is offered and started as the built-in
    /// server with `root` its document root on a free loopback port, once
    /// it says it listens there: a port found free can be taken before
    /// the server binds it, so it is offered [`witness::ATTEMPTS`] ports,
    /// and refused naming what it said on each.
    pub(crate) fn start(
        command: impl Fn() -> Result<Command>,
        root: &Path,
        timeout: Duration,
    ) -> Result<Server> {
        let mut refused = Vec::new();
        for _ in 0..witness::ATTEMPTS {
            let port = witness::free_port()?;
            let mut server = command()?;
            let program = PathBuf::from(server.get_program());
            server
                .arg("-S")
                .arg(format!("127.0.0.1:{port}"))
                .arg("-t")
                .arg(root)
                .stdin(Stdio::null())
                .stderr(Stdio::piped());
            let mut child = server
                .spawn()
                .with_context(|| format!("running {}", program.display()))?;
            let stderr = child.stderr.take();
            let (sent, lines) = mpsc::channel();
            let mut server = Server { child, port, lines };
            let stderr =
                stderr.ok_or_else(|| anyhow!("the server's second stream is not piped"))?;
            // The stream is read to its end, past the line waited for: a
            // server whose log has no reader dies of `SIGPIPE` at the next
            // request it logs.
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { return };
                    sent.send(line).ok();
                }
            });
            match server.listening(timeout)? {
                Ok(()) => return Ok(server),
                Err(said) => refused.push(said),
            }
        }
        bail!(
            "the server listened on none of {} ports: {}",
            witness::ATTEMPTS,
            refused.join("; ")
        )
    }

    /// Whether the server says it listens on its port: `Ok(Ok(()))` once
    /// its `Development Server (http://127.0.0.1:<port>) started` line is
    /// in its log, `Ok(Err(said))` where it ended first, with what it said.
    fn listening(&mut self, timeout: Duration) -> Result<Result<(), String>> {
        let started = format!("(http://127.0.0.1:{}) started", self.port);
        let deadline = Instant::now() + timeout;
        let mut heard = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(&started) => return Ok(Ok(())),
                Ok(line) => heard.push(line),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(Err(heard.join(" "))),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!(
                        "the server did not listen on {} within {timeout:?}",
                        self.port
                    )
                }
            }
        }
    }

    /// `GET <target>` over HTTP/1.0, so the response ends at the close,
    /// and the response's status and body.
    pub(crate) fn get(&self, target: &str, timeout: Duration) -> Result<(u16, Vec<u8>)> {
        self.get_as(&format!("127.0.0.1:{}", self.port), target, timeout)
    }

    /// [`Server::get`] with `host` the request's `Host`, for a site that
    /// answers only to its own address, wherever it is served from.
    pub(crate) fn get_as(
        &self,
        host: &str,
        target: &str,
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>)> {
        let port = self.port;
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .with_context(|| format!("connecting to {port}"))?;
        stream.set_read_timeout(Some(timeout))?;
        // One write: a request that reaches php's server in small pieces can
        // be refused as `Malformed HTTP request`, and the connection closed
        // with nothing written.
        let request = format!("GET {target} HTTP/1.0\r\nHost: {host}\r\n\r\n");
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

    /// The log's lines since the last read, through the one that says the
    /// connection served ends: what one request of [`Server::get`] logged,
    /// read once its response has come, its own lines among them.
    pub(crate) fn logged(&self, timeout: Duration) -> Result<Vec<String>> {
        let deadline = Instant::now() + timeout;
        let mut logged = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.ends_with(CLOSING) => {
                    logged.push(line);
                    return Ok(logged);
                }
                Ok(line) => logged.push(line),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("the server ended before it logged the connection's end")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("the server logged no connection's end within {timeout:?}")
                }
            }
        }
    }
}
