//! What php's CLI serves in place of one program (`Z/sapi/cli/php_cli.c`
//! `do_cli`): `-r`'s code, and `PHP_CLI_MODE_PROCESS_STDIN`, `-B`'s code,
//! `-R`'s code or `-F`'s file for each line of stdin, and `-E`'s code.
//! Each code string is `zend_eval_string_ex` with `handle_exceptions`: an
//! `eval` of it at the top of the request under its own name, what it
//! leaves uncaught rendered as php's uncaught fatal without bailing out,
//! so what follows still runs (`Z/Zend/zend_execute_API.c:1402`). A fatal
//! the program raises leaves the whole of it, as `zend_bailout` leaves
//! `do_cli`'s `zend_try`. The CLI host's tests hold the three switches to
//! what the oracle prints (`sapi/cli/tests/process_stdin.rs`).

use std::sync::Arc;

use php_compile::SourceKind;

use crate::api::raising::Level;
use crate::api::request::Request;
use crate::api::values::{PhpString, Value};
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
        sink: &mut dyn OutputSink,
    ) -> Observables {
        let mut req = match self.begin(description, sink, false) {
            Ok(req) => req,
            Err(refused) => return refused,
        };
        match command {
            CommandLine::Direct(code) => {
                self.eval_string(&mut req, &code, b"Command line code");
            }
            CommandLine::Lines { begin, each, end } => {
                self.lines(&mut req, begin.as_deref(), each.as_ref(), end.as_deref());
            }
        }
        self.shutdown(&mut req, sink).0
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
            && !self.eval_string(req, begin, b"Command line begin code")
        {
            return;
        }
        let stdin = req
            .constant_named(b"STDIN")
            .and_then(|value| req.stream_of(&value));
        let mut index = 0;
        while req.exit_status() == 0 {
            let Some(line) = stdin.and_then(|stdin| stdin.get_line(req, None).ok().flatten())
            else {
                break;
            };
            let line = argn(line.as_bytes().to_vec());
            let Ok(line) = PhpString::new(req, &line) else {
                return;
            };
            index += 1;
            req.set_global(b"argn", Some(Value::string(line)));
            req.set_global(b"argi", Some(Value::int(index)));
            let goes_on = match each {
                Some(EachLine::Code(code)) => self.eval_string(req, code, b"Command line run code"),
                Some(EachLine::File) => {
                    self.execute_script(req, None);
                    !req.bailing_out()
                }
                None => true,
            };
            if !goes_on {
                return;
            }
        }
        if let Some(end) = end {
            self.eval_string(req, end, b"Command line end code");
        }
    }

    /// `zend_eval_string_ex(code, NULL, name, 1)`: the code compiled as an
    /// `eval` names it and served at the top of the request, an `exit`
    /// and what it leaves uncaught taken as `zend_exception_error` takes
    /// them, at `E_ERROR` and not bailing out; whether the request goes
    /// on, which a fatal it raised, or a compile error other than a
    /// parse error, ends.
    fn eval_string(&self, req: &mut Request, code: &[u8], name: &[u8]) -> bool {
        let unit = match self.shared.compile_for(req, code, name, SourceKind::Eval) {
            Ok(unit) => unit,
            Err(refused) => {
                crate::engine::refused_program(req, &refused, name);
                return refused.level == Level::Parse && dont_bail(req);
            }
        };
        if !crate::engine::note_compiled(req, &unit, SourceKind::Eval) {
            return take_uncaught(req);
        }
        let frame = match req.push_frame(Arc::clone(&unit)) {
            Ok(frame) => frame,
            Err(fatal) => {
                req.end_in(&fatal);
                return false;
            }
        };
        let served = (self.shared.entry)(req, frame);
        req.detach_symbol_table(frame);
        req.pop_frame();
        if let Err(fatal) = served {
            req.end_in(&fatal);
            return false;
        }
        take_uncaught(req)
    }
}

/// `zend_exception_error(EG(exception), E_ERROR)` over what the code
/// left: the unwind exit given up, a throwable rendered uncaught with
/// `E_DONT_BAIL`; whether the request goes on, which a fatal raised on
/// the way ends.
fn take_uncaught(req: &mut Request) -> bool {
    if req.bailing_out() {
        return false;
    }
    if !req.is_pending() {
        return true;
    }
    match req.exception_error(Level::Error) {
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
