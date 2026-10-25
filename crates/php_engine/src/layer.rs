//! The stream layer's face of the engine (design §4.3 seam 5; #1959):
//! what `php_streams`, php's `main/streams/` and `main/network.c` above
//! the engine, reaches beside the native face, and no extension does.
//! `php_ext` re-exports nothing here, so an extension cannot name it, and
//! `api-check` fails a crate other than `php_streams` that `use`s it;
//! `layer.lock` lists every item.
//!
//! Three kinds of item: [`Face`], the request's operations the layer
//! calls, a sealed trait implemented for [`Request`] alone, since a path
//! `php_ext` does not re-export cannot hide a method of a type it does;
//! [`StreamHooks`], what the engine asks of the layer in turn; and
//! the system calls `main/network.c` and the plain files kind make over
//! the fds a stream holds, `php_process`'s bindings in its `unsafe` home
//! (#1935), which the layer has no edge of its own to (#1874).

use std::any::Any;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::{HostPath, Request, Standard, Unresolved};
use crate::api::values::{ClassId, Owner, PhpObject, PhpString, Resource, Value};

pub use crate::api::request::paths::scheme_length;
pub use crate::api::request::temporary::TemporaryGuard;
pub use php_process::{
    BUFSIZ, Readiness, SockOpt, bind_fd, connect_fd, dup_fd, fd_table_size, inherit_fd, listen_fd,
    new_socket, peek_fd, poll_fd, set_fd_blocking, set_sockopt, sync_fd,
};

/// What the engine asks of the stream layer above it, `php_streams`,
/// handed with [`ExtensionBuilder::stream_hooks`](crate::api::abi::ExtensionBuilder::stream_hooks):
/// php's `zend_stream_open_function` and `zend_resolve_path` as
/// `php_module_startup` points them at main's streams, opcache's open of
/// a file it holds, and the standard streams `do_cli` registers and reads
/// stdin through; and the layer's engine state, which the engine builds
/// and ends for it. With none the engine is Zend alone: an include opens
/// a plain file and nothing else, a path resolves by `realpath(3)`
/// (`zend_resolve_path_func`), no standard stream is registered and no
/// stream outlives its request.
///
/// Unlike the hooks an extension supplies for its own behaviour, these
/// are not served as their supplier: the layer is main's streams, which
/// php calls on behalf of whatever opened the stream, an include,
/// `fopen()` or the CLI's stdin. So what it allocates is tagged with the
/// code that asked, as a stream php opens is charged to that request's
/// operation, and the wrappers, filters and kinds an extension supplies
/// to the layer are served as theirs (#2113, #2864).
#[derive(Debug, Clone, Copy)]
pub struct StreamHooks {
    /// `php_stream_open_for_zend_ex(USE_PATH | REPORT_ERRORS |
    /// STREAM_OPEN_FOR_INCLUDE)` and the whole read after it: the file a
    /// path names read through its wrapper, with the `*opened_path` its
    /// opener set; `None` for the open or the read that failed, which
    /// warned. The `Err` is what the open threw.
    pub open: StreamOpenHook,
    /// `php_resolve_path_for_zend`: `zend_resolve_path` past every
    /// extension's own
    /// ([`ExtensionBuilder::resolve_path`](crate::api::abi::ExtensionBuilder::resolve_path)).
    pub resolve_path: fn(&mut Request, &[u8]) -> Result<Vec<u8>, Unresolved>,
    /// opcache's `zend_get_stream_timestamp`
    /// (`Z/ext/opcache/ZendAccelerator.c:944-978`): the stat of the file a
    /// wrapper's path names, as its wrapper's `url_stat` reports it asked
    /// quietly, laid out as `php_fstat` lays it out, which the unit cache
    /// stamps and validates a file it holds by; `st_mtime` one and the
    /// rest zero for a wrapper with no `url_stat`, and `None` where the
    /// stat failed.
    pub timestamp: fn(&mut Request, &[u8]) -> Option<[i64; 13]>,
    /// The stream opcache's `zend_stream_open` of a file it holds makes
    /// and frees: its resource's number spent.
    pub spend: fn(&mut Request),
    /// `php_fopen_primary_script`'s `zend_stream_open` of the host's
    /// program where opcache does not hold it
    /// (`Z/main/fopen_wrappers.c:468-473`): the file opened as
    /// `php_stream_open_for_zend_ex` opens it, a stream among the
    /// request's resources, held as the host's `zend_file_handle` until
    /// [`StreamHooks::close_primary`], its fd inherited by a process the
    /// request execs meanwhile; the `*opened_path` its opener set, which
    /// `EG(included_files)` holds the program under, or `None` where the
    /// file did not open. The `Err` is what the open threw.
    pub open_primary: fn(&mut Request, &[u8]) -> Result<Option<PhpString>, Thrown>,
    /// `zend_destroy_file_handle` over what
    /// [`StreamHooks::open_primary`] opened.
    pub close_primary: fn(&mut Request),
    /// `cli_register_file_handles`: the streams over the host's three
    /// standard handles, `STDIN`, `STDOUT` and `STDERR` in that order, or
    /// `None` where one of them could not be opened; with `true` once the
    /// host's stdin was replaced under them, as phpdbg's `run <file`
    /// replaces it.
    pub standard_handles: StandardHandlesHook,
    /// `php_stream_get_line` on the stream `value` names, as `do_cli`
    /// reads `STDIN` a line at a time for `-R`, `-F` and `-B`; `None` at
    /// its end, or where it names no stream.
    pub get_line: fn(&mut Request, &Value) -> Option<Vec<u8>>,
    /// The layer's engine state, built once as the engine is
    /// ([`Face::layer_state`]): what streams keep across requests, php's
    /// `EG(persistent_list)` as `zend_startup`'s `zend_init_rsrc_plist`
    /// sets it up (#1962). The engine holds it and cannot name its type;
    /// the layer reads it back as the type it is.
    pub state: fn() -> Arc<dyn Any + Send + Sync>,
    /// `zend_shutdown`'s `zend_destroy_rsrc_list(&EG(persistent_list))`
    /// (`Z/Zend/zend.c:1166`): served on the engine-end request before
    /// any extension's engine end, so the layer's idle persistent
    /// sockets close first.
    pub engine_end: fn(&mut Request),
}

/// [`StreamHooks::open`].
pub type StreamOpenHook = fn(&mut Request, &[u8]) -> Result<Option<IncludeSource>, Thrown>;

/// What [`StreamHooks::open`] read for an include: the file's bytes, and
/// the `*opened_path` the wrapper's opener set, which the file is
/// compiled under where it set one.
#[derive(Debug)]
pub struct IncludeSource {
    pub source: Vec<u8>,
    pub opened_path: Option<Vec<u8>>,
}

/// [`StreamHooks::standard_handles`].
/// Each stream's one owner the caller's, which the constant it registers
/// holds.
pub type StandardHandlesHook = fn(&mut Request, bool) -> Result<Option<[Owner<Value>; 3]>, Thrown>;

mod sealed {
    /// Implemented for the request alone, so that [`super::Face`] is the
    /// engine's to grow and no other crate's to implement.
    pub trait Sealed {}
    impl Sealed for crate::api::request::Request {}
}

/// The request's operations the stream layer calls and no extension
/// does, each but [`Face::layer_state`] the engine's own method of the
/// name, which the engine calls as well and documents where it is
/// written: the host's standard fds, `php://input` and `php://memory`'s
/// limit, the open of a temporary file and the path past the
/// `open_basedir` guard, the wrapper's warning and `realpath`, a
/// user-space wrapper's calls into PHP code and what a throwable left
/// pending, and the layer's own request state, read shared, and engine
/// state.
pub trait Face: sealed::Sealed {
    /// The fd the host handed under one of its standard streams, `None`
    /// where it handed none.
    #[must_use]
    fn host_fd(&self, standard: Standard) -> Option<BorrowedFd<'_>>;
    /// Whether the host's standard fds are the process's own, as php's
    /// CLI registers them.
    #[must_use]
    fn host_fds_are_process(&self) -> bool;
    /// `fclose()` reaching the stream a standard stream's constant holds.
    fn close_host_fd(&mut self, standard: Standard);
    /// What `php://input` reads.
    fn input_bytes(&mut self) -> Vec<u8>;
    /// The heap's memory limit in bytes, `PG(memory_limit)`.
    fn heap_memory_limit(&self) -> usize;
    /// `php_open_temporary_fd_ex`: the file made and its path.
    fn open_temporary_fd(
        &mut self,
        dir: Option<&[u8]>,
        prefix: &[u8],
        check: TemporaryGuard,
    ) -> Option<(std::fs::File, std::path::PathBuf)>;
    /// The path under `STREAM_DISABLE_OPEN_BASEDIR`.
    fn unguarded_host_path(&self, path: &[u8]) -> HostPath;
    /// `tsrm_realpath` of a path that is a regular file.
    fn realpath_file(&mut self, path: &[u8]) -> Result<Vec<u8>, std::io::Error>;
    /// `php_stream_display_wrapper_errors`' warning.
    fn wrapper_warning(&mut self, function: &str, path: &[u8], text: &str);
    /// Whether `EG(exception)` holds a throwable or an exit, or the
    /// request is leaving for the `zend_try` that catches a fatal.
    fn is_pending(&self) -> bool;
    /// The tail of `zend_call_function` where the callee left something
    /// pending and no frame stands to take it.
    fn thrown_without_frame(&mut self, thrown: &Thrown) -> Result<Value, Thrown>;
    /// `zend_call_function` over a method, as a user-space wrapper's
    /// calls are served.
    fn serve_method(
        &mut self,
        function: Function,
        this: Option<PhpObject>,
        called_scope: ClassId,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Owner<Value>, Thrown>;
    /// `object_init_ex`'s refusal of a class that cannot be instantiated.
    fn instantiable(&mut self, class: ClassId) -> Result<(), Thrown>;
    /// The request state of type `T` where it stands, read through a
    /// request held shared.
    #[must_use]
    fn state_ref<T: Send + 'static>(&self) -> Option<&T>;
    /// The layer's engine state ([`StreamHooks::state`]) as the `T` it
    /// is, shared, so a request state can hold it past the request's
    /// last borrow; `None` with no stream layer supplied, or for another
    /// type.
    #[must_use]
    fn layer_state<T: Send + Sync + 'static>(&self) -> Option<Arc<T>>;
    /// The resource's owner handed to `EG(regular_list)`, which keeps it
    /// to the request's end: the list's close serves its destructor
    /// (`zend_close_rsrc_list`) and its destruction frees it, what php
    /// does with a resource it names by pointer alone,
    /// `FG(default_context)` (#2246). The id answered names the resource
    /// with no owner, read back with [`Memory::resource`].
    fn keep_in_list(&self, resource: Owner<Resource>) -> i64;
}

impl Face for Request {
    fn host_fd(&self, standard: Standard) -> Option<BorrowedFd<'_>> {
        Request::host_fd(self, standard)
    }
    fn host_fds_are_process(&self) -> bool {
        Request::host_fds_are_process(self)
    }
    fn close_host_fd(&mut self, standard: Standard) {
        Request::close_host_fd(self, standard);
    }
    fn input_bytes(&mut self) -> Vec<u8> {
        Request::input_bytes(self)
    }
    fn heap_memory_limit(&self) -> usize {
        Request::heap_memory_limit(self)
    }
    fn open_temporary_fd(
        &mut self,
        dir: Option<&[u8]>,
        prefix: &[u8],
        check: TemporaryGuard,
    ) -> Option<(std::fs::File, std::path::PathBuf)> {
        Request::open_temporary_fd(self, dir, prefix, check)
    }
    fn unguarded_host_path(&self, path: &[u8]) -> HostPath {
        Request::unguarded_host_path(self, path)
    }
    fn realpath_file(&mut self, path: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        Request::realpath_file(self, path)
    }
    fn wrapper_warning(&mut self, function: &str, path: &[u8], text: &str) {
        Request::wrapper_warning(self, function, path, text);
    }
    fn is_pending(&self) -> bool {
        Request::is_pending(self)
    }
    fn thrown_without_frame(&mut self, thrown: &Thrown) -> Result<Value, Thrown> {
        Request::thrown_without_frame(self, thrown)
    }
    #[cfg_attr(debug_assertions, track_caller)]
    fn serve_method(
        &mut self,
        function: Function,
        this: Option<PhpObject>,
        called_scope: ClassId,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Owner<Value>, Thrown> {
        Request::serve_method(self, function, this, called_scope, arguments, extra_named)
    }
    fn instantiable(&mut self, class: ClassId) -> Result<(), Thrown> {
        Request::instantiable(self, class)
    }
    fn state_ref<T: Send + 'static>(&self) -> Option<&T> {
        Request::state_ref(self)
    }
    fn layer_state<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        Arc::clone(self.shared.layer.as_ref()?).downcast().ok()
    }
    fn keep_in_list(&self, resource: Owner<Resource>) -> i64 {
        resource.hand_on().id()
    }
}
