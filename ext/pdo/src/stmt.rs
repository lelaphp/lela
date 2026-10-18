//! `pdo_stmt.c` less its fetch half ([`crate::fetch`]): the
//! `PDOStatement` class, its statement `pdo_stmt_t` as the object's
//! native payload ([`Stmt`]), the handlers `pdo_stmt_init` installs, the
//! bound parameters and columns with the connector's binding callback, and
//! the methods that start, bind and describe.

use std::ops::ControlFlow;

use php_ext::abi::{
    ClassIterator, GcTable, Instance, ObjectHandlers, OrFalse, Passed, Raw, Ref, Refusals, Std,
    This, True, Union, Variadic,
};
use php_ext::attr::php_class;
use php_ext::call::internal_iterator;
use php_ext::coerce::to_string;
use php_ext::raising::{Thrown, argument_error, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::bound::{Bound, BoundKey, BoundTable};
use crate::connector::{
    BindEvent, BindKind, BindType, Cause, Failure, ParameterMap, Placeholders, Prepared, SqlState,
    Statement,
};
use crate::dbh::{self, PDO};
use crate::error::{self, On};
use crate::fetch::{self, Target};

/// How the statement reaches its connection: `stmt->dbh` with the owner
/// `database_object_handle` takes, or, for the statement a failed
/// `query()` left on the connection, `stmt->dbh` alone, the connection holding
/// the statement and not the other way round.
#[derive(Default)]
pub(crate) enum DbLink {
    /// A statement no connection made, which `PHP_STMT_GET_OBJ` refuses.
    #[default]
    None,
    Owned(Owner<PhpObject>),
    Held(PhpObject),
}

impl DbLink {
    pub(crate) fn object(&self) -> Option<PhpObject> {
        match self {
            DbLink::None => None,
            DbLink::Owned(dbh) => Some(**dbh),
            DbLink::Held(dbh) => Some(*dbh),
        }
    }

    /// The statement's owner of its connection given up, where it holds
    /// one.
    pub(crate) fn release(self, req: &mut Request) {
        if let DbLink::Owned(dbh) = self {
            dbh.give_up(req);
        }
    }
}

/// `stmt->active_query_string`, each string one owner of a string of
/// the request's: none before a statement's first emulated start; the
/// query's own (`zend_string_copy(stmt->query_string)`, `query()`'s and an
/// emulated start's that rewrote nothing), which `debugDumpParams()` does
/// not print again; or the query PDO rewrote for an emulating connector,
/// released at the next start and with the statement.
///
/// ```php
/// $q = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// $sql = 'SELECT ' . str_repeat('1', 3);
/// $s = $q->query($sql);
/// debug_zval_dump($sql);
/// $s = $q->prepare($sql);
/// $s->execute();
/// $s->debugDumpParams();
/// $s = $q->prepare($sql . ', ?');
/// $s->execute([12345]);
/// $m = memory_get_usage();
/// $s->execute([12346]);
/// echo memory_get_usage() - $m, "\n";
/// $s->debugDumpParams();
/// ```
/// ```expect mysql
/// string(10) "SELECT 111" refcount(5)
/// SQL: [10] SELECT 111
/// Params:  0
/// 32
/// SQL: [13] SELECT 111, ?
/// Sent SQL: [19] SELECT 111, '12346'
/// Params:  1
/// Key: Position #0:
/// paramno=0
/// name=[0] ""
/// is_param=1
/// param_type=2
/// ```
#[derive(Default)]
pub(crate) enum Active {
    #[default]
    None,
    Query(Owner<PhpString>),
    Sent(Owner<PhpString>),
}

impl Active {
    /// The string, a view.
    fn string(&self) -> Option<PhpString> {
        match self {
            Active::None => None,
            Active::Query(s) | Active::Sent(s) => Some(**s),
        }
    }

    /// `zend_string_release(stmt->active_query_string)`.
    fn give_up(self, req: &mut Request) {
        if let Active::Query(s) | Active::Sent(s) = self {
            s.give_up(req);
        }
    }
}

/// `stmt->active_query_string = zend_string_copy(stmt->query_string)`,
/// or the query `pdo_parse_params` made, in place of the one before,
/// which is released.
pub(crate) fn set_active(req: &mut Request, object: PhpObject, sent: Option<Owner<PhpString>>) {
    let active = match sent {
        Some(sent) => Active::Sent(sent),
        None => match read(req, object, |s| s.query.as_deref().copied()).flatten() {
            Some(query) => Active::Query(Owner::take(req, query)),
            None => Active::None,
        },
    };
    let before = match object.native_mut::<Stmt>(req) {
        Some(s) => std::mem::replace(&mut s.active, active),
        None => active,
    };
    before.give_up(req);
}

/// `struct pdo_column_data`: the name one owner of the string the
/// connector described it with, folded to the connection's case.
pub(crate) struct ColumnInfo {
    pub(crate) name: Owner<PhpString>,
    pub(crate) max_len: u64,
    pub(crate) precision: u64,
}

/// `sizeof(struct pdo_column_data)`.
const COLUMN_DATA: usize = 24;

/// `stmt->columns`: the columns described, in the
/// `ecalloc(column_count, sizeof(struct pdo_column_data))` php keeps them
/// in, on the request's heap until `pdo_stmt_reset_columns`.
pub(crate) struct Columns {
    list: Vec<ColumnInfo>,
    held: Emalloc,
}

impl Columns {
    /// The allocation for `count` columns, none described yet; the `Err`
    /// is the memory limit's fatal.
    fn new(req: &mut Request, count: usize) -> Result<Columns, Thrown> {
        Ok(Columns {
            list: Vec::with_capacity(count),
            held: Emalloc::new(req, count * COLUMN_DATA)?,
        })
    }

    /// `pdo_stmt_reset_columns`: each name released, then the
    /// allocation given back.
    fn free(self, req: &mut Request) {
        for column in self.list {
            column.name.give_up(req);
        }
        self.held.free(req);
    }

    /// The name of column `at`, a view the caller takes an owner of
    /// before any PHP code can reset the columns.
    pub(crate) fn name(&self, at: usize) -> Option<PhpString> {
        self.list.get(at).map(|c| *c.name)
    }

    /// The position of the first column so named.
    pub(crate) fn position(&self, name: &[u8]) -> Option<usize> {
        self.list.iter().position(|c| c.name.as_bytes() == name)
    }
}

impl std::ops::Deref for Columns {
    type Target = [ColumnInfo];

    fn deref(&self) -> &[ColumnInfo] {
        &self.list
    }
}

/// Where the connector's statement is: none before a prepare, the
/// connector's while no call holds it, lent while one does.
#[derive(Default)]
enum Slot {
    #[default]
    None,
    Ready(Box<dyn Statement>),
    Lent,
}

/// `pdo_stmt_t`, the payload of a `PDOStatement` and of every subclass.
#[derive(Default)]
pub(crate) struct StmtState {
    slot: Slot,
    pub(crate) dbh: DbLink,
    /// `stmt->query_string`: one owner of the string `prepare()` or
    /// `query()` was handed, which `queryString` shares.
    pub(crate) query: Option<Owner<PhpString>>,
    pub(crate) active: Active,
    /// `stmt->executed`.
    pub(crate) executed: bool,
    /// `stmt->in_fetch`.
    pub(crate) in_fetch: bool,
    placeholders: Option<Placeholders>,
    /// `stmt->bound_params`, `None` until the first is registered.
    pub(crate) params: Option<BoundTable>,
    /// `stmt->bound_param_map`, for a query the connector rewrote.
    pub(crate) param_map: Option<ParameterMap>,
    /// `stmt->bound_columns`, likewise.
    pub(crate) columns_bound: Option<BoundTable>,
    /// `stmt->columns`, `None` until described.
    pub(crate) columns: Option<Columns>,
    /// `stmt->column_count`.
    pub(crate) column_count: usize,
    /// `stmt->default_fetch_type`, as set, read as a style where a fetch
    /// uses it.
    pub(crate) default_fetch: i64,
    /// `stmt->fetch`, what the default style fetches with.
    pub(crate) target: Target,
    /// `stmt->lazy_object_ref`: the `PDORow` of `FETCH_LAZY`, no owner.
    pub(crate) lazy: Option<PhpObject>,
    /// `stmt->row_count`.
    pub(crate) row_count: i64,
    /// `stmt->error_code`, `None` before anything cleared it.
    pub(crate) error_code: Option<SqlState>,
}

impl StmtState {
    /// The query's bytes, `ZSTR_VAL(stmt->query_string)`.
    pub(crate) fn query(&self) -> &[u8] {
        self.query.as_ref().map_or(&[], |q| q.as_bytes())
    }

    /// `stmt->bound_columns` or `stmt->bound_params`, read.
    fn table(&self, columns: bool) -> Option<&BoundTable> {
        if columns {
            self.columns_bound.as_ref()
        } else {
            self.params.as_ref()
        }
    }

    /// `stmt->bound_columns` or `stmt->bound_params`.
    fn list(&mut self, columns: bool) -> &mut Option<BoundTable> {
        if columns {
            &mut self.columns_bound
        } else {
            &mut self.params
        }
    }
}

/// The payload: `pdo_stmt_t`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type Stmt = Box<StmtState>;

/// `pdo_dbstmt_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning and comparison.
const REFUSALS: Refusals = Refusals::NONE.cloning().comparison();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `struct php_pdo_iterator`: the iterator, its key and the row fetched ahead.
    fn iterator_size(&self) -> usize {
        112
    }

    /// `php_pdo_free_statement`: the bound parameters and columns given
    /// back through the callback's `FREE`, the connector's statement dropped,
    /// the fetch style's values and the connection released.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(stmt) = object.native_mut::<Stmt>(req) else {
            return;
        };
        let params = stmt.params.take();
        let columns = stmt.columns_bound.take();
        let described = stmt.columns.take();
        let query = stmt.query.take();
        let active = std::mem::take(&mut stmt.active);
        let map = stmt.param_map.take();
        let slot = std::mem::take(&mut stmt.slot);
        let target = std::mem::take(&mut stmt.target);
        let dbh = std::mem::take(&mut stmt.dbh);
        let mut driver = match slot {
            Slot::Ready(driver) => Some(driver),
            Slot::None | Slot::Lent => None,
        };
        if let Some(params) = params {
            for bound in params.destroy(req) {
                free_bound(req, driver.as_deref_mut(), bound);
            }
        }
        if let Some(map) = map {
            map.give_up(req);
        }
        if let Some(columns) = columns {
            for bound in columns.destroy(req) {
                free_bound(req, driver.as_deref_mut(), bound);
            }
        }
        if let Some(mut driver) = driver {
            driver.close(req);
        }
        target.release(req);
        if let Some(described) = described {
            described.free(req);
        }
        active.give_up(req);
        if let Some(query) = query {
            query.give_up(req);
        }
        dbh.release(req);
    }

    /// `dbstmt_get_gc`: the connection, and what the default fetch style
    /// holds.
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(stmt) = object.native::<Stmt>(req) {
            if let DbLink::Owned(dbh) = &stmt.dbh {
                visit(dbh.as_value());
            }
            stmt.target.visit(visit);
        }
        GcTable::Properties
    }

    /// `dbstmt_prop_write` answers for the declared `queryString`, so
    /// every assignment reaches it.
    ///
    /// ```php
    /// $s = new PDOStatement;
    /// $s->queryString = 'SELECT 1';
    /// $p = 'queryString';
    /// try { $s->queryString = 'SELECT 2'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $s->$p = 'SELECT 3'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($s->queryString);
    /// ```
    /// ```expect
    /// Property queryString is read only
    /// Property queryString is read only
    /// string(8) "SELECT 1"
    /// ```
    fn write_overloaded(&self) -> bool {
        true
    }

    /// `dbstmt_prop_write`: `queryString` once written is read only.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if name.as_bytes() == b"queryString"
            && object
                .slot(0)
                .is_some_and(|v| !matches!(v.as_ref(), ValueRef::Undef))
        {
            value.give_up(req);
            name.give_up(req);
            return Err(throw_error(
                req,
                "Error",
                "Property queryString is read only",
            ));
        }
        Std.write_property(req, object, name, value)
    }

    /// `dbstmt_prop_delete`.
    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if name == b"queryString" {
            return Err(throw_error(
                req,
                "Error",
                "Property queryString is read only",
            ));
        }
        Std.unset_property(req, object, name)
    }

    /// `pdo_stmt_iter_get`, what a `foreach` walks without asking
    /// `getIterator()`, so a fetch's throw is raised in no frame of the
    /// statement's: the walk by reference refused, an uninitialized
    /// statement refused, and the iterator, whose first row is fetched
    /// once it stands in the object store.
    ///
    /// ```php
    /// class P { protected $v; }
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query("SELECT 'y' AS v");
    /// $s->setFetchMode(PDO::FETCH_INTO, new P);
    /// try { foreach ($s as $o) {} } catch (Error $e) { echo count($e->getTrace()), ' ', $e->getMessage(), "\n"; }
    /// try { foreach ($db->query('SELECT 1') as &$r) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// foreach ($db->query('SELECT 1 AS n UNION SELECT 2') as $k => $r) echo $k, ':', $r['n'], "\n";
    /// ```
    /// ```expect
    /// 0 Cannot access protected property P::$v
    /// An iterator cannot be used with foreach by reference
    /// 0:1
    /// 1:2
    /// ```
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            return Err(throw_error(
                req,
                "Error",
                "An iterator cannot be used with foreach by reference",
            ));
        }
        handle(req, object)?;
        Ok(Some(Box::new(Rows {
            key: None,
            ahead: None,
        })))
    }
}

/// `struct php_pdo_iterator`: the key and the row fetched ahead, `None`
/// once a fetch found none; the first is fetched once the iterator
/// stands in the object store ([`ClassIterator::made`]), as
/// `pdo_stmt_iter_get` fetches it after `zend_iterator_init`.
struct Rows {
    key: Option<i64>,
    ahead: Option<Owner<Value>>,
}

impl Rows {
    /// `pdo_stmt_iter_move_forwards`.
    fn forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        if let Some(old) = self.ahead.take() {
            old.give_up(req);
        }
        self.ahead = fetch::row(req, object, 0)?;
        if self.ahead.is_none() {
            self.key = None;
            return handle_error(req, object, false);
        }
        self.key = Some(self.key.map_or(0, |key| key + 1));
        Ok(())
    }
}

impl ClassIterator for Rows {
    /// The rest of `pdo_stmt_iter_get`: the first row fetched ahead, once
    /// the iterator stands in the object store, so a row's object takes a
    /// number after the iterator's and before an `InternalIterator`'s, and
    /// an `IteratorIterator` fetches it as it is made.
    ///
    /// ```php
    /// class C { public $n; function __construct() { echo "ctor\n"; } }
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query("SELECT 1 AS n", PDO::FETCH_CLASS, 'C');
    /// $it = new IteratorIterator($s);
    /// echo "made\n";
    /// foreach ($it as $r) {}
    /// $s = $db->query("SELECT 1 AS n", PDO::FETCH_OBJ);
    /// $it = $s->getIterator();
    /// foreach ($it as $r) var_dump(spl_object_id($r), spl_object_id($it));
    /// ```
    /// ```expect
    /// ctor
    /// made
    /// int(10)
    /// int(11)
    /// ```
    fn made(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.forward(req, object)
    }

    /// No `rewind` in `pdo_stmt_iter_funcs`.
    fn rewind(&mut self, _: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        Ok(())
    }

    fn valid(&mut self, _: &mut Request, _: PhpObject) -> Result<bool, Thrown> {
        Ok(self.ahead.is_some())
    }

    fn current(&mut self, req: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        let row = self.ahead.as_deref().copied().unwrap_or_default();
        Ok(Owner::take(req, row))
    }

    fn key(&mut self, req: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        let key = self.key.map_or_else(Value::null, Value::int);
        Ok(Owner::take(req, key))
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.forward(req, object)
    }

    fn release(&mut self, req: &mut Request, _: PhpObject) {
        if let Some(row) = self.ahead.take() {
            row.give_up(req);
        }
    }
}

/// `param_dtor`: the connector told through `FREE`, and what the entry
/// holds given up.
fn free_bound(req: &mut Request, driver: Option<&mut dyn Statement>, mut bound: Bound) {
    if let Some(driver) = driver {
        driver.bind(req, bound.binding(), BindEvent::Free).ok();
    }
    bound.give_up(req);
}

// ---- the statement's state, lent and read

pub(super) fn read<R>(
    req: &Request,
    object: PhpObject,
    look: impl FnOnce(&Stmt) -> R,
) -> Option<R> {
    object.native::<Stmt>(req).map(look)
}

pub(super) fn write<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut Stmt) -> R,
) -> Option<R> {
    object.native_mut::<Stmt>(req).map(change)
}

/// The connection the statement reports to.
pub(super) fn dbh_of(req: &Request, object: PhpObject) -> Option<PhpObject> {
    read(req, object, |s| s.dbh.object()).flatten()
}

/// `PHP_STMT_GET_OBJ`: the statement's connection, php's `Error` naming
/// the object's class where no connection made it.
pub(super) fn handle(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    if let Some(dbh) = dbh_of(req, object) {
        return Ok(dbh);
    }
    let class = crate::class_name(req, object.class());
    Err(throw_error(
        req,
        "Error",
        &format!("{class} object is uninitialized"),
    ))
}

/// `$this` with `PHP_STMT_GET_OBJ`.
pub(super) fn this_stmt(
    req: &mut Request,
    stmt: PhpObject,
) -> Result<(PhpObject, PhpObject), Thrown> {
    let dbh = handle(req, stmt)?;
    Ok((stmt, dbh))
}

/// `PDO_STMT_CLEAR_ERR`.
pub(super) fn clear_error(req: &mut Request, object: PhpObject) {
    write(req, object, |s| s.error_code = Some(SqlState::NONE));
}

/// `PDO_HANDLE_STMT_ERR`: the error on the statement handled under its
/// connection's error handling where there is one.
pub(super) fn handle_error(
    req: &mut Request,
    object: PhpObject,
    pending: bool,
) -> Result<(), Thrown> {
    let state = read(req, object, |s| s.error_code).flatten();
    if state == Some(SqlState::NONE) {
        return Ok(());
    }
    let Some(dbh) = dbh_of(req, object) else {
        return Ok(());
    };
    error::handle(req, dbh, On::Stmt(object), pending)
}

/// A connector's failure on the statement recorded as its error function
/// records it, and nothing handled yet: the throwable left pending
/// comes back.
pub(super) fn record(req: &mut Request, object: PhpObject, failure: Failure) -> Result<(), Thrown> {
    if let Some(dbh) = dbh_of(req, object) {
        error::record(req, dbh, On::Stmt(object), failure.cause)?;
    }
    match failure.thrown {
        Some(thrown) => Err(thrown),
        None => Ok(()),
    }
}

/// The connector's statement lent to `f` for one call and put back.
pub(super) fn with_driver<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, &mut dyn Statement) -> R,
) -> Result<R, Thrown> {
    let slot = write(req, object, |s| std::mem::replace(&mut s.slot, Slot::Lent));
    let Some(Slot::Ready(mut driver)) = slot else {
        if let Some(taken) = slot {
            write(req, object, |s| s.slot = taken);
        }
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: a PDOStatement's driver statement was asked for while a callback held it",
        ));
    };
    let connector = dbh_of(req, object).and_then(|dbh| crate::dbh::driver_of(req, dbh));
    let answered = crate::as_driver(req, connector, |req| f(req, &mut *driver));
    write(req, object, |s| s.slot = Slot::Ready(driver));
    Ok(answered)
}

/// The connector's statement lent to `f` for one call and put back, as
/// [`with_driver`] lends it, `None` for a statement no prepare gave one.
pub(super) fn with_statement<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, Option<&dyn Statement>) -> R,
) -> Result<R, Thrown> {
    if read(req, object, |s| matches!(s.slot, Slot::None)).unwrap_or(true) {
        return Ok(f(req, None));
    }
    with_driver(req, object, |req, driver| f(req, Some(&*driver)))
}

/// A prepared statement's connector half laid down on the new statement.
pub(super) fn attach(req: &mut Request, object: PhpObject, prepared: Prepared) {
    let refused = write(req, object, |s| {
        s.slot = Slot::Ready(prepared.statement);
        s.placeholders = Some(prepared.placeholders);
        std::mem::replace(&mut s.param_map, prepared.parameter_map)
    });
    if let Some(Some(map)) = refused {
        map.give_up(req);
    }
}

/// `php_pdo_stmt_set_column_count`: the described columns kept while
/// the count stands, dropped where it changed.
pub(super) fn set_column_count(req: &mut Request, object: PhpObject, count: usize) {
    let reset = write(req, object, |s| {
        let reset = if s.column_count == count {
            None
        } else {
            s.columns.take()
        };
        s.column_count = count;
        reset
    });
    if let Some(columns) = reset.flatten() {
        columns.free(req);
    }
}

/// `zend_string_tolower` or `zend_string_toupper` of a column's name
/// where the connection's case asks for one: the same string where no
/// byte changes, else a new one and the old released.
fn folded(
    req: &mut Request,
    name: Owner<PhpString>,
    case: dbh::Case,
) -> Result<Owner<PhpString>, Thrown> {
    let bytes = name.as_bytes();
    let lower = match case {
        dbh::Case::Natural => return Ok(name),
        dbh::Case::Lower if !bytes.iter().any(u8::is_ascii_uppercase) => return Ok(name),
        dbh::Case::Upper if !bytes.iter().any(u8::is_ascii_lowercase) => return Ok(name),
        dbh::Case::Lower => true,
        dbh::Case::Upper => false,
    };
    let made = PhpString::filled(req, bytes.len(), |to| {
        to.copy_from_slice(bytes);
        if lower {
            to.make_ascii_lowercase();
        } else {
            to.make_ascii_uppercase();
        }
    });
    name.give_up(req);
    made
}

/// `pdo_stmt_describe_columns`: each column described by the connector,
/// its name folded to the connection's case, and a column bound by name
/// placed at its position; `false` where the connector failed.
pub(super) fn describe_columns(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let count = read(req, object, |s| s.column_count).unwrap_or(0);
    let case = dbh_of(req, object)
        .and_then(|dbh| crate::dbh::state(req, dbh).map(|d| d.case))
        .unwrap_or_default();
    let columns = Columns::new(req, count)?;
    if let Some(old) = write(req, object, |s| s.columns.replace(columns)).flatten() {
        old.free(req);
    }
    for column in 0..count {
        let described = with_driver(req, object, |req, driver| driver.describe(req, column))?;
        let info = match described {
            Ok(info) => info,
            Err(failure) => {
                record(req, object, failure)?;
                return Ok(false);
            }
        };
        let name = folded(req, info.name, case)?;
        let info = ColumnInfo {
            name,
            max_len: info.max_len,
            precision: info.precision,
        };
        let mut info = Some(info);
        write(req, object, |s| {
            let Some(name) = info.as_ref().map(|i| i.name.as_bytes()) else {
                return;
            };
            if let Some(bound) = s
                .columns_bound
                .iter_mut()
                .flat_map(BoundTable::iter_mut)
                .find(|b| b.name_bytes() == Some(name))
            {
                bound.position = i64::try_from(column).unwrap_or(-1);
            }
            if let Some(columns) = s.columns.as_mut() {
                columns.list.extend(info.take());
            }
        });
        // A statement with no columns to hold it gives the name back.
        if let Some(unheld) = info {
            unheld.name.give_up(req);
        }
    }
    Ok(true)
}

/// `dispatch_param_event`: the callback served on every bound parameter and
/// then every bound column; `false` at the first it refused, its error
/// recorded.
pub(super) fn dispatch(
    req: &mut Request,
    object: PhpObject,
    event: BindEvent,
) -> Result<bool, Thrown> {
    for columns in [false, true] {
        let len = read(req, object, |s| s.table(columns).map_or(0, BoundTable::len)).unwrap_or(0);
        for at in 0..len {
            if let Err(failure) = hook_at(req, object, columns, at, event)? {
                record(req, object, failure)?;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// One event of the callback on the entry stored at `at`, taken out of
/// its list for the call and put back at its place.
fn hook_at(
    req: &mut Request,
    object: PhpObject,
    columns: bool,
    at: usize,
    event: BindEvent,
) -> Result<Result<(), Failure>, Thrown> {
    let taken = write(req, object, |s| {
        s.list(columns).as_mut().and_then(|t| t.lend(at))
    });
    let Some(mut row) = taken.flatten() else {
        return Ok(Ok(()));
    };
    let answered = with_driver(req, object, |req, driver| {
        driver.bind(req, row.bound.binding(), event)
    });
    if let Some(table) = object
        .native_mut::<Stmt>(req)
        .and_then(|s| s.list(columns).as_mut())
    {
        table.put_back(at, row);
    } else {
        let bound = row.release(req);
        free_bound(req, None, bound);
    }
    answered
}

/// The statement's connector started, `stmt->methods->executer`, and on
/// its first start the columns described where the connection allocates its own:
/// `Err(None)` for a failure recorded on the statement and not yet
/// handled.
pub(super) fn run(req: &mut Request, object: PhpObject) -> Result<(), Option<Thrown>> {
    let (executed, query) = read(req, object, |s| {
        (
            s.executed,
            s.active.string().or(s.query.as_deref().copied()),
        )
    })
    .unwrap_or_default();
    // An owner for the call: the connector's PHP code may start the
    // statement again and release the string it was handed.
    let query = query.map(|q| Owner::take(req, q));
    let ran = with_driver(req, object, |req, driver| {
        driver.execute(req, executed, query.as_ref().map_or(&[], |q| q.as_bytes()))
    });
    if let Some(query) = query {
        query.give_up(req);
    }
    let ran = ran.map_err(Some)?;
    let done = match ran {
        Ok(done) => done,
        Err(failure) => {
            return match record(req, object, failure) {
                Ok(()) => Err(None),
                Err(thrown) => Err(Some(thrown)),
            };
        }
    };
    set_column_count(req, object, done.columns);
    if let Some(rows) = done.rows {
        write(req, object, |s| s.row_count = rows);
    }
    if !executed {
        let own = dbh_of(req, object)
            .and_then(|dbh| crate::dbh::state(req, dbh).map(|d| d.describes_at_execute))
            .unwrap_or(false);
        let described = read(req, object, |s| s.columns.is_some()).unwrap_or(false);
        if own && !described && !describe_columns(req, object).map_err(Some)? {
            return Err(None);
        }
        write(req, object, |s| s.executed = true);
    }
    Ok(())
}

// ---- binding

/// `really_register_bound_param`: the value converted where its type
/// asks, a column bound by name placed, a parameter's name given its
/// `:`, the callback's `NORMALIZE` and `ALLOC`, and the entry stored under
/// its name or position; `false` where any of those refused.
pub(super) fn register(
    req: &mut Request,
    object: PhpObject,
    mut bound: Bound,
) -> Result<bool, Thrown> {
    let columns = !bound.is_parameter;
    if read(req, object, |s| s.table(columns).is_none()) == Some(true) {
        let table = match BoundTable::new(req) {
            Ok(table) => table,
            Err(thrown) => {
                bound.give_up(req);
                return Err(thrown);
            }
        };
        write(req, object, |s| *s.list(columns) = Some(table));
    }
    if let Err(thrown) = convert(req, &mut bound) {
        bound.give_up(req);
        return Err(thrown);
    }
    if !bound.is_parameter
        && let Some(name) = bound.name_bytes().map(<[u8]>::to_vec)
        && let Some(found) = read(req, object, |s| {
            s.columns.as_ref().map(|c| c.position(&name))
        })
        .flatten()
    {
        if let Some(at) = found {
            bound.position = i64::try_from(at).unwrap_or(-1);
        }
        if bound.position == -1
            && let Some(dbh) = dbh_of(req, object)
        {
            let supp = format!(
                "Did not find column name '{}' in the defined columns; it will not be bound",
                String::from_utf8_lossy(&name)
            );
            if let Err(thrown) =
                error::raise_impl(req, dbh, On::Stmt(object), SqlState::GENERAL, Some(&supp))
            {
                bound.give_up(req);
                return Err(thrown);
            }
        }
    }
    if let Err(thrown) = bound.own_name(req) {
        bound.give_up(req);
        return Err(thrown);
    }
    if bound.is_parameter && !crate::parser::name_to_position(req, object, &mut bound)? {
        bound.give_up(req);
        return Ok(false);
    }
    if !hook(req, object, &mut bound, BindEvent::Normalize)? {
        handle_error(req, object, false)?;
        bound.give_up(req);
        return Ok(false);
    }
    let key = bound.key();
    let mut pending = Some(bound);
    let stored = with_table(req, object, columns, |req, t| {
        pending.take().map(|bound| t.store(req, bound))
    });
    // PHP code the conversion ran took the table: the entry is given back.
    if let Some(bound) = pending {
        bound.give_up(req);
    }
    let Some((at, displaced)) = stored.flatten().transpose()? else {
        return Ok(false);
    };
    for old in displaced {
        free_bound_through(req, object, old)?;
    }
    // The connector told of the entry where it is stored, `pparam`.
    let at = read(req, object, |s| s.table(columns).and_then(|t| t.find(&key)))
        .flatten()
        .unwrap_or(at);
    if let Err(failure) = hook_at(req, object, columns, at, BindEvent::Alloc)? {
        record(req, object, failure)?;
        handle_error(req, object, false)?;
        let taken = with_table(req, object, columns, |req, t| t.delete(req, &key)).transpose()?;
        if let Some(taken) = taken.flatten() {
            free_bound_through(req, object, taken)?;
        }
        return Ok(false);
    }
    Ok(true)
}

/// `stmt->bound_params` or `stmt->bound_columns` taken off the statement
/// for a change that runs no PHP code, and put back; `None` where the
/// statement holds none.
fn with_table<R>(
    req: &mut Request,
    object: PhpObject,
    columns: bool,
    change: impl FnOnce(&mut Request, &mut BoundTable) -> R,
) -> Option<R> {
    let mut table = write(req, object, |s| s.list(columns).take()).flatten()?;
    let changed = change(req, &mut table);
    let mut unheld = Some(table);
    write(req, object, |s| *s.list(columns) = unheld.take());
    // A statement gone in between gives the table back.
    if let Some(table) = unheld {
        for bound in table.destroy(req) {
            free_bound(req, None, bound);
        }
    }
    Some(changed)
}

/// The value converted in place where its type asks,
/// `really_register_bound_param`'s `try_convert_to_string`,
/// `convert_to_long` and `convert_to_boolean`.
fn convert(req: &mut Request, bound: &mut Bound) -> Result<(), Thrown> {
    let inner = bound.value.deref();
    let converted = match (bound.ty.kind(), inner.as_ref()) {
        (BindKind::Str, ValueRef::Null | ValueRef::Str(_)) => None,
        (BindKind::Str, _) if bound.max_len <= 0 => Some(to_string(req, &inner)?.into_value()),
        (BindKind::Int, ValueRef::Bool(b)) => Some(Owner::int(i64::from(b))),
        (BindKind::Bool, ValueRef::Int(n)) => Some(Owner::bool(n != 0)),
        _ => None,
    };
    if let Some(converted) = converted {
        bound.value.convert(req, converted);
    }
    Ok(())
}

/// `param_dtor` with the statement's connector told.
fn free_bound_through(req: &mut Request, object: PhpObject, bound: Bound) -> Result<(), Thrown> {
    let has_driver = read(req, object, |s| matches!(s.slot, Slot::Ready(_))).unwrap_or(false);
    if has_driver {
        with_driver(req, object, |req, driver| {
            free_bound(req, Some(driver), bound);
        })?;
    } else {
        free_bound(req, None, bound);
    }
    Ok(())
}

/// One event of the callback on one entry not yet stored.
fn hook(
    req: &mut Request,
    object: PhpObject,
    bound: &mut Bound,
    event: BindEvent,
) -> Result<bool, Thrown> {
    let has_driver = read(req, object, |s| matches!(s.slot, Slot::Ready(_))).unwrap_or(false);
    if !has_driver {
        return Ok(true);
    }
    let answered = with_driver(req, object, |req, driver| {
        driver.bind(req, bound.binding(), event)
    })?;
    match answered {
        Ok(()) => Ok(true),
        Err(failure) => {
            record(req, object, failure)?;
            Ok(false)
        }
    }
}

/// `register_bound_param` and `bindValue()`'s body: the position or
/// name, checked; the value, a copy; the type; for a parameter or a
/// column bound by reference, the length and the connector's options.
/// What a bind method was given: the key, the value (the caller's
/// reference for a by-reference bind), the type, and for `bindParam` and
/// `bindColumn` the length and the connector's options.
#[derive(Clone, Copy)]
pub(super) struct Asked {
    key: Union<PhpString, i64>,
    value: Value,
    ty: i64,
    max_len: i64,
    driver_params: Value,
}

impl Asked {
    /// A by-reference bind's: the caller's reference itself where the call
    /// passed one, the value where it passed a value.
    fn by_ref(
        key: Union<PhpString, i64>,
        var: &Ref<Value>,
        ty: i64,
        (max_len, driver_params): (i64, Value),
    ) -> Asked {
        let value = match var.reference() {
            Some(cell) => Value::reference(cell),
            None => var.get().copied().unwrap_or_default(),
        };
        let driver_params = if crate::is_null(&driver_params) {
            Value::default()
        } else {
            driver_params
        };
        Asked {
            key,
            value,
            ty,
            max_len,
            driver_params,
        }
    }
}

fn bind(
    req: &mut Request,
    stmt: PhpObject,
    is_parameter: bool,
    asked: Asked,
) -> Result<bool, Thrown> {
    let Asked {
        key,
        value,
        ty,
        max_len,
        driver_params,
    } = asked;
    let ty = BindType::of(ty);
    let (stmt, _) = this_stmt(req, stmt)?;
    let (name, position) = if let Union::A(name) = key {
        if name.is_empty() {
            return Err(argument_error(req, "ValueError", 1, "must not be empty"));
        }
        (Some(Owner::take(req, name)), -1)
    } else {
        let n = if let Union::B(n) = key { n } else { 0 };
        if n <= 0 {
            return Err(argument_error(
                req,
                "ValueError",
                1,
                "must be greater than or equal to 1",
            ));
        }
        (None, n - 1)
    };
    let bound = Bound {
        position,
        name,
        ty,
        value: Owner::take(req, value),
        driver_params: Owner::take(req, driver_params),
        max_len,
        is_parameter,
    };
    register(req, stmt, bound)
}

// ---- executing

// ---- attributes, metadata and cursors

/// `pdo_raise_impl_error(stmt->dbh, stmt, "IM001", supp)` and `false`.
fn unsupported(
    req: &mut Request,
    stmt: PhpObject,
    dbh: PhpObject,
    supp: &str,
) -> Result<(), Thrown> {
    error::raise_impl(req, dbh, On::Stmt(stmt), SqlState(*b"IM001"), Some(supp))
}

/// A connector slot on the statement, its failure handled on it:
/// `Ok(None)` for a failure handled, `Err(())` for the connector's null slot.
fn on_driver<T>(
    req: &mut Request,
    stmt: PhpObject,
    call: impl FnOnce(&mut Request, &mut dyn Statement) -> Result<T, Failure>,
) -> Result<Result<Option<T>, ()>, Thrown> {
    match with_driver(req, stmt, call)? {
        Ok(value) => Ok(Ok(Some(value))),
        Err(Failure {
            cause: Cause::Unsupported,
            thrown: None,
        }) => Ok(Err(())),
        Err(failure) => {
            let pending = failure.thrown.is_some();
            let recorded = record(req, stmt, failure);
            handle_error(req, stmt, pending)?;
            recorded?;
            Ok(Ok(None))
        }
    }
}

// ---- the class

/// `interface IteratorAggregate`, Core's, which `PDOStatement` implements.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

/// `interface Iterator`, Core's, what `getIterator()` answers.
#[php_class(extern)]
pub(crate) struct Iterator;

/// `@not-serializable class PDOStatement implements IteratorAggregate`, as
/// `pdo_stmt.stub.php` declares it, over the statement as its payload and
/// `pdo_dbh_stmt_handlers`.
///
/// `pdo_dbstmt_new` allocates a `pdo_stmt_t`, which `memory_get_usage()`
/// counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['PDOStatement'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// PDOStatement 224
/// ```
#[php_class(not_serializable, implements(IteratorAggregate), size = 216, payload = Stmt, handlers = HANDLERS)]
pub(crate) struct PDOStatement;

/// `public string $queryString`, then the methods in the stub's order.
#[php_class]
impl PDOStatement {
    #[php(property)]
    const queryString: PhpString;

    /// `PDOStatement::bindColumn(string|int $column, mixed &$var, ...): bool`:
    /// the variable written at each fetch.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query('SELECT 1 AS a, 2 AS b UNION SELECT 3, 4');
    /// $s->bindColumn('a', $a); $s->bindColumn(2, $b, PDO::PARAM_STR);
    /// while ($s->fetch(PDO::FETCH_BOUND)) var_dump($a, $b);
    /// $db->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_WARNING);
    /// var_dump($s->bindColumn('nope', $c));
    /// ```
    /// ```expect
    /// string(1) "1"
    /// string(1) "2"
    /// string(1) "3"
    /// string(1) "4"
    ///
    /// Warning: PDOStatement::bindColumn(): SQLSTATE[HY000]: General error: Did not find column name 'nope' in the defined columns; it will not be bound in %s on line 7
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn bindColumn(
        req: &mut Request,
        this: This<Self>,
        column: Union<PhpString, i64>,
        var: Ref<Value>,
        #[php(default = PDO::PARAM_STR)] r#type: i64,
        #[php(default = 0)] maxLength: i64,
        #[php(default = null)] driverOptions: Value,
    ) -> Result<bool, Thrown> {
        let asked = Asked::by_ref(column, &var, r#type, (maxLength, driverOptions));
        bind(req, this.object(), false, asked)
    }

    /// `PDOStatement::bindParam(string|int $param, mixed &$var, ...): bool`:
    /// the variable read at each start, and converted in place where the
    /// type asks.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT ? * 2');
    /// $n = 1;
    /// $s->bindParam(1, $n, PDO::PARAM_INT);
    /// foreach ([2, 3] as $n) { $s->execute(); echo $s->fetchColumn(), "\n"; }
    /// $b = true; $s->bindParam(1, $b, PDO::PARAM_INT); var_dump($b);
    /// ```
    /// ```expect
    /// 4
    /// 6
    /// int(1)
    /// ```
    #[php(tentative)]
    fn bindParam(
        req: &mut Request,
        this: This<Self>,
        param: Union<PhpString, i64>,
        var: Ref<Value>,
        #[php(default = PDO::PARAM_STR)] r#type: i64,
        #[php(default = 0)] maxLength: i64,
        #[php(default = null)] driverOptions: Value,
    ) -> Result<bool, Thrown> {
        let asked = Asked::by_ref(param, &var, r#type, (maxLength, driverOptions));
        bind(req, this.object(), true, asked)
    }

    /// `PDOStatement::bindValue(string|int $param, mixed $value, int $type = PDO::PARAM_STR): bool`:
    /// the value copied now, converted to the type.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT :a, typeof(:b), ?3');
    /// var_dump($s->bindValue(':a', 5), $s->bindValue('b', true, PDO::PARAM_INT), $s->bindValue(3, null, PDO::PARAM_NULL));
    /// $s->execute();
    /// var_dump($s->fetch(PDO::FETCH_NUM));
    /// try { $s->bindValue(0, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// array(3) {
    ///   [0]=>
    ///   string(1) "5"
    ///   [1]=>
    ///   string(7) "integer"
    ///   [2]=>
    ///   NULL
    /// }
    /// PDOStatement::bindValue(): Argument #1 ($param) must be greater than or equal to 1
    /// ```
    #[php(tentative)]
    fn bindValue(
        req: &mut Request,
        this: This<Self>,
        param: Union<PhpString, i64>,
        value: Value,
        #[php(default = PDO::PARAM_STR)] r#type: i64,
    ) -> Result<bool, Thrown> {
        let asked = Asked {
            key: param,
            value,
            ty: r#type,
            max_len: 0,
            driver_params: Value::null(),
        };
        bind(req, this.object(), true, asked)
    }

    /// `PDOStatement::closeCursor(): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT 1 UNION SELECT 2');
    /// $s->execute(); var_dump($s->fetchColumn(), $s->closeCursor());
    /// $s->execute(); var_dump($s->fetchColumn());
    /// ```
    /// ```expect
    /// int(1)
    /// bool(true)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn closeCursor(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (stmt, _) = this_stmt(req, this.object())?;
        clear_error(req, stmt);
        match on_driver(req, stmt, |req, driver| driver.close_cursor(req))? {
            Ok(Some(())) => {}
            Ok(None) => return Ok(false),
            Err(()) => {
                while with_driver(req, stmt, |req, driver| {
                    driver.fetch(req, crate::connector::Orientation::Forward, 0)
                })?
                .unwrap_or(false)
                {}
            }
        }
        write(req, stmt, |s| s.executed = false);
        Ok(true)
    }

    /// `PDOStatement::columnCount(): int`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT 1, 2, 3');
    /// var_dump($s->columnCount()); $s->execute(); var_dump($s->columnCount());
    /// ```
    /// ```expect
    /// int(0)
    /// int(3)
    /// ```
    #[php(tentative)]
    fn columnCount(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (stmt, _) = this_stmt(req, this.object())?;
        let count = read(req, stmt, |s| s.column_count).unwrap_or(0);
        Ok(i64::try_from(count).unwrap_or(i64::MAX))
    }

    /// `PDOStatement::debugDumpParams(): ?bool`: the query, the query sent
    /// where PDO rewrote it, and each bound parameter.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT :a, :b');
    /// $s->bindValue(':a', 1, PDO::PARAM_INT); $s->bindValue('b', 'x');
    /// var_dump($s->debugDumpParams());
    /// ```
    /// ```expect
    /// SQL: [13] SELECT :a, :b
    /// Params:  2
    /// Key: Name: [2] :a
    /// paramno=-1
    /// name=[2] ":a"
    /// is_param=1
    /// param_type=1
    /// Key: Name: [2] :b
    /// paramno=-1
    /// name=[2] ":b"
    /// is_param=1
    /// param_type=2
    /// NULL
    /// ```
    #[php(tentative)]
    fn debugDumpParams(req: &mut Request, this: This<Self>) -> Result<Option<bool>, Thrown> {
        let (stmt, _) = this_stmt(req, this.object())?;
        let Some(out) = read(req, stmt, dump_params) else {
            return Ok(None);
        };
        req.write(&out);
        Ok(None)
    }

    /// `PDOStatement::errorCode(): ?string`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);
    /// $s = $db->prepare('SELECT * FROM sqlite_master');
    /// var_dump($s->errorCode()); $s->execute(); var_dump($s->errorCode());
    /// ```
    /// ```expect
    /// NULL
    /// string(5) "00000"
    /// ```
    #[php(tentative)]
    fn errorCode(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        let (stmt, _) = this_stmt(req, this.object())?;
        match read(req, stmt, |s| s.error_code).flatten() {
            Some(state) => PhpString::new(req, state.as_str().as_bytes()).map(Some),
            None => Ok(None),
        }
    }

    /// `PDOStatement::errorInfo(): array`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);
    /// $db->exec('CREATE TABLE t (a UNIQUE)');
    /// $s = $db->prepare('INSERT INTO t VALUES (1)');
    /// $s->execute(); $s->execute();
    /// var_dump($s->errorInfo());
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   string(5) "23000"
    ///   [1]=>
    ///   int(19)
    ///   [2]=>
    ///   string(29) "UNIQUE constraint failed: t.a"
    /// }
    /// ```
    #[php(tentative)]
    fn errorInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let (stmt, dbh) = this_stmt(req, this.object())?;
        let state = read(req, stmt, |s| s.error_code).flatten();
        let mut out = PhpArray::new(req)?;
        let made = crate::string(req, state.as_ref().map_or("", SqlState::as_str).as_bytes())?;
        out.append(req, made)?;
        if state != Some(SqlState::NONE) {
            error::fetch_error(req, dbh, On::Stmt(stmt), &mut out)?;
        }
        while out.len() < 3 {
            out.append(req, Owner::null())?;
        }
        Ok(out)
    }

    /// `PDOStatement::execute(?array $params = null): bool`: the array's
    /// values bound in its place by key, then the connector started.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->exec('CREATE TABLE t (a, b)');
    /// $s = $db->prepare('INSERT INTO t VALUES (:a, :b)');
    /// var_dump($s->execute([':a' => 1, 'b' => 'x']), $s->rowCount());
    /// $s = $db->prepare('SELECT * FROM t WHERE a = ?');
    /// var_dump($s->execute([1]), $s->fetch(PDO::FETCH_NUM));
    /// $s = $db->prepare('INSERT INTO t VALUES (?, ?)');
    /// try { $s->execute([1]); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// int(1)
    /// bool(true)
    /// array(2) {
    ///   [0]=>
    ///   string(1) "1"
    ///   [1]=>
    ///   string(1) "x"
    /// }
    /// ```
    ///
    /// Each bound parameter and column is the request's at php's sizes:
    /// the table at the first, `zend_hash_init(hash, 13, ...)`, its
    /// packed or hash data laid down, the 80-byte entry, and a name
    /// `zend_string_init`'d; an array given here replaces the table.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $m = memory_get_usage();
    /// $s = $db->prepare('SELECT ?');
    /// echo memory_get_usage() - $m, "\n";
    /// $s->execute([1]);
    /// echo memory_get_usage() - $m, "\n";
    /// $s->bindValue(1, 2);
    /// echo memory_get_usage() - $m, "\n";
    /// $t = $db->prepare('SELECT :a');
    /// $m = memory_get_usage();
    /// $t->bindValue('a', 1);
    /// echo memory_get_usage() - $m, "\n";
    /// $t->bindColumn(1, $c);
    /// echo memory_get_usage() - $m, "\n";
    /// $t->execute(['a' => 2]);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 248
    /// 792
    /// 792
    /// 808
    /// 1296
    /// 1352
    /// ```
    #[php(tentative)]
    fn execute(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] params: Option<PhpArray>,
    ) -> Result<bool, Thrown> {
        let input = params;
        let (stmt, dbh) = this_stmt(req, this.object())?;
        clear_error(req, stmt);
        if let Some(input) = input {
            if let Some(old) = write(req, stmt, |s| s.params.take()).flatten() {
                for bound in old.destroy(req) {
                    free_bound_through(req, stmt, bound)?;
                }
            }
            // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(input_params), …)`
            // (`Z/ext/pdo/pdo_stmt.c:407`), no owner of the table taken:
            // the argument holds it while a driver's hook converts a value.
            let walked = walk::foreach(req, input, Hold::None, |req, row| -> walk::Step<bool> {
                let key = row.key().clone();
                let value = row.value().deref();
                let (name, position) = match key {
                    Key::Str(name) => (Some(Owner::take(req, name)), -1),
                    Key::Int(n) => (None, n),
                };
                let bound = Bound {
                    position,
                    name,
                    ty: BindType::STR,
                    value: Owner::take(req, value),
                    driver_params: Owner::null(),
                    max_len: 0,
                    is_parameter: true,
                };
                if !register(req, stmt, bound)? {
                    return Ok(ControlFlow::Break(false));
                }
                Ok(ControlFlow::Continue(()))
            })?;
            if let Some(answer) = walked {
                return Ok(answer);
            }
        }
        let emulated = read(req, stmt, |s| {
            s.placeholders == Some(Placeholders::Emulated)
        })
        .unwrap_or(false);
        if emulated {
            // The string left from the previous start, released before
            // the parse, as `pdo_stmt.c` releases it.
            let old = write(req, stmt, |s| std::mem::take(&mut s.active));
            if let Some(old) = old {
                old.give_up(req);
            }
            let query = read(req, stmt, |s| s.query().to_vec()).unwrap_or_default();
            match crate::parser::parse_params(req, stmt, dbh, &query)? {
                crate::parser::Parsed::Unchanged => set_active(req, stmt, None),
                crate::parser::Parsed::Rewritten(sent) => set_active(req, stmt, Some(sent)),
                crate::parser::Parsed::Failed => return Ok(false),
            }
        } else if !dispatch(req, stmt, BindEvent::ExecPre)? {
            handle_error(req, stmt, false)?;
            return Ok(false);
        }
        match run(req, stmt) {
            Ok(()) => {}
            Err(Some(thrown)) => return Err(thrown),
            Err(None) => {
                handle_error(req, stmt, false)?;
                return Ok(false);
            }
        }
        if !dispatch(req, stmt, BindEvent::ExecPost)? {
            handle_error(req, stmt, false)?;
            return Ok(false);
        }
        Ok(true)
    }

    /// `PDOStatement::fetch(int $mode = PDO::FETCH_DEFAULT, int $cursorOrientation = PDO::FETCH_ORI_NEXT, int $cursorOffset = 0): mixed`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query("SELECT 1 AS a, 'x' AS b, NULL AS c");
    /// var_dump($s->fetch());
    /// $s = $db->query("SELECT 1 AS a, 2 AS a");
    /// var_dump($s->fetch(PDO::FETCH_NAMED), $s->fetch());
    /// $s = $db->query("SELECT 1 AS a");
    /// var_dump($s->fetch(PDO::FETCH_OBJ));
    /// try { $s->fetch(13); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(6) {
    ///   ["a"]=>
    ///   int(1)
    ///   [0]=>
    ///   int(1)
    ///   ["b"]=>
    ///   string(1) "x"
    ///   [1]=>
    ///   string(1) "x"
    ///   ["c"]=>
    ///   NULL
    ///   [2]=>
    ///   NULL
    /// }
    /// array(1) {
    ///   ["a"]=>
    ///   array(2) {
    ///     [0]=>
    ///     int(1)
    ///     [1]=>
    ///     int(2)
    ///   }
    /// }
    /// bool(false)
    /// object(stdClass)#3 (1) {
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// PDOStatement::fetch(): Argument #1 ($mode) must be a bitmask of PDO::FETCH_* constants
    /// ```
    #[php(tentative)]
    fn fetch(
        req: &mut Request,
        this: This<Self>,
        #[php(default = PDO::FETCH_DEFAULT)] mode: i64,
        #[php(default = PDO::FETCH_ORI_NEXT)] cursorOrientation: i64,
        #[php(default = 0)] cursorOffset: i64,
    ) -> Result<Owner<Value>, Thrown> {
        fetch::fetch(req, this.object(), mode, (cursorOrientation, cursorOffset))
    }

    /// `PDOStatement::fetchAll(int $mode = PDO::FETCH_DEFAULT, mixed ...$args): array`:
    /// every row, grouped or keyed where the bits ask.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->exec("CREATE TABLE t (k, v)");
    /// $db->exec("INSERT INTO t VALUES ('a', 1), ('a', 2), ('b', 3)");
    /// var_dump($db->query('SELECT v FROM t')->fetchAll(PDO::FETCH_COLUMN));
    /// var_dump($db->query('SELECT k, v FROM t')->fetchAll(PDO::FETCH_KEY_PAIR));
    /// var_dump($db->query('SELECT k, v FROM t')->fetchAll(PDO::FETCH_COLUMN | PDO::FETCH_GROUP));
    /// var_dump($db->query('SELECT k, v FROM t')->fetchAll(PDO::FETCH_FUNC, fn ($k, $v) => "$k$v"));
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    ///   [2]=>
    ///   int(3)
    /// }
    /// array(2) {
    ///   ["a"]=>
    ///   int(2)
    ///   ["b"]=>
    ///   int(3)
    /// }
    /// array(2) {
    ///   ["a"]=>
    ///   array(2) {
    ///     [0]=>
    ///     int(1)
    ///     [1]=>
    ///     int(2)
    ///   }
    ///   ["b"]=>
    ///   array(1) {
    ///     [0]=>
    ///     int(3)
    ///   }
    /// }
    /// array(3) {
    ///   [0]=>
    ///   string(2) "a1"
    ///   [1]=>
    ///   string(2) "a2"
    ///   [2]=>
    ///   string(2) "b3"
    /// }
    /// ```
    ///
    /// `ZEND_PARSE_PARAMETERS_START(0, 3)` under the stub's variadic, so
    /// the body counts, then holds `$mode`. A fetch that fails gives the
    /// rows read so far back and answers the process's empty array.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);
    /// $s = $db->query('SELECT 1 UNION ALL SELECT abs(-9223372036854775807-1)');
    /// debug_zval_dump($s->fetchAll());
    /// var_dump($s->errorInfo()[2]);
    /// ```
    /// ```expect
    /// array(0) interned {
    /// }
    /// string(16) "integer overflow"
    /// ```
    #[php(tentative, arity = manual)]
    fn fetchAll(
        req: &mut Request,
        this: This<Self>,
        #[php(default = PDO::FETCH_DEFAULT)] mode: Raw<i64>,
        args: Variadic<Value>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        fetch::fetch_all(req, this.object(), mode, &args.0)
    }

    /// `PDOStatement::fetchColumn(int $column = 0): mixed`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query("SELECT 'a', 'b' UNION SELECT 'c', 'd'");
    /// var_dump($s->fetchColumn(1), $s->fetchColumn(), $s->fetchColumn());
    /// $s = $db->query('SELECT 1');
    /// try { $s->fetchColumn(3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "b"
    /// string(1) "c"
    /// bool(false)
    /// Invalid column index
    /// ```
    #[php(tentative)]
    fn fetchColumn(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] column: i64,
    ) -> Result<Owner<Value>, Thrown> {
        fetch::fetch_column(req, this.object(), column)
    }

    /// `PDOStatement::fetchObject(?string $class = "stdClass", array $constructorArgs = []): object|false`.
    ///
    /// ```php
    /// class P { public $a; function __construct(public $tag = 'none') {} }
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query('SELECT 1 AS a UNION SELECT 2');
    /// var_dump($s->fetchObject('P', ['t']), $s->fetchObject(), $s->fetchObject());
    /// try { $s->fetchObject('Nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(P)#3 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["tag"]=>
    ///   string(1) "t"
    /// }
    /// object(stdClass)#4 (1) {
    ///   ["a"]=>
    ///   int(2)
    /// }
    /// bool(false)
    /// PDOStatement::fetchObject(): Argument #1 ($class) must be a valid class name, Nope given
    /// ```
    #[php(tentative)]
    fn fetchObject(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "stdClass")] class: Option<PhpString>,
        #[php(default = [])] constructorArgs: Passed<PhpArray>,
    ) -> Result<OrFalse<Owner<PhpObject>>, Thrown> {
        let args = match constructorArgs {
            Passed::Given(args) => Some(args),
            Passed::Omitted => None,
        };
        fetch::fetch_object(req, this.object(), class, args)
    }

    /// `PDOStatement::getAttribute(int $name): mixed`: the connector's, then
    /// `generic_stmt_attr_get`'s `ATTR_EMULATE_PREPARES`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT 1');
    /// var_dump($s->getAttribute(PDO::ATTR_EMULATE_PREPARES));
    /// try { $s->getAttribute(PDO::ATTR_CURSOR); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(false)
    /// SQLSTATE[IM001]: Driver does not support this function: driver doesn't support getting that attribute
    /// ```
    #[php(tentative)]
    fn getAttribute(
        req: &mut Request,
        this: This<Self>,
        name: i64,
    ) -> Result<Owner<Value>, Thrown> {
        stmt_attribute(req, this, name)
    }

    /// `PDOStatement::getColumnMeta(int $column): array|false`: the
    /// connector's rows, then `name`, `len` and `precision`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->exec('CREATE TABLE t (a INTEGER)');
    /// $db->exec('INSERT INTO t VALUES (1)');
    /// $s = $db->query('SELECT a FROM t');
    /// var_dump($s->getColumnMeta(0));
    /// try { $s->getColumnMeta(1); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// try { $s->getColumnMeta(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(8) {
    ///   ["native_type"]=>
    ///   string(7) "integer"
    ///   ["pdo_type"]=>
    ///   int(1)
    ///   ["sqlite:decl_type"]=>
    ///   string(7) "INTEGER"
    ///   ["table"]=>
    ///   string(1) "t"
    ///   ["flags"]=>
    ///   array(0) {
    ///   }
    ///   ["name"]=>
    ///   string(1) "a"
    ///   ["len"]=>
    ///   int(-1)
    ///   ["precision"]=>
    ///   int(0)
    /// }
    /// SQLSTATE[HY000]: General error: 100 another row available
    /// PDOStatement::getColumnMeta(): Argument #1 ($column) must be greater than or equal to 0
    /// ```
    #[php(tentative)]
    fn getColumnMeta(
        req: &mut Request,
        this: This<Self>,
        column: i64,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        let (stmt, dbh) = this_stmt(req, this.object())?;
        let Ok(at) = usize::try_from(column) else {
            return Err(argument_error(
                req,
                "ValueError",
                1,
                "must be greater than or equal to 0",
            ));
        };
        clear_error(req, stmt);
        let executed = read(req, stmt, |s| s.executed).unwrap_or(false);
        let mut meta = match on_driver(req, stmt, |req, driver| {
            driver.column_meta(req, at, executed)
        })? {
            Ok(Some(meta)) => meta,
            Ok(None) => return Ok(OrFalse::False),
            Err(()) => {
                unsupported(req, stmt, dbh, "driver doesn't support meta data")?;
                return Ok(OrFalse::False);
            }
        };
        let info = read(req, stmt, |s| {
            let c = s.columns.as_ref()?.get(at)?;
            Some((*c.name, c.max_len, c.precision))
        })
        .flatten();
        if let Some((name, max_len, precision)) = info {
            // `add_assoc_str(..., zend_string_copy(col->name))`.
            meta.set_str(req, b"name", Owner::take(req, name).into_value())?;
            // php's `add_assoc_long(..., col->maxlen)` over a `size_t`.
            #[allow(clippy::cast_possible_wrap)]
            meta.set_str(req, b"len", Owner::int(max_len as i64))?;
            #[allow(clippy::cast_possible_wrap)]
            meta.set_str(req, b"precision", Owner::int(precision as i64))?;
        }
        Ok(OrFalse::Value(meta))
    }

    /// `PDOStatement::nextRowset(): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query('SELECT 1');
    /// try { $s->nextRowset(); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// SQLSTATE[IM001]: Driver does not support this function: driver does not support multiple rowsets
    /// ```
    #[php(tentative)]
    fn nextRowset(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let (stmt, dbh) = this_stmt(req, this.object())?;
        let reset = write(req, stmt, |s| {
            s.column_count = 0;
            s.columns.take()
        });
        if let Some(columns) = reset.flatten() {
            columns.free(req);
        }
        clear_error(req, stmt);
        match on_driver(req, stmt, |req, driver| driver.next_rowset(req))? {
            Ok(Some(done)) => {
                set_column_count(req, stmt, done.columns);
                if let Some(rows) = done.rows {
                    write(req, stmt, |s| s.row_count = rows);
                }
                describe_columns(req, stmt)?;
                Ok(true)
            }
            Ok(None) => {
                write(req, stmt, |s| s.executed = false);
                Ok(false)
            }
            Err(()) => unsupported(req, stmt, dbh, "driver does not support multiple rowsets")
                .map(|()| false),
        }
    }

    /// `PDOStatement::rowCount(): int`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->exec('CREATE TABLE t (a)');
    /// $s = $db->prepare('INSERT INTO t VALUES (1), (2), (3)');
    /// var_dump($s->rowCount()); $s->execute(); var_dump($s->rowCount());
    /// ```
    /// ```expect
    /// int(0)
    /// int(3)
    /// ```
    #[php(tentative)]
    fn rowCount(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let (stmt, _) = this_stmt(req, this.object())?;
        Ok(read(req, stmt, |s| s.row_count).unwrap_or(0))
    }

    /// `PDOStatement::setAttribute(int $attribute, mixed $value): bool`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->prepare('SELECT 1');
    /// var_dump($s->setAttribute(PDO::ATTR_CURSOR, 1));
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn setAttribute(
        req: &mut Request,
        this: This<Self>,
        attribute: i64,
        value: Value,
    ) -> Result<bool, Thrown> {
        let attr = attribute;
        let (stmt, dbh) = this_stmt(req, this.object())?;
        clear_error(req, stmt);
        match on_driver(req, stmt, |req, driver| {
            driver.set_attribute(req, attr, &value)
        })? {
            Ok(Some(())) => Ok(true),
            Ok(None) => Ok(false),
            Err(()) => unsupported(
                req,
                stmt,
                dbh,
                "This driver doesn't support setting attributes",
            )
            .map(|()| false),
        }
    }

    /// `PDOStatement::setFetchMode(int $mode, mixed ...$args): true`.
    ///
    /// ```php
    /// class Row { public $a; }
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query('SELECT 1 AS a');
    /// var_dump($s->setFetchMode(PDO::FETCH_CLASS, 'Row'));
    /// var_dump($s->fetch());
    /// try { $s->setFetchMode(PDO::FETCH_COLUMN); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// $o = new stdClass;
    /// $s = $db->query('SELECT 2 AS b');
    /// $s->setFetchMode(PDO::FETCH_INTO, $o); $s->fetch(); var_dump($o);
    /// ```
    /// ```expect
    /// bool(true)
    /// object(Row)#3 (1) {
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// PDOStatement::setFetchMode() expects exactly 2 arguments for the fetch mode provided, 1 given
    /// object(stdClass)#4 (1) {
    ///   ["b"]=>
    ///   int(2)
    /// }
    /// ```
    #[php(tentative)]
    fn setFetchMode(
        req: &mut Request,
        this: This<Self>,
        mode: i64,
        args: Variadic<Value>,
    ) -> Result<True, Thrown> {
        fetch::set_fetch_mode(req, this.object(), mode, &args.0)
    }

    /// `PDOStatement::getIterator(): Iterator`: an `InternalIterator` over
    /// the statement's own iterator ([`Handlers`]'s `get_iterator`), its
    /// first row fetched once that iterator has its number in the object
    /// store, so a `PDORow` the fetch makes takes the number after it.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $s = $db->query('SELECT 1 AS n UNION SELECT 2');
    /// $it = $s->getIterator();
    /// var_dump(get_class($it));
    /// foreach ($it as $k => $row) echo $k, ':', $row['n'], "\n";
    /// class S extends PDOStatement { function __construct() {} }
    /// try { foreach (new S as $row) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(16) "InternalIterator"
    /// 0:1
    /// 1:2
    /// S object is uninitialized
    /// ```
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// foreach ($db->query('SELECT 1 AS n', PDO::FETCH_LAZY) as $row) var_dump(spl_object_id($row));
    /// ```
    /// ```expect
    /// int(4)
    /// ```
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<Iterator>>, Thrown> {
        let stmt = this.object();
        handle(req, stmt)?;
        Ok(internal_iterator(req, &stmt)?.into_instance())
    }
}

/// `debugDumpParams`'s text: the query, the query sent, and each bound
/// parameter.
fn dump_params(s: &Stmt) -> Vec<u8> {
    let mut out = format!("SQL: [{}] ", s.query().len()).into_bytes();
    out.extend_from_slice(s.query());
    out.push(b'\n');
    if let Active::Sent(sent) = &s.active {
        out.extend_from_slice(format!("Sent SQL: [{}] ", sent.len()).as_bytes());
        out.extend_from_slice(sent.as_bytes());
        out.push(b'\n');
    }
    let params = s.params.as_ref();
    out.extend_from_slice(format!("Params:  {}\n", params.map_or(0, BoundTable::len)).as_bytes());
    for bound in params.into_iter().flat_map(BoundTable::iter) {
        match &bound.key() {
            BoundKey::Name(name) => {
                out.extend_from_slice(format!("Key: Name: [{}] ", name.len()).as_bytes());
                out.extend_from_slice(name);
                out.push(b'\n');
            }
            BoundKey::Position(n) => {
                out.extend_from_slice(format!("Key: Position #{n}:\n").as_bytes());
            }
        }
        let name = bound.name_bytes().unwrap_or_default();
        out.extend_from_slice(
            format!("paramno={}\nname=[{}] \"", bound.position, name.len()).as_bytes(),
        );
        out.extend_from_slice(name);
        out.extend_from_slice(
            format!(
                "\"\nis_param={}\nparam_type={}\n",
                i32::from(bound.is_parameter),
                bound.ty.raw()
            )
            .as_bytes(),
        );
    }
    out
}

/// What `PDOStatement::getAttribute` answers.
fn stmt_attribute(
    req: &mut Request,
    this: This<PDOStatement>,
    name: i64,
) -> Result<Owner<Value>, Thrown> {
    let attr = name;
    let (stmt, dbh) = this_stmt(req, this.object())?;
    let generic = |req: &Request| {
        (attr == PDO::ATTR_EMULATE_PREPARES).then(|| {
            Value::bool(
                read(req, stmt, |s| {
                    s.placeholders == Some(Placeholders::Emulated)
                })
                .unwrap_or(false),
            )
        })
    };
    clear_error(req, stmt);
    let answered = match on_driver(req, stmt, |req, driver| driver.attribute(req, attr))? {
        Ok(Some(Some(value))) => return Ok(value),
        Ok(Some(None)) => match generic(req) {
            Some(value) => Ok(value),
            None => unsupported(
                req,
                stmt,
                dbh,
                "driver doesn't support getting that attribute",
            )
            .map(|()| Value::bool(false)),
        },
        Ok(None) => Ok(Value::bool(false)),
        Err(()) => match generic(req) {
            Some(value) => Ok(value),
            None => unsupported(
                req,
                stmt,
                dbh,
                "This driver doesn't support getting attributes",
            )
            .map(|()| Value::bool(false)),
        },
    };
    Ok(Owner::take(req, answered?))
}
