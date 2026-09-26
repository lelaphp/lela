//! Core's constants, in the order php registers them at startup:
//! `zend_register_standard_constants` (`Z/Zend/zend_constants.c:130`),
//! which is `zend_constants.stub.php`'s error levels and
//! `zend_builtin_functions.stub.php`'s two, then the three the engine
//! reports itself by and `TRUE`, `FALSE` and `NULL`; then
//! `php_module_startup`'s `register_main_symbols`
//! (`Z/main/main_arginfo.h`), the version, the build, the host and the
//! numeric limits; the constants of `php_output.h` (`Z/main/php_output.h:23-45`);
//! `php_rfc1867_register_constants`'s upload errors; and `PHP_BINARY`.
//! `PHP_SAPI`, which php registers between the last two, is the host's
//! and so the request's ([`crate::api::request::Request::constant_named`]).
//!
//! A value that is a fact of the build is the pinned oracle's, since
//! the engine answers as that build does: its version, its date and
//! provider, its install paths, and the `ZEND_VM_KIND` it was compiled
//! with. A value that is a fact of the host, the system's name and its
//! family, its path limit and `FD_SETSIZE`, is read off the target the
//! engine is compiled for, which is what php's `configure` reads it off.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_heap::{Intern, Strings, Zval};
use php_unit::decl::{Attribute, AttributeArgument, ConstDecl, ConstExpr, Name};

use crate::api::request::PHP_VERSION;

/// `PHP_INCLUDE_PATH`, the pinned build's `.:${PEAR_INSTALLDIR}`: what
/// `DEFAULT_INCLUDE_PATH` says and what `include_path` defaults to.
pub(crate) const PHP_INCLUDE_PATH: &str = ".:/opt/homebrew/Cellar/php/8.5.9/share/php/pear";

/// `PHP_EXTENSION_DIR`, the pinned build's: the constant, and the
/// default of `extension_dir`, which `dl()` names a library under.
pub(crate) const PHP_EXTENSION_DIR: &str = "/opt/homebrew/Cellar/php/8.5.9/lib/php/20250925";

/// The install paths the pinned build's `configure` wrote into
/// `main/build-defs.h`, in `register_main_symbols`' order from
/// `PEAR_INSTALL_DIR` to `PHP_CONFIG_FILE_SCAN_DIR`.
const BUILD_PATHS: [(&str, &str); 14] = [
    (
        "PEAR_INSTALL_DIR",
        "/opt/homebrew/Cellar/php/8.5.9/share/php/pear",
    ),
    (
        "PEAR_EXTENSION_DIR",
        "/opt/homebrew/Cellar/php/8.5.9/lib/php/20250925",
    ),
    ("PHP_EXTENSION_DIR", PHP_EXTENSION_DIR),
    ("PHP_PREFIX", "/opt/homebrew/Cellar/php/8.5.9"),
    ("PHP_BINDIR", "/opt/homebrew/Cellar/php/8.5.9/bin"),
    ("PHP_SBINDIR", "/opt/homebrew/Cellar/php/8.5.9/sbin"),
    ("PHP_MANDIR", "/opt/homebrew/Cellar/php/8.5.9/share/man"),
    ("PHP_LIBDIR", "/opt/homebrew/Cellar/php/8.5.9/lib/php"),
    ("PHP_DATADIR", "/opt/homebrew/Cellar/php/8.5.9/share/php"),
    ("PHP_SYSCONFDIR", "/opt/homebrew/etc/php/8.5"),
    ("PHP_LOCALSTATEDIR", "/opt/homebrew/var"),
    ("PHP_CONFIG_FILE_PATH", "/opt/homebrew/etc/php/8.5"),
    (
        "PHP_CONFIG_FILE_SCAN_DIR",
        "/opt/homebrew/etc/php/8.5/conf.d",
    ),
    ("PHP_SHLIB_SUFFIX", SHLIB_SUFFIX),
];

/// `PHP_SHLIB_SUFFIX`: `dll` on Windows and `so` everywhere else, macOS
/// included, as php's `configure` sets it.
const SHLIB_SUFFIX: &str = if cfg!(windows) { "dll" } else { "so" };

/// `PHP_OS_STR`, the system `configure` ran on as `uname -s` names it,
/// and `WINNT` on Windows.
const OS: &str = if cfg!(target_os = "macos") {
    "Darwin"
} else if cfg!(target_os = "linux") {
    "Linux"
} else if cfg!(target_os = "freebsd") {
    "FreeBSD"
} else if cfg!(target_os = "openbsd") {
    "OpenBSD"
} else if cfg!(target_os = "netbsd") {
    "NetBSD"
} else if cfg!(windows) {
    "WINNT"
} else {
    "Unknown"
};

/// `PHP_OS_FAMILY` (`Z/main/php.h:30-48`): one of php's seven words for
/// the system.
const OS_FAMILY: &str = if cfg!(target_os = "macos") {
    "Darwin"
} else if cfg!(target_os = "linux") {
    "Linux"
} else if cfg!(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)) {
    "BSD"
} else if cfg!(windows) {
    "Windows"
} else if cfg!(target_os = "solaris") {
    "Solaris"
} else {
    "Unknown"
};

/// `MAXPATHLEN`: `PATH_MAX` of the host's headers, 1024 on macOS and
/// the BSDs, 4096 on Linux, `_MAX_PATH` on Windows.
const MAXPATHLEN: i64 = if cfg!(target_os = "linux") {
    4096
} else if cfg!(windows) {
    260
} else {
    1024
};

/// `sapi_module.name` of php's hosts, which `PHP_SAPI` says and the
/// engine interns at build: the CLI, `cgi-fcgi`, the development
/// server, FPM, phpdbg and the embed host.
pub(crate) const HOSTS: [&str; 6] = [
    "cli",
    "cgi-fcgi",
    "cli-server",
    "fpm-fcgi",
    "phpdbg",
    "embed",
];

/// [`HOSTS`] interned into the engine's table, each beside its name,
/// what `Shared::sapi_constant` reads `PHP_SAPI` from.
pub(crate) fn host_names(strings: &mut Strings) -> Vec<(Box<[u8]>, Zval)> {
    HOSTS
        .iter()
        .filter_map(|name| {
            let interned = strings.intern(name.as_bytes()).ok()?;
            Some((Box::from(name.as_bytes()), Zval::string(interned)))
        })
        .collect()
}

/// A string constant's row.
fn string(name: &'static str, value: &str) -> (&'static str, ConstExpr) {
    (name, ConstExpr::Str(value.as_bytes().into()))
}

/// The error levels `register_zend_constants_symbols` registers
/// (`Z/Zend/zend_constants_arginfo.h:6-20`), in the order php
/// registers them, with `E_STRICT` among them carrying the one
/// attribute `zend_add_global_constant_attribute` hangs on a Zend
/// constant at the tag: `#[\Deprecated(since: '8.4', message: 'the
/// error level was removed')]`, which is the whole of its
/// `CONST_DEPRECATED` (`Z/Zend/zend_constants_arginfo.h:32-37`), so a
/// read of it is the deprecation and a fold of it is refused. `E_ALL`
/// after them is their union without `E_STRICT`, the level 8.4 dropped
/// from the mask (`Z/Zend/zend_errors.h:43`).
///
/// ```php
/// var_dump(E_ERROR, E_WARNING, E_USER_DEPRECATED, E_ALL);
/// var_dump(E_ALL, E_STRICT & E_ALL, E_ALL & ~E_WARNING);
/// var_dump(error_reporting(E_ALL & ~E_DEPRECATED), error_reporting());
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(16384)
/// int(30719)
///
/// Deprecated: Constant E_STRICT is deprecated since 8.4, the error level was removed in %s on line 3
/// int(30719)
/// int(0)
/// int(30717)
/// int(32767)
/// int(22527)
/// ```
fn error_levels() -> Vec<ConstDecl> {
    [
        ("E_ERROR", 1 << 0),
        ("E_WARNING", 1 << 1),
        ("E_PARSE", 1 << 2),
        ("E_NOTICE", 1 << 3),
        ("E_CORE_ERROR", 1 << 4),
        ("E_CORE_WARNING", 1 << 5),
        ("E_COMPILE_ERROR", 1 << 6),
        ("E_COMPILE_WARNING", 1 << 7),
        ("E_USER_ERROR", 1 << 8),
        ("E_USER_WARNING", 1 << 9),
        ("E_USER_NOTICE", 1 << 10),
        ("E_STRICT", 1 << 11),
        ("E_RECOVERABLE_ERROR", 1 << 12),
        ("E_DEPRECATED", 1 << 13),
        ("E_USER_DEPRECATED", 1 << 14),
        ("E_ALL", 30719),
    ]
    .into_iter()
    .map(|(name, value)| ConstDecl {
        name: Name::from(name),
        value: ConstExpr::long(value),
        attributes: if name == "E_STRICT" {
            vec![Attribute {
                name: Name::from("Deprecated"),
                arguments: vec![
                    AttributeArgument {
                        name: Some(Name::from("since")),
                        value: ConstExpr::Str(b"8.4".as_slice().into()),
                    },
                    AttributeArgument {
                        name: Some(Name::from("message")),
                        value: ConstExpr::Str(b"the error level was removed".as_slice().into()),
                    },
                ],
            }]
        } else {
            Vec::new()
        },
    })
    .collect()
}

/// Core's constants in php's order, each a `ConstDecl` the compiler
/// folds where it stands (`zend_try_ct_eval_const`, design §6.1) and
/// the request's table reads while a program executes, from one row.
///
/// ```php
/// echo PHP_VERSION, PHP_EOL;
/// var_dump(PHP_INT_SIZE, E_ALL, PHP_INT_MAX, PHP_INT_MIN);
/// var_dump(PHP_OUTPUT_HANDLER_STDFLAGS, \PHP_EOL === "\n");
/// ```
/// ```expect
/// 8.5.9
/// int(8)
/// int(30719)
/// int(9223372036854775807)
/// int(-9223372036854775808)
/// int(112)
/// bool(true)
/// ```
///
/// The version in its parts, the build and the host:
///
/// ```php
/// var_dump(PHP_MAJOR_VERSION, PHP_MINOR_VERSION, PHP_RELEASE_VERSION, PHP_EXTRA_VERSION, PHP_VERSION_ID, PHP_ZTS, PHP_DEBUG, ZEND_THREAD_SAFE, ZEND_DEBUG_BUILD, ZEND_VM_KIND);
/// var_dump(PHP_OS, PHP_OS_FAMILY, PHP_MAXPATHLEN, PHP_FD_SETSIZE, PHP_SHLIB_SUFFIX, DEFAULT_INCLUDE_PATH === ini_get('include_path'), PHP_BUILD_PROVIDER, PHP_BUILD_DATE);
/// var_dump(TRUE, FALSE, NULL, defined('TRUE'), constant('NULL'));
/// ```
/// ```expect
/// int(8)
/// int(5)
/// int(9)
/// string(0) ""
/// int(80509)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// string(21) "ZEND_VM_KIND_TAILCALL"
/// string(6) "Darwin"
/// string(6) "Darwin"
/// int(1024)
/// int(1024)
/// string(2) "so"
/// bool(true)
/// string(8) "Homebrew"
/// string(20) "Jul 28 2026 13:06:52"
/// bool(true)
/// bool(false)
/// NULL
/// bool(true)
/// NULL
/// ```
///
/// The limits of a double are `float.h`'s, and the upload errors
/// `rfc1867.h`'s:
///
/// ```php
/// var_dump(PHP_FLOAT_DIG, PHP_FLOAT_EPSILON, PHP_FLOAT_MAX, PHP_FLOAT_MIN, 1.0 + PHP_FLOAT_EPSILON > 1.0);
/// var_dump(UPLOAD_ERR_OK, UPLOAD_ERR_NO_FILE, UPLOAD_ERR_NO_TMP_DIR, UPLOAD_ERR_EXTENSION, PHP_SAPI, PHP_BINARY !== '');
/// ```
/// ```expect
/// int(15)
/// float(2.220446049250313E-16)
/// float(1.7976931348623157E+308)
/// float(2.2250738585072014E-308)
/// bool(true)
/// int(0)
/// int(4)
/// int(6)
/// int(8)
/// string(3) "cli"
/// bool(true)
/// ```
pub(crate) fn core_constants() -> Vec<ConstDecl> {
    let mut out = error_levels();
    let mut rows: Vec<(&'static str, ConstExpr)> = vec![
        ("DEBUG_BACKTRACE_PROVIDE_OBJECT", ConstExpr::long(1)),
        ("DEBUG_BACKTRACE_IGNORE_ARGS", ConstExpr::long(2)),
        ("ZEND_THREAD_SAFE", ConstExpr::Bool(false)),
        ("ZEND_DEBUG_BUILD", ConstExpr::Bool(false)),
        (
            "ZEND_VM_KIND",
            ConstExpr::Str(b"ZEND_VM_KIND_TAILCALL".as_slice().into()),
        ),
        ("TRUE", ConstExpr::Bool(true)),
        ("FALSE", ConstExpr::Bool(false)),
        ("NULL", ConstExpr::Null),
        ("PHP_VERSION", ConstExpr::Str(PHP_VERSION.as_bytes().into())),
        ("PHP_MAJOR_VERSION", ConstExpr::long(8)),
        ("PHP_MINOR_VERSION", ConstExpr::long(5)),
        ("PHP_RELEASE_VERSION", ConstExpr::long(9)),
        ("PHP_EXTRA_VERSION", ConstExpr::Str(Box::new([]))),
        ("PHP_VERSION_ID", ConstExpr::long(80509)),
        (
            "PHP_BUILD_DATE",
            ConstExpr::Str(b"Jul 28 2026 13:06:52".as_slice().into()),
        ),
        (
            "PHP_BUILD_PROVIDER",
            ConstExpr::Str(b"Homebrew".as_slice().into()),
        ),
        ("PHP_ZTS", ConstExpr::Bool(false)),
        ("PHP_DEBUG", ConstExpr::Bool(false)),
        ("PHP_OS", ConstExpr::Str(OS.as_bytes().into())),
        ("PHP_OS_FAMILY", ConstExpr::Str(OS_FAMILY.as_bytes().into())),
        (
            "DEFAULT_INCLUDE_PATH",
            ConstExpr::Str(PHP_INCLUDE_PATH.as_bytes().into()),
        ),
    ];
    rows.extend(BUILD_PATHS.iter().map(|(name, value)| string(name, value)));
    rows.extend([
        ("PHP_EOL", ConstExpr::Str(b"\n".as_slice().into())),
        ("PHP_MAXPATHLEN", ConstExpr::long(MAXPATHLEN)),
        ("PHP_INT_MAX", ConstExpr::long(i64::MAX)),
        ("PHP_INT_MIN", ConstExpr::long(i64::MIN)),
        ("PHP_INT_SIZE", ConstExpr::long(8)),
        ("PHP_FD_SETSIZE", ConstExpr::long(1024)),
        ("PHP_FLOAT_DIG", ConstExpr::long(15)),
        ("PHP_FLOAT_EPSILON", ConstExpr::Double(f64::EPSILON)),
        ("PHP_FLOAT_MAX", ConstExpr::Double(f64::MAX)),
        ("PHP_FLOAT_MIN", ConstExpr::Double(f64::MIN_POSITIVE)),
    ]);
    // `main/main.stub.php`: the constants of `php_output.h`, the
    // operation `ob_start`'s callback is handed and the flags it is
    // started with.
    rows.extend(
        [
            ("PHP_OUTPUT_HANDLER_START", 0x01),
            ("PHP_OUTPUT_HANDLER_WRITE", 0x00),
            ("PHP_OUTPUT_HANDLER_FLUSH", 0x04),
            ("PHP_OUTPUT_HANDLER_CLEAN", 0x02),
            ("PHP_OUTPUT_HANDLER_FINAL", 0x08),
            ("PHP_OUTPUT_HANDLER_CONT", 0x00),
            ("PHP_OUTPUT_HANDLER_END", 0x08),
            ("PHP_OUTPUT_HANDLER_CLEANABLE", 0x10),
            ("PHP_OUTPUT_HANDLER_FLUSHABLE", 0x20),
            ("PHP_OUTPUT_HANDLER_REMOVABLE", 0x40),
            ("PHP_OUTPUT_HANDLER_STDFLAGS", 0x70),
            ("PHP_OUTPUT_HANDLER_STARTED", 0x1000),
            ("PHP_OUTPUT_HANDLER_DISABLED", 0x2000),
            ("PHP_OUTPUT_HANDLER_PROCESSED", 0x4000),
            ("UPLOAD_ERR_OK", 0),
            ("UPLOAD_ERR_INI_SIZE", 1),
            ("UPLOAD_ERR_FORM_SIZE", 2),
            ("UPLOAD_ERR_PARTIAL", 3),
            ("UPLOAD_ERR_NO_FILE", 4),
            ("UPLOAD_ERR_NO_TMP_DIR", 6),
            ("UPLOAD_ERR_CANT_WRITE", 7),
            ("UPLOAD_ERR_EXTENSION", 8),
        ]
        .map(|(name, value)| (name, ConstExpr::long(value))),
    );
    // `php_binary_init` (`Z/main/main.c:418`): the realpath of the
    // executable the process runs, and the empty string where there is
    // none to name.
    let binary = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .map(|path| path.as_os_str().as_encoded_bytes().to_vec())
        .unwrap_or_default();
    rows.push(("PHP_BINARY", ConstExpr::Str(binary.into())));
    out.extend(rows.into_iter().map(|(name, value)| ConstDecl {
        name: Name::from(name),
        value,
        attributes: Vec::new(),
    }));
    out
}
