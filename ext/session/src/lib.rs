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
//! The mm handler and upload progress are not ported: the first is not
//! in php's default build, the second needs a request body the CLI
//! host does not have.
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
mod serializers;
mod session;
mod user;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::coerce::coerce_param;
use php_ext::decl::{
    ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr, EnumBacking, FunctionDecl, MethodDecl,
    Name, Parameter, Type, Visibility,
};
use php_ext::raising::{Level, Thrown, arg_count_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

/// `php_session_status`: the three values `session_status()` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Status {
    Disabled = 0,
    #[default]
    None = 1,
    Active = 2,
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
    /// Whether `PS(http_session_vars)` holds the session's array.
    tracked: bool,
    /// `PS(session_started_filename)` and its line.
    started_at: Option<(Vec<u8>, u32)>,
    /// The `session_shutdown` entry `session_set_save_handler` registers.
    shutdown_registered: bool,
    /// `EG(exception)` as a user handler left it: php goes on after a
    /// handler throws, and the native raises it when it returns.
    thrown: Option<Thrown>,
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
    let status = |name: &[u8], value: Status| ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Long(value as i64),
        attributes: Vec::new(),
    };
    let mut builder = ExtensionBuilder::default()
        .constants(vec![
            status(b"PHP_SESSION_DISABLED", Status::Disabled),
            status(b"PHP_SESSION_NONE", Status::None),
            status(b"PHP_SESSION_ACTIVE", Status::Active),
        ])
        .functions(natives::table())
        .ini(ini::declared())
        .dependencies(vec![Dependency {
            name: String::from("spl"),
            kind: DependencyKind::Optional,
            rel: None,
            version: None,
        }]);
    for (class, methods) in user::classes() {
        builder = builder.classes(vec![(class, methods)]);
    }
    builder
        .request_state::<State>()
        .rinit(session::rinit)
        .rshutdown(session::rshutdown)
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
    req.info_table(&[
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
                _ if value.is_empty() => String::from("no value"),
                _ => String::from_utf8_lossy(value).into_owned(),
            };
            let local = req
                .ini_get(name.as_bytes())
                .map_or_else(|| shown(default.as_bytes()), shown);
            [(*name).to_owned(), local, shown(default.as_bytes())]
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

// ------------------------------------------------------------ helpers

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The bytes up to the first NUL, as C reads a `char *`.
fn c_str(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or(&[])
}

/// The bytes as C's `%s` prints them.
fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(c_str(bytes)).into_owned()
}

/// One function or method as `session.stub.php` declares it.
fn declared(name: &[u8], parameters: Vec<Parameter>, returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters,
        return_type: Some(returns),
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

fn parameter(name: &[u8], ty: Option<Type>, default: Option<ConstExpr>) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default,
        attributes: Vec::new(),
    }
}

/// A public method of an interface or a class, its return tentative
/// as the stub's `@tentative-return-type` marks every one.
fn method(function: FunctionDecl, is_abstract: bool) -> MethodDecl {
    MethodDecl {
        function,
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract,
        is_final: false,
        tentative_return: true,
    }
}

fn class(
    name: &[u8],
    kind: ClassKind,
    interfaces: &[&[u8]],
    methods: Vec<MethodDecl>,
) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: interfaces.iter().map(|i| Name((*i).into())).collect(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
fn arguments_between(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (word, bound) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}

/// The argument at `position` when the call gave one that is not
/// `null`, for the `!` forms, and any one given for the rest.
fn given(req: &Request, position: u32, nullable: bool) -> Option<Value> {
    req.argument(position).filter(|value| {
        let value = value.deref();
        !(matches!(value.as_ref(), ValueRef::Undef)
            || (nullable && matches!(value.as_ref(), ValueRef::Null)))
    })
}

/// A parameter read as `coerce_param` reads its declared type.
fn coerced(req: &mut Request, position: u32, ty: &Type) -> Result<Value, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    Ok(coerce_param(req, &value, ty, position)?.deref())
}

/// `Z_PARAM_STR` (or `P`, where php refuses a NUL byte, with `path`),
/// optional with `nullable` for `S!` and `P!`.
fn string_argument(
    req: &mut Request,
    position: u32,
    nullable: bool,
    path: bool,
) -> Result<Option<Vec<u8>>, Thrown> {
    if given(req, position, nullable).is_none() {
        return Ok(None);
    }
    let ty = if nullable {
        Type::Nullable(Box::new(Type::String))
    } else {
        Type::String
    };
    let bytes = coerced(req, position, &ty)?
        .as_str_bytes()
        .unwrap_or_default()
        .to_vec();
    if path && bytes.contains(&0) {
        return Err(php_ext::raising::arg_value_error(
            req,
            position,
            "not contain any null bytes",
        ));
    }
    Ok(Some(bytes))
}

/// `Z_PARAM_BOOL`, `absent` where the call gave none.
fn bool_argument(req: &mut Request, position: u32, absent: bool) -> Result<bool, Thrown> {
    if given(req, position, false).is_none() {
        return Ok(absent);
    }
    Ok(coerced(req, position, &Type::Bool)?
        .as_bool()
        .unwrap_or(false))
}

/// A new string of the request's as a value.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// An ini entry's text, the engine default where the request set none.
fn ini(req: &Request, name: &[u8]) -> Vec<u8> {
    req.ini_get(name).map(<[u8]>::to_vec).unwrap_or_default()
}

/// A long entry as `OnUpdateLong` stores it, `ZEND_STRTOL`'s leading
/// number.
fn ini_long(req: &Request, name: &[u8]) -> i64 {
    atol(&ini(req, name))
}

/// A boolean entry as `OnUpdateBool` stores it.
fn ini_bool(req: &Request, name: &[u8]) -> bool {
    Request::ini_parse_bool(&ini(req, name))
}

/// `atol`: optional blanks, a sign and the digits that follow, zero for
/// none, saturating as `strtol` does.
fn atol(bytes: &[u8]) -> i64 {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim_start();
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits: String = digits.chars().take_while(char::is_ascii_digit).collect();
    let n = digits
        .parse::<i64>()
        .unwrap_or(if digits.is_empty() { 0 } else { i64::MAX });
    if negative { n.saturating_neg() } else { n }
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

/// The session's array, `Z_REFVAL(PS(http_session_vars))`, while one is
/// tracked.
fn session_array(req: &mut Request) -> Option<PhpArray> {
    if req.state::<State>().tracked {
        req.superglobal(b"_SESSION").copied()
    } else {
        None
    }
}

/// `$_SESSION` bound to `array`, which it owns from here; what it held
/// before given up.
fn bind_array(req: &mut Request, array: PhpArray) {
    if let Some(old) = req.superglobal(b"_SESSION").copied() {
        req.set_superglobal(b"_SESSION", array);
        req.release(old.to_value());
    } else {
        req.set_superglobal(b"_SESSION", array);
    }
    req.state::<State>().tracked = true;
}

/// A throwable made pending the way php leaves one in `EG(exception)`:
/// kept while the native goes on, raised when it returns; a fatal or an
/// `exit` is handed back at once, as php's `zend_bailout` unwinds.
fn absorb<T>(req: &mut Request, done: Result<T, Thrown>) -> Result<Option<T>, Thrown> {
    match done {
        Ok(value) => Ok(Some(value)),
        Err(thrown) if thrown.exception().is_some() => {
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
        let thrown = throw(req, "Error", message);
        req.state::<State>().thrown = Some(thrown);
    }
}

/// `EG(exception)`.
fn pending(req: &mut Request) -> bool {
    req.state::<State>().thrown.is_some()
}

/// A native's return: `value`, or the throwable a handler left pending,
/// `value` given up.
fn answer(req: &mut Request, value: Value) -> Result<Value, Thrown> {
    match req.state::<State>().thrown.take() {
        Some(thrown) => {
            req.release(value);
            Err(thrown)
        }
        None => Ok(value),
    }
}
