//! `mysql_statement.c` behind PDO's statement face: a query sent as text
//! where prepares are emulated, a server-side statement otherwise; its
//! rows buffered or read one at a time as `USE_BUFFERED_QUERY` says at
//! `executer` call; the columns' descriptions and the `getColumnMeta` rows.

use php_ext::raising::Thrown;
use php_ext::request::{Request, Stream};
use php_ext::values::{Emalloc, Owner, PhpArray, PhpString, Value, ValueRef};
use php_ext_mysqlnd::client::{
    Fetched, Field, FieldType, ResultSet, Stmt as Ps, StmtFetch, column,
};
use php_ext_pdo::connector::{
    BindEvent, BindKind, Binding, Column, Executed, Failure, Orientation, SqlState, Statement,
};

use crate::driver::{Einfo, H, error_of, handle, persistent};

/// `sizeof(pdo_mysql_stmt)`.
const PDO_MYSQL_STMT: usize = 88;
/// `sizeof(zval)`.
const ZVAL: usize = 16;

/// `pdo_mysql_stmt`, in the allocation php `ecalloc`s for it, which the
/// statement's `dtor` gives back.
pub(crate) struct Stmt {
    /// `S->H`, by number.
    h: usize,
    /// `S->result`: a text query's rows, or the columns
    /// `mysqlnd_stmt_result_metadata` describes of a server-side
    /// statement's rows; `S->fields` are its.
    result: Option<ResultSet>,
    /// `S->stmt`.
    pub(crate) ps: Option<Ps>,
    /// `S->current_row`.
    current_row: Option<CurrentRow>,
    /// `S->num_params`.
    pub(crate) num_params: u32,
    /// `S->max_length`.
    pub(crate) max_length: bool,
    /// `S->done`: every set of rows read.
    done: bool,
    /// The parameters `EXEC_PRE` was dispatched for, which PDO's
    /// `bound_params` holds.
    bound: u32,
    /// The `pdo_mysql_stmt` itself, none once the `dtor` gave it back.
    held: Option<Emalloc>,
    /// `S->einfo`.
    pub(crate) einfo: Einfo,
}

/// `S->current_row`: a text row's values, one owner of each, in the
/// `ecalloc(column_count, sizeof(zval))` the first fetch makes.
struct CurrentRow {
    values: Vec<Owner<Value>>,
    held: Emalloc,
}

impl Stmt {
    /// `ecalloc(1, sizeof(pdo_mysql_stmt))` for the connection `h`; the
    /// `Err` is the memory limit's fatal. What `memory_get_usage()` sees of
    /// a statement past its rows is the `pdo_mysql_stmt`, PDO's columns, a
    /// text row's `current_row` and a server-side statement's
    /// `mysqlnd_stmt_result_metadata`, the last two given back by
    /// `closeCursor()`, all of it by the statement's free:
    ///
    /// ```php
    /// $p = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
    /// $m = memory_get_usage();
    /// $st = $p->query("SELECT REPEAT('x', 100000) a");
    /// echo memory_get_usage() - $m, "\n";
    /// $st->fetchAll();
    /// echo memory_get_usage() - $m, "\n";
    /// unset($st);
    /// echo memory_get_usage() - $m, "\n";
    /// $p->setAttribute(PDO::ATTR_EMULATE_PREPARES, false);
    /// $st = $p->prepare("SELECT REPEAT('x', 1000000) a, REPEAT('y', 1000000) b");
    /// echo memory_get_usage() - $m, "\n";
    /// $st->execute();
    /// echo memory_get_usage() - $m, "\n";
    /// $st->closeCursor();
    /// echo memory_get_usage() - $m, "\n";
    /// unset($st);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect mysql
    /// 221544
    /// 323992
    /// 32
    /// 21792
    /// 4044176
    /// 21840
    /// 32
    /// ```
    pub(crate) fn new(req: &mut Request, h: usize) -> Result<Stmt, Thrown> {
        Ok(Stmt {
            h,
            result: None,
            ps: None,
            current_row: None,
            num_params: 0,
            max_length: false,
            done: false,
            bound: 0,
            held: Some(Emalloc::new(req, PDO_MYSQL_STMT)?),
            einfo: Einfo::default(),
        })
    }

    /// `S->fields`: the columns of the rows, none where there are none.
    fn fields(&self) -> &[Field] {
        self.result.as_ref().map_or(&[], ResultSet::fields)
    }

    /// The connection's `pdo_mysql_db_handle`.
    fn handle(&self, req: &mut Request) -> Result<H, Failure> {
        handle(req, self.h).ok_or_else(Failure::silent)
    }

    /// `pdo_mysql_error_stmt`: the statement's error, its code and message
    /// kept on `S->einfo`.
    fn error(&mut self, req: &mut Request) -> Failure {
        let Some(h) = handle(req, self.h) else {
            return Failure::silent();
        };
        let failure = error_of(req, h.server, self.ps.as_ref());
        let persistent = persistent(req, self.h);
        self.einfo.keep(req, failure, persistent)
    }

    /// Whether there are rows to read, `S->result`.
    fn has_result(&self) -> bool {
        self.result.is_some()
    }

    /// `pdo_mysql_free_result`.
    fn free_rows(&mut self, req: &mut Request) -> Result<(), Failure> {
        if let Some(row) = self.current_row.take() {
            for value in row.values {
                value.give_up(req);
            }
            row.held.free(req);
        }
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
            h.server.use_unbuffered(req)?
        };
        let Some(result) = result else {
            return Err(self.error(req));
        };
        let rows = result.num_rows().cast_signed();
        let columns = result.num_fields() as usize;
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
            ps.bind_column(req, i)?;
        }
        self.result = ps.described(req)?;
        if self.result.is_some() && h.buffered && !ps.store(req)? {
            return Err(self.error(req));
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
                return Err(Failure::error(SqlState(*b"HY093")));
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
                StmtFetch::NoData => Ok(false),
                StmtFetch::Failed => {
                    let failure = self.error(req);
                    if self.einfo.code() == 0 {
                        Ok(false)
                    } else {
                        Err(failure)
                    }
                }
            };
        }
        let Some(result) = self.result.as_mut() else {
            return Ok(false);
        };
        let columns = result.num_fields() as usize;
        match result.fetch(req)? {
            Fetched::Row(row) => {
                if self.current_row.is_none() {
                    let held = match Emalloc::new(req, columns * ZVAL) {
                        Ok(held) => held,
                        Err(thrown) => {
                            for value in row {
                                value.give_up(req);
                            }
                            return Err(thrown.into());
                        }
                    };
                    self.current_row = Some(CurrentRow {
                        values: Vec::new(),
                        held,
                    });
                }
                if let Some(current) = self.current_row.as_mut() {
                    for value in std::mem::replace(&mut current.values, row) {
                        value.give_up(req);
                    }
                }
                Ok(true)
            }
            Fetched::Done => Ok(false),
            Fetched::Failed => Err(self.error(req)),
        }
    }

    /// `pdo_mysql_stmt_describe`: a column's name, the interned string
    /// mysqlnd read it as, or `table.name` under `ATTR_FETCH_TABLE_NAMES`;
    /// its decimals and width. A row fetched by name is keyed by the
    /// name, no string made for it:
    ///
    /// ```php
    /// $p = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
    /// $st = $p->query("SELECT 1 AS firstcol, 2 AS secondcol UNION SELECT 3, 4 UNION SELECT 5, 6");
    /// $m = memory_get_usage();
    /// $rows = $st->fetchAll(PDO::FETCH_ASSOC);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect mysql
    /// 1376
    /// ```
    fn describe(&mut self, req: &mut Request, column: usize) -> Result<Column, Failure> {
        if !self.has_result() {
            return Err(Failure::silent());
        }
        let h = self.handle(req)?;
        let Some(field) = self.fields().get(column) else {
            return Err(Failure::silent());
        };
        let name = if h.fetch_table_names {
            // `strpprintf(0, "%s.%s", table, name)`.
            let mut name = field.table.clone();
            name.push(b'.');
            name.extend_from_slice(field.name.as_bytes());
            PhpString::filled(req, name.len(), |to| to.copy_from_slice(&name))?
        } else {
            // `zend_string_copy(S->fields[i].sname)`.
            Owner::take(req, *field.name)
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
    ) -> Result<Owner<Value>, Failure> {
        let _ = ty;
        if !self.has_result() || column >= self.fields().len() {
            return Err(Failure::silent());
        }
        if let Some(ps) = &self.ps {
            return Ok(ps.column_value(req, u32::try_from(column).unwrap_or(u32::MAX)));
        }
        let value = self
            .current_row
            .as_ref()
            .and_then(|row| row.values.get(column))
            .map_or(Value::null(), |v| **v);
        Ok(Owner::take(req, value))
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
                    return Err(Failure::error(SqlState(*b"HY093")));
                }
                Ok(())
            }
            BindEvent::ExecPre => {
                self.bound += 1;
                let n = u32::try_from(*param.position).unwrap_or(u32::MAX);
                let value = param.value.deref();
                if param.ty.kind() == BindKind::Null || matches!(value.as_ref(), ValueRef::Null) {
                    ps.bind_parameter(req, n, Owner::null(), FieldType::Null)?;
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
                        param.value.convert(req, text.into_value());
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
                let value = Owner::take(req, value);
                ps.bind_parameter(req, n, value, ty)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// `pdo_mysql_stmt_col_meta`: each string `add_assoc_string`'s, an
    /// allocation of its own, the empty table of a `SELECT` without one
    /// too.
    ///
    /// ```php
    /// $p = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
    /// $st = $p->query("SELECT 1 AS firstcol, 2 AS secondcol");
    /// $st->getColumnMeta(1);
    /// $m = memory_get_usage();
    /// $meta = $st->getColumnMeta(0);
    /// echo memory_get_usage() - $m, "\n";
    /// var_dump($meta['table'], $meta['native_type']);
    /// ```
    /// ```expect mysql
    /// 952
    /// string(0) ""
    /// string(8) "LONGLONG"
    /// ```
    fn column_meta(
        &mut self,
        req: &mut Request,
        column: usize,
        executed: bool,
    ) -> Result<Owner<PhpArray>, Failure> {
        let _ = executed;
        if !self.has_result() {
            return Err(Failure::silent());
        }
        let Some(field) = self.fields().get(column) else {
            return Err(Failure::silent());
        };
        let mut meta = PhpArray::new(req)?;
        let mut flags = PhpArray::new(req)?;
        let set = |req: &mut Request,
                   meta: &mut Owner<PhpArray>,
                   key: &[u8],
                   value: Owner<Value>| { meta.set_str(req, key, value) };
        if let Some(def) = &field.def {
            let text = string_init(req, def)?;
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
                let text = string_init(req, name)?;
                flags.append(req, text)?;
            }
        }
        let ty = field.field_type();
        if let Some(name) = native_type(ty) {
            let text = string_init(req, name.as_bytes())?;
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
        set(req, &mut meta, b"pdo_type", Owner::int(pdo_type))?;
        set(req, &mut meta, b"flags", flags.into_value())?;
        let table = string_init(req, &field.table)?;
        set(req, &mut meta, b"table", table)?;
        Ok(meta)
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
        self.einfo.free(req);
        self.free_rows(req).ok();
        if let Some(ps) = self.ps.take() {
            ps.close(req, false).ok();
        }
        if !self.done {
            self.drain(req).ok();
        }
        if let Some(held) = self.held.take() {
            held.free(req);
        }
    }
}

/// `ZVAL_STRING`'s `zend_string_init`, which `add_assoc_string` and
/// `add_next_index_string` make their value with: an allocation of its
/// own whatever the length, the empty table of a `SELECT` without one
/// too, where [`PhpString::new`] hands out the interned empty and
/// one-byte strings.
fn string_init(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    let made = PhpString::filled(req, bytes.len(), |to| to.copy_from_slice(bytes))?;
    Ok(made.into_value())
}
