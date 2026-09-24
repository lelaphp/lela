//! `pdo_stmt.c` less its fetch half ([`crate::fetch`]): the
//! `PDOStatement` class, its statement `pdo_stmt_t` as the object's
//! native payload ([`Stmt`]), the handlers `pdo_stmt_init` installs, the
//! bound parameters and columns with the connector's binding callback, and
//! the methods that start, bind and describe.

use php_ext::abi::{CloneObj, CloneObjWith, ObjectHandlers, PropertyPurpose, Std};
use php_ext::call::internal_iterator;
use php_ext::coerce::to_string;
use php_ext::raising::{Thrown, argument_error, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::connector::{
    BindEvent, BindKind, BindType, Binding, Cause, Failure, Placeholders, Prepared, SqlState,
    Statement,
};
use crate::dbh::{self, Dbh};
use crate::error::{self, On};
use crate::fetch::{self, Target};
use crate::{Entry, arity, long_arg, this};

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

/// The fetch the statement's iterator made ahead, `fetch_ahead` of
/// `struct php_pdo_iterator`: the element at `index` steps past the
/// start with its key, or nothing more to walk.
#[derive(Clone, Copy, Default)]
pub(crate) enum Ahead {
    #[default]
    Unset,
    Row {
        index: usize,
        key: i64,
        value: Value,
    },
    Done,
}

/// `pdo_stmt_t`, the payload of a `PDOStatement` and of every subclass.
#[derive(Default)]
pub(crate) struct Stmt {
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
    pub(crate) ahead: Ahead,
}

/// `pdo_dbstmt_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
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
        let ahead = std::mem::take(&mut stmt.ahead);
        let dbh = std::mem::take(&mut stmt.dbh);
        let mut driver = match slot {
            Slot::Ready(driver) => Some(driver),
            Slot::None | Slot::Lent => None,
        };
        for bound in params.into_iter().chain(columns) {
            free_bound(req, driver.as_deref_mut(), bound);
        }
        drop(driver);
        target.release(req);
        if let Ahead::Row { value, .. } = ahead {
            req.release(value);
        }
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

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn clone_obj_with(&self) -> Option<CloneObjWith> {
        None
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        let _ = (req, left, right);
        Ok(None)
    }

    /// `dbstmt_prop_write`: `queryString` once written is read only.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        if name.as_bytes() == b"queryString"
            && object
                .slot(0)
                .is_some_and(|v| !matches!(v.as_ref(), ValueRef::Undef))
        {
            req.release(value);
            return Err(throw(req, "Error", "Property queryString is read only"));
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
            return Err(throw(req, "Error", "Property queryString is read only"));
        }
        Std.unset_property(req, object, name)
    }

    /// `pdo_stmt_iter_funcs` read through an `InternalIterator`: the row
    /// fetched ahead at `index`, the next one fetched when the iterator
    /// has moved past it.
    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<(Value, Value)>, Thrown> {
        let ahead = object
            .native::<Stmt>(req)
            .map(|s| s.ahead)
            .unwrap_or_default();
        let (key, value) = match ahead {
            Ahead::Row {
                index: at,
                key,
                value,
            } if at == index => (key, value),
            Ahead::Row { index: at, key, .. } if at + 1 == index => {
                match fetch_ahead(req, object, index, key + 1)? {
                    Some(row) => row,
                    None => return Ok(None),
                }
            }
            Ahead::Unset if index == 0 => match fetch_ahead(req, object, 0, 0)? {
                Some(row) => row,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        req.addref(&value);
        Ok(Some((Value::int(key), value)))
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        let _ = (req, object, purpose);
        Ok(None)
    }
}

/// `pdo_stmt_iter_move_forwards`: the element fetched ahead given up
/// and the next row fetched with the default style, its error handled on
/// the statement where there is none.
fn fetch_ahead(
    req: &mut Request,
    object: PhpObject,
    index: usize,
    key: i64,
) -> Result<Option<(i64, Value)>, Thrown> {
    let old = object
        .native_mut::<Stmt>(req)
        .map(|s| std::mem::replace(&mut s.ahead, Ahead::Done));
    if let Some(Ahead::Row { value, .. }) = old {
        req.release(value);
    }
    if let Some(value) = fetch::row(req, object, 0)? {
        if let Some(s) = object.native_mut::<Stmt>(req) {
            s.ahead = Ahead::Row { index, key, value };
        }
        Ok(Some((key, value)))
    } else {
        handle_error(req, object, false)?;
        Ok(None)
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
        let _ = driver.bind(req, param, BindEvent::Free);
    }
    req.release(bound.value);
    req.release(bound.driver_params);
}

/// The methods, `Class::method` to entry, for the stub reader.
pub(crate) const METHODS: [(&str, Entry); 20] = [
    ("PDOStatement::bindColumn", bind_column),
    ("PDOStatement::bindParam", bind_param),
    ("PDOStatement::bindValue", bind_value),
    ("PDOStatement::closeCursor", close_cursor),
    ("PDOStatement::columnCount", column_count),
    ("PDOStatement::debugDumpParams", debug_dump_params),
    ("PDOStatement::errorCode", error_code),
    ("PDOStatement::errorInfo", error_info),
    ("PDOStatement::execute", execute),
    ("PDOStatement::fetch", fetch::fetch),
    ("PDOStatement::fetchAll", fetch::fetch_all),
    ("PDOStatement::fetchColumn", fetch::fetch_column),
    ("PDOStatement::fetchObject", fetch::fetch_object),
    ("PDOStatement::getAttribute", get_attribute),
    ("PDOStatement::getColumnMeta", get_column_meta),
    ("PDOStatement::nextRowset", next_rowset),
    ("PDOStatement::rowCount", row_count),
    ("PDOStatement::setAttribute", set_attribute),
    ("PDOStatement::setFetchMode", fetch::set_fetch_mode),
    ("PDOStatement::getIterator", get_iterator),
];

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
    Err(throw(
        req,
        "Error",
        &format!("{class} object is uninitialized"),
    ))
}

/// `$this` with `PHP_STMT_GET_OBJ`.
pub(super) fn this_stmt(req: &mut Request) -> Result<(PhpObject, PhpObject), Thrown> {
    let stmt = this(req)?;
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
        .and_then(|dbh| dbh.native::<Dbh>(req).map(|d| d.case))
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
    let executed = read(req, object, |s| s.executed).unwrap_or(false);
    let ran =
        with_driver(req, object, |req, driver| driver.execute(req, executed)).map_err(Some)?;
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
            .and_then(|dbh| dbh.native::<Dbh>(req).map(|d| d.describes_at_execute))
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
        (BindKind::Str, _) if bound.max_len <= 0 => Some(Value::string(to_string(req, &inner)?)),
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
        cell.set(req, new);
    } else {
        let old = std::mem::replace(value, new);
        req.release(old);
    }
}

/// `register_bound_param` and `bindValue()`'s body: the position or
/// name, checked; the value, a copy; the type; for a parameter or a
/// column bound by reference, the length and the connector's options.
fn bind(req: &mut Request, is_parameter: bool, by_ref: bool) -> Result<Value, Thrown> {
    if by_ref {
        arity(req, 2, 5)?;
    } else {
        arity(req, 2, 3)?;
    }
    let key = crate::arg(req, 1)?.unwrap_or_default();
    let value = req.argument(2).unwrap_or_default();
    let ty = BindType::of(long_arg(req, 3, 2)?);
    let max_len = if by_ref { long_arg(req, 4, 0)? } else { 0 };
    let driver_params = if by_ref {
        crate::arg(req, 5)?
            .filter(|v| !crate::is_null(v))
            .unwrap_or_default()
    } else {
        Value::null()
    };
    let (stmt, _) = this_stmt(req)?;
    let (name, position) = if let ValueRef::Str(name) = key.as_ref() {
        if name.is_empty() {
            return Err(argument_error(req, "ValueError", 1, "must not be empty"));
        }
        (Some(name.as_bytes().to_vec()), -1)
    } else {
        let n = key.as_int().unwrap_or(0);
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
    let value = if by_ref { value } else { value.deref() };
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
    register(req, stmt, bound).map(Value::bool)
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
fn bind_value(req: &mut Request) -> Result<Value, Thrown> {
    bind(req, true, false)
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
fn bind_param(req: &mut Request) -> Result<Value, Thrown> {
    bind(req, true, true)
}

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
fn bind_column(req: &mut Request) -> Result<Value, Thrown> {
    bind(req, false, true)
}

// ---- executing

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
fn execute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let input = crate::array_arg(req, 1)?;
    let (stmt, dbh) = this_stmt(req)?;
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
                return Ok(Value::bool(false));
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
            crate::parser::Parsed::Failed => return Ok(Value::bool(false)),
        }
    } else if !dispatch(req, stmt, BindEvent::ExecPre)? {
        handle_error(req, stmt, false)?;
        return Ok(Value::bool(false));
    }
    match run(req, stmt) {
        Ok(()) => {}
        Err(Some(thrown)) => return Err(thrown),
        Err(None) => {
            handle_error(req, stmt, false)?;
            return Ok(Value::bool(false));
        }
    }
    if !dispatch(req, stmt, BindEvent::ExecPost)? {
        handle_error(req, stmt, false)?;
        return Ok(Value::bool(false));
    }
    Ok(Value::bool(true))
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
fn row_count(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (stmt, _) = this_stmt(req)?;
    Ok(Value::int(read(req, stmt, |s| s.row_count).unwrap_or(0)))
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
fn column_count(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (stmt, _) = this_stmt(req)?;
    let count = read(req, stmt, |s| s.column_count).unwrap_or(0);
    Ok(Value::int(i64::try_from(count).unwrap_or(i64::MAX)))
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
fn error_code(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (stmt, _) = this_stmt(req)?;
    match read(req, stmt, |s| s.error_code).flatten() {
        Some(state) => crate::string(req, state.as_str().as_bytes()),
        None => Ok(Value::null()),
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
fn error_info(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (stmt, dbh) = this_stmt(req)?;
    let state = read(req, stmt, |s| s.error_code).flatten();
    let mut out = PhpArray::new(req)?;
    let made = crate::string(req, state.as_ref().map_or("", SqlState::as_str).as_bytes())?;
    out.append(req, made)?;
    if state != Some(SqlState::NONE) {
        error::fetch_error(req, dbh, &mut out)?;
    }
    while out.len() < 3 {
        out.append(req, Value::null())?;
    }
    Ok(Value::array(out))
}

// ---- attributes, metadata and cursors

/// `pdo_raise_impl_error(stmt->dbh, stmt, "IM001", supp)` and `false`.
fn unsupported(
    req: &mut Request,
    stmt: PhpObject,
    dbh: PhpObject,
    supp: &str,
) -> Result<Value, Thrown> {
    error::raise_impl(req, dbh, On::Stmt(stmt), SqlState(*b"IM001"), Some(supp))?;
    Ok(Value::bool(false))
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
fn set_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let attr = long_arg(req, 1, 0)?;
    let value = crate::arg(req, 2)?.unwrap_or_default();
    let (stmt, dbh) = this_stmt(req)?;
    clear_error(req, stmt);
    match on_driver(req, stmt, |req, driver| {
        driver.set_attribute(req, attr, &value)
    })? {
        Ok(Some(())) => Ok(Value::bool(true)),
        Ok(None) => Ok(Value::bool(false)),
        Err(()) => unsupported(
            req,
            stmt,
            dbh,
            "This driver doesn't support setting attributes",
        ),
    }
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
fn get_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let attr = long_arg(req, 1, 0)?;
    let (stmt, dbh) = this_stmt(req)?;
    let generic = |req: &Request| {
        (attr == dbh::ATTR_EMULATE_PREPARES).then(|| {
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
            ),
        },
        Ok(None) => Ok(Value::bool(false)),
        Err(()) => match generic(req) {
            Some(value) => Ok(value),
            None => unsupported(
                req,
                stmt,
                dbh,
                "This driver doesn't support getting attributes",
            ),
        },
    }
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
fn get_column_meta(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let column = long_arg(req, 1, 0)?;
    let (stmt, dbh) = this_stmt(req)?;
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
        Ok(None) => return Ok(Value::bool(false)),
        Err(()) => return unsupported(req, stmt, dbh, "driver doesn't support meta data"),
    };
    let info = read(req, stmt, |s| {
        s.columns.as_ref().and_then(|c| c.get(at).cloned())
    })
    .flatten();
    let mut meta = meta;
    if let Some(info) = info {
        let key = PhpString::new(req, b"name")?;
        let made = crate::string(req, &info.name)?;
        meta.set(req, Key::Str(key), made)?;
        let key = PhpString::new(req, b"len")?;
        // php's `add_assoc_long(..., col->maxlen)` over a `size_t`.
        #[allow(clippy::cast_possible_wrap)]
        meta.set(req, Key::Str(key), Value::int(info.max_len as i64))?;
        let key = PhpString::new(req, b"precision")?;
        #[allow(clippy::cast_possible_wrap)]
        meta.set(req, Key::Str(key), Value::int(info.precision as i64))?;
    }
    Ok(Value::array(meta))
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
fn next_rowset(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (stmt, dbh) = this_stmt(req)?;
    write(req, stmt, |s| {
        s.columns = None;
        s.column_count = 0;
    });
    clear_error(req, stmt);
    match on_driver(req, stmt, |req, driver| driver.next_rowset(req))? {
        Ok(Some(count)) => {
            set_column_count(req, stmt, count);
            describe_columns(req, stmt)?;
            Ok(Value::bool(true))
        }
        Ok(None) => {
            write(req, stmt, |s| s.executed = false);
            Ok(Value::bool(false))
        }
        Err(()) => unsupported(req, stmt, dbh, "driver does not support multiple rowsets"),
    }
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
fn close_cursor(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (stmt, _) = this_stmt(req)?;
    clear_error(req, stmt);
    match on_driver(req, stmt, |req, driver| driver.close_cursor(req))? {
        Ok(Some(())) => {}
        Ok(None) => return Ok(Value::bool(false)),
        Err(()) => {
            while with_driver(req, stmt, |req, driver| {
                driver.fetch(req, crate::connector::Orientation::Forward, 0)
            })?
            .unwrap_or(false)
            {}
        }
    }
    write(req, stmt, |s| s.executed = false);
    Ok(Value::bool(true))
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
fn debug_dump_params(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (stmt, _) = this_stmt(req)?;
    let Some((query, active, params)) = read(req, stmt, |s| {
        (s.query.clone(), s.active.clone(), s.params.clone())
    }) else {
        return Ok(Value::null());
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
    Ok(Value::null())
}

/// `PDOStatement::getIterator(): Iterator`: an `InternalIterator` over
/// the statement, its first row fetched now, as `pdo_stmt_iter_get`
/// fetches it.
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
fn get_iterator(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let stmt = this(req)?;
    handle(req, stmt)?;
    fetch_ahead(req, stmt, 0, 0)?;
    internal_iterator(req, &stmt).map(|it| it.as_value())
}
