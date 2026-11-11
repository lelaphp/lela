//! `pdo_dbh.c`: the `PDO` class, its connection `pdo_dbh_t` as the object's
//! native payload ([`Dbh`]), the handlers `pdo_dbh_init` installs, and
//! the methods, each checking the connection was constructed and clearing
//! the error state where php does, in php's order.

use std::ops::ControlFlow;

use php_ext::abi::{
    EngineState, GcTable, Instance, Lent, ObjectHandlers, OrFalse, Passed, Pool, Refusals, Static,
    Std, This, Variadic,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, autoload, known_method, lookup_class, new_object};
use php_ext::coerce::{c_str, value_name};
use php_ext::decl::Name;
use php_ext::raising::{
    Thrown, argument_error, deprecated, pending, throw_error, throw_exception_ex,
    throwing_warnings, type_error, value_error,
};
use php_ext::request::{OwnedStream as _, Request, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{
    ClassId, Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef,
};

use crate::connector::{
    BindType, Cause, Connection, Connector, Failure, OnError, Source, SqlState,
};
use crate::error::{self, On};
use crate::stmt::{DbLink, PDOStatement, Stmt};
use crate::throwables::Throwables;

/// `enum pdo_case_conversion`: what the column names are folded to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Case {
    #[default]
    Natural,
    Upper,
    Lower,
}

impl Case {
    fn code(self) -> i64 {
        match self {
            Case::Natural => 0,
            Case::Upper => 1,
            Case::Lower => 2,
        }
    }
}

/// `enum pdo_null_handling`, as `PDO::ATTR_ORACLE_NULLS` sets it: php stores
/// any number it is given and acts on two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Nulls {
    #[default]
    Natural,
    EmptyString,
    ToString,
    Other(u8),
}

impl Nulls {
    fn of(n: i64) -> Nulls {
        // php stores the attribute in a `uint8_t`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        match n as u8 {
            0 => Nulls::Natural,
            1 => Nulls::EmptyString,
            2 => Nulls::ToString,
            other => Nulls::Other(other),
        }
    }

    fn code(self) -> i64 {
        match self {
            Nulls::Natural => 0,
            Nulls::EmptyString => 1,
            Nulls::ToString => 2,
            Nulls::Other(n) => i64::from(n),
        }
    }
}

/// Where the connection's connection is: none before the factory ran, the
/// connector's while no call holds it, and lent while one does.
#[derive(Default)]
enum Conn {
    #[default]
    Closed,
    Open(Box<dyn Connection>),
    Lent,
}

/// `pdo_dbh_t`, the payload of a `PDO` and of every subclass.
// php's bit fields `stringify`, `in_txn`, `is_persistent` and
// `alloc_own_columns`, one each.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
pub(crate) struct DbhState {
    /// `dbh->driver`: set once the factory succeeded, which is what
    /// `PDO_CONSTRUCT_CHECK` asks.
    pub(crate) driver: Option<&'static dyn Connector>,
    conn: Conn,
    pub(crate) error_mode: OnError,
    /// `dbh->error_code`: `None` before anything cleared it, php's
    /// zeroed array, which `errorCode()` answers `null` for.
    pub(crate) error_code: Option<SqlState>,
    /// `dbh->desired_case`.
    pub(crate) case: Case,
    /// `dbh->oracle_nulls`.
    pub(crate) nulls: Nulls,
    /// `dbh->stringify`.
    pub(crate) stringify: bool,
    /// `dbh->default_fetch_type`, php's `uint16_t` as set, read as a
    /// fetch style where a fetch uses it.
    pub(crate) default_fetch: u16,
    /// `dbh->def_stmt_ce`, `None` for `PDOStatement`.
    stmt_class: Option<ClassId>,
    /// `dbh->def_stmt_ctor_args`, one owner.
    stmt_args: Option<Owner<PhpArray>>,
    /// `dbh->in_txn`.
    in_txn: bool,
    /// `dbh->is_persistent`: the connection is the engine's pool's, lent
    /// to the request ([`Persistent`]).
    persistent: bool,
    /// The place of the persistent `pdo_dbh_t` the request holds, which
    /// every `PDO` asking its key shares in place of its own, as
    /// `pdo_obj->inner = pdbh` shares it.
    shares: Option<usize>,
    /// Of a persistent `pdo_dbh_t` the request holds, the `PDO` objects
    /// sharing it: php's `refcount` less the one `EG(persistent_list)`
    /// keeps, which `pdo_dbh_free_storage` reads to roll back only as the
    /// last of them goes (GH-23962).
    holders: usize,
    /// `dbh->query_stmt`: the statement a failed `query()` left, whose
    /// error state `errorCode()` and `errorInfo()` read.
    query_stmt: Option<Owner<PhpObject>>,
    /// `dbh->alloc_own_columns`.
    pub(crate) describes_at_execute: bool,
    /// `dbh->data_source`, `username` and `password`, the copies the
    /// constructor `pestrdup`s, on the request's heap for a connection
    /// that is not persistent, given back in `dbh_free`.
    copies: Vec<Emalloc>,
}

impl DbhState {
    /// `PDO_FETCH_BOTH`, the connection's default before the constructor
    /// sets it.
    fn fresh() -> DbhState {
        DbhState {
            default_fetch: 4,
            ..DbhState::default()
        }
    }
}

/// The payload, `pdo_dbh_object_t`'s `inner`: the pointer to the
/// `pdo_dbh_t` php allocates beside the object (design §5.13).
pub(crate) type Dbh = Box<DbhState>;

/// The `pdo_dbh_t` an object reaches through its payload: its own, or
/// the persistent one the request holds where the payload
/// [`shares`](DbhState::shares) it.
pub(crate) fn state(req: &mut Request, object: PhpObject) -> Option<&mut DbhState> {
    match object.native::<Dbh>(req)?.shares {
        Some(at) => req.state::<Persistent>().lent.get_mut(at).map(|d| &mut **d),
        None => object.native_mut::<Dbh>(req).map(|d| &mut **d),
    }
}

/// The object's own `pdo_dbh_t` taken off its payload, which shares
/// nothing until [`share`] points it at the persistent one.
fn detach(req: &mut Request, object: PhpObject) -> Option<Dbh> {
    object.native_mut::<Dbh>(req).map(std::mem::take)
}

/// An object's payload switched over to the persistent `pdo_dbh_t` the
/// request holds at `at`, `pdo_obj->inner = pdbh`, which counts the
/// object among its holders, `pdbh->refcount++`.
fn share(req: &mut Request, object: PhpObject, at: usize) {
    if let Some(payload) = object.native_mut::<Dbh>(req) {
        **payload = DbhState {
            shares: Some(at),
            ..DbhState::default()
        };
    }
    if let Some(held) = req.state::<Persistent>().lent.get_mut(at) {
        held.holders += 1;
    }
}

/// `EG(persistent_list)` as far as PDO reaches it (design §4.10): the
/// engine's pool of persistent connections, keyed `PDO:DBH:DSN=…`.
#[derive(EngineState)]
pub(crate) struct Pooled {
    pub(crate) pool: Pool<Vec<u8>, Box<DbhState>>,
    /// The classes the natives raise, resolved at the same start.
    pub(crate) throwables: Throwables,
}

/// The idle persistent connections the pool keeps; php bounds pdo's by
/// nothing but the process, and this is the engine's own streams' bound.
pub(crate) const POOLED: usize = 1024;

/// The persistent connections the request took out of the pool or
/// opened, each held under its key until `post_deactivate` gives it back.
#[derive(Default)]
pub(crate) struct Persistent {
    lent: Lent<Vec<u8>, Box<DbhState>>,
}

/// What a persistent connection holds of the request given back, the
/// connector's `persistent_shutdown` and the statement a failed
/// `query()` left.
fn persistent_shutdown(req: &mut Request, dbh: &mut DbhState) {
    if let Some(stmt) = dbh.query_stmt.take() {
        stmt.give_up(req);
    }
    if let Some(args) = dbh.stmt_args.take() {
        args.give_up(req);
    }
    if let Conn::Open(conn) = &mut dbh.conn {
        conn.persistent_shutdown(req);
    }
}

/// The first step of php's `pdo_dbh_free_storage` (`pdo_dbh.c:1588-1591`):
/// a transaction the connection still has open rolled back, asked of the
/// connector's `in_transaction` where it answers and of `in_txn` where it
/// does not. True where the connector's `rollback` ran, failed or not,
/// for the caller to clear `in_txn`; a connector whose slot is php's null
/// one leaves it set, as php's `methods->rollback` test does.
#[must_use]
fn roll_back_open(req: &mut Request, conn: &mut dyn Connection, in_txn: bool) -> bool {
    if !conn.in_transaction(req).unwrap_or(in_txn) {
        return false;
    }
    !matches!(
        conn.rollback(req),
        Err(Failure {
            cause: Cause::Unsupported,
            ..
        })
    )
}

/// Every persistent connection the request holds given back to the
/// engine's pool, after the request's objects are freed, where php's
/// connection simply stays in `EG(persistent_list)`: each `PDO` sharing
/// it was freed in `shutdown_executor`, after every extension's
/// `RSHUTDOWN` (so a session save handler's `write` still had it), and
/// its `pdo_dbh_free_storage` rolled back the transaction it left open
/// and ran `persistent_shutdown` (`free_obj`), so nothing of the
/// request's is left on it. One past the pool's bound is closed with
/// this request, the connector's `closer` as `dbh_free` calls it
/// (ADR-0010's amendment of ADR-0005).
pub(crate) fn post_deactivate(req: &mut Request) {
    for (key, mut dbh) in req.state::<Persistent>().lent.drain() {
        if let Conn::Open(conn) = &mut dbh.conn {
            conn.give_back(req);
        }
        let past = match req.engine::<Pooled>() {
            Some(Pooled { pool, .. }) => pool.give_back(key, dbh),
            None => Some(dbh),
        };
        if let Some(mut dbh) = past {
            close_pooled(req, &mut dbh);
        }
    }
}

/// What php's `zend_shutdown` does to PDO's persistent connections when it
/// destroys `EG(persistent_list)` (`Z/Zend/zend.c:1166`): each the pool
/// keeps closed on the shutdown request through its connector's `closer`,
/// as `php_pdo_pdbh_dtor` reaches `dbh_free` (`pdo_dbh.c:1617`, `:1547`), so
/// `pdo_mysql` sends its `COM_QUIT` (#1961).
pub(crate) fn engine_end(req: &mut Request) {
    let idle = req
        .engine::<Pooled>()
        .map(|Pooled { pool, .. }| pool.drain())
        .unwrap_or_default();
    for (_, mut dbh) in idle {
        close_pooled(req, &mut dbh);
    }
}

/// `dbh_free`'s `closer` over a persistent connection the pool will not
/// keep, given back past its bound or drained at engine end.
fn close_pooled(req: &mut Request, dbh: &mut DbhState) {
    if let Conn::Open(conn) = &mut dbh.conn {
        conn.close(req);
    }
}

/// `pdo_dbh_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning and comparison.
const REFUSALS: Refusals = Refusals::NONE.cloning().comparison();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `pdo_dbh_new` past the allocation: the property table built at
    /// once, `zend_std_get_properties_ex(&dbh->std)`.
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        object.rebuild_properties(req)
    }

    /// `pdo_dbh_free_storage`: a transaction still open rolled back,
    /// `zend_object_std_dtor`, and then `dbh_free`: the failed statement
    /// given up, the connection closed and the constructor arguments
    /// released. Over a persistent connection the rollback waits for the
    /// last object sharing it (GH-23962):
    ///
    /// ```php
    /// $a = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);
    /// $b = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);
    /// $b->beginTransaction();
    /// unset($a);
    /// var_dump($b->inTransaction(), $b->commit());
    /// $b->beginTransaction();
    /// $b->exec('CREATE TABLE t (a)');
    /// unset($b);
    /// $c = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);
    /// var_dump($c->inTransaction(), $c->query("SELECT count(*) FROM sqlite_master WHERE name = 't'")->fetchColumn());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// int(0)
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if object
            .native::<Dbh>(req)
            .is_some_and(|d| d.shares.is_some())
        {
            // Over a persistent `pdo_dbh_t`: the transaction rolled back
            // where no other object shares the connection (php 8.5.12's
            // `refcount <= 2`, GH-23962), `persistent_shutdown`, and
            // `dbh_free`'s `--dbh->refcount`, which leaves the connection
            // held.
            let Some((tracked, last)) = dbh_ref(req, object, |d| (d.in_txn, d.holders <= 1)) else {
                return;
            };
            let rolled = with_connection(req, object, |req, conn| {
                let rolled = last && roll_back_open(req, conn, tracked);
                conn.persistent_shutdown(req);
                Ok(rolled)
            });
            let held = dbh_mut(req, object, |d| {
                if rolled.unwrap_or(false) {
                    d.in_txn = false;
                }
                // php leaves the objects sharing a dead connection on it
                // and counts a new one from the factory's object alone; the
                // engine replaces the dead one in place (`hold`), so those
                // objects share the new one uncounted, and what their frees
                // take off the count stops at zero.
                d.holders = d.holders.saturating_sub(1);
                d.query_stmt.take()
            });
            Std.free_obj(req, object);
            if let Some(stmt) = held.flatten() {
                stmt.give_up(req);
            }
            return;
        }
        let Some(dbh) = state(req, object) else {
            return;
        };
        let conn = std::mem::take(&mut dbh.conn);
        let copies = std::mem::take(&mut dbh.copies);
        let held = (dbh.query_stmt.take(), dbh.stmt_args.take());
        let tracked = dbh.in_txn;
        if let Conn::Open(mut conn) = conn {
            let _rolled_back = roll_back_open(req, &mut *conn, tracked);
            Std.free_obj(req, object);
            if let Some(stmt) = held.0 {
                stmt.give_up(req);
            }
            conn.close(req);
        } else {
            Std.free_obj(req, object);
            if let Some(stmt) = held.0 {
                stmt.give_up(req);
            }
        }
        if let Some(args) = held.1 {
            args.give_up(req);
        }
        for copy in copies {
            copy.free(req);
        }
    }

    /// `dbh_get_gc`: the constructor arguments and what the connector
    /// holds.
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(dbh) = state(req, object) {
            if let Some(args) = &dbh.stmt_args {
                visit(Value::array(**args));
            }
            if let Conn::Open(conn) = &dbh.conn {
                conn.gc(visit);
            }
        }
        GcTable::Properties
    }

    /// `dbh_method_get`: the class's own method, else one the connection's
    /// connector lays on `PDO`, `pdo_hash_methods` over
    /// `get_driver_methods`.
    fn get_method(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        if let Some(found) = Std.get_method(req, object, lowered)? {
            return Ok(Some(found));
        }
        let Some(driver) = state(req, object).and_then(|dbh| dbh.driver) else {
            return Ok(None);
        };
        if !driver.methods().contains(&lowered) {
            return Ok(None);
        }
        let pdo = lookup_class(req, b"PDO").ok().flatten();
        Ok(pdo.and_then(|pdo| req.symbols().trampoline(pdo, lowered)))
    }
}

// ---- the handle's state, lent and read

fn dbh_ref<R>(
    req: &mut Request,
    object: PhpObject,
    read: impl FnOnce(&DbhState) -> R,
) -> Option<R> {
    state(req, object).map(|d| read(d))
}

fn dbh_mut<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut DbhState) -> R,
) -> Option<R> {
    state(req, object).map(change)
}

/// `PDO_CONSTRUCT_CHECK`: php's `Error` naming the object's class where
/// the connection's factory never succeeded.
pub(super) fn constructed(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    if dbh_ref(req, object, |d| d.driver.is_some()).unwrap_or(false) {
        return Ok(());
    }
    let class = crate::class_name(req, object.class());
    let ce = Throwables::of(req)?;
    Err(throw_error(
        req,
        ce.error,
        &format!("{class} object is uninitialized"),
    ))
}

/// `PDO_DBH_CLEAR_ERR`: the state `00000`, and the statement a failed
/// `query()` left given up.
pub(super) fn clear_error(req: &mut Request, object: PhpObject) {
    let held = dbh_mut(req, object, |d| {
        d.error_code = Some(SqlState::NONE);
        d.query_stmt.take()
    })
    .flatten();
    if let Some(stmt) = held {
        stmt.give_up(req);
    }
}

/// The connection's connection lent to `f` for one call and put back.
pub(super) fn with_connection<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, &mut dyn Connection) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    constructed(req, object)?;
    let conn = dbh_mut(req, object, |d| std::mem::replace(&mut d.conn, Conn::Lent));
    let Some(Conn::Open(mut conn)) = conn else {
        if let Some(taken) = conn {
            dbh_mut(req, object, |d| d.conn = taken);
        }
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: a PDO handle's connection was asked for while a callback held it",
        ));
    };
    let driver = dbh_ref(req, object, |d| d.driver).flatten();
    let answered = crate::as_driver(req, driver, |req| f(req, &mut *conn));
    dbh_mut(req, object, |d| d.conn = Conn::Open(conn));
    answered
}

/// The connector of the connection `object`, none before it connected.
pub(super) fn driver_of(req: &mut Request, object: PhpObject) -> Option<&'static dyn Connector> {
    dbh_ref(req, object, |d| d.driver).flatten()
}

/// A connector call on the connection's connection whose failure PDO connections on
/// the connection, `PDO_HANDLE_DBH_ERR`: `Ok(None)` for a failure handled,
/// the connector's `Unsupported` handed back as `Err(None)` for the caller
/// to word.
fn on_connection<T>(
    req: &mut Request,
    object: PhpObject,
    call: impl FnOnce(&mut Request, &mut dyn Connection) -> Result<T, Failure>,
) -> Result<Result<Option<T>, ()>, Thrown> {
    let answered = with_connection(req, object, |req, conn| Ok(call(req, conn)))?;
    match answered {
        Ok(value) => Ok(Ok(Some(value))),
        Err(Failure {
            cause: Cause::Unsupported,
            thrown: None,
        }) => Ok(Err(())),
        Err(failure) => {
            error::settle(req, object, On::Dbh, failure)?;
            Ok(Ok(None))
        }
    }
}

/// `pdo_raise_impl_error(dbh, NULL, "IM001", supp)` and `false`.
fn unsupported(req: &mut Request, object: PhpObject, supp: &str) -> Result<(), Thrown> {
    error::raise_impl(req, object, On::Dbh, SqlState(*b"IM001"), Some(supp))
}

// ---- construction

/// What `__construct` and `connect` are given, in the stub's order.
#[derive(Clone, Copy)]
struct Given {
    dsn: PhpString,
    username: Option<PhpString>,
    password: Option<PhpString>,
    options: Option<PhpArray>,
}

/// `php_pdo_internal_construct_driver`: the constructor's body for
/// `current`, or `connect()`'s for a new object of the class it makes,
/// which comes back. The DSN and the credentials end at their first NUL,
/// as php holds each as a `char *` and hands the factory
/// `strlen(colon + 1)` bytes of the DSN, so a connector never reads past
/// it:
///
/// ```php
/// foreach (["mysql:host=-\0;dbname=test;host=127.0.0.1", "mysql:host=127.0.0.1;dbname=test\0;dbname=nope"] as $dsn) {
///     try { $p = new PDO($dsn, "root\0x", ''); echo $p->query('SELECT DATABASE()')->fetchColumn(), "\n"; }
///     catch (PDOException $e) { echo strstr($e->getMessage(), ' failed', true), "\n"; }
/// }
/// try { new PDO("nope\0:x"); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// SQLSTATE[HY000] [2002] php_network_getaddresses: getaddrinfo for -
/// test
/// PDO::__construct(): Argument #1 ($dsn) must be a valid data source name
/// ```
fn construct_driver(
    req: &mut Request,
    current: Option<PhpObject>,
    called: ClassId,
    given: Given,
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let source = c_str(given.dsn.as_bytes()).to_vec();
    let bytes = |s: PhpString| c_str(s.as_bytes()).to_vec();
    let (username, password) = (given.username.map(bytes), given.password.map(bytes));
    let options = given.options;
    let source = resolve_dsn(req, source)?;
    let colon = source.iter().position(|b| *b == b':').unwrap_or(0);
    let scheme = &source[..colon];
    let Some(driver) = crate::drivers(req).find(|d| d.name().as_bytes() == scheme) else {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.pdo_exception,
            "could not find driver",
        ));
    };
    let fresh = specific_object(req, driver, called, current.is_none())?;
    let object = match (&fresh, current) {
        (Some(made), _) => **made,
        (None, Some(current)) => current,
        (None, None) => {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: PDO::connect() made no object",
            ));
        }
    };
    let opened = open(
        req,
        object,
        driver,
        (&source, colon),
        username.as_deref(),
        password.as_deref(),
        options,
    );
    match opened {
        Ok(()) => Ok(fresh),
        Err(thrown) => {
            if let Some(fresh) = fresh {
                fresh.give_up(req);
            }
            Err(thrown)
        }
    }
}

/// The DSN as the constructor reads it: a name without a colon looked
/// up as `pdo.dsn.<name>`, and a `uri:` one read from the first line of
/// what the URI opens, each refused as php refuses it. A handler that
/// throws on the `uri:` deprecation ends the constructor there, before
/// anything is opened.
///
/// ```php
/// $dsn = tempnam(sys_get_temp_dir(), 'dsn');
/// $db = $dsn . '.db';
/// file_put_contents($dsn, "sqlite:$db");
/// set_error_handler(function ($no, $str) { throw new ErrorException($str); });
/// try { new PDO("uri:$dsn"); } catch (ErrorException $e) { echo $e->getMessage(), "\n"; }
/// var_dump(file_exists($db));
/// unlink($dsn);
/// ```
/// ```expect
/// Looking up the DSN from a URI is deprecated due to possible security concerns with DSNs coming from remote URIs
/// bool(false)
/// ```
fn resolve_dsn(req: &mut Request, mut source: Vec<u8>) -> Result<Vec<u8>, Thrown> {
    let ce = Throwables::of(req)?;
    if !source.contains(&b':') {
        let alias = [b"pdo.dsn.".as_slice(), &source].concat();
        let Some(found) = req.ini().get(&alias).map(<[u8]>::to_vec) else {
            return Err(argument_error(
                req,
                ce.pdo_exception,
                1,
                "must be a valid data source name",
            ));
        };
        if !found.contains(&b':') {
            let alias = String::from_utf8_lossy(&alias).into_owned();
            return Err(throw_exception_ex(
                req,
                ce.pdo_exception,
                &format!("invalid data source name (via INI: {alias})"),
            ));
        }
        source = found;
    }
    if let Some(uri) = source.strip_prefix(b"uri:").map(<[u8]>::to_vec) {
        deprecated(
            req,
            "Looking up the DSN from a URI is deprecated due to possible security concerns with DSNs coming from remote URIs",
        );
        pending(req)?;
        let Some(mut line) = dsn_from_uri(req, &uri)? else {
            return Err(argument_error(
                req,
                ce.pdo_exception,
                1,
                "must be a valid data source URI",
            ));
        };
        line.truncate(c_str(&line).len());
        if !line.contains(&b':') {
            return Err(argument_error(
                req,
                ce.pdo_exception,
                1,
                "must be a valid data source name (via URI)",
            ));
        }
        source = line;
    }
    Ok(source)
}

/// The payload laid down and the factory served, under
/// `zend_replace_error_handling(EH_THROW, pdo_exception_ce)`; for a
/// persistent connection, the one the request holds under its key or the
/// pool's, alive, in place of the factory (`pdo_dbh.c:413-457`).
fn open(
    req: &mut Request,
    object: PhpObject,
    driver: &'static dyn Connector,
    (source, colon): (&[u8], usize),
    username: Option<&[u8]>,
    password: Option<&[u8]>,
    options: Option<PhpArray>,
) -> Result<(), Thrown> {
    let dsn = &source[colon + 1..];
    let (persistent, named) = match options.and_then(|o| o.get(&Key::Int(PDO::ATTR_PERSISTENT))) {
        Some(v) => persistent_asked(req, &v.deref())?,
        None => (false, None),
    };
    let auto_commit = crate::connector::option_long(req, options, PDO::ATTR_AUTOCOMMIT, 1)?;
    let mode = crate::connector::option_long(req, options, PDO::ATTR_ERRMODE, 2)?;
    let _ = auto_commit;
    let error_mode = match mode {
        0 => OnError::Silent,
        1 => OnError::Warn,
        _ => OnError::Throw,
    };
    let ce = Throwables::of(req)?;
    let mut dead = None;
    let key = persistent.then(|| persistent_key(source, username, password, named.as_deref()));
    if let Some(key) = &key {
        // `efree(dbh)`: the object's own `pdo_dbh_t` gives way to the
        // persistent one, lent or made.
        object.free_beside(req);
        match lend(req, key) {
            Ok(at) => {
                share(req, object, at);
                dbh_mut(req, object, |d| d.error_mode = error_mode);
                return throwing_warnings(req, ce.pdo_exception, |req| {
                    apply_options(req, object, options)
                });
            }
            Err(at) => dead = at,
        }
    }
    // `pestrdup(colon + 1)`, and the credentials where given.
    let mut copies = Vec::new();
    if !persistent {
        for copied in [Some(dsn), username, password].into_iter().flatten() {
            copies.push(Emalloc::new(req, copied.len() + 1)?);
        }
    }
    dbh_mut(req, object, |d| {
        *d = DbhState {
            error_mode,
            persistent,
            copies,
            ..DbhState::fresh()
        };
    });
    throwing_warnings(req, ce.pdo_exception, |req| {
        let source = Source {
            dsn,
            username,
            password,
            options,
            persistent,
        };
        let conn = match crate::as_driver(req, Some(driver), |req| driver.open(req, &source)) {
            Ok(conn) => conn,
            Err(failure) => return Err(error::throw_connect(req, failure)),
        };
        let describes = conn.describes_at_execute();
        dbh_mut(req, object, |d| {
            d.conn = Conn::Open(conn);
            d.driver = Some(driver);
            d.describes_at_execute = describes;
        });
        if let Some(key) = key {
            hold(req, object, key, dead);
        }
        apply_options(req, object, options)
    })
}

/// `pdo_dbh_attribute_set` over each attribute the options array names.
fn apply_options(
    req: &mut Request,
    object: PhpObject,
    options: Option<PhpArray>,
) -> Result<(), Thrown> {
    if let Some(options) = options {
        // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(options), …)`
        // (`Z/ext/pdo/pdo_dbh.c:509`), no owner of the table taken: the
        // argument holds it while an attribute's conversion runs.
        walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
            let key = row.key().clone();
            let value = row.value().deref();
            if let Key::Int(attr) = key {
                attribute_set(req, object, attr, &value.deref(), 3)?;
            }
            Ok(ControlFlow::Continue(()))
        })?;
    }
    Ok(())
}

/// The persistent list's key, `PDO:DBH:DSN=<dsn>:<user>:<password>`, and
/// the attribute's string after one more colon where it is one, as `%s`
/// reads it, to its first NUL; the DSN and the credentials are cut there
/// already.
fn persistent_key(
    source: &[u8],
    username: Option<&[u8]>,
    password: Option<&[u8]>,
    named: Option<&[u8]>,
) -> Vec<u8> {
    let mut key = [b"PDO:DBH:DSN=".as_slice(), source, b":"].concat();
    key.extend(username.unwrap_or_default());
    key.push(b':');
    key.extend(password.unwrap_or_default());
    if let Some(named) = named {
        key.push(b':');
        key.extend(c_str(named));
    }
    key
}

/// The persistent connection under `key` lent to the request: the one it
/// holds, or the pool's, each where the connector's `check_liveness`
/// finds it alive. `Err` has the factory open one, in place of the dead
/// one the request held where it is `Some`, as `zend_list_close` drops it.
fn lend(req: &mut Request, key: &[u8]) -> Result<usize, Option<usize>> {
    let lent = &mut req.state::<Persistent>().lent;
    if let Some(at) = lent.find(&key.to_vec()) {
        let conn = lent.get_mut(at).map(|d| std::mem::take(&mut d.conn));
        let (alive, conn) = alive(req, conn.unwrap_or_default());
        if let Some(dbh) = req.state::<Persistent>().lent.get_mut(at) {
            dbh.conn = conn;
        }
        return if alive { Ok(at) } else { Err(Some(at)) };
    }
    let Some(mut dbh) = req
        .engine::<Pooled>()
        .and_then(|Pooled { pool, .. }| pool.take(&key.to_vec()))
    else {
        return Err(None);
    };
    let (alive, conn) = alive(req, std::mem::take(&mut dbh.conn));
    dbh.conn = conn;
    if !alive {
        // `zend_list_close` of the dead one: its destructor closes the
        // connection, its stream with it.
        if let Conn::Open(conn) = &mut dbh.conn {
            conn.close(req);
        }
        return Err(None);
    }
    Ok(req.state::<Persistent>().lent.hold(key.to_vec(), dbh))
}

/// `pdbh->methods->check_liveness`, php's null slot answering alive.
fn alive(req: &mut Request, conn: Conn) -> (bool, Conn) {
    match conn {
        Conn::Open(mut open) => (open.check_liveness(req), Conn::Open(open)),
        other => (false, other),
    }
}

/// `zend_register_persistent_resource`: the connection the factory just
/// opened for `object` held by the request under `key`, in place of the
/// dead one at `dead`, and the object switched over to it.
fn hold(req: &mut Request, object: PhpObject, key: Vec<u8>, dead: Option<usize>) {
    let Some(dbh) = detach(req, object) else {
        return;
    };
    let lent = &mut req.state::<Persistent>().lent;
    let at = match dead.and_then(|at| lent.get_mut(at).map(|held| (at, held))) {
        Some((at, held)) => {
            let mut old = std::mem::replace(held, dbh);
            persistent_shutdown(req, &mut old);
            // `zend_list_close` of the dead one: its destructor closes
            // the connection, its stream with it.
            if let Conn::Open(conn) = &mut old.conn {
                conn.close(req);
            }
            at
        }
        None => lent.hold(key, dbh),
    };
    share(req, object, at);
}

/// Whether `PDO::ATTR_PERSISTENT` asks for a persistent connection, and
/// the name it gives it: a non-numeric string is a name and asks,
/// anything else asks by its truth as `zval_get_long` reads it.
fn persistent_asked(req: &mut Request, value: &Value) -> Result<(bool, Option<Vec<u8>>), Thrown> {
    if let Some(bytes) = value.as_str_bytes()
        && !bytes.is_empty()
        && php_ext::coerce::numeric_string(bytes).is_none()
    {
        return Ok((true, Some(bytes.to_vec())));
    }
    Ok((php_ext::coerce::to_long(req, value)? != 0, None))
}

/// `dsn_from_uri`: the first line of what the URI opens, at most 511
/// bytes, as `php_stream_get_line` reads it into `alt_dsn`.
fn dsn_from_uri(req: &mut Request, uri: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(stream) = req.open_stream(uri, b"rb", false)? else {
        return Ok(None);
    };
    let line = stream.get_line(req, Some(512));
    stream.free(req);
    line
}

/// `create_driver_specific_pdo_object`: `None` where the current object
/// is the one to open, a new object of the class `connect()` makes
/// otherwise, and php's `PDOException` where the class called on cannot
/// open a connection of this connector.
fn specific_object(
    req: &mut Request,
    driver: &'static dyn Connector,
    called: ClassId,
    connecting: bool,
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let pdo = lookup_class(req, b"PDO")?;
    let is_pdo = Some(called) == pdo;
    let own = match driver.class() {
        Some(name) => lookup_class(req, name)?,
        None => None,
    };
    let classes: Vec<&'static [u8]> = crate::drivers(req)
        .filter_map(super::connector::Connector::class)
        .collect();
    let mut based_on_called = false;
    if !is_pdo {
        for name in classes {
            if crate::extends(req, called, name) {
                based_on_called = true;
                break;
            }
        }
    }
    let verb = if connecting { "connect" } else { "__construct" };
    let called_name = crate::class_name(req, called);
    let ce = Throwables::of(req)?;
    let wrong = |req: &mut Request, own: ClassId| {
        let own = crate::class_name(req, own);
        let name = driver.name();
        throw_exception_ex(
            req,
            ce.pdo_exception,
            &format!(
                "{called_name}::{verb}() cannot be used for connecting to the \"{name}\" driver, either call {own}::{verb}() or PDO::{verb}() instead"
            ),
        )
    };
    if based_on_called {
        let Some(own) = own else {
            return Err(throw_exception_ex(
                req,
                ce.pdo_exception,
                &format!(
                    "{called_name}::{verb}() cannot be used for connecting to an unknown driver, call PDO::{verb}() instead"
                ),
            ));
        };
        let own_name = req
            .symbols()
            .class_decl(own)
            .map(|d| d.name.0.to_vec())
            .unwrap_or_default();
        if !crate::extends(req, called, &own_name) {
            return Err(wrong(req, own));
        }
        return if connecting {
            new_object(req, called).map(Some)
        } else {
            Ok(None)
        };
    }
    if !is_pdo && !connecting {
        return Ok(None);
    }
    match own {
        Some(own) if !is_pdo => Err(wrong(req, own)),
        Some(own) if connecting => new_object(req, own).map(Some),
        _ if connecting => new_object(req, called).map(Some),
        _ => Ok(None),
    }
}

// ---- attributes

/// `pdo_dbh_attribute_set`: an attribute PDO connections itself, or the
/// connector's; `value_arg` is the argument a refusal names, `3` from the
/// constructor's options and `2` from `setAttribute()`.
fn attribute_set(
    req: &mut Request,
    object: PhpObject,
    attr: i64,
    value: &Value,
    value_arg: u32,
) -> Result<bool, Thrown> {
    let long = |req: &mut Request| crate::connector::long_attribute(req, value);
    match attr {
        PDO::ATTR_ERRMODE => {
            let mode = match long(req)? {
                0 => OnError::Silent,
                1 => OnError::Warn,
                2 => OnError::Throw,
                _ => {
                    let ce = Throwables::of(req)?;
                    return Err(argument_error(
                        req,
                        ce.value_error,
                        value_arg,
                        "Error mode must be one of the PDO::ERRMODE_* constants",
                    ));
                }
            };
            dbh_mut(req, object, |d| d.error_mode = mode);
            return Ok(true);
        }
        PDO::ATTR_CASE => {
            let case = match long(req)? {
                0 => Case::Natural,
                1 => Case::Upper,
                2 => Case::Lower,
                _ => {
                    let ce = Throwables::of(req)?;
                    return Err(argument_error(
                        req,
                        ce.value_error,
                        value_arg,
                        "Case folding mode must be one of the PDO::CASE_* constants",
                    ));
                }
            };
            dbh_mut(req, object, |d| d.case = case);
            return Ok(true);
        }
        PDO::ATTR_ORACLE_NULLS => {
            let nulls = Nulls::of(long(req)?);
            dbh_mut(req, object, |d| d.nulls = nulls);
            return Ok(true);
        }
        PDO::ATTR_DEFAULT_FETCH_MODE => {
            let mode = default_fetch_mode(req, value, value_arg)?;
            dbh_mut(req, object, |d| d.default_fetch = mode);
            return Ok(true);
        }
        PDO::ATTR_STRINGIFY_FETCHES => {
            let stringify = match value.as_ref() {
                ValueRef::Bool(b) => b,
                ValueRef::Int(n) => n != 0,
                _ => {
                    let given = value_name(req, value);
                    return Err(type_error(
                        req,
                        &[
                            b"Attribute value must be of type bool for selected attribute, "
                                .as_slice(),
                            given.as_slice(),
                            b" given".as_slice(),
                        ]
                        .concat(),
                    ));
                }
            };
            dbh_mut(req, object, |d| d.stringify = stringify);
            with_connection(req, object, |req, conn| {
                conn.set_attribute(req, attr, value).ok();
                Ok(())
            })
            .ok();
            return Ok(true);
        }
        PDO::ATTR_STATEMENT_CLASS => return set_statement_class(req, object, value, value_arg),
        _ => {}
    }
    clear_error(req, object);
    match on_connection(req, object, |req, conn| {
        conn.set_attribute(req, attr, value)
    })? {
        Ok(Some(())) => Ok(true),
        Ok(None) => Ok(false),
        Err(()) => {
            error::raise_impl(
                req,
                object,
                On::Dbh,
                SqlState(*b"IM001"),
                Some("driver does not support setting attributes"),
            )?;
            Ok(false)
        }
    }
}

/// `PDO::ATTR_DEFAULT_FETCH_MODE`'s value: an integer, or an array whose
/// first element is not `FETCH_INTO` or `FETCH_CLASS`, read as a
/// number, never `FETCH_DEFAULT`, and stored as php's `uint16_t`.
fn default_fetch_mode(req: &mut Request, value: &Value, value_arg: u32) -> Result<u16, Thrown> {
    let mode = if let Some(array) = value.as_array() {
        if let Some(first) = array.get(&Key::Int(0))
            && let Some(kind) = first.as_int()
            && (kind == 9 || kind == 8)
        {
            let ce = Throwables::of(req)?;
            return Err(argument_error(
                req,
                ce.value_error,
                value_arg,
                "PDO::FETCH_INTO and PDO::FETCH_CLASS cannot be set as the default fetch mode",
            ));
        }
        php_ext::coerce::to_long(req, value)?
    } else {
        crate::connector::long_attribute(req, value)?
    };
    if mode == 0 {
        let ce = Throwables::of(req)?;
        return Err(argument_error(
            req,
            ce.value_error,
            value_arg,
            "Fetch mode must be a bitmask of PDO::FETCH_* constants",
        ));
    }
    // php stores the default in a `uint16_t`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(mode as u16)
}

/// `PDO::ATTR_STATEMENT_CLASS`: `[class, constructor_args]`, the class a
/// statement is made as, which must extend `PDOStatement` and keep its
/// constructor from the public.
fn set_statement_class(
    req: &mut Request,
    object: PhpObject,
    value: &Value,
    value_arg: u32,
) -> Result<bool, Thrown> {
    if dbh_ref(req, object, |d| d.persistent).unwrap_or(false) {
        error::raise_impl(
            req,
            object,
            On::Dbh,
            SqlState::GENERAL,
            Some("PDO::ATTR_STATEMENT_CLASS cannot be used with persistent PDO instances"),
        )?;
        error::handle(req, object, On::Dbh, false)?;
        return Ok(false);
    }
    let (class, args) = statement_class(req, value, Some(value_arg))?;
    let args = args.map(|args| Owner::take(req, args));
    let old = match state(req, object) {
        Some(d) => {
            d.stmt_class = Some(class);
            std::mem::replace(&mut d.stmt_args, args)
        }
        None => args,
    };
    if let Some(old) = old {
        old.give_up(req);
    }
    Ok(true)
}

/// The class and the constructor arguments an `PDO::ATTR_STATEMENT_CLASS`
/// value names, held to php's refusals; `arg` is the argument
/// `setAttribute()` names, `None` for `prepare()`'s options, whose
/// refusals name none.
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy extends PDOStatement { protected function __construct() {} } } });
/// $db = new PDO('sqlite::memory:');
/// $db->setAttribute(PDO::ATTR_STATEMENT_CLASS, ['Lazy']);
/// echo get_class($db->query('SELECT 1')), "\n";
/// ```
/// ```expect
/// load Lazy
/// Lazy
/// ```
///
/// Constructor arguments that are not an array are named by their own
/// type, and a refused value leaves the class set before it:
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// try { $db->setAttribute(PDO::ATTR_STATEMENT_CLASS, ['PDOStatement', 'x']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $db->prepare('SELECT 1', [PDO::ATTR_STATEMENT_CLASS => ['PDOStatement', 1]]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($db->getAttribute(PDO::ATTR_STATEMENT_CLASS));
/// ```
/// ```expect
/// PDO::setAttribute(): Argument #2 ($value) PDO::ATTR_STATEMENT_CLASS constructor_args must be of type ?array, string given
/// PDO::ATTR_STATEMENT_CLASS constructor_args must be of type ?array, int given
/// array(1) {
///   [0]=>
///   string(12) "PDOStatement"
/// }
/// ```
pub(super) fn statement_class(
    req: &mut Request,
    value: &Value,
    arg: Option<u32>,
) -> Result<(ClassId, Option<PhpArray>), Thrown> {
    let ce = Throwables::of(req)?;
    let type_error = |req: &mut Request, tail: &[u8]| match arg {
        Some(at) => argument_error(req, ce.type_error, at, tail),
        None => type_error(req, tail),
    };
    let Some(array) = value.as_array() else {
        let given = value_name(req, value);
        return Err(type_error(
            req,
            &[
                b"PDO::ATTR_STATEMENT_CLASS value must be of type array, ".as_slice(),
                &given,
                b" given",
            ]
            .concat(),
        ));
    };
    let Some(first) = array.get(&Key::Int(0)) else {
        let tail = "PDO::ATTR_STATEMENT_CLASS value must be an array with the format array(classname, constructor_args)";
        return Err(match arg {
            Some(at) => argument_error(req, ce.value_error, at, tail),
            None => value_error(req, tail),
        });
    };
    let class = match first.deref().as_str_bytes().map(<[u8]>::to_vec) {
        Some(name) => autoload(req, &name)?,
        None => None,
    };
    let Some(class) = class else {
        return Err(type_error(
            req,
            b"PDO::ATTR_STATEMENT_CLASS class must be a valid class",
        ));
    };
    if !crate::extends(req, class, b"PDOStatement") {
        return Err(type_error(
            req,
            b"PDO::ATTR_STATEMENT_CLASS class must be derived from PDOStatement",
        ));
    }
    if public_constructor(req, class) {
        return Err(type_error(
            req,
            b"User-supplied statement class cannot have a public constructor",
        ));
    }
    let args = match array.get(&Key::Int(1)) {
        Some(args) => {
            if let Some(args) = args.deref().as_array() {
                Some(args)
            } else {
                // php 8.5.12 names the element, where 8.5.9 named the pair.
                let given = value_name(req, &args.deref());
                return Err(type_error(
                    req,
                    &[
                        b"PDO::ATTR_STATEMENT_CLASS constructor_args must be of type ?array, "
                            .as_slice(),
                        &given,
                        b" given",
                    ]
                    .concat(),
                ));
            }
        }
        None => None,
    };
    Ok((class, args))
}

/// Whether `class` has a constructor that is public.
fn public_constructor(req: &mut Request, class: ClassId) -> bool {
    constructor_visibility(req, class).is_some_and(|v| v == php_ext::decl::Visibility::Public)
}

/// The visibility of the constructor `class` declares or inherits.
fn constructor_visibility(req: &mut Request, class: ClassId) -> Option<php_ext::decl::Visibility> {
    let row = req.symbols().method_row(class, b"__construct")?;
    req.symbols()
        .row_method(row)
        .map(|method| method.visibility)
}

// ---- statements

/// `pdo_stmt_instantiate` and the fields `prepare()` and `query()` lay
/// down on the new statement: its query, the connection's default fetch
/// style and the connection itself, one owner.
fn instantiate(
    req: &mut Request,
    dbh: PhpObject,
    class: Option<ClassId>,
    with_args: bool,
    query: PhpString,
) -> Result<Owner<PhpObject>, Thrown> {
    let class = match class {
        Some(class) => class,
        None => match lookup_class(req, b"PDOStatement")? {
            Some(class) => class,
            None => {
                return Err(php_ext::raising::fatal(
                    req,
                    "InternalError: PDOStatement is not declared",
                ));
            }
        },
    };
    if with_args && constructor_visibility(req, class).is_none() {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "User-supplied statement does not accept constructor arguments",
        ));
    }
    let stmt = new_object(req, class)?;
    let fetch = dbh_ref(req, dbh, |d| d.default_fetch).unwrap_or(4);
    let held = Owner::take(req, dbh);
    let query = Owner::take(req, query);
    if let Some(s) = stmt.native_mut::<Stmt>(req) {
        s.query = Some(query);
        s.default_fetch = i64::from(fetch);
        s.dbh = DbLink::Owned(held);
    } else {
        held.give_up(req);
        query.give_up(req);
    }
    Ok(stmt)
}

/// `pdo_stmt_construct`: `queryString` written past the class's own
/// write handler, one more owner of the query's string, and the class's
/// constructor served with the arguments as `zend_call_known_function`
/// serves them.
fn construct_statement(
    req: &mut Request,
    stmt: PhpObject,
    query: PhpString,
    args: Option<PhpArray>,
) -> Result<(), Thrown> {
    let name = PhpString::new(req, b"queryString")?;
    let query = Owner::take(req, query);
    Std.write_property(req, stmt, name, query.into_value())?;
    if constructor_visibility(req, stmt.class()).is_none() {
        return Ok(());
    }
    let constructor = Name(b"__construct".as_slice().into());
    let Some(callable) = known_method(req, stmt.class(), &constructor, Some(stmt)) else {
        return Ok(());
    };
    // The statement's owner is the caller's, given up where this throws.
    crate::fetch::call_constructor(req, &callable, args)
}

// ---- transactions

/// `pdo_is_in_transaction`: the connector's answer, or PDO's own tracking.
fn in_txn(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let tracked = dbh_ref(req, object, |d| d.in_txn).unwrap_or(false);
    with_connection(req, object, |req, conn| {
        Ok(conn.in_transaction(req).unwrap_or(tracked))
    })
}

/// One of `begin`, `commit` and `rollback`: `true` with the tracking
/// set to `after`, `false` with the error handled.
fn transaction(
    req: &mut Request,
    object: PhpObject,
    after: bool,
    call: fn(&mut dyn Connection, &mut Request) -> Result<(), Failure>,
) -> Result<Result<bool, ()>, Thrown> {
    match on_connection(req, object, |req, conn| call(conn, req))? {
        Ok(Some(())) => {
            dbh_mut(req, object, |d| d.in_txn = after);
            Ok(Ok(true))
        }
        Ok(None) => Ok(Ok(false)),
        Err(()) => Ok(Err(())),
    }
}

fn end_transaction(
    req: &mut Request,
    this: PhpObject,
    call: fn(&mut dyn Connection, &mut Request) -> Result<(), Failure>,
) -> Result<bool, Thrown> {
    constructed(req, this)?;
    if !in_txn(req, this)? {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.pdo_exception,
            "There is no active transaction",
        ));
    }
    match transaction(req, this, false, call)? {
        Ok(done) => Ok(done),
        Err(()) => Ok(false),
    }
}

// ---- the error state

// ---- the class

/// `@not-serializable class PDO`, as `pdo_dbh.stub.php` declares it, over
/// the connection as its payload and `pdo_dbh_object_handlers`.
///
/// ```php
/// $m = new ReflectionMethod("PDO", "__construct");
/// foreach ($m->getParameters() as $p) echo $p, "\n";
/// echo (new ReflectionMethod("PDOStatement", "fetchObject"))->getParameters()[0], "\n";
/// var_dump((new ReflectionMethod("PDO", "exec"))->hasTentativeReturnType(), (new ReflectionClass("PDORow"))->isFinal());
/// ```
/// ```expect
/// Parameter #0 [ <required> string $dsn ]
/// Parameter #1 [ <optional> ?string $username = null ]
/// Parameter #2 [ <optional> ?string $password = null ]
/// Parameter #3 [ <optional> ?array $options = null ]
/// Parameter #0 [ <optional> ?string $class = "stdClass" ]
/// bool(true)
/// bool(true)
/// ```
#[php_class(not_serializable, size = 64, beside(152), payload = Dbh, handlers = HANDLERS)]
pub(crate) struct PDO;

/// The constants in the stub's order, each `@cvalue` written as its value,
/// and the methods as `pdo_dbh.stub.php` lists them.
#[php_class]
impl PDO {
    pub(crate) const PARAM_NULL: i64 = 0;
    pub(crate) const PARAM_BOOL: i64 = 5;
    pub(crate) const PARAM_INT: i64 = 1;
    pub(crate) const PARAM_STR: i64 = 2;
    pub(crate) const PARAM_LOB: i64 = 3;
    pub(crate) const PARAM_STMT: i64 = 4;
    pub(crate) const PARAM_INPUT_OUTPUT: i64 = 2_147_483_648;
    pub(crate) const PARAM_STR_NATL: i64 = 1_073_741_824;
    pub(crate) const PARAM_STR_CHAR: i64 = 536_870_912;
    pub(crate) const PARAM_EVT_ALLOC: i64 = 0;
    pub(crate) const PARAM_EVT_FREE: i64 = 1;
    pub(crate) const PARAM_EVT_EXEC_PRE: i64 = 2;
    pub(crate) const PARAM_EVT_EXEC_POST: i64 = 3;
    pub(crate) const PARAM_EVT_FETCH_PRE: i64 = 4;
    pub(crate) const PARAM_EVT_FETCH_POST: i64 = 5;
    pub(crate) const PARAM_EVT_NORMALIZE: i64 = 6;
    pub(crate) const FETCH_DEFAULT: i64 = 0;
    pub(crate) const FETCH_LAZY: i64 = 1;
    pub(crate) const FETCH_ASSOC: i64 = 2;
    pub(crate) const FETCH_NUM: i64 = 3;
    pub(crate) const FETCH_BOTH: i64 = 4;
    pub(crate) const FETCH_OBJ: i64 = 5;
    pub(crate) const FETCH_BOUND: i64 = 6;
    pub(crate) const FETCH_COLUMN: i64 = 7;
    pub(crate) const FETCH_CLASS: i64 = 8;
    pub(crate) const FETCH_INTO: i64 = 9;
    pub(crate) const FETCH_FUNC: i64 = 10;
    pub(crate) const FETCH_GROUP: i64 = 32;
    pub(crate) const FETCH_UNIQUE: i64 = 64;
    pub(crate) const FETCH_KEY_PAIR: i64 = 12;
    pub(crate) const FETCH_CLASSTYPE: i64 = 128;
    pub(crate) const FETCH_SERIALIZE: i64 = 512;
    pub(crate) const FETCH_PROPS_LATE: i64 = 256;
    pub(crate) const FETCH_NAMED: i64 = 11;
    pub(crate) const ATTR_AUTOCOMMIT: i64 = 0;
    pub(crate) const ATTR_PREFETCH: i64 = 1;
    pub(crate) const ATTR_TIMEOUT: i64 = 2;
    pub(crate) const ATTR_ERRMODE: i64 = 3;
    pub(crate) const ATTR_SERVER_VERSION: i64 = 4;
    pub(crate) const ATTR_CLIENT_VERSION: i64 = 5;
    pub(crate) const ATTR_SERVER_INFO: i64 = 6;
    pub(crate) const ATTR_CONNECTION_STATUS: i64 = 7;
    pub(crate) const ATTR_CASE: i64 = 8;
    pub(crate) const ATTR_CURSOR_NAME: i64 = 9;
    pub(crate) const ATTR_CURSOR: i64 = 10;
    pub(crate) const ATTR_ORACLE_NULLS: i64 = 11;
    pub(crate) const ATTR_PERSISTENT: i64 = 12;
    pub(crate) const ATTR_STATEMENT_CLASS: i64 = 13;
    pub(crate) const ATTR_FETCH_TABLE_NAMES: i64 = 14;
    pub(crate) const ATTR_FETCH_CATALOG_NAMES: i64 = 15;
    pub(crate) const ATTR_DRIVER_NAME: i64 = 16;
    pub(crate) const ATTR_STRINGIFY_FETCHES: i64 = 17;
    pub(crate) const ATTR_MAX_COLUMN_LEN: i64 = 18;
    pub(crate) const ATTR_EMULATE_PREPARES: i64 = 20;
    pub(crate) const ATTR_DEFAULT_FETCH_MODE: i64 = 19;
    pub(crate) const ATTR_DEFAULT_STR_PARAM: i64 = 21;
    pub(crate) const ERRMODE_SILENT: i64 = 0;
    pub(crate) const ERRMODE_WARNING: i64 = 1;
    pub(crate) const ERRMODE_EXCEPTION: i64 = 2;
    pub(crate) const CASE_NATURAL: i64 = 0;
    pub(crate) const CASE_LOWER: i64 = 2;
    pub(crate) const CASE_UPPER: i64 = 1;
    pub(crate) const NULL_NATURAL: i64 = 0;
    pub(crate) const NULL_EMPTY_STRING: i64 = 1;
    pub(crate) const NULL_TO_STRING: i64 = 2;
    pub(crate) const ERR_NONE: &str = "00000";
    pub(crate) const FETCH_ORI_NEXT: i64 = 0;
    pub(crate) const FETCH_ORI_PRIOR: i64 = 1;
    pub(crate) const FETCH_ORI_FIRST: i64 = 2;
    pub(crate) const FETCH_ORI_LAST: i64 = 3;
    pub(crate) const FETCH_ORI_ABS: i64 = 4;
    pub(crate) const FETCH_ORI_REL: i64 = 5;
    pub(crate) const CURSOR_FWDONLY: i64 = 0;
    pub(crate) const CURSOR_SCROLL: i64 = 1;

    /// `PDO::__construct(string $dsn, ?string $username = null, ?string
    /// $password = null, ?array $options = null)`: the connector the DSN names,
    /// its factory served the rest of the DSN, and each option set as
    /// `setAttribute()` would, a warning meanwhile thrown as a
    /// `PDOException`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_WARNING, PDO::ATTR_CASE => PDO::CASE_UPPER]);
    /// var_dump($db->getAttribute(PDO::ATTR_ERRMODE), $db->getAttribute(PDO::ATTR_CASE), $db->getAttribute(PDO::ATTR_DRIVER_NAME));
    /// foreach (['nope', 'nosuch:x', 'sqlite:/no/such/dir/x.db'] as $dsn) {
    ///     try { new PDO($dsn); } catch (PDOException $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// }
    /// class MyPDO extends PDO { function __construct() {} }
    /// try { (new MyPDO)->exec('x'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// int(1)
    /// string(6) "sqlite"
    /// PDOException: PDO::__construct(): Argument #1 ($dsn) must be a valid data source name
    /// PDOException: could not find driver
    /// PDOException: SQLSTATE[HY000] [14] unable to open database file
    /// MyPDO object is uninitialized
    /// ```
    ///
    /// The DSN's and the credentials' copies, `pestrdup`, and what the
    /// connector's factory allocates, `pdo_sqlite_db_handle` here, are
    /// the request's, and go back with the object:
    ///
    /// ```php
    /// $w = new PDO('sqlite::memory:'); unset($w);
    /// $m = memory_get_usage(); $p = new PDO('sqlite::memory:'); $n = memory_get_usage(); unset($p);
    /// $o = memory_get_usage(); $q = new PDO('sqlite::memory:', 'user', 'secret'); $r = memory_get_usage();
    /// var_dump($n - $m, memory_get_usage() - $n - ($r - $o), $r - $o);
    /// ```
    /// ```expect
    /// int(376)
    /// int(-376)
    /// int(408)
    /// ```
    ///
    /// Two persistent handles with one key share one connection, and a
    /// `PDO` freed leaves it held for the next to ask the key:
    ///
    /// ```php
    /// $a = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);
    /// $a->exec('CREATE TABLE t (a)'); $a->exec('INSERT INTO t VALUES (1)');
    /// $b = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);
    /// var_dump($b->query('SELECT COUNT(*) FROM t')->fetchColumn(), $b->getAttribute(PDO::ATTR_PERSISTENT));
    /// $a->setAttribute(PDO::ATTR_CASE, PDO::CASE_UPPER); unset($a);
    /// var_dump($b->getAttribute(PDO::ATTR_CASE) === PDO::CASE_UPPER);
    /// $c = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => 'other']);
    /// try { $c->query('SELECT COUNT(*) FROM t'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// unset($b);
    /// $d = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => 1]);
    /// var_dump($d->query('SELECT COUNT(*) FROM t')->fetchColumn());
    /// ```
    /// ```expect
    /// int(1)
    /// bool(true)
    /// bool(true)
    /// SQLSTATE[HY000]: General error: 1 no such table: t
    /// int(1)
    /// ```
    ///
    /// A persistent `PDO` costs the request its object alone, the
    /// `pdo_dbh_t` allocated beside it freed as it switches over:
    ///
    /// ```php
    /// $w = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]);
    /// $m = memory_get_usage(); $p = new PDO('sqlite::memory:', null, null, [PDO::ATTR_PERSISTENT => true]); $n = memory_get_usage(); unset($p);
    /// var_dump($n - $m, memory_get_usage() - $n);
    /// ```
    /// ```expect
    /// int(104)
    /// int(-104)
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        dsn: PhpString,
        #[php(default = null)] username: Option<PhpString>,
        #[php(sensitive, default = null)] password: Option<PhpString>,
        #[php(default = null)] options: Option<PhpArray>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let given = Given {
            dsn,
            username,
            password,
            options,
        };
        if let Some(made) = construct_driver(req, Some(this), this.class(), given)? {
            made.give_up(req);
        }
        Ok(())
    }

    /// `PDO::connect(...): static`: the connection made as the connector's own
    /// class where it registered one, `Pdo\Sqlite` for `sqlite:`, and the
    /// class it was called on otherwise.
    ///
    /// ```php
    /// $db = PDO::connect('sqlite::memory:');
    /// var_dump(get_class($db), $db->query('SELECT 1')->fetchColumn());
    /// class Mine extends PDO {}
    /// try { Mine::connect('sqlite::memory:'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(10) "Pdo\Sqlite"
    /// int(1)
    /// Mine::connect() cannot be used for connecting to the "sqlite" driver, either call Pdo\Sqlite::connect() or PDO::connect() instead
    /// ```
    fn connect(
        req: &mut Request,
        dsn: PhpString,
        #[php(default = null)] username: Option<PhpString>,
        #[php(sensitive, default = null)] password: Option<PhpString>,
        #[php(default = null)] options: Option<PhpArray>,
    ) -> Result<Owner<Static>, Thrown> {
        let Some(called) = req.frames().called_scope() else {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: PDO::connect() served without a called scope",
            ));
        };
        let given = Given {
            dsn,
            username,
            password,
            options,
        };
        match construct_driver(req, None, called, given)? {
            Some(object) => Ok(object.into_static()),
            None => Err(php_ext::raising::fatal(
                req,
                "InternalError: PDO::connect() made no object",
            )),
        }
    }

    /// `PDO::beginTransaction(): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->beginTransaction(), $db->inTransaction());
    /// try { $db->beginTransaction(); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// There is already an active transaction
    /// ```
    #[php(tentative)]
    fn beginTransaction(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        constructed(req, this)?;
        let ce = Throwables::of(req)?;
        if in_txn(req, this)? {
            return Err(throw_exception_ex(
                req,
                ce.pdo_exception,
                "There is already an active transaction",
            ));
        }
        match transaction(req, this, true, super::connector::Connection::begin)? {
            Ok(done) => Ok(done),
            Err(()) => Err(throw_exception_ex(
                req,
                ce.pdo_exception,
                "This driver doesn't support transactions",
            )),
        }
    }

    /// `PDO::commit(): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->exec('CREATE TABLE t (a)');
    /// $db->beginTransaction(); $db->exec('INSERT INTO t VALUES (1)');
    /// var_dump($db->commit(), $db->query('SELECT COUNT(*) FROM t')->fetchColumn());
    /// try { $db->commit(); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// int(1)
    /// There is no active transaction
    /// ```
    #[php(tentative)]
    fn commit(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        end_transaction(req, this.object(), super::connector::Connection::commit)
    }

    /// `PDO::errorCode(): ?string`: the failed `query()`'s state, else the
    /// connection's, `null` before anything set one.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);
    /// var_dump($db->errorCode());
    /// $db->exec('SELECT 1');
    /// var_dump($db->errorCode());
    /// $db->exec('NOPE');
    /// var_dump($db->errorCode());
    /// ```
    /// ```expect
    /// NULL
    /// string(5) "00000"
    /// string(5) "HY000"
    /// ```
    #[php(tentative)]
    fn errorCode(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        let this = this.object();
        constructed(req, this)?;
        let (held, code) = dbh_ref(req, this, |d| {
            (d.query_stmt.as_deref().copied(), d.error_code)
        })
        .unwrap_or((None, None));
        if let Some(stmt) = held {
            let state = stmt.native::<Stmt>(req).and_then(|s| s.error_code);
            return PhpString::new(req, state.as_ref().map_or("", SqlState::as_str).as_bytes())
                .map(Some);
        }
        match code {
            Some(state) => PhpString::new(req, state.as_str().as_bytes()).map(Some),
            None => Ok(None),
        }
    }

    /// `PDO::errorInfo(): array`: the state, and where it is an error what
    /// the connector recorded, filled to three with `null`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);
    /// var_dump($db->errorInfo());
    /// $db->exec('NOPE');
    /// var_dump($db->errorInfo());
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   string(0) ""
    ///   [1]=>
    ///   NULL
    ///   [2]=>
    ///   NULL
    /// }
    /// array(3) {
    ///   [0]=>
    ///   string(5) "HY000"
    ///   [1]=>
    ///   int(1)
    ///   [2]=>
    ///   string(25) "near "NOPE": syntax error"
    /// }
    /// ```
    #[php(tentative)]
    fn errorInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        constructed(req, this)?;
        let (held, code) = dbh_ref(req, this, |d| {
            (d.query_stmt.as_deref().copied(), d.error_code)
        })
        .unwrap_or((None, None));
        let state = match held {
            Some(stmt) => stmt.native::<Stmt>(req).and_then(|s| s.error_code),
            None => code,
        };
        let mut out = PhpArray::new(req)?;
        let made = crate::string(req, state.as_ref().map_or("", SqlState::as_str).as_bytes())?;
        out.append(req, made)?;
        if state != Some(SqlState::NONE) {
            let on = held.map_or(On::Dbh, On::Stmt);
            error::fetch_error(req, this, on, &mut out)?;
        }
        while out.len() < 3 {
            out.append(req, Owner::null())?;
        }
        Ok(out)
    }

    /// `PDO::exec(string $statement): int|false`: the rows it changed.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->exec('CREATE TABLE t (a)'), $db->exec('INSERT INTO t VALUES (1), (2)'), $db->exec('UPDATE t SET a = 3'));
    /// try { $db->exec(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { $db->exec('NOPE'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// $db->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_WARNING);
    /// var_dump($db->exec('NOPE'));
    /// ```
    /// ```expect
    /// int(0)
    /// int(2)
    /// int(2)
    /// PDO::exec(): Argument #1 ($statement) must not be empty
    /// SQLSTATE[HY000]: General error: 1 near "NOPE": syntax error
    ///
    /// Warning: PDO::exec(): SQLSTATE[HY000]: General error: 1 near "NOPE": syntax error in %s on line 7
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn exec(
        req: &mut Request,
        this: This<Self>,
        statement: PhpString,
    ) -> Result<OrFalse<i64>, Thrown> {
        let this = this.object();
        let sql = statement.as_bytes().to_vec();
        if sql.is_empty() {
            let ce = Throwables::of(req)?;
            return Err(argument_error(req, ce.value_error, 1, "must not be empty"));
        }
        clear_error(req, this);
        constructed(req, this)?;
        match on_connection(req, this, |req, conn| conn.exec(req, &sql))? {
            Ok(Some(rows)) => Ok(OrFalse::Value(rows)),
            Ok(None) | Err(()) => Ok(OrFalse::False),
        }
    }

    /// `PDO::getAttribute(int $attribute): mixed`: PDO's own attributes, then
    /// the connector's.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->getAttribute(PDO::ATTR_PERSISTENT), $db->getAttribute(PDO::ATTR_DEFAULT_FETCH_MODE), $db->getAttribute(PDO::ATTR_STRINGIFY_FETCHES), $db->getAttribute(PDO::ATTR_STATEMENT_CLASS));
    /// var_dump(is_string($db->getAttribute(PDO::ATTR_SERVER_VERSION)));
    /// try { $db->getAttribute(PDO::ATTR_PREFETCH); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(false)
    /// int(4)
    /// bool(false)
    /// array(1) {
    ///   [0]=>
    ///   string(12) "PDOStatement"
    /// }
    /// bool(true)
    /// SQLSTATE[IM001]: Driver does not support this function: driver does not support that attribute
    /// ```
    #[php(tentative)]
    fn getAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
    ) -> Result<Owner<Value>, Thrown> {
        let this = this.object();
        let attr = attribute;
        clear_error(req, this);
        constructed(req, this)?;
        let Some(state) = dbh_ref(req, this, |d| {
            (
                d.persistent,
                d.case,
                d.nulls,
                d.error_mode,
                d.driver.map(Connector::name),
                d.stmt_class,
                d.stmt_args.as_deref().copied(),
                d.default_fetch,
                d.stringify,
            )
        }) else {
            return Ok(Owner::bool(false));
        };
        let (persistent, case, nulls, mode, name, class, args, fetch, stringify) = state;
        match attr {
            PDO::ATTR_PERSISTENT => return Ok(Owner::bool(persistent)),
            PDO::ATTR_CASE => return Ok(Owner::int(case.code())),
            PDO::ATTR_ORACLE_NULLS => return Ok(Owner::int(nulls.code())),
            PDO::ATTR_ERRMODE => {
                let code = match mode {
                    OnError::Silent => 0,
                    OnError::Warn => 1,
                    OnError::Throw => 2,
                };
                return Ok(Owner::int(code));
            }
            PDO::ATTR_DRIVER_NAME => {
                return crate::string(req, name.unwrap_or("").as_bytes());
            }
            PDO::ATTR_STATEMENT_CLASS => {
                let mut out = PhpArray::new(req)?;
                let class = class.map_or_else(
                    || String::from("PDOStatement"),
                    |c| crate::class_name(req, c),
                );
                let text = crate::string(req, class.as_bytes())?;
                out.append(req, text)?;
                if let Some(args) = args {
                    out.append(req, Owner::take(req, Value::array(args)))?;
                }
                return Ok(out.into_value());
            }
            PDO::ATTR_DEFAULT_FETCH_MODE => {
                return Ok(Owner::int(i64::from(fetch)));
            }
            PDO::ATTR_STRINGIFY_FETCHES => return Ok(Owner::bool(stringify)),
            _ => {}
        }
        match on_connection(req, this, |req, conn| conn.attribute(req, attr))? {
            Ok(Some(Some(value))) => Ok(value),
            Ok(Some(None)) => unsupported(req, this, "driver does not support that attribute")
                .map(|()| Owner::bool(false)),
            Ok(None) => Ok(Owner::bool(false)),
            Err(()) => unsupported(req, this, "driver does not support getting attributes")
                .map(|()| Owner::bool(false)),
        }
    }

    /// `PDO::getAvailableDrivers(): array`.
    ///
    /// ```php
    /// var_dump(in_array('sqlite', PDO::getAvailableDrivers(), true));
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn getAvailableDrivers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        crate::driver_names(req)
    }

    /// `PDO::inTransaction(): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->inTransaction());
    /// $db->exec('BEGIN');
    /// var_dump($db->inTransaction());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn inTransaction(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        constructed(req, this)?;
        in_txn(req, this)
    }

    /// `PDO::lastInsertId(?string $name = null): string|false`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->exec('CREATE TABLE t (id INTEGER PRIMARY KEY, a)');
    /// $db->exec("INSERT INTO t (a) VALUES ('x'), ('y')");
    /// var_dump($db->lastInsertId(), $db->lastInsertId('t'));
    /// ```
    /// ```expect
    /// string(1) "2"
    /// string(1) "2"
    /// ```
    #[php(tentative)]
    fn lastInsertId(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] name: Option<PhpString>,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let this = this.object();
        let name = name.map(|name| name.as_bytes().to_vec());
        constructed(req, this)?;
        clear_error(req, this);
        match on_connection(req, this, |req, conn| {
            conn.last_insert_id(req, name.as_deref())
        })? {
            Ok(Some(id)) => PhpString::new(req, &id).map(OrFalse::Value),
            Ok(None) => Ok(OrFalse::False),
            Err(()) => unsupported(req, this, "driver does not support lastInsertId()")
                .map(|()| OrFalse::False),
        }
    }

    /// `PDO::prepare(string $query, array $options = []): PDOStatement|false`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT ? + ?');
    /// $s->execute([1, 2]);
    /// var_dump($s->queryString, $s->fetchColumn());
    /// class St extends PDOStatement { protected function __construct(public $tag) {} }
    /// $s = $db->prepare('SELECT 1', [PDO::ATTR_STATEMENT_CLASS => ['St', ['x']]]);
    /// var_dump(get_class($s), $s->tag);
    /// try { $db->prepare('SELEKT'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// try { $db->prepare('SELECT 1', [PDO::ATTR_STATEMENT_CLASS => 'St']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(12) "SELECT ? + ?"
    /// int(3)
    /// string(2) "St"
    /// string(1) "x"
    /// SQLSTATE[HY000]: General error: 1 near "SELEKT": syntax error
    /// PDO::ATTR_STATEMENT_CLASS value must be of type array, string given
    /// ```
    #[php(tentative)]
    fn prepare(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
        #[php(default = [])] options: Passed<PhpArray>,
    ) -> Result<OrFalse<Owner<Instance<PDOStatement>>>, Thrown> {
        let this = this.object();
        let sql = query.as_bytes().to_vec();
        let options = match options {
            Passed::Given(options) => Some(options),
            Passed::Omitted => None,
        };
        constructed(req, this)?;
        if sql.is_empty() {
            let ce = Throwables::of(req)?;
            return Err(argument_error(req, ce.value_error, 1, "must not be empty"));
        }
        clear_error(req, this);
        let (class, args) = match options.and_then(|o| o.get(&Key::Int(PDO::ATTR_STATEMENT_CLASS)))
        {
            Some(value) => {
                let (class, args) = statement_class(req, &value, None)?;
                (Some(class), args)
            }
            None => dbh_ref(req, this, |d| {
                (d.stmt_class, d.stmt_args.as_deref().copied())
            })
            .unwrap_or((None, None)),
        };
        let owner = instantiate(req, this, class, args.is_some(), query)?;
        let stmt = *owner;
        let prepared = with_connection(req, this, |req, conn| Ok(conn.prepare(req, &sql, options)));
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(thrown) => {
                owner.give_up(req);
                return Err(thrown);
            }
        };
        match prepared {
            Ok(prepared) => {
                crate::stmt::attach(req, stmt, prepared);
                if let Err(thrown) = construct_statement(req, stmt, query, args) {
                    owner.give_up(req);
                    return Err(thrown);
                }
                Ok(OrFalse::Value(owner.into_instance()))
            }
            Err(failure) => {
                owner.give_up(req);
                error::settle(req, this, On::Dbh, failure)?;
                Ok(OrFalse::False)
            }
        }
    }

    /// `PDO::query(string $query, ?int $fetchMode = null, mixed ...$fetchModeArgs): PDOStatement|false`:
    /// prepared and executed at once, the fetch style set where one is given.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// foreach ($db->query('SELECT 1 AS a UNION SELECT 2', PDO::FETCH_ASSOC) as $row) var_dump($row);
    /// var_dump($db->query('SELECT 5', PDO::FETCH_COLUMN, 0)->fetch());
    /// $db->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_SILENT);
    /// var_dump($db->query('SELECT * FROM nope'), $db->errorCode(), $db->errorInfo()[2]);
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// array(1) {
    ///   ["a"]=>
    ///   int(2)
    /// }
    /// int(5)
    /// bool(false)
    /// string(5) "HY000"
    /// string(19) "no such table: nope"
    /// ```
    #[php(tentative)]
    fn query(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
        #[php(default = null)] fetchMode: Option<i64>,
        fetchModeArgs: Variadic<Value>,
    ) -> Result<OrFalse<Owner<Instance<PDOStatement>>>, Thrown> {
        let this = this.object();
        let sql = query.as_bytes().to_vec();
        let mode = fetchMode;
        let extra: Vec<Value> = fetchModeArgs.map(|value| value.deref()).collect();
        constructed(req, this)?;
        if sql.is_empty() {
            let ce = Throwables::of(req)?;
            return Err(argument_error(req, ce.value_error, 1, "must not be empty"));
        }
        clear_error(req, this);
        let (class, args) = dbh_ref(req, this, |d| {
            (d.stmt_class, d.stmt_args.as_deref().copied())
        })
        .unwrap_or((None, None));
        let owner = instantiate(req, this, class, args.is_some(), query)?;
        let stmt = *owner;
        crate::stmt::set_active(req, stmt, None);
        let prepared = with_connection(req, this, |req, conn| Ok(conn.prepare(req, &sql, None)));
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(failure)) => {
                owner.give_up(req);
                error::settle(req, this, On::Dbh, failure)?;
                return Ok(OrFalse::False);
            }
            Err(thrown) => {
                owner.give_up(req);
                return Err(thrown);
            }
        };
        crate::stmt::attach(req, stmt, prepared);
        crate::stmt::clear_error(req, stmt);
        let set = match mode {
            Some(mode) => crate::fetch::setup_mode(req, stmt, mode, 2, &extra),
            None => Ok(()),
        };
        let ran = match set {
            Ok(()) => {
                crate::stmt::clear_error(req, stmt);
                crate::stmt::run(req, stmt)
            }
            Err(thrown) => Err(Some(thrown)),
        };
        match ran {
            Ok(()) => {
                if let Err(thrown) = construct_statement(req, stmt, query, args) {
                    owner.give_up(req);
                    return Err(thrown);
                }
                Ok(OrFalse::Value(owner.into_instance()))
            }
            Err(thrown) => {
                let link = stmt
                    .native_mut::<Stmt>(req)
                    .map(|s| std::mem::replace(&mut s.dbh, DbLink::Held(this)));
                if let Some(link) = link {
                    link.release(req);
                }
                let old = dbh_mut(req, this, |d| d.query_stmt.replace(owner)).flatten();
                if let Some(old) = old {
                    old.give_up(req);
                }
                if let Some(thrown) = thrown {
                    return Err(thrown);
                }
                crate::stmt::handle_error(req, stmt, false)?;
                Ok(OrFalse::False)
            }
        }
    }

    /// `PDO::quote(string $string, int $type = PDO::PARAM_STR): string|false`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->quote("it's"), $db->quote('1', PDO::PARAM_INT));
    /// try { $db->quote("a\0b"); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(7) "'it''s'"
    /// string(3) "'1'"
    /// SQLite PDO::quote does not support null bytes
    /// ```
    #[php(tentative)]
    fn quote(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        #[php(default = PDO::PARAM_STR)] r#type: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        let this = this.object();
        let text = string.as_bytes().to_vec();
        let ty = BindType::of(r#type);
        constructed(req, this)?;
        clear_error(req, this);
        let mode = dbh_ref(req, this, |d| d.error_mode).unwrap_or_default();
        match on_connection(req, this, |req, conn| conn.quote(req, &text, ty, mode))? {
            Ok(Some(quoted)) => Ok(OrFalse::Value(quoted)),
            Ok(None) => Ok(OrFalse::False),
            Err(()) => {
                unsupported(req, this, "driver does not support quoting").map(|()| OrFalse::False)
            }
        }
    }

    /// `PDO::rollBack(): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->exec('CREATE TABLE t (a)');
    /// $db->beginTransaction(); $db->exec('INSERT INTO t VALUES (1)');
    /// var_dump($db->rollBack(), $db->inTransaction(), $db->query('SELECT COUNT(*) FROM t')->fetchColumn());
    /// try { $db->rollBack(); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// int(0)
    /// There is no active transaction
    /// ```
    #[php(tentative)]
    fn rollBack(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        end_transaction(req, this.object(), super::connector::Connection::rollback)
    }

    /// `PDO::setAttribute(int $attribute, mixed $value): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_SILENT), $db->setAttribute(PDO::ATTR_ORACLE_NULLS, 7), $db->getAttribute(PDO::ATTR_ORACLE_NULLS));
    /// try { $db->setAttribute(PDO::ATTR_ERRMODE, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { $db->setAttribute(PDO::ATTR_CASE, 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $db->setAttribute(PDO::ATTR_STATEMENT_CLASS, ['stdClass']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($db->setAttribute(12345, 1));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// int(7)
    /// PDO::setAttribute(): Argument #2 ($value) Error mode must be one of the PDO::ERRMODE_* constants
    /// Attribute value must be of type int for selected attribute, string given
    /// PDO::setAttribute(): Argument #2 ($value) PDO::ATTR_STATEMENT_CLASS class must be derived from PDOStatement
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn setAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
        value: Value,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        clear_error(req, this);
        constructed(req, this)?;
        attribute_set(req, this, attribute, &value, 2)
    }
}
