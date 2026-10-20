//! The natives of `session.stub.php`, `PHP_FUNCTION(session_*)` of
//! `session.c` each: its parameters, its refusals and warnings, and the
//! state machine in `session` behind it.

use php_ext::coerce::c_str;
use std::ops::ControlFlow;

use php_ext::abi::{Callable, OrFalse, Passed, Path, Raw, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::call::lookup_class;
use php_ext::coerce::{callable_checked, is_callable, to_string, truthy, value_name};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, arity, docref, throw_error,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

use crate::user::{Slot, method_of, store};
use crate::{
    Module, State, Status, answer, headers_sent, ini, ini_bool, pending, sent_error, session,
    started_error, string_value,
};

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
#[php_function]
fn session_name(
    req: &mut Request,
    #[php(default = null)] name: Option<Path>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if name.is_some() && !can_change(req, "name", false) {
        return Ok(OrFalse::False);
    }
    let current = ini(req, b"session.name");
    let current = PhpString::new(req, &current)?;
    if let Some(name) = name
        && let Err(thrown) = crate::ini::alter_string(req, b"session.name", name.0)
    {
        current.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(current))
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
#[php_function]
fn session_module_name(
    req: &mut Request,
    #[php(default = null)] module: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let name = module.map(|module| module.as_bytes().to_vec());
    if name.is_some() && !can_change(req, "save handler module", false) {
        return Ok(OrFalse::False);
    }
    let current = req.state::<State>().module.map_or("", Module::name);
    let current = PhpString::new(req, current.as_bytes())?;
    let Some(name) = name else {
        return Ok(OrFalse::Value(current));
    };
    if c_str(&name).eq_ignore_ascii_case(b"user") {
        current.give_up(req);
        return Err(php_ext::raising::argument_error(
            req,
            "ValueError",
            1,
            "cannot be \"user\"",
        ));
    }
    if Module::find(&name).is_none() {
        current.give_up(req);
        docref(
            req,
            Level::Warning,
            &format!(
                "Session handler module \"{}\" cannot be found",
                crate::shown(&name)
            ),
        );
        return Ok(OrFalse::False);
    }
    if let Err(thrown) = session::close(req) {
        current.give_up(req);
        return Err(thrown);
    }
    req.state::<State>().files = None;
    if let Some(module) = module
        && let Err(thrown) = crate::ini::alter_string(req, b"session.save_handler", module)
    {
        current.give_up(req);
        return Err(thrown);
    }
    answer(req, OrFalse::Value(current))
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
#[php_function]
fn session_save_path(
    req: &mut Request,
    #[php(default = null)] path: Option<Path>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if path.is_some() && !can_change(req, "save path", false) {
        return Ok(OrFalse::False);
    }
    let current = ini(req, b"session.save_path");
    let current = PhpString::new(req, &current)?;
    if let Some(path) = path
        && let Err(thrown) = crate::ini::alter_string(req, b"session.save_path", path.0)
    {
        current.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(current))
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
#[php_function]
fn session_id(
    req: &mut Request,
    #[php(default = null)] id: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if id.is_some() && !can_change(req, "ID", true) {
        return Ok(OrFalse::False);
    }
    let current = req.state::<State>().id.clone().unwrap_or_default();
    let current = PhpString::new(req, c_str(&current))?;
    if let Some(id) = id {
        req.state::<State>().id = Some(id.as_bytes().to_vec());
    }
    Ok(OrFalse::Value(current))
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
#[php_function]
fn session_regenerate_id(
    req: &mut Request,
    #[php(default = false)] delete_old_session: bool,
) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session ID cannot be regenerated when there is no active session",
        );
        return Ok(false);
    }
    if headers_sent(req) {
        sent_error(
            req,
            "Session ID cannot be regenerated after headers have already been sent",
        );
        return Ok(false);
    }
    let done = regenerate(req, delete_old_session)?;
    answer(req, done)
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
#[php_function]
fn session_create_id(
    req: &mut Request,
    #[php(default = "")] prefix: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let prefix = prefix.0.as_bytes().to_vec();
    if !prefix.is_empty() {
        if prefix.len() > 256 {
            return Err(php_ext::raising::argument_error(
                req,
                "ValueError",
                1,
                "cannot be longer than 256 characters",
            ));
        }
        if !session::valid_key(&prefix) {
            docref(
                req,
                Level::Warning,
                "Prefix cannot contain special characters. Only the A-Z, a-z, 0-9, \"-\", and \",\" characters are allowed",
            );
            return Ok(OrFalse::False);
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
        return answer(req, OrFalse::False);
    };
    let mut id = prefix;
    id.extend_from_slice(&new_id);
    let id = PhpString::new(req, &id)?;
    answer(req, OrFalse::Value(id))
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
#[php_function]
fn session_cache_limiter(
    req: &mut Request,
    #[php(default = null)] value: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if value.is_some() && req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session cache limiter cannot be changed when a session is active",
        );
        return Ok(OrFalse::False);
    }
    if value.is_some() && headers_sent(req) {
        sent_error(
            req,
            "Session cache limiter cannot be changed after headers have already been sent",
        );
        return Ok(OrFalse::False);
    }
    let current = ini(req, b"session.cache_limiter");
    let current = PhpString::new(req, &current)?;
    if let Some(limiter) = value
        && let Err(thrown) = crate::ini::alter_string(req, b"session.cache_limiter", limiter)
    {
        current.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(current))
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
#[php_function]
fn session_cache_expire(
    req: &mut Request,
    #[php(default = null)] value: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let expires = value;
    let current = req.ini().long(b"session.cache_expire");
    if expires.is_some() && req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session cache expiration cannot be changed when a session is active",
        );
        return Ok(OrFalse::Value(current));
    }
    if expires.is_some() && headers_sent(req) {
        sent_error(
            req,
            "Session cache expiration cannot be changed after headers have already been sent",
        );
        return Ok(OrFalse::False);
    }
    if let Some(expires) = expires {
        let expires = php_ext::coerce::int_to_string(req, expires)?;
        crate::ini::alter_made(req, b"session.cache_expire", expires)?;
    }
    Ok(OrFalse::Value(current))
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
#[php_function]
fn session_encode(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let encoded = session::encode(req)?;
    let value = match encoded {
        Some(bytes) => OrFalse::Value(PhpString::new(req, &bytes)?),
        None => OrFalse::False,
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
#[php_function]
fn session_decode(req: &mut Request, data: PhpString) -> Result<bool, Thrown> {
    let data = data.as_bytes().to_vec();
    if req.state::<State>().status != Status::Active {
        docref(
            req,
            Level::Warning,
            "Session data cannot be decoded when there is no active session",
        );
        return Ok(false);
    }
    let ok = session::decode(req, &data)?;
    answer(req, ok)
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
#[php_function]
fn session_start(
    req: &mut Request,
    #[php(default = [])] options: PhpArray,
) -> Result<bool, Thrown> {
    if req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Notice,
            "Ignoring session_start() because a session is already active",
        );
        return Ok(true);
    }
    if ini_bool(req, b"session.use_cookies") && headers_sent(req) {
        sent_error(
            req,
            "Session cannot be started after headers have already been sent",
        );
        return Ok(false);
    }
    let mut read_and_close = false;
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(options), str_idx, value)`
    // (`Z/ext/session/session.c:2654`), no owner of the table taken: the
    // argument holds it while a `TypeError` reaches the program.
    walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
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
                return Err(throw_error(
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
                        return Err(throw_error(
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
            return Ok(ControlFlow::Continue(()));
        }
        let text = to_string(req, &value)?;
        let mut entry = b"session.".to_vec();
        entry.extend_from_slice(&name);
        if !crate::ini::alter_made(req, &entry, text)? {
            docref(
                req,
                Level::Warning,
                &format!("Setting option \"{}\" failed", crate::shown(&name)),
            );
        }
        Ok(ControlFlow::Continue(()))
    })?;
    session::start(req)?;
    if req.state::<State>().status != Status::Active {
        clean(req)?;
        return answer(req, false);
    }
    if read_and_close {
        session::flush(req, false)?;
    }
    answer(req, true)
}

/// `SEPARATE_ARRAY` and `zend_hash_clean` over the session's array, where
/// there is one (`Z/ext/session/session.c:2702`): no owner of the table
/// taken, the session's reference holding it.
fn clean(req: &mut Request) -> Result<(), Thrown> {
    match crate::session_array_mut(req)? {
        Some(mut array) => array.clean(req),
        None => Ok(()),
    }
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
#[php_function]
fn session_destroy(req: &mut Request) -> Result<bool, Thrown> {
    let ok = session::destroy(req)?;
    answer(req, ok)
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
///
/// The variables go as `zend_hash_clean` destroys them: each value in
/// order, its destructor served as it goes, every key standing until
/// the last is destroyed, and the next free index back to the start.
///
/// ```php
/// ob_start(); session_start(["save_path" => sys_get_temp_dir(), "use_cookies" => 0, "cache_limiter" => ""]);
/// class D { function __construct(public $n) {} function __destruct() { echo "d{$this->n}\n"; } }
/// class E { function __destruct() { echo "E sees ", implode(',', array_keys($_SESSION)), "\n"; } }
/// $_SESSION['a'] = new D(1); $_SESSION['b'] = new D(2); $_SESSION['e'] = new E; $_SESSION[5] = new D(3);
/// var_dump(session_unset()); $_SESSION[] = 0; var_dump($_SESSION);
/// session_destroy(); ob_end_flush();
/// ```
/// ```expect
/// d1
/// d2
/// E sees a,b,e,5
/// d3
/// bool(true)
/// array(1) {
///   [0]=>
///   int(0)
/// }
/// ```
#[php_function]
fn session_unset(req: &mut Request) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        return Ok(false);
    }
    clean(req)?;
    Ok(true)
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
#[php_function]
fn session_gc(req: &mut Request) -> Result<OrFalse<i64>, Thrown> {
    if req.state::<State>().status != Status::Active {
        docref(
            req,
            Level::Warning,
            "Session cannot be garbage collected when there is no active session",
        );
        return Ok(OrFalse::False);
    }
    let n = session::gc(req, true)?;
    answer(
        req,
        if n < 0 {
            OrFalse::False
        } else {
            OrFalse::Value(n)
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
#[php_function]
fn session_get_cookie_params(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
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
            0 => Owner::int(req.ini().long(entry)),
            1 => {
                let text = ini(req, entry);
                string_value(req, &text)?
            }
            _ => Owner::bool(ini_bool(req, entry)),
        };
        array.set_str(req, key, value)?;
    }
    Ok(array)
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
///
/// Each entry keeps the string it is handed: an argument's or an
/// option's one more owner, a number's or a boolean's a new one
/// (`Z/ext/session/session.c:1907-1970`):
///
/// ```php
/// ob_start(); ini_set('from', 'x');
/// $v = str_repeat('/', 40); $m = memory_get_usage(); session_set_cookie_params(['path' => $v, 'lifetime' => 100, 'secure' => true]); unset($v); echo memory_get_usage() - $m, "\n";
/// $v = str_repeat('/', 40); $m = memory_get_usage(); session_set_cookie_params(5, $v); unset($v); echo memory_get_usage() - $m, "\n";
/// $v = str_repeat('a', 40); $m = memory_get_usage(); session_name($v); session_cache_expire(100); unset($v); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 64
/// -112
/// 32
/// ```
///
/// A key given twice keeps the last, and the first's string lives to the
/// request's end, as php overwrites it unreleased
/// (`Z/ext/session/session.c:1859-1885`):
///
/// ```php
/// ob_start(); ini_set('from', 'x');
/// $m = memory_get_usage(); session_set_cookie_params(['path' => 123456, 'PATH' => 654321]); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); session_set_cookie_params(['path' => 123456]); echo memory_get_usage() - $m, "\n";
/// $a = str_repeat('a', 40); $m = memory_get_usage(); session_set_cookie_params(['domain' => $a, 'Domain' => 'x']); unset($a); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 64
/// 0
/// 0
/// ```
#[allow(clippy::too_many_lines)] // php's function
#[php_function]
fn session_set_cookie_params(
    req: &mut Request,
    lifetime_or_options: Union<PhpArray, i64>,
    #[php(default = null)] path: Option<PhpString>,
    #[php(default = null)] domain: Option<PhpString>,
    #[php(default = null)] secure: Option<bool>,
    #[php(default = null)] httponly: Option<bool>,
) -> Result<bool, Thrown> {
    let (options, lifetime_long) = match lifetime_or_options {
        Union::A(array) => (Some(array), 0),
        Union::B(lifetime) => (None, lifetime),
    };
    let mut path = path.map(Cookie::Held);
    let mut domain = domain.map(Cookie::Held);
    let (mut secure, mut httponly) = (secure, httponly);
    if !ini_bool(req, b"session.use_cookies") {
        docref(
            req,
            Level::Warning,
            "Session cookies cannot be used when session.use_cookies is disabled",
        );
        return Ok(false);
    }
    if req.state::<State>().status == Status::Active {
        started_error(
            req,
            Level::Warning,
            "Session cookie parameters cannot be changed when a session is active",
        );
        return Ok(false);
    }
    if headers_sent(req) {
        sent_error(
            req,
            "Session cookie parameters cannot be changed after headers have already been sent",
        );
        return Ok(false);
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
        // `ZEND_HASH_FOREACH_STR_KEY_VAL(options_ht, key, value)`
        // (`Z/ext/session/session.c:1860`), no owner of the table taken: the
        // argument holds it while `zval_get_string` runs `__toString`.
        let walked = walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
            let key = row.key().clone();
            let value = row.value().deref();
            let Key::Str(key) = key else {
                docref(
                    req,
                    Level::Warning,
                    "Argument #1 ($lifetime_or_options) cannot contain numeric keys",
                );
                return Ok(ControlFlow::Continue(()));
            };
            let key = key.as_bytes().to_ascii_lowercase();
            let made = match key.as_slice() {
                b"lifetime" | b"path" | b"domain" | b"samesite" => match to_string(req, &value) {
                    Ok(made) => Some(Cookie::Made(made)),
                    Err(thrown) => return Err(thrown),
                },
                _ => None,
            };
            let slot = match key.as_slice() {
                b"lifetime" => Some(&mut lifetime),
                b"path" => Some(&mut path),
                b"domain" => Some(&mut domain),
                b"samesite" => Some(&mut samesite),
                _ => None,
            };
            if let (Some(slot), Some(made)) = (slot, made) {
                // A key given twice keeps the last; php overwrites the
                // first without releasing it, so it lives to the request's
                // end, as `memory_get_usage()` reads it, on the state.
                if let Some(Cookie::Made(first)) = slot.replace(made) {
                    req.state::<State>().overwritten.push(first);
                }
                found += 1;
                return Ok(ControlFlow::Continue(()));
            }
            match key.as_slice() {
                b"secure" => secure = Some(truthy(req, &value)?),
                b"partitioned" => partitioned = Some(truthy(req, &value)?),
                b"httponly" => httponly = Some(truthy(req, &value)?),
                _ => {
                    docref(
                        req,
                        Level::Warning,
                        &format!(
                            "Argument #1 ($lifetime_or_options) contains an unrecognized key \"{}\"",
                            crate::shown(&key)
                        ),
                    );
                    return Ok(ControlFlow::Continue(()));
                }
            }
            found += 1;
            Ok(ControlFlow::Continue(()))
        });
        if let Err(thrown) = walked {
            for set in [lifetime, path, domain, samesite].into_iter().flatten() {
                set.release(req);
            }
            return Err(thrown);
        }
        if found == 0 {
            return Err(arg_value_error(req, 1, "contain at least 1 valid key"));
        }
    } else {
        lifetime = Some(Cookie::Made(php_ext::coerce::int_to_string(
            req,
            lifetime_long,
        )?));
    }
    let flag = |b: bool| Cookie::Chars(if b { b"1" } else { b"0" });
    let settings: [(&[u8], Option<Cookie>); 7] = [
        (b"session.cookie_lifetime", lifetime),
        (b"session.cookie_path", path),
        (b"session.cookie_domain", domain),
        (b"session.cookie_secure", secure.map(flag)),
        (b"session.cookie_partitioned", partitioned.map(flag)),
        (b"session.cookie_httponly", httponly.map(flag)),
        (b"session.cookie_samesite", samesite),
    ];
    // php's `cleanup`: every string made for the call released, whether
    // an entry refused its value or not.
    let mut result = Ok(true);
    for (entry, value) in settings {
        let Some(value) = value else { continue };
        if matches!(result, Ok(true)) {
            result = match &value {
                Cookie::Held(string) => crate::ini::alter_string(req, entry, *string),
                Cookie::Made(string) => crate::ini::alter_string(req, entry, **string),
                Cookie::Chars(bytes) => crate::ini::alter(req, entry, bytes),
            };
        }
        value.release(req);
    }
    result
}

/// One value `session_set_cookie_params()` hands `zend_alter_ini_entry`:
/// an argument's string, which stays the caller's, one `zval_get_string`
/// or `zend_long_to_str` made, released after, or the bytes
/// `zend_alter_ini_entry_chars` makes a string of.
enum Cookie {
    Held(PhpString),
    Made(Owner<PhpString>),
    Chars(&'static [u8]),
}

impl Cookie {
    /// The string made for the call released; an argument's stays.
    fn release(self, req: &mut Request) {
        if let Cookie::Made(string) = self {
            string.give_up(req);
        }
    }
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
#[php_function(aliases(session_commit))]
fn session_write_close(req: &mut Request) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        return Ok(false);
    }
    session::flush(req, true)?;
    answer(req, true)
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
#[php_function]
fn session_abort(req: &mut Request) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        return Ok(false);
    }
    session::abort(req)?;
    answer(req, true)
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
#[php_function]
fn session_reset(req: &mut Request) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        return Ok(false);
    }
    session::reset(req)?;
    answer(req, true)
}

/// `session_status(): int`.
///
/// ```php
/// var_dump(session_status() === PHP_SESSION_NONE);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn session_status(req: &mut Request) -> i64 {
    req.state::<State>().status as i64
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
#[php_function]
fn session_register_shutdown(req: &mut Request) -> Result<(), Thrown> {
    let callable = named(req, b"session_write_close")?;
    let callable = Owner::take(req, callable);
    req.code_mut()
        .register_shutdown_function(callable, Vec::new())
}

/// The callable a native's name is: the name the bundle declared,
/// interned and owned by no count, as php's entry holds the function
/// alone (`fci_cache.function_handler`).
fn named(req: &mut Request, name: &[u8]) -> Result<Callable, Thrown> {
    let value = Value::string(PhpString::interned(req, name)?);
    callable_checked(req, &value).map_err(|message| throw_error(req, "Error", &message))
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
    // `ZSTR_KNOWN(ZEND_STR_USER)`, which costs nothing.
    let user = PhpString::interned(req, b"user")?;
    let user = Owner::take(req, user);
    let set = crate::ini::alter_made(req, b"session.save_handler", user);
    req.state::<State>().set_handler = false;
    set.map(|_| ())
}

/// Whether the object's class declares or inherits `method`, and whether
/// it is an instance of the interface named.
fn class_has(req: &mut Request, object: php_ext::values::PhpObject, method: &[u8]) -> bool {
    req.symbols().method_row(object.class(), method).is_some()
}

fn implements(
    req: &mut Request,
    object: php_ext::values::PhpObject,
    interface: &[u8],
) -> Result<bool, Thrown> {
    Ok(lookup_class(req, interface)?.is_some_and(|class| object.instance_of(req, class)))
}

/// A `Passed` argument of the callbacks form as the call gave it.
fn passed<T: php_ext::abi::FromArgument>(argument: Passed<Raw<T>>) -> Option<Value> {
    match argument {
        Passed::Given(raw) => Some(raw.value()),
        Passed::Omitted => None,
    }
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
#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // php's function, both arms
#[php_function(arity = manual)]
fn session_set_save_handler(
    req: &mut Request,
    open: Raw<Untyped<Value>>,
    close: Passed<Raw<Untyped<bool>>>,
    read: Passed<Raw<Callable>>,
    write: Passed<Raw<Callable>>,
    destroy: Passed<Raw<Callable>>,
    gc: Passed<Raw<Callable>>,
    #[php(default = null)] create_sid: Raw<Option<Callable>>,
    #[php(default = null)] validate_sid: Raw<Option<Callable>>,
    #[php(default = null)] update_timestamp: Raw<Option<Callable>>,
) -> Result<bool, Thrown> {
    let count = req.frames().argument_count();
    if count <= 2 {
        arity(req, 1, Some(2))?;
        let first = open.value();
        let is_handler = match first.as_object() {
            Some(object) => implements(req, object, b"SessionHandlerInterface")?,
            None => false,
        };
        let Some(object) = first.as_object().filter(|_| is_handler) else {
            let name = value_name(req, &first);
            return Err(arg_type_error(
                req,
                1,
                &format!("be of type SessionHandlerInterface, {name} given"),
            ));
        };
        let register = match close {
            Passed::Given(register) => register.hold(req)?.0,
            Passed::Omitted => true,
        };
        if !handler_changeable(req) {
            return Ok(false);
        }
        let class_name = req
            .symbols()
            .class_decl(object.class())
            .map(|d| d.name.0.to_vec());
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
        }
        let optional =
            |req: &mut Request, slot: Slot, interface: &[u8], name: &[u8]| -> Result<(), Thrown> {
                store(req, slot, None);
                if implements(req, object, interface)? || class_has(req, object, name) {
                    let pair = method_of(req, first, name)?;
                    store(req, slot, Some(pair));
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
            let callable = Owner::take(req, callable);
            req.code_mut()
                .set_shutdown_function(b"session_shutdown", Some((callable, Vec::new())))?;
        } else {
            req.code_mut()
                .set_shutdown_function(b"session_shutdown", None)?;
        }
        let state = req.state::<State>();
        if state.status != Status::Active && state.module != Some(Module::User) {
            user_ini(req)?;
        }
        return Ok(true);
    }
    docref(
        req,
        Level::Deprecated,
        "Providing individual callbacks instead of an object implementing SessionHandlerInterface is deprecated",
    );
    arity(req, 6, Some(9))?;
    let given = [
        Some(open.value()),
        passed(close),
        passed(read),
        passed(write),
        passed(destroy),
        passed(gc),
        Some(create_sid.value()),
        Some(validate_sid.value()),
        Some(update_timestamp.value()),
    ];
    let mut callables = Vec::new();
    for (at, value) in (1..=9u32).zip(given) {
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
        return Ok(false);
    }
    req.state::<State>().class_name = None;
    req.code_mut()
        .set_shutdown_function(b"session_shutdown", None)?;
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
            let value = value.map(|value| Owner::take(req, value));
            store(req, slot, value);
        }
    }
    Ok(true)
}
