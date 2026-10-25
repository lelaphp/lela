//! `file.c`'s natives (`Z/ext/standard/file.c`) over the engine's
//! streams facet (design §4.3 seam 5), which is where
//! `php_stream_open_wrapper_ex`, `php_stream_read` and their siblings
//! live, beside the directory natives of `dir.c` and
//! `stream_get_meta_data` of `streamsfuncs.c`, the two that read the same
//! facet. In the stub's order (`basic_functions.stub.php`): `opendir`,
//! `closedir`, `rewinddir` and `readdir`; `rewind`, `fclose`, `feof`,
//! `fgetc`, `fgets`, `fread`, `fopen`, `ftruncate`, `fstat`, `fseek`,
//! `ftell`, `fflush`, `fwrite` and its alias `fputs`,
//! `file_get_contents` and `file_put_contents`; then
//! `stream_get_meta_data`; then the natives over the wrapper operations
//! past opening, `rmdir`, `umask`, `mkdir`, `rename`, `copy`, `tempnam`,
//! `tmpfile` and `unlink`; then the rest of `file.c`'s, `flock` to
//! `sys_get_temp_dir`. A `$stream` parameter is `abi::Stream`,
//! `PHP_Z_PARAM_STREAM`: a resource, and one that is an open stream.
//! `file.stub.php`'s constants are all here, once, in its order.
//!
//! The natives declared here, with the untyped `$stream` the stub
//! declares and `fopen`'s return left undeclared, as php declares
//! them, so `ReflectionFunction::__toString` renders each as the oracle
//! does.
//!
//! ```php
//! echo new ReflectionFunction('fopen'), new ReflectionFunction('fwrite'), new ReflectionFunction('fputs');
//! ```
//! ```expect
//! Function [ <internal:standard> function fopen ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> string $filename ]
//!     Parameter #1 [ <required> string $mode ]
//!     Parameter #2 [ <optional> bool $use_include_path = false ]
//!     Parameter #3 [ <optional> $context = null ]
//!   }
//! }
//! Function [ <internal:standard> function fwrite ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> string $data ]
//!     Parameter #2 [ <optional> ?int $length = null ]
//!   }
//!   - Return [ int|false ]
//! }
//! Function [ <internal:standard> function fputs ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> string $data ]
//!     Parameter #2 [ <optional> ?int $length = null ]
//!   }
//!   - Return [ int|false ]
//! }
//! ```
//!
//! The rest of `file.c`'s natives, as the stub declares them. The
//! witness leaves out `fputcsv` and `fgetcsv`, whose defaults `"\""`
//! and `"\\"` the engine's Reflection prints with their escapes undone.
//!
//! ```php
//! foreach (['flock', 'get_meta_tags', 'readfile', 'fscanf', 'fpassthru', 'fsync', 'fdatasync', 'realpath', 'fnmatch', 'sys_get_temp_dir'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function flock ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> int $operation ]
//!     Parameter #2 [ <optional> &$would_block = null ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function get_meta_tags ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $filename ]
//!     Parameter #1 [ <optional> bool $use_include_path = false ]
//!   }
//!   - Return [ array|false ]
//! }
//! Function [ <internal:standard> function readfile ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> string $filename ]
//!     Parameter #1 [ <optional> bool $use_include_path = false ]
//!     Parameter #2 [ <optional> $context = null ]
//!   }
//!   - Return [ int|false ]
//! }
//! Function [ <internal:standard> function fscanf ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> string $format ]
//!     Parameter #2 [ <optional> mixed &...$vars ]
//!   }
//!   - Return [ array|int|false|null ]
//! }
//! Function [ <internal:standard> function fpassthru ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $stream ]
//!   }
//!   - Return [ int ]
//! }
//! Function [ <internal:standard> function fsync ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $stream ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function fdatasync ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $stream ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function realpath ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $path ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function fnmatch ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> string $pattern ]
//!     Parameter #1 [ <required> string $filename ]
//!     Parameter #2 [ <optional> int $flags = 0 ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function sys_get_temp_dir ] {
//!
//!   - Parameters [0] {
//!   }
//!   - Return [ string ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, Passed, Path, Ref, Union, Untyped, Variadic};
use php_ext::attr::{php_const, php_function};
use php_ext::call::lookup_class;
use php_ext::coerce::{to_string, with_string};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, raise, type_error};
use php_ext::request::{
    Ini, MAXPATHLEN, OptionAnswer, OwnedStream as _, Request, Resolve, Stream, StreamOption,
    Streams as _, Whence, WrapperOp,
};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Resource, Value, ValueRef};

/// `file.c`'s `auto_detect_line_endings`, `PHP_INI_ALL` and off, which
/// every stream made while it is on starts detecting its line endings
/// with (the engine's `_php_stream_alloc`).
pub(crate) fn ini() -> Vec<IniDecl> {
    vec![IniDecl {
        name: Name(b"auto_detect_line_endings".as_slice().into()),
        default: Some(b"0".as_slice().into()),
        user: true,
        per_dir: true,
        system: true,
    }]
}

/// `OnUpdateAutoDetectLineEndings` (`Z/ext/standard/file.c:143`): a value
/// that turns it on is php's deprecation, raised by `zend_error` and so
/// with no function before it, `in Unknown on line 0` at startup.
///
/// ```php
/// var_dump(ini_set('auto_detect_line_endings', '1'), ini_get('auto_detect_line_endings'), ini_set('auto_detect_line_endings', '0'));
/// ```
/// ```expect
/// Deprecated: auto_detect_line_endings is deprecated in %s on line 2
/// string(1) "0"
/// string(1) "1"
/// string(1) "1"
/// ```
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::on_modify`'s signature
pub(crate) fn on_auto_detect_line_endings(
    req: &mut Request,
    value: Option<&[u8]>,
) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    if Ini::parse_bool(value) {
        raise(
            req,
            Level::Deprecated,
            "auto_detect_line_endings is deprecated",
        );
    }
    Ok(true)
}

use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;

use crate::State;
use crate::ffi::locale::{self as c, Class};
use crate::streamsfuncs::{NullContext, context_of};

use constants::{
    FILE_APPEND, FILE_IGNORE_NEW_LINES, FILE_NO_DEFAULT_CONTEXT, FILE_SKIP_EMPTY_LINES,
    FILE_USE_INCLUDE_PATH, LOCK_NB, LOCK_UN,
};

/// `flock(2)`'s `LOCK_EX`, what `file_put_contents` hands
/// `php_stream_lock` and tests its `$flags` for.
const FLOCK_EX: i64 = 2;

/// The constants `file.stub.php` declares that the natives here read,
/// in its order, the two 8.1 deprecated among them.
///
/// ```php
/// var_dump(SEEK_SET, SEEK_CUR, SEEK_END, LOCK_SH, LOCK_EX, LOCK_UN, LOCK_NB, FILE_USE_INCLUDE_PATH, FILE_IGNORE_NEW_LINES, FILE_SKIP_EMPTY_LINES, FILE_APPEND, FILE_NO_DEFAULT_CONTEXT, FNM_NOESCAPE, FNM_PATHNAME, FNM_PERIOD, FNM_CASEFOLD);
/// var_dump(FILE_TEXT, FILE_BINARY);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(1)
/// int(2)
/// int(4)
/// int(8)
/// int(16)
/// int(1)
/// int(2)
/// int(4)
/// int(16)
///
/// Deprecated: Constant FILE_TEXT is deprecated since 8.1, as the constant has no effect in %s on line 3
///
/// Deprecated: Constant FILE_BINARY is deprecated since 8.1, as the constant has no effect in %s on line 3
/// int(0)
/// int(0)
/// ```
///
/// The `STREAM_*` constants the streams natives read or hand back,
/// with this host's values where the stub takes a header's.
///
/// ```php
/// var_dump(STREAM_NOTIFY_CONNECT, STREAM_NOTIFY_RESOLVE, STREAM_NOTIFY_SEVERITY_ERR, STREAM_FILTER_ALL, STREAM_CLIENT_CONNECT, STREAM_CRYPTO_METHOD_ANY_CLIENT, STREAM_CRYPTO_METHOD_TLSv1_3_SERVER, STREAM_CRYPTO_PROTO_TLSv1_2, STREAM_SHUT_RDWR, STREAM_PF_INET6, STREAM_PF_UNIX, STREAM_IPPROTO_RAW, STREAM_SOCK_SEQPACKET, STREAM_SOCK_RDM, STREAM_PEEK, STREAM_OOB, STREAM_SERVER_BIND, STREAM_SERVER_LISTEN);
/// ```
/// ```expect
/// int(2)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(127)
/// int(64)
/// int(32)
/// int(2)
/// int(30)
/// int(1)
/// int(255)
/// int(5)
/// int(4)
/// int(2)
/// int(1)
/// int(4)
/// int(8)
/// ```
///
/// The socket constants, with the values this host's `<sys/socket.h>`
/// and php's headers give them, and `default_socket_timeout`.
///
/// ```php
/// var_dump(STREAM_CLIENT_PERSISTENT, STREAM_CLIENT_ASYNC_CONNECT, STREAM_CLIENT_CONNECT, STREAM_CRYPTO_METHOD_ANY_CLIENT, STREAM_CRYPTO_METHOD_TLS_CLIENT, STREAM_CRYPTO_METHOD_TLS_SERVER, STREAM_CRYPTO_PROTO_TLSv1_3, STREAM_SHUT_RD, STREAM_SHUT_WR, STREAM_SHUT_RDWR, STREAM_PF_INET, STREAM_PF_INET6, STREAM_PF_UNIX, STREAM_IPPROTO_IP, STREAM_IPPROTO_TCP, STREAM_IPPROTO_UDP, STREAM_IPPROTO_ICMP, STREAM_IPPROTO_RAW, STREAM_SOCK_STREAM, STREAM_SOCK_DGRAM, STREAM_SOCK_RAW, STREAM_SOCK_SEQPACKET, STREAM_SOCK_RDM, STREAM_PEEK, STREAM_OOB, STREAM_SERVER_BIND, STREAM_SERVER_LISTEN, ini_get('default_socket_timeout'));
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(4)
/// int(127)
/// int(121)
/// int(120)
/// int(64)
/// int(0)
/// int(1)
/// int(2)
/// int(2)
/// int(30)
/// int(1)
/// int(0)
/// int(6)
/// int(17)
/// int(1)
/// int(255)
/// int(1)
/// int(2)
/// int(3)
/// int(5)
/// int(4)
/// int(2)
/// int(1)
/// int(4)
/// int(8)
/// string(2) "60"
/// ```
#[php_const]
pub(crate) mod constants {
    /// `SEEK_SET`, `SEEK_CUR` and `SEEK_END` as `<stdio.h>` has them.
    pub(crate) const SEEK_SET: i64 = 0;
    pub(crate) const SEEK_CUR: i64 = 1;
    pub(crate) const SEEK_END: i64 = 2;
    /// `PHP_LOCK_SH`, `PHP_LOCK_EX`, `LOCK_UN` and `LOCK_NB`
    /// (`Z/ext/standard/flock_compat.h:46`), what `LOCK_*` are to a program.
    pub(crate) const LOCK_SH: i64 = 1;
    pub(crate) const LOCK_EX: i64 = 2;
    pub(crate) const LOCK_UN: i64 = 3;
    pub(crate) const LOCK_NB: i64 = 4;
    /// `PHP_STREAM_NOTIFY_*` and `PHP_STREAM_NOTIFY_SEVERITY_*`
    /// (`Z/main/streams/php_stream_context.h`), a notifier's codes.
    pub(crate) const STREAM_NOTIFY_CONNECT: i64 = 2;
    pub(crate) const STREAM_NOTIFY_AUTH_REQUIRED: i64 = 3;
    pub(crate) const STREAM_NOTIFY_AUTH_RESULT: i64 = 10;
    pub(crate) const STREAM_NOTIFY_MIME_TYPE_IS: i64 = 4;
    pub(crate) const STREAM_NOTIFY_FILE_SIZE_IS: i64 = 5;
    pub(crate) const STREAM_NOTIFY_REDIRECTED: i64 = 6;
    pub(crate) const STREAM_NOTIFY_PROGRESS: i64 = 7;
    pub(crate) const STREAM_NOTIFY_FAILURE: i64 = 9;
    pub(crate) const STREAM_NOTIFY_COMPLETED: i64 = 8;
    pub(crate) const STREAM_NOTIFY_RESOLVE: i64 = 1;
    pub(crate) const STREAM_NOTIFY_SEVERITY_INFO: i64 = 0;
    pub(crate) const STREAM_NOTIFY_SEVERITY_WARN: i64 = 1;
    pub(crate) const STREAM_NOTIFY_SEVERITY_ERR: i64 = 2;
    /// `PHP_STREAM_FILTER_*` (`Z/main/streams/php_stream_filter_api.h`),
    /// the chains `stream_filter_append` and `_prepend` name.
    pub(crate) const STREAM_FILTER_READ: i64 = 1;
    pub(crate) const STREAM_FILTER_WRITE: i64 = 2;
    pub(crate) const STREAM_FILTER_ALL: i64 = 3;
    /// `PHP_STREAM_CLIENT_*` and `STREAM_XPORT_*` (`Z/main/php_streams.h`,
    /// `Z/main/streams/php_stream_transport.h`): what `$flags` holds.
    pub(crate) const STREAM_CLIENT_PERSISTENT: i64 = 1;
    pub(crate) const STREAM_CLIENT_ASYNC_CONNECT: i64 = 2;
    pub(crate) const STREAM_CLIENT_CONNECT: i64 = 4;
    pub(crate) const STREAM_CRYPTO_METHOD_ANY_CLIENT: i64 = 127;
    pub(crate) const STREAM_CRYPTO_METHOD_SSLv2_CLIENT: i64 = 3;
    pub(crate) const STREAM_CRYPTO_METHOD_SSLv3_CLIENT: i64 = 5;
    pub(crate) const STREAM_CRYPTO_METHOD_SSLv23_CLIENT: i64 = 57;
    pub(crate) const STREAM_CRYPTO_METHOD_TLS_CLIENT: i64 = 121;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_0_CLIENT: i64 = 9;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_1_CLIENT: i64 = 17;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_2_CLIENT: i64 = 33;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_3_CLIENT: i64 = 65;
    pub(crate) const STREAM_CRYPTO_METHOD_ANY_SERVER: i64 = 126;
    pub(crate) const STREAM_CRYPTO_METHOD_SSLv2_SERVER: i64 = 2;
    pub(crate) const STREAM_CRYPTO_METHOD_SSLv3_SERVER: i64 = 4;
    pub(crate) const STREAM_CRYPTO_METHOD_SSLv23_SERVER: i64 = 120;
    pub(crate) const STREAM_CRYPTO_METHOD_TLS_SERVER: i64 = 120;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_0_SERVER: i64 = 8;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_1_SERVER: i64 = 16;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_2_SERVER: i64 = 32;
    pub(crate) const STREAM_CRYPTO_METHOD_TLSv1_3_SERVER: i64 = 64;
    pub(crate) const STREAM_CRYPTO_PROTO_SSLv3: i64 = 4;
    pub(crate) const STREAM_CRYPTO_PROTO_TLSv1_0: i64 = 8;
    pub(crate) const STREAM_CRYPTO_PROTO_TLSv1_1: i64 = 16;
    pub(crate) const STREAM_CRYPTO_PROTO_TLSv1_2: i64 = 32;
    pub(crate) const STREAM_CRYPTO_PROTO_TLSv1_3: i64 = 64;
    /// `STREAM_SHUT_*`, what `stream_socket_shutdown` takes.
    pub(crate) const STREAM_SHUT_RD: i64 = 0;
    pub(crate) const STREAM_SHUT_WR: i64 = 1;
    pub(crate) const STREAM_SHUT_RDWR: i64 = 2;
    pub(crate) const STREAM_PF_INET: i64 = 2;
    pub(crate) const STREAM_PF_INET6: i64 = 30;
    pub(crate) const STREAM_PF_UNIX: i64 = 1;
    pub(crate) const STREAM_IPPROTO_IP: i64 = 0;
    pub(crate) const STREAM_IPPROTO_TCP: i64 = 6;
    pub(crate) const STREAM_IPPROTO_UDP: i64 = 17;
    pub(crate) const STREAM_IPPROTO_ICMP: i64 = 1;
    pub(crate) const STREAM_IPPROTO_RAW: i64 = 255;
    pub(crate) const STREAM_SOCK_STREAM: i64 = 1;
    pub(crate) const STREAM_SOCK_DGRAM: i64 = 2;
    pub(crate) const STREAM_SOCK_RAW: i64 = 3;
    pub(crate) const STREAM_SOCK_SEQPACKET: i64 = 5;
    pub(crate) const STREAM_SOCK_RDM: i64 = 4;
    pub(crate) const STREAM_PEEK: i64 = 2;
    pub(crate) const STREAM_OOB: i64 = 1;
    pub(crate) const STREAM_SERVER_BIND: i64 = 4;
    pub(crate) const STREAM_SERVER_LISTEN: i64 = 8;
    /// `PHP_FILE_*` (`Z/ext/standard/file.h`), the bits of `$flags`.
    pub(crate) const FILE_USE_INCLUDE_PATH: i64 = 1;
    pub(crate) const FILE_IGNORE_NEW_LINES: i64 = 2;
    pub(crate) const FILE_SKIP_EMPTY_LINES: i64 = 4;
    pub(crate) const FILE_APPEND: i64 = 8;
    pub(crate) const FILE_NO_DEFAULT_CONTEXT: i64 = 16;
    #[php(deprecated(since = "8.1", message = "as the constant has no effect"))]
    pub(crate) const FILE_TEXT: i64 = 0;
    #[php(deprecated(since = "8.1", message = "as the constant has no effect"))]
    pub(crate) const FILE_BINARY: i64 = 0;
    /// `FNM_NOESCAPE`, `FNM_PATHNAME`, `FNM_PERIOD` and `FNM_CASEFOLD` as
    /// this host's `<fnmatch.h>` has them, what `fnmatch`'s `$flags` hands
    /// `fnmatch(3)` unchanged.
    pub(crate) const FNM_NOESCAPE: i64 = 1;
    pub(crate) const FNM_PATHNAME: i64 = 2;
    pub(crate) const FNM_PERIOD: i64 = 4;
    pub(crate) const FNM_CASEFOLD: i64 = 16;
}

/// `PHP_Z_PARAM_STREAM_OR_NULL` then
/// `php_dir_get_directory_stream_from_user_arg`
/// (`Z/ext/standard/dir.c:165`): the directory stream the argument at
/// position one names; no argument or `null` is the 8.4 deprecation and
/// then the directory `opendir` opened last, or php's `No resource
/// supplied` where there is none; a stream that is no directory is php's
/// `must be a valid Directory resource`.
fn dir_stream(req: &mut Request, given: Option<Stream>) -> Result<Stream, Thrown> {
    let stream = if let Some(stream) = given {
        stream
    } else {
        docref(
            req,
            Level::Deprecated,
            "Passing null is deprecated, instead the last opened directory stream should be provided",
        );
        let default = req.state::<State>().default_dir.as_deref().copied();
        let Some(stream) = default.and_then(|res| Stream::from_value(req, &res.as_value())) else {
            return Err(type_error(req, "No resource supplied"));
        };
        stream
    };
    if !stream.is_dir(req) {
        return Err(arg_type_error(req, 1, "be a valid Directory resource"));
    }
    Ok(stream)
}

/// `php_set_default_dir` (`Z/ext/standard/dir.c:62`): the owner the last
/// directory held given up, and one taken on the new one, or none.
pub(super) fn set_default_dir(req: &mut Request, stream: Option<Stream>) {
    let res = stream.map(|stream| Owner::take(req, stream.resource()));
    if let Some(old) = std::mem::replace(&mut req.state::<State>().default_dir, res) {
        old.give_up(req);
    }
}

/// `opendir(string $directory, $context = null)`, `PHP_FUNCTION(opendir)`
/// (`Z/ext/standard/dir.c:146`): `php_stream_opendir` under
/// `REPORT_ERRORS`, the stream a resource of the `stream` type that
/// becomes the directory `readdir()` reads with no argument, or `false`
/// after the warning. The plain files wrapper gives `.` and `..` first
/// and then the entries in the order `readdir(3)` has them.
///
/// ```php
/// $d = __DIR__; fclose(fopen("$d/b.txt", 'w')); fclose(fopen("$d/a.txt", 'w'));
/// $h = opendir($d); var_dump($h);
/// $names = []; while (($name = readdir($h)) !== false) { $names[] = $name; }
/// sort($names); var_dump($names, readdir($h));
/// var_dump(opendir("$d/missing"), opendir("$d/a.txt"), opendir('php://memory'), opendir(''));
/// ```
/// ```expect
/// resource(%d) of type (stream)
/// array(5) {
///   [0]=>
///   string(1) "."
///   [1]=>
///   string(2) ".."
///   [2]=>
///   string(5) "a.txt"
///   [3]=>
///   string(5) "b.txt"
///   [4]=>
///   string(11) "witness.php"
/// }
/// bool(false)
///
/// Warning: opendir(%s/missing): Failed to open directory: No such file or directory in %s on line 6
///
/// Warning: opendir(%s/a.txt): Failed to open directory: Not a directory in %s on line 6
///
/// Warning: opendir(php://memory): Failed to open directory: not implemented in %s on line 6
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn opendir(
    req: &mut Request,
    directory: Path,
    #[php(default = null)] context: Option<Resource>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let Path(directory) = directory;
    let context = context_of(req, context, NullContext::Default)?;
    let Some(dir) = req.with_stream_ctx(context, |req| req.open_dir(directory.as_bytes()))? else {
        return Ok(Untyped(OrFalse::False));
    };
    set_default_dir(req, Some(*dir));
    Ok(Untyped(OrFalse::Value(dir.into_resource())))
}

/// `closedir($dir_handle = null): void`, `PHP_FUNCTION(closedir)`
/// (`Z/ext/standard/dir.c:205`): the directory's resource closed, and
/// the last opened one forgotten where it was that. A stream that is no
/// directory is refused, and `fclose` refuses a directory.
///
/// ```php
/// $h = opendir(__DIR__);
/// var_dump(closedir($h), $h, fclose($f = opendir(__DIR__)), readdir($f));
/// try { closedir(fopen('php://memory', 'r')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { closedir("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { closedir($h); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// closedir(); try { closedir(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 3
/// NULL
/// resource(%d) of type (Unknown)
/// bool(false)
/// string(1) "."
/// closedir(): Argument #1 ($dir_handle) must be a valid Directory resource
/// closedir(): Argument #1 ($dir_handle) must be of type resource or null, string given
/// closedir(): Argument #1 ($dir_handle) must be an open stream resource
///
/// Deprecated: closedir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 7
///
/// Deprecated: closedir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 7
/// No resource supplied
/// ```
#[php_function]
fn closedir(
    req: &mut Request,
    #[php(default = null)] dir_handle: Option<Stream>,
) -> Result<(), Thrown> {
    let dir = dir_stream(req, dir_handle)?;
    let res = dir.resource();
    dir.close(req);
    let is_default = req
        .state::<State>()
        .default_dir
        .as_deref()
        .is_some_and(|default| default.id() == res.id());
    if is_default {
        set_default_dir(req, None);
    }
    Ok(())
}

/// `rewinddir($dir_handle = null): void`, `PHP_FUNCTION(rewinddir)`
/// (`Z/ext/standard/dir.c:245`): `php_stream_rewinddir`, a seek to the
/// start, which the plain files wrapper takes as `rewinddir(3)`.
///
/// ```php
/// $h = opendir(__DIR__);
/// var_dump(readdir($h), readdir($h), rewinddir($h), readdir($h) === readdir(opendir(__DIR__)));
/// ```
/// ```expect
/// string(1) "."
/// string(2) ".."
/// NULL
/// bool(true)
/// ```
#[php_function]
fn rewinddir(
    req: &mut Request,
    #[php(default = null)] dir_handle: Option<Stream>,
) -> Result<(), Thrown> {
    let dir = dir_stream(req, dir_handle)?;
    dir.seek(req, 0, Whence::Set);
    Ok(())
}

/// `readdir($dir_handle = null): string|false`, `PHP_FUNCTION(readdir)`
/// (`Z/ext/standard/dir.c:276`): the next entry's name, `false` past the
/// last; with no argument, the directory `opendir` opened last.
///
/// ```php
/// $d = __DIR__; fclose(fopen("$d/only.txt", 'w'));
/// $h = opendir($d); $names = []; while (($name = readdir()) !== false) { $names[] = $name; }
/// sort($names); var_dump($names);
/// try { readdir(fopen('php://memory', 'r')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
///
/// Deprecated: readdir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 3
/// array(4) {
///   [0]=>
///   string(1) "."
///   [1]=>
///   string(2) ".."
///   [2]=>
///   string(8) "only.txt"
///   [3]=>
///   string(11) "witness.php"
/// }
/// readdir(): Argument #1 ($dir_handle) must be a valid Directory resource
/// ```
#[php_function]
fn readdir(
    req: &mut Request,
    #[php(default = null)] dir_handle: Option<Stream>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let dir = dir_stream(req, dir_handle)?;
    Ok(match dir.read_dir(req)? {
        Some(name) => OrFalse::Value(PhpString::new(req, &name)?),
        None => OrFalse::False,
    })
}

/// `rewind($stream): bool`, `PHP_FUNCTION(rewind)`
/// (`Z/ext/standard/file.c:1027`): `php_stream_rewind`, a seek to zero,
/// and whether it was taken. On `php://memory` the seek clears the eof
/// the last read set, so the same bytes read again; the position the
/// generic layer keeps is the seek's and then the write's, two, where
/// the memory kind's own is four.
///
/// ```php
/// $f = fopen('php://memory', 'r+');
/// var_dump(fwrite($f, "ab"), fread($f, 2), feof($f), rewind($f), feof($f), fread($f, 2), rewind($f), fwrite($f, "cd"), ftell($f), rewind($f), fread($f, 10));
/// ```
/// ```expect
/// int(2)
/// string(0) ""
/// bool(true)
/// bool(true)
/// bool(false)
/// string(2) "ab"
/// bool(true)
/// int(2)
/// int(2)
/// bool(true)
/// string(2) "cd"
/// ```
#[php_function]
pub fn rewind(req: &mut Request, stream: Stream) -> bool {
    stream.seek(req, 0, Whence::Set)
}

/// `fclose($stream): bool`, `PHP_FUNCTION(fclose)`
/// (`Z/ext/standard/file.c:752`): `php_stream_free` under
/// `PHP_STREAM_FREE_KEEP_RSRC | PHP_STREAM_FREE_CLOSE`, which closes the
/// resource and leaves it in the list for its owners, and `true`. What
/// is left is a closed resource: `var_dump` prints php's `Unknown` for
/// its type, `is_resource` says no, and a second `fclose` is the
/// `TypeError` `PHP_Z_PARAM_STREAM` raises for a resource that is not an
/// open stream, beside the parser's own wording for what is no resource.
///
/// ```php
/// $f = fopen('php://memory', 'r+');
/// var_dump(fclose($f), $f, is_resource($f));
/// try { fclose($f); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fclose("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fclose(null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fclose(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// resource(%d) of type (Unknown)
/// bool(false)
/// fclose(): Argument #1 ($stream) must be an open stream resource
/// fclose(): Argument #1 ($stream) must be of type resource, string given
/// fclose(): Argument #1 ($stream) must be of type resource, null given
/// fclose() expects exactly 1 argument, 0 given
/// ```
///
/// A `php://temp` is two resources, the stream and the memory stream it
/// encloses, as `_php_stream_temp_create_ex` registers them, so the
/// next resource's id is two further on; closing the temp closes and
/// frees the inner one with it.
///
/// ```php
/// $a = fopen('php://temp', 'r+'); $b = fopen('php://memory', 'r+');
/// var_dump((int)$b - (int)$a, fwrite($a, "temp!"), rewind($a), fread($a, 10), feof($a), fclose($a), $a);
/// ```
/// ```expect
/// int(2)
/// int(5)
/// bool(true)
/// string(5) "temp!"
/// bool(true)
/// bool(true)
/// resource(%d) of type (Unknown)
/// ```
#[php_function]
pub fn fclose(req: &mut Request, stream: Stream) -> bool {
    // `PHP_STREAM_FLAG_NO_FCLOSE`: a directory, and a stream an operation
    // holds while it serves PHP code on it, which `close` refuses.
    if stream.no_fclose(req) || !stream.close(req) {
        docref(
            req,
            Level::Warning,
            "cannot close the provided stream, as it must not be manually closed",
        );
        return false;
    }
    true
}

/// `feof($stream): bool`, `PHP_FUNCTION(feof)`
/// (`Z/ext/standard/file.c:845`): `php_stream_eof`, which is never true
/// while the read buffer holds a byte and otherwise what the kind's
/// last read said. A plain file is read a chunk at a time, so after a
/// short file is read whole in two calls eof is still clear, and
/// it is the read that finds nothing that sets it; `php://memory` sets
/// it on the read that reaches its end, and only a read asking for
/// more than is there reaches it, since a read satisfied in full stops
/// before the kind is asked again.
///
/// ```php
/// $w = fopen(__DIR__ . '/feof.txt', 'w'); fwrite($w, "hello"); fclose($w);
/// $r = fopen(__DIR__ . '/feof.txt', 'r');
/// var_dump(feof($r), fread($r, 3), feof($r), fread($r, 2), feof($r), fread($r, 1), feof($r), fclose($r));
/// $r = fopen(__DIR__ . '/feof.txt', 'r');
/// var_dump(fread($r, 10), feof($r), rewind($r), feof($r), fread($r, 2));
/// $m = fopen('php://memory', 'r+'); fwrite($m, "hello"); rewind($m);
/// var_dump(feof($m), fread($m, 5), feof($m), rewind($m), fread($m, 3), feof($m), fread($m, 3), feof($m));
/// ```
/// ```expect
/// bool(false)
/// string(3) "hel"
/// bool(false)
/// string(2) "lo"
/// bool(false)
/// string(0) ""
/// bool(true)
/// bool(true)
/// string(5) "hello"
/// bool(true)
/// bool(true)
/// bool(false)
/// string(2) "he"
/// bool(false)
/// string(5) "hello"
/// bool(false)
/// bool(true)
/// string(3) "hel"
/// bool(false)
/// string(2) "lo"
/// bool(true)
/// ```
#[php_function]
pub fn feof(req: &mut Request, stream: Stream) -> bool {
    stream.eof(req)
}

/// `fgetc($stream): string|false`, `PHP_FUNCTION(fgetc)`
/// (`Z/ext/standard/file.c:910`): `php_stream_getc`, one byte read, and
/// `false` where none was.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "ab"); rewind($m);
/// var_dump(fgetc($m), fgetc($m), feof($m), fgetc($m), feof($m), ftell($m));
/// $w = fopen(__DIR__ . '/fgetc.txt', 'w'); var_dump(fgetc($w));
/// try { fgetc("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// bool(false)
/// bool(false)
/// bool(true)
/// int(2)
///
/// Notice: fgetc(): Read of 8192 bytes failed with errno=9 Bad file descriptor in %s on line 4
/// bool(false)
/// fgetc(): Argument #1 ($stream) must be of type resource, string given
/// ```
#[php_function]
pub fn fgetc(req: &mut Request, stream: Stream) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    Ok(match stream.read(req, 1)? {
        Some(byte) if !byte.is_empty() => OrFalse::Value(byte),
        Some(empty) => {
            empty.give_up(req);
            OrFalse::False
        }
        None => OrFalse::False,
    })
}

/// `fgets($stream, ?int $length = null): string|false`,
/// `PHP_FUNCTION(fgets)` (`Z/ext/standard/file.c:862`):
/// `php_stream_get_line`, the next line with its `\n`, of any length for
/// `null` and at most `$length - 1` bytes otherwise, a length at or
/// below zero being php's `ValueError`; `false` where no byte was left.
/// The line is read through the read buffer a chunk at a time, so a
/// `php://memory` read by line has its bytes in the buffer, which a
/// seek or a write drops.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "one\ntwo\nthree"); rewind($m);
/// var_dump(fgets($m), ftell($m), fgets($m, 3), fgets($m, 2), fgets($m), fgets($m), fgets($m), feof($m));
/// rewind($m); var_dump(fgets($m, 1), fread($m, 3), fgets($m, null), fwrite($m, "!"), rewind($m), fgets($m));
/// try { fgets($m, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "one
/// "
/// int(4)
/// string(2) "tw"
/// string(1) "o"
/// string(1) "
/// "
/// string(5) "three"
/// bool(false)
/// bool(true)
/// bool(false)
/// string(3) "one"
/// string(1) "
/// "
/// int(1)
/// bool(true)
/// string(4) "one
/// "
/// fgets(): Argument #2 ($length) must be greater than 0
/// ```
///
/// A plain file's line longer than a chunk is read whole, and the
/// length is asked for as a string before a byte is read, so one the
/// memory limit cannot hold is php's fatal.
///
/// ```php
/// $p = __DIR__ . '/fgets.txt'; $w = fopen($p, 'w'); fwrite($w, str_repeat("x", 10000) . "\nend"); fclose($w);
/// $r = fopen($p, 'r'); var_dump(strlen(fgets($r)), fgets($r), fgets($r), feof($r));
/// fgets($r, PHP_INT_MAX);
/// ```
/// ```expect exit=255
/// int(10001)
/// string(3) "end"
/// bool(false)
/// bool(true)
///
/// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate %d bytes) in %s on line 4
/// ```
#[php_function]
pub fn fgets(
    req: &mut Request,
    stream: Stream,
    #[php(default = null)] length: Option<i64>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let maxlen = match length {
        Some(len) if len <= 0 => return Err(arg_value_error(req, 2, "be greater than 0")),
        Some(len) => Some(usize::try_from(len).unwrap_or(usize::MAX)),
        None => None,
    };
    Ok(match stream.get_line(req, maxlen)? {
        // `zend_string_finit` of the line (`Z/ext/standard/file.c:891`).
        Some(line) => OrFalse::Value(PhpString::finit(req, &line)?),
        None => OrFalse::False,
    })
}

/// `fread($stream, int $length): string|false`, `PHP_FUNCTION(fread)`
/// (`Z/ext/standard/file.c:1539`): a length at or below zero is php's
/// `ValueError`; then `php_stream_read_to_str`, up to `$length` bytes
/// and fewer at the end, and `false` for the read the kind refused,
/// which a file opened for writing alone does with php's notice and
/// `errno` in it.
///
/// ```php
/// $w = fopen(__DIR__ . '/fread.txt', 'w');
/// var_dump(fwrite($w, "hello"), fread($w, 3), feof($w), fclose($w));
/// $m = fopen('php://memory', 'r+'); fwrite($m, "hello"); rewind($m);
/// var_dump(fread($m, 2), fread($m, 100), fread($m, 1));
/// try { fread($m, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { fread($m, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { fread("x", 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Notice: fread(): Read of 8192 bytes failed with errno=9 Bad file descriptor in %s on line 3
/// int(5)
/// bool(false)
/// bool(false)
/// bool(true)
/// string(2) "he"
/// string(3) "llo"
/// string(0) ""
/// fread(): Argument #2 ($length) must be greater than 0
/// fread(): Argument #2 ($length) must be greater than 0
/// fread(): Argument #1 ($stream) must be of type resource, string given
/// ```
///
/// The string is asked for at `$length` before a byte is read,
/// `zend_string_alloc`, so a length the memory limit cannot hold is
/// php's fatal and not a read.
///
/// ```php
/// var_dump(fread(fopen('php://memory', 'r'), PHP_INT_MAX));
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate %d bytes) in %s on line 2
/// ```
#[php_function]
pub fn fread(
    req: &mut Request,
    stream: Stream,
    length: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if length <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let len = usize::try_from(length).unwrap_or(usize::MAX);
    Ok(stream
        .read(req, len)?
        .map_or(OrFalse::False, OrFalse::Value))
}

/// `fopen(string $filename, string $mode, bool $use_include_path = false, $context = null)`,
/// `PHP_FUNCTION(fopen)` (`Z/ext/standard/file.c:722`):
/// `php_stream_open_wrapper_ex` under `REPORT_ERRORS`, with `USE_PATH`
/// where asked, and the stream as a resource, or `false` after the
/// warning the open left. `$filename` is `Z_PARAM_PATH`, a string with
/// no null byte in it. `$context` is taken and not read: no wrapper
/// here reads one.
///
/// ```php
/// $f = fopen('php://memory', 'r+');
/// var_dump($f, fwrite($f, "hello"), ftell($f), fread($f, 3), feof($f), rewind($f), fread($f, 3), feof($f), fread($f, 10), feof($f), fread($f, 1), feof($f));
/// ```
/// ```expect
/// resource(%d) of type (stream)
/// int(5)
/// int(5)
/// string(0) ""
/// bool(true)
/// bool(true)
/// string(3) "hel"
/// bool(false)
/// string(2) "lo"
/// bool(true)
/// string(0) ""
/// bool(true)
/// ```
///
/// A plain file is opened through `php_stream_parse_fopen_modes`'s
/// `open(2)` flags: `w` creates and truncates, `a` appends, `r` needs
/// the file there, and a `$mode` php does not know is the wrapper's own
/// line after `Failed to open stream: `. The path is against the
/// working directory, and `file://` names the same wrapper.
///
/// ```php
/// $p = __DIR__ . '/fopen.txt';
/// $w = fopen($p, 'w'); var_dump(fwrite($w, "one"), fclose($w));
/// $a = fopen($p, 'a'); var_dump(fwrite($a, "two"), fclose($a));
/// $r = fopen('file://' . $p, 'r'); var_dump(fread($r, 100), fclose($r));
/// var_dump(fopen($p, 'q'), fopen(__DIR__ . '/missing.txt', 'r'));
/// try { fopen("a\0b", 'r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { fopen('', 'r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// bool(true)
/// int(3)
/// bool(true)
/// string(6) "onetwo"
/// bool(true)
///
/// Warning: fopen(%s/fopen.txt): Failed to open stream: `q' is not a valid mode for fopen in %s on line 6
///
/// Warning: fopen(%s/missing.txt): Failed to open stream: No such file or directory in %s on line 6
/// bool(false)
/// bool(false)
/// fopen(): Argument #1 ($filename) must not contain any null bytes
/// Path must not be empty
/// ```
///
/// `php://stdout` and `php://stderr` are the host's two fds, past the
/// `php_output` layer as php's CLI writes them; `php://output` is that
/// layer itself, so it is what `ob_start` catches and `php://stdout` is
/// not.
///
/// ```php
/// ob_start(); var_dump(fwrite(fopen('php://stdout', 'w'), "out\n"), fwrite(fopen('php://output', 'w'), "output\n")); $held = ob_get_clean(); echo "[", $held, "]";
/// var_dump(fwrite(fopen('php://stderr', 'w'), "err\n"));
/// ```
/// ```expect
/// out
/// [output
/// int(4)
/// int(7)
/// ]int(4)
/// ```
/// ```stderr
/// err
/// ```
#[php_function]
fn fopen(
    req: &mut Request,
    filename: Path,
    mode: PhpString,
    #[php(default = false)] use_include_path: bool,
    #[php(default = null)] context: Option<Resource>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let Path(filename) = filename;
    let context = context_of(req, context, NullContext::Default)?;
    let opened = req.with_stream_ctx(context, |req| {
        req.open_stream(filename.as_bytes(), mode.as_bytes(), use_include_path)
    })?;
    Ok(Untyped(opened.map_or(OrFalse::False, |stream| {
        OrFalse::Value(stream.into_resource())
    })))
}

/// `ftruncate($stream, int $size): bool`, `PHP_FUNCTION(ftruncate)`
/// (`Z/ext/standard/file.c:1299`): a size below zero is php's
/// `ValueError`, a stream that does not truncate is php's warning and
/// `false`, and otherwise whether `php_stream_truncate_set_size` took
/// the size. Neither the position nor the kind's offset moves.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "hello");
/// var_dump(ftruncate($m, 2), ftell($m), rewind($m), fread($m, 10), ftruncate($m, 4), rewind($m), fread($m, 10) === "he\0\0");
/// var_dump(ftruncate(fopen('php://memory', 'r'), 0), ftruncate(fopen('php://output', 'w'), 0));
/// $p = __DIR__ . '/ftruncate.txt'; $w = fopen($p, 'w+'); fwrite($w, "0123456789");
/// var_dump(ftruncate($w, 3), ftell($w), fstat($w)['size'], rewind($w), fread($w, 10), ftruncate(fopen($p, 'r'), 0));
/// try { ftruncate($m, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(5)
/// bool(true)
/// string(2) "he"
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Warning: ftruncate(): Can't truncate this stream! in %s on line 4
/// bool(false)
/// bool(false)
/// bool(true)
/// int(10)
/// int(3)
/// bool(true)
/// string(3) "012"
/// bool(false)
/// ftruncate(): Argument #2 ($size) must be greater than or equal to 0
/// ```
#[php_function]
fn ftruncate(req: &mut Request, stream: Stream, size: i64) -> Result<bool, Thrown> {
    if size < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    let Some(taken) = stream.truncate(req, size)? else {
        docref(req, Level::Warning, "Can't truncate this stream!");
        return Ok(false);
    };
    Ok(taken)
}

/// The names `php_fstat` gives the thirteen fields after their numbers
/// (`Z/ext/standard/file.c:1327`).
const STAT_NAMES: [&[u8]; 13] = [
    b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
    b"ctime", b"blksize", b"blocks",
];

/// `fstat($stream): array|false`, `PHP_FUNCTION(fstat)`
/// (`Z/ext/standard/file.c:1395`) over `php_fstat`: the thirteen fields
/// of `php_stream_stat` under the numbers `0` to `12` and then under
/// their names, or `false` for a stream with no stat. A `php://memory`
/// is a regular file of its length, `0666` or `0444` read only, on
/// device `0xC`.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "abc"); var_dump(fstat($m));
/// $p = __DIR__ . '/fstat.txt'; $w = fopen($p, 'w'); fwrite($w, "hello"); $s = fstat($w);
/// var_dump(count($s), $s['size'], $s[7], $s['mode'] & 0170000, $s['nlink'], $s['mtime'] > 0, $s['blksize'] > 0);
/// var_dump(fstat(fopen('php://output', 'w')), fstat(opendir(__DIR__)));
/// ```
/// ```expect
/// array(26) {
///   [0]=>
///   int(12)
///   [1]=>
///   int(0)
///   [2]=>
///   int(33206)
///   [3]=>
///   int(1)
///   [4]=>
///   int(0)
///   [5]=>
///   int(0)
///   [6]=>
///   int(-1)
///   [7]=>
///   int(3)
///   [8]=>
///   int(0)
///   [9]=>
///   int(0)
///   [10]=>
///   int(0)
///   [11]=>
///   int(-1)
///   [12]=>
///   int(-1)
///   ["dev"]=>
///   int(12)
///   ["ino"]=>
///   int(0)
///   ["mode"]=>
///   int(33206)
///   ["nlink"]=>
///   int(1)
///   ["uid"]=>
///   int(0)
///   ["gid"]=>
///   int(0)
///   ["rdev"]=>
///   int(-1)
///   ["size"]=>
///   int(3)
///   ["atime"]=>
///   int(0)
///   ["mtime"]=>
///   int(0)
///   ["ctime"]=>
///   int(0)
///   ["blksize"]=>
///   int(-1)
///   ["blocks"]=>
///   int(-1)
/// }
/// int(26)
/// int(5)
/// int(5)
/// int(32768)
/// int(1)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn fstat(req: &mut Request, stream: Stream) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let answer = php_fstat(req, stream)?;
    let array = answer.as_array().map(|array| Owner::take(req, array));
    answer.give_up(req);
    Ok(array.map_or(OrFalse::False, OrFalse::Value))
}

/// `php_fstat(stream, return_value)`, `PHPAPI` in `file.h`
/// (`Z/ext/standard/file.c:1327`): the stat array of `stream`, `false`
/// for a stream with no stat. What `fstat()` answers, and what
/// `SplFileObject::fstat()` calls directly, so neither `disable_functions`
/// nor a namespaced `fstat` reaches it.
///
/// ```php
/// $t = new SplTempFileObject(); $t->fwrite("abc"); $s = $t->fstat();
/// var_dump(count($s), $s['size'], $s['mode'] & 0170000);
/// ```
/// ```expect
/// int(26)
/// int(3)
/// int(32768)
/// ```
pub fn php_fstat(req: &mut Request, stream: Stream) -> Result<Owner<Value>, Thrown> {
    let Some(sb) = stream.stat(req) else {
        return Ok(Owner::bool(false));
    };
    let mut out = PhpArray::new(req)?;
    for field in sb {
        out.append(req, Owner::int(field))?;
    }
    for (name, field) in STAT_NAMES.iter().zip(sb) {
        out.set_str(req, name, Owner::int(field))?;
    }
    Ok(out.into_value())
}

/// `fseek($stream, int $offset, int $whence = SEEK_SET): int`,
/// `PHP_FUNCTION(fseek)` (`Z/ext/standard/file.c:1061`):
/// `php_stream_seek`, `0` where it was taken and `-1` where not. A
/// `$whence` that is none of `SEEK_SET`, `SEEK_CUR` and `SEEK_END` is
/// handed on as it is, which a stream that seeks answers `-1` to and one
/// that cannot is php's warning for; a seek before the start is `-1` and
/// one past the end is taken.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "hello");
/// var_dump(fseek($m, 1), ftell($m), fread($m, 2), fseek($m, -1, SEEK_CUR), fread($m, 10), fseek($m, -2, SEEK_END), fread($m, 10));
/// var_dump(fseek($m, 10, SEEK_END), ftell($m), fseek($m, -1), ftell($m), fseek($m, -20, SEEK_END), ftell($m), fseek($m, 0, 7), ftell($m));
/// $o = fopen('php://output', 'w'); var_dump(fseek($o, 0), fseek($o, 1, SEEK_CUR), fseek($o, 0, 7));
/// ```
/// ```expect
/// int(0)
/// int(1)
/// string(2) "el"
/// int(0)
/// string(3) "llo"
/// int(0)
/// string(2) "lo"
/// int(0)
/// int(15)
/// int(-1)
/// int(15)
/// int(-1)
/// bool(false)
/// int(-1)
/// int(0)
///
/// Warning: fseek(): Stream does not support seeking in %s on line 5
///
/// Warning: fseek(): Stream does not support seeking in %s on line 5
/// int(-1)
/// int(-1)
/// int(-1)
/// ```
///
/// On a plain file a seek that lands inside what the read buffer holds
/// moves the cursor in it; any other is `lseek(2)` and the buffer
/// dropped.
///
/// ```php
/// $p = __DIR__ . '/fseek.txt'; $w = fopen($p, 'w'); fwrite($w, "0123456789"); fclose($w);
/// $r = fopen($p, 'r'); var_dump(fread($r, 2), fseek($r, 5), fread($r, 2), fseek($r, 2, SEEK_CUR), fread($r, 10), feof($r));
/// var_dump(fseek($r, 20), ftell($r), fread($r, 1), feof($r), fseek($r, 0, 9), ftell($r), fseek($r, 1, SEEK_SET), fgetc($r));
/// ```
/// ```expect
/// string(2) "01"
/// int(0)
/// string(2) "56"
/// int(0)
/// string(1) "9"
/// bool(true)
/// int(0)
/// int(20)
/// string(0) ""
/// bool(true)
/// int(-1)
/// int(20)
/// int(0)
/// string(1) "1"
/// ```
#[php_function]
pub fn fseek(
    req: &mut Request,
    stream: Stream,
    offset: i64,
    #[php(default = SEEK_SET)] whence: i64,
) -> i64 {
    let sought = stream.seek(req, offset, Whence::from_int(whence));
    if sought { 0 } else { -1 }
}

/// `ftell($stream): int|false`, `PHP_FUNCTION(ftell)`
/// (`Z/ext/standard/file.c:1049`): `php_stream_tell`, the position the
/// generic layer keeps, and `false` where it is `-1`, which a stream
/// that cannot seek carries from its open. A file opened for append
/// starts at the offset `lseek(SEEK_CUR)` answers, which is zero until
/// the first write, so what `ftell` says after that write is what was
/// written and not the file's size; php reads the same offset.
///
/// ```php
/// $p = __DIR__ . '/ftell.txt';
/// $w = fopen($p, 'w'); fwrite($w, "hello"); fclose($w);
/// $a = fopen($p, 'a+'); var_dump(ftell($a), fwrite($a, "!!"), ftell($a), rewind($a), fread($a, 100), ftell($a), fclose($a));
/// var_dump(ftell(fopen('php://stdin', 'r')));
/// ```
/// ```expect
/// int(0)
/// int(2)
/// int(2)
/// bool(true)
/// string(7) "hello!!"
/// int(7)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
pub fn ftell(req: &mut Request, stream: Stream) -> OrFalse<i64> {
    let at = stream.tell(req);
    if at == -1 {
        OrFalse::False
    } else {
        OrFalse::Value(at)
    }
}

/// `fflush($stream): bool`, `PHP_FUNCTION(fflush)`
/// (`Z/ext/standard/file.c:1009`): `php_stream_flush`, the flush owed at
/// close taken now, and whether the kind's flush answered `0`, which
/// every kind here does.
///
/// ```php
/// $p = __DIR__ . '/fflush.txt'; $w = fopen($p, 'w');
/// var_dump(fwrite($w, "ab"), fflush($w), fflush(fopen('php://memory', 'r')), fflush(fopen('php://temp', 'r')), fflush(fopen('php://output', 'w')));
/// try { fflush("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// fflush(): Argument #1 ($stream) must be of type resource, string given
/// ```
#[php_function]
fn fflush(req: &mut Request, stream: Stream) -> bool {
    stream.flush(req)
}

/// `fwrite($stream, string $data, ?int $length = null): int|false`,
/// `PHP_FUNCTION(fwrite)` (`Z/ext/standard/file.c:970`) and its alias
/// `fputs`: the bytes to write are `$data` whole, or the first
/// `$length` of them, or none for a length at or below zero, and none
/// is `0` before the stream is asked; then `php_stream_write`, how
/// many the stream took, and `false` for the `-1` a stream that does
/// not take them answers, `php://memory` opened `r` silently and a
/// file opened `r` with php's notice.
///
/// ```php
/// $m = fopen('php://memory', 'r+');
/// var_dump(fwrite($m, "abc", -1), fwrite($m, "abc", 2), fwrite($m, "", 5), fwrite($m, "xyz", null), fputs($m, 12, 1), ftell($m));
/// $r = fopen('php://memory', 'r');
/// var_dump(fwrite($r, "x"), fwrite($r, ""));
/// $p = __DIR__ . '/fwrite.txt'; $w = fopen($p, 'w'); fclose($w);
/// $f = fopen($p, 'r'); var_dump(fwrite($f, "x"));
/// try { fwrite("x", "y"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fwrite($m, "x", "z"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// int(2)
/// int(0)
/// int(3)
/// int(1)
/// int(6)
/// bool(false)
/// int(0)
///
/// Notice: fwrite(): Write of 1 bytes failed with errno=9 Bad file descriptor in %s on line 7
/// bool(false)
/// fwrite(): Argument #1 ($stream) must be of type resource, string given
/// fwrite(): Argument #3 ($length) must be of type ?int, string given
/// ```
///
/// A `php://temp` keeps its bytes in the memory stream it encloses
/// until a write would reach its `maxmemory`, `PHP_STREAM_MAX_MEM` by
/// default, and then spills them to a temporary file, a third resource,
/// which `fstat` reads from then on and the close unlinks.
///
/// ```php
/// $t = fopen('php://temp/maxmemory:4', 'w+'); $a = fopen('php://memory', 'r');
/// var_dump(fwrite($t, "ab"), fstat($t)['mode'], fwrite($t, "cdefgh"), fstat($t)['mode'], fstat($t)['size']);
/// $b = fopen('php://memory', 'r');
/// var_dump((int)$a - (int)$t, (int)$b - (int)$a, rewind($t), fread($t, 100), ftell($t), stream_get_meta_data($t)['stream_type']);
/// $big = fopen('php://temp', 'w+'); fwrite($big, str_repeat("x", 2 * 1024 * 1024 - 1)); $c = fopen('php://memory', 'r');
/// fwrite($big, "y"); $e = fopen('php://memory', 'r');
/// var_dump((int)$c - (int)$big, (int)$e - (int)$c, fstat($big)['size'], fstat($big)['mode']);
/// ```
/// ```expect
/// int(2)
/// int(33206)
/// int(6)
/// int(33152)
/// int(8)
/// int(2)
/// int(2)
/// bool(true)
/// string(8) "abcdefgh"
/// int(8)
/// string(4) "TEMP"
/// int(2)
/// int(2)
/// int(2097152)
/// int(33152)
/// ```
#[php_function(aliases(fputs))]
pub fn fwrite(
    req: &mut Request,
    stream: Stream,
    data: PhpString,
    #[php(default = null)] length: Option<i64>,
) -> OrFalse<i64> {
    // `Z_PARAM_LONG_OR_NULL`: `null`, given or defaulted, is the whole.
    let bytes = data.as_bytes();
    let num_bytes = match length {
        None => bytes.len(),
        Some(n) if n <= 0 => 0,
        Some(n) => usize::try_from(n).unwrap_or(usize::MAX).min(bytes.len()),
    };
    if num_bytes == 0 {
        return OrFalse::Value(0);
    }
    let wrote = stream.write(req, &bytes[..num_bytes]);
    wrote.map_or(OrFalse::False, |n| {
        OrFalse::Value(i64::try_from(n).unwrap_or(i64::MAX))
    })
}

/// `flags & FILE_NO_DEFAULT_CONTEXT`, as `file()` and
/// `file_put_contents()` hand it to `php_stream_context_from_zval`.
fn no_default_context(flags: i64) -> NullContext {
    if flags & FILE_NO_DEFAULT_CONTEXT == 0 {
        NullContext::Default
    } else {
        NullContext::None
    }
}

/// `file_get_contents(string $filename, bool $use_include_path = false, $context = null, int $offset = 0, ?int $length = null): string|false`,
/// `PHP_FUNCTION(file_get_contents)` (`Z/ext/standard/file.c:377`): a
/// length below zero is php's `ValueError`; the file opened `rb` under
/// `REPORT_ERRORS`, with `USE_PATH` where asked, and `false` after the
/// warning where it was not; a plain file read unbuffered; a nonzero
/// offset sought first, from the start where it is positive and from the
/// end where it is negative, a seek refused being php's warning and
/// `false`; then `_php_stream_copy_to_mem`, all of it or `$length`
/// bytes, and the empty string where nothing was read. `$context` is
/// taken and not read: no wrapper here reads one.
///
/// ```php
/// $p = __DIR__ . '/fgc.txt'; $w = fopen($p, 'w'); fwrite($w, "from stream"); fclose($w);
/// var_dump(file_get_contents($p), file_get_contents($p, false, null, 5), file_get_contents($p, false, null, -3), file_get_contents($p, false, null, 2, 3), file_get_contents($p, false, null, 100), file_get_contents($p, false, null, 0, 0));
/// var_dump(file_get_contents($p, false, null, -100), file_get_contents(__DIR__ . '/missing.txt'));
/// try { file_get_contents($p, false, null, 0, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { file_get_contents(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "from stream"
/// string(6) "stream"
/// string(3) "eam"
/// string(3) "om "
/// string(0) ""
/// string(0) ""
///
/// Warning: file_get_contents(): Failed to seek to position -100 in the stream in %s on line 4
///
/// Warning: file_get_contents(%s/missing.txt): Failed to open stream: No such file or directory in %s on line 4
/// bool(false)
/// bool(false)
/// file_get_contents(): Argument #5 ($length) must be greater than or equal to 0
/// Path must not be empty
/// ```
///
/// A directory opens as a file does and is read unbuffered, so the one
/// read that fails asks for what `fstat` says the directory holds and a
/// chunk more.
///
/// ```php
/// $d = __DIR__; var_dump(file_get_contents($d) === "", file_get_contents($d, false, null, 0, 10));
/// ```
/// ```expect
///
/// Notice: file_get_contents(): Read of 8288 bytes failed with errno=21 Is a directory in %s on line 2
///
/// Notice: file_get_contents(): Read of 10 bytes failed with errno=21 Is a directory in %s on line 2
/// bool(true)
/// string(0) ""
/// ```
///
/// A native that opens a stream of its own closes it with
/// `php_stream_close`, which gives up the owner the open handed it
/// ([`Stream::free`]), so nothing it opened is left in `get_resources()`:
/// this one, and every native here that reads or writes a file whole.
///
/// ```php
/// stream_context_get_default();
/// $d = __DIR__; $n = fn($b) => count(array_diff_key(get_resources(), $b));
/// $b = get_resources(); file_put_contents("$d/r.txt", "abc"); echo "fpc ", $n($b), "\n";
/// $b = get_resources(); file_get_contents("$d/r.txt"); echo "fgc ", $n($b), "\n";
/// $b = get_resources(); file("$d/r.txt"); echo "file ", $n($b), "\n";
/// $b = get_resources(); readfile("$d/r.txt"); echo " readfile ", $n($b), "\n";
/// $b = get_resources(); md5_file("$d/r.txt"); sha1_file("$d/r.txt"); hash_file('md5', "$d/r.txt"); echo "hash ", $n($b), "\n";
/// $b = get_resources(); parse_ini_file("$d/r.txt"); echo "ini ", $n($b), "\n";
/// $b = get_resources(); get_meta_tags("$d/r.txt"); @getimagesize("$d/r.txt"); echo "meta ", $n($b), "\n";
/// $b = get_resources(); highlight_file("$d/r.txt", true); php_strip_whitespace("$d/r.txt"); echo "source ", $n($b), "\n";
/// $b = get_resources(); scandir($d); copy("$d/r.txt", "$d/s.txt"); error_log("x", 3, "$d/r.log"); echo "dir ", $n($b), "\n";
/// ```
/// ```expect
/// fpc 0
/// fgc 0
/// file 0
/// abc readfile 0
/// hash 0
/// ini 0
/// meta 0
/// source 0
/// dir 0
/// ```
#[php_function]
fn file_get_contents(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] use_include_path: bool,
    #[php(default = null)] context: Option<Resource>,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] length: Option<i64>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Path(filename) = filename;
    let maxlen = match length {
        Some(len) if len < 0 => {
            return Err(arg_value_error(req, 5, "be greater than or equal to 0"));
        }
        Some(len) => Some(usize::try_from(len).unwrap_or(usize::MAX)),
        None => None,
    };
    let context = context_of(req, context, NullContext::Default)?;
    let Some(stream) = req.with_stream_ctx(context, |req| {
        req.open_stream(filename.as_bytes(), b"rb", use_include_path)
    })?
    else {
        return Ok(OrFalse::False);
    };
    if stream.is_stdio(req) {
        let unbuffered = StreamOption::ReadBuffer {
            buffered: false,
            size: i64::from(libc::BUFSIZ),
        };
        stream.set_option(req, unbuffered);
    }
    if offset != 0 {
        let whence = if offset > 0 { Whence::Set } else { Whence::End };
        if !stream.seek(req, offset, whence) {
            docref(
                req,
                Level::Warning,
                &format!("Failed to seek to position {offset} in the stream"),
            );
            stream.free(req);
            return Ok(OrFalse::False);
        }
    }
    let contents = match stream.copy_to_mem(req, maxlen) {
        Ok(contents) => contents,
        Err(thrown) => {
            stream.give_up(req);
            return Err(thrown);
        }
    };
    stream.free(req);
    Ok(OrFalse::Value(contents))
}

/// `file_put_contents(string $filename, mixed $data, int $flags = 0, $context = null): int|false`,
/// `PHP_FUNCTION(file_put_contents)` (`Z/ext/standard/file.c:439`): the
/// file opened `wb`, or `ab` under `FILE_APPEND`, or `cb` under
/// `LOCK_EX`, which only a path with no scheme or `file://` may ask for
/// and which truncates after `flock(2)` took the file; then `$data` written: a
/// stream copied whole, a scalar or `null` as its string, an array's
/// elements one after the other as strings, an object through its
/// `__toString`. What was written is the count, and `false` is an object
/// that has no `__toString`, a write that took less, or a file that did
/// not open.
///
/// ```php
/// $p = __DIR__ . '/fpc.txt';
/// var_dump(file_put_contents($p, "one"), file_get_contents($p), file_put_contents($p, 12, FILE_APPEND), file_get_contents($p));
/// var_dump(file_put_contents($p, [1, 2.5, "x", null, true]), file_get_contents($p), file_put_contents($p, "lock", LOCK_EX), file_get_contents($p));
/// class S { function __toString(): string { return "str"; } }
/// var_dump(file_put_contents($p, new S), file_get_contents($p), file_put_contents($p, new stdClass), file_get_contents($p), file_put_contents($p, ""));
/// $m = fopen('php://memory', 'w+'); fwrite($m, "from stream"); rewind($m);
/// var_dump(file_put_contents($p, $m), file_get_contents($p), file_put_contents('php://memory', "x"));
/// var_dump(file_put_contents('php://memory', "x", LOCK_EX), file_put_contents(__DIR__, "x"));
/// try { file_put_contents($p, "x", 0, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// string(3) "one"
/// int(2)
/// string(5) "one12"
/// int(6)
/// string(6) "12.5x1"
/// int(4)
/// string(4) "lock"
/// int(3)
/// string(3) "str"
/// bool(false)
/// string(0) ""
/// int(0)
/// int(11)
/// string(11) "from stream"
/// int(1)
///
/// Warning: file_put_contents(): Exclusive locks may only be set for regular files in %s on line 9
///
/// Warning: file_put_contents(%s): Failed to open stream: Is a directory in %s on line 9
/// bool(false)
/// bool(false)
/// file_put_contents(): Argument #4 ($context) must be of type resource or null, int given
/// ```
///
/// An array element that is itself an array is php's warning and the
/// word `Array`.
///
/// ```php
/// $p = __DIR__ . '/fpc.txt'; var_dump(file_put_contents($p, ["a", ["b"]]), file_get_contents($p));
/// ```
/// ```expect
///
/// Warning: Array to string conversion in %s on line 2
/// int(6)
/// string(6) "aArray"
/// ```
#[php_function]
fn file_put_contents(
    req: &mut Request,
    filename: Path,
    data: Value,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<i64>, Thrown> {
    let Path(filename) = filename;
    let context = context_of(req, context, no_default_context(flags))?;
    let source = match data.as_ref() {
        ValueRef::Resource(_) => match Stream::from_value(req, &data) {
            Some(stream) => Some(stream),
            None => {
                return Err(type_error(
                    req,
                    "file_put_contents(): supplied resource is not a valid stream resource",
                ));
            }
        },
        _ => None,
    };
    let name = filename.as_bytes();
    let mode: &[u8] = if flags & FILE_APPEND != 0 {
        b"ab"
    } else if flags & FLOCK_EX != 0 {
        let has_scheme = name.windows(3).any(|w| w == b"://");
        if has_scheme && !(name.len() >= 7 && name[..7].eq_ignore_ascii_case(b"file://")) {
            docref(
                req,
                Level::Warning,
                "Exclusive locks may only be set for regular files",
            );
            return Ok(OrFalse::False);
        }
        b"cb"
    } else {
        b"wb"
    };
    let use_include_path = flags & FILE_USE_INCLUDE_PATH != 0;
    let Some(stream) =
        req.with_stream_ctx(context, |req| req.open_stream(name, mode, use_include_path))?
    else {
        return Ok(OrFalse::False);
    };
    if flags & FLOCK_EX != 0 && !stream.flock(req, FLOCK_EX) {
        stream.free(req);
        docref(
            req,
            Level::Warning,
            "Exclusive locks are not supported for this stream",
        );
        return Ok(OrFalse::False);
    }
    if mode[0] == b'c'
        && let Err(thrown) = stream.truncate(req, 0)
    {
        stream.give_up(req);
        return Err(thrown);
    }
    let written = write_data(req, *stream, source, &data, name);
    stream.free(req);
    Ok(match written? {
        Some(n) => OrFalse::Value(i64::try_from(n).unwrap_or(i64::MAX)),
        None => OrFalse::False,
    })
}

/// `file_put_contents`'s switch over `$data`: how many bytes went to
/// `stream`, `None` for php's `-1`.
fn write_data(
    req: &mut Request,
    stream: Stream,
    source: Option<Stream>,
    data: &Value,
    filename: &[u8],
) -> Result<Option<usize>, Thrown> {
    let whole = |req: &mut Request, bytes: &[u8]| -> Option<usize> {
        if bytes.is_empty() {
            return Some(0);
        }
        let wrote = stream.write(req, bytes)?;
        if wrote != bytes.len() {
            docref(
                req,
                Level::Warning,
                &format!(
                    "Only {wrote} of {} bytes written, possibly out of free disk space",
                    bytes.len()
                ),
            );
            return None;
        }
        Some(wrote)
    };
    match data.as_ref() {
        ValueRef::Resource(_) => {
            let Some(source) = source else {
                return Ok(None);
            };
            let bytes = source.copy_to_mem(req, None)?;
            let wrote = if bytes.is_empty() {
                Some(0)
            } else {
                stream.write(req, bytes.as_bytes())
            };
            bytes.give_up(req);
            Ok(wrote)
        }
        ValueRef::Null
        | ValueRef::Bool(_)
        | ValueRef::Int(_)
        | ValueRef::Float(_)
        | ValueRef::Str(_) => {
            let text = to_string(req, data)?;
            let wrote = whole(req, text.as_bytes());
            text.give_up(req);
            Ok(wrote)
        }
        ValueRef::Array(array) => {
            let mut numbytes = 0;
            // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(data), tmp)`
            // (`Z/ext/standard/file.c:531`), no owner of the table taken:
            // the argument holds it while `__toString` and the stream's
            // wrapper run.
            let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
                let element = row.value().deref();
                let text = to_string(req, &element)?;
                let len = text.len();
                if len > 0 {
                    numbytes += len;
                    let wrote = stream.write(req, text.as_bytes());
                    if wrote != Some(len) {
                        let name = String::from_utf8_lossy(filename).into_owned();
                        docref(
                            req,
                            Level::Warning,
                            &format!("Failed to write {len} bytes to {name}"),
                        );
                        text.give_up(req);
                        return Ok(ControlFlow::Break(()));
                    }
                }
                text.give_up(req);
                Ok(ControlFlow::Continue(()))
            })?;
            if walked.is_some() {
                return Ok(None);
            }
            Ok(Some(numbytes))
        }
        ValueRef::Object(object) => {
            if !has_to_string(req, object)? {
                return Ok(None);
            }
            let text = to_string(req, data)?;
            let wrote = whole(req, text.as_bytes());
            text.give_up(req);
            Ok(wrote)
        }
        ValueRef::Undef | ValueRef::Reference(_) => Ok(None),
    }
}

/// `ce->__tostring`, which `zend_std_cast_object_tostring` reads before
/// it calls anything: a `__toString` the class or a class it extends,
/// or one of their traits, declares, or `Stringable` among what it
/// implements.
fn has_to_string(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    if let Some(stringable) = lookup_class(req, b"Stringable")?
        && object.instance_of(req, stringable)
    {
        return Ok(true);
    }
    // `ce->__tostring`: the function table's row, a trait's method and
    // an inherited one among them.
    Ok(req
        .symbols()
        .method_row(object.class(), b"__tostring")
        .is_some())
}

/// `stream_get_meta_data($stream): array`,
/// `PHP_FUNCTION(stream_get_meta_data)`
/// (`Z/ext/standard/streamsfuncs.c:514`): what the stream's kind reports
/// of itself, then `wrapper_type`, `stream_type`, `mode`, `unread_bytes`,
/// `seekable` and `uri`. A `php://temp` reports nothing of itself, and a
/// plain file's `eof` is what its last read left, not `feof`'s answer.
///
/// ```php
/// var_dump(stream_get_meta_data(fopen('php://temp', 'r+')), stream_get_meta_data(fopen('php://memory', 'a')));
/// $p = __DIR__ . '/meta.txt'; $w = fopen($p, 'w'); fwrite($w, "a\nb\n"); fclose($w);
/// $r = fopen($p, 'r'); fgets($r); var_dump(stream_get_meta_data($r));
/// var_dump(stream_get_meta_data(opendir(__DIR__)), stream_get_meta_data(fopen('php://output', 'w')));
/// ```
/// ```expect
/// array(6) {
///   ["wrapper_type"]=>
///   string(3) "PHP"
///   ["stream_type"]=>
///   string(4) "TEMP"
///   ["mode"]=>
///   string(3) "w+b"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(true)
///   ["uri"]=>
///   string(10) "php://temp"
/// }
/// array(9) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(3) "PHP"
///   ["stream_type"]=>
///   string(6) "MEMORY"
///   ["mode"]=>
///   string(3) "a+b"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(true)
///   ["uri"]=>
///   string(12) "php://memory"
/// }
/// array(9) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(9) "plainfile"
///   ["stream_type"]=>
///   string(5) "STDIO"
///   ["mode"]=>
///   string(1) "r"
///   ["unread_bytes"]=>
///   int(2)
///   ["seekable"]=>
///   bool(true)
///   ["uri"]=>
///   string(%d) "%s/meta.txt"
/// }
/// array(8) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(9) "plainfile"
///   ["stream_type"]=>
///   string(3) "dir"
///   ["mode"]=>
///   string(1) "r"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(true)
/// }
/// array(9) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["wrapper_type"]=>
///   string(3) "PHP"
///   ["stream_type"]=>
///   string(6) "Output"
///   ["mode"]=>
///   string(2) "wb"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(false)
///   ["uri"]=>
///   string(12) "php://output"
/// }
/// ```
#[php_function(aliases(socket_get_status))]
fn stream_get_meta_data(req: &mut Request, stream: Stream) -> Result<Owner<PhpArray>, Thrown> {
    stream.meta_data(req)
}

/// `rmdir(string $directory, $context = null): bool`,
/// `PHP_FUNCTION(rmdir)` (`Z/ext/standard/file.c:1102`): the wrapper's
/// `stream_rmdir`, the plain files wrapper's `rmdir(2)` with php's
/// warning, the path and `strerror(errno)`, where it failed; a wrapper
/// without one is `false` and says nothing.
///
/// ```php
/// $d = __DIR__; mkdir("$d/empty"); mkdir("$d/full"); touch("$d/full/f");
/// var_dump(rmdir("$d/empty"), is_dir("$d/empty"), rmdir("$d/full"), rmdir("$d/nope"), rmdir('php://memory'));
/// ```
/// ```expect
/// Warning: rmdir(%s/full): Directory not empty in %s on line 3
///
/// Warning: rmdir(%s/nope): No such file or directory in %s on line 3
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn rmdir(
    req: &mut Request,
    directory: Path,
    #[php(default = null)] context: Option<Resource>,
) -> Result<bool, Thrown> {
    let Path(dir) = directory;
    let context = context_of(req, context, NullContext::Default)?;
    req.with_stream_ctx(context, |req| {
        req.wrapper_op(dir.as_bytes(), WrapperOp::Rmdir)
    })
}

/// `umask(?int $mask = null): int`, `PHP_FUNCTION(umask)`
/// (`Z/ext/standard/file.c:1153`): the mask as it stood, and the new
/// one set where one is given, `umask(2)`'s nine bits of it. The mask is
/// the request's, and a file or directory the request makes after is
/// given `mode & ~mask` by it.
///
/// ```php
/// $d = __DIR__; $old = umask();
/// var_dump($old, umask(0), umask(), umask(0777), umask()); umask(0);
/// mkdir("$d/open", 0777); touch("$d/open.txt"); fclose(fopen("$d/w.txt", 'w'));
/// umask(077); mkdir("$d/closed", 0777); touch("$d/closed.txt"); umask($old);
/// printf("%o %o %o %o %o\n", fileperms("$d/open"), fileperms("$d/open.txt"), fileperms("$d/w.txt"), fileperms("$d/closed"), fileperms("$d/closed.txt"));
/// ```
/// ```expect
/// int(18)
/// int(18)
/// int(0)
/// int(0)
/// int(511)
/// 40777 100666 100666 40700 100600
/// ```
#[php_function]
fn umask(req: &mut Request, #[php(default = null)] mask: Option<i64>) -> i64 {
    let old = req.environment().umask();
    if let Some(mask) = mask {
        req.environment_mut()
            .set_umask(u32::try_from(mask & UMASK_BITS).unwrap_or(0));
    }
    i64::from(old)
}

/// The bits of a mask `umask(2)` keeps: `ALLPERMS` on macOS, the nine
/// permission bits elsewhere.
#[cfg(target_os = "macos")]
const UMASK_BITS: i64 = 0o7777;
#[cfg(not(target_os = "macos"))]
const UMASK_BITS: i64 = 0o777;

/// `mkdir(string $directory, int $permissions = 0777, bool $recursive = false, $context = null): bool`,
/// `PHP_FUNCTION(mkdir)` (`Z/ext/standard/file.c:1078`): the wrapper's
/// `stream_mkdir`, which for a plain path is `mkdir(2)` with php's
/// `strerror(errno)` warning where it failed, and under `$recursive`
/// every missing parent made first, one that exists passed over; a
/// wrapper without one is `false` and says nothing.
///
/// ```php
/// $d = __DIR__;
/// var_dump(mkdir("$d/a"), is_dir("$d/a"), mkdir("$d/a"), mkdir("$d/b/c"), mkdir("$d/b/c/d", 0777, true), is_dir("$d/b/c/d"));
/// var_dump(mkdir("$d/b/c/d", 0777, true), mkdir("$d/b//e/./f/", 0777, true), is_dir("$d/b/e/f"), mkdir("file://$d/g"), is_dir("$d/g"), mkdir('php://memory'));
/// ```
/// ```expect
/// Warning: mkdir(): File exists in %s on line 3
///
/// Warning: mkdir(): No such file or directory in %s on line 3
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
///
/// Warning: mkdir(): File exists in %s on line 4
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn mkdir(
    req: &mut Request,
    directory: Path,
    #[php(default = 0777)] permissions: i64,
    #[php(default = false)] recursive: bool,
    #[php(default = null)] context: Option<Resource>,
) -> Result<bool, Thrown> {
    let Path(dir) = directory;
    let context = context_of(req, context, NullContext::Default)?;
    let op = WrapperOp::Mkdir {
        mode: permissions,
        recursive,
    };
    req.with_stream_ctx(context, |req| req.wrapper_op(dir.as_bytes(), op))
}

/// `rename(string $from, string $to, $context = null): bool`,
/// `PHP_FUNCTION(rename)` (`Z/ext/standard/file.c:1196`): the source's
/// wrapper's `rename`, php's warning where it has none, and where the
/// target is another wrapper's; the plain files wrapper's is
/// `rename(2)`, its failure php's warning with both paths.
///
/// ```php
/// $d = __DIR__; file_put_contents("$d/from.txt", "moved");
/// var_dump(rename("$d/from.txt", "$d/to.txt"), file_exists("$d/from.txt"), file_get_contents("$d/to.txt"));
/// var_dump(rename("$d/nope", "$d/x"), rename("$d/to.txt", 'php://memory'), rename('php://memory', "$d/x"), rename("file://$d/to.txt", "$d/again.txt"), file_exists("$d/again.txt"));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(5) "moved"
///
/// Warning: rename(%s/nope,%s/x): No such file or directory in %s on line 4
///
/// Warning: rename(): Cannot rename a file across wrapper types in %s on line 4
///
/// Warning: rename(): PHP wrapper does not support renaming in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn rename(
    req: &mut Request,
    from: Path,
    to: Path,
    #[php(default = null)] context: Option<Resource>,
) -> Result<bool, Thrown> {
    let (Path(from), Path(to)) = (from, to);
    let context = context_of(req, context, NullContext::Default)?;
    let op = WrapperOp::Rename { to: to.as_bytes() };
    req.with_stream_ctx(context, |req| req.wrapper_op(from.as_bytes(), op))
}

/// `php_copy_file_ctx` (`Z/ext/standard/file.c:1447`): the source
/// stated, a directory being php's warning, and one that cannot be
/// stated copied without more; then the target, a directory php's
/// warning and the same file by device and inode `false` with nothing
/// said; then the source opened `rb` and the target `wb`, each failure
/// php's `Failed to open stream` warning, and every byte read written.
fn copy_file(req: &mut Request, source: &[u8], target: &[u8]) -> Result<bool, Thrown> {
    const S_IFMT: i64 = 0o170_000;
    const S_IFDIR: i64 = 0o040_000;
    if let Some(src) = req.url_stat(source, false, false)?.found() {
        if src[2] & S_IFMT == S_IFDIR {
            docref(
                req,
                Level::Warning,
                "The first argument to copy() function cannot be a directory",
            );
            return Ok(false);
        }
        if let Some(dest) = req.wrapper_url_stat(target)?.found() {
            if dest[2] & S_IFMT == S_IFDIR {
                docref(
                    req,
                    Level::Warning,
                    "The second argument to copy() function cannot be a directory",
                );
                return Ok(false);
            }
            if src[1] == 0 || dest[1] == 0 {
                if req.environment().absolutize(source) == req.environment().absolutize(target) {
                    return Ok(false);
                }
            } else if src[1] == dest[1] && src[0] == dest[0] {
                return Ok(false);
            }
        }
    }
    let Some(from) = req.open_stream(source, b"rb", false)? else {
        return Ok(false);
    };
    let mut done = false;
    let to = match req.open_stream(target, b"wb", false) {
        Ok(to) => to,
        Err(thrown) => {
            from.give_up(req);
            return Err(thrown);
        }
    };
    if let Some(to) = to {
        done = crate::streamsfuncs::copy_stream(req, *from, *to, None).is_some();
        to.free(req);
    }
    from.free(req);
    Ok(done)
}

/// `copy(string $from, string $to, $context = null): bool`,
/// `PHP_FUNCTION(copy)` (`Z/ext/standard/file.c:1408`): the source's
/// wrapper located as the `open_basedir` check does, then
/// `php_copy_file_ctx`.
///
/// ```php
/// $d = __DIR__; file_put_contents("$d/src.txt", str_repeat("x", 10000));
/// var_dump(copy("$d/src.txt", "$d/dst.txt"), filesize("$d/dst.txt"), copy("$d/src.txt", "$d/src.txt"), copy($d, "$d/y"), copy("$d/src.txt", $d), copy("$d/nope", "$d/y"));
/// var_dump(copy('php://memory', "$d/empty.txt"), filesize("$d/empty.txt"), copy("$d/src.txt", "$d/nodir/y"));
/// ```
/// ```expect
/// Warning: copy(): The first argument to copy() function cannot be a directory in %s on line 3
///
/// Warning: copy(): The second argument to copy() function cannot be a directory in %s on line 3
///
/// Warning: copy(%s/nope): Failed to open stream: No such file or directory in %s on line 3
/// bool(true)
/// int(10000)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
///
/// Warning: copy(%s/nodir/y): Failed to open stream: No such file or directory in %s on line 4
/// bool(true)
/// int(0)
/// bool(false)
/// ```
#[php_function]
fn copy(
    req: &mut Request,
    from: Path,
    to: Path,
    #[php(default = null)] context: Option<Resource>,
) -> Result<bool, Thrown> {
    let (Path(source), Path(target)) = (from, to);
    if let Some(local) = req.plain_path(source.as_bytes())
        && req
            .environment_mut()
            .check_open_basedir(&local, true)
            .is_err()
    {
        return Ok(false);
    }
    let context = context_of(req, context, NullContext::Default)?;
    req.with_stream_ctx(context, |req| {
        copy_file(req, source.as_bytes(), target.as_bytes())
    })
}

/// `tempnam(string $directory, string $prefix): string|false`,
/// `PHP_FUNCTION(tempnam)` (`Z/ext/standard/file.c:676`): the prefix's
/// basename, cut to 63 bytes, and a new `0600` file made in the
/// directory, or after php's notice in the temporary directory, whose
/// path is returned; the file is closed and stays.
///
/// ```php
/// $d = __DIR__; $t = tempnam($d, 'pre'); var_dump($t, file_exists($t), filesize($t)); printf("%o\n", fileperms($t) & 0777);
/// var_dump(tempnam($d, 'x/y/zz'), $n = tempnam("$d/nodir", 'n'), unlink($n), strlen(tempnam($d, str_repeat('p', 80))) - strlen($t));
/// ```
/// ```expect
/// string(%d) "%s/pre%s"
/// bool(true)
/// int(0)
/// 600
///
/// Notice: tempnam(): file created in the system's temporary directory in %s on line 3
/// string(%d) "%s/zz%s"
/// string(%d) "%s/n%s"
/// bool(true)
/// int(60)
/// ```
///
/// The fallback is `php_get_temporary_directory`'s, the directory
/// `sys_get_temp_dir()` names.
///
/// ```php
/// $f = tempnam(__DIR__ . '/none/x', 't'); var_dump(dirname($f) === realpath(sys_get_temp_dir())); unlink($f);
/// $f = tmpfile(); var_dump(dirname(stream_get_meta_data($f)['uri']) === realpath(sys_get_temp_dir()));
/// ```
/// ```expect
/// Notice: tempnam(): file created in the system's temporary directory in %s on line 2
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn tempnam(
    req: &mut Request,
    directory: Path,
    prefix: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (Path(dir), Path(prefix)) = (directory, prefix);
    let prefix = crate::string::path::php_basename(prefix.as_bytes(), b"");
    let prefix = &prefix[..prefix.len().min(63)];
    Ok(req
        .temporary_name(dir.as_bytes(), prefix)?
        .map_or(OrFalse::False, OrFalse::Value))
}

/// `tmpfile()`, `PHP_FUNCTION(tmpfile)` (`Z/ext/standard/file.c:705`):
/// `php_stream_fopen_tmpfile`, a new file in the temporary directory
/// opened `r+b` that is unlinked when it closes, or `false`.
///
/// ```php
/// $f = tmpfile(); var_dump($f); fwrite($f, "abc"); rewind($f); var_dump(fread($f, 10));
/// $m = stream_get_meta_data($f); var_dump($m['mode'], $m['wrapper_type'], file_exists($m['uri'])); fclose($f); var_dump(file_exists($m['uri']));
/// ```
/// ```expect
/// resource(%d) of type (stream)
/// string(3) "abc"
/// string(3) "r+b"
/// string(9) "plainfile"
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn tmpfile(req: &mut Request) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    Ok(Untyped(match req.tmpfile()? {
        Some(stream) => OrFalse::Value(stream.into_resource()),
        None => OrFalse::False,
    }))
}

/// `unlink(string $filename, $context = null): bool`,
/// `PHP_FUNCTION(unlink)` (`Z/ext/standard/file.c:1235`): the wrapper's
/// `unlink`, php's warning where there is none; the plain files
/// wrapper's is `unlink(2)`, its failure php's warning with the path and
/// `strerror(errno)`.
///
/// ```php
/// $d = __DIR__; touch("$d/u.txt"); touch("$d/v.txt");
/// var_dump(unlink("$d/u.txt"), file_exists("$d/u.txt"), unlink("$d/u.txt"), unlink("file://$d/v.txt"), file_exists("$d/v.txt"), unlink('php://memory'), unlink(''));
/// ```
/// ```expect
/// Warning: unlink(%s/u.txt): No such file or directory in %s on line 3
///
/// Warning: unlink(): PHP does not allow unlinking in %s on line 3
///
/// Warning: unlink(): No such file or directory in %s on line 3
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn unlink(
    req: &mut Request,
    filename: Path,
    #[php(default = null)] context: Option<Resource>,
) -> Result<bool, Thrown> {
    let Path(filename) = filename;
    let context = context_of(req, context, NullContext::Default)?;
    req.with_stream_ctx(context, |req| {
        req.wrapper_op(filename.as_bytes(), WrapperOp::Unlink)
    })
}

/// `file(string $filename, int $flags = 0, $context = null): array|false`,
/// `PHP_FUNCTION(file)` (`Z/ext/standard/file.c:581`): the file read
/// whole and cut after each `\n`; under `FILE_IGNORE_NEW_LINES` the
/// `\n` (and a `\r` before it) is dropped, and under
/// `FILE_SKIP_EMPTY_LINES` as well a line that is then empty is too. A
/// last line with no `\n` is kept as it is.
///
/// ```php
/// $f = sys_get_temp_dir() . '/php-file-witness.txt';
/// file_put_contents($f, "a\r\n\nb\nc");
/// echo json_encode(file($f)), "\n", json_encode(file($f, FILE_IGNORE_NEW_LINES)), "\n", json_encode(file($f, FILE_IGNORE_NEW_LINES | FILE_SKIP_EMPTY_LINES)), "\n";
/// file_put_contents($f, ""); var_dump(file($f));
/// unlink($f);
/// try { file($f, 64); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(@file($f));
/// ```
/// ```expect
/// ["a\r\n","\n","b\n","c"]
/// ["a","","b","c"]
/// ["a","b","c"]
/// array(0) {
/// }
/// file(): Argument #2 ($flags) must be a valid flag value
/// bool(false)
/// ```
#[php_function]
fn file(
    req: &mut Request,
    filename: Path,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Path(filename) = filename;
    let known = FILE_USE_INCLUDE_PATH
        | FILE_IGNORE_NEW_LINES
        | FILE_SKIP_EMPTY_LINES
        | FILE_NO_DEFAULT_CONTEXT;
    if flags & !known != 0 {
        return Err(arg_value_error(req, 2, "be a valid flag value"));
    }
    let context = context_of(req, context, no_default_context(flags))?;
    let include_new_line = flags & FILE_IGNORE_NEW_LINES == 0;
    let skip_blank_lines = flags & FILE_SKIP_EMPTY_LINES != 0;
    let use_include_path = flags & FILE_USE_INCLUDE_PATH != 0;
    let Some(stream) = req.with_stream_ctx(context, |req| {
        req.open_stream(filename.as_bytes(), b"rb", use_include_path)
    })?
    else {
        return Ok(OrFalse::False);
    };
    let contents = match stream.copy_to_mem(req, None) {
        Ok(contents) => contents,
        Err(thrown) => {
            stream.give_up(req);
            return Err(thrown);
        }
    };
    stream.free(req);
    let buf = contents.as_bytes().to_vec();
    // `zend_string_free(target_buf)`: the lines are copies.
    contents.give_up(req);
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut s = 0;
    for (p, _) in buf.iter().enumerate().filter(|(_, c)| **c == b'\n') {
        if include_new_line {
            lines.push(&buf[s..=p]);
        } else {
            let windows_eol = usize::from(p > 0 && buf[p - 1] == b'\r');
            if skip_blank_lines && p - s == windows_eol {
                s = p + 1;
                continue;
            }
            lines.push(&buf[s..p - windows_eol]);
        }
        s = p + 1;
    }
    if s != buf.len() {
        lines.push(&buf[s..]);
    }
    let mut array = PhpArray::new(req)?;
    for line in lines {
        let v = PhpString::new(req, line)?.into_value();
        array.append(req, v)?;
    }
    Ok(OrFalse::Value(array))
}

/// `flock($stream, int $operation, &$would_block = null): bool`,
/// `PHP_FUNCTION(flock)` over `php_flock_common`
/// (`Z/ext/standard/file.c:185`): the operation's low two bits are one
/// of `LOCK_SH`, `LOCK_EX` or `LOCK_UN`, else php's `ValueError`;
/// `$would_block` is zeroed, then `flock(2)` with `LOCK_NB` where the
/// operation asks for it, and a refused `LOCK_NB` request sets it
/// to one, which is `EWOULDBLOCK`'s answer. A stream with no `flock(2)`,
/// `php://memory` among them, answers `false`.
///
/// ```php
/// $p = __DIR__ . '/flock.txt'; touch($p);
/// $a = fopen($p, 'r'); $b = fopen($p, 'r');
/// var_dump(flock($a, LOCK_EX), flock($b, LOCK_EX | LOCK_NB, $wb), $wb, flock($a, LOCK_UN), flock($b, LOCK_SH | LOCK_NB, $wb), $wb, flock(fopen('php://memory', 'r'), LOCK_SH));
/// try { flock($a, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// int(1)
/// bool(true)
/// bool(true)
/// int(0)
/// bool(false)
/// flock(): Argument #2 ($operation) must be one of LOCK_SH, LOCK_EX, or LOCK_UN
/// ```
#[php_function]
fn flock(
    req: &mut Request,
    stream: Stream,
    operation: i64,
    #[php(default = null)] would_block: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let act = operation & LOCK_UN;
    if !(1..=3).contains(&act) {
        return Err(arg_value_error(
            req,
            2,
            "be one of LOCK_SH, LOCK_EX, or LOCK_UN",
        ));
    }
    would_block.set(req, Untyped(Value::int(0)))?;
    // `flock_values[act - 1]`: `LOCK_SH`, `LOCK_EX` and `LOCK_UN` are
    // `flock(2)`'s 1, 2 and 8, and `LOCK_NB` its 4.
    let lock = [1, 2, 8][usize::try_from(act - 1).unwrap_or(0)];
    let non_blocking = operation & LOCK_NB != 0;
    let lock = lock | if non_blocking { 4 } else { 0 };
    if stream.flock(req, lock) {
        return Ok(true);
    }
    if non_blocking && stream.is_stdio(req) {
        would_block.set(req, Untyped(Value::int(1)))?;
    }
    Ok(false)
}

/// `PHP_META_UNSAFE`: the bytes a meta tag's name has replaced by `_`.
const META_UNSAFE: &[u8] = b".\\+*?[^]$() ";

/// `META_DEF_BUFSIZE`: the longest token `php_next_meta_token` gathers.
const META_DEF_BUFSIZE: usize = 8192;

/// `php_meta_tags_token`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MetaToken {
    Eof,
    OpenTag,
    CloseTag,
    Slash,
    Equal,
    Space,
    Id,
    String,
    Other,
}

/// `php_meta_tags_data`: the stream, the byte put back in place of
/// `ungetc`, whether the tag is a `meta`, and the token's bytes.
struct MetaData {
    stream: Owner<Stream>,
    lc: Option<i32>,
    in_meta: bool,
    token: Option<Vec<u8>>,
}

impl MetaData {
    /// `php_stream_getc`: the next byte, or `EOF`'s `-1`, read into the
    /// one `char` php reads it into, so no string is made for it.
    fn getc(&self, req: &mut Request) -> i32 {
        let mut byte = [0];
        match self.stream.read_into(req, &mut byte) {
            Some(1) => i32::from(byte[0]),
            _ => -1,
        }
    }

    /// `php_next_meta_token` (`Z/ext/standard/file.c:2140`), a byte at
    /// a time as php reads it, `eof` read after each byte, so the byte
    /// that brings the stream to its end is dropped as php's loop drops
    /// it. A name is `isalnum(3)`'s and `isalpha(3)`'s under the thread's
    /// `LC_CTYPE`, and `EOF` in neither.
    fn next(&mut self, req: &mut Request) -> Result<MetaToken, Thrown> {
        let alnum = |ch: i32| u8::try_from(ch).is_ok_and(|c| c::is(Class::Alnum, c));
        loop {
            let mut ch = if self.lc.is_some() {
                0
            } else {
                if self.stream.eof(req) {
                    return Ok(MetaToken::Eof);
                }
                let ch = self.getc(req);
                if ch == 0 {
                    return Ok(MetaToken::Eof);
                }
                ch
            };
            if self.stream.eof(req) {
                return Ok(MetaToken::Eof);
            }
            if let Some(put_back) = self.lc.take() {
                ch = put_back;
            }
            match u8::try_from(ch).unwrap_or(0) {
                b'<' => return Ok(MetaToken::OpenTag),
                b'>' => return Ok(MetaToken::CloseTag),
                b'=' => return Ok(MetaToken::Equal),
                b'/' => return Ok(MetaToken::Slash),
                b'\'' | b'"' => {
                    let compliment = ch;
                    let mut buff = Vec::new();
                    loop {
                        if self.stream.eof(req) {
                            break;
                        }
                        ch = self.getc(req);
                        if ch == 0
                            || ch == compliment
                            || ch == i32::from(b'<')
                            || ch == i32::from(b'>')
                        {
                            break;
                        }
                        // `buff[len++] = ch`: `EOF` is the byte `0xff`.
                        buff.push(ch.to_le_bytes()[0]);
                        if buff.len() == META_DEF_BUFSIZE {
                            break;
                        }
                    }
                    if ch == i32::from(b'<') || ch == i32::from(b'>') {
                        // Was just an apostrophe.
                        self.lc = Some(ch);
                    }
                    if self.in_meta {
                        self.token = Some(buff);
                    }
                    return Ok(MetaToken::String);
                }
                b'\n' | b'\r' | b'\t' => {}
                b' ' => return Ok(MetaToken::Space),
                _ if alnum(ch) => {
                    let mut buff = vec![ch.to_le_bytes()[0]];
                    loop {
                        if self.stream.eof(req) {
                            break;
                        }
                        ch = self.getc(req);
                        let html401 = u8::try_from(ch).is_ok_and(|c| b"-_.:".contains(&c));
                        if ch == 0 || !(alnum(ch) || html401) {
                            break;
                        }
                        buff.push(ch.to_le_bytes()[0]);
                        if buff.len() == META_DEF_BUFSIZE {
                            break;
                        }
                    }
                    let alpha = u8::try_from(ch).is_ok_and(|c| c::is(Class::Alpha, c));
                    if !alpha && ch != i32::from(b'-') {
                        self.lc = Some(ch);
                    }
                    self.token = Some(buff);
                    return Ok(MetaToken::Id);
                }
                _ => return Ok(MetaToken::Other),
            }
        }
    }
}

/// `get_meta_tags(string $filename, bool $use_include_path = false): array|false`,
/// `PHP_FUNCTION(get_meta_tags)` (`Z/ext/standard/file.c:233`): the
/// file read through `php_next_meta_token` up to its `</head>`, and each
/// `<meta name=... content=...>` a pair, the name lowercased with
/// `PHP_META_UNSAFE`'s bytes as `_`, a tag with a name and no content an
/// empty string; `false` after the open's warning.
///
/// ```php
/// $p = __DIR__ . '/meta.html';
/// file_put_contents($p, "<html><head>\n<meta name=\"Author\" content=\"Jo Q\">\n<META NAME='key words' CONTENT='a, b'>\n<meta name=robots content=noindex>\n<meta name=\"empty\">\n<meta content=\"x\">\n</head><meta name=\"after\" content=\"no\"></html>");
/// var_dump(get_meta_tags($p), get_meta_tags(__DIR__ . '/nope.html'));
/// ```
/// ```expect
/// Warning: get_meta_tags(%s/nope.html): Failed to open stream: No such file or directory in %s on line 4
/// array(4) {
///   ["author"]=>
///   string(4) "Jo Q"
///   ["key_words"]=>
///   string(4) "a, b"
///   ["robots"]=>
///   string(7) "noindex"
///   ["empty"]=>
///   string(0) ""
/// }
/// bool(false)
/// ```
///
/// An unquoted name is `isalnum(3)`'s under the request's `LC_CTYPE`.
///
/// ```php
/// $f = __DIR__ . '/meta-locale.html'; file_put_contents($f, "<meta name=caf\xe9 content=x></head>");
/// foreach (['C', 'fr_FR.ISO8859-1'] as $l) { setlocale(LC_CTYPE, $l); var_dump(array_map('bin2hex', array_flip(get_meta_tags($f)))); }
/// unlink($f);
/// ```
/// ```expect locale=fr_FR.ISO8859-1
/// array(1) {
///   ["x"]=>
///   string(6) "636166"
/// }
/// array(1) {
///   ["x"]=>
///   string(8) "636166e9"
/// }
/// ```
#[php_function]
fn get_meta_tags(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] use_include_path: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Path(filename) = filename;
    let Some(stream) = req.open_stream(filename.as_bytes(), b"rb", use_include_path)? else {
        return Ok(OrFalse::False);
    };
    let mut md = MetaData {
        stream,
        lc: None,
        in_meta: false,
        token: None,
    };
    let tags = match PhpArray::new(req) {
        Ok(mut array) => match meta_tags(req, &mut md, &mut array) {
            Ok(()) => Ok(array),
            Err(thrown) => {
                array.give_up(req);
                Err(thrown)
            }
        },
        Err(thrown) => Err(thrown),
    };
    md.stream.free(req);
    Ok(OrFalse::Value(tags?))
}

/// The scan of `PHP_FUNCTION(get_meta_tags)` (`Z/ext/standard/file.c:233`)
/// over the stream `md` reads: each `<meta>`'s `name` and `content` set
/// in `array`, up to `</head>`.
#[allow(clippy::too_many_lines)] // one C function, kept whole
fn meta_tags(
    req: &mut Request,
    md: &mut MetaData,
    array: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    let (mut in_tag, mut looking_for_val) = (false, false);
    let (mut have_name, mut have_content) = (false, false);
    let (mut saw_name, mut saw_content) = (false, false);
    let mut name: Option<Vec<u8>> = None;
    let mut value: Option<Vec<u8>> = None;
    let mut tok_last = MetaToken::Eof;
    let unsafe_to_underscore = |bytes: &[u8]| -> Vec<u8> {
        bytes
            .iter()
            .map(|c| if META_UNSAFE.contains(c) { b'_' } else { *c })
            .collect()
    };
    loop {
        let tok = md.next(req)?;
        if tok == MetaToken::Eof {
            break;
        }
        let token = md.token.take();
        let is = |word: &[u8]| {
            token
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(word))
        };
        let mut done = false;
        match tok {
            MetaToken::Id => {
                if tok_last == MetaToken::OpenTag {
                    md.in_meta = is(b"meta");
                } else if tok_last == MetaToken::Slash && in_tag {
                    done = is(b"head");
                } else if tok_last == MetaToken::Equal && looking_for_val {
                    if saw_name {
                        name = Some(unsafe_to_underscore(token.as_deref().unwrap_or_default()));
                        have_name = true;
                    } else if saw_content {
                        value.clone_from(&token);
                        have_content = true;
                    }
                    looking_for_val = false;
                } else if md.in_meta {
                    if is(b"name") {
                        (saw_name, saw_content, looking_for_val) = (true, false, true);
                    } else if is(b"content") {
                        (saw_name, saw_content, looking_for_val) = (false, true, true);
                    }
                }
            }
            MetaToken::String if tok_last == MetaToken::Equal && looking_for_val => {
                if saw_name {
                    name = Some(unsafe_to_underscore(token.as_deref().unwrap_or_default()));
                    have_name = true;
                } else if saw_content {
                    value.clone_from(&token);
                    have_content = true;
                }
                looking_for_val = false;
            }
            MetaToken::OpenTag => {
                if looking_for_val {
                    looking_for_val = false;
                    (have_name, saw_name, have_content, saw_content) = (false, false, false, false);
                }
                in_tag = true;
            }
            MetaToken::CloseTag => {
                if have_name {
                    let key = name.take().unwrap_or_default().to_ascii_lowercase();
                    let content = if have_content {
                        value.take().unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let content = PhpString::new(req, &content)?.into_value();
                    array.set_symtable_str(req, &key, content)?;
                }
                name = None;
                value = None;
                (in_tag, looking_for_val) = (false, false);
                (have_name, saw_name, have_content, saw_content) = (false, false, false, false);
                md.in_meta = false;
            }
            _ => {}
        }
        tok_last = tok;
        if done {
            break;
        }
    }
    Ok(())
}

/// `_php_stream_passthru` (`Z/main/streams/streams.c`): what is left
/// of the stream written to the request's body a read of up to 8192
/// bytes at a time, each read given up once written, as php's
/// `char buf[8192]` holds one read at a time, and how many bytes that
/// was.
fn passthru(req: &mut Request, stream: Stream) -> Result<i64, Thrown> {
    let mut size: i64 = 0;
    while let Some(chunk) = stream.read(req, 8192)? {
        let written = chunk.len();
        if written > 0 {
            req.response_mut().write(chunk.as_bytes());
        }
        chunk.give_up(req);
        if written == 0 {
            break;
        }
        size = size.saturating_add(i64::try_from(written).unwrap_or(i64::MAX));
    }
    Ok(size)
}

/// `readfile(string $filename, bool $use_include_path = false, $context = null): int|false`,
/// `PHP_FUNCTION(readfile)` (`Z/ext/standard/file.c:1122`): the file
/// opened `rb` and written through to the page whole, and how many
/// bytes it held; `false` after the open's warning.
///
/// ```php
/// $p = __DIR__ . '/readfile.txt'; file_put_contents($p, "one\ntwo\n");
/// var_dump(readfile($p), readfile(__DIR__ . '/nope.txt'));
/// ```
/// ```expect
/// one
/// two
///
/// Warning: readfile(%s/nope.txt): Failed to open stream: No such file or directory in %s on line 3
/// int(8)
/// bool(false)
/// ```
///
/// A file is written a read at a time, each read given up once
/// written, so a 2 MiB file raises the peak by no more than a stream's
/// buffers (through a filter, which php cannot map whole):
///
/// ```php
/// $p = __DIR__ . '/big.txt'; $f = fopen($p, 'w'); for ($i = 0; $i < 512; $i++) fwrite($f, str_repeat("\0", 4096)); fclose($f);
/// ob_start(fn($b) => '', 4096);
/// memory_reset_peak_usage(); $at = memory_get_usage();
/// $n = readfile("php://filter/read=string.toupper/resource=$p");
/// $rise = memory_get_peak_usage() - $at;
/// ob_end_clean();
/// var_dump($n, $rise < 65536);
/// ```
/// ```expect
/// int(2097152)
/// bool(true)
/// ```
#[php_function]
fn readfile(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] use_include_path: bool,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<i64>, Thrown> {
    let Path(filename) = filename;
    let context = context_of(req, context, NullContext::Default)?;
    let Some(stream) = req.with_stream_ctx(context, |req| {
        req.open_stream(filename.as_bytes(), b"rb", use_include_path)
    })?
    else {
        return Ok(OrFalse::False);
    };
    let size = match passthru(req, *stream) {
        Ok(size) => size,
        Err(thrown) => {
            stream.give_up(req);
            return Err(thrown);
        }
    };
    stream.free(req);
    Ok(OrFalse::Value(size))
}

/// `fpassthru($stream): int`, `PHP_FUNCTION(fpassthru)`
/// (`Z/ext/standard/file.c:1181`): the rest of the stream from where it
/// stands written through to the page, and how many bytes.
///
/// ```php
/// $m = fopen('php://memory', 'r+'); fwrite($m, "abcdef"); fseek($m, 2);
/// var_dump(fpassthru($m), fpassthru($m));
/// ```
/// ```expect
/// cdefint(4)
/// int(0)
/// ```
///
/// Each read is given up once written, so passing 2 MiB through raises
/// the peak by no more than a stream's buffers:
///
/// ```php
/// $m = fopen('php://memory', 'w+'); for ($i = 0; $i < 512; $i++) fwrite($m, str_repeat("\0", 4096)); rewind($m);
/// ob_start(fn($b) => '', 4096);
/// memory_reset_peak_usage(); $at = memory_get_usage();
/// $n = fpassthru($m);
/// $rise = memory_get_peak_usage() - $at;
/// ob_end_clean();
/// var_dump($n, $rise < 65536);
/// ```
/// ```expect
/// int(2097152)
/// bool(true)
/// ```
#[php_function]
pub fn fpassthru(req: &mut Request, stream: Stream) -> Result<i64, Thrown> {
    passthru(req, stream)
}

/// What `fscanf()` answers short of `false` and `null`: the values, or
/// the count assigned.
type Scanned = Union<Owner<PhpArray>, i64>;

/// `fscanf($stream, string $format, mixed &...$vars): array|int|false|null`,
/// `PHP_FUNCTION(fscanf)` (`Z/ext/standard/file.c:929`): the next line
/// of the stream, `php_stream_get_line` with no limit, scanned as
/// `sscanf` scans a string, into the variables where they are given;
/// `false` at the end of the stream. The stream is `Z_PARAM_RESOURCE`
/// then `zend_fetch_resource2` for a `File-Handle`, so a closed one is
/// php's `supplied resource` `TypeError`.
///
/// ```php
/// $m = fopen('php://memory', 'r+'); fwrite($m, "12 apples\nbob 7\n"); rewind($m);
/// var_dump(fscanf($m, "%d %s"), fscanf($m, "%s %d", $who, $n), $who, $n, fscanf($m, "%d"));
/// fclose($m);
/// try { fscanf($m, "%d"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { fscanf("x", "%d"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(12)
///   [1]=>
///   string(6) "apples"
/// }
/// int(2)
/// string(3) "bob"
/// int(7)
/// bool(false)
/// fscanf(): supplied resource is not a valid File-Handle resource
/// fscanf(): Argument #1 ($stream) must be of type resource, string given
/// ```
#[php_function]
fn fscanf(
    req: &mut Request,
    stream: Resource,
    format: PhpString,
    vars: Variadic<Ref<Value>>,
) -> Result<Option<OrFalse<Scanned>>, Thrown> {
    // `php_sscanf_internal` assigns through the call's own arguments from
    // the third on, `vars` as the frame holds them.
    let count = vars.len();
    let Some(stream) = Stream::from_value(req, &stream.as_value()) else {
        return Err(type_error(
            req,
            "fscanf(): supplied resource is not a valid File-Handle resource",
        ));
    };
    let Some(line) = stream.get_line(req, None)? else {
        return Ok(Some(OrFalse::False));
    };
    let scanned = crate::scanf::php_sscanf_internal(req, &line, format.as_bytes(), 3, count)?;
    let answer = match scanned.as_ref() {
        ValueRef::Array(array) => Some(OrFalse::Value(Union::A(Owner::take(req, array)))),
        ValueRef::Int(n) => Some(OrFalse::Value(Union::B(n))),
        ValueRef::Bool(false) => Some(OrFalse::False),
        _ => None,
    };
    scanned.give_up(req);
    Ok(answer)
}

/// `php_stream_sync_supported`, which the plain files kind and the
/// host's standard streams are, php's warning and `false` for any other;
/// then what was written flushed, and `php_stream_sync`, which is
/// `fsync(2)`, or `fdatasync(2)` for `data_only`, of the stream's fd.
fn sync(req: &mut Request, stream: Stream, data_only: bool) -> bool {
    if !stream.is_stdio(req) {
        docref(req, Level::Warning, "Can't fsync this stream!");
        return false;
    }
    stream.flush(req)
        && stream.set_option(req, StreamOption::Sync { data_only }) == OptionAnswer::Ok
}

/// `fsync($stream): bool`, `PHP_FUNCTION(fsync)`
/// (`Z/ext/standard/file.c:1266`): php's warning for a stream with no
/// `fsync(2)`, else what was written handed to the file and what
/// `fsync(2)` of its fd answers, which a pipe refuses.
///
/// ```php
/// $f = fopen(__DIR__ . '/fsync.txt', 'w'); fwrite($f, 'x');
/// var_dump(fsync($f), file_get_contents(__DIR__ . '/fsync.txt'), fsync(fopen('php://memory', 'r')));
/// ```
/// ```expect
/// Warning: fsync(): Can't fsync this stream! in %s on line 3
/// bool(true)
/// string(1) "x"
/// bool(false)
/// ```
///
/// ```php
/// $p = proc_open(['/bin/cat'], [['pipe', 'r'], ['pipe', 'w']], $pipes);
/// var_dump(fsync($pipes[0]), fsync($pipes[1]));
/// fclose($pipes[0]); fclose($pipes[1]); proc_close($p);
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn fsync(req: &mut Request, stream: Stream) -> bool {
    sync(req, stream, false)
}

/// `fdatasync($stream): bool`, `PHP_FUNCTION(fdatasync)`
/// (`Z/ext/standard/file.c:1282`): `fsync`'s answer with `data_only`,
/// `fdatasync(2)` of the fd.
///
/// ```php
/// $f = fopen(__DIR__ . '/fdatasync.txt', 'w'); fwrite($f, 'y');
/// var_dump(fdatasync($f), fdatasync(fopen('php://temp', 'r')));
/// $p = proc_open(['/bin/cat'], [['pipe', 'r'], ['pipe', 'w']], $pipes);
/// var_dump(fdatasync($pipes[0]), fdatasync($pipes[1]));
/// fclose($pipes[0]); fclose($pipes[1]); proc_close($p);
/// ```
/// ```expect
/// Warning: fdatasync(): Can't fsync this stream! in %s on line 3
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn fdatasync(req: &mut Request, stream: Stream) -> bool {
    sync(req, stream, true)
}

/// The one-byte `$separator` or `$enclosure` at `position`, or the
/// default where the call gave none, and php's `ValueError` for any
/// other length.
fn one_byte(req: &mut Request, given: PhpString, position: u32) -> Result<u8, Thrown> {
    match given.as_bytes() {
        [c] => Ok(*c),
        _ => Err(arg_value_error(req, position, "be a single character")),
    }
}

/// `fputcsv($stream, array $fields, ...): int|false`,
/// `PHP_FUNCTION(fputcsv)` over `php_fputcsv`
/// (`Z/ext/standard/file.c:1683`): each field as a string, enclosed
/// where it holds the separator, the enclosure, the escape, a line
/// break, a tab or a space, an enclosure inside doubled unless the
/// escape precedes it; the fields joined by the separator, `$eol` or
/// `\n` after, written at once; the bytes written, or `false`.
///
/// ```php
/// $m = fopen('php://memory', 'r+');
/// var_dump(fputcsv($m, ['a', 'b c', 'd"e', 7, 1.5, null, "x\ny", 'p\\"q'], ',', '"', '\\'), fputcsv($m, ['one', 'two'], ';', "'", '', "!\n"));
/// rewind($m); while (!feof($m)) { echo fread($m, 100); }
/// fputcsv($m, ['z']);
/// try { fputcsv($m, [], 'ab'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(35)
/// int(9)
/// a,"b c","d""e",7,1.5,,"x
/// y","p\"q"
/// one;two!
///
/// Deprecated: fputcsv(): the $escape parameter must be provided as its default value will change in %s on line 5
/// fputcsv(): Argument #3 ($separator) must be a single character
/// ```
#[php_function]
fn fputcsv(
    req: &mut Request,
    stream: Stream,
    fields: PhpArray,
    #[php(default = ",")] separator: PhpString,
    #[php(default = "\"")] enclosure: PhpString,
    #[php(default = "\\")] escape: Passed<PhpString>,
    #[php(default = "\n")] eol: PhpString,
) -> Result<OrFalse<i64>, Thrown> {
    let delimiter = one_byte(req, separator, 3)?;
    let enclosure = one_byte(req, enclosure, 4)?;
    let escape = crate::csv::escape_of(req, escape, 5)?;
    let written = php_fputcsv(
        req,
        stream,
        fields,
        (delimiter, enclosure, escape),
        Some(eol.as_bytes()),
    )?;
    Ok(written.map_or(OrFalse::False, |n| {
        OrFalse::Value(i64::try_from(n).unwrap_or(i64::MAX))
    }))
}

/// `php_fputcsv(stream, fields, delimiter, enclosure, escape_char,
/// eol_str)`, `PHPAPI` in `file.h` (`Z/ext/standard/file.c:1683`): the
/// fields as one CSV line written to `stream`, `eol` or `\n` after; the
/// bytes written, `None` where the write failed. The control bytes are
/// the separator, the enclosure and the escape, `None` for
/// `PHP_CSV_NO_ESCAPE`. What `fputcsv()` writes through, and what
/// `SplFileObject::fputcsv()` calls directly.
///
/// ```php
/// $t = new SplTempFileObject();
/// var_dump($t->fputcsv(['a', 'b c', 'd"e'], ';', '"', ''), $t->fputcsv([1, null], ',', '"', '', "!\n"));
/// $t->rewind(); $t->fpassthru();
/// ```
/// ```expect
/// int(15)
/// int(4)
/// a;"b c";"d""e"
/// 1,!
/// ```
pub fn php_fputcsv(
    req: &mut Request,
    stream: Stream,
    fields: PhpArray,
    (delimiter, enclosure, escape): (u8, u8, Option<u8>),
    eol: Option<&[u8]>,
) -> Result<Option<usize>, Thrown> {
    let count = fields.len();
    let mut line = Vec::new();
    let mut i = 0;
    // `ZEND_HASH_FOREACH_VAL(fields, field_tmp)`, holding nothing: a
    // conversion may serve `__toString`.
    walk::foreach(req, fields, Hold::None, |req, row| -> walk::Step {
        with_string(req, row.value(), |bytes| {
            let special = [delimiter, enclosure, b'\n', b'\r', b'\t', b' ']
                .iter()
                .chain(escape.iter())
                .any(|c| bytes.contains(c));
            if special {
                line.push(enclosure);
                let mut escaped = false;
                for &ch in bytes {
                    if escape == Some(ch) {
                        escaped = true;
                    } else if !escaped && ch == enclosure {
                        line.push(enclosure);
                    } else {
                        escaped = false;
                    }
                    line.push(ch);
                }
                line.push(enclosure);
            } else {
                line.extend_from_slice(bytes);
            }
        })?;
        i += 1;
        if i != count {
            line.push(delimiter);
        }
        Ok(std::ops::ControlFlow::Continue(()))
    })?;
    match eol {
        Some(eol) => line.extend_from_slice(eol),
        None => line.push(b'\n'),
    }
    Ok(stream.write(req, &line))
}

/// `fgetcsv($stream, ?int $length = null, ...): array|false`,
/// `PHP_FUNCTION(fgetcsv)` (`Z/ext/standard/file.c:1749`): the next
/// line, at most `$length` bytes of it where one is given and not zero,
/// parsed by `php_fgetcsv` with the stream beside it, so an enclosed
/// field that runs past the line's end reads the lines after it; `false`
/// at the end of the stream, and php's one `null` for an empty line.
///
/// ```php
/// $m = fopen('php://memory', 'r+'); fwrite($m, "a,\"b\nc\",d\n\nx;y\n\"open"); rewind($m);
/// var_dump(fgetcsv($m, null, ',', '"', '\\'), fgetcsv($m, 0, ',', '"', ''), fgetcsv($m, 2, ';', '"', ''), fgetcsv($m, null, ';', '"', ''), fgetcsv($m, null, ',', '"', ''), fgetcsv($m, null, ',', '"', ''));
/// try { fgetcsv($m, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(3) "b
/// c"
///   [2]=>
///   string(1) "d"
/// }
/// array(1) {
///   [0]=>
///   NULL
/// }
/// array(2) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(0) ""
/// }
/// array(1) {
///   [0]=>
///   string(1) "y"
/// }
/// array(1) {
///   [0]=>
///   string(4) "open"
/// }
/// bool(false)
///
/// Deprecated: fgetcsv(): the $escape parameter must be provided as its default value will change in %s on line 4
/// fgetcsv(): Argument #2 ($length) must be between 0 and 9223372036854775806
/// ```
///
/// The walk is by `php_mblen`'s characters under the request's
/// `LC_CTYPE`, as `str_getcsv`'s.
///
/// ```php
/// $f = fopen('php://memory', 'w+'); fwrite($f, "\x95\\,x\n\x95\\,x\n"); rewind($f);
/// foreach (['C', 'ja_JP.SJIS'] as $l) { setlocale(LC_CTYPE, $l); var_dump(array_map('bin2hex', fgetcsv($f, null, "\\", '"', ""))); }
/// ```
/// ```expect locale=ja_JP.SJIS
/// array(2) {
///   [0]=>
///   string(2) "95"
///   [1]=>
///   string(4) "2c78"
/// }
/// array(1) {
///   [0]=>
///   string(8) "955c2c78"
/// }
/// ```
#[php_function]
fn fgetcsv(
    req: &mut Request,
    stream: Stream,
    #[php(default = null)] length: Option<i64>,
    #[php(default = ",")] separator: PhpString,
    #[php(default = "\"")] enclosure: PhpString,
    #[php(default = "\\")] escape: Passed<PhpString>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let len = length;
    let delimiter = one_byte(req, separator, 3)?;
    let enclosure = one_byte(req, enclosure, 4)?;
    let escape = crate::csv::escape_of(req, escape, 5)?;
    let len = match len {
        None | Some(0) => None,
        Some(n) if !(0..=i64::MAX - 1).contains(&n) => {
            return Err(arg_value_error(
                req,
                2,
                &format!("be between 0 and {}", i64::MAX - 1),
            ));
        }
        Some(n) => Some(usize::try_from(n).unwrap_or(usize::MAX).saturating_add(1)),
    };
    let Some(line) = stream.get_line(req, len)? else {
        return Ok(OrFalse::False);
    };
    let mut failed = None;
    let mut more = || match stream.get_line(req, None) {
        Ok(next) => next,
        Err(thrown) => {
            failed = Some(thrown);
            None
        }
    };
    let fields = crate::csv::fgetcsv(&line, delimiter, enclosure, escape, &mut more);
    if let Some(thrown) = failed {
        return Err(thrown);
    }
    let mut array = PhpArray::new(req)?;
    match fields {
        None => array.append(req, Owner::null())?,
        Some(fields) => {
            for field in fields {
                let value = PhpString::new(req, &field)?.into_value();
                array.append(req, value)?;
            }
        }
    }
    Ok(OrFalse::Value(array))
}

/// `realpath(string $path): string|false`, `PHP_FUNCTION(realpath)`
/// (`Z/ext/standard/file.c:2105`) over `tsrm_realpath`
/// (`Z/Zend/zend_virtual_cwd.c:1717`): the path against the working
/// directory with every link resolved, `false` where any part of it is
/// not there, and the working directory itself for the empty path.
///
/// ```php
/// $d = __DIR__ . '/realpath-d'; @mkdir($d); touch("$d/f"); @symlink("$d/f", "$d/l");
/// chdir($d);
/// var_dump(realpath('f') === realpath("$d/f"), realpath('l') === realpath('f'), realpath('./../realpath-d/f') === realpath('f'), realpath('') === getcwd(), realpath('.') === getcwd(), realpath('nope'), realpath('f/x'), basename(realpath('/')));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// string(0) ""
/// ```
#[php_function]
fn realpath(req: &mut Request, path: Path) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Path(path) = path;
    let cwd = req.environment().cwd().as_os_str().as_bytes().to_vec();
    let written = if path.as_bytes().is_empty() {
        cwd.clone()
    } else {
        path.as_bytes().to_vec()
    };
    let cwd: &[u8] = if written.first() == Some(&b'/') {
        b""
    } else {
        &cwd
    };
    match req
        .environment_mut()
        .virtual_file_ex(cwd, &written, Resolve::RealPath)
    {
        // The guard is asked of the resolved path, and warns.
        Some(resolved)
            if req
                .environment_mut()
                .check_open_basedir(&resolved, true)
                .is_ok() =>
        {
            Ok(OrFalse::Value(PhpString::new(req, &resolved)?))
        }
        Some(_) | None => Ok(OrFalse::False),
    }
}

/// `fnmatch(string $pattern, string $filename, int $flags = 0): bool`,
/// `PHP_FUNCTION(fnmatch)` (`Z/ext/standard/file.c:2241`): the host's
/// `fnmatch(3)` with the flags as given, after php's warnings for a
/// name or a pattern of `MAXPATHLEN` bytes or more.
///
/// ```php
/// var_dump(fnmatch('*.txt', 'a.txt'), fnmatch('*.txt', 'a.TXT'), fnmatch('*.txt', 'a.TXT', FNM_CASEFOLD), fnmatch('a/*', 'a/b/c'), fnmatch('a/*', 'a/b/c', FNM_PATHNAME), fnmatch('*', '.hidden', FNM_PERIOD), fnmatch('\*', '*'), fnmatch('\*', '\*', FNM_NOESCAPE), fnmatch('[a-c]?', 'bz'));
/// var_dump(fnmatch(str_repeat('x', 1024), 'x'), fnmatch('x', str_repeat('x', 1024)));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Warning: fnmatch(): Pattern exceeds the maximum allowed length of 1024 characters in %s on line 3
///
/// Warning: fnmatch(): Filename exceeds the maximum allowed length of 1024 characters in %s on line 3
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn fnmatch(
    req: &mut Request,
    pattern: Path,
    filename: Path,
    #[php(default = 0)] flags: i64,
) -> bool {
    let (Path(pattern), Path(filename)) = (pattern, filename);
    if filename.len() >= MAXPATHLEN {
        let text =
            format!("Filename exceeds the maximum allowed length of {MAXPATHLEN} characters");
        docref(req, Level::Warning, &text);
        return false;
    }
    if pattern.len() >= MAXPATHLEN {
        let text = format!("Pattern exceeds the maximum allowed length of {MAXPATHLEN} characters");
        docref(req, Level::Warning, &text);
        return false;
    }
    // `(int)flags`: the low 32 bits, as C's conversion keeps them.
    #[allow(clippy::cast_possible_truncation)]
    let flags = flags as i32;
    crate::ffi::fnmatch(pattern.as_bytes(), filename.as_bytes(), flags)
}

/// `sys_get_temp_dir(): string`, `PHP_FUNCTION(sys_get_temp_dir)`
/// (`Z/ext/standard/file.c:2269`) over `php_get_temporary_directory`
/// (`Z/main/php_open_temporary_file.c:236`), the one directory
/// [`Environment::temp_dir`] names.
///
/// ```php
/// $t = sys_get_temp_dir(); var_dump($t[0], is_dir($t));
/// ```
/// ```expect
/// string(1) "/"
/// bool(true)
/// ```
#[php_function]
fn sys_get_temp_dir(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let dir = req.environment().temp_dir();
    PhpString::new(req, dir.as_os_str().as_encoded_bytes())
}
