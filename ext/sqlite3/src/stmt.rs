//! `SQLite3Stmt`: `php_sqlite3_stmt` as [`StmtObj`], its bound
//! parameters as a typed table ([`Bound`]), the step's code decoded once
//! ([`Step`]), and the methods of `class SQLite3Stmt` in
//! `sqlite3.stub.php`'s order.

use php_ext::abi::{Instance, ObjectHandlers, OrFalse, Ref, Refusals, This, True, Union};
use php_ext::attr::php_class;
use php_ext::coerce::{to_double, to_long, with_string};
use php_ext::raising::{Thrown, argument_error};
use php_ext::request::{Request, Stream};
use php_ext::values::{Owner, PhpObject, PhpString, Value, ValueRef};
use php_ext_pdo_sqlite::ffi::{self, SQLITE_OK, Stmt};

use crate::conn::{
    SQLite3, StmtId, Taken, add_stmt, fail, give_back, instantiate, not_ready, open_db,
    remove_stmt, take, text,
};
use crate::result::{self, Origin, SQLite3Result};

/// `php_sqlite3_stmt` less its `zend_object`: the `SQLite3` it was
/// prepared on, an owner of it, and its place in that connection's
/// list, `None` before a prepare succeeded.
#[derive(Default)]
pub(crate) struct StmtObj {
    db: Option<Owner<PhpObject>>,
    id: Option<StmtId>,
    params: Vec<Bound>,
}

/// What `sqlite3_step` answered, decoded once.
pub(crate) enum Step {
    Row,
    Done,
    /// `SQLITE_ERROR`, after which `execute()` resets the statement.
    Error,
    Failed,
}

/// `sqlite3_step` on the statement, `SQLITE_MISUSE` for a text that
/// held none, as the library answers a null statement.
pub(crate) fn step(req: &mut Request, stmt: Option<&Stmt>) -> (Step, Option<Thrown>) {
    let Some(stmt) = stmt else {
        return (Step::Failed, None);
    };
    let (rc, thrown) = stmt.step(req);
    let step = match rc {
        ffi::SQLITE_ROW => Step::Row,
        ffi::SQLITE_DONE => Step::Done,
        ffi::SQLITE_ERROR => Step::Error,
        _ => Step::Failed,
    };
    (step, thrown)
}

/// `sqlite3_errcode(sqlite3_db_handle(stmt))` and its message: what the
/// library answers for a null statement where there is none.
pub(crate) fn last_error(stmt: Option<&Stmt>) -> (i64, String) {
    match stmt {
        Some(stmt) => (i64::from(stmt.errcode()), text(&stmt.errmsg())),
        None => (21, String::from("out of memory")),
    }
}

/// A bound parameter's type, `SQLITE3_*` decoded once.
#[derive(Clone, Copy)]
enum Kind {
    Integer,
    Float,
    Text,
    Blob,
    Null,
    /// A type the library has not, refused when the statement binds.
    Unknown(i64),
}

impl Kind {
    fn of(ty: i64) -> Kind {
        match ty {
            1 => Kind::Integer,
            2 => Kind::Float,
            3 => Kind::Text,
            4 => Kind::Blob,
            5 => Kind::Null,
            other => Kind::Unknown(other),
        }
    }

    /// `PHP_SQLITE3_SET_TYPE`: the type a value is bound as where the
    /// call named none.
    fn guess(value: &Value) -> Kind {
        match value.as_ref() {
            ValueRef::Int(_) | ValueRef::Bool(_) => Kind::Integer,
            ValueRef::Float(_) => Kind::Float,
            ValueRef::Null => Kind::Null,
            _ => Kind::Text,
        }
    }
}

/// How a parameter is filed in `bound_params`: by its name, `:` first,
/// or by its number.
#[derive(PartialEq)]
enum Key {
    Name(Vec<u8>),
    Number(i64),
}

/// `php_sqlite3_bound_param`: the value, a reference where `bindParam`
/// bound one.
pub(crate) struct Bound {
    key: Key,
    number: i64,
    kind: Kind,
    value: Owner<Value>,
}

/// `SQLITE3_CHECK_INITIALIZED` and, where `need` asks,
/// `SQLITE3_CHECK_INITIALIZED_STMT`: the statement out of its list for
/// the call.
fn held(req: &mut Request, object: PhpObject, need: bool) -> Result<Taken, Thrown> {
    let (db, id) = match object.native::<Box<StmtObj>>(req) {
        Some(s) => (s.db.as_deref().copied(), s.id),
        None => (None, None),
    };
    let taken = match (db, id) {
        (Some(db), Some(id)) => take(req, db, id),
        _ => None,
    };
    let Some(taken) = taken else {
        return Err(not_ready(req, "SQLite3"));
    };
    if need && taken.stmt.is_none() {
        give_back(req, taken);
        return Err(not_ready(req, "SQLite3Stmt"));
    }
    Ok(taken)
}

/// `f` over the statement taken for the call, given back after.
fn with<R>(
    req: &mut Request,
    object: PhpObject,
    need: bool,
    f: impl FnOnce(&mut Request, &Taken) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let taken = held(req, object, need)?;
    let answer = f(req, &taken);
    give_back(req, taken);
    answer
}

/// The statement's `SQLite3`, an owner the caller took, recorded; the
/// one a constructor served again replaces is given up.
pub(crate) fn attach(req: &mut Request, object: PhpObject, db: Owner<PhpObject>) {
    let old = match object.native_mut::<Box<StmtObj>>(req) {
        Some(s) => s.db.replace(db),
        None => Some(db),
    };
    if let Some(old) = old {
        old.give_up(req);
    }
}

/// The statement's place in its connection's list, recorded.
pub(crate) fn set_id(req: &mut Request, object: PhpObject, id: StmtId) {
    if let Some(s) = object.native_mut::<Box<StmtObj>>(req) {
        s.id = Some(id);
    }
}

/// The `SQLite3` and the place of a `SQLite3Stmt`, where it was prepared.
pub(crate) fn place(req: &Request, object: PhpObject) -> Option<(PhpObject, StmtId)> {
    let s = object.native::<Box<StmtObj>>(req)?;
    Some((s.db.as_deref().copied()?, s.id?))
}

/// `prepare()` and the constructor: the text prepared on `owner`'s
/// connection and its statement in `existing`, or a new `SQLite3Stmt`;
/// php's warning and `None` where the library refuses it.
pub(crate) fn prepare_on(
    req: &mut Request,
    owner: PhpObject,
    existing: Option<PhpObject>,
    sql: &[u8],
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let (db, exceptions) = open_db(req, owner)?;
    let made = match existing {
        Some(object) => Owner::take(req, object),
        None => instantiate(req, b"SQLite3Stmt")?,
    };
    let object = *made;
    let held = Owner::take(req, owner);
    attach(req, object, held);
    let (prepared, thrown) = db.prepare(req, sql);
    let stmt = match prepared {
        Ok(stmt) => stmt,
        Err(code) => {
            let message = format!("Unable to prepare statement: {}", text(&db.errmsg(req)));
            made.give_up(req);
            fail(req, exceptions, thrown, i64::from(code), &message)?;
            return Ok(None);
        }
    };
    if let Some(id) = add_stmt(req, owner, stmt) {
        set_id(req, object, id);
    }
    if let Some(thrown) = thrown {
        made.give_up(req);
        return Err(thrown);
    }
    Ok(Some(made))
}

/// `php_sqlite3_bind_params`: each parameter bound by its type, `null`
/// as `NULL` whatever the type; `false` for php's `FAILURE`.
fn bind_params(req: &mut Request, taken: &Taken, params: &mut [Bound]) -> Result<bool, Thrown> {
    let Some(stmt) = taken.stmt.as_ref() else {
        return Ok(true);
    };
    let exceptions = taken.exceptions;
    for param in params {
        let at = param.number;
        let value = param.value.deref();
        let rc = if matches!(value.as_ref(), ValueRef::Null) {
            stmt.bind_null(at)
        } else {
            match param.kind {
                Kind::Integer => {
                    let n = to_long(req, &value)?;
                    param.value.convert(req, Owner::int(n));
                    stmt.bind_int(at, n)
                }
                Kind::Float => {
                    let f = to_double(req, &value)?;
                    param.value.convert(req, Owner::float(f));
                    stmt.bind_double(at, f)
                }
                Kind::Blob if matches!(value.as_ref(), ValueRef::Resource(_)) => {
                    let Some(stream) = Stream::from_value(req, &value) else {
                        let message = format!("Unable to read stream for parameter {at}");
                        fail(req, exceptions, None, 0, &message)?;
                        return Ok(false);
                    };
                    let text = stream.copy_to_mem(req, None)?;
                    let rc = stmt.bind_blob(at, text.as_bytes());
                    text.give_up(req);
                    rc
                }
                Kind::Blob => with_string(req, &value, |text| stmt.bind_blob(at, text))?,
                Kind::Text => with_string(req, &value, |text| stmt.bind_text(at, text))?,
                Kind::Null => stmt.bind_null(at),
                Kind::Unknown(ty) => {
                    let message = format!("Unknown parameter type: {ty} for parameter {at}");
                    fail(req, exceptions, None, 0, &message)?;
                    return Ok(false);
                }
            }
        };
        if rc != SQLITE_OK {
            let message = format!("Unable to bind parameter number {at}");
            fail(req, exceptions, None, i64::from(rc), &message)?;
        }
    }
    Ok(true)
}

/// The parameters bound on the statement taken for the call, taken off
/// the object for the length of the binding and laid back after.
fn bind_all(req: &mut Request, object: PhpObject, taken: &Taken) -> Result<bool, Thrown> {
    let mut params = object
        .native_mut::<Box<StmtObj>>(req)
        .map(|s| std::mem::take(&mut s.params))
        .unwrap_or_default();
    let bound = bind_params(req, taken, &mut params);
    match object.native_mut::<Box<StmtObj>>(req) {
        Some(s) => {
            let later = std::mem::replace(&mut s.params, params);
            release_params(req, later);
        }
        None => release_params(req, params),
    }
    bound
}

fn release_params(req: &mut Request, params: Vec<Bound>) {
    for param in params {
        param.value.give_up(req);
    }
}

/// `sqlite3stmt_bind`: the parameter filed by its name or number, the
/// number looked up for a name; `false` where the statement has none.
fn bind(
    req: &mut Request,
    object: PhpObject,
    param: Union<PhpString, i64>,
    value: Value,
    kind: Kind,
) -> Result<bool, Thrown> {
    let number = with(req, object, true, |_, taken| {
        Ok(match (&param, taken.stmt.as_ref()) {
            (Union::A(name), Some(stmt)) => {
                let mut full = name.as_bytes().to_vec();
                if !matches!(full.first(), Some(b':' | b'@')) {
                    full.insert(0, b':');
                }
                (Key::Name(full.clone()), stmt.parameter_index(&full))
            }
            (Union::B(n), _) => (Key::Number(*n), *n),
            (Union::A(_), None) => (Key::Number(0), 0),
        })
    })?;
    let (key, number) = number;
    if number < 1 {
        return Ok(false);
    }
    let value = Owner::take(req, value);
    let Some(s) = object.native_mut::<Box<StmtObj>>(req) else {
        value.give_up(req);
        return Ok(false);
    };
    let mut gone = Vec::new();
    if let Some(at) = s.params.iter().position(|p| p.key == Key::Number(number)) {
        gone.push(s.params.remove(at).value);
    }
    let bound = Bound {
        key,
        number,
        kind,
        value,
    };
    match s.params.iter_mut().find(|p| p.key == bound.key) {
        Some(slot) => gone.push(std::mem::replace(slot, bound).value),
        None => s.params.push(bound),
    }
    for value in gone {
        value.give_up(req);
    }
    Ok(true)
}

/// `class SQLite3Stmt`, `@not-serializable`, uncloneable.
///
/// ```php
/// $db = new SQLite3(':memory:');
/// $s = $db->prepare('SELECT 1');
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new SQLite3Stmt($db, 'SELECT 1'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Trying to clone an uncloneable object of class SQLite3Stmt
/// Call to private SQLite3Stmt::__construct() from global scope
/// ```
#[php_class(not_serializable, size = 88, payload = Box<StmtObj>, handlers = HANDLERS)]
pub(crate) struct SQLite3Stmt;

/// `sqlite3_stmt_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `php_sqlite3_stmt_object_free_storage`: the parameters given up,
    /// the statement out of its list and finalized, the `SQLite3` let go.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(s) = object.native_mut::<Box<StmtObj>>(req).map(std::mem::take) else {
            return;
        };
        release_params(req, s.params);
        if let (Some(db), Some(id)) = (s.db.as_deref().copied(), s.id) {
            remove_stmt(req, db, id);
        }
        if let Some(db) = s.db {
            db.give_up(req);
        }
    }
}

type Answer<T> = Result<T, Thrown>;

#[php_class]
impl SQLite3Stmt {
    const EXPLAIN_MODE_PREPARED: i64 = 0;
    const EXPLAIN_MODE_EXPLAIN: i64 = 1;
    const EXPLAIN_MODE_EXPLAIN_QUERY_PLAN: i64 = 2;

    /// `SQLite3Stmt::__construct()`, private: `SQLite3::prepare()`'s body
    /// on this object.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = (new ReflectionClass('SQLite3Stmt'))->newInstanceWithoutConstructor();
    /// (new ReflectionMethod('SQLite3Stmt', '__construct'))->invoke($s, $db, 'SELECT 7');
    /// var_dump($s->execute()->fetchArray(SQLITE3_NUM)[0]);
    /// ```
    /// ```expect
    /// int(7)
    /// ```
    #[php(private)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        sqlite3: Instance<SQLite3>,
        query: PhpString,
    ) -> Answer<()> {
        open_db(req, sqlite3.object())?;
        if query.as_bytes().is_empty() {
            return Ok(());
        }
        let sql = query.as_bytes().to_vec();
        if let Some(made) = prepare_on(req, sqlite3.object(), Some(this.object()), &sql)? {
            made.give_up(req);
        }
        Ok(())
    }

    /// `SQLite3Stmt::bindParam()`: the variable bound by reference, read
    /// when the statement executes; the type is text unless named.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT :a, typeof(:a)');
    /// $a = 1;
    /// var_dump($s->bindParam(':a', $a), $s->bindParam(':nope', $a));
    /// $a = 5;
    /// var_dump($s->execute()->fetchArray(SQLITE3_NUM));
    /// $s->bindParam('a', $a, SQLITE3_INTEGER);
    /// var_dump($s->execute()->fetchArray(SQLITE3_NUM)[1]);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// array(2) {
    ///   [0]=>
    ///   string(1) "5"
    ///   [1]=>
    ///   string(4) "text"
    /// }
    /// string(7) "integer"
    /// ```
    #[php(tentative)]
    fn bindParam(
        req: &mut Request,
        this: This<Self>,
        param: Union<PhpString, i64>,
        var: Ref<Value>,
        #[php(default = SQLITE3_TEXT)] r#type: i64,
    ) -> Answer<bool> {
        let value = match var.reference() {
            Some(cell) => Value::reference(cell),
            None => var.get().copied().unwrap_or_default(),
        };
        let kind = if req.frames().argument_count() < 3 {
            Kind::guess(&value)
        } else {
            Kind::of(r#type)
        };
        bind(req, this.object(), param, value, kind)
    }

    /// `SQLite3Stmt::bindValue()`: the value copied now, its type guessed
    /// from it unless named.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT typeof(?1), typeof(?2), typeof(?3), ?4');
    /// $s->bindValue(1, 5); $s->bindValue(2, 1.5); $s->bindValue(3, null); $s->bindValue(4, '7', SQLITE3_INTEGER);
    /// var_dump($s->execute()->fetchArray(SQLITE3_NUM));
    /// ```
    /// ```expect
    /// array(4) {
    ///   [0]=>
    ///   string(7) "integer"
    ///   [1]=>
    ///   string(4) "real"
    ///   [2]=>
    ///   string(4) "null"
    ///   [3]=>
    ///   int(7)
    /// }
    /// ```
    #[php(tentative)]
    fn bindValue(
        req: &mut Request,
        this: This<Self>,
        param: Union<PhpString, i64>,
        value: Value,
        #[php(default = SQLITE3_TEXT)] r#type: i64,
    ) -> Answer<bool> {
        let kind = if req.frames().argument_count() < 3 {
            Kind::guess(&value)
        } else {
            Kind::of(r#type)
        };
        bind(req, this.object(), param, value, kind)
    }

    /// `SQLite3Stmt::clear()`: every binding taken off.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT ?');
    /// $s->bindValue(1, 'x');
    /// var_dump($s->clear(), $s->execute()->fetchArray(SQLITE3_NUM)[0]);
    /// ```
    /// ```expect
    /// bool(true)
    /// NULL
    /// ```
    #[php(tentative)]
    fn clear(req: &mut Request, this: This<Self>) -> Answer<bool> {
        let object = this.object();
        let cleared = with(req, object, true, |req, taken| {
            let Some(stmt) = taken.stmt.as_ref() else {
                return Ok(true);
            };
            if stmt.clear_bindings() == SQLITE_OK {
                return Ok(true);
            }
            let (code, message) = last_error(Some(stmt));
            fail(
                req,
                taken.exceptions,
                None,
                code,
                &format!("Unable to clear statement: {message}"),
            )?;
            Ok(false)
        })?;
        if cleared {
            let params = object
                .native_mut::<Box<StmtObj>>(req)
                .map(|s| std::mem::take(&mut s.params))
                .unwrap_or_default();
            release_params(req, params);
        }
        Ok(cleared)
    }

    /// `SQLite3Stmt::close()`: the statement finalized.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT 1');
    /// var_dump($s->close());
    /// try { $s->close(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// The SQLite3 object has not been correctly initialised or is already closed
    /// ```
    #[php(tentative)]
    fn close(req: &mut Request, this: This<Self>) -> Answer<True> {
        match place(req, this.object()) {
            Some((db, id)) if remove_stmt(req, db, id) => Ok(True),
            _ => Err(not_ready(req, "SQLite3")),
        }
    }

    /// `SQLite3Stmt::execute()`: reset, bound, stepped once and reset,
    /// and a `SQLite3Result` over it.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec('CREATE TABLE t (a UNIQUE)');
    /// $s = $db->prepare('INSERT INTO t VALUES (?)');
    /// $s->bindValue(1, 1);
    /// var_dump(get_class($s->execute()), $s->execute());
    /// ```
    /// ```expect
    /// Warning: SQLite3Stmt::execute(): Unable to execute statement: UNIQUE constraint failed: t.a in %s on line 6
    /// string(13) "SQLite3Result"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn execute(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<OrFalse<Owner<Instance<SQLite3Result>>>> {
        let object = this.object();
        let taken = held(req, object, false)?;
        let pending = taken.stmt.as_ref().and_then(|stmt| stmt.reset(req).1);
        let bound = bind_all(req, object, &taken);
        let bound = match (bound, pending) {
            (Err(thrown), _) | (Ok(_), Some(thrown)) => {
                give_back(req, taken);
                return Err(thrown);
            }
            (Ok(bound), None) => bound,
        };
        if !bound {
            give_back(req, taken);
            return Ok(OrFalse::False);
        }
        let (step, thrown) = step(req, taken.stmt.as_ref());
        if matches!(step, Step::Row | Step::Done | Step::Error)
            && let Some(stmt) = taken.stmt.as_ref()
        {
            let _ = stmt.reset(req);
        }
        if matches!(step, Step::Row | Step::Done) {
            give_back(req, taken);
            let result = instantiate(req, b"SQLite3Result")?;
            let held = Owner::take(req, object);
            result::attach(req, *result, held, Origin::Execute);
            if let Some(thrown) = thrown {
                result.give_up(req);
                return Err(thrown);
            }
            return Ok(OrFalse::Value(result.into_instance()));
        }
        let failed = if let Some(thrown) = thrown {
            Err(thrown)
        } else {
            let (code, message) = last_error(taken.stmt.as_ref());
            let message = format!("Unable to execute statement: {message}");
            fail(req, taken.exceptions, None, code, &message)
        };
        give_back(req, taken);
        failed.map(|()| OrFalse::False)
    }

    /// `SQLite3Stmt::getSQL()`: the text, or with the bound values in
    /// place.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT :a');
    /// $s->bindValue(':a', "it's");
    /// var_dump($s->getSQL(), $s->getSQL(true));
    /// ```
    /// ```expect
    /// string(9) "SELECT :a"
    /// string(14) "SELECT 'it''s'"
    /// ```
    #[php(tentative)]
    fn getSQL(
        req: &mut Request,
        this: This<Self>,
        #[php(default = false)] expand: bool,
    ) -> Answer<OrFalse<Owner<PhpString>>> {
        let object = this.object();
        let taken = held(req, object, true)?;
        let bound = bind_all(req, object, &taken);
        let text = match (bound, taken.stmt.as_ref()) {
            (Ok(true), Some(stmt)) if expand => Ok(Some(stmt.expanded_sql())),
            (Ok(true), Some(stmt)) => Ok(Some(stmt.sql())),
            (Ok(_), _) => Ok(None),
            (Err(thrown), _) => Err(thrown),
        };
        give_back(req, taken);
        match text? {
            Some(text) => Ok(OrFalse::Value(PhpString::new(req, &text)?)),
            None => Ok(OrFalse::False),
        }
    }

    /// `SQLite3Stmt::paramCount()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// var_dump($db->prepare('SELECT ?, :a, ?')->paramCount());
    /// ```
    /// ```expect
    /// int(3)
    /// ```
    #[php(tentative)]
    fn paramCount(req: &mut Request, this: This<Self>) -> Answer<i64> {
        with(req, this.object(), true, |_, t| {
            Ok(t.stmt.as_ref().map_or(0, Stmt::parameter_count))
        })
    }

    /// `SQLite3Stmt::readOnly()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $db->exec('CREATE TABLE t (a)');
    /// var_dump($db->prepare('SELECT 1')->readOnly(), $db->prepare('DELETE FROM t')->readOnly());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn readOnly(req: &mut Request, this: This<Self>) -> Answer<bool> {
        with(req, this.object(), true, |_, t| {
            Ok(t.stmt.as_ref().is_some_and(Stmt::readonly))
        })
    }

    /// `SQLite3Stmt::reset()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT 1 UNION SELECT 2');
    /// $r = $s->execute();
    /// $r->fetchArray();
    /// var_dump($s->reset(), $r->fetchArray(SQLITE3_NUM)[0]);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn reset(req: &mut Request, this: This<Self>) -> Answer<bool> {
        with(req, this.object(), true, |req, taken| {
            let Some(stmt) = taken.stmt.as_ref() else {
                return Ok(true);
            };
            let (rc, thrown) = stmt.reset(req);
            if rc == SQLITE_OK {
                return thrown.map_or(Ok(true), Err);
            }
            let (code, message) = last_error(Some(stmt));
            let message = format!("Unable to reset statement: {message}");
            fail(req, taken.exceptions, thrown, code, &message)?;
            Ok(false)
        })
    }

    /// `SQLite3Stmt::busy()`: whether a step left a row unread.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT 1 UNION SELECT 2');
    /// $r = $s->execute();
    /// var_dump($s->busy());
    /// $r->fetchArray();
    /// var_dump($s->busy());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// ```
    fn busy(req: &mut Request, this: This<Self>) -> Answer<bool> {
        with(req, this.object(), true, |_, t| {
            Ok(t.stmt.as_ref().is_some_and(Stmt::busy))
        })
    }

    /// `SQLite3Stmt::explain()`.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $s = $db->prepare('SELECT 1');
    /// var_dump($s->explain(), $s->setExplain(SQLite3Stmt::EXPLAIN_MODE_EXPLAIN), $s->explain());
    /// ```
    /// ```expect
    /// int(0)
    /// bool(true)
    /// int(1)
    /// ```
    fn explain(req: &mut Request, this: This<Self>) -> Answer<i64> {
        with(req, this.object(), true, |_, t| {
            Ok(t.stmt.as_ref().map_or(0, Stmt::explain))
        })
    }

    /// `SQLite3Stmt::setExplain()`: the argument checked before the object.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// try { $db->prepare('SELECT 1')->setExplain(3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// SQLite3Stmt::setExplain(): Argument #1 ($mode) must be one of the SQLite3Stmt::EXPLAIN_MODE_* constants
    /// ```
    fn setExplain(req: &mut Request, this: This<Self>, mode: i64) -> Answer<bool> {
        if !(0..=2).contains(&mode) {
            let tail = "must be one of the SQLite3Stmt::EXPLAIN_MODE_* constants";
            return Err(argument_error(req, "ValueError", 1, tail));
        }
        with(req, this.object(), true, |_, t| {
            Ok(t.stmt
                .as_ref()
                .is_some_and(|stmt| stmt.set_explain(mode) == SQLITE_OK))
        })
    }
}
