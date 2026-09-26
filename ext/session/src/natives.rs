//! The natives of `session.stub.php`, `PHP_FUNCTION(session_*)` of
//! `session.c` each: its parameters, its refusals and warnings, and the
//! state machine in `session` behind it.

use php_ext::abi::Callable;
use php_ext::call::lookup_class;
use php_ext::coerce::{callable_checked, is_callable, to_string, truthy, value_name};
use php_ext::decl::{ConstExpr, FunctionDecl, ParameterDefault, Type};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, Value, ValueRef};

use crate::user::{Slot, method_of, store};
use crate::{
    Entry, Module, State, Status, answer, arguments_between, bool_argument, coerced, declared,
    given, headers_sent, ini, ini_bool, ini_long, parameter, pending, sent_error, session,
    started_error, string_argument, string_value,
};

/// The natives in `session.stub.php`'s order, `session_commit` the alias
/// of `session_write_close`.
#[allow(clippy::too_many_lines)] // one row per native of the stub
pub(super) fn table() -> Vec<(FunctionDecl, Entry)> {
    let null = || ParameterDefault::Expr(ConstExpr::Null);
    let nullable = |ty: Type| Some(Type::Nullable(Box::new(ty)));
    let string_or_false = || Type::Union(vec![Type::String, Type::False]);
    let optional_string = |name: &[u8]| parameter(name, nullable(Type::String), null());
    let unknown = || ParameterDefault::Unshown;
    let callable = |name: &[u8]| parameter(name, Some(Type::Callable), unknown());
    let nullable_callable = |name: &[u8]| parameter(name, nullable(Type::Callable), null());
    let none =
        |name: &[u8], returns: Type, entry: Entry| (declared(name, Vec::new(), returns), entry);
    vec![
        (
            declared(
                b"session_name",
                vec![optional_string(b"name")],
                string_or_false(),
            ),
            session_name,
        ),
        (
            declared(
                b"session_module_name",
                vec![optional_string(b"module")],
                string_or_false(),
            ),
            session_module_name,
        ),
        (
            declared(
                b"session_save_path",
                vec![optional_string(b"path")],
                string_or_false(),
            ),
            session_save_path,
        ),
        (
            declared(
                b"session_id",
                vec![optional_string(b"id")],
                string_or_false(),
            ),
            session_id,
        ),
        (
            declared(
                b"session_create_id",
                vec![parameter(
                    b"prefix",
                    Some(Type::String),
                    ParameterDefault::Expr(ConstExpr::Str(Box::new([]))),
                )],
                string_or_false(),
            ),
            session_create_id,
        ),
        (
            declared(
                b"session_regenerate_id",
                vec![parameter(
                    b"delete_old_session",
                    Some(Type::Bool),
                    ParameterDefault::Expr(ConstExpr::Bool(false)),
                )],
                Type::Bool,
            ),
            session_regenerate_id,
        ),
        (
            declared(
                b"session_decode",
                vec![parameter(
                    b"data",
                    Some(Type::String),
                    ParameterDefault::None,
                )],
                Type::Bool,
            ),
            session_decode,
        ),
        none(b"session_encode", string_or_false(), session_encode),
        none(b"session_destroy", Type::Bool, session_destroy),
        none(b"session_unset", Type::Bool, session_unset),
        none(
            b"session_gc",
            Type::Union(vec![Type::Int, Type::False]),
            session_gc,
        ),
        none(
            b"session_get_cookie_params",
            Type::Array,
            session_get_cookie_params,
        ),
        none(b"session_write_close", Type::Bool, session_write_close),
        none(b"session_abort", Type::Bool, session_abort),
        none(b"session_reset", Type::Bool, session_reset),
        none(b"session_status", Type::Int, session_status),
        none(
            b"session_register_shutdown",
            Type::Void,
            session_register_shutdown,
        ),
        none(b"session_commit", Type::Bool, session_write_close),
        (
            declared(
                b"session_set_save_handler",
                vec![
                    parameter(b"open", None, ParameterDefault::None),
                    parameter(b"close", None, unknown()),
                    callable(b"read"),
                    callable(b"write"),
                    callable(b"destroy"),
                    callable(b"gc"),
                    nullable_callable(b"create_sid"),
                    nullable_callable(b"validate_sid"),
                    nullable_callable(b"update_timestamp"),
                ],
                Type::Bool,
            ),
            session_set_save_handler,
        ),
        (
            declared(
                b"session_cache_limiter",
                vec![optional_string(b"value")],
                string_or_false(),
            ),
            session_cache_limiter,
        ),
        (
            declared(
                b"session_cache_expire",
                vec![parameter(b"value", nullable(Type::Int), null())],
                Type::Union(vec![Type::Int, Type::False]),
            ),
            session_cache_expire,
        ),
        (
            declared(
                b"session_set_cookie_params",
                vec![
                    parameter(
                        b"lifetime_or_options",
                        Some(Type::Union(vec![Type::Array, Type::Int])),
                        ParameterDefault::None,
                    ),
                    optional_string(b"path"),
                    optional_string(b"domain"),
                    parameter(b"secure", nullable(Type::Bool), null()),
                    parameter(b"httponly", nullable(Type::Bool), null()),
                ],
                Type::Bool,
            ),
            session_set_cookie_params,
        ),
        (
            declared(
                b"session_start",
                vec![parameter(
                    b"options",
                    Some(Type::Array),
                    ParameterDefault::Expr(ConstExpr::Array(Vec::new())),
                )],
                Type::Bool,
            ),
            session_start,
        ),
    ]
}

/// `php_can_change_session_setting`: `false` after php's warning while a
/// session is active, or once the headers are sent (for the id, only
/// where cookies carry it).
fn can_change(req: &mut Request, setting: &str, check_cookies: bool) -> bool {
    if req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            &format!("Session {setting} cannot be changed when a session is active"),
        );
        return false;
    }
    if headers_sent(req) && (!check_cookies || ini_bool(req, b"session.use_cookies")) {
        sent_error(
            req,
            &format!("Session {setting} cannot be changed after headers have already been sent"),
        );
        return false;
    }
    true
}

/// `session_name(?string $name = null): string|false`: the name, and a
/// new one set through `session.name`'s rule.
///
/// ```php
/// var_dump(session_name(), session_name("other"), session_name());
/// try { session_name("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(9) "PHPSESSID"
/// string(9) "PHPSESSID"
/// string(5) "other"
/// session_name(): Argument #1 ($name) must not contain any null bytes
/// ```
fn session_name(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let name = string_argument(req, 1, true, true)?;
    if name.is_some() && !can_change(req, "name", false) {
        return Ok(Value::bool(false));
    }
    let current = ini(req, b"session.name");
    let current = string_value(req, &current)?;
    if let Some(name) = name {
        crate::ini::alter(req, b"session.name", &name)?;
    }
    Ok(current)
}

/// `session_module_name(?string $module = null): string|false`: the
/// save handler's name, and a new one set through `session.save_handler`.
///
/// ```php
/// var_dump(session_module_name(), session_module_name("files"), session_module_name("nope"));
/// try { session_module_name("USER"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: session_module_name(): Session handler module "nope" cannot be found in %s on line 2
/// string(5) "files"
/// string(5) "files"
/// bool(false)
///
/// Warning: session_module_name(): Session save handler module cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
/// ```
fn session_module_name(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let name = string_argument(req, 1, true, false)?;
    if name.is_some() && !can_change(req, "save handler module", false) {
        return Ok(Value::bool(false));
    }
    let current = req.state::<State>().module.map_or("", Module::name);
    let current = string_value(req, current.as_bytes())?;
    let Some(name) = name else {
        return Ok(current);
    };
    if crate::c_str(&name).eq_ignore_ascii_case(b"user") {
        req.release(current);
        return Err(throw(
            req,
            "ValueError",
            "session_module_name(): Argument #1 ($module) cannot be \"user\"",
        ));
    }
    if Module::find(&name).is_none() {
        req.release(current);
        docref(
            req,
            Level::Warning,
            &format!(
                "Session handler module \"{}\" cannot be found",
                crate::shown(&name)
            ),
        );
        return Ok(Value::bool(false));
    }
    session::close(req)?;
    req.state::<State>().files = None;
    crate::ini::alter(req, b"session.save_handler", &name)?;
    answer(req, current)
}

/// `session_save_path(?string $path = null): string|false`.
///
/// ```php
/// var_dump(session_save_path(), session_save_path("/tmp/x"), session_save_path());
/// ```
/// ```expect
/// string(0) ""
/// string(0) ""
/// string(6) "/tmp/x"
/// ```
fn session_save_path(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let path = string_argument(req, 1, true, true)?;
    if path.is_some() && !can_change(req, "save path", false) {
        return Ok(Value::bool(false));
    }
    let current = ini(req, b"session.save_path");
    let current = string_value(req, &current)?;
    if let Some(path) = path {
        crate::ini::alter(req, b"session.save_path", &path)?;
    }
    Ok(current)
}

/// `session_id(?string $id = null): string|false`: the id up to its
/// first NUL, the empty string for none, and a new one set.
///
/// ```php
/// var_dump(session_id(), session_id("abc"), session_id(), session_id("x\0y"), session_id());
/// ```
/// ```expect
/// string(0) ""
/// string(0) ""
/// string(3) "abc"
/// string(3) "abc"
/// string(1) "x"
/// ```
fn session_id(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let id = string_argument(req, 1, true, false)?;
    if id.is_some() && !can_change(req, "ID", true) {
        return Ok(Value::bool(false));
    }
    let current = req.state::<State>().id.clone().unwrap_or_default();
    let current = string_value(req, crate::c_str(&current))?;
    if let Some(id) = id {
        req.state::<State>().id = Some(id);
    }
    Ok(current)
}

/// `session_regenerate_id(bool $delete_old_session = false): bool`: the
/// data written under the old id, or the old id destroyed, then a new id
/// opened, read, and its cookie sent.
///
/// ```php
/// ob_start(); session_save_path(__DIR__); session_id("regen1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// session_decode("a|i:1;"); var_dump(session_regenerate_id(), session_id() !== "regen1", file_get_contents(__DIR__ . "/sess_regen1"));
/// $new = session_id(); var_dump(session_regenerate_id(true), file_exists(__DIR__ . "/sess_$new"));
/// session_write_close(); var_dump(session_regenerate_id());
/// ob_end_flush();
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(6) "a|i:1;"
/// bool(true)
/// bool(false)
///
/// Warning: session_regenerate_id(): Session ID cannot be regenerated when there is no active session (started from %s on line 2) in %s on line 5
/// bool(false)
/// ```
fn session_regenerate_id(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let delete = bool_argument(req, 1, false)?;
    if req.state::<State>().status != Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session ID cannot be regenerated when there is no active session",
        );
        return Ok(Value::bool(false));
    }
    if headers_sent(req) {
        sent_error(
            req,
            "Session ID cannot be regenerated after headers have already been sent",
        );
        return Ok(Value::bool(false));
    }
    let done = regenerate(req, delete)?;
    answer(req, Value::bool(done))
}

/// The body of `session_regenerate_id` past its checks.
#[allow(clippy::too_many_lines)] // php's function, step for step
fn regenerate(req: &mut Request, delete: bool) -> Result<bool, Thrown> {
    let Some(module) = req.state::<State>().module else {
        return Ok(false);
    };
    let path = crate::shown(&ini(req, b"session.save_path"));
    let id = req.state::<State>().id.clone().unwrap_or_default();
    let fail = |req: &mut Request| -> Result<(), Thrown> {
        session::close(req)?;
        req.state::<State>().status = Status::None;
        Ok(())
    };
    if delete {
        let destroyed = session::destroy_id(req, module, &id);
        if !crate::absorb(req, destroyed)?.unwrap_or(false) {
            fail(req)?;
            if !pending(req) {
                docref(
                    req,
                    Level::Warning,
                    &format!(
                        "Session object destruction failed. ID: {} (path: {path})",
                        module.name()
                    ),
                );
            }
            return Ok(false);
        }
    } else {
        let data = session::encode(req)?.unwrap_or_default();
        let written = session::write(req, module, &id, &data);
        if !crate::absorb(req, written)?.unwrap_or(false) {
            fail(req)?;
            docref(
                req,
                Level::Warning,
                &format!("Session write failed. ID: {} (path: {path})", module.name()),
            );
            return Ok(false);
        }
    }
    session::close(req)?;
    let state = req.state::<State>();
    state.session_vars = None;
    state.id = None;
    let opened = session::open(req, module);
    if !crate::absorb(req, opened)?.unwrap_or(false) {
        req.state::<State>().status = Status::None;
        if !pending(req) {
            crate::throw_error(
                req,
                &format!("Failed to open session: {} (path: {path})", module.name()),
            );
        }
        return Ok(false);
    }
    let made = session::create_sid(req, module);
    let Some(mut new_id) = crate::absorb(req, made)?.flatten() else {
        req.state::<State>().status = Status::None;
        if !pending(req) {
            crate::throw_error(
                req,
                &format!(
                    "Failed to create new session ID: {} (path: {path})",
                    module.name()
                ),
            );
        }
        return Ok(false);
    };
    let checks = req.state::<State>().handlers.validate_sid.is_some() || module == Module::Files;
    if ini_bool(req, b"session.use_strict_mode") && checks {
        for _ in 0..3 {
            let taken = session::validate_sid(req, module, &new_id);
            if !crate::absorb(req, taken)?.unwrap_or(false) {
                break;
            }
            let made = session::create_sid(req, module);
            let Some(made) = crate::absorb(req, made)?.flatten() else {
                session::close(req)?;
                req.state::<State>().status = Status::None;
                if !pending(req) {
                    crate::throw_error(
                        req,
                        &format!(
                            "Failed to create session ID by collision: {} (path: {path})",
                            module.name()
                        ),
                    );
                }
                return Ok(false);
            };
            new_id = made;
        }
    }
    req.state::<State>().id = Some(new_id.clone());
    let read = match module {
        Module::Files => crate::files::read(req, &new_id),
        Module::User => crate::user::read(req, &new_id),
    };
    if crate::absorb(req, read)?.flatten().is_none() {
        session::close(req)?;
        req.state::<State>().status = Status::None;
        if !pending(req) {
            crate::throw_error(
                req,
                &format!(
                    "Failed to create(read) session ID: {} (path: {path})",
                    module.name()
                ),
            );
        }
        return Ok(false);
    }
    if ini_bool(req, b"session.use_cookies") {
        req.state::<State>().send_cookie = true;
    }
    session::reset_id(req)
}

/// `session_create_id(string $prefix = ""): string|false`: a new id
/// behind the prefix, through the handler while a session is active so
/// it can refuse one that collides.
///
/// ```php
/// var_dump(strlen(session_create_id()), substr(session_create_id("ab-,"), 0, 4), session_create_id("no*"));
/// try { session_create_id(str_repeat("a", 257)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: session_create_id(): Prefix cannot contain special characters. Only the A-Z, a-z, 0-9, "-", and "," characters are allowed in %s on line 2
/// int(32)
/// string(4) "ab-,"
/// bool(false)
/// session_create_id(): Argument #1 ($prefix) cannot be longer than 256 characters
/// ```
fn session_create_id(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let prefix = string_argument(req, 1, false, true)?.unwrap_or_default();
    if !prefix.is_empty() {
        if prefix.len() > 256 {
            return Err(throw(
                req,
                "ValueError",
                "session_create_id(): Argument #1 ($prefix) cannot be longer than 256 characters",
            ));
        }
        if !session::valid_key(&prefix) {
            docref(
                req,
                Level::Warning,
                "Prefix cannot contain special characters. Only the A-Z, a-z, 0-9, \"-\", and \",\" characters are allowed",
            );
            return Ok(Value::bool(false));
        }
    }
    let state = req.state::<State>();
    let mut new_id = None;
    if let (false, Status::Active, Some(module)) =
        (state.in_save_handler, state.status, state.module)
    {
        let no_check = module == Module::User && state.handlers.validate_sid.is_none();
        for _ in 0..3 {
            let made = session::create_sid(req, module);
            new_id = crate::absorb(req, made)?.flatten();
            if no_check {
                break;
            }
            let Some(id) = new_id.clone() else { break };
            let taken = session::validate_sid(req, module, &id);
            if crate::absorb(req, taken)?.unwrap_or(false) {
                new_id = None;
                continue;
            }
            break;
        }
    } else {
        new_id = session::create_id(req);
    }
    let Some(new_id) = new_id else {
        docref(req, Level::Warning, "Failed to create new ID");
        return answer(req, Value::bool(false));
    };
    let mut id = prefix;
    id.extend_from_slice(&new_id);
    let id = string_value(req, &id)?;
    answer(req, id)
}

/// `session_cache_limiter(?string $value = null): string|false`.
///
/// ```php
/// var_dump(session_cache_limiter(), session_cache_limiter("public"), session_cache_limiter());
/// ```
/// ```expect
/// string(7) "nocache"
/// string(7) "nocache"
/// string(6) "public"
/// ```
fn session_cache_limiter(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let limiter = string_argument(req, 1, true, false)?;
    if limiter.is_some() && req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session cache limiter cannot be changed when a session is active",
        );
        return Ok(Value::bool(false));
    }
    if limiter.is_some() && headers_sent(req) {
        sent_error(
            req,
            "Session cache limiter cannot be changed after headers have already been sent",
        );
        return Ok(Value::bool(false));
    }
    let current = ini(req, b"session.cache_limiter");
    let current = string_value(req, &current)?;
    if let Some(limiter) = limiter {
        crate::ini::alter(req, b"session.cache_limiter", &limiter)?;
    }
    Ok(current)
}

/// `session_cache_expire(?int $value = null): int|false`.
///
/// ```php
/// var_dump(session_cache_expire(), session_cache_expire(30), session_cache_expire());
/// ```
/// ```expect
/// int(180)
/// int(180)
/// int(30)
/// ```
fn session_cache_expire(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let expires = match given(req, 1, true) {
        Some(_) => coerced(req, 1, &Type::Nullable(Box::new(Type::Int)))?.as_int(),
        None => None,
    };
    let current = ini_long(req, b"session.cache_expire");
    if expires.is_some() && req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session cache expiration cannot be changed when a session is active",
        );
        return Ok(Value::int(current));
    }
    if expires.is_some() && headers_sent(req) {
        sent_error(
            req,
            "Session cache expiration cannot be changed after headers have already been sent",
        );
        return Ok(Value::bool(false));
    }
    if let Some(expires) = expires {
        crate::ini::alter(req, b"session.cache_expire", expires.to_string().as_bytes())?;
    }
    Ok(Value::int(current))
}

/// `session_encode(): string|false`: the session's variables as the
/// serializer writes them.
///
/// ```php
/// var_dump(session_encode());
/// ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]);
/// var_dump(session_encode(), session_decode('a|s:1:"b";'), session_encode());
/// session_write_close(); ob_end_flush();
/// ```
/// ```expect
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 2
/// bool(false)
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 4
///
/// Warning: session_decode(): Session data cannot be decoded when there is no active session in %s on line 4
///
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn session_encode(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let encoded = session::encode(req)?;
    let value = match encoded {
        Some(bytes) => string_value(req, &bytes)?,
        None => Value::bool(false),
    };
    answer(req, value)
}

/// `session_decode(string $data): bool`: the data read into the active
/// session's variables.
///
/// ```php
/// var_dump(session_decode("a|i:1;"));
/// ```
/// ```expect
/// Warning: session_decode(): Session data cannot be decoded when there is no active session in %s on line 2
/// bool(false)
/// ```
fn session_decode(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 1)?;
    let data = string_argument(req, 1, false, false)?.unwrap_or_default();
    if req.state::<State>().status != Status::Active {
        docref(
            req,
            Level::Warning,
            "Session data cannot be decoded when there is no active session",
        );
        return Ok(Value::bool(false));
    }
    let ok = session::decode(req, &data)?;
    answer(req, Value::bool(ok))
}

/// `session_start(array $options = []): bool`: each option set as its
/// `session.*` entry, `read_and_close` read as an int, then the session
/// started.
///
/// ```php
/// ob_start();
/// var_dump(session_start(["save_path" => __DIR__, "use_cookies" => "0", "cache_limiter" => "", "read_and_close" => true]), session_status());
/// var_dump(session_start(), session_start(), session_status());
/// session_write_close();
/// try { session_start([1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { session_start(["name" => []]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { session_start(["read_and_close" => "x"]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ob_end_flush();
/// ```
/// ```expect
/// bool(true)
/// int(1)
///
/// Notice: session_start(): Ignoring session_start() because a session is already active (started from %s on line 4) in %s on line 4
/// bool(true)
/// bool(true)
/// int(2)
/// session_start(): Argument #1 ($options) must be of type array with keys as string
/// session_start(): Option "name" must be of type string|int|bool, array given
/// session_start(): Option "read_and_close" value must be of type compatible with int, "x" given
/// ```
fn session_start(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 1)?;
    let options = match given(req, 1, false) {
        Some(_) => coerced(req, 1, &Type::Array)?.as_array().copied(),
        None => None,
    };
    if req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Notice,
            "Ignoring session_start() because a session is already active",
        );
        return Ok(Value::bool(true));
    }
    if ini_bool(req, b"session.use_cookies") && headers_sent(req) {
        sent_error(
            req,
            "Session cannot be started after headers have already been sent",
        );
        return Ok(Value::bool(false));
    }
    let mut read_and_close = false;
    for (key, value) in options.map(|o| o.snapshot()).unwrap_or_default() {
        let Key::Str(name) = key else {
            return Err(arg_value_error(
                req,
                1,
                "be of type array with keys as string",
            ));
        };
        let name = name.as_bytes().to_vec();
        match value.as_ref() {
            ValueRef::Str(_) | ValueRef::Bool(_) | ValueRef::Int(_) => {}
            _ => {
                let given = value_name(req, &value);
                return Err(throw(
                    req,
                    "TypeError",
                    &format!(
                        "session_start(): Option \"{}\" must be of type string|int|bool, {given} given",
                        crate::shown(&name)
                    ),
                ));
            }
        }
        if name == b"read_and_close" {
            let n = match value.as_ref() {
                ValueRef::Str(s) => match php_ext::coerce::numeric_string(s.as_bytes())
                    .as_ref()
                    .map(Value::as_ref)
                {
                    Some(ValueRef::Int(n)) => n,
                    _ => {
                        return Err(throw(
                            req,
                            "TypeError",
                            &format!(
                                "session_start(): Option \"read_and_close\" value must be of type compatible with int, \"{}\" given",
                                crate::shown(s.as_bytes())
                            ),
                        ));
                    }
                },
                ValueRef::Int(n) => n,
                ValueRef::Bool(b) => i64::from(b),
                _ => 0,
            };
            read_and_close = n > 0;
            continue;
        }
        let text = to_string(req, &value)?;
        let text_bytes = text.as_bytes().to_vec();
        req.release(Value::string(text));
        let mut entry = b"session.".to_vec();
        entry.extend_from_slice(&name);
        if !crate::ini::alter(req, &entry, &text_bytes)? {
            docref(
                req,
                Level::Warning,
                &format!("Setting option \"{}\" failed", crate::shown(&name)),
            );
        }
    }
    session::start(req)?;
    if req.state::<State>().status != Status::Active {
        clean(req)?;
        return answer(req, Value::bool(false));
    }
    if read_and_close {
        session::flush(req, false)?;
    }
    answer(req, Value::bool(true))
}

/// `zend_hash_clean` over the session's array, separated first, where
/// there is one.
fn clean(req: &mut Request) -> Result<(), Thrown> {
    let Some(mut array) = crate::session_array_mut(req)? else {
        return Ok(());
    };
    for (key, _) in array.snapshot_refs() {
        array.unset(req, &key)?;
    }
    Ok(())
}

/// `session_destroy(): bool`.
///
/// ```php
/// var_dump(session_destroy());
/// ob_start(); session_save_path(__DIR__); session_id("destroy1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// session_write_close(); session_start(); var_dump(file_exists(__DIR__ . "/sess_destroy1"), session_destroy(), file_exists(__DIR__ . "/sess_destroy1"), session_status(), session_id(), session_encode());
/// ob_end_flush();
/// ```
/// ```expect
/// Warning: session_destroy(): Trying to destroy uninitialized session in %s on line 2
/// bool(false)
///
/// Warning: session_save_path(): Session save path cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_id(): Session ID cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 4
///
/// Warning: session_destroy(): Trying to destroy uninitialized session in %s on line 4
///
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// int(1)
/// string(0) ""
/// bool(false)
/// ```
fn session_destroy(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let ok = session::destroy(req)?;
    answer(req, Value::bool(ok))
}

/// `session_unset(): bool`: the active session's variables removed.
///
/// ```php
/// var_dump(session_unset());
/// ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]); session_decode("a|i:1;");
/// var_dump(session_unset(), session_encode()); session_write_close(); ob_end_flush();
/// ```
/// ```expect
/// bool(false)
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_decode(): Session data cannot be decoded when there is no active session in %s on line 3
///
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 4
/// bool(false)
/// bool(false)
/// ```
fn session_unset(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    if req.state::<State>().status != Status::Active {
        return Ok(Value::bool(false));
    }
    clean(req)?;
    Ok(Value::bool(true))
}

/// `session_gc(): int|false`: the handler's collection forced.
///
/// ```php
/// var_dump(session_gc());
/// ```
/// ```expect
/// Warning: session_gc(): Session cannot be garbage collected when there is no active session in %s on line 2
/// bool(false)
/// ```
fn session_gc(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    if req.state::<State>().status != Status::Active {
        docref(
            req,
            Level::Warning,
            "Session cannot be garbage collected when there is no active session",
        );
        return Ok(Value::bool(false));
    }
    let n = session::gc(req, true)?;
    answer(
        req,
        if n < 0 {
            Value::bool(false)
        } else {
            Value::int(n)
        },
    )
}

/// `session_get_cookie_params(): array`.
///
/// ```php
/// var_dump(session_get_cookie_params());
/// ```
/// ```expect
/// array(7) {
///   ["lifetime"]=>
///   int(0)
///   ["path"]=>
///   string(1) "/"
///   ["domain"]=>
///   string(0) ""
///   ["secure"]=>
///   bool(false)
///   ["partitioned"]=>
///   bool(false)
///   ["httponly"]=>
///   bool(false)
///   ["samesite"]=>
///   string(0) ""
/// }
/// ```
fn session_get_cookie_params(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let mut array = PhpArray::new(req)?;
    let rows: [(&[u8], &[u8], u8); 7] = [
        (b"lifetime", b"session.cookie_lifetime", 0),
        (b"path", b"session.cookie_path", 1),
        (b"domain", b"session.cookie_domain", 1),
        (b"secure", b"session.cookie_secure", 2),
        (b"partitioned", b"session.cookie_partitioned", 2),
        (b"httponly", b"session.cookie_httponly", 2),
        (b"samesite", b"session.cookie_samesite", 1),
    ];
    for (key, entry, kind) in rows {
        let value = match kind {
            0 => Value::int(ini_long(req, entry)),
            1 => {
                let text = ini(req, entry);
                string_value(req, &text)?
            }
            _ => Value::bool(ini_bool(req, entry)),
        };
        let key = php_ext::values::PhpString::new(req, key)?;
        array.set(req, Key::Str(key), value)?;
    }
    Ok(array.to_value())
}

/// `session_set_cookie_params(array|int $lifetime_or_options, ?string
/// $path = null, ?string $domain = null, ?bool $secure = null, ?bool
/// $httponly = null): bool`: each given one set through its
/// `session.cookie_*` entry.
///
/// ```php
/// var_dump(session_set_cookie_params(60, "/p", "d.example", true, true), session_get_cookie_params()["path"]);
/// var_dump(session_set_cookie_params(["lifetime" => 5, "SameSite" => "Lax", "bogus" => 1, 3 => 4]), session_get_cookie_params()["samesite"]);
/// try { session_set_cookie_params([]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { session_set_cookie_params([], "/x"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(session_set_cookie_params(-1));
/// ```
/// ```expect
/// bool(true)
/// string(2) "/p"
///
/// Warning: session_set_cookie_params(): Session cookie parameters cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
/// bool(false)
/// string(0) ""
///
/// Warning: session_set_cookie_params(): Session cookie parameters cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 4
///
/// Warning: session_set_cookie_params(): Session cookie parameters cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 5
///
/// Warning: session_set_cookie_params(): Session cookie parameters cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 6
/// bool(false)
/// ```
#[allow(clippy::too_many_lines)] // php's function
fn session_set_cookie_params(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 5)?;
    let first = req.argument(1).unwrap_or_default().deref();
    let options = if let Some(array) = first.as_array() {
        Some(*array)
    } else {
        if matches!(first.as_ref(), ValueRef::Object(_) | ValueRef::Resource(_)) {
            let name = value_name(req, &first);
            return Err(arg_type_error(
                req,
                1,
                &format!("be of type array|int, {name} given"),
            ));
        }
        None
    };
    let lifetime_long = if options.is_none() {
        coerced(req, 1, &Type::Int)?.as_int().unwrap_or(0)
    } else {
        0
    };
    let mut path = string_argument(req, 2, true, false)?;
    let mut domain = string_argument(req, 3, true, false)?;
    let bool_or_null = |req: &mut Request, at: u32| -> Result<Option<bool>, Thrown> {
        match given(req, at, true) {
            Some(_) => Ok(coerced(req, at, &Type::Nullable(Box::new(Type::Bool)))?.as_bool()),
            None => Ok(None),
        }
    };
    let mut secure = bool_or_null(req, 4)?;
    let mut httponly = bool_or_null(req, 5)?;
    if !ini_bool(req, b"session.use_cookies") {
        docref(
            req,
            Level::Warning,
            "Session cookies cannot be used when session.use_cookies is disabled",
        );
        return Ok(Value::bool(false));
    }
    if req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session cookie parameters cannot be changed when a session is active",
        );
        return Ok(Value::bool(false));
    }
    if headers_sent(req) {
        sent_error(
            req,
            "Session cookie parameters cannot be changed after headers have already been sent",
        );
        return Ok(Value::bool(false));
    }
    let mut lifetime = None;
    let mut partitioned = None;
    let mut samesite = None;
    if let Some(options) = options {
        let must_be_null = "be null when argument #1 ($lifetime_or_options) is an array";
        for (at, set) in [
            (2, path.is_some()),
            (3, domain.is_some()),
            (4, secure.is_some()),
            (5, httponly.is_some()),
        ] {
            if set {
                return Err(arg_value_error(req, at, must_be_null));
            }
        }
        let mut found = 0;
        for (key, value) in options.snapshot() {
            let Key::Str(key) = key else {
                docref(
                    req,
                    Level::Warning,
                    "Argument #1 ($lifetime_or_options) cannot contain numeric keys",
                );
                continue;
            };
            let key = key.as_bytes().to_ascii_lowercase();
            let text = |req: &mut Request| -> Result<Vec<u8>, Thrown> {
                let s = to_string(req, &value)?;
                let bytes = s.as_bytes().to_vec();
                req.release(Value::string(s));
                Ok(bytes)
            };
            match key.as_slice() {
                b"lifetime" => lifetime = Some(text(req)?),
                b"path" => path = Some(text(req)?),
                b"domain" => domain = Some(text(req)?),
                b"secure" => secure = Some(truthy(req, &value)),
                b"partitioned" => partitioned = Some(truthy(req, &value)),
                b"httponly" => httponly = Some(truthy(req, &value)),
                b"samesite" => samesite = Some(text(req)?),
                _ => {
                    docref(
                        req,
                        Level::Warning,
                        &format!(
                            "Argument #1 ($lifetime_or_options) contains an unrecognized key \"{}\"",
                            crate::shown(&key)
                        ),
                    );
                    continue;
                }
            }
            found += 1;
        }
        if found == 0 {
            return Err(arg_value_error(req, 1, "contain at least 1 valid key"));
        }
    } else {
        lifetime = Some(lifetime_long.to_string().into_bytes());
    }
    let flag = |b: bool| if b { b"1".to_vec() } else { b"0".to_vec() };
    let settings: [(&[u8], Option<Vec<u8>>); 7] = [
        (b"session.cookie_lifetime", lifetime),
        (b"session.cookie_path", path),
        (b"session.cookie_domain", domain),
        (b"session.cookie_secure", secure.map(flag)),
        (b"session.cookie_partitioned", partitioned.map(flag)),
        (b"session.cookie_httponly", httponly.map(flag)),
        (b"session.cookie_samesite", samesite),
    ];
    for (entry, value) in settings {
        if let Some(value) = value
            && !crate::ini::alter(req, entry, &value)?
        {
            return Ok(Value::bool(false));
        }
    }
    Ok(Value::bool(true))
}

/// `session_write_close(): bool` and its alias `session_commit()`: the
/// session written and closed.
///
/// ```php
/// var_dump(session_write_close());
/// ob_start(); session_save_path(__DIR__); session_id("close1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); session_decode("n|i:5;");
/// var_dump(session_commit(), session_status(), file_get_contents(__DIR__ . "/sess_close1"), session_encode());
/// ob_end_flush();
/// ```
/// ```expect
/// bool(false)
///
/// Warning: session_save_path(): Session save path cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_id(): Session ID cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_decode(): Session data cannot be decoded when there is no active session in %s on line 3
///
/// Warning: file_get_contents(%SCRATCH%/sess_close1): Failed to open stream: No such file or directory in %s on line 4
///
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 4
/// bool(false)
/// int(1)
/// bool(false)
/// bool(false)
/// ```
fn session_write_close(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    if req.state::<State>().status != Status::Active {
        return Ok(Value::bool(false));
    }
    session::flush(req, true)?;
    answer(req, Value::bool(true))
}

/// `session_abort(): bool`: the session closed, nothing written.
///
/// ```php
/// var_dump(session_abort());
/// ob_start(); session_save_path(__DIR__); session_id("abort1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); session_decode("n|i:5;");
/// var_dump(session_abort(), session_status(), file_get_contents(__DIR__ . "/sess_abort1"));
/// ob_end_flush();
/// ```
/// ```expect
/// bool(false)
///
/// Warning: session_save_path(): Session save path cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_id(): Session ID cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_decode(): Session data cannot be decoded when there is no active session in %s on line 3
///
/// Warning: file_get_contents(%SCRATCH%/sess_abort1): Failed to open stream: No such file or directory in %s on line 4
/// bool(false)
/// int(1)
/// bool(false)
/// ```
fn session_abort(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    if req.state::<State>().status != Status::Active {
        return Ok(Value::bool(false));
    }
    session::abort(req)?;
    answer(req, Value::bool(true))
}

/// `session_reset(): bool`: the variables read back from the handler.
///
/// ```php
/// var_dump(session_reset());
/// ob_start(); session_save_path(__DIR__); session_id("reset1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); session_decode("n|i:5;"); session_write_close();
/// session_start(); session_decode("n|i:6;"); var_dump(session_encode(), session_reset(), session_encode()); session_write_close();
/// ob_end_flush();
/// ```
/// ```expect
/// bool(false)
///
/// Warning: session_save_path(): Session save path cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_id(): Session ID cannot be changed after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 3
///
/// Warning: session_decode(): Session data cannot be decoded when there is no active session in %s on line 3
///
/// Warning: session_start(): Session cannot be started after headers have already been sent (sent from %s on line 2) in %s on line 4
///
/// Warning: session_decode(): Session data cannot be decoded when there is no active session in %s on line 4
///
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 4
///
/// Warning: session_encode(): Cannot encode non-existent session in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn session_reset(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    if req.state::<State>().status != Status::Active {
        return Ok(Value::bool(false));
    }
    session::reset(req)?;
    answer(req, Value::bool(true))
}

/// `session_status(): int`.
///
/// ```php
/// var_dump(session_status() === PHP_SESSION_NONE);
/// ```
/// ```expect
/// bool(true)
/// ```
fn session_status(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    Ok(Value::int(req.state::<State>().status as i64))
}

/// `session_register_shutdown(): void`: `session_write_close` registered
/// as a shutdown function.
///
/// ```php
/// ob_start(); session_save_path(__DIR__); session_id("shut1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); session_decode("s|i:1;");
/// session_register_shutdown();
/// register_shutdown_function(function () { var_dump(session_status()); ob_end_flush(); });
/// ```
/// ```expect
/// int(1)
/// ```
fn session_register_shutdown(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let callable = named(req, b"session_write_close")?;
    req.register_shutdown_function(callable, Vec::new())?;
    Ok(Value::null())
}

/// The callable a native's name is.
fn named(req: &mut Request, name: &[u8]) -> Result<Callable, Thrown> {
    let value = string_value(req, name)?;
    let callable = callable_checked(req, &value);
    match callable {
        Ok(callable) => Ok(callable),
        Err(message) => {
            req.release(value);
            Err(throw(req, "Error", &message))
        }
    }
}

/// `can_session_handler_be_changed`.
fn handler_changeable(req: &mut Request) -> bool {
    if req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session save handler cannot be changed when a session is active",
        );
        return false;
    }
    if headers_sent(req) {
        sent_error(
            req,
            "Session save handler cannot be changed after headers have already been sent",
        );
        return false;
    }
    true
}

/// `set_user_save_handler_ini`: `session.save_handler` set to `user`,
/// which only this may do.
fn user_ini(req: &mut Request) -> Result<(), Thrown> {
    req.state::<State>().set_handler = true;
    let set = crate::ini::alter(req, b"session.save_handler", b"user");
    req.state::<State>().set_handler = false;
    set.map(|_| ())
}

/// Whether the object's class declares or inherits `method`, and whether
/// it is an instance of the interface named.
fn class_has(req: &mut Request, object: php_ext::values::PhpObject, method: &[u8]) -> bool {
    let mut at = Some(object.class());
    for _ in 0..64 {
        let Some(here) = at else { return false };
        let Some(decl) = req.class_decl(here) else {
            return false;
        };
        if decl
            .methods
            .iter()
            .any(|m| m.function.name.0.eq_ignore_ascii_case(method))
        {
            return true;
        }
        let parent = decl.parent.as_ref().map(|p| p.0.to_vec());
        at = parent.and_then(|p| lookup_class(req, &p).ok().flatten());
    }
    false
}

fn implements(
    req: &mut Request,
    object: php_ext::values::PhpObject,
    interface: &[u8],
) -> Result<bool, Thrown> {
    Ok(lookup_class(req, interface)?.is_some_and(|class| object.instance_of(req, class)))
}

/// `session_set_save_handler(...)`: an object of `SessionHandlerInterface`
/// with `$register_shutdown`, its methods the callables; or, deprecated,
/// six to nine callables.
///
/// ```php
/// class M implements SessionHandlerInterface, SessionUpdateTimestampHandlerInterface {
///   public $d = []; function open($p, $n): bool { echo "open\n"; return true; } function close(): bool { echo "close\n"; return true; }
///   function read($id): string|false { echo "read $id\n"; return $this->d[$id] ?? ""; } function write($id, $data): bool { echo "write $id $data\n"; $this->d[$id] = $data; return true; }
///   function destroy($id): bool { return true; } function gc($l): int|false { return 0; }
///   function validateId($id): bool { echo "validate $id\n"; return true; } function updateTimestamp($id, $data): bool { echo "update $id\n"; return true; } }
/// ob_start(); $h = new M; var_dump(session_set_save_handler($h, false), session_module_name());
/// session_id("m1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); session_decode("a|i:1;"); session_write_close();
/// session_start(["use_strict_mode" => 1]); session_write_close();
/// var_dump(session_set_save_handler("strlen", "strlen", "strlen", "strlen", "strlen", "strlen"));
/// try { session_set_save_handler(1, 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ob_end_flush();
/// ```
/// ```expect
/// bool(true)
/// string(4) "user"
/// open
/// read m1
/// write m1 a|i:1;
/// close
/// open
/// validate m1
/// read m1
/// update m1
/// close
///
/// Deprecated: session_set_save_handler(): Providing individual callbacks instead of an object implementing SessionHandlerInterface is deprecated in %s on line 10
/// bool(true)
/// session_set_save_handler(): Argument #1 ($open) must be of type SessionHandlerInterface, int given
/// ```
///
/// The object form keeps `session_register_shutdown` under the name
/// `session_shutdown`, replacing an earlier one, so the session is
/// written by the shutdown functions, before the destructors; with
/// `$register_shutdown = false`, or in the callbacks' form, the entry is
/// removed and the session's `RSHUTDOWN` writes it, after them.
///
/// ```php
/// class H implements SessionHandlerInterface { function open($p, $n): bool { return true; } function close(): bool { return true; }
///   function read($id): string|false { return ""; } function write($id, $d): bool { echo "write\n"; return true; }
///   function destroy($id): bool { return true; } function gc($l): int|false { return 0; } }
/// class D { function __destruct() { echo "destruct\n"; } }
/// $d = new D; $h = new H; session_set_save_handler($h); session_id("sh1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// ```
/// ```expect
/// write
/// destruct
/// ```
///
/// ```php
/// class H implements SessionHandlerInterface { function open($p, $n): bool { return true; } function close(): bool { return true; }
///   function read($id): string|false { return ""; } function write($id, $d): bool { echo "write\n"; return true; }
///   function destroy($id): bool { return true; } function gc($l): int|false { return 0; } }
/// class D { function __destruct() { echo "destruct\n"; } }
/// $d = new D; $h = new H; session_set_save_handler($h); session_set_save_handler($h, false); session_id("sh1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// ```
/// ```expect
/// destruct
/// write
/// ```
#[allow(clippy::too_many_lines)] // php's function, both arms
fn session_set_save_handler(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count <= 2 {
        arguments_between(req, 1, 2)?;
        let first = req.argument(1).unwrap_or_default().deref();
        let is_handler = match first.as_object() {
            Some(object) => implements(req, *object, b"SessionHandlerInterface")?,
            None => false,
        };
        let Some(object) = first.as_object().copied().filter(|_| is_handler) else {
            let name = value_name(req, &first);
            return Err(arg_type_error(
                req,
                1,
                &format!("be of type SessionHandlerInterface, {name} given"),
            ));
        };
        let register = bool_argument(req, 2, true)?;
        if !handler_changeable(req) {
            return Ok(Value::bool(false));
        }
        let class_name = req.class_decl(object.class()).map(|d| d.name.0.to_vec());
        req.state::<State>().class_name = class_name;
        for (slot, name) in [
            (Slot::Open, b"open".as_slice()),
            (Slot::Close, b"close"),
            (Slot::Read, b"read"),
            (Slot::Write, b"write"),
            (Slot::Destroy, b"destroy"),
            (Slot::Gc, b"gc"),
        ] {
            let pair = method_of(req, first, name)?;
            store(req, slot, Some(pair));
            req.release(pair);
        }
        let optional =
            |req: &mut Request, slot: Slot, interface: &[u8], name: &[u8]| -> Result<(), Thrown> {
                store(req, slot, None);
                if implements(req, object, interface)? || class_has(req, object, name) {
                    let pair = method_of(req, first, name)?;
                    store(req, slot, Some(pair));
                    req.release(pair);
                }
                Ok(())
            };
        optional(req, Slot::CreateSid, b"SessionIdInterface", b"create_sid")?;
        optional(
            req,
            Slot::ValidateSid,
            b"SessionUpdateTimestampHandlerInterface",
            b"validateid",
        )?;
        optional(
            req,
            Slot::UpdateTimestamp,
            b"SessionUpdateTimestampHandlerInterface",
            b"updatetimestamp",
        )?;
        // `register_user_shutdown_function("session_shutdown", ...)`,
        // replacing an earlier one, or `remove_user_shutdown_function`.
        if register {
            let callable = named(req, b"session_register_shutdown")?;
            req.set_shutdown_function(b"session_shutdown", Some((callable, Vec::new())))?;
        } else {
            req.set_shutdown_function(b"session_shutdown", None)?;
        }
        let state = req.state::<State>();
        if state.status != Status::Active && state.module != Some(Module::User) {
            user_ini(req)?;
        }
        return Ok(Value::bool(true));
    }
    docref(
        req,
        Level::Deprecated,
        "Providing individual callbacks instead of an object implementing SessionHandlerInterface is deprecated",
    );
    arguments_between(req, 6, 9)?;
    let mut callables = Vec::new();
    for at in 1..=9u32 {
        let value = req.argument(at).map(|v| v.deref());
        let value = match value {
            Some(v) if at > 6 && matches!(v.as_ref(), ValueRef::Null) => None,
            Some(v) => Some(v),
            None => None,
        };
        if let Some(v) = &value
            && !is_callable(req, v)
        {
            let message = callable_checked(req, v).err().unwrap_or_default();
            return Err(arg_type_error(
                req,
                at,
                &format!("be a valid callback, {message}"),
            ));
        }
        callables.push(value);
    }
    if !handler_changeable(req) {
        return Ok(Value::bool(false));
    }
    req.state::<State>().class_name = None;
    req.set_shutdown_function(b"session_shutdown", None)?;
    if req.state::<State>().module != Some(Module::User) {
        user_ini(req)?;
    }
    let slots = [
        Slot::Open,
        Slot::Close,
        Slot::Read,
        Slot::Write,
        Slot::Destroy,
        Slot::Gc,
        Slot::CreateSid,
        Slot::ValidateSid,
        Slot::UpdateTimestamp,
    ];
    for (at, (slot, value)) in slots.into_iter().zip(callables).enumerate() {
        if at < 6 || value.is_some() {
            store(req, slot, value);
        }
    }
    Ok(Value::bool(true))
}
