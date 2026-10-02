//! The requests phpdbg serves (`Z/sapi/phpdbg/phpdbg.c:1159-1767`): php
//! keeps one request standing and restarts it after every run, its
//! execution context compiled again each time; this host serves the
//! same cycle as requests of the engine's. The context is served held at
//! its first opline for the prompt before `run`, and `run` goes on in
//! that request; a run ends with its request, and the next is held again.
//! Where there is no context to hold, a request with no program of the
//! context's stands across the prompt's commands instead.

use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use php_sapi::{Engine, FileHandles, Observables, Primary, RequestDescription};

use crate::cmd::Command;
use crate::out::{self, Sink};
use crate::prompt;
use crate::session::{self, AfterRun, Context, Execution, Plan, Session, Stop};
use crate::{NAME, input};

/// The session's requests, until it quits; the status the last of them
/// left, which a request that writes none does not replace, as php's
/// `EG(exit_status)` stands across the cycles of its main loop.
///
/// ```php
/// if ($argc > 1) {
///   exit(2);
/// }
/// echo "ran\n";
/// ```
/// ```request phpdbg
/// r a
/// r
/// q
/// ```
/// ```expect exit=2
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Script ended normally]
/// prompt> ran
/// [Script ended normally]
/// prompt>
/// ```
pub fn serve(engine: &Engine) -> i32 {
    let mut status = 0;
    loop {
        let Some(mut s) = session::take() else {
            return status;
        };
        let plan = match s.stopped.take() {
            Some(Stop::Quit) => {
                s.stopped = Some(Stop::Quit);
                let status = s.exit_status;
                session::put(s);
                return status;
            }
            Some(Stop::Restart(args) | Stop::RunWith(args)) => {
                if let Some(args) = args {
                    set_arguments(&mut s, &args);
                }
                Plan::Run
            }
            Some(Stop::Idle) => Plan::Idle,
            Some(Stop::Clean | Stop::Exec) | None => {
                if s.exec.is_some() && s.context != Context::Refused {
                    Plan::Hold
                } else {
                    Plan::Idle
                }
            }
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
        s.exit_status = observed.exit_code;
        status = s.exit_status;
        ended(&mut s, plan, &observed);
        session::put(s);
    }
}

/// After a request: the context marked as one that does not compile
/// where its held request never reached its first opline, and a run
/// that ended by itself reported as php reports it; the cycle ended
/// where the request was a run or `clean` ended it. A context the
/// compiler refused leaves the fatal's 255 in php's `EG(exit_status)`.
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
///
/// What `register` named goes where php's cycle starts its module again,
/// after a run among them, so a user function registered before the run
/// is no function after it.
///
/// ```php
/// function foo($a = 0) {
///   echo "in foo $a\n";
/// }
/// foo();
/// ```
/// ```request phpdbg
/// register foo
/// r
/// b foo
/// foo 2
/// c
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Registered foo]
/// prompt> in foo 0
/// [Script ended normally]
/// prompt> [Breakpoint #0 added at foo]
/// prompt> [The command "foo" could not be found]
/// prompt> [Not running]
/// prompt>
/// ```
fn ended(s: &mut Session, plan: Plan, observed: &Observables) {
    let reached = s.plan != Some(Plan::Hold);
    s.plan = None;
    // `run`'s arguments and its stdin are its run's alone; the next is
    // served with the command line's (`phpdbg.c`'s restart).
    s.args.clone_from(&s.line_args);
    s.stdin_file = None;
    if plan == Plan::Hold && !reached {
        s.context = Context::Refused;
        return;
    }
    // php's cycle ends with the request a run, a restart, `clean` or an
    // `exec` during a run stood in (`phpdbg_clean(1, ...)`): the module
    // starts again, `PHPDBG_G(registered)` built empty with it, and the
    // next cycle runs the init files first.
    let cycle_ended = s.execution != Execution::Not || s.stopped == Some(Stop::Clean);
    if cycle_ended {
        s.registered.clear();
        s.init_due = true;
    }
    let execution = std::mem::replace(&mut s.execution, Execution::Not);
    if execution != Execution::Not {
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
    }
    match s.stopped {
        None if execution == Execution::Run => {
            prompt::run_queued(s);
            // `-rr` quits with `PHPDBG_IS_QUITTING` set
            // (`Z/sapi/phpdbg/phpdbg.c:1713-1719`), so the module's
            // shutdown writes no notice.
            if s.after_run == AfterRun::Quit {
                s.stopped = Some(Stop::Quit);
            } else if s.stopped != Some(Stop::Quit) {
                cycle_notice();
            }
        }
        // `clean`, or `exec` where php takes something to be executing,
        // at a prompt no run stands under: the cycle ends with the run
        // flag clear, so the module's shutdown writes its notice, which
        // a run that `clean` or `exec` stopped does not reach.
        Some(Stop::Clean | Stop::Exec) if cycle_ended && execution != Execution::Run => {
            cycle_notice();
        }
        None
        | Some(
            Stop::Quit
            | Stop::Idle
            | Stop::Clean
            | Stop::Exec
            | Stop::Restart(_)
            | Stop::RunWith(_),
        ) => {}
    }
    // The next cycle's `main` sets `PHPDBG_G(flags)` from the line again
    // (`Z/sapi/phpdbg/phpdbg.c:1348-1349`), so colours are on or off as
    // the line says, in the colours the last cycle left. The breakpoints
    // go out as `break` lines (`bp_tmp`), and the next cycle's module
    // starts with its tables empty and `bp_count` at zero
    // (`PHP_MINIT_FUNCTION(phpdbg)`, `php_phpdbg_globals_ctor`), its init
    // files numbering from there before the lines are run again.
    if cycle_ended {
        out::set_colours(s.line_colours);
        s.carried = s.breaks.export();
        s.breaks = crate::bp::Breakpoints::default();
    }
}

/// What `PHP_MSHUTDOWN_FUNCTION(phpdbg)` writes where the cycle ends and
/// php is not quitting (`Z/sapi/phpdbg/phpdbg.c:172-180`), with the last
/// line it kept to repeat let go. `clean` at the held prompt ends the
/// cycle so, and what `register` named goes with it.
///
/// ```php
/// function foo($a = 0) {
///   echo "in foo $a\n";
/// }
/// foo();
/// ```
/// ```request phpdbg
/// register intdiv
/// clean
/// intdiv 1
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Registered intdiv]
/// prompt> Cleaning Execution Environment
/// Classes    %d
/// Functions  %d
/// Constants  %d
/// Includes   0
/// [Script ended normally]
/// prompt> [The command "intdiv" could not be found]
/// prompt>
/// ```
fn cycle_notice() {
    out::notice(b"Script ended normally");
    input::forget();
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
        exit_status: s.exit_status,
        ..RequestDescription::default()
    }
}

/// A command a run left that needs a request standing: served by a
/// request of its own, after the run's has ended.
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
