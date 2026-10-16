//! `Engine` and `serve` (design §5.14): the process-wide, `Sync` engine
//! and the one request transaction every host crosses. It compiles the
//! source through `php_compile`, builds the request with every
//! extension's request state, serves `rinit`, hands the main op array's
//! entry frame to the tier-bound loop, serves `rshutdown` in reverse,
//! renders what php renders, and hands the bytes to the sink.
//!
//! The engine's own declarations go in first: the natives design §13
//! moves out of `ext/standard` and the throwable classes Zend declares,
//! so an engine with no extensions serves a program that uses Zend alone
//! (design §4.10).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::any::Any;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use php_compile::multibyte::Multibyte;
use php_compile::{CompileError, RequestClasses, SourceKind, StackLimit};
use php_heap::{Intern, Strings, Zval};
use php_link::{Exact, Folded, Indexed, LinkedClass, NameMap, Names};
use php_unit::decl::{
    ClassDecl, ConstDecl, ConstExpr, FunctionDecl, HookKind, MethodDecl, Name, ParameterDefault,
    Type,
};
use php_unit::{FramelessEntry, Unit, frameless_table};

use crate::api::abi::{
    Declarations, DependencyKind, EndFunction, Entry, Extension, ExtensionEntry, IniStage,
    NativeNew, ObjectHandlers, OnModify, Refusal, Start, StartupIni, States, Validation,
};
use crate::api::call::Function;
use crate::api::raising::{Level, Thrown};
use crate::api::request::{
    BuildDefs, Ending, ExtensionId, HandlePath, LongEntry, LongHandler, Placed, PropertyInfo,
    Request, TypedIni, placed_of,
};
use crate::api::values::{ClassId, Value};
use crate::exec;
use crate::ini_syntax::{IniEvent, IniScanner, IniValue};
use crate::natives::StubPlace;
use crate::supplied::{Keyed, Resolved, Suppliers};
use crate::tier::{BindLoops, Loop, Loops, Nop, Tier};
pub(crate) use crate::unit_cache::Cached;
use crate::unit_cache::UnitCache;

/// Everything a host knows about one request before it starts (design
/// §4.3 seam 4): one plain `Send` value, handed over once.
#[derive(Debug, Default)]
pub struct RequestDescription {
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    /// The request's own variables, what `sapi_module.getenv` answers
    /// (php-fpm's `FastCGI` parameters), in the order the host walks
    /// them: `getenv()` reads them before the environment, and `$_SERVER`
    /// and `$_ENV` register them after it.
    pub params: Vec<(Vec<u8>, Vec<u8>)>,
    pub cwd: PathBuf,
    /// The host's rows of `$_SERVER` past the environment, which the
    /// engine imports first for every host
    /// (`sapi_module.register_server_variables`).
    pub server: Vec<(Vec<u8>, Vec<u8>)>,
    pub sapi_name: String,
    /// The request of the web the host serves, what php's
    /// `SG(request_info)` carries past the command line; `None` for the
    /// CLI, which serves none and whose `header_handler` takes no line.
    pub web: Option<WebRequest>,
    /// The served file's name, for `$_SERVER['SCRIPT_NAME']` and error text.
    pub script_name: Option<Vec<u8>>,
    /// The source to serve, when the host has it in hand (`php -r`, the
    /// in-process tests); else the file is read by its name.
    pub source: Option<Vec<u8>>,
    /// The CLI's standard handles, which `do_cli` registers after
    /// `php_request_startup` and php's other hosts have none of; `None`
    /// registers nothing.
    pub file_handles: Option<FileHandles>,
    /// `PHP_CLI_PROCESS_TITLE`, which php's CLI alone registers.
    pub process_title: ProcessTitle,
    /// The entries the host sets before the request starts, each as its
    /// [`IniOrigin`] says, in the order given: php-fpm's `PHP_VALUE` and
    /// `PHP_ADMIN_VALUE` and the lines of the request's `.user.ini` files.
    pub ini: Vec<(Vec<u8>, Vec<u8>, IniOrigin)>,
    /// How the program is served.
    pub primary: Primary,
    /// How the host hands a wrapped program's file over, which decides
    /// whether the request holds it open, and until when.
    pub program_file: ProgramFile,
    /// Whether a wrapped program is served in its own directory, the
    /// request starting in [`RequestDescription::cwd`] either way.
    pub chdir: Chdir,
    /// php's `EG(exit_status)` as the request starts: zero, as
    /// `init_executor` writes it, but where a host serves one request of
    /// php's as several of the engine's and hands each the status the
    /// one before it left, as phpdbg's cycle keeps one request standing
    /// across its commands (`Z/sapi/phpdbg/phpdbg.c:1578-1619`).
    pub exit_status: i32,
}

/// How a host has its program served: wrapped, as `php_execute_script`
/// serves it for every host of php's but one; or bare, as php's phpdbg
/// serves it, the file compiled with `zend_compile_file` and the op array
/// handed to `zend_execute` (`Z/sapi/phpdbg/phpdbg_prompt.c:534-563, 866`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Primary {
    /// `php_execute_script`: `auto_prepend_file` and `auto_append_file`
    /// around the program, its path expanded and put in
    /// `EG(included_files)`, and what it leaves uncaught rendered by
    /// `zend_exception_error` once its handler has had it.
    #[default]
    Wrapped,
    /// `zend_compile_file` and `zend_execute`: the program alone, under
    /// the name the host gave, in no table of included files, and what
    /// it leaves uncaught, once its handler has had it, kept for the host
    /// to render in [`Observables::uncaught`], as `phpdbg_handle_exception`
    /// renders it.
    Bare,
}

/// The `zend_file_handle` a host hands `php_execute_script` for the
/// program it names, as the request holds it: open, and inherited by a
/// process the request execs, while the program is served, since php
/// opens it with no `O_CLOEXEC` (#2458). A program the host has in hand,
/// `-r` or stdin, has no file, and a bare one is closed before it is
/// served, as `phpdbg_compile` closes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProgramFile {
    /// The file's name, which `zend_stream_open` opens before the
    /// program is served unless opcache holds the file,
    /// `persistent_stream_open_function` answering it without opening
    /// it: the `zend_file_handle` of `php_fopen_primary_script`
    /// (`Z/main/fopen_wrappers.c:468-473`), php-cgi's, which the host
    /// destroys once the program has been served.
    #[default]
    Named,
    /// The file's name alone, as `zend_stream_init_filename` leaves it
    /// and the host hands it to `php_execute_script` unopened: the
    /// built-in server's (`php_cli_server_dispatch_script`,
    /// `Z/sapi/cli/php_cli_server.c:2132-2138`), opened as the program
    /// compiles, once `auto_prepend_file` is served, where the unit cache
    /// does not answer it by its name, as the server's router is
    /// ([`Engine::dispatch`]), and destroyed once the program has been
    /// served.
    Unopened,
    /// php's CLI, `cli_seek_file_begin`'s `fopen` before the request
    /// starts (`Z/sapi/cli/php_cli.c:566`), open whatever opcache holds,
    /// and destroyed once the program has been served, before
    /// `php_request_shutdown` (`:1165-1170`).
    Opened,
    /// php-fpm's: as [`ProgramFile::Named`], but a file opened is
    /// closed only as the request ends, since `fpm_main` destroys no
    /// `zend_file_handle` the scanner has listed
    /// (`Z/sapi/fpm/fpm/fpm_main.c:1905`), and `shutdown_executor` closes
    /// the list after the shutdown functions and destructors.
    Listed,
}

impl ProgramFile {
    /// Whether `php_execute_script_ex` puts the program's file in
    /// `EG(included_files)` before `auto_prepend_file`, as it does for a
    /// `zend_file_handle` the host opened as a `FILE *` and for no other
    /// (`Z/main/main.c:2610-2619`); any other enters the table as it
    /// compiles ([`Request::note_compiled_program`]).
    pub(crate) fn noted_before_prepend(self) -> bool {
        self == ProgramFile::Opened
    }
}

/// Where `php_execute_script` serves a wrapped program
/// (`Z/main/main.c:2596-2604`): in the working directory the request
/// started in, under `SAPI_OPTION_NO_CHDIR`, or in the program's own
/// directory, `VCWD_CHDIR_FILE` over its name before `auto_prepend_file`,
/// and the directory before it restored once the program and
/// `auto_append_file` have been served. What request startup resolves, a
/// relative `upload_tmp_dir` among it, is resolved in the directory the
/// request started in, and so are the shutdown functions and destructors
/// after.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Chdir {
    /// `SAPI_OPTION_NO_CHDIR`: php's CLI, and php-cgi's `-C`.
    #[default]
    Stay,
    /// The program's directory, as php-fpm and php-cgi serve a program:
    /// its name cut at its last `/`, as `virtual_chdir_file` cuts it,
    /// resolved against the working directory as `chdir(2)` resolves it.
    IntoProgramDir,
}

/// A throwable the program left uncaught, for a host that renders it:
/// its class, its `file` and `line`, and what its `__toString` answered,
/// empty where that threw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uncaught {
    pub class: Vec<u8>,
    pub file: Vec<u8>,
    pub line: i64,
    pub text: Vec<u8>,
}

/// How a host sets an entry for a request, php-fpm's three ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IniOrigin {
    /// `php_value`: the value, whatever the entry's `modifiable`, as
    /// `fpm_php_zend_ini_alter_master` sets it under `ZEND_INI_USER`.
    Value,
    /// `php_admin_value`: the value, and the entry `PHP_INI_SYSTEM` after
    /// it, so neither `ini_set()` nor a `.user.ini` moves it.
    Admin,
    /// A `.user.ini` line, `php_ini_activate_config` at
    /// `PHP_INI_STAGE_HTACCESS`: taken where the entry is
    /// `PHP_INI_PERDIR` and no admin value holds it.
    PerDir,
}

/// A value a host hands a request, made a PHP value there: what
/// `fpm_get_status()` answers from the process manager's shared table.
#[derive(Debug, Clone, PartialEq)]
pub enum HostValue {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    /// An array of string keys, in order.
    Map(Vec<(Vec<u8>, HostValue)>),
    /// An array keyed 0, 1, and on.
    List(Vec<HostValue>),
}

/// `PHP_CLI_PROCESS_TITLE` for a request: which `do_cli` registers under
/// `Core` after `php_request_startup` as whether `cli_set_process_title`
/// can set the title, that is whether the host saved the area `ps` shows
/// the command line from (`save_ps_args`, `Z/sapi/cli/php_cli.c:928`), and
/// which no other host of php's registers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProcessTitle {
    /// Not registered: every host but the CLI.
    #[default]
    Unregistered,
    /// `false`: the CLI saved no area.
    Unsaved,
    /// `true`: the CLI saved the area.
    Saved,
}

/// One request of the web as its host received it, php's
/// `SG(request_info)` and the response state a host sets before
/// `php_request_startup` (`Z/main/SAPI.h:73-120`): what php's variable
/// registration reads for `$_GET`, `$_POST`, `$_COOKIE` and `$_FILES`
/// (`Z/main/php_variables.c`) and `sapi_activate` for the body
/// (`Z/main/SAPI.c:430`).
#[derive(Debug, Default)]
pub struct WebRequest {
    /// `request_method`; `None` where the host was given none, as php-cgi
    /// started from a shell is.
    pub method: Option<Method>,
    /// `request_uri`, the path the request asked for.
    pub request_uri: Option<Vec<u8>>,
    /// `query_string`, what `$_GET` is parsed from.
    pub query: Option<Vec<u8>>,
    /// `cookie_data`, the `Cookie` header, what `$_COOKIE` is parsed from.
    pub cookies: Option<Vec<u8>>,
    /// `content_type`, which picks the body's reader.
    pub content_type: Option<Vec<u8>>,
    /// `content_length`, as the host was told it.
    pub content_length: i64,
    /// The body, what `sapi_module.read_post` hands out.
    pub body: Body,
    /// The request headers, what `getallheaders()` answers.
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// `no_headers`: the host writes no header block, and the request
    /// starts with its headers counted sent (php-cgi's `-q`).
    pub no_headers: bool,
    /// `SG(sapi_headers).http_response_code` as the host leaves it
    /// before the request starts: 200, or php-cgi's 404 for a file it
    /// could not open.
    pub response_code: i64,
    /// The ini entries the host's `send_headers` reads at the send, as
    /// php's CGI hosts read `cgi.nph` and `cgi.rfc2616_headers`: each
    /// one's value then rides in [`ResponseHead::ini`].
    pub head_ini: &'static [&'static [u8]],
}

/// What a host's `sapi_module.read_post` hands out over one request
/// (`Z/main/SAPI.c:232`, `sapi_read_post_block`): the engine pulls it as
/// the request asks, when the content type's reader takes the form at
/// request start, when `php://input` is read, when `request_parse_body()`
/// runs, and never where none of them does, so a body declared over
/// `post_max_size` is never read (`Z/main/SAPI.c:258`).
pub enum Body {
    /// The bytes the host reads, no more than `content_length`, from a
    /// reader the request pulls blocks from as php's `read_post` pulls
    /// them: a read that answers fewer than it was asked for ends the
    /// body, as php's `SG(post_read)`, and so does one that fails. A body
    /// the host read before the request started is a reader over those
    /// bytes, [`Body::from`].
    Stream(Box<dyn std::io::Read + Send>),
    /// Nothing, and the `E_WARNING` the host's read raises each time it
    /// is asked, as php-fpm's `sapi_cgi_read_post` raises
    /// `REQUEST_BODY_FILE: open('%s') failed: %s (%d)` where the file the
    /// server names does not open (`Z/sapi/fpm/fpm/fpm_main.c:448`).
    Refused(String),
}

impl From<Vec<u8>> for Body {
    /// A body the host read whole before the request started.
    fn from(bytes: Vec<u8>) -> Body {
        Body::Stream(Box::new(std::io::Cursor::new(bytes)))
    }
}

impl Default for Body {
    /// No body.
    fn default() -> Body {
        Body::Stream(Box::new(std::io::empty()))
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Stream(_) => f.write_str("Stream"),
            Body::Refused(warning) => f.debug_tuple("Refused").field(warning).finish(),
        }
    }
}

/// A request's method, `request_method`, read as `sapi_activate` and
/// `php_auto_globals_create_post` compare it: `POST` and `HEAD` are the
/// two php acts on, spelled exactly; any other stands as the host was
/// given it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Other(Vec<u8>),
}

impl Method {
    /// The method a host was handed.
    #[must_use]
    pub fn of(name: &[u8]) -> Method {
        match name {
            b"GET" => Method::Get,
            b"HEAD" => Method::Head,
            b"POST" => Method::Post,
            b"PUT" => Method::Put,
            other => Method::Other(other.to_vec()),
        }
    }

    /// The method as the host was handed it.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        match self {
            Method::Get => b"GET",
            Method::Head => b"HEAD",
            Method::Post => b"POST",
            Method::Put => b"PUT",
            Method::Other(name) => name,
        }
    }
}

/// What a request answers its host with before the body, at the commit
/// `sapi_send_headers` makes (`Z/main/SAPI.c:861`): the response code,
/// the `HTTP/` status line `header()` was given where it was, and the
/// header lines in the order `sapi_header_add_op` took them. How it goes
/// on the wire is the host's: php's CGI writes a `Status:` line from the
/// code, the dev server an HTTP status line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseHead {
    /// `SG(sapi_headers).http_response_code`.
    pub code: i64,
    /// `SG(sapi_headers).http_status_line`.
    pub status_line: Option<Vec<u8>>,
    /// `SG(sapi_headers).headers`, each line without its line end.
    pub lines: Vec<Vec<u8>>,
    /// The entries [`WebRequest::head_ini`] names, each with its value at
    /// the send.
    pub ini: Vec<(&'static [u8], Vec<u8>)>,
}

/// What `do_cli` registers for a request it serves a program in
/// (`Z/sapi/cli/php_cli.c:926-980` at 8.5.9), and the host's fds under
/// it. `PHP_CLI_PROCESS_TITLE` first, then `cli_register_file_handles`
/// (`:519`): `php://stdin`, `php://stdout` and `php://stderr` opened in
/// that order, which makes them the request's resources 1, 2 and 3, and
/// the constants `STDIN`, `STDOUT` and `STDERR` on them, each under
/// `Core`. `do_cli`'s `-l` registers no stream, so a host leaves this
/// `None` for a lint, and php's CGI and `FastCGI` hosts have none of it.
///
/// What is written to the streams goes to the sink, `STDOUT` past the
/// `php_output` layer and `STDERR` onto its error stream (design
/// revision 16), and `STDIN` reads the host's fd 0 as php's CLI reads
/// it, through the plain file's kind over a copy. The fds are what
/// `fstat` and `ftruncate` on the three ask, as php's `STDIO` kind asks
/// `fstat(2)` and `ftruncate(2)` of fds 0, 1 and 2; a host with no fd for
/// one of them hands `None`, which `fstat(2)` and `ftruncate(2)` answer
/// as they answer a closed fd, so both answer `false`.
///
/// `fclose()` on one of the three constants closes what is under it, as
/// php's closes fd 0, 1 or 2 itself: a later `php://stdin`, `stdout` or
/// `stderr` fails to open, and where the host handed the process's own
/// fds ([`FileHandles::process`]) the fd is closed, so what the process
/// opens next takes its number, as under php (`sapi/cli/tests/gh8827-*`).
#[derive(Debug, Clone)]
pub struct FileHandles {
    fds: Arc<Fds>,
}

/// The fds under the three, and whose they are.
#[derive(Debug)]
struct Fds {
    fds: [Option<std::os::fd::OwnedFd>; 3],
    process: bool,
}

impl Drop for Fds {
    /// Copies are closed with the last description that holds them; the
    /// process's own are left open, as php's CLI leaves fds 0, 1 and 2
    /// open at the end of a request (`PHP_STREAM_FLAG_NO_RSCR_DTOR_CLOSE`).
    fn drop(&mut self) {
        if self.process {
            for fd in self.fds.iter_mut().filter_map(Option::take) {
                let _left_open = std::os::fd::IntoRawFd::into_raw_fd(fd);
            }
        }
    }
}

/// Which of the host's three standard streams a [`FileHandles`] slot
/// holds, fd 0, 1 or 2 under php's CLI, as a `php://` path names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standard {
    Stdin = 0,
    Stdout = 1,
    Stderr = 2,
}

impl FileHandles {
    /// The three over copies of the host's fds, as `dup(2)` makes them,
    /// closed when the last description holding them is dropped.
    #[must_use]
    pub fn new(
        stdin: Option<std::os::fd::OwnedFd>,
        stdout: Option<std::os::fd::OwnedFd>,
        stderr: Option<std::os::fd::OwnedFd>,
    ) -> FileHandles {
        FileHandles {
            fds: Arc::new(Fds {
                fds: [stdin, stdout, stderr],
                process: false,
            }),
        }
    }

    /// The three over the process's own fds 0, 1 and 2, as php's CLI
    /// registers them: closed by `fclose()` on the constant alone, and
    /// left open when the request ends; `php://stdout` and `php://stderr`
    /// opened again are copies of them, as php's are.
    #[must_use]
    pub fn process(
        stdin: Option<std::os::fd::OwnedFd>,
        stdout: Option<std::os::fd::OwnedFd>,
        stderr: Option<std::os::fd::OwnedFd>,
    ) -> FileHandles {
        FileHandles {
            fds: Arc::new(Fds {
                fds: [stdin, stdout, stderr],
                process: true,
            }),
        }
    }

    /// The fd under one of the three, `None` where the host handed none.
    pub(crate) fn fd(&self, standard: Standard) -> Option<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.fds.fds[standard as usize].as_ref().map(AsFd::as_fd)
    }

    /// Whether the fds are the process's own.
    pub(crate) fn is_process(&self) -> bool {
        self.fds.process
    }

    /// The fd under the `at`th of the three closed, where this
    /// description is the only one holding it.
    pub(crate) fn close(&mut self, at: usize) {
        if let Some(fd) = Arc::get_mut(&mut self.fds).and_then(|fds| fds.fds.get_mut(at)) {
            *fd = None;
        }
    }

    /// The three with `stdin` under the first and copies of the other
    /// two, as phpdbg's `php://stdin` opens its `stdin_file`.
    pub(crate) fn set_stdin(&mut self, stdin: std::os::fd::OwnedFd) {
        let copy = |at: usize| self.fds.fds[at].as_ref().and_then(|fd| fd.try_clone().ok());
        *self = FileHandles::new(Some(stdin), copy(1), copy(2));
    }
}

/// A body write the host could not make, php's hosts'
/// `php_handle_aborted_connection` (`Z/main/main.c:2722`): the client
/// went away, or the process's fd 1 refused it; with what the host sets
/// `EG(exit_status)` to first, php's CLI 255 (`sapi_cli_ub_write`), and
/// nothing for a host that sets none, as the built-in server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aborted {
    pub exit_status: Option<i32>,
}

/// Where a request's bytes go (design §4.3 seam 4): the engine owns the
/// header list, the status and the commit moment; a sink only writes.
/// The request owns its sink while it runs: [`Engine::serve`] takes it
/// and hands it back when the request ends, and each call reaches it as
/// the request makes it, the body as it leaves the `php_output` layer,
/// the head at the commit, the log and the error stream as written,
/// php's `sapi_module` timing, which a child process writing to the
/// host's stdout is ordered against (#741, #946). A host that wants its
/// own type back has it from the [`Any`] the sink is.
pub trait OutputSink: Any {
    /// Body bytes, and whether the host could write them: [`Aborted`]
    /// where the client went away or the fd refused them, which the
    /// request meets as `php_handle_aborted_connection`.
    ///
    /// # Errors
    ///
    /// [`Aborted`], a write the host could not make.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted>;
    /// The host may push what it has.
    fn flush(&mut self);
    /// The response's head, once, before the first body byte.
    fn send_headers(&mut self, head: &ResponseHead);
    /// One entry for the host's log, whole: what `php_log_err` was
    /// given where `error_log` names no file it could open
    /// (`sapi_module.log_message`), a diagnostic in its log form with
    /// its stack trace under it where it has one, or `error_log()`'s
    /// message as given, so it may carry newlines inside it and ends in
    /// none. The host ends it: php's CLI writes `"%s\n"` to stderr.
    fn log(&mut self, entry: &[u8]);
    /// Bytes for the host's error stream, as given: what php writes to
    /// the process's `stderr` past the host, the `display_errors=stderr`
    /// form of a diagnostic with its `\n` (`Z/main/main.c:1484`) and,
    /// when streams exist, what the `STDERR` stream constant is given.
    /// Where the host's log goes is the host's: php's CLI puts it on this
    /// same stream, and an `error_log` file parts the two, so a
    /// diagnostic is logged and displayed in that order on one stream
    /// and in no order across two (#216).
    fn write_stderr(&mut self, bytes: &[u8]);
    /// Whether the host's `flush` sends the head first, as php-fpm's
    /// `sapi_cgibin_flush` calls `sapi_send_headers`. No, by default.
    fn flush_sends_head(&self) -> bool {
        false
    }
    /// `fastcgi_finish_request()`'s `fcgi_end` and `fcgi_close`, once the
    /// buffers are flushed and the head sent: the response ends here and
    /// the program runs on unheard; whether the connection is closed
    /// after it (`fcgi_is_closed`), which makes a later call `false`.
    /// Nothing, and `false`, by default.
    fn finish(&mut self) -> bool {
        false
    }
    /// What the host says of itself, `fpm_get_status()`'s array; `None`,
    /// the default, is its `false`.
    fn status(&mut self) -> Option<HostValue> {
        None
    }
    /// `php://stdout` written to the host's own fd 1, where that is not
    /// the response: php-fpm's, the pipe its master logs, which
    /// `php_stream_url_wrap_php` reaches through `dup(STDOUT_FILENO)`.
    /// `false`, the default, has the bytes go to the body as the CLI's
    /// and a CGI's fd 1 is.
    fn write_stdout(&mut self, bytes: &[u8]) -> bool {
        let _ = bytes;
        false
    }
}

/// What the built-in server reads off a request it dispatched
/// ([`Engine::dispatch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    /// What the router answered; `Taken` where there is none.
    pub routed: Routed,
    /// What `php_cli_server_dispatch_script` did with the program, where
    /// the request named one and no router took it.
    pub program: Option<ProgramServed>,
}

/// What `php_cli_server_dispatch_script` did with a program
/// (`Z/sapi/cli/php_cli_server.c:2125`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramServed {
    /// It ran.
    Ran {
        /// `SG(sapi_headers).http_response_code` just after it.
        code: i64,
        /// `PG(last_error_message)`, `PG(last_error_file)` and
        /// `PG(last_error_lineno)` then, where `PG(last_error_type)` is
        /// one of `E_FATAL_ERRORS`.
        fatal: Option<(String, Vec<u8>, u32)>,
    },
    /// Its path holds a NUL, which php refuses
    /// (`can't handle paths that contain nul bytes`): it was never served,
    /// and the 400 php logs is of a page it built and never sent, the
    /// request ending with the head it had.
    NulInPath,
}

/// What a request [`Engine::dispatch`] serves names after its router.
enum Named {
    /// No program: a static file's request.
    Nothing,
    /// A program to serve.
    Program,
    /// A program whose path holds a NUL.
    NulInPath,
}

/// What the built-in server's router answered, `decline` in
/// `php_cli_server_dispatch_router`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    /// The router served the request.
    Taken,
    /// The router returned `false`: the request is the file's to serve.
    Declined,
}

/// What a host serves in a request, which it looks at once the request
/// has started ([`Engine::serve`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostServes {
    /// The program the request description names.
    Program,
    /// A page the host answers with in place of a program: php-fpm's ping
    /// and status pages, and its `File not found.` and its kin.
    Page(HostPage),
    /// Nothing: php-fpm's request with no `REQUEST_METHOD`, not a request
    /// of the web.
    Nothing,
}

/// A page a host answers a started request with: its response code, the
/// header lines it sets, each replacing the lines of its name, and its
/// body, written through the `php_output` layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPage {
    pub code: i64,
    pub headers: Vec<Vec<u8>>,
    pub body: Vec<u8>,
}

/// What `serve` reports of one request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observables {
    pub exit_code: i32,
    pub headers_sent: bool,
    /// The fatal's rendered text, when the request ended in one.
    pub fatal: Option<String>,
    /// `zend_memory_peak_usage(1)`: the most memory the request held.
    pub memory_peak: usize,
    /// What the program left uncaught, where the host renders it
    /// ([`Primary::Bare`]).
    pub uncaught: Option<Uncaught>,
    /// `php_request_startup` returned `FAILURE`: an extension's `rinit`
    /// raised, which is php's fatal inside `RINIT` (#1775). What it
    /// raised was rendered as the uncaught fatal with no frame standing,
    /// no later `rinit` and no program ran, and the host ends as its php
    /// counterpart ends after the failure: the CLI prints `Could not
    /// startup.`, the CGI returns `FAILURE`, php-fpm finishes the
    /// `FastCGI` request and ends the worker, and phpdbg prints `Could not
    /// startup`.
    pub start_failed: bool,
}

/// What `php_request_startup` came to ([`Engine::begin`]).
enum Begun {
    /// The request started, and what the host asked for is served in it.
    Started(Request),
    /// An extension's `rinit` raised, and its throwable was rendered as
    /// the uncaught fatal: the request, which the host's own branch ends.
    Failed(Request),
    /// No request started: the engine's start refused, `opcache.preload`
    /// refused, or `php_module_startup` failed, and what the host is
    /// handed instead, with its sink.
    Refused(Observables, Lent),
}

/// The sink a host lent the engine, owned while a request writes to it and
/// handed back when the request ends.
pub(crate) type Lent = Box<dyn OutputSink + Send>;

/// php's `opcache.max_accelerated_files` default: the units one engine
/// keeps before it drops half of them.
const MAX_ACCELERATED_FILES: usize = 10_000;

/// What every request shares: the bundle, the linked declarations, the
/// engine states, the ini defaults and the loop (design §5.14).
pub(crate) struct Shared {
    /// `module_registry`: `Core`, then the bundle in startup order, each
    /// at its [`ExtensionId`], its declarations moved into the tables
    /// below.
    pub(crate) extensions: Vec<ExtensionEntry>,
    /// `CG(function_table)`'s natives, the engine's own and then each
    /// extension's in bundle order, less what the startup ini's
    /// `disable_functions` took out of it ([`function_table`]): the
    /// declarations alone, which the compiler reads as they stand on
    /// every compile ([`Declared`]).
    pub(crate) functions: Vec<FunctionDecl>,
    /// `functions` by name, any case, `CG(function_table)`'s keys.
    function_names: Names<Folded>,
    /// Each native's entry, one row per row of `functions`, the
    /// interception's where an extension intercepts it.
    pub(crate) entries: Vec<Entry>,
    /// The entries the extensions' interceptions replaced, one row per
    /// row of `functions`, the nearest first and the native's own last,
    /// empty where nothing intercepts the native
    /// ([`crate::api::abi::ExtensionBuilder::intercept`]).
    pub(crate) replaced: Vec<Vec<Entry>>,
    /// Each native's `internal_function.module`, one row per row of
    /// `functions`: the extension that declared it, `Core` for Zend's
    /// own, and none for the natives of php's `standard` the engine
    /// declares itself.
    pub(crate) function_modules: Vec<Option<ExtensionId>>,
    /// Each native's `ZEND_ACC_DEPRECATED` and `ZEND_ACC_NODISCARD`, one
    /// row per row of `functions`, decoded from its declaration's
    /// attributes once, as `zend_register_functions` copies the stub's
    /// flags onto the `zend_internal_function`.
    pub(crate) function_flags: Vec<crate::attributes::Flags>,
    /// `zend_flf_functions` (`Z/Zend/zend_API.c:3115-3133`): the natives
    /// with a frameless entry, one row per arity in registration order,
    /// which a `FRAMELESS_ICALL_*`'s `extended_value` indexes. Computed
    /// from `functions` by the one function the compiler numbers by.
    pub(crate) frameless: Vec<FramelessEntry>,
    /// The classes Zend declares and then each extension's, in bundle
    /// order, each with its methods on it: what a `ClassId` below the
    /// request's own declarations indexes.
    pub(crate) classes: Vec<ClassDecl>,
    /// `CG(class_table)`'s keys over `classes` as startup leaves them
    /// ([`Shared::class_at`]), whose first half the compiler finds a
    /// native class by.
    class_keys: ClassKeys,
    /// Each native class's `ce->info.internal.module`, one row per row
    /// of `classes`: the extension that declared it, `Core` for Zend's.
    pub(crate) class_modules: Vec<ExtensionId>,
    /// Each native class's handler table (design §5.4), one row per row
    /// of `classes`: the table the class declared, or none where it
    /// takes its parent's, `Std`'s at a root.
    pub(crate) handlers: Vec<Option<&'static dyn ObjectHandlers>>,
    /// Each native class's `create_object` where its objects carry a
    /// native payload (design §5.4), one row per row of `classes`: the
    /// `NativeNew` the class declared, or none where it takes its
    /// parent's and `zend_objects_new` at a root.
    pub(crate) natives: Vec<Option<NativeNew>>,
    /// Each native method's entry, in the order the classes declare
    /// them and then the trampolines': a `Function` past `functions`
    /// indexes here.
    pub(crate) methods: Vec<NativeMethod>,
    /// The declaration of each trampoline, one row per
    /// [`MethodAt::Trampoline`]: a method a class's `get_method`
    /// synthesises, which `ce->function_table` has no row for, so no
    /// lookup by name and no rendering finds it.
    pub(crate) trampolines: Vec<MethodDecl>,
    /// The class each parameter of each native holds its argument to, by
    /// the native's number ([`parameter_classes`]).
    pub(crate) parameter_classes: Vec<Box<[Option<ClassId>]>>,
    /// Each class's linked record, one row per row of `classes`
    /// ([`native_records`]): `ce->function_table` whole, in php's order.
    pub(crate) records: Vec<LinkedClass<ClassId>>,
    /// The function each class's own methods are, one row per row of
    /// `classes` ([`method_functions`]), by their index among its
    /// declaration.
    pub(crate) method_functions: Vec<Box<[Option<Function>]>>,
    /// Each class's trampolines, one row per row of `classes`
    /// ([`trampoline_tables`]): what its `get_method` synthesises under
    /// a name its function table has no row for, each under its name,
    /// found in any case.
    pub(crate) trampoline_tables: Vec<FunctionTable>,
    /// Each class's `ce->parent`, one row per row of `classes`, resolved
    /// once as `zend_register_internal_class_ex` links it: a native
    /// class's parent is a native class.
    pub(crate) parents: Vec<Option<ClassId>>,
    /// Each class's placed properties, one row per row of `classes`,
    /// built at startup ([`native_placed`]) and read as plain data
    /// ([`Request::placed_properties`]): what the class and its native
    /// parents declare, which never changes.
    pub(crate) placed: Vec<crate::api::request::Placed>,
    constants: Vec<ConstDecl>,
    /// `constants` by name, `EG(zend_constants)`'s keys.
    constant_names: Names<Exact>,
    /// Each constant's `ZEND_CONSTANT_MODULE_NUMBER`, one row per row of
    /// `constants`: the extension that declared it, `Core` for Zend's.
    pub(crate) constant_modules: Vec<ExtensionId>,
    pub(crate) engine_states: States<Box<dyn Any + Send + Sync>>,
    /// Where each extension's request and engine state stands, by its
    /// type ([`crate::api::request::StateSlots`]).
    pub(crate) state_slots: crate::api::request::StateSlots,
    ini: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// The typed entries `ini`'s master values give, which each request
    /// starts with ([`TypedIni`]).
    pub(crate) typed_ini: TypedIni,
    /// The constants' values, one row per row of `constants`, the string
    /// ones interned into the engine's table as `REGISTER_STRING_CONSTANT`
    /// interns php's at `MINIT`, so a request reading one allocates
    /// nothing and counts nothing; beside each, its `CONST_DEPRECATED`,
    /// decoded once from the declaration's attributes as `gen_stub.php`
    /// writes it. None for a declaration whose value is no scalar.
    constant_values: Vec<Option<(Zval, crate::attributes::Flags)>>,
    /// `PHP_SAPI`'s value under each host's name, interned at build with
    /// the other constants, so a request that reads the name its host
    /// gave it allocates nothing ([`crate::constants::HOSTS`]).
    sapi_names: Vec<(Box<[u8]>, Zval)>,
    /// The build's facts ([`EngineBuilder::configure`]).
    pub(crate) build_defs: BuildDefs,
    /// `CG(skip_shebang)` for every file a request compiles, which the
    /// host asked for ([`EngineBuilder::skip_shebang`]).
    pub(crate) shebang: php_compile::Shebang,
    /// The hooks the bound tier replaces ([`Tier::HOOKED`]), every unit
    /// compiled under them (design §4.3, revision 58).
    hooked: php_compile::Hooked,
    /// Whether every unit is compiled with extended info, which the
    /// host's `-e` asked for ([`EngineBuilder::extended_info`]).
    extended_info: bool,
    /// What the bundle supplies beyond its declarations, each kind
    /// resolved by php's rule ([`crate::supplied`]); the shell only
    /// where the host carries php's table of shell callbacks
    /// ([`EngineBuilder::cli_shell`]); on an engine whose start refused,
    /// only what the extensions started before the refusal installed
    /// ([`Suppliers::refused_at`]).
    pub(crate) supplied: Resolved,
    /// What reading the host's ini entries wrote to `stderr` in php
    /// ([`Configuration`]), and after it what the extensions' `on_modify`
    /// wrote there over the host's values, as opcache's handlers log a
    /// refused setting: for the host to write before it serves.
    startup_errors: Vec<u8>,
    /// What the extensions' `on_modify` raised as the engine registered
    /// their ini entries ([`Engine::register_ini`]), which php shows once
    /// `php_module_startup` is done and before any request's `RINIT`,
    /// once: the first request raises it first, at no file and no line.
    startup_raised: Vec<(Level, Vec<u8>)>,
    /// Whether a request has raised [`Shared::startup_raised`].
    startup_shown: AtomicBool,
    /// Where an extension's engine start refused: the [`Refusal`] with
    /// `Unable to start %s module` raised after what the extension
    /// raised ([`Engine::refused`]).
    refused: Option<Refusal>,
    /// `sapi_module.name`, the host's name the engine was built under
    /// ([`EngineBuilder::sapi_name`]): what the requests the engine makes
    /// for itself, its startup, its refusal and its end, are made under.
    pub(crate) host: &'static str,
    /// `configuration_hash`: what the host's ini text set, before any
    /// entry read it, which `get_cfg_var()` answers from
    /// ([`Request::cfg_entry`]).
    configuration: Vec<(Vec<u8>, Configured)>,
    /// Whether it holds a `[PATH=]` table and a `[HOST=]` table.
    sections: [bool; 2],
    /// The php.ini files read into it ([`EngineBuilder::ini_files`]).
    pub(crate) ini_files: crate::api::request::IniFiles,
    /// Where every request of this engine appends the natives it
    /// entered, one line each, where a host asked for it
    /// ([`EngineBuilder::trace_natives_to`]).
    trace_to: Option<std::path::PathBuf>,
    /// Whether a debug build's request leak sensor is armed, and whether
    /// it traces allocations ([`EngineBuilder::sense_leaks`]).
    #[cfg(debug_assertions)]
    pub(crate) sense_leaks: LeakSensor,
    /// The loop's entries, bound to the engine's tier and, once its
    /// declared opcodes are fixed as it starts, to whether it tells
    /// their observers ([`Shared::observe_opcodes`]).
    pub(crate) loops: Loops,
    /// The observer word (design §4.3 seam 9): how many observers the
    /// bundle declared ([`crate::supplied::Resolved::observers`]), fixed
    /// at the engine's build and read at every call and return home
    /// ([`crate::observer`]); zero while none is declared.
    pub(crate) observers: u32,
    /// The opcodes the bundle observes (design §7.1, #1780), declared as
    /// the engine starts and fixed after it, which choose the loop the
    /// engine binds ([`Shared::observe_opcodes`]); empty while none is
    /// declared.
    pub(crate) opcodes: crate::observer::ObservedOpcodes,
    /// The unit cache (design §6.3): what a file compiled to, held for
    /// every request that asks for the same file unchanged. A source the
    /// host handed over is not in it, because php caches neither `-r`
    /// nor `eval`. It stands above the table its units' constants are
    /// interned into, so a dropped engine drops its units first.
    /// The entries a host's `php_admin_value` made `PHP_INI_SYSTEM`,
    /// which no request moves ([`IniOrigin::Admin`]).
    pub(crate) locked: Vec<Vec<u8>>,
    /// `CG(script_encoding_list)` as the engine started: what the
    /// startup `zend.script_encoding` named, through the encoding
    /// functions ([`crate::multibyte`]).
    pub(crate) script_encodings: Vec<php_compile::multibyte::Encoding>,
    pub(crate) units: UnitCache,
    /// The engine's published arena (design §5.14, *Identities*): the
    /// rows of the classes and functions published past the engine's
    /// own, which every request reads with nothing taken and which no row is
    /// written in after it is published, laid over the preload's once
    /// that has published. Each request holds the arena of the epoch it
    /// started in, and an opcache restart hands the requests after it a
    /// new one, the old dropped with its last holder
    /// ([`crate::arena::Epochs`]).
    pub(crate) arena: crate::arena::Epochs,
    /// The stream layer's engine state, which the engine builds at its
    /// start from [`StreamHooks::state`](crate::layer::StreamHooks::state)
    /// and cannot name: the layer's own types, its persistent pool among
    /// them, php's `EG(persistent_list)` (design §4.10, #1172, #1962). The
    /// layer reads it as [`Face::layer_state`](crate::layer::Face::layer_state);
    /// `None` with no stream layer supplied.
    pub(crate) layer: Option<Arc<dyn Any + Send + Sync>>,
    /// The engine's execution timer (design §5.14, **Threads**): the one
    /// thread that raises every request's interrupt at its deadlines,
    /// started by the first deadline a request sets, parked between
    /// requests, and joined before a fork and at the engine's end.
    pub(crate) timer: crate::interrupt::Timer,
    /// The engine's interned strings (design §5.2, §5.14): what a unit's
    /// constant strings enter as it is published, grown here alone and
    /// read by every request thread with nothing taken.
    strings: Mutex<Strings>,
    /// php's `interned_strings_permanent` as `zend_interned_strings_switch_storage`
    /// leaves it after `MINIT`: `zend_known_strings`, and every name and
    /// string the engine's and the bundle's registrations intern
    /// ([`permanent_names`]), interned at startup and frozen, so a request
    /// asking for one finds it here and allocates nothing
    /// ([`Shared::permanent`]).
    names: Strings,
    /// The string a native's parameter defaults to where its stub writes
    /// a string, made once at startup and immutable after: an omitted
    /// argument is php's C default, which costs the request nothing, and
    /// so does this ([`Shared::default_string`]). Kept apart from
    /// [`Shared::permanent`], since php interns none of them.
    default_strings: Strings,
    /// How the engine's startup finished, which the first request's
    /// host finishes ([`Engine::started`]): serving, with what the unit
    /// cache preloaded, or refusing to serve anything.
    pub(crate) started: OnceLock<crate::preload::Started>,
    /// Held by the one thread that finishes the startup, while
    /// [`Shared::started`] is unset ([`Engine::started`]).
    pub(crate) starting: Mutex<()>,
    /// `CG(map_ptr_last)`: how many map pointers the engine has handed
    /// out (design §5.14), which is the next one's offset. Every op
    /// array of a unit takes one as the unit is published
    /// (`ZEND_MAP_PTR_NEW` at `zend_persist_op_array`,
    /// `Z/ext/opcache/zend_persist.c:715`), and a request's map table
    /// grows to what its units index.
    pub(crate) map_ptr_last: AtomicU32,
}

/// `ZEND_MAP_PTR_NEW` over every op array of `unit`, at its publication:
/// each takes the next offset the engine has, so a request serving the
/// unit finds each op array's runtime cache at its own entry
/// ([`exec::init_runtime_cache`]). A unit compiled for one request
/// alone takes none ([`exec::RuntimeCaches::take`]).
pub(crate) fn assign_map_ptrs(unit: &mut Unit, map_ptr_last: &AtomicU32) {
    for code in exec::op_arrays_mut(unit) {
        code.runtime_cache =
            php_unit::MapPtrOffset::Engine(map_ptr_last.fetch_add(1, Ordering::Relaxed));
    }
}

/// `ZEND_MAP_PTR_NEW` for a function the engine publishes beside an op
/// array it does not own, a trait's method copied into a published
/// class (`function_add_ref`): the next of the engine's map pointers,
/// which each request lays its own cache down at.
pub(crate) fn new_map_ptr(map_ptr_last: &AtomicU32) -> exec::CacheAt {
    exec::CacheAt::of(php_unit::MapPtrOffset::Engine(
        map_ptr_last.fetch_add(1, Ordering::Relaxed),
    ))
}

/// One native method's entry, `zend_internal_function` on a class:
/// where its declaration stands, the class with it where one holds it,
/// and the function served for it.
#[derive(Clone, Copy)]
pub(crate) struct NativeMethod {
    pub(crate) at: MethodAt,
    pub(crate) entry: Entry,
    /// Its `ZEND_ACC_DEPRECATED` and `ZEND_ACC_NODISCARD`, decoded from
    /// the declaration's attributes where the method is linked.
    pub(crate) flags: crate::attributes::Flags,
}

/// Where a native method's declaration stands: among its class's own,
/// php's `ce->function_table`, which is what a lookup by name walks and
/// a rendering lists; or among the engine's trampolines, a method the
/// class's `get_method` answers that no table carries
/// (`ZEND_ACC_CALL_VIA_HANDLER`). `zend_get_closure_invoke_method`
/// makes one for `Closure::__invoke`, which the stub declares nowhere.
#[derive(Clone, Copy)]
pub(crate) enum MethodAt {
    /// The index among `class`'s `ce->function_table` own declarations.
    Declared { class: ClassId, at: u32 },
    /// A trampoline `class`'s `get_method` answers, the index among
    /// [`Shared::trampolines`].
    Trampoline { class: ClassId, at: u32 },
    /// `zend_get_property_hook_trampoline`'s function of `kind`, what
    /// `parent::$name::get()` calls where the parent's property has no
    /// hook of that kind: no class holds it, its scope is the property's
    /// class, carried on the call, and its declaration is the index
    /// among [`Shared::trampolines`].
    ParentHook { kind: HookKind, at: u32 },
    /// A native function no function table carries, which php declares
    /// as a `zend_internal_function` of its own and serves only by
    /// pointer, `gc_destructor_fiber`: no lookup by name finds it, and a
    /// backtrace names it; its declaration is the index among
    /// [`Shared::trampolines`].
    Unlisted { at: u32 },
}

impl MethodAt {
    /// The class that holds the method, `func->common.scope`; none for a
    /// parent hook's trampoline, whose scope its call carries.
    pub(crate) fn class(self) -> Option<ClassId> {
        match self {
            MethodAt::Declared { class, .. } | MethodAt::Trampoline { class, .. } => Some(class),
            MethodAt::ParentHook { .. } | MethodAt::Unlisted { .. } => None,
        }
    }
}

/// `zend_register_internal_enum`'s `zend_register_functions` over the
/// enum methods: a native enum takes `cases`, and `from` and `tryFrom`
/// for a backed one, after the methods its stub declares, served by the
/// entries every enum's are ([`crate::enums::EnumMethod`]).
fn link_enum_methods(decl: &mut ClassDecl, class: ClassId, methods: &mut Vec<NativeMethod>) {
    if decl.kind != php_unit::decl::ClassKind::Enum {
        return;
    }
    for method in php_unit::enum_methods(decl.backing) {
        let lowered = method.function.name.0.to_ascii_lowercase();
        let Some(served) = crate::enums::EnumMethod::of(&lowered, decl.backing) else {
            continue;
        };
        let index = u32::try_from(decl.methods.len()).unwrap_or(u32::MAX);
        decl.methods.push(method);
        methods.push(NativeMethod {
            at: MethodAt::Declared { class, at: index },
            entry: served.entry(),
            flags: crate::attributes::Flags::default(),
        });
    }
}

/// The classes the engine links: Zend's own and then each extension's,
/// in bundle order, each with its methods, handler table and
/// `create_object`, the constants later extensions add to them, and the
/// trampolines.
struct LinkedClasses {
    classes: Vec<ClassDecl>,
    modules: Vec<ExtensionId>,
    handlers: Vec<Option<&'static dyn ObjectHandlers>>,
    natives: Vec<Option<NativeNew>>,
    methods: Vec<NativeMethod>,
    trampolines: Vec<MethodDecl>,
}

/// [`LinkedClasses`] for each extension's classes in `declarations`,
/// Core's first, each declaration moved into the table.
fn link_classes(declarations: &mut [Declarations]) -> LinkedClasses {
    // `zend_register_internal_class_with_flags` and `zend_register_functions`
    // over the class's methods: each declaration takes its methods,
    // and each method's entry is numbered in the order it is met.
    let mut classes: Vec<ClassDecl> = Vec::new();
    let mut modules = Vec::new();
    let mut handlers = Vec::new();
    let mut natives = Vec::new();
    let mut methods = Vec::new();
    for (at, declared) in declarations.iter_mut().enumerate() {
        for (mut decl, entries, table, native) in std::mem::take(&mut declared.classes) {
            // A debug build serves an extension's table as the extension
            // (#2113); Core's runs as whatever called it, as it always
            // has, and keeps the identity its own tests compare.
            #[cfg(debug_assertions)]
            let table = match at {
                0 => table,
                _ => table.map(|table| crate::served::served(table, ExtensionId::at(at))),
            };
            handlers.push(table);
            natives.push(native);
            modules.push(ExtensionId::at(at));
            let class = ClassId(u32::try_from(classes.len()).unwrap_or(u32::MAX));
            for (method, entry) in entries {
                // A method the declaration carries already is the one the
                // entry serves, so the table holds the name once, as
                // `zend_register_functions` registers each name once.
                let flags = crate::attributes::Flags::of(&method.function.attributes);
                let held = (decl.methods.iter_mut()).enumerate().find(|(_, held)| {
                    held.function
                        .name
                        .0
                        .eq_ignore_ascii_case(&method.function.name.0)
                });
                let index = if let Some((at, held)) = held {
                    *held = method;
                    at
                } else {
                    decl.methods.push(method);
                    decl.methods.len().saturating_sub(1)
                };
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                methods.push(NativeMethod {
                    at: MethodAt::Declared { class, at: index },
                    entry,
                    flags,
                });
            }
            link_enum_methods(&mut decl, class, &mut methods);
            classes.push(decl);
        }
    }
    let bundle = declarations.get_mut(1..).unwrap_or_default();
    link_class_constants(&mut classes, bundle);
    let trampolines = link_trampolines(&classes, bundle, &mut methods);
    LinkedClasses {
        classes,
        modules,
        handlers,
        natives,
        methods,
        trampolines,
    }
}

/// `CG(class_table)`'s keys at the end of startup, any case: each class
/// under its name, as `zend_register_internal_class_ex` adds it, then
/// each stub `@alias` under its own, as `zend_register_class_alias_ex`
/// adds it after its class. `zend_hash_add` refuses a key the table
/// holds, so the first row of a name is the one found.
struct ClassKeys {
    /// The classes by name, an index over the rows that carry them.
    names: Names<Folded>,
    /// The aliases and the class each names.
    aliases: NameMap<Folded, ClassId>,
}

impl ClassKeys {
    fn over(classes: &[ClassDecl]) -> ClassKeys {
        let mut aliases = NameMap::new();
        for (class, decl) in (0u32..).map(ClassId).zip(classes) {
            for alias in &decl.aliases {
                aliases.add(&alias.0, class);
            }
        }
        ClassKeys {
            names: Names::over(classes),
            aliases,
        }
    }

    /// `zend_hash_find(CG(class_table), lcname)` over `classes`, the rows
    /// the keys were made over.
    fn find(&self, classes: &[ClassDecl], name: &[u8]) -> Option<ClassId> {
        match self.names.find(classes, name) {
            Some(at) => u32::try_from(at).ok().map(ClassId),
            None => self.aliases.find(name).copied(),
        }
    }
}

/// A class's trampolines by name, any case, which its `get_method`
/// answers past its function table ([`trampoline_tables`]): one lookup.
pub(crate) type FunctionTable = NameMap<Folded, Function>;

/// The function each native class's own methods are, by their index
/// among its declaration: every row of `methods` a class declares,
/// numbered past the `functions` natives, which a row of a record
/// naming the class reads.
fn method_functions(
    classes: &[ClassDecl],
    methods: &[NativeMethod],
    functions: usize,
) -> Vec<Box<[Option<Function>]>> {
    let mut tables: Vec<Vec<Option<Function>>> = classes
        .iter()
        .map(|decl| vec![None; decl.methods.len()])
        .collect();
    for (row, method) in (functions..).zip(methods) {
        let MethodAt::Declared { class, at } = method.at else {
            continue;
        };
        let slot = usize::try_from(class.0)
            .ok()
            .and_then(|class| tables.get_mut(class))
            .and_then(|table| table.get_mut(usize::try_from(at).ok()?));
        if let (Some(slot), Ok(row)) = (slot, u32::try_from(row)) {
            *slot = Some(Function(row));
        }
    }
    tables.into_iter().map(Vec::into_boxed_slice).collect()
}

/// Each native class's linked record, as `zend_register_internal_class_ex`
/// links a class it registers to its parent and
/// `zend_class_implements` to its interfaces: each built once every
/// class it names is, a class standing before what it names waiting a
/// round. One whose names never resolve has its own methods alone.
fn native_records(classes: &[ClassDecl], keys: &ClassKeys) -> Vec<LinkedClass<ClassId>> {
    let mut records: Vec<Option<LinkedClass<ClassId>>> = vec![None; classes.len()];
    let mut progress = true;
    while progress {
        progress = false;
        for (at, decl) in classes.iter().enumerate() {
            if records.get(at).is_none_or(Option::is_some) {
                continue;
            }
            let built = {
                let lookup = |name: &[u8]| {
                    let class = keys.find(classes, name)?;
                    let at = usize::try_from(class.0).ok()?;
                    Some(php_link::Held {
                        decl: classes.get(at)?,
                        bodies: None,
                        linked: true,
                        record: records.get(at)?.as_ref().map(|record| (class, record)),
                    })
                };
                php_link::linked_class(decl, 0, &lookup, &|class| {
                    classes.get(usize::try_from(class.0).ok()?)
                })
            };
            if let (Some(record), Some(slot)) = (built, records.get_mut(at)) {
                *slot = Some(record);
                progress = true;
            }
        }
    }
    records
        .into_iter()
        .zip(classes)
        .map(|(record, decl)| {
            record
                .or_else(|| php_link::linked_class(decl, 0, &|_| None, &|_| None))
                .unwrap_or_default()
        })
        .collect()
}

/// Each native class's placed properties ([`placed_of`]), built as
/// `zend_register_internal_class_ex` builds them when it links the
/// class: each after its parent's, a class standing before its
/// parent waiting a round, and one whose parent never resolves placed
/// over none.
fn native_placed(
    classes: &[ClassDecl],
    records: &[LinkedClass<ClassId>],
    parents: &[Option<ClassId>],
) -> Vec<Placed> {
    let mut placed: Vec<Option<Placed>> = vec![None; classes.len()];
    let decl_of = |info: PropertyInfo| {
        classes
            .get(usize::try_from(info.declaring.0).ok()?)?
            .properties
            .get(usize::try_from(info.at).ok()?)
    };
    let place = |at: usize, parent: &[PropertyInfo]| {
        let class = ClassId(u32::try_from(at).unwrap_or(u32::MAX));
        let decl = classes.get(at)?;
        Some(placed_of(class, decl, records.get(at), parent, decl_of))
    };
    let mut progress = true;
    while progress {
        progress = false;
        for at in 0..classes.len() {
            if placed.get(at).is_none_or(Option::is_some) {
                continue;
            }
            let parent = match parents.get(at).copied().flatten() {
                None => Some(Placed::default()),
                Some(parent) => usize::try_from(parent.0)
                    .ok()
                    .and_then(|parent| placed.get(parent)?.clone()),
            };
            let built = parent.and_then(|parent| place(at, &parent));
            if let (Some(built), Some(slot)) = (built, placed.get_mut(at)) {
                *slot = Some(built);
                progress = true;
            }
        }
    }
    placed
        .into_iter()
        .enumerate()
        .map(|(at, built)| built.or_else(|| place(at, &[])).unwrap_or_default())
        .collect()
}

/// Each native class's trampolines by name: every row of `methods` a
/// class's `get_method` answers ([`MethodAt::Trampoline`]), under the
/// name of its row of `trampolines`, the first of a name kept.
fn trampoline_tables(
    classes: usize,
    methods: &[NativeMethod],
    trampolines: &[MethodDecl],
    functions: usize,
) -> Vec<FunctionTable> {
    tables_of(classes, methods, functions, |at| match at {
        MethodAt::Trampoline { class, at } => {
            Some((class, trampolines.get(usize::try_from(at).ok()?)?))
        }
        _ => None,
    })
}

/// One table per class, `classes` of them: each row of `methods` that
/// `place` gives a class and a declaration, filed among that class's
/// rows under the declaration's name and numbered past the
/// `functions` natives; the first of a name stays.
fn tables_of<'a>(
    classes: usize,
    methods: &[NativeMethod],
    functions: usize,
    place: impl Fn(MethodAt) -> Option<(ClassId, &'a MethodDecl)>,
) -> Vec<FunctionTable> {
    let mut tables = vec![FunctionTable::new(); classes];
    for (row, method) in (functions..).zip(methods) {
        let Some((class, decl)) = place(method.at) else {
            continue;
        };
        let (Some(table), Ok(row)) = (
            usize::try_from(class.0)
                .ok()
                .and_then(|class| tables.get_mut(class)),
            u32::try_from(row),
        ) else {
            continue;
        };
        table.add(&decl.function.name.0, Function(row));
    }
    tables
}

/// `zend_string_tolower`: the key a name is found under.
/// `zend_parse_arg_object`'s class for each parameter of each native, by
/// the native's number among the functions and then the methods: the
/// class a parameter's type names where it names one, `?Class` among
/// them, resolved once as the engine starts, as ZPP's `O` names a
/// `zend_class_entry *`, so holding an argument to it compares
/// identities ([`crate::api::coerce::argument`]); none for every other
/// parameter.
fn parameter_classes(
    functions: &[FunctionDecl],
    methods: &[NativeMethod],
    classes: &[ClassDecl],
    trampolines: &[MethodDecl],
    class_at: impl Fn(&[u8]) -> Option<ClassId>,
) -> Vec<Box<[Option<ClassId>]>> {
    let of = |decl: Option<&FunctionDecl>| -> Box<[Option<ClassId>]> {
        decl.map_or_else(Box::default, |decl| {
            decl.parameters
                .iter()
                .map(|parameter| {
                    let named = match parameter.ty.as_ref()? {
                        Type::Named(name) => name,
                        Type::Nullable(inner) => match &**inner {
                            Type::Named(name) => name,
                            _ => return None,
                        },
                        _ => return None,
                    };
                    class_at(&named.0)
                })
                .collect()
        })
    };
    let method_decl = |method: &NativeMethod| match method.at {
        MethodAt::Declared { class, at } => classes
            .get(class.0 as usize)?
            .methods
            .get(usize::try_from(at).ok()?),
        MethodAt::Trampoline { at, .. }
        | MethodAt::ParentHook { at, .. }
        | MethodAt::Unlisted { at } => trampolines.get(usize::try_from(at).ok()?),
    };
    functions
        .iter()
        .map(|decl| of(Some(decl)))
        .chain(
            methods
                .iter()
                .map(|method| of(method_decl(method).map(|decl| &decl.function))),
        )
        .collect()
}

fn lowered(name: &[u8]) -> Box<[u8]> {
    name.to_ascii_lowercase().into_boxed_slice()
}

/// `zend_declare_class_constant_ex` over a class another extension
/// registered, from a later `MINIT`: each extension's
/// [`ExtensionBuilder::class_constants`] appended to the class it names,
/// in bundle order.
///
/// [`ExtensionBuilder::class_constants`]: crate::api::abi::ExtensionBuilder::class_constants
fn link_class_constants(classes: &mut [ClassDecl], declarations: &mut [Declarations]) {
    let added = (declarations.iter_mut()).flat_map(|d| std::mem::take(&mut d.class_constants));
    for (class_name, constants) in added {
        if let Some(decl) = classes
            .iter_mut()
            .find(|decl| decl.name.0.eq_ignore_ascii_case(&class_name.0))
        {
            decl.constants.extend(constants);
        }
    }
}

/// `zend_register_closure_ce` leaves `__invoke` off `Closure`'s table
/// and `zend_closure_get_method` answers it on the lookup: every
/// trampoline [`crate::classes::trampolines`] names is numbered past
/// the declared methods, so a `Function` still names one row of
/// `methods`, and its declaration stands beside the linked table
/// rather than in it. A row naming a class nothing declares is left
/// unlinked, as a method of an absent class would be.
fn link_trampolines(
    classes: &[ClassDecl],
    declarations: &mut [Declarations],
    methods: &mut Vec<NativeMethod>,
) -> Vec<MethodDecl> {
    let mut trampolines = Vec::new();
    let own = crate::classes::trampolines();
    let laid = (declarations.iter_mut()).flat_map(|d| std::mem::take(&mut d.trampolines));
    for (class_name, method, entry) in own.into_iter().chain(laid) {
        let Some(at) = classes
            .iter()
            .position(|decl| decl.name.0.eq_ignore_ascii_case(&class_name.0))
        else {
            continue;
        };
        let index = u32::try_from(trampolines.len()).unwrap_or(u32::MAX);
        methods.push(NativeMethod {
            at: MethodAt::Trampoline {
                class: ClassId(u32::try_from(at).unwrap_or(u32::MAX)),
                at: index,
            },
            entry,
            flags: crate::attributes::Flags::of(&method.function.attributes),
        });
        trampolines.push(method);
    }
    // `zend_get_property_hook_trampoline`'s two, which no class holds.
    for (kind, method, entry) in crate::api::request::parent_hook_trampolines() {
        let index = u32::try_from(trampolines.len()).unwrap_or(u32::MAX);
        methods.push(NativeMethod {
            at: MethodAt::ParentHook { kind, at: index },
            entry,
            flags: crate::attributes::Flags::of(&method.function.attributes),
        });
        trampolines.push(method);
    }
    for (method, entry) in crate::fiber::unlisted_functions() {
        let index = u32::try_from(trampolines.len()).unwrap_or(u32::MAX);
        methods.push(NativeMethod {
            at: MethodAt::Unlisted { at: index },
            entry,
            flags: crate::attributes::Flags::of(&method.function.attributes),
        });
        trampolines.push(method);
    }
    trampolines
}

/// Where a unit is compiled from, as the request describes it: the name
/// the unit and its diagnostics carry, `CG(compiled_filename)`, the
/// working directory `__DIR__` folds to where that name has no directory
/// (`VCWD_GETCWD`, the request's own under ZTS), which is the request's
/// for a request and the process's for a unit compiled outside one
/// ([`Engine::unit`]), and whether the source is a file or the string an
/// `eval` handed over ([`SourceKind`]), with the runtime definition keys
/// the request made before it where it is an `eval`'s
/// ([`php_compile::Options::keys_before`]), and the classes the request
/// had declared by then, which a unit compiled for no request has none of
/// ([`php_compile::RequestClasses`]); and, where `zend.multibyte` is on,
/// the encoding functions bound to the request and what the scanner
/// reads beside them ([`crate::multibyte::Bound`]).
#[derive(Clone, Copy)]
struct Origin<'a> {
    filename: &'a [u8],
    cwd: &'a Path,
    kind: SourceKind,
    keys_before: u32,
    request_classes: RequestClasses<'a>,
    multibyte: Option<Multibyte<'a>>,
}

/// What the engine has declared, as the compiler reads it: php's
/// `CG(function_table)`, `CG(class_table)` and the persistent constants,
/// each a view of the engine's own table.
#[derive(Clone, Copy, Default)]
pub(crate) struct Declared<'a> {
    pub(crate) constants: Indexed<'a, ConstDecl, Exact>,
    pub(crate) functions: Indexed<'a, FunctionDecl, Folded>,
    pub(crate) frameless: &'a [FramelessEntry],
    pub(crate) classes: Indexed<'a, ClassDecl, Folded>,
    pub(crate) hooked: php_compile::Hooked,
    pub(crate) extended_info: bool,
}

impl Shared {
    /// `zend_flf_functions` by name, in the frameless table's order
    /// ([`Engine::frameless`]).
    pub(crate) fn frameless_names(&self) -> Vec<Name> {
        self.frameless
            .iter()
            .map(|entry| {
                self.functions
                    .get(entry.function as usize)
                    .map(|decl| decl.name.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The opcodes the bundle observes, asked of each extension once its
    /// engine start is over, and the loop bound to them: the one that
    /// tells their observers where any is declared and the one with no
    /// test where none is, as php resolves an opline's handler when it
    /// compiles it (design §5.8, §7.1).
    fn observe_opcodes(&mut self, start: &Start, bind_loops: BindLoops) {
        self.opcodes = crate::observer::ObservedOpcodes::start(&self.supplied.opcodes, start);
        self.loops = bind_loops(&self.opcodes);
    }

    /// The tables the compiler reads, as [`Declared`].
    pub(crate) fn declared(&self) -> Declared<'_> {
        Declared {
            constants: Indexed::new(&self.constants, &self.constant_names),
            functions: Indexed::new(&self.functions, &self.function_names),
            frameless: &self.frameless,
            classes: Indexed::new(&self.classes, &self.class_keys.names),
            hooked: self.hooked,
            extended_info: self.extended_info,
        }
    }

    /// One source compiled with its constant strings interned into the
    /// engine's table, which is what publication is (design §5.2, §6.3):
    /// opcache's persist walks an op array's constants and re-interns
    /// each into the shared table (`Z/ext/opcache/zend_persist.c:525-540`
    /// over `Z/ext/opcache/ZendAccelerator.c:515-575`), and compiling
    /// into that table reaches the same published unit with one
    /// allocation instead of two.
    fn publish(
        &self,
        source: &[u8],
        origin: Origin<'_>,
        stack: StackLimit,
    ) -> Result<Arc<Unit>, CompileError> {
        self.compile_shared(source, origin, stack)
            .map(|unit| self.publish_compiled(unit))
    }

    /// [`Shared::publish`]'s compile alone: the constant strings in the
    /// engine's table and no map-pointer offset taken, for a caller that
    /// decides afterwards whether the unit is published
    /// ([`Shared::publish_compiled`]) or the request's own.
    fn compile_shared(
        &self,
        source: &[u8],
        origin: Origin<'_>,
        stack: StackLimit,
    ) -> Result<Unit, CompileError> {
        let mut strings = match self.strings.lock() {
            Ok(strings) => strings,
            // The table is append-only and every string in it was
            // finished before it was published, so what a thread that
            // ended holding the lock left is consistent.
            Err(poisoned) => poisoned.into_inner(),
        };
        compile(
            source,
            origin,
            self.compile_ini(),
            self.declared(),
            stack,
            &mut *strings,
        )
    }

    /// `unit` published for every request: each op array given its
    /// map-pointer offset ([`assign_map_ptrs`]), which only a unit held
    /// for more than one request takes, as only what opcache persists
    /// moves `CG(map_ptr_last)`.
    pub(crate) fn publish_compiled(&self, mut unit: Unit) -> Arc<Unit> {
        assign_map_ptrs(&mut unit, &self.map_ptr_last);
        unit.published()
    }

    /// One source compiled for one request alone, under the request's
    /// ini and working directory, its constant strings interned into the
    /// request's own memory and gone with it (design §5.2): a source the
    /// host handed over, and the string an `eval` hands over
    /// ([`SourceKind::EVAL`]), as php holds what it compiles for `-r`
    /// and for `eval`. Such a unit is never published: nothing outside
    /// the request reaches it, and it lives as long as the frame serving
    /// it or the functions and classes it declared.
    ///
    /// The runtime definition keys such a unit makes count on from the
    /// ones the request's earlier compiles made, as php's one
    /// `CG(rtd_key_counter)` does, so two `eval`s of one string declare
    /// two anonymous classes:
    ///
    /// ```php
    /// function make() { return eval('return new class { public $n; };'); }
    /// $a = make(); $b = make();
    /// var_dump(get_class($a) === get_class($b), $a instanceof $b);
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// ```
    pub(crate) fn compile_for(
        &self,
        req: &mut Request,
        source: &[u8],
        filename: &[u8],
        kind: SourceKind,
    ) -> Result<Arc<Unit>, CompileError> {
        let ini = CompileIni {
            precision: req.precision(),
            short_open_tag: req.short_open_tag(),
            assertions: req.assertions() >= 0,
            shebang: self.shebang,
            multibyte: req.typed_ini().multibyte,
        };
        let bound = crate::multibyte::bound(req, ini.multibyte);
        let cwd = req.cwd();
        let keys_before = req.keys_made();
        let stack = req.stack_limit();
        let declared = Declared {
            extended_info: self.extended_info && !req.compiling_host_string,
            ..self.declared()
        };
        let unit = req.compiling(|heap, request_classes| {
            let origin = Origin {
                filename,
                cwd: &cwd,
                kind,
                keys_before,
                request_classes,
                multibyte: bound.as_ref().map(crate::multibyte::Bound::settings),
            };
            compile(source, origin, ini, declared, stack, heap)
        })?;
        req.count_keys_made(unit.runtime_keys.len());
        Ok(req.take_unit(unit))
    }

    /// `zend_call_stack_init` on the calling thread, from the startup
    /// ini's `zend.max_allowed_stack_size` and `zend.reserved_stack_size`
    /// (`Z/Zend/zend.c:277-279`), as php runs it once per thread at
    /// `zend_post_startup` and at each new thread's start: what a request
    /// reads at its start, and what a unit compiled outside one is held
    /// to on the thread that asked for it.
    pub(crate) fn stack_limit(&self) -> StackLimit {
        let bytes = |name: &[u8]| self.ini_default(name).map_or(0, Request::ini_parse_bytes);
        StackLimit::init(
            bytes(b"zend.max_allowed_stack_size"),
            u64::try_from(bytes(b"zend.reserved_stack_size")).unwrap_or(0),
        )
    }

    /// `zend.multibyte` as the startup ini has it, which a unit bound for
    /// the cache is compiled under ([`CompileIni`]).
    pub(crate) fn compile_multibyte(&self) -> bool {
        self.compile_ini().multibyte
    }

    /// What a file compiled for no request is read under where the
    /// startup `zend.multibyte` is on: no request to bind the encoding
    /// functions to, so Zend's own dummies, and no list.
    fn unbound(&self) -> Option<Multibyte<'static>> {
        self.compile_ini().multibyte.then_some(Multibyte {
            encodings: &php_compile::multibyte::Dummy,
            detect_unicode: true,
            listed: &[],
        })
    }

    /// The compiler's ini as the startup one has it, which a unit bound
    /// for the cache is compiled under; a request compiles its own
    /// source under its own ([`CompileIni`]).
    fn compile_ini(&self) -> CompileIni {
        CompileIni {
            precision: self.typed_ini.precision,
            short_open_tag: self.typed_ini.short_open_tag,
            assertions: self.typed_ini.assertions >= 0,
            shebang: self.shebang,
            multibyte: self.typed_ini.multibyte,
        }
    }

    /// The unit for the file `path` names, as the unit cache holds it for
    /// no request: compiled and published once, and shared by every
    /// caller that asks for it afterwards while the file stands as it
    /// was (design §6.3). A file the engine cannot read is no unit at
    /// all, which is the `None` `serve` serves nothing for.
    pub(crate) fn cached(&self, path: &[u8], cwd: &Path, stack: StackLimit) -> Cached {
        let full = crate::unit_cache::full_path(path, cwd);
        let asked = crate::unit_cache::Asked {
            path: &full,
            source: None,
            written: None,
            validation: Validation::EveryHit,
            protection: 0,
            now: crate::unit_cache::now(),
            levels: crate::unit_cache::Levels::Memory,
        };
        let arena = self.arena.current();
        let (compiled, _, _) = self.units.file(
            &arena.files,
            asked,
            |source| {
                let multibyte = self.unbound();
                self.compile_file(
                    source,
                    path,
                    cwd,
                    stack,
                    RequestClasses::default(),
                    multibyte,
                )
            },
            |_| None,
            |unit| self.publish_compiled(unit),
            // No request to publish its early-bound classes for, so
            // a request that loads it binds them as its own.
            |_| Arc::new([]),
        );
        // With no request to own it, a unit held nowhere is published
        // all the same.
        crate::unit_cache::served(compiled, |unit| self.publish_compiled(unit))
    }

    /// A unit the file cache kept, read back from its bytes as a compiled
    /// one is: its strings and constant arrays laid down in the engine's
    /// table, and no map-pointer offset taken until it is held
    /// ([`Shared::publish_compiled`]); none where the bytes are no whole
    /// unit.
    pub(crate) fn decode_kept(&self, body: &[u8]) -> Option<Unit> {
        let mut strings = match self.strings.lock() {
            Ok(strings) => strings,
            // As in `publish`: the table is append-only and consistent.
            Err(poisoned) => poisoned.into_inner(),
        };
        crate::unit_cache::decode(body, &mut *strings)
    }

    /// `source`, read from the file `path` names, compiled and published
    /// as a file of its own, for a request that has declared
    /// `request_classes`.
    pub(crate) fn publish_file(
        &self,
        source: &[u8],
        path: &[u8],
        cwd: &Path,
        stack: StackLimit,
        request_classes: RequestClasses<'_>,
        multibyte: Option<Multibyte<'_>>,
    ) -> Result<Arc<Unit>, CompileError> {
        self.compile_file(source, path, cwd, stack, request_classes, multibyte)
            .map(|unit| self.publish_compiled(unit))
    }

    /// [`Shared::publish_file`]'s compile alone, which the unit cache
    /// publishes where it holds the unit and hands its request otherwise.
    pub(crate) fn compile_file(
        &self,
        source: &[u8],
        path: &[u8],
        cwd: &Path,
        stack: StackLimit,
        request_classes: RequestClasses<'_>,
        multibyte: Option<Multibyte<'_>>,
    ) -> Result<Unit, CompileError> {
        self.compile_shared(
            source,
            Origin {
                filename: path,
                cwd,
                kind: SourceKind::File,
                keys_before: 0,
                request_classes,
                multibyte,
            },
            stack,
        )
    }

    /// The permanent string for `bytes`, a name the engine or the bundle
    /// declared, or `None` for one no declaration made
    /// (`zend_string_init_interned_request`'s first lookup).
    pub(crate) fn permanent(&self, bytes: &[u8]) -> Option<php_heap::Str> {
        self.names.find(bytes)
    }

    /// The string a native's parameter defaults to where its stub writes
    /// the string `bytes`, made at startup; `None` for bytes no default
    /// writes.
    pub(crate) fn default_string(&self, bytes: &[u8]) -> Option<php_heap::Str> {
        self.default_strings.find(bytes)
    }

    /// The engine's string for `bytes` where a declaration or a published
    /// unit's constant made one, php's lookup over
    /// `interned_strings_permanent`, or opcache's shared table, in
    /// `zend_string_init_existing_interned`; nothing is interned.
    pub(crate) fn existing_interned(&self, bytes: &[u8]) -> Option<php_heap::Str> {
        if let Some(name) = self.permanent(bytes) {
            return Some(name);
        }
        let strings = match self.strings.lock() {
            Ok(strings) => strings,
            Err(poisoned) => poisoned.into_inner(),
        };
        strings.find(bytes)
    }

    /// How many strings the engine's table holds, opcache's
    /// `interned_strings_usage.number_of_strings`.
    pub(crate) fn interned_strings(&self) -> usize {
        match self.strings.lock() {
            Ok(strings) => strings.count(),
            Err(poisoned) => poisoned.into_inner().count(),
        }
    }

    /// The value the entry `name` starts every request with as
    /// `zend_ini_get_value` reads it, the empty string where it has none
    /// (php's `NULL`); `None` where php has no such entry.
    pub(crate) fn ini_default(&self, name: &[u8]) -> Option<&[u8]> {
        master_of(&self.ini, name)
    }

    /// `zend_disable_functions` over a started engine's table, as
    /// php-fpm's child calls it for its pool's `disable_functions`
    /// (`fpm_php_apply_defines_ex`): each native the list names, matched
    /// as the table's lowered key ([`disabled_functions`]), leaves the
    /// index its name finds it by, and its row stays where it stands, so
    /// every place already numbered keeps its number and no lookup, the
    /// compiler's among them, finds it
    /// ([`Shared::in_function_table`]).
    fn disable_functions(&mut self, list: &[u8]) {
        for name in disabled_functions(list) {
            let keyed = (self.function_names.find(&self.functions, name))
                .and_then(|at| self.functions.get(at))
                .is_some_and(|decl| decl.name.0.to_ascii_lowercase() == name);
            if keyed {
                self.function_names.remove(&self.functions, name);
            }
        }
    }

    /// Whether the native at `at` stands in `CG(function_table)`: its name
    /// finds it, which a native `disable_functions` took out of a started
    /// engine's table no longer does ([`Shared::disable_functions`]).
    pub(crate) fn in_function_table(&self, at: usize) -> bool {
        (self.functions.get(at))
            .is_some_and(|decl| self.function_names.find(&self.functions, &decl.name.0) == Some(at))
    }

    /// The master value of the entry `name` set to `value`, the entry
    /// moved last, and the typed entries with it.
    fn set_ini_default(&mut self, name: Vec<u8>, value: Option<Vec<u8>>) {
        self.typed_ini.update(&name, value.as_deref());
        self.ini.retain(|(n, _)| *n != name);
        self.ini.push((name, value));
    }

    /// The value the entry `name` starts every request with as php holds
    /// it, `INI_ORIG_STR`: `None` where it has none (php's `NULL`), as for
    /// a name php has no entry for.
    pub(crate) fn ini_default_value(&self, name: &[u8]) -> Option<&[u8]> {
        self.ini
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .and_then(|(_, v)| v.as_deref())
    }

    pub(crate) fn ini_defaults(&self) -> &[(Vec<u8>, Option<Vec<u8>>)] {
        &self.ini
    }

    /// `cfg_get_entry`: the row of `configuration_hash` under `name`.
    pub(crate) fn configured(&self, name: &[u8]) -> Option<&Configured> {
        self.configuration
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, entry)| entry)
    }

    /// `zend_get_configuration_directive`, what an INI text's `${name}`
    /// reads first: the string the host's ini text set under `name`,
    /// whether or not an entry of that name exists.
    pub(crate) fn directive(&self, name: &[u8]) -> Option<&[u8]> {
        self.configured(name).and_then(Configured::text)
    }

    /// The value of the constant `name` as the bundle declared it, or as
    /// the engine's own table has it: a value nothing owns, since the
    /// string ones are the engine's interned strings.
    /// `PHP_SAPI` for a request its host named `sapi`: the string the
    /// engine interned for that name at build, or, for a name no host
    /// of [`crate::constants::HOSTS`] has, one interned into the
    /// engine's table the first time a request asks, where every later
    /// request finds it.
    pub(crate) fn sapi_constant(&self, sapi: &[u8]) -> Option<Value> {
        if let Some((_, z)) = self.sapi_names.iter().find(|(n, _)| n.as_ref() == sapi) {
            return Some(exec::value_of(*z));
        }
        let mut strings = match self.strings.lock() {
            Ok(strings) => strings,
            Err(poisoned) => poisoned.into_inner(),
        };
        let interned = strings.intern(sapi).ok()?;
        Some(exec::value_of(Zval::string(interned)))
    }

    pub(crate) fn constant(&self, name: &[u8]) -> Option<Value> {
        self.constant_at(self.constant_names.find(&self.constants, name)?)
    }

    /// The value of the constant at `at` of the declared table, as
    /// [`Shared::constant`] reads it.
    pub(crate) fn constant_at(&self, at: usize) -> Option<Value> {
        let (z, _) = self.constant_values.get(at)?.as_ref()?;
        Some(exec::value_of(*z))
    }

    /// `ZEND_CONSTANT_MODULE_NUMBER` of the constant `name` an extension
    /// or the engine declared with a value.
    pub(crate) fn constant_module(&self, name: &[u8]) -> Option<ExtensionId> {
        let at = self.constant_names.find(&self.constants, name)?;
        self.constant_values.get(at)?.as_ref()?;
        self.constant_modules.get(at).copied()
    }

    /// `CONST_DEPRECATED` of an extension's or the engine's constant,
    /// none for a name no extension declared.
    pub(crate) fn constant_deprecated(&self, name: &[u8]) -> Option<bool> {
        let at = self.constant_names.find(&self.constants, name)?;
        let (_, flags) = self.constant_values.get(at)?.as_ref()?;
        Some(flags.deprecated)
    }

    /// The native named `name` whatever its case, its place in
    /// `functions`: `zend_hash_find(CG(function_table), lcname)`.
    pub(crate) fn function_at(&self, name: &[u8]) -> Option<usize> {
        self.function_names.find(&self.functions, name)
    }

    /// The engine's class named `name` whatever its case, or the class a
    /// stub alias of that name names: `zend_hash_find(CG(class_table),
    /// lcname)` over the table startup leaves.
    pub(crate) fn class_at(&self, name: &[u8]) -> Option<ClassId> {
        self.class_keys.find(&self.classes, name)
    }
}

/// The core ini entries and their defaults: php's `main.c` table and
/// six entries of `Zend/zend.c`'s, the two a throwable's trace reads,
/// the two the stack limit reads (`Shared::stack_limit`),
/// `zend.enable_gc`, which turns the cycle collector on, and
/// `fatal_error_backtraces`, php 8.5's, `On` by default
/// (`Z/Zend/zend.c:263`), which a fatal's trace reads; the CLI's
/// `HARDCODED_INI` is its host's first `ini_entries`, not these.
/// An entry php registers with no value, `NULL`, has `None`: `ini_get()`
/// reads it as the empty string, while its `on_modify` is handed it
/// apart from one set empty. `error_prepend_string` and
/// `error_append_string` are such, and so are `internal_encoding`,
/// `input_encoding` and `output_encoding`, which an extension reads
/// before `default_charset` (`php_get_internal_encoding` and its kin);
/// `default_mimetype` is `SAPI_DEFAULT_MIMETYPE`. `short_open_tag`
/// is the one entry the scanner reads, `CG(short_tags)`, and
/// `token_get_all` with it.
///
/// ```php
/// var_dump(ini_get('internal_encoding'), ini_get('output_encoding'), ini_get('default_mimetype'), ini_set('input_encoding', 'ISO-8859-1'), ini_get('input_encoding'));
/// ```
/// ```expect
/// string(0) ""
/// string(0) ""
/// string(9) "text/html"
/// string(0) ""
/// string(10) "ISO-8859-1"
/// ```
const CORE_INI: &[(&str, Option<&str>)] = &[
    ("short_open_tag", Some("1")),
    ("fatal_error_backtraces", Some("1")),
    ("precision", Some("14")),
    ("serialize_precision", Some("-1")),
    // `error_reporting`, `NULL` in php's table (`Z/Zend/zend.c:262`),
    // which `ini_get` reads as the empty string, phpinfo as `no value`
    // and `OnUpdateErrorReporting` as `E_ALL`.
    ("error_reporting", None),
    ("display_errors", Some("1")),
    // `display_startup_errors`, `On` (`Z/main/main.c:800`): whether what
    // is raised while php starts is displayed ([`Request::display_errors`]).
    ("display_startup_errors", Some("1")),
    ("log_errors", Some("0")),
    // `error_log`, `NULL` in php's table, and `error_log_mode`, both
    // `PHP_INI_ALL` (`Z/main/main.c:835-836`): the file `php_log_err`
    // appends to and the permissions it creates one with.
    ("error_log", None),
    ("error_log_mode", Some("0644")),
    ("html_errors", Some("1")),
    // `docref_root` and `docref_ext`, `PHP_INI_ALL`: where a function's
    // diagnostic under `html_errors` links to (`raising::origin`).
    ("docref_root", Some("")),
    ("docref_ext", Some("")),
    ("error_prepend_string", None),
    ("error_append_string", None),
    ("ignore_repeated_errors", Some("0")),
    ("ignore_repeated_source", Some("0")),
    ("report_zend_debug", Some("0")),
    ("xmlrpc_errors", Some("0")),
    ("xmlrpc_error_number", Some("0")),
    // `max_memory_limit`, `PHP_INI_SYSTEM`, declared before
    // `memory_limit`, whose handler holds it under the maximum, and
    // `report_memleaks`, deprecated when off (`Z/main/main.c:814-866`).
    ("max_memory_limit", Some("-1")),
    ("memory_limit", Some("128M")),
    ("report_memleaks", Some("1")),
    ("max_execution_time", Some("30")),
    // `hard_timeout`, `PHP_INI_SYSTEM` (`Z/main/main.c:885`): the seconds
    // past a timeout the execution timer waits for the loop to take it
    // before it ends the process ([`crate::interrupt::Timer`]).
    ("hard_timeout", Some("2")),
    ("default_charset", Some("UTF-8")),
    ("default_mimetype", Some("text/html")),
    ("internal_encoding", None),
    ("input_encoding", None),
    ("output_encoding", None),
    // `PHP_INCLUDE_PATH`, this build's `.:${PEAR_INSTALLDIR}`, which
    // `DEFAULT_INCLUDE_PATH` says too.
    ("include_path", Some(crate::constants::PHP_INCLUDE_PATH)),
    // `allow_url_fopen` and `allow_url_include`, `PHP_INI_SYSTEM`
    // (`Z/main/main.c:876-877`): whether a URL wrapper opens, and opens
    // for an include (`php_stream_locate_url_wrapper`).
    ("allow_url_fopen", Some("1")),
    ("allow_url_include", Some("0")),
    // `open_basedir`, `PHP_INI_ALL` and `NULL` in php's table
    // (`Z/main/main.c:841`): the base directories the guard holds a path
    // to ([`Request::check_open_basedir`]), any list at startup and only
    // a tighter one after (`OnUpdateBaseDir`).
    ("open_basedir", None),
    ("zend.exception_ignore_args", Some("0")),
    ("zend.exception_string_param_max_len", Some("15")),
    ("zend.max_allowed_stack_size", Some("0")),
    ("zend.reserved_stack_size", Some("0")),
    // `zend.assertions`, `1` (`Z/Zend/zend.c:264`), `EG(assertions)`:
    // what `zend_compile_assert` compiles under and `ASSERT_CHECK`
    // reads, moved between `-1` and the rest only at startup
    // (`OnUpdateAssertions`, [`Request::assertions_updated`]).
    ("zend.assertions", Some("1")),
    // `zend.enable_gc`, `On` (`Z/Zend/zend.c:265`), whose `on_modify` is
    // the collector's `gc_enable`.
    ("zend.enable_gc", Some("1")),
    // `zend.multibyte`, `zend.script_encoding` and `zend.detect_unicode`
    // (`Z/Zend/zend.c:266-268`): whether a file is read in its own
    // encoding, the encodings it may be in, and whether a byte order
    // mark or NULs say which first ([`crate::multibyte`]).
    ("zend.multibyte", Some("0")),
    ("zend.script_encoding", None),
    ("zend.detect_unicode", Some("1")),
    // `zend.signal_check`, `ZEND_INI_SYSTEM` and `SIGNAL_CHECK_DEFAULT`
    // (`Z/Zend/zend.c:270`): whether shutdown checks the signal handlers
    // `zend_signal` installed were left in place.
    ("zend.signal_check", Some("0")),
    // `fiber.stack_size`, `NULL` in php's table, which `ini_get` reads
    // as the empty string and `OnUpdateFiberStackSize` as php's default.
    ("fiber.stack_size", None),
    // `disable_functions`, `PHP_INI_SYSTEM` (`Z/main/main.c:872`): read
    // once, as the function table is built (`function_table`).
    ("disable_functions", Some("")),
    // `variables_order` and `request_order`, `PHP_INI_SYSTEM|PHP_INI_PERDIR`
    // (`Z/main/main.c:851`): which auto-globals the request fills and
    // the order `$_REQUEST` merges them in, read as each is made.
    ("variables_order", Some("EGPCS")),
    ("request_order", None),
    // `main.c`'s entries for `mail()` and `php_syslog`, and
    // `sys_temp_dir`, `PHP_INI_SYSTEM` or `PHP_INI_SYSTEM|PHP_INI_PERDIR`
    // but `SMTP`, `smtp_port` and `syslog.filter`; a `NULL` default is
    // `None`, as the two error strings' above are.
    ("sys_temp_dir", None),
    ("SMTP", Some("localhost")),
    ("smtp_port", Some("25")),
    ("mail.add_x_header", Some("0")),
    ("mail.mixed_lf_and_crlf", Some("0")),
    ("mail.cr_lf_mode", Some("crlf")),
    ("mail.log", None),
    ("sendmail_path", Some("/usr/sbin/sendmail -t -i")),
    ("sendmail_from", None),
    ("mail.force_extra_parameters", None),
    ("syslog.facility", Some("LOG_USER")),
    ("syslog.ident", Some("php")),
    ("syslog.filter", Some("no-ctrl")),
    // `main.c`'s `highlight.*`, `php_get_highlight_struct`'s colours,
    // which `highlight_string()`, `highlight_file()` and `php -s` read
    // (`Z/main/main.c:763-767`, `Z/main/php_ini.h`'s `HL_*_COLOR`).
    ("highlight.comment", Some("#FF8000")),
    ("highlight.default", Some("#0000BB")),
    ("highlight.html", Some("#000000")),
    ("highlight.keyword", Some("#007700")),
    ("highlight.string", Some("#DD0000")),
    // `ignore_user_abort`, `PG(ignore_user_abort)`, and the two `dl()`
    // reads, `enable_dl` and `extension_dir`, `PHP_INI_SYSTEM`.
    ("ignore_user_abort", Some("0")),
    ("implicit_flush", Some("0")),
    // `realpath_cache_size` and `realpath_cache_ttl`, `PHP_INI_SYSTEM`,
    // the realpath cache's limit and time to live (`crate::virtual_cwd`).
    ("realpath_cache_size", Some("4096K")),
    // `arg_separator.input` and `arg_separator.output`, `PHP_INI_ALL`:
    // what `parse_str` splits on and what the URL rewriter and
    // `http_build_query` join with.
    ("arg_separator.input", Some("&")),
    ("arg_separator.output", Some("&")),
    // `auto_prepend_file` and `auto_append_file`,
    // `PHP_INI_SYSTEM|PHP_INI_PERDIR`: required before and after the
    // primary script (`php_execute_script`).
    ("auto_prepend_file", None),
    ("auto_append_file", None),
    ("realpath_cache_ttl", Some("120")),
    // `main.c`'s entries for what a request of the web carries
    // (`Z/main/main.c:802-878`): the `X-Powered-By` header, the auto
    // globals made when named, `$_SERVER['argv']`, the body and its
    // limits, and the uploads.
    ("expose_php", Some("1")),
    ("auto_globals_jit", Some("1")),
    ("register_argc_argv", Some("0")),
    ("enable_post_data_reading", Some("1")),
    ("post_max_size", Some("8M")),
    ("max_input_time", Some("-1")),
    ("max_input_nesting_level", Some("64")),
    ("max_input_vars", Some("1000")),
    ("file_uploads", Some("1")),
    ("upload_max_filesize", Some("2M")),
    ("upload_tmp_dir", None),
    ("max_file_uploads", Some("20")),
    ("max_multipart_body_parts", Some("-1")),
    ("enable_dl", Some("1")),
    ("extension_dir", Some(crate::constants::PHP_EXTENSION_DIR)),
    // Two `main.c` registers for ext/standard to read (`Z/main/main.c:822, 863`).
    ("unserialize_callback_func", None),
    ("browscap", None),
    // `doc_root`, `user_dir` and `user_ini.*`, `PHP_INI_SYSTEM`
    // (`Z/main/main.c:829-884`): what a host of the web reads to find a
    // request's program and its `.user.ini` files.
    // `output_buffering` and `output_handler`, `PHP_INI_PERDIR|
    // PHP_INI_SYSTEM` (`Z/main/main.c:816-817`): the buffer a request
    // starts with ([`crate::output::start_at_request`]).
    ("output_buffering", Some("0")),
    ("output_handler", None),
    ("doc_root", None),
    ("user_dir", None),
    ("user_ini.filename", Some(".user.ini")),
    ("user_ini.cache_ttl", Some("300")),
];

/// `php_ini_builder_define`'s line (`Z/main/php_ini_builder.c:63-81`)
/// for one `name=value`: as given, or with the value in double quotes
/// where it opens with a byte that is neither alphanumeric nor a quote,
/// so that ` off` and `;x` stand as written while `off ` is the boolean
/// and `UTC;` is `UTC` before a comment.
fn define_line(entries: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    let quoted = value
        .first()
        .is_some_and(|first| !first.is_ascii_alphanumeric() && *first != b'"' && *first != b'\'');
    entries.extend_from_slice(name);
    entries.push(b'=');
    if quoted {
        entries.push(b'"');
    }
    entries.extend_from_slice(value);
    if quoted {
        entries.push(b'"');
    }
    entries.push(b'\n');
}

/// `php_init_config` (`Z/main/php_ini.c:409-723`): the host's
/// `ini_defaults`, then the php.ini it found, its path set as
/// `cfg_file_path`, then each scanned file, a file whose parse failed
/// dropped from the list, then its `ini_entries`, each text read by the
/// one INI grammar ([`crate::ini_syntax`]) in the normal scanner into the
/// configuration table by `php_ini_parser_cb`, the last value of a name
/// winning. A syntax error stops that text's parse with the entries
/// before it kept, and `ini_error` writes its line to the process's
/// `stderr`, naming the file, or `Unknown` for the entries.
struct Configuration {
    table: Vec<(Vec<u8>, Configured)>,
    stderr: Vec<u8>,
    sections: [bool; 2],
    loads: Loads,
}

/// php's `extension_lists`: the names `zend_extension=` and
/// `extension=` lines set outside a `[PATH=]` or `[HOST=]` section, which
/// never reach the table, each list in the order written.
#[derive(Default)]
struct Loads {
    engine: Vec<Vec<u8>>,
    functions: Vec<Vec<u8>>,
}

/// One row of php's `configuration_hash`, what `cfg_get_entry` finds
/// and `get_cfg_var()` answers: a string `name=value` set, or the array
/// `name[]=value` and `name[offset]=value` lines built, or a `[PATH=]`
/// or `[HOST=]` section's table, which holds either in turn
/// (`php_ini_parser_cb`, `Z/main/php_ini.c:199-305`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Configured {
    Str(Vec<u8>),
    Array(Vec<(ConfiguredKey, Configured)>),
}

/// A key of such an array, as `zend_symtable_update` makes it: an
/// offset that reads as a canonical integer is that integer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConfiguredKey {
    Index(i64),
    Name(Vec<u8>),
}

impl Configured {
    /// The string the entry holds; an array has none, and no directive
    /// reads one.
    fn text(&self) -> Option<&[u8]> {
        match self {
            Configured::Str(bytes) => Some(bytes),
            Configured::Array(_) => None,
        }
    }
}

impl Configuration {
    fn read(
        defaults: &[(Vec<u8>, Vec<u8>)],
        files: &mut crate::api::request::IniFiles,
        entries: &[u8],
        constants: &[ConstDecl],
    ) -> Configuration {
        let table = defaults
            .iter()
            .map(|(name, value)| (name.clone(), Configured::Str(value.clone())))
            .collect();
        let mut startup = Startup {
            constants,
            table,
            special: false,
            section: None,
            stderr: Vec::new(),
            sections: [false; 2],
            loads: Loads::default(),
        };
        if let Some(file) = &mut files.loaded {
            startup.read(&std::mem::take(&mut file.bytes), &file.path);
            startup.set(
                b"cfg_file_path".to_vec(),
                Configured::Str(file.path.clone()),
            );
        }
        files
            .scanned
            .retain_mut(|file| startup.read(&std::mem::take(&mut file.bytes), &file.path));
        startup.read(entries, b"Unknown");
        Configuration {
            table: startup.table,
            stderr: startup.stderr,
            sections: startup.sections,
            loads: startup.loads,
        }
    }

    /// The directives the table sets, its strings, in the order first set.
    fn directives(&self) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + '_ {
        self.table
            .iter()
            .filter_map(|(name, entry)| entry.text().map(|text| (name.clone(), text.to_vec())))
    }
}

/// The grammar's host at startup: Zend's own constants and `main`'s,
/// registered before `php_init_config` runs and so before any
/// extension's; a `${…}` reads what the text set before it, then the
/// process's environment; and `php_ini_parser_cb` over the table.
struct Startup<'c> {
    constants: &'c [ConstDecl],
    table: Vec<(Vec<u8>, Configured)>,
    /// `is_special_section`: the last section header was `[PATH…]` or
    /// `[HOST…]`, where `extension=` is an entry like any other.
    special: bool,
    /// `active_ini_hash`: the key of the `[PATH=]` or `[HOST=]` table the
    /// entries go to, which a CGI applies per request and a CLI never.
    section: Option<Vec<u8>>,
    /// What `ini_error` wrote.
    stderr: Vec<u8>,
    /// `has_per_dir_config` and `has_per_host_config`.
    sections: [bool; 2],
    loads: Loads,
}

/// `zend_get_constant` for the INI grammar: the declared value as a
/// string, a double as `precision` digits write it.
fn constant_text(constants: &[ConstDecl], name: &[u8], precision: i32) -> Option<Vec<u8>> {
    let decl = constants.iter().find(|c| &*c.name.0 == name)?;
    match &decl.value {
        ConstExpr::Null | ConstExpr::Bool(false) => Some(Vec::new()),
        ConstExpr::Bool(true) => Some(b"1".to_vec()),
        ConstExpr::Long { value: n, .. } => Some(n.to_string().into_bytes()),
        ConstExpr::Double(d) => {
            Some(php_heap::double_to_string(*d, precision, php_heap::Exponent::Upper).into_bytes())
        }
        ConstExpr::Str(s) => Some(s.to_vec()),
        _ => None,
    }
}

/// The grammar's host for [`Engine::parse_ini`]: the engine's
/// constants and configuration, the process's environment, and the
/// events kept, a double written at php's default `precision`.
struct Configure<'e> {
    shared: &'e Shared,
    events: Vec<IniEvent>,
}

impl crate::ini_syntax::Host for Configure<'_> {
    type Error = std::convert::Infallible;

    fn constant(&mut self, name: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(constant_text(&self.shared.constants, name, 14))
    }

    fn directive(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        self.shared.directive(name).map(<[u8]>::to_vec)
    }

    fn env(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        std::env::var_os(&*String::from_utf8_lossy(name))
            .map(std::ffi::OsString::into_encoded_bytes)
    }

    fn double(&mut self, d: f64) -> Result<Vec<u8>, Self::Error> {
        Ok(php_heap::double_to_string(d, 14, php_heap::Exponent::Upper).into_bytes())
    }

    fn event(&mut self, event: IniEvent<crate::ini_syntax::Val>) -> Result<(), Self::Error> {
        self.events.push(event.map(|val| val.value));
        Ok(())
    }
}

impl crate::ini_syntax::Host for Startup<'_> {
    type Error = std::convert::Infallible;

    fn constant(&mut self, name: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(constant_text(self.constants, name, 1))
    }

    fn directive(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        self.table
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .and_then(|(_, value)| value.text().map(<[u8]>::to_vec))
    }

    fn env(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        std::env::var_os(&*String::from_utf8_lossy(name))
            .map(std::ffi::OsString::into_encoded_bytes)
    }

    /// `zend_double_to_str` before any ini entry is registered, when
    /// `EG(precision)` is still zero and `zend_gcvt` is handed one digit.
    fn double(&mut self, d: f64) -> Result<Vec<u8>, Self::Error> {
        Ok(php_heap::double_to_string(d, 1, php_heap::Exponent::Upper).into_bytes())
    }

    fn event(&mut self, event: IniEvent<crate::ini_syntax::Val>) -> Result<(), Self::Error> {
        self.entry(event.map(|val| val.value));
        Ok(())
    }
}

impl Startup<'_> {
    /// `zend_parse_ini_file` or `zend_parse_ini_string` over one text,
    /// after `RESET_ACTIVE_INI_HASH`: whether it parsed.
    fn read(&mut self, text: &[u8], file: &[u8]) -> bool {
        (self.special, self.section) = (false, None);
        match crate::ini_syntax::parse(text, IniScanner::Normal, self) {
            Ok(Ok(())) => true,
            Ok(Err((message, line))) => {
                self.stderr
                    .extend_from_slice(format!("PHP:  {message} in ").as_bytes());
                self.stderr.extend_from_slice(file);
                self.stderr
                    .extend_from_slice(format!(" on line {line}\n").as_bytes());
                false
            }
            Err(never) => match never {},
        }
    }

    /// `zend_hash_update` of the table.
    fn set(&mut self, name: Vec<u8>, value: Configured) {
        match self.table.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = value,
            None => self.table.push((name, value)),
        }
    }

    /// `php_ini_parser_cb` over one statement of the startup entries.
    fn entry(&mut self, event: IniEvent) {
        match event {
            IniEvent::Section(name) => {
                let name = ini_text(&name);
                let key = [b"PATH".as_slice(), b"HOST"]
                    .into_iter()
                    .find(|prefix| name.len() >= 4 && name[..4].eq_ignore_ascii_case(prefix))
                    .map(|_| &name[4..]);
                self.special = key.is_some();
                if key.is_some() {
                    self.sections[usize::from(name[..4].eq_ignore_ascii_case(b"HOST"))] = true;
                }
                if let Some(key) = key.filter(|key| !key.is_empty()) {
                    let mut key = key.to_vec();
                    if name[..4].eq_ignore_ascii_case(b"HOST") {
                        key.make_ascii_lowercase();
                    }
                    while key.last().is_some_and(|b| matches!(b, b'/' | b'\\')) {
                        key.pop();
                    }
                    let lead = key.iter().take_while(|b| matches!(b, b'=' | b' ' | b'\t'));
                    let key = key[lead.count()..].to_vec();
                    match self.table.iter().find(|(n, _)| *n == key) {
                        Some((_, Configured::Array(_))) => self.section = Some(key),
                        Some(_) => {}
                        None => {
                            self.table
                                .push((key.clone(), Configured::Array(Vec::new())));
                            self.section = Some(key);
                        }
                    }
                }
            }
            IniEvent::Entry(label, Some(value)) => {
                let label = ini_text(&label);
                let loads = label.eq_ignore_ascii_case(b"extension")
                    || label.eq_ignore_ascii_case(b"zend_extension");
                if loads && !self.special {
                    let list = if label.len() == 9 {
                        &mut self.loads.functions
                    } else {
                        &mut self.loads.engine
                    };
                    return list.push(ini_text(&value));
                }
                let value = ini_text(&value);
                if let Some(rows) = self.section_rows() {
                    let key = configured_key(&label);
                    match rows.iter_mut().find(|(k, _)| *k == key) {
                        Some(row) => row.1 = Configured::Str(value),
                        None => rows.push((key, Configured::Str(value))),
                    }
                    return;
                }
                self.set(label, Configured::Str(value));
            }
            // `ZEND_INI_PARSER_POP_ENTRY`: the array under the label in
            // the active table, a section's where one is active, made where
            // the label holds none, the value placed at the offset or
            // appended where the offset is empty.
            IniEvent::PopEntry(label, value, offset) => {
                let label = ini_text(&label);
                let (value, offset) = (ini_text(&value), ini_text(&offset));
                match self.section_rows() {
                    Some(rows) => popped(rows, configured_key(&label), &offset, value),
                    None => popped(&mut self.table, label, &offset, value),
                }
            }
            // A bare label sets nothing.
            IniEvent::Entry(_, None) => {}
        }
    }
}

impl Startup<'_> {
    /// The rows of the `[PATH=]` or `[HOST=]` table that is active.
    fn section_rows(&mut self) -> Option<&mut Vec<(ConfiguredKey, Configured)>> {
        let section = self.section.as_ref()?;
        match self.table.iter_mut().find(|(n, _)| n == section) {
            Some((_, Configured::Array(rows))) => Some(rows),
            _ => None,
        }
    }
}

/// The array under `label` in `table`, made where it holds none, and
/// `value` inserted into it at `offset`.
fn popped<K: PartialEq>(table: &mut Vec<(K, Configured)>, label: K, offset: &[u8], value: Vec<u8>) {
    let at = table.iter().position(|(n, _)| *n == label);
    let at = at.unwrap_or(table.len());
    if at == table.len() {
        table.push((label, Configured::Array(Vec::new())));
    }
    let entry = &mut table[at].1;
    if !matches!(entry, Configured::Array(_)) {
        *entry = Configured::Array(Vec::new());
    }
    if let Configured::Array(rows) = entry {
        configured_insert(rows, offset, value);
    }
}

/// A key as `zend_symtable_update` makes it.
fn configured_key(offset: &[u8]) -> ConfiguredKey {
    match canonical_integer(offset) {
        Some(n) => ConfiguredKey::Index(n),
        None => ConfiguredKey::Name(offset.to_vec()),
    }
}

/// `zend_symtable_update` for a non-empty offset, else
/// `zend_hash_next_index_insert`, over one array of the table.
fn configured_insert(rows: &mut Vec<(ConfiguredKey, Configured)>, offset: &[u8], value: Vec<u8>) {
    let value = Configured::Str(value);
    let key = if offset.is_empty() {
        let next = rows
            .iter()
            .filter_map(|(key, _)| match key {
                ConfiguredKey::Index(n) => Some(n.saturating_add(1)),
                ConfiguredKey::Name(_) => None,
            })
            .max()
            .unwrap_or(0)
            .max(0);
        ConfiguredKey::Index(next)
    } else {
        configured_key(offset)
    };
    match rows.iter_mut().find(|(k, _)| *k == key) {
        Some(row) => row.1 = value,
        None => rows.push((key, value)),
    }
}

/// `ZEND_HANDLE_NUMERIC_STR`: the offset as an integer where it is one
/// written canonically, no leading zero, no `+`, within `i64`.
fn canonical_integer(bytes: &[u8]) -> Option<i64> {
    let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
    let canonical = !digits.is_empty()
        && digits.iter().all(u8::is_ascii_digit)
        && (digits == b"0" || digits[0] != b'0')
        && bytes != b"-0";
    if !canonical {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// A label or a value of the normal scanner, which types nothing, so
/// every one is a string.
fn ini_text(value: &IniValue) -> Vec<u8> {
    match value {
        IniValue::Str(bytes) => bytes.clone(),
        _ => Vec::new(),
    }
}

/// `ZEND_DUMP_BEFORE_OPTIMIZER` (`Z/Zend/Optimizer/zend_optimizer.h:41`):
/// the bit of `opcache.opt_debug_level` that asks for each op array as
/// the compiler left it.
const BEFORE_OPTIMIZER: i64 = 0x1_0000;

/// `zend_ini_parse_quantity`, which `OnUpdateLong` reads every
/// whole-number ini entry by: `crate::ini_quantity`, the one reading
/// of it, here without the sentence `Request::ini_quantity_warn` raises.
fn ini_quantity(value: &[u8]) -> i64 {
    crate::ini_quantity::parse(value).0
}

/// `php_module_startup`'s check for deprecated directives
/// (`Z/main/main.c:2393-2450`), its last step: each name of the
/// configuration table `cfg_get_long` reads as a number other than zero,
/// `zval_get_long` over the string or the array, raised at `Unknown`
/// line 0. `allow_url_include` is deprecated; each name of the second
/// half is `E_CORE_ERROR`, the first ending the startup, so no request
/// runs ([`Engine::begin`]).
///
/// ```php
/// foreach ([['allow_url_include=1', 'magic_quotes_gpc=1', 'track_errors=1'], ['register_globals=0.9e1'], ['safe_mode=0.5', 'allow_url_include=On']] as $set) {
///     $args = [PHP_BINARY, '-n'];
///     foreach ($set as $d) { array_push($args, '-d', $d); }
///     $p = proc_open([...$args, '-r', 'echo "ran\n";'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
///     echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]); var_dump(proc_close($p));
/// }
/// ```
/// ```expect process
/// Deprecated: Directive 'allow_url_include' is deprecated in Unknown on line 0
///
/// Fatal error: Directive 'magic_quotes_gpc' is no longer available in PHP in Unknown on line 0
/// Stack trace:
/// #0 {main}
/// int(1)
///
/// Fatal error: Directive 'register_globals' is no longer available in PHP in Unknown on line 0
/// Stack trace:
/// #0 {main}
/// int(1)
///
/// Deprecated: Directive 'allow_url_include' is deprecated in Unknown on line 0
/// ran
/// int(0)
/// ```
fn startup_directives(req: &mut Request, shared: &Shared) {
    const REMOVED: [&str; 18] = [
        "allow_call_time_pass_reference",
        "asp_tags",
        "define_syslog_variables",
        "highlight.bg",
        "magic_quotes_gpc",
        "magic_quotes_runtime",
        "magic_quotes_sybase",
        "register_globals",
        "register_long_arrays",
        "safe_mode",
        "safe_mode_gid",
        "safe_mode_include_dir",
        "safe_mode_exec_dir",
        "safe_mode_allowed_env_vars",
        "safe_mode_protected_env_vars",
        "zend.ze1_compatibility_mode",
        "track_errors",
        "disable_classes",
    ];
    let set = |name: &str| {
        shared
            .configured(name.as_bytes())
            .is_some_and(|entry| match entry {
                Configured::Str(text) => match php_heap::numeric_prefix(text) {
                    Some((php_heap::Number::Long(n), _)) => n != 0,
                    Some((php_heap::Number::Double(d), _)) => d.is_finite() && d.abs() >= 1.0,
                    None => false,
                },
                Configured::Array(rows) => !rows.is_empty(),
            })
    };
    if set("allow_url_include") {
        let message = "Directive 'allow_url_include' is deprecated";
        crate::api::raising::raise_at(req, Level::Deprecated, message, b"Unknown", 0);
    }
    if let Some(name) = REMOVED.into_iter().find(|name| set(name)) {
        let message = format!("Directive '{name}' is no longer available in PHP");
        crate::api::raising::raise_at(req, Level::CoreError, &message, b"Unknown", 0);
    }
}

/// The core entries' handlers over what the host set, in the order
/// `main.c` declares them (`Z/main/main.c:807-885`): each whole-number
/// entry's `zend_ini_parse_quantity_warn` over the value the host set,
/// one `OnUpdateLongGEZero` refused among them, `report_memleaks` off is
/// `OnUpdateReportMemleaks`'s deprecation, and `memory_limit` past
/// `max_memory_limit` is `OnChangeMemoryLimit`'s warning with the
/// maximum's text in its place. What an entry takes is the answer.
fn core_startup(req: &mut Request) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    // `main.c` declares `report_memleaks` after `max_input_time` and
    // `memory_limit` after `max_input_vars`.
    let (before_memleaks, rest) = split_after(&LongHandler::ENTRIES, b"max_input_time");
    let (before_limit, after_limit) = split_after(rest, b"max_input_vars");
    quantities_warn(req, before_memleaks);
    if req
        .ini_get(b"report_memleaks")
        .is_some_and(|v| !Request::ini_parse_bool(v))
    {
        let message = "Directive 'report_memleaks' is deprecated";
        crate::api::raising::docref(req, Level::Deprecated, message);
    }
    quantities_warn(req, before_limit);
    let limit = req
        .ini_get(b"memory_limit")
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    let taken = match req.memory_limit_verdict(&limit) {
        Some(taken) if taken != limit => {
            req.take_startup_value(b"memory_limit", &taken);
            vec![(b"memory_limit".to_vec(), Some(taken))]
        }
        _ => Vec::new(),
    };
    quantities_warn(req, after_limit);
    taken
}

/// `entries` up to and with the one named `name`, and the rest.
fn split_after<'a>(entries: &'a [LongEntry], name: &[u8]) -> (&'a [LongEntry], &'a [LongEntry]) {
    let at = (entries.iter().position(|(n, _)| *n == name)).map_or(0, |at| at + 1);
    entries.split_at(at)
}

/// `zend_register_ini_entries_ex`'s handler over each of `entries` the
/// host set a value for, as [`Request::ini_quantity_warn`] reads it.
/// The value comes from the configuration, not the entry, since a
/// value an `OnUpdateLongGEZero` refused was warned about before the
/// refusal, and never reached the entry ([`Request::ini_verdict`]).
fn quantities_warn(req: &mut Request, entries: &[LongEntry]) {
    for (name, _) in entries {
        if let Some(value) = req.shared.directive(name).map(<[u8]>::to_vec) {
            req.ini_quantity_warn(name, &value);
        }
    }
}

/// `php_ini_register_extensions` (`Z/main/php_ini.c:741-749`) with no
/// loader behind it (§7.1): each `zend_extension=` name, then each
/// `extension=` name, through php's path building for its kind, and the
/// `dlopen` this engine cannot make failing with "dynamic modules are not
/// supported" (`trace:ini-extension-no-loader`). An absolute Zend
/// extension is `zend_load_extension`'s line on `stderr`, the answer.
///
/// ```php
/// // The parentheses carry the host's dlerror text, the paths php's.
/// $tried = '/^Warning: (.+) \(tried: (\S+) \(.+?\), (\/\S+) \(.+\)\) in Unknown on line 0$/m';
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'extension_dir=/nope', '-d', 'extension=x', '-v'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// $out = stream_get_contents($pipes[1]); echo stream_get_contents($pipes[2]), proc_close($p), "\n";
/// preg_match($tried, $out, $m); echo "$m[1]: $m[2], $m[3]\n", str_contains($out, "\nPHP 8.5.9 (cli)") ? "version\n" : $out;
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'extension_dir=/z/', '-d', 'extension=/a/q.so', '-d', 'zend_extension=y', '-d', 'zend_extension=/a/z.so', '-r', 'echo "ran\n";'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// $out = stream_get_contents($pipes[1]); echo strstr(stream_get_contents($pipes[2]), ':  ', true), "\n", proc_close($p), "\n";
/// preg_match_all($tried, $out, $m, PREG_SET_ORDER); foreach ($m as $w) { echo "$w[1]: $w[2], $w[3]\n"; } echo substr($out, strrpos(rtrim($out), "\n") + 1);
/// // A directory and a name that are not UTF-8 are written as their bytes.
/// $p = proc_open([PHP_BINARY, '-n', '-d', "extension_dir=a/\xE4/w", '-d', 'extension=x', '-d', "zend_extension=/a/\xE4.so", '-r', ''], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// $out = stream_get_contents($pipes[1]); echo addcslashes(strstr(stream_get_contents($pipes[2]), ':  ', true), "\200..\377"), "\n", proc_close($p), "\n";
/// preg_match('/^Warning: (.+) \(tried: (\S+) \(.+?\), (a\S+) \(.+\)\) in Unknown on line 0$/m', $out, $m); echo addcslashes("$m[1]: $m[2], $m[3]", "\200..\377"), "\n";
/// ```
/// ```expect process
/// 0
/// PHP Startup: Unable to load dynamic library 'x': /nope/x, /nope/x.so
/// version
/// Failed loading /a/z.so
/// 0
/// Failed loading Zend extension 'y': /z/y, /z/y.so
/// PHP Startup: Unable to load dynamic library '/a/q.so': /a/q.so, /z///a/q.so.so
/// ran
/// Failed loading /a/\344.so
/// 0
/// PHP Startup: Unable to load dynamic library 'x': a/\344/w/x, a/\344/w/x.so
/// ```
fn register_extensions(req: &mut Request, loads: &Loads) -> Vec<u8> {
    const NO_LOADER: &str = "dynamic modules are not supported";
    // php's messages are bytes throughout: a directory or a name that is
    // not UTF-8 is written as it was set.
    let dir = req.ini_get(b"extension_dir").unwrap_or_default().to_vec();
    let sep: &[u8] = if dir.ends_with(b"/") { b"" } else { b"/" };
    let so = crate::constants::SHLIB_SUFFIX.as_bytes();
    let no = NO_LOADER.as_bytes();
    let tried = |first: &[u8], sep: &[u8], name: &[u8]| {
        [
            &b"' (tried: "[..],
            first,
            b" (",
            no,
            b"), ",
            &dir,
            sep,
            name,
            b".",
            so,
            b" (",
            no,
            b"))",
        ]
        .concat()
    };
    let mut stderr = Vec::new();
    for name in &loads.engine {
        if name.starts_with(b"/") {
            stderr.extend_from_slice(&[&b"Failed loading "[..], name, b":  ", no, b"\n"].concat());
            continue;
        }
        let first = [&dir, sep, name].concat();
        let message = [
            &b"Failed loading Zend extension '"[..],
            name,
            &tried(&first, sep, name),
        ]
        .concat();
        if let Ok(string) = crate::api::raising::strpprintf(req, &message) {
            req.note_error_string(Level::CoreWarning, string);
            req.release(Value::string(string));
        }
    }
    for name in &loads.functions {
        let slash = name.contains(&b'/');
        let first = if slash {
            name.clone()
        } else if dir.is_empty() {
            continue;
        } else {
            [&dir, sep, name].concat()
        };
        let sep: &[u8] = if slash { b"/" } else { sep };
        let message = [
            &b"Unable to load dynamic library '"[..],
            name,
            &tried(&first, sep, name),
        ]
        .concat();
        // `php_error_docref`'s origin while modules start.
        let origin = crate::api::raising::Origin::Phase("PHP Startup");
        crate::api::raising::verror(req, Level::CoreWarning, origin, b"", &message);
    }
    stderr
}

/// `zend_disable_function`'s refusal of `exit`, `die` and `clone`: a
/// startup warning for each the list names, which keeps it.
fn disable_warnings(req: &mut Request) {
    let list = req
        .ini_get(b"disable_functions")
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    let names = list.split(|byte| matches!(byte, b' ' | b','));
    for name in names.filter(|name| matches!(*name, b"exit" | b"die" | b"clone")) {
        let message = format!(
            "Cannot disable function {}()",
            String::from_utf8_lossy(name)
        );
        crate::api::raising::warning(req, &message);
    }
}

/// `zend_disable_functions` (`Z/Zend/zend_API.c:3668-3710`): the names
/// the startup ini's `disable_functions` takes out of the function table
/// before any request runs, so a call to one is a call to an undefined
/// function and no lookup, `function_exists()`, `is_callable()` or
/// `get_defined_functions()` finds it. The list is split at every space
/// and every comma and at nothing else, so a tab or a `;` is a byte of a
/// name. Each name is a key of the table, which holds a native under its
/// lowered name, matched byte for byte: `STRLEN` takes nothing out, and a
/// name no native has is passed over. `exit`, `die` and `clone` are the
/// three `zend_disable_function` keeps in the table; php says so with a
/// startup `E_WARNING`, `Cannot disable function %s()` in `Unknown` on
/// line 0 ([`disable_warnings`]).
fn disabled_functions(list: &[u8]) -> Vec<&[u8]> {
    list.split(|byte| matches!(byte, b' ' | b','))
        .filter(|name| !name.is_empty())
        .filter(|name| !matches!(*name, b"exit" | b"die" | b"clone"))
        .collect()
}

/// `CG(function_table)`'s natives once every `MINIT` has registered its
/// own and `php_module_startup` has called `zend_disable_functions` over
/// the startup ini's `disable_functions` (`Z/main/main.c:2370`): the engine's own
/// and then each extension's in the order it was registered,
/// `registered`, its places in start order as [`sort_modules`] answers
/// them, since `zend_register_module_ex` registers an extension's functions
/// as it is handed the extension and `zend_startup_modules` sorts
/// `module_registry` after; the host's `additional`
/// after them under `standard`, less what the list names
/// ([`disabled_functions`]), each beside the extension that declared it.
/// The natives the engine declares for `standard` ([`crate::natives::standard`])
/// are `standard`'s where the bundle has it, at their stub places among
/// its own, and follow Core's under no extension where it does not.
/// The one place the startup's disabled set is decided: what it takes out
/// is in no table the compiler or a request reads. A list a host lays over
/// the started engine takes its natives out of the index instead, since
/// the rows are numbered by then ([`Shared::disable_functions`]).
fn function_table(
    extensions: &[ExtensionEntry],
    declarations: &mut [Declarations],
    registered: &[usize],
    additional: Vec<(FunctionDecl, Entry)>,
    ini: &[(Vec<u8>, Option<Vec<u8>>)],
) -> (Vec<Option<ExtensionId>>, Vec<(FunctionDecl, Entry)>) {
    let list = ini
        .iter()
        .find(|(name, _)| name.as_slice() == b"disable_functions")
        .and_then(|(_, list)| list.as_deref())
        .unwrap_or_default();
    let disabled = disabled_functions(list);
    // `module_registry` numbers Core 0 and the bundle from 1.
    let standard = extensions
        .iter()
        .zip(1u32..)
        .find(|(e, _)| e.name == "standard")
        .map(|(_, at)| ExtensionId(at));
    let mut engine_standard = Some(crate::natives::standard());
    let mut take = |at: usize| {
        declarations
            .get_mut(at)
            .map(|declared| std::mem::take(&mut declared.functions))
            .unwrap_or_default()
    };
    let mut table: Vec<(Option<ExtensionId>, (FunctionDecl, Entry))> = take(0)
        .into_iter()
        .map(|native| (Some(ExtensionId::CORE), native))
        .collect();
    if standard.is_none() {
        table.extend(
            engine_standard
                .take()
                .into_iter()
                .flatten()
                .map(|(_, native)| (None, native)),
        );
    }
    // Each extension in registration order, beside its number in
    // `module_registry` once sorted: its place in start order plus one.
    let by_registration = registered
        .iter()
        .filter_map(|&place| Some((place + 1, u32::try_from(place + 1).ok()?)));
    for (place, at) in by_registration {
        let id = Some(ExtensionId(at));
        let own = take(place).into_iter();
        match engine_standard.take_if(|_| Some(ExtensionId(at)) == standard) {
            Some(engine) => {
                table.extend(
                    at_stub_places(engine, own)
                        .into_iter()
                        .map(|native| (id, native)),
                );
            }
            None => table.extend(own.map(|native| (id, native))),
        }
    }
    // A host's natives go under `standard` once every module has
    // started (`Z/main/main.c:2359-2366`).
    table.extend(additional.into_iter().map(|native| (standard, native)));
    table
        .into_iter()
        .filter(|(_, (decl, _))| !disabled.contains(&decl.name.0.to_ascii_lowercase().as_slice()))
        .unzip()
}

/// `ext/standard`'s natives in its own order with the engine's
/// declared for it spliced in at their [`StubPlace`]s: each after the
/// function it names, whichever side declared that one, and at the end
/// where the stub's neighbour is not in the list.
fn at_stub_places(
    engine: Vec<(StubPlace, (FunctionDecl, Entry))>,
    own: impl Iterator<Item = (FunctionDecl, Entry)>,
) -> Vec<(FunctionDecl, Entry)> {
    let mut waiting = engine;
    let mut placed: Vec<(FunctionDecl, Entry)> = Vec::new();
    let settle = |waiting: &mut Vec<(StubPlace, (FunctionDecl, Entry))>,
                  placed: &mut Vec<(FunctionDecl, Entry)>| {
        while let Some(at) = waiting
            .iter()
            .position(|(place, _)| place.follows(placed.last().map(|(decl, _)| decl)))
        {
            let (_, native) = waiting.remove(at);
            placed.push(native);
        }
    };
    settle(&mut waiting, &mut placed);
    for native in own {
        placed.push(native);
        settle(&mut waiting, &mut placed);
    }
    placed.extend(waiting.into_iter().map(|(_, native)| native));
    placed
}

/// `phar_intercept_functions_init` and opcache's
/// `zend_accel_override_file_functions` for every extension, in the
/// order php installs them ([`Phase`]): each native an extension
/// intercepts has its entry replaced, and the entry it replaced goes in
/// front of its row of what this answers, so a later interception
/// stands over an earlier one, as opcache's handler is saved over
/// phar's.
fn intercept(
    functions: &[FunctionDecl],
    entries: &mut [Entry],
    intercepts: &Keyed<Name, Entry>,
) -> Vec<Vec<Entry>> {
    let mut replaced = vec![Vec::new(); functions.len()];
    for (name, entry) in intercepts.iter() {
        let found = functions
            .iter()
            .position(|decl| decl.name.0.eq_ignore_ascii_case(&name.0));
        if let Some(at) = found
            && let Some(row) = entries.get_mut(at)
            && let Some(chain) = replaced.get_mut(at)
        {
            chain.insert(0, std::mem::replace(row, *entry));
        }
    }
    replaced
}

/// Builds an [`Engine`]: the bundle, the tier and the startup ini.
pub struct EngineBuilder {
    extensions: Vec<Extension>,
    ini_defaults: Vec<(Vec<u8>, Vec<u8>)>,
    ini_files: crate::api::request::IniFiles,
    ini_entries: Vec<u8>,
    /// The tier's loop, bound as the engine starts to the opcodes its
    /// bundle declared ([`Loops::bound`] for the tier).
    bind_loops: BindLoops,
    tier_extension: Option<Extension>,
    hooked: php_compile::Hooked,
    extended_info: bool,
    sapi_name: &'static str,
    /// Where the engine end writes ([`EngineBuilder::ends_into`]).
    ends_into: Option<Box<dyn OutputSink + Send>>,
    additional_extension: Option<Extension>,
    additional_functions: Vec<(FunctionDecl, Entry)>,
    shebang: php_compile::Shebang,
    shebang_while: Option<&'static str>,
    cli_shell: bool,
    configure: &'static [&'static str],
    trace_to: Option<std::path::PathBuf>,
    #[cfg(debug_assertions)]
    sense_leaks: LeakSensor,
}

/// A debug build's request leak sensor (#1992, #1993), as a host sets it
/// for every request of its engine ([`EngineBuilder::sense_leaks`]):
/// armed unless the engine is built with it off, as a debug php reports
/// its leaks unless told not to.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LeakSensor {
    /// Unarmed: a debug request ends as a release one does.
    Off,
    /// Armed, the default: each request ends in php's debug shutdown,
    /// every block of the request's lifetime freed by name, and in the
    /// `InternalError` naming what the heap still held and the extension
    /// whose native or lifecycle hook allocated the first of it; silent
    /// after a fatal.
    #[default]
    Armed,
    /// Armed, and every allocation keeps the backtrace that made it, so
    /// the `InternalError` names the frames that allocated the first
    /// block too: for digging locally, since each allocation then pays a
    /// backtrace's capture, which costs many times the allocation.
    Traced,
}

#[cfg(debug_assertions)]
impl LeakSensor {
    /// Whether a request ends in php's debug shutdown and its walk.
    pub(crate) fn armed(self) -> bool {
        self != LeakSensor::Off
    }
}

impl EngineBuilder {
    /// A debug build's request leak sensor, for every request of the
    /// engine ([`LeakSensor`]); armed unless this sets it otherwise, so
    /// every test that builds an engine and every request a debug host
    /// serves is a leak test. The hosts turn it off from
    /// `PHP_SENSE_LEAKS=0` and trace allocations under it from
    /// `PHP_SENSE_LEAKS_BACKTRACE=1`, read once as they build their
    /// engine (`php_sapi::prepare`).
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn sense_leaks(mut self, sensor: LeakSensor) -> EngineBuilder {
        self.sense_leaks = sensor;
        self
    }

    /// Where the engine end (ADR-0010) writes: what an extension's
    /// engine-end function raises, displayed to `sink`'s body and logged
    /// through its log, as `php_module_shutdown` serves `MSHUTDOWN` with
    /// `php_output` not activated, so php displays through `php_output_direct`
    /// to the process's stdout and logs through `sapi_module.log_message`.
    /// The streams are the host's, so the host leaves them; an engine left
    /// none ends with what it writes reaching no host.
    #[must_use]
    pub fn ends_into(mut self, sink: Box<dyn OutputSink + Send>) -> EngineBuilder {
        self.ends_into = Some(sink);
        self
    }

    /// Every request of the engine records the natives it enters and
    /// appends them to `path` as it ends, one line each,
    /// [`Entered::line`](crate::trace::Entered::line): what a host's
    /// binary does where the harness names a file in `PHP_WITNESS_TRACE`, so a fence served by the binary is covered by what it
    /// entered as one served by the linked engine is (harness §4.5
    /// condition 2, revisions 58 and 61). A forked child's request
    /// appends its own.
    #[must_use]
    pub fn trace_natives_to(mut self, path: std::path::PathBuf) -> EngineBuilder {
        self.trace_to = Some(path);
        self
    }

    /// The `configure` switches that build the bundle the host hands the
    /// engine, php's spelling one each (`--enable-mbstring`), which
    /// [`BuildDefs::configure`] renders as `CONFIGURE_COMMAND`; none
    /// until a host names them.
    #[must_use]
    pub fn configure(mut self, switches: &'static [&'static str]) -> EngineBuilder {
        self.configure = switches;
        self
    }

    /// The extensions in the bundle's order, `enabled()`, which the
    /// engine starts in as `zend_sort_modules` leaves it
    /// ([`sort_modules`]). The engine moves what each declares into its
    /// own tables, so a host hands them over and keeps no copy; one
    /// handed by reference is cloned first.
    #[must_use]
    pub fn extensions<E: Into<Extension>>(
        mut self,
        extensions: impl IntoIterator<Item = E>,
    ) -> EngineBuilder {
        self.extensions
            .extend(extensions.into_iter().map(Into::into));
        self
    }

    /// One ini entry as the host's `-d name=value` sets it: the line
    /// `php_ini_builder_define` writes into the host's `ini_entries`
    /// ([`define_line`]), which [`EngineBuilder::build`] reads with the
    /// one INI grammar as `php_init_config` does, so a `;` starts a
    /// comment, quotes are stripped, a bare value is trimmed, a Zend
    /// constant and a `${…}` are read and `on`, `yes` and `true` are `1`
    /// where `off`, `no`, `false`, `none` and `null` are empty. The
    /// oracle's recipe comes this way into the engine that serves the
    /// witnesses, `fatal_error_backtraces=Off`, `docref_ext=.html` and
    /// `output_handler=` among it:
    ///
    /// ```php
    /// var_dump(ini_get('fatal_error_backtraces'), ini_get('docref_ext'), ini_get('output_handler'), ini_get('date.timezone'));
    /// ```
    /// ```expect
    /// string(0) ""
    /// string(5) ".html"
    /// string(0) ""
    /// string(3) "UTC"
    /// ```
    #[must_use]
    pub fn ini(mut self, name: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> EngineBuilder {
        define_line(&mut self.ini_entries, name.as_ref(), value.as_ref());
        self
    }

    /// One of `sapi_module.ini_defaults`, set in the configuration table
    /// before any php.ini is read, so a file's value of `name` wins over it.
    #[must_use]
    pub fn ini_default(mut self, name: &str, value: &str) -> EngineBuilder {
        let row = (name.as_bytes().to_vec(), value.as_bytes().to_vec());
        self.ini_defaults.push(row);
        self
    }

    /// The php.ini files a host's search found, read after the defaults
    /// and before the `ini_entries` as `php_init_config` reads them; a
    /// builder handed none reads no file, as under `-n`.
    #[must_use]
    pub fn ini_files(mut self, files: crate::api::request::IniFiles) -> EngineBuilder {
        self.ini_files = files;
        self
    }

    /// `sapi_module.name` as `php_module_startup` finds it: the name the
    /// request laid for startup carries, which an entry's `on_modify`
    /// reads as opcache's does; empty until a host names it.
    #[must_use]
    pub fn sapi_name(mut self, name: &'static str) -> EngineBuilder {
        self.sapi_name = name;
        self
    }

    /// The extension a host built itself and hands `php_module_startup`
    /// as its `additional_module`, as php-cgi hands `cgi_module_entry`
    /// (`Z/sapi/cgi/cgi_main.c:1564-1712`) and phpdbg
    /// `sapi_phpdbg_module_entry` (`Z/sapi/phpdbg/phpdbg.c:682-697`): its
    /// natives, constants and ini entries, started after the bundle's as
    /// `zend_sort_modules` leaves them, its name last in
    /// `get_loaded_extensions()`, for a host that declares natives of its
    /// own (ADR-0008, #1348).
    #[must_use]
    pub fn additional_extension(mut self, extension: Extension) -> EngineBuilder {
        self.additional_extension = Some(extension);
        self
    }

    /// The natives a host declared itself and names in
    /// `sapi_module.additional_functions`, as php's CLI names `dl()` and
    /// the process title's two (`Z/sapi/cli/php_cli.c:449-453`):
    /// registered under `standard` once every extension has started, in
    /// the order given (`Z/main/main.c:2359-2366`), and under no
    /// extension where the bundle has no `standard`.
    #[must_use]
    pub fn additional_functions(mut self, natives: Vec<(FunctionDecl, Entry)>) -> EngineBuilder {
        self.additional_functions.extend(natives);
        self
    }

    /// `CG(skip_shebang)` for every request: a file's `#!` first line is
    /// passed over, primary or included, as php's CLI, CGI and built-in
    /// server set it once a request has started
    /// (`Z/sapi/cli/php_cli.c:925`, `Z/sapi/cgi/cgi_main.c:2524`).
    #[must_use]
    pub fn skip_shebang(mut self) -> EngineBuilder {
        self.shebang = php_compile::Shebang::Skipped;
        self
    }

    /// [`EngineBuilder::skip_shebang`] where the entry `name` reads on once
    /// startup has read the ini, as php-cgi sets `CG(skip_shebang)` from
    /// `cgi.check_shebang_line` after `php_module_startup`
    /// (`Z/sapi/cgi/cgi_main.c:2523`).
    #[must_use]
    pub fn skip_shebang_while(mut self, name: &'static str) -> EngineBuilder {
        self.shebang_while = Some(name);
        self
    }

    /// The host carries php's table of shell callbacks, the CLI's
    /// `php_cli_get_shell_callbacks` (`Z/sapi/cli/php_cli.c:110`), which
    /// an extension's [`Shell`](crate::api::abi::Shell) fills: every body
    /// write is shown to the shell's `write` before it reaches the sink,
    /// and [`Engine::has_shell`] answers whether `-a` has a shell to
    /// serve ([`CommandLine::Shell`](crate::CommandLine::Shell)).
    #[must_use]
    pub fn cli_shell(mut self) -> EngineBuilder {
        self.cli_shell = true;
        self
    }

    /// Compile every unit with extended info, as php's `-e` sets
    /// `ZEND_COMPILE_EXTENDED_INFO` in `CG(compiler_options)` before the
    /// first file compiles (`Z/sapi/cli/php_cli.c:1366`): an `EXT_STMT`
    /// before each statement and an `EXT_FCALL_BEGIN` and
    /// `EXT_FCALL_END` around each call, an `include` and an `eval`
    /// ([`php_compile::Options::extended_info`]). It is the engine's,
    /// beside the tier's hooks: every request compiles under it, what a
    /// host evaluates as a string of its own excepted, and the file
    /// cache keeps such units apart from a plain engine's. `on` is what
    /// the host's line said; off is php's default.
    #[must_use]
    pub fn extended_info(mut self, on: bool) -> EngineBuilder {
        self.extended_info = on;
        self
    }

    /// Bind the tier: `execute::<T>` becomes the loop, its instantiation
    /// chosen as the engine starts by whether the bundle declares an
    /// opcode (design §5.8, §7.1), and the tier's state is declared
    /// under seam 3 (design §4.3 seam 8).
    #[must_use]
    pub fn tier<T: Tier>(mut self) -> EngineBuilder {
        self.bind_loops = Loops::bound::<T>;
        self.tier_extension = T::extension();
        self.hooked = T::HOOKED;
        self
    }

    /// The engine: every engine state built from the startup ini, in
    /// bundle order (ADR-0005), the declarations linked. A name the host
    /// set that neither the core table nor an extension declares is no
    /// entry, as the recipe's `track_errors`, gone since php 8, is not:
    ///
    /// ```php
    /// var_dump(ini_get('track_errors'), get_cfg_var('track_errors'), ini_set('track_errors', '1'), isset(ini_get_all()['track_errors']));
    /// ```
    /// ```expect
    /// bool(false)
    /// string(1) "0"
    /// bool(false)
    /// bool(false)
    /// ```
    #[must_use]
    #[allow(clippy::too_many_lines)] // one line per field of the engine
    pub fn build(self) -> Engine {
        let mut ini: Vec<(Vec<u8>, Option<Vec<u8>>)> = CORE_INI
            .iter()
            .map(|(n, v)| (n.as_bytes().to_vec(), v.map(|v| v.as_bytes().to_vec())))
            .collect();
        let mut extensions = self.extensions;
        extensions.extend(self.tier_extension);
        let additional = self.additional_functions;
        extensions.extend(self.additional_extension);
        let mut registered = sort_modules(&mut extensions);
        let unloaded = unload_unmet(&mut extensions, &mut registered);
        // Each extension's supplies taken out of it, by its number in
        // `module_registry`, which numbers Core 0 and the bundle from 1,
        // and installed by the phase it starts in.
        let mut suppliers = Suppliers::take((1..).map(ExtensionId::at).zip(&mut extensions));
        if !self.cli_shell {
            suppliers = suppliers.without_shell();
        }
        let supplied = suppliers.resolved();
        // What a `MINIT` registers into another extension's tables, as
        // `php_image_register_handler` adds `IMAGETYPE_SVG` to
        // `standard`'s constants: each extension's constants rewritten
        // once with every value the bundle registered.
        for extension in &mut extensions {
            let declared = &mut extension.declarations;
            if let Some(link) = declared.constants_link.take() {
                declared.constants = link(
                    std::mem::take(&mut declared.constants),
                    &supplied.registered,
                );
            }
        }
        // The declarations apart from the rest of each entry, by
        // `ExtensionId`: Core's at 0, as `module_registry` numbers it.
        let Extension {
            entry: core,
            declarations: core_declared,
        } = crate::core::extension();
        let (mut extensions, declared): (Vec<ExtensionEntry>, Vec<Declarations>) = extensions
            .into_iter()
            .map(|extension| (extension.entry, extension.declarations))
            .unzip();
        let mut declarations = Vec::with_capacity(declared.len() + 1);
        declarations.push(core_declared);
        declarations.extend(declared);
        for extension in &extensions {
            for entry in &extension.ini {
                ini.push((
                    entry.name.0.to_vec(),
                    entry.default.as_deref().map(<[u8]>::to_vec),
                ));
            }
        }
        let mut ini_files = self.ini_files;
        let configuration = Configuration::read(
            &self.ini_defaults,
            &mut ini_files,
            &self.ini_entries,
            &declarations[0].constants,
        );
        // An entry with an `on_modify` keeps its default until the
        // handler has taken the value the host set ([`Engine::register_ini`]).
        let mut held = Vec::new();
        for (name, value) in configuration.directives() {
            if on_modify(&extensions, &name).is_some() {
                held.push((name, value));
                continue;
            }
            // A name no table declares is no entry: it stays in the
            // configuration table, where `get_cfg_var()` finds it, as
            // `zend_register_ini_entries_ex` reads that table by the
            // names it registers and by no other.
            // `zend_register_ini_entries_ex` takes the value only where
            // the entry's `on_modify` does, and the default stays.
            if !ini.iter().any(|(n, _)| *n == name)
                || Request::ini_verdict(&name, &value) == crate::api::abi::IniVerdict::Rejected
            {
                continue;
            }
            ini.retain(|(n, _)| *n != name);
            ini.push((name, Some(value)));
        }
        let on = |name: &str| {
            ini.iter()
                .find(|(n, _)| n.as_slice() == name.as_bytes())
                .and_then(|(_, value)| value.as_deref())
                .is_some_and(Request::ini_parse_bool)
        };
        let shebang = match self.shebang_while {
            Some(name) if on(name) => php_compile::Shebang::Skipped,
            _ => self.shebang,
        };
        let (function_modules, natives): (_, Vec<(FunctionDecl, Entry)>) = function_table(
            &extensions,
            &mut declarations,
            &registered,
            additional,
            &ini,
        );
        let (functions, mut entries): (Vec<FunctionDecl>, Vec<Entry>) = natives.into_iter().unzip();
        let replaced = intercept(&functions, &mut entries, &supplied.intercepts);
        let function_flags = functions
            .iter()
            .map(|decl| crate::attributes::Flags::of(&decl.attributes))
            .collect();
        let frameless = frameless_table(functions.iter());
        let function_names = Names::over(&functions);
        let LinkedClasses {
            classes,
            modules: class_modules,
            handlers,
            natives,
            methods,
            trampolines,
        } = link_classes(&mut declarations);
        let class_keys = ClassKeys::over(&classes);
        let class_at = |name: &[u8]| class_keys.find(&classes, name);
        let records = native_records(&classes, &class_keys);
        let method_functions = method_functions(&classes, &methods, functions.len());
        let trampoline_tables =
            trampoline_tables(classes.len(), &methods, &trampolines, functions.len());
        let parents: Vec<Option<ClassId>> = classes
            .iter()
            .map(|decl| class_at(&decl.parent.as_ref()?.0))
            .collect();
        let placed = native_placed(&classes, &records, &parents);
        let parameter_classes =
            parameter_classes(&functions, &methods, &classes, &trampolines, class_at);
        // `module_registry`: `Core` first, as `zend_startup` registers it,
        // so Zend's constants stand before the bundle's, as
        // `zend_register_standard_constants` runs before any `MINIT`.
        extensions.insert(0, core);
        let (constant_modules, constants): (Vec<ExtensionId>, Vec<ConstDecl>) =
            (declarations.into_iter().enumerate())
                .flat_map(|(at, declared)| {
                    (declared.constants.into_iter()).map(move |decl| (ExtensionId::at(at), decl))
                })
                .unzip();
        let mut names = permanent_names(&extensions, &functions, &classes, &constants, &ini);
        let constant_values = constant_values(&constants, &mut names);
        let constant_names = Names::over(&constants);
        let default_strings = default_strings(&functions, &classes);
        let mut strings = Strings::new();
        let sapi_names = crate::constants::host_names(&mut strings);
        let units = UnitCache::new(self.hooked, self.extended_info, &functions, &classes);
        let arena = crate::arena::Epochs::of(crate::arena::Arena::past(
            classes.len(),
            functions.len().saturating_add(methods.len()),
            crate::unit_cache::Files::new(MAX_ACCELERATED_FILES),
        ));
        let layer = supplied.stream_hooks.get().map(|hooks| (hooks.state)());
        let mut engine = Engine {
            shared: Arc::new(Shared {
                extensions,
                functions,
                function_names,
                entries,
                replaced,
                function_modules,
                function_flags,
                frameless,
                classes,
                class_keys,
                class_modules,
                handlers,
                natives,
                methods,
                trampolines,
                parameter_classes,
                records,
                method_functions,
                trampoline_tables,
                parents,
                placed,
                constants,
                constant_names,
                constant_modules,
                engine_states: States::new(),
                state_slots: crate::api::request::StateSlots::default(),
                typed_ini: TypedIni::of(|name| master_of(&ini, name)),
                ini,
                constant_values,
                sapi_names,
                build_defs: BuildDefs::of(self.configure),
                shebang,
                hooked: self.hooked,
                extended_info: self.extended_info,
                observers: supplied.observers.count(),
                opcodes: crate::observer::ObservedOpcodes::default(),
                supplied,
                startup_errors: configuration.stderr,
                startup_raised: Vec::new(),
                startup_shown: AtomicBool::new(false),
                refused: None,
                host: self.sapi_name,
                configuration: configuration.table,
                sections: configuration.sections,
                ini_files,
                trace_to: self.trace_to,
                #[cfg(debug_assertions)]
                sense_leaks: self.sense_leaks,
                // No opcode is declared until the extensions have started.
                loops: (self.bind_loops)(&crate::observer::ObservedOpcodes::default()),
                locked: Vec::new(),
                script_encodings: Vec::new(),
                units,
                arena,
                layer,
                timer: crate::interrupt::Timer::default(),
                strings: Mutex::new(strings),
                names,
                default_strings,
                map_ptr_last: AtomicU32::new(0),
                started: OnceLock::new(),
                starting: Mutex::new(()),
            }),
            end: Arc::default(),
        };
        engine.register_ini(
            held,
            Registry {
                loads: &configuration.loads,
                unloaded,
                suppliers,
            },
            self.bind_loops,
        );
        engine.end.arm(&engine.shared, self.ends_into);
        engine
    }
}

/// `ZEND_KNOWN_STRINGS` (`Z/Zend/zend_string.h`): what
/// `zend_interned_strings_init` interns before any extension starts, the
/// keys of a backtrace's rows and of a `Throwable`'s properties, `->` and
/// `::` among them, which `ZSTR_KNOWN` hands out with no allocation.
const KNOWN_STRINGS: [&str; 88] = [
    "file",
    "line",
    "function",
    "class",
    "object",
    "type",
    "->",
    "::",
    "args",
    "unknown",
    "Unknown",
    "exit",
    "clone",
    "eval",
    "include",
    "require",
    "include_once",
    "require_once",
    "scalar",
    "error_reporting",
    "static",
    "this",
    "value",
    "key",
    "__invoke",
    "previous",
    "code",
    "message",
    "severity",
    "string",
    "trace",
    "scheme",
    "host",
    "port",
    "user",
    "username",
    "pass",
    "password",
    "path",
    "query",
    "fragment",
    "NULL",
    "boolean",
    "integer",
    "double",
    "array",
    "resource",
    "resource (closed)",
    "name",
    "argv",
    "argc",
    "Array",
    "bool",
    "int",
    "float",
    "callable",
    "iterable",
    "void",
    "never",
    "false",
    "true",
    "null",
    "mixed",
    "Traversable",
    "self",
    "parent",
    "__sleep",
    "__wakeup",
    "cases",
    "from",
    "tryFrom",
    "tryfrom",
    "_SERVER",
    "_ENV",
    "_REQUEST",
    "count",
    "SensitiveParameter",
    "[constant expression]",
    "Deprecated",
    "since",
    "get",
    "set",
    "8.0",
    "8.1",
    "8.2",
    "8.3",
    "8.4",
    "8.5",
];

/// The strings the bundle's natives default a parameter to,
/// made once for [`Shared::default_string`].
fn default_strings(functions: &[FunctionDecl], classes: &[ClassDecl]) -> Strings {
    let mut strings = Strings::new();
    let declared = functions.iter().chain(
        classes
            .iter()
            .flat_map(|decl| decl.methods.iter().map(|method| &method.function)),
    );
    for function in declared {
        for parameter in &function.parameters {
            if let ParameterDefault::Expr(ConstExpr::Str(written)) = &parameter.default {
                strings.intern(written).ok();
            }
        }
    }
    strings
}

/// `interned_strings_permanent` as `MINIT` leaves it: what php interns
/// permanently as the engine and the bundle register their declarations.
/// `zend_register_module_ex` interns each extension's lowercased name;
/// `zend_register_functions` a function's and a method's name as
/// declared and lowercased, the function table's key;
/// `zend_register_internal_class_ex` a class's name and its lowercased
/// key, and `zend_register_class_alias_ex` an alias's key;
/// `zend_declare_typed_property` a property's name and
/// `zend_declare_typed_class_constant` a class constant's name and string
/// value, an enum case's among them; `zend_register_constant` a
/// constant's name and string value; and `zend_register_ini_entries_ex`
/// each entry's name and the value it starts with. A name the table
/// refuses is left to the request that asks for it.
///
/// The bundle's constants' values are interned here, and their zvals,
/// built over this table, are the second half of what this answers.
fn permanent_names(
    extensions: &[ExtensionEntry],
    functions: &[FunctionDecl],
    classes: &[ClassDecl],
    constants: &[ConstDecl],
    ini: &[(Vec<u8>, Option<Vec<u8>>)],
) -> Strings {
    let mut names = Strings::new();
    for known in KNOWN_STRINGS {
        names.intern(known.as_bytes()).ok();
    }
    for extension in extensions {
        names.intern(&lowered(extension.name.as_bytes())).ok();
    }
    let declared = functions
        .iter()
        .map(|decl| &decl.name)
        .chain(classes.iter().flat_map(|decl| {
            std::iter::once(&decl.name)
                .chain(decl.methods.iter().map(|method| &method.function.name))
                .chain(decl.properties.iter().map(|property| &property.name))
                .chain(decl.constants.iter().map(|constant| &constant.name))
        }))
        .chain(constants.iter().map(|decl| &decl.name));
    for name in declared {
        names.intern(&name.0).ok();
    }
    // `prop_info->name`, the key a protected or a private property's
    // row is held under, mangled and interned as it is declared.
    for decl in classes {
        for property in &decl.properties {
            if let Some(prefix) = php_compile::mangling(property.visibility, &decl.name.0) {
                names
                    .intern(&[prefix.as_slice(), &property.name.0].concat())
                    .ok();
            }
        }
    }
    // `php_startup_auto_globals` and the session's `MINIT` register each
    // auto-global under `zend_string_init_interned`.
    {
        use php_unit::AutoGlobal::{Cookie, Env, Files, Get, Post, Request, Server, Session};
        for global in [Get, Post, Cookie, Server, Env, Request, Files, Session] {
            names.intern(global.name()).ok();
        }
    }
    let keyed = functions
        .iter()
        .map(|decl| &decl.name)
        .chain(classes.iter().flat_map(|decl| {
            std::iter::once(&decl.name)
                .chain(&decl.aliases)
                .chain(decl.methods.iter().map(|method| &method.function.name))
        }));
    for name in keyed {
        names.intern(&lowered(&name.0)).ok();
    }
    for decl in classes {
        for constant in &decl.constants {
            if let ConstExpr::Str(value) = &constant.value {
                names.intern(value).ok();
            }
        }
    }
    for (name, value) in ini {
        names.intern(name).ok();
        if let Some(value) = value {
            names.intern(value).ok();
        }
    }
    // `gen_stub.php`'s attribute arguments, `zend_string_init_interned`
    // of each string a native's `#[\Deprecated]` is written with, so a
    // deprecation's suffix is read and nothing is made, a global
    // constant's among them (`zend_add_global_constant_attribute`).
    let attributed = functions
        .iter()
        .chain(
            classes
                .iter()
                .flat_map(|decl| decl.methods.iter().map(|m| &m.function)),
        )
        .flat_map(|decl| &decl.attributes)
        .chain(
            classes
                .iter()
                .flat_map(|decl| decl.constants.iter().flat_map(|c| &c.attributes)),
        )
        .chain(constants.iter().flat_map(|decl| &decl.attributes));
    for argument in attributed.flat_map(|attribute| &attribute.arguments) {
        if let ConstExpr::Str(written) = &argument.value {
            names.intern(written).ok();
        }
    }
    names
}

/// `zend_register_constant` at `MINIT`: each constant's value, a string
/// constant's interned into the permanent table once, before any request,
/// so reading it later is a pointer and no allocation. A constant whose
/// value is an expression, or a string the table refuses, has none.
fn constant_values(
    constants: &[ConstDecl],
    strings: &mut Strings,
) -> Vec<Option<(Zval, crate::attributes::Flags)>> {
    let value = |decl: &ConstDecl, strings: &mut Strings| match &decl.value {
        ConstExpr::Null => Some(Zval::null()),
        ConstExpr::Bool(b) => Some(Zval::bool(*b)),
        ConstExpr::Long { value: n, .. } => Some(Zval::long(*n)),
        ConstExpr::Double(f) => Some(Zval::double(*f)),
        ConstExpr::Str(s) => strings.intern(s).ok().map(Zval::string),
        _ => None,
    };
    (constants.iter())
        .map(|decl| {
            let flags = crate::attributes::Flags::of(&decl.attributes);
            value(decl, strings).map(|z| (z, flags))
        })
        .collect()
}

/// `zend_sort_modules` (`Z/Zend/zend_API.c:2483-2515`), which
/// `zend_startup_modules` runs over `module_registry` before the first
/// `MINIT`: walking from the first, an extension that names a required
/// or optional dependency standing after it trades places with that one,
/// and the walk stays where it is until the extension there names none
/// that stands later. The order the engine starts, activates and lists
/// its extensions in is the bundle's as this leaves it, so `standard`,
/// which names `session` optional (`standard_deps`), starts after it
/// whatever the crates depend on:
///
/// ```php
/// $loaded = get_loaded_extensions();
/// var_dump(array_search('session', $loaded) < array_search('standard', $loaded), array_search('libxml', $loaded) < array_search('dom', $loaded));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// What it answers is the order the modules were registered in, each
/// extension's place in start order listed in the order it was handed:
/// the order `zend_register_module_ex` filled the function table in,
/// before the sort ([`function_table`]).
///
/// php loops for ever over a cycle of dependencies; the walk here stops
/// after as many trades as a list of its length can need without one.
fn sort_modules(modules: &mut [Extension]) -> Vec<usize> {
    let mut handed: Vec<usize> = (0..modules.len()).collect();
    let mut trades = modules.len() * modules.len();
    let mut at = 0;
    while at < modules.len() {
        let later = modules[at]
            .entry
            .dependencies
            .iter()
            .filter(|dep| dep.kind != DependencyKind::Conflicts)
            .find_map(|dep| {
                (at + 1..modules.len())
                    .find(|&other| modules[other].entry.name.eq_ignore_ascii_case(&dep.name))
            });
        match later {
            Some(other) if trades > 0 => {
                modules.swap(at, other);
                handed.swap(at, other);
                trades -= 1;
            }
            _ => at += 1,
        }
    }
    // `handed[place]` is where the extension now at `place` was handed;
    // ordering the places by it lists them in registration order.
    let mut registered: Vec<usize> = (0..modules.len()).collect();
    registered.sort_by_key(|&place| handed[place]);
    registered
}

/// `module_registry` as `php_module_startup` meets it beyond what the
/// engine's tables hold: the dynamic libraries the ini asked to load,
/// the extensions refused for a dependency ([`unload_unmet`]), and what
/// every extension kept supplies ([`Suppliers`]).
struct Registry<'a> {
    loads: &'a Loads,
    unloaded: Vec<Unloaded>,
    suppliers: Suppliers,
}

/// An extension `zend_startup_module_ex` refused for a dependency: where
/// in start order its check stood, as the place in `module_registry`
/// (Core 0) of the extension that starts next, and php's warning.
struct Unloaded {
    before: usize,
    message: String,
}

/// `zend_startup_module_ex`'s check (`Z/Zend/zend_API.c:2417-2447`), made
/// over the sorted bundle before any `MINIT`: an extension one of whose
/// `MODULE_DEP_REQUIRED` rows names no extension started before it,
/// compared case-insensitively, is taken out of `modules`, with php's
/// `E_CORE_WARNING` kept for the startup to raise in its place, so an
/// extension requiring it is refused in turn; `registered` is renumbered
/// to the places left. php registers a persistent extension's functions
/// before the check and keeps them once it fails; here nothing of the
/// refused extension stays, since none of it was started.
fn unload_unmet(modules: &mut Vec<Extension>, registered: &mut Vec<usize>) -> Vec<Unloaded> {
    let mut started: Vec<usize> = Vec::with_capacity(modules.len());
    let mut unloaded = Vec::new();
    for (at, module) in modules.iter().enumerate() {
        let unmet = (module.entry.dependencies.iter())
            .filter(|dep| dep.kind == DependencyKind::Required)
            .find(|dep| {
                !dep.name.eq_ignore_ascii_case("Core")
                    && !(started.iter())
                        .any(|&s| modules[s].entry.name.eq_ignore_ascii_case(&dep.name))
            });
        match unmet {
            Some(dep) => unloaded.push(Unloaded {
                before: started.len() + 1,
                message: format!(
                    "Cannot load module \"{}\" because required module \"{}\" is not loaded",
                    module.entry.name, dep.name
                ),
            }),
            None => started.push(at),
        }
    }
    if unloaded.is_empty() {
        return unloaded;
    }
    let place = |old: usize| started.binary_search(&old).ok();
    registered.retain_mut(|old| place(*old).inspect(|new| *old = *new).is_some());
    *modules = (std::mem::take(modules).into_iter().enumerate())
        .filter_map(|(at, module)| place(at).map(|_| module))
        .collect();
    unloaded
}

/// One extension's `MINIT` (ADR-0010): its process start, called once
/// for the process and its outcome kept ([`crate::process_scope`]), its
/// process state then handed to `start` for this engine start and every
/// later one, and its engine start, whose state joins `states`.
fn start_module(
    extension: &ExtensionEntry,
    start: &mut Start,
    states: &mut States<Box<dyn Any + Send + Sync>>,
) -> Result<(), Refusal> {
    if let Some(process) = crate::process_scope::reach(extension).map_err(Refusal::clone)? {
        start.process.push(process);
    }
    if let Some(build) = &extension.engine_state {
        states.push(build(start)?);
    }
    Ok(())
}

/// What the startup request meets as `zend_startup_modules` walks the
/// bundle, before any engine start: each value the host set for an entry
/// with an `on_modify` handed to it in `held`'s order, those it took
/// added to `taken`, and each extension refused for a dependency raising as its
/// turn comes ([`unload_unmet`]). The marks say where each extension's
/// raising starts, by its place in `module_registry`: a refused start
/// shows what php raised before it, which is nothing of a later
/// extension's (`Engine::refused`).
fn modules_startup(
    req: &mut Request,
    extensions: &[ExtensionEntry],
    held: Vec<(Vec<u8>, Vec<u8>)>,
    unloaded: Vec<Unloaded>,
    taken: &mut Vec<(Vec<u8>, Option<Vec<u8>>)>,
) -> Vec<(usize, usize)> {
    let mut marks = Vec::new();
    let mut unloaded = unloaded.into_iter().peekable();
    let mut refuse_until = |req: &mut Request, marks: &mut Vec<(usize, usize)>, owner| {
        while let Some(refused) = unloaded.next_if(|refused| refused.before <= owner) {
            marks.push((refused.before, req.startup_raised_len()));
            crate::api::raising::raise(req, Level::CoreWarning, &refused.message);
        }
    };
    for (name, value) in held {
        let owner = (extensions.iter())
            .position(|e| e.on_modify.iter().any(|(n, _)| *n == name))
            .unwrap_or(usize::MAX);
        refuse_until(req, &mut marks, owner);
        marks.push((owner, req.startup_raised_len()));
        let accepted = on_modify(extensions, &name)
            .is_some_and(|handler| matches!(handler(req, Some(&value)), Ok(true)));
        if accepted {
            req.take_startup_value(&name, &value);
            taken.push((name, Some(value)));
        }
    }
    refuse_until(req, &mut marks, usize::MAX);
    marks
}

/// The `on_modify` an extension of `extensions` gave the entry `name`.
fn on_modify(extensions: &[ExtensionEntry], name: &[u8]) -> Option<OnModify> {
    extensions.iter().find_map(|e| {
        e.on_modify
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, handler)| *handler)
    })
}

/// The process-wide engine (design §5.14): immutable once built, shared
/// by every request thread. The drop of its last clone ends it
/// ([`EngineEnd`]).
#[derive(Clone)]
pub struct Engine {
    pub(crate) shared: Arc<Shared>,
    /// Held by every clone, so its drop is the last clone's.
    end: Arc<EngineEnd>,
}

/// Why [`Engine::alter_master_ini`] laid nothing: another clone of the
/// engine stands, and the master values a clone reads are the engine's
/// for as long as it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAlone;

/// The engine end (ADR-0010): php's `php_module_shutdown` as far as an
/// extension sees it, served once, from the drop of the engine's last
/// clone, so no host can forget it and a forked child that drops its
/// engine ends it too, as php's `php_module_shutdown` runs in each
/// php-fpm child, on the engine its master started and the fork copied,
/// which the child keeps with its pool's values laid over
/// ([`Engine::alter_master_ini`]). Armed once the engine has started; an
/// engine whose start was refused is not ended.
#[derive(Default)]
struct EngineEnd {
    shared: OnceLock<Arc<Shared>>,
    /// What the host left for the end to write to
    /// ([`EngineBuilder::ends_into`]), taken once, by the drop.
    sink: Mutex<Option<Box<dyn OutputSink + Send>>>,
}

impl EngineEnd {
    /// From here the drop of the engine's last clone ends `shared`,
    /// writing to `sink` where the host left one.
    fn arm(&self, shared: &Arc<Shared>, sink: Option<Box<dyn OutputSink + Send>>) {
        self.shared.set(Arc::clone(shared)).ok();
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = sink;
    }
}

impl Drop for EngineEnd {
    /// `php_module_shutdown`'s `zend_shutdown` (`Z/Zend/zend.c:1162-1170`):
    /// the shutdown request, the mirror of the startup request, on which
    /// the stream layer's engine end closes its persistent sockets first
    /// ([`StreamHooks::engine_end`](crate::layer::StreamHooks::engine_end)),
    /// as `EG(persistent_list)` is destroyed first; then request states
    /// built in bundle order, each extension's engine-end function in reverse
    /// ([`end_phase`]), the request dropped with its states in reverse;
    /// and then, with the last owner of what every request shares, the
    /// engine states in reverse (`zend_destroy_modules`). What the
    /// request writes, a diagnostic an engine-end function raised among
    /// it, goes to the sink the host left ([`EngineBuilder::ends_into`]),
    /// as php writes it with `php_output` not activated, and reaches no host
    /// where none was left. A start an extension refused, or
    /// the extensions' post-startup refused, ends nothing: a failed
    /// `MINIT` ends php's process from `php_error_cb` (`exit(-2)`), and no
    /// host of php's calls `php_module_shutdown` once
    /// `php_module_startup` failed.
    fn drop(&mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        if shared.refused.is_some()
            || matches!(
                shared.started.get(),
                Some(crate::preload::Started::Refused(_))
            )
        {
            return;
        }
        let description = RequestDescription {
            sapi_name: shared.host.to_owned(),
            ..RequestDescription::default()
        };
        let sink = (self.sink.get_mut())
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut req = match sink {
            Some(sink) => Request::lent(Arc::clone(&shared), description, sink),
            None => Request::new(Arc::clone(&shared), description),
        };
        req.enter_phase(crate::api::request::Phase::EngineEnd);
        if let Some(hooks) = shared.supplied.stream_hooks.get() {
            (hooks.engine_end)(&mut req);
        }
        end_phase(&shared.extensions, &mut req, |e| e.engine_end);
        drop(req.release_sink());
    }
}

impl Engine {
    /// A builder under the CLI's `HARDCODED_INI` (`Z/sapi/cli/php_cli.c:116-121`),
    /// for the tests that hold what php's CLI writes.
    #[cfg(test)]
    pub(crate) fn cli_builder() -> EngineBuilder {
        let hardcoded = [
            ("html_errors", "0"),
            ("implicit_flush", "1"),
            ("output_buffering", "0"),
            ("max_execution_time", "0"),
            ("max_input_time", "-1"),
        ];
        hardcoded
            .into_iter()
            .fold(Engine::builder(), |builder, (name, value)| {
                builder.ini(name, value)
            })
    }

    /// A builder with no extensions, `Nop` as the tier and the core ini.
    #[must_use]
    pub fn builder() -> EngineBuilder {
        EngineBuilder {
            extensions: Vec::new(),
            ini_defaults: Vec::new(),
            ini_files: crate::api::request::IniFiles::default(),
            ini_entries: Vec::new(),
            bind_loops: Loops::bound::<Nop>,
            tier_extension: Nop::extension(),
            hooked: Nop::HOOKED,
            extended_info: false,
            sapi_name: "",
            additional_extension: None,
            additional_functions: Vec::new(),
            shebang: php_compile::Shebang::Read,
            shebang_while: None,
            cli_shell: false,
            configure: &[],
            trace_to: None,
            ends_into: None,
            #[cfg(debug_assertions)]
            sense_leaks: LeakSensor::default(),
        }
    }

    /// The build's facts, php's `main/build-defs.h`: what the host's
    /// `-v` prints and phpinfo's build rows say.
    #[must_use]
    pub fn build_defs(&self) -> &BuildDefs {
        &self.shared.build_defs
    }

    /// `zend_flf_functions` by name, what `Unit::dump` prints a
    /// `FRAMELESS_ICALL_*` with in parentheses: the declared name of the
    /// native each row of the engine's frameless table is, in the
    /// table's order, so the `extended_value` a unit this engine compiled
    /// carries indexes the name php's dump would print.
    #[must_use]
    pub fn frameless(&self) -> Vec<Name> {
        self.shared.frameless_names()
    }

    /// `zend_register_ini_entries_ex` at `ZEND_INI_STAGE_STARTUP`
    /// (`Z/Zend/zend_ini.c:238-257`), then each extension's engine state
    /// as `MINIT` builds its state from the entries: every value the
    /// host set for an entry with an `on_modify` is handed to it, on a
    /// request laid for the stage alone, and taken where it answers
    /// `true`; where it refuses, the entry keeps its default, so
    /// `ini_get()`, the master value and the startup ini the engine
    /// states are built from all read the default. What the handlers
    /// raise is kept for every request to raise first
    /// ([`Engine::transaction`]), a docref as `PHP Startup: …`, in the
    /// order the entries are declared. ext/session's witness under the
    /// pin's `session-startup` regime holds it, with the test below for
    /// a refused value, and `ext/date`'s `date.timezone` is the one the
    /// phpt corpus reads.
    fn register_ini(
        &mut self,
        mut held: Vec<(Vec<u8>, Vec<u8>)>,
        registry: Registry<'_>,
        bind_loops: BindLoops,
    ) {
        // `php_module_startup`'s `zend_reset_lc_ctype_locale`, before any
        // handler or engine state runs: the process's, so the first engine
        // a process builds sets it and every later one finds it set.
        php_process::reset_lc_ctype_locale();
        // `zend_register_ini_entries_ex` walks each module's entries in
        // its `PHP_INI_BEGIN()` order, the modules in startup order, and
        // finds the host's value for each: the handlers meet the values
        // in declaration order, whatever order the host set them in.
        let declared_at = |name: &[u8]| {
            self.shared
                .extensions
                .iter()
                .flat_map(|e| e.ini.iter())
                .position(|entry| entry.name.0.as_ref() == name)
                .unwrap_or(usize::MAX)
        };
        held.sort_by_key(|(name, _)| declared_at(name));
        let sapi_name = self.shared.host;
        let description = RequestDescription {
            sapi_name: sapi_name.to_owned(),
            ..RequestDescription::default()
        };
        let mut req = Request::new(Arc::clone(&self.shared), description);
        req.begin_startup();
        let mut taken = core_startup(&mut req);
        let Registry {
            loads,
            unloaded,
            suppliers,
        } = registry;
        let failed = register_extensions(&mut req, loads);
        let marks = modules_startup(
            &mut req,
            &self.shared.extensions,
            held,
            unloaded,
            &mut taken,
        );
        req.set_ini_stage(IniStage::Startup);
        let encoding_at = req.startup_raised_len();
        let (script_encodings, taken_encoding) = crate::multibyte::startup(&mut req);
        if taken_encoding == crate::multibyte::Startup::Refused {
            taken.push((b"zend.script_encoding".to_vec(), None));
        }
        // What php raises after `zend_startup_modules` follows.
        let modules_end = req.startup_raised_len();
        disable_warnings(&mut req);
        startup_directives(&mut req, &self.shared);
        let raised = req.end_startup();
        let written = req.startup_stderr();
        drop(req);
        let Some(shared) = Arc::get_mut(&mut self.shared) else {
            return;
        };
        // What the startup stage took is the master value and the startup
        // ini alike.
        for (name, value) in taken {
            shared.set_ini_default(name, value);
        }
        shared.startup_errors.extend_from_slice(&failed);
        shared.script_encodings = script_encodings;
        shared.startup_errors.extend_from_slice(&written);
        let mut start = Start {
            ini: StartupIni {
                entries: shared.ini.clone(),
            },
            host: sapi_name,
            process: Vec::new(),
        };
        // `zend_startup_modules`: each `MINIT` in bundle order, its process
        // start (once for the process, ADR-0010) and then its engine
        // start, and the first that fails ends the startup
        // (`zend_startup_module_ex`).
        for (at, extension) in shared.extensions.iter().enumerate() {
            let Err(refusal) = start_module(extension, &mut start, &mut shared.engine_states)
            else {
                continue;
            };
            let cut = marks
                .iter()
                .find(|(owner, _)| *owner > at)
                .map_or(encoding_at, |(_, len)| *len);
            let before = raised.get(..cut).into_iter().flatten();
            let encoding = raised.get(encoding_at..modules_end).into_iter().flatten();
            let mut shown = Refusal::new();
            shown.raised = before.chain(encoding).cloned().collect();
            shown.raised.extend(refusal.raised);
            let failed = format!("Unable to start {} module", extension.name);
            shared.refused = Some(shown.raise(Level::CoreError, failed));
            shared.supplied = suppliers.refused_at(ExtensionId::at(at));
            break;
        }
        shared.state_slots =
            crate::api::request::StateSlots::of(&shared.extensions, &shared.engine_states);
        if shared.refused.is_none() {
            shared.observe_opcodes(&start, bind_loops);
        }
        shared.startup_raised = raised;
    }

    /// Where an extension's engine start refused ([`Refusal`]): what php
    /// displays as its `MINIT` fails, written to `sink` as `php_error_cb`
    /// writes a startup error, past the `php_output` layer and under the
    /// startup ini's `display_errors`, `html_errors` and `log_errors`;
    /// then the status php ends the process with, the `-2` of its
    /// `exit(-2)` (`Z/main/main.c:1505-1507`), the same in every host.
    /// What php raised before the failed start comes first: the host's
    /// values the handlers of the extensions up to it met, never a later
    /// extension's nor what php raises after `zend_startup_modules`.
    /// `None` for an engine that started. Every request a refused engine
    /// is asked for is refused so. `sink` is handed back.
    pub fn refused(
        &self,
        sink: Box<dyn OutputSink + Send>,
    ) -> (Option<i32>, Box<dyn OutputSink + Send>) {
        let Some(refused) = self.shared.refused.as_ref() else {
            return (None, sink);
        };
        let description = RequestDescription {
            sapi_name: self.shared.host.to_owned(),
            ..RequestDescription::default()
        };
        let mut req = Request::lent(Arc::clone(&self.shared), description, sink);
        for (level, message) in &refused.raised {
            req.note_startup_error(*level, message);
        }
        (Some(-2), req.release_sink())
    }

    /// What php writes to the process's `stderr` while it starts up and
    /// this engine kept for its host, since it owns no stream: the line
    /// `ini_error` writes for a syntax error in the host's ini entries,
    /// or nothing, as bytes, a name that is not UTF-8 written as it was
    /// set. A host writes it before anything else it serves.
    #[must_use]
    pub fn startup_errors(&self) -> &[u8] {
        &self.shared.startup_errors
    }

    /// `fpm_php_apply_defines` (`Z/sapi/fpm/fpm/fpm_php.c:207-226`): a
    /// host's entries laid over the started engine, in the order given,
    /// as the master value every request starts with, as php-fpm's child
    /// lays its pool's `php_value` and `php_admin_value` over the engine
    /// its master started and the fork copied. Each is the value as
    /// given, not read again by the INI grammar
    /// (`fpm_php_zend_ini_alter_master`): one php knows, and whose value
    /// its check takes, is handed to the entry's `on_modify` at
    /// `ZEND_INI_STAGE_ACTIVATE` and set where that takes it, or set where
    /// it has none, an admin value so set locked against every request
    /// ([`IniOrigin::Admin`]); one php does not know is left, so
    /// [`Engine::ini_get`] answers `None` for it; and `disable_functions`
    /// takes the natives it names out of the function table as the
    /// startup's does (`zend_disable_functions`). None of it reaches the
    /// startup ini the extensions' engine starts read nor the
    /// post-startup, which ran before it under the master's ini, as php's
    /// did: a pool's `opcache.preload` preloads nothing.
    ///
    /// What the handlers wrote to `stderr`, for the host to write; what
    /// they raised is shown nowhere, as php-fpm's child shows nothing of
    /// what its pool's values raise. [`NotAlone`], with nothing laid,
    /// where another clone of the engine stands, which could be serving.
    pub fn alter_master_ini(
        &mut self,
        entries: Vec<(Vec<u8>, Vec<u8>, IniOrigin)>,
    ) -> Result<Vec<u8>, NotAlone> {
        let shared = &self.shared;
        let description = RequestDescription {
            sapi_name: shared.host.to_owned(),
            ..RequestDescription::default()
        };
        let mut req = Request::new(Arc::clone(shared), description);
        req.begin_startup();
        req.set_ini_stage(IniStage::Activate);
        let (mut taken, mut locked) = (Vec::new(), Vec::new());
        for (name, value, mode) in entries {
            let per_dir = || {
                (shared.extensions.iter())
                    .flat_map(|e| e.ini.iter())
                    .find(|d| *d.name.0 == *name)
                    .map_or(Request::ini_modifiable(&name).1, |d| d.per_dir)
            };
            if shared.ini.iter().all(|(n, _)| *n != name)
                || (mode == IniOrigin::PerDir && !per_dir())
                || Request::ini_verdict(&name, &value) == crate::api::abi::IniVerdict::Rejected
            {
                continue;
            }
            let took = on_modify(&shared.extensions, &name)
                .is_none_or(|handler| matches!(handler(&mut req, Some(&value)), Ok(true)));
            if !took {
                continue;
            }
            req.take_startup_value(&name, &value);
            if mode == IniOrigin::Admin {
                locked.push(name.clone());
            }
            taken.push((name, value));
        }
        req.end_startup();
        let written = req.startup_stderr();
        drop(req);
        let end = Arc::get_mut(&mut self.end).ok_or(NotAlone)?;
        // The end's own hold on what every request shares, let go while
        // the values are laid and taken again after.
        let armed = end.shared.take().is_some();
        let laid = match Arc::get_mut(&mut self.shared) {
            Some(shared) => {
                for (name, value) in taken {
                    if name == b"disable_functions" {
                        shared.disable_functions(&value);
                    }
                    shared.set_ini_default(name, Some(value));
                }
                shared.locked.extend(locked);
                Ok(written)
            }
            None => Err(NotAlone),
        };
        if armed {
            end.shared.set(Arc::clone(&self.shared)).ok();
        }
        laid
    }

    /// Whether `cli_shell_callbacks.cli_shell_run` is set: the host
    /// carries the table ([`EngineBuilder::cli_shell`]) and an extension
    /// of the bundle filled it, which `do_cli` asks of `-a`.
    #[must_use]
    pub fn has_shell(&self) -> bool {
        self.shared.supplied.shell.get().is_some()
    }

    /// `php_ini_activate_per_host_config` for `host`, lowered as a CGI's
    /// `SERVER_NAME` is, then `php_ini_activate_per_dir_config` along each
    /// directory prefix of `path_translated`'s directory
    /// (`Z/main/php_ini.c:815-875`): each `[HOST=]` and `[PATH=]` table's
    /// strings, which `php_ini_activate_config` sets under `PHP_INI_SYSTEM`
    /// at `PHP_INI_STAGE_ACTIVATE` and so locks as an admin value is
    /// locked. A `name[]=` array in such a table, which php hands
    /// `zend_alter_ini_entry_ex` as the string it is not, is passed over.
    #[must_use]
    pub fn activated_ini(
        &self,
        host: Option<&[u8]>,
        path_translated: &[u8],
    ) -> Vec<(Vec<u8>, Vec<u8>, IniOrigin)> {
        let [per_dir, per_host] = self.shared.sections;
        let mut tables = Vec::new();
        if let Some(host) = host.filter(|host| per_host && !host.is_empty()) {
            tables.push(host.to_ascii_lowercase());
        }
        // `zend_dirname`, and the slash `sapi_cgi_activate` puts after it.
        let mut dir = path_translated;
        while dir.len() > 1 && dir.ends_with(b"/") {
            dir = &dir[..dir.len() - 1];
        }
        dir = match dir.iter().rposition(|b| *b == b'/') {
            Some(at) => match dir[..at].iter().rposition(|b| *b != b'/') {
                Some(end) => &dir[..=end],
                None => b"/",
            },
            None => b".",
        };
        let path = [dir, b"/"].concat();
        if per_dir && path.len() <= crate::virtual_cwd::MAXPATHLEN {
            for (at, byte) in path.iter().enumerate().skip(1) {
                if *byte == b'/' {
                    tables.push(path[..at].to_vec());
                }
            }
        }
        let mut entries = Vec::new();
        for table in tables {
            if let Some(Configured::Array(rows)) = self.shared.configured(&table) {
                for (key, value) in rows {
                    let name = match key {
                        ConfiguredKey::Name(name) => name.clone(),
                        ConfiguredKey::Index(n) => n.to_string().into_bytes(),
                    };
                    if let Configured::Str(value) = value {
                        entries.push((name, value.clone(), IniOrigin::Admin));
                    }
                }
            }
        }
        entries
    }

    /// `ini_get()` outside any request: the value each request starts
    /// with, `None` for an entry php does not know.
    #[must_use]
    pub fn ini_get(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.shared.ini_default(name).map(<[u8]>::to_vec)
    }

    /// `INI_STR` outside any request: the value each request starts
    /// with as php holds it, `None` where it has none (`NULL`), as for a
    /// name php does not know; the empty string where the ini set it so.
    #[must_use]
    pub fn ini_value(&self, name: &[u8]) -> Option<Vec<u8>> {
        self.shared.ini_default_value(name).map(<[u8]>::to_vec)
    }

    /// `zend_parse_ini_string` under the normal scanner, as a host reads
    /// its own configuration once php has started (php-fpm's
    /// `fpm_conf_load_ini_file`): each statement's event in order, a
    /// constant read from this engine's table and `${…}` from the
    /// process's environment; and where the text is not INI, bison's
    /// message and the line after the events before it.
    #[must_use]
    pub fn parse_ini(&self, text: &[u8]) -> (Vec<IniEvent>, Option<(String, u32)>) {
        let mut host = Configure {
            shared: &self.shared,
            events: Vec::new(),
        };
        let failed = match crate::ini_syntax::parse(text, IniScanner::Normal, &mut host) {
            Ok(parsed) => parsed.err(),
            Err(never) => match never {},
        };
        (host.events, failed)
    }

    /// The extensions this engine loaded, `Core` first and then the
    /// bundle's in startup order, as `get_loaded_extensions()` and the
    /// CLI's `-m` list them: what a host prints without a request.
    #[must_use]
    pub fn loaded_extensions(&self) -> Vec<String> {
        self.shared
            .extensions
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    /// The Zend extensions this engine loaded, in startup order, each the
    /// name its `zend_extension` row carries: what the CLI's `-m` lists
    /// under `[Zend Modules]`.
    #[must_use]
    pub fn loaded_zend_extensions(&self) -> Vec<String> {
        self.shared
            .extensions
            .iter()
            .filter_map(|e| e.zend.as_ref())
            .map(|row| row.name.clone())
            .collect()
    }

    /// `get_zend_version`: `ZEND_CORE_VERSION_INFO` and a line for each
    /// Zend extension loaded, in startup order, as
    /// `zend_append_version_info` appends it (`Z/Zend/zend.c:1289`), the
    /// tail of what a host's `-v` prints.
    #[must_use]
    pub fn zend_version_info(&self) -> String {
        let mut text = format!(
            "Zend Engine v{}, Copyright (c) Zend Technologies\n",
            crate::natives::ZEND_VERSION
        );
        for row in self
            .shared
            .extensions
            .iter()
            .filter_map(|e| e.zend.as_ref())
        {
            writeln!(
                text,
                "    with {} v{}, {}, by {}",
                row.name, row.version, row.copyright, row.author
            )
            .ok();
        }
        text
    }

    /// The compiled unit for `source`, as the cache holds it: its
    /// constant strings interned into this engine's table, so two units
    /// carrying one constant carry the one `zend_string` (design §5.2).
    #[must_use]
    pub fn unit(&self, source: &[u8], filename: &[u8]) -> Option<Arc<Unit>> {
        let stack = self.shared.stack_limit();
        let cwd = std::env::current_dir().unwrap_or_default();
        let origin = Origin {
            filename,
            cwd: &cwd,
            kind: SourceKind::File,
            keys_before: 0,
            request_classes: RequestClasses::default(),
            multibyte: self.shared.unbound(),
        };
        self.shared.publish(source, origin, stack).ok()
    }

    /// What php prints on the second stream for the program
    /// `description` names before it runs it, `zend_optimize_op_array`'s
    /// `ZEND_DUMP_BEFORE_OPTIMIZER` arm
    /// (`Z/Zend/Optimizer/zend_optimizer.c:1055-1062`): the unit's op
    /// arrays through `Unit::dump`, or nothing where the engine's
    /// `opcache.opt_debug_level` carries no `0x10000` bit and nothing
    /// where the program does not compile (design §6.1).
    ///
    /// **The host asks for it, and asks once, before it serves.** php's
    /// dump is opcache's, taken as the unit is persisted, which is
    /// after the compiler and before the optimizer; this engine has no
    /// optimizer and one place a program is compiled, so what is left of
    /// php's moment is the host's, between describing the request and
    /// serving it. `php -r`'s code is the one program php dumps nothing
    /// for, `zend_optimize_op_array`'s `op_array->type ==
    /// ZEND_EVAL_CODE` return, and the host is what knows a program is
    /// that (`php_sapi::cli`).
    ///
    /// A file's unit is the cache's, the very one the request that
    /// follows serves; a source the host holds is compiled a second time
    /// here, into the engine's own table rather than the request's, since
    /// a unit compiled for one request is not reachable from outside it.
    /// The dump reads what the compiler emitted and nothing of where the
    /// strings live, so the two compiles print one dump.
    #[must_use]
    pub fn dump_before_optimizer(&self, description: &RequestDescription) -> Option<Vec<u8>> {
        let level = ini_quantity(self.shared.ini_default(b"opcache.opt_debug_level")?);
        if level & BEFORE_OPTIMIZER == 0 {
            return None;
        }
        let stack = self.shared.stack_limit();
        let script = description
            .script_name
            .clone()
            .unwrap_or_else(|| b"Standard input code".to_vec());
        let unit = match &description.source {
            Some(source) => {
                let origin = Origin {
                    filename: &script,
                    cwd: &description.cwd,
                    kind: SourceKind::File,
                    keys_before: 0,
                    request_classes: RequestClasses::default(),
                    multibyte: self.shared.unbound(),
                };
                self.shared.publish(source, origin, stack).ok()?
            }
            None => self.shared.cached(&script, &description.cwd, stack)?.ok()?,
        };
        Some(unit.dump(&self.frameless()))
    }

    /// The one request transaction (design §5.14): compile, seed, serve,
    /// commit, shut down, observe. The request owns `sink` while it runs,
    /// each call reaching it as the request makes it, and hands it back
    /// with what it observed (#946). What the request serves is `look`'s
    /// answer, asked once `php_request_startup` returns, every extension's
    /// `rinit` among it, as php-fpm's loop looks at its request
    /// (`Z/sapi/fpm/fpm/fpm_main.c:1838-1903`); a host that serves the
    /// program the description names answers [`HostServes::Program`].
    /// Where the start failed or was refused, `look` is never called, so
    /// nothing the host would have logged or answered is.
    ///
    /// An engine with no extensions serves what uses Zend alone (design
    /// §4.10): the loop over the main op array, `ECHO` through the
    /// string kernel, and the engine's own natives.
    ///
    /// ```php
    /// echo 1; var_dump(extension_loaded('Core'));
    /// ```
    /// ```expect
    /// 1bool(true)
    /// ```
    ///
    /// A source php refuses ends the request the way php ends one:
    /// `E_PARSE` through `php_error_cb`, the parser's first diagnostic in
    /// php's words at php's line, and php's status. The message is the
    /// parser's, held to the oracle byte for byte; only the file name is
    /// the harness's.
    ///
    /// ```php
    /// echo 1 +;
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected token ";" in %s on line 2
    /// ```
    ///
    /// What php's scanner raised before its parser refused is rendered
    /// before the refusal, in the order php raised them, since php's
    /// `php_error_cb` had already written each when the parser bailed
    /// out; the scanner's octal overflow is an `E_COMPILE_WARNING`
    /// (design §6.1).
    ///
    /// ```php
    /// echo "\777";
    /// echo 1 +;
    /// ```
    /// ```expect exit=255
    /// Warning: Octal escape sequence overflow \777 is greater than \377 in %s on line 2
    ///
    /// Parse error: syntax error, unexpected token ";" in %s on line 3
    /// ```
    ///
    /// The `(unset)` cast is what php's compiler refuses after the whole
    /// file has parsed, `zend_compile_cast`'s `E_COMPILE_ERROR`: the
    /// `Fatal error:` head, and php's status for a fatal, with the
    /// scanner's deprecation on the line before it rendered first. The
    /// frame carries no `Stack trace:` under the recipe, whose
    /// `fatal_error_backtraces=Off` is `run-tests.php`'s own; php's default
    /// of `On` prints `#0 {main}` under it, which
    /// `a_compile_error_carries_php_s_trace_under_the_default_ini` below
    /// holds, since no `ini_set()` runs before the compiler refuses.
    ///
    /// ```php
    /// $x = 1; (integer)$x;
    /// (unset)$x;
    /// echo 'never';
    /// ```
    /// ```expect exit=255
    /// Deprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in %s on line 2
    ///
    /// Fatal error: The (unset) cast is no longer supported in %s on line 3
    /// ```
    pub fn serve(
        &self,
        description: RequestDescription,
        sink: Box<dyn OutputSink + Send>,
        look: impl FnOnce() -> HostServes,
    ) -> (Observables, Box<dyn OutputSink + Send>) {
        let (observed, _, sink) = self.serve_looked(description, sink, look, false);
        (observed, sink)
    }

    /// [`Engine::serve`], with the natives the request enters recorded
    /// where `traced` asks.
    fn serve_looked(
        &self,
        description: RequestDescription,
        sink: Lent,
        look: impl FnOnce() -> HostServes,
        traced: bool,
    ) -> (Observables, Trace, Lent) {
        let source = description.source.clone();
        let mut req = match self.begin(description, sink, traced) {
            Ok(req) => req,
            Err((refused, sink)) => return (refused, Trace::default(), sink),
        };
        self.serve_in(&mut req, source.as_deref(), look());
        self.shutdown(&mut req)
    }

    /// What a started request serves, then each extension's hook for the
    /// moment after it, which php-fpm's `fastcgi_request_done` stands at
    /// for a page as for a program.
    fn serve_in(&self, req: &mut Request, source: Option<&[u8]>, serves: HostServes) {
        match serves {
            HostServes::Program => {
                self.execute_script(req, source);
            }
            // `sapi_add_header_ex(.., replace = 1)` for each line, the
            // code, and `PUTS` of the body, as `fpm_status_handle_request`
            // and the loop's refusals write them.
            HostServes::Page(page) => {
                for line in &page.headers {
                    req.header_add(line, true, 0);
                }
                req.set_response_code(page.code);
                req.write(&page.body);
            }
            HostServes::Nothing => {}
        }
        req.program_ended();
    }

    /// `php_lint_script` under the CLI's `-l` (`Z/main/main.c:2779-2798`,
    /// `Z/sapi/cli/php_cli.c:955-960`): the request started, the program
    /// compiled as [`Engine::serve`] compiles it, what the compiler raised
    /// rendered where `serve` renders it, and the request shut down with
    /// no frame pushed. The exit is `serve`'s for a compile that was
    /// refused, php's status for a parse error, and 0 for one that was
    /// not, whatever the compiler warned of; the two sentences `do_cli`
    /// prints around it are the host's, read off that status.
    pub fn lint(
        &self,
        description: RequestDescription,
        sink: Box<dyn OutputSink + Send>,
    ) -> (Observables, Box<dyn OutputSink + Send>) {
        self.transaction(description, sink, Work::Lint)
    }

    /// The CLI's `-s`, `PHP_CLI_MODE_HIGHLIGHT` (`Z/sapi/cli/php_cli.c:969-977`):
    /// the request started, the program read as `open_file_for_scanning`
    /// reads it and written as `zend_highlight` writes it under the
    /// request's `highlight.*` entries, and the request shut down with no
    /// frame pushed and nothing compiled.
    pub fn highlight(
        &self,
        description: RequestDescription,
        sink: Box<dyn OutputSink + Send>,
    ) -> (Observables, Box<dyn OutputSink + Send>) {
        self.transaction(description, sink, Work::Highlight)
    }

    /// The CLI's `-w`, `PHP_CLI_MODE_STRIP` (`Z/sapi/cli/php_cli.c:963-968`):
    /// as [`Engine::highlight`], with the program written as `zend_strip`
    /// writes it.
    pub fn strip(
        &self,
        description: RequestDescription,
        sink: Box<dyn OutputSink + Send>,
    ) -> (Observables, Box<dyn OutputSink + Send>) {
        self.transaction(description, sink, Work::Strip)
    }

    /// `preload_activate`: the auto-registered arrays the preloaded code
    /// names (`$_SERVER` and its kin), armed for every request.
    fn arm_preloaded(&self, req: &mut Request) {
        if let Some(preloaded) = self.shared.units.preloaded() {
            for global in &preloaded.arms {
                req.arm(*global).ok();
            }
        }
    }

    /// The one transaction behind [`Engine::lint`],
    /// [`Engine::highlight`] and [`Engine::strip`]: `work` says what the
    /// program is for once the request has started.
    fn transaction(
        &self,
        description: RequestDescription,
        sink: Lent,
        work: Work,
    ) -> (Observables, Lent) {
        let source = description.source.clone();
        let cwd = description.cwd.clone();
        let mut req = match self.begin(description, sink, false) {
            Ok(req) => req,
            Err(refused) => return refused,
        };
        // Where the unit's constant strings live is where the unit
        // lives (design §5.2): a file's unit is published into the
        // engine's table and shared by every request that serves it,
        // and a source the host handed over is this request's alone,
        // interned into this request's own memory and gone with it, as
        // php holds what it compiles for `-r` and for `eval`.
        if matches!(work, Work::Highlight | Work::Strip) {
            // `open_file_for_scanning` over the program: the source the
            // host handed over, or the file read whole.
            let script = req
                .script_name()
                .map_or_else(|| b"Standard input code".to_vec(), <[u8]>::to_vec);
            let text = source.or_else(|| std::fs::read(cwd.join(OsStr::from_bytes(&script))).ok());
            if let Some(text) = text {
                use crate::api::syntax::Scanning;
                let written = if work == Work::Highlight {
                    crate::api::syntax::highlight(&mut req, &text, Scanning::File)
                } else {
                    crate::api::syntax::strip(&mut req, &text)
                };
                // A source the filter refused ended the request.
                if let Ok(written) = written {
                    req.write(&written);
                }
            }
            return self.shutdown_untraced(&mut req);
        }
        // `php_lint_script` compiles and drops the op array, and what
        // the compiler warned of it rendered as it compiled.
        match self.primary(&mut req, source.as_deref(), None) {
            Some(Ok(unit)) => {
                note_compiled(&mut req, &unit, SourceKind::File);
            }
            Some(Err(refused)) => {
                let file = req.current_file().map(<[u8]>::to_vec).unwrap_or_default();
                refused_program(&mut req, &refused, &file);
            }
            None => {}
        }
        self.shutdown_untraced(&mut req)
    }

    /// `php_request_startup` for `description`, and what `do_cli` does
    /// after it before a program is compiled, the request handed back to
    /// serve; the refusal `opcache.preload` answers with where it is one,
    /// and where an extension's `rinit` raised, the startup failure every
    /// host but the built-in server ends on ([`Engine::start_failed`]).
    #[expect(
        clippy::result_large_err,
        reason = "the request the Ok variant carries is many times the Err's size"
    )]
    pub(crate) fn begin(
        &self,
        description: RequestDescription,
        sink: Lent,
        traced: bool,
    ) -> Result<Request, (Observables, Lent)> {
        match self.start_request(description, sink, traced) {
            Begun::Started(req) => Ok(req),
            Begun::Failed(req) => Err(Engine::start_failed(req)),
            Begun::Refused(refused, sink) => Err((refused, sink)),
        }
    }

    /// What the extensions' `MINIT`s raised, noted in `req` past the
    /// `php_output` layer, as php displays it before `php_output_activate`,
    /// so no header is sent by it and a session may still start; and once,
    /// as php writes it to the process's `stdout` as its modules start, so
    /// a later request of a long-lived host carries none of it. Whether an
    /// `E_CORE_ERROR` among it bailed out of `php_module_startup`, which
    /// then failed.
    pub(crate) fn show_startup_raised(&self, req: &mut Request) -> bool {
        if !self.shared.startup_shown.swap(true, Ordering::Relaxed) {
            for (level, message) in &self.shared.startup_raised {
                req.note_startup_error(*level, message);
            }
        }
        (self.shared.startup_raised.iter()).any(|(level, _)| *level == Level::CoreError)
    }

    /// [`Engine::begin`] with a failed start handed back as the request
    /// it failed in, for the built-in server, which shuts it down.
    fn start_request(&self, description: RequestDescription, sink: Lent, traced: bool) -> Begun {
        let ended = |status| Observables {
            exit_code: status,
            headers_sent: true,
            ..Observables::default()
        };
        // php never reaches `zend_post_startup` past a failed `MINIT`.
        let (status, sink) = self.refused(sink);
        if let Some(status) = status {
            return Begun::Refused(ended(status), sink);
        }
        let (started, sink) = self.started(&description, sink);
        if let crate::preload::Started::Refused(status) = started {
            return Begun::Refused(ended(status), sink);
        }
        let web = description.web.is_some();
        let mut req = Request::lent(Arc::clone(&self.shared), description, sink);
        if traced || self.shared.trace_to.is_some() {
            req.trace_natives();
        }
        // `php_request_startup` (`Z/main/main.c:1890`): `sapi_activate`
        // reads the body, the `X-Powered-By` header goes on while
        // `expose_php` is on, and the environment is hashed before the
        // extensions' `RINIT`; what can fail is the memory limit, which the
        // first allocation after it meets again.
        req.apply_host_ini();
        req.read_post_at_startup();
        if web && req.ini_flag(b"expose_php") && req.headers_sent().is_none() {
            let line = format!("X-Powered-By: PHP/{}", crate::api::request::PHP_VERSION);
            let _ = req.header_add(line.as_bytes(), true, 0);
        }
        crate::output::start_at_request(&mut req).ok();
        req.hash_environment().ok();
        // An `E_CORE_ERROR` bailed out of `php_module_startup`, which
        // failed, and the host ends with status 1 before any request.
        if self.show_startup_raised(&mut req) {
            return Begun::Refused(ended(1), req.release_sink());
        }
        if let Err(thrown) = serve_rinits(&mut req, &self.shared.extensions) {
            // `zend_throw_exception_internal` with no frame standing
            // (`Z/Zend/zend_exceptions.c:206-224`): the throwable rendered
            // as the uncaught fatal, and `zend_bailout` to the `zend_catch`
            // of `php_request_startup`, which answers `FAILURE`; a fatal
            // the `rinit` met was rendered where it was raised.
            if let Ok(undef) = req.thrown_without_frame(&thrown) {
                req.release(undef);
            }
            return Begun::Failed(req);
        }
        self.arm_preloaded(&mut req);
        if req.web().is_some_and(|web| web.no_headers) {
            req.output_mut().quiet();
        }
        // What `do_cli` does once `php_request_startup` has run, before
        // the program is compiled: `$_SERVER` named, which makes it
        // (`Z/sapi/cli/php_cli.c:941`), and the standard handles
        // registered.
        if !web {
            req.arm(php_unit::AutoGlobal::Server).ok();
        }
        req.register_file_handles(false).ok();
        req.enter_phase(crate::api::request::Phase::Running);
        Begun::Started(req)
    }

    /// The end of a request whose start failed, as php-cgi, php-fpm, the
    /// CLI and phpdbg end it: what the host's `php_request_startup` left
    /// written reaches the host, and the request's states drop with it,
    /// with no request-end phase served, since each goes from the failure
    /// to `php_module_shutdown` without `php_request_shutdown` (#1775).
    /// What a buffer of the `php_output` layer still holds is never
    /// flushed, and the head is sent only where something written sent
    /// it. The host is handed the failure ([`Observables::start_failed`]),
    /// and each host's tests hold its ending, since a fence carries
    /// neither the ini nor the session file a failing `rinit` needs
    /// (#1634).
    fn start_failed(mut req: Request) -> (Observables, Lent) {
        #[cfg(debug_assertions)]
        req.end_in_sensed();
        let fatal = match req.ending() {
            Some(Ending::Fatal(logged)) => Some(logged.clone()),
            _ => None,
        };
        let observed = Observables {
            exit_code: req.exit_status(),
            headers_sent: req.output().headers_sent(),
            fatal,
            memory_peak: req.memory_peak(true),
            uncaught: None,
            start_failed: true,
        };
        (observed, req.release_sink())
    }

    /// `php_execute_script` (`Z/main/main.c:2568-2648`) over the
    /// request's program: `auto_prepend_file` required first, then the
    /// program, then `auto_append_file`, each while the one before ran
    /// to its end, with the program's file held open around the three
    /// as the host's `zend_file_handle` holds it ([`ProgramFile`]);
    /// whether the program did.
    pub(crate) fn execute_script(&self, req: &mut Request, source: Option<&[u8]>) -> bool {
        let script = req.primary() == Primary::Wrapped;
        // The name the host wrote, `primary_file->filename`, which
        // `php_execute_script_ex` leaves as it is beside the expanded
        // `opened_path`, and which opcache makes the file's key of.
        let written = req.script_name().map(<[u8]>::to_vec);
        let mut left = None;
        if script {
            // php-cgi and php-fpm open the file before
            // `php_execute_script` (`php_fopen_primary_script`), so its
            // `opened_path` is whole against the directory the request
            // started in, and the program's is entered by the name written.
            // php -S hands a name it has not opened, which `php_execute_script`
            // opens after it entered the directory; the name is whole, so
            // either order reads the same file.
            req.expand_primary_file(source.is_some());
            left = req.enter_program_dir(written.as_deref());
        }
        if script && source.is_none() {
            req.open_primary_file();
        }
        let ran = self.execute_wrapped(req, source, script, written.as_deref());
        req.close_served_files();
        if let Some(left) = left {
            req.go_back(left);
        }
        ran
    }

    /// [`Engine::execute_script`] with the primary file open:
    /// `auto_prepend_file`, the program the host wrote as `written` and
    /// `auto_append_file`.
    fn execute_wrapped(
        &self,
        req: &mut Request,
        source: Option<&[u8]>,
        script: bool,
        written: Option<&[u8]>,
    ) -> bool {
        if script && !auto_file(req, b"auto_prepend_file", self.shared.loops.entry) {
            return false;
        }
        let compiled = self.primary(req, source, written);
        // A source the host served under a name was noted as it was
        // expanded ([`Request::expand_primary_file`]).
        if script && source.is_none() && compiled.is_some() {
            req.note_compiled_program();
        }
        let ran = match compiled {
            Some(Ok(unit)) => serve_unit(req, &unit, self.shared.loops.entry),
            // `php_error_cb` renders it where it renders every
            // diagnostic, at the level the compiler refused at, and the
            // request ends before a frame is pushed.
            Some(Err(refused)) => {
                let file = req.current_file().map(<[u8]>::to_vec).unwrap_or_default();
                refused_program(req, &refused, &file);
                false
            }
            None => false,
        };
        ran && (!script || auto_file(req, b"auto_append_file", self.shared.loops.entry))
    }

    /// The program compiled: the source the host handed over under the
    /// name the request gives it, or the file the request names, under
    /// the name `php_execute_script` and not `php_lint_script` has
    /// expanded first ([`Request::expand_primary_file`]), so the unit and
    /// its diagnostics carry the whole path, and filed in the unit cache
    /// under its key as the host `written` it too, which a relative name
    /// or one through a link makes a second key of, as
    /// `persistent_compile_file` files it:
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/keys' . getmypid(); @mkdir($dir); @symlink($dir, "$dir.link");
    /// file_put_contents("$dir/keys.php", '<?php $s = opcache_get_status()["opcache_statistics"]; echo $s["num_cached_scripts"], " ", $s["num_cached_keys"], "\n";');
    /// touch("$dir/keys.php", time() - 60);
    /// foreach (["keys.php", "$dir/keys.php", "$dir.link/keys.php"] as $name) {
    ///     $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', $name], [1 => ['pipe', 'w']], $pipes, $dir);
    ///     echo stream_get_contents($pipes[1]); proc_close($p);
    /// }
    /// unlink("$dir.link"); unlink("$dir/keys.php"); rmdir($dir);
    /// ```
    /// ```expect process
    /// 1 2
    /// 1 1
    /// 1 2
    /// ```
    fn primary(
        &self,
        req: &mut Request,
        source: Option<&[u8]>,
        written: Option<&[u8]>,
    ) -> Option<Result<Arc<Unit>, CompileError>> {
        let laid = req.heap_mut().lay_interned_table();
        if let Err(thrown) = req.fatal_of(laid) {
            req.leave_pending(&thrown);
            return None;
        }
        if let Some(source) = source {
            let script = req
                .script_name()
                .map_or_else(|| b"Standard input code".to_vec(), <[u8]>::to_vec);
            return Some(
                self.shared
                    .compile_for(req, source, &script, SourceKind::File),
            );
        }
        let script = req.script_name().map(<[u8]>::to_vec).unwrap_or_default();
        // A bare program is opened as phpdbg's `phpdbg_compile` opens it,
        // a stream through `php_stream_open_for_zend_ex`, and spends a
        // resource id where the CLI's `FILE *` spends none; the built-in
        // server's is opened by its compile, as its router is.
        let handle = match (req.primary(), req.program_file()) {
            (Primary::Wrapped, ProgramFile::Unopened) => HandlePath::Kept,
            (Primary::Wrapped, _) => HandlePath::Host,
            (Primary::Bare, _) => HandlePath::Opened,
        };
        Engine::compile_program(req, &script, written.unwrap_or(&script), handle)
    }

    /// The program or the router at `path`, named `written` by the host,
    /// compiled for the request through a `zend_file_handle` of the kind
    /// `handle` names, `zend_compile_file` ([`Request::compile_file`]);
    /// what a link threw is left pending and nothing compiled.
    fn compile_program(
        req: &mut Request,
        path: &[u8],
        written: &[u8],
        handle: HandlePath,
    ) -> Option<Result<Arc<Unit>, CompileError>> {
        req.compile_file(path, Some(written), handle)
            .unwrap_or_else(|thrown| {
                req.leave_pending(&thrown);
                None
            })
    }

    /// `php_cli_server_dispatch` (`Z/sapi/cli/php_cli_server.c:2294`)
    /// once the request has started: where the server has a router, it
    /// is served first as `php_execute_script_ex` serves a program, never
    /// moving the working directory, and declines where it returns
    /// `false` (`php_cli_server_dispatch_router`); where there is none, or
    /// it declined, the program the request names is served in the same
    /// request, and the response code and the last error just after it
    /// are what `php_cli_server_log_response` writes, a 304 then
    /// committing no default `Content-type`; a program whose path holds a
    /// NUL is never served, and the request ends with the head it has
    /// ([`ProgramServed::NulInPath`]). A request the router declined
    /// that names no program is a static file, the host's to send, and
    /// the request ends with its head not sent
    /// (`sapi_cli_server_discard_headers`).
    pub fn dispatch(
        &self,
        description: RequestDescription,
        router: Option<&[u8]>,
        sink: Box<dyn OutputSink + Send>,
    ) -> (Observables, Dispatched, Box<dyn OutputSink + Send>) {
        // `php_cli_server_dispatch_script`'s check
        // (`Z/sapi/cli/php_cli_server.c:2127`): a path holding a NUL is
        // one php refuses.
        let named = match description.script_name.as_deref() {
            None => Named::Nothing,
            Some(script) if script.contains(&0) => Named::NulInPath,
            Some(_) => Named::Program,
        };
        let mut dispatched = Dispatched {
            routed: Routed::Taken,
            program: None,
        };
        let mut req = match self.start_request(description, sink, false) {
            Begun::Started(req) => req,
            // `php_cli_server_dispatch` shuts a request whose start
            // failed down and closes the connection
            // (`Z/sapi/cli/php_cli_server.c:2307-2310`): the one host
            // that serves `php_request_shutdown` after the failure, so
            // the request-end phases run, and no response line is logged.
            Begun::Failed(mut req) => {
                let (observed, sink) = self.shutdown_untraced(&mut req);
                let observed = Observables {
                    start_failed: true,
                    ..observed
                };
                return (observed, dispatched, sink);
            }
            Begun::Refused(refused, sink) => return (refused, dispatched, sink),
        };
        if let Some(router) = router {
            dispatched.routed = self.execute_router(&mut req, router);
        }
        if router.is_none() || dispatched.routed == Routed::Declined {
            match named {
                Named::Program => {
                    self.execute_script(&mut req, None);
                    dispatched.program = Some(ProgramServed::Ran {
                        code: req.response_code(),
                        fatal: req.last_fatal(),
                    });
                    if req.response_code() == 304 {
                        req.output_mut().no_default_content_type();
                    }
                }
                Named::NulInPath => dispatched.program = Some(ProgramServed::NulInPath),
                Named::Nothing if dispatched.routed == Routed::Declined => req.discard_headers(),
                Named::Nothing => {}
            }
        }
        let (observed, sink) = self.shutdown_untraced(&mut req);
        (observed, dispatched, sink)
    }

    /// `php_cli_server_dispatch_router`: `router` served as
    /// `php_execute_script_ex` serves a program, through the path alone
    /// as `zend_stream_init_filename` leaves it, so its file is opened as
    /// it compiles, where the unit cache does not answer it by its name,
    /// and held open until the router and the append file are served, as
    /// `zend_destroy_file_handle` closes it after `php_execute_script_ex`
    /// ([`HandlePath::Kept`]); and whether it declined. It is served in
    /// the directory the request stands in, under `SAPI_OPTION_NO_CHDIR`
    /// (`Z/sapi/cli/php_cli_server.c:2269-2270`), which is restored after
    /// it whatever it moved to, as the server's `VCWD_CHDIR(old_cwd)`
    /// (`:2284-2286`).
    fn execute_router(&self, req: &mut Request, router: &[u8]) -> Routed {
        let left = req.standing_dir();
        let answered = self.execute_router_files(req, router);
        req.close_served_files();
        req.go_back(left);
        answered
    }

    /// [`Engine::execute_router`] with the files it opens held:
    /// `auto_prepend_file`, the router and `auto_append_file`.
    fn execute_router_files(&self, req: &mut Request, router: &[u8]) -> Routed {
        let entry = self.shared.loops.entry;
        if !auto_file(req, b"auto_prepend_file", entry) {
            return Routed::Taken;
        }
        let path = req
            .expand_filepath(router)
            .unwrap_or_else(|| router.to_vec());
        let ran = match Engine::compile_program(req, &path, router, HandlePath::Kept) {
            Some(Ok(unit)) => serve_unit(req, &unit, entry),
            Some(Err(refused)) => {
                refused_program(req, &refused, &path);
                false
            }
            None => false,
        };
        let returned = req.take_returned();
        let declined = ran && returned.as_bool() == Some(false);
        req.release(returned);
        if ran && !auto_file(req, b"auto_append_file", entry) {
            return Routed::Taken;
        }
        if declined {
            Routed::Declined
        } else {
            Routed::Taken
        }
    }

    /// `php_request_shutdown` and what the host is handed after it. The
    /// request is shut down where it was served and never moved first: a
    /// fiber still suspended holds the address it was resumed with, and
    /// is resumed with it again to be destroyed.
    pub(crate) fn shutdown(&self, req: &mut Request) -> (Observables, Trace, Lent) {
        Engine::shutdown_program(req);
        self.shutdown_request(req)
    }

    /// [`Engine::shutdown`] with no trace handed back, for what records
    /// none.
    pub(crate) fn shutdown_untraced(&self, req: &mut Request) -> (Observables, Lent) {
        let (observed, _, sink) = self.shutdown(req);
        (observed, sink)
    }

    /// The first steps of `php_request_shutdown`, what the program still
    /// runs in: its shutdown functions, its destructors, and every
    /// handler of the buffer stack popped.
    pub(crate) fn shutdown_program(req: &mut Request) {
        // `php_request_shutdown`'s order (design §5.10): every frame a
        // fatal left open to an observer ended first,
        // `zend_observer_fcall_end_all`; the shutdown functions next,
        // which is `php_call_shutdown_functions`; then
        // the destructors of what is still live, which is
        // `zend_call_destructors`, the symbol table walked in reverse
        // and the object store walked after it; then every output
        // handler popped (`php_output_end_all`), before the extensions'
        // `rshutdown`, in reverse bundle order; the output layer
        // deactivated after them, with the header block committed if
        // nothing had, and the shutdown entries freed behind it
        // (`php_free_shutdown_functions`); then `PG(http_globals)` given
        // up; then `shutdown_executor`, after each extension's
        // `deactivate`, which closes the open resources first, destroys the symbol
        // table after them (`zend_shutdown_executor_values`) and frees
        // the object storage last. The globals stand through all of it,
        // as php's `EG(symbol_table)` does, so the shutdown functions
        // and an extension's `rshutdown` read a request that still has
        // them.
        // Each step is its own `zend_try`, so a fatal's bailout leaves
        // the step it was raised in and the next is served.
        req.enter_phase(crate::api::request::Phase::Shutdown);
        req.catch_bailout();
        crate::observer::end_all(req);
        crate::natives::call_shutdown_functions(req);
        req.catch_bailout();
        req.shutdown_destructors();
        req.call_destructors();
        req.catch_bailout();
        crate::output::end_all(req);
        req.catch_bailout();
        // "4. Reset max_execution_time (no longer executing php code
        // after response sent)" (`Z/main/main.c:1990-1994`).
        req.unset_timer();
    }

    /// The rest of `php_request_shutdown` after
    /// [`Engine::shutdown_program`], and what the host is handed.
    pub(crate) fn shutdown_request(&self, req: &mut Request) -> (Observables, Trace, Lent) {
        // `PG(report_memleaks)` as `php_request_shutdown` reads it first.
        // The debug teardown below runs where the request leak sensor is
        // armed, as it is unless the engine was built with it off
        // ([`EngineBuilder::sense_leaks`]); unarmed, a debug request ends
        // as a release one does.
        #[cfg(debug_assertions)]
        let report_memleaks = req.ini_flag(b"report_memleaks");
        end_phase(&self.shared.extensions, req, |e| e.rshutdown);
        crate::output::deactivate(req);
        req.catch_bailout();
        req.free_shutdown_functions();
        req.destroy_http_globals();
        req.destroy_uploaded_files();
        // `zend_deactivate` reaches `shutdown_executor`: each extension's
        // `deactivate`, in reverse, before the resources are closed.
        end_phase(&self.shared.extensions, req, |e| e.deactivate);
        req.close_resources();
        req.free_user_handlers();
        req.destroy_symbol_table();
        req.destroy_static_vars();
        req.free_default_arguments();
        req.release_all_unit_names();
        #[cfg(debug_assertions)]
        if self.shared.sense_leaks.armed() {
            req.free_executor_values();
            req.shutdown_collect();
        }
        req.free_object_storage();
        req.free_in_autoload();
        // `zend_deactivate`'s `zend_ini_deactivate`, after
        // `shutdown_executor` (`Z/Zend/zend.c:1320-1330`).
        req.ini_deactivate();
        // `php_free_request_globals`' `clear_last_error()`.
        req.clear_last_error_strings();
        // `zend_post_deactivate_modules`, after the executor is shut
        // down and the request globals freed (`Z/main/main.c:2026-2029`),
        // and before the host's deactivation hands over what is left.
        end_phase(&self.shared.extensions, req, |e| e.post_deactivate);
        // A debug build's request leak sensor, after the request's last
        // release, then its last sensing point, which raises what the
        // leak sensor recorded as it raises every other sensor's defect.
        #[cfg(debug_assertions)]
        {
            if self.shared.sense_leaks.armed() {
                req.sense_leaks(report_memleaks);
            }
            req.end_in_sensed();
        }
        // `php_error_cb` rendered the fatal where it was raised, on
        // whichever of the two streams the ini entries name; what is left
        // for the host is the log form of it, whether or not `log_errors`
        // put that form on the log.
        let fatal = match req.ending() {
            Some(Ending::Fatal(logged)) => Some(logged.clone()),
            _ => None,
        };
        let exit_code = req.exit_status();
        // The block reached the sink at the commit, which the output
        // layer's deactivation makes if nothing had; one it refused, for
        // the memory limit, is handed over now.
        if !req.output().headers_sent() {
            req.send_header_block();
        }
        let sink = req.release_sink();
        // What the request entered, taken last, so a destructor or a
        // shutdown function served above is in it.
        let trace = req.take_trace();
        if let Some(path) = &self.shared.trace_to {
            crate::trace::append_trace(path, &trace);
        }
        (
            Observables {
                exit_code,
                headers_sent: true,
                fatal,
                memory_peak: req.memory_peak(true),
                uncaught: req.take_kept_uncaught(),
                start_failed: false,
            },
            trace,
            sink,
        )
    }

    /// [`Engine::serve`] of the program with the natives the request
    /// enters recorded and handed back beside what it observed: the
    /// witness runner's switch (harness §4.5 condition 2, revision 58).
    #[cfg(any(feature = "test", test))]
    pub(crate) fn serve_traced(
        &self,
        description: RequestDescription,
        sink: Lent,
    ) -> (Observables, Trace, Lent) {
        self.serve_looked(description, sink, || HostServes::Program, true)
    }
}

/// One end phase of the extensions' (ADR-0010): `hook` of each that
/// declared one, in reverse bundle order, each in a `zend_try` of its
/// own, as `zend_deactivate_modules` serves each extension's `RSHUTDOWN`
/// (`Z/Zend/zend_API.c:3441-3448`). A fatal one raises is rendered and
/// ends that function only; the next is still served, and a fatal it
/// raises is rendered too.
fn end_phase(
    extensions: &[ExtensionEntry],
    req: &mut Request,
    hook: fn(&ExtensionEntry) -> Option<EndFunction>,
) {
    for (at, extension) in extensions.iter().enumerate().rev() {
        let Some(serve) = hook(extension) else {
            continue;
        };
        req.serve_for(ExtensionId::at(at), serve);
        req.catch_bailout();
    }
}

/// What a transaction hands back beside its observables: the natives the
/// request entered, where it was asked to record them, and none where not.
pub(crate) type Trace = Vec<crate::trace::Entered>;

/// What a transaction is for once the request has started: the program
/// linted, or written as `zend_highlight` or `zend_strip` writes it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    Lint,
    Highlight,
    Strip,
}

/// The ini entries php reads while a file compiles, read once where the
/// compile begins and handed down: `precision`, which `zend_double_to_str`
/// renders a folded double constant at (`EG(precision)`),
/// `short_open_tag`, which php's scanner opens a `<?` under
/// (`CG(short_tags)`, `Z/main/main.c:820`), and whether `zend.assertions`
/// is at or above zero, which `zend_compile_assert` compiles a call of
/// `assert()` under (`EG(assertions)`); beside them `CG(skip_shebang)`,
/// which the host sets and no ini moves. php keeps them where its
/// compiler reads them as it compiles, so what the ini says when the
/// compile begins is what the whole file is compiled under: the startup ini's
/// for a unit bound for the cache, which every request that serves it
/// shares, and the request's own for a source that request holds.
#[derive(Debug, Clone, Copy)]
struct CompileIni {
    precision: i32,
    short_open_tag: bool,
    assertions: bool,
    shebang: php_compile::Shebang,
    /// `CG(multibyte)`, `zend.multibyte`, `PHP_INI_PERDIR`.
    multibyte: bool,
}

/// One source compiled, with what the compiler refused kept instead of
/// dropped: what `serve` needs to render php's parse error, and what
/// [`Engine::unit`] is the lossy face over. `origin` is the name the
/// unit carries and the request's working directory, `strings` the home
/// the unit's constant strings are interned into (design §5.2), `ini`
/// the entries php's compiler reads as it compiles ([`CompileIni`]),
/// `declared` the engine's tables: the constants the bundle declared, which the
/// compiler substitutes where they are written, the natives, which it
/// reads as `CG(function_table)` before it serves a call of
/// `zend_try_compile_special_func`'s table itself, and the classes, which
/// it reads as `CG(class_table)` to know a method's callee; and `stack`
/// the calling thread's limit, which the compiler's recursion is held to.
/// The unit's op arrays take their runtime caches where it is shared:
/// offsets of the engine's as it is published ([`assign_map_ptrs`]), or
/// numbers of the request's it was compiled for
/// ([`exec::RuntimeCaches::take`]).
fn compile(
    source: &[u8],
    origin: Origin<'_>,
    ini: CompileIni,
    declared: Declared<'_>,
    stack: StackLimit,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    let options = php_compile::Options {
        filename: origin.filename,
        kind: origin.kind,
        cwd: origin.cwd.as_os_str().as_encoded_bytes(),
        strict_types: false,
        precision: ini.precision,
        short_open_tag: ini.short_open_tag,
        assertions: ini.assertions,
        shebang: ini.shebang,
        persistent: declared.constants,
        functions: declared.functions,
        frameless: declared.frameless,
        classes: declared.classes,
        request_classes: origin.request_classes,
        stack,
        keys_before: origin.keys_before,
        multibyte: origin.multibyte,
        hooked: declared.hooked,
        extended_info: declared.extended_info,
    };
    php_compile::compile_source(source, &options, strings)
}

/// What compiling the unit raised, raised now, each through the
/// path a runtime diagnostic takes and at the unit's own file and the
/// compiler's line, which is `zend_emit_recorded_errors_ex` over a
/// cached `persistent_script`'s `warnings`
/// (`Z/ext/opcache/ZendAccelerator.c:1939`) and, for a unit compiled by
/// this request, where `zend_error` under `CG(in_compilation)` had
/// already put it. So the order against what the program writes is
/// php's, and a cache hit raises what a miss raised, as the oracle does
/// with no cache at all.
///
/// ```php
/// echo 'a';
/// echo NAN . 'z';
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to string in %s on line 3
/// aNANz
/// ```
///
/// The scanner's are among them, before the compiler's, at the levels
/// php raises each under (design §6.1): the octal escape overflow is an
/// `E_COMPILE_WARNING` and the byte written is the low eight bits of
/// what the digits spell, `(char) ZEND_STRTOL`; a non-canonical cast
/// spelling is the `E_DEPRECATED` 8.5 raises, and the cast itself runs.
///
/// ```php
/// echo "\777", "|", ord("\777"), "|", "\101\x41";
/// ```
/// ```expect
/// Warning: Octal escape sequence overflow \777 is greater than \377 in %s on line 2
///
/// Warning: Octal escape sequence overflow \777 is greater than \377 in %s on line 2
/// %c|255|AA
/// ```
///
/// ```php
/// $x = "7 apples"; echo (integer)$x, "|", (double)$x, "|", (binary)$x, "|", (boolean)$x;
/// ```
/// ```expect
/// Deprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in %s on line 2
///
/// Deprecated: Non-canonical cast (double) is deprecated, use the (float) cast instead in %s on line 2
///
/// Deprecated: Non-canonical cast (binary) is deprecated, use the (string) cast instead in %s on line 2
///
/// Deprecated: Non-canonical cast (boolean) is deprecated, use the (bool) cast instead in %s on line 2
/// 7|7|7 apples|1
/// ```
///
/// Each is raised as php's `zend_error` raises one while serving, so a
/// user error handler is handed the ones its mask and its level admit,
/// at the unit's file and the compiler's line. An `eval`'s are raised as
/// php's `compile_string` raises them, as it compiles; a file's are what
/// `compile_file` recorded, emitted once the file's functions and
/// classes are declared, and emitted ahead of a fatal its declarations
/// raise with the handler masked off, as `zend_error_impl` emits what it
/// holds before a fatal.
///
/// ```php
/// set_error_handler(function ($no, $str) { echo "handler: $str\n"; });
/// eval('function w() { switch (1) { case 1: continue; } }');
/// ```
/// ```expect
/// handler: "continue" targeting switch is equivalent to "break"
/// ```
///
/// ```php
/// function g() {}
/// set_error_handler(function ($no, $str) { echo "handler: $str\n"; });
/// $f = tempnam(sys_get_temp_dir(), 'w');
/// file_put_contents($f, '<?php function h() { switch (1) { case 1: continue; } } echo function_exists("h") ? "declared\n" : "";');
/// include $f;
/// file_put_contents($f, '<?php switch (1) { case 1: continue; } function g() {}');
/// register_shutdown_function('unlink', $f);
/// include $f;
/// ```
/// ```expect exit=255
/// handler: "continue" targeting switch is equivalent to "break"
/// declared
///
/// Warning: "continue" targeting switch is equivalent to "break" in %s on line 1
///
/// Fatal error: Cannot redeclare function g() (previously declared in %s:2) in %s on line 1
/// ```
///
/// Before them, each auto-global the unit names is armed
/// ([`Unit::arms`]), as php's compiler arms one where it meets the name
/// and opcache arms a cached file's `ping_auto_globals_mask` on a hit
/// (`Z/ext/opcache/ZendAccelerator.c:1941`), so one made just in time is
/// in `EG(symbol_table)` before the unit's first line, where the file
/// names it and whether or not the line that names it is reached.
pub(crate) fn note_compiled(req: &mut Request, unit: &Arc<Unit>, kind: SourceKind) -> bool {
    // A request-only unit's names, which php's compile made as strings
    // of the request; the memory limit refusing them ends the request.
    if matches!(kind, SourceKind::Eval(_)) && req.hold_unit_names(unit).is_err() {
        return false;
    }
    // `zend_compile_halt_compiler`'s constant, and opcache's again on a
    // hit, under the file's mangled name, where the file has not
    // registered it already.
    if let Some(offset) = unit.halt_offset {
        let key = crate::api::request::halt_offset_key(&unit.main.filename.0);
        if req.constant_named(&key).is_none() {
            let offset = crate::api::values::Owner::int(offset);
            req.define_constant(&key, offset);
        }
    }
    for global in &unit.arms {
        // What a callback can raise is the memory limit, which the
        // loop meets again at its first allocation.
        req.arm(*global).ok();
    }
    // `compile_file` records what it raises and emits it once the
    // compile has declared the file's functions and early-bound its
    // classes; `compile_string` records nothing, so an `eval` raises as
    // it compiles, before what it declares.
    let recording = match kind {
        SourceKind::File => Some(req.begin_recording()),
        SourceKind::Eval(_) => None,
    };
    for raised in &unit.diagnostics {
        req.note_error_in(
            raised.level,
            &raised.message,
            &unit.main.filename.0,
            raised.line,
        );
    }
    // The observers are told what the load declares as it declares
    // it, where php tells them: as its compile declares, before the
    // compile's record is raised, or, where opcache serves the unit, as
    // its load copies the tables, after it raised the cached file's
    // warnings (`Z/ext/opcache/ZendAccelerator.c:1939-1945`).
    let mut load = req.class_load(unit);
    if load.accelerated()
        && let Some(recording) = recording
    {
        req.emit_recorded(recording);
    }
    let declared = req.declare_unit(unit, &mut load);
    if !load.accelerated()
        && let Some(recording) = recording
    {
        req.emit_recorded(recording);
    }
    declared
}

/// A program in `file` the compiler refused: `php_error_cb` renders it
/// where it renders every diagnostic, at the level the compiler refused
/// at, what it raised before first, and the request ends before a frame
/// is pushed. A refusal php throws as a `CompileError` is uncaught here,
/// so `zend_exception_error` renders it through `zend_error_cb` alone,
/// with no fatal's trace after it:
///
/// ```php
/// declare(encoding = M_PI);
/// ```
/// ```expect exit=255
/// Fatal error: Encoding must be a literal in %s on line 2
/// ```
pub(crate) fn refused_program(req: &mut Request, refused: &CompileError, file: &[u8]) {
    for raised in &refused.raised {
        req.note_error_in(raised.level, &raised.message, file, raised.line);
    }
    match (refused.stage, refused.thrown) {
        // Opened before anything compiled or ran: `zend_error` finds no
        // file compiling and none executing.
        (php_compile::Stage::Opening, _) => {
            req.end_in_compile_time(refused.level, &refused.message, b"Unknown", 0);
        }
        (php_compile::Stage::Compiling, true) => {
            req.end_in_uncaught(
                refused.level,
                refused.message.as_bytes(),
                file,
                refused.line,
            );
        }
        (php_compile::Stage::Compiling, false) => {
            req.end_in_compile_time(refused.level, &refused.message, file, refused.line);
        }
    }
}

/// `zend_execute_script(ZEND_REQUIRE, NULL, file)` over the file the
/// ini entry `name` gives, `auto_prepend_file` or `auto_append_file`
/// (`Z/main/main.c:2621-2645`): required as `require` resolves it, its
/// file held open until the program has been served
/// ([`Request::require_kept`]), and served where the program is, and
/// whether it ran to its end; an entry left empty is nothing to serve.
pub(crate) fn auto_file(req: &mut Request, name: &[u8], entry: Loop) -> bool {
    let Some(file) = req.ini_get(name).filter(|file| !file.is_empty()) else {
        return true;
    };
    let file = file.to_vec();
    match req.require_kept(&file) {
        Ok(crate::api::request::Opened::Unit(unit)) => serve_unit(req, &unit, entry),
        Ok(_) => true,
        Err(thrown) => {
            req.leave_pending(&thrown);
            req.uncaught().ok();
            false
        }
    }
}

/// The unit's main op array served on a fresh entry frame, what it
/// compiled to raised first, the frame attached to `EG(symbol_table)`,
/// which is how its `$argv` and `$argc` are the ones
/// `php_hash_environment` wrote there; the loop's fatal, if any, ends
/// the request.
///
/// The frame is left the way php leaves the main one, which is not the
/// way it leaves any other: its CVs are `EG(symbol_table)`, so
/// `zend_detach_symbol_table` moves them to the request before the pop
/// ([`Request::detach_symbol_table`]) and the pop frees a frame whose
/// CVs are all `UNDEF`. The variables are then the request's, through
/// the shutdown functions and the extensions' `rshutdown`, until
/// `shutdown_executor` destroys the table (design §5.10).
///
/// A fatal the loop raised leaves every frame above the main one
/// standing, as `zend_bailout` jumps past them; `_zend_bailout` sets
/// `EG(current_execute_data)` to null (`Z/Zend/zend.c:1268`), so they are
/// popped here, before the main one is left, and a shutdown function is
/// called from nothing.
///
/// ```php
/// set_time_limit(1);
/// register_shutdown_function(function () { debug_print_backtrace(); });
/// function spin() { while (true); }
/// spin();
/// ```
/// ```expect exit=255 wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 4
/// #0 [internal function]: {closure:%s:3}()
/// ```
///
/// Whether the program ran to its end: not a fatal, a refused
/// declaration or an `exit`, each of which php leaves by `zend_bailout`,
/// and not a throwable no handler took.
pub(crate) fn serve_unit(req: &mut Request, unit: &Arc<Unit>, entry: Loop) -> bool {
    if !note_compiled(req, unit, SourceKind::File) {
        return false;
    }
    let standing = req.vm().depth();
    let frame = match req.push_frame(Arc::clone(unit)) {
        Ok(frame) => frame,
        Err(fatal) => {
            req.end_in(&fatal);
            return false;
        }
    };
    if let Err(fatal) = entry(req, frame) {
        req.end_in(&fatal);
    }
    while req.vm().depth() > standing + 1 {
        req.pop_frame();
    }
    req.detach_symbol_table(frame);
    req.pop_frame();
    let exited = req.unwind_exit();
    // What the loop left pending for the user exception handler, served
    // with the main frame left (`zend_execute_scripts`).
    if req.is_pending() {
        match req.primary() {
            Primary::Wrapped => {
                req.uncaught().ok();
            }
            Primary::Bare => req.keep_uncaught(),
        }
    }
    !exited && !req.bailing_out() && req.ending().is_none()
}

/// Each extension's `rinit` in startup order, `zend_activate_modules`,
/// so the wrappers and filters each registers stand in the stream
/// layer's tables in the order php's `MINIT`s registered them, the
/// bundle started in php's order ([`sort_modules`]); the first that
/// raises ends the walk, as the
/// `zend_bailout` of php's fatal inside `RINIT` leaves
/// `zend_activate_modules` (#1775), and what it raised is handed back.
/// The pinned build has `zip`, which the bundle has not:
///
/// ```php
/// echo implode(',', array_diff(stream_get_wrappers(), ['zip'])), "\n", implode(',', stream_get_filters()), "\n";
/// ```
/// ```expect
/// https,ftps,compress.zlib,compress.bzip2,php,file,glob,data,http,ftp,phar
/// zlib.*,bzip2.*,convert.iconv.*,string.rot13,string.toupper,string.tolower,convert.*,consumed,dechunk
/// ```
fn serve_rinits(req: &mut Request, extensions: &[ExtensionEntry]) -> Result<(), Thrown> {
    let mut served = Ok(());
    for (at, extension) in extensions.iter().enumerate() {
        let Some(rinit) = extension.rinit else {
            continue;
        };
        served = req.serve_for(ExtensionId::at(at), rinit);
        if served.is_err() {
            break;
        }
    }
    served
}

/// The master value of the entry `name` among `ini` as
/// `zend_ini_get_value` reads it, the empty string where it has none
/// (php's `NULL`); `None` where php has no such entry.
fn master_of<'a>(ini: &'a [(Vec<u8>, Option<Vec<u8>>)], name: &[u8]) -> Option<&'a [u8]> {
    ini.iter()
        .find(|(n, _)| n.as_slice() == name)
        .map(|(_, v)| v.as_deref().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_heap::{Fatal, FrameRef};

    use crate::observer::Untold;
    use crate::probe::{Asked, Probe};
    use crate::tier::Site;

    /// One call the sink saw on a channel beside the body.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Saw {
        Log(Vec<u8>),
        Stderr(Vec<u8>),
    }

    /// Everything the request wrote, kept: the body as `Capture` keeps
    /// it behind the `test` feature, and the log and the error stream
    /// as the calls the sink saw, in the order it saw them.
    #[derive(Default)]
    struct Kept {
        body: Vec<u8>,
        seen: Vec<Saw>,
    }

    impl Kept {
        /// The log's entries alone, one per call.
        fn log(&self) -> Vec<Vec<u8>> {
            self.seen
                .iter()
                .filter_map(|saw| match saw {
                    Saw::Log(entry) => Some(entry.clone()),
                    Saw::Stderr(_) => None,
                })
                .collect()
        }

        /// The log as php's CLI writes it to stderr, `"%s\n"` per entry.
        fn logged(&self) -> String {
            let mut text = String::new();
            for entry in self.log() {
                text.push_str(&String::from_utf8_lossy(&entry));
                text.push('\n');
            }
            text
        }

        /// The error stream alone, as a host with an `error_log` file
        /// would have it, the log parted from it.
        fn stderr(&self) -> String {
            let mut text = String::new();
            for saw in &self.seen {
                if let Saw::Stderr(bytes) = saw {
                    text.push_str(&String::from_utf8_lossy(bytes));
                }
            }
            text
        }
    }

    impl OutputSink for Kept {
        fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
            self.body.extend_from_slice(bytes);
            Ok(())
        }
        fn flush(&mut self) {}
        fn send_headers(&mut self, _: &ResponseHead) {}
        fn log(&mut self, entry: &[u8]) {
            self.seen.push(Saw::Log(entry.to_vec()));
        }
        fn write_stderr(&mut self, bytes: &[u8]) {
            self.seen.push(Saw::Stderr(bytes.to_vec()));
        }
    }

    fn description(source: &str) -> RequestDescription {
        RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(b"witness.php".to_vec()),
            source: Some(source.as_bytes().to_vec()),
            ..RequestDescription::default()
        }
    }

    /// `sapi_send_headers` under a host whose `header_handler` takes the
    /// line: `expose_php`'s `X-Powered-By` laid at request start, and the
    /// default `Content-type` after it once the first body byte reaches
    /// the block; under the CLI's description the block
    /// carries nothing, since `sapi_cli_header_handler` answers zero and
    /// the line is freed (#289).
    #[test]
    fn the_default_content_type_reaches_a_web_host_s_block_and_not_the_cli_s() {
        let engine = Engine::cli_builder().build();
        let mut web = crate::test::Capture::default();
        engine.serve_into(
            RequestDescription {
                web: Some(WebRequest {
                    response_code: 200,
                    ..WebRequest::default()
                }),
                sapi_name: String::from("cgi-fcgi"),
                ..description("<?php echo 'x';")
            },
            &mut web,
        );
        assert_eq!(web.stdout, b"x");
        assert_eq!(
            web.headers,
            b"X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n"
        );
        let mut cli = crate::test::Capture::default();
        engine.serve_into(description("<?php echo 'x';"), &mut cli);
        assert_eq!(cli.stdout, b"x");
        assert_eq!(cli.headers, b"");
    }

    /// `__DIR__` where the request's name has no directory is the
    /// request's working directory, `VCWD_GETCWD` (#366): two requests
    /// in one process described with two directories read two, which is
    /// php's under ZTS and what design §3's threads need; the process's
    /// own directory is neither.
    #[test]
    fn dir_is_the_described_request_s_working_directory_not_the_process_s() {
        let engine = Engine::cli_builder().build();
        let dir_under = |cwd: &str| {
            let mut sink = Kept::default();
            engine.serve_into(
                RequestDescription {
                    cwd: PathBuf::from(cwd),
                    ..description("<?php echo __DIR__;")
                },
                &mut sink,
            );
            String::from_utf8_lossy(&sink.body).into_owned()
        };
        assert_eq!(dir_under("/srv/one"), "/srv/one");
        assert_eq!(dir_under("/srv/two"), "/srv/two");
    }

    /// `error_log=syslog` with no `request::Syslog` registered, as
    /// without ext/standard: the entry reaches the host's log (#1364).
    #[test]
    fn error_log_syslog_without_a_registered_syslog_reaches_the_host_s_log() {
        let engine = Engine::builder().ini("error_log", "syslog").build();
        let mut sink = Kept::default();
        let source = "<?php ini_set('log_errors', 1); ini_set('display_errors', 0); echo $u;";
        engine.serve_into(description(source), &mut sink);
        assert_eq!(
            sink.logged(),
            "PHP Warning:  Undefined variable $u in witness.php on line 1\n"
        );
    }

    fn served(engine: &Engine, source: &str) -> (String, Observables) {
        let mut sink = Kept::default();
        let observables = engine.serve_into(description(source), &mut sink);
        (
            String::from_utf8_lossy(&sink.body).into_owned(),
            observables,
        )
    }

    /// A unit compiled from source the host handed over is the request's
    /// alone, and so is every `eval`'s: their op arrays take runtime
    /// caches of the request's own and no map-pointer offset of the
    /// engine's, so `CG(map_ptr_last)` moves only for what the engine
    /// publishes, as php's moves only for what opcache persists, and a
    /// request that evaluates in a loop grows it by nothing (#528).
    #[test]
    fn a_unit_compiled_for_one_request_moves_the_engine_s_map_pointer_count_by_nothing() {
        let engine = Engine::cli_builder().build();
        let count = || engine.shared.map_ptr_last.load(Ordering::Relaxed);
        let before = count();
        let (body, _) = served(
            &engine,
            "<?php function f($s) { return strlen($s); } echo f('ab'), f('abc');\n\
             for ($i = 0; $i < 3; $i++) { echo eval('return $i;'); }",
        );
        assert_eq!(body, "23012");
        assert_eq!(count(), before, "the engine's count is where it was");
    }

    /// A file the unit cache compiles inside `opcache.file_update_protection`
    /// and holds nowhere is the request's alone, as php keeps a file it
    /// does not persist in process memory: requests that include it move
    /// the engine's map-pointer count by nothing, where each moved it by
    /// its op arrays (GH-8646, #2624). The same file held moves it once,
    /// at the request that held it.
    #[test]
    fn a_file_the_cache_holds_nowhere_moves_the_engine_s_map_pointer_count_by_nothing() {
        use crate::api::abi::{Admission, ExtensionBuilder, Lookup};
        let dir = std::env::temp_dir().join(format!("engine-map-ptr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let file = dir.join("declares.inc");
        std::fs::write(
            &file,
            "<?php class GhMapPtr {} function gh_f() {} return 1;",
        )
        .ok();
        let program = format!("<?php echo include '{}';", file.display());
        let cache = |admission: fn(&mut Request, &[u8]) -> Admission| {
            let extension = ExtensionBuilder::default().admission(admission).build("t");
            Engine::cli_builder().extensions(&[extension]).build()
        };
        let count = |engine: &Engine| engine.shared.map_ptr_last.load(Ordering::Relaxed);
        let protected = cache(|_, _| Admission::Held {
            validation: Validation::Never,
            lookup: Lookup::Resolved,
            protection: 3600,
            file_cache: None,
        });
        let before = count(&protected);
        let bodies: Vec<String> = (0..3).map(|_| served(&protected, &program).0).collect();
        assert_eq!(bodies, ["1", "1", "1"]);
        assert_eq!(count(&protected), before, "nothing held, nothing counted");
        let holding = cache(|_, _| Admission::Held {
            validation: Validation::Never,
            lookup: Lookup::Resolved,
            protection: 0,
            file_cache: None,
        });
        let before = count(&holding);
        assert_eq!(served(&holding, &program).0, "1");
        let after_first = count(&holding);
        assert_eq!(served(&holding, &program).0, "1");
        std::fs::remove_dir_all(&dir).ok();
        assert!(after_first > before, "the held file took its offsets");
        assert_eq!(count(&holding), after_first, "and took them once");
    }

    /// What the pinned oracle answers for
    /// `var_dump(ini_get('error_append_string'))` under
    /// `php_request_startup` starts the buffer the ini asks for before
    /// the program runs: the handler `output_handler` names, which must
    /// be a function by then, php's warning where it is not, or else
    /// `output_buffering`'s default handler.
    #[test]
    fn the_request_starts_the_buffer_output_handler_or_output_buffering_names() {
        let program = "<?php function h($s) { return '[' . $s . ']'; } echo 'x'; echo 'y';";
        let serve = |name: &str, value: &str| {
            let engine = Engine::cli_builder().ini(name, value).build();
            let out = engine.serve_capture(program, crate::test::Capture::default());
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        assert_eq!(
            serve("output_handler", "h"),
            "\nWarning: PHP Request Startup: function \"h\" not found or invalid function name in Unknown on line 0\nxy"
        );
        assert_eq!(serve("output_buffering", "4096"), "xy");
        assert_eq!(serve("output_handler", ""), "xy");
    }

    /// A startup diagnostic is its message's bytes: `extension_dir` set to
    /// `a/<0xE4>/w`, which a host's ini text can carry and `-d` does not
    /// yet (#1686), is written as php writes it,
    /// `php -n -d extension_dir=a/$'\xe4'/w -d extension=x -r ''` with
    /// the `dlopen` parentheses the engine cannot make.
    #[test]
    fn a_startup_diagnostic_writes_its_bytes() {
        let mut builder = Engine::cli_builder();
        builder
            .ini_entries
            .extend_from_slice(b"extension_dir=\"a/\xe4/w\"\nextension=x\n");
        let out = builder
            .build()
            .serve_capture("", crate::test::Capture::default());
        let expected: &[u8] = b"\nWarning: PHP Startup: Unable to load dynamic library 'x' (tried: a/\xe4/w/x (dynamic modules are not supported), a/\xe4/w/x.so (dynamic modules are not supported)) in Unknown on line 0\n";
        assert_eq!(out.stdout, expected);
    }

    /// `sapi_module.treat_data` replaced, as mbstring replaces it: the
    /// query string is handed to the extension's treater, whose variables
    /// are registered; a source it leaves, the cookie header here, is
    /// `php_default_treat_data`'s.
    #[test]
    fn a_treat_data_hook_reads_the_sources_it_takes_and_leaves_the_rest() {
        use crate::api::request::Input;
        let upper: crate::api::abi::TreatDataHook = |_, input, source| {
            Ok((input == Input::Get).then(|| vec![(b"q".to_vec(), source.to_ascii_uppercase())]))
        };
        let extension = crate::api::abi::ExtensionBuilder::default()
            .treat_data(upper)
            .build("t");
        let engine = Engine::cli_builder().extensions(&[extension]).build();
        let mut sink = crate::test::Capture::default();
        engine.serve_into(
            RequestDescription {
                web: Some(WebRequest {
                    query: Some(b"a=1&b=2".to_vec()),
                    cookies: Some(b"c=3".to_vec()),
                    ..WebRequest::default()
                }),
                sapi_name: String::from("cgi-fcgi"),
                ..description("<?php echo $_GET['q'], ' ', isset($_GET['a']) ? 'a' : '-', ' ', $_COOKIE['c'];")
            },
            &mut sink,
        );
        assert_eq!(sink.stdout, b"A=1&B=2 - 3");
    }

    /// `-d error_append_string=<value>`: `Off`, `OFF`, `oFf`, `FALSE`,
    /// `none`, `None`, `NULL` and `off ` are `string(0) ""`; `On`, `YES`
    /// and `true` are `string(1) "1"`; `0`, `2`, `abc`, `stderr`, `1M`,
    /// `of` and ` off` stand as written.
    #[test]
    fn a_host_s_ini_value_is_read_as_the_scanner_reads_a_bare_word() {
        let read = |value: &str| {
            let engine = Engine::cli_builder()
                .ini("error_append_string", value)
                .build();
            engine
                .shared
                .ini_default(b"error_append_string")
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        };
        for off in ["Off", "OFF", "oFf", "FALSE", "none", "None", "NULL", "off "] {
            assert_eq!(read(off).as_deref(), Some(""), "{off:?}");
        }
        for on in ["On", "YES", "true"] {
            assert_eq!(read(on).as_deref(), Some("1"), "{on:?}");
        }
        for written in ["0", "2", "abc", "stderr", "1M", "of", " off", ""] {
            assert_eq!(read(written).as_deref(), Some(written), "{written:?}");
        }
    }

    /// `zend_register_ini_entries_ex` sets each entry before it walks to
    /// the next, so a handler at startup sees the host's value of an
    /// entry declared before its own, and what it writes to the error
    /// stream is kept for the host to write before it serves, as
    /// opcache's handlers log a refused setting.
    #[test]
    fn a_startup_handler_sees_the_entries_before_it_and_its_stderr_is_kept() {
        let second: OnModify = |req, value| {
            let mut line = req.ini_get(b"t.first").unwrap_or_default().to_vec();
            line.extend_from_slice(b" then ");
            line.extend_from_slice(value.unwrap_or_default());
            line.push(b'\n');
            req.write_stderr(&line);
            Ok(true)
        };
        let first: OnModify = |_, _| Ok(true);
        let entry = |name: &str| php_unit::decl::IniDecl {
            name: Name(name.as_bytes().into()),
            default: Some(b"0"[..].into()),
            user: true,
            per_dir: true,
            system: true,
        };
        let extension = crate::api::abi::ExtensionBuilder::default()
            .ini(vec![entry("t.first"), entry("t.second")])
            .on_modify(b"t.first", first)
            .on_modify(b"t.second", second)
            .build("t");
        let engine = Engine::cli_builder()
            .extensions(&[extension])
            .ini("t.second", "2")
            .ini("t.first", "1")
            .build();
        assert!(engine.startup_errors().ends_with(b"1 then 2\n"));
    }

    /// Two extensions that intercept one native stack in bundle order,
    /// each at its `MINIT`: the later serves
    /// first and passes through to the earlier, the earlier to the
    /// native, and each hands on the call's own arguments.
    #[test]
    fn interceptions_of_one_native_stack_in_bundle_order() {
        let earlier: Entry = |req| {
            req.write(b"earlier ");
            crate::api::call::passthrough(req, None)
        };
        let later: Entry = |req| {
            req.write(b"later ");
            crate::api::call::passthrough(req, None)
        };
        let extensions = [
            crate::api::abi::ExtensionBuilder::default()
                .intercept(b"strlen", earlier)
                .build("earlier"),
            crate::api::abi::ExtensionBuilder::default()
                .intercept(b"strlen", later)
                .build("later"),
        ];
        let served = Engine::cli_builder()
            .extensions(&extensions)
            .build()
            .serve_capture(
                "<?php $f = 'strlen'; echo $f('abcd'), ' ', $f('ab');",
                crate::test::Capture::default(),
            );
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            "later earlier 4 later earlier 2"
        );
    }

    /// An extension that names a file to preload as the engine
    /// finishes starting up, reading it from an ini entry of its own,
    /// and holds every file in the cache, never validated, as opcache
    /// holds one with `opcache.validate_timestamps=0`.
    fn preloading(script: &Path) -> Extension {
        let entry = php_unit::decl::IniDecl {
            name: Name(b"t.preload"[..].into()),
            default: Some(b""[..].into()),
            user: false,
            per_dir: false,
            system: true,
        };
        crate::api::abi::ExtensionBuilder::default()
            .ini(vec![entry])
            .admission(|_, _| crate::api::abi::Admission::Held {
                validation: Validation::Never,
                lookup: crate::api::abi::Lookup::Resolved,
                protection: 0,
                file_cache: None,
            })
            .post_startup(|req, _| {
                let script = req.ini_get(b"t.preload").unwrap_or_default().to_vec();
                if script.is_empty() {
                    crate::api::abi::PostStartup::Serve
                } else {
                    crate::api::abi::PostStartup::Preload(script)
                }
            })
            .build(&script.to_string_lossy())
    }

    /// `opcache.preload` as the unit cache answers it (design §6.3): the
    /// file is served once, before the first request, which writes after
    /// it; what it declared stands in every request, a class it left
    /// unlinked is php's warning, and the file included again declares
    /// nothing the preload holds.
    #[test]
    fn a_preloaded_script_runs_once_and_its_declarations_stand_in_every_request() {
        let dir = std::env::temp_dir().join(format!("engine-preload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let script = dir.join("preload.inc");
        std::fs::write(
            &script,
            "<?php\necho \"preloading\\n\";\nfunction pre_f() { return 'f'; }\n\
             class PreA { function m() { return 'm'; } }\nclass PreB extends PreA {}\n\
             interface PreI {}\nclass PreC implements PreI {}\nreturn;\n\
             class Late extends Missing {}\n",
        )
        .ok();
        let engine = Engine::cli_builder()
            .extensions(&[preloading(&script)])
            .ini("t.preload", script.as_os_str().as_encoded_bytes())
            .build();
        let program = format!(
            "<?php echo pre_f(), (new PreB)->m(), (new PreC) instanceof PreI ? 'y' : 'n', \
             class_exists('Late', false) ? 'y' : 'n', \"\\n\"; include '{}'; echo \"again\\n\";",
            script.display()
        );
        let first = engine.serve_capture(&program, crate::test::Capture::default());
        let second = engine.serve_capture(&program, crate::test::Capture::default());
        let resolved = script.canonicalize().unwrap_or_default();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            String::from_utf8_lossy(&first.stdout),
            format!(
                "preloading\n\nWarning: Can't preload unlinked class Late: Unknown parent Missing in {} on line 9\nfmyn\npreloading\nagain\n",
                resolved.display()
            )
        );
        assert_eq!(
            String::from_utf8_lossy(&second.stdout),
            "fmyn\npreloading\nagain\n"
        );
    }

    /// An opcache restart past the preload (design §5.14): the next
    /// request is handed a new epoch, and the preload's classes and
    /// functions stand in it at the numbers they had, the very rows.
    #[test]
    fn a_restart_keeps_the_preloads_rows_at_their_numbers() {
        let dir =
            std::env::temp_dir().join(format!("engine-preload-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let script = dir.join("preload.inc");
        std::fs::write(
            &script,
            "<?php\nfunction pre_g() { return 'g'; }\nclass PreD { function m() { return 'd'; } }\n",
        )
        .ok();
        let engine = Engine::cli_builder()
            .extensions(&[preloading(&script)])
            .ini("t.preload", script.as_os_str().as_encoded_bytes())
            .build();
        let program = "<?php echo pre_g(), (new PreD)->m();";
        let first = engine.serve_capture(program, crate::test::Capture::default());
        let before = engine.shared.arena.current();
        let scheduled = before.files.schedule_restart();
        let second = engine.serve_capture(program, crate::test::Capture::default());
        let after = engine.shared.arena.current();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(scheduled, crate::api::request::Restart::Scheduled);
        assert_eq!(String::from_utf8_lossy(&first.stdout), "gd");
        assert_eq!(String::from_utf8_lossy(&second.stdout), "gd");
        assert!(!Arc::ptr_eq(&before, &after), "a new epoch");
        let preloaded = engine.shared.units.preloaded();
        let functions =
            preloaded.map_or_else(Vec::new, |preloaded| preloaded.functions.rows().to_vec());
        assert!(!functions.is_empty());
        for function in functions {
            let (was, is) = (before.function(function), after.function(function));
            assert!(was.zip(is).is_some_and(|(was, is)| std::ptr::eq(was, is)));
        }
    }

    /// An extension that keeps every file in the cache, never validated,
    /// and names a file cache from an ini entry of its own.
    fn file_caching(dir: &Path) -> Extension {
        let entry = php_unit::decl::IniDecl {
            name: Name(b"t.file_cache"[..].into()),
            default: Some(b""[..].into()),
            user: false,
            per_dir: false,
            system: true,
        };
        crate::api::abi::ExtensionBuilder::default()
            .ini(vec![entry])
            .admission(|req, _| {
                let dir = req.ini_get(b"t.file_cache").unwrap_or_default();
                crate::api::abi::Admission::Held {
                    validation: Validation::Never,
                    lookup: crate::api::abi::Lookup::Resolved,
                    protection: 0,
                    file_cache: Some(crate::api::abi::FileCache {
                        directory: std::path::PathBuf::from(
                            String::from_utf8_lossy(dir).into_owned(),
                        ),
                        read_only: false,
                    }),
                }
            })
            .build(&dir.to_string_lossy())
    }

    /// `opcache.file_cache` as the unit cache's second level (design
    /// §6.3): a file one engine compiled is kept on disk, and another
    /// engine of the same build reads it back instead of compiling the
    /// file, so what it serves is what was kept though the file has
    /// changed since; a kept file another build wrote is never read.
    #[test]
    fn a_kept_unit_is_read_back_by_the_next_engine_and_another_builds_is_not() {
        let dir = std::env::temp_dir().join(format!("engine-file-cache-{}", std::process::id()));
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).ok();
        let script = dir.join("kept.inc");
        std::fs::write(&script, "<?php return 'kept';").ok();
        let script = script.canonicalize().unwrap_or_default();
        let engine = || {
            Engine::cli_builder()
                .extensions(&[file_caching(&dir)])
                .ini("t.file_cache", cache.as_os_str().as_encoded_bytes())
                .build()
        };
        let program = format!("<?php echo include '{}';", script.display());
        let first = engine().serve_capture(&program, crate::test::Capture::default());
        std::fs::write(&script, "<?php return 'compiled';").ok();
        let second = engine().serve_capture(&program, crate::test::Capture::default());
        let kept = engine();
        let kept_at = crate::unit_cache::Disk {
            directory: &cache,
            read_only: false,
            build: &kept.shared.units.build,
        }
        .path(script.as_os_str().as_encoded_bytes());
        let mut stale = std::fs::read(&kept_at).unwrap_or_default();
        if let Some(byte) = stale.get_mut(8) {
            *byte ^= 1;
        }
        std::fs::write(&kept_at, &stale).ok();
        let third = kept.serve_capture(&program, crate::test::Capture::default());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(String::from_utf8_lossy(&first.stdout), "kept");
        assert_eq!(String::from_utf8_lossy(&second.stdout), "kept");
        assert_eq!(String::from_utf8_lossy(&third.stdout), "compiled");
    }

    /// A startup an extension refuses serves nothing, and every request
    /// ends with the status it named.
    #[test]
    fn a_refused_startup_serves_nothing() {
        let refusing = crate::api::abi::ExtensionBuilder::default()
            .post_startup(|req, _| {
                req.write_stderr(b"refused\n");
                crate::api::abi::PostStartup::Refuse { status: -2 }
            })
            .build("t");
        let engine = Engine::cli_builder().extensions(&[refusing]).build();
        let first = engine.serve_capture("<?php echo 1;", crate::test::Capture::default());
        let second = engine.serve_capture("<?php echo 2;", crate::test::Capture::default());
        assert_eq!(
            (
                first.stdout.as_slice(),
                first.stderr.as_slice(),
                first.exit_code
            ),
            (&b""[..], &b"refused\n"[..], -2)
        );
        assert_eq!((second.stderr.as_slice(), second.exit_code), (&b""[..], -2));
    }

    /// What the pinned oracle answers for `ini_get` under `-n` and one
    /// `-d` whose value carries a newline, so the text holds a section:
    /// after `[PATH=/x]` no entry reaches a directive, after `[foo]` or a
    /// bare `[PATH]` every one does. And a constant at startup is Zend's
    /// or `main`'s, a double one written at one digit as
    /// `zend_double_to_str` writes it before `precision` is registered,
    /// while `M_PI`, `ext/standard`'s, is not there yet and stands.
    #[test]
    fn the_startup_entries_are_read_as_php_init_config_reads_them() {
        let read = |name: &str, value: &str, probe: &[u8]| {
            let engine = Engine::cli_builder().ini(name, value).build();
            engine
                .shared
                .ini_default(probe)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        };
        assert_eq!(
            read("x", "1\n[PATH=/x]\nprecision=3", b"precision").as_deref(),
            Some("14")
        );
        assert_eq!(
            read("x", "1\n[foo]\nprecision=3", b"precision").as_deref(),
            Some("3")
        );
        assert_eq!(
            read("x", "1\n[PATH]\nprecision=3", b"precision").as_deref(),
            Some("3")
        );
        assert_eq!(
            read("error_log", "PHP_FLOAT_EPSILON", b"error_log").as_deref(),
            Some("2.0E-16")
        );
        assert_eq!(
            read("docref_root", "PHP_INT_SIZE | 1", b"docref_root").as_deref(),
            Some("9")
        );
        assert_eq!(
            read("precision", "M_PI", b"precision").as_deref(),
            Some("M_PI")
        );
    }

    /// The `[PATH=]` and `[HOST=]` tables as the pinned oracle keeps them:
    /// `php -n -d 'x=1<newline>[PATH=/w]…'` dumps `get_cfg_var('/w')` as
    /// `["precision"]=>"5"` and `["arr"]=>[0=>"a","k"=>"b"]`, a `name[]=`
    /// nested in the section's table; and php-cgi serving `/w/sub/t.php`
    /// for `SERVER_NAME=Example.com` reads `precision` 3 and
    /// `serialize_precision` 7, the host's table and then each directory
    /// from the root down, the nested array set as nothing.
    #[test]
    fn a_section_s_table_nests_its_arrays_and_activates_its_strings() {
        let text = "1\n[PATH=/w]\nprecision=5\narr[]=a\narr[k]=b\n\
                    [PATH=/w/sub/]\nprecision=3\n[HOST=Example.COM]\nserialize_precision=7";
        let engine = Engine::cli_builder().ini("x", text).build();
        let str = |s: &str| Configured::Str(s.as_bytes().to_vec());
        let name = |s: &str| ConfiguredKey::Name(s.as_bytes().to_vec());
        assert_eq!(
            engine.shared.configured(b"/w"),
            Some(&Configured::Array(vec![
                (name("precision"), str("5")),
                (
                    name("arr"),
                    Configured::Array(vec![
                        (ConfiguredKey::Index(0), str("a")),
                        (name("k"), str("b")),
                    ])
                ),
            ]))
        );
        let row = |n: &str, v: &str| {
            (
                n.as_bytes().to_vec(),
                v.as_bytes().to_vec(),
                IniOrigin::Admin,
            )
        };
        assert_eq!(
            engine.activated_ini(Some(b"Example.com"), b"/w/sub/t.php"),
            vec![
                row("serialize_precision", "7"),
                row("precision", "5"),
                row("precision", "3"),
            ]
        );
        assert_eq!(engine.activated_ini(Some(b"other"), b"sub/t.php"), vec![]);
        let none = Engine::cli_builder().ini("arr[]", "a").build();
        assert_eq!(none.activated_ini(Some(b"arr"), b"/arr/t.php"), vec![]);
    }

    /// What the pinned oracle answers under `-n -d disable_functions=<list>`
    /// for [`disabled_functions`]'s cases: a disabled native is no
    /// function to a call, a dynamic call, `function_exists()`,
    /// `get_defined_functions()` or a callback's check; the list splits
    /// at spaces and commas alone; a name is matched as the table's
    /// lowered key; an unknown one is passed over; `exit` and `die` stay,
    /// each with php's startup warning;
    /// and a user function may take a disabled native's name (Zend's
    /// `bug79382.phpt`). A list a host lays over the started engine, as
    /// php-fpm's child lays its pool's (`fpm_php_apply_defines_ex`), takes
    /// the same natives out and raises nothing a request shows (#2273).
    #[test]
    fn disable_functions_takes_its_natives_out_of_the_function_table() {
        const PROBE: &str = r#"<?php
            $listed = false;
            foreach (get_defined_functions()['internal'] as $name) { $listed = $listed || $name === 'strlen'; }
            var_dump(function_exists('strlen'), function_exists('defined'), function_exists('exit'), $listed);
            try { strlen('a'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
            try { $f = 'defined'; $f('A'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
            try { set_error_handler('strlen'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        "#;
        const UNDEFINED_STRLEN: &str = "Call to undefined function strlen()\n";
        const UNDEFINED_DEFINED: &str = "Call to undefined function defined()\n";
        const CALLBACK: &str = "set_error_handler(): Argument #1 ($callback) must be a valid callback or null, function \"strlen\" not found or invalid function name\n";
        let served_under = |list: &str| {
            let engine = Engine::cli_builder().ini("disable_functions", list).build();
            served(&engine, PROBE).0
        };
        let laid = |list: &str| {
            let mut engine = Engine::cli_builder().build();
            let entry = (b"disable_functions".to_vec(), list.as_bytes().to_vec());
            let laid = engine.alter_master_ini(vec![(entry.0, entry.1, IniOrigin::Admin)]);
            assert_eq!(laid, Ok(Vec::new()));
            engine
        };
        let answers = |strlen: bool, defined: bool| {
            let mut expected =
                format!("bool({strlen})\nbool({defined})\nbool(true)\nbool({strlen})\n");
            if !strlen {
                expected.push_str(UNDEFINED_STRLEN);
            }
            if !defined {
                expected.push_str(UNDEFINED_DEFINED);
            }
            if !strlen {
                expected.push_str(CALLBACK);
            }
            expected
        };
        for (list, strlen, defined) in [
            ("", true, true),
            ("strlen", false, true),
            (" strlen , defined", false, false),
            ("strlen,,defined", false, false),
            ("STRLEN", true, true),
            ("nosuchfn,strlen", false, true),
            ("\tstrlen", true, true),
            ("die,strlen", false, true),
            ("exit", true, true),
        ] {
            // `zend_disable_function`'s startup warning for a name it keeps.
            let mut kept = String::new();
            for name in list.split([' ', ',']) {
                if matches!(name, "exit" | "die" | "clone") {
                    kept.push_str("\nWarning: Cannot disable function ");
                    kept.push_str(name);
                    kept.push_str("() in Unknown on line 0\n");
                }
            }
            let expected = kept + &answers(strlen, defined);
            assert_eq!(served_under(list), expected, "{list:?}");
            let (out, _) = served(&laid(list), PROBE);
            assert_eq!(out, answers(strlen, defined), "laid {list:?}");
        }
        let taken = "<?php function strlen($x) { return 42; } var_dump(strlen('foobar'));";
        let engine = Engine::cli_builder()
            .ini("disable_functions", "strlen")
            .build();
        assert_eq!(served(&engine, taken).0, "int(42)\n");
        assert_eq!(served(&laid("strlen"), taken).0, "int(42)\n");
    }

    /// `fatal_error_backtraces` is `On` in php's table (`Z/Zend/zend.c:263`)
    /// and the CLI leaves it, so `php -n` over
    /// `<?php\nvar_dump(ini_get('fatal_error_backtraces'));` prints
    /// `string(1) "1"`; the recipe's `Off` is `run-tests.php`'s, which is
    /// why the witness beside `Request::fatal_error_backtraces` reads `""`.
    #[test]
    fn fatal_error_backtraces_is_on_by_default() {
        let engine = Engine::cli_builder().build();
        let (body, _) = served(
            &engine,
            "<?php\nvar_dump(ini_get('fatal_error_backtraces'));",
        );
        assert_eq!(body, "string(1) \"1\"\n");
    }

    /// The compiler's `E_COMPILE_ERROR` goes through `zend_error_impl`, so
    /// under php's default it fetches its trace, `#0 {main}` with no
    /// frame standing, and the parser's `E_PARSE` is a `ParseError`
    /// `zend_exception_error` renders with none. What the pinned oracle
    /// writes under `php -n` for `<?php\n(unset)$x;` is the fatal, then
    /// `Stack trace:` and `#0 {main}`, in the plain form and, under
    /// `-d html_errors=1`, in the second html format, the trace between
    /// `<br />` and the newline; and for `<?php\necho 1 +;` the parse
    /// error alone, under both.
    #[test]
    fn a_compile_error_carries_php_s_trace_under_the_default_ini() {
        let engine = Engine::cli_builder().build();
        let (body, observables) = served(&engine, "<?php\n(unset)$x;");
        assert_eq!(
            body,
            "\nFatal error: The (unset) cast is no longer supported in witness.php on line 2\nStack trace:\n#0 {main}\n"
        );
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  The (unset) cast is no longer supported in witness.php on line 2\nStack trace:\n#0 {main}"
            )
        );
        let (body, observables) = served(&engine, "<?php\necho 1 +;");
        assert_eq!(
            (body.as_str(), observables.exit_code),
            (
                "\nParse error: syntax error, unexpected token \";\" in witness.php on line 2\n",
                255
            )
        );

        let html = Engine::cli_builder().ini("html_errors", "1").build();
        let (body, _) = served(&html, "<?php\n(unset)$x;");
        assert_eq!(
            body,
            "<br />\n<b>Fatal error</b>:  The (unset) cast is no longer supported in <b>witness.php</b> on line <b>2</b><br />\nStack trace:\n#0 {main}\n"
        );
        let (body, _) = served(&html, "<?php\necho 1 +;");
        assert_eq!(
            body,
            "<br />\n<b>Parse error</b>:  syntax error, unexpected token &quot;;&quot; in <b>witness.php</b> on line <b>2</b><br />\n"
        );
    }

    /// A class the compiler bound with no opline (`zend_compile_class_decl`
    /// under `toplevel`) is declared into the request's table before the
    /// main op array runs, so the source the doc-comment witness of
    /// `php_syntax` declares runs (#257). A name the table already holds
    /// is no early bind: its `DECLARE_CLASS` raises
    /// `zend_class_redeclaration_error` where it stands, after what ran
    /// before it, in the wording for an internal class, or naming the
    /// earlier declaration's site for one of the file's. Both are
    /// `E_COMPILE_ERROR` at the class's line, over `#0 {main}` under
    /// php's default `fatal_error_backtraces`, which is what the pinned
    /// oracle writes for `<?php\necho 1;\nclass Exception {}` under
    /// `php -n`.
    #[test]
    fn a_unit_s_classes_are_declared_before_its_main_op_array_runs() {
        let engine = Engine::cli_builder().build();
        let (body, observables) = served(
            &engine,
            "<?php\nclass M { /** P */ public $x, $y; /** K */ const A = 1, B = 2; }\necho 1;",
        );
        assert_eq!((body.as_str(), observables.exit_code), ("1", 0));
        let (body, observables) = served(&engine, "<?php\necho 1;\nclass Exception {}");
        assert_eq!(
            (body.as_str(), observables.exit_code),
            (
                "1\nFatal error: Cannot redeclare class Exception in witness.php on line 3\nStack trace:\n#0 {main}\n",
                255
            )
        );
        let (body, observables) = served(&engine, "<?php\necho 1;\nclass M {}\nclass m {}");
        assert_eq!(
            (body.as_str(), observables.exit_code),
            (
                "1\nFatal error: Cannot redeclare class M (previously declared in witness.php:3) in witness.php on line 4\nStack trace:\n#0 {main}\n",
                255
            ),
            "php emits a DECLARE_CLASS for the second and fails it at run time"
        );
    }

    #[test]
    fn an_engine_with_no_extensions_serves_what_uses_zend_alone() {
        let engine = Engine::cli_builder().build();
        let (body, observables) = served(&engine, "<?php echo 1;");
        assert_eq!(body, "1");
        assert_eq!(observables.exit_code, 0);
        assert_eq!(observables.fatal, None);
    }

    /// A construct php accepts and this compiler cannot read yet is
    /// refused in the compiler's own sentence, which says which construct
    /// it could not read; a construct php refuses is php's `syntax
    /// error`, and the witness beside `serve` holds that to the oracle.
    /// The frame around either is php's.
    #[test]
    fn a_source_the_compiler_refuses_names_the_construct_it_could_not_read() {
        let engine = Engine::cli_builder().build();
        let (body, observables) = served(&engine, "<?php\ndeclare(ticks=0): enddeclare;");
        assert_eq!(
            body,
            "\nParse error: a `declare` in the alternative syntax this compiler cannot read yet in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);
        assert_eq!(
            observables.fatal, None,
            "a parse error is not the fatal `serve` renders a second time"
        );
    }

    /// A parse error is `error_reporting`'s and `log_errors`' to render,
    /// as every other diagnostic is; the status is php's either way.
    #[test]
    fn a_parse_error_is_logged_and_masked_the_way_php_masks_one() {
        let mut logged = Kept::default();
        let observables = Engine::cli_builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve_into(
                description("<?php\ndeclare(ticks=0): enddeclare;"),
                &mut logged,
            );
        assert!(logged.body.is_empty());
        assert_eq!(
            logged.logged(),
            "PHP Parse error:  a `declare` in the alternative syntax this compiler cannot read yet in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);

        let mut masked = Kept::default();
        let observables = Engine::cli_builder()
            .ini("error_reporting", "0")
            .build()
            .serve_into(
                description("<?php\ndeclare(ticks=0): enddeclare;"),
                &mut masked,
            );
        assert!(masked.body.is_empty(), "E_PARSE is masked out");
        assert_eq!(observables.exit_code, 255, "the status is php's regardless");
    }

    /// What php's compiler refuses after parsing is `E_COMPILE_ERROR`,
    /// rendered as a fatal and ending the request as one, with the log
    /// form beside it: the compiler's stack limit (design §5.6), which
    /// `zend.max_allowed_stack_size` bounds from where the request
    /// started, on the thread the request runs on. A chain of 200,000
    /// links is a tree that deep, and the frame that trips the limit
    /// is refused in php's sentence, with the number php prints, and
    /// nothing of the tree left to a native overflow, on the way down
    /// or on the way out. The witness beside `php_compile::StackLimit`
    /// holds the sentence to the oracle; what is held here is the
    /// frame the engine puts around it, on a thread of the test
    /// harness's own size.
    #[test]
    fn what_the_compiler_refuses_past_its_stack_limit_is_a_fatal() {
        let source = format!("<?php\n$y = 1{};\necho 'never';", "+1".repeat(200_000));
        let engine = Engine::cli_builder()
            .ini("zend.max_allowed_stack_size", "512K")
            .build();
        let (body, observables) = served(&engine, &source);
        // 512K less the least `zend.reserved_stack_size`: 491520 is the
        // oracle's number under `php -n` on macOS, where php reserves
        // 32K, and 475136 is php's where `ZEND_ALLOCA_MAX_SIZE` is
        // compiled and it reserves 48K (`php_compile::stack::RESERVED_MIN`).
        let number = if cfg!(target_vendor = "apple") {
            491_520
        } else {
            475_136
        };
        let sentence = format!(
            "Maximum call stack size of {number} bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached during compilation. Try splitting expression in witness.php on line 2\nStack trace:\n#0 {{main}}"
        );
        assert_eq!(body, format!("\nFatal error: {sentence}\n"));
        assert_eq!(observables.exit_code, 255);
        assert_eq!(
            observables.fatal.as_deref(),
            Some(format!("PHP Fatal error:  {sentence}").as_str()),
            "the log form the host is handed, as for every fatal, the trace with it"
        );

        let mut masked = Kept::default();
        let observables = Engine::cli_builder()
            .ini("zend.max_allowed_stack_size", "512K")
            .ini("error_reporting", "0")
            .build()
            .serve_into(description(&source), &mut masked);
        assert!(masked.body.is_empty(), "E_COMPILE_ERROR is masked out");
        assert_eq!(observables.exit_code, 255, "the status is php's regardless");

        let (body, observables) = served(
            &Engine::cli_builder()
                .ini("zend.max_allowed_stack_size", "-1")
                .build(),
            "<?php echo 1;",
        );
        assert_eq!(
            (body.as_str(), observables.exit_code),
            ("1", 0),
            "-1 checks nothing"
        );
    }

    /// `php_error_cb`'s two renderings of one diagnostic, which the two
    /// ini entries decide, each naming the line the opline stands on.
    /// The oracle writes the same bytes for `<?php echo NAN;` at
    /// `display_errors=1, log_errors=0` and at the two swapped.
    #[test]
    fn a_diagnostic_is_displayed_or_logged_as_the_ini_entries_say() {
        let mut displayed = Kept::default();
        declaring_nan(Engine::cli_builder())
            .build()
            .serve_into(description("<?php echo NAN;"), &mut displayed);
        assert_eq!(
            String::from_utf8_lossy(&displayed.body),
            "\nWarning: unexpected NAN value was coerced to string in witness.php on line 1\nNAN"
        );
        assert!(
            displayed.log().is_empty(),
            "log_errors is off, so nothing is logged"
        );

        let mut logged = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php echo NAN;"), &mut logged);
        assert_eq!(String::from_utf8_lossy(&logged.body), "NAN");
        assert_eq!(
            logged.logged(),
            "PHP Warning:  unexpected NAN value was coerced to string in witness.php on line 1\n"
        );
    }

    /// `builder` with `INF` and `NAN` among its persistent constants, as
    /// ext/standard declares them, which the bundle of these tests has
    /// not: what the diagnostics tests below echo to raise php's warning.
    fn declaring_nan(builder: EngineBuilder) -> EngineBuilder {
        let constant = |name: &str, value: f64| php_unit::decl::ConstDecl {
            name: Name::from(name),
            value: php_unit::decl::ConstExpr::Double(value),
            attributes: Vec::new(),
        };
        builder.additional_extension(
            crate::api::abi::ExtensionBuilder::default()
                .constants(vec![
                    constant("INF", f64::INFINITY),
                    constant("NAN", f64::NAN),
                ])
                .build("standard"),
        )
    }

    /// The warning php writes for `echo NAN;` on line 2 of the witness
    /// regime's file, which the display-branch tests below stand on.
    const NAN_WARNING: &str = "unexpected NAN value was coerced to string";

    /// `error_prepend_string` and `error_append_string` stand around
    /// php's plain display form and around nothing else: the log form
    /// carries neither. What the pinned oracle writes for
    /// `-d error_prepend_string='<PRE>' -d error_append_string='</PRE>'`
    /// over `<?php\necho NAN;` is
    /// `<PRE>\nWarning: … on line 2\n</PRE>NAN`.
    #[test]
    fn the_two_error_strings_stand_around_the_display_form() {
        let mut around = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("error_prepend_string", "<PRE>")
            .ini("error_append_string", "</PRE>")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php\necho NAN;"), &mut around);
        assert_eq!(
            String::from_utf8_lossy(&around.body),
            format!("<PRE>\nWarning: {NAN_WARNING} in witness.php on line 2\n</PRE>NAN")
        );
        assert_eq!(
            around.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "php's log form is the one rendering neither string reaches"
        );
    }

    /// `html_errors` is php's other display form, and the two strings
    /// stand around it as they stand around the plain one. What the
    /// pinned oracle writes for `-d html_errors=1` over
    /// `<?php\necho NAN;` is
    /// `<br />\n<b>Warning</b>:  … in <b>%s</b> on line <b>2</b><br />\n`.
    #[test]
    fn html_errors_is_phps_other_display_form() {
        let mut html = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("html_errors", "1")
            .build()
            .serve_into(description("<?php\necho NAN;"), &mut html);
        assert_eq!(
            String::from_utf8_lossy(&html.body),
            format!(
                "<br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\nNAN"
            )
        );

        let mut around = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("html_errors", "1")
            .ini("error_prepend_string", "<PRE>")
            .ini("error_append_string", "</PRE>")
            .build()
            .serve_into(description("<?php\necho NAN;"), &mut around);
        assert_eq!(
            String::from_utf8_lossy(&around.body),
            format!(
                "<PRE><br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\n</PRE>NAN"
            )
        );
    }

    /// `ignore_repeated_errors` drops a repeat of the last diagnostic,
    /// and `ignore_repeated_source` says whether a repeat at another
    /// line is one. What the pinned oracle writes for
    /// `<?php\necho NAN, NAN;\necho NAN;` is three warnings with both
    /// entries off, two with `ignore_repeated_errors=1` — the second on
    /// line 2 dropped, the one on line 3 kept — and one with
    /// `ignore_repeated_source=1` beside it.
    #[test]
    fn a_repeat_is_dropped_the_way_php_drops_one() {
        let source = "<?php\necho NAN, NAN;\necho NAN;";
        let warning =
            |line: u32| format!("\nWarning: {NAN_WARNING} in witness.php on line {line}\n");

        let mut every = Kept::default();
        declaring_nan(Engine::cli_builder())
            .build()
            .serve_into(description(source), &mut every);
        assert_eq!(
            String::from_utf8_lossy(&every.body),
            format!("{}NAN{}NAN{}NAN", warning(2), warning(2), warning(3))
        );

        let mut once_a_site = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("ignore_repeated_errors", "1")
            .build()
            .serve_into(description(source), &mut once_a_site);
        assert_eq!(
            String::from_utf8_lossy(&once_a_site.body),
            format!("{}NANNAN{}NAN", warning(2), warning(3)),
            "the repeat on line 2 is dropped and the one on line 3 stands"
        );

        let mut once = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("ignore_repeated_errors", "1")
            .ini("ignore_repeated_source", "1")
            .build()
            .serve_into(description(source), &mut once);
        assert_eq!(
            String::from_utf8_lossy(&once.body),
            format!("{}NANNANNAN", warning(2)),
            "the site no longer parts them, so one warning stands for all three"
        );
    }

    /// `xmlrpc_errors` is php's first display form, ahead of both the
    /// plain and the html one, and the two strings stand around neither
    /// it nor the log form. What the pinned oracle writes for
    /// `-d xmlrpc_errors=1 -d xmlrpc_error_number=42
    /// -d error_prepend_string='<PRE>'` over `<?php\necho NAN;` is one
    /// `methodResponse` fault, the code `42`, the message after
    /// `Warning:` with no space, and `NAN` straight after it.
    #[test]
    fn xmlrpc_errors_is_phps_first_display_form() {
        let mut fault = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("xmlrpc_errors", "1")
            .ini("xmlrpc_error_number", "42")
            .ini("error_prepend_string", "<PRE>")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php\necho NAN;"), &mut fault);
        assert_eq!(
            String::from_utf8_lossy(&fault.body),
            format!(
                "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>42</int></value></member><member><name>faultString</name><value><string>Warning:{NAN_WARNING} in witness.php on line 2</string></value></member></struct></value></fault></methodResponse>NAN"
            )
        );
        assert_eq!(
            fault.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "the log form is the same whichever display form is on"
        );
    }

    /// `display_errors=stderr` is the error stream under the hosts php
    /// names at `Z/main/main.c:1484`, `cli`, `cgi` and `phpdbg`, and
    /// the plain form on the body under every other. What the pinned
    /// oracle writes for `-d display_errors=stderr` over `<?php\necho
    /// NAN;` is `NAN` on stdout and `Warning: … on line 2` on stderr,
    /// with no leading newline; under `fpm-fcgi` php's test on
    /// `sapi_module.name` fails and the `else` is the plain form.
    #[test]
    fn stderr_is_the_error_stream_only_under_the_hosts_php_names() {
        let engine = declaring_nan(Engine::cli_builder())
            .ini("display_errors", "stderr")
            .build();
        let under = |host: &str| {
            let mut kept = Kept::default();
            engine.serve_into(
                RequestDescription {
                    sapi_name: String::from(host),
                    ..description("<?php\necho NAN;")
                },
                &mut kept,
            );
            (
                String::from_utf8_lossy(&kept.body).into_owned(),
                kept.stderr(),
            )
        };
        for host in ["cli", "cgi", "phpdbg"] {
            assert_eq!(
                under(host),
                (
                    String::from("NAN"),
                    format!("Warning: {NAN_WARNING} in witness.php on line 2\n")
                ),
                "{host}"
            );
        }
        assert_eq!(
            under("fpm-fcgi"),
            (
                format!("\nWarning: {NAN_WARNING} in witness.php on line 2\nNAN"),
                String::new()
            ),
            "another host falls to the plain form on the body"
        );
    }

    /// `display_errors` is read as `php_get_display_errors_mode`
    /// (`Z/main/main.c:492-517`) reads it: a number is `ZEND_ATOL` of the
    /// value narrowed to php's `uint8_t`, where `2` is the error stream,
    /// `0` is off and any other is the body, php's `STDOUT` fallback.
    /// Measured under the pin, `-d display_errors=2` over `<?php\necho
    /// NAN;` writes `NAN` alone to stdout and the warning to stderr, as
    /// ` 2` and `2x` do, `atoll` skipping the space and stopping at the
    /// `x`; `7` and `-1` write the plain form on the body; `256` narrows
    /// to `0` and writes `NAN` alone, and `258` narrows to `2`; a word
    /// that is none of php's five, `abc`, is `ZEND_ATOL`'s zero.
    #[test]
    fn a_numeric_display_errors_is_read_as_phps_uint8_mode() {
        let under = |value: &str| {
            let engine = declaring_nan(Engine::cli_builder())
                .ini("display_errors", value)
                .build();
            let mut kept = Kept::default();
            engine.serve_into(description("<?php\necho NAN;"), &mut kept);
            (
                String::from_utf8_lossy(&kept.body).into_owned(),
                kept.stderr(),
            )
        };
        let on_stderr = (
            String::from("NAN"),
            format!("Warning: {NAN_WARNING} in witness.php on line 2\n"),
        );
        let on_body = (
            format!("\nWarning: {NAN_WARNING} in witness.php on line 2\nNAN"),
            String::new(),
        );
        let off = (String::from("NAN"), String::new());
        for value in ["2", " 2", "2x", "258", "-254"] {
            assert_eq!(under(value), on_stderr, "{value:?} is the error stream");
        }
        for value in ["7", "-1", "1", "stdout", "on", "yes", "true"] {
            assert_eq!(under(value), on_body, "{value:?} is the body");
        }
        for value in ["0", "256", "abc", "", "off", "no", "false"] {
            assert_eq!(under(value), off, "{value:?} is off");
        }
    }

    /// The error stream and the log are two channels on the sink, so a
    /// host with an `error_log` file keeps the stderr form of a
    /// diagnostic off the file, and one diagnostic at
    /// `display_errors=stderr` and `log_errors=1` is two calls in
    /// php's order: `php_error_cb` logs before it displays
    /// (`Z/main/main.c:1459`, then `:1484`), and the stderr form ends in
    /// `\n` where the log entry ends in nothing. Under the CLI the two
    /// are one fd, and what it reads is
    /// `PHP Warning:  … line 2\nWarning: … line 2\n`, which is what the
    /// pinned oracle writes for `-d display_errors=stderr -d
    /// log_errors=1` over `<?php\necho NAN;`.
    #[test]
    fn the_stderr_form_and_the_log_entry_are_two_calls_in_phps_order() {
        let mut kept = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("display_errors", "stderr")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php\necho NAN;"), &mut kept);
        assert_eq!(String::from_utf8_lossy(&kept.body), "NAN");
        assert_eq!(
            kept.seen,
            vec![
                Saw::Log(
                    format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2").into_bytes()
                ),
                Saw::Stderr(
                    format!("Warning: {NAN_WARNING} in witness.php on line 2\n").into_bytes()
                ),
            ]
        );
    }

    /// A dropped repeat is dropped whole, php's `if (display)` around
    /// the store as well as the two renderings: it is not logged, and
    /// the diagnostic it repeats stays the one `error_get_last()`
    /// answers.
    #[test]
    fn a_dropped_repeat_is_neither_logged_nor_stored() {
        let mut logged = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("ignore_repeated_errors", "1")
            .ini("ignore_repeated_source", "1")
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php\necho NAN, NAN;"), &mut logged);
        assert_eq!(
            logged.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "one log line for the two warnings"
        );
    }

    /// The same request through the `Capture` sink, which is the shape
    /// every extension's own test reads (design §4.3 seam 4, §4.10).
    #[test]
    fn the_capture_sink_holds_what_a_zend_only_request_wrote() {
        let served = Engine::cli_builder()
            .build()
            .serve_capture("<?php echo 1;", crate::test::Capture::default());
        assert_eq!(served.stdout, b"1");
        assert_eq!(served.exit_code, 0);
        assert_eq!(served.fatal, None);
        let served = Engine::cli_builder().build().serve_capture(
            "<?php var_dump(extension_loaded('Core'));",
            crate::test::Capture::default(),
        );
        assert_eq!(served.stdout, b"bool(true)\n");
    }

    /// How many strings the engine has published past the ones its build
    /// interned for the constants it declares, which is what says a
    /// constant of a unit was interned once.
    fn interned(engine: &Engine) -> usize {
        let at_build = strings_of(&Engine::cli_builder().build());
        strings_of(engine).saturating_sub(at_build)
    }

    /// How many strings the engine's table holds.
    fn strings_of(engine: &Engine) -> usize {
        match engine.shared.strings.lock() {
            Ok(strings) => strings.count(),
            Err(poisoned) => poisoned.into_inner().count(),
        }
    }

    /// `Engine::cli_builder().build()` interns a string constant's name and
    /// value into its permanent table at build, as php's `MINIT` does,
    /// and no request grows the published table for reading one: that
    /// table holds the hosts' names `PHP_SAPI` is read from and nothing
    /// else, and each value of Core's longer than one byte is the
    /// permanent string, `PHP_EOL` being the frozen one-byte string.
    #[test]
    fn a_string_constant_is_interned_into_the_engine_at_build() {
        let engine = Engine::cli_builder().build();
        let hosts: std::collections::BTreeSet<&[u8]> = crate::constants::HOSTS
            .iter()
            .map(|name| name.as_bytes())
            .collect();
        assert_eq!(
            strings_of(&engine),
            hosts.len(),
            "the hosts' names, each once"
        );
        for decl in &engine.shared.constants {
            assert!(engine.shared.permanent(&decl.name.0).is_some());
        }
        let rows = (engine.shared.constants.iter()).zip(&engine.shared.constant_values);
        for (decl, row) in rows {
            let Some((value, _)) = row else { continue };
            let name = &decl.name.0;
            if let Some(value) = value.as_str()
                && value.len() > 1
            {
                assert!(
                    engine
                        .shared
                        .permanent(value.bytes())
                        .is_some_and(|permanent| permanent.is_same(value)),
                    "{} is its permanent string",
                    String::from_utf8_lossy(name)
                );
            }
        }
        let version = engine.shared.constant(b"PHP_VERSION");
        assert_eq!(
            version.as_ref().and_then(Value::as_str_bytes),
            Some(crate::api::request::PHP_VERSION.as_bytes())
        );
        assert_eq!(
            engine
                .shared
                .constant(b"PHP_EOL")
                .as_ref()
                .and_then(Value::as_str_bytes),
            Some(&b"\n"[..])
        );
        assert_eq!(interned(&engine), 0);
    }

    /// A unit published into the cache interns its constant strings once,
    /// and every request serving it reads the one `zend_string` (design
    /// §5.2, §5.14, §6.3): two requests over one file write the same
    /// bytes out of one allocation the engine owns, and the second
    /// compiles nothing. Beside the constant the table holds the file's
    /// name, which the unit's op arrays carry
    /// ([`php_compile::intern_names`]).
    #[test]
    fn two_requests_serving_one_file_share_the_one_constant_string() {
        let engine = Engine::cli_builder().build();
        let path = std::env::temp_dir().join(format!("dax-{}-shared.php", std::process::id()));
        assert!(std::fs::write(&path, b"<?php echo 'shared';").is_ok());
        let name = path.to_string_lossy().as_bytes().to_vec();
        for _ in 0..2 {
            let mut sink = Kept::default();
            let observables = engine.serve_into(
                RequestDescription {
                    sapi_name: String::from("cli"),
                    script_name: Some(name.clone()),
                    ..RequestDescription::default()
                },
                &mut sink,
            );
            assert_eq!(String::from_utf8_lossy(&sink.body), "shared");
            assert_eq!(observables.exit_code, 0);
        }
        assert_eq!(
            interned(&engine),
            2,
            "one string for the one constant and one for the file's name, however many requests read them"
        );
        // What the cache holds after both: one unit, and the constant
        // string its `ECHO` names is the one allocation.
        let (Some(Ok(one)), Some(Ok(again))) = (
            engine
                .shared
                .cached(&name, Path::new(""), StackLimit::UNCHECKED),
            engine
                .shared
                .cached(&name, Path::new(""), StackLimit::UNCHECKED),
        ) else {
            return;
        };
        assert!(Arc::ptr_eq(&one, &again), "one published unit");
        let named = |unit: &Arc<Unit>| {
            unit.main
                .literals
                .first()
                .copied()
                .and_then(php_heap::Zval::as_str)
        };
        let (Some(first), Some(second)) = (named(&one), named(&again)) else {
            return;
        };
        assert_eq!(first.bytes(), b"shared");
        assert!(first.is_interned(), "a published constant is interned");
        assert!(first.is_same(second), "one `zend_string`, read by both");
        std::fs::remove_file(&path).ok();
    }

    /// A tree of files for one test, under the system's temporary
    /// directory and named for the process and the test, served as the
    /// CLI serves `php main.php` from that directory; removed after.
    struct Files {
        dir: PathBuf,
    }

    impl Files {
        fn new(name: &str, files: &[(&str, &str)]) -> Files {
            let dir = std::env::temp_dir().join(format!("dax-{}-{name}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            assert!(std::fs::create_dir_all(&dir).is_ok());
            for (file, text) in files {
                assert!(std::fs::write(dir.join(file), text).is_ok());
            }
            Files { dir }
        }

        /// `php <dir>/<name>` in the tree's directory, the path as
        /// upstream's runner hands it: what the request wrote and how it
        /// ended.
        fn serve(&self, engine: &Engine, name: &str) -> (String, Observables) {
            let mut sink = Kept::default();
            let observables = engine.serve_into(
                RequestDescription {
                    sapi_name: String::from("cli"),
                    script_name: Some(self.dir.join(name).as_os_str().as_encoded_bytes().to_vec()),
                    cwd: self.dir.clone(),
                    ..RequestDescription::default()
                },
                &mut sink,
            );
            (
                String::from_utf8_lossy(&sink.body).into_owned(),
                observables,
            )
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// The once forms over a second file (#396): the file is served the
    /// first time and answers what it returned, and `true` every time
    /// after, from either once form, as the oracle prints for these
    /// files.
    #[test]
    fn a_once_form_serves_a_file_once_and_answers_true_after() {
        let files = Files::new(
            "once",
            &[
                ("b.php", "<?php\necho \"b\\n\"; return 7;\n"),
                (
                    "main.php",
                    "<?php\nvar_dump(include_once \"b.php\", include_once \"b.php\", require_once \"b.php\");\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::cli_builder().build(), "main.php");
        assert_eq!(body, "b\nint(7)\nbool(true)\nbool(true)\n");
        assert_eq!(observables.exit_code, 0);
    }

    /// An include inside a function reads and writes the function's
    /// variables (#396): `zend_rebuild_symbol_table` gives the frame a
    /// table, the included file attaches to it, and what it wrote to
    /// `$r` is in the function's CV when the include returns, as the
    /// oracle prints for these files.
    #[test]
    fn an_include_inside_a_function_shares_the_function_s_variables() {
        let files = Files::new(
            "func",
            &[
                (
                    "fb.php",
                    "<?php\necho \"p=$p\\n\"; $r = 3; return \"ok\";\n",
                ),
                (
                    "main.php",
                    "<?php\nfunction f($p) { $q = include \"fb.php\"; return \"$p|$q|$r\"; }\nvar_dump(f(\"z\"));\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::cli_builder().build(), "main.php");
        assert_eq!(body, "p=z\nstring(6) \"z|ok|3\"\n");
        assert_eq!(observables.exit_code, 0);
    }

    /// A variable an included file writes at the top level is the
    /// includer's, and stands in `EG(symbol_table)` after every name the
    /// main op array attached at its start (#396), so
    /// `shutdown_destructors`' reverse walk serves it first, then the
    /// main file's own in reverse: `d2`, `d3`, `d1`, as the oracle
    /// prints for these files.
    #[test]
    fn a_variable_an_included_file_wrote_is_destructed_in_php_s_order() {
        let files = Files::new(
            "dtor",
            &[
                ("db.php", "<?php\n$b = new A(2);\n"),
                (
                    "main.php",
                    "<?php\nclass A { public $n; function __construct($n) { $this->n = $n; } function __destruct() { echo \"d{$this->n}\\n\"; } }\n$a = new A(1);\ninclude \"db.php\";\n$c = new A(3);\necho \"end\\n\";\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::cli_builder().build(), "main.php");
        assert_eq!(body, "end\nd2\nd3\nd1\n");
        assert_eq!(observables.exit_code, 0);
    }

    /// A program served as php's phpdbg serves one
    /// ([`Primary::Bare`]): the throwable it leaves uncaught is kept
    /// for the host, its class, site and `__toString`, and nothing is
    /// rendered; the handler `set_exception_handler` set has it first,
    /// and what it takes is not kept; the file is no included file.
    /// The native a host's own extension declares, for the test below.
    #[php_macros::php_function]
    fn host_module_answer() -> i64 {
        42
    }

    /// An extension a host built itself starts with the bundle: its natives
    /// are callable and its name is a loaded extension's, the last one
    /// `get_loaded_extensions()` lists, as php lists phpdbg's.
    #[test]
    fn an_extension_the_host_built_starts_after_the_bundle() {
        let built = crate::api::abi::ExtensionBuilder::default()
            .functions(vec![host_module_answer::declared()])
            .build("phpdbg");
        let engine = Engine::cli_builder().additional_extension(built).build();
        let (body, _) = served(
            &engine,
            "<?php echo host_module_answer(), ' ', extension_loaded('phpdbg') ? 'loaded' : 'absent', ' '; foreach (get_loaded_extensions() as $last) {} echo $last;",
        );
        assert_eq!(body, "42 loaded phpdbg");
    }

    #[test]
    fn a_program_served_for_a_host_that_renders_the_uncaught_keeps_it() {
        let files = Files::new(
            "primary",
            &[
                (
                    "thrown.php",
                    "<?php\necho get_included_files() === [] ? \"none\\n\" : \"some\\n\";\nthrow new ErrorException(\"no\");\n",
                ),
                (
                    "handled.php",
                    "<?php\nset_exception_handler(function ($e) { echo \"took \", $e->getMessage(), \"\\n\"; });\nthrow new Exception(\"x\");\n",
                ),
            ],
        );
        let serve = |name: &str| {
            let mut sink = Kept::default();
            let observables = Engine::cli_builder().build().serve_into(
                RequestDescription {
                    sapi_name: String::from("phpdbg"),
                    script_name: Some(files.dir.join(name).as_os_str().as_encoded_bytes().to_vec()),
                    cwd: files.dir.clone(),
                    primary: Primary::Bare,
                    ..RequestDescription::default()
                },
                &mut sink,
            );
            (
                String::from_utf8_lossy(&sink.body).into_owned(),
                observables,
            )
        };
        let (body, observables) = serve("thrown.php");
        assert_eq!(body, "none\n");
        let uncaught = observables.uncaught.unwrap_or_default();
        assert_eq!(uncaught.class, b"ErrorException");
        assert_eq!(uncaught.line, 3);
        assert!(uncaught.file.ends_with(b"thrown.php"));
        assert!(
            uncaught.text.starts_with(b"ErrorException: no in "),
            "{uncaught:?}"
        );
        assert_eq!(observables.exit_code, 255);
        let (body, observables) = serve("handled.php");
        assert_eq!(body, "took x\n");
        assert_eq!(observables.uncaught, None);
    }

    /// A file the parser refuses is the `ParseError` the includer may
    /// catch, at the file's own line (#396); a plain `include` whose
    /// compile was refused is not in the included-files table, so a
    /// once form after it compiles the file again and the uncaught
    /// `ParseError` is rendered at its level, as the oracle prints for
    /// these files.
    #[test]
    fn a_parse_error_in_an_included_file_is_a_parse_error_the_includer_may_catch() {
        let files = Files::new(
            "parse",
            &[
                ("bad.php", "<?php\n$x = ;\n"),
                (
                    "main.php",
                    "<?php\ntry { include \"bad.php\"; } catch (ParseError $e) { echo $e->getMessage(), \" at line \", $e->getLine(), \"\\n\"; }\necho \"after\\n\";\nvar_dump(include_once \"bad.php\");\n",
                ),
            ],
        );
        let (body, observables) = files.serve(&Engine::cli_builder().build(), "main.php");
        let bad = files.dir.join("bad.php");
        assert_eq!(
            body,
            format!(
                "syntax error, unexpected token \";\" at line 2\nafter\n\nParse error: syntax error, unexpected token \";\" in {} on line 2\n",
                bad.canonicalize().unwrap_or(bad).display()
            )
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// What compiling a file raised is raised on every request that
    /// serves it, the cache hit as the miss, in the unit's own file and
    /// at the compiler's line, before the op array writes anything: what
    /// the oracle does with no cache, and what opcache does on a hit
    /// with `opcache.record_warnings` on. `error_reporting` set from
    /// the ini masks it as it masks a runtime warning, since the path
    /// is one.
    #[test]
    fn a_cached_unit_raises_what_compiling_it_raised_on_every_request() {
        let engine = declaring_nan(Engine::cli_builder()).build();
        let path = std::env::temp_dir().join(format!("dax-{}-nan.php", std::process::id()));
        assert!(std::fs::write(&path, b"<?php\necho 'a';\necho NAN . 'z';").is_ok());
        let name = path.to_string_lossy().as_bytes().to_vec();
        let described = || RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(name.clone()),
            ..RequestDescription::default()
        };
        // `php_execute_script` expands the primary file's path with its
        // links resolved, which a temporary directory behind a link shows.
        let expanded = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        for _ in 0..2 {
            let mut sink = Kept::default();
            engine.serve_into(described(), &mut sink);
            assert_eq!(
                String::from_utf8_lossy(&sink.body),
                format!(
                    "\nWarning: unexpected NAN value was coerced to string in {} on line 3\naNANz",
                    expanded.to_string_lossy()
                )
            );
        }
        let mut masked = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("error_reporting", "0")
            .build()
            .serve_into(described(), &mut masked);
        assert_eq!(String::from_utf8_lossy(&masked.body), "aNANz");
        std::fs::remove_file(&path).ok();
    }

    /// What `memory_get_usage()` reads (design §5.13) after a request
    /// that built strings: a request that compiles for itself interns
    /// into its own memory, and the next request starts where the first
    /// one did, because the first request's end took every byte of it
    /// (design §5.2). The native that asks is #165; the accountant it
    /// asks is this one.
    #[test]
    fn what_memory_get_usage_reads_starts_where_it_started_for_every_request() {
        let engine = Engine::cli_builder().build();
        let source = b"<?php echo 'a constant string long enough to be allocated';";
        let mut started_at = Vec::new();
        for _ in 0..2 {
            let mut req = Request::new(Arc::clone(&engine.shared), description(""));
            started_at.push(req.memory_usage(false));
            let compiled = compile(
                source,
                Origin {
                    filename: b"witness.php",
                    cwd: Path::new(""),
                    kind: SourceKind::File,
                    keys_before: 0,
                    request_classes: RequestClasses::default(),
                    multibyte: None,
                },
                CompileIni {
                    precision: 14,
                    short_open_tag: true,
                    assertions: true,
                    shebang: php_compile::Shebang::Read,
                    multibyte: false,
                },
                Declared::default(),
                StackLimit::UNCHECKED,
                req.heap_mut(),
            );
            assert!(compiled.is_ok(), "the source compiles");
            assert!(
                req.memory_usage(false) > 0,
                "what the request interned is the request's own memory"
            );
        }
        // The object store's table and the VM stack's first page, and
        // nothing else, before a line runs (design §5.4, §5.6): 1024
        // buckets of a pointer each, and `ZEND_VM_STACK_PAGE_SIZE`.
        assert_eq!(
            started_at,
            vec![8192 + 262_144, 8192 + 262_144],
            "every request starts where the one before it did"
        );
        assert_eq!(
            interned(&engine),
            0,
            "and the engine's table never heard of it"
        );
    }

    /// A source the host handed over is the request's alone, as php's
    /// `-r` and `eval` are: its constant strings are interned into the
    /// request's own memory and the engine's table never hears of them.
    #[test]
    fn what_the_host_handed_over_interns_into_the_request_and_not_the_engine() {
        let engine = Engine::cli_builder().build();
        let (body, _) = served(&engine, "<?php echo 'inline';");
        assert_eq!(body, "inline");
        assert_eq!(
            interned(&engine),
            0,
            "nothing a request compiled for itself outlives it"
        );
    }

    #[test]
    fn the_call_sequence_reaches_a_native_and_writes_what_it_returned() {
        let engine = Engine::cli_builder().build();
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('Core'));");
        assert_eq!(body, "bool(true)\n");
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('nope'));");
        assert_eq!(body, "bool(false)\n");
    }

    /// The four arithmetic opcodes end to end, each snippet the one its
    /// kernel's witness pins, so that what the fences claim is what this
    /// engine writes and not only what the oracle wrote.
    #[test]
    fn the_arithmetic_opcodes_write_what_their_witnesses_pin() {
        let engine = Engine::cli_builder().build();
        for (source, pinned) in [
            (
                "<?php echo 1 + (2 + extension_loaded('Core')), \"|\", 1.5 + (2.5 + extension_loaded('Core')), \"|\", \"1\" + (1 + extension_loaded('Core')), \"|\", 9223372036854775807 + (1 + extension_loaded('Core'));",
                "4|5|3|9.2233720368548E+18",
            ),
            (
                "<?php echo 1 - (2 - extension_loaded('Core')), \"|\", 1.5 - (2.5 - extension_loaded('Core')), \"|\", \"3\" - (1 + extension_loaded('Core')), \"|\", -9223372036854775807 - (1 + extension_loaded('Core'));",
                "0|0|1|-9.2233720368548E+18",
            ),
            (
                "<?php echo 2 * (3 * extension_loaded('Core')), \"|\", 1.5 * (2.5 * extension_loaded('Core')), \"|\", \"3\" * (1 + extension_loaded('Core')), \"|\", 4611686018427387904 * (1 + extension_loaded('Core'));",
                "6|3.75|6|9.2233720368548E+18",
            ),
            (
                "<?php echo 6 / (1 + extension_loaded('Core')), \"|\", 7.5 / (1.5 * extension_loaded('Core')), \"|\", \"7\" / (1 + extension_loaded('Core')), \"|\", (-9223372036854775807 - extension_loaded('Core')) / (0 - extension_loaded('Core'));",
                "3|5|3.5|9.2233720368548E+18",
            ),
        ] {
            let (body, observables) = served(&engine, source);
            assert_eq!(body, pinned, "{source}");
            assert_eq!(observables.fatal, None, "{source}");
        }
    }

    /// `div_function`: the divisor of zero php refuses to fold, so the
    /// opcode throws `DivisionByZeroError` where it stands, on line 1 of
    /// a source whose `<?php` and whose `echo` share it. The witness on
    /// [`crate::ops::div`] pins the oracle's rendering of the display branch;
    /// what this holds is the log branch's spelling of the same fatal,
    /// the status, and the double divisor the witness does not reach.
    #[test]
    fn a_division_by_zero_throws_php_s_error_where_the_opcode_stands() {
        let engine = Engine::cli_builder().build();
        let (_, observables) = served(&engine, "<?php echo 1/0;");
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  Uncaught DivisionByZeroError: Division by zero \
                 in witness.php:1\nStack trace:\n#0 {main}\n  thrown in witness.php on line 1"
            )
        );
        assert_eq!(observables.exit_code, 255);
        let (_, observables) = served(&engine, "<?php echo 1/0.0;");
        assert_eq!(
            observables
                .fatal
                .as_deref()
                .map(|text| text.contains("Uncaught DivisionByZeroError: Division by zero")),
            Some(true)
        );
    }

    /// `zend_binop_error`: an operand php will not read as a number is a
    /// `TypeError` naming php's types around the operator as written, and
    /// one with a number in front of it is the warning and the number.
    #[test]
    fn an_operand_php_refuses_to_read_as_a_number_is_php_s_type_error() {
        let engine = Engine::cli_builder().build();
        let (_, observables) = served(&engine, "<?php echo \"abc\" + extension_loaded('Core');");
        assert_eq!(
            observables.fatal.as_deref().map(|text| text
                .contains("Uncaught TypeError: Unsupported operand types: string + bool")),
            Some(true),
            "{:?}",
            observables.fatal
        );
        let (body, observables) = served(
            &engine,
            "<?php echo \"1abc\" + (1 + extension_loaded('Core'));",
        );
        assert_eq!(
            body,
            "\nWarning: A non-numeric value encountered in witness.php on line 1\n3"
        );
        assert_eq!(observables.fatal, None);
    }

    #[test]
    fn echo_writes_each_part_as_php_converts_it() {
        let engine = Engine::cli_builder().build();
        let (body, _) = served(
            &engine,
            "<?php echo 1, \"|\", true, \"|\", null, \"|\", 2.5;",
        );
        assert_eq!(body, "1|1||2.5");
    }

    /// `zend_exception_error` through `php_error_cb`: the four lines the
    /// witness beside [`Request::uncaught`] pins against the oracle, with
    /// the throw site in php's two spellings.
    #[test]
    fn a_call_of_a_name_nothing_declares_ends_the_request_as_php_names_it() {
        let engine = Engine::cli_builder().build();
        let (body, observables) = served(&engine, "<?php\nnope();");
        assert_eq!(
            body,
            "\nFatal error: Uncaught Error: Call to undefined function nope() \
             in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2\n"
        );
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
                 in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
            )
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// A throwable a native left pending names the native's call as its
    /// site, and the native's own frame stands above `{main}`.
    #[test]
    fn a_throwable_a_native_left_pending_names_the_natives_call() {
        let engine = Engine::cli_builder().build();
        let (body, observables) = served(&engine, "<?php\nvar_dump();");
        assert_eq!(
            body,
            "\nFatal error: Uncaught ArgumentCountError: var_dump() expects at least 1 argument, \
             0 given in witness.php:2\nStack trace:\n#0 witness.php(2): var_dump()\n#1 {main}\n  \
             thrown in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// The fatal is `php_error_cb`'s to render, once, on the streams the
    /// two ini entries name; `serve` reports its log form and writes
    /// nothing of its own.
    #[test]
    fn a_fatal_is_displayed_or_logged_as_the_ini_entries_say() {
        let mut logged = Kept::default();
        let observables = Engine::cli_builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php\nnope();"), &mut logged);
        assert!(logged.body.is_empty(), "display_errors is off");
        assert_eq!(
            logged
                .log()
                .first()
                .map(|entry| String::from_utf8_lossy(entry).into_owned()),
            observables.fatal,
            "what the log holds is the fatal, once"
        );
        assert_eq!(logged.log().len(), 1, "the four lines are one entry");

        let mut masked = Kept::default();
        Engine::cli_builder()
            .ini("error_reporting", "0")
            .build()
            .serve_into(description("<?php\nnope();"), &mut masked);
        assert!(masked.body.is_empty(), "E_ERROR is masked out");
        assert!(masked.log().is_empty(), "log_errors is off");
    }

    /// `php_error_cb` builds one diagnostic with one `spprintf` and gives
    /// it to `php_log_err` once, the backtrace inside it, and
    /// `php_log_err_with_severity` stamps one date on it and writes it
    /// as one message (`Z/main/main.c:1459`). So the sink sees one call
    /// for the four-line fatal, and one call per warning for two
    /// warnings, with no terminator on any of them.
    #[test]
    fn the_sink_is_given_one_call_per_diagnostic_whatever_its_lines() {
        let mut fatal = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php\nnope();"), &mut fatal);
        assert_eq!(
            fatal.log(),
            vec![
                b"PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
                   in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
                    .to_vec()
            ],
            "four lines, one call"
        );

        let mut warned = Kept::default();
        declaring_nan(Engine::cli_builder())
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve_into(description("<?php\necho NAN;\necho NAN;"), &mut warned);
        assert_eq!(
            warned.log(),
            vec![
                format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2").into_bytes(),
                format!("PHP Warning:  {NAN_WARNING} in witness.php on line 3").into_bytes(),
            ],
            "two lines, two calls"
        );
    }

    #[test]
    fn an_opcode_with_no_handler_ends_the_request_naming_it() {
        let engine = Engine::cli_builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = Unit {
            main: php_unit::OpArray {
                oplines: vec![php_unit::Opline {
                    opcode: php_unit::Opcode::UserOpcode,
                    ..php_unit::Opline::default()
                }]
                .into_boxed_slice(),
                ..php_unit::OpArray::default()
            },
            ..Unit::default()
        };
        let frame = req.pushed(unit.published());
        assert_eq!(
            crate::tier::execute::<Nop, Untold>(&mut req, frame),
            Err(Fatal::error(
                "InternalError: ZEND_USER_OPCODE has no handler in this engine yet"
            ))
        );
    }

    /// Design §4.3 seam 8: `Site::Entry` is raised for the entry frame of
    /// an `execute` whatever the caller is, and `Site::Return` for no
    /// entry frame, which the stack's entry slot says.
    #[test]
    fn the_loop_asks_the_tier_at_the_entry_and_at_no_entry_frames_return() {
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = engine
            .unit(b"<?php echo 1;", b"witness.php")
            .unwrap_or_default();
        let unit = Unit::clone(&unit);
        let frame = req.pushed(unit.published());
        assert_eq!(
            crate::tier::execute::<Probe, Untold>(&mut req, frame),
            Ok(())
        );
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Entry, frame)],
            "the entry frame is activated, and its own return raises no site"
        );
    }

    /// Design §5.6, §8: a user frame is activated after its `RECV*`
    /// prologue, so `Probe` sees `(Entry, callee)` once per call and
    /// `Return` once per callee frame; a call whose `RECV` throws is
    /// reported `Threw` before the activation, so no `Entry` is raised
    /// for it (#309).
    #[test]
    fn a_call_activates_its_frame_once_and_a_recv_that_throws_never_does() {
        let engine = Engine::cli_builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = engine
            .unit(
                b"<?php function f($a, $b = 1) { return $a; } f(1); f(2, 3); f();",
                b"witness.php",
            )
            .unwrap_or_default();
        req.declare_functions(&unit);
        let frame = req.pushed(unit);
        let outcome = crate::tier::execute::<Probe, Untold>(&mut req, frame);
        assert!(
            outcome.is_ok() && req.is_pending(),
            "the third call's RECV throws and nothing catches it: it stays pending for the caller"
        );
        let callee = FrameRef(frame.0 + 1);
        assert_eq!(
            req.state::<Asked>().sites,
            vec![
                (Site::Entry, frame),
                (Site::Entry, callee),
                (Site::Return(crate::tier::SafepointIdx(1)), callee),
                (Site::Entry, callee),
                (Site::Return(crate::tier::SafepointIdx(1)), callee),
            ],
            "two calls activated and returned, the third never activated"
        );
    }

    /// `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`), which is
    /// how `opcache.opt_debug_level=0x10000` is a bit and not a zero: a
    /// base prefix is read as `strtoul` with base zero reads it, a
    /// leading `0` before a digit is octal, a `K`, `M` or `G` multiplies,
    /// and every shape php interprets as zero for backward compatibility
    /// is zero.
    #[test]
    fn a_whole_number_ini_value_is_read_as_php_s_quantity_reads_it() {
        let read = |value: &str| ini_quantity(value.as_bytes());
        assert_eq!(
            read("0x10000"),
            0x1_0000,
            "the dump bit, as the pin writes it"
        );
        assert_eq!(
            read("  0X10 "),
            16,
            "whitespace either side, and either case"
        );
        assert_eq!(read("65536"), 65536, "the same number in decimal");
        assert_eq!(read("0777"), 511, "a leading zero before a digit is octal");
        assert_eq!(read("0o17"), 15, "and the explicit octal prefix");
        assert_eq!(read("0b101"), 5, "and the binary one");
        assert_eq!(read("-1"), -1, "the sign is read before the digits");
        assert_eq!(read("2M"), 2 << 20, "a multiplier over decimal digits");
        assert_eq!(read("0"), 0, "a bare zero is no prefix at all");
        assert_eq!(read(""), 0, "nothing at all");
        assert_eq!(read("off"), 0, "no valid leading digits");
        assert_eq!(read("0z1"), 0, "an unknown prefix");
        assert_eq!(read("0x"), 0, "no digits after the prefix");
        assert_eq!(read("09"), 0, "octal that holds no octal digit");
    }

    /// Design §6.1: what php prints on the second stream before the
    /// program runs is the op arrays as the compiler left them, and it
    /// prints them only where `opcache.opt_debug_level` carries the
    /// `0x10000` bit. The bytes are held to the oracle's by
    /// `xtask compile-identity`, which runs both binaries over one file;
    /// what is held here is the switch.
    #[test]
    fn the_dump_before_the_optimizer_is_the_op_arrays_under_the_ini_bit_and_nothing_without_it() {
        // The entry is opcache's, and php has none without it.
        let opcache = || {
            let entry = php_unit::decl::IniDecl {
                name: Name(b"opcache.opt_debug_level"[..].into()),
                default: Some(b"0"[..].into()),
                user: false,
                per_dir: false,
                system: true,
            };
            let extension = crate::api::abi::ExtensionBuilder::default()
                .ini(vec![entry])
                .build("t");
            Engine::cli_builder().extensions(&[extension])
        };
        let described = description("<?php echo 1;");
        assert_eq!(
            Engine::cli_builder()
                .ini("opcache.opt_debug_level", "0x10000")
                .build()
                .dump_before_optimizer(&described),
            None,
            "no opcache, and so no entry"
        );
        assert_eq!(
            opcache().build().dump_before_optimizer(&described),
            None,
            "no opt_debug_level at all"
        );
        assert_eq!(
            opcache()
                .ini("opcache.opt_debug_level", "0")
                .build()
                .dump_before_optimizer(&described),
            None,
            "the entry at its default"
        );
        let dumped = opcache()
            .ini("opcache.opt_debug_level", "0x10000")
            .build()
            .dump_before_optimizer(&described)
            .unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&dumped),
            "\n$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; witness.php:1-1\n     ; return  [] RANGE[0..0]\n0000 ECHO int(1)\n0001 RETURN int(1)\n",
            "the one op array, in php's format"
        );
        assert_eq!(
            opcache()
                .ini("opcache.opt_debug_level", "0x10000")
                .build()
                .dump_before_optimizer(&description("<?php echo 1 +;")),
            None,
            "a source the compiler refused is no dump, as php's empty second stream is"
        );
    }
}
