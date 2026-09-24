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
//! The operations are php's generic half over the kind's: read, write,
//! seek, tell, eof, flush and close, `php_stream_get_line`,
//! `_php_stream_copy_to_mem`, `php_stream_stat`, the truncate and locking
//! options of `php_stream_set_option`, the read buffer's, and what
//! `stream_get_meta_data` reads (`stream->mode`, `stream->orig_path`, the
//! kind's own `PHP_STREAM_OPTION_META_DATA_API` answer). `php://temp`
//! spills to a file past its `PHP_STREAM_MAX_MEM`, and the plain files
//! wrapper opens directories as `php_plain_files_dirstream_ops`.
//!
//! What is not here: the lazy body under `php://stdin` and `php://input`
//! (#544), `php://fd/` and `php://filter/`, the filter chains, and the
//! read buffer's `DETECT_EOL` arm.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use php_heap::ResourceType;

use crate::api::raising::{Level, Thrown, raise, throw, warning};
use crate::api::request::{Cast, FdStream, FilterFlags, Request, Stream, Whence};
use crate::api::values::{ClassId, Key, PhpArray, PhpString, Resource, Value};

mod filters;
mod sockets;
pub(crate) mod userspace;

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

    /// `ops->close`, with what the kind holds: php's answer, `0` but for
    /// a process pipe, whose close is `pclose(3)`'s exit status, which
    /// `pclose()` hands back.
    fn close(&mut self, req: &mut Request) -> i32;

    /// `ops->flush`: `true` for php's `0`.
    fn flush(&mut self, _req: &mut Request) -> bool {
        true
    }

    /// `php_stream_encloses`: the stream this kind reads and writes
    /// through, which only `php://temp` has.
    fn enclose(&mut self, _inner: Stream) {}

    /// `ops->seek`, or `None` where the kind has none: `position` is
    /// php's `*newoffs`, written as the kind writes it, which the
    /// memory kind does on failure too; `Some(true)` for php's `0`. A
    /// `whence` of `None` is a number that is none of the three, which
    /// every kind here answers `-1` to.
    fn seek(
        &mut self,
        _req: &mut Request,
        _offset: i64,
        _whence: Option<Whence>,
        _position: &mut i64,
    ) -> Option<bool> {
        None
    }

    /// Whether the kind has an `ops->seek`, which `stream_get_meta_data`
    /// reads as half of `seekable`.
    fn seekable(&self) -> bool {
        false
    }

    /// `ops->stat`, `None` where the kind has none or `fstat(2)` failed.
    fn stat(&mut self, _req: &mut Request) -> Option<StatBuf> {
        None
    }

    /// `PHP_STREAM_OPTION_TRUNCATE_API`: `None` where the kind does not
    /// answer `PHP_STREAM_TRUNCATE_SUPPORTED`, else whether
    /// `PHP_STREAM_TRUNCATE_SET_SIZE` to `size` was taken.
    fn truncate(&mut self, _req: &mut Request, _size: usize) -> Option<bool> {
        None
    }

    /// `PHP_STREAM_OPTION_LOCKING` with `flock(2)`'s operation,
    /// `LOCK_SH`, `LOCK_EX` or `LOCK_UN` with `LOCK_NB` beside it:
    /// `None` where the kind has no `flock(2)`, else whether `flock(2)`
    /// took it.
    fn flock(&mut self, _req: &mut Request, _operation: i64) -> Option<bool> {
        None
    }

    /// What `PHP_STREAM_OPTION_META_DATA_API` answers for the kind.
    fn meta_data(&self) -> MetaData {
        MetaData::Generic
    }

    /// `php_plain_files_dirstream_read`: the next entry's name, `None`
    /// past the last and for a kind that is no directory.
    fn read_dir(&mut self, _req: &mut Request) -> Option<Vec<u8>> {
        None
    }

    /// `php_stream_memory_get_buffer`: the bytes of a memory stream,
    /// `None` for every other kind, which is `php_stream_is(stream,
    /// PHP_STREAM_IS_MEMORY)` answered no.
    fn memory_buffer(&self) -> Option<&[u8]> {
        None
    }

    /// `php_stream_is(stream, PHP_STREAM_IS_STDIO)`: the kind is
    /// `php_stream_stdio_ops`.
    fn is_stdio(&self) -> bool {
        false
    }

    /// `ops->cast` to `PHP_STREAM_AS_FD` or `PHP_STREAM_AS_FD_FOR_SELECT`,
    /// which every kind here answers alike: the host fd the kind
    /// reads and writes, `None` for php's `FAILURE`, a kind with no
    /// fd.
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }

    /// `PHP_STREAM_OPTION_BLOCKING`: whether the kind blocked before, or
    /// `None` for php's `-1`, a kind with no such option or an fd that
    /// refused it.
    fn set_blocking(&mut self, _req: &mut Request, _blocking: bool) -> Option<bool> {
        None
    }

    /// Whether the last `ops->seek` found the kind cannot seek after all,
    /// which sets `PHP_STREAM_FLAG_NO_SEEK`, as a user-space stream with
    /// no `stream_seek` does.
    fn lost_seek(&self) -> bool {
        false
    }

    /// `stream->wrapperdata`, which `stream_get_meta_data` prints as
    /// `wrapper_data`: a user-space stream's instance.
    fn wrapper_data(&self) -> Option<Value> {
        None
    }

    /// `php_userstreamop_cast`'s `stream_cast`: the stream a user-space
    /// stream says it is to be cast as, `None` for every other kind.
    fn user_cast(&mut self, _req: &mut Request, _select: bool) -> Option<Stream> {
        None
    }

    /// `PHP_STREAM_OPTION_READ_TIMEOUT`: `false` for a kind with no such
    /// option.
    fn set_timeout(&mut self, _timeout: std::time::Duration) -> bool {
        false
    }

    /// `PHP_STREAM_OPTION_XPORT_API`: one operation of a `transport`,
    /// `None` for php's `PHP_STREAM_OPTION_RETURN_NOTIMPL`, a kind that
    /// is no socket.
    fn xport(&mut self, _req: &mut Request, _op: sockets::XportOp<'_>) -> Option<sockets::Xported> {
        None
    }
}

/// `php_stream_statbuf`'s thirteen fields in the order `php_fstat` puts
/// them: `dev`, `ino`, `mode`, `nlink`, `uid`, `gid`, `rdev`, `size`,
/// `atime`, `mtime`, `ctime`, `blksize` and `blocks`.
pub(crate) type StatBuf = [i64; 13];

/// What a kind's `PHP_STREAM_OPTION_META_DATA_API` answers, which is
/// what `stream_get_meta_data` puts first.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetaData {
    /// `PHP_STREAM_OPTION_RETURN_NOTIMPL`: the generic three,
    /// `timed_out`, `blocked` and `eof` as `php_stream_eof` answers it.
    Generic,
    /// `php_stdiop_set_option`'s three, with `eof` as the kind's last
    /// read left `stream->eof` and `blocked` from `O_NONBLOCK`.
    Stdio { blocked: bool },
    /// `php_sockop_set_option`'s three: whether the last read timed out,
    /// whether the socket blocks, and `stream->eof`.
    Socket { timed_out: bool, blocked: bool },
    /// `php_stream_temp_set_option`'s copy of `ts->meta`, which only
    /// `data:` fills: nothing at all for `php://temp`.
    Own,
}

/// php's `php_stream` (`Z/main/php_streams.h`), the half every kind
/// shares: the position, `eof`, the read buffer with its two
/// cursors, the wrapper it was opened through, the bits the generic
/// operations read, and the `stream->mode` and path `stream_get_meta_data`
/// prints.
pub(crate) struct PhpStream {
    ops: Box<dyn Ops>,
    /// `stream->wrapper->wops->label`.
    wrapper: &'static str,
    /// `stream->mode`, what `php_stream_alloc` was handed, cut to the
    /// fifteen bytes its `char mode[16]` holds.
    mode: Vec<u8>,
    /// `stream->orig_path`: the path `php_stream_open_wrapper_ex` opened,
    /// after `include_path`, and `None` for a stream no wrapper opened by
    /// path, a directory stream among them.
    orig_path: Option<Vec<u8>>,
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
    /// `stream->res`, which a filter is handed as its stream.
    res: Option<Resource>,
    /// `stream->readfilters`, the chain every read of the kind passes
    /// through, head first.
    readfilters: Vec<filters::Attached>,
    /// `stream->writefilters`, the chain every write passes through
    /// before the kind.
    writefilters: Vec<filters::Attached>,
}

/// `stream->flags`, the four of `PHP_STREAM_FLAG_*` the generic
/// operations read.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // php's bitset, one field per bit a generic operation reads
struct Flags {
    /// `PHP_STREAM_FLAG_NO_BUFFER`: the kind is read directly.
    no_buffer: bool,
    /// `PHP_STREAM_FLAG_NO_SEEK`.
    no_seek: bool,
    /// `PHP_STREAM_FLAG_WAS_WRITTEN`: a flush is owed at close.
    was_written: bool,
    /// `PHP_STREAM_FLAG_IS_DIR`, which `php_stream_opendir` sets, with
    /// the `PHP_STREAM_FLAG_NO_FCLOSE` `opendir` sets beside it.
    is_dir: bool,
}

impl PhpStream {
    /// `_php_stream_alloc` (`Z/main/streams/streams.c:301`) less the
    /// resource, which [`Request::stream_alloc`] registers: the kind,
    /// its `stream->mode`, the chunk size, and nothing read or written.
    fn new(ops: Box<dyn Ops>, mode: &[u8]) -> PhpStream {
        PhpStream {
            ops,
            wrapper: "",
            mode: mode[..mode.len().min(15)].to_vec(),
            orig_path: None,
            position: 0,
            eof: false,
            fatal_error: false,
            flags: Flags::default(),
            readbuf: Vec::new(),
            readpos: 0,
            writepos: 0,
            chunk_size: CHUNK_SIZE,
            res: None,
            readfilters: Vec::new(),
            writefilters: Vec::new(),
        }
    }

    /// `_php_stream_tell` (`Z/main/streams/streams.c:1380`).
    pub(crate) const fn position(&self) -> i64 {
        self.position
    }

    /// `php_stream_is(stream, PHP_STREAM_IS_STDIO)`.
    pub(crate) fn is_stdio(&self) -> bool {
        self.ops.is_stdio()
    }

    /// `stream->flags & PHP_STREAM_FLAG_IS_DIR`.
    pub(crate) const fn is_dir(&self) -> bool {
        self.flags.is_dir
    }

    /// `_php_stream_fill_read_buffer` (`Z/main/streams/streams.c:564`)
    /// without a filter chain: nothing when the buffer already holds
    /// `size` bytes; otherwise the buffer compacted and grown by one
    /// chunk where the chunk would not fit, and one read of the kind
    /// into the room after `writepos`. `false` is the read that failed.
    fn fill_read_buffer(&mut self, req: &mut Request, size: usize) -> bool {
        if !self.readfilters.is_empty() {
            return self.fill_filtered(req, size);
        }
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
            let direct = self.flags.no_buffer || self.chunk_size == 1;
            let toread = if self.readfilters.is_empty() && direct {
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
        if self.writefilters.is_empty() {
            return self.write_buffer(req, buf);
        }
        let bytes = self.write_filtered(req, Some(buf), FilterFlags::Normal);
        self.flags.was_written = true;
        bytes
    }

    /// `_php_stream_write_buffer` (`Z/main/streams/streams.c:1189`): the
    /// kind written past the filters, as [`PhpStream::write`] describes.
    fn write_buffer(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if !self.flags.no_seek && self.readpos != self.writepos {
            self.readpos = 0;
            self.writepos = 0;
            let Self { ops, position, .. } = self;
            let _ = ops.seek(req, *position, Some(Whence::Set), position);
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
    /// by reading and is php's warning for anything else. A `whence`
    /// that is none of the three reaches the kind as it is, and a kind
    /// answers it `-1`.
    pub(crate) fn seek(&mut self, req: &mut Request, offset: i64, whence: Option<Whence>) -> bool {
        if !self.flags.no_buffer {
            let held = i64::try_from(self.writepos - self.readpos).unwrap_or(i64::MAX);
            match whence {
                Some(Whence::Cur) if offset > 0 && offset <= held => {
                    self.readpos += usize::try_from(offset).unwrap_or(0);
                    self.position += offset;
                    self.eof = false;
                    self.fatal_error = false;
                    return true;
                }
                Some(Whence::Set) if offset > self.position && offset <= self.position + held => {
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
                Some(Whence::Cur) => (self.position.saturating_add(offset), Some(Whence::Set)),
                Some(Whence::Set) if offset < 0 => return false,
                other => (offset, other),
            };
            if !self.writefilters.is_empty() {
                self.flush(req);
            }
            let Self { ops, position, .. } = self;
            if let Some(sought) = ops.seek(req, offset, whence, position) {
                if self.ops.lost_seek() {
                    self.flags.no_seek = true;
                }
                if !self.flags.no_seek || sought {
                    if sought {
                        self.eof = false;
                        self.fatal_error = false;
                    }
                    self.readpos = 0;
                    self.writepos = 0;
                    return sought;
                }
            }
        }
        if whence == Some(Whence::Cur) && offset >= 0 {
            let mut left = usize::try_from(offset).unwrap_or(usize::MAX);
            let mut tmp = [0u8; 1024];
            while left > 0 {
                let take = left.min(tmp.len());
                match self.read(req, &mut tmp[..take]) {
                    Some(didread) if didread > 0 => left -= didread,
                    _ => return false,
                }
            }
            self.eof = false;
            self.fatal_error = false;
            return true;
        }
        let text = format!("{}(): Stream does not support seeking", req.active_native());
        warning(req, &text);
        false
    }

    /// `_php_stream_get_line` (`Z/main/streams/streams.c:958`) without
    /// `DETECT_EOL`: bytes up to and including the first `\n`, taken
    /// from the read buffer and the buffer filled a chunk at a time
    /// while it holds none, for every kind, the unbuffered ones too;
    /// `maxlen` is php's, so at most `maxlen - 1` bytes, and `None` is
    /// php's `grow_mode`, a line of any length. `None` back is a line of no
    /// bytes, at eof or after a read that failed.
    pub(crate) fn get_line(&mut self, req: &mut Request, maxlen: Option<usize>) -> Option<Vec<u8>> {
        if maxlen == Some(0) {
            return None;
        }
        let mut line = Vec::new();
        let mut left = maxlen.unwrap_or(0);
        loop {
            let avail = self.writepos - self.readpos;
            if avail > 0 {
                let held = &self.readbuf[self.readpos..self.writepos];
                let (mut cpysz, mut done) = match held.iter().position(|b| *b == b'\n') {
                    Some(eol) => (eol + 1, true),
                    None => (avail, false),
                };
                if maxlen.is_some() && cpysz >= left - 1 {
                    cpysz = left - 1;
                    done = true;
                }
                line.extend_from_slice(&held[..cpysz]);
                self.position += i64::try_from(cpysz).unwrap_or(i64::MAX);
                self.readpos += cpysz;
                left = left.saturating_sub(cpysz);
                if done {
                    break;
                }
            } else if self.eof {
                break;
            } else {
                let toread = match maxlen {
                    None => self.chunk_size,
                    Some(_) => (left - 1).min(self.chunk_size),
                };
                // php gives up on a failed fill only where a filter set
                // `fatal_error`, which no stream here has.
                let _ = self.fill_read_buffer(req, toread);
                if self.writepos == self.readpos {
                    break;
                }
            }
        }
        (!line.is_empty()).then_some(line)
    }

    /// `_php_stream_copy_to_mem` (`Z/main/streams/streams.c:1569`): up
    /// to `maxlen` bytes, `None` for all of them. A bound under four
    /// chunks reads into a string of that size until it is full or the
    /// stream is at eof; anything else reads into a buffer sized from
    /// `php_stream_stat`'s size past the position and a chunk over,
    /// grown a chunk at a time while a quarter chunk of room is left,
    /// until a read gives nothing.
    pub(crate) fn copy_to_mem(&mut self, req: &mut Request, maxlen: Option<usize>) -> Vec<u8> {
        if maxlen == Some(0) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(max) = maxlen
            && max < 4 * CHUNK_SIZE
        {
            out.resize(max, 0);
            let mut len = 0;
            while len < max && !self.eof() {
                match self.read(req, &mut out[len..]) {
                    Some(n) if n > 0 => len += n,
                    _ => break,
                }
            }
            out.truncate(len);
            return out;
        }
        let size = self.ops.stat(req).map_or(0, |sb| sb[7]);
        let mut buflen = if size > 0 {
            usize::try_from((size - self.position).max(0)).unwrap_or(0) + CHUNK_SIZE
        } else {
            CHUNK_SIZE
        };
        if let Some(max) = maxlen {
            buflen = buflen.min(max);
        }
        out.resize(buflen, 0);
        let min_room = CHUNK_SIZE / 4;
        let mut len = 0;
        while let Some(n) = self.read(req, &mut out[len..buflen]) {
            if n == 0 {
                break;
            }
            len += n;
            if len + min_room >= buflen {
                if maxlen == Some(len) {
                    break;
                }
                buflen = match maxlen {
                    Some(max) if buflen + CHUNK_SIZE > max => max,
                    _ => buflen + CHUNK_SIZE,
                };
                out.resize(buflen, 0);
            }
        }
        out.truncate(len);
        out
    }

    /// `_php_stream_stat` (`Z/main/streams/streams.c:888`): the kind's
    /// `ops->stat`, since no wrapper here has a `stream_stat`.
    pub(crate) fn stat(&mut self, req: &mut Request) -> Option<StatBuf> {
        self.ops.stat(req)
    }

    /// `php_stream_truncate_set_size` behind
    /// `php_stream_truncate_supported`: `None` where the kind does not
    /// truncate.
    pub(crate) fn truncate(&mut self, req: &mut Request, size: usize) -> Option<bool> {
        self.ops.truncate(req, size)
    }

    /// `php_stream_lock`: `false` where the kind has no `flock(2)` or
    /// `flock(2)` refused.
    pub(crate) fn flock(&mut self, req: &mut Request, operation: i64) -> bool {
        self.ops.flock(req, operation).unwrap_or(false)
    }

    /// `PHP_STREAM_OPTION_READ_BUFFER` as `_php_stream_set_option`
    /// answers it for a kind that leaves it to the generic half:
    /// `PHP_STREAM_BUFFER_NONE` sets `NO_BUFFER`, anything else clears
    /// it.
    pub(crate) const fn set_read_buffer(&mut self, buffered: bool) {
        self.flags.no_buffer = !buffered;
    }

    /// `_php_stream_readdir` (`Z/main/streams/streams.c:2221`): the next
    /// entry, which `php_stream_read` of one `php_stream_dirent` is.
    pub(crate) fn read_dir(&mut self, req: &mut Request) -> Option<Vec<u8>> {
        self.ops.read_dir(req)
    }

    /// `_php_stream_flush` (`Z/main/streams/streams.c:1316`): the write
    /// chain flushed through to the kind first, with `closing` its last
    /// call; the owed flush cleared and the kind flushed.
    pub(crate) fn flush_closing(&mut self, req: &mut Request, closing: bool) -> bool {
        if !self.writefilters.is_empty() {
            let flags = if closing {
                FilterFlags::FlushClose
            } else {
                FilterFlags::FlushInc
            };
            let _ = self.write_filtered(req, None, flags);
        }
        self.flags.was_written = false;
        let Self { ops, .. } = self;
        ops.flush(req)
    }

    /// `php_stream_flush`, not closing.
    pub(crate) fn flush(&mut self, req: &mut Request) -> bool {
        self.flush_closing(req, false)
    }

    /// `_php_stream_cast` (`Z/main/streams/cast.c:194`) to a fd:
    /// short of `PHP_STREAM_AS_FD_FOR_SELECT` the stream is flushed and a
    /// kind that seeks is sought to the position, the read buffer
    /// dropped; the kind is asked for its fd, and one with none
    /// is php's `Cannot represent a stream of type %s as a %s` under
    /// `report`; a cast without `PHP_STREAM_CAST_INTERNAL` that leaves
    /// bytes in the read buffer is php's warning that they are lost.
    pub(crate) fn cast(&mut self, req: &mut Request, cast: Cast, warn: bool) -> Option<RawFd> {
        if cast != Cast::Select {
            self.flush(req);
            if self.ops.seekable() && !self.flags.no_seek {
                let Self { ops, position, .. } = self;
                let mut dummy = 0;
                let _ = ops.seek(req, *position, Some(Whence::Set), &mut dummy);
                self.readpos = 0;
                self.writepos = 0;
            }
        }
        if !self.readfilters.is_empty() || !self.writefilters.is_empty() {
            if warn {
                let text = format!(
                    "{}(): Cannot cast a filtered stream on this system",
                    req.active_native()
                );
                warning(req, &text);
            }
            return None;
        }
        if let Some(inner) = self.ops.user_cast(req, cast == Cast::Select) {
            return req
                .stream_op(inner, |req, s| s.cast(req, cast, true))
                .flatten();
        }
        let Some(fd) = self.ops.fd().map(|fd| fd.as_raw_fd()) else {
            if warn {
                let target = if cast == Cast::Select {
                    "select()able descriptor"
                } else {
                    "File Descriptor"
                };
                let text = format!(
                    "{}(): Cannot represent a stream of type {} as a {target}",
                    req.active_native(),
                    self.ops.label()
                );
                warning(req, &text);
            }
            return None;
        };
        let held = self.writepos - self.readpos;
        if held > 0 && cast == Cast::Fd {
            let text = format!(
                "{}(): {held} bytes of buffered data lost during stream conversion!",
                req.active_native()
            );
            warning(req, &text);
        }
        Some(fd)
    }

    /// `_php_stream_free` under `PHP_STREAM_FREE_CLOSE`
    /// (`Z/main/streams/streams.c:388`), what the resource's destructor
    /// runs, so `fclose`, the last owner's release and the shutdown walk
    /// are one path: the owed flush, then the kind closed, then the
    /// stream dropped.
    fn free(mut self, req: &mut Request) -> i32 {
        if self.flags.was_written || !self.writefilters.is_empty() {
            self.flush_closing(req, true);
        }
        let Self { ops, .. } = &mut self;
        let ret = ops.close(req);
        self.free_filters(req);
        ret
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

    /// `_php_stream_mode_to_str` (`Z/main/streams/memory.c:261`): the
    /// `stream->mode` a memory or temp stream is allocated with.
    const fn as_mode(self) -> &'static [u8] {
        match self {
            TempMode::ReadOnly => b"rb",
            TempMode::Append => b"a+b",
            TempMode::Default => b"w+b",
        }
    }
}

/// `S_IFREG`, the regular file's bits in `st_mode`.
const S_IFREG: i64 = 0o100_000;

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
    fn close(&mut self, _req: &mut Request) -> i32 {
        self.data = Vec::new();
        0
    }

    /// `php_stream_memory_seek` (`Z/main/streams/memory.c:120`): a
    /// position before the start is refused with the position left at
    /// zero and `*newoffs` at `-1`; `SEEK_END` past the end and
    /// `SEEK_SET` anywhere at or past zero are taken as they are; any
    /// other `whence` is `-1` with `*newoffs` at the position it stands
    /// at.
    fn seek(
        &mut self,
        _req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let len = i64::try_from(self.data.len()).unwrap_or(i64::MAX);
        let fpos = i64::try_from(self.fpos).unwrap_or(i64::MAX);
        let target = match whence {
            Some(Whence::Cur) => fpos.saturating_add(offset),
            Some(Whence::Set) => offset,
            Some(Whence::End) => len.saturating_add(offset),
            None => {
                *position = fpos;
                return Some(false);
            }
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

    fn seekable(&self) -> bool {
        true
    }

    /// `php_stream_memory_stat` (`Z/main/streams/memory.c:163`): a
    /// regular file of the bytes' length, `0444` read only and `0666`
    /// otherwise, one link, device `0xC`, inode, owner and times zero,
    /// and `-1` for `rdev`, `blksize` and `blocks`.
    fn stat(&mut self, _req: &mut Request) -> Option<StatBuf> {
        let perms = if self.mode == TempMode::ReadOnly {
            0o444
        } else {
            0o666
        };
        let size = i64::try_from(self.data.len()).unwrap_or(i64::MAX);
        Some([0xC, 0, perms | S_IFREG, 1, 0, 0, -1, size, 0, 0, 0, -1, -1])
    }

    /// `php_stream_memory_set_option`'s `PHP_STREAM_OPTION_TRUNCATE_API`
    /// (`Z/main/streams/memory.c:193`): refused on a read-only stream;
    /// a shorter size cuts the bytes and pulls the position back to it,
    /// a longer one zero-fills.
    fn truncate(&mut self, _req: &mut Request, size: usize) -> Option<bool> {
        if self.mode == TempMode::ReadOnly {
            return Some(false);
        }
        self.data.resize(size, 0);
        self.fpos = self.fpos.min(size);
        Some(true)
    }

    fn meta_data(&self) -> MetaData {
        if self.label == "STDIO" {
            MetaData::Stdio { blocked: true }
        } else {
            MetaData::Generic
        }
    }

    fn memory_buffer(&self) -> Option<&[u8]> {
        (self.label == "MEMORY").then_some(self.data.as_slice())
    }

    fn is_stdio(&self) -> bool {
        self.label == "STDIO"
    }
}

/// `_php_stream_memory_create` (`Z/main/streams/memory.c:271`): empty,
/// at zero, unbuffered.
fn memory(mode: TempMode) -> PhpStream {
    let mut stream = PhpStream::new(
        Box::new(Memory {
            data: Vec::new(),
            fpos: 0,
            mode,
            label: "MEMORY",
        }),
        mode.as_mode(),
    );
    stream.flags.no_buffer = true;
    stream
}

/// `PHP_STREAM_MAX_MEM` (`Z/main/php_memory_streams.h:27`): what a
/// `php://temp` holds in memory before it spills to a file.
const PHP_STREAM_MAX_MEM: usize = 2 * 1024 * 1024;

/// `php_stream_temp_data` under `php_stream_temp_ops`
/// (`Z/main/streams/memory.c:337`): the stream it encloses, its own
/// resource as php's is, so a `php://temp` takes two ids as the oracle's
/// does, `None` where php's `innerstream` is `NULL`; and `smax`, the
/// bytes it keeps in memory before the spill.
struct Temp {
    inner: Option<Stream>,
    smax: usize,
}

impl Temp {
    /// The spill of `php_stream_temp_write` (`Z/main/streams/memory.c:350`):
    /// while the inner stream is the memory one and a write of `count`
    /// at its position would reach `smax`, a temporary file opened by
    /// `php_stream_fopen_temporary_file` takes the bytes, the memory
    /// stream is freed, and the file, a third resource, is sought to
    /// where the memory stream stood. `false` is the file that could not
    /// be made, after php's warning.
    fn spill(&mut self, req: &mut Request, inner: Stream, count: usize) -> Result<bool, Thrown> {
        let Some((pos, membuf)) = req
            .stream_op(inner, |_, s| {
                s.ops
                    .memory_buffer()
                    .map(|data| (s.position, data.to_vec()))
            })
            .flatten()
        else {
            return Ok(true);
        };
        if usize::try_from(pos).unwrap_or(0) + count < self.smax {
            return Ok(true);
        }
        let Some(file) = temporary_file(req, None, b"php")? else {
            let text = format!(
                "{}(): Unable to create temporary file, Check permissions in temporary files directory.",
                req.active_native()
            );
            warning(req, &text);
            return Ok(false);
        };
        req.stream_op(file, |req, f| f.write(req, &membuf));
        inner.resource().close(req);
        req.release(inner.resource().as_value());
        self.inner = Some(file);
        req.stream_op(file, |req, f| f.seek(req, pos, Some(Whence::Set)));
        Ok(true)
    }
}

impl Ops for Temp {
    fn label(&self) -> &'static str {
        "TEMP"
    }

    fn enclose(&mut self, inner: Stream) {
        self.inner = Some(inner);
    }

    /// `php_stream_temp_write` (`Z/main/streams/memory.c:350`): the spill
    /// where the write would reach `smax`, then the inner stream
    /// written; a spill that could not make its file writes nothing.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let inner = self.inner?;
        match self.spill(req, inner, buf.len()) {
            Ok(true) => {}
            Ok(false) => return Some(0),
            Err(_) => return None,
        }
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
    /// the inner resource closed and its one owner given up, which
    /// unlinks a spilled file.
    fn close(&mut self, req: &mut Request) -> i32 {
        if let Some(inner) = self.inner.take() {
            inner.resource().close(req);
            req.release(inner.resource().as_value());
        }
        0
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
        whence: Option<Whence>,
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

    fn seekable(&self) -> bool {
        true
    }

    /// `php_stream_temp_stat` (`Z/main/streams/memory.c:510`): the inner
    /// stream's.
    fn stat(&mut self, req: &mut Request) -> Option<StatBuf> {
        req.stream_op(self.inner?, |req, inner| inner.stat(req))
            .flatten()
    }

    /// `php_stream_temp_set_option`'s default arm: the inner stream's.
    fn truncate(&mut self, req: &mut Request, size: usize) -> Option<bool> {
        req.stream_op(self.inner?, |req, inner| inner.truncate(req, size))
            .flatten()
    }

    /// `php_stream_temp_set_option`'s default arm: the inner stream's.
    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        req.stream_op(self.inner?, |req, inner| inner.ops.flock(req, operation))
            .flatten()
    }

    fn meta_data(&self) -> MetaData {
        MetaData::Own
    }
}

/// `php_stream_fopen_temporary_file` (`Z/main/streams/plain_wrapper.c:237`)
/// over `php_open_temporary_fd_ex`: the file [`Request::open_temporary_fd`]
/// makes in `dir`, or in the temporary directory, opened `r+b` as a plain
/// file of its own resource, with the path as `orig_path` and the file
/// unlinked at close. `None` is the file that could not be made.
pub(crate) fn temporary_file(
    req: &mut Request,
    dir: Option<&[u8]>,
    prefix: &[u8],
) -> Result<Option<Stream>, Thrown> {
    let Some((file, path)) = req.open_temporary_fd(dir, prefix) else {
        return Ok(None);
    };
    let orig_path = path.as_os_str().as_encoded_bytes().to_vec();
    let mut stream = PhpStream::new(
        Box::new(Stdio {
            file: Some(file),
            is_seekable: true,
            temp_name: Some(path),
            process: None,
            blocking: true,
        }),
        b"r+b",
    );
    stream.wrapper = PLAIN_FILES;
    stream.orig_path = Some(orig_path);
    Ok(Some(req.stream_alloc(stream)?))
}

// -------------------------------------------------------- plain files

/// `php_plain_files_wrapper.wops->label`.
const PLAIN_FILES: &str = "plainfile";

/// `php_stream_php_wrapper.wops->label`.
const PHP_WRAPPER: &str = "PHP";

/// `php_stdio_stream_data` under `php_stream_stdio_ops`
/// (`Z/main/streams/plain_wrapper.c:135`): the file, `None` once closed;
/// whether it seeks; `temp_name`, the path of a temporary file to unlink
/// at close; and for a pipe `popen` opened, `is_process_pipe`, the child
/// at its other end, whom the close waits for as `pclose(3)` does.
struct Stdio {
    file: Option<std::fs::File>,
    is_seekable: bool,
    temp_name: Option<std::path::PathBuf>,
    process: Option<std::process::Child>,
    /// Whether the fd blocks, what `stream_set_blocking` last set.
    blocking: bool,
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
pub(crate) fn strerror(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.split_once(" (os error")
        .map_or(text.clone(), |(text, _)| text.to_owned())
}

/// A `zend_stat_t` as `php_fstat` lays it out, from what `fstat(2)`,
/// `stat(2)` or `lstat(2)` answered.
pub(crate) fn stat_buf(meta: &std::fs::Metadata) -> StatBuf {
    use std::os::unix::fs::MetadataExt;
    let long = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    [
        long(meta.dev()),
        long(meta.ino()),
        i64::from(meta.mode()),
        long(meta.nlink()),
        i64::from(meta.uid()),
        i64::from(meta.gid()),
        long(meta.rdev()),
        long(meta.size()),
        meta.atime(),
        meta.mtime(),
        meta.ctime(),
        long(meta.blksize()),
        long(meta.blocks()),
    ]
}

/// `EBADF`, the one `errno` `php_stdiop_read` leaves eof clear on.
const EBADF: i32 = 9;

impl Ops for Stdio {
    fn label(&self) -> &'static str {
        "STDIO"
    }

    /// `php_stdiop_write` (`Z/main/streams/plain_wrapper.c:374`): one
    /// `write(2)`, and php's notice with `errno` where it failed; the
    /// stat cache cleared after it, since the file's times moved.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let written = self.file.as_mut()?.write(buf);
        req.forget_stats();
        match written {
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(0),
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
    /// it failed, eof set then too unless the fd was bad, `EBADF`; the
    /// stat cache cleared after it, since the file's atime moved.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let read = self.file.as_mut()?.read(buf);
        req.forget_stats();
        match read {
            Ok(0) => {
                *eof = true;
                Some(0)
            }
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(0),
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

    /// `php_stdiop_flush` (`Z/main/streams/plain_wrapper.c:573`): nothing
    /// is buffered above `write(2)` here, so only the stat cache php
    /// clears before its `fflush(3)`.
    fn flush(&mut self, req: &mut Request) -> bool {
        req.forget_stats();
        true
    }

    /// `php_stdiop_close` (`Z/main/streams/plain_wrapper.c:508`): the
    /// file closed, which its drop does, and a temporary file unlinked;
    /// a process pipe is `pclose(3)`, the pipe closed and the child
    /// waited for, and its answer `WEXITSTATUS` where the child exited
    /// and the wait status as it is where a signal ended it.
    fn close(&mut self, _req: &mut Request) -> i32 {
        drop(self.file.take());
        if let Some(mut child) = self.process.take() {
            use std::os::unix::process::ExitStatusExt;
            return match child.wait() {
                Ok(status) => status.code().unwrap_or_else(|| status.into_raw()),
                Err(_) => -1,
            };
        }
        if let Some(path) = self.temp_name.take() {
            let _ = std::fs::remove_file(path);
        }
        0
    }

    /// `php_stdiop_seek` (`Z/main/streams/plain_wrapper.c:616`): php's
    /// warning on a stream that cannot seek, else `lseek(2)` and the
    /// offset it answered, and `EINVAL`'s `-1` for a `whence` it does
    /// not know.
    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        if !self.is_seekable {
            let text = format!("{}(): Cannot seek on this stream", req.active_native());
            warning(req, &text);
            return Some(false);
        }
        let from = match whence {
            Some(Whence::Set) => SeekFrom::Start(u64::try_from(offset).unwrap_or(0)),
            Some(Whence::Cur) => SeekFrom::Current(offset),
            Some(Whence::End) => SeekFrom::End(offset),
            None => return Some(false),
        };
        let Some(file) = self.file.as_mut() else {
            return Some(false);
        };
        match file.seek(from) {
            Ok(at) => {
                *position = i64::try_from(at).unwrap_or(i64::MAX);
                Some(true)
            }
            Err(_) => Some(false),
        }
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `php_stdiop_stat` (`Z/main/streams/plain_wrapper.c:703`):
    /// `fstat(2)`.
    fn stat(&mut self, _req: &mut Request) -> Option<StatBuf> {
        Some(stat_buf(&self.file.as_ref()?.metadata().ok()?))
    }

    /// `php_stdiop_set_option`'s `PHP_STREAM_OPTION_TRUNCATE_API`
    /// (`Z/main/streams/plain_wrapper.c:983`): `ftruncate(2)`, which
    /// leaves the offset where it was.
    fn truncate(&mut self, _req: &mut Request, size: usize) -> Option<bool> {
        Some(
            self.file
                .as_ref()?
                .set_len(u64::try_from(size).unwrap_or(u64::MAX))
                .is_ok(),
        )
    }

    /// `php_stdiop_set_option`'s `PHP_STREAM_OPTION_LOCKING`
    /// (`Z/main/streams/plain_wrapper.c:774`): `flock(2)`, `LOCK_SH`,
    /// `LOCK_EX` or `LOCK_UN`, without blocking under `LOCK_NB`.
    fn flock(&mut self, _req: &mut Request, operation: i64) -> Option<bool> {
        let nonblocking = operation & LOCK_NB != 0;
        let file = self.file.as_ref()?;
        let taken = match operation & !LOCK_NB {
            LOCK_SH if nonblocking => file.try_lock_shared().is_ok(),
            LOCK_SH => file.lock_shared().is_ok(),
            LOCK_EX if nonblocking => file.try_lock().is_ok(),
            LOCK_EX => file.lock().is_ok(),
            LOCK_UN => file.unlock().is_ok(),
            _ => false,
        };
        Some(taken)
    }

    fn meta_data(&self) -> MetaData {
        MetaData::Stdio {
            blocked: self.blocking,
        }
    }

    fn is_stdio(&self) -> bool {
        true
    }

    /// `php_stdiop_set_option`'s `PHP_STREAM_OPTION_BLOCKING`
    /// (`Z/main/streams/plain_wrapper.c:803`): `O_NONBLOCK` set or
    /// cleared on the fd, and whether it blocked before.
    fn set_blocking(&mut self, _req: &mut Request, blocking: bool) -> Option<bool> {
        let file = self.file.as_ref()?;
        let was = php_heap::set_fd_blocking(file.as_fd(), blocking).ok()?;
        self.blocking = blocking;
        Some(was)
    }

    /// `php_stdiop_cast` (`Z/main/streams/plain_wrapper.c:645`): the
    /// file's fd, for either cast.
    fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.file.as_ref().map(AsFd::as_fd)
    }
}

/// `flock(2)`'s `LOCK_SH`, `LOCK_EX`, `LOCK_NB` and `LOCK_UN`, which is
/// what `php_stream_lock` takes; php's own `PHP_LOCK_UN` is `3`, which
/// `php_flock_common` maps (`Z/ext/standard/flock_compat.h:46`).
const LOCK_SH: i64 = 1;
const LOCK_EX: i64 = 2;
const LOCK_NB: i64 = 4;
const LOCK_UN: i64 = 8;

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
/// expanded against the working directory, `open(2)` with `0666` and the
/// request's `umask` applied to a file it made, then
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
    let host = req.absolutize(path).0;
    let creates = matches!(mode.first(), Some(b'w' | b'a' | b'x' | b'c'))
        && std::fs::symlink_metadata(&host).is_err();
    let mut file = options.open(&host).map_err(|error| strerror(&error))?;
    if creates {
        req.apply_umask(&host, 0o666);
    }
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
    let mut stream = PhpStream::new(
        Box::new(Stdio {
            file: Some(file),
            is_seekable,
            temp_name: None,
            process: None,
            blocking: true,
        }),
        mode,
    );
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

    fn close(&mut self, _req: &mut Request) -> i32 {
        0
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `ftruncate(2)` on a pipe or a terminal, which is what the host's
    /// fds are to this engine: supported, and refused.
    fn truncate(&mut self, _req: &mut Request, _size: usize) -> Option<bool> {
        Some(false)
    }

    fn meta_data(&self) -> MetaData {
        MetaData::Stdio { blocked: true }
    }

    fn is_stdio(&self) -> bool {
        true
    }
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

    fn close(&mut self, _req: &mut Request) -> i32 {
        0
    }
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
        let mut smax = PHP_STREAM_MAX_MEM;
        if let Some(digits) = rest.strip_prefix(b"/maxmemory:") {
            let max_memory = strtol(digits);
            if max_memory < 0 {
                return Err(crate::api::raising::arg_value_error(
                    req,
                    2,
                    "be greater than or equal to 0",
                ));
            }
            smax = usize::try_from(max_memory).unwrap_or(usize::MAX);
        }
        return Ok(Ok(temp(req, TempMode::of(mode), smax)?));
    }
    let stream = match lower.as_slice() {
        b"memory" => memory(TempMode::of(mode)),
        b"output" => PhpStream::new(Box::new(Output), b"wb"),
        b"stdin" | b"input" => {
            let (label, allocated): (_, &[u8]) = if lower == b"stdin" {
                ("STDIO", mode)
            } else {
                ("Input", b"rb")
            };
            let mut stream = PhpStream::new(
                Box::new(Memory {
                    data: req.body_bytes().to_vec(),
                    fpos: 0,
                    mode: TempMode::ReadOnly,
                    label,
                }),
                allocated,
            );
            // `php_stream_input_ops` seeks the body it reads
            // (`php_stream_input_seek`); fd 0 under the CLI is a pipe or
            // a terminal, which `detect_is_seekable` says does not.
            if label == "STDIO" {
                stream.flags.no_seek = true;
                stream.position = -1;
            }
            stream
        }
        b"stdout" | b"stderr" => {
            let mut stream = PhpStream::new(
                Box::new(HostFd {
                    stderr: lower == b"stderr",
                }),
                mode,
            );
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

/// `ZEND_STRTOL(path, NULL, 10)`: leading white space, a sign and the
/// decimal digits after it, saturating where they overflow, and zero
/// where there are none.
fn strtol(bytes: &[u8]) -> i64 {
    let bytes = bytes.trim_ascii_start();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut value: i64 = 0;
    for digit in digits.iter().take_while(|b| b.is_ascii_digit()) {
        let d = i64::from(digit - b'0');
        value = if negative {
            value.saturating_mul(10).saturating_sub(d)
        } else {
            value.saturating_mul(10).saturating_add(d)
        };
    }
    value
}

/// `_php_stream_temp_create_ex` (`Z/main/streams/memory.c:519`): the
/// temp stream registered first, then the memory stream it encloses, so
/// the two ids come out in php's order.
fn temp(req: &mut Request, mode: TempMode, smax: usize) -> Result<Stream, Thrown> {
    let mut outer = PhpStream::new(Box::new(Temp { inner: None, smax }), mode.as_mode());
    outer.flags.no_buffer = true;
    let outer = req.stream_alloc(outer)?;
    let inner = req.stream_alloc(memory(mode))?;
    req.stream_op(outer, |_, stream| stream.ops.enclose(inner));
    Ok(outer)
}

// ---------------------------------------------------------- wrappers

/// Which wrapper `php_stream_locate_url_wrapper`
/// (`Z/main/streams/streams.c:2010`) found for a path: the plain files
/// wrapper, the `php://` wrapper, one an extension registered (its place
/// in [`Request::wrappers`]), one a program registered with
/// `stream_wrapper_register`, or none.
#[derive(Clone)]
pub(crate) enum Located {
    Plain,
    Php,
    Ext(usize),
    Space(userspace::UserWrapper),
    Nothing,
}

/// One row of a request's wrapper table, `url_stream_wrappers_hash` or
/// `FG(stream_wrappers)`: a wrapper under its scheme.
#[derive(Clone)]
pub(crate) struct Registered {
    pub(crate) scheme: Vec<u8>,
    pub(crate) wrapper: Located,
}

/// Whether two rows name the same wrapper, as `zend_hash_find_ptr(...) ==
/// wrapper` compares them.
fn same_wrapper(a: &Located, b: &Located) -> bool {
    match (a, b) {
        (Located::Plain, Located::Plain) | (Located::Php, Located::Php) => true,
        (Located::Ext(x), Located::Ext(y)) => x == y,
        _ => false,
    }
}

impl Request {
    /// `url_stream_wrappers_hash`, the wrappers every request starts
    /// with: `php`, `file`, then those the extensions registered.
    pub(crate) fn global_wrappers(&self) -> Vec<Registered> {
        let mut table = vec![
            Registered {
                scheme: b"php".to_vec(),
                wrapper: Located::Php,
            },
            Registered {
                scheme: b"file".to_vec(),
                wrapper: Located::Plain,
            },
        ];
        for (at, w) in self.wrappers.iter().enumerate() {
            table.push(Registered {
                scheme: w.scheme.as_bytes().to_vec(),
                wrapper: Located::Ext(at),
            });
        }
        table
    }

    /// `php_stream_get_url_stream_wrappers_hash`: the request's own table
    /// once a program changed it, the global one before.
    pub(crate) fn wrapper_table(&self) -> Vec<Registered> {
        self.url_wrappers
            .clone()
            .unwrap_or_else(|| self.global_wrappers())
    }

    /// The row a scheme names in a table, `zend_hash_str_find_ptr`.
    fn wrapper_row(table: &[Registered], scheme: &[u8]) -> Option<Located> {
        table
            .iter()
            .find(|r| r.scheme == scheme)
            .map(|r| r.wrapper.clone())
    }

    /// `php_stream_locate_url_wrapper` (`Z/main/streams/streams.c:2010`):
    /// a scheme is alphanumerics with `+`, `-` and `.`, two bytes or
    /// more, followed by `://`, looked up as written and then lowered;
    /// one nobody registered is php's warning and then the path as
    /// written for the plain files wrapper. `file://` (or any scheme
    /// `strncasecmp` takes for it) with a host that is not `localhost`
    /// is php's warning and no wrapper, and otherwise the path after the
    /// slashes; once a program changed the table, a plain path goes to
    /// whatever it registered as `file` and to none where it unregistered
    /// that, php's `file:// wrapper is disabled`. A wrapper registered
    /// as a URL wrapper under `allow_url_fopen=0` is refused.
    pub(crate) fn locate_wrapper(&mut self, path: &[u8]) -> (Located, Vec<u8>) {
        let n = path
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            .count();
        let mut protocol =
            (n > 1 && path.get(n) == Some(&b':') && path[n + 1..].starts_with(b"//"))
                .then(|| path[..n].to_vec());
        let table = self.wrapper_table();
        let mut wrapper = None;
        if let Some(scheme) = &protocol {
            wrapper = Request::wrapper_row(&table, scheme)
                .or_else(|| Request::wrapper_row(&table, &scheme.to_ascii_lowercase()));
            if wrapper.is_none() {
                let text = format!(
                    "{}(): Unable to find the wrapper \"{}\" - did you forget to enable it when you configured PHP?",
                    self.active_native(),
                    String::from_utf8_lossy(&scheme[..scheme.len().min(31)])
                );
                warning(self, &text);
                protocol = None;
            }
        }
        let is_file = protocol
            .as_ref()
            .is_none_or(|p| p.len() <= 4 && p.eq_ignore_ascii_case(&b"file"[..p.len()]));
        if is_file {
            let mut local = path.to_vec();
            if protocol.is_some() {
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
                local = path[at..].to_vec();
            }
            if self.url_wrappers.is_some() {
                if let Some(found) = wrapper.or_else(|| Request::wrapper_row(&table, b"file")) {
                    return (found, local);
                }
                let text = format!(
                    "{}(): file:// wrapper is disabled in the server configuration",
                    self.active_native()
                );
                warning(self, &text);
                return (Located::Nothing, local);
            }
            return (Located::Plain, local);
        }
        let Some(found) = wrapper else {
            return (Located::Plain, path.to_vec());
        };
        if let Located::Space(user) = &found
            && user.is_url
            && !self
                .ini_get(b"allow_url_fopen")
                .is_none_or(Request::ini_parse_bool)
        {
            let text = format!(
                "{}(): {}:// wrapper is disabled in the server configuration by allow_url_fopen=0",
                self.active_native(),
                String::from_utf8_lossy(&path[..n])
            );
            warning(self, &text);
            return (Located::Nothing, path.to_vec());
        }
        (found, path.to_vec())
    }

    /// `php_register_url_stream_wrapper_volatile` and its three callers'
    /// checks: a scheme of alphanumerics, `+`, `-` and `.`; the table
    /// the request's own from the first change on.
    fn own_wrappers(&mut self) -> &mut Vec<Registered> {
        if self.url_wrappers.is_none() {
            self.url_wrappers = Some(self.global_wrappers());
        }
        self.url_wrappers.get_or_insert_with(Vec::new)
    }

    /// `stream_wrapper_register()` (`Z/main/streams/userspace.c:437`) past
    /// its parameters: `class` registered under `protocol` for the rest of
    /// the request, a URL wrapper where `flags` holds `STREAM_IS_URL`;
    /// php's `Protocol %s:// is already defined.` where the scheme is
    /// taken and `Invalid protocol scheme specified` where it is not one.
    pub fn wrapper_register(&mut self, protocol: &[u8], class: ClassId, flags: i64) -> bool {
        let valid = protocol
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
        let taken = self.wrapper_table().iter().any(|r| r.scheme == protocol);
        if valid && !taken {
            let wrapper = Located::Space(userspace::UserWrapper::new(
                class,
                flags & 1 != 0,
                protocol.to_vec(),
            ));
            self.own_wrappers().push(Registered {
                scheme: protocol.to_vec(),
                wrapper,
            });
            return true;
        }
        let shown = String::from_utf8_lossy(protocol);
        let text = if taken {
            format!("Protocol {shown}:// is already defined.")
        } else {
            let name = self.class_name(class);
            format!(
                "Invalid protocol scheme specified. Unable to register wrapper class {name} to {shown}://"
            )
        };
        crate::api::raising::docref(self, Level::Warning, &text);
        false
    }

    /// `stream_wrapper_unregister()` (`Z/main/streams/userspace.c:473`):
    /// the scheme taken out of the request's table, a built-in one
    /// among them, and php's `Unable to unregister protocol %s://` where
    /// it is not there.
    pub fn wrapper_unregister(&mut self, protocol: &[u8]) -> bool {
        let table = self.own_wrappers();
        let Some(at) = table.iter().position(|r| r.scheme == protocol) else {
            let text = format!(
                "Unable to unregister protocol {}://",
                String::from_utf8_lossy(protocol)
            );
            crate::api::raising::docref(self, Level::Warning, &text);
            return false;
        };
        table.remove(at);
        true
    }

    /// `stream_wrapper_restore()` (`Z/main/streams/userspace.c:499`): the
    /// built-in wrapper put back under its scheme; php's `%s:// never
    /// existed, nothing to restore` for a scheme no built-in has, and its
    /// notice `%s:// was never changed, nothing to restore`, `true`, where
    /// the table has the built-in one already.
    pub fn wrapper_restore(&mut self, protocol: &[u8]) -> bool {
        let shown = String::from_utf8_lossy(protocol).into_owned();
        let Some(original) = Request::wrapper_row(&self.global_wrappers(), protocol) else {
            let text = format!("{shown}:// never existed, nothing to restore");
            crate::api::raising::docref(self, Level::Warning, &text);
            return false;
        };
        let unchanged = match &self.url_wrappers {
            None => true,
            Some(table) => Request::wrapper_row(table, protocol)
                .is_some_and(|current| same_wrapper(&current, &original)),
        };
        if unchanged {
            let text = format!("{shown}:// was never changed, nothing to restore");
            crate::api::raising::docref(self, Level::Notice, &text);
            return true;
        }
        let table = self.own_wrappers();
        table.retain(|r| r.scheme != protocol);
        table.push(Registered {
            scheme: protocol.to_vec(),
            wrapper: original,
        });
        true
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
                    Ok(stream) => {
                        // `_php_stream_fopen`'s `php_clear_stat_cache`
                        // (`Z/main/streams/plain_wrapper.c:1209`).
                        self.forget_stats();
                        Ok(self.stream_alloc(stream)?)
                    }
                    Err(message) => Err(message),
                },
            ),
            Located::Php => (PHP_WRAPPER, php_open(self, &path_to_open, mode)?),
            Located::Ext(at) => {
                let open = self.wrappers[at].open;
                (
                    "user-space",
                    open(self, &path_to_open, mode)?
                        .ok_or_else(|| String::from("operation failed")),
                )
            }
            Located::Space(user) => (
                "user-space",
                userspace::open(
                    self,
                    &user,
                    &path_to_open,
                    mode,
                    i64::from(use_include_path),
                )?,
            ),
            Located::Nothing => ("", Err(String::from("no suitable wrapper could be found"))),
        };
        match opened {
            Ok(stream) => {
                let append = mode.contains(&b'a');
                self.stream_op(stream, |req, s| {
                    s.wrapper = label;
                    s.orig_path = Some(to_open);
                    if append && !s.flags.no_seek && s.position == 0 {
                        let PhpStream { ops, position, .. } = s;
                        let mut newpos = 0;
                        if ops.seek(req, 0, Some(Whence::Cur), &mut newpos) == Some(true) {
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

    /// `_php_stream_opendir` (`Z/main/streams/streams.c:2184`) under
    /// `REPORT_ERRORS`: nothing for an empty path; the wrapper located
    /// as for a stream and asked for its directory, which only the plain
    /// files wrapper and a registered one with a directory opener have,
    /// every other being php's `not implemented`; a stream opened is a
    /// directory stream, unbuffered, opened `r`, that `fclose` refuses;
    /// and one not opened is `php_stream_display_wrapper_errors`'s
    /// warning, `Failed to open directory: ` and the message, at the path
    /// as written.
    pub(crate) fn open_dir_wrapper(&mut self, path: &[u8]) -> Result<Option<Stream>, Thrown> {
        if path.is_empty() {
            return Ok(None);
        }
        let (wrapper, path_to_open) = self.locate_wrapper(path);
        let (label, opened) = match wrapper {
            Located::Plain => (
                PLAIN_FILES,
                match std::fs::read_dir(self.absolutize(&path_to_open).0) {
                    Ok(entries) => {
                        let dir = PhpStream::new(
                            Box::new(Dir {
                                path: self.absolutize(&path_to_open).0,
                                entries: Some(entries),
                                dots: 0,
                            }),
                            b"r",
                        );
                        Ok(self.stream_alloc(dir)?)
                    }
                    Err(error) => Err(strerror(&error)),
                },
            ),
            Located::Php => (PHP_WRAPPER, Err(String::from("not implemented"))),
            Located::Ext(at) => match self.wrappers[at].open_dir {
                Some(open_dir) => (
                    "user-space",
                    open_dir(self, &path_to_open)?.ok_or_else(|| String::from("operation failed")),
                ),
                None => ("user-space", Err(String::from("not implemented"))),
            },
            Located::Space(user) => (
                "user-space",
                userspace::open_dir(self, &user, &path_to_open)?,
            ),
            Located::Nothing => ("", Err(String::from("no suitable wrapper could be found"))),
        };
        match opened {
            Ok(stream) => {
                self.stream_op(stream, |_, s| {
                    s.wrapper = label;
                    s.flags.no_buffer = true;
                    s.flags.is_dir = true;
                });
                Ok(Some(stream))
            }
            Err(message) => {
                self.display_wrapper_errors(path, "Failed to open directory", &message);
                Ok(None)
            }
        }
    }

    /// `stream_get_meta_data` (`Z/ext/standard/streamsfuncs.c:514`) over
    /// the stream: the kind's own answer first, which for most kinds is
    /// `timed_out`, `blocked` and `eof`; then `wrapper_type` where a
    /// wrapper opened it, `stream_type`, `mode`, `unread_bytes`,
    /// `seekable` and, where it was opened by path, `uri`.
    pub(crate) fn stream_meta_data(&mut self, stream: Stream) -> Result<PhpArray, Thrown> {
        let mut out = PhpArray::new(self)?;
        let Some(s) = self.stream_ref(stream) else {
            return Ok(out);
        };
        let head = match s.ops.meta_data() {
            MetaData::Generic => Some((false, true, s.eof())),
            MetaData::Stdio { blocked } => Some((false, blocked, s.eof)),
            MetaData::Socket { timed_out, blocked } => Some((timed_out, blocked, s.eof)),
            MetaData::Own => None,
        };
        let wrapper = s.wrapper;
        let wrapper_data = s.ops.wrapper_data();
        let stream_type = s.ops.label();
        let mode = s.mode.clone();
        let unread = i64::try_from(s.writepos - s.readpos).unwrap_or(i64::MAX);
        let seekable = s.ops.seekable() && !s.flags.no_seek;
        let uri = s.orig_path.clone();
        let mut entries = Vec::new();
        if let Some((timed_out, blocked, eof)) = head {
            entries.push(("timed_out", MetaValue::Bool(timed_out)));
            entries.push(("blocked", MetaValue::Bool(blocked)));
            entries.push(("eof", MetaValue::Bool(eof)));
        }
        if let Some(data) = wrapper_data {
            entries.push(("wrapper_data", MetaValue::Value(data)));
        }
        if !wrapper.is_empty() {
            entries.push(("wrapper_type", MetaValue::Str(wrapper.as_bytes().to_vec())));
        }
        entries.push((
            "stream_type",
            MetaValue::Str(stream_type.as_bytes().to_vec()),
        ));
        entries.push(("mode", MetaValue::Str(mode)));
        entries.push(("unread_bytes", MetaValue::Int(unread)));
        entries.push(("seekable", MetaValue::Bool(seekable)));
        if let Some(uri) = uri {
            entries.push(("uri", MetaValue::Str(uri)));
        }
        for (key, value) in entries {
            let value = match value {
                MetaValue::Bool(b) => Value::bool(b),
                MetaValue::Int(n) => Value::int(n),
                MetaValue::Str(bytes) => Value::string(PhpString::new(self, &bytes)?),
                MetaValue::Value(value) => {
                    self.addref(&value);
                    value
                }
            };
            let key = Key::Str(PhpString::new(self, key.as_bytes())?);
            out.set(self, key, value)?;
        }
        Ok(out)
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
        if let Some(Some(stream)) = self.resource_native_mut::<Option<PhpStream>>(res.heap()) {
            stream.res = Some(res);
        }
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
            None => {
                self.pclose_ret = taken.free(self);
            }
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
    ) -> Result<Option<PhpString>, Thrown> {
        if len > self.heap_memory_limit() {
            let asked = self.string_with(len, |_| {})?;
            self.release(Value::string(asked));
        }
        let mut buf = vec![0u8; len];
        let Some(read) = self.stream_op(stream, |req, s| s.read(req, &mut buf)) else {
            return Ok(None);
        };
        match read {
            Some(n) => Ok(Some(PhpString::new(self, &buf[..n])?)),
            None => Ok(None),
        }
    }
}

impl Request {
    /// `_php_stream_fopen_from_fd` (`Z/main/streams/plain_wrapper.c:303`)
    /// and `_php_stream_fopen_from_pipe` (`:350`): an open host fd as a
    /// stream of the plain files kind, `STDIO`, with no wrapper and no
    /// path, `mode` as `stream->mode`. A fd of a pipe, a fifo or a
    /// character device cannot seek (`detect_is_seekable`) and has no
    /// position; any other has the one `lseek(SEEK_CUR)` answers. A
    /// process pipe never seeks, and its close waits for the child.
    ///
    /// The `Err` is the memory limit, met registering the resource.
    pub fn stream_from_fd(&mut self, fd: OwnedFd, kind: FdStream<'_>) -> Result<Stream, Thrown> {
        let (mode, process) = match kind {
            FdStream::File { mode } => (mode, None),
            FdStream::Process { mode, child } => (mode, Some(child)),
            FdStream::Socket => return self.socket_stream(fd),
        };
        let mut file = std::fs::File::from(fd);
        let is_seekable = process.is_none()
            && file.metadata().is_ok_and(|meta| {
                use std::os::unix::fs::FileTypeExt;
                let kind = meta.file_type();
                !(kind.is_fifo() || kind.is_char_device() || kind.is_socket())
            });
        let position = if is_seekable {
            file.stream_position()
                .map_or(-1, |at| i64::try_from(at).unwrap_or(i64::MAX))
        } else {
            -1
        };
        let mut stream = PhpStream::new(
            Box::new(Stdio {
                file: Some(file),
                is_seekable,
                temp_name: None,
                process,
                blocking: true,
            }),
            mode,
        );
        stream.flags.no_seek = !is_seekable;
        stream.position = position;
        self.stream_alloc(stream)
    }
}

impl Stream {
    /// `pclose()`'s `zend_list_close(stream->res)`
    /// (`Z/ext/standard/file.c:829`): the resource closed, its
    /// destructor served, and `FG(pclose_ret)`, what the kind's close
    /// answered: a process pipe's exit status, `0` for any other kind,
    /// and `-1` where the child could not be waited for.
    pub fn pclose(self, req: &mut Request) -> i64 {
        req.pclose_ret = 0;
        self.resource().close(req);
        i64::from(req.pclose_ret)
    }

    /// `php_stream_cast` to a fd, `PHP_STREAM_AS_FD_FOR_SELECT`
    /// for [`Cast::Select`] and `PHP_STREAM_AS_FD` otherwise: the host
    /// fd the stream's kind reads and writes, or `None` for a
    /// kind with none, php's `Cannot represent a stream of type %s as a
    /// %s` warning under `warn`. Short of [`Cast::Select`] the stream
    /// is flushed and sought to its position first, and a [`Cast::Fd`]
    /// that leaves bytes in the read buffer warns that they are lost.
    /// The fd is the stream's, open while the stream is.
    pub fn cast(self, req: &mut Request, cast: Cast, warn: bool) -> Option<RawFd> {
        req.stream_op(self, |req, stream| stream.cast(req, cast, warn))
            .flatten()
    }

    /// `stream->writepos - stream->readpos`: the bytes read from the
    /// kind and not yet handed on, which `stream_select` counts as
    /// readable without asking the host.
    #[must_use]
    pub fn unread(self, req: &Request) -> usize {
        req.stream_ref(self).map_or(0, |s| s.writepos - s.readpos)
    }
}

/// One entry of what [`Request::stream_meta_data`] puts in the array,
/// before it is a value of the request's.
enum MetaValue {
    Bool(bool),
    Int(i64),
    Str(Vec<u8>),
    /// A value of the request's, one more owner taken for the array.
    Value(Value),
}

// ---------------------------------------------------------- directories

/// `php_plain_files_dirstream_ops` (`Z/main/streams/plain_wrapper.c:1104`)
/// over a `DIR *`: the directory's path, what `readdir(3)` has left, and
/// how many of `.` and `..` it has given, which `readdir(3)` gives first
/// and Rust's iterator leaves out.
struct Dir {
    path: std::path::PathBuf,
    entries: Option<std::fs::ReadDir>,
    dots: u8,
}

impl Ops for Dir {
    fn label(&self) -> &'static str {
        "dir"
    }

    /// `ops->write` is `NULL`: `-1`.
    fn write(&mut self, _req: &mut Request, _buf: &[u8]) -> Option<usize> {
        None
    }

    /// `php_plain_files_dirstream_read` for a count that is not one
    /// `php_stream_dirent`, which every read but `readdir`'s is: `-1`.
    fn read(&mut self, _req: &mut Request, _buf: &mut [u8], _eof: &mut bool) -> Option<usize> {
        None
    }

    /// `php_plain_files_dirstream_close`: `closedir(3)`, which dropping
    /// the iterator is.
    fn close(&mut self, _req: &mut Request) -> i32 {
        self.entries = None;
        0
    }

    /// `php_plain_files_dirstream_rewind`: `rewinddir(3)` whatever it is
    /// asked, and `0`, the position left as it was.
    fn seek(
        &mut self,
        _req: &mut Request,
        _offset: i64,
        _whence: Option<Whence>,
        _position: &mut i64,
    ) -> Option<bool> {
        self.entries = std::fs::read_dir(&self.path).ok();
        self.dots = 0;
        Some(true)
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `php_plain_files_dirstream_read`: the next name `readdir(3)`
    /// gives, `.` and `..` first, and `None` past the last.
    fn read_dir(&mut self, _req: &mut Request) -> Option<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        if self.dots < 2 {
            self.dots += 1;
            return Some(if self.dots == 1 {
                b".".to_vec()
            } else {
                b"..".to_vec()
            });
        }
        let entry = self.entries.as_mut()?.next()?.ok()?;
        Some(entry.file_name().as_bytes().to_vec())
    }
}

/// `stream_resource_regular_dtor` (`Z/main/streams/streams.c:1873`):
/// the stream freed under `PHP_STREAM_FREE_CLOSE |
/// PHP_STREAM_FREE_RSRC_DTOR`, with the resource already closed, and
/// what the kind's close answered kept as `FG(pclose_ret)`.
pub(crate) fn dtor(req: &mut Request, r: Resource) {
    let Some(stream) = req
        .resource_native_mut::<Option<PhpStream>>(r.heap())
        .and_then(Option::take)
    else {
        return;
    };
    req.pclose_ret = stream.free(req);
}

/// The `stream` resource type, `le_stream`, as `PHP_MINIT(streams)`
/// registers it (`Z/main/streams/streams.c:1910`).
pub(crate) fn register(resources: &mut crate::api::request::Resources) -> ResourceType {
    resources.register_type(b"stream", Some(dtor))
}
