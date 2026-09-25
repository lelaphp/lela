//! `mod_user.c` and `mod_user_class.c`: the `user` save handler, each
//! operation a call of the callable `session_set_save_handler` stored,
//! its answer held to `bool` as `verify_bool_return_type_userland_calls`
//! holds it; the three interfaces of `session.stub.php`; and
//! `SessionHandler`, whose methods call the handler that stood before
//! `user`, `PS(default_mod)`.

use php_ext::call::call;
use php_ext::coerce::{callable_checked, value_name};
use php_ext::decl::{ClassDecl, ClassKind, MethodDecl, ParameterDefault, Type};
use php_ext::raising::{Level, Thrown, docref, throw};
use php_ext::request::Request;
use php_ext::values::{PhpArray, Value, ValueRef};

use crate::{
    Entry, Module, State, Status, absorb, answer, arguments_between, class, coerced, declared,
    files, method, parameter, pending, session, string_value,
};

/// `PS(mod_user_names)`: the callables, each a value the request holds
/// one owner of.
#[derive(Default)]
pub(crate) struct Handlers {
    pub(crate) open: Option<Value>,
    pub(crate) close: Option<Value>,
    pub(crate) read: Option<Value>,
    pub(crate) write: Option<Value>,
    pub(crate) destroy: Option<Value>,
    pub(crate) gc: Option<Value>,
    pub(crate) create_sid: Option<Value>,
    pub(crate) validate_sid: Option<Value>,
    pub(crate) update_timestamp: Option<Value>,
}

/// Which of the nine callables.
#[derive(Clone, Copy)]
pub(crate) enum Slot {
    Open,
    Close,
    Read,
    Write,
    Destroy,
    Gc,
    CreateSid,
    ValidateSid,
    UpdateTimestamp,
}

impl Handlers {
    pub(crate) const fn is_set(&self) -> bool {
        self.open.is_some()
    }

    pub(crate) const fn slot(&mut self, slot: Slot) -> &mut Option<Value> {
        match slot {
            Slot::Open => &mut self.open,
            Slot::Close => &mut self.close,
            Slot::Read => &mut self.read,
            Slot::Write => &mut self.write,
            Slot::Destroy => &mut self.destroy,
            Slot::Gc => &mut self.gc,
            Slot::CreateSid => &mut self.create_sid,
            Slot::ValidateSid => &mut self.validate_sid,
            Slot::UpdateTimestamp => &mut self.update_timestamp,
        }
    }

    /// Every callable given up, `SESSION_FREE_USER_HANDLER`.
    pub(crate) fn release(self, req: &mut Request) {
        for value in [
            self.open,
            self.close,
            self.read,
            self.write,
            self.destroy,
            self.gc,
            self.create_sid,
            self.validate_sid,
            self.update_timestamp,
        ]
        .into_iter()
        .flatten()
        {
            req.release(value);
        }
    }
}

/// One callable stored, one owner of `value` taken, what stood there
/// given up.
pub(super) fn store(req: &mut Request, slot: Slot, value: Option<Value>) {
    if let Some(value) = &value {
        req.addref(value);
    }
    let old = std::mem::replace(req.state::<State>().handlers.slot(slot), value);
    if let Some(old) = old {
        req.release(old);
    }
}

/// `ps_call_handler`: the callable called with `arguments`, which it
/// gives up after; `None` is php's `UNDEF`, for a call that threw, a
/// recursive call refused with php's warning, and a throwable already
/// pending, as `zend_call_function` refuses to call then.
fn call_handler(
    req: &mut Request,
    slot: Slot,
    arguments: &[Value],
) -> Result<Option<Value>, Thrown> {
    let callable = *req.state::<State>().handlers.slot(slot);
    let done = if req.state::<State>().in_save_handler {
        req.state::<State>().in_save_handler = false;
        docref(
            req,
            Level::Warning,
            "Cannot call session save handler in a recursive manner",
        );
        Ok(None)
    } else if let Some(callable) = callable
        && !pending(req)
        && let Ok(callable) = callable_checked(req, &callable)
    {
        req.state::<State>().in_save_handler = true;
        let answered = call(req, &callable, arguments);
        req.state::<State>().in_save_handler = false;
        absorb(req, answered)
    } else {
        Ok(None)
    };
    for argument in arguments {
        req.release(*argument);
    }
    done
}

/// `verify_bool_return_type_userland_calls`: `true` is success, `false`
/// failure, `0` and `-1` the deprecated ints, anything else php's
/// `TypeError`.
fn verify_bool(req: &mut Request, answer: Option<Value>) -> bool {
    let Some(value) = answer else {
        return false;
    };
    let verdict = match value.as_ref() {
        ValueRef::Bool(ok) => Some(ok),
        ValueRef::Int(n @ (0 | -1)) => {
            if !pending(req) {
                let name = value_name(req, &value);
                docref(
                    req,
                    Level::Deprecated,
                    &format!(
                        "Session callback must have a return value of type bool, {name} returned"
                    ),
                );
            }
            Some(n == 0)
        }
        _ => None,
    };
    let verdict = verdict.unwrap_or_else(|| {
        if !pending(req) {
            let name = value_name(req, &value);
            let thrown = throw(
                req,
                "TypeError",
                &format!("Session callback must have a return value of type bool, {name} returned"),
            );
            req.state::<State>().thrown = Some(thrown);
        }
        false
    });
    req.release(value);
    verdict
}

/// `PS_OPEN_FUNC(user)`: `open($path, $name)`, and the handler counted
/// open whatever it answered.
pub(super) fn open(req: &mut Request, path: &[u8], name: &[u8]) -> Result<bool, Thrown> {
    let arguments = [string_value(req, path)?, string_value(req, name)?];
    let answered = call_handler(req, Slot::Open, &arguments);
    req.state::<State>().user_open = true;
    let answered = answered?;
    Ok(verify_bool(req, answered))
}

/// `PS_CLOSE_FUNC(user)`: `close()` once the handler is open.
pub(super) fn close(req: &mut Request) -> Result<(), Thrown> {
    if !req.state::<State>().user_open {
        return Ok(());
    }
    let answered = call_handler(req, Slot::Close, &[]);
    req.state::<State>().user_open = false;
    let answered = answered?;
    verify_bool(req, answered);
    Ok(())
}

/// `PS_READ_FUNC(user)`: `read($id)`, whose string is the data.
pub(super) fn read(req: &mut Request, id: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let arguments = [string_value(req, id)?];
    let Some(value) = call_handler(req, Slot::Read, &arguments)? else {
        return Ok(None);
    };
    let data = value.as_str_bytes().map(<[u8]>::to_vec);
    req.release(value);
    Ok(data)
}

fn two(req: &mut Request, slot: Slot, id: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    let arguments = [string_value(req, id)?, string_value(req, val)?];
    let answered = call_handler(req, slot, &arguments)?;
    Ok(verify_bool(req, answered))
}

/// `PS_WRITE_FUNC(user)`: `write($id, $data)`.
pub(super) fn write(req: &mut Request, id: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    two(req, Slot::Write, id, val)
}

/// `PS_UPDATE_TIMESTAMP_FUNC(user)`: `updateTimestamp($id, $data)`, or
/// `write` where the handler has none.
pub(super) fn update_timestamp(req: &mut Request, id: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    let slot = if req.state::<State>().handlers.update_timestamp.is_some() {
        Slot::UpdateTimestamp
    } else {
        Slot::Write
    };
    two(req, slot, id, val)
}

/// `PS_DESTROY_FUNC(user)`: `destroy($id)`.
pub(super) fn destroy(req: &mut Request, id: &[u8]) -> Result<bool, Thrown> {
    let arguments = [string_value(req, id)?];
    let answered = call_handler(req, Slot::Destroy, &arguments)?;
    Ok(verify_bool(req, answered))
}

/// `PS_GC_FUNC(user)`: `gc($max_lifetime)`, an int the count, `true`
/// one, anything else `-1`.
pub(super) fn gc(req: &mut Request, maxlifetime: i64) -> Result<i64, Thrown> {
    let answered = call_handler(req, Slot::Gc, &[Value::int(maxlifetime)])?;
    let Some(value) = answered else {
        return Ok(-1);
    };
    let count = match value.as_ref() {
        ValueRef::Int(n) => n,
        ValueRef::Bool(true) => 1,
        _ => -1,
    };
    req.release(value);
    Ok(count)
}

/// `PS_CREATE_SID_FUNC(user)`: `create_sid()` where the handler has
/// one, php's own id otherwise.
pub(super) fn create_sid(req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
    if req.state::<State>().handlers.create_sid.is_none() {
        return Ok(session::create_id(req));
    }
    let Some(value) = call_handler(req, Slot::CreateSid, &[])? else {
        crate::throw_error(req, "No session id returned by function");
        return Ok(None);
    };
    let id = value.as_str_bytes().map(<[u8]>::to_vec);
    req.release(value);
    if id.is_none() {
        crate::throw_error(req, "Session id must be a string");
    }
    Ok(id)
}

/// `PS_VALIDATE_SID_FUNC(user)`: `validateId($id)` where the handler has
/// one; every id is valid otherwise.
pub(super) fn validate_sid(req: &mut Request, id: &[u8]) -> Result<bool, Thrown> {
    if req.state::<State>().handlers.validate_sid.is_none() {
        return Ok(true);
    }
    let arguments = [string_value(req, id)?];
    let answered = call_handler(req, Slot::ValidateSid, &arguments)?;
    Ok(verify_bool(req, answered))
}

// ------------------------------------------------------------ classes

/// The interfaces and `SessionHandler` in `PHP_MINIT_FUNCTION(session)`'s
/// order, the class's methods paired with their entries.
///
/// ```php
/// var_dump((new ReflectionClass("SessionHandler"))->getInterfaceNames(), (string) (new ReflectionMethod("SessionHandlerInterface", "gc"))->getTentativeReturnType(), interface_exists("SessionIdInterface"));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(23) "SessionHandlerInterface"
///   [1]=>
///   string(18) "SessionIdInterface"
/// }
/// string(9) "int|false"
/// bool(true)
/// ```
pub(super) fn classes() -> Vec<(ClassDecl, Vec<(MethodDecl, Entry)>)> {
    let typed = |name: &[u8], ty: Type| parameter(name, Some(ty), ParameterDefault::None);
    let string = |name: &[u8]| typed(name, Type::String);
    let string_or_false = || Type::Union(vec![Type::String, Type::False]);
    let int_or_false = || Type::Union(vec![Type::Int, Type::False]);
    let handler: [(&[u8], Vec<_>, Type, Entry); 6] = [
        (
            b"open",
            vec![string(b"path"), string(b"name")],
            Type::Bool,
            handler_open,
        ),
        (b"close", Vec::new(), Type::Bool, handler_close),
        (
            b"read",
            vec![string(b"id")],
            string_or_false(),
            handler_read,
        ),
        (
            b"write",
            vec![string(b"id"), string(b"data")],
            Type::Bool,
            handler_write,
        ),
        (b"destroy", vec![string(b"id")], Type::Bool, handler_destroy),
        (
            b"gc",
            vec![typed(b"max_lifetime", Type::Int)],
            int_or_false(),
            handler_gc,
        ),
    ];
    let abstract_of = |rows: &[(&[u8], Vec<php_ext::decl::Parameter>, Type)]| {
        rows.iter()
            .map(|(name, parameters, returns)| {
                method(declared(name, parameters.clone(), returns.clone()), true)
            })
            .collect::<Vec<_>>()
    };
    let interface_rows: Vec<_> = handler
        .iter()
        .map(|(n, p, t, _)| (*n, p.clone(), t.clone()))
        .collect();
    let mut methods: Vec<(MethodDecl, Entry)> = handler
        .into_iter()
        .map(|(name, parameters, returns, entry)| {
            (method(declared(name, parameters, returns), false), entry)
        })
        .collect();
    methods.push((
        method(declared(b"create_sid", Vec::new(), Type::String), false),
        handler_create_sid,
    ));
    vec![
        (
            class(
                b"SessionHandlerInterface",
                ClassKind::Interface,
                &[],
                abstract_of(&interface_rows),
            ),
            Vec::new(),
        ),
        (
            class(
                b"SessionIdInterface",
                ClassKind::Interface,
                &[],
                abstract_of(&[(b"create_sid", Vec::new(), Type::String)]),
            ),
            Vec::new(),
        ),
        (
            class(
                b"SessionUpdateTimestampHandlerInterface",
                ClassKind::Interface,
                &[],
                abstract_of(&[
                    (b"validateId", vec![string(b"id")], Type::Bool),
                    (
                        b"updateTimestamp",
                        vec![string(b"id"), string(b"data")],
                        Type::Bool,
                    ),
                ]),
            ),
            Vec::new(),
        ),
        (
            class(
                b"SessionHandler",
                ClassKind::Class,
                &[b"SessionHandlerInterface", b"SessionIdInterface"],
                Vec::new(),
            ),
            methods,
        ),
    ]
}

/// `PS_SANITY_CHECK`: an active session and a handler to call, else
/// php's `Error`; with `open`, the parent opened too, else php's warning
/// and `false`.
fn sanity(req: &mut Request, open: bool) -> Result<Option<Module>, Thrown> {
    let state = req.state::<State>();
    if state.status != Status::Active {
        return Err(throw(req, "Error", "Session is not active"));
    }
    let Some(module) = state.default_module else {
        return Err(throw(req, "Error", "Cannot call default session handler"));
    };
    if open && !state.parent_open {
        docref(req, Level::Warning, "Parent session handler is not open");
        return Ok(None);
    }
    Ok(Some(module))
}

/// A string parameter, `Z_PARAM_STR`.
fn string_at(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    Ok(coerced(req, position, &Type::String)?
        .as_str_bytes()
        .unwrap_or_default()
        .to_vec())
}

/// `SessionHandler::open(string $path, string $name): bool`: the
/// default handler opened on the path and name given.
///
/// ```php
/// class H implements SessionHandlerInterface { public $p; function __construct() { $this->p = new SessionHandler; }
///   function open($p, $n): bool { echo "open ", $p === __DIR__ ? "dir" : $p, " $n\n"; return $this->p->open($p, $n); } function read($id): string|false { echo "read\n"; return $this->p->read($id); }
///   function write($id, $d): bool { echo "write $d\n"; return $this->p->write($id, $d); } function close(): bool { echo "close\n"; return $this->p->close(); }
///   function destroy($id): bool { return $this->p->destroy($id); } function gc($l): int|false { return $this->p->gc($l); } }
/// ob_start(); session_set_save_handler(new H); session_save_path(__DIR__); session_id("handler1"); var_dump(session_start(["use_cookies" => 0]));
/// session_decode('k|i:1;'); session_write_close(); echo file_get_contents(__DIR__ . "/sess_handler1"), "\n";
/// try { (new SessionHandler)->open("", ""); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ob_end_flush();
/// ```
/// ```expect
/// open dir PHPSESSID
/// read
/// bool(true)
/// write k|i:1;
/// close
/// k|i:1;
/// Session is not active
/// ```
fn handler_open(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 2, 2)?;
    let path = string_at(req, 1)?;
    let _name = string_at(req, 2)?;
    let Some(module) = sanity(req, false)? else {
        return Ok(Value::bool(false));
    };
    let opened = match module {
        Module::Files => files::open(req, &path),
        Module::User => false,
    };
    if opened {
        req.state::<State>().parent_open = true;
    }
    answer(req, Value::bool(opened))
}

/// `SessionHandler::close(): bool`.
fn handler_close(req: &mut Request) -> Result<Value, Thrown> {
    let Some(module) = sanity(req, true)? else {
        return Ok(Value::bool(false));
    };
    req.state::<State>().parent_open = false;
    if module == Module::Files {
        files::close(req);
    }
    Ok(Value::bool(true))
}

/// `SessionHandler::read(string $id): string|false`.
fn handler_read(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 1)?;
    let id = string_at(req, 1)?;
    let Some(module) = sanity(req, true)? else {
        return Ok(Value::bool(false));
    };
    let data = match module {
        Module::Files => files::read(req, &id)?,
        Module::User => None,
    };
    match data {
        Some(data) => string_value(req, &data),
        None => Ok(Value::bool(false)),
    }
}

/// `SessionHandler::write(string $id, string $data): bool`.
fn handler_write(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 2, 2)?;
    let id = string_at(req, 1)?;
    let data = string_at(req, 2)?;
    let Some(module) = sanity(req, true)? else {
        return Ok(Value::bool(false));
    };
    Ok(Value::bool(
        module == Module::Files && files::write(req, &id, &data)?,
    ))
}

/// `SessionHandler::destroy(string $id): bool`.
///
/// ```php
/// ob_start(); session_set_save_handler(new SessionHandler); session_save_path(__DIR__); session_id("gone1");
/// session_start(["use_cookies" => 0]); session_write_close(); var_dump(file_exists(__DIR__ . "/sess_gone1"));
/// session_start(); var_dump(session_destroy(), file_exists(__DIR__ . "/sess_gone1"), session_status());
/// ob_end_flush();
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// int(1)
/// ```
fn handler_destroy(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 1)?;
    let id = string_at(req, 1)?;
    let Some(module) = sanity(req, true)? else {
        return Ok(Value::bool(false));
    };
    Ok(Value::bool(
        module == Module::Files && files::destroy(req, &id)?,
    ))
}

/// `SessionHandler::gc(int $max_lifetime): int|false`: the default
/// handler's collection, `false` where it counted nothing it could.
///
/// ```php
/// ob_start(); $h = new SessionHandler; session_set_save_handler($h); session_save_path(__DIR__); touch(__DIR__ . "/sess_stale", 1000);
/// session_start(["use_cookies" => 0, "cache_limiter" => ""]); var_dump($h->gc(1440), file_exists(__DIR__ . "/sess_stale"));
/// session_write_close(); ob_end_flush();
/// ```
/// ```expect
/// int(1)
/// bool(false)
/// ```
fn handler_gc(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 1, 1)?;
    let maxlifetime = coerced(req, 1, &Type::Int)?.as_int().unwrap_or(0);
    let Some(module) = sanity(req, true)? else {
        return Ok(Value::bool(false));
    };
    let deleted = match module {
        Module::Files => files::gc(req, maxlifetime)?,
        Module::User => -1,
    };
    Ok(if deleted == -1 {
        Value::bool(false)
    } else {
        Value::int(deleted)
    })
}

/// `SessionHandler::create_sid(): string`.
///
/// ```php
/// class C extends SessionHandler { public $p; function create_sid(): string { $id = (new SessionHandler)->create_sid(); echo strlen($id), "\n"; return "made" . substr($id, 4); } }
/// ob_start(); session_set_save_handler(new C); session_save_path(__DIR__); session_start(["use_cookies" => 0]); echo substr(session_id(), 0, 4), "\n"; session_write_close(); ob_end_flush();
/// ```
/// ```expect
/// 32
/// made
/// ```
fn handler_create_sid(req: &mut Request) -> Result<Value, Thrown> {
    arguments_between(req, 0, 0)?;
    let Some(module) = sanity(req, false)? else {
        return Ok(Value::bool(false));
    };
    let id = match module {
        Module::Files => files::create_sid(req)?,
        Module::User => session::create_id(req),
    };
    let id = id.unwrap_or_default();
    string_value(req, &id)
}

/// `[$object, $method]`, the callable `SESSION_SET_USER_HANDLER_OO`
/// stores.
pub(super) fn method_of(req: &mut Request, object: Value, name: &[u8]) -> Result<Value, Thrown> {
    let mut pair = PhpArray::new(req)?;
    req.addref(&object);
    pair.append(req, object)?;
    let name = string_value(req, name)?;
    pair.append(req, name)?;
    Ok(pair.to_value())
}
