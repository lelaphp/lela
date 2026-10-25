//! The state machine of `session.c` over `PS(session_status)`:
//! `php_session_start`, `php_session_initialize`, `php_session_flush`,
//! `php_session_abort`, `php_session_reset`, `php_session_destroy`, the
//! id and its cookie, the cache limiter's headers, garbage collection,
//! and the request hooks. Each save handler operation is dispatched by
//! the save handler standing in `PS(mod)`.

use php_ext::coerce::c_str;
use std::io::Read;

use php_ext::coerce::strtol;
use php_ext::decl::{Attribute, AttributeArgument, ConstExpr, Name};
use php_ext::raising::{Cause, Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::PhpArray;

use crate::{
    Format, Module, Phase, State, Status, absorb, bind_vars, files, headers_sent, ini, ini_bool,
    pending, sent_error, serializers, session_array, shown, started_error, throw_error, user,
};

/// `PS_MAX_SID_LENGTH`.
const MAX_SID_LENGTH: usize = 256;

/// `hexconvtab`.
const HEXCONVTAB: &[u8; 64] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ,-";

// ------------------------------------------------------------ modules

/// `PS(mod)->s_open`.
pub(super) fn open(req: &mut Request, module: Module) -> Result<bool, Thrown> {
    let path = ini(req, b"session.save_path");
    let name = ini(req, b"session.name");
    match module {
        Module::Files => Ok(files::open(req, &path)),
        Module::User => user::open(req, &path, &name),
    }
}

/// `PS(mod)->s_close`, where `PS(mod_data) || PS(mod_user_implemented)`.
pub(super) fn close(req: &mut Request) -> Result<(), Thrown> {
    let state = req.state::<State>();
    if state.files.is_none() && !state.user_open {
        return Ok(());
    }
    if state.module == Some(Module::User) {
        return user::close(req);
    }
    files::close(req);
    Ok(())
}

fn read(req: &mut Request, module: Module, id: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    match module {
        Module::Files => files::read(req, id),
        Module::User => user::read(req, id),
    }
}

pub(super) fn write(
    req: &mut Request,
    module: Module,
    id: &[u8],
    val: &[u8],
) -> Result<bool, Thrown> {
    match module {
        Module::Files => files::write(req, id, val),
        Module::User => user::write(req, id, val),
    }
}

fn update_timestamp(
    req: &mut Request,
    module: Module,
    id: &[u8],
    val: &[u8],
) -> Result<bool, Thrown> {
    match module {
        Module::Files => files::update_timestamp(req, id, val),
        Module::User => user::update_timestamp(req, id, val),
    }
}

pub(super) fn destroy_id(req: &mut Request, module: Module, id: &[u8]) -> Result<bool, Thrown> {
    match module {
        Module::Files => Ok(files::destroy(req, id)),
        Module::User => user::destroy(req, id),
    }
}

pub(super) fn module_gc(
    req: &mut Request,
    module: Module,
    maxlifetime: i64,
) -> Result<i64, Thrown> {
    match module {
        Module::Files => Ok(files::gc(req, maxlifetime)),
        Module::User => user::gc(req, maxlifetime),
    }
}

pub(super) fn create_sid(req: &mut Request, module: Module) -> Result<Option<Vec<u8>>, Thrown> {
    match module {
        Module::Files => Ok(files::create_sid(req)),
        Module::User => user::create_sid(req),
    }
}

pub(super) fn validate_sid(req: &mut Request, module: Module, id: &[u8]) -> Result<bool, Thrown> {
    match module {
        Module::Files => Ok(files::validate_sid(req, id)),
        Module::User => user::validate_sid(req, id),
    }
}

// ------------------------------------------------------------ ids

/// `php_session_valid_key`: `[a-zA-Z0-9,-]`, one to 256 of them, read
/// up to the first NUL.
pub(super) fn valid_key(key: &[u8]) -> bool {
    let key = c_str(key);
    !key.is_empty()
        && key.len() <= MAX_SID_LENGTH
        && key
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b',' || c == b'-')
}

/// `php_session_create_id`: `session.sid_length` random bytes of the
/// system's CSPRNG, read as `session.sid_bits_per_character` bits a
/// character by `bin_to_readable`.
///
/// ```php
/// $id = session_create_id(); var_dump(strlen($id), strspn($id, "0123456789abcdefghijklmnopqrstuv") === 32);
/// var_dump(strlen(session_create_id("pre-")), session_create_id("bad prefix"));
/// ```
/// ```expect
/// int(32)
/// bool(true)
///
/// Warning: session_create_id(): Prefix cannot contain special characters. Only the A-Z, a-z, 0-9, "-", and "," characters are allowed in %s on line 3
/// int(36)
/// bool(false)
/// ```
pub(super) fn create_id(req: &mut Request) -> Option<Vec<u8>> {
    let length = usize::try_from(strtol(&ini(req, b"session.sid_length")))
        .ok()
        .filter(|n| (22..=MAX_SID_LENGTH).contains(n))
        .unwrap_or(32);
    let bits = u32::try_from(strtol(&ini(req, b"session.sid_bits_per_character")))
        .ok()
        .filter(|n| (4..=6).contains(n))
        .unwrap_or(4);
    let mut random = vec![0u8; length];
    let got = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut random));
    if got.is_err() {
        throw_error(req, "Cannot open source device");
        return None;
    }
    let mut out = Vec::with_capacity(length);
    let (mut w, mut have, mut p) = (0u32, 0u32, 0);
    let mask = (1u32 << bits) - 1;
    while out.len() < length {
        if have < bits {
            let Some(&byte) = random.get(p) else { break };
            w |= u32::from(byte) << have;
            p += 1;
            have += 8;
        }
        out.push(HEXCONVTAB[usize::try_from(w & mask).unwrap_or(0)]);
        w >>= bits;
        have -= bits;
    }
    Some(out)
}

// ------------------------------------------------------------ vars

/// `php_session_track_init`: `$_SESSION` deleted from `EG(symbol_table)`
/// whatever it held, then bound to a new empty array.
///
/// ```php
/// ob_start(); session_save_path(__DIR__); session_id("track1");
/// session_start(["use_cookies" => 0, "cache_limiter" => ""]); $_SESSION['a'] = 1; $_SESSION['b'] = [2];
/// session_write_close(); var_dump(file_get_contents(__DIR__ . "/sess_track1"), $_SESSION);
/// $_SESSION = []; session_start(); var_dump($_SESSION);
/// unset($_SESSION['a']); function f() { $_SESSION['c'] = 3; } f(); session_write_close();
/// session_start(); var_dump(session_encode()); $_SESSION = ['d' => 4]; session_write_close();
/// session_start(); unset($_SESSION); var_dump(isset($_SESSION), session_encode()); session_write_close();
/// session_start(); var_dump($_SESSION); session_destroy(); ob_end_flush();
/// ```
/// ```expect
/// string(22) "a|i:1;b|a:1:{i:0;i:2;}"
/// array(2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   array(1) {
///     [0]=>
///     int(2)
///   }
/// }
/// array(2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   array(1) {
///     [0]=>
///     int(2)
///   }
/// }
/// string(22) "b|a:1:{i:0;i:2;}c|i:3;"
/// bool(false)
/// string(6) "d|i:4;"
/// array(1) {
///   ["d"]=>
///   int(4)
/// }
/// ```
pub(super) fn track_init(req: &mut Request) -> Result<(), Thrown> {
    req.variables_mut().set_global(b"_SESSION", None);
    let array = PhpArray::new(req)?;
    bind_vars(req, array.into_value())
}

/// `php_session_encode`: the session's variables through the serializer,
/// `None` after php's warning where there is no session array, and
/// `None` where the serializer wrote nothing.
pub(super) fn encode(req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
    if session_array(req).is_none() {
        docref(req, Level::Warning, "Cannot encode non-existent session");
        return Ok(None);
    }
    let format = req.state::<State>().format.unwrap_or(Format::Php);
    let encoded = serializers::encode(req, format);
    Ok(absorb(req, encoded)?.flatten())
}

/// `php_session_decode`: the data through the serializer into the
/// session's array; a failure destroys the session with php's warning.
/// The serializer runs under `zend_try`, and a fatal raised under it,
/// a class declaration the data's autoloader compiles among them, is
/// caught to destroy the session and warn before it goes on
/// (`zend_catch`, `Z/ext/session/session.c:289-302`); the warning names
/// no function and no file, since the fatal left every frame.
///
/// ```php
/// session_set_save_handler(new class implements SessionHandlerInterface {
///     function open($p, $n): bool { return true; } function close(): bool { return true; }
///     function read($id): string|false { return 'x|O:1:"X":0:{}'; } function write($id, $d): bool { return true; }
///     function destroy($id): bool { return true; } function gc($m): int|false { return 0; }
/// });
/// spl_autoload_register(function ($c) { eval('class X { public int $p = null; }'); });
/// register_shutdown_function(function () { var_dump($_SESSION); });
/// session_start();
/// ```
/// ```expect exit=255
/// Fatal error: Default value for property of type int may not be null. Use the nullable type ?int to allow null default value in %s(7) : eval()'d code on line 1
///
/// Warning: Unknown: Failed to decode session object. Session has been destroyed in Unknown on line 0
/// array(0) {
/// }
/// ```
pub(super) fn decode(req: &mut Request, data: &[u8]) -> Result<bool, Thrown> {
    let format = req.state::<State>().format.unwrap_or(Format::Php);
    let decoded = match serializers::decode(req, format, data) {
        Err(thrown) if thrown.cause(req) == Cause::Fatal => {
            // `zend_catch { php_session_cancel_decode(); zend_bailout(); }`:
            // what the cancel raises in turn cannot outlive the fatal.
            cancel_decode(req).ok();
            return Err(thrown);
        }
        decoded => decoded,
    };
    let ok = absorb(req, decoded)?.unwrap_or(false);
    if !ok {
        cancel_decode(req)?;
    }
    Ok(ok)
}

/// `php_session_cancel_decode`: the session destroyed, its tracking
/// begun again, and php's warning.
fn cancel_decode(req: &mut Request) -> Result<(), Thrown> {
    destroy(req)?;
    track_init(req)?;
    docref(
        req,
        Level::Warning,
        "Failed to decode session object. Session has been destroyed",
    );
    Ok(())
}

// ------------------------------------------------------------ lifecycle

/// `php_rinit_session(false)`: the request state reset and the save handler and
/// the serializer the ini names found, the session disabled where either
/// is unknown; `false` then.
pub(super) fn rinit_without_start(req: &mut Request) -> bool {
    rinit_globals(req);
    let module = Module::find(&ini(req, b"session.save_handler"));
    let format = Format::find(&ini(req, b"session.serialize_handler"));
    let state = req.state::<State>();
    state.module = module;
    state.format = format;
    if module.is_none() || format.is_none() {
        state.status = Status::Disabled;
        return false;
    }
    true
}

/// `php_rinit_session_globals`.
pub(super) fn rinit_globals(req: &mut Request) {
    let state = req.state::<State>();
    // `PS(in_rfc1867_update) = false`.
    if state.phase == Phase::ProgressUpdate {
        state.phase = Phase::Startup;
    }
    state.id = None;
    state.status = Status::None;
    state.in_save_handler = false;
    state.set_handler = false;
    state.files = None;
    state.parent_open = false;
    state.define_sid = true;
    state.session_vars = None;
    state.vars = None;
}

/// `php_rshutdown_session_globals`: the handler closed, the id, the
/// read data and the start site forgotten.
pub(super) fn rshutdown_globals(req: &mut Request) -> Result<(), Thrown> {
    crate::unbind(req);
    let closed = close(req);
    absorb(req, closed)?;
    let state = req.state::<State>();
    state.id = None;
    state.session_vars = None;
    state.class_name = None;
    state.started_at = None;
    state.status = Status::None;
    Ok(())
}

/// `PHP_RINIT_FUNCTION(session)`: the save handler and the serializer the ini
/// names, the session disabled where either is unknown, and started
/// under `session.auto_start`, with no file executing to have started it
/// from (`zend_get_executed_filename_ex()` is null there), so what names
/// the start site later says the session started automatically. The
/// startup rules are the engine's to serve, over the host's values, as
/// the engine starts (`ZEND_INI_STAGE_STARTUP`), before this runs.
pub(super) fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.state::<State>().phase = Phase::Activated;
    if !rinit_without_start(req) {
        return Ok(());
    }
    if ini_bool(req, b"session.auto_start") {
        start(req)?;
        req.state::<State>().started_at = None;
        if let Some(thrown) = req.state::<State>().thrown.take() {
            return Err(thrown);
        }
    }
    Ok(())
}

/// `PHP_RSHUTDOWN_FUNCTION(session)`: an active session written and
/// closed, the handler's callables given up. The flush stands in a
/// `zend_try`: a user handler that throws here has no frame to throw to,
/// so the throwable is the uncaught fatal where it was thrown and the
/// flush leaves by `zend_bailout`, `PS(in_save_handler)` still set; the
/// close after it is then refused as a recursive call, and what the
/// session raises here names `PHP Request Shutdown` as its origin.
///
/// ```php
/// class H implements SessionHandlerInterface { function open($p, $n): bool { return true; } function close(): bool { echo "close\n"; return true; }
///   function read($id): string|false { return ""; } function write($id, $d): bool { echo "write\n"; throw new Exception("w"); }
///   function destroy($id): bool { return true; } function gc($l): int|false { return 0; } }
/// ob_start(); session_set_save_handler(new H, false); session_id("rs1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); $_SESSION["k"] = 1;
/// ```
/// ```expect exit=255
/// write
///
/// Fatal error: Uncaught Exception: w in %s:3
/// Stack trace:
/// #0 [internal function]: H->write('rs1', 'k|i:1;')
/// #1 {main}
///   thrown in %s on line 3
///
/// Warning: PHP Request Shutdown: Cannot call session save handler in a recursive manner in Unknown on line 0
/// ```
///
/// A warning of the write itself is the request's shutdown's too:
///
/// ```php
/// session_save_path(__DIR__); session_id("rs3"); session_start(["use_cookies" => 0, "cache_limiter" => ""]); $_SESSION['a|b'] = 1;
/// ```
/// ```expect
/// Warning: PHP Request Shutdown: Failed to write session data. Data contains invalid key "a|b" in Unknown on line 0
/// ```
pub(super) fn rshutdown(req: &mut Request) {
    if req.state::<State>().status == Status::Active {
        flush(req, true).ok();
    }
    rshutdown_globals(req).ok();
    let handlers = std::mem::take(&mut req.state::<State>().handlers);
    handlers.release(req);
    for first in std::mem::take(&mut req.state::<State>().overwritten) {
        first.give_up(req);
    }
    req.state::<State>().thrown = None;
}

/// `php_session_start`.
#[allow(clippy::too_many_lines)] // php's function, arm for arm
pub(super) fn start(req: &mut Request) -> Result<bool, Thrown> {
    match req.state::<State>().status {
        Status::Active => {
            started_error(
                req,
                Level::Notice,
                "Ignoring session_start() because a session has already been started",
            );
            return Ok(false);
        }
        Status::Disabled => {
            let handler = ini(req, b"session.save_handler");
            if req.state::<State>().module.is_none() {
                let Some(module) = Module::find(&handler) else {
                    docref(
                        req,
                        Level::Warning,
                        &format!(
                            "Cannot find session save handler \"{}\" - session startup failed",
                            shown(&handler)
                        ),
                    );
                    return Ok(false);
                };
                req.state::<State>().module = Some(module);
            }
            let serializer = ini(req, b"session.serialize_handler");
            if req.state::<State>().format.is_none() {
                let Some(format) = Format::find(&serializer) else {
                    docref(
                        req,
                        Level::Warning,
                        &format!(
                            "Cannot find session serialization handler \"{}\" - session startup failed",
                            shown(&serializer)
                        ),
                    );
                    return Ok(false);
                };
                req.state::<State>().format = Some(format);
            }
            req.state::<State>().status = Status::None;
            prime(req);
        }
        Status::None => prime(req),
    }
    if req.state::<State>().id.is_none() {
        find_id(req);
    }
    let dangerous = req
        .state::<State>()
        .id
        .as_deref()
        .is_some_and(|id| c_str(id).iter().any(|c| b"\r\n\t <>'\"\\".contains(c)));
    if dangerous {
        req.state::<State>().id = None;
    }
    if !initialize(req)? || cache_limiter(req)? == -2 {
        let state = req.state::<State>();
        state.status = Status::None;
        state.id = None;
        return Ok(false);
    }
    Ok(true)
}

/// The flags `php_session_start` sets on its way into `none`.
fn prime(req: &mut Request) {
    let only_cookies = ini_bool(req, b"session.use_only_cookies");
    let cookies = ini_bool(req, b"session.use_cookies");
    let state = req.state::<State>();
    state.define_sid = !only_cookies;
    state.send_cookie = cookies || only_cookies;
}

/// `try_find_session_id_in_cookies` and `try_find_session_id_in_global`:
/// the id the request's cookie, `$_GET` or `$_POST` carries under the
/// session's name.
fn find_id(req: &mut Request) {
    let name = ini(req, b"session.name");
    let lookup = |req: &mut Request, global: &[u8]| -> Option<Option<Vec<u8>>> {
        let array = req.variables_mut().superglobal(global)?;
        let found = array.find(&name)?.deref();
        Some(found.as_str_bytes().map(<[u8]>::to_vec))
    };
    if ini_bool(req, b"session.use_cookies")
        && let Some(found) = lookup(req, b"_COOKIE")
    {
        let state = req.state::<State>();
        state.id = found;
        state.send_cookie = false;
        state.define_sid = false;
    }
    if !ini_bool(req, b"session.use_only_cookies") {
        for global in [b"_GET".as_slice(), b"_POST"] {
            if req.state::<State>().id.is_some() {
                break;
            }
            if let Some(found) = lookup(req, global) {
                let state = req.state::<State>();
                state.send_cookie = found.is_none();
                state.id = found;
            }
        }
    }
}

/// `php_session_initialize`: the handler opened, an id made or checked,
/// the cookie and `SID`, the data read and decoded, garbage collected.
pub(super) fn initialize(req: &mut Request) -> Result<bool, Thrown> {
    req.state::<State>().status = Status::Active;
    let Some(module) = req.state::<State>().module else {
        req.state::<State>().status = Status::Disabled;
        docref(
            req,
            Level::Warning,
            "No storage module chosen - failed to initialize session",
        );
        return Ok(false);
    };
    let path = shown(&ini(req, b"session.save_path"));
    let opened = open(req, module);
    if !absorb(req, opened)?.unwrap_or(false) {
        abort(req)?;
        if !pending(req) {
            docref(
                req,
                Level::Warning,
                &format!(
                    "Failed to initialize storage module: {} (path: {path})",
                    module.name()
                ),
            );
        }
        return Ok(false);
    }
    let id = req.state::<State>().id.clone();
    match id {
        Some(id) if !c_str(&id).is_empty() => {
            if ini_bool(req, b"session.use_strict_mode") {
                let valid = validate_sid(req, module, &id);
                if !absorb(req, valid)?.unwrap_or(false) && !made_id(req, module, &path)? {
                    return Ok(false);
                }
            }
        }
        _ => {
            if !made_id(req, module, &path)? {
                return Ok(false);
            }
        }
    }
    if !reset_id(req)? {
        abort(req)?;
        return Ok(false);
    }
    track_init(req)?;
    let id = req.state::<State>().id.clone().unwrap_or_default();
    let read = read(req, module, &id);
    let Some(Some(val)) = absorb(req, read)? else {
        abort(req)?;
        if !pending(req) {
            docref(
                req,
                Level::Warning,
                &format!(
                    "Failed to read session data: {} (path: {path})",
                    module.name()
                ),
            );
        }
        return Ok(false);
    };
    gc(req, false)?;
    req.state::<State>().session_vars = ini_bool(req, b"session.lazy_write").then(|| val.clone());
    decode(req, &val)?;
    let file = req.frames().current_file().map(<[u8]>::to_vec);
    let line = req.frames().current_line();
    req.state::<State>().started_at = file.map(|f| (f, line));
    Ok(true)
}

/// `PS(id) = PS(mod)->s_create_sid(...)` in `php_session_initialize`,
/// for no id and for one `use_strict_mode` refused: the id made, and the cookie
/// to send for it; where none was made the session aborted and, unless
/// the handler threw, php's `Error`, and `false`. An id `use_strict_mode`
/// had remade fails as the first does since 8.5.12, where 8.5.9 fell back
/// to php's own id.
///
/// ```php
/// class F implements SessionHandlerInterface, SessionIdInterface {
///   function open($p, $n): bool { return true; } function close(): bool { return true; } function read($id): string|false { return ""; }
///   function write($id, $d): bool { return true; } function destroy($id): bool { return true; } function gc($l): int|false { return 0; }
///   function create_sid(): string { throw new RuntimeException("create_sid failed"); } function validateId($id): bool { return false; } }
/// session_set_save_handler(new F, true); session_id(str_repeat("a", 32));
/// try { var_dump(session_start(["use_strict_mode" => true, "use_cookies" => 0, "cache_limiter" => ""])); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// var_dump(session_status() === PHP_SESSION_ACTIVE, defined("SID"));
/// ```
/// ```expect
/// RuntimeException: create_sid failed
/// bool(false)
/// bool(false)
/// ```
fn made_id(req: &mut Request, module: Module, path: &str) -> Result<bool, Thrown> {
    req.state::<State>().id = None;
    let made = create_sid(req, module);
    let Some(made) = absorb(req, made)?.flatten() else {
        abort(req)?;
        if !pending(req) {
            throw_error(
                req,
                &format!(
                    "Failed to create session ID: {} (path: {path})",
                    module.name()
                ),
            );
        }
        return Ok(false);
    };
    req.state::<State>().id = Some(made);
    if ini_bool(req, b"session.use_cookies") {
        req.state::<State>().send_cookie = true;
    }
    Ok(true)
}

/// `php_session_gc`: the handler's collection, forced or drawn with
/// `gc_probability / gc_divisor`; the count it deleted, `-1` for none.
pub(super) fn gc(req: &mut Request, immediate: bool) -> Result<i64, Thrown> {
    let state = req.state::<State>();
    if state.files.is_none() && !state.user_open {
        return Ok(-1);
    }
    let Some(module) = state.module else {
        return Ok(-1);
    };
    let probability = req.ini().long(b"session.gc_probability");
    let divisor = req.ini().long(b"session.gc_divisor").max(1);
    let mut collect = immediate;
    if !collect && probability > 0 {
        let mut draw = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut draw))
            .ok();
        let range = u64::try_from(divisor).unwrap_or(1);
        collect = i64::try_from(u64::from_le_bytes(draw) % range).unwrap_or(0) < probability;
    }
    if !collect {
        return Ok(-1);
    }
    let maxlifetime = req.ini().long(b"session.gc_maxlifetime");
    let collected = module_gc(req, module, maxlifetime);
    Ok(absorb(req, collected)?.unwrap_or(-1))
}

/// `php_session_save_current_state`: the variables encoded and written,
/// or the timestamp updated where `lazy_write` finds them unchanged,
/// then the handler closed.
fn save_current_state(req: &mut Request, write_data: bool) -> Result<(), Thrown> {
    if write_data && session_array(req).is_some() {
        let state = req.state::<State>();
        let (module, class_name) = (state.module, state.class_name.clone());
        let open = state.files.is_some() || state.user_open;
        let mut function = "write";
        let mut ret = false;
        if let (true, Some(module)) = (open, module) {
            let id = req.state::<State>().id.clone().unwrap_or_default();
            let encoded = encode(req)?;
            let lazy = ini_bool(req, b"session.lazy_write");
            let read_back = req.state::<State>().session_vars.clone();
            let done = match encoded {
                Some(val) if lazy && read_back.as_deref() == Some(val.as_slice()) => {
                    function = if class_name.is_some() {
                        "updateTimestamp"
                    } else {
                        "update_timestamp"
                    };
                    update_timestamp(req, module, &id, &val)
                }
                Some(val) => write(req, module, &id, &val),
                None => write(req, module, &id, b""),
            };
            ret = absorb(req, done)?.unwrap_or(false);
        }
        if !ret && !pending(req) {
            let path = shown(&ini(req, b"session.save_path"));
            let message = if !req.state::<State>().user_open {
                format!(
                    "Failed to write session data ({}). Please verify that the current setting of session.save_path is correct ({path})",
                    module.map_or("", Module::name)
                )
            } else if let Some(class) = class_name {
                format!(
                    "Failed to write session data using user defined save handler. (session.save_path: {path}, handler: {}::{function})",
                    shown(&class)
                )
            } else {
                format!(
                    "Failed to write session data using user defined save handler. (session.save_path: {path}, handler: {function})"
                )
            };
            docref(req, Level::Warning, &message);
        }
    }
    let closed = close(req);
    absorb(req, closed)?;
    Ok(())
}

/// `php_session_flush`.
pub(super) fn flush(req: &mut Request, write_data: bool) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        return Ok(false);
    }
    save_current_state(req, write_data)?;
    req.state::<State>().status = Status::None;
    Ok(true)
}

/// `php_session_abort`: the handler closed, nothing written.
pub(super) fn abort(req: &mut Request) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        return Ok(false);
    }
    let closed = close(req);
    absorb(req, closed)?;
    req.state::<State>().status = Status::None;
    Ok(true)
}

/// `php_session_reset`: the session read again from its handler.
pub(super) fn reset(req: &mut Request) -> Result<bool, Thrown> {
    Ok(req.state::<State>().status == Status::Active && initialize(req)?)
}

/// `php_session_destroy`: the handler's data for the id removed, the
/// session's request state reset.
pub(super) fn destroy(req: &mut Request) -> Result<bool, Thrown> {
    if req.state::<State>().status != Status::Active {
        docref(
            req,
            Level::Warning,
            "Trying to destroy uninitialized session",
        );
        return Ok(false);
    }
    let mut ok = true;
    let state = req.state::<State>();
    if let (Some(id), Some(module)) = (state.id.clone(), state.module) {
        let destroyed = destroy_id(req, module, &id);
        if !absorb(req, destroyed)?.unwrap_or(false) {
            ok = false;
            if !pending(req) {
                docref(req, Level::Warning, "Session object destruction failed");
            }
        }
    }
    rshutdown_globals(req)?;
    rinit_globals(req);
    Ok(ok)
}

// ------------------------------------------------------------ the id's cookie

/// `php_session_reset_id`: the cookie sent where it is due, and `SID`,
/// registered under the session with php's `#[\Deprecated]` the first
/// time and its value replaced every time after, `session_regenerate_id`
/// among them, as the `zval` `zend_get_constant_str` finds is written.
///
/// ```php
/// ob_start(); ini_set("session.use_only_cookies", "0"); session_save_path(__DIR__); session_id("sid1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// echo SID, "\n"; session_regenerate_id(true); var_dump(SID === "PHPSESSID=" . session_id(), SID !== "PHPSESSID=sid1");
/// $c = get_defined_constants(true); var_dump(isset($c['session']['SID']), (new ReflectionConstant('SID'))->isDeprecated()); session_destroy();
/// ```
/// ```expect
///
/// Deprecated: ini_set(): Disabling session.use_only_cookies INI setting is deprecated in %s on line 2
///
/// Deprecated: Constant SID is deprecated since 8.4, as GET/POST sessions were deprecated in %s on line 3
/// PHPSESSID=sid1
///
/// Deprecated: Constant SID is deprecated since 8.4, as GET/POST sessions were deprecated in %s on line 3
///
/// Deprecated: Constant SID is deprecated since 8.4, as GET/POST sessions were deprecated in %s on line 3
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// A `SID` the program defined first is written the same way:
///
/// Under `session.use_trans_sid`, with no cookie carrying the id, the id
/// is added to the relative and listed URLs and to the forms, through
/// ext/standard's session rewriter; the rewriter reads its pair as it
/// flushes, and a regenerated id takes the old one's place rather than
/// joining it, so the whole body carries the new id once per URL:
///
/// ```php
/// @ini_set('session.use_only_cookies', '0'); @ini_set('session.use_trans_sid', '1');
/// session_id('abc'); session_start(['use_cookies' => 0, 'cache_limiter' => '', 'save_path' => __DIR__]);
/// ?>
/// <a href="x.php?q=1">x</a> <a href="http://other.org/">y</a>
/// <form action="x.php"></form>
/// <?php
/// print_r(ob_list_handlers()); session_regenerate_id(true);
/// ?>
/// <a href="y.php">y</a>
/// <?php
/// echo session_id() === 'abc' ? "old\n" : "new\n"; session_destroy();
/// ```
/// ```expect
/// <a href="x.php?q=1&PHPSESSID=%s">x</a> <a href="http://other.org/">y</a>
/// <form action="x.php"><input type="hidden" name="PHPSESSID" value="%s" /></form>
/// Array
/// (
///     [0] => URL-Rewriter
/// )
/// <a href="y.php?PHPSESSID=%s">y</a>
/// new
/// ```
///
/// ```php
/// define('SID', [0xdeadbeef]); ob_start(); session_save_path(__DIR__); session_start(["use_cookies" => 0, "cache_limiter" => ""]); var_dump(SID); session_destroy();
/// ```
/// ```expect
/// string(0) ""
/// ```
pub(super) fn reset_id(req: &mut Request) -> Result<bool, Thrown> {
    let Some(id) = req.state::<State>().id.clone() else {
        docref(
            req,
            Level::Warning,
            "Cannot set session ID - session ID is not initialized",
        );
        return Ok(false);
    };
    if ini_bool(req, b"session.use_cookies") && req.state::<State>().send_cookie {
        if !send_cookie(req, &id) {
            return Ok(false);
        }
        req.state::<State>().send_cookie = false;
    }
    let sid = if req.state::<State>().define_sid {
        let mut sid = ini(req, b"session.name");
        sid.push(b'=');
        sid.extend_from_slice(&id);
        sid
    } else {
        Vec::new()
    };
    let sid = crate::string_value(req, &sid)?;
    match req.extensions().named(b"session") {
        Some(session) => {
            req.symbols_mut()
                .register_constant(session, b"SID", sid, sid_attributes());
        }
        None => sid.give_up(req),
    }
    // `APPLY_TRANS_SID`, where no cookie of the session's name came with
    // the request: the id added to the session context's URLs and forms,
    // the old pair taken out first, which fails where the name changed.
    if ini_bool(req, b"session.use_trans_sid") && !ini_bool(req, b"session.use_only_cookies") {
        let name = ini(req, b"session.name");
        let cookie_carries_it = ini_bool(req, b"session.use_cookies")
            && req
                .variables_mut()
                .superglobal(b"_COOKIE")
                .is_some_and(|cookies| cookies.find(&name).is_some());
        if !cookie_carries_it {
            php_ext_standard::php_url_scanner_reset_session_var(req, &name, true);
            php_ext_standard::php_url_scanner_add_session_var(req, &name, &id, true)?;
        }
    }
    Ok(true)
}

/// What `php_session_reset_id` puts on a `SID` it registers:
/// `#[\Deprecated(since: "8.4", message: "as GET/POST sessions were
/// deprecated")]`, so a read of `SID` is php's deprecation.
fn sid_attributes() -> Vec<Attribute> {
    let argument = |name: &[u8], value: &[u8]| AttributeArgument {
        name: Some(Name(name.into())),
        value: ConstExpr::Str(value.into()),
    };
    vec![Attribute {
        name: Name(b"Deprecated".as_slice().into()),
        arguments: vec![
            argument(b"since", b"8.4"),
            argument(b"message", b"as GET/POST sessions were deprecated"),
        ],
        line: 0,
        strict: false,
        own_name: true,
    }]
}

/// `php_url_encode`.
fn url_encode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        match c {
            b' ' => out.push(b'+'),
            c if c.is_ascii_alphanumeric() || b"-_.".contains(&c) => out.push(c),
            c => out.extend_from_slice(format!("%{c:02X}").as_bytes()),
        }
    }
    out
}

/// The civil date of a Unix time, `D, d M Y H:i:s` in GMT.
fn gmt(t: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02}",
        DAYS[usize::try_from(days.rem_euclid(7)).unwrap_or(0)],
        MONTHS[usize::try_from(month - 1).unwrap_or(0)],
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// The time now, in seconds.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `php_session_send_cookie`: `Set-Cookie` for the session's name and
/// id with the `session.cookie_*` attributes, added beside any other
/// after `php_session_remove_cookie` dropped the one an earlier id sent,
/// so a regenerated id leaves one session cookie in the head:
///
/// ```php
/// ob_start();
/// session_start();
/// session_regenerate_id();
/// $cookies = array_filter(headers_list(), fn($h) => str_starts_with($h, "Set-Cookie: PHPSESSID="));
/// var_dump(count($cookies));
/// session_destroy();
/// header_remove("Set-Cookie");
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.12
/// Expires: Thu, 19 Nov 1981 08:52:00 GMT
/// Cache-Control: no-store, no-cache, must-revalidate
/// Pragma: no-cache
/// Content-type: text/html; charset=UTF-8
///
/// int(1)
/// ```
///
/// A cookie the host refuses fails the start, as `sapi_add_header_ex`
/// answers since 8.5.12:
///
/// ```php
/// ob_start();
/// set_error_handler(function ($n, $s) { echo "handler: $s\n"; return true; });
/// session_set_cookie_params(["path" => "/\r\nX-Injected: yes"]);
/// session_save_path(__DIR__);
/// var_dump(session_start(["cache_limiter" => ""]), session_status() === PHP_SESSION_NONE);
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.12
/// Content-type: text/html; charset=UTF-8
///
/// handler: Header may not contain more than a single header, new line detected
/// bool(false)
/// bool(true)
/// ```
fn send_cookie(req: &mut Request, id: &[u8]) -> bool {
    if headers_sent(req) {
        sent_error(
            req,
            "Session cookie cannot be sent after headers have already been sent",
        );
        return false;
    }
    let secure = ini_bool(req, b"session.cookie_secure");
    if ini_bool(req, b"session.cookie_partitioned") && !secure {
        docref(
            req,
            Level::Warning,
            "Partitioned session cookie cannot be used without also configuring it as secure",
        );
        return false;
    }
    let mut cookie = b"Set-Cookie: ".to_vec();
    cookie.extend_from_slice(&ini(req, b"session.name"));
    cookie.push(b'=');
    cookie.extend_from_slice(&url_encode(id));
    let lifetime = req.ini().long(b"session.cookie_lifetime");
    if lifetime > 0 {
        let t = now().saturating_add(lifetime);
        if t > 0 {
            cookie.extend_from_slice(
                format!("; expires={} GMT; Max-Age={lifetime}", gmt(t)).as_bytes(),
            );
        }
    }
    for (entry, attribute) in [
        (b"session.cookie_path".as_slice(), "; path="),
        (b"session.cookie_domain", "; domain="),
    ] {
        let value = ini(req, entry);
        if !value.is_empty() {
            cookie.extend_from_slice(attribute.as_bytes());
            cookie.extend_from_slice(&value);
        }
    }
    for (entry, attribute) in [
        (b"session.cookie_secure".as_slice(), "; secure"),
        (b"session.cookie_partitioned", "; Partitioned"),
        (b"session.cookie_httponly", "; HttpOnly"),
    ] {
        if ini_bool(req, entry) {
            cookie.extend_from_slice(attribute.as_bytes());
        }
    }
    let samesite = ini(req, b"session.cookie_samesite");
    if !samesite.is_empty() {
        cookie.extend_from_slice(b"; SameSite=");
        cookie.extend_from_slice(&samesite);
    }
    let name = ini(req, b"session.name");
    let mut prefix = b"Set-Cookie: ".to_vec();
    prefix.extend_from_slice(&name);
    prefix.push(b'=');
    req.response_mut()
        .header_remove_prefix(&prefix, b"Set-Cookie".len());
    // What `sapi_add_header_ex` answers, since 8.5.12: a header the host
    // refuses, one with a newline in it, fails the cookie and the start.
    req.response_mut().header_add(&cookie, false, 0)
}

/// `php_session_cache_limiter`: the headers of `session.cache_limiter`,
/// `0` for none to send, `-1` for a limiter php does not know or a
/// session not active, and `-2` after php's warning where the headers
/// are sent.
fn cache_limiter(req: &mut Request) -> Result<i32, Thrown> {
    let limiter = ini(req, b"session.cache_limiter");
    if limiter.is_empty() {
        return Ok(0);
    }
    if req.state::<State>().status != Status::Active {
        return Ok(-1);
    }
    if headers_sent(req) {
        abort(req)?;
        sent_error(
            req,
            "Session cache limiter cannot be sent after headers have already been sent",
        );
        return Ok(-2);
    }
    let expire = req.ini().long(b"session.cache_expire").saturating_mul(60);
    let limiter = c_str(&limiter).to_ascii_lowercase();
    let past = "Expires: Thu, 19 Nov 1981 08:52:00 GMT".to_owned();
    let private = format!("Cache-Control: private, max-age={expire}");
    let headers: Vec<String> = match limiter.as_slice() {
        b"public" => vec![
            format!("Expires: {} GMT", gmt(now().saturating_add(expire))),
            format!("Cache-Control: public, max-age={expire}"),
        ],
        b"private" => vec![past, private],
        b"private_no_expire" => vec![private],
        b"nocache" => vec![
            past,
            "Cache-Control: no-store, no-cache, must-revalidate".to_owned(),
            "Pragma: no-cache".to_owned(),
        ],
        _ => return Ok(-1),
    };
    for header in &headers {
        req.response_mut().header_add(header.as_bytes(), true, 0);
    }
    if limiter != b"nocache"
        && let Some(script) = req.environment().script_name().map(<[u8]>::to_vec)
        // `VCWD_STAT(path_translated)`, the host's own stat, which no
        // wrapper and no `open_basedir` guard stands before.
        && let Ok(stat) = std::fs::metadata(req.environment().absolutize(&script))
    {
        use std::os::unix::fs::MetadataExt;
        req.response_mut().header_add(
            format!("Last-Modified: {} GMT", gmt(stat.mtime())).as_bytes(),
            true,
            0,
        );
    }
    Ok(0)
}
