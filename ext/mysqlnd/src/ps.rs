//! `mysqlnd_ps.c`, and the request half of `mysqlnd_ps_codec.c`: a
//! prepared statement, its state machine, the parameters and columns it
//! binds, the `COM_STMT_EXECUTE` request built from the bound values as
//! php builds it (with the cursor bit `mysql_common` cannot send), and the
//! rows read buffered, unbuffered or through a server-side cursor.
//!
//! A statement owns its data and one owner of its connection, reached
//! through the request's table for each call; what php keeps as zvals,
//! the bound parameters and columns, are values with one owner each,
//! given back on rebind, free and close.

use php_ext::coerce::{to_double, to_long, with_string};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, Value, ValueRef};

use crate::alloc::{BIND, PLUGIN_ROOM, STMT, STMT_DATA};
use crate::client::{Mysqlnd, ResultSet};
use crate::conn::ConnData;
use crate::consts::{
    Com, ConnState, CursorType, FieldType, NET_CMD_BUFFER_MIN_SIZE, QueryType, ResType, StmtState,
    cr, status,
};
use crate::error::{ErrorInfo, OUT_OF_SYNC, Upsert};
use crate::pool::Pool;
use crate::result::{Field, Res, Row};
use crate::stats::Stat;
use crate::wire::{Prot, store_length};

/// `mysqlnd_stmt_not_prepared`.
const NOT_PREPARED: &str = "Statement not prepared";
/// `mysqlnd_not_bound_as_blob`.
const NOT_BOUND_AS_BLOB: &str = "Can't send long data for non-string/non-binary data types";

/// `enum mysqlnd_stmt_attr`, the attributes `mysqlnd_stmt_attr_set` and
/// `_get` take; `PrefetchRows` is libmysql's, which mysqlnd answers with
/// "Not implemented".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtAttr {
    UpdateMaxLength,
    CursorType,
    PrefetchRows,
}

/// What `mysqlnd_stmt_fetch` came to: a row, `MYSQLND_NO_DATA`, or `FAIL`
/// with the statement's error set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtFetch {
    Row,
    NoData,
    Failed,
}

/// The statement's facts as its accessors read them: `mysqlnd_stmt_errno`,
/// `_sqlstate`, `_error`, the error list, `_affected_rows` (`~0` where
/// none), `_insert_id`, `_num_rows`, `_warning_count`, `_server_status`,
/// `_field_count`, `_param_count` and the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StmtStatus {
    pub errno: u32,
    pub sqlstate: [u8; 5],
    pub error: Vec<u8>,
    pub error_list: Vec<(u32, [u8; 5], Vec<u8>)>,
    pub affected_rows: u64,
    pub insert_id: u64,
    pub num_rows: u64,
    pub warning_count: u32,
    pub server_status: u32,
    pub field_count: u32,
    pub parameter_count: u32,
    pub state: StmtState,
    /// `stmt_id`, the server's number for the statement.
    pub id: u32,
}

/// `MYSQLND_PARAM_BIND`: the value and the type it is sent as, and
/// `MYSQLND_PARAM_BIND_BLOB_USED`.
#[derive(Debug, Default)]
struct ParamBind {
    zv: Option<Owner<Value>>,
    ty: Option<FieldType>,
    blob_used: bool,
}

/// `MYSQLND_RESULT_BIND`: the value a fetch assigns, where bound.
#[derive(Debug, Default)]
struct ResultBind {
    zv: Option<Owner<Value>>,
    bound: bool,
}

/// `MYSQLND_PARAM_BIND *` or `MYSQLND_RESULT_BIND *`: a bind for each
/// parameter or column, in the `mnd_ecalloc(n, sizeof(bind))` php keeps
/// them in, on the request's heap until the binding is freed.
#[derive(Debug)]
struct Binds<T> {
    list: Vec<T>,
    held: Emalloc,
}

impl<T> Binds<T> {
    /// The binds allocated; the `Err` is the memory limit's fatal.
    fn new(req: &mut Request, list: Vec<T>) -> Result<Binds<T>, Thrown> {
        let held = Emalloc::new(req, list.len() * BIND)?;
        Ok(Binds { list, held })
    }

    /// `mnd_efree`: the allocation given back, the binds left to give
    /// back their values.
    fn free(self, req: &mut Request) -> Vec<T> {
        self.held.free(req);
        self.list
    }
}

impl<T> std::ops::Deref for Binds<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.list
    }
}

impl<T> std::ops::DerefMut for Binds<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.list
    }
}

/// `stmt->default_rset_handler`: what a `COM_STMT_EXECUTE`'s rows are read with
/// when the first fetch asks for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handler {
    Use,
    Store,
}

/// `MYSQLND_STMT_DATA`.
#[derive(Debug)]
struct StmtData {
    stmt_id: u32,
    state: StmtState,
    parameter_count: u32,
    field_count: u32,
    result: Option<Box<Res>>,
    param_bind: Option<Binds<ParamBind>>,
    result_bind: Option<Binds<ResultBind>>,
    send_types_to_server: bool,
    upsert: Upsert,
    error: ErrorInfo,
    cursor: CursorType,
    cursor_exists: bool,
    update_max_length: bool,
    execute_count: u64,
    handler: Handler,
}

impl StmtData {
    /// `mysqlnd_object_factory::get_prepared_statement`.
    fn new() -> StmtData {
        StmtData {
            stmt_id: 0,
            state: StmtState::Initted,
            parameter_count: 0,
            field_count: 0,
            result: None,
            param_bind: None,
            result_bind: None,
            send_types_to_server: false,
            upsert: Upsert::default(),
            error: ErrorInfo::new(false),
            cursor: CursorType::NoCursor,
            cursor_exists: false,
            update_max_length: false,
            execute_count: 0,
            handler: Handler::Use,
        }
    }
}

/// `MYSQLND_STMT *`: owns its data, holds one owner of its connection,
/// released by [`Stmt::close`].
#[derive(Debug)]
pub struct Stmt {
    data: Box<StmtData>,
    conn: Mysqlnd,
    held: Held,
}

/// What `get_prepared_statement` allocates on the request's heap, a
/// persistent connection's statement too: `MYSQLND_STMT` with its
/// `PLUGIN_ROOM`, `MYSQLND_STMT_DATA` and the `execute_cmd_buffer`;
/// `dtor` frees them.
#[derive(Debug)]
struct Held {
    stmt: Emalloc,
    data: Emalloc,
    execute_cmd_buffer: Emalloc,
}

impl Held {
    /// The three allocated in php's order; the `Err` is the memory
    /// limit's fatal.
    fn new(req: &mut Request) -> Result<Held, Thrown> {
        Ok(Held {
            stmt: Emalloc::new(req, STMT + PLUGIN_ROOM)?,
            data: Emalloc::new(req, STMT_DATA)?,
            execute_cmd_buffer: Emalloc::new(req, NET_CMD_BUFFER_MIN_SIZE as usize)?,
        })
    }

    /// `mysqlnd_stmt::dtor`'s frees, in its order.
    fn free(self, req: &mut Request) {
        self.execute_cmd_buffer.free(req);
        self.data.free(req);
        self.stmt.free(req);
    }
}

/// Both error infos emptied, `SET_EMPTY_ERROR` on each.
fn empty_errors(req: &mut Request, stmt: &mut StmtData, conn: &mut ConnData) {
    stmt.error.reset(req);
    conn.error.reset(req);
}

impl Stmt {
    /// `mysqlnd_stmt_init`: a statement of `conn`, which it holds one
    /// owner of; the `Err` is the memory limit's fatal. What
    /// `memory_get_usage()` sees of a statement is its structures, its
    /// `execute_cmd_buffer`, its columns' pool and the bound parameters'
    /// binds (with mysqli's `MY_STMT` and resource), all of it but the
    /// bound reference given back by the close:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $m = memory_get_usage();
    /// $s = mysqli_prepare($l, "SELECT REPEAT('x', 100000) a, ? b");
    /// echo memory_get_usage() - $m, "\n";
    /// $v = 5;
    /// mysqli_stmt_bind_param($s, "i", $v);
    /// echo memory_get_usage() - $m, "\n";
    /// mysqli_stmt_close($s);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect mysql
    /// 21768
    /// 21856
    /// 288
    /// ```
    pub fn init(req: &mut Request, conn: Mysqlnd) -> Result<Stmt, Thrown> {
        Ok(Stmt {
            held: Held::new(req)?,
            data: Box::new(StmtData::new()),
            conn: conn.acquire(req),
        })
    }

    /// The connection lent with the statement's data; `None` where the
    /// connection is gone, php's `!conn`.
    fn call<R>(
        &mut self,
        req: &mut Request,
        f: impl FnOnce(&mut Request, &mut StmtData, &mut ConnData) -> Result<R, Thrown>,
    ) -> Result<Option<R>, Thrown> {
        let data = &mut self.data;
        self.conn
            .handle()
            .with(req, |req, conn| f(req, data, conn))
            .transpose()
    }

    /// `mysqlnd_stmt_prepare`: a statement already prepared closed and
    /// made anew; the query sent, the parameters' definitions skipped, the
    /// columns' read.
    pub fn prepare(&mut self, req: &mut Request, sql: &[u8]) -> Result<bool, Thrown> {
        if self.data.state > StmtState::Initted {
            let made = Stmt::init(req, self.conn)?;
            let old = std::mem::replace(self, made);
            old.dtor(req, true)?;
        }
        Ok(self
            .call(req, |req, stmt, conn| prepare(req, stmt, conn, sql))?
            .unwrap_or(false))
    }

    /// `mysqlnd_stmt_param_count`.
    #[must_use]
    pub fn parameter_count(&self) -> u32 {
        self.data.parameter_count
    }

    /// `mysqlnd_stmt_field_count`.
    #[must_use]
    pub fn field_count(&self) -> u32 {
        self.data.field_count
    }

    /// `mysqlnd_stmt_bind_one_param`: `value`'s owner taken for parameter
    /// `n`, sent as `ty`, the one bound before given back; the binds
    /// allocated where none were, the `Err` the memory limit's fatal.
    pub fn bind_parameter(
        &mut self,
        req: &mut Request,
        n: u32,
        value: Owner<Value>,
        ty: FieldType,
    ) -> Result<bool, Thrown> {
        let stmt = &mut self.data;
        if stmt.state < StmtState::Prepared {
            stmt.error.client(req, cr::NO_PREPARE_STMT, NOT_PREPARED)?;
            value.give_up(req);
            return Ok(false);
        }
        if n >= stmt.parameter_count {
            stmt.error
                .client(req, cr::INVALID_PARAMETER_NO, "Invalid parameter number")?;
            value.give_up(req);
            return Ok(false);
        }
        stmt.error.reset(req);
        if stmt.param_bind.is_none() {
            let count = stmt.parameter_count as usize;
            let list = (0..count).map(|_| ParamBind::default()).collect();
            stmt.param_bind = Some(Binds::new(req, list)?);
        }
        let Some(bind) = stmt.param_bind.as_mut().and_then(|b| b.get_mut(n as usize)) else {
            value.give_up(req);
            return Ok(false);
        };
        if let Some(old) = bind.zv.replace(value) {
            old.give_up(req);
        }
        if ty == FieldType::LongBlob {
            bind.blob_used = false;
        }
        bind.ty = Some(ty);
        stmt.send_types_to_server = true;
        self.call(req, |req, _, conn| {
            conn.error.reset(req);
            Ok(())
        })
        .ok();
        Ok(true)
    }

    /// `mysqlnd_stmt_bind_param`: every parameter bound at once in the
    /// binds `mysqlnd_stmt_alloc_param_bind` allocates, each value's owner
    /// taken, the binds before freed with their values given back; the
    /// `Err` is the memory limit's fatal.
    pub fn bind_parameters(
        &mut self,
        req: &mut Request,
        params: Vec<(Owner<Value>, FieldType)>,
    ) -> Result<bool, Thrown> {
        let stmt = &mut self.data;
        if stmt.state < StmtState::Prepared {
            stmt.error.client(req, cr::NO_PREPARE_STMT, NOT_PREPARED)?;
            for (v, _) in params {
                v.give_up(req);
            }
            return Ok(false);
        }
        stmt.error.reset(req);
        if stmt.parameter_count == 0 {
            for (v, _) in params {
                v.give_up(req);
            }
            return Ok(true);
        }
        let list = params
            .into_iter()
            .map(|(zv, ty)| ParamBind {
                zv: Some(zv),
                ty: Some(ty),
                blob_used: false,
            })
            .collect();
        let binds = Binds::new(req, list)?;
        if let Some(old) = stmt.param_bind.replace(binds) {
            for v in old.free(req).into_iter().filter_map(|b| b.zv) {
                v.give_up(req);
            }
        }
        stmt.send_types_to_server = true;
        Ok(true)
    }

    /// `mysqlnd_stmt_refresh_bind_param`: the types sent again with the
    /// next `COM_STMT_EXECUTE`; the `Err` is the memory limit's fatal.
    pub fn rebind_parameters(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let stmt = &mut self.data;
        if stmt.state < StmtState::Prepared {
            stmt.error.client(req, cr::NO_PREPARE_STMT, NOT_PREPARED)?;
            return Ok(false);
        }
        stmt.error.reset(req);
        if stmt.parameter_count > 0 {
            stmt.send_types_to_server = true;
        }
        Ok(true)
    }

    /// `mysqlnd_stmt_execute`: `send_execute`, then the answer parsed.
    pub fn execute(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, stmt, conn| {
                Ok(send_execute(req, stmt, conn)? && parse_execute_response(req, stmt, conn)?)
            })?
            .unwrap_or(false))
    }

    /// `mysqlnd_stmt_bind_one_result`: column `n` bound to a value of the
    /// statement's own, which each fetch replaces; the binds allocated
    /// where none were, the `Err` the memory limit's fatal.
    pub fn bind_column(&mut self, req: &mut Request, n: u32) -> Result<bool, Thrown> {
        let stmt = &mut self.data;
        if stmt.state < StmtState::Prepared {
            stmt.error.client(req, cr::NO_PREPARE_STMT, NOT_PREPARED)?;
            return Ok(false);
        }
        if n >= stmt.field_count {
            stmt.error
                .client(req, cr::INVALID_PARAMETER_NO, "Invalid parameter number")?;
            return Ok(false);
        }
        stmt.error.reset(req);
        if stmt.result_bind.is_none() {
            let count = stmt.field_count as usize;
            let list = (0..count).map(|_| ResultBind::default()).collect();
            stmt.result_bind = Some(Binds::new(req, list)?);
        }
        if let Some(bind) = stmt
            .result_bind
            .as_mut()
            .and_then(|b| b.get_mut(n as usize))
        {
            if let Some(old) = bind.zv.replace(Owner::null()) {
                old.give_up(req);
            }
            bind.bound = true;
        }
        Ok(true)
    }

    /// `mysqlnd_stmt_bind_result`: every column bound to the values given
    /// in the binds `mysqlnd_stmt_alloc_result_bind` allocates, a reference
    /// assigned through, each value's owner taken; the `Err` is the memory
    /// limit's fatal.
    pub fn bind_columns(
        &mut self,
        req: &mut Request,
        values: Vec<Owner<Value>>,
    ) -> Result<bool, Thrown> {
        let stmt = &mut self.data;
        if stmt.state < StmtState::Prepared {
            stmt.error.client(req, cr::NO_PREPARE_STMT, NOT_PREPARED)?;
            for v in values {
                v.give_up(req);
            }
            return Ok(false);
        }
        stmt.error.reset(req);
        if stmt.field_count == 0 {
            for v in values {
                v.give_up(req);
            }
            return Ok(true);
        }
        let list = values
            .into_iter()
            .map(|zv| ResultBind {
                zv: Some(zv),
                bound: true,
            })
            .collect();
        let binds = Binds::new(req, list)?;
        separate_result_bind(req, stmt);
        stmt.result_bind = Some(binds);
        Ok(true)
    }

    /// The columns' descriptions `mysqlnd_stmt_result_metadata` copies,
    /// `None` where the statement has no rows.
    #[must_use]
    pub fn fields(&self) -> Option<&[Field]> {
        let result = self.data.result.as_ref()?;
        (self.data.field_count > 0 && !result.fields.is_empty()).then_some(&result.fields[..])
    }

    /// `mysqlnd_stmt_store_result`: every row read and held; a statement
    /// with no columns has none to hold and passes, as the macro in
    /// `mysqlnd.h` answers `PASS` before asking the statement.
    pub fn store(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        if self.data.field_count == 0 {
            return Ok(true);
        }
        Ok(self.call(req, store_result)?.unwrap_or(false))
    }

    /// `mysqlnd_stmt_use_result`: rows to be read one fetch at a time,
    /// through the cursor where the server opened one.
    pub fn use_unbuffered(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self.call(req, use_result)?.unwrap_or(false))
    }

    /// `mysqlnd_stmt_get_result`: the rows read and held in a `MYSQLND_RES`
    /// of their own, the statement prepared again.
    pub fn take_rows(&mut self, req: &mut Request) -> Result<Option<ResultSet>, Thrown> {
        let got = self.call(req, get_result)?;
        Ok(got
            .flatten()
            .map(|res| ResultSet::of(res, self.conn.acquire(req))))
    }

    /// `mysqlnd_stmt_fetch`: the next row into the bound columns.
    pub fn fetch(&mut self, req: &mut Request) -> Result<StmtFetch, Thrown> {
        Ok(self.call(req, fetch)?.unwrap_or(StmtFetch::Failed))
    }

    /// `ZVAL_COPY` of column `n`'s bound value: one owner the caller's,
    /// `null` where the column is not bound.
    pub fn column_value(&self, req: &mut Request, n: u32) -> Owner<Value> {
        let bound = self
            .data
            .result_bind
            .as_ref()
            .and_then(|b| b.get(n as usize))
            .and_then(|b| b.zv.as_deref());
        let value = bound.map_or_else(Value::null, Value::deref);
        Owner::take(req, value)
    }

    /// The statement's facts.
    #[must_use]
    pub fn status(&self, req: &mut Request) -> StmtStatus {
        let _ = req;
        let s = &self.data;
        StmtStatus {
            errno: s.error.errno,
            sqlstate: s.error.state(),
            error: s.error.error.clone(),
            error_list: s
                .error
                .list()
                .iter()
                .map(|e| (e.errno, e.sqlstate, e.error.clone()))
                .collect(),
            affected_rows: s.upsert.affected_rows,
            insert_id: s.upsert.last_insert_id,
            num_rows: s.result.as_ref().map_or(0, |r| r.num_rows()),
            warning_count: s.upsert.warning_count,
            server_status: s.upsert.server_status,
            field_count: s.field_count,
            parameter_count: s.parameter_count,
            state: s.state,
            id: s.stmt_id,
        }
    }

    /// `mysqlnd_stmt_more_results`: the connection's status says another
    /// set of rows follows.
    pub fn more_rowsets(&self, req: &mut Request) -> bool {
        self.conn
            .handle()
            .peek(req, ConnData::more_rowsets)
            .unwrap_or(false)
    }

    /// `mysqlnd_stmt_next_result`.
    pub fn next_rowset(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self.call(req, next_result)?.unwrap_or(false))
    }

    /// `mysqlnd_stmt_free_result`: what is left of the rows read off the
    /// line and dropped, the bound columns given back, the statement
    /// prepared again.
    pub fn free_rows(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self.call(req, free_result)?.unwrap_or(false))
    }

    /// `mysqlnd_stmt_reset`: the long data forgotten, the rows flushed,
    /// `COM_STMT_RESET` sent.
    pub fn reset(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self.call(req, reset)?.unwrap_or(false))
    }

    /// `mysqlnd_stmt_send_long_data`: a chunk of parameter `n` sent with
    /// `COM_STMT_SEND_LONG_DATA`, which the server does not answer.
    pub fn send_long_data(
        &mut self,
        req: &mut Request,
        n: u32,
        data: &[u8],
    ) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, stmt, conn| {
                send_long_data(req, stmt, conn, n, data)
            })?
            .unwrap_or(false))
    }

    /// `mysqlnd_stmt_data_seek`.
    pub fn data_seek(&mut self, row: u64) -> bool {
        self.data.result.as_mut().is_some_and(|r| r.data_seek(row))
    }

    /// `mysqlnd_stmt_attr_set`; the `Err` is the memory limit's fatal.
    pub fn attr_set(
        &mut self,
        req: &mut Request,
        attr: StmtAttr,
        value: u64,
    ) -> Result<bool, Thrown> {
        let stmt = &mut self.data;
        match attr {
            StmtAttr::UpdateMaxLength => stmt.update_max_length = value != 0,
            StmtAttr::CursorType if value <= 1 => {
                stmt.cursor = if value == 1 {
                    CursorType::ReadOnly
                } else {
                    CursorType::NoCursor
                };
            }
            StmtAttr::CursorType | StmtAttr::PrefetchRows => {
                stmt.error
                    .client(req, cr::NOT_IMPLEMENTED, "Not implemented")?;
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `mysqlnd_stmt_attr_get`.
    #[must_use]
    pub fn attr_get(&self, attr: StmtAttr) -> Option<u64> {
        match attr {
            StmtAttr::UpdateMaxLength => Some(u64::from(self.data.update_max_length)),
            StmtAttr::CursorType => Some(u64::from(self.data.cursor == CursorType::ReadOnly)),
            StmtAttr::PrefetchRows => None,
        }
    }

    /// `stmt->data->conn`: the connection the statement runs over.
    #[must_use]
    pub fn connection(&self) -> Mysqlnd {
        self.conn
    }

    /// `mysqlnd_stmt_result_metadata`: a set of rows with the statement's
    /// columns and no rows, read to its end, over the statement's
    /// connection; `None` where the statement has no columns.
    pub fn described(&self, req: &mut Request) -> Result<Option<ResultSet>, Thrown> {
        let Some(fields) = self.fields() else {
            return Ok(None);
        };
        let res = Box::new(Res::metadata_only(req, self.data.field_count, fields)?);
        Ok(Some(ResultSet::of(res, self.conn.acquire(req))))
    }

    /// mysqli's `close_stmt_and_copy_errors`: the statement closed, and
    /// its error, which the close clears, put on the connection in place
    /// of the connection's, with the connection's affected rows kept.
    pub fn close_into_connection(mut self, req: &mut Request) -> Result<bool, Thrown> {
        let error = self.data.error.keep();
        let conn = self.conn;
        let affected = conn
            .handle()
            .peek(req, |c| c.upsert.affected_rows)
            .unwrap_or(u64::MAX);
        let held = conn.acquire(req);
        let closed = self.close(req, false);
        let _ = held.handle().with(req, |req, c| {
            c.error.restore(req, error);
            c.upsert.affected_rows = affected;
        });
        held.release(req);
        closed
    }

    /// `mysqlnd_stmt_close`: `dtor`, the close counted by how it came,
    /// the statement closed on the server.
    pub fn close(self, req: &mut Request, implicit: bool) -> Result<bool, Thrown> {
        self.dtor(req, implicit)
    }

    /// `mysqlnd_stmt::dtor` with `close_on_server`; what the statement
    /// holds of the request's given back and its connection's owner
    /// released whatever the server answered.
    fn dtor(mut self, req: &mut Request, implicit: bool) -> Result<bool, Thrown> {
        let closed = self.call(req, |req, stmt, conn| {
            conn.stats.global(
                if implicit {
                    Stat::StmtCloseImplicit
                } else {
                    Stat::StmtCloseExplicit
                },
                1,
            );
            close_on_server(req, stmt, conn, implicit)
        });
        let closed = closed.map(|closed| closed.unwrap_or(false));
        free_stmt_content(req, &mut self.data);
        if let Some(result) = self.data.result.take() {
            result.release(req);
        }
        // `free_stmt_result`'s `zend_llist_clean(&stmt->error_info->error_list)`.
        self.data.error.clean(req);
        self.held.free(req);
        self.conn.release(req);
        closed
    }
}

// ---- preparing

/// `mysqlnd_stmt::prepare` past the remake.
fn prepare(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
    sql: &[u8],
) -> Result<bool, Thrown> {
    stmt.upsert.affected_rows_to_error();
    conn.upsert.affected_rows_to_error();
    empty_errors(req, stmt, conn);
    if !conn.send_command(req, Com::StmtPrepare, sql, false)? {
        stmt.error.copy_from(req, &conn.error)?;
        return Ok(false);
    }
    // `mysqlnd_stmt_read_prepare_response`.
    let Some(response) = conn.read_prepare_response(req)? else {
        return Ok(false);
    };
    if let Some(e) = response.error {
        e.set_on(req, &mut stmt.error)?;
        e.set_on(req, &mut conn.error)?;
        return Ok(false);
    }
    stmt.stmt_id = response.stmt_id;
    conn.upsert.warning_count = response.warning_count;
    stmt.upsert.affected_rows = 0;
    stmt.field_count = response.field_count;
    conn.field_count = response.field_count;
    stmt.parameter_count = response.param_count;
    if stmt.parameter_count > 0 {
        // `mysqlnd_stmt_skip_metadata`: the parameters' columns read and
        // dropped, as libmysql does, over a pool of their own that none
        // of them is carved from.
        let pool = Pool::create(req)?;
        let mut skipped = true;
        for _ in 0..stmt.parameter_count {
            match conn.read_packet(req, Prot::RsetField) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    skipped = false;
                    break;
                }
                Err(thrown) => {
                    pool.destroy(req);
                    return Err(thrown);
                }
            }
        }
        pool.destroy(req);
        if !skipped || !prepare_read_eof(req, stmt, conn)? {
            return Ok(false);
        }
    }
    if stmt.field_count > 0 {
        let mut result = Box::new(Res::new(req, stmt.field_count)?);
        result.kind = ResType::PsBuffered;
        let read = result.read_metadata(req, conn)?;
        if let Some(old) = stmt.result.replace(result) {
            old.release(req);
        }
        if !read || !prepare_read_eof(req, stmt, conn)? {
            return Ok(false);
        }
    }
    stmt.state = StmtState::Prepared;
    Ok(true)
}

/// `mysqlnd_stmt_prepare_read_eof`: the EOF after the column definitions, its
/// status the statement's; a failed read leaves the statement as new.
fn prepare_read_eof(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<bool, Thrown> {
    let Some(eof) = conn.read_eof(req)? else {
        if let Some(result) = stmt.result.take() {
            result.release(req);
            *stmt = StmtData::new();
        }
        return Ok(false);
    };
    stmt.upsert.server_status = eof.server_status;
    stmt.upsert.warning_count = eof.warning_count;
    stmt.state = StmtState::Prepared;
    Ok(true)
}

// ---- executing

/// `mysqlnd_stmt::send_execute`: the rows of a previous `COM_STMT_EXECUTE` flushed,
/// every parameter required bound, the request built and sent.
fn send_execute(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<bool, Thrown> {
    stmt.upsert.affected_rows_to_error();
    conn.upsert.affected_rows_to_error();
    if stmt.result.is_some() && stmt.state >= StmtState::Prepared && stmt.field_count > 0 {
        flush(req, stmt, conn)?;
        if let Some(result) = stmt.result.as_mut() {
            result.free_buffers(req);
        }
        stmt.state = StmtState::Prepared;
    } else if stmt.state < StmtState::Prepared {
        stmt.error
            .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
        return Ok(false);
    }
    if stmt.parameter_count > 0 {
        let Some(binds) = &stmt.param_bind else {
            stmt.error.client(
                req,
                cr::PARAMS_NOT_BOUND,
                "No data supplied for parameters in prepared statement",
            )?;
            return Ok(false);
        };
        let not_bound = binds.iter().filter(|b| b.zv.is_none()).count();
        if not_bound > 0 {
            let plural = if not_bound > 1 { "s" } else { "" };
            stmt.error.client(
                req,
                cr::PARAMS_NOT_BOUND,
                &format!(
                    "No data supplied for {not_bound} parameter{plural} in prepared statement"
                ),
            )?;
            return Ok(false);
        }
    }
    let request = match generate_request(req, stmt) {
        Ok(request) => request,
        Err(thrown) => {
            stmt.error.client(
                req,
                cr::UNKNOWN_ERROR,
                "Couldn't generate the request. Possibly OOM.",
            )?;
            return Err(thrown);
        }
    };
    if !conn.send_command(req, Com::StmtExecute, &request, false)? {
        stmt.error.copy_from(req, &conn.error)?;
        return Ok(false);
    }
    stmt.execute_count += 1;
    Ok(true)
}

/// `mysqlnd_stmt_execute_parse_response`: the answer to a
/// `COM_STMT_EXECUTE` (or the next set of rows, or the OUT parameters a `CALL` sends after its
/// own), the statement's status taken from the connection, the rows'
/// reader chosen by the cursor the server opened or did not.
fn parse_execute_response(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<bool, Thrown> {
    conn.state = ConnState::QuerySent;
    let mut ok = conn.read_result_set_header(req, Some(&mut stmt.result))?;
    if ok {
        empty_errors(req, stmt, conn);
        stmt.upsert.warning_count = conn.upsert.warning_count;
        stmt.upsert.affected_rows = conn.upsert.affected_rows;
        stmt.upsert.server_status = conn.upsert.server_status;
        stmt.upsert.last_insert_id = conn.upsert.last_insert_id;
        stmt.state = StmtState::Executed;
        if matches!(
            conn.last_query_type,
            QueryType::Upsert | QueryType::LoadLocal
        ) {
            return Ok(true);
        }
        if let Some(result) = stmt.result.as_mut() {
            result.kind = ResType::PsBuffered;
        }
        if conn.field_count != stmt.field_count
            && let Some(Binds { mut list, held }) = stmt.result_bind.take()
        {
            let count = conn.field_count as usize;
            for bind in list.drain(count.min(list.len())..) {
                if let Some(v) = bind.zv {
                    v.give_up(req);
                }
            }
            list.resize_with(count, ResultBind::default);
            let held = held.resize(req, count * BIND)?;
            stmt.result_bind = Some(Binds { list, held });
        }
        stmt.field_count = conn.field_count;
        if let Some(result) = stmt.result.as_mut() {
            result.field_count = conn.field_count;
        }
        if stmt.field_count > 0 {
            stmt.state = StmtState::WaitingUseOrStore;
            stmt.handler = if stmt.cursor == CursorType::ReadOnly {
                if stmt.upsert.server_status & status::CURSOR_EXISTS != 0 {
                    stmt.cursor_exists = true;
                    conn.state = ConnState::Ready;
                    Handler::Use
                } else {
                    Handler::Store
                }
            } else {
                Handler::Use
            };
        }
    } else {
        stmt.error.copy_from(req, &conn.error)?;
        let affected = conn.upsert.affected_rows;
        stmt.upsert.reset();
        stmt.upsert.affected_rows = affected;
        stmt.state = StmtState::Prepared;
        stmt.send_types_to_server = true;
    }
    if stmt.upsert.server_status & status::PS_OUT_PARAMS != 0 {
        free_stmt_content(req, stmt);
        free_stmt_result(req, stmt, conn)?;
        ok = parse_execute_response(req, stmt, conn)?;
    }
    if ok && conn.last_query_type == QueryType::Upsert && stmt.upsert.affected_rows != 0 {
        conn.stats
            .add(Stat::RowsAffectedPs, stmt.upsert.affected_rows);
    }
    Ok(ok)
}

/// One parameter as it goes on the wire, after the conversions of
/// `mysqlnd_stmt_execute_prepare_param_types` and of
/// `mysqlnd_stmt_execute_calculate_param_values_size`.
enum Wire {
    Null,
    Long(i64),
    Double,
    Str(Vec<u8>),
    /// A `LONG_BLOB`: whether `send_long_data` sent its value.
    Blob(bool),
    /// A type php sends no value for, marked `NULL`.
    Other,
}

/// `mysqlnd_stmt_execute_prepare_param_types`: each integer parameter
/// that is not an integer made one, or a string where its value does not
/// fit, which asks for the types to be sent now and with the next
/// `COM_STMT_EXECUTE`.
fn prepare_param_types(
    req: &mut Request,
    binds: &[(Value, FieldType, bool)],
) -> Result<(Vec<Option<Wire>>, bool), Thrown> {
    let mut resend = false;
    let mut longs = Vec::with_capacity(binds.len());
    for (v, ty, _) in binds {
        let integer = matches!(ty, FieldType::Long | FieldType::LongLong | FieldType::Tiny);
        let converted = match v.as_ref() {
            ValueRef::Null => None,
            ValueRef::Int(n) if integer => Some(Wire::Long(n)),
            _ if integer => {
                let d = to_double(req, v)?;
                // php's `(double) ZEND_LONG_MAX` and `ZEND_LONG_MIN`.
                #[allow(clippy::cast_precision_loss)]
                let (max, min) = (i64::MAX as f64, i64::MIN as f64);
                if d >= max || d < min {
                    resend = true;
                    Some(Wire::Str(with_string(req, v, <[u8]>::to_vec)?))
                } else {
                    Some(Wire::Long(to_long(req, v)?))
                }
            }
            _ => None,
        };
        longs.push(converted);
    }
    Ok((longs, resend))
}

/// `mysqlnd_stmt_execute_generate_request` with
/// `mysqlnd_stmt_execute_store_params`: the statement's id, the cursor
/// bit, an iteration count of one; then the NULL bitmap, the
/// new-params-bound byte, each type (an integer that does not fit sent as
/// a string, and the types sent again next time), and each value.
fn generate_request(req: &mut Request, stmt: &mut StmtData) -> Result<Vec<u8>, Thrown> {
    let mut out = stmt.stmt_id.to_le_bytes().to_vec();
    out.push(u8::from(stmt.cursor == CursorType::ReadOnly));
    out.extend_from_slice(&[1, 0, 0, 0]);
    if stmt.parameter_count == 0 {
        return Ok(out);
    }
    let count = stmt.parameter_count as usize;
    let null_at = out.len();
    out.resize(null_at + count.div_ceil(8), 0);
    let binds: Vec<(Value, FieldType, bool)> = stmt
        .param_bind
        .as_ref()
        .map(|b| {
            b.iter()
                .map(|p| {
                    let v = p.zv.as_deref().map_or_else(Value::null, Value::deref);
                    (v, p.ty.unwrap_or(FieldType::Null), p.blob_used)
                })
                .collect()
        })
        .unwrap_or_default();
    let (longs, resend) = prepare_param_types(req, &binds)?;
    if resend {
        stmt.send_types_to_server = true;
    }
    out.push(u8::from(stmt.send_types_to_server));
    if stmt.send_types_to_server {
        // `mysqlnd_stmt_execute_store_types`: the unsigned bit never set.
        for ((v, ty, _), long) in binds.iter().zip(&longs) {
            let mut sent = if *ty == FieldType::Long {
                FieldType::LongLong
            } else {
                *ty
            };
            if !matches!(v.as_ref(), ValueRef::Null)
                && sent == FieldType::LongLong
                && matches!(long, Some(Wire::Str(_)))
            {
                sent = FieldType::VarString;
            }
            out.extend_from_slice(&u16::from(sent.byte()).to_le_bytes());
        }
    }
    stmt.send_types_to_server = resend;
    // `mysqlnd_stmt_execute_calculate_param_values_size`: the strings made.
    let mut wires = Vec::with_capacity(count);
    for ((v, ty, used), long) in binds.iter().zip(longs) {
        if matches!(v.as_ref(), ValueRef::Null) {
            wires.push(Wire::Null);
            continue;
        }
        wires.push(match ty {
            FieldType::Double => Wire::Double,
            FieldType::LongLong | FieldType::Long | FieldType::Tiny => {
                long.unwrap_or(Wire::Long(0))
            }
            FieldType::LongBlob => Wire::Blob(*used),
            FieldType::VarString => match v.as_ref() {
                ValueRef::Str(s) => Wire::Str(s.as_bytes().to_vec()),
                _ => Wire::Str(with_string(req, v, <[u8]>::to_vec)?),
            },
            _ => Wire::Other,
        });
    }
    // `mysqlnd_stmt_execute_store_param_values`.
    for (i, (wire, (v, ty, _))) in wires.into_iter().zip(&binds).enumerate() {
        let null = |out: &mut Vec<u8>| {
            if let Some(byte) = out.get_mut(null_at + i / 8) {
                *byte |= 1 << (i & 7);
            }
        };
        match wire {
            Wire::Null | Wire::Other => null(&mut out),
            Wire::Double => {
                let d = match v.as_ref() {
                    ValueRef::Float(d) => d,
                    _ => to_double(req, v)?,
                };
                out.extend_from_slice(&d.to_le_bytes());
            }
            Wire::Str(bytes) => {
                store_length(&mut out, bytes.len() as u64);
                out.extend_from_slice(&bytes);
            }
            // `int4store` and `int1store` keep the low bytes.
            Wire::Long(n) => match ty {
                FieldType::LongLong => out.extend_from_slice(&n.to_le_bytes()),
                FieldType::Long => out.extend_from_slice(&n.to_le_bytes()[..4]),
                _ => out.push(n.to_le_bytes()[0]),
            },
            Wire::Blob(true) => {
                if let Some(bind) = stmt.param_bind.as_mut().and_then(|b| b.get_mut(i)) {
                    bind.blob_used = false;
                }
            }
            Wire::Blob(false) => store_length(&mut out, 0),
        }
    }
    Ok(out)
}

// ---- reading the rows

/// `mysqlnd_stmt_check_state`: waiting for its rows to be asked for,
/// with the connection where a cursor or the rows leave it.
fn check_state(stmt: &StmtData, conn: &ConnData) -> bool {
    stmt.state == StmtState::WaitingUseOrStore
        && conn.state
            == if stmt.cursor_exists {
                ConnState::Ready
            } else {
                ConnState::FetchingData
            }
}

/// `COM_STMT_FETCH` asking for `rows`, the connection's error copied
/// where it failed.
fn cursor_fetch(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
    rows: u32,
) -> Result<bool, Thrown> {
    let mut payload = stmt.stmt_id.to_le_bytes().to_vec();
    payload.extend_from_slice(&rows.to_le_bytes());
    if !conn.send_command(req, Com::StmtFetch, &payload, false)? {
        stmt.error.copy_from(req, &conn.error)?;
        return Ok(false);
    }
    Ok(true)
}

/// `mysqlnd_stmt::store_result`.
fn store_result(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<bool, Thrown> {
    if stmt.result.is_none() || stmt.field_count == 0 {
        return Ok(false);
    }
    if !check_state(stmt, conn) {
        stmt.error
            .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
        return Ok(false);
    }
    stmt.handler = Handler::Store;
    empty_errors(req, stmt, conn);
    conn.stats.inc(Stat::PsBufferedSets);
    if stmt.cursor_exists && !cursor_fetch(req, stmt, conn, u32::MAX)? {
        return Ok(false);
    }
    let Some(result) = stmt.result.as_mut() else {
        return Ok(false);
    };
    if result.store(req, conn, true)? {
        stmt.upsert.affected_rows = result.num_rows();
        stmt.state = StmtState::UseOrStoreCalled;
        return Ok(true);
    }
    stmt.error.copy_from(req, &conn.error)?;
    if let Some(result) = stmt.result.take() {
        result.release(req);
    }
    stmt.state = StmtState::Prepared;
    Ok(false)
}

/// `mysqlnd_stmt::use_result`.
fn use_result(req: &mut Request, stmt: &mut StmtData, conn: &mut ConnData) -> Result<bool, Thrown> {
    if stmt.result.is_none() {
        return Ok(false);
    }
    if stmt.field_count == 0 || !check_state(stmt, conn) {
        conn.error
            .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
        return Ok(false);
    }
    stmt.error.reset(req);
    conn.stats.inc(Stat::PsUnbufferedSets);
    let cursor = stmt.cursor_exists.then_some(stmt.stmt_id);
    if let Some(result) = stmt.result.as_mut() {
        result.use_unbuffered(req, true)?;
        result.cursor = cursor;
    }
    stmt.state = StmtState::UseOrStoreCalled;
    Ok(true)
}

/// `stmt->default_rset_handler(s)`, whose answer php does not read.
fn run_handler(req: &mut Request, stmt: &mut StmtData, conn: &mut ConnData) -> Result<(), Thrown> {
    match stmt.handler {
        Handler::Use => {
            use_result(req, stmt, conn)?;
        }
        Handler::Store => {
            store_result(req, stmt, conn)?;
        }
    }
    Ok(())
}

/// `mysqlnd_stmt::get_result`.
fn get_result(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<Option<Box<Res>>, Thrown> {
    if stmt.result.is_none() || stmt.field_count == 0 {
        return Ok(None);
    }
    if !check_state(stmt, conn) {
        stmt.error
            .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
        return Ok(None);
    }
    empty_errors(req, stmt, conn);
    conn.stats.inc(Stat::BufferedSets);
    if stmt.cursor_exists && !cursor_fetch(req, stmt, conn, u32::MAX)? {
        return Ok(None);
    }
    let fields = stmt.result.as_ref().map_or(&[][..], |r| &r.fields[..]);
    let mut result = Box::new(Res::with_fields(req, stmt.field_count, fields)?);
    let stored = result.store(req, conn, true)?;
    stmt.state = StmtState::Prepared;
    if !stored {
        stmt.error.copy_from(req, &conn.error)?;
        result.free(req, conn, true)?;
        return Ok(None);
    }
    stmt.upsert.affected_rows = result.num_rows();
    Ok(Some(result))
}

/// `mysqlnd_stmt::fetch`: the rows asked for on the first fetch, then the
/// next row assigned into each bound column (a reference assigned
/// through) and the values of the unbound ones given back.
fn fetch(req: &mut Request, stmt: &mut StmtData, conn: &mut ConnData) -> Result<StmtFetch, Thrown> {
    if stmt.result.is_none() || stmt.state < StmtState::WaitingUseOrStore {
        stmt.error
            .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
        return Ok(StmtFetch::Failed);
    }
    if stmt.state == StmtState::WaitingUseOrStore {
        run_handler(req, stmt, conn)?;
    }
    stmt.state = StmtState::UserFetching;
    empty_errors(req, stmt, conn);
    let Some(result) = stmt.result.as_mut() else {
        return Ok(StmtFetch::Failed);
    };
    let cursor = result.cursor.is_some();
    let got = result.fetch_row(req, conn, stmt.result_bind.is_some())?;
    if cursor {
        let (warnings, server_status) = result.cursor_status;
        stmt.upsert.reset();
        stmt.upsert.warning_count = warnings;
        stmt.upsert.server_status = server_status;
    }
    match got {
        Ok(Some(mut row)) => {
            let binds = stmt.result_bind.as_mut();
            assign_row(req, binds, &mut row)?;
            result.recycle(row);
            Ok(StmtFetch::Row)
        }
        Ok(None) => Ok(StmtFetch::NoData),
        Err(()) => {
            if conn.error.errno != 0 {
                stmt.error.copy_from(req, &conn.error)?;
            }
            Ok(StmtFetch::Failed)
        }
    }
}

/// `ZEND_TRY_ASSIGN_VALUE_EX` of each fetched value into its bound
/// column; the values of the unbound ones given back.
fn assign_row(
    req: &mut Request,
    binds: Option<&mut Binds<ResultBind>>,
    row: &mut Row,
) -> Result<(), Thrown> {
    let Some(binds) = binds else {
        for value in row.drain(..) {
            value.give_up(req);
        }
        return Ok(());
    };
    let mut values = row.drain(..);
    for bind in binds.iter_mut() {
        let Some(value) = values.next() else { break };
        if !bind.bound {
            value.give_up(req);
            continue;
        }
        let cell = bind.zv.as_deref().and_then(|old| match old.as_ref() {
            ValueRef::Reference(r) => Some(r),
            _ => None,
        });
        if let Some(cell) = cell {
            // The row's owner of the value handed to the cell.
            if let Err(thrown) = cell.set(req, value) {
                // A typed reference refused it: the values not yet
                // assigned are given back, as at the end.
                for value in values {
                    value.give_up(req);
                }
                return Err(thrown);
            }
        } else if let Some(old) = bind.zv.replace(value) {
            old.give_up(req);
        }
    }
    for value in values {
        value.give_up(req);
    }
    Ok(())
}

/// `mysqlnd_stmt::next_result`.
fn next_result(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<bool, Thrown> {
    if stmt.result.is_none()
        || conn.state != ConnState::NextResultPending
        || conn.upsert.server_status & status::MORE_RESULTS_EXISTS == 0
    {
        return Ok(false);
    }
    free_stmt_result(req, stmt, conn)?;
    parse_execute_response(req, stmt, conn)
}

/// `mysqlnd_stmt::flush`: each set of rows of the `COM_STMT_EXECUTE` asked for and
/// read off the line.
fn flush(req: &mut Request, stmt: &mut StmtData, conn: &mut ConnData) -> Result<(), Thrown> {
    if stmt.stmt_id == 0 {
        return Ok(());
    }
    loop {
        if stmt.state == StmtState::WaitingUseOrStore {
            run_handler(req, stmt, conn)?;
            stmt.state = StmtState::UserFetching;
        }
        if let Some(result) = stmt.result.as_mut() {
            result.skip(req, conn)?;
        }
        if !(conn.more_rowsets() && next_result(req, stmt, conn)?) {
            return Ok(());
        }
    }
}

/// `mysqlnd_stmt::free_result`.
fn free_result(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<bool, Thrown> {
    if stmt.result.is_none() {
        return Ok(true);
    }
    if stmt.state == StmtState::WaitingUseOrStore {
        stmt.handler = Handler::Use;
        use_result(req, stmt, conn)?;
    }
    if stmt.state > StmtState::WaitingUseOrStore {
        if let Some(result) = stmt.result.as_mut() {
            result.skip(req, conn)?;
        }
        separate_result_bind(req, stmt);
        if let Some(result) = stmt.result.as_mut() {
            result.free_buffers(req);
        }
    }
    if stmt.state > StmtState::Prepared {
        stmt.state = StmtState::Prepared;
    }
    Ok(true)
}

/// `mysqlnd_stmt::reset`.
fn reset(req: &mut Request, stmt: &mut StmtData, conn: &mut ConnData) -> Result<bool, Thrown> {
    empty_errors(req, stmt, conn);
    if stmt.stmt_id == 0 {
        return Ok(true);
    }
    if let Some(binds) = stmt.param_bind.as_mut() {
        for bind in binds.iter_mut() {
            bind.blob_used = false;
        }
    }
    flush(req, stmt, conn)?;
    let mut ok = true;
    if conn.state == ConnState::Ready {
        ok = conn.send_command(req, Com::StmtReset, &stmt.stmt_id.to_le_bytes(), false)?
            && conn.handle_response(req, false, false, Com::StmtReset, true)?;
        if !ok {
            stmt.error.copy_from(req, &conn.error)?;
        }
    }
    stmt.upsert = conn.upsert;
    Ok(ok)
}

/// `mysqlnd_stmt::send_long_data`.
fn send_long_data(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
    n: u32,
    data: &[u8],
) -> Result<bool, Thrown> {
    empty_errors(req, stmt, conn);
    if stmt.state < StmtState::Prepared {
        stmt.error.client(req, cr::NO_PREPARE_STMT, NOT_PREPARED)?;
        return Ok(false);
    }
    let Some(binds) = stmt.param_bind.as_mut() else {
        stmt.error
            .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
        return Ok(false);
    };
    let Some(bind) = binds
        .get_mut(n as usize)
        .filter(|_| n < stmt.parameter_count)
    else {
        stmt.error
            .client(req, cr::INVALID_PARAMETER_NO, "Invalid parameter number")?;
        return Ok(false);
    };
    if bind.ty != Some(FieldType::LongBlob) {
        stmt.error
            .client(req, cr::INVALID_BUFFER_USE, NOT_BOUND_AS_BLOB)?;
        return Ok(false);
    }
    if conn.state != ConnState::Ready {
        return Ok(false);
    }
    bind.blob_used = true;
    let mut payload = stmt.stmt_id.to_le_bytes().to_vec();
    payload.extend_from_slice(&n.to_le_bytes()[..2]);
    payload.extend_from_slice(data);
    let sent = conn.send_command(req, Com::StmtSendLongData, &payload, false)?;
    if !sent {
        stmt.error.copy_from(req, &conn.error)?;
    }
    Ok(sent)
}

// ---- freeing

/// `mysqlnd_stmt_separate_result_bind`: each bound column's value given
/// back, the binding dropped.
fn separate_result_bind(req: &mut Request, stmt: &mut StmtData) {
    if let Some(binds) = stmt.result_bind.take() {
        for bind in binds.free(req) {
            if let Some(v) = bind.zv {
                v.give_up(req);
            }
        }
    }
}

/// `mysqlnd_stmt::free_stmt_result`.
fn free_stmt_result(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
) -> Result<(), Thrown> {
    separate_result_bind(req, stmt);
    if let Some(result) = stmt.result.take() {
        result.free(req, conn, true)?;
    }
    stmt.error.clean(req);
    Ok(())
}

/// The parameters' half of `mysqlnd_stmt::free_stmt_content`, and the
/// bound columns' values given back, which the statement does whether or
/// not its connection is there to free its rows through
/// ([`free_stmt_result`]).
fn free_stmt_content(req: &mut Request, stmt: &mut StmtData) {
    if let Some(binds) = stmt.param_bind.take() {
        for v in binds.free(req).into_iter().filter_map(|b| b.zv) {
            v.give_up(req);
        }
    }
    separate_result_bind(req, stmt);
}

/// `mysqlnd_stmt::close_on_server`: the rows on the line read off, the
/// statement closed with `COM_STMT_CLOSE` while the connection is ready,
/// counted by how often it ran.
fn close_on_server(
    req: &mut Request,
    stmt: &mut StmtData,
    conn: &mut ConnData,
    implicit: bool,
) -> Result<bool, Thrown> {
    empty_errors(req, stmt, conn);
    loop {
        if stmt.state == StmtState::WaitingUseOrStore {
            run_handler(req, stmt, conn)?;
            stmt.state = StmtState::UserFetching;
        }
        if let Some(result) = stmt.result.as_mut() {
            result.skip(req, conn)?;
        }
        if !(conn.more_rowsets() && next_result(req, stmt, conn)?) {
            break;
        }
    }
    if stmt.stmt_id != 0 {
        conn.stats.global(
            if implicit {
                Stat::FreeResultImplicit
            } else {
                Stat::FreeResultExplicit
            },
            1,
        );
        if conn.state == ConnState::Ready
            && !conn.send_command(req, Com::StmtClose, &stmt.stmt_id.to_le_bytes(), false)?
        {
            stmt.error.copy_from(req, &conn.error)?;
            return Ok(false);
        }
    }
    match stmt.execute_count {
        0 => conn.stats.inc(Stat::PsPreparedNeverExecuted),
        1 => conn.stats.inc(Stat::PsPreparedOnceUsed),
        _ => {}
    }
    if let Some(result) = stmt.result.take() {
        result.free(req, conn, true)?;
    }
    Ok(true)
}
