//! The connector face, php's `php_pdo_driver.h`: what a PDO connector
//! implements and the few things of PDO's it calls, and nothing else
//! (design §4.10). A connector is a [`Driver`] handed over with
//! `ExtensionBuilder::register` from its own `extension()`, which is
//! `php_pdo_register_driver` from the connector's `MINIT`; its factory opens
//! a [`Connection`], php's `pdo_dbh_methods`, and a connection prepares
//! [`Statement`]s, php's `pdo_stmt_methods`.
//!
//! **Errors are values, the native ones the connector's.** Where a php
//! connector writes an SQLSTATE into `dbh->error_code` or
//! `stmt->error_code` and its native code and text into its own `einfo`,
//! a connector here keeps its `einfo` as php's does and answers a
//! [`Failure`] carrying the SQLSTATE alone, which PDO records on the
//! connection or the statement it was raised on. The native code and text
//! are read back through [`Connection::fetch_err`] wherever php calls
//! `fetch_err`: `errorInfo()` and the message and `errorInfo` of the error
//! PDO raises. So each connector says when its text goes, as
//! `_pdo_mysql_error` and `_pdo_sqlite_error` differ, and PDO holds no
//! copy of it. A factory's error is the one that carries its text, a
//! [`Cause::Refused`], there being no connection yet to keep it on.
//!
//! **An optional slot stays optional.** A slot php lets a connector leave
//! null has a default body here answering [`Cause::Unsupported`], and
//! PDO raises php's own "connector does not support" text for it where php
//! does, at the same point of the call.

use std::any::Any;

use php_ext::coerce::{to_long, value_name, with_string};
use php_ext::raising::{Thrown, type_error};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

/// `pdo_driver_t`: a connector's name, the class `PDO::connect()` makes
/// for it, and its factory.
pub trait Connector: Send + Sync {
    /// `driver_name`, the DSN prefix that selects the connector and what
    /// `PDO::getAvailableDrivers()` lists.
    fn name(&self) -> &'static str;

    /// The class `php_pdo_register_driver_specific_ce` registers for the
    /// connector, `Pdo\Sqlite`, which `PDO::connect()` makes for a DSN of
    /// this connector; none for a connector without one.
    fn class(&self) -> Option<&'static [u8]> {
        None
    }

    /// The lowered names of the methods the connector lays on `PDO` as
    /// trampolines, `get_driver_methods` for `PDO_DBH_DRIVER_METHOD_KIND_DBH`:
    /// what `PDO`'s `get_method` hands out for a connection of this connector.
    fn methods(&self) -> &'static [&'static [u8]] {
        &[]
    }

    /// `db_handle_factory`: a connection opened from `source`. A
    /// [`Cause::Refused`] here is what `pdo_throw_exception` words, the
    /// native code in brackets, since PDO has no connection to record it on
    /// yet.
    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure>;
}

/// What a connector's factory opens from: the DSN after the connector's
/// prefix, the credentials, the options array the constructor was
/// given, and whether the connection was asked to persist,
/// `dbh->data_source`, `username`, `password`, `driver_options` and
/// `dbh->is_persistent`, which a factory's `pecalloc`s read: what it
/// allocates for a connection that is not persistent is the request's.
pub struct Source<'a> {
    pub dsn: &'a [u8],
    pub username: Option<&'a [u8]>,
    pub password: Option<&'a [u8]>,
    pub options: Option<PhpArray>,
    pub persistent: bool,
}

/// `pdo_attr_lval`: the option under the attribute number `attr`, as
/// `zval_get_long` reads it, or `default` where the array has none.
///
/// ```php
/// $db = new PDO('sqlite::memory:', null, null, [Pdo\Sqlite::ATTR_OPEN_FLAGS => '1']);
/// try { $db->exec('CREATE TABLE t (a)'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
/// try { $db->prepare('SELECT 1', [PDO::ATTR_CURSOR => PDO::CURSOR_SCROLL]); } catch (PDOException $e) { echo 'again: ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// SQLSTATE[HY000]: General error: 8 attempt to write a readonly database
/// again: SQLSTATE[HY000]: General error: 8 attempt to write a readonly database
/// ```
pub fn option_long(
    req: &mut Request,
    options: Option<PhpArray>,
    attr: i64,
    default: i64,
) -> Result<i64, Thrown> {
    match options.and_then(|options| options.get(&Key::Int(attr))) {
        Some(value) => to_long(req, &value.deref()),
        None => Ok(default),
    }
}

/// `pdo_get_long_param`: an attribute's value as an integer, `true`,
/// `false` and a numeric string taken, php's `TypeError` for the rest.
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// var_dump($db->setAttribute(PDO::ATTR_TIMEOUT, '5'), $db->setAttribute(PDO::ATTR_TIMEOUT, true));
/// try { $db->setAttribute(PDO::ATTR_TIMEOUT, 'soon'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $db->setAttribute(PDO::ATTR_TIMEOUT, 1.5); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// Attribute value must be of type int for selected attribute, string given
/// Attribute value must be of type int for selected attribute, float given
/// ```
pub fn long_attribute(req: &mut Request, value: &Value) -> Result<i64, Thrown> {
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Int(n) => return Ok(n),
        ValueRef::Bool(b) => return Ok(i64::from(b)),
        ValueRef::Str(s) => {
            if let Some(n) = php_ext::coerce::numeric_string(s.as_bytes()).and_then(|v| v.as_int())
            {
                return Ok(n);
            }
        }
        _ => {}
    }
    let given = value_name(req, &value);
    Err(type_error(
        req,
        &format!("Attribute value must be of type int for selected attribute, {given} given"),
    ))
}

/// `pdo_error_type`: the five characters of an SQLSTATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlState(pub [u8; 5]);

impl SqlState {
    /// `PDO_ERR_NONE`.
    pub const NONE: SqlState = SqlState(*b"00000");
    /// `HY000`, the general error.
    pub const GENERAL: SqlState = SqlState(*b"HY000");

    /// The five characters as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|b| *b == 0).unwrap_or(5);
        std::str::from_utf8(&self.0[..end]).unwrap_or("HY000")
    }
}

/// What a connector's error function found in its factory, where php's
/// `_pdo_mysql_error` and `_pdo_sqlite_error` call `pdo_throw_exception`
/// for want of a connection's methods: the SQLSTATE it maps its library's
/// code to, the library's code and the library's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    pub state: SqlState,
    pub code: i64,
    pub message: Vec<u8>,
}

/// What [`Connection::fetch_err`] adds to an `errorInfo`: the code and the
/// text of the connector's last error, php's `einfo->errcode` and
/// `einfo->errmsg`, the text a string of the request's as
/// `add_next_index_string` makes it.
#[derive(Debug)]
pub struct NativeError {
    pub code: i64,
    pub message: Owner<PhpString>,
}

/// Why a connector's call did not succeed.
#[derive(Debug)]
pub enum Cause {
    /// The connector's error function ran on a connection or a statement:
    /// the SQLSTATE it set, its native code and text kept in its own
    /// `einfo` for [`Connection::fetch_err`].
    Error(SqlState),
    /// The connector's error function ran in its factory: what
    /// `pdo_throw_exception` words. Only [`Connector::open`] answers it.
    Refused(SqlError),
    /// `pdo_raise_impl_error` from inside the connector: an SQLSTATE and
    /// PDO's supplementary text, no native error.
    Impl { state: SqlState, message: String },
    /// A failure that sets no SQLSTATE, a connector slot answering `0` or
    /// `false` and leaving the error code as it was.
    Silent,
    /// php's null slot: the connector does not serve this call.
    Unsupported,
}

/// A failed call: its [`Cause`], and the throwable a PHP callback or a
/// type check left pending while it failed, which PDO lets through
/// after it has recorded the cause, as php's `EG(exception)` is seen by
/// `pdo_handle_error`.
#[derive(Debug)]
pub struct Failure {
    pub cause: Cause,
    pub thrown: Option<Thrown>,
}

impl Failure {
    /// The connector's error function ran, setting `state`.
    #[must_use]
    pub fn error(state: SqlState) -> Failure {
        Failure {
            cause: Cause::Error(state),
            thrown: None,
        }
    }

    /// The connector's error function ran in its factory.
    #[must_use]
    pub fn refused(error: SqlError) -> Failure {
        Failure {
            cause: Cause::Refused(error),
            thrown: None,
        }
    }

    /// A failure that sets nothing.
    #[must_use]
    pub fn silent() -> Failure {
        Failure {
            cause: Cause::Silent,
            thrown: None,
        }
    }

    /// php's null slot.
    #[must_use]
    pub fn unsupported() -> Failure {
        Failure {
            cause: Cause::Unsupported,
            thrown: None,
        }
    }

    /// `pdo_raise_impl_error` from the connector.
    #[must_use]
    pub fn raised(state: SqlState, message: &str) -> Failure {
        Failure {
            cause: Cause::Impl {
                state,
                message: message.to_owned(),
            },
            thrown: None,
        }
    }
}

impl From<Thrown> for Failure {
    fn from(thrown: Thrown) -> Failure {
        Failure {
            cause: Cause::Silent,
            thrown: Some(thrown),
        }
    }
}

/// `enum pdo_param_type` without its bits, `PDO_PARAM_TYPE(x)`: what a
/// bound value is sent and fetched as. A number that is none of php's
/// six is carried as given, since php stores it and every connector treats
/// it as its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindKind {
    Null,
    Int,
    Str,
    Lob,
    Stmt,
    Bool,
    Other(i32),
}

/// A parameter type as `bindParam()` and `bindValue()` were given it:
/// its kind and php's whole `int`, the bits
/// (`PDO_PARAM_INPUT_OUTPUT`, `PDO_PARAM_STR_NATL`, ...) beside the kind,
/// which `debugDumpParams()` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindType {
    kind: BindKind,
    raw: i32,
}

impl BindType {
    /// `PDO_PARAM_STR`, the default.
    pub const STR: BindType = BindType {
        kind: BindKind::Str,
        raw: 2,
    };

    /// The type php's `(int) param_type` makes of a `zend_long`.
    #[must_use]
    pub fn of(raw: i64) -> BindType {
        // php stores the parameter type as a C `int`.
        #[allow(clippy::cast_possible_truncation)]
        let raw = raw as i32;
        let kind = match raw & 0xFFFF {
            0 => BindKind::Null,
            1 => BindKind::Int,
            2 => BindKind::Str,
            3 => BindKind::Lob,
            4 => BindKind::Stmt,
            5 => BindKind::Bool,
            other => BindKind::Other(other),
        };
        BindType { kind, raw }
    }

    /// `PDO_PARAM_TYPE(x)`.
    #[must_use]
    pub fn kind(self) -> BindKind {
        self.kind
    }

    /// The whole `int`, bits and all.
    #[must_use]
    pub fn raw(self) -> i32 {
        self.raw
    }
}

/// `enum pdo_param_event`: the moments PDO calls a connector's parameter
/// callback, in php's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindEvent {
    Alloc,
    Free,
    ExecPre,
    ExecPost,
    FetchPre,
    FetchPost,
    Normalize,
}

/// One bound parameter or column as the binding callback sees it,
/// `struct pdo_bound_param_data`: its position, which the callback may
/// place a name at, its name, its type, whether it is a parameter or a
/// column, and the value itself, a reference for `bindParam()`, which
/// the callback may convert in place as php's `convert_to_long` does.
pub struct Binding<'a> {
    /// `paramno`, zero-based; `-1` for a name the statement has not
    /// placed yet.
    pub position: &'a mut i64,
    pub name: Option<&'a [u8]>,
    pub ty: BindType,
    pub is_parameter: bool,
    pub value: &'a mut Owner<Value>,
}

/// `enum pdo_placeholder_support`: what a statement's connector takes
/// itself, the rest being PDO's to rewrite or emulate
/// (`pdo_sql_parser.re`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placeholders {
    /// `PDO_PLACEHOLDER_NONE`: PDO quotes every value into the query.
    Emulated,
    /// `PDO_PLACEHOLDER_NAMED`, `PDO_PLACEHOLDER_POSITIONAL` or both.
    Native { named: bool, positional: bool },
}

/// `enum pdo_fetch_orientation`, a scrollable cursor's move; a number
/// that is none of php's six is carried as given, as php hands it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Forward,
    Prior,
    First,
    Last,
    Absolute,
    Relative,
    Other(i64),
}

impl Orientation {
    /// php's `(enum pdo_fetch_orientation) zend_long`.
    #[must_use]
    pub fn of(raw: i64) -> Orientation {
        match raw {
            0 => Orientation::Forward,
            1 => Orientation::Prior,
            2 => Orientation::First,
            3 => Orientation::Last,
            4 => Orientation::Absolute,
            5 => Orientation::Relative,
            other => Orientation::Other(other),
        }
    }
}

/// What a prepare hands back: the connector's statement and the
/// placeholders it takes itself.
pub struct Prepared {
    pub statement: Box<dyn Statement>,
    pub placeholders: Placeholders,
    /// `stmt->bound_param_map` where the connector rewrote the query with
    /// [`rewrite_named`].
    pub parameter_map: Option<ParameterMap>,
}

/// What a statement's start leaves: the column count `php_pdo_stmt_set_column_count`
/// is given, and the row count, where the connector sets `stmt->row_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Executed {
    pub columns: usize,
    pub rows: Option<i64>,
}

/// `struct pdo_column_data`: what `describer` fills for one column. Its
/// name is one owner of the string the connector holds or made for it,
/// `pdo_mysql`'s interned `sname` or `pdo_sqlite`'s `zend_string_init`, which
/// the statement keeps until `pdo_stmt_reset_columns` and every fetched
/// row is keyed by.
#[derive(Debug)]
pub struct Column {
    pub name: Owner<PhpString>,
    pub max_len: u64,
    pub precision: u64,
}

/// `pdo_dbh_methods`: an open connection. PDO holds it on the connection and
/// lends it to each call, so a slot runs with the request beside it and
/// may serve PHP code. `Any` lets a connector's own methods
/// ([`with_connection`]) reach their connection as its own type.
pub trait Connection: Any + Send {
    /// `preparer`: `sql` prepared, with the options `prepare()` was
    /// given.
    fn prepare(
        &mut self,
        req: &mut Request,
        sql: &[u8],
        options: Option<PhpArray>,
    ) -> Result<Prepared, Failure>;

    /// `doer`: `sql` served for its effect, the rows it changed.
    fn exec(&mut self, req: &mut Request, sql: &[u8]) -> Result<i64, Failure>;

    /// `quoter`: `text` quoted as a string of the connector's SQL.
    /// `raises` is the connection's error handling, which a quoter that raises
    /// by itself reads, as `sqlite_handle_quoter` does.
    fn quote(
        &mut self,
        req: &mut Request,
        text: &[u8],
        ty: BindType,
        raises: OnError,
    ) -> Result<Vec<u8>, Failure> {
        let _ = (req, text, ty, raises);
        Err(Failure::unsupported())
    }

    /// `begin`.
    fn begin(&mut self, req: &mut Request) -> Result<(), Failure> {
        let _ = req;
        Err(Failure::unsupported())
    }

    /// `commit`.
    fn commit(&mut self, req: &mut Request) -> Result<(), Failure> {
        let _ = req;
        Err(Failure::unsupported())
    }

    /// `rollback`.
    fn rollback(&mut self, req: &mut Request) -> Result<(), Failure> {
        let _ = req;
        Err(Failure::unsupported())
    }

    /// `in_transaction`: `None` is php's null slot, which leaves PDO to
    /// track the transaction itself.
    fn in_transaction(&mut self, req: &mut Request) -> Option<bool> {
        let _ = req;
        None
    }

    /// `set_attribute`, for an attribute PDO does not connection itself.
    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &Value,
    ) -> Result<(), Failure> {
        let _ = (req, attr, value);
        Err(Failure::unsupported())
    }

    /// `get_attribute`: the value, `None` for an attribute the connector
    /// does not know, php's `0`.
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Owner<Value>>, Failure> {
        let _ = (req, attr);
        Err(Failure::unsupported())
    }

    /// `last_id`.
    fn last_insert_id(
        &mut self,
        req: &mut Request,
        name: Option<&[u8]>,
    ) -> Result<Vec<u8>, Failure> {
        let _ = (req, name);
        Err(Failure::unsupported())
    }

    /// `scanner`: the connector's own placeholder scanner, which
    /// `pdo_parse_params` uses in place of `default_scanner`.
    fn scanner(&self) -> Option<Scanner> {
        None
    }

    /// `dbh->alloc_own_columns`: whether the columns are described at
    /// the statement's first start rather than at the first fetch.
    fn describes_at_execute(&self) -> bool {
        false
    }

    /// `fetch_err`: the native code and text of the error the connector
    /// last recorded, the connection's own or, for `stmt`, the statement's
    /// where the connector keeps one per statement
    /// (`pdo_mysql_fetch_error_func` reads `S->einfo`,
    /// `pdo_sqlite_fetch_error_func` `H->einfo` for both); `None` where its
    /// code is zero, which php's slot adds nothing for, and for php's null
    /// slot. The `Err` is the memory limit's fatal, met making the text.
    fn fetch_err(
        &self,
        req: &mut Request,
        stmt: Option<&dyn Statement>,
    ) -> Result<Option<NativeError>, Thrown> {
        let _ = (req, stmt);
        Ok(None)
    }

    /// `get_gc`: the values the connection owns, for the cycle
    /// collector.
    fn gc(&self, _visit: &mut dyn FnMut(Value)) {}

    /// `closer`, where what the connection owns of the request's is
    /// given back; what it holds of its library goes with its `Drop`.
    fn close(&mut self, req: &mut Request) {
        let _ = req;
    }

    /// `check_liveness`: whether a persistent connection found under its
    /// key is alive and ready for another use, asked before PDO lends it
    /// again; php's null slot answers that it is.
    fn check_liveness(&mut self, req: &mut Request) -> bool {
        let _ = req;
        true
    }

    /// `persistent_shutdown`: what a persistent connection holds of the
    /// request given back, when a `PDO` sharing it is freed, so that
    /// nothing of one request's reaches the next.
    fn persistent_shutdown(&mut self, req: &mut Request) {
        let _ = req;
    }

    /// A persistent connection given back to the engine's pool as the
    /// request ends, after its objects are freed: what it holds in the
    /// request's tables moved out of them, for the next request to take
    /// up. php's `pdo_dbh_t` stands in `EG(persistent_list)` as it is,
    /// which this default answers.
    fn give_back(&mut self, req: &mut Request) {
        let _ = req;
    }
}

/// `pdo_stmt_methods`: a prepared statement.
pub trait Statement: Any + Send {
    /// `executer`: `executed` is whether the statement ran before.
    /// `query` is `stmt->active_query_string`: the query as PDO rewrote it
    /// for a connector that emulates prepares, the query as written
    /// otherwise.
    fn execute(
        &mut self,
        req: &mut Request,
        executed: bool,
        query: &[u8],
    ) -> Result<Executed, Failure>;

    /// `fetcher`: whether a row was fetched.
    fn fetch(
        &mut self,
        req: &mut Request,
        orientation: Orientation,
        offset: i64,
    ) -> Result<bool, Failure>;

    /// `describer`: column `column` of the rows.
    fn describe(&mut self, req: &mut Request, column: usize) -> Result<Column, Failure>;

    /// `get_col`: the value of column `column` in the current row, one
    /// owner the caller's; `ty` is what `bindColumn()` asked for.
    fn value(
        &mut self,
        req: &mut Request,
        column: usize,
        ty: Option<BindKind>,
    ) -> Result<Owner<Value>, Failure>;

    /// `param_hook`.
    fn bind(
        &mut self,
        req: &mut Request,
        param: Binding<'_>,
        event: BindEvent,
    ) -> Result<(), Failure> {
        let _ = (req, param, event);
        Ok(())
    }

    /// `set_attribute`.
    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &Value,
    ) -> Result<(), Failure> {
        let _ = (req, attr, value);
        Err(Failure::unsupported())
    }

    /// `get_attribute`: `None` for an attribute the connector does not
    /// know.
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Owner<Value>>, Failure> {
        let _ = (req, attr);
        Err(Failure::unsupported())
    }

    /// `get_column_meta`, the connector's half: `executed` is whether the
    /// statement has started.
    fn column_meta(
        &mut self,
        req: &mut Request,
        column: usize,
        executed: bool,
    ) -> Result<Owner<PhpArray>, Failure> {
        let _ = (req, column, executed);
        Err(Failure::unsupported())
    }

    /// `stmt->methods->next_rowset`, a slot php tests for `NULL` before
    /// PDO acts on the statement (`nextRowset()` refuses with `IM001`
    /// before it resets the columns, and `closeCursor()`'s emulation stops
    /// after the first rowset). A connector with the slot answers itself.
    fn rowsets(&mut self) -> Option<&mut dyn Rowsets> {
        None
    }

    /// `stmt->methods->cursor_closer`, a slot php tests for `NULL` before
    /// PDO acts (`closeCursor()` clears the error only where there is one,
    /// and emulates it by fetching every row of every rowset where there
    /// is none). A connector with the slot answers itself.
    fn cursor_closer(&mut self) -> Option<&mut dyn CursorCloser> {
        None
    }

    /// `dtor`: what the statement holds of the request's or of its
    /// connection's given back, as the statement is freed.
    fn close(&mut self, req: &mut Request) {
        let _ = req;
    }
}

/// `next_rowset`, the slot [`Statement::rowsets`] answers.
pub trait Rowsets {
    /// `next_rowset`: the next rowset's column count, and its row count
    /// where the connector sets `stmt->row_count`; the `Err` where there is
    /// none or the connector failed.
    fn next_rowset(&mut self, req: &mut Request) -> Result<Executed, Failure>;
}

/// `cursor_closer`, the slot [`Statement::cursor_closer`] answers.
pub trait CursorCloser {
    /// `cursor_closer`: the rows of the start before given back, so the
    /// statement can start again.
    fn close_cursor(&mut self, req: &mut Request) -> Result<(), Failure>;
}

/// `enum pdo_error_mode`: what PDO does with an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnError {
    Silent,
    Warn,
    #[default]
    Throw,
}

/// `Z_PDO_DBH_P(ZEND_THIS)` with `PDO_CONSTRUCT_CHECK`, for a connector's
/// own methods: `f` served with the connection of the connection `this`,
/// php's `Error` naming the class where the connection was never
/// constructed.
///
/// ```php
/// class Later extends Pdo\Sqlite { function __construct() {} }
/// try { (new Later)->createFunction('f', 'strlen'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump((new Pdo\Sqlite('sqlite::memory:'))->createFunction('f', 'strlen'));
/// ```
/// ```expect
/// Later object is uninitialized
/// bool(true)
/// ```
pub fn with_connection<R>(
    req: &mut Request,
    this: PhpObject,
    f: impl FnOnce(&mut Request, &mut dyn Connection) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    crate::dbh::with_connection(req, this, f)
}

/// `PDO_PARSER_TEXT`, `PDO_PARSER_BIND` and `PDO_PARSER_BIND_POS`: what a
/// scanner makes of one stretch of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Text,
    Bind,
    Position,
}

/// `int (*scanner)(pdo_scanner_t *s)`: the token at `at` of the query and
/// where it ends, `None` at its end or at a NUL, `PDO_PARSER_EOI`.
pub type Scanner = fn(&[u8], usize) -> Option<(Token, usize)>;

/// `pdo_parse_params` for a connector that takes positional placeholders
/// natively: each `:name` rewritten to `?`, and the number each stood
/// for mapped to its name; `None` where the query has no named one; the
/// mix of the two kinds refused as php refuses it. The map is the
/// request's, as php's is ([`ParameterMap`]); a `Failure` carrying a throw
/// is the memory limit's fatal.
pub fn rewrite_named(
    req: &mut Request,
    sql: &[u8],
    scanner: Option<Scanner>,
) -> Result<Option<Rewritten>, Failure> {
    crate::parser::rewrite_named(req, sql, scanner)
}

/// A query [`rewrite_named`] rewrote: the query with `?` in each name's
/// place, and `stmt->bound_param_map`.
pub struct Rewritten {
    pub sql: Vec<u8>,
    pub parameter_map: ParameterMap,
}

/// `stmt->bound_param_map` of a query whose `:name`s were rewritten to
/// `?`: each placeholder's number keys the name it stood for, a
/// `zend_string_init` of its own, in a table `ALLOC_HASHTABLE` and
/// `zend_hash_init(map, 13, ...)` made, all of it on the request's heap
/// until `php_pdo_free_statement` destroys it. A parameter bound by
/// position over the rewritten query takes one more owner of its name
/// (`rewrite_name_to_position`'s `zend_string_copy`), so its name costs
/// nothing of its own. The last statement names its column with `AS v`,
/// so the name mysqlnd interns is one the compiled program already holds,
/// opcache or not:
///
/// ```php
/// $p = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_EMULATE_PREPARES => false]);
/// $p->prepare('SELECT ?, ?');
/// $m = memory_get_usage();
/// $s = $p->prepare('SELECT ?, ?');
/// echo memory_get_usage() - $m, "\n";
/// unset($s);
/// $m = memory_get_usage();
/// $s = $p->prepare('SELECT :a, :b');
/// echo memory_get_usage() - $m, "\n";
/// $n = memory_get_usage();
/// $s->bindValue(1, 5);
/// echo memory_get_usage() - $n, "\n";
/// unset($s);
/// echo memory_get_usage() - $m, "\n";
/// $q = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// $s = $q->prepare('SELECT ? AS v');
/// $m = memory_get_usage();
/// $s->execute([12345]);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql
/// 21760
/// 22200
/// 776
/// 0
/// 16960
/// ```
pub struct ParameterMap {
    table: Owner<PhpArray>,
}

impl ParameterMap {
    /// `ALLOC_HASHTABLE` and `zend_hash_init` at 13, the size php gives
    /// it; the `Err` is the memory limit's fatal.
    pub(crate) fn new(req: &mut Request) -> Result<ParameterMap, Thrown> {
        Ok(ParameterMap {
            table: PhpArray::init_size(req, 13)?,
        })
    }

    /// `zend_hash_index_update_ptr(map, bindno, zend_string_init(name))`;
    /// the `Err` is the memory limit's fatal.
    pub(crate) fn insert(
        &mut self,
        req: &mut Request,
        bindno: i64,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let name = PhpString::filled(req, name.len(), |to| to.copy_from_slice(name))?;
        let key = Owner::index(bindno);
        self.table.set(req, key, name.into_value())
    }

    /// `zend_hash_index_find_ptr(map, paramno)`: the name placeholder
    /// `position` stood for, a view the caller takes an owner of.
    pub(crate) fn name(&self, position: i64) -> Option<PhpString> {
        match self.table.get(&Key::Int(position))?.as_ref() {
            ValueRef::Str(name) => Some(name),
            _ => None,
        }
    }

    /// `ZEND_HASH_FOREACH_PTR(map, name)` counting to the first name
    /// equal to `name`: the position its placeholder took.
    pub(crate) fn position(&self, name: &[u8]) -> Option<usize> {
        let mut at = 0;
        let mut position = 0;
        while let Some((bucket, _, value)) = self.table.live_from(at) {
            if matches!(value.as_ref(), ValueRef::Str(s) if s.as_bytes() == name) {
                return Some(position);
            }
            position += 1;
            at = bucket + 1;
        }
        None
    }

    /// `zend_hash_destroy` and `FREE_HASHTABLE`: each name released, then
    /// the table.
    pub fn give_up(self, req: &mut Request) {
        self.table.give_up(req);
    }
}

/// `php_pdo_parse_data_source`: each `name=value;` of the DSN found
/// among `names`, `;;` read as one `;`, the last of a name kept; the
/// default where the DSN has none.
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// var_dump($db->getAttribute(PDO::ATTR_DRIVER_NAME));
/// ```
/// ```expect
/// string(6) "sqlite"
/// ```
#[must_use]
pub fn parse_data_source(dsn: &[u8], names: &[(&str, Option<&[u8]>)]) -> Vec<Option<Vec<u8>>> {
    let mut out: Vec<Option<Vec<u8>>> = names.iter().map(|(_, d)| d.map(<[u8]>::to_vec)).collect();
    let len = dsn.len();
    let mut i = 0;
    let mut optstart = 0;
    while i < len {
        if dsn[i] == 0 {
            break;
        }
        if dsn[i] != b'=' {
            i += 1;
            continue;
        }
        i += 1;
        let valstart = i;
        let mut semi = None;
        let mut doubled = false;
        while i < len {
            if dsn[i] == 0 {
                semi = Some(i);
                i += 1;
                break;
            }
            if dsn[i] == b';' {
                if i + 1 >= len || dsn[i + 1] != b';' {
                    semi = Some(i);
                    i += 1;
                    break;
                }
                doubled = true;
                i += 2;
                continue;
            }
            i += 1;
        }
        let semi = semi.unwrap_or(i);
        let name = &dsn[optstart..valstart - 1];
        if let Some(at) = names.iter().position(|(n, _)| n.as_bytes() == name) {
            let raw = &dsn[valstart..semi];
            let value = if doubled {
                let mut v = Vec::with_capacity(raw.len());
                let mut k = 0;
                while k < raw.len() && raw[k] != 0 {
                    v.push(raw[k]);
                    k += if raw[k] == b';' { 2 } else { 1 };
                }
                v
            } else {
                raw.to_vec()
            };
            if let Some(slot) = out.get_mut(at) {
                *slot = Some(value);
            }
        }
        while i < len && dsn[i].is_ascii_whitespace() {
            i += 1;
        }
        optstart = i;
    }
    out
}

/// `pdo_attr_strval`: the option under `attr` as `zval_try_get_string`
/// reads it, `None` where the array has none.
pub fn option_string(
    req: &mut Request,
    options: Option<PhpArray>,
    attr: i64,
) -> Result<Option<Vec<u8>>, Thrown> {
    match options.and_then(|options| options.get(&Key::Int(attr))) {
        Some(value) => Ok(Some(with_string(req, &value, <[u8]>::to_vec)?)),
        None => Ok(None),
    }
}

/// `pdo_get_bool_param`: `true`, `false` and an integer taken, php's
/// `TypeError` for the rest.
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// try { $db->setAttribute(PDO::ATTR_STRINGIFY_FETCHES, 'yes'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Attribute value must be of type bool for selected attribute, string given
/// ```
pub fn bool_attribute(req: &mut Request, value: &Value) -> Result<bool, Thrown> {
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Bool(b) => Ok(b),
        ValueRef::Int(n) => Ok(n != 0),
        _ => {
            let given = value_name(req, &value);
            Err(type_error(
                req,
                &format!(
                    "Attribute value must be of type bool for selected attribute, {given} given"
                ),
            ))
        }
    }
}
