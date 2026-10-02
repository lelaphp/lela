//! The init file (`Z/sapi/phpdbg/phpdbg_prompt.c:182-356`): the commands
//! of the file `-i` names, or, unless `-I`, of a `.phpdbginit` in each
//! directory of `PHP_INI_SCAN_DIR` and then the working directory's, run
//! a line at a time where php's `main` runs `phpdbg_init`, before the
//! execution context is compiled (`Z/sapi/phpdbg/phpdbg.c:1502-1507`).
//! php runs it again at the start of every cycle of its main loop, which
//! starts over from `phpdbg_main:`; this host runs it in the first
//! request of each cycle, at its entry, before anything else there.

use std::os::unix::ffi::OsStrExt as _;

use php_sapi::tier;

use crate::cmd;
use crate::input;
use crate::out;
use crate::prompt::{self, Here, InitLine};
use crate::session::Session;

/// The files `phpdbg_init` reads, in its order: the one the line names,
/// or the default ones unless `-I`.
pub fn files(named: Option<&[u8]>, default: bool) -> Vec<Vec<u8>> {
    let mut files: Vec<Vec<u8>> = Vec::new();
    if let Some(file) = named {
        files.push(file.to_vec());
    } else if default {
        if let Some(dirs) = std::env::var_os("PHP_INI_SCAN_DIR") {
            for dir in dirs.as_bytes().split(|b| *b == b':') {
                files.push([dir, b"/.phpdbginit"].concat());
            }
        }
        files.push(b".phpdbginit".to_vec());
    }
    files
}

/// `phpdbg_init` with `PHPDBG_IS_INITIALIZING` set, where the session
/// says a cycle has started: each file read a line at a time, with
/// nothing of the execution context compiled. A `quit` among them ends
/// the reading, and so does a fatal or an exit in a code block, which
/// bails out of `phpdbg_init` to the `zend_try` around it; else the
/// breakpoints the last cycle carried are set again, their lines run as
/// php's `phpdbg_string_init` runs them, with nothing written, after the
/// init files have numbered theirs. The first cycle's init files run
/// before `phpdbg_fully_started`, so a fatal there stops at no prompt.
pub fn run(s: &mut Session, here: &mut Here<'_>) {
    if !std::mem::take(&mut s.init_due) {
        return;
    }
    s.initializing = true;
    let started = std::mem::replace(&mut s.started, s.init_ran);
    s.init_ran = true;
    for file in s.init_files.clone() {
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&file));
        if std::fs::metadata(path).is_err() {
            continue;
        }
        let Ok(text) = std::fs::read(path) else {
            out::error(
                format!(
                    "Failed to open {} for initialization",
                    String::from_utf8_lossy(&file)
                )
                .as_bytes(),
            );
            continue;
        };
        if lines(s, here, Some(&file), &text) != Read::Whole {
            break;
        }
    }
    s.started = started;
    if s.stopped.is_none()
        && let Some(carried) = s.carried.take()
    {
        out::discarded(|| {
            lines(s, here, None, &carried);
        });
    }
    s.initializing = false;
}

/// How far a file's lines were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    Whole,
    /// A command stopped the session.
    Stopped,
    /// A code block bailed out of `phpdbg_init`.
    BailedOut,
}

/// What `phpdbg_init` names an init file's code block by, the file name
/// what it raises carries.
const CODE: &[u8] = b"phpdbginit code";

/// `phpdbg_try_file_init`'s lines, or `phpdbg_string_init`'s where no
/// file is named, through `phpdbg_line_init`: trailing blanks cut, an
/// empty line and one that starts with `#` skipped, and every other line
/// run on its own, so a line that fails leaves the next to run. The
/// lines between a `<:` line and a `:>` line are a code block, joined
/// with nothing between them and evaluated at the `:>` as
/// `zend_eval_stringl` evaluates `phpdbginit code` with no value asked
/// for: what it leaves uncaught, or a parse error, stays pending for
/// the next `ev` to render.
fn lines(s: &mut Session, here: &mut Here<'_>, file: Option<&[u8]>, text: &[u8]) -> Read {
    let mut code: Option<Vec<u8>> = None;
    let mut text = text;
    if text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    for (at, line) in text.split(|b| *b == b'\n').enumerate() {
        let line = line.trim_ascii_end();
        if line.is_empty() || line.first() == Some(&b'#') {
            continue;
        }
        match line {
            b"<:" => {
                code.get_or_insert_with(Vec::new);
                continue;
            }
            b":>" => {
                if let Some(code) = code.take() {
                    match block(s, here, &code) {
                        Read::Whole => {}
                        ended => return ended,
                    }
                }
                continue;
            }
            _ => {}
        }
        if let Some(code) = &mut code {
            code.extend_from_slice(line);
            continue;
        }
        // `phpdbg_read_input(cmd)`: the leading blanks taken off, and the
        // line kept as the one an empty prompt line repeats.
        let line = input::remembered(line);
        let line = line.as_slice();
        let Ok(commands) = cmd::parse(line) else {
            continue;
        };
        match prompt::init_line(s, here, commands) {
            InitLine::Done => {}
            InitLine::Stopped => return Read::Stopped,
            InitLine::Unrecognized(message) => {
                let line = String::from_utf8_lossy(line);
                let message = match file {
                    Some(file) => format!(
                        "Unrecognized command in {}:{}: {line}, {message}!",
                        String::from_utf8_lossy(file),
                        at + 1
                    ),
                    None => format!(
                        "Unrecognized command on line {}: {line}, {message}!",
                        at + 1
                    ),
                };
                out::error(message.as_bytes());
            }
        }
    }
    Read::Whole
}

/// A code block evaluated where the init file stands, with the session
/// lent back to its cell, so the loop that serves it reports what it
/// leaves uncaught and stops there, which the checks between handlers
/// do before they give way to `PHPDBG_IS_INITIALIZING`.
fn block(s: &mut Session, here: &mut Here<'_>, code: &[u8]) -> Read {
    let Some(req) = here.request() else {
        return Read::Whole;
    };
    let evaluated = crate::session::lend(s, || tier::evaluate_code(req, CODE, code));
    if s.stopped.is_some() {
        return Read::Stopped;
    }
    match evaluated {
        // A fatal served bailed out of the `phpdbg_execute_ex` that
        // `zend_execute` entered, which leaves `in_execution` set.
        tier::Evaluated::BailedOut => {
            prompt::bailed_out(s);
            Read::BailedOut
        }
        // The compiler's fatal bails out before anything is served.
        tier::Evaluated::NotCompiled if tier::pending(req).is_none() => Read::BailedOut,
        _ => Read::Whole,
    }
}
