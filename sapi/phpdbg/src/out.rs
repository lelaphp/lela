//! What phpdbg writes, and the program's output beside it
//! (`Z/sapi/phpdbg/phpdbg_out.c`): one stream, stdout, and one fact about
//! it, whether the last byte written was a newline, which a notice or an
//! error reads to start on a line of its own; an op array's listing goes
//! to stderr, where `zend_dump_op_array` writes it. The program's output comes
//! through [`Sink`] as `php_sapi_phpdbg_ub_write` sends it, `P_STDOUT`.

use std::cell::Cell;
use std::io::Write;

use php_sapi::{Aborted, OutputSink, ResponseHead};

thread_local! {
    /// `PHPDBG_G(last_was_newline)`.
    static LAST_WAS_NEWLINE: Cell<bool> = const { Cell::new(true) };
    /// A write of the debugger's own through the program's output is
    /// under way, [`interruptible`].
    static INTERRUPTIBLE: Cell<bool> = const { Cell::new(false) };
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
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

/// `phpdbg_out`: the bytes as they are, the newline fact untouched.
pub fn out(bytes: &[u8]) {
    raw(bytes);
}

/// `zend_dump_op_array`'s listing, which it writes to stderr
/// (`Z/Zend/Optimizer/zend_dump.c`), the newline fact untouched.
pub fn dump(bytes: &[u8]) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(bytes);
    let _ = stderr.flush();
}

/// `phpdbg_write`, `P_WRITE`: the bytes, and the fact read off their last.
pub fn write(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    raw(bytes);
    LAST_WAS_NEWLINE.set(bytes.last() == Some(&b'\n'));
}

/// `phpdbg_writeln`, `P_WRITELN`: the bytes and a newline.
pub fn writeln(bytes: &[u8]) {
    let mut line = bytes.to_vec();
    line.push(b'\n');
    raw(&line);
    LAST_WAS_NEWLINE.set(true);
}

/// `phpdbg_notice` and `phpdbg_error`, which read alike without colours:
/// a newline first where the last byte was none, then `[message]`.
pub fn notice(message: &[u8]) {
    if message.is_empty() {
        return;
    }
    let mut line = Vec::with_capacity(message.len() + 4);
    if !LAST_WAS_NEWLINE.get() {
        line.push(b'\n');
    }
    line.push(b'[');
    line.extend_from_slice(message);
    line.extend_from_slice(b"]\n");
    raw(&line);
    LAST_WAS_NEWLINE.set(true);
}

/// `phpdbg_error`, the same bytes as a notice.
pub fn error(message: &[u8]) {
    notice(message);
}

/// The line the prompt reads was taken, `phpdbg_consume_stdin_line`: the
/// next notice starts where it stands.
pub fn line_read() {
    LAST_WAS_NEWLINE.set(true);
}

/// The program's output, `P_STDOUT`: the bytes, the newline fact read off
/// their last; the header block nowhere, the log and the error stream on
/// stderr.
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
        let _ = std::io::stdout().lock().flush();
    }

    fn send_headers(&mut self, _head: &ResponseHead) {}

    fn log(&mut self, entry: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(entry);
        let _ = stderr.write_all(b"\n");
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        let _ = std::io::stderr().lock().write_all(bytes);
    }

    fn held(&mut self) -> Option<Box<dyn OutputSink + Send>> {
        Some(Box::new(Sink))
    }
}
