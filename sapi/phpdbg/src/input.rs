//! What phpdbg reads, a line at a time off stdin
//! (`Z/sapi/phpdbg/phpdbg_io.c:26`, `phpdbg_cmd.c:756-833`): the prompt
//! written first, the line taken whole with its newline, an end of input
//! before a newline being the end of the session, and a line of nothing
//! but blanks standing for the last line that had something on it.

use std::cell::RefCell;
use std::io::BufRead;

use crate::out;

/// What reading a line found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A line, its newline kept.
    Read(Vec<u8>),
    /// Stdin ended before a newline: `PHPDBG_IS_QUITTING`.
    End,
}

thread_local! {
    /// `PHPDBG_G(buffer)`, the last line that held something.
    static LAST: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// `phpdbg_consume_stdin_line`: one line with its newline, the `^C`
/// bytes a terminal sends dropped; the end of input where no newline
/// comes.
pub fn consume_line() -> Line {
    out::line_read();
    let mut line = Vec::new();
    let read = std::io::stdin().lock().read_until(b'\n', &mut line);
    match read {
        Ok(_) if line.last() == Some(&b'\n') => {
            line.retain(|b| *b != 0x03);
            Line::Read(line)
        }
        _ => Line::End,
    }
}

/// `phpdbg_read_input(NULL)`: the prompt, then the line with its leading
/// blanks taken off; a line left empty is the last one that was not.
pub fn read_input(prompt: &[u8]) -> Line {
    out::write(prompt);
    let Line::Read(line) = consume_line() else {
        return Line::End;
    };
    Line::Read(remembered(&line))
}

/// A line given rather than read (`phpdbg_read_input(buffered)`), held
/// to the same rule.
pub fn remembered(line: &[u8]) -> Vec<u8> {
    let start = line
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let trimmed = line[start..].to_vec();
    if trimmed.is_empty() {
        return LAST.with_borrow(Clone::clone).unwrap_or_default();
    }
    LAST.set(Some(trimmed.clone()));
    trimmed
}

/// What a question was answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// Stdin ended first: `phpdbg_consume_stdin_line` sets
    /// `PHPDBG_IS_QUITTING` and bails out.
    End,
}

/// `phpdbg_ask_user_permission`: the question, then lines until one is
/// `y` or `n` alone, or the input ends.
pub fn ask(question: &[u8]) -> Answer {
    out::out(question);
    out::out(b" (type y or n): ");
    loop {
        let Line::Read(line) = consume_line() else {
            return Answer::End;
        };
        let answer = line.strip_suffix(b"\n").unwrap_or(&line);
        let answer = answer.strip_suffix(b"\r").unwrap_or(answer);
        match answer {
            b"y" => return Answer::Yes,
            b"n" => return Answer::No,
            _ => out::out(b"Please enter either y (yes) or n (no): "),
        }
    }
}

/// The last line forgotten, as php's restart forgets it.
pub fn forget() {
    LAST.set(None);
}
