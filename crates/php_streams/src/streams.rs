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

use crate::api::values::{Emalloc, Owner, ResourceType};

use crate::api::layer::{Face as _, TemporaryGuard};
use crate::api::raising::{Level, Thrown, raise, throw_error, warning};
use crate::api::request::{
    Cast, Closer, DirListing, EngineWrapper, FdStream, FilterFlags, HostPath, Notification,
    OptionAnswer, OwnedStream, Request, Standard, Stream, StreamCtx, StreamOption, Whence,
};
use crate::api::values::{ClassId, FreshString, PhpArray, PhpString, Resource, Value};

pub(crate) mod filters;
pub(crate) mod persistent;
mod rfc2397;
pub(crate) mod sockets;
use crate::state::Layer;
use crate::wrapper::PlainFiles;
pub(crate) use filters::FilterCall;
use persistent::Persistent;
use sockets::Transports;
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
    /// through, which only `php://temp` has; a kind that encloses none
    /// hands the owner back.
    fn enclose(&mut self, inner: Owner<Stream>) -> Option<Owner<Stream>> {
        Some(inner)
    }

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
    /// under, which a user-space stream's `stream_cast` must not answer,
    /// and `ret` whether php hands the op a `ret` to fill or asks only
    /// whether it would. [`CastTarget::Refused`] for a kind with no fd, or
    /// none of the kind `cast` asks for, php's `FAILURE`.
    fn cast_target(
        &mut self,
        _req: &mut Request,
        _cast: Cast,
        _own: Option<Stream>,
        _ret: Ret,
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

    /// What the kind `pemalloc`s for its own data before
    /// `_php_stream_alloc` lays the stream down, on the request's heap at
    /// php's size and given back by its `close`; nothing for a kind whose
    /// data is not yet charged. The `Err` is the memory limit's fatal.
    fn lay_down(&mut self, _req: &mut Request) -> Result<(), Thrown> {
        Ok(())
    }

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

/// What `_php_stream_cast` is handed as `ret`: a place for the fd, which
/// a cast fills and which may make something to fill it with, or `NULL`,
/// `php_stream_can_cast`, which asks only whether the kind would answer
/// and neither flushes, seeks, converts nor warns of its own. A kind that
/// answers the two alike ignores it; `php_stream_temp_cast` refuses the
/// question with no `ret` while its bytes are in memory, where a cast
/// moves them to a file (`Z/main/streams/memory.c:463-488`), and `php_userstreamop_cast` warns
/// of a bad `stream_cast` only when a `ret` was handed
/// (`Z/main/streams/userspace.c:1406`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ret {
    Null,
    Given,
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
    /// `stream->mode`.
    mode: Mode,
    /// `stream->orig_path`: the path `php_stream_open_wrapper_ex` opened,
    /// after `include_path`, and `None` for a stream no wrapper opened by
    /// path, a directory stream among them.
    orig_path: Option<OrigPath>,
    /// `stream->position`, `-1` for a stream that cannot seek.
    position: i64,
    /// `stream->eof`.
    eof: bool,
    /// `stream->fatal_error`.
    fatal_error: bool,
    /// `stream->flags`.
    flags: Flags,
    /// `stream->readbuf` over `readbuflen`.
    readbuf: ReadBuf,
    /// `stream->readpos`.
    readpos: usize,
    /// `stream->writepos`.
    writepos: usize,
    /// `stream->chunk_size`, which `stream_set_chunk_size` holds to
    /// `INT_MAX`; four bytes, so the stream stays inside the
    /// `php_stream` php allocates ([`PhpStream::chunk_size`]).
    chunk_size: u32,
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
/// over another kind: `stream->wrapperthis` with the wrapper's `stream_closer`,
/// `stream->wrapperdata` (one owner the stream's, which
/// `stream_get_meta_data` prints ahead of what the kind keeps),
/// `stream->wrapper->wops->stream_stat`, which `fstat()` asks in place of
/// the kind's, and `stream->ctx`, `PHP_STREAM_CONTEXT(stream)`, one owner
/// the stream's, which a socket's `transport` and the wrappers that
/// connect one set, and whose notifier the stream's reads and writes
/// tell.
#[derive(Default)]
struct Wrapped {
    /// `PHP_STREAM_CONTEXT(stream)`, the stream's owner of its `context`.
    context: Option<Owner<StreamCtx>>,
    closer: Option<Box<dyn Closer>>,
    wrapperdata: Option<Owner<Value>>,
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
    /// `stream->is_persistent`: what php allocates for the stream,
    /// `pemalloc`'d persistent, is no request's to count.
    persistent: bool,
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

/// `sizeof(php_stream)` (`Z/main/php_streams.h`), what
/// `_php_stream_alloc` `pemalloc`s and `_php_stream_free` gives back; the
/// stream's resource payload is laid inside an allocation of this size.
const PHP_STREAM_SIZE: usize = 208;

// The payload sits behind the resource payload's one-word table.
const _: () = assert!(
    size_of::<Option<PhpStream>>() + 8 <= PHP_STREAM_SIZE,
    "a stream does not fit the php_stream php allocates"
);

/// `stream->mode`, php's `char mode[16]`: what `php_stream_alloc` was
/// handed, cut to the fifteen bytes before its NUL.
#[derive(Clone, Copy)]
struct Mode([u8; 16]);

impl Mode {
    /// `strlcpy(ret->mode, mode, sizeof(ret->mode))`.
    fn of(mode: &[u8]) -> Mode {
        let mut held = [0; 16];
        let len = mode.len().min(15);
        held[..len].copy_from_slice(&mode[..len]);
        Mode(held)
    }

    /// The bytes before the NUL.
    fn bytes(&self) -> &[u8] {
        let len = self.0.iter().position(|b| *b == 0).unwrap_or(16);
        &self.0[..len]
    }
}

/// `stream->orig_path`, `pestrdup`'d: on the request's heap, the path
/// and its NUL, for a stream of the request's, and the process's own for
/// one the persistent list keeps across requests, as `pestrdup`'s
/// `persistent` argument picks.
enum OrigPath {
    Request(Emalloc),
    Persistent(Box<[u8]>),
}

impl OrigPath {
    /// `estrdup(path)`; the `Err` is the memory limit's fatal.
    fn request(req: &mut Request, path: &[u8]) -> Result<OrigPath, Thrown> {
        let mut block = Emalloc::new(req, path.len() + 1)?;
        block.bytes_mut()[..path.len()].copy_from_slice(path);
        Ok(OrigPath::Request(block))
    }

    /// The path, without the NUL.
    fn bytes(&self) -> &[u8] {
        match self {
            OrigPath::Request(block) => block.bytes().split_last().map_or(&[], |(_, path)| path),
            OrigPath::Persistent(path) => path,
        }
    }

    /// `pefree(stream->orig_path)`.
    fn free(self, req: &mut Request) {
        if let OrigPath::Request(block) = self {
            block.free(req);
        }
    }

    /// The path as the persistent list keeps it past the request, the
    /// request's copy given back.
    fn into_persistent(self, req: &mut Request) -> Box<[u8]> {
        match self {
            OrigPath::Request(block) => {
                let path = Box::from(block.bytes().split_last().map_or(&[][..], |(_, path)| path));
                block.free(req);
                path
            }
            OrigPath::Persistent(path) => path,
        }
    }
}

/// `stream->readbuf` over `readbuflen`: none before the first read,
/// `perealloc`'d as `_php_stream_fill_read_buffer` grows it, and
/// `pefree`'d with the stream: on the request's heap, or for a
/// persistent stream on the system allocator, which no request counts.
#[derive(Default)]
enum ReadBuf {
    #[default]
    None,
    Request(Emalloc),
    Persistent(Box<[u8]>),
}

impl std::ops::Deref for ReadBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            ReadBuf::None => &[],
            ReadBuf::Request(block) => block.bytes(),
            ReadBuf::Persistent(bytes) => bytes,
        }
    }
}

impl std::ops::DerefMut for ReadBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        match self {
            ReadBuf::None => &mut [],
            ReadBuf::Request(block) => block.bytes_mut(),
            ReadBuf::Persistent(bytes) => bytes,
        }
    }
}

impl ReadBuf {
    /// `stream->readbuf = perealloc(stream->readbuf, len, persistent)`,
    /// the bytes kept; the `Err` is the memory limit's fatal.
    fn resize(&mut self, req: &mut Request, len: usize, persistent: bool) -> Result<(), Thrown> {
        let grown = match std::mem::take(self) {
            ReadBuf::Persistent(bytes) => {
                let mut bytes = bytes.into_vec();
                bytes.resize(len, 0);
                *self = ReadBuf::Persistent(bytes.into_boxed_slice());
                return Ok(());
            }
            ReadBuf::None if persistent => {
                *self = ReadBuf::Persistent(vec![0; len].into_boxed_slice());
                return Ok(());
            }
            ReadBuf::Request(block) => block.resize(req, len),
            ReadBuf::None => Emalloc::new(req, len),
        };
        *self = ReadBuf::Request(grown?);
        Ok(())
    }

    /// `pefree(stream->readbuf)`.
    fn free(&mut self, req: &mut Request) {
        if let ReadBuf::Request(block) = std::mem::take(self) {
            block.free(req);
        }
    }
}

impl PhpStream {
    /// `stream->chunk_size`.
    fn chunk_size(&self) -> usize {
        self.chunk_size as usize
    }

    /// `_php_stream_alloc` (`Z/main/streams/streams.c:301`) less the
    /// resource, which [`Request::stream_alloc`] registers: the kind,
    /// its `stream->mode`, the chunk size, and nothing read or written.
    fn new(ops: Box<dyn Ops>, mode: &[u8]) -> PhpStream {
        PhpStream {
            ops,
            wrapper: "",
            mode: Mode::of(mode),
            orig_path: None,
            position: 0,
            eof: false,
            fatal_error: false,
            flags: Flags::default(),
            readbuf: ReadBuf::default(),
            readpos: 0,
            writepos: 0,
            chunk_size: u32::try_from(CHUNK_SIZE).unwrap_or(u32::MAX),
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
        self.orig_path.as_ref().map(OrigPath::bytes)
    }

    /// `stream->orig_path = pestrdup(path, 0)` over the one it held,
    /// which is given back first.
    pub(crate) fn set_orig_path(&mut self, req: &mut Request, path: &[u8]) -> Result<(), Thrown> {
        if let Some(old) = self.orig_path.take() {
            old.free(req);
        }
        self.orig_path = Some(OrigPath::request(req, path)?);
        Ok(())
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
    pub(crate) fn set_wrapper_data(&mut self, value: Option<Owner<Value>>) -> Option<Owner<Value>> {
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

    /// `PHP_STREAM_CONTEXT(stream)`, whose notifier the stream tells.
    fn context(&self) -> Option<StreamCtx> {
        self.wrapped.as_ref()?.context.as_deref().copied()
    }

    /// `stream->position = 0` and `strlcpy(stream->mode, mode)`, cut to
    /// the fifteen bytes `char mode[16]` holds.
    pub(crate) fn rewind_as(&mut self, mode: &[u8]) {
        self.position = 0;
        self.mode = Mode::of(mode);
    }

    /// `php_stream_notify_progress_increment(PHP_STREAM_CONTEXT(stream),
    /// n, 0)` for what a socket's read or write took; a throwable the
    /// callback leaves is kept for the native.
    fn count_progress(&mut self, req: &mut Request, n: usize) {
        if n == 0 || !self.ops.counts_progress() {
            return;
        }
        let increment = Notification::ProgressIncrement { dsofar: n, dmax: 0 };
        if let Some(context) = self.context()
            && let Err(thrown) = context.notify(req, increment)
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
        if let Some(context) = self.context()
            && let Err(thrown) = context.completed(req)
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
        if !self.readbuf.is_empty() && self.readbuf.len() - self.writepos < self.chunk_size() {
            self.readbuf.copy_within(self.readpos..self.writepos, 0);
            self.writepos -= self.readpos;
            self.readpos = 0;
        }
        if self.readbuf.len() - self.writepos < self.chunk_size() {
            let len = self.readbuf.len() + self.chunk_size();
            if self
                .readbuf
                .resize(req, len, self.flags.persistent)
                .is_err()
            {
                return false;
            }
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
            let direct = self.flags.no_buffer || self.chunk_size() == 1;
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
            let text = format!("{}(): Stream is not writable", req.active_function_name());
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
            self.chunk_size().max(1)
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
        let text = format!(
            "{}(): Stream does not support seeking",
            req.active_function_name()
        );
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
            let to_read_now = (maxlen - buffered_len).min(self.chunk_size());
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
                    None => self.chunk_size(),
                    Some(_) => (left - 1).min(self.chunk_size()),
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

    /// `php_zend_stream_fsizer` (`Z/main/main.c:1661-1675`): the size
    /// `php_stream_stat` gives a stream with no read filter, which a user
    /// wrapper serves with `stream_stat`, and zero for one with a filter
    /// or no size, since what a filter gives need not be the file's size.
    pub(crate) fn zend_fsize(&mut self, req: &mut Request) -> usize {
        if !self.readfilters.is_empty() {
            return 0;
        }
        self.stat(req)
            .map_or(0, |sb| usize::try_from(sb[7]).unwrap_or(0))
    }

    /// `_php_stream_copy_to_mem` (`Z/main/streams/streams.c:1569`): up
    /// to `maxlen` bytes, `None` for all of them, read into the string
    /// php allocates first, so `memory_get_peak_usage()` sees php's
    /// sizes. A bound under four chunks reads into a string of that size
    /// until it is full or the stream is at eof, cut down only where
    /// less than half of it was read; anything else reads into a string
    /// sized from `php_stream_stat`'s size past the position and a chunk
    /// over, extended a chunk at a time while a quarter chunk of room is
    /// left, until a read gives nothing, and cut to what was read. `None`
    /// is php's `NULL`, nothing read, which the string is freed for; the
    /// `Err` is the memory limit's fatal.
    ///
    /// ```php
    /// $p = __DIR__ . '/big.txt'; $w = fopen($p, 'w'); fwrite($w, str_repeat('x', 442 * 1024)); fclose($w);
    /// $s = file_get_contents($p); unset($s);
    /// memory_reset_peak_usage(); $m = memory_get_usage(); $s = file_get_contents($p);
    /// echo memory_get_peak_usage() - $m - filesize($p), ' ', memory_get_usage() - $m - filesize($p), "\n";
    /// $f = fopen($p, 'r'); $t = stream_get_contents($f, 40000); fclose($f); echo strlen($t), "\n";
    /// unlink($p);
    /// ```
    /// ```expect
    /// 10776 2080
    /// 40000
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn copy_to_mem(
        &mut self,
        req: &mut Request,
        maxlen: Option<usize>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        if maxlen == Some(0) {
            return Ok(None);
        }
        if let Some(max) = maxlen
            && max < 4 * CHUNK_SIZE
        {
            let mut out = FreshString::new(req, max)?;
            let mut len = 0;
            while len < max && !self.eof(req) {
                match self.read(req, &mut out.bytes_mut()[len..]) {
                    Some(n) if n > 0 => len += n,
                    _ => break,
                }
            }
            if len == 0 {
                out.free(req);
                return Ok(None);
            }
            out.set_len(len);
            // "Only truncate if the savings are large enough".
            if len < max / 2 {
                out = out.truncate(req, len)?;
            }
            return Ok(Some(out.finish()));
        }
        let size = self.stat(req).map_or(0, |sb| sb[7]);
        let mut buflen = if size > 0 {
            usize::try_from((size - self.position).max(0)).unwrap_or(0) + CHUNK_SIZE
        } else {
            CHUNK_SIZE
        };
        if let Some(max) = maxlen {
            buflen = buflen.min(max);
        }
        let mut out = FreshString::new(req, buflen)?;
        let min_room = CHUNK_SIZE / 4;
        let mut len = 0;
        while let Some(n) = self.read(req, &mut out.bytes_mut()[len..buflen]) {
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
                out = out.extend(req, buflen)?;
            }
        }
        if len == 0 {
            out.free(req);
            return Ok(None);
        }
        Ok(Some(out.truncate(req, len)?.finish()))
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
                let size = u32::try_from(size).unwrap_or(u32::MAX);
                OptionAnswer::Previous(std::mem::replace(&mut self.chunk_size, size) as usize)
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

    /// `_php_stream_cast` (`Z/main/streams/cast.c:194`) to a fd, with a
    /// `ret` to fill.
    pub(crate) fn cast(&mut self, req: &mut Request, cast: Cast, warn: bool) -> Option<RawFd> {
        self.cast_as(req, cast, Ret::Given, warn)
    }

    /// A kind that seeks sought to the stream's position and the read
    /// buffer dropped, as `_php_stream_cast` does before it hands a fd
    /// (`Z/main/streams/cast.c:200-208`).
    fn seek_to_position(&mut self, req: &mut Request) {
        if self.ops.seekable() && !self.flags.no_seek {
            let Self { ops, position, .. } = self;
            let mut dummy = 0;
            let _ = ops.seek(req, *position, Some(Whence::Set), &mut dummy);
            self.readpos = 0;
            self.writepos = 0;
        }
    }

    /// The fd [`Stream::dup_fd`] copies: the stream flushed, and sought
    /// to its position with the read buffer dropped only where the buffer
    /// holds bytes, the guard `_php_stream_write_buffer` puts before a
    /// write (`Z/main/streams/streams.c:1197`), taken here once since the
    /// buffer does not outlive the stream; the kind's fd as a cast with
    /// no `ret` answers it.
    fn keep_fd(&mut self, req: &mut Request) -> Option<RawFd> {
        self.flush(req);
        if self.readpos != self.writepos {
            self.seek_to_position(req);
        }
        self.cast_as(req, Cast::Fd, Ret::Null, false)
    }

    /// `php_stream_can_cast`, `_php_stream_cast` with no `ret` and no
    /// `show_err`: whether the kind would answer the cast, asked of it
    /// without flushing, seeking or converting the stream.
    pub(crate) fn can_cast(&mut self, req: &mut Request, cast: Cast) -> bool {
        self.cast_as(req, cast, Ret::Null, false).is_some()
    }

    /// `_php_stream_cast` (`Z/main/streams/cast.c:194`) to a fd: with a
    /// `ret` and short of `PHP_STREAM_AS_FD_FOR_SELECT`, the stream is
    /// flushed and a kind that seeks is sought to the position, the read
    /// buffer dropped; the kind is asked for its fd, and one with none is
    /// php's `Cannot represent a stream of type %s as a %s` under `warn`;
    /// a cast without `PHP_STREAM_CAST_INTERNAL` that leaves bytes in the
    /// read buffer is php's warning that they are lost. Asked with no
    /// `ret`, it answers the fd too where the kind has one, and `Some` is
    /// php's `SUCCESS`.
    fn cast_as(&mut self, req: &mut Request, cast: Cast, ret: Ret, warn: bool) -> Option<RawFd> {
        if ret == Ret::Given && cast != Cast::Select {
            self.flush(req);
            self.seek_to_position(req);
        }
        // `php_openssl_sockop_cast`'s `PHP_STREAM_AS_FD_FOR_SELECT`: what
        // the crypto layer holds decrypted moved into the empty read
        // buffer, so `select(2)` is not asked to wait for bytes already had.
        if ret == Ret::Given && cast == Cast::Select && self.writepos == self.readpos {
            let pending = self
                .ops
                .as_socket()
                .map_or(0, |socket| socket.crypto_pending());
            if pending > 0 {
                let chunk = self.chunk_size();
                self.fill_read_buffer(req, pending.min(chunk));
            }
        }
        if !self.readfilters.is_empty() || !self.writefilters.is_empty() {
            if warn {
                let text = format!(
                    "{}(): Cannot cast a filtered stream on this system",
                    req.active_function_name()
                );
                warning(req, &text);
            }
            return None;
        }
        let own = self.res.map(Stream::of);
        let fd = match self.ops.cast_target(req, cast, own, ret) {
            CastTarget::Fd(fd) => Some(fd.as_raw_fd()),
            CastTarget::Host(standard) => req.host_fd(standard).map(|fd| fd.as_raw_fd()),
            // `php_stream_cast(intstream, castas, retptr, 1)`: the stream
            // in this one's place warns of itself, and this one of itself
            // after it where that failed.
            CastTarget::Stream(inner) => req
                .stream_op(inner, |req, s| s.cast_as(req, cast, ret, true))
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
                    req.active_function_name(),
                    self.ops.label()
                );
                warning(req, &text);
            }
            return None;
        };
        let held = self.writepos - self.readpos;
        if ret == Ret::Given && held > 0 && matches!(cast, Cast::Fd | Cast::Socket) {
            let text = format!(
                "{}(): {held} bytes of buffered data lost during stream conversion!",
                req.active_function_name()
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
        let mut context = None;
        if let Some(wrapped) = self.wrapped.take() {
            let Wrapped {
                context: kept,
                closer,
                wrapperdata,
                ..
            } = *wrapped;
            context = kept;
            if let Some(closer) = closer
                && let Err(thrown) = closer.close(req)
            {
                filters::defer(req, thrown);
            }
            if let Some(data) = wrapperdata {
                data.give_up(req);
            }
        }
        // `pefree(stream->readbuf)` and `pefree(stream->orig_path)`.
        self.readbuf.free(req);
        if let Some(path) = self.orig_path.take() {
            path.free(req);
        }
        // `zend_list_delete(context->res)`, once everything else is gone.
        if let Some(context) = context {
            context.give_up(req);
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
        let taken = res
            .native_mut::<Option<PhpStream>>(req)
            .and_then(Option::take);
        res.close(req);
        if let Some(taken) = taken {
            req.streams().pclose_ret = taken.release(req);
        }
        // `PHP_STREAM_FREE_CLOSE_PERSISTENT`: a persistent stream closed
        // is gone from the list for every holder.
        req.drop_persistent(res);
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
    data: MemBytes,
    fpos: usize,
    mode: TempMode,
    label: &'static str,
    /// The `php_stream_memory_data` php `emalloc`s, [`MEMORY_DATA_SIZE`]
    /// bytes on the request's heap from [`Ops::lay_down`] to the close;
    /// none for `php://input` and a host's standard input, which php
    /// reads through kinds of their own.
    own: Option<Emalloc>,
}

/// `sizeof(php_stream_memory_data)` on the pinned 64-bit build: `data`,
/// `fpos` and `mode` (`Z/main/streams/memory.c:37-41`).
const MEMORY_DATA_SIZE: usize = 24;

/// `ms->data`, a memory stream's bytes as php holds them.
enum MemBytes {
    /// `ZSTR_EMPTY_ALLOC()`: nothing written, nothing allocated.
    Empty,
    /// A string of the request's as long as the bytes,
    /// `zend_string_realloc`ed by a write past its end and
    /// `zend_string_truncate`d by a shorter `ftruncate`.
    Own(crate::api::values::FreshString),
    /// What the host handed the request, `php://input`'s body and a
    /// host's standard input, which php keeps in streams of their own
    /// and never in a memory stream's data.
    Host(Vec<u8>),
}

impl MemBytes {
    fn bytes(&self) -> &[u8] {
        match self {
            MemBytes::Empty => &[],
            MemBytes::Own(s) => s.bytes(),
            MemBytes::Host(bytes) => bytes,
        }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        match self {
            MemBytes::Empty => &mut [],
            MemBytes::Own(s) => s.bytes_mut(),
            MemBytes::Host(bytes) => bytes,
        }
    }

    fn len(&self) -> usize {
        self.bytes().len()
    }

    /// The bytes made `len` long, the new ones zero:
    /// `zend_string_realloc` past the length, which allocates where the
    /// bytes were the empty string, and `zend_string_truncate` under it.
    /// The `Err` is the memory limit's fatal, the bytes the request's
    /// end's then.
    fn resize(&mut self, req: &mut Request, len: usize) -> Result<(), Thrown> {
        let resized = match std::mem::replace(self, MemBytes::Empty) {
            MemBytes::Empty if len == 0 => return Ok(()),
            MemBytes::Empty => FreshString::new(req, len),
            MemBytes::Own(s) if len >= s.len() => s.extend(req, len),
            MemBytes::Own(s) => s.truncate(req, len),
            MemBytes::Host(mut bytes) => {
                bytes.resize(len, 0);
                *self = MemBytes::Host(bytes);
                return Ok(());
            }
        };
        *self = MemBytes::Own(resized?);
        Ok(())
    }

    /// `zend_string_release(ms->data)`.
    fn release(&mut self, req: &mut Request) {
        if let MemBytes::Own(s) = std::mem::replace(self, MemBytes::Empty) {
            s.free(req);
        }
    }
}

impl Ops for Memory {
    fn label(&self) -> &'static str {
        self.label
    }

    /// `_php_stream_memory_create`'s `self = emalloc(sizeof(*self))`;
    /// the bytes are a string of the request's from the first write on.
    ///
    /// ```php
    /// $w = fopen('php://memory', 'w+'); fwrite($w, 'x'); fclose($w); unset($w);
    /// $m = memory_get_usage(); $f = fopen('php://memory', 'w+'); echo memory_get_usage() - $m, "\n";
    /// fwrite($f, str_repeat('x', 1000)); echo memory_get_usage() - $m, "\n";
    /// ftruncate($f, 10); echo memory_get_usage() - $m, "\n";
    /// fclose($f); unset($f); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 296
    /// 1608
    /// 368
    /// 32
    /// ```
    fn lay_down(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if matches!(self.data, MemBytes::Host(_)) {
            return Ok(());
        }
        self.own = Some(Emalloc::new(req, MEMORY_DATA_SIZE)?);
        Ok(())
    }

    /// `php_stream_memory_write` (`Z/main/streams/memory.c:44`): `-1` on
    /// a read-only stream; an append stream writes at the end; a write
    /// past the end reallocates the bytes to reach it and zero-fills the
    /// gap a seek left. A reallocation past the memory limit is the
    /// heap's fatal, kept for the native.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        if self.mode == TempMode::ReadOnly {
            return None;
        }
        if self.mode == TempMode::Append {
            self.fpos = self.data.len();
        }
        if self.fpos + buf.len() > self.data.len()
            && let Err(thrown) = self.data.resize(req, self.fpos + buf.len())
        {
            if req.stream_thrown().is_none() {
                *req.stream_thrown() = Some(thrown);
            }
            return None;
        }
        self.data.bytes_mut()[self.fpos..self.fpos + buf.len()].copy_from_slice(buf);
        self.fpos += buf.len();
        Some(buf.len())
    }

    /// `php_stream_memory_read` (`Z/main/streams/memory.c:75`): eof and
    /// nothing at or past the end, else up to what is left.
    fn read(&mut self, _req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let data = self.data.bytes();
        if self.fpos >= data.len() {
            *eof = true;
            return Some(0);
        }
        let count = buf.len().min(data.len() - self.fpos);
        buf[..count].copy_from_slice(&data[self.fpos..self.fpos + count]);
        self.fpos += count;
        Some(count)
    }

    /// `php_stream_memory_close` (`Z/main/streams/memory.c:97`): the
    /// bytes released, then the struct `efree`d.
    fn close(&mut self, req: &mut Request) -> i32 {
        self.data.release(req);
        if let MemBytes::Host(_) = self.data {
            self.data = MemBytes::Empty;
        }
        if let Some(own) = self.own.take() {
            own.free(req);
        }
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
    /// a shorter size cuts the bytes with `zend_string_truncate` and
    /// pulls the position back to it, a longer one
    /// `zend_string_realloc`s and zero-fills, so a size past the memory
    /// limit is the heap's fatal for that string, kept for the native.
    fn truncate(&mut self, req: &mut Request, size: usize) -> Option<bool> {
        if self.mode == TempMode::ReadOnly {
            return Some(false);
        }
        if let Err(thrown) = self.data.resize(req, size) {
            if req.stream_thrown().is_none() {
                *req.stream_thrown() = Some(thrown);
            }
            return Some(false);
        }
        self.fpos = self.fpos.min(size);
        Some(true)
    }

    fn memory_buffer(&self) -> Option<&[u8]> {
        (self.label == "MEMORY").then_some(self.data.bytes())
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
            data: MemBytes::Empty,
            fpos: 0,
            mode,
            own: None,
            label: "MEMORY",
        }),
        mode.as_mode(),
    );
    stream.flags.no_buffer = true;
    stream
}

/// `sizeof(php_stream_temp_data)` on the pinned 64-bit build:
/// `innerstream`, `smax`, `mode` and its padding, the `meta` zval and
/// `tmpdir` (`Z/main/streams/memory.c:325-331`).
const TEMP_DATA_SIZE: usize = 48;

/// `PHP_STREAM_MAX_MEM` (`Z/main/php_memory_streams.h:27`): what a
/// `php://temp` holds in memory before it spills to a file.
const PHP_STREAM_MAX_MEM: usize = 2 * 1024 * 1024;

/// `php_stream_temp_data` under `php_stream_temp_ops`
/// (`Z/main/streams/memory.c:337`): the stream it encloses, its own
/// resource as php's is, so a `php://temp` takes two ids as the oracle's
/// does, `None` where php's `innerstream` is `NULL`; and `smax`, the
/// bytes it keeps in memory before the spill.
struct Temp {
    inner: Option<Owner<Stream>>,
    /// The `php_stream_temp_data` php `ecalloc`s, [`TEMP_DATA_SIZE`]
    /// bytes on the request's heap from [`Ops::lay_down`] to the close.
    own: Option<Emalloc>,
    smax: usize,
    /// `stream->ops->label`: `TEMP`, or `RFC2397` for what the `data:`
    /// wrapper opened, which swaps in `php_stream_rfc2397_ops`.
    label: &'static str,
    /// `ts->meta`, what the `data:` wrapper read from its URL, and nothing
    /// for `php://temp`.
    meta: Vec<(Vec<u8>, MetaValue)>,
}

impl Temp {
    /// The inner stream, a view of the owner the kind keeps.
    fn inner(&self) -> Option<Stream> {
        self.inner.as_deref().copied()
    }

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
                req.active_function_name()
            );
            warning(req, &text);
            return Ok(false);
        };
        let at = *file;
        req.stream_op(at, |req, f| f.write(req, &membuf));
        if let Some(memory) = self.inner.replace(file) {
            memory.free(req);
        }
        req.stream_op(at, |req, f| f.seek(req, pos, Some(Whence::Set)));
        Ok(true)
    }
}

impl Ops for Temp {
    fn label(&self) -> &'static str {
        self.label
    }

    fn enclose(&mut self, inner: Owner<Stream>) -> Option<Owner<Stream>> {
        self.inner = Some(inner);
        None
    }

    /// `_php_stream_temp_create_ex`'s `self = ecalloc(1, sizeof(*self))`,
    /// beside the memory stream it encloses.
    ///
    /// ```php
    /// $w = fopen('php://temp', 'w+'); fwrite($w, 'x'); fclose($w); unset($w);
    /// $m = memory_get_usage(); $f = fopen('php://temp', 'w+'); echo memory_get_usage() - $m, "\n";
    /// fwrite($f, str_repeat('x', 1000)); echo memory_get_usage() - $m, "\n";
    /// fclose($f); unset($f); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 760
    /// 2072
    /// 192
    /// ```
    fn lay_down(&mut self, req: &mut Request) -> Result<(), Thrown> {
        self.own = Some(Emalloc::new(req, TEMP_DATA_SIZE)?);
        Ok(())
    }

    /// `php_stream_temp_write` (`Z/main/streams/memory.c:350`): the spill
    /// where the write would reach `smax`, then the inner stream
    /// written; a spill that could not make its file writes nothing.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let inner = self.inner()?;
        match self.spill(req, inner, buf.len()) {
            Ok(true) => {}
            Ok(false) => return Some(0),
            Err(_) => return None,
        }
        req.stream_op(self.inner()?, |req, inner| inner.write(req, buf))
            .flatten()
    }

    /// `php_stream_temp_read` (`Z/main/streams/memory.c:381`): the inner
    /// stream read and its eof taken.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let (got, inner_eof) = req.stream_op(self.inner()?, |req, inner| {
            (inner.read(req, buf), inner.eof)
        })?;
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
        if let Some(own) = self.own.take() {
            own.free(req);
        }
        0
    }

    /// `php_stream_temp_flush` (`Z/main/streams/memory.c:418`).
    fn flush(&mut self, req: &mut Request) -> bool {
        self.inner()
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
        let Some(inner) = self.inner() else {
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
        req.stream_op(self.inner()?, |req, inner| inner.stat(req))
            .flatten()
    }

    /// `php_stream_temp_set_option`'s default arm: the inner stream's.
    fn truncate(&mut self, req: &mut Request, size: usize) -> Option<bool> {
        req.stream_op(self.inner()?, |req, inner| inner.truncate(req, size))
            .flatten()
    }

    /// `php_stream_temp_set_option`'s default arm: the inner stream's.
    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        req.stream_op(self.inner()?, |req, inner| inner.ops.flock(req, operation))
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
    /// temporary one first where it is still in memory; asked with no
    /// `ret`, one still in memory is refused, nothing moved.
    fn cast_target(
        &mut self,
        req: &mut Request,
        _cast: Cast,
        _own: Option<Stream>,
        ret: Ret,
    ) -> CastTarget<'_> {
        let Some(inner) = self.inner() else {
            return CastTarget::Refused;
        };
        if ret == Ret::Null {
            let in_memory = req
                .stream_ref(inner)
                .is_some_and(|s| s.ops.memory_buffer().is_some());
            return if in_memory {
                CastTarget::Refused
            } else {
                CastTarget::Stream(inner)
            };
        }
        if !matches!(self.move_to_file(req, inner, None), Ok(true)) {
            return CastTarget::Refused;
        }
        self.inner().map_or(CastTarget::Refused, CastTarget::Stream)
    }

    /// `php_stream_temp_set_option`'s default arm: the inner stream's,
    /// its generic layer included.
    fn set_option(&mut self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        let Some(inner) = self.inner() else {
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
) -> Result<Option<Owner<Stream>>, Thrown> {
    let Some((file, path)) = req.open_temporary_fd(dir, prefix, TemporaryGuard::Never) else {
        return Ok(None);
    };
    let orig_path = path.as_os_str().as_encoded_bytes().to_vec();
    let mut stream = PhpStream::new(
        Box::new(Stdio {
            data: None,
            file: Some(file),
            is_seekable: true,
            temp_name: Some(path),
            process: None,
            blocking: true,
            suppress_errors: false,
        }),
        b"r+b",
    );
    stream.wrapper = PLAIN_FILES;
    let stream = req.stream_alloc(stream)?;
    Ok(Some(orig_path_set(req, stream, &orig_path)?))
}

/// `stream->orig_path` set on a stream an open has just made, the stream
/// freed where that threw.
pub(crate) fn orig_path_set(
    req: &mut Request,
    stream: Owner<Stream>,
    path: &[u8],
) -> Result<Owner<Stream>, Thrown> {
    let named = req.stream_op(*stream, |req, s| s.set_orig_path(req, path));
    Ok(freed_on_throw(req, stream, named.transpose())?.0)
}

/// The stream an open has just made beside what the open did after it,
/// `done`, or the stream freed where that threw, so no owner is left
/// behind.
pub(crate) fn freed_on_throw<T>(
    req: &mut Request,
    stream: Owner<Stream>,
    done: Result<T, Thrown>,
) -> Result<(Owner<Stream>, T), Thrown> {
    match done {
        Ok(done) => Ok((stream, done)),
        Err(thrown) => {
            stream.free(req);
            Err(thrown)
        }
    }
}

// -------------------------------------------------------- plain files

/// `php_plain_files_wrapper.wops->label`.
const PLAIN_FILES: &str = "plainfile";

/// `php_stdio_stream_data` under `php_stream_stdio_ops`
/// (`Z/main/streams/plain_wrapper.c:135`): the file, `None` once closed;
/// whether it seeks; `temp_name`, the path of a temporary file to unlink
/// at close; and for a pipe `popen` opened, `is_process_pipe`, the child
/// at its other end, whom the close waits for as `pclose(3)` does.
struct Stdio {
    /// The `php_stdio_stream_data` php `pemalloc`s, [`STDIO_DATA_SIZE`]
    /// bytes on the request's heap from [`Ops::lay_down`] to the close,
    /// standing for the struct whose fields are these.
    data: Option<Emalloc>,
    file: Option<std::fs::File>,
    is_seekable: bool,
    temp_name: Option<std::path::PathBuf>,
    process: Option<std::process::Child>,
    /// Whether the fd blocks, what `stream_set_blocking` last set.
    blocking: bool,
    /// `PHP_STREAM_FLAG_SUPPRESS_ERRORS`: a failed read or write raises
    /// no notice.
    suppress_errors: bool,
}

/// `sizeof(php_stdio_stream_data)` (`Z/main/streams/plain_wrapper.c:135`),
/// what `_php_stream_fopen_from_fd_int` `pemalloc`s for a plain file.
const STDIO_DATA_SIZE: usize = 200;

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

    /// `_php_stream_fopen_from_fd_int`'s `self = pemalloc(sizeof(*self))`.
    fn lay_down(&mut self, req: &mut Request) -> Result<(), Thrown> {
        self.data = Some(Emalloc::new(req, STDIO_DATA_SIZE)?);
        Ok(())
    }

    /// `php_stdiop_write` (`Z/main/streams/plain_wrapper.c:374`): one
    /// `write(2)`, and php's notice with `errno` where it failed, unless
    /// the stream was marked [`StreamOption::SuppressErrors`]; the
    /// stat cache cleared after it, since the file's times moved.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let written = self.file.as_mut()?.write(buf);
        req.forget_stats();
        match written {
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(0),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => None,
            Err(_) if self.suppress_errors => None,
            Err(error) => {
                let text = format!(
                    "{}(): Write of {} bytes failed with {}",
                    req.active_function_name(),
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
    /// it failed, unless the stream was marked
    /// [`StreamOption::SuppressErrors`], eof set then too unless the fd
    /// was bad, `EBADF`; the
    /// stat cache cleared after it, since the file's atime moved.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let file = self.file.as_mut()?;
        let read = req.blocking(|| file.read(buf));
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
                if !self.suppress_errors {
                    let text = format!(
                        "{}(): Read of {} bytes failed with {}",
                        req.active_function_name(),
                        buf.len(),
                        errno(&error)
                    );
                    raise(req, Level::Notice, &text);
                }
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
    fn close(&mut self, req: &mut Request) -> i32 {
        drop(self.file.take());
        // `pefree(data)`, the struct given back as the close ends.
        if let Some(data) = self.data.take() {
            data.free(req);
        }
        if let Some(mut child) = self.process.take() {
            use std::os::unix::process::ExitStatusExt;
            // The wait lends the request ([`Request::blocking`]), as
            // every wait on a child is (#2401).
            return match req.blocking(|| child.wait()) {
                Ok(status) => status.code().unwrap_or_else(|| status.into_raw()),
                Err(_) => -1,
            };
        }
        if let Some(path) = self.temp_name.take() {
            std::fs::remove_file(path).ok();
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
            let text = format!(
                "{}(): Cannot seek on this stream",
                req.active_function_name()
            );
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
    /// `LOCK_EX` or `LOCK_UN`, without blocking under `LOCK_NB`; a
    /// `flock(2)` that waits for another holder lends the request while
    /// it waits ([`Request::blocking`], #2401).
    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        let nonblocking = operation & LOCK_NB != 0;
        let file = self.file.as_ref()?;
        let taken = match operation & !LOCK_NB {
            LOCK_SH if nonblocking => file.try_lock_shared().is_ok(),
            LOCK_SH => req.blocking(|| file.lock_shared()).is_ok(),
            LOCK_EX if nonblocking => file.try_lock().is_ok(),
            LOCK_EX => req.blocking(|| file.lock()).is_ok(),
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
                if crate::api::layer::set_fd_blocking(file.as_fd(), blocking).is_err() {
                    return OptionAnswer::Err;
                }
                self.blocking = blocking;
                OptionAnswer::Ok
            }
            StreamOption::Sync { data_only } => match self.file.as_ref() {
                Some(file) if crate::api::layer::sync_fd(file.as_fd(), data_only).is_ok() => {
                    OptionAnswer::Ok
                }
                _ => OptionAnswer::Err,
            },
            StreamOption::WriteBuffer { .. } => OptionAnswer::Err,
            StreamOption::SuppressErrors(suppress) => {
                self.suppress_errors = suppress;
                OptionAnswer::Ok
            }
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
        _ret: Ret,
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
///
/// The fd is a process's the request execs, as php's `open(2)` with no
/// `O_CLOEXEC` leaves it, unless `$mode` carries `e`; an include's open
/// keeps `O_CLOEXEC`, since php closes the file once it has compiled it
/// (`zend_destroy_file_handle`) and nothing runs between:
///
/// ```php
/// $path = __DIR__ . '/held.inc'; file_put_contents($path, '<?php ');
/// $f = fopen($path, 'r'); $e = fopen($path, 're'); $c = fopen($path, 'r'); fclose($c); include $path;
/// preg_match_all('/^ *' . fileinode($path) . ' /m', shell_exec('ls -iLd /dev/fd/* 2>/dev/null'), $held);
/// unlink($path);
/// var_dump(count($held[0]));
/// ```
/// ```expect
/// int(1)
/// ```
///
/// The stream costs the request what php's does: the
/// `php_stdio_stream_data`, the `php_stream` and its resource, and the
/// path `estrdup`'d as `orig_path` at the open; the read buffer of one
/// chunk at the first read; all of it but the resource given back at
/// the close.
///
/// ```php
/// chdir(__DIR__); file_put_contents('plain.txt', str_repeat('x', 100));
/// $g = fopen('plain.txt', 'rb'); fread($g, 10); fclose($g);
/// $m = memory_get_usage(); $f = fopen('plain.txt', 'rb'); $a = memory_get_usage() - $m;
/// $m = memory_get_usage(); $s = fread($f, 10); $b = memory_get_usage() - $m;
/// $m = memory_get_usage(); fclose($f); $c = memory_get_usage() - $m;
/// unlink('plain.txt');
/// var_dump($a, $b, $c);
/// ```
/// ```expect
/// int(496)
/// int(8232)
/// int(-8656)
/// ```
fn plain_open(
    req: &mut Request,
    path: &HostPath,
    mode: &[u8],
    for_include: bool,
) -> Result<PhpStream, String> {
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
    if !for_include && !mode.contains(&b'e') {
        crate::api::layer::inherit_fd(file.as_fd()).map_err(|error| strerror(&error))?;
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
            data: None,
            file: Some(file),
            is_seekable,
            temp_name: None,
            process: None,
            blocking: true,
            suppress_errors: false,
        }),
        mode,
    );
    stream.flags.no_seek = !is_seekable;
    stream.position = position;
    Ok(stream)
}

// --------------------------------------------------------- php://

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
///
/// What php keeps of the stream on the request's heap is the kind's data
/// over the fd it dupped, a `php_stdio_stream_data`, or a
/// `php_netstream_data_t` where the fd is a socket, which `do_cli`'s
/// three opens lay down before a program's first line
/// ([`HostFd::data`]).
struct HostFd {
    end: HostEnd,
    /// The kind's data php `pemalloc`s over the fd, on the request's heap
    /// from [`Ops::lay_down`] to the close, standing for the struct.
    data: Option<Emalloc>,
}

/// Which of the host's three a [`HostFd`] is.
enum HostEnd {
    /// fd 0 under a host that handed none, over the body it reads.
    Stdin(Memory),
    /// fd 1 and fd 2, each with the label php's kind over the host's fd
    /// has: `tcp_socket` over a socket, as `php_stream_url_wrap_php`
    /// swaps the ops, and `STDIO` otherwise.
    Stdout(&'static str),
    Stderr(&'static str),
}

impl HostFd {
    fn new(end: HostEnd) -> HostFd {
        HostFd { end, data: None }
    }

    fn standard(&self) -> Standard {
        match self.end {
            HostEnd::Stdin(_) => Standard::Stdin,
            HostEnd::Stdout(_) => Standard::Stdout,
            HostEnd::Stderr(_) => Standard::Stderr,
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
        match self.end {
            HostEnd::Stdin(_) => "STDIO",
            HostEnd::Stdout(label) | HostEnd::Stderr(label) => label,
        }
    }

    /// `_php_stream_fopen_from_fd_int`'s `php_stdio_stream_data`, or
    /// `php_stream_sock_open_from_socket`'s `php_netstream_data_t` over a
    /// socket.
    fn lay_down(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let size = if self.label() == "tcp_socket" {
            sockets::NETSTREAM_DATA_SIZE
        } else {
            STDIO_DATA_SIZE
        };
        self.data = Some(Emalloc::new(req, size)?);
        Ok(())
    }

    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        match &mut self.end {
            HostEnd::Stdin(body) => body.write(req, buf),
            HostEnd::Stdout(_) => {
                req.write_stdout(buf);
                Some(buf.len())
            }
            HostEnd::Stderr(_) => {
                req.write_stderr(buf);
                Some(buf.len())
            }
        }
    }

    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        if let HostEnd::Stdin(body) = &mut self.end {
            return body.read(req, buf, eof);
        }
        let text = format!(
            "{}(): Read of {} bytes failed with errno={EBADF} Bad file descriptor",
            req.active_function_name(),
            buf.len()
        );
        raise(req, Level::Notice, &text);
        None
    }

    fn close(&mut self, req: &mut Request) -> i32 {
        if let Some(data) = self.data.take() {
            data.free(req);
        }
        match &mut self.end {
            HostEnd::Stdin(body) => body.close(req),
            HostEnd::Stdout(_) | HostEnd::Stderr(_) => 0,
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
            Some(fd) if crate::api::layer::sync_fd(fd, data_only).is_ok() => OptionAnswer::Ok,
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
        _ret: Ret,
    ) -> CastTarget<'_> {
        if cast == Cast::Socket {
            return CastTarget::Refused;
        }
        CastTarget::Host(self.standard())
    }
}

/// [`StreamHooks::standard_handles`](crate::api::layer::StreamHooks::standard_handles):
/// `cli_register_file_handles`' three opens, `php://stdin` `rb`,
/// `php://stdout` and `php://stderr` `wb`, each stream's one owner
/// handed to the constant the engine registers; the others freed where
/// one does not open. `stdin_replaced` forgets that `STDIN` was closed,
/// since the host's fd 0 under it is new.
pub(crate) fn standard_handles(
    req: &mut Request,
    stdin_replaced: bool,
) -> Result<Option<[Owner<Value>; 3]>, Thrown> {
    if stdin_replaced {
        req.streams().standard_closed[0] = false;
    }
    let free = |req: &mut Request, opened: [Option<Owner<Stream>>; 3]| {
        opened
            .into_iter()
            .flatten()
            .for_each(|stream| stream.free(req));
    };
    let mut opened = [None, None, None];
    for (at, (path, mode)) in [
        (&b"php://stdin"[..], &b"rb"[..]),
        (b"php://stdout", b"wb"),
        (b"php://stderr", b"wb"),
    ]
    .into_iter()
    .enumerate()
    {
        match req.open_stream_wrapper(path, mode, false) {
            Ok(stream) => opened[at] = stream,
            Err(thrown) => {
                free(req, opened);
                return Err(thrown);
            }
        }
    }
    let [stdin, stdout, stderr] = match opened {
        [Some(stdin), Some(stdout), Some(stderr)] => [stdin, stdout, stderr],
        opened => {
            free(req, opened);
            return Ok(None);
        }
    };
    req.streams().standard_streams = [
        Some(stdin.resource()),
        Some(stdout.resource()),
        Some(stderr.resource()),
    ];
    Ok(Some(
        [stdin, stdout, stderr].map(|stream| stream.into_resource().into_value()),
    ))
}

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
    /// `STREAM_OPEN_FOR_ZEND_STREAM`: the open `php_stream_open_for_zend_ex`
    /// makes for the engine's scanner, whose caller holds the
    /// `*opened_path` the opener sets ([`Request::open_wrapper_named`]).
    pub(crate) for_zend_stream: bool,
    /// The file stands open while PHP runs, as the host's
    /// `zend_file_handle` holds the program's: its fd inherited as php's
    /// `open(2)` leaves it, where an include's, closed once compiled,
    /// keeps `O_CLOEXEC` ([`plain_open`]).
    pub(crate) held: bool,
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
        for_zend_stream: false,
        held: false,
    };

    /// The options a wrapper's opener is handed, with `USE_PATH` as
    /// `use_path` says and the warning bit cleared,
    /// `options & ~REPORT_ERRORS`.
    const fn opening(self, use_path: bool) -> crate::api::request::Opening {
        crate::api::request::Opening {
            use_path,
            warn: false,
            for_include: self.for_include,
            must_seek: self.must_seek,
            will_cast: self.will_cast,
            disable_open_basedir: self.disable_open_basedir,
            only_get_headers: self.only_get_headers,
            for_zend_stream: self.for_zend_stream,
        }
    }

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
    /// `STREAM_OPEN_FOR_INCLUDE` 128, `STREAM_USE_URL` 256,
    /// `STREAM_ONLY_GET_HEADERS` 512 and `STREAM_OPEN_FOR_ZEND_STREAM`
    /// 0x10000.
    const fn bits(self) -> i64 {
        (self.use_path as i64)
            | (self.report as i64) << 3
            | (self.must_seek as i64) << 4
            | (self.will_cast as i64) << 5
            | (self.for_include as i64) << 7
            | (self.use_url as i64) << 8
            | (self.only_get_headers as i64) << 9
            | (self.for_zend_stream as i64) << 16
    }
}

/// The options an extension's [`Opening`](crate::api::request::Opening)
/// names, as the bits php's `options` carries: `REPORT_ERRORS` where it
/// warns, and never `STREAM_USE_URL`.
impl From<crate::api::request::Opening> for OpenOptions {
    fn from(opening: crate::api::request::Opening) -> OpenOptions {
        OpenOptions {
            use_path: opening.use_path,
            use_url: false,
            report: opening.warn,
            for_include: opening.for_include,
            must_seek: opening.must_seek,
            will_cast: opening.will_cast,
            disable_open_basedir: opening.disable_open_basedir,
            only_get_headers: opening.only_get_headers,
            for_zend_stream: opening.for_zend_stream,
            held: false,
        }
    }
}

/// [`Streams::standard_stream`](crate::api::request::Streams::standard_stream):
/// the host's standard stream as `php://stdin`, `php://stdout` and
/// `php://stderr` open it (`Z/ext/standard/php_fopen_wrapper.c:238-290`).
/// Nothing once a program closed the constant over it; under a host
/// that handed its fd 0, `php://stdin` over a copy of it
/// ([`standard_open`]); `php://stdout` and `php://stderr` over copies of
/// the process's fds once the constants stand under php's CLI; and
/// otherwise the host's sink and body, [`HostFd`], which does not seek
/// (a pipe or a terminal under the CLI, which `detect_is_seekable` says
/// does not seek).
pub(crate) fn open_standard(
    req: &mut Request,
    standard: Standard,
    mode: &[u8],
) -> Result<Option<Owner<Stream>>, Thrown> {
    if req.standard_closed(standard) {
        return Ok(None);
    }
    let copied = match standard {
        Standard::Stdin => req.host_fd(Standard::Stdin).is_some(),
        Standard::Stdout | Standard::Stderr => req.copies_standard(),
    };
    if copied {
        return standard_open(req, standard, mode);
    }
    let end = match standard {
        Standard::Stdin => HostEnd::Stdin(Memory {
            data: MemBytes::Host(Vec::new()),
            fpos: 0,
            mode: TempMode::ReadOnly,
            label: "STDIO",
            own: None,
        }),
        Standard::Stdout => HostEnd::Stdout(host_label(req, Standard::Stdout)),
        Standard::Stderr => HostEnd::Stderr(host_label(req, Standard::Stderr)),
    };
    let mut stream = PhpStream::new(Box::new(HostFd::new(end)), mode);
    stream.flags.no_seek = true;
    stream.position = -1;
    Ok(Some(req.stream_alloc(stream)?))
}

/// [`Streams::fd_stream`](crate::api::request::Streams::fd_stream):
/// `php://fd/N` past its form (`Z/ext/standard/php_fopen_wrapper.c:316-337`),
/// `N` below `getdtablesize()` and `dup(2)` of it, the host's own fd for
/// 0, 1 and 2 where it handed them; a socket opens as a socket, anything
/// else as a plain file's kind over the copy. A refusal is logged as
/// `php_stream_wrapper_log_error` logs it.
pub(crate) fn open_fd(
    req: &mut Request,
    fildes: i64,
    mode: &[u8],
) -> Result<Option<Owner<Stream>>, Thrown> {
    let dtablesize = crate::api::layer::fd_table_size();
    if fildes < 0 || fildes >= i64::from(dtablesize) {
        req.streams().wrapper_errors.push(format!(
            "The file descriptors must be non-negative numbers smaller than {dtablesize}"
        ));
        return Ok(None);
    }
    let standard = match fildes {
        0 => Some(Standard::Stdin),
        1 => Some(Standard::Stdout),
        2 => Some(Standard::Stderr),
        _ => None,
    };
    let duped = match standard.and_then(|s| req.host_fd(s)) {
        Some(fd) => fd.try_clone_to_owned(),
        None => crate::api::layer::dup_fd(i32::try_from(fildes).unwrap_or(-1)),
    };
    match duped {
        Ok(fd) => Ok(Some(fd_stream(req, fd, mode)?)),
        Err(error) => {
            req.streams().wrapper_errors.push(format!(
                "Error duping file descriptor {fildes}; possibly it doesn't exist: [{}]: {}",
                error.raw_os_error().unwrap_or(0),
                strerror(&error)
            ));
            Ok(None)
        }
    }
}

/// [`Streams::input_stream`](crate::api::request::Streams::input_stream):
/// `php://input`, `php_stream_input_ops` over the request's body
/// (`Z/ext/standard/php_fopen_wrapper.c:58-146`), read whole, read only,
/// and sought as `php_stream_input_seek` seeks it.
pub(crate) fn open_input(req: &mut Request) -> Result<Owner<Stream>, Thrown> {
    let kind = Memory {
        data: MemBytes::Host(req.input_bytes()),
        fpos: 0,
        mode: TempMode::ReadOnly,
        label: "Input",
        own: None,
    };
    req.stream_alloc(PhpStream::new(Box::new(kind), b"rb"))
}

/// [`Streams::memory_stream`](crate::api::request::Streams::memory_stream):
/// `php_stream_memory_create(php_stream_mode_from_str(mode))`.
pub(crate) fn open_memory(req: &mut Request, mode: &[u8]) -> Result<Owner<Stream>, Thrown> {
    req.stream_alloc(memory(TempMode::of(mode)))
}

/// [`Streams::temp_stream`](crate::api::request::Streams::temp_stream):
/// `php_stream_temp_create(php_stream_mode_from_str(mode), max_memory)`,
/// [`PHP_STREAM_MAX_MEM`] where no bound is named.
pub(crate) fn open_temp(
    req: &mut Request,
    mode: &[u8],
    max_memory: Option<usize>,
) -> Result<Owner<Stream>, Thrown> {
    temp(
        req,
        TempMode::of(mode),
        max_memory.unwrap_or(PHP_STREAM_MAX_MEM),
    )
}
/// The stream a `php://` fd opens as: over a socket, the `tcp_socket` kind
/// php swaps its ops to ([`Request::php_socket_stream`]); over anything
/// else, the plain file's kind.
fn fd_stream(req: &mut Request, fd: OwnedFd, mode: &[u8]) -> Result<Owner<Stream>, Thrown> {
    match fd_kind(&fd, mode) {
        FdStream::Socket => req.php_socket_stream(fd),
        kind => StreamLayer::stream_from_fd(req, fd, kind),
    }
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
fn standard_open(
    req: &mut Request,
    standard: Standard,
    mode: &[u8],
) -> Result<Option<Owner<Stream>>, Thrown> {
    let duped = req.host_fd(standard).map_or_else(
        || Err(std::io::Error::from_raw_os_error(EBADF)),
        |fd| fd.try_clone_to_owned(),
    );
    let fd = match duped {
        Ok(fd) => fd,
        Err(error) => {
            req.streams().wrapper_errors.push(strerror(&error));
            return Ok(None);
        }
    };
    let seeks = standard == Standard::Stdin || matches!(fd_kind(&fd, mode), FdStream::Socket);
    let stream = fd_stream(req, fd, mode)?;
    if !seeks {
        req.stream_op(*stream, |_, opened| {
            opened.flags.no_seek = true;
            opened.position = -1;
        });
    }
    Ok(Some(stream))
}

/// The label of php's kind over the host's fd `standard`: `tcp_socket`
/// where it is a socket, `STDIO` where it is anything else or the host
/// handed none.
fn host_label(req: &Request, standard: Standard) -> &'static str {
    let socket = req
        .host_fd(standard)
        .and_then(|fd| fd.try_clone_to_owned().ok())
        .is_some_and(|fd| matches!(fd_kind(&fd, b""), FdStream::Socket));
    if socket { "tcp_socket" } else { "STDIO" }
}

/// `_php_stream_temp_create_ex` (`Z/main/streams/memory.c:519`): the
/// temp stream registered first, then the memory stream it encloses, so
/// the two ids come out in php's order.
fn temp(req: &mut Request, mode: TempMode, smax: usize) -> Result<Owner<Stream>, Thrown> {
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
) -> Result<Owner<Stream>, Thrown> {
    let kind = Temp {
        inner: None,
        own: None,
        smax,
        label,
        meta,
    };
    let mut outer = PhpStream::new(Box::new(kind), mode.as_mode());
    outer.flags.no_buffer = true;
    let outer = req.stream_alloc(outer)?;
    let inner = req.stream_alloc(memory(mode));
    let (outer, inner) = freed_on_throw(req, outer, inner)?;
    let mut inner = Some(inner);
    req.stream_op(*outer, |_, s| {
        inner = inner.take().and_then(|i| s.ops.enclose(i));
    });
    if let Some(inner) = inner {
        inner.free(req);
    }
    Ok(outer)
}

// ---------------------------------------------------------- wrappers

/// Which wrapper `php_stream_locate_url_wrapper`
/// (`Z/main/streams/streams.c:2010`) found for a path: the plain files
/// wrapper, the `data:` wrapper, one an extension registered (its place
/// in [`Request::wrappers`]), `php://` among them since `standard`
/// registers it, one a program registered with `stream_wrapper_register`,
/// or none.
#[derive(Clone)]
pub(crate) enum Located {
    Plain,
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
            Located::Ext(at) => req
                .streams_ref()
                .and_then(|state| state.wrappers.get(*at))
                .is_some_and(|w| w.is_url),
            Located::Plain | Located::Nothing => false,
        }
    }
}

/// Whether two rows name the same wrapper, as `zend_hash_find_ptr(...) ==
/// wrapper` compares them.
fn same_wrapper(a: &Located, b: &Located) -> bool {
    match (a, b) {
        (Located::Plain, Located::Plain) | (Located::Data, Located::Data) => true,
        (Located::Ext(x), Located::Ext(y)) => x == y,
        _ => false,
    }
}

/// What an open answers: the stream, and the path its opener named,
/// php's `*opened_path`, as a string of the request's.
pub(crate) type OpenedNamed = (Owner<Stream>, Option<Owner<PhpString>>);

pub(crate) trait StreamLayer {
    fn global_wrappers(&self) -> Vec<Registered>;
    fn add_url_wrapper(&mut self, row: Registered) -> bool;
    fn wrapper_table(&self) -> Vec<Registered>;
    fn wrapper_row(table: &[Registered], scheme: &[u8]) -> Option<Located>;
    fn locate_wrapper(&mut self, path: &[u8]) -> (Located, Vec<u8>);
    fn locate_for(&mut self, path: &[u8], options: OpenOptions) -> (Located, Vec<u8>);
    fn locate(&mut self, path: &[u8], options: OpenOptions) -> (Located, Vec<u8>);
    fn own_wrappers(&mut self) -> &mut Vec<Registered>;
    fn open_stream_wrapper(
        &mut self,
        path: &[u8],
        mode: &[u8],
        use_include_path: bool,
    ) -> Result<Option<Owner<Stream>>, Thrown>;
    fn open_wrapper_with(
        &mut self,
        path: &[u8],
        mode: &[u8],
        opening: crate::api::request::Opening,
    ) -> Result<Option<Owner<Stream>>, Thrown>;
    fn open_url_wrapper(
        &mut self,
        path: &[u8],
        mode: &[u8],
    ) -> Result<Option<Owner<Stream>>, Thrown>;
    fn open_wrapper_ex(
        &mut self,
        path: &[u8],
        mode: &[u8],
        options: OpenOptions,
    ) -> Result<Option<Owner<Stream>>, Thrown>;
    fn open_wrapper_named(
        &mut self,
        path: &[u8],
        mode: &[u8],
        options: OpenOptions,
    ) -> Result<Option<OpenedNamed>, Thrown>;
    fn open_located(
        &mut self,
        to_open: &[u8],
        mode: &[u8],
        options: OpenOptions,
        resolved: bool,
    ) -> Result<Option<OpenedNamed>, Thrown>;
    fn plain_opener(
        &mut self,
        path: &[u8],
        mode: &[u8],
        options: OpenOptions,
    ) -> Result<PhpStream, String>;
    fn open_ext(
        &mut self,
        at: usize,
        path: &[u8],
        mode: &[u8],
        use_path: bool,
        options: OpenOptions,
    ) -> Result<(&'static str, Result<Owner<Stream>, String>), Thrown>;
    fn learn_append_position(&mut self, stream: Stream);
    fn make_seekable(
        &mut self,
        stream: Owner<Stream>,
        path: &[u8],
        options: OpenOptions,
    ) -> Result<Option<Owner<Stream>>, Thrown>;
    fn copy_all(&mut self, from: Stream, to: Stream) -> bool;
    fn open_dir_wrapper(&mut self, path: &[u8]) -> Result<Option<Owner<Stream>>, Thrown>;
    fn stream_meta_data(&mut self, stream: Stream) -> Result<Owner<PhpArray>, Thrown>;
    fn set_stream_context(&mut self, stream: Stream, context: Option<StreamCtx>);
    fn stream_context(&self, stream: Stream) -> Option<StreamCtx>;
    fn crypto_meta(&mut self, stream: Stream) -> Result<Option<Owner<Value>>, Thrown>;
    fn kind_meta_data(&self, stream: Stream) -> Option<Vec<(Vec<u8>, MetaValue)>>;
    fn wrapper_errors_text(&self, logged: &[String]) -> String;
    fn display_wrapper_errors(&mut self, path: &[u8], caption: &str, message: &str);
    fn stream_alloc(&mut self, stream: PhpStream) -> Result<Owner<Stream>, Thrown>;
    fn stream_alloc_as(
        &mut self,
        stream: PhpStream,
        persistent: bool,
    ) -> Result<Owner<Stream>, Thrown>;
    fn stream_op<R>(
        &mut self,
        stream: Stream,
        f: impl FnOnce(&mut Request, &mut PhpStream) -> R,
    ) -> Option<R>;
    fn streams_held<R>(&mut self, streams: &[Stream], wait: impl FnOnce(&mut Request) -> R) -> R;
    fn stream_ref(&self, stream: Stream) -> Option<&PhpStream>;
    fn stream_of(&self, value: &Value) -> Option<Stream>;
    fn stream_read_to_str(
        &mut self,
        stream: Stream,
        len: usize,
    ) -> Result<Option<Owner<PhpString>>, Thrown>;
    fn stream_from_fd(&mut self, fd: OwnedFd, kind: FdStream<'_>) -> Result<Owner<Stream>, Thrown>;
    fn wrapper_register(&mut self, protocol: &[u8], class: ClassId, flags: i64) -> bool;
    fn wrapper_unregister(&mut self, protocol: &[u8]) -> bool;
    fn wrapper_restore(&mut self, protocol: &[u8]) -> bool;
}

impl StreamLayer for Request {
    /// `url_stream_wrappers_hash`, the wrappers every request starts
    /// with, in the order they were registered: the engine's own where
    /// `standard` places them, among the extensions' (#920).
    fn global_wrappers(&self) -> Vec<Registered> {
        match self.streams_ref() {
            Some(state) => state.url_stream_wrappers.clone(),
            None => crate::state::layer_wrappers(),
        }
    }

    /// `php_register_url_stream_wrapper`'s insert, `zend_hash_add` on
    /// the global table: the row appended, or `false` where its scheme
    /// is taken. The extensions activate in php's startup order, so the
    /// rows they register stand in the order php's `MINIT`s registered
    /// them, and one registered once the program runs after every other.
    fn add_url_wrapper(&mut self, row: Registered) -> bool {
        let rows = &mut self.streams().url_stream_wrappers;
        if rows.iter().any(|held| held.scheme == row.scheme) {
            return false;
        }
        rows.push(row);
        true
    }

    /// `php_stream_get_url_stream_wrappers_hash`: the request's own table
    /// once a program changed it, the global one before.
    fn wrapper_table(&self) -> Vec<Registered> {
        self.streams_ref()
            .and_then(|state| state.url_wrappers.clone())
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
    fn locate_wrapper(&mut self, path: &[u8]) -> (Located, Vec<u8>) {
        self.locate(path, OpenOptions::REPORT)
    }

    /// [`Request::locate_wrapper`] under `options`, as `php_resolve_path`
    /// asks it with `STREAM_OPEN_FOR_INCLUDE` and no `REPORT_ERRORS`.
    fn locate_for(&mut self, path: &[u8], options: OpenOptions) -> (Located, Vec<u8>) {
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
                    self.active_function_name(),
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
                            self.active_function_name(),
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
            if self.streams().url_wrappers.is_some() {
                if let Some(found) = wrapper.or_else(|| Request::wrapper_row(&table, b"file")) {
                    return (found, local);
                }
                if options.report {
                    let text = format!(
                        "{}(): file:// wrapper is disabled in the server configuration",
                        self.active_function_name()
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
            } else if (options.for_include || self.streams().in_user_include)
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
                        self.active_function_name(),
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
        if self.streams().url_wrappers.is_none() {
            self.streams().url_wrappers = Some(self.global_wrappers());
        }
        self.streams().url_wrappers.get_or_insert_with(Vec::new)
    }

    /// `_php_stream_open_wrapper_ex` (`Z/main/streams/streams.c:2233`)
    /// under `REPORT_ERRORS`: an empty path is php's `ValueError`; with
    /// `USE_PATH` the path is resolved through `include_path` first; the
    /// wrapper is located and asked; a stream opened for append learns
    /// its position from the kind; and a stream not opened is
    /// `php_stream_display_wrapper_errors`'s warning, `Failed to open
    /// stream: ` and the message, at the path as written.
    fn open_stream_wrapper(
        &mut self,
        path: &[u8],
        mode: &[u8],
        use_include_path: bool,
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        let options = OpenOptions {
            use_path: use_include_path,
            ..OpenOptions::REPORT
        };
        self.open_wrapper_ex(path, mode, options)
    }

    /// `_php_stream_open_wrapper_ex` under an extension's [`Opening`],
    /// with the `*opened_path` it was handed: under
    /// `STREAM_OPEN_FOR_ZEND_STREAM` the name this open sets is the
    /// calling opener's own, as php's openers hand theirs on.
    fn open_wrapper_with(
        &mut self,
        path: &[u8],
        mode: &[u8],
        opening: crate::api::request::Opening,
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        // The opener's caller holds `*opened_path` where the opener hands
        // on `STREAM_OPEN_FOR_ZEND_STREAM`, and this open the
        // `opened_path`: the name it sets is the opener's own, set once
        // the open stands, and the string the open made of it given up.
        let Some((stream, named)) = self.open_wrapper_named(path, mode, opening.into())? else {
            return Ok(None);
        };
        if let Some(name) = named {
            let bytes = name.as_bytes().to_vec();
            name.give_up(self);
            self.streams().opened_path = Some(bytes);
        }
        Ok(Some(stream))
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
    fn open_url_wrapper(
        &mut self,
        path: &[u8],
        mode: &[u8],
    ) -> Result<Option<Owner<Stream>>, Thrown> {
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
    fn open_wrapper_ex(
        &mut self,
        path: &[u8],
        mode: &[u8],
        options: OpenOptions,
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        let Some((stream, named)) = self.open_wrapper_named(path, mode, options)? else {
            return Ok(None);
        };
        if let Some(name) = named {
            name.give_up(self);
        }
        Ok(Some(stream))
    }

    /// `_php_stream_open_wrapper_ex` with the `*opened_path` beside the
    /// stream where `options.for_zend_stream` asks for it: the name the
    /// opener set, a string of the request's (`_php_stream_fopen`'s
    /// `zend_string_init` of the real path for a plain file), or the
    /// path `USE_PATH` resolved where the opener set none. The resolved
    /// path is `zend_resolve_path`'s string of the request's, held while
    /// the wrapper opens and freed after unless handed out so, which an
    /// open's peak stands on.
    fn open_wrapper_named(
        &mut self,
        path: &[u8],
        mode: &[u8],
        options: OpenOptions,
    ) -> Result<Option<OpenedNamed>, Thrown> {
        if path.is_empty() {
            return Err(throw_error(self, "ValueError", "Path must not be empty"));
        }
        let mut resolved = None;
        if options.use_path {
            match self.resolve_path(path) {
                Ok(found) => {
                    resolved = Some(PhpString::filled(self, found.len(), |b| {
                        b.copy_from_slice(&found);
                    })?);
                }
                Err(crate::api::request::Unresolved::Threw(thrown)) => return Err(thrown),
                Err(crate::api::request::Unresolved::Missing(_)) => {}
            }
        }
        let to_open = resolved
            .as_ref()
            .map_or_else(|| path.to_vec(), |found| found.as_bytes().to_vec());
        let opened = self.open_located(&to_open, mode, options, resolved.is_some());
        match opened {
            Ok(Some((stream, None))) if options.for_zend_stream && resolved.is_some() => {
                Ok(Some((stream, resolved)))
            }
            opened => {
                if let Some(found) = resolved {
                    found.give_up(self);
                }
                opened
            }
        }
    }

    /// The wrapper `to_open` names located and asked, past
    /// `_php_stream_open_wrapper_ex`'s `USE_PATH`; `resolved` is whether
    /// that found the path, php's `STREAM_ASSUME_REALPATH`.
    fn open_located(
        &mut self,
        to_open: &[u8],
        mode: &[u8],
        options: OpenOptions,
        resolved: bool,
    ) -> Result<Option<OpenedNamed>, Thrown> {
        let use_path = options.use_path && !resolved;
        let (wrapper, path_to_open) = self.locate(to_open, options);
        if options.use_url && !wrapper.is_url(self) {
            let text = format!(
                "{}(): This function may only be used against URLs",
                self.active_function_name()
            );
            warning(self, &text);
            return Ok(None);
        }
        let outer = options
            .for_zend_stream
            .then(|| self.streams().opened_path.take());
        let (label, opened) = match wrapper {
            Located::Plain => (
                PLAIN_FILES,
                match self.plain_opener(&path_to_open, mode, options) {
                    Ok(stream) => {
                        // `_php_stream_fopen`'s `php_clear_stat_cache`
                        // (`Z/main/streams/plain_wrapper.c:1209`).
                        self.forget_stats();
                        let stream = self.stream_alloc(stream)?;
                        if options.for_zend_stream {
                            let real = if resolved {
                                Some(path_to_open.clone())
                            } else {
                                self.expand_filepath(&path_to_open)
                            };
                            self.streams().opened_path = real;
                        }
                        Ok(stream)
                    }
                    Err(message) => Err(message),
                },
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
        // The name the opener set, `*opened_path`, as the string php's
        // opener allocates; only the open that asks for one keeps it.
        let named = match outer {
            Some(outer) => std::mem::replace(&mut self.streams().opened_path, outer),
            None => None,
        };
        let (opened, named) = match (opened, named) {
            (Ok(stream), Some(name)) => {
                let filled = PhpString::filled(self, name.len(), |b| b.copy_from_slice(&name));
                let (stream, named) = freed_on_throw(self, stream, filled)?;
                (Ok(stream), Some(named))
            }
            (opened, _) => (opened, None),
        };
        match opened {
            Ok(stream) => {
                let labelled = self
                    .stream_op(*stream, |req, s| {
                        s.wrapper = label;
                        s.set_orig_path(req, to_open)
                    })
                    .transpose();
                if let Err(thrown) = labelled {
                    if let Some(name) = named {
                        name.give_up(self);
                    }
                    stream.free(self);
                    return Err(thrown);
                }
                if options.must_seek {
                    let seekable = self.make_seekable(stream, to_open, options);
                    return match seekable {
                        Ok(Some(stream)) => Ok(Some((stream, named))),
                        failed => {
                            if let Some(name) = named {
                                name.give_up(self);
                            }
                            failed.map(|_| None)
                        }
                    };
                }
                if mode.contains(&b'a') {
                    self.learn_append_position(*stream);
                }
                Ok(Some((stream, named)))
            }
            Err(message) => {
                if options.report {
                    self.display_wrapper_errors(to_open, "Failed to open stream", &message);
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
            Ok(host) => plain_open(self, &host, mode, options.for_include && !options.held),
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
    ) -> Result<(&'static str, Result<Owner<Stream>, String>), Thrown> {
        let wrapper = &self.streams().wrappers[at];
        let (label, open, stream_stat) = (wrapper.label, wrapper.open, wrapper.stream_stat);
        let Some(open) = open else {
            return Ok((
                label,
                Err(String::from("wrapper does not support stream open")),
            ));
        };
        let opening = options.opening(use_path);
        let outer = std::mem::take(&mut self.streams().wrapper_errors);
        let opened = crate::supplied::wrapper(self, at, |req| open(req, path, mode, opening));
        let logged = std::mem::replace(&mut self.streams().wrapper_errors, outer);
        // The opener's owner, which the open hands its caller.
        let opened = opened?;
        if let Some(stream) = &opened {
            self.stream_op(**stream, |_, s| s.set_wrapper_stat(stream_stat));
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
        stream: Owner<Stream>,
        path: &[u8],
        options: OpenOptions,
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        if self.stream_ref(*stream).is_some_and(|s| s.ops.seekable()) {
            return Ok(Some(stream));
        }
        let copy = if options.will_cast {
            temporary_file(self, None, b"php")
        } else {
            temp(self, TempMode::Default, PHP_STREAM_MAX_MEM).map(Some)
        };
        let (stream, copy) = freed_on_throw(self, stream, copy)?;
        if let Some(copy) = copy {
            if self.copy_all(*stream, *copy) {
                stream.free(self);
                let sought = self.stream_op(*copy, |req, s| {
                    s.seek(req, 0, Some(Whence::Set));
                    s.set_orig_path(req, path)
                });
                return Ok(Some(freed_on_throw(self, copy, sought.transpose())?.0));
            }
            copy.free(self);
        }
        stream.free(self);
        if options.report {
            let text = format!(
                "{}({}): could not make seekable - {}",
                self.active_function_name(),
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
    fn open_dir_wrapper(&mut self, path: &[u8]) -> Result<Option<Owner<Stream>>, Thrown> {
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
            Located::Data => ("RFC2397", Err(String::from("not implemented"))),
            Located::Ext(at) => {
                let wrapper = &self.streams().wrappers[at];
                let (label, open_dir) = (wrapper.label, wrapper.open_dir);
                let outer = std::mem::take(&mut self.streams().wrapper_errors);
                let opened = open_dir.map(|open_dir| {
                    crate::supplied::wrapper(self, at, |req| open_dir(req, &path_to_open))
                });
                let logged = std::mem::replace(&mut self.streams().wrapper_errors, outer);
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
                self.stream_op(*stream, |_, s| {
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
    #[cfg_attr(debug_assertions, track_caller)]
    fn stream_meta_data(&mut self, stream: Stream) -> Result<Owner<PhpArray>, Thrown> {
        let mut out = PhpArray::new(self)?;
        // The meta-data row the array is written into takes an owner of
        // its own; this one is given up once the rows are written.
        let crypto = match self.crypto_meta(stream) {
            Ok(crypto) => crypto,
            Err(thrown) => {
                out.give_up(self);
                return Err(thrown);
            }
        };
        let head = if let Some(mut head) = self.kind_meta_data(stream) {
            if let Some(crypto) = &crypto {
                head.insert(0, (b"crypto".to_vec(), MetaValue::Value(**crypto)));
            }
            head
        } else {
            let Some(eof) = self.stream_op(stream, |req, s| s.eof(req)) else {
                if let Some(crypto) = crypto {
                    crypto.give_up(self);
                }
                return Ok(out);
            };
            meta_three(false, true, eof)
        };
        let Some(s) = self.stream_ref(stream) else {
            if let Some(crypto) = crypto {
                crypto.give_up(self);
            }
            return Ok(out);
        };
        let wrapper = s.wrapper;
        let wrapper_data = s
            .wrapped
            .as_ref()
            .and_then(|w| w.wrapperdata.as_deref().copied())
            .or_else(|| s.ops.wrapper_data());
        let stream_type = s.ops.label();
        let mode = s.mode.bytes().to_vec();
        let unread = i64::try_from(s.writepos - s.readpos).unwrap_or(i64::MAX);
        let seekable = s.ops.seekable() && !s.flags.no_seek;
        let uri = s.orig_path().map(<[u8]>::to_vec);
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
        let written = meta_rows(self, &mut out, entries);
        if let Some(crypto) = crypto {
            crypto.give_up(self);
        }
        match written {
            Ok(()) => Ok(out),
            Err(thrown) => {
                out.give_up(self);
                Err(thrown)
            }
        }
    }

    /// `php_stream_context_set` (`Z/main/streams/streams.c:2385`): the
    /// stream keeps `context`, one owner more, in place of the one it
    /// kept, which gives its owner up; a socket's crypto layer reads it
    /// from then.
    fn set_stream_context(&mut self, stream: Stream, context: Option<StreamCtx>) {
        let mut held = context.map(|context| Owner::take(self, context));
        let old = self
            .stream_op(stream, |_, s| {
                if let Some(socket) = s.ops.as_socket() {
                    socket.set_ctx(context);
                }
                match held.take() {
                    Some(context) => s.wrapped_mut().context.replace(context),
                    None => s.wrapped.as_mut().and_then(|w| w.context.take()),
                }
            })
            .flatten();
        for owner in [old, held].into_iter().flatten() {
            owner.give_up(self);
        }
    }

    /// `PHP_STREAM_CONTEXT(stream)`.
    fn stream_context(&self, stream: Stream) -> Option<StreamCtx> {
        self.stream_ref(stream)?
            .wrapped
            .as_ref()?
            .context
            .as_deref()
            .copied()
    }

    /// What a socket's active crypto layer shows under `crypto`, ahead of
    /// the three the socket shows.
    fn crypto_meta(&mut self, stream: Stream) -> Result<Option<Owner<Value>>, Thrown> {
        let meta = self
            .stream_op(stream, |req, s| {
                let socket = s.ops.as_socket()?;
                let layer = socket.crypto_layer()?;
                Some(layer.meta(req))
            })
            .flatten();
        match meta {
            Some(Ok(Some(array))) => Ok(Some(array.into_value())),
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
    fn display_wrapper_errors(&mut self, path: &[u8], caption: &str, message: &str) {
        if self.is_pending() {
            return;
        }
        let function = self.active_function_name();
        self.wrapper_warning(&function, path, &format!("{caption}: {message}"));
    }

    /// `php_stream_alloc`'s `zend_register_resource(ret, le_stream)`: the
    /// stream as the payload of a resource of the `stream` type, one
    /// owner, under the next id.
    fn stream_alloc(&mut self, stream: PhpStream) -> Result<Owner<Stream>, Thrown> {
        self.stream_alloc_as(stream, false)
    }

    /// [`Request::stream_alloc`] under `le_pstream` where `persistent`
    /// says, `_php_stream_alloc` handed a `persistent_id`: the resource's
    /// type is `persistent stream`, whose regular-list destructor leaves
    /// the socket open ([`persistent::dtor`]).
    fn stream_alloc_as(
        &mut self,
        mut stream: PhpStream,
        persistent: bool,
    ) -> Result<Owner<Stream>, Thrown> {
        // `_php_stream_alloc`'s `FG(auto_detect_line_endings)`
        // (`Z/main/streams/streams.c:326`).
        if self
            .ini_get(b"auto_detect_line_endings")
            .is_some_and(Request::ini_parse_bool)
        {
            stream.flags.eol = Eol::Detect;
        }
        let types = self.types();
        let kind = if persistent {
            types.pstream
        } else {
            types.stream
        };
        // The kind's own data first, then the `php_stream` and its
        // resource, `_php_stream_fopen_from_fd_int`'s order; a
        // persistent stream's is `pemalloc`'d persistent, off the
        // request's count.
        stream.flags.persistent = persistent;
        let owner = if persistent {
            Resource::persistent(self, kind, Some(stream), PHP_STREAM_SIZE)?
        } else {
            stream.ops.lay_down(self)?;
            Resource::sized(self, kind, Some(stream), PHP_STREAM_SIZE)?
        };
        let res = *owner;
        if let Some(Some(stream)) = res.native_mut::<Option<PhpStream>>(self) {
            stream.res = Some(res);
            stream.ops.adopt(Stream::of(res));
        }
        Ok(owner.into_typed(Stream::of))
    }

    /// One operation on a stream with the request free beside it: the
    /// stream taken out of its resource for the call and put back
    /// after. `None` is a resource that is closed, is not a stream, or
    /// is inside a call already, which is php's `in_free` guard. A
    /// stream whose resource was closed while it was out is freed here,
    /// since the destructor found the slot empty.
    fn stream_op<R>(
        &mut self,
        stream: Stream,
        f: impl FnOnce(&mut Request, &mut PhpStream) -> R,
    ) -> Option<R> {
        let res = stream.resource();
        let mut taken = res.native_mut::<Option<PhpStream>>(self)?.take()?;
        let out = f(self, &mut taken);
        match res.native_mut::<Option<PhpStream>>(self) {
            Some(slot) => *slot = Some(taken),
            None => {
                self.streams().pclose_ret = taken.free(self);
            }
        }
        Some(out)
    }

    /// [`StreamLayer::stream_op`] over several streams at once, for the
    /// length of `wait`, each with an owner of its resource held: a
    /// stream out already, or closed, is passed over, and one whose
    /// resource was closed while it was out is freed as `wait` returns.
    fn streams_held<R>(&mut self, streams: &[Stream], wait: impl FnOnce(&mut Request) -> R) -> R {
        let mut taken = Vec::with_capacity(streams.len());
        for stream in streams {
            let res = stream.resource();
            let out = res
                .native_mut::<Option<PhpStream>>(self)
                .and_then(Option::take);
            if let Some(out) = out {
                taken.push((Owner::take(self, res), out));
            }
        }
        let answered = wait(self);
        for (res, out) in taken {
            match res.native_mut::<Option<PhpStream>>(self) {
                Some(slot) => *slot = Some(out),
                None => {
                    self.streams().pclose_ret = out.free(self);
                }
            }
            res.give_up(self);
        }
        answered
    }

    /// The stream a resource holds, read: `None` where it is closed, is
    /// not a stream, or is inside a call.
    fn stream_ref(&self, stream: Stream) -> Option<&PhpStream> {
        stream
            .resource()
            .native::<Option<PhpStream>>(self)?
            .as_ref()
    }

    /// `php_stream_from_zval_no_verify_no_error`: the stream a value
    /// names, which is an open resource of the `stream` type or the
    /// `persistent stream` type.
    fn stream_of(&self, value: &Value) -> Option<Stream> {
        // Only the `stream` and `persistent stream` types carry a stream,
        // so the payload says what `le_stream` and `le_pstream` would.
        let r = value.as_resource()?;
        r.kind()?;
        r.native::<Option<PhpStream>>(self).map(|_| Stream::of(r))
    }

    /// `php_stream_read_to_str` (`Z/main/streams/streams.c:822`): a
    /// string of `len` asked for first, so a length past the memory
    /// limit is php's fatal before a byte is read, then one read into
    /// it, its length set to what was read and the string cut down only
    /// where that is less than half of `len`; `None` is the read that
    /// failed, the string freed.
    ///
    /// ```php
    /// $p = __DIR__ . '/r.txt'; $w = fopen($p, 'w'); fwrite($w, str_repeat('x', 70000)); fclose($w);
    /// $f = fopen($p, 'r'); $s = fread($f, 100000); unset($s); fclose($f);
    /// $f = fopen($p, 'r'); memory_reset_peak_usage(); $m = memory_get_usage(); $s = fread($f, 100000);
    /// echo memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, ' ', strlen($s), "\n";
    /// $t = fread($f, 100000); var_dump($t); fclose($f); unlink($p);
    /// ```
    /// ```expect
    /// 110592 110624 70000
    /// string(0) ""
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    fn stream_read_to_str(
        &mut self,
        stream: Stream,
        len: usize,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let mut out = FreshString::new(self, len)?;
        let Some(Some(read)) = self.stream_op(stream, |req, s| s.read(req, out.bytes_mut())) else {
            out.free(self);
            return Ok(None);
        };
        out.set_len(read);
        if read < len / 2 {
            out = out.truncate(self, read)?;
        }
        Ok(Some(out.finish()))
    }

    fn stream_from_fd(&mut self, fd: OwnedFd, kind: FdStream<'_>) -> Result<Owner<Stream>, Thrown> {
        let (mode, process, kept) = match kind {
            FdStream::File { mode } => (mode, None, None),
            FdStream::Kept { mode, position } => (mode, None, Some(position)),
            FdStream::Process { mode, child } => (mode, Some(child), None),
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
        // A process pipe keeps the zero `php_stream_alloc` gives it
        // (`_php_stream_fopen_from_pipe`); only `_php_stream_fopen_from_fd`
        // marks a descriptor that cannot seek with no position.
        let position = if let Some(position) = kept.filter(|_| is_seekable) {
            position
        } else if is_seekable {
            file.stream_position()
                .map_or(-1, |at| i64::try_from(at).unwrap_or(i64::MAX))
        } else if process.is_some() {
            0
        } else {
            -1
        };
        let mut stream = PhpStream::new(
            Box::new(Stdio {
                data: None,
                file: Some(file),
                is_seekable,
                temp_name: None,
                process,
                blocking: true,
                suppress_errors: false,
            }),
            mode,
        );
        stream.flags.no_seek = !is_seekable;
        stream.position = position;
        self.stream_alloc(stream)
    }

    fn wrapper_register(&mut self, protocol: &[u8], class: ClassId, flags: i64) -> bool {
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

    fn wrapper_unregister(&mut self, protocol: &[u8]) -> bool {
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

    fn wrapper_restore(&mut self, protocol: &[u8]) -> bool {
        let shown = String::from_utf8_lossy(protocol).into_owned();
        let Some(original) = Request::wrapper_row(&self.global_wrappers(), protocol) else {
            let text = format!("{shown}:// never existed, nothing to restore");
            crate::api::raising::docref(self, Level::Warning, &text);
            return false;
        };
        let unchanged = match &self.streams().url_wrappers {
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
}

impl Stream {
    /// `pclose()`'s `zend_list_close(stream->res)`
    /// (`Z/ext/standard/file.c:829`): the resource closed, its
    /// destructor served, and `FG(pclose_ret)`, what the kind's close
    /// answered: a process pipe's exit status, `0` for any other kind,
    /// and `-1` where the child could not be waited for.
    pub fn pclose(self, req: &mut Request) -> i64 {
        req.streams().pclose_ret = 0;
        self.resource().close(req);
        i64::from(req.streams().pclose_ret)
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

    /// `dup(2)` of the stream's fd, the copy the caller's: the open file,
    /// its offset and a `flock(2)` taken on it outlive the stream, as
    /// php's persistent list keeps a `dba_popen` link's streams past the
    /// request, to be made a stream again with [`Request::stream_from_fd`]
    /// as [`FdStream::Kept`] at the position [`tell`](Stream::tell)
    /// answered. The write chain is flushed; the fd is sought to the
    /// position only where the read buffer holds bytes, which are dropped
    /// for the next stream to read again from the file; otherwise its
    /// offset is left where it stands, so where a forked process sharing
    /// the open file moved it, the next write lands there, as on php's
    /// persistent stream, which nothing touches between requests. `None`
    /// for a kind with no fd.
    pub fn dup_fd(self, req: &mut Request) -> Option<OwnedFd> {
        let fd = req
            .stream_op(self, |req, stream| stream.keep_fd(req))
            .flatten()?;
        crate::api::layer::dup_fd(fd).ok()
    }

    /// `php_stream_can_cast` (`Z/main/php_streams.h:567`), `_php_stream_cast`
    /// handed no `ret`: whether the stream's kind would answer `cast`, asked
    /// of the kind alone, the stream neither flushed, sought nor converted
    /// and nothing warned of. A user-space stream's `stream_cast` is still
    /// served, its bad answers refused in silence, and a `php://temp` whose
    /// bytes are in memory answers no where a cast would move them to a
    /// file first. What `stream_isatty` and ext/posix ask before they cast.
    /// A refusal names the kind that refused, `stream->ops->label`, read
    /// without asking the kind anything, which `php_posix_stream_get_fd`
    /// prints; a stream closed or inside a call refuses as no kind.
    ///
    /// ```php
    /// class W { public $context;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_seek($o, $w) { echo "sought\n"; return true; }
    ///     function stream_cast($as) { echo "cast $as\n"; return false; } }
    /// stream_wrapper_register('ww', 'W');
    /// var_dump(stream_isatty(fopen('ww://x', 'r')));
    /// $t = fopen('php://temp', 'w+'); fwrite($t, 'x');
    /// var_dump(stream_isatty($t), fstat($t)['ino']);
    /// ```
    /// ```expect
    /// cast 3
    /// cast 0
    /// bool(false)
    /// bool(false)
    /// int(0)
    /// ```
    ///
    /// # Errors
    ///
    /// The label of a kind that would not answer the cast.
    pub fn can_cast(self, req: &mut Request, cast: Cast) -> Result<(), &'static str> {
        req.stream_op(self, |req, stream| {
            if stream.can_cast(req, cast) {
                Ok(())
            } else {
                Err(stream.ops.label())
            }
        })
        .unwrap_or(Err(""))
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

/// The rows of `stream_get_meta_data`'s array written in order, each key
/// and value an owner the array takes.
fn meta_rows(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    entries: Vec<(Vec<u8>, MetaValue)>,
) -> Result<(), Thrown> {
    for (key, value) in entries {
        let key = PhpString::new(req, &key)?.into_key();
        let value = match value {
            MetaValue::Bool(b) => Owner::bool(b),
            MetaValue::Int(n) => Owner::int(n),
            MetaValue::Str(bytes) => match PhpString::new(req, &bytes) {
                Ok(bytes) => bytes.into_value(),
                Err(thrown) => {
                    key.give_up(req);
                    return Err(thrown);
                }
            },
            MetaValue::Value(value) => Owner::take(req, value),
        };
        out.set(req, key, value)?;
    }
    Ok(())
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
/// for such a wrapper: `phar_dir_ops` for a wrapper's own listing and
/// `php_glob_stream_ops` for a glob, the entries in order, `readdir` the
/// next and `rewinddir` back to the first, under the wrapper's label.
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

    /// `phar_dir_write`, which a wrapper's own listing has and a glob's
    /// has not, so `fwrite()` over the first answers `false` and raises
    /// nothing, where over a glob it is php's notice that the stream is
    /// not writable.
    fn writable(&self) -> bool {
        matches!(self.listed, DirListing::Names(_))
    }

    /// `phar_dir_write`'s `-1`.
    fn write(&mut self, _req: &mut Request, _buf: &[u8]) -> Option<usize> {
        None
    }

    fn read(&mut self, _req: &mut Request, _buf: &mut [u8], _eof: &mut bool) -> Option<usize> {
        None
    }

    fn close(&mut self, _req: &mut Request) -> i32 {
        0
    }

    /// `php_glob_stream_rewind` for a glob: back to the first entry,
    /// whatever was asked, its path given up. `phar_dir_seek`
    /// (`Z/ext/phar/dirstream.c:56-83`) for a wrapper's own listing:
    /// `SEEK_END` counted from the number of entries, the cursor back to
    /// the first for it and for `SEEK_SET`, a negative offset refused
    /// after that, and otherwise the cursor moved on as far as the offset
    /// or the end, the position how far it moved.
    ///
    /// ```php
    /// $p = sys_get_temp_dir() . '/listing' . getmypid() . '.phar';
    /// $phar = new Phar($p); $phar['a.txt'] = 'a'; $phar['b.txt'] = 'b'; $phar['c/d.txt'] = 'd'; unset($phar);
    /// $d = opendir("phar://$p");
    /// var_dump(fseek($d, 0, SEEK_END), ftell($d), fseek($d, -1), ftell($d), fseek($d, 1), ftell($d), readdir($d), fwrite($d, 'hi'));
    /// closedir($d); unlink($p);
    /// ```
    /// ```expect regime=phar-writable
    /// int(0)
    /// int(3)
    /// int(-1)
    /// int(3)
    /// int(0)
    /// int(1)
    /// string(5) "b.txt"
    /// bool(false)
    /// ```
    fn seek(
        &mut self,
        _req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let DirListing::Names(entries) = &self.listed else {
            self.at = 0;
            self.path = None;
            return Some(true);
        };
        let count = i64::try_from(entries.len()).unwrap_or(i64::MAX);
        let offset = match whence {
            Some(Whence::End) => count.saturating_add(offset),
            _ => offset,
        };
        if matches!(whence, Some(Whence::Set | Whence::End)) {
            self.at = 0;
        }
        if offset < 0 {
            return Some(false);
        }
        let left = entries.len().saturating_sub(self.at);
        let moved = usize::try_from(offset).unwrap_or(usize::MAX).min(left);
        self.at += moved;
        *position = i64::try_from(moved).unwrap_or(i64::MAX);
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the resource destructor's shape, which a destructor that throws needs"
)]
pub(crate) fn dtor(req: &mut Request, r: Resource) -> Result<(), Thrown> {
    let Some(stream) = r
        .native_mut::<Option<PhpStream>>(req)
        .and_then(Option::take)
    else {
        return Ok(());
    };
    req.streams().pclose_ret = stream.free(req);
    Ok(())
}

/// The `stream` resource type, `le_stream`, and the `persistent stream`
/// type, `le_pstream`, as `PHP_MINIT(streams)` registers them in turn
/// (`Z/main/streams/streams.c:1910-1911`).
pub(crate) fn register(
    resources: &mut crate::api::request::Resources,
) -> (ResourceType, ResourceType) {
    (
        resources.register_type(b"stream", Some(dtor)),
        resources.register_type(b"persistent stream", Some(persistent::dtor)),
    )
}
