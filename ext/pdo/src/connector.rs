//! The connector face, php's `php_pdo_driver.h`: what a PDO connector
//! implements and the few things of PDO's it calls, and nothing else
//! (design §4.10). A connector is a [`Driver`] handed over with
//! `ExtensionBuilder::register` from its own `extension()`, which is
//! `php_pdo_register_driver` from the connector's `MINIT`; its factory opens
//! a [`Connection`], php's `pdo_dbh_methods`, and a connection prepares
//! [`Statement`]s, php's `pdo_stmt_methods`.
//!
//! **Errors are values.** Where a php connector writes an SQLSTATE into
//! `dbh->error_code` or `stmt->error_code` and its native code and text
//! into its own `einfo` for `fetch_err` to read back, a connector here
//! answers a [`Failure`], and PDO keeps what it says: the SQLSTATE on the
//! connection or the statement it was raised on, and the native code and
//! text on the connection, where `pdo_sqlite_fetch_error_func` reads
//! them. So the error state has one home, PDO's, and `errorInfo()`,
//! `errorCode()` and the error handling read it there.
//!
//! **An optional slot stays optional.** A slot php lets a connector leave
//! null has a default body here answering [`Cause::Unsupported`], and
//! PDO raises php's own "connector does not support" text for it where php
//! does, at the same point of the call.

use std::any::Any;

use php_ext::coerce::{to_long, value_name};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, Value, ValueRef};

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
    /// [`Cause::Error`] here is what `pdo_throw_exception` words, the
    /// native code in brackets, since PDO has no connection to record it on
    /// yet.
    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure>;
}

/// What a connector's factory opens from: the DSN after the connector's
/// prefix, the credentials, and the options array the constructor was
/// given, `dbh->data_source`, `username`, `password` and
/// `driver_options`.
pub struct Source<'a> {
    pub dsn: &'a [u8],
    pub username: Option<&'a [u8]>,
    pub password: Option<&'a [u8]>,
    pub options: Option<PhpArray>,
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
    Err(throw(
        req,
        "TypeError",
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

/// What a connector's own error function records: the SQLSTATE it maps
/// its library's code to, the library's code and the library's text,
/// php's `einfo`. A code of zero is no native error, which `fetch_err`
/// adds nothing for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    pub state: SqlState,
    pub code: i64,
    pub message: Vec<u8>,
}

/// Why a connector's call did not succeed.
#[derive(Debug)]
pub enum Cause {
    /// The connector's error function ran: the SQLSTATE and the native
    /// error it recorded.
    Error(SqlError),
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
    /// The connector's error function ran.
    #[must_use]
    pub fn error(error: SqlError) -> Failure {
        Failure {
            cause: Cause::Error(error),
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
    pub value: &'a mut Value,
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
}

/// What a statement's start leaves: the column count `php_pdo_stmt_set_column_count`
/// is given, and the row count, where the connector sets `stmt->row_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Executed {
    pub columns: usize,
    pub rows: Option<i64>,
}

/// `struct pdo_column_data`: what `describer` fills for one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: Vec<u8>,
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
    fn in_transaction(&self) -> Option<bool> {
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
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Value>, Failure> {
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

    /// `dbh->alloc_own_columns`: whether the columns are described at
    /// the statement's first start rather than at the first fetch.
    fn describes_at_execute(&self) -> bool {
        false
    }

    /// `get_gc`: the values the connection owns, for the cycle
    /// collector.
    fn gc(&self, visit: &mut dyn FnMut(Value)) {
        let _ = visit;
    }

    /// `closer`, where what the connection owns of the request's is
    /// given back; what it holds of its library goes with its `Drop`.
    fn close(&mut self, req: &mut Request) {
        let _ = req;
    }
}

/// `pdo_stmt_methods`: a prepared statement.
pub trait Statement: Any + Send {
    /// `executer`: `executed` is whether the statement ran before.
    fn execute(&mut self, req: &mut Request, executed: bool) -> Result<Executed, Failure>;

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
    ) -> Result<Value, Failure>;

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
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Value>, Failure> {
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
    ) -> Result<PhpArray, Failure> {
        let _ = (req, column, executed);
        Err(Failure::unsupported())
    }

    /// `next_rowset`: the next rowset's column count.
    fn next_rowset(&mut self, req: &mut Request) -> Result<usize, Failure> {
        let _ = req;
        Err(Failure::unsupported())
    }

    /// `cursor_closer`.
    fn close_cursor(&mut self, req: &mut Request) -> Result<(), Failure> {
        let _ = req;
        Err(Failure::unsupported())
    }
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
