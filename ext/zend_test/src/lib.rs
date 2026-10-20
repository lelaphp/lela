//! The `zend_test` extension: `php-src/ext/zend_test` as one crate under
//! `ext/` (ADR-0004), off by default as php builds it only with
//! `--enable-zend-test`, and held by the oracle variant that loads it
//! (`oracle = "zend-test"`, #1782). Beside the facade it depends on
//! ext/standard alone, for `php_var_export_ex`; one entry,
//! `extension()`; its state on the value (ADR-0005). The phpt subset is
//! every `.phpt` under `php-src/ext/zend_test`, through `xtask phpt
//! --subset`.
//!
//! php's test extension is the native half of its Zend-level tests: each
//! function, class and constant reaches one API of the engine. Here
//! `test.c`'s are [`functions`] and [`classes`], with its weak table
//! [`weakmap`] and the functions that reach past the program [`internals`],
//! whose one act in C's terms, `zend_test_crash`'s `SIGSEGV`, is `ffi`'s
//! (`unsafe.homes`); `iterators.c`'s is [`iterators`] and `object_handlers.c`'s
//! are [`object_handlers`]. The observer of `observer.c` is [`observer`], the one
//! bundled user of design §4.3 seam 9, and the allocator hooks of
//! `zend_mm_custom_handlers.c` are ported by observation
//! ([`mm_custom_handlers`], #2024).
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod classes;
mod ffi;
mod fiber;
mod functions;
mod internals;
mod iterators;
mod mm_custom_handlers;
mod object_handlers;
mod observer;
mod weakmap;

use std::io::Write as _;

use php_ext::abi::{Extension, ExtensionBuilder, LongHandler};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::{Ini, Request};
use php_ext::values::{Emalloc, Owner, PhpObject, ResourceType, Value};

/// `zend_test.str_test`, `OnUpdateStr`.
pub(crate) const STR_TEST: &[u8] = b"zend_test.str_test";

/// `zend_test.quantity_value`, `OnUpdateLong`.
const QUANTITY_VALUE: &[u8] = b"zend_test.quantity_value";

/// `zend_test.not_empty_str_test`, `OnUpdateStrNotEmpty`.
const NOT_EMPTY_STR_TEST: &[u8] = b"zend_test.not_empty_str_test";

/// `zend_test.print_stderr_mshutdown`.
const PRINT_STDERR_MSHUTDOWN: &[u8] = b"zend_test.print_stderr_mshutdown";

/// `zend_test.register_passes`.
const REGISTER_PASSES: &[u8] = b"zend_test.register_passes";

/// `test.c`'s `PHP_INI_BEGIN` table, in its order.
/// `zend_test.limit_copy_file_range` is `HAVE_COPY_FILE_RANGE`'s, which
/// the oracle's platform has not. `replace_zend_execute_ex` and
/// `observe_opline_in_zendmm` swap a pointer and a heap of php's, nothing
/// a program prints tells apart, and here are entries alone.
const ENTRIES: [Entry; 7] = {
    use Modifiable::{All, System};
    [
        Entry::boolean(b"zend_test.replace_zend_execute_ex", b"0", System),
        Entry::boolean(REGISTER_PASSES, b"0", System),
        Entry::boolean(PRINT_STDERR_MSHUTDOWN, b"0", System),
        Entry::plain(QUANTITY_VALUE, b"0", All),
        Entry::plain(STR_TEST, b"", All),
        Entry::plain(NOT_EMPTY_STR_TEST, b"val", All),
        Entry::boolean(b"zend_test.observe_opline_in_zendmm", b"0", All),
    ]
};

/// Who may change an entry: its `modifiable` bits, `PHP_INI_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modifiable {
    /// `PHP_INI_ALL`: the host, a directory and a request.
    All,
    /// `PHP_INI_SYSTEM`: the host alone.
    System,
    /// `PHP_INI_USER`: a request alone.
    User,
}

/// The displayer an entry is registered with, which `MINFO` shows its
/// values through (`Z/main/php_ini.c:67`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Displayer {
    /// None, `STD_PHP_INI_ENTRY`'s: the value as it is, which
    /// `display_ini_entries` shows as `no value` where it is empty.
    Plain,
    /// `zend_ini_boolean_displayer_cb`, `STD_PHP_INI_BOOLEAN`'s: `On` or
    /// `Off`.
    Boolean,
}

/// One row of a `PHP_INI_BEGIN` table: the entry's name, its default,
/// who may change it and the displayer it is registered with.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) name: &'static [u8],
    pub(crate) default: &'static [u8],
    modifiable: Modifiable,
    displayer: Displayer,
}

impl Entry {
    /// `STD_PHP_INI_BOOLEAN`.
    pub(crate) const fn boolean(
        name: &'static [u8],
        default: &'static [u8],
        modifiable: Modifiable,
    ) -> Entry {
        Entry {
            name,
            default,
            modifiable,
            displayer: Displayer::Boolean,
        }
    }

    /// `STD_PHP_INI_ENTRY`.
    pub(crate) const fn plain(
        name: &'static [u8],
        default: &'static [u8],
        modifiable: Modifiable,
    ) -> Entry {
        Entry {
            name,
            default,
            modifiable,
            displayer: Displayer::Plain,
        }
    }

    /// The entry as the extension declares it.
    pub(crate) fn decl(&self) -> IniDecl {
        let (user, per_dir, system) = match self.modifiable {
            Modifiable::All => (true, true, true),
            Modifiable::System => (false, false, true),
            Modifiable::User => (true, false, false),
        };
        IniDecl {
            name: Name(self.name.into()),
            default: Some(self.default.into()),
            user,
            per_dir,
            system,
        }
    }

    /// A value of the entry as its displayer shows it.
    fn shown(&self, value: &[u8]) -> String {
        match self.displayer {
            Displayer::Boolean if Ini::parse_bool(value) => String::from("On"),
            Displayer::Boolean => String::from("Off"),
            Displayer::Plain => String::from_utf8_lossy(value).into_owned(),
        }
    }
}

/// The extension value (design §7.1): `zend_test_module_entry`, `MINIT`'s
/// registrations in its order, the request's edges and the `MINFO`
/// function.
///
/// ```php
/// var_dump(extension_loaded('zend_test'), ini_get('zend_test.observer.enabled'), ini_get('zend_test.observer.show_output'));
/// var_dump(ini_set('zend_test.observer.observe_function_names', 'foo'), ini_get('zend_test.observer.observe_function_names'));
/// var_dump(ini_set('zend_test.not_empty_str_test', ''), ini_set('zend_test.quantity_value', '1MB'), ini_get('zend_test.quantity_value'));
/// ```
/// ```expect
/// bool(true)
/// string(1) "0"
/// string(1) "1"
/// bool(false)
/// string(0) ""
///
/// Warning: Invalid "zend_test.quantity_value" setting. Invalid quantity "1MB": unknown multiplier "B", interpreting as "1" for backwards compatibility in %s on line 4
/// bool(false)
/// string(1) "0"
/// string(3) "1MB"
/// ```
pub fn extension() -> Extension {
    let builder = functions::declared(ExtensionBuilder::default());
    let builder = classes::declared(builder);
    let builder = fiber::declared(builder);
    let builder = iterators::declared(builder);
    let builder = object_handlers::declared(builder)
        .ini(ini())
        .on_update_long(QUANTITY_VALUE, LongHandler::Any)
        .on_modify(NOT_EMPTY_STR_TEST, on_update_not_empty);
    let builder = observer::declared(builder);
    mm_custom_handlers::declared(builder)
        .optimizer_pass(pass1)
        .optimizer_pass(pass2)
        .request_state::<State>()
        .rinit(rinit)
        .rshutdown(rshutdown)
        .engine_end(mshutdown)
        .info(info)
        .version("0.1.0")
        .build("zend_test")
}

/// The table as the extension declares it.
fn ini() -> Vec<IniDecl> {
    ENTRIES.iter().map(Entry::decl).collect()
}

/// `OnUpdateStrNotEmpty`: the empty string refused.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_not_empty(_: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    Ok(value.is_some_and(|value| !value.is_empty()))
}

/// The extension's request state (ADR-0005), the part of php's
/// `zend_test` request state this crate carries: the observer's, where
/// the allocation observer stands, the weak table, the resource type
/// whose destructor throws, the owners `zend_leak_variable` took, the
/// blocks `zend_leak_bytes` allocated, and `ZT_G(active_fiber)`, the
/// `_ZendTestFiber` a resume runs.
#[derive(Debug, Default)]
struct State {
    observer: observer::Observing,
    allocations: mm_custom_handlers::ObservedHeap,
    weakmap: weakmap::Map,
    throwing_resource: Option<ResourceType>,
    leaked: Vec<Owner<Value>>,
    leaked_bytes: Vec<Emalloc>,
    active_fiber: Option<PhpObject>,
}

/// `PHP_RINIT_FUNCTION(zend_test)`: the allocator hooks' start; the weak
/// table is made on its first use.
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    mm_custom_handlers::rinit(req)
}

/// `PHP_RSHUTDOWN_FUNCTION(zend_test)`: the weak table given up, then
/// the allocator hooks' end; and the owners `zend_leak_variable` took
/// and the blocks `zend_leak_bytes` allocated, which php frees with its
/// heap.
fn rshutdown(req: &mut Request) {
    weakmap::release(req);
    for leaked in std::mem::take(&mut req.state::<State>().leaked) {
        leaked.give_up(req);
    }
    for leaked in std::mem::take(&mut req.state::<State>().leaked_bytes) {
        leaked.free(req);
    }
    mm_custom_handlers::rshutdown(req);
}

/// `pass1`, the first of the two optimizer passes `MINIT` registers
/// under `zend_test.register_passes`, a `PHP_INI_SYSTEM` entry read
/// once there, so a pass is registered always and acts as one where the
/// entry stands on: a line printed for each file opcache compiles and
/// stores, and none for a file it serves as stored, nor where opcache
/// is off (`optimizer_register_pass.phpt`). The witness starts a child
/// with the entry on, served by a `php` carrying this extension (#2721):
/// the passes are called in registration order over each file opcache compiles,
/// and over none it serves as stored, nor where opcache is off.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/passes' . getmypid(); @mkdir($dir);
/// file_put_contents("$dir/main.php", '<?php echo "main\n"; include __DIR__ . "/inc.php"; include __DIR__ . "/inc.php";');
/// file_put_contents("$dir/inc.php", '<?php echo "inc\n";');
/// touch("$dir/main.php", time() - 10); touch("$dir/inc.php", time() - 10);
/// foreach ([['1', '1'], ['0', '1'], ['1', '0']] as [$cache, $passes]) {
///     $p = proc_open([PHP_BINARY, '-n', '-d', "opcache.enable_cli=$cache", '-d', 'opcache.jit=off', '-d', "zend_test.register_passes=$passes", "$dir/main.php"], [1 => ['pipe', 'w']], $pipes);
///     echo stream_get_contents($pipes[1]), "--\n"; proc_close($p);
/// }
/// exec('rm -rf ' . escapeshellarg($dir));
/// ```
/// ```expect process
/// pass1
/// pass2
/// main
/// pass1
/// pass2
/// inc
/// inc
/// --
/// main
/// inc
/// inc
/// --
/// main
/// inc
/// inc
/// --
/// ```
fn pass1(req: &mut Request, _: &[u8]) {
    registered_pass(req, b"pass1\n");
}

/// `pass2`, the second, called after the first.
fn pass2(req: &mut Request, _: &[u8]) {
    registered_pass(req, b"pass2\n");
}

/// What each pass prints, `php_printf`, where `MINIT` registered it.
fn registered_pass(req: &mut Request, line: &[u8]) {
    if req.ini().get(REGISTER_PASSES).is_some_and(Ini::parse_bool) {
        req.response_mut().write(line);
    }
}

/// `PHP_MSHUTDOWN_FUNCTION(zend_test)`: under `print_stderr_mshutdown`,
/// a line written to the process's standard error, as php's `fprintf`
/// writes it past the request.
fn mshutdown(req: &mut Request) {
    if req
        .ini()
        .get(PRINT_STDERR_MSHUTDOWN)
        .is_some_and(Ini::parse_bool)
    {
        std::io::stderr().write_all(b"[zend_test] MSHUTDOWN\n").ok();
    }
}

/// `PHP_MINFO_FUNCTION(zend_test)`: the one row, then the ini entries,
/// `DISPLAY_INI_ENTRIES()`, each value through its entry's displayer.
///
/// ```php
/// (new ReflectionExtension('zend_test'))->info();
/// ```
/// ```expect
///
/// zend_test
///
/// zend_test extension => enabled
///
/// Directive => Local Value => Master Value
/// zend_test.replace_zend_execute_ex => Off => Off
/// %A
/// zend_test.quantity_value => 0 => 0
/// zend_test.str_test => no value => no value
/// zend_test.not_empty_str_test => val => val
/// zend_test.observe_opline_in_zendmm => Off => Off
/// zend_test.observer.enabled => Off => Off
/// zend_test.observer.show_output => On => On
/// %A
/// zend_test.zend_mm_custom_handlers.enabled => Off => Off
/// ```
fn info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["zend_test extension", "enabled"]]);
    let Some(me) = req.extensions().named(b"zend_test") else {
        return;
    };
    let tables = ENTRIES
        .iter()
        .chain(&observer::ENTRIES)
        .chain([&mm_custom_handlers::ENTRY]);
    let rows: Vec<[String; 3]> = req
        .ini()
        .extension_ini(me)
        .into_iter()
        .filter_map(|held| {
            let entry = tables
                .clone()
                .find(|entry| *entry.name == *held.decl.name.0)?;
            let local = held.value.as_deref().unwrap_or_default();
            let master = held.master.as_deref().unwrap_or_default();
            let name = String::from_utf8_lossy(entry.name).into_owned();
            Some([name, entry.shown(local), entry.shown(master)])
        })
        .collect();
    let rows: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&rows);
}
