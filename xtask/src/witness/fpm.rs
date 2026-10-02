//! A fence with a `request fpm` (#1291): the snippet served as a request
//! of the web by a php-fpm, php-fpm's pin in `[sapi]` for the oracle and
//! `sapi/fpm`'s `php-fpm-witness` for the engine. The master runs in the
//! foreground over a pool of one child listening on a Unix socket in the
//! fence's own scratch tree, under the recipe's ini as `-d`; the request the fence describes goes to it as
//! `FastCGI` records, its variables the ones [`cgi::request`] makes of a
//! CGI-shaped test with the web server's own beside them, and what comes
//! back on `FCGI_STDOUT` and `FCGI_STDERR` is the two streams, held as a
//! php-cgi fence's are. Its natives run in php-fpm's own module,
//! `fastcgi_finish_request()` and `fpm_get_status()`, which no other host
//! declares.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::oracle::{Observation, Oracle, normalise_in};
use crate::phpt::environment::Environment;
use crate::phpt::{cgi, file::Phpt};
use crate::scratch::Tree;

/// `FCGI_BEGIN_REQUEST`, `FCGI_END_REQUEST`, `FCGI_PARAMS`, `FCGI_STDIN`,
/// `FCGI_STDOUT` and `FCGI_STDERR`, the record types a request uses.
const BEGIN_REQUEST: u8 = 1;
const END_REQUEST: u8 = 3;
const PARAMS: u8 = 4;
const STDIN: u8 = 5;
const STDOUT: u8 = 6;
const STDERR: u8 = 7;

/// One record, as a web server writes it.
fn record(kind: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in content
        .chunks(0xffff)
        .chain(content.is_empty().then_some(&[][..]))
    {
        let len = u16::try_from(chunk.len()).unwrap_or(u16::MAX).to_be_bytes();
        out.extend_from_slice(&[1, kind, 0, 1, len[0], len[1], 0, 0]);
        out.extend_from_slice(chunk);
    }
    out
}

/// One name-value pair of `FCGI_PARAMS`.
fn pair(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    for len in [name.len(), value.len()] {
        match u8::try_from(len) {
            Ok(short) if short < 0x80 => out.push(short),
            _ => out
                .extend_from_slice(&(u32::try_from(len).unwrap_or(0) | 0x8000_0000).to_be_bytes()),
        }
    }
    out.extend_from_slice(name);
    out.extend_from_slice(value);
}

/// How long a master has to listen before the fence gives it up.
const PATIENCE: Duration = Duration::from_secs(10);

/// How many of the log's last lines a master that never listened is
/// named with.
const TAIL: usize = 10;

/// A php-fpm master, ended when dropped.
struct Master(Child);

impl Drop for Master {
    fn drop(&mut self) {
        // A master that already ended, one that never listened among them,
        // has nothing left to be sent.
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.0.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `snippet` served as the request `request` describes by the php-fpm at
/// `host`, with `env` in the master's environment, which its child keeps.
pub(crate) fn served_as_fpm(
    oracle: &Oracle<'_>,
    host: &Path,
    snippet: &str,
    request: &str,
    env: &[(String, OsString)],
) -> Result<Observation> {
    let recipe = &oracle.pin().recipe;
    let own = Tree::new("witness-fpm")?;
    let program = own.join(&recipe.script_name);
    fs::write(&program, format!("{}{snippet}", recipe.preamble))
        .with_context(|| format!("writing {}", program.display()))?;
    let test = format!("--TEST--\nwitness\n{request}\n--FILE--\n\n--EXPECT--\n\n");
    let path = own.join("witness.phpt");
    let phpt = Phpt::parse(&path, test.as_bytes())
        .map_err(|borked| anyhow!("the request fence is not a test's sections: {borked}"))?;
    let program = crate::oracle::run_by(&program)?;
    // The CGI variables emptied and the fence's `--ENV--` over them, as
    // the phpt lane builds a test's environment, then the request.
    let mut own_env = Environment::default();
    own_env.reset();
    own_env.extend(crate::phpt::env_of(&phpt, &path));
    let described = cgi::request(&phpt, &program, &own_env)
        .map_err(|(_, why)| anyhow!("the request fence describes no request: {why}"))?;
    // A later entry of a name stands over an earlier, as for php-cgi; the
    // names in the order they were first given.
    let mut given: Vec<(String, OsString)> = Vec::new();
    for (name, value) in described.env.entries().iter().cloned() {
        match given.iter_mut().find(|(had, _)| *had == name) {
            Some(slot) => slot.1 = value,
            None => given.push((name, value)),
        }
    }
    // The address is a path in the fence's own tree, so no other process
    // can listen there before the master does (#1474): a loopback port
    // found free and then let go was free for anyone between the letting
    // go and the master's bind, and a request sent there reached whatever
    // took it, another run's php-fpm among them.
    let socket = own.join("php-fpm.sock");
    let address = SocketAddr::from_pathname(&socket).with_context(|| {
        format!(
            "{} is longer than a socket's address holds",
            socket.display()
        )
    })?;
    let log = own.join("php-fpm.log");
    let conf = own.join("php-fpm.conf");
    fs::write(
        &conf,
        format!(
            "[global]\nerror_log = {}\n[witness]\nlisten = {}\npm = static\npm.max_children = 1\nclear_env = no\n",
            log.display(),
            socket.display()
        ),
    )
    .context("writing the php-fpm configuration")?;
    // `-O` keeps the master's log on its stderr, so the stderr is the log.
    let logged = fs::File::options()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("opening {}", log.display()))?;
    let mut master = Command::new(host);
    master.args(["-n", "-F", "-O", "-y"]).arg(&conf);
    for ini in &recipe.ini {
        master.arg("-d").arg(ini);
    }
    if recipe.env_clear {
        master.env_clear();
    }
    master
        .envs(&recipe.env)
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(logged);
    let mut master = Master(
        master
            .spawn()
            .with_context(|| format!("running {}", host.display()))?,
    );
    let started = Instant::now();
    let stream = loop {
        match UnixStream::connect_addr(&address) {
            Ok(stream) => break stream,
            Err(refused) => {
                if let Some(status) = master.0.try_wait()? {
                    bail!(
                        "php-fpm ended with {status} before it listened at {}; its log ends:\n{}",
                        socket.display(),
                        tail(&log)
                    );
                }
                if started.elapsed() >= PATIENCE {
                    bail!(
                        "php-fpm did not listen at {} in {}s: {refused}; its log ends:\n{}",
                        socket.display(),
                        PATIENCE.as_secs(),
                        tail(&log)
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(recipe.limits.timeout_s)))?;
    let uri = format!("/{}", recipe.script_name);
    let (stdout, stderr) = exchange(stream, &uri, &given, described.stdin.as_deref())?;
    let rules = &recipe.normalise;
    Ok(Observation {
        stdout: normalise_in(rules, own.path(), stdout),
        stderr: normalise_in(rules, own.path(), stderr),
        exit: Some(0),
        wall: started.elapsed(),
    })
}

/// The last [`TAIL`] lines of the log at `log`, or what kept it unread.
fn tail(log: &Path) -> String {
    match fs::read_to_string(log) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(TAIL)..].join("\n")
        }
        Err(unread) => format!("({} unread: {unread})", log.display()),
    }
}

/// Request 1 written to `stream` as a web server writes it, the server's
/// own variables for `uri` ahead of `given`, and what php-fpm writes back
/// on `FCGI_STDOUT` and `FCGI_STDERR` until `FCGI_END_REQUEST`.
fn exchange(
    mut stream: UnixStream,
    uri: &str,
    given: &[(String, OsString)],
    stdin: Option<&[u8]>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut params = Vec::new();
    for (name, value) in [
        ("GATEWAY_INTERFACE", "FastCGI/1.0"),
        ("SCRIPT_NAME", uri),
        ("REQUEST_URI", uri),
        ("DOCUMENT_URI", uri),
        ("SERVER_SOFTWARE", "php/fcgiclient"),
        ("REMOTE_ADDR", "127.0.0.1"),
        ("SERVER_NAME", "localhost"),
        ("SERVER_PROTOCOL", "HTTP/1.1"),
    ] {
        pair(&mut params, name.as_bytes(), value.as_bytes());
    }
    for (name, value) in given {
        pair(&mut params, name.as_bytes(), value.as_encoded_bytes());
    }
    let mut out = record(BEGIN_REQUEST, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(PARAMS, &params));
    out.extend(record(PARAMS, &[]));
    if let Some(body) = stdin {
        out.extend(record(STDIN, body));
    }
    out.extend(record(STDIN, &[]));
    stream.write_all(&out).context("writing the request")?;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let mut header = [0u8; 8];
        stream
            .read_exact(&mut header)
            .context("reading php-fpm's response")?;
        let len = usize::from(u16::from_be_bytes([header[4], header[5]]));
        let mut content = vec![0u8; len + usize::from(header[6])];
        stream
            .read_exact(&mut content)
            .context("reading php-fpm's response")?;
        content.truncate(len);
        match header[1] {
            STDOUT => stdout.extend_from_slice(&content),
            STDERR => stderr.extend_from_slice(&content),
            END_REQUEST => return Ok((stdout, stderr)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Instant;

    use crate::oracle::Pin;
    use crate::scratch::Tree;

    /// A master that ends before it listens fails the fence with its exit
    /// and its log's last lines as soon as it has ended, not at the end of
    /// the patience and not over a connection to whatever else listens
    /// (#1474). The stand-in writes what php-fpm writes when its address
    /// is taken, on the stderr `-O` keeps its log on, and exits as php-fpm
    /// then does.
    #[test]
    fn a_master_that_ends_before_it_listens_is_named_by_its_exit_and_its_log() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let pin =
            Pin::parse(&fs::read_to_string(workspace.join("../oracle.toml")).unwrap()).unwrap();
        let hosts = Tree::new("witness-fpm-ended").unwrap();
        let host = hosts.join("php-fpm");
        fs::write(
            &host,
            "#!/bin/sh\necho 'ERROR: unable to bind listening socket for address: Address already in use (48)' >&2\n\
             echo 'ERROR: FPM initialization failed' >&2\nexit 78\n",
        )
        .unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let failed =
            super::served_as_fpm(&pin.oracle(), &host, "echo 1;", "--GET--\nq=1", &[]).unwrap_err();
        let said = format!("{failed:#}");
        assert!(
            said.contains("ended with exit status: 78 before it listened")
                && said.contains("unable to bind listening socket")
                && said.contains("FPM initialization failed"),
            "{said}"
        );
        assert!(
            started.elapsed() < super::PATIENCE,
            "the fence waited out its patience for a master already ended: {said}"
        );
    }
}
