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
//! A value that is a fact of the build is the pinned oracle's where the
//! engine answers as that build does: its version and the `ZEND_VM_KIND`
//! it was compiled with. Its date, provider and install layout are this
//! build's own, from the one stamp every host reads them from
//! ([`crate::api::request::BuildDefs`]). A value that is a fact of the host, the system's name and its
//! family, its path limit and `FD_SETSIZE`, is read off the target the
//! engine is compiled for, which is what php's `configure` reads it off.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_heap::{Intern, Strings, Zval};
use php_macros::php_const;

/// `PHP_INCLUDE_PATH`, `.:${PEAR_INSTALLDIR}` of this build's layout
/// (`build.rs`): what `DEFAULT_INCLUDE_PATH` says and what
/// `include_path` defaults to.
pub(crate) const PHP_INCLUDE_PATH: &str = env!("PHP_INCLUDE_PATH");

/// `PHP_EXTENSION_DIR`, this build's: the constant, and the default of
/// `extension_dir`, which `dl()` names a library under.
pub(crate) const PHP_EXTENSION_DIR: &str = env!("PHP_EXTENSION_DIR");

/// `PHP_CONFIG_FILE_PATH` and `PHP_CONFIG_FILE_SCAN_DIR`, this build's:
/// where a host's php.ini search ends and what it scans where the
/// environment names no scan directory (`Z/main/php_ini.c:534`, `:626`).
pub const PHP_CONFIG_FILE_PATH: &str = env!("PHP_CONFIG_FILE_PATH");
pub const PHP_CONFIG_FILE_SCAN_DIR: &str = env!("PHP_CONFIG_FILE_SCAN_DIR");

/// `PHP_PREFIX`, `PHP_SYSCONFDIR` and `PHP_LOCALSTATEDIR`, this build's,
/// which php-fpm puts its configuration, its pid file and its log under.
pub const PHP_PREFIX: &str = env!("PHP_PREFIX");
pub const PHP_SYSCONFDIR: &str = env!("PHP_SYSCONFDIR");
pub const PHP_LOCALSTATEDIR: &str = env!("PHP_LOCALSTATEDIR");

/// `PHP_SHLIB_SUFFIX`: `dll` on Windows and `so` everywhere else, macOS
/// included, as php's `configure` sets it.
pub(crate) const SHLIB_SUFFIX: &str = if cfg!(windows) { "dll" } else { "so" };

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

/// `PHP_MAXPATHLEN`: [`crate::virtual_cwd::MAXPATHLEN`] as a long, the
/// one value the path checks read too.
#[expect(
    clippy::cast_possible_wrap,
    reason = "MAXPATHLEN is at most 4096, far inside a long"
)]
const MAXPATHLEN: i64 = crate::virtual_cwd::MAXPATHLEN as i64;

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
/// and `host`, the name the engine is built for, where it is none of
/// them and not empty, the name of no host: what `Shared::sapi_constant`
/// reads `PHP_SAPI` from, so the engine's own host is found without
/// waiting on the table, which a compile holds while it asks
/// ([`php_compile::Persistent::host`]).
pub(crate) fn host_names(strings: &mut Strings, host: &str) -> Vec<(Box<[u8]>, Zval)> {
    let own = (!host.is_empty() && !HOSTS.contains(&host)).then_some(host);
    HOSTS
        .iter()
        .copied()
        .chain(own)
        .filter_map(|name| {
            let interned = strings.intern(name.as_bytes()).ok()?;
            Some((Box::from(name.as_bytes()), Zval::string(interned)))
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
/// The version in its parts, the build and the host; the build's date is
/// this build's, so its shape is what is held, and its provider stands
/// only where the build named one:
///
/// ```php
/// var_dump(PHP_MAJOR_VERSION, PHP_MINOR_VERSION, PHP_RELEASE_VERSION, PHP_EXTRA_VERSION, PHP_VERSION_ID, PHP_ZTS, PHP_DEBUG, ZEND_THREAD_SAFE, ZEND_DEBUG_BUILD, ZEND_VM_KIND);
/// var_dump(PHP_OS, PHP_OS_FAMILY, PHP_MAXPATHLEN, PHP_FD_SETSIZE, PHP_SHLIB_SUFFIX, DEFAULT_INCLUDE_PATH === ini_get('include_path'), strlen(PHP_BUILD_DATE));
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
/// int(20)
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
///
/// The error levels are `register_zend_constants_symbols`'
/// (`Z/Zend/zend_constants_arginfo.h:6-20`), in the order php registers
/// them, with `E_STRICT` among them carrying the one attribute
/// `zend_add_global_constant_attribute` hangs on a Zend constant at the
/// tag, `#[\Deprecated(since: '8.4', message: 'the error level was
/// removed')]`, which is the whole of its `CONST_DEPRECATED`
/// (`Z/Zend/zend_constants_arginfo.h:32-37`), so a read of it is the
/// deprecation and a fold of it is refused. `E_ALL` after them is their
/// union without `E_STRICT`, the level 8.4 dropped from the mask
/// (`Z/Zend/zend_errors.h:43`).
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
/// int(30719)
/// int(22527)
/// ```
#[php_const]
pub(crate) mod startup {
    use super::{MAXPATHLEN, OS, OS_FAMILY, PHP_INCLUDE_PATH, SHLIB_SUFFIX};
    use crate::api::abi::ConstValue;

    const E_ERROR: i64 = 1 << 0;
    const E_WARNING: i64 = 1 << 1;
    const E_PARSE: i64 = 1 << 2;
    const E_NOTICE: i64 = 1 << 3;
    const E_CORE_ERROR: i64 = 1 << 4;
    const E_CORE_WARNING: i64 = 1 << 5;
    const E_COMPILE_ERROR: i64 = 1 << 6;
    const E_COMPILE_WARNING: i64 = 1 << 7;
    const E_USER_ERROR: i64 = 1 << 8;
    const E_USER_WARNING: i64 = 1 << 9;
    const E_USER_NOTICE: i64 = 1 << 10;
    #[php(deprecated(since = "8.4", message = "the error level was removed"))]
    const E_STRICT: i64 = 1 << 11;
    const E_RECOVERABLE_ERROR: i64 = 1 << 12;
    const E_DEPRECATED: i64 = 1 << 13;
    const E_USER_DEPRECATED: i64 = 1 << 14;
    const E_ALL: i64 = 30719;
    const DEBUG_BACKTRACE_PROVIDE_OBJECT: i64 = 1;
    const DEBUG_BACKTRACE_IGNORE_ARGS: i64 = 2;
    const ZEND_THREAD_SAFE: bool = false;
    const ZEND_DEBUG_BUILD: bool = false;
    const ZEND_VM_KIND: &str = "ZEND_VM_KIND_TAILCALL";
    const TRUE: bool = true;
    const FALSE: bool = false;
    const NULL: ConstValue = ConstValue::Null;
    // `register_main_symbols` (`Z/main/main_arginfo.h`): the version,
    // the build and the host.
    const PHP_VERSION: &str = crate::api::request::PHP_VERSION;
    const PHP_MAJOR_VERSION: i64 = 8;
    const PHP_MINOR_VERSION: i64 = 5;
    const PHP_RELEASE_VERSION: i64 = 9;
    const PHP_EXTRA_VERSION: &str = "";
    const PHP_VERSION_ID: i64 = 80509;

    /// `php_build_date`, this build's (`BuildDefs::date`).
    fn PHP_BUILD_DATE() -> String {
        crate::api::request::build_date()
    }

    /// Declared only where the build named a provider, as
    /// `#if defined(PHP_BUILD_PROVIDER)` declares it.
    #[cfg(php_build_provider)]
    const PHP_BUILD_PROVIDER: &str = match crate::api::request::BUILD_PROVIDER {
        Some(provider) => provider,
        None => "",
    };
    const PHP_ZTS: bool = false;
    const PHP_DEBUG: bool = false;
    const PHP_OS: &str = OS;
    const PHP_OS_FAMILY: &str = OS_FAMILY;
    const DEFAULT_INCLUDE_PATH: &str = PHP_INCLUDE_PATH;
    // The install layout this build's `build.rs` stamped, as `configure`
    // writes it into `main/build-defs.h`, and the shared library suffix.
    const PEAR_INSTALL_DIR: &str = env!("PEAR_INSTALL_DIR");
    const PEAR_EXTENSION_DIR: &str = super::PHP_EXTENSION_DIR;
    const PHP_EXTENSION_DIR: &str = super::PHP_EXTENSION_DIR;
    const PHP_PREFIX: &str = super::PHP_PREFIX;
    const PHP_BINDIR: &str = env!("PHP_BINDIR");
    const PHP_SBINDIR: &str = env!("PHP_SBINDIR");
    const PHP_MANDIR: &str = env!("PHP_MANDIR");
    const PHP_LIBDIR: &str = env!("PHP_LIBDIR");
    const PHP_DATADIR: &str = env!("PHP_DATADIR");
    const PHP_SYSCONFDIR: &str = super::PHP_SYSCONFDIR;
    const PHP_LOCALSTATEDIR: &str = super::PHP_LOCALSTATEDIR;
    const PHP_CONFIG_FILE_PATH: &str = super::PHP_CONFIG_FILE_PATH;
    const PHP_CONFIG_FILE_SCAN_DIR: &str = super::PHP_CONFIG_FILE_SCAN_DIR;
    const PHP_SHLIB_SUFFIX: &str = SHLIB_SUFFIX;
    const PHP_EOL: &str = "\n";
    const PHP_MAXPATHLEN: i64 = MAXPATHLEN;
    const PHP_INT_MAX: i64 = i64::MAX;
    const PHP_INT_MIN: i64 = i64::MIN;
    const PHP_INT_SIZE: i64 = 8;
    const PHP_FD_SETSIZE: i64 = 1024;
    const PHP_FLOAT_DIG: i64 = 15;
    const PHP_FLOAT_EPSILON: f64 = f64::EPSILON;
    const PHP_FLOAT_MAX: f64 = f64::MAX;
    const PHP_FLOAT_MIN: f64 = f64::MIN_POSITIVE;
    // `main/main.stub.php`: the constants of `php_output.h`, the
    // operation `ob_start`'s callback is handed and the flags it is
    // started with.
    const PHP_OUTPUT_HANDLER_START: i64 = 0x01;
    const PHP_OUTPUT_HANDLER_WRITE: i64 = 0x00;
    const PHP_OUTPUT_HANDLER_FLUSH: i64 = 0x04;
    const PHP_OUTPUT_HANDLER_CLEAN: i64 = 0x02;
    const PHP_OUTPUT_HANDLER_FINAL: i64 = 0x08;
    const PHP_OUTPUT_HANDLER_CONT: i64 = 0x00;
    const PHP_OUTPUT_HANDLER_END: i64 = 0x08;
    const PHP_OUTPUT_HANDLER_CLEANABLE: i64 = 0x10;
    const PHP_OUTPUT_HANDLER_FLUSHABLE: i64 = 0x20;
    const PHP_OUTPUT_HANDLER_REMOVABLE: i64 = 0x40;
    const PHP_OUTPUT_HANDLER_STDFLAGS: i64 = 0x70;
    const PHP_OUTPUT_HANDLER_STARTED: i64 = 0x1000;
    const PHP_OUTPUT_HANDLER_DISABLED: i64 = 0x2000;
    const PHP_OUTPUT_HANDLER_PROCESSED: i64 = 0x4000;
    // `php_rfc1867_register_constants`' upload errors.
    const UPLOAD_ERR_OK: i64 = 0;
    const UPLOAD_ERR_INI_SIZE: i64 = 1;
    const UPLOAD_ERR_FORM_SIZE: i64 = 2;
    const UPLOAD_ERR_PARTIAL: i64 = 3;
    const UPLOAD_ERR_NO_FILE: i64 = 4;
    const UPLOAD_ERR_NO_TMP_DIR: i64 = 6;
    const UPLOAD_ERR_CANT_WRITE: i64 = 7;
    const UPLOAD_ERR_EXTENSION: i64 = 8;

    /// `php_binary_init` (`Z/main/main.c:418`): the realpath of the
    /// executable the process runs, and the empty string where there is
    /// none to name.
    fn PHP_BINARY() -> Vec<u8> {
        std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .map(|path| path.as_os_str().as_encoded_bytes().to_vec())
            .unwrap_or_default()
    }
}
