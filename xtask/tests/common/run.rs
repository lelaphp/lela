//! The instrument binary run over a scratch root, as an integration test
//! runs it: `xtask(root, args)` for the common case, [`Run`] for a test
//! that sets the environment, and [`Ran`] for what came back, which prints
//! its command line, exit and both streams when it is not what the test
//! asked for. The binary is `CARGO_BIN_EXE_xtask`, which cargo gives only
//! an integration test, so the unit tests under `src` never include this.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod run; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use std::ffi::OsStr;
use std::fmt;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

/// One run of the binary to be made: `--root` and the arguments, the
/// environment beside the test's own, and what it reads on stdin, which
/// is closed where the test gives it nothing.
#[derive(Debug)]
pub struct Run {
    command: Command,
    stdin: Option<Vec<u8>>,
}

impl Run {
    /// The binary over `root`, as `xtask --root <root>`.
    pub fn at(root: &Path) -> Run {
        let mut command = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")));
        command.arg("--root").arg(root);
        Run {
            command,
            stdin: None,
        }
    }

    /// `args` after the ones already given.
    pub fn args<I, S>(mut self, args: I) -> Run
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    /// `key` set to `value` in the binary's environment.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Run {
        self.command.env(key, value);
        self
    }

    /// `key` taken out of the binary's environment, which the test's
    /// process may carry.
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Run {
        self.command.env_remove(key);
        self
    }

    /// The binary started in `dir`, where it is not the test's own.
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Run {
        self.command.current_dir(dir);
        self
    }

    /// `text` on the binary's stdin, as a hook hands it its event.
    pub fn stdin(mut self, text: impl AsRef<[u8]>) -> Run {
        self.stdin = Some(text.as_ref().to_vec());
        self
    }

    /// The run, to its end.
    pub fn go(mut self) -> Ran {
        let line = line(&self.command);
        let started = self
            .command
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = started.unwrap_or_else(|err| panic!("running {line}: {err}"));
        if let (Some(text), Some(mut pipe)) = (self.stdin, child.stdin.take()) {
            // A binary that exits before reading all of it closes the
            // pipe; what it said is the test's, not the write's.
            pipe.write_all(&text).ok();
        }
        let out = child
            .wait_with_output()
            .unwrap_or_else(|err| panic!("waiting on {line}: {err}"));
        Ran {
            line,
            status: out.status,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// `xtask --root <root> <args>`, run to its end.
pub fn xtask(root: &Path, args: &[&str]) -> Ran {
    Run::at(root).args(args).go()
}

/// What one run came back with: its command line, how it ended and both
/// streams whole.
#[derive(Debug)]
pub struct Ran {
    pub line: String,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// Its stdout, the run having exited 0; any other end fails the test
    /// with the run printed.
    pub fn ok(self) -> String {
        assert!(self.status.success(), "expected a success: {self}");
        self.stdout
    }

    /// Its stderr, the run having exited with a code other than 0; a
    /// success, or a signal, fails the test with the run printed.
    pub fn failed(self) -> String {
        assert!(
            self.status.code().is_some_and(|code| code != 0),
            "expected a failure: {self}"
        );
        self.stderr
    }

    /// The code it exited with; a run a signal ended has none, and fails
    /// the test naming the signal.
    pub fn code(&self) -> i32 {
        self.status
            .code()
            .unwrap_or_else(|| panic!("no exit code: {self}"))
    }
}

impl fmt::Display for Ran {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let end = match (self.status.code(), self.status.signal()) {
            (Some(code), _) => format!("exit {code}"),
            (None, Some(signal)) => format!("killed by signal {signal}"),
            (None, None) => self.status.to_string(),
        };
        write!(
            f,
            "{}\n{end}\n--- stdout\n{}\n--- stderr\n{}",
            self.line, self.stdout, self.stderr
        )
    }
}

/// The command line `command` runs, each word as the shell would read it
/// back where it holds no space.
fn line(command: &Command) -> String {
    let words = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(quoted);
    let mut line: Vec<String> = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| format!("{}={}", key.to_string_lossy(), quoted(value)))
        })
        .collect();
    line.extend(words);
    line.join(" ")
}

fn quoted(word: &OsStr) -> String {
    let word = word.to_string_lossy();
    if word.is_empty() || word.contains(char::is_whitespace) {
        format!("'{word}'")
    } else {
        word.into_owned()
    }
}
