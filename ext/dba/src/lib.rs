//! The `dba` extension: `php-src/ext/dba` as one crate under `ext/`
//! (ADR-0004), behind the `dba` feature as php builds it with
//! `--enable-dba`. `dba.c` with the three handlers php's own C provides,
//! flatfile, inifile and cdb with `cdb_make`, ported with their libraries,
//! every read and write through the engine's streams; and ndbm over the
//! platform's library, `<ndbm.h>` from the C library as the oracle's build
//! links it on macOS, bound in `ffi` alone. The facade and, for that
//! binding, `libc`; one entry, `extension()`; its state on the request
//! (ADR-0005). The phpt subset
//! is `php-src/ext/dba/tests`, through `xtask phpt --subset`.
//!
//! A connection is a `Dba\Connection` object over a link, php's
//! `dba_info`, kept in the request's state. A `dba_popen` link is kept in
//! the request's list of persistent links under php's key, so a second
//! `dba_popen` of one path, `$mode` and handler answers a new object over
//! the same link, and `dba_close` leaves it open, as php's
//! `EG(persistent_list)` does for the life of a CLI process.
#![deny(unsafe_code)]
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

mod cdb;
#[cfg(target_os = "macos")]
mod ffi;
mod flatfile;
mod inifile;
mod link;
mod natives;

use std::cmp::Ordering;

use php_ext::abi::{CloneObj, Extension, ExtensionBuilder, ObjectHandlers};
use php_ext::attr::php_class;
use php_ext::call::Function;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, docref, throw};
use php_ext::request::Request;
use php_ext::values::{PhpObject, TypeTag, Value};

use crate::link::{Handler, Link};

/// `dba.default_handler`.
const DEFAULT_HANDLER: &[u8] = b"dba.default_handler";

/// The extension value (design §7.1): the fifteen natives, the class and
/// the one ini entry.
///
/// ```php
/// var_dump(extension_loaded('dba'), ini_get('dba.default_handler'));
/// ```
/// ```expect
/// bool(true)
/// string(8) "flatfile"
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            natives::dba_popen::declared(),
            natives::dba_open::declared(),
            natives::dba_close::declared(),
            natives::dba_exists::declared(),
            natives::dba_fetch::declared(),
            natives::dba_key_split::declared(),
            natives::dba_firstkey::declared(),
            natives::dba_nextkey::declared(),
            natives::dba_delete::declared(),
            natives::dba_insert::declared(),
            natives::dba_replace::declared(),
            natives::dba_optimize::declared(),
            natives::dba_sync::declared(),
            natives::dba_handlers::declared(),
            natives::dba_list::declared(),
        ])
        .class::<Connection>()
        .request_state::<State>()
        .ini(vec![IniDecl {
            name: Name(DEFAULT_HANDLER.into()),
            default: Some(b"flatfile".as_slice().into()),
            user: true,
            per_dir: true,
            system: true,
        }])
        .on_modify(DEFAULT_HANDLER, on_update_default_handler)
        .info(dba_info)
        .build("dba")
}

/// `PHP_MINFO_FUNCTION(dba)`: the handlers, each with a blank after it,
/// then the ini entry.
///
/// ```php
/// (new ReflectionExtension('dba'))->info();
/// ```
/// ```expect
///
/// dba
///
/// DBA support => enabled
/// Supported handlers => %s
///
/// Directive => Local Value => Master Value
/// dba.default_handler => flatfile => flatfile
/// ```
fn dba_info(req: &mut Request) {
    let handlers: String = Handler::ALL.iter().fold(String::new(), |mut all, h| {
        all.push_str(h.name());
        all.push(' ');
        all
    });
    req.info_table(&[
        &["DBA support", "enabled"],
        &["Supported handlers", &handlers],
    ]);
    req.info_ini_entries(&[DEFAULT_HANDLER]);
}

/// `OnUpdateDefaultHandler`: empty selects none; any other value must
/// name a handler, php's warning and the old value kept otherwise.
///
/// ```php
/// var_dump(ini_set('dba.default_handler', 'nope'), ini_get('dba.default_handler'));
/// var_dump(ini_set('dba.default_handler', 'INIFILE'), ini_set('dba.default_handler', ''));
/// ```
/// ```expect
/// Warning: ini_set(): No such handler: nope in %s on line 2
/// bool(false)
/// string(8) "flatfile"
/// string(8) "flatfile"
/// string(7) "INIFILE"
/// ```
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_default_handler(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    if value.is_empty() || Handler::named(value).is_some() {
        return Ok(true);
    }
    let name = String::from_utf8_lossy(value);
    docref(req, Level::Warning, &format!("No such handler: {name}"));
    Ok(false)
}

/// `DBA_G(default_hptr)`: the handler `dba.default_handler` names.
fn default_handler(req: &Request) -> Option<Handler> {
    req.ini_get(DEFAULT_HANDLER).and_then(Handler::named)
}

/// A link's slot in the request's table, `dba_info *`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LinkId(u32);

/// A connection's key in the request's table of keys, `connection->hash`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct KeyId(u32);

/// Request-scoped (ADR-0005): `DBA_G(connections)`,
/// `DBA_G(connection_counter)`, the links open and the persistent ones
/// by php's key, `EG(persistent_list)`'s `dba persistent` rows.
#[derive(Default)]
struct State {
    /// Each live connection by its key, the object named without an
    /// owner, as php's table holds the zval with no destructor.
    connections: Vec<(KeyId, PhpObject)>,
    /// Every key a connection was given, interned.
    keys: Vec<Vec<u8>>,
    counter: u32,
    links: Vec<Option<Link>>,
    persistent: Vec<(Vec<u8>, LinkId)>,
}

impl State {
    fn store(&mut self, link: Link) -> LinkId {
        let at = if let Some(at) = self.links.iter().position(Option::is_none) {
            self.links[at] = Some(link);
            at
        } else {
            self.links.push(Some(link));
            self.links.len() - 1
        };
        LinkId(u32::try_from(at).unwrap_or(u32::MAX))
    }

    fn slot(&mut self, id: LinkId) -> Option<&mut Option<Link>> {
        self.links.get_mut(usize::try_from(id.0).ok()?)
    }

    fn link(&self, id: LinkId) -> Option<&Link> {
        self.links.get(usize::try_from(id.0).ok()?)?.as_ref()
    }

    fn key(&mut self, key: &[u8]) -> KeyId {
        let at = if let Some(at) = self.keys.iter().position(|held| held.as_slice() == key) {
            at
        } else {
            self.keys.push(key.to_vec());
            self.keys.len() - 1
        };
        KeyId(u32::try_from(at).unwrap_or(u32::MAX))
    }

    fn forget_connection(&mut self, key: KeyId) {
        self.connections.retain(|(held, _)| *held != key);
    }
}

/// A `Dba\Connection`'s payload, php's `dba_connection`: the link and
/// the key the connection is listed under; `None` once closed.
#[derive(Clone, Copy)]
pub(crate) struct Conn {
    link: LinkId,
    key: KeyId,
}

/// `final class Dba\Connection`, `@strict-properties`,
/// `@not-serializable`: one connection, 72 bytes as php's
/// `dba_connection` is.
///
/// ```php
/// try { new Dba\Connection; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $f = tempnam(sys_get_temp_dir(), 'dba');
/// $a = dba_open($f, 'n', 'flatfile');
/// try { clone $a; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($a); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $a->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump((int) $a === spl_object_id($a), $a == dba_open($f, 'r-', 'flatfile'));
/// try { echo $a; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// dba_close($a); unlink($f);
/// ```
/// ```expect
/// Cannot directly construct Dba\Connection, use dba_open() or dba_popen() instead
/// Trying to clone an uncloneable object of class Dba\Connection
/// Serialization of 'Dba\Connection' is not allowed
/// Cannot create dynamic property Dba\Connection::$x
/// bool(true)
/// bool(false)
/// Object of class Dba\Connection could not be converted to string
/// ```
#[php_class(
    namespace(Dba),
    final,
    not_serializable,
    size = 72, payload = Option<Conn>,
    handlers = CONNECTION,
    dynamic_properties = forbidden
)]
pub(crate) struct Connection;

#[php_class]
impl Connection {}

/// `dba_connection_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static CONNECTION: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn get_constructor(
        &self,
        req: &mut Request,
        _object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        Err(throw(
            req,
            "Error",
            "Cannot directly construct Dba\\Connection, use dba_open() or dba_popen() instead",
        ))
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let _ = (req, left, right);
        Ok(None)
    }

    /// `dba_connection_cast_object`: `(int)` is the object's id, as it
    /// was the resource's; any other cast is the standard one.
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Value>, Thrown> {
        let _ = req;
        Ok(match to {
            TypeTag::Int => Some(Value::int(i64::from(object.index()))),
            // `zend_std_cast_object_tostring`, for a class with no
            // `__toString`.
            TypeTag::Bool => Some(Value::bool(true)),
            _ => None,
        })
    }

    /// `dba_connection_free_obj`: the connection closed.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let held = object
            .native_mut::<Option<Conn>>(req)
            .and_then(Option::take);
        if let Some(conn) = held {
            close_connection(req, conn);
        }
    }
}

/// `dba_close_connection`: the connection unlisted and, for a link that
/// is not persistent, the link closed.
fn close_connection(req: &mut Request, conn: Conn) {
    let state = req.state::<State>();
    state.forget_connection(conn.key);
    if state.link(conn.link).is_some_and(|link| link.persistent) {
        return;
    }
    let link = state.slot(conn.link).and_then(Option::take);
    if let Some(link) = link {
        link.close(req);
    }
}
