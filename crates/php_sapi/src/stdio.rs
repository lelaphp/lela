//! C's `stdout`, the one `FILE` php's hosts share with libc: what php
//! writes through stdio, `php_output_stdout` while php starts and ends
//! (`php_output_direct`, `Z/main/output.c:83`), a host's `printf` or a
//! `php_printf` with no request started, opcache's `preload_ub_write`, is
//! held here and written to fd 1 when libc would write it, while a host's
//! body goes past it with `write(2)` (`sapi_cgi_single_write` and the
//! CLI's, under `PHP_WRITE_STDOUT`). So where stdout is a pipe or a file a
//! startup diagnostic php displayed before the body reaches fd 1 after it:
//! the buffer is written by `fflush` ([`flush`]: a host's `sapi_flush`,
//! `sapi_cli_deactivate` as `php_module_startup` ends, opcache's
//! `preload_flush`), by `exit` ([`crate::exit`]), and as it fills.
//!
//! The buffering is `__smakebuf`'s, chosen at the first write: by lines
//! where fd 1 is a terminal, whole otherwise, in blocks of the fd's
//! `st_blksize` (`__swhatbuf`), `BUFSIZ` where it has none. A full buffer
//! is written a block at a time, as `__sfvwrite` fills it and flushes it,
//! or writes a block past it from an empty one, so what reaches fd 1
//! before an `fflush` is always whole blocks.
//!
//! It is one per process, as libc's `__sF[1]` and the standard library's
//! `Stdout` are, which it writes through: a forked child has a copy of
//! what its parent held, as it has a copy of libc's.

use std::io::{IsTerminal, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, PoisonError};

/// `BUFSIZ`, the block where `fstat` gives none.
const BUFSIZ: usize = if cfg!(target_os = "macos") {
    1024
} else {
    8192
};

/// The buffer, held behind the standard library's `Stdout`.
static STDOUT: Mutex<Buffer> = Mutex::new(Buffer {
    held: Vec::new(),
    made: None,
});

/// What stdio holds for `stdout`, and how it was made, once the first
/// write made it.
struct Buffer {
    held: Vec<u8>,
    made: Option<Made>,
}

/// How `__smakebuf` buffers a stream, with its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Made {
    /// A terminal's: written through each newline.
    Lines(usize),
    /// Written only as whole blocks, or by a flush.
    Blocks(usize),
}

impl Made {
    /// How fd 1 is buffered as it stands now.
    fn of_stdout() -> Made {
        let stdout = std::io::stdout();
        let block = (stdout.as_fd().try_clone_to_owned().ok())
            .and_then(|fd| std::fs::File::from(fd).metadata().ok())
            .and_then(|meta| usize::try_from(meta.blksize()).ok())
            .filter(|block| *block > 0)
            .unwrap_or(BUFSIZ);
        if stdout.is_terminal() {
            Made::Lines(block)
        } else {
            Made::Blocks(block)
        }
    }

    /// How many of `held`'s first bytes libc has written by now: a
    /// terminal's through its last newline, where each newline flushed
    /// the buffer, and then, as from an empty buffer, the whole blocks of
    /// what follows.
    fn written(self, held: &[u8]) -> usize {
        let (lines, block) = match self {
            Made::Lines(block) => (
                held.iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |at| at + 1),
                block,
            ),
            Made::Blocks(block) => (0, block),
        };
        let rest = held.len() - lines;
        lines + rest - rest % block
    }
}

/// `fwrite(bytes, 1, len, stdout)`: `bytes` held, and what the buffer
/// can no longer hold written.
pub fn write(bytes: &[u8]) {
    let mut buffer = STDOUT.lock().unwrap_or_else(PoisonError::into_inner);
    let made = *buffer.made.get_or_insert_with(Made::of_stdout);
    buffer.held.extend_from_slice(bytes);
    let written = made.written(&buffer.held);
    if written > 0 {
        out(buffer.held.get(..written).unwrap_or_default());
        buffer.held.drain(..written);
    }
}

/// `fflush(stdout)`: what is held written.
pub fn flush() {
    let mut buffer = STDOUT.lock().unwrap_or_else(PoisonError::into_inner);
    if !buffer.held.is_empty() {
        out(&buffer.held);
        buffer.held.clear();
    }
}

/// `bytes` to fd 1, after what the standard library's `Stdout` held, as
/// a body's `write(2)` already reached it.
fn out(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).and_then(|()| stdout.flush()).ok();
}

#[cfg(test)]
mod tests {
    use super::Made;

    /// A pipe's or a file's buffer is written in whole blocks only, as
    /// php-cgi's 40 startup warnings reach a pipe as two 16384-byte blocks
    /// before the body; a terminal's through its last newline, and the
    /// whole blocks after it, as `__sfvwrite` writes them.
    #[test]
    fn written_is_whole_blocks_or_lines() {
        assert_eq!(Made::Blocks(4).written(b"abc"), 0);
        assert_eq!(Made::Blocks(4).written(b"a\nbcdefghi"), 8);
        assert_eq!(Made::Lines(4).written(b"a\nb"), 2);
        assert_eq!(Made::Lines(4).written(b"abc"), 0);
        assert_eq!(Made::Lines(4).written(b"abcdef\ng"), 7);
        assert_eq!(Made::Lines(4).written(b"a\nbcdefghi"), 10);
    }
}
