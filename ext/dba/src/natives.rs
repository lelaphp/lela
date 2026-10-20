//! The natives of `dba.c`, in `dba.stub.php`'s order, over the links of
//! [`crate::link`].

use std::ops::ControlFlow;

use php_ext::abi::{False, Instance, OrFalse, Raw, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{c_str, coerce_param};
use php_ext::decl::{Name, Type};
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_error, arity, deprecated, docref, pending, throw,
};
use php_ext::request::{Cast, OwnedStream as _, Request, Resolve, Stream, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::flatfile::{Flatfile, Stored};
use crate::inifile::Key as IniKey;
use crate::link::{Access, BadMode, Handler, Held, Link, Locking, Mode, Store, not_possible};
use crate::{Conn, Connection, LinkId, State, default_handler};

/// `dba_popen(string $path, string $mode, ?string $handler = null, int
/// $permission = 0o644, int $map_size = 0, ?int $flags = null):
/// Dba\Connection|false`: `dba_open` kept in the persistent list, so a
/// second ask by the same path, `$mode` and handler answers another object
/// over the same link.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_popen_' . getmypid() . '.db';
/// $a = dba_popen($f, 'n', 'flatfile'); dba_insert('k', 'v', $a);
/// $b = dba_popen($f, 'n', 'flatfile');
/// var_dump($a === $b, dba_fetch('k', $b));
/// dba_close($b); var_dump(dba_fetch('k', $a));
/// var_dump(dba_popen('/inexistent', 'r'));
/// unlink($f);
/// ```
/// ```expect
/// bool(false)
/// string(1) "v"
/// string(1) "v"
///
/// Warning: dba_popen(/inexistent): Failed to open stream: No such file or directory in %s on line 7
/// bool(false)
/// ```
#[php_function]
pub(crate) fn dba_popen(
    req: &mut Request,
    path: php_ext::abi::Path,
    mode: PhpString,
    #[php(default = null)] handler: Option<PhpString>,
    #[php(default = 0644)] permission: i64,
    #[php(default = 0)] map_size: i64,
    #[php(default = null)] flags: Option<i64>,
) -> Result<OrFalse<Owner<Instance<Connection>>>, Thrown> {
    let asked = Asked {
        path: path.0.as_bytes().to_vec(),
        mode: mode.as_bytes().to_vec(),
        handler: handler.map(|h| h.as_bytes().to_vec()),
        permission,
        map_size,
        flags,
    };
    open(req, &asked, true)
}

/// `dba_open(string $path, string $mode, ?string $handler = null, int
/// $permission = 0o644, int $map_size = 0, ?int $flags = null):
/// Dba\Connection|false`: `php_dba_open`, the arguments checked, the
/// handler and `$mode` decoded, the `flock(2)` taken and the database stream
/// opened before the handler's own open.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_open_' . getmypid() . '.db';
/// var_dump(dba_open($f, 'n', 'bogus'));
/// foreach (['', 'q', 'nq', 'rdq', 'n-t', 'rwxy'] as $m) {
///     try { dba_open($f, $m); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// try { dba_open('', 'r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { dba_open($f, 'r', ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { dba_open($f, 'r', null, 0644, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { dba_open($f, 'r', null, 0644, 0, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $w = dba_open($f, 'n', 'flatfile');
/// var_dump(dba_open($f, 'rdt', 'flatfile'), dba_open($f, 'rlt', 'flatfile'), dba_open($f, 'rt', 'flatfile'));
/// dba_close($w);
/// var_dump(dba_open($f, 'c', 'cdb'));
/// unlink($f);
/// ```
/// ```expect
/// Warning: dba_open(): Handler "bogus" is not available in %s on line 3
/// bool(false)
/// dba_open(): Argument #2 ($mode) must not be empty
/// dba_open(): Argument #2 ($mode) first character must be one of "r", "w", "c", or "n"
/// dba_open(): Argument #2 ($mode) second character must be one of "d", "l", "-", or "t"
/// dba_open(): Argument #2 ($mode) third character must be "t"
/// dba_open(): Argument #2 ($mode) cannot combine mode "-" (no lock) and "t" (test lock)
/// dba_open(): Argument #2 ($mode) must be at most 3 characters
/// dba_open(): Argument #1 ($path) must not be empty
/// dba_open(): Argument #3 ($handler) must not be empty
/// dba_open(): Argument #5 ($map_size) must be greater than or equal to 0
/// dba_open(): Argument #6 ($flags) must be greater than or equal to 0
///
/// Warning: dba_open(): Driver initialization failed for handler: flatfile: Unable to establish lock (database file already open) in %s on line 12
///
/// Warning: dba_open(): Driver initialization failed for handler: flatfile: Unable to establish lock (database file already open) in %s on line 12
///
/// Warning: dba_open(): Handler flatfile doesn't uses locking for this mode which makes modifier t (test lock) obsolete in %s on line 12
/// bool(false)
/// bool(false)
/// bool(false)
///
/// Warning: dba_open(): Driver initialization failed for handler: cdb: Update operations are not supported in %s on line 14
/// bool(false)
/// ```
#[php_function]
pub(crate) fn dba_open(
    req: &mut Request,
    path: php_ext::abi::Path,
    mode: PhpString,
    #[php(default = null)] handler: Option<PhpString>,
    #[php(default = 0644)] permission: i64,
    #[php(default = 0)] map_size: i64,
    #[php(default = null)] flags: Option<i64>,
) -> Result<OrFalse<Owner<Instance<Connection>>>, Thrown> {
    let asked = Asked {
        path: path.0.as_bytes().to_vec(),
        mode: mode.as_bytes().to_vec(),
        handler: handler.map(|h| h.as_bytes().to_vec()),
        permission,
        map_size,
        flags,
    };
    open(req, &asked, false)
}

/// What `dba_open` and `dba_popen` were handed.
struct Asked {
    path: Vec<u8>,
    mode: Vec<u8>,
    handler: Option<Vec<u8>>,
    permission: i64,
    map_size: i64,
    flags: Option<i64>,
}

/// A C string's bytes as `%s` prints them.
fn c_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(c_str(bytes)).into_owned()
}

/// `object_init_ex(return_value, dba_connection_ce)`.
fn new_connection(req: &mut Request) -> Result<Owner<PhpObject>, Thrown> {
    let Some(class) = lookup_class(req, b"Dba\\Connection")? else {
        return Err(throw(req, "Error", "Class \"Dba\\Connection\" not found"));
    };
    new_object(req, class)
}

/// The connection listed under its key, in place of any listed there.
fn list(req: &mut Request, object: PhpObject, link: LinkId, key: &[u8]) -> Result<(), Thrown> {
    let key = req.state::<State>().key(key);
    *object.native_mut_or_fatal::<Option<Conn>>(req)? = Some(Conn { link, key });
    let state = req.state::<State>();
    state.forget_connection(key);
    state.connections.push((key, object));
    Ok(())
}

/// `php_dba_open`.
fn open(
    req: &mut Request,
    asked: &Asked,
    persistent: bool,
) -> Result<OrFalse<Owner<Instance<Connection>>>, Thrown> {
    if asked.path.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if asked.mode.is_empty() {
        return Err(arg_value_error(req, 2, "not be empty"));
    }
    if asked.handler.as_ref().is_some_and(Vec::is_empty) {
        return Err(arg_value_error(req, 3, "not be empty"));
    }
    if asked.map_size < 0 {
        return Err(arg_value_error(req, 5, "be greater than or equal to 0"));
    }
    if asked.flags.is_some_and(|f| f < 0) {
        return Err(arg_value_error(req, 6, "be greater than or equal to 0"));
    }
    let counter = if persistent {
        0
    } else {
        let state = req.state::<State>();
        let n = state.counter;
        state.counter = n.wrapping_add(1);
        n
    };
    let key = format!(
        "dba_{}_{}_{}_{}_{}",
        u8::from(persistent),
        counter,
        c_text(&asked.path),
        c_text(&asked.mode),
        asked.handler.as_deref().map(c_text).unwrap_or_default()
    )
    .into_bytes();
    if persistent {
        let held = &mut req.state::<State>().persistent;
        let mut found = held.find(&key).and_then(|at| held.get_mut(at).copied());
        if found.is_none() {
            let kept = req
                .engine::<crate::Pooled>()
                .and_then(|crate::Pooled(pool)| pool.take(&key));
            if let Some(kept) = kept {
                let link = kept.take_up(req)?;
                let state = req.state::<State>();
                let id = state.store(link);
                state.persistent.hold(key.clone(), id);
                found = Some(id);
            }
        }
        if let Some(link) = found {
            let object = new_connection(req)?;
            if let Err(thrown) = list(req, *object, link, &key) {
                object.give_up(req);
                return Err(thrown);
            }
            return Ok(OrFalse::Value(object.into_instance()));
        }
    }
    let Some(handler) = handler_asked(req, asked.handler.as_deref()) else {
        return Ok(OrFalse::False);
    };
    let mode = match Mode::decode(&asked.mode) {
        Ok(mode) => mode,
        Err(BadMode::TooLong) => {
            return Err(arg_value_error(req, 2, "be at most 3 characters"));
        }
        Err(bad) => return Err(argument_error(req, "ValueError", 2, bad.text())),
    };
    let lock = mode.lock();
    if mode.test && lock == Held::None {
        let text = format!(
            "Handler {} doesn't uses locking for this mode which makes modifier t (test lock) obsolete",
            handler.name()
        );
        docref(req, Level::Warning, &text);
        return Ok(OrFalse::False);
    }
    let object = new_connection(req)?;
    let Some(link) = open_link(req, asked, handler, mode, persistent)? else {
        object.give_up(req);
        return Ok(OrFalse::False);
    };
    let id = req.state::<State>().store(link);
    if persistent {
        req.state::<State>().persistent.hold(key.clone(), id);
    }
    if let Err(thrown) = list(req, *object, id, &key) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(object.into_instance()))
}

/// The handler `$handler` names, or the default one; `None` after php's
/// warning for neither.
fn handler_asked(req: &mut Request, name: Option<&[u8]>) -> Option<Handler> {
    let Some(name) = name else {
        let handler = default_handler(req);
        if handler.is_none() {
            docref(req, Level::Warning, "No default handler selected");
        }
        return handler;
    };
    let handler = Handler::named(name);
    if handler.is_none() {
        let text = format!("Handler \"{}\" is not available", c_text(name));
        docref(req, Level::Warning, &text);
    }
    handler
}

/// `php_dba_find`: the `flock(2)` a listed connection holds on `path`.
fn lock_held_on(req: &mut Request, path: &[u8]) -> Option<Held> {
    let objects: Vec<PhpObject> = req
        .state::<State>()
        .connections
        .iter()
        .map(|(_, object)| *object)
        .collect();
    for object in objects {
        let Some(Some(conn)) = object.native::<Option<Conn>>(req) else {
            continue;
        };
        let id = conn.link;
        if let Some(link) = req.state::<State>().link(id)
            && link.path == path
        {
            return Some(link.lock);
        }
    }
    None
}

/// `opened_path` as the plain files wrapper sets it, `expand_filepath`
/// of the path; `None` for a path another wrapper opens, which sets
/// none (`php_stream_locate_url_wrapper`'s scheme, `data:` among them).
fn opened_path(req: &mut Request, path: &[u8]) -> Option<Vec<u8>> {
    let scheme = path
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')));
    let path = match scheme {
        Some(at) if path[at..].starts_with(b"://") => path.strip_prefix(b"file://")?,
        Some(at) if path[at] == b':' && path[..at].eq_ignore_ascii_case(b"data") => return None,
        _ => path,
    };
    let cwd = req
        .environment()
        .cwd()
        .into_os_string()
        .into_encoded_bytes();
    req.environment_mut()
        .virtual_file_ex(&cwd, path, Resolve::FilePath)
}

/// `php_stream_open_wrapper` under `STREAM_MUST_SEEK`, `IGNORE_PATH`
/// and, with `warn`, `REPORT_ERRORS`.
fn open_stream(
    req: &mut Request,
    name: &[u8],
    mode: &[u8],
    warn: bool,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let opening = php_ext::request::Opening {
        warn,
        must_seek: true,
        ..Default::default()
    };
    req.open_stream_with(name, mode, opening)
}

/// The rest of `php_dba_open` past the object: the `flock(2)` checked against
/// the links listed, taken on the database or its `.lck` file, the
/// database stream opened for a handler that has one (the `.lck` file's
/// unless the database is locked), cast to an fd, and the handler's open;
/// `None` after php's warning for any step that failed, every stream
/// opened freed.
fn open_link(
    req: &mut Request,
    asked: &Asked,
    handler: Handler,
    mode: Mode,
    persistent: bool,
) -> Result<Option<Link>, Thrown> {
    let lock = mode.lock();
    let mut path = asked.path.clone();
    let mut error: Option<&'static str> = None;
    if lock_held_on(req, &path).is_some_and(|other| lock.conflicts(other)) {
        error = Some("Unable to establish lock (database file already open)");
    }
    let on_database = mode.locking == Locking::Database;
    let mut lock_file = None;
    if error.is_none() && lock != Held::None {
        let mut lock_mode = mode.lock_file_mode();
        let name = if on_database {
            path.clone()
        } else {
            let mut name = path.clone();
            name.extend_from_slice(b".lck");
            if mode.access == Access::Read {
                // A reader tries the lock file that is there first, and
                // says nothing where it is not.
                lock_file = open_stream(req, &name, b"rb", false)?;
            }
            if lock_file.is_none() {
                lock_mode = b"a+b";
            }
            name
        };
        if lock_file.is_none() {
            lock_file = open_stream(req, &name, lock_mode, true)?;
            if lock_file.is_some() && on_database {
                match opened_path(req, &name) {
                    Some(opened) => path = opened,
                    None => error = Some("Unable to determine path for locking"),
                }
            }
        }
        let Some(held) = lock_file.as_deref() else {
            // The stream's own warning was raised.
            return Ok(None);
        };
        let operation = lock.operation() | if mode.test { 4 } else { 0 };
        if error.is_none() && !held.flock(req, operation) {
            error = Some("Unable to establish lock");
        }
    }
    // Locked on the database, the lock's stream is the database's own.
    let (file, lock_file) = if error.is_some() || !handler.streams() {
        (None, lock_file)
    } else if on_database && lock_file.is_some() {
        (lock_file, None)
    } else {
        match open_stream(req, &path, mode.file_mode(), true) {
            Ok(Some(file)) => (Some(file), lock_file),
            opened => {
                free_streams(req, None, lock_file);
                return opened.map(|_| None);
            }
        }
    };
    if let Some(held) = file.as_deref()
        && held.cast(req, Cast::Fd, true).is_none()
    {
        docref(req, Level::Warning, "Could not cast stream");
        free_streams(req, file, lock_file);
        return Ok(None);
    }
    let at = library_path(req, &path);
    let store = match error {
        Some(why) => Err((why, file)),
        None => Store::open(req, handler, mode.access, file, &at, asked.permission),
    };
    match store {
        Ok(store) => Ok(Some(Link {
            path,
            access: mode.access,
            handler,
            persistent,
            lock,
            lock_file,
            store,
        })),
        Err((why, file)) => {
            refuse(req, handler, Some(why), file, lock_file)?;
            Ok(None)
        }
    }
}

/// `info->path` as a library that opens it itself reads it: relative to
/// the request's working directory, which php's process shares.
fn library_path(req: &Request, path: &[u8]) -> Vec<u8> {
    if path.first() == Some(&b'/') {
        return path.to_vec();
    }
    let mut at = req
        .environment()
        .cwd()
        .into_os_string()
        .into_encoded_bytes();
    at.push(b'/');
    at.extend_from_slice(path);
    at
}

/// php's warning for a link the `flock(2)`, the cast or the handler's open
/// refused, unless a throwable is pending, and the streams freed.
fn refuse(
    req: &mut Request,
    handler: Handler,
    why: Option<&'static str>,
    file: Option<Owner<Stream>>,
    lock_file: Option<Owner<Stream>>,
) -> Result<(), Thrown> {
    let raised = pending(req);
    if raised.is_ok() {
        let text = match why {
            Some(why) if !why.is_empty() => format!(
                "Driver initialization failed for handler: {}: {why}",
                handler.name()
            ),
            _ => format!(
                "Driver initialization failed for handler: {}",
                handler.name()
            ),
        };
        docref(req, Level::Warning, &text);
    }
    free_streams(req, file, lock_file);
    raised
}

/// `dba_close_info`'s frees for a link that never opened.
fn free_streams(req: &mut Request, file: Option<Owner<Stream>>, lock: Option<Owner<Stream>>) {
    for stream in file.into_iter().chain(lock) {
        stream.free(req);
    }
}

/// `CHECK_DBA_CONNECTION`: the open connection's link taken out of the
/// table for the operation, `f`'s answer, and the link put back.
fn with_link<R>(
    req: &mut Request,
    dba: Instance<Connection>,
    f: impl FnOnce(&mut Request, &mut Link) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let id = match dba.payload_mut(req)? {
        Some(conn) => conn.link,
        None => {
            return Err(throw(
                req,
                "Error",
                "DBA connection has already been closed",
            ));
        }
    };
    let Some(mut link) = req.state::<State>().slot(id).and_then(Option::take) else {
        return Err(throw(
            req,
            "Error",
            "DBA connection has already been closed",
        ));
    };
    let answer = f(req, &mut link);
    match req.state::<State>().slot(id) {
        Some(slot) => *slot = Some(link),
        None => link.close(req),
    }
    answer
}

/// `php_dba_make_key`: an array key is `[group]name` of its two
/// elements, the name alone where the group is empty.
fn key_bytes(req: &mut Request, key: Union<PhpArray, PhpString>) -> Result<Vec<u8>, Thrown> {
    let array = match key {
        Union::B(key) => return Ok(key.as_bytes().to_vec()),
        Union::A(array) => array,
    };
    if array.len() != 2 {
        return Err(argument_error(
            req,
            "Error",
            1,
            "must have exactly two elements: \"key\" and \"name\"",
        ));
    }
    let mut parts = Vec::with_capacity(2);
    // `zend_hash_get_current_data_ex` and `zend_hash_move_forward_ex` over a
    // position of its own (`Z/ext/dba/dba.c:107`), no owner of the table
    // taken: the argument holds it while `zval_try_get_string` runs
    // `__toString`. A table freed after the group ends the walk, and the
    // name is then empty.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        parts.push(php_ext::coerce::with_string(
            req,
            row.value(),
            <[u8]>::to_vec,
        )?);
        Ok(ControlFlow::Continue(()))
    })?;
    let mut parts = parts.into_iter();
    let (group, name) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    if group.is_empty() {
        return Ok(name);
    }
    let mut out = b"[".to_vec();
    out.extend_from_slice(c_text(&group).as_bytes());
    out.push(b']');
    out.extend_from_slice(c_text(&name).as_bytes());
    Ok(out)
}

/// The bytes as a string of the request's.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, bytes)
}

/// `dba_close(Dba\Connection $dba): void`: the connection closed; a
/// persistent link stays open for the next `dba_popen`.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_close_' . getmypid() . '.db';
/// $d = dba_open($f, 'n', 'flatfile'); dba_close($d);
/// try { dba_close($d); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($d); unlink($f);
/// ```
/// ```expect
/// DBA connection has already been closed
/// object(Dba\Connection)#1 (0) {
/// }
/// ```
#[php_function]
pub(crate) fn dba_close(req: &mut Request, dba: Instance<Connection>) -> Result<(), Thrown> {
    match dba.payload_mut(req)?.take() {
        Some(conn) => {
            crate::close_connection(req, conn);
            Ok(())
        }
        None => Err(throw(
            req,
            "Error",
            "DBA connection has already been closed",
        )),
    }
}

/// `dba_exists(string|array $key, Dba\Connection $dba): bool`.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_exists_' . getmypid() . '.db';
/// $d = dba_open($f, 'n', 'flatfile'); dba_insert(['g', 'n'], 'v', $d);
/// $k = 1; var_dump(dba_exists('[g]n', $d), dba_exists($k, $d), gettype($k));
/// try { dba_exists(['a'], $d); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// dba_close($d); unlink($f);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(7) "integer"
/// dba_exists(): Argument #1 ($key) must have exactly two elements: "key" and "name"
/// ```
#[php_function]
pub(crate) fn dba_exists(
    req: &mut Request,
    key: Union<PhpArray, PhpString>,
    dba: Instance<Connection>,
) -> Result<bool, Thrown> {
    with_link(req, dba, |req, link| {
        let key = key_bytes(req, key)?;
        Ok(match &mut link.store {
            #[cfg(target_os = "macos")]
            Store::Ndbm(db) => db.as_mut().and_then(|db| db.fetch(&key)).is_some(),
            Store::Flatfile(_, file) => Flatfile::fetch(req, **file, &key)?.is_some(),
            Store::Inifile(ini, file) => ini.fetch(req, **file, &IniKey::split(&key), 0)?.is_some(),
            Store::CdbReader(reader, file) => reader.exists(req, **file, &key),
            Store::CdbMaker(..) => false,
        })
    })
}

/// `dba_fetch(string|array $key, $dba, $skip = 0): string|false`, with
/// the deprecated legacy order `($key, $skip, $dba)`: the value under the
/// key; cdb skips `$skip` records and inifile `$skip` lines under it,
/// where any other handler ignores it with php's notice.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_fetch_' . getmypid() . '.ini';
/// $d = dba_open($f, 'n', 'inifile');
/// dba_insert('foo', 23, $d); dba_insert('foo', 42, $d);
/// var_dump(dba_fetch('foo', $d, -1), dba_fetch('foo', $d, -1), dba_fetch('foo', $d, 1), dba_fetch('foo', 0, $d));
/// var_dump(dba_fetch('foo', $d, -2));
/// try { dba_fetch('foo', $d, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { dba_fetch('foo', 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// dba_close($d); unlink($f);
/// $g = dba_open($f, 'n', 'flatfile'); dba_insert('a', 'b', $g); var_dump(dba_fetch('a', $g, 1)); dba_close($g); unlink($f);
/// ```
/// ```expect
/// Deprecated: Calling dba_fetch() with $dba at the 3rd parameter is deprecated in %s on line 5
/// string(2) "23"
/// string(2) "42"
/// string(2) "42"
/// string(2) "23"
///
/// Notice: dba_fetch(): Handler inifile accepts only skip value -1 and greater, using skip=0 in %s on line 6
/// string(2) "23"
/// dba_fetch(): Argument #3 ($skip) must be between -2147483648 and 2147483647
/// dba_fetch(): Argument #2 ($dba) must be of type Dba\Connection, int given
///
/// Notice: dba_fetch(): Handler flatfile does not support optional skip parameter, the value will be ignored in %s on line 10
/// string(1) "b"
/// ```
#[php_function(arity = manual)]
pub(crate) fn dba_fetch(
    req: &mut Request,
    key: Raw<Union<PhpArray, PhpString>>,
    dba: Raw<Untyped<Value>>,
    #[php(default = 0)] skip: Raw<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let three = req.argument_count() == 3;
    // The legacy `($key, $skip, $dba)`, tried quietly first as php's
    // `ZEND_PARSE_PARAMS_QUIET` tries it.
    let legacy = three && is_connection(req, &skip.value()) && longish(&dba.value());
    let (key, skip, object) = if legacy {
        let key = key.hold(req)?;
        // The legacy order's `Z_PARAM_LONG`, where the stub declares `$dba`
        // untyped.
        let skip = long_of(&coerce_param(req, &dba.value(), &Type::Int, 2)?);
        deprecated(
            req,
            "Calling dba_fetch() with $dba at the 3rd parameter is deprecated",
        );
        pending(req)?;
        (key, skip, skip_given(&skip_arg(req)))
    } else {
        arity(req, 2, Some(3))?;
        let key = key.hold(req)?;
        let class = Type::Named(Name(b"Dba\\Connection".as_slice().into()));
        // `Z_PARAM_OBJECT_OF_CLASS` over `Dba\Connection` and
        // `Z_PARAM_LONG`, where the stub declares `$dba` and `$skip`
        // untyped for the legacy order.
        let object = coerce_param(req, &dba.value(), &class, 2)?;
        let skip = if three {
            long_of(&coerce_param(req, &skip.value(), &Type::Int, 3)?)
        } else {
            0
        };
        (key, skip, object)
    };
    if i32::try_from(skip).is_err() {
        return Err(arg_value_error(
            req,
            3,
            &format!("be between {} and {}", i32::MIN, i32::MAX),
        ));
    }
    let object = object.deref();
    let ValueRef::Object(object) = object.as_ref() else {
        return Ok(OrFalse::False);
    };
    let dba = Instance::<Connection>::of(*object);
    let value = with_link(req, dba, |req, link| {
        let key = key_bytes(req, key)?;
        let mut skip = skip;
        if skip != 0 {
            let name = link.handler.name();
            let text = match link.handler {
                Handler::Cdb if skip < 0 => Some(format!(
                    "Handler {name} accepts only skip values greater than or equal to zero, using skip=0"
                )),
                Handler::Inifile if skip < -1 => Some(format!(
                    "Handler {name} accepts only skip value -1 and greater, using skip=0"
                )),
                Handler::Cdb | Handler::Inifile => None,
                #[cfg(target_os = "macos")]
                Handler::Ndbm => Some(format!(
                    "Handler {name} does not support optional skip parameter, the value will be ignored"
                )),
                Handler::CdbMake | Handler::Flatfile => Some(format!(
                    "Handler {name} does not support optional skip parameter, the value will be ignored"
                )),
            };
            if let Some(text) = text {
                docref(req, Level::Notice, &text);
                skip = 0;
            }
        }
        Ok(match &mut link.store {
            #[cfg(target_os = "macos")]
            Store::Ndbm(db) => db.as_mut().and_then(|db| db.fetch(&key)),
            Store::Flatfile(_, file) => Flatfile::fetch(req, **file, &key)?,
            Store::Inifile(ini, file) => ini.fetch(req, **file, &IniKey::split(&key), skip)?,
            Store::CdbReader(reader, file) => reader.fetch(req, **file, &key, skip),
            Store::CdbMaker(..) => None,
        })
    })?;
    or_false(req, value)
}

/// The third argument, the connection of the legacy order.
fn skip_arg(req: &Request) -> Value {
    req.argument(3).unwrap_or_default()
}

/// The value a reference holds.
fn skip_given(value: &Value) -> Value {
    value.deref()
}

/// An `int` the coercion made.
fn long_of(value: &Value) -> i64 {
    match value.as_ref() {
        ValueRef::Int(n) => n,
        _ => 0,
    }
}

/// Whether the value is a `Dba\Connection`.
fn is_connection(req: &mut Request, value: &Value) -> bool {
    let value = value.deref();
    let ValueRef::Object(object) = value.as_ref() else {
        return false;
    };
    match lookup_class(req, b"Dba\\Connection") {
        Ok(Some(class)) => object.instance_of(req, class),
        _ => false,
    }
}

/// Whether `Z_PARAM_LONG` would take the value quietly.
fn longish(value: &Value) -> bool {
    matches!(
        value.deref().as_ref(),
        ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_)
    )
}

/// `dba_key_split(string|false|null $key): array|false`: `[group]name`
/// split into the group and the name, the group empty for a bare name;
/// `false` and `null` are deprecated and answer `false`.
///
/// ```php
/// var_dump(dba_key_split(null), dba_key_split("[a]b[c]d"), dba_key_split("x]"));
/// ```
/// ```expect
/// Deprecated: dba_key_split(): Passing false or null is deprecated since 8.4 in %s on line 2
/// bool(false)
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(5) "b[c]d"
/// }
/// array(2) {
///   [0]=>
///   string(0) ""
///   [1]=>
///   string(2) "x]"
/// }
/// ```
///
/// The key is read through the argument, which keeps its owner: a
/// string the program built is still its own after the call, and an
/// integer is read as its string.
///
/// ```php
/// $s = str_repeat("[g]", 2) . "n"; $r = dba_key_split($s); echo $s, ' ', $r[0], ' ', $r[1], "\n";
/// $r = dba_key_split(12); echo $r[0], '|', $r[1], "\n";
/// ```
/// ```expect
/// [g][g]n g [g]n
/// |12
/// ```
#[php_function]
pub(crate) fn dba_key_split(
    req: &mut Request,
    key: Raw<Option<Union<PhpString, False>>>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let value = key.value();
    if matches!(
        value.deref().as_ref(),
        ValueRef::Null | ValueRef::Bool(false)
    ) {
        docref(
            req,
            Level::Deprecated,
            "Passing false or null is deprecated since 8.4",
        );
        return Ok(OrFalse::False);
    }
    // `"s"` past the `null` and `false` arm, where the stub declares
    // `string|false|null $key`.
    let held = coerce_param(req, &value, &Type::String, 1)?;
    let ValueRef::Str(text) = held.as_ref() else {
        return Ok(OrFalse::False);
    };
    let bytes = text.as_bytes().to_vec();
    let (group, name) = match bytes.iter().position(|&b| b == b']') {
        Some(close) if bytes.first() == Some(&b'[') => (&bytes[1..close], &bytes[close + 1..]),
        _ => (&[][..], &bytes[..]),
    };
    let mut array = PhpArray::new(req)?;
    for part in [group, name] {
        let part = string(req, part)?;
        array.append(req, part.into_value())?;
    }
    Ok(OrFalse::Value(array))
}

/// `dba_firstkey(Dba\Connection $dba): string|false`: the walk restarted
/// at the first key.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_firstkey_' . getmypid() . '.db';
/// $d = dba_open($f, 'n', 'flatfile');
/// var_dump(dba_firstkey($d));
/// dba_insert('a', '1', $d); dba_insert('b', '2', $d); dba_delete('a', $d);
/// var_dump(dba_firstkey($d), dba_nextkey($d));
/// dba_close($d); unlink($f);
/// ```
/// ```expect
/// bool(false)
/// string(1) "b"
/// bool(false)
/// ```
#[php_function]
pub(crate) fn dba_firstkey(
    req: &mut Request,
    dba: Instance<Connection>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let key = with_link(req, dba, |req, link| {
        Ok(match &mut link.store {
            #[cfg(target_os = "macos")]
            Store::Ndbm(db) => db.as_mut().and_then(crate::ffi::Ndbm::first_key),
            Store::Flatfile(walk, file) => walk.first_key(req, **file)?,
            Store::Inifile(ini, file) => ini.first_key(req, **file)?,
            Store::CdbReader(reader, file) => reader.first_key(req, **file),
            Store::CdbMaker(..) => None,
        })
    })?;
    or_false(req, key)
}

/// `dba_nextkey(Dba\Connection $dba): string|false`: the walk's next
/// key.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_nextkey_' . getmypid() . '.ini';
/// $d = dba_open($f, 'n', 'inifile');
/// var_dump(dba_nextkey($d));
/// dba_insert('a', '1', $d); dba_insert('[g]b', '2', $d);
/// var_dump(dba_firstkey($d), dba_nextkey($d), dba_nextkey($d), dba_nextkey($d));
/// dba_close($d); unlink($f);
/// ```
/// ```expect
/// bool(false)
/// string(1) "a"
/// string(3) "[g]"
/// string(4) "[g]b"
/// bool(false)
/// ```
#[php_function]
pub(crate) fn dba_nextkey(
    req: &mut Request,
    dba: Instance<Connection>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let key = with_link(req, dba, |req, link| {
        Ok(match &mut link.store {
            #[cfg(target_os = "macos")]
            Store::Ndbm(db) => db.as_mut().and_then(crate::ffi::Ndbm::next_key),
            Store::Flatfile(walk, file) => walk.next_key(req, **file)?,
            Store::Inifile(ini, file) if ini.walking() => ini.next_key(req, **file)?,
            Store::Inifile(..) | Store::CdbMaker(..) => None,
            Store::CdbReader(reader, file) => reader.next_key(req, **file),
        })
    })?;
    or_false(req, key)
}

/// A key or a value as the native returns it.
fn or_false(
    req: &mut Request,
    bytes: Option<Vec<u8>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match bytes {
        Some(bytes) => Ok(OrFalse::Value(string(req, &bytes)?)),
        None => Ok(OrFalse::False),
    }
}

/// `DBA_WRITE_CHECK`: php's warning for a modification of a database
/// opened to read.
fn read_only(req: &mut Request, link: &Link) -> bool {
    if link.access.writes() {
        return false;
    }
    docref(
        req,
        Level::Warning,
        "Cannot perform a modification on a readonly database",
    );
    true
}

/// `dba_delete(string|array $key, Dba\Connection $dba): bool`.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_delete_' . getmypid() . '.db';
/// $d = dba_open($f, 'n', 'flatfile'); dba_insert('a', '1', $d);
/// var_dump(dba_delete('a', $d), dba_delete('a', $d)); dba_close($d);
/// $r = dba_open($f, 'r', 'flatfile'); var_dump(dba_delete('a', $r)); dba_close($r);
/// $c = dba_open($f, 'n', 'cdb_make'); var_dump(dba_delete('a', $c)); dba_close($c); unlink($f);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
///
/// Warning: dba_delete(): Cannot perform a modification on a readonly database in %s on line 5
/// bool(false)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn dba_delete(
    req: &mut Request,
    key: Union<PhpArray, PhpString>,
    dba: Instance<Connection>,
) -> Result<bool, Thrown> {
    with_link(req, dba, |req, link| {
        if read_only(req, link) {
            return Ok(false);
        }
        let key = key_bytes(req, key)?;
        Ok(match &mut link.store {
            #[cfg(target_os = "macos")]
            Store::Ndbm(db) => db.as_mut().is_some_and(|db| db.delete(&key)),
            Store::Flatfile(_, file) => Flatfile::delete(req, **file, &key)?,
            Store::Inifile(ini, file) => {
                let mut found = false;
                let ok = ini.rewrite(req, **file, &IniKey::split(&key), None, false, &mut found)?;
                ok && found
            }
            Store::CdbReader(..) | Store::CdbMaker(..) => false,
        })
    })
}

/// `php_dba_update`: `dba_insert` and `dba_replace`.
fn update(
    req: &mut Request,
    key: Union<PhpArray, PhpString>,
    value: &[u8],
    dba: Instance<Connection>,
    insert: bool,
) -> Result<bool, Thrown> {
    with_link(req, dba, |req, link| {
        if read_only(req, link) {
            return Ok(false);
        }
        let key = key_bytes(req, key)?;
        Ok(match &mut link.store {
            #[cfg(target_os = "macos")]
            Store::Ndbm(db) => db.as_mut().is_some_and(|db| db.store(&key, value, insert)),
            Store::Flatfile(_, file) => match Flatfile::store(req, **file, &key, value, insert)? {
                Stored::Done => true,
                Stored::Exists => false,
                Stored::Short => {
                    not_possible(req);
                    false
                }
            },
            Store::Inifile(ini, file) => {
                let mut found = false;
                let ok = ini.rewrite(
                    req,
                    **file,
                    &IniKey::split(&key),
                    Some(value),
                    insert,
                    &mut found,
                )?;
                if !ok {
                    not_possible(req);
                }
                ok
            }
            Store::CdbMaker(Some(maker), file) if insert => {
                maker.add(req, **file, &key, value).is_some()
            }
            Store::CdbMaker(..) | Store::CdbReader(..) => false,
        })
    })
}

/// `dba_insert(string|array $key, string $value, Dba\Connection $dba):
/// bool`: the value added under a key not there; inifile adds another
/// line under a key that is.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_insert_' . getmypid() . '.cdb';
/// $d = dba_open($f, 'n', 'cdb_make');
/// var_dump(dba_insert(pack('i', 129), 'Booo!', $d), dba_replace('x', 'y', $d));
/// dba_close($d); var_dump(md5_file($f));
/// $r = dba_open($f, 'r', 'cdb'); var_dump(dba_fetch(pack('i', 129), $r), dba_insert('a', 'b', $r));
/// dba_close($r); unlink($f);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(32) "1f34b74bde3744265acfc21e0f30af95"
///
/// Warning: dba_insert(): Cannot perform a modification on a readonly database in %s on line 6
/// string(5) "Booo!"
/// bool(false)
/// ```
#[php_function]
pub(crate) fn dba_insert(
    req: &mut Request,
    key: Union<PhpArray, PhpString>,
    value: PhpString,
    dba: Instance<Connection>,
) -> Result<bool, Thrown> {
    let value = value.as_bytes().to_vec();
    update(req, key, &value, dba, true)
}

/// `dba_replace(string|array $key, string $value, Dba\Connection $dba):
/// bool`: the value set under the key, whatever was there.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_replace_' . getmypid() . '.ini';
/// $d = dba_open($f, 'c', 'inifile');
/// dba_insert('foo', 'value1', $d); dba_insert('[g]foo', 'x', $d);
/// var_dump(dba_replace('foo', 'value2', $d), dba_fetch('foo', $d), dba_fetch('[G]FOO', $d));
/// dba_close($d); var_dump(file_get_contents($f)); unlink($f);
/// ```
/// ```expect
/// bool(true)
/// string(6) "value2"
/// string(1) "x"
/// string(21) "foo=value2
/// [g]
/// foo=x
/// "
/// ```
#[php_function]
pub(crate) fn dba_replace(
    req: &mut Request,
    key: Union<PhpArray, PhpString>,
    value: PhpString,
    dba: Instance<Connection>,
) -> Result<bool, Thrown> {
    let value = value.as_bytes().to_vec();
    update(req, key, &value, dba, false)
}

/// `dba_optimize(Dba\Connection $dba): bool`: nothing for the bundled
/// handlers, past the write check.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_optimize_' . getmypid() . '.db';
/// $d = dba_open($f, 'n', 'flatfile'); var_dump(dba_optimize($d)); dba_close($d);
/// $r = dba_open($f, 'r', 'flatfile'); var_dump(dba_optimize($r)); dba_close($r); unlink($f);
/// ```
/// ```expect
/// bool(true)
///
/// Warning: dba_optimize(): Cannot perform a modification on a readonly database in %s on line 4
/// bool(false)
/// ```
#[php_function]
pub(crate) fn dba_optimize(req: &mut Request, dba: Instance<Connection>) -> Result<bool, Thrown> {
    with_link(req, dba, |req, link| Ok(!read_only(req, link)))
}

/// `dba_sync(Dba\Connection $dba): bool`: nothing for the bundled
/// handlers, which write through.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_sync_' . getmypid() . '.db';
/// $d = dba_open($f, 'n', 'flatfile'); var_dump(dba_sync($d)); dba_close($d); unlink($f);
/// try { dba_sync($d); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// DBA connection has already been closed
/// ```
#[php_function]
pub(crate) fn dba_sync(req: &mut Request, dba: Instance<Connection>) -> Result<bool, Thrown> {
    with_link(req, dba, |_, _| Ok(true))
}

/// `dba_handlers(bool $full_info = false): array`: the handlers' names,
/// or each name with its library's version.
///
/// ```php
/// var_dump(implode(' ', dba_handlers()), dba_handlers(true)['inifile'], dba_handlers(true)['cdb_make']);
/// ```
/// ```expect
/// string(34) "ndbm cdb cdb_make inifile flatfile"
/// string(52) "1.0, $Id: 4dc687ea6681ffc42006a5fa64ac97f828c52ce1 $"
/// string(53) "0.75, $Id: 570e3625b0cea5c29455cecf40fdfd491c0eb3c6 $"
/// ```
#[php_function]
pub(crate) fn dba_handlers(
    req: &mut Request,
    #[php(default = false)] full_info: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for &handler in Handler::ALL {
        if full_info {
            let version = string(req, handler.version().as_bytes())?;
            array.set_str(req, handler.name().as_bytes(), version.into_value())?;
        } else {
            let name = string(req, handler.name().as_bytes())?;
            array.append(req, name.into_value())?;
        }
    }
    Ok(array)
}

/// `dba_list(): array`: each open connection's path by its object's id.
///
/// ```php
/// $f = sys_get_temp_dir() . '/dba_list_' . getmypid() . '.db';
/// $o = new stdClass; $d = dba_open($f, 'n', 'flatfile');
/// var_dump(array_keys(dba_list())); dba_close($d); var_dump(dba_list()); unlink($f);
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   int(2)
/// }
/// array(0) {
/// }
/// ```
#[php_function]
pub(crate) fn dba_list(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    let objects: Vec<PhpObject> = req
        .state::<State>()
        .connections
        .iter()
        .map(|(_, object)| *object)
        .collect();
    for object in objects {
        let Some(Some(conn)) = object.native::<Option<Conn>>(req) else {
            continue;
        };
        let id = conn.link;
        let Some(path) = req.state::<State>().link(id).map(|link| link.path.clone()) else {
            continue;
        };
        let path = string(req, &path)?;
        array.set(
            req,
            Owner::index(i64::from(object.index())),
            path.into_value(),
        )?;
    }
    Ok(array)
}
