//! The requests phpdbg serves (`Z/sapi/phpdbg/phpdbg.c:1159-1767`): php
//! keeps one request standing and restarts it after every run, its
//! execution context compiled again each time; this host serves the
//! same cycle as requests of the engine's. The context is served held at
//! its first opline for the prompt before `run`, and `run` goes on in
//! that request; a run ends with its request, and the next is held again.
//! A command outside any request that needs one standing, `ev` with no
//! context, is served a request of its own with no program of the
//! context's.

use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use php_sapi::{Engine, FileHandles, Observables, Primary, RequestDescription};

use crate::cmd::Command;
use crate::out::{self, Sink};
use crate::prompt::{self, Here};
use crate::session::{self, AfterRun, Context, Plan, Session, Stop};
use crate::{NAME, input};

/// The session's requests, until it quits; the status of the last.
pub fn serve(engine: &Engine) -> i32 {
    let mut status = 0;
    loop {
        let Some(mut s) = session::take() else {
            return status;
        };
        let plan = match s.stopped.take() {
            Some(Stop::Quit) => {
                s.stopped = Some(Stop::Quit);
                session::put(s);
                return status;
            }
            Some(Stop::Restart(args) | Stop::RunWith(args)) => {
                if let Some(args) = args {
                    set_arguments(&mut s, &args);
                }
                Some(Plan::Run)
            }
            Some(Stop::Idle) => Some(Plan::Idle),
            Some(Stop::Clean | Stop::Exec) | None => {
                (s.exec.is_some() && s.context != Context::Refused).then_some(Plan::Hold)
            }
        };
        let Some(plan) = plan else {
            let mut here = Here::Outside;
            prompt::interactive(&mut s, &mut here);
            session::put(s);
            continue;
        };
        if plan == Plan::Run {
            prompt::begin_run(&mut s);
        }
        s.plan = Some(plan);
        s.standing = None;
        s.thrown_into = None;
        let description = describe(&s, plan);
        session::put(s);
        let observed = engine.serve(description, &mut Sink);
        let Some(mut s) = session::take() else {
            return status;
        };
        status = ended(&mut s, plan, &observed).unwrap_or(status);
        session::put(s);
    }
}

/// After a request: the context marked as one that does not compile
/// where its held request never reached its first opline, and a run
/// that ended by itself reported as php reports it; the status where
/// the request was a run. A context the compiler refused leaves the
/// fatal's 255, which php's `EG(exit_status)` keeps for the session.
///
/// ```php
/// function (
/// ```
/// ```request phpdbg
/// q
/// ```
/// ```expect exit=255
/// Parse error: Unclosed '(' in %SCRATCH%/witness.php on line 2
/// prompt>
/// ```
fn ended(s: &mut Session, plan: Plan, observed: &Observables) -> Option<i32> {
    let reached = s.plan != Some(Plan::Hold);
    s.plan = None;
    // `run`'s arguments and its stdin are its run's alone; the next is
    // served with the command line's (`phpdbg.c`'s restart).
    s.args.clone_from(&s.line_args);
    s.stdin_file = None;
    if plan == Plan::Hold && !reached {
        s.context = Context::Refused;
        return (observed.exit_code != 0).then_some(observed.exit_code);
    }
    if !s.executing {
        // php's `EG(exit_status)` outlives the prompt it was set at: a
        // fatal `ev` rendered there is the status the session quits with.
        return (observed.exit_code != 0).then_some(observed.exit_code);
    }
    s.executing = false;
    if let Some(uncaught) = &observed.uncaught {
        out::error(
            format!(
                "Uncaught {} in {} on line {}",
                String::from_utf8_lossy(&uncaught.class),
                String::from_utf8_lossy(&uncaught.file),
                uncaught.line
            )
            .as_bytes(),
        );
        out::writeln(&uncaught.text);
    }
    crate::watch::removed_at_end(s);
    match s.stopped {
        None => {
            prompt::run_queued(s);
            if s.stopped != Some(Stop::Quit) {
                out::notice(b"Script ended normally");
                input::forget();
            }
            if s.after_run == AfterRun::Quit {
                s.stopped = Some(Stop::Quit);
            }
        }
        Some(
            Stop::Quit
            | Stop::Idle
            | Stop::Clean
            | Stop::Exec
            | Stop::Restart(_)
            | Stop::RunWith(_),
        ) => {}
    }
    Some(observed.exit_code)
}

/// `run`'s arguments, split as php's `run` splits them
/// (`phpdbg_prompt.c:751-846`): words, a quoted one kept whole with its
/// escapes, and `<file` the program's stdin.
fn set_arguments(s: &mut Session, line: &[u8]) {
    let mut args = Vec::new();
    s.stdin_file = None;
    let end = line
        .iter()
        .rposition(|b| *b != b'\r' && *b != b'\n')
        .map_or(0, |at| at + 1);
    let line = &line[..end];
    let mut at = line.iter().take_while(|b| **b == b' ').count();
    while at < line.len() {
        let mut sep = b' ';
        let mut word = Vec::new();
        if line[at] == b'<' {
            at += 1;
            while line.get(at) == Some(&b' ') {
                at += 1;
            }
            if matches!(line.get(at), Some(b'\'' | b'"')) {
                sep = line[at];
                at += 1;
            }
            while at < line.len() && line[at] != sep {
                if line[at] == b'\\'
                    && matches!(line.get(at + 1), Some(&b) if b == sep || b == b'\\')
                {
                    at += 1;
                }
                word.push(line[at]);
                at += 1;
            }
            s.stdin_file = Some(Path::new(std::ffi::OsStr::from_bytes(&word)).to_path_buf());
            break;
        }
        if matches!(line[at], b'\'' | b'"') {
            sep = line[at];
            at += 1;
        }
        if line.get(at) == Some(&b'\\') && matches!(line.get(at + 1), Some(b'<' | b'\'' | b'"')) {
            at += 1;
        }
        while at < line.len() && line[at] != sep {
            if line[at] == b'\\'
                && matches!(line.get(at + 1), Some(&b) if b == sep || b == b'\\' || (b == b'#' && sep == b' '))
            {
                at += 1;
            }
            word.push(line[at]);
            at += 1;
        }
        args.push(word);
        if at < line.len() {
            at += 1;
            while line.get(at) == Some(&b' ') {
                at += 1;
            }
        }
    }
    s.args = args;
}

/// The request for `plan`: the context by name, or the code `stdin` read
/// under `Standard input code`, or, for an idle request, an empty
/// program; `argv` the context and the arguments after it.
fn describe(s: &Session, plan: Plan) -> RequestDescription {
    let exec = s.exec.clone().unwrap_or_default();
    let mut argv = vec![exec.clone()];
    argv.extend(s.args.iter().cloned());
    let (script_name, source) = match (plan, &s.stdin_code) {
        (Plan::Idle, _) => (None, Some(b"<?php ".to_vec())),
        (_, Some(code)) => (Some(exec.clone()), Some(code.clone())),
        (_, None) => (Some(exec.clone()), None),
    };
    let stdin = match &s.stdin_file {
        Some(path) => std::fs::File::open(path)
            .ok()
            .map(std::os::fd::OwnedFd::from),
        None => std::os::fd::AsFd::as_fd(&std::io::stdin())
            .try_clone_to_owned()
            .ok(),
    };
    let stdout = std::os::fd::AsFd::as_fd(&std::io::stdout())
        .try_clone_to_owned()
        .ok();
    let stderr = std::os::fd::AsFd::as_fd(&std::io::stderr())
        .try_clone_to_owned()
        .ok();
    RequestDescription {
        argv,
        env: std::env::vars_os()
            .map(|(n, v)| (n.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
            .collect(),
        cwd: std::env::current_dir().unwrap_or_default(),
        server: vec![
            (b"PHP_SELF".to_vec(), exec.clone()),
            (b"SCRIPT_NAME".to_vec(), exec.clone()),
            (b"SCRIPT_FILENAME".to_vec(), exec.clone()),
            (b"PATH_TRANSLATED".to_vec(), exec),
            (b"DOCUMENT_ROOT".to_vec(), Vec::new()),
        ],
        sapi_name: String::from(NAME),
        script_name,
        source,
        file_handles: Some(FileHandles::new(stdin, stdout, stderr)),
        primary: Primary::Bare,
        ..RequestDescription::default()
    }
}

/// A command outside any request that needs one standing: served by a
/// request of its own, after the prompt that asked for it has ended.
pub fn idle(s: &mut Session, command: Command) {
    s.job = Some(command);
    s.stopped = Some(Stop::Idle);
}

/// `VCWD_REALPATH` over a path, the path as given where it resolves to
/// nothing.
pub fn real_path(path: &[u8]) -> Vec<u8> {
    std::fs::canonicalize(Path::new(std::ffi::OsStr::from_bytes(path))).map_or_else(
        |_| path.to_vec(),
        |real| real.as_os_str().as_bytes().to_vec(),
    )
}

/// Where a breakpoint on `name:line` goes (`phpdbg_set_breakpoint_file`):
/// the real path of a file that exists, and else, for a relative name,
/// the compiled file it is the tail of; none leaves it pending. An
/// absolute name that exists nowhere is refused with php's error, and
/// the breakpoint is not set.
pub fn file_break(s: &mut Session, name: &[u8], line: i64) {
    let os = Path::new(std::ffi::OsStr::from_bytes(name));
    match std::fs::metadata(os) {
        Ok(meta) if !meta.is_file() => {
            out::error(
                format!(
                    "Cannot set breakpoint in {}, it is not a regular file",
                    String::from_utf8_lossy(name)
                )
                .as_bytes(),
            );
        }
        Ok(_) => {
            let real = std::fs::canonicalize(os).map_or_else(
                |_| name.to_vec(),
                |path| path.as_os_str().as_bytes().to_vec(),
            );
            s.breaks.set_file(Some(real), name, line);
        }
        Err(_) if name.first() == Some(&b'/') => {
            out::error(
                format!(
                    "Cannot stat {}, it does not exist",
                    String::from_utf8_lossy(name)
                )
                .as_bytes(),
            );
        }
        Err(_) => {
            let compiled = s.exec.clone().filter(|exec| {
                exec.len() > name.len()
                    && exec.ends_with(name)
                    && exec[exec.len() - name.len() - 1] == b'/'
            });
            s.breaks.set_file(compiled, name, line);
        }
    }
}
