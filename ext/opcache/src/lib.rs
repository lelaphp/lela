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
//! natives answer from [`Code::unit_cache`]. What this crate keeps is
//! php's configuration of it: the ini entries and their handlers, the
//! startup verdict `accel_startup_ok` with the reason it failed, the
//! request's `accelerator_enabled`, and the blacklist. The compiled tier
//! is not behind it: the tier's entries are registered as php registers
//! them, and the tier they configure reports itself disabled.
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

mod blacklist;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod ffi;
mod file_override;
mod ini;
mod preload_user;
mod startup;

use php_ext::abi::{
    Admission, Extension, ExtensionBuilder, Instance, Lookup, OrFalse, Start, Validation,
    ZendExtension,
};
use php_ext::attr::{php_class, php_function};
use php_ext::coerce::{atoi, to_callable};
use php_ext::raising::{Thrown, notice, warning};
use php_ext::request::{
    CacheStatus, Invalidation, PHP_VERSION, PreloadStatistics, Request, Restart, TierStatus,
};
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::blacklist::Blacklist;
use crate::ini::{Log, Stored, accel_log, flag, long, value};

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
/// beside the startup verdict it was drawn from, and the directives an
/// include reads ([`ini::Directives`]).
#[derive(Default)]
struct State {
    pub(crate) startup: Startup,
    pub(crate) accelerator_enabled: bool,
    pub(crate) directives: ini::Directives,
}

/// Engine-scoped: `accel_blacklist`, loaded once from the startup ini.
struct Accelerator {
    blacklist: Blacklist,
}

impl Accelerator {
    fn start(start: &Start) -> Accelerator {
        let filename = start
            .ini()
            .entries
            .iter()
            .find(|(name, _)| name.as_slice() == b"opcache.blacklist_filename")
            .and_then(|(_, value)| value.as_deref())
            .unwrap_or_default();
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
        .functions(&[
            opcache_reset::DECLARED,
            opcache_get_status::DECLARED,
            opcache_compile_file::DECLARED,
            opcache_invalidate::DECLARED,
            opcache_jit_blacklist::DECLARED,
            opcache_get_configuration::DECLARED,
            opcache_is_script_cached::DECLARED,
            opcache_is_script_cached_in_file_cache::DECLARED,
        ])
        .ini(ini::declared());
    let builder = ini::handlers!(builder;
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
        24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
        46, 47, 48, 49, 50, 51, 52, 53);
    file_override::install(builder)
        .engine_state(|start| Ok(Accelerator::start(start)))
        .request_state::<State>()
        .rinit(rinit)
        .admission(admission)
        .post_startup(startup::post_startup)
        .cached(cached)
        .info(info)
        .zend_extension(ZendExtension {
            name: PRODUCT.to_owned(),
            version: PHP_VERSION.to_owned(),
            author: String::from("Zend Technologies"),
            url: String::from("http://www.zend.com/"),
            copyright: String::from("Copyright (c)"),
            statement_handler: None,
        })
        .build(PRODUCT)
}

/// `accel_startup`'s verdict and `ZEND_RINIT_FUNCTION(zend_accelerator)`:
/// the command line with `opcache.enable_cli` off fails to start, an
/// `opcache.enable` that is off never starts, and a request caches where
/// the cache started, is enabled, and is not the file cache alone; and
/// the request's `JIT_G` as `accel_post_startup` leaves it.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the engine's `RINIT` shape, which a hook that can throw needs"
)]
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    let startup = verdict(req);
    let enabled = startup == Startup::Ok && !flag(req, "opcache.file_cache_only");
    // `accel_post_startup` drops `JIT_G(enabled)` and `JIT_G(on)` where
    // the cache started without a buffer for the tier or with the file
    // cache alone (`Z/ext/opcache/ZendAccelerator.c:3336-3360`), so the
    // request's `JIT_G`, which starts as `opcache.jit`'s handler folded
    // the host's value, refuses every change from then on.
    if startup == Startup::Ok && !crate::ini::jit_kept(req) {
        let jit = req.code_mut().unit_cache().tier_setting();
        (jit.enabled, jit.on) = (false, false);
    }
    let directives = ini::Directives::of(req, None);
    let state = req.state::<State>();
    state.startup = startup;
    state.accelerator_enabled = enabled;
    state.directives = directives;
    Ok(())
}

/// `accel_startup`'s verdict, from the host's name and the startup ini.
pub(crate) fn verdict(req: &Request) -> Startup {
    if cli(req) && !flag(req, "opcache.enable_cli") {
        Startup::DisabledForCli
    } else if !flag(req, "opcache.enable") {
        Startup::Off
    } else {
        Startup::Ok
    }
}

/// `accel_sapi_is_cli`: the command line, or phpdbg.
pub(crate) fn cli(req: &Request) -> bool {
    matches!(req.environment().sapi_name(), "cli" | "phpdbg")
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
    let state = req.state::<State>();
    let set = state.directives.clone();
    let (startup, enabled) = (state.startup, state.accelerator_enabled);
    // `persistent_compile_file` without the accelerator: php without the
    // cache, or `file_cache_compile_file` where a file cache is set, the
    // cache started and `opcache.enable` stands.
    let started = startup == Startup::Ok && set.enable;
    let file_cache = set.file_cache.filter(|_| enabled || started);
    if !enabled && file_cache.is_none() {
        return Admission::Held {
            validation: Validation::EveryHit,
            lookup: Lookup::Resolved,
            protection: 0,
            file_cache: None,
        };
    }
    let listed = req
        .engine::<Accelerator>()
        .is_some_and(|accelerator| accelerator.blacklist.is_blacklisted(path));
    let limit = set.max_file_size;
    let large = limit > 0
        && std::fs::metadata(String::from_utf8_lossy(path).as_ref())
            .is_ok_and(|meta| i64::try_from(meta.len()).unwrap_or(i64::MAX) > limit);
    if listed || large {
        return Admission::KeptOut;
    }
    let validation = if set.validate_timestamps {
        Validation::Window {
            seconds: u32::try_from(set.revalidate_freq.max(0)).unwrap_or(u32::MAX),
        }
    } else {
        Validation::Never
    };
    let protection = u32::try_from(set.file_update_protection.max(0)).unwrap_or(u32::MAX);
    match file_cache {
        Some(file_cache) if !enabled => Admission::OnDisk {
            validation,
            protection,
            file_cache,
        },
        file_cache => {
            let lookup = if set.revalidate_path {
                Lookup::Resolved
            } else {
                Lookup::AsWritten
            };
            Admission::Held {
                validation,
                lookup,
                protection,
                file_cache,
            }
        }
    }
}

/// `cache_script_in_shared_memory` and `preload_script_in_shared_memory`
/// once a file is persisted: `Cached script` at `ACCEL_LOG_INFO`.
fn cached(req: &mut Request, path: &[u8]) {
    let message = format!("Cached script '{}'", String::from_utf8_lossy(path));
    accel_log(req, Log::Info, &message);
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
            .environment()
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
/// the cache emptied at the start of the first request no other request
/// runs beside, `accel_is_inactive`, and nothing new held until then; `false` where the cache is not running or a restart is
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
///
/// Served by the built-in server, whose requests share the cache: an
/// invalidated file's weight is wasted, out of what was used and none
/// of what was free, until the reset, which the next request's start
/// carries out once no request runs beside it, and which empties the
/// cache and the published arena (design §5.14) and wastes nothing:
///
/// ```php
/// $dir = realpath(sys_get_temp_dir()) . '/reset' . bin2hex(random_bytes(8));
/// mkdir($dir);
/// file_put_contents("$dir/held.inc", '<?php class Held { function m() { return 1; } }');
/// file_put_contents("$dir/index.php", '<?php $f = __DIR__ . "/held.inc";
/// if ($_GET["step"] == "a") {
///     include $f;
///     $s = opcache_get_status(); $was = $s["memory_usage"]; $weight = $s["scripts"][$f]["memory_consumption"];
///     var_dump($was["wasted_memory"], $was["current_wasted_percentage"], opcache_invalidate($f, true));
///     $now = opcache_get_status(false)["memory_usage"];
///     var_dump($now["wasted_memory"] === $weight, $now["used_memory"] === $was["used_memory"] - $weight, $now["free_memory"] === $was["free_memory"]);
///     var_dump($now["current_wasted_percentage"] === $weight / (128 * 1024 * 1024) * 100);
///     var_dump(opcache_reset(), opcache_get_status(false)["restart_pending"]);
/// } else {
///     $s = opcache_get_status(false);
///     var_dump($s["restart_pending"], $s["opcache_statistics"]["manual_restarts"], $s["memory_usage"]["wasted_memory"], $s["memory_usage"]["current_wasted_percentage"]);
///     include $f; var_dump((new Held)->m());
/// }');
/// touch("$dir/held.inc", time() - 60); touch("$dir/index.php", time() - 60);
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable=1', '-d', 'opcache.jit=off', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
/// $ready = [$pipes[2]]; $none = null;
/// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
/// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
/// foreach (['a', 'b'] as $step) { echo file_get_contents("http://127.0.0.1:$port/?step=$step"); }
/// proc_terminate($p); proc_close($p);
/// unlink("$dir/held.inc"); unlink("$dir/index.php"); rmdir($dir);
/// ```
/// ```expect process
/// int(0)
/// float(0)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// int(1)
/// int(0)
/// float(0)
/// int(1)
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
    let scheduled = req.code_mut().unit_cache().reset() == Restart::Scheduled;
    if scheduled {
        accel_log(req, Log::Debug, "Restart Scheduled! Reason: user");
    }
    scheduled
}

/// `add_assoc_*`: `value` under `name`.
fn put(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &str,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, name.as_bytes(), value)
}

/// A string of the request's as a value.
fn text(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// Signed from unsigned, saturating.
fn int(n: u64) -> Value {
    Value::int(i64::try_from(n).unwrap_or(i64::MAX))
}

/// `zend_accel_hash_init`'s table size: the first of its primes that
/// `opcache.max_accelerated_files` does not exceed.
pub(crate) fn max_cached_keys(req: &Request) -> i64 {
    const PRIMES: [i64; 18] = [
        5, 11, 19, 53, 107, 223, 463, 983, 1979, 3907, 7963, 16229, 32531, 65407, 130_987, 262_237,
        524_521, 1_048_793,
    ];
    let files = i64::from(atoi(value(req, "opcache.max_accelerated_files")));
    PRIMES.into_iter().find(|p| files <= *p).unwrap_or(files)
}

/// The megabytes `opcache.memory_consumption` gives the cache, in bytes.
pub(crate) fn consumption(req: &Request) -> i64 {
    i64::from(atoi(value(req, "opcache.memory_consumption"))).saturating_mul(1 << 20)
}

/// `date($format, $at)` as php's `php_format_date` answers it, under the
/// request's `date.timezone`; the seconds themselves where ext/date is
/// not loaded.
fn formatted(req: &mut Request, format: &str, at: i64) -> Result<String, Thrown> {
    let named = text(req, b"date")?;
    let date = to_callable(req, &named);
    named.give_up(req);
    let Ok(date) = date else {
        return Ok(at.to_string());
    };
    let format = text(req, format.as_bytes())?;
    let shown = php_ext::call::call(req, &date, &[*format, Value::int(at)]);
    format.give_up(req);
    let shown = shown?;
    let bytes = shown.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
    shown.give_up(req);
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
/// engine keeping no shared segment to measure: a file held weighs what
/// the file does, a file discarded is wasted at that weight until a
/// restart, and the interned strings the engine's table holds are
/// counted and weigh nothing. The `jit` row speaks for the compiled
/// tier ([`jit_status`]).
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
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if !api_allowed(req) {
        return Ok(OrFalse::False);
    }
    let (startup, enabled) = state(req);
    if startup != Startup::Ok {
        return Ok(OrFalse::False);
    }
    let status = req.code_mut().unit_cache().status();
    let mut out = PhpArray::new(req)?;
    put(req, &mut out, "opcache_enabled", Owner::bool(enabled))?;
    let file_cache = value(req, "opcache.file_cache").to_vec();
    if !file_cache.is_empty() {
        let shown = text(req, &file_cache)?;
        put(req, &mut out, "file_cache", shown)?;
    }
    if flag(req, "opcache.file_cache_only") {
        put(req, &mut out, "file_cache_only", Owner::bool(true))?;
        return Ok(OrFalse::Value(out));
    }
    put(req, &mut out, "cache_full", Owner::bool(status.cache_full))?;
    put(
        req,
        &mut out,
        "restart_pending",
        Owner::bool(status.restart_pending),
    )?;
    put(req, &mut out, "restart_in_progress", Owner::bool(false))?;

    let memory = memory_usage(req, &status)?;
    put(req, &mut out, "memory_usage", memory.into_value())?;
    if let Some(interned) = interned_strings_usage(req, &status)? {
        put(
            req,
            &mut out,
            "interned_strings_usage",
            interned.into_value(),
        )?;
    }
    let statistics = statistics(req, &status)?;
    put(req, &mut out, "opcache_statistics", statistics.into_value())?;
    if let Some(preload) = &status.preload {
        let preloaded = preload_statistics(req, preload)?;
        put(req, &mut out, "preload_statistics", preloaded.into_value())?;
    }
    if include_scripts && enabled {
        let listed = scripts(req, &status)?;
        put(req, &mut out, "scripts", listed.into_value())?;
    }
    let jit = jit_status(req, status.tier)?;
    put(req, &mut out, "jit", jit.into_value())?;
    Ok(OrFalse::Value(out))
}

/// An array of the request's with `rows` under their names, in order,
/// each one owner of its own of the view it is handed.
fn assoc(req: &mut Request, rows: Vec<(&str, Value)>) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for (name, value) in rows {
        put(req, &mut array, name, Owner::take(req, value))?;
    }
    Ok(array)
}

/// `memory_usage`: what the files held weigh, the configured memory
/// less that and what is wasted, `ZSMMG(wasted_shared_memory)`, and
/// the wasted part of the configured memory as a percentage.
fn memory_usage(req: &mut Request, status: &CacheStatus) -> Result<Owner<PhpArray>, Thrown> {
    let used = i64::try_from(status.used).unwrap_or(i64::MAX);
    let total = consumption(req);
    let wasted = i64::try_from(status.wasted).unwrap_or(i64::MAX);
    let free = total.saturating_sub(used).saturating_sub(wasted);
    #[allow(clippy::cast_precision_loss)] // php divides two doubles
    let percentage = wasted as f64 / total as f64 * 100.0;
    assoc(
        req,
        vec![
            ("used_memory", Value::int(used)),
            ("free_memory", Value::int(free)),
            ("wasted_memory", Value::int(wasted)),
            ("current_wasted_percentage", Value::float(percentage)),
        ],
    )
}

/// `interned_strings_usage`, where `opcache.interned_strings_buffer`
/// gives the strings a buffer: its size, the strings the engine's table
/// holds, and none of it used, the engine's table being its own.
fn interned_strings_usage(
    req: &mut Request,
    status: &CacheStatus,
) -> Result<Option<Owner<PhpArray>>, Thrown> {
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
fn statistics(req: &mut Request, status: &CacheStatus) -> Result<Owner<PhpArray>, Thrown> {
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
            ("oom_restarts", int(status.oom_restarts)),
            ("hash_restarts", int(status.hash_restarts)),
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

/// `preload_statistics`: what the preload weighs, then its functions and
/// its classes where it has any, and its scripts.
fn preload_statistics(
    req: &mut Request,
    preload: &PreloadStatistics,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    put(
        req,
        &mut out,
        "memory_consumption",
        Owner::take(req, int(preload.memory_consumption)),
    )?;
    for (name, names) in [
        ("functions", &preload.functions),
        ("classes", &preload.classes),
        ("scripts", &preload.scripts),
    ] {
        if names.is_empty() && name != "scripts" {
            continue;
        }
        let mut listed = PhpArray::new(req)?;
        for each in names {
            let each = text(req, each)?;
            listed.append(req, each)?;
        }
        put(req, &mut out, name, listed.into_value())?;
    }
    Ok(out)
}

/// `accelerator_get_scripts`: each file held under its full path.
fn scripts(req: &mut Request, status: &CacheStatus) -> Result<Owner<PhpArray>, Thrown> {
    let validated = flag(req, "opcache.validate_timestamps");
    let mut listed = PhpArray::new(req)?;
    let zone = php_ext::request::Zone::of(req);
    for script in &status.files {
        let path = text(req, &script.path)?;
        let when = text(req, ini::asctime(&zone, script.last_used).as_bytes())?;
        let mut rows = vec![
            ("full_path", *path),
            ("hits", int(script.hits)),
            ("memory_consumption", int(script.size)),
            ("last_used", *when),
            ("last_used_timestamp", Value::int(script.last_used)),
        ];
        if validated {
            rows.push(("timestamp", Value::int(script.timestamp)));
        }
        rows.push(("revalidate", Value::int(script.revalidate)));
        let row = assoc(req, rows);
        path.give_up(req);
        when.give_up(req);
        let row = row?;
        listed.set_str(req, &script.path, row.into_value())?;
    }
    Ok(listed)
}

/// `zend_jit_status` (`Z/ext/opcache/jit/zend_jit.c:825-842`): the
/// engine's tier as it reports itself, `JIT_G` as php's start left it
/// and the code buffer, of which `buffer_free` is what no trace's code
/// takes; php's stubs take some of it before any trace, which this tier
/// has none of, so the engine's whole buffer is pinned here
/// (`trace:jit-buffer-free-engine-code`). A build without the tier reports one that compiles
/// nothing, every field zero.
///
/// ```php
/// $jit = opcache_get_status(false)['jit'];
/// var_dump(implode(' ', array_keys($jit)), $jit['enabled'], $jit['on'], $jit['kind'], $jit['opt_level'], $jit['opt_flags']);
/// var_dump($jit['buffer_size'], $jit['buffer_free']);
/// ```
/// ```expect regime=jit-tracing process
/// string(59) "enabled on kind opt_level opt_flags buffer_size buffer_free"
/// bool(true)
/// bool(true)
/// int(5)
/// int(4)
/// int(6)
/// int(67108848)
/// int(67108848)
/// ```
///
/// A buffer of `opcache.jit_buffer_size=16K`, rounded to the page, less
/// what a loop's trace took once it compiled:
///
/// ```php
/// for ($i = 0, $s = 0; $i < 100; $i++) { $s += $i; }
/// $jit = opcache_get_status(false)['jit'];
/// var_dump($jit['buffer_size'], $jit['buffer_free'] < $jit['buffer_size'], $jit['buffer_free'] > 0);
/// ```
/// ```expect regime=jit-small-buffer process
/// int(16368)
/// bool(true)
/// bool(true)
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:2
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 1 $main() %s:3
/// ---- TRACE 2 start (side trace 1/1) $main() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
/// ```
fn jit_status(req: &mut Request, tier: TierStatus) -> Result<Owner<PhpArray>, Thrown> {
    let long = |n: u64| Value::int(i64::try_from(n).unwrap_or(i64::MAX));
    assoc(
        req,
        vec![
            ("enabled", Value::bool(tier.enabled)),
            ("on", Value::bool(tier.on)),
            ("kind", Value::int(tier.kind.into())),
            ("opt_level", Value::int(tier.opt_level.into())),
            ("opt_flags", Value::int(tier.opt_flags.into())),
            ("buffer_size", long(tier.buffer_size)),
            ("buffer_free", long(tier.buffer_free)),
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
    req.code_mut().unit_cache().compile(&written)
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
    req.code_mut().unit_cache().invalidate(&written, how)
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
///
/// Each entry is listed as `expand_filepath` resolved it at startup: a
/// link followed where the path has one, so a `..` after it climbs out of
/// where it led, a part that does not exist kept as written, a relative
/// entry taken against the list's directory and a trailing slash kept. A
/// file through the link is kept out of the cache.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/blacklist' . getmypid();
/// @mkdir("$dir/a/real", 0777, true);
/// symlink("$dir/a/real", "$dir/link");
/// file_put_contents("$dir/link/c.php", '<?php var_dump(opcache_is_script_cached(__FILE__));');
/// file_put_contents("$dir/list", "$dir/link/c.php\n$dir/link/none/x.php\n$dir/link/../y.php\nlink/\n");
/// echo str_replace(realpath($dir), 'D', shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 -d opcache.blacklist_filename=$dir/list -r 'echo implode(\"\\n\", opcache_get_configuration()[\"blacklist\"]), \"\\n\"; include \$argv[1];' $dir/link/c.php"));
/// exec('rm -rf ' . escapeshellarg($dir));
/// ```
/// ```expect process
/// D/a/real/c.php
/// D/a/real/none/x.php
/// D/a/y.php
/// D/a/real/
/// bool(false)
/// ```
///
/// A line is read as `fgets` hands it over and loses one pair of
/// surrounding quotes, so a line of a lone `"` is no entry at all.
///
/// ```php
/// $list = tempnam(sys_get_temp_dir(), 'blacklist');
/// file_put_contents($list, "\"\n/x\n\"/q\"\n\"\"\n");
/// echo shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 -d opcache.blacklist_filename=$list -r 'echo implode(\"\\n\", opcache_get_configuration()[\"blacklist\"]), \"\\n\";'");
/// unlink($list);
/// ```
/// ```expect process
/// /x
/// /q
/// ```
#[php_function]
fn opcache_get_configuration(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if !api_allowed(req) {
        return Ok(OrFalse::False);
    }
    let mut directives = PhpArray::new(req)?;
    for name in CONFIGURATION {
        let Some(entry) = ini::named(name) else {
            continue;
        };
        let stored = match entry.stored(value(req, name)) {
            Stored::Bool(b) => Owner::bool(b),
            Stored::Long(n) => Owner::int(n),
            Stored::Double(f) => Owner::float(f),
            Stored::Text(bytes) => text(req, &bytes)?,
        };
        put(req, &mut directives, name, stored)?;
    }
    let mut out = PhpArray::new(req)?;
    put(req, &mut out, "directives", directives.into_value())?;
    let mut version = PhpArray::new(req)?;
    let shown = text(req, PHP_VERSION.as_bytes())?;
    put(req, &mut version, "version", shown)?;
    let product = text(req, PRODUCT.as_bytes())?;
    put(req, &mut version, "opcache_product_name", product)?;
    put(req, &mut out, "version", version.into_value())?;
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
    put(req, &mut out, "blacklist", blacklist.into_value())?;
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
    req.code_mut().unit_cache().holds(&written, None)
}

/// `opcache_is_script_cached_in_file_cache(string $filename): bool`:
/// whether `opcache.file_cache` keeps the file as it stands, as
/// `filename_is_in_file_cache` asks it; `false` where neither the cache
/// nor the file cache alone runs, or no file cache is set.
///
/// ```php
/// var_dump(opcache_is_script_cached_in_file_cache(__FILE__));
/// ```
/// ```expect regime=jit-off
/// bool(false)
/// ```
///
/// A file compiled into the cache is kept on disk too, and invalidating
/// it removes both copies; with the file cache alone, it is kept on disk
/// and nothing invalidates it, the accelerator being off. The file cache
/// is set at startup, so a program of its own asks it.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/witness-fc-' . getmypid();
/// @mkdir($dir);
/// file_put_contents("$dir/kept.inc", '<?php return 1;');
/// file_put_contents("$dir/ask.php", '<?php $f = __DIR__ . "/kept.inc";
/// var_dump(opcache_is_script_cached_in_file_cache($f));
/// opcache_compile_file($f);
/// var_dump(opcache_is_script_cached_in_file_cache($f), opcache_is_script_cached($f));
/// opcache_invalidate($f, true);
/// var_dump(opcache_is_script_cached_in_file_cache($f), opcache_is_script_cached($f));');
/// echo shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 -d opcache.file_update_protection=0 -d opcache.file_cache=$dir $dir/ask.php");
/// echo shell_exec(escapeshellarg(PHP_BINARY) . " -n -d opcache.enable_cli=1 -d opcache.file_cache_only=1 -d opcache.file_update_protection=0 -d opcache.file_cache=$dir $dir/ask.php");
/// exec('rm -rf ' . escapeshellarg($dir));
/// ```
/// ```expect process
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn opcache_is_script_cached_in_file_cache(
    req: &mut Request,
    filename: PhpString,
) -> Result<bool, Thrown> {
    if !api_allowed(req) {
        return Ok(false);
    }
    let (_, enabled) = state(req);
    if !enabled && !flag(req, "opcache.file_cache_only") {
        return Ok(false);
    }
    let Some(file_cache) = req.state::<State>().directives.file_cache.clone() else {
        return Ok(false);
    };
    let written = filename.as_bytes().to_vec();
    req.code_mut()
        .unit_cache()
        .holds(&written, Some(&file_cache))
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
///
/// The `JIT` row is the tier's `JIT_G`, `On` while it traces:
///
/// ```php
/// ob_start(); (new ReflectionExtension('Zend OPcache'))->info();
/// echo implode("\n", preg_grep('/^JIT /', explode("\n", ob_get_clean()))), "\n";
/// ```
/// ```expect regime=jit-tracing process
/// JIT => On
/// ```
fn info(req: &mut Request) {
    info_rows(req).ok();
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
    let jit = *req.code_mut().unit_cache().tier_setting();
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
        (
            "JIT",
            match (jit.enabled, jit.on) {
                (false, _) => "Disabled",
                (true, true) => "On",
                (true, false) => "Off",
            },
        ),
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
        let status = req.code_mut().unit_cache().status();
        let consumption = consumption(req);
        let used = i64::try_from(status.used).unwrap_or(i64::MAX);
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
    req.extensions_mut().info_table(&table);
    Ok(())
}

/// `DISPLAY_INI_ENTRIES()` over opcache's entries: each under
/// `Directive => Local Value => Master Value`, the master being the
/// value the engine started with.
fn display_ini_entries(req: &mut Request) {
    let Some(me) = req.extensions().named(PRODUCT.as_bytes()) else {
        return;
    };
    let rows: Vec<[String; 3]> = req
        .ini()
        .extension_ini(me)
        .into_iter()
        .filter_map(|held| {
            let name = String::from_utf8_lossy(&held.decl.name.0).into_owned();
            let entry = ini::named(&name)?;
            let local = held.value.as_deref().unwrap_or_default();
            let master = held.master.as_deref().unwrap_or_default();
            Some([name.clone(), entry.shown(local), entry.shown(master)])
        })
        .collect();
    let rows: Vec<[&str; 3]> = rows
        .iter()
        .map(|[n, l, m]| [n.as_str(), l.as_str(), m.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&rows);
}
