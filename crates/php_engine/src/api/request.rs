//! The request-environment group (design §4.4, §5.14): what a native
//! reaches through `&mut Request`, the one parameter that says a function
//! may re-enter PHP code (principle 9, ADR-0002). Ninety-eight methods,
//! `api.lock`'s largest group. The skeleton answers each from what the
//! request holds; the body bytes and the header block are kept on the
//! request and handed to the sink by `serve`, which owns the commit
//! moment; `serve` says what the request came to.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use php_compile::{CompileError, SourceKind, StackLimit};
use php_heap::{
    Arr, Fatal, FrameRef, Heap, HeapHooks, Intern, MapTable, Obj, Ref, Res, ResourceType, Slot,
    SortEntry, VmStack, Zval, ZvalRef,
};
use php_unit::decl::{
    Attribute, ClassConstDecl, ClassDecl, ClassKind, FunctionDecl, HookKind, IniDecl, MethodDecl,
    Name, PropertyDecl, Visibility,
};
use php_unit::{CacheSlot, FramelessEntry, OpArray, Opcode, Unit};

use crate::api::abi::{
    Callable, Dependency, DimensionRead, Extension, IniVerdict, NativeNew, ObjectHandlers,
    PropertyRead, PropertyTest, Std,
};
use crate::api::call::{Function, Target};
use crate::api::raising::{Level, Thrown};
use crate::api::values::{
    ClassId, ClosureInfo, Key, PhpArray, PhpObject, PhpRef, PhpString, Resource, TypeTag, Value,
    ValueRef,
};
use crate::engine::{
    Configured, ConfiguredKey, MethodAt, NativeMethod, OutputSink, RequestDescription, Shared,
};
use crate::exec::{self, CallStage, CallState, IncludeKind, TableId, Vm};
use crate::output::Output;
use crate::virtual_cwd::RealpathCache;
pub use crate::virtual_cwd::Resolve;

mod hold;
mod hooks;
mod lazy;
mod linking;
mod load_file;
mod table;
mod variables;

pub(crate) use hooks::parent_hook_trampolines;
pub(crate) use lazy::Strategy;

/// php's version, what `PHP_VERSION` and `phpversion()` say.
pub const PHP_VERSION: &str = "8.5.9";

/// `IN_GET`, `IN_SET`, `IN_UNSET` and `IN_ISSET`
/// (`Z/Zend/zend_object_handlers.c:43`, over
/// `ZEND_GUARD_PROPERTY_*`): the bit each magic accessor takes in the
/// property guard while it serves one name on one object, so that a
/// `__get` reading the name it was called for finds the guard standing
/// and falls through to the `Undefined property` warning instead of
/// reaching itself.
const IN_GET: u32 = 1 << 0;
const IN_SET: u32 = 1 << 1;
const IN_UNSET: u32 = 1 << 2;
const IN_ISSET: u32 = 1 << 3;

/// A path on the host's filesystem, absolute, after the request's `cwd`
/// and `open_basedir` have been applied.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct HostPath(pub PathBuf);

/// A linked extension, by identity: its position in the bundle's startup
/// order, which is where its `zend_module_entry` stands in
/// `module_registry`. `Core`, which php registers first, is no
/// `Extension` value of the bundle and has no identity here yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtensionId(pub(crate) u32);

/// One ini entry as this request sees it, php's `zend_ini_entry` read
/// by `_extension_ini_string`, `ReflectionExtension::getINIEntries` and
/// `ini_get_all`: the declaration with its `modifiable` bits, the
/// extension that registered it (`module_number`), none for the
/// engine's own table, the value in force, and the value the entry had
/// before the request changed it, php's `orig_value`, which is `Some`
/// exactly where php's `modified` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniEntry {
    pub decl: IniDecl,
    pub extension: Option<ExtensionId>,
    pub value: Box<[u8]>,
    pub original: Option<Box<[u8]>>,
}

/// One bucket of the realpath cache, `realpath_cache_bucket` as
/// `realpath_cache_get()` shows it, with the bytes `realpath_cache_size()`
/// counts it as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealpathEntry {
    pub path: Vec<u8>,
    pub realpath: Vec<u8>,
    pub key: u64,
    pub is_dir: bool,
    pub expires: i64,
    pub size: i64,
}

/// Who a row the request added to `EG(zend_constants)` belongs to,
/// php's `ZEND_CONSTANT_MODULE_NUMBER` beside `zend_constant.filename`:
/// the host, which registers under `Core` at request start; an
/// extension that registers one while the request runs; or the program,
/// `PHP_USER_CONSTANT`, with the file `zend_register_constant` read off
/// `zend_get_executed_filename_ex` where one was executing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConstantOwner {
    Core,
    Extension(ExtensionId),
    User(Option<Box<[u8]>>),
}

/// The parameters of one callee whose arguments a trace redacts:
/// `declared` by position, the variadic's for the rest.
#[derive(Default)]
struct Sensitive {
    declared: Vec<bool>,
    variadic: bool,
}

impl Sensitive {
    /// Whether the argument at `at`, from zero, is shown redacted.
    fn at(&self, at: usize) -> bool {
        self.declared.get(at).copied().unwrap_or(self.variadic)
    }
}

/// One row a request added to `EG(zend_constants)`: its owner, its name
/// as `zend_register_constant` keys it, its value, and
/// `zend_constant::attributes`, what a `DECLARE_ATTRIBUTED_CONST` hung
/// on it, with `CONST_DEPRECATED` decoded from them where it was
/// registered.
struct ConstantRow {
    owner: ConstantOwner,
    name: Vec<u8>,
    value: Value,
    attributes: Vec<Attribute>,
    deprecated: bool,
}

impl ConstantRow {
    fn new(owner: ConstantOwner, name: Vec<u8>, value: Value) -> ConstantRow {
        ConstantRow {
            owner,
            name,
            value,
            attributes: Vec::new(),
            deprecated: false,
        }
    }
}

/// Where a request stands, as `php_verror` reads it to name the origin
/// of a diagnostic raised while no function is active
/// (`Z/main/main.c:1086-1129`): `PG(during_request_startup)`, set by
/// `php_request_startup` and cleared by the CLI before it compiles the
/// file (`Z/sapi/cli/php_cli.c:943`), so the extensions' `RINIT` are
/// served inside it; the main op array; and `EG_FLAGS_IN_SHUTDOWN`, which
/// `php_request_shutdown` sets first (`Z/main/main.c:1960`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Startup,
    Running,
    Shutdown,
}

/// What a native intends to do at a path, for the `open_basedir` guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathIntent {
    Read,
    Write,
    Create,
    Remove,
    Stat,
    Exec,
}

/// One `ob_*` operation on the request's buffer stack.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputOp {
    Start {
        handler: StartHandler,
        chunk_size: i64,
        flags: i64,
    },
    Flush,
    Clean,
    EndFlush,
    EndClean,
    GetFlush,
    GetClean,
    GetContents,
    GetLength,
    GetLevel,
    GetStatus {
        full: bool,
    },
    ListHandlers,
    ImplicitFlush(bool),
}

/// What `ob_start()` starts, `php_output_handler_create_user`'s switch
/// over its `$callback` (`Z/main/output.c:466`).
#[derive(Debug, Clone, PartialEq)]
pub enum StartHandler {
    /// `null`: the default handler, which passes its buffer on.
    Default,
    /// A name an extension registered its handler under, found by
    /// [`Request::buffer_handler`] before the name is read as a callable.
    Registered(BufferHandler),
    /// Anything else, as the callable it resolved to.
    User(Callable),
}

/// A handler of the buffer stack an extension hands to
/// [`ExtensionBuilder::register`] under a name `ob_start()` takes,
/// `php_output_handler_alias_register` from its `MINIT`, with the
/// handlers it cannot stand beside: while the stack is not empty and one
/// of `conflicts` is on it, starting this one is php's warning and a
/// refusal, the check every `php_output_handler_conflict_register` in
/// php-src makes through `php_output_handler_conflict`.
///
/// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
#[derive(Debug, Clone, Copy)]
pub struct BufferHandler {
    pub name: &'static str,
    pub conflicts: &'static [&'static str],
    /// `php_output_handler_context_func_t`.
    pub handler: fn(&mut Request, &mut HandlerCall<'_>) -> Result<HandlerStatus, Thrown>,
}

impl PartialEq for BufferHandler {
    /// One registration a name, as php's alias table keys them.
    fn eq(&self, other: &BufferHandler) -> bool {
        self.name == other.name
    }
}

/// The operation a handler is served for, `php_output_context.op`'s
/// bits read once; none set is a plain write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // php's op bits, one field per bit, each read alone
pub struct HandlerOp {
    /// `PHP_OUTPUT_HANDLER_START`: the handler's first call.
    pub start: bool,
    /// `PHP_OUTPUT_HANDLER_CLEAN`.
    pub clean: bool,
    /// `PHP_OUTPUT_HANDLER_FLUSH`.
    pub flush: bool,
    /// `PHP_OUTPUT_HANDLER_FINAL`: its last.
    pub finish: bool,
}

/// One call of an extension's buffer handler, `php_output_context`, and
/// the one `php_output_handler_hook` an extension in php-src asks of the
/// handler being served.
#[derive(Debug)]
pub struct HandlerCall<'a> {
    pub op: HandlerOp,
    /// `context->in`: the handler's buffered bytes.
    pub input: &'a [u8],
    /// `context->out`: what goes on, none being `PHP_OUTPUT_HANDLER_NO_DATA`.
    pub out: Vec<u8>,
    /// `php_output_handler_hook(PHP_OUTPUT_HANDLER_HOOK_IMMUTABLE)`: set,
    /// the handler can no longer be cleaned or removed.
    pub immutable: bool,
}

/// What an extension's buffer handler answers, its `zend_result`: on
/// `Failure` the handler is disabled and its buffer goes on unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerStatus {
    Success,
    Failure,
}

/// `SG(sapi_headers)`'s content type, `mimetype` and
/// `send_default_content_type` read as the one state they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentType {
    /// The mimetype a `Content-Type` header, or the default one sent
    /// with the block, laid down.
    Held(Vec<u8>),
    /// None yet, and the default one still to be sent.
    Default,
    /// None, and none to be sent.
    Unset,
}

/// An open stream, php's `php_stream` (design §4.3 seam 5), named by its
/// resource as php names it through `stream->res`: what `fopen` hands
/// back and `php_stream_to_zval` puts in a value, and what a native
/// reads back out with [`Stream::from_value`], `PHP_Z_PARAM_STREAM`'s
/// fetch. The operations are `php_stream_read` and its siblings, each
/// through the request that holds the stream; the layer under them is
/// the engine's `streams` layer.
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
/// see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cast {
    Select,
    Internal,
    Fd,
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
}

/// What a path's wrapper answers to its own `url_stat`, asked as
/// [`Request::wrapper_url_stat`] asks it: the stat, laid out as
/// `php_fstat` lays it out; `Failed` for the wrapper's `-1`; and
/// `Unsupported` where no wrapper was located or the one located has no
/// `url_stat`, as `php://` has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlStat {
    Found([i64; 13]),
    Failed,
    Unsupported,
}

/// What `php_stream_xport_create` is asked to make of a `transport`'s
/// name: a client connected within `timeout`, `None` to wait for ever,
/// or a server bound to the name, listening where `listen` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Xport {
    Client {
        timeout: Option<std::time::Duration>,
    },
    Server {
        listen: bool,
    },
}

/// Why `php_stream_xport_create` made no stream: the `errno` the natives
/// hand back as `$error_code`, `0` where there was none, and the text
/// they hand back as `$error_message` and print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XportFailure {
    pub code: i32,
    pub text: String,
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
    pub fn read(self, req: &mut Request, len: usize) -> Result<Option<PhpString>, Thrown> {
        req.stream_read_to_str(self, len)
    }

    /// `php_stream_write`: how many bytes the stream took, `None` for
    /// php's `-1` (`fwrite` answers `false`).
    pub fn write(self, req: &mut Request, bytes: &[u8]) -> Option<usize> {
        req.stream_op(self, |req, stream| stream.write(req, bytes))
            .flatten()
    }

    /// `php_stream_seek`: `true` for php's `0`. A `whence` of `None` is
    /// a number that is none of the three, [`Whence::from_int`]'s
    /// `None`, which a stream that seeks refuses and one that cannot is
    /// php's warning for.
    ///
    /// Asked from inside a filter's call on the stream, while the stream
    /// is out of its resource, the seek is taken when the call returns
    /// ([`crate::streams::FilterCall`]).
    pub fn seek(self, req: &mut Request, offset: i64, whence: impl Into<Option<Whence>>) -> bool {
        let whence = whence.into();
        if let Some(sought) = req.stream_op(self, |req, stream| stream.seek(req, offset, whence)) {
            return sought;
        }
        req.filtering
            .as_mut()
            .is_some_and(|call| call.defer_seek(self, offset, whence))
    }

    /// `php_stream_get_line`: the next line with its `\n`, at most
    /// `maxlen - 1` bytes of it, or all of it for `None`; `None` back is
    /// no byte to give, at eof or after a read that failed (`fgets`
    /// answers `false`). The string is asked for at `maxlen` first, as
    /// `fgets` asks `zend_string_alloc`, so a length past the memory
    /// limit is php's fatal before a byte is read.
    pub fn get_line(
        self,
        req: &mut Request,
        maxlen: Option<usize>,
    ) -> Result<Option<PhpString>, Thrown> {
        if let Some(len) = maxlen
            && len > req.heap_memory_limit()
        {
            let asked = req.string_with(len, |_| {})?;
            req.release(Value::string(asked));
        }
        match req
            .stream_op(self, |req, stream| stream.get_line(req, maxlen))
            .flatten()
        {
            Some(line) => Ok(Some(PhpString::new(req, &line)?)),
            None => Ok(None),
        }
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
    pub fn get_record(
        self,
        req: &mut Request,
        maxlen: usize,
        delim: &[u8],
    ) -> Result<Option<PhpString>, Thrown> {
        match req
            .stream_op(self, |req, stream| stream.get_record(req, maxlen, delim))
            .flatten()
        {
            Some(record) => Ok(Some(PhpString::new(req, &record)?)),
            None => Ok(None),
        }
    }

    /// `_php_stream_copy_to_mem`: the rest of the stream, or up to
    /// `maxlen` bytes of it, as one string, empty where nothing was read.
    pub fn copy_to_mem(
        self,
        req: &mut Request,
        maxlen: Option<usize>,
    ) -> Result<PhpString, Thrown> {
        let bytes = req
            .stream_op(self, |req, stream| stream.copy_to_mem(req, maxlen))
            .unwrap_or_default();
        PhpString::new(req, &bytes)
    }

    /// `php_stream_stat`, as `php_fstat` lays it out: `dev`, `ino`,
    /// `mode`, `nlink`, `uid`, `gid`, `rdev`, `size`, `atime`, `mtime`,
    /// `ctime`, `blksize` and `blocks`, or `None` where the stream has
    /// no stat (`fstat` answers `false`).
    pub fn stat(self, req: &mut Request) -> Option<[i64; 13]> {
        req.stream_op(self, |req, stream| stream.stat(req))
            .flatten()
    }

    /// `php_stream_truncate_set_size` behind
    /// `php_stream_truncate_supported`: `None` where the stream does not
    /// truncate (`ftruncate`'s `Can't truncate this stream!`), else
    /// whether it took the size.
    pub fn truncate(self, req: &mut Request, size: usize) -> Option<bool> {
        req.stream_op(self, |req, stream| stream.truncate(req, size))
            .flatten()
    }

    /// `php_stream_lock`, `flock(2)` with its operation, which
    /// `php_stream_supports_lock` guards: `false` where the stream has
    /// no `flock(2)` or `flock(2)` refused.
    pub fn flock(self, req: &mut Request, operation: i64) -> bool {
        req.stream_op(self, |req, stream| stream.flock(req, operation))
            .unwrap_or(false)
    }

    /// `php_stream_is(stream, PHP_STREAM_IS_STDIO)`: the stream is a
    /// plain file's, or one of the host's standard ones.
    #[must_use]
    pub fn is_stdio(self, req: &Request) -> bool {
        req.stream_ref(self)
            .is_some_and(crate::streams::PhpStream::is_stdio)
    }

    /// `php_stream_set_option` (`Z/main/streams/streams.c:1447`): the
    /// stream's kind asked first, then, where it does not take the
    /// option, the generic layer, which takes the chunk size and whether
    /// the read buffer is used. A stream that is closed answers
    /// [`OptionAnswer::Err`].
    pub fn set_option(self, req: &mut Request, option: StreamOption) -> OptionAnswer {
        req.stream_op(self, |req, stream| stream.set_option(req, option))
            .unwrap_or(OptionAnswer::Err)
    }

    /// What `stream_get_meta_data` answers: `timed_out`, `blocked` and
    /// `eof` where the stream's kind reports them, `wrapper_type`,
    /// `stream_type`, `mode`, `unread_bytes`, `seekable` and `uri`, in
    /// php's order.
    pub fn meta_data(self, req: &mut Request) -> Result<PhpArray, Thrown> {
        req.stream_meta_data(self)
    }

    /// Whether `opendir` opened the stream, `PHP_STREAM_FLAG_IS_DIR`,
    /// which is what `readdir` and `closedir` accept and `fclose`
    /// refuses.
    #[must_use]
    pub fn is_dir(self, req: &Request) -> bool {
        req.stream_ref(self)
            .is_some_and(crate::streams::PhpStream::is_dir)
    }

    /// `php_stream_readdir`: the next entry's name, `.` and `..` first,
    /// `None` past the last (`readdir` answers `false`).
    pub fn read_dir(self, req: &mut Request) -> Result<Option<PhpString>, Thrown> {
        match req
            .stream_op(self, |req, stream| stream.read_dir(req))
            .flatten()
        {
            Some(name) => Ok(Some(PhpString::new(req, &name)?)),
            None => Ok(None),
        }
    }

    /// `php_stream_tell`: the position, `-1` for a stream that has none.
    /// Asked from inside a filter's call on the stream, it is the
    /// position the call was made at ([`crate::streams::FilterCall`]).
    #[must_use]
    pub fn tell(self, req: &Request) -> i64 {
        match req.stream_ref(self) {
            Some(stream) => stream.position(),
            None => req
                .filtering
                .as_ref()
                .and_then(|call| call.position_of(self))
                .unwrap_or(-1),
        }
    }

    /// `php_stream_eof`: no byte left in the read buffer, and the kind at
    /// eof or, asked `PHP_STREAM_OPTION_CHECK_LIVENESS`, found dead, which
    /// a user-space stream answers from its `stream_eof`. Asked from
    /// inside a filter's call on the stream, it is what it was when the
    /// call was made ([`crate::streams::FilterCall`]).
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
        if let Some(eof) = req.stream_op(self, |req, stream| stream.eof(req)) {
            return eof;
        }
        req.filtering
            .as_ref()
            .and_then(|call| call.eof_of(self))
            .unwrap_or(false)
    }

    /// `php_stream_flush`: `true` for php's `0`.
    pub fn flush(self, req: &mut Request) -> bool {
        req.stream_op(self, |req, stream| stream.flush(req))
            .unwrap_or(false)
    }

    /// `php_stream_close`, what `fclose` does past its checks: the
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
        if req.held_streams.contains(&self.0) {
            return false;
        }
        self.0.close(req);
        true
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
    /// `stream_opener`, `None` for a wrapper that opens no stream, php's
    /// `wrapper does not support stream open`; the opener's own `None`
    /// is the open that failed, which `php_stream_display_wrapper_errors`
    /// reports as `operation failed`.
    pub open: Option<fn(&mut Request, &[u8], &[u8]) -> Result<Option<Stream>, Thrown>>,
    /// `dir_opener`: the directory's entries, in the order `readdir`
    /// gives them, which the engine serves as a directory stream under
    /// the wrapper's label (`rewinddir` back to the first); `None` is the
    /// open that failed.
    pub open_dir: Option<fn(&mut Request, &[u8]) -> Result<Option<Vec<Vec<u8>>>, Thrown>>,
    pub stat: Option<fn(&mut Request, &[u8]) -> Result<Option<PhpArray>, Thrown>>,
    pub unlink: Option<fn(&mut Request, &[u8]) -> Result<bool, Thrown>>,
    pub rename: Option<fn(&mut Request, &[u8], &[u8]) -> Result<bool, Thrown>>,
    pub mkdir: Option<fn(&mut Request, &[u8], i64, bool) -> Result<bool, Thrown>>,
    pub rmdir: Option<fn(&mut Request, &[u8]) -> Result<bool, Thrown>>,
    /// `stream_metadata`: one of [`WrapperOp::Touch`], [`WrapperOp::Chmod`],
    /// [`WrapperOp::Owner`] and [`WrapperOp::Group`], with the path as
    /// the program wrote it.
    pub set_stat: Option<fn(&mut Request, &[u8], WrapperOp<'_>) -> Result<bool, Thrown>>,
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

    /// `fops->dtor`, served once when the filter leaves its chain.
    fn close(&mut self, _req: &mut Request) -> Result<(), Thrown> {
        Ok(())
    }
}

/// `php_stream_filter_factory`'s `create_filter`: a filter of the name
/// the program asked for, which may be one a wildcard matched, with the
/// parameters it was given; `None` is the factory's refusal, after
/// whatever it said.
pub type FilterFactory =
    fn(&mut Request, &[u8], Option<&Value>) -> Result<Option<Box<dyn StreamFilter>>, Thrown>;

/// A resource's per-type destructor, php's `list_dtor_ex`: served once,
/// by [`Resource::close`] or by the release that reaches zero, with the
/// resource already closed and its payload still readable.
pub type ResourceDtor = fn(&mut Request, Resource);

/// The resource types this request knows, php's `list_destructors`
/// (`zend_register_list_destructors_ex`): a name and a destructor per
/// type, numbered in registration order. The list itself,
/// `EG(regular_list)`, is the heap's, reached through
/// [`Request::resource`].
#[derive(Default)]
pub struct Resources {
    types: Vec<(Vec<u8>, Option<ResourceDtor>)>,
}

impl Resources {
    /// `zend_register_list_destructors_ex`: the type's name, which
    /// `get_resource_type()` prints, and its destructor, which seam 1
    /// serves when the resource closes. The number is the type's.
    pub fn register_type(&mut self, name: &[u8], dtor: Option<ResourceDtor>) -> ResourceType {
        let kind = ResourceType(u32::try_from(self.types.len()).unwrap_or(u32::MAX));
        self.types.push((name.to_vec(), dtor));
        kind
    }

    /// The type's name, `zend_rsrc_list_get_rsrc_type` over the table.
    fn type_name(&self, kind: ResourceType) -> Option<&[u8]> {
        self.types
            .get(kind.0 as usize)
            .map(|(name, _)| name.as_slice())
    }

    /// `zend_fetch_list_dtor_id`: the type registered under `name`.
    fn type_named(&self, name: &[u8]) -> Option<ResourceType> {
        self.types
            .iter()
            .position(|(registered, _)| registered == name)
            .and_then(|at| u32::try_from(at).ok())
            .map(ResourceType)
    }

    /// The type's destructor, if it registered one.
    fn dtor(&self, kind: ResourceType) -> Option<ResourceDtor> {
        self.types.get(kind.0 as usize).and_then(|(_, dtor)| *dtor)
    }
}

/// One request's interrupt (design §5.8; `CONTEXT.md`, Interrupt), as
/// the thread that raises it sees it: `Send` and `Sync`, and cloned so
/// that the engine's execution timer and the thread serving the request
/// share one. Raising is all a setter does; acting on it is the loop's,
/// at a back-edge, at a frame's activation and after every
/// `Flow::Resume`, and nothing else touches it.
#[derive(Debug, Clone, Default)]
pub struct Interrupt(Arc<State>);

impl Interrupt {
    /// What signal delivery raises it with: the loop dispatches what was
    /// delivered to its PHP handlers at its next check, which is php's
    /// `zend_interrupt_function`.
    pub fn raise(&self) {
        self.0.raised.store(true, Ordering::Release);
    }

    /// Whether it is raised: what the loop tests at its three safepoints
    /// and what compiled code tests once per loop iteration through the
    /// tier's `ExitCtx` (`jit-design.md` §4.3).
    #[must_use]
    pub fn raised(&self) -> &AtomicBool {
        &self.0.raised
    }

    /// What the engine's execution timer raises it with,
    /// `max_execution_time` having elapsed: the seconds it was armed
    /// with, then the two words `zend_timeout_handler` stores
    /// (`Z/Zend/zend_execute_API.c:1516`). The seconds go down first and
    /// the store that raises it releases them, because php's handler
    /// runs on the interrupted thread and this timer does not.
    pub(crate) fn elapsed(&self, seconds: i64) {
        self.0.seconds.store(seconds, Ordering::Relaxed);
        self.0.timed_out.store(true, Ordering::Relaxed);
        self.0.raised.store(true, Ordering::Release);
    }

    /// What unsetting the execution timer does beside stopping it
    /// (`zend_unset_timeout`, `Z/Zend/zend_execute_API.c:1657-1685`): the
    /// timer's word is cleared, so a deadline the old timer reached as
    /// it was being unset is not the fatal the new one raises.
    pub(crate) fn timer_unset(&self) {
        self.0.timed_out.store(false, Ordering::Relaxed);
    }

    /// What a check that found it raised reads: it is cleared, and the
    /// seconds come back when the execution timer was what raised it,
    /// both as php clears them (`Z/Zend/zend_vm_def.h:10526`, `:10537`).
    /// Clearing it acquires what the setter released, so the seconds the
    /// text names are the ones the timer was armed with; the loop's own
    /// test stays a relaxed load, because this is where the two threads
    /// meet.
    pub(crate) fn taken(&self) -> Option<i64> {
        self.0.raised.swap(false, Ordering::Acquire);
        self.0
            .timed_out
            .swap(false, Ordering::Relaxed)
            .then(|| self.0.seconds.load(Ordering::Relaxed))
    }
}

/// What an [`Interrupt`] holds, one per request: the atomic the loop
/// tests, which of the two setters raised it, and the seconds php's
/// timeout text names.
#[derive(Debug, Default)]
struct State {
    raised: AtomicBool,
    /// php's `EG(timed_out)`.
    timed_out: AtomicBool,
    /// php's `EG(timeout_seconds)`.
    seconds: AtomicI64,
}

/// The fatal the request is ending in, when it is one: `E_ERROR` and
/// its kin, or a source that did not compile. An `exit` is not one:
/// php unwinds it as a throwable ([`Request::throw_unwind_exit`]) and
/// records its status alone, in `EG(exit_status)`, which a fatal writes
/// too and the last write wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ending {
    /// `E_ERROR`: the fatal as `php_error_cb` gave it to `php_log_err`,
    /// one entry, which is the text `serve` reports and not a second
    /// rendering; the streams already have theirs.
    Fatal(String),
    /// `E_PARSE`: the text is already on the stream `php_error_cb` put it
    /// on, and what is left is php's status, 255.
    Parse,
}

/// What a request wrote beside the body, one call the sink will see:
/// an entry for the log or bytes for the error stream. Kept in the
/// order they were written, since under php's CLI the two land on one
/// fd, and there the log form of a diagnostic stands before its
/// `display_errors=stderr` form (#216).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Aside {
    /// `OutputSink::log`: one entry, whole, ended by the host.
    Log(Vec<u8>),
    /// `OutputSink::write_stderr`: bytes as given.
    Stderr(Vec<u8>),
}

/// Where what leaves a request goes while it runs, decided once when
/// `serve` starts it: the host's sink, held, where the host hands one
/// over ([`OutputSink::held`]), and otherwise the calls kept for the sink
/// the host lent `serve`.
pub(crate) enum Sink {
    Held(Box<dyn OutputSink + Send>),
    Kept(Kept),
}

impl Sink {
    /// The one sink every call of the request goes to.
    fn get(&mut self) -> &mut dyn OutputSink {
        match self {
            Sink::Held(held) => held.as_mut(),
            Sink::Kept(kept) => kept,
        }
    }
}

/// A request's calls kept until it ends, for a host whose sink stays in
/// its hands: the log's entries and the error stream's bytes in the
/// order written, the header block once committed, and the body.
#[derive(Default)]
pub(crate) struct Kept {
    aside: Vec<Aside>,
    headers: Option<Vec<u8>>,
    body: Vec<u8>,
}

impl Kept {
    /// Every call handed to `sink`, one each: the log and the error
    /// stream in the order written, then the header block, then the body.
    pub(crate) fn hand_to(self, sink: &mut dyn OutputSink) {
        for aside in self.aside {
            match aside {
                Aside::Log(entry) => sink.log(&entry),
                Aside::Stderr(bytes) => sink.write_stderr(&bytes),
            }
        }
        if let Some(headers) = self.headers {
            sink.send_headers(&headers);
        }
        sink.write(&self.body);
        sink.flush();
    }
}

impl OutputSink for Kept {
    fn write(&mut self, bytes: &[u8]) {
        self.body.extend_from_slice(bytes);
    }

    fn flush(&mut self) {}

    fn send_headers(&mut self, block: &[u8]) {
        self.headers = Some(block.to_vec());
    }

    fn log(&mut self, entry: &[u8]) {
        self.aside.push(Aside::Log(entry.to_vec()));
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.aside.push(Aside::Stderr(bytes.to_vec()));
    }
}

/// A write a debug build stopped: the opline whose `TMP` it was, the
/// opcode serving it, the temporary's number and what the slot held.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy)]
struct DirtyTemporary {
    opline: u32,
    opcode: Opcode,
    number: u32,
    held: php_heap::ZvalType,
}

/// One request (design §5.14): `Send`, single-threaded, dropped at the
/// end of `serve`. Everything a native may touch is a method here or on
/// a value it hands out.
#[allow(clippy::struct_excessive_bools)] // php's EG and PG flags, each its own global, read alone
pub struct Request {
    shared: Arc<Shared>,
    description: RequestDescription,
    /// The natives the request has entered, in order, where the witness
    /// runner asked for them (harness §4.5 condition 2, revision 58);
    /// `None` records nothing. The field exists in a build with the
    /// `witness-trace` feature alone, so the host's binary has none.
    #[cfg(feature = "witness-trace")]
    trace: Option<Vec<crate::test::Entered>>,
    heap: Heap,
    stack: VmStack,
    vm: Vm,
    /// The map table of design §5.14, php's `CG(map_ptr_base)`: what a
    /// published unit's map pointers index, laid down with the request,
    /// every entry `NULL` until touched. Every op array's runtime cache
    /// stands in it ([`exec::RuntimeCache`]) and nowhere else.
    map: MapTable,
    states: Vec<(TypeId, Box<dyn Any + Send>)>,
    ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// `EG(error_reporting)`, the mask a diagnostic is held to
    /// ([`Request::error_cb`]): what `OnUpdateErrorReporting`
    /// (`Z/Zend/zend.c:111`) reads out of the `error_reporting` entry at
    /// request start and whenever the entry moves, what
    /// `error_reporting()` and `BEGIN_SILENCE` write past the entry, and
    /// what `error_reporting()` answers. php keeps it apart from the ini
    /// entry, which is why `ini_get('error_reporting')` under a `@` reads
    /// the level the `@` masked.
    error_reporting: i64,
    /// Where what passed the `php_output` layer goes, php's
    /// `sapi_module.ub_write`, and the header block, the log and the
    /// error stream beside it: the host's sink, or the calls kept for it.
    sink: Sink,
    /// The `php_output` layer, `main/output.c`'s `OG` and the header state
    /// `sapi_send_headers` keeps beside it (`crate::output`).
    output: Output,
    header_callback: Option<Callable>,
    last_error: Option<(Level, String, Vec<u8>, u32)>,
    /// The diagnostics raised while the engine registers its extensions'
    /// ini entries (`php_during_module_startup()`), held in the order
    /// raised rather than shown, on the one request the engine lays for
    /// that stage; every request it serves raises them again first.
    startup: Option<Vec<(Level, String)>>,
    /// php's `EG(last_fatal_error_backtrace)`, rendered by
    /// `zend_trace_to_string`: the trace `zend_error_impl` fetched for the
    /// fatal being raised under `fatal_error_backtraces`, cleared for
    /// every diagnostic and by `zend_exception_error`, read by
    /// `php_error_cb` for the display and the log forms.
    last_fatal_error_backtrace: Option<String>,
    ending: Option<Ending>,
    /// php's `EG(exit_status)`: what the process ends with, 255 written
    /// by `php_error_cb` for a fatal and the code by `exit`, whichever
    /// wrote last.
    exit_status: i32,
    resources: Resources,
    superglobals: Vec<(Vec<u8>, PhpArray)>,
    /// Each auto-global made just in time and not made yet, php's
    /// `armed` on each of `CG(auto_globals)`: the first
    /// `zend_is_auto_global` of one lays it into `EG(symbol_table)`
    /// ([`Request::arm`]).
    armed: Vec<php_unit::AutoGlobal>,
    /// The symbol tables, [`TableId`] naming one: php's
    /// `EG(symbol_table)` first, the main op array's variables (design
    /// §5.6, §5.10), and after it one table per function frame that
    /// included a file, `zend_rebuild_symbol_table`'s, which goes with
    /// its frame. A table's names stand in the order they were attached
    /// or written, each once; while a frame is attached to a name the
    /// bucket points at the frame's CV ([`Symbol::Indirect`]) and the
    /// frame owns the value, and `zend_detach_symbol_table` moves the
    /// value into the bucket when the frame leaves. The first table
    /// outlives the main frame, so a variable the program wrote at the
    /// top level is still an owner while the shutdown functions are
    /// served; `shutdown_destructors` walks it in reverse before the
    /// store's walk, and `zend_shutdown_executor_values` destroys it
    /// after the extensions' `rshutdown`. `$GLOBALS`, `compact`,
    /// `extract` and `get_defined_vars` read these tables (design §5.6).
    tables: Vec<SymbolTable>,
    /// `EG(included_files)`: the resolved path of every file compiled by
    /// the request, the primary one first (`php_execute_script_ex`),
    /// then each an `INCLUDE_OR_EVAL` opened, which the once forms read
    /// before they open one (`zend_include_or_eval`).
    included_files: Vec<Vec<u8>>,
    /// The rows this request added to `EG(zend_constants)`, in the order
    /// it added them, each under the extension it is registered by:
    /// `Core` for what the host registers at request start
    /// ([`crate::engine::FileHandles`]), `None` for what `define()` and
    /// `const` wrote, php's `PHP_USER_CONSTANT`, with the file it was
    /// defined in.
    constants: Vec<ConstantRow>,
    /// `zend_enum_get_case`'s slot (`Z/Zend/zend_enum.c:410`): one
    /// instance per case of each enum, made on the first read of the
    /// case and handed back by identity from then on, so that
    /// `E::A === E::A`. php keeps it in the class's constant table,
    /// which outlives the request; the linked tables here are shared
    /// and immutable and an object is neither, so the slot is the
    /// request's and the cases go with it.
    enum_cases: Vec<((ClassId, Vec<u8>), PhpObject)>,
    /// The backed enums whose case values `zend_enum_build_backed_enum_table`
    /// has held to their backing type and found no two alike, php's
    /// `ZEND_ACC_CONSTANTS_UPDATED` on each; a check that failed is made
    /// again on the next read, as php's is.
    enums_updated: Vec<ClassId>,
    /// The methods `zend_enum_register_funcs` gives each enum a program
    /// declares, one function number each past
    /// [`crate::enums::ENUM_FUNCTIONS`], as php allocates a
    /// `zend_internal_function` per method per enum.
    enum_functions: Vec<(ClassId, crate::enums::EnumMethod)>,
    /// The functions `zend_get_call_trampoline_func` makes of a call to
    /// a name only `__call` or `__callStatic` answers, one per magic
    /// method and name, two function numbers each
    /// ([`crate::trampoline`]).
    trampolines: Vec<crate::trampoline::CallTrampoline>,
    /// `CONST_IS_RECURSIVE` over the constants a deprecation is being
    /// worded for, so an attribute naming the constant it marks reads it
    /// without raising again (GH-18463): the declaring class and the
    /// name of a class constant, a program's alone, or the name of a
    /// global one.
    deprecating: Vec<(Option<ClassId>, Box<[u8]>)>,
    /// `CE_STATIC_MEMBERS(ce)` (`Z/Zend/zend_object_handlers.c:2094`):
    /// one slot per declared `static` property, keyed by the class that
    /// declared it and by the property's name. php gives every class a
    /// table of its own whose inherited rows are `IS_INDIRECT` into the
    /// parent's, so a child and its parent read and write one cell; the
    /// declaring class is that cell's name here. A slot is laid down on
    /// first use from the default the declaration carries,
    /// `zend_class_init_statics`, and a typed property with no default
    /// is laid down `IS_UNDEF`. php's table hangs off a map pointer and
    /// is the request's; so is this one.
    statics: Vec<((ClassId, Vec<u8>), Value)>,
    /// The evaluated defaults of natives' absent arguments, each kept by
    /// the request under its native and position (design §4.7).
    defaults: Vec<((Function, u32), Value)>,
    /// `zend_get_property_guard` (`Z/Zend/zend_object_handlers.c:866`),
    /// what stops a magic accessor reaching itself: one word per
    /// property name per object, carrying `IN_GET`, `IN_SET`,
    /// `IN_ISSET` or `IN_UNSET` while that accessor serves that name
    /// on that object. php keeps the word on the object, in the slot
    /// after its declared properties; an object here carries what the
    /// heap lays down and nothing more, so the words are the request's,
    /// named by `spl_object_id()` and the property's name. A row is
    /// dropped the moment its word empties, which is before the object
    /// can die, so an index the store gives out again is never one an
    /// old row still names.
    guards: Vec<((php_heap::ObjectId, Vec<u8>), u32)>,
    /// `zend_property_info_source_list` (`Z/Zend/zend_types.h:626`),
    /// one row per reference a typed property's slot holds: the word
    /// [`php_heap::Ref::set_sources`] carries names the row, and the row
    /// lists the properties that type the cell. php hangs the list off
    /// the cell itself; the cell is the heap's and a declared type is
    /// the engine's, so the list is here and the cell carries its name.
    /// A row is dropped by `zend_reference_destroy`, which reaches
    /// [`HeapHooks::free_reference`].
    ref_sources: Vec<(usize, Vec<RefSource>)>,
    /// php's `EG(weakrefs)`: every weakly referenced object's id against
    /// the `WeakReference` and `WeakMap` objects that name it
    /// (`crate::weakrefs`).
    pub(crate) weakrefs: crate::weakrefs::Table,
    /// php's `EG(lazy_objects_store)`: what goes with each lazy object's
    /// state, by its id ([`lazy`]).
    lazy: lazy::Table,
    /// The next word [`Request::ref_add_type_source`] hands out, never
    /// reused, so a cell freed and a cell laid down at its address are
    /// never one row.
    next_ref_source: usize,
    pub(crate) wrappers: Vec<StreamWrapper>,
    /// `FG(stream_wrappers)`: the request's own wrapper table once a
    /// program changed it, `None` while it has the built-in one.
    pub(crate) url_wrappers: Option<Vec<crate::streams::Registered>>,
    /// `FG(user_stream_current_filename)`: the path a user-space wrapper's
    /// opener is opening, which the same path opened from inside it is
    /// refused as.
    pub(crate) user_stream_current: Option<Vec<u8>>,
    /// The `php_stream_context` the stream operation in progress was
    /// handed ([`Request::with_stream_ctx`]), which a user-space wrapper's
    /// instance is given as its `$context`; `None` is php's `NULL`.
    pub(crate) stream_ctx: Option<Resource>,
    /// The include opening a file through a stream wrapper, and how many
    /// calls stood when it began: php's `get_active_function_name` names
    /// the construct (`include`, `require_once`) for what the stream layer
    /// raises under it, and a native a user-space wrapper serves from
    /// inside it is one call deeper.
    pub(crate) including: Option<(IncludeKind, usize)>,
    /// `PG(in_user_include)`: a user-space wrapper that is no URL wrapper
    /// is opening for an include under `allow_url_include=0`, so the
    /// opens it makes are held to `allow_url_include` as an include's are.
    pub(crate) in_user_include: bool,
    /// `FG(pclose_ret)`: what the last stream freed by its resource's
    /// destructor answered from its kind's close, which `pclose()` reads.
    pub(crate) pclose_ret: i32,
    /// `FG(stream_filters)`: each filter name and its factory, in the
    /// order they were registered.
    pub(crate) filters: Vec<(Vec<u8>, FilterFactory)>,
    /// A throwable PHP code served by a stream's kind or filter raised
    /// inside an operation whose answer has no room for it: php's
    /// `EG(exception)` standing until the native that asked for the
    /// operation returns, which is when [`Request::serve_native`] raises
    /// it, and before which no more such code is served.
    pub(crate) stream_thrown: Option<Thrown>,
    /// The streams an operation holds while it serves PHP code on them,
    /// php's `PHP_STREAM_FLAG_NO_FCLOSE`, which `fclose` refuses.
    pub(crate) held_streams: Vec<Resource>,
    /// The filter call in progress, whose stream is out of its resource
    /// while the filter runs.
    pub(crate) filtering: Option<crate::streams::FilterCall>,
    /// `le_stream_filter`, the type of the resource a filter on a chain
    /// is named by.
    pub(crate) filter_type: ResourceType,
    /// php's `EG(user_error_handler)`, the handler `set_error_handler`
    /// installed, `IS_UNDEF` as none; the stack under it is
    /// `EG(user_error_handlers)`, each row the handler it replaced and
    /// that handler's mask, `EG(user_error_handlers_error_reporting)`
    /// kept beside it. Every handler here is one owner of its value.
    /// `EG(error_handling)` at `EH_THROW` with `EG(exception_class)`: the
    /// class a warning is thrown as meanwhile, `None` at `EH_NORMAL`.
    error_handling: Option<String>,
    /// What a warning was thrown as under `EH_THROW`, held until the
    /// native that asked for it hands it on.
    error_handling_thrown: Option<Thrown>,
    error_handler: Option<Callable>,
    /// `EG(user_error_handler_error_reporting)`: the levels the handler
    /// is handed, `set_error_handler`'s `$error_levels`.
    error_handler_levels: i64,
    error_handlers: Vec<(Option<Callable>, i64)>,
    /// php's `EG(user_exception_handler)` and the stack under it,
    /// `EG(user_exception_handlers)`, the same shape without a mask.
    exception_handler: Option<Callable>,
    exception_handlers: Vec<Option<Callable>>,
    /// `BG(user_shutdown_function_names)`.
    shutdown_functions: ShutdownFunctions,
    tick_functions: Vec<(Callable, Vec<Value>)>,
    autoloaders: Vec<Callable>,
    /// `EG(in_autoload)`: the lowered names an autoloader is loading.
    in_autoload: Vec<Vec<u8>>,
    assert_callback: Option<Value>,
    umask: u32,
    /// `BG(CurrentStatFile)` and `BG(CurrentLStatFile)`, `php_stat`'s
    /// cache, which the plain files kind clears (design §4.3 seam 5).
    pub(crate) stat_cache: crate::wrapper::StatCache,
    /// `CWDG(realpath_cache)`, which every absolute path resolved through
    /// [`Request::virtual_file_ex`] is entered in (`crate::virtual_cwd`).
    realpath_cache: RealpathCache,
    cwd: PathBuf,
    /// `le_stream`, the resource type every stream is registered under.
    stream_type: ResourceType,
    /// The fibers running on this request, innermost last, each with the
    /// execution state of the stack that resumed it, and where each
    /// suspends to (design §5.11, `crate::fiber`).
    fibers: crate::fiber::Fibers,
    /// php's `EG(exception)`: the throwable a handler left pending, which
    /// the loop reads at the `Threw` edge.
    pending: Option<PhpObject>,
    /// php's `zend_is_unwind_exit(EG(exception))`: the unwind exit an
    /// `exit` threw is standing where a throwable would, and nothing is
    /// in `pending` while it does. What `exit` leaves for the unwinder,
    /// which skips every `finally` block for it and gives up every
    /// throwable it meets, and for a native whose callback exited; the
    /// outermost serving's end releases it, as `zend_exception_error`
    /// does.
    unwind_exit: bool,
    /// php's `zend_bailout` under way: a fatal was rendered where it was
    /// raised and the request is leaving for the `zend_try` that catches
    /// it, the outermost serving's end or the step of
    /// `php_request_shutdown` it was raised in ([`Request::catch_bailout`]).
    /// Nothing is served until then: the loop unwinds as it unwinds the
    /// unwind exit, past every `catch` and `finally`, a native reading
    /// what its callback left reads the fatal, and a second fatal raised
    /// on the way is not rendered.
    bailout: bool,
    /// Whether a diagnostic is displayed past the `php_output` layer,
    /// `php_output_direct`, as php displays what it raises while the
    /// engine starts ([`crate::api::raising::startup`]).
    display_direct: bool,
    /// Where the request stands in `php_request_startup`,
    /// `php_execute_script` and `php_request_shutdown`, what `php_verror`
    /// names a diagnostic's origin by when no function is active.
    phase: Phase,
    /// `zend_objects_store_mark_destructed`: every object behind the
    /// request, set where `shutdown_destructors` catches the
    /// `zend_bailout` a destructor's raise ends the walk with
    /// (`Z/Zend/zend_execute_API.c:258-266`). No destructor is served
    /// once it stands.
    marked_destructed: bool,
    /// What the entry frame of the innermost serving returned.
    returned: Value,
    /// The interrupt, shared with whatever thread sets it (design §5.8).
    interrupt: Interrupt,
    /// The execution timer, armed while `max_execution_time` is not
    /// zero; dropping the request unsets it.
    timer: Option<crate::interrupt::Timer>,
    /// The thread's stack limit, php's `EG(stack_limit)`, worked out
    /// once at request start on the thread the request runs on
    /// (`zend_call_stack_init`) from the two `zend.*_stack_size`
    /// entries; what the compiler is held to (design §5.6).
    stack_limit: StackLimit,
    /// A debug build's record of the first write of a `TMP` it found
    /// over a slot that still held an owner, until the loop takes it
    /// ([`Request::take_sensed`]).
    #[cfg(debug_assertions)]
    dirty: Option<DirtyTemporary>,
    /// The classes the units this request served declared, each the
    /// unit that holds it and the index of its declaration, in the
    /// order declared: php's `EG(class_table)` past what the engine
    /// registered at startup. A `ClassId` past the engine's classes
    /// counts into this list. What design §6.2 binds early into an
    /// immutable linked class shared across requests is not built yet,
    /// so every class a unit declares is this request's, the
    /// `DECLARE_CLASS_DELAYED` path, and dropped with it.
    declared: Vec<Declared>,
    /// The classes a unit's load bound early at their
    /// `DECLARE_CLASS_DELAYED` ([`Request::declare_classes`]), each the
    /// unit and the class's index, until that opline runs and passes
    /// over it: php's runtime definition key gone from `EG(class_table)`
    /// once `zend_accel_do_delayed_early_binding` has bound the class
    /// under its name. A load that bound nothing leaves nothing here, so
    /// the opline of a file loaded again declares, and is refused.
    bound_at_load: Vec<(Arc<Unit>, u32)>,
    /// The classes whose link is under way and what their links left
    /// for later ([`linking::Links`]).
    links: linking::Links,
    /// `CG(rtd_key_counter)` over the units this request compiled for
    /// itself: the runtime definition keys and anonymous class names
    /// they made, which the next one counts on from
    /// ([`crate::engine::Shared::compile_for`]).
    pub(crate) keys_made: u32,
    /// The aliases `class_alias()` added to `EG(class_table)`, each the
    /// lowered name it is keyed by, the class it names, and how many
    /// classes the table held when it was added, which is where php's
    /// table has it among them: `ZVAL_ALIAS_PTR` rows, request state as
    /// the classes the request declared are.
    aliases: Vec<(usize, Vec<u8>, ClassId)>,
    /// `PHP_SAPI`, `sapi_module.name` as `php_module_startup` registers
    /// it: the host's, so the request's, the engine's interned string
    /// for the name the description carries.
    sapi_constant: Value,
    /// The functions and methods the units this request served declared,
    /// each the unit that holds it and which of its op arrays is the
    /// body: php's `EG(function_table)` past the natives and each user
    /// class's `function_table`. A `Function` past the natives and the
    /// native methods counts into this list (design §5.6).
    user_functions: Vec<UserFunction>,
    /// `ZEND_MAP_PTR(op_array->static_variables_ptr)` for every function
    /// and main op array whose `static` variables the request has
    /// touched: the values, one per entry of the op array's
    /// [`php_unit::OpArray::bound_vars`], which the published unit never
    /// holds (design §5.14). A closure's are its object's.
    static_vars: HashMap<Statics, Vec<Value>>,
}

/// One row of an object's property table as [`Request::property_rows`]
/// reads it: the name, the value, and for a declared slot the class
/// that declared it and its visibility.
pub(crate) type PropertyRow = (PhpString, Value, Option<(ClassId, Visibility)>);

/// What called the native being served ([`Request::native_caller`]):
/// a user function's frame and its arguments, a code frame, or another
/// native.
pub(crate) enum NativeCaller {
    Function(FrameRef, Vec<Value>),
    Code(FrameRef),
    Native,
}

/// `DEBUG_BACKTRACE_PROVIDE_OBJECT` (`Z/Zend/zend_constants.h`): a
/// method's row carries its receiver.
pub(crate) const DEBUG_BACKTRACE_PROVIDE_OBJECT: i64 = 1;

/// `DEBUG_BACKTRACE_IGNORE_ARGS`: no row carries its arguments.
pub(crate) const DEBUG_BACKTRACE_IGNORE_ARGS: i64 = 2;

/// One bucket of a symbol table, php's zval in `EG(symbol_table)` or a
/// rebuilt one: the value the table owns, or `IS_INDIRECT`, the CV of the
/// frame the name is attached to (`zend_attach_symbol_table`), which
/// owns the value while the frame stands.
///
/// `Held` is the value the table owns kept in a place of its own: php
/// hands a fetch by name for writing the bucket's own `zval *`, and a
/// bucket here moves as the list of names grows, so the first such fetch
/// moves the value into a cell that does not ([`Request::symbol_place`]).
/// The cell is the table's alone and never a PHP reference: what it holds
/// is the bucket's value, a reference among them.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Symbol {
    Value(Value),
    Held(PhpRef),
    Indirect(FrameRef, u32),
}

/// One symbol table: its names in order, and the function frame it was
/// rebuilt for, which frees it when it pops; `EG(symbol_table)` has no
/// owner and lives as long as the request.
#[derive(Debug, Default)]
pub(crate) struct SymbolTable {
    owner: Option<FrameRef>,
    entries: Vec<(Vec<u8>, Symbol)>,
}

/// What `zend_include_or_eval` handed the handler: `ZEND_FAKE_OP_ARRAY`,
/// a once form over a path the table already had, which answers `true`;
/// `NULL`, a path nothing opened or an empty string to `eval`, which
/// answers `false`; or the unit of the file or the string, whose main op
/// array the handler serves on a frame of its own.
pub(crate) enum Opened {
    Already,
    Failed,
    Unit(Arc<Unit>),
}

/// One row of a backtrace before it is an array: the callee, its
/// receiver, the scope it was called in where the callee is a closure,
/// its arguments, the extra named arguments the callee collected, and
/// the file and line of its call site, which a call a native made has
/// none of. php reads the scope off `func->common.scope` of the
/// closure's own copy of the function; the copy here is the `Closure`
/// object the call stands on, so the scope travels beside the callee.
type TraceRow = (
    Traced,
    Option<PhpObject>,
    Option<ClassId>,
    Vec<Value>,
    Vec<(PhpString, Value)>,
    Option<(Vec<u8>, i64)>,
);

/// What a trace row names: the function a link serves, or the construct
/// `zend_fetch_debug_backtrace` names a code frame by, `include` and its
/// kin (`Z/Zend/zend_builtin_functions.c:2154`), with the included file
/// as its one argument where a link stands above the frame, since
/// `include_filename` is the file of the link the walk passed last, and
/// none where the frame is the innermost link.
enum Traced {
    Function(Function),
    Include {
        kind: IncludeKind,
        file: Option<Vec<u8>>,
    },
}

/// What `zend_is_callable_ex` reads off the first user frame: its
/// function's scope, its called scope and its `$this`.
#[derive(Default)]
struct CallFrame {
    scope: Option<ClassId>,
    called: Option<ClassId>,
    this: Option<PhpObject>,
}

/// `zend_fcall_info_cache` as `zend_is_callable_at_frame` fills it: the
/// class whose table the method is looked up in, the called scope, the
/// callee and the receiver.
#[derive(Default)]
struct Fcc {
    calling: Option<ClassId>,
    called: Option<ClassId>,
    function: Option<Callee>,
    object: Option<PhpObject>,
}

/// `fcc->function_handler` once `zend_is_callable_check_func` found
/// one: a function the tables have; the trampoline of a class's
/// `__call`, or of its `__callStatic` where `is_static`, under the name
/// written (`zend_get_call_trampoline_func`); or a closure's own
/// `__invoke` (`zend_get_closure_invoke_method`). The resolver answers
/// the last two without making them, and [`Request::callable_made`]
/// makes them.
#[derive(Clone)]
enum Callee {
    Function(Function),
    Trampoline {
        class: ClassId,
        name: Vec<u8>,
        is_static: bool,
    },
    Invoke(PhpObject),
}

/// What `zend_is_callable_ex` resolved a value to: a callable, or one
/// whose callee is still to be made, with the scope and the receiver it
/// is called on.
enum Resolved {
    Made(Callable),
    Pending {
        held: Value,
        name: String,
        callee: Callee,
        scope: ClassId,
        this: Option<PhpObject>,
    },
}

/// One link of php's `prev_execute_data` chain as this engine holds it:
/// a user frame, with its place among the frames, or a call a native
/// is serving.
enum Link<'a> {
    Frame(&'a VmStack, usize, &'a exec::FrameState),
    Call(&'a exec::CallState),
}

/// What `zend_get_property_offset` places a declared instance property
/// by: the slot it sits in, the visibility a read is held to, the set
/// visibility a write is held to where it has one of its own, and
/// whether the property carries a type.
#[derive(Debug, Clone, Copy)]
struct DeclaredSlot {
    slot: u32,
    /// `property_info->ce`: the class that declared what stands in the
    /// slot, which is the scope its visibility is measured against.
    declaring: ClassId,
    visibility: Visibility,
    set_visibility: Option<Visibility>,
    typed: bool,
    /// `ZEND_ACC_READONLY`, which the set visibility's refusal names.
    readonly: bool,
}

/// What a write to a typed declared slot reads off its
/// `zend_property_info`: the class that declared it, which the
/// `TypeError` of a value the type refuses and the `Error` of a write
/// to a readonly property both name; the declared type; and whether
/// php marked it `ZEND_ACC_READONLY`.
pub(crate) struct TypedProperty {
    pub(crate) declaring: ClassId,
    pub(crate) ty: php_unit::decl::Type,
    pub(crate) readonly: bool,
}

/// Whose `static` table the request holds: a linked function's, the
/// `zend_function` a request's function table or a class's method table
/// holds, or a file's main op array's, by its map pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Statics {
    Function(Function),
    Main(u32),
}

/// Where the `static` variables of what a frame runs stand: in a
/// closure's object, or in one of the request's tables.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StaticsAt {
    Closure(PhpObject),
    Table(Statics),
}

/// One user function or method a request declared: the unit and the
/// body its op array is read from, and for a method the row of its
/// class's method table it is, from which its declaration is read. The
/// two differ for a method a trait gave a class: php copies the
/// `zend_function` into the class with the class as its scope and
/// shares the trait's opcodes (`zend_add_trait_method`), so the row is
/// the class's and the op array the trait's unit's.
pub(crate) struct UserFunction {
    pub(crate) unit: Arc<Unit>,
    pub(crate) body: exec::Body,
    pub(crate) member: Option<Member>,
    /// `ZEND_ACC_DEPRECATED` and `ZEND_ACC_NODISCARD`, which
    /// `zend_compile_func_decl` and the `#[\NoDiscard]` validator raise
    /// from the declaration's attributes, decoded where the function is
    /// declared.
    pub(crate) flags: crate::attributes::Flags,
}

/// A method as its class holds it: the class, `common.scope`, and
/// where among the class's declaration it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) class: ClassId,
    pub(crate) at: MemberAt,
}

/// Where a method stands in its class's declaration: among its methods,
/// `ce->function_table`, or as one of a property's hooks,
/// `prop_info->hooks[kind]`, which php's function table never lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberAt {
    Method(u32),
    Hook { property: u32, kind: HookKind },
}

/// One class a request declared: the unit that holds its declaration and
/// the declaration's index there, and, for a class that uses traits,
/// the class composed with them at its link, which is the table's row
/// in its place (`zend_do_bind_traits`).
pub(crate) struct Declared {
    unit: Arc<Unit>,
    at: u32,
    composed: Option<Box<php_unit::link::Composed>>,
}

impl Declared {
    /// The declaration the table holds for the class.
    fn decl(&self) -> Option<&ClassDecl> {
        match &self.composed {
            Some(composed) => Some(&composed.decl),
            None => self.unit.classes.get(usize::try_from(self.at).ok()?),
        }
    }
}

/// The flags of the function at `at` of a unit's `dynamic` table, one
/// a `DECLARE_FUNCTION` binds or a closure is made of.
fn dynamic_flags(unit: &Unit, at: u32) -> crate::attributes::Flags {
    usize::try_from(at)
        .ok()
        .and_then(|at| unit.dynamic.get(at))
        .map(|(decl, _)| crate::attributes::Flags::of(&decl.attributes))
        .unwrap_or_default()
}

/// The declaration of a user function the request's table holds, from
/// whichever table of its unit the body came out of: one the file
/// declared at its top level, or one a `DECLARE_FUNCTION` bound where
/// it stood. None for a method, whose declaration is its class's.
fn user_function_decl(user: &UserFunction) -> Option<&FunctionDecl> {
    match user.body {
        exec::Body::Function(at) => user
            .unit
            .functions
            .get(usize::try_from(at).ok()?)
            .map(|(decl, _)| decl),
        exec::Body::Dynamic(at) => user
            .unit
            .dynamic
            .get(usize::try_from(at).ok()?)
            .map(|(decl, _)| decl),
        exec::Body::Main | exec::Body::Method { .. } | exec::Body::Hook { .. } => None,
    }
}

impl Request {
    #[allow(clippy::too_many_lines)] // one line per field of the request
    pub(crate) fn new(shared: Arc<Shared>, description: RequestDescription) -> Request {
        let cwd = description.cwd.clone();
        let web = description.is_web;
        let mut resources = Resources::default();
        let stream_type = crate::streams::register(&mut resources);
        let filter_type = resources.register_type(b"stream filter", None);
        // `init_executor`'s `zend_vm_stack_init`: the first page of the
        // VM stack, out of the heap the object store's table stands in.
        // The main chunk holds it, so nothing refuses it; a heap that
        // did would refuse the first push again, with php's fatal.
        let mut heap = Heap::new();
        let mut stack = VmStack::new();
        let _laid = stack.open(&mut heap);
        let mut req = Request {
            // Read before `shared` moves in, as fields are in the order written.
            realpath_cache: RealpathCache::startup(&shared),
            shared,
            #[cfg(feature = "witness-trace")]
            trace: None,
            description,
            heap,
            stack,
            vm: Vm::default(),
            map: MapTable::new(),
            states: Vec::new(),
            ini: Vec::new(),
            error_reporting: 0,
            sink: Sink::Kept(Kept::default()),
            output: Output::activate(web),
            header_callback: None,
            last_error: None,
            startup: None,
            last_fatal_error_backtrace: None,
            ending: None,
            exit_status: 0,
            resources,
            superglobals: Vec::new(),
            armed: Vec::new(),
            tables: vec![SymbolTable::default()],
            included_files: Vec::new(),
            constants: Vec::new(),
            enum_cases: Vec::new(),
            enums_updated: Vec::new(),
            enum_functions: Vec::new(),
            trampolines: Vec::new(),
            deprecating: Vec::new(),
            statics: Vec::new(),
            defaults: Vec::new(),
            guards: Vec::new(),
            ref_sources: Vec::new(),
            weakrefs: crate::weakrefs::Table::default(),
            lazy: lazy::Table::default(),
            next_ref_source: 1,
            wrappers: Vec::new(),
            url_wrappers: None,
            user_stream_current: None,
            in_user_include: false,
            including: None,
            stream_ctx: None,
            pclose_ret: 0,
            filters: Vec::new(),
            error_handling: None,
            error_handling_thrown: None,
            stream_thrown: None,
            filtering: None,
            held_streams: Vec::new(),
            filter_type,
            error_handler: None,
            error_handler_levels: 0,
            error_handlers: Vec::new(),
            exception_handler: None,
            exception_handlers: Vec::new(),
            shutdown_functions: ShutdownFunctions::default(),
            tick_functions: Vec::new(),
            autoloaders: Vec::new(),
            in_autoload: Vec::new(),
            assert_callback: None,
            umask: 0o022,
            stat_cache: crate::wrapper::StatCache::default(),
            cwd,
            stream_type,
            fibers: crate::fiber::Fibers::default(),
            pending: None,
            unwind_exit: false,
            bailout: false,
            display_direct: false,
            phase: Phase::Startup,
            marked_destructed: false,
            returned: Value::null(),
            interrupt: Interrupt::default(),
            timer: None,
            stack_limit: StackLimit::UNCHECKED,
            declared: Vec::new(),
            bound_at_load: Vec::new(),
            links: linking::Links::default(),
            keys_made: 0,
            aliases: Vec::new(),
            sapi_constant: Value::null(),
            user_functions: Vec::new(),
            static_vars: HashMap::new(),
            #[cfg(debug_assertions)]
            dirty: None,
        };
        req.activate();
        req
    }

    /// `php_request_startup` and `zend_activate` over a request just
    /// laid: the host's constant, each extension's request state, the
    /// timer, the memory limit, the collector and the error level.
    fn activate(&mut self) {
        if let Some(name) = self
            .shared
            .sapi_constant(self.description.sapi_name.as_bytes())
        {
            self.sapi_constant = name;
        }
        // Request state in bundle order (ADR-0005).
        for extension in &self.shared.extensions {
            if let Some((type_id, build)) = &extension.request_state {
                self.states.push((*type_id, build()));
            }
        }
        // The execution timer, as `zend_set_timeout` arms one at the
        // start of a request (design §5.8), and the memory limit, as
        // `php_request_startup` sets one from the ini.
        self.arm_timer();
        self.set_memory_limit();
        // `zend.enable_gc` as the startup ini leaves the collector
        // (`OnUpdateGCEnabled`), and `gc_reset` from `zend_activate`.
        let enable_gc = ini_truth(self.ini_get(b"zend.enable_gc"));
        self.heap.gc_startup(enable_gc);
        self.update_error_reporting();
        self.stack_limit = self.shared.stack_limit();
    }

    /// The thread's stack limit as request start read it, for the
    /// compiler of whatever this request compiles.
    pub(crate) fn stack_limit(&self) -> StackLimit {
        self.stack_limit
    }

    /// `zend_unset_timeout` then `zend_set_timeout` over
    /// `max_execution_time` as the ini table has it now: at request
    /// start, and again whenever the entry moves, which is php's
    /// `OnUpdateTimeout` at runtime. Whatever was armed is unset first,
    /// so the seconds count from here.
    fn arm_timer(&mut self) {
        self.timer = None;
        self.timer = crate::interrupt::Timer::arm(&self.interrupt, self.max_execution_time());
    }

    // -------------------------------------------------- crate-internal

    pub(crate) fn stack_mut(&mut self) -> &mut VmStack {
        &mut self.stack
    }

    /// The first page of the VM stack the request runs on, out of its
    /// heap, where none stands: what `zend_fiber_execute` takes for a
    /// fiber's stack before the fiber's function is called.
    pub(crate) fn open_stack(&mut self) -> Result<(), Fatal> {
        self.stack.open(&mut self.heap)
    }

    /// The engine's half of the frames: what the loop and the handlers
    /// read (design §5.6).
    pub(crate) fn vm(&self) -> &Vm {
        &self.vm
    }

    pub(crate) fn vm_mut(&mut self) -> &mut Vm {
        &mut self.vm
    }

    /// The fibers running on this request (design §5.11).
    pub(crate) fn fibers(&self) -> &crate::fiber::Fibers {
        &self.fibers
    }

    pub(crate) fn fibers_mut(&mut self) -> &mut crate::fiber::Fibers {
        &mut self.fibers
    }

    /// The half of `zend_fiber_switch_context` that is the engine's:
    /// the execution state the request runs on and `other` change
    /// places, `zend_fiber_capture_vm_state` and
    /// `zend_fiber_restore_vm_state` as one swap. The VM stack carries
    /// the current frame and the entry slot with it, so neither is
    /// saved on its own.
    pub(crate) fn switch_execution(&mut self, other: &mut crate::fiber::Execution) {
        std::mem::swap(&mut self.stack, &mut other.stack);
        std::mem::swap(&mut self.vm, &mut other.vm);
        std::mem::swap(&mut self.error_reporting, &mut other.error_reporting);
        std::mem::swap(&mut self.stack_limit, &mut other.stack_limit);
    }

    /// The stacks php's one `prev_execute_data` chain runs through: the
    /// one the request is running on, then each resumer's below it, the
    /// request's own last.
    fn executions(&self) -> impl Iterator<Item = (&VmStack, &Vm)> {
        std::iter::once((&self.stack, &self.vm)).chain(
            self.fibers
                .below()
                .map(|execution| (&execution.stack, &execution.vm)),
        )
    }

    /// The innermost user frame on the chain, which is where
    /// `zend_get_executed_filename` and `zend_get_executed_lineno` stop:
    /// on a fiber whose callable is a native, the resumer's.
    fn innermost_frame(&self) -> Option<&exec::FrameState> {
        self.executions().find_map(|(_, vm)| vm.innermost())
    }

    /// `zend_objects_store_mark_destructed` has marked the store: no
    /// destructor is served from here, a fiber's teardown among them.
    pub(crate) fn destructors_marked(&self) -> bool {
        self.marked_destructed
    }

    /// The loop over a generator's frame, bound to the engine's tier
    /// ([`crate::tier::resume`]).
    pub(crate) fn resume_loop(&self) -> fn(&mut Request, FrameRef) -> Result<(), Fatal> {
        self.shared.resume
    }

    /// Push a frame for `unit`'s op array: its shape on the VM stack and
    /// its engine state, which are pushed and popped together and share a
    /// `FrameRef`. A VM stack whose page the memory limit refuses is
    /// the fatal php raises there, and nothing is pushed.
    pub(crate) fn push_frame(&mut self, unit: Arc<Unit>) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let frame = self.stack.push_frame(&mut self.heap, slots, 0)?;
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        self.vm.push(unit);
        // `zend_execute` gives the main op array's frame `EG(symbol_table)`
        // and `i_init_code_execute_data` attaches it, so every CV name
        // stands in the table, in the compiler's order, from the start.
        if let Some(state) = self.vm.at_mut(frame) {
            state.symbol_table = Some(TableId::GLOBAL);
        }
        self.attach_symbol_table(frame);
        Ok(frame)
    }

    /// The frame for the main op array of a file an `INCLUDE_OR_EVAL`
    /// opened, `zend_vm_stack_push_call_frame` under
    /// `ZEND_CALL_NESTED_CODE | ZEND_CALL_HAS_SYMBOL_TABLE` and
    /// `i_init_code_execute_data` (`Z/Zend/zend_vm_def.h:6590`,
    /// `Z/Zend/zend_execute.c:4504`): the includer's receiver and called
    /// scope carried over, the receiver with one more owner since the pop
    /// gives one up, the includer's scope written onto the op array
    /// (`new_op_array->scope`), and the includer's symbol table shared,
    /// rebuilt for it first where it is a function frame with none
    /// ([`Request::rebuild_symbol_table`]); then the new frame attached
    /// to it, so the included code reads the includer's variables.
    /// `caller` is `prev_execute_data`, the frame whose opline is the
    /// `INCLUDE_OR_EVAL`, and `returns_to` its `VAR` where the value is
    /// read. The push comes first, as php's does, so a VM stack page the
    /// memory limit refuses is the fatal and nothing else is touched.
    pub(crate) fn push_code_frame(
        &mut self,
        unit: Arc<Unit>,
        caller: FrameRef,
        returns_to: Option<(FrameRef, u32)>,
    ) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let frame = self.stack.push_frame(&mut self.heap, slots, 0)?;
        let (this, called_scope, scope) = self.vm.at(caller).map_or((None, None, None), |state| {
            (state.this, state.called_scope, self.frame_scope(caller))
        });
        if let Some(this) = this {
            self.addref(&this.as_value());
        }
        let table = self.rebuild_symbol_table(caller);
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        self.vm.push_body(
            unit,
            exec::Body::Main,
            None,
            this,
            called_scope,
            0,
            returns_to,
            Some(caller),
        );
        if let Some(state) = self.vm.at_mut(frame) {
            state.scope = scope;
            state.symbol_table = Some(table);
        }
        self.attach_symbol_table(frame);
        Ok(frame)
    }

    /// `zend_leave_helper`'s arm for a nested code frame
    /// (`Z/Zend/zend_vm_def.h:3030`): the frame's CVs detached into the
    /// table it shares with its includer, the frame popped, and the
    /// includer attached to the table again, so what the included file
    /// wrote to a variable of the includer's is in the includer's CV
    /// when the loop goes on there. A function frame is popped alone,
    /// as `zend_leave_helper`'s other arms pop one.
    pub(crate) fn leave_frame(&mut self, frame: FrameRef) {
        let code_frame = self
            .vm
            .at(frame)
            .is_some_and(|state| state.function.is_none());
        if code_frame {
            self.detach_symbol_table(frame);
        }
        self.pop_frame();
        if code_frame {
            let caller = self.stack.current();
            self.attach_symbol_table(caller);
        }
    }

    /// `CACHED_PTR(num)`: what the runtime cache of the op array `frame`
    /// serves holds at `slot`, in this request's map table
    /// ([`exec::Vm::cached`]).
    pub(crate) fn cached(&self, frame: FrameRef, slot: CacheSlot) -> exec::Cached {
        self.vm.cached(&self.map, frame, slot)
    }

    /// `CACHE_PTR(num, ptr)`: `value` into the runtime cache of the op
    /// array `frame` serves at `slot`, in this request's map table
    /// ([`exec::Vm::prime`]), where every pass after reads it.
    pub(crate) fn prime(&mut self, frame: FrameRef, slot: CacheSlot, value: exec::Cached) {
        self.vm.prime(&mut self.map, frame, slot, value);
    }

    /// Push a frame for the body of the user function `function`, the
    /// call that reached it served (`zend_vm_stack_push_call_frame` and
    /// `i_init_func_execute_data`, design §5.6): the CVs and the
    /// temporaries as slots, the arguments the call placed written into
    /// the first CVs, which are the parameters, the ones past the
    /// declared parameters kept beside the frame, and the receiver and
    /// the called scope carried over; `returns_to` is the caller's slot
    /// for what the frame returns and `caller` the frame whose opline
    /// pushed the call, `None` where a native did. The frame's room on
    /// the VM stack is php's `zend_vm_calc_used_stack`, the extra
    /// arguments' slots with it, and a page the memory limit refuses is
    /// the fatal php raises at the push; a function that is no user
    /// function is an internal error.
    pub(crate) fn push_user_frame(
        &mut self,
        function: Function,
        call: exec::CallState,
        returns_to: Option<(FrameRef, u32)>,
        caller: Option<FrameRef>,
    ) -> Result<FrameRef, Fatal> {
        let no_body = || {
            Fatal::Error(String::from(
                "InternalError: a user function whose body the unit has not",
            ))
        };
        let (unit, body) = {
            let user = self.user_function(function).ok_or_else(no_body)?;
            (Arc::clone(&user.unit), user.body)
        };
        let code = exec::body_of(&unit, body).ok_or_else(no_body)?;
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        // The declared parameters, which is php's `common.num_args` and
        // excludes a variadic: an argument from the variadic's position
        // on is an extra one, where php leaves it on the stack for
        // `RECV_VARIADIC` to collect.
        let parameters = self.function_decl(function).map_or(0, |decl| {
            decl.parameters.iter().filter(|p| !p.variadic).count()
        });
        let num_args = u32::try_from(call.arguments.len()).unwrap_or(u32::MAX);
        let extra_args = call.arguments.len().saturating_sub(parameters);
        let frame = self.stack.push_frame(&mut self.heap, slots, extra_args)?;
        let closure = call.closure;
        exec::init_runtime_cache(&mut self.map, code, body);
        self.vm.push_body(
            unit,
            body,
            Some(function),
            call.this,
            call.called_scope,
            num_args,
            returns_to,
            caller,
        );
        let mut extra = Vec::new();
        for (at, argument) in call.arguments.into_iter().enumerate() {
            if at < parameters {
                let previous =
                    self.set_slot(frame, u32::try_from(at).unwrap_or(u32::MAX), argument);
                self.release(previous);
            } else {
                extra.push(argument);
            }
        }
        if let Some(state) = self.vm.at_mut(frame) {
            state.extra_args = extra;
            state.extra_named = call.extra_named;
            state.closure = closure;
        }
        if let Some(name) = call.name {
            self.release(Value::string(name));
        }
        for coerced in call.coerced {
            self.release(coerced);
        }
        Ok(frame)
    }

    /// The frame off both stacks, every owner its slots held given up
    /// while the frame still stands, `i_free_compiled_variables` over
    /// the CVs, each a possible root of the cycle collector as php's
    /// `i_zval_ptr_dtor` makes it, and the temporaries after them as
    /// `FREE_OP` gives one up, since a temporary the compiler numbered
    /// for an opline's value and nothing read is still an owner here;
    /// then, in `zend_leave_helper`'s order, the extra named arguments,
    /// the extra arguments (`zend_vm_stack_free_extra_args`, `nogc`),
    /// the receiver (`ZEND_CALL_RELEASE_THIS`) and the closure, all of
    /// them while the frame still stands on both stacks, as php's
    /// `vm_stack_top` is only put back after them, so a destructor one
    /// of them serves pushes its frames above it; then
    /// `zend_vm_stack_free_call_frame`.
    pub(crate) fn pop_frame(&mut self) {
        let frame = self.stack.current();
        let slots = self.stack.frame(frame).map_or(0, php_heap::Frame::len);
        let cvs = self
            .vm
            .serving(frame)
            .and_then(|serving| serving.code().map(|code| code.num_cvs as usize))
            .unwrap_or(slots);
        for at in 0..slots {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            if at < cvs {
                self.release(value);
            } else {
                self.release_nogc(value);
            }
        }
        self.free_rebuilt_table(frame);
        let held = self.vm.at_mut(frame).map(|state| {
            (
                std::mem::take(&mut state.extra_named),
                std::mem::take(&mut state.extra_args),
                state.this.take(),
                state.closure.take(),
            )
        });
        if let Some((named, extra_args, this, closure)) = held {
            self.release_named(named);
            for extra in extra_args {
                self.release_nogc(extra);
            }
            if let Some(this) = this {
                self.release(this.as_value());
            }
            if let Some(closure) = closure {
                self.release(closure.as_value());
            }
        }
        self.vm.pop();
        self.stack.pop_frame(&mut self.heap);
    }

    /// The value in a slot of `frame`, a view of it: `ZVAL_COPY_VALUE`,
    /// no owner taken, off the heap's frame (design §5.6). A slot past
    /// the end reads as `UNDEF`, as an uninitialised zval reads.
    pub(crate) fn slot(&self, frame: FrameRef, at: u32) -> Value {
        self.stack
            .frame(frame)
            .map_or(Value::undef(), |f| Value::of_zval(f.slot(Slot(at))))
    }

    /// The value out of a slot, the slot left `UNDEF`: what the slot
    /// owned is the caller's now, which is how a temporary is consumed.
    pub(crate) fn take_slot(&mut self, frame: FrameRef, at: u32) -> Value {
        let Some(f) = self.stack.frame_mut(frame) else {
            return Value::undef();
        };
        let z = f.slot(Slot(at));
        f.set(Slot(at), Zval::UNDEF);
        Value::of_zval(z)
    }

    /// `value` into a slot, the slot owning it from here, and what the
    /// slot held handed back for the caller to release; a slot past the
    /// end takes nothing and hands `value` back.
    ///
    /// php writes an opline's `TMP` with `ZVAL_COPY_VALUE` and never
    /// releases what stood there, because a live temporary is never
    /// written over: the handler that reads one frees it or moves it out.
    /// Here the write hands the old value back to be released, so a `TMP`
    /// whose owner a handler moved without emptying the slot is released
    /// a second time by the next write (#723). A debug build stops that
    /// write: the `TMP` the opline the frame stands at writes, found
    /// holding an owner, is recorded for [`Request::take_sensed`]. A
    /// scalar left there is no owner and no second release, and php
    /// writes over one itself: `JMPZ_EX` and `JMPNZ_EX` write the `TMP`
    /// the `BOOL` after them writes again.
    pub(crate) fn set_slot(&mut self, frame: FrameRef, at: u32, value: Value) -> Value {
        let Some(f) = self
            .stack
            .frame_mut(frame)
            .filter(|f| (at as usize) < f.len())
        else {
            return value;
        };
        let old = f.slot(Slot(at));
        f.set(Slot(at), value.to_zval());
        #[cfg(debug_assertions)]
        if old.is_refcounted() {
            self.sense_dirty_temporary(frame, at, old.kind());
        }
        Value::of_zval(old)
    }

    /// A debug build's question at a write over a slot that held an
    /// owner of a `held`: whether the slot is the `TMP` of the opline
    /// `frame` stands at, which the handler that read it last should
    /// have left empty. The first such write is recorded.
    #[cfg(debug_assertions)]
    fn sense_dirty_temporary(&mut self, frame: FrameRef, at: u32, held: php_heap::ZvalType) {
        if self.dirty.is_some() {
            return;
        }
        let opline = self.vm.opline(frame);
        let Some(serving) = self.vm.serving(frame) else {
            return;
        };
        let Some(op) = serving
            .code()
            .and_then(|code| exec::opline_at(code, opline).map(|op| (code, *op)))
        else {
            return;
        };
        let (code, op) = op;
        let its_own = match op.out_type {
            php_unit::OperandKind::Tmp => exec::temporary_slot(code, op.out) == at,
            php_unit::OperandKind::Var
            | php_unit::OperandKind::Cv
            | php_unit::OperandKind::Const
            | php_unit::OperandKind::Unused => false,
        };
        if its_own {
            self.dirty = Some(DirtyTemporary {
                opline,
                opcode: op.opcode,
                number: op.out,
                held,
            });
        }
    }

    /// What a debug build's two sensors found since the loop last asked
    /// (design §10, #761): a `TMP` written over an owner it still held,
    /// or a refcount operation on a block the heap had freed, as the
    /// `InternalError` fatal that ends the request; the first of them,
    /// and taken. The loop asks after every handler.
    #[cfg(debug_assertions)]
    pub(crate) fn take_sensed(&mut self) -> Option<Fatal> {
        let dirty = self.dirty.take().map(|dirty| {
            Fatal::Error(format!(
                "InternalError: {} at opline {} writes its TMP #{} over an owner it still holds ({:?}): the handler that last read the slot moved it out and did not empty it",
                dirty.opcode.name(),
                dirty.opline,
                dirty.number,
                dirty.held,
            ))
        });
        dirty.or_else(|| self.heap.take_freed_use())
    }

    /// The zval in a slot of `frame` in place, php's `EX_VAR(var)`
    /// (design §5.6): what a `FETCH_*_W` points its `VAR` at, and what
    /// the `ASSIGN*` over that `VAR` reads through when an `IS_INDIRECT`
    /// stands there (`_get_zval_ptr_ptr_var`). None for a slot the
    /// frame does not have.
    pub(crate) fn slot_ptr(&mut self, frame: FrameRef, at: u32) -> Option<ZvalRef> {
        self.stack.frame_mut(frame)?.slot_ptr(Slot(at))
    }

    /// The served call off the stack, `zend_vm_stack_free_args` over what
    /// the `SEND_*` oplines placed and `zend_vm_stack_free_call_frame`
    /// after it. A native's argument is given up as php's `nogc` gives it
    /// up and is no possible root of the cycle collector, where a user
    /// function's, a CV of its frame, is one when the frame is left.
    ///
    /// ```php
    /// $o = new stdClass;
    /// var_dump(is_object($o), gc_status()["roots"]);
    /// function f($x) {}
    /// f($o);
    /// var_dump(gc_status()["roots"]);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(0)
    /// int(1)
    /// ```
    pub(crate) fn pop_call(&mut self) {
        if let Some(call) = self.vm.pop_call() {
            // `zend_vm_stack_free_args`, whose `zval_ptr_dtor_nogc` makes
            // no argument a possible root; the receiver and the closure
            // after them are `OBJ_RELEASE`, which does.
            for value in call.coerced {
                self.release_nogc(value);
            }
            for value in call.arguments {
                self.release_nogc(value);
            }
            if let Some(name) = call.name {
                self.release(Value::string(name));
            }
            if let Some(this) = call.this {
                self.release(this.as_value());
            }
            if let Some(closure) = call.closure {
                self.release(closure.as_value());
            }
            self.release_named(call.extra_named);
        }
    }

    /// `zend_free_extra_named_params`: each name and value of a call's
    /// or a frame's extra named arguments given up.
    fn release_named(&mut self, named: Vec<(PhpString, Value)>) {
        for (name, value) in named {
            self.release(Value::string(name));
            self.release(value);
        }
    }

    /// `zend_call_function` over a native method: the call frame pushed
    /// as `INIT_METHOD_CALL` and the `SEND_*` oplines would push it, the
    /// receiver and each argument one more owner the frame holds, the
    /// entry served, and the frame popped whether it returned or threw.
    /// `extra_named` is what `zend_handle_named_arg` collected for a
    /// variadic callee, carried on the call as
    /// `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`.
    pub(crate) fn serve_method(
        &mut self,
        function: Function,
        this: Option<PhpObject>,
        called_scope: ClassId,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        self.serve_call(
            function,
            this,
            Some(called_scope),
            None,
            arguments,
            extra_named,
        )
    }

    /// `zend_call_known_fcc` over a `Closure`, which is what
    /// `Closure::__invoke` serves: the payload's function on the
    /// payload's receiver and scope, with the closure itself on the
    /// call so the body's `BIND_STATIC` prologue finds the variables it
    /// bound (`ZEND_CALL_CLOSURE`). The closure is one more owner for
    /// the span of the call, as php's `zend_init_func_execute_data`
    /// takes one.
    pub(crate) fn serve_closure(
        &mut self,
        object: PhpObject,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        let Some(closure) = object.closure(self).cloned() else {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                "Closure object cannot have properties",
            ));
        };
        self.addref(&object.as_value());
        self.serve_call(
            closure.function,
            closure.this,
            closure.called_scope,
            Some(object),
            arguments,
            extra_named,
        )
    }

    /// `Closure::call`'s serving (`Z/Zend/zend_closures.c:190-231`): the
    /// closure's function on `this`, in `scope`, with the closure itself
    /// on the call so the body's `BIND_STATIC` prologue finds what it
    /// bound. php serves a copy of the function whose scope is `scope`
    /// and puts no object in the store for it; here the closure's own
    /// payload carries the scope for the length of the call and has its
    /// own back after, so no object id is spent where php spends none.
    pub(crate) fn serve_closure_as(
        &mut self,
        object: PhpObject,
        this: PhpObject,
        scope: ClassId,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        let Some(closure) = object.closure(self).cloned() else {
            return Ok(Value::null());
        };
        let held = object
            .native_mut::<ClosureInfo>(self)
            .map(|payload| payload.scope.replace(scope));
        self.addref(&object.as_value());
        let served = self.serve_call(
            closure.function,
            Some(this),
            Some(scope),
            Some(object),
            arguments,
            extra_named,
        );
        if let Some(held) = held
            && let Some(payload) = object.native_mut::<ClosureInfo>(self)
        {
            payload.scope = held;
        }
        served
    }

    /// Whether a closure's function is `ZEND_ACC_STATIC`: a closure
    /// written `static function`, or one made of a `static` method.
    ///
    /// ```php
    /// class F { static function s() {} function q() { return static function () {}; } }
    /// $r = fn ($c) => (new ReflectionFunction($c))->isStatic();
    /// var_dump($r(function () {}), $r(static function () {}), $r((new F)->q()), $r(Closure::fromCallable(['F', 's'])));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    #[must_use]
    pub fn closure_refuses_receiver(&self, closure: &ClosureInfo) -> bool {
        if let Some(decl) = self.method_decl(closure.function) {
            return !decl.is_instance;
        }
        self.closure_code(closure.function)
            .is_some_and(|code| code.no_receiver)
    }

    /// `ZEND_ACC_USES_THIS`, which `zend_compile` sets on an op array
    /// that reads `$this`: a `FETCH_THIS`, an `ISSET_ISEMPTY_THIS`, or an
    /// object opcode whose object operand is the frame's own (`UNUSED`).
    pub(crate) fn closure_uses_this(&self, closure: &ClosureInfo) -> bool {
        use php_unit::{Opcode, OperandKind};
        let Some(code) = self.closure_code(closure.function) else {
            return false;
        };
        code.oplines.iter().any(|op| {
            matches!(op.opcode, Opcode::FetchThis | Opcode::IssetIsemptyThis)
                || (op.op1_type == OperandKind::Unused
                    && matches!(
                        op.opcode,
                        Opcode::FetchObjR
                            | Opcode::FetchObjW
                            | Opcode::FetchObjRw
                            | Opcode::FetchObjIs
                            | Opcode::FetchObjUnset
                            | Opcode::FetchObjFuncArg
                            | Opcode::AssignObj
                            | Opcode::AssignObjOp
                            | Opcode::AssignObjRef
                            | Opcode::PreIncObj
                            | Opcode::PreDecObj
                            | Opcode::PostIncObj
                            | Opcode::PostDecObj
                            | Opcode::UnsetObj
                            | Opcode::IssetIsemptyPropObj
                            | Opcode::InitMethodCall
                    ))
        })
    }

    /// The op array a user function serves, none for a native.
    fn closure_code(&self, function: Function) -> Option<&php_unit::OpArray> {
        let user = self.user_function(function)?;
        exec::body_of(&user.unit, user.body)
    }

    /// The live object the store holds under the id `spl_object_id`
    /// reports, none for an id no live object has.
    pub(crate) fn stored_object(&self, id: u32) -> Option<PhpObject> {
        self.heap.stored(php_heap::ObjectId(id)).map(PhpObject::of)
    }

    /// `ce->type == ZEND_INTERNAL_CLASS`: a class the engine or an
    /// extension declared, and not one a program did.
    pub(crate) fn class_is_internal(&self, class: ClassId) -> bool {
        usize::try_from(class.0).is_ok_and(|at| at < self.shared.classes.len())
    }

    /// `func->common.function_name`: a function's name as declared.
    pub(crate) fn function_name(&self, function: Function) -> String {
        self.declared_name(function)
    }

    /// The `Closure` whose body is the first user frame standing, what
    /// `Closure::getCurrent` reads off `EX(prev_execute_data)`: none
    /// where that frame serves no closure, or a closure made of a named
    /// function (`ZEND_ACC_FAKE_CLOSURE`).
    pub(crate) fn calling_closure(&self) -> Option<PhpObject> {
        for link in self.chain() {
            if let Link::Frame(_, _, state) = link {
                let closure = state.closure?;
                return closure
                    .closure(self)
                    .filter(|info| !info.fake)
                    .map(|_| closure);
            }
        }
        None
    }

    /// `zend_call_function` over a native function: [`Request::serve_method`]
    /// with no receiver and no called scope, what `Closure::__invoke`
    /// serves for a closure over a function.
    pub(crate) fn serve_function(
        &mut self,
        function: Function,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        self.serve_call(function, None, None, None, arguments, extra_named)
    }

    fn serve_call(
        &mut self,
        function: Function,
        this: Option<PhpObject>,
        called_scope: Option<ClassId>,
        closure: Option<PhpObject>,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        // `zend_call_function`: the deprecation before the arguments
        // are placed, and no call where a handler threw.
        if self.function_flags(function).deprecated {
            crate::attributes::deprecated_function(self, function)?;
        }
        let wrapped = self.values_for_references(function, arguments)?;
        let name = self.declared_name(function);
        let name = self.interned(name.as_bytes())?;
        let mut sent = arguments.to_vec();
        for (at, argument) in sent.iter_mut().enumerate() {
            self.addref(argument);
            if wrapped.contains(&at) {
                // `Z_TRY_ADDREF_P(arg); ZVAL_NEW_REF(param, arg)`: the
                // owner just taken is the new cell's, and the frame
                // holds the cell.
                *argument = PhpRef::new(self, *argument)?.as_value();
            }
        }
        for (extra, value) in extra_named {
            self.addref(&Value::string(*extra));
            self.addref(value);
        }
        if let Some(this) = this {
            self.addref(&this.as_value());
        }
        // `ZEND_CALL_MAY_HAVE_UNDEF`: a named argument skipped a slot.
        let may_have_undef = sent
            .iter()
            .any(|argument| matches!(argument.as_ref(), ValueRef::Undef));
        let call = CallState {
            function,
            name: Some(name),
            count: sent.len(),
            arguments: sent,
            this,
            called_scope,
            closure,
            extra_named: extra_named.to_vec(),
            stage: CallStage::Serving,
            frameless: false,
            dynamic: false,
            coerced: Vec::new(),
            send_arg_by_ref: false,
        };
        self.vm.push_call(call);
        if may_have_undef && let Err(thrown) = crate::api::call::handle_undef_args(self) {
            self.pop_call();
            return Err(thrown);
        }
        let function = match crate::trampoline::hand_to_magic(self, function) {
            Ok(function) => function,
            Err(thrown) => {
                self.pop_call();
                return Err(thrown);
            }
        };
        if self.user_function(function).is_some() {
            let Some(call) = self.vm.pop_call() else {
                return self.fatal_of(Err(Fatal::Error(String::from(
                    "InternalError: the call just pushed is not on the call stack",
                ))));
            };
            return self.serve_user(function, call);
        }
        let served = self.serve_native(function);
        self.pop_call();
        served
    }

    /// `zend_call_function`'s walk of `fci->params`
    /// (`Z/Zend/zend_execute_API.c:883-902`) over the by-reference
    /// parameters: an argument that is no reference, handed to a
    /// parameter `ARG_SHOULD_BE_SENT_BY_REF` and not
    /// `ARG_MAY_BE_SENT_BY_REF`, is `zend_param_must_be_ref`'s warning,
    /// and the call goes on with the value wrapped in a reference of its
    /// own, so what the callee writes through it is lost. The positions
    /// to wrap come back; a warning a handler turned into a throw stops
    /// the call before it is made, as php's `cleanup_args` does, and is
    /// the `Err`. The parameter is named where it is one of the declared
    /// ones, `get_function_arg_name`, and a variadic's is not.
    ///
    /// ```php
    /// function f(&$v) { $v = 1; return 'f'; }
    /// class C { static function m(&$v, ...$rest) { return 'm'; } }
    /// $x = 5;
    /// var_dump(call_user_func('f', $x), $x, array_map('f', [1]), call_user_func('C::m', 1, 2));
    /// ```
    /// ```expect
    /// Warning: f(): Argument #1 ($v) must be passed by reference, value given in %s on line 5
    ///
    /// Warning: f(): Argument #1 ($v) must be passed by reference, value given in %s on line 5
    ///
    /// Warning: C::m(): Argument #1 ($v) must be passed by reference, value given in %s on line 5
    /// string(1) "f"
    /// int(5)
    /// array(1) {
    ///   [0]=>
    ///   string(1) "f"
    /// }
    /// string(1) "m"
    /// ```
    fn values_for_references(
        &mut self,
        function: Function,
        arguments: &[Value],
    ) -> Result<Vec<usize>, Thrown> {
        let Some(decl) = self.function_decl(function) else {
            return Ok(Vec::new());
        };
        let variadic = decl.parameters.last().filter(|last| last.variadic);
        let wrapped: Vec<usize> = arguments
            .iter()
            .enumerate()
            .filter(|(_, argument)| {
                !matches!(argument.as_ref(), ValueRef::Undef | ValueRef::Reference(_))
            })
            .filter(|(at, _)| {
                decl.parameters
                    .get(*at)
                    .or(variadic)
                    .is_some_and(|parameter| parameter.by_ref && !parameter.prefer_ref)
            })
            .map(|(at, _)| at)
            .collect();
        for at in &wrapped {
            self.param_must_be_ref(function, u32::try_from(*at + 1).unwrap_or(u32::MAX));
            if self.is_pending() {
                return Err(Thrown::of(self.take_pending()));
            }
        }
        Ok(wrapped)
    }

    /// `zend_param_must_be_ref` (`Z/Zend/zend_execute.c`): the warning
    /// for an argument that is no reference handed to a parameter that
    /// takes one by reference and nothing else, naming the parameter
    /// where it is one of the declared ones (`get_function_arg_name`)
    /// and not where a variadic collects it. What `zend_call_function`,
    /// `SEND_USER` and `SEND_ARRAY` raise before they wrap the value in
    /// a reference of its own and go on.
    pub(crate) fn param_must_be_ref(&mut self, function: Function, number: u32) {
        let callee = self.qualified_name(function, None);
        let at = usize::try_from(number.saturating_sub(1)).unwrap_or(usize::MAX);
        let name = self
            .function_decl(function)
            .and_then(|decl| decl.parameters.get(at))
            .filter(|parameter| !parameter.variadic)
            .map(|parameter| format!(" (${})", String::from_utf8_lossy(&parameter.name.0)))
            .unwrap_or_default();
        let message = format!(
            "{callee}(): Argument #{number}{name} must be passed by reference, value given"
        );
        crate::api::raising::warning(self, &message);
    }

    /// `zend_call_function` over a user function, the branch that calls
    /// `zend_execute_ex` (design §5.6): the frame the call reached
    /// pushed as `DO_UCALL`'s `enter` pushes it, with no caller slot to
    /// return into, and the engine's loop entered on it once more,
    /// bound to the same tier as the serving that is already standing.
    /// Every frame the serving left behind is popped after it, its own
    /// last, since a throwable unwinds to here and not through the
    /// frames it passed.
    ///
    /// What the body returned is the caller's, one owner; what it left
    /// pending is the `Err`, the throwable's one owner with it, so a
    /// native reads a callback's raise the way it reads its own
    /// ([`Request::leave_pending`] puts it back).
    ///
    /// A callback with a declared parameter type is held to it here as
    /// it is held to it under `DO_UCALL`, since one
    /// [`crate::exec::parameter_prologue`] serves both: a typed
    /// parameter reaches its `RECV` whatever the call filled, so the
    /// coercion a scalar argument carries outside `strict_types` is
    /// served and the `TypeError` for what does not convert is raised,
    /// with the boundary named `[internal function]` as
    /// `zend_trace_to_string` names a frame no opline called.
    ///
    /// ```php
    /// function f(int $a) { var_dump($a); }
    /// register_shutdown_function('f', "5");
    /// ```
    /// ```expect
    /// int(5)
    /// ```
    ///
    /// ```php
    /// function f(int $a) { var_dump($a); }
    /// register_shutdown_function('f', []);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: f(): Argument #1 ($a) must be of type int, array given in %s:2
    /// Stack trace:
    /// #0 [internal function]: f(Array)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A fatal the body raises is rendered while the body's frames still
    /// stand, as `php_error_cb` renders it under `zend_error` before
    /// anything unwinds, so the site it names is the body's own line and
    /// not the caller's.
    ///
    /// ```php
    /// set_time_limit(1);
    /// register_shutdown_function(function () {
    ///     for (;;) {}
    /// });
    /// ```
    /// ```expect exit=255 wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 4
    /// ```
    fn serve_user(&mut self, function: Function, call: CallState) -> Result<Value, Thrown> {
        let standing = self.vm.frames().len();
        let passed = call.arguments.len();
        let pushed = self.push_user_frame(function, call, None, None);
        let frame = self.fatal_of(pushed)?;
        // `enter`'s own prologue, so a body reached from here stands
        // where a body reached from `DO_UCALL` stands: every `RECV*`
        // php runs is run, which is where a declared parameter type is
        // checked and a scalar argument coerced, and the entry
        // safepoint is raised after them by the loop below.
        let serving = self.vm.serving(frame);
        let raised = match serving.as_ref().and_then(crate::exec::Serving::code) {
            Some(body) => crate::exec::parameter_prologue(self, frame, body, function, passed),
            None => Err(Fatal::Error(String::from(
                "InternalError: the frame just pushed names a body the unit has not",
            ))),
        };
        let raised = match self.fatal_of(raised) {
            Ok(raised) => raised,
            Err(thrown) => {
                while self.vm.frames().len() > standing {
                    self.pop_frame();
                }
                return Err(thrown);
            }
        };
        if raised.is_some() {
            while self.vm.frames().len() > standing {
                self.pop_frame();
            }
            return match self.pending.take() {
                Some(pending) => Err(Thrown::of(Some(pending))),
                None => Ok(Value::null()),
            };
        }
        let reenter = self.shared.reenter;
        let served = reenter(self, frame);
        // A fatal is rendered where it was raised, `php_error_cb` under
        // `zend_error`, so the frames that raised it still stand while
        // it is: the site a timeout names inside a callback, or inside a
        // fiber's body, is the callback's and not its caller's.
        let served = self.fatal_of(served);
        while self.vm.frames().len() > standing {
            self.pop_frame();
        }
        served?;
        match self.pending.take() {
            Some(pending) => Err(Thrown::of(Some(pending))),
            // The callback exited: what the native reads is php's
            // `EG(exception)` holding the unwind exit, and the native
            // returns with it standing, for the unwinder to go on
            // from the call opline; a fatal leaves the native as it left
            // the callback.
            None if self.unwinding() => Err(Thrown::of(None)),
            None => Ok(std::mem::replace(&mut self.returned, Value::null())),
        }
    }

    /// The entry a `Function` names: one of the natives, or a native
    /// method numbered past them; none for a number nothing is linked
    /// under.
    fn entry_of(&self, function: Function) -> Option<crate::api::abi::Entry> {
        if let Some((_, method)) = self.enum_method_of(function) {
            return Some(method.entry());
        }
        if let Some((trampoline, served)) = self.trampoline_of(function) {
            use crate::trampoline::{Served, Via};
            return match (trampoline.via, served) {
                (Via::Invoke(_), Served::Trampoline) => {
                    Some(crate::classes::closure_invoke as crate::api::abi::Entry)
                }
                (Via::Magic(_), Served::ClosureCall) => {
                    Some(crate::trampoline::closure_call_magic as crate::api::abi::Entry)
                }
                (Via::Magic(_), Served::Trampoline) | (Via::Invoke(_), Served::ClosureCall) => None,
            };
        }
        let at = usize::try_from(function.0).ok()?;
        match at.checked_sub(self.shared.functions.len()) {
            None => self.shared.functions.get(at).map(|(_, entry)| *entry),
            Some(method) => self.shared.methods.get(method).map(|m| m.entry),
        }
    }

    /// Whether `function` is a native, php's `ZEND_INTERNAL_FUNCTION`:
    /// numbered below the user functions and methods.
    pub(crate) fn is_native_function(&self, function: Function) -> bool {
        self.user_function(function).is_none()
    }

    /// The native method `function` is, if it is one: its class and its
    /// index among the class's declared methods.
    fn native_method(&self, function: Function) -> Option<&NativeMethod> {
        let at = usize::try_from(function.0).ok()?;
        self.shared
            .methods
            .get(at.checked_sub(self.shared.functions.len())?)
    }

    /// The user function or method `function` is, if it is one: numbered
    /// past the natives and the native methods.
    pub(crate) fn user_function(&self, function: Function) -> Option<&UserFunction> {
        let at = usize::try_from(function.0).ok()?;
        self.user_functions.get(
            at.checked_sub(self.shared.functions.len())?
                .checked_sub(self.shared.methods.len())?,
        )
    }

    /// `fbc->common.fn_flags & (ZEND_ACC_DEPRECATED | ZEND_ACC_NODISCARD)`:
    /// what the callee's declaration raised where it was linked, none
    /// for an enum's own method and the pass function.
    pub(crate) fn function_flags(&self, function: Function) -> crate::attributes::Flags {
        if let Some(user) = self.user_function(function) {
            return user.flags;
        }
        if let Some(method) = self.native_method(function) {
            return method.flags;
        }
        usize::try_from(function.0)
            .ok()
            .and_then(|at| self.shared.function_flags.get(at))
            .copied()
            .unwrap_or_default()
    }

    /// The number a user function at `index` of the table is known by.
    fn user_function_id(&self, index: usize) -> Option<Function> {
        let at = self
            .shared
            .functions
            .len()
            .checked_add(self.shared.methods.len())?
            .checked_add(index)?;
        u32::try_from(at).ok().map(Function)
    }

    /// The class a method belongs to, `func->common.scope`: a native
    /// method's class, or the request's `ClassId` for the class of a
    /// user method; none for a function.
    pub(crate) fn method_class(&self, function: Function) -> Option<ClassId> {
        if let Some(method) = self.native_method(function) {
            // A parent hook's trampoline has the property's class as its
            // scope, which its call carries and no row of the table does.
            if matches!(method.at, MethodAt::ParentHook { .. }) {
                return None;
            }
            return Some(method.class);
        }
        if let Some((class, _)) = self.enum_method_of(function) {
            return Some(class);
        }
        if let Some((trampoline, _)) = self.trampoline_of(function) {
            return match trampoline.via {
                crate::trampoline::Via::Magic(magic) => self.method_class(magic),
                crate::trampoline::Via::Invoke(_) => self.class_named(b"Closure"),
            };
        }
        self.user_function(function)?
            .member
            .map(|member| member.class)
    }

    /// The trampoline a function number names and which of its two
    /// functions it is; none for any other function.
    pub(crate) fn trampoline_of(
        &self,
        function: Function,
    ) -> Option<(
        &crate::trampoline::CallTrampoline,
        crate::trampoline::Served,
    )> {
        let (index, served) = crate::trampoline::decode(function)?;
        Some((self.trampolines.get(index)?, served))
    }

    /// `zend_get_call_trampoline_func(ce, name, is_static)`
    /// (`Z/Zend/zend_object_handlers.c:1764`): the trampoline `class`'s
    /// `__call`, or its `__callStatic` where `is_static`, serves under
    /// `name`, the one the request already made for that method and
    /// name where it made one; none where the class has no such
    /// method.
    pub(crate) fn call_trampoline(
        &mut self,
        class: ClassId,
        name: &[u8],
        is_static: bool,
    ) -> Option<Function> {
        let magic = self.method_named(
            class,
            if is_static {
                b"__callstatic"
            } else {
                b"__call"
            },
        )?;
        let held = self
            .trampolines
            .iter()
            .position(|trampoline| trampoline.is(magic, name));
        let at = if let Some(at) = held {
            at
        } else {
            let by_ref = self
                .function_decl(magic)
                .is_some_and(|decl| decl.returns_by_ref);
            self.trampolines
                .push(crate::trampoline::CallTrampoline::new(
                    magic, name, is_static, by_ref,
                ));
            self.trampolines.len().checked_sub(1)?
        };
        crate::trampoline::number(at, crate::trampoline::Served::Trampoline)
    }

    /// `zend_get_closure_invoke_method` (`Z/Zend/zend_closures.c:484`):
    /// the `__invoke` of `closure`, which carries the signature of the
    /// function it holds, the one the request already made for that
    /// function where it made one; none for an object that is no
    /// closure.
    pub(crate) fn invoke_trampoline(&mut self, closure: PhpObject) -> Option<Function> {
        let function = closure.closure(self)?.function;
        let via = crate::trampoline::Via::Invoke(function);
        let at = if let Some(at) = self.trampolines.iter().position(|made| made.via == via) {
            at
        } else {
            let decl = self.function_decl(function)?.clone();
            self.trampolines
                .push(crate::trampoline::CallTrampoline::invoke(function, &decl));
            self.trampolines.len().checked_sub(1)?
        };
        crate::trampoline::number(at, crate::trampoline::Served::Trampoline)
    }

    /// `get_static_method_fallback` (`Z/Zend/zend_object_handlers.c:2016`):
    /// where the name of a call on a class finds nothing `class` lets the scope
    /// call, the trampoline of `this`'s class's `__call` where the
    /// calling frame has a receiver that is an instance of `class`, and
    /// otherwise `class`'s `__callStatic`'s; none where neither serves.
    pub(crate) fn static_trampoline(
        &mut self,
        class: ClassId,
        name: &[u8],
        this: Option<PhpObject>,
    ) -> Option<Function> {
        if self.method_named(class, b"__call").is_some()
            && let Some(this) = this.filter(|this| self.instance_of(this.class(), class))
        {
            return self.call_trampoline(this.class(), name, false);
        }
        self.call_trampoline(class, name, true)
    }

    /// `zend_hash_find_ptr(&ce->function_table, lcname)` over the
    /// class's native methods: the method under the lowered name, by
    /// identity, the class's own first and then each parent's, since
    /// `zend_do_inheritance` copies a parent's table into the child's
    /// (`ReflectionObject` answers `getName` with `ReflectionClass`'s);
    /// none where no class of the chain has an entry of that name.
    pub(crate) fn method_named(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        let mut at = Some(class);
        let mut depth = 0;
        while let Some(class) = at {
            if let Some(function) = self.own_method_named(class, lowered) {
                return Some(function);
            }
            depth += 1;
            if depth > 64 {
                return None;
            }
            at = self
                .class_decl(class)?
                .parent
                .as_ref()
                .and_then(|name| self.class_named(&name.0));
        }
        None
    }

    /// [`Request::method_named`] over one class's own declarations: a
    /// native class's among the native methods, a user class's among the
    /// user functions declared with it.
    fn own_method_named(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        let decl = self.class_decl(class)?;
        let at = usize::try_from(class.0).ok()?;
        if let Some(declared) = at.checked_sub(self.shared.classes.len()) {
            if decl.kind == ClassKind::Enum
                && let Some(method) = crate::enums::EnumMethod::of(lowered, decl.backing)
            {
                return self.enum_function(class, method);
            }
            self.declared.get(declared)?;
            let at = decl
                .methods
                .iter()
                .position(|method| method.function.name.0.eq_ignore_ascii_case(lowered))?;
            let member = Member {
                class,
                at: MemberAt::Method(u32::try_from(at).ok()?),
            };
            return self.member_function(member);
        }
        self.native_method_id(|m| {
            m.class == class
                && matches!(m.at, MethodAt::Declared(index)
                    if decl
                        .methods
                        .get(usize::try_from(index).unwrap_or(usize::MAX))
                        .is_some_and(|method| method.function.name.0.eq_ignore_ascii_case(lowered)))
        })
    }

    /// The function number `zend_enum_register_funcs` gave `method` of
    /// the enum `class` a program declared.
    fn enum_function(&self, class: ClassId, method: crate::enums::EnumMethod) -> Option<Function> {
        let at = self
            .enum_functions
            .iter()
            .position(|held| *held == (class, method))?;
        crate::enums::ENUM_FUNCTIONS
            .checked_add(u32::try_from(at).ok()?)
            .map(Function)
    }

    /// The enum method a function number past
    /// [`crate::enums::ENUM_FUNCTIONS`] names, and its enum.
    fn enum_method_of(&self, function: Function) -> Option<(ClassId, crate::enums::EnumMethod)> {
        let at = function.0.checked_sub(crate::enums::ENUM_FUNCTIONS)?;
        self.enum_functions.get(usize::try_from(at).ok()?).copied()
    }

    /// The trampoline `class` answers under the lowered name, php's
    /// `ZEND_ACC_CALL_VIA_HANDLER` function: a method the class's
    /// `get_method` synthesises, which `ce->function_table` has no row
    /// for, so [`Request::method_named`] never finds it and no
    /// rendering lists it. `zend_get_closure_invoke_method` is the one
    /// the engine makes, `Closure::__invoke`.
    pub(crate) fn trampoline_named(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        self.native_method_id(|m| {
            m.class == class
                && matches!(m.at, MethodAt::Trampoline(index)
                    if self
                        .shared
                        .trampolines
                        .get(usize::try_from(index).unwrap_or(usize::MAX))
                        .is_some_and(|method| method.function.name.0.eq_ignore_ascii_case(lowered)))
        })
    }

    /// The number of the first native method the predicate takes, which
    /// is its position among `methods` past the natives.
    fn native_method_id(&self, matching: impl FnMut(NativeMethod) -> bool) -> Option<Function> {
        let base = self.shared.functions.len();
        self.shared
            .methods
            .iter()
            .copied()
            .position(matching)
            .and_then(|i| u32::try_from(base.saturating_add(i)).ok())
            .map(Function)
    }

    /// The declaration of a method, native or user, if `function` is one.
    pub(crate) fn method_decl(&self, function: Function) -> Option<&MethodDecl> {
        if let Some((class, method)) = self.enum_method_of(function) {
            return self
                .class_decl(class)?
                .methods
                .iter()
                .find(|decl| decl.function.name.0.eq_ignore_ascii_case(method.name()));
        }
        if let Some((trampoline, _)) = self.trampoline_of(function) {
            return Some(&trampoline.decl);
        }
        if let Some(method) = self.native_method(function) {
            return match method.at {
                MethodAt::Declared(index) => self
                    .class_decl(method.class)?
                    .methods
                    .get(usize::try_from(index).ok()?),
                MethodAt::Trampoline(index) | MethodAt::ParentHook { at: index, .. } => {
                    self.shared.trampolines.get(usize::try_from(index).ok()?)
                }
            };
        }
        let member = self.user_function(function)?.member?;
        let decl = self.class_decl(member.class)?;
        match member.at {
            MemberAt::Method(at) => decl.methods.get(usize::try_from(at).ok()?),
            MemberAt::Hook { property, kind } => decl
                .properties
                .get(usize::try_from(property).ok()?)?
                .hooks
                .as_ref()?
                .of(kind),
        }
    }

    /// The number of the user function a class's declaration holds at
    /// `member`, a method or a hook, among the functions the request
    /// declared with it.
    pub(crate) fn member_function(&self, member: Member) -> Option<Function> {
        let position = self
            .user_functions
            .iter()
            .position(|user| user.member == Some(member))?;
        self.user_function_id(position)
    }

    /// `ZSTR_VAL(ce->name)` as a message's `%s` prints it: the name a
    /// class was declared under up to its NUL, so an anonymous class is
    /// `class@anonymous` ([`php_unit::decl::shown`]), and `object` for
    /// one the tables have no declaration for. A value holding the name,
    /// `get_class()`'s, is [`Request::class_name_bytes`].
    pub(crate) fn class_name(&self, class: ClassId) -> String {
        self.class_decl(class).map_or_else(
            || String::from("object"),
            |decl| php_unit::decl::shown(&decl.name.0).into_owned(),
        )
    }

    /// `ce->name` whole, the NUL and what follows it included.
    pub(crate) fn class_name_bytes(&self, class: ClassId) -> Vec<u8> {
        self.class_decl(class)
            .map_or_else(|| b"object".to_vec(), |decl| decl.name.0.to_vec())
    }

    /// `instanceof_function`: whether `class` is `of`, extends it, or
    /// implements it, the parents walked and each one's interfaces with
    /// it. A chain longer than any declaration can make is cut, so a
    /// declaration naming itself as its parent ends here rather than
    /// never.
    pub(crate) fn instance_of(&self, class: ClassId, of: ClassId) -> bool {
        let mut at = Some(class);
        let mut depth = 0;
        while let Some(class) = at {
            if class == of {
                return true;
            }
            let Some(decl) = self.class_decl(class) else {
                return false;
            };
            if decl
                .interfaces
                .iter()
                .filter_map(|name| self.class_named(&name.0))
                .any(|interface| depth < 64 && self.instance_of(interface, of))
            {
                return true;
            }
            depth += 1;
            if depth > 64 {
                return false;
            }
            at = decl
                .parent
                .as_ref()
                .and_then(|name| self.class_named(&name.0));
        }
        false
    }

    /// `zend_get_executed_scope` (`Z/Zend/zend_execute_API.c:712`): the
    /// chain walked from `EG(current_execute_data)` to the first link
    /// that is user code or a function with a scope, and that link's
    /// `func->common.scope`: a user method's class, a closure's bound
    /// scope, and none for a plain user function or the main op array.
    /// A native with no scope, `constant()` or `get_class()` called
    /// from a method, is walked past, so the scope a native reads is
    /// its caller's; a native method's is its own class.
    pub(crate) fn executed_scope(&self) -> Option<ClassId> {
        for link in self.chain() {
            let (function, closure, carried) = match link {
                Link::Frame(_, _, state) => (state.function, state.closure, state.scope),
                Link::Call(call) => (
                    Some(call.function),
                    call.closure,
                    self.is_parent_hook_trampoline(call.function)
                        .then_some(call.called_scope)
                        .flatten(),
                ),
            };
            let scope = self
                .closure_scope(closure)
                .or_else(|| self.method_class(function?))
                .or(carried);
            let user = function.is_some_and(|function| self.user_function(function).is_some());
            if scope.is_some() || user || matches!(link, Link::Frame(..)) {
                return scope;
            }
        }
        None
    }

    /// `ce->parent`: the linked parent of a class, none at a root.
    pub(crate) fn class_parent(&self, class: ClassId) -> Option<ClassId> {
        self.class_decl(class)?
            .parent
            .as_ref()
            .and_then(|name| self.class_named(&name.0))
    }

    /// `zend_check_protected`: whether `scope` is `class` or one of its
    /// parents, or `class` is `scope` or one of its parents; false with
    /// no scope. Each walk is cut at a depth no declaration can make.
    pub(crate) fn check_protected(&self, class: ClassId, scope: Option<ClassId>) -> bool {
        let Some(scope) = scope else {
            return false;
        };
        let up = |from: ClassId, to: ClassId| {
            let mut at = Some(from);
            for _ in 0..64 {
                match at {
                    Some(here) if here == to => return true,
                    Some(here) => at = self.class_parent(here),
                    None => return false,
                }
            }
            false
        };
        up(class, scope) || up(scope, class)
    }

    /// `zend_std_get_method`'s access check, which
    /// `zend_std_get_static_method` and `zend_std_get_constructor` share:
    /// a public method is anyone's; a private or a protected one is
    /// called from its own class's scope, a protected one from a scope
    /// `zend_check_protected` relates to it, and otherwise php's `Error`,
    /// `zend_bad_method_call`, naming the visibility, the method's class
    /// and the name as `written`, and the calling scope or `global
    /// scope`. A class with `__call` would take the call instead; none
    /// declared here has one.
    pub(crate) fn check_method_access(
        &mut self,
        function: Function,
        written: &[u8],
    ) -> Result<(), Thrown> {
        if self.method_accessible(function) {
            return Ok(());
        }
        let shown = visibility_string(self.method_visibility(function));
        let class = self
            .method_class(function)
            .map_or_else(|| String::from("object"), |class| self.class_name(class));
        let from = self.scope_phrase();
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!(
                "Call to {shown} method {class}::{}() from {from}",
                String::from_utf8_lossy(written)
            ),
        ))
    }

    /// The end of `zend_std_get_static_method`
    /// (`Z/Zend/zend_object_handlers.c:2066`), once the method is found
    /// and the scope may reach it: an abstract method is
    /// `zend_abstract_method_call`'s `Error`, and one whose scope is a
    /// trait, which is a method named on the trait itself and never one
    /// a class took from it, is php's deprecation, after which a
    /// handler that threw stops the call. The answer is whether the
    /// method is a trait's, which is the callee php never caches.
    pub(crate) fn static_method_found(&mut self, function: Function) -> Result<bool, Thrown> {
        let Some(decl) = self.method_decl(function) else {
            return Ok(false);
        };
        let (is_abstract, name) = (decl.is_abstract, decl.function.name.clone());
        let Some(class) = self.method_class(function) else {
            return Ok(false);
        };
        let is_trait = self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait);
        let shown = format!(
            "{}::{}",
            self.class_name(class),
            String::from_utf8_lossy(&name.0)
        );
        if is_abstract {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Cannot call abstract method {shown}()"),
            ));
        }
        if is_trait {
            crate::api::raising::deprecated(
                self,
                &format!(
                    "Calling static trait method {shown} is deprecated, it should only be called on a class using the trait"
                ),
            );
            if self.is_pending() {
                return Err(Thrown::of(None));
            }
        }
        Ok(is_trait)
    }

    /// `zend_check_method_accessible` (`Z/Zend/zend_execute.h:399`): a
    /// public method is anyone's, a private one is its own class's
    /// scope alone, and a protected one is a scope
    /// `zend_check_protected` relates to it. A function that is no
    /// method, and one the tables have no declaration for, is anyone's.
    pub(crate) fn method_accessible(&self, function: Function) -> bool {
        let Some(class) = self.method_class(function) else {
            return true;
        };
        let scope = self.executed_scope();
        match self.method_visibility(function) {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(class, scope),
        }
    }

    /// `fn_flags & (ZEND_ACC_PUBLIC|ZEND_ACC_PROTECTED|ZEND_ACC_PRIVATE)`:
    /// how a method was declared, public for a function that is none.
    fn method_visibility(&self, function: Function) -> Visibility {
        self.method_decl(function)
            .map_or(Visibility::Public, |decl| decl.visibility)
    }

    /// `scope ? "scope " : "global scope"` with the scope's name after
    /// it, the tail php's bad-call texts end in.
    fn scope_phrase(&self) -> String {
        match self.executed_scope() {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        }
    }

    /// `object_init_ex`'s refusals: an interface, a trait, an enum or an
    /// abstract class is php's `Error` for what cannot be instantiated.
    ///
    /// ```php
    /// abstract class A { static function make() { return new static; } }
    /// class B extends A {}
    /// var_dump(B::make());
    /// try { A::make(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// new A;
    /// ```
    /// ```expect exit=255
    /// object(B)#1 (0) {
    /// }
    /// Cannot instantiate abstract class A
    ///
    /// Fatal error: Uncaught Error: Cannot instantiate abstract class A in %s:6
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 6
    /// ```
    pub(crate) fn instantiable(&mut self, class: ClassId) -> Result<(), Thrown> {
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        let what = match decl.kind {
            ClassKind::Interface => "interface",
            ClassKind::Trait => "trait",
            ClassKind::Enum => "enum",
            ClassKind::Class if decl.flags.is_abstract => "abstract class",
            ClassKind::Class => return Ok(()),
        };
        let name = self.class_name(class);
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot instantiate {what} {name}"),
        ))
    }

    /// The name a call frame carries for a native method, the interned
    /// string of its declared name, and the empty string for the pass
    /// function, which has none.
    pub(crate) fn method_name_string(&mut self, function: Function) -> Result<PhpString, Thrown> {
        let name = if function == Function::PASS {
            String::new()
        } else {
            self.declared_name(function)
        };
        self.interned(name.as_bytes())
    }

    /// [`Request::qualified_name`] for a raise about a method called on
    /// its class.
    pub(crate) fn qualified_name_of(&self, function: Function) -> String {
        self.qualified_name(function, None)
    }

    /// An interned string of this request's heap, never refcounted, for
    /// a name the engine holds as bytes; the `Err` is the memory limit
    /// refusing the intern.
    pub(crate) fn interned(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        let interned = self.heap.intern(bytes);
        self.fatal_of(interned).map(PhpString::of)
    }

    /// What the entry frame of the innermost serving returned.
    pub(crate) fn set_returned(&mut self, value: Value) {
        self.returned = value;
    }

    /// Leave what a raise made pending where the loop reads it, php's
    /// `EG(exception)`. A `Thrown` that carries none is a fatal or an
    /// `exit` the raiser already recorded on the request. The
    /// throwable's one owner is the pending slot's from here; a second
    /// throwable raised while one is pending is given up, where php
    /// chains it as the first's `previous` (`zend_throw_exception_internal`),
    /// which arrives with `catch`. One raised while the unwind exit
    /// stands is given up too, which is php's own arm there: what is
    /// unwinding is not replaced by a different throwable
    /// (`Z/Zend/zend_exceptions.c:195-199`).
    pub(crate) fn leave_pending(&mut self, thrown: &Thrown) {
        let Some(object) = thrown.exception().copied() else {
            return;
        };
        if self.pending.is_none() && !self.unwinding() {
            self.pending = Some(object);
        } else {
            self.release(object.as_value());
        }
    }

    /// The pending throwable at the end of a serving, php's
    /// `zend_exception_error` (design §5.9): the throwable's own
    /// `__toString` behind `Uncaught `, with `\n  thrown` after it, given
    /// to `php_error_cb` as `E_ERROR` at the site the throwable recorded
    /// rather than the site the loop has reached. The `in ... on line ...`
    /// that closes php's last line is that call's own tail, which is why
    /// the site appears twice and in two spellings.
    ///
    /// Nothing pending means the request is ending in the fatal or the
    /// `exit` the raiser recorded, and the loop has nothing to add.
    ///
    /// ```php
    /// nope();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// The rows are the frame chain as it stands at the raise, and
    /// `{main}` is the line every trace ends on whether or not the main
    /// frame still does. A destructor the shutdown walk serves stands
    /// with that frame already popped, so it is the outermost row and
    /// php gives it no file: `[internal function]`, the site of a frame
    /// no opline called.
    ///
    /// ```php
    /// class A { function __destruct() { nope(); } }
    /// $a = new A;
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A `ParseError` or a `CompileError`, the class itself and not a
    /// child, is the branch before that one: its message alone, as
    /// `E_PARSE` or `E_COMPILE_ERROR` at the site the throwable recorded,
    /// with no `Uncaught` and no trace, and the request ends as the
    /// diagnostic ends it. `token_get_all()` under `TOKEN_PARSE` is what
    /// throws one here, and its witness in `ext/tokenizer` pins the
    /// rendering; `a_parse_error_is_rendered_as_its_level` in
    /// `api::raising` holds it without the extension.
    pub(crate) fn uncaught(&mut self) -> Result<(), Fatal> {
        // `zend_execute_scripts` and `zend_throw_exception_internal` with
        // no frame standing: a user exception handler takes the
        // throwable first, and what it leaves is rendered.
        self.user_exception_handler();
        let Some(throwable) = self.pending.take() else {
            // `zend_exception_error` on the unwind exit: "We
            // successfully unwound, nothing more to do", and the
            // object is released (`Z/Zend/zend_exceptions.c:1005`); a
            // fatal's bailout is caught where the serving is left.
            self.unwind_exit = false;
            self.bailout = false;
            return Ok(());
        };
        let (file, line) = self.throw_site(throwable);
        let class = self.class_name_of(throwable);
        let compile_time = match class.as_str() {
            "ParseError" => Some(Level::Parse),
            "CompileError" => Some(Level::CompileError),
            _ => None,
        };
        let message = match compile_time {
            Some(_) => string_prop(self, throwable, b"message"),
            None => format!("Uncaught {}\n  thrown", self.throwable_to_string(throwable)),
        };
        self.end_in_uncaught(compile_time.unwrap_or(Level::Error), &message, &file, line);
        let fatal = Fatal::Error(message);
        // `zend_exception_error` ends with `OBJ_RELEASE(ex)`: the pending
        // slot's owner, given up once the rendering has read it.
        self.release(throwable.as_value());
        Err(fatal)
    }

    /// The tail of `zend_call_function` where the callee left something
    /// pending and no frame stands to take it
    /// (`Z/Zend/zend_execute_API.c:1069`), as in an extension's
    /// `RSHUTDOWN` or a shutdown function: `zend_throw_exception_internal`
    /// with no frame hands a throwable to the handler `set_exception_handler` set,
    /// and the call is done where the handler took it; otherwise it is
    /// rendered as the uncaught fatal, and the unwind exit is dropped,
    /// and either leaves by `zend_bailout`, which is the `Thrown` with
    /// no throwable handed back. The session's write at request end is
    /// the witness, in `ext/session`.
    pub(crate) fn thrown_without_frame(&mut self, thrown: &Thrown) -> Result<Value, Thrown> {
        let threw = thrown.exception().is_some();
        self.leave_pending(thrown);
        match self.uncaught() {
            Err(fatal) => {
                self.end_in(&fatal);
                self.bailout = true;
                Err(Thrown::of(None))
            }
            Ok(()) if threw => Ok(Value::null()),
            Ok(()) => {
                self.bailout = true;
                Err(Thrown::of(None))
            }
        }
    }

    /// The declared name of a throwable's class, `Error` for one the
    /// tables do not know.
    fn class_name_of(&self, throwable: PhpObject) -> String {
        self.class_decl(throwable.class()).map_or_else(
            || String::from("Error"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        )
    }

    /// `Exception::__toString` (`Z/Zend/zend_exceptions.c:692`): each
    /// throwable of the `previous` chain rendered by
    /// [`Request::rendering_of`], the deepest first and `this` last,
    /// with `\n\nNext ` between them, which is php's `prev_str` carried
    /// through the loop and put after the one it is rendering. The
    /// walk stops at a throwable it has already rendered, php's
    /// `Z_PROTECT_RECURSION_P`; a chain is a path through live objects,
    /// so the store's count is a length it cannot pass.
    ///
    /// The ` and defined` a `TypeError` whose message names a call site
    /// takes arrives with the frames that make it reachable.
    pub(crate) fn throwable_to_string(&self, throwable: PhpObject) -> String {
        let mut chain = vec![throwable];
        for _ in 0..=self.heap.objects() {
            let Some(at) = chain.last().copied() else {
                break;
            };
            let Some(previous) = self.previous_of(at) else {
                break;
            };
            if chain.iter().any(|seen| seen.index() == previous.index()) {
                break;
            }
            chain.push(previous);
        }
        chain
            .iter()
            .rev()
            .map(|throwable| self.rendering_of(*throwable))
            .collect::<Vec<String>>()
            .join("\n\nNext ")
    }

    /// One throwable of that chain: the class, the message where there
    /// is one, the site behind a colon and the trace under `Stack
    /// trace:`.
    fn rendering_of(&self, throwable: PhpObject) -> String {
        let class = self.class_name_of(throwable);
        let mut message = string_prop(self, throwable, b"message");
        // `Z/Zend/zend_exceptions.c:740`: a `TypeError` or an
        // `ArgumentCountError` whose message names the call site is
        // closed with ` and defined`, so the site the rendering puts
        // after it reads as the function's own. php compares the class
        // itself and not what it descends from.
        if (class == "TypeError" || class == "ArgumentCountError")
            && message.contains(", called in ")
        {
            message.push_str(" and defined");
        }
        let (file, line) = self.throw_site(throwable);
        let file = String::from_utf8_lossy(&file);
        let trace = self.trace_string_of(throwable);
        if message.is_empty() {
            format!("{class} in {file}:{line}\nStack trace:\n{trace}")
        } else {
            format!("{class}: {message} in {file}:{line}\nStack trace:\n{trace}")
        }
    }

    /// The `file` and `line` a throwable recorded where it was made.
    fn throw_site(&self, throwable: PhpObject) -> (Vec<u8>, u32) {
        let file = throwable
            .prop(self, b"file")
            .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default();
        let line = throwable
            .prop(self, b"line")
            .and_then(|value| value.as_int())
            .and_then(|line| u32::try_from(line).ok())
            .unwrap_or(0);
        (file, line)
    }

    /// `zend_default_exception_new`: a throwable records where it was
    /// made, `zend_get_executed_filename` and `zend_get_executed_lineno`,
    /// and the backtrace `zend_fetch_debug_backtrace` builds there, the
    /// array `getTrace` answers, written into the declared `trace` slot;
    /// `getTraceAsString`, `__toString` and the uncaught fatal render it
    /// through [`Request::trace_string_of`] when they are asked.
    pub(crate) fn note_throw_site(&mut self, throwable: PhpObject) -> Result<(), Thrown> {
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        let trace = self.debug_backtrace()?;
        let line = Value::int(i64::from(self.current_line()));
        for (name, value) in [
            (&b"file"[..], Value::string(PhpString::new(self, &file)?)),
            (b"line", line),
            (b"trace", Value::array(trace)),
        ] {
            let name = PhpString::new(self, name)?;
            throwable.set_prop(self, name, value)?;
        }
        Ok(())
    }

    /// `zend_trace_to_string` over the `trace` a throwable holds, with
    /// `include_main`; a slot that holds no array, which is what a
    /// throwable made before this engine recorded one has, renders as
    /// the empty trace does, `#0 {main}`.
    pub(crate) fn trace_string_of(&self, throwable: PhpObject) -> String {
        match throwable
            .prop(self, b"trace")
            .and_then(|value| value.as_array().copied())
        {
            Some(trace) => self.trace_to_string(trace, true),
            None => String::from("#0 {main}"),
        }
    }

    /// `zend_fetch_debug_backtrace` (`Z/Zend/zend_builtin_functions.c`)
    /// with the options a throwable is built under, no `include_main`
    /// and no `DEBUG_BACKTRACE_PROVIDE_OBJECT`: one row per link of the
    /// chain below the op array being served, `file`, `line`,
    /// `function`, then `class` and `type` for a method, then `args`, in
    /// the order php adds the keys. The chain is
    /// [`Request::trace_chain`]; a row whose link stands on a native
    /// names no `file` and no `line`, which is the test php makes on
    /// `prev->func` with `ZEND_USER_CODE`.
    /// The arguments are the frame's, each followed through a reference
    /// and one more owner taken, `debug_backtrace_get_args`; under
    /// `zend.exception_ignore_args` the row has no `args`,
    /// `DEBUG_BACKTRACE_IGNORE_ARGS`, a user frame's row and a native's
    /// alike, and the rendering has nothing between the parentheses. The
    /// `Err` is the memory limit refusing the rows.
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', '1');
    /// class C { function m($a) { return new Exception('m'); } }
    /// function f($x) { return (new C)->m($x); }
    /// var_dump(f(1)->getTrace());
    /// echo f(1)->getTraceAsString(), "\n";
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   array(5) {
    ///     ["file"]=>
    ///     string(%d) "%s.php"
    ///     ["line"]=>
    ///     int(4)
    ///     ["function"]=>
    ///     string(1) "m"
    ///     ["class"]=>
    ///     string(1) "C"
    ///     ["type"]=>
    ///     string(2) "->"
    ///   }
    ///   [1]=>
    ///   array(3) {
    ///     ["file"]=>
    ///     string(%d) "%s.php"
    ///     ["line"]=>
    ///     int(5)
    ///     ["function"]=>
    ///     string(1) "f"
    ///   }
    /// }
    /// #0 %s(4): C->m()
    /// #1 %s(6): f()
    /// #2 {main}
    /// ```
    fn debug_backtrace(&mut self) -> Result<PhpArray, Thrown> {
        let options = if self.exception_ignore_args() {
            DEBUG_BACKTRACE_IGNORE_ARGS
        } else {
            0
        };
        self.fetch_debug_backtrace(false, options, 0)
    }

    /// `zend_fetch_debug_backtrace` with its three parameters: the link
    /// of the native being served left out where `skip_last` says, as
    /// `debug_backtrace()` leaves itself out; `options`,
    /// `DEBUG_BACKTRACE_PROVIDE_OBJECT` for an `object` row of a
    /// method's receiver between `class` and `type`, where php adds it,
    /// and `DEBUG_BACKTRACE_IGNORE_ARGS` for no `args`; and `limit`
    /// rows at most, none meaning every row.
    pub(crate) fn fetch_debug_backtrace(
        &mut self,
        skip_last: bool,
        options: i64,
        limit: i64,
    ) -> Result<PhpArray, Thrown> {
        let ignore_arguments = options & DEBUG_BACKTRACE_IGNORE_ARGS != 0;
        let provide_object = options & DEBUG_BACKTRACE_PROVIDE_OBJECT != 0;
        let mut links = self.trace_chain();
        if skip_last && !links.is_empty() {
            links.remove(0);
        }
        if let Ok(limit) = usize::try_from(limit)
            && limit > 0
        {
            links.truncate(limit);
        }
        let mut trace = PhpArray::new(self)?;
        for (traced, this, scope, mut arguments, named, site) in links {
            let mut row = PhpArray::new(self)?;
            let is_function = matches!(traced, Traced::Function(_));
            let sensitive = match traced {
                Traced::Function(function) => self.sensitive_parameters(function),
                Traced::Include { .. } => Sensitive::default(),
            };
            let (name, method) = match traced {
                Traced::Function(function) => (
                    self.declared_name(function),
                    self.method_class(function).or(scope).map(|class| {
                        let class = self.class_name(class);
                        (class, if this.is_some() { "->" } else { "::" })
                    }),
                ),
                Traced::Include { kind, file } => {
                    if let Some(file) = file {
                        arguments.push(Value::string(PhpString::new(self, &file)?));
                    }
                    (kind.name().to_owned(), None)
                }
            };
            if let Some((file, line)) = site {
                let file = Value::string(PhpString::new(self, &file)?);
                self.set_string_key(&mut row, b"file", file)?;
                self.set_string_key(&mut row, b"line", Value::int(line))?;
            }
            let name = Value::string(PhpString::new(self, name.as_bytes())?);
            self.set_string_key(&mut row, b"function", name)?;
            if let Some((class, join)) = method {
                let class = Value::string(PhpString::new(self, class.as_bytes())?);
                self.set_string_key(&mut row, b"class", class)?;
                if provide_object && let Some(this) = this {
                    self.addref(&this.as_value());
                    self.set_string_key(&mut row, b"object", this.as_value())?;
                }
                let join = Value::string(PhpString::new(self, join.as_bytes())?);
                self.set_string_key(&mut row, b"type", join)?;
            }
            // A code frame's row has an `args` only where it has the
            // file, and a function's always, `debug_backtrace_get_args`,
            // which adds the extra named arguments under their names
            // after the positional ones.
            let with_args = is_function || !arguments.is_empty();
            if !ignore_arguments && with_args {
                let mut args = PhpArray::new(self)?;
                for (at, argument) in arguments.into_iter().enumerate() {
                    let argument = argument.deref();
                    let argument = if sensitive.at(at) {
                        self.sensitive_value(argument)?
                    } else {
                        self.addref(&argument);
                        argument
                    };
                    args.append(self, argument)?;
                }
                for (name, argument) in named {
                    let argument = argument.deref();
                    let argument = if sensitive.variadic {
                        self.sensitive_value(argument)?
                    } else {
                        self.addref(&argument);
                        argument
                    };
                    self.addref(&Value::string(name));
                    args.set(self, Key::Str(name), argument)?;
                }
                self.set_string_key(&mut row, b"args", Value::array(args))?;
            } else {
                for argument in arguments {
                    self.release(argument);
                }
            }
            trace.append(self, Value::array(row))?;
        }
        Ok(trace)
    }

    /// Which arguments of a call to `function` a trace shows redacted,
    /// php's `zend_get_parameter_attribute_str(..., "sensitiveparameter",
    /// i)` over its parameters: each declared one by position, and the
    /// variadic's for every argument past them and every extra named one.
    fn sensitive_parameters(&self, function: Function) -> Sensitive {
        let marked = |parameter: &php_unit::decl::Parameter| {
            parameter.attributes.iter().any(|attribute| {
                let name = attribute
                    .name
                    .0
                    .strip_prefix(b"\\")
                    .unwrap_or(&attribute.name.0);
                name.eq_ignore_ascii_case(b"SensitiveParameter")
            })
        };
        let Some(decl) = self.function_decl(function) else {
            return Sensitive::default();
        };
        Sensitive {
            declared: decl
                .parameters
                .iter()
                .filter(|parameter| !parameter.variadic)
                .map(marked)
                .collect(),
            variadic: decl
                .parameters
                .iter()
                .find(|parameter| parameter.variadic)
                .is_some_and(marked),
        }
    }

    /// `object_init_ex(zend_ce_sensitive_parameter_value)` and its
    /// constructor over `argument`: what a trace shows in place of an
    /// argument `#[\SensitiveParameter]` marks, one owner, the trace's.
    ///
    /// ```php
    /// function f($a, #[\SensitiveParameter] $secret, ...$rest) {
    ///     debug_print_backtrace();
    ///     var_dump(debug_backtrace()[0]['args'][1]->getValue());
    ///     throw new Exception('boom');
    /// }
    /// function g(#[\SensitiveParameter] ...$all) { var_dump(debug_backtrace()[0]['args']); }
    /// g(1, x: 2);
    /// f(1, 'hunter2', 3);
    /// ```
    /// ```expect exit=255
    /// array(2) {
    ///   [0]=>
    ///   object(SensitiveParameterValue)#1 (0) {
    ///   }
    ///   ["x"]=>
    ///   object(SensitiveParameterValue)#2 (0) {
    ///   }
    /// }
    /// #0 %s(9): f(1, Object(SensitiveParameterValue), 3)
    /// string(7) "hunter2"
    ///
    /// Fatal error: Uncaught Exception: boom in %s:5
    /// Stack trace:
    /// #0 %s(9): f(1, Object(SensitiveParameterValue), 3)
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    fn sensitive_value(&mut self, argument: Value) -> Result<Value, Thrown> {
        let Some(class) = self.class_named(b"SensitiveParameterValue") else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: SensitiveParameterValue is not linked",
            ));
        };
        crate::api::call::instantiate(self, class, &[argument]).map(|object| object.as_value())
    }

    /// php's one `prev_execute_data` chain, innermost first, walked back
    /// out of this engine's two stacks: the user frames, and the calls
    /// being served, each frame's `calls_below` saying where among the
    /// calls it was pushed. A call still being built is not on the
    /// chain, as php's is a chain of activations.
    ///
    /// A fiber's chain goes on into its resumer's, as php's
    /// `stack_bottom->prev_execute_data` names the frame that resumed
    /// it: each stack's links innermost first, the running stack's
    /// before the stacks below it (design §5.11).
    fn chain(&self) -> impl Iterator<Item = Link<'_>> {
        let mut chain: Vec<Link> = Vec::new();
        for (stack, vm) in self.executions() {
            let frames = vm.frames();
            let calls = vm.calls();
            let mut links: Vec<Link> = Vec::new();
            let mut taken = 0usize;
            for (at, state) in frames.iter().enumerate() {
                let below = usize::try_from(state.calls_below)
                    .unwrap_or(usize::MAX)
                    .min(calls.len());
                while taken < below {
                    if calls[taken].stage == exec::CallStage::Serving {
                        links.push(Link::Call(&calls[taken]));
                    }
                    taken += 1;
                }
                links.push(Link::Frame(stack, at, state));
            }
            while taken < calls.len() {
                if calls[taken].stage == exec::CallStage::Serving {
                    links.push(Link::Call(&calls[taken]));
                }
                taken += 1;
            }
            chain.extend(links.into_iter().rev());
        }
        chain.into_iter()
    }

    /// `EG(current_execute_data)` and its `prev_execute_data` chain,
    /// innermost first: one row per link, the function it serves, its
    /// receiver, its arguments, and the site of the link below it. The
    /// main op array's frame is the link that serves no function and
    /// makes no row of its own, since `zend_trace_to_string` closes
    /// every trace with `{main}` whether or not that frame still
    /// stands; every other outermost link is a row with no site, php's
    /// `[internal function]`, which is what a shutdown function or a
    /// destructor served after the main frame popped is.
    ///
    /// php holds one chain; this engine holds two stacks, the user
    /// frames and the calls it is serving, and a frame's `calls_below`
    /// says where among the calls it was pushed, which walks them back
    /// into one. A native that re-entered the loop stands below the
    /// frame it entered, so that frame's site is the native's, which
    /// names no file: `prev->func && ZEND_USER_CODE(...)` is php's
    /// test, and a row with no `file` renders `[internal function]`.
    ///
    /// A user frame's arguments are its parameters' CVs up to the
    /// number passed and then its extra ones
    /// (`debug_backtrace_get_args`), with the extra named ones beside
    /// them; a native's are the call's, as they were placed. A call
    /// still being built is not on the chain, as php's is a chain of
    /// activations.
    fn trace_chain(&self) -> Vec<TraceRow> {
        let chain: Vec<Link> = self.chain().collect();
        let mut rows = Vec::new();
        for at in 0..chain.len() {
            let site = match chain.get(at.saturating_add(1)) {
                Some(Link::Frame(_, _, below)) => exec::body_of(&below.unit, below.body)
                    .map(|code| (code.filename.0.to_vec(), i64::from(exec::line_of(below)))),
                Some(Link::Call(_)) | None => None,
            };
            match &chain[at] {
                Link::Frame(stack, place, state) => {
                    let Some(function) = state.function else {
                        // A code frame: named by the includer's opline
                        // where that is an `INCLUDE_OR_EVAL`, and the
                        // main op array's frame, which no opline pushed,
                        // makes no row (`zend_trace_to_string` closes
                        // every trace with `{main}`).
                        let Some(kind) = chain
                            .get(at.saturating_add(1))
                            .and_then(|below| match below {
                                Link::Frame(_, _, below) => Some(below),
                                Link::Call(_) => None,
                            })
                            .and_then(|below| {
                                let code = exec::body_of(&below.unit, below.body)?;
                                exec::opline_at(code, below.opline)
                            })
                            .filter(|op| op.opcode == Opcode::IncludeOrEval)
                            .and_then(|op| IncludeKind::of(op.extended_value))
                        else {
                            continue;
                        };
                        // `build_filename_arg`: an `eval` names no file.
                        let file = (at > 0 && kind != IncludeKind::Eval)
                            .then(|| exec::body_of(&state.unit, state.body))
                            .flatten()
                            .map(|code| code.filename.0.to_vec());
                        rows.push((
                            Traced::Include { kind, file },
                            None,
                            None,
                            Vec::new(),
                            Vec::new(),
                            site,
                        ));
                        continue;
                    };
                    let frame = FrameRef(u32::try_from(*place).unwrap_or(u32::MAX));
                    let passed = usize::try_from(state.num_args).unwrap_or(0);
                    let mut arguments: Vec<Value> = (0..passed
                        .saturating_sub(state.extra_args.len()))
                        .map(|slot| {
                            let slot = Slot(u32::try_from(slot).unwrap_or(u32::MAX));
                            stack
                                .frame(frame)
                                .map_or(Value::undef(), |f| Value::of_zval(f.slot(slot)))
                        })
                        .collect();
                    arguments.extend(state.extra_args.iter().copied());
                    rows.push((
                        Traced::Function(function),
                        state.this,
                        self.closure_scope(state.closure),
                        arguments,
                        state.extra_named.clone(),
                        site,
                    ));
                }
                Link::Call(call) => {
                    rows.push((
                        Traced::Function(call.function),
                        call.this,
                        self.closure_scope(call.closure),
                        call.arguments.clone(),
                        call.extra_named.clone(),
                        site,
                    ));
                }
            }
        }
        rows
    }

    /// `ZEND_MAP_PTR_GET(closure->func.op_array.static_variables_ptr)`
    /// beside the names the op array carries: what the closure bound,
    /// name and value, in the order the table took them, with a slot
    /// nothing filled left out, and with `lexical` the `use` variables
    /// alone. A closure made of a named function shares that function's
    /// table (`zend_create_closure_ex` under `is_fake`), which is read
    /// where the request has laid it down and at its starting values
    /// where it has not. Empty for an object that is no closure and for
    /// a closure whose op array binds nothing.
    pub(crate) fn closure_bound(&self, object: PhpObject, lexical: bool) -> Vec<(Vec<u8>, Value)> {
        let Some(closure) = object.closure(self) else {
            return Vec::new();
        };
        let Some(user) = self.user_function(closure.function) else {
            return Vec::new();
        };
        let Some(code) = exec::body_of(&user.unit, user.body) else {
            return Vec::new();
        };
        let starting: Vec<Value>;
        let values = if closure.fake {
            if let Some(held) = self.static_vars.get(&Statics::Function(closure.function)) {
                held.as_slice()
            } else {
                starting = exec::starting_statics(code);
                starting.as_slice()
            }
        } else {
            closure.bound.as_slice()
        };
        code.bound_vars
            .iter()
            .zip(values.iter())
            .filter(|(var, value)| value.tag() != TypeTag::Undef && (var.lexical || !lexical))
            .map(|(var, value)| (var.name.0.to_vec(), *value))
            .collect()
    }

    /// The `static` variables of what `frame` runs, php's
    /// `ZEND_MAP_PTR_GET(EX(func)->op_array.static_variables_ptr)`: a
    /// closure's own table, which its object holds; a function's or a
    /// method's, which the request holds per linked function, so a
    /// method a trait gave two classes counts apart in each and an
    /// inherited one counts with its parent's; or, for the main op array
    /// of a file, the request's table for that op array.
    pub(crate) fn statics_of(&self, frame: FrameRef, code: &OpArray) -> StaticsAt {
        let Some(state) = self.vm.at(frame) else {
            return StaticsAt::Table(Statics::Main(code.runtime_cache));
        };
        if let Some(object) = state.closure
            && object.closure(self).is_some_and(|closure| !closure.fake)
        {
            return StaticsAt::Closure(object);
        }
        match state.function {
            Some(function) => StaticsAt::Table(Statics::Function(function)),
            None => StaticsAt::Table(Statics::Main(code.runtime_cache)),
        }
    }

    /// The value at `slot` of a `static` table, the table laid down from
    /// `code`'s starting values where the request has not touched it
    /// yet, which is `zend_array_dup(op_array->static_variables)` at the
    /// first `BIND_STATIC`. None for a slot the table has not.
    pub(crate) fn static_at(
        &mut self,
        at: StaticsAt,
        code: &OpArray,
        slot: usize,
    ) -> Option<Value> {
        match at {
            StaticsAt::Closure(object) => object.closure(self)?.bound.get(slot).copied(),
            StaticsAt::Table(of) => self
                .static_vars
                .entry(of)
                .or_insert_with(|| exec::starting_statics(code))
                .get(slot)
                .copied(),
        }
    }

    /// `value` into `slot` of a `static` table, which takes the owner
    /// the caller held; what stood there is handed back, since the table
    /// only ever trades a value for the reference made over it.
    pub(crate) fn set_static(&mut self, at: StaticsAt, slot: usize, value: Value) -> Value {
        let cell = match at {
            StaticsAt::Closure(object) => object
                .native_mut::<ClosureInfo>(self)
                .and_then(|payload| payload.bound.get_mut(slot)),
            StaticsAt::Table(of) => self
                .static_vars
                .get_mut(&of)
                .and_then(|table| table.get_mut(slot)),
        };
        match cell {
            Some(cell) => std::mem::replace(cell, value),
            None => value,
        }
    }

    /// The `static` variables `callable` reads, name and value, as
    /// `ReflectionFunctionAbstract::getStaticVariables` copies them: a
    /// closure's own table, and a function's or a method's laid down
    /// from its starting values where no call has yet, as php lays it
    /// down there. Empty for a native and for a function with none.
    pub(crate) fn static_variables(&mut self, target: Target) -> Vec<(Vec<u8>, Value)> {
        let function = match target {
            Target::Closure(object) => {
                let fake = object.closure(self).is_some_and(|closure| closure.fake);
                match object.closure(self).map(|closure| closure.function) {
                    Some(function) if fake => function,
                    _ => return self.closure_bound(object, false),
                }
            }
            Target::Function(function) | Target::Method { function, .. } => function,
        };
        let Some((unit, body)) = self
            .user_function(function)
            .map(|user| (Arc::clone(&user.unit), user.body))
        else {
            return Vec::new();
        };
        let Some(code) = exec::body_of(&unit, body) else {
            return Vec::new();
        };
        if code.bound_vars.is_empty() {
            return Vec::new();
        }
        let values = self
            .static_vars
            .entry(Statics::Function(function))
            .or_insert_with(|| exec::starting_statics(code));
        code.bound_vars
            .iter()
            .zip(values.iter())
            .map(|(var, value)| (var.name.0.to_vec(), *value))
            .collect()
    }

    /// The `static` variables of the main op array whose map pointer is
    /// `offset` given up: php compiles a file afresh at each include, so
    /// its main op array's table starts over each time it is entered.
    fn forget_main_statics(&mut self, offset: u32) {
        if let Some(values) = self.static_vars.remove(&Statics::Main(offset)) {
            for value in values {
                self.release(value);
            }
        }
    }

    /// The request's `static` tables given up, php's walk over
    /// `EG(function_table)` in `zend_shutdown_executor_values`
    /// (`Z/Zend/zend_execute_API.c:313`), after the symbol table: the
    /// destructors are behind the request by then, so this frees and
    /// serves nothing.
    pub(crate) fn destroy_static_vars(&mut self) {
        for (_, values) in std::mem::take(&mut self.static_vars) {
            for value in values {
                self.release(value);
            }
        }
    }

    /// `func->common.scope` of a closure's own copy of its function: the
    /// class the closure was written in, which a backtrace names before
    /// the closure's own name. None where the call stands on no closure
    /// or the closure was made outside a class.
    fn closure_scope(&self, closure: Option<PhpObject>) -> Option<ClassId> {
        closure?.closure(self)?.scope
    }

    /// `zend_hash_str_update` on a row: `value` at the string key `name`,
    /// the key a string of the request's that the array takes.
    fn set_string_key(
        &mut self,
        row: &mut PhpArray,
        name: &[u8],
        value: Value,
    ) -> Result<(), Thrown> {
        let key = Key::Str(PhpString::new(self, name)?);
        row.set(self, key, value)
    }

    /// `zend_trace_to_string` (`Z/Zend/zend_exceptions.c:668`):
    /// `_build_trace_string` over each row that is an array, `#N
    /// file(line): ` where the row names a `file`, `[internal
    /// function]: ` where it names none, the `class` and `type` where
    /// there are ones, the `function`, and the `args` between the
    /// parentheses; then, under `include_main`, `#N {main}`, and without
    /// it every line closed by its newline, which is what
    /// `debug_print_backtrace()` writes.
    pub(crate) fn trace_to_string(&self, trace: PhpArray, include_main: bool) -> String {
        let mut frames: Vec<String> = Vec::new();
        for (_, row) in &trace {
            let Some(row) = row.as_array() else {
                continue;
            };
            let field = |name: &[u8]| {
                row.into_iter()
                    .find(|(key, _)| matches!(key, Key::Str(s) if s.as_bytes() == name))
                    .map(|(_, value)| value)
            };
            let string = |name: &[u8]| {
                field(name).and_then(|v| {
                    v.as_str_bytes()
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                })
            };
            let site = match string(b"file") {
                Some(file) => {
                    let line = field(b"line").and_then(|v| v.as_int()).unwrap_or(0);
                    format!("{file}({line}): ")
                }
                None => String::from("[internal function]: "),
            };
            let class = string(b"class").unwrap_or_default();
            let join = string(b"type").unwrap_or_default();
            let function = string(b"function").unwrap_or_else(|| String::from("unknown"));
            let arguments = field(b"args")
                .and_then(|v| v.as_array().copied())
                .map(|args| self.trace_arguments(&args.snapshot_refs()))
                .unwrap_or_default();
            frames.push(format!(
                "#{} {site}{class}{join}{function}({arguments})",
                frames.len()
            ));
        }
        if !include_main {
            let mut out = frames.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            return out;
        }
        let main = frames.len();
        frames.push(format!("#{main} {{main}}"));
        frames.join("\n")
    }

    /// The backtrace a fatal carries, `zend_fetch_debug_backtrace`
    /// rendered by `zend_trace_to_string` at the site, its rows given
    /// back once rendered; the memory limit refusing the rows leaves the
    /// fatal without one.
    fn trace_as_string(&mut self) -> String {
        let Ok(trace) = self.debug_backtrace() else {
            return String::from("#0 {main}");
        };
        let text = self.trace_to_string(trace, true);
        self.release(Value::array(trace));
        text
    }

    /// `_build_trace_args` (`Z/Zend/zend_exceptions.c:539`) over one
    /// frame's arguments, `, ` between them and `name: ` before one the
    /// row keeps under a string key, which is an extra named argument
    /// (`_build_trace_string`'s walk of `args`): `smart_str_append_zval`
    /// for a scalar and an enum case, and php's three words for the rest.
    /// `NULL`, `true`, `false` and a long's digits; a double through
    /// `smart_str_append_double` at `precision` with `zero_fraction` set,
    /// which is `zend_gcvt` and then `.0` on a finite text with no `.` in
    /// it, so `1.0`, `-0.0` and `1.0E+20` where `echo` writes `1`, `-0`
    /// and `1.0E+20`; a string in `'`, escaped by `smart_str_append_escaped`
    /// and cut at `zend.exception_string_param_max_len` bytes with `...`
    /// after the cut; `Array`, `Object(ClassName)`, `Resource id #N`, and
    /// an enum case as `Enum::Case`. A reference is followed first,
    /// `ZVAL_DEREF`.
    ///
    /// ```php
    /// memory_get_usage(null, true, false, -7, 1.0, -0.0, 1e20, 0.1, INF, NAN, "a\nb\\c\te\$\e", "exactly fifteen", "sixteen chars!!!", "é");
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: memory_get_usage() expects at most 1 argument, 14 given in %s:2
    /// Stack trace:
    /// #0 %s(2): memory_get_usage(NULL, true, false, -7, 1.0, -0.0, 1.0E+20, 0.1, INF, NAN, 'a\nb\\c\te$\e', 'exactly fifteen', 'sixteen chars!!...', '\xC3\xA9')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// `zend.exception_ignore_args` leaves the frame without its
    /// arguments, and `zend.exception_string_param_max_len` moves the cut.
    ///
    /// ```php
    /// ini_set('zend.exception_string_param_max_len', '3'); ini_set('zend.exception_ignore_args', '0');
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit('sec...', 2)
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', '1');
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit()
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    fn trace_arguments(&self, arguments: &[(Key, Value)]) -> String {
        let mut out = String::new();
        for (i, (key, argument)) in arguments.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            if let Key::Str(name) = key {
                out.push_str(&String::from_utf8_lossy(name.as_bytes()));
                out.push_str(": ");
            }
            self.trace_argument(&mut out, &argument.deref());
        }
        out
    }

    /// One argument as [`Request::trace_arguments`] renders it.
    fn trace_argument(&self, out: &mut String, argument: &Value) {
        match argument.as_ref() {
            ValueRef::Undef | ValueRef::Null => out.push_str("NULL"),
            ValueRef::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            ValueRef::Int(n) => out.push_str(&n.to_string()),
            ValueRef::Float(f) => {
                // `smart_str_append_double`: `precision ? precision : 1`,
                // then `.0` on a finite text that carries no `.`.
                let precision = match self.precision() {
                    0 => 1,
                    precision => precision,
                };
                let text = php_heap::double_to_string(f, precision, php_heap::Exponent::Upper);
                out.push_str(&text);
                if f.is_finite() && !text.contains('.') {
                    out.push_str(".0");
                }
            }
            ValueRef::Str(s) => {
                out.push('\'');
                let cut = self.exception_string_param_max_len();
                append_escaped(out, &s.as_bytes()[..cut.min(s.len())]);
                if s.len() > cut {
                    out.push_str("...");
                }
                out.push('\'');
            }
            ValueRef::Array(_) => out.push_str("Array"),
            ValueRef::Object(object) => {
                let decl = self.class_decl(object.class());
                // `smart_str_appends(str, ZSTR_VAL(class_name))`: up to
                // the NUL of an anonymous class's name.
                let class = decl.map_or_else(String::new, |decl| {
                    php_unit::decl::shown(&decl.name.0).into_owned()
                });
                let case = decl
                    .filter(|decl| decl.kind == ClassKind::Enum)
                    .and_then(|_| object.prop(self, b"name"))
                    .and_then(|name| name.as_php_string().copied());
                // `zend_enum_fetch_case_name`: the case's `name`.
                if let Some(case) = case {
                    out.push_str(&class);
                    out.push_str("::");
                    out.push_str(&String::from_utf8_lossy(case.as_bytes()));
                } else {
                    out.push_str("Object(");
                    out.push_str(&class);
                    out.push(')');
                }
            }
            ValueRef::Resource(resource) => {
                out.push_str("Resource id #");
                out.push_str(&resource.id().to_string());
            }
            // `deref` followed the reference already.
            ValueRef::Reference(r) => self.trace_argument(out, &r.get()),
        }
    }

    /// `zend.exception_ignore_args`: whether a throwable's trace is
    /// built without the frames' arguments.
    fn exception_ignore_args(&self) -> bool {
        self.ini_get(b"zend.exception_ignore_args")
            .is_some_and(Request::ini_parse_bool)
    }

    /// `zend.exception_string_param_max_len`: the bytes of a string
    /// argument a trace prints before `...`, `EG(exception_string_param_max_len)`,
    /// which `OnSetExceptionStringParamMaxLen` set from the text with
    /// `ZEND_ATOL` (`Z/Zend/zend.c:177`), so `' +4x'` cuts at four.
    ///
    /// ```php
    /// var_dump(ini_set('zend.exception_string_param_max_len', ' +4x'));
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// string(2) "15"
    ///
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit('seco...', 2)
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    pub(crate) fn exception_string_param_max_len(&self) -> usize {
        self.ini_get(b"zend.exception_string_param_max_len")
            .map_or(15, |value| usize::try_from(ini_atol(value)).unwrap_or(0))
    }

    /// Serve the native the call frame on top of [`Vm`] names: its entry
    /// with the request, off which it reads its arguments (ADR-0002); the
    /// frame is the active function for the raisers' text while the entry
    /// runs, and stays on the stack until `DO_ICALL` pops it. `function`
    /// is the callee the `INIT_FCALL` resolved; one with no entry is the
    /// `Error` a function nothing is linked under raises, in the caller's
    /// spelling.
    pub(crate) fn serve_native(&mut self, function: Function) -> Result<Value, Thrown> {
        if function == Function::PASS {
            return Ok(Value::null());
        }
        let Some(entry) = self.entry_of(function) else {
            let name = self
                .vm
                .serving_call()
                .and_then(|call| call.name)
                .map(|name| String::from_utf8_lossy(name.as_bytes()).into_owned())
                .unwrap_or_default();
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Call to undefined function {name}()"),
            ));
        };
        let served = entry(self);
        match (served, self.stream_thrown.take()) {
            (Ok(value), Some(thrown)) => {
                self.release(value);
                Err(thrown)
            }
            (Err(thrown), Some(deferred)) => {
                if let Some(object) = deferred.exception() {
                    self.release(object.as_value());
                }
                Err(thrown)
            }
            (served, None) => served,
        }
    }

    /// The host's sink, held from here to the request's end, so each
    /// call reaches the host as the request makes it.
    pub(crate) fn hold(&mut self, held: Box<dyn OutputSink + Send>) {
        self.sink = Sink::Held(held);
    }

    /// Record the natives this request enters from here on, for the
    /// witness runner (harness §4.5 condition 2, revision 58).
    #[cfg(feature = "witness-trace")]
    pub(crate) fn trace_natives(&mut self) {
        self.trace = Some(Vec::new());
    }

    /// One native entered, where the request records them; what the
    /// entry a macro expansion builds calls on its way in.
    #[cfg(feature = "witness-trace")]
    pub(crate) fn entered(&mut self, native: crate::test::Entered) {
        if let Some(trace) = &mut self.trace {
            trace.push(native);
        }
    }

    /// The natives recorded, given up at the request's end; none where
    /// nothing asked for them.
    #[cfg(feature = "witness-trace")]
    pub(crate) fn take_trace(&mut self) -> Vec<crate::test::Entered> {
        self.trace.take().unwrap_or_default()
    }

    /// The sink the request wrote to, given back at its end: the held
    /// one to flush, or the calls kept for the host's.
    pub(crate) fn release_sink(&mut self) -> Sink {
        std::mem::replace(&mut self.sink, Sink::Kept(Kept::default()))
    }

    /// The body the request's own sink kept, for a test that serves no
    /// host.
    #[cfg(test)]
    pub(crate) fn take_body(&mut self) -> Vec<u8> {
        match &mut self.sink {
            Sink::Kept(kept) => std::mem::take(&mut kept.body),
            Sink::Held(_) => Vec::new(),
        }
    }

    /// The log's entries and the error stream's bytes the request's own
    /// sink kept, in the order written, for a test that serves no host.
    #[cfg(test)]
    pub(crate) fn take_aside(&mut self) -> Vec<Aside> {
        match &mut self.sink {
            Sink::Kept(kept) => std::mem::take(&mut kept.aside),
            Sink::Held(_) => Vec::new(),
        }
    }

    /// `php_log_err`: one entry for the host's log, whole.
    fn log_err(&mut self, entry: &[u8]) {
        self.sink.get().log(entry);
    }

    /// `sapi_module.send_headers`: the header block handed to the sink,
    /// once, at the commit `sapi_send_headers` makes, or when the request
    /// ends where nothing committed it.
    pub(crate) fn send_header_block(&mut self) {
        let block = self.header_block();
        self.sink.get().send_headers(&block);
    }

    /// The header block as the sink is handed it: `SG(sapi_headers).headers`
    /// rendered one line each, and nothing else, since the status line
    /// and its shape are the host's (`sapi_cgi_send_headers` writes
    /// `Status:`, the dev server the HTTP status line).
    pub(crate) fn header_block(&self) -> Vec<u8> {
        let mut block = Vec::new();
        for header in self.output.headers() {
            block.extend_from_slice(header);
            block.extend_from_slice(b"\r\n");
        }
        block
    }

    pub(crate) fn ending(&self) -> Option<&Ending> {
        self.ending.as_ref()
    }

    /// `php_error_cb`: one diagnostic, rendered twice. A repeat
    /// `ignore_repeated_errors` drops is dropped whole; what
    /// `error_reporting` masks is neither logged nor displayed; what is
    /// left goes to the log while `log_errors` is on, in php's log form,
    /// and to the body while `display_errors` is, in whichever of php's
    /// two display forms `html_errors` names, with
    /// `error_prepend_string` and `error_append_string` standing around
    /// it. The user handler takes it first where one stands
    /// ([`Request::note_error_in`]); the message-id table and `EH_THROW`
    /// are the rest of that branch and are not read yet. `true` says the
    /// built-in handler served it.
    pub(crate) fn note_error(&mut self, level: Level, message: &str) -> bool {
        // `zend_error_va_list`'s site (`Z/Zend/zend.c:1633-1651`): the
        // executing frame's file and line, and, where nothing is
        // executing and nothing compiling, no file at all, which
        // `php_error_cb` is handed as `Unknown` at line zero
        // (`ZEND_STR_UNKNOWN_CAPITALIZED`). A destructor the shutdown
        // walk refuses to serve raises there.
        if !self.executing() {
            return self.note_error_in(level, message, b"Unknown", 0);
        }
        let line = self.current_line();
        self.note_error_at(level, message, line)
    }

    /// The same diagnostic at a line no frame stands on: what a
    /// compile-time error carries, since php's `zend_error_cb` is given
    /// the compiler's line while `zend_get_executed_lineno` has none.
    pub(crate) fn note_error_at(&mut self, level: Level, message: &str, line: u32) -> bool {
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        self.note_error_in(level, message, &file, line)
    }

    /// The same diagnostic at a file and line of its own, which is what
    /// `zend_exception_error` gives `zend_error_va` for an uncaught
    /// throwable: the site the throwable recorded, not the site the
    /// request has reached.
    ///
    /// The recipe passes both strings empty, `html_errors=0` and
    /// `ignore_repeated_errors=0`, so each witness here sets its entry
    /// with `ini_set()` first. `error_prepend_string` and
    /// `error_append_string` stand around the plain form, with no
    /// newline of their own: the form's own leading newline is what
    /// parts `<PRE>` from `Warning`.
    ///
    /// ```php
    /// ini_set('error_prepend_string', '<PRE>');
    /// ini_set('error_append_string', '</PRE>');
    /// echo NAN;
    /// ```
    /// ```expect
    /// <PRE>
    /// Warning: unexpected NAN value was coerced to string in %s on line 4
    /// </PRE>NAN
    /// ```
    ///
    /// `html_errors` is the other form, `<br />\n<b>Warning</b>:  …`,
    /// with the file and the line in `<b>` and two spaces after the
    /// colon. The message of an `E_ERROR` or an `E_PARSE` is escaped
    /// under it and a warning's stands as it is.
    ///
    /// ```php
    /// ini_set('html_errors', '1');
    /// echo NAN;
    /// ```
    /// ```expect
    /// <br />
    /// <b>Warning</b>:  unexpected NAN value was coerced to string in <b>%s</b> on line <b>3</b><br />
    /// NAN
    /// ```
    ///
    /// The `E_ERROR` an uncaught throwable becomes carries its trace in
    /// its message, and a string argument in the trace is where the
    /// four bytes `escape_html` moves reach a fatal: the argument is
    /// escaped, and the `<b>` around the file and the line are written
    /// after the message and stand.
    ///
    /// ```php
    /// ini_set('html_errors', '1');
    /// ini_set('<&">');
    /// ```
    /// ```expect exit=255
    /// <br />
    /// <b>Fatal error</b>:  Uncaught ArgumentCountError: ini_set() expects exactly 2 arguments, 1 given in %s:3
    /// Stack trace:
    /// #0 %s(3): ini_set('&lt;&amp;&quot;&gt;')
    /// #1 {main}
    ///   thrown in <b>%s</b> on line <b>3</b><br />
    /// ```
    ///
    /// `ignore_repeated_errors` drops a repeat of the last diagnostic
    /// stored, and while `ignore_repeated_source` is off a repeat is one
    /// at the same file and line: the second `NAN` on line 3 is dropped,
    /// the one on line 4 stands. With `ignore_repeated_source` on the
    /// site no longer parts them, and the one on line 6 is dropped too.
    ///
    /// ```php
    /// ini_set('ignore_repeated_errors', '1');
    /// echo NAN, NAN;
    /// echo NAN;
    /// ini_set('ignore_repeated_source', '1');
    /// echo NAN;
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    /// NANNAN
    /// Warning: unexpected NAN value was coerced to string in %s on line 4
    /// NANNAN
    /// ```
    ///
    /// The rest of php's display branch (`Z/main/main.c:1409-1489`),
    /// condition by condition. `E_CORE` stands past `error_reporting`,
    /// `(EG(error_reporting) & type) || (type & E_CORE)`, and the mask
    /// here reads it; nothing in this engine raises `E_CORE_ERROR` or
    /// `E_CORE_WARNING` yet, since php's own raisers of the two are its
    /// own startup and the ini loader, so the mask is held by a test
    /// on this function and no witness reaches it. `xmlrpc_errors` is
    /// the form ahead of the two `html_errors` names, read below;
    /// `PHP_INI_SYSTEM`, so a host's startup ini sets it and no
    /// `ini_set()` does, and it is held by an engine test rather than a
    /// witness, whose recipe passes the entry nothing. The stderr form
    /// is the CLI's, the CGI's and phpdbg's, `sapi_module.name` read at
    /// `Z/main/main.c:1484`; under another host `display_errors=stderr`
    /// falls to the plain form on the body, which [`Request::display_errors`]
    /// answers off the host's name. `display_startup_errors` and
    /// `during_request_startup` are not read: php's display and log
    /// branches both ask `module_initialized && !during_request_startup`
    /// before them, and a diagnostic of php's startup window is one
    /// `php_request_startup` raises between `sapi_activate` and the
    /// file's compile, in the GPC seeding or a per-directory ini. This
    /// engine raises nothing before the file's first opline, so the two
    /// entries decide nothing a request can see, and the recipe's
    /// `display_startup_errors=1` shows no difference from php's default
    /// of the same.
    ///
    /// A fatal's own trace is php 8.5's `fatal_error_backtraces`
    /// (`Z/Zend/zend.c:263`, `On` by default), the half of
    /// `zend_error_impl` before `zend_error_cb` (`Z/Zend/zend.c:1500-1526`):
    /// `EG(last_fatal_error_backtrace)` is cleared for every diagnostic,
    /// and while the entry is on a level in `E_FATAL_ERRORS` fetches one
    /// where the request stands, `zend_fetch_debug_backtrace` under
    /// `zend.exception_ignore_args`, unless a throwable is pending, which
    /// php reports ahead of the fatal instead. `php_error_cb` renders it
    /// under `Stack trace:` after the site in the log form, the xml
    /// fault, the stderr form and the plain form alike. The recipe
    /// carries `run-tests.php`'s `fatal_error_backtraces=Off`, so a
    /// witness turns the entry on first; the frames are the ones a
    /// throwable's trace walks, so a fatal at a back-edge of the main op
    /// array stands over `{main}` alone.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
    /// Stack trace:
    /// #0 {main}
    /// ```
    ///
    /// A fatal raised inside a native stands over the native's frame, as
    /// a throwable's does, since both are [`Request::trace_as_string`],
    /// whose witnesses pin the frame and its arguments; the one fatal a
    /// native of this engine raises while serving, `var_dump()` over a
    /// `__debugInfo` that answers no array, waits on a compiler that
    /// reads a method, and the oracle prints
    /// `#0 %s(2): var_dump(Object(A))` over `#1 {main}` for it.
    ///
    /// The stderr form carries the trace as the plain form does, on the
    /// error stream with it.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); ini_set('display_errors', 'stderr'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// ```
    /// ```stderr
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
    /// Stack trace:
    /// #0 {main}
    /// ```
    ///
    /// The trace is what parts php's two html formats
    /// (`Z/main/main.c:1471-1476`): an `E_ERROR` or an `E_PARSE` takes the
    /// first, its message escaped and no trace however the entry stands,
    /// and every other level the second, unescaped, with the trace
    /// between `<br />` and the newline that closes the line. A
    /// `TypeError` left uncaught is the `E_ERROR` an uncaught throwable
    /// becomes, and `zend_exception_error` fetches no trace for it
    /// ([`Request::end_in_uncaught`]), so under html an `E_ERROR` with a
    /// trace is the time limit's or the memory limit's, and it prints
    /// none.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); ini_set('html_errors', '1'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// <br />
    /// <b>Fatal error</b>:  Maximum execution time of 1 second exceeded in <b>%s</b> on line <b>2</b><br />
    /// ```
    ///
    /// The second html format is `E_USER_ERROR`'s, `E_COMPILE_ERROR`'s,
    /// `E_CORE_ERROR`'s and `E_RECOVERABLE_ERROR`'s; no native here
    /// raises one while serving, so
    /// `the_second_html_format_carries_the_trace_after_the_line` below
    /// holds it to the oracle's bytes for `trigger_error()`.
    pub(crate) fn note_error_in(
        &mut self,
        level: Level,
        message: &str,
        file: &[u8],
        line: u32,
    ) -> bool {
        if let Some(held) = self.startup.as_mut() {
            held.push((level, message.to_owned()));
            return false;
        }
        // `php_error_cb` under `EH_THROW` (`Z/main/main.c:1368`), which
        // `zend_error_impl` reaches past the user handler: a warning is
        // thrown as the class `zend_replace_error_handling` named, where
        // nothing is pending, and shown nowhere.
        if let Some(class) = self.error_handling.clone()
            && matches!(
                level,
                Level::Warning | Level::CoreWarning | Level::CompileWarning | Level::UserWarning
            )
        {
            if self.pending.is_none() && self.error_handling_thrown.is_none() {
                let thrown = crate::api::raising::throw(self, &class, message);
                self.error_handling_thrown = Some(thrown);
            }
            return false;
        }
        // "Always clear the last backtrace."
        self.last_fatal_error_backtrace = None;
        if self.pending.is_none() && self.fatal_error_backtraces() && fatal_errors(level) {
            self.last_fatal_error_backtrace = Some(self.trace_as_string());
        }
        if self.user_error_handler(level, message, file, line) {
            return false;
        }
        self.error_cb(level, message, file, line);
        true
    }

    /// `php_error_cb` (`Z/main/main.c:1370-1500`): the diagnostic stored,
    /// masked, logged and displayed, with the trace
    /// [`Request::note_error_in`] fetched, if one, under `Stack trace:`.
    fn error_cb(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
        if !self.displayed(message, file, line) {
            return;
        }
        let named = String::from_utf8_lossy(file).into_owned();
        let file = file.to_vec();
        // php stores the diagnostic before it masks it, which is why
        // `error_get_last()` answers one `error_reporting` hid.
        self.last_error = Some((level, message.to_owned(), file, line));
        // `(EG(error_reporting) & type) || (type & E_CORE)`: a core
        // level is rendered whatever `error_reporting` says.
        let core = matches!(level, Level::CoreError | Level::CoreWarning);
        if self.error_reporting() & level.bit() == 0 && !core {
            return;
        }
        let name = level_name(level);
        let trace = self.stack_trace_after_the_site();
        if self.log_errors() {
            let text = format!("PHP {name}:  {message} in {named} on line {line}{trace}");
            self.log_err(text.as_bytes());
        }
        let display = self.display_errors();
        if display == Display::Off {
            return;
        }
        if self.xmlrpc_errors() {
            // php's first display form, one `methodResponse` fault with
            // `xmlrpc_error_number` as its code and the diagnostic as
            // its string, the message unescaped and no space after the
            // colon; neither string stands around it.
            let text = format!(
                "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>{}</int></value></member><member><name>faultString</name><value><string>{name}:{message} in {named} on line {line}{trace}</string></value></member></struct></value></fault></methodResponse>",
                self.xmlrpc_error_number()
            );
            self.display(text.as_bytes());
        } else if self.html_errors() {
            // php's two html formats: the message of an `E_ERROR` or an
            // `E_PARSE` escaped and no trace after it, and every other
            // level's as it is with the trace between the `<br />` and
            // the newline.
            let prepend = self.ini_text(b"error_prepend_string");
            let append = self.ini_text(b"error_append_string");
            let text = match level {
                Level::Error | Level::Parse => format!(
                    "{prepend}<br />\n<b>{name}</b>:  {} in <b>{named}</b> on line <b>{line}</b><br />\n{append}",
                    escape_html(message)
                ),
                _ => format!(
                    "{prepend}<br />\n<b>{name}</b>:  {message} in <b>{named}</b> on line <b>{line}</b><br />{trace}\n{append}"
                ),
            };
            self.display(text.as_bytes());
        } else if display == Display::Stderr {
            // php's stderr form stands inside the same `else` as the
            // plain one, so `html_errors` takes a CLI diagnostic off the
            // error stream and onto the body; and it carries neither
            // string around it.
            let text = format!("{name}: {message} in {named} on line {line}{trace}\n");
            self.write_stderr(text.as_bytes());
        } else {
            let text = format!(
                "{}\n{name}: {message} in {named} on line {line}{trace}\n{}",
                self.ini_text(b"error_prepend_string"),
                self.ini_text(b"error_append_string"),
            );
            self.display(text.as_bytes());
        }
    }

    /// A diagnostic's display on the body: through the `php_output`
    /// layer, or past it where it is raised as the engine starts.
    fn display(&mut self, text: &[u8]) {
        if self.display_direct {
            self.ub_write(text);
        } else {
            self.write(text);
        }
    }

    /// A diagnostic raised as the engine starts, at `Unknown` on line
    /// zero and displayed past the `php_output` layer
    /// ([`crate::api::raising::startup`]).
    pub(crate) fn note_startup_error(&mut self, level: Level, message: &str) {
        self.display_direct = true;
        self.note_error_in(level, message, b"Unknown", 0);
        self.display_direct = false;
    }

    /// What `php_error_cb` writes after `on line N` for the fatal's trace:
    /// `ZSTR_LEN(backtrace) ? "\nStack trace:\n" : ""` and the trace, so
    /// nothing at all while none was fetched.
    fn stack_trace_after_the_site(&self) -> String {
        self.last_fatal_error_backtrace
            .as_deref()
            .map_or_else(String::new, |trace| format!("\nStack trace:\n{trace}"))
    }

    /// `fatal_error_backtraces`, `EG(fatal_error_backtrace_on)` through
    /// `OnUpdateBool` (`Z/Zend/zend.c:263`), `PHP_INI_ALL`: whether a
    /// fatal fetches its trace. The engine's default is php's, `1`, which
    /// `fatal_error_backtraces_is_on_by_default` in `engine` holds; the
    /// recipe's `fatal_error_backtraces=Off` reaches the table through the
    /// ini scanner as the empty string, which is what php answers under
    /// the same `-d`, and `ini_set()` moves it.
    ///
    /// ```php
    /// var_dump(ini_get('fatal_error_backtraces'), ini_set('fatal_error_backtraces', '1'), ini_get('fatal_error_backtraces'));
    /// ```
    /// ```expect
    /// string(0) ""
    /// string(0) ""
    /// string(1) "1"
    /// ```
    fn fatal_error_backtraces(&self) -> bool {
        self.ini_get(b"fatal_error_backtraces")
            .is_some_and(Request::ini_parse_bool)
    }

    /// php's `display`, the first branch of `php_error_cb`: a diagnostic
    /// stands unless `ignore_repeated_errors` is on and it repeats the
    /// last one stored. It repeats when the message is the same and
    /// either `ignore_repeated_source` is on or the site is the same
    /// too. A repeat is dropped whole — not stored, not logged, not
    /// displayed — so the one it repeats stays the last error.
    fn displayed(&self, message: &str, file: &[u8], line: u32) -> bool {
        if !ini_truth(self.ini_get(b"ignore_repeated_errors")) {
            return true;
        }
        let Some((_, last, last_file, last_line)) = self.last_error.as_ref() else {
            return true;
        };
        if last != message {
            return true;
        }
        if ini_truth(self.ini_get(b"ignore_repeated_source")) {
            return false;
        }
        *last_line != line || last_file.as_slice() != file
    }

    /// `log_errors`, whether a diagnostic reaches the host's log.
    fn log_errors(&self) -> bool {
        ini_truth(self.ini_get(b"log_errors"))
    }

    /// `html_errors`, which of php's two display forms a diagnostic
    /// takes: the plain one, or `<br />\n<b>Warning</b>:  …`.
    fn html_errors(&self) -> bool {
        ini_truth(self.ini_get(b"html_errors"))
    }

    /// `xmlrpc_errors`, whether a diagnostic takes php's xml fault form
    /// ahead of both of the others.
    fn xmlrpc_errors(&self) -> bool {
        ini_truth(self.ini_get(b"xmlrpc_errors"))
    }

    /// `xmlrpc_error_number`, the fault's code, `OnUpdateLong`.
    fn xmlrpc_error_number(&self) -> i64 {
        self.ini_get(b"xmlrpc_error_number")
            .and_then(|value| String::from_utf8_lossy(value).parse().ok())
            .unwrap_or(0)
    }

    /// One ini entry as php's `STR_PRINT(INI_STR(name))` reads it: its
    /// bytes, and nothing at all where php left the entry `NULL`.
    fn ini_text(&self, name: &[u8]) -> String {
        self.ini_get(name)
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .unwrap_or_default()
    }

    /// `display_errors`, php's three values: off, the body, and the
    /// host's error stream, which the hosts php names at
    /// `Z/main/main.c:1484`, `cli`, `cgi` and `phpdbg`, write a
    /// diagnostic to without php's leading newline. Under any other
    /// host `stderr` is the body, since php's test is on
    /// `sapi_module.name` and the plain form is its `else`.
    ///
    /// The value is read as `php_get_display_errors_mode`
    /// (`Z/main/main.c:492-517`) reads it: no entry, `on`, `yes`,
    /// `true` and `stdout` are `PHP_DISPLAY_ERRORS_STDOUT`, `stderr` is
    /// `PHP_DISPLAY_ERRORS_STDERR`, and any other value is `ZEND_ATOL`
    /// of it narrowed to php's `uint8_t`, where `2` is the error stream,
    /// `0` is off and every other number is `STDOUT`. The narrowing is
    /// php's and observable, `256` is off and `258` the error stream,
    /// so it is copied.
    fn display_errors(&self) -> Display {
        let mode = match self.ini_get(b"display_errors") {
            None => DISPLAY_ERRORS_STDOUT,
            Some(value) => match value.to_ascii_lowercase().as_slice() {
                b"on" | b"yes" | b"true" | b"stdout" => DISPLAY_ERRORS_STDOUT,
                b"stderr" => DISPLAY_ERRORS_STDERR,
                _ => display_errors_mode(ini_atol(value)),
            },
        };
        match mode {
            0 => Display::Off,
            DISPLAY_ERRORS_STDERR if self.host_writes_stderr() => Display::Stderr,
            _ => Display::Body,
        }
    }

    /// Whether the host is one php writes `display_errors=stderr` to
    /// the error stream under.
    fn host_writes_stderr(&self) -> bool {
        matches!(
            self.description.sapi_name.as_str(),
            "cli" | "cgi" | "phpdbg"
        )
    }

    /// `zend_error_noreturn(E_ERROR, ...)`: `php_error_cb` renders the
    /// fatal where the request stands, on the streams the two ini entries
    /// say, and the request ends in it. It is rendered once, here, and
    /// `serve` writes nothing of its own.
    pub(crate) fn end_in_fatal(&mut self, message: &str) {
        // One raised while a bailout is under way is not rendered: php
        // left what raised it at the first. One raised after the bailout
        // was caught, in a later step of `php_request_shutdown`, is.
        if self.bailout {
            return;
        }
        let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        let line = self.current_line();
        self.note_error_in(Level::Error, message, &file, line);
        self.end_at_level(Level::Error, message, &file, line);
    }

    /// `zend_error_noreturn(E_CORE_ERROR, ...)`: the one core fatal
    /// this engine raises, `zend_objects_destroy_object`'s. php gives
    /// `E_CORE_ERROR` no site at all (`get_filename_lineno`,
    /// `Z/Zend/zend.c:1642`), so it is rendered at `Unknown` on line
    /// zero wherever it was raised, and the request ends in it.
    fn end_in_core_error(&mut self, message: &str) {
        if self.bailout {
            return;
        }
        self.note_error_in(Level::CoreError, message, b"Unknown", 0);
        self.end_at_level(Level::CoreError, message, b"Unknown", 0);
    }

    /// `zend_exception_error` (`Z/Zend/zend_exceptions.c:936-990`): an
    /// uncaught throwable rendered at the site it recorded, through
    /// `zend_error_cb` alone and never `zend_error_impl`, with
    /// `EG(last_fatal_error_backtrace)` cleared first, so the diagnostic
    /// carries the throwable's own trace in its message and never a
    /// fatal's after the site, whatever `fatal_error_backtraces` says. A
    /// `ParseError` or a `CompileError` is rendered at its own level and
    /// the rest as `E_ERROR`, and the request ends as the level ends it.
    pub(crate) fn end_in_uncaught(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
        if self.bailout {
            return;
        }
        self.last_fatal_error_backtrace = None;
        self.error_cb(level, message, file, line);
        self.end_at_level(level, message, file, line);
    }

    /// `php_error_cb`'s `_zend_bailout` (`Z/main/main.c:1492-1520`) for a level
    /// it cannot recover from that the built-in handler served, raised
    /// where the request stands: every object marked destructed,
    /// `zend_objects_store_mark_destructed`, so no destructor is served
    /// after it, and the request ended at the level. What
    /// `trigger_error()` reaches with `E_USER_ERROR`.
    pub(crate) fn bail_out(&mut self, level: Level, message: &str) {
        let (file, line) = if self.executing() {
            let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
            (file, self.current_line())
        } else {
            (b"Unknown".to_vec(), 0)
        };
        self.marked_destructed = true;
        self.end_at_level(level, message, &file, line);
    }

    /// The ending alone, the diagnostic already rendered at its own
    /// level: php's status alone for `E_PARSE`, and for every other
    /// fatal level the log form the host is handed, the trace under
    /// `Stack trace:` where `php_error_cb` gave `php_log_err` one. The
    /// cycle collector takes no root from here on, as `_zend_bailout`
    /// protects it (`Z/Zend/zend.c:1263`), and a collection serving
    /// destructors when the fatal came returns without freeing.
    fn end_at_level(&mut self, level: Level, message: &str, file: &[u8], line: u32) {
        self.heap.gc_protect(true);
        self.exit_status = 255;
        self.bailout = true;
        if self.ending.is_some() {
            return;
        }
        self.ending = Some(match level {
            Level::Parse => Ending::Parse,
            _ => Ending::Fatal(format!(
                "PHP Fatal error:  {message} in {} on line {line}{}",
                String::from_utf8_lossy(file),
                self.stack_trace_after_the_site()
            )),
        });
    }

    /// `E_PARSE` or `E_COMPILE_ERROR` at a site of its own, which is what
    /// the compiler's refusal carries: rendered once through
    /// `php_error_cb` at its level, and the request ended as that level
    /// ends it, php's status alone for the parse error and the log form
    /// beside it for the compile error, which `php_error_cb` bails out
    /// of as it does from `E_ERROR`. php's parser refuses by throwing a
    /// `ParseError` (`zenderror`, `Z/Zend/zend_language_parser.y`), which
    /// `zend_exception_error` renders with no trace fetched, while the
    /// compiler's `zend_error_noreturn(E_COMPILE_ERROR, …)` goes through
    /// `zend_error_impl` and fetches one, `#0 {main}` with no frame
    /// standing.
    pub(crate) fn end_in_compile_time(
        &mut self,
        level: Level,
        message: &str,
        file: &[u8],
        line: u32,
    ) {
        if level == Level::Parse {
            self.end_in_uncaught(level, message, file, line);
            return;
        }
        self.note_error_in(level, message, file, line);
        self.end_at_level(level, message, file, line);
    }

    /// `EG(exit_status) = status`, `ZEND_FUNCTION(exit)`'s int arm: the
    /// last write wins, so an `exit` in a destructor or a shutdown
    /// function moves the status a fatal or an earlier `exit` set.
    pub(crate) fn end_in_exit(&mut self, code: i32) {
        self.exit_status = code;
    }

    /// php's `EG(exit_status)`, what `serve` reports the request ended
    /// with.
    pub(crate) fn exit_status(&self) -> i32 {
        self.exit_status
    }

    /// `zend_throw_unwind_exit` (`Z/Zend/zend_exceptions.c:1061`): the
    /// unwind exit is standing where a throwable would be, for the
    /// unwinder to read; php asserts nothing else is pending, and a
    /// raiser that found something pending returns it instead.
    pub(crate) fn throw_unwind_exit(&mut self) {
        if self.pending.is_none() {
            self.unwind_exit = true;
        }
    }

    /// Whether the unwind exit is what is being unwound: php's
    /// `zend_is_unwind_exit(EG(exception))`, read by the unwinder where
    /// it would enter a `finally` block or chain a throwable, and by
    /// [`Request::serve_user`] where a native reads what its callback
    /// left.
    pub(crate) fn unwind_exit(&self) -> bool {
        self.unwind_exit
    }

    /// What the unwinder leaves by that no `catch` takes and no `finally`
    /// is served for: the unwind exit, or a fatal leaving for its
    /// `zend_try`.
    pub(crate) fn unwinding(&self) -> bool {
        self.unwind_exit || self.bailout
    }

    /// `zend_end_try()` over a fatal's `zend_bailout`: the `zend_try` it left
    /// for has caught it, and what follows is served again.
    ///
    /// A fatal a native raises inside a callback leaves the callback,
    /// the native that called it and the program, as `zend_bailout`
    /// leaves them all for the `zend_try` in `php_execute_script`:
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// array_map(function ($x) { echo "cb $x\n"; str_repeat('x', 4 << 20); echo "after $x\n"; }, [1, 2]);
    /// echo "main after\n";
    /// ```
    /// ```expect exit=255
    /// cb 1
    ///
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 3
    /// ```
    ///
    /// No `finally` is served on the way out, and each step of
    /// `php_request_shutdown` is a `zend_try` of its own: the shutdown
    /// functions are served after the program's fatal, one of them
    /// raising a second, which is rendered and ends the walk.
    ///
    /// ```php
    /// register_shutdown_function(function () { echo "sd1\n"; str_repeat('x', 4 << 20); echo "sd1 after\n"; });
    /// register_shutdown_function(function () { echo "sd2\n"; });
    /// ini_set('memory_limit', '2M');
    /// try { str_repeat('y', 4 << 20); } finally { echo "finally\n"; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 5
    /// sd1
    ///
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 2
    /// ```
    pub(crate) fn catch_bailout(&mut self) {
        self.bailout = false;
    }

    /// `zend_is_unwind_exit(EG(exception))` for a native: whether a
    /// [`Thrown`] that carries no throwable is the `exit` a callback
    /// unwinds, which php's C code goes on past with the unwind exit
    /// pending, rather than a fatal, which `zend_bailout` leaves the
    /// native by. The session's save handler reads it after a user
    /// handler returns (`ps_call_handler`).
    #[must_use]
    pub fn is_unwind_exit(&self) -> bool {
        self.unwind_exit
    }

    /// The request's [`Phase`].
    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }

    /// The request moved to `phase`, as `serve` crosses php's startup
    /// and shutdown.
    pub(crate) fn enter_phase(&mut self, phase: Phase) {
        self.phase = phase;
    }

    pub(crate) fn end_in(&mut self, fatal: &Fatal) {
        match fatal {
            Fatal::Error(message) => self.end_in_fatal(message),
            Fatal::OutOfMemory(bytes) => self.end_in_fatal(&format!(
                "Allowed memory size of {} bytes exhausted (tried to allocate {bytes} bytes)",
                self.ini_parse_bytes_of("memory_limit")
            )),
        }
        // A fatal the heap raised lifted its limit for the rendering, as
        // `zend_mm_safe_error` does; it holds again from here.
        self.heap.overflow_rendered();
    }

    /// `zend_objects_new` over `class` (`object_init_ex` without the
    /// constructor): a store entry out of this request's heap with one
    /// owner, a slot per declared instance property, as
    /// `zend_object_properties_size` counts them, and the memory limit
    /// as the fatal that ends the request.
    pub(crate) fn new_object(&mut self, class: ClassId) -> Result<PhpObject, Thrown> {
        let slots = self.class_slots(class);
        let defaults = self.property_defaults(class)?;
        // `ce->create_object`: a class that declares a native payload
        // lays it down here, with the object, as
        // `zend_object_alloc(sizeof(custom), ce)` does; every other
        // class is `zend_objects_new` (design §5.4).
        let allocated = match self.native_new_of(class) {
            Some(new) => new(&mut self.heap, class.0, slots),
            None => self.heap.object(class.0, slots),
        };
        let object = match self.fatal_of(allocated).map(PhpObject::of) {
            Ok(object) => object,
            Err(thrown) => {
                self.release_defaults(defaults);
                return Err(thrown);
            }
        };
        self.init_properties(object, defaults);
        // `zend_default_exception_new`, the throwables' `create_object`:
        // `file`, `line` and `trace` are the site of `new`, written as
        // the object is made and before any constructor runs.
        if crate::classes::records_throw_site(self.handlers_of(class))
            && let Err(thrown) = self.note_throw_site(object)
        {
            self.release(object.as_value());
            return Err(thrown);
        }
        Ok(object)
    }

    /// [`Request::new_object`] carrying `payload` as the native slot of
    /// design §5.4, `zend_object_alloc(sizeof(custom), ce)`: what an
    /// internal class's methods reach through the receiver as `T`, and
    /// what its table's `free_obj` and `get_gc` release and walk.
    pub(crate) fn new_object_with<T: std::any::Any + Send>(
        &mut self,
        class: ClassId,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let slots = self.class_slots(class);
        let defaults = self.property_defaults(class)?;
        let allocated = self.heap.object_with(class.0, slots, payload);
        let object = match self.fatal_of(allocated).map(PhpObject::of) {
            Ok(object) => object,
            Err(thrown) => {
                self.release_defaults(defaults);
                return Err(thrown);
            }
        };
        self.init_properties(object, defaults);
        Ok(object)
    }

    /// `object_properties_init`: each declared instance property's slot
    /// takes its default, `zend_update_class_constants` having evaluated
    /// the expression it was written as in the scope of the class that
    /// declared it (`ce->properties_info_table[i]->ce`); a property with
    /// no default is `null` where it is untyped and stays uninitialised
    /// where it is typed, as `zend_declare_typed_property` leaves it.
    /// The slots are [`Request::placed_properties`]', so an inherited
    /// property takes the parent's default in the parent's slot.
    ///
    /// The defaults are evaluated before the object is made, as
    /// `zend_update_class_constants` runs inside `object_init_ex` before
    /// `zend_objects_new`, so an object a default makes, an enum's case
    /// among them, takes its number in the object store before the object being made does.
    ///
    /// ```php
    /// class A { public $p = E::V; }
    /// enum E { case V; }
    /// var_dump(new A);
    /// ```
    /// ```expect
    /// object(A)#2 (1) {
    ///   ["p"]=>
    ///   enum(E::V)
    /// }
    /// ```
    fn property_defaults(&mut self, class: ClassId) -> Result<Vec<Option<Value>>, Thrown> {
        let defaults: Vec<(ClassId, bool, Option<php_unit::decl::ConstExpr>)> = self
            .placed_properties(class)
            .into_iter()
            .map(|(declaring, p)| (declaring, p.ty.is_some(), p.default))
            .collect();
        let mut out = Vec::with_capacity(defaults.len());
        for (declaring, typed, default) in defaults {
            out.push(match default {
                Some(expr) => match crate::api::call::eval_const_expr(self, &expr, Some(declaring))
                {
                    Ok(value) => Some(value),
                    Err(thrown) => {
                        self.release_defaults(out);
                        return Err(thrown);
                    }
                },
                None if typed => None,
                None => Some(Value::null()),
            });
        }
        Ok(out)
    }

    /// Defaults [`Request::property_defaults`] evaluated for an object
    /// that was never made, given up.
    fn release_defaults(&mut self, defaults: Vec<Option<Value>>) {
        for value in defaults.into_iter().flatten() {
            self.release(value);
        }
    }

    /// The defaults written into the object's slots, each slot's owner
    /// from here; a typed property with none stays uninitialised.
    fn init_properties(&mut self, object: PhpObject, defaults: Vec<Option<Value>>) {
        for (i, value) in defaults.into_iter().enumerate() {
            let slot = u32::try_from(i).unwrap_or(u32::MAX);
            // A typed slot with no default is `IS_UNDEF` and
            // `IS_PROP_UNINIT`, as `object_properties_init` copies it.
            let Some(value) = value else {
                object
                    .heap()
                    .set_slot_bit(slot, php_heap::PropBit::Uninit, true);
                continue;
            };
            Heap::object_set_slot(&mut Hooks(self), object.heap(), slot, value.into_zval());
        }
    }

    /// `ce->default_properties_table` beside `ce->properties_info_table`
    /// as `zend_do_inheritance` and `zend_build_properties_info_table`
    /// leave them (`Z/Zend/zend_inheritance.c:1889`, `:1686`): the
    /// declared instance properties in slot order, a parent's before the
    /// class's own, since inheritance copies the parent's table to the
    /// front of the child's and moves the child's own up behind it. A
    /// property the class redeclares keeps the slot the parent gave it,
    /// `do_inherit_property`'s `child_info->offset =
    /// parent_info->offset`, and the class's own declaration is what
    /// stands there. A parent's *private* property is not redeclarable:
    /// php leaves its slot alone and gives the child's a slot of its
    /// own, which is how `var_dump` shows both. Each row names the class
    /// that declared what stands in the slot, which is the scope a
    /// private name is measured against, `property_info->ce`.
    ///
    /// The order is what `rebuild_object_properties_internal` walks
    /// (`Z/Zend/zend_object_handlers.c:75`), so it is the order
    /// `var_dump`, the `(array)` cast and a `foreach` over an object
    /// read; `ReflectionMethod` declares `$class` over
    /// `ReflectionFunctionAbstract`'s `$name`, so the inherited name
    /// prints first. An inherited property has a slot from here on, so
    /// it takes the parent's default where php's
    /// `object_properties_init` copies one.
    ///
    /// ```php
    /// class P { public $a = 1; private $p = 2; protected $q = 3; }
    /// class C extends P { public $b = 4; public $p = 5; }
    /// var_dump(new C, new ReflectionMethod('Exception', 'getCode'), (new ReflectionClass('JsonException'))->newInstanceWithoutConstructor()->getMessage());
    /// ```
    /// ```expect
    /// object(C)#1 (5) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["p":"P":private]=>
    ///   int(2)
    ///   ["q":protected]=>
    ///   int(3)
    ///   ["b"]=>
    ///   int(4)
    ///   ["p"]=>
    ///   int(5)
    /// }
    /// object(ReflectionMethod)#2 (2) {
    ///   ["name"]=>
    ///   string(7) "getCode"
    ///   ["class"]=>
    ///   string(9) "Exception"
    /// }
    /// string(0) ""
    /// ```
    ///
    /// The `(array)` cast mangles by the slot and not by the name, so
    /// the two `$p` slots are two keys, php's
    /// `zend_mangle_property_name` naming the private one with the class
    /// that declared it:
    ///
    /// ```php
    /// class P { private $p = 1; protected $q = 2; public $r = 3; }
    /// class C extends P { public $p = 9; public $s = 4; }
    /// var_dump((array)(new C));
    /// ```
    /// ```expect
    /// array(5) {
    ///   ["%0P%0p"]=>
    ///   int(1)
    ///   ["%0*%0q"]=>
    ///   int(2)
    ///   ["r"]=>
    ///   int(3)
    ///   ["p"]=>
    ///   int(9)
    ///   ["s"]=>
    ///   int(4)
    /// }
    /// ```
    pub(crate) fn placed_properties(&self, class: ClassId) -> Vec<(ClassId, PropertyDecl)> {
        let mut chain = Vec::new();
        let mut at = Some(class);
        for _ in 0..64 {
            let Some(here) = at else { break };
            chain.push(here);
            at = self.class_parent(here);
        }
        // Each row beside whether it is still `ZEND_ACC_VIRTUAL`, which
        // a virtual property is while no declaration of it up the chain
        // is backed, and which has no slot (`ZEND_VIRTUAL_PROPERTY_OFFSET`):
        // a backed redeclaration of a virtual one takes a slot of its
        // own where the child's properties are, as
        // `do_inherit_property` leaves the child's offset alone over a
        // virtual parent.
        let mut table: Vec<(ClassId, PropertyDecl, bool)> = Vec::new();
        for here in chain.into_iter().rev() {
            let Some(decl) = self.class_decl(here) else {
                continue;
            };
            for property in decl.properties.iter().filter(|p| p.is_instance) {
                let declared_virtual = property.is_virtual;
                let inherited = table.iter().position(|(_, p, _)| {
                    p.name == property.name && p.visibility != Visibility::Private
                });
                match inherited {
                    Some(i) if !table[i].2 => table[i] = (here, property.clone(), false),
                    Some(i) => {
                        table.remove(i);
                        table.push((here, property.clone(), declared_virtual));
                    }
                    None => table.push((here, property.clone(), declared_virtual)),
                }
            }
        }
        table
            .into_iter()
            .filter(|(_, _, is_virtual)| !is_virtual)
            .map(|(class, property, _)| (class, property))
            .collect()
    }

    /// [`Request::new_object`] by the class's name; a name the tables do
    /// not know is class zero, `Stringable`, as the skeleton has it.
    pub(crate) fn new_object_of(&mut self, class_name: &str) -> Result<PhpObject, Thrown> {
        let class = self
            .class_named(class_name.as_bytes())
            .unwrap_or(ClassId(0));
        self.new_object(class)
    }

    /// `ce->default_properties_count`: the declared instance properties
    /// of the class and of every class above it, each a slot on the
    /// object ([`Request::placed_properties`]).
    fn class_slots(&self, class: ClassId) -> u32 {
        u32::try_from(self.placed_properties(class).len()).unwrap_or(u32::MAX)
    }

    /// The slot a declared instance property of `class` sits in,
    /// `zend_get_property_offset` without visibility yet; `None` for a
    /// name no class of the chain declares, which is a dynamic property.
    /// The last row of the name is the one `ce->properties_info` holds,
    /// since a class's own declaration is appended after a parent's
    /// private one of the same name and hides it.
    fn prop_slot(&self, class: ClassId, name: &[u8]) -> Option<u32> {
        self.placed_properties(class)
            .iter()
            .rposition(|(_, p)| &*p.name.0 == name)
            .and_then(|i| u32::try_from(i).ok())
    }

    /// A property read, `zend_std_read_property` without the handler
    /// in the way: the declared slot, `None` while nothing wrote it, or
    /// the dynamic table's entry. A view, no owner taken.
    pub(crate) fn object_prop(&self, o: Obj, name: &[u8]) -> Option<Value> {
        if let Some(i) = self.prop_slot(ClassId(o.class()), name) {
            let z = o.slot(i);
            return (!z.is_undef()).then(|| Value::of_zval(z));
        }
        o.properties()?
            .iter()
            .find(|(k, _)| matches!(k, php_heap::Key::Str(s) if s.bytes() == name))
            .map(|(_, z)| Value::of_zval(z))
    }

    /// `zend_get_property_offset` over the placed instance properties:
    /// the slot `name` sits in, the class that declared what stands
    /// there, its visibility, its set visibility and whether it is
    /// typed; none for a name no class of the chain declares, which is
    /// a dynamic property.
    fn declared_slot(&self, class: ClassId, name: &[u8]) -> Option<DeclaredSlot> {
        let placed = self.placed_properties(class);
        let at = placed.iter().rposition(|(_, p)| &*p.name.0 == name)?;
        let (declaring, p) = placed.get(at)?;
        Some(DeclaredSlot {
            slot: u32::try_from(at).unwrap_or(u32::MAX),
            declaring: *declaring,
            visibility: p.visibility,
            set_visibility: p.set_visibility,
            typed: p.ty.is_some(),
            readonly: p.is_readonly,
        })
    }

    /// `zend_asymmetric_property_has_set_access`
    /// (`Z/Zend/zend_object_handlers.c:590`): whether the executed
    /// scope may write a property of `class` whose set visibility is
    /// `set`. The declaring class writes its own; a `protected(set)`
    /// one is a related scope's too. A `public(set)` never reaches
    /// here, since `zend_declare_typed_property` drops a set
    /// visibility the read visibility already equals and refuses one
    /// weaker than it.
    fn has_set_access(&self, class: ClassId, set: Visibility) -> bool {
        let scope = self.executed_scope();
        scope == Some(class) || (set == Visibility::Protected && self.check_protected(class, scope))
    }

    /// `zend_asymmetric_visibility_property_modification_error`
    /// (`Z/Zend/zend_execute.c:931`): php's `Error` for a write the set
    /// visibility refuses, `operation` the word the site does
    /// (`modify`, `indirectly modify`, `unset`). php names the called
    /// scope here, not the executed one; the two differ only where a
    /// method is reached through a class that inherited it, and the
    /// linker that inherits a method arrives with the classes of design
    /// §11 days 8-14. A readonly property's `protected(set)` is named
    /// with its `readonly`, which is the set visibility
    /// `zend_declare_typed_property` gives every public readonly one:
    ///
    /// ```php
    /// class A { public readonly int $x; function __construct(bool $set) { if ($set) { $this->x = 1; } } }
    /// $a = new A(false);
    /// try { $a->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($a->x); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $b = new A(true);
    /// try { $b->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($b->x); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot modify protected(set) readonly property A::$x from global scope
    /// Cannot unset protected(set) readonly property A::$x from global scope
    /// Cannot modify readonly property A::$x
    /// Cannot unset readonly property A::$x
    /// ```
    ///
    /// `__clone` may unset and set one again, php's `IS_PROP_REINITABLE`:
    ///
    /// ```php
    /// class F { function __construct(public readonly stdClass $b) {} function __clone() { unset($this->b); $this->b = new stdClass; } }
    /// $f = new F(new stdClass);
    /// $g = clone $f;
    /// var_dump($f->b === $g->b);
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn set_inaccessible_property(
        &mut self,
        class: ClassId,
        set: Visibility,
        readonly: bool,
        name: &[u8],
        operation: &str,
    ) -> Thrown {
        let word = match set {
            Visibility::Private => "private(set)",
            _ if readonly => "protected(set) readonly",
            _ => "protected(set)",
        };
        let from = match self.executed_scope() {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        };
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot {operation} {word} property {class}::${shown} from {from}"),
        )
    }

    /// `zend_verify_property_access`: whether the executed scope may
    /// reach a property of `class` declared with `visibility`.
    fn may_access(&self, class: ClassId, visibility: Visibility) -> bool {
        self.may_access_from(class, visibility, self.executed_scope())
    }

    /// [`Request::may_access`] from a scope named rather than the one
    /// executing: what `zend_verify_const_access` checks a class
    /// constant against inside a constant expression, the scope it is
    /// evaluated in.
    fn may_access_from(
        &self,
        class: ClassId,
        visibility: Visibility,
        scope: Option<ClassId>,
    ) -> bool {
        match visibility {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(class, scope),
        }
    }

    /// `CE_CONSTANTS_TABLE(ce)` as `zend_do_inheritance` leaves it, read
    /// by walking the declarations instead of copying them: the class's
    /// own constants, then its parent's and its interfaces', and a
    /// parent's private constant left out, which is
    /// `do_inherit_class_constant`'s skip. The answer is the class that
    /// declared it, which is the scope the expression is evaluated in
    /// and the scope its visibility is checked against, and the
    /// declaration itself.
    fn class_constant_of(
        &self,
        class: ClassId,
        name: &[u8],
        depth: u32,
    ) -> Option<(ClassId, ClassConstDecl)> {
        if depth > 64 {
            return None;
        }
        let decl = self.class_decl(class)?;
        if let Some(constant) = decl
            .constants
            .iter()
            .find(|c| *c.name.0 == *name && (depth == 0 || c.visibility != Visibility::Private))
        {
            return Some((class, constant.clone()));
        }
        let sources: Vec<Name> = decl
            .parent
            .iter()
            .chain(decl.interfaces.iter())
            .cloned()
            .collect();
        for source in sources {
            let Some(source) = self.class_named(&source.0) else {
                continue;
            };
            if let Some(found) = self.class_constant_of(source, name, depth.saturating_add(1)) {
                return Some(found);
            }
        }
        None
    }

    /// `zend_fetch_class_constant`'s lookup (`Z/Zend/zend_constants.c:355`):
    /// the constant `name` of `class` from the declaration's table,
    /// `Undefined constant C::A` where it has none, `Cannot access
    /// private constant C::A` where `zend_verify_const_access` refuses
    /// the executing scope, and otherwise its value, the constant
    /// expression the declaration kept evaluated in the class's scope
    /// (`zval_update_constant_ex`), one owner the caller's. The class
    /// is named in the error as the opline names it, `ce->name`.
    pub(crate) fn class_constant(&mut self, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
        let class_name = self.class_name(class);
        self.class_constant_shown(class, name, &class_name)
    }

    /// [`Request::class_constant`] with the class named in the error as
    /// `class_name` spells it: `zend_get_class_constant_ex` words
    /// `Undefined constant a::Y` with the name it was handed, which is
    /// the string `constant()` or `defined()` was given, and not the
    /// linked class's own.
    pub(crate) fn class_constant_shown(
        &mut self,
        class: ClassId,
        name: &[u8],
        class_name: &str,
    ) -> Result<Value, Thrown> {
        let scope = self.executed_scope();
        self.class_constant_from(class, name, class_name, scope)
    }

    /// `zend_get_class_constant_ex` with the `scope` a constant
    /// expression is evaluated in, `zend_ast_evaluate`'s: the access is
    /// checked against it and not against the scope executing, so an
    /// attribute's `parent::X` reads a protected constant its class may.
    ///
    /// ```php
    /// #[Attribute(parent::x)]
    /// class x extends y {}
    /// class y { protected const x = Attribute::TARGET_CLASS; }
    /// #[x]
    /// class z {}
    /// var_dump((new ReflectionClass(z::class))->getAttributes()[0]->newInstance());
    /// ```
    /// ```expect
    /// object(x)#1 (0) {
    /// }
    /// ```
    pub(crate) fn class_constant_from(
        &mut self,
        class: ClassId,
        name: &[u8],
        class_name: &str,
        scope: Option<ClassId>,
    ) -> Result<Value, Thrown> {
        let shown = String::from_utf8_lossy(name).into_owned();
        let Some((declaring, constant)) = self.class_constant_of(class, name, 0) else {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Undefined constant {class_name}::{shown}"),
            ));
        };
        let visibility = constant.visibility;
        let value = constant.value.clone();
        let asked = class;
        let class = declaring;
        if !self.may_access_from(class, visibility, scope) {
            let word = match visibility {
                Visibility::Public => "public",
                Visibility::Protected => "protected",
                Visibility::Private => "private",
            };
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Cannot access {word} constant {class_name}::{shown}"),
            ));
        }
        // A constant a trait declares is the composing class's to hand
        // out, never the trait's own.
        if self
            .class_decl(asked)
            .is_some_and(|decl| decl.kind == ClassKind::Trait)
        {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Cannot access trait constant {class_name}::{shown} directly"),
            ));
        }
        if crate::attributes::Flags::of(&constant.attributes).deprecated {
            self.deprecated_class_constant(class, name, &constant)?;
        }
        // `ZEND_FETCH_CLASS_CONSTANT` updates a backed enum's constants
        // whole before it reads one (`zend_update_class_constants`), and
        // `zend_fetch_class_constant`'s `ZEND_CLASS_CONST_IS_CASE` arm
        // hands out a case as the enum's instance for it and not as the
        // expression it was written with.
        self.update_enum_constants(class)?;
        if constant.is_case {
            return self.enum_case(class, name).map(|case| case.as_value());
        }
        let evaluated = crate::api::call::eval_const_expr(self, &value, Some(class))?;
        match &constant.ty {
            Some(ty) if ty.mask() != php_unit::decl::Type::MAY_BE_ANY => {
                crate::ops::verify_class_constant_type(self, evaluated, ty, class, name)
            }
            _ => Ok(evaluated),
        }
    }

    /// `zend_get_class_constant_ex` under `ZEND_FETCH_CLASS_SILENT`, as
    /// `defined()` asks it: whether `class` has a constant `name` the
    /// executed scope may reach, not through a trait, with nothing
    /// raised and nothing evaluated.
    pub(crate) fn class_constant_defined(&self, class: ClassId, name: &[u8]) -> bool {
        let trait_asked = self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait);
        self.class_constant_of(class, name, 0)
            .is_some_and(|(declaring, constant)| {
                !trait_asked && self.may_access(declaring, constant.visibility)
            })
    }

    /// `ZEND_ACC_DEPRECATED` on a class constant, as
    /// `ZEND_FETCH_CLASS_CONSTANT` and `zend_get_class_constant_ex` test
    /// it after the access checks and before the value is evaluated:
    /// [`crate::attributes::deprecated_class_constant`], once per fetch,
    /// and not again while its own attribute's arguments are read for
    /// the message (`CONST_IS_RECURSIVE`, a program's class alone).
    fn deprecated_class_constant(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        constant: &ClassConstDecl,
    ) -> Result<(), Thrown> {
        let user = !self.native_class(declaring);
        if user && !self.protect_recursion(Some(declaring), name) {
            return Ok(());
        }
        let raised =
            crate::attributes::deprecated_class_constant(self, declaring, name, constant, !user);
        if user {
            self.deprecating.pop();
        }
        raised
    }

    /// `CONST_PROTECT_RECURSION` over a constant, `class` none for a
    /// global one: `false`, and nothing held, where it is held already,
    /// which is `CONST_IS_RECURSIVE`; the caller pops what it held.
    pub(crate) fn protect_recursion(&mut self, class: Option<ClassId>, name: &[u8]) -> bool {
        if self
            .deprecating
            .iter()
            .any(|(held_class, held)| *held_class == class && **held == *name)
        {
            return false;
        }
        self.deprecating.push((class, name.into()));
        true
    }

    /// `CONST_UNPROTECT_RECURSION`: the last constant
    /// [`Request::protect_recursion`] held let go.
    pub(crate) fn unprotect_recursion(&mut self) {
        self.deprecating.pop();
    }

    /// `zend_enum_build_backed_enum_table` (`Z/Zend/zend_enum.c:197`),
    /// which `zend_update_class_constants` runs for a backed enum a
    /// program declared: each case's value evaluated and held to the
    /// backing type, php's `TypeError` for one of another type, and two
    /// cases of one value php's `Error`. A native enum's values are the
    /// stub's and are not checked, as php checks only a user class's.
    ///
    /// ```php
    /// enum Foo: int { case Bar = 'bar'; }
    /// try { var_dump(Foo::Bar); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// enum Dup: int { case A = 1; case B = 1; }
    /// try { Dup::from(1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Enum case type string does not match enum backing type int
    /// Duplicate value in enum Dup for cases A and B
    /// ```
    pub(crate) fn update_enum_constants(&mut self, class: ClassId) -> Result<(), Thrown> {
        if self.enums_updated.contains(&class) || self.native_class(class) {
            return Ok(());
        }
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        if decl.kind != ClassKind::Enum || decl.backing == php_unit::decl::EnumBacking::Pure {
            return Ok(());
        }
        let backing = decl.backing;
        let cases: Vec<(Vec<u8>, php_unit::decl::ConstExpr)> = decl
            .constants
            .iter()
            .filter(|constant| constant.is_case)
            .map(|constant| (constant.name.0.to_vec(), constant.value.clone()))
            .collect();
        let enum_name = self.class_name(class);
        let mut seen: Vec<(Value, Vec<u8>)> = Vec::new();
        let mut outcome = Ok(());
        for (name, expr) in cases {
            let value = match crate::api::call::eval_const_expr(self, &expr, Some(class)) {
                Ok(value) => value,
                Err(thrown) => {
                    outcome = Err(thrown);
                    break;
                }
            };
            let wanted = match backing {
                php_unit::decl::EnumBacking::Int => TypeTag::Int,
                _ => TypeTag::String,
            };
            if value.tag() != wanted {
                let given = value.type_name();
                let want = if wanted == TypeTag::Int {
                    "int"
                } else {
                    "string"
                };
                self.release(value);
                outcome = Err(crate::api::raising::throw(
                    self,
                    "TypeError",
                    &format!("Enum case type {given} does not match enum backing type {want}"),
                ));
                break;
            }
            if let Some((_, earlier)) = seen
                .iter()
                .find(|(held, _)| crate::api::coerce::identical(held, &value))
            {
                let earlier = String::from_utf8_lossy(earlier).into_owned();
                self.release(value);
                outcome = Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!(
                        "Duplicate value in enum {enum_name} for cases {earlier} and {}",
                        String::from_utf8_lossy(&name)
                    ),
                ));
                break;
            }
            seen.push((value, name));
        }
        for (value, _) in seen {
            self.release(value);
        }
        outcome?;
        self.enums_updated.push(class);
        Ok(())
    }

    /// Whether `class` is one the engine was built with, a native's,
    /// rather than one a program declared.
    fn native_class(&self, class: ClassId) -> bool {
        usize::try_from(class.0).is_ok_and(|at| at < self.shared.classes.len())
    }

    /// `zend_enum_get_case` (`Z/Zend/zend_enum.c:410`): the one instance
    /// of `class` standing for the case `name`, made on the first read
    /// and handed back by identity from then on, its `name` the case's
    /// own and, for a backed enum, its `value` the backing value the
    /// declaration carries. One more owner is taken for the caller, the
    /// slot keeping its own, as php's constant table keeps one.
    pub(crate) fn enum_case(&mut self, class: ClassId, name: &[u8]) -> Result<PhpObject, Thrown> {
        if let Some((_, case)) = self
            .enum_cases
            .iter()
            .find(|((held, under), _)| *held == class && under == name)
        {
            let case = *case;
            self.addref(&case.as_value());
            return Ok(case);
        }
        let Some((_, constant)) = self.class_constant_of(class, name, 0) else {
            let class = self.class_name(class);
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!(
                    "Undefined constant {class}::{}",
                    String::from_utf8_lossy(name)
                ),
            ));
        };
        let backing = self
            .class_decl(class)
            .map_or(php_unit::decl::EnumBacking::Pure, |decl| decl.backing);
        let case = self.new_object(class)?;
        let spelled = PhpString::new(self, name)?;
        let slot = PhpString::new(self, b"name")?;
        case.set_prop(self, slot, Value::string(spelled))?;
        if backing != php_unit::decl::EnumBacking::Pure {
            let value = crate::api::call::eval_const_expr(self, &constant.value, Some(class))?;
            let slot = PhpString::new(self, b"value")?;
            case.set_prop(self, slot, value)?;
        }
        self.enum_cases.push(((class, name.to_vec()), case));
        self.addref(&case.as_value());
        Ok(case)
    }

    /// `zend_hash_find_ptr(&ce->properties_info, name)` over the table
    /// `zend_do_inheritance` left: the class's own declaration of
    /// `name`, then each parent's, since a parent's entries are
    /// appended into the child's `properties_info` and one of the same
    /// name the child declares hides the parent's. The answer names the
    /// class that declared it, which for a `static` property is the
    /// class whose slot the value sits in (php's `IS_INDIRECT` into the
    /// parent's table) and the scope its visibility is measured
    /// against. An instance property is found here too, as php finds
    /// one in the same table and rejects it after the access check.
    pub(crate) fn property_info_of(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<(ClassId, PropertyDecl)> {
        let mut at = Some(class);
        for _ in 0..64 {
            let here = at?;
            let decl = self.class_decl(here)?;
            if let Some(property) = decl.properties.iter().find(|p| &*p.name.0 == name) {
                return Some((here, property.clone()));
            }
            at = self.class_parent(here);
        }
        None
    }

    /// `zend_std_get_static_property_with_info`
    /// (`Z/Zend/zend_object_handlers.c:2118`): the slot of the `static`
    /// property `name` on `class`, named by the class that declared it,
    /// with the declaration beside it. The access check is php's and
    /// comes first, so a private property of another class is `Cannot
    /// access private property C::$p` and not the undeclared text; a
    /// name no class of the chain declares, and one declared as an
    /// instance property, are `Access to undeclared static property
    /// C::$p`, both naming the class asked about. `scope` is
    /// `get_fake_or_executed_scope()`: the executed scope at an opline,
    /// the class itself where an extension reads through
    /// `zend_read_static_property_ex`, which sets the fake scope to it.
    /// The slot is laid down here on its first use,
    /// `zend_class_init_statics`, from the default the declaration
    /// carries, evaluated in the declaring scope; a typed property
    /// with no default is laid down `IS_UNDEF`, which is what php's
    /// read of one before initialization refuses.
    ///
    /// Under [`PropertyRead::Silent`], php's `BP_VAR_IS`, neither the
    /// access check nor the undeclared text raises: both are the lines
    /// php guards with `type != BP_VAR_IS`, and the answer is `None`,
    /// with no slot laid down, since php returns before
    /// `zend_class_init_statics`.
    ///
    /// `ZEND_ASSIGN_STATIC_PROP` addresses the slot here before it
    /// reads the value operand, which is why the handlers reach this.
    ///
    /// A property addressed on a trait itself, and not on a class that
    /// took it, is php's deprecation once the slot is found; each class
    /// that uses the trait has a slot of its own:
    ///
    /// ```php
    /// trait T { public static $s = 1; }
    /// class C { use T; }
    /// C::$s = 2;
    /// echo T::$s, C::$s, "\n";
    /// T::$s = 3;
    /// echo T::$s, C::$s, "\n";
    /// ```
    /// ```expect
    ///
    /// Deprecated: Accessing static trait property T::$s is deprecated, it should only be accessed on a class using the trait in %s on line 5
    /// 12
    ///
    /// Deprecated: Accessing static trait property T::$s is deprecated, it should only be accessed on a class using the trait in %s on line 6
    ///
    /// Deprecated: Accessing static trait property T::$s is deprecated, it should only be accessed on a class using the trait in %s on line 7
    /// 32
    /// ```
    pub(crate) fn static_slot(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        read: PropertyRead,
    ) -> Result<Option<(ClassId, PropertyDecl)>, Thrown> {
        let silent = read == PropertyRead::Silent;
        let found = self.property_info_of(class, name);
        if let Some((declaring, property)) = found.as_ref()
            && property.visibility != Visibility::Public
            && Some(*declaring) != scope
            && (property.visibility == Visibility::Private
                || !self.check_protected(*declaring, scope))
        {
            if silent {
                return Ok(None);
            }
            let visibility = property.visibility;
            return Err(self.inaccessible_property(class, visibility, name));
        }
        let Some((declaring, property)) = found.filter(|(_, p)| !p.is_instance) else {
            if silent {
                return Ok(None);
            }
            let class = self.class_name(class);
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!(
                    "Access to undeclared static property {class}::${}",
                    String::from_utf8_lossy(name)
                ),
            ));
        };
        if !self
            .statics
            .iter()
            .any(|((held, under), _)| *held == declaring && under == name)
        {
            let value = match property.default.as_ref() {
                Some(expr) => {
                    let expr = expr.clone();
                    crate::api::call::eval_const_expr(self, &expr, Some(declaring))?
                }
                None if property.ty.is_some() => Value::undef(),
                None => Value::null(),
            };
            self.statics.push(((declaring, name.to_vec()), value));
        }
        if self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait)
        {
            let shown = format!(
                "{}::${}",
                self.class_name(declaring),
                String::from_utf8_lossy(name)
            );
            crate::api::raising::deprecated(
                self,
                &format!(
                    "Accessing static trait property {shown} is deprecated, it should only be accessed on a class using the trait"
                ),
            );
        }
        Ok(Some((declaring, property)))
    }

    /// `CE_STATIC_MEMBERS(ce)[prop_info->offset]`: what the slot of the
    /// `static` property `name` on the class that declared it holds,
    /// nobody's to release, and `IS_UNDEF` where the slot stands
    /// unwritten or the table has none.
    pub(crate) fn held_static(&self, declaring: ClassId, name: &[u8]) -> Value {
        self.statics
            .iter()
            .find(|((at, under), _)| *at == declaring && under == name)
            .map_or_else(Value::undef, |(_, value)| *value)
    }

    /// `zend_read_static_property_ex` (`Z/Zend/zend_API.c`): the value
    /// of a `static` property, one more owner taken for the caller and
    /// the slot keeping its own (`RETURN_COPY_DEREF`), a reference
    /// followed. A typed slot nothing has written is php's `Error` for
    /// a `static` property read before initialization, which names the
    /// declaring class and not the one asked about.
    ///
    /// `read` is php's `type`, the `silent` argument of
    /// `zend_read_static_property_ex` under another name: under
    /// [`PropertyRead::Silent`] the property nothing declares, the one
    /// the scope may not reach and the typed slot nothing has written
    /// are each `null`, the last because php guards that throw with
    /// `type == BP_VAR_R || type == BP_VAR_RW`.
    pub(crate) fn static_property(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        let Some((declaring, property)) = self.static_slot(class, name, scope, read)? else {
            return Ok(Value::null());
        };
        let held = self.held_static(declaring, name);
        if held.tag() == TypeTag::Undef {
            if property.ty.is_some() && read == PropertyRead::Warning {
                let declaring = self.class_name(declaring);
                return Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!(
                        "Typed static property {declaring}::${} must not be accessed before initialization",
                        String::from_utf8_lossy(name)
                    ),
                ));
            }
            return Ok(Value::null());
        }
        let held = held.deref();
        self.addref(&held);
        Ok(held)
    }

    /// `zend_std_get_static_property(ce, name, BP_VAR_IS)` as the
    /// pointer php hands back, which
    /// `ReflectionClass::getStaticPropertyValue` and
    /// `setStaticPropertyValue` read rather than a value:
    /// [`Request::static_slot`] under [`PropertyRead::Silent`] is the
    /// `NULL`, so nothing here is a name no class of the chain
    /// declares, one declared as an instance property or one the scope
    /// may not reach; an undefined value is the slot php hands back
    /// holding `IS_UNDEF`, a typed property nothing has written, which
    /// [`Request::static_property`] flattens to `null` because php's
    /// `zend_read_static_property_ex` reads the slot and never the
    /// pointer; and otherwise one more owner of the value, a reference
    /// followed. Nothing is raised on those paths; a default
    /// expression that throws as the slot is laid down is php's own
    /// `zend_update_class_constants` failure and is handed on.
    pub(crate) fn static_property_silent(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
    ) -> Result<Option<Value>, Thrown> {
        let Some((declaring, property)) =
            self.static_slot(class, name, scope, PropertyRead::Silent)?
        else {
            return Ok(None);
        };
        let held = self.held_static(declaring, name);
        if held.tag() == TypeTag::Undef {
            return Ok(Some(if property.ty.is_some() {
                Value::undef()
            } else {
                Value::null()
            }));
        }
        let held = held.deref();
        self.addref(&held);
        Ok(Some(held))
    }

    /// `zend_update_static_property_ex` (`Z/Zend/zend_API.c`): the value
    /// is the slot's from here, and what the slot held is given up. A
    /// slot holding a reference is written through
    /// (`ZEND_TRY_ASSIGN_COPY_PROP`), so a name bound to it sees the
    /// write.
    ///
    /// A property with a declared type is held to it here
    /// ([`crate::ops::verify_property_type`]) under the regime the call
    /// site asks for, since php's two entries ask for different ones:
    /// `ZEND_ASSIGN_STATIC_PROP` verifies under the frame's
    /// `EX_USES_STRICT_TYPES()` and `zend_update_static_property_ex`,
    /// the entry Reflection reaches, under `strict = 0` whatever the
    /// file that called it declared (`Z/Zend/zend_API.c:1585`).
    pub(crate) fn set_static_property(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        value: Value,
        strict: bool,
    ) -> Result<(), Thrown> {
        let (declaring, property) =
            match self.static_slot(class, name, scope, PropertyRead::Warning) {
                Ok(Some(slot)) => slot,
                // A write is never `BP_VAR_IS`, so the silent miss cannot
                // stand here; the value is given up as a raising one is.
                Ok(None) => {
                    self.release(value);
                    return Ok(());
                }
                Err(thrown) => {
                    self.release(value);
                    return Err(thrown);
                }
            };
        self.set_static_slot(declaring, &property, name, value, strict)
    }

    /// [`Request::set_static_property`] over a slot already addressed,
    /// as `ZEND_ASSIGN_STATIC_PROP` addresses it before it reads its
    /// value and writes it after: `declaring` is the class the slot is
    /// in and `property` its declaration.
    pub(crate) fn set_static_slot(
        &mut self,
        declaring: ClassId,
        property: &PropertyDecl,
        name: &[u8],
        mut value: Value,
        strict: bool,
    ) -> Result<(), Thrown> {
        // `zend_verify_property_type`: the value held to the declared
        // type and coerced by the weak rule where the writer's regime is
        // not strict, and php's `TypeError` where it neither satisfies
        // nor coerces, the value given up as php gives it up.
        if let Some(ty) = property.ty.clone() {
            match crate::ops::verify_property_type(self, &value, &ty, declaring, name, strict) {
                Ok(None) => {}
                Ok(Some(coerced)) => {
                    self.release(value);
                    value = coerced;
                }
                Err(thrown) => {
                    self.release(value);
                    return Err(thrown);
                }
            }
        }
        // `zend_assign_to_variable` over a slot bound by reference: the
        // cell is held to every property it is typed by, which may be
        // another `static` than this one (`Test::$x =& Test::$y`).
        if let Some(cell) = self.static_slot_cell(declaring, name)
            && cell.heap().sources() != 0
        {
            return self.assign_to_reference(cell.heap(), value, strict);
        }
        self.store_static_property(declaring, name, value);
        Ok(())
    }

    /// The write half of [`Request::set_static_property`], php's
    /// `ZEND_TRY_ASSIGN_COPY_PROP` over the slot the declaring class
    /// owns: the value is the slot's from here and what it held is
    /// given up after the write, and a slot holding a reference is
    /// written *through*, so a name bound to it sees the write. A
    /// handler that has already held the value to the declared type
    /// reaches this one, since the verify raises a deprecation of its
    /// own and php runs it once (`zend_binary_assign_op_typed_prop`).
    ///
    /// The write through a reference gives what the cell held up once,
    /// the cell's own release after the write, so each value written
    /// over is freed and nothing is freed twice:
    ///
    /// ```php
    /// class C { public static $p; }
    /// $v = str_repeat("x", 32);
    /// C::$p = &$v;
    /// C::$p = str_repeat("y", 32);
    /// C::$p = str_repeat("z", 32);
    /// var_dump($v, C::$p);
    /// ```
    /// ```expect
    /// string(32) "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
    /// string(32) "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
    /// ```
    pub(crate) fn store_static_property(&mut self, declaring: ClassId, name: &[u8], value: Value) {
        let held = self
            .statics
            .iter()
            .position(|((at, under), _)| *at == declaring && under == name);
        let Some(at) = held else {
            self.release(value);
            return;
        };
        if let Some(ValueRef::Reference(cell)) = self.statics.get(at).map(|(_, v)| v.as_ref()) {
            // What the cell held is given up after the write.
            let cell = cell.heap();
            self.reference_set(cell, value);
            return;
        }
        let previous = self
            .statics
            .get_mut(at)
            .map(|(_, slot)| std::mem::replace(slot, value))
            .unwrap_or_default();
        self.release(previous);
    }

    /// `zend_fetch_static_property_address` under `BP_VAR_W`
    /// (`Z/Zend/zend_execute.c:3084`): the `static` property's slot as a
    /// place, which is what `FETCH_STATIC_PROP_W` hands its `VAR` as an
    /// `IS_INDIRECT` and the `ASSIGN_DIM` over it writes through.
    /// The slot is one [`Request::static_slot`] has already found and
    /// filled, which is where access is refused and a default laid down,
    /// so a handler can read the slot as it stands before this binding
    /// lays `null` in an unwritten one.
    ///
    /// php's slot is one zval of `CE_STATIC_MEMBERS(ce)`, a table whose
    /// address is stable for the life of the request. The slots here
    /// are values of a `Vec` the request owns, and no address of one
    /// outlives a write to that table, so a slot a place is asked for
    /// is bound to a reference cell first and the cell is the place.
    /// Every read and every write of a `static` property follows a
    /// reference already ([`Request::static_property`] derefs what it
    /// reads and [`Request::store_static_property`] writes through what
    /// the slot holds), so the binding is a place to write through and
    /// nothing a program can see.
    pub(crate) fn bind_static_place(
        &mut self,
        declaring: ClassId,
        name: &[u8],
    ) -> Result<php_heap::ZvalRef, Thrown> {
        if let Some(cell) = self.static_slot_cell(declaring, name) {
            return Ok(cell.heap().val_ref());
        }
        let held = self.held_static(declaring, name);
        let held = if held.tag() == TypeTag::Undef {
            Value::null()
        } else {
            held
        };
        // The row owns what it holds and the cell takes that owner, so
        // the row's value is replaced and never released.
        let cell = self.reference(held)?;
        let at = self
            .statics
            .iter()
            .position(|((held, under), _)| *held == declaring && under == name);
        let Some(at) = at else {
            return Ok(cell.heap().val_ref());
        };
        if let Some((_, slot)) = self.statics.get_mut(at) {
            *slot = Value::reference(cell);
        }
        Ok(cell.heap().val_ref())
    }

    /// The cell the `static` property's slot holds, for the handler that
    /// must know whether it is writing through a reference php would
    /// have typed (`ZEND_REF_HAS_TYPE_SOURCES` at the top of
    /// `ZEND_ASSIGN_STATIC_PROP_OP`); none where the slot holds a value.
    pub(crate) fn static_slot_cell(&self, declaring: ClassId, name: &[u8]) -> Option<PhpRef> {
        self.statics
            .iter()
            .find(|((at, under), _)| *at == declaring && under == name)
            .and_then(|(_, value)| value.as_php_ref().copied())
    }

    /// `zend_assign_to_variable_reference` over a `static` property's
    /// slot, which `ZEND_ASSIGN_STATIC_PROP_REF` runs: the cell itself
    /// becomes what the slot holds, so the property and every name
    /// bound to it are one value, and what the slot held is given up
    /// after the write. This is not [`Request::set_static_property`],
    /// which writes *through* a reference standing there; a bind
    /// replaces it.
    pub(crate) fn bind_static_property(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        bound: Value,
    ) -> Result<(), Thrown> {
        let (declaring, _) = match self.static_slot(class, name, scope, PropertyRead::Warning) {
            Ok(Some(slot)) => slot,
            Ok(None) => {
                self.release(bound);
                return Ok(());
            }
            Err(thrown) => {
                self.release(bound);
                return Err(thrown);
            }
        };
        let held = self
            .statics
            .iter()
            .position(|((at, under), _)| *at == declaring && under == name);
        let Some(at) = held else {
            self.release(bound);
            return Ok(());
        };
        let previous = self
            .statics
            .get_mut(at)
            .map(|(_, slot)| std::mem::replace(slot, bound))
            .unwrap_or_default();
        self.release(previous);
        Ok(())
    }

    /// `ZEND_REF_ADD_TYPE_SOURCE` (`Z/Zend/zend_types.h:1082`): the
    /// declared type of a property whose slot holds `cell` is put on the
    /// cell, so every write through any name bound to it is held to the
    /// type as well ([`Request::assign_to_reference`]). A cell typed by
    /// nothing yet takes a row of its own, whose word the cell carries.
    pub(crate) fn ref_add_type_source(&mut self, cell: Ref, source: RefSource) {
        let word = cell.sources();
        if word != 0
            && let Some((_, sources)) = self.ref_sources.iter_mut().find(|(at, _)| *at == word)
        {
            sources.push(source);
            return;
        }
        let word = self.next_ref_source;
        self.next_ref_source = self.next_ref_source.saturating_add(1);
        self.ref_sources.push((word, vec![source]));
        cell.set_sources(word);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE`: the property is no longer one of the
    /// cell's sources, because its slot now holds something else. The
    /// row goes with the last source, and the cell's word with it.
    pub(crate) fn ref_del_type_source(&mut self, cell: Ref, declaring: ClassId, name: &[u8]) {
        let word = cell.sources();
        if word == 0 {
            return;
        }
        let Some(at) = self.ref_sources.iter().position(|(held, _)| *held == word) else {
            return;
        };
        if let Some((_, sources)) = self.ref_sources.get_mut(at) {
            if let Some(i) = sources
                .iter()
                .position(|s| s.declaring == declaring && s.name == name)
            {
                sources.remove(i);
            }
            if !sources.is_empty() {
                return;
            }
        }
        self.ref_sources.remove(at);
        cell.set_sources(0);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE` over the property whose slot `slot`
    /// of `object` is (`zend_get_property_info_for_slot`): the cell the
    /// slot held is typed by that property no longer, because the slot
    /// is unset, reset or freed. A slot of an untyped property typed
    /// nothing.
    pub(crate) fn slot_source_gone(&mut self, object: PhpObject, slot: u32, cell: Ref) {
        let placed = self.placed_properties(object.class());
        let Some((declaring, p)) = usize::try_from(slot).ok().and_then(|at| placed.get(at)) else {
            return;
        };
        if p.ty.is_some() {
            self.ref_del_type_source(cell, *declaring, &p.name.0);
        }
    }

    /// `ZEND_REF_FOREACH_TYPE_SOURCES` as a slice: the properties whose
    /// slots hold this cell, empty for a cell no typed property's slot
    /// holds, which is `ZEND_REF_HAS_TYPE_SOURCES` reading false.
    pub(crate) fn ref_type_sources(&self, cell: Ref) -> &[RefSource] {
        let word = cell.sources();
        if word == 0 {
            return &[];
        }
        self.ref_sources
            .iter()
            .find(|(at, _)| *at == word)
            .map_or(&[], |(_, sources)| sources.as_slice())
    }

    /// `zend_assign_to_typed_ref` (`Z/Zend/zend_execute.c:4070`): a
    /// write through a cell, held first to every type its sources
    /// declare ([`crate::ops::verify_ref_assignable`]) and coerced to
    /// what they agree on. A cell no typed property's slot holds is
    /// [`Request::reference_set`] and nothing else, which is what php's
    /// `zend_assign_to_variable` does where `ZEND_REF_HAS_TYPE_SOURCES`
    /// reads false. The value is given up with the `TypeError` and what
    /// the cell held stands, as php gives up its copy.
    pub(crate) fn assign_to_reference(
        &mut self,
        cell: Ref,
        value: Value,
        strict: bool,
    ) -> Result<(), Thrown> {
        if cell.sources() == 0 {
            self.reference_set(cell, value);
            return Ok(());
        }
        match crate::ops::verify_ref_assignable(self, cell, &value, strict) {
            Ok(None) => self.reference_set(cell, value),
            Ok(Some(coerced)) => {
                self.release(value);
                self.reference_set(cell, coerced);
            }
            Err(thrown) => {
                self.release(value);
                return Err(thrown);
            }
        }
        Ok(())
    }

    /// php's notice where a write fetch fell back to `__get` and what it
    /// answered is neither an object nor a reference: a write through the
    /// copy has no effect on anything (`zend_std_read_property`, `Indirect
    /// modification of overloaded property`), naming the object's class.
    ///
    /// A private property the scope may not reach is `__get`'s too (#513):
    /// an int it answers is the notice, and an object is written through.
    ///
    /// ```php
    /// class C { private $p = 1; private $q;
    ///     function __construct() { $this->q = new stdClass; }
    ///     function __get($n) { return $this->$n; } }
    /// $c = new C;
    /// try { $c->p->x = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $c->q->x = 3;
    /// var_dump($c);
    /// ```
    /// ```expect
    ///
    /// Notice: Indirect modification of overloaded property C::$p has no effect in %s on line 6
    /// Attempt to assign property "x" on int
    /// object(C)#1 (2) {
    ///   ["p":"C":private]=>
    ///   int(1)
    ///   ["q":"C":private]=>
    ///   object(stdClass)#2 (1) {
    ///     ["x"]=>
    ///     int(3)
    ///   }
    /// }
    /// ```
    fn overloaded_write_notice(
        &mut self,
        class: ClassId,
        name: &[u8],
        read: PropertyRead,
        got: &Value,
    ) {
        if matches!(read, PropertyRead::Write | PropertyRead::Unset)
            && got.as_php_ref().is_none()
            && got.as_object().is_none()
        {
            let class = self.class_name(class);
            let shown = String::from_utf8_lossy(name).into_owned();
            crate::api::raising::notice(
                self,
                &format!(
                    "Indirect modification of overloaded property {class}::${shown} has no effect"
                ),
            );
        }
    }

    /// Whether a write fetch over `name` falls back to `read_property`
    /// for the class's `__get` to answer, where php's
    /// `zend_std_get_property_ptr_ptr` answers `NULL` for it: a declared
    /// property the scope may not reach, a declared slot it reaches that
    /// an `unset` emptied, or a name the
    /// object holds no dynamic property for, on a class with a `__get`
    /// not already serving the name. A slot that is `IS_PROP_UNINIT` or
    /// waits on a lazy initializer is the placement's.
    ///
    /// ```php
    /// class M { private $data = ['a' => [1], 'o' => null];
    ///     function __construct() { $this->data['o'] = new stdClass; }
    ///     function __get($n) { echo "__get $n\n"; return $this->data[$n]; } }
    /// $m = new M;
    /// $m->a[] = 2;
    /// $m->o->x = 3;
    /// var_dump($m->a, $m->o);
    /// ```
    /// ```expect
    /// __get a
    ///
    /// Notice: Indirect modification of overloaded property M::$a has no effect in %s on line 6
    /// __get o
    /// __get a
    /// __get o
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// object(stdClass)#2 (1) {
    ///   ["x"]=>
    ///   int(3)
    /// }
    /// ```
    pub(crate) fn overloaded_fetch(&self, object: PhpObject, name: &[u8]) -> bool {
        let class = object.class();
        if !self.serves_accessor(class, b"__get") || self.guard_word(object, name) & IN_GET != 0 {
            return false;
        }
        let o = object.heap();
        match self.declared_slot(class, name) {
            // A property the scope may not reach is placed silently where
            // the class has a `__get`, which answers for it.
            Some(placed) if !self.may_access(placed.declaring, placed.visibility) => true,
            Some(placed) => {
                o.slot(placed.slot).is_undef()
                    && !o.slot_uninit(placed.slot)
                    && !self.lazy_slot(o, placed.slot)
            }
            None => !o.properties().is_some_and(|table| {
                table
                    .iter()
                    .any(|(k, _)| matches!(k, php_heap::Key::Str(s) if s.bytes() == name))
            }),
        }
    }

    /// `fn_flags & ZEND_ACC_STRICT_TYPES` over a user function: whether the
    /// file it was declared in is `strict_types`. A native carries no such
    /// bit.
    pub(crate) fn function_strict_types(&self, function: Function) -> bool {
        self.user_function(function).is_some_and(|user| {
            crate::exec::body_of(&user.unit, user.body).is_some_and(|code| code.strict_types)
        })
    }

    /// `zend_verify_prop_assignable_by_ref_ex` under
    /// `ZEND_VERIFY_PROP_ASSIGNABLE_BY_REF_CONTEXT_MAGIC_GET`: what `__get`
    /// answered for a typed property an `unset` emptied, held to the
    /// property's type under `__get`'s own `strict_types`, converted by the
    /// weak rule where it can be, and php's `TypeError` naming `__get`
    /// where not. A name that is no typed declared property the scope
    /// reaches is the answer as it is.
    ///
    /// ```php
    /// class T { public int $i = 1; public int $u;
    ///     function __get($n) { echo "__get $n\n"; return $n === 'i' ? "42" : "x"; }
    ///     function __isset($n) { echo "__isset $n\n"; return true; } }
    /// $t = new T; unset($t->i);
    /// var_dump($t->i, isset($t->u));
    /// unset($t->u);
    /// try { var_dump($t->u); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// __get i
    /// int(42)
    /// bool(false)
    /// __get u
    /// Value of type string returned from T::__get() must be compatible with unset property T::$u of type int
    /// ```
    fn hold_magic_get(
        &mut self,
        class: ClassId,
        name: &[u8],
        getter: Function,
        got: Value,
    ) -> Result<Value, Thrown> {
        let Some(placed) = self
            .declared_slot(class, name)
            .filter(|p| p.typed && self.may_access(p.declaring, p.visibility))
        else {
            return Ok(got);
        };
        let Some(typed) = self.declared_typed_property(class, name) else {
            return Ok(got);
        };
        let strict = self.function_strict_types(getter);
        let held = got.deref();
        let admitted = match crate::ops::property_type_admits(
            self,
            &held,
            &typed.ty,
            placed.declaring,
            strict,
        ) {
            Ok(admitted) => admitted,
            Err(thrown) => {
                self.release(got);
                return Err(thrown);
            }
        };
        match admitted {
            crate::ops::Admitted::As => Ok(got),
            crate::ops::Admitted::Converted(coerced) => {
                self.release(got);
                Ok(coerced)
            }
            crate::ops::Admitted::Refused => {
                let given = crate::ops::type_name(self, &held);
                self.release(got);
                let class = self.class_name(placed.declaring);
                let ty = crate::api::coerce::declared_type_name(&typed.ty);
                let shown = String::from_utf8_lossy(name).into_owned();
                Err(crate::api::raising::throw(
                    self,
                    "TypeError",
                    &format!(
                        "Value of type {given} returned from {class}::__get() must be compatible with unset property {class}::${shown} of type {ty}"
                    ),
                ))
            }
        }
    }

    /// `zend_readonly_property_modification_error` (`Cannot modify
    /// readonly property C::$p`) and
    /// `zend_readonly_property_indirect_modification_error` (`Cannot
    /// indirectly modify ...`), by `operation`, naming the class that
    /// declared the property.
    ///
    /// Refused from the class's own scope as from any other, the unwritten
    /// property included where the write is indirect; `__clone` writes it
    /// once more, never indirectly:
    ///
    /// ```php
    /// class T {
    ///     public readonly int $i; public readonly array $a;
    ///     function __construct() { $this->i = 1; }
    ///     function bump() { try { $this->i++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { $this->i += 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { $this->a[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         try { unset($this->i); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    ///     function __clone() { $this->i = 5;
    ///         try { $this->i = 6; } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    /// }
    /// $t = new T; $t->bump();
    /// var_dump((clone $t)->i);
    /// ```
    /// ```expect
    /// Cannot modify readonly property T::$i
    /// Cannot modify readonly property T::$i
    /// Cannot indirectly modify readonly property T::$a
    /// Cannot unset readonly property T::$i
    /// Cannot modify readonly property T::$i
    /// int(5)
    /// ```
    fn readonly_modification(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        operation: &str,
    ) -> Thrown {
        let class = self.class_name(declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot {operation} readonly property {class}::${shown}"),
        )
    }

    /// php's `Error` for a typed property read before anything wrote it
    /// (`zend_throw_error` under `uninit_error`), naming the class that
    /// declared it, `prop_info->ce`.
    fn uninitialized_property(&mut self, declaring: ClassId, name: &[u8]) -> Thrown {
        let class = self.class_name(declaring);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Typed property {class}::${shown} must not be accessed before initialization"),
        )
    }

    /// `zend_bad_property_access`: php's `Error` for a property the
    /// scope may not reach.
    fn inaccessible_property(
        &mut self,
        class: ClassId,
        visibility: Visibility,
        name: &[u8],
    ) -> Thrown {
        let word = match visibility {
            Visibility::Public => "public",
            Visibility::Protected => "protected",
            Visibility::Private => "private",
        };
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot access {word} property {class}::${shown}"),
        )
    }

    /// `zobj->ce->__get` and its three neighbours: the magic accessor
    /// `lowered` names, which php resolves once at link time and holds
    /// on the class entry as the pointer every handler tests. Each of
    /// the four is public and takes a receiver, which
    /// `zend_check_magic_method_implementation` refuses a class that
    /// declares otherwise, so the scope that reached the property never
    /// bars the call.
    fn magic_accessor(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        self.method_named(class, lowered)
    }

    /// `zobj->ce->__set != NULL`, the test a handler makes before it
    /// places a name: whether the class serves the magic accessor
    /// `lowered` names, which is what makes a placement silent
    /// ([`Request::declared_slot_for_write`]).
    pub(crate) fn serves_accessor(&self, class: ClassId, lowered: &[u8]) -> bool {
        self.magic_accessor(class, lowered).is_some()
    }

    /// `*zend_get_property_guard(zobj, name)`: the word `name` carries
    /// on `object`, zero for a name no accessor is serving.
    fn guard_word(&self, object: PhpObject, name: &[u8]) -> u32 {
        let id = object.heap().id();
        self.guards
            .iter()
            .find(|((held, under), _)| *held == id && under == name)
            .map_or(0, |(_, word)| *word)
    }

    /// `(*guard) |= bit`, the row laid down by the first accessor to
    /// take it.
    fn guard_take(&mut self, object: PhpObject, name: &[u8], bit: u32) {
        let id = object.heap().id();
        if let Some((_, word)) = self
            .guards
            .iter_mut()
            .find(|((held, under), _)| *held == id && under == name)
        {
            *word |= bit;
            return;
        }
        self.guards.push(((id, name.to_vec()), bit));
    }

    /// `(*guard) &= ~bit`, and the row dropped once the word is empty.
    /// php frees the word with the object; a row here is gone before
    /// the accessor that took it returns, so an `spl_object_id()` the
    /// store hands out again names no row of an object that has died.
    fn guard_drop(&mut self, object: PhpObject, name: &[u8], bit: u32) {
        let id = object.heap().id();
        let at = self
            .guards
            .iter()
            .position(|((held, under), _)| *held == id && under == name);
        let Some(at) = at else {
            return;
        };
        let Some((_, word)) = self.guards.get_mut(at) else {
            return;
        };
        *word &= !bit;
        if *word == 0 {
            self.guards.swap_remove(at);
        }
    }

    /// `zend_std_call_getter` and its three neighbours
    /// (`Z/Zend/zend_object_handlers.c:836`): one magic accessor served
    /// on the object with the property's name as its first argument and
    /// `arguments` after it, under `bit` in the property guard, which is
    /// taken before the call and given back after it whatever the call
    /// did. The object is one more owner for the span of the call
    /// (`GC_ADDREF`, `OBJ_RELEASE`), so an accessor that drops the last
    /// other owner does not free the object under the handler.
    fn call_accessor(
        &mut self,
        object: PhpObject,
        accessor: Function,
        bit: u32,
        name: &[u8],
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        self.guard_take(object, name, bit);
        let served = self.serve_accessor(object, accessor, name, arguments);
        self.guard_drop(object, name, bit);
        served
    }

    /// [`Request::call_accessor`] without the guard, for the one site
    /// that holds a bit across two calls: `zend_std_has_property` keeps
    /// `IN_ISSET` standing while it asks the `__get` that `empty` reads
    /// the truth of.
    fn serve_accessor(
        &mut self,
        object: PhpObject,
        accessor: Function,
        name: &[u8],
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let interned = self.interned(name)?;
        let mut sent = Vec::with_capacity(arguments.len().saturating_add(1));
        sent.push(Value::string(interned));
        sent.extend_from_slice(arguments);
        self.addref(&object.as_value());
        let served = self.serve_method(accessor, Some(object), object.class(), &sent, &[]);
        self.release(object.as_value());
        served
    }

    /// `zend_std_read_property`, the handler `FETCH_OBJ_R` and
    /// `FETCH_OBJ_IS` call: `zend_get_property_offset` places the name,
    /// a declared property's slot where the scope may see it, php's
    /// `Error` `Cannot access private property C::$p` where it may not
    /// (`zend_bad_property_access`), and the dynamic table for a name
    /// the class did not declare. A typed slot nothing has written is
    /// php's `Error` for a property read before initialization; an
    /// untyped one that `unset` emptied reads as a dynamic name does,
    /// and a name neither holds warns `Undefined property: C::$p` and
    /// is `null`. What is read is one more owner of the value
    /// (`ZVAL_COPY_DEREF`), a reference followed.
    ///
    /// A class with a `__get` is served between the slot and the
    /// warning, under `IN_GET` in the property guard: the placement is
    /// silent where a `__get` may serve what it refuses
    /// (`zend_get_property_offset(ce, name, (type == BP_VAR_IS) ||
    /// (ce->__get != NULL), ...)`), so a property the scope may not
    /// reach is the wrong-offset marker and reaches the accessor rather
    /// than the `Error`; the uninitialised typed slot is the one arm
    /// that skips `__get`, php's `IS_PROP_UNINIT`; and a `__get` that
    /// reads the name it was called for finds the guard standing and
    /// falls through to the warning, with the `Error` raised again
    /// (`zend_wrong_offset`) where the placement was the one the scope
    /// may not reach.
    ///
    /// Under [`PropertyRead::Silent`], php's `BP_VAR_IS`, each of the
    /// three is `null` instead: `zend_get_property_offset` returns the
    /// wrong-offset marker rather than raising for a property the scope
    /// may not reach, the uninitialized typed slot is `ZVAL_NULL`, and
    /// the warning is the one line php guards with `type != BP_VAR_IS`.
    /// A class with a `__isset` serves it there before the `__get`, and
    /// a falsy answer is the `null` with no `__get` called at all.
    ///
    /// ```php
    /// class M { public function __get($k) { echo "get $k\n"; return 7; } }
    /// $m = new M;
    /// var_dump($m->z);
    /// class P { private $secret = 5; public function __get($k) { echo "get $k\n"; return 9; } }
    /// $p = new P;
    /// var_dump($p->secret);
    /// class T { public int $t; public function __get($k) { echo "get $k\n"; return 1; } }
    /// $t = new T;
    /// try { var_dump($t->t); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// get z
    /// int(7)
    /// get secret
    /// int(9)
    /// Error: Typed property T::$t must not be accessed before initialization
    /// ```
    ///
    /// The guard: a `__get` that reads the name it was called for finds
    /// it standing and warns where the recursion would have been.
    ///
    /// ```php
    /// class R { public function __get($k) { echo "in get $k\n"; return $this->a; } }
    /// $r = new R;
    /// var_dump($r->a);
    /// ```
    /// ```expect
    /// in get a
    ///
    /// Warning: Undefined property: R::$a in %s on line 2
    /// NULL
    /// ```
    ///
    /// A lazy object is initialised where the read finds nothing
    /// (`zend_lazy_object_init` at `uninit_error`): a declared slot that
    /// waits skips `__get` and initialises the object, while a name no
    /// class declared is the accessor's first.
    ///
    /// ```php
    /// class C { public $a = 1; public function __get($n) { echo "get $n\n"; return 0; } }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { echo "init\n"; });
    /// var_dump($o->dyn);
    /// var_dump($r->isUninitializedLazyObject($o));
    /// var_dump($o->a);
    /// ```
    /// ```expect
    /// get dyn
    /// int(0)
    /// bool(true)
    /// init
    /// int(1)
    /// ```
    pub(crate) fn read_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        let silent = read == PropertyRead::Silent;
        // What nothing holds is `null`, quietly, under `BP_VAR_IS` and
        // `BP_VAR_UNSET`; only the first asks `__isset`.
        let quiet = matches!(read, PropertyRead::Silent | PropertyRead::Unset);
        let o = object.heap();
        let class = object.class();
        let shown = String::from_utf8_lossy(name).into_owned();
        if let Some(read) = self.read_hooked_arm(object, name, read) {
            return read;
        }
        // `ZEND_WRONG_PROPERTY_OFFSET`: the placement found a property
        // the scope may not reach and said so quietly, because a
        // `__get` may serve it. php tests one pointer for that; the
        // class is asked for the accessor only where the answer is
        // read, so a slot that holds a value is placed and returned
        // without a lookup.
        let mut wrong = false;
        if let Some(placed) = self.declared_slot(class, name) {
            if self.may_access(placed.declaring, placed.visibility) {
                let z = o.slot(placed.slot);
                if !z.is_undef() {
                    let value = Value::of_zval(z).deref();
                    self.addref(&value);
                    return Ok(value);
                }
                // `IS_PROP_UNINIT | IS_PROP_LAZY`: a slot that waits for
                // the initializer skips `__get` too, and the object is
                // initialised and read instead (`uninit_error`).
                if self.lazy_slot(o, placed.slot) {
                    let instance = self.lazy_init(object)?;
                    return self.read_property(instance, name, read);
                }
                // `IS_PROP_UNINIT`: a typed slot nothing has written
                // skips `__get` and is the uninitialised-access error; one
                // an `unset` emptied is the magic methods' where the class
                // has one that answers, and the same error where not
                // (`uninit_error` with `prop_info` set).
                let magic = self.serves_accessor(class, b"__get")
                    || (silent && self.serves_accessor(class, b"__isset"));
                if placed.typed && (o.slot_uninit(placed.slot) || !magic) {
                    if quiet {
                        return Ok(Value::null());
                    }
                    return Err(self.uninitialized_property(placed.declaring, name));
                }
            } else {
                if !silent && !self.serves_accessor(class, b"__get") {
                    return Err(self.inaccessible_property(class, placed.visibility, name));
                }
                wrong = true;
            }
        } else {
            let dynamic = o
                .properties()
                .into_iter()
                .flat_map(|table| table.iter().collect::<Vec<_>>())
                .find(|(k, _)| matches!(k, php_heap::Key::Str(s) if s.bytes() == name))
                .map(|(_, z)| Value::of_zval(z));
            if let Some(value) = dynamic {
                let value = value.deref();
                self.addref(&value);
                return Ok(value);
            }
        }
        let getter = self.magic_accessor(class, b"__get");
        let issetter = silent
            .then(|| self.magic_accessor(class, b"__isset"))
            .flatten();
        let guard_bit = if issetter.is_some() { IN_ISSET } else { IN_GET };
        // GH-21478: a proxy that forwards, read from inside the
        // instance's own accessor for this name, reads the instance
        // directly rather than serving the accessor twice.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & guard_bit != 0
        {
            return self.read_property(instance, name, read);
        }
        // `/* magic isset */`: under `BP_VAR_IS` a class with a
        // `__isset` is asked first, and the `__get` after it only where
        // it said yes.
        if let Some(issetter) = issetter {
            if self.guard_word(object, name) & IN_ISSET == 0 {
                let answered = self.call_accessor(object, issetter, IN_ISSET, name, &[])?;
                let truthy = crate::api::coerce::truthy(self, &answered);
                self.release(answered);
                if !truthy {
                    return Ok(Value::null());
                }
            }
            if let Some(getter) = getter
                && self.guard_word(object, name) & IN_GET == 0
            {
                let got = self.call_accessor(object, getter, IN_GET, name, &[])?;
                self.overloaded_write_notice(class, name, read, &got);
                return self.hold_magic_get(class, name, getter, got);
            }
        } else if let Some(getter) = getter {
            if self.guard_word(object, name) & IN_GET == 0 {
                let got = self.call_accessor(object, getter, IN_GET, name, &[])?;
                self.overloaded_write_notice(class, name, read, &got);
                return self.hold_magic_get(class, name, getter, got);
            }
            // `zend_wrong_offset`: the guard stands and the placement
            // was one the scope may not reach, so it is made again,
            // loudly, and the `Error` it raises is the answer.
            if wrong && let Some(placed) = self.declared_slot(class, name) {
                return Err(self.inaccessible_property(class, placed.visibility, name));
            }
        }
        // `uninit_error` over a lazy object: a name nothing holds
        // initialises it and is read on what that answers, under the
        // guard the accessor above stood under where one did.
        if o.laziness() != php_heap::Laziness::Eager {
            let instance = self.lazy_init(object)?;
            let guarded = getter.is_some() || issetter.is_some();
            if guarded && self.guard_word(instance, name) & guard_bit == 0 {
                self.guard_take(instance, name, guard_bit);
                let read = self.read_property(instance, name, read);
                self.guard_drop(instance, name, guard_bit);
                return read;
            }
            return self.read_property(instance, name, read);
        }
        if !quiet {
            let class = self.class_name(class);
            crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        }
        Ok(Value::null())
    }

    /// `zend_get_property_offset` at a write site, what `FETCH_OBJ_W`
    /// and `ASSIGN_OBJ` place the name by: the declared slot `name` sits
    /// in and whether it is typed, where the executed scope may reach
    /// it; php's `Error` `Cannot access private property C::$p` where
    /// it may not (`zend_bad_property_access`); and none for a name the
    /// class did not declare, which is a dynamic property. A property
    /// with a set visibility is held to that one as well
    /// ([`Request::has_set_access`]), which is the second check
    /// `zend_std_write_property` and `zend_std_get_property_ptr_ptr`
    /// make; `operation` is the word the site's error carries.
    ///
    /// `silent` is php's own argument to `zend_get_property_offset`: a
    /// site whose class serves the accessor that may take the write
    /// places the name quietly, so a property the scope may not reach
    /// is `None`, the wrong-offset marker, and the handler goes on to
    /// `write_property` or to the overloaded pair, where the `Error` is
    /// raised again if no accessor takes it.
    ///
    /// ```php
    /// class Foo { public private(set) int $bar = 1; public protected(set) int $baz = 2; }
    /// $f = new Foo;
    /// try { $f->bar = 3; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $f->baz = 4; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $f->bar->x = 5; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($f->bar); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($f->bar, $f->baz);
    /// ```
    /// ```expect
    /// Cannot modify private(set) property Foo::$bar from global scope
    /// Cannot modify protected(set) property Foo::$baz from global scope
    /// Cannot indirectly modify private(set) property Foo::$bar from global scope
    /// Cannot unset private(set) property Foo::$bar from global scope
    /// int(1)
    /// int(2)
    /// ```
    ///
    /// Uncaught, the write from global scope is php's `Error`:
    ///
    /// ```php
    /// class Foo { public private(set) int $bar = 1; }
    /// $f = new Foo;
    /// $f->bar = 2;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Cannot modify private(set) property Foo::$bar from global scope in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// A property the class declares with a set visibility and no
    /// default is uninitialised like any other typed one:
    ///
    /// ```php
    /// class Foo { public private(set) int $bar; }
    /// $f = new Foo;
    /// var_dump($f->bar);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Typed property Foo::$bar must not be accessed before initialization in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    pub(crate) fn declared_slot_for_write(
        &mut self,
        object: PhpObject,
        name: &[u8],
        operation: &str,
        silent: bool,
    ) -> Result<Option<(u32, bool)>, Thrown> {
        let class = object.class();
        let Some(placed) = self.declared_slot(class, name) else {
            return Ok(None);
        };
        if !self.may_access(placed.declaring, placed.visibility) {
            if silent {
                return Ok(None);
            }
            return Err(self.inaccessible_property(class, placed.visibility, name));
        }
        // A slot an `unset` emptied, on a class with a `__set`, is the
        // setter's: `zend_std_write_property` reaches the magic method
        // before the set visibility, which `write_std_property` holds the
        // write to after it.
        let o = object.heap();
        if o.slot(placed.slot).is_undef()
            && !o.slot_uninit(placed.slot)
            && !self.lazy_slot(o, placed.slot)
            && self.serves_accessor(class, b"__set")
        {
            return Ok(None);
        }
        // `zend_readonly_property_modification_error` and its indirect
        // twin, ahead of the set visibility: an initialised readonly
        // property is modified from no scope, save once more under
        // `IS_PROP_REINITABLE` inside `__clone`, and an unwritten one is
        // no place for an indirect modification either, which would
        // initialise it by a write the property never sees.
        let initialized = !o.slot(placed.slot).is_undef();
        // `IS_PROP_REINITABLE` lets `__clone` write the property again,
        // never modify it indirectly (`zend_std_read_property` under
        // `BP_VAR_W` does not read the bit).
        let indirect = operation == "indirectly modify";
        if placed.readonly
            && (indirect
                || (initialized && !o.slot_bit(placed.slot, php_heap::PropBit::Reinitable)))
        {
            return Err(self.readonly_modification(placed.declaring, name, operation));
        }
        // An initialised readonly property is refused as readonly, the
        // caller's, ahead of its set visibility
        // (`zend_std_write_property`).
        let readonly_first = placed.readonly && initialized;
        if let Some(set) = placed.set_visibility
            && !readonly_first
            && !self.has_set_access(placed.declaring, set)
        {
            return Err(self.set_inaccessible_property(
                placed.declaring,
                set,
                placed.readonly,
                name,
                operation,
            ));
        }
        Ok(Some((placed.slot, placed.typed)))
    }

    /// `if (UNEXPECTED(Z_TYPE_P(retval) == IS_UNDEF))` in
    /// `zend_std_get_property_ptr_ptr` under `BP_VAR_RW`, ahead of any
    /// placement: a declared slot the scope reaches that nothing holds,
    /// where no `__get` answers for it or it is `IS_PROP_UNINIT`, raises
    /// the read's own diagnostic, the typed property's `Error` naming the
    /// class that declared it or the warning, an untyped one laid to
    /// `null` before the warning as php lays it. Anything else is the
    /// placement's.
    pub(crate) fn read_before_write(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let class = object.class();
        let o = object.heap();
        let Some(placed) = self.declared_slot(class, name) else {
            return Ok(());
        };
        if !self.may_access(placed.declaring, placed.visibility)
            || !o.slot(placed.slot).is_undef()
            || self.lazy_slot(o, placed.slot)
        {
            return Ok(());
        }
        let magic =
            self.serves_accessor(class, b"__get") && self.guard_word(object, name) & IN_GET == 0;
        if magic && !o.slot_uninit(placed.slot) {
            return Ok(());
        }
        if placed.typed {
            return Err(self.uninitialized_property(placed.declaring, name));
        }
        // `ZVAL_NULL(retval)` before the warning, so what an error handler
        // writes into the slot is what the fetch then reads.
        self.write_declared_slot(o, placed.slot, Value::null())?;
        let class = self.class_name(class);
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        Ok(())
    }

    /// Whether a write fetch over `name` reads the property rather than
    /// placing it: a declared slot the scope reaches whose readonly or
    /// set visibility bars this scope from writing it, holding an object.
    /// php's `zend_std_get_property_ptr_ptr` hands such a slot to
    /// `read_property`, which under `BP_VAR_W` returns the object as a
    /// copy (`zend_std_read_property`), so a write through the object
    /// changes the object and never the property.
    ///
    /// Under an `unset` fetch the same slot holding nothing is read too,
    /// as `null` and quietly: there is nothing to modify.
    pub(crate) fn write_fetch_reads_object(
        &self,
        object: PhpObject,
        name: &[u8],
        unset: bool,
    ) -> bool {
        let Some(placed) = self.declared_slot(object.class(), name) else {
            return false;
        };
        let barred = placed.readonly
            || placed
                .set_visibility
                .is_some_and(|set| !self.has_set_access(placed.declaring, set));
        let held = object.heap().slot(placed.slot);
        barred
            && self.may_access(placed.declaring, placed.visibility)
            && (Value::of_zval(held).deref().as_object().is_some() || (unset && held.is_undef()))
    }

    /// The `zend_property_info` beside a typed slot, what
    /// `zend_assign_to_typed_prop` is handed: the declared type, the
    /// class that declared it, and `ZEND_ACC_READONLY`. Read only where
    /// [`Request::declared_slot_for_write`] said the slot is typed, so
    /// an untyped write pays nothing for it; a property php lets carry
    /// `readonly` is typed by php's own refusal, so no untyped write
    /// misses the bit.
    pub(crate) fn declared_typed_property(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypedProperty> {
        let placed = self.placed_properties(class);
        let at = placed.iter().rposition(|(_, p)| &*p.name.0 == name)?;
        let (declaring, p) = placed.get(at)?;
        p.ty.clone().map(|ty| TypedProperty {
            declaring: *declaring,
            ty,
            readonly: p.is_readonly,
        })
    }

    /// [`Request::declared_typed_property`] for a `static` property: the
    /// `zend_property_info` `zend_fetch_static_property_address` hands on,
    /// its type and the class that declared it, where the property is
    /// typed.
    pub(crate) fn declared_typed_static(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<TypedProperty> {
        let (declaring, p) = self.property_info_of(class, name)?;
        if p.is_instance {
            return None;
        }
        p.ty.map(|ty| TypedProperty {
            declaring,
            ty,
            readonly: p.is_readonly,
        })
    }

    /// `zend_verify_property_type` over a value about to land in a typed
    /// property, the step `zend_assign_to_typed_prop`,
    /// `zend_binary_assign_op_typed_prop`, `zend_incdec_typed_prop` and
    /// `zend_std_write_property` share: the value as given where the
    /// declared type accepts it, the weak rule's conversion in its place
    /// where `strict` is off and the rule makes one, and php's
    /// `TypeError` otherwise. `value` is given up on every arm that does
    /// not land it.
    pub(crate) fn hold_to_property_type(
        &mut self,
        typed: &TypedProperty,
        name: &[u8],
        value: Value,
        strict: bool,
    ) -> Result<Value, Thrown> {
        let verified = crate::ops::verify_property_type(
            self,
            &value,
            &typed.ty,
            typed.declaring,
            name,
            strict,
        );
        match verified {
            Ok(None) => Ok(value),
            Ok(Some(coerced)) => {
                self.release(value);
                Ok(coerced)
            }
            Err(thrown) => {
                self.release(value);
                Err(thrown)
            }
        }
    }

    /// `zend_assign_to_variable` over a declared slot, `ASSIGN_OBJ`'s
    /// fast path over a slot that holds a value (`fast_assign_obj`):
    /// `value` is the slot's from here and what it held is released
    /// last, after the write, which is where a destructor is served; a
    /// slot holding a reference is written through the cell
    /// (`Z_ISREF_P(variable_ptr)`), so every holder of the cell reads
    /// the new value.
    pub(crate) fn assign_declared_slot(&mut self, o: Obj, i: u32, value: Value) {
        if let Some(r) = o.slot(i).as_reference() {
            self.reference_set(r, value);
        } else {
            Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
        }
    }

    /// `OBJ_PROP_NUM(obj, i)` written with no name in hand, what a
    /// native that laid the object down does (`add_token`): the slot is
    /// `value`'s from here and what it held is released last, as
    /// [`Heap::object_set_slot`] does. An index past the declared count
    /// is the internal-error fatal naming the class and the index, the
    /// value released, since no slot is there to take it.
    pub(crate) fn write_declared_slot(
        &mut self,
        o: Obj,
        i: u32,
        value: Value,
    ) -> Result<(), Thrown> {
        if i >= o.slots() {
            self.release(value);
            let class = self.class_name(ClassId(o.class()));
            let slots = o.slots();
            return self.fatal_of(Err(Fatal::Error(format!(
                "InternalError: slot {i} written on an object of class {class}, which declares {slots}"
            ))));
        }
        Heap::object_set_slot(&mut Hooks(self), o, i, value.into_zval());
        Ok(())
    }

    /// `zend_std_write_property`, the `Std` slot: the placement, then
    /// the class's `__set` for a name it found nothing at, and
    /// otherwise `write_std_property`
    /// ([`Request::write_std_property`]).
    ///
    /// php places the name with `zend_get_property_offset(ce, name,
    /// ce->__set != NULL, ...)` and reaches the setter where the
    /// placement found no value to write over: a name no class declared
    /// that the dynamic table does not hold, a declared untyped slot an
    /// `unset` emptied, and a declared slot the scope may not reach,
    /// which is the wrong-offset marker a `__set` makes quiet. A
    /// declared typed slot nothing has written is the one arm that
    /// bypasses `__set`, and so is every slot that holds a value. The
    /// setter is served under `IN_SET` in the property guard, so a
    /// `__set` writing the name it was called for lays the property
    /// down instead of reaching itself; where the placement was the one
    /// the scope may not reach, that second pass is the `Error`
    /// (`zend_wrong_offset`) rather than a write.
    ///
    /// `value` is given up here whatever arm runs, as php's caller
    /// gives it up, and so is `name`.
    ///
    /// ```php
    /// class M { public $kept = 1; public function __set($k, $v) { echo "set $k=$v\n"; } }
    /// $m = new M;
    /// $m->y = 3;
    /// $m->kept = 2;
    /// var_dump($m);
    /// class P { private $secret = 5; public function __set($k, $v) { echo "set $k=$v\n"; } }
    /// $p = new P;
    /// $p->secret = 8;
    /// var_dump($p);
    /// ```
    /// ```expect
    /// set y=3
    /// object(M)#1 (1) {
    ///   ["kept"]=>
    ///   int(2)
    /// }
    /// set secret=8
    /// object(P)#2 (1) {
    ///   ["secret":"P":private]=>
    ///   int(5)
    /// }
    /// ```
    pub(crate) fn write_property(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let class = object.class();
        // `if (Z_TYPE_P(variable_ptr) != IS_UNDEF) goto found` and
        // `if (Z_PROP_FLAG_P(variable_ptr) & IS_PROP_UNINIT) goto
        // write_std_property`: a declared slot the scope may reach that
        // holds a value, and a typed one nothing has written, are both
        // the setter's business to stay out of, so neither asks the
        // class for one.
        let o = object.heap();
        let lazy = o.laziness() != php_heap::Laziness::Eager;
        if let Some(hooked) = self.hooked(class, name.as_bytes()) {
            if self.hooked_accessible(&hooked) {
                return self.write_hooked(object, name, value, hooked);
            }
            if !self.serves_accessor(class, b"__set") {
                let thrown = self.inaccessible_property(class, hooked.visibility, name.as_bytes());
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
        }
        let placed = self.declared_slot(class, name.as_bytes());
        if let Some(p) = placed.as_ref()
            && self.may_access(p.declaring, p.visibility)
        {
            if !o.slot(p.slot).is_undef() {
                return self.write_std_property(object, name, value);
            }
            // `IS_PROP_LAZY`: a slot that waits is the initializer's
            // first, then the write's on what it answers.
            if self.lazy_slot(o, p.slot) {
                return self.lazy_write(object, name, value, false);
            }
            // `IS_PROP_UNINIT`: a slot nothing has written passes `__set`
            // by; one an `unset` emptied is the setter's.
            if o.slot_uninit(p.slot) {
                return self.write_std_property(object, name, value);
            }
        }
        let held = placed.is_none()
            && o.properties()
                .is_some_and(|table| table.contains(php_heap::Key::Str(name.heap())));
        if held {
            return self.write_std_property(object, name, value);
        }
        // GH-21478: a proxy that forwards, written from inside the
        // instance's own `__set` for this name, writes the instance.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name.as_bytes()) & IN_SET != 0
        {
            return self.write_property(instance, name, value);
        }
        let Some(setter) = self.magic_accessor(class, b"__set") else {
            if lazy {
                return self.lazy_write(object, name, value, false);
            }
            return self.write_std_property(object, name, value);
        };
        let bytes = name.as_bytes().to_vec();
        let wrong = placed
            .as_ref()
            .is_some_and(|p| !self.may_access(p.declaring, p.visibility));
        if self.guard_word(object, &bytes) & IN_SET == 0 {
            let served = self.call_accessor(object, setter, IN_SET, &bytes, &[value]);
            self.release(value);
            self.release(Value::string(name));
            let answered = served?;
            self.release(answered);
            return Ok(());
        }
        // `zend_wrong_offset`: the guard stands, so the placement is
        // made again and the `Error` it raises for a property the scope
        // may not reach is the answer.
        if wrong && let Some(placed) = self.declared_slot(class, &bytes) {
            let thrown = self.inaccessible_property(class, placed.visibility, &bytes);
            self.release(value);
            self.release(Value::string(name));
            return Err(thrown);
        }
        if lazy {
            return self.lazy_write(object, name, value, true);
        }
        // `write_std_property`'s set-visibility arm, which the write a
        // `__set` makes on its own property, under the guard, meets.
        if let Some(placed) = self.declared_slot(class, &bytes)
            && !placed.readonly
            && let Some(set) = placed.set_visibility
            && !self.has_set_access(placed.declaring, set)
        {
            let thrown =
                self.set_inaccessible_property(placed.declaring, set, false, &bytes, "modify");
            self.release(value);
            self.release(Value::string(name));
            return Err(thrown);
        }
        self.write_std_property(object, name, value)
    }

    /// `forward_write_to_lazy_object`
    /// (`Z/Zend/zend_object_handlers.c:1029`): the object initialised,
    /// and the write made on what that answers, under `IN_SET` there
    /// where the `__set` guard stood on the lazy object. `value` and
    /// `name` are given up whatever arm runs.
    fn lazy_write(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
        guarded: bool,
    ) -> Result<(), Thrown> {
        let instance = match self.lazy_init(object) {
            Ok(instance) => instance,
            Err(thrown) => {
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
        };
        let bytes = name.as_bytes().to_vec();
        if guarded && self.guard_word(instance, &bytes) & IN_SET == 0 {
            self.guard_take(instance, &bytes, IN_SET);
            let written = self.write_property(instance, name, value);
            self.guard_drop(instance, &bytes, IN_SET);
            return written;
        }
        self.write_property(instance, name, value)
    }

    /// `write_std_property`, `zend_std_write_property`'s label and what
    /// a class with no `__set` is: into the declared
    /// slot, what it held released last, or into the dynamic table,
    /// materialised on the first write and separated when a cast shares
    /// it. The name is given up once the table has taken its own owner
    /// of it. A name the table does not hold yet is a creation and goes
    /// through [`Request::dynamic_property_creation`] first, php's two
    /// arms at `write_std_property`'s `else`; the value is given up with
    /// the `Error`, as `FREE_OP_DATA` gives it up.
    ///
    /// A typed slot is `zend_verify_property_type` under
    /// `property_uses_strict_types()` ([`Request::property_strict_types`])
    /// first: the value the declared type accepts, the weak rule's
    /// conversion where the frame the write stands in is not strict,
    /// which a native's own frame never is, and php's `TypeError` with
    /// the value given up where neither holds. The value then lands as
    /// `zend_assign_to_variable_ex` lands it, through the cell where the
    /// slot holds a reference.
    ///
    /// ```php
    /// class Foo { public int $id; public ?float $f = null; }
    /// $db = new PDO('sqlite::memory:');
    /// $db->setAttribute(PDO::ATTR_STRINGIFY_FETCHES, true);
    /// $st = $db->query("SELECT '1' AS id, '2.5' AS f");
    /// var_dump($st->fetchObject(Foo::class));
    /// $st = $db->query("SELECT 'x' AS id");
    /// try { $st->fetchObject(Foo::class); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Foo)#3 (2) {
    ///   ["id"]=>
    ///   int(1)
    ///   ["f"]=>
    ///   float(2.5)
    /// }
    /// Cannot assign string to property Foo::$id of type int
    /// ```
    ///
    /// The weak rule holds while the native writes, though the file
    /// that called it declares `strict_types`, since php's frame there
    /// is the native's own; a reference the slot holds is written
    /// through.
    ///
    /// ```php
    /// declare(strict_types=1);
    /// class Foo { public int $id = 0; }
    /// $db = new PDO('sqlite::memory:');
    /// $st = $db->query("SELECT '7' AS id");
    /// $st->setFetchMode(PDO::FETCH_INTO, $o = new Foo);
    /// $r = &$o->id;
    /// $st->fetch();
    /// var_dump($o->id, $r);
    /// ```
    /// ```expect
    /// int(7)
    /// int(7)
    /// ```
    fn write_std_property(
        &mut self,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let o = object.heap();
        if let Some(placed) = self.declared_slot(object.class(), name.as_bytes()) {
            let value = if placed.typed {
                let Some(typed) = self.declared_typed_property(object.class(), name.as_bytes())
                else {
                    self.release(value);
                    self.release(Value::string(name));
                    return self.fatal_of(Err(Fatal::Error(String::from(
                        "InternalError: write_property placed a typed property whose declared type the class does not carry",
                    ))));
                };
                let strict = self.property_strict_types();
                match self.hold_to_property_type(&typed, name.as_bytes(), value, strict) {
                    Ok(value) => value,
                    Err(thrown) => {
                        self.release(Value::string(name));
                        return Err(thrown);
                    }
                }
            } else {
                value
            };
            self.assign_declared_slot(o, placed.slot, value);
        } else {
            let key = php_heap::Key::Str(name.heap());
            // `zend_array_dup` over a table a clone or a cast shares,
            // before the entry is looked for, so a reference the other
            // holder's copy owns alone is its value here and not a cell
            // both write through.
            if let Some(table) = o.properties().filter(|table| table.refcount() > 1) {
                match self.array_separate(table) {
                    Ok(table) => self.heap.object_set_properties(o, table),
                    Err(thrown) => {
                        self.release(value);
                        self.release(Value::string(name));
                        return Err(thrown);
                    }
                }
            }
            let held = o.properties().is_some_and(|table| table.contains(key));
            // `zend_assign_to_variable` over the entry found: one that
            // holds a reference, what `$o->p = &$x` bound, is written
            // through, so every name of the cell reads the value.
            let bound = o
                .properties()
                .and_then(|table| table.get(key))
                .and_then(|found| Value::of_zval(found).as_php_ref().copied());
            if let Some(cell) = bound {
                self.reference_set(cell.heap(), value);
                self.release(Value::string(name));
                return Ok(());
            }
            if !held
                && let Err(thrown) = self.dynamic_property_creation(object.class(), name.as_bytes())
            {
                self.release(value);
                self.release(Value::string(name));
                return Err(thrown);
            }
            let table = self.heap.object_properties(o);
            let table = self.fatal_of(table)?;
            let table = self.array_separate(table)?;
            self.heap.object_set_properties(o, table);
            self.array_update(table, key, value)?;
        }
        self.release(Value::string(name));
        Ok(())
    }

    /// `process_nested_object_data`'s write of one property
    /// (`Z/ext/standard/var_unserializer.re:554`): `key` as a serialized
    /// object carries it, the name plain for a public property, `\0*\0`
    /// before a protected one's and `\0Class\0` before a private one's.
    /// A key that names a declared property is written into its slot
    /// directly, visibility, `readonly` and `__set` not consulted, as php
    /// writes `Z_OBJPROP`; one whose mangling names another visibility
    /// than the declaration's is placed by `is_property_visibility_changed`,
    /// by the unmangled name among the class's own properties and the
    /// ones it inherits that are not private. Any other key is a dynamic
    /// property under the key as written, created through
    /// [`Request::dynamic_property_creation`]'s two arms where the table
    /// does not hold it yet. `value` and `key` are the object's from here;
    /// what the slot held is released after the write.
    pub(crate) fn unserialize_property(
        &mut self,
        object: PhpObject,
        key: PhpString,
        value: Value,
    ) -> Result<bool, Thrown> {
        let class = object.class();
        let bytes = key.as_bytes().to_vec();
        let (mangled_class, name) = match bytes.strip_prefix(b"\0") {
            Some(rest) => match rest.iter().position(|b| *b == 0) {
                Some(end) => (Some(rest[..end].to_vec()), rest[end + 1..].to_vec()),
                None => (None, bytes.clone()),
            },
            None => (None, bytes.clone()),
        };
        let class_name = self.class_name_bytes(class);
        // `is_property_visibility_changed`'s `ZEND_ACC_VIRTUAL` arm: a
        // key the class's table holds a virtual property under has no
        // slot to take the value, and the whole unserialization fails.
        let reaches = match &mangled_class {
            None => true,
            Some(c) => c.as_slice() == b"*" || c.eq_ignore_ascii_case(&class_name),
        };
        if reaches
            && let Some(hooked) = self.hooked(class, &name)
            && hooked.is_virtual
        {
            let declaring = self.class_name(hooked.declaring);
            crate::api::raising::docref(
                self,
                Level::Warning,
                &format!(
                    "Cannot unserialize value for virtual property {declaring}::${}",
                    String::from_utf8_lossy(&name)
                ),
            );
            self.release(value);
            self.release(Value::string(key));
            return Ok(false);
        }
        let placed = self.placed_properties(class);
        let exact = placed.iter().rposition(|(declaring, p)| {
            if *p.name.0 != *name.as_slice() {
                return false;
            }
            match (&mangled_class, p.visibility) {
                (None, Visibility::Public) => true,
                (Some(c), Visibility::Protected) => c.as_slice() == b"*",
                (Some(c), Visibility::Private) => self.class_name_bytes(*declaring) == c.as_slice(),
                _ => false,
            }
        });
        let changed = || {
            let reachable = match &mangled_class {
                None => true,
                Some(c) => c.as_slice() == b"*" || c.eq_ignore_ascii_case(&class_name),
            };
            if !reachable {
                return None;
            }
            placed.iter().rposition(|(declaring, p)| {
                *p.name.0 == *name.as_slice()
                    && (*declaring == class || p.visibility != Visibility::Private)
            })
        };
        let slot = exact.or_else(changed);
        if let Some(at) = slot.and_then(|at| u32::try_from(at).ok()) {
            let z = value.into_zval();
            Heap::object_set_slot(&mut Hooks(self), object.heap(), at, z);
            self.release(Value::string(key));
            return Ok(true);
        }
        let o = object.heap();
        let table_key = php_heap::Key::Str(key.heap());
        let held = o
            .properties()
            .is_some_and(|table| table.contains(table_key));
        if !held && let Err(thrown) = self.dynamic_property_creation(class, &name) {
            self.release(value);
            self.release(Value::string(key));
            return Err(thrown);
        }
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        let table = self.array_separate(table)?;
        self.heap.object_set_properties(o, table);
        self.array_update(table, table_key, value)?;
        self.release(Value::string(key));
        Ok(true)
    }

    /// The two arms php takes before it lays a name the class did not
    /// declare into the dynamic table, in
    /// `zend_std_write_property` and in `zend_std_get_property_ptr_ptr`
    /// alike (`Z/Zend/zend_object_handlers.c:1288`, `:1522`): a class
    /// with `ZEND_ACC_NO_DYNAMIC_PROPERTIES` is
    /// `zend_forbidden_dynamic_property`'s `Error`, and a class without
    /// `ZEND_ACC_ALLOW_DYNAMIC_PROPERTIES` is
    /// `zend_deprecated_dynamic_property`'s 8.2 deprecation. Only a
    /// creation takes them: a name the table already holds is written
    /// with neither.
    ///
    /// php's raiser holds one more owner of the object across the
    /// deprecation and turns the last release into the same `Error`;
    /// nothing here can drop the object under a diagnostic, since the
    /// handler that reached this holds it.
    pub(crate) fn dynamic_property_creation(
        &mut self,
        class: ClassId,
        name: &[u8],
    ) -> Result<(), Thrown> {
        use php_unit::decl::DynamicProperties;
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        let carries = decl.flags.dynamic_properties;
        if carries == DynamicProperties::Allowed {
            return Ok(());
        }
        let shown = String::from_utf8_lossy(name).into_owned();
        let class_name = self.class_name(class);
        if carries == DynamicProperties::Forbidden {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Cannot create dynamic property {class_name}::${shown}"),
            ));
        }
        crate::api::raising::deprecated(
            self,
            &format!("Creation of dynamic property {class_name}::${shown} is deprecated"),
        );
        Ok(())
    }

    /// `ZEND_FE_RESET_R`'s separation of a shared property table
    /// (`Z/Zend/zend_vm_def.h:6903`): the loop walks the object's own
    /// table, so one a cast shares is duplicated first and the object
    /// takes the copy, php's `zend_array_dup` under
    /// `GC_REFCOUNT(zobj->properties) > 1`. An object that has no table
    /// has nothing to separate and is given none, as php's arm is under
    /// `Z_OBJ_P(array_ptr)->properties` being there at all.
    pub(crate) fn separate_object_properties(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let o = object.heap();
        let Some(table) = o.properties() else {
            return Ok(());
        };
        let table = self.array_separate(table)?;
        self.heap.object_set_properties(o, table);
        Ok(())
    }

    /// The start of a `foreach` over an object whose class has no
    /// `get_iterator`, the object arm `FE_RESET_R` and `FE_RESET_RW`
    /// share (`Z/Zend/zend_vm_def.h:6903-6934`): the property table the
    /// walk goes over made the object's own, separated where a cast
    /// shares it and laid down where the object has none, which is php's
    /// `get_properties` materialising it; then, where the object has a
    /// property at all, declared or dynamic, the hash iterator the walk
    /// is registered at (`zend_hash_iterator_add(properties, 0)`), whose
    /// index is the loop variable's `Z_FE_ITER`. `None` is the empty
    /// table php jumps past the loop over with `Z_FE_ITER` at `-1`.
    pub(crate) fn object_walk_start(&mut self, object: PhpObject) -> Result<Option<u32>, Thrown> {
        // `zend_lazy_object_init`: a lazy object is initialised before
        // the walk, which goes over what that answers.
        let object = self.lazy_init(object)?;
        self.separate_object_properties(object)?;
        let o = object.heap();
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        if self.class_slots(object.class()) == 0 && table.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.heap.iterator_add(table, 0)))
    }

    /// `zend_fe_fetch_object_helper` (`Z/Zend/zend_vm_def.h:7058`) over
    /// an object whose class has no `get_iterator`, and the object arm
    /// of `FE_FETCH_RW`: the next property the executing scope may see
    /// from where iterator `idx` stands, with its key and where it sits,
    /// the iterator left one past it. The order is `Z_OBJPROP`'s: the
    /// class's declared instance properties in declaration order, which
    /// php's table carries as its first buckets and this heap walks by
    /// the iterator's lead ([`php_heap::Heap::iterator_lead`]), then the
    /// dynamic table from the iterator's own position, which every write
    /// to the table moves as php's writes move it. A slot nothing has
    /// written is stepped over as php steps over an `IS_UNDEF`
    /// behind an `IS_INDIRECT`, and one the executing scope may not
    /// reach is stepped over as php steps over a mangled key
    /// `zend_check_property_access` refuses. `None` is nothing left,
    /// which is the fetch's jump past the loop. The `Err` is the memory
    /// limit refusing the intern of a declared name; php's key is
    /// interned already and takes no allocation.
    pub(crate) fn object_walk(
        &mut self,
        object: PhpObject,
        idx: u32,
        by_ref: bool,
    ) -> Result<Option<(php_heap::Key, Walked)>, Thrown> {
        let o = object.heap();
        let class = object.class();
        // `zend_hooked_object_get_iterator`: a class with a hooked
        // property walks its declared rows in the table's order, each
        // hooked one read through its hook.
        if self.class_has_hooks(class) {
            let mut lead = self.heap.iterator_lead(idx) as usize;
            let found = self.hooked_object_walk(object, &mut lead, by_ref);
            self.heap
                .iterator_set_lead(idx, u32::try_from(lead).unwrap_or(u32::MAX));
            if let Some((key, walked)) = found? {
                return Ok(Some((php_heap::Key::Str(key.heap()), walked)));
            }
        } else {
            let declared: Vec<(Box<[u8]>, ClassId, Visibility)> = self
                .placed_properties(class)
                .into_iter()
                .map(|(declaring, p)| (p.name.0, declaring, p.visibility))
                .collect();
            let mut at = self.heap.iterator_lead(idx) as usize;
            while let Some((name, declaring, visibility)) = declared.get(at) {
                let slot = u32::try_from(at).unwrap_or(u32::MAX);
                if !o.slot(slot).is_undef() && self.may_access(*declaring, *visibility) {
                    let interned = self.heap.intern(name);
                    let key = self.fatal_of(interned).map(php_heap::Key::Str)?;
                    self.heap.iterator_set_lead(idx, slot.saturating_add(1));
                    return Ok(Some((key, Walked::Slot(slot))));
                }
                at = at.saturating_add(1);
            }
            self.heap
                .iterator_set_lead(idx, u32::try_from(declared.len()).unwrap_or(u32::MAX));
        }
        let Some(table) = o.properties() else {
            return Ok(None);
        };
        let pos = self.heap.iterator_pos(idx, table);
        let Some((found, key, _)) = table.live_from(pos) else {
            return Ok(None);
        };
        self.heap.iterator_set_pos(idx, found.saturating_add(1));
        Ok(Some((key, Walked::Dynamic(key))))
    }

    /// What [`Request::object_walk`] found at a place: the value, a view
    /// with no owner taken and a reference followed where `deref`.
    pub(crate) fn walked_value(object: PhpObject, walked: Walked) -> Value {
        let o = object.heap();
        let z = match walked {
            Walked::Slot(slot) => o.slot(slot),
            Walked::Dynamic(key) => o
                .properties()
                .and_then(|table| table.get(key))
                .unwrap_or(php_heap::Zval::UNDEF),
            Walked::Hooked(value) => return value,
        };
        Value::of_zval(z)
    }

    /// `FE_FETCH_RW`'s bind over an object's property
    /// (`Z/Zend/zend_vm_def.h:7331-7357`, `:7434-7439`): the cell the
    /// place holds, or one made there over the value it held
    /// (`ZVAL_NEW_EMPTY_REF`), the place keeping the owner it had. A
    /// declared `readonly` slot is php's `Error` and no cell; a typed
    /// one puts its type on the cell it makes (`ZEND_REF_ADD_TYPE_SOURCE`),
    /// so a write through the loop's variable is held to it.
    pub(crate) fn walked_reference(
        &mut self,
        object: PhpObject,
        walked: Walked,
    ) -> Result<PhpRef, Thrown> {
        let o = object.heap();
        let held = Request::walked_value(object, walked);
        if let Some(cell) = held.as_php_ref() {
            return Ok(*cell);
        }
        match walked {
            Walked::Slot(slot) => {
                let placed = self.placed_properties(object.class());
                let Some((declaring, decl)) = placed.get(slot as usize).cloned() else {
                    return self.fatal_of(Err(Fatal::Error(format!(
                        "InternalError: a walk over slot {slot}, which the class does not declare"
                    ))));
                };
                if decl.is_readonly {
                    let class = self.class_name(declaring);
                    let shown = String::from_utf8_lossy(&decl.name.0).into_owned();
                    return Err(crate::api::raising::throw(
                        self,
                        "Error",
                        &format!("Cannot acquire reference to readonly property {class}::${shown}"),
                    ));
                }
                let cell = self.reference(held)?;
                if let Some(place) = o.slot_ref(slot) {
                    place.write(cell.as_value().to_zval());
                }
                if let Some(ty) = decl.ty.clone() {
                    self.ref_add_type_source(
                        cell.heap(),
                        RefSource {
                            declaring,
                            name: decl.name.0.to_vec(),
                            ty,
                        },
                    );
                }
                Ok(cell)
            }
            Walked::Dynamic(key) => {
                let place = o.properties().and_then(|table| table.value_ref(key));
                let cell = self.reference(held)?;
                if let Some(place) = place {
                    place.write(cell.as_value().to_zval());
                }
                Ok(cell)
            }
            // A hook that answered no reference has no place to bind; the
            // walk refused one whose `get` hook returns none before it
            // ran it ([`Request::hooked_object_walk`]).
            Walked::Hooked(value) => {
                self.release(value);
                self.fatal_of(Err(Fatal::Error(String::from(
                    "InternalError: a hooked property's value bound by reference that its hook did not return by reference",
                ))))
            }
        }
    }

    /// `zend_std_get_property_ptr_ptr`'s dynamic arm under `BP_VAR_W`
    /// (`Z/Zend/zend_object_handlers.c:1491`): the dynamic table
    /// separated where a cast shares it and the name looked up in it;
    /// where it is not there, php's two arms over the creation
    /// ([`Request::dynamic_property_creation`]) and then
    /// `zend_hash_add(zobj->properties, name, &EG(uninitialized_zval))`,
    /// which is `IS_NULL` (`zend_execute_API.c:132`) and is what the
    /// `ASSIGN*` beside this writes through, and what a caught `Error`
    /// leaves the property holding. The name is given up here, as
    /// [`Request::write_property`] gives it up.
    ///
    /// The `ZvalRef` names a bucket of that table, so it is a name only
    /// until the table is written again ([`php_heap::Arr::value_ref`]);
    /// the one opline between this and the write through it is the
    /// `ASSIGN*` php pairs it with, which inserts nothing.
    ///
    /// `read` is php's `type`: under [`PropertyRead::Warning`], which is
    /// `BP_VAR_RW` and `BP_VAR_R`, the `Undefined property` warning
    /// stands between the creation's deprecation and the entry
    /// (`Z/Zend/zend_object_handlers.c:1530`), which is the order php
    /// raises the two in and the reverse of the order the pair
    /// [`Request::read_property`] and [`Request::write_property`]
    /// raises them in over the same name. A name the table already
    /// holds raises neither, php returning before both.
    pub(crate) fn dynamic_property_ref(
        &mut self,
        object: PhpObject,
        name: PhpString,
        read: PropertyRead,
    ) -> Result<Option<php_heap::ZvalRef>, Thrown> {
        let o = object.heap();
        let key = php_heap::Key::Str(name.heap());
        let held = o.properties().is_some_and(|table| table.contains(key));
        if !held
            && let Err(thrown) = self.dynamic_property_creation(object.class(), name.as_bytes())
        {
            self.release(Value::string(name));
            return Err(thrown);
        }
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        let table = self.array_separate(table)?;
        self.heap.object_set_properties(o, table);
        if !held {
            if read == PropertyRead::Warning {
                let class = self.class_name(object.class());
                let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
                crate::api::raising::warning(
                    self,
                    &format!("Undefined property: {class}::${shown}"),
                );
            }
            self.array_update(table, key, Value::null())?;
        }
        self.release(Value::string(name));
        Ok(table.value_ref(key))
    }

    /// `EXPECTED(!zobj->ce->__get) || UNEXPECTED((*zend_get_property_guard(zobj,
    /// name)) & IN_GET)` (`Z/Zend/zend_object_handlers.c:1489`) read the
    /// other way round: whether `zend_std_get_property_ptr_ptr` hands
    /// out no place for `name` and leaves the site to `read_property`
    /// and `write_property`, which is a class with a `__get` that the
    /// guard is not already holding.
    pub(crate) fn overloaded_place(&self, object: PhpObject, name: &[u8]) -> bool {
        self.serves_accessor(object.class(), b"__get")
            && self.guard_word(object, name) & IN_GET == 0
    }

    /// The properties in order, the declared slots that hold a value
    /// then the dynamic table, which is the order
    /// `rebuild_object_properties` lays the table out in. Views, and a
    /// declared name is the interned string php's `prop_info->name` is,
    /// never refcounted, so a caller that takes an owner of it takes
    /// none; the `Err` is the memory limit refusing the intern.
    /// The rows `get_properties` answers for `object`, each beside the
    /// declared slot it is, the class that declared what stands there
    /// and its visibility, or none for a dynamic row: the rows of the
    /// std table are the slots that hold a value, in slot order, and
    /// then the dynamic table, so the slots are walked with a cursor
    /// beside them, as `zend_mangle_property_name` is walked by the
    /// `(array)` cast. Views, no owner taken.
    pub(crate) fn property_rows(&mut self, object: PhpObject) -> Result<Vec<PropertyRow>, Thrown> {
        let table = self.object_handlers(object.heap());
        let rows = table.get_properties(self, object)?;
        let placed = self.placed_properties(object.class());
        let mut at = 0;
        Ok(rows
            .into_iter()
            .map(|(name, value)| {
                let slot = placed
                    .iter()
                    .skip(at)
                    .position(|(_, p)| &*p.name.0 == name.as_bytes())
                    .map(|i| i + at);
                let declared = slot.and_then(|i| {
                    at = i + 1;
                    placed.get(i).map(|(class, p)| (*class, p.visibility))
                });
                (name, value, declared)
            })
            .collect())
    }

    pub(crate) fn object_entries(&mut self, o: Obj) -> Result<Vec<(PhpString, Value)>, Thrown> {
        let mut entries = Vec::new();
        let declared: Vec<Box<[u8]>> = self
            .placed_properties(ClassId(o.class()))
            .into_iter()
            .map(|(_, p)| p.name.0)
            .collect();
        for (i, name) in declared.iter().enumerate() {
            let z = o.slot(u32::try_from(i).unwrap_or(u32::MAX));
            if z.is_undef() {
                continue;
            }
            let interned = self.heap.intern(name);
            let name = self.fatal_of(interned).map(PhpString::of)?;
            entries.push((name, Value::of_zval(z)));
        }
        if let Some(table) = o.properties() {
            for (k, z) in &table {
                if let php_heap::Key::Str(s) = k {
                    entries.push((PhpString::of(s), Value::of_zval(z)));
                }
            }
        }
        Ok(entries)
    }

    /// `zend_objects_clone_obj`, the `Std` `clone_obj` slot: a fresh
    /// entry of the same class holding what this one holds, one more
    /// owner each, and the class's `__clone` served on it, the copy held
    /// at one more owner meanwhile and given up with what `__clone`
    /// threw. An object with a native payload is its class's own
    /// `clone_obj` to copy, since the heap cannot copy a payload it only
    /// knows how to drop; `Std` reaching one is the internal-error fatal
    /// naming the class.
    pub(crate) fn clone_members(&mut self, object: PhpObject) -> Result<PhpObject, Thrown> {
        if object.heap().laziness() != php_heap::Laziness::Eager {
            return self.lazy_clone(object);
        }
        let cloned = self.heap.object_clone(object.heap());
        let Some(new) = self.fatal_of(cloned)? else {
            let class = self.class_name(object.class());
            return self.fatal_of(Err(Fatal::Error(format!(
                "InternalError: Std::clone_obj over an object of class {class}, whose native payload its own table must copy"
            ))));
        };
        let new = PhpObject::of(new);
        if let Err(thrown) = self.serve_clone_method(new) {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// The tail of `zend_objects_clone_members`: the class's `__clone`
    /// served on the copy, with every declared slot of it
    /// `IS_PROP_REINITABLE` for the call, so the method may write a
    /// readonly property once more, and none after it, whatever the method
    /// threw. A class with no `__clone` is nothing to serve.
    pub(crate) fn serve_clone_method(&mut self, copy: PhpObject) -> Result<(), Thrown> {
        if self.method_named(copy.class(), b"__clone").is_none() {
            return Ok(());
        }
        Self::mark_reinitable(copy, true);
        let served = crate::api::call::call_method(self, &copy, b"__clone", &[]);
        Self::mark_reinitable(copy, false);
        served.map(|answered| self.release(answered))
    }

    /// `IS_PROP_REINITABLE` set or cleared on every declared slot of
    /// `object`, as `zend_objects_clone_members` and
    /// `zend_objects_clone_obj_with` do around `__clone` and a `clone
    /// with`'s writes.
    fn mark_reinitable(object: PhpObject, on: bool) {
        let o = object.heap();
        for slot in 0..o.slots() {
            o.set_slot_bit(slot, php_heap::PropBit::Reinitable, on);
        }
    }

    /// [`PhpObject::clone_with`]: the members copied beside `payload`,
    /// and `__clone` served on the copy.
    pub(crate) fn clone_members_payload<T: Any + Send>(
        &mut self,
        object: PhpObject,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let cloned = self.heap.object_clone_with(object.heap(), payload);
        let new = PhpObject::of(self.fatal_of(cloned)?);
        if let Err(thrown) = self.serve_clone_method(new) {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// `zend_objects_clone_obj_with`, the `Std` `clone_obj_with` slot:
    /// the class's `clone_obj`, then each of `properties` written on the
    /// copy through the class's `write_property`, a reference with one
    /// owner followed and one with more php's `Error`; the copy is given
    /// up with what a write threw. The scope php writes under as
    /// `fake_scope` is not modelled, so the writes are the executed
    /// scope's.
    pub(crate) fn clone_members_with(
        &mut self,
        object: PhpObject,
        scope: Option<ClassId>,
        properties: PhpArray,
    ) -> Result<PhpObject, Thrown> {
        let _ = scope;
        let table = self.object_handlers(object.heap());
        let Some(clone_obj) = table.clone_obj() else {
            return Err(self.uncloneable(object));
        };
        let new = clone_obj(self, object)?;
        // "Unlock readonly properties once more" for the writes, and lock
        // them again after, whatever a write threw.
        Self::mark_reinitable(new, true);
        let written = self.clone_with_writes(new, table, properties);
        Self::mark_reinitable(new, false);
        if let Err(thrown) = written {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// The writes of [`Request::clone_members_with`] over the copy, each
    /// through the class's `write_property`.
    fn clone_with_writes(
        &mut self,
        new: PhpObject,
        table: &'static dyn crate::api::abi::ObjectHandlers,
        properties: PhpArray,
    ) -> Result<(), Thrown> {
        for (key, value) in properties.snapshot() {
            let value = match value.as_php_ref() {
                Some(r) if r.as_value().refcount() > 1 => {
                    return Err(crate::api::raising::throw(
                        self,
                        "Error",
                        "Cannot assign by reference when cloning with updated properties",
                    ));
                }
                Some(_) => value.deref(),
                None => value,
            };
            let name = key.to_prop_name(self)?;
            self.addref(&value);
            table.write_property(self, new, name, value)?;
        }
        Ok(())
    }

    /// php's `Error` for a class whose `clone_obj` slot is null.
    fn uncloneable(&mut self, object: PhpObject) -> Thrown {
        let class = self.class_name(object.class());
        crate::api::raising::throw(
            self,
            "Error",
            &format!("Trying to clone an uncloneable object of class {class}"),
        )
    }

    /// The body `ZEND_CLONE` and the `clone()` function share
    /// (`Z/Zend/zend_vm_def.h:6010`, `Z/Zend/zend_builtin_functions.c:72`):
    /// a null `clone_obj` is php's `Error` for an uncloneable object,
    /// read before anything else; a `__clone` that is not public is held
    /// to `zend_check_method_accessible` from the executed scope, the
    /// `Error` `zend_bad_method_call`'s; then the class's `clone_obj`,
    /// or its `clone_obj_with` where `with` names properties, php's
    /// `Error` where the class has none. The copy is the caller's, one
    /// owner.
    pub(crate) fn clone_object(
        &mut self,
        object: PhpObject,
        with: Option<PhpArray>,
    ) -> Result<PhpObject, Thrown> {
        let table = self.object_handlers(object.heap());
        let Some(clone_obj) = table.clone_obj() else {
            return Err(self.uncloneable(object));
        };
        if let Some(clone) = self.method_named(object.class(), b"__clone") {
            self.check_method_access(clone, b"__clone")?;
        }
        match with.filter(|with| !with.is_empty()) {
            Some(with) => {
                let Some(clone_obj_with) = table.clone_obj_with() else {
                    let class = self.class_name(object.class());
                    return Err(crate::api::raising::throw(
                        self,
                        "Error",
                        &format!(
                            "Cloning objects of class {class} with updated properties is not supported"
                        ),
                    ));
                };
                let scope = self.executed_scope();
                clone_obj_with(self, object, scope, with)
            }
            None => clone_obj(self, object),
        }
    }

    /// A class's `clone_obj` over an object with a native payload: the
    /// members copied as `clone_members` copies them and `payload`, the
    /// class's copy of the old one, in the new object's slot. What the
    /// copy names of the heap is the caller's to have taken an owner of.
    pub(crate) fn clone_with_payload<T: std::any::Any + Send>(
        &mut self,
        object: PhpObject,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let cloned = self.heap.object_clone_with(object.heap(), payload);
        self.fatal_of(cloned).map(PhpObject::of)
    }

    /// The native payload of `o` as `T`, borrowed for as long as the
    /// request is.
    pub(crate) fn native<T: std::any::Any + Send>(&self, o: Obj) -> Option<&T> {
        self.heap.native(o)
    }

    /// The native payload of `o` as `T`, mutably.
    pub(crate) fn native_mut<T: std::any::Any + Send>(&mut self, o: Obj) -> Option<&mut T> {
        self.heap.native_mut(o)
    }

    /// [`Request::native_mut`], or the `InternalError` of a method served
    /// on an object without the payload. The first read answers whether
    /// it is there, so the fatal is raised with the request free; the
    /// second cannot miss, since nothing runs between the two.
    pub(crate) fn native_mut_or_fatal<T: std::any::Any + Send>(
        &mut self,
        o: Obj,
    ) -> Result<&mut T, Thrown> {
        if self.heap.native::<T>(o).is_none() {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a method served on an object without its payload",
            ));
        }
        self.heap.native_mut(o).ok_or(Thrown::of(None))
    }

    /// The native being served, by identity.
    pub(crate) fn serving_function(&self) -> Option<Function> {
        self.active_native_function()
    }

    /// The default of the parameter `key` names, evaluated earlier in
    /// this request: a view, the request keeping the owner.
    pub(crate) fn default_argument(&self, key: (Function, u32)) -> Option<Value> {
        self.defaults
            .iter()
            .find(|(at, _)| *at == key)
            .map(|(_, value)| *value)
    }

    /// Keep the owner of an evaluated default until the request ends, as
    /// php's `zend_get_default_from_internal_arg_info` evaluates one where
    /// the call left it absent.
    pub(crate) fn keep_default_argument(&mut self, key: (Function, u32), value: Value) {
        self.defaults.push((key, value));
    }

    /// Give up the evaluated defaults, beside the functions' `static`
    /// variables.
    pub(crate) fn free_default_arguments(&mut self) {
        for (_, value) in std::mem::take(&mut self.defaults) {
            self.release(value);
        }
    }

    /// `zend_objects_destroy_object` (`Z/Zend/zend_objects.c:114`): the
    /// class's `__destruct` served on the object, which the store holds
    /// at one owner meanwhile (`zend_objects_store_del` holds the count
    /// there, where php's `GC_ADDREF` and `OBJ_RELEASE` around the call
    /// hold it). A class with none serves nothing, and so does a lazy
    /// object, which is the first test php makes after finding the
    /// destructor (`zend_object_is_lazy`). The visibility check and the
    /// pending-throwable rules are the branches under this one.
    ///
    /// An array gives its elements up in element order
    /// (`zend_array_destroy` over `i_zval_ptr_dtor`), so the
    /// destructors of the objects it held are served in that order, and
    /// the array is gone before the statement after it runs.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = [new A(1), new A(2)];
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect
    /// d1
    /// d2
    /// after
    /// ```
    ///
    /// A destructor that takes an owner of its object resurrects it:
    /// the release that served it finds a count above zero and frees
    /// nothing, and the object is never served again, because
    /// `IS_OBJ_DESTRUCTOR_CALLED` stands on it for the rest of the
    /// request.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; $this->self = $this; }
    /// }
    /// $a = new A(1);
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect
    /// d1
    /// after
    /// ```
    pub(crate) fn destruct(&mut self, object: PhpObject) {
        if self.marked_destructed {
            return;
        }
        let class = object.class();
        let Some(destructor) = self.method_named(class, b"__destruct") else {
            return;
        };
        // `zend_object_is_lazy`: a lazy object's destructor is not
        // served, nor a forwarding proxy's, whose instance serves its own.
        if object.heap().laziness() != php_heap::Laziness::Eager {
            return;
        }
        if !self.destructor_reachable(object, destructor) {
            return;
        }
        let saved = self.save_pending(object);
        if matches!(saved, Saved::TheObject) {
            return;
        }
        if let Err(thrown) = self.serve_method(destructor, Some(object), class, &[], &[]) {
            self.leave_pending(&thrown);
            if !self.executing() {
                self.end_the_walk();
                return;
            }
        }
        self.restore_pending(saved);
    }

    /// `zend_call_function`'s tail (`Z/Zend/zend_execute_API.c:1069-1076`)
    /// where a destructor left a throwable pending and nothing is
    /// executing: php calls `zend_throw_exception_internal(NULL)`, which
    /// renders the uncaught fatal there and `zend_bailout`s
    /// (`Z/Zend/zend_exceptions.c:203-222`); `shutdown_destructors`
    /// catches that `zend_bailout` and calls
    /// `zend_objects_store_mark_destructed`, so neither walk serves
    /// another destructor. The fatal is rendered here rather than left
    /// for the end of the walk, which is what keeps it one throwable
    /// with no `Next` (design §5.10).
    ///
    /// The rule is `zend_call_function`'s and not the walk's: a raise
    /// inside a serving has a frame to land in and is the pending
    /// throwable of that frame, which is the branch above this one.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $b->self = $b;
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    /// d1
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:6
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 6
    /// ```
    ///
    /// The symbol table's walk ends the same way, and it runs first, so
    /// a raise there leaves the store's walk nothing to serve: `$b` is
    /// deleted before `$a`, as `zend_hash_reverse_apply` takes them, and
    /// `$a`'s destructor is never served.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = new A(1);
    /// $b = new A(2);
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    /// d2
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    fn end_the_walk(&mut self) {
        self.marked_destructed = true;
        if let Err(fatal) = self.uncaught() {
            self.end_in(&fatal);
        }
    }

    /// The first half of `zend_objects_destroy_object`'s protection of
    /// a destructor from a throwable raised before it
    /// (`Z/Zend/zend_objects.c:148-168`): the pending slot is emptied
    /// for the call, and what it held is handed back for
    /// [`Request::restore_pending`]. A destructor served on the pending
    /// throwable itself is php's one `E_CORE_ERROR`, and the `None`
    /// here is that: the request is ending and nothing is served.
    ///
    /// No PHP snippet reaches that fatal, in this engine or in php,
    /// whose own corpus has no test naming it (harness §8). It would
    /// take the pending throwable's last owner released while the slot
    /// still holds it, and php releases one in that state in two places
    /// only: `zend_clear_exception` empties the slot before
    /// `OBJ_RELEASE`, under php's own comment that the throwable may
    /// have a destructor (`Z/Zend/zend_exceptions.c:251-254`), and
    /// `zend_user_exception_handler` releases `EG(exception)` before
    /// emptying it (`Z/Zend/zend.c:1949-1953`) but is reached only with
    /// `EG(current_execute_data)` null, where `zend_call_function`'s
    /// tail renders the handler's own throwable uncaught and
    /// `zend_bailout`s first. `zend_exception_set_previous`, called from
    /// `zend_throw_exception_internal` before the slot is reassigned
    /// (`Z/Zend/zend_exceptions.c:201-202`), gives up the slot's object
    /// only where a `previous` chain or the throw itself holds it a
    /// second time. This engine's slot is the one owner of what stands
    /// in it and nothing releases that while it stands, so the state is
    /// unreachable here for its own reason as well. The unit test beside
    /// this builds what the release cannot, and #458 is the question
    /// that leaves for a person.
    fn save_pending(&mut self, object: PhpObject) -> Saved {
        match self.pending {
            Some(pending) if pending.index() == object.index() => {
                self.end_in_core_error("Attempt to destruct pending exception");
                Saved::TheObject
            }
            _ => match self.pending.take() {
                Some(pending) => Saved::Throwable(pending),
                None if self.unwind_exit => {
                    self.unwind_exit = false;
                    Saved::UnwindExit
                }
                None => Saved::Nothing,
            },
        }
    }

    /// The second half: the throwable the destructor was protected from
    /// is pending again, and one the destructor itself raised keeps the
    /// slot with the old one chained onto it as its `previous`
    /// (`zend_exception_set_previous`), which is what the rendering
    /// prints under `Next`.
    ///
    /// The array here gives its two elements up in order, and the first
    /// destructor's raise is pending while the second is served: it is
    /// saved, the second destructor writes its own line and raises in
    /// its turn, and the two are one uncaught throwable with the first
    /// under `Next`.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = [new A(1), new A(2)];
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect exit=255
    /// d1
    /// d2
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 %s(8): A->__destruct()
    /// #1 {main}
    ///
    /// Next Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 %s(8): A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    fn restore_pending(&mut self, saved: Saved) {
        match saved {
            Saved::Throwable(old) => match self.pending {
                Some(new) => self.set_previous(new, old),
                None => self.pending = Some(old),
            },
            // `zend_exception_set_previous(EG(exception), old_exception)`
            // with the unwind exit as the old one releases it
            // (`Z/Zend/zend_exceptions.c:113-116`): what the destructor
            // raised is what is unwound from here, and the `exit`'s
            // status stands only until that throwable is rendered.
            Saved::UnwindExit => {
                if self.pending.is_none() {
                    self.unwind_exit = true;
                }
            }
            Saved::Nothing | Saved::TheObject => {}
        }
    }

    /// `zend_exception_set_previous` (`Z/Zend/zend_exceptions.c:103`):
    /// `previous` is chained onto the end of the chain `throwable`
    /// already carries, its one owner the property's from here. A
    /// throwable that is already in that chain, or is `throwable`
    /// itself, takes its owner back instead, since php refuses to make
    /// a cycle. A chain is a path through live objects, so the store's
    /// count is a length no walk of it passes.
    pub(crate) fn set_previous(&mut self, throwable: PhpObject, previous: PhpObject) {
        let mut at = throwable;
        for _ in 0..=self.heap.objects() {
            if at.index() == previous.index() {
                break;
            }
            if let Some(next) = self.previous_of(at) {
                at = next;
                continue;
            }
            let Ok(name) = PhpString::new(self, b"previous") else {
                return;
            };
            if at.set_prop(self, name, previous.as_value()).is_ok() {
                return;
            }
            break;
        }
        self.release(previous.as_value());
    }

    /// The `previous` property of a throwable, when it holds one.
    fn previous_of(&self, throwable: PhpObject) -> Option<PhpObject> {
        throwable
            .prop(self, b"previous")
            .and_then(|value| value.as_object().copied())
    }

    /// `zend_objects_destroy_object`'s visibility branch: a public
    /// `__destruct` is served from anywhere, and a private or a
    /// protected one is held to `zend_check_method_accessible` from the
    /// executed scope while a frame stands, php's `Error` naming the
    /// visibility, the object's class and the scope the release came
    /// from. With no frame standing, which is the store's shutdown walk
    /// ([`Request::call_destructors`]) and php's `EG(current_execute_data)`
    /// being null, there is no scope to check and the destructor is
    /// skipped with a warning instead.
    ///
    /// ```php
    /// class A { private function __destruct() { echo "never\n"; } }
    /// $a = new A();
    /// $a = 1;
    /// echo "after\n";
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to private A::__destruct() from global scope in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// ```php
    /// class A { public $self; private function __destruct() { echo "never\n"; } }
    /// $a = new A();
    /// $a->self = $a;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    ///
    /// Warning: Call to private A::__destruct() from global scope during shutdown ignored in Unknown on line 0
    /// ```
    fn destructor_reachable(&mut self, object: PhpObject, destructor: Function) -> bool {
        let visibility = self.method_visibility(destructor);
        if visibility == Visibility::Public {
            return true;
        }
        let shown = visibility_string(visibility);
        let class = self.class_name(object.class());
        if !self.executing() {
            crate::api::raising::warning(
                self,
                &format!(
                    "Call to {shown} {class}::__destruct() from global scope during shutdown ignored"
                ),
            );
            return false;
        }
        if self.method_accessible(destructor) {
            return true;
        }
        let from = self.scope_phrase();
        let thrown = crate::api::raising::throw(
            self,
            "Error",
            &format!("Call to {shown} {class}::__destruct() from {from}"),
        );
        self.leave_pending(&thrown);
        false
    }

    /// `EG(current_execute_data)`: whether a frame or a native's call
    /// stands, which is every release inside a serving and none of the
    /// releases `php_request_shutdown` makes after the main frame popped.
    pub(crate) fn executing(&self) -> bool {
        self.fibers.active().is_some()
            || self.vm.innermost().is_some()
            || self.vm.serving_call().is_some()
    }

    /// `zend_attach_symbol_table` (`Z/Zend/zend_execute_API.c:1857`) on
    /// the table `frame` names: every CV of the frame's op array, in the
    /// order the compiler numbered them, takes what the table holds
    /// under its name, moved out of the bucket or out of the CV of the
    /// frame the bucket points at (`ZVAL_COPY_VALUE`, no owner taken),
    /// and the bucket points at this frame's CV from here; a name the
    /// table has not is added, its CV left `UNDEF`. What the main op
    /// array's activation does with `EG(symbol_table)`, what a code
    /// frame's does with its includer's, and what the includer does
    /// again when the code frame leaves, so a variable the included
    /// file wrote is the includer's.
    pub(crate) fn attach_symbol_table(&mut self, frame: FrameRef) {
        let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) else {
            return;
        };
        for (at, name) in self.frame_var_names(frame).into_iter().enumerate() {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            let found = self.table_mut(table).and_then(|entries| {
                let index = entries.iter().position(|(key, _)| *key == name)?;
                Some(std::mem::replace(
                    &mut entries[index].1,
                    Symbol::Indirect(frame, at),
                ))
            });
            let value = match found {
                Some(Symbol::Value(value)) => value,
                Some(Symbol::Held(cell)) => self.unhold(cell),
                Some(Symbol::Indirect(other, slot)) => self.take_slot(other, slot),
                None => {
                    if let Some(entries) = self.table_mut(table) {
                        entries.push((name, Symbol::Indirect(frame, at)));
                    }
                    continue;
                }
            };
            // The CV was `UNDEF` when the frame was pushed, or is its
            // own value taken back, so the slot gives up nothing.
            let previous = self.set_slot(frame, at, value);
            self.release(previous);
        }
    }

    /// `zend_detach_symbol_table` (`Z/Zend/zend_execute_API.c:1892`),
    /// what php does when it leaves a frame that has a symbol table, the
    /// main op array's frame, the one it marks `ZEND_CALL_TOP_CODE` and
    /// gives `EG(symbol_table)` as its own, and every code frame an
    /// `INCLUDE_OR_EVAL` pushed (design §5.6): every CV that holds
    /// something is moved into the table under the name the compiler
    /// numbered it by, the bucket keeping its place, the slot left
    /// `UNDEF` so the pop releases nothing, and a name whose CV holds
    /// nothing is deleted from the table. The variables are the
    /// table's from here: the request's for the main frame, which is
    /// why `$a = new A;` is still an owner while the shutdown
    /// functions are served and the destructor waits for
    /// [`Request::shutdown_destructors`], and the includer's for a code
    /// frame, whose re-attach takes them back.
    pub(crate) fn detach_symbol_table(&mut self, frame: FrameRef) {
        let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) else {
            return;
        };
        for (at, name) in self.frame_var_names(frame).into_iter().enumerate() {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            if value.tag() == TypeTag::Undef {
                self.symbol_del(table, &name);
            } else {
                self.symbol_update(table, name, value);
            }
        }
    }

    /// `zend_rebuild_symbol_table` (`Z/Zend/zend_execute_API.c:1810`) for
    /// `frame`: the table it already has, or a new one of its own with
    /// every CV of its op array appended as `IS_INDIRECT` to the CV
    /// (`_zend_hash_append_ind`), the values staying where they are, and
    /// the frame marked `ZEND_CALL_HAS_SYMBOL_TABLE`. What a function
    /// frame gets when it includes a file, so the included code reads
    /// and writes the function's variables; freed when the frame pops
    /// (`zend_clean_and_cache_symbol_table`).
    pub(crate) fn rebuild_symbol_table(&mut self, frame: FrameRef) -> TableId {
        if let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) {
            return table;
        }
        let entries = self
            .frame_var_names(frame)
            .into_iter()
            .enumerate()
            .map(|(at, name)| {
                (
                    name,
                    Symbol::Indirect(frame, u32::try_from(at).unwrap_or(u32::MAX)),
                )
            })
            .collect();
        let table = TableId(u32::try_from(self.tables.len()).unwrap_or(u32::MAX));
        self.tables.push(SymbolTable {
            owner: Some(frame),
            entries,
        });
        if let Some(state) = self.vm.at_mut(frame) {
            state.symbol_table = Some(table);
        }
        table
    }

    /// The names of the CVs of the op array `frame` serves, in the
    /// order the compiler numbered them, `op_array->vars`.
    fn frame_var_names(&self, frame: FrameRef) -> Vec<Vec<u8>> {
        self.vm
            .at(frame)
            .and_then(|state| exec::body_of(&state.unit, state.body))
            .map(|code| code.vars.iter().map(|name| name.0.to_vec()).collect())
            .unwrap_or_default()
    }

    fn table_mut(&mut self, table: TableId) -> Option<&mut Vec<(Vec<u8>, Symbol)>> {
        self.tables
            .get_mut(table.0 as usize)
            .map(|table| &mut table.entries)
    }

    /// `zend_clean_and_cache_symbol_table` when the frame that rebuilt
    /// the last table pops: its values released and the table gone. A
    /// bucket still pointing at a CV points at a slot the pop is giving
    /// up, and owns nothing.
    fn free_rebuilt_table(&mut self, frame: FrameRef) {
        if self
            .tables
            .last()
            .is_none_or(|table| table.owner != Some(frame))
        {
            return;
        }
        let Some(table) = self.tables.pop() else {
            return;
        };
        for (_, symbol) in table.entries {
            self.release_symbol(symbol);
        }
    }

    /// `ZEND_BIND_GLOBAL`'s half over `EG(symbol_table)`
    /// (`Z/Zend/zend_vm_def.h:8679`): the bucket `name` has, laid down
    /// holding `null` where the table has none (`zend_hash_lookup`); an
    /// `IS_INDIRECT` bucket followed to the CV of the frame attached to
    /// it, an `UNDEF` there taken as `null`; what stands there made a
    /// reference in place where it is none (`ZVAL_MAKE_REF_EX`); and the
    /// cell handed back with one more owner, the binding's.
    pub(crate) fn global_cell(&mut self, name: &[u8]) -> Result<PhpRef, Thrown> {
        let found = self.globals().iter().position(|(key, _)| key == name);
        let at = found.unwrap_or_else(|| {
            self.globals()
                .push((name.to_vec(), Symbol::Value(Value::null())));
            self.globals().len() - 1
        });
        let cell = match self.globals()[at].1 {
            Symbol::Held(held) => {
                let place = held.heap().val_ref();
                let value = Value::of_zval(place.read());
                if let Some(cell) = value.as_php_ref() {
                    *cell
                } else {
                    let cell = self.reference(value.undef_as_null())?;
                    place.write(cell.as_value().to_zval());
                    cell
                }
            }
            Symbol::Indirect(frame, slot) => {
                let Some(place) = self.slot_ptr(frame, slot) else {
                    return Err(crate::api::raising::fatal(
                        self,
                        "InternalError: a global bound to a CV the frame does not have",
                    ));
                };
                let held = Value::of_zval(place.read());
                if let Some(cell) = held.as_php_ref() {
                    *cell
                } else {
                    let cell = self.reference(held.undef_as_null())?;
                    // The CV's owner of what it held is the cell's now.
                    place.write(cell.as_value().to_zval());
                    cell
                }
            }
            Symbol::Value(held) => {
                if let Some(cell) = held.as_php_ref() {
                    *cell
                } else {
                    let cell = self.reference(held.undef_as_null())?;
                    self.globals()[at].1 = Symbol::Value(cell.as_value());
                    cell
                }
            }
        };
        self.addref(&cell.as_value());
        Ok(cell)
    }

    /// One symbol written, `zend_hash_update`: `value` is the table's
    /// and what stood under the name is released after the write, the
    /// name keeping the position it had, as php's update keeps a
    /// bucket's and only a name the table has not appends. A bucket
    /// that pointed at a CV owned nothing.
    fn symbol_update(&mut self, table: TableId, name: Vec<u8>, value: Value) {
        let Some(entries) = self.table_mut(table) else {
            return;
        };
        if let Some(at) = entries.iter().position(|(key, _)| *key == name) {
            let old = std::mem::replace(&mut entries[at].1, Symbol::Value(value));
            self.release_symbol(old);
            return;
        }
        entries.push((name, Symbol::Value(value)));
    }

    /// One symbol deleted, `zend_hash_del`: the name gone and what it
    /// held released; a name the table has not is no error, as php's
    /// delete of a missing key is none.
    fn symbol_del(&mut self, table: TableId, name: &[u8]) {
        let Some(entries) = self.table_mut(table) else {
            return;
        };
        let Some(at) = entries.iter().position(|(key, _)| key == name) else {
            return;
        };
        let (_, symbol) = entries.remove(at);
        self.release_symbol(symbol);
    }

    /// `zend_unwrap_reference`: a reference a call handed back, which a
    /// method declared `&__toString` or `&__debugInfo` does, given up for
    /// the value it holds, one owner of that the caller's; any other
    /// value as it stands.
    ///
    /// ```php
    /// class S { private $s = 'x'; function &__toString(): string { return $this->s; } }
    /// echo new S, "\n", (string) new S, "\n";
    /// class D { private $d = ['k' => 1]; function &__debugInfo(): array { return $this->d; } }
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// x
    /// x
    /// object(D)#1 (1) {
    ///   ["k"]=>
    ///   int(1)
    /// }
    /// ```
    pub(crate) fn unwrap_reference(&mut self, value: Value) -> Value {
        let Some(cell) = value.as_php_ref() else {
            return value;
        };
        let held = cell.get();
        self.addref(&held);
        self.release(value);
        held
    }

    /// `zend_hash_del_ind` (`Z/Zend/zend_hash.c:1572`), what `UNSET_VAR`
    /// deletes a name with: a bucket attached to a CV stays and the CV is
    /// left `UNDEF`, what it held released after it is gone; any other
    /// bucket is deleted as [`Request::symbol_del`] deletes it.
    pub(crate) fn symbol_unset(&mut self, table: TableId, name: &[u8]) {
        let found = self
            .table_mut(table)
            .and_then(|entries| entries.iter().find(|(key, _)| key == name))
            .map(|(_, symbol)| *symbol);
        match found {
            Some(Symbol::Indirect(frame, slot)) => {
                let value = self.take_slot(frame, slot);
                self.release(value);
            }
            Some(_) => self.symbol_del(table, name),
            None => {}
        }
    }

    /// What a bucket owns given up: its value, or the cell a fetch by
    /// name moved it into with the value in it; an attached bucket owns
    /// nothing.
    fn release_symbol(&mut self, symbol: Symbol) {
        match symbol {
            Symbol::Value(value) => self.release(value),
            Symbol::Held(cell) => self.release(cell.as_value()),
            Symbol::Indirect(..) => {}
        }
    }

    /// The value a [`Symbol::Held`] cell holds taken out of it, the owner
    /// the cell had the caller's, and the cell given up.
    fn unhold(&mut self, cell: PhpRef) -> Value {
        let place = cell.heap().val_ref();
        let value = Value::of_zval(place.read());
        place.write(Value::undef().to_zval());
        self.release(cell.as_value());
        value
    }

    /// `EG(symbol_table)`, laid down where nothing has written it yet.
    pub(crate) fn global_table(&mut self) -> TableId {
        let _ = self.globals();
        TableId::GLOBAL
    }
    /// `zend_hash_find` in a symbol table for a fetch by name that
    /// reads: what the bucket `name` holds, an `IS_INDIRECT` bucket
    /// followed to the CV of the frame attached to it; `None` where the
    /// table has no such name. A view: no owner taken.
    pub(crate) fn symbol_value(&mut self, table: TableId, name: &[u8]) -> Option<Value> {
        let symbol = self
            .table_mut(table)?
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, symbol)| *symbol)?;
        Some(match symbol {
            Symbol::Value(value) => value,
            Symbol::Held(cell) => cell.get(),
            Symbol::Indirect(frame, at) => self.slot(frame, at),
        })
    }

    /// `zend_hash_find` in a symbol table for a fetch by name that
    /// writes, php's `zval *` into the table that the `VAR` holds as an
    /// `IS_INDIRECT`: an attached name is its CV's slot, and a bucket
    /// that holds its value in the table is moved into a place of its
    /// own first ([`Symbol::Held`]), where php's bucket already is one;
    /// with `add`, a name the table has not is laid down holding `null`
    /// (`zend_hash_add_new`). `None` where the name is not there and
    /// `add` is not asked.
    pub(crate) fn symbol_place(
        &mut self,
        table: TableId,
        name: &[u8],
        add: bool,
    ) -> Result<Option<php_heap::ZvalRef>, Thrown> {
        let Some(entries) = self.table_mut(table) else {
            return Ok(None);
        };
        let at = match entries.iter().position(|(key, _)| key == name) {
            Some(at) => at,
            None if add => {
                entries.push((name.to_vec(), Symbol::Value(Value::null())));
                entries.len() - 1
            }
            None => return Ok(None),
        };
        let symbol = entries[at].1;
        let cell = match symbol {
            Symbol::Indirect(frame, slot) => return Ok(self.slot_ptr(frame, slot)),
            Symbol::Held(cell) => cell,
            Symbol::Value(value) => {
                let cell = self.reference(value)?;
                if let Some(entries) = self.table_mut(table) {
                    entries[at].1 = Symbol::Held(cell);
                }
                cell
            }
        };
        Ok(Some(cell.heap().val_ref()))
    }

    /// `shutdown_destructors` (`Z/Zend/zend_execute_API.c:251`), the
    /// first half of `zend_call_destructors` and so the first
    /// destructor php serves at shutdown (design §5.10): the symbol
    /// table walked in reverse, `zend_hash_reverse_apply` over
    /// `zval_call_destructor`, each symbol that holds an object nothing
    /// else owns deleted, which serves that object's destructor; the
    /// walk repeated while the count of symbols moves, since one
    /// destructor's work can drop another symbol's last other owner.
    /// [`Request::call_destructors`], the store's walk, runs after it
    /// and takes what is left.
    ///
    /// So two top-level variables holding an object each are served in
    /// reverse declaration order, the order the CVs were numbered read
    /// backwards, and not in the order the store made them.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $b = new A(2);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d2
    /// d1
    /// ```
    ///
    /// A global whose object has an owner beside the symbol is kept by
    /// the walk and served by the store's after it, in store order, so
    /// the two orders are visible in one program: `$b`, owned by its
    /// symbol alone, is served first and backwards; `$a` and `$c`, each
    /// holding itself, are served after it and forwards.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $c = new A(3);
    /// $c->self = $c;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d2
    /// d1
    /// d3
    /// ```
    pub(crate) fn shutdown_destructors(&mut self) {
        loop {
            let symbols = self.globals().len();
            let mut at = symbols;
            while at > 0 && !self.marked_destructed {
                at -= 1;
                let alone = self.globals().get(at).is_some_and(|(_, symbol)| {
                    let value = match symbol {
                        Symbol::Value(value) => *value,
                        Symbol::Held(cell) => cell.get(),
                        Symbol::Indirect(..) => return false,
                    };
                    value.as_object().is_some() && value.refcount() == 1
                });
                if alone {
                    let (_, symbol) = self.globals().remove(at);
                    self.release_symbol(symbol);
                }
            }
            if symbols == self.globals().len() || self.marked_destructed {
                break;
            }
        }
    }

    /// `EG(symbol_table)`'s buckets, the first table's.
    fn globals(&mut self) -> &mut Vec<(Vec<u8>, Symbol)> {
        if self.tables.is_empty() {
            self.tables.push(SymbolTable::default());
        }
        &mut self.tables[0].entries
    }

    /// `zend_hash_graceful_reverse_destroy(&EG(symbol_table))` in
    /// `zend_shutdown_executor_values`
    /// (`Z/Zend/zend_execute_API.c:271`), after the open resources are
    /// closed and before the object storage is freed: every symbol left
    /// released from the last to the first. The destructors are behind
    /// the request by now, `zend_objects_store_call_destructors` having
    /// marked every object, so this frees and serves nothing.
    pub(crate) fn destroy_symbol_table(&mut self) {
        while let Some((_, symbol)) = self.globals().pop() {
            self.release_symbol(symbol);
        }
    }

    /// `zend_objects_store_call_destructors`, the second half of
    /// `zend_call_destructors` (design §5.10): every live object's
    /// destructor not yet served, in store order, until one leaves a
    /// throwable pending, which [`Request::end_the_walk`] renders where
    /// php renders it and ends both walks with. What the symbol table
    /// owned alone was served before this by
    /// [`Request::shutdown_destructors`]. The store's walk still marks
    /// every object it passes, as php's `zend_objects_store_mark_destructed`
    /// marks them behind the `zend_bailout`, so nothing is served after
    /// it either.
    ///
    /// An object a variable no longer names alone outlives that walk,
    /// so the store's is what serves it: each of these holds itself,
    /// so their destructors are served after the last statement, and in
    /// the order the store made them, which is not the order the
    /// variables were written.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $b->self = $b;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d1
    /// d2
    /// ```
    pub(crate) fn call_destructors(&mut self) {
        Heap::call_destructors(&mut Hooks(self));
        if let Err(fatal) = self.uncaught() {
            self.end_in(&fatal);
        }
    }

    /// `zend_objects_store_free_object_storage`, in `shutdown_executor`
    /// after the extensions' `rshutdown`: every live object's members
    /// and payload freed once, the bytes left for the mass release.
    pub(crate) fn free_object_storage(&mut self) {
        Heap::free_object_storage(&mut Hooks(self));
    }

    /// The handler table an object of `class` is reached through
    /// (design §5.4): the class's own where it declared one, else the
    /// nearest ancestor's, as php inherits `create_object` and the table
    /// it installs, and `Std`'s at a root and for every user class. The
    /// walk is cut at a depth no declaration can make.
    pub(crate) fn handlers_of(&self, class: ClassId) -> &'static dyn ObjectHandlers {
        let mut at = Some(class);
        for _ in 0..64 {
            let Some(class) = at else {
                break;
            };
            if let Some(Some(table)) = usize::try_from(class.0)
                .ok()
                .and_then(|i| self.shared.handlers.get(i))
            {
                return *table;
            }
            at = self.class_parent(class);
        }
        if self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Enum)
        {
            return &crate::enums::ENUM;
        }
        &Std
    }

    /// The `create_object` of the class or of the nearest parent that
    /// declares one (design §5.4), php's inherited `ce->create_object`;
    /// `None` for a class whose objects carry no native payload, which
    /// is `zend_objects_new`.
    pub(crate) fn native_new_of(&self, class: ClassId) -> Option<NativeNew> {
        let mut at = Some(class);
        for _ in 0..64 {
            let class = at?;
            if let Some(Some(new)) = usize::try_from(class.0)
                .ok()
                .and_then(|i| self.shared.natives.get(i))
            {
                return Some(*new);
            }
            at = self.class_parent(class);
        }
        None
    }

    /// [`Request::handlers_of`] for the object's class, php's
    /// `zobj->handlers`.
    pub(crate) fn object_handlers(&self, o: Obj) -> &'static dyn ObjectHandlers {
        self.handlers_of(ClassId(o.class()))
    }

    /// `zend_std_has_property`: the declared slot where the scope may
    /// reach it and it holds a value, else the dynamic table's entry;
    /// then what `test` asks of the value: that it is there, that it is
    /// not `null`, or that it is truthy. A slot the scope may not reach
    /// or nothing wrote is the class's `__isset`, whose answer's truth
    /// is the answer; php places the name silently here whatever the
    /// class declares (`zend_get_property_offset(ce, name, 1, ...)`),
    /// so the property the scope may not reach reaches the accessor and
    /// never the `Error`. Under [`PropertyTest::Exists`],
    /// `property_exists()`'s `ZEND_PROPERTY_EXISTS`, no accessor is
    /// called at all.
    ///
    /// Under [`PropertyTest::NotEmpty`], `empty($o->p)`, a `__isset`
    /// that said yes is followed by `__get`, whose value's truth is the
    /// answer, and a class with no `__get` answers `false`; both are
    /// served under the guard the `__isset` still holds, so an
    /// accessor that asks about its own name answers for itself.
    ///
    /// ```php
    /// class M {
    ///     public $kept = 1;
    ///     public function __isset($k) { echo "isset $k\n"; return $k === "y"; }
    ///     public function __get($k) { echo "get $k\n"; return 0; }
    /// }
    /// $m = new M;
    /// var_dump(isset($m->kept), isset($m->y), isset($m->nope));
    /// var_dump(empty($m->y), empty($m->nope));
    /// ```
    /// ```expect
    /// isset y
    /// isset nope
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// isset y
    /// get y
    /// isset nope
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn has_property(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let class = object.class();
        let o = object.heap();
        if let Some(hooked) = self.hooked(class, name)
            && self.hooked_accessible(&hooked)
        {
            return self.has_hooked(object, name, test, hooked);
        }
        // php's `value`, the slot the placement found, which keeps a
        // lazy object from initialising unless the slot itself waits.
        let mut placed_slot = None;
        let value = match self.declared_slot(class, name) {
            Some(placed) => {
                if self.may_access(placed.declaring, placed.visibility) {
                    placed_slot = Some(placed.slot);
                    let z = o.slot(placed.slot);
                    (!z.is_undef()).then(|| Value::of_zval(z))
                } else {
                    None
                }
            }
            None => self.object_prop(o, name),
        };
        if let Some(value) = value {
            let value = value.deref();
            return Ok(match test {
                PropertyTest::Exists => true,
                PropertyTest::Isset => value.tag() != TypeTag::Null,
                PropertyTest::NotEmpty => crate::api::coerce::truthy(self, &value),
            });
        }
        // `IS_PROP_UNINIT`: "Skip __isset() for uninitialized typed
        // properties", which answer no whatever the test; a slot that
        // waits on a lazy initializer is `IS_PROP_LAZY` first, which
        // initialises the object below.
        if placed_slot.is_some_and(|slot| o.slot_uninit(slot) && !self.lazy_slot(o, slot)) {
            return Ok(false);
        }
        // `lazy_init:`, which a slot that waits reaches past `__isset`
        // and a name the placement found no slot for reaches where no
        // accessor answers it.
        let initialises = |req: &Request| match placed_slot {
            Some(slot) => req.lazy_slot(o, slot),
            None => o.laziness() != php_heap::Laziness::Eager,
        };
        if placed_slot.is_some_and(|slot| self.lazy_slot(o, slot)) {
            return self.lazy_has(object, name, test);
        }
        // GH-21478: a proxy that forwards, asked from inside the
        // instance's own `__isset` for this name, asks the instance.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & IN_ISSET != 0
        {
            return self.has_property(instance, name, test);
        }
        let issetter = self.magic_accessor(class, b"__isset");
        if issetter.is_some() && test == PropertyTest::Exists {
            return Ok(false);
        }
        let Some(issetter) = issetter else {
            if initialises(self) {
                return self.lazy_has(object, name, test);
            }
            return Ok(false);
        };
        if self.guard_word(object, name) & IN_ISSET != 0 {
            if initialises(self) {
                return self.lazy_has(object, name, test);
            }
            return Ok(false);
        }
        // php holds `IN_ISSET` across both calls, so the `__get` the
        // `empty` arm asks for runs with the `__isset`'s guard still
        // standing.
        self.guard_take(object, name, IN_ISSET);
        let answered = self.serve_accessor(object, issetter, name, &[]);
        let mut held = match answered {
            Ok(answered) => {
                let truthy = crate::api::coerce::truthy(self, &answered);
                self.release(answered);
                truthy
            }
            Err(thrown) => {
                self.guard_drop(object, name, IN_ISSET);
                return Err(thrown);
            }
        };
        // `if (has_set_exists == ZEND_PROPERTY_NOT_EMPTY && result)`:
        // `empty` asks the `__get` after the `__isset` and reads the
        // truth of what it answered; a class with no `__get` is
        // `result = false`, php's own `else`.
        if test == PropertyTest::NotEmpty && held {
            held = false;
            if let Some(getter) = self.magic_accessor(class, b"__get")
                && self.guard_word(object, name) & IN_GET == 0
            {
                let answered = self.call_accessor(object, getter, IN_GET, name, &[]);
                match answered {
                    Ok(answered) => {
                        held = crate::api::coerce::truthy(self, &answered);
                        self.release(answered);
                    }
                    Err(thrown) => {
                        self.guard_drop(object, name, IN_ISSET);
                        return Err(thrown);
                    }
                }
            }
        }
        self.guard_drop(object, name, IN_ISSET);
        Ok(held)
    }

    /// `zend_std_has_property`'s `lazy_init:` arm: the object
    /// initialised and asked again on what that answers, under
    /// `IN_ISSET` there where its class has a `__isset`, so the accessor
    /// is not served twice for one question.
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function ($o) { echo "init o\n"; });
    /// var_dump(isset($o->nope));
    /// $p = $r->newLazyProxy(function () { echo "factory\n"; return new C; });
    /// unset($p->a);
    /// var_dump($p);
    /// ```
    /// ```expect
    /// init o
    /// bool(false)
    /// factory
    /// lazy proxy object(C)#4 (1) {
    ///   ["instance"]=>
    ///   object(C)#5 (0) {
    ///   }
    /// }
    /// ```
    fn lazy_has(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let instance = self.lazy_init(object)?;
        if self.serves_accessor(instance.class(), b"__isset")
            && self.guard_word(instance, name) & IN_ISSET == 0
        {
            self.guard_take(instance, name, IN_ISSET);
            let held = self.has_property(instance, name, test);
            self.guard_drop(instance, name, IN_ISSET);
            return held;
        }
        self.has_property(instance, name, test)
    }

    /// `zend_std_unset_property`: a declared slot the scope may reach is
    /// left uninitialised, what it held released; one it may not reach
    /// is php's `Error`, and so is one whose set visibility the scope
    /// may not write, whose word here is `unset`
    /// (`Z/Zend/zend_object_handlers.c:1583`); a dynamic name is
    /// removed from the table, separated first where a cast shares it.
    ///
    /// A class with a `__unset` is served where the placement found
    /// nothing to remove: a name no class declared that the table does
    /// not hold, a declared untyped slot an `unset` already emptied,
    /// and, since the placement is made silent by the accessor
    /// (`zend_get_property_offset(ce, name, ce->__unset != NULL, ...)`),
    /// a declared slot the scope may not reach. It is served under
    /// `IN_UNSET`, so an `__unset` unsetting the name it was called for
    /// removes nothing rather than reaching itself, and the `Error` for
    /// the property the scope may not reach is raised on that second
    /// pass (`zend_wrong_offset`).
    ///
    /// ```php
    /// class M {
    ///     public $kept = 1;
    ///     public function __unset($k) { echo "unset $k\n"; }
    /// }
    /// $m = new M;
    /// $m->dyn = 2;
    /// unset($m->kept);
    /// unset($m->dyn);
    /// unset($m->nope);
    /// var_dump($m);
    /// class U { public function __unset($k) { echo "unset $k\n"; unset($this->w); } }
    /// $u = new U;
    /// unset($u->w);
    /// ```
    /// ```expect
    ///
    /// Deprecated: Creation of dynamic property M::$dyn is deprecated in %s on line 7
    /// unset nope
    /// object(M)#1 (0) {
    /// }
    /// unset w
    /// ```
    ///
    /// `IS_PROP_UNINIT` tells a typed slot nothing has written, which
    /// passes the magic methods by, from one an `unset` emptied, which
    /// reaches them; the set visibility is checked where php checks it,
    /// inside the accessor, and names the class that declared the
    /// property:
    ///
    /// ```php
    /// class C { public private(set) int $a = 1; public int $u;
    ///     function __construct() { unset($this->a); } }
    /// class D extends C {
    ///     function __unset($n) { echo "__unset $n\n"; unset($this->a); }
    ///     function __get($n) { echo "__get $n\n"; return 7; }
    /// }
    /// $d = new D;
    /// try { unset($d->a); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($d->a);
    /// try { var_dump($d->u); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// unset($d->u);
    /// var_dump($d->u);
    /// ```
    /// ```expect
    /// __unset a
    /// Cannot unset private(set) property C::$a from scope D
    /// __get a
    /// int(7)
    /// Typed property C::$u must not be accessed before initialization
    /// __get u
    /// int(7)
    /// ```
    pub(crate) fn unset_property(&mut self, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
        let class = object.class();
        let o = object.heap();
        if let Some(hooked) = self.hooked(class, name)
            && self.hooked_accessible(&hooked)
        {
            return Err(self.unset_hooked(object, name));
        }
        let unsetter = self.magic_accessor(class, b"__unset");
        let mut wrong = false;
        if let Some(placed) = self.declared_slot(class, name) {
            if self.may_access(placed.declaring, placed.visibility) {
                let empty = o.slot(placed.slot).is_undef();
                // `error = Z_TYPE_P(slot) != IS_UNDEF || (Z_PROP_FLAG_P(slot)
                // & IS_PROP_UNINIT) || !zobj->ce->__unset`: a slot that
                // holds a value, and a typed one nothing has written,
                // are held to the set visibility here; an untyped slot
                // an `unset` emptied is the `__unset`'s.
                let waits = self.lazy_slot(o, placed.slot);
                let uninit = o.slot_uninit(placed.slot);
                let in_unset = unsetter.is_some() && self.guard_word(object, name) & IN_UNSET != 0;
                if !empty || uninit || unsetter.is_none() || in_unset || waits {
                    // `zend_readonly_property_unset_error`, ahead of the
                    // set visibility: an initialised readonly property is
                    // unset from no scope, the class's own included, save
                    // inside `__clone`, where the slot is
                    // `IS_PROP_REINITABLE`.
                    if placed.readonly
                        && !empty
                        && !o.slot_bit(placed.slot, php_heap::PropBit::Reinitable)
                    {
                        let declaring = self.class_name(placed.declaring);
                        let shown = String::from_utf8_lossy(name).into_owned();
                        return Err(crate::api::raising::throw(
                            self,
                            "Error",
                            &format!("Cannot unset readonly property {declaring}::${shown}"),
                        ));
                    }
                    if let Some(set) = placed.set_visibility
                        && !self.has_set_access(placed.declaring, set)
                    {
                        return Err(self.set_inaccessible_property(
                            placed.declaring,
                            set,
                            placed.readonly,
                            name,
                            "unset",
                        ));
                    }
                    if !empty {
                        if let Some(cell) = o.slot(placed.slot).as_reference() {
                            self.slot_source_gone(object, placed.slot, cell);
                        }
                        Heap::object_set_slot(&mut Hooks(self), o, placed.slot, Zval::UNDEF);
                        // An emptied slot is no longer `IS_PROP_UNINIT`,
                        // whatever it was made with.
                        o.set_slot_bit(placed.slot, php_heap::PropBit::Uninit, false);
                    } else if uninit && !waits {
                        // `Z_PROP_FLAG_P(slot) = 0`: the bit reset and
                        // `__unset` passed by.
                        o.set_slot_bit(placed.slot, php_heap::PropBit::Uninit, false);
                    } else if waits {
                        // `IS_PROP_LAZY`: the object initialised and the
                        // property unset on what that answers.
                        let instance = self.lazy_init(object)?;
                        return self.unset_property(instance, name);
                    }
                    return Ok(());
                }
            } else {
                if unsetter.is_none() {
                    return Err(self.inaccessible_property(class, placed.visibility, name));
                }
                wrong = true;
            }
        } else if let Some(table) = o.properties() {
            let key = self.interned(name)?;
            let key = php_heap::Key::Str(key.heap());
            if table.contains(key) {
                let table = self.array_separate(table)?;
                self.heap.object_set_properties(o, table);
                self.array_del(table, key);
                return Ok(());
            }
        }
        // GH-21478: a proxy that forwards, unset from inside the
        // instance's own `__unset` for this name, unsets the instance's.
        if let Some(instance) = self.lazy_instance(o)
            && self.guard_word(instance, name) & IN_UNSET != 0
        {
            return self.unset_property(instance, name);
        }
        if let Some(unsetter) = unsetter {
            if self.guard_word(object, name) & IN_UNSET == 0 {
                let answered = self.call_accessor(object, unsetter, IN_UNSET, name, &[])?;
                self.release(answered);
                return Ok(());
            }
            if wrong && let Some(placed) = self.declared_slot(class, name) {
                return Err(self.inaccessible_property(class, placed.visibility, name));
            }
        }
        // A lazy object with nothing that answered the name is
        // initialised and the name unset on what that answers, under
        // `IN_UNSET` there where the `__unset` guard stood here.
        if o.laziness() != php_heap::Laziness::Eager {
            let instance = self.lazy_init(object)?;
            if unsetter.is_some() && self.guard_word(instance, name) & IN_UNSET == 0 {
                self.guard_take(instance, name, IN_UNSET);
                let unset = self.unset_property(instance, name);
                self.guard_drop(instance, name, IN_UNSET);
                return unset;
            }
            return self.unset_property(instance, name);
        }
        Ok(())
    }

    /// `zend_class_implements_interface(ce, zend_ce_arrayaccess)`: the
    /// object's class where it is an `ArrayAccess`, else
    /// `zend_bad_array_access`, php's `Error` for an object used as an
    /// array.
    fn array_access(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let implements = self
            .class_named(b"ArrayAccess")
            .is_some_and(|interface| self.instance_of(object.class(), interface));
        if implements {
            return Ok(());
        }
        let class = self.class_name(object.class());
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!("Cannot use object of type {class} as array"),
        ))
    }

    /// `zend_std_read_dimension`: `offsetGet` on an `ArrayAccess`, the
    /// offset `null` for `$o[]`; under `BP_VAR_IS` `offsetExists` first,
    /// `null` where it says no, and `offsetGet` after it where it says
    /// yes, so a chain under an `isset` calls the two in that order and
    /// calls neither on a class that is no `ArrayAccess`.
    pub(crate) fn read_dimension(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Value, Thrown> {
        self.array_access(object)?;
        let offset = offset.copied().unwrap_or_else(Value::null);
        if read == DimensionRead::Silent {
            let exists = crate::api::call::call_method(
                self,
                &object,
                b"offsetExists",
                std::slice::from_ref(&offset),
            )?;
            let answer = crate::api::coerce::truthy(self, &exists);
            self.release(exists);
            if !answer {
                return Ok(Value::null());
            }
        }
        crate::api::call::call_method(self, &object, b"offsetGet", &[offset])
    }

    /// `zend_std_write_dimension`: `offsetSet` on an `ArrayAccess`, the
    /// offset `null` for `$o[] = v`; `value` is given up either way.
    pub(crate) fn write_dimension(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        if let Err(thrown) = self.array_access(object) {
            self.release(value);
            return Err(thrown);
        }
        let offset = offset.copied().unwrap_or_else(Value::null);
        let served = crate::api::call::call_method(self, &object, b"offsetSet", &[offset, value]);
        self.release(value);
        self.release(served?);
        Ok(())
    }

    /// `zend_std_has_dimension`: `offsetExists` on an `ArrayAccess`,
    /// and under `not_empty` the truth of `offsetGet` after it.
    pub(crate) fn has_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        self.array_access(object)?;
        let exists = crate::api::call::call_method(
            self,
            &object,
            b"offsetExists",
            std::slice::from_ref(offset),
        )?;
        let mut answer = crate::api::coerce::truthy(self, &exists);
        self.release(exists);
        if answer && not_empty {
            let value = crate::api::call::call_method(
                self,
                &object,
                b"offsetGet",
                std::slice::from_ref(offset),
            )?;
            answer = crate::api::coerce::truthy(self, &value);
            self.release(value);
        }
        Ok(answer)
    }

    /// `zend_std_unset_dimension`: `offsetUnset` on an `ArrayAccess`.
    pub(crate) fn unset_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        self.array_access(object)?;
        let served = crate::api::call::call_method(
            self,
            &object,
            b"offsetUnset",
            std::slice::from_ref(offset),
        )?;
        self.release(served);
        Ok(())
    }

    /// `zend_std_get_constructor`: the class's `__construct` where the
    /// executed scope may reach it, `zend_bad_constructor_call`'s
    /// `Error` where it may not, and none for a class with no
    /// constructor.
    pub(crate) fn constructor_of(&mut self, object: PhpObject) -> Result<Option<Function>, Thrown> {
        let Some(constructor) = self.method_named(object.class(), b"__construct") else {
            return Ok(None);
        };
        if self.method_accessible(constructor) {
            return Ok(Some(constructor));
        }
        // `zend_bad_constructor_call`: the sentence names no `method`.
        let shown = visibility_string(self.method_visibility(constructor));
        let class = self
            .method_class(constructor)
            .map_or_else(|| String::from("object"), |class| self.class_name(class));
        let from = self.scope_phrase();
        Err(crate::api::raising::throw(
            self,
            "Error",
            &format!("Call to {shown} {class}::__construct() from {from}"),
        ))
    }

    /// `zend_std_cast_object_tostring`: `__toString` for a string, its
    /// value where it is one and php's `Error` where it is not; `true`
    /// for a bool; and none for every other type and for a class with no
    /// `__toString`.
    pub(crate) fn cast_object(
        &mut self,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Value>, Thrown> {
        match to {
            TypeTag::String => {
                if self.method_named(object.class(), b"__tostring").is_none() {
                    return Ok(None);
                }
                let value = crate::api::call::call_method(self, &object, b"__toString", &[])?;
                let value = self.unwrap_reference(value);
                if value.as_php_string().is_some() {
                    return Ok(Some(value));
                }
                self.release(value);
                let class = self.class_name(object.class());
                Err(crate::api::raising::throw(
                    self,
                    "Error",
                    &format!("Method {class}::__toString() must return a string value"),
                ))
            }
            TypeTag::Bool => Ok(Some(Value::bool(true))),
            _ => Ok(None),
        }
    }

    /// `zend_std_get_debug_info` (`Z/Zend/zend_object_handlers.c:2148`):
    /// what `__debugInfo` returns, an array's rows, the empty set for
    /// `null` and `E_ERROR` for anything else. `None` for a class that
    /// declares none, which is php's `get_properties(object)` there: the
    /// members themselves, left to the reader that asked. The rows the
    /// method returned are the user's own and carry no mangling, so a
    /// row named as a private property is shown under the name the
    /// method gave it.
    ///
    /// ```php
    /// class D { public $a = 1; public function __debugInfo(): array { return ['shown' => 2]; } }
    /// print_r(new D);
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// D Object
    /// (
    ///     [shown] => 2
    /// )
    /// object(D)#1 (1) {
    ///   ["shown"]=>
    ///   int(2)
    /// }
    /// ```
    ///
    /// ```php
    /// class D { private $a = 1; public function __debugInfo(): array { return ['a' => 2, 5]; } }
    /// print_r(new D);
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// D Object
    /// (
    ///     [a] => 2
    ///     [0] => 5
    /// )
    /// object(D)#1 (2) {
    ///   ["a"]=>
    ///   int(2)
    ///   [0]=>
    ///   int(5)
    /// }
    /// ```
    ///
    /// ```php
    /// class E { public function __debugInfo() { return 1; } }
    /// var_dump(new E);
    /// ```
    /// ```expect exit=255
    /// Fatal error: __debuginfo() must return an array in %s on line 3
    /// ```
    pub(crate) fn debug_info(&mut self, object: PhpObject) -> Result<Option<PhpArray>, Thrown> {
        if self.method_named(object.class(), b"__debuginfo").is_none() {
            // `zend_lazy_object_debug_info`: a proxy that forwards shows
            // its instance, and a lazy object that waits shows its
            // members as they stand, neither initialised for it.
            return self.lazy_debug_info(object);
        }
        let value = crate::api::call::call_method(self, &object, b"__debugInfo", &[])?;
        let value = self.unwrap_reference(value);
        match value.as_array() {
            // The array the method returned, its one owner the caller's,
            // which is php's `*is_temp = 1` over the same table.
            Some(array) => Ok(Some(*array)),
            None if value.tag() == TypeTag::Null => {
                self.release(value);
                PhpArray::new(self).map(Some)
            }
            None => {
                self.release(value);
                Err(crate::api::raising::fatal(
                    self,
                    "__debuginfo() must return an array",
                ))
            }
        }
    }

    /// `zend_std_get_closure`: the class's `__invoke` with the object as
    /// its receiver and the class as its scope, and none for a class
    /// with no `__invoke`.
    pub(crate) fn invoke_closure(&mut self, object: PhpObject) -> Option<ClosureInfo> {
        let function = self.method_named(object.class(), b"__invoke")?;
        Some(ClosureInfo {
            function,
            this: Some(object),
            scope: Some(object.class()),
            called_scope: Some(object.class()),
            bound: Vec::new(),
            fake: true,
        })
    }

    /// `zend_std_compare_objects` (`Z/Zend/zend_object_handlers.c:2227`),
    /// the `Std` `compare` slot. An object against a non-object is the
    /// cast path: `cast_object` to the other side's type, a refusal for
    /// an int or a float php's notice and `1` or `1.0` in the object's
    /// place, a refusal for anything else the object as the greater side,
    /// and the cast compared by [`crate::ops::compare`] in the object's
    /// position. Two objects are equal when one; uncomparable, `None`,
    /// across classes; and within one class, while neither carries a
    /// dynamic table, each declared slot in turn, both uninitialised
    /// passing, one uninitialised `1`, both held compared as `zend_compare`
    /// compares them, the left object protected for the walk and the
    /// `Error` php throws when the walk reaches it again. Where either
    /// carries a dynamic table it is `zend_compare_symbol_tables` over
    /// the two rebuilt tables: the counts first, then the left one's keys
    /// looked up in the right one, an uninitialised slot `-1` against a
    /// held one and `1` the other way.
    ///
    /// php opens the handler with `zend_objects_check_stack_limit`
    /// (`Z/Zend/zend_object_handlers.c:2231`) and throws
    /// `Maximum call stack size reached during object comparison` where
    /// the thread has come down to `zend.max_allowed_stack_size`, which
    /// is [`php_compile::StackLimit::overflowed`] here and is asked in
    /// the same place, before either side is even read as an object. It
    /// ends a walk no recursion guard can: a chain of objects deep
    /// enough to exhaust the C stack without ever reaching one twice.
    pub(crate) fn compare_objects(
        &mut self,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        use std::cmp::Ordering;
        if self.stack_limit.overflowed() {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                "Maximum call stack size reached during object comparison",
            ));
        }
        let (a, b) = match (left.as_object().copied(), right.as_object().copied()) {
            (Some(a), Some(b)) => (a, b),
            (Some(object), None) => return self.compare_cast(object, right, true).map(Some),
            (None, Some(object)) => return self.compare_cast(object, left, false).map(Some),
            (None, None) => return Ok(None),
        };
        if a == b {
            return Ok(Some(Ordering::Equal));
        }
        if a.class() != b.class() {
            return Ok(None);
        }
        let (x, y) = (a.heap(), b.heap());
        // `GC_ADDREF` on both for the walk, given up at `done`.
        self.addref(left);
        self.addref(right);
        let eager = |o: Obj| o.laziness() == php_heap::Laziness::Eager;
        let order = if x.properties().is_none() && y.properties().is_none() && eager(x) && eager(y)
        {
            self.compare_slots(x, y)
        } else {
            // `zend_std_get_properties_ex` on each side, which
            // initialises a lazy one first, the left before the right.
            self.lazy_init(a).and_then(|a| {
                let b = self.lazy_init(b)?;
                self.compare_tables(a.heap(), b.heap())
            })
        };
        self.release(*left);
        self.release(*right);
        order.map(Some)
    }

    /// The declared-slot walk of [`Request::compare_objects`], under the
    /// recursion guard on the left object; it is enough to protect one,
    /// as php notes, since the other may be reached from it.
    fn compare_slots(&mut self, x: Obj, y: Obj) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        if x.slots() == 0 {
            return Ok(Ordering::Equal);
        }
        if x.is_recursive() {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                "Nesting level too deep - recursive dependency?",
            ));
        }
        x.protect();
        let mut order = Ordering::Equal;
        for i in 0..x.slots() {
            let (p, q) = (x.slot(i), y.slot(i));
            order = match (p.is_undef(), q.is_undef()) {
                (true, true) => continue,
                (true, false) | (false, true) => Ordering::Greater,
                (false, false) => {
                    match crate::ops::compare(self, &Value::of_zval(p), &Value::of_zval(q)) {
                        Ok(Ordering::Equal) => continue,
                        Ok(order) => order,
                        Err(thrown) => {
                            x.unprotect();
                            return Err(thrown);
                        }
                    }
                }
            };
            break;
        }
        x.unprotect();
        Ok(order)
    }

    /// `zend_compare_symbol_tables` over the two objects' rebuilt tables
    /// (`rebuild_object_properties_internal`): every declared slot, held
    /// or not, in declaration order and then the dynamic table, so the
    /// count is the slots and the dynamic entries, and `zend_hash_compare`
    /// with `ordered` off over them.
    fn compare_tables(&mut self, x: Obj, y: Obj) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        let dynamic = |o: Obj| o.properties().map_or(0, |t| t.len());
        if dynamic(x) != dynamic(y) {
            return Ok(dynamic(x).cmp(&dynamic(y)));
        }
        for i in 0..x.slots() {
            let (p, q) = (x.slot(i), y.slot(i));
            match (p.is_undef(), q.is_undef()) {
                (true, true) => {}
                (true, false) => return Ok(Ordering::Less),
                (false, true) => return Ok(Ordering::Greater),
                (false, false) => {
                    let order = crate::ops::compare(self, &Value::of_zval(p), &Value::of_zval(q))?;
                    if order != Ordering::Equal {
                        return Ok(order);
                    }
                }
            }
        }
        let (Some(p), Some(q)) = (x.properties(), y.properties()) else {
            return Ok(Ordering::Equal);
        };
        crate::ops::compare(
            self,
            &Value::array(PhpArray::of(p)),
            &Value::array(PhpArray::of(q)),
        )
    }

    /// The cast path of [`Request::compare_objects`]: the object's
    /// `cast_object` to the other side's type, `object_lhs` saying which
    /// side the object is on, as `zend_std_compare_objects` reads it.
    fn compare_cast(
        &mut self,
        object: PhpObject,
        other: &Value,
        object_lhs: bool,
    ) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        let target = other.tag();
        let casted = match self
            .object_handlers(object.heap())
            .cast_object(self, object, target)?
        {
            Some(casted) => casted,
            None => match target {
                TypeTag::Int | TypeTag::Float => {
                    let class = self.class_name(object.class());
                    let to = if target == TypeTag::Int {
                        "int"
                    } else {
                        "float"
                    };
                    crate::api::raising::notice(
                        self,
                        &format!("Object of class {class} could not be converted to {to}"),
                    );
                    if target == TypeTag::Int {
                        Value::int(1)
                    } else {
                        Value::float(1.0)
                    }
                }
                _ => {
                    return Ok(if object_lhs {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    });
                }
            },
        };
        let order = if object_lhs {
            crate::ops::compare(self, &casted, other)
        } else {
            crate::ops::compare(self, other, &casted)
        };
        self.release(casted);
        order
    }

    /// `zend_active_function()`: the native being served, which is the
    /// call frame on top of [`Vm`] while an entry runs, and none while
    /// the loop is between calls. The raisers render its declared name
    /// and its parameters' names off the declaration, never off the
    /// caller's spelling.
    fn active_native_function(&self) -> Option<Function> {
        self.vm.serving_call().map(|call| call.function)
    }

    /// Enter php's startup on this request: what is raised from here is
    /// held, not shown ([`Request::end_startup`]).
    pub(crate) fn begin_startup(&mut self) {
        self.startup = Some(Vec::new());
    }

    /// Leave the startup: what was raised during it, in order.
    pub(crate) fn end_startup(&mut self) -> Vec<(Level, String)> {
        self.startup.take().unwrap_or_default()
    }

    /// `php_during_module_startup()`, which `php_verror` names the
    /// origin of a docref by, `PHP Startup` for a native's name, and
    /// what an extension's on-modify rule reads as
    /// `ZEND_INI_STAGE_STARTUP`: `true` while the engine serves the
    /// rules over the host's values, before any request.
    pub fn at_startup(&self) -> bool {
        self.startup.is_some()
    }

    /// `get_active_function_name`: the declared name of the native being
    /// served, `func->common.function_name`, whatever case the call
    /// spelled it in.
    pub(crate) fn vm_serving_native(&self) -> bool {
        self.active_native_function().is_some()
    }

    pub(crate) fn active_native(&self) -> String {
        if let Some((kind, depth)) = self.including
            && depth == self.vm.call_depth()
        {
            return String::from(kind.name());
        }
        self.active_native_function().map_or_else(
            || String::from("native"),
            |function| self.qualified_name(function, None),
        )
    }

    /// `get_active_function_or_method_name`: the native being served as
    /// a message names it, `Class::method` for a method, which is what
    /// a native writes before `(): ` in a sentence of its own that no
    /// raiser here words, as `pdo_sqlite`'s callback refusals do.
    #[must_use]
    pub fn active_function_name(&self) -> String {
        self.active_native()
    }

    /// `func->common.function_name`: the name a function was declared
    /// under, and `native` for one the table has no declaration for. A
    /// method's is its own name alone, what `get_active_function_name`
    /// answers; the class before it is [`Request::qualified_name`]'s.
    pub(crate) fn declared_name(&self, function: Function) -> String {
        self.function_decl(function).map_or_else(
            || String::from("native"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        )
    }

    /// The name a raise or a trace prints a callee under: a function's
    /// declared name, and `Class::method` for a method, as
    /// `zend_wrong_parameters_count_error` and `zend_trace_to_string`
    /// join `func->common.scope->name` to it; a trace joins with `->`
    /// where the call had a receiver.
    fn qualified_name(&self, function: Function, this: Option<PhpObject>) -> String {
        let name = self.declared_name(function);
        match self.method_class(function) {
            Some(class) => {
                let class = self.class_name(class);
                let join = if this.is_some() { "->" } else { "::" };
                format!("{class}{join}{name}")
            }
            None => name,
        }
    }

    /// `get_active_function_arg_name`: the declared name of the active
    /// native's parameter at `position`, one-based, and none past the
    /// declared count, where php's variadic slot sits
    /// (`Z/Zend/zend_execute_API.c:643-654`).
    fn active_native_arg_name(&self, position: u32) -> Option<String> {
        let decl = self.function_decl(self.active_native_function()?)?;
        let parameter = decl
            .parameters
            .get(usize::try_from(position.checked_sub(1)?).ok()?)?;
        (!parameter.variadic).then(|| String::from_utf8_lossy(&parameter.name.0).into_owned())
    }

    /// `arg_info->type` of the active native's parameter at `position`,
    /// what `zend_null_arg_deprecated` names the type by and what
    /// [`crate::api::coerce::argument`] holds the argument to: the
    /// variadic's own past the declared count, as php's `arg_offset`
    /// stops at `num_args` there; none where the native declares no
    /// type there.
    pub(crate) fn active_parameter_type(&self, position: u32) -> Option<php_unit::decl::Type> {
        let decl = self.function_decl(self.active_native_function()?)?;
        let at = usize::try_from(position.checked_sub(1)?).ok()?;
        let parameter = match decl.parameters.get(at) {
            Some(parameter) => parameter,
            None => decl.parameters.last().filter(|last| last.variadic)?,
        };
        parameter.ty.clone()
    }

    /// `func->common.scope` and `Z_CE(EX(This))` of the native being
    /// served: the class its method is declared on and the class it was
    /// called on, what a parameter typed `self`, `parent` or `static`
    /// resolves against; none for a function.
    pub(crate) fn active_native_scopes(&self) -> crate::api::coerce::Scopes {
        let own = self
            .active_native_function()
            .and_then(|function| self.method_class(function));
        let called = self.vm.serving_call().and_then(|call| call.called_scope);
        crate::api::coerce::Scopes {
            own,
            called: called.or(own),
        }
    }

    /// The `($name)` `zend_argument_error_variadic` and
    /// `zend_param_must_be_nullable` write after `#N`, with its leading
    /// space, and nothing where the parameter has no name.
    fn argument_name_suffix(&self, position: u32) -> String {
        self.active_native_arg_name(position)
            .map(|name| format!(" (${name})"))
            .unwrap_or_default()
    }

    /// `zend_argument_error_variadic`'s message:
    /// `{fn}(): Argument #N ($name) must {tail}`.
    pub(crate) fn argument_message(&self, position: u32, tail: &str) -> String {
        format!(
            "{}(): Argument #{position}{} must {tail}",
            self.active_native(),
            self.argument_name_suffix(position)
        )
    }

    /// `zend_param_must_be_nullable`'s message:
    /// `{fn}(): Passing null to parameter #N ($name) of type {type} is
    /// deprecated`, one sentence, built as php builds it.
    pub(crate) fn null_parameter_message(&self, position: u32, type_name: &str) -> String {
        format!(
            "{}(): Passing null to parameter #{position}{} of type {type_name} is deprecated",
            self.active_native(),
            self.argument_name_suffix(position)
        )
    }

    pub(crate) fn precision(&self) -> i32 {
        self.ini_number(b"precision", 14)
    }

    /// `short_open_tag`, php's `CG(short_tags)`: whether a `<?` opens
    /// code in the source this request compiles, which its scanner reads
    /// at every tag it meets (`Z/Zend/zend_language_scanner.l:2331`).
    /// php's built-in default is on (`DEFAULT_SHORT_OPEN_TAG`), which is
    /// what an engine with no entry for it answers, and the entry is
    /// `PHP_INI_SYSTEM|PHP_INI_PERDIR`, so no `ini_set()` of the
    /// request's moves it. `token_get_all` reads the same entry, which
    /// is why it is the request's and not the engine's alone.
    pub(crate) fn short_open_tag(&self) -> bool {
        self.ini_get(b"short_open_tag")
            .is_none_or(Request::ini_parse_bool)
    }

    /// `EG(assertions)`, php's `zend.assertions` read as
    /// `OnUpdateAssertions` reads it (`zend_ini_parse_quantity_warn`):
    /// `1` compiles and runs an assertion, `0` compiles it behind an
    /// `ASSERT_CHECK` that jumps over it, and `-1` compiles nothing of it.
    pub(crate) fn assertions(&self) -> i64 {
        self.ini_get(b"zend.assertions")
            .map_or(1, Request::ini_parse_bytes)
    }

    /// `OnUpdateAssertions` (`Z/Zend/zend.c:156-172`) for an
    /// `ini_set()`: a value that moves the entry to or from a negative
    /// one is php's warning and a refusal that keeps the entry as it
    /// was, since what is compiled already was compiled under it; any
    /// other value is taken.
    ///
    /// ```php
    /// var_dump(ini_get('zend.assertions'), ini_set('zend.assertions', '-1'), ini_set('zend.assertions', '0'), ini_get('zend.assertions'), ini_set('zend.assertions', '1'));
    /// ```
    /// ```expect
    /// Warning: zend.assertions may be completely enabled or disabled only in php.ini in %s on line 2
    /// string(1) "1"
    /// bool(false)
    /// string(1) "1"
    /// string(1) "0"
    /// string(1) "0"
    /// ```
    fn assertions_updated(&mut self, value: &[u8]) -> bool {
        let (current, next) = (self.assertions(), Request::ini_parse_bytes(value));
        if current != next && (current < 0 || next < 0) {
            crate::api::raising::warning(
                self,
                "zend.assertions may be completely enabled or disabled only in php.ini",
            );
            return false;
        }
        true
    }

    /// `serialize_precision`, what `var_dump`, `var_export`, `serialize`
    /// and `json_encode` print a float at; -1, its default, asks for the
    /// shortest text that reads back as the same double.
    #[must_use]
    pub fn serialize_precision(&self) -> i32 {
        self.ini_number(b"serialize_precision", -1)
    }

    /// `max_execution_time`, the seconds the engine's execution timer is
    /// armed for; zero, php's CLI default, arms none.
    pub(crate) fn max_execution_time(&self) -> i64 {
        i64::from(self.ini_number(b"max_execution_time", 0))
    }

    /// Whether a handler left a throwable pending, php's `EG(exception)`:
    /// the test the loop performs after a `throw`-effect op and after a
    /// `Flow::Resume` (`jit-design.md` §2.2). The unwind exit is one
    /// answer, since it stands in php's slot as any throwable does;
    /// [`Request::pending`] is the object alone.
    /// `zend_replace_error_handling`: the class a warning is thrown as,
    /// `None` for `EH_NORMAL`; the one it replaces comes back.
    pub(crate) fn replace_error_handling(&mut self, class: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.error_handling, class)
    }

    /// The throwable a warning became under `EH_THROW`, taken.
    pub(crate) fn take_error_handling_thrown(&mut self) -> Option<Thrown> {
        self.error_handling_thrown.take()
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some() || self.unwinding()
    }

    /// The pending throwable as a view, no owner taken: what `CATCH`
    /// tests its class against and what the unwinder chains onto.
    pub(crate) fn pending(&self) -> Option<PhpObject> {
        self.pending
    }

    /// The pending throwable out, the slot left empty and its one owner
    /// the caller's: php's `EG(exception) = NULL` beside the object it
    /// took, which is what a matching `CATCH` and an unwind into a
    /// `finally` block do with it.
    pub(crate) fn take_pending(&mut self) -> Option<PhpObject> {
        self.pending.take()
    }

    fn ini_number(&self, name: &[u8], default: i32) -> i32 {
        self.ini_get(name)
            .and_then(|value| String::from_utf8_lossy(value).parse().ok())
            .unwrap_or(default)
    }

    /// `ZEND_ARG_USES_STRICT_TYPES()` (`Z/Zend/zend_compile.h:723`) for
    /// the native being served, and `EX_USES_STRICT_TYPES()` of the
    /// innermost frame where none is: whether its caller was compiled
    /// under `declare(strict_types=1)`, which is what a native's own
    /// argument parsing reads. A native holds no frame of this engine's,
    /// so its `prev_execute_data` is the innermost frame, unless another
    /// native is being served above that frame, which is a call
    /// `zend_call_function` made: php's caller there is that native's
    /// frame, which carries no `ZEND_ACC_STRICT_TYPES`, so a native a
    /// callback names coerces its arguments though the file that named
    /// it is strict.
    ///
    /// ```php
    /// declare(strict_types=1);
    /// var_dump(array_map('strlen', [12]), array_map('str_repeat', ['a'], ['2']));
    /// try { str_repeat(12, 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   int(2)
    /// }
    /// array(1) {
    ///   [0]=>
    ///   string(2) "aa"
    /// }
    /// str_repeat(): Argument #1 ($string) must be of type string, int given
    /// ```
    ///
    /// `call_user_func` in a namespace is such a native too, since php
    /// compiles it to `INIT_USER_CALL` only where it resolves the name at
    /// compile time (`zend_compile_func_cuf`), so its callee coerces.
    ///
    /// ```php
    /// declare(strict_types=1);
    /// namespace Foo;
    /// var_dump(call_user_func('strlen', false));
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    pub(crate) fn strict_types(&self) -> bool {
        let floor = self.vm.innermost().map_or(0, |state| {
            usize::try_from(state.calls_below).unwrap_or(usize::MAX)
        });
        let calls = self.vm.calls();
        let serving = calls
            .iter()
            .rposition(|call| call.stage == exec::CallStage::Serving);
        if let Some(at) = serving
            && calls.get(floor..at).is_some_and(|below| {
                below
                    .iter()
                    .any(|call| call.stage == exec::CallStage::Serving)
            })
        {
            return false;
        }
        self.vm
            .innermost()
            .is_some_and(crate::exec::strict_types_of)
    }

    /// [`Request::strict_types`] over one frame rather than the
    /// innermost: the regime a write standing in `frame` verifies under,
    /// which is what every opcode's handler asks, since a handler holds
    /// its own frame and the innermost is only the same frame by luck.
    pub(crate) fn frame_strict_types(&self, frame: FrameRef) -> bool {
        self.vm.at(frame).is_some_and(crate::exec::strict_types_of)
    }

    /// `property_uses_strict_types()`
    /// (`Z/Zend/zend_object_handlers.c:1022`): `ZEND_ACC_STRICT_TYPES` on
    /// `EG(current_execute_data)`, the regime `zend_std_write_property`
    /// holds a typed property to. While a native is being served above
    /// the innermost frame, php's current frame is the native's own,
    /// which carries no such bit, so the weak rule; otherwise the
    /// innermost frame's own file, where the write a handler table makes
    /// on an opcode's behalf stands.
    pub(crate) fn property_strict_types(&self) -> bool {
        let Some(innermost) = self.vm.innermost() else {
            return false;
        };
        let floor = usize::try_from(innermost.calls_below).unwrap_or(usize::MAX);
        let serving = self.vm.calls().get(floor..).is_some_and(|above| {
            above
                .iter()
                .any(|call| call.stage == exec::CallStage::Serving)
        });
        !serving && crate::exec::strict_types_of(innermost)
    }

    /// `ZEND_ARG_USES_STRICT_TYPES()` (`Z/Zend/zend_compile.h:723`):
    /// `ZEND_ACC_STRICT_TYPES` on the op array the call into `frame` was
    /// made from, which is what an argument's type check reads and never
    /// the entered op array's own. False where the frame has no caller,
    /// which is the frame `serve` pushed and one a native re-entered the
    /// loop on: php's there is the native's own frame, and no native
    /// this engine declares carries the bit.
    pub(crate) fn caller_strict_types(&self, frame: FrameRef) -> bool {
        self.vm
            .at(frame)
            .and_then(|state| state.caller)
            .is_some_and(|caller| self.frame_strict_types(caller))
    }

    /// `zend_is_callable_ex` (`Z/Zend/zend_API.c:4250`) without the
    /// error text and with php's `IS_CALLABLE_SUPPRESS_DEPRECATIONS`:
    /// the callee a value names, or none.
    pub(crate) fn callable_of(&mut self, value: &Value) -> Option<Callable> {
        let resolved = self.callable_resolved(value, &mut None).ok()?;
        self.callable_made(resolved).ok()
    }

    /// Whether `zend_is_callable_ex` accepts a value, with php's
    /// `IS_CALLABLE_SUPPRESS_DEPRECATIONS`; no trampoline is made.
    pub(crate) fn is_callable_value(&self, value: &Value) -> bool {
        self.callable_resolved(value, &mut None).is_ok()
    }

    /// The callable a resolution answered, the trampoline it names made
    /// where it names one.
    fn callable_made(&mut self, resolved: Resolved) -> Result<Callable, String> {
        match resolved {
            Resolved::Made(callable) => Ok(callable),
            Resolved::Pending {
                held,
                name,
                callee,
                scope,
                this,
            } => {
                let made = match &callee {
                    Callee::Function(function) => Some(*function),
                    Callee::Trampoline {
                        class,
                        name,
                        is_static,
                    } => self.call_trampoline(*class, name, *is_static),
                    Callee::Invoke(closure) => self.invoke_trampoline(*closure),
                };
                let Some(function) = made else {
                    return Err(format!(
                        "class {} does not have a method \"{name}\"",
                        self.class_name(scope),
                    ));
                };
                let target = Target::Method {
                    function,
                    scope,
                    this,
                };
                Ok(Callable::of(held, name.as_bytes(), target))
            }
        }
    }

    /// `zend_is_callable_ex` with its `*error` (`Z/Zend/zend_API.c:4250`),
    /// the one resolver every callable php accepts goes through: a
    /// function name, a `Class::method` string, a `[$object, 'method']`
    /// or `['Class', 'method']` pair, an object whose class has
    /// `__invoke`, and a `Closure`, resolved as `zend_is_callable_at_frame`
    /// resolves them at the first user frame, to the callee, the scope
    /// and the receiver a call takes, and named as
    /// `zend_get_callable_name_ex` names it. `self`, `parent` and
    /// `static` as the class half are the frame's, each with php's 8.2
    /// deprecation; a class half named on an instance of the frame's
    /// scope binds the frame's `$this`; a method's visibility is held
    /// against the frame's scope. The `Err` is the sentence php's
    /// `*error` carries, which is what a native's `must be a valid
    /// callback` ends with; a deprecation a user handler turned into a
    /// throw is an `Err` too, whose text no caller reads past the
    /// pending throwable.
    ///
    /// What php reaches through a class's `__call` or `__callStatic`
    /// this does not: no class here serves either, so a name only they
    /// would answer is `does not have a method`.
    ///
    /// Every form, each served where `call_user_func` serves it: a
    /// function by name with and without its leading `\`, a pair on an
    /// object and on a class, the `Class::method` string, an object with
    /// `__invoke`, a closure, and a `static` method named on an object,
    /// which is served with no receiver.
    ///
    /// ```php
    /// class C { public function m($x) { return "m$x "; } public static function s($x) { return "s$x "; } public function __invoke($x) { return "i$x "; } }
    /// function f($x) { return "f$x "; }
    /// $c = new C;
    /// echo call_user_func('f', 1), call_user_func('\f', 2), call_user_func([$c, 'm'], 3), call_user_func(['C', 's'], 4), call_user_func('C::s', 5), call_user_func($c, 6), call_user_func(function ($x) { return "c$x "; }, 7), call_user_func([$c, 's'], 8), "|\n";
    /// ```
    /// ```expect
    /// f1 f2 m3 s4 s5 i6 c7 s8 |
    /// ```
    ///
    /// Each refusal is the sentence `zend_is_callable_check_func` and its
    /// kin write, which the native's `must be a valid callback` ends with.
    ///
    /// ```php
    /// class C { function m() {} private function priv() {} }
    /// foreach ([[], [1, 2, 3], [1 => 'a', 2 => 'b'], [1, 'm'], ['C', 1], 'nope', 'C::nope', 'Nope::m', '::m', 'C::m', ['C', 'priv'], 42, new C] as $cb) {
    ///     try { call_user_func($cb); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, array callback must have exactly two members
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, array callback must have exactly two members
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, array callback has to contain indices 0 and 1
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, first array member is not a valid class name or object
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, second array member is not a valid method
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class C does not have a method "nope"
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class "Nope" not found
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, invalid function name
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, non-static method C::m() cannot be called statically
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, non-static method C::priv() cannot be called statically
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, no array or string given
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, no array or string given
    /// ```
    ///
    /// The class half is read at the first user frame: `self`, `parent`
    /// and `static` are the method's own, each with its deprecation, a
    /// class the scope extends binds the frame's `$this`, the array form
    /// `[$this, 'parent::m']` is deprecated in its own words, and a
    /// method's visibility is held against that frame's scope.
    ///
    /// ```php
    /// class P { public static function who() { return 'P'; } public function inst() { return 'inst'; } private function hidden() { return 'hidden'; } }
    /// class C extends P {
    ///     public static function who() { return 'C'; }
    ///     public function test() { return [call_user_func('self::who'), call_user_func('parent::who'), call_user_func('static::who'), call_user_func(['P', 'inst']), call_user_func([$this, 'parent::who'])]; }
    /// }
    /// var_dump((new C)->test());
    /// try { call_user_func('self::who'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { call_user_func('parent::who'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { call_user_func([new C, 'hidden']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Deprecated: Use of "self" in callables is deprecated in %s on line 5
    ///
    /// Deprecated: Use of "parent" in callables is deprecated in %s on line 5
    ///
    /// Deprecated: Use of "static" in callables is deprecated in %s on line 5
    ///
    /// Deprecated: Callables of the form ["C", "parent::who"] are deprecated in %s on line 5
    /// array(5) {
    ///   [0]=>
    ///   string(1) "C"
    ///   [1]=>
    ///   string(1) "P"
    ///   [2]=>
    ///   string(1) "C"
    ///   [3]=>
    ///   string(4) "inst"
    ///   [4]=>
    ///   string(1) "P"
    /// }
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, cannot access "self" when no class scope is active
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, cannot access "parent" when no class scope is active
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, cannot access private method C::hidden()
    /// ```
    pub(crate) fn callable_checked(&mut self, value: &Value) -> Result<Callable, String> {
        let mut raised = Some(Vec::new());
        let resolved = self.callable_resolved(value, &mut raised);
        for message in raised.unwrap_or_default() {
            crate::api::raising::deprecated(self, &message);
        }
        if self.is_pending() {
            return Err(String::new());
        }
        self.callable_made(resolved?)
    }

    /// `zend_is_callable_at_frame` (`Z/Zend/zend_API.c:4139`) over the
    /// frame `zend_is_callable_ex` walks to, the deprecations it would
    /// raise collected in `raised` in the order php raises them, and
    /// none collected where `raised` is `None`, php's
    /// `IS_CALLABLE_SUPPRESS_DEPRECATIONS`.
    fn callable_resolved(
        &self,
        value: &Value,
        raised: &mut Option<Vec<String>>,
    ) -> Result<Resolved, String> {
        let frame = self.callable_frame();
        // `case IS_REFERENCE: goto again`.
        let held = value.deref();
        let mut fcc = Fcc::default();
        if let Some(object) = held.as_object().copied() {
            // `case IS_OBJECT`: the `get_closure` handler, a `Closure`'s
            // payload, then `zend_std_get_closure`'s `__invoke`, which
            // reads the class's table and no visibility.
            if object.closure(self).is_some() {
                let name = self.callable_name(&held);
                return Ok(Resolved::Made(Callable::of(
                    held,
                    name.as_bytes(),
                    Target::Closure(object),
                )));
            }
            let class = object.class();
            let Some(function) = self.method_named(class, b"__invoke") else {
                return Err(String::from("no array or string given"));
            };
            let this =
                Some(object).filter(|_| self.method_decl(function).is_none_or(|d| d.is_instance));
            let name = self.callable_name(&held);
            return Ok(Resolved::Made(Callable::of(
                held,
                name.as_bytes(),
                Target::Method {
                    function,
                    scope: class,
                    this,
                },
            )));
        }
        let mut strict = false;
        let method = if let Some(array) = held.as_array().copied() {
            if array.len() != 2 {
                return Err(String::from("array callback must have exactly two members"));
            }
            let (Some(first), Some(second)) = (array.get(&Key::Int(0)), array.get(&Key::Int(1)))
            else {
                return Err(String::from(
                    "array callback has to contain indices 0 and 1",
                ));
            };
            let (first, second) = (first.deref(), second.deref());
            if first.as_object().is_none() && first.as_str_bytes().is_none() {
                return Err(String::from(
                    "first array member is not a valid class name or object",
                ));
            }
            let Some(method) = second.as_str_bytes() else {
                return Err(String::from("second array member is not a valid method"));
            };
            if let Some(object) = first.as_object().copied() {
                fcc.calling = Some(object.class());
                fcc.object = Some(object);
            } else if let Some(class) = first.as_str_bytes() {
                strict = self.callable_class(class, frame.scope, &frame, &mut fcc, raised)?;
            }
            method.to_vec()
        } else if let Some(name) = held.as_str_bytes() {
            name.to_vec()
        } else {
            return Err(String::from("no array or string given"));
        };
        self.callable_func(&method, &frame, &mut fcc, strict, raised)?;
        let Some(callee) = fcc.function.take() else {
            return Err(String::from("no array or string given"));
        };
        let name = self.callable_name(&held);
        let scope = fcc.called.or(fcc.calling);
        let function = match callee {
            Callee::Function(function) => function,
            Callee::Trampoline { class, .. } => {
                return Ok(Resolved::Pending {
                    held,
                    name,
                    scope: scope.unwrap_or(class),
                    callee,
                    this: fcc.object,
                });
            }
            Callee::Invoke(closure) => {
                return Ok(Resolved::Pending {
                    held,
                    name,
                    scope: scope.unwrap_or(closure.class()),
                    callee,
                    this: fcc.object,
                });
            }
        };
        let target = match scope {
            None if fcc.object.is_none() => Target::Function(function),
            scope => Target::Method {
                function,
                scope: scope.unwrap_or(ClassId(0)),
                this: fcc.object,
            },
        };
        Ok(Resolved::Made(Callable::of(held, name.as_bytes(), target)))
    }

    /// The frame `zend_is_callable_ex` resolves at: the first link of
    /// the chain from `EG(current_execute_data)` that is user code, its
    /// function's scope (`get_scope`), its called scope
    /// (`zend_get_called_scope`) and its `$this`
    /// (`zend_get_this_object`); none of the three for the main op
    /// array's frame, and none where no user frame stands.
    fn callable_frame(&self) -> CallFrame {
        for link in self.chain() {
            let Link::Frame(_, _, state) = link else {
                continue;
            };
            let scope = self
                .closure_scope(state.closure)
                .or_else(|| state.function.and_then(|f| self.method_class(f)))
                .or(state.scope);
            let called = state.this.map(|this| this.class()).or(state.called_scope);
            return CallFrame {
                scope,
                called,
                this: state.this,
            };
        }
        CallFrame::default()
    }

    /// `zend_is_callable_check_class` (`Z/Zend/zend_API.c:3721`): the
    /// class half of a callable, `self`, `parent` and `static` read off
    /// the frame with their deprecation and their refusals where the
    /// frame has no class to give, any other name looked up. A class
    /// named on the frame's scope, or one the scope extends, binds the
    /// frame's `$this` where it is an instance of the scope. The `bool`
    /// is php's `*strict_class`.
    fn callable_class(
        &self,
        name: &[u8],
        scope: Option<ClassId>,
        frame: &CallFrame,
        fcc: &mut Fcc,
        raised: &mut Option<Vec<String>>,
    ) -> Result<bool, String> {
        let lowered = name.to_ascii_lowercase();
        let mut deprecate = |word: &str| {
            if let Some(raised) = raised.as_mut() {
                raised.push(format!("Use of \"{word}\" in callables is deprecated"));
            }
        };
        match lowered.as_slice() {
            b"self" => {
                let Some(scope) = scope else {
                    return Err(String::from(
                        "cannot access \"self\" when no class scope is active",
                    ));
                };
                deprecate("self");
                fcc.called = frame
                    .called
                    .filter(|called| self.instance_of(*called, scope));
                fcc.called = fcc.called.or(Some(scope));
                fcc.calling = Some(scope);
                fcc.object = fcc.object.or(frame.this);
                Ok(false)
            }
            b"parent" => {
                let Some(scope) = scope else {
                    return Err(String::from(
                        "cannot access \"parent\" when no class scope is active",
                    ));
                };
                let Some(parent) = self.class_parent(scope) else {
                    return Err(String::from(
                        "cannot access \"parent\" when current class scope has no parent",
                    ));
                };
                deprecate("parent");
                fcc.called = frame
                    .called
                    .filter(|called| self.instance_of(*called, parent));
                fcc.called = fcc.called.or(Some(parent));
                fcc.calling = Some(parent);
                fcc.object = fcc.object.or(frame.this);
                Ok(true)
            }
            b"static" => {
                let Some(called) = frame.called else {
                    return Err(String::from(
                        "cannot access \"static\" when no class scope is active",
                    ));
                };
                deprecate("static");
                fcc.called = Some(called);
                fcc.calling = Some(called);
                fcc.object = fcc.object.or(frame.this);
                Ok(true)
            }
            _ => {
                let Some(class) = self.class_named(name.strip_prefix(b"\\").unwrap_or(name)) else {
                    return Err(format!(
                        "class \"{}\" not found",
                        String::from_utf8_lossy(name)
                    ));
                };
                fcc.calling = Some(class);
                if let Some(scope) = frame.scope
                    && fcc.object.is_none()
                {
                    match frame.this {
                        Some(this)
                            if self.instance_of(this.class(), scope)
                                && self.instance_of(scope, class) =>
                        {
                            fcc.object = Some(this);
                            fcc.called = Some(this.class());
                        }
                        _ => fcc.called = Some(class),
                    }
                } else {
                    fcc.called = Some(fcc.object.map_or(class, |object| object.class()));
                }
                Ok(true)
            }
        }
    }

    /// `zend_is_callable_check_func` (`Z/Zend/zend_API.c:3829`): the
    /// function half. With no class resolved yet, a function by name,
    /// a leading `\` skipped; then a compound `Class::method`, its class
    /// half resolved as [`Request::callable_class`] resolves it, which
    /// under an array's own class is php's deprecated
    /// `["C", "parent::m"]` form and must name a class that one
    /// extends; then the method under the lowered name, `__construct`
    /// being the class's constructor under a strict class, the
    /// `Closure` trampoline `__invoke` through the object's
    /// `get_method`; and php's refusals, each in its words: an abstract
    /// method, an instance method with no receiver, a method the
    /// frame's scope may not call, and a name nothing answers.
    fn callable_func(
        &self,
        callable: &[u8],
        frame: &CallFrame,
        fcc: &mut Fcc,
        mut strict: bool,
        raised: &mut Option<Vec<String>>,
    ) -> Result<(), String> {
        let ce_org = fcc.calling.take();
        if ce_org.is_none() {
            let bare = callable.strip_prefix(b"\\").unwrap_or(callable);
            if let Some(function) = self.function_named(bare) {
                fcc.function = Some(Callee::Function(function));
                return Ok(());
            }
        }
        let split = callable.windows(2).rposition(|pair| pair == b"::");
        let method: Vec<u8> = match split {
            Some(0) => return Err(String::from("invalid function name")),
            Some(at) => {
                let scope = ce_org.or(frame.scope);
                let class = &callable[..at];
                let quiet = raised.is_none() || ce_org.is_some();
                let mut none = None;
                strict = self.callable_class(
                    class,
                    scope,
                    frame,
                    fcc,
                    if quiet { &mut none } else { raised },
                )?;
                let calling = fcc.calling.unwrap_or(ClassId(0));
                if let Some(org) = ce_org {
                    if !self.instance_of(org, calling) {
                        return Err(format!(
                            "class {} is not a subclass of {}",
                            self.class_name(org),
                            self.class_name(calling)
                        ));
                    }
                    if let Some(raised) = raised.as_mut() {
                        raised.push(format!(
                            "Callables of the form [\"{}\", \"{}\"] are deprecated",
                            self.class_name(org),
                            String::from_utf8_lossy(callable)
                        ));
                    }
                }
                callable[at + 2..].to_vec()
            }
            None if ce_org.is_some() => {
                fcc.calling = ce_org;
                callable.to_vec()
            }
            None => {
                return Err(format!(
                    "function \"{}\" not found or invalid function name",
                    String::from_utf8_lossy(callable)
                ));
            }
        };
        let lowered = method.to_ascii_lowercase();
        let found = match fcc.calling {
            Some(calling) if strict && lowered == b"__construct" => self
                .method_named(calling, b"__construct")
                .map(|function| (Callee::Function(function), false)),
            Some(calling) => {
                // A method the frame's scope may not call, where a magic
                // method would take the call, is looked for again
                // through the handler: `goto get_function_via_handler`.
                let magic: &[u8] = if fcc.object.is_some() {
                    b"__call"
                } else {
                    b"__callstatic"
                };
                self.method_named(calling, &lowered)
                    .filter(|function| {
                        self.method_visibility(*function) == Visibility::Public
                            || self.method_named(calling, magic).is_none()
                            || self.method_accessible_from(*function, frame.scope)
                    })
                    .map(|function| (Callee::Function(function), false))
                    .or_else(|| {
                        self.callable_via_handler(
                            calling, ce_org, &method, &lowered, strict, frame, fcc,
                        )
                        .map(|callee| (callee, true))
                    })
            }
            None => None,
        };
        let Some((callee, via_handler)) = found else {
            return Err(match fcc.calling {
                Some(calling) => format!(
                    "class {} does not have a method \"{}\"",
                    self.class_name(calling),
                    String::from_utf8_lossy(&method)
                ),
                None => format!(
                    "function {}() does not exist",
                    String::from_utf8_lossy(&method)
                ),
            });
        };
        self.callable_settle(callee, frame, fcc, via_handler)
    }

    /// `get_function_via_handler` in `zend_is_callable_check_func`
    /// (`Z/Zend/zend_API.c:3980`): on the object a pair named, its
    /// class's `__call` where the class half was named strictly, and
    /// otherwise its `get_method`, which is `Closure`'s `__invoke`
    /// trampoline, or the method itself, or `__call`'s trampoline where
    /// the scope may not call it or the class has none of that name; on
    /// a class, `zend_std_get_static_method`'s fallback, `__call` on the
    /// frame's `$this` where it is an instance of the class, which then
    /// is the receiver, and `__callStatic` otherwise.
    #[allow(clippy::too_many_arguments)] // `zend_is_callable_check_func`'s state, each one fact
    fn callable_via_handler(
        &self,
        calling: ClassId,
        ce_org: Option<ClassId>,
        method: &[u8],
        lowered: &[u8],
        strict: bool,
        frame: &CallFrame,
        fcc: &mut Fcc,
    ) -> Option<Callee> {
        let trampoline = |class: ClassId, is_static: bool| Callee::Trampoline {
            class,
            name: method.to_vec(),
            is_static,
        };
        if let Some(object) = fcc.object.filter(|_| Some(calling) == ce_org) {
            let class = object.class();
            let has_call = self.method_named(class, b"__call").is_some();
            if strict && has_call {
                return Some(trampoline(class, false));
            }
            if lowered == b"__invoke" && object.closure(self).is_some() {
                return Some(Callee::Invoke(object));
            }
            if let Some(function) = self.trampoline_named(class, lowered) {
                return Some(Callee::Function(function));
            }
            return has_call.then(|| trampoline(class, false));
        }
        if self.method_named(calling, b"__call").is_some()
            && let Some(this) = frame
                .this
                .filter(|this| self.instance_of(this.class(), calling))
        {
            fcc.object = fcc.object.or(Some(this));
            return Some(trampoline(this.class(), false));
        }
        self.method_named(calling, b"__callstatic")
            .map(|_| trampoline(calling, true))
    }

    /// The end of `zend_is_callable_check_func` once a callee is found:
    /// php's refusals of an abstract method, of an instance method with
    /// no receiver and of a method the frame's scope may not call, each
    /// in its words, and a `static` method's receiver dropped. A callee
    /// reached through a trampoline is held to none of them.
    fn callable_settle(
        &self,
        callee: Callee,
        frame: &CallFrame,
        fcc: &mut Fcc,
        via_handler: bool,
    ) -> Result<(), String> {
        let is_static = match &callee {
            Callee::Function(function) => self
                .method_decl(*function)
                .is_some_and(|decl| !decl.is_instance),
            Callee::Trampoline { is_static, .. } => *is_static,
            Callee::Invoke(_) => false,
        };
        if let Callee::Function(function) = callee
            && let Some(calling) = fcc.calling
            && !via_handler
        {
            let decl = self.method_decl(function);
            let shown = || {
                format!(
                    "{}::{}",
                    self.class_name(calling),
                    decl.map_or_else(String::new, |decl| {
                        String::from_utf8_lossy(&decl.function.name.0).into_owned()
                    })
                )
            };
            if decl.is_some_and(|decl| decl.is_abstract) {
                return Err(format!("cannot call abstract method {}()", shown()));
            }
            if fcc.object.is_none() && !is_static {
                return Err(format!(
                    "non-static method {}() cannot be called statically",
                    shown()
                ));
            }
            let visibility = self.method_visibility(function);
            if visibility != Visibility::Public
                && !self.method_accessible_from(function, frame.scope)
            {
                return Err(format!(
                    "cannot access {} method {}()",
                    visibility_string(visibility),
                    shown()
                ));
            }
        }
        fcc.function = Some(callee);
        if let Some(object) = fcc.object {
            fcc.called = Some(object.class());
            if is_static {
                fcc.object = None;
            }
        }
        Ok(())
    }

    /// `zend_check_method_accessible` (`Z/Zend/zend_execute.h:399`) with
    /// the scope given rather than read: a private method is its own
    /// class's, a protected one a scope `zend_check_protected` relates
    /// to its class.
    fn method_accessible_from(&self, function: Function, scope: Option<ClassId>) -> bool {
        let Some(class) = self.method_class(function) else {
            return true;
        };
        match self.method_visibility(function) {
            Visibility::Public => true,
            Visibility::Private => scope == Some(class),
            Visibility::Protected => self.check_protected(class, scope),
        }
    }

    /// `zend_get_callable_name_ex` (`Z/Zend/zend_API.c:4075`): a string
    /// as it is written, a pair as `Class::method` with the class half
    /// as written or the object's class, a fake closure made of a method
    /// as `Scope::name` and any other closure by its function's name, an
    /// object of another class as `Class::__invoke`, and `Array` for a
    /// pair of any other shape. An object's class is named whole, so an
    /// anonymous class's name resolves back to the class:
    ///
    /// ```php
    /// new class {
    ///     function __construct() {
    ///         if (is_callable([$this, 'f'], false, $name)) { call_user_func($name); }
    ///     }
    ///     function f() { echo "nice\n"; }
    /// };
    /// ```
    /// ```expect
    /// nice
    /// ```
    pub(crate) fn callable_name(&self, value: &Value) -> String {
        let held = value.deref();
        if let Some(name) = held.as_str_bytes() {
            return String::from_utf8_lossy(name).into_owned();
        }
        if let Some(array) = held.as_array().copied() {
            let pair =
                (array.len() == 2).then(|| (array.get(&Key::Int(0)), array.get(&Key::Int(1))));
            if let Some((Some(first), Some(second))) = pair
                && let Some(method) = second.deref().as_str_bytes()
            {
                let method = String::from_utf8_lossy(method);
                let first = first.deref();
                if let Some(class) = first.as_str_bytes() {
                    return format!("{}::{method}", String::from_utf8_lossy(class));
                }
                if let Some(object) = first.as_object() {
                    return format!("{}::{method}", self.whole_class_name(object.class()));
                }
            }
            return String::from("Array");
        }
        if let Some(object) = held.as_object().copied() {
            if let Some(closure) = object.closure(self) {
                let name = self.declared_name(closure.function);
                return match closure.scope.filter(|_| closure.fake) {
                    Some(scope) => format!("{}::{name}", self.whole_class_name(scope)),
                    None => name,
                };
            }
            return format!("{}::__invoke", self.whole_class_name(object.class()));
        }
        String::new()
    }
    /// `ZSTR_VAL(ce->name)` at its whole length, as
    /// `zend_create_member_string` joins it: an anonymous class's name
    /// with its NUL and its site, which names the class again when the
    /// string is resolved.
    fn whole_class_name(&self, class: ClassId) -> String {
        String::from_utf8_lossy(&self.class_name_bytes(class)).into_owned()
    }

    pub(crate) fn class_named(&self, name: &[u8]) -> Option<ClassId> {
        let wanted = name.to_ascii_lowercase();
        self.shared
            .classes
            .iter()
            .map(|decl| &decl.name)
            .chain(
                self.declared
                    .iter()
                    .filter_map(Declared::decl)
                    .map(|decl| &decl.name),
            )
            .position(|name| name.0.to_ascii_lowercase() == wanted)
            .and_then(|i| u32::try_from(i).ok())
            .map(ClassId)
            .or_else(|| {
                self.aliases
                    .iter()
                    .find(|(_, alias, _)| *alias == wanted)
                    .map(|(_, _, class)| *class)
            })
    }

    /// `EG(class_table)` in its order, each row the class and, for a row
    /// `class_alias()` added, the lowered name it is keyed by: the
    /// engine's classes, then the request's in the order they were
    /// bound, each alias standing where it was added.
    pub(crate) fn class_table(&self) -> Vec<(ClassId, Option<Vec<u8>>)> {
        let count = self.shared.classes.len() + self.declared.len();
        let mut rows = Vec::with_capacity(count + self.aliases.len());
        let mut aliases = self.aliases.iter().peekable();
        for at in 0..count {
            while let Some((_, alias, class)) = aliases.next_if(|(before, _, _)| *before <= at) {
                rows.push((*class, Some(alias.clone())));
            }
            rows.push((ClassId(u32::try_from(at).unwrap_or(u32::MAX)), None));
        }
        rows.extend(aliases.map(|(_, alias, class)| (*class, Some(alias.clone()))));
        rows
    }

    /// `do_bind_class` for every class the unit declares that php bound
    /// while the file compiled, before its main op array runs: a
    /// top-level class implementing no interface, with no parent or one
    /// the table already held, is early bound and the compiler emitted
    /// no opline for it, so the request's table takes it here. Every
    /// other class carries a `ZEND_DECLARE_CLASS` where it was written,
    /// in whichever op array of the unit that is, and is filed under its
    /// runtime definition key ([`Unit::runtime_keys`]), so it is passed
    /// over here and bound as the program serves ([`Request::bind_class`]).
    /// A name already declared is `zend_class_redeclaration_error`,
    /// `E_COMPILE_ERROR` in php's two wordings, the site of the earlier
    /// declaration named when it was a user class, and the request ends
    /// before the frame is pushed; php runs nothing of a file whose
    /// class fails to bind.
    ///
    /// A class the main op array declares with `DECLARE_CLASS_DELAYED`
    /// is tried again here, in its place among the others, as
    /// `zend_accel_do_delayed_early_binding` tries it when opcache loads
    /// a cached file: the compiler knows the engine's classes and the file's
    /// own, and a parent an earlier file of the request declared is
    /// what php's compiler, which sees the request's whole table, binds
    /// early. So a class that extends one from an include before it
    /// exists before its line runs:
    ///
    /// ```php
    /// file_put_contents('child.php', '<?php var_dump(class_exists("C", false)); class C extends P {} class Q {}');
    /// class P {}
    /// include 'child.php';
    /// echo implode(' ', array_slice(get_declared_classes(), -3)), "\n";
    /// unlink('child.php');
    /// ```
    /// ```expect
    /// bool(true)
    /// P C Q
    /// ```
    pub(crate) fn declare_classes(&mut self, unit: &Arc<Unit>) -> bool {
        let delayed: Vec<Vec<u8>> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode == Opcode::DeclareClassDelayed)
            .filter_map(|op| {
                let key = usize::try_from(op.op1).ok()?.checked_add(1)?;
                unit.main
                    .literals
                    .get(key)?
                    .as_str()
                    .map(|key| key.bytes().to_vec())
            })
            .collect();
        for (at, decl) in unit.classes.iter().enumerate() {
            let key = unit
                .runtime_keys
                .iter()
                .find(|(_, class)| usize::try_from(*class).ok() == Some(at));
            if let Some((key, _)) = key {
                if decl.flags.is_anonymous {
                    if !self.link_unbound(unit, at, decl) {
                        return false;
                    }
                    continue;
                }
                if delayed.iter().any(|delayed| **delayed == *key.0)
                    && !self.bind_at_load(unit, at, decl)
                {
                    return false;
                }
                continue;
            }
            if let Some(existing) = self.class_named(&decl.name.0) {
                let message = self.redeclaration(existing);
                let file = decl.file.0.to_vec();
                self.end_in_compile_time(Level::CompileError, &message, &file, decl.line);
                return false;
            }
            let Ok(at) = u32::try_from(at) else {
                return false;
            };
            self.bind_declared(unit, at, None);
        }
        true
    }

    /// One entry of `zend_accel_do_delayed_early_binding`: the class at
    /// `at` of `unit` bound now where its name is free and its parent,
    /// if it names one, is already declared, looked up without
    /// autoloading, then linked as the early bind links
    /// ([`Request::link_declared`] under `early`), and recorded so its
    /// opline passes over it. A link that cannot resolve yet leaves the
    /// class to the opline; one that is refused ends the request before
    /// the file runs, as php's compile-time early bind would. False when
    /// the request ended.
    fn bind_at_load(&mut self, unit: &Arc<Unit>, at: usize, decl: &ClassDecl) -> bool {
        if self.class_named(&decl.name.0).is_some() {
            return true;
        }
        if let Some(parent) = &decl.parent
            && self.class_named(&parent.0).is_none()
        {
            return true;
        }
        match self.link_declared(unit, at, decl, true) {
            Ok(true) => {
                let at = u32::try_from(at).unwrap_or(u32::MAX);
                self.bound_at_load.push((Arc::clone(unit), at));
                true
            }
            Ok(false) => true,
            Err(_) => false,
        }
    }

    /// `zend_compile_class_decl`'s `link_unbound` for the anonymous class
    /// at `at` of `unit`: one with no parent, no interface and no trait
    /// php links while the file compiles and holds in `CG(class_table)`
    /// from then on, so it is linked and bound as the unit loads, and
    /// its `DECLARE_ANON_CLASS` finds it by name
    /// ([`Request::declare_anonymous`]); any other is linked by the
    /// opline. A name already bound is the same unit loaded again, whose
    /// class stands. False when the link ended the request.
    ///
    /// ```php
    /// $before = array_map(fn($c) => strstr($c, "\0", true) ?: $c, array_slice(get_declared_classes(), -2));
    /// $o = new class {};
    /// class Q {}
    /// var_dump($before);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(15) "class@anonymous"
    ///   [1]=>
    ///   string(1) "Q"
    /// }
    /// ```
    fn link_unbound(&mut self, unit: &Arc<Unit>, at: usize, decl: &ClassDecl) -> bool {
        let unbound = decl.parent.is_none() && decl.interfaces.is_empty() && decl.traits.is_empty();
        if !unbound || self.class_named(&decl.name.0).is_some() {
            return true;
        }
        self.link_declared(unit, at, decl, true).is_ok()
    }

    /// `ZEND_DECLARE_CLASS_DELAYED` (`Z/Zend/zend_vm_def.h:7968`):
    /// nothing where the unit's load bound the class under the opline's
    /// runtime definition key ([`Request::bind_at_load`]), and
    /// [`Request::bind_class`] where it did not, the key still standing.
    pub(crate) fn bind_delayed(&mut self, unit: &Arc<Unit>, key: &[u8]) -> Result<(), Thrown> {
        let at = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .map(|(_, at)| *at);
        if let Some(bound) = self
            .bound_at_load
            .iter()
            .position(|(held, class)| Arc::ptr_eq(held, unit) && Some(*class) == at)
        {
            self.bound_at_load.swap_remove(bound);
            return Ok(());
        }
        self.bind_class(unit, key)
    }

    /// `zend_class_redeclaration_error`'s two wordings, which name the
    /// earlier declaration as it was declared, what it is and its own
    /// name, and its site when it was a user class.
    ///
    /// ```php
    /// class stdclass {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare class stdClass in %s on line 2
    /// ```
    fn redeclaration(&self, existing: ClassId) -> String {
        match self.class_decl(existing) {
            Some(earlier) => redeclaration(earlier),
            None => String::from("InternalError: a class redeclared over none"),
        }
    }

    /// The class at `at` of `unit` into the request's table, with its
    /// methods into the function table beside it, which is the whole of
    /// what binding a class is here: the parent chain is walked from the
    /// declaration on every lookup, so nothing is copied.
    fn bind_declared(
        &mut self,
        unit: &Arc<Unit>,
        at: u32,
        composed: Option<php_unit::link::Composed>,
    ) {
        let class = self
            .shared
            .classes
            .len()
            .saturating_add(self.declared.len());
        let class = ClassId(u32::try_from(class).unwrap_or(u32::MAX));
        self.declared.push(Declared {
            unit: Arc::clone(unit),
            at,
            composed: composed.map(Box::new),
        });
        let Some(decl) = self.class_decl(class) else {
            return;
        };
        // `zend_enum_register_funcs` at the link: the enum's own
        // `cases`, `from` and `tryFrom`, which the compiler declared
        // after its methods and which no body of the unit serves.
        let mut enum_functions = Vec::new();
        if decl.kind == ClassKind::Enum {
            for method in &decl.methods {
                let lowered = method.function.name.0.to_ascii_lowercase();
                if let Some(method) = crate::enums::EnumMethod::of(&lowered, decl.backing) {
                    enum_functions.push((class, method));
                }
            }
        }
        self.enum_functions.extend(enum_functions);
        let methods = self.class_decl(class).map_or(0, |decl| decl.methods.len());
        for method in 0..methods {
            let method = u32::try_from(method).unwrap_or(u32::MAX);
            let Some((unit, body)) = self.method_body(class, method) else {
                continue;
            };
            let flags = self
                .class_decl(class)
                .and_then(|decl| decl.methods.get(usize::try_from(method).ok()?))
                .map(|method| crate::attributes::Flags::of(&method.function.attributes))
                .unwrap_or_default();
            self.user_functions.push(UserFunction {
                unit,
                body,
                member: Some(Member {
                    class,
                    at: MemberAt::Method(method),
                }),
                flags,
            });
        }
        // Each hook of the class's own properties, `prop_info->hooks`,
        // a function of its own that php's function table never lists.
        let hooks: Vec<(u32, HookKind)> = self.class_decl(class).map_or_else(Vec::new, |decl| {
            decl.properties
                .iter()
                .enumerate()
                .flat_map(|(at, property)| {
                    let at = u32::try_from(at).unwrap_or(u32::MAX);
                    HookKind::ALL.into_iter().filter_map(move |kind| {
                        property
                            .hooks
                            .as_ref()
                            .and_then(|hooks| hooks.of(kind))
                            .map(|_| (at, kind))
                    })
                })
                .collect()
        });
        for (property, kind) in hooks {
            let Some((unit, body)) = self.hook_body(class, property, kind) else {
                continue;
            };
            let flags = self
                .class_decl(class)
                .and_then(|decl| decl.properties.get(usize::try_from(property).ok()?))
                .and_then(|property| property.hooks.as_ref()?.of(kind))
                .map(|hook| crate::attributes::Flags::of(&hook.function.attributes))
                .unwrap_or_default();
            self.user_functions.push(UserFunction {
                unit,
                body,
                member: Some(Member {
                    class,
                    at: MemberAt::Hook { property, kind },
                }),
                flags,
            });
        }
    }

    /// Where the op array of a hook of the property at `property` of a
    /// class the request declared is: its own unit's body, or for a
    /// property a trait gave the class the body in the trait's unit,
    /// which php shares between the copies as it shares a method's
    /// (`zend_do_traits_property_binding`).
    fn hook_body(
        &self,
        class: ClassId,
        property: u32,
        kind: HookKind,
    ) -> Option<(Arc<Unit>, exec::Body)> {
        let declared = self.declared.get(
            usize::try_from(class.0)
                .ok()?
                .checked_sub(self.shared.classes.len())?,
        )?;
        let decl = declared.decl()?;
        let name = &decl.properties.get(usize::try_from(property).ok()?)?.name;
        let own = declared
            .unit
            .classes
            .get(usize::try_from(declared.at).ok()?)?;
        let (from, at) = match own.properties.iter().position(|p| p.name == *name) {
            Some(at) => (declared, at),
            None => decl.traits.iter().find_map(|trait_name| {
                let holder = self.class_named(&trait_name.0)?;
                let from = self.declared.get(
                    usize::try_from(holder.0)
                        .ok()?
                        .checked_sub(self.shared.classes.len())?,
                )?;
                let at = from
                    .decl()?
                    .properties
                    .iter()
                    .position(|p| p.name == *name && p.hooks.is_some())?;
                Some((from, at))
            })?,
        };
        Some((
            Arc::clone(&from.unit),
            exec::Body::Hook {
                class: from.at,
                property: u32::try_from(at).ok()?,
                kind,
            },
        ))
    }

    /// Where the op array of the method at `method` of a class the
    /// request declared is: its own unit's body, or for a method a trait
    /// gave it the body in the unit of the class the composition names,
    /// which the table held when the class linked. The copy runs the
    /// trait's op array with the class as its scope, as
    /// `zend_fixup_trait_method` leaves it, so `self`, `static`, a
    /// closure made in it and a `static` property are the using class's,
    /// each class with its own:
    ///
    /// ```php
    /// trait T {
    ///     public static $n = 0;
    ///     static function inc() { return ++self::$n; }
    ///     function f() { return function () { return [static::class, self::class, __CLASS__]; }; }
    /// }
    /// class A { use T; }
    /// class B { use T; }
    /// class D extends A {}
    /// A::inc(); A::inc(); B::inc();
    /// var_dump(A::$n, B::$n, ((new D)->f())());
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// array(3) {
    ///   [0]=>
    ///   string(1) "D"
    ///   [1]=>
    ///   string(1) "A"
    ///   [2]=>
    ///   string(1) "A"
    /// }
    /// ```
    fn method_body(&self, class: ClassId, method: u32) -> Option<(Arc<Unit>, exec::Body)> {
        let declared = self.declared.get(
            usize::try_from(class.0)
                .ok()?
                .checked_sub(self.shared.classes.len())?,
        )?;
        let Some(composed) = &declared.composed else {
            return Some((
                Arc::clone(&declared.unit),
                exec::Body::Method {
                    class: declared.at,
                    method,
                },
            ));
        };
        let body = composed.bodies.get(usize::try_from(method).ok()?)?;
        let own = composed.decl.name.0.eq_ignore_ascii_case(&body.class.0);
        let from = if own {
            declared
        } else {
            let holder = self.class_named(&body.class.0)?;
            self.declared.get(
                usize::try_from(holder.0)
                    .ok()?
                    .checked_sub(self.shared.classes.len())?,
            )?
        };
        Some((
            Arc::clone(&from.unit),
            exec::Body::Method {
                class: from.at,
                method: body.method,
            },
        ))
    }

    /// `do_bind_class` at a `ZEND_DECLARE_CLASS`: the declaration the
    /// unit files under the opline's runtime definition key
    /// ([`Unit::runtime_keys`]), refused where its name is already
    /// declared as `zend_class_redeclaration_error` refuses it, at
    /// `E_COMPILE_ERROR` though the program is running, which is also
    /// what the same opline running a second time meets; then
    /// `zend_do_link_class`, the one link the early bind makes too
    /// (`php_unit::link::link`), with the parent fetched first, then
    /// each trait, then each interface, all of them before any is
    /// checked, which is the order php's messages come in. A name
    /// nothing declares, once autoloading has been asked, is php's
    /// `Error`, `Class "P" not found` for a parent, `Trait "T" not
    /// found` for a trait and `Interface "I" not found` for an
    /// interface, and a class used as a trait is refused as php's
    /// `ZEND_FETCH_CLASS_TRAIT` refuses it.
    ///
    /// A class declared in a function body is bound each time the
    /// function runs, and the second time is the redeclaration, at the
    /// level `error_get_last()` reads as 64:
    ///
    /// ```php
    /// register_shutdown_function(function () { var_dump(error_get_last()['type']); });
    /// function f() {
    ///     class G {}
    ///     echo "declared\n";
    /// }
    /// f();
    /// f();
    /// ```
    /// ```expect exit=255
    /// declared
    ///
    /// Fatal error: Cannot redeclare class G (previously declared in %s:4) in %s on line 4
    /// int(64)
    /// ```
    ///
    /// A second top-level declaration of a name the file bound is not
    /// bound early, and its opline raises the redeclaration where it
    /// stands, after what ran before it:
    ///
    /// ```php
    /// echo "a\n";
    /// class A {}
    /// class A {}
    /// ```
    /// ```expect exit=255
    /// a
    ///
    /// Fatal error: Cannot redeclare class A (previously declared in %s:3) in %s on line 4
    /// ```
    ///
    /// ```php
    /// class C { use Nope; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Trait "Nope" not found in %s:2
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// ```php
    /// class N {}
    /// class C { use N; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: C cannot use N - it is not a trait in %s:3
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// A class that uses no trait is linked without a copy: an inherited
    /// constant, property or method is found by walking `parent` from
    /// the declaration, so what this adds is the class's own row and its
    /// methods. One that uses traits is the class composed with them
    /// ([`php_unit::link::Composed`]), which stands in the table in its
    /// place.
    ///
    /// The bind stands where the class was written, so what it refuses
    /// is refused after what the file has already printed, which is
    /// what tells this apart from the early bind:
    ///
    /// ```php
    /// echo "before\n";
    /// class C implements Nope {}
    /// ```
    /// ```expect exit=255
    /// before
    ///
    /// Fatal error: Uncaught Error: Interface "Nope" not found in %s:3
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// Then the class is linked (`zend_do_link_class`) by the checks
    /// `zend_inheritance.c` makes, [`php_unit::link`], which the early
    /// bind runs too: a class implementing an interface is never early
    /// bound, so what linking refuses about one is refused here, after
    /// what the file printed, at the site php names, and a deprecation
    /// a tentative return raises is raised here before the class runs.
    ///
    /// ```php
    /// interface I { const X = 1; }
    /// interface J { const X = 2; }
    /// class C implements I, J {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C inherits both I::X and J::X, which is ambiguous in %s on line 4
    /// ```
    ///
    /// ```php
    /// interface I { const X = 1; }
    /// class C implements I { protected const X = 2; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Access level to C::X must be public (as in interface I) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C implements Throwable {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C cannot implement interface Throwable, extend Exception or Error instead in %s on line 2
    /// ```
    ///
    /// ```php
    /// class C implements UnitEnum {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-enum class C cannot implement interface UnitEnum in %s on line 2
    /// ```
    ///
    /// ```php
    /// class Foo implements UnitEnum { public static function cases(): array { return []; } }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-enum class Foo cannot implement interface UnitEnum in %s on line 2
    /// ```
    ///
    /// ```php
    /// enum Foo implements Throwable {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Enum Foo cannot implement interface Throwable in %s on line 2
    /// ```
    ///
    /// ```php
    /// enum E implements BackedEnum {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Non-backed enum E cannot implement interface BackedEnum in %s on line 2
    /// ```
    ///
    /// An interface's methods are held before what the interface itself
    /// guards (`do_interface_implementation`), and the parent before the
    /// interface list, of which a class is refused before a repeat:
    ///
    /// ```php
    /// class C implements Throwable { function getMessage(): int {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Declaration of C::getMessage(): int must be compatible with Throwable::getMessage(): string in %s on line 2
    /// ```
    ///
    /// ```php
    /// class P {}
    /// class C extends Closure implements P {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C cannot extend final class Closure in %s on line 3
    /// ```
    ///
    /// ```php
    /// class P {}
    /// interface I {}
    /// class C implements I, P, I {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: C cannot implement P - it is not an interface in %s on line 4
    /// ```
    ///
    /// ```php
    /// class P {}
    /// class C implements P {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: C cannot implement P - it is not an interface in %s on line 3
    /// ```
    ///
    /// ```php
    /// class C implements ArrayAccess {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C contains 4 abstract methods and must therefore be declared abstract or implement the remaining methods (ArrayAccess::offsetExists, ArrayAccess::offsetGet, ArrayAccess::offsetSet, ...) in %s on line 2
    /// ```
    ///
    /// ```php
    /// echo "before\n";
    /// class C implements ArrayAccess {
    ///     function offsetExists($o): bool { return true; }
    ///     function offsetGet($o): mixed { return 1; }
    ///     function offsetSet(): void {}
    ///     function offsetUnset($o): void {}
    /// }
    /// ```
    /// ```expect exit=255
    /// before
    ///
    /// Fatal error: Declaration of C::offsetSet(): void must be compatible with ArrayAccess::offsetSet(mixed $offset, mixed $value): void in %s on line 6
    /// ```
    ///
    /// ```php
    /// class A { function f(): X {} }
    /// class B extends A implements Stringable {
    ///     function __toString(): string { return ""; }
    ///     function f(): Y {}
    /// }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Could not check compatibility between B::f(): Y and A::f(): X, because class Y is not available in %s on line 5
    /// ```
    ///
    /// What php takes, it takes: an override whose parameter is wider
    /// and whose return is narrower, and one of a tentative return that
    /// declares none, which is php's deprecation and links:
    ///
    /// ```php
    /// class C implements ArrayAccess {
    ///     function offsetExists(mixed $o): bool { return true; }
    ///     function offsetGet($o) { return "got $o"; }
    ///     function offsetSet($o, $v): void {}
    ///     function offsetUnset($o): void {}
    /// }
    /// echo (new C)["k"], "\n";
    /// ```
    /// ```expect exit=0
    /// Deprecated: Return type of C::offsetGet($o) should either be compatible with ArrayAccess::offsetGet(mixed $offset): mixed, or the #[\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice in %s on line 4
    /// got k
    /// ```
    ///
    /// `Traversable` itself is php's `E_CORE_ERROR`, which names no site:
    ///
    /// ```php
    /// class C implements Traversable {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Class C must implement interface Traversable as part of either Iterator or IteratorAggregate in Unknown on line 0
    /// ```
    ///
    /// An interface list naming one interface twice is refused in the
    /// words for what the declaration is:
    ///
    /// ```php
    /// interface I {}
    /// interface J extends I, I {}
    /// ```
    /// ```expect exit=255
    /// Fatal error: Interface J cannot implement previously implemented interface I in %s on line 3
    /// ```
    pub(crate) fn bind_class(&mut self, unit: &Arc<Unit>, key: &[u8]) -> Result<(), Thrown> {
        let Some(at) = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .and_then(|(_, at)| usize::try_from(*at).ok())
        else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_CLASS carries a key the unit files no class under",
            ));
        };
        let Some(decl) = unit.classes.get(at).cloned() else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_CLASS carries a key the unit files no class under",
            ));
        };
        // `zend_bind_class_in_slot`'s failed `zend_hash_set_bucket_key`,
        // or `do_bind_class` finding its key gone because the opline ran
        // before: `E_COMPILE_ERROR` at the site that runs it either way.
        let existing = self
            .class_named(&decl.name.0)
            .and_then(|existing| self.class_decl(existing))
            .or_else(|| self.unlinked_decl(&decl.name.0));
        if let Some(existing) = existing {
            let message = redeclaration(existing);
            let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
            let line = self.current_line();
            self.end_in_compile_time(Level::CompileError, &message, &file, line);
            return Err(Thrown::of(None));
        }
        let Ok(slot) = u32::try_from(at) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a unit declares more classes than can be numbered",
            ));
        };
        self.link_begun(unit, slot);
        let name = &decl.name.0;
        if let Some(parent) = &decl.parent
            && !self.nearly_linked(&parent.0)
            && let Err(thrown) = self.fetch_to_link(parent, "Class")
        {
            return Err(self.link_abandoned(name, thrown, true));
        }
        for used in &decl.traits {
            let found = match self.fetch_to_link(used, "Trait") {
                Ok(found) => found,
                Err(thrown) => return Err(self.link_abandoned(name, thrown, false)),
            };
            if self.class_decl(found).map(|t| t.kind) != Some(ClassKind::Trait) {
                let message = format!(
                    "{} cannot use {} - it is not a trait",
                    String::from_utf8_lossy(name),
                    self.class_name(found)
                );
                let thrown = crate::api::raising::throw(self, "Error", &message);
                return Err(self.link_abandoned(name, thrown, false));
            }
            if let Err(thrown) = self.deprecated_trait(found, name) {
                return Err(self.link_abandoned(name, thrown, false));
            }
        }
        for interface in &decl.interfaces {
            if !self.nearly_linked(&interface.0)
                && let Err(thrown) = self.fetch_to_link(interface, "Interface")
            {
                return Err(self.link_abandoned(name, thrown, true));
            }
        }
        self.link_declared(unit, at, &decl, false).map(|_| ())
    }

    /// The class of a `ZEND_DECLARE_ANON_CLASS`: the anonymous class the
    /// unit files under `key`, its lowered name. php's compiler put it
    /// in `CG(class_table)` under that name, and the opline links it the
    /// first time it finds it unlinked (`zend_do_link_class`), which
    /// here is [`Request::bind_class`]; after that, and where the unit's
    /// load linked it ([`Request::declare_classes`]), the name finds the
    /// class, so one declaration is one class however often its opline
    /// is reached.
    pub(crate) fn declare_anonymous(
        &mut self,
        unit: &Arc<Unit>,
        key: &[u8],
    ) -> Result<ClassId, Thrown> {
        let name = unit
            .runtime_keys
            .iter()
            .find(|(filed, _)| *filed.0 == *key)
            .and_then(|(_, at)| unit.classes.get(usize::try_from(*at).ok()?))
            .map(|decl| decl.name.0.to_vec());
        let Some(name) = name else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_ANON_CLASS names no class the unit files",
            ));
        };
        if let Some(class) = self.class_named(&name) {
            return Ok(class);
        }
        self.bind_class(unit, key)?;
        self.class_named(&name).ok_or_else(|| {
            crate::api::raising::fatal(
                self,
                "InternalError: an anonymous class its declaration did not bind",
            )
        })
    }

    /// `zend_do_link_class` over the class at `at` of `unit`, whose
    /// parent, traits and interfaces the table already holds: the link
    /// both the early bind and a `DECLARE_CLASS` make
    /// (`php_unit::link::link`), its diagnostics raised at their sites,
    /// a refusal the end of the request at its level, and the linked
    /// class bound. A link that needs a class the table does not hold
    /// yet is php's `INHERITANCE_UNRESOLVED`: under `early`, the answer
    /// `zend_can_early_bind` gives before anything is raised or bound,
    /// so the class is left for its opline and the answer is false;
    /// otherwise the refusal it names.
    fn link_declared(
        &mut self,
        unit: &Arc<Unit>,
        at: usize,
        decl: &ClassDecl,
        early: bool,
    ) -> Result<bool, Thrown> {
        let linking = self.link_against_table(decl);
        if early && !linking.obligations.is_empty() {
            return Ok(false);
        }
        let Ok(at) = u32::try_from(at) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a unit declares more classes than can be numbered",
            ));
        };
        self.link_settled(unit, at, linking)?;
        Ok(true)
    }

    /// `zend_use_of_deprecated_trait` (`Z/Zend/zend_execute.c:2012`), as
    /// `zend_do_link_class` raises it for each trait it fetches that the
    /// `#[\Deprecated]` attribute marks: `Trait T used by C is
    /// deprecated`, with the attribute's `since` and `message`, at
    /// `E_USER_DEPRECATED` for a program's trait. A handler that throws
    /// stops the link.
    ///
    /// ```php
    /// #[\Deprecated("use U", "2.0")]
    /// trait T {}
    /// class C { use T; }
    /// class D extends C {}
    /// echo "done\n";
    /// ```
    /// ```expect
    ///
    /// Deprecated: Trait T used by C is deprecated since 2.0, use U in %s on line 4
    /// done
    /// ```
    fn deprecated_trait(&mut self, found: ClassId, used_by: &[u8]) -> Result<(), Thrown> {
        let attributes = match self.class_decl(found) {
            Some(decl) if crate::attributes::Flags::of(&decl.attributes).deprecated => {
                decl.attributes.clone()
            }
            _ => return Ok(()),
        };
        let suffix = crate::attributes::deprecation_suffix(self, &attributes, Some(found))?;
        let level = if self.native_class(found) {
            Level::Deprecated
        } else {
            Level::UserDeprecated
        };
        let message = format!(
            "Trait {} used by {} is deprecated{suffix}",
            self.class_name(found),
            String::from_utf8_lossy(used_by)
        );
        crate::api::raising::raise(self, level, &message);
        if self.is_pending() {
            return Err(Thrown::of(None));
        }
        Ok(())
    }

    /// A class as the link reads the table: its declaration, and the
    /// bodies of its methods where it was composed with traits.
    fn held_class(&self, name: &[u8]) -> Option<php_unit::link::Held<'_>> {
        let class = self.class_named(name)?;
        let decl = self.class_decl(class)?;
        let bodies = usize::try_from(class.0)
            .ok()
            .and_then(|at| at.checked_sub(self.shared.classes.len()))
            .and_then(|at| self.declared.get(at))
            .and_then(|declared| declared.composed.as_ref())
            .map(|composed| composed.bodies.as_slice());
        Some(php_unit::link::Held {
            decl,
            bodies,
            linked: true,
        })
    }

    /// `zend_fetch_class_by_name` under `ZEND_FETCH_CLASS_EXCEPTION`, as
    /// `zend_do_link_class` fetches the parent and each interface before
    /// any check is made of them: the class of that name, or the one
    /// autoloading it declared, and php's `Error` where neither is,
    /// `what` naming the class a parent and the interface an interface.
    fn fetch_to_link(
        &mut self,
        name: &php_unit::decl::Name,
        what: &str,
    ) -> Result<ClassId, Thrown> {
        let found = match self.class_named(&name.0) {
            Some(found) => Some(found),
            None => crate::api::call::autoload(self, &name.0)?,
        };
        found.ok_or_else(|| {
            crate::api::raising::throw(
                self,
                "Error",
                &format!("{what} \"{}\" not found", String::from_utf8_lossy(&name.0)),
            )
        })
    }

    /// `zend_hash_find(EG(function_table), lcname)`: the native of that
    /// name, or the user function a unit of this request declared.
    pub(crate) fn function_named(&self, name: &[u8]) -> Option<Function> {
        let wanted = name.to_ascii_lowercase();
        if let Some(i) = self
            .shared
            .functions
            .iter()
            .position(|(decl, _)| decl.name.0.to_ascii_lowercase() == wanted)
        {
            return u32::try_from(i).ok().map(Function);
        }
        let position = self.user_functions.iter().position(|user| {
            user_function_decl(user).is_some_and(|decl| decl.name.0.to_ascii_lowercase() == wanted)
        })?;
        self.user_function_id(position)
    }

    /// `zend_hash_add_ptr(CG(function_table), …)` for every function the
    /// unit declares, before its main op array runs: php binds a
    /// top-level function at compile time and the compiler emitted no
    /// opline for it, so the request's table takes it here. The compiler
    /// refused a name the natives have or the file declares twice, so
    /// nothing collides in a request that serves one unit.
    /// `do_bind_function` at a `ZEND_DECLARE_FUNCTION`
    /// (`Z/Zend/zend_compile.c:1287`): the op array the opline's `op2`
    /// found among the frame's `dynamic_func_defs` into the request's
    /// function table under the lowered name in `op1`, which is
    /// `zend_hash_add_ptr(EG(function_table), lcname, func)`. A name the
    /// table already holds is `do_bind_function_error` at `E_ERROR`,
    /// naming the new declaration and, where the earlier one was a user
    /// function with a body, its file and its first line.
    pub(crate) fn bind_function(
        &mut self,
        unit: &Arc<Unit>,
        at: u32,
        lowered: &[u8],
    ) -> Result<(), Thrown> {
        let Some((decl, _)) = unit.dynamic.get(usize::try_from(at).unwrap_or(usize::MAX)) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a DECLARE_FUNCTION naming a definition the unit has not",
            ));
        };
        let name = String::from_utf8_lossy(&decl.name.0).into_owned();
        if let Some(earlier) = self.function_named(lowered) {
            let site = self
                .user_function(earlier)
                .and_then(user_function_decl)
                .map(|decl| {
                    format!(
                        " (previously declared in {}:{})",
                        String::from_utf8_lossy(&decl.file.0),
                        decl.line
                    )
                })
                .unwrap_or_default();
            let message = format!("Cannot redeclare function {name}(){site}");
            return Err(crate::api::raising::fatal(self, &message));
        }
        self.user_functions.push(UserFunction {
            unit: Arc::clone(unit),
            body: exec::Body::Dynamic(at),
            member: None,
            flags: dynamic_flags(unit, at),
        });
        Ok(())
    }

    /// The `Function` a closure's op array is reached by, made where
    /// the request has not named it yet: php copies the whole
    /// `zend_function` into every `Closure` it creates
    /// (`zend_create_closure_ex`'s `memcpy`), and an op array here is
    /// immutable and shared, so one name stands for the declaration
    /// and what differs between two closures of it — the receiver, the
    /// scope and the bound variables — is the object's payload. The
    /// name is no entry of the function table under a callable name: a
    /// closure is reached through its object and never by name.
    pub(crate) fn closure_function(&mut self, unit: &Arc<Unit>, at: u32) -> Option<Function> {
        unit.dynamic.get(usize::try_from(at).ok()?)?;
        let body = exec::Body::Dynamic(at);
        if let Some(position) = self
            .user_functions
            .iter()
            .position(|user| user.body == body && Arc::ptr_eq(&user.unit, unit))
        {
            return self.user_function_id(position);
        }
        self.user_functions.push(UserFunction {
            unit: Arc::clone(unit),
            body,
            member: None,
            flags: dynamic_flags(unit, at),
        });
        self.user_function_id(self.user_functions.len() - 1)
    }

    /// `EX(func)->common.scope` of a frame, the class whose body the op
    /// array it serves was written in: what `zend_create_closure` is
    /// handed as the scope of a closure declared there, and none in a
    /// function or the main op array. Read off the frame and not off
    /// [`Request::executed_scope`], which answers for the native being
    /// served where one is. A closure's frame serves the closure's own
    /// copy of its function, whose scope is the one the closure was
    /// made or bound in, which `Closure::bind` and `Closure::call` move.
    pub(crate) fn frame_scope(&self, frame: FrameRef) -> Option<ClassId> {
        let state = self.vm.at(frame)?;
        if state.closure.is_some() {
            return self.closure_scope(state.closure);
        }
        match state.function {
            Some(function) => self.method_class(function),
            None => state.scope,
        }
    }

    /// The classes a `self`, a `parent` and a `static` in a type checked
    /// in `frame` name: the frame's scope and its called scope, which
    /// is what `zend_check_type_slow` resolves them against for an
    /// argument's `RECV` and for a return.
    pub(crate) fn frame_scopes(&self, frame: FrameRef) -> crate::api::coerce::Scopes {
        let own = self.frame_scope(frame);
        let called = self.vm.at(frame).and_then(|state| state.called_scope);
        crate::api::coerce::Scopes {
            own,
            called: called.or(own),
        }
    }

    /// `zend_hash_add_ptr(CG(function_table), …)` for every function the
    /// unit declares at its top level, before its main op array runs:
    /// php binds one at compile time and the compiler emitted no opline
    /// for it, so the request's table takes it here. The compiler
    /// refused a name the natives have or the file declares twice; a
    /// name another file of the request declared is
    /// `do_bind_function_error` at `E_COMPILE_ERROR`, which names the
    /// earlier declaration's file and line and ends the request before
    /// the frame is pushed, so nothing of a file that redeclares runs;
    /// `false` says so.
    pub(crate) fn declare_functions(&mut self, unit: &Arc<Unit>) -> bool {
        for (at, (decl, _)) in unit.functions.iter().enumerate() {
            if let Some(earlier) = self.function_named(&decl.name.0) {
                let site = self
                    .user_function(earlier)
                    .and_then(user_function_decl)
                    .map(|decl| {
                        format!(
                            " (previously declared in {}:{})",
                            String::from_utf8_lossy(&decl.file.0),
                            decl.line
                        )
                    })
                    .unwrap_or_default();
                let message = format!(
                    "Cannot redeclare function {}(){site}",
                    String::from_utf8_lossy(&decl.name.0)
                );
                let file = decl.file.0.to_vec();
                self.end_in_compile_time(Level::CompileError, &message, &file, decl.line);
                return false;
            }
            self.user_functions.push(UserFunction {
                unit: Arc::clone(unit),
                body: exec::Body::Function(u32::try_from(at).unwrap_or(u32::MAX)),
                member: None,
                flags: crate::attributes::Flags::of(&decl.attributes),
            });
        }
        true
    }

    pub(crate) fn constant_named(&self, name: &[u8]) -> Option<Value> {
        if name == b"PHP_SAPI" {
            return Some(self.sapi_constant);
        }
        self.constants
            .iter()
            .find(|row| row.name == name)
            .map(|row| row.value)
            .or_else(|| self.shared.constant(name))
    }

    fn ini_parse_bytes_of(&self, name: &str) -> i64 {
        self.ini_get(name.as_bytes())
            .map_or(0, Request::ini_parse_bytes)
    }

    // -------------------------------------------------- output and headers

    /// Write body bytes, `php_output_write`: through the handler stack
    /// while one is active, and what passes it reaches the host, with
    /// the header block committed at the first byte that does
    /// (`crate::output`).
    pub fn write(&mut self, bytes: &[u8]) {
        crate::output::write(self, bytes);
    }

    /// `sapi_module.ub_write`: bytes past the `php_output` layer, the
    /// host's, handed to the sink as they come.
    pub(crate) fn ub_write(&mut self, bytes: &[u8]) {
        self.sink.get().write(bytes);
    }

    /// The `php_output` layer, and the heap it lays its handlers down in.
    pub(crate) fn output_and_heap(&mut self) -> (&mut Output, &mut Heap) {
        (&mut self.output, &mut self.heap)
    }

    pub(crate) fn output(&self) -> &Output {
        &self.output
    }

    pub(crate) fn output_mut(&mut self) -> &mut Output {
        &mut self.output
    }

    /// `header_register_callback`'s callable, taken to be served once by
    /// `sapi_send_headers`, which unsets `SG(callback_func)` before the
    /// call so a second send finds none.
    pub(crate) fn take_header_callback(&mut self) -> Option<Callable> {
        self.header_callback.take()
    }

    /// Write to the host's error stream, bytes as given: php's
    /// `fputs(stderr)`, which the `display_errors=stderr` form takes and
    /// the `STDERR` stream constant will. The log is another channel,
    /// [`Request::error_log`]'s; whether the host puts the two on one fd
    /// is the host's, as it is php's CLI's (#216).
    pub fn write_stderr(&mut self, bytes: &[u8]) {
        self.sink.get().write_stderr(bytes);
    }

    /// `flush()`, `sapi_flush`: the host may push what it has; the
    /// handler stack is `ob_flush`'s, not this.
    pub fn flush(&mut self) {
        self.sink.get().flush();
    }

    /// One `ob_*` operation on the handler stack, the `PHP_FUNCTION`
    /// bodies of `Z/main/output.c:1373-1591` over the layer in
    /// `crate::output`; the notices are theirs, raised through the
    /// native being served.
    pub fn output_buffer_op(&mut self, op: &OutputOp) -> Result<Value, Thrown> {
        crate::output::buffer_op(self, op)
    }

    /// Where the first byte reached the host, `php_output_get_start_filename`
    /// and `_lineno`, once one has.
    #[must_use]
    pub fn output_started_at(&self) -> Option<(Vec<u8>, u32)> {
        self.output.started_at()
    }

    /// `php_output_handler_alias`: the handler an extension registered
    /// under `name`, which `ob_start()` takes before it reads the name as
    /// a callable.
    #[must_use]
    pub fn buffer_handler(&self, name: &[u8]) -> Option<BufferHandler> {
        self.registered::<BufferHandler>()
            .find(|handler| handler.name.as_bytes() == name)
            .copied()
    }

    /// `SG(sapi_headers).mimetype` and `send_default_content_type`.
    #[must_use]
    pub fn content_type(&self) -> ContentType {
        self.output.content_type()
    }

    /// `header()`, `sapi_header_op` under `SAPI_HEADER_ADD` or
    /// `SAPI_HEADER_REPLACE` (`crate::output`): the line taken, replacing
    /// the lines of its name when `replace` is set, with `response_code`
    /// set when it is not zero; once the block is sent, php's warning
    /// naming where the body started, and `false`. Under the CLI the
    /// host's `header_handler` takes no line, so the list stays empty
    /// and the status line and the response code are all a `header()`
    /// moves, as under php's.
    pub fn header_add(&mut self, line: &[u8], replace: bool, response_code: i64) -> bool {
        crate::output::header_add(self, line, replace, response_code)
    }

    /// `header_remove()`, `sapi_header_op` under `SAPI_HEADER_DELETE` or,
    /// with `None`, `SAPI_HEADER_DELETE_ALL`: the lines of that name
    /// dropped, or all of them; the warnings are `sapi_header_op`'s.
    pub fn header_remove(&mut self, name: Option<&[u8]>) -> bool {
        crate::output::header_remove(self, name)
    }

    /// `headers_list()`: `SG(sapi_headers).headers`, in order.
    #[must_use]
    pub fn headers_list(&self) -> Vec<Vec<u8>> {
        self.output.headers().to_vec()
    }

    /// `SG(sapi_headers).http_status_line`: the `HTTP/...` line a
    /// `header()` set, held until the block is sent.
    #[must_use]
    pub fn status_line(&self) -> Option<&[u8]> {
        self.output.status_line()
    }

    /// `headers_sent()`: `SG(headers_sent)`, and where the body started
    /// when it was a body byte that sent them.
    #[must_use]
    pub fn headers_sent(&self) -> Option<(Vec<u8>, u32)> {
        if self.output.headers_sent() {
            Some(self.output.started_at().unwrap_or_default())
        } else {
            None
        }
    }

    /// `header_register_callback()` (`Z/main/SAPI.c:98`): the callable
    /// held, with the owners `zend_fcc_addref` takes, for
    /// `sapi_send_headers` to serve once; one held before is given up.
    pub fn set_header_callback(&mut self, callback: Callable) {
        callback.addref(self);
        if let Some(previous) = self.header_callback.replace(callback) {
            previous.release(self);
        }
    }

    /// `SG(sapi_headers).http_response_code` as it stands: 200 under a
    /// web host and zero under the CLI, which `http_response_code()`
    /// answers `false` for.
    #[must_use]
    pub fn response_code(&self) -> i64 {
        self.output.response_code()
    }

    /// `http_response_code(int)`'s write, the code alone
    /// (`Z/ext/standard/head.c:399`); the checks before it are the
    /// native's.
    pub fn set_response_code(&mut self, code: i64) {
        self.output.set_response_code(code);
    }

    /// The request headers the host received, `getallheaders()`, as
    /// strings of the request's.
    pub fn request_headers(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        let headers = self.description.headers.clone();
        for (name, value) in &headers {
            let key = Key::Str(PhpString::new(self, name)?);
            let value = Value::string(PhpString::new(self, value)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    // -------------------------------------------------- include

    /// `php_execute_script_ex` (`Z/main/main.c:2617`): the primary file
    /// into `EG(included_files)` under its expanded path before it runs,
    /// so a once form over the file being served answers `true`. A source
    /// the host handed over, `php -r` and stdin, has no path and is
    /// nowhere in the table, as php's `Standard input code` is not.
    pub(crate) fn note_primary_file(&mut self, script: &[u8]) {
        if let Some(HostPath(real)) = self.realpath(script) {
            self.included_files
                .push(real.as_os_str().as_encoded_bytes().to_vec());
        }
    }

    /// `zend_include_or_eval` (`Z/Zend/zend_execute.c:5287`) for the four
    /// file constructs: the once forms resolve the path first
    /// ([`Request::resolve_path`]) and answer [`Opened::Already`] for one
    /// the table has, then put the path in the table before the file is
    /// compiled, so a file whose compile is refused is in the table
    /// still; the other two compile first, `compile_filename`, and put
    /// the path in the table after (`Z/Zend/zend_language_scanner.l:709`).
    /// The file is compiled once and published through the unit cache
    /// (design §6.3), as opcache keeps it, and every request after
    /// reads the same unit.
    ///
    /// A path nothing opens is `zend_stream_open`'s warning, the stream
    /// layer's `Failed to open stream` with the construct and the path
    /// as written before it (`php_verror`, `Z/main/main.c:1096`), then
    /// `zend_message_dispatcher`'s (`Z/main/main.c:1746`): the warning
    /// `Failed opening '%s' for inclusion` and [`Opened::Failed`] under
    /// an `include`, php's `Error` `Failed opening required '%s'` under
    /// a `require`, each naming `include_path` as the ini has it. A
    /// path with a NUL byte in it is the second message alone.
    ///
    /// A file the compiler refuses is php's `ParseError`, thrown at the
    /// file and line the parser stopped at, which the includer may
    /// catch; what the compiler raised before it was refused is raised
    /// first. A refusal after the parse, `E_COMPILE_ERROR`, ends the
    /// request as it ends one for the primary file.
    pub(crate) fn include_file(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
    ) -> Result<Opened, Thrown> {
        if written.contains(&0) {
            return self.failed_opening(kind, written);
        }
        let resolved = self.resolve_path(written);
        if kind.once()
            && let Ok(path) = &resolved
            && self.included_files.contains(path)
        {
            return Ok(Opened::Already);
        }
        let path = match resolved {
            Ok(path) if !is_wrapper_url(&path) => path,
            // php's `resolved_path = zend_string_copy(inc_filename)` for a
            // path it could not resolve, which a stream wrapper opens.
            Ok(path) => return self.include_stream(kind, written, &path),
            Err(_) if is_wrapper_url(written) => {
                if kind.once() && self.included_files.iter().any(|p| p == written) {
                    return Ok(Opened::Already);
                }
                return self.include_stream(kind, written, written);
            }
            Err(reason) => {
                self.open_stream_failed(kind, written, &reason);
                return self.failed_opening(kind, written);
            }
        };
        if kind.once() {
            self.included_files.push(path.clone());
        }
        let cwd = self.cwd.clone();
        let stack = self.stack_limit();
        match self.shared.cached(&path, &cwd, stack) {
            Some(Ok(unit)) => {
                if !self.included_files.contains(&path) {
                    self.included_files.push(path);
                }
                Ok(Opened::Unit(unit))
            }
            Some(Err(refused)) => Err(self.refused_include(&refused, &path)),
            // The file went between the stat that resolved it and the
            // read that would have compiled it.
            None => {
                let reason = std::io::Error::from(std::io::ErrorKind::NotFound);
                self.open_stream_failed(kind, written, &reason);
                self.failed_opening(kind, written)
            }
        }
    }

    /// An include of a path a stream wrapper serves, `zend_stream_open`'s
    /// `php_stream_open_for_zend_ex` under `USE_PATH | REPORT_ERRORS |
    /// STREAM_OPEN_FOR_INCLUDE`: the stream opened as its wrapper opens it,
    /// the warnings raised under it naming the construct, read whole, and
    /// compiled as a file under the path, a unit of the request's own
    /// that nothing publishes, since opcache keeps plain files alone. The
    /// path goes in the included-files table as the construct puts a
    /// file's there, before the compile for the once forms and after it
    /// for the others.
    ///
    /// ```php
    /// class M { public $context; private $s;
    ///     function stream_open($p, $m, $o, &$op) { $this->s = '<?php echo __FILE__, " ", ' . strlen($p) . ', "\n"; return 7;'; return true; }
    ///     function stream_read($n) { $r = $this->s; $this->s = ''; return $r; }
    ///     function stream_eof() { return $this->s === ''; }
    ///     function stream_stat() { return []; }
    ///     function stream_set_option($o, $a, $b) { return false; }
    ///     function url_stat($p, $f) { return []; } }
    /// stream_wrapper_register('mm', 'M');
    /// var_dump(include 'mm://one', include_once 'mm://two', include_once 'mm://two');
    /// set_include_path('mm://dir:' . get_include_path());
    /// var_dump(include 'three');
    /// set_include_path('.');
    /// set_error_handler(function ($no, $msg) { echo $msg, "\n"; return true; });
    /// var_dump(include 'data:,x');
    /// ```
    /// ```expect
    /// mm://one 8
    /// mm://two 8
    /// int(7)
    /// int(7)
    /// bool(true)
    /// mm://dir/three 14
    /// int(7)
    /// include(): data:// wrapper is disabled in the server configuration by allow_url_include=0
    /// include(data:,x): Failed to open stream: no suitable wrapper could be found
    /// include(): Failed opening 'data:,x' for inclusion (include_path='.')
    /// bool(false)
    /// ```
    fn include_stream(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
        path: &[u8],
    ) -> Result<Opened, Thrown> {
        if kind.once() {
            self.included_files.push(path.to_vec());
        }
        let options = crate::streams::OpenOptions {
            use_path: true,
            for_include: true,
            ..crate::streams::OpenOptions::REPORT
        };
        let outer = self.including.replace((kind, self.vm.call_depth()));
        let opened = self.open_wrapper_ex(path, b"rb", options);
        let source = match &opened {
            Ok(Some(stream)) => {
                // "Disable buffering to avoid double buffering between PHP
                // and Zend streams."
                let unbuffered = StreamOption::ReadBuffer {
                    buffered: false,
                    size: php_heap::BUFSIZ,
                };
                stream.set_option(self, unbuffered);
                let source = self
                    .stream_op(*stream, |req, s| s.copy_to_mem(req, None))
                    .unwrap_or_default();
                stream.close(self);
                Some(source)
            }
            _ => None,
        };
        self.including = outer;
        opened?;
        let Some(source) = source else {
            return self.failed_opening(kind, written);
        };
        if let Some(thrown) = self.stream_thrown.take() {
            return Err(thrown);
        }
        let shared = Arc::clone(&self.shared);
        let unit = match shared.compile_for(self, &source, path, SourceKind::File) {
            Ok(unit) => unit,
            Err(refused) => return Err(self.refused_include(&refused, path)),
        };
        if !self.included_files.iter().any(|p| p == path) {
            self.included_files.push(path.to_vec());
        }
        Ok(Opened::Unit(unit))
    }

    /// `zend_include_or_eval`'s `ZEND_EVAL` arm
    /// (`Z/Zend/zend_execute.c:5354`): the string compiled as
    /// `compile_string` compiles it, from after an open tag and ending in
    /// `RETURN null` ([`SourceKind::Eval`]), under the name
    /// `zend_make_compiled_string_description` gives it, the file and
    /// line the `eval` stands at and `: eval()'d code`, which is what
    /// `__FILE__` folds to inside it and what a diagnostic raised in it
    /// names. The unit is this request's own, compiled into its memory
    /// and never published ([`Shared::compile_for`]). An empty string is
    /// `compile_string`'s `NULL` with nothing raised, which the handler
    /// answers `false` for ([`Opened::Failed`]); a string the compiler
    /// refuses is raised as an included file's refusal is, at the name
    /// the unit would have carried.
    ///
    /// ```php
    /// eval("echo __FILE__, ' ', __LINE__, \"\\n\";\necho \$nope;");
    /// ```
    /// ```expect
    /// %s(2) : eval()'d code 1
    ///
    /// Warning: Undefined variable $nope in %s(2) : eval()'d code on line 2
    /// ```
    ///
    /// A parse error in the string is the `ParseError` `zendparse`
    /// throws, at the string's name and the line it stopped on, which
    /// the caller may catch; one nothing catches ends the request as
    /// php's `Parse error`. A refusal after the parse is the
    /// `E_COMPILE_ERROR` that ends the request.
    ///
    /// ```php
    /// try { eval("\n1 +;"); } catch (ParseError $e) { echo get_class($e), ": ", $e->getMessage(), " in ", $e->getFile(), ":", $e->getLine(), "\n"; }
    /// eval('$a = ;');
    /// ```
    /// ```expect exit=255
    /// ParseError: syntax error, unexpected token ";" in %s(2) : eval()'d code:2
    ///
    /// Parse error: syntax error, unexpected token ";" in %s(3) : eval()'d code on line 1
    /// ```
    ///
    /// ```php
    /// eval('break;');
    /// ```
    /// ```expect exit=255
    /// Fatal error: 'break' not in the 'loop' or 'switch' context in %s(2) : eval()'d code on line 1
    /// ```
    ///
    /// The unit lives as long as what holds it: the frame while the
    /// string runs, and after it the functions, classes and closures
    /// the string declared, which the request keeps as it keeps a file's.
    ///
    /// ```php
    /// $f = eval('function g() { return "g"; } class K { const C = "k"; } return fn() => "closure";');
    /// var_dump(g(), K::C, $f(), function_exists('g'));
    /// ```
    /// ```expect
    /// string(1) "g"
    /// string(1) "k"
    /// string(7) "closure"
    /// bool(true)
    /// ```
    pub(crate) fn eval_string(&mut self, code: &[u8]) -> Result<Opened, Thrown> {
        if code.is_empty() {
            return Ok(Opened::Failed);
        }
        let mut name = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        name.extend_from_slice(format!("({}) : eval()'d code", self.current_line()).as_bytes());
        let shared = Arc::clone(&self.shared);
        match shared.compile_for(self, code, &name, SourceKind::Eval) {
            Ok(unit) => Ok(Opened::Unit(unit)),
            Err(refused) => Err(self.refused_include(&refused, &name)),
        }
    }

    /// `php_resolve_path` (`Z/main/fopen_wrappers.c:500`) over the ini's
    /// `include_path`: a path with a scheme is resolved by its wrapper,
    /// the plain files wrapper's to its realpath and any other's to
    /// nothing; a path that begins `./` or `../`, or is absolute, or is
    /// asked with no `include_path` at all, is `tsrm_realpath` over
    /// itself; any other is tried under each entry of `include_path` in
    /// turn, an entry that is a stream wrapper's URL (whose `://` is no
    /// separator) found by that wrapper's `url_stat` and answered as the
    /// URL, and last under the directory of the file being served
    /// (`zend_get_executed_filename`), which is how a file beside the
    /// primary file is found from another working directory.
    /// `tsrm_realpath` is the kernel's canonical path against the
    /// request's working directory ([`Request::realpath`]), and here a
    /// regular file alone, which is the `S_ISREG` the plain-files wrapper
    /// holds an include to when it opens; php reports a directory with
    /// whatever `errno` the failed open left, and this engine as a file
    /// that is not there. The `Err` is the last attempt's reason, which
    /// the stream warning prints.
    pub(crate) fn resolve_path(&mut self, written: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        let not_found = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let for_include = crate::streams::OpenOptions {
            for_include: true,
            report: false,
            ..crate::streams::OpenOptions::default()
        };
        if scheme_length(written).is_some() {
            return match self.locate_for(written, for_include) {
                (crate::streams::Located::Plain, local) => self.realpath_file(&local),
                _ => Err(not_found()),
            };
        }
        let include_path = self.ini_get(b"include_path").unwrap_or_default().to_vec();
        let dot_relative = written.starts_with(b"./") || written.starts_with(b"../");
        if dot_relative || written.starts_with(b"/") || include_path.is_empty() {
            return self.realpath_file(written);
        }
        let mut last = not_found();
        let mut entries: Vec<Vec<u8>> = include_path_entries(&include_path);
        if let Some(file) = self.current_file()
            && let Some(slash) = file.iter().rposition(|b| *b == b'/')
            && slash > 0
        {
            entries.push(file[..slash].to_vec());
        }
        for entry in entries {
            let mut candidate = entry.clone();
            candidate.push(b'/');
            candidate.extend_from_slice(written);
            if scheme_length(&entry).is_some() {
                match self.locate_for(&candidate, for_include) {
                    (crate::streams::Located::Nothing, _) => continue,
                    (crate::streams::Located::Plain, local) => match self.realpath_file(&local) {
                        Ok(path) => return Ok(path),
                        Err(reason) => last = reason,
                    },
                    _ => {
                        if matches!(self.url_stat_quiet(&candidate, false), Ok(Some(_))) {
                            return Ok(candidate);
                        }
                    }
                }
                continue;
            }
            match self.realpath_file(&candidate) {
                Ok(path) => return Ok(path),
                Err(reason) => last = reason,
            }
        }
        Err(last)
    }

    /// `tsrm_realpath` of a path that is a regular file: the canonical
    /// path, against the request's working directory where the path is
    /// relative.
    fn realpath_file(&mut self, path: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        // `virtual_file_ex` sets `errno = ENOENT` for a path it could not
        // resolve.
        const ENOENT: i32 = 2;
        let real = self
            .realpath(path)
            .ok_or_else(|| std::io::Error::from_raw_os_error(ENOENT))?
            .0;
        if !real.is_file() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        Ok(real.as_os_str().as_encoded_bytes().to_vec())
    }

    /// `php_stream_open_wrapper`'s warning under `REPORT_ERRORS`, the
    /// `Failed to open stream: %s` of `php_stream_display_wrapper_errors`
    /// with `strerror(errno)`, raised through `php_error_docref1` with
    /// the construct and the path as written as its origin.
    fn open_stream_failed(&mut self, kind: IncludeKind, written: &[u8], reason: &std::io::Error) {
        let reason = reason.to_string();
        let reason = reason
            .split_once(" (os error")
            .map_or(reason.as_str(), |(text, _)| text);
        let message = format!(
            "{}({}): Failed to open stream: {reason}",
            kind.name(),
            String::from_utf8_lossy(written)
        );
        crate::api::raising::warning(self, &message);
    }

    /// `php_message_handler_for_zend`'s `ZMSG_FAILED_INCLUDE_FOPEN` and
    /// `ZMSG_FAILED_REQUIRE_FOPEN` (`Z/main/main.c:1746`): the warning
    /// and `false` for an `include`, the `Error` for a `require`, each
    /// naming the path as written and `include_path` as the ini has it.
    fn failed_opening(&mut self, kind: IncludeKind, written: &[u8]) -> Result<Opened, Thrown> {
        let include_path =
            String::from_utf8_lossy(self.ini_get(b"include_path").unwrap_or_default()).into_owned();
        let shown = String::from_utf8_lossy(written).into_owned();
        if kind.required() {
            return Err(crate::api::raising::throw(
                self,
                "Error",
                &format!("Failed opening required '{shown}' (include_path='{include_path}')"),
            ));
        }
        crate::api::raising::warning(
            self,
            &format!(
                "{}(): Failed opening '{shown}' for inclusion (include_path='{include_path}')",
                kind.name()
            ),
        );
        Ok(Opened::Failed)
    }

    /// What the compiler refused an included file with, raised as php
    /// raises it: the diagnostics before the refusal at the file's own
    /// name, then a parse error as the `ParseError` `zendparse` throws,
    /// at the file and the line the parser stopped on, and a refusal
    /// after the parse as the `E_COMPILE_ERROR` that ends the request.
    fn refused_include(&mut self, refused: &CompileError, path: &[u8]) -> Thrown {
        for raised in &refused.raised {
            self.note_error_in(raised.level, &raised.message, path, raised.line);
        }
        if refused.level == Level::Parse {
            return crate::api::raising::throw_at(
                self,
                "ParseError",
                &refused.message,
                path,
                refused.line,
            );
        }
        self.end_in_compile_time(refused.level, &refused.message, path, refused.line);
        Thrown::of(None)
    }

    // -------------------------------------------------- diagnostics

    /// `error_get_last()`: the last diagnostic as php's four-key array,
    /// its strings the request's.
    pub fn last_error(&mut self) -> Result<Option<PhpArray>, Thrown> {
        let Some((level, message, file, line)) = self.last_error.clone() else {
            return Ok(None);
        };
        let mut array = PhpArray::new(self)?;
        for (name, value) in [
            (&b"type"[..], Value::int(level.bit())),
            (
                b"message",
                Value::string(PhpString::new(self, message.as_bytes())?),
            ),
            (b"file", Value::string(PhpString::new(self, &file)?)),
            (b"line", Value::int(i64::from(line))),
        ] {
            let key = Key::Str(PhpString::new(self, name)?);
            array.set(self, key, value)?;
        }
        Ok(Some(array))
    }

    /// `error_clear_last()`.
    pub fn clear_last_error(&mut self) {
        self.last_error = None;
    }

    /// `error_log()` to the host's log: `php_log_err` with the message
    /// as given, one entry.
    pub fn error_log(&mut self, message: &[u8]) {
        self.log_err(message);
    }

    /// `EG(error_reporting)` as it stands: what `error_reporting()`
    /// answers and what every diagnostic is masked by.
    #[must_use]
    pub fn error_reporting(&self) -> i64 {
        self.error_reporting
    }

    /// `EG(error_reporting) = level`, the store `BEGIN_SILENCE` and
    /// `END_SILENCE` make and the unwinder makes for a `ZEND_LIVE_SILENCE`
    /// range: the word alone, the ini entry left where it stands.
    pub(crate) fn set_error_reporting(&mut self, level: i64) {
        self.error_reporting = level;
    }

    /// The write `error_reporting(int $error_level)` makes
    /// (`Z/Zend/zend_builtin_functions.c:470-497`): the entry's value
    /// replaced by the number's decimal text, `zend_long_to_str`, past
    /// its `on_modify`, and the word set to the number.
    pub(crate) fn alter_error_reporting(&mut self, level: i64) {
        self.ini.retain(|(n, _)| n.as_slice() != b"error_reporting");
        self.ini
            .push((b"error_reporting".to_vec(), level.to_string().into_bytes()));
        self.error_reporting = level;
    }

    /// `OnUpdateErrorReporting` (`Z/Zend/zend.c:111`): the word read out
    /// of the `error_reporting` entry, `atoi` of its text, and `E_ALL`
    /// where the entry has no value. Made at request start and by
    /// `ini_set('error_reporting', ...)`.
    fn update_error_reporting(&mut self) {
        self.error_reporting = self.ini_error_reporting();
    }

    /// `INI_INT("error_reporting")`, `E_ALL` where the entry is unset:
    /// the level the entry says, which is what a fiber starts its body
    /// at (`zend_fiber_execute`), whatever an `@` around the `start`
    /// masked.
    pub(crate) fn ini_error_reporting(&self) -> i64 {
        self.ini_get(b"error_reporting").map_or(E_ALL, ini_atol)
    }

    /// `ZEND_FUNCTION(set_error_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1306`): the handler standing is
    /// pushed with its mask, `zend_stack_push` of both, and `handler`
    /// stands with `levels` as its mask; none leaves no handler and the
    /// mask where it was. The answer is the handler that stood, a view
    /// the stack keeps the owner of, which the native copies. The
    /// handler given is one more owner from here.
    pub fn set_error_handler(&mut self, handler: Option<Callable>, levels: i64) -> Option<Value> {
        let previous = self.error_handler.take();
        let answer = previous.as_ref().map(Callable::value);
        self.error_handlers
            .push((previous, self.error_handler_levels));
        if let Some(handler) = handler {
            self.addref(&handler.value());
            self.error_handler = Some(handler);
            self.error_handler_levels = levels;
        }
        answer
    }

    /// `ZEND_FUNCTION(restore_error_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1332`): the handler standing
    /// released and the top of the stack standing in its place with its
    /// mask; with the stack empty, no handler and the mask where it was.
    /// php answers `true` whatever it found.
    pub fn restore_error_handler(&mut self) -> bool {
        if let Some(handler) = self.error_handler.take() {
            self.release(handler.into_value());
        }
        if let Some((handler, levels)) = self.error_handlers.pop() {
            self.error_handler = handler;
            self.error_handler_levels = levels;
        }
        true
    }

    /// `EG(user_error_handler)` as `get_error_handler()` reads it: the
    /// value the handler was set from, a view, none while no handler
    /// stands.
    pub(crate) fn error_handler(&self) -> Option<Value> {
        self.error_handler.as_ref().map(Callable::value)
    }

    /// `ZEND_FUNCTION(set_exception_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1367`): the error handler's
    /// shape without a mask.
    pub fn set_exception_handler(&mut self, handler: Option<Callable>) -> Option<Value> {
        let previous = self.exception_handler.take();
        let answer = previous.as_ref().map(Callable::value);
        self.exception_handlers.push(previous);
        if let Some(handler) = handler {
            self.addref(&handler.value());
            self.exception_handler = Some(handler);
        }
        answer
    }

    /// `ZEND_FUNCTION(restore_exception_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1399`).
    pub fn restore_exception_handler(&mut self) -> bool {
        if let Some(handler) = self.exception_handler.take() {
            self.release(handler.into_value());
        }
        if let Some(handler) = self.exception_handlers.pop() {
            self.exception_handler = handler;
        }
        true
    }

    /// `EG(user_exception_handler)` as `get_exception_handler()` reads
    /// it, a view.
    pub(crate) fn exception_handler(&self) -> Option<Value> {
        self.exception_handler.as_ref().map(Callable::value)
    }

    /// `zend_shutdown_executor_values`' release of both handlers and
    /// both stacks (`Z/Zend/zend_execute_API.c:410-420`), and SPL's
    /// `RSHUTDOWN` of the autoload stack beside them, after the open
    /// resources are closed, so a resource's destructor that raises
    /// still reaches the handler that stood.
    pub(crate) fn free_user_handlers(&mut self) {
        let error = self.error_handler.take().into_iter().chain(
            std::mem::take(&mut self.error_handlers)
                .into_iter()
                .filter_map(|(handler, _)| handler),
        );
        let exception = self.exception_handler.take().into_iter().chain(
            std::mem::take(&mut self.exception_handlers)
                .into_iter()
                .flatten(),
        );
        let handlers: Vec<Callable> = error.chain(exception).collect();
        for handler in handlers {
            self.release(handler.into_value());
        }
        for loader in std::mem::take(&mut self.autoloaders) {
            loader.release(self);
        }
        // `PHP_RSHUTDOWN(basic)`'s `zend_llist_destroy` of
        // `BG(user_tick_functions)`, and `SG(callback_func)` where the
        // header block was never sent.
        for (callback, arguments) in std::mem::take(&mut self.tick_functions) {
            callback.release(self);
            for value in arguments {
                self.release(value);
            }
        }
        if let Some(callback) = self.header_callback.take() {
            callback.release(self);
        }
    }

    /// `zend_error_impl`'s user branch (`Z/Zend/zend.c:1527-1604`): the
    /// handler standing is handed the diagnostic where its mask has the
    /// level, and where the level is not one php holds unsafe to hand to
    /// user code, `E_ERROR`, `E_PARSE`, the two core levels and the two
    /// compile levels. The handler is taken down while it runs, so a
    /// diagnostic it raises reaches the built-in handler, and put back
    /// after unless it set one of its own. It is handed the level, the
    /// message, the file and the line; `true` says the diagnostic is
    /// served, which is every answer but `false` and a throw, which is
    /// left pending in the diagnostic's place. With a throwable already
    /// pending, php's `zend_call_function` returns before the call and
    /// the diagnostic is dropped, handled by nobody.
    fn user_error_handler(&mut self, level: Level, message: &str, file: &[u8], line: u32) -> bool {
        if self.error_handler.is_none()
            || self.error_handler_levels & level.bit() == 0
            || matches!(
                level,
                Level::Error
                    | Level::Parse
                    | Level::CoreError
                    | Level::CoreWarning
                    | Level::CompileError
                    | Level::CompileWarning
            )
        {
            return false;
        }
        if self.is_pending() {
            return true;
        }
        let Some(handler) = self.error_handler.take() else {
            return false;
        };
        let arguments = (|| -> Result<[Value; 4], Thrown> {
            Ok([
                Value::int(level.bit()),
                Value::string(PhpString::new(self, message.as_bytes())?),
                Value::string(PhpString::new(self, file)?),
                Value::int(i64::from(line)),
            ])
        })();
        let served = match arguments {
            Ok(arguments) => {
                let answer = crate::api::call::call(self, &handler, &arguments);
                for argument in arguments {
                    self.release(argument);
                }
                match answer {
                    Ok(value) => {
                        let fall_through = matches!(value.as_ref(), ValueRef::Bool(false));
                        self.release(value);
                        !fall_through
                    }
                    Err(thrown) => {
                        self.leave_pending(&thrown);
                        true
                    }
                }
            }
            Err(thrown) => {
                self.leave_pending(&thrown);
                true
            }
        };
        if self.error_handler.is_none() {
            self.error_handler = Some(handler);
        } else {
            self.release(handler.into_value());
        }
        served
    }

    /// `zend_user_exception_handler` (`Z/Zend/zend.c:1931`): the pending
    /// throwable handed to the handler standing, which is taken down
    /// onto the stack while it runs and stands again after unless it
    /// set one of its own. The throwable is released once the handler
    /// returns; what the handler throws is rendered as the uncaught
    /// fatal, since no handler stands to take it, which ends the
    /// request as php's `zend_throw_exception_internal` does with no
    /// frame standing. Nothing is done for the unwind exit.
    pub(crate) fn user_exception_handler(&mut self) {
        if self.exception_handler.is_none() {
            return;
        }
        let Some(throwable) = self.pending.take() else {
            return;
        };
        let handler = self.exception_handler.take();
        self.exception_handlers.push(handler.clone());
        if let Some(handler) = handler {
            match crate::api::call::call(self, &handler, &[throwable.as_value()]) {
                Ok(value) => self.release(value),
                Err(thrown) => {
                    self.leave_pending(&thrown);
                    if let Err(fatal) = self.uncaught() {
                        self.end_in(&fatal);
                    }
                }
            }
        }
        self.release(throwable.as_value());
        if self.exception_handler.is_none()
            && let Some(top) = self.exception_handlers.pop()
        {
            self.exception_handler = top;
        }
    }

    /// Whether a user handler for throwables stands, which is what
    /// decides whether a throwable the main op array did not catch is
    /// rendered in its frame or handed to the handler once the frame is
    /// left.
    pub(crate) fn has_exception_handler(&self) -> bool {
        self.exception_handler.is_some()
    }

    /// `assert_options(ASSERT_CALLBACK)` as it stands.
    #[must_use]
    pub fn assert_callback(&self) -> Option<&Value> {
        self.assert_callback.as_ref()
    }

    /// Set the assertion callback.
    pub fn set_assert_callback(&mut self, callback: Option<Value>) {
        self.assert_callback = callback;
    }

    /// `zend_fetch_debug_backtrace(return_value, 1, options, limit)`,
    /// what `debug_backtrace()` answers from inside the native being
    /// served: the rows below it, under php's options and limit.
    pub fn backtrace(&mut self, options: i64, limit: i64) -> Result<PhpArray, Thrown> {
        self.fetch_debug_backtrace(true, options, limit)
    }

    /// The file of the frame that called the native,
    /// `zend_get_executed_filename`.
    #[must_use]
    pub fn current_file(&self) -> Option<&[u8]> {
        self.innermost_frame()
            .and_then(|state| exec::body_of(&state.unit, state.body))
            .map(|code| code.filename.0.as_ref())
            .or(self.description.script_name.as_deref())
    }

    /// `zend_get_executed_lineno`: the line the innermost frame stands
    /// on, which is the line a diagnostic and a backtrace name.
    #[must_use]
    ///
    /// The line of the opline the innermost frame stands at, zero while
    /// no frame is being served; inside a fiber whose callable is a
    /// native, the resumer's frame's.
    pub fn current_line(&self) -> u32 {
        self.innermost_frame().map_or(0, exec::line_of)
    }

    /// Whether the call being served is a frameless one, a
    /// `FRAMELESS_ICALL_*` the compiler emitted for a declared arity: php
    /// serves such a call with the native's `ZEND_FRAMELESS_FUNCTION`
    /// body, which for `min` and `max` answers a tie differently from
    /// the framed body, so the native reads which one it is.
    #[must_use]
    pub fn frameless(&self) -> bool {
        self.vm.serving_call().is_some_and(|call| call.frameless)
    }

    /// `zend_hash_find_ind(zend_rebuild_symbol_table(), name)`: the
    /// variable `name` of the innermost user frame, the one that called
    /// the native, as it stands, a reference not followed; none where the
    /// frame has no such variable or it holds nothing. The frame's symbol
    /// table is rebuilt over its CVs first where it has none
    /// ([`Request::rebuild_symbol_table`]), as `extract` and `compact`
    /// rebuild it in php. A view: no owner taken.
    #[must_use]
    pub fn caller_var(&mut self, name: &[u8]) -> Option<Value> {
        let frame = self.caller_frame()?;
        let table = self.rebuild_symbol_table(frame);
        let symbol = self
            .table_mut(table)?
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, symbol)| match symbol {
                Symbol::Value(value) => Ok(*value),
                Symbol::Held(cell) => Ok(cell.get()),
                Symbol::Indirect(frame, at) => Err((*frame, *at)),
            })?;
        let value = match symbol {
            Ok(value) => value,
            Err((frame, at)) => self.slot(frame, at),
        };
        (value.tag() != TypeTag::Undef).then_some(value)
    }

    /// The variable `name` of the innermost user frame written with
    /// `value`, which is the frame's from here: a CV's slot where the
    /// table points the name at one, the bucket otherwise, a name the
    /// table has not added to it; what stood there before is released
    /// after the write, as `zend_hash_update` and `ZVAL_COPY_VALUE` into
    /// an `IS_INDIRECT` slot leave it. What `extract` writes; a write
    /// through a reference the variable holds is the native's to make.
    pub fn set_caller_var(&mut self, name: &[u8], value: Value) {
        let Some(frame) = self.caller_frame() else {
            self.release(value);
            return;
        };
        let table = self.rebuild_symbol_table(frame);
        let indirect = self.table_mut(table).and_then(|entries| {
            entries
                .iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, symbol)| match symbol {
                    Symbol::Indirect(frame, at) => Some((*frame, *at)),
                    Symbol::Value(_) | Symbol::Held(_) => None,
                })
        });
        match indirect {
            Some((frame, at)) => {
                let previous = self.set_slot(frame, at, value);
                self.release(previous);
            }
            None => self.symbol_update(table, name.to_vec(), value),
        }
    }

    /// The innermost user frame, the one a native's variable access
    /// reads: `EG(current_execute_data)`'s caller.
    fn caller_frame(&self) -> Option<FrameRef> {
        let count = self.vm.frames().len();
        let at = count.checked_sub(1)?;
        Some(FrameRef(u32::try_from(at).ok()?))
    }

    /// `ZEND_NUM_ARGS()`: how many arguments the call being served was
    /// given, and zero outside a native's body.
    #[must_use]
    pub fn argument_count(&self) -> usize {
        self.vm
            .serving_call()
            .map_or(0, |call| call.arguments.len())
    }

    /// `ZEND_CALL_ARG(execute_data, position)`: the argument at
    /// `position`, one-based as php counts them and as `Argument #N`
    /// names them, read off the call frame the native is being served
    /// from; none past what the call was given. What is read is the
    /// value as passed, a reference not followed, as `Z_PARAM_*` reads
    /// its slot before coercing it.
    #[must_use]
    pub fn argument(&self, position: u32) -> Option<Value> {
        let call = self.vm.serving_call()?;
        let index = usize::try_from(position.checked_sub(1)?).ok()?;
        call.arguments.get(index).copied()
    }

    /// `Z_PARAM_VARIADIC` (`Z/Zend/zend_API.h:2181`) from `position`
    /// on: the call's remaining arguments as they stand on the frame,
    /// each a view the call keeps one owner of, and none where the call
    /// gave fewer. A call that carries extra named arguments is
    /// `ZPP_ERROR_UNEXPECTED_EXTRA_NAMED`, php's `ArgumentCountError`
    /// from `zend_unexpected_extra_named_error`
    /// (`Z/Zend/zend_API.c:394`), since a variadic native that reads
    /// them by name reads [`Request::named_arguments`] instead. The
    /// witness stands beside `array_merge` in `ext/standard`, which is
    /// where the oracle's text is pinned.
    pub fn variadic_arguments(&mut self, position: u32) -> Result<Vec<Value>, Thrown> {
        let Some(call) = self.vm.serving_call() else {
            return Ok(Vec::new());
        };
        if !call.extra_named.is_empty() {
            let message = format!(
                "{}() does not accept unknown named parameters",
                self.active_native()
            );
            return Err(crate::api::raising::throw(
                self,
                "ArgumentCountError",
                &message,
            ));
        }
        let from = usize::try_from(position.saturating_sub(1)).unwrap_or(usize::MAX);
        Ok(call.arguments.get(from..).unwrap_or_default().to_vec())
    }

    /// `Z_PARAM_VARIADIC_WITH_NAMED`'s named half: the extra named
    /// arguments the call being served carries, `EX(extra_named_params)`
    /// under `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`, name and value in the
    /// order they were given, each a view the call keeps one owner of.
    /// What `call_user_func` hands on as its own call's named
    /// arguments; empty for a call that carries none.
    #[must_use]
    pub fn named_arguments(&self) -> Vec<(PhpString, Value)> {
        self.vm
            .serving_call()
            .map(|call| call.extra_named.clone())
            .unwrap_or_default()
    }

    /// The callee of the call being served, its receiver and its called
    /// scope, php's `EX(func)` and `EX(This)` inside a native.
    pub(crate) fn serving_callee(&self) -> Option<(Function, Option<PhpObject>, Option<ClassId>)> {
        self.vm
            .serving_call()
            .map(|call| (call.function, call.this, call.called_scope))
    }

    /// The arguments of the call being served and its extra named ones,
    /// each owner moved out to the caller and the call left with none.
    pub(crate) fn take_serving_arguments(&mut self) -> (Vec<Value>, Vec<(PhpString, Value)>) {
        match self.vm.serving_call_mut() {
            Some(call) => {
                call.count = 0;
                (
                    std::mem::take(&mut call.arguments),
                    std::mem::take(&mut call.extra_named),
                )
            }
            None => (Vec::new(), Vec::new()),
        }
    }

    /// The call being served handed to `function` with `arguments`,
    /// each owner the call's from here: what `ZEND_CALL_TRAMPOLINE`
    /// writes into the frame it reuses.
    pub(crate) fn serve_instead(&mut self, function: Function, arguments: Vec<Value>) {
        if let Some(call) = self.vm.serving_call_mut() {
            call.function = function;
            call.count = arguments.len();
            call.arguments = arguments;
        }
    }

    /// `zend_parse_arg_*_weak` writing what it coerced back into the
    /// argument's zval: the call frame gives up what it held at
    /// `position` and owns `value` from here, so the native reads a view
    /// and the frame frees the string with the call. A position past
    /// what the call was given writes nothing. A frameless call's
    /// argument is the opline's operand, which php's `Z_FLF_PARAM_STR`
    /// never writes: it converts a temporary of the body's and frees it
    /// at the body's end, so the frame keeps `value` beside the argument
    /// and a trace taken after the coercion prints the operand as passed.
    pub(crate) fn replace_argument(&mut self, position: u32, value: Value) {
        let Some(index) = position
            .checked_sub(1)
            .and_then(|at| usize::try_from(at).ok())
        else {
            return;
        };
        let Some(call) = self.vm.serving_call_mut() else {
            return;
        };
        if call.frameless {
            call.coerced.push(value);
            return;
        }
        let Some(previous) = call
            .arguments
            .get_mut(index)
            .map(|slot| std::mem::replace(slot, value))
        else {
            return;
        };
        self.release(previous);
    }

    /// What called the native being served, `EX(prev_execute_data)` as
    /// `func_get_args` and `get_defined_vars` read it: a user function's
    /// frame, with the arguments it was passed as its slots hold them
    /// now, the extra ones after, `ZEND_CALL_NUM_ARGS` of them in all;
    /// a code frame, the main op array or an included file's, which
    /// php's `ZEND_CALL_CODE` names; or another native, which is a call
    /// `zend_call_function` made and php's `ZEND_CALL_DYNAMIC`, as is a
    /// call whose callee the program named as it served
    /// ([`exec::CallState::dynamic`]).
    ///
    /// ```php
    /// function f() { $g = 'func_get_args'; return $g(); }
    /// function h() { $g = 'func_num_args'; return call_user_func($g); }
    /// function k() { return call_user_func('func_get_args'); }
    /// foreach (['f', 'h', 'k'] as $fn) { try { var_dump($fn(1)); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    /// ```
    /// ```expect
    /// Cannot call func_get_args() dynamically
    /// Cannot call func_num_args() dynamically
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// ```
    pub(crate) fn native_caller(&self) -> NativeCaller {
        let mut chain = self.chain();
        if let Some(Link::Call(served)) = chain.next()
            && served.dynamic
        {
            return NativeCaller::Native;
        }
        match chain.next() {
            // A frame on a stack below the running one is a fiber's
            // resumer: a native that is a fiber's own callable was called
            // from the fiber's bottom frame, which is `zend_fiber_function`,
            // a native's, so it answers as a native caller below.
            Some(Link::Frame(stack, place, state))
                if std::ptr::eq(stack, &raw const self.stack) && state.function.is_some() =>
            {
                let frame = FrameRef(u32::try_from(place).unwrap_or(u32::MAX));
                let passed = usize::try_from(state.num_args).unwrap_or(0);
                let mut arguments: Vec<Value> = (0..passed.saturating_sub(state.extra_args.len()))
                    .map(|slot| self.slot(frame, u32::try_from(slot).unwrap_or(u32::MAX)))
                    .collect();
                arguments.extend(state.extra_args.iter().copied());
                NativeCaller::Function(frame, arguments)
            }
            Some(Link::Frame(stack, place, _)) if std::ptr::eq(stack, &raw const self.stack) => {
                NativeCaller::Code(FrameRef(u32::try_from(place).unwrap_or(u32::MAX)))
            }
            Some(Link::Frame(..) | Link::Call(_)) | None => NativeCaller::Native,
        }
    }

    /// `zend_proptable_to_symtable(&EG(symbol_table), 1)`
    /// (`Z/Zend/zend_hash.c:3396`), what `FETCH_GLOBALS` hands the
    /// program as `$GLOBALS`: a copy of the global table, every name
    /// that has a value in the table's order, a CV the name is attached
    /// to read where it stands and skipped where it holds nothing, a
    /// name that reads as an integer that integer (`ZEND_HANDLE_NUMERIC_STR`),
    /// and a reference nothing else holds its value, as
    /// `zend_array_dup` unwraps one; each value one more owner.
    pub(crate) fn symbol_table_array(&mut self) -> Result<PhpArray, Thrown> {
        let entries: Vec<(Vec<u8>, Symbol)> = self.globals().clone();
        let mut array = PhpArray::new(self)?;
        for (name, symbol) in entries {
            let value = match symbol {
                Symbol::Value(value) => value,
                Symbol::Held(cell) => cell.get(),
                Symbol::Indirect(frame, slot) => self.slot(frame, slot),
            };
            if value.tag() == TypeTag::Undef {
                continue;
            }
            let value = match value.as_php_ref() {
                Some(cell) if value.refcount() == 1 => cell.get(),
                _ => value,
            };
            self.addref(&value);
            let key = Key::from_string(PhpString::new(self, &name)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `zend_rebuild_symbol_table` for `frame` and `zend_array_dup` of
    /// it: every name the frame's table holds that has a value, in the
    /// table's order, each value one more owner, a reference kept as
    /// one; what `get_defined_vars()` answers.
    pub(crate) fn defined_vars(&mut self, frame: FrameRef) -> Result<PhpArray, Thrown> {
        let table = self.rebuild_symbol_table(frame);
        let entries: Vec<(Vec<u8>, Symbol)> = self
            .tables
            .get(table.0 as usize)
            .map(|table| table.entries.clone())
            .unwrap_or_default();
        let mut array = PhpArray::new(self)?;
        for (name, symbol) in entries {
            let value = match symbol {
                Symbol::Value(value) => value,
                Symbol::Held(cell) => cell.get(),
                Symbol::Indirect(frame, slot) => self.slot(frame, slot),
            };
            if value.tag() == TypeTag::Undef {
                continue;
            }
            self.addref(&value);
            let key = Key::from_string(PhpString::new(self, &name)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// The arguments of the frame `depth` levels up, `func_get_args` from
    /// outside.
    #[must_use]
    pub fn frame_args(&self, depth: u32) -> Option<Vec<Value>> {
        let _ = depth;
        None
    }

    /// The variables of the frame `depth` levels up, `get_defined_vars`.
    #[must_use]
    pub fn frame_vars(&self, depth: u32) -> Option<PhpArray> {
        let _ = depth;
        None
    }

    /// `zend_get_executed_scope`, the class scope where the native being
    /// served was called: a native method's own class
    /// (`func->common.scope`), and for a native function the scope of
    /// the user code that called it, a method's class or a closure's
    /// bound scope; none under a plain function or the main op array.
    #[must_use]
    pub fn scope(&self) -> Option<ClassId> {
        self.executed_scope()
    }

    /// `zend_get_called_scope` (`Z/Zend/zend_execute_API.c:727`), the
    /// called scope of the native being served, `static`: the chain
    /// walked from `EG(current_execute_data)` to the first link with a
    /// receiver, whose class it is, or a called scope of its own; a
    /// user link or a native method without one ends the walk with
    /// none, and a native function without one, this native's own
    /// call, is walked past. So `static` inside a native is the
    /// calling method's, as `constant("static::X")` reads it.
    #[must_use]
    pub fn called_scope(&self) -> Option<ClassId> {
        for link in self.chain() {
            let (function, this, called) = match link {
                Link::Frame(_, _, state) => (state.function, state.this, state.called_scope),
                Link::Call(call) => (Some(call.function), call.this, call.called_scope),
            };
            if let Some(this) = this {
                return Some(this.class());
            }
            if called.is_some() {
                return called;
            }
            let function = function?;
            if self.user_function(function).is_some() || self.method_class(function).is_some() {
                return None;
            }
        }
        None
    }

    /// `ZEND_THIS`, `getThis()`: the receiver of the method being
    /// served, a name the frame holds an owner of while the call
    /// stands; none for a call on the class, a function, or outside a
    /// native's body.
    #[must_use]
    pub fn this(&self) -> Option<PhpObject> {
        if let Some(call) = self.vm.serving_call() {
            return call.this;
        }
        self.vm.innermost()?.this
    }

    // -------------------------------------------------- symbols

    /// The declaration of a linked class.
    #[must_use]
    pub fn class_decl(&self, class: ClassId) -> Option<&ClassDecl> {
        let at = usize::try_from(class.0).ok()?;
        match at.checked_sub(self.shared.classes.len()) {
            None => self.shared.classes.get(at),
            Some(declared) => self.declared.get(declared)?.decl(),
        }
    }

    /// `ZEND_FLF_FUNC(opline)`: the native and the arity the engine's
    /// frameless table holds at `at`, a `FRAMELESS_ICALL_*`'s
    /// `extended_value`; none past the table, which a unit this engine
    /// compiled never carries.
    pub(crate) fn frameless_entry(&self, at: u32) -> Option<FramelessEntry> {
        self.shared.frameless.get(at as usize).copied()
    }

    /// `zend_flf_count`: how many rows the frameless table has.
    pub(crate) fn frameless_count(&self) -> usize {
        self.shared.frameless.len()
    }

    /// The declaration of a linked function.
    #[must_use]
    pub fn function_decl(&self, function: Function) -> Option<&FunctionDecl> {
        let at = usize::try_from(function.0).ok()?;
        if at < self.shared.functions.len() {
            return self.shared.functions.get(at).map(|(decl, _)| decl);
        }
        if let Some(decl) = self.user_function(function).and_then(user_function_decl) {
            return Some(decl);
        }
        self.method_decl(function).map(|method| &method.function)
    }

    /// `get_declared_classes()`, in declaration order.
    #[must_use]
    pub fn declared_classes(&self) -> Vec<ClassId> {
        let count = self.shared.classes.len() + self.declared.len();
        (0..u32::try_from(count).unwrap_or(u32::MAX))
            .map(ClassId)
            .collect()
    }

    /// `get_defined_functions()` over `EG(function_table)`: `internal`,
    /// every native's key, and `user`, every user function's, each the
    /// lowered name the table is keyed by and in the order the table
    /// took them. A closure is no row, since php never adds one to the
    /// table; a method is none either, being its class's.
    pub fn defined_functions(&mut self) -> Result<PhpArray, Thrown> {
        let internal: Vec<Vec<u8>> = self
            .shared
            .functions
            .iter()
            .map(|(decl, _)| decl.name.0.to_ascii_lowercase())
            .collect();
        let user: Vec<Vec<u8>> = self
            .user_functions
            .iter()
            .filter_map(user_function_decl)
            .map(|decl| decl.name.0.to_ascii_lowercase())
            .filter(|name| !name.starts_with(b"{"))
            .collect();
        let mut array = PhpArray::new(self)?;
        for (list, names) in [(&b"internal"[..], internal), (b"user", user)] {
            let mut rows = PhpArray::new(self)?;
            for name in names {
                let name = Value::string(PhpString::new(self, &name)?);
                rows.append(self, name)?;
            }
            let key = Key::Str(PhpString::new(self, list)?);
            array.set(self, key, Value::array(rows))?;
        }
        Ok(array)
    }

    /// `EG(zend_constants)` in its order, each row the extension it was
    /// registered under, `ZEND_CONSTANT_MODULE_NUMBER`, by name, none for
    /// one the program defined, `PHP_USER_CONSTANT`: the engine's own
    /// under `Core`, then each extension's in bundle order, then what
    /// `define()` and `const` wrote. A value is a view, no owner taken;
    /// a declared constant whose value the engine did not publish is no
    /// row.
    pub(crate) fn constant_table(&self) -> Vec<(Option<&str>, Vec<u8>, Value)> {
        let decls = self.shared.declared().constants;
        let from_extensions: usize = self
            .shared
            .extensions
            .iter()
            .map(|e| e.constants.len())
            .sum();
        let own = decls.len().saturating_sub(from_extensions);
        let mut modules: Vec<&str> = vec!["Core"; own];
        for extension in &self.shared.extensions {
            modules.extend(std::iter::repeat_n(
                extension.name.as_str(),
                extension.constants.len(),
            ));
        }
        let mut rows: Vec<(Option<&str>, Vec<u8>, Value)> = decls
            .iter()
            .zip(modules)
            .flat_map(|(decl, module)| {
                // `PHP_SAPI` stands where php registers it, before
                // `PHP_BINARY`, and is the request's.
                let sapi = (decl.name.0.as_ref() == b"PHP_BINARY")
                    .then(|| (Some("Core"), b"PHP_SAPI".to_vec(), self.sapi_constant));
                let value = self.shared.constant(&decl.name.0);
                sapi.into_iter()
                    .chain(value.map(|value| (Some(module), decl.name.0.to_vec(), value)))
            })
            .collect();
        rows.extend(self.constants.iter().map(|row| {
            let module = match &row.owner {
                ConstantOwner::Core => Some("Core"),
                ConstantOwner::Extension(extension) => self.extension_name(*extension),
                ConstantOwner::User(_) => None,
            };
            (module, row.name.clone(), row.value)
        }));
        rows
    }

    /// `zend_register_class_alias_ex`'s `zend_hash_add` into
    /// `EG(class_table)`: `class` under the lowered name `alias`, and
    /// `false` where the table has that name already, a class's or
    /// another alias's, and nothing is added. The name is the caller's
    /// to have lowered and held to php's reserved names.
    pub fn class_alias(&mut self, class: ClassId, alias: &[u8]) -> bool {
        if self.class_named(alias).is_some() {
            return false;
        }
        let at = self.shared.classes.len() + self.declared.len();
        self.aliases.push((at, alias.to_ascii_lowercase(), class));
        true
    }

    /// `get_defined_constants()`: what `define()` declared in this
    /// request, each name a string of the request's.
    pub fn constants(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        let constants: Vec<(Vec<u8>, Value)> = self
            .constants
            .iter()
            .filter(|row| matches!(row.owner, ConstantOwner::User(_)))
            .map(|row| (row.name.clone(), row.value))
            .collect();
        for (name, value) in constants {
            let key = Key::Str(PhpString::new(self, &name)?);
            self.addref(&value);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `define()`: `false` when the constant exists. The row keeps the
    /// file executing when it was written, `zend_register_constant`'s
    /// `filename`.
    pub fn define_constant(&mut self, name: &[u8], value: Value) -> Result<bool, Thrown> {
        Ok(self.define_attributed_constant(name, value, Vec::new()))
    }

    /// `ZEND_DECLARE_ATTRIBUTED_CONST`'s `zend_register_constant` and
    /// `zend_constant_add_attributes`: [`Request::define_constant`] with
    /// the attributes the declaration carries hung on the row, and
    /// `CONST_DEPRECATED` raised from them. `false`, and nothing hung,
    /// where the name stands already.
    pub(crate) fn define_attributed_constant(
        &mut self,
        name: &[u8],
        value: Value,
        attributes: Vec<Attribute>,
    ) -> bool {
        if self.constant_named(name).is_some() {
            return false;
        }
        // `zend_get_executed_filename_ex`: the innermost frame's file,
        // none where nothing is executing.
        let file = self
            .innermost_frame()
            .and_then(|state| exec::body_of(&state.unit, state.body))
            .map(|code| code.filename.0.clone());
        let deprecated = crate::attributes::Flags::of(&attributes).deprecated;
        self.constants.push(ConstantRow {
            owner: ConstantOwner::User(file),
            name: name.to_vec(),
            value,
            attributes,
            deprecated,
        });
        true
    }

    /// `zend_constant.filename`: the file a constant the program
    /// defined was defined in, and none for one an extension or the
    /// engine registered, php's `NULL` for a persistent constant.
    ///
    /// ```php
    /// define('D', 1); const C = 2;
    /// var_dump((new ReflectionConstant('D'))->getFileName() === __FILE__, (new ReflectionConstant('C'))->getFileName() === __FILE__, (new ReflectionConstant('PHP_VERSION'))->getFileName());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    #[must_use]
    pub fn constant_file(&self, name: &[u8]) -> Option<&[u8]> {
        self.constants
            .iter()
            .find(|row| row.name == name)
            .and_then(|row| match &row.owner {
                ConstantOwner::User(file) => file.as_deref(),
                ConstantOwner::Core | ConstantOwner::Extension(_) => None,
            })
    }

    /// `zend_register_constant` at request start for a constant the
    /// host registers under `Core`, as `do_cli` registers
    /// `PHP_CLI_PROCESS_TITLE` and `cli_register_file_handles` the three
    /// streams: after every extension's own, so it stands last in
    /// `Core`'s group and before what the program defines. The value's
    /// one owner is the table's.
    pub(crate) fn register_host_constant(&mut self, name: &'static [u8], value: Value) {
        self.constants
            .push(ConstantRow::new(ConstantOwner::Core, name.to_vec(), value));
    }

    /// `REGISTER_STRINGL_CONSTANT` and its kin while a request runs:
    /// `name` registered under `extension`, php's `module_number`,
    /// carrying `attributes` as `zend_add_attribute` puts them on the
    /// constant, a `#[\Deprecated]` among them php's `CONST_DEPRECATED`,
    /// which a read raises. Where the request has registered the name
    /// already, by an extension or by `define()`, the row keeps its
    /// owner and attributes and takes `value`, the old value released,
    /// which is what an extension writes through the `zval` that
    /// `zend_get_constant_str` finds. `value` is the row's from here;
    /// `false`, and `value` still the caller's, for a name the engine or
    /// an extension declared as it started, which no request writes.
    /// The session's `SID` is the witness, in `ext/session`.
    pub fn register_constant(
        &mut self,
        extension: ExtensionId,
        name: &[u8],
        value: Value,
        attributes: Vec<Attribute>,
    ) -> bool {
        if let Some(row) = self.constants.iter_mut().find(|row| row.name == name) {
            let old = std::mem::replace(&mut row.value, value);
            self.release(old);
            return true;
        }
        if name == b"PHP_SAPI" || self.shared.constant(name).is_some() {
            return false;
        }
        let deprecated = crate::attributes::Flags::of(&attributes).deprecated;
        self.constants.push(ConstantRow {
            owner: ConstantOwner::Extension(extension),
            name: name.to_vec(),
            value,
            attributes,
            deprecated,
        });
        true
    }

    /// The fd the host handed under one of its standard streams
    /// ([`crate::engine::FileHandles`]), `None` where it handed none.
    pub(crate) fn host_fd(
        &self,
        standard: crate::streams::Standard,
    ) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.description.file_handles.as_ref()?.fd(standard)
    }

    /// Whether the host asked for the CLI's standard handles.
    pub(crate) fn registers_file_handles(&self) -> bool {
        self.description.file_handles.is_some()
    }

    /// `zend_constant::attributes`: the attributes the declaration of
    /// the global constant `name` carries, what an extension declared or
    /// a program's `DECLARE_ATTRIBUTED_CONST` hung on it, or an extension
    /// registered it with while the request runs
    /// ([`Request::register_constant`]), and the empty list for a name no
    /// table has. The key is the one
    /// `zend_register_constant` writes, the namespace lowered. What
    /// `ReflectionConstant::getAttributes()` reads, and what the
    /// deprecation a read raises is worded from
    /// ([`crate::api::raising::deprecated_constant`]).
    ///
    /// ```php
    /// #[MyAttribute(1)]
    /// const C = 1;
    /// define('D', 2);
    /// var_dump(count((new ReflectionConstant('C'))->getAttributes()),
    ///     count((new ReflectionConstant('D'))->getAttributes()),
    ///     count((new ReflectionConstant('E_STRICT'))->getAttributes()));
    /// ```
    /// ```expect
    /// int(1)
    /// int(0)
    /// int(1)
    /// ```
    #[must_use]
    pub fn constant_attributes(&self, name: &[u8]) -> &[Attribute] {
        if let Some(row) = self.constants.iter().find(|row| row.name == name) {
            return &row.attributes;
        }
        self.shared
            .declared()
            .constants
            .iter()
            .find(|decl| decl.name.0.as_ref() == name)
            .map_or(&[], |decl| decl.attributes.as_slice())
    }

    /// `ZEND_CONSTANT_FLAGS(c) & CONST_DEPRECATED` of the constant the
    /// table holds under `name`, and whether a program defined it,
    /// `PHP_USER_CONSTANT`: what a fetch reads before it words a
    /// deprecation. None for a name no table has.
    pub(crate) fn constant_deprecation(&self, name: &[u8]) -> Option<(bool, bool)> {
        if let Some(row) = self.constants.iter().find(|row| row.name == name) {
            let user = matches!(row.owner, ConstantOwner::User(_));
            return Some((row.deprecated, user));
        }
        self.shared
            .constant_deprecated(name)
            .map(|deprecated| (deprecated, false))
    }

    /// `spl_autoload_register()`: the callable onto the stack, at its
    /// head under `prepend`, and nothing where one of the same target
    /// is there already (`spl_find_registered_function`). The stack
    /// takes the callable's owners, [`Callable::addref`], as
    /// `autoload_func_info_from_fci` takes one of its object and closure,
    /// and gives them up when the callable leaves the stack or the
    /// request ends.
    /// A `Class::method` string registered with `$this` in scope is
    /// served on that object after the expression that made it is gone:
    ///
    /// ```php
    /// class Test {
    ///     function register() { spl_autoload_register('Test::load'); }
    ///     function load($name) { echo "load $name\n"; var_dump($this); }
    ///     function __destruct() { echo "destructed\n"; }
    /// }
    /// (new Test)->register();
    /// $a = [1, 2, 3];
    /// var_dump(class_exists('Missing'));
    /// echo "end\n";
    /// ```
    /// ```expect
    /// load Missing
    /// object(Test)#1 (0) {
    /// }
    /// bool(false)
    /// end
    /// destructed
    /// ```
    pub fn autoload_register(&mut self, loader: Callable, prepend: bool) -> Result<(), Thrown> {
        if self
            .autoloaders
            .iter()
            .any(|held| held.target() == loader.target())
        {
            return Ok(());
        }
        loader.addref(self);
        if prepend {
            self.autoloaders.insert(0, loader);
        } else {
            self.autoloaders.push(loader);
        }
        Ok(())
    }

    /// `spl_autoload_unregister()`.
    pub fn autoload_unregister(&mut self, loader: &Callable) -> bool {
        let Some(at) = self
            .autoloaders
            .iter()
            .position(|l| l.target() == loader.target())
        else {
            return false;
        };
        self.autoloaders.remove(at).release(self);
        true
    }

    /// The autoloader at `at` on the stack, what `spl_perform_autoload`
    /// serves next.
    pub(crate) fn autoloader(&self, at: usize) -> Option<Callable> {
        self.autoloaders.get(at).cloned()
    }

    /// `zend_hash_add_empty_element(EG(in_autoload), lc_name)`: `false`
    /// for a name an autoloader is loading already.
    pub(crate) fn autoload_enter(&mut self, lowered: &[u8]) -> bool {
        if self.in_autoload.iter().any(|name| name == lowered) {
            return false;
        }
        self.in_autoload.push(lowered.to_vec());
        true
    }

    /// `zend_hash_del(EG(in_autoload), lc_name)`.
    pub(crate) fn autoload_leave(&mut self, lowered: &[u8]) {
        self.in_autoload.retain(|name| name != lowered);
    }

    /// `spl_autoload_functions()`.
    #[must_use]
    pub fn autoload_functions(&self) -> Vec<Value> {
        self.autoloaders
            .iter()
            .cloned()
            .map(Callable::into_value)
            .collect()
    }

    /// `get_included_files()`: the paths, as `EG(included_files)` holds
    /// them, the primary file's first.
    #[must_use]
    pub fn included_files(&self) -> Vec<Vec<u8>> {
        self.included_files.clone()
    }

    /// `get_loaded_extensions()`: the bundle in `MINIT` order, after
    /// `Core`, each the name its `zend_module_entry` carries.
    #[must_use]
    pub fn loaded_extensions(&self) -> Vec<String> {
        std::iter::once(String::from("Core"))
            .chain(self.shared.extensions.iter().map(|e| e.name.clone()))
            .collect()
    }

    /// `zend_hash_str_find_ptr(&module_registry, lcname)`: the linked
    /// extension named, php's table being keyed by the lowered name;
    /// none for a name the bundle has not, and none for `Core` until
    /// the engine's own natives are an `Extension` value.
    #[must_use]
    pub fn extension_named(&self, name: &[u8]) -> Option<ExtensionId> {
        let wanted = name.to_ascii_lowercase();
        self.shared
            .extensions
            .iter()
            .position(|e| e.name.as_bytes().to_ascii_lowercase() == wanted)
            .and_then(|i| u32::try_from(i).ok())
            .map(ExtensionId)
    }

    /// `module->name`: the name an extension registered under, as its
    /// `zend_module_entry` spells it.
    #[must_use]
    pub fn extension_name(&self, extension: ExtensionId) -> Option<&str> {
        self.extension_of(extension).map(|e| e.name.as_str())
    }

    /// `module->module_number`, `zend_next_free_module` at registration:
    /// `Core` takes zero at `zend_startup` and each extension the next
    /// number in startup order, so an extension's is its position in
    /// the bundle plus one.
    #[must_use]
    pub fn extension_number(&self, extension: ExtensionId) -> u32 {
        extension.0.saturating_add(1)
    }

    /// The natives an extension declared, in the order it declared
    /// them: what `CG(function_table)` holds with
    /// `internal_function.module` naming it.
    #[must_use]
    pub fn extension_functions(&self, extension: ExtensionId) -> Vec<Function> {
        self.shared
            .function_modules
            .iter()
            .zip(0u32..)
            .filter(|(module, _)| **module == Some(extension))
            .map(|(_, at)| Function(at))
            .collect()
    }

    /// `internal_function.module`: the extension that declared a native
    /// function; none for a native the engine declares itself, for a
    /// method and for a user function.
    #[must_use]
    pub fn function_extension(&self, function: Function) -> Option<ExtensionId> {
        let at = usize::try_from(function.0).ok()?;
        self.shared.function_modules.get(at).copied().flatten()
    }

    /// `ce->info.internal.module`: the extension that declared a native
    /// class, `ReflectionClass::getExtension` reads it; none for a class
    /// Zend declares itself, whose `zend_module_entry` php has as `Core` and this
    /// engine as no `Extension` value yet, and none for a user class.
    #[must_use]
    pub fn class_extension(&self, class: ClassId) -> Option<ExtensionId> {
        let at = usize::try_from(class.0).ok()?;
        let zend = self
            .shared
            .classes
            .len()
            .saturating_sub(self.shared.extensions.iter().map(|e| e.classes.len()).sum());
        let mut start = zend;
        for (i, extension) in self.shared.extensions.iter().enumerate() {
            let end = start.saturating_add(extension.classes.len());
            if (start..end).contains(&at) {
                return u32::try_from(i).ok().map(ExtensionId);
            }
            start = end;
        }
        None
    }

    /// `ce->info.internal.module->name` as `_class_string` and
    /// `_function_string` print it after `<internal:`: the extension's
    /// name for a class an extension declared, `Core` for one Zend
    /// declares itself, whose `zend_module_entry` php has under that
    /// name and this engine as no `Extension` value yet (#320), and
    /// none for a user class.
    #[must_use]
    pub fn class_extension_name(&self, class: ClassId) -> Option<&str> {
        if let Some(extension) = self.class_extension(class) {
            return self.extension_name(extension);
        }
        let at = usize::try_from(class.0).ok()?;
        (at < self.shared.classes.len()).then_some("Core")
    }

    /// `ZEND_CONSTANT_MODULE_NUMBER(constant) == module->module_number`
    /// over `EG(zend_constants)`: the names of the constants an
    /// extension declared through `ExtensionBuilder::constants`, in the
    /// order it declared them, each one `api::call::constant` reads the
    /// value of; empty for an extension that declares none. The table
    /// holds the engine's own constants first, as
    /// `zend_register_standard_constants` runs before any `MINIT`, then
    /// each extension's in bundle order (`Engine::build`), the layout
    /// [`Request::extension_functions`] reads too.
    #[must_use]
    pub fn extension_constants(&self, extension: ExtensionId) -> Vec<Name> {
        let Some(at) = usize::try_from(extension.0).ok() else {
            return Vec::new();
        };
        let all: usize = self
            .shared
            .extensions
            .iter()
            .map(|e| e.constants.len())
            .sum();
        let own = self.shared.declared().constants.len().saturating_sub(all);
        let start: usize = self
            .shared
            .extensions
            .iter()
            .take(at)
            .map(|e| e.constants.len())
            .fold(own, usize::saturating_add);
        let count = self
            .extension_of(extension)
            .map_or(0, |e| e.constants.len());
        self.shared
            .declared()
            .constants
            .iter()
            .skip(start)
            .take(count)
            .map(|decl| decl.name.clone())
            .collect()
    }

    /// `module->deps`: the rows an extension declared through
    /// `ExtensionBuilder::dependencies`, in their order; the empty
    /// table where php's pointer is null, which is every extension that
    /// declares none.
    #[must_use]
    pub fn extension_dependencies(&self, extension: ExtensionId) -> Vec<Dependency> {
        self.extension_of(extension)
            .map(|e| e.dependencies.clone())
            .unwrap_or_default()
    }

    /// The ini entries an extension declared, in the order it declared
    /// them, which is where `zend_register_ini_entries` put them in
    /// `EG(ini_directives)`: each with the value in force, the request's
    /// `ini_set` over the engine's startup value, and the startup value
    /// again as `original` where the request changed it.
    #[must_use]
    pub fn extension_ini(&self, extension: ExtensionId) -> Vec<IniEntry> {
        let Some(module) = self.extension_of(extension) else {
            return Vec::new();
        };
        module
            .ini
            .iter()
            .map(|decl| self.ini_entry(decl.clone(), Some(extension)))
            .collect()
    }

    /// One entry as the request sees it: the value in force, the
    /// request's `ini_set` over the engine's startup value, and the
    /// startup value again as `original` where the request changed it.
    fn ini_entry(&self, decl: IniDecl, extension: Option<ExtensionId>) -> IniEntry {
        let name = decl.name.0.as_ref();
        let startup = self.shared.ini_default(name).unwrap_or(&decl.default);
        let changed = self.ini.iter().find(|(n, _)| n.as_slice() == name);
        let value = changed.map_or(startup, |(_, v)| v.as_slice()).into();
        let original = changed.map(|_| startup.into());
        IniEntry {
            decl,
            extension,
            value,
            original,
        }
    }

    /// `php_info_print_module` as the CLI prints it, `phpinfo_as_text`
    /// (`Z/ext/standard/info.c:133`): the name as a one-column table,
    /// then the extension's `MINFO`; for an extension that declared
    /// none, a `Version` row and `DISPLAY_INI_ENTRIES`, its ini entries
    /// under `Directive => Local Value => Master Value`
    /// (`Z/main/php_ini.c:115`), the master being the engine's default
    /// and an empty value `no value`. Every bundled extension has a
    /// version, `PHP_VERSION`, so php's other branch, the bare name, is
    /// never taken. What `ReflectionExtension::info` and `php --ri` print.
    pub fn extension_info(&mut self, extension: ExtensionId) {
        let Some(module) = self.extension_of(extension).cloned() else {
            return;
        };
        self.info_table(&[&[module.name.as_str()]]);
        if let Some(info) = module.info {
            info(self);
            return;
        }
        self.info_table(&[&["Version", PHP_VERSION]]);
        let names: Vec<&[u8]> = module.ini.iter().map(|entry| &*entry.name.0).collect();
        self.info_ini_entries(&names);
    }

    /// `DISPLAY_INI_ENTRIES()`, `display_ini_entries`
    /// (`Z/main/php_ini.c:115`), for the entries named, as an
    /// extension's `MINFO` ends with it: nothing for none, else a table
    /// under `Directive => Local Value => Master Value`, the master being
    /// the engine's default and an empty value `no value`.
    ///
    /// ```php
    /// @ini_set('iconv.output_encoding', 'ISO-8859-1');
    /// $e = new ReflectionExtension('iconv'); ob_start(); $e->info(); echo strstr(ob_get_clean(), 'Directive');
    /// ```
    /// ```expect
    /// Directive => Local Value => Master Value
    /// iconv.input_encoding => no value => no value
    /// iconv.output_encoding => ISO-8859-1 => no value
    /// iconv.internal_encoding => no value => no value
    /// ```
    pub fn info_ini_entries(&mut self, names: &[&[u8]]) {
        if names.is_empty() {
            return;
        }
        let no_value = || String::from("no value");
        let shown = |value: Option<&[u8]>| {
            value
                .filter(|v| !v.is_empty())
                .map_or_else(no_value, |v| String::from_utf8_lossy(v).into_owned())
        };
        let rows: Vec<[String; 3]> = names
            .iter()
            .map(|name| {
                [
                    String::from_utf8_lossy(name).into_owned(),
                    shown(self.ini_get(name)),
                    shown(self.shared.ini_default(name)),
                ]
            })
            .collect();
        let mut table: Vec<&[&str]> = vec![&["Directive", "Local Value", "Master Value"]];
        let cells: Vec<[&str; 3]> = rows
            .iter()
            .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
            .collect();
        table.extend(cells.iter().map(<[&str; 3]>::as_slice));
        self.info_table(&table);
    }

    /// `display_ini_entries(NULL)` (`Z/main/php_ini.c:131`) as the CLI
    /// prints it after `zend_ini_sort_entries`: the entries no extension
    /// declared, the engine's own, sorted by name, under `Directive =>
    /// Local Value => Master Value`, an empty value `no value`. What
    /// `PHP_MINFO_FUNCTION(core)` and `phpinfo()`'s `PHP Core` print.
    pub fn core_ini_info(&mut self) {
        let shared = Arc::clone(&self.shared);
        let owned = |name: &[u8]| {
            shared
                .extensions
                .iter()
                .any(|e| e.ini.iter().any(|entry| entry.name.0.as_ref() == name))
        };
        let mut names: Vec<&[u8]> = shared
            .ini_defaults()
            .iter()
            .map(|(name, _)| name.as_slice())
            .filter(|name| !owned(name))
            .collect();
        // `ini_key_compare`: `zend_binary_strcasecmp` of the names.
        names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        let shown = |value: Option<&[u8]>| {
            value.filter(|v| !v.is_empty()).map_or_else(
                || String::from("no value"),
                |v| String::from_utf8_lossy(v).into_owned(),
            )
        };
        let rows: Vec<[String; 3]> = names
            .iter()
            .map(|name| {
                [
                    String::from_utf8_lossy(name).into_owned(),
                    shown(self.ini_get(name)),
                    shown(shared.ini_default(name)),
                ]
            })
            .collect();
        let mut table: Vec<&[&str]> = vec![&["Directive", "Local Value", "Master Value"]];
        let cells: Vec<[&str; 3]> = rows
            .iter()
            .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
            .collect();
        table.extend(cells.iter().map(<[&str; 3]>::as_slice));
        self.info_table(&table);
    }

    /// `php_info_print_table_start` and one `php_info_print_table_row`
    /// per row as the CLI prints them, `phpinfo_as_text`
    /// (`Z/ext/standard/info.c:1062-1220`): a blank line, then each
    /// row's cells with ` => ` between and a newline after, an empty
    /// cell one space with no ` => ` after it, as php's loop has it.
    /// A header row prints the same way in text, so an extension's
    /// `MINFO` writes its whole table through this.
    pub fn info_table(&mut self, rows: &[&[&str]]) {
        let mut out = String::from("\n");
        for row in rows {
            let last = row.len().saturating_sub(1);
            for (i, cell) in row.iter().enumerate() {
                if cell.is_empty() {
                    out.push(' ');
                } else {
                    out.push_str(cell);
                    if i < last {
                        out.push_str(" => ");
                    }
                }
                if i == last {
                    out.push('\n');
                }
            }
        }
        self.write(out.as_bytes());
    }

    fn extension_of(&self, extension: ExtensionId) -> Option<&Extension> {
        self.shared
            .extensions
            .get(usize::try_from(extension.0).ok()?)
    }

    // -------------------------------------------------- state

    /// The request state of the calling extension, `T` being the type it
    /// declared with `request_state::<T>()` (ADR-0005). A type no bundled
    /// extension declared is built on first touch here, where the design
    /// names a bug; the linker's slot table replaces this scan.
    pub fn state<T: Default + Send + 'static>(&mut self) -> &mut T {
        let wanted = TypeId::of::<T>();
        let at = if let Some(at) = self.states.iter().position(|(id, _)| *id == wanted) {
            at
        } else {
            self.states.push((wanted, Box::new(T::default())));
            self.states.len() - 1
        };
        if self.states[at].1.downcast_ref::<T>().is_none() {
            self.states[at].1 = Box::new(T::default());
        }
        match self.states[at].1.downcast_mut::<T>() {
            Some(state) => state,
            // Written as a `T` on the line above; this arm is the type
            // system's, not a path a request takes.
            None => Box::leak(Box::new(T::default())),
        }
    }

    /// The engine state of the calling extension, `E` being the type its
    /// `engine_state` built (ADR-0005). `None` for a type no extension
    /// built, where the design names a bug.
    #[must_use]
    pub fn engine<E: Send + Sync + 'static>(&self) -> Option<&E> {
        self.shared
            .engine_states
            .iter()
            .find_map(|state| state.downcast_ref::<E>())
    }

    /// Every value of type `T` an extension of the bundle handed over
    /// with [`ExtensionBuilder::register`], in bundle order: what
    /// `pdo_driver_hash` holds after every `MINIT` of the bundle has
    /// registered into it, immutable for the engine's life
    /// (ADR-0005).
    ///
    /// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
    pub fn registered<T: Send + Sync + 'static>(&self) -> impl Iterator<Item = &T> {
        self.shared
            .extensions
            .iter()
            .flat_map(|e| e.registered.iter())
            .filter_map(|value| value.downcast_ref::<T>())
    }

    /// The trampoline `class` answers under the lowered name, a method
    /// its `get_method` synthesises and its table has no row for: one
    /// an extension laid down with [`ExtensionBuilder::trampolines`],
    /// which is what a class's own `get_method` hands back for it.
    ///
    /// [`ExtensionBuilder::trampolines`]: crate::api::abi::ExtensionBuilder::trampolines
    #[must_use]
    pub fn trampoline(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        self.trampoline_named(class, lowered)
    }

    /// The request's resource types, for an extension registering one.
    pub fn resources(&mut self) -> &mut Resources {
        &mut self.resources
    }

    /// `EG(regular_list)` in id order, every resource whose entry stands,
    /// closed or not: what `get_resources()` walks.
    pub(crate) fn resource_list(&self) -> Vec<Resource> {
        let live = self.heap.resources();
        let mut found = Vec::with_capacity(live);
        let mut id = 1;
        while found.len() < live && id < i64::MAX {
            if let Some(resource) = self.resource(id) {
                found.push(resource);
            }
            id += 1;
        }
        found
    }

    /// `zend_fetch_list_dtor_id`: the resource type registered under
    /// `name`, as `get_resources()` is given it.
    pub(crate) fn resource_type_named(&self, name: &[u8]) -> Option<ResourceType> {
        self.resources.type_named(name)
    }

    /// `zend_hash_index_find(&EG(regular_list), id)`: the resource with
    /// `id`, while its entry stands, closed or not.
    #[must_use]
    pub fn resource(&self, id: i64) -> Option<Resource> {
        self.heap.resource_at(id).map(Resource::of)
    }

    /// This request's interrupt (design §5.8): what the loop tests at a
    /// back-edge, at a frame's activation and after every
    /// `Flow::Resume`, and what the engine's execution timer and signal
    /// delivery raise. A `tier.lock` item, because compiled code tests
    /// the same atomic once per loop iteration through the tier's
    /// `ExitCtx` (`jit-design.md` §4.3).
    #[must_use]
    pub fn interrupt(&self) -> &Interrupt {
        &self.interrupt
    }

    // -------------------------------------------------- ini

    /// `ini_get()`: the request's override, else the engine's default.
    #[must_use]
    pub fn ini_get(&self, name: &[u8]) -> Option<&[u8]> {
        self.ini
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| v.as_slice())
            .or_else(|| self.shared.ini_default(name))
    }

    /// `ini_set()`: the previous value, or `None` for an entry that does
    /// not exist, may not change at runtime, or whose `on_modify` refuses
    /// the value ([`Request::ini_verdict`]). An entry with an on-modify
    /// handler runs it here: `max_execution_time` re-arms the execution
    /// timer (`OnUpdateTimeout`, `Z/main/main.c:467`).
    ///
    /// The entries of the table without `PHP_INI_USER`, `xmlrpc_errors`,
    /// `short_open_tag`, `sys_temp_dir`, the `mail.*` ones but `SMTP` and
    /// `smtp_port`, `sendmail_path`, two of the three `syslog.*` and
    /// `disable_functions`, are
    /// `PHP_INI_SYSTEM` or `PHP_INI_SYSTEM|PHP_INI_PERDIR`, which
    /// `zend_alter_ini_entry_ex` refuses at `PHP_INI_STAGE_RUNTIME` as it
    /// refuses every entry whose `modifiable` lacks the stage; the rest of
    /// the table is `PHP_INI_ALL`. An extension's entry is refused where
    /// its declaration lacks `PHP_INI_USER`, as `browscap` does.
    ///
    /// ```php
    /// var_dump(ini_set('xmlrpc_errors', '1'), ini_set('short_open_tag', '1'), ini_set('sendmail_path', 'x'), ini_set('mail.log', 'x'), ini_set('SMTP', 'mx'), ini_get('sendmail_path'), ini_get('mail.cr_lf_mode'), ini_set('browscap', 'x'), ini_set('assert.warning', '1'));
    /// echo NAN;
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// string(9) "localhost"
    /// string(24) "/usr/sbin/sendmail -t -i"
    /// string(4) "crlf"
    /// bool(false)
    /// string(1) "1"
    ///
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    /// NAN
    /// ```
    pub fn ini_set(&mut self, name: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
        let previous = self.ini_get(name).map(<[u8]>::to_vec);
        if previous.is_none() || !Request::ini_modifiable(name).0 || !self.ini_user(name) {
            return Ok(None);
        }
        if Request::ini_verdict(name, value) == IniVerdict::Rejected {
            return Ok(None);
        }
        if name == b"fiber.stack_size" && !crate::fiber::stack_size_updated(self, value) {
            return Ok(None);
        }
        if name == b"zend.assertions" && !self.assertions_updated(value) {
            return Ok(None);
        }
        let handler = self.shared.extensions.iter().find_map(|e| {
            e.on_modify
                .iter()
                .find(|(n, _)| n.as_slice() == name)
                .map(|(_, h)| *h)
        });
        if let Some(handler) = handler
            && !handler(self, value)?
        {
            return Ok(None);
        }
        self.ini.retain(|(n, _)| n.as_slice() != name);
        self.ini.push((name.to_vec(), value.to_vec()));
        match name {
            b"max_execution_time" => self.arm_timer(),
            b"memory_limit" => self.set_memory_limit(),
            b"error_reporting" => self.update_error_reporting(),
            b"zend.enable_gc" => {
                self.heap.gc_enable(ini_truth(Some(value)));
            }
            _ => {}
        }
        Ok(previous)
    }

    /// Whether an extension's entry is `PHP_INI_USER`, its declaration's
    /// `user`; an entry no extension declared is the core table's, which
    /// [`Request::ini_modifiable`] answers for.
    fn ini_user(&self, name: &[u8]) -> bool {
        self.shared
            .extensions
            .iter()
            .flat_map(|extension| extension.ini.iter())
            .find(|decl| decl.name.0.as_ref() == name)
            .is_none_or(|decl| decl.user)
    }

    /// A core entry's `modifiable` bits as [`IniDecl`] spells them,
    /// `PHP_INI_USER`, `PHP_INI_PERDIR` and `PHP_INI_SYSTEM`; the first
    /// is what `ini_set()` asks: every entry of the table is
    /// `PHP_INI_ALL` but `xmlrpc_errors`, `sys_temp_dir`,
    /// `sendmail_path`, `syslog.facility`, `syslog.ident`,
    /// `disable_functions`, `enable_dl` and `extension_dir`,
    /// `PHP_INI_SYSTEM` (`Z/main/main.c:806-890`), and
    /// `short_open_tag` and the four `mail.*` beside `sendmail_path`,
    /// `PHP_INI_SYSTEM|PHP_INI_PERDIR` (`Z/main/main.c:820-871`).
    ///
    /// `disable_functions` is read once, as the engine is built, so
    /// setting it from inside is refused and takes nothing out.
    ///
    /// ```php
    /// var_dump(ini_get('disable_functions'), ini_set('disable_functions', 'strlen'), function_exists('strlen'), strlen('abc'));
    /// ```
    /// ```expect
    /// string(0) ""
    /// bool(false)
    /// bool(true)
    /// int(3)
    /// ```
    fn ini_modifiable(name: &[u8]) -> (bool, bool, bool) {
        match name {
            b"xmlrpc_errors" | b"sys_temp_dir" | b"sendmail_path" | b"syslog.facility"
            | b"syslog.ident" | b"disable_functions" | b"enable_dl" | b"extension_dir"
            | b"allow_url_fopen" | b"allow_url_include" => (false, false, true),
            b"short_open_tag"
            | b"mail.add_x_header"
            | b"mail.mixed_lf_and_crlf"
            | b"mail.cr_lf_mode"
            | b"mail.log"
            | b"mail.force_extra_parameters" => (false, true, true),
            _ => (true, true, true),
        }
    }

    /// What a core entry's `on_modify` says of a new value before
    /// anything is written, `zend_alter_ini_entry_ex` asking it first
    /// (`Z/Zend/zend_ini.c:342`): `zend.exception_string_param_max_len`
    /// is `OnSetExceptionStringParamMaxLen` (`Z/Zend/zend.c:175-184`),
    /// which reads the text with `ZEND_ATOL` and refuses a number outside
    /// `0..=1000000`; the rest of the table accepts anything. A refusal
    /// keeps the entry as it was and `ini_set()` answers `false`.
    ///
    /// ```php
    /// var_dump(ini_set("zend.exception_string_param_max_len", "-1"), ini_set("zend.exception_string_param_max_len", "1000001"), ini_set("zend.exception_string_param_max_len", "3"), ini_get("zend.exception_string_param_max_len"));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(2) "15"
    /// string(1) "3"
    /// ```
    ///
    /// `ZEND_ATOL` is `atoll`, so text that is not a number is zero and
    /// accepted, and the entry keeps the text it was given.
    ///
    /// ```php
    /// var_dump(ini_set("zend.exception_string_param_max_len", "abc"), ini_get("zend.exception_string_param_max_len"), ini_set("zend.exception_string_param_max_len", "99999999999999999999"));
    /// ```
    /// ```expect
    /// string(2) "15"
    /// string(3) "abc"
    /// bool(false)
    /// ```
    fn ini_verdict(name: &[u8], value: &[u8]) -> IniVerdict {
        match name {
            b"zend.exception_string_param_max_len"
                if !(0..=1_000_000).contains(&ini_atol(value)) =>
            {
                IniVerdict::Rejected
            }
            _ => IniVerdict::Accepted,
        }
    }

    /// `EG(ini_directives)`, what `ini_get_all()` walks: the engine's
    /// own entries, registered with `module_number` zero before any
    /// `MINIT`, then each extension's in bundle order, each as
    /// [`Request::extension_ini`] reads one. A core entry's `modifiable`
    /// bits are the table's ([`Request::ini_modifiable`]).
    ///
    /// ```php
    /// ini_set('precision', '10');
    /// var_dump(ini_get_all(null)['precision'], ini_get_all(null)['enable_dl'], ini_get_all('pcre', false));
    /// ```
    /// ```expect
    /// array(3) {
    ///   ["global_value"]=>
    ///   string(2) "14"
    ///   ["local_value"]=>
    ///   string(2) "10"
    ///   ["access"]=>
    ///   int(7)
    /// }
    /// array(3) {
    ///   ["global_value"]=>
    ///   string(1) "1"
    ///   ["local_value"]=>
    ///   string(1) "1"
    ///   ["access"]=>
    ///   int(4)
    /// }
    /// array(3) {
    ///   ["pcre.backtrack_limit"]=>
    ///   string(7) "1000000"
    ///   ["pcre.jit"]=>
    ///   string(1) "1"
    ///   ["pcre.recursion_limit"]=>
    ///   string(6) "100000"
    /// }
    /// ```
    #[must_use]
    pub fn ini_entries(&self) -> Vec<IniEntry> {
        let owned = |name: &[u8]| {
            self.shared
                .extensions
                .iter()
                .any(|e| e.ini.iter().any(|entry| entry.name.0.as_ref() == name))
        };
        let core = self
            .shared
            .ini_defaults()
            .iter()
            .filter(|(name, _)| !owned(name))
            .map(|(name, default)| {
                let (user, per_dir, system) = Request::ini_modifiable(name);
                let decl = IniDecl {
                    name: Name(name.as_slice().into()),
                    default: default.as_slice().into(),
                    user,
                    per_dir,
                    system,
                };
                self.ini_entry(decl, None)
            });
        let extensions = (0..self.shared.extensions.len())
            .filter_map(|at| u32::try_from(at).ok())
            .flat_map(|at| self.extension_ini(ExtensionId(at)));
        core.chain(extensions).collect()
    }

    /// `zend_ini_parse_bool`: `1`, `on`, `yes`, `true` are true.
    #[must_use]
    pub fn ini_parse_bool(value: &[u8]) -> bool {
        let lower = value.to_ascii_lowercase();
        matches!(lower.as_slice(), b"1" | b"on" | b"yes" | b"true")
            || lower.iter().all(u8::is_ascii_digit)
                && String::from_utf8_lossy(&lower)
                    .parse::<i64>()
                    .is_ok_and(|n| n != 0)
    }

    /// `cfg_get_entry` as `get_cfg_var()` hands it back
    /// (`Z/ext/standard/basic_functions.c:1306`): the row of
    /// `configuration_hash` under `name`, what the host's ini text set
    /// before any entry read it, a string or the array its `name[]=`
    /// lines built, as strings of the request's; `None` where the text
    /// set nothing under the name, a default no text named included.
    ///
    /// ```php
    /// var_dump(get_cfg_var('precision'), get_cfg_var('highlight.string'), get_cfg_var('nope'));
    /// ```
    /// ```expect
    /// string(2) "14"
    /// bool(false)
    /// bool(false)
    /// ```
    pub fn cfg_entry(&mut self, name: &[u8]) -> Result<Option<Value>, Thrown> {
        let shared = Arc::clone(&self.shared);
        let Some(entry) = shared.configured(name) else {
            return Ok(None);
        };
        Ok(Some(match entry {
            Configured::Str(bytes) => Value::string(PhpString::new(self, bytes)?),
            Configured::Array(rows) => {
                let mut array = PhpArray::new(self)?;
                for (key, bytes) in rows {
                    let key = match key {
                        ConfiguredKey::Index(n) => Key::Int(*n),
                        ConfiguredKey::Name(name) => Key::Str(PhpString::new(self, name)?),
                    };
                    let value = Value::string(PhpString::new(self, bytes)?);
                    array.set(self, key, value)?;
                }
                Value::array(array)
            }
        }))
    }

    /// `zend_ini_parse_quantity`'s value alone, what `OnUpdateLong`
    /// stores for a whole-number entry: [`Request::ini_parse_quantity`]
    /// with the sentence it would say left unsaid.
    #[must_use]
    pub fn ini_parse_bytes(value: &[u8]) -> i64 {
        crate::ini_quantity::parse(value).0
    }

    /// `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`): leading
    /// and trailing whitespace ignored, an optional sign, a base prefix
    /// (`0x`, `0o`, `0b`) or `strtoull`'s own reading under base zero,
    /// where a leading `0` is octal, whitespace, and an optional `K`,
    /// `M` or `G` read off the last byte; with the value, the sentence
    /// php's `errstr` holds for a shape it interprets for backwards
    /// compatibility, which `ini_parse_quantity()` raises as a warning
    /// and an entry's `on_modify` as its own.
    ///
    /// ```php
    /// foreach (['1M', ' 2k ', '0x10', '010', '0b101', '-0x0B', '', '1.5M', 'abc', '0x', '0z1', '5 KB', '99999999999999999999'] as $q) var_dump(ini_parse_quantity($q));
    /// ```
    /// ```expect
    /// int(1048576)
    /// int(2048)
    /// int(16)
    /// int(8)
    /// int(5)
    /// int(-11)
    /// int(0)
    ///
    /// Warning: Invalid quantity "1.5M", interpreting as "1M" for backwards compatibility in %s on line 2
    /// int(1048576)
    ///
    /// Warning: Invalid quantity "abc": no valid leading digits, interpreting as "0" for backwards compatibility in %s on line 2
    /// int(0)
    ///
    /// Warning: Invalid quantity "0x": no digits after base prefix, interpreting as "0" for backwards compatibility in %s on line 2
    /// int(0)
    ///
    /// Warning: Invalid prefix "0z", interpreting as "0" for backwards compatibility in %s on line 2
    /// int(0)
    ///
    /// Warning: Invalid quantity "5 KB": unknown multiplier "B", interpreting as "5 " for backwards compatibility in %s on line 2
    /// int(5)
    ///
    /// Warning: Invalid quantity "99999999999999999999": value is out of range, using overflow result for backwards compatibility in %s on line 2
    /// int(-1)
    /// ```
    #[must_use]
    pub fn ini_parse_quantity(value: &[u8]) -> (i64, Option<String>) {
        crate::ini_quantity::parse(value)
    }

    // -------------------------------------------------- environment

    /// `getenv(name)`: the bytes the host's environment holds.
    #[must_use]
    pub fn env(&self, name: &[u8]) -> Option<&[u8]> {
        self.description
            .env
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, v)| v.as_slice())
    }

    /// `getenv()` with no name: every pair.
    #[must_use]
    pub fn env_pairs(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.description.env
    }

    /// `putenv()`: set, or unset with `None`.
    pub fn set_env(&mut self, name: &[u8], value: Option<&[u8]>) -> bool {
        self.description.env.retain(|(n, _)| n.as_slice() != name);
        if let Some(value) = value {
            self.description.env.push((name.to_vec(), value.to_vec()));
        }
        true
    }

    /// The superglobal named, `$_SERVER` and its kin.
    #[must_use]
    pub fn superglobal(&self, name: &[u8]) -> Option<&PhpArray> {
        self.superglobals
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, a)| a)
    }

    /// `php_sapi_name()`: `sapi_module.name`.
    #[must_use]
    pub fn sapi_name(&self) -> &str {
        &self.description.sapi_name
    }

    /// The host's `argv`, `php_self` first: what `php_build_argv` puts in
    /// `$_SERVER['argv']` (`Z/main/php_variables.c:657-713`), which a
    /// write to `$_SERVER` from PHP code leaves as it was, and which
    /// `getopt` reads; empty for a host that described none, php's web
    /// hosts.
    #[must_use]
    pub fn argv(&self) -> &[Vec<u8>] {
        &self.description.argv
    }

    /// The file the request serves, `$_SERVER['SCRIPT_NAME']`.
    #[must_use]
    pub fn script_name(&self) -> Option<&[u8]> {
        self.description.script_name.as_deref()
    }

    /// `php://stdin`, which is what the CLI's `STDIN` constant holds: the
    /// host's fd 0 where it handed one, else the body as the description
    /// carries it, read whole until #544 reads it lazily.
    pub fn stdin(&mut self) -> Result<Option<Stream>, Thrown> {
        self.open_stream(b"php://stdin", b"r", false)
    }

    /// The request body's bytes, `SG(request_info).request_body`.
    pub(crate) fn body_bytes(&self) -> &[u8] {
        &self.description.body
    }

    /// `le_stream`.
    pub(crate) const fn stream_type(&self) -> ResourceType {
        self.stream_type
    }

    /// The heap's memory limit in bytes, `PG(memory_limit)` as
    /// `zend_set_memory_limit` set it.
    pub(crate) fn heap_memory_limit(&self) -> usize {
        self.heap.memory_limit()
    }

    /// `getcwd()`.
    #[must_use]
    pub fn cwd(&self) -> HostPath {
        HostPath(self.cwd.clone())
    }

    /// `chdir()`.
    pub fn set_cwd(&mut self, path: HostPath) -> Result<(), Thrown> {
        self.cwd = path.0;
        self.forget_relative_stats();
        Ok(())
    }

    /// `sys_get_temp_dir()`.
    #[must_use]
    pub fn temp_dir(&self) -> HostPath {
        HostPath(std::env::temp_dir())
    }

    /// `umask()` as it stands.
    #[must_use]
    pub fn umask(&self) -> u32 {
        self.umask
    }

    /// `umask(int)`: the previous mask.
    pub fn set_umask(&mut self, mask: u32) -> u32 {
        std::mem::replace(&mut self.umask, mask)
    }

    /// `strerror` text for an OS error code, as php's `%s` of it: the
    /// system's sentence alone, without the ` (os error N)` Rust's
    /// rendering of an OS error puts after it.
    #[must_use]
    pub fn os_error_text(&self, code: i32) -> String {
        let text = std::io::Error::from_raw_os_error(code).to_string();
        let suffix = format!(" (os error {code})");
        match text.strip_suffix(&suffix) {
            Some(sentence) => sentence.to_owned(),
            None => text,
        }
    }

    /// `parse_str()`: a query string as an array, its strings the
    /// request's.
    pub fn parse_query(&mut self, query: &[u8]) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        for pair in query.split(|b| *b == b'&').filter(|p| !p.is_empty()) {
            let mut halves = pair.splitn(2, |b| *b == b'=');
            let name = halves.next().unwrap_or_default();
            let value = halves.next().unwrap_or_default();
            let key = Key::from_string(PhpString::new(self, name)?);
            let value = Value::string(PhpString::new(self, value)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `set_time_limit()`, which is `zend_alter_ini_entry_ex` over
    /// `max_execution_time` at runtime (`Z/main/main.c:1631`): the
    /// entry moved through [`Self::ini_set`], so the timer is re-armed
    /// as it is there, and `true` when the entry took the value.
    pub fn set_time_limit(&mut self, seconds: i64) -> bool {
        self.ini_set(b"max_execution_time", seconds.to_string().as_bytes())
            .is_ok_and(|previous| previous.is_some())
    }

    /// `register_shutdown_function()`, `append_user_shutdown_function`
    /// (`Z/ext/standard/basic_functions.c:1680-1735`): one entry appended
    /// at the table's next index, which is the order
    /// [`crate::natives::call_shutdown_functions`] serves them in, out of
    /// the request's heap as php lays it down ([`ShutdownFunctions`]). The
    /// callback and the arguments are the entry's owners from here,
    /// given up by [`Request::free_shutdown_functions`]; where the memory
    /// limit refuses the entry they are given up at once and the `Err` is
    /// that fatal.
    pub fn register_shutdown_function(
        &mut self,
        callback: Callable,
        arguments: Vec<Value>,
    ) -> Result<(), Thrown> {
        let (entry, mut table) = self.shutdown_entry(callback, arguments)?;
        let index = self.shutdown_functions.slot(entry);
        if let Err(thrown) = table.append(self, Value::int(index)) {
            self.drop_shutdown_entry(index);
            return Err(thrown);
        }
        Ok(())
    }

    /// `register_user_shutdown_function` and
    /// `remove_user_shutdown_function` (`Z/ext/standard/basic_functions.c:1705-1725`),
    /// the entry an extension keeps under a name of its own: `Some`
    /// replaces the entry of that name where it stands, which
    /// `zend_hash_str_update_mem` keeps in its place, or adds one; `None`
    /// removes it. The owners are the table's as
    /// [`Request::register_shutdown_function`]'s, and what an entry held
    /// is given up where it goes. The session's `session_shutdown` is the
    /// witness, in `ext/session`.
    pub fn set_shutdown_function(
        &mut self,
        name: &[u8],
        entry: Option<(Callable, Vec<Value>)>,
    ) -> Result<(), Thrown> {
        // `zend_hash_str_update_mem` keys by the bytes, never numerically.
        let string = PhpString::new(self, name)?;
        let key = Key::Str(string);
        let standing = self
            .shutdown_functions
            .table
            .and_then(|table| table.get(&key))
            .and_then(|index| index.as_int());
        let result = if let Some((callback, arguments)) = entry {
            self.set_named_shutdown_entry(key, callback, arguments)
        } else {
            let unset = match self.shutdown_functions.table {
                Some(mut table) => table.unset(self, &key).map(drop),
                None => Ok(()),
            };
            self.release(Value::string(string));
            unset
        };
        if let Some(index) = standing {
            self.drop_shutdown_entry(index);
        }
        result
    }

    /// `zend_hash_str_update_mem`: the entry made and written under
    /// `key`, whose owner the table takes, or given up with the key
    /// where the memory limit refuses it.
    fn set_named_shutdown_entry(
        &mut self,
        key: Key,
        callback: Callable,
        arguments: Vec<Value>,
    ) -> Result<(), Thrown> {
        let (entry, mut table) = match self.shutdown_entry(callback, arguments) {
            Ok(made) => made,
            Err(thrown) => {
                if let Key::Str(string) = key {
                    self.release(Value::string(string));
                }
                return Err(thrown);
            }
        };
        let index = self.shutdown_functions.slot(entry);
        let set = table.set(self, key, Value::int(index));
        if set.is_err() {
            self.drop_shutdown_entry(index);
        }
        set
    }

    /// The entry at or after the table's position `at`, as
    /// `zend_hash_apply` reaches it: the position it stands at, and its
    /// callback and arguments as views, the table keeping its owners, so
    /// the walk serves what it reads and an entry appended while it runs
    /// is reached by the same walk. `None` past the end is what ends it.
    pub(crate) fn shutdown_function(&self, at: u32) -> Option<(u32, Callable, Vec<Value>)> {
        let mut at = at;
        loop {
            let (position, _, index) = self.shutdown_functions.table?.live_from(at)?;
            if let Some(entry) = self.shutdown_functions.entry(&index) {
                return Some((position, entry.callback.clone(), entry.arguments.clone()));
            }
            at = position + 1;
        }
    }

    /// `php_free_shutdown_functions` (`Z/ext/standard/basic_functions.c:1660`),
    /// step 7 of `php_request_shutdown`, behind step 6 and its
    /// `php_output_deactivate`: `zend_hash_destroy` over the table, each
    /// entry's arguments, callback and record given up in the table's
    /// order by `user_shutdown_function_dtor`, then the table.
    pub(crate) fn free_shutdown_functions(&mut self) {
        let Some(table) = self.shutdown_functions.table.take() else {
            return;
        };
        for (_, index) in table.snapshot() {
            if let Some(index) = index.as_int() {
                self.drop_shutdown_entry(index);
            }
        }
        self.release(Value::array(table));
        self.shutdown_functions.entries.clear();
    }

    /// One `php_shutdown_function_entry` out of the heap in php's order,
    /// with the table it goes into: the arguments' `params` first,
    /// `safe_emalloc(param_count, sizeof(zval))` where there are any, in
    /// `register_shutdown_function`; then the table, laid down at the
    /// first entry (`ALLOC_HASHTABLE`); then the entry itself, which
    /// `zend_hash_next_index_insert_mem` and `zend_hash_str_update_mem`
    /// copy into a block of its size. Where the memory limit refuses one
    /// of them, what was made for the entry is given up with the
    /// callback and the arguments.
    fn shutdown_entry(
        &mut self,
        callback: Callable,
        arguments: Vec<Value>,
    ) -> Result<(ShutdownEntry, PhpArray), Thrown> {
        let params = if arguments.is_empty() {
            Ok(None)
        } else {
            let params = self.heap.block(arguments.len() * ZVAL_SIZE);
            self.fatal_of(params).map(Some)
        };
        let made = params.and_then(|params| {
            let table = self.shutdown_table();
            let record = table.and_then(|table| {
                let record = self.heap.block(SHUTDOWN_RECORD);
                self.fatal_of(record).map(|record| (record, table))
            });
            match record {
                Ok((record, table)) => Ok((record, params, table)),
                Err(thrown) => {
                    if let Some(params) = params {
                        self.heap.free_block(params);
                    }
                    Err(thrown)
                }
            }
        });
        match made {
            Ok((record, params, table)) => Ok((
                ShutdownEntry {
                    callback,
                    arguments,
                    record,
                    params,
                },
                table,
            )),
            Err(thrown) => {
                callback.release(self);
                for value in arguments {
                    self.release(value);
                }
                Err(thrown)
            }
        }
    }

    /// `BG(user_shutdown_function_names)`, laid down at the first entry
    /// as `ALLOC_HASHTABLE` and `zend_hash_init` lay it down.
    fn shutdown_table(&mut self) -> Result<PhpArray, Thrown> {
        if let Some(table) = self.shutdown_functions.table {
            return Ok(table);
        }
        let table = PhpArray::new(self)?;
        self.shutdown_functions.table = Some(table);
        Ok(table)
    }

    /// `user_shutdown_function_dtor`: the entry's arguments released, its
    /// `params` freed, its callback released and its record freed.
    fn drop_shutdown_entry(&mut self, index: i64) {
        let Some(entry) = self.shutdown_functions.take(index) else {
            return;
        };
        for value in entry.arguments {
            self.release(value);
        }
        if let Some(params) = entry.params {
            self.heap.free_block(params);
        }
        entry.callback.release(self);
        self.heap.free_block(entry.record);
    }

    /// `register_tick_function()`: the callable and its arguments held,
    /// with the owners `zend_fcc_addref` and `ZVAL_COPY` take, in
    /// `BG(user_tick_functions)`.
    pub fn register_tick_function(&mut self, callback: Callable, arguments: Vec<Value>) -> bool {
        callback.addref(self);
        for value in &arguments {
            self.addref(value);
        }
        self.tick_functions.push((callback, arguments));
        true
    }

    /// `unregister_tick_function()`: `zend_llist_del_element`, the first
    /// entry for the same callee, scope and object dropped and its
    /// owners given up.
    pub fn unregister_tick_function(&mut self, callback: &Callable) -> bool {
        let Some(at) = self.tick_functions.iter().position(|(c, _)| c == callback) else {
            return false;
        };
        let (held, arguments) = self.tick_functions.remove(at);
        held.release(self);
        for value in arguments {
            self.release(value);
        }
        true
    }

    // -------------------------------------------------- memory and gc

    /// The request's own memory, for the compilation whose unit is this
    /// request's alone (design §5.2): what a host handed the source for,
    /// an `eval` and an uncached include, whose constant strings are
    /// interned here and are gone when the request ends.
    pub(crate) fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// A string of `bytes` out of this request's heap, what
    /// [`PhpString::new`] is: php's `ZVAL_STRINGL_FAST`, the process's
    /// interned string for the empty string and a string of one byte
    /// (`ZSTR_EMPTY_ALLOC`, `ZSTR_CHAR`), `zend_string_init` otherwise.
    /// The memory limit is the fatal that ends the request, recorded
    /// here where `zend_mm` leaves the request, and the `Thrown` that
    /// carries no throwable ends the native.
    pub(crate) fn string(&mut self, bytes: &[u8]) -> Result<PhpString, Thrown> {
        let allocated = if bytes.len() <= 1 {
            self.heap.intern(bytes)
        } else {
            self.heap.string(bytes)
        };
        self.allocated(allocated)
    }

    /// A string of `len` bytes written in place, what
    /// [`PhpString::filled`] is: `zend_string_alloc`, then the caller's
    /// write into `ZSTR_VAL`.
    pub(crate) fn string_with(
        &mut self,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_with(len, fill);
        self.allocated(allocated)
    }

    /// A string of `n * m + l` bytes written in place, what
    /// [`PhpString::product`] is: `zend_string_safe_alloc`.
    pub(crate) fn string_product(
        &mut self,
        n: usize,
        m: usize,
        l: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_safe_alloc(n, m, l, fill);
        self.allocated(allocated)
    }

    /// `s` with `tail`'s bytes after its own, `zend_string_extend` and
    /// the `memcpy` after it ([`php_heap::Heap::string_append`]): one
    /// owner of `s` handed over, one of the longer string handed back, grown
    /// where it stands when that owner was the only one.
    pub(crate) fn string_append(
        &mut self,
        s: PhpString,
        tail: PhpString,
    ) -> Result<PhpString, Thrown> {
        let allocated = self.heap.string_append(s.heap(), tail.heap());
        self.allocated(allocated)
    }

    /// What the heap answered, as the native sees it: the string, or the
    /// memory limit as php's fatal, which `zend_mm` raises where the
    /// allocation was asked for and which ends the request from there.
    fn allocated(&mut self, allocated: Result<php_heap::Str, Fatal>) -> Result<PhpString, Thrown> {
        self.fatal_of(allocated).map(PhpString::of)
    }

    /// One more owner of what `value` holds: `Z_TRY_ADDREF_P`, a refcount
    /// for a string, an array, an object, a resource or a reference of
    /// the request's, nothing for a scalar, an interned string or an
    /// immutable array. What a native calls where php's body copies a
    /// value it already holds into a second place (`zend_string_copy`,
    /// `ZVAL_COPY`): the clone of the `Value` is `ZVAL_COPY_VALUE`, and
    /// this beside it is the second owner, given up with whatever row or
    /// property the copy is put in. What a native does before it returns an argument
    /// as its value, `RETURN_COPY` and `RETURN_STR_COPY`: the frame gives
    /// its owner up with the call, and the caller's temporary holds the
    /// one taken here.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(&self, value: &Value) {
        self.heap.addref(value.to_zval());
    }

    /// One owner fewer of what `value` holds, `zval_ptr_dtor`: the
    /// string, the array, the object, the resource or the reference is
    /// freed when its last owner gives it up, an array's elements and a
    /// reference's value released with it, an object's destructor and a
    /// resource's per-type destructor served first through seam 1, and
    /// an interned string or an immutable array never is. The request
    /// is the hooks over itself, so the heap a destructor allocates from
    /// is this one, handed back for the duration ([`Hooks`]).
    // Taking the value is the point: the owner is given up here, and a
    // `Value` that is still in hand after this call names what may be
    // freed.
    #[allow(clippy::needless_pass_by_value)]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn release(&mut self, value: Value) {
        Heap::release(&mut Hooks(self), value.to_zval());
    }

    /// One owner fewer as `zval_ptr_dtor_nogc` gives it up: what
    /// [`Request::release`] does, and never a possible root of the cycle
    /// collector (design §5.12). What `FREE_OP` does to a temporary
    /// operand and `zend_vm_stack_free_args` to an internal call's
    /// arguments.
    #[allow(clippy::needless_pass_by_value)]
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn release_nogc(&mut self, value: Value) {
        Heap::release_nogc(&mut Hooks(self), value.to_zval());
    }

    // ------------------------------------------ references, resources

    /// `ZVAL_NEW_REF`, what [`PhpRef::new`] is: a cell of the request's
    /// heap holding `value`, one owner of the cell, `value` the cell's
    /// from here, and the memory limit as the fatal that ends the
    /// request.
    pub(crate) fn reference(&mut self, value: Value) -> Result<PhpRef, Thrown> {
        let allocated = self.heap.reference(value.into_zval());
        self.fatal_of(allocated).map(PhpRef::of)
    }

    /// The reference's half of `zend_assign_to_variable`, what
    /// [`PhpRef::set`] and `ASSIGN` over a CV holding a reference are:
    /// `value` is the cell's from here, and what the cell held is
    /// released last, after the write.
    pub(crate) fn reference_set(&mut self, r: Ref, value: Value) {
        Heap::reference_set(&mut Hooks(self), r, value.into_zval());
    }

    /// `zend_register_resource`, what [`Resource::new`] is: the
    /// thirty-two bytes and the payload out of this request's heap, one
    /// owner, in the list under the next id, and the memory limit
    /// or php's `Resource ID space overflow` as the fatal that ends the
    /// request.
    pub(crate) fn new_resource<T: std::any::Any + Send>(
        &mut self,
        kind: ResourceType,
        payload: T,
    ) -> Result<Resource, Thrown> {
        let allocated = self.heap.resource_with(kind, payload);
        self.fatal_of(allocated).map(Resource::of)
    }

    /// The type's name as registered.
    pub(crate) fn resource_type_name(&self, kind: ResourceType) -> Option<&[u8]> {
        self.resources.type_name(kind)
    }

    /// `zend_list_close`, what [`Resource::close`] is.
    pub(crate) fn close_resource(&mut self, r: Res) {
        Heap::resource_close(&mut Hooks(self), r);
    }

    /// The native payload of `r` as `T`, borrowed for as long as the
    /// request is.
    pub(crate) fn resource_native<T: std::any::Any + Send>(&self, r: Res) -> Option<&T> {
        self.heap.resource_native(r)
    }

    /// The native payload of `r` as `T`, mutably.
    pub(crate) fn resource_native_mut<T: std::any::Any + Send>(
        &mut self,
        r: Res,
    ) -> Option<&mut T> {
        self.heap.resource_native_mut(r)
    }

    /// `zend_close_rsrc_list`, the first act of
    /// `zend_shutdown_executor_values` (design §5.10): every open
    /// resource closed from the top down, its destructor served, before
    /// the object storage is freed.
    pub(crate) fn close_resources(&mut self) {
        Heap::close_resources(&mut Hooks(self));
    }

    // -------------------------------------------------------- arrays

    /// An empty array out of this request's heap, what [`PhpArray::new`]
    /// is: php's `array_init`, and the memory limit as the fatal that
    /// ends the request.
    pub(crate) fn array(&mut self) -> Result<PhpArray, Thrown> {
        let allocated = self.heap.array();
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_new_array(size)`, what `INIT_ARRAY` lays down for the
    /// elements it was told to expect, and `zend_hash_real_init_mixed`
    /// over it where the opline says the array cannot be packed
    /// (`ZEND_ARRAY_NOT_PACKED`); the memory limit as the fatal that ends
    /// the request.
    pub(crate) fn array_sized(&mut self, size: u32, packed: bool) -> Result<PhpArray, Thrown> {
        let allocated = if packed {
            self.heap.array_sized(size)
        } else {
            self.heap.array_mixed(size)
        };
        self.fatal_of(allocated).map(PhpArray::of)
    }

    /// `zend_hash_update` or `zend_hash_index_update` over `a`, which
    /// has one owner: `value` is the array's from here, what `key` held
    /// is released, and a string key is one more owner.
    pub(crate) fn array_update(
        &mut self,
        a: Arr,
        key: php_heap::Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let z = value.into_zval();
        let written = Heap::array_update(&mut Hooks(self), a, key, z);
        self.fatal_of(written)
    }

    /// `zend_hash_next_index_insert` over `a`, which has one owner:
    /// `false`, and `value` given up, when the next element is taken.
    pub(crate) fn array_append(&mut self, a: Arr, value: Value) -> Result<bool, Thrown> {
        let z = value.into_zval();
        let appended = self.heap.array_append(a, z);
        if appended == Ok(false) {
            Heap::release(&mut Hooks(self), z);
        }
        self.fatal_of(appended)
    }

    /// `zend_hash_lookup` over `a`, which has one owner: the entry at
    /// `key` in place, laid down holding `null` where the table has none,
    /// which is what a `FETCH_DIM_W` and an `ASSIGN_DIM` write through.
    pub(crate) fn array_lookup(
        &mut self,
        a: Arr,
        key: php_heap::Key,
    ) -> Result<Option<ZvalRef>, Thrown> {
        let entry = self.heap.array_lookup(a, key);
        self.fatal_of(entry)
    }

    /// `zend_hash_next_index_insert` of `null` over `a`, which has one
    /// owner: the entry appended, in place; `None` when the next element
    /// is taken.
    pub(crate) fn array_next_entry(&mut self, a: Arr) -> Result<Option<ZvalRef>, Thrown> {
        let entry = self.heap.array_next_entry(a);
        self.fatal_of(entry)
    }

    /// `zend_hash_del` over `a`, which has one owner: `true` when the key
    /// was there, its value released.
    pub(crate) fn array_del(&mut self, a: Arr, key: php_heap::Key) -> bool {
        Heap::array_del(&mut Hooks(self), a, key)
    }

    /// `zend_array_dup`: a copy of `a` with one owner.
    pub(crate) fn array_dup(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let copied = self.heap.array_dup(a);
        self.fatal_of(copied)
    }

    /// `SEPARATE_ARRAY`: `a` itself with one owner, else a copy of it
    /// with one, `a` one owner fewer.
    pub(crate) fn array_separate(&mut self, a: Arr) -> Result<Arr, Thrown> {
        let separated = self.heap.separate(a);
        self.fatal_of(separated)
    }

    /// `zend_hash_sort` over `a`, which has one owner: the comparison
    /// carried through seam 1, since a `usort` callback runs PHP code,
    /// and the keys dropped under `renumber`.
    pub(crate) fn array_sort(
        &mut self,
        a: Arr,
        renumber: bool,
        compare: &mut dyn FnMut(&mut Request, &SortEntry, &SortEntry) -> std::cmp::Ordering,
    ) -> Result<(), Thrown> {
        let sorted = Heap::array_sort(&mut Hooks(self), a, renumber, &mut |hooks, x, y| {
            compare(hooks.0, x, y)
        });
        self.fatal_of(sorted)
    }

    /// `SEPARATE_ZVAL_NOREF` on what `r` holds, what
    /// [`PhpRef::separate_array`] is: the array in the cell with one
    /// owner, a copy written into the cell where it had more.
    pub(crate) fn reference_separate(&mut self, r: Ref) -> Result<Option<Arr>, Thrown> {
        let separated = self.heap.reference_separate(r);
        self.fatal_of(separated)
    }

    /// What the heap answered, as the native sees it: the value, or the
    /// fatal the allocator raised, recorded here where `zend_mm` leaves
    /// the request, as the `Thrown` carrying no throwable that ends the
    /// native.
    pub(crate) fn fatal_of<T>(&mut self, answered: Result<T, Fatal>) -> Result<T, Thrown> {
        match answered {
            Ok(value) => Ok(value),
            Err(fatal) => {
                self.end_in(&fatal);
                Err(Thrown::of(None))
            }
        }
    }

    /// `zend_set_memory_limit` over the ini entry as the table has it:
    /// at request start, and again when `ini_set()` moves the entry,
    /// which is php's `OnChangeMemoryLimit`. `-1` is no limit.
    fn set_memory_limit(&mut self) {
        let limit = self.ini_parse_bytes_of("memory_limit");
        self.heap
            .set_memory_limit(usize::try_from(limit).unwrap_or(usize::MAX >> 1));
    }

    /// `memory_get_usage()`.
    #[must_use]
    pub fn memory_usage(&self, real: bool) -> usize {
        self.heap.memory_usage(real)
    }

    /// `memory_get_peak_usage()`: the chunks' high-water mark for
    /// `real == true`, the bin accounting's for `real == false`, the two
    /// the heap keeps apart (design §5.13).
    #[must_use]
    pub fn memory_peak(&self, real: bool) -> usize {
        if real {
            self.heap.memory_real_peak()
        } else {
            self.heap.memory_peak()
        }
    }

    /// `memory_reset_peak_usage()`.
    pub fn reset_memory_peak(&mut self) {
        self.heap.reset_peak();
    }

    /// `gc_collect_cycles()`, `zend_gc_collect_cycles` over the
    /// request's heap (design §5.12): the values the collection freed, as php
    /// 8.5 counts them.
    pub fn gc_collect(&mut self) -> i64 {
        Heap::gc_collect_cycles(&mut Hooks(self))
    }

    /// `gc_enable()` and `gc_disable()`: `zend.enable_gc` altered, whose
    /// `OnUpdateGCEnabled` is `gc_enable`, so `ini_get` reads what the
    /// call left.
    pub fn gc_enable(&mut self, enabled: bool) {
        let value: &[u8] = if enabled { b"1" } else { b"0" };
        self.ini.retain(|(n, _)| n.as_slice() != b"zend.enable_gc");
        self.ini.push((b"zend.enable_gc".to_vec(), value.to_vec()));
        self.heap.gc_enable(enabled);
    }

    /// `gc_enabled()`.
    #[must_use]
    pub fn gc_enabled(&self) -> bool {
        self.heap.gc_enabled()
    }

    /// `gc_status()`, `zend_gc_get_status` in php 8.5's twelve fields, the
    /// times in seconds as php divides its nanoseconds, the keys strings
    /// of the request's.
    pub fn gc_status(&mut self) -> Result<PhpArray, Thrown> {
        let status = self.heap.gc_status();
        let mut array = PhpArray::new(self)?;
        for (name, value) in [
            ("running", Value::bool(status.running)),
            ("protected", Value::bool(status.protected)),
            ("full", Value::bool(status.full)),
            ("runs", Value::int(i64::from(status.runs))),
            ("collected", Value::int(i64::from(status.collected))),
            ("threshold", Value::int(i64::from(status.threshold))),
            ("buffer_size", Value::int(i64::from(status.buffer_size))),
            ("roots", Value::int(i64::from(status.roots))),
            (
                "application_time",
                Value::float(seconds(status.application_time)),
            ),
            (
                "collector_time",
                Value::float(seconds(status.collector_time)),
            ),
            (
                "destructor_time",
                Value::float(seconds(status.destructor_time)),
            ),
            ("free_time", Value::float(seconds(status.free_time))),
        ] {
            let key = Key::Str(PhpString::new(self, name.as_bytes())?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    // -------------------------------------------------- paths and streams

    /// A path made absolute against the request's `cwd`.
    #[must_use]
    pub fn absolutize(&self, path: &[u8]) -> HostPath {
        let given = PathBuf::from(String::from_utf8_lossy(path).into_owned());
        if given.is_absolute() {
            HostPath(given)
        } else {
            HostPath(self.cwd.join(given))
        }
    }

    /// `VCWD_REALPATH`, `tsrm_realpath`: the path against the request's
    /// working directory with every link resolved, through the realpath
    /// cache; `None` where the path does not exist.
    pub fn realpath(&mut self, path: &[u8]) -> Option<HostPath> {
        let cwd = self.cwd.as_os_str().as_encoded_bytes().to_vec();
        let real = self.virtual_file_ex(&cwd, path, Resolve::RealPath)?;
        Some(HostPath(PathBuf::from(std::ffi::OsStr::from_bytes(&real))))
    }

    /// `virtual_file_ex(state, path, NULL, how)` (`Z/Zend/zend_virtual_cwd.c:1007`)
    /// with `state` the directory `cwd`: `path` made absolute against it
    /// where it is relative (and left relative where `cwd` is empty),
    /// `.` and `..` taken out, and under [`Resolve::FilePath`] and
    /// [`Resolve::RealPath`] each link resolved, `tsrm_realpath_r`, every
    /// absolute path met entered in and read from the realpath cache;
    /// `None` where php answers `1`, an empty path, one of `MAXPATHLEN`
    /// bytes or more, and under `RealPath` one that is not there.
    ///
    /// ```php
    /// $dir = dirname(realpath(__FILE__));
    /// var_dump(realpath_cache_size() > 0, isset(realpath_cache_get()[$dir]), realpath_cache_get()[$dir]['is_dir'], realpath_cache_get()[$dir]['realpath'] === realpath($dir));
    /// clearstatcache(true, $dir); var_dump(isset(realpath_cache_get()[$dir]));
    /// clearstatcache(true); var_dump(realpath_cache_size(), realpath_cache_get());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// int(0)
    /// array(0) {
    /// }
    /// ```
    pub fn virtual_file_ex(&mut self, cwd: &[u8], path: &[u8], how: Resolve) -> Option<Vec<u8>> {
        crate::virtual_cwd::virtual_file_ex(&mut self.realpath_cache, cwd, path, how)
    }

    /// `CWDG(realpath_cache)` as `realpath_cache_get()` walks it, slot by
    /// slot and each chain newest first: each bucket's path as it was
    /// written, what it resolved to, its key, whether it is a directory,
    /// when it expires, and the bytes `realpath_cache_size()` counts it as.
    #[must_use]
    pub fn realpath_cache(&self) -> Vec<RealpathEntry> {
        self.realpath_cache
            .buckets()
            .map(|bucket| RealpathEntry {
                path: bucket.path.clone(),
                realpath: bucket.realpath.clone(),
                key: bucket.key,
                is_dir: bucket.is_dir,
                expires: bucket.expires,
                size: bucket.size(),
            })
            .collect()
    }

    /// `clearstatcache(true, ...)`'s half: `realpath_cache_del` of one
    /// path, or with none `realpath_cache_clean`.
    pub fn realpath_cache_clear(&mut self, path: Option<&[u8]>) {
        match path {
            Some(path) => self.realpath_cache.delete(path),
            None => self.realpath_cache.clean(),
        }
    }

    /// The `open_basedir` guard Mahout's `check_fs_access` performed at
    /// 27 sites: php's warning and a `Thrown` when the path is outside.
    pub fn fs_guard(&mut self, path: &HostPath, intent: PathIntent) -> Result<(), Thrown> {
        let _ = (path, intent);
        Ok(())
    }

    /// `is_uploaded_file()`.
    #[must_use]
    pub fn is_uploaded_file(&self, path: &[u8]) -> bool {
        let _ = path;
        false
    }

    /// `move_uploaded_file()`'s first half: take the upload's temporary
    /// path out of the request, once.
    pub fn take_uploaded_file(&mut self, path: &[u8]) -> Option<HostPath> {
        let _ = path;
        None
    }

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
    pub fn open_stream(
        &mut self,
        path: &[u8],
        mode: &[u8],
        use_include_path: bool,
    ) -> Result<Option<Stream>, Thrown> {
        self.open_stream_wrapper(path, mode, use_include_path)
    }

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
    pub fn open_url(&mut self, url: &[u8], mode: &[u8]) -> Result<Option<Stream>, Thrown> {
        self.open_url_wrapper(url, mode)
    }

    /// `php_stream_alloc(ops, abstract, 0, mode)`: a stream of a kind the
    /// extension implements, as a resource of the `stream` type with one
    /// owner, the caller's, at position zero with nothing read.
    pub fn new_stream(&mut self, ops: Box<dyn StreamOps>, mode: &[u8]) -> Result<Stream, Thrown> {
        let stream = crate::streams::of_extension(ops, mode);
        self.stream_alloc(stream)
    }

    /// `opendir()`, `php_stream_opendir` under `REPORT_ERRORS`: the
    /// directory as a stream of the `stream` type, through the plain
    /// files wrapper's `php_plain_files_dirstream_ops` or a registered
    /// wrapper's directory opener, and `None` after php's `Failed to
    /// open directory: ` warning, which an empty path is not given.
    pub fn open_dir(&mut self, path: &[u8]) -> Result<Option<Stream>, Thrown> {
        self.open_dir_wrapper(path)
    }

    /// `php_stream_stat_path_ex`, and with `cached` the stat `php_stat`
    /// makes: the path's wrapper located and its `url_stat` asked,
    /// `lstat(2)` where `link` and `stat(2)` otherwise for a plain file,
    /// laid out as `php_fstat` lays it out; `None` is the stat that
    /// failed or a wrapper with none. With `cached` the answer comes
    /// from, and goes into, `BG(CurrentStatFile)` or
    /// `BG(CurrentLStatFile)` by the path as written, which
    /// [`Request::clear_stat_cache`], a plain file's read, write or open,
    /// and every operation of [`Request::wrapper_op`] that changed a
    /// file clear.
    pub fn url_stat(
        &mut self,
        path: &[u8],
        link: bool,
        cached: bool,
    ) -> Result<Option<[i64; 13]>, Thrown> {
        self.stat_url(path, link, cached, false)
    }

    /// [`Request::url_stat`] with `cached`, as `php_stat` makes it for an
    /// existence check, `file_exists` and the `is_*` natives: a
    /// user-space wrapper's `url_stat` is handed
    /// `STREAM_URL_STAT_QUIET` beside the link bit.
    pub fn url_stat_quiet(&mut self, path: &[u8], link: bool) -> Result<Option<[i64; 13]>, Thrown> {
        self.stat_url(path, link, true, true)
    }

    /// `php_stream_locate_url_wrapper(path, &local, 0)` and the located
    /// wrapper's `url_stat` under `PHP_STREAM_URL_STAT_QUIET`, asked
    /// directly as `php_libxml_streams_IO_open_wrapper` asks it before
    /// it opens a path: no warning from the locate or the stat, the stat
    /// cache neither read nor written, and a wrapper with no `url_stat`
    /// told apart from a stat that failed.
    pub fn wrapper_url_stat(&mut self, path: &[u8]) -> Result<UrlStat, Thrown> {
        self.stat_url_quiet(path)
    }

    /// `php_clear_stat_cache`: the stat cache emptied, both entries.
    pub fn clear_stat_cache(&mut self) {
        self.forget_stats();
    }

    /// `php_stream_locate_url_wrapper(path, &local, 0) ==
    /// &php_plain_files_wrapper`: the path the plain files wrapper would
    /// be handed, `file://` taken off, or `None` where the path is
    /// another wrapper's or none's, after the warnings the locate raises.
    pub fn plain_path(&mut self, path: &[u8]) -> Option<Vec<u8>> {
        self.plain_local(path)
    }

    /// One [`WrapperOp`] at `path`, as `unlink`, `rename`, `mkdir`,
    /// `rmdir`, and `touch`, `chmod`, `chown` and `chgrp` over a wrapper
    /// reach it: the wrapper located, php's warning where there is none
    /// or it lacks the operation, else the wrapper's own answer, the
    /// plain files wrapper's with php's warnings, `strerror(errno)` among
    /// them. `mkdir` and `rmdir` of a wrapper without them answer
    /// `false` and say nothing, as `php_stream_mkdir` does.
    pub fn wrapper_op(&mut self, path: &[u8], op: WrapperOp<'_>) -> Result<bool, Thrown> {
        self.path_op(path, op)
    }

    /// `tempnam()`'s `php_open_temporary_fd_ex` under
    /// `PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ALWAYS`: a new `0600` file in
    /// `dir`, or after php's notice in the temporary directory, named by
    /// the prefix and nineteen random characters, and closed; its path,
    /// or `None` where none could be made.
    pub fn temporary_name(
        &mut self,
        dir: &[u8],
        prefix: &[u8],
    ) -> Result<Option<PhpString>, Thrown> {
        match self.open_temporary_fd(Some(dir), prefix) {
            Some((_, path)) => Ok(Some(PhpString::new(
                self,
                path.as_os_str().as_encoded_bytes(),
            )?)),
            None => Ok(None),
        }
    }

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
    pub fn with_stream_ctx<R>(
        &mut self,
        ctx: Option<Resource>,
        f: impl FnOnce(&mut Request) -> R,
    ) -> R {
        let outer = std::mem::replace(&mut self.stream_ctx, ctx);
        let answer = f(self);
        self.stream_ctx = outer;
        answer
    }

    /// `php_stream_fopen_tmpfile`, what `tmpfile()` is: a new file in the
    /// temporary directory, opened `r+b` as a plain file and unlinked
    /// when it closes, or `None` where none could be made.
    pub fn tmpfile(&mut self) -> Result<Option<Stream>, Thrown> {
        crate::streams::temporary_file(self, None, b"php")
    }

    /// `stream_wrapper_register()`: `false` when the scheme is taken.
    pub fn register_wrapper(&mut self, wrapper: StreamWrapper) -> bool {
        if self.wrappers.iter().any(|w| w.scheme == wrapper.scheme) {
            return false;
        }
        self.wrappers.push(wrapper);
        true
    }

    /// `stream_get_wrappers()`: the schemes of
    /// `php_stream_get_url_stream_wrappers_hash()`, the request's own
    /// table once a program changed it, with what `stream_wrapper_register`
    /// added and `stream_wrapper_unregister` took out, and the built-in
    /// one before, in the order each row went in.
    #[must_use]
    pub fn stream_wrappers(&self) -> Vec<String> {
        self.wrapper_table()
            .into_iter()
            .map(|row| String::from_utf8_lossy(&row.scheme).into_owned())
            .collect()
    }

    /// `php_stream_filter_register_factory_volatile`
    /// (`Z/main/streams/filter.c:50`): `factory` for the filter `name`,
    /// which may end in `.*` to take every name under it, for the rest of
    /// the request; `false` where the name is taken.
    pub fn register_filter(&mut self, name: &[u8], factory: FilterFactory) -> bool {
        if self.filters.iter().any(|(n, _)| n.as_slice() == name) {
            return false;
        }
        self.filters.push((name.to_vec(), factory));
        true
    }

    /// `stream_get_filters()`'s names: every filter registered, in the
    /// order they were.
    #[must_use]
    pub fn stream_filters(&self) -> Vec<Vec<u8>> {
        self.filters.iter().map(|(name, _)| name.clone()).collect()
    }
}

/// The length of a path's scheme where it has one as
/// `php_stream_locate_url_wrapper` reads it: alphanumerics, `+`, `-` and
/// `.`, two or more, then `://`, or `data:` alone.
fn scheme_length(path: &[u8]) -> Option<usize> {
    let n = path
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        .count();
    (n > 1
        && path.get(n) == Some(&b':')
        && (path[n + 1..].starts_with(b"//") || (n == 4 && path.starts_with(b"data:"))))
    .then_some(n)
}

/// Whether a path names a stream wrapper other than the plain files one,
/// which an include opens through that wrapper.
fn is_wrapper_url(path: &[u8]) -> bool {
    scheme_length(path).is_some_and(|n| !path[..n].eq_ignore_ascii_case(b"file"))
}

/// `include_path`'s entries as `php_resolve_path` walks them: split at
/// `:`, where an entry that begins with a wrapper's scheme keeps its
/// `://`, and `.://` or `..://` is no scheme.
fn include_path_entries(include_path: &[u8]) -> Vec<Vec<u8>> {
    let mut entries = Vec::new();
    let mut rest = include_path;
    while !rest.is_empty() {
        let n = rest
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            .count();
        let skip = if n > 1 && rest[n..].starts_with(b"://") && &rest[..n] != b".." {
            n + 3
        } else {
            0
        };
        if let Some(at) = rest[skip..].iter().position(|b| *b == b':') {
            entries.push(rest[..skip + at].to_vec());
            rest = &rest[skip + at + 1..];
        } else {
            entries.push(rest.to_vec());
            rest = &[];
        }
    }
    entries
}

/// The request's side of the heap's upward seam (design §4.3, seam 1):
/// the request itself, whole, so that `__destruct` is served by the
/// request that owns the frames, and the heap the operation is on is
/// the request's own field, handed back through [`HeapHooks::heap`] at
/// every step. No second `&mut` on the request: the heap never holds a
/// borrow of itself across a call up, which is the discipline php keeps
/// by re-reading `EG(objects_store)` after `dtor_obj`.
struct Hooks<'a>(&'a mut Request);

/// Where a walk over an object stands at a property: a declared slot,
/// the key of a bucket of the dynamic table, or a hooked property's
/// value as its `get` hook answered it, one owner the walker's to give
/// up.
#[derive(Clone, Copy)]
pub(crate) enum Walked {
    Slot(u32),
    Dynamic(php_heap::Key),
    Hooked(Value),
}

/// One `zend_property_info *` of a reference's source list
/// (`Z/Zend/zend_types.h:626`): a typed property whose slot holds the
/// cell, named by the class that declared it, the name the `TypeError`
/// prints and the declared type the write is held to. php names the
/// declaration itself; the linked tables here are shared and immutable,
/// so the row carries what the check and the message read.
pub(crate) struct RefSource {
    /// The class that declared the property, which is the one php's
    /// `prop->ce->name` prints.
    pub(crate) declaring: ClassId,
    /// The property's name, unmangled, as php's
    /// `zend_get_unmangled_property_name` hands it over.
    pub(crate) name: Vec<u8>,
    /// `prop_info->type`.
    pub(crate) ty: php_unit::decl::Type,
}

impl HeapHooks for Hooks<'_> {
    fn heap(&mut self) -> &mut Heap {
        &mut self.0.heap
    }

    /// `dtor_obj`, through the table of the object's class (design §5.4).
    fn destruct(&mut self, obj: Obj) {
        let table = self.0.object_handlers(obj);
        table.dtor_obj(self.0, PhpObject::of(obj));
    }

    /// `free_obj`, through the table: the payload's release is the
    /// class's, and the drop after it is the heap's.
    fn free_native(&mut self, obj: Obj) {
        let table = self.0.object_handlers(obj);
        table.free_obj(self.0, PhpObject::of(obj));
    }

    /// php's `dtor_obj != zend_objects_destroy_object || ce->destructor`:
    /// the class declares or inherits `__destruct`, or its handler
    /// table serves a destructor of its own, which `Fiber`'s does.
    fn has_destructor(&mut self, obj: Obj) -> bool {
        let class = ClassId(obj.class());
        self.0.method_named(class, b"__destruct").is_some()
            || crate::fiber::serves_destructor(self.0.object_handlers(obj))
    }

    /// The live temporaries of every frame of the running execution,
    /// innermost first (`zend_gc_check_root_tmpvars`): each frame's
    /// `ZEND_LIVE_TMPVAR` and `ZEND_LIVE_LOOP` ranges open at the opline
    /// it stands at, a range that starts past it ending the walk of that
    /// frame's table, as php's sorted table does. A `foreach` by
    /// reference that a collection ran inside holds its array in a live
    /// `LOOP` temporary, which the collection roots again after it, and
    /// the array's cycles are found by the next one.
    ///
    /// ```php
    /// $a = [0, 1];
    /// foreach ($a as &$v) {
    ///     $a[0] =& $a;
    ///     $a[1] = array();
    ///     $a[1][0] =& $a[1];
    ///     $b = 1;
    ///     $a =& $b;
    ///     var_dump(gc_collect_cycles());
    ///     break;
    /// }
    /// var_dump(gc_status()['roots']);
    /// unset($v);
    /// var_dump(gc_collect_cycles());
    /// ```
    /// ```expect
    /// int(0)
    /// int(1)
    /// int(2)
    /// ```
    fn gc_live_temporaries(&mut self, visit: &mut dyn FnMut(Zval)) {
        let req = &*self.0;
        for (at, state) in req.vm.frames().iter().enumerate().rev() {
            let Some(code) = crate::exec::body_of(&state.unit, state.body) else {
                continue;
            };
            let frame = FrameRef(u32::try_from(at).unwrap_or(u32::MAX));
            for range in &code.live_ranges {
                if range.start.0 > state.opline {
                    break;
                }
                if range.end.0 <= state.opline {
                    continue;
                }
                if matches!(
                    range.kind,
                    php_unit::LiveKind::TmpVar | php_unit::LiveKind::Loop
                ) {
                    let slot = crate::exec::temporary_slot(code, range.var);
                    visit(req.slot(frame, slot).to_zval());
                }
            }
        }
    }

    /// A `WeakMap`'s keys beside its entries (`crate::weakrefs::gc_entries`).
    fn gc_weakmap_entries(&mut self, map: Obj, visit: &mut dyn FnMut(Obj, ZvalRef)) -> bool {
        crate::weakrefs::gc_entries(self.0, map, visit)
    }

    /// The `WeakMap` entries keyed by an object
    /// (`crate::weakrefs::gc_keyed`).
    fn gc_weakly_keyed(&mut self, key: Obj, visit: &mut dyn FnMut(ZvalRef, Obj)) {
        crate::weakrefs::gc_keyed(self.0, key, visit);
    }

    /// `get_gc`, through the table: what the payload owns, handed to the
    /// collector's walk as zvals.
    fn gc_children(&mut self, obj: Obj, visit: &mut dyn FnMut(Zval)) {
        let table = self.0.object_handlers(obj);
        table.get_gc(self.0, PhpObject::of(obj), &mut |value| {
            visit(value.to_zval());
        });
        // `zend_lazy_object_get_gc`: the initializer a lazy object waits
        // for, or the instance a proxy forwards to.
        self.0.lazy_gc(obj, &mut |value| visit(value.to_zval()));
    }

    /// `list_dtor_ex`: the type's destructor as registered, handed the
    /// resource with the type it had, as php hands it a copy of the
    /// struct; a type with no destructor closes silently.
    fn free_resource(&mut self, res: Res, kind: ResourceType) {
        if let Some(dtor) = self.0.resources.dtor(kind) {
            dtor(self.0, Resource::of(res));
        }
    }

    fn out_of_memory(&mut self, bytes: usize) -> Fatal {
        Fatal::OutOfMemory(bytes)
    }

    /// `zend_free_source_list`: the row the cell's word names, dropped
    /// with the cell. The heap has already read the word, so nothing
    /// here reads the reference again.
    fn free_reference(&mut self, r: Ref) {
        let word = r.sources();
        self.0.ref_sources.retain(|(at, _)| *at != word);
    }

    /// `ZEND_REF_DEL_TYPE_SOURCE` for a slot of an object being freed
    /// ([`Request::slot_source_gone`]).
    fn slot_source_gone(&mut self, obj: Obj, slot: u32, r: Ref) {
        self.0.slot_source_gone(PhpObject::of(obj), slot, r);
    }

    /// `zend_weakrefs_notify`: every weak holder of the object cleared
    /// before its members go (`crate::weakrefs::notify`).
    fn weakrefs_notify(&mut self, obj: Obj) {
        crate::weakrefs::notify(self.0, obj);
    }

    /// `zend_lazy_object_del_info` as a lazy object is freed
    /// ([`Request::lazy_forget`]).
    fn forget_lazy(&mut self, obj: Obj) {
        self.0.lazy_forget(obj);
    }
}

/// What `zend_objects_destroy_object` found in the pending slot before
/// it served a destructor ([`Request::save_pending`]): nothing, one
/// throwable held there for the duration of the call, the unwind exit
/// held the same way, or the throwable that is the object being
/// destroyed, which is php's one `E_CORE_ERROR` and no call at all.
#[derive(Clone, Copy)]
enum Saved {
    Nothing,
    Throwable(PhpObject),
    UnwindExit,
    TheObject,
}

/// Where `display_errors` sends a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Display {
    Off,
    Body,
    Stderr,
}

/// `PHP_DISPLAY_ERRORS_STDOUT` and `PHP_DISPLAY_ERRORS_STDERR`
/// (`Z/main/php_globals.h:36-37`), the two modes of `PG(display_errors)`
/// beside zero.
/// `sizeof(zval)`, what `safe_emalloc` asks per argument a shutdown
/// entry keeps.
const ZVAL_SIZE: usize = 16;

/// `sizeof(php_shutdown_function_entry)` at 8.5
/// (`Z/ext/standard/basic_functions.h:125`): the `zend_fcall_info_cache`'s
/// five pointers, `params` and `param_count`.
const SHUTDOWN_RECORD: usize = 56;

/// `BG(user_shutdown_function_names)`: the table php keeps the entries
/// in, one of the request's own arrays, laid down at the first entry,
/// grown as a packed table grows and turned into a hash by the first
/// entry registered under a name, so what `memory_get_usage()` counts
/// and where the memory limit refuses are php's; and the entries its
/// elements name by index, each with the blocks php allocates for it. An
/// entry's slot stays empty once its entry is gone, the table having
/// dropped the element that named it.
///
/// The first entry lays the table's header down and its eight packed
/// slots beside the entry and its one argument; the ninth grows the
/// slots to sixteen:
///
/// ```php
/// $f = function (...$a) {};
/// $m = [memory_get_usage()];
/// register_shutdown_function($f, 'a');
/// $m[] = memory_get_usage();
/// register_shutdown_function($f);
/// $m[] = memory_get_usage();
/// for ($i = 0; $i < 7; $i++) register_shutdown_function($f);
/// $m[] = memory_get_usage();
/// register_shutdown_function($f, 1, 2, 3);
/// $m[] = memory_get_usage();
/// for ($i = 1; $i < count($m); $i++) echo $m[$i] - $m[$i - 1], "\n";
/// ```
/// ```expect
/// 504
/// 56
/// 552
/// 104
/// ```
#[derive(Default)]
struct ShutdownFunctions {
    table: Option<PhpArray>,
    entries: Vec<Option<ShutdownEntry>>,
}

/// One `php_shutdown_function_entry`: its callback and arguments, which
/// it owns, the block the entry is, and the arguments' `params` block.
struct ShutdownEntry {
    callback: Callable,
    arguments: Vec<Value>,
    record: php_heap::Block,
    params: Option<php_heap::Block>,
}

impl ShutdownFunctions {
    /// The entry kept, and the index the table's element names it by.
    fn slot(&mut self, entry: ShutdownEntry) -> i64 {
        self.entries.push(Some(entry));
        i64::try_from(self.entries.len() - 1).unwrap_or(i64::MAX)
    }

    /// The entry a table element names.
    fn entry(&self, index: &Value) -> Option<&ShutdownEntry> {
        let index = usize::try_from(index.as_int()?).ok()?;
        self.entries.get(index)?.as_ref()
    }

    /// The entry at `index`, taken out of its slot.
    fn take(&mut self, index: i64) -> Option<ShutdownEntry> {
        self.entries.get_mut(usize::try_from(index).ok()?)?.take()
    }
}

const DISPLAY_ERRORS_STDOUT: u8 = 1;
const DISPLAY_ERRORS_STDERR: u8 = 2;

/// `uint8_t mode = ZEND_ATOL(...)` at `Z/main/main.c:513`: the number
/// narrowed to php's byte.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "php's `uint8_t mode = ZEND_ATOL(value)`, whose narrowing is the behaviour being copied"
)]
fn display_errors_mode(number: i64) -> u8 {
    number as u8
}

/// `(double) hrtime / ZEND_NANO_IN_SEC`, a collector time in seconds.
fn seconds(d: std::time::Duration) -> f64 {
    d.as_secs_f64()
}

/// `zend_ini_parse_bool`: an ini entry read as on or off, where "off",
/// "no", "false" and "" are off beside "0" and everything else is on.
fn ini_truth(value: Option<&[u8]>) -> bool {
    let Some(value) = value else { return false };
    let text = value.to_ascii_lowercase();
    !matches!(text.as_slice(), b"" | b"0" | b"off" | b"no" | b"false")
}

/// `E_ALL` (`Z/Zend/zend_errors.h:43`), the fourteen levels php 8.5
/// raises, which is every bit below `E_USER_DEPRECATED`'s but
/// `E_STRICT`'s, the one 8.4 removed from the sum.
pub(crate) const E_ALL: i64 = 32767 & !Level::Strict.bit();

/// `ZEND_ATOL`, which is `atoll` (`Z/Zend/zend_long.h:77`): leading
/// whitespace skipped, one sign, the digits up to the first byte that is
/// not one, and zero where there are none; a number past `i64` saturates
/// as `strtoll` does.
fn ini_atol(value: &[u8]) -> i64 {
    let mut bytes = value
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .peekable();
    let negative = match bytes.peek() {
        Some(b'-') => {
            bytes.next();
            true
        }
        Some(b'+') => {
            bytes.next();
            false
        }
        _ => false,
    };
    let mut n: i64 = 0;
    for digit in bytes.take_while(|b| b.is_ascii_digit()) {
        let digit = i64::from(digit - b'0');
        n = if negative {
            n.checked_mul(10)
                .and_then(|n| n.checked_sub(digit))
                .unwrap_or(i64::MIN)
        } else {
            n.checked_mul(10)
                .and_then(|n| n.checked_add(digit))
                .unwrap_or(i64::MAX)
        };
    }
    n
}

/// `zend_visibility_string` (`Z/Zend/zend_compile.c:1063`): the word
/// php's bad-call texts name a method's visibility by.
fn visibility_string(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Private => "private",
    }
}

/// One string property of a throwable, empty where there is none.
fn string_prop(req: &Request, object: PhpObject, name: &[u8]) -> String {
    object
        .prop(req, name)
        .and_then(|value| {
            value
                .as_str_bytes()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        })
        .unwrap_or_default()
}

/// php's `error_type_str` (`Z/main/main.c:1414-1448`), the word both
/// renderings name the level by. `E_RECOVERABLE_ERROR` is the one level
/// whose word is not its family's, and `E_STRICT` is the one level php
/// 8.5 has no case for, so it falls to the `default:` every level php
/// dropped falls to.
///
/// No fence reaches either word, so the two arms are held by the test
/// below and not by a witness. php 8.5 raises `E_RECOVERABLE_ERROR` at
/// one site, `zend_object_is_true` (`Z/Zend/zend_operators.c:2918`),
/// when an object's `cast_object` handler refuses `_IS_BOOL`; the
/// standard handler answers `true` for every user class, and no class
/// in the bundle brings a handler that refuses, so no PHP source raises
/// it, and `trigger_error()` takes the four `E_USER_*` levels alone.
/// `E_STRICT` has no raise anywhere: its constant stands `#[Deprecated]`
/// since 8.4 with the message that the level was removed.
/// `E_FATAL_ERRORS` (`Z/Zend/zend_errors.h:47`): the six levels
/// `php_error_cb` bails out of, and the ones `zend_error_impl` fetches a
/// trace for under `fatal_error_backtraces`.
fn fatal_errors(level: Level) -> bool {
    matches!(
        level,
        Level::Error
            | Level::CoreError
            | Level::CompileError
            | Level::UserError
            | Level::RecoverableError
            | Level::Parse
    )
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error | Level::CoreError | Level::CompileError | Level::UserError => "Fatal error",
        Level::RecoverableError => "Recoverable fatal error",
        Level::Warning | Level::CoreWarning | Level::CompileWarning | Level::UserWarning => {
            "Warning"
        }
        Level::Parse => "Parse error",
        Level::Notice | Level::UserNotice => "Notice",
        Level::Deprecated | Level::UserDeprecated => "Deprecated",
        Level::Strict => "Unknown error",
    }
}

/// php's `escape_html` (`Z/main/main.c:1038-1050`): the message of an
/// `E_ERROR` or an `E_PARSE` through `php_escape_html_entities_ex` at
/// `ENT_COMPAT` with `double_encode`, which is `htmlspecialchars`'s four
/// — `&`, `<`, `>` and `"`, an already-written entity escaped again —
/// and leaves `'` alone. php's retry with the invalid bytes substituted
/// is for a message that is not its charset, and a message this engine
/// renders is a `&str`.
fn escape_html(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for letter in message.chars() {
        match letter {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(letter),
        }
    }
    escaped
}

/// `smart_str_append_escaped` (`Z/Zend/zend_smart_str.c:77`): a byte
/// under 32, over 126 or a backslash written as `\n`, `\r`, `\t`, `\f`,
/// `\v`, `\\`, `\e` or `\xNN` with the digits in upper case, and every
/// other byte as itself. What a string argument in a trace and
/// `zend_print_zval_r`'s cousins write.
pub(crate) fn append_escaped(out: &mut String, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0c => out.push_str("\\f"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            0x1b => out.push_str("\\e"),
            byte if !(32..=126).contains(&byte) => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push_str("\\x");
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0xf)]));
            }
            byte => out.push(char::from(byte)),
        }
    }
}

#[cfg(test)]
impl Request {
    /// [`Request::push_frame`] in a test, whose request no memory limit
    /// holds, so the push is never refused.
    pub(crate) fn pushed(&mut self, unit: Arc<Unit>) -> FrameRef {
        let pushed = self.push_frame(unit);
        assert_eq!(pushed.as_ref().err(), None, "a test's frame is pushed");
        pushed.unwrap_or(FrameRef(0))
    }
}

/// `zend_class_redeclaration_error`'s two wordings over the earlier
/// declaration: what it is and its own name, and its site when it was a
/// user class.
fn redeclaration(earlier: &ClassDecl) -> String {
    let what = php_unit::link::object_type(earlier.kind);
    let name = String::from_utf8_lossy(&earlier.name.0);
    if earlier.file.0.is_empty() {
        return format!("Cannot redeclare {what} {name}");
    }
    format!(
        "Cannot redeclare {what} {name} (previously declared in {}:{})",
        String::from_utf8_lossy(&earlier.file.0),
        earlier.line
    )
}

#[cfg(test)]
mod tests {
    use super::{Level, Request, append_escaped, escape_html, level_name};
    use crate::engine::{Engine, RequestDescription};

    /// A class declaring `__destruct`, as an extension declares one:
    /// what [`Request::destruct`] looks for on the class of the object
    /// it is handed. Its entry is never served, since the branch under
    /// test refuses the call before it.
    fn destructible() -> php_unit::decl::ClassDecl {
        use php_unit::decl::{
            ClassDecl, ClassFlags, ClassKind, EnumBacking, FunctionDecl, MethodDecl, Name,
            Visibility,
        };
        ClassDecl {
            name: Name(b"Destructible".as_slice().into()),
            kind: ClassKind::Class,
            flags: ClassFlags::default(),
            parent: None,
            interfaces: Vec::new(),
            traits: Vec::new(),
            adaptations: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: vec![MethodDecl {
                function: FunctionDecl {
                    name: Name(b"__destruct".as_slice().into()),
                    parameters: Vec::new(),
                    return_type: None,
                    returns_by_ref: false,
                    is_generator: false,
                    attributes: Vec::new(),
                    doc_comment: None,
                    file: Name(Box::new([])),
                    line: 0,
                    end_line: 0,
                    frameless: Vec::new(),
                },
                visibility: Visibility::Public,
                is_instance: true,
                is_abstract: false,
                is_final: false,
                tentative_return: false,
            }],
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
        }
    }

    /// The entry of the `__destruct` above: the branch under test
    /// refuses the call, so a raise here is the test failing.
    fn never_served(req: &mut Request) -> Result<crate::api::values::Value, super::Thrown> {
        Err(crate::api::raising::throw(
            req,
            "Error",
            "the destructor was served",
        ))
    }

    /// The same entry, serving: what a native class's `__destruct` is,
    /// a Rust body on the request that served it.
    fn writes_d(req: &mut Request) -> Result<crate::api::values::Value, super::Thrown> {
        let text = crate::api::values::PhpString::new(req, b"d\n")?;
        req.write(text.as_bytes());
        Ok(crate::api::values::Value::null())
    }

    /// Design §5.10 and seam 1: a native class's `__destruct` is served
    /// at the last release like a user class's, through the same
    /// `Request::destruct` and the native branch of `serve_call`. No
    /// witness can hold this one, because the class an extension of
    /// this engine declares is no class of the oracle's, so the
    /// `ASSIGN` the witness beside `handlers::assign` pins is served
    /// here over a class whose body is Rust.
    #[test]
    fn a_native_classs_destructor_is_served_at_the_last_release() {
        let decl = destructible();
        let Some(method) = decl.methods.first().cloned() else {
            return;
        };
        let destructible = crate::api::abi::ExtensionBuilder::default()
            .classes(vec![(
                decl,
                vec![(method, writes_d as crate::api::abi::Entry)],
            )])
            .build("destructible");
        let engine = Engine::builder().extensions(&[destructible]).build();

        let served = engine.serve_capture(
            "<?php $a = new Destructible(); $a = 1; echo \"after\\n\";",
            crate::test::Capture::default(),
        );

        assert_eq!(String::from_utf8_lossy(&served.stdout), "d\nafter\n");
        assert_eq!(served.fatal, None);
    }

    /// `strerror(ENOENT)` is the sentence alone, which php prints inside
    /// `open(%s, O_RDWR) failed: %s (%d)` and every message like it.
    #[test]
    fn os_error_text_is_strerror() {
        let engine = Engine::builder().build();
        let req = Request::new(
            std::sync::Arc::clone(&engine.shared),
            RequestDescription::default(),
        );
        assert_eq!(req.os_error_text(2), "No such file or directory");
    }

    /// An extension entry's `on_modify` (`PHP_INI_MH` at
    /// `ZEND_INI_STAGE_RUNTIME`) as `ini_set()` serves it: before the
    /// value is taken, what it raises raised, and its refusal leaving the
    /// entry as it was. No witness holds it, because the entry is no
    /// entry of the oracle's; `assert.c`'s `OnUpdateAssert` family is
    /// the one php-src has, which reaches it with #753.
    #[test]
    fn an_extension_entrys_on_modify_is_served_by_ini_set() {
        #[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
        fn guarded(req: &mut Request, value: &[u8]) -> Result<bool, crate::api::raising::Thrown> {
            if value == b"0" {
                crate::api::raising::docref(
                    req,
                    Level::Deprecated,
                    "t.flag INI setting is deprecated",
                );
            }
            Ok(value != b"bad")
        }
        let extension = crate::api::abi::ExtensionBuilder::default()
            .ini(vec![php_unit::decl::IniDecl {
                name: php_unit::decl::Name(b"t.flag".as_slice().into()),
                default: b"1".as_slice().into(),
                user: true,
                per_dir: true,
                system: true,
            }])
            .on_modify(b"t.flag", guarded)
            .build("modified");
        let engine = Engine::builder().extensions(&[extension]).build();
        let served = engine.serve_capture(
            "<?php var_dump(ini_set('t.flag', '0'), ini_set('t.flag', 'bad'), ini_get('t.flag'));",
            crate::test::Capture::default(),
        );
        let out = String::from_utf8_lossy(&served.stdout);
        assert!(
            out.contains("Deprecated: ini_set(): t.flag INI setting is deprecated"),
            "{out}"
        );
        assert!(
            out.ends_with("string(1) \"1\"\nbool(false)\nstring(1) \"0\"\n"),
            "{out}"
        );
    }

    /// The same `on_modify` at `ZEND_INI_STAGE_STARTUP`, over the value
    /// the host set (`zend_register_ini_entries_ex`): a value it refuses
    /// leaves the entry at its default, for `ini_get()` and for what
    /// `ini_set()` answers was there, and its docref is raised
    /// first in every request as `PHP Startup: …` at no file; a value
    /// it takes is the entry's.
    #[test]
    fn a_startup_value_the_on_modify_refuses_keeps_the_default() {
        #[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
        fn guarded(req: &mut Request, value: &[u8]) -> Result<bool, crate::api::raising::Thrown> {
            if value == b"bad" {
                crate::api::raising::docref(req, Level::Warning, "t.flag refused");
                return Ok(false);
            }
            Ok(true)
        }
        let extension = || {
            crate::api::abi::ExtensionBuilder::default()
                .ini(vec![php_unit::decl::IniDecl {
                    name: php_unit::decl::Name(b"t.flag".as_slice().into()),
                    default: b"1".as_slice().into(),
                    user: true,
                    per_dir: true,
                    system: true,
                }])
                .on_modify(b"t.flag", guarded)
                .build("modified")
        };
        let script = "<?php var_dump(ini_get('t.flag'), ini_set('t.flag', '3'));";
        let refused = Engine::builder()
            .extensions(&[extension()])
            .ini("t.flag", "bad")
            .build()
            .serve_capture(script, crate::test::Capture::default());
        assert_eq!(
            String::from_utf8_lossy(&refused.stdout),
            "\nWarning: PHP Startup: t.flag refused in Unknown on line 0\nstring(1) \"1\"\nstring(1) \"1\"\n"
        );
        let taken = Engine::builder()
            .extensions(&[extension()])
            .ini("t.flag", "2")
            .build()
            .serve_capture(script, crate::test::Capture::default());
        assert_eq!(
            String::from_utf8_lossy(&taken.stdout),
            "string(1) \"2\"\nstring(1) \"2\"\n"
        );
    }

    /// `zend_objects_destroy_object`'s `E_CORE_ERROR`
    /// (`Z/Zend/zend_objects.c:157`): a destructor served on the
    /// throwable that is itself pending ends the request there, with no
    /// site, and the destructor is not served. No PHP snippet reaches
    /// it, here or in php, whose corpus names it nowhere, and
    /// [`Request::save_pending`] says which of php's releases would
    /// have to be the one that reaches it. So the state is built here
    /// instead of witnessed, which is what harness §8 allows and #390
    /// settled. The fatal carries no trace of its own, because php
    /// fetches one only while nothing is pending, and here the
    /// throwable is.
    #[test]
    fn a_destructor_served_on_the_pending_throwable_is_the_core_fatal() {
        let decl = destructible();
        let Some(method) = decl.methods.first().cloned() else {
            return;
        };
        let destructible = crate::api::abi::ExtensionBuilder::default()
            .classes(vec![(
                decl,
                vec![(method, never_served as crate::api::abi::Entry)],
            )])
            .build("destructible");
        let engine = Engine::builder().extensions(&[destructible]).build();
        let mut request = request(&engine);
        let Ok(object) = request.new_object_of("Destructible") else {
            return;
        };
        request.pending = Some(object);

        request.destruct(object);

        assert!(
            request.pending.is_some(),
            "the throwable is pending still, since nothing was served"
        );
        assert_eq!(
            String::from_utf8_lossy(&request.take_body()),
            "\nFatal error: Attempt to destruct pending exception in Unknown on line 0\n"
        );
    }

    /// A request on a fresh engine's shared state, for a test that gives
    /// `note_error_in` a level no raiser reaches yet.
    fn request(engine: &Engine) -> Request {
        Request::new(
            engine.shared.clone(),
            RequestDescription {
                sapi_name: String::from("cli"),
                ..RequestDescription::default()
            },
        )
    }

    /// `(EG(error_reporting) & type) || (type & E_CORE)`: a core level
    /// is rendered under `error_reporting=0`, in the same word its
    /// family takes, and every other level is masked. The one raiser
    /// that reaches `E_CORE_ERROR` is
    /// [`Request::save_pending`], which no PHP snippet reaches, and no
    /// raiser reaches `E_CORE_WARNING`, so the mask is held here and not
    /// by a witness. `E_CORE_ERROR` is in
    /// `E_FATAL_ERRORS`, so under php's default `fatal_error_backtraces`
    /// it fetches its trace, `#0 {main}` with no frame standing, and the
    /// warning fetches none.
    #[test]
    fn a_core_level_is_rendered_past_error_reporting() {
        let engine = Engine::builder().ini("error_reporting", "0").build();
        let file = b"witness.php";
        let mut request = request(&engine);
        request.note_error_in(Level::CoreWarning, "core", file, 2);
        request.note_error_in(Level::CoreError, "core", file, 3);
        request.note_error_in(Level::Warning, "masked", file, 4);
        request.note_error_in(Level::Error, "masked", file, 5);
        assert_eq!(
            String::from_utf8_lossy(&request.take_body()),
            "\nWarning: core in witness.php on line 2\n\nFatal error: core in witness.php on line 3\nStack trace:\n#0 {main}\n"
        );
    }

    /// php's second html format (`Z/main/main.c:1475`), the one every
    /// fatal level but `E_ERROR` and `E_PARSE` takes: the message as it
    /// is, and the trace between the `<br />` and the newline. What the
    /// pinned oracle writes for `-d html_errors=1` over
    /// `<?php\nini_set('fatal_error_backtraces', '1'); trigger_error('b<o>om', E_USER_ERROR);`
    /// is the 8.4 deprecation and then
    /// `<br />\n<b>Fatal error</b>:  b<o>om in <b>%s</b> on line <b>2</b><br />\nStack trace:\n#0 %s(2): trigger_error('b<o>om', 256)\n#1 {main}\n`;
    /// no native here raises `E_USER_ERROR`, so the request has no frame
    /// and the trace is `{main}` alone. The same level under
    /// `fatal_error_backtraces=0` fetches none, and the two formats then
    /// write the same bytes but for the escaping.
    #[test]
    fn the_second_html_format_carries_the_trace_after_the_line() {
        let file = b"witness.php";
        let engine = Engine::builder().ini("html_errors", "1").build();
        let mut req = request(&engine);
        req.note_error_in(Level::UserError, "b<o>om", file, 2);
        req.note_error_in(Level::Error, "b<o>om", file, 3);
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<br />\n<b>Fatal error</b>:  b<o>om in <b>witness.php</b> on line <b>2</b><br />\nStack trace:\n#0 {main}\n\
             <br />\n<b>Fatal error</b>:  b&lt;o&gt;om in <b>witness.php</b> on line <b>3</b><br />\n"
        );

        let engine = Engine::builder()
            .ini("html_errors", "1")
            .ini("fatal_error_backtraces", "0")
            .build();
        let mut req = request(&engine);
        req.note_error_in(Level::UserError, "b<o>om", file, 2);
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<br />\n<b>Fatal error</b>:  b<o>om in <b>witness.php</b> on line <b>2</b><br />\n"
        );
    }

    /// The log form and the xml fault carry the trace after the site as
    /// the plain form does, `ZSTR_LEN(backtrace) ? "\nStack trace:\n" : ""`
    /// in each of `php_error_cb`'s `spprintf` calls. What the pinned
    /// oracle writes for `-d log_errors=1 -d display_errors=0` over
    /// `<?php\n(unset)$x;` is
    /// `PHP Fatal error:  The (unset) cast is no longer supported in %s on line 2\nStack trace:\n#0 {main}`,
    /// and for `-d xmlrpc_errors=1` the fault's string is
    /// `Fatal error:The (unset) cast … on line 2\nStack trace:\n#0 {main}`.
    #[test]
    fn the_log_form_and_the_xml_fault_carry_the_trace() {
        let file = b"witness.php";
        let engine = Engine::builder()
            .ini("log_errors", "1")
            .ini("xmlrpc_errors", "1")
            .build();
        let mut req = request(&engine);
        req.note_error_in(Level::CompileError, "refused", file, 2);
        assert_eq!(
            req.take_aside(),
            vec![super::Aside::Log(
                b"PHP Fatal error:  refused in witness.php on line 2\nStack trace:\n#0 {main}"
                    .to_vec()
            )]
        );
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>0</int></value></member><member><name>faultString</name><value><string>Fatal error:refused in witness.php on line 2\nStack trace:\n#0 {main}</string></value></member></struct></value></fault></methodResponse>"
        );
    }

    /// `smart_str_append_escaped`'s table, one byte per arm: the seven
    /// named escapes, the hex form for the rest of the control range and
    /// everything past ASCII, and a printable byte untouched.
    #[test]
    fn a_trace_escapes_a_string_as_smart_str_does() {
        let mut out = String::new();
        append_escaped(&mut out, b"a\n\r\t\x0c\x0b\\\x1b\x01\x7f\xff'z");
        assert_eq!(out, "a\\n\\r\\t\\f\\v\\\\\\e\\x01\\x7F\\xFF'z");
    }

    /// php's `error_type_str` for every level `Level` names, the switch
    /// at `Z/main/main.c:1414-1448` read case by case. No PHP source
    /// raises `E_RECOVERABLE_ERROR` or `E_STRICT` under php 8.5, as the
    /// rustdoc beside `level_name` says, so this is the one place the
    /// two words are held.
    #[test]
    fn every_level_is_named_as_php_names_it() {
        for (level, name) in [
            (Level::Error, "Fatal error"),
            (Level::CoreError, "Fatal error"),
            (Level::CompileError, "Fatal error"),
            (Level::UserError, "Fatal error"),
            (Level::RecoverableError, "Recoverable fatal error"),
            (Level::Warning, "Warning"),
            (Level::CoreWarning, "Warning"),
            (Level::CompileWarning, "Warning"),
            (Level::UserWarning, "Warning"),
            (Level::Parse, "Parse error"),
            (Level::Notice, "Notice"),
            (Level::UserNotice, "Notice"),
            (Level::Deprecated, "Deprecated"),
            (Level::UserDeprecated, "Deprecated"),
            (Level::Strict, "Unknown error"),
        ] {
            assert_eq!(level_name(level), name, "{level:?}");
        }
    }

    /// What php's `escape_html` writes, from the pinned oracle under the
    /// witness recipe with `-d html_errors=1`: `<?php\necho <;` is a
    /// parse error whose message names the token it did not want, and
    /// php writes `syntax error, unexpected token &quot;&lt;&quot;`.
    /// `htmlspecialchars($s, ENT_COMPAT)` over `<a&b'c"d` under the same
    /// oracle is `&lt;a&amp;b'c&quot;d`, which is the whole set: the
    /// apostrophe `ENT_QUOTES` would take stands as it is.
    #[test]
    fn the_message_of_a_fatal_is_escaped_as_php_escapes_it() {
        assert_eq!(
            escape_html("syntax error, unexpected token \"<\""),
            "syntax error, unexpected token &quot;&lt;&quot;"
        );
        assert_eq!(escape_html("<a&b'c\"d"), "&lt;a&amp;b'c&quot;d");
        assert_eq!(
            escape_html("&amp; &gt;"),
            "&amp;amp; &amp;gt;",
            "php passes double_encode, so a written entity is written again"
        );
        assert_eq!(escape_html("nothing to escape"), "nothing to escape");
    }
}
