//! What php's CLI serves in place of one program (`Z/sapi/cli/php_cli.c`
//! `do_cli`): `-r`'s code, and `PHP_CLI_MODE_PROCESS_STDIN`, `-B`'s code,
//! `-R`'s code or `-F`'s file for each line of stdin, and `-E`'s code.
//! Each code string is `zend_eval_string_ex` with `handle_exceptions`: an
//! `eval` of it at the top of the request under its own name, what it
//! leaves uncaught rendered as php's uncaught fatal without bailing out,
//! so what follows still runs (`Z/Zend/zend_execute_API.c:1402`). A fatal
//! the program raises leaves the whole of it, as `zend_bailout` leaves
//! `do_cli`'s `zend_try`. The CLI host's tests hold the three switches to
//! what the oracle prints (`sapi/cli/tests/process_stdin.rs`). `-a` is
//! the shell an extension provides ([`Shell`](crate::api::abi::Shell)),
//! served where `do_cli` serves it, each block of its code through
//! [`Code::eval_stringl`].

use std::sync::Arc;

use php_compile::SourceKind;

use crate::api::raising::{Level, Thrown};
use crate::api::request::Code;
use crate::api::request::Request;
use crate::api::values::{Owner, PhpString};
use crate::engine::{Engine, Observables, OutputSink, RequestDescription};

/// What the CLI's command line serves where no program is, php's
/// `PHP_CLI_MODE_CLI_DIRECT` and `PHP_CLI_MODE_PROCESS_STDIN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandLine {
    /// `-r`: the code under the name `Command line code`.
    Direct(Vec<u8>),
    /// `-B`, `-R` or `-F`, and `-E`: `begin` under `Command line begin
    /// code`, then for each line of the request's `STDIN` while the exit
    /// status is 0, `$argn` the line without its line end and `$argi` its
    /// number, `each`; then `end` under `Command line end code`.
    Lines {
        begin: Option<Vec<u8>>,
        each: Option<EachLine>,
        end: Option<Vec<u8>>,
    },
    /// `-a`: `cli_shell_callbacks.cli_shell_run()` in a request of its
    /// own, after `auto_prepend_file` as `readline_shell_run` requires
    /// it, with the status 0 that function sets first, and the request's
    /// exit status what it answers; nothing where the engine has no shell
    /// ([`Engine::has_shell`]).
    Shell,
}

/// What runs for each line of stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EachLine {
    /// `-R`: the code under the name `Command line run code`.
    Code(Vec<u8>),
    /// `-F`: the file the request names, served as `php_execute_script`
    /// serves it, opened again for each line.
    File,
}

impl Engine {
    /// One request serving `command` where `do_cli` serves it, with the
    /// standard handles registered as for a program: an `exit` with
    /// status 0 in a line's code goes on to the next line, one that sets
    /// a status ends the lines, and what a line leaves uncaught is
    /// rendered and ends them, `-E` still served after either.
    pub fn command_line(
        &self,
        description: RequestDescription,
        command: CommandLine,
        sink: Box<dyn OutputSink + Send>,
    ) -> (Observables, Box<dyn OutputSink + Send>) {
        let mut req = match self.begin(description, sink, false) {
            Ok(req) => req,
            Err(refused) => return refused,
        };
        match command {
            CommandLine::Direct(code) => {
                eval_string(&mut req, &code, b"Command line code");
            }
            CommandLine::Lines { begin, each, end } => {
                self.lines(&mut req, begin.as_deref(), each.as_ref(), end.as_deref());
            }
            CommandLine::Shell => {
                if let Some((by, shell)) = self.shared.supplied.shell.entry() {
                    crate::engine::auto_file(
                        &mut req,
                        b"auto_prepend_file",
                        self.shared.loops.entry,
                    );
                    // `readline_shell` destroys the file's
                    // `zend_file_handle` once it has run, before the
                    // shell reads a line.
                    req.close_served_files();
                    if !req.diagnostics().bailing_out() {
                        req.end_in_exit(0);
                        let status = req.serve_for(by, shell.serve);
                        req.end_in_exit(status);
                    }
                }
            }
        }
        self.shutdown_untraced(&mut req)
    }

    /// `PHP_CLI_MODE_PROCESS_STDIN` (`Z/sapi/cli/php_cli.c:996-1034`):
    /// the lines are read off the request's `STDIN` stream, so what a
    /// line's code reads from it is what the next `php_stream_gets` did
    /// not take.
    fn lines(
        &self,
        req: &mut Request,
        begin: Option<&[u8]>,
        each: Option<&EachLine>,
        end: Option<&[u8]>,
    ) {
        if let Some(begin) = begin
            && !eval_string(req, begin, b"Command line begin code")
        {
            return;
        }
        let stdin = req.constant_named(b"STDIN");
        let get_line = req.stream_hooks().map(|hooks| hooks.get_line);
        let mut index = 0;
        while req.diagnostics().exit_status() == 0 {
            let Some(line) = stdin
                .as_ref()
                .zip(get_line)
                .and_then(|(stdin, get_line)| get_line(req, stdin))
            else {
                break;
            };
            let line = argn(line);
            let Ok(line) = PhpString::new(req, &line) else {
                return;
            };
            index += 1;
            req.variables_mut()
                .set_global(b"argn", Some(line.into_value()));
            req.variables_mut()
                .set_global(b"argi", Some(Owner::int(index)));
            let goes_on = match each {
                Some(EachLine::Code(code)) => eval_string(req, code, b"Command line run code"),
                Some(EachLine::File) => {
                    self.execute_script(req, None);
                    !req.diagnostics().bailing_out()
                }
                None => true,
            };
            if !goes_on {
                return;
            }
        }
        if let Some(end) = end {
            eval_string(req, end, b"Command line end code");
        }
    }
}

/// `zend_eval_string_ex(code, NULL, name, 1)`: the code compiled as an
/// `eval` names it and served at the top of the request, an `exit` and
/// what it leaves uncaught taken as `zend_exception_error` takes them, at
/// `E_ERROR` and not bailing out; whether the request goes on, which a
/// fatal it raised, or a compile error other than a parse error, ends.
fn eval_string(req: &mut Request, code: &[u8], name: &[u8]) -> bool {
    eval(req, code, name, Some(Level::Error))
}

impl Code<&mut Request> {
    /// `zend_eval_stringl(code, len, NULL, name)` in a `zend_try`, as
    /// the shell serves each block of code (`readline_shell_run`): the
    /// code compiled as an `eval` names it and served at the top of the
    /// request, a parse error rendered as the compiler raised it, a fatal
    /// rendered and caught, so the request goes on with the status 255 it
    /// left, and an `exit` taken without a word. What the code left
    /// uncaught is the `Err`, `EG(exception)` for the caller to render
    /// with [`uncaught_at`](crate::api::raising::uncaught_at) where php's
    /// does.
    pub fn eval_stringl(self, code: &[u8], name: &[u8]) -> Result<(), Thrown> {
        eval(self.0, code, name, None);
        self.0.catch_bailout();
        if let Some(exception) = self.0.take_pending_view() {
            return Err(Thrown::of(Some(exception)));
        }
        if self.0.is_pending() {
            // The unwind exit, which `zend_exception_error` gives up.
            self.0.exception_error(Level::Warning).ok();
        }
        Ok(())
    }
}

/// `zend_eval_string_ex(code, NULL, name, 1)`'s body with what the code
/// leaves uncaught rendered at `uncaught`, or left pending for `None`;
/// whether the request goes on, which a fatal, or a compile error other
/// than a parse error, ends.
fn eval(req: &mut Request, code: &[u8], name: &[u8], uncaught: Option<Level>) -> bool {
    let shared = Arc::clone(&req.shared);
    let unit = match req.host_string(|req| {
        req.compile_string(code, name, php_compile::syntax::Position::AfterOpenTag)
    }) {
        Ok(Ok(unit)) => unit,
        Ok(Err(refused)) => {
            crate::engine::refused_program(req, &refused, name);
            return refused.error.level == Level::Parse && dont_bail(req);
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            return left(req, uncaught);
        }
    };
    if !crate::engine::note_compiled(req, &unit, SourceKind::EVAL) {
        return left(req, uncaught);
    }
    let standing = req.vm().depth();
    let program = crate::api::request::CodeFrom::Top(None);
    let frame = match req.push_code_frame(php_unit::PublishedUnit::clone(&unit), program) {
        Ok(frame) => frame,
        Err(fatal) => {
            req.end_in(&fatal);
            return false;
        }
    };
    let served = req.serve_host_string(|req| (shared.loops.entry)(req, frame));
    // What a fatal left standing above the main frame, as
    // `serve_unit` pops it.
    req.leave_code(standing, frame);
    if let Err(fatal) = served {
        req.end_in(&fatal);
        return false;
    }
    left(req, uncaught)
}

/// What the code left once it ends: rendered uncaught at `uncaught`
/// ([`take_uncaught`]), or for `None` left pending, the request going on
/// unless it is bailing out.
fn left(req: &mut Request, uncaught: Option<Level>) -> bool {
    uncaught.map_or(!req.diagnostics().bailing_out(), |level| {
        take_uncaught(req, level)
    })
}

/// `zend_exception_error(EG(exception), severity)` over what the code
/// left: the unwind exit given up, a throwable rendered uncaught with
/// `E_DONT_BAIL`; whether the request goes on, which a fatal raised on
/// the way ends.
fn take_uncaught(req: &mut Request, severity: Level) -> bool {
    if req.diagnostics().bailing_out() {
        return false;
    }
    if !req.is_pending() {
        return true;
    }
    match req.exception_error(severity) {
        Ok(()) => true,
        Err(_) => dont_bail(req),
    }
}

/// `E_DONT_BAIL`: the status is php's 255 and nothing is left for a
/// `zend_try` to catch.
fn dont_bail(req: &mut Request) -> bool {
    req.catch_bailout();
    true
}

/// `$argn` as `do_cli` cuts it from the line `php_stream_gets` read
/// (`Z/sapi/cli/php_cli.c:1003-1007`): each line end at the tail made a
/// NUL, and the string one byte past the last byte that is not one, so a
/// line of nothing but its end is one NUL.
fn argn(mut line: Vec<u8>) -> Vec<u8> {
    let mut len = line.len();
    while len > 0 {
        len -= 1;
        match line.get_mut(len) {
            Some(byte @ (b'\n' | b'\r')) => *byte = 0,
            _ => break,
        }
    }
    line.truncate(len + 1);
    line
}

#[cfg(test)]
mod tests {
    use super::argn;

    #[test]
    fn argn_is_the_line_without_its_end_and_an_empty_line_one_nul() {
        assert_eq!(argn(b"abc\n".to_vec()), b"abc");
        assert_eq!(argn(b"x\r\n".to_vec()), b"x");
        assert_eq!(argn(b"last".to_vec()), b"last");
        assert_eq!(argn(b"\n".to_vec()), b"\0");
        assert_eq!(argn(b"\r\n".to_vec()), b"\0");
    }
}
