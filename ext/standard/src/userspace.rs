//! The natives of `Z/main/streams/userspace.c`, which php's
//! `basic_functions` declares: `stream_wrapper_register` and its alias
//! `stream_register_wrapper`, `stream_wrapper_unregister` and
//! `stream_wrapper_restore`, over the engine's user-space wrapper, and the
//! constants `userspace.stub.php` declares for a wrapper's methods to
//! read.
//!
//! The four natives, as `basic_functions.stub.php` declares them.
//!
//! ```php
//! foreach (['stream_wrapper_register', 'stream_register_wrapper', 'stream_wrapper_unregister', 'stream_wrapper_restore'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function stream_wrapper_register ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> string $protocol ]
//!     Parameter #1 [ <required> string $class ]
//!     Parameter #2 [ <optional> int $flags = 0 ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function stream_register_wrapper ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> string $protocol ]
//!     Parameter #1 [ <required> string $class ]
//!     Parameter #2 [ <optional> int $flags = 0 ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function stream_wrapper_unregister ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $protocol ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function stream_wrapper_restore ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $protocol ]
//!   }
//!   - Return [ bool ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::Raw;
use php_ext::attr::{php_const, php_function};
use php_ext::call::autoload;
use php_ext::raising::{Thrown, arg_type_error};
use php_ext::request::{Request, Streams as _};
use php_ext::values::PhpString;

/// The constants of `userspace.stub.php` and `file.stub.php` a wrapper's
/// methods are handed or answer with.
///
/// ```php
/// var_dump(STREAM_USE_PATH, STREAM_IGNORE_URL, STREAM_REPORT_ERRORS, STREAM_MUST_SEEK, STREAM_URL_STAT_LINK, STREAM_URL_STAT_QUIET, STREAM_MKDIR_RECURSIVE, STREAM_IS_URL, STREAM_OPTION_BLOCKING, STREAM_OPTION_READ_TIMEOUT, STREAM_OPTION_READ_BUFFER, STREAM_OPTION_WRITE_BUFFER, STREAM_BUFFER_NONE, STREAM_BUFFER_LINE, STREAM_BUFFER_FULL, STREAM_CAST_AS_STREAM, STREAM_CAST_FOR_SELECT, STREAM_META_TOUCH, STREAM_META_OWNER, STREAM_META_OWNER_NAME, STREAM_META_GROUP, STREAM_META_GROUP_NAME, STREAM_META_ACCESS);
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(8)
/// int(16)
/// int(1)
/// int(2)
/// int(1)
/// int(1)
/// int(1)
/// int(4)
/// int(2)
/// int(3)
/// int(0)
/// int(1)
/// int(2)
/// int(0)
/// int(3)
/// int(1)
/// int(3)
/// int(2)
/// int(5)
/// int(4)
/// int(6)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const STREAM_USE_PATH: i64 = 1;
    pub(crate) const STREAM_IGNORE_URL: i64 = 2;
    pub(crate) const STREAM_REPORT_ERRORS: i64 = 8;
    pub(crate) const STREAM_MUST_SEEK: i64 = 16;
    pub(crate) const STREAM_URL_STAT_LINK: i64 = 1;
    pub(crate) const STREAM_URL_STAT_QUIET: i64 = 2;
    pub(crate) const STREAM_MKDIR_RECURSIVE: i64 = 1;
    pub(crate) const STREAM_IS_URL: i64 = 1;
    pub(crate) const STREAM_OPTION_BLOCKING: i64 = 1;
    pub(crate) const STREAM_OPTION_READ_TIMEOUT: i64 = 4;
    pub(crate) const STREAM_OPTION_READ_BUFFER: i64 = 2;
    pub(crate) const STREAM_OPTION_WRITE_BUFFER: i64 = 3;
    pub(crate) const STREAM_BUFFER_NONE: i64 = 0;
    pub(crate) const STREAM_BUFFER_LINE: i64 = 1;
    pub(crate) const STREAM_BUFFER_FULL: i64 = 2;
    pub(crate) const STREAM_CAST_AS_STREAM: i64 = 0;
    pub(crate) const STREAM_CAST_FOR_SELECT: i64 = 3;
    pub(crate) const STREAM_META_TOUCH: i64 = 1;
    pub(crate) const STREAM_META_OWNER: i64 = 3;
    pub(crate) const STREAM_META_OWNER_NAME: i64 = 2;
    pub(crate) const STREAM_META_GROUP: i64 = 5;
    pub(crate) const STREAM_META_GROUP_NAME: i64 = 4;
    pub(crate) const STREAM_META_ACCESS: i64 = 6;
}

/// `stream_wrapper_register(string $protocol, string $class, int $flags
/// = 0): bool` and its alias `stream_register_wrapper`,
/// `PHP_FUNCTION(stream_wrapper_register)`
/// (`Z/main/streams/userspace.c:437`): `"SC|l"`, a class that is not
/// defined php's `TypeError`; the class registered under the scheme for
/// the rest of the request, a URL wrapper under `STREAM_IS_URL`, and
/// `false` with php's warning where the scheme is taken or is not one.
/// Every stream, directory and path operation under the scheme is then a
/// method of an instance of the class.
///
/// ```php
/// class Mem {
///     public $context;
///     private $data = "hello\nworld\n"; private $pos = 0;
///     function stream_open($path, $mode, $options, &$opened) { echo "open $path $mode\n"; return true; }
///     function stream_read($n) { $r = substr($this->data, $this->pos, $n); $this->pos += strlen($r); return $r; }
///     function stream_eof() { return $this->pos >= strlen($this->data); }
///     function stream_write($d) { $this->data .= $d; return strlen($d); }
///     function stream_stat() { return ['size' => strlen($this->data)]; }
///     function url_stat($p, $f) { echo "url_stat $p\n"; return ['mode' => 0100644, 'size' => 3]; }
///     function unlink($p) { echo "unlink $p\n"; return true; }
///     function stream_close() { echo "close\n"; }
/// }
/// var_dump(stream_wrapper_register('mem', 'Mem'), stream_wrapper_register('mem', 'Mem'), stream_register_wrapper('a b', 'Mem'));
/// try { stream_wrapper_register('x', 'Nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $f = fopen('mem://one', 'r+');
/// var_dump(stream_get_meta_data($f)['wrapper_type'], fgets($f), fread($f, 100), feof($f), fwrite($f, "x"), fstat($f)['size']);
/// fclose($f);
/// var_dump(filesize('mem://two'), unlink('mem://three'));
/// class Bare {}
/// stream_wrapper_register('bare', 'Bare'); var_dump(@fopen('bare://x', 'r'), error_get_last()['message']);
/// ```
/// ```expect
/// Warning: stream_wrapper_register(): Protocol mem:// is already defined. in %s on line 14
///
/// Warning: stream_register_wrapper(): Invalid protocol scheme specified. Unable to register wrapper class Mem to a b:// in %s on line 14
/// bool(true)
/// bool(false)
/// bool(false)
/// stream_wrapper_register(): Argument #2 ($class) must be a valid class name, Nope given
/// open mem://one r+
/// string(10) "user-space"
/// string(6) "hello
/// "
/// string(6) "world
/// "
/// bool(true)
/// int(1)
/// int(13)
/// close
/// url_stat mem://two
/// unlink mem://three
/// int(3)
/// bool(true)
/// bool(false)
/// string(78) "fopen(bare://x): Failed to open stream: "Bare::stream_open" is not implemented"
/// ```
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy { public $context; } } });
/// var_dump(stream_wrapper_register('lazy', 'Lazy'));
/// ```
/// ```expect
/// load Lazy
/// bool(true)
/// ```
#[php_function(aliases(stream_register_wrapper))]
fn stream_wrapper_register(
    req: &mut Request,
    protocol: PhpString,
    class: PhpString,
    #[php(default = 0)] flags: Raw<i64>,
) -> Result<bool, Thrown> {
    let protocol = protocol.as_bytes().to_vec();
    let name = class.as_bytes().to_vec();
    // `Z_PARAM_CLASS` looks the class up before `$flags` is held.
    let Some(class) = autoload(req, &name)? else {
        let text = format!(
            "be a valid class name, {} given",
            String::from_utf8_lossy(&name)
        );
        return Err(arg_type_error(req, 2, &text));
    };
    let flags = flags.hold(req)?;
    req.wrapper_register(&protocol, class, flags)
}

/// `stream_wrapper_unregister(string $protocol): bool`,
/// `PHP_FUNCTION(stream_wrapper_unregister)`
/// (`Z/main/streams/userspace.c:473`): the scheme's wrapper taken out for
/// the rest of the request, a built-in one among them, so a plain path is
/// then php's `file:// wrapper is disabled`; php's warning for a scheme
/// no wrapper has.
///
/// ```php
/// var_dump(stream_wrapper_unregister('file'), file_get_contents(__FILE__), stream_wrapper_unregister('file'));
/// ```
/// ```expect
/// Warning: file_get_contents(): file:// wrapper is disabled in the server configuration in %s on line 2
///
/// Warning: file_get_contents(%s): Failed to open stream: no suitable wrapper could be found in %s on line 2
///
/// Warning: stream_wrapper_unregister(): Unable to unregister protocol file:// in %s on line 2
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn stream_wrapper_unregister(req: &mut Request, protocol: PhpString) -> bool {
    req.wrapper_unregister(protocol.as_bytes())
}

/// `stream_wrapper_restore(string $protocol): bool`,
/// `PHP_FUNCTION(stream_wrapper_restore)`
/// (`Z/main/streams/userspace.c:499`): the built-in wrapper put back under
/// its scheme; php's warning for a scheme no built-in has, and its notice
/// and `true` for one the table has as it was.
///
/// ```php
/// var_dump(stream_wrapper_restore('file'));
/// stream_wrapper_unregister('file');
/// var_dump(stream_wrapper_restore('file'), strlen(file_get_contents(__FILE__)) > 0, stream_wrapper_restore('nope'));
/// ```
/// ```expect
/// Notice: stream_wrapper_restore(): file:// was never changed, nothing to restore in %s on line 2
/// bool(true)
///
/// Warning: stream_wrapper_restore(): nope:// never existed, nothing to restore in %s on line 4
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn stream_wrapper_restore(req: &mut Request, protocol: PhpString) -> bool {
    req.wrapper_restore(protocol.as_bytes())
}
