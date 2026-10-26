//! opcache's ini entries, `ZEND_INI_BEGIN()` of
//! `zend_accelerator_module.c` at the tag, the compiled tier's among them as php
//! registers them under `HAVE_JIT`: each name with its default, where it
//! may be changed, and what its `ZEND_INI_MH` does with a value. What an
//! entry holds is its text; what php's handler stored, `accel_directives`
//! and `jit_globals`, is [`Stored`], read off the text when asked.

use php_ext::abi::{ExtensionBuilder, IniStage, LongHandler};
use php_ext::coerce::atoi;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Thrown, warning};
use php_ext::request::{Ini, Request, TierStatus, Zone};

use crate::State;

/// What an entry's handler does with a value, and so how the value reads
/// back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `STD_PHP_INI_BOOLEAN` over `OnUpdateBool`, shown `On` or `Off`.
    Bool,
    /// `opcache.enable`'s `OnEnable`: a boolean that a request may turn
    /// off and never on.
    Enable,
    /// `OnUpdateLong`: a quantity, whose odd shapes warn.
    Long,
    /// `OnUpdateString`.
    Text,
    /// `OnUpdateStringUnempty`: the empty string is refused.
    Unempty,
    /// `OnUpdateFileCache`: the empty string is no directory at all.
    FileCache,
    /// `OnUpdateMemoryConsumption`: megabytes, eight at least.
    Memory,
    /// `OnUpdateInternedStringsBuffer`: megabytes, a quantity in range.
    InternedStrings,
    /// `OnUpdateMaxAcceleratedFiles`: between 200 and a million.
    MaxFiles,
    /// `OnUpdateMaxWastedPercentage`: a percentage from 1 to 50.
    MaxWasted,
    /// `OnUpdateJit`, `zend_jit_config`.
    Jit,
    /// `OnUpdateJitDebug`, `zend_jit_debug_config`.
    JitDebug,
    /// `OnUpdateReal`.
    Real,
    /// `OnUpdateCounter`: 0 to 255.
    Counter,
    /// `OnUpdateUnrollL`: 1 to `ZEND_JIT_TRACE_MAX_LOOPS_UNROLL` less one.
    UnrollLoops,
    /// `OnUpdateUnrollC`: 1 to `ZEND_JIT_TRACE_MAX_CALL_DEPTH` less one.
    UnrollCalls,
    /// `OnUpdateUnrollR`: 0 to `ZEND_JIT_TRACE_MAX_RET_DEPTH` less one.
    UnrollReturns,
    /// `OnUpdateMaxTraceLength`: 4 to `ZEND_JIT_TRACE_MAX_LENGTH`.
    TraceLength,
}

/// One entry: its name, its default, `None` for php's `NULL`, whether a
/// program may change it (`PHP_INI_ALL`) or only the host
/// (`PHP_INI_SYSTEM`), and its kind.
pub(crate) struct Entry {
    pub(crate) name: &'static str,
    default: Option<&'static str>,
    all: bool,
    pub(crate) kind: Kind,
}

const fn entry(name: &'static str, default: &'static str, all: bool, kind: Kind) -> Entry {
    Entry {
        name,
        default: Some(default),
        all,
        kind,
    }
}

/// An entry php registers with no value, `NULL`.
const fn unset(name: &'static str, all: bool, kind: Kind) -> Entry {
    Entry {
        name,
        default: None,
        all,
        kind,
    }
}

const ALL: bool = true;
const SYSTEM: bool = false;

/// `ZEND_JIT_TRACE_MAX_CALL_DEPTH`, `_RET_DEPTH`, `_LOOPS_UNROLL` and
/// `_LENGTH` (`ext/opcache/jit/zend_jit.h:89-95`).
const MAX_CALL_DEPTH: i64 = 10;
const MAX_RET_DEPTH: i64 = 4;
const MAX_LOOPS_UNROLL: i64 = 10;
const MAX_TRACE_LENGTH: i64 = 1024;

/// The entries in registration order, which is the order `php --re`
/// and `ini_get_all()` list them.
pub(crate) const ENTRIES: [Entry; 54] = [
    entry("opcache.enable", "1", ALL, Kind::Enable),
    entry("opcache.use_cwd", "1", SYSTEM, Kind::Bool),
    entry("opcache.validate_timestamps", "1", ALL, Kind::Bool),
    entry("opcache.validate_permission", "0", SYSTEM, Kind::Bool),
    entry("opcache.validate_root", "0", SYSTEM, Kind::Bool),
    entry("opcache.dups_fix", "0", ALL, Kind::Bool),
    entry("opcache.revalidate_path", "0", ALL, Kind::Bool),
    entry("opcache.log_verbosity_level", "1", SYSTEM, Kind::Long),
    entry("opcache.memory_consumption", "128", SYSTEM, Kind::Memory),
    entry(
        "opcache.interned_strings_buffer",
        "8",
        SYSTEM,
        Kind::InternedStrings,
    ),
    entry(
        "opcache.max_accelerated_files",
        "10000",
        SYSTEM,
        Kind::MaxFiles,
    ),
    entry(
        "opcache.max_wasted_percentage",
        "5",
        SYSTEM,
        Kind::MaxWasted,
    ),
    entry("opcache.force_restart_timeout", "180", SYSTEM, Kind::Long),
    entry("opcache.revalidate_freq", "2", ALL, Kind::Long),
    entry("opcache.file_update_protection", "2", ALL, Kind::Long),
    entry("opcache.preferred_memory_model", "", SYSTEM, Kind::Unempty),
    entry("opcache.blacklist_filename", "", SYSTEM, Kind::Text),
    entry("opcache.max_file_size", "0", SYSTEM, Kind::Long),
    entry("opcache.protect_memory", "0", SYSTEM, Kind::Bool),
    entry("opcache.save_comments", "1", SYSTEM, Kind::Bool),
    entry("opcache.record_warnings", "0", SYSTEM, Kind::Bool),
    entry(
        "opcache.optimization_level",
        "0x7FFEBFFF",
        SYSTEM,
        Kind::Long,
    ),
    entry("opcache.opt_debug_level", "0", SYSTEM, Kind::Long),
    entry("opcache.enable_file_override", "0", SYSTEM, Kind::Bool),
    entry("opcache.enable_cli", "0", SYSTEM, Kind::Bool),
    entry("opcache.error_log", "", SYSTEM, Kind::Text),
    entry("opcache.restrict_api", "", SYSTEM, Kind::Text),
    entry("opcache.lockfile_path", "/tmp", SYSTEM, Kind::Text),
    unset("opcache.file_cache", SYSTEM, Kind::FileCache),
    entry("opcache.file_cache_read_only", "0", SYSTEM, Kind::Bool),
    entry("opcache.file_cache_only", "0", SYSTEM, Kind::Bool),
    entry(
        "opcache.file_cache_consistency_checks",
        "1",
        SYSTEM,
        Kind::Bool,
    ),
    entry("opcache.huge_code_pages", "0", SYSTEM, Kind::Bool),
    entry("opcache.preload", "", SYSTEM, Kind::Unempty),
    entry("opcache.preload_user", "", SYSTEM, Kind::Unempty),
    entry("opcache.jit", "disable", ALL, Kind::Jit),
    entry("opcache.jit_buffer_size", "64M", SYSTEM, Kind::Long),
    entry("opcache.jit_debug", "0", ALL, Kind::JitDebug),
    entry("opcache.jit_bisect_limit", "0", ALL, Kind::Long),
    entry("opcache.jit_prof_threshold", "0.005", ALL, Kind::Real),
    entry("opcache.jit_max_root_traces", "1024", SYSTEM, Kind::Long),
    entry("opcache.jit_max_side_traces", "128", SYSTEM, Kind::Long),
    entry("opcache.jit_max_exit_counters", "8192", SYSTEM, Kind::Long),
    entry("opcache.jit_hot_loop", "61", SYSTEM, Kind::Counter),
    entry("opcache.jit_hot_func", "127", SYSTEM, Kind::Counter),
    entry("opcache.jit_hot_return", "8", SYSTEM, Kind::Counter),
    entry("opcache.jit_hot_side_exit", "8", ALL, Kind::Counter),
    entry("opcache.jit_blacklist_root_trace", "16", ALL, Kind::Counter),
    entry("opcache.jit_blacklist_side_trace", "8", ALL, Kind::Counter),
    entry("opcache.jit_max_loop_unrolls", "8", ALL, Kind::UnrollLoops),
    entry(
        "opcache.jit_max_recursive_calls",
        "2",
        ALL,
        Kind::UnrollCalls,
    ),
    entry(
        "opcache.jit_max_recursive_returns",
        "2",
        ALL,
        Kind::UnrollReturns,
    ),
    entry("opcache.jit_max_polymorphic_calls", "2", ALL, Kind::Long),
    entry(
        "opcache.jit_max_trace_length",
        "1024",
        ALL,
        Kind::TraceLength,
    ),
];

/// The declarations the engine registers.
pub(crate) fn declared() -> Vec<IniDecl> {
    ENTRIES
        .iter()
        .map(|entry| IniDecl {
            name: Name(entry.name.as_bytes().into()),
            default: entry.default.map(|default| default.as_bytes().into()),
            user: entry.all,
            per_dir: entry.all,
            system: true,
        })
        .collect()
}

/// The entry named `name`.
pub(crate) fn named(name: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|entry| entry.name == name)
}

/// `zend_strtod` over the leading number, zero where there is none.
fn strtod(value: &[u8]) -> f64 {
    let text = String::from_utf8_lossy(value);
    let text = text.trim_start();
    (1..=text.len())
        .rev()
        .filter(|end| text.is_char_boundary(*end))
        .find_map(|end| text[..end].parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// What php's handler stored for an entry, as `opcache_get_configuration`
/// reads it back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stored {
    Bool(bool),
    Long(i64),
    Double(f64),
    Text(Vec<u8>),
}

impl Entry {
    /// What the handler stored for `value`, the entry's text.
    pub(crate) fn stored(&self, value: &[u8]) -> Stored {
        match self.kind {
            Kind::Bool | Kind::Enable => Stored::Bool(Ini::parse_bool(value)),
            Kind::Memory => Stored::Long(i64::from(atoi(value)).saturating_mul(1024 * 1024)),
            Kind::MaxFiles => Stored::Long(i64::from(atoi(value))),
            Kind::MaxWasted => Stored::Double(f64::from(atoi(value)) / 100.0),
            Kind::Real => Stored::Double(strtod(value)),
            Kind::Text | Kind::Unempty | Kind::FileCache | Kind::Jit => {
                Stored::Text(value.to_vec())
            }
            Kind::Long
            | Kind::InternedStrings
            | Kind::JitDebug
            | Kind::Counter
            | Kind::UnrollLoops
            | Kind::UnrollCalls
            | Kind::UnrollReturns
            | Kind::TraceLength => Stored::Long(Ini::parse_quantity(value).0),
        }
    }

    /// The value as its displayer shows it: `On` and `Off` for a boolean,
    /// the value as it is otherwise, which `display_ini_entries` shows as
    /// `no value` where it is empty.
    pub(crate) fn shown(&self, value: &[u8]) -> String {
        match self.kind {
            Kind::Bool | Kind::Enable if Ini::parse_bool(value) => String::from("On"),
            Kind::Bool | Kind::Enable => String::from("Off"),
            _ => String::from_utf8_lossy(value).into_owned(),
        }
    }
}

/// The entry's value in force for this request.
pub(crate) fn value<'r>(req: &'r Request, name: &str) -> &'r [u8] {
    let fallback = named(name)
        .and_then(|entry| entry.default)
        .unwrap_or_default();
    req.ini()
        .get(name.as_bytes())
        .unwrap_or(fallback.as_bytes())
}

/// The entry read as a boolean.
pub(crate) fn flag(req: &Request, name: &str) -> bool {
    Ini::parse_bool(value(req, name))
}

/// The entry read as `OnUpdateLong` stores it.
pub(crate) fn long(req: &Request, name: &str) -> i64 {
    Ini::parse_quantity(value(req, name)).0
}

/// `zend_ini_parse_quantity_warn`: the value, and php's warning naming
/// the entry for a shape it reads for backwards compatibility.
fn quantity(req: &mut Request, name: &str, value: &[u8]) -> i64 {
    req.ini_mut().parse_quantity_warn(name.as_bytes(), value)
}

/// The `ZEND_INI_MH` of entry number `AT`, which the engine serves with
/// the host's value at startup and with a program's at `ini_set()`.
/// `false` is php's `FAILURE`: the entry keeps what it had.
///
/// A refused memory setting is logged at `ACCEL_LOG_WARNING`
/// ([`accel_log`]), which the host writes before it serves.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the engine's `OnModify` shape, which a handler that can throw needs"
)]
pub(crate) fn modify<const AT: usize>(
    req: &mut Request,
    value: Option<&[u8]>,
) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    let Some(entry) = ENTRIES.get(AT) else {
        return Ok(false);
    };
    let name = entry.name;
    let ranged = |req: &mut Request, low: i64, high: i64, message: &str| {
        let parsed = quantity(req, name, value);
        if (low..=high).contains(&parsed) {
            true
        } else {
            warning(req, &format!("Invalid \"{name}\" setting{message}"));
            false
        }
    };
    Ok(match entry.kind {
        Kind::Bool | Kind::Text | Kind::FileCache | Kind::Real => true,
        Kind::Unempty => !value.is_empty(),
        // A `Kind::Long` entry's handler is the engine's ([`handler`]).
        Kind::Long => {
            quantity(req, name, value);
            true
        }
        Kind::JitDebug => on_jit_debug(req, value),
        Kind::Enable => on_enable(req, value),
        Kind::Memory => refused_unless(
            req,
            atoi(value) >= 8,
            "opcache.memory_consumption is set below the required 8MB.\n",
        ),
        Kind::InternedStrings => {
            let size = quantity(req, name, value);
            refused_unless(
                req,
                size >= 0,
                &format!(
                    "opcache.interned_strings_buffer must be greater than or equal to 0, {size} given.\n"
                ),
            ) && refused_unless(
                req,
                size <= MAX_INTERNED_STRINGS_BUFFER,
                &format!(
                    "opcache.interned_strings_buffer must be less than or equal to {MAX_INTERNED_STRINGS_BUFFER}, {size} given.\n"
                ),
            )
        }
        Kind::MaxFiles => {
            let files = i64::from(atoi(value));
            refused_unless(
                req,
                files >= 200,
                "opcache.max_accelerated_files is set below the required minimum (200).\n",
            ) && refused_unless(
                req,
                files <= 1_000_000,
                "opcache.max_accelerated_files is set above the limit (1000000).\n",
            )
        }
        Kind::MaxWasted => refused_unless(
            req,
            (1..=50).contains(&atoi(value)),
            "opcache.max_wasted_percentage must be set between 1 and 50.\n",
        ),
        Kind::Jit => on_jit(req, value),
        Kind::Counter => ranged(
            req,
            0,
            255,
            "; using default value instead. Should be between 0 and 255",
        ),
        Kind::UnrollLoops => ranged(
            req,
            1,
            MAX_LOOPS_UNROLL - 1,
            &format!(". Should be between 1 and {MAX_LOOPS_UNROLL}"),
        ),
        Kind::UnrollCalls => ranged(
            req,
            1,
            MAX_CALL_DEPTH - 1,
            &format!(". Should be between 1 and {MAX_CALL_DEPTH}"),
        ),
        Kind::UnrollReturns => ranged(
            req,
            0,
            MAX_RET_DEPTH - 1,
            &format!(". Should be between 0 and {MAX_RET_DEPTH}"),
        ),
        Kind::TraceLength => ranged(
            req,
            4,
            MAX_TRACE_LENGTH,
            &format!(". Should be between 4 and {MAX_TRACE_LENGTH}"),
        ),
    })
}

/// `MAX_INTERNED_STRINGS_BUFFER_SIZE` on a 64-bit build: what
/// `nTableMask` bounds, `UINT32_MAX / (32 * 1024 * 4)`.
const MAX_INTERNED_STRINGS_BUFFER: i64 = 32767;

/// `OnEnable`: at startup, and as the request ends and hands the entry
/// back the value it started with, a boolean like any other; in a
/// request, off turns the cache off for the rest of it, and on is
/// refused with php's warning unless it is on already, so the request's
/// end raises nothing over the value it turned off.
///
/// ```php
/// var_dump(ini_set('opcache.enable', '1'), ini_set('opcache.enable', '0'), ini_get('opcache.enable'));
/// var_dump(ini_set('opcache.enable', '1'), ini_get('opcache.enable'));
/// ```
/// ```expect
/// string(1) "1"
/// string(1) "1"
/// string(1) "0"
///
/// Warning: Zend OPcache can't be temporarily enabled (it may be only disabled until the end of request) in %s on line 3
/// bool(false)
/// string(1) "0"
/// ```
fn on_enable(req: &mut Request, value: &[u8]) -> bool {
    let stage = req.ini().stage();
    if matches!(stage, IniStage::Startup | IniStage::Deactivate) || !Ini::parse_bool(value) {
        if stage == IniStage::Runtime {
            req.state::<State>().accelerator_enabled = false;
        }
        return true;
    }
    if flag(req, "opcache.enable") {
        return true;
    }
    if stage == IniStage::Activate {
        // `ZEND_INI_STAGE_ACTIVATE`: a host's own value, which php-fpm's
        // pools set, logged rather than raised.
        let message = if req.environment().sapi_name() == "fpm-fcgi" {
            "Zend OPcache can't be temporarily enabled. Are you using php_admin_value[opcache.enable]=1 in an individual pool's configuration?"
        } else {
            "Zend OPcache can't be temporarily enabled (it may be only disabled until the end of request)"
        };
        accel_log(req, Log::Warning, message);
        return false;
    }
    warning(
        req,
        "Zend OPcache can't be temporarily enabled (it may be only disabled until the end of request)",
    );
    false
}

/// `OnUpdateJit`, `zend_jit_config` (`Z/ext/opcache/jit/zend_jit.c:3680-3733`):
/// `disable`, the forms of off and on, `tracing`, `function`, or php's
/// four digits, folded into the request's `JIT_G`
/// ([`UnitCache::tier_setting`](php_ext::request::UnitCache::tier_setting)),
/// which the tier's countability and `opcache_get_status()['jit']` read.
/// Once started, a setting is refused while `JIT_G(enabled)` is off, with
/// php's warning where a program's `ini_set` asks: under
/// `disable`, and after a setting php refused, which turns the tier off
/// for the rest of the request (`failure:`). `off` keeps the digits the
/// last setting that turned the tier on wrote, and a number php refuses
/// those it passed.
///
/// ```php
/// var_dump(ini_set('opcache.jit', 'tracing'), ini_get('opcache.jit'));
/// ```
/// ```expect
/// Warning: Cannot change opcache.jit setting at run-time (JIT is disabled) in %s on line 2
/// bool(false)
/// string(7) "disable"
/// ```
///
/// ```php
/// var_dump(ini_set('opcache.jit', 'tracing'), ini_set('opcache.jit', 'sideways'), ini_get('opcache.jit'));
/// var_dump(ini_set('opcache.jit', 'off'), ini_get('opcache.jit'));
/// ```
/// ```expect regime=jit-off
/// Warning: Invalid "opcache.jit" setting. Should be "disable", "on", "off", "tracing", "function" or 4-digit number in %s on line 2
/// string(0) ""
/// bool(false)
/// string(7) "tracing"
///
/// Warning: Cannot change opcache.jit setting at run-time (JIT is disabled) in %s on line 3
/// bool(false)
/// string(7) "tracing"
/// ```
///
/// and `disable` turns the tier off for the rest of the request:
///
/// ```php
/// var_dump(ini_set('opcache.jit', 'disable'), ini_set('opcache.jit', 'tracing'), ini_get('opcache.jit'));
/// ```
/// ```expect regime=jit-off
/// Warning: Cannot change opcache.jit setting at run-time (JIT is disabled) in %s on line 2
/// string(0) ""
/// bool(false)
/// string(7) "disable"
/// ```
///
/// as a cache that starts with no buffer for the tier, or with the file
/// cache alone, starts every request with it off (`rinit`):
///
/// ```php
/// foreach ([['opcache.jit_buffer_size=0'], ['opcache.file_cache_only=1', 'opcache.file_cache=' . sys_get_temp_dir()]] as $ini) {
///     $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=tracing'];
///     foreach ($ini as $line) { array_push($cmd, '-d', $line); }
///     array_push($cmd, '-r', 'var_dump(ini_set("opcache.jit", "off"));');
///     $p = proc_open($cmd, [1 => ['pipe', 'w']], $pipes);
///     echo stream_get_contents($pipes[1]);
///     proc_close($p);
/// }
/// ```
/// ```expect process
/// Warning: Cannot change opcache.jit setting at run-time (JIT is disabled) in Command line code on line 1
/// bool(false)
///
/// Warning: Cannot change opcache.jit setting at run-time (JIT is disabled) in Command line code on line 1
/// bool(false)
/// ```
fn on_jit(req: &mut Request, value: &[u8]) -> bool {
    let stage = req.ini().stage();
    let mut jit = *req.code_mut().unit_cache().tier_setting();
    if stage != IniStage::Startup && !jit.enabled {
        if stage == IniStage::Runtime {
            warning(
                req,
                "Cannot change opcache.jit setting at run-time (JIT is disabled)",
            );
        }
        return false;
    }
    let taken = jit_config(&mut jit, value);
    *req.code_mut().unit_cache().tier_setting() = jit;
    if !taken {
        warning(
            req,
            "Invalid \"opcache.jit\" setting. Should be \"disable\", \"on\", \"off\", \"tracing\", \"function\" or 4-digit number",
        );
    }
    taken
}

/// `OnUpdateJitDebug`, `zend_jit_debug_config`
/// (`Z/ext/opcache/zend_accelerator_module.c:208-218`,
/// `Z/ext/opcache/jit/zend_jit.c:3735-3746`): the value read as a
/// quantity, warning for a shape php reads for backwards compatibility,
/// and once started, refused where it changes a bit of
/// `ZEND_JIT_DEBUG_PERSISTENT`, the profiler's and the debugger's, which
/// php set up at its start; otherwise written to the request's
/// `JIT_G(debug)` ([`UnitCache::tier_setting`](php_ext::request::UnitCache::tier_setting)),
/// which the tier reads where it writes each `TRACE` line.
///
/// ```php
/// var_dump(ini_set('opcache.jit_debug', '16'), ini_get('opcache.jit_debug'));
/// var_dump(ini_set('opcache.jit_debug', '0x3000'), ini_set('opcache.jit_debug', '1x'), ini_get('opcache.jit_debug'));
/// ```
/// ```expect
/// Warning: Some opcache.jit_debug bits cannot be changed after startup in %s on line 2
/// bool(false)
/// string(1) "0"
///
/// Warning: Invalid "opcache.jit_debug" setting. Invalid quantity "1x": unknown multiplier "x", interpreting as "1" for backwards compatibility in %s on line 3
/// string(1) "0"
/// string(6) "0x3000"
/// string(2) "1x"
/// ```
fn on_jit_debug(req: &mut Request, value: &[u8]) -> bool {
    /// `ZEND_JIT_DEBUG_PERSISTENT` (`Z/ext/opcache/jit/zend_jit.h:87`).
    const PERSISTENT: i64 = 0x1f0;
    let new = quantity(req, "opcache.jit_debug", value);
    let stage = req.ini().stage();
    let jit = req.code_mut().unit_cache().tier_setting();
    if stage == IniStage::Startup || (i64::from(jit.debug) ^ new) & PERSISTENT == 0 {
        // `JIT_G(debug)` is a `zend_long`; its low 32 bits are every bit
        // php names.
        jit.debug = u32::try_from(new & 0xFFFF_FFFF).unwrap_or(0);
        return true;
    }
    if stage == IniStage::Runtime {
        warning(
            req,
            "Some opcache.jit_debug bits cannot be changed after startup",
        );
    }
    false
}

/// `ZEND_JIT_LEVEL_OPT_FUNCS`, `ZEND_JIT_LEVEL_OPT_SCRIPT`,
/// `ZEND_JIT_ON_SCRIPT_LOAD`, `ZEND_JIT_ON_HOT_TRACE`,
/// `ZEND_JIT_REG_ALLOC_GLOBAL` and `ZEND_JIT_CPU_AVX`
/// (`Z/ext/opcache/jit/zend_jit.h:36-48`).
const OPT_FUNCS: u8 = 4;
const OPT_SCRIPT: u8 = 5;
const ON_SCRIPT_LOAD: u8 = 0;
const ON_HOT_TRACE: u8 = 5;
const REG_ALLOC_GLOBAL: u8 = 1 << 1;
const CPU_AVX: u8 = 1 << 2;

/// What `zend_jit_config` writes to `jit` for `value` past its stage's
/// test, and whether it took it: a setting it refuses leaves the tier
/// neither enabled nor on, with the digits it passed (`failure:`).
fn jit_config(jit: &mut TierStatus, value: &[u8]) -> bool {
    let set = match value.to_ascii_lowercase().as_slice() {
        b"disable" => Some((false, false)),
        b"" | b"0" | b"off" | b"no" | b"false" => Some((true, false)),
        b"1" | b"on" | b"yes" | b"true" | b"tracing" => {
            (jit.opt_level, jit.kind) = (OPT_FUNCS, ON_HOT_TRACE);
            jit.opt_flags = REG_ALLOC_GLOBAL | CPU_AVX;
            Some((true, true))
        }
        b"function" => {
            (jit.opt_level, jit.kind) = (OPT_SCRIPT, ON_SCRIPT_LOAD);
            jit.opt_flags = REG_ALLOC_GLOBAL | CPU_AVX;
            Some((true, true))
        }
        _ => jit_number(jit, value).map(|on| (true, on)),
    };
    (jit.enabled, jit.on) = set.unwrap_or((false, false));
    set.is_some()
}

/// `ZEND_STRTOL` over the whole setting and `zend_jit_parse_config_num`:
/// zero is off; otherwise each `CRTO` digit from the right in its range,
/// `O` 1 to 5, `T` 0 to 5 but 4, `R` 0 to 2, `C` 0 or 1 for
/// `ZEND_JIT_CPU_AVX`, and nothing past them, each written to `jit` as
/// php passes it. Whether the number turns the tier on; none where php
/// refuses it.
fn jit_number(jit: &mut TierStatus, value: &[u8]) -> Option<bool> {
    let text = std::str::from_utf8(value).ok()?;
    // C's `isspace`, which `ZEND_STRTOL` skips: Rust's ASCII whitespace
    // and the vertical tab.
    let space = |c: char| c.is_ascii_whitespace() || c == '\x0B';
    let mut rest: i64 = (text.trim_start_matches(space)).parse().ok()?;
    if rest < 0 {
        return None;
    }
    if rest == 0 {
        return Some(false);
    }
    let mut digit = || {
        let digit = u8::try_from(rest % 10).unwrap_or(u8::MAX);
        rest /= 10;
        digit
    };
    let opt_level = digit();
    if opt_level == 0 || opt_level > 5 {
        return None;
    }
    jit.opt_level = opt_level;
    let trigger = digit();
    if trigger > 5 || trigger == 4 {
        return None;
    }
    jit.kind = trigger;
    let flags = digit();
    if flags > 2 {
        return None;
    }
    jit.opt_flags = flags;
    let cpu = digit();
    if cpu > 1 {
        return None;
    }
    if cpu == 1 {
        jit.opt_flags |= CPU_AVX;
    }
    (rest == 0).then_some(true)
}

/// Whether `accel_post_startup`, over a cache that started, sets the
/// tier up in the segment it laid down (`zend_jit_startup`,
/// `Z/ext/opcache/ZendAccelerator.c:3252-3254`, `:3321-3331`): the
/// setting the engine started with enabled it, its buffer passes
/// `zend_jit_check_support` ([`jit_buffer_supported`]; the engine holds
/// the check's hooks and opcodes), and [`jit_kept`].
pub(crate) fn jit_starts(req: &mut Request) -> bool {
    jit_enabled(req) && jit_buffer_supported(req) && jit_kept(req)
}

/// `JIT_G(enabled)` as the startup's `opcache.jit` set it
/// ([`jit_config`]): anything but `disable` and a setting php refuses.
fn jit_enabled(req: &mut Request) -> bool {
    let mut jit = TierStatus::default();
    jit_config(&mut jit, &master(req, "opcache.jit")) && jit.enabled
}

/// What a started cache keeps `JIT_G(enabled)` for: a buffer to start
/// in, and a segment, which the file cache alone lays none of.
pub(crate) fn jit_kept(req: &mut Request) -> bool {
    let buffer = Ini::parse_quantity(&master(req, "opcache.jit_buffer_size")).0;
    buffer > 0 && !flag(req, "opcache.file_cache_only")
}

/// `zend_jit_check_support`'s test of the buffer
/// (`Z/ext/opcache/jit/zend_jit.c:3759-3772`): the largest the target's
/// code can reach, 128 MiB on `aarch64` and 2 GiB on `x86_64`.
fn jit_buffer_supported(req: &mut Request) -> bool {
    const LARGEST: i64 = if cfg!(target_arch = "aarch64") {
        128 << 20
    } else {
        2 << 30
    };
    Ini::parse_quantity(&master(req, "opcache.jit_buffer_size")).0 <= LARGEST
}

/// The startup value of opcache's entry `name`, empty where it has none.
fn master(req: &mut Request, name: &str) -> Vec<u8> {
    let Some(me) = req.extensions().named(crate::PRODUCT.as_bytes()) else {
        return Vec::new();
    };
    req.ini()
        .extension_ini(me)
        .into_iter()
        .find(|held| held.decl.name.0.as_ref() == name.as_bytes())
        .and_then(|held| held.master)
        .map(<[u8]>::into_vec)
        .unwrap_or_default()
}

/// Every entry's handler registered on `builder`, in table order.
macro_rules! handlers {
    ($builder:expr; $($at:literal),*) => {{
        let builder = $builder;
        $(let builder = crate::ini::handler::<$at>(builder);)*
        builder
    }};
}
pub(crate) use handlers;

/// Entry `AT`'s handler on `builder`: `OnUpdateLong`, which the engine
/// serves, for a [`Kind::Long`] entry, and [`modify`] for the rest.
pub(crate) fn handler<const AT: usize>(builder: ExtensionBuilder) -> ExtensionBuilder {
    match ENTRIES.get(AT) {
        Some(entry) if entry.kind == Kind::Long => {
            builder.on_update_long(entry.name.as_bytes(), LongHandler::Any)
        }
        Some(entry) => builder.on_modify(entry.name.as_bytes(), modify::<AT>),
        None => builder,
    }
}

/// `ACCEL_LOG_FATAL` to `ACCEL_LOG_DEBUG`, the levels
/// `opcache.log_verbosity_level` admits up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Log {
    /// `ACCEL_LOG_FATAL`, which ends the process whether it is written
    /// or not; the caller refuses to serve.
    Fatal = 0,
    Warning = 2,
    /// `ACCEL_LOG_INFO`, the cache's own events.
    Info = 3,
    Debug = 4,
}

impl Log {
    /// The word `zend_accel_error_va_args` writes before the message.
    fn word(self) -> &'static str {
        match self {
            Log::Fatal => "Fatal Error",
            Log::Warning => "Warning",
            Log::Info => "Message",
            Log::Debug => "Debug",
        }
    }
}

/// `true` where `accepted`, and else `message` logged at
/// `ACCEL_LOG_WARNING` and `false`, php's `FAILURE`.
fn refused_unless(req: &mut Request, accepted: bool, message: &str) -> bool {
    if !accepted {
        accel_log(req, Log::Warning, message);
    }
    accepted
}

/// `zend_accel_error`: where `level` is within
/// `opcache.log_verbosity_level`, one line `<asctime> (<pid>): <Level>
/// <message>` on the error stream, or appended to the file
/// `opcache.error_log` names where it opens.
pub(crate) fn accel_log(req: &mut Request, level: Log, message: &str) {
    if (level as i64) > long(req, "opcache.log_verbosity_level") {
        return;
    }
    let line = format!(
        "{} ({}): {} {message}\n",
        asctime(&Zone::of(req), crate::now()),
        std::process::id(),
        level.word()
    );
    let file = String::from_utf8_lossy(value(req, "opcache.error_log")).into_owned();
    if !file.is_empty() && file != "stderr" {
        let appended = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&file)
            .and_then(|mut log| std::io::Write::write_all(&mut log, line.as_bytes()));
        if appended.is_ok() {
            return;
        }
    }
    req.response_mut().write_stderr(line.as_bytes());
}

/// `asctime(localtime(&at))` less its line ending: `at` in `zone`, the
/// request's local zone as `localtime` reads `TZ` ([`Zone::of`]), as
/// `zend_accel_error` and `accelerator_get_scripts` print it (#2397).
pub(crate) fn asctime(zone: &Zone, at: i64) -> String {
    shape(at.saturating_add(zone.offset(at)))
}

/// `asctime`'s shape of a local time: `Mon Sep 28 16:15:44 2026`, the
/// day padded to two columns with a space.
fn shape(at: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = at.div_euclid(86_400);
    let seconds = at.rem_euclid(86_400);
    // Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = DAYS
        .get(usize::try_from(days.rem_euclid(7)).unwrap_or(0))
        .copied()
        .unwrap_or("Thu");
    let month = MONTHS
        .get(usize::try_from(month - 1).unwrap_or(0))
        .copied()
        .unwrap_or("Jan");
    format!(
        "{weekday} {month} {day:>2} {:02}:{:02}:{:02} {year}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::shape;

    #[test]
    fn asctime_s_shape_is_c_s() {
        assert_eq!(shape(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(shape(1_790_614_522), "Mon Sep 28 16:55:22 2026");
    }
}
