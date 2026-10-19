//! What phpdbg writes, and the program's output beside it
//! (`Z/sapi/phpdbg/phpdbg_out.c`): one stream, stdout, and one fact about
//! it, whether the last byte written was a newline, which a notice or an
//! error reads to start on a line of its own; an op array's listing goes
//! to stderr, where `zend_dump_op_array` writes it. The program's output comes
//! through [`Sink`] as `php_sapi_phpdbg_ub_write` sends it, `P_STDOUT`.
//!
//! A notice and an error are coloured, as php colours them whether or not
//! stdout is a terminal, while the session's [`Colours`] are on: `-b`
//! starts them off, `set colors` turns them, and each element's colour is
//! a row of php's table ([`COLOURS`]) that `set color` and
//! `phpdbg_color()` choose. The prompt is never coloured: the oracle's
//! phpdbg is built with libedit, under which `phpdbg_get_prompt` writes
//! it plain (`Z/sapi/phpdbg/phpdbg_utils.c:321-331`).

use std::io::Write;

use php_ext::abi::AfterLog;
use php_sapi::tier::Request;
use php_sapi::{Aborted, OutputSink, ResponseHead};

use crate::session::{Session, Stop};

/// A row of php's colour table (`colors[]`,
/// `Z/sapi/phpdbg/phpdbg_utils.c:41-67`): its name and the SGR code it
/// writes.
#[derive(Debug, PartialEq, Eq)]
pub struct Colour {
    pub name: &'static str,
    pub code: &'static str,
}

const fn colour(name: &'static str, code: &'static str) -> Colour {
    Colour { name, code }
}

/// `colors[]`, in php's order; the first, `none`, is what an element is
/// given for a name the table does not hold (`phpdbg_set_color_ex`).
pub static COLOURS: [Colour; 25] = [
    colour("none", "0;0"),
    colour("white", "0;64"),
    colour("white-bold", "1;64"),
    colour("white-underline", "4;64"),
    colour("red", "0;31"),
    colour("red-bold", "1;31"),
    colour("red-underline", "4;31"),
    colour("green", "0;32"),
    colour("green-bold", "1;32"),
    colour("green-underline", "4;32"),
    colour("yellow", "0;33"),
    colour("yellow-bold", "1;33"),
    colour("yellow-underline", "4;33"),
    colour("blue", "0;34"),
    colour("blue-bold", "1;34"),
    colour("blue-underline", "4;34"),
    colour("purple", "0;35"),
    colour("purple-bold", "1;35"),
    colour("purple-underline", "4;35"),
    colour("cyan", "0;36"),
    colour("cyan-bold", "1;36"),
    colour("cyan-underline", "4;36"),
    colour("black", "0;30"),
    colour("black-bold", "1;30"),
    colour("black-underline", "4;30"),
];

/// `phpdbg_get_color`: the row named exactly `name`.
pub fn find_colour(name: &[u8]) -> Option<&'static Colour> {
    COLOURS.iter().find(|row| row.name.as_bytes() == name)
}

/// An element php colours (`PHPDBG_COLOR_PROMPT`, `_ERROR`, `_NOTICE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    Prompt,
    Error,
    Notice,
}

impl Element {
    /// `phpdbg_get_element`: the element by its name, in any case.
    pub fn named(name: &[u8]) -> Option<Element> {
        [
            (&b"prompt"[..], Element::Prompt),
            (b"error", Element::Error),
            (b"notice", Element::Notice),
        ]
        .into_iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|(_, element)| element)
    }
}

/// `PHPDBG_IS_COLOURED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colours {
    On,
    Off,
}

/// `PHPDBG_IS_COLOURED` and `PHPDBG_G(colors)`, the colour of the error
/// and the notice; php keeps both across the cycles of its main loop
/// (`PHPDBG_PRESERVE_FLAGS_MASK`, `Z/sapi/phpdbg/phpdbg.c:1679-1690`).
/// The prompt's colour is not kept: nothing the oracle's build writes
/// shows it.
#[derive(Debug, Clone, Copy)]
struct Palette {
    colours: Colours,
    error: &'static Colour,
    notice: &'static Colour,
}

impl Palette {
    /// `PHPDBG_DEFAULT_FLAGS` and the defaults `main` sets
    /// (`Z/sapi/phpdbg/phpdbg.c:1330-1333`): red-bold and green.
    const DEFAULT: Palette = Palette {
        colours: Colours::On,
        error: &COLOURS[5],
        notice: &COLOURS[7],
    };
}

/// What phpdbg keeps of its own stream (`phpdbg_out.c`'s part of
/// `PHPDBG_G`): the palette, whether the last byte written was a newline,
/// `PHPDBG_DISCARD_OUTPUT`, the log's guard, and whether a write of its
/// own through the program's output is under way. The session holds it
/// ([`crate::session::Session::out`]), and the program's bytes reach it
/// through [`Sink`], which holds the session while a request runs.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // flags of `PHPDBG_G` and `PG(in_error_log)`, one field each
pub struct Out {
    palette: Palette,
    /// `PHPDBG_G(last_was_newline)`.
    last_was_newline: bool,
    /// `PHPDBG_DISCARD_OUTPUT`, [`discarded`].
    discarding: bool,
    /// `PG(in_error_log)`: a prompt inside `php_sapi_phpdbg_log_message`
    /// stands, [`in_log`].
    in_log: bool,
    /// `PG(in_error_log)` left set by a bailout out of the log,
    /// [`Out::stick_log`].
    stuck: bool,
    /// A write of the debugger's own through the program's output is
    /// under way, [`interruptible`].
    interruptible: bool,
}

impl Default for Out {
    fn default() -> Out {
        Out {
            palette: Palette::DEFAULT,
            last_was_newline: true,
            discarding: false,
            in_log: false,
            stuck: false,
            interruptible: false,
        }
    }
}

/// `f` inside the host's log: `php_log_err` guards against itself, so
/// whatever is logged while `f` runs, the compiler's refusal of the
/// context among it, is logged nowhere.
pub fn in_log<R>(s: &mut Session, f: impl FnOnce(&mut Session) -> R) -> R {
    let outer = std::mem::replace(&mut s.out.in_log, true);
    let result = f(s);
    s.out.in_log = outer;
    result
}

/// `f` with `PHPDBG_DISCARD_OUTPUT` set: what phpdbg writes in it goes
/// nowhere and leaves the newline fact as it stood, as `phpdbg_print`,
/// `phpdbg_out` and `phpdbg_xml_internal` return first
/// (`Z/sapi/phpdbg/phpdbg_out.c:178-233`).
pub fn discarded(s: &mut Session, f: impl FnOnce(&mut Session)) {
    let outer = std::mem::replace(&mut s.out.discarding, true);
    f(s);
    s.out.discarding = outer;
}

/// `f`, a write of the debugger's own that goes through the program's
/// output of the request standing, run so that what it still sends once
/// a prompt inside it quit is dropped: php's `quit` bails out of the
/// write it interrupted (`zend_bailout`), which never finishes.
pub fn interruptible(req: &mut Request, f: impl FnOnce(&mut Request)) {
    let outer = crate::session::with(req, |s| std::mem::replace(&mut s.out.interruptible, true));
    f(req);
    if let Some(outer) = outer {
        crate::session::with(req, |s| s.out.interruptible = outer);
    }
}

impl Out {
    /// Whether a prompt inside the host's log stands, or the log's guard
    /// was left set.
    pub fn logging(&self) -> bool {
        self.in_log || self.stuck
    }

    /// Whether a prompt inside the host's log stands.
    pub fn in_log_prompt(&self) -> bool {
        self.in_log
    }

    /// A refusal inside the log's prompt bails out of `php_log_err`, past
    /// the line that clears `PG(in_error_log)` (`Z/main/main.c:928-990`),
    /// so nothing is logged until the next cycle's `php_request_startup`
    /// clears it ([`Out::unstick_log`]).
    pub fn stick_log(&mut self) {
        self.stuck = true;
    }

    /// `php_request_startup`'s `PG(in_error_log) = 0`.
    pub fn unstick_log(&mut self) {
        self.stuck = false;
    }

    /// Whether notices and errors are coloured.
    pub fn colours(&self) -> Colours {
        self.palette.colours
    }

    /// Notices and errors coloured from here, or not.
    pub fn set_colours(&mut self, colours: Colours) {
        self.palette.colours = colours;
    }

    /// `phpdbg_set_color`: the element's colour from here.
    pub fn set_colour(&mut self, element: Element, colour: &'static Colour) {
        match element {
            Element::Prompt => {}
            Element::Error => self.palette.error = colour,
            Element::Notice => self.palette.notice = colour,
        }
    }

    /// `phpdbg_set_color_ex`: the element's colour by its name, `none`
    /// for a name the table does not hold.
    pub fn set_colour_named(&mut self, element: Element, name: &[u8]) {
        self.set_colour(element, find_colour(name).unwrap_or(&COLOURS[0]));
    }

    /// `phpdbg_out`: the bytes as they are, the newline fact untouched.
    pub fn out(&mut self, bytes: &[u8]) {
        if !self.discarding {
            raw(bytes);
        }
    }

    /// `phpdbg_write`, `P_WRITE`: the bytes, and the fact read off their
    /// last.
    pub fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.discarding {
            return;
        }
        raw(bytes);
        self.last_was_newline = bytes.last() == Some(&b'\n');
    }

    /// `phpdbg_writeln`, `P_WRITELN`: the bytes and a newline.
    pub fn writeln(&mut self, bytes: &[u8]) {
        if self.discarding {
            return;
        }
        let mut line = bytes.to_vec();
        line.push(b'\n');
        raw(&line);
        self.last_was_newline = true;
    }

    /// `phpdbg_process_print`'s `P_NOTICE` and `P_ERROR`: a newline
    /// first where the last byte was none, then `[message]`, inside the
    /// element's colour while colours are on.
    fn bracketed(&mut self, message: &[u8], colour: &'static Colour) {
        if message.is_empty() || self.discarding {
            return;
        }
        let mut line = Vec::with_capacity(message.len() + 16);
        if !self.last_was_newline {
            line.push(b'\n');
        }
        let coloured = self.palette.colours == Colours::On;
        if coloured {
            line.extend_from_slice(format!("\x1b[{}m", colour.code).as_bytes());
        }
        line.push(b'[');
        line.extend_from_slice(message);
        line.push(b']');
        if coloured {
            line.extend_from_slice(b"\x1b[0m");
        }
        line.push(b'\n');
        raw(&line);
        self.last_was_newline = true;
    }

    /// `phpdbg_notice`.
    pub fn notice(&mut self, message: &[u8]) {
        self.bracketed(message, self.palette.notice);
    }

    /// `phpdbg_error`: a notice's bytes in the error's colour.
    pub fn error(&mut self, message: &[u8]) {
        self.bracketed(message, self.palette.error);
    }

    /// The line the prompt reads was taken, `phpdbg_consume_stdin_line`:
    /// the next notice starts where it stands.
    pub fn line_read(&mut self) {
        self.last_was_newline = true;
    }

    /// The program's output, `P_STDOUT`: the bytes, the newline fact read
    /// off their last.
    pub fn program(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            raw(bytes);
            self.last_was_newline = bytes.last() == Some(&b'\n');
        }
    }

    /// `php_sapi_phpdbg_log_message`'s entry through `phpdbg_error`
    /// (`Z/sapi/phpdbg/phpdbg.c:731-770`), where the log is open.
    pub fn log(&mut self, entry: &[u8]) {
        if !self.logging() {
            self.error(entry);
        }
    }
}

/// The bytes to stdout as they are.
fn raw(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).ok();
    stdout.flush().ok();
}

/// `zend_dump_op_array`'s listing, which it writes to stderr
/// (`Z/Zend/Optimizer/zend_dump.c`), the newline fact untouched.
pub fn dump(bytes: &[u8]) {
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(bytes).ok();
    stderr.flush().ok();
}

/// The program's output, `P_STDOUT`, as `php_sapi_phpdbg_ub_write` sends
/// it; the header block nowhere, the error stream on stderr, and the log
/// as `php_sapi_phpdbg_log_message` writes it. It is the value the host
/// lends each request it serves and takes back at its end, so it holds
/// the session ([`crate::session`]) while the request runs: what phpdbg
/// keeps across the cycles of its main loop, which the tier and the
/// natives reach through the request ([`php_sapi::tier::sink`]) and the
/// program's bytes reach here, as php's `ub_write` reads `PHPDBG_G`.
#[derive(Debug, Default)]
pub struct Sink {
    /// The session, none while the debugger holds it.
    pub session: Option<Session>,
}

impl OutputSink for Sink {
    /// The bytes, unless a write of the debugger's own through the
    /// program's output was interrupted by a prompt inside it that quit.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        match &mut self.session {
            Some(s) if s.out.interruptible && s.stopped == Some(Stop::Quit) => {}
            Some(s) => s.out.program(bytes),
            None => raw(bytes),
        }
        Ok(())
    }

    fn flush(&mut self) {
        std::io::stdout().lock().flush().ok();
    }

    fn send_headers(&mut self, _head: &ResponseHead) {}

    /// `php_sapi_phpdbg_log_message`: what follows a fatal's entry is
    /// [`crate::debugger::fatal_logged`].
    fn log(&mut self, entry: &[u8]) {
        match &mut self.session {
            Some(s) => s.out.log(entry),
            None => Out::default().log(entry),
        }
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().lock().write_all(bytes).ok();
    }
}

/// What a request wrote, one write at a time, on each of its streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    Out(Vec<u8>),
    Log(Vec<u8>),
    Err(Vec<u8>),
}

/// A sink that keeps what it is given and writes nothing: what the host
/// serves to learn what the compiler would say, said later, through
/// [`Sink`], where php says it.
#[derive(Debug, Default)]
pub struct Kept {
    pub written: Vec<Written>,
}

impl OutputSink for Kept {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        self.written.push(Written::Out(bytes.to_vec()));
        Ok(())
    }

    fn flush(&mut self) {}

    fn send_headers(&mut self, _: &ResponseHead) {}

    fn log(&mut self, entry: &[u8]) {
        self.written.push(Written::Log(entry.to_vec()));
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.written.push(Written::Err(bytes.to_vec()));
    }
}

/// What a [`Kept`] sink kept, written as the request would have written
/// it, each diagnostic's entry before its display, and `fatal` served
/// once the entry logged last is written: a refused compile ends at its
/// fatal, which `php_sapi_phpdbg_log_message` follows with its listing,
/// and its prompt once php is fully started, inside the log and so
/// before `php_error_cb` displays the fatal
/// (`Z/sapi/phpdbg/phpdbg.c:731-770`, `Z/main/main.c:1411-1495`). A log
/// that bailed out leaves the display unwritten; nothing logged serves
/// no `fatal`. Which of the two it was is the answer, `Returned` where
/// nothing was logged.
pub fn replay(
    s: &mut Session,
    written: &[Written],
    fatal: impl FnOnce(&mut Session) -> AfterLog,
) -> AfterLog {
    let logged = written
        .iter()
        .rposition(|each| matches!(each, Written::Log(_)))
        .map_or(0, |at| at + 1);
    let (through_fatal, displayed) = written.split_at(logged);
    let write = |s: &mut Session, each: &Written| match each {
        Written::Out(bytes) => s.out.program(bytes),
        Written::Log(entry) => s.out.log(entry),
        Written::Err(bytes) => {
            std::io::stderr().lock().write_all(bytes).ok();
        }
    };
    for each in through_fatal {
        write(s, each);
    }
    if logged > 0 && fatal(s) == AfterLog::BailedOut {
        return AfterLog::BailedOut;
    }
    for each in displayed {
        write(s, each);
    }
    AfterLog::Returned
}
