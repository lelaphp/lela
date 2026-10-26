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
//! `serve_capture` gives a piped source (#293); the file stands in a
//! scratch tree of this run's own and the request's working directory is
//! that tree, as the fresh-process adapter runs the program in its
//! directory, so an `include` of the witness's own file reads the same
//! file under both adapters and the tree's path is normalised to the
//! recipe's token where php's would be (#396); and the recipe's ini is
//! laid over the engine's defaults, so `precision`, `error_reporting` and
//! the rest are the pin's, and a named `[regime.<name>]` of the pin lays
//! its own after them, in the order the fresh-process adapter passes the
//! two as `-d` (harness-design.md §8, ledger 55); the binary's process
//! runs under the recipe's environment, cleared and rebuilt from
//! `[recipe.env]` as the fresh-process adapter's is ([`Recipe::environ`]),
//! and its request reads that environment, so `getenv()`, `$_ENV` and the
//! C library's time code read the recipe's `TZ` and not the shell's
//! (#1077, #1130); of a regime's `args`
//! this adapter takes `-e`, building the engine with extended info as a
//! host's `-e` builds it, and refuses any other, since it has no command
//! line to hand them (#1780, #1822, [`Linked`]). An entry the engine has
//! no name for is carried and read by nobody, which is what php does with
//! `opcache.*` when opcache is not loaded.
//!
//! **The streams.** The first stream is the sink's bytes; the second is
//! `Capture`'s `stderr`, which is the CLI's second stream whole (#216):
//! each log entry with `\n` after it, as `sapi_cli_log_message` writes,
//! and what the error stream was given, in the order the request wrote
//! them. That is the stream the fresh-process adapter reads from php's
//! fd 2, and it goes through the recipe's normalisation the same way, so
//! a `stderr` fence is held to what the engine wrote and a witness with
//! no such fence asserts the engine wrote nothing there (#248).
//!
//! **A process of its own.** The engine is `sapi/cli`'s `php-linked`,
//! built with the bundle's `linked` set in the lanes' profile at the
//! first snippet that asks for it, from the workspace this harness was
//! built in, and the harness links no engine of its own (harness §8,
//! revision 84; #2166): an edit under `crates/` or `ext/` rebuilds that
//! binary, never the one every hook runs. A thread that serves takes an
//! idle `php-linked --serve`, or starts one, hands it the snippet as a
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

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};

use super::{Ended, Linked, Observation, Place, Program, Recipe, Regime, Scratch};
use crate::scratch::Tree;

/// The host whose binary serves the snippets, its package and binary.
const PACKAGE: &str = "php_sapi_cli";
const BINARY: &str = "php-linked";

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
    serve_by(None, recipe, regime, snippet).map(|(observed, _)| observed)
}

/// [`serve`] by `server`, a binary standing for `php-linked` that a test
/// hands a lane to serve through one that misbehaves (#1069), or the one
/// built from the workspace where none is named; with the natives the
/// request entered beside what it observed, in the order it entered them,
/// one per line as the trace file holds them: what witness coverage counts
/// (harness §4.5 condition 2, revision 58).
pub(crate) fn serve_by(
    server: Option<&Path>,
    recipe: &Recipe,
    regime: Option<&Regime>,
    snippet: &str,
) -> Result<(Observation, String)> {
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
    let started = Instant::now();
    ask(&request, server, recipe)?;
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
    // binary, serving on, records beside its status (#3515).
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
    Ok((
        observed,
        String::from_utf8_lossy(&read("entered")).into_owned(),
    ))
}

/// The binaries standing idle, each returned after the snippet it served:
/// as many run as threads serve at once, and they outlive the pool of one
/// crate's fences into the next crate's.
static IDLE: Mutex<Vec<Server>> = Mutex::new(Vec::new());

/// The request at `dir` served by an idle `server`, the built binary
/// where none is named, started under `recipe`'s environment, or one
/// started so where none is idle; one the request ended is dropped, so
/// the next starts afresh.
fn ask(dir: &Path, server: Option<&Path>, recipe: &Recipe) -> Result<()> {
    let binary = match server {
        Some(path) => path.to_path_buf(),
        None => binary()?,
    };
    let environ = Environ::of(recipe);
    let idle = {
        let mut idle = crate::locked(&IDLE);
        let at = idle
            .iter()
            .position(|server| server.binary == binary && server.environ == environ);
        at.map(|at| idle.swap_remove(at))
    };
    let mut server = match idle {
        Some(server) => server,
        None => Server::start(binary, recipe, environ)?,
    };
    server.ask(dir)?;
    crate::locked(&IDLE).push(server);
    Ok(())
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

/// A running `php-linked --serve`, the binary it runs and the environment
/// it runs under, and the two ends of its pipes.
struct Server {
    binary: PathBuf,
    environ: Environ,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
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
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    /// The request at `dir` handed over and its answer awaited; the
    /// binary's end, where it ends instead, is the error.
    fn ask(&mut self, dir: &Path) -> Result<()> {
        let mut line = String::new();
        let handed = self
            .stdin
            .as_mut()
            .context("the server's stdin is closed")
            .and_then(|stdin| {
                writeln!(stdin, "{}", dir.display())
                    .and_then(|()| stdin.flush())
                    .context("handing the server its request")
            });
        let answered = handed.and_then(|()| {
            self.stdout
                .read_line(&mut line)
                .context("reading the server's answer")
        });
        if matches!(answered, Ok(n) if n > 0) && line.trim() == "served" {
            return Ok(());
        }
        self.stdin = None;
        let status = self.child.wait().context("waiting for the linked engine")?;
        Err(match status.signal() {
            Some(signal) => anyhow!("the linked engine died on signal {signal} serving it"),
            None => anyhow!("the linked engine ended serving it, {status}"),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // End of input is the binary's end; waiting keeps no zombie.
        self.stdin = None;
        self.child.wait().ok();
    }
}

/// The binary, built at the first snippet of this process from the
/// workspace this harness was built in, in the lanes' profile.
fn binary() -> Result<PathBuf> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            super::profile::build(&crate::built_root(), PACKAGE, BINARY, Some("linked"), None)
                .map_err(|e| format!("{e:#}"))
        })
        .clone()
        .map_err(|why| anyhow!("{why}"))
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// A server holds no fd beyond the three it is handed (#2550):
    /// one started holding the writing end of another's pipe as fd 3, as a
    /// server spawned while another thread's pipe is not yet close-on-exec
    /// holds it, lets that pipe reach its end while it stands idle.
    #[test]
    fn a_server_lets_go_of_a_pipe_it_inherited_beyond_its_streams() {
        let binary = super::binary().unwrap();
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
