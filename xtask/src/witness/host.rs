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
//!                                      # (or once it waits 2s, writing nothing)
//! streams = "merged"                   # "pipes", the default, "merged" or "files"
//! mask = [['\((\d+)\): ', '(<pid>): ']]  # a pattern and what stands for each match
//! [line.serve]                         # a server: started, asked, then stopped
//! listen = "{dir}/fpm.sock"            # a path is a Unix socket, else host:port
//! ready = ["fpm.log", "ready to handle connections"]  # waited for in a file
//! stop = "SIGQUIT"                     # sent again while it stands asleep 2s after
//! settled = ["fpm.log", '(?s)started.*started']  # matched once answered, before the stop
//! [[line.serve.request]]
//! http = "GET / HTTP/1.0\r\n\r\n"      # or `params` and `body`, as FastCGI records,
//!                                     # whose connection dropped unanswered is the answer
//! shut = true                         # the client's half shut once sent
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
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use regex_lite::Regex;
use serde::Deserialize;

use super::fpm::Closed;
use super::{Judged, Judgement, Scope};
use crate::divergence::Registry;
use crate::manifest::Crate;
use crate::oracle::hosts::{self, EngineHost};
use crate::oracle::{self, Oracle, Running, Stdin, Streams, Verdict};
use crate::scratch::Tree;

/// Where a host crate keeps its lines, under the crate.
pub(crate) const LINES: &str = "tests/lines.toml";

/// How long a server has to listen, or to write what `ready` waits for,
/// and a line with a `head` its first byte.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a line with a `head` may write nothing after its last byte
/// before it is taken to wait and is killed, where no process of its group
/// is runnable then.
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
/// `FastCGI` one, and whether the client shuts its half of the connection
/// once the request is written, as a server that sends nothing more does
/// (#3457). A `FastCGI` one the server drops unanswered is answered by
/// that ([`DROPPED`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    http: Option<String>,
    params: Option<Vec<(String, String)>>,
    body: Option<String>,
    #[serde(default)]
    shut: bool,
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
        let again = ran.as_ref().is_ok_and(|(.., again)| *again);
        let (judgement, mut note) = match ran {
            Err(trouble) => (Judgement::Bork, format!("{trouble:#}")),
            Ok((theirs, ours, _)) => {
                match oracle::compare(registry, &id, theirs.as_bytes(), ours.as_bytes()) {
                    Verdict::Same => (Judgement::Pass, String::new()),
                    Verdict::Diverged(row) => (Judgement::Diverged, row.reason),
                    Verdict::Moved(moved) => (Judgement::Fail, moved.to_string()),
                    Verdict::Differs(diff) => (Judgement::Fail, diff),
                }
            }
        };
        if again {
            note = [note.as_str(), AGAIN].join(if note.is_empty() { "" } else { "\n" });
        }
        Judged::new(&id, &id, judgement, note)
    }

    /// The host these lines run.
    pub(super) fn host(&self) -> EngineHost {
        self.host
    }
}

impl Line {
    /// Whether the line is a server's, started, asked and stopped.
    pub(crate) fn serves(&self) -> bool {
        self.serve.is_some()
    }

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
/// rendering and the engine's, and whether the engine's host ended only on
/// its stop sent again ([`stopped`]).
fn both(
    oracle: &Oracle<'_>,
    pinned: &Path,
    engine: &Path,
    line: &Line,
) -> Result<(String, String, bool)> {
    let dir = Tree::new("line")?;
    let masks = line.masks()?;
    let masked = |text: String| {
        masks.iter().fold(text, |text, (re, with)| {
            re.replace_all(&text, *with).into_owned()
        })
    };
    let mut taken = Vec::new();
    for _ in 0..ATTEMPTS {
        let port = free_port()?;
        let renderings = ran(oracle, pinned, line, dir.path(), port)
            .and_then(|(theirs, _)| Ok((theirs, ran(oracle, engine, line, dir.path(), port)?)));
        match renderings {
            Err(err) if err.is::<InUse>() => taken.push(port.to_string()),
            renderings => {
                let (theirs, (ours, again)) = renderings?;
                return Ok((masked(theirs), masked(ours), again));
            }
        }
    }
    bail!(
        "line {:?}: each of the ports {} was found free and taken before a host bound it",
        line.name,
        taken.join(", ")
    )
}

/// How many ports a server is offered, a server's line here and the
/// built-in server `crosscheck` starts: a port found free can be taken by
/// another process before the host binds it, which then says so and
/// serves nothing (#2653).
pub(crate) const ATTEMPTS: usize = 3;

/// A loopback port no process holds as it is asked, which one may take
/// before the host binds it ([`ATTEMPTS`]).
pub(crate) fn free_port() -> Result<u16> {
    let probe = TcpListener::bind("127.0.0.1:0").and_then(|probe| probe.local_addr());
    Ok(probe.context("finding a free loopback port")?.port())
}

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
/// directory holds after, rendered as one text; and whether a server's
/// stop was sent again ([`stopped`]).
fn ran(
    oracle: &Oracle<'_>,
    binary: &Path,
    line: &Line,
    dir: &Path,
    port: u16,
) -> Result<(String, bool)> {
    lay(line, dir, port)?;
    let recipe = &oracle.pin().recipe;
    let limit = Duration::from_secs(recipe.limits.timeout_s);
    let stdin = line.stdin.as_deref().map(|text| fill(text, dir, port));
    let (mut out, mut again) = (String::new(), false);
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
                (None, Some(head)) => headed(running, head).map(|()| (Vec::new(), false)),
                (None, None) => Ok((Vec::new(), false)),
            }
        })?;
        if serve.is_some() && in_use(port, [&ended.stdout, &ended.stderr], dir)? {
            return Err(InUse(port).into());
        }
        let during = ended
            .during
            .with_context(|| format!("{} serving", binary.display()));
        let Some(exit) = ended.exit else {
            // A request whose answer never came waits out its minute, which
            // takes the run past the limit: what it met is the cause.
            let met = during.err().map(|e| format!(": {e:#}")).unwrap_or_default();
            bail!(
                "{} ran past the recipe's limit of {} s on line {:?}{met}",
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
        let (answers, resent) = during?;
        again |= resent;
        for (asked, answer) in answers.into_iter().enumerate() {
            writeln!(out, "== run {run} request {}\n{answer}", asked + 1).ok();
        }
    }
    for (name, bytes) in files(dir)? {
        writeln!(out, "== file {name}\n{bytes}").ok();
    }
    Ok((out, again))
}

/// How much of what a server's line saw a reading keeps, in characters.
const SEEN: usize = 1500;

/// What a server's line that missed saw, as its reading keeps it (#3846):
/// where the two hosts' renderings `differs`, the note's `==` headings and
/// the lines only one host wrote, each request's answer among them, and
/// otherwise the note whole, a refused connection's or an unread answer's
/// error; on one line, at most [`SEEN`] characters of it.
pub(crate) fn seen(note: &str, differs: bool) -> String {
    let kept = note.lines().filter(|line| {
        !differs
            || line.starts_with(['-', '+'])
            || line.get(1..).is_some_and(|l| l.starts_with("== "))
    });
    let mut said = kept.collect::<Vec<_>>().join(" | ");
    if let Some((at, _)) = said.char_indices().nth(SEEN) {
        said.truncate(at);
        said.push_str(" ...");
    }
    said
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
/// sent its stop until it ends ([`stopped`]); the answers, and whether the
/// stop was sent again. Where any of it fails the server is killed rather
/// than left to the recipe's limit.
fn served(serve: &Serve, pid: u32, dir: &Path, port: u16) -> Result<(Vec<String>, bool)> {
    let answers = asked(serve, dir, port);
    let signal = match answers {
        Ok(_) => Signal::from_str(&serve.stop)?,
        Err(_) => Signal::SIGKILL,
    };
    signalled(pid, signal)?;
    let again = signal != Signal::SIGKILL && stopped(pid, signal)?;
    Ok((answers?, again))
}

/// A server sent `signal` as its stop, sent it again while it stands
/// [`QUIET`] after the last with none of its group runnable, until it ends
/// or [`PATIENCE`] runs out; whether it was sent again.
///
/// The resend is for php-cgi's accept loop, which reads `in_shutdown` and
/// then blocks in `accept` (`Z/main/fastcgi.c:1361-1368`), so a stop
/// handled between the two is lost, and the process waits in `accept` for
/// a connection that never comes; under load that reads as a line past the
/// recipe's limit (#3846), 2 in 1000 in #3869's control. A second stop
/// interrupts the `accept` and ends it as the first would have; php-fpm's
/// master ignores a stop it is already acting on. The engine's loop ends on
/// a stop handled in that window since #3869, its `accept` failing on a
/// socket that listens on nothing, so the engine's host sent its stop again
/// has lost one it should not have, and its line's note says so
/// ([`AGAIN`]; #3879).
fn stopped(pid: u32, signal: Signal) -> Result<bool> {
    let host = Pid::from_raw(i32::try_from(pid).context("a process id past pid_t")?);
    // The host leads its group, and a group that ended and is not yet
    // reaped answers ESRCH, or EPERM on macOS.
    let ended = || matches!(killpg(host, None), Err(Errno::ESRCH | Errno::EPERM));
    let (since, mut sent, mut again) = (Instant::now(), Instant::now(), false);
    while !ended() && since.elapsed() < PATIENCE {
        if sent.elapsed() >= QUIET && !runnable(host) {
            again = kill(host, signal).is_ok() || again;
            sent = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(again)
}

/// What a server's line's note says, last, where the engine's host ended
/// only on its stop sent again ([`stopped`]): a loss its loop has not had
/// since #3869, which the line's reading names (#3879).
pub(crate) const AGAIN: &str = "the engine's host ended only on its stop sent again";

/// A program that does not end on its own, a session that loops or one
/// that waits at a prompt with nothing more to read, killed with its group
/// once it has written `head` bytes to stdout, or nothing for [`QUIET`]
/// since its last byte. Before its first it has [`PATIENCE`], a server's
/// time to be ready: on a loaded machine a program may not have run at
/// all by [`QUIET`], and one killed then reads as one that wrote nothing
/// (#3616). After it, one still runnable when [`QUIET`] runs out has not
/// reached its input, being busy or not yet scheduled, and is given a
/// further [`QUIET`] (#3779); one asleep or reading is killed there. Each
/// gap is bounded at [`PATIENCE`], and one whose group has ended is waited
/// for no longer.
fn headed(running: &Running<'_>, head: u64) -> Result<()> {
    headed_after(running, head, |quiet| quiet.elapsed() >= QUIET)
}

/// [`headed`] with the end of each quiet gap told by `passed`, which is
/// given the instant the gap began: [`QUIET`] by the clock for every line,
/// and for the test of a program asleep through the gap, the moment `ps`
/// reads it stopped, so no margin of time orders the reading and the
/// program's next write (#4436).
fn headed_after(
    running: &Running<'_>,
    head: u64,
    mut passed: impl FnMut(Instant) -> bool,
) -> Result<()> {
    let group = Pid::from_raw(i32::try_from(running.pid).context("a process id past pid_t")?);
    // A program that ended on its own leaves a group of processes not yet
    // reaped, which answers ESRCH, or EPERM on macOS; its ending is held
    // as any line's is.
    let ended = |sent| matches!(sent, Err(Errno::ESRCH | Errno::EPERM));
    let (mut written, mut since, mut quiet) = (running.written(), Instant::now(), Instant::now());
    while written < head && since.elapsed() < PATIENCE && !ended(killpg(group, None)) {
        if written > 0 && passed(quiet) {
            if !runnable(group) {
                break;
            }
            quiet = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(10));
        let now = running.written();
        if now != written {
            (written, since, quiet) = (now, Instant::now(), Instant::now());
        }
    }
    match killpg(group, Signal::SIGKILL) {
        Err(e) if !ended(Err(e)) => Err(e).with_context(|| format!("killing the group {group}")),
        _ => Ok(()),
    }
}

/// Whether a process of `group` is runnable, or in an uninterruptible
/// wait, as `ps` reads its state (`R`, `D` on Linux, `U` on macOS); one
/// asleep, reading its input or stopped is not, and none is where `ps`
/// does not answer within [`QUIET`], which is then killed.
fn runnable(group: Pid) -> bool {
    let Ok(mut ps) = Command::new("ps")
        .args(["-A", "-o", "pgid=,stat="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let asked = Instant::now();
    while matches!(ps.try_wait(), Ok(None)) && asked.elapsed() < QUIET {
        std::thread::sleep(Duration::from_millis(10));
    }
    ps.kill().ok();
    let listed = match ps.wait_with_output() {
        Ok(listed) if listed.status.success() => listed.stdout,
        _ => return false,
    };
    let group = group.to_string();
    String::from_utf8_lossy(&listed).lines().any(|row| {
        let mut row = row.split_whitespace();
        row.next() == Some(group.as_str())
            && row.next().is_some_and(|s| s.starts_with(['R', 'D', 'U']))
    })
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
    for (n, request) in serve.request.iter().enumerate() {
        let answer = loop {
            match answered(request, &listen, &|text| fill(text, dir, port)) {
                Err(refused) if refused.is::<Refused>() && started.elapsed() < PATIENCE => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => {
                    break other.with_context(|| {
                        let at = started.elapsed().as_secs_f32();
                        format!("request {} at {listen}, {at:.1}s after the start", n + 1)
                    })?;
                }
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
/// `FCGI_STDOUT` with its `FCGI_STDERR` after, and [`DROPPED`] last where
/// the server ended the connection before its `FCGI_END_REQUEST`.
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
            let written = super::fpm::request(pairs, body);
            // A path is a Unix socket, and anything else `host:port`.
            let round = if listen.starts_with('/') {
                let stream = UnixStream::connect(listen).map_err(Refused)?;
                stream.set_read_timeout(wait)?;
                fastcgi_round(stream, &written, request.shut, UnixStream::shutdown)
            } else {
                let stream = TcpStream::connect(listen).map_err(Refused)?;
                stream.set_read_timeout(wait)?;
                fastcgi_round(stream, &written, request.shut, TcpStream::shutdown)
            };
            let ((stdout, stderr), dropped) = match round {
                Ok(streams) => (streams, ""),
                Err(error) => {
                    let closed = error.downcast::<Closed>()?;
                    ((closed.stdout, closed.stderr), DROPPED)
                }
            };
            Ok(format!(
                "{}\n-- stderr\n{}{dropped}",
                shown(&stdout),
                shown(&stderr)
            ))
        }
        (None, None) => bail!("a request is `http` or `params`"),
    }
}

/// What a `FastCGI` request's answer ends with where the server ended the
/// connection, closed or reset, before its `FCGI_END_REQUEST`
/// ([`Closed`], #4397), after what it wrote in whole records until then.
/// It names neither a close nor a reset, nor whether a read met it or a
/// write: php drops a peer its `listen.allowed_clients` does not name
/// with a bare `close` (`Z/main/fastcgi.c:1401-1405`), which resets the
/// connection where the request has arrived unread and ends it cleanly
/// where it has not, so which one a client meets is the two processes'
/// timing and no behaviour of the host's.
const DROPPED: &str = "\n-- closed before FCGI_END_REQUEST";

/// One `FastCGI` request's records written to `stream`, the client's half
/// shut after them where the line asks, and the answer read; a write or a
/// shut the server refused is the answer's to say, as
/// [`sent`](super::fpm::sent) takes it.
fn fastcgi_round<S: Read + Write>(
    mut stream: S,
    written: &[u8],
    shut: bool,
    shutdown: fn(&S, std::net::Shutdown) -> std::io::Result<()>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let half = std::net::Shutdown::Write;
    let shut = |stream: &S| if shut { shutdown(stream, half) } else { Ok(()) };
    super::fpm::sent(stream.write_all(written).and_then(|()| shut(&stream)))?;
    super::fpm::answer(stream)
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use nix::sys::signal::Signal;

    use super::{Held, Judgement, hosts, signalled};
    use crate::divergence::Registry;
    use crate::oracle::{Host, Pin, Promise, Stdin, Streams};
    use crate::scratch::Tree;

    /// The tracked pin, whole.
    fn tracked() -> Pin {
        let root = crate::package_dir().join("..");
        Pin::parse(&fs::read_to_string(root.join("oracle.toml")).unwrap()).unwrap()
    }

    /// Whether the tracked pin's php is on this machine, which a line
    /// judged under the pin whole reads beside its stand-in host; a skip
    /// where this machine did not promise it, and a failure where it did
    /// (#38, #3249).
    fn oracle() -> bool {
        let pin = tracked();
        Promise::Oracle
            .or_skip(pin.oracle.file().map(Path::to_path_buf))
            .unwrap()
            .is_some()
    }

    /// The tracked pin with `php-cgi` standing at `cgi`, pinned by the
    /// digest given, the file's own where none is.
    fn pin(cgi: &Path, md5: Option<&str>) -> Pin {
        let mut pin = tracked();
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
        if !oracle() {
            return;
        }
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
        if !oracle() {
            return;
        }
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
        if !oracle() {
            return;
        }
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
    /// has written nothing for a while: both held as any line is. One whose
    /// first byte comes later than [`super::QUIET`] after it starts, as a
    /// program the machine has not yet run, keeps it (#3616); one that
    /// writes nothing at all is killed all the same, and fails where the
    /// other host wrote; one that ends on its own is not waited for. One
    /// runnable through a gap past [`super::QUIET`] between two writes, as
    /// a program busy or starved, keeps its next write (#3779), one asleep
    /// through it is cut there whatever the machine's load, its gap an
    /// event and not a margin of time ([`asleep`], #4436), and one that
    /// spins for good is still killed.
    #[test]
    fn a_line_s_head_ends_a_program_that_does_not_end_on_its_own() {
        if !oracle() {
            return;
        }
        let hosts = Tree::new("host-line-head").unwrap();
        let (told, stdout) = asleep(&hosts);
        assert_ne!(
            told,
            Told::Waiting,
            "asleep: never read stopped: {stdout:?}"
        );
        assert_eq!((told, stdout.as_str()), (Told::Passed, "x\n"), "asleep");
        let text = "[[line]]\nname = \"one\"\nargs = []\nhead = 8\n";
        const { assert!(super::QUIET.as_secs() + 1 < super::PATIENCE.as_secs()) };
        let gap = super::QUIET.as_secs() + 1;
        let late = format!("sleep {gap}; echo x; sleep 60");
        let busy = format!(
            "echo x; yes >/dev/null & s=$!; sleep {gap}; kill $s; wait $s 2>/dev/null; echo y; sleep 60"
        );
        let both = "echo x; echo y; sleep 60";
        for (name, pinned, engine) in [
            ("loops", "yes x", "yes x"),
            ("waits", "echo x; sleep 60", "echo x; sleep 60"),
            ("late", "echo x; sleep 60", late.as_str()),
            ("busy", both, busy.as_str()),
        ] {
            let pin = pin(&host(&hosts, "php-cgi", pinned), None);
            let (judgement, note) = judged(text, &pin, &host(&hosts, name, engine));
            assert_eq!(judgement, Judgement::Pass, "{name}: {note}");
        }
        let (ends, began) = (
            pin(&host(&hosts, "php-cgi", "exit 3"), None),
            Instant::now(),
        );
        let (judgement, note) = judged(text, &ends, &host(&hosts, "ends", "exit 3"));
        assert_eq!(judgement, Judgement::Pass, "ends: {note}");
        assert!(began.elapsed() < super::PATIENCE, "ends: {note}");
        let (waits, began) = (
            pin(&host(&hosts, "php-cgi", "echo x; sleep 60"), None),
            Instant::now(),
        );
        let (judgement, note) = judged(
            text,
            &waits,
            &host(&hosts, "spins", "echo x; yes >/dev/null"),
        );
        assert_eq!(judgement, Judgement::Pass, "spins: {note}");
        assert!(began.elapsed() < 2 * super::PATIENCE, "spins: {note}");
        for (name, pinned, engine, differs) in [
            ("otherwise", "yes x", "yes y", "+y\n+y\n+y\n+y"),
            ("mute", "echo x; sleep 60", "sleep 60", "-x"),
        ] {
            let pin = pin(&host(&hosts, "php-cgi", pinned), None);
            let (judgement, note) = judged(text, &pin, &host(&hosts, name, engine));
            assert_eq!(judgement, Judgement::Fail, "{name}: {note}");
            assert!(note.contains(differs), "{name}: {note}");
        }
    }

    /// Where a program asleep through the gap stands when its run ends:
    /// waiting to stop, told the gap passed once it stood stopped, or
    /// continued after a reading that kept it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Told {
        Waiting,
        Passed,
        Continued,
    }

    /// A program asleep through a quiet gap, run under [`super::headed_after`]
    /// with the gap's end an event and not a time (#4436): it writes `x` and
    /// stops itself, the gap is told passed only once `ps` reads it stopped,
    /// and a host that keeps it past that reading has it continued, so it
    /// writes `y`. A host that cuts a program asleep keeps `x` alone; one
    /// that keeps it keeps `y` too, however loaded the machine.
    fn asleep(hosts: &Tree) -> (Told, String) {
        let program = host(hosts, "asleep", "echo x; kill -STOP $$; echo y; sleep 60");
        let pin = tracked();
        let limit = Duration::from_mins(1);
        let ended = pin
            .oracle()
            .to_end(
                Command::new(program),
                Streams::Pipes { merged: false },
                Stdin::Null,
                limit,
                |running| {
                    let pid = running.pid;
                    let mut told = Told::Waiting;
                    let cut = super::headed_after(running, 8, |quiet| match told {
                        Told::Waiting => {
                            if stopped(pid) {
                                told = Told::Passed;
                            }
                            told == Told::Passed
                        }
                        Told::Passed => {
                            told = Told::Continued;
                            signalled(pid, Signal::SIGCONT).unwrap();
                            false
                        }
                        Told::Continued => quiet.elapsed() >= super::QUIET,
                    });
                    cut.unwrap();
                    told
                },
            )
            .unwrap();
        (
            ended.during,
            String::from_utf8_lossy(&ended.stdout).into_owned(),
        )
    }

    /// Whether `ps` reads the process `pid` stopped (`T`).
    fn stopped(pid: u32) -> bool {
        let read = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&read.stdout)
            .trim_start()
            .starts_with('T')
    }

    /// A server's port taken by another process between the probe and the
    /// host's bind is not the line's miss (#2653): the pinned host here
    /// says, once, what php -S says of a port in use and serves nothing,
    /// and the line is run again, both hosts, on another port, and passes.
    #[test]
    fn a_port_taken_before_the_host_binds_it_is_tried_again_on_another() {
        if !oracle() {
            return;
        }
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
        if !oracle() {
            return;
        }
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

    /// A stop the server loses, as php-cgi's accept loop loses one handled
    /// between its `in_shutdown` read and its `accept` (#3846), is sent
    /// again once the server stands asleep [`super::QUIET`] after it: the
    /// engine's stand-in ends only on its second `SIGTERM` and the pinned
    /// one on its first, and the line passes well inside the recipe's
    /// limit, which it ran to while the stop was sent once. The pass's
    /// note says the engine's host was sent its stop again, a loss its
    /// loop has not had since #3869 (#3879), and a pinned host's lost stop
    /// sent again is said nowhere, php-cgi's loop losing one as it stands.
    #[test]
    fn a_stop_the_server_loses_is_sent_again() {
        if !oracle() {
            return;
        }
        let hosts = Tree::new("host-line-lost-stop").unwrap();
        let waits = "echo up > up; while :; do /bin/sleep 1 & wait $!; done";
        let ends = host(&hosts, "php-cgi", &format!("trap 'exit 0' TERM; {waits}"));
        let lost = format!("trap 'trap \"exit 0\" TERM' TERM; {waits}");
        let loses = host(&hosts, "engine", &lost);
        let text = "[[line]]\nname = \"one\"\nargs = []\n[line.serve]\nlisten = \"127.0.0.1:1\"\n\
                    ready = [\"up\", \"up\"]\nstop = \"SIGTERM\"\n";
        let started = Instant::now();
        let (judgement, note) = judged(text, &pin(&ends, None), &loses);
        assert_eq!(judgement, Judgement::Pass, "{note}");
        assert!(
            started.elapsed() < super::PATIENCE * 2,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(note, "the engine's host ended only on its stop sent again");
        let (judgement, note) = judged(text, &pin(&loses, None), &ends);
        assert_eq!((judgement, note.as_str()), (Judgement::Pass, ""));
    }

    /// A stand-in server: it listens on the loopback port its argument
    /// names, writes `up` once it does, and runs `each` over every
    /// connection `$c` it accepts before it closes it, where `whole($c)`
    /// reads a request to its empty `FCGI_STDIN` record and `record` is
    /// one record of request 1.
    fn server(hosts: &Tree, name: &str, each: &str) -> PathBuf {
        let program = format!(
            "#!/usr/bin/perl\nuse Socket;\n\
             socket(my $s, PF_INET, SOCK_STREAM, 0) or die;\n\
             setsockopt($s, SOL_SOCKET, SO_REUSEADDR, 1) or die;\n\
             bind($s, sockaddr_in($ARGV[0], INADDR_LOOPBACK)) or die;\nlisten($s, 5) or die;\n\
             sub full {{ my ($c, $n) = @_; my $b = '';\n\
               while (length($b) < $n) {{ return undef unless sysread($c, $b, $n - length($b), length($b)); }}\n\
               return $b; }}\n\
             sub whole {{ my ($c) = @_;\n\
               while (defined(my $h = full($c, 8))) {{ my ($v, $type, $id, $len, $pad) = unpack('CCnnC', $h);\n\
                 full($c, $len + $pad); last if $type == 5 && $len == 0; }} }}\n\
             sub record {{ my ($type, $content) = @_; pack('CCnnCC', 1, $type, 1, length($content), 0, 0) . $content }}\n\
             open(my $up, '>', 'up') or die; print $up \"up\\n\"; close($up);\n\
             while (1) {{ accept(my $c, $s) or next; {each}; close($c); }}\n"
        );
        let path = hosts.write(name, program).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A `FastCGI` request whose server ends the connection before
    /// `FCGI_END_REQUEST` is answered by that, and the answer is held
    /// between the two hosts (#4397). The pinned stand-in closes each
    /// connection with the request unread, as php drops a peer its
    /// `listen.allowed_clients` does not name, which the client meets as a
    /// reset or, its request longer than the socket holds, as a write
    /// refused; the engine's reads the request whole and closes, an end of
    /// file: one answer, so the line passes either way. A host that
    /// answers where the other drops fails the line, and so does one that
    /// writes a record the other does not before it drops.
    #[test]
    fn a_fastcgi_connection_dropped_unanswered_is_the_request_s_answer() {
        if !oracle() {
            return;
        }
        let hosts = Tree::new("host-line-dropped").unwrap();
        let line = |body: &str| {
            format!(
                "[[line]]\nname = \"one\"\nargs = [\"{{port}}\"]\n[line.serve]\n\
                 listen = \"127.0.0.1:{{port}}\"\nready = [\"up\", \"up\"]\nstop = \"SIGTERM\"\n\
                 [[line.serve.request]]\nparams = [[\"REQUEST_METHOD\", \"POST\"]]\nbody = \"{body}\"\n"
            )
        };
        let unread = server(&hosts, "php-cgi", "select(undef, undef, undef, 0.2)");
        let unread = pin(&unread, None);
        let read = server(&hosts, "read", "whole($c)");
        // More than a loopback connection's two buffers hold of a request
        // nobody reads, so the client's write is still under way when the
        // server closes.
        let long = "b".repeat(16 << 20);
        for (name, body) in [("reset", "b"), ("refused", long.as_str())] {
            let (judgement, note) = judged(&line(body), &unread, &read);
            assert_eq!(judgement, Judgement::Pass, "{name}: {note}");
        }
        let read = pin(&server(&hosts, "php-cgi", "whole($c)"), None);
        let dropped = "closed before FCGI_END_REQUEST";
        let wrote = "whole($c); syswrite($c, record(6, \"x\\n\"))";
        let ended = format!("{wrote}; syswrite($c, record(3, \"\\0\" x 8))");
        for (name, each, answered) in [("wrote", wrote, false), ("answered", ended.as_str(), true)]
        {
            let (judgement, note) = judged(&line("b"), &read, &server(&hosts, name, each));
            assert_eq!(judgement, Judgement::Fail, "{name}: {note}");
            assert!(note.contains("+x"), "{name}: {note}");
            let only_theirs = format!("--- {dropped}");
            assert_eq!(note.contains(&only_theirs), answered, "{name}: {note}");
        }
    }

    /// What a server's line that missed keeps for its reading (#3846): a
    /// difference's headings and changed lines, any other note whole, on
    /// one line and no longer than [`super::SEEN`] characters.
    #[test]
    fn a_server_s_line_that_missed_keeps_what_it_saw() {
        let diff = " == run 1 stdout\n \n == run 1 request 1\n-Status: 404\n+Status: 200\n same\n";
        let kept = " == run 1 stdout |  == run 1 request 1 | -Status: 404 | +Status: 200";
        assert_eq!(super::seen(diff, true), kept);
        let refused = "php-cgi serving: request 1 at 127.0.0.1:5: not listening\n- refused";
        assert_eq!(super::seen(refused, false), refused.replace('\n', " | "));
        let long = super::seen(&"x".repeat(super::SEEN * 2), false);
        assert_eq!(long.chars().count(), super::SEEN + " ...".len());
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
