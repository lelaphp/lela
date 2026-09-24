//! `PHP_INI_BEGIN()` of `session.c`: every `session.*` entry with php's
//! default and where it may be changed, and the on-modify rule each
//! runs, `SESSION_CHECK_ACTIVE_STATE` and `SESSION_CHECK_OUTPUT_STATE`
//! first. [`alter`] is `zend_alter_ini_entry` at `PHP_INI_USER` as the
//! natives call it; `ini_set()` reaches the same rules once the face
//! carries an on-modify function for an extension's entry (#721, #744).

use php_ext::coerce::numeric_string;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;

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
pub(super) fn declared() -> Vec<IniDecl> {
    ENTRIES
        .iter()
        .map(|(name, default, kind)| {
            let per_dir = matches!(kind, Kind::PerDir | Kind::PerDirBool);
            IniDecl {
                name: Name(name.as_bytes().into()),
                default: default.as_bytes().into(),
                user: !per_dir,
                per_dir: true,
                system: true,
            }
        })
        .collect()
}

/// `SESSION_CHECK_ACTIVE_STATE` and `SESSION_CHECK_OUTPUT_STATE`: `false`
/// after php's warning while a session is active or the headers are
/// sent.
fn changeable(req: &mut Request) -> bool {
    if req.state::<State>().status == crate::Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session ini settings cannot be changed when a session is active",
        );
        return false;
    }
    if headers_sent(req) {
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

/// The entry's on-modify rule, `true` where it takes the value.
#[allow(clippy::too_many_lines)] // one arm per `PHP_INI_MH` of session.c
fn on_modify(req: &mut Request, name: &[u8], value: &[u8]) -> bool {
    let kind = ENTRIES
        .iter()
        .find(|(n, _, _)| n.as_bytes() == name)
        .map(|(_, _, kind)| *kind);
    match kind {
        None => return true,
        Some(Kind::PerDir | Kind::PerDirBool) => return false,
        Some(_) => {}
    }
    if !changeable(req) {
        return false;
    }
    let deprecated = |req: &mut Request, message: &str| docref(req, Level::Deprecated, message);
    match name {
        b"session.save_path" => !value.contains(&0),
        b"session.name" => {
            let forbidden = value.iter().any(|c| b"=,;.[ \t\r\n\x0b\x0c".contains(c));
            if value.is_empty()
                || value.contains(&0)
                || numeric_string(value).is_some()
                || forbidden
            {
                docref(
                    req,
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
                docref(
                    req,
                    Level::Warning,
                    &format!("Session save handler \"{}\" cannot be found", shown(value)),
                );
                return false;
            };
            let state = req.state::<State>();
            if !state.set_handler && module == Module::User {
                docref(
                    req,
                    Level::Warning,
                    "Session save handler \"user\" cannot be set by ini_set()",
                );
                return false;
            }
            state.default_module = state.module;
            state.module = Some(module);
            true
        }
        b"session.serialize_handler" => {
            let Some(format) = Format::find(value) else {
                docref(
                    req,
                    Level::Warning,
                    &format!("Serialization handler \"{}\" cannot be found", shown(value)),
                );
                return false;
            };
            req.state::<State>().format = Some(format);
            true
        }
        b"session.cookie_lifetime" => {
            if crate::atol(value) < 0 {
                docref(req, Level::Warning, "CookieLifetime cannot be negative");
                return false;
            }
            true
        }
        b"session.sid_length" | b"session.sid_bits_per_character" => {
            let (bounds, usual, what) = if name == b"session.sid_length" {
                (22..=256, 32, "session.sid_length")
            } else {
                (4..=6, 4, "session.sid_bits_per_character")
            };
            if crate::atol(value) != usual {
                deprecated(req, &format!("{what} INI setting is deprecated"));
            }
            if whole_number(value).is_some_and(|n| bounds.contains(&n)) {
                return true;
            }
            docref(
                req,
                Level::Warning,
                &format!(
                    "session.configuration \"{what}\" must be between {} and {}",
                    bounds.start(),
                    bounds.end()
                ),
            );
            false
        }
        b"session.gc_probability" if Request::ini_parse_bytes(value) < 0 => {
            docref(
                req,
                Level::Warning,
                "session.gc_probability must be greater than or equal to 0",
            );
            false
        }
        b"session.gc_divisor" if Request::ini_parse_bytes(value) <= 0 => {
            docref(
                req,
                Level::Warning,
                "session.gc_divisor must be greater than 0",
            );
            false
        }
        b"session.use_only_cookies" => {
            if !Request::ini_parse_bool(value) {
                deprecated(
                    req,
                    "Disabling session.use_only_cookies INI setting is deprecated",
                );
            }
            true
        }
        b"session.use_trans_sid" => {
            if Request::ini_parse_bool(value) {
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

/// `zend_alter_ini_entry(name, value, PHP_INI_USER,
/// PHP_INI_STAGE_RUNTIME)` over a `session.*` entry: the entry's
/// on-modify rule, then the value set; `false` where the entry does not
/// exist or the rule refused it.
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
    if req.ini_get(name).is_none() || !on_modify(req, name, value) {
        return Ok(false);
    }
    Ok(req.ini_set(name, value)?.is_some())
}
