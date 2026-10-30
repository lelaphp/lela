//! The requests phpdbg serves (`Z/sapi/phpdbg/phpdbg.c:1159-1767`): php
//! keeps one request standing and restarts it after every `run`, its
//! `exec` file compiled again each time; this host serves the
//! same cycle as requests of the engine's. The `exec` file is served held at
//! its first opline for the prompt before `run`, and `run` goes on in
//! that request; a `run` ends with its request, and the next is held again.
//! Where there is no `exec` file to hold, a request with no program of
//! its own stands across the prompt's commands instead.

use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use php_sapi::{
    Engine, FileHandles, HostServes, Observables, OutputSink, Primary, RequestDescription,
};

use crate::cmd::Command;
use crate::out::{self, Sink};
use crate::prompt;
use crate::session::{AfterServe, ExecCompile, Execution, Plan, Session, Stop};
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
pub fn serve(engine: &Engine, mut s: Session) -> i32 {
    let mut status = 0;
    loop {
        let plan = match s.stopped.take() {
            Some(Stop::Quit) => return s.exit_status,
            Some(Stop::Restart(args) | Stop::ServeWith(args)) => {
                if let Some(args) = args {
                    set_arguments(&mut s, &args);
                }
                if s.init_due && refuses(engine, &s) {
                    s.exec_compile = ExecCompile::Refused;
                    s.init_runs = true;
                    Plan::Init
                } else {
                    Plan::Serve
                }
            }
            Some(Stop::Idle) => Plan::Idle,
            Some(Stop::Clean | Stop::Exec) | None => {
                // No context, or one the compiler refused, which the
                // prompt compiles again in the request.
                if s.exec.is_none() || s.exec_compile == ExecCompile::Refused {
                    Plan::Idle
                } else if std::mem::take(&mut s.exec_in_log) && refuses(engine, &s) {
                    // `exec`'s compile inside the log: its refusal is
                    // logged nowhere and bails out of the log with its
                    // guard left set, to the prompt outside it.
                    s.exec_compile = ExecCompile::Refused;
                    s.status = Some(255);
                    s.out.stick_log();
                    Plan::Idle
                } else if s.init_due && refuses(engine, &s) {
                    s.exec_compile = ExecCompile::Refused;
                    Plan::Init
                } else {
                    Plan::Hold
                }
            }
        };
        if plan == Plan::Serve {
            prompt::begin_serve(&mut s);
        }
        s.plan = Some(plan);
        s.standing = None;
        s.thrown_into = None;
        // A cycle's compile of a context that compiles comes as its request
        // starts, and php's after the init files: what it writes is held
        // for them.
        if matches!(plan, Plan::Hold | Plan::Serve) && s.init_due {
            s.out.hold();
        }
        let description = describe(&s, plan);
        let sink = Box::new(Sink { session: Some(s) });
        let (observed, mut sink) = engine.serve(description, sink, |_| HostServes::Program);
        // What a request held and never gave its place, because it never
        // entered its program or its init files never ran, written as it
        // was written.
        let any: &mut dyn std::any::Any = sink.as_mut();
        if let Some(back) = any
            .downcast_mut::<Sink>()
            .and_then(|sink| sink.session.as_mut())
        {
            back.out.compiled(false);
        }
        if observed.start_failed {
            return start_failed(sink.as_mut());
        }
        let sink: Box<dyn std::any::Any> = sink;
        let Some(back) = sink.downcast::<Sink>().ok().and_then(|sink| sink.session) else {
            return status;
        };
        s = back;
        s.exit_status = s.status.take().unwrap_or(observed.exit_code);
        status = s.exit_status;
        // `php_sapi_phpdbg_deactivate`: what the request compiled goes
        // with it.
        s.sources.clear();
        ended(&mut s, plan, &observed);
    }
}

/// Where `php_request_startup` failed, an extension's `rinit` having
/// raised (#1775): `PUTS("Could not startup")` after what the failure
/// displayed, and 1 out of `main` (`Z/sapi/phpdbg/phpdbg.c:1449-1452`),
/// every later cycle and the prompt left.
fn start_failed(sink: &mut dyn OutputSink) -> i32 {
    sink.write(b"Could not startup").ok();
    sink.flush();
    1
}

/// After a request: the `exec` file marked as one that does not compile
/// where its held request never reached its first opline, and a `run`
/// that ended by itself reported as php reports it; the cycle ended
/// where the request was a `run` or `clean` ended it. An `exec` file the
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
/// What `register` named goes where php's cycle calls `php_module_startup` again,
/// after a `run` among them, so a user function registered before the `run`
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
        s.exec_compile = ExecCompile::Refused;
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
        s.out.unstick_log();
    }
    let execution = std::mem::replace(&mut s.execution, Execution::Not);
    if execution != Execution::Not {
        if let Some(uncaught) = &observed.uncaught {
            s.out.error(
                format!(
                    "Uncaught {} in {} on line {}",
                    String::from_utf8_lossy(&uncaught.class),
                    String::from_utf8_lossy(&uncaught.file),
                    uncaught.line
                )
                .as_bytes(),
            );
            s.out.writeln(&uncaught.text);
        }
        crate::watch::removed_at_end(s);
    }
    if std::mem::take(&mut s.inline_serve) {
        inline_ran(s, plan);
    } else {
        // `run`'s `phpdbg_clean(1, 0)` once its program is over lets the
        // line the cycle parsed last go.
        if execution == Execution::Serving && s.stopped.is_none() {
            s.command_line = None;
        }
        stopped(s, execution, cycle_ended);
    }
    // The next cycle's `main` sets `PHPDBG_G(flags)` from the line again
    // (`Z/sapi/phpdbg/phpdbg.c:1348-1349`), so colours are on or off and
    // quietness is as the line says, in the colours the last cycle left;
    // `set breaks off` goes with the tables. The breakpoints
    // go out as `break` lines (`bp_tmp`), and the next cycle's module
    // starts with its tables empty and `bp_count` at zero
    // (`PHP_MINIT_FUNCTION(phpdbg)`, `php_phpdbg_globals_ctor`), its init
    // files numbering from there before the lines are run again.
    if cycle_ended {
        s.out.set_colours(s.line_colours);
        s.quiet = s.line_quiet;
        s.carried = s.breaks.export();
        s.breaks = crate::bp::Breakpoints::default();
        s.vmret = 0;
    }
}

/// What follows the request where it ended early or its `run` is over.
fn stopped(s: &mut Session, execution: Execution, cycle_ended: bool) {
    match s.stopped {
        None if execution == Execution::Serving => {
            prompt::serve_queued(s);
            // `-rr` quits with `PHPDBG_IS_QUITTING` set
            // (`Z/sapi/phpdbg/phpdbg.c:1713-1719`), so the module's
            // shutdown writes no notice.
            if s.after_serve == AfterServe::Quit {
                s.stopped = Some(Stop::Quit);
            } else if s.stopped != Some(Stop::Quit) {
                cycle_notice(s);
            }
        }
        // `clean`, or `exec` where php takes something to be executing,
        // at a prompt no run stands under: the cycle ends with the run
        // flag clear, so the module's shutdown writes its notice, which
        // a run that `clean` or `exec` stopped does not reach.
        Some(Stop::Clean | Stop::Exec) if cycle_ended && execution != Execution::Serving => {
            cycle_notice(s);
        }
        None
        | Some(
            Stop::Quit
            | Stop::Idle
            | Stop::Clean
            | Stop::Exec
            | Stop::Restart(_)
            | Stop::ServeWith(_),
        ) => {}
    }
}

/// What follows a `run` in the init files once the program it ran is
/// over: its `phpdbg_clean` has the cycle cleaned, so the init files'
/// lines after it are read under that ([`crate::init::resume`]); then a
/// cycle that began with a `run`, or whose init files said yes to a `run`
/// after theirs, finds `in_execution` set and asks before
/// it restarts, `-rr` quitting whatever the answer, and else the cycle's
/// compile after the init files writes its notice, the first cycle's
/// alone, and the prompt bails out at once, which ends the cycle. php
/// loops so for as long as the init files start the program.
fn inline_ran(s: &mut Session, plan: Plan) {
    // The run left `in_execution` set, which a `run` after it asks about.
    s.execution = Execution::Serving;
    crate::init::resume(s);
    s.execution = Execution::Not;
    if s.stopped.is_some() {
        return;
    }
    // A startup run the init files counted resubmits the line the last
    // cycle kept, which bails out at once, where it kept one.
    let counted = std::mem::take(&mut s.startup_serve);
    if plan == Plan::Serve || counted && s.first_command.is_none() {
        // `-rr` quits whatever the answer. Else a yes asks for the run
        // again and a no does not; the input's end bails out quitting,
        // which the cycle being cleaned turns into its end, so the
        // session goes on. A no alone reaches the module's shutdown with
        // nothing quitting, and its notice. A yes has the next cycle's
        // startup run resubmit the last command line the init files read
        // after their run, where they read one, to a prompt the cleaned
        // cycle bails out of at once, so that cycle asks nothing and ends
        // as one with no startup run does; with none, it is `run` again.
        let answer = input::ask(s, b"Do you really want to restart execution?");
        if s.after_serve == AfterServe::Quit {
            s.stopped = Some(Stop::Quit);
            return;
        }
        match answer {
            input::Answer::Yes if s.command_line.is_none() => {
                s.stopped = Some(Stop::Restart(None));
            }
            input::Answer::No => cycle_notice(s),
            input::Answer::Yes | input::Answer::End => {}
        }
        return;
    }
    if !counted {
        crate::debugger::announce(s);
    }
    cycle_notice(s);
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
fn cycle_notice(s: &mut Session) {
    s.out.notice(b"Script ended normally");
    input::forget(s);
}

/// Whether the compiler refuses the `exec` file, learned from a
/// lint of it whose sink writes nothing: php reads the init files before
/// its compile, so an `exec` file that will be refused has them read first and
/// its compile after them, in a request of their own that the prompt then
/// stands in ([`crate::debugger`]'s `Plan::Init`), where the compile is
/// made again and says what it says ([`prompt::refused_again`]).
fn refuses(engine: &Engine, s: &Session) -> bool {
    let mut description = describe(s, Plan::Hold);
    description.file_handles = None;
    description.exit_status = 0;
    let (linted, _) = engine.lint(description, Box::new(out::Unwritten));
    linted.exit_code != 0
}

/// `run`'s arguments, split as php's `run` splits them
/// (`phpdbg_prompt.c:751-846`): words, a quoted one kept whole with its
/// escapes, and `<file` the program's stdin.
pub fn set_arguments(s: &mut Session, line: &[u8]) {
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

/// The request for `plan`: the `exec` file by name, or the code `stdin` read
/// under `Standard input code`, or, for an idle request, an empty
/// program; `argv` the `exec` file and the arguments after it.
fn describe(s: &Session, plan: Plan) -> RequestDescription {
    let exec = s.exec.clone().unwrap_or_default();
    let mut argv = vec![exec.clone()];
    argv.extend(s.args.iter().cloned());
    let (script_name, source) = match (plan, &s.stdin_code) {
        (Plan::Idle | Plan::Init, _) => (None, Some(b"<?php ".to_vec())),
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
            .map(|(n, v)| {
                let name = n.as_encoded_bytes().to_vec();
                // `phpdbg_init`'s walk cut it at its first `:`.
                match s.init_files.scan_dir_seen() {
                    Some(dir) if name == b"PHP_INI_SCAN_DIR" => (name, dir),
                    _ => (name, v.as_encoded_bytes().to_vec()),
                }
            })
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

/// A command a `run` left that needs a request standing: served by a
/// request of its own, after the `run`'s has ended.
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
/// a file the request compiled under that name, `stdin`'s code among
/// them, as php finds it in `file_sources`; the real path of a file that
/// exists, and else, for a relative name, the compiled file it is the
/// tail of; none leaves it pending. An absolute name that exists nowhere
/// is refused with php's error, and the breakpoint is not set.
pub fn file_break(s: &mut Session, name: &[u8], line: i64) {
    let stdin = name == b"Standard input code" && s.stdin_code.is_some();
    if stdin || s.sources.iter().any(|kept| kept.file == name) {
        s.breaks
            .set_file(&mut s.out, Some(name.to_vec()), name, line);
        return;
    }
    let os = Path::new(std::ffi::OsStr::from_bytes(name));
    match std::fs::metadata(os) {
        Ok(meta) if !meta.is_file() => {
            s.out.error(
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
            s.breaks.set_file(&mut s.out, Some(real), name, line);
        }
        Err(_) if name.first() == Some(&b'/') => {
            s.out.error(
                format!(
                    "Cannot stat {}, it does not exist",
                    String::from_utf8_lossy(name)
                )
                .as_bytes(),
            );
        }
        Err(_) => {
            let tail = |file: &[u8]| {
                file.len() > name.len()
                    && file.ends_with(name)
                    && file[file.len() - name.len() - 1] == b'/'
            };
            let kept = s.sources.iter().map(|kept| kept.file.as_slice());
            let compiled = s
                .exec
                .as_deref()
                .into_iter()
                .chain(kept)
                .find(|file| tail(file));
            s.breaks
                .set_file(&mut s.out, compiled.map(<[u8]>::to_vec), name, line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_sapi::{Aborted, ResponseHead};

    /// The body the request wrote, kept.
    #[derive(Default)]
    struct Capture(Vec<u8>);

    impl OutputSink for Capture {
        fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
        fn flush(&mut self) {}
        fn send_headers(&mut self, _head: &ResponseHead) -> Result<(), Aborted> {
            Ok(())
        }
        fn log(&mut self, _entry: &[u8]) {}
        fn write_stderr(&mut self, _bytes: &[u8]) {}
    }

    /// An engine whose one extension's `rinit` throws, as session's does
    /// where its data does not decode (#1775); phpdbg reads no cookie, so
    /// no extension of the bundle fails its request's start.
    fn failing_start() -> Engine {
        /// Core's `Error`.
        #[php_ext::attr::php_class(extern)]
        struct Error;
        /// `Error` as the extension's engine start resolved it.
        struct Failing(php_ext::values::ClassId);
        fn throws(req: &mut php_ext::request::Request) -> Result<(), php_ext::raising::Thrown> {
            let Some(&Failing(error)) = req.engine::<Failing>() else {
                return Err(php_ext::raising::fatal(
                    req,
                    "InternalError: no engine state",
                ));
            };
            Err(php_ext::raising::throw_error(req, error, "rinit threw"))
        }
        let failing = php_ext::abi::ExtensionBuilder::default()
            .engine_state(|start| start.class::<Error>().map(Failing))
            .rinit(throws)
            .build("failing");
        Engine::builder()
            .ini("html_errors", "0")
            .extensions(&[failing])
            .build()
    }

    /// The session's first request fails its start: the session ends there
    /// with 1, no prompt read, and what the failure displayed is followed by
    /// `Could not startup` with no newline, as phpdbg's `PUTS` writes it.
    #[test]
    fn a_failed_start_is_could_not_startup_and_status_one() {
        let engine = failing_start();
        assert_eq!(serve(&engine, Session::new()), 1);
        let s = Session::new();
        let sink = Box::new(Capture::default());
        let (observed, mut sink) =
            engine.serve(describe(&s, Plan::Idle), sink, |_| HostServes::Program);
        assert!(observed.start_failed);
        assert_eq!(start_failed(sink.as_mut()), 1);
        let sink: Box<dyn std::any::Any> = sink;
        let sink = sink.downcast::<Capture>().unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&sink.0),
            "\nFatal error: Uncaught Error: rinit threw in [no active file]:0\nStack trace:\n#0 {main}\n  thrown in [no active file] on line 0\nCould not startup"
        );
    }
}
