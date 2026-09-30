//! Source listings (`Z/sapi/phpdbg/phpdbg_list.c`): a file's lines as
//! phpdbg split them when it compiled the file, each numbered to five
//! places, the current one marked.

use crate::out;
use crate::session::Session;

/// The source of `file`: the code `stdin` read where that is the file,
/// and the file's bytes otherwise; none where neither is to be had.
fn source(session: &Session, file: &[u8]) -> Option<Vec<u8>> {
    if file == b"Standard input code" {
        return session.stdin_code.clone();
    }
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(file));
    std::fs::read(path).ok()
}

use std::os::unix::ffi::OsStrExt as _;

/// `phpdbg_list_file`: `count` lines from `offset`, the zero-based
/// index of the first, the line numbered `highlight` marked with `>`.
pub fn file(session: &Session, file: &[u8], count: i64, offset: i64, highlight: u32) {
    let Some(buf) = source(session, file) else {
        out::error(b"Could not find information about included file...");
        return;
    };
    // `data->line[]`: where each line starts, and one past the end.
    let mut starts = vec![0usize];
    for (at, byte) in buf.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(at + 1);
        }
    }
    let lines = starts.len();
    starts.push(buf.len());
    let (mut count, mut offset) = (count, offset);
    if offset < 0 {
        count += offset;
        offset = 0;
    }
    let last = usize::try_from(offset.saturating_add(count).max(0))
        .unwrap_or(0)
        .min(lines);
    let mut line = usize::try_from(offset).unwrap_or(0);
    while line < last {
        let start = starts[line];
        line += 1;
        let end = starts[line];
        let text = &buf[start..end];
        let mark = if highlight != 0 && u32::try_from(line).ok() == Some(highlight) {
            b'>'
        } else {
            b' '
        };
        let mut shown = vec![mark];
        shown.extend_from_slice(format!("{line:05}: ").as_bytes());
        shown.extend_from_slice(text);
        out::write(&shown);
        if text.last() != Some(&b'\n') || text.is_empty() {
            out::out(b"\n");
        }
    }
}

/// `list_code`: three lines from the current one, it marked.
pub fn code(session: &Session, file: &[u8], line: u32) {
    self::file(session, file, 3, i64::from(line) - 1, line);
}
