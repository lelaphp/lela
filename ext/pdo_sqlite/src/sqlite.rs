//! `sqlite_driver.c` and `sqlite_statement.c` behind PDO's connector face:
//! the factory, the connection's methods and the statement's, the
//! SQLSTATE mapping of `pdo_sqlite_error` and the column types of
//! `pdo_sqlite_stmt_get_col`.

use php_ext::coerce::{to_long, to_string, value_name};
use php_ext::raising::{Level, Thrown, docref, throw};
use php_ext::request::{Request, Stream};
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};
use php_ext_pdo::connector::{
    BindEvent, BindKind, BindType, Binding, Column, Connection, Connector, Executed, Failure,
    OnError, Orientation, Placeholders, Prepared, Source, SqlError, SqlState, Statement,
    long_attribute, option_long,
};

use crate::Sqlite;
use crate::callbacks::Handler;
use crate::ffi::{self, Db, Registration};

/// `PDO_ATTR_TIMEOUT` and `PDO_ATTR_CURSOR`, which the connector reads.
const ATTR_TIMEOUT: i64 = 2;
const ATTR_CURSOR: i64 = 10;
/// `PDO_ATTR_CLIENT_VERSION` and `PDO_ATTR_SERVER_VERSION`.
const ATTR_SERVER_VERSION: i64 = 4;
const ATTR_CLIENT_VERSION: i64 = 5;
/// The connector's own attributes, from `PDO_ATTR_DRIVER_SPECIFIC`,
/// `Pdo\Sqlite`'s constants.
const ATTR_OPEN_FLAGS: i64 = Sqlite::ATTR_OPEN_FLAGS;
const ATTR_READONLY_STATEMENT: i64 = Sqlite::ATTR_READONLY_STATEMENT;
const ATTR_EXTENDED_RESULT_CODES: i64 = Sqlite::ATTR_EXTENDED_RESULT_CODES;
const ATTR_BUSY_STATEMENT: i64 = Sqlite::ATTR_BUSY_STATEMENT;
const ATTR_EXPLAIN_STATEMENT: i64 = Sqlite::ATTR_EXPLAIN_STATEMENT;
const ATTR_TRANSACTION_MODE: i64 = Sqlite::ATTR_TRANSACTION_MODE;

/// `pdo_sqlite_driver`.
pub(crate) struct Driver;

pub(crate) static SQLITE: Driver = Driver;

/// The names of the three methods `get_driver_methods` lays on `PDO`.
const METHODS: [&[u8]; 3] = [
    b"sqlitecreatefunction",
    b"sqlitecreateaggregate",
    b"sqlitecreatecollation",
];

/// `_pdo_sqlite_error`: the SQLSTATE the connection's last error code
/// maps to, with the code and the message, or `00000` and nothing where
/// the code is `SQLITE_OK`.
pub(super) fn error_of(db: &Db) -> SqlError {
    let code = db.errcode();
    if code == ffi::SQLITE_OK {
        return SqlError {
            state: SqlState::NONE,
            code: 0,
            message: Vec::new(),
        };
    }
    SqlError {
        state: state_for(code),
        code: i64::from(code),
        message: db.errmsg(),
    }
}

/// `_pdo_sqlite_error`'s switch over the library's code.
fn state_for(code: i32) -> SqlState {
    SqlState(match code {
        ffi::SQLITE_NOTFOUND => *b"42S02",
        ffi::SQLITE_INTERRUPT => *b"01002",
        ffi::SQLITE_NOLFS => *b"HYC00",
        ffi::SQLITE_TOOBIG => *b"22001",
        ffi::SQLITE_CONSTRAINT => *b"23000",
        _ => *b"HY000",
    })
}

/// The connection's error as a failure, with what a callback threw.
fn failed(db: &Db, thrown: Option<Thrown>) -> Failure {
    Failure {
        thrown,
        ..Failure::error(error_of(db))
    }
}

impl Connector for Driver {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn class(&self) -> Option<&'static [u8]> {
        Some(b"Pdo\\Sqlite")
    }

    fn methods(&self) -> &'static [&'static [u8]] {
        &METHODS
    }

    /// `pdo_sqlite_handle_factory`: the file made safe as
    /// `make_filename_safe` makes it, opened with the bits the options
    /// give, the busy timeout set.
    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure> {
        let Some(filename) = safe_filename(req, Some(source.dsn)) else {
            let shown = String::from_utf8_lossy(source.dsn).into_owned();
            return Err(Failure::from(throw(
                req,
                "PDOException",
                &format!("open_basedir prohibits opening {shown}"),
            )));
        };
        let flags = option_long(
            req,
            source.options,
            ATTR_OPEN_FLAGS,
            i64::from(ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE),
        )?;
        // php hands the library its `zend_long` as an `int`.
        #[allow(clippy::cast_possible_truncation)]
        let mut flags = flags as i32;
        if !open_basedir(req) {
            flags |= ffi::SQLITE_OPEN_URI;
        }
        let db = match Db::open(&filename, flags) {
            Ok(db) => db,
            Err((code, message)) => {
                return Err(Failure::error(SqlError {
                    state: state_for(code),
                    code: i64::from(code),
                    message,
                }));
            }
        };
        let timeout = match source.options {
            Some(options) => option_long(req, Some(options), ATTR_TIMEOUT, 60)?,
            None => 60,
        };
        db.busy_timeout(timeout.saturating_mul(1000));
        Ok(Box::new(Handle {
            db,
            registrations: Vec::new(),
            transaction_mode: 0,
        }))
    }
}

/// Whether `open_basedir` is set, `PG(open_basedir) && *PG(open_basedir)`.
pub(super) fn open_basedir(req: &Request) -> bool {
    req.ini_get(b"open_basedir").is_some_and(|v| !v.is_empty())
}

/// `make_filename_safe`: a `file:` URI as given, refused under
/// `open_basedir`; `:memory:` and the empty name as given; any other
/// name expanded against the request's directory (`expand_filepath`)
/// and held to the `open_basedir` guard, which warns; `None` for no name
/// at all and for a name refused. `PDO::__construct` runs the open under
/// `EH_THROW`, so the guard's warning is a `PDOException` and the
/// refusal `pdo_sqlite` throws after it has that one as its `previous`.
///
/// ```php
/// ini_set('open_basedir', __DIR__);
/// try { new PDO('sqlite:/nope.db'); } catch (PDOException $e) { echo $e->getMessage(), "\n", $e->getPrevious()->getMessage(), "\n"; }
/// try { new PDO('sqlite:file:nope.db'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// open_basedir prohibits opening /nope.db
/// PDO::__construct(): open_basedir restriction in effect. File(/nope.db) is not within the allowed path(s): (%s)
/// open_basedir prohibits opening file:nope.db
/// ```
pub(super) fn safe_filename(req: &mut Request, name: Option<&[u8]>) -> Option<Vec<u8>> {
    let name = name?;
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    let name = &name[..end];
    if name.len() >= 5 && name[..5].eq_ignore_ascii_case(b"file:") {
        return (!open_basedir(req)).then(|| name.to_vec());
    }
    if name.is_empty() || name == b":memory:" {
        return Some(name.to_vec());
    }
    let cwd = req.cwd().into_os_string().into_encoded_bytes();
    let full = req.virtual_file_ex(&cwd, name, php_ext::request::Resolve::FilePath)?;
    let full = req.check_open_basedir(&full, true).ok()?;
    Some(full.as_os_str().as_encoded_bytes().to_vec())
}

/// `pdo_sqlite_db_handle`: the connection, the functions and collations
/// registered on it, and the transaction style.
pub(crate) struct Handle {
    pub(crate) db: Db,
    pub(crate) registrations: Vec<Registration>,
    transaction_mode: i64,
}

impl Handle {
    /// A statement served for its effect under the request, the failure
    /// handed back as the connection's error.
    fn run(&mut self, req: &mut Request, sql: &[u8]) -> Result<(), Failure> {
        match self.db.exec(req, sql) {
            (ffi::SQLITE_OK, None) => Ok(()),
            (ffi::SQLITE_OK, Some(thrown)) => Err(Failure::from(thrown)),
            (_, thrown) => Err(failed(&self.db, thrown)),
        }
    }
}

impl Connection for Handle {
    /// `sqlite_handle_preparer`: a scrollable cursor refused as php
    /// refuses it, with whatever the connection last recorded.
    fn prepare(
        &mut self,
        req: &mut Request,
        sql: &[u8],
        options: Option<PhpArray>,
    ) -> Result<Prepared, Failure> {
        if option_long(req, options, ATTR_CURSOR, 0)? != 0 {
            return Err(failed(&self.db, None));
        }
        match self.db.prepare(req, sql) {
            (Ok(stmt), thrown) => {
                if let Some(thrown) = thrown {
                    return Err(Failure::from(thrown));
                }
                Ok(Prepared {
                    statement: Box::new(Stmt {
                        handle: stmt,
                        pre_fetched: false,
                        done: false,
                        executed: false,
                    }),
                    placeholders: Placeholders::Native {
                        named: true,
                        positional: true,
                    },
                    parameter_map: None,
                })
            }
            (Err(_), thrown) => Err(failed(&self.db, thrown)),
        }
    }

    /// `sqlite_handle_doer`.
    fn exec(&mut self, req: &mut Request, sql: &[u8]) -> Result<i64, Failure> {
        self.run(req, sql)?;
        Ok(self.db.changes())
    }

    /// `sqlite_handle_quoter`: `'...'` with each `'` doubled, `%q`; a
    /// NUL byte refused under the error handling.
    fn quote(
        &mut self,
        req: &mut Request,
        text: &[u8],
        ty: BindType,
        raises: OnError,
    ) -> Result<Vec<u8>, Failure> {
        let _ = ty;
        if text.len() > (i32::MAX as usize - 3) / 2 {
            return Err(Failure::silent());
        }
        if text.contains(&0) {
            let message = "SQLite PDO::quote does not support null bytes";
            return match raises {
                OnError::Throw => Err(Failure::from(throw(req, "PDOException", message))),
                OnError::Warn => {
                    docref(req, Level::Warning, message);
                    Err(Failure::silent())
                }
                OnError::Silent => Err(Failure::silent()),
            };
        }
        let mut quoted = Vec::with_capacity(text.len() + 2);
        quoted.push(b'\'');
        for &b in text {
            quoted.push(b);
            if b == b'\'' {
                quoted.push(b);
            }
        }
        quoted.push(b'\'');
        Ok(quoted)
    }

    /// `sqlite_handle_begin`.
    fn begin(&mut self, req: &mut Request) -> Result<(), Failure> {
        let sql: &[u8] = match self.transaction_mode {
            0 => b"BEGIN DEFERRED TRANSACTION",
            1 => b"BEGIN IMMEDIATE TRANSACTION",
            2 => b"BEGIN EXCLUSIVE TRANSACTION",
            _ => b"BEGIN",
        };
        self.run(req, sql)
    }

    /// `sqlite_handle_commit`.
    fn commit(&mut self, req: &mut Request) -> Result<(), Failure> {
        self.run(req, b"COMMIT")
    }

    /// `sqlite_handle_rollback`.
    fn rollback(&mut self, req: &mut Request) -> Result<(), Failure> {
        self.run(req, b"ROLLBACK")
    }

    /// `pdo_sqlite_in_transaction`: autocommit off.
    fn in_transaction(&mut self, req: &mut Request) -> Option<bool> {
        let _ = req;
        Some(!self.db.autocommit())
    }

    /// `pdo_sqlite_set_attr`.
    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &Value,
    ) -> Result<(), Failure> {
        match attr {
            ATTR_TIMEOUT => {
                let n = long_attribute(req, value)?;
                self.db.busy_timeout(n.saturating_mul(1000));
                Ok(())
            }
            ATTR_EXTENDED_RESULT_CODES => {
                let n = long_attribute(req, value)?;
                self.db.extended_result_codes(n);
                Ok(())
            }
            ATTR_TRANSACTION_MODE => match long_attribute(req, value)? {
                n @ 0..=2 => {
                    self.transaction_mode = n;
                    Ok(())
                }
                _ => Err(Failure::silent()),
            },
            _ => Err(Failure::silent()),
        }
    }

    /// `pdo_sqlite_get_attribute`.
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Value>, Failure> {
        match attr {
            ATTR_CLIENT_VERSION | ATTR_SERVER_VERSION => {
                let version = PhpString::new(req, ffi::version().as_bytes())?;
                Ok(Some(Value::string(version)))
            }
            ATTR_TRANSACTION_MODE => Ok(Some(Value::int(self.transaction_mode))),
            _ => Ok(None),
        }
    }

    /// `pdo_sqlite_last_insert_id`.
    fn last_insert_id(
        &mut self,
        req: &mut Request,
        name: Option<&[u8]>,
    ) -> Result<Vec<u8>, Failure> {
        let _ = (req, name);
        Ok(self.db.last_insert_rowid().to_string().into_bytes())
    }

    fn describes_at_execute(&self) -> bool {
        true
    }

    /// `pdo_sqlite_get_gc`.
    fn gc(&self, visit: &mut dyn FnMut(Value)) {
        if let Some(handler) = self.db.authorizer() {
            visit(handler.value);
        }
        for registration in &self.registrations {
            for handler in registration.handlers() {
                visit(handler.value);
            }
        }
    }

    /// `pdo_sqlite_cleanup_callbacks`: every function and collation
    /// taken off the library and its callables given up.
    fn close(&mut self, req: &mut Request) {
        if let Some(handler) = self.db.set_authorizer(None) {
            handler.release(req);
        }
        for registration in std::mem::take(&mut self.registrations) {
            for handler in self.db.unregister(registration) {
                handler.release(req);
            }
        }
    }
}

/// `pdo_sqlite_stmt`: the statement, and whether a row is waiting and
/// whether the last row was stepped past.
struct Stmt {
    handle: ffi::Stmt,
    pre_fetched: bool,
    done: bool,
    /// `stmt->executed`, as the binding callback reads it.
    executed: bool,
}

impl Stmt {
    fn failed(&self, thrown: Option<Thrown>) -> Failure {
        failed(self.handle.db(), thrown)
    }

    /// `sqlite3_reset` with what an aggregate's final threw carried.
    fn reset(&self, req: &mut Request) -> Result<(), Failure> {
        match self.handle.reset(req) {
            Some(thrown) => Err(Failure::from(thrown)),
            None => Ok(()),
        }
    }
}

/// A value the callback converts in place, through the reference where the
/// parameter holds one, as php's `convert_to_*` writes.
fn replace(req: &mut Request, value: &mut Value, new: Value) {
    if let Some(cell) = value.as_php_ref().copied() {
        cell.convert(req, new);
    } else {
        let old = std::mem::replace(value, new);
        req.release(old);
    }
}

impl Statement for Stmt {
    /// `pdo_sqlite_stmt_execute`.
    fn execute(
        &mut self,
        req: &mut Request,
        executed: bool,
        query: &[u8],
    ) -> Result<Executed, Failure> {
        let _ = query;
        if executed && !self.done {
            self.reset(req)?;
        }
        self.executed = true;
        self.done = false;
        let (rc, thrown) = self.handle.step(req);
        let answered = match rc {
            ffi::SQLITE_ROW => {
                self.pre_fetched = true;
                Ok(Executed {
                    columns: self.handle.data_count(),
                    rows: None,
                })
            }
            ffi::SQLITE_DONE => {
                let done = Executed {
                    columns: self.handle.column_count(),
                    rows: Some(self.handle.db().changes()),
                };
                self.reset(req)?;
                self.done = true;
                Ok(done)
            }
            ffi::SQLITE_ERROR => {
                let failure = self.failed(thrown);
                let _ = self.handle.reset(req);
                return Err(failure);
            }
            _ => return Err(self.failed(thrown)),
        };
        match thrown {
            Some(thrown) => Err(Failure::from(thrown)),
            None => answered,
        }
    }

    /// `pdo_sqlite_stmt_fetch`.
    fn fetch(
        &mut self,
        req: &mut Request,
        orientation: Orientation,
        offset: i64,
    ) -> Result<bool, Failure> {
        let _ = (orientation, offset);
        if self.pre_fetched {
            self.pre_fetched = false;
            return Ok(true);
        }
        if self.done {
            return Ok(false);
        }
        let (rc, thrown) = self.handle.step(req);
        match rc {
            ffi::SQLITE_ROW => match thrown {
                Some(thrown) => Err(Failure::from(thrown)),
                None => Ok(true),
            },
            ffi::SQLITE_DONE => {
                self.done = true;
                self.reset(req)?;
                match thrown {
                    Some(thrown) => Err(Failure::from(thrown)),
                    None => Ok(false),
                }
            }
            ffi::SQLITE_ERROR => {
                let failure = self.failed(thrown);
                let _ = self.handle.reset(req);
                Err(failure)
            }
            _ => Err(self.failed(thrown)),
        }
    }

    /// `pdo_sqlite_stmt_describe`.
    fn describe(&mut self, req: &mut Request, column: usize) -> Result<Column, Failure> {
        let _ = req;
        if column >= self.handle.column_count() {
            return Err(self.failed(None));
        }
        Ok(Column {
            name: self.handle.column_name(column),
            max_len: u64::MAX,
            precision: 0,
        })
    }

    /// `pdo_sqlite_stmt_get_col`: an integer, a float, a string of a
    /// blob or of text, or `null`, as the column's storage class is.
    fn value(
        &mut self,
        req: &mut Request,
        column: usize,
        ty: Option<BindKind>,
    ) -> Result<Value, Failure> {
        let _ = ty;
        if column >= self.handle.data_count() {
            return Err(self.failed(None));
        }
        Ok(match self.handle.column_type(column) {
            ffi::SQLITE_NULL => Value::null(),
            ffi::SQLITE_INTEGER => Value::int(self.handle.column_int64(column)),
            ffi::SQLITE_FLOAT => Value::float(self.handle.column_double(column)),
            ffi::SQLITE_BLOB => {
                Value::string(PhpString::new(req, &self.handle.column_blob(column))?)
            }
            _ => Value::string(PhpString::new(req, &self.handle.column_text(column))?),
        })
    }

    /// `pdo_sqlite_stmt_param_hook` at `PDO_PARAM_EVT_EXEC_PRE`: a named
    /// parameter placed, and the value bound as its type says, converted
    /// in place where php converts it.
    fn bind(
        &mut self,
        req: &mut Request,
        param: Binding<'_>,
        event: BindEvent,
    ) -> Result<(), Failure> {
        if event != BindEvent::ExecPre {
            return Ok(());
        }
        if self.executed && !self.done {
            self.reset(req)?;
            self.done = true;
        }
        if !param.is_parameter {
            return Ok(());
        }
        if *param.position == -1 {
            *param.position = self.handle.parameter_index(param.name.unwrap_or_default()) - 1;
        }
        let at = *param.position + 1;
        let value = param.value.deref();
        let rc = match param.ty.kind() {
            BindKind::Stmt => return Err(Failure::silent()),
            BindKind::Null => self.handle.bind_null(at),
            BindKind::Int | BindKind::Bool => {
                if matches!(value.as_ref(), ValueRef::Null) {
                    self.handle.bind_null(at)
                } else {
                    let n = to_long(req, &value)?;
                    replace(req, param.value, Value::int(n));
                    self.handle.bind_int(at, n)
                }
            }
            BindKind::Lob => {
                let bytes = match value.as_ref() {
                    ValueRef::Resource(_) => {
                        let Some(stream) = Stream::from_value(req, &value) else {
                            return Err(Failure::raised(
                                SqlState(*b"HY105"),
                                "Expected a stream resource",
                            ));
                        };
                        let text = stream.copy_to_mem(req, None)?;
                        replace(req, param.value, Value::string(text));
                        text.as_bytes().to_vec()
                    }
                    ValueRef::Null => {
                        let rc = self.handle.bind_null(at);
                        return if rc == ffi::SQLITE_OK {
                            Ok(())
                        } else {
                            Err(self.failed(None))
                        };
                    }
                    _ => {
                        let text = to_string(req, &value)?;
                        replace(req, param.value, Value::string(text));
                        text.as_bytes().to_vec()
                    }
                };
                return if self.handle.bind_blob(at, &bytes) == ffi::SQLITE_OK {
                    Ok(())
                } else {
                    Err(Failure::silent())
                };
            }
            BindKind::Str | BindKind::Other(_) => {
                if matches!(value.as_ref(), ValueRef::Null) {
                    self.handle.bind_null(at)
                } else {
                    let text = if let Some(text) = value.as_php_string() {
                        *text
                    } else {
                        let text = to_string(req, &value)?;
                        replace(req, param.value, Value::string(text));
                        text
                    };
                    self.handle.bind_text(at, text.as_bytes())
                }
            }
        };
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(self.failed(None))
        }
    }

    /// `pdo_sqlite_stmt_set_attribute`.
    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &Value,
    ) -> Result<(), Failure> {
        if attr != ATTR_EXPLAIN_STATEMENT {
            return Err(Failure::silent());
        }
        let Some(mode) = value.deref().as_int() else {
            let given = value_name(req, value);
            return Err(Failure::from(throw(
                req,
                "TypeError",
                &format!("explain mode must be of type int, {given} given"),
            )));
        };
        if !(0..=2).contains(&mode) {
            return Err(Failure::from(throw(
                req,
                "ValueError",
                "explain mode must be one of the Pdo\\Sqlite::EXPLAIN_MODE_* constants",
            )));
        }
        if self.handle.set_explain_mode(mode) != ffi::SQLITE_OK {
            return Err(Failure::silent());
        }
        Ok(())
    }

    /// `pdo_sqlite_stmt_get_attribute`.
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Value>, Failure> {
        let _ = req;
        Ok(match attr {
            ATTR_READONLY_STATEMENT => Some(Value::bool(self.handle.readonly())),
            ATTR_BUSY_STATEMENT => Some(Value::bool(self.handle.busy())),
            ATTR_EXPLAIN_STATEMENT => Some(Value::int(self.handle.explain_mode())),
            _ => None,
        })
    }

    /// `pdo_sqlite_stmt_col_meta`.
    fn column_meta(
        &mut self,
        req: &mut Request,
        column: usize,
        executed: bool,
    ) -> Result<PhpArray, Failure> {
        if !executed {
            return Err(Failure::silent());
        }
        if column >= self.handle.column_count() {
            return Err(self.failed(None));
        }
        let mut out = PhpArray::new(req)?;
        let mut flags = PhpArray::new(req)?;
        let (native, pdo_type): (&[u8], i64) = match self.handle.column_type(column) {
            ffi::SQLITE_NULL => (b"null", 0),
            ffi::SQLITE_FLOAT => (b"double", 2),
            ffi::SQLITE_BLOB => {
                let blob = PhpString::new(req, b"blob")?;
                flags.append(req, Value::string(blob))?;
                (b"string", 2)
            }
            ffi::SQLITE_INTEGER => (b"integer", 1),
            _ => (b"string", 2),
        };
        set(req, &mut out, b"native_type", Some(native))?;
        let key = Key::Str(PhpString::new(req, b"pdo_type")?);
        out.set(req, key, Value::int(pdo_type))?;
        set(
            req,
            &mut out,
            b"sqlite:decl_type",
            self.handle.column_decltype(column).as_deref(),
        )?;
        set(
            req,
            &mut out,
            b"table",
            self.handle.column_table_name(column).as_deref(),
        )?;
        let key = Key::Str(PhpString::new(req, b"flags")?);
        out.set(req, key, Value::array(flags))?;
        Ok(out)
    }

    /// `pdo_sqlite_stmt_cursor_closer`.
    fn close_cursor(&mut self, req: &mut Request) -> Result<(), Failure> {
        self.executed = false;
        self.reset(req)
    }
}

/// `add_assoc_string` where there is a string.
fn set(
    req: &mut Request,
    out: &mut PhpArray,
    key: &[u8],
    value: Option<&[u8]>,
) -> Result<(), Thrown> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = Value::string(PhpString::new(req, value)?);
    let key = Key::Str(PhpString::new(req, key)?);
    out.set(req, key, value)
}

/// `pdo_sqlite_create_function_internal` and its two kin: a function
/// registered on the connection `this`'s connection.
pub(super) fn register(
    req: &mut Request,
    this: php_ext::values::PhpObject,
    name: &[u8],
    argc: i64,
    flags: i64,
    function: crate::callbacks::Function,
) -> Result<bool, Thrown> {
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<Handle>() else {
            release(req, function);
            return Ok(false);
        };
        // php hands the library its `zend_long`s as `int`s.
        #[allow(clippy::cast_possible_truncation)]
        match handle
            .db
            .create_function(name, argc as i32, flags as i32, function)
        {
            Ok(registration) => {
                handle.registrations.push(registration);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    })
}

/// The handlers of a function that was not registered, given up.
fn release(req: &mut Request, function: crate::callbacks::Function) {
    match function {
        crate::callbacks::Function::Scalar(handler) => handler.release(req),
        crate::callbacks::Function::Aggregate { step, finalize } => {
            step.release(req);
            finalize.release(req);
        }
    }
}

/// `pdo_sqlite_create_collation_internal`.
pub(super) fn register_collation(
    req: &mut Request,
    this: php_ext::values::PhpObject,
    name: &[u8],
    handler: Handler,
) -> Result<bool, Thrown> {
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<Handle>() else {
            handler.release(req);
            return Ok(false);
        };
        match handle.db.create_collation(name, handler) {
            Ok(registration) => {
                handle.registrations.push(registration);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    })
}
