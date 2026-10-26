//! `mod_user.c` and `mod_user_class.c`: the `user` save handler, each
//! operation a call of the callable `session_set_save_handler` stored,
//! its answer held to `bool` as `verify_bool_return_type_userland_calls`
//! holds it; the three interfaces of `session.stub.php`; and
//! `SessionHandler`, whose methods call the handler that stood before
//! `user`, `PS(default_mod)`.

use php_ext::abi::{OrFalse, This};
use php_ext::attr::php_class;
use php_ext::call::call;
use php_ext::coerce::{callable_checked, value_name};
use php_ext::raising::{Cause, Level, Thrown, docref, fatal, throw_error, type_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::throwables::Throwables;
use crate::{Module, State, Status, absorb, answer, files, pending, session, string_value};

/// `PS(mod_user_names)`: the callables, the request's owner of each.
#[derive(Default)]
pub(crate) struct Handlers {
    pub(crate) open: Option<Owner<Value>>,
    pub(crate) close: Option<Owner<Value>>,
    pub(crate) read: Option<Owner<Value>>,
    pub(crate) write: Option<Owner<Value>>,
    pub(crate) destroy: Option<Owner<Value>>,
    pub(crate) gc: Option<Owner<Value>>,
    pub(crate) create_sid: Option<Owner<Value>>,
    pub(crate) validate_sid: Option<Owner<Value>>,
    pub(crate) update_timestamp: Option<Owner<Value>>,
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
    pub(crate) const fn slot(&mut self, slot: Slot) -> &mut Option<Owner<Value>> {
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
            value.give_up(req);
        }
    }
}

/// One callable stored, its owner the request's, what stood there given
/// up.
pub(super) fn store(req: &mut Request, slot: Slot, value: Option<Owner<Value>>) {
    let old = std::mem::replace(req.state::<State>().handlers.slot(slot), value);
    if let Some(old) = old {
        old.give_up(req);
    }
}

/// `ps_call_handler`: the callable called with `arguments`, which it
/// gives up after; `None` is php's `UNDEF`, for a call that threw, a
/// recursive call refused with php's warning, and a throwable already
/// pending, as `zend_call_function` refuses to call then. An `exit` in
/// a handler is pending the same way, so the flush it stands in goes on
/// with every later call refused and the session no longer active, and
/// the request's end has nothing left to write:
///
/// ```php
/// class H implements SessionHandlerInterface { function open($p, $n): bool { return true; } function close(): bool { die("close\n"); }
///   function read($id): string|false { return ""; } function write($id, $d): bool { die("write\n"); }
///   function destroy($id): bool { return true; } function gc($l): int|false { return 0; } }
/// session_set_save_handler(new H); session_id("rs2"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); session_write_close(); echo "after\n";
/// ```
/// ```expect
/// write
/// ```
fn call_handler(
    req: &mut Request,
    slot: Slot,
    arguments: Vec<Owner<Value>>,
) -> Result<Option<Owner<Value>>, Thrown> {
    let callable = req.state::<State>().handlers.slot(slot).as_deref().copied();
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
        let views: Vec<Value> = arguments.iter().map(|argument| **argument).collect();
        let answered = call(req, &callable, &views);
        // `zend_bailout` leaves `ps_call_handler` before it clears
        // `PS(in_save_handler)`, so the next call is refused as a
        // recursive one; an `exit` unwinds as a throwable does, and the
        // flag is cleared behind it.
        let bailed = matches!(&answered, Err(thrown) if thrown.cause(req) == Cause::Fatal);
        if !bailed {
            req.state::<State>().in_save_handler = false;
        }
        absorb(req, answered)
    } else {
        Ok(None)
    };
    for argument in arguments {
        argument.give_up(req);
    }
    done
}

/// `verify_bool_return_type_userland_calls`: `true` is success, `false`
/// failure, `0` and `-1` the deprecated ints, anything else php's
/// `TypeError`.
fn verify_bool(req: &mut Request, answer: Option<Owner<Value>>) -> bool {
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
                    &[
                        b"Session callback must have a return value of type bool, ".as_slice(),
                        name.as_slice(),
                        b" returned".as_slice(),
                    ]
                    .concat(),
                );
            }
            Some(n == 0)
        }
        _ => None,
    };
    let verdict = verdict.unwrap_or_else(|| {
        if !pending(req) {
            let name = value_name(req, &value);
            let thrown = type_error(
                req,
                &[
                    b"Session callback must have a return value of type bool, ".as_slice(),
                    name.as_slice(),
                    b" returned".as_slice(),
                ]
                .concat(),
            );
            req.state::<State>().thrown = Some(thrown);
        }
        false
    });
    value.give_up(req);
    verdict
}

/// `PS_OPEN_FUNC(user)`: `open($path, $name)`, and the handler counted
/// open whatever it answered.
pub(super) fn open(req: &mut Request, path: &[u8], name: &[u8]) -> Result<bool, Thrown> {
    let path = string_value(req, path)?;
    let arguments = match string_value(req, name) {
        Ok(name) => vec![path, name],
        Err(thrown) => {
            path.give_up(req);
            return Err(thrown);
        }
    };
    let answered = call_handler(req, Slot::Open, arguments);
    req.state::<State>().user_open = true;
    let answered = answered?;
    Ok(verify_bool(req, answered))
}

/// `PS_CLOSE_FUNC(user)`: `close()` once the handler is open.
pub(super) fn close(req: &mut Request) -> Result<(), Thrown> {
    if !req.state::<State>().user_open {
        return Ok(());
    }
    let answered = call_handler(req, Slot::Close, Vec::new());
    req.state::<State>().user_open = false;
    let answered = answered?;
    verify_bool(req, answered);
    Ok(())
}

/// `PS_READ_FUNC(user)`: `read($id)`, whose string is the data.
pub(super) fn read(req: &mut Request, id: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let arguments = vec![string_value(req, id)?];
    let Some(value) = call_handler(req, Slot::Read, arguments)? else {
        return Ok(None);
    };
    let data = value.as_str_bytes().map(<[u8]>::to_vec);
    value.give_up(req);
    Ok(data)
}

fn two(req: &mut Request, slot: Slot, id: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    let id = string_value(req, id)?;
    let arguments = match string_value(req, val) {
        Ok(val) => vec![id, val],
        Err(thrown) => {
            id.give_up(req);
            return Err(thrown);
        }
    };
    let answered = call_handler(req, slot, arguments)?;
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
    let arguments = vec![string_value(req, id)?];
    let answered = call_handler(req, Slot::Destroy, arguments)?;
    Ok(verify_bool(req, answered))
}

/// `PS_GC_FUNC(user)`: `gc($max_lifetime)`, an int the count, `true`
/// one, anything else `-1`.
pub(super) fn gc(req: &mut Request, maxlifetime: i64) -> Result<i64, Thrown> {
    let arguments = vec![Owner::int(maxlifetime)];
    let answered = call_handler(req, Slot::Gc, arguments)?;
    let Some(value) = answered else {
        return Ok(-1);
    };
    let count = match value.as_ref() {
        ValueRef::Int(n) => n,
        ValueRef::Bool(true) => 1,
        _ => -1,
    };
    value.give_up(req);
    Ok(count)
}

/// `PS_CREATE_SID_FUNC(user)`: `create_sid()` where the handler has
/// one, php's own id otherwise.
pub(super) fn create_sid(req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
    if req.state::<State>().handlers.create_sid.is_none() {
        return Ok(session::create_id(req));
    }
    let Some(value) = call_handler(req, Slot::CreateSid, Vec::new())? else {
        crate::throw_error(req, "No session id returned by function");
        return Ok(None);
    };
    let id = value.as_str_bytes().map(<[u8]>::to_vec);
    value.give_up(req);
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
    let arguments = vec![string_value(req, id)?];
    let answered = call_handler(req, Slot::ValidateSid, arguments)?;
    Ok(verify_bool(req, answered))
}

/// `PS_SANITY_CHECK`: an active session and a handler to call, else
/// php's `Error`; with `open`, the parent opened too, else php's warning
/// and `false`.
fn sanity(req: &mut Request, open: bool) -> Result<Option<Module>, Thrown> {
    let state = req.state::<State>();
    if state.status != Status::Active {
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, "Session is not active"));
    }
    let Some(module) = state.default_module else {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "Cannot call default session handler",
        ));
    };
    if open && !state.parent_open {
        docref(req, Level::Warning, "Parent session handler is not open");
        return Ok(None);
    }
    Ok(Some(module))
}

/// `interface SessionHandlerInterface`, the six operations a save handler
/// object answers, each return tentative.
///
/// ```php
/// $m = new ReflectionMethod('SessionHandlerInterface', 'read');
/// var_dump($m->isAbstract(), (string) $m->getTentativeReturnType());
/// ```
/// ```expect
/// bool(true)
/// string(12) "string|false"
/// ```
#[php_class(interface)]
pub(crate) struct SessionHandlerInterface;

#[php_class]
impl SessionHandlerInterface {
    #[php(tentative)]
    fn open(this: This<Self>, path: PhpString, name: PhpString) -> bool;
    #[php(tentative)]
    fn close(this: This<Self>) -> bool;
    #[php(tentative)]
    fn read(this: This<Self>, id: PhpString) -> OrFalse<PhpString>;
    #[php(tentative)]
    fn write(this: This<Self>, id: PhpString, data: PhpString) -> bool;
    #[php(tentative)]
    fn destroy(this: This<Self>, id: PhpString) -> bool;
    #[php(tentative)]
    fn gc(this: This<Self>, max_lifetime: i64) -> OrFalse<i64>;
}

/// `interface SessionIdInterface`: a handler's own ids.
///
/// ```php
/// var_dump(interface_exists('SessionIdInterface'), (string) (new ReflectionMethod('SessionIdInterface', 'create_sid'))->getTentativeReturnType());
/// ```
/// ```expect
/// bool(true)
/// string(6) "string"
/// ```
#[php_class(interface)]
pub(crate) struct SessionIdInterface;

#[php_class]
impl SessionIdInterface {
    #[php(tentative)]
    fn create_sid(this: This<Self>) -> PhpString;
}

/// `interface SessionUpdateTimestampHandlerInterface`: the check
/// `session.use_strict_mode` makes of an id, and `lazy_write`'s touch.
///
/// ```php
/// echo implode(' ', get_class_methods('SessionUpdateTimestampHandlerInterface')), "\n";
/// ```
/// ```expect
/// validateId updateTimestamp
/// ```
#[php_class(interface)]
pub(crate) struct SessionUpdateTimestampHandlerInterface;

#[php_class]
impl SessionUpdateTimestampHandlerInterface {
    #[php(tentative)]
    fn validateId(this: This<Self>, id: PhpString) -> bool;
    #[php(tentative)]
    fn updateTimestamp(this: This<Self>, id: PhpString, data: PhpString) -> bool;
}

/// `class SessionHandler implements SessionHandlerInterface,
/// SessionIdInterface`, each method calling the handler that stood before
/// `user`, `PS(default_mod)`, its return tentative as the stub's
/// `@tentative-return-type` marks every one.
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
#[php_class(implements(SessionHandlerInterface, SessionIdInterface))]
pub(crate) struct SessionHandler;

/// The default handler's `read` and `write` reached through a class that
/// extends `SessionHandler`, as a handler that decorates `files` calls
/// them.
///
/// ```php
/// ob_start(); session_save_path(__DIR__); file_put_contents(__DIR__ . "/sess_rw1", "k|i:5;");
/// class R extends SessionHandler { function read($id): string|false { $d = parent::read($id); echo "read $d\n"; return $d; }
///   function write($id, $d): bool { echo "write $d\n"; return parent::write($id, $d); } }
/// session_set_save_handler(new R); session_id("rw1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// $_SESSION["k"]++; session_write_close(); echo file_get_contents(__DIR__ . "/sess_rw1"), "\n"; unlink(__DIR__ . "/sess_rw1");
/// ob_end_flush();
/// ```
/// ```expect
/// read k|i:5;
/// write k|i:6;
/// k|i:6;
/// ```
#[php_class]
impl SessionHandler {
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
    #[php(tentative)]
    fn open(
        req: &mut Request,
        _this: This<Self>,
        path: PhpString,
        name: PhpString,
    ) -> Result<bool, Thrown> {
        let _ = name;
        let Some(module) = sanity(req, false)? else {
            return Ok(false);
        };
        let opened = match module {
            Module::Files => files::open(req, path.as_bytes()),
            Module::User => false,
        };
        if opened {
            req.state::<State>().parent_open = true;
        }
        answer(req, opened)
    }

    /// `SessionHandler::close(): bool`, which takes no argument
    /// (`ZEND_PARSE_PARAMETERS_NONE`).
    ///
    /// ```php
    /// class H extends SessionHandler { function close(): bool { return parent::close(false); } }
    /// ob_start(); session_set_save_handler(new H); session_save_path(__DIR__); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
    /// try { session_write_close(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// SessionHandler::close() expects exactly 0 arguments, 1 given
    /// ```
    #[php(tentative)]
    fn close(req: &mut Request, _this: This<Self>) -> Result<bool, Thrown> {
        let Some(module) = sanity(req, true)? else {
            return Ok(false);
        };
        req.state::<State>().parent_open = false;
        if module == Module::Files {
            files::close(req);
        }
        Ok(true)
    }

    /// `SessionHandler::read(string $id): string|false`.
    #[php(tentative)]
    fn read(
        req: &mut Request,
        _this: This<Self>,
        id: PhpString,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let Some(module) = sanity(req, true)? else {
            return Ok(OrFalse::False);
        };
        let data = match module {
            Module::Files => files::read(req, id.as_bytes())?,
            Module::User => None,
        };
        match data {
            Some(data) => PhpString::new(req, &data).map(OrFalse::Value),
            None => Ok(OrFalse::False),
        }
    }

    /// `SessionHandler::write(string $id, string $data): bool`.
    #[php(tentative)]
    fn write(
        req: &mut Request,
        _this: This<Self>,
        id: PhpString,
        data: PhpString,
    ) -> Result<bool, Thrown> {
        let Some(module) = sanity(req, true)? else {
            return Ok(false);
        };
        Ok(module == Module::Files && files::write(req, id.as_bytes(), data.as_bytes())?)
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
    #[php(tentative)]
    fn destroy(req: &mut Request, _this: This<Self>, id: PhpString) -> Result<bool, Thrown> {
        let Some(module) = sanity(req, true)? else {
            return Ok(false);
        };
        Ok(module == Module::Files && files::destroy(req, id.as_bytes()))
    }

    /// `SessionHandler::gc(int $max_lifetime): int|false`: the default
    /// handler's collection, `false` where it counted nothing it could.
    ///
    /// ```php
    /// ob_start(); $h = new SessionHandler; session_set_save_handler($h); session_save_path(__DIR__); touch(__DIR__ . "/sess_stale", 1000);
    /// session_start(["use_cookies" => 0, "cache_limiter" => "", "gc_probability" => 0]); var_dump($h->gc(1440), file_exists(__DIR__ . "/sess_stale"));
    /// session_write_close(); ob_end_flush();
    /// ```
    /// ```expect
    /// int(1)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn gc(req: &mut Request, _this: This<Self>, max_lifetime: i64) -> Result<OrFalse<i64>, Thrown> {
        let Some(module) = sanity(req, true)? else {
            return Ok(OrFalse::False);
        };
        let deleted = match module {
            Module::Files => files::gc(req, max_lifetime),
            Module::User => -1,
        };
        Ok(if deleted == -1 {
            OrFalse::False
        } else {
            OrFalse::Value(deleted)
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
    #[php(tentative)]
    fn create_sid(req: &mut Request, _this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        // `sanity` answers a module whenever it does not require the
        // parent opened.
        let Some(module) = sanity(req, false)? else {
            return Err(fatal(
                req,
                "InternalError: SessionHandler::create_sid() without a default handler",
            ));
        };
        let id = match module {
            Module::Files => files::create_sid(req),
            Module::User => session::create_id(req),
        };
        let id = id.unwrap_or_default();
        PhpString::new(req, &id)
    }
}

/// `[$object, $method]`, the callable `SESSION_SET_USER_HANDLER_OO`
/// stores.
pub(super) fn method_of(
    req: &mut Request,
    object: Value,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let mut pair = PhpArray::new(req)?;
    let built = pair
        .append(req, Owner::take(req, object))
        .and_then(|()| string_value(req, name))
        .and_then(|name| pair.append(req, name));
    match built {
        Ok(()) => Ok(pair.into_value()),
        Err(thrown) => {
            pair.give_up(req);
            Err(thrown)
        }
    }
}
