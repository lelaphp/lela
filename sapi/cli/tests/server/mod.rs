//! A `php -S` process for a test: started over a document root of its
//! own on a free `port`, its log collected line by line as it is written,
//! and stopped by `SIGINT`, as php's own server tests drive it.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One running server: the process, its address, its document root and
/// the lines of its log so far.
pub struct Server {
    child: Child,
    pub address: String,
    pub root: PathBuf,
    log: Arc<Mutex<Vec<String>>>,
}

/// How many `port` numbers a server is offered: one found free can be taken by
/// another process before php binds it, which php then says and exits.
/// php -S cannot be handed a socket already bound.
const ATTEMPTS: usize = 3;

/// What php -S says, and exits on, where another process holds its `port`.
const IN_USE: &str = "(reason: Address already in use)";

/// A loopback `port` free when asked: bound and let go again, so it is
/// free only until another process binds it.
pub fn free_loopback() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|probe| probe.local_addr())
        .map_or_else(|e| panic!("finding a free port: {e}"), |bound| bound.port())
}

impl Server {
    /// `php -n -S 127.0.0.1:<free port> -t <root>` with `files` written
    /// under a root named for `name`, and `env` set, once its first
    /// "started" line is written.
    pub fn start(name: &str, files: &[(&str, &str)], env: &[(&str, &str)]) -> Server {
        Server::start_on(std::iter::repeat_with(free_loopback), name, files, env, &[])
    }

    /// [`Server::start`] with `args` before `-S`, on the first of `ports`
    /// php binds: a `port` php says is in use is given up for the next,
    /// [`ATTEMPTS`] at most.
    pub fn start_on(
        ports: impl IntoIterator<Item = u16>,
        name: &str,
        files: &[(&str, &str)],
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Server {
        let root = std::env::temp_dir().join(format!("php-server-{name}-{}", std::process::id()));
        let mut refused = Vec::new();
        for port in ports.into_iter().take(ATTEMPTS) {
            std::fs::remove_dir_all(&root).ok();
            std::fs::create_dir_all(&root)
                .unwrap_or_else(|e| panic!("making {}: {e}", root.display()));
            for (file, text) in files {
                std::fs::write(root.join(file), text)
                    .unwrap_or_else(|e| panic!("writing {file}: {e}"));
            }
            let mut child = Command::new(env!("CARGO_BIN_EXE_lela"))
                .arg("-n")
                .args(args)
                .arg("-S")
                .arg(format!("127.0.0.1:{port}"))
                .arg("-t")
                .arg(&root)
                .envs(env.iter().copied())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("running php -S: {e}"));
            let stderr = child.stderr.take().unwrap_or_else(|| panic!("no stderr"));
            let log = Arc::new(Mutex::new(Vec::new()));
            let kept = Arc::clone(&log);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    kept.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(line);
                }
            });
            let server = Server {
                child,
                address: format!("127.0.0.1:{port}"),
                root: root.clone(),
                log,
            };
            server.wait_for(Duration::from_secs(30), |lines| {
                lines
                    .iter()
                    .any(|line| line.ends_with("started") || line.ends_with(IN_USE))
            });
            let log = server.log();
            if log.iter().any(|line| line.ends_with("started")) {
                return server;
            }
            assert!(
                log.iter().any(|line| line.ends_with(IN_USE)),
                "the server never said it started: {log:?}"
            );
            // Dropped, the child is killed and reaped and the root taken
            // away, to be laid again for the next port.
            refused.extend(log);
        }
        panic!("php -S found each port it was offered in use: {refused:?}")
    }

    /// The log so far.
    pub fn log(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether `done` holds of the log within `within`.
    pub fn wait_for(&self, within: Duration, done: impl Fn(&[String]) -> bool) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if done(&self.log()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done(&self.log())
    }

    /// A connection to the server.
    pub fn connect(&self) -> TcpStream {
        let stream =
            TcpStream::connect(&self.address).unwrap_or_else(|e| panic!("connecting: {e}"));
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        stream
    }

    /// `GET <path>` over a connection of its own, and the response's body.
    pub fn get(&self, path: &str) -> String {
        body(&mut self.send(path))
    }

    /// `GET <path>` sent whole over a connection of its own, its response
    /// not yet read.
    pub fn send(&self, path: &str) -> TcpStream {
        let mut stream = self.connect();
        stream
            .write_all(format!("GET {path} HTTP/1.0\r\nHost: x\r\n\r\n").as_bytes())
            .ok();
        stream
    }

    /// The process's id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `kill -INT` to `pid`.
    pub fn interrupt(pid: u32) {
        let killed = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status();
        assert!(killed.is_ok_and(|status| status.success()));
    }

    /// `SIGINT` to the server, and the status it exits with within ten
    /// seconds; `None` where it was still running, which is then killed.
    pub fn stop(mut self) -> Option<i32> {
        Server::interrupt(self.child.id());
        self.exit_status(Duration::from_secs(10))
    }

    /// The status the server exits with within `within`; `None` where it
    /// was still running, which is then killed.
    pub fn exit_status(&mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            if Instant::now() > deadline {
                self.child.kill().ok();
                self.child.wait().ok();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_dir_all(&self.root).ok();
    }
}

/// The body of the response read to its end from `stream`.
pub fn body(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok();
    let text = String::from_utf8_lossy(&response).into_owned();
    match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_owned(),
        None => text,
    }
}
