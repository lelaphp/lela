//! The `opcache` extension: `php-src/ext/opcache` as one crate under `ext/`
//! (ADR-0004), `zend_accelerator_module.c` and
//! `zend_accelerator_blacklist.c` at the tag. One dependency, the
//! facade; one entry, `extension()`; its state on the value (ADR-0005).
//! The phpt subset is `php-src/ext/opcache/tests`, through
//! `xtask phpt --subset`.
//!
//! php registers opcache twice, as the extension `Zend OPcache` and as the
//! Zend extension of the same name, and 8.5 builds it in always. The
//! cache it fronts is the engine's (design §6.3): no shared memory
//! segment, no persist and no file cache live here, and the eight
//! natives answer from [`Request::unit_cache`]. What this crate keeps is
//! php's configuration of it: the ini entries and their handlers, the
//! startup verdict `accel_startup_ok` with the reason it failed, the
//! request's `accelerator_enabled`, and the blacklist. The compiled tier
//! is not behind it: the tier's entries are registered as php registers
//! them, and the tier they configure reports itself disabled.
#![forbid(unsafe_code)]
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

mod blacklist;
mod ini;

use php_ext::abi::{
    Admission, Extension, ExtensionBuilder, Instance, Lookup, OrFalse, StartupIni, Validation,
    ZendExtension,
};
use php_ext::attr::{php_class, php_function};
use php_ext::coerce::to_callable;
use php_ext::raising::{Thrown, notice, warning};
use php_ext::request::{CacheStatus, Invalidation, PHP_VERSION, Request, Restart};
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::blacklist::Blacklist;
use crate::ini::{Stored, flag, long, value};

/// `ACCELERATOR_PRODUCT_NAME`, the extension's name and the Zend
/// extension's.
pub(crate) const PRODUCT: &str = "Zend OPcache";

/// Core's `Closure`, what `opcache_jit_blacklist` takes.
#[php_class(extern)]
struct Closure;

/// `accel_startup_ok` and why it is not: `accel_startup` decides it once
/// per process, by the host's name and the startup ini. The engine is
/// built before any host names itself, so a request decides it at its
/// start from the same two, which no request can change.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Startup {
    /// `opcache.enable` was off: `accel_startup` returned before it
    /// started anything, and gave no reason.
    #[default]
    Off,
    /// `zps_startup_failure("Opcode Caching is disabled for CLI")`: the
    /// host is the command line and `opcache.enable_cli` is off.
    DisabledForCli,
    /// Started.
    Ok,
}

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005). `ZCG(accelerator_enabled)`, which
/// `ini_set('opcache.enable', 0)` turns off for the rest of the request,
/// beside the startup verdict it was drawn from.
#[derive(Default)]
struct State {
    pub(crate) startup: Startup,
    pub(crate) accelerator_enabled: bool,
}

/// Engine-scoped: `accel_blacklist`, loaded once from the startup ini.
struct Accelerator {
    blacklist: Blacklist,
}

impl Accelerator {
    fn start(ini: &StartupIni) -> Accelerator {
        let filename = ini
            .entries
            .iter()
            .find(|(name, _)| name.as_slice() == b"opcache.blacklist_filename")
            .map_or(&[][..], |(_, value)| value.as_slice());
        Accelerator {
            blacklist: Blacklist::load(filename),
        }
    }
}

/// The extension value (design §7.1): `Zend OPcache`, a Zend
/// extension too, its eight natives, its ini entries each with its
/// handler, its state, and the answer the unit cache asks for a file.
///
/// ```php
/// var_dump(extension_loaded('Zend OPcache'), extension_loaded('opcache'), count(get_extension_funcs('Zend OPcache')));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// int(8)
/// ```
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .functions(vec![
            opcache_reset::declared(),
            opcache_get_status::declared(),
            opcache_compile_file::declared(),
            opcache_invalidate::declared(),
            opcache_jit_blacklist::declared(),
            opcache_get_configuration::declared(),
            opcache_is_script_cached::declared(),
            opcache_is_script_cached_in_file_cache::declared(),
        ])
        .ini(ini::declared());
    ini::handlers!(builder;
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
        24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
        46, 47, 48, 49, 50, 51, 52, 53)
    .engine_state(Accelerator::start)
    .request_state::<State>()
    .rinit(rinit)
    .admission(admission)
    .info(info)
    .zend_extension(ZendExtension {
        name: PRODUCT.to_owned(),
        version: PHP_VERSION.to_owned(),
        author: String::from("Zend Technologies"),
        url: String::from("http://www.zend.com/"),
        copyright: String::from("Copyright (c)"),
    })
    .build(PRODUCT)
}

/// `accel_startup`'s verdict and `ZEND_RINIT_FUNCTION(zend_accelerator)`:
/// the command line with `opcache.enable_cli` off fails to start, an
/// `opcache.enable` that is off never starts, and a request caches where
/// the cache started, is enabled, and is not the file cache alone.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the engine's `RINIT` shape, which a hook that can throw needs"
)]
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    let cli = matches!(req.sapi_name(), "cli" | "phpdbg");
    let startup = if cli && !flag(req, "opcache.enable_cli") {
        Startup::DisabledForCli
    } else if !flag(req, "opcache.enable") {
        Startup::Off
    } else {
        Startup::Ok
    };
    let enabled = startup == Startup::Ok && !flag(req, "opcache.file_cache_only");
    let state = req.state::<State>();
    state.startup = startup;
    state.accelerator_enabled = enabled;
    Ok(())
}

/// What the unit cache does with the file at the full path `path`, as
/// `persistent_compile_file` decides it: without the accelerator, php
/// without the cache; a file the blacklist names, or one larger than a
/// positive `opcache.max_file_size`, kept out; any other held, validated
/// as `opcache.validate_timestamps` and `opcache.revalidate_freq` say,
/// looked for as `opcache.revalidate_path` says, and held only once
/// `opcache.file_update_protection` has passed since it was written.
///
/// ```php
/// $file = __DIR__ . '/window.inc';
/// file_put_contents($file, '<?php return 1;');
/// ini_set('opcache.revalidate_freq', 60);
/// var_dump(include $file);
/// file_put_contents($file, '<?php return 22;');
/// touch($file, time() - 10);
/// $s = opcache_get_status()['scripts'][$file];
/// var_dump(include $file, opcache_is_script_cached($file), $s['revalidate'] - $s['last_used_timestamp']);
/// ini_set('opcache.revalidate_freq', 0);
/// var_dump(include $file);
/// unlink($file);
/// ```
/// ```expect regime=jit-off
/// int(1)
/// int(1)
/// bool(true)
/// int(60)
/// int(22)
/// ```
///
/// A relative path is filed under the path as written, with the working
/// directory and the `include_path` beside it, and a hit found by it is
/// served without the path resolved where nothing is due; with
/// `opcache.revalidate_path` on, the path is resolved on every hit.
///
/// ```php
/// ini_set('opcache.validate_timestamps', 0);
/// chdir(__DIR__);
/// file_put_contents('relative.inc', '<?php return 5;');
/// $keys = fn () => opcache_get_status()['opcache_statistics']['num_cached_keys'];
/// $before = $keys();
/// var_dump(include 'relative.inc', $keys() - $before);
/// unlink('relative.inc');
/// var_dump(include 'relative.inc', opcache_is_script_cached('relative.inc'));
/// ini_set('opcache.revalidate_path', 1);
/// var_dump(@include 'relative.inc');
/// ```
/// ```expect regime=jit-off
/// int(5)
/// int(2)
/// int(5)
/// bool(true)
/// bool(false)
/// ```
fn admission(req: &mut Request, path: &[u8]) -> Admission {
    let (_, enabled) = state(req);
    if !enabled {
        return Admission::Held {
            validation: Validation::EveryHit,
            lookup: Lookup::Resolved,
            protection: 0,
        };
    }
    let listed = req
        .engine::<Accelerator>()
        .is_some_and(|accelerator| accelerator.blacklist.is_blacklisted(path));
    let limit = long(req, "opcache.max_file_size");
    let large = limit > 0
        && std::fs::metadata(String::from_utf8_lossy(path).as_ref())
            .is_ok_and(|meta| i64::try_from(meta.len()).unwrap_or(i64::MAX) > limit);
    if listed || large {
        return Admission::KeptOut;
    }
    let validation = if flag(req, "opcache.validate_timestamps") {
        let seconds = long(req, "opcache.revalidate_freq");
        Validation::Window {
            seconds: u32::try_from(seconds.max(0)).unwrap_or(u32::MAX),
        }
    } else {
        Validation::Never
    };
    let lookup = if flag(req, "opcache.revalidate_path") {
        Lookup::Resolved
    } else {
        Lookup::AsWritten
    };
    let protection = long(req, "opcache.file_update_protection");
    Admission::Held {
        validation,
        lookup,
        protection: u32::try_from(protection.max(0)).unwrap_or(u32::MAX),
    }
}

/// The startup verdict and `accelerator_enabled`.
fn state(req: &mut Request) -> (Startup, bool) {
    let state = req.state::<State>();
    (state.startup, state.accelerator_enabled)
}

/// `validate_api_restriction`: with `opcache.restrict_api` set, only a
/// program whose path begins with it may ask, and any other is php's
/// warning and `false`.
fn api_allowed(req: &mut Request) -> bool {
    let restrict = value(req, "opcache.restrict_api").to_vec();
    if restrict.is_empty()
        || req
            .script_name()
            .is_some_and(|script| script.starts_with(&restrict))
    {
        return true;
    }
    warning(
        req,
        "Zend OPcache API is restricted by \"restrict_api\" configuration directive",
    );
    false
}

/// `opcache_reset(): bool`: `zend_accel_schedule_restart(ACCEL_RESTART_USER)`,
/// the cache emptied at the next request's start and nothing new held
/// until then; `false` where the cache is not running or a restart is
/// pending already.
///
/// ```php
/// var_dump(opcache_reset());
/// ```
/// ```expect
/// bool(false)
/// ```
///
/// A reset holds the cache as it is for the rest of the request, and
/// what the request compiles after it is held nowhere.
///
/// ```php
/// file_put_contents(__DIR__ . '/before.inc', '<?php return 1;');
/// file_put_contents(__DIR__ . '/after.inc', '<?php return 2;');
/// include __DIR__ . '/before.inc';
/// var_dump(opcache_reset(), opcache_reset(), opcache_get_status(false)['restart_pending']);
/// include __DIR__ . '/after.inc';
/// var_dump(opcache_is_script_cached(__DIR__ . '/before.inc'), opcache_is_script_cached(__DIR__ . '/after.inc'));
/// unlink(__DIR__ . '/before.inc'); unlink(__DIR__ . '/after.inc');
/// ```
/// ```expect regime=jit-off
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn opcache_reset(req: &mut Request) -> bool {
    if !api_allowed(req) {
        return false;
    }
    let (startup, _) = state(req);
    if !flag(req, "opcache.enable") || startup != Startup::Ok {
        return false;
    }
    req.unit_cache().reset() == Restart::Scheduled
}

/// `add_assoc_*`: `value` under `name`.
fn put(req: &mut Request, array: &mut PhpArray, name: &str, value: Value) -> Result<(), Thrown> {
    let key = Key::Str(PhpString::new(req, name.as_bytes())?);
    array.set(req, key, value)
}

/// A string of the request's as a value.
fn text(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// Signed from unsigned, saturating.
fn int(n: u64) -> Value {
    Value::int(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `zend_accel_hash_init`'s table size: the first of its primes that
/// `opcache.max_accelerated_files` does not exceed.
fn max_cached_keys(req: &Request) -> i64 {
    const PRIMES: [i64; 18] = [
        5, 11, 19, 53, 107, 223, 463, 983, 1979, 3907, 7963, 16229, 32531, 65407, 130_987, 262_237,
        524_521, 1_048_793,
    ];
    let files = ini::atoi(value(req, "opcache.max_accelerated_files"));
    PRIMES.into_iter().find(|p| files <= *p).unwrap_or(files)
}

/// The megabytes `opcache.memory_consumption` gives the cache, in bytes.
fn consumption(req: &Request) -> i64 {
    ini::atoi(value(req, "opcache.memory_consumption")).saturating_mul(1 << 20)
}

/// `date($format, $at)` as php's `php_format_date` answers it, under the
/// request's `date.timezone`; the seconds themselves where ext/date is
/// not loaded.
fn formatted(req: &mut Request, format: &str, at: i64) -> Result<String, Thrown> {
    let named = text(req, b"date")?;
    let Ok(date) = to_callable(req, &named) else {
        return Ok(at.to_string());
    };
    let format = text(req, format.as_bytes())?;
    let shown = php_ext::call::call(req, &date, &[format, Value::int(at)])?;
    let bytes = shown.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
    req.release(shown);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Now, in seconds since the epoch.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or(0)
}

/// `opcache_get_status(bool $include_scripts = true): array|false`: the
/// cache's state as php's accelerator reports it, `false` where the
/// cache never started. The memory figures are the configuration's, the
/// engine keeping no shared segment to measure: nothing is wasted, a
/// file held weighs what the file does, and the interned strings the
/// engine's table holds are counted and weigh nothing. The `jit` row
/// speaks for the compiled tier, and this engine's compiles nothing.
///
/// ```php
/// var_dump(opcache_get_status());
/// ```
/// ```expect
/// bool(false)
/// ```
///
/// ```php
/// $file = __DIR__ . '/held.inc';
/// file_put_contents($file, '<?php return 1;');
/// include $file; include $file;
/// $s = opcache_get_status();
/// var_dump($s['opcache_enabled'], $s['cache_full'], $s['restart_pending'], $s['restart_in_progress']);
/// var_dump(implode(' ', array_keys($s)), implode(' ', array_keys($s['opcache_statistics'])), $s['opcache_statistics']['max_cached_keys']);
/// $held = $s['scripts'][realpath($file)];
/// var_dump($held['full_path'] === realpath($file), $held['hits'], implode(' ', array_keys($held)), $s['jit']['on']);
/// var_dump($s['interned_strings_usage']['buffer_size'], array_key_exists('scripts', opcache_get_status(false)));
/// unlink($file);
/// ```
/// ```expect regime=jit-off
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// string(129) "opcache_enabled cache_full restart_pending restart_in_progress memory_usage interned_strings_usage opcache_statistics scripts jit"
/// string(189) "num_cached_scripts num_cached_keys max_cached_keys hits start_time last_restart_time oom_restarts hash_restarts manual_restarts misses blacklist_misses blacklist_miss_ratio opcache_hit_rate"
/// int(16229)
/// bool(true)
/// int(1)
/// string(84) "full_path hits memory_consumption last_used last_used_timestamp timestamp revalidate"
/// bool(false)
/// int(8388608)
/// bool(false)
/// ```
#[php_function]
fn opcache_get_status(
    req: &mut Request,
    #[php(default = true)] include_scripts: bool,
) -> Result<OrFalse<PhpArray>, Thrown> {
    if !api_allowed(req) {
        return Ok(OrFalse::False);
    }
    let (startup, enabled) = state(req);
    if startup != Startup::Ok {
        return Ok(OrFalse::False);
    }
    let status = req.unit_cache().status();
    let mut out = PhpArray::new(req)?;
    put(req, &mut out, "opcache_enabled", Value::bool(enabled))?;
    let file_cache = value(req, "opcache.file_cache").to_vec();
    if !file_cache.is_empty() {
        let shown = text(req, &file_cache)?;
        put(req, &mut out, "file_cache", shown)?;
    }
    if flag(req, "opcache.file_cache_only") {
        put(req, &mut out, "file_cache_only", Value::bool(true))?;
        return Ok(OrFalse::Value(out));
    }
    put(req, &mut out, "cache_full", Value::bool(false))?;
    put(
        req,
        &mut out,
        "restart_pending",
        Value::bool(status.restart_pending),
    )?;
    put(req, &mut out, "restart_in_progress", Value::bool(false))?;

    let memory = memory_usage(req, &status)?;
    put(req, &mut out, "memory_usage", Value::array(memory))?;
    if let Some(interned) = interned_strings_usage(req, &status)? {
        put(
            req,
            &mut out,
            "interned_strings_usage",
            Value::array(interned),
        )?;
    }
    let statistics = statistics(req, &status)?;
    put(
        req,
        &mut out,
        "opcache_statistics",
        Value::array(statistics),
    )?;
    if include_scripts && enabled {
        let listed = scripts(req, &status)?;
        put(req, &mut out, "scripts", Value::array(listed))?;
    }
    let jit = jit_status(req)?;
    put(req, &mut out, "jit", Value::array(jit))?;
    Ok(OrFalse::Value(out))
}

/// An array of the request's with `rows` under their names, in order.
fn assoc(req: &mut Request, rows: Vec<(&str, Value)>) -> Result<PhpArray, Thrown> {
    let mut array = PhpArray::new(req)?;
    for (name, value) in rows {
        put(req, &mut array, name, value)?;
    }
    Ok(array)
}

/// The bytes the files held weigh, what the status calls used memory.
fn used_memory(status: &CacheStatus) -> i64 {
    status
        .files
        .iter()
        .map(|file| i64::try_from(file.size).unwrap_or(i64::MAX))
        .fold(0, i64::saturating_add)
}

/// `memory_usage`: the configured memory less what the files held weigh,
/// nothing wasted.
fn memory_usage(req: &mut Request, status: &CacheStatus) -> Result<PhpArray, Thrown> {
    let used = used_memory(status);
    let free = consumption(req).saturating_sub(used);
    assoc(
        req,
        vec![
            ("used_memory", Value::int(used)),
            ("free_memory", Value::int(free)),
            ("wasted_memory", Value::int(0)),
            ("current_wasted_percentage", Value::float(0.0)),
        ],
    )
}

/// `interned_strings_usage`, where `opcache.interned_strings_buffer`
/// gives the strings a buffer: its size, the strings the engine's table
/// holds, and none of it used, the engine's table being its own.
fn interned_strings_usage(
    req: &mut Request,
    status: &CacheStatus,
) -> Result<Option<PhpArray>, Thrown> {
    let buffer = long(req, "opcache.interned_strings_buffer").saturating_mul(1 << 20);
    if buffer <= 0 {
        return Ok(None);
    }
    assoc(
        req,
        vec![
            ("buffer_size", Value::int(buffer)),
            ("used_memory", Value::int(0)),
            ("free_memory", Value::int(buffer)),
            ("number_of_strings", int(status.interned_strings)),
        ],
    )
    .map(Some)
}

/// `part` of `whole` as php's status divides them, a percentage of
/// doubles and zero for no whole.
#[allow(
    clippy::cast_precision_loss,
    reason = "php divides the two counters as doubles"
)]
fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// `opcache_statistics`: the counters, one key per file held, and the
/// two ratios over every file asked for.
fn statistics(req: &mut Request, status: &CacheStatus) -> Result<PhpArray, Thrown> {
    let scripts = u64::try_from(status.files.len()).unwrap_or(u64::MAX);
    let asked = status
        .hits
        .saturating_add(status.misses)
        .saturating_add(status.blacklist_misses);
    let keys = max_cached_keys(req);
    assoc(
        req,
        vec![
            ("num_cached_scripts", int(scripts)),
            ("num_cached_keys", int(status.keys)),
            ("max_cached_keys", Value::int(keys)),
            ("hits", int(status.hits)),
            ("start_time", Value::int(status.start_time)),
            ("last_restart_time", Value::int(status.last_restart_time)),
            ("oom_restarts", Value::int(0)),
            ("hash_restarts", Value::int(0)),
            ("manual_restarts", int(status.manual_restarts)),
            ("misses", int(status.misses)),
            ("blacklist_misses", int(status.blacklist_misses)),
            (
                "blacklist_miss_ratio",
                Value::float(percent(status.blacklist_misses, asked)),
            ),
            (
                "opcache_hit_rate",
                Value::float(percent(status.hits, asked)),
            ),
        ],
    )
}

/// `accelerator_get_scripts`: each file held under its full path.
fn scripts(req: &mut Request, status: &CacheStatus) -> Result<PhpArray, Thrown> {
    let validated = flag(req, "opcache.validate_timestamps");
    let mut listed = PhpArray::new(req)?;
    for script in &status.files {
        let path = text(req, &script.path)?;
        let when = text(req, ini::asctime(script.last_used).as_bytes())?;
        let mut rows = vec![
            ("full_path", path),
            ("hits", int(script.hits)),
            ("memory_consumption", int(script.size)),
            ("last_used", when),
            ("last_used_timestamp", Value::int(script.last_used)),
        ];
        if validated {
            rows.push(("timestamp", Value::int(script.timestamp)));
        }
        rows.push(("revalidate", Value::int(script.revalidate)));
        let row = assoc(req, rows)?;
        let at = Key::Str(PhpString::new(req, &script.path)?);
        listed.set(req, at, Value::array(row))?;
    }
    Ok(listed)
}

/// `zend_jit_status` for a tier that compiles nothing.
fn jit_status(req: &mut Request) -> Result<PhpArray, Thrown> {
    assoc(
        req,
        vec![
            ("enabled", Value::bool(false)),
            ("on", Value::bool(false)),
            ("kind", Value::int(0)),
            ("opt_level", Value::int(0)),
            ("opt_flags", Value::int(0)),
            ("buffer_size", Value::int(0)),
            ("buffer_free", Value::int(0)),
        ],
    )
}

/// `opcache_compile_file(string $filename): bool`: the file compiled into
/// the cache without running it, its functions and the classes it can
/// bind declared, as `persistent_compile_file` under
/// `ZEND_COMPILE_WITHOUT_EXECUTION` leaves them; a cache that never
/// started compiles nothing and says so.
///
/// ```php
/// var_dump(opcache_compile_file(__FILE__));
/// ```
/// ```expect
/// Notice: Zend OPcache has not been properly started, can't compile file in %s on line 2
/// bool(false)
/// ```
///
/// ```php
/// file_put_contents(__DIR__ . '/compiled.inc', '<?php echo "ran\n"; function compiled_f() { return 1; } class Compiled {}');
/// var_dump(opcache_compile_file(__DIR__ . '/compiled.inc'), function_exists('compiled_f'), class_exists('Compiled', false));
/// var_dump(opcache_is_script_cached(__DIR__ . '/compiled.inc'), in_array(realpath(__DIR__ . '/compiled.inc'), get_included_files()));
/// var_dump(@opcache_compile_file('nowhere.inc'));
/// unlink(__DIR__ . '/compiled.inc');
/// ```
/// ```expect regime=jit-off
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn opcache_compile_file(req: &mut Request, filename: PhpString) -> Result<bool, Thrown> {
    let (startup, _) = state(req);
    if startup != Startup::Ok {
        notice(
            req,
            "Zend OPcache has not been properly started, can't compile file",
        );
        return Ok(false);
    }
    let written = filename.as_bytes().to_vec();
    req.unit_cache().compile(&written)
}

/// `opcache_invalidate(string $filename, bool $force = false): bool`:
/// `zend_accel_invalidate`, the file's cached copy discarded where it is
/// forced, where timestamps are not validated, or where the file changed
/// since; `true` where the file was found, on disk or in the cache.
///
/// ```php
/// var_dump(opcache_invalidate(__FILE__, true));
/// ```
/// ```expect
/// bool(false)
/// ```
///
/// ```php
/// $file = __DIR__ . '/held.inc';
/// file_put_contents($file, '<?php return 1;');
/// include $file;
/// var_dump(opcache_invalidate($file), opcache_is_script_cached($file));
/// var_dump(opcache_invalidate($file, true), opcache_is_script_cached($file));
/// var_dump(opcache_invalidate('nowhere.php'));
/// unlink($file);
/// ```
/// ```expect regime=jit-off
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
///
/// The path is resolved as an include resolves it, and what a wrapper's
/// `url_stat` throws on the way is thrown.
///
/// ```php
/// class W { public $context; function url_stat($path, $flags) { throw new Exception("stat failed"); } }
/// stream_wrapper_register('test', 'W');
/// set_include_path('test://dir');
/// try { opcache_invalidate('missing.php'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect regime=jit-off
/// stat failed
/// ```
#[php_function]
fn opcache_invalidate(
    req: &mut Request,
    filename: PhpString,
    #[php(default = false)] force: bool,
) -> Result<bool, Thrown> {
    if !api_allowed(req) {
        return Ok(false);
    }
    let (_, enabled) = state(req);
    if !enabled {
        return Ok(false);
    }
    let how = if force || !flag(req, "opcache.validate_timestamps") {
        Invalidation::Forced
    } else {
        Invalidation::Stale
    };
    let written = filename.as_bytes().to_vec();
    req.unit_cache().invalidate(&written, how)
}

/// `opcache_jit_blacklist(Closure $closure): void`: keeps the compiled tier from
/// the closure's function; this engine's tier compiles nothing, so there
/// is nothing to keep it from.
///
/// ```php
/// var_dump(opcache_jit_blacklist(function () {}));
/// try { opcache_jit_blacklist('strlen'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// opcache_jit_blacklist(): Argument #1 ($closure) must be of type Closure, string given
/// ```
#[php_function]
fn opcache_jit_blacklist(closure: Instance<Closure>) {
    let _ = closure;
}

/// `opcache_get_configuration(): array|false`: the directives as the
/// handlers stored them, in php's order for this array, the version, and
/// the blacklist's entries where the cache started.
///
/// ```php
/// $c = opcache_get_configuration();
/// var_dump(count($c['directives']), $c['directives']['opcache.enable'], $c['directives']['opcache.memory_consumption']);
/// var_dump($c['directives']['opcache.max_wasted_percentage'], $c['directives']['opcache.jit_prof_threshold'], $c['directives']['opcache.optimization_level']);
/// var_dump($c['version']['opcache_product_name'], $c['version']['version'] === PHP_VERSION, $c['blacklist']);
/// var_dump(array_diff_key(ini_get_all('zend opcache'), $c['directives']));
/// ```
/// ```expect
/// int(54)
/// bool(true)
/// int(134217728)
/// float(0.05)
/// float(0.005)
/// int(2147401727)
/// string(12) "Zend OPcache"
/// bool(true)
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
#[php_function]
fn opcache_get_configuration(req: &mut Request) -> Result<OrFalse<PhpArray>, Thrown> {
    if !api_allowed(req) {
        return Ok(OrFalse::False);
    }
    let mut directives = PhpArray::new(req)?;
    for name in CONFIGURATION {
        let Some(entry) = ini::named(name) else {
            continue;
        };
        let stored = match entry.stored(value(req, name)) {
            Stored::Bool(b) => Value::bool(b),
            Stored::Long(n) => Value::int(n),
            Stored::Double(f) => Value::float(f),
            Stored::Text(bytes) => text(req, &bytes)?,
        };
        put(req, &mut directives, name, stored)?;
    }
    let mut out = PhpArray::new(req)?;
    put(req, &mut out, "directives", Value::array(directives))?;
    let mut version = PhpArray::new(req)?;
    let shown = text(req, PHP_VERSION.as_bytes())?;
    put(req, &mut version, "version", shown)?;
    let product = text(req, PRODUCT.as_bytes())?;
    put(req, &mut version, "opcache_product_name", product)?;
    put(req, &mut out, "version", Value::array(version))?;
    let (startup, _) = state(req);
    let entries: Vec<Vec<u8>> = if startup == Startup::Ok {
        req.engine::<Accelerator>()
            .map(|accelerator| accelerator.blacklist.entries().to_vec())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut blacklist = PhpArray::new(req)?;
    for entry in entries {
        let entry = text(req, &entry)?;
        blacklist.append(req, entry)?;
    }
    put(req, &mut out, "blacklist", Value::array(blacklist))?;
    Ok(OrFalse::Value(out))
}

/// The directives in the order `opcache_get_configuration` adds them,
/// which is not the order php registers the entries in.
const CONFIGURATION: [&str; 54] = [
    "opcache.enable",
    "opcache.enable_cli",
    "opcache.use_cwd",
    "opcache.validate_timestamps",
    "opcache.validate_permission",
    "opcache.validate_root",
    "opcache.dups_fix",
    "opcache.revalidate_path",
    "opcache.log_verbosity_level",
    "opcache.memory_consumption",
    "opcache.interned_strings_buffer",
    "opcache.max_accelerated_files",
    "opcache.max_wasted_percentage",
    "opcache.force_restart_timeout",
    "opcache.revalidate_freq",
    "opcache.preferred_memory_model",
    "opcache.blacklist_filename",
    "opcache.max_file_size",
    "opcache.error_log",
    "opcache.protect_memory",
    "opcache.save_comments",
    "opcache.record_warnings",
    "opcache.enable_file_override",
    "opcache.optimization_level",
    "opcache.lockfile_path",
    "opcache.file_cache",
    "opcache.file_cache_read_only",
    "opcache.file_cache_only",
    "opcache.file_cache_consistency_checks",
    "opcache.file_update_protection",
    "opcache.opt_debug_level",
    "opcache.restrict_api",
    "opcache.huge_code_pages",
    "opcache.preload",
    "opcache.preload_user",
    "opcache.jit",
    "opcache.jit_buffer_size",
    "opcache.jit_debug",
    "opcache.jit_bisect_limit",
    "opcache.jit_blacklist_root_trace",
    "opcache.jit_blacklist_side_trace",
    "opcache.jit_hot_func",
    "opcache.jit_hot_loop",
    "opcache.jit_hot_return",
    "opcache.jit_hot_side_exit",
    "opcache.jit_max_exit_counters",
    "opcache.jit_max_loop_unrolls",
    "opcache.jit_max_polymorphic_calls",
    "opcache.jit_max_recursive_calls",
    "opcache.jit_max_recursive_returns",
    "opcache.jit_max_root_traces",
    "opcache.jit_max_side_traces",
    "opcache.jit_prof_threshold",
    "opcache.jit_max_trace_length",
];

/// `opcache_is_script_cached(string $filename): bool`: whether the cache
/// holds the file as it stands, `false` where the cache is not running.
///
/// ```php
/// var_dump(opcache_is_script_cached(__FILE__));
/// ```
/// ```expect
/// bool(false)
/// ```
///
/// ```php
/// $file = __DIR__ . '/held.inc';
/// file_put_contents($file, '<?php return 1;');
/// var_dump(opcache_is_script_cached($file));
/// include $file;
/// var_dump(opcache_is_script_cached($file), opcache_is_script_cached('nowhere.php'));
/// unlink($file);
/// ```
/// ```expect regime=jit-off
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn opcache_is_script_cached(req: &mut Request, filename: PhpString) -> Result<bool, Thrown> {
    if !api_allowed(req) {
        return Ok(false);
    }
    let (_, enabled) = state(req);
    if !enabled {
        return Ok(false);
    }
    let written = filename.as_bytes().to_vec();
    req.unit_cache().holds(&written)
}

/// `opcache_is_script_cached_in_file_cache(string $filename): bool`:
/// `false` where neither the cache nor the file cache runs or no
/// `opcache.file_cache` is set, and `false` again where one is, since the
/// engine keeps no second-level cache on disk (design §6.3, month two).
///
/// ```php
/// var_dump(opcache_is_script_cached_in_file_cache(__FILE__));
/// ```
/// ```expect regime=jit-off
/// bool(false)
/// ```
#[php_function]
fn opcache_is_script_cached_in_file_cache(req: &mut Request, filename: PhpString) -> bool {
    let _ = filename;
    let _ = api_allowed(req);
    false
}

/// `zend_accel_info`, opcache's `MINFO`: what runs and what does not,
/// then, where the cache started, its counters, then the directives with
/// a boolean shown `On` or `Off` and an empty one `no value`.
///
/// ```php
/// (new ReflectionExtension('Zend OPcache'))->info();
/// ```
/// ```expect
/// Zend OPcache
///
/// Opcode Caching => Disabled
/// Optimization => Disabled
/// SHM Cache => Enabled
/// File Cache => Disabled
/// JIT => Disabled
/// Startup Failed => Opcode Caching is disabled for CLI
///
/// Directive => Local Value => Master Value
/// opcache.enable => On => On
/// opcache.use_cwd => On => On
/// opcache.validate_timestamps => On => On
/// opcache.validate_permission => Off => Off
/// opcache.validate_root => Off => Off
/// opcache.dups_fix => Off => Off
/// opcache.revalidate_path => Off => Off
/// opcache.log_verbosity_level => 1 => 1
/// opcache.memory_consumption => 128 => 128
/// opcache.interned_strings_buffer => 8 => 8
/// opcache.max_accelerated_files => 10000 => 10000
/// opcache.max_wasted_percentage => 5 => 5
/// opcache.force_restart_timeout => 180 => 180
/// opcache.revalidate_freq => 0 => 0
/// opcache.file_update_protection => 0 => 0
/// opcache.preferred_memory_model => no value => no value
/// opcache.blacklist_filename => no value => no value
/// opcache.max_file_size => 0 => 0
/// opcache.protect_memory => On => On
/// opcache.save_comments => On => On
/// opcache.record_warnings => Off => Off
/// opcache.optimization_level => 0x7FFEBFFF => 0x7FFEBFFF
/// opcache.opt_debug_level => 0 => 0
/// opcache.enable_file_override => Off => Off
/// opcache.enable_cli => Off => Off
/// opcache.error_log => no value => no value
/// opcache.restrict_api => no value => no value
/// opcache.lockfile_path => /tmp => /tmp
/// opcache.file_cache => no value => no value
/// opcache.file_cache_read_only => Off => Off
/// opcache.file_cache_only => Off => Off
/// opcache.file_cache_consistency_checks => On => On
/// opcache.huge_code_pages => Off => Off
/// opcache.preload => no value => no value
/// opcache.preload_user => no value => no value
/// opcache.jit => disable => disable
/// opcache.jit_buffer_size => 64M => 64M
/// opcache.jit_debug => 0 => 0
/// opcache.jit_bisect_limit => 0 => 0
/// opcache.jit_prof_threshold => 0.005 => 0.005
/// opcache.jit_max_root_traces => 100000 => 100000
/// opcache.jit_max_side_traces => 100000 => 100000
/// opcache.jit_max_exit_counters => 100000 => 100000
/// opcache.jit_hot_loop => 1 => 1
/// opcache.jit_hot_func => 1 => 1
/// opcache.jit_hot_return => 1 => 1
/// opcache.jit_hot_side_exit => 1 => 1
/// opcache.jit_blacklist_root_trace => 16 => 16
/// opcache.jit_blacklist_side_trace => 8 => 8
/// opcache.jit_max_loop_unrolls => 8 => 8
/// opcache.jit_max_recursive_calls => 2 => 2
/// opcache.jit_max_recursive_returns => 2 => 2
/// opcache.jit_max_polymorphic_calls => 2 => 2
/// opcache.jit_max_trace_length => 1024 => 1024
/// ```
fn info(req: &mut Request) {
    let _ = info_rows(req);
    display_ini_entries(req);
}

/// The status rows of [`info`].
fn info_rows(req: &mut Request) -> Result<(), Thrown> {
    let (startup, enabled) = state(req);
    let file_cache_only = flag(req, "opcache.file_cache_only");
    let on = flag(req, "opcache.enable");
    let started = on && startup == Startup::Ok;
    let optimising = started && long(req, "opcache.optimization_level") != 0;
    let file_cache = !value(req, "opcache.file_cache").is_empty();
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, shown) in [
        (
            "Opcode Caching",
            if enabled || file_cache_only {
                "Up and Running"
            } else {
                "Disabled"
            },
        ),
        (
            "Optimization",
            if optimising { "Enabled" } else { "Disabled" },
        ),
        (
            "SHM Cache",
            if file_cache_only {
                "Disabled"
            } else {
                "Enabled"
            },
        ),
        (
            "File Cache",
            if file_cache { "Enabled" } else { "Disabled" },
        ),
        ("JIT", "Disabled"),
    ] {
        rows.push((name.to_owned(), shown.to_owned()));
    }
    if on && startup == Startup::DisabledForCli {
        rows.push((
            String::from("Startup Failed"),
            String::from("Opcode Caching is disabled for CLI"),
        ));
    }
    if started && !file_cache_only {
        let status = req.unit_cache().status();
        let consumption = consumption(req);
        let used = used_memory(&status);
        let buffer = long(req, "opcache.interned_strings_buffer").saturating_mul(1 << 20);
        let scripts = status.files.len().to_string();
        let since = formatted(req, "Y-m-d\\TH:i:sO", status.start_time)?;
        let restarted = if status.last_restart_time == 0 {
            String::from("none")
        } else {
            formatted(req, "Y-m-d\\TH:i:sO", status.last_restart_time)?
        };
        let mut shown = vec![
            ("Startup", String::from("OK")),
            ("Shared memory model", String::from("mmap")),
            ("Cache hits", status.hits.to_string()),
            ("Cache misses", status.misses.to_string()),
            ("Used memory", used.to_string()),
            ("Free memory", consumption.saturating_sub(used).to_string()),
            ("Wasted memory", String::from("0")),
        ];
        if buffer > 0 {
            shown.push(("Interned Strings Used memory", String::from("0")));
            shown.push(("Interned Strings Free memory", buffer.to_string()));
        }
        shown.extend([
            ("Cached scripts", scripts.clone()),
            ("Cached keys", scripts),
            ("Max keys", max_cached_keys(req).to_string()),
            ("OOM restarts", String::from("0")),
            ("Hash keys restarts", String::from("0")),
            ("Manual restarts", status.manual_restarts.to_string()),
            ("Start time", since),
            ("Last restart time", restarted),
            ("Last force restart time", String::from("none")),
        ]);
        rows.extend(
            shown
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value)),
        );
    }
    let table: Vec<[&str; 2]> = rows
        .iter()
        .map(|(name, shown)| [name.as_str(), shown.as_str()])
        .collect();
    let table: Vec<&[&str]> = table.iter().map(<[&str; 2]>::as_slice).collect();
    req.info_table(&table);
    Ok(())
}

/// `DISPLAY_INI_ENTRIES()` over opcache's entries: each under
/// `Directive => Local Value => Master Value`, the master being the
/// value the engine started with.
fn display_ini_entries(req: &mut Request) {
    let Some(me) = req.extension_named(PRODUCT.as_bytes()) else {
        return;
    };
    let rows: Vec<[String; 3]> = req
        .extension_ini(me)
        .into_iter()
        .filter_map(|held| {
            let name = String::from_utf8_lossy(&held.decl.name.0).into_owned();
            let entry = ini::named(&name)?;
            let master = held.original.as_deref().unwrap_or(&held.value);
            Some([name.clone(), entry.shown(&held.value), entry.shown(master)])
        })
        .collect();
    let mut table: Vec<[&str; 3]> = vec![["Directive", "Local Value", "Master Value"]];
    table.extend(
        rows.iter()
            .map(|[n, l, m]| [n.as_str(), l.as_str(), m.as_str()]),
    );
    let table: Vec<&[&str]> = table.iter().map(<[&str; 3]>::as_slice).collect();
    req.info_table(&table);
}
