//! A host's own lines, held beside the pinned host's (#1975; ruled on
//! #2468, option C). What a fence cannot carry, an ini of the line's own
//! (#1634), a file laid before the run, a request a server takes, a
//! process that ends at its startup, is a line in the host crate's
//! `tests/lines.toml`: a command line, an environment over the recipe's,
//! the files laid in a directory of the line's own, what stdin reads, and
//! the masks the line names, never one by default. Each line runs under the
//! pinned host of its kind, `[oracle]`'s php for `sapi/cli` and `[sapi]`'s
//! for the others, its digest asserted first, and then under the engine's
//! own host in the same directory laid again; what the two wrote, how
//! they ended, what each request was answered and the directory's files
//! after are held equal, through the registry as a witness is. No byte of
//! the oracle's is kept in the file, so an oracle bump moves the
//! expectation with it. The lines are judged among the crate's witnesses
//! and the crate's reading is over both; a line whose pinned host is not
//! on this machine, or not the pin's by its digest, fails and is never a
//! skip.
//!
//! ```toml
//! [[line]]
//! name = "a-refused-start"             # what the verdict names it by
//! args = ["-n", "-d", "x=1", "t.php"]  # the host's command line
//! env = { SCRIPT_FILENAME = "{dir}/t.php" }  # an empty value takes the name out
//! files = { "t.php" = "<?php echo 1;", "scan/" = "" }  # a name ending in `/` is a directory
//! modes = { "t.php" = 0o000 }
//! stdin = "a\n"                        # a pipe; without it, /dev/null
//! head = 4096                          # stdout's first bytes, then killed
//!                                      # (or once it writes nothing for 2s)
//! streams = "merged"                   # "pipes", the default, "merged" or "files"
//! mask = [['\((\d+)\): ', '(<pid>): ']]  # a pattern and what stands for each match
//! [line.serve]                         # a server: started, asked, then stopped
//! listen = "{dir}/fpm.sock"            # a path is a Unix socket, else host:port
//! ready = ["fpm.log", "ready to handle connections"]  # waited for in a file
//! stop = "SIGQUIT"
//! settled = ["fpm.log", '(?s)started.*started']  # matched once answered, before the stop
//! [[line.serve.request]]
//! http = "GET / HTTP/1.0\r\n\r\n"      # or `params` and `body`, as FastCGI records
//! [[line.then]]                        # a command line run after it, in the same
//! args = ["-n", "u.php"]               # directory and under its env with this one's
//! env = { SCRIPT_FILENAME = "{dir}/u.php" }  # over it
//! ```
//!
//! `{dir}` is the line's directory and `{port}` a loopback port found
//! free, each the same for both runs, so a path or a port the line names
//! needs no mask.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use regex_lite::Regex;
use serde::Deserialize;

use super::{Judged, Judgement, Scope};
use crate::divergence::Registry;
use crate::manifest::Crate;
use crate::oracle::hosts::{self, EngineHost};
use crate::oracle::{self, Oracle, Running, Stdin, Streams, Verdict};
use crate::scratch::Tree;

/// Where a host crate keeps its lines, under the crate.
pub(crate) const LINES: &str = "tests/lines.toml";

/// How long a server has to listen, or to write what `ready` waits for.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a line with a `head` may write nothing before it is taken to
/// wait and is killed.
const QUIET: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    line: Vec<Line>,
}

/// One line, as the file writes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Line {
    pub(crate) name: String,
    args: Vec<String>,
    #[serde(default)]
    then: Vec<Then>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    files: BTreeMap<String, String>,
    #[serde(default)]
    modes: BTreeMap<String, u32>,
    stdin: Option<String>,
    #[serde(default)]
    streams: Taken,
    head: Option<u64>,
    serve: Option<Serve>,
    #[serde(default)]
    mask: Vec<(String, String)>,
}

/// A command line run after the line's own, in the same directory, under
/// the line's environment with its own over it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Then {
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// How a line's two streams are taken, each as [`Streams`] takes them.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Taken {
    #[default]
    Pipes,
    Merged,
    Files,
}

impl From<Taken> for Streams {
    fn from(taken: Taken) -> Streams {
        match taken {
            Taken::Pipes => Streams::Pipes { merged: false },
            Taken::Merged => Streams::Pipes { merged: true },
            Taken::Files => Streams::Files,
        }
    }
}

/// What makes a line a server's: where it listens, what it writes once it
/// is ready, the requests it is sent, each on a connection of its own and
/// answered before the next, what a file of its matches once the server
/// has done what the last answer set going, and the signal that stops it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Serve {
    listen: String,
    ready: Option<(String, String)>,
    #[serde(default)]
    request: Vec<Request>,
    /// A file and a pattern its text is waited to match after the last
    /// answer and before the stop: the event a stop would otherwise race,
    /// such as php-fpm replacing a child that ended at its
    /// `pm.max_requests` after answering (#2531).
    settled: Option<(String, String)>,
    stop: String,
}

/// One request: the bytes of an HTTP one, or the parameters and body of a
/// `FastCGI` one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    http: Option<String>,
    params: Option<Vec<(String, String)>>,
    body: Option<String>,
}

/// One host crate's lines: the host they run, the file they stand in, and
/// the lines a run serves.
#[derive(Debug)]
pub(crate) struct Held {
    host: EngineHost,
    file: PathBuf,
    pub(crate) lines: Vec<Line>,
}

impl Held {
    /// The lines of `krate` where it is a host's crate with a lines file,
    /// read and checked before anything runs; none for any other crate.
    pub(crate) fn of(root: &Path, krate: &Crate) -> Result<Option<Held>> {
        let Some(host) = hosts::HOSTS.into_iter().find(|h| h.krate == krate.name) else {
            return Ok(None);
        };
        let file = krate.path().join(LINES);
        let Some(text) = crate::read_if(&root.join(&file))? else {
            return Ok(None);
        };
        Held::parse(host, file, &text).map(Some)
    }

    /// `text` as `host`'s lines in `file`: every name the file's once, every
    /// mask a pattern, every request one kind, every stop a signal.
    fn parse(host: EngineHost, file: PathBuf, text: &str) -> Result<Held> {
        let at = file.display().to_string();
        let written: Written =
            toml::from_str(text).with_context(|| format!("{at} is no host's lines"))?;
        let mut seen = BTreeSet::new();
        for line in &written.line {
            let named = format!("{at}: line {:?}", line.name);
            if !seen.insert(&line.name) {
                bail!("{named} is named twice");
            }
            line.masks().with_context(|| named.clone())?;
            if line.head.is_some() && line.serve.is_some() {
                bail!("{named}: a server is stopped by its `stop`, never by a `head`");
            }
            if let Some(serve) = &line.serve {
                Signal::from_str(&serve.stop).with_context(|| format!("{named}: stop"))?;
                if let Some((_, pattern)) = &serve.settled {
                    Regex::new(pattern).with_context(|| format!("{named}: settled"))?;
                }
                if serve
                    .request
                    .iter()
                    .any(|r| r.http.is_some() == r.params.is_some())
                {
                    bail!("{named}: a request is `http` or `params`, one of the two");
                }
            }
        }
        Ok(Held {
            host,
            file,
            lines: written.line,
        })
    }

    /// The lines `scope` serves: all of them for the crate, all for a run
    /// narrowed to this file, and the one of that name for a run narrowed
    /// to an item.
    pub(crate) fn narrowed(mut self, scope: &Scope) -> Held {
        match scope {
            Scope::Crate => {}
            Scope::File(path) if self.file.ends_with(path) => {}
            Scope::File(_) => self.lines.clear(),
            Scope::Item(name) => self.lines.retain(|line| &line.name == name),
        }
        self
    }

    /// One line run under the pinned host and then under the engine's,
    /// and held: a pass where the two are equal, the registry's word
    /// where they are not, and a bork where either could not run.
    pub(super) fn judge(
        &self,
        oracle: &Oracle<'_>,
        engine: Result<PathBuf>,
        registry: &Registry,
        line: &Line,
    ) -> Judged {
        let id = format!("{}#{}", self.file.display(), line.name);
        let ran = oracle
            .pinned_host(self.host)
            .and_then(|pinned| Ok((pinned, engine?)))
            .and_then(|(pinned, engine)| both(oracle, &pinned, &engine, line));
        let (judgement, note) = match ran {
            Err(trouble) => (Judgement::Bork, format!("{trouble:#}")),
            Ok((theirs, ours)) => {
                match oracle::compare(registry, &id, theirs.as_bytes(), ours.as_bytes()) {
                    Verdict::Same => (Judgement::Pass, String::new()),
                    Verdict::Diverged(row) => (Judgement::Diverged, row.reason),
                    Verdict::Differs(diff) => (Judgement::Fail, diff),
                }
            }
        };
        Judged::new(&id, &id, judgement, note)
    }

    /// The host these lines run.
    pub(super) fn host(&self) -> EngineHost {
        self.host
    }
}

impl Line {
    /// The line's masks, each pattern compiled.
    fn masks(&self) -> Result<Vec<(Regex, &str)>> {
        self.mask
            .iter()
            .map(|(pattern, with)| {
                Regex::new(pattern)
                    .map(|re| (re, with.as_str()))
                    .with_context(|| format!("mask {pattern:?}"))
            })
            .collect()
    }
}

/// The line under `pinned` and under `engine`, each in the same directory
/// laid afresh and on the same port, rendered and masked: the oracle's
/// rendering and the engine's.
fn both(
    oracle: &Oracle<'_>,
    pinned: &Path,
    engine: &Path,
    line: &Line,
) -> Result<(String, String)> {
    let dir = Tree::new("line")?;
    let masks = line.masks()?;
    let masked = |text: String| {
        masks.iter().fold(text, |text, (re, with)| {
            re.replace_all(&text, *with).into_owned()
        })
    };
    let mut taken = Vec::new();
    for _ in 0..ATTEMPTS {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|probe| probe.local_addr())
            .context("finding a free loopback port")?
            .port();
        let renderings = ran(oracle, pinned, line, dir.path(), port)
            .and_then(|theirs| Ok((theirs, ran(oracle, engine, line, dir.path(), port)?)));
        match renderings {
            Err(err) if err.is::<InUse>() => taken.push(port.to_string()),
            renderings => {
                let (theirs, ours) = renderings?;
                return Ok((masked(theirs), masked(ours)));
            }
        }
    }
    bail!(
        "line {:?}: each of the ports {} was found free and taken before a host bound it",
        line.name,
        taken.join(", ")
    )
}

/// How many ports a server's line is offered: a port found free can be
/// taken by another process before the host binds it, which then says so
/// and serves nothing (#2653).
const ATTEMPTS: usize = 3;

/// A server's line whose host could not bind `{port}` because something
/// else had it: the line is run again, both hosts, on another port.
#[derive(Debug)]
struct InUse(u16);

impl std::fmt::Display for InUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "port {} was taken before the host bound it", self.0)
    }
}

impl std::error::Error for InUse {}

/// Whether a server's run said, on a stream or in a file of `dir`, that
/// `port` was in use when it bound it: php -S's `Failed to listen on
/// 127.0.0.1:<port> (reason: Address already in use)`, php-fpm's `unable
/// to bind listening socket for address '127.0.0.1:<port>': Address
/// already in use (48)`.
fn in_use(port: u16, streams: [&[u8]; 2], dir: &Path) -> Result<bool> {
    let (address, said) = (format!(":{port}"), "Address already in use");
    // `:5000` is not `:50001`: the port is followed by no other digit.
    let port_named = |text: &str| {
        text.match_indices(&address)
            .any(|(at, _)| !text[at + address.len()..].starts_with(|c: char| c.is_ascii_digit()))
    };
    let names = |text: &str| port_named(text) && text.contains(said);
    Ok(streams
        .into_iter()
        .any(|bytes| names(&String::from_utf8_lossy(bytes)))
        || files(dir)?.values().any(|text| names(text)))
}

/// `{dir}` and `{port}` in `text` as this line's run has them.
fn fill(text: &str, dir: &Path, port: u16) -> String {
    text.replace("{dir}", &dir.display().to_string())
        .replace("{port}", &port.to_string())
}

/// The line run by `binary` over `dir` laid afresh: each command line's
/// streams and ending, each request's answer, and every file the
/// directory holds after, rendered as one text.
fn ran(oracle: &Oracle<'_>, binary: &Path, line: &Line, dir: &Path, port: u16) -> Result<String> {
    lay(line, dir, port)?;
    let recipe = &oracle.pin().recipe;
    let limit = Duration::from_secs(recipe.limits.timeout_s);
    let stdin = line.stdin.as_deref().map(|text| fill(text, dir, port));
    let mut out = String::new();
    let runs = std::iter::once((&line.args, None))
        .chain(line.then.iter().map(|then| (&then.args, Some(&then.env))));
    for (n, (args, own)) in runs.enumerate() {
        let mut cmd = Command::new(binary);
        cmd.args(args.iter().map(|arg| fill(arg, dir, port)))
            .current_dir(dir);
        recipe.environ(&mut cmd)?;
        for (name, value) in line.env.iter().chain(own.into_iter().flatten()) {
            if value.is_empty() {
                cmd.env_remove(name);
            } else {
                cmd.env(name, fill(value, dir, port));
            }
        }
        let serve = line.serve.as_ref().filter(|_| n == 0);
        let fed = stdin
            .as_deref()
            .map_or(Stdin::Null, |text| Stdin::Pipe(text.as_bytes()));
        let mut ended = oracle.to_end(cmd, line.streams.into(), fed, limit, |running| {
            match (serve, line.head) {
                (Some(serve), _) => served(serve, running.pid, dir, port),
                (None, Some(head)) => headed(running, head).map(|()| Vec::new()),
                (None, None) => Ok(Vec::new()),
            }
        })?;
        if serve.is_some() && in_use(port, [&ended.stdout, &ended.stderr], dir)? {
            return Err(InUse(port).into());
        }
        let Some(exit) = ended.exit else {
            bail!(
                "{} ran past the recipe's limit of {} s on line {:?}",
                binary.display(),
                limit.as_secs(),
                line.name
            );
        };
        if let Some(head) = line.head {
            ended
                .stdout
                .truncate(usize::try_from(head).unwrap_or(usize::MAX));
        }
        let run = n + 1;
        write!(
            out,
            "== run {run} stdout\n{}\n== run {run} stderr\n{}\n== run {run} ended with {exit}\n",
            shown(&ended.stdout),
            shown(&ended.stderr)
        )
        .ok();
        for (asked, answer) in ended.during?.into_iter().enumerate() {
            writeln!(out, "== run {run} request {}\n{answer}", asked + 1).ok();
        }
    }
    for (name, bytes) in files(dir)? {
        writeln!(out, "== file {name}\n{bytes}").ok();
    }
    Ok(out)
}

/// Bytes as text: as they are where they are UTF-8, escaped where not, so
/// no two differing streams render alike.
fn shown(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.escape_ascii().to_string(),
    }
}

/// `dir` emptied and the line's files laid in it, each mode set after.
fn lay(line: &Line, dir: &Path, port: u16) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        }
        .with_context(|| format!("removing {}", path.display()))?;
    }
    for (name, text) in &line.files {
        let path = dir.join(name);
        let parent = if name.ends_with('/') {
            path.as_path()
        } else {
            path.parent().unwrap_or(dir)
        };
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        if !name.ends_with('/') {
            crate::write(&path, fill(text, dir, port))?;
        }
    }
    for (name, mode) in &line.modes {
        let path = dir.join(name);
        fs::set_permissions(&path, fs::Permissions::from_mode(*mode))
            .with_context(|| format!("setting the mode of {}", path.display()))?;
    }
    Ok(())
}

/// Every regular file under `dir`, by its path from `dir`, and what it
/// holds, or why it could not be read.
fn files(dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut found = BTreeMap::new();
    let mut left = vec![dir.to_path_buf()];
    while let Some(at) = left.pop() {
        for entry in fs::read_dir(&at).with_context(|| format!("reading {}", at.display()))? {
            let path = entry?.path();
            let kind = fs::symlink_metadata(&path)?.file_type();
            if kind.is_dir() {
                left.push(path);
            } else if kind.is_file() {
                let name = path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let held = fs::read(&path).map_or_else(|e| format!("({e})"), |b| shown(&b));
                found.insert(name, held);
            }
        }
    }
    Ok(found)
}

/// A server's run while it stands: ready, asked each request in turn, and
/// sent its stop; the answers. Where any of it fails the server is killed
/// rather than left to the recipe's limit.
fn served(serve: &Serve, pid: u32, dir: &Path, port: u16) -> Result<Vec<String>> {
    let answers = asked(serve, dir, port);
    let signal = match answers {
        Ok(_) => Signal::from_str(&serve.stop)?,
        Err(_) => Signal::SIGKILL,
    };
    signalled(pid, signal)?;
    answers
}

/// A program that does not end on its own, a session that loops or one
/// that waits at a prompt with nothing more to read, killed with its group
/// once it has written `head` bytes to stdout or once it has written
/// nothing for [`QUIET`].
fn headed(running: &Running<'_>, head: u64) -> Result<()> {
    let (mut written, mut since) = (running.written(), Instant::now());
    while written < head && since.elapsed() < QUIET {
        std::thread::sleep(Duration::from_millis(10));
        let now = running.written();
        if now != written {
            (written, since) = (now, Instant::now());
        }
    }
    let group = Pid::from_raw(i32::try_from(running.pid).context("a process id past pid_t")?);
    match killpg(group, Signal::SIGKILL) {
        // A program that ended on its own before its `head` leaves a group
        // of processes not yet reaped, which answers ESRCH, or EPERM on
        // macOS; its ending is held as any line's is.
        Ok(()) | Err(Errno::ESRCH | Errno::EPERM) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("killing the group {group}")),
    }
}

/// `signal` sent to the program `pid`.
fn signalled(pid: u32, signal: Signal) -> Result<()> {
    let pid = Pid::from_raw(i32::try_from(pid).context("a process id past pid_t")?);
    kill(pid, signal).with_context(|| format!("sending {signal} to {pid}"))
}

/// What a server answered each of its requests, once it was ready, and
/// once its file says it has settled after them.
fn asked(serve: &Serve, dir: &Path, port: u16) -> Result<Vec<String>> {
    let started = Instant::now();
    if let Some((file, text)) = &serve.ready {
        waited(&dir.join(file), text, |written| written.contains(text))?;
    }
    let listen = fill(&serve.listen, dir, port);
    let mut answers = Vec::new();
    for request in &serve.request {
        let answer = loop {
            match answered(request, &listen, &|text| fill(text, dir, port)) {
                Err(refused) if refused.is::<Refused>() && started.elapsed() < PATIENCE => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => break other?,
            }
        };
        answers.push(answer);
    }
    if let Some((file, pattern)) = &serve.settled {
        let pattern = Regex::new(pattern)?;
        waited(&dir.join(file), pattern.as_str(), |written| {
            pattern.is_match(written)
        })?;
    }
    Ok(answers)
}

/// Until the file at `path` holds what `said` describes, as `holds` reads
/// it, [`PATIENCE`] at most.
fn waited(path: &Path, said: &str, holds: impl Fn(&str) -> bool) -> Result<()> {
    let started = Instant::now();
    while !fs::read_to_string(path).is_ok_and(|written| holds(&written)) {
        if started.elapsed() > PATIENCE {
            bail!(
                "{} did not say {said:?} in {}s",
                path.display(),
                PATIENCE.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// A connection the server did not take, which is tried again until it
/// listens.
#[derive(Debug)]
struct Refused(std::io::Error);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not listening: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// One request on a connection of its own to `listen`, and the answer
/// read to its end: an HTTP response's bytes, or a `FastCGI` response's
/// `FCGI_STDOUT` with its `FCGI_STDERR` after.
fn answered(request: &Request, listen: &str, fill: &dyn Fn(&str) -> String) -> Result<String> {
    let wait = Some(Duration::from_mins(1));
    match (&request.http, &request.params) {
        (Some(http), _) => {
            let mut stream = TcpStream::connect(listen).map_err(Refused)?;
            stream.set_read_timeout(wait)?;
            stream.write_all(fill(http).as_bytes())?;
            let mut answer = Vec::new();
            stream
                .read_to_end(&mut answer)
                .context("reading the response")?;
            Ok(shown(&answer))
        }
        (None, Some(params)) => {
            let params: Vec<(&str, String)> =
                params.iter().map(|(n, v)| (n.as_str(), fill(v))).collect();
            let pairs = params.iter().map(|(n, v)| (n.as_bytes(), v.as_bytes()));
            let body = request.body.as_deref().map(fill);
            let body = body.as_deref().map(str::as_bytes);
            // A path is a Unix socket, and anything else `host:port`.
            let (stdout, stderr) = if listen.starts_with('/') {
                let stream = UnixStream::connect(listen).map_err(Refused)?;
                stream.set_read_timeout(wait)?;
                super::fpm::round(stream, pairs, body)?
            } else {
                let stream = TcpStream::connect(listen).map_err(Refused)?;
                stream.set_read_timeout(wait)?;
                super::fpm::round(stream, pairs, body)?
            };
            Ok(format!("{}\n-- stderr\n{}", shown(&stdout), shown(&stderr)))
        }
        (None, None) => bail!("a request is `http` or `params`"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::{Held, Judgement, hosts};
    use crate::divergence::Registry;
    use crate::oracle::{Host, Pin};
    use crate::scratch::Tree;

    /// The tracked pin with `php-cgi` standing at `cgi`, pinned by the
    /// digest given, the file's own where none is.
    fn pin(cgi: &Path, md5: Option<&str>) -> Pin {
        let root = crate::package_dir().join("..");
        let mut pin = Pin::parse(&fs::read_to_string(root.join("oracle.toml")).unwrap()).unwrap();
        let md5 = md5.map_or_else(
            || format!("{:x}", md5::compute(fs::read(cgi).unwrap_or_default())),
            str::to_owned,
        );
        pin.sapi.cgi = Some(Host {
            path: cgi.to_path_buf(),
            md5,
        });
        pin
    }

    /// A stand-in host: `body` as a shell program in `hosts`.
    fn host(hosts: &Tree, name: &str, body: &str) -> PathBuf {
        let path = hosts.write(name, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The one line of `text`, judged under the pinned host at `pinned`
    /// and the engine's at `engine`.
    fn judged(text: &str, pin: &Pin, engine: &Path) -> (Judgement, String) {
        let held = Held::parse(hosts::CGI, "sapi/cgi/tests/lines.toml".into(), text).unwrap();
        let judged = held.judge(
            &pin.oracle(),
            Ok(engine.to_path_buf()),
            &Registry::default(),
            &held.lines[0],
        );
        (judged.judgement, judged.note)
    }

    const LINE: &str =
        "[[line]]\nname = \"one\"\nargs = [\"{dir}\"]\nfiles = { \"in.txt\" = \"in\" }\n";

    /// The two hosts' streams, endings and files held equal: the same
    /// program passes; one that writes another byte, ends otherwise or
    /// leaves another file fails, the difference named.
    #[test]
    fn a_line_passes_where_the_hosts_agree_and_fails_where_they_differ() {
        let hosts = Tree::new("host-line-hosts").unwrap();
        let same = "echo \"$1\" | wc -c; echo err >&2; cat in.txt > out.txt; exit 3";
        let pinned = host(&hosts, "php-cgi", same);
        let pin = pin(&pinned, None);
        let engine = host(&hosts, "same", same);
        assert_eq!(judged(LINE, &pin, &engine).0, Judgement::Pass);
        for (name, other, differs) in [
            (
                "stream",
                "echo x; echo err >&2; cat in.txt > out.txt; exit 3",
                "+x",
            ),
            (
                "status",
                "echo \"$1\" | wc -c; echo err >&2; cat in.txt > out.txt; exit 4",
                "+== run 1 ended with status 4",
            ),
            (
                "file",
                "echo \"$1\" | wc -c; echo err >&2; echo no > out.txt; exit 3",
                "+no",
            ),
        ] {
            let engine = host(&hosts, name, other);
            let (judgement, note) = judged(LINE, &pin, &engine);
            assert_eq!(judgement, Judgement::Fail, "{name}: {note}");
            assert!(note.contains(differs), "{name}: {note}");
        }
    }

    /// A mask the line names stands for what it matches in both runs, and
    /// no mask stands where the line names none.
    #[test]
    fn a_mask_the_line_names_hides_what_it_matches() {
        let hosts = Tree::new("host-line-masked").unwrap();
        let pinned = host(&hosts, "php-cgi", "echo \"pid $$\"");
        let pin = pin(&pinned, None);
        let engine = host(&hosts, "engine", "echo \"pid $$\"");
        let (judgement, _) = judged("[[line]]\nname = \"one\"\nargs = []\n", &pin, &engine);
        assert_eq!(judgement, Judgement::Fail);
        let masked = "[[line]]\nname = \"one\"\nargs = []\nmask = [['pid \\d+', 'pid N']]\n";
        assert_eq!(judged(masked, &pin, &engine).0, Judgement::Pass);
    }

    /// The pinned host absent, or not the pin's by its digest, fails the
    /// line and names it; neither is a skip.
    #[test]
    fn an_absent_or_drifted_pinned_host_fails_the_line() {
        let hosts = Tree::new("host-line-absent").unwrap();
        let engine = host(&hosts, "engine", "echo 1");
        let absent = hosts.join("php-cgi");
        let (judgement, note) = judged(LINE, &pin(&absent, Some("0")), &engine);
        assert_eq!(judgement, Judgement::Bork, "{note}");
        assert!(note.contains(&absent.display().to_string()), "{note}");
        let pinned = host(&hosts, "php-cgi", "echo 1");
        let (judgement, note) = judged(LINE, &pin(&pinned, Some("0")), &engine);
        assert_eq!(judgement, Judgement::Bork, "{note}");
        assert!(note.contains("the pin's [sapi] says 0"), "{note}");
    }

    /// A program that does not end on its own is cut at the line's `head`
    /// and killed there, and one that waits at its input is killed once it
    /// has written nothing for a while: both held as any line is.
    #[test]
    fn a_line_s_head_ends_a_program_that_does_not_end_on_its_own() {
        let hosts = Tree::new("host-line-head").unwrap();
        for (name, body) in [("loops", "yes x"), ("waits", "echo x; sleep 60")] {
            let pinned = host(&hosts, "php-cgi", body);
            let pin = pin(&pinned, None);
            let engine = host(&hosts, name, body);
            let text = "[[line]]\nname = \"one\"\nargs = []\nhead = 8\n";
            let (judgement, note) = judged(text, &pin, &engine);
            assert_eq!(judgement, Judgement::Pass, "{name}: {note}");
        }
        let pin = pin(&host(&hosts, "php-cgi", "yes x"), None);
        let engine = host(&hosts, "otherwise", "yes y");
        let text = "[[line]]\nname = \"one\"\nargs = []\nhead = 8\n";
        let (judgement, note) = judged(text, &pin, &engine);
        assert_eq!(judgement, Judgement::Fail, "{note}");
        assert!(note.contains("+y\n+y\n+y\n+y"), "{note}");
    }

    /// A server's port taken by another process between the probe and the
    /// host's bind is not the line's miss (#2653): the pinned host here
    /// says, once, what php -S says of a port in use and serves nothing,
    /// and the line is run again, both hosts, on another port, and passes.
    #[test]
    fn a_port_taken_before_the_host_binds_it_is_tried_again_on_another() {
        let hosts = Tree::new("host-line-taken").unwrap();
        let once = hosts.join("refused-once");
        let serve = "echo up > up; exec sleep 30";
        let pinned = host(
            &hosts,
            "php-cgi",
            &format!(
                "if [ ! -e '{0}' ]; then : > '{0}'; echo \"Failed to listen on 127.0.0.1:$1 \
                 (reason: Address already in use)\" >&2; exit 1; fi; {serve}",
                once.display()
            ),
        );
        let engine = host(&hosts, "engine", serve);
        let text = "[[line]]\nname = \"one\"\nargs = [\"{port}\"]\n[line.serve]\n\
                    listen = \"127.0.0.1:{port}\"\nready = [\"up\", \"up\"]\nstop = \"SIGTERM\"\n";
        let (judgement, note) = judged(text, &pin(&pinned, None), &engine);
        assert!(once.exists(), "the first port was refused");
        assert_eq!(judgement, Judgement::Pass, "{note}");
    }

    /// A port is in use where a host names that port, and not a longer
    /// one that begins with its digits, beside the words it is refused with.
    #[test]
    fn a_port_in_use_is_the_port_named_and_no_longer_one() {
        let dir = Tree::new("host-line-in-use").unwrap();
        let said = |port: u16| {
            format!("Failed to listen on 127.0.0.1:{port} (reason: Address already in use)")
        };
        let in_use = |port, text: &str| super::in_use(port, [text.as_bytes(), b""], &dir).unwrap();
        assert!(in_use(5000, &said(5000)));
        assert!(!in_use(5000, &said(50001)));
        assert!(!in_use(
            5000,
            "Failed to listen on 127.0.0.1:5000 (reason: Permission denied)"
        ));
    }

    /// A server's stop waits for what its `settled` file must match after
    /// the last answer (#2531): here the pinned host writes its log line
    /// some seconds after it is ready and the engine's at once, so a stop
    /// sent at the answer finds the two files apart, and one sent once the
    /// line is there finds them alike. The stop at the answer is held
    /// against a host whose line comes thirty seconds late, when the host
    /// would have ended on its own, so no stall of a loaded machine
    /// between the answer and the stop lets the line land first.
    #[test]
    fn a_server_is_stopped_once_its_file_matches_what_it_settles_on() {
        let hosts = Tree::new("host-line-settled").unwrap();
        let late = |name: &str, seconds: u32| {
            let body = format!(
                "echo up > up; (/bin/sleep {seconds}; echo done >> log) >/dev/null 2>&1 & exec /bin/sleep 30"
            );
            pin(&host(&hosts, name, &body), None)
        };
        let (unsettled, pin) = (late("php-cgi-later", 30), late("php-cgi", 3));
        let engine = host(
            &hosts,
            "engine",
            "echo up > up; echo done >> log; exec /bin/sleep 30",
        );
        let line = |settled: &str| {
            format!(
                "[[line]]\nname = \"one\"\nargs = []\n[line.serve]\nlisten = \"127.0.0.1:1\"\n\
                 ready = [\"up\", \"up\"]\n{settled}stop = \"SIGTERM\"\n"
            )
        };
        assert_eq!(judged(&line(""), &unsettled, &engine).0, Judgement::Fail);
        let settled = line("settled = [\"log\", \"^done\"]\n");
        let (judgement, note) = judged(&settled, &pin, &engine);
        assert_eq!(judgement, Judgement::Pass, "{note}");
    }

    /// A file of lines is refused whole for a name given twice, a mask
    /// that is no pattern, a request of neither kind or both, a stop that
    /// is no signal, a key the grammar does not have and a server cut at
    /// a `head`.
    #[test]
    fn a_file_the_grammar_does_not_read_is_refused() {
        for (text, said) in [
            (
                "[[line]]\nname = \"a\"\nargs = []\n[[line]]\nname = \"a\"\nargs = []\n",
                "named twice",
            ),
            (
                "[[line]]\nname = \"a\"\nargs = []\nmask = [['(', 'x']]\n",
                "mask",
            ),
            (
                "[[line]]\nname = \"a\"\nargs = []\n[line.serve]\nlisten = \"x\"\nstop = \"SIGINT\"\n[[line.serve.request]]\n",
                "one of the two",
            ),
            (
                "[[line]]\nname = \"a\"\nargs = []\n[line.serve]\nlisten = \"x\"\nstop = \"SIGNOPE\"\n",
                "stop",
            ),
            (
                "[[line]]\nname = \"a\"\nargs = []\nini = [\"x=1\"]\n",
                "unknown field",
            ),
            (
                "[[line]]\nname = \"a\"\nargs = []\nhead = 1\n[line.serve]\nlisten = \"x\"\nstop = \"SIGINT\"\n",
                "never by a `head`",
            ),
        ] {
            let refused = Held::parse(hosts::CGI, "lines.toml".into(), text).unwrap_err();
            assert!(format!("{refused:#}").contains(said), "{text}: {refused:#}");
        }
    }
}
