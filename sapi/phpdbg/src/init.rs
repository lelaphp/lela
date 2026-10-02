//! The init file (`Z/sapi/phpdbg/phpdbg_prompt.c:182-356`): the commands
//! of the file `-i` names, or, unless `-I`, of a `.phpdbginit` in each
//! directory of `PHP_INI_SCAN_DIR` and then the working directory's, run
//! a line at a time where php's `main` runs `phpdbg_init`, before the
//! execution context is compiled (`Z/sapi/phpdbg/phpdbg.c:1502-1507`).
//! php runs it again at the start of every cycle of its main loop, which
//! starts over from `phpdbg_main:`; this host runs it in the first
//! request of each cycle, at its entry, before anything else there.

use std::os::unix::ffi::OsStrExt as _;

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
/// the reading; else the breakpoints the last cycle carried are set
/// again, their lines run as php's `phpdbg_string_init` runs them, with
/// nothing written, after the init files have numbered theirs.
pub fn run(s: &mut Session, here: &mut Here<'_>) {
    if !std::mem::take(&mut s.init_due) {
        return;
    }
    s.initializing = true;
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
        if !lines(s, here, Some(&file), &text) {
            break;
        }
    }
    if s.stopped.is_none()
        && let Some(carried) = s.carried.take()
    {
        out::discarded(|| {
            lines(s, here, None, &carried);
        });
    }
    s.initializing = false;
}

/// `phpdbg_try_file_init`'s lines, or `phpdbg_string_init`'s where no
/// file is named, through `phpdbg_line_init`: trailing
/// blanks cut, an empty line and one that starts with `#` skipped, and
/// every other line run on its own, so a line that fails leaves the
/// next to run; a code block between `<:` and `:>` is not run. False
/// where the session stopped.
fn lines(s: &mut Session, here: &mut Here<'_>, file: Option<&[u8]>, text: &[u8]) -> bool {
    let mut in_code = false;
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
                in_code = true;
                continue;
            }
            b":>" => {
                in_code = false;
                continue;
            }
            _ if in_code => continue,
            _ => {}
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
            InitLine::Stopped => return false,
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
    true
}
