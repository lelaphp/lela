//! `sqlite_driver.c` and `sqlite_statement.c` behind PDO's connector face:
//! the factory, the connection's methods and the statement's, the
//! SQLSTATE mapping of `pdo_sqlite_error` and the column types of
//! `pdo_sqlite_stmt_get_col`.

use php_ext::abi::{Callable, EngineState};
use php_ext::coerce::{to_long, to_string, value_name};
use php_ext::raising::{Level, Thrown, docref, throw_exception_ex, type_error, value_error};
use php_ext::request::{Foreign, Request, Stream};
use php_ext::values::{Emalloc, Owner, PhpArray, PhpString, Value, ValueRef};
use php_ext_pdo::connector::{
    BindEvent, BindKind, BindType, Binding, Column, Connection, Connector, CursorCloser, Executed,
    Failure, IdleConnection, NativeError, OnError, Orientation, Placeholders, Prepared, Source,
    SqlError, SqlState, Statement, long_attribute, option_long,
};

use crate::Sqlite;
use crate::ffi::{self, Conn, Db, Function, Handler, Registration};
use crate::throwables::Throwables;

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
#[derive(EngineState)]
pub(crate) struct Driver;

pub(crate) static SQLITE: Driver = Driver;

/// The names of the three methods `get_driver_methods` lays on `PDO`.
const METHODS: [&[u8]; 3] = [
    b"sqlitecreatefunction",
    b"sqlitecreateaggregate",
    b"sqlitecreatecollation",
];

/// `pdo_sqlite_error_info`: the code `_pdo_sqlite_error` last read and the
/// message it `pestrdup`'d, on the request's heap for a connection that is
/// not persistent and `malloc`'s for one that is. A code of `SQLITE_OK`
/// keeps the message there was, which only the next error replaces and
/// the closer frees, so `errorInfo()` shows nothing after it while the
/// text stays on the heap:
///
/// ```php
/// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);
/// $m = memory_get_usage();
/// $db->exec('NOPE');
/// echo memory_get_usage() - $m, " ", json_encode($db->errorInfo()), "\n";
/// $db->exec('SELECT 1');
/// var_dump($db->prepare('SELECT 1', [PDO::ATTR_CURSOR => PDO::CURSOR_SCROLL]));
/// echo memory_get_usage() - $m, " ", json_encode($db->errorInfo()), "\n";
/// $db->exec('SELECT * FROM ' . str_repeat('x', 100));
/// echo memory_get_usage() - $m, " ", json_encode($db->errorInfo()), "\n";
/// $db = null;
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 32 ["HY000",1,"near \"NOPE\": syntax error"]
/// bool(false)
/// 64 ["00000",null,null]
/// 160 ["HY000",1,"no such table: xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"]
/// -344
/// ```
#[derive(Default)]
pub(crate) struct Einfo {
    /// `dbh->is_persistent`, where `pestrdup` puts the message.
    persistent: bool,
    /// `einfo->errcode`.
    code: i32,
    /// `einfo->errmsg`.
    message: Option<Errmsg>,
}

/// Where `pestrdup` put a message: the request's heap, with the NUL php
/// copies, or the process's.
enum Errmsg {
    Request(Emalloc),
    Process(Box<[u8]>),
}

/// An [`Einfo`] while PDO's pool keeps its connection: the message the
/// process's, as a persistent connection's is.
#[derive(Default, EngineState)]
pub(crate) struct IdleEinfo {
    persistent: bool,
    code: i32,
    message: Option<Box<[u8]>>,
}

impl Einfo {
    /// What the pool keeps of the `einfo`; a message on the request's
    /// heap, which a persistent connection never makes, copied out and
    /// freed with the request.
    pub(crate) fn idle(self, req: &mut Request) -> IdleEinfo {
        let message = self.message.map(|message| match message {
            Errmsg::Process(bytes) => bytes,
            Errmsg::Request(held) => {
                let bytes = held.bytes().split_last().map_or(&[][..], |(_, m)| m).into();
                held.free(req);
                bytes
            }
        });
        IdleEinfo {
            persistent: self.persistent,
            code: self.code,
            message,
        }
    }
}

impl From<IdleEinfo> for Einfo {
    fn from(idle: IdleEinfo) -> Einfo {
        Einfo {
            persistent: idle.persistent,
            code: idle.code,
            message: idle.message.map(Errmsg::Process),
        }
    }
}

impl Einfo {
    /// `pefree(einfo->errmsg, dbh->is_persistent)`.
    fn free(&mut self, req: &mut Request) {
        if let Some(Errmsg::Request(held)) = self.message.take() {
            held.free(req);
        }
    }

    /// `pestrdup(message, dbh->is_persistent)` over the message kept; the
    /// `Err` is the memory limit's fatal.
    fn keep(&mut self, req: &mut Request, message: &[u8]) -> Result<(), Thrown> {
        self.free(req);
        if self.persistent {
            self.message = Some(Errmsg::Process(message.into()));
            return Ok(());
        }
        let mut held = Emalloc::new(req, message.len() + 1)?;
        if let Some(at) = held.bytes_mut().get_mut(..message.len()) {
            at.copy_from_slice(message);
        }
        self.message = Some(Errmsg::Request(held));
        Ok(())
    }

    /// `pdo_sqlite_fetch_error_func`: the code and the message where the
    /// code is not `SQLITE_OK`.
    fn fetch(&self, req: &mut Request) -> Result<Option<NativeError>, Thrown> {
        if self.code == ffi::SQLITE_OK {
            return Ok(None);
        }
        let message = match &self.message {
            Some(Errmsg::Request(held)) => held.bytes().split_last().map_or(&[][..], |(_, m)| m),
            Some(Errmsg::Process(bytes)) => bytes,
            None => &[],
        };
        Ok(Some(NativeError {
            code: i64::from(self.code),
            message: PhpString::new(req, message)?,
        }))
    }
}

/// `_pdo_sqlite_error`: the connection's last code recorded on its
/// `H->einfo`, with its message where it is an error; the SQLSTATE the code
/// maps to, `00000` for `SQLITE_OK`, with what a callback threw, or the
/// memory limit's fatal where the message could not be kept.
fn failed(req: &mut Request, db: Db, thrown: Option<Thrown>) -> Failure {
    let code = db.errcode(req);
    let mut einfo = db.replace_einfo(req, Einfo::default());
    einfo.code = code;
    let (state, kept) = if code == ffi::SQLITE_OK {
        (SqlState::NONE, Ok(()))
    } else {
        let message = db.errmsg(req);
        (state_for(code), einfo.keep(req, &message))
    };
    db.replace_einfo(req, einfo).free(req);
    Failure {
        thrown: kept.err().or(thrown),
        ..Failure::error(state)
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
        // `H = pecalloc(1, sizeof(pdo_sqlite_db_handle), dbh->is_persistent)`,
        // first, and given back by the closer whatever the open does.
        let allocation = if source.persistent {
            None
        } else {
            Some(Emalloc::new(req, DB_HANDLE)?)
        };
        match open_db(req, source) {
            Ok(owner) => {
                let einfo = Einfo {
                    persistent: source.persistent,
                    ..Einfo::default()
                };
                Db::of(&owner).replace_einfo(req, einfo).free(req);
                Ok(Box::new(Handle {
                    place: Place::Request(owner),
                    funcs: Vec::new(),
                    collations: Vec::new(),
                    authorizer: None,
                    transaction_mode: 0,
                    allocation,
                }))
            }
            Err(failure) => {
                if let Some(allocation) = allocation {
                    allocation.free(req);
                }
                Err(failure)
            }
        }
    }
}

/// The rest of `pdo_sqlite_handle_factory`: the file made safe as
/// `make_filename_safe` makes it, opened with the bits the options give,
/// the busy timeout set.
fn open_db(req: &mut Request, source: &Source<'_>) -> Result<Foreign<Conn>, Failure> {
    {
        let Some(filename) = safe_filename(req, Some(source.dsn)) else {
            let shown = String::from_utf8_lossy(source.dsn).into_owned();
            let ce = Throwables::of(req)?;
            return Err(Failure::from(throw_exception_ex(
                req,
                ce.pdo_exception,
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
        let owner = match Db::open(req, &filename, flags, &crate::callbacks::PDO_SQLITE) {
            Ok(owner) => owner,
            Err((code, message)) => {
                return Err(Failure::refused(SqlError {
                    state: state_for(code),
                    code: i64::from(code),
                    message,
                }));
            }
        };
        let timeout = match source.options {
            Some(options) => match option_long(req, Some(options), ATTR_TIMEOUT, 60) {
                Ok(timeout) => timeout,
                Err(failure) => {
                    drop(req.memory_mut().disown(owner));
                    return Err(Failure::from(failure));
                }
            },
            None => 60,
        };
        Db::of(&owner).busy_timeout(req, timeout.saturating_mul(1000));
        Ok(owner)
    }
}

/// Whether `open_basedir` is set, `PG(open_basedir) && *PG(open_basedir)`.
pub(super) fn open_basedir(req: &Request) -> bool {
    req.ini()
        .get(b"open_basedir")
        .is_some_and(|v| !v.is_empty())
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
    let name = php_ext::coerce::c_str(name);
    if name.len() >= 5 && name[..5].eq_ignore_ascii_case(b"file:") {
        return (!open_basedir(req)).then(|| name.to_vec());
    }
    if name.is_empty() || name == b":memory:" {
        return Some(name.to_vec());
    }
    let cwd = req
        .environment()
        .cwd()
        .into_os_string()
        .into_encoded_bytes();
    let full =
        req.environment_mut()
            .virtual_file_ex(&cwd, name, php_ext::request::Resolve::FilePath)?;
    let full = req.environment_mut().check_open_basedir(&full, true).ok()?;
    Some(full.as_os_str().as_encoded_bytes().to_vec())
}

/// `pdo_sqlite_db_handle`: the connection, the functions and collations
/// registered on it, the authorizer, and the transaction style.
pub(crate) struct Handle {
    place: Place,
    /// `H->funcs`, oldest first; php prepends each, so it gives them up
    /// newest first.
    pub(crate) funcs: Vec<Registration>,
    /// `H->collations`, kept and given up as [`Handle::funcs`] are.
    pub(crate) collations: Vec<Registration>,
    /// `authorizer_fcc`, the callable the connection's authorizer calls
    /// through a view of it ([`Db::set_authorizer`]).
    pub(crate) authorizer: Option<Handler>,
    transaction_mode: i64,
    /// The `pdo_sqlite_db_handle` the factory `pecalloc`s for the
    /// connection, on the request's heap where the connection is not
    /// persistent, given back by the closer, `sqlite_handle_closer`'s
    /// `pefree(H)`.
    allocation: Option<Emalloc>,
}

/// `sizeof(pdo_sqlite_db_handle)` on the pinned 64-bit build: `db`,
/// `einfo`, `funcs`, `collations`, `authorizer_fcc` and
/// `transaction_mode` (`Z/ext/pdo_sqlite/php_pdo_sqlite_int.h:48-55`).
const DB_HANDLE: usize = 96;

/// Where a connection's [`Conn`] stands: kept by the request the
/// connection serves (ADR-0013), or gone.
enum Place {
    Request(Foreign<Conn>),
    /// Given up: closed, or kept past the request by a call still holding
    /// it.
    Gone,
}

/// A persistent connection while PDO's pool keeps it between requests
/// (#4474): the connection idle ([`ffi::IdleConn`]) and the transaction
/// style, and no function, collation or authorizer a request registered,
/// which `persistent_shutdown` gave up as php's
/// `pdo_sqlite_request_shutdown` cleans them up, nor `H`'s allocation, which
/// a persistent connection makes with `malloc`.
#[derive(EngineState)]
struct IdleHandle {
    conn: ffi::IdleConn,
    transaction_mode: i64,
}

impl IdleConnection for IdleHandle {
    /// The connection kept by the request PDO lends it to.
    fn take_up(self: Box<Self>, req: &mut Request) -> Box<dyn Connection> {
        let IdleHandle {
            conn,
            transaction_mode,
        } = *self;
        Box::new(Handle {
            place: Place::Request(req.memory_mut().adopt(conn.take_up())),
            funcs: Vec::new(),
            collations: Vec::new(),
            authorizer: None,
            transaction_mode,
            allocation: None,
        })
    }
}

impl Handle {
    /// The connection's name, while the request keeps it.
    pub(crate) fn db(&self) -> Option<Db> {
        match &self.place {
            Place::Request(owner) => Some(Db::of(owner)),
            Place::Gone => None,
        }
    }

    /// The connection's name, or php's `InternalError` where the request
    /// does not keep it, which PDO, taking every connection up before it
    /// lends it, never leaves a method to meet.
    fn db_or(&self, req: &mut Request) -> Result<Db, Failure> {
        self.db().ok_or_else(|| {
            Failure::from(php_ext::raising::fatal(
                req,
                "InternalError: a pdo_sqlite connection is used outside the request that keeps it",
            ))
        })
    }

    /// A statement served for its effect under the request, the failure
    /// handed back as the connection's error.
    fn run(&mut self, req: &mut Request, sql: &[u8]) -> Result<(), Failure> {
        let db = self.db_or(req)?;
        match db.exec(req, sql) {
            (ffi::SQLITE_OK, None) => Ok(()),
            (ffi::SQLITE_OK, Some(thrown)) => Err(Failure::from(thrown)),
            (_, thrown) => Err(failed(req, db, thrown)),
        }
    }
}

impl Connection for Handle {
    /// `sqlite_handle_preparer`: a scrollable cursor refused as php
    /// refuses it, with whatever the connection last recorded. What a
    /// statement holds of the request's is its object, its
    /// `pdo_sqlite_stmt`, PDO's columns and their names, all given back
    /// with it:
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $q = str_repeat('SELECT 1 AS a, 2 AS b', 1);
    /// $m = memory_get_usage();
    /// $s = $db->query($q);
    /// echo memory_get_usage() - $m, "\n";
    /// $s->fetch();
    /// echo memory_get_usage() - $m, "\n";
    /// unset($s);
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 360
    /// 392
    /// 32
    /// ```
    fn prepare(
        &mut self,
        req: &mut Request,
        sql: &[u8],
        options: Option<PhpArray>,
    ) -> Result<Prepared, Failure> {
        let scrollable = option_long(req, options, ATTR_CURSOR, 0)? != 0;
        // `ecalloc(1, sizeof(pdo_sqlite_stmt))`, which the statement's
        // `dtor` frees, as PDO's free of a statement its preparer failed
        // runs it.
        let held = Emalloc::new(req, PDO_SQLITE_STMT)?;
        let db = match self.db_or(req) {
            Ok(db) => db,
            Err(failure) => {
                held.free(req);
                return Err(failure);
            }
        };
        if scrollable {
            held.free(req);
            return Err(failed(req, db, None));
        }
        match db.prepare(req, sql) {
            (Ok(Some(stmt)), thrown) => {
                if let Some(thrown) = thrown {
                    held.free(req);
                    return Err(Failure::from(thrown));
                }
                Ok(Prepared {
                    statement: Box::new(Stmt {
                        handle: stmt,
                        pre_fetched: false,
                        done: false,
                        executed: false,
                        held: Some(held),
                    }),
                    placeholders: Placeholders::Native {
                        named: true,
                        positional: true,
                    },
                    parameter_map: None,
                })
            }
            (Ok(None) | Err(_), thrown) => {
                held.free(req);
                Err(failed(req, db, thrown))
            }
        }
    }

    /// `sqlite_handle_doer`.
    fn exec(&mut self, req: &mut Request, sql: &[u8]) -> Result<i64, Failure> {
        self.run(req, sql)?;
        Ok(self.db_or(req)?.changes(req))
    }

    /// `sqlite_handle_quoter`: `'...'` with each `'` doubled, `%q`, a NUL
    /// byte refused under the error handling. php prints it into
    /// `safe_emalloc(2, len, 3)` with `sqlite3_snprintf`, makes the string
    /// with `zend_string_init` and frees the buffer
    /// (`ext/pdo_sqlite/sqlite_driver.c:246-251` at php-8.5.12), so the
    /// buffer and the string are held together for a moment. After the
    /// loop's first pass:
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// foreach (['', '', 'a', "it's", str_repeat('x', 100)] as $v) {
    ///     $v .= '';
    ///     memory_reset_peak_usage(); $m = memory_get_usage();
    ///     $x = $db->quote($v);
    ///     echo strlen($x), ' ', memory_get_usage() - $m, ' ', memory_get_peak_usage() - $m, "\n";
    ///     unset($x);
    /// }
    /// ```
    /// ```expect
    /// 2 64 104
    /// 2 32 64
    /// 3 32 64
    /// 7 32 64
    /// 102 128 352
    /// ```
    fn quote(
        &mut self,
        req: &mut Request,
        text: &[u8],
        ty: BindType,
        raises: OnError,
    ) -> Result<Owner<PhpString>, Failure> {
        let _ = ty;
        if text.len() > (i32::MAX as usize - 3) / 2 {
            return Err(Failure::silent());
        }
        if text.contains(&0) {
            let message = "SQLite PDO::quote does not support null bytes";
            return match raises {
                OnError::Throw => {
                    let ce = Throwables::of(req)?;
                    Err(Failure::from(throw_exception_ex(
                        req,
                        ce.pdo_exception,
                        message,
                    )))
                }
                OnError::Warn => {
                    docref(req, Level::Warning, message);
                    Err(Failure::silent())
                }
                OnError::Silent => Err(Failure::silent()),
            };
        }
        // The length check above keeps `2 * len + 3` in range.
        let mut buffer = Emalloc::new(req, 2 * text.len() + 3)?;
        let printed = buffer.bytes_mut();
        let mut len = 0;
        let mut put = |b: u8| {
            printed[len] = b;
            len += 1;
        };
        put(b'\'');
        for &b in text {
            put(b);
            if b == b'\'' {
                put(b);
            }
        }
        put(b'\'');
        let quoted = PhpString::new(req, &buffer.bytes()[..len]);
        buffer.free(req);
        Ok(quoted?)
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
        self.db().map(|db| !db.autocommit(req))
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
                self.db_or(req)?.busy_timeout(req, n.saturating_mul(1000));
                Ok(())
            }
            ATTR_EXTENDED_RESULT_CODES => {
                let n = long_attribute(req, value)?;
                self.db_or(req)?.extended_codes(req, n);
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
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Owner<Value>>, Failure> {
        match attr {
            ATTR_CLIENT_VERSION | ATTR_SERVER_VERSION => {
                let version = PhpString::new(req, ffi::version().as_bytes())?;
                Ok(Some(version.into_value()))
            }
            ATTR_TRANSACTION_MODE => Ok(Some(Owner::int(self.transaction_mode))),
            _ => Ok(None),
        }
    }

    /// `pdo_sqlite_fetch_error_func`: the connection's `H->einfo`, which
    /// a statement's error writes too, so the latest error on either is
    /// what both answer:
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_SILENT]);
    /// $db->exec('CREATE TABLE t (a UNIQUE)');
    /// $db->exec('INSERT INTO t VALUES (1)');
    /// $db->exec('NOPE');
    /// echo json_encode($db->errorInfo()), "\n";
    /// $s = $db->query('SELECT 1');
    /// $t = $db->prepare('INSERT INTO t VALUES (1)');
    /// $t->execute();
    /// echo json_encode($t->errorInfo()), "\n";
    /// $db->exec('NOPE');
    /// echo json_encode($t->errorInfo()), "\n";
    /// $db->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_EXCEPTION);
    /// try { $t->execute(); } catch (PDOException $e) { echo $e->getMessage(), "\n", json_encode($e->errorInfo), "\n"; }
    /// ```
    /// ```expect
    /// ["HY000",1,"near \"NOPE\": syntax error"]
    /// ["23000",19,"UNIQUE constraint failed: t.a"]
    /// ["23000",1,"near \"NOPE\": syntax error"]
    /// SQLSTATE[23000]: Integrity constraint violation: 19 UNIQUE constraint failed: t.a
    /// ["23000",19,"UNIQUE constraint failed: t.a"]
    /// ```
    fn fetch_err(
        &self,
        req: &mut Request,
        stmt: Option<&dyn Statement>,
    ) -> Result<Option<NativeError>, Thrown> {
        let _ = stmt;
        let Some(db) = self.db() else {
            return Ok(None);
        };
        let einfo = db.replace_einfo(req, Einfo::default());
        let native = einfo.fetch(req);
        db.replace_einfo(req, einfo).free(req);
        native
    }

    /// `pdo_sqlite_last_insert_id`.
    fn last_insert_id(
        &mut self,
        req: &mut Request,
        name: Option<&[u8]>,
    ) -> Result<Vec<u8>, Failure> {
        let _ = name;
        let rowid = self.db_or(req)?.last_insert_rowid(req);
        Ok(rowid.to_string().into_bytes())
    }

    fn describes_at_execute(&self) -> bool {
        true
    }

    /// `pdo_sqlite_request_shutdown`, `pdo_sqlite_cleanup_callbacks`:
    /// every function and collation taken off the library and its
    /// callables given up, so none lingers for the next request: the
    /// authorizer, then the functions, then the collations, each list
    /// newest first, as php prepends them (`Z/ext/pdo_sqlite/sqlite_driver.c:104-140`).
    ///
    /// ```php
    /// class A { function __construct(public $n) {} function __destruct() { echo "dtor {$this->n}\n"; } }
    /// $db = new Pdo\Sqlite('sqlite::memory:');
    /// $f1 = new A('f1'); $db->createFunction('f1', function () use ($f1) { return 1; }); unset($f1);
    /// $c1 = new A('c1'); $db->createCollation('c1', function ($a, $b) use ($c1) { return 0; }); unset($c1);
    /// $f2 = new A('f2'); $db->createFunction('f2', function () use ($f2) { return 1; }); unset($f2);
    /// $c2 = new A('c2'); $db->createCollation('c2', function ($a, $b) use ($c2) { return 0; }); unset($c2);
    /// $au = new A('auth'); $db->setAuthorizer(function () use ($au) { return Pdo\Sqlite::OK; }); unset($au);
    /// unset($db);
    /// echo "end\n";
    /// ```
    /// ```expect exit=0
    /// dtor auth
    /// dtor f2
    /// dtor f1
    /// dtor c2
    /// dtor c1
    /// end
    /// ```
    fn persistent_shutdown(&mut self, req: &mut Request) {
        if let Some(db) = self.db() {
            db.set_authorizer(req, None);
        }
        if let Some(handler) = self.authorizer.take() {
            handler.give_up(req);
        }
        // `H->funcs` and then `H->collations`, each walked from its head,
        // which is the newest, as php prepends at registration.
        let funcs = std::mem::take(&mut self.funcs);
        let collations = std::mem::take(&mut self.collations);
        for registration in funcs.into_iter().rev().chain(collations.into_iter().rev()) {
            for handler in registration.unregister(req) {
                handler.give_up(req);
            }
        }
    }

    /// `pdo_sqlite_get_gc`.
    fn gc(&self, visit: &mut dyn FnMut(Value)) {
        if let Some(handler) = &self.authorizer {
            handler.visit(visit);
        }
        for registration in self.funcs.iter().rev().chain(self.collations.iter().rev()) {
            registration.visit(visit);
        }
    }

    /// `sqlite_handle_closer`: the callbacks cleaned up, the connection
    /// closed with `sqlite3_close_v2` and `H`'s allocation given back.
    fn close(&mut self, req: &mut Request) {
        self.persistent_shutdown(req);
        if let Place::Request(owner) = std::mem::replace(&mut self.place, Place::Gone) {
            Db::of(&owner)
                .replace_einfo(req, Einfo::default())
                .free(req);
            Db::release(req, owner);
        }
        if let Some(allocation) = self.allocation.take() {
            allocation.free(req);
        }
    }

    /// A persistent connection found in the pool and kept by this
    /// request again, alive: php's null `check_liveness`.
    fn check_liveness(&mut self, req: &mut Request) -> bool {
        let _ = req;
        true
    }

    /// The connection out of the request's keeping, idle in PDO's pool for
    /// the next request to take up: the callbacks the request registered
    /// cleaned up first, as php's `pdo_sqlite_request_shutdown` cleans them
    /// so that they do not linger for the next request, and `H`'s
    /// allocation, which a persistent connection never makes on the
    /// request's heap, freed. One the request cannot let go of, a call
    /// still holding it, is gone.
    fn give_back(mut self: Box<Self>, req: &mut Request) -> Option<Box<dyn IdleConnection>> {
        self.persistent_shutdown(req);
        if let Some(allocation) = self.allocation.take() {
            allocation.free(req);
        }
        let Place::Request(owner) = std::mem::replace(&mut self.place, Place::Gone) else {
            return None;
        };
        let conn = req.memory_mut().disown(owner)?;
        Some(Box::new(IdleHandle {
            conn: conn.idle(req),
            transaction_mode: self.transaction_mode,
        }))
    }
}

/// `sizeof(pdo_sqlite_stmt)`.
const PDO_SQLITE_STMT: usize = 24;

/// `pdo_sqlite_stmt`: the statement, and whether a row is waiting and
/// whether the last row was stepped past.
struct Stmt {
    handle: ffi::Stmt,
    pre_fetched: bool,
    done: bool,
    /// `stmt->executed`, as the binding callback reads it.
    executed: bool,
    /// The `pdo_sqlite_stmt` itself, none once the `dtor` gave it back.
    held: Option<Emalloc>,
}

impl Stmt {
    /// `pdo_sqlite_error_stmt`: the error recorded on the connection's
    /// `H->einfo`, which every statement on it shares.
    fn failed(&self, req: &mut Request, thrown: Option<Thrown>) -> Failure {
        failed(req, self.handle.db(), thrown)
    }

    /// `sqlite3_reset` with what an aggregate's final threw carried.
    fn reset(&self, req: &mut Request) -> Result<(), Failure> {
        match self.handle.reset(req).1 {
            Some(thrown) => Err(Failure::from(thrown)),
            None => Ok(()),
        }
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
                    rows: Some(self.handle.changes()),
                };
                self.reset(req)?;
                self.done = true;
                Ok(done)
            }
            ffi::SQLITE_ERROR => {
                let failure = self.failed(req, thrown);
                let _ = self.handle.reset(req);
                return Err(failure);
            }
            _ => return Err(self.failed(req, thrown)),
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
                let failure = self.failed(req, thrown);
                let _ = self.handle.reset(req);
                Err(failure)
            }
            _ => Err(self.failed(req, thrown)),
        }
    }

    /// `pdo_sqlite_stmt_describe`: the name `zend_string_init`'d, a
    /// string of the request's whatever its length.
    fn describe(&mut self, req: &mut Request, column: usize) -> Result<Column, Failure> {
        if column >= self.handle.column_count() {
            return Err(self.failed(req, None));
        }
        let name = self.handle.column_name(column).unwrap_or_default();
        Ok(Column {
            name: PhpString::filled(req, name.len(), |to| to.copy_from_slice(&name))?,
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
    ) -> Result<Owner<Value>, Failure> {
        let _ = ty;
        if column >= self.handle.data_count() {
            return Err(self.failed(req, None));
        }
        let scalar = match self.handle.column_type(column) {
            ffi::SQLITE_NULL => Value::null(),
            ffi::SQLITE_INTEGER => Value::int(self.handle.column_int64(column)),
            ffi::SQLITE_FLOAT => Value::float(self.handle.column_double(column)),
            // `ZVAL_STRINGL_FAST` (`Z/ext/pdo_sqlite/sqlite_statement.c:296`, `:301`).
            ffi::SQLITE_BLOB => {
                return Ok(PhpString::fast(req, &self.handle.column_blob(column))?.into_value());
            }
            _ => return Ok(PhpString::fast(req, &self.handle.column_text(column))?.into_value()),
        };
        Ok(Owner::take(req, scalar))
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
                    param.value.convert(req, Owner::int(n));
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
                        let bytes = text.as_bytes().to_vec();
                        param.value.convert(req, text.into_value());
                        bytes
                    }
                    ValueRef::Null => {
                        let rc = self.handle.bind_null(at);
                        return if rc == ffi::SQLITE_OK {
                            Ok(())
                        } else {
                            Err(self.failed(req, None))
                        };
                    }
                    _ => {
                        let text = to_string(req, &value)?;
                        let bytes = text.as_bytes().to_vec();
                        param.value.convert(req, text.into_value());
                        bytes
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
                } else if let Some(text) = value.as_php_string() {
                    self.handle.bind_text(at, text.as_bytes())
                } else {
                    // Bound before the parameter takes it: the write
                    // releases what the parameter held, which may run a
                    // destructor, and no view is read past it.
                    let text = to_string(req, &value)?;
                    let rc = self.handle.bind_text(at, text.as_bytes());
                    param.value.convert(req, text.into_value());
                    rc
                }
            }
        };
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(self.failed(req, None))
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
            return Err(Failure::from(type_error(
                req,
                &[
                    b"explain mode must be of type int, ".as_slice(),
                    given.as_slice(),
                    b" given".as_slice(),
                ]
                .concat(),
            )));
        };
        if !(0..=2).contains(&mode) {
            return Err(Failure::from(value_error(
                req,
                "explain mode must be one of the Pdo\\Sqlite::EXPLAIN_MODE_* constants",
            )));
        }
        if self.handle.set_explain(mode) != ffi::SQLITE_OK {
            return Err(Failure::silent());
        }
        Ok(())
    }

    /// `pdo_sqlite_stmt_get_attribute`.
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Owner<Value>>, Failure> {
        let value = match attr {
            ATTR_READONLY_STATEMENT => Value::bool(self.handle.readonly()),
            ATTR_BUSY_STATEMENT => Value::bool(self.handle.busy()),
            ATTR_EXPLAIN_STATEMENT => Value::int(self.handle.explain()),
            _ => return Ok(None),
        };
        Ok(Some(Owner::take(req, value)))
    }

    /// `pdo_sqlite_stmt_col_meta`: the native type php's own interned
    /// string, `ZSTR_KNOWN`, and every other string `add_assoc_string`'s,
    /// an allocation of its own.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $st = $db->query("SELECT 1 AS aaaa, 2 AS bb");
    /// $st->getColumnMeta(1);
    /// $m = memory_get_usage();
    /// $meta = $st->getColumnMeta(0);
    /// echo memory_get_usage() - $m, "\n";
    /// $db->exec('CREATE TABLE t (i INTEGER, f REAL, x CHAR(1))');
    /// $db->exec("INSERT INTO t VALUES (1, 1.5, 'z')");
    /// $st = $db->query('SELECT i, f, x FROM t');
    /// $st->fetch();
    /// foreach ([0, 1, 2] as $i) {
    ///     $m = memory_get_usage();
    ///     $meta = $st->getColumnMeta($i);
    ///     echo memory_get_usage() - $m, " ", $meta['native_type'], " ", $meta['sqlite:decl_type'], "\n";
    ///     unset($meta);
    /// }
    /// ```
    /// ```expect
    /// 648
    /// 112 integer INTEGER
    /// 792 double REAL
    /// 792 string CHAR(1)
    /// ```
    fn column_meta(
        &mut self,
        req: &mut Request,
        column: usize,
        executed: bool,
    ) -> Result<Owner<PhpArray>, Failure> {
        if !executed {
            return Err(Failure::silent());
        }
        if column >= self.handle.column_count() {
            return Err(self.failed(req, None));
        }
        let mut out = PhpArray::new(req)?;
        let mut flags = PhpArray::new(req)?;
        let (native, pdo_type): (&[u8], i64) = match self.handle.column_type(column) {
            ffi::SQLITE_NULL => (b"null", 0),
            ffi::SQLITE_FLOAT => (b"double", 2),
            ffi::SQLITE_BLOB => {
                let blob = string_init(req, b"blob")?;
                flags.append(req, blob)?;
                (b"string", 2)
            }
            ffi::SQLITE_INTEGER => (b"integer", 1),
            _ => (b"string", 2),
        };
        // `add_assoc_str(..., ZSTR_KNOWN(...))`: php's own interned string.
        let native = PhpString::interned(req, native)?;
        out.set_str(req, b"native_type", Owner::take(req, native).into_value())?;
        out.set_str(req, b"pdo_type", Owner::int(pdo_type))?;
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
        out.set_str(req, b"flags", flags.into_value())?;
        Ok(out)
    }

    /// `cursor_closer`, `pdo_sqlite_stmt_cursor_closer`; `pdo_sqlite` has
    /// no `next_rowset`.
    fn cursor_closer(&mut self) -> Option<&mut dyn CursorCloser> {
        Some(self)
    }

    /// `pdo_sqlite_stmt_dtor`'s `efree(S)`; the statement is finalized
    /// with its `Drop`.
    fn close(&mut self, req: &mut Request) {
        if let Some(held) = self.held.take() {
            held.free(req);
        }
    }
}

impl CursorCloser for Stmt {
    /// `pdo_sqlite_stmt_cursor_closer`.
    fn close_cursor(&mut self, req: &mut Request) -> Result<(), Failure> {
        self.executed = false;
        self.reset(req)
    }
}

/// `add_assoc_string` where there is a string.
fn set(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    key: &[u8],
    value: Option<&[u8]>,
) -> Result<(), Thrown> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = string_init(req, value)?;
    out.set_str(req, key, value)
}

/// `ZVAL_STRING`'s `zend_string_init`, which `add_assoc_string` and
/// `add_next_index_string` make their value with: an allocation of its
/// own whatever the length, where [`PhpString::new`] hands out the
/// interned empty and one-byte strings.
fn string_init(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    let made = PhpString::filled(req, bytes.len(), |to| to.copy_from_slice(bytes))?;
    Ok(made.into_value())
}

/// `pdo_sqlite_create_function_internal` and its two kin: a function
/// registered on the connection `this`'s connection.
pub(super) fn register(
    req: &mut Request,
    this: php_ext::values::PhpObject,
    name: &[u8],
    argc: i64,
    flags: i64,
    function: impl FnOnce(&Request) -> Function,
) -> Result<bool, Thrown> {
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<Handle>() else {
            return Ok(false);
        };
        // `zend_fcc_dup` once the connection stands.
        let function = function(req);
        // php hands the library its `zend_long`s as `int`s.
        #[allow(clippy::cast_possible_truncation)]
        let created = match handle.db() {
            Some(db) => db.create_function(req, name, argc as i32, flags as i32, function),
            None => Err((Box::new(function), ffi::SQLITE_MISUSE)),
        };
        match created {
            Ok(registration) => {
                handle.funcs.push(registration);
                Ok(true)
            }
            Err((function, _)) => {
                function.release(req);
                Ok(false)
            }
        }
    })
}

/// `pdo_sqlite_create_collation_internal`.
pub(super) fn register_collation(
    req: &mut Request,
    this: php_ext::values::PhpObject,
    name: &[u8],
    callback: Callable,
) -> Result<bool, Thrown> {
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<Handle>() else {
            return Ok(false);
        };
        // `zend_fcc_dup` once the connection stands.
        let handler = Owner::take(req, callback);
        let created = match handle.db() {
            Some(db) => db.create_collation(req, name, handler),
            None => Err((handler, ffi::SQLITE_MISUSE)),
        };
        match created {
            Ok(registration) => {
                handle.collations.push(registration);
                Ok(true)
            }
            Err((handler, _)) => {
                handler.give_up(req);
                Ok(false)
            }
        }
    })
}
