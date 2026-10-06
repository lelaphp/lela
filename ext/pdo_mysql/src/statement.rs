//! `mysql_statement.c` behind PDO's statement face: a query sent as text
//! where prepares are emulated, a server-side statement otherwise; its
//! rows buffered or read one at a time as `USE_BUFFERED_QUERY` says at
//! `executer` call; the columns' descriptions and the `getColumnMeta` rows.

use php_ext::request::{Request, Stream};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};
use php_ext_mysqlnd::client::{
    Fetched, Field, FieldType, ResultSet, Stmt as Ps, StmtFetch, column,
};
use php_ext_pdo::connector::{
    BindEvent, BindKind, Binding, Column, Executed, Failure, Orientation, SqlError, SqlState,
    Statement,
};

use crate::driver::{H, error_of, handle};

/// `pdo_mysql_stmt`.
pub(crate) struct Stmt {
    /// `S->H`, by number.
    h: usize,
    /// `S->result` of a text query.
    result: Option<ResultSet>,
    /// `S->stmt`.
    pub(crate) ps: Option<Ps>,
    /// Whether a server-side statement's `mysql_stmt_execute` left rows,
    /// `S->result` from `mysqlnd_stmt_result_metadata`.
    has_metadata: bool,
    /// `S->fields`.
    fields: Vec<Field>,
    /// `S->current_row`, one owner of each value.
    current_row: Vec<Value>,
    /// `S->num_params`.
    pub(crate) num_params: u32,
    /// `S->max_length`.
    pub(crate) max_length: bool,
    /// `S->done`: every set of rows read.
    done: bool,
    /// The parameters `EXEC_PRE` was dispatched for, which PDO's
    /// `bound_params` holds.
    bound: u32,
}

impl Stmt {
    pub(crate) fn new(h: usize) -> Stmt {
        Stmt {
            h,
            result: None,
            ps: None,
            has_metadata: false,
            fields: Vec::new(),
            current_row: Vec::new(),
            num_params: 0,
            max_length: false,
            done: false,
            bound: 0,
        }
    }

    /// The connection's `pdo_mysql_db_handle`.
    fn handle(&self, req: &mut Request) -> Result<H, Failure> {
        handle(req, self.h).ok_or_else(Failure::silent)
    }

    /// `pdo_mysql_error_stmt`.
    fn error(&self, req: &mut Request) -> Failure {
        match handle(req, self.h) {
            Some(h) => error_of(req, h.server, self.ps.as_ref()),
            None => Failure::silent(),
        }
    }

    /// Whether there are rows to read, `S->result`.
    fn has_result(&self) -> bool {
        self.result.is_some() || self.has_metadata
    }

    /// `pdo_mysql_free_result`.
    fn free_rows(&mut self, req: &mut Request) -> Result<(), Failure> {
        for value in std::mem::take(&mut self.current_row) {
            req.release(value);
        }
        self.has_metadata = false;
        if let Some(result) = self.result.take() {
            result.free(req, false)?;
        }
        Ok(())
    }

    /// `pdo_mysql_fill_stmt_from_result`.
    fn fill_from_result(&mut self, req: &mut Request) -> Result<Executed, Failure> {
        let h = self.handle(req)?;
        let affected = h.server.status(req).affected_rows;
        if affected != u64::MAX {
            return Ok(Executed {
                columns: 0,
                rows: Some(affected.cast_signed()),
            });
        }
        let result = if h.buffered {
            h.server.store(req)?
        } else {
            h.server.use_unbuffered(req)
        };
        let Some(result) = result else {
            return Err(self.error(req));
        };
        let rows = result.num_rows().cast_signed();
        let columns = result.num_fields() as usize;
        self.fields = result.fields().to_vec();
        self.result = Some(result);
        Ok(Executed {
            columns,
            rows: Some(rows),
        })
    }

    /// `pdo_mysql_stmt_after_execute_prepared`: every column bound, the
    /// column descriptions kept, the rows fetched now where buffered, the affected
    /// rows set where the server sent a number.
    fn after_execute_prepared(&mut self, req: &mut Request) -> Result<Executed, Failure> {
        let h = self.handle(req)?;
        let Some(ps) = self.ps.as_mut() else {
            return Err(Failure::silent());
        };
        let columns = ps.field_count();
        for i in 0..columns {
            ps.bind_column(req, i);
        }
        if let Some(fields) = ps.fields() {
            self.fields = fields;
            self.has_metadata = true;
            if h.buffered && !ps.store(req)? {
                return Err(self.error(req));
            }
        }
        let affected = self
            .ps
            .as_ref()
            .map_or(u64::MAX, |ps| ps.status(req).affected_rows);
        Ok(Executed {
            columns: columns as usize,
            rows: (affected != u64::MAX).then_some(affected.cast_signed()),
        })
    }

    /// The results left after this statement read and dropped, as the
    /// destructor and `closeCursor` read them; `false` where reading one
    /// failed.
    fn drain(&mut self, req: &mut Request) -> Result<bool, Failure> {
        let h = self.handle(req)?;
        while h.server.status(req).more_results {
            if !h.server.next_rowset(req)? {
                return Ok(false);
            }
            if let Some(result) = h.server.store(req)? {
                result.free(req, false)?;
            }
        }
        Ok(true)
    }
}

/// `zval_ptr_dtor(parameter); ZVAL_STR(parameter, ...)`: the bound value
/// replaced, through its reference for `bindParam()`.
fn replace(req: &mut Request, value: &mut Value, new: Value) {
    if let Some(cell) = value.as_php_ref().copied() {
        cell.convert(req, new);
    } else {
        let old = std::mem::replace(value, new);
        req.release(old);
    }
}

/// `type_to_name_native`.
fn native_type(ty: FieldType) -> Option<&'static str> {
    Some(match ty {
        FieldType::String => "STRING",
        FieldType::VarString => "VAR_STRING",
        FieldType::Tiny => "TINY",
        FieldType::Bit => "BIT",
        FieldType::Short => "SHORT",
        FieldType::Long => "LONG",
        FieldType::LongLong => "LONGLONG",
        FieldType::Int24 => "INT24",
        FieldType::Float => "FLOAT",
        FieldType::Double => "DOUBLE",
        FieldType::Decimal => "DECIMAL",
        FieldType::NewDecimal => "NEWDECIMAL",
        FieldType::Geometry => "GEOMETRY",
        FieldType::Timestamp => "TIMESTAMP",
        FieldType::Year => "YEAR",
        FieldType::Set => "SET",
        FieldType::Enum => "ENUM",
        FieldType::Date => "DATE",
        FieldType::NewDate => "NEWDATE",
        FieldType::Vector => "VECTOR",
        FieldType::Json => "JSON",
        FieldType::Time => "TIME",
        FieldType::DateTime => "DATETIME",
        FieldType::TinyBlob => "TINY_BLOB",
        FieldType::MediumBlob => "MEDIUM_BLOB",
        FieldType::LongBlob => "LONG_BLOB",
        FieldType::Blob => "BLOB",
        FieldType::Null => "NULL",
        FieldType::VarChar | FieldType::Other(_) => return None,
    })
}

impl Statement for Stmt {
    /// `pdo_mysql_stmt_execute`.
    fn execute(
        &mut self,
        req: &mut Request,
        executed: bool,
        query: &[u8],
    ) -> Result<Executed, Failure> {
        let _ = executed;
        self.free_rows(req)?;
        self.done = false;
        if self.ps.is_some() {
            let bound = std::mem::take(&mut self.bound);
            if bound < self.num_params {
                return Err(Failure::error(SqlError {
                    state: SqlState(*b"HY093"),
                    code: 0,
                    message: Vec::new(),
                }));
            }
            let ran = match self.ps.as_mut() {
                Some(ps) => ps.execute(req)?,
                None => false,
            };
            if !ran {
                return Err(self.error(req));
            }
            return self.after_execute_prepared(req);
        }
        let h = self.handle(req)?;
        if !h.server.query(req, query)? {
            return Err(self.error(req));
        }
        self.fill_from_result(req)
    }

    /// `pdo_mysql_stmt_fetch`.
    fn fetch(
        &mut self,
        req: &mut Request,
        orientation: Orientation,
        offset: i64,
    ) -> Result<bool, Failure> {
        let _ = (orientation, offset);
        if !self.has_result() {
            return Ok(false);
        }
        if let Some(ps) = self.ps.as_mut() {
            return match ps.fetch(req)? {
                StmtFetch::Row => Ok(true),
                StmtFetch::NoData | StmtFetch::Failed => match self.error(req) {
                    failure @ Failure {
                        cause: php_ext_pdo::connector::Cause::Error(SqlError { code: 1.., .. }),
                        ..
                    } => Err(failure),
                    _ => Ok(false),
                },
            };
        }
        let Some(result) = self.result.as_mut() else {
            return Ok(false);
        };
        match result.fetch(req)? {
            Fetched::Row(row) => {
                for value in std::mem::replace(&mut self.current_row, row) {
                    req.release(value);
                }
                Ok(true)
            }
            Fetched::Done => Ok(false),
            Fetched::Failed => Err(self.error(req)),
        }
    }

    /// `pdo_mysql_stmt_describe`: a column's name, or `table.name` under
    /// `ATTR_FETCH_TABLE_NAMES`; its decimals and width.
    fn describe(&mut self, req: &mut Request, column: usize) -> Result<Column, Failure> {
        if !self.has_result() {
            return Err(Failure::silent());
        }
        let h = self.handle(req)?;
        let Some(field) = self.fields.get(column) else {
            return Err(Failure::silent());
        };
        let name = if h.fetch_table_names {
            let mut name = field.table.clone();
            name.push(b'.');
            name.extend_from_slice(&field.name);
            name
        } else {
            field.name.clone()
        };
        Ok(Column {
            name,
            max_len: field.length,
            precision: u64::from(field.decimals),
        })
    }

    /// `pdo_mysql_stmt_get_col`.
    fn value(
        &mut self,
        req: &mut Request,
        column: usize,
        ty: Option<BindKind>,
    ) -> Result<Value, Failure> {
        let _ = ty;
        if !self.has_result() || column >= self.fields.len() {
            return Err(Failure::silent());
        }
        if let Some(ps) = &self.ps {
            return Ok(ps.column_value(req, u32::try_from(column).unwrap_or(u32::MAX)));
        }
        let value = self
            .current_row
            .get(column)
            .copied()
            .unwrap_or(Value::null());
        req.addref(&value);
        Ok(value)
    }

    /// `pdo_mysql_stmt_param_hook`: a server-side statement's parameter
    /// checked at `ALLOC` and bound at `EXEC_PRE` by its value's type, a
    /// LOB's stream read into a string first.
    fn bind(
        &mut self,
        req: &mut Request,
        param: Binding<'_>,
        event: BindEvent,
    ) -> Result<(), Failure> {
        let Some(ps) = self.ps.as_mut() else {
            return Ok(());
        };
        if !param.is_parameter {
            return Ok(());
        }
        match event {
            BindEvent::Alloc => {
                if *param.position < 0 || *param.position >= i64::from(self.num_params) {
                    return Err(Failure::error(SqlError {
                        state: SqlState(*b"HY093"),
                        code: 0,
                        message: Vec::new(),
                    }));
                }
                Ok(())
            }
            BindEvent::ExecPre => {
                self.bound += 1;
                let n = u32::try_from(*param.position).unwrap_or(u32::MAX);
                let value = param.value.deref();
                if param.ty.kind() == BindKind::Null || matches!(value.as_ref(), ValueRef::Null) {
                    ps.bind_parameter(req, n, Value::null(), FieldType::Null);
                    return Ok(());
                }
                match param.ty.kind() {
                    BindKind::Stmt => return Err(Failure::silent()),
                    BindKind::Lob if matches!(value.as_ref(), ValueRef::Resource(_)) => {
                        let Some(stream) = Stream::from_value(req, &value) else {
                            return Err(Failure::raised(
                                SqlState(*b"HY105"),
                                "Expected a stream resource",
                            ));
                        };
                        let text = stream.copy_to_mem(req, None)?;
                        replace(req, param.value, Value::string(text.hand_on_unmigrated()));
                    }
                    _ => {}
                }
                let value = param.value.deref();
                let ty = match value.as_ref() {
                    ValueRef::Str(_) => FieldType::VarString,
                    ValueRef::Int(_) => FieldType::LongLong,
                    ValueRef::Bool(_) => FieldType::Tiny,
                    ValueRef::Float(_) => FieldType::Double,
                    _ => return Err(Failure::silent()),
                };
                req.addref(&value);
                ps.bind_parameter(req, n, value, ty);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// `pdo_mysql_stmt_col_meta`.
    fn column_meta(
        &mut self,
        req: &mut Request,
        column: usize,
        executed: bool,
    ) -> Result<PhpArray, Failure> {
        let _ = executed;
        if !self.has_result() {
            return Err(Failure::silent());
        }
        let Some(field) = self.fields.get(column).cloned() else {
            return Err(Failure::silent());
        };
        let mut meta = PhpArray::new(req)?;
        let mut flags = PhpArray::new(req)?;
        let set = |req: &mut Request, meta: &mut Owner<PhpArray>, key: &[u8], value: Value| {
            let key = Key::Str(PhpString::new(req, key)?.hand_on_unmigrated());
            meta.set(req, key, value)
        };
        if let Some(def) = &field.def {
            let text = Value::string(PhpString::new(req, def)?.hand_on_unmigrated());
            set(req, &mut meta, b"mysql:def", text)?;
        }
        for (bit, name) in [
            (column::NOT_NULL, &b"not_null"[..]),
            (column::PRI_KEY, b"primary_key"),
            (column::MULTIPLE_KEY, b"multiple_key"),
            (column::UNIQUE_KEY, b"unique_key"),
            (column::BLOB, b"blob"),
        ] {
            if field.flags & bit != 0 {
                let text = Value::string(PhpString::new(req, name)?.hand_on_unmigrated());
                flags.append(req, text)?;
            }
        }
        let ty = field.field_type();
        if let Some(name) = native_type(ty) {
            let text = Value::string(PhpString::new(req, name.as_bytes())?.hand_on_unmigrated());
            set(req, &mut meta, b"native_type", text)?;
        }
        let pdo_type = match ty {
            FieldType::Bit
            | FieldType::Year
            | FieldType::Tiny
            | FieldType::Short
            | FieldType::Int24
            | FieldType::Long
            | FieldType::LongLong => 1,
            _ => 2,
        };
        set(req, &mut meta, b"pdo_type", Value::int(pdo_type))?;
        set(
            req,
            &mut meta,
            b"flags",
            flags.into_value().hand_on_unmigrated(),
        )?;
        let table = Value::string(PhpString::new(req, &field.table)?.hand_on_unmigrated());
        set(req, &mut meta, b"table", table)?;
        Ok(meta.hand_on_unmigrated())
    }

    /// `pdo_mysql_stmt_next_rowset`.
    fn next_rowset(&mut self, req: &mut Request) -> Result<Executed, Failure> {
        self.free_rows(req)?;
        if let Some(ps) = self.ps.as_mut() {
            ps.free_rows(req)?;
            if !ps.next_rowset(req)? {
                self.done = true;
                return Err(self.error(req));
            }
            return self.after_execute_prepared(req);
        }
        let h = self.handle(req)?;
        if !h.server.next_rowset(req)? {
            self.done = true;
            return Err(self.error(req));
        }
        self.fill_from_result(req)
    }

    /// `pdo_mysql_stmt_cursor_closer`.
    fn close_cursor(&mut self, req: &mut Request) -> Result<(), Failure> {
        self.done = true;
        self.free_rows(req)?;
        if let Some(ps) = self.ps.as_mut() {
            ps.free_rows(req)?;
        }
        if !self.drain(req)? {
            return Err(self.error(req));
        }
        Ok(())
    }

    /// `pdo_mysql_stmt_dtor`.
    fn close(&mut self, req: &mut Request) {
        let _ = self.free_rows(req);
        if let Some(ps) = self.ps.take() {
            let _ = ps.close(req, false);
        }
        if !self.done {
            let _ = self.drain(req);
        }
    }
}
