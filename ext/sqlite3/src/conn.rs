//! `SQLite3`: `php_sqlite3_db_object` as [`Conn`], its handlers, the
//! errors every class raises through it (`php_sqlite3_error`), and the
//! methods of `class SQLite3` in `sqlite3.stub.php`'s order.

use std::collections::BTreeMap;

use php_ext::abi::{
    Callable, GcTable, Instance, ObjectHandlers, OrFalse, Path, Refusals, This, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{
    Level, Thrown, argument_error, docref, fatal, throw, throw_error, throw_error_chained,
    while_pending,
};
use php_ext::request::{Foreign, Request, Resolve, Streams as _};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_pdo_sqlite::BlobStream;
use php_ext_pdo_sqlite::ffi::{
    self, Conn as Connection, Db, Function, Handler, Registration, SQLITE_BUSY, SQLITE_LOCKED,
    SQLITE_OK, SQLITE_OPEN_READONLY, SQLITE_OPEN_READWRITE, Stmt,
};

use crate::callbacks::serving;
use crate::result::{self, Origin};
use crate::stmt::{self, Step};

/// A statement's place in its connection's list, php's `free_list`
/// entry: unique for the life of the `SQLite3` object, so a statement
/// of a connection since closed and reopened names nothing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct StmtId(u64);

/// `php_sqlite3_db_object` less its `zend_object`.
#[derive(Default)]
pub(crate) struct Conn {
    pub(crate) link: Link,
    /// `exception`, what `enableExceptions` set.
    pub(crate) exceptions: bool,
    /// The next statement's id.
    next: u64,
    /// `authorizer_fcc`, the callable the open connection's authorizer
    /// calls through a view of it ([`Db::set_authorizer`]), which
    /// `SQLite3::close` leaves on the object for
    /// `php_sqlite3_object_free_storage` to give up, and a successful
    /// `SQLite3::open` forgets ([`Forgotten`]). Boxed, so the payload
    /// fits the struct php allocates.
    authorizer: Option<Box<Handler>>,
    /// `funcs`, every function and aggregate created on the object,
    /// across a close and an open, oldest first: php prepends each, so
    /// its walk from the head is this list's from the end.
    funcs: Vec<Registration>,
    /// `collations`, as `funcs`.
    collations: Vec<Registration>,
}

/// Which of the object's lists a registration joins.
#[derive(Clone, Copy)]
enum List {
    Funcs,
    Collations,
}

impl Conn {
    fn list(&mut self, list: List) -> &mut Vec<Registration> {
        match list {
            List::Funcs => &mut self.funcs,
            List::Collations => &mut self.collations,
        }
    }
}

/// Request-scoped (ADR-0005): the authorizers `SQLite3::open` forgot,
/// `db_obj->authorizer_fcc = empty_fcall_info_cache` with no
/// `zend_fcc_dtor` (`Z/ext/sqlite3/sqlite3.c:169`), which php leaks
/// (#2514). The leak is copied where it is observable: no object owns
/// them, so what they hold is destructed by the store's walk at the
/// request's end, and `rshutdown`, which runs after that walk, gives
/// them up, where php frees them with the request's memory.
#[derive(Default)]
pub(crate) struct Forgotten(Vec<Handler>);

/// `RSHUTDOWN`'s part here: the authorizers [`Forgotten`] holds given up.
pub(crate) fn rshutdown(req: &mut Request) {
    let forgotten = std::mem::take(&mut req.state::<Forgotten>().0);
    for handler in forgotten {
        handler.give_up(req);
    }
}

/// `initialised` and `db` as one state.
#[derive(Default)]
pub(crate) enum Link {
    /// `db` null: never opened, every method's `Error`.
    #[default]
    Unopened,
    Open(Box<Open>),
    /// `db` set and `initialised` false: closed, or a failed open.
    Closed,
}

/// An open connection and what hangs off it.
pub(crate) struct Open {
    /// `db`, which the request keeps for the object (ADR-0013).
    owner: Foreign<Connection>,
    /// `free_list`: each statement prepared, none for a text that held
    /// no statement, finalized when it leaves the list.
    stmts: BTreeMap<StmtId, Option<Stmt>>,
}

impl Open {
    /// The connection's name.
    pub(crate) fn db(&self) -> Db {
        Db::of(&self.owner)
    }
}

/// php's `Error` for an object a method cannot serve,
/// `SQLITE3_CHECK_INITIALIZED`'s.
pub(crate) fn not_ready(req: &mut Request, class: &str) -> Thrown {
    let text =
        format!("The {class} object has not been correctly initialised or is already closed");
    throw_error(req, "Error", &text)
}

/// `php_sqlite3_error` with no throwable pending: a `SQLite3Exception`
/// with `code` where `exceptions` is on, `E_WARNING` otherwise. With one
/// pending, [`fail_after`]'s, and the throwable is handed on either way.
/// php prints the message with `vspprintf` and throws it with
/// `zend_throw_exception` before it frees the buffer, so the throw is
/// [`throw_error`]'s, the buffer standing across it:
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
/// $db = new SQLite3(':memory:'); $db->enableExceptions(true);
/// $db->createFunction('f', function () { throw new Exception('x'); });
/// pk(function () use ($db) { try { $db->query('SELECT f()'); } catch (Exception $e) {} });
/// pk(function () use ($db) { try { $db->query('SELECT 1 FROM nowhere'); } catch (Exception $e) {} });
/// pk(function () use ($db) { try { $db->exec('SELECT 1 FROM nowhere'); } catch (Exception $e) {} });
/// ```
/// ```expect
/// 4992
/// 2616
/// 2616
/// ```
pub(crate) fn fail(
    req: &mut Request,
    exceptions: bool,
    pending: Option<Thrown>,
    code: i64,
    message: &str,
) -> Result<(), Thrown> {
    if let Some(pending) = pending {
        return Err(fail_after(req, exceptions, pending, code, message));
    }
    if !exceptions {
        docref(req, Level::Warning, message);
        return Ok(());
    }
    let thrown = throw_error(req, "SQLite3Exception", message);
    Err(with_code(req, thrown, code))
}

/// `php_sqlite3_error` with a throwable pending, as php raises it with
/// `EG(exception)` set: the `SQLite3Exception` chained over it, or the warning
/// raised while it is pending and the throwable handed on. The throw is
/// [`throw_error_chained`], `vspprintf`'s buffer standing across it as
/// [`fail`] says.
pub(crate) fn fail_after(
    req: &mut Request,
    exceptions: bool,
    pending: Thrown,
    code: i64,
    message: &str,
) -> Thrown {
    if exceptions {
        let thrown = throw_error_chained(req, pending, "SQLite3Exception", message);
        return with_code(req, thrown, code);
    }
    while_pending(req, &pending, |req| docref(req, Level::Warning, message));
    pending
}

/// The throwable's `code`, `zend_throw_exception`'s third argument.
fn with_code(req: &mut Request, thrown: Thrown, code: i64) -> Thrown {
    let Some(object) = thrown.exception().copied() else {
        return thrown;
    };
    let code = Owner::int(code);
    let written = php_ext::call::write_prop(req, &object, b"code", code);
    match written {
        Ok(()) => thrown,
        Err(failed) => failed,
    }
}

/// The text of a library message.
pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A new object of an internal class of this extension, `object_init_ex`.
pub(crate) fn instantiate(req: &mut Request, class: &[u8]) -> Result<Owner<PhpObject>, Thrown> {
    let Some(id) = lookup_class(req, class)? else {
        return Err(fatal(req, "InternalError: a sqlite3 class is not linked"));
    };
    new_object(req, id)
}

/// The `Conn` of a `SQLite3` object.
pub(crate) fn conn(req: &mut Request, object: PhpObject) -> Result<&mut Conn, Thrown> {
    object.native_mut_or_fatal::<Conn>(req)
}

/// The open connection of a `SQLite3`, its name, and whether its errors
/// throw; php's `Error` where it is not open.
pub(crate) fn open_db(req: &mut Request, object: PhpObject) -> Result<(Db, bool), Thrown> {
    let conn = conn(req, object)?;
    match &conn.link {
        Link::Open(open) => Ok((open.db(), conn.exceptions)),
        _ => Err(not_ready(req, "SQLite3")),
    }
}

/// `zend_llist_add_element(&free_list, ..)`: the statement in the list,
/// its id, `None` where the connection is not open.
pub(crate) fn add_stmt(req: &mut Request, object: PhpObject, stmt: Option<Stmt>) -> Option<StmtId> {
    let conn = object.native_mut::<Conn>(req)?;
    let Link::Open(open) = &mut conn.link else {
        return None;
    };
    let id = StmtId(conn.next);
    conn.next += 1;
    open.stmts.insert(id, stmt);
    Some(id)
}

/// `zend_llist_del_element`: the statement out of the list and
/// finalized; whether it was there.
pub(crate) fn remove_stmt(req: &mut Request, object: PhpObject, id: StmtId) -> bool {
    match object.native_mut::<Conn>(req).map(|conn| &mut conn.link) {
        Some(Link::Open(open)) => open.stmts.remove(&id).is_some(),
        _ => false,
    }
}

/// A statement taken out of its connection's list for one call, with
/// what the call reaches beside it.
pub(crate) struct Taken {
    pub(crate) owner: PhpObject,
    id: StmtId,
    pub(crate) stmt: Option<Stmt>,
    pub(crate) exceptions: bool,
}

impl Taken {
    pub(crate) fn id(&self) -> StmtId {
        self.id
    }
}

/// The statement `id` of `owner`'s connection, out of the list until
/// [`give_back`]; `None` where it is not in the list.
pub(crate) fn take(req: &mut Request, owner: PhpObject, id: StmtId) -> Option<Taken> {
    let conn = owner.native_mut::<Conn>(req)?;
    let exceptions = conn.exceptions;
    let Link::Open(open) = &mut conn.link else {
        return None;
    };
    let stmt = open.stmts.remove(&id)?;
    Some(Taken {
        owner,
        id,
        stmt,
        exceptions,
    })
}

/// The statement back in its list; finalized where the list is gone,
/// which a statement out of it keeps from happening (the library refuses
/// to close a connection a statement still holds).
pub(crate) fn give_back(req: &mut Request, taken: Taken) {
    let Taken {
        owner, id, stmt, ..
    } = taken;
    if let Some(Link::Open(open)) = owner.native_mut::<Conn>(req).map(|conn| &mut conn.link) {
        open.stmts.insert(id, stmt);
    }
}

/// What a column holds as php reads it, `sqlite_value_to_zval`: text
/// up to its first NUL, as `ZVAL_STRING` copies it.
pub(crate) fn column_value(
    req: &mut Request,
    stmt: &Stmt,
    column: usize,
) -> Result<Owner<Value>, Thrown> {
    let value = match stmt.column_type(column) {
        ffi::SQLITE_INTEGER => Value::int(stmt.column_int64(column)),
        ffi::SQLITE_FLOAT => Value::float(stmt.column_double(column)),
        ffi::SQLITE_NULL => Value::null(),
        ffi::SQLITE_TEXT => {
            let text = stmt.column_text(column);
            return Ok(PhpString::new(req, php_ext::coerce::c_str(&text))?.into_value());
        }
        _ => return Ok(PhpString::new(req, &stmt.column_blob(column))?.into_value()),
    };
    Ok(Owner::take(req, value))
}

/// `SQLITE3_OPEN_READWRITE | SQLITE3_OPEN_CREATE`.
const OPEN_DEFAULT: i64 = 6;

/// `class SQLite3`, `@not-serializable`, uncloneable.
///
/// ```php
/// $db = new SQLite3(':memory:');
/// try { clone $db; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($db); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// $r = new ReflectionClass('SQLite3');
/// $u = $r->newInstanceWithoutConstructor();
/// try { $u->exec('SELECT 1'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Trying to clone an uncloneable object of class SQLite3
/// Serialization of 'SQLite3' is not allowed
/// The SQLite3 object has not been correctly initialised or is already closed
/// ```
#[php_class(not_serializable, size = 176, payload = Conn, handlers = HANDLERS)]
pub(crate) struct SQLite3;

/// `sqlite3_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `php_sqlite3_object_free_storage`: the statements finalized, the
    /// authorizer given up, then every function and every collation,
    /// each list newest first, as php walks the lists it prepends to,
    /// those of a connection since closed after the open one's, then
    /// the connection closed:
    ///
    /// ```php
    /// class A { function __construct(public $n) {} function __destruct() { echo "dtor {$this->n}\n"; } }
    /// $db = new SQLite3(':memory:');
    /// $f1 = new A('f1'); $db->createFunction('f1', function () use ($f1) { return 1; }); unset($f1);
    /// $f2 = new A('f2'); $db->createFunction('f2', function () use ($f2) { return 1; }); unset($f2);
    /// $c = new A('c'); $db->createCollation('c', function ($a, $b) use ($c) { return 0; }); unset($c);
    /// $au = new A('auth'); $db->setAuthorizer(function () use ($au) { return SQLite3::OK; }); unset($au);
    /// unset($db);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// dtor auth
    /// dtor f2
    /// dtor f1
    /// dtor c
    /// end
    /// ```
    ///
    /// ```php
    /// class A { function __construct(public $n) {} function __destruct() { echo "dtor {$this->n}\n"; } }
    /// $db = new SQLite3(':memory:');
    /// $f1 = new A('f1'); $db->createFunction('f1', function () use ($f1) { return 1; }); unset($f1);
    /// $c1 = new A('c1'); $db->createCollation('c1', function ($a, $b) use ($c1) { return 0; }); unset($c1);
    /// $f2 = new A('f2'); $db->createAggregate('f2', function () use ($f2) { return 1; }, function () { return 1; }); unset($f2);
    /// $db->close();
    /// $db->open(':memory:');
    /// $f3 = new A('f3'); $db->createFunction('f3', function () use ($f3) { return 1; }); unset($f3);
    /// $c2 = new A('c2'); $db->createCollation('c2', function ($a, $b) use ($c2) { return 0; }); unset($c2);
    /// $au = new A('auth'); $db->setAuthorizer(function () use ($au) { return SQLite3::OK; }); unset($au);
    /// unset($db);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// dtor auth
    /// dtor f3
    /// dtor f2
    /// dtor f1
    /// dtor c2
    /// dtor c1
    /// end
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(conn) = object.native_mut::<Conn>(req) else {
            return;
        };
        let link = std::mem::take(&mut conn.link);
        let authorizer = conn.authorizer.take();
        let funcs = std::mem::take(&mut conn.funcs);
        let collations = std::mem::take(&mut conn.collations);
        let owner = match link {
            Link::Open(open) => {
                let Open { owner, stmts } = *open;
                drop(stmts);
                Db::of(&owner).set_authorizer(req, None);
                Some(owner)
            }
            Link::Unopened | Link::Closed => None,
        };
        if let Some(handler) = authorizer {
            handler.give_up(req);
        }
        for registration in funcs.into_iter().rev().chain(collations.into_iter().rev()) {
            for handler in registration.unregister(req) {
                handler.give_up(req);
            }
        }
        if let Some(owner) = owner {
            Db::release(req, owner);
        }
    }

    /// `php_sqlite3_get_gc`: the authorizer, the functions and the
    /// collations.
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(conn) = object.native::<Conn>(req) else {
            return GcTable::Properties;
        };
        if let Some(handler) = &conn.authorizer {
            handler.visit(visit);
        }
        for registration in conn.funcs.iter().rev().chain(conn.collations.iter().rev()) {
            registration.visit(visit);
        }
        GcTable::Properties
    }
}

/// `SQLite3::open`'s body, `__construct`'s too.
fn open(req: &mut Request, object: PhpObject, filename: &[u8], flags: i64) -> Result<(), Thrown> {
    if matches!(conn(req, object)?.link, Link::Open(_)) {
        return Err(throw(req, "Exception", "Already initialised DB Object"));
    }
    let path = if filename.is_empty() || filename == b":memory:" {
        filename.to_vec()
    } else {
        let cwd = req.cwd().into_os_string().into_encoded_bytes();
        let Some(full) = req.virtual_file_ex(&cwd, filename, Resolve::FilePath) else {
            return Err(throw(req, "Exception", "Unable to expand filepath"));
        };
        if req.check_open_basedir(&full, true).is_err() {
            let shown = text(&full);
            return Err(throw(
                req,
                "Exception",
                &format!("open_basedir prohibits opening {shown}"),
            ));
        }
        full
    };
    let exceptions = conn(req, object)?.exceptions;
    // php hands the library its `zend_long` as an `int`.
    #[allow(clippy::cast_possible_truncation)]
    let opened = Db::open(req, &path, flags as i32, serving(exceptions));
    let owner = match opened {
        Ok(owner) => owner,
        Err((_, message)) => {
            conn(req, object)?.link = Link::Closed;
            let message = format!("Unable to open database: {}", text(&message));
            return Err(throw(req, "Exception", &message));
        }
    };
    if req
        .ini_get(b"sqlite3.defensive")
        .is_some_and(Request::ini_parse_bool)
    {
        Db::of(&owner).defensive(req);
    }
    let open = Link::Open(Box::new(Open {
        owner,
        stmts: BTreeMap::new(),
    }));
    let conn = conn(req, object)?;
    conn.link = open;
    if let Some(forgotten) = conn.authorizer.take() {
        req.state::<Forgotten>().0.push(*forgotten);
    }
    Ok(())
}

/// The connection's library state, read where it was ever opened:
/// `SQLITE3_CHECK_INITIALIZED(db_obj, db_obj->db, ..)`, `None` once closed.
fn ever_opened(req: &mut Request, object: PhpObject) -> Result<Option<Db>, Thrown> {
    match &conn(req, object)?.link {
        Link::Open(open) => Ok(Some(open.db())),
        Link::Closed => Ok(None),
        Link::Unopened => Err(not_ready(req, "SQLite3")),
    }
}

/// `sqlite3_create_function` or `sqlite3_create_collation` of what
/// `make` registers, its callables kept (`zend_fcc_dup`) once the
/// connection stands, kept on `list` of the object; the handlers given
/// up where the library refused it.
fn register(
    req: &mut Request,
    object: PhpObject,
    list: List,
    make: impl FnOnce(&mut Request, Db) -> Result<Registration, Vec<Handler>>,
) -> Result<bool, Thrown> {
    let db = match &conn(req, object)?.link {
        Link::Open(open) => Some(open.db()),
        _ => None,
    };
    let outcome = match db {
        Some(db) => make(req, db),
        None => Err(Vec::new()),
    };
    match outcome {
        Ok(registration) => {
            conn(req, object)?.list(list).push(registration);
            Ok(true)
        }
        Err(handlers) => {
            for handler in handlers {
                handler.give_up(req);
            }
            Ok(false)
        }
    }
}

/// `SQLITE3_CHECK_INITIALIZED` for a method that registers callables,
/// which php checks after it has parsed them.
fn check_open(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    open_db(req, object).map(|_| ())
}

/// `query()` past its checks: the statement prepared on a new
/// `SQLite3Stmt`, stepped once and reset, and a `SQLite3Result` over it.
fn run_query(
    req: &mut Request,
    object: PhpObject,
    sql: &[u8],
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let (db, exceptions) = open_db(req, object)?;
    let stmt_owner = instantiate(req, b"SQLite3Stmt")?;
    let stmt_object = *stmt_owner;
    let held = Owner::take(req, object);
    stmt::attach(req, stmt_object, held);
    let (prepared, thrown) = db.prepare(req, sql);
    let stmt = match prepared {
        Ok(stmt) => stmt,
        Err(code) => {
            let message = format!("Unable to prepare statement: {}", text(&db.errmsg(req)));
            stmt_owner.give_up(req);
            fail(req, exceptions, thrown, i64::from(code), &message)?;
            return Ok(None);
        }
    };
    let Some(id) = add_stmt(req, object, stmt) else {
        stmt_owner.give_up(req);
        return Err(not_ready(req, "SQLite3"));
    };
    stmt::set_id(req, stmt_object, id);
    let result_object = instantiate(req, b"SQLite3Result")?;
    result::attach(req, *result_object, stmt_owner, Origin::Query);
    let Some(mut taken) = take(req, object, id) else {
        result_object.give_up(req);
        return Err(not_ready(req, "SQLite3"));
    };
    let (step, thrown) = stmt::step(req, taken.stmt.as_ref());
    let reset = match (&step, taken.stmt.as_ref()) {
        (Step::Row | Step::Done, Some(stmt)) => stmt.reset(req).1,
        _ => None,
    };
    let thrown = thrown.or(reset);
    if matches!(step, Step::Row | Step::Done) {
        give_back(req, taken);
        return match thrown {
            Some(thrown) => {
                result_object.give_up(req);
                Err(thrown)
            }
            None => Ok(Some(result_object)),
        };
    }
    let failed = thrown.map_or_else(
        || {
            let message = format!("Unable to execute statement: {}", text(&db.errmsg(req)));
            let code = i64::from(db.errcode(req));
            fail(req, exceptions, None, code, &message)
        },
        Err,
    );
    taken.stmt = None;
    drop(taken);
    result_object.give_up(req);
    failed.map(|()| None)
}

/// `querySingle()` past its checks.
fn query_single(
    req: &mut Request,
    object: PhpObject,
    sql: &[u8],
    entire: bool,
) -> Result<Owner<Value>, Thrown> {
    let (db, exceptions) = open_db(req, object)?;
    let (prepared, thrown) = db.prepare(req, sql);
    let stmt = match prepared {
        Ok(stmt) => stmt,
        Err(code) => {
            let message = format!("Unable to prepare statement: {}", text(&db.errmsg(req)));
            fail(req, exceptions, thrown, i64::from(code), &message)?;
            return Ok(Owner::bool(false));
        }
    };
    let (step, thrown) = stmt::step(req, stmt.as_ref());
    let answer = match (step, stmt.as_ref()) {
        (Step::Row, Some(stmt)) if entire => {
            let mut row = PhpArray::new(req)?;
            for column in 0..stmt.data_count() {
                let value = column_value(req, stmt, column)?;
                let name = stmt.column_name(column).unwrap_or_default();
                row.set_symtable_str(req, &name, value)?;
            }
            row.into_value()
        }
        (Step::Row, Some(stmt)) => column_value(req, stmt, 0)?,
        (Step::Done, _) if entire => PhpArray::new(req)?.into_value(),
        (Step::Done, _) => Owner::null(),
        _ => {
            if thrown.is_none() {
                let message = format!("Unable to execute statement: {}", text(&db.errmsg(req)));
                let code = i64::from(db.errcode(req));
                fail(req, exceptions, None, code, &message)?;
            }
            Owner::bool(false)
        }
    };
    drop(stmt);
    match thrown {
        Some(thrown) => {
            answer.give_up(req);
            Err(thrown)
        }
        None => Ok(answer),
    }
}

/// `sqlite3_mprintf("%q", ..)`: each `'` doubled, up to the first NUL.
fn escape(bytes: &[u8]) -> Vec<u8> {
    let bytes = php_ext::coerce::c_str(bytes);
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        out.push(b);
        if b == b'\'' {
            out.push(b'\'');
        }
    }
    out
}

/// `loadExtension()` past its checks: the library under
/// `sqlite3.extension_dir`, and nowhere outside it.
fn load_extension(
    req: &mut Request,
    db: Db,
    exceptions: bool,
    name: &[u8],
) -> Result<bool, Thrown> {
    let dir = req
        .ini_get(b"sqlite3.extension_dir")
        .unwrap_or_default()
        .to_vec();
    if dir.is_empty() {
        fail(req, exceptions, None, 0, "SQLite Extensions are disabled")?;
        return Ok(false);
    }
    if name.is_empty() {
        return Err(argument_error(req, "ValueError", 1, "must not be empty"));
    }
    let mut lib = dir.clone();
    if !dir.ends_with(b"/") {
        lib.push(b'/');
    }
    lib.extend_from_slice(name);
    let Some(full) = req.realpath(&lib) else {
        let message = format!("Unable to load extension at '{}'", text(&lib));
        fail(req, exceptions, None, 0, &message)?;
        return Ok(false);
    };
    let full = full.into_os_string().into_encoded_bytes();
    if !full.starts_with(&dir) {
        let message = "Unable to open extensions outside the defined directory";
        fail(req, exceptions, None, 0, message)?;
        return Ok(false);
    }
    match db.load_extension(req, &full) {
        Ok(()) => Ok(true),
        Err(message) => {
            fail(req, exceptions, None, 0, &text(&message))?;
            Ok(false)
        }
    }
}

type Answer<T> = Result<T, Thrown>;

#[php_class]
impl SQLite3 {
    const OK: i64 = 0;
    const DENY: i64 = 1;
    const IGNORE: i64 = 2;
    const CREATE_INDEX: i64 = 1;
    const CREATE_TABLE: i64 = 2;
    const CREATE_TEMP_INDEX: i64 = 3;
    const CREATE_TEMP_TABLE: i64 = 4;
    const CREATE_TEMP_TRIGGER: i64 = 5;
    const CREATE_TEMP_VIEW: i64 = 6;
    const CREATE_TRIGGER: i64 = 7;
    const CREATE_VIEW: i64 = 8;
    const DELETE: i64 = 9;
    const DROP_INDEX: i64 = 10;
    const DROP_TABLE: i64 = 11;
    const DROP_TEMP_INDEX: i64 = 12;
    const DROP_TEMP_TABLE: i64 = 13;
    const DROP_TEMP_TRIGGER: i64 = 14;
    const DROP_TEMP_VIEW: i64 = 15;
    const DROP_TRIGGER: i64 = 16;
    const DROP_VIEW: i64 = 17;
    const INSERT: i64 = 18;
    const PRAGMA: i64 = 19;
    const READ: i64 = 20;
    const SELECT: i64 = 21;
    const TRANSACTION: i64 = 22;
    const UPDATE: i64 = 23;
    const ATTACH: i64 = 24;
    const DETACH: i64 = 25;
    const ALTER_TABLE: i64 = 26;
    const REINDEX: i64 = 27;
    const ANALYZE: i64 = 28;
    const CREATE_VTABLE: i64 = 29;
    const DROP_VTABLE: i64 = 30;
    const FUNCTION: i64 = 31;
    const SAVEPOINT: i64 = 32;
    const COPY: i64 = 0;
    const RECURSIVE: i64 = 33;

    /// `SQLite3::__construct()`, `@implementation-alias SQLite3::open`.
    ///
    /// ```php
    /// try { new SQLite3('/no/such/dir/x.db'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// $db = new SQLite3(':memory:');
    /// try { $db->open(':memory:'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Unable to open database: unable to open database file
    /// Already initialised DB Object
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        filename: Path,
        #[php(default = SQLITE3_OPEN_READWRITE | SQLITE3_OPEN_CREATE)] flags: i64,
        #[php(default = "")] encryptionKey: PhpString,
    ) -> Answer<()> {
        let _ = encryptionKey;
        open(req, this.object(), filename.0.as_bytes(), flags)
    }

    /// `SQLite3::open()`: a closed connection may open again.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->close();
    /// $db->open(':memory:', SQLITE3_OPEN_READWRITE);
    /// var_dump($db->querySingle('SELECT 42'));
    /// ```
    /// ```expect
    /// int(42)
    /// ```
    ///
    /// The open forgets the authorizer a connection since closed held,
    /// `authorizer_fcc = empty_fcall_info_cache` with no `zend_fcc_dtor`
    /// (`Z/ext/sqlite3/sqlite3.c:169`), so php leaks it: its callable
    /// outlives the object and what it holds is destructed at the
    /// request's end (#2514):
    ///
    /// ```php
    /// class A { function __destruct() { echo "dtor\n"; } }
    /// $db = new SQLite3(':memory:');
    /// $a = new A;
    /// $db->setAuthorizer(function () use ($a) { return SQLite3::OK; });
    /// unset($a);
    /// $db->close();
    /// $db->open(':memory:');
    /// unset($db);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// dtor
    /// ```
    ///
    /// The store's walk at the request's end destructs what the leaked
    /// callable holds in the store's order, as it does any object an
    /// owner still holds then: `$c` takes the store's slot the freed
    /// `$db` left, so its destructor comes before `$a`'s.
    ///
    /// ```php
    /// class A { public $self; function __construct(public $n) {} function __destruct() { echo "dtor{$this->n}\n"; } }
    /// $b = new A(0);
    /// $b->self = $b;
    /// $db = new SQLite3(':memory:');
    /// $a = new A(1);
    /// $db->setAuthorizer(function () use ($a) { return SQLite3::OK; });
    /// unset($a);
    /// $db->close();
    /// $db->open(':memory:');
    /// $db->setAuthorizer(null);
    /// unset($db);
    /// $c = new A(2);
    /// $c->self = $c;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// dtor0
    /// dtor2
    /// dtor1
    /// ```
    #[php(tentative)]
    fn open(
        req: &mut Request,
        this: This<Self>,
        filename: Path,
        #[php(default = SQLITE3_OPEN_READWRITE | SQLITE3_OPEN_CREATE)] flags: i64,
        #[php(default = "")] encryptionKey: PhpString,
    ) -> Answer<()> {
        let _ = (encryptionKey, OPEN_DEFAULT);
        open(req, this.object(), filename.0.as_bytes(), flags)
    }

    /// `SQLite3::close()`: the statements finalized, then the connection
    /// closed; `true` for one already closed. The callables its
    /// functions, aggregates, collations and authorizer held stay on the
    /// object until it is freed, as php's `free_storage` alone gives them
    /// up:
    ///
    /// ```php
    /// class A { function __destruct() { echo "dtor\n"; } }
    /// $db = new SQLite3(':memory:');
    /// $a = new A;
    /// $db->createFunction('f', function () use ($a) { return 1; });
    /// unset($a);
    /// $db->close();
    /// echo "closed\n";
    /// unset($db);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// closed
    /// dtor
    /// end
    /// ```
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT 1');
    /// var_dump($db->close(), $db->close());
    /// try { $s->execute(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($db->lastErrorCode(), $db->lastErrorMsg());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// The SQLite3 object has not been correctly initialised or is already closed
    /// int(0)
    /// string(0) ""
    /// ```
    ///
    /// From inside a user function or a collation the call in progress
    /// holds the connection, so the close is refused as the library
    /// refuses it for the statement the call runs, and the call runs to
    /// its end:
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->createFunction('f', function ($x) use ($db) { var_dump($db->close()); return $x * 2; });
    /// $db->createCollation('c', function ($a, $b) use ($db) { var_dump($db->close()); return strcmp($a, $b); });
    /// var_dump($db->querySingle('SELECT f(21)'));
    /// var_dump($db->querySingle("SELECT 'b' AS x UNION ALL SELECT 'a' ORDER BY x COLLATE c"));
    /// var_dump($db->lastErrorMsg(), $db->close());
    /// ```
    /// ```expect
    /// Warning: SQLite3::close(): Unable to close database: unable to close due to unfinalized statements or unfinished backups in %s on line 3
    /// bool(false)
    /// int(42)
    ///
    /// Warning: SQLite3::close(): Unable to close database: unable to close due to unfinalized statements or unfinished backups in %s on line 4
    /// bool(false)
    /// string(1) "a"
    /// string(12) "not an error"
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn close(req: &mut Request, this: This<Self>) -> Answer<bool> {
        let object = this.object();
        let conn = conn(req, object)?;
        let exceptions = conn.exceptions;
        let Link::Open(open) = std::mem::replace(&mut conn.link, Link::Closed) else {
            if matches!(conn.link, Link::Closed) {
                return Ok(true);
            }
            conn.link = Link::Unopened;
            return Ok(true);
        };
        let Open { owner, stmts } = *open;
        drop(stmts);
        let mut funcs = std::mem::take(&mut conn.funcs);
        let mut collations = std::mem::take(&mut conn.collations);
        let closed = Db::close(req, owner, funcs.iter_mut().chain(&mut collations));
        let conn = self::conn(req, object)?;
        conn.funcs = funcs;
        conn.collations = collations;
        match closed {
            Ok(()) => Ok(true),
            Err(unclosed) => {
                let ffi::Unclosed {
                    owner,
                    code,
                    message,
                } = *unclosed;
                conn.link = Link::Open(Box::new(Open {
                    owner,
                    stmts: BTreeMap::new(),
                }));
                let message = format!("Unable to close database: {}", text(&message));
                fail(req, exceptions, None, i64::from(code), &message)?;
                Ok(false)
            }
        }
    }

    /// `SQLite3::version()`.
    ///
    /// ```php
    /// $v = SQLite3::version();
    /// var_dump(array_keys($v), is_string($v['versionString']), $v['versionNumber'] > 3000000);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(13) "versionString"
    ///   [1]=>
    ///   string(13) "versionNumber"
    /// }
    /// bool(true)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn version(req: &mut Request) -> Answer<Owner<PhpArray>> {
        let mut out = PhpArray::new(req)?;
        let version = PhpString::new(req, ffi::version().as_bytes())?.into_value();
        out.set_str(req, b"versionString", version)?;
        out.set_str(req, b"versionNumber", Owner::int(ffi::version_number()))?;
        Ok(out)
    }

    /// `SQLite3::lastInsertRowID()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec('CREATE TABLE t (a); INSERT INTO t VALUES (1), (2)');
    /// var_dump($db->lastInsertRowID());
    /// ```
    /// ```expect
    /// int(2)
    /// ```
    #[php(tentative)]
    fn lastInsertRowID(req: &mut Request, this: This<Self>) -> Answer<i64> {
        Ok(open_db(req, this.object())?.0.last_insert_rowid(req))
    }

    /// `SQLite3::lastErrorCode()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// @$db->exec('NOPE');
    /// var_dump($db->lastErrorCode());
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    #[php(tentative)]
    fn lastErrorCode(req: &mut Request, this: This<Self>) -> Answer<i64> {
        Ok(ever_opened(req, this.object())?.map_or(0, |db| i64::from(db.errcode(req))))
    }

    /// `SQLite3::lastExtendedErrorCode()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec('CREATE TABLE t (a UNIQUE); INSERT INTO t VALUES (1)');
    /// @$db->exec('INSERT INTO t VALUES (1)');
    /// var_dump($db->lastErrorCode(), $db->lastExtendedErrorCode());
    /// ```
    /// ```expect
    /// int(19)
    /// int(2067)
    /// ```
    #[php(tentative)]
    fn lastExtendedErrorCode(req: &mut Request, this: This<Self>) -> Answer<i64> {
        Ok(ever_opened(req, this.object())?.map_or(0, |db| i64::from(db.extended_errcode(req))))
    }

    /// `SQLite3::lastErrorMsg()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->lastErrorMsg());
    /// ```
    /// ```expect
    /// string(12) "not an error"
    /// ```
    #[php(tentative)]
    fn lastErrorMsg(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        let message = ever_opened(req, this.object())?
            .map(|db| db.errmsg(req))
            .unwrap_or_default();
        PhpString::new(req, &message)
    }

    /// `SQLite3::changes()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec('CREATE TABLE t (a); INSERT INTO t VALUES (1), (2), (3)');
    /// var_dump($db->changes());
    /// ```
    /// ```expect
    /// int(3)
    /// ```
    #[php(tentative)]
    fn changes(req: &mut Request, this: This<Self>) -> Answer<i64> {
        Ok(open_db(req, this.object())?.0.changes(req))
    }

    /// `SQLite3::busyTimeout()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->busyTimeout(100), $db->querySingle('PRAGMA busy_timeout'));
    /// ```
    /// ```expect
    /// bool(true)
    /// int(100)
    /// ```
    #[php(tentative)]
    fn busyTimeout(req: &mut Request, this: This<Self>, milliseconds: i64) -> Answer<bool> {
        open_db(req, this.object())?
            .0
            .busy_timeout(req, milliseconds);
        Ok(true)
    }

    /// `SQLite3::loadExtension()`: nothing while
    /// `sqlite3.extension_dir` is unset.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->loadExtension('x.so'));
    /// ```
    /// ```expect
    /// Warning: SQLite3::loadExtension(): SQLite Extensions are disabled in %s on line 3
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn loadExtension(req: &mut Request, this: This<Self>, name: PhpString) -> Answer<bool> {
        let (db, exceptions) = open_db(req, this.object())?;
        let name = name.as_bytes().to_vec();
        load_extension(req, db, exceptions, &name)
    }

    /// `SQLite3::backup()`: the source database copied whole into the
    /// destination's.
    ///
    /// ```php
    /// $a = new SQLite3(':memory:');
    /// $a->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('x')");
    /// $b = new SQLite3(':memory:');
    /// var_dump($a->backup($b), $b->querySingle('SELECT a FROM t'));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "x"
    /// ```
    #[php(tentative)]
    fn backup(
        req: &mut Request,
        this: This<Self>,
        destination: Instance<SQLite3>,
        #[php(default = "main")] sourceDatabase: Path,
        #[php(default = "main")] destinationDatabase: Path,
    ) -> Answer<bool> {
        let (source, exceptions) = open_db(req, this.object())?;
        let (target, _) = open_db(req, destination.object())?;
        let rc = source.backup(
            req,
            target,
            sourceDatabase.0.as_bytes(),
            destinationDatabase.0.as_bytes(),
        );
        if rc == SQLITE_OK {
            return Ok(true);
        }
        let message = match rc {
            SQLITE_BUSY => String::from("Backup failed: source database is busy"),
            SQLITE_LOCKED => String::from("Backup failed: source database is locked"),
            _ => format!("Backup failed: {}", text(&source.errmsg(req))),
        };
        fail(req, exceptions, None, i64::from(rc), &message)?;
        Ok(false)
    }

    /// `SQLite3::escapeString()`.
    ///
    /// ```php
    /// var_dump(SQLite3::escapeString("it's"), SQLite3::escapeString(''));
    /// ```
    /// ```expect
    /// string(5) "it''s"
    /// string(0) ""
    /// ```
    #[php(tentative)]
    fn escapeString(req: &mut Request, string: PhpString) -> Answer<Owner<PhpString>> {
        PhpString::new(req, &escape(string.as_bytes()))
    }

    /// `SQLite3::prepare()`: `false` for an empty text, php's warning
    /// and `false` for one the library refuses.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump(get_class($db->prepare('SELECT 1')), $db->prepare(''), $db->prepare('NOPE'));
    /// ```
    /// ```expect
    /// Warning: SQLite3::prepare(): Unable to prepare statement: near "NOPE": syntax error in %s on line 3
    /// string(11) "SQLite3Stmt"
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn prepare(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
    ) -> Answer<OrFalse<Owner<Instance<crate::stmt::SQLite3Stmt>>>> {
        let object = this.object();
        open_db(req, object)?;
        if query.as_bytes().is_empty() {
            return Ok(OrFalse::False);
        }
        let sql = query.as_bytes().to_vec();
        Ok(match stmt::prepare_on(req, object, None, &sql)? {
            Some(stmt) => OrFalse::Value(stmt.into_instance()),
            None => OrFalse::False,
        })
    }

    /// `SQLite3::exec()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->exec('CREATE TABLE t (a)'), $db->exec('NOPE'));
    /// ```
    /// ```expect
    /// Warning: SQLite3::exec(): near "NOPE": syntax error in %s on line 3
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn exec(req: &mut Request, this: This<Self>, query: PhpString) -> Answer<bool> {
        let (db, exceptions) = open_db(req, this.object())?;
        let sql = query.as_bytes().to_vec();
        let (rc, thrown) = db.exec(req, &sql);
        if rc == SQLITE_OK {
            return thrown.map_or(Ok(true), Err);
        }
        {
            let message = text(&db.errmsg(req));
            fail(req, exceptions, thrown, i64::from(rc), &message)?;
        }
        Ok(false)
    }

    /// `SQLite3::query()`: a `SQLite3Result` over the statement, stepped
    /// once and reset; executed for its effect alone where the return is
    /// not used.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->query('CREATE TABLE t (a)');
    /// $r = $db->query("INSERT INTO t VALUES ('x')");
    /// var_dump(get_class($r), $db->querySingle('SELECT COUNT(*) FROM t'), $db->query(''));
    /// ```
    /// ```expect
    /// string(13) "SQLite3Result"
    /// int(1)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn query(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
    ) -> Answer<OrFalse<Owner<Instance<crate::result::SQLite3Result>>>> {
        let object = this.object();
        let (db, exceptions) = open_db(req, object)?;
        let sql = query.as_bytes().to_vec();
        if sql.is_empty() {
            return Ok(OrFalse::False);
        }
        if !req.return_value_used() {
            let (rc, thrown) = db.exec(req, &sql);
            if rc != SQLITE_OK {
                {
                    let message = text(&db.errmsg(req));
                    fail(req, exceptions, thrown, i64::from(rc), &message)?;
                }
            } else if let Some(thrown) = thrown {
                return Err(thrown);
            }
            return Ok(OrFalse::False);
        }
        Ok(run_query(req, object, &sql)?
            .map_or(OrFalse::False, |r| OrFalse::Value(r.into_instance())))
    }

    /// `SQLite3::querySingle()`: the first column of the first row, or
    /// the whole row by name.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->querySingle("SELECT 1 AS a, 'b' AS b"), $db->querySingle("SELECT 1 AS a, 'b' AS b", true));
    /// var_dump($db->querySingle('SELECT 1 WHERE 0'), $db->querySingle('SELECT 1 WHERE 0', true));
    /// ```
    /// ```expect
    /// int(1)
    /// array(2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   string(1) "b"
    /// }
    /// NULL
    /// array(0) {
    /// }
    /// ```
    #[php(tentative)]
    fn querySingle(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
        #[php(default = false)] entireRow: bool,
    ) -> Answer<Owner<Value>> {
        let object = this.object();
        let (db, exceptions) = open_db(req, object)?;
        let sql = query.as_bytes().to_vec();
        if sql.is_empty() {
            return Ok(Owner::bool(false));
        }
        if !req.return_value_used() {
            let (rc, thrown) = db.exec(req, &sql);
            if rc != SQLITE_OK {
                {
                    let message = text(&db.errmsg(req));
                    fail(req, exceptions, thrown, i64::from(rc), &message)?;
                }
            } else if let Some(thrown) = thrown {
                return Err(thrown);
            }
            return Ok(Owner::bool(false));
        }
        query_single(req, object, &sql, entireRow)
    }

    /// `SQLite3::createFunction()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->createFunction('twice', fn ($x) => $x * 2, 1, SQLITE3_DETERMINISTIC), $db->createFunction('', 'strlen'));
    /// var_dump($db->querySingle('SELECT twice(21)'), $db->querySingle('SELECT twice(4611686018427387904)'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// int(42)
    /// float(9.223372036854776E+18)
    /// ```
    ///
    /// The callback is kept as `zend_fcc_dup` keeps it, so a
    /// `Class::method` string resolved with `$this` in scope keeps that
    /// object until the connection gives the function up:
    ///
    /// ```php
    /// class A { public $n = 'kept'; function __destruct(){ echo "dtor\n"; } function f($x){ return $this->n . $x; }
    ///   function reg($db){ $db->createFunction('f', 'A::f'); } }
    /// $db = new SQLite3(':memory:');
    /// (new A)->reg($db);
    /// echo "registered\n";
    /// var_dump($db->querySingle("SELECT f('1')"));
    /// unset($db);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// registered
    /// string(5) "kept1"
    /// dtor
    /// end
    /// ```
    #[php(tentative)]
    fn createFunction(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        callback: Callable,
        #[php(default = -1)] argCount: i64,
        #[php(default = 0)] flags: i64,
    ) -> Answer<bool> {
        let object = this.object();
        check_open(req, object)?;
        if name.as_bytes().is_empty() {
            return Ok(false);
        }
        let name = name.as_bytes().to_vec();
        // php hands the library its `zend_long`s as `int`s.
        #[allow(clippy::cast_possible_truncation)]
        register(req, object, List::Funcs, |req, db| {
            let function = Function::Scalar(Owner::take(req, callback));
            db.create_function(req, &name, argCount as i32, flags as i32, function)
                .map_err(|(function, _)| function.into_handlers())
        })
    }

    /// `SQLite3::createAggregate()`: the step handed the aggregate's
    /// value and the row number, the final the value and `0`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('x'), ('y'), ('z')");
    /// $db->createAggregate('cat', fn ($c, $row, $v) => "$c$row$v", fn ($c, $rows) => "$c/$rows");
    /// var_dump($db->querySingle('SELECT cat(a) FROM t'));
    /// ```
    /// ```expect
    /// string(8) "1x2y3z/0"
    /// ```
    #[php(tentative)]
    fn createAggregate(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        stepCallback: Callable,
        finalCallback: Callable,
        #[php(default = -1)] argCount: i64,
    ) -> Answer<bool> {
        let object = this.object();
        check_open(req, object)?;
        if name.as_bytes().is_empty() {
            return Ok(false);
        }
        let name = name.as_bytes().to_vec();
        // php hands the library its `zend_long` as an `int`.
        #[allow(clippy::cast_possible_truncation)]
        register(req, object, List::Funcs, |req, db| {
            let step = Owner::take(req, stepCallback);
            let finalize = Owner::take(req, finalCallback);
            let function = Function::Aggregate { step, finalize };
            db.create_function(req, &name, argCount as i32, 0, function)
                .map_err(|(function, _)| function.into_handlers())
        })
    }

    /// `SQLite3::createCollation()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('a10'), ('a9'), ('a1')");
    /// $db->createCollation('NAT', 'strnatcmp');
    /// $r = $db->query('SELECT a FROM t ORDER BY a COLLATE NAT');
    /// while ($row = $r->fetchArray(SQLITE3_NUM)) echo $row[0], "\n";
    /// ```
    /// ```expect
    /// a1
    /// a9
    /// a10
    /// ```
    #[php(tentative)]
    fn createCollation(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        callback: Callable,
    ) -> Answer<bool> {
        let object = this.object();
        check_open(req, object)?;
        if name.as_bytes().is_empty() {
            return Ok(false);
        }
        let name = name.as_bytes().to_vec();
        register(req, object, List::Collations, |req, db| {
            let handler = Owner::take(req, callback);
            db.create_collation(req, &name, handler)
                .map_err(|(handler, _)| vec![handler])
        })
    }

    /// `SQLite3::openBlob()`: the blob as a stream, read only unless the
    /// bits ask to write.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec("CREATE TABLE t (b BLOB); INSERT INTO t VALUES ('hello')");
    /// $s = $db->openBlob('t', 'b', 1);
    /// var_dump(stream_get_contents($s), fwrite($s, 'x'));
    /// $w = $db->openBlob('t', 'b', 1, 'main', SQLITE3_OPEN_READWRITE);
    /// var_dump(fwrite($w, 'J'));
    /// fclose($w);
    /// var_dump($db->querySingle('SELECT b FROM t'), $db->openBlob('t', 'b', 9));
    /// ```
    /// ```expect
    /// Warning: fwrite(): Can't write to blob stream: is open as read only in %s on line 5
    /// string(5) "hello"
    /// bool(false)
    /// int(1)
    ///
    /// Warning: SQLite3::openBlob(): Unable to open blob: no such rowid: 9 in %s on line 9
    /// string(5) "Jello"
    /// bool(false)
    /// ```
    fn openBlob(
        req: &mut Request,
        this: This<Self>,
        table: PhpString,
        column: PhpString,
        rowid: i64,
        #[php(default = "main")] database: Path,
        #[php(default = SQLITE3_OPEN_READONLY)] flags: i64,
    ) -> Answer<Untyped<Owner<Value>>> {
        let (db, exceptions) = open_db(req, this.object())?;
        let writable = flags & i64::from(SQLITE_OPEN_READWRITE) != 0;
        let opened = db.blob_open(
            req,
            database.0.as_bytes(),
            table.as_bytes(),
            column.as_bytes(),
            rowid,
            writable,
        );
        let blob = match opened {
            Ok(blob) => blob,
            Err(message) => {
                let message = format!("Unable to open blob: {}", text(&message));
                let code = i64::from(db.errcode(req));
                fail(req, exceptions, None, code, &message)?;
                return Ok(Untyped(Owner::bool(false)));
            }
        };
        let read_only = flags & i64::from(SQLITE_OPEN_READONLY) != 0;
        let stream = BlobStream::new(blob, read_only, "SQLite3");
        let mode: &[u8] = if writable { b"r+b" } else { b"rb" };
        Ok(Untyped(
            req.new_stream(Box::new(stream), mode)?
                .into_resource()
                .into_value(),
        ))
    }

    /// `SQLite3::enableExceptions()`: the setting it replaced; warnings
    /// asked for are deprecated.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->enableExceptions(true), $db->enableExceptions());
    /// ```
    /// ```expect
    /// Deprecated: SQLite3::enableExceptions(): Use of warnings for SQLite3 is deprecated in %s on line 3
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn enableExceptions(
        req: &mut Request,
        this: This<Self>,
        #[php(default = false)] enable: bool,
    ) -> Answer<bool> {
        let object = this.object();
        let was = conn(req, object)?.exceptions;
        if !enable {
            docref(
                req,
                Level::Deprecated,
                "Use of warnings for SQLite3 is deprecated",
            );
        }
        let conn = conn(req, object)?;
        conn.exceptions = enable;
        if let Link::Open(open) = &conn.link {
            open.db().serve_with(req, serving(enable));
        }
        Ok(was)
    }

    /// `SQLite3::enableExtendedResultCodes()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec('CREATE TABLE t (a UNIQUE); INSERT INTO t VALUES (1)');
    /// var_dump($db->enableExtendedResultCodes());
    /// @$db->exec('INSERT INTO t VALUES (1)');
    /// var_dump($db->lastErrorCode());
    /// ```
    /// ```expect
    /// bool(true)
    /// int(2067)
    /// ```
    #[php(tentative)]
    fn enableExtendedResultCodes(
        req: &mut Request,
        this: This<Self>,
        #[php(default = true)] enable: bool,
    ) -> Answer<bool> {
        let Some(db) = ever_opened(req, this.object())? else {
            return Ok(false);
        };
        db.extended_codes(req, i64::from(enable));
        Ok(true)
    }

    /// `SQLite3::setAuthorizer()`: the callback asked of every action a
    /// statement prepares, or none.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec('CREATE TABLE t (a)');
    /// $db->setAuthorizer(fn ($action, $a) => $a === 't' ? SQLite3::DENY : SQLite3::OK);
    /// var_dump($db->querySingle('SELECT * FROM t'));
    /// $db->setAuthorizer(null);
    /// var_dump($db->querySingle('SELECT COUNT(*) FROM t'));
    /// ```
    /// ```expect
    /// Warning: SQLite3::querySingle(): Unable to prepare statement: access to t.a is prohibited in %s on line 5
    /// bool(false)
    /// int(0)
    /// ```
    #[php(tentative)]
    fn setAuthorizer(
        req: &mut Request,
        this: This<Self>,
        callback: Option<Callable>,
    ) -> Answer<bool> {
        let object = this.object();
        let (db, _) = open_db(req, object)?;
        let handler = callback.map(|callable| Owner::take(req, callable));
        db.set_authorizer(req, handler.as_deref());
        let old = std::mem::replace(&mut conn(req, object)?.authorizer, handler.map(Box::new));
        if let Some(old) = old {
            old.give_up(req);
        }
        Ok(true)
    }
}
