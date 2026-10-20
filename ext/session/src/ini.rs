//! `PHP_INI_BEGIN()` of `session.c`: every `session.*` entry with php's
//! default and where it may be changed, and the on-modify rule each
//! runs, `SESSION_CHECK_ACTIVE_STATE` and `SESSION_CHECK_OUTPUT_STATE`
//! first. Each rule is the entry's `on_modify` on the face, so
//! `ini_set()` and the natives, which call [`alter`] where php calls
//! `zend_alter_ini_entry`, meet the same rule; the engine runs them
//! over the values the host started the engine with (#744).

use php_ext::abi::{ExtensionBuilder, IniStage, OnModify};
use php_ext::coerce::{atoi, numeric_string, strtol};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{self, Level, Thrown, docref};
use php_ext::request::{Ini, Request};
use php_ext::values::{Owner, PhpString};

use crate::{Format, Module, State, headers_sent, sent_error, shown, started_error};

/// How an entry is stored, what `MINFO` shows it as.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Other,
    /// `ZEND_INI_PERDIR`: never changed at `PHP_INI_USER`.
    PerDir,
    PerDirBool,
}

/// The entries in `PHP_INI_BEGIN()`'s order, with their defaults.
pub(crate) const ENTRIES: [(&str, &str, Kind); 31] = [
    ("session.save_path", "", Kind::Other),
    ("session.name", "PHPSESSID", Kind::Other),
    ("session.save_handler", "files", Kind::Other),
    ("session.auto_start", "0", Kind::PerDirBool),
    ("session.gc_probability", "1", Kind::Other),
    ("session.gc_divisor", "100", Kind::Other),
    ("session.gc_maxlifetime", "1440", Kind::Other),
    ("session.serialize_handler", "php", Kind::Other),
    ("session.cookie_lifetime", "0", Kind::Other),
    ("session.cookie_path", "/", Kind::Other),
    ("session.cookie_domain", "", Kind::Other),
    ("session.cookie_secure", "0", Kind::Bool),
    ("session.cookie_partitioned", "0", Kind::Bool),
    ("session.cookie_httponly", "0", Kind::Bool),
    ("session.cookie_samesite", "", Kind::Other),
    ("session.use_cookies", "1", Kind::Bool),
    ("session.use_only_cookies", "1", Kind::Bool),
    ("session.use_strict_mode", "0", Kind::Bool),
    ("session.referer_check", "", Kind::Other),
    ("session.cache_limiter", "nocache", Kind::Other),
    ("session.cache_expire", "180", Kind::Other),
    ("session.use_trans_sid", "0", Kind::Bool),
    ("session.sid_length", "32", Kind::Other),
    ("session.sid_bits_per_character", "4", Kind::Other),
    ("session.lazy_write", "1", Kind::Bool),
    ("session.upload_progress.enabled", "1", Kind::PerDirBool),
    ("session.upload_progress.cleanup", "1", Kind::PerDirBool),
    (
        "session.upload_progress.prefix",
        "upload_progress_",
        Kind::PerDir,
    ),
    (
        "session.upload_progress.name",
        "PHP_SESSION_UPLOAD_PROGRESS",
        Kind::PerDir,
    ),
    ("session.upload_progress.freq", "1%", Kind::PerDir),
    ("session.upload_progress.min_freq", "1", Kind::PerDir),
];

/// The entries as the extension declares them.
///
/// ```php
/// foreach (["session.save_handler", "session.name", "session.serialize_handler", "session.gc_maxlifetime", "session.cookie_path", "session.use_only_cookies", "session.sid_length", "session.cache_limiter", "session.upload_progress.freq"] as $e) var_dump(ini_get($e));
/// ```
/// ```expect
/// string(5) "files"
/// string(9) "PHPSESSID"
/// string(3) "php"
/// string(4) "1440"
/// string(1) "/"
/// string(1) "1"
/// string(2) "32"
/// string(7) "nocache"
/// string(2) "1%"
/// ```
///
/// Where each may be changed, `ZEND_INI_PERDIR` alone for `auto_start`
/// and the upload progress entries and `PHP_INI_ALL` for the rest:
///
/// ```php
/// $all = ini_get_all('session');
/// var_dump($all['session.auto_start']['access'], $all['session.upload_progress.freq']['access'], $all['session.name']['access']);
/// ```
/// ```expect
/// int(2)
/// int(2)
/// int(7)
/// ```
pub(super) fn declared() -> Vec<IniDecl> {
    ENTRIES
        .iter()
        .map(|(name, default, kind)| {
            let per_dir = matches!(kind, Kind::PerDir | Kind::PerDirBool);
            IniDecl {
                name: Name(name.as_bytes().into()),
                default: Some(default.as_bytes().into()),
                user: !per_dir,
                per_dir: true,
                // `ZEND_INI_PERDIR` alone, which `php --re` shows as
                // `<PERDIR>`; every other entry is `PHP_INI_ALL`.
                system: !per_dir,
            }
        })
        .collect()
}

/// `SESSION_CHECK_ACTIVE_STATE` and `SESSION_CHECK_OUTPUT_STATE`: `false`
/// after php's warning while a session is active, or while the headers
/// are sent at any stage but the request's end, where the entries are
/// restored after the response went out. An entry refused here is still
/// one the request changed, so its end hands it back the value it
/// started with:
///
/// ```php
/// echo "sent\n"; var_dump(ini_set('session.use_only_cookies', '1'));
/// ```
/// ```expect regime=session-startup
/// Deprecated: PHP Startup: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0
///
/// Deprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0
/// sent
///
/// Warning: ini_set(): Session ini settings cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 2
/// bool(false)
///
/// Deprecated: PHP Request Shutdown: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0
/// ```
fn changeable(req: &mut Request, stage: IniStage) -> bool {
    if req.state::<State>().status == crate::Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session ini settings cannot be changed when a session is active",
        );
        return false;
    }
    if stage != IniStage::Deactivate && headers_sent(req) {
        sent_error(
            req,
            "Session ini settings cannot be changed after headers have already been sent",
        );
        return false;
    }
    true
}

/// `ZEND_STRTOL(value, &endptr, 10)` with `*endptr == '\0'`: the whole
/// value a decimal number.
fn whole_number(value: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(value).ok()?;
    let digits = text.trim_start();
    let body = digits.strip_prefix(['+', '-']).unwrap_or(digits);
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `php_error_docref` at the stage: behind `PHP Startup` in `Unknown`
/// on line 0 as the engine starts, which is `php_verror`'s origin while
/// `php_during_module_startup()`, and behind the origin the request
/// names otherwise, the active native's name while it runs and `PHP
/// Request Shutdown` as it ends.
fn raise(req: &mut Request, stage: IniStage, level: Level, message: &str) {
    match stage {
        IniStage::Startup => raising::startup(req, level, &format!("PHP Startup: {message}")),
        IniStage::Activate | IniStage::Runtime | IniStage::Deactivate => {
            docref(req, level, message);
        }
    }
}

/// php's `err_type` for a save handler or serializer refused:
/// `E_WARNING` at runtime, `E_ERROR` at any other stage.
fn refusal(stage: IniStage) -> Level {
    if stage == IniStage::Runtime {
        Level::Warning
    } else {
        Level::Error
    }
}

/// A save handler or serializer that is not found: taken without a
/// word before any request has activated the extensions
/// (`PG(modules_activated)`), refused without one as the request ends
/// (php's comment: restoring says nothing), and refused
/// after `message` otherwise.
fn not_found(req: &mut Request, stage: IniStage, message: &str) -> bool {
    match stage {
        IniStage::Startup | IniStage::Activate => true,
        IniStage::Deactivate => false,
        IniStage::Runtime => {
            docref(req, refusal(stage), message);
            false
        }
    }
}

/// The entry's on-modify rule at `stage`, `true` where it takes the
/// value. At startup no session is active and no header is sent, and
/// `PG(modules_activated)` is not yet set, so an unknown save handler or
/// serializer is taken without a word there, and the session is disabled
/// when a request starts under it (`PHP_RINIT_FUNCTION(session)`). As
/// the request ends its session is closed, and the headers it sent do
/// not hold an entry back from the value it started with.
///
/// What a rule raises at startup is displayed before any request, in the
/// order the entries are declared, and sends no header, so the session
/// natives still take their values:
///
/// ```php
/// var_dump(headers_sent(), session_save_path("/tmp"), session_save_path());
/// ```
/// ```expect regime=session-startup
/// Deprecated: PHP Startup: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0
///
/// Deprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0
/// bool(false)
/// string(0) ""
/// string(4) "/tmp"
/// ```
///
/// The whole-number entries are read by the quantity rule, with php's
/// warning where it interprets a value, at startup, at `ini_set()` and as
/// the request ends: `OnUpdateSessionLong`'s `OnUpdateLong`,
/// `OnUpdateCookieLifetime`'s `OnUpdateLongGEZero`, and the bound
/// `OnUpdateSessionGcProbability` holds the quantity to.
///
/// ```php
/// $code = 'ob_start(); var_dump(ini_get("session.gc_maxlifetime"), session_cache_expire(), ini_set("session.cache_expire", "1K"), session_cache_expire(), ini_set("session.gc_probability", "-1x"), ini_set("session.cookie_lifetime", "-0x2"), session_get_cookie_params()["lifetime"]); ob_end_flush();';
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'session.gc_maxlifetime=1K', '-d', 'session.cache_expire=0x10q', '-r', $code], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
/// ```
/// ```expect process
/// Warning: Invalid "session.cache_expire" setting. Invalid quantity "0x10q": unknown multiplier "q", interpreting as "0x10" for backwards compatibility in Unknown on line 0
///
/// Warning: Invalid "session.gc_probability" setting. Invalid quantity "-1x": unknown multiplier "x", interpreting as "-1" for backwards compatibility in Command line code on line 1
///
/// Warning: ini_set(): session.gc_probability must be greater than or equal to 0 in Command line code on line 1
/// string(2) "1K"
/// int(16)
/// string(5) "0x10q"
/// int(1024)
/// bool(false)
/// bool(false)
/// int(0)
///
/// Warning: Invalid "session.cache_expire" setting. Invalid quantity "0x10q": unknown multiplier "q", interpreting as "0x10" for backwards compatibility in Unknown on line 0
/// 0
/// ```
#[allow(clippy::too_many_lines)] // one arm per `PHP_INI_MH` of session.c
fn on_modify(req: &mut Request, name: &[u8], value: &[u8], stage: IniStage) -> bool {
    let kind = ENTRIES
        .iter()
        .find(|(n, _, _)| n.as_bytes() == name)
        .map(|(_, _, kind)| *kind);
    match kind {
        None => return true,
        Some(Kind::PerDir | Kind::PerDirBool) => return per_dir(req, name, value, stage),
        Some(_) => {}
    }
    if stage != IniStage::Startup && !changeable(req, stage) {
        return false;
    }
    let deprecated =
        |req: &mut Request, message: &str| raise(req, stage, Level::Deprecated, message);
    match name {
        b"session.save_path" => {
            if value.contains(&0) {
                return false;
            }
            // `OnUpdateSaveDir`: at runtime the directory, after the
            // `N;` and `MODE;` it may begin with, is held to the guard.
            let dir = match value.iter().position(|b| *b == b';') {
                Some(first) => {
                    let rest = &value[first + 1..];
                    match rest.iter().position(|b| *b == b';') {
                        Some(second) => &rest[second + 1..],
                        None => rest,
                    }
                }
                None => value,
            };
            stage != IniStage::Runtime
                || dir.is_empty()
                || req.check_open_basedir(dir, true).is_ok()
        }
        b"session.name" => {
            let forbidden = value.iter().any(|c| b"=,;.[ \t\r\n\x0b\x0c".contains(c));
            if value.is_empty()
                || value.contains(&0)
                || numeric_string(value).is_some()
                || forbidden
            {
                // "Do not output error when restoring ini options."
                if stage == IniStage::Deactivate {
                    return false;
                }
                raise(
                    req,
                    stage,
                    Level::Warning,
                    &format!(
                        "session.name \"{}\" must not be numeric, empty, contain null bytes or any of the following characters \"=,;.[ \\t\\r\\n\\013\\014\"",
                        shown(value)
                    ),
                );
                return false;
            }
            true
        }
        b"session.save_handler" => {
            let Some(module) = Module::find(value) else {
                let message = format!("Session save handler \"{}\" cannot be found", shown(value));
                return not_found(req, stage, &message);
            };
            let session = req.state::<State>();
            if !session.set_handler && module == Module::User {
                raise(
                    req,
                    stage,
                    refusal(stage),
                    "Session save handler \"user\" cannot be set by ini_set()",
                );
                return false;
            }
            session.default_module = session.module;
            session.module = Some(module);
            true
        }
        b"session.serialize_handler" => {
            let Some(format) = Format::find(value) else {
                let message = format!("Serialization handler \"{}\" cannot be found", shown(value));
                return not_found(req, stage, &message);
            };
            req.state::<State>().format = Some(format);
            true
        }
        // `OnUpdateCookieLifetime`: `atol`'s negative refused with its
        // warning, then `OnUpdateLongGEZero`'s quantity.
        b"session.cookie_lifetime" => {
            if strtol(value) < 0 {
                raise(
                    req,
                    stage,
                    Level::Warning,
                    "CookieLifetime cannot be negative",
                );
                return false;
            }
            req.ini_mut().parse_quantity_warn(name, value) >= 0
        }
        // `OnUpdateSessionLong`: `OnUpdateLong` once the checks above hold.
        b"session.gc_maxlifetime" | b"session.cache_expire" => {
            req.ini_mut().parse_quantity_warn(name, value);
            true
        }
        b"session.sid_length" | b"session.sid_bits_per_character" => {
            let (bounds, usual, what) = if name == b"session.sid_length" {
                (22..=256, 32, "session.sid_length")
            } else {
                (4..=6, 4, "session.sid_bits_per_character")
            };
            if strtol(value) != usual {
                deprecated(req, &format!("{what} INI setting is deprecated"));
            }
            if whole_number(value).is_some_and(|n| bounds.contains(&n)) {
                return true;
            }
            raise(
                req,
                stage,
                Level::Warning,
                &format!(
                    "session.configuration \"{what}\" must be between {} and {}",
                    bounds.start(),
                    bounds.end()
                ),
            );
            false
        }
        // `OnUpdateSessionGcProbability` and `OnUpdateSessionDivisor`:
        // the quantity, then the bound.
        b"session.gc_probability" => {
            if req.ini_mut().parse_quantity_warn(name, value) >= 0 {
                return true;
            }
            raise(
                req,
                stage,
                Level::Warning,
                "session.gc_probability must be greater than or equal to 0",
            );
            false
        }
        b"session.gc_divisor" => {
            if req.ini_mut().parse_quantity_warn(name, value) > 0 {
                return true;
            }
            raise(
                req,
                stage,
                Level::Warning,
                "session.gc_divisor must be greater than 0",
            );
            false
        }
        b"session.use_only_cookies" => {
            if !Ini::parse_bool(value) {
                deprecated(
                    req,
                    "Disabling session.use_only_cookies INI setting is deprecated",
                );
            }
            true
        }
        b"session.use_trans_sid" => {
            if Ini::parse_bool(value) {
                deprecated(
                    req,
                    "Enabling session.use_trans_sid INI setting is deprecated",
                );
            }
            true
        }
        b"session.referer_check" => {
            if !value.is_empty() {
                deprecated(
                    req,
                    "Usage of session.referer_check INI setting is deprecated",
                );
            }
            true
        }
        _ => true,
    }
}

/// The rules of the entries `ZEND_INI_PERDIR` leaves no user to
/// change, which only the host's values meet: `OnUpdateRfc1867Freq`'s,
/// and `OnUpdateBool` and `OnUpdateStr`, which take anything.
fn per_dir(req: &mut Request, name: &[u8], value: &[u8], stage: IniStage) -> bool {
    if name != b"session.upload_progress.freq" {
        return true;
    }
    // `int tmp = ZEND_ATOL(...)`: C's `atoi`.
    let percent = i64::from(atoi(value));
    if percent < 0 {
        raise(
            req,
            stage,
            Level::Warning,
            "session.upload_progress.freq must be greater than or equal to 0",
        );
        return false;
    }
    if value.last() == Some(&b'%') && percent > 100 {
        raise(
            req,
            stage,
            Level::Warning,
            "session.upload_progress.freq must be less than or equal to 100%",
        );
        return false;
    }
    true
}

/// Entry `AT` of [`ENTRIES`]'s rule at runtime, what the face serves
/// from `ini_set()` and [`alter`]; one function per entry, since an
/// `on_modify` is told the value and not the name.
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature, which may throw
fn modify<const AT: usize>(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    let Some((name, _, _)) = ENTRIES.get(AT) else {
        return Ok(true);
    };
    let stage = req.ini().stage();
    Ok(on_modify(req, name.as_bytes(), value, stage))
}

/// One [`modify`] per entry, in [`ENTRIES`]' order.
macro_rules! modifiers {
    ($($at:literal)*) => { [$(modify::<$at> as OnModify),*] };
}
const MODIFIERS: [OnModify; 31] =
    modifiers!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30);

/// Every entry's rule on the extension, as `PHP_INI_BEGIN()` pairs each
/// with its `PHP_INI_MH`. The `ZEND_INI_PERDIR` ones are refused before
/// any rule by `ini_set()`, and their rules are met only as the engine
/// starts, over the host's values.
///
/// ```php
/// ob_start(); var_dump(ini_set('session.name', '123'), ini_set('session.gc_divisor', '0'), ini_get('session.gc_divisor'), ini_set('session.save_handler', 'user'));
/// var_dump(ini_set('session.auto_start', '1'), ini_set('session.upload_progress.freq', '5'));
/// session_save_path(__DIR__); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// var_dump(ini_set('session.serialize_handler', 'php'), ini_set('session.use_only_cookies', '0')); session_write_close();
/// var_dump(ini_set('session.serialize_handler', 'nope'), ini_set('session.serialize_handler', 'php_serialize'), ini_set('session.use_only_cookies', '0'));
/// ```
/// ```expect
///
/// Warning: ini_set(): session.name "123" must not be numeric, empty, contain null bytes or any of the following characters "=,;.[ \t\r\n\013\014" in %s on line 2
///
/// Warning: ini_set(): session.gc_divisor must be greater than 0 in %s on line 2
///
/// Warning: ini_set(): Session save handler "user" cannot be set by ini_set() in %s on line 2
/// bool(false)
/// bool(false)
/// string(3) "100"
/// bool(false)
/// bool(false)
/// bool(false)
///
/// Warning: ini_set(): Session ini settings cannot be changed when a session is active (started from %s on line 4) in %s on line 5
///
/// Warning: ini_set(): Session ini settings cannot be changed when a session is active (started from %s on line 4) in %s on line 5
/// bool(false)
/// bool(false)
///
/// Warning: ini_set(): Serialization handler "nope" cannot be found in %s on line 6
///
/// Deprecated: ini_set(): Disabling session.use_only_cookies INI setting is deprecated in %s on line 6
/// bool(false)
/// string(3) "php"
/// string(1) "1"
/// ```
pub(super) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    ENTRIES
        .iter()
        .zip(MODIFIERS)
        .fold(builder, |builder, ((name, _, _), modify)| {
            builder.on_modify(name.as_bytes(), modify)
        })
}

/// `zend_alter_ini_entry(name, value, PHP_INI_USER,
/// PHP_INI_STAGE_RUNTIME)` over a `session.*` entry: the value set
/// through the face, which serves the entry's rule first; `false` where
/// the entry does not exist, may not change here, or the rule refused
/// it.
///
/// ```php
/// ob_start();
/// var_dump(session_name("x.y"), session_name("123"), session_name(""), session_name("ok"), session_name());
/// var_dump(session_start(["serialize_handler" => "nope", "sid_length" => "10", "gc_divisor" => "0", "no_such" => "1"]));
/// session_write_close();
/// ob_end_flush();
/// ```
/// ```expect
/// Warning: session_name(): session.name "x.y" must not be numeric, empty, contain null bytes or any of the following characters "=,;.[ \t\r\n\013\014" in %s on line 3
///
/// Warning: session_name(): session.name "123" must not be numeric, empty, contain null bytes or any of the following characters "=,;.[ \t\r\n\013\014" in %s on line 3
///
/// Warning: session_name(): session.name "" must not be numeric, empty, contain null bytes or any of the following characters "=,;.[ \t\r\n\013\014" in %s on line 3
/// string(9) "PHPSESSID"
/// string(9) "PHPSESSID"
/// string(9) "PHPSESSID"
/// string(9) "PHPSESSID"
/// string(2) "ok"
///
/// Warning: session_start(): Serialization handler "nope" cannot be found in %s on line 4
///
/// Warning: session_start(): Setting option "serialize_handler" failed in %s on line 4
///
/// Deprecated: session_start(): session.sid_length INI setting is deprecated in %s on line 4
///
/// Warning: session_start(): session.configuration "session.sid_length" must be between 22 and 256 in %s on line 4
///
/// Warning: session_start(): Setting option "sid_length" failed in %s on line 4
///
/// Warning: session_start(): session.gc_divisor must be greater than 0 in %s on line 4
///
/// Warning: session_start(): Setting option "gc_divisor" failed in %s on line 4
///
/// Warning: session_start(): Setting option "no_such" failed in %s on line 4
/// bool(true)
/// ```
pub(super) fn alter(req: &mut Request, name: &[u8], value: &[u8]) -> Result<bool, Thrown> {
    Ok(req.ini_mut().set(name, value)?.is_some())
}

/// [`alter`] over a request string the caller holds,
/// `zend_alter_ini_entry`'s: the entry that takes it is one more owner,
/// and the caller's own owner stays the caller's.
pub(super) fn alter_string(
    req: &mut Request,
    name: &[u8],
    value: PhpString,
) -> Result<bool, Thrown> {
    Ok(req.ini_mut().set_string(name, value)?.is_some())
}

/// [`alter_string`] over a string made for it and released after,
/// `zend_long_to_str`'s or `zval_get_string`'s.
pub(super) fn alter_made(
    req: &mut Request,
    name: &[u8],
    value: Owner<PhpString>,
) -> Result<bool, Thrown> {
    let set = req.ini_mut().set_string(name, *value);
    value.give_up(req);
    Ok(set?.is_some())
}
