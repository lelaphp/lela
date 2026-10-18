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

use std::cell::Cell;
use std::io::Write;

use php_ext::abi::AfterLog;
use php_sapi::{Aborted, OutputSink, ResponseHead};

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

thread_local! {
    static PALETTE: Cell<Palette> = const { Cell::new(Palette::DEFAULT) };
    /// `PHPDBG_G(last_was_newline)`.
    static LAST_WAS_NEWLINE: Cell<bool> = const { Cell::new(true) };
    /// A write of the debugger's own through the program's output is
    /// under way, [`interruptible`].
    static INTERRUPTIBLE: Cell<bool> = const { Cell::new(false) };
    /// `PHPDBG_DISCARD_OUTPUT`, [`discarded`].
    static DISCARDING: Cell<bool> = const { Cell::new(false) };
    /// `PG(in_error_log)`: a prompt inside `php_sapi_phpdbg_log_message`
    /// stands, [`in_log`].
    static IN_LOG: Cell<bool> = const { Cell::new(false) };
    /// `PG(in_error_log)` left set by a bailout out of the log,
    /// [`stick_log`].
    static STUCK: Cell<bool> = const { Cell::new(false) };
}

/// `f` inside the host's log: `php_log_err` guards against itself, so
/// whatever is logged while `f` runs, the compiler's refusal of the
/// context among it, is logged nowhere.
pub fn in_log<R>(f: impl FnOnce() -> R) -> R {
    let outer = IN_LOG.replace(true);
    let result = f();
    IN_LOG.set(outer);
    result
}

/// Whether a prompt inside the host's log stands, or the log's guard
/// was left set.
pub fn logging() -> bool {
    IN_LOG.get() || STUCK.get()
}

/// Whether a prompt inside the host's log stands.
pub fn in_log_prompt() -> bool {
    IN_LOG.get()
}

/// A refusal inside the log's prompt bails out of `php_log_err`, past
/// the line that clears `PG(in_error_log)` (`Z/main/main.c:928-990`), so
/// nothing is logged until the next cycle's `php_request_startup` clears
/// it ([`unstick_log`]).
pub fn stick_log() {
    STUCK.set(true);
}

/// `php_request_startup`'s `PG(in_error_log) = 0`.
pub fn unstick_log() {
    STUCK.set(false);
}

/// `f` with `PHPDBG_DISCARD_OUTPUT` set: what phpdbg writes in it goes
/// nowhere and leaves the newline fact as it stood, as `phpdbg_print`,
/// `phpdbg_out` and `phpdbg_xml_internal` return first
/// (`Z/sapi/phpdbg/phpdbg_out.c:178-233`).
pub fn discarded(f: impl FnOnce()) {
    let outer = DISCARDING.replace(true);
    f();
    DISCARDING.set(outer);
}

/// Whether notices and errors are coloured.
pub fn colours() -> Colours {
    PALETTE.get().colours
}

/// Notices and errors coloured from here, or not.
pub fn set_colours(colours: Colours) {
    PALETTE.set(Palette {
        colours,
        ..PALETTE.get()
    });
}

/// `phpdbg_set_color`: the element's colour from here.
pub fn set_colour(element: Element, colour: &'static Colour) {
    let mut palette = PALETTE.get();
    match element {
        Element::Prompt => {}
        Element::Error => palette.error = colour,
        Element::Notice => palette.notice = colour,
    }
    PALETTE.set(palette);
}

/// `phpdbg_set_color_ex`: the element's colour by its name, `none` for a
/// name the table does not hold.
pub fn set_colour_named(element: Element, name: &[u8]) {
    set_colour(element, find_colour(name).unwrap_or(&COLOURS[0]));
}

/// `f`, a write of the debugger's own that goes through the program's
/// output, run so that what it still sends once a prompt inside it quit
/// is dropped: php's `quit` bails out of the write it interrupted
/// (`zend_bailout`), which never finishes.
pub fn interruptible(f: impl FnOnce()) {
    let outer = INTERRUPTIBLE.replace(true);
    f();
    INTERRUPTIBLE.set(outer);
}

/// Whether the program's bytes are dropped: inside an interruptible
/// write, after the session quit.
fn dropped() -> bool {
    INTERRUPTIBLE.get()
        && crate::session::with(|s| s.stopped == Some(crate::session::Stop::Quit)).unwrap_or(false)
}

/// The bytes to stdout as they are.
fn raw(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).ok();
    stdout.flush().ok();
}

/// `phpdbg_out`: the bytes as they are, the newline fact untouched.
pub fn out(bytes: &[u8]) {
    if DISCARDING.get() {
        return;
    }
    raw(bytes);
}

/// `zend_dump_op_array`'s listing, which it writes to stderr
/// (`Z/Zend/Optimizer/zend_dump.c`), the newline fact untouched.
pub fn dump(bytes: &[u8]) {
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(bytes).ok();
    stderr.flush().ok();
}

/// `phpdbg_write`, `P_WRITE`: the bytes, and the fact read off their last.
pub fn write(bytes: &[u8]) {
    if bytes.is_empty() || DISCARDING.get() {
        return;
    }
    raw(bytes);
    LAST_WAS_NEWLINE.set(bytes.last() == Some(&b'\n'));
}

/// `phpdbg_writeln`, `P_WRITELN`: the bytes and a newline.
pub fn writeln(bytes: &[u8]) {
    if DISCARDING.get() {
        return;
    }
    let mut line = bytes.to_vec();
    line.push(b'\n');
    raw(&line);
    LAST_WAS_NEWLINE.set(true);
}

/// `phpdbg_process_print`'s `P_NOTICE` and `P_ERROR`: a newline first
/// where the last byte was none, then `[message]`, inside the element's
/// colour while colours are on.
fn bracketed(message: &[u8], pick: fn(&Palette) -> &'static Colour) {
    if message.is_empty() || DISCARDING.get() {
        return;
    }
    let mut line = Vec::with_capacity(message.len() + 16);
    if !LAST_WAS_NEWLINE.get() {
        line.push(b'\n');
    }
    let palette = PALETTE.get();
    let colour = pick(&palette);
    if palette.colours == Colours::On {
        line.extend_from_slice(format!("\x1b[{}m", colour.code).as_bytes());
    }
    line.push(b'[');
    line.extend_from_slice(message);
    line.push(b']');
    if palette.colours == Colours::On {
        line.extend_from_slice(b"\x1b[0m");
    }
    line.push(b'\n');
    raw(&line);
    LAST_WAS_NEWLINE.set(true);
}

/// `phpdbg_notice`.
pub fn notice(message: &[u8]) {
    bracketed(message, |palette| palette.notice);
}

/// `phpdbg_error`: a notice's bytes in the error's colour.
pub fn error(message: &[u8]) {
    bracketed(message, |palette| palette.error);
}

/// The line the prompt reads was taken, `phpdbg_consume_stdin_line`: the
/// next notice starts where it stands.
pub fn line_read() {
    LAST_WAS_NEWLINE.set(true);
}

/// The program's output, `P_STDOUT`: the bytes, the newline fact read off
/// their last; the header block nowhere, the error stream on stderr, and
/// the log as `php_sapi_phpdbg_log_message` writes it.
#[derive(Debug, Default)]
pub struct Sink;

impl OutputSink for Sink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        if !bytes.is_empty() && !dropped() {
            raw(bytes);
            LAST_WAS_NEWLINE.set(bytes.last() == Some(&b'\n'));
        }
        Ok(())
    }

    fn flush(&mut self) {
        std::io::stdout().lock().flush().ok();
    }

    fn send_headers(&mut self, _head: &ResponseHead) {}

    /// `php_sapi_phpdbg_log_message`: the entry through `phpdbg_error`
    /// (`Z/sapi/phpdbg/phpdbg.c:731-770`); what follows a fatal's is
    /// [`crate::debugger::fatal_logged`].
    fn log(&mut self, entry: &[u8]) {
        if !logging() {
            error(entry);
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
pub fn replay(written: &[Written], fatal: impl FnOnce() -> AfterLog) -> AfterLog {
    let logged = written
        .iter()
        .rposition(|each| matches!(each, Written::Log(_)))
        .map_or(0, |at| at + 1);
    let (through_fatal, displayed) = written.split_at(logged);
    let mut sink = Sink;
    let mut write = |each: &Written| match each {
        Written::Out(bytes) => {
            sink.write(bytes).ok();
        }
        Written::Log(entry) => sink.log(entry),
        Written::Err(bytes) => sink.write_stderr(bytes),
    };
    through_fatal.iter().for_each(&mut write);
    if logged > 0 && fatal() == AfterLog::BailedOut {
        return AfterLog::BailedOut;
    }
    displayed.iter().for_each(write);
    AfterLog::Returned
}
