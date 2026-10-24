//! `class mysqli_stmt`: a prepared statement, `MY_STMT` behind its
//! `MYSQLI_RESOURCE`, with the statement's functions of `mysqli_api.c`
//! and `mysqli_nonapi.c`, `mysqli_execute_query`, and the statement's
//! properties.

#![allow(non_camel_case_types)]

use php_ext::abi::{Instance, OrFalse, Ref, This, True, Union, Variadic};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{Emalloc, Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_mysqlnd::client::{FieldType, Mysqlnd, Stmt, StmtAttr, StmtFetch, StmtStatus};

use crate::link::{self, Need, mysqli};
use crate::props::{self, Props};
use crate::result::{AsIs, mysqli_result};
use crate::{
    On, Resource, affected, closed, long_or_string, not_initialized, report, report_conn,
    report_error, report_index, report_mode, string,
};

/// `sizeof(MY_STMT)`.
const MY_STMT: usize = 80;

/// The statement: closed (php's null `ptr`), made by `stmt_init`
/// (`MYSQLI_STATUS_INITIALIZED`), or prepared (`MYSQLI_STATUS_VALID`),
/// behind its `MYSQLI_RESOURCE`.
#[derive(Default)]
pub(crate) enum StmtObj {
    #[default]
    Closed,
    Made(Resource<MyStmt>),
    Prepared(Resource<MyStmt>),
}

/// `MY_STMT`, in the allocation php `ecalloc`s for it: the statement and,
/// under `MYSQLI_REPORT_INDEX`, its query, `estrdup`'d with its NUL.
pub(crate) struct MyStmt {
    stmt: Stmt,
    query: Option<Emalloc>,
    held: Emalloc,
}

impl MyStmt {
    /// `ecalloc(1, sizeof(MY_STMT))` and `mysql_stmt_init` of `conn`; the
    /// `Err` is the memory limit's fatal.
    fn init(req: &mut Request, conn: Mysqlnd) -> Result<MyStmt, Thrown> {
        let held = Emalloc::new(req, MY_STMT)?;
        Ok(MyStmt {
            stmt: Stmt::init(req, conn)?,
            query: None,
            held,
        })
    }

    /// The query kept for `MYSQLI_REPORT_INDEX`, without its NUL.
    fn query(&self) -> &[u8] {
        self.query
            .as_ref()
            .and_then(|q| q.bytes().split_last())
            .map_or(&[], |(_, query)| query)
    }

    /// `mysqli_stmt_close(stmt->stmt, implicit)`, then
    /// `php_clear_stmt_bind`'s `efree(stmt->query)` and `efree(stmt)`,
    /// whether or not the close was taken (php 8.5.12 no longer returns
    /// before them).
    fn close(self, req: &mut Request, implicit: bool) -> Result<bool, Thrown> {
        let closed = self.stmt.close(req, implicit);
        if let Some(query) = self.query {
            query.free(req);
        }
        self.held.free(req);
        closed
    }
}

/// `mysqli_stmt_class_entry`.
fn stmt_class(req: &mut Request) -> Result<php_ext::values::ClassId, Thrown> {
    lookup_class(req, b"mysqli_stmt")?
        .ok_or_else(|| fatal(req, "InternalError: mysqli_stmt is not linked"))
}

/// `MYSQLI_FETCH_RESOURCE_STMT` around one use: the statement lent to `f`
/// and put back, or php's `Error` for a closed one or one short of `need`.
pub(crate) fn with_stmt<R>(
    req: &mut Request,
    object: PhpObject,
    need: Need,
    f: impl FnOnce(&mut Request, &mut MyStmt) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let mut held = crate::payload::<StmtObj>(req, object)
        .map(std::mem::take)
        .unwrap_or_default();
    let ready = match &held {
        StmtObj::Closed => None,
        StmtObj::Made(_) => Some(need == Need::Made),
        StmtObj::Prepared(_) => Some(true),
    };
    let answered = match (ready, &mut held) {
        (None, _) | (Some(true), StmtObj::Closed) => Err(closed(req, object)),
        (Some(false), _) => Err(not_initialized(req, object)),
        (Some(true), StmtObj::Made(s) | StmtObj::Prepared(s)) => f(req, s),
    };
    if let Some(slot) = crate::payload::<StmtObj>(req, object)
        && matches!(slot, StmtObj::Closed)
    {
        *slot = held;
    }
    answered
}

/// `MYSQLI_REPORT_STMT_ERROR(stmt)`.
fn report_stmt(req: &mut Request, stmt: &Stmt) -> Result<(), Thrown> {
    if report_mode(req) & report::ERROR == 0 {
        return Ok(());
    }
    let status = stmt.status(req);
    report_error(req, status.errno, status.sqlstate, &status.error)
}

/// The statement's facts.
fn facts(req: &mut Request, on: On, need: Need) -> Result<StmtStatus, Thrown> {
    with_stmt(req, on.object, need, |req, s| Ok(s.stmt.status(req)))
}

/// Whether `query` is kept for `MYSQLI_REPORT_INDEX`.
fn keeps_query(req: &mut Request, query: &[u8]) -> bool {
    !query.is_empty() && report_mode(req) & report::INDEX != 0
}

/// A statement on a new `mysqli_stmt` in `state`, made of its resource.
fn wrap(
    req: &mut Request,
    state: fn(Resource<MyStmt>) -> StmtObj,
    stmt: MyStmt,
) -> Result<Owner<Instance<mysqli_stmt>>, Thrown> {
    let resource = Resource::new(req, stmt)?;
    let class = stmt_class(req)?;
    let object = new_object(req, class)?;
    if let Some(slot) = crate::payload::<StmtObj>(req, *object) {
        *slot = state(resource);
    }
    Ok(object.into_instance())
}

/// `mysqli_stmt_init`. A statement's `MY_STMT` and resource are the
/// request's from its construction to its close:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $m = memory_get_usage();
/// $s = $l->stmt_init();
/// echo memory_get_usage() - $m, "\n";
/// $s->prepare("SELECT ? AS a");
/// echo memory_get_usage() - $m, "\n";
/// $s->close();
/// echo memory_get_usage() - $m, "\n";
/// unset($s);
/// $s = new mysqli_stmt($l, "SELECT ? AS a");
/// echo memory_get_usage() - $m, "\n";
/// unset($s);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql
/// 5384
/// 21800
/// 256
/// 21800
/// 32
/// ```
pub(crate) fn stmt_init(
    req: &mut Request,
    on: On,
) -> Result<OrFalse<Owner<Instance<mysqli_stmt>>>, Thrown> {
    let conn = link::conn(req, on.object, Need::Open)?;
    let stmt = MyStmt::init(req, conn)?;
    wrap(req, StmtObj::Made, stmt).map(OrFalse::Value)
}

/// `mysqli_prepare`: a failed prepare's statement closed with the
/// connection's error kept through the close, and reported.
pub(crate) fn prepare(
    req: &mut Request,
    on: On,
    query: PhpString,
) -> Result<OrFalse<Owner<Instance<mysqli_stmt>>>, Thrown> {
    let conn = link::conn(req, on.object, Need::Open)?;
    let mut stmt = MyStmt::init(req, conn)?;
    if !stmt.stmt.prepare(req, query.as_bytes())? {
        conn.keeping_error(req, |req| stmt.close(req, false))?;
        report_conn(req, conn)?;
        return Ok(OrFalse::False);
    }
    if keeps_query(req, query.as_bytes()) {
        // `estrdup(query)`.
        let mut kept = Emalloc::new(req, query.len() + 1)?;
        if let Some(bytes) = kept.bytes_mut().get_mut(..query.len()) {
            bytes.copy_from_slice(query.as_bytes());
        }
        stmt.query = Some(kept);
    }
    wrap(req, StmtObj::Prepared, stmt).map(OrFalse::Value)
}

/// `mysqli_stmt_prepare`.
pub(crate) fn stmt_prepare(req: &mut Request, on: On, query: PhpString) -> Result<bool, Thrown> {
    let prepared = with_stmt(req, on.object, Need::Made, |req, s| {
        if !s.stmt.prepare(req, query.as_bytes())? {
            report_stmt(req, &s.stmt)?;
            return Ok(false);
        }
        Ok(true)
    })?;
    if prepared && let Some(slot) = crate::payload::<StmtObj>(req, on.object) {
        *slot = match std::mem::take(slot) {
            StmtObj::Made(s) | StmtObj::Prepared(s) => StmtObj::Prepared(s),
            StmtObj::Closed => StmtObj::Closed,
        };
    }
    Ok(prepared)
}

/// The type `mysqli_stmt_bind_param_do_bind` sends a specifier as.
fn bind_type(specifier: u8) -> Option<FieldType> {
    Some(match specifier {
        b'd' => FieldType::Double,
        b'i' => FieldType::LongLong,
        b'b' => FieldType::LongBlob,
        b's' => FieldType::VarString,
        _ => return None,
    })
}

/// A by-reference argument as the statement holds it: the caller's
/// reference with one more owner, the value where none was passed.
fn held(req: &mut Request, var: &Ref<Value>) -> Owner<Value> {
    let value = match var.reference() {
        Some(cell) => Value::reference(cell),
        None => var.get().copied().unwrap_or_default(),
    };
    Owner::take(req, value)
}

/// `mysqli_stmt_bind_param`.
pub(crate) fn bind_param(
    req: &mut Request,
    on: On,
    types: PhpString,
    vars: &[Ref<Value>],
) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        let types = types.as_bytes();
        if types.is_empty() {
            return Err(arg_value_error(req, on.at(2), "not be empty"));
        }
        if types.len() != vars.len() {
            return Err(throw_error(
                req,
                "ArgumentCountError",
                "The number of elements in the type definition string must match the number of bind variables",
            ));
        }
        if types.len() != s.stmt.parameter_count() as usize {
            return Err(throw_error(
                req,
                "ArgumentCountError",
                "The number of variables must match the number of parameters in the prepared statement",
            ));
        }
        let mut params: Vec<(Owner<Value>, FieldType)> = Vec::with_capacity(vars.len());
        for (specifier, var) in types.iter().zip(vars) {
            let Some(ty) = bind_type(*specifier) else {
                for (value, _) in params {
                    value.give_up(req);
                }
                return Err(arg_value_error(
                    req,
                    on.at(2),
                    "only contain the \"b\", \"d\", \"i\", \"s\" type specifiers",
                ));
            };
            params.push((held(req, var), ty));
        }
        let bound = s.stmt.bind_parameters(req, params)?;
        report_stmt(req, &s.stmt)?;
        Ok(bound)
    })
}

/// `mysqli_stmt_bind_result`.
pub(crate) fn bind_result(req: &mut Request, on: On, vars: &[Ref<Value>]) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        if vars.len() != s.stmt.field_count() as usize {
            return Err(throw_error(
                req,
                "ArgumentCountError",
                "Number of bind variables doesn't match number of fields in prepared statement",
            ));
        }
        let values = vars.iter().map(|var| held(req, var)).collect();
        s.stmt.bind_columns(req, values)
    })
}

/// `$params` of `execute` and `execute_query`, each bound as a string,
/// php's two refusals named at `position`.
fn bind_list(
    req: &mut Request,
    stmt: &mut Stmt,
    params: PhpArray,
    position: u32,
) -> Result<Option<bool>, Thrown> {
    if !params.is_list() {
        return Err(arg_value_error(req, position, "be a list array"));
    }
    let count = stmt.parameter_count() as usize;
    if params.len() != count {
        let tail = format!(
            "consist of exactly {count} elements, {} present",
            params.len()
        );
        return Err(arg_value_error(req, position, &tail));
    }
    // `ZEND_HASH_FOREACH_VAL(input_params, tmp)`
    // (`Z/ext/mysqli/mysqli_api.c:441`), read only: each value bound one
    // owner more, as `mysqlnd_stmt_bind_param` takes it.
    let values: Vec<(Owner<Value>, FieldType)> = walk::rows(req, params)
        .map(|(_, value)| (Owner::take(req, value.deref()), FieldType::VarString))
        .collect();
    Ok(Some(stmt.bind_parameters(req, values)?))
}

/// `mysqli_stmt_execute`.
pub(crate) fn execute(req: &mut Request, on: On, params: Option<PhpArray>) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        let mut answer = true;
        if let Some(params) = params
            && bind_list(req, &mut s.stmt, params, on.at(2))? == Some(false)
        {
            report_stmt(req, &s.stmt)?;
            answer = false;
        }
        if s.stmt.execute(req)? {
            answer &= true;
        } else {
            report_stmt(req, &s.stmt)?;
            answer = false;
        }
        if report_mode(req) & report::INDEX != 0 {
            let status = s.stmt.status(req).server_status;
            report_index(req, s.query(), status)?;
        }
        Ok(answer)
    })
}

/// `mysqli_execute_query`: prepared, bound, sent and the rows read, the
/// statement closed after with its error put on the connection.
pub(crate) fn execute_query(
    req: &mut Request,
    on: On,
    query: PhpString,
    params: Option<PhpArray>,
) -> Result<Union<Owner<Instance<mysqli_result>>, bool>, Thrown> {
    let conn = link::conn(req, on.object, Need::Open)?;
    let mut stmt = Stmt::init(req, conn)?;
    if !stmt.prepare(req, query.as_bytes())? {
        let reported = report_stmt(req, &stmt);
        stmt.close_into_connection(req)?;
        reported?;
        return Ok(Union::B(false));
    }
    let kept = keeps_query(req, query.as_bytes()).then_some(query);
    if let Some(params) = params {
        match bind_list(req, &mut stmt, params, on.at(3)) {
            Err(thrown) => {
                stmt.close(req, false)?;
                return Err(thrown);
            }
            Ok(Some(false)) => {
                stmt.close_into_connection(req)?;
                return Ok(Union::B(false));
            }
            Ok(_) => {}
        }
    }
    let index = |req: &mut Request, stmt: &Stmt| {
        if report_mode(req) & report::INDEX == 0 {
            return Ok(());
        }
        let status = stmt.status(req).server_status;
        report_index(req, kept.as_ref().map_or(&[], PhpString::as_bytes), status)
    };
    if !stmt.execute(req)? {
        let reported = report_stmt(req, &stmt).and_then(|()| index(req, &stmt));
        stmt.close_into_connection(req)?;
        reported?;
        return Ok(Union::B(false));
    }
    if stmt.field_count() == 0 {
        stmt.close_into_connection(req)?;
        return Ok(Union::B(true));
    }
    if let Err(thrown) = index(req, &stmt) {
        stmt.close_into_connection(req)?;
        return Err(thrown);
    }
    let Some(rows) = stmt.take_rows(req)? else {
        let reported = report_stmt(req, &stmt);
        stmt.close_into_connection(req)?;
        reported?;
        return Ok(Union::B(false));
    };
    let result = crate::result::wrap(req, rows);
    if let Err(thrown) = stmt.close_into_connection(req) {
        if let Ok(result) = result {
            result.give_up(req);
        }
        return Err(thrown);
    }
    result.map(Union::A)
}

/// `mysqli_stmt_fetch`: `true` for a row, `null` past the last, `false`
/// on a failure, reported. A bound temporal or zero-filled column's value
/// is the string mysqlnd's `zend_strpprintf` makes, at
/// `zend_string_finit`'s size under php-8.5.12:
///
/// ```php
/// $m = new mysqli('127.0.0.1', 'root', '', 'test');
/// $m->query('CREATE TEMPORARY TABLE tz (d DATE, t TIME(3), dt DATETIME, z SMALLINT(7) UNSIGNED ZEROFILL)');
/// $m->query("INSERT INTO tz VALUES ('2026-10-24', '12:34:56.5', '2026-10-24 12:34:56', 42)");
/// foreach (['d', 't', 'dt', 'z'] as $c) {
///     $s = $m->prepare("SELECT $c FROM tz");
///     $s->execute();
///     $s->bind_result($v);
///     $before = memory_get_usage();
///     $s->fetch();
///     $used = memory_get_usage() - $before;
///     echo $c, ' ', $v, ' ', $used, "\n";
///     $s->close();
/// }
/// ```
/// ```expect mysql
/// d 2026-10-24 816
/// t 12:34:56.500 768
/// dt 2026-10-24 12:34:56 776
/// z 0000042 752
/// ```
pub(crate) fn fetch(req: &mut Request, on: On) -> Result<Option<bool>, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        match s.stmt.fetch(req)? {
            StmtFetch::Row => Ok(Some(true)),
            StmtFetch::NoData => Ok(None),
            StmtFetch::Failed => {
                report_stmt(req, &s.stmt)?;
                Ok(Some(false))
            }
        }
    })
}

/// `mysqli_stmt_get_result`.
pub(crate) fn get_result(
    req: &mut Request,
    on: On,
) -> Result<OrFalse<Owner<Instance<mysqli_result>>>, Thrown> {
    let rows = with_stmt(req, on.object, Need::Open, |req, s| {
        let rows = s.stmt.take_rows(req)?;
        if rows.is_none() {
            report_stmt(req, &s.stmt)?;
        }
        Ok(rows)
    })?;
    match rows {
        Some(rows) => crate::result::wrap(req, rows).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `mysqli_stmt_result_metadata`.
pub(crate) fn result_metadata(
    req: &mut Request,
    on: On,
) -> Result<OrFalse<Owner<Instance<mysqli_result>>>, Thrown> {
    let rows = with_stmt(req, on.object, Need::Open, |req, s| {
        let rows = s.stmt.described(req)?;
        if rows.is_none() {
            report_stmt(req, &s.stmt)?;
        }
        Ok(rows)
    })?;
    match rows {
        Some(rows) => crate::result::wrap(req, rows).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `mysqli_stmt_store_result`.
pub(crate) fn store_result(req: &mut Request, on: On) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        if !s.stmt.store(req)? {
            report_stmt(req, &s.stmt)?;
            return Ok(false);
        }
        Ok(true)
    })
}

/// `mysqli_stmt_next_result`.
pub(crate) fn next_result(req: &mut Request, on: On) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        if !s.stmt.next_rowset(req)? {
            report_stmt(req, &s.stmt)?;
            return Ok(false);
        }
        Ok(true)
    })
}

/// `mysqli_stmt_more_results`.
pub(crate) fn more_results(req: &mut Request, on: On) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        Ok(s.stmt.more_rowsets(req))
    })
}

/// `mysqli_stmt_free_result`.
pub(crate) fn free_result(req: &mut Request, on: On) -> Result<(), Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        s.stmt.free_rows(req).map(|_| ())
    })
}

/// `mysqli_stmt_reset`.
pub(crate) fn reset(req: &mut Request, on: On) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| s.stmt.reset(req))
}

/// `mysqli_stmt_close`.
pub(crate) fn close(req: &mut Request, on: On) -> Result<True, Thrown> {
    with_stmt(req, on.object, Need::Open, |_, _| Ok(()))?;
    let held = crate::payload::<StmtObj>(req, on.object)
        .map(std::mem::take)
        .unwrap_or_default();
    if let StmtObj::Made(s) | StmtObj::Prepared(s) = held {
        // `MYSQLI_CLEAR_RESOURCE` after `php_clear_stmt_bind`.
        let (resource, stmt) = s.repoint(());
        let closed = stmt.close(req, false);
        resource.free(req);
        closed?;
    }
    Ok(True)
}

/// `mysqli_stmt_data_seek`.
pub(crate) fn data_seek(req: &mut Request, on: On, offset: i64) -> Result<(), Thrown> {
    if offset < 0 {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be greater than or equal to 0",
        ));
    }
    with_stmt(req, on.object, Need::Open, |_, s| {
        s.stmt.data_seek(u64::try_from(offset).unwrap_or(0));
        Ok(())
    })
}

/// `mysqli_stmt_send_long_data`.
pub(crate) fn send_long_data(
    req: &mut Request,
    on: On,
    param_num: i64,
    data: PhpString,
) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        if param_num < 0 {
            return Err(arg_value_error(
                req,
                on.at(2),
                "be greater than or equal to 0",
            ));
        }
        let n = u32::try_from(param_num).unwrap_or(u32::MAX);
        s.stmt.send_long_data(req, n, data.as_bytes())
    })
}

/// The attribute `mysqli_stmt_attr_set` and `_get` take.
fn attribute(attr: i64) -> Option<StmtAttr> {
    match attr {
        0 => Some(StmtAttr::UpdateMaxLength),
        1 => Some(StmtAttr::CursorType),
        2 => Some(StmtAttr::PrefetchRows),
        _ => None,
    }
}

/// `mysqli_stmt_attr_set`.
pub(crate) fn attr_set(req: &mut Request, on: On, attr: i64, value: i64) -> Result<bool, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        let which = match attribute(attr) {
            Some(StmtAttr::UpdateMaxLength) if value != 0 && value != 1 => {
                return Err(arg_value_error(
                    req,
                    on.at(3),
                    "be 0 or 1 for attribute MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH",
                ));
            }
            Some(StmtAttr::CursorType) if value != 0 && value != 1 => {
                return Err(arg_value_error(
                    req,
                    on.at(3),
                    "be one of the MYSQLI_CURSOR_TYPE_* constants for attribute MYSQLI_STMT_ATTR_CURSOR_TYPE",
                ));
            }
            Some(which @ (StmtAttr::UpdateMaxLength | StmtAttr::CursorType)) => which,
            _ => {
                return Err(arg_value_error(
                    req,
                    on.at(2),
                    "be either MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH or MYSQLI_STMT_ATTR_CURSOR_TYPE",
                ));
            }
        };
        if !s
            .stmt
            .attr_set(req, which, u64::try_from(value).unwrap_or(0))?
        {
            report_stmt(req, &s.stmt)?;
            return Ok(false);
        }
        Ok(true)
    })
}

/// `mysqli_stmt_attr_get`.
pub(crate) fn attr_get(req: &mut Request, on: On, attr: i64) -> Result<i64, Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        match attribute(attr).and_then(|a| s.stmt.attr_get(a)) {
            Some(value) => Ok(i64::try_from(value).unwrap_or(0)),
            None => Err(arg_value_error(
                req,
                on.at(2),
                "be either MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH or MYSQLI_STMT_ATTR_CURSOR_TYPE",
            )),
        }
    })
}

/// `mysqli_stmt_error_list`.
pub(crate) fn error_list(req: &mut Request, on: On) -> Result<Owner<PhpArray>, Thrown> {
    let status = facts(req, on, Need::Made)?;
    props::error_list(req, &status.error_list)
}

/// The connection a statement runs over, for its warnings.
pub(crate) fn connection(req: &mut Request, on: On) -> Result<(Mysqlnd, u32), Thrown> {
    with_stmt(req, on.object, Need::Open, |req, s| {
        Ok((s.stmt.connection(), s.stmt.status(req).warning_count))
    })
}

/// One of the statement's counts.
pub(crate) fn count(
    req: &mut Request,
    on: On,
    need: Need,
    of: fn(&StmtStatus) -> u64,
) -> Result<Owner<Value>, Thrown> {
    let status = facts(req, on, need)?;
    long_or_string(req, of(&status))
}

/// `mysqli_stmt_affected_rows`.
pub(crate) fn affected_rows(
    req: &mut Request,
    on: On,
) -> Result<AsIs<Union<i64, PhpString>>, Thrown> {
    let status = facts(req, on, Need::Open)?;
    affected(req, status.affected_rows).map(AsIs::new)
}

/// `mysqli_stmt_errno`, `_error` and `_sqlstate`'s facts.
pub(crate) fn error_facts(req: &mut Request, on: On, need: Need) -> Result<StmtStatus, Thrown> {
    facts(req, on, need)
}

// ---- the class

/// `class mysqli_stmt`, its properties read through
/// `mysqli_stmt_property_entries` ([`StmtProps`]).
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = new mysqli('127.0.0.1', 'root', '', 'test');
/// $l->query('CREATE TEMPORARY TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, s TEXT)');
/// $s = new mysqli_stmt($l, 'INSERT INTO t (s) VALUES (?)');
/// $v = 'a'; var_dump($s->bind_param('s', $v), $s->execute(), $s->insert_id, $s->affected_rows, $s->param_count);
/// $v = 'bb'; var_dump($s->execute(), $s->send_long_data(0, 'x'), $s->reset(), $s->execute(['ccc']), $s->close());
/// $s = $l->stmt_init(); var_dump($s->prepare('SELECT id, s FROM t ORDER BY id'), $s->attr_set(MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH, 1), $s->attr_get(MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH));
/// var_dump($s->execute(), $s->store_result(), $s->num_rows, $s->num_rows(), $s->field_count, $s->bind_result($id, $text));
/// while ($s->fetch()) echo $id, '=', $text, "\n";
/// $s->data_seek(1); $s->fetch(); echo $id, "\n";
/// var_dump(get_class($s->result_metadata()), $s->free_result(), $s->more_results(), $s->next_result());
/// var_dump($s->execute(), $s->get_result()->fetch_all(), $s->errno, $s->error, $s->sqlstate, $s->error_list, $s->id > 0);
/// var_dump($s->prepare('SELECT nope'), $s->errno, $s->get_warnings());
/// try { (new mysqli_stmt($l))->execute(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// bool(true)
/// int(1)
/// int(1)
/// int(1)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(1)
/// bool(true)
/// bool(true)
/// int(3)
/// int(3)
/// int(2)
/// bool(true)
/// 1=a
/// 2=bb
/// 3=ccc
/// 2
/// string(13) "mysqli_result"
/// NULL
/// bool(false)
/// bool(false)
/// bool(true)
/// array(3) {
///   [0]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     string(1) "a"
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     int(2)
///     [1]=>
///     string(2) "bb"
///   }
///   [2]=>
///   array(2) {
///     [0]=>
///     int(3)
///     [1]=>
///     string(3) "ccc"
///   }
/// }
/// int(0)
/// string(0) ""
/// string(5) "00000"
/// array(0) {
/// }
/// bool(true)
/// bool(false)
/// int(1054)
/// bool(false)
/// mysqli_stmt object is not fully initialized
/// ```
#[php_class(size = 72, payload = Box<StmtObj>, handlers = STMT)]
pub(crate) struct mysqli_stmt;

pub(crate) static STMT: props::Handlers<StmtProps> = props::Handlers::new();

type Answer<T> = Result<T, Thrown>;

#[php_class]
impl mysqli_stmt {
    #[php(property)]
    const affected_rows: Union<i64, PhpString>;
    #[php(property)]
    const insert_id: Union<i64, PhpString>;
    #[php(property)]
    const num_rows: Union<i64, PhpString>;
    #[php(property)]
    const param_count: i64;
    #[php(property)]
    const field_count: i64;
    #[php(property)]
    const errno: i64;
    #[php(property)]
    const error: PhpString;
    #[php(property)]
    const error_list: PhpArray;
    #[php(property)]
    const sqlstate: PhpString;
    #[php(property)]
    const id: i64;

    fn __construct(
        req: &mut Request,
        this: This<Self>,
        mysql: Instance<mysqli>,
        #[php(default = null)] query: Option<PhpString>,
    ) -> Answer<()> {
        let conn = link::conn(req, mysql.object(), Need::Open)?;
        let stmt = MyStmt::init(req, conn)?;
        let resource = Resource::new(req, stmt)?;
        let object = this.object();
        if let Some(slot) = crate::payload::<StmtObj>(req, object) {
            *slot = StmtObj::Made(resource);
        }
        if let Some(query) = query {
            stmt_prepare(req, On::method(object), query)?;
        }
        Ok(())
    }

    #[php(tentative)]
    fn attr_get(req: &mut Request, this: This<Self>, attribute: i64) -> Answer<i64> {
        attr_get(req, On::method(this.object()), attribute)
    }

    #[php(tentative)]
    fn attr_set(req: &mut Request, this: This<Self>, attribute: i64, value: i64) -> Answer<bool> {
        attr_set(req, On::method(this.object()), attribute, value)
    }

    #[php(tentative)]
    fn bind_param(
        req: &mut Request,
        this: This<Self>,
        types: PhpString,
        vars: Variadic<Ref<Value>>,
    ) -> Answer<bool> {
        bind_param(req, On::method(this.object()), types, &vars)
    }

    #[php(tentative)]
    fn bind_result(
        req: &mut Request,
        this: This<Self>,
        vars: Variadic<Ref<Value>>,
    ) -> Answer<bool> {
        bind_result(req, On::method(this.object()), &vars)
    }

    #[php(tentative)]
    fn close(req: &mut Request, this: This<Self>) -> Answer<True> {
        close(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn data_seek(req: &mut Request, this: This<Self>, offset: i64) -> Answer<()> {
        data_seek(req, On::method(this.object()), offset)
    }

    #[php(tentative)]
    fn execute(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] params: Option<PhpArray>,
    ) -> Answer<bool> {
        execute(req, On::method(this.object()), params)
    }

    #[php(tentative)]
    fn fetch(req: &mut Request, this: This<Self>) -> Answer<Option<bool>> {
        fetch(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn get_warnings(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<OrFalse<Owner<Instance<crate::extras::mysqli_warning>>>> {
        crate::extras::stmt_warnings(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn result_metadata(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<OrFalse<Owner<Instance<mysqli_result>>>> {
        result_metadata(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn more_results(req: &mut Request, this: This<Self>) -> Answer<bool> {
        more_results(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn next_result(req: &mut Request, this: This<Self>) -> Answer<bool> {
        next_result(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn num_rows(req: &mut Request, this: This<Self>) -> Answer<AsIs<Union<i64, PhpString>>> {
        count(req, On::method(this.object()), Need::Open, |s| s.num_rows).map(AsIs::new)
    }

    #[php(tentative)]
    fn send_long_data(
        req: &mut Request,
        this: This<Self>,
        param_num: i64,
        data: PhpString,
    ) -> Answer<bool> {
        send_long_data(req, On::method(this.object()), param_num, data)
    }

    #[php(tentative)]
    fn free_result(req: &mut Request, this: This<Self>) -> Answer<()> {
        free_result(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn reset(req: &mut Request, this: This<Self>) -> Answer<bool> {
        reset(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn prepare(req: &mut Request, this: This<Self>, query: PhpString) -> Answer<bool> {
        stmt_prepare(req, On::method(this.object()), query)
    }

    #[php(tentative)]
    fn store_result(req: &mut Request, this: This<Self>) -> Answer<bool> {
        store_result(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn get_result(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<OrFalse<Owner<Instance<mysqli_result>>>> {
        get_result(req, On::method(this.object()))
    }
}

/// `mysqli_stmt_property_entries`.
pub(crate) struct StmtProps;

impl Props for StmtProps {
    const NAMES: &'static [&'static str] = &[
        "affected_rows",
        "insert_id",
        "num_rows",
        "param_count",
        "field_count",
        "errno",
        "error",
        "error_list",
        "sqlstate",
        "id",
    ];

    fn read(
        req: &mut Request,
        object: PhpObject,
        name: &str,
        quiet: bool,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let made_enough = matches!(name, "errno" | "error" | "error_list" | "sqlstate");
        let status = match crate::payload::<StmtObj>(req, object).map(std::mem::take) {
            Some(StmtObj::Prepared(s)) => {
                let status = s.stmt.status(req);
                put_back(req, object, StmtObj::Prepared(s));
                status
            }
            Some(StmtObj::Made(s)) if made_enough => {
                let status = s.stmt.status(req);
                put_back(req, object, StmtObj::Made(s));
                status
            }
            Some(StmtObj::Made(s)) => {
                put_back(req, object, StmtObj::Made(s));
                return props::not_yet(req, quiet);
            }
            _ if matches!(name, "affected_rows" | "error_list" | "id") => {
                return props::not_yet(req, quiet);
            }
            _ => return props::gone(req, object, quiet),
        };
        let value = match name {
            "affected_rows" => affected(req, status.affected_rows)?,
            "insert_id" => long_or_string(req, status.insert_id)?,
            "num_rows" => long_or_string(req, status.num_rows)?,
            "param_count" => Owner::int(i64::from(status.parameter_count)),
            "field_count" => Owner::int(i64::from(status.field_count)),
            "errno" => Owner::int(i64::from(status.errno)),
            "error" => string(req, &status.error)?,
            "error_list" => props::error_list(req, &status.error_list)?.into_value(),
            "sqlstate" => string(req, &status.sqlstate)?,
            "id" => Owner::int(i64::from(status.id)),
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    /// `mysqli_stmt_free_storage`: `php_clear_stmt_bind`, the statement
    /// closed as an implicit close, php's warning where that fails.
    fn free(req: &mut Request, object: PhpObject) {
        let held = crate::payload::<StmtObj>(req, object)
            .map(std::mem::take)
            .unwrap_or_default();
        if let StmtObj::Made(s) | StmtObj::Prepared(s) = held {
            let (resource, stmt) = s.repoint(());
            if !stmt.close(req, true).unwrap_or(false) {
                docref(
                    req,
                    Level::Warning,
                    "Error occurred while closing statement",
                );
            }
            resource.free(req);
        }
    }
}

fn put_back(req: &mut Request, object: PhpObject, held: StmtObj) {
    if let Some(slot) = crate::payload::<StmtObj>(req, object) {
        *slot = held;
    }
}
