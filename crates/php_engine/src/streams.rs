//! The streams layer (design §4.3 seam 5): php's `main/streams/streams.c`
//! as one generic half, [`PhpStream`], over the per-kind half, [`Ops`],
//! with the two wrappers this engine has so far, `php://`
//! (`Z/ext/standard/php_fopen_wrapper.c`) and the plain files wrapper
//! (`Z/main/streams/plain_wrapper.c`).
//!
//! A stream is a resource of type `stream`, as `php_stream_alloc`
//! registers one (`Z/main/streams/streams.c:301`, `stream->res`), whose
//! payload is the `php_stream` itself; what `fopen` hands back is that
//! resource (`php_stream_to_zval`), and `fclose` closes it, so the
//! resource's destructor, [`dtor`], is `php_stream_free` under
//! `PHP_STREAM_FREE_RSRC_DTOR`. An operation takes the stream out of its
//! resource for the call and puts it back after, which is what lets a
//! kind reach the request beside the stream: `php://temp` reads through
//! the memory stream it encloses, and the plain files kind raises php's
//! notice on a failed read. Nothing runs PHP code inside a call yet, so
//! the empty slot a re-entrant call would find is php's `in_free` guard
//! and not a case a program reaches.
//!
//! What is not here: `php://temp`'s spill to a file past
//! `PHP_STREAM_MAX_MEM`, the lazy body under `php://stdin` and
//! `php://input` (#544), `php://fd/` and `php://filter/`, the filter
//! chains, the read buffer's `DETECT_EOL` arm, the directory streams, and
//! every stream operation past read, write, seek, tell, eof, flush and
//! close.

use std::io::{Read, Seek, SeekFrom, Write};

use php_heap::ResourceType;

use crate::api::raising::{Level, Thrown, raise, throw, warning};
use crate::api::request::{Request, Stream, Whence};
use crate::api::values::{Resource, Value};

/// `FG(def_chunk_size)`, which `PHP_SOCK_CHUNK_SIZE` sets at 8192
/// (`Z/ext/standard/file.c:131`): what one fill of the read buffer asks
/// the kind for.
const CHUNK_SIZE: usize = 8192;

/// `php_stream_ops`, the per-kind half: the label `stream_get_meta_data`
/// prints as `stream_type`, and the operations, each answering as php's
/// does, with `None` for the `-1` an operation fails with. A kind holds
/// its own `abstract` as its fields.
pub(crate) trait Ops: Send {
    /// `ops->label`.
    fn label(&self) -> &'static str;

    /// `ops->write`: how many bytes were taken, `None` for `-1`.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize>;

    /// `ops->read`: how many bytes were given, `eof` set where the kind
    /// sets `stream->eof`, `None` for `-1`.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize>;

    /// `ops->close`, with what the kind holds.
    fn close(&mut self, req: &mut Request);

    /// `ops->flush`: `true` for php's `0`.
    fn flush(&mut self, _req: &mut Request) -> bool {
        true
    }

    /// `php_stream_encloses`: the stream this kind reads and writes
    /// through, which only `php://temp` has.
    fn enclose(&mut self, _inner: Stream) {}

    /// `ops->seek`, or `None` where the kind has none: `position` is
    /// php's `*newoffs`, written as the kind writes it, which the
    /// memory kind does on failure too; `Some(true)` for php's `0`.
    fn seek(
        &mut self,
        _req: &mut Request,
        _offset: i64,
        _whence: Whence,
        _position: &mut i64,
    ) -> Option<bool> {
        None
    }
}

/// php's `php_stream` (`Z/main/php_streams.h`), the half every kind
/// shares: the position, `eof`, the read buffer with its two
/// cursors, the wrapper it was opened through, and the bits the
/// generic operations read. `stream->mode` and `stream->orig_path`,
/// which `stream_get_meta_data` prints, wait for it.
pub(crate) struct PhpStream {
    ops: Box<dyn Ops>,
    /// `stream->wrapper->wops->label`.
    wrapper: &'static str,
    /// `stream->position`, `-1` for a stream that cannot seek.
    position: i64,
    /// `stream->eof`.
    eof: bool,
    /// `stream->fatal_error`.
    fatal_error: bool,
    /// `stream->flags`.
    flags: Flags,
    /// `stream->readbuf` over `readbuflen`.
    readbuf: Vec<u8>,
    /// `stream->readpos`.
    readpos: usize,
    /// `stream->writepos`.
    writepos: usize,
    /// `stream->chunk_size`.
    chunk_size: usize,
}

/// `stream->flags`, the three of `PHP_STREAM_FLAG_*` the generic
/// operations read.
#[derive(Default)]
struct Flags {
    /// `PHP_STREAM_FLAG_NO_BUFFER`: the kind is read directly.
    no_buffer: bool,
    /// `PHP_STREAM_FLAG_NO_SEEK`.
    no_seek: bool,
    /// `PHP_STREAM_FLAG_WAS_WRITTEN`: a flush is owed at close.
    was_written: bool,
}

impl PhpStream {
    /// `_php_stream_alloc` (`Z/main/streams/streams.c:301`) less the
    /// resource, which [`Request::stream_alloc`] registers: the kind,
    /// the chunk size, and nothing read or written.
    fn new(ops: Box<dyn Ops>) -> PhpStream {
        PhpStream {
            ops,
            wrapper: "",
            position: 0,
            eof: false,
            fatal_error: false,
            flags: Flags::default(),
            readbuf: Vec::new(),
            readpos: 0,
            writepos: 0,
            chunk_size: CHUNK_SIZE,
        }
    }

    /// `_php_stream_tell` (`Z/main/streams/streams.c:1380`).
    pub(crate) const fn position(&self) -> i64 {
        self.position
    }

    /// `_php_stream_fill_read_buffer` (`Z/main/streams/streams.c:564`)
    /// without a filter chain: nothing when the buffer already holds
    /// `size` bytes; otherwise the buffer compacted and grown by one
    /// chunk where the chunk would not fit, and one read of the kind
    /// into the room after `writepos`. `false` is the read that failed.
    fn fill_read_buffer(&mut self, req: &mut Request, size: usize) -> bool {
        if self.writepos - self.readpos >= size {
            return true;
        }
        if !self.readbuf.is_empty() && self.readbuf.len() - self.writepos < self.chunk_size {
            self.readbuf.copy_within(self.readpos..self.writepos, 0);
            self.writepos -= self.readpos;
            self.readpos = 0;
        }
        if self.readbuf.len() - self.writepos < self.chunk_size {
            self.readbuf.resize(self.readbuf.len() + self.chunk_size, 0);
        }
        let Self {
            ops,
            eof,
            readbuf,
            writepos,
            ..
        } = self;
        match ops.read(req, &mut readbuf[*writepos..], eof) {
            Some(justread) => {
                *writepos += justread;
                true
            }
            None => false,
        }
    }

    /// `_php_stream_read` (`Z/main/streams/streams.c:736`): the buffer
    /// drained first, then the kind read directly for an unbuffered
    /// stream and through the buffer otherwise, looping while `buf` has
    /// room for a plain file, `php://memory` and `php://temp` and
    /// stopping at the first read for every other kind; `None` is the
    /// read that failed before a byte was had.
    pub(crate) fn read(&mut self, req: &mut Request, buf: &mut [u8]) -> Option<usize> {
        let greedy = self.greedy();
        let mut didread = 0;
        let mut size = buf.len();
        while size > 0 {
            if self.writepos > self.readpos {
                let toread = (self.writepos - self.readpos).min(size);
                buf[didread..didread + toread]
                    .copy_from_slice(&self.readbuf[self.readpos..self.readpos + toread]);
                self.readpos += toread;
                size -= toread;
                didread += toread;
            }
            if size == 0 {
                break;
            }
            let toread = if self.flags.no_buffer || self.chunk_size == 1 {
                let Self { ops, eof, .. } = self;
                match ops.read(req, &mut buf[didread..], eof) {
                    Some(n) => n,
                    None if didread == 0 => return None,
                    None => break,
                }
            } else {
                if !self.fill_read_buffer(req, size) {
                    if didread == 0 {
                        return None;
                    }
                    break;
                }
                let toread = (self.writepos - self.readpos).min(size);
                if toread > 0 {
                    buf[didread..didread + toread]
                        .copy_from_slice(&self.readbuf[self.readpos..self.readpos + toread]);
                    self.readpos += toread;
                }
                toread
            };
            if toread > 0 {
                didread += toread;
                size -= toread;
            } else {
                break;
            }
            if !greedy {
                break;
            }
        }
        if didread > 0 {
            self.position += i64::try_from(didread).unwrap_or(i64::MAX);
        }
        Some(didread)
    }

    /// The three kinds `_php_stream_read` keeps reading for until the
    /// caller's buffer is full: `php_plain_files_wrapper`,
    /// `php_stream_memory_ops` and `php_stream_temp_ops`.
    fn greedy(&self) -> bool {
        self.wrapper == PLAIN_FILES || matches!(self.ops.label(), "MEMORY" | "TEMP")
    }

    /// `_php_stream_eof` (`Z/main/streams/streams.c:840`): not while the
    /// buffer holds a byte, else what the kind's last read said.
    pub(crate) const fn eof(&self) -> bool {
        if self.writepos > self.readpos {
            return false;
        }
        self.eof
    }

    /// `_php_stream_write` (`Z/main/streams/streams.c:1333`) over
    /// `_php_stream_write_buffer` (`:1189`): nothing for no bytes; a
    /// seekable stream with bytes still in its read buffer drops them
    /// and seeks the kind to the position first; then the kind written
    /// until every byte is taken or one write takes none, `None` where
    /// the first took none and php returned its `-1`. A write that
    /// took a byte marks the flush owed at close.
    pub(crate) fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if !self.flags.no_seek && self.readpos != self.writepos {
            self.readpos = 0;
            self.writepos = 0;
            let Self { ops, position, .. } = self;
            let _ = ops.seek(req, *position, Whence::Set, position);
        }
        let mut didwrite = 0;
        let mut rest = buf;
        while !rest.is_empty() {
            let Self { ops, .. } = self;
            match ops.write(req, rest) {
                Some(justwrote) if justwrote > 0 => {
                    rest = &rest[justwrote..];
                    didwrite += justwrote;
                    self.position += i64::try_from(justwrote).unwrap_or(i64::MAX);
                }
                _ if didwrite == 0 => return None,
                _ => break,
            }
        }
        if didwrite > 0 {
            self.flags.was_written = true;
        }
        Some(didwrite)
    }

    /// `_php_stream_seek` (`Z/main/streams/streams.c:1385`): a seek that
    /// lands inside the read buffer moves the cursor and nothing else;
    /// otherwise a kind that seeks is asked, `SEEK_CUR` turned into
    /// `SEEK_SET` from the position first, a negative `SEEK_SET` refused
    /// before the kind is, and the buffer dropped whatever the kind
    /// answered; a kind that cannot seek emulates a forward `SEEK_CUR`
    /// by reading and is php's warning for anything else.
    pub(crate) fn seek(&mut self, req: &mut Request, offset: i64, whence: Whence) -> bool {
        if !self.flags.no_buffer {
            let held = i64::try_from(self.writepos - self.readpos).unwrap_or(i64::MAX);
            match whence {
                Whence::Cur if offset > 0 && offset <= held => {
                    self.readpos += usize::try_from(offset).unwrap_or(0);
                    self.position += offset;
                    self.eof = false;
                    self.fatal_error = false;
                    return true;
                }
                Whence::Set if offset > self.position && offset <= self.position + held => {
                    self.readpos += usize::try_from(offset - self.position).unwrap_or(0);
                    self.position = offset;
                    self.eof = false;
                    self.fatal_error = false;
                    return true;
                }
                _ => {}
            }
        }
        if !self.flags.no_seek {
            let (offset, whence) = match whence {
                Whence::Cur => (self.position.saturating_add(offset), Whence::Set),
                Whence::Set if offset < 0 => return false,
                other => (offset, other),
            };
            let Self { ops, position, .. } = self;
            if let Some(sought) = ops.seek(req, offset, whence, position) {
                if sought {
                    self.eof = false;
                    self.fatal_error = false;
                }
                self.readpos = 0;
                self.writepos = 0;
                return sought;
            }
        }
        if whence == Whence::Cur && offset >= 0 {
            let mut left = usize::try_from(offset).unwrap_or(usize::MAX);
            let mut tmp = [0u8; 1024];
            while left > 0 {
                let take = left.min(tmp.len());
                match self.read(req, &mut tmp[..take]) {
                    Some(didread) if didread > 0 => left -= didread,
                    _ => return false,
                }
            }
            return true;
        }
        let text = format!("{}(): Stream does not support seeking", req.active_native());
        warning(req, &text);
        false
    }

    /// `_php_stream_flush` (`Z/main/streams/streams.c:1316`) without a
    /// filter chain: the owed flush cleared and the kind flushed.
    pub(crate) fn flush(&mut self, req: &mut Request) -> bool {
        self.flags.was_written = false;
        let Self { ops, .. } = self;
        ops.flush(req)
    }

    /// `_php_stream_free` under `PHP_STREAM_FREE_CLOSE`
    /// (`Z/main/streams/streams.c:388`), what the resource's destructor
    /// runs, so `fclose`, the last owner's release and the shutdown walk
    /// are one path: the owed flush, then the kind closed, then the
    /// stream dropped.
    fn free(mut self, req: &mut Request) {
        if self.flags.was_written {
            self.flush(req);
        }
        let Self { ops, .. } = &mut self;
        ops.close(req);
    }
}

impl Whence {
    /// The number `fseek()` is given, `SEEK_SET`, `SEEK_CUR` or
    /// `SEEK_END`, and `None` for one that is none of the three, which
    /// `_php_stream_seek`'s kinds answer `-1` to.
    #[must_use]
    pub fn from_int(whence: i64) -> Option<Whence> {
        match whence {
            0 => Some(Whence::Set),
            1 => Some(Whence::Cur),
            2 => Some(Whence::End),
            _ => None,
        }
    }
}

// ------------------------------------------------------------- memory

/// `TEMP_STREAM_DEFAULT`, `TEMP_STREAM_READONLY` and `TEMP_STREAM_APPEND`
/// (`Z/main/php_memory_streams.h`), what `php_stream_mode_from_str`
/// makes of `fopen`'s `$mode` for `php://memory` and `php://temp`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TempMode {
    Default,
    ReadOnly,
    Append,
}

impl TempMode {
    /// `php_stream_mode_from_str` (`Z/main/streams/memory.c:249`): an
    /// `a` anywhere is append, else a `w` or a `+` is read and write,
    /// else read only.
    fn of(mode: &[u8]) -> TempMode {
        if mode.contains(&b'a') {
            TempMode::Append
        } else if mode.iter().any(|b| matches!(b, b'w' | b'+')) {
            TempMode::Default
        } else {
            TempMode::ReadOnly
        }
    }
}

/// `php_stream_memory_data` under `php_stream_memory_ops`
/// (`Z/main/streams/memory.c:36`): the bytes, the position in them, and
/// which of the three ways it was opened. The label is `MEMORY` for `php://memory`; the request's
/// body read through the same bytes carries the label of the kind php
/// reads it with, since nothing in the engine reads a body lazily yet
/// (#544).
struct Memory {
    data: Vec<u8>,
    fpos: usize,
    mode: TempMode,
    label: &'static str,
}

impl Ops for Memory {
    fn label(&self) -> &'static str {
        self.label
    }

    /// `php_stream_memory_write` (`Z/main/streams/memory.c:44`): `-1` on
    /// a read-only stream; an append stream writes at the end; a write
    /// past the end zero-fills the gap a seek left.
    fn write(&mut self, _req: &mut Request, buf: &[u8]) -> Option<usize> {
        if self.mode == TempMode::ReadOnly {
            return None;
        }
        if self.mode == TempMode::Append {
            self.fpos = self.data.len();
        }
        if self.fpos + buf.len() > self.data.len() {
            self.data.resize(self.fpos + buf.len(), 0);
        }
        self.data[self.fpos..self.fpos + buf.len()].copy_from_slice(buf);
        self.fpos += buf.len();
        Some(buf.len())
    }

    /// `php_stream_memory_read` (`Z/main/streams/memory.c:75`): eof and
    /// nothing at or past the end, else up to what is left.
    fn read(&mut self, _req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        if self.fpos >= self.data.len() {
            *eof = true;
            return Some(0);
        }
        let count = buf.len().min(self.data.len() - self.fpos);
        buf[..count].copy_from_slice(&self.data[self.fpos..self.fpos + count]);
        self.fpos += count;
        Some(count)
    }

    /// `php_stream_memory_close` (`Z/main/streams/memory.c:97`): the
    /// bytes released.
    fn close(&mut self, _req: &mut Request) {
        self.data = Vec::new();
    }

    /// `php_stream_memory_seek` (`Z/main/streams/memory.c:120`): a
    /// position before the start is refused with the position left at
    /// zero and `*newoffs` at `-1`; `SEEK_END` past the end and
    /// `SEEK_SET` anywhere at or past zero are taken as they are.
    fn seek(
        &mut self,
        _req: &mut Request,
        offset: i64,
        whence: Whence,
        position: &mut i64,
    ) -> Option<bool> {
        let len = i64::try_from(self.data.len()).unwrap_or(i64::MAX);
        let fpos = i64::try_from(self.fpos).unwrap_or(i64::MAX);
        let target = match whence {
            Whence::Cur => fpos.saturating_add(offset),
            Whence::Set => offset,
            Whence::End => len.saturating_add(offset),
        };
        if target < 0 {
            self.fpos = 0;
            *position = -1;
            return Some(false);
        }
        self.fpos = usize::try_from(target).unwrap_or(usize::MAX);
        *position = target;
        Some(true)
    }
}

/// `_php_stream_memory_create` (`Z/main/streams/memory.c:271`): empty,
/// at zero, unbuffered.
fn memory(mode: TempMode) -> PhpStream {
    let mut stream = PhpStream::new(Box::new(Memory {
        data: Vec::new(),
        fpos: 0,
        mode,
        label: "MEMORY",
    }));
    stream.flags.no_buffer = true;
    stream
}

/// `php_stream_temp_data` under `php_stream_temp_ops`
/// (`Z/main/streams/memory.c:337`): a memory stream it encloses, its own
/// resource as php's is, so a `php://temp` takes two ids as the oracle's
/// does; `None` where php's `innerstream` is `NULL`. The spill to a file
/// past `PHP_STREAM_MAX_MEM` is not here.
struct Temp {
    inner: Option<Stream>,
}

impl Ops for Temp {
    fn label(&self) -> &'static str {
        "TEMP"
    }

    fn enclose(&mut self, inner: Stream) {
        self.inner = Some(inner);
    }

    /// `php_stream_temp_write` (`Z/main/streams/memory.c:350`) less the
    /// spill: the inner stream written.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        req.stream_op(self.inner?, |req, inner| inner.write(req, buf))
            .flatten()
    }

    /// `php_stream_temp_read` (`Z/main/streams/memory.c:381`): the inner
    /// stream read and its eof taken.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let (got, inner_eof) =
            req.stream_op(self.inner?, |req, inner| (inner.read(req, buf), inner.eof))?;
        *eof = inner_eof;
        got
    }

    /// `php_stream_temp_close` (`Z/main/streams/memory.c:399`),
    /// `php_stream_free_enclosed(innerstream, PHP_STREAM_FREE_CLOSE)`:
    /// the inner resource closed and its one owner given up.
    fn close(&mut self, req: &mut Request) {
        if let Some(inner) = self.inner.take() {
            inner.resource().close(req);
            req.release(inner.resource().as_value());
        }
    }

    /// `php_stream_temp_flush` (`Z/main/streams/memory.c:418`).
    fn flush(&mut self, req: &mut Request) -> bool {
        self.inner
            .and_then(|inner| req.stream_op(inner, |req, inner| inner.flush(req)))
            .unwrap_or(false)
    }

    /// `php_stream_temp_seek` (`Z/main/streams/memory.c:428`): the inner
    /// stream sought, its position and eof taken.
    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Whence,
        position: &mut i64,
    ) -> Option<bool> {
        let Some(inner) = self.inner else {
            *position = -1;
            return Some(false);
        };
        let (sought, at) = req.stream_op(inner, |req, inner| {
            (inner.seek(req, offset, whence), inner.position)
        })?;
        *position = at;
        Some(sought)
    }
}

// -------------------------------------------------------- plain files

/// `php_plain_files_wrapper.wops->label`.
const PLAIN_FILES: &str = "plainfile";

/// `php_stream_php_wrapper.wops->label`.
const PHP_WRAPPER: &str = "PHP";

/// `php_stdio_stream_data` under `php_stream_stdio_ops`
/// (`Z/main/streams/plain_wrapper.c:135`): the file, and whether it
/// seeks.
struct Stdio {
    file: std::fs::File,
    is_seekable: bool,
}

/// `php_socket_strerror_s` over `errno`, as the plain files kind's two
/// notices print it: the number and the text, without the suffix Rust
/// adds.
fn errno(error: &std::io::Error) -> String {
    format!(
        "errno={} {}",
        error.raw_os_error().unwrap_or(0),
        strerror(error)
    )
}

/// `strerror(errno)`, which is the text Rust's error carries before its
/// own ` (os error N)`.
fn strerror(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.split_once(" (os error")
        .map_or(text.clone(), |(text, _)| text.to_owned())
}

/// `EBADF`, the one `errno` `php_stdiop_read` leaves eof clear on.
const EBADF: i32 = 9;

impl Ops for Stdio {
    fn label(&self) -> &'static str {
        "STDIO"
    }

    /// `php_stdiop_write` (`Z/main/streams/plain_wrapper.c:374`): one
    /// `write(2)`, and php's notice with `errno` where it failed.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        match self.file.write(buf) {
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => None,
            Err(error) => {
                let text = format!(
                    "{}(): Write of {} bytes failed with {}",
                    req.active_native(),
                    buf.len(),
                    errno(&error)
                );
                raise(req, Level::Notice, &text);
                None
            }
        }
    }

    /// `php_stdiop_read` (`Z/main/streams/plain_wrapper.c:421`): one
    /// `read(2)`, eof at zero bytes, and php's notice with `errno` where
    /// it failed, eof set then too unless the fd was bad, `EBADF`.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        match self.file.read(buf) {
            Ok(0) => {
                *eof = true;
                Some(0)
            }
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => None,
            Err(error) => {
                let text = format!(
                    "{}(): Read of {} bytes failed with {}",
                    req.active_native(),
                    buf.len(),
                    errno(&error)
                );
                raise(req, Level::Notice, &text);
                if error.raw_os_error() != Some(EBADF) {
                    *eof = true;
                }
                None
            }
        }
    }

    /// `php_stdiop_close` (`Z/main/streams/plain_wrapper.c:508`): the
    /// file closed, which its drop does.
    fn close(&mut self, _req: &mut Request) {}

    /// `php_stdiop_seek` (`Z/main/streams/plain_wrapper.c:616`): php's
    /// warning on a stream that cannot seek, else `lseek(2)` and the
    /// offset it answered.
    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Whence,
        position: &mut i64,
    ) -> Option<bool> {
        if !self.is_seekable {
            let text = format!("{}(): Cannot seek on this stream", req.active_native());
            warning(req, &text);
            return Some(false);
        }
        let from = match whence {
            Whence::Set => SeekFrom::Start(u64::try_from(offset).unwrap_or(0)),
            Whence::Cur => SeekFrom::Current(offset),
            Whence::End => SeekFrom::End(offset),
        };
        match self.file.seek(from) {
            Ok(at) => {
                *position = i64::try_from(at).unwrap_or(i64::MAX);
                Some(true)
            }
            Err(_) => Some(false),
        }
    }
}

/// `php_stream_parse_fopen_modes` (`Z/main/streams/plain_wrapper.c:75`)
/// as `open(2)`'s flags: the first byte says what to do with the file,
/// a `+` anywhere opens it for both, and `None` is a first byte php
/// does not know.
fn open_options(mode: &[u8]) -> Option<std::fs::OpenOptions> {
    let mut options = std::fs::OpenOptions::new();
    let plus = mode.contains(&b'+');
    match mode.first()? {
        b'r' => {
            options.read(true).write(plus);
        }
        b'w' => {
            options.read(plus).write(true).create(true).truncate(true);
        }
        b'a' => {
            options.read(plus).append(true).create(true);
        }
        b'x' => {
            options.read(plus).write(true).create_new(true);
        }
        b'c' => {
            options.read(plus).write(true).create(true);
        }
        _ => return None,
    }
    Some(options)
}

/// `_php_stream_fopen` (`Z/main/streams/plain_wrapper.c:1151`) and
/// `_php_stream_fopen_from_fd` (`:303`): `$mode` parsed, the path
/// expanded against the working directory, `open(2)` with `0666`, then
/// whether the file seeks (`detect_is_seekable`: not a fifo or a
/// character device), and the position `lseek(SEEK_CUR)` answers, zero
/// for a fresh open and `-1` with `NO_SEEK` for one that cannot. The
/// `Err` is what `php_stream_display_wrapper_errors` prints after
/// `Failed to open stream: `.
fn plain_open(req: &Request, path: &[u8], mode: &[u8]) -> Result<PhpStream, String> {
    let Some(options) = open_options(mode) else {
        return Err(format!(
            "`{}' is not a valid mode for fopen",
            String::from_utf8_lossy(mode)
        ));
    };
    let mut file = options
        .open(req.absolutize(path).0)
        .map_err(|error| strerror(&error))?;
    let is_seekable = file.metadata().map_or(true, |meta| {
        use std::os::unix::fs::FileTypeExt;
        let kind = meta.file_type();
        !(kind.is_fifo() || kind.is_char_device())
    });
    let position = if is_seekable {
        file.stream_position()
            .map_or(-1, |at| i64::try_from(at).unwrap_or(i64::MAX))
    } else {
        -1
    };
    let mut stream = PhpStream::new(Box::new(Stdio { file, is_seekable }));
    stream.flags.no_seek = !is_seekable;
    stream.position = position;
    Ok(stream)
}

// --------------------------------------------------------- php://

/// The host's standard streams as `php://stdout` and `php://stderr`
/// reach them under the CLI: fd 1 is the body the sink is handed, past
/// the `php_output` layer (`sapi_cli_ub_write`, since php's CLI writes
/// both to the one fd), and fd 2 is the sink's error stream (design
/// revision 16). Reading either is the `EBADF` a write-only fd
/// answers.
struct HostFd {
    stderr: bool,
}

impl Ops for HostFd {
    fn label(&self) -> &'static str {
        "STDIO"
    }

    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        if self.stderr {
            req.write_stderr(buf);
        } else {
            req.ub_write(buf);
        }
        Some(buf.len())
    }

    fn read(&mut self, req: &mut Request, buf: &mut [u8], _eof: &mut bool) -> Option<usize> {
        let text = format!(
            "{}(): Read of {} bytes failed with errno={EBADF} Bad file descriptor",
            req.active_native(),
            buf.len()
        );
        raise(req, Level::Notice, &text);
        None
    }

    fn close(&mut self, _req: &mut Request) {}
}

/// `php_stream_output_ops` (`Z/ext/standard/php_fopen_wrapper.c:52`),
/// `php://output`: a write is `PHPWRITE`, through the `php_output` layer, a
/// read is eof and `-1`, and there is no seek.
struct Output;

impl Ops for Output {
    fn label(&self) -> &'static str {
        "Output"
    }

    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        req.write(buf);
        Some(buf.len())
    }

    fn read(&mut self, _req: &mut Request, _buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        *eof = true;
        None
    }

    fn close(&mut self, _req: &mut Request) {}
}

/// What one wrapper's opener answered: the stream, or the message
/// `php_stream_display_wrapper_errors` prints after `Failed to open
/// stream: `, which is what the wrapper logged
/// (`php_stream_wrapper_log_error`), `strerror(errno)` for the plain
/// files wrapper, and `operation failed` for any other that logged
/// nothing.
type Opened = Result<Stream, String>;

/// `php_stream_url_wrap_php` (`Z/ext/standard/php_fopen_wrapper.c:176`):
/// the path after `php://`, case-insensitively, as `temp`, with
/// `/maxmemory:N` after it, `memory`, `output`, `input`, `stdin`,
/// `stdout` or `stderr`; anything else is php's `Invalid php:// URL
/// specified` and nothing opened. `fd/` and `filter/` are not here and
/// open nothing.
fn php_open(req: &mut Request, path: &[u8], mode: &[u8]) -> Result<Opened, Thrown> {
    let path = if path.len() >= 6 && path[..6].eq_ignore_ascii_case(b"php://") {
        &path[6..]
    } else {
        path
    };
    let lower = path.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix(b"temp") {
        if let Some(digits) = rest.strip_prefix(b"/maxmemory:") {
            // `ZEND_STRTOL(path, NULL, 10)`: the sign and the digits
            // it can read, and a value below zero refused.
            if digits.first() == Some(&b'-') && digits.get(1).is_some_and(u8::is_ascii_digit) {
                return Err(crate::api::raising::arg_value_error(
                    req,
                    2,
                    "be greater than or equal to 0",
                ));
            }
        }
        return Ok(Ok(temp(req, TempMode::of(mode))?));
    }
    let stream = match lower.as_slice() {
        b"memory" => memory(TempMode::of(mode)),
        b"output" => PhpStream::new(Box::new(Output)),
        b"stdin" | b"input" => {
            let label = if lower == b"stdin" { "STDIO" } else { "Input" };
            let mut stream = PhpStream::new(Box::new(Memory {
                data: req.body_bytes().to_vec(),
                fpos: 0,
                mode: TempMode::ReadOnly,
                label,
            }));
            stream.flags.no_seek = true;
            stream.position = -1;
            stream
        }
        b"stdout" | b"stderr" => {
            let mut stream = PhpStream::new(Box::new(HostFd {
                stderr: lower == b"stderr",
            }));
            stream.flags.no_seek = true;
            stream.position = -1;
            stream
        }
        _ if lower.starts_with(b"fd/") || lower.starts_with(b"filter/") => {
            return Ok(Err(String::from("operation failed")));
        }
        _ => {
            let text = format!("{}(): Invalid php:// URL specified", req.active_native());
            warning(req, &text);
            return Ok(Err(String::from("operation failed")));
        }
    };
    Ok(Ok(req.stream_alloc(stream)?))
}

/// `_php_stream_temp_create_ex` (`Z/main/streams/memory.c:519`): the
/// temp stream registered first, then the memory stream it encloses, so
/// the two ids come out in php's order.
fn temp(req: &mut Request, mode: TempMode) -> Result<Stream, Thrown> {
    let mut outer = PhpStream::new(Box::new(Temp { inner: None }));
    outer.flags.no_buffer = true;
    let outer = req.stream_alloc(outer)?;
    let inner = req.stream_alloc(memory(mode))?;
    req.stream_op(outer, |_, stream| stream.ops.enclose(inner));
    Ok(outer)
}

// ---------------------------------------------------------- wrappers

/// Which wrapper `php_stream_locate_url_wrapper`
/// (`Z/main/streams/streams.c:2010`) found for a path: the plain files
/// wrapper for no scheme and for `file://`, the `php://` wrapper, one
/// the program registered, or none.
enum Located {
    Plain,
    Php,
    User(usize),
    Nothing,
}

impl Request {
    /// `php_stream_locate_url_wrapper` (`Z/main/streams/streams.c:2010`):
    /// a scheme is alphanumerics with `+`, `-` and `.`, two bytes or
    /// more, followed by `://`; one nobody registered is php's warning
    /// and then the plain files wrapper over the path as written;
    /// `file://` with a host that is not `localhost` is php's warning and
    /// no wrapper, and otherwise the path after the slashes.
    fn locate_wrapper(&mut self, path: &[u8]) -> (Located, Vec<u8>) {
        let n = path
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            .count();
        let protocol = (n > 1 && path.get(n) == Some(&b':') && path[n + 1..].starts_with(b"//"))
            .then(|| path[..n].to_ascii_lowercase());
        let mut wrapper = None;
        if let Some(scheme) = &protocol {
            wrapper = match scheme.as_slice() {
                b"php" => Some(Located::Php),
                b"file" => Some(Located::Plain),
                _ => self
                    .wrappers
                    .iter()
                    .position(|w| w.scheme.as_bytes().eq_ignore_ascii_case(scheme))
                    .map(Located::User),
            };
            if wrapper.is_none() {
                let text = format!(
                    "{}(): Unable to find the wrapper \"{}\" - did you forget to enable it when you configured PHP?",
                    self.active_native(),
                    String::from_utf8_lossy(&scheme[..scheme.len().min(31)])
                );
                warning(self, &text);
            }
        }
        match wrapper {
            Some(Located::Plain) => {
                let localhost =
                    path.len() >= 17 && path[..17].eq_ignore_ascii_case(b"file://localhost/");
                if !localhost && path.get(n + 3).is_some_and(|b| *b != b'/') {
                    let text = format!(
                        "{}(): Remote host file access not supported, {}",
                        self.active_native(),
                        String::from_utf8_lossy(path)
                    );
                    warning(self, &text);
                    return (Located::Nothing, path.to_vec());
                }
                let mut at = n + 1;
                if localhost {
                    at += 11;
                }
                while path.get(at + 1) == Some(&b'/') {
                    at += 1;
                }
                (Located::Plain, path[at..].to_vec())
            }
            Some(found) => (found, path.to_vec()),
            None => (Located::Plain, path.to_vec()),
        }
    }

    /// `_php_stream_open_wrapper_ex` (`Z/main/streams/streams.c:2233`)
    /// under `REPORT_ERRORS`: an empty path is php's `ValueError`; with
    /// `USE_PATH` the path is resolved through `include_path` first; the
    /// wrapper is located and asked; a stream opened for append learns
    /// its position from the kind; and a stream not opened is
    /// `php_stream_display_wrapper_errors`'s warning, `Failed to open
    /// stream: ` and the message, at the path as written.
    pub(crate) fn open_stream_wrapper(
        &mut self,
        path: &[u8],
        mode: &[u8],
        use_include_path: bool,
    ) -> Result<Option<Stream>, Thrown> {
        if path.is_empty() {
            return Err(throw(self, "ValueError", "Path must not be empty"));
        }
        let mut to_open = path.to_vec();
        if use_include_path && let Ok(resolved) = self.resolve_path(path) {
            to_open = resolved;
        }
        let (wrapper, path_to_open) = self.locate_wrapper(&to_open);
        let (label, opened) = match wrapper {
            Located::Plain => (
                PLAIN_FILES,
                match plain_open(self, &path_to_open, mode) {
                    Ok(stream) => Ok(self.stream_alloc(stream)?),
                    Err(message) => Err(message),
                },
            ),
            Located::Php => (PHP_WRAPPER, php_open(self, &path_to_open, mode)?),
            Located::User(at) => {
                let open = self.wrappers[at].open;
                (
                    "user-space",
                    open(self, &path_to_open, mode)?
                        .ok_or_else(|| String::from("operation failed")),
                )
            }
            Located::Nothing => ("", Err(String::from("no suitable wrapper could be found"))),
        };
        match opened {
            Ok(stream) => {
                let append = mode.contains(&b'a');
                self.stream_op(stream, |req, s| {
                    s.wrapper = label;
                    if append && !s.flags.no_seek && s.position == 0 {
                        let PhpStream { ops, position, .. } = s;
                        let mut newpos = 0;
                        if ops.seek(req, 0, Whence::Cur, &mut newpos) == Some(true) {
                            *position = newpos;
                        }
                    }
                });
                Ok(Some(stream))
            }
            Err(message) => {
                self.display_wrapper_errors(&to_open, "Failed to open stream", &message);
                Ok(None)
            }
        }
    }

    /// `php_stream_display_wrapper_errors` (`Z/main/streams/streams.c:175`)
    /// as `php_error_docref1` renders it: the native, the path in
    /// parentheses, the caption and the message.
    pub(crate) fn display_wrapper_errors(&mut self, path: &[u8], caption: &str, message: &str) {
        let text = format!(
            "{}({}): {caption}: {message}",
            self.active_native(),
            String::from_utf8_lossy(path)
        );
        warning(self, &text);
    }

    /// `php_stream_alloc`'s `zend_register_resource(ret, le_stream)`: the
    /// stream as the payload of a resource of the `stream` type, one
    /// owner, under the next id.
    pub(crate) fn stream_alloc(&mut self, stream: PhpStream) -> Result<Stream, Thrown> {
        let kind = self.stream_type();
        let res = self.new_resource(kind, Some(stream))?;
        Ok(Stream::of(res))
    }

    /// One operation on a stream with the request free beside it: the
    /// stream taken out of its resource for the call and put back
    /// after. `None` is a resource that is closed, is not a stream, or
    /// is inside a call already, which is php's `in_free` guard. A
    /// stream whose resource was closed while it was out is freed here,
    /// since the destructor found the slot empty.
    pub(crate) fn stream_op<R>(
        &mut self,
        stream: Stream,
        f: impl FnOnce(&mut Request, &mut PhpStream) -> R,
    ) -> Option<R> {
        let res = stream.resource().heap();
        let mut taken = self.resource_native_mut::<Option<PhpStream>>(res)?.take()?;
        let out = f(self, &mut taken);
        match self.resource_native_mut::<Option<PhpStream>>(res) {
            Some(slot) => *slot = Some(taken),
            None => taken.free(self),
        }
        Some(out)
    }

    /// The stream a resource holds, read: `None` where it is closed, is
    /// not a stream, or is inside a call.
    pub(crate) fn stream_ref(&self, stream: Stream) -> Option<&PhpStream> {
        self.resource_native::<Option<PhpStream>>(stream.resource().heap())?
            .as_ref()
    }

    /// `php_stream_from_zval_no_verify_no_error`: the stream a value
    /// names, which is an open resource of the `stream` type.
    pub(crate) fn stream_of(&self, value: &Value) -> Option<Stream> {
        let r = value.as_resource()?;
        (r.kind() == Some(self.stream_type())).then(|| Stream::of(*r))
    }

    /// `php_stream_read_to_str` (`Z/main/streams/streams.c:822`): a
    /// string of `len` asked for first, so a length past the memory
    /// limit is php's fatal before a byte is read, then one read into
    /// it and the string cut to what was read; `None` is the read that
    /// failed.
    pub(crate) fn stream_read_to_str(
        &mut self,
        stream: Stream,
        len: usize,
    ) -> Result<Option<crate::api::values::PhpString>, Thrown> {
        if len > self.heap_memory_limit() {
            let asked = self.string_with(len, |_| {})?;
            self.release(Value::string(asked));
        }
        let mut buf = vec![0u8; len];
        let Some(read) = self.stream_op(stream, |req, s| s.read(req, &mut buf)) else {
            return Ok(None);
        };
        match read {
            Some(n) => Ok(Some(crate::api::values::PhpString::new(self, &buf[..n])?)),
            None => Ok(None),
        }
    }
}

/// `stream_resource_regular_dtor` (`Z/main/streams/streams.c:1873`):
/// the stream freed under `PHP_STREAM_FREE_CLOSE |
/// PHP_STREAM_FREE_RSRC_DTOR`, with the resource already closed.
pub(crate) fn dtor(req: &mut Request, r: Resource) {
    let Some(stream) = req
        .resource_native_mut::<Option<PhpStream>>(r.heap())
        .and_then(Option::take)
    else {
        return;
    };
    stream.free(req);
}

/// The `stream` resource type, `le_stream`, as `PHP_MINIT(streams)`
/// registers it (`Z/main/streams/streams.c:1910`).
pub(crate) fn register(resources: &mut crate::api::request::Resources) -> ResourceType {
    resources.register_type(b"stream", Some(dtor))
}
