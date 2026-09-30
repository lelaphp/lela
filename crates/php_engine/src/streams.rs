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
//! What is not here: the lazy body under `php://input` (#544), `php://fd/` and `php://filter/`, the filter chains, and the
//! read buffer's `DETECT_EOL` arm.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use php_heap::ResourceType;

use crate::api::raising::{Level, Thrown, raise, throw, warning};
use crate::api::request::{
    Cast, Closer, DirListing, EngineWrapper, FdStream, FilterFlags, HostPath, Notifier,
    OptionAnswer, Request, Stream, StreamOption, Whence,
};
use crate::api::values::{ClassId, Key, PhpArray, PhpString, Resource, Value};
use crate::wrapper::TemporaryCheck;

mod filters;
mod rfc2397;
mod sockets;
pub(crate) use filters::FilterCall;
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

    /// `ops->read` with the stream's `has_buffered_data` beside it, which
    /// a socket reads (`php_sockop_read`) and no other kind does.
    fn read_after_buffer(
        &mut self,
        req: &mut Request,
        buf: &mut [u8],
        eof: &mut bool,
        has_buffered_data: bool,
    ) -> Option<usize> {
        let _ = has_buffered_data;
        self.read(req, buf, eof)
    }

    /// The socket kind, where the stream is one: what the
    /// `PHP_STREAM_OPTION_CRYPTO_API` options and
    /// `PHP_STREAM_CONTEXT(stream)`'s lending reach.
    fn as_socket(&mut self) -> Option<&mut sockets::Socket> {
        None
    }

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

    /// Whether what `ops->read` and `ops->write` take counts into the
    /// notifier's `progress`, as `php_sockop_read` and `php_sockop_write`
    /// count it (`Z/main/streams/xp_socket.c:117`, `:205`): a socket's.
    fn counts_progress(&self) -> bool {
        false
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

    /// `php_glob_stream_get_path` and `php_glob_stream_get_count` of a
    /// glob stream, `None` for every other kind, which is
    /// `php_stream_is(stream, &php_glob_stream_ops)` answered no.
    fn glob(&self) -> Option<(Option<&[u8]>, usize)> {
        None
    }

    /// `php_stream_is(stream, PHP_STREAM_IS_STDIO)`: the kind is
    /// `php_stream_stdio_ops`.
    fn is_stdio(&self) -> bool {
        false
    }

    /// `ops->cast` to a fd, `cast` naming which of php's casts, what the
    /// kind is to it, [`CastTarget`]; `own` is the stream the kind is
    /// under, which a user-space stream's `stream_cast` must not answer.
    /// [`CastTarget::Refused`] for a kind with no fd, or none of the
    /// kind `cast` asks for, php's `FAILURE`.
    fn cast_target(
        &mut self,
        _req: &mut Request,
        _cast: Cast,
        _own: Option<Stream>,
    ) -> CastTarget<'_> {
        CastTarget::Refused
    }

    /// `ops->set_option` for the options [`StreamOption`] names: php's
    /// `PHP_STREAM_OPTION_RETURN_NOTIMPL` for a kind that takes none of
    /// them, which the generic layer answers next.
    fn set_option(&mut self, _req: &mut Request, _option: StreamOption) -> OptionAnswer {
        OptionAnswer::NotImplemented
    }

    /// The kind's own meta data under [`MetaData::Own`], the entries
    /// `stream_get_meta_data` puts first.
    fn own_meta(&self) -> Vec<(Vec<u8>, MetaValue)> {
        Vec::new()
    }

    /// The stream the kind is under, handed once it is allocated: what a
    /// user-space stream holds against `fclose` while one of its methods
    /// runs.
    fn adopt(&mut self, _own: Stream) {}

    /// Whether the kind has an `ops->write`: a directory and the `data:`
    /// wrapper's stream have none, which a write is php's `Stream is not
    /// writable` for. A kind that takes nothing, as a socket that would
    /// block does, answers the write `0`, where one that failed answers
    /// `false`.
    ///
    /// ```php
    /// $d = opendir('.'); var_dump(fwrite($d, 'x'));
    /// var_dump(file_put_contents('data:,abc', 'x'));
    /// [$a, $b] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, STREAM_IPPROTO_IP);
    /// stream_set_blocking($a, false);
    /// $big = str_repeat('a', 1000000);
    /// var_dump(fwrite($a, $big) > 0, fwrite($a, $big));
    /// ```
    /// ```expect
    /// Notice: fwrite(): Stream is not writable in %s on line 2
    /// bool(false)
    ///
    /// Notice: file_put_contents(): Stream is not writable in %s on line 3
    /// bool(false)
    /// bool(true)
    /// int(0)
    /// ```
    fn writable(&self) -> bool {
        true
    }

    /// `php_stream_is(stream, PHP_STREAM_IS_USERSPACE)`: a user-space
    /// stream, whose writes are handed to it a chunk at a time.
    fn is_userspace(&self) -> bool {
        false
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

    /// `PHP_STREAM_OPTION_XPORT_API`: one operation of a `transport`,
    /// `None` for php's `PHP_STREAM_OPTION_RETURN_NOTIMPL`, a kind that
    /// is no socket.
    fn xport(&mut self, _req: &mut Request, _op: sockets::XportOp<'_>) -> Option<sockets::Xported> {
        None
    }
}

/// What `ops->cast` makes of a kind asked for a fd, decoded once and
/// matched where php casts (`_php_stream_cast`): the host fd the kind
/// reads and writes (`php_stdiop_cast`, `php_sockop_cast`), one of the
/// host's standard streams, whose fd is the one the host handed, another
/// stream cast in its place (a user-space stream's `stream_cast`, a temp
/// stream's file), or nothing.
///
/// ```php
/// $t = fopen('php://temp', 'w+'); fwrite($t, "from temp\n"); rewind($t);
/// $p = proc_open('cat', [0 => $t, 1 => ['pipe', 'w']], $pipes);
/// var_dump(stream_get_contents($pipes[1]), proc_close($p));
/// var_dump(@proc_open('true', [0 => fopen('php://memory', 'r')], $pipes), error_get_last()['message']);
/// class W { public $context; public $with;
///     function stream_open($p, $m, $o, &$op) { return true; }
///     function stream_eof() { return false; }
///     function stream_cast($as) { return $this->with; } }
/// stream_wrapper_register('ww', 'W');
/// $u = fopen('ww://x', 'r');
/// stream_get_meta_data($u)['wrapper_data']->with = $t;
/// $r = [$u]; $w = $e = null;
/// var_dump(stream_select($r, $w, $e, 0));
/// stream_get_meta_data($u)['wrapper_data']->with = $u;
/// $r = [$u];
/// var_dump(@stream_select($r, $w, $e, 0), error_get_last()['message']);
/// ```
/// ```expect exit=255
/// string(10) "from temp
/// "
/// int(0)
/// bool(false)
/// string(74) "proc_open(): Cannot represent a stream of type MEMORY as a File Descriptor"
/// int(1)
///
/// Fatal error: Uncaught ValueError: No stream arrays were passed in %s:17
/// Stack trace:
/// #0 %s(17): stream_select(Array, NULL, NULL, 0)
/// #1 {main}
///   thrown in %s on line 17
/// ```
pub(crate) enum CastTarget<'a> {
    Fd(BorrowedFd<'a>),
    Host(Standard),
    Stream(Stream),
    Refused,
}

/// `php_stream_statbuf`'s thirteen fields in the order `php_fstat` puts
/// them: `dev`, `ino`, `mode`, `nlink`, `uid`, `gid`, `rdev`, `size`,
/// `atime`, `mtime`, `ctime`, `blksize` and `blocks`.
pub(crate) type StatBuf = [i64; 13];

/// What a kind's `PHP_STREAM_OPTION_META_DATA_API` answers, which is
/// what `stream_get_meta_data` puts first.
#[derive(Clone, Copy)]
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
    /// The option handed to the kind of the stream under this one,
    /// `self->stream->ops->set_option`, as `php_gziop_set_option` hands
    /// it: that kind's answer, and the generic three of this stream
    /// where that kind takes no option.
    Of(Stream),
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
    /// `stream->has_buffered_data`: within one `_php_stream_read`, bytes
    /// were had already, so the kind's read that follows must not wait.
    has_buffered_data: bool,
    /// What a wrapper set on the stream, boxed apart since a stream of
    /// the engine's own kinds has none of it.
    wrapped: Option<Box<Wrapped>>,
}

/// The fields of `php_stream` a wrapper writes on a stream it opened
/// over another kind: `PHP_STREAM_CONTEXT(stream)`'s notifier,
/// `stream->wrapperthis` with the wrapper's `stream_closer`,
/// `stream->wrapperdata` (one owner the stream's, which
/// `stream_get_meta_data` prints ahead of what the kind keeps),
/// `stream->wrapper->wops->stream_stat`, which `fstat()` asks in place of
/// the kind's, and `stream->ctx`, `PHP_STREAM_CONTEXT(stream)`, one owner
/// the stream's, which a socket's `transport` and the wrappers that
/// connect one set.
#[derive(Default)]
struct Wrapped {
    context: Option<Resource>,
    notifier: Option<Box<dyn Notifier>>,
    closer: Option<Box<dyn Closer>>,
    wrapperdata: Option<Value>,
    stat: Option<fn(&mut Request, Stream) -> Option<StatBuf>>,
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
    /// `PHP_STREAM_FLAG_NO_FCLOSE` on a stream an object keeps, which
    /// `SplFileObject` sets on the file it opened: `fclose()` refuses it.
    no_fclose: bool,
    /// `PHP_STREAM_FLAG_DETECT_EOL` and `PHP_STREAM_FLAG_EOL_MAC`.
    eol: Eol,
}

/// What ends a line for `php_stream_locate_eol`: `\n`, php's default,
/// which a DOS `\r\n` ends in too; `\r` alone, once a line was found
/// to end so; or not yet known, where `auto_detect_line_endings` was on
/// as the stream was made (`PHP_STREAM_FLAG_DETECT_EOL`), which the
/// first line that ends settles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Eol {
    #[default]
    Unix,
    Mac,
    Detect,
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
            has_buffered_data: false,
            wrapped: None,
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

    /// `stream->flags & PHP_STREAM_FLAG_NO_FCLOSE`, which a directory
    /// carries too.
    pub(crate) const fn no_fclose(&self) -> bool {
        self.flags.is_dir || self.flags.no_fclose
    }

    /// `stream->flags |= PHP_STREAM_FLAG_NO_FCLOSE`.
    pub(crate) fn forbid_fclose(&mut self) {
        self.flags.no_fclose = true;
    }

    /// `stream->orig_path`.
    pub(crate) fn orig_path(&self) -> Option<&[u8]> {
        self.orig_path.as_deref()
    }

    /// `php_stream_context_set`'s notifier: the one given in place, the
    /// one the stream had handed back.
    pub(crate) fn set_notifier(
        &mut self,
        notifier: Option<Box<dyn Notifier>>,
    ) -> Option<Box<dyn Notifier>> {
        std::mem::replace(&mut self.wrapped_mut().notifier, notifier)
    }

    /// `stream->wrapperthis`: the closer given in place, where the
    /// stream has none yet; one it has already is kept, and the new one
    /// is left in `closer` for the caller to close.
    pub(crate) fn set_closer(&mut self, closer: Option<Box<dyn Closer>>) -> Option<()> {
        let wrapped = self.wrapped_mut();
        if wrapped.closer.is_some() {
            return None;
        }
        wrapped.closer = closer;
        Some(())
    }

    /// `stream->wrapperdata`: the value given in place, the one the
    /// stream held handed back.
    pub(crate) fn set_wrapper_data(&mut self, value: Option<Value>) -> Option<Value> {
        std::mem::replace(&mut self.wrapped_mut().wrapperdata, value)
    }

    /// `stream->wrapper->wops->stream_stat`, where the wrapper has one.
    pub(crate) fn set_wrapper_stat(
        &mut self,
        stat: Option<fn(&mut Request, Stream) -> Option<StatBuf>>,
    ) {
        if stat.is_some() {
            self.wrapped_mut().stat = stat;
        }
    }

    /// What a wrapper set on the stream, made where it set nothing yet.
    fn wrapped_mut(&mut self) -> &mut Wrapped {
        self.wrapped.get_or_insert_with(Box::default)
    }

    /// The notifier the stream's `context` has, where a wrapper set one.
    fn notifier_mut(&mut self) -> Option<&mut Box<dyn Notifier>> {
        self.wrapped.as_mut()?.notifier.as_mut()
    }

    /// `stream->position = 0` and `strlcpy(stream->mode, mode)`, cut to
    /// the fifteen bytes `char mode[16]` holds.
    pub(crate) fn rewind_as(&mut self, mode: &[u8]) {
        self.position = 0;
        self.mode = mode[..mode.len().min(15)].to_vec();
    }

    /// `php_stream_notify_progress_increment(PHP_STREAM_CONTEXT(stream),
    /// n, 0)` for what a socket's read or write took; a throwable the
    /// callback leaves is kept for the native.
    fn count_progress(&mut self, req: &mut Request, n: usize) {
        if n == 0 || !self.ops.counts_progress() {
            return;
        }
        if let Some(notifier) = self.notifier_mut()
            && let Err(thrown) = notifier.progress(req, n)
        {
            filters::defer(req, thrown);
        }
    }

    /// `out_check_eof`'s `php_stream_notify_completed`: the notifier told
    /// where the read or write changed `eof` from `old_eof`.
    fn notify_eof_change(&mut self, req: &mut Request, old_eof: bool) {
        if old_eof == self.eof {
            return;
        }
        if let Some(notifier) = self.notifier_mut()
            && let Err(thrown) = notifier.completed(req)
        {
            filters::defer(req, thrown);
        }
    }

    /// `_php_stream_fill_read_buffer` (`Z/main/streams/streams.c:564`):
    /// through the filter chain where the stream has one, else as
    /// [`PhpStream::fill_unfiltered`] does; a fill that changed `eof`
    /// notifies `PHP_STREAM_NOTIFY_COMPLETED`.
    fn fill_read_buffer(&mut self, req: &mut Request, size: usize) -> bool {
        let old_eof = self.eof;
        let filled = if self.readfilters.is_empty() {
            self.fill_unfiltered(req, size)
        } else {
            self.fill_filtered(req, size)
        };
        self.notify_eof_change(req, old_eof);
        filled
    }

    /// `_php_stream_fill_read_buffer` without a filter chain: nothing
    /// when the buffer already holds `size` bytes; otherwise the buffer
    /// compacted and grown by one chunk where the chunk would not fit,
    /// and one read of the kind into the room after `writepos`. `false`
    /// is the read that failed.
    fn fill_unfiltered(&mut self, req: &mut Request, size: usize) -> bool {
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
            has_buffered_data,
            ..
        } = self;
        match ops.read_after_buffer(req, &mut readbuf[*writepos..], eof, *has_buffered_data) {
            Some(justread) => {
                *writepos += justread;
                self.count_progress(req, justread);
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
                self.has_buffered_data = true;
            }
            if size == 0 {
                break;
            }
            let direct = self.flags.no_buffer || self.chunk_size == 1;
            let toread = if self.readfilters.is_empty() && direct {
                let Self {
                    ops,
                    eof,
                    has_buffered_data,
                    ..
                } = self;
                match ops.read_after_buffer(req, &mut buf[didread..], eof, *has_buffered_data) {
                    Some(n) => {
                        self.count_progress(req, n);
                        n
                    }
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
                self.has_buffered_data = true;
            } else {
                break;
            }
            if !greedy {
                break;
            }
        }
        if didread > 0 {
            self.position += i64::try_from(didread).unwrap_or(i64::MAX);
            self.has_buffered_data = false;
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
    /// buffer holds a byte, else what the kind's last read said, or, where
    /// that was not eof, whether the kind answers
    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` with `_ERR`.
    pub(crate) fn eof(&mut self, req: &mut Request) -> bool {
        if self.writepos > self.readpos {
            return false;
        }
        if !self.eof && self.set_option(req, StreamOption::Liveness) == OptionAnswer::Err {
            self.eof = true;
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
        if !self.ops.writable() {
            let text = format!("{}(): Stream is not writable", req.active_native());
            raise(req, Level::Notice, &text);
            return None;
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
        let old_eof = self.eof;
        let wrote = self.write_kind(req, buf);
        self.notify_eof_change(req, old_eof);
        wrote
    }

    /// The body of [`PhpStream::write_buffer`]: the kind written until
    /// every byte is taken or one write takes none.
    fn write_kind(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        if buf.is_empty() {
            return Some(0);
        }
        if !self.flags.no_seek && self.readpos != self.writepos {
            self.readpos = 0;
            self.writepos = 0;
            let Self { ops, position, .. } = self;
            let _ = ops.seek(req, *position, Some(Whence::Set), position);
        }
        // A user-space stream is handed a chunk at a time, since what it
        // is handed is a string of the request's (GH-13071).
        let chunk = if self.ops.is_userspace() {
            self.chunk_size.max(1)
        } else {
            usize::MAX
        };
        let mut didwrite = 0;
        let mut rest = buf;
        while !rest.is_empty() {
            let Self { ops, .. } = self;
            match ops.write(req, &rest[..rest.len().min(chunk)]) {
                Some(justwrote) if justwrote > 0 => {
                    self.count_progress(req, justwrote);
                    rest = &rest[justwrote..];
                    didwrite += justwrote;
                    self.position += i64::try_from(justwrote).unwrap_or(i64::MAX);
                }
                // `retval = justwrote`: a kind that took nothing, as a
                // socket that would block does, answers `0`, and one that
                // failed `-1`, where nothing was written before.
                answered if didwrite == 0 => return answered,
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

    /// `_php_stream_search_delim` (`Z/main/streams/streams.c:1036`): where
    /// `delim` starts in the buffered bytes, at most `maxlen` of them,
    /// looked for past the first `skip`.
    fn search_delim(&self, maxlen: usize, skip: usize, delim: &[u8]) -> Option<usize> {
        let seek_len = (self.writepos - self.readpos).min(maxlen);
        if skip >= seek_len {
            return None;
        }
        let hay = &self.readbuf[self.readpos + skip..self.readpos + seek_len];
        hay.windows(delim.len())
            .position(|w| w == delim)
            .map(|at| skip + at)
    }

    /// `php_stream_get_record` (`Z/main/streams/streams.c:1063`), what
    /// `stream_get_line` reads: the buffer filled a chunk at a time, for
    /// every kind, until `delim` is in it or `maxlen` bytes are; then the
    /// bytes before the delimiter, which is skipped, or `maxlen` of them.
    /// `None` is php's `NULL`: nothing found and nothing more to come yet
    /// on a stream short of eof, which a non-blocking one answers, or no
    /// byte at all at eof.
    pub(crate) fn get_record(
        &mut self,
        req: &mut Request,
        maxlen: usize,
        delim: &[u8],
    ) -> Option<Vec<u8>> {
        if maxlen == 0 {
            return None;
        }
        let has_delim = !delim.is_empty();
        let mut found = if has_delim {
            self.search_delim(maxlen, 0, delim)
        } else {
            None
        };
        let mut buffered_len = self.writepos - self.readpos;
        while found.is_none() && buffered_len < maxlen {
            let to_read_now = (maxlen - buffered_len).min(self.chunk_size);
            if !self.fill_read_buffer(req, buffered_len + to_read_now) && self.fatal_error {
                return None;
            }
            let just_read = (self.writepos - self.readpos).saturating_sub(buffered_len);
            if just_read == 0 {
                break;
            }
            if has_delim {
                let skip = buffered_len.saturating_sub(delim.len() - 1);
                found = self.search_delim(maxlen, skip, delim);
                if found.is_some() {
                    break;
                }
            }
            buffered_len += just_read;
        }
        let buffered = self.writepos - self.readpos;
        let len = match found {
            Some(at) => at,
            None if !has_delim && buffered >= maxlen => maxlen,
            None if buffered < maxlen && !self.eof => return None,
            None if buffered == 0 && self.eof => return None,
            None => buffered.min(maxlen),
        };
        let mut out = vec![0; len];
        let got = self.read(req, &mut out).unwrap_or(0);
        out.truncate(got);
        if found.is_some() {
            self.readpos += delim.len();
            self.position += i64::try_from(delim.len()).unwrap_or(0);
        }
        Some(out)
    }

    /// `php_stream_locate_eol` (`Z/main/streams/streams.c:909`) over the
    /// read buffer: where the line ends, `\n` or, for a stream found to
    /// end its lines with `\r` alone, `\r`; a stream still detecting
    /// settles on `\r` where one stands with no `\n` right after it and
    /// no `\n` before it, and on `\n` where a `\n` stands.
    ///
    /// ```php
    /// $f = __DIR__ . '/eol.txt';
    /// file_put_contents($f, "one\rtwo\rthree");
    /// $show = fn($line) => var_dump(strtr($line, "\r", '|'));
    /// $h = fopen($f, 'r'); $show(fgets($h));
    /// @ini_set('auto_detect_line_endings', '1');
    /// $h = fopen($f, 'r'); $show(fgets($h)); $show(fgets($h)); $show(fgets($h));
    /// unlink($f);
    /// ```
    /// ```expect
    /// string(13) "one|two|three"
    /// string(4) "one|"
    /// string(4) "two|"
    /// string(5) "three"
    /// ```
    fn locate_eol(&mut self) -> Option<usize> {
        let held = &self.readbuf[self.readpos..self.writepos];
        let find = |byte: u8| held.iter().position(|b| *b == byte);
        match self.flags.eol {
            Eol::Unix => find(b'\n'),
            Eol::Mac => find(b'\r'),
            Eol::Detect => {
                let (cr, lf) = (find(b'\r'), find(b'\n'));
                match cr {
                    Some(cr) if lf != Some(cr + 1) && lf.is_none_or(|lf| lf > cr) => {
                        self.flags.eol = Eol::Mac;
                        Some(cr)
                    }
                    _ if lf.is_some() => {
                        self.flags.eol = Eol::Unix;
                        lf
                    }
                    _ => None,
                }
            }
        }
    }

    /// `_php_stream_get_line` (`Z/main/streams/streams.c:958`) without
    /// `DETECT_EOL`: bytes up to and including the first `\n`, taken
    /// from the read buffer and the buffer filled a chunk at a time
    /// while it holds none, for every kind, the unbuffered ones too;
    /// `maxlen` is php's, so at most `maxlen - 1` bytes, and `None` is
    /// php's `grow_mode`, a line of any length. `None` back is a line of no
    /// bytes, at eof or after a read that failed.
    ///
    /// A fill a read filter broke (`PSFS_ERR_FATAL`, `fatal_error`) ends
    /// the line with nothing, what was gathered dropped, and so does
    /// [`PhpStream::get_record`]'s (GH-13264):
    ///
    /// ```php
    /// $s = fopen('php://memory', 'r+'); $z = gzcompress(str_repeat("line\n", 3000)); fwrite($s, substr($z, 0, -1) . "1"); rewind($s);
    /// stream_filter_append($s, 'zlib.inflate', STREAM_FILTER_READ, ['window' => 15]);
    /// $n = 0; while (($l = fgets($s)) !== false) $n++; var_dump($n, feof($s));
    /// rewind($s); var_dump(stream_get_line($s, 0, "\n"));
    /// ```
    /// ```expect
    /// Notice: fgets(): zlib: data error in %s on line 4
    /// int(0)
    /// bool(true)
    ///
    /// Notice: stream_get_line(): zlib: data error in %s on line 5
    /// bool(false)
    /// ```
    pub(crate) fn get_line(&mut self, req: &mut Request, maxlen: Option<usize>) -> Option<Vec<u8>> {
        if maxlen == Some(0) {
            return None;
        }
        let mut line = Vec::new();
        let mut left = maxlen.unwrap_or(0);
        loop {
            let avail = self.writepos - self.readpos;
            if avail > 0 {
                let eol = self.locate_eol();
                let held = &self.readbuf[self.readpos..self.writepos];
                let (mut cpysz, mut done) = match eol {
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
                // `fatal_error`, dropping what the line held (GH-13264).
                if !self.fill_read_buffer(req, toread) && self.fatal_error {
                    return None;
                }
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
            while len < max && !self.eof(req) {
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

    /// `_php_stream_stat` (`Z/main/streams/streams.c:888`): the
    /// `stream_stat` of the wrapper that opened the stream where it has
    /// one, else the kind's `ops->stat`.
    pub(crate) fn stat(&mut self, req: &mut Request) -> Option<StatBuf> {
        if let Some(stat) = self.wrapped.as_ref().and_then(|w| w.stat) {
            return self.res.and_then(|res| stat(req, Stream::of(res)));
        }
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

    /// `_php_stream_set_option` (`Z/main/streams/streams.c:1447`): the
    /// kind first; what it does not take, the chunk size and whether the
    /// read buffer is used, the generic layer takes.
    pub(crate) fn set_option(&mut self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        let answer = self.ops.set_option(req, option);
        if answer != OptionAnswer::NotImplemented {
            return answer;
        }
        match option {
            StreamOption::ChunkSize(size) => {
                OptionAnswer::Previous(std::mem::replace(&mut self.chunk_size, size))
            }
            StreamOption::ReadBuffer { buffered, .. } => {
                self.set_read_buffer(buffered);
                OptionAnswer::Ok
            }
            _ => answer,
        }
    }

    /// `PHP_STREAM_OPTION_READ_BUFFER` as `_php_stream_set_option`
    /// answers it for a kind that leaves it to the generic half:
    /// `PHP_STREAM_BUFFER_NONE` sets `NO_BUFFER`, anything else clears
    /// it.
    const fn set_read_buffer(&mut self, buffered: bool) {
        self.flags.no_buffer = !buffered;
    }

    /// The glob stream's path and count ([`Ops::glob`]).
    pub(crate) fn glob(&self) -> Option<(Option<&[u8]>, usize)> {
        self.ops.glob()
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
        // `php_openssl_sockop_cast`'s `PHP_STREAM_AS_FD_FOR_SELECT`: what
        // the crypto layer holds decrypted moved into the empty read
        // buffer, so `select(2)` is not asked to wait for bytes already had.
        if cast == Cast::Select && self.writepos == self.readpos {
            let pending = self
                .ops
                .as_socket()
                .map_or(0, |socket| socket.crypto_pending());
            if pending > 0 {
                let chunk = self.chunk_size;
                self.fill_read_buffer(req, pending.min(chunk));
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
        let own = self.res.map(Stream::of);
        let fd = match self.ops.cast_target(req, cast, own) {
            CastTarget::Fd(fd) => Some(fd.as_raw_fd()),
            CastTarget::Host(standard) => req.host_fd(standard).map(|fd| fd.as_raw_fd()),
            // `php_stream_cast(intstream, castas, retptr, 1)`: the stream
            // in this one's place warns of itself, and this one of itself
            // after it where that failed.
            CastTarget::Stream(inner) => req
                .stream_op(inner, |req, s| s.cast(req, cast, true))
                .flatten(),
            CastTarget::Refused => None,
        };
        let Some(fd) = fd else {
            if warn {
                let target = match cast {
                    Cast::Select => "select()able descriptor",
                    Cast::Socket => "Socket Descriptor",
                    Cast::Internal | Cast::Fd => "File Descriptor",
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
        if held > 0 && matches!(cast, Cast::Fd | Cast::Socket) {
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
        self.flush_owed(req);
        self.release(req)
    }

    /// `_php_stream_free`'s "make sure everything is saved": the write
    /// chain flushed closing, where the stream was written or has one.
    fn flush_owed(&mut self, req: &mut Request) {
        if self.flags.was_written || !self.writefilters.is_empty() {
            self.flush_closing(req, true);
        }
    }

    /// `_php_stream_free` past the flush: the kind closed, then each
    /// filter taken off, and what the close answered.
    fn release(mut self, req: &mut Request) -> i32 {
        let Self { ops, .. } = &mut self;
        let ret = ops.close(req);
        self.free_filters(req);
        if let Some(wrapped) = self.wrapped.take() {
            let Wrapped {
                context,
                notifier,
                closer,
                wrapperdata,
                ..
            } = *wrapped;
            if let Some(context) = context {
                req.release(context.as_value());
            }
            if let Some(closer) = closer
                && let Err(thrown) = closer.close(req)
            {
                filters::defer(req, thrown);
            }
            if let Some(data) = wrapperdata {
                req.release(data);
            }
            if let Some(notifier) = notifier {
                notifier.release(req);
            }
        }
        ret
    }

    /// `php_stream_free(stream, PHP_STREAM_FREE_KEEP_RSRC | ...)` as
    /// `fclose` reaches it: the owed flush while the resource is still
    /// open, so a user filter's `$this->stream` is the open `resource`
    /// php hands it; then `zend_list_close`, whose destructor finds the
    /// stream in its free already (php's `in_free` guard) and leaves it;
    /// then the kind closed and the filters taken off. The last owner's
    /// release reaches the free through the destructor instead, the
    /// resource closed already, as `zend_resource_dtor` closes it first.
    ///
    /// ```php
    /// class F extends php_user_filter {
    ///     function filter($in, $out, &$consumed, $closing): int {
    ///         while ($b = stream_bucket_make_writeable($in)) { $consumed += $b->datalen; stream_bucket_append($out, $b); }
    ///         if ($closing) echo get_resource_type($this->stream), "\n";
    ///         return PSFS_PASS_ON;
    ///     }
    /// }
    /// stream_filter_register('f', 'F');
    /// $m = fopen('php://memory', 'w');
    /// stream_filter_append($m, 'f', STREAM_FILTER_WRITE);
    /// fwrite($m, 'x');
    /// var_dump(fclose($m));
    /// $m = fopen('php://memory', 'w');
    /// stream_filter_append($m, 'f', STREAM_FILTER_WRITE);
    /// $m = null;
    /// ```
    /// ```expect
    /// stream
    /// bool(true)
    /// Unknown
    /// ```
    pub(crate) fn close_open(req: &mut Request, stream: Stream) {
        let res = stream.resource();
        req.stream_op(stream, |req, s| s.flush_owed(req));
        let taken = req
            .resource_native_mut::<Option<PhpStream>>(res.heap())
            .and_then(Option::take);
        res.close(req);
        if let Some(taken) = taken {
            req.pclose_ret = taken.release(req);
        }
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
/// which of the three ways it was opened. The label is `MEMORY` for
/// `php://memory`; `php://input` reads the request's body through the
/// same bytes under the label of the kind php reads it with, and
/// `php://stdin` through [`HostFd`] under a host that handed no fd 0,
/// since nothing in the engine reads a body lazily yet (#544).
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
    /// a longer one zero-fills. The bytes are php's `zend_string`, which
    /// `zend_string_realloc` grows, so a size past the memory limit is
    /// the heap's fatal for that string, asked of the request's heap as
    /// `php_stream_get_line` asks it, and kept for the native.
    fn truncate(&mut self, req: &mut Request, size: usize) -> Option<bool> {
        if self.mode == TempMode::ReadOnly {
            return Some(false);
        }
        if size > self.data.len() && size > req.heap_memory_limit() {
            match req.string_with(size, |_| {}) {
                Ok(asked) => req.release(Value::string(asked)),
                Err(thrown) => {
                    if req.stream_thrown.is_none() {
                        req.stream_thrown = Some(thrown);
                    }
                    return Some(false);
                }
            }
        }
        self.data.resize(size, 0);
        self.fpos = self.fpos.min(size);
        Some(true)
    }

    fn memory_buffer(&self) -> Option<&[u8]> {
        (self.label == "MEMORY").then_some(self.data.as_slice())
    }
}

/// A kind an extension implements, [`crate::api::request::StreamOps`],
/// served as every kind here is.
struct OfExtension(Box<dyn crate::api::request::StreamOps>);

impl Ops for OfExtension {
    fn label(&self) -> &'static str {
        self.0.label()
    }

    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        self.0.write(req, buf)
    }

    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        self.0.read(req, buf, eof)
    }

    fn close(&mut self, req: &mut Request) -> i32 {
        self.0.close(req);
        0
    }

    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        self.0.seek(req, offset, whence, position)
    }

    fn seekable(&self) -> bool {
        self.0.seekable()
    }

    fn stat(&mut self, req: &mut Request) -> Option<StatBuf> {
        self.0.stat(req)
    }

    fn flush(&mut self, req: &mut Request) -> bool {
        self.0.flush(req)
    }

    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        self.0.flock(req, operation)
    }

    fn meta_data(&self) -> MetaData {
        self.0
            .meta_data_from()
            .map_or(MetaData::Generic, MetaData::Of)
    }
}

/// `_php_stream_alloc` over a kind an extension implements, with
/// `PHP_STREAM_FLAG_NO_BUFFER` where the kind keeps no read buffer.
pub(crate) fn of_extension(ops: Box<dyn crate::api::request::StreamOps>, mode: &[u8]) -> PhpStream {
    let buffered = ops.buffered();
    let mut stream = PhpStream::new(Box::new(OfExtension(ops)), mode);
    stream.flags.no_buffer = !buffered;
    stream
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
    /// `stream->ops->label`: `TEMP`, or `RFC2397` for what the `data:`
    /// wrapper opened, which swaps in `php_stream_rfc2397_ops`.
    label: &'static str,
    /// `ts->meta`, what the `data:` wrapper read from its URL, and nothing
    /// for `php://temp`.
    meta: Vec<(Vec<u8>, MetaValue)>,
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
        self.move_to_file(req, inner, Some(count))
    }

    /// The memory stream moved into a temporary file, where a write of
    /// `count` at its position would reach `smax`, or always for `None`,
    /// which is `php_stream_temp_cast`'s conversion
    /// (`Z/main/streams/memory.c:463`).
    fn move_to_file(
        &mut self,
        req: &mut Request,
        inner: Stream,
        count: Option<usize>,
    ) -> Result<bool, Thrown> {
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
        if let Some(count) = count
            && usize::try_from(pos).unwrap_or(0) + count < self.smax
        {
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
        inner.free(req);
        self.inner = Some(file);
        req.stream_op(file, |req, f| f.seek(req, pos, Some(Whence::Set)));
        Ok(true)
    }
}

impl Ops for Temp {
    fn label(&self) -> &'static str {
        self.label
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
            inner.free(req);
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

    fn own_meta(&self) -> Vec<(Vec<u8>, MetaValue)> {
        self.meta.clone()
    }

    /// `php_stream_rfc2397_ops` has no `write`, where
    /// `php_stream_temp_ops` has `php_stream_temp_write`.
    fn writable(&self) -> bool {
        self.label != "RFC2397"
    }

    /// `php_stream_temp_cast` (`Z/main/streams/memory.c:463`): a file
    /// cast in this stream's place, the memory stream moved into a
    /// temporary one first where it is still in memory.
    fn cast_target(
        &mut self,
        req: &mut Request,
        _cast: Cast,
        _own: Option<Stream>,
    ) -> CastTarget<'_> {
        let Some(inner) = self.inner else {
            return CastTarget::Refused;
        };
        if !matches!(self.move_to_file(req, inner, None), Ok(true)) {
            return CastTarget::Refused;
        }
        self.inner.map_or(CastTarget::Refused, CastTarget::Stream)
    }

    /// `php_stream_temp_set_option`'s default arm: the inner stream's,
    /// its generic layer included.
    fn set_option(&mut self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        let Some(inner) = self.inner else {
            return OptionAnswer::NotImplemented;
        };
        req.stream_op(inner, |req, inner| inner.set_option(req, option))
            .unwrap_or(OptionAnswer::NotImplemented)
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
    let Some((file, path)) = req.open_temporary_fd(dir, prefix, TemporaryCheck::Never) else {
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
    /// not know and for a position before the start, the position left
    /// where it was.
    ///
    /// ```php
    /// $h = fopen(__DIR__ . '/seek.tmp', 'wb+');
    /// fwrite($h, '123456789');
    /// var_dump(fseek($h, -4, SEEK_SET), ftell($h), fseek($h, -1, SEEK_CUR), fread($h, 1));
    /// var_dump(fseek($h, -20, SEEK_CUR), ftell($h), fread($h, 1));
    /// fclose($h); unlink(__DIR__ . '/seek.tmp');
    /// ```
    /// ```expect
    /// int(-1)
    /// int(9)
    /// int(0)
    /// string(1) "9"
    /// int(-1)
    /// int(9)
    /// string(0) ""
    /// ```
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
            Some(Whence::Set) => match u64::try_from(offset) {
                Ok(offset) => SeekFrom::Start(offset),
                Err(_) => return Some(false),
            },
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

    /// `php_stdiop_set_option` (`Z/main/streams/plain_wrapper.c:803`):
    /// `PHP_STREAM_OPTION_BLOCKING` is `O_NONBLOCK` set or cleared on the
    /// fd, `PHP_STREAM_OPTION_SYNC_API` `php_stdiop_sync`'s `fsync(2)` or
    /// `fdatasync(2)` of it, and `PHP_STREAM_OPTION_WRITE_BUFFER` `setvbuf` on a `FILE *`,
    /// which a stream opened over a fd, as every one here is, has not:
    /// `-1`.
    fn set_option(&mut self, _req: &mut Request, option: StreamOption) -> OptionAnswer {
        match option {
            StreamOption::Blocking(blocking) => {
                let Some(file) = self.file.as_ref() else {
                    return OptionAnswer::Err;
                };
                if php_heap::set_fd_blocking(file.as_fd(), blocking).is_err() {
                    return OptionAnswer::Err;
                }
                self.blocking = blocking;
                OptionAnswer::Ok
            }
            StreamOption::Sync { data_only } => match self.file.as_ref() {
                Some(file) if php_heap::sync_fd(file.as_fd(), data_only).is_ok() => {
                    OptionAnswer::Ok
                }
                _ => OptionAnswer::Err,
            },
            StreamOption::WriteBuffer { .. } => OptionAnswer::Err,
            _ => OptionAnswer::NotImplemented,
        }
    }

    /// `php_stdiop_cast` (`Z/main/streams/plain_wrapper.c:645`): the
    /// file's fd, for any cast but `PHP_STREAM_AS_SOCKETD`, which it
    /// refuses.
    fn cast_target(
        &mut self,
        _req: &mut Request,
        cast: Cast,
        _own: Option<Stream>,
    ) -> CastTarget<'_> {
        if cast == Cast::Socket {
            return CastTarget::Refused;
        }
        self.file
            .as_ref()
            .map_or(CastTarget::Refused, |file| CastTarget::Fd(file.as_fd()))
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
fn plain_open(req: &mut Request, path: &HostPath, mode: &[u8]) -> Result<PhpStream, String> {
    let Some(options) = open_options(mode) else {
        return Err(format!(
            "`{}' is not a valid mode for fopen",
            String::from_utf8_lossy(mode)
        ));
    };
    // `_php_stream_fopen`'s `expand_filepath`, under `CWD_FILEPATH`, which
    // enters the path in the realpath cache; where it refuses, the host
    // is handed the path as written and answers for it.
    let cwd = req.cwd().into_os_string().into_encoded_bytes();
    let written = path.as_os_str().as_encoded_bytes();
    let host = match req.virtual_file_ex(&cwd, written, crate::api::request::Resolve::FilePath) {
        Some(expanded) => std::path::PathBuf::from(
            <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&expanded),
        ),
        None => path.to_path_buf(),
    };
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

/// Which of the host's three standard streams a `php://` path names,
/// fd 0, 1 or 2 under php's CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standard {
    Stdin = 0,
    Stdout = 1,
    Stderr = 2,
}

/// The host's standard streams as `php://stdin`, `php://stdout` and
/// `php://stderr` reach them, php's `STDIO` kind over fds 0, 1 and 2.
/// What is read and written is the host's sink and body, not the fds:
/// fd 1 is the body the sink is handed, past the `php_output` layer
/// (`sapi_cli_ub_write`, since php's CLI writes both to the one fd), fd 2
/// is the sink's error stream (design revision 16), and fd 0, under a
/// host that handed none, is the request's body read whole; a host that
/// handed its fd 0 has `php://stdin` read it ([`standard_open`]). Reading
/// fd 1 or fd 2 is the `EBADF` a write-only fd answers.
///
/// `fstat` and `ftruncate` are `fstat(2)` and `ftruncate(2)` of the fd
/// the host handed under the stream ([`crate::engine::FileHandles`]):
/// the process's own under the CLI binary, and a pipe's under the
/// in-process `Capture` host, since a pipe is what php's CLI has there
/// under a test runner. A host that handed none, and one that registers
/// no handles, which is every host but the CLI's, answers as a closed fd
/// does, `false` to both. What `ftruncate` answers on fd 1 is the kind of
/// fd the runner hands, a file or a pipe, so the fence leaves it out.
///
/// ```php
/// var_dump(count(fstat(STDOUT)), fstat(STDERR)['size'], ftruncate(STDIN, 0));
/// ```
/// ```expect
/// int(26)
/// int(0)
/// bool(false)
/// ```
enum HostFd {
    /// fd 0 under a host that handed none, over the body it reads.
    Stdin(Memory),
    Stdout,
    Stderr,
}

impl HostFd {
    fn standard(&self) -> Standard {
        match self {
            HostFd::Stdin(_) => Standard::Stdin,
            HostFd::Stdout => Standard::Stdout,
            HostFd::Stderr => Standard::Stderr,
        }
    }

    /// The fd under the stream, as a file `fstat(2)` and `ftruncate(2)`
    /// can be asked through: a copy the call closes behind it.
    fn file(&self, req: &Request) -> Option<std::fs::File> {
        let fd = req.host_fd(self.standard())?;
        fd.try_clone_to_owned().ok().map(std::fs::File::from)
    }
}

impl Ops for HostFd {
    fn label(&self) -> &'static str {
        "STDIO"
    }

    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        match self {
            HostFd::Stdin(body) => body.write(req, buf),
            HostFd::Stdout => {
                req.ub_write(buf);
                Some(buf.len())
            }
            HostFd::Stderr => {
                req.write_stderr(buf);
                Some(buf.len())
            }
        }
    }

    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        if let HostFd::Stdin(body) = self {
            return body.read(req, buf, eof);
        }
        let text = format!(
            "{}(): Read of {} bytes failed with errno={EBADF} Bad file descriptor",
            req.active_native(),
            buf.len()
        );
        raise(req, Level::Notice, &text);
        None
    }

    fn close(&mut self, req: &mut Request) -> i32 {
        match self {
            HostFd::Stdin(body) => body.close(req),
            HostFd::Stdout | HostFd::Stderr => 0,
        }
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `php_stdiop_stat` (`Z/main/streams/plain_wrapper.c:703`):
    /// `fstat(2)` of the host's fd.
    fn stat(&mut self, req: &mut Request) -> Option<StatBuf> {
        Some(stat_buf(&self.file(req)?.metadata().ok()?))
    }

    /// `php_stdiop_set_option`'s `PHP_STREAM_OPTION_TRUNCATE_API`
    /// (`Z/main/streams/plain_wrapper.c:983`): supported, as an fd is,
    /// and `ftruncate(2)` of the host's fd, which a pipe and a terminal
    /// refuse.
    fn truncate(&mut self, req: &mut Request, size: usize) -> Option<bool> {
        let Some(file) = self.file(req) else {
            return Some(false);
        };
        Some(
            file.set_len(u64::try_from(size).unwrap_or(u64::MAX))
                .is_ok(),
        )
    }

    /// `php_stdiop_set_option`'s `PHP_STREAM_OPTION_SYNC_API` over the
    /// host's fd, `php_stdiop_sync`: `fsync(2)` or `fdatasync(2)` of it,
    /// which a pipe refuses, and refused where the host handed no fd.
    fn set_option(&mut self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        let StreamOption::Sync { data_only } = option else {
            return OptionAnswer::NotImplemented;
        };
        match req.host_fd(self.standard()) {
            Some(fd) if php_heap::sync_fd(fd, data_only).is_ok() => OptionAnswer::Ok,
            _ => OptionAnswer::Err,
        }
    }

    fn meta_data(&self) -> MetaData {
        MetaData::Stdio { blocked: true }
    }

    fn is_stdio(&self) -> bool {
        true
    }

    /// `php_stdiop_cast` over fd 0, 1 or 2: the host's own, which the
    /// cast answers where the host handed its fds, and refuses as a
    /// socket.
    fn cast_target(
        &mut self,
        _req: &mut Request,
        cast: Cast,
        _own: Option<Stream>,
    ) -> CastTarget<'_> {
        if cast == Cast::Socket {
            return CastTarget::Refused;
        }
        CastTarget::Host(self.standard())
    }
}

impl Request {
    /// `PHP_CLI_PROCESS_TITLE`, which `do_cli` registers after
    /// `php_request_startup` as whether `cli_set_process_title` can set
    /// the title, which is whether the host saved the area `ps` shows
    /// (`crate::engine::ProcessTitle`), then
    /// `cli_register_file_handles` (`Z/sapi/cli/php_cli.c:519`):
    /// `php://stdin` opened `rb`, `php://stdout` and `php://stderr` `wb`,
    /// and the three constants on them, each holding its stream's one
    /// owner. Where one of the three does not open, php closes the others
    /// and registers none, which no host here reaches. A no-op for a host
    /// that asked for no handles ([`crate::engine::FileHandles`]).
    ///
    /// ```php
    /// var_dump(is_resource(STDIN), get_resource_type(STDOUT), defined('PHP_CLI_PROCESS_TITLE'));
    /// var_dump(STDIN, STDOUT, STDERR, fwrite(STDERR, "x"));
    /// var_dump(array_slice(array_keys(get_defined_constants(true)['Core']), -5));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(6) "stream"
    /// bool(true)
    /// resource(1) of type (stream)
    /// resource(2) of type (stream)
    /// resource(3) of type (stream)
    /// int(1)
    /// array(5) {
    ///   [0]=>
    ///   string(10) "PHP_BINARY"
    ///   [1]=>
    ///   string(21) "PHP_CLI_PROCESS_TITLE"
    ///   [2]=>
    ///   string(5) "STDIN"
    ///   [3]=>
    ///   string(6) "STDOUT"
    ///   [4]=>
    ///   string(6) "STDERR"
    /// }
    /// ```
    /// ```stderr
    /// x
    /// ```
    pub(crate) fn register_file_handles(&mut self) -> Result<(), Thrown> {
        if !self.registers_file_handles() {
            return Ok(());
        }
        let titled = self.process_title().is_some();
        self.register_host_constant(b"PHP_CLI_PROCESS_TITLE", Value::bool(titled));
        let mut opened = Vec::with_capacity(3);
        for (path, mode) in [
            (&b"php://stdin"[..], &b"rb"[..]),
            (b"php://stdout", b"wb"),
            (b"php://stderr", b"wb"),
        ] {
            opened.push(self.open_stream(path, mode, false)?);
        }
        let [Some(stdin), Some(stdout), Some(stderr)] = opened[..] else {
            for stream in opened.into_iter().flatten() {
                stream.free(self);
            }
            return Ok(());
        };
        self.standard_streams = [
            Some(stdin.resource()),
            Some(stdout.resource()),
            Some(stderr.resource()),
        ];
        self.register_host_constant(b"STDIN", stdin.as_value());
        self.register_host_constant(b"STDOUT", stdout.as_value());
        self.register_host_constant(b"STDERR", stderr.as_value());
        Ok(())
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

/// `php_stream_open_wrapper_ex`'s `options`, the bits of it this engine
/// reads.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // php's bitset, one field per bit read
pub(crate) struct OpenOptions {
    /// `USE_PATH`: the path looked for along `include_path` first.
    pub(crate) use_path: bool,
    /// `STREAM_USE_URL`: a wrapper that is no URL wrapper refused.
    pub(crate) use_url: bool,
    /// `REPORT_ERRORS`: a refusal is a warning.
    pub(crate) report: bool,
    /// `STREAM_OPEN_FOR_INCLUDE`: the open is an include's, which
    /// `allow_url_include` holds a URL wrapper to.
    pub(crate) for_include: bool,
    /// `STREAM_MUST_SEEK`: a stream whose kind cannot seek is copied
    /// into one that can, [`Request::make_seekable`].
    pub(crate) must_seek: bool,
    /// `STREAM_WILL_CAST`: the stream is to be cast to a fd, so the copy
    /// `STREAM_MUST_SEEK` makes is a temporary file,
    /// `PHP_STREAM_PREFER_STDIO`.
    pub(crate) will_cast: bool,
    /// `STREAM_DISABLE_OPEN_BASEDIR`: the plain files wrapper opens
    /// without asking the `open_basedir` guard.
    pub(crate) disable_open_basedir: bool,
    /// `STREAM_ONLY_GET_HEADERS`: the open `get_headers` makes.
    pub(crate) only_get_headers: bool,
}

impl OpenOptions {
    /// `REPORT_ERRORS` alone, what a native opens with.
    pub(crate) const REPORT: OpenOptions = OpenOptions {
        use_path: false,
        use_url: false,
        report: true,
        for_include: false,
        must_seek: false,
        will_cast: false,
        disable_open_basedir: false,
        only_get_headers: false,
    };

    /// `options & ~REPORT_ERRORS`, what a wrapper's opener is handed.
    const fn unreported(self) -> OpenOptions {
        OpenOptions {
            report: false,
            ..self
        }
    }

    /// The bits as php numbers them, what a user-space wrapper's
    /// `stream_open` is handed: `STREAM_USE_PATH` 1, `REPORT_ERRORS` 8,
    /// `STREAM_MUST_SEEK` 16, `STREAM_WILL_CAST` 32,
    /// `STREAM_OPEN_FOR_INCLUDE` 128, `STREAM_USE_URL` 256 and
    /// `STREAM_ONLY_GET_HEADERS` 512, with `STREAM_OPEN_FOR_ZEND_STREAM`
    /// 0x10000 beside the include's, since it is the one open that sets it
    /// here (`php_stream_open_for_zend_ex`).
    const fn bits(self) -> i64 {
        (self.use_path as i64)
            | (self.report as i64) << 3
            | (self.must_seek as i64) << 4
            | (self.will_cast as i64) << 5
            | (self.for_include as i64) << 7
            | (self.use_url as i64) << 8
            | (self.only_get_headers as i64) << 9
            | (self.for_include as i64) << 16
    }
}

/// `php_stream_url_wrap_php` (`Z/ext/standard/php_fopen_wrapper.c:176`):
/// the path after `php://`, case-insensitively, as `temp`, with
/// `/maxmemory:N` after it, `memory`, `output`, `input`, `stdin`,
/// `stdout`, `stderr`, `fd/N` or `filter/...`; anything else is php's
/// `Invalid php:// URL specified` and nothing opened. `input`, `stdin`
/// and `fd/` are refused an include under `allow_url_include=0`.
#[allow(clippy::too_many_lines)] // php's one `php_stream_url_wrap_php`, kept as one
fn php_open(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    options: OpenOptions,
) -> Result<Opened, Thrown> {
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
    if lower.starts_with(b"filter/") {
        return filter_open(req, &path[6..], mode, options);
    }
    let for_include_refused = |req: &mut Request| {
        let refused = options.for_include
            && !req
                .ini_get(b"allow_url_include")
                .is_none_or(Request::ini_parse_bool);
        if refused && options.report {
            let text = format!(
                "{}(): URL file-access is disabled in the server configuration",
                req.active_native()
            );
            warning(req, &text);
        }
        refused
    };
    let stream = match lower.as_slice() {
        b"memory" => memory(TempMode::of(mode)),
        b"output" => PhpStream::new(Box::new(Output), b"wb"),
        // `php_stream_input_ops` seeks the body it reads
        // (`php_stream_input_seek`).
        b"input" => {
            if for_include_refused(req) {
                return Ok(Err(String::from("operation failed")));
            }
            PhpStream::new(
                Box::new(Memory {
                    data: req.input_bytes(),
                    fpos: 0,
                    mode: TempMode::ReadOnly,
                    label: "Input",
                }),
                b"rb",
            )
        }
        // Each a pipe or a terminal under the CLI, which
        // `detect_is_seekable` says does not seek.
        b"stdin" if for_include_refused(req) => return Ok(Err(String::from("operation failed"))),
        b"stdin" | b"stdout" | b"stderr" if req.standard_closed(standard_of(&lower)) => {
            return Ok(Err(String::from("operation failed")));
        }
        b"stdin" if req.host_fd(Standard::Stdin).is_some() => {
            return standard_open(req, Standard::Stdin, mode);
        }
        b"stdout" | b"stderr" if req.copies_standard() => {
            return standard_open(req, standard_of(&lower), mode);
        }
        b"stdin" | b"stdout" | b"stderr" => {
            let kind = match lower.as_slice() {
                b"stdin" => HostFd::Stdin(Memory {
                    data: Vec::new(),
                    fpos: 0,
                    mode: TempMode::ReadOnly,
                    label: "STDIO",
                }),
                b"stdout" => HostFd::Stdout,
                _ => HostFd::Stderr,
            };
            let mut stream = PhpStream::new(Box::new(kind), mode);
            stream.flags.no_seek = true;
            stream.position = -1;
            stream
        }
        _ if lower.starts_with(b"fd/") => {
            if req.sapi_name() != "cli" {
                if options.report {
                    let text = format!(
                        "{}(): Direct access to file descriptors is only available from command-line PHP",
                        req.active_native()
                    );
                    warning(req, &text);
                }
                return Ok(Err(String::from("operation failed")));
            }
            if for_include_refused(req) {
                return Ok(Err(String::from("operation failed")));
            }
            return fd_open(req, &path[3..], mode);
        }
        _ => {
            let text = format!("{}(): Invalid php:// URL specified", req.active_native());
            warning(req, &text);
            return Ok(Err(String::from("operation failed")));
        }
    };
    Ok(Ok(req.stream_alloc(stream)?))
}

/// `php://filter/` (`Z/ext/standard/php_fopen_wrapper.c:339`): the
/// resource after the first `/resource=` opened as its own wrapper opens
/// it, reporting nothing of its own, then each `/`-separated part before
/// it applied to it: `read=` a `|`-separated list for the read chain,
/// `write=` one for the write chain, and a bare list for the chains
/// `fopen`'s `$mode` reads and writes, each name URL-decoded, and php's `Unable to
/// create filter (%s)` after the filter layer's own warning for one
/// that cannot be made. A path with no `/resource=` is php's `Error`.
///
/// ```php
/// var_dump(file_get_contents('php://filter/read=string.toupper|string.rot13/resource=data:,abc'));
/// var_dump(file_get_contents('php://filter/string.toupper/nope%2Ex/resource=data:,abc'));
/// $f = fopen('php://filter/read=string.rot13/resource=data:,abc', 'r');
/// echo json_encode(array_slice(stream_get_meta_data($f), 1, 2)), ' ', stream_get_meta_data($f)['uri'], "\n";
/// var_dump(@file_get_contents('php://filter/read=string.rot13/resource=nofile'), error_get_last()['message']);
/// $w = fopen('php://filter/write=string.toupper/resource=php://memory', 'w+');
/// fwrite($w, "shout"); rewind($w); var_dump(fread($w, 10));
/// try { fopen('php://filter/read=string.rot13', 'r'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "NOP"
///
/// Warning: file_get_contents(): Unable to locate filter "nope.x" in %s on line 3
///
/// Warning: file_get_contents(): Unable to create filter (nope.x) in %s on line 3
/// string(3) "ABC"
/// {"wrapper_type":"PHP","stream_type":"RFC2397"} php://filter/read=string.rot13/resource=data:,abc
/// bool(false)
/// string(106) "file_get_contents(php://filter/read=string.rot13/resource=nofile): Failed to open stream: operation failed"
/// string(5) "SHOUT"
/// No URL resource specified
/// ```
fn filter_open(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    options: OpenOptions,
) -> Result<Opened, Thrown> {
    let Some(at) = path.windows(10).position(|w| w == b"/resource=") else {
        return Err(throw(req, "Error", "No URL resource specified"));
    };
    let Some(stream) = req.open_wrapper_ex(&path[at + 10..], mode, options)? else {
        return Ok(Err(String::from("operation failed")));
    };
    let read = mode.contains(&b'r') || mode.contains(&b'+');
    let write = mode.iter().any(|b| matches!(b, b'w' | b'+' | b'a'));
    for part in path[..at].split(|b| *b == b'/').filter(|p| !p.is_empty()) {
        let (list, chains) = if part.len() >= 5 && part[..5].eq_ignore_ascii_case(b"read=") {
            (&part[5..], (true, false))
        } else if part.len() >= 6 && part[..6].eq_ignore_ascii_case(b"write=") {
            (&part[6..], (false, true))
        } else {
            (part, (read, write))
        };
        for name in list.split(|b| *b == b'|').filter(|n| !n.is_empty()) {
            let name = rfc2397::url_decode(name);
            // `if (EG(exception)) { php_stream_close(stream); return NULL; }`
            if let Err(thrown) = stream.apply_filter(req, &name, chains) {
                stream.free(req);
                return Err(thrown);
            }
        }
    }
    Ok(Ok(stream))
}

/// `php://fd/N` (`Z/ext/standard/php_fopen_wrapper.c:292`): `N` all
/// decimal digits, below `getdtablesize()`, and `dup(2)` of it, the
/// host's own fd for 0, 1 and 2 where it handed them; a socket opens as
/// a socket, anything else as a plain file's kind over the copy.
///
/// ```php
/// foreach (['php://fd', 'php://fd/', 'php://fd/-2', 'php://fd/1/', 'php://fd/1023'] as $p) {
///     var_dump(@fopen($p, 'w'));
///     echo preg_replace('/smaller than \d+/', 'smaller than N', error_get_last()['message']), "\n";
/// }
/// var_dump(get_resource_type(fopen('php://fd/2', 'w')));
/// ```
/// ```expect
/// bool(false)
/// fopen(php://fd): Failed to open stream: operation failed
/// bool(false)
/// fopen(php://fd/): Failed to open stream: php://fd/ stream must be specified in the form php://fd/<orig fd>
/// bool(false)
/// fopen(php://fd/-2): Failed to open stream: The file descriptors must be non-negative numbers smaller than N
/// bool(false)
/// fopen(php://fd/1/): Failed to open stream: php://fd/ stream must be specified in the form php://fd/<orig fd>
/// bool(false)
/// fopen(php://fd/1023): Failed to open stream: Error duping file descriptor 1023; possibly it doesn't exist: [9]: Bad file descriptor
/// string(6) "stream"
/// ```
fn fd_open(req: &mut Request, digits: &[u8], mode: &[u8]) -> Result<Opened, Thrown> {
    let form = || String::from("php://fd/ stream must be specified in the form php://fd/<orig fd>");
    let body = digits.trim_ascii_start();
    let unsigned = body
        .strip_prefix(b"-")
        .or_else(|| body.strip_prefix(b"+"))
        .unwrap_or(body);
    if unsigned.is_empty() || !unsigned.iter().all(u8::is_ascii_digit) {
        return Ok(Err(form()));
    }
    let fildes = strtol(digits);
    let dtablesize = php_heap::fd_table_size();
    if fildes < 0 || fildes >= i64::from(dtablesize) {
        return Ok(Err(format!(
            "The file descriptors must be non-negative numbers smaller than {dtablesize}"
        )));
    }
    let standard = match fildes {
        0 => Some(Standard::Stdin),
        1 => Some(Standard::Stdout),
        2 => Some(Standard::Stderr),
        _ => None,
    };
    let duped = match standard.and_then(|s| req.host_fd(s)) {
        Some(fd) => fd.try_clone_to_owned(),
        None => php_heap::dup_fd(i32::try_from(fildes).unwrap_or(-1)),
    };
    let fd = match duped {
        Ok(fd) => fd,
        Err(error) => {
            return Ok(Err(format!(
                "Error duping file descriptor {fildes}; possibly it doesn't exist: [{}]: {}",
                error.raw_os_error().unwrap_or(0),
                strerror(&error)
            )));
        }
    };
    let kind = fd_kind(&fd, mode);
    Ok(Ok(req.stream_from_fd(fd, kind)?))
}

/// What a `php://` fd opens as: a socket's kind where
/// `zend_fstat(fd, &st) == 0 && (st.st_mode & S_IFMT) == S_IFSOCK`, the
/// plain file's otherwise (`Z/ext/standard/php_fopen_wrapper.c:361`).
fn fd_kind<'m>(fd: &OwnedFd, mode: &'m [u8]) -> FdStream<'m> {
    use std::os::unix::fs::FileTypeExt;
    let socket = fd.try_clone().is_ok_and(|copy| {
        std::fs::File::from(copy)
            .metadata()
            .is_ok_and(|meta| meta.file_type().is_socket())
    });
    if socket {
        FdStream::Socket
    } else {
        FdStream::File { mode }
    }
}

/// `php://stdin` under a host that handed its fd 0
/// (`Z/ext/standard/php_fopen_wrapper.c:238-256`): `php_stream_fopen_from_fd`
/// over a copy of it, the plain file's kind, so a read waits on the fd
/// as `read(2)` does and answers what the process at its other end has
/// sent so far, `stream_set_blocking` sets `O_NONBLOCK` on it, and
/// `stream_select` waits on it. What php's CLI reads its `STDIN` from is
/// the process's fd 0 and never the host's request body, which is
/// `php://input`'s. A child that reads its `STDIN` to the end before it
/// writes, as `proc_open`'s tests start one, reads what its parent wrote
/// and not the empty body. Under a test runner fd 0 is a pipe or
/// `/dev/null` that ends at once:
///
/// ```php
/// var_dump(stream_set_blocking(STDIN, false), fread(STDIN, 5), feof(STDIN));
/// $r = [STDIN]; $w = $e = null;
/// var_dump(stream_select($r, $w, $e, 0));
/// $m = stream_get_meta_data(STDIN); unset($m['uri']); echo json_encode($m), "\n";
/// ```
/// ```expect
/// bool(true)
/// string(0) ""
/// bool(true)
/// int(1)
/// {"timed_out":false,"blocked":false,"eof":true,"wrapper_type":"PHP","stream_type":"STDIO","mode":"rb","unread_bytes":0,"seekable":false}
/// ```
///
/// `php://stdout` and `php://stderr` opened again under php's CLI once the
/// constants stand are `dup(STDOUT_FILENO)` in the same way, writing past
/// the host's sink as php's write past `php_output`, and read as the
/// constants are, a pipe or a terminal that does not seek; a socket is
/// the socket kind (`Z/ext/standard/php_fopen_wrapper.c:361`).
fn standard_open(req: &mut Request, standard: Standard, mode: &[u8]) -> Result<Opened, Thrown> {
    let duped = req.host_fd(standard).map_or_else(
        || Err(std::io::Error::from_raw_os_error(EBADF)),
        |fd| fd.try_clone_to_owned(),
    );
    let fd = match duped {
        Ok(fd) => fd,
        Err(error) => return Ok(Err(strerror(&error))),
    };
    let kind = fd_kind(&fd, mode);
    let seeks = standard == Standard::Stdin || matches!(kind, FdStream::Socket);
    let stream = req.stream_from_fd(fd, kind)?;
    if !seeks {
        req.stream_op(stream, |_, opened| {
            opened.flags.no_seek = true;
            opened.position = -1;
        });
    }
    Ok(Ok(stream))
}

/// Which of the three a `php://` name is, `stdin` for any other.
fn standard_of(lower: &[u8]) -> Standard {
    match lower {
        b"stdout" => Standard::Stdout,
        b"stderr" => Standard::Stderr,
        _ => Standard::Stdin,
    }
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
    temp_stream(req, mode, smax, "TEMP", Vec::new())
}

/// [`temp`] under a label of its own and with the meta data
/// `stream_get_meta_data` starts with, which is what the `data:` wrapper
/// makes of one.
fn temp_stream(
    req: &mut Request,
    mode: TempMode,
    smax: usize,
    label: &'static str,
    meta: Vec<(Vec<u8>, MetaValue)>,
) -> Result<Stream, Thrown> {
    let kind = Temp {
        inner: None,
        smax,
        label,
        meta,
    };
    let mut outer = PhpStream::new(Box::new(kind), mode.as_mode());
    outer.flags.no_buffer = true;
    let outer = req.stream_alloc(outer)?;
    let inner = req.stream_alloc(memory(mode))?;
    req.stream_op(outer, |_, stream| stream.ops.enclose(inner));
    Ok(outer)
}

// ---------------------------------------------------------- wrappers

/// Which wrapper `php_stream_locate_url_wrapper`
/// (`Z/main/streams/streams.c:2010`) found for a path: the plain files
/// wrapper, the `php://` wrapper, the `data:` wrapper, one an extension
/// registered (its place in [`Request::wrappers`]), one a program
/// registered with `stream_wrapper_register`, or none.
#[derive(Clone)]
pub(crate) enum Located {
    Plain,
    Php,
    Data,
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

impl Registered {
    /// The row of a wrapper the engine keeps, under php's scheme for it.
    pub(crate) fn engine(which: EngineWrapper) -> Registered {
        let (scheme, wrapper) = match which {
            EngineWrapper::Php => (&b"php"[..], Located::Php),
            EngineWrapper::Plain => (&b"file"[..], Located::Plain),
            EngineWrapper::Data => (&b"data"[..], Located::Data),
        };
        Registered {
            scheme: scheme.to_vec(),
            wrapper,
        }
    }
}

impl Located {
    /// `wrapper->is_url`: the `data:` wrapper, and one a program
    /// registered with `STREAM_IS_URL`.
    pub(crate) fn is_url(&self, req: &Request) -> bool {
        match self {
            Located::Data => true,
            Located::Space(user) => user.is_url,
            Located::Ext(at) => req.wrappers.get(*at).is_some_and(|w| w.is_url),
            Located::Plain | Located::Php | Located::Nothing => false,
        }
    }
}

/// Whether two rows name the same wrapper, as `zend_hash_find_ptr(...) ==
/// wrapper` compares them.
fn same_wrapper(a: &Located, b: &Located) -> bool {
    match (a, b) {
        (Located::Plain, Located::Plain)
        | (Located::Php, Located::Php)
        | (Located::Data, Located::Data) => true,
        (Located::Ext(x), Located::Ext(y)) => x == y,
        _ => false,
    }
}

impl Request {
    /// `url_stream_wrappers_hash`, the wrappers every request starts
    /// with, in the order they were registered: the engine's own where
    /// `standard` places them, among the extensions' (#920).
    pub(crate) fn global_wrappers(&self) -> Vec<Registered> {
        self.url_stream_wrappers.clone()
    }

    /// `php_register_url_stream_wrapper`'s insert: the row at the end of
    /// the global table, `false` where its scheme is taken.
    pub(crate) fn add_url_wrapper(&mut self, row: Registered) -> bool {
        if self
            .url_stream_wrappers
            .iter()
            .any(|r| r.scheme == row.scheme)
        {
            return false;
        }
        self.url_stream_wrappers.push(row);
        true
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
    /// more, followed by `://`, or `data:` alone, RFC 2397's form,
    /// looked up as written and then lowered;
    /// one nobody registered is php's warning and then the path as
    /// written for the plain files wrapper. `file://` (or any scheme
    /// `strncasecmp` takes for it) with a host that is not `localhost`
    /// is php's warning and no wrapper, and otherwise the path after the
    /// slashes; once a program changed the table, a plain path goes to
    /// whatever it registered as `file` and to none where it unregistered
    /// that, php's `file:// wrapper is disabled`. A URL wrapper is refused
    /// under `allow_url_fopen=0`, and under `allow_url_include=0` for an
    /// include. Under `REPORT_ERRORS` alone is a refusal a warning; the
    /// wrapper nobody registered is warned of always.
    pub(crate) fn locate_wrapper(&mut self, path: &[u8]) -> (Located, Vec<u8>) {
        self.locate(path, OpenOptions::REPORT)
    }

    /// [`Request::locate_wrapper`] under `options`, as `php_resolve_path`
    /// asks it with `STREAM_OPEN_FOR_INCLUDE` and no `REPORT_ERRORS`.
    pub(crate) fn locate_for(&mut self, path: &[u8], options: OpenOptions) -> (Located, Vec<u8>) {
        self.locate(path, options)
    }

    fn locate(&mut self, path: &[u8], options: OpenOptions) -> (Located, Vec<u8>) {
        let n = path
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            .count();
        let mut protocol = (n > 1
            && path.get(n) == Some(&b':')
            && (path[n + 1..].starts_with(b"//") || (n == 4 && path.starts_with(b"data:"))))
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
                if options.report {
                    warning(self, &text);
                }
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
                    if options.report {
                        let text = format!(
                            "{}(): Remote host file access not supported, {}",
                            self.active_native(),
                            String::from_utf8_lossy(path)
                        );
                        warning(self, &text);
                    }
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
                if options.report {
                    let text = format!(
                        "{}(): file:// wrapper is disabled in the server configuration",
                        self.active_native()
                    );
                    warning(self, &text);
                }
                return (Located::Nothing, local);
            }
            return (Located::Plain, local);
        }
        let Some(found) = wrapper else {
            return (Located::Plain, path.to_vec());
        };
        let allowed =
            |req: &Request, name: &[u8]| req.ini_get(name).is_none_or(Request::ini_parse_bool);
        if found.is_url(self) {
            let refused = if !allowed(self, b"allow_url_fopen") {
                Some("allow_url_fopen")
            } else if (options.for_include || self.in_user_include)
                && !allowed(self, b"allow_url_include")
            {
                Some("allow_url_include")
            } else {
                None
            };
            if let Some(entry) = refused {
                if options.report {
                    let text = format!(
                        "{}(): {}:// wrapper is disabled in the server configuration by {entry}=0",
                        self.active_native(),
                        String::from_utf8_lossy(&path[..n])
                    );
                    warning(self, &text);
                }
                return (Located::Nothing, path.to_vec());
            }
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
        let options = OpenOptions {
            use_path: use_include_path,
            ..OpenOptions::REPORT
        };
        self.open_wrapper_ex(path, mode, options)
    }

    /// `_php_stream_open_wrapper_ex` under an extension's [`Opening`].
    pub(crate) fn open_wrapper_with(
        &mut self,
        path: &[u8],
        mode: &[u8],
        opening: crate::api::request::Opening,
    ) -> Result<Option<Stream>, Thrown> {
        let options = OpenOptions {
            use_path: opening.use_path,
            report: opening.warn,
            must_seek: opening.must_seek,
            will_cast: opening.will_cast,
            disable_open_basedir: opening.disable_open_basedir,
            ..OpenOptions::REPORT
        };
        self.open_wrapper_ex(path, mode, options)
    }

    /// `_php_stream_open_wrapper_ex` under `REPORT_ERRORS | STREAM_USE_URL
    /// | STREAM_ONLY_GET_HEADERS`, what `get_headers` opens with: a wrapper
    /// located that is no URL wrapper, the plain files one, `php://` and a
    /// registered one not flagged `STREAM_IS_URL` among them, is php's
    /// warning that the function may only be used against URLs, and
    /// nothing is opened (`Z/main/streams/streams.c:2278`); a URL wrapper
    /// is handed the two bits, as a user-space one's `stream_open` shows.
    ///
    /// ```php
    /// class W { public $context; function stream_open($p, $m, $o, &$op) { var_dump($o); return true; } function stream_eof() { return true; } }
    /// stream_wrapper_register('web', 'W', STREAM_IS_URL);
    /// var_dump(get_headers('web://x'));
    /// fopen('web://x', 'r');
    /// ```
    /// ```expect
    /// int(768)
    /// bool(false)
    /// int(0)
    /// ```
    pub(crate) fn open_url_wrapper(
        &mut self,
        path: &[u8],
        mode: &[u8],
    ) -> Result<Option<Stream>, Thrown> {
        let options = OpenOptions {
            use_url: true,
            only_get_headers: true,
            ..OpenOptions::REPORT
        };
        self.open_wrapper_ex(path, mode, options)
    }

    /// `_php_stream_open_wrapper_ex` under `options`; the opener is asked
    /// without `REPORT_ERRORS`, as php's is, so a wrapper that opens
    /// another, `php://filter`, reports nothing of the inner open.
    pub(crate) fn open_wrapper_ex(
        &mut self,
        path: &[u8],
        mode: &[u8],
        options: OpenOptions,
    ) -> Result<Option<Stream>, Thrown> {
        if path.is_empty() {
            return Err(throw(self, "ValueError", "Path must not be empty"));
        }
        let mut to_open = path.to_vec();
        let mut use_path = options.use_path;
        if options.use_path {
            match self.resolve_path(path) {
                Ok(resolved) => {
                    to_open = resolved;
                    use_path = false;
                }
                Err(crate::api::request::Unresolved::Threw(thrown)) => return Err(thrown),
                Err(crate::api::request::Unresolved::Missing(_)) => {}
            }
        }
        let (wrapper, path_to_open) = self.locate(&to_open, options);
        if options.use_url && !wrapper.is_url(self) {
            let text = format!(
                "{}(): This function may only be used against URLs",
                self.active_native()
            );
            warning(self, &text);
            return Ok(None);
        }
        let (label, opened) = match wrapper {
            Located::Plain => (
                PLAIN_FILES,
                match self.plain_opener(&path_to_open, mode, options) {
                    Ok(stream) => {
                        // `_php_stream_fopen`'s `php_clear_stat_cache`
                        // (`Z/main/streams/plain_wrapper.c:1209`).
                        self.forget_stats();
                        Ok(self.stream_alloc(stream)?)
                    }
                    Err(message) => Err(message),
                },
            ),
            Located::Php => (
                PHP_WRAPPER,
                php_open(self, &path_to_open, mode, options.unreported())?,
            ),
            Located::Data => ("RFC2397", rfc2397::open(self, &path_to_open, mode)?),
            Located::Ext(at) => self.open_ext(at, &path_to_open, mode, use_path, options)?,
            Located::Space(user) => (
                "user-space",
                userspace::open(
                    self,
                    &user,
                    &path_to_open,
                    mode,
                    options.unreported().bits(),
                )?,
            ),
            Located::Nothing => ("", Err(String::from("no suitable wrapper could be found"))),
        };
        match opened {
            Ok(stream) => {
                self.stream_op(stream, |_, s| {
                    s.wrapper = label;
                    s.orig_path = Some(to_open.clone());
                });
                if options.must_seek {
                    return self.make_seekable(stream, &to_open, options);
                }
                if mode.contains(&b'a') {
                    self.learn_append_position(stream);
                }
                Ok(Some(stream))
            }
            Err(message) => {
                if options.report {
                    self.display_wrapper_errors(&to_open, "Failed to open stream", &message);
                }
                Ok(None)
            }
        }
    }

    /// `php_plain_files_stream_opener` (`Z/main/streams/plain_wrapper.c:1262`):
    /// the `open_basedir` guard, which warns, unless
    /// `STREAM_DISABLE_OPEN_BASEDIR`, then `_php_stream_fopen` of the path
    /// it let through; a refusal is `strerror(errno)` as any failed open.
    fn plain_opener(
        &mut self,
        path: &[u8],
        mode: &[u8],
        options: OpenOptions,
    ) -> Result<PhpStream, String> {
        let guarded = if options.disable_open_basedir {
            Ok(self.unguarded_host_path(path))
        } else {
            self.check_open_basedir(path, true)
        };
        match guarded {
            Ok(host) => plain_open(self, &host, mode),
            Err(refused) => Err(strerror(&refused.into())),
        }
    }

    /// `_php_stream_open_wrapper_ex`'s call of an extension wrapper's
    /// `stream_opener`, handed the `options` less `REPORT_ERRORS` with
    /// the errors it logs kept apart from any logged before: the stream
    /// it opened, with the wrapper's `stream_stat` on it, or the text
    /// the open's warning shows.
    fn open_ext(
        &mut self,
        at: usize,
        path: &[u8],
        mode: &[u8],
        use_path: bool,
        options: OpenOptions,
    ) -> Result<(&'static str, Result<Stream, String>), Thrown> {
        let wrapper = &self.wrappers[at];
        let (label, open, stream_stat) = (wrapper.label, wrapper.open, wrapper.stream_stat);
        let Some(open) = open else {
            return Ok((
                label,
                Err(String::from("wrapper does not support stream open")),
            ));
        };
        let opening = crate::api::request::Opening {
            use_path,
            warn: false,
            for_include: options.for_include,
            must_seek: options.must_seek,
            will_cast: options.will_cast,
            disable_open_basedir: options.disable_open_basedir,
            only_get_headers: options.only_get_headers,
        };
        let outer = std::mem::take(&mut self.wrapper_errors);
        let opened = open(self, path, mode, opening);
        let logged = std::mem::replace(&mut self.wrapper_errors, outer);
        let opened = opened?;
        if let Some(stream) = opened {
            self.stream_op(stream, |_, s| s.set_wrapper_stat(stream_stat));
        }
        Ok((
            label,
            opened.ok_or_else(|| self.wrapper_errors_text(&logged)),
        ))
    }

    /// `_php_stream_open_wrapper_ex` for a stream opened for append
    /// (`Z/main/streams/streams.c:2360`): a kind that seeks, at position
    /// zero, learns where it is from `SEEK_CUR`.
    fn learn_append_position(&mut self, stream: Stream) {
        self.stream_op(stream, |req, s| {
            if !s.flags.no_seek && s.position == 0 {
                let PhpStream { ops, position, .. } = s;
                let mut newpos = 0;
                if ops.seek(req, 0, Some(Whence::Cur), &mut newpos) == Some(true) {
                    *position = newpos;
                }
            }
        });
    }

    /// `_php_stream_open_wrapper_ex`'s `STREAM_MUST_SEEK` arm
    /// (`Z/main/streams/streams.c:2325`) over `_php_stream_make_seekable`
    /// (`Z/main/streams/cast.c:373`): a stream whose kind seeks is
    /// handed back as it is, before the append position is learnt;
    /// anything else is copied whole into a temporary file under
    /// `STREAM_WILL_CAST`, a `php://temp` otherwise, sought back to its
    /// start with `path` as its `orig_path`, and closed. A copy that could
    /// not be made or failed closes both, and under `REPORT_ERRORS` is
    /// php's `could not make seekable` warning in place of the `Failed to
    /// open stream` one.
    ///
    /// ```php
    /// var_dump(gzopen("php://output", 14));
    /// $f = sys_get_temp_dir().'/must-seek-witness.gz.bz2';
    /// file_put_contents("compress.bzip2://$f", gzencode("line one\nline two\n"));
    /// $b = count(get_resources());
    /// var_dump(gzfile("compress.bzip2://$f"), count(get_resources()) - $b);
    /// unlink($f);
    /// ```
    /// ```expect
    /// Warning: gzopen(php://output): could not make seekable - php://output in %s on line 2
    /// bool(false)
    /// array(2) {
    ///   [0]=>
    ///   string(9) "line one
    /// "
    ///   [1]=>
    ///   string(9) "line two
    /// "
    /// }
    /// int(0)
    /// ```
    fn make_seekable(
        &mut self,
        stream: Stream,
        path: &[u8],
        options: OpenOptions,
    ) -> Result<Option<Stream>, Thrown> {
        if self.stream_ref(stream).is_some_and(|s| s.ops.seekable()) {
            return Ok(Some(stream));
        }
        let copy = if options.will_cast {
            temporary_file(self, None, b"php")?
        } else {
            Some(temp(self, TempMode::Default, PHP_STREAM_MAX_MEM)?)
        };
        if let Some(copy) = copy {
            if self.copy_all(stream, copy) {
                stream.free(self);
                self.stream_op(copy, |req, s| {
                    s.seek(req, 0, Some(Whence::Set));
                    s.orig_path = Some(path.to_vec());
                });
                return Ok(Some(copy));
            }
            copy.free(self);
        }
        stream.free(self);
        if options.report {
            let text = format!(
                "{}({}): could not make seekable - {}",
                self.active_native(),
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(path)
            );
            warning(self, &text);
        }
        Ok(None)
    }

    /// `_php_stream_copy_to_stream_ex(from, to, PHP_STREAM_COPY_ALL)`
    /// (`Z/main/streams/streams.c:1664`) past its `mmap` and
    /// `copy_file_range` shortcuts: a chunk read and written whole until
    /// a read gives nothing; `false` for a read that failed or a write
    /// that took nothing.
    fn copy_all(&mut self, from: Stream, to: Stream) -> bool {
        let mut buf = vec![0; CHUNK_SIZE];
        loop {
            let Some(Some(didread)) = self.stream_op(from, |req, s| s.read(req, &mut buf)) else {
                return false;
            };
            if didread == 0 {
                return true;
            }
            let mut rest = &buf[..didread];
            while !rest.is_empty() {
                match self.stream_op(to, |req, s| s.write(req, rest)).flatten() {
                    Some(n) if n > 0 => rest = &rest[n..],
                    _ => return false,
                }
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
            // `php_plain_files_dir_opener` (`Z/main/streams/plain_wrapper.c:1114`):
            // the guard, which warns, then `opendir(3)`.
            Located::Plain => (
                PLAIN_FILES,
                match self.check_open_basedir(&path_to_open, true) {
                    Err(refused) => Err(strerror(&refused.into())),
                    Ok(dir) => match std::fs::read_dir(&dir) {
                        Ok(entries) => {
                            let dir = PhpStream::new(
                                Box::new(Dir {
                                    path: dir.into(),
                                    entries: Some(entries),
                                    dots: 0,
                                }),
                                b"r",
                            );
                            Ok(self.stream_alloc(dir)?)
                        }
                        Err(error) => Err(strerror(&error)),
                    },
                },
            ),
            Located::Php => (PHP_WRAPPER, Err(String::from("not implemented"))),
            Located::Data => ("RFC2397", Err(String::from("not implemented"))),
            Located::Ext(at) => {
                let wrapper = &self.wrappers[at];
                let (label, open_dir) = (wrapper.label, wrapper.open_dir);
                let outer = std::mem::take(&mut self.wrapper_errors);
                let opened = open_dir.map(|open_dir| open_dir(self, &path_to_open));
                let logged = std::mem::replace(&mut self.wrapper_errors, outer);
                match opened {
                    Some(opened) => match opened? {
                        Some(listed) => {
                            let kind = Listing::new(label, listed);
                            (
                                label,
                                Ok(self.stream_alloc(PhpStream::new(Box::new(kind), b"r"))?),
                            )
                        }
                        None => (label, Err(self.wrapper_errors_text(&logged))),
                    },
                    None => (label, Err(String::from("not implemented"))),
                }
            }
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
        let crypto = self.crypto_meta(stream)?;
        let head = if let Some(mut head) = self.kind_meta_data(stream) {
            if let Some(crypto) = crypto {
                head.insert(0, (b"crypto".to_vec(), MetaValue::Value(crypto)));
            }
            head
        } else {
            let Some(eof) = self.stream_op(stream, |req, s| s.eof(req)) else {
                return Ok(out);
            };
            meta_three(false, true, eof)
        };
        let Some(s) = self.stream_ref(stream) else {
            return Ok(out);
        };
        let wrapper = s.wrapper;
        let wrapper_data = s
            .wrapped
            .as_ref()
            .and_then(|w| w.wrapperdata)
            .or_else(|| s.ops.wrapper_data());
        let stream_type = s.ops.label();
        let mode = s.mode.clone();
        let unread = i64::try_from(s.writepos - s.readpos).unwrap_or(i64::MAX);
        let seekable = s.ops.seekable() && !s.flags.no_seek;
        let uri = s.orig_path.clone();
        let mut entries = head;
        if let Some(data) = wrapper_data {
            entries.push((b"wrapper_data".to_vec(), MetaValue::Value(data)));
        }
        if !wrapper.is_empty() {
            entries.push((
                b"wrapper_type".to_vec(),
                MetaValue::Str(wrapper.as_bytes().to_vec()),
            ));
        }
        entries.push((
            b"stream_type".to_vec(),
            MetaValue::Str(stream_type.as_bytes().to_vec()),
        ));
        entries.push((b"mode".to_vec(), MetaValue::Str(mode)));
        entries.push((b"unread_bytes".to_vec(), MetaValue::Int(unread)));
        entries.push((b"seekable".to_vec(), MetaValue::Bool(seekable)));
        if let Some(uri) = uri {
            entries.push((b"uri".to_vec(), MetaValue::Str(uri)));
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
            let key = Key::Str(PhpString::new(self, &key)?);
            out.set(self, key, value)?;
        }
        if let Some(crypto) = crypto {
            self.release(crypto);
        }
        Ok(out)
    }

    /// `php_stream_context_set` (`Z/main/streams/streams.c:2385`): the
    /// stream keeps `context`, one owner more, in place of the one it
    /// kept, which gives its owner up; a socket's crypto layer reads it
    /// from then.
    pub(crate) fn set_stream_context(&mut self, stream: Stream, context: Option<Resource>) {
        if let Some(context) = context {
            self.addref(&context.as_value());
        }
        let old = self
            .stream_op(stream, |_, s| {
                if let Some(socket) = s.ops.as_socket() {
                    socket.set_ctx(context);
                }
                match context {
                    Some(context) => s.wrapped_mut().context.replace(context),
                    None => s.wrapped.as_mut().and_then(|w| w.context.take()),
                }
            })
            .flatten();
        if let Some(old) = old {
            self.release(old.as_value());
        }
    }

    /// `PHP_STREAM_CONTEXT(stream)`.
    pub(crate) fn stream_context(&self, stream: Stream) -> Option<Resource> {
        self.stream_ref(stream)?.wrapped.as_ref()?.context
    }

    /// What a socket's active crypto layer shows under `crypto`, ahead of
    /// the three the socket shows.
    fn crypto_meta(&mut self, stream: Stream) -> Result<Option<Value>, Thrown> {
        let meta = self
            .stream_op(stream, |req, s| {
                let socket = s.ops.as_socket()?;
                let layer = socket.crypto_layer()?;
                Some(layer.meta(req))
            })
            .flatten();
        match meta {
            Some(Ok(Some(array))) => Ok(Some(Value::array(array))),
            Some(Err(thrown)) => Err(thrown),
            Some(Ok(None)) | None => Ok(None),
        }
    }

    /// `php_stream_populate_meta_data`, `PHP_STREAM_OPTION_META_DATA_API`
    /// asked of the stream's kind: the entries it adds, `None` for the
    /// `PHP_STREAM_OPTION_RETURN_NOTIMPL` of a kind that takes no option
    /// and of a stream that is closed. A kind over another stream hands
    /// the option to that stream's kind, which answers with its own
    /// `stream->eof`.
    fn kind_meta_data(&self, stream: Stream) -> Option<Vec<(Vec<u8>, MetaValue)>> {
        let s = self.stream_ref(stream)?;
        match s.ops.meta_data() {
            MetaData::Generic => None,
            MetaData::Stdio { blocked } => Some(meta_three(false, blocked, s.eof)),
            MetaData::Socket { timed_out, blocked } => Some(meta_three(timed_out, blocked, s.eof)),
            MetaData::Own => Some(s.ops.own_meta()),
            MetaData::Of(inner) => self.kind_meta_data(inner),
        }
    }

    /// The message `php_stream_display_wrapper_errors` prints for an
    /// extension's wrapper: what its opener logged, a line each, `<br />`
    /// before each break under `html_errors`, or `operation failed` where
    /// it logged nothing.
    fn wrapper_errors_text(&self, logged: &[String]) -> String {
        if logged.is_empty() {
            return String::from("operation failed");
        }
        let html = self
            .ini_get(b"html_errors")
            .is_some_and(Request::ini_parse_bool);
        logged.join(if html { "<br />\n" } else { "\n" })
    }

    /// `php_stream_display_wrapper_errors` (`Z/main/streams/streams.c:175`)
    /// as `php_error_docref1` renders it: the native, the path in
    /// parentheses, the caption and the message; nothing where a
    /// throwable is pending, php testing for one ahead of it, so no
    /// warning added over what was already thrown.
    ///
    /// ```php
    /// class Test { function __destruct() { global $box; $box->value = null; } }
    /// $box = [new Test];
    /// try { getimagesize("dummy", $box); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Attempt to assign property "value" on null
    /// ```
    pub(crate) fn display_wrapper_errors(&mut self, path: &[u8], caption: &str, message: &str) {
        if self.is_pending() {
            return;
        }
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
    pub(crate) fn stream_alloc(&mut self, mut stream: PhpStream) -> Result<Stream, Thrown> {
        // `_php_stream_alloc`'s `FG(auto_detect_line_endings)`
        // (`Z/main/streams/streams.c:326`).
        if self
            .ini_get(b"auto_detect_line_endings")
            .is_some_and(Request::ini_parse_bool)
        {
            stream.flags.eol = Eol::Detect;
        }
        let kind = self.stream_type();
        let res = self.new_resource(kind, Some(stream))?;
        if let Some(Some(stream)) = self.resource_native_mut::<Option<PhpStream>>(res.heap()) {
            stream.res = Some(res);
            stream.ops.adopt(Stream::of(res));
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
            FdStream::Xport { name, blocking } => return self.xport_stream(fd, name, blocking),
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
    /// for [`Cast::Select`], `PHP_STREAM_AS_SOCKETD` for
    /// [`Cast::Socket`] and `PHP_STREAM_AS_FD` otherwise: the host
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

/// `timed_out`, `blocked` and `eof`, the three most kinds answer
/// `PHP_STREAM_OPTION_META_DATA_API` with and `stream_get_meta_data`
/// adds for one that takes no option.
fn meta_three(timed_out: bool, blocked: bool, eof: bool) -> Vec<(Vec<u8>, MetaValue)> {
    vec![
        (b"timed_out".to_vec(), MetaValue::Bool(timed_out)),
        (b"blocked".to_vec(), MetaValue::Bool(blocked)),
        (b"eof".to_vec(), MetaValue::Bool(eof)),
    ]
}

/// One entry of what [`Request::stream_meta_data`] puts in the array,
/// before it is a value of the request's.
#[derive(Clone)]
pub(crate) enum MetaValue {
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

    fn writable(&self) -> bool {
        false
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

/// A directory an extension's wrapper listed, php's own dir stream ops
/// for such a wrapper, `php_glob_stream_ops` among them: the entries in
/// order, `readdir` the next and `rewinddir` back to the first, under the
/// wrapper's label.
struct Listing {
    label: &'static str,
    listed: DirListing,
    at: usize,
    /// `pglob->path`: the directory part of the match a glob last handed
    /// out; nothing for any other listing.
    path: Option<Vec<u8>>,
}

impl Listing {
    /// `php_glob_stream_opener`'s last step for a glob, the path split
    /// off its first match or, where it matched nothing, its pattern.
    fn new(label: &'static str, listed: DirListing) -> Listing {
        let path = match &listed {
            DirListing::Names(_) => None,
            DirListing::Glob { pattern, matches } => {
                Some(glob_split(matches.first().unwrap_or(pattern)).0.to_vec())
            }
        };
        Listing {
            label,
            listed,
            at: 0,
            path,
        }
    }
}

/// `php_glob_stream_path_split` (`Z/main/streams/glob_wrapper.c:95`):
/// the directory part of a path and the name after its last `/`, the
/// directory keeping its `/` where it is the root alone.
fn glob_split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|b| *b == b'/') {
        Some(at) => {
            let dir_len = if at + 1 > 1 { at } else { at + 1 };
            (&path[..dir_len], &path[at + 1..])
        }
        None => (&path[..0], path),
    }
}

impl Ops for Listing {
    fn label(&self) -> &'static str {
        self.label
    }

    fn writable(&self) -> bool {
        false
    }

    fn write(&mut self, _req: &mut Request, _buf: &[u8]) -> Option<usize> {
        None
    }

    fn read(&mut self, _req: &mut Request, _buf: &mut [u8], _eof: &mut bool) -> Option<usize> {
        None
    }

    fn close(&mut self, _req: &mut Request) -> i32 {
        0
    }

    /// `php_glob_stream_rewind`: back to the first entry, a glob's path
    /// given up.
    fn seek(
        &mut self,
        _req: &mut Request,
        _offset: i64,
        _whence: Option<Whence>,
        _position: &mut i64,
    ) -> Option<bool> {
        self.at = 0;
        self.path = None;
        Some(true)
    }

    fn seekable(&self) -> bool {
        true
    }

    /// `php_glob_stream_read` for a glob: the name after the match's last
    /// `/`, the part before it kept as the path (`PHP_GLOB_APPEND` is
    /// always set), and past the last the path given up.
    fn read_dir(&mut self, _req: &mut Request) -> Option<Vec<u8>> {
        match &self.listed {
            DirListing::Names(entries) => {
                let entry = entries.get(self.at)?.clone();
                self.at += 1;
                Some(entry)
            }
            DirListing::Glob { matches, .. } => {
                let Some(found) = matches.get(self.at) else {
                    self.at = matches.len();
                    self.path = None;
                    return None;
                };
                let (dir, name) = glob_split(found);
                let name = name.to_vec();
                self.path = Some(dir.to_vec());
                self.at += 1;
                Some(name)
            }
        }
    }

    fn glob(&self) -> Option<(Option<&[u8]>, usize)> {
        match &self.listed {
            DirListing::Glob { matches, .. } => Some((self.path.as_deref(), matches.len())),
            DirListing::Names(_) => None,
        }
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
