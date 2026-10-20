//! The `session` extension: `php-src/ext/session` as one crate under
//! `ext/` (ADR-0004). Beside the facade, one dependency, ext/standard, for
//! the serializer `session.c` includes of it; one entry, `extension()`;
//! the session itself request state (ADR-0005). The phpt subset is
//! `php-src/ext/session/tests`, through `xtask phpt --subset`.
//!
//! `session.c` is copied as php writes it: `PS(session_status)` and the
//! state machine over it (`session`), the natives and their warnings
//! (`natives`), the three serializers over ext/standard's var hash
//! (`serializers`), the `files` save handler of `mod_files.c` over the
//! streams facet (`files`), the `user` handler of `mod_user.c` and the
//! `SessionHandler` class of `mod_user_class.c` (`user`), and the on-modify
//! rules of every `session.*` entry (`ini`). The session's variables are
//! the `$_SESSION` superglobal's array, which the natives read and write;
//! a program reads it once the compiler fetches a superglobal (#633, #745).
//! The process's real and effective uids, which the files handler
//! reads, are the engine's (`php_ext::process`). Upload progress is the engine's upload hook
//! (`progress`). The mm handler is not ported: it is not in php's
//! default build.
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

mod files;
mod ini;
mod natives;
mod progress;
mod serializers;
mod session;
mod user;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder, IntoReturn};
use php_ext::attr::php_const;
use php_ext::coerce::c_str;
use php_ext::raising::{Cause, Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpRef, PhpString, Value};

use crate::natives::{
    session_abort, session_cache_expire, session_cache_limiter, session_commit, session_create_id,
    session_decode, session_destroy, session_encode, session_gc, session_get_cookie_params,
    session_id, session_module_name, session_name, session_regenerate_id,
    session_register_shutdown, session_reset, session_save_path, session_set_cookie_params,
    session_set_save_handler, session_start, session_status, session_unset, session_write_close,
};
use crate::user::{
    SessionHandler, SessionHandlerInterface, SessionIdInterface,
    SessionUpdateTimestampHandlerInterface,
};

/// `php_session_status`: the three values `session_status()` answers,
/// each the `PHP_SESSION_*` constant of its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i64)]
enum Status {
    Disabled = constants::PHP_SESSION_DISABLED,
    #[default]
    None = constants::PHP_SESSION_NONE,
    Active = constants::PHP_SESSION_ACTIVE,
}

/// The stub's three constants.
#[php_const]
mod constants {
    pub(crate) const PHP_SESSION_DISABLED: i64 = 0;
    pub(crate) const PHP_SESSION_NONE: i64 = 1;
    pub(crate) const PHP_SESSION_ACTIVE: i64 = 2;
}

/// A save handler, `ps_module`: the two `ps_modules` registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Module {
    Files,
    User,
}

impl Module {
    /// `s_name`.
    const fn name(self) -> &'static str {
        match self {
            Module::Files => "files",
            Module::User => "user",
        }
    }

    /// `_php_find_ps_module`: by name, without case.
    fn find(name: &[u8]) -> Option<Module> {
        [Module::Files, Module::User]
            .into_iter()
            .find(|m| m.name().as_bytes().eq_ignore_ascii_case(c_str(name)))
    }
}

/// A serializer, `ps_serializer`: the three `ps_serializers` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    PhpSerialize,
    Php,
    PhpBinary,
}

impl Format {
    /// `_php_find_ps_serializer`: by name, without case.
    fn find(name: &[u8]) -> Option<Format> {
        [
            (Format::PhpSerialize, "php_serialize"),
            (Format::Php, "php"),
            (Format::PhpBinary, "php_binary"),
        ]
        .into_iter()
        .find(|(_, n)| n.as_bytes().eq_ignore_ascii_case(c_str(name)))
        .map(|(f, _)| f)
    }
}

/// `php_ps_globals` as one request's: built at request start, dropped
/// after `rshutdown`, private to this crate (ADR-0005). The handles it
/// holds, the open file and the user handler's callables, are the
/// request's and released by the request.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // `php_ps_globals`'s flags, one each
struct State {
    status: Status,
    /// `PS(id)`.
    id: Option<Vec<u8>>,
    /// `PS(mod)`, and `PS(default_mod)`, the handler `SessionHandler`
    /// calls.
    module: Option<Module>,
    default_module: Option<Module>,
    /// `PS(serializer)`.
    format: Option<Format>,
    /// `PS(mod_data)`: the files handler's open state.
    files: Option<files::Files>,
    /// `PS(mod_user_implemented)`, the user handler opened, and
    /// `PS(mod_user_is_open)`, `SessionHandler`'s parent opened.
    user_open: bool,
    parent_open: bool,
    /// `PS(mod_user_names)` and `PS(mod_user_class_name)`.
    handlers: user::Handlers,
    class_name: Option<Vec<u8>>,
    in_save_handler: bool,
    set_handler: bool,
    define_sid: bool,
    send_cookie: bool,
    /// `PS(session_vars)`: the data read, for `lazy_write`.
    session_vars: Option<Vec<u8>>,
    /// `PS(http_session_vars)`: the reference `$_SESSION` in
    /// `EG(symbol_table)` is bound to, one owner of it the session's, so
    /// what the program assigns through `$_SESSION` is what the session
    /// writes, and what it unsets from the table the session still holds.
    vars: Option<Owner<PhpRef>>,
    /// `PS(session_started_filename)` and its line.
    started_at: Option<(Vec<u8>, u32)>,
    /// `EG(exception)` as a user handler left it: php goes on after a
    /// handler throws, and the native raises it when it returns.
    thrown: Option<Thrown>,
    /// `PS(rfc1867_progress)`: an upload's progress while its body is read.
    progress: Option<progress::Progress>,
    /// The strings `session_set_cookie_params` made for a key its array
    /// gave twice and overwrote with the second, which php's leaves held
    /// to the request's end (`memory_get_usage()` reads them), given up
    /// at `rshutdown` with the rest of the request's.
    overwritten: Vec<Owner<PhpString>>,
}

/// The extension value (design §7.1): the natives `session.stub.php`
/// declares, its three interfaces and `SessionHandler`, the three
/// constants, the `session.*` ini entries, the request hooks and the
/// `deps` table.
///
/// ```php
/// var_dump(extension_loaded('session'), PHP_SESSION_DISABLED, PHP_SESSION_NONE, PHP_SESSION_ACTIVE, session_status(), (new ReflectionExtension('session'))->getDependencies());
/// echo new ReflectionFunction('session_set_save_handler');
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// int(1)
/// int(2)
/// int(1)
/// array(1) {
///   ["spl"]=>
///   string(8) "Optional"
/// }
/// Function [ <internal:session> function session_set_save_handler ] {
///
///   - Parameters [9] {
///     Parameter #0 [ <required> $open ]
///     Parameter #1 [ <optional> $close = <default> ]
///     Parameter #2 [ <optional> callable $read = <default> ]
///     Parameter #3 [ <optional> callable $write = <default> ]
///     Parameter #4 [ <optional> callable $destroy = <default> ]
///     Parameter #5 [ <optional> callable $gc = <default> ]
///     Parameter #6 [ <optional> ?callable $create_sid = null ]
///     Parameter #7 [ <optional> ?callable $validate_sid = null ]
///     Parameter #8 [ <optional> ?callable $update_timestamp = null ]
///   }
///   - Return [ bool ]
/// }
/// ```
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .constants(constants::declared())
        .functions(vec![
            session_name::declared(),
            session_module_name::declared(),
            session_save_path::declared(),
            session_id::declared(),
            session_create_id::declared(),
            session_regenerate_id::declared(),
            session_decode::declared(),
            session_encode::declared(),
            session_destroy::declared(),
            session_unset::declared(),
            session_gc::declared(),
            session_get_cookie_params::declared(),
            session_write_close::declared(),
            session_abort::declared(),
            session_reset::declared(),
            session_status::declared(),
            session_register_shutdown::declared(),
            session_commit::declared(),
            session_set_save_handler::declared(),
            session_cache_limiter::declared(),
            session_cache_expire::declared(),
            session_set_cookie_params::declared(),
            session_start::declared(),
        ])
        .ini(ini::declared())
        .dependencies(vec![Dependency {
            name: String::from("spl"),
            kind: DependencyKind::Optional,
            rel: None,
            version: None,
        }])
        .class::<SessionHandlerInterface>()
        .class::<SessionIdInterface>()
        .class::<SessionUpdateTimestampHandlerInterface>()
        .class::<SessionHandler>();
    ini::register(builder)
        .request_state::<State>()
        .rinit(session::rinit)
        .rshutdown(session::rshutdown)
        .upload(progress::hook)
        .info(session_info)
        .build("session")
}

/// `PHP_MINFO_FUNCTION(session)`: the three rows, then the ini entries,
/// a boolean one `On` or `Off` as `display_on_off` shows it.
///
/// ```php
/// ob_start(); (new ReflectionExtension('session'))->info(); echo str_replace(" \n", "\n", ob_get_clean());
/// ```
/// ```expect
/// session
///
/// Session Support => enabled
/// Registered save handlers => files user
/// Registered serializer handlers => php_serialize php php_binary
///
/// Directive => Local Value => Master Value
/// session.save_path => no value => no value
/// session.name => PHPSESSID => PHPSESSID
/// session.save_handler => files => files
/// session.auto_start => Off => Off
/// session.gc_probability => 1 => 1
/// session.gc_divisor => 100 => 100
/// session.gc_maxlifetime => 1440 => 1440
/// session.serialize_handler => php => php
/// session.cookie_lifetime => 0 => 0
/// session.cookie_path => / => /
/// session.cookie_domain => no value => no value
/// session.cookie_secure => Off => Off
/// session.cookie_partitioned => Off => Off
/// session.cookie_httponly => Off => Off
/// session.cookie_samesite => no value => no value
/// session.use_cookies => On => On
/// session.use_only_cookies => On => On
/// session.use_strict_mode => Off => Off
/// session.referer_check => no value => no value
/// session.cache_limiter => nocache => nocache
/// session.cache_expire => 180 => 180
/// session.use_trans_sid => Off => Off
/// session.sid_length => 32 => 32
/// session.sid_bits_per_character => 4 => 4
/// session.lazy_write => On => On
/// session.upload_progress.enabled => On => On
/// session.upload_progress.cleanup => On => On
/// session.upload_progress.prefix => upload_progress_ => upload_progress_
/// session.upload_progress.name => PHP_SESSION_UPLOAD_PROGRESS => PHP_SESSION_UPLOAD_PROGRESS
/// session.upload_progress.freq => 1% => 1%
/// session.upload_progress.min_freq => 1 => 1
/// ```
fn session_info(req: &mut Request) {
    req.extensions_mut().info_table(&[
        &["Session Support", "enabled"],
        &["Registered save handlers", "files user "],
        &[
            "Registered serializer handlers",
            "php_serialize php php_binary ",
        ],
    ]);
    let rows: Vec<[String; 3]> = ini::ENTRIES
        .iter()
        .map(|(name, default, kind)| {
            let shown = |value: &[u8]| match kind {
                ini::Kind::Bool | ini::Kind::PerDirBool if Request::ini_parse_bool(value) => {
                    String::from("On")
                }
                ini::Kind::Bool | ini::Kind::PerDirBool => String::from("Off"),
                _ => String::from_utf8_lossy(value).into_owned(),
            };
            let local = req
                .ini_get(name.as_bytes())
                .map_or_else(|| shown(default.as_bytes()), shown);
            [(*name).to_owned(), local, shown(default.as_bytes())]
        })
        .collect();
    let rows: Vec<[&str; 3]> = rows
        .iter()
        .map(|[n, l, m]| [n.as_str(), l.as_str(), m.as_str()])
        .collect();
    req.info_ini_table(&rows);
}

// ------------------------------------------------------------ helpers

/// The bytes as C's `%s` prints them.
fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(c_str(bytes)).into_owned()
}

/// A new string of the request's as a value.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// An ini entry's text, the engine default where the request set none.
fn ini(req: &Request, name: &[u8]) -> Vec<u8> {
    req.ini_get(name).map(<[u8]>::to_vec).unwrap_or_default()
}

/// A boolean entry as `OnUpdateBool` stores it.
fn ini_bool(req: &Request, name: &[u8]) -> bool {
    Request::ini_parse_bool(&ini(req, name))
}

/// `php_session_session_already_started_error`: the message, then where
/// the session was started.
fn started_error(req: &mut Request, level: Level, message: &str) {
    let started = req.state::<State>().started_at.clone();
    let text = match started {
        Some((file, line)) => format!("{message} (started from {} on line {line})", shown(&file)),
        None if ini_bool(req, b"session.auto_start") => {
            format!("{message} (session started automatically)")
        }
        None => message.to_owned(),
    };
    docref(req, level, &text);
}

/// `php_session_headers_already_sent_error`: the message, then where the
/// body started.
fn sent_error(req: &mut Request, message: &str) {
    let text = match req.output_started_at() {
        Some((file, line)) if !file.is_empty() => {
            format!("{message} (sent from {} on line {line})", shown(&file))
        }
        _ => message.to_owned(),
    };
    docref(req, Level::Warning, &text);
}

/// `SG(headers_sent)`.
fn headers_sent(req: &Request) -> bool {
    req.headers_sent().is_some()
}

/// The session's array, `Z_REFVAL(PS(http_session_vars))`, where
/// `IF_SESSION_VARS()` finds one: a reference bound and an array in it.
/// A view: no owner taken.
fn session_array(req: &mut Request) -> Option<PhpArray> {
    req.state::<State>()
        .vars
        .as_deref()
        .and_then(|vars| vars.get().as_array().copied())
}

/// The session's array made writable in place, `SEPARATE_ARRAY` over
/// `Z_REFVAL(PS(http_session_vars))`, what `php_set_session_var` and
/// `session_unset` write through.
fn session_array_mut(req: &mut Request) -> Result<Option<PhpArray>, Thrown> {
    match req.state::<State>().vars.as_deref().copied() {
        Some(vars) => vars.separate_array(req),
        None => Ok(None),
    }
}

/// `vars` bound as the session's variables, which own it from here:
/// `PS(http_session_vars)` a new reference to it with a second owner,
/// laid into `EG(symbol_table)` as `$_SESSION` by
/// `zend_hash_update_ind`, and the reference held before given up. An
/// array but for what `php_serialize` decodes, which binds whatever the
/// data held.
fn bind_vars(req: &mut Request, vars: Owner<Value>) -> Result<(), Thrown> {
    if let Some(old) = req.state::<State>().vars.take() {
        old.give_up(req);
    }
    let vars = PhpRef::new(req, vars)?;
    let kept = Owner::take(req, *vars);
    req.state::<State>().vars = Some(kept);
    req.variables_mut()
        .set_global(b"_SESSION", Some(vars.into_value()));
    Ok(())
}

/// `PS(http_session_vars)` given up, `zval_ptr_dtor` and `ZVAL_UNDEF`;
/// `$_SESSION` in the table keeps what it holds.
fn unbind(req: &mut Request) {
    if let Some(vars) = req.state::<State>().vars.take() {
        vars.give_up(req);
    }
}

/// A throwable made pending the way php leaves one in `EG(exception)`:
/// kept while the native goes on, raised when it returns. The unwind
/// exit is kept the same way, since php's `exit` is a throwable that C
/// code goes on past (`zend_is_unwind_exit`); a fatal is handed back at
/// once, as php's `zend_bailout` unwinds.
fn absorb<T>(req: &mut Request, done: Result<T, Thrown>) -> Result<Option<T>, Thrown> {
    match done {
        Ok(value) => Ok(Some(value)),
        Err(thrown) if thrown.cause(req) != Cause::Fatal => {
            let state = req.state::<State>();
            if state.thrown.is_none() {
                state.thrown = Some(thrown);
            }
            Ok(None)
        }
        Err(thrown) => Err(thrown),
    }
}

/// `zend_throw_error(NULL, ...)` where no throwable is pending yet.
fn throw_error(req: &mut Request, message: &str) {
    if req.state::<State>().thrown.is_none() {
        let thrown = php_ext::raising::throw_error(req, "Error", message);
        req.state::<State>().thrown = Some(thrown);
    }
}

/// `EG(exception)`.
fn pending(req: &mut Request) -> bool {
    req.state::<State>().thrown.is_some()
}

/// A native's return: `value`, or the throwable a handler left pending,
/// `value` given up.
fn answer<T: IntoReturn>(req: &mut Request, value: T) -> Result<T, Thrown> {
    match req.state::<State>().thrown.take() {
        Some(thrown) => {
            if let Ok(value) = value.into_return(req) {
                value.give_up(req);
            }
            Err(thrown)
        }
        None => Ok(value),
    }
}
