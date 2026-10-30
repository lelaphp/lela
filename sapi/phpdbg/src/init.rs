//! The init file (`Z/sapi/phpdbg/phpdbg_prompt.c:182-356`): the commands
//! of the file `-i` names, or, unless `-I`, of a `.phpdbginit` in
//! `PHP_CONFIG_FILE_PATH`, in each directory of `PHP_INI_SCAN_DIR` or,
//! where it is unset, `PHP_CONFIG_FILE_SCAN_DIR`, and then the working
//! directory's, read a line at a time where php's `main` runs
//! `phpdbg_init`, before the `exec` file is compiled
//! (`Z/sapi/phpdbg/phpdbg.c:1502-1507`). php runs it again at the start
//! of every cycle of its main loop, which starts over from
//! `phpdbg_main:`; this host runs it in the first request of each cycle,
//! at its entry, before anything else there, and in a request of its own
//! where the `exec` file will not compile.

use std::os::unix::ffi::OsStrExt as _;

use php_sapi::tier;

use crate::cmd;
use crate::input;
use crate::out;
use crate::prompt::{self, Here, InitLine};
use crate::session::Session;

/// `PHP_CONFIG_FILE_PATH` and `PHP_CONFIG_FILE_SCAN_DIR`, the engine's,
/// from the build's layout.
const CONFIG_FILE_PATH: &[u8] = php_sapi::PHP_CONFIG_FILE_PATH.as_bytes();
const CONFIG_FILE_SCAN_DIR: &[u8] = php_sapi::PHP_CONFIG_FILE_SCAN_DIR.as_bytes();

/// What the line asked of `phpdbg_init`: the file `-i` named, or the
/// default ones unless `-I`; and `PHP_INI_SCAN_DIR` as the process's
/// environment holds it.
#[derive(Debug, Clone, Default)]
pub struct Files {
    pub named: Option<Vec<u8>>,
    pub default: bool,
    pub scan_dir: Option<Vec<u8>>,
}

impl Files {
    /// `PHP_INI_SCAN_DIR` as the program finds it: cut at its first `:`
    /// where `phpdbg_init` walks it, which it does before any program of
    /// the session runs; none where the line names no walk of it.
    pub fn scan_dir_seen(&self) -> Option<Vec<u8>> {
        if self.named.is_some() || !self.default {
            return None;
        }
        let dir = self.scan_dir.as_ref()?;
        let end = dir.iter().position(|b| *b == b':').unwrap_or(dir.len());
        Some(dir[..end].to_vec())
    }

    /// The files `phpdbg_init` reads, in its order. Its walk of the scan
    /// directories writes a NUL over each `:` it passes, in the
    /// environment's own string, so from then on `PHP_INI_SCAN_DIR` reads
    /// as its first directory, to the next cycle and to the program.
    fn read(&mut self) -> Vec<Vec<u8>> {
        let mut files: Vec<Vec<u8>> = Vec::new();
        if let Some(file) = &self.named {
            files.push(file.clone());
        } else if self.default {
            files.push([CONFIG_FILE_PATH, b"/.phpdbginit"].concat());
            let mut rest = self.scan_dir.as_deref().unwrap_or(CONFIG_FILE_SCAN_DIR);
            while !rest.is_empty() {
                if let Some(at) = rest.iter().position(|b| *b == b':') {
                    files.push([&rest[..at], b"/.phpdbginit"].concat());
                    rest = &rest[at + 1..];
                } else {
                    files.push([rest, b"/.phpdbginit"].concat());
                    break;
                }
            }
            if let Some(scan) = &mut self.scan_dir
                && let Some(at) = scan.iter().position(|b| *b == b':')
            {
                scan.truncate(at);
            }
            files.push(b".phpdbginit".to_vec());
        }
        files
    }
}

/// Where a `run` in the init files left the reading: php runs the
/// program inline, uninterrupted, and goes on with the line after it
/// once the `run` is over. The lines after it of the file it stood in,
/// numbered from `first`, the files after that, and whether the session
/// was fully started before the files ran.
#[derive(Debug, Clone)]
pub struct Rest {
    file: Option<Vec<u8>>,
    text: Vec<u8>,
    first: usize,
    files: Vec<Vec<u8>>,
    started: bool,
}

/// `phpdbg_init` with `PHPDBG_IS_INITIALIZING` set, where the session
/// says a cycle has started: each file read a line at a time, with
/// nothing of the `exec` file compiled. A `quit` among them ends
/// the reading, and so does a fatal or an exit in a code block, which
/// bails out of `phpdbg_init` to the `zend_try` around it; else the
/// breakpoints the last cycle carried are set again, their lines read as
/// php's `phpdbg_string_init` runs them, with nothing written, after the
/// init files have numbered theirs. The first cycle's init files are read
/// before `phpdbg_fully_started`, so a fatal there stops at no prompt.
/// A `run` among them leaves the reading in the session's `init_rest`
/// for [`resume`], with `PHPDBG_IS_INITIALIZING` still set for the `run`.
pub fn serve(s: &mut Session, here: &mut Here<'_>) {
    if !std::mem::take(&mut s.init_due) {
        return;
    }
    s.initializing = true;
    s.init_compiled = false;
    s.first_command = s.command_line.take();
    s.startup_serve = false;
    let started = std::mem::replace(&mut s.started, s.init_ran);
    s.init_ran = true;
    let files = s.init_files.read();
    read(s, here, files, started);
}

/// The init files from the first of `files` on.
fn read(s: &mut Session, here: &mut Here<'_>, files: Vec<Vec<u8>>, started: bool) {
    let mut files = files.into_iter();
    while let Some(file) = files.next() {
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&file));
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        // `fopen` opens a directory, whose first `fgets` reads nothing.
        if meta.is_dir() {
            continue;
        }
        let Ok(text) = std::fs::read(path) else {
            s.out.error(
                format!(
                    "Failed to open {} for initialization",
                    String::from_utf8_lossy(&file)
                )
                .as_bytes(),
            );
            continue;
        };
        match lines(s, here, Some(&file), &text, 0) {
            Read::Whole => {}
            Read::Ran { rest, first } => {
                s.init_rest = Some(Rest {
                    file: Some(file),
                    text: rest,
                    first,
                    files: files.collect(),
                    started,
                });
                return;
            }
            Read::Stopped | Read::BailedOut => break,
        }
    }
    finish(s, here, started);
}

/// The reading over: the session as fully started as it was, and the
/// breakpoints the last cycle carried set again.
fn finish(s: &mut Session, here: &mut Here<'_>, started: bool) {
    s.started = started;
    if s.stopped.is_none()
        && let Some(carried) = s.carried.take()
    {
        out::discarded(s, |s| {
            lines(s, here, None, &carried, 0);
        });
    }
    s.initializing = false;
}

/// The init files' lines after a `run` among them, once the `run` is over:
/// read outside any request, with `PHPDBG_IS_CLEANING` set by the `run`'s
/// `phpdbg_clean`, under which whatever would serve code bails out of
/// `phpdbg_execute_ex` at its entry, an `ev` writing nothing and a code
/// block ending the reading.
pub fn resume(s: &mut Session) {
    let Some(rest) = s.init_rest.take() else {
        return;
    };
    s.init_cleaning = true;
    let mut here = Here::Outside;
    match lines(s, &mut here, rest.file.as_deref(), &rest.text, rest.first) {
        Read::Whole => read(s, &mut here, rest.files, rest.started),
        Read::Ran { .. } | Read::Stopped | Read::BailedOut => {
            s.init_rest = None;
            finish(s, &mut here, rest.started);
        }
    }
    s.init_cleaning = false;
}

/// How far a file's lines were read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Read {
    Whole,
    /// A command stopped the session.
    Stopped,
    /// A code block bailed out of `phpdbg_init`.
    BailedOut,
    /// `run` runs the program inline; the lines after it, the first
    /// numbered `first` from zero, are read once it is over.
    Ran {
        rest: Vec<u8>,
        first: usize,
    },
}

/// What `phpdbg_init` names an init file's code block by, the file name
/// what it raises carries.
const CODE: &[u8] = b"phpdbginit code";

/// `phpdbg_try_file_init`'s lines, or `phpdbg_string_init`'s where no
/// file is named, through `phpdbg_line_init`: trailing blanks cut, an
/// empty line and one that starts with `#` skipped, and every other line
/// taken on its own, so a line that fails leaves the next to be taken. The
/// lines between a `<:` line and a `:>` line are a code block, joined
/// with nothing between them and evaluated at the `:>` as
/// `zend_eval_stringl` evaluates `phpdbginit code` with no value asked
/// for: what it leaves uncaught, or a parse error, stays pending for
/// the next `ev` to render. `first` numbers the first line of `text`.
fn lines(
    s: &mut Session,
    here: &mut Here<'_>,
    file: Option<&[u8]>,
    text: &[u8],
    first: usize,
) -> Read {
    let mut code: Option<Vec<u8>> = None;
    let mut text = text;
    if text.last() == Some(&b'\n') {
        text = &text[..text.len() - 1];
    }
    let mut next = 0;
    for (at, line) in text.split(|b| *b == b'\n').enumerate() {
        let at = at + first;
        next += line.len() + 1;
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
        // line kept as the one an empty prompt line repeats. The lexer
        // unquotes the copy it reads in place, and the message below
        // quotes that copy.
        let mut line = input::remembered(s, line);
        s.command_line = Some(line.clone());
        let Ok(commands) = cmd::parse(&mut line) else {
            continue;
        };
        match prompt::init_line(s, here, commands) {
            InitLine::Done => {}
            InitLine::Stopped => return Read::Stopped,
            InitLine::Ran => {
                return Read::Ran {
                    rest: text.get(next..).unwrap_or_default().to_vec(),
                    first: at + 1,
                };
            }
            InitLine::Unrecognized(message) => {
                let line = String::from_utf8_lossy(&line);
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
                s.out.error(message.as_bytes());
            }
        }
    }
    Read::Whole
}

/// A code block evaluated where the init file stands, with the session
/// lent back to the request's sink, so the loop that serves it reports what it
/// leaves uncaught and stops there, which the checks between handlers
/// do before they give way to `PHPDBG_IS_INITIALIZING`.
fn block(s: &mut Session, here: &mut Here<'_>, code: &[u8]) -> Read {
    if s.init_cleaning {
        return Read::BailedOut;
    }
    let Some(req) = here.request() else {
        return Read::Whole;
    };
    let evaluated = crate::session::lend(req, s, |req| tier::evaluate_code(req, CODE, code));
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
        tier::Evaluated::NotCompiled if crate::read::pending(req).is_none() => Read::BailedOut,
        _ => Read::Whole,
    }
}
