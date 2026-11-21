//! The in-process adapter of design §8's `ScriptExec` seam: the engine
//! linked into the harness, one fresh engine per snippet over the
//! `Capture` sink (design §4.3 seam 4, the row that names the harness
//! among the hosts; `CONTEXT.md`, Host). The fresh-process adapter is
//! [`Oracle::run_under`](super::Oracle::run_under) beside it, and both
//! answer one [`Observation`], so what judges a witness never learns which
//! of the two served it.
//!
//! **Under the recipe.** The pin's preamble goes ahead of the snippet, so
//! the code stands on line 2 where php's own error text puts it; the
//! request is named the recipe's `script_name`, so `__FILE__` and the
//! `in %s on line %d` of a diagnostic read `witness.php` as they do under
//! the fresh-process adapter, and not `Standard input code`, the name
//! `serve_capture` gives a piped source; the file stands in a
//! scratch tree of this run's own and the request's working directory is
//! that tree, as the fresh-process adapter runs the program in its
//! directory, so an `include` of the witness's own file reads the same
//! file under both adapters and the tree's path is normalised to the
//! recipe's token where php's would be; and the recipe's ini is
//! laid over the engine's defaults, so `precision`, `error_reporting` and
//! the rest are the pin's, and a named `[regime.<name>]` of the pin lays
//! its own after them, in the order the fresh-process adapter passes the
//! two as `-d` (harness-design.md §8, ledger 55); the binary's process
//! runs under the recipe's environment, cleared and rebuilt from
//! `[recipe.env]` as the fresh-process adapter's is ([`Recipe::environ`]),
//! and its request reads that environment, so `getenv()`, `$_ENV` and the
//! C library's time code read the recipe's `TZ` and not the shell's;
//! of a regime's `args`
//! this adapter takes `-e`, building the engine with extended info as a
//! host's `-e` builds it, and refuses any other, since it has no command
//! line to hand them ([`Linked`]). An entry the engine has
//! no name for is carried and read by nobody, which is what php does with
//! `opcache.*` when opcache is not loaded.
//!
//! **The streams.** The first stream is the sink's bytes; the second is
//! `Capture`'s `stderr`, which is the CLI's second stream whole:
//! each log entry with `\n` after it, as `sapi_cli_log_message` writes,
//! and what the error stream was given, in the order the request wrote
//! them. That is the stream the fresh-process adapter reads from php's
//! fd 2, and it goes through the recipe's normalisation the same way, so
//! a `stderr` fence is held to what the engine wrote and a witness with
//! no such fence asserts the engine wrote nothing there.
//!
//! **A process of its own.** The engine is `sapi/cli`'s `lela-linked`,
//! built with the bundle's `linked` set in the lanes' profile at the
//! first snippet that asks for it, from the workspace this harness was
//! built in, and the harness links no engine of its own (harness §8,
//! revision 84): an edit under `crates/` or `ext/` rebuilds that
//! binary, never the one every hook runs. A thread that serves takes an
//! idle `lela-linked --serve`, or starts one, hands it the snippet as a
//! request of files in a directory of its own, and leaves it idle again
//! for the next, so as many run as threads serve at once and they outlive
//! one crate's pool into the next crate's; the binary builds a fresh
//! engine per snippet, the isolation a process
//! of its own gave, and writes back the streams, the status and the
//! natives the snippet entered. A snippet that forks, signals or changes
//! the process is still marked `process` and served by the `php` binary
//! (`witness::process`; revision 61).
//!
//! **A death or a panic is a failure, not the harness's end** (design §8):
//! a panic leaves the binary standing and the snippet borks on it, naming
//! the crate of the engine; a snippet that ends the binary, on an abort,
//! a stack overflow or a signal, borks naming how it ended, and the next
//! snippet is served by a binary started again.
//!
//! **A request has the recipe's limit**, as a program under the
//! fresh-process adapter has: a server that has not answered by
//! `[recipe.limits] timeout_s` is killed and reaped, never handed out
//! again, and the request answered with [`PastLimit`], the verdict that
//! adapter gives a program past it. The wait is a `poll` on the answer's
//! pipe with what is left of the limit, so an ask costs one call more
//! than the read it was, and no thread; a server dropped is given the
//! same limit to end on the end of its input before it is killed.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use super::profile::{Built, Profile};
use super::{Ended, Linked, Observation, PastLimit, Place, Program, Recipe, Regime, Scratch};
use crate::scratch::Tree;

/// The host whose binary serves the snippets, its package and binary.
const PACKAGE: &str = "php_sapi_cli";
const BINARY: &str = super::hosts::LINKED;

/// One snippet through the linked engine under `recipe`, and under
/// `regime` after it where a run names one, refused where the regime
/// carries an argument the linked engine does not take ([`Linked`]),
/// answered as the fresh-process adapter answers it: the program written
/// where that adapter writes it and served in that directory, both streams
/// normalised by the recipe's rules with the directory as the token, and
/// the exit code the request ended on, or `SIGABRT` where php's process
/// would have ended on `abort()`.
pub(crate) fn serve(
    recipe: &Recipe,
    regime: Option<&Regime>,
    snippet: &str,
) -> Result<Observation> {
    serve_by(None, recipe, regime, snippet).map(|(observed, ..)| observed)
}

/// [`serve`] by `server`, a binary standing for `lela-linked` that a test
/// hands a lane to serve through one that misbehaves, or the one
/// built from the workspace where none is named; with the natives the
/// request entered beside what it observed, in the order it entered them,
/// one per line as the trace file holds them: what witness coverage counts
/// (harness §4.5 condition 2, revision 58); and the profile of the build
/// that served it, none for a `server` the caller named.
pub(crate) fn serve_by(
    server: Option<&Path>,
    recipe: &Recipe,
    regime: Option<&Regime>,
    snippet: &str,
) -> Result<(Observation, String, Option<Profile>)> {
    let scratch = Scratch::for_program(recipe, Program::Snippet(snippet), Place::Named)?;
    // The engine has no command line here, so a regime's arguments other
    // than `-e` are refused rather than dropped from this side of the
    // comparison alone. The witness lane reads this before anything runs,
    // naming the regime (`witness::under`); this is the adapter's own
    // backstop.
    let linked = Linked::of(regime)?;
    // The recipe's entries, the regime's after them, so an entry both name
    // is the regime's, as the later `-d` is under the fresh-process
    // adapter; the binary lays both over the CLI's own layer.
    let ini: String = recipe
        .ini
        .iter()
        .chain(&linked.ini)
        .flat_map(|entry| [entry.as_str(), "\n"])
        .collect();
    let request = Tree::new("linked-request")?;
    let write = |name: &str, text: &[u8]| {
        fs::write(request.join(name), text).with_context(|| format!("writing the request's {name}"))
    };
    write("source", format!("{}{snippet}", recipe.preamble).as_bytes())?;
    write("script_name", recipe.script_name.as_bytes())?;
    write("cwd", scratch.dir.as_os_str().as_encoded_bytes())?;
    write("ini", ini.as_bytes())?;
    write("extended", if linked.extended_info { b"1" } else { b"0" })?;
    let (binary, profile) = match server {
        Some(path) => (path.to_path_buf(), None),
        None => binary().map(|built| (built.path, Some(built.profile)))?,
    };
    let started = Instant::now();
    if let Answer::PastLimit = ask(&request, &binary, recipe)? {
        return Err(PastLimit {
            binary,
            script: scratch.script.clone(),
            limit_s: recipe.limits.timeout_s,
            stopped: crate::scratch::stop_naming(&scratch.script),
        }
        .into());
    }
    let wall = started.elapsed();
    if request.join("panicked").exists() {
        bail!("the linked engine panicked; the panic itself is on the harness's second stream");
    }
    let read = |name: &str| fs::read(request.join(name)).unwrap_or_default();
    let exit = String::from_utf8_lossy(&read("exit"))
        .trim()
        .parse()
        .context("the linked engine wrote no status")?;
    // The request ended where php's process ends on `abort()`, which the
    // binary, serving on, records beside its status.
    let aborted = request.join("abort").exists();
    let observed = Observation {
        stdout: scratch.normalise(&recipe.normalise, read("stdout")),
        stderr: scratch.normalise(&recipe.normalise, read("stderr")),
        exit: if aborted {
            Ended::Signal(nix::sys::signal::Signal::SIGABRT as i32)
        } else {
            Ended::Status(exit)
        },
        wall,
    };
    let entered = String::from_utf8_lossy(&read("entered")).into_owned();
    Ok((observed, entered, profile))
}

/// The binaries standing idle, each returned after the snippet it served:
/// as many run as threads serve at once, and they outlive the pool of one
/// crate's fences into the next crate's.
static IDLE: Mutex<Vec<Server>> = Mutex::new(Vec::new());

/// What a server that went on standing made of a request: served, or
/// unanswered at the recipe's limit, when it was killed and reaped.
enum Answer {
    Served,
    PastLimit,
}

/// The request at `dir` served by an idle `binary` started under
/// `recipe`'s environment and limit, or one started so where none is
/// idle; one the request ended or the limit killed is dropped, so the
/// next starts afresh.
fn ask(dir: &Path, binary: &Path, recipe: &Recipe) -> Result<Answer> {
    let environ = Environ::of(recipe);
    let limit = Duration::from_secs(recipe.limits.timeout_s);
    let idle = {
        let mut idle = crate::locked(&IDLE);
        let at = idle.iter().position(|server| {
            server.binary == binary && server.environ == environ && server.limit == limit
        });
        at.map(|at| idle.swap_remove(at))
    };
    let mut server = match idle {
        Some(server) => server,
        None => Server::start(binary.to_path_buf(), recipe, environ)?,
    };
    let answer = server.ask(dir)?;
    if let Answer::Served = answer {
        crate::locked(&IDLE).push(server);
    }
    Ok(answer)
}

/// The environment a server's process was started under, what every
/// request it serves reads: a server serves only a recipe that names the
/// same, and the pool keeps it for the next such snippet at no cost.
#[derive(PartialEq, Eq)]
struct Environ {
    clear: bool,
    env: BTreeMap<String, String>,
}

impl Environ {
    fn of(recipe: &Recipe) -> Environ {
        Environ {
            clear: recipe.env_clear,
            env: recipe.env.clone(),
        }
    }
}

/// A running `lela-linked --serve`, the binary it runs, the environment
/// it runs under and the recipe's limit it serves under, and the two ends
/// of its pipes.
struct Server {
    binary: PathBuf,
    environ: Environ,
    limit: Duration,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
}

impl Server {
    fn start(binary: PathBuf, recipe: &Recipe, environ: Environ) -> Result<Server> {
        let mut child = recipe
            .environ(Command::new(&binary).arg("--serve"))?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {BINARY}"))?;
        let stdin = child.stdin.take().context("the server's stdin")?;
        let stdout = child.stdout.take().context("the server's stdout")?;
        Ok(Server {
            binary,
            environ,
            limit: Duration::from_secs(recipe.limits.timeout_s),
            child,
            stdin: Some(stdin),
            stdout,
        })
    }

    /// The request at `dir` handed over and its answer awaited within the
    /// limit, past which the server is killed and reaped; the binary's
    /// end, where it ends instead, is the error.
    fn ask(&mut self, dir: &Path) -> Result<Answer> {
        let handed = self
            .stdin
            .as_mut()
            .context("the server's stdin is closed")
            .and_then(|stdin| {
                writeln!(stdin, "{}", dir.display())
                    .and_then(|()| stdin.flush())
                    .context("handing the server its request")
            });
        match handed.and_then(|()| self.answer()) {
            Ok(Some(line)) if line.trim_ascii() == b"served" => return Ok(Answer::Served),
            Ok(None) => return self.killed().map(|()| Answer::PastLimit),
            _ => {}
        }
        Err(match self.ended()? {
            Some(status) => match status.signal() {
                Some(signal) => anyhow!("the linked engine died on signal {signal} serving it"),
                None => anyhow!("the linked engine ended serving it, {status}"),
            },
            None => anyhow!(
                "the linked engine stopped answering and stood past the recipe's limit of {} s, and was killed",
                self.limit.as_secs()
            ),
        })
    }

    /// The server's answer, read as it comes and for no longer than the
    /// limit: a line, what came before its stdout ended, or none where
    /// the limit passed first. The server writes one line a request and
    /// nothing after it until the next, so no read takes past the line.
    fn answer(&mut self) -> Result<Option<Vec<u8>>> {
        let until = Instant::now() + self.limit;
        let mut line = Vec::new();
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let wait =
                PollTimeout::try_from(left.as_micros().div_ceil(1000)).unwrap_or(PollTimeout::MAX);
            match poll(
                &mut [PollFd::new(self.stdout.as_fd(), PollFlags::POLLIN)],
                wait,
            ) {
                Ok(0) if left.is_zero() => return Ok(None),
                Ok(0) | Err(Errno::EINTR) => continue,
                Ok(_) => {}
                Err(errno) => return Err(errno).context("waiting for the server's answer"),
            }
            let mut chunk = [0u8; 256];
            let n = match self.stdout.read(&mut chunk) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("reading the server's answer"),
            };
            line.extend_from_slice(&chunk[..n]);
            if n == 0 || line.ends_with(b"\n") {
                return Ok(Some(line));
            }
        }
    }

    /// The server killed and reaped, so it is gone and not a zombie.
    fn killed(&mut self) -> Result<()> {
        self.stdin = None;
        self.child.kill().context("killing the linked engine")?;
        self.child.wait().context("reaping the linked engine")?;
        Ok(())
    }

    /// The server ended on the end of its input, its status awaited for
    /// no longer than the limit, past which it is [`killed`](Self::killed)
    /// and there is none.
    fn ended(&mut self) -> Result<Option<ExitStatus>> {
        self.stdin = None;
        let until = Instant::now() + self.limit;
        loop {
            let status = self
                .child
                .try_wait()
                .context("waiting for the linked engine")?;
            if status.is_some() {
                return Ok(status);
            }
            if Instant::now() >= until {
                return self.killed().map(|()| None);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // End of input is the binary's end, and a binary that does not
        // take it is killed at the limit; either way it is reaped, so no
        // zombie is kept.
        self.ended().ok();
    }
}

/// The binary as [`binary`] built it, once this process has.
static BUILT: OnceLock<Result<Built, String>> = OnceLock::new();

/// The binary, built at the first snippet of this process from the
/// workspace this harness was built in, in the lanes' profile, and that
/// profile.
fn binary() -> Result<Built> {
    BUILT
        .get_or_init(|| {
            super::profile::build(&crate::built_root(), PACKAGE, BINARY, Some("linked"), None)
                .map_err(|e| format!("{e:#}"))
        })
        .clone()
        .map_err(|why| anyhow!("{why}"))
}

/// The profile the linked engine's build says it was built in, once this
/// process has built it to serve a snippet: what a reading of what it
/// served names ([`super::Runner::profile`]). None before the first
/// snippet and where the build failed; a server a caller named is no
/// build, and [`serve_by`] answers none for it whatever this says.
pub(crate) fn profile() -> Option<Profile> {
    BUILT.get()?.as_ref().ok().map(|built| built.profile)
}

#[cfg(test)]
mod bound_tests {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::super::{Limits, PastLimit, Recipe};
    use super::{Environ, Server};
    use crate::scratch::Tree;

    /// A recipe whose limit is one second, the least it can name.
    fn recipe() -> Recipe {
        Recipe {
            script_name: "witness.php".into(),
            limits: Limits { timeout_s: 1 },
            ..Recipe::default()
        }
    }

    /// A stand-in for `lela-linked --serve` in `tree`: a shell that writes
    /// its pid beside itself, reads one request and then does as `then`
    /// says, which never reads its input again.
    fn stand_in(tree: &Tree, then: &str) -> PathBuf {
        let path = tree.join("server");
        let script = format!("#!/bin/sh\necho $$ > \"$0.pid\"\nread request\n{then}\n");
        std::fs::write(&path, script).unwrap();
        crate::scratch::executable(&path).unwrap();
        path
    }

    /// The stand-in's process, from the pid it wrote: gone once it is
    /// killed and reaped, since a zombie still answers signal 0.
    fn standing(server: &std::path::Path) -> bool {
        let pid = std::fs::read_to_string(server.with_extension("pid")).unwrap();
        let pid = nix::unistd::Pid::from_raw(pid.trim().parse().unwrap());
        nix::sys::signal::kill(pid, None).is_ok()
    }

    /// `f` run on a thread of its own, and what it returned with how long
    /// it took, or none where it had not returned by `within`.
    fn within<T: Send + 'static>(
        within: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<(T, Duration)> {
        let (tell, told) = mpsc::channel();
        let started = Instant::now();
        thread::spawn(move || tell.send(f()).ok());
        told.recv_timeout(within)
            .ok()
            .map(|returned| (returned, started.elapsed()))
    }

    /// A server that reads the request and never answers is killed and
    /// reaped at the recipe's limit, and the request answered as the
    /// fresh-process adapter answers a program past it.
    #[test]
    fn a_request_never_answered_is_past_the_limit_and_its_server_killed() {
        let tree = Tree::new("linked-bound").unwrap();
        let server = stand_in(&tree, "exec sleep 30");
        let at = server.clone();
        let (answered, took) = within(Duration::from_secs(15), move || {
            super::serve_by(Some(&at), &recipe(), None, "echo 1;").map(|_| ())
        })
        .expect("the request was not ended within a bound of the limit");
        let why = answered.expect_err("a request never answered is not served");
        let past = why
            .downcast_ref::<PastLimit>()
            .unwrap_or_else(|| panic!("the verdict is {why:#} and not PastLimit"));
        assert_eq!((past.binary.as_path(), past.limit_s), (server.as_path(), 1));
        assert!(
            took >= Duration::from_secs(1),
            "ended before the limit, at {took:?}"
        );
        assert!(!standing(&server), "the server past the limit still stands");
    }

    /// A server that answers and then ignores the end of its input is
    /// killed as it drops, once the limit has passed.
    #[test]
    fn a_server_that_outlives_its_input_is_killed_as_it_drops() {
        let tree = Tree::new("linked-drop").unwrap();
        let server = stand_in(&tree, "echo served\nexec sleep 30");
        let recipe = recipe();
        let mut started = Server::start(server.clone(), &recipe, Environ::of(&recipe)).unwrap();
        started.ask(tree.path()).unwrap();
        let ((), took) = within(Duration::from_secs(15), move || drop(started))
            .expect("the server's drop did not end within a bound of the limit");
        assert!(
            took >= Duration::from_secs(1),
            "killed before the limit, at {took:?}"
        );
        assert!(!standing(&server), "the dropped server still stands");
    }
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// A server holds no fd beyond the three it is handed:
    /// one started holding the writing end of another's pipe as fd 3, as a
    /// server spawned while another thread's pipe is not yet close-on-exec
    /// holds it, lets that pipe reach its end while it stands idle.
    #[test]
    fn a_server_lets_go_of_a_pipe_it_inherited_beyond_its_streams() {
        let binary = super::binary().unwrap().path;
        let (mut reader, writer) = std::io::pipe().unwrap();
        let mut server = Command::new("/bin/sh")
            .args(["-c", "exec \"$0\" --serve 3>&1 >/dev/null"])
            .arg(&binary)
            .stdin(Stdio::piped())
            .stdout(writer)
            .spawn()
            .unwrap();
        let (tell, ended) = mpsc::channel();
        thread::spawn(move || tell.send(reader.read_to_end(&mut Vec::new()).is_ok()));
        let reached = ended.recv_timeout(Duration::from_secs(10));
        let standing = server.try_wait().unwrap().is_none();
        drop(server.stdin.take());
        server.wait().unwrap();
        assert_eq!(reached, Ok(true), "the idle server still holds the pipe");
        assert!(standing, "the server ended instead of standing idle");
    }
}
