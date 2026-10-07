//! `pdo_stmt.c` less its fetch half ([`crate::fetch`]): the
//! `PDOStatement` class, its statement `pdo_stmt_t` as the object's
//! native payload ([`Stmt`]), the handlers `pdo_stmt_init` installs, the
//! bound parameters and columns with the connector's binding callback, and
//! the methods that start, bind and describe.

use php_ext::abi::{
    ClassIterator, Instance, ObjectHandlers, OrFalse, Passed, PropertiesFor, PropertyPurpose, Raw,
    Ref, Refusals, Std, This, True, Union, Variadic,
};
use php_ext::attr::php_class;
use php_ext::call::internal_iterator;
use php_ext::coerce::to_string;
use php_ext::raising::{Thrown, argument_error, throw_error};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::connector::{
    BindEvent, BindKind, BindType, Binding, Cause, Failure, Placeholders, Prepared, SqlState,
    Statement,
};
use crate::dbh::{self, PDO};
use crate::error::{self, On};
use crate::fetch::{self, Target};

/// How the statement reaches its connection: `stmt->dbh` with the owner
/// `database_object_handle` takes, or, for the statement a failed
/// `query()` left on the connection, `stmt->dbh` alone, the connection holding
/// the statement and not the other way round.
#[derive(Clone, Copy, Default)]
pub(crate) enum DbLink {
    /// A statement no connection made, which `PHP_STMT_GET_OBJ` refuses.
    #[default]
    None,
    Owned(PhpObject),
    Held(PhpObject),
}

impl DbLink {
    pub(crate) fn object(self) -> Option<PhpObject> {
        match self {
            DbLink::None => None,
            DbLink::Owned(dbh) | DbLink::Held(dbh) => Some(dbh),
        }
    }
}

/// `stmt->active_query_string`: none, the query itself (`query()`'s,
/// which `debugDumpParams()` does not print again), or the query as
/// PDO rewrote it for an emulating connector.
#[derive(Clone, Default)]
pub(crate) enum Active {
    #[default]
    None,
    Same,
    Sent(Vec<u8>),
}

/// The key a bound parameter or column is held under: its position or
/// its name, php's `HashTable` over both.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum BoundKey {
    Position(i64),
    Name(Vec<u8>),
}

/// `struct pdo_bound_param_data`.
#[derive(Clone)]
pub(crate) struct Bound {
    pub(crate) key: BoundKey,
    /// `paramno`, `-1` for a name not yet placed.
    pub(crate) position: i64,
    pub(crate) name: Option<Vec<u8>>,
    pub(crate) ty: BindType,
    /// `parameter`, one owner: a reference for `bindParam()` and
    /// `bindColumn()`.
    pub(crate) value: Value,
    /// `driver_params`, one owner, `null` for none.
    pub(crate) driver_params: Value,
    pub(crate) max_len: i64,
    pub(crate) is_parameter: bool,
}

/// `struct pdo_column_data`.
#[derive(Clone)]
pub(crate) struct ColumnInfo {
    pub(crate) name: Vec<u8>,
    pub(crate) max_len: u64,
    pub(crate) precision: u64,
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
    /// `stmt->query_string`.
    pub(crate) query: Vec<u8>,
    pub(crate) active: Active,
    /// `stmt->executed`.
    pub(crate) executed: bool,
    /// `stmt->in_fetch`.
    pub(crate) in_fetch: bool,
    placeholders: Option<Placeholders>,
    /// `stmt->bound_params`, in the table's order.
    pub(crate) params: Vec<Bound>,
    /// The statement's own native code and text, for a connector that
    /// keeps them per statement (`Connector::errors_per_statement`).
    pub(crate) native_error: Option<(i64, Vec<u8>)>,
    /// `stmt->bound_param_map`: for a rewritten query, each placeholder's
    /// number with the name it was rewritten from or to.
    pub(crate) param_map: Option<Vec<(Key, Vec<u8>)>>,
    /// `stmt->bound_columns`.
    pub(crate) columns_bound: Vec<Bound>,
    /// `stmt->columns`, `None` until described.
    pub(crate) columns: Option<Vec<ColumnInfo>>,
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
        let params = std::mem::take(&mut stmt.params);
        let columns = std::mem::take(&mut stmt.columns_bound);
        let slot = std::mem::take(&mut stmt.slot);
        let target = std::mem::take(&mut stmt.target);
        let dbh = std::mem::take(&mut stmt.dbh);
        let mut driver = match slot {
            Slot::Ready(driver) => Some(driver),
            Slot::None | Slot::Lent => None,
        };
        for bound in params.into_iter().chain(columns) {
            free_bound(req, driver.as_deref_mut(), bound);
        }
        if let Some(mut driver) = driver {
            driver.close(req);
        }
        target.release(req);
        if let DbLink::Owned(dbh) = dbh {
            req.release(dbh.as_value());
        }
    }

    /// `dbstmt_get_gc`: the connection, and what the default fetch style
    /// holds.
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(stmt) = object.native::<Stmt>(req) {
            if let DbLink::Owned(dbh) = stmt.dbh {
                visit(dbh.as_value());
            }
            stmt.target.visit(visit);
        }
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
            started: false,
            key: None,
            ahead: None,
        })))
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        let _ = (req, object, purpose);
        Ok(PropertiesFor::Standard)
    }
}

/// `struct php_pdo_iterator`: the key and the row fetched ahead, `None`
/// once a fetch found none; the first is fetched at the first step, as
/// `pdo_stmt_iter_get` fetches it after `zend_iterator_init`.
struct Rows {
    started: bool,
    key: Option<i64>,
    ahead: Option<Value>,
}

impl Rows {
    /// `pdo_stmt_iter_move_forwards`.
    fn forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.started = true;
        if let Some(old) = self.ahead.take() {
            req.release(old);
        }
        self.ahead = fetch::row(req, object, 0)?;
        if self.ahead.is_none() {
            self.key = None;
            return handle_error(req, object, false);
        }
        self.key = Some(self.key.map_or(0, |key| key + 1));
        Ok(())
    }

    fn start(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        if self.started {
            return Ok(());
        }
        self.forward(req, object)
    }
}

impl ClassIterator for Rows {
    /// No `rewind` in `pdo_stmt_iter_funcs`: the first step alone fetches.
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.start(req, object)
    }

    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
        self.start(req, object)?;
        Ok(self.ahead.is_some())
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        self.start(req, object)?;
        let row = self.ahead.unwrap_or_default();
        Ok(Owner::take(req, row))
    }

    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        self.start(req, object)?;
        let key = self.key.map_or_else(Value::null, Value::int);
        Ok(Owner::take(req, key))
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.forward(req, object)
    }

    fn release(&mut self, req: &mut Request, _: PhpObject) {
        if let Some(row) = self.ahead.take() {
            req.release(row);
        }
    }
}

/// `param_dtor`: the connector told through `FREE`, and what the entry
/// holds given up.
fn free_bound(req: &mut Request, driver: Option<&mut dyn Statement>, mut bound: Bound) {
    if let Some(driver) = driver {
        let param = Binding {
            position: &mut bound.position,
            name: bound.name.as_deref(),
            ty: bound.ty,
            is_parameter: bound.is_parameter,
            value: &mut bound.value,
        };
        driver.bind(req, param, BindEvent::Free).ok();
    }
    req.release(bound.value);
    req.release(bound.driver_params);
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
    let answered = f(req, &mut *driver);
    write(req, object, |s| s.slot = Slot::Ready(driver));
    Ok(answered)
}

/// A prepared statement's connector half laid down on the new statement.
pub(super) fn attach(req: &mut Request, object: PhpObject, prepared: Prepared) {
    write(req, object, |s| {
        s.slot = Slot::Ready(prepared.statement);
        s.placeholders = Some(prepared.placeholders);
        if let Some(map) = prepared.parameter_map {
            s.param_map = Some(
                map.into_iter()
                    .map(|(n, name)| (Key::Int(n), name))
                    .collect(),
            );
        }
    });
}

/// `php_pdo_stmt_set_column_count`: the described columns kept while
/// the count stands, dropped where it changed.
pub(super) fn set_column_count(req: &mut Request, object: PhpObject, count: usize) {
    write(req, object, |s| {
        if s.columns.is_some() && s.column_count != count {
            s.columns = None;
        }
        s.column_count = count;
    });
}

/// `pdo_stmt_describe_columns`: each column described by the connector,
/// its name folded to the connection's case, and a column bound by name
/// placed at its position; `false` where the connector failed.
pub(super) fn describe_columns(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let count = read(req, object, |s| s.column_count).unwrap_or(0);
    let case = dbh_of(req, object)
        .and_then(|dbh| crate::dbh::state(req, dbh).map(|d| d.case))
        .unwrap_or_default();
    write(req, object, |s| s.columns = Some(Vec::with_capacity(count)));
    for column in 0..count {
        let described = with_driver(req, object, |req, driver| driver.describe(req, column))?;
        let mut info = match described {
            Ok(info) => info,
            Err(failure) => {
                record(req, object, failure)?;
                return Ok(false);
            }
        };
        match case {
            dbh::Case::Lower => info.name.make_ascii_lowercase(),
            dbh::Case::Upper => info.name.make_ascii_uppercase(),
            dbh::Case::Natural => {}
        }
        write(req, object, |s| {
            if let Some(bound) = s
                .columns_bound
                .iter_mut()
                .find(|b| b.key == BoundKey::Name(info.name.clone()))
            {
                bound.position = i64::try_from(column).unwrap_or(-1);
            }
            if let Some(columns) = s.columns.as_mut() {
                columns.push(ColumnInfo {
                    name: info.name,
                    max_len: info.max_len,
                    precision: info.precision,
                });
            }
        });
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
        let len = read(req, object, |s| {
            if columns {
                s.columns_bound.len()
            } else {
                s.params.len()
            }
        })
        .unwrap_or(0);
        for at in 0..len {
            let Some(bound) = read(req, object, |s| {
                let list = if columns { &s.columns_bound } else { &s.params };
                list.get(at).cloned()
            })
            .flatten() else {
                continue;
            };
            let Bound {
                mut position,
                name,
                ty,
                is_parameter,
                mut value,
                ..
            } = bound;
            let answered = with_driver(req, object, |req, driver| {
                let param = Binding {
                    position: &mut position,
                    name: name.as_deref(),
                    ty,
                    is_parameter,
                    value: &mut value,
                };
                driver.bind(req, param, event)
            })?;
            write(req, object, |s| {
                let list = if columns {
                    &mut s.columns_bound
                } else {
                    &mut s.params
                };
                if let Some(slot) = list.get_mut(at) {
                    slot.position = position;
                    slot.value = value;
                }
            });
            if let Err(failure) = answered {
                record(req, object, failure)?;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// The statement's connector started, `stmt->methods->executer`, and on
/// its first start the columns described where the connection allocates its own:
/// `Err(None)` for a failure recorded on the statement and not yet
/// handled.
pub(super) fn run(req: &mut Request, object: PhpObject) -> Result<(), Option<Thrown>> {
    let (executed, query) = read(req, object, |s| {
        let query = match &s.active {
            Active::Sent(sent) => sent.clone(),
            Active::None | Active::Same => s.query.clone(),
        };
        (s.executed, query)
    })
    .unwrap_or_default();
    let ran = with_driver(req, object, |req, driver| {
        driver.execute(req, executed, &query)
    })
    .map_err(Some)?;
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
    if let Err(thrown) = convert(req, &mut bound) {
        req.release(bound.value);
        req.release(bound.driver_params);
        return Err(thrown);
    }
    if !bound.is_parameter
        && let Some(name) = bound.name.clone()
        && let Some(columns) = read(req, object, |s| s.columns.clone()).flatten()
    {
        if let Some(at) = columns.iter().position(|c| c.name == name) {
            bound.position = i64::try_from(at).unwrap_or(-1);
        }
        if bound.position == -1
            && let Some(dbh) = dbh_of(req, object)
        {
            let supp = format!(
                "Did not find column name '{}' in the defined columns; it will not be bound",
                String::from_utf8_lossy(&name)
            );
            error::raise_impl(req, dbh, On::Stmt(object), SqlState::GENERAL, Some(&supp))?;
        }
    }
    if bound.is_parameter
        && let Some(name) = bound.name.as_mut()
        && name.first() != Some(&b':')
    {
        name.insert(0, b':');
    }
    if bound.is_parameter && !crate::parser::name_to_position(req, object, &mut bound)? {
        free_bound(req, None, bound);
        return Ok(false);
    }
    if !hook(req, object, &mut bound, BindEvent::Normalize)? {
        handle_error(req, object, false)?;
        free_bound(req, None, bound);
        return Ok(false);
    }
    bound.key = match &bound.name {
        Some(name) => BoundKey::Name(name.clone()),
        None => BoundKey::Position(bound.position),
    };
    let is_parameter = bound.is_parameter;
    let position = bound.position;
    let removed = write(req, object, |s| {
        let list = if is_parameter {
            &mut s.params
        } else {
            &mut s.columns_bound
        };
        let mut removed = Vec::new();
        if position >= 0
            && let Some(at) = list
                .iter()
                .position(|b| b.key == BoundKey::Position(position))
        {
            removed.push(list.remove(at));
        }
        match list.iter().position(|b| b.key == bound.key) {
            Some(at) => removed.push(std::mem::replace(&mut list[at], bound.clone())),
            None => list.push(bound.clone()),
        }
        removed
    })
    .unwrap_or_default();
    for old in removed {
        free_bound_through(req, object, old)?;
    }
    if !hook(req, object, &mut bound, BindEvent::Alloc)? {
        handle_error(req, object, false)?;
        let key = bound.key.clone();
        let taken = write(req, object, |s| {
            let list = if is_parameter {
                &mut s.params
            } else {
                &mut s.columns_bound
            };
            list.iter()
                .position(|b| b.key == key)
                .map(|at| list.remove(at))
        })
        .flatten();
        if let Some(taken) = taken {
            free_bound_through(req, object, taken)?;
        }
        return Ok(false);
    }
    Ok(true)
}

/// The value converted in place where its type asks,
/// `really_register_bound_param`'s `try_convert_to_string`,
/// `convert_to_long` and `convert_to_boolean`.
fn convert(req: &mut Request, bound: &mut Bound) -> Result<(), Thrown> {
    let inner = bound.value.deref();
    let converted = match (bound.ty.kind(), inner.as_ref()) {
        (BindKind::Str, ValueRef::Null | ValueRef::Str(_)) => None,
        (BindKind::Str, _) if bound.max_len <= 0 => {
            Some(Value::string(to_string(req, &inner)?.hand_on_unmigrated()))
        }
        (BindKind::Int, ValueRef::Bool(b)) => Some(Value::int(i64::from(b))),
        (BindKind::Bool, ValueRef::Int(n)) => Some(Value::bool(n != 0)),
        _ => None,
    };
    if let Some(converted) = converted {
        replace_inner(req, &mut bound.value, converted);
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
        let param = Binding {
            position: &mut bound.position,
            name: bound.name.as_deref(),
            ty: bound.ty,
            is_parameter: bound.is_parameter,
            value: &mut bound.value,
        };
        driver.bind(req, param, event)
    })?;
    match answered {
        Ok(()) => Ok(true),
        Err(failure) => {
            record(req, object, failure)?;
            Ok(false)
        }
    }
}

/// The value a bound entry holds replaced by `new`, through the
/// reference where the entry holds one, as php's `convert_to_*` writes
/// in place.
pub(super) fn replace_inner(req: &mut Request, value: &mut Value, new: Value) {
    if let Some(cell) = value.as_php_ref().copied() {
        cell.convert(req, new);
    } else {
        let old = std::mem::replace(value, new);
        req.release(old);
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
        (Some(name.as_bytes().to_vec()), -1)
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
    req.addref(&value);
    req.addref(&driver_params);
    let bound = Bound {
        key: BoundKey::Position(position),
        position,
        name,
        ty,
        value,
        driver_params,
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
        let Some((query, active, params)) = read(req, stmt, |s| {
            (s.query.clone(), s.active.clone(), s.params.clone())
        }) else {
            return Ok(None);
        };
        let mut out = format!("SQL: [{}] ", query.len()).into_bytes();
        out.extend_from_slice(&query);
        out.push(b'\n');
        if let Active::Sent(sent) = active {
            out.extend_from_slice(format!("Sent SQL: [{}] ", sent.len()).as_bytes());
            out.extend_from_slice(&sent);
            out.push(b'\n');
        }
        out.extend_from_slice(format!("Params:  {}\n", params.len()).as_bytes());
        for bound in &params {
            match &bound.key {
                BoundKey::Name(name) => {
                    out.extend_from_slice(format!("Key: Name: [{}] ", name.len()).as_bytes());
                    out.extend_from_slice(name);
                    out.push(b'\n');
                }
                BoundKey::Position(n) => {
                    out.extend_from_slice(format!("Key: Position #{n}:\n").as_bytes());
                }
            }
            let name = bound.name.clone().unwrap_or_default();
            out.extend_from_slice(
                format!("paramno={}\nname=[{}] \"", bound.position, name.len()).as_bytes(),
            );
            out.extend_from_slice(&name);
            out.extend_from_slice(
                format!(
                    "\"\nis_param={}\nparam_type={}\n",
                    i32::from(bound.is_parameter),
                    bound.ty.raw()
                )
                .as_bytes(),
            );
        }
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
        out.append(req, made.hand_on_unmigrated())?;
        if state != Some(SqlState::NONE) {
            error::fetch_error(req, dbh, On::Stmt(stmt), &mut out)?;
        }
        while out.len() < 3 {
            out.append(req, Value::null())?;
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
            let old = write(req, stmt, |s| std::mem::take(&mut s.params)).unwrap_or_default();
            for bound in old {
                free_bound_through(req, stmt, bound)?;
            }
            for (key, value) in input.snapshot() {
                let (name, position) = match key {
                    Key::Str(name) => (Some(name.as_bytes().to_vec()), -1),
                    Key::Int(n) => (None, n),
                };
                let value = crate::copied(req, value);
                let bound = Bound {
                    key: BoundKey::Position(position),
                    position,
                    name,
                    ty: BindType::STR,
                    value,
                    driver_params: Value::null(),
                    max_len: 0,
                    is_parameter: true,
                };
                if !register(req, stmt, bound)? {
                    return Ok(false);
                }
            }
        }
        let emulated = read(req, stmt, |s| {
            s.placeholders == Some(Placeholders::Emulated)
        })
        .unwrap_or(false);
        if emulated {
            let query = read(req, stmt, |s| s.query.clone()).unwrap_or_default();
            match crate::parser::parse_params(req, stmt, dbh, &query)? {
                crate::parser::Parsed::Unchanged => {
                    write(req, stmt, |s| s.active = Active::Sent(s.query.clone()));
                }
                crate::parser::Parsed::Rewritten(sent) => {
                    write(req, stmt, |s| s.active = Active::Sent(sent));
                }
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
        let value = fetch::fetch(req, this.object(), mode, (cursorOrientation, cursorOffset))?;
        Ok(crate::owned(req, value))
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
        let value = fetch::fetch_column(req, this.object(), column)?;
        Ok(crate::owned(req, value))
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
        Ok(
            match fetch::fetch_object(req, this.object(), class, args)? {
                // The fetch engine's object comes with its one owner, the caller's.
                OrFalse::Value(object) => {
                    let owner = Owner::take(req, object);
                    req.release(object.as_value());
                    OrFalse::Value(owner)
                }
                OrFalse::False => OrFalse::False,
            },
        )
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
        let value = stmt_attribute(req, this, name)?;
        Ok(crate::owned(req, value))
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
        let meta = match on_driver(req, stmt, |req, driver| {
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
            s.columns.as_ref().and_then(|c| c.get(at).cloned())
        })
        .flatten();
        // The driver's table comes with its one owner, the caller's.
        let mut meta = {
            let owner = Owner::take(req, meta);
            req.release(Value::array(meta));
            owner
        };
        if let Some(info) = info {
            let key = PhpString::new(req, b"name")?.hand_on_unmigrated();
            let made = crate::string(req, &info.name)?;
            meta.set(req, Key::Str(key), made.hand_on_unmigrated())?;
            let key = PhpString::new(req, b"len")?.hand_on_unmigrated();
            // php's `add_assoc_long(..., col->maxlen)` over a `size_t`.
            #[allow(clippy::cast_possible_wrap)]
            meta.set(req, Key::Str(key), Value::int(info.max_len as i64))?;
            let key = PhpString::new(req, b"precision")?.hand_on_unmigrated();
            #[allow(clippy::cast_possible_wrap)]
            meta.set(req, Key::Str(key), Value::int(info.precision as i64))?;
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
        write(req, stmt, |s| {
            s.columns = None;
            s.column_count = 0;
        });
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

/// What `PDOStatement::getAttribute` answers, its one owner the
/// caller's.
fn stmt_attribute(req: &mut Request, this: This<PDOStatement>, name: i64) -> Result<Value, Thrown> {
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
    match on_driver(req, stmt, |req, driver| driver.attribute(req, attr))? {
        Ok(Some(Some(value))) => Ok(value),
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
    }
}
