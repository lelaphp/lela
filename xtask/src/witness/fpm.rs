//! A fence with a `request fpm`: the snippet served as a request
//! of the web by a php-fpm, php-fpm's pin in `[sapi]` for the oracle and
//! `sapi/fpm`'s `php-fpm` for the engine. The master runs in the
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
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;

use crate::oracle::{Observation, Oracle};
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

/// The pool's ping page, asked for once the fence's response has ended.
const PING: &str = "/witness-ping";

/// How many of the log's last lines a master that never listened is
/// named with.
const TAIL: usize = 10;

/// A php-fpm master over a pool of static children, one for a fence,
/// listening on the socket at `socket`, its log at `log`; ended with
/// `SIGTERM` when dropped.
pub(super) struct Pool {
    master: Child,
    socket: PathBuf,
    address: SocketAddr,
    log: PathBuf,
}

impl Pool {
    /// The master's process.
    pub(super) fn pid(&self) -> u32 {
        self.master.id()
    }

    /// A connection to the pool once the master listens. A master that
    /// ends first is named by its exit and the log's last lines as soon as
    /// it has ended, and one still not listening after [`PATIENCE`] by
    /// the same lines.
    pub(super) fn connect(&mut self) -> Result<UnixStream> {
        let started = Instant::now();
        loop {
            let refused = match UnixStream::connect_addr(&self.address) {
                Ok(stream) => return Ok(stream),
                Err(refused) => refused,
            };
            if let Some(status) = self.master.try_wait()? {
                bail!(
                    "php-fpm ended with {status} before it listened at {}; its log ends:\n{}",
                    self.socket.display(),
                    tail(&self.log)
                );
            }
            if started.elapsed() >= PATIENCE {
                bail!(
                    "php-fpm did not listen at {} in {}s: {refused}; its log ends:\n{}",
                    self.socket.display(),
                    PATIENCE.as_secs(),
                    tail(&self.log)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Where the pool listens: what many clients at once connect to
    /// without [`Pool::connect`]'s `&mut`, once it listens.
    pub(super) fn address(&self) -> SocketAddr {
        self.address.clone()
    }

    /// Once the request whose response was read has ended, as well as its
    /// response.
    ///
    /// The response can end before the request does: a program that calls
    /// `fastcgi_finish_request()` runs on in the child after the last
    /// record, and what the child appends at the request's end, the trace
    /// of the natives it entered, comes after that. Dropping the pool ends
    /// the child wherever it stands, so a run that dropped it once the
    /// response was read lost the trace whenever the child had not yet
    /// reached the end, which two runs at once on one machine made likely.
    /// The pool's one child takes the next connection only once its
    /// request has ended, so the ping page it answers says that it has.
    pub(super) fn settle(&mut self, timeout: Duration) -> Result<()> {
        let ping = self.connect()?;
        ping.set_read_timeout(Some(timeout))?;
        exchange(
            ping,
            PING,
            &[("REQUEST_METHOD".to_owned(), OsString::from("GET"))],
            None,
        )
        .context("asking for the ping once the response had ended")?;
        Ok(())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        terminate(&mut self.master);
    }
}

/// A server process ended as a service manager ends one: SIGTERM, and a
/// kill once it has had five seconds; nothing where it has ended, one
/// that never listened among them.
pub(super) fn terminate(server: &mut Child) {
    if matches!(server.try_wait(), Ok(Some(_))) {
        return;
    }
    Command::new("kill")
        .arg("-TERM")
        .arg(server.id().to_string())
        .status()
        .ok();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(server.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    server.kill().ok();
    server.wait().ok();
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
    let (own, program) = super::process::program(oracle, "witness-fpm", snippet)?;
    let program = crate::oracle::run_by(&program)?;
    let described = super::process::described(&own, &program, request, &[])?;
    // A later entry of a name stands over an earlier, as for php-cgi; the
    // names in the order they were first given.
    let mut given: Vec<(String, OsString)> = Vec::new();
    for (name, value) in described.env.entries().iter().cloned() {
        match given.iter_mut().find(|(had, _)| *had == name) {
            Some(slot) => slot.1 = value,
            None => given.push((name, value)),
        }
    }
    let mut pool = Pool::start(oracle, host, &own, env, &[], (false, 1))?;
    let started = Instant::now();
    let timeout = Duration::from_secs(recipe.limits.timeout_s);
    let stream = pool.connect()?;
    stream.set_read_timeout(Some(timeout))?;
    let uri = format!("/{}", recipe.script_name);
    let streams = exchange(stream, &uri, &given, described.stdin.as_deref())?;
    pool.settle(timeout)?;
    Ok(super::process::answered(oracle, &own, streams, started))
}

impl Pool {
    /// The php-fpm at `host`, its master in the foreground over a pool of
    /// `children` children, `pm = static`, on a Unix socket in `own`, under
    /// the recipe's ini and `ini` after it as `-d`, with `env` in the
    /// master's environment, which its children keep, or none of it where
    /// `clear_env` says, php-fpm's default.
    pub(super) fn start(
        oracle: &Oracle<'_>,
        host: &Path,
        own: &Tree,
        env: &[(String, OsString)],
        ini: &[String],
        (clear_env, children): (bool, usize),
    ) -> Result<Pool> {
        let recipe = &oracle.pin().recipe;
        // The address is a path in the fence's own tree, so no other process
        // can listen there before the master does: a loopback port
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
        let conf = own.write("php-fpm.conf", conf(&log, &socket, (clear_env, children)))?;
        // `-O` keeps the master's log on its stderr, so the stderr is the log.
        let logged = fs::File::options()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("opening {}", log.display()))?;
        let mut master = Command::new(host);
        master.args(["-n", "-F", "-O", "-y"]).arg(&conf);
        super::process::under_recipe(recipe, &mut master, ini, env)?.stderr(logged);
        Ok(Pool {
            master: master
                .spawn()
                .with_context(|| format!("running {}", host.display()))?,
            socket,
            address,
            log,
        })
    }
}

/// The pool's configuration: its log at `log`, listening at `socket`,
/// `children` static children, its environment cleared where `clear_env`
/// says, and the ping page.
fn conf(log: &Path, socket: &Path, (clear_env, children): (bool, usize)) -> String {
    format!(
        "[global]\nerror_log = {}\n[witness]\nlisten = {}\npm = static\npm.max_children = {children}\nclear_env = {}\nping.path = {PING}\n",
        log.display(),
        socket.display(),
        if clear_env { "yes" } else { "no" }
    )
}

/// The last [`TAIL`] lines of the log at `log`, or what kept it unread.
fn tail(log: &Path) -> String {
    match fs::read_to_string(log) {
        Ok(text) => crate::batch::tail(&text, TAIL),
        Err(unread) => format!("({} unread: {unread})", log.display()),
    }
}

/// Request 1 written to `stream` as a web server writes it, the server's
/// own variables for `uri` ahead of `given`, and what php-fpm writes back
/// on `FCGI_STDOUT` and `FCGI_STDERR` until `FCGI_END_REQUEST`.
fn exchange(
    stream: UnixStream,
    uri: &str,
    given: &[(String, OsString)],
    stdin: Option<&[u8]>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let server = [
        ("GATEWAY_INTERFACE", "FastCGI/1.0"),
        ("SCRIPT_NAME", uri),
        ("REQUEST_URI", uri),
        ("DOCUMENT_URI", uri),
        ("SERVER_SOFTWARE", "php/fcgiclient"),
        ("REMOTE_ADDR", "127.0.0.1"),
        ("SERVER_NAME", "localhost"),
        ("SERVER_PROTOCOL", "HTTP/1.1"),
    ];
    let pairs = server
        .iter()
        .map(|(name, value)| (name.as_bytes(), value.as_bytes()))
        .chain(
            given
                .iter()
                .map(|(name, value)| (name.as_bytes(), value.as_encoded_bytes())),
        );
    round(stream, pairs, stdin)
}

/// Request 1 written to `stream` with `pairs` as its `FCGI_PARAMS` and
/// `stdin` as its `FCGI_STDIN`, and what php-fpm writes back on
/// `FCGI_STDOUT` and `FCGI_STDERR` until `FCGI_END_REQUEST`, or [`Closed`]
/// where it ends the connection first: a fence's request, and a host
/// line's by its parts (`super::host`).
pub(super) fn round<'a>(
    mut stream: impl Read + Write,
    pairs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    stdin: Option<&[u8]>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    sent(stream.write_all(&request(pairs, stdin)))?;
    answer(stream)
}

/// The records of request 1, [`round`]'s, as they are written.
pub(super) fn request<'a>(
    pairs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    stdin: Option<&[u8]>,
) -> Vec<u8> {
    let mut params = Vec::new();
    for (name, value) in pairs {
        pair(&mut params, name, value);
    }
    let mut out = record(BEGIN_REQUEST, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(PARAMS, &params));
    out.extend(record(PARAMS, &[]));
    if let Some(body) = stdin {
        out.extend(record(STDIN, body));
    }
    out.extend(record(STDIN, &[]));
    out
}

/// What the server writes back on `FCGI_STDOUT` and `FCGI_STDERR` until
/// `FCGI_END_REQUEST`, [`round`]'s; [`Closed`], with the records it wrote
/// whole, where the connection ends before that record.
pub(super) fn answer(mut stream: impl Read) -> Result<(Vec<u8>, Vec<u8>)> {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let mut header = [0u8; 8];
        let record = stream.read_exact(&mut header).and_then(|()| {
            let len = usize::from(u16::from_be_bytes([header[4], header[5]]));
            let mut content = vec![0u8; len + usize::from(header[6])];
            stream.read_exact(&mut content)?;
            content.truncate(len);
            Ok(content)
        });
        let content = match record {
            Ok(content) => content,
            Err(error) if gone(&error) => {
                return Err(Closed {
                    stdout,
                    stderr,
                    error,
                }
                .into());
            }
            Err(error) => {
                return Err(anyhow::Error::from(error).context("reading php-fpm's response"));
            }
        };
        match header[1] {
            STDOUT => stdout.extend_from_slice(&content),
            STDERR => stderr.extend_from_slice(&content),
            END_REQUEST => return Ok((stdout, stderr)),
            _ => {}
        }
    }
}

/// Whether `error` is a read or a write meeting a connection its peer has
/// ended, and no trouble of the client's own, a read past its time among
/// them. A peer that closes a socket holding bytes it has not read resets
/// the connection, and one that closes having read them ends it cleanly,
/// so which of the two a client meets is how far its request had come
/// when the server closed: an end of file or a reset on a read, a broken
/// pipe or a reset on a write, a socket no longer connected on a shut
/// half, and on macOS `EPROTOTYPE` on a write to a socket the reset is
/// taking down.
fn gone(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{
        BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected, UnexpectedEof,
    };
    matches!(
        error.kind(),
        UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected
    ) || error.raw_os_error().map(Errno::from_raw) == Some(Errno::EPROTOTYPE)
}

/// A request's write, or its half's shut, as a round takes it: one the
/// server refused, having ended the connection ([`gone`]), is no failure
/// of the round, since what the server wrote before then is [`answer`]'s
/// to read, which meets the end itself and says [`Closed`].
pub(super) fn sent(written: std::io::Result<()>) -> Result<()> {
    match written {
        Err(refused) if gone(&refused) => Ok(()),
        written => written.context("writing the request"),
    }
}

/// A connection the server ended, closed or reset, before its
/// `FCGI_END_REQUEST`: php-fpm's child where it `_exit`s under a request,
/// the pinned php's hard memory limit among them
/// (`Z/Zend/zend_alloc.c:519`), and where it drops a peer its
/// `listen.allowed_clients` does not name, with a bare `close` and no
/// record (`Z/main/fastcgi.c:1401-1405`). It holds what the server wrote on
/// `FCGI_STDOUT` and `FCGI_STDERR` in whole records until then, and the
/// read that met the end, whose error a `request fpm` fence fails with; a
/// host line holds it as the request's answer (`super::host`).
#[derive(Debug)]
pub(super) struct Closed {
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    pub(super) error: std::io::Error,
}

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "reading php-fpm's response: {}, before FCGI_END_REQUEST",
            self.error
        )
    }
}

impl std::error::Error for Closed {}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    /// A pool is `pm = static` with the children asked for: one for a
    /// fence, eight for the throughput bar.
    #[test]
    fn a_pool_is_static_with_the_children_asked_for() {
        let at = |children| super::conf("l".as_ref(), "s".as_ref(), (true, children));
        assert!(
            at(8).contains("pm = static\npm.max_children = 8\nclear_env = yes\n"),
            "{}",
            at(8)
        );
        assert!(at(1).contains("pm.max_children = 1\n"));
    }

    use std::fs;
    use std::time::Instant;

    use crate::oracle::Pin;
    use crate::scratch::Tree;

    /// A master that ends before it listens fails the fence with its exit
    /// and its log's last lines as soon as it has ended, not at the end of
    /// the patience and not over a connection to whatever else listens.
    /// The stand-in writes what php-fpm writes when its address
    /// is taken, on the stderr `-O` keeps its log on, and exits as php-fpm
    /// then does.
    #[test]
    fn a_master_that_ends_before_it_listens_is_named_by_its_exit_and_its_log() {
        let workspace = crate::package_dir();
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
        crate::scratch::executable(&host).unwrap();
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

    /// A server that fails the request once it has read all of it fails
    /// the fence in the transport's words: the stand-ins of the
    /// fixtures `request-reset-sent`, which ends the connection partway
    /// through its first record, and `request-empty`, which sends nothing,
    /// each say where the response broke off.
    #[test]
    fn a_response_that_breaks_off_names_where() {
        let workspace = crate::package_dir();
        let pin =
            Pin::parse(&fs::read_to_string(workspace.join("../oracle.toml")).unwrap()).unwrap();
        for name in ["request-reset-sent", "request-empty"] {
            let host = workspace
                .join("fixtures/witness")
                .join(name)
                .join("php-fpm");
            let failed = super::served_as_fpm(&pin.oracle(), &host, "echo 1;", "--GET--\nq=1", &[])
                .unwrap_err();
            let said = format!("{failed:#}");
            assert!(
                said.contains("reading php-fpm's response") && said.contains("fill whole buffer"),
                "{name}: {said}"
            );
        }
    }

    /// A connection the server ends before `FCGI_END_REQUEST` is
    /// [`super::Closed`] however the client meets the end, an end
    /// of file or a reset, at a record's edge or inside one, and holds the
    /// records that came whole; a read past its time is the client's own
    /// trouble and no end. A write the server refused is left to the read
    /// to say, and one that failed otherwise fails the round.
    #[test]
    fn a_connection_ended_before_its_last_record_is_closed_however_it_ends() {
        use std::io::{Cursor, Error, ErrorKind, Read};

        /// What a server wrote, and what the read after it meets.
        struct Ends(Cursor<Vec<u8>>, Option<ErrorKind>);
        impl Read for Ends {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match (self.0.read(buf)?, self.1.take()) {
                    (0, Some(met)) => Err(met.into()),
                    (got, met) => {
                        self.1 = met;
                        Ok(got)
                    }
                }
            }
        }
        let whole = super::record(super::STDOUT, b"out");
        let partway = [
            whole.as_slice(),
            &super::record(super::STDERR, b"err")[..10],
        ]
        .concat();
        for (wrote, met) in [
            (&whole, None),
            (&whole, Some(ErrorKind::ConnectionReset)),
            (&partway, None),
            (&partway, Some(ErrorKind::ConnectionReset)),
        ] {
            let ended = super::answer(Ends(Cursor::new(wrote.clone()), met)).unwrap_err();
            let closed = ended.downcast::<super::Closed>().unwrap();
            assert_eq!(
                (closed.stdout.as_slice(), closed.stderr.as_slice()),
                (&b"out"[..], &b""[..])
            );
        }
        let late =
            super::answer(Ends(Cursor::new(whole), Some(ErrorKind::WouldBlock))).unwrap_err();
        assert!(!late.is::<super::Closed>(), "{late:#}");

        let prototype = Error::from_raw_os_error(nix::errno::Errno::EPROTOTYPE as i32);
        for refused in [
            ErrorKind::BrokenPipe.into(),
            ErrorKind::ConnectionReset.into(),
            prototype,
        ] {
            assert!(super::sent(Err(refused)).is_ok());
        }
        let failed = super::sent(Err(ErrorKind::TimedOut.into())).unwrap_err();
        assert!(
            format!("{failed:#}").contains("writing the request"),
            "{failed:#}"
        );
    }

    /// What the child appends after the response has ended is there when
    /// the fence returns, though the master is ended where its child
    /// stands. The stand-in answers the request with its last record at
    /// once, as a child whose program called `fastcgi_finish_request()`
    /// does, takes half a second before it appends to the file the
    /// variable names, and only then takes the next connection, as
    /// php-fpm's one child does.
    #[test]
    fn what_the_child_appends_after_the_response_ends_is_there_when_the_fence_returns() {
        let workspace = crate::package_dir();
        let pin =
            Pin::parse(&fs::read_to_string(workspace.join("../oracle.toml")).unwrap()).unwrap();
        let hosts = Tree::new("witness-fpm-finishing").unwrap();
        let host = hosts.join("php-fpm");
        fs::write(
            &host,
            "#!/usr/bin/perl\nuse Socket;\nuse Time::HiRes qw(time sleep);\n\
             my ($conf, $path);\nfor my $i (0 .. $#ARGV) { $conf = $ARGV[$i + 1] if $ARGV[$i] eq '-y'; }\n\
             open(my $f, '<', $conf) or die;\nwhile (<$f>) { $path = $1 if /^listen = (\\S+)/; }\n\
             socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die;\nbind($s, sockaddr_un($path)) or die;\nlisten($s, 5) or die;\n\
             sub answer { my ($c) = @_; my $b; sysread($c, $b, 65536); syswrite($c, pack('CCnnCC', 1, 3, 1, 8, 0, 0) . (\"\\0\" x 8)); close($c); }\n\
             accept(my $c, $s) or die;\nanswer($c);\n\
             my $until = time + 0.5;\nsleep(0.05) while time < $until;\n\
             open(my $t, '>>', $ENV{WITNESS_FPM_APPENDED}) or die;\nprint $t \"appended\\n\";\nclose($t);\n\
             while (accept(my $next, $s)) { answer($next); }\n",
        )
        .unwrap();
        crate::scratch::executable(&host).unwrap();
        let appended = hosts.join("appended");
        let env = [(
            "WITNESS_FPM_APPENDED".to_owned(),
            appended.clone().into_os_string(),
        )];
        super::served_as_fpm(&pin.oracle(), &host, "echo 1;", "--GET--\nq=1", &env).unwrap();
        assert_eq!(
            fs::read_to_string(&appended).unwrap_or_default(),
            "appended\n",
            "the child was ended before the end of its request"
        );
    }
}
