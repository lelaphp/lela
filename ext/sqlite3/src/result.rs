//! `SQLite3Result`: `php_sqlite3_result` as [`ResultObj`] and the
//! methods of `class SQLite3Result` in `sqlite3.stub.php`'s order.

use php_ext::abi::{ObjectHandlers, OrFalse, Refusals, This, True};
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject};
use php_ext_pdo_sqlite::ffi::{SQLITE_OK, Stmt};

use crate::conn::{Taken, column_value, fail, give_back, not_ready, remove_stmt, take};
use crate::stmt::{self, Step, last_error, place};

/// Where a `SQLite3Result`'s statement came from, `is_prepared_statement`: a
/// `query()` finalizes its own statement, an `execute()` resets the
/// caller's.
#[derive(Default, Clone, Copy)]
pub(crate) enum Origin {
    #[default]
    Query,
    Execute,
}

/// `php_sqlite3_result` less its `zend_object`: the `SQLite3Stmt` and
/// the column count and names php caches between
/// fetches until a reset.
#[derive(Default)]
pub(crate) struct ResultObj {
    stmt: Option<Owner<PhpObject>>,
    origin: Origin,
    columns: Option<usize>,
    names: Option<Vec<Vec<u8>>>,
}

/// `SQLITE3_ASSOC` and `SQLITE3_NUM`, the bits a fetch is asked with.
const ASSOC: i64 = 1;
const NUM: i64 = 2;

/// The statement object recorded, its owner kept.
pub(crate) fn attach(req: &mut Request, object: PhpObject, stmt: Owner<PhpObject>, origin: Origin) {
    match object.native_mut::<Box<ResultObj>>(req) {
        Some(r) => {
            r.stmt = Some(stmt);
            r.origin = origin;
        }
        None => stmt.give_up(req),
    }
}

/// The statement taken for a call on a `SQLite3Result`, php's
/// `SQLITE3_CHECK_INITIALIZED(.., SQLite3Result)`.
fn held(req: &mut Request, object: PhpObject) -> Result<Taken, Thrown> {
    let at = object
        .native::<Box<ResultObj>>(req)
        .and_then(|r| r.stmt.as_deref().copied())
        .and_then(|stmt| place(req, stmt));
    match at.and_then(|(db, id)| take(req, db, id)) {
        Some(taken) => Ok(taken),
        None => Err(not_ready(req, "SQLite3Result")),
    }
}

fn result(req: &mut Request, object: PhpObject) -> Option<&mut ResultObj> {
    object.native_mut::<Box<ResultObj>>(req).map(|b| &mut **b)
}

/// `sqlite3result_clear_column_names_cache`.
fn clear_cache(req: &mut Request, object: PhpObject) {
    if let Some(r) = result(req, object) {
        r.columns = None;
        r.names = None;
    }
}

/// The column names, cached on the object where the fetch asks for them.
fn cache_names(req: &mut Request, object: PhpObject, stmt: &Stmt, count: usize, mode: i64) {
    if mode & ASSOC == 0 {
        return;
    }
    if let Some(r) = result(req, object)
        && r.names.is_none()
    {
        let names = (0..count)
            .map(|column| stmt.column_name(column).unwrap_or_default())
            .collect();
        r.names = Some(names);
    }
}

/// `php_sqlite3_fetch_one`: the row by number, by name or both, a later
/// column of one name over an earlier.
fn fetch_one(
    req: &mut Request,
    object: PhpObject,
    stmt: &Stmt,
    count: usize,
    mode: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    let names = result(req, object)
        .and_then(|r| r.names.clone())
        .unwrap_or_default();
    let mut row = PhpArray::new(req)?;
    for column in 0..count {
        let value = column_value(req, stmt, column)?;
        if mode & NUM != 0 {
            let at = i64::try_from(column).unwrap_or(i64::MAX);
            let at = Owner::take(req, php_ext::values::Key::Int(at));
            let numbered = Owner::take(req, *value);
            row.set(req, at, numbered)?;
        }
        if mode & ASSOC != 0 {
            let name = names.get(column).cloned().unwrap_or_default();
            row.set_symtable_str(req, &name, value)?;
        } else {
            value.give_up(req);
        }
    }
    Ok(row)
}

/// `class SQLite3Result`, `@not-serializable`, uncloneable, made only by
/// `query()` and `execute()`.
///
/// ```php
/// try { new SQLite3Result; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $r = (new SQLite3(':memory:'))->query('SELECT 1');
/// try { clone $r; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Call to private SQLite3Result::__construct() from global scope
/// Trying to clone an uncloneable object of class SQLite3Result
/// ```
#[php_class(not_serializable, size = 88, payload = Box<ResultObj>, handlers = HANDLERS)]
pub(crate) struct SQLite3Result;

/// `sqlite3_result_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `php_sqlite3_result_object_free_storage`: the statement reset
    /// where it is still in its list, and let go.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(r) = object.native_mut::<Box<ResultObj>>(req).map(std::mem::take) else {
            return;
        };
        let Some(stmt_object) = r.stmt else {
            return;
        };
        if let Some(taken) = place(req, *stmt_object).and_then(|(db, id)| take(req, db, id)) {
            if let Some(stmt) = taken.stmt.as_ref() {
                let _ = stmt.reset(req);
            }
            give_back(req, taken);
        }
        stmt_object.give_up(req);
    }
}

type Answer<T> = Result<T, Thrown>;

#[php_class]
impl SQLite3Result {
    /// `SQLite3Result::__construct()`, private, and refusing where it
    /// is reached anyway.
    ///
    /// ```php
    /// $o = (new ReflectionClass('SQLite3Result'))->newInstanceWithoutConstructor();
    /// try { (new ReflectionMethod('SQLite3Result', '__construct'))->invoke($o); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// SQLite3Result cannot be directly instantiated
    /// ```
    #[php(private)]
    fn __construct(req: &mut Request, this: This<Self>) -> Answer<()> {
        let _ = this;
        Err(throw(
            req,
            "Exception",
            "SQLite3Result cannot be directly instantiated",
        ))
    }

    /// `SQLite3Result::numColumns()`.
    ///
    /// ```php
    /// var_dump((new SQLite3(':memory:'))->query('SELECT 1, 2')->numColumns());
    /// ```
    /// ```expect
    /// int(2)
    /// ```
    #[php(tentative)]
    fn numColumns(req: &mut Request, this: This<Self>) -> Answer<i64> {
        let taken = held(req, this.object())?;
        let count = taken.stmt.as_ref().map_or(0, Stmt::column_count);
        give_back(req, taken);
        Ok(i64::try_from(count).unwrap_or(i64::MAX))
    }

    /// `SQLite3Result::columnName()`.
    ///
    /// ```php
    /// $r = (new SQLite3(':memory:'))->query('SELECT 1 AS a');
    /// var_dump($r->columnName(0), $r->columnName(1), $r->columnName(-1));
    /// ```
    /// ```expect
    /// string(1) "a"
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn columnName(
        req: &mut Request,
        this: This<Self>,
        column: i64,
    ) -> Answer<OrFalse<Owner<php_ext::values::PhpString>>> {
        let taken = held(req, this.object())?;
        let name = match (taken.stmt.as_ref(), usize::try_from(column)) {
            (Some(stmt), Ok(column)) => stmt.column_name(column),
            _ => None,
        };
        give_back(req, taken);
        match name {
            Some(name) => Ok(OrFalse::Value(php_ext::values::PhpString::new(req, &name)?)),
            None => Ok(OrFalse::False),
        }
    }

    /// `SQLite3Result::columnType()`: `false` before a row is fetched.
    ///
    /// ```php
    /// $r = (new SQLite3(':memory:'))->query("SELECT 1, 'a', NULL");
    /// var_dump($r->columnType(0));
    /// $r->fetchArray();
    /// var_dump($r->columnType(0), $r->columnType(1), $r->columnType(2));
    /// ```
    /// ```expect
    /// bool(false)
    /// int(1)
    /// int(3)
    /// int(5)
    /// ```
    #[php(tentative)]
    fn columnType(req: &mut Request, this: This<Self>, column: i64) -> Answer<OrFalse<i64>> {
        let taken = held(req, this.object())?;
        let ty = match taken.stmt.as_ref() {
            Some(stmt) if stmt.data_count() > 0 => Some(match usize::try_from(column) {
                Ok(column) => i64::from(stmt.column_type(column)),
                Err(_) => 5,
            }),
            _ => None,
        };
        give_back(req, taken);
        Ok(ty.map_or(OrFalse::False, OrFalse::Value))
    }

    /// `SQLite3Result::fetchArray()`: the next row, `false` past the
    /// last; the row is not built where the return is not used.
    ///
    /// ```php
    /// $r = (new SQLite3(':memory:'))->query("SELECT 1 AS a, 'x' AS a");
    /// var_dump($r->fetchArray(), $r->fetchArray());
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   int(1)
    ///   ["a"]=>
    ///   string(1) "x"
    ///   [1]=>
    ///   string(1) "x"
    /// }
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn fetchArray(
        req: &mut Request,
        this: This<Self>,
        #[php(default = SQLITE3_BOTH)] mode: i64,
    ) -> Answer<OrFalse<Owner<PhpArray>>> {
        let object = this.object();
        let taken = held(req, object)?;
        let fetched = fetch_array(req, object, &taken, mode);
        give_back(req, taken);
        fetched
    }

    /// `SQLite3Result::fetchAll()`: every row left.
    ///
    /// ```php
    /// $r = (new SQLite3(':memory:'))->query('SELECT 1 AS a UNION SELECT 2');
    /// var_dump($r->fetchAll(SQLITE3_ASSOC));
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   array(1) {
    ///     ["a"]=>
    ///     int(1)
    ///   }
    ///   [1]=>
    ///   array(1) {
    ///     ["a"]=>
    ///     int(2)
    ///   }
    /// }
    /// ```
    fn fetchAll(
        req: &mut Request,
        this: This<Self>,
        #[php(default = SQLITE3_BOTH)] mode: i64,
    ) -> Answer<OrFalse<Owner<PhpArray>>> {
        let object = this.object();
        let taken = held(req, object)?;
        let fetched = fetch_all(req, object, &taken, mode);
        give_back(req, taken);
        fetched
    }

    /// `SQLite3Result::reset()`: back to the first row.
    ///
    /// ```php
    /// $r = (new SQLite3(':memory:'))->query('SELECT 1');
    /// $r->fetchArray();
    /// var_dump($r->reset(), $r->fetchArray(SQLITE3_NUM)[0]);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn reset(req: &mut Request, this: This<Self>) -> Answer<bool> {
        let object = this.object();
        let taken = held(req, object)?;
        clear_cache(req, object);
        let (rc, thrown) = match taken.stmt.as_ref() {
            Some(stmt) => stmt.reset(req),
            None => (SQLITE_OK, None),
        };
        give_back(req, taken);
        match thrown {
            Some(thrown) => Err(thrown),
            None => Ok(rc == SQLITE_OK),
        }
    }

    /// `SQLite3Result::finalize()`: a `query()`'s statement finalized, an
    /// `execute()`'s reset.
    ///
    /// ```php
    /// $db = new SQLite3(':memory:');
    /// $r = $db->query('SELECT 1');
    /// var_dump($r->finalize());
    /// try { $r->fetchArray(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// The SQLite3Result object has not been correctly initialised or is already closed
    /// ```
    #[php(tentative)]
    fn finalize(req: &mut Request, this: This<Self>) -> Answer<True> {
        let object = this.object();
        let taken = held(req, object)?;
        clear_cache(req, object);
        let origin = result(req, object).map(|r| r.origin).unwrap_or_default();
        let thrown = match (origin, taken.stmt.as_ref()) {
            (Origin::Execute, Some(stmt)) => stmt.reset(req).1,
            _ => None,
        };
        let (owner, id) = (taken.owner, taken.id());
        give_back(req, taken);
        if matches!(origin, Origin::Query) {
            remove_stmt(req, owner, id);
        }
        thrown.map_or(Ok(True), Err)
    }
}

/// `fetchArray()` over the statement taken for the call.
fn fetch_array(
    req: &mut Request,
    object: PhpObject,
    taken: &Taken,
    mode: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let (step, thrown) = stmt::step(req, taken.stmt.as_ref());
    match (step, taken.stmt.as_ref()) {
        (Step::Row, Some(stmt)) => {
            if let Some(thrown) = thrown {
                return Err(thrown);
            }
            if !req.frames().return_value_used() {
                return Ok(OrFalse::False);
            }
            let count = result(req, object).map_or_else(
                || stmt.column_count(),
                |r| *r.columns.get_or_insert_with(|| stmt.column_count()),
            );
            cache_names(req, object, stmt, count, mode);
            Ok(OrFalse::Value(fetch_one(req, object, stmt, count, mode)?))
        }
        (Step::Done, _) => thrown.map_or(Ok(OrFalse::False), Err),
        _ => {
            let (code, message) = last_error(taken.stmt.as_ref());
            let message = format!("Unable to execute statement: {message}");
            fail(req, taken.exceptions, thrown, code, &message)?;
            Ok(OrFalse::False)
        }
    }
}

/// `fetchAll()` over the statement taken for the call.
fn fetch_all(
    req: &mut Request,
    object: PhpObject,
    taken: &Taken,
    mode: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let count = taken.stmt.as_ref().map_or(0, Stmt::column_count);
    if let Some(stmt) = taken.stmt.as_ref() {
        cache_names(req, object, stmt, count, mode);
    }
    if let Some(r) = result(req, object) {
        r.columns = Some(count);
    }
    let mut rows = PhpArray::new(req)?;
    loop {
        let (step, thrown) = stmt::step(req, taken.stmt.as_ref());
        match (step, taken.stmt.as_ref()) {
            (Step::Row, Some(stmt)) => {
                let row = fetch_one(req, object, stmt, count, mode)?;
                rows.append(req, row.into_value())?;
                if let Some(thrown) = thrown {
                    rows.give_up(req);
                    return Err(thrown);
                }
            }
            (Step::Done, _) => {
                if let Some(thrown) = thrown {
                    rows.give_up(req);
                    return Err(thrown);
                }
                return Ok(OrFalse::Value(rows));
            }
            _ => {
                rows.give_up(req);
                if let Some(thrown) = thrown {
                    return Err(thrown);
                }
                let (code, message) = last_error(taken.stmt.as_ref());
                let message = format!("Unable to execute statement: {message}");
                fail(req, taken.exceptions, None, code, &message)?;
                return Ok(OrFalse::False);
            }
        }
    }
}
