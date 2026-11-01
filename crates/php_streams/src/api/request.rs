//! The streams facet of the request group (design §4.3 seam 5, §4.4):
//! [`Stream`] and the wrapper, filter and socket types a native opens and
//! registers through, and [`Streams`], the request's operations on them.
//! `php_ext` re-exports the public items here into its `request` group
//! beside the engine's, and `api.lock` counts them there.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

pub(crate) use php_engine::api::request::*;

use std::os::fd::OwnedFd;

use crate::api::decl::Type;

use crate::api::abi::FromArgument;
use crate::api::layer::{Face as _, TemporaryGuard};
use crate::api::raising::{Thrown, arg_type_error, fatal};
use crate::api::values::{ClassId, Owner, PhpArray, PhpString, Resource, ResourceView, Value};

mod context;
pub(crate) use context::context_dtor;
pub use context::{Notification, NotifyCode, StreamCtx};
mod crypto;
pub use crypto::{Crypto, CryptoSocket, Handshake, Readiness, XportFactory};
mod scanner_file;
pub use scanner_file::ScannerFile;

use crate::state::Layer;
use crate::streams::filters::FilterLayer;
use crate::streams::sockets::Transports;
use crate::streams::{PhpStream, StreamLayer, StreamOp};
use crate::supplied::Supplied;
use crate::wrapper::PlainFiles;
pub use crate::zend::STREAM_HOOKS;

/// An open stream, php's `php_stream` (design §4.3 seam 5), named by its
/// resource as php names it through `stream->res`: what `fopen` hands
/// back and `php_stream_to_zval` puts in a value, and what a native
/// reads back out with [`Stream::from_value`], `PHP_Z_PARAM_STREAM`'s
/// fetch. The operations are `php_stream_read` and its siblings, each
/// through the request that holds the stream; the layer under them is
/// the stream layer's ([`crate::streams`]).
#[derive(Clone, Copy)]
pub struct Stream(Resource);

/// `SEEK_SET`, `SEEK_CUR` and `SEEK_END`: where a seek's offset counts
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whence {
    Set,
    Cur,
    End,
}

/// What `php_stream_cast` is asked to make of a stream, of the casts
/// that give a fd: `PHP_STREAM_AS_FD_FOR_SELECT` with
/// `PHP_STREAM_CAST_INTERNAL`, which `stream_select` and `stream_isatty`
/// ask and which neither flushes nor seeks; `PHP_STREAM_AS_FD` with it;
/// and `PHP_STREAM_AS_FD` alone, which `proc_open` asks of a stream it
/// hands a child and which warns of buffered bytes the child will not
/// see; and `PHP_STREAM_AS_SOCKETD`, which `socket_import_stream` asks,
/// which only a socket's kind answers and which warns as `Fd` does; and
/// `PHP_STREAM_AS_STDIO` as php casts it where the C library has
/// `fopencookie` or `funopen`: an unfiltered plain file's fd, which the
/// caller `fdopen`s, and `None` for every other stream, which the caller
/// wraps in a cookie `FILE *` over the stream's own writes. So it never
/// fails and never warns that it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cast {
    Select,
    Internal,
    Fd,
    Socket,
    Stdio,
}

/// One option `php_stream_set_option` sets, with what it is set to: the
/// ones a native asks for through [`Stream::set_option`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOption {
    /// `PHP_STREAM_OPTION_BLOCKING`: `O_NONBLOCK` cleared for `true`.
    Blocking(bool),
    /// `PHP_STREAM_OPTION_READ_TIMEOUT`: how long a read waits.
    ReadTimeout(std::time::Duration),
    /// `PHP_STREAM_OPTION_READ_BUFFER`: `PHP_STREAM_BUFFER_NONE` for
    /// `buffered: false`, else `PHP_STREAM_BUFFER_FULL`, with the size a
    /// user-space stream's `stream_set_option` is handed, the `long` php
    /// reads through `ptrparam`.
    ReadBuffer { buffered: bool, size: i64 },
    /// `PHP_STREAM_OPTION_WRITE_BUFFER`, as the read buffer's.
    WriteBuffer { buffered: bool, size: i64 },
    /// `PHP_STREAM_OPTION_SET_CHUNK_SIZE`: how much one read of the kind
    /// asks for, and one write of a user-space stream hands it.
    ChunkSize(usize),
    /// `PHP_STREAM_OPTION_CHECK_LIVENESS`: whether the kind is still
    /// there to read, [`OptionAnswer::Err`] for one that is not, which
    /// `php_stream_eof` asks before it answers no.
    Liveness,
    /// `PHP_STREAM_OPTION_SYNC_API` under `PHP_STREAM_SYNC_FSYNC`, or
    /// `PHP_STREAM_SYNC_FDSYNC` for `data_only`: what was written made
    /// durable, `fsync(2)` or `fdatasync(2)` of a plain file's fd, which
    /// a pipe refuses. What `fsync()` and `fdatasync()` ask.
    Sync { data_only: bool },
    /// `PHP_STREAM_FLAG_SUPPRESS_ERRORS` (`Z/main/php_streams.h:187`),
    /// set for `true` and cleared for `false`: the kind's own notice for
    /// a read or a write that failed, a socket's `Send of %zu bytes
    /// failed` and a plain file's `Read of` and `Write of`, is not
    /// raised, and the caller reports the failure in its own words, as
    /// `mysqlnd_vio::connect` marks its connection's stream
    /// (`Z/ext/mysqlnd/mysqlnd_vio.c:288`). The two kinds that raise
    /// such a notice take it; any other kind has none to silence and
    /// answers [`OptionAnswer::NotImplemented`].
    SuppressErrors(bool),
}

/// What `php_stream_set_option` answers: `PHP_STREAM_OPTION_RETURN_OK`,
/// `_ERR` and `_NOTIMPL`, the last where neither the stream's kind nor
/// the generic layer takes the option, and the chunk size the stream had
/// for [`StreamOption::ChunkSize`], which the generic layer answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionAnswer {
    Ok,
    Err,
    NotImplemented,
    Previous(usize),
}

/// What a host fd becomes as a stream, [`Request::stream_from_fd`]:
/// the three openers php has over one that is already open.
#[derive(Debug)]
pub enum FdStream<'a> {
    /// `php_stream_fopen_from_fd(fd, mode, NULL)`: a plain file's kind,
    /// `STDIO`, which seeks where the fd does and has no position where
    /// it is a pipe or a character device; what `proc_open` hands back
    /// for a `pipe`.
    File { mode: &'a [u8] },
    /// The same over a fd [`Stream::dup_fd`] kept past its stream, at the
    /// `position` the stream held, which php's persistent stream keeps in
    /// memory between requests: the fd's own offset is not asked, where
    /// a forked process sharing the open file may have moved it.
    Kept { mode: &'a [u8], position: i64 },
    /// `php_stream_fopen_from_pipe` over what `popen(3)` opened: the
    /// same kind, never seeking, whose close waits for `child`, the
    /// process at the pipe's other end, and answers its exit status.
    Process {
        mode: &'a [u8],
        child: std::process::Child,
    },
    /// `php_stream_sock_open_from_socket`: a socket, of the
    /// `generic_socket` kind, `r+`, blocking, with
    /// `default_socket_timeout` as its read timeout; what
    /// `stream_socket_pair` and `proc_open`'s `socket` hand back.
    Socket,
    /// A socket `socket_export_stream` hands a stream: the kind of the
    /// registered xport `name` names, `php_stream_xport_create` with no
    /// address and the socket laid in afterwards, `generic_socket` for
    /// none; `r+`, with `blocking` as the stream's `is_blocked`.
    Xport {
        name: Option<&'a str>,
        blocking: bool,
    },
}

/// What `php_clear_stat_cache(clear_realpath_cache, filename,
/// filename_len)` (`Z/ext/standard/filestat.c:697`) does to the realpath
/// cache beside the stat cache, which it empties in every case, since an
/// entry of either path may be stale whatever file is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Realpaths<'p> {
    /// `clear_realpath_cache` false: the realpath cache kept, whatever
    /// file is named, as every caller but `clearstatcache(true)` asks.
    Kept,
    /// `clear_realpath_cache` true and no `filename`:
    /// `realpath_cache_clean`, every entry dropped, as `php_child_init`
    /// and `clearstatcache(true)` ask.
    All,
    /// `clear_realpath_cache` true and a `filename`:
    /// `realpath_cache_del` of that path as it was written, the entry of
    /// no other path dropped, as `clearstatcache(true, $filename)` asks.
    Of(&'p [u8]),
}

/// What a path's wrapper answers to its own `url_stat`, asked as
/// [`Request::url_stat`] and [`Request::wrapper_url_stat`] ask it: the
/// stat, laid out as `php_fstat` lays it out; `Failed` for the wrapper's
/// `-1`; `Unsupported` where no wrapper was located or the one located
/// has no `url_stat`, as `php://` has none; and `Refused` where the
/// `open_basedir` guard refused a plain file before it was stated, which
/// `php_stat` answers `false` to without its `stat failed` warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlStat {
    Found([i64; 13]),
    Failed,
    Unsupported,
    Refused,
}

impl UrlStat {
    /// The stat where there is one, `None` for every way there is not.
    #[must_use]
    pub fn found(self) -> Option<[i64; 13]> {
        match self {
            UrlStat::Found(sb) => Some(sb),
            UrlStat::Failed | UrlStat::Unsupported | UrlStat::Refused => None,
        }
    }
}

/// What `php_stream_xport_create` is asked to make of a `transport`'s
/// name: a client, connected within `timeout`, `None` to wait for ever,
/// where `connect` says, which is `STREAM_XPORT_CONNECT` or
/// `STREAM_XPORT_CONNECT_ASYNC` among php's flags, and otherwise a
/// stream of the `transport`'s kind holding no socket, the name not
/// even parsed; or a server bound to the name, listening where `listen`
/// says; each with the `context` the native resolved, which the stream
/// keeps as `PHP_STREAM_CONTEXT(stream)` and whose `socket` options the
/// `transport` reads as it connects or binds, as `xp_socket.c` does, and
/// the `socket` options the caller sets on the socket itself
/// ([`SocketOptions`]). A client with a `persistent` key, php's
/// `persistent_id`, is first looked for among the persistent streams
/// under it, and one made is kept there.
#[derive(Clone)]
pub enum Xport {
    Client {
        timeout: Option<std::time::Duration>,
        connect: bool,
        persistent: Option<Vec<u8>>,
        socket: SocketOptions,
        context: Option<StreamCtx>,
    },
    Server {
        listen: bool,
        socket: SocketOptions,
        context: Option<StreamCtx>,
    },
}

/// A persistent stream's connection out of every request, what
/// [`Stream::keep`] answers: the socket with its `transport`'s and crypto
/// layer's state, kept by the extension that opened it and by nothing
/// else, until a request lends it again with [`Streams::stream_lend`].
/// Dropped, it closes the socket.
pub struct Persisted(crate::streams::persistent::Entry);

/// The `socket` options of a socket, as `xp_socket.c` and `transports.c`
/// read them off `PHP_STREAM_CONTEXT(stream)`: each is
/// `php_stream_context_get_option(context, "socket", name)` read the way
/// its reader reads it, and the default is the value of none, which a
/// stream opened with no `context` has. A caller of
/// [`Request::xport_create`] hands the ones it sets on the socket itself,
/// which the `transport` lays over those it reads off the `context`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // php's options, one flag per name
pub struct SocketOptions {
    /// `bindto`, where a client binds before it connects.
    pub bindto: Option<BindTo>,
    /// `backlog`, `zval_get_long`: the queue a server's `listen(2)` is
    /// given; `32` where it is absent.
    pub listen_queue: Option<i64>,
    /// `tcp_nodelay`, `zend_is_true`: `TCP_NODELAY` on a TCP client and
    /// on each client a server accepts.
    pub tcp_nodelay: bool,
    /// `so_reuseport`, `zend_is_true`: `SO_REUSEPORT` on a server.
    pub so_reuseport: bool,
    /// `so_broadcast`, `zend_is_true`: `SO_BROADCAST` on a UDP socket.
    pub so_broadcast: bool,
    /// `ipv6_v6only` where it is not `null`, `zend_is_true`: the
    /// `IPV6_V6ONLY` a server's socket is given.
    pub ipv6_v6only: Option<bool>,
}

impl SocketOptions {
    /// What a socket is made with: the options `read` off its `context`,
    /// as php's `transport` reads them, under these, which the caller sets
    /// on the socket itself as `mysqlnd`'s `setsockopt` and `ftp`'s
    /// `listen` do once php's `transport` is done with the `context`: an
    /// option either turns on, and a value the caller gives in place of
    /// the read one.
    pub(crate) fn under(self, read: SocketOptions) -> SocketOptions {
        SocketOptions {
            bindto: self.bindto.or(read.bindto),
            listen_queue: self.listen_queue.or(read.listen_queue),
            tcp_nodelay: self.tcp_nodelay || read.tcp_nodelay,
            so_reuseport: self.so_reuseport || read.so_reuseport,
            so_broadcast: self.so_broadcast || read.so_broadcast,
            ipv6_v6only: self.ipv6_v6only.or(read.ipv6_v6only),
        }
    }
}

/// The `bindto` option as `php_tcp_sockop_connect` reads it: the address
/// written, or a value that is no string, which refuses the connect with
/// `local_addr context option is not a string.`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindTo {
    Address(Vec<u8>),
    NotAString,
}

/// Why `php_stream_xport_create` made no stream
/// (`Z/main/streams/transports.c:52-180`): the step that failed, the
/// `errno` the natives hand back as `$error_code`, `0` where there was
/// none, and what php writes into the caller's `error_string`, which the
/// natives hand back as `$error_message`. The text is `None` where php's
/// is `NULL`, a step that gave none, as a crypto layer that refuses the
/// connected socket gives none, which a caller asking for the text tells
/// apart from an empty one (`errstr == NULL ? "Unknown error" : ...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XportFailure {
    pub step: XportStep,
    pub code: i32,
    pub text: Option<String>,
}

/// The step of `_php_stream_xport_create` that made no stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XportStep {
    /// No xport is registered under the name's protocol in `xport_hash`:
    /// php's `ERR_REPORT`, whose text names the protocol, at most 31 bytes
    /// of it, in its `Unable to find the socket transport "%s"` sentence.
    Unregistered,
    /// `php_stream_xport_connect`, a client's.
    Connect,
    /// `php_stream_xport_bind`, a server's.
    Bind,
    /// `php_stream_xport_listen`, a server's once bound.
    Listen,
}

impl XportFailure {
    /// The warning `_php_stream_xport_create` raises itself where its
    /// caller asks for no `error_string`, which the caller raises as
    /// php's `php_error_docref(NULL, E_WARNING, ...)` does: an
    /// unregistered xport's text bare (`ERR_REPORT`), and a step's as
    /// `ERR_RETURN` writes it, `connect() failed: %s`, `bind() failed: %s`
    /// or `listen() failed: %s`, with `Unspecified error` where the step
    /// gave no text.
    #[must_use]
    pub fn warning(&self) -> String {
        let text = self.text.as_deref();
        let step = match self.step {
            XportStep::Unregistered => return text.unwrap_or_default().to_owned(),
            XportStep::Connect => "connect",
            XportStep::Bind => "bind",
            XportStep::Listen => "listen",
        };
        format!("{step}() failed: {}", text.unwrap_or("Unspecified error"))
    }
}

impl ResourceView for Stream {
    fn resource(&self) -> Resource {
        self.0
    }
}

impl Stream {
    pub(crate) const fn of(r: Resource) -> Stream {
        Stream(r)
    }

    /// The resource the stream is, `stream->res`.
    #[must_use]
    pub const fn resource(self) -> Resource {
        self.0
    }

    /// `php_stream_to_zval`: the resource as a value, `ZVAL_RES`, no
    /// owner taken, so the one owner `fopen` was given goes with the
    /// value it returns.
    #[must_use]
    pub const fn as_value(self) -> Value {
        self.0.as_value()
    }

    /// `php_stream_from_zval_no_verify_no_error`, what
    /// `PHP_Z_PARAM_STREAM` fetches: the stream `value` names, an open
    /// resource of the `stream` type, and `None` for anything else, a
    /// closed one included.
    #[must_use]
    pub fn from_value(req: &Request, value: &Value) -> Option<Stream> {
        req.stream_of(value)
    }

    /// `php_stream_read_to_str`: up to `len` bytes as one string of the
    /// request's, fewer at the end, `None` for the read that failed
    /// before a byte was had (`fread` answers `false`); the `Err` is
    /// the memory limit, met before a byte is read.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn read(self, req: &mut Request, len: usize) -> Result<Option<Owner<PhpString>>, Thrown> {
        req.stream_read_to_str(self, len)
    }

    /// `php_stream_read(stream, buf, len)` into a buffer the native holds,
    /// as php's natives read into a `char buf[N]` on their stack: how
    /// many bytes were had, zero at the end, `None` for the read that
    /// failed before a byte was had. Nothing is allocated for the bytes.
    pub fn read_into(self, req: &mut Request, buf: &mut [u8]) -> Option<usize> {
        req.stream_op(self, |op| op.read(buf)).flatten()
    }

    /// `php_stream_write`: how many bytes the stream took, `None` for
    /// php's `-1` (`fwrite` answers `false`).
    pub fn write(self, req: &mut Request, bytes: &[u8]) -> Option<usize> {
        req.stream_op(self, |op| op.write(bytes)).flatten()
    }

    /// `php_stream_seek`: `true` for php's `0`. A `whence` of `None` is
    /// a number that is none of the three, [`Whence::from_int`]'s
    /// `None`, which a stream that seeks refuses and one that cannot is
    /// php's warning for.
    pub fn seek(self, req: &mut Request, offset: i64, whence: impl Into<Option<Whence>>) -> bool {
        let whence = whence.into();
        req.stream_op(self, |op| op.seek(offset, whence))
            .unwrap_or(false)
    }

    /// `php_stream_get_line`: the next line with its `\n`, at most
    /// `maxlen - 1` bytes of it, or all of it for `None`; `None` back is
    /// no byte to give, at eof or after a read that failed (`fgets`
    /// answers `false`). The line is bytes, the buffer php's
    /// `php_stream_get_line(stream, NULL, 0, &len)` hands its caller to
    /// `efree`, and no string: a native that reads it for a parser holds
    /// nothing to give up, and one that hands it to a program, as `fgets`
    /// does, makes the string. Room for `maxlen` is asked of the heap
    /// first, as `fgets` asks `zend_string_alloc` and `fgetcsv`
    /// `emalloc`, so a length past the memory limit is php's fatal before
    /// a byte is read.
    pub fn get_line(
        self,
        req: &mut Request,
        maxlen: Option<usize>,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        if let Some(len) = maxlen
            && len > req.heap_memory_limit()
        {
            let asked = PhpString::filled(req, len, |_| {})?;
            asked.give_up(req);
        }
        Ok(req.stream_op(self, |op| op.get_line(maxlen)).flatten())
    }

    /// `php_stream_get_record`, what `stream_get_line` answers: the bytes
    /// before `delim`, which is read past, or `maxlen` of them where it
    /// comes no sooner, the stream's read buffer filled a chunk at a
    /// time; `None` for nothing found and nothing more to come yet short of
    /// eof, as a non-blocking stream answers, and for no byte at eof.
    ///
    /// ```php
    /// $m = fopen('php://memory', 'w+'); fwrite($m, "a\r\nbb\r\nccc"); rewind($m);
    /// var_dump(stream_get_line($m, 100, "\r\n"), stream_get_meta_data($m)['unread_bytes']);
    /// var_dump(stream_get_line($m, 1, "\r\n"), stream_get_line($m, 100, "\r\n"), stream_get_line($m, 100, "\r\n"), stream_get_line($m, 100, "\r\n"));
    /// ```
    /// ```expect
    /// string(1) "a"
    /// int(7)
    /// string(1) "b"
    /// string(1) "b"
    /// string(3) "ccc"
    /// bool(false)
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn get_record(
        self,
        req: &mut Request,
        maxlen: usize,
        delim: &[u8],
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        match req
            .stream_op(self, |op| op.get_record(maxlen, delim))
            .flatten()
        {
            // php 8.5.12's `zend_string_falloc(tent_ret_len, 0, true)`:
            // a record the memory limit has no room for is `NULL`, the
            // limit's fatal raised after the call.
            Some(record) => req
                .memory_mut()
                .fallible(|req| PhpString::new(req, &record)),
            None => Ok(None),
        }
    }

    /// `_php_stream_copy_to_mem`: the rest of the stream, or up to
    /// `maxlen` bytes of it, as one string, empty where nothing was read.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn copy_to_mem(
        self,
        req: &mut Request,
        maxlen: Option<usize>,
    ) -> Result<Owner<PhpString>, Thrown> {
        match req.stream_op(self, |op| op.copy_to_mem(maxlen)) {
            Some(Ok(Some(read))) => Ok(read),
            Some(Err(thrown)) => Err(thrown),
            // php's `NULL`, nothing read, which its callers answer as
            // `ZSTR_EMPTY_ALLOC()`.
            Some(Ok(None)) | None => PhpString::new(req, b""),
        }
    }

    /// `php_stream_stat`, as `php_fstat` lays it out: `dev`, `ino`,
    /// `mode`, `nlink`, `uid`, `gid`, `rdev`, `size`, `atime`, `mtime`,
    /// `ctime`, `blksize` and `blocks`, or `None` where the stream has
    /// no stat (`fstat` answers `false`).
    pub fn stat(self, req: &mut Request) -> Option<[i64; 13]> {
        req.stream_op(self, StreamOp::stat).flatten()
    }

    /// `php_stream_truncate_set_size` behind
    /// `php_stream_truncate_supported`: `None` where the stream does not
    /// truncate (`ftruncate`'s `Can't truncate this stream!`), else
    /// whether it took the size; the `Err` is the memory limit's fatal of
    /// a memory stream grown past it.
    ///
    /// ```php
    /// $t = fopen('php://temp', 'w+');
    /// var_dump(ftruncate($t, 3), strlen(stream_get_contents($t, -1, 0)));
    /// ftruncate($t, PHP_INT_MAX);
    /// ```
    /// ```expect exit=255
    /// bool(true)
    /// int(3)
    ///
    /// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate 9223372036854792192 bytes) in %s on line 4
    /// ```
    pub fn truncate(self, req: &mut Request, size: usize) -> Result<Option<bool>, Thrown> {
        let answer = req.stream_op(self, |op| op.truncate(size)).flatten();
        match req.diagnostics_mut().stream_thrown().take() {
            Some(thrown) => Err(thrown),
            None => Ok(answer),
        }
    }

    /// `php_stream_lock`, `flock(2)` with its operation, which
    /// `php_stream_supports_lock` guards: `false` where the stream has
    /// no `flock(2)` or `flock(2)` refused.
    pub fn flock(self, req: &mut Request, operation: i64) -> bool {
        req.stream_op(self, |op| op.flock(operation))
            .unwrap_or(false)
    }

    /// `php_stream_is(stream, PHP_STREAM_IS_STDIO)`: the stream is a
    /// plain file's, or one of the host's standard ones.
    #[must_use]
    pub fn is_stdio(self, req: &Request) -> bool {
        req.stream_ref(self).is_some_and(PhpStream::is_stdio)
    }

    /// `php_stream_set_option` (`Z/main/streams/streams.c:1447`): the
    /// stream's kind asked first, then, where it does not take the
    /// option, the generic layer, which takes the chunk size and whether
    /// the read buffer is used. A stream that is closed answers
    /// [`OptionAnswer::Err`].
    pub fn set_option(self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        req.stream_op(self, |op| op.set_option(option))
            .unwrap_or(OptionAnswer::Err)
    }

    /// What `stream_get_meta_data` answers: `timed_out`, `blocked` and
    /// `eof` where the stream's kind reports them, `wrapper_type`,
    /// `stream_type`, `mode`, `unread_bytes`, `seekable` and `uri`, in
    /// php's order.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn meta_data(self, req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        req.stream_meta_data(self)
    }

    /// `PHP_STREAM_CONTEXT(stream)`: the `context` the stream keeps, which
    /// a socket's `transport` is given as it is made
    /// ([`Request::xport_create`]) and a native sets
    /// ([`Stream::set_ctx`]); `None` for a stream that keeps none, a
    /// closed one, and one inside a call.
    #[must_use]
    pub fn ctx(self, req: &Request) -> Option<StreamCtx> {
        req.stream_context(self)
    }

    /// `php_stream_context_set` (`Z/main/streams/streams.c:2385`): the
    /// stream keeps `context`, one owner more, and gives up the one it
    /// kept; `None` keeps none.
    ///
    /// ```php
    /// $s = stream_socket_server('tcp://127.0.0.1:0', $no, $str, STREAM_SERVER_BIND | STREAM_SERVER_LISTEN, stream_context_create(['x' => ['y' => 1]]));
    /// var_dump(stream_context_get_options($s));
    /// $f = fopen(__FILE__, 'r');
    /// stream_context_set_option($f, 'a', 'b', 2);
    /// var_dump(stream_context_get_options($f), stream_context_get_options(stream_context_get_default()));
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["x"]=>
    ///   array(1) {
    ///     ["y"]=>
    ///     int(1)
    ///   }
    /// }
    /// array(1) {
    ///   ["a"]=>
    ///   array(1) {
    ///     ["b"]=>
    ///     int(2)
    ///   }
    /// }
    /// array(0) {
    /// }
    /// ```
    pub fn set_ctx(self, req: &mut Request, ctx: Option<StreamCtx>) {
        req.set_stream_context(self, ctx);
    }

    /// `stream->orig_path`: the path the stream was opened by, the one
    /// the include path resolved where it was searched, as
    /// `stream_get_meta_data` answers it under `uri`, read without asking
    /// the stream anything; `None` for a closed stream and one opened by
    /// no path.
    ///
    /// ```php
    /// $d = sys_get_temp_dir() . '/spl-orig-' . getmypid(); @mkdir("$d/sub", 0777, true); touch("$d/sub/x.txt");
    /// set_include_path('sub'); chdir($d);
    /// $o = new SplFileObject('x.txt', 'r', true);
    /// var_dump($o->getPath() === realpath("$d/sub"), $o->getFilename(), $o->getRealPath() === realpath("$d/sub/x.txt"));
    /// unlink("$d/sub/x.txt"); rmdir("$d/sub"); rmdir($d);
    /// ```
    /// ```expect
    /// bool(true)
    /// string(5) "x.txt"
    /// bool(true)
    /// ```
    #[must_use]
    pub fn orig_path(self, req: &Request) -> Option<Vec<u8>> {
        req.stream_ref(self)?.orig_path().map(<[u8]>::to_vec)
    }

    /// `stream->mode`: the bytes the stream was opened as (`fopen`'s
    /// `$mode`), cut to fifteen as php's `char mode[16]` holds them,
    /// which `stream_get_meta_data` answers under `mode`, read without
    /// asking the stream anything, so no method of a user wrapper runs;
    /// `None` for a closed stream and one inside a call.
    #[must_use]
    pub fn opened_as(self, req: &Request) -> Option<Vec<u8>> {
        Some(req.stream_ref(self)?.mode.bytes().to_vec())
    }

    /// `stream->wrapperdata`: what `stream_get_meta_data` answers under
    /// `wrapper_data`, the header lines the http wrapper read or a
    /// user-space stream's instance, read without asking the stream
    /// anything, so no method of a user wrapper runs; the stream's own
    /// value, no owner taken, and `None` where it holds none, for a
    /// closed stream and one inside a call.
    #[must_use]
    pub fn wrapper_data(self, req: &Request) -> Option<Value> {
        req.stream_ref(self)?.wrapper_data()
    }

    /// `stream->wrapper->is_url`: the wrapper that opened the stream is a
    /// URL wrapper, `http`, `ftp`, `data` or one a program registered
    /// with `STREAM_IS_URL`, read without asking the stream anything;
    /// `false` for a stream no wrapper opened, a closed one and one
    /// inside a call.
    #[must_use]
    pub fn is_url(self, req: &Request) -> bool {
        req.stream_ref(self)
            .is_some_and(|stream| stream.wrapper.is_url)
    }

    /// `stream->orig_path = estrdup(path)` over a stream an extension
    /// made ([`Request::new_stream`]), which no path opened:
    /// `php_stream_zip_open` names an entry's stream by the entry, and
    /// `stream_get_meta_data` answers it under `uri`. Nothing for a closed
    /// stream.
    ///
    /// ```php
    /// $f = 'orig-path-witness.zip';
    /// $z = new ZipArchive; $z->open($f, ZipArchive::CREATE); $z->addFromString('dir/a.txt', 'abc'); $z->close();
    /// $z->open($f); $s = $z->getStream('dir/a.txt');
    /// var_dump(stream_get_meta_data($s)['uri'], fread($s, 3));
    /// fclose($s); $z->close(); unlink($f);
    /// ```
    /// ```expect
    /// string(9) "dir/a.txt"
    /// string(3) "abc"
    /// ```
    pub fn set_orig_path(self, req: &mut Request, path: &[u8]) -> Result<(), Thrown> {
        req.stream_op(self, |op| op.set_orig_path(path))
            .transpose()
            .map(|_| ())
    }

    /// `stream->flags |= PHP_STREAM_FLAG_NO_FCLOSE`: an object keeps the
    /// stream, and `fclose()` over its resource refuses it with php's
    /// warning; the object's own close is not refused.
    ///
    /// ```php
    /// $o = new SplFileObject(__FILE__);
    /// $r = get_resources(); var_dump(fclose(end($r)), $o->fgets());
    /// ```
    /// ```expect
    /// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 3
    /// bool(false)
    /// string(6) "<?php
    /// "
    /// ```
    pub fn forbid_fclose(self, req: &mut Request) {
        req.stream_with(self, PhpStream::forbid_fclose);
    }

    /// Whether `fclose()` refuses the stream,
    /// `PHP_STREAM_FLAG_NO_FCLOSE`: a directory, and a stream an object
    /// keeps ([`Stream::forbid_fclose`]).
    #[must_use]
    pub fn no_fclose(self, req: &Request) -> bool {
        req.stream_ref(self).is_some_and(PhpStream::no_fclose)
    }

    /// Whether `opendir` opened the stream, `PHP_STREAM_FLAG_IS_DIR`,
    /// which is what `readdir` and `closedir` accept and `fclose`
    /// refuses.
    #[must_use]
    pub fn is_dir(self, req: &Request) -> bool {
        req.stream_ref(self).is_some_and(PhpStream::is_dir)
    }

    /// `php_glob_stream_get_count` under `php_stream_is(stream,
    /// &php_glob_stream_ops)`: how many paths a `glob://` directory lists,
    /// `None` for a stream that is no glob.
    #[must_use]
    pub fn glob_count(self, req: &Request) -> Option<usize> {
        req.stream_ref(self)?.glob().map(|(_, count)| count)
    }

    /// `php_glob_stream_get_path`: the directory part of the match the
    /// glob last handed out, or of its first match or pattern before any
    /// read; `None` past the last, after a rewind, and for a stream that
    /// is no glob.
    #[must_use]
    pub fn glob_path(self, req: &Request) -> Option<Vec<u8>> {
        req.stream_ref(self)?
            .glob()
            .and_then(|(path, _)| path.map(<[u8]>::to_vec))
    }

    /// `php_stream_readdir`: the next entry's name, `.` and `..` first,
    /// `None` past the last (`readdir` answers `false`). The name is
    /// bytes, the `d_name` of the `php_stream_dirent` php fills, and no
    /// string: a native that lists names for itself holds nothing to
    /// give up, and one that hands a name to a program, as `readdir`
    /// does, makes the string.
    pub fn read_dir(self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        Ok(req.stream_op(self, StreamOp::read_dir).flatten())
    }

    /// `php_stream_tell`: the position, `-1` for a stream that has none.
    #[must_use]
    pub fn tell(self, req: &Request) -> i64 {
        req.stream_ref(self).map_or(-1, PhpStream::position)
    }

    /// `php_stream_eof`: no byte left in the read buffer, and the kind at
    /// eof or, asked `PHP_STREAM_OPTION_CHECK_LIVENESS`, found dead, which
    /// a user-space stream answers from its `stream_eof`.
    ///
    /// ```php
    /// class W { public $context; public $n = 0;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_eof() { echo "eof? ", $this->n, "\n"; return $this->n++ > 0; }
    ///     function stream_read($c) { return ""; } }
    /// class V { public $context; function stream_open($p, $m, $o, &$op) { return true; } function stream_eof() { return 1; } }
    /// stream_wrapper_register('ww', 'W'); stream_wrapper_register('vv', 'V');
    /// $f = fopen('ww://x', 'r');
    /// var_dump(feof($f), feof($f), feof($f));
    /// var_dump(feof(fopen('vv://x', 'r')));
    /// ```
    /// ```expect
    /// eof? 0
    /// eof? 1
    /// bool(false)
    /// bool(true)
    /// bool(true)
    ///
    /// Warning: feof(): V::stream_eof value must be of type bool, int given in %s on line 10
    /// bool(true)
    /// ```
    pub fn eof(self, req: &mut Request) -> bool {
        req.stream_op(self, StreamOp::eof).unwrap_or(false)
    }

    /// `php_stream_flush`: `true` for php's `0`.
    pub fn flush(self, req: &mut Request) -> bool {
        req.stream_op(self, StreamOp::flush).unwrap_or(false)
    }

    /// `php_stream_free(stream, PHP_STREAM_FREE_KEEP_RSRC |
    /// PHP_STREAM_FREE_CLOSE)`, what `fclose` does past its checks: the
    /// resource closed and its destructor served, which flushes what was
    /// written and closes the kind; the resource stays for its owners,
    /// `resource(N) of type (Unknown)` under `var_dump`. `false`, and
    /// nothing closed, for a stream held by the operation serving PHP code
    /// on it, php's `PHP_STREAM_FLAG_NO_FCLOSE`, which a user-space
    /// stream's `stream_read`, `stream_eof`, `stream_write`, `stream_seek`
    /// and `stream_cast` and a user filter's call set.
    ///
    /// ```php
    /// class B { public $context;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_read($n) { global $s; var_dump(fclose($s)); return ""; }
    ///     function stream_eof() { return true; }
    ///     function stream_flush() { global $s; var_dump(fclose($s)); return true; } }
    /// stream_wrapper_register('bb', 'B');
    /// $s = fopen('bb://x', 'r');
    /// fread($s, 1);
    /// fflush($s);
    /// try { fclose($s); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 4
    /// bool(false)
    /// bool(true)
    /// fclose(): Argument #1 ($stream) must be an open stream resource
    /// ```
    pub fn close(self, req: &mut Request) -> bool {
        if req.streams().held_streams.contains(&self.0) {
            return false;
        }
        req.close_standard(self.0);
        crate::streams::PhpStream::close_open(req, self);
        true
    }

    /// `stream->wrapperthis`, with the wrapper's `stream_closer` the
    /// stream's free runs over it ([`Closer`]); where the stream is closed
    /// or inside a call, it is closed at once.
    pub fn set_closer(self, req: &mut Request, closer: Box<dyn Closer>) -> Result<(), Thrown> {
        let mut closer = Some(closer);
        req.stream_with(self, |stream| stream.set_closer(closer.take()));
        match closer {
            Some(left) => left.close(req),
            None => Ok(()),
        }
    }

    /// `ZVAL_COPY(&stream->wrapperdata, value)`: what `stream_get_meta_data`
    /// prints as `wrapper_data`, the headers the http wrapper read, one
    /// owner moving to the stream; where it is closed, the owner is
    /// given up.
    pub fn set_wrapper_data(self, req: &mut Request, value: Owner<Value>) {
        let mut value = Some(value);
        let old = req
            .stream_with(self, |stream| stream.set_wrapper_data(value.take()))
            .flatten();
        for left in [old, value].into_iter().flatten() {
            left.give_up(req);
        }
    }

    /// The end of `php_stream_url_wrap_http_ex` on the stream it read a
    /// head off: `stream->position = 0`, since as far as streams are
    /// concerned it is at its start, and `strlcpy(stream->mode, mode)`,
    /// the `mode` the program opened it with over the `transport`'s `r+`.
    pub fn rewind_as(self, req: &mut Request, mode: &[u8]) {
        req.stream_with(self, |stream| stream.rewind_as(mode));
    }
}

/// `php_stream_ops` for a kind of stream an extension implements, which
/// the engine's generic half of every stream runs over as it runs its
/// own kinds: the label `stream_get_meta_data` prints as `stream_type`,
/// the operations each answering as php's does, `None` for the `-1` an
/// operation fails with, and a kind holding its own `abstract` as its
/// fields. What `pdo_sqlite`'s blob streams and their kin are made of,
/// through [`Request::new_stream`].
pub trait StreamOps: Send {
    /// `ops->label`.
    fn label(&self) -> &'static str;

    /// `ops->write`: how many bytes were taken.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize>;

    /// `ops->read`: how many bytes were given, `eof` set where the kind
    /// sets `stream->eof`.
    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize>;

    /// `ops->close`, with what the kind holds.
    fn close(&mut self, req: &mut Request);

    /// `ops->seek`, `None` where the kind has none: `position` is php's
    /// `*newoffs`, written as the kind writes it; `Some(true)` for
    /// php's `0`.
    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let _ = (req, offset, whence, position);
        None
    }

    /// Whether the kind has an `ops->seek`.
    fn seekable(&self) -> bool {
        false
    }

    /// `ops->stat` for a kind that fills `st_size` alone, `None` where it
    /// has none.
    fn size(&self) -> Option<i64> {
        None
    }

    /// `ops->stat` whole, the thirteen fields `fstat()` prints in their
    /// order, as phar's `phar_stream_stat` fills them from the entry;
    /// by default the one [`StreamOps::size`] fills, `None` where the
    /// kind has no stat.
    fn stat(&mut self, req: &mut Request) -> Option<[i64; 13]> {
        let _ = req;
        let size = self.size()?;
        let mut buf = [0; 13];
        buf[7] = size;
        Some(buf)
    }

    /// `ops->flush`: `true` for php's `0`, which a kind with nothing of
    /// its own to flush answers.
    fn flush(&mut self, req: &mut Request) -> bool {
        let _ = req;
        true
    }

    /// `PHP_STREAM_OPTION_LOCKING` with `flock(2)`'s operation, as
    /// [`Stream::flock`] asks it: `None` where the kind has no
    /// `flock(2)`, else whether `flock(2)` took it, which a kind over
    /// another stream answers by asking that one, as
    /// `php_gziop_set_option` does.
    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        let _ = (req, operation);
        None
    }

    /// The stream `PHP_STREAM_OPTION_META_DATA_API` is handed to, as
    /// `php_gziop_set_option` hands it to the stream under the `gzFile`:
    /// `stream_get_meta_data` then starts with what that stream's kind
    /// answers, its `timed_out`, `blocked` and `eof`. `None` is a kind that
    /// takes no option, for which the generic half answers the three with
    /// `eof` as `php_stream_eof` reads it.
    fn meta_data_from(&self) -> Option<Stream> {
        None
    }

    /// Whether the generic half keeps a read buffer before the kind:
    /// `false` is `PHP_STREAM_FLAG_NO_BUFFER`, set on the stream as
    /// `php_stream_gzopen` sets it, so every read reaches the kind.
    fn buffered(&self) -> bool {
        true
    }
}

/// A stream wrapper's operations, Zend's `php_stream_wrapper_ops` as a
/// value: the scheme it serves and one function per operation, `None`
/// where the wrapper does not support it.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)] // Zend's ops struct: a table of function pointers
pub struct StreamWrapper {
    /// The protocol, as `php_register_url_stream_wrapper` is given it.
    pub scheme: String,
    /// `wops->label`, which `stream_get_meta_data` prints as a stream's
    /// `wrapper_type` and a directory's `stream_type`, and the refusals
    /// of `unlink` and `rename` name.
    pub label: &'static str,
    /// `wrapper->is_url`: a URL wrapper, which `allow_url_fopen=0`
    /// refuses every open of, `allow_url_include=0` an include's, and
    /// which `get_headers` alone opens through (`STREAM_USE_URL`).
    pub is_url: bool,
    /// `stream_opener`, `None` for a wrapper that opens no stream, php's
    /// `wrapper does not support stream open`; the opener's own `None`
    /// is the open that failed, which `php_stream_display_wrapper_errors`
    /// reports as `operation failed`. It is handed the path, the `mode` and
    /// the [`Opening`] php hands it, `REPORT_ERRORS` cleared, and answers
    /// the stream's one owner, which the open hands its caller.
    pub open:
        Option<fn(&mut Request, &[u8], &[u8], Opening) -> Result<Option<Owner<Stream>>, Thrown>>,
    /// `dir_opener`: what the directory lists, which the engine serves
    /// as a directory stream under the wrapper's label (`rewinddir` back
    /// to the first); `None` is the open that failed.
    pub open_dir: Option<fn(&mut Request, &[u8]) -> Result<Option<DirListing>, Thrown>>,
    /// `url_stat`: the array `statbuf_from_array` reads the thirteen
    /// fields out of, its one owner the caller's, which gives it up once
    /// read; `None` is the stat that failed.
    pub stat: Option<fn(&mut Request, &[u8]) -> Result<Option<Owner<PhpArray>>, Thrown>>,
    pub unlink: Option<fn(&mut Request, &[u8]) -> Result<bool, Thrown>>,
    pub rename: Option<fn(&mut Request, &[u8], &[u8]) -> Result<bool, Thrown>>,
    pub mkdir: Option<fn(&mut Request, &[u8], i64, bool) -> Result<bool, Thrown>>,
    pub rmdir: Option<fn(&mut Request, &[u8]) -> Result<bool, Thrown>>,
    /// `stream_metadata`: one of [`WrapperOp::Touch`], [`WrapperOp::Chmod`],
    /// [`WrapperOp::Owner`] and [`WrapperOp::Group`], with the path as
    /// the program wrote it.
    pub set_stat: Option<fn(&mut Request, &[u8], WrapperOp<'_>) -> Result<bool, Thrown>>,
    /// `stream_stat`, which `fstat()` of a stream the wrapper opened asks
    /// in place of the stream's kind (`_php_stream_stat`): `None` where
    /// the wrapper has none and the kind answers; the function's `None`
    /// is php's `-1`, as the http and ftp wrappers answer always.
    pub stream_stat: Option<fn(&mut Request, Stream) -> Option<[i64; 13]>>,
}

/// `stream->wrapperthis` with its wrapper's `wops->stream_closer`: what a
/// wrapper keeps beside a stream it opened, set by [`Stream::set_closer`]
/// and handed back by the stream's free once the kind is closed and its
/// filters are off, as `php_stream_ftp_stream_close` then reads the
/// control connection's last reply and quits it.
pub trait Closer: Send {
    /// `wops->stream_closer(wrapper, stream)`.
    fn close(self: Box<Self>, req: &mut Request) -> Result<(), Thrown>;
}

/// What a wrapper's directory opener lists, which the engine serves as
/// a directory stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirListing {
    /// The entries in the order `readdir` hands them out, a wrapper's own
    /// dir stream (phar's).
    Names(Vec<Vec<u8>>),
    /// `php_glob_stream_ops` (`Z/main/streams/glob_wrapper.c`): the paths
    /// `php_glob` matched, those `open_basedir` refuses left out, and
    /// the pattern after `glob://`. `readdir` hands out the part of each
    /// match after its last `/` and keeps the part before it as the
    /// stream's path (`php_glob_stream_path_split`), which
    /// [`Stream::glob_path`] reads, as [`Stream::glob_count`] reads how
    /// many there are.
    Glob {
        pattern: Vec<u8>,
        matches: Vec<Vec<u8>>,
    },
}

/// The `options` of `php_stream_open_wrapper_ex` a wrapper's opener
/// reads and hands on to an open of its own, [`Request::open_stream_with`]:
/// `USE_PATH`, the path looked for along `include_path` first;
/// `REPORT_ERRORS`, a refusal a warning; `STREAM_OPEN_FOR_INCLUDE`, the
/// open an `include` makes; `STREAM_MUST_SEEK`, a stream whose kind
/// cannot seek copied into one that can, `php_stream_make_seekable`; and
/// `STREAM_WILL_CAST`, the caller about to cast the stream to a fd, which
/// makes that copy a temporary file rather than a `php://temp`;
/// `STREAM_DISABLE_OPEN_BASEDIR`, the plain files wrapper opening without
/// the guard, as `mail.log` is opened; `STREAM_ONLY_GET_HEADERS`, the
/// open `get_headers` makes, for which the http wrapper reads the head
/// alone and fails on no status; and `STREAM_OPEN_FOR_ZEND_STREAM`, the
/// open an include makes for the scanner, whose caller holds the
/// `*opened_path` the opener sets: an opener that hands it on to its own
/// open hands on that `opened_path`, so the name the inner open sets is
/// the opener's, as `php_stream_gzopen` names an included file by the
/// file it read. The
/// engine clears `warn` before it asks an opener, as php does, so what
/// fails inside one is what the opener logged
/// ([`Request::wrapper_log_error`]), or the generic `operation failed`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // php's options, one flag per bit
pub struct Opening {
    pub use_path: bool,
    pub warn: bool,
    pub for_include: bool,
    pub must_seek: bool,
    pub will_cast: bool,
    pub disable_open_basedir: bool,
    pub only_get_headers: bool,
    /// `STREAM_OPEN_FOR_ZEND_STREAM`: the open an `include` makes for the
    /// engine's scanner, whose caller holds the `*opened_path` the opener
    /// sets. An opener that hands its `Opening` on to an open of its own,
    /// as `php://filter` opens its resource, hands on that `*opened_path`
    /// with it, as php's openers pass theirs: the name the inner open
    /// sets is the opener's own.
    pub for_zend_stream: bool,
}

/// A wrapper whose kinds the layer keeps, as php's `main/streams/`
/// does, which the extension that registers it in php, `standard`,
/// registers here by name at its place in `PHP_MINIT(basic)`'s order
/// (`Z/ext/standard/basic_functions.c:341`), so `url_stream_wrappers_hash`
/// has one order for the layer's wrappers and every extension's.
/// `php://` is `standard`'s own, a [`StreamWrapper`] as php keeps it in
/// `ext/standard/php_fopen_wrapper.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineWrapper {
    /// `file://`, `php_plain_files_wrapper`.
    Plain,
    /// `data:`, `php_stream_rfc2397_wrapper`.
    Data,
}

/// One wrapper operation past opening, php's `php_stream_wrapper_ops`
/// from `unlink` on, as [`Request::wrapper_op`] hands it to the wrapper
/// of a path: `stream_metadata`'s options are the last four, with an
/// owner or a group already a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperOp<'a> {
    /// `unlink`.
    Unlink,
    /// `rename`, to the path `to` as the program wrote it.
    Rename { to: &'a [u8] },
    /// `stream_mkdir` with its `mode` and `PHP_STREAM_MKDIR_RECURSIVE`.
    Mkdir { mode: i64, recursive: bool },
    /// `stream_rmdir`.
    Rmdir,
    /// `PHP_STREAM_META_TOUCH`: the modification and access times, or
    /// the current time for both.
    Touch(Option<(i64, i64)>),
    /// `PHP_STREAM_META_ACCESS`, the permission bits `chmod(2)` sets.
    Chmod(i64),
    /// `PHP_STREAM_META_OWNER`, a user id.
    Owner(i64),
    /// `PHP_STREAM_META_GROUP`, a group id.
    Group(i64),
    /// `PHP_STREAM_META_OWNER_NAME`, a user's name, which `chown` hands a
    /// wrapper that is not the plain files one as it was given.
    OwnerName(&'a [u8]),
    /// `PHP_STREAM_META_GROUP_NAME`, a group's name, as `chgrp` hands it.
    GroupName(&'a [u8]),
}

/// `php_stream_bucket_brigade`: the buckets a filter is handed and the
/// ones it hands on, each bucket its bytes, in order.
pub type Brigade = std::collections::VecDeque<Vec<u8>>;

/// `php_stream_filter_status_t`, what a filter answers: `PSFS_ERR_FATAL`,
/// the stream broken; `PSFS_FEED_ME`, nothing to hand on yet;
/// `PSFS_PASS_ON`, buckets on the `output` brigade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterStatus {
    ErrFatal,
    FeedMe,
    PassOn,
}

/// Which of a stream's two filter chains, `stream->readfilters` or
/// `stream->writefilters`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterChain {
    Read,
    Write,
}

/// `PSFS_FLAG_NORMAL`, `PSFS_FLAG_FLUSH_INC` and `PSFS_FLAG_FLUSH_CLOSE`:
/// whether a filter call is one more bucket, a flush, or the last call
/// before the stream closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterFlags {
    Normal,
    FlushInc,
    FlushClose,
}

/// `php_stream_filter_ops` (`Z/main/streams/php_stream_filter_api.h`):
/// one filter on one of a stream's chains, holding what its factory
/// made it with. The filter takes the buckets of `input` it consumes,
/// adds to `consumed` how many bytes they held where it is asked, and
/// puts what it makes on `output`; a call may serve PHP code, whose
/// throwable the `Err` is and which the stream treats as
/// `PSFS_ERR_FATAL`.
pub trait StreamFilter: Send {
    /// `fops->filter`, on the stream it is attached to.
    fn filter(
        &mut self,
        req: &mut Request,
        stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown>;

    /// `fops->dtor`, served once when the filter leaves its chain, a
    /// throwable pending or not: it gives up what the filter holds, and
    /// serves no PHP code while [`Diagnostics::stream_thrown`] holds one, as
    /// `zend_call_function` serves none under `EG(exception)`.
    fn close(&mut self, _req: &mut Request) -> Result<(), Thrown> {
        Ok(())
    }

    /// A second filter over this one's state for one call, closed after
    /// it, where PHP code a call of this filter serves reads its own
    /// stream and so calls the filter again. php has no such method: it
    /// re-enters the same filter in place (`userfilter_filter`,
    /// `Z/ext/standard/user_filters.c:124`, called again from the chain
    /// walk of `Z/main/streams/filter.c`), where the chain here lends a
    /// filter mutably for its call, so `share` is how a filter that can be
    /// re-entered says so: `Some` for the user filter, whose state is the
    /// PHP object, and `None` for a filter with native state, which a call
    /// made inside its own finds in use.
    fn share(&self, _req: &mut Request) -> Option<Box<dyn StreamFilter>> {
        None
    }
}

/// `php_stream_filter_factory`'s `create_filter`: a filter of the name
/// the program asked for, which may be one a wildcard matched, with the
/// parameters it was given, and `persistent`, whether the stream it is
/// made for is (`php_stream_is_persistent`), so that a factory keeps what
/// a persistent stream's filter allocates off the request's heap, as
/// `pemalloc(.., persistent)` does; `None` is the factory's refusal,
/// after whatever it said.
pub type FilterFactory =
    fn(&mut Request, &[u8], Option<&Value>, bool) -> Result<Option<Box<dyn StreamFilter>>, Thrown>;

/// No declared type, the stream `PHP_Z_PARAM_STREAM` holds: a resource,
/// as [`Resource`] holds one, and then an open stream
/// ([`Stream::from_value`]); a closed resource or one of another type is
/// php's "must be an open stream resource".
impl FromArgument for Stream {
    const MASK: u32 = <Resource as FromArgument>::MASK;
    fn declared() -> Option<Type> {
        None
    }
    fn accepts(value: &Value) -> bool {
        Resource::accepts(value)
    }
    fn from_value(req: &mut Request, value: Value) -> Result<Self, Thrown> {
        match Stream::from_value(req, &value) {
            Some(stream) => Ok(stream),
            None => Err(fatal(
                req,
                "InternalError: a held argument is not the open stream its declaration names",
            )),
        }
    }
    fn hold(req: &mut Request, given: Value, position: u32) -> Result<Option<Self>, Thrown> {
        let Some(resource) = Resource::hold(req, given, position)? else {
            return Ok(None);
        };
        match Stream::from_value(req, &resource.as_value()) {
            Some(stream) => Ok(Some(stream)),
            None => Err(arg_type_error(req, position, "be an open stream resource")),
        }
    }
}

/// The streams facet of the request group (design §4.3 seam 5, §4.4):
/// what a native opens, stats, registers and closes through the request,
/// php's `php_stream_open_wrapper_ex` and its siblings. Implemented for
/// [`Request`] alone; a native names it to call its methods.
pub trait Streams {
    /// `php://stdin`, which is what the CLI's `STDIN` constant holds: the
    /// host's fd 0 where it handed one, else nothing.
    fn stdin(&mut self) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `wait` with each of `streams` taken out of its resource for its
    /// length, as one operation on a stream takes it, an owner of the
    /// resource held, and put back after. A callback a lent wait serves
    /// ([`Limits::blocking`]) that `fclose`s one of them closes its
    /// resource, and the stream, its fd with it, is freed as the wait
    /// returns, so the fds a wait stands on stay open until it does, as
    /// php's, during whose wait nothing runs (#2664). A stream that is
    /// closed, not a stream, or out already, named twice among them, is
    /// passed over.
    fn holding_streams<R>(&mut self, streams: &[Stream], wait: impl FnOnce(&mut Request) -> R)
    -> R;

    /// `php_stream_open_wrapper_ex(path, mode, REPORT_ERRORS | USE_PATH?)`,
    /// what `fopen()` is: the stream, or `None` after php's warning for
    /// the open that failed (`fopen` answers `false`); the `Err` is a
    /// throwable, php's `ValueError` for an empty path or the memory
    /// limit. The wrapper is the path's scheme's, `php://` or the plain
    /// files wrapper for none and for `file://`, or one the program
    /// registered.
    ///
    /// ```php
    /// var_dump(fopen("nope.txt", "r"), fopen(__FILE__, "r"), fopen("file://" . __DIR__ . "/witness.php", "r"), fopen("php://memory", "r"), fopen("foo://x", "r"), fopen("php://nope", "r"), fopen("file://host/x", "r"), fopen("x", "q"));
    /// try { fopen("", "r"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Warning: fopen(nope.txt): Failed to open stream: No such file or directory in %s on line 2
    ///
    /// Warning: fopen(): Unable to find the wrapper "foo" - did you forget to enable it when you configured PHP? in %s on line 2
    ///
    /// Warning: fopen(foo://x): Failed to open stream: No such file or directory in %s on line 2
    ///
    /// Warning: fopen(): Invalid php:// URL specified in %s on line 2
    ///
    /// Warning: fopen(php://nope): Failed to open stream: operation failed in %s on line 2
    ///
    /// Warning: fopen(): Remote host file access not supported, file://host/x in %s on line 2
    ///
    /// Warning: fopen(file://host/x): Failed to open stream: no suitable wrapper could be found in %s on line 2
    ///
    /// Warning: fopen(x): Failed to open stream: `q' is not a valid mode for fopen in %s on line 2
    /// bool(false)
    /// resource(%d) of type (stream)
    /// resource(%d) of type (stream)
    /// resource(%d) of type (stream)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// Path must not be empty
    /// ```
    fn open_stream(
        &mut self,
        path: &[u8],
        mode: &[u8],
        use_include_path: bool,
    ) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `php_stream_open_wrapper_ex(path, mode, options)` under the
    /// options an opener was handed, or a native chose: what
    /// [`Request::open_stream`] is with `REPORT_ERRORS` and `USE_PATH`
    /// each as `opening` says.
    fn open_stream_with(
        &mut self,
        path: &[u8],
        mode: &[u8],
        opening: Opening,
    ) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `php_stream_open_wrapper_ex(url, mode, REPORT_ERRORS |
    /// STREAM_USE_URL)`, what `get_headers()` opens: [`Request::open_stream`]
    /// for a path whose wrapper is a URL wrapper, and for any other php's
    /// warning that the function may only be used against URLs, and
    /// `None`.
    ///
    /// ```php
    /// var_dump(get_headers(__FILE__), get_headers("php://memory"), get_headers("foo://x"));
    /// ```
    /// ```expect
    /// Warning: get_headers(): This function may only be used against URLs in %s on line 2
    ///
    /// Warning: get_headers(): This function may only be used against URLs in %s on line 2
    ///
    /// Warning: get_headers(): Unable to find the wrapper "foo" - did you forget to enable it when you configured PHP? in %s on line 2
    ///
    /// Warning: get_headers(): This function may only be used against URLs in %s on line 2
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    fn open_url(&mut self, url: &[u8], mode: &[u8]) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `php_stream_alloc(ops, abstract, 0, mode)`: a stream of a kind the
    /// extension implements, as a resource of the `stream` type with one
    /// owner, the caller's, at position zero with nothing read.
    fn new_stream(&mut self, ops: Box<dyn StreamOps>, mode: &[u8])
    -> Result<Owner<Stream>, Thrown>;

    /// `php_stream_memory_create(php_stream_mode_from_str(mode))`: an
    /// empty memory stream, read only unless `mode` has a `w` or a `+`,
    /// and appending where it has an `a`.
    fn memory_stream(&mut self, mode: &[u8]) -> Result<Owner<Stream>, Thrown>;

    /// `php_stream_temp_create(php_stream_mode_from_str(mode),
    /// max_memory)`: a memory stream that spills to a temporary file past
    /// `max_memory` bytes, `PHP_STREAM_MAX_MEM` where none is named.
    fn temp_stream(
        &mut self,
        mode: &[u8],
        max_memory: Option<usize>,
    ) -> Result<Owner<Stream>, Thrown>;

    /// `php://input`, `php_stream_input_ops` over the request's body: read
    /// only, seekable, labelled `Input`.
    fn input_stream(&mut self) -> Result<Owner<Stream>, Thrown>;

    /// One of the host's standard streams as `php://stdin`,
    /// `php://stdout` and `php://stderr` open it: nothing once a program
    /// closed the constant over it, a copy of the host's fd where it
    /// handed one to be read or the constants stand over the process's
    /// own, and otherwise the host's body and sink, which do not seek.
    /// A copy that fails is logged as a wrapper's opener logs it
    /// ([`Request::wrapper_log_error`]), so this is an opener's to ask.
    fn standard_stream(
        &mut self,
        standard: Standard,
        mode: &[u8],
    ) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `dup(fd)` and `php_stream_fopen_from_fd` over the copy, what
    /// `php://fd/N` opens: `fd` below `getdtablesize()`, the host's own
    /// for 0, 1 and 2 where it handed them, a socket as a socket and
    /// anything else as a plain file. The refusals, an `fd` out of range
    /// and a `dup` that fails, are php's messages logged as a wrapper's
    /// opener logs them, so this is an opener's to ask.
    fn fd_stream(&mut self, fd: i64, mode: &[u8]) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `opendir()`, `php_stream_opendir` under `REPORT_ERRORS`: the
    /// directory as a stream of the `stream` type, through the plain
    /// files wrapper's `php_plain_files_dirstream_ops` or a registered
    /// wrapper's directory opener, and `None` after php's `Failed to
    /// open directory: ` warning, which an empty path is not given.
    fn open_dir(&mut self, path: &[u8]) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `php_stream_stat_path_ex`, and with `cached` the stat `php_stat`
    /// makes: the path's wrapper located and its `url_stat` asked,
    /// `lstat(2)` where `link` and `stat(2)` otherwise for a plain file,
    /// laid out as `php_fstat` lays it out, a plain file's through the
    /// `open_basedir` guard first, which warns unless the stat is quiet
    /// and always for `php_stat`'s own. With `cached` the answer comes
    /// from, and goes into, `BG(CurrentStatFile)` or
    /// `BG(CurrentLStatFile)` by the path as written, which
    /// [`Request::clear_stat_cache`], a plain file's read, write or open,
    /// and every operation of [`Request::wrapper_op`] that changed a
    /// file clear.
    fn url_stat(&mut self, path: &[u8], link: bool, cached: bool) -> Result<UrlStat, Thrown>;

    /// [`Request::url_stat`] with `cached`, as `php_stat` makes it for an
    /// existence check, `file_exists` and the `is_*` natives: a
    /// user-space wrapper's `url_stat` is handed
    /// `STREAM_URL_STAT_QUIET` beside the link bit.
    fn url_stat_quiet(&mut self, path: &[u8], link: bool) -> Result<UrlStat, Thrown>;

    /// `php_stream_locate_url_wrapper(path, &local, 0)` and the located
    /// wrapper's `url_stat` under `PHP_STREAM_URL_STAT_QUIET`, asked
    /// directly as `php_libxml_streams_IO_open_wrapper` asks it before
    /// it opens a path: no warning from the locate or the stat, the stat
    /// cache neither read nor written, and a wrapper with no `url_stat`
    /// told apart from a stat that failed.
    fn wrapper_url_stat(&mut self, path: &[u8]) -> Result<UrlStat, Thrown>;

    /// `php_clear_stat_cache`: the stat cache emptied, both entries, and
    /// the realpath cache as `realpaths` says.
    fn clear_stat_cache(&mut self, realpaths: Realpaths<'_>);

    /// What `chdir()` does to the stat cache past moving the working
    /// directory (`PHP_FUNCTION(chdir)`, `Z/ext/standard/dir.c:367`): an
    /// entry whose path is relative is dropped, since it now names
    /// another file.
    fn forget_relative_stats(&mut self);

    /// `php_stream_locate_url_wrapper(path, &local, 0) ==
    /// &php_plain_files_wrapper`: the path the plain files wrapper would
    /// be handed, `file://` taken off, or `None` where the path is
    /// another wrapper's or none's, after the warnings the locate raises.
    fn plain_path(&mut self, path: &[u8]) -> Option<Vec<u8>>;

    /// One [`WrapperOp`] at `path`, as `unlink`, `rename`, `mkdir`,
    /// `rmdir`, and `touch`, `chmod`, `chown` and `chgrp` over a wrapper
    /// reach it: the wrapper located, php's warning where there is none
    /// or it lacks the operation, else the wrapper's own answer, the
    /// plain files wrapper's with php's warnings, `strerror(errno)` among
    /// them. `mkdir` and `rmdir` of a wrapper without them answer
    /// `false` and say nothing, as `php_stream_mkdir` does.
    fn wrapper_op(&mut self, path: &[u8], op: WrapperOp<'_>) -> Result<bool, Thrown>;

    /// `tempnam()`'s `php_open_temporary_fd_ex` under
    /// `PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ALWAYS`: a new `0600` file in
    /// `dir`, or after php's notice in the temporary directory, named by
    /// the prefix and nineteen random characters, and closed; its path,
    /// or `None` where none could be made.
    fn temporary_name(
        &mut self,
        dir: &[u8],
        prefix: &[u8],
    ) -> Result<Option<Owner<PhpString>>, Thrown>;

    /// `f` with `ctx` as the `php_stream_context` every stream operation
    /// it makes is handed, the last argument of php's
    /// `php_stream_open_wrapper_ex`, `php_stream_opendir` and the wrapper
    /// operations: what a native resolved its `$context` to, the
    /// `stream-context` resource given or `FG(default_context)`, or
    /// `None`. A user-space wrapper's instance is given it as its
    /// `$context` property, as `user_stream_create_object` sets it; the
    /// one in effect before is back when `f` returns.
    ///
    /// ```php
    /// class W { public $context;
    ///     function stream_open($p, $m, $o, &$op) { var_dump($this->context, stream_context_get_options($this->context)); return true; }
    ///     function unlink($p) { var_dump(stream_context_get_options($this->context)); return true; } }
    /// stream_wrapper_register('ww', 'W');
    /// $c = stream_context_create(['ww' => ['k' => 'v']]);
    /// fopen('ww://x', 'r', false, $c);
    /// fopen('ww://x', 'r');
    /// unlink('ww://x', $c);
    /// class T { public Exception $context; }
    /// stream_wrapper_register('tt', 'T');
    /// try { opendir('tt://'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// resource(%d) of type (stream-context)
    /// array(1) {
    ///   ["ww"]=>
    ///   array(1) {
    ///     ["k"]=>
    ///     string(1) "v"
    ///   }
    /// }
    /// resource(%d) of type (stream-context)
    /// array(0) {
    /// }
    /// array(1) {
    ///   ["ww"]=>
    ///   array(1) {
    ///     ["k"]=>
    ///     string(1) "v"
    ///   }
    /// }
    /// Cannot assign resource to property T::$context of type Exception
    /// ```
    fn with_stream_ctx<R>(
        &mut self,
        ctx: Option<StreamCtx>,
        f: impl FnOnce(&mut Request) -> R,
    ) -> R;

    /// The `php_stream_context *context` a wrapper's opener is handed:
    /// what [`Request::with_stream_ctx`] set around the operation being
    /// served, `None` outside one and where the native resolved its
    /// `$context` to none. An extension's wrapper reads its options off
    /// it, as `php_stream_gzopen` reads `zlib`'s `level`.
    #[must_use]
    fn stream_ctx(&self) -> Option<StreamCtx>;

    /// `php_stream_wrapper_log_error` from a wrapper's opener, which php's
    /// streams layer hands `options` without `REPORT_ERRORS`: `message`
    /// kept for `php_stream_display_wrapper_errors`, which prints what
    /// the opener logged, one message a line, after `Failed to open
    /// stream: ` or `Failed to open directory: ` where it opened nothing,
    /// and drops it where it opened something. An operation php hands
    /// `REPORT_ERRORS`, as `unlink` and `mkdir` do, warns instead.
    ///
    /// ```php
    /// var_dump(@fopen('phar://nope.phar/a', 'r'), error_get_last()['message']);
    /// var_dump(@opendir('phar://nope'), error_get_last()['message']);
    /// ```
    /// ```expect
    /// bool(false)
    /// string(%d) "fopen(phar://nope.phar/a): Failed to open stream: phar error: invalid url or non-existent phar "phar://nope.phar/a""
    /// bool(false)
    /// string(%d) "opendir(phar://nope): Failed to open directory: phar error: no directory in "phar://nope", must have at least phar://nope/ for root directory (always use full path to a new phar)
    /// phar url "phar://nope" is unknown"
    /// ```
    fn wrapper_log_error(&mut self, message: &str);

    /// `*opened_path = ...` from a wrapper's opener: the name the stream
    /// it opens stands for, which an `include` of it compiles the file
    /// under and lists in `get_included_files()`, as `zend_compile_file`
    /// takes `file_handle->opened_path` over the path as written. phar's
    /// opener names an entry `phar://<archive>/<entry>` and the stub of
    /// a bare archive the archive itself.
    ///
    /// ```php
    /// $p = __DIR__ . '/opened-path.phar';
    /// $c = '<?php return __FILE__;';
    /// $m = pack('V', 5) . 'a.php' . pack('VVVVVV', strlen($c), 0, strlen($c), crc32($c), 0x1b6, 0);
    /// $m = pack('VnVV', 1, 0x1110, 0x10000, 0) . pack('V', 0) . $m;
    /// $data = '<?php return __FILE__; __HALT_COMPILER(); ?>' . "\r\n" . pack('V', strlen($m)) . $m . $c;
    /// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
    /// echo str_replace(__DIR__, 'D', implode("\n", [include "phar://$p/./a.php", include "phar://$p"])), "\n";
    /// echo str_replace(__DIR__, 'D', implode("\n", array_filter(get_included_files(), fn($f) => str_contains($f, 'opened-path')))), "\n";
    /// unlink($p);
    /// ```
    /// ```expect
    /// phar://D/opened-path.phar/a.php
    /// D/opened-path.phar
    /// phar://D/opened-path.phar/a.php
    /// D/opened-path.phar
    /// ```
    fn set_opened_path(&mut self, path: &[u8]);

    /// `php_stream_fopen_tmpfile`, what `tmpfile()` is: a new file in the
    /// temporary directory, opened `r+b` as a plain file and unlinked
    /// when it closes, or `None` where none could be made.
    fn tmpfile(&mut self) -> Result<Option<Owner<Stream>>, Thrown>;

    /// `php_register_url_stream_wrapper`: the wrapper at the end of the
    /// table every request starts with; `false` when the scheme is taken.
    fn register_wrapper(&mut self, wrapper: StreamWrapper) -> bool;

    /// `php_register_url_stream_wrapper` of a wrapper the layer keeps,
    /// under the scheme php registers it by, `file` or `data`: its row
    /// moved to the end of the table, where [`Request::register_wrapper`]
    /// puts one. The layer's table has the two from the start, so a
    /// bundle without the extension that registers them still opens a
    /// plain file; the registration is what gives each its place among
    /// the extensions' wrappers.
    /// A wrapper restored goes back at the end, after one a program
    /// registered meanwhile, as php's `stream_wrapper_restore` deletes
    /// and adds it.
    ///
    /// ```php
    /// class W { public $context; }
    /// $ours = ['php', 'file', 'glob', 'data', 'mine'];
    /// echo implode(',', array_values(array_intersect(stream_get_wrappers(), $ours))), "\n";
    /// stream_wrapper_unregister('data');
    /// stream_wrapper_register('mine', 'W');
    /// stream_wrapper_restore('data');
    /// echo implode(',', array_values(array_intersect(stream_get_wrappers(), $ours))), "\n";
    /// ```
    /// ```expect
    /// php,file,glob,data
    /// php,file,glob,mine,data
    /// ```
    fn register_engine_wrapper(&mut self, which: EngineWrapper);

    /// `stream_get_wrappers()`: the schemes of
    /// `php_stream_get_url_stream_wrappers_hash()`, the request's own
    /// table once a program changed it, with what `stream_wrapper_register`
    /// added and `stream_wrapper_unregister` took out, and the built-in
    /// one before, in the order each row went in.
    #[must_use]
    fn stream_wrappers(&self) -> Vec<String>;

    /// `php_stream_filter_register_factory_volatile`
    /// (`Z/main/streams/filter.c:50`): `factory` for the filter `name`,
    /// which may end in `.*` to take every name under it, for the rest of
    /// the request; `false` where the name is taken.
    fn register_filter(&mut self, name: &[u8], factory: FilterFactory) -> bool;

    /// `stream_get_filters()`'s names: every filter registered, in the
    /// order they were.
    #[must_use]
    fn stream_filters(&self) -> Vec<Vec<u8>>;

    /// `_php_stream_fopen_from_fd` (`Z/main/streams/plain_wrapper.c:303`)
    /// and `_php_stream_fopen_from_pipe` (`:350`): an open host fd as a
    /// stream of the plain files kind, `STDIO`, with no wrapper and no
    /// path, `mode` as `stream->mode`. A fd of a pipe, a fifo or a
    /// character device cannot seek (`detect_is_seekable`) and has no
    /// position; any other has the one `lseek(SEEK_CUR)` answers. A
    /// process pipe never seeks, tells the zero it opened at, and its
    /// close waits for the child.
    ///
    /// ```php
    /// $p = popen('true', 'r');
    /// var_dump(ftell($p), pclose($p));
    /// ```
    /// ```expect
    /// int(0)
    /// int(0)
    /// ```
    ///
    /// The `Err` is the memory limit, met registering the resource.
    fn stream_from_fd(&mut self, fd: OwnedFd, kind: FdStream<'_>) -> Result<Owner<Stream>, Thrown>;

    /// `stream_wrapper_register()` (`Z/main/streams/userspace.c:437`) past
    /// its parameters: `class` registered under `protocol` for the rest of
    /// the request, a URL wrapper where `flags` holds `STREAM_IS_URL`;
    /// php's `Protocol %s:// is already defined.` where the scheme is
    /// taken and `Invalid protocol scheme specified` where it is not one.
    ///
    /// The registration is a resource of php's `le_protocols` type,
    /// `stream factory`, made before the scheme is tried
    /// (`zend_register_resource(uwrap, le_protocols)`), so it spends an id
    /// whether or not the scheme takes it:
    ///
    /// ```php
    /// var_dump(fopen('php://memory', 'r'));
    /// class F { public $context; }
    /// stream_wrapper_register('ff', 'F');
    /// var_dump(fopen('php://memory', 'r'));
    /// ```
    /// ```expect
    /// resource(5) of type (stream)
    /// resource(7) of type (stream)
    /// ```
    ///
    /// The resource stands in `get_resources()` while the registration or
    /// a stream it opened holds it: a refused registration lets it go at
    /// once, `stream_wrapper_unregister()` lets the registration's go
    /// (`zend_list_delete`), and the stream's goes with its close
    /// (`user_wrapper_close`). `stream_wrapper_restore()` over a user
    /// wrapper lets nothing go, so its resource stands to the request's end.
    ///
    /// ```php
    /// class F { public $context; function stream_open($p, $m, $o, &$op) { return true; } }
    /// var_dump(@stream_wrapper_register('a b', 'F'), stream_wrapper_register('ff', 'F'), get_resources('stream factory'));
    /// $s = fopen('ff://x', 'r');
    /// stream_wrapper_unregister('ff');
    /// var_dump(get_resources('stream factory'));
    /// fclose($s);
    /// var_dump(get_resources('stream factory'));
    /// stream_wrapper_unregister('file'); stream_wrapper_register('file', 'F'); stream_wrapper_restore('file');
    /// var_dump(get_resources('stream factory'));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// array(1) {
    ///   [5]=>
    ///   resource(5) of type (stream factory)
    /// }
    /// array(1) {
    ///   [5]=>
    ///   resource(5) of type (stream factory)
    /// }
    /// array(0) {
    /// }
    /// array(1) {
    ///   [8]=>
    ///   resource(8) of type (stream factory)
    /// }
    /// ```
    ///
    /// The `Err` is the memory limit, met making the resource.
    fn wrapper_register(
        &mut self,
        protocol: &[u8],
        class: ClassId,
        flags: i64,
    ) -> Result<bool, Thrown>;

    /// `stream_wrapper_unregister()` (`Z/main/streams/userspace.c:473`):
    /// the scheme taken out of the request's table, a built-in one
    /// among them, and php's `Unable to unregister protocol %s://` where
    /// it is not there.
    fn wrapper_unregister(&mut self, protocol: &[u8]) -> bool;

    /// `stream_wrapper_restore()` (`Z/main/streams/userspace.c:499`): the
    /// built-in wrapper put back under its scheme; php's `%s:// never
    /// existed, nothing to restore` for a scheme no built-in has, and its
    /// notice `%s:// was never changed, nothing to restore`, `true`, where
    /// the table has the built-in one already.
    fn wrapper_restore(&mut self, protocol: &[u8]) -> bool;

    /// `stream_filter_remove`'s work (`Z/ext/standard/streamsfuncs.c:1298`)
    /// past its parameters: the filter a `stream filter` resource names
    /// flushed to the chain's end as the stream closing would flush it,
    /// php's `Unable to flush filter, not removing` and `false` where that
    /// breaks; then the resource closed and the filter taken off its
    /// chain and freed. `None` is a resource of another type, or closed,
    /// which is php's `TypeError` for the native to raise.
    fn remove_filter(&mut self, res: Resource) -> Result<Option<bool>, Thrown>;

    /// `stream_get_transports()`: the registered `transports` in the order
    /// they were registered, `tcp`, `udp`, `unix` and `udg`, then what the
    /// bundle's extensions registered, in bundle order, a name already
    /// there taken over in its place as `zend_hash_update` keeps it.
    ///
    /// ```php
    /// echo implode(',', array_slice(stream_get_transports(), 0, 4)), "\n";
    /// ```
    /// ```expect
    /// tcp,udp,unix,udg
    /// ```
    #[must_use]
    fn transports(&self) -> Vec<&'static str>;

    /// `_php_stream_xport_create` (`Z/main/streams/transports.c:52`) under
    /// `REPORT_ERRORS` with an error string asked for: the `transport` read
    /// off the front of `name`, `tcp` where it names none, one nobody
    /// registered being php's `Unable to find the socket transport`; for
    /// one an extension registered, its crypto layer made first, as the
    /// factory makes the stream; then a client connected to the rest of
    /// the name, within `timeout`, where it asked to connect, or a server
    /// bound to it, and listening where `listen` says. A client that did
    /// not ask holds no socket, php's `sock->socket == -1`, the rest of the
    /// name not parsed: a write takes nothing, a read fails, and it is at
    /// its end. The stream is `r+`, of the `transport`'s kind, with the
    /// name as written as its `uri`, the `context` as its own and
    /// `default_socket_timeout` as its read timeout; a connected client's
    /// layer is then asked [`Crypto::connected`], and one that refuses
    /// fails the connect with no text. A failure is the step that failed,
    /// its error code and its text, which a caller that asks php for an
    /// `error_string` prints in its own words and one that does not
    /// raises as [`XportFailure::warning`]; the `Err` is the memory limit
    /// or what the layer's factory threw.
    fn xport_create(
        &mut self,
        name: &[u8],
        xport: Xport,
    ) -> Result<Result<Owner<Stream>, XportFailure>, Thrown>;

    /// The connection [`OwnedStream::keep`] took out lent to this request:
    /// a fresh resource of the `persistent stream` type over its socket,
    /// `r+` and seeking nowhere, in neither of the request's lists
    /// ([`OwnedStream::unlist`]), as php's persistent mysqlnd connection
    /// carries its `php_stream` between requests in none, until its owner
    /// keeps it again or frees it.
    fn stream_lend(&mut self, kept: Persisted) -> Result<Owner<Stream>, Thrown>;
}

/// What a native does with the owner of a stream it opened, which it
/// keeps as an [`Owner<Stream>`] for as long as it holds the stream: the
/// stream's own exits, beside [`Owner::give_up`], which lets the owner go
/// and leaves the stream open for its other owners.
pub trait OwnedStream {
    /// `php_stream_close`, `_php_stream_free` under
    /// `PHP_STREAM_FREE_CLOSE`: [`Stream::close`], then the owner given
    /// up, `zend_list_delete`, so `get_resources()` lists the stream no
    /// longer once no value holds it.
    fn free(self, req: &mut Request);

    /// The persistent stream's connection taken out of the request for
    /// the extension that opened it to keep across requests, in the one
    /// list it keeps its connection in, as php's persistent mysqlnd
    /// connection keeps its `php_stream`: the socket, its owed write
    /// flushed, out of the resource, which is closed, and the owner given
    /// up. The request that takes the connection up lends it again with
    /// [`Streams::stream_lend`]. `None`, and the stream freed, for a
    /// stream that is not a socket's.
    #[must_use]
    fn keep(self, req: &mut Request) -> Option<Persisted>;

    /// What php's mysqlnd does to the stream it opens
    /// (`mysqlnd_vio::open_tcp_or_unix`, `mysqlnd_fixup_regular_list`):
    /// the stream deleted from `EG(regular_list)` and, a persistent one,
    /// from `EG(persistent_list)`, each without its destructor, so it is
    /// the owner's alone. `get_resources()` no longer lists it, its id
    /// stays spent, and the request's end neither closes it nor holds a
    /// persistent one's socket idle under its key; the owner frees it
    /// ([`OwnedStream::free`]) or keeps it ([`OwnedStream::keep`]).
    fn unlist(&self, req: &mut Request);
}

impl OwnedStream for Owner<Stream> {
    fn free(self, req: &mut Request) {
        self.close(req);
        self.give_up(req);
    }

    fn keep(self, req: &mut Request) -> Option<Persisted> {
        crate::streams::persistent::keep(req, self).map(Persisted)
    }

    fn unlist(&self, req: &mut Request) {
        crate::streams::persistent::unlist(req, **self);
    }
}

impl Streams for Request {
    fn stdin(&mut self) -> Result<Option<Owner<Stream>>, Thrown> {
        self.open_stream(b"php://stdin", b"r", false)
    }

    fn holding_streams<R>(
        &mut self,
        streams: &[Stream],
        wait: impl FnOnce(&mut Request) -> R,
    ) -> R {
        self.streams_held(streams, wait)
    }

    fn open_stream(
        &mut self,
        path: &[u8],
        mode: &[u8],
        use_include_path: bool,
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        self.open_stream_wrapper(path, mode, use_include_path)
    }

    fn open_stream_with(
        &mut self,
        path: &[u8],
        mode: &[u8],
        opening: Opening,
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        self.open_wrapper_with(path, mode, opening)
    }

    fn open_url(&mut self, url: &[u8], mode: &[u8]) -> Result<Option<Owner<Stream>>, Thrown> {
        self.open_url_wrapper(url, mode)
    }

    fn new_stream(
        &mut self,
        ops: Box<dyn StreamOps>,
        mode: &[u8],
    ) -> Result<Owner<Stream>, Thrown> {
        let ops = crate::supplied::kind(self, ops);
        self.stream_alloc(crate::streams::of_extension(ops, mode))
    }

    fn memory_stream(&mut self, mode: &[u8]) -> Result<Owner<Stream>, Thrown> {
        crate::streams::open_memory(self, mode)
    }

    fn temp_stream(
        &mut self,
        mode: &[u8],
        max_memory: Option<usize>,
    ) -> Result<Owner<Stream>, Thrown> {
        crate::streams::open_temp(self, mode, max_memory)
    }

    fn input_stream(&mut self) -> Result<Owner<Stream>, Thrown> {
        crate::streams::open_input(self)
    }

    fn standard_stream(
        &mut self,
        standard: Standard,
        mode: &[u8],
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        crate::streams::open_standard(self, standard, mode)
    }

    fn fd_stream(&mut self, fd: i64, mode: &[u8]) -> Result<Option<Owner<Stream>>, Thrown> {
        crate::streams::open_fd(self, fd, mode)
    }

    fn open_dir(&mut self, path: &[u8]) -> Result<Option<Owner<Stream>>, Thrown> {
        self.open_dir_wrapper(path)
    }

    fn url_stat(&mut self, path: &[u8], link: bool, cached: bool) -> Result<UrlStat, Thrown> {
        self.stat_url(path, link, cached, false)
    }

    fn url_stat_quiet(&mut self, path: &[u8], link: bool) -> Result<UrlStat, Thrown> {
        self.stat_url(path, link, true, true)
    }

    fn wrapper_url_stat(&mut self, path: &[u8]) -> Result<UrlStat, Thrown> {
        Ok(match self.stat_url_quiet(path)? {
            UrlStat::Refused => UrlStat::Failed,
            answer => answer,
        })
    }

    fn clear_stat_cache(&mut self, realpaths: Realpaths<'_>) {
        match realpaths {
            Realpaths::Kept => self.forget_stats(),
            Realpaths::All => self.forget_stats_and_realpaths(),
            Realpaths::Of(path) => {
                self.forget_stats();
                self.environment_mut().realpath_cache_clear(Some(path));
            }
        }
    }

    fn forget_relative_stats(&mut self) {
        PlainFiles::forget_relative_stats(self);
    }

    fn plain_path(&mut self, path: &[u8]) -> Option<Vec<u8>> {
        self.plain_local(path)
    }

    fn wrapper_op(&mut self, path: &[u8], op: WrapperOp<'_>) -> Result<bool, Thrown> {
        self.path_op(path, op)
    }

    #[cfg_attr(debug_assertions, track_caller)]
    fn temporary_name(
        &mut self,
        dir: &[u8],
        prefix: &[u8],
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        match self.open_temporary_fd(Some(dir), prefix, TemporaryGuard::Always) {
            Some((_, path)) => Ok(Some(PhpString::new(
                self,
                path.as_os_str().as_encoded_bytes(),
            )?)),
            None => Ok(None),
        }
    }

    fn with_stream_ctx<R>(
        &mut self,
        ctx: Option<StreamCtx>,
        f: impl FnOnce(&mut Request) -> R,
    ) -> R {
        let outer = std::mem::replace(&mut self.streams().stream_ctx, ctx);
        let answer = f(self);
        self.streams().stream_ctx = outer;
        answer
    }

    fn stream_ctx(&self) -> Option<StreamCtx> {
        self.streams_ref().and_then(|state| state.stream_ctx)
    }

    fn wrapper_log_error(&mut self, message: &str) {
        self.streams().wrapper_errors.push(message.to_string());
    }

    fn set_opened_path(&mut self, path: &[u8]) {
        self.streams().opened_path = Some(path.to_vec());
    }

    fn tmpfile(&mut self) -> Result<Option<Owner<Stream>>, Thrown> {
        crate::streams::temporary_file(self, None, b"php")
    }

    fn register_wrapper(&mut self, wrapper: StreamWrapper) -> bool {
        let row = crate::streams::Registered {
            scheme: wrapper.scheme.as_bytes().to_vec(),
            wrapper: crate::streams::Located::Ext(self.streams().wrappers.len()),
        };
        if !self.add_url_wrapper(row) {
            return false;
        }
        let by = self.code().supplier();
        self.streams().wrappers.push(Supplied { row: wrapper, by });
        true
    }

    fn register_engine_wrapper(&mut self, which: EngineWrapper) {
        let row = crate::streams::Registered::engine(which);
        let rows = &mut self.streams().url_stream_wrappers;
        rows.retain(|held| held.scheme != row.scheme);
        rows.push(row);
    }

    fn stream_wrappers(&self) -> Vec<String> {
        self.wrapper_table()
            .into_iter()
            .map(|row| String::from_utf8_lossy(&row.scheme).into_owned())
            .collect()
    }

    fn register_filter(&mut self, name: &[u8], factory: FilterFactory) -> bool {
        let by = self.code().supplier();
        let rows = &mut self.streams().filters;
        if rows.iter().any(|(n, _)| n.as_slice() == name) {
            return false;
        }
        rows.push((name.to_vec(), Supplied { row: factory, by }));
        true
    }

    fn stream_filters(&self) -> Vec<Vec<u8>> {
        self.streams_ref().map_or_else(Vec::new, |state| {
            state.filters.iter().map(|(name, _)| name.clone()).collect()
        })
    }

    fn stream_from_fd(&mut self, fd: OwnedFd, kind: FdStream<'_>) -> Result<Owner<Stream>, Thrown> {
        StreamLayer::stream_from_fd(self, fd, kind)
    }

    fn wrapper_register(
        &mut self,
        protocol: &[u8],
        class: ClassId,
        flags: i64,
    ) -> Result<bool, Thrown> {
        StreamLayer::wrapper_register(self, protocol, class, flags)
    }

    fn wrapper_unregister(&mut self, protocol: &[u8]) -> bool {
        StreamLayer::wrapper_unregister(self, protocol)
    }

    fn wrapper_restore(&mut self, protocol: &[u8]) -> bool {
        StreamLayer::wrapper_restore(self, protocol)
    }

    fn remove_filter(&mut self, res: Resource) -> Result<Option<bool>, Thrown> {
        FilterLayer::remove_filter(self, res)
    }

    fn transports(&self) -> Vec<&'static str> {
        Transports::transports(self)
    }

    fn xport_create(
        &mut self,
        name: &[u8],
        xport: Xport,
    ) -> Result<Result<Owner<Stream>, XportFailure>, Thrown> {
        Transports::xport_create(self, name, xport)
    }

    fn stream_lend(&mut self, kept: Persisted) -> Result<Owner<Stream>, Thrown> {
        crate::streams::persistent::lend_alone(self, kept.0)
    }
}
