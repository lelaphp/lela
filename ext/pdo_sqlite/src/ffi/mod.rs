//! The binding of the platform's libsqlite3, the one the oracle links,
//! through `libsqlite3-sys`'s prebuilt declarations (design §7.2, #1510,
//! `unsafe.homes`' `ext/*/src/ffi/`): the calls
//! `sqlite_driver.c`, `sqlite_statement.c` and `pdo_sqlite.c` make, and
//! the calls `ext/sqlite3`'s `sqlite3.c` makes over the same library,
//! which reaches it through these safe types and keeps no binding of its
//! own (design §4.10, §7.2), and nothing beside them.
//!
//! **Owners.** A connection is a [`Db`], shared by the connection that
//! opened it, each statement prepared on it, each blob opened on it and
//! each call in progress that holds it, and closed with `sqlite3_close`
//! when the last of them goes, which is what `sqlite3_close_v2` gives
//! php: a statement outliving its connection in the cycle collector's
//! order still finalizes against an open connection. [`Db::close`]
//! closes it sooner, where nothing else holds it. The count is not atomic
//! because every owner of one `Db` belongs to one request, which is
//! single-threaded and moves between threads only whole (ADR-0005).
//!
//! **Callbacks.** A user function, an aggregate, a collation and the
//! authorizer are called by the library from inside `sqlite3_step`,
//! `sqlite3_exec`, `sqlite3_prepare_v2` or `sqlite3_reset`. Each of
//! those calls is made through `Db::enter`, which leaves the request
//! where a trampoline finds it for the length of the call, and hands
//! back what a callback threw, which the library cannot carry; a
//! trampoline outside such a call reaches no request and answers as php
//! answers a callback that was not served. What a callback does in PHP
//! is the extension's, a [`Callbacks`] value the connection serves with,
//! since `pdo_sqlite`'s and `sqlite3`'s callbacks are two algorithms over
//! the one library.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::{self, NonNull};

use libsqlite3_sys as sys;
use php_ext::abi::Callable;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, Value};

pub use sys::{
    SQLITE_ATTACH, SQLITE_BLOB, SQLITE_BUSY, SQLITE_CONSTRAINT, SQLITE_DENY, SQLITE_DETERMINISTIC,
    SQLITE_DONE, SQLITE_ERROR, SQLITE_FLOAT, SQLITE_IGNORE, SQLITE_INTEGER, SQLITE_INTERRUPT,
    SQLITE_LOCKED, SQLITE_NOLFS, SQLITE_NOTFOUND, SQLITE_NULL, SQLITE_OK, SQLITE_OPEN_CREATE,
    SQLITE_OPEN_READONLY, SQLITE_OPEN_READWRITE, SQLITE_OPEN_URI, SQLITE_ROW, SQLITE_TEXT,
    SQLITE_TOOBIG,
};

/// A callable kept past the call it was given to, `zend_fcc_dup`.
pub type Handler = Owner<Callable>;

/// What a registered function serves.
pub enum Function {
    Scalar(Handler),
    Aggregate { step: Handler, finalize: Handler },
}

impl Function {
    /// What its handlers count, for the cycle collector
    /// (`zend_get_gc_buffer_add_fcc`).
    pub fn visit(&self, visit: &mut dyn FnMut(Value)) {
        match self {
            Function::Scalar(handler) => handler.visit(visit),
            Function::Aggregate { step, finalize } => {
                step.visit(visit);
                finalize.visit(visit);
            }
        }
    }

    /// Its handlers, handed over.
    pub fn into_handlers(self) -> Vec<Handler> {
        match self {
            Function::Scalar(handler) => vec![handler],
            Function::Aggregate { step, finalize } => vec![step, finalize],
        }
    }

    /// Its handlers given up, `zend_fcc_dtor`.
    pub fn release(self, req: &mut Request) {
        match self {
            Function::Scalar(handler) => handler.give_up(req),
            Function::Aggregate { step, finalize } => {
                step.give_up(req);
                finalize.give_up(req);
            }
        }
    }
}

/// One group's aggregate state, which the library keeps between the
/// step calls and hands the final: the value the callbacks are handed
/// and the rows counted so far.
pub struct Aggregate {
    pub value: Owner<Value>,
    pub row: i64,
}

/// An aggregate's state given up, after its final or without it.
pub fn drop_aggregate(req: &mut Request, state: Option<Box<Aggregate>>) {
    if let Some(state) = state {
        state.value.give_up(req);
    }
}

/// What an extension does in PHP when the library calls back: the
/// bodies of its `do_callback`, collation and authorizer, served with
/// the request of the call in progress. A connection serves with one
/// static value, set when it opens and changed with
/// [`Db::serve_with`].
pub trait Callbacks: Sync {
    /// A scalar function's call, its answer set on `call`.
    fn scalar(
        &self,
        req: &mut Request,
        handler: &Callable,
        call: &mut Invocation<'_>,
    ) -> Result<(), Thrown>;

    /// An aggregate's step over the group's state, empty the first time.
    fn step(
        &self,
        req: &mut Request,
        handler: &Callable,
        state: &mut Option<Box<Aggregate>>,
        args: Vec<Argument>,
    ) -> Result<(), Thrown>;

    /// An aggregate's final, the state handed over, its answer set on
    /// `call`.
    fn finalize(
        &self,
        req: &mut Request,
        handler: &Callable,
        state: Option<Box<Aggregate>>,
        call: &mut Invocation<'_>,
    ) -> Result<(), Thrown>;

    /// A collation's comparison of two strings: the order the library
    /// is answered, and what the callback threw beside it.
    fn collate(
        &self,
        req: &mut Request,
        handler: &Callable,
        left: &[u8],
        right: &[u8],
    ) -> (i32, Option<Thrown>);

    /// The authorizer's answer to one action and its four names, with
    /// the handler set, if any, and whether a callback already threw
    /// during the call in progress.
    fn authorize(
        &self,
        req: &mut Request,
        handler: Option<&Callable>,
        action: i32,
        names: [Option<Vec<u8>>; 4],
        pending: bool,
    ) -> Result<i32, Thrown>;
}

/// `sqlite3_libversion()`: the keg's library, the one the oracle links
/// (#1510), its version and its compile options the oracle's.
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// echo $db->getAttribute(PDO::ATTR_SERVER_VERSION), ' ', SQLite3::version()['versionNumber'], "\n";
/// echo wordwrap(implode(' ', $db->query('PRAGMA compile_options')->fetchAll(PDO::FETCH_COLUMN)), 72), "\n";
/// ```
/// ```expect
/// 3.53.4 3053004
/// ATOMIC_INTRINSICS=1 COMPILER=clang-17.0.0 DEFAULT_AUTOVACUUM
/// DEFAULT_CACHE_SIZE=-2000 DEFAULT_FILE_FORMAT=4
/// DEFAULT_JOURNAL_SIZE_LIMIT=-1 DEFAULT_MMAP_SIZE=0 DEFAULT_PAGE_SIZE=4096
/// DEFAULT_PCACHE_INITSZ=20 DEFAULT_RECURSIVE_TRIGGERS
/// DEFAULT_SECTOR_SIZE=4096 DEFAULT_SYNCHRONOUS=2
/// DEFAULT_WAL_AUTOCHECKPOINT=1000 DEFAULT_WAL_SYNCHRONOUS=2
/// DEFAULT_WORKER_THREADS=0 DIRECT_OVERFLOW_READ ENABLE_API_ARMOR
/// ENABLE_COLUMN_METADATA ENABLE_DBSTAT_VTAB ENABLE_FTS3
/// ENABLE_FTS3_PARENTHESIS ENABLE_FTS5 ENABLE_GEOPOLY ENABLE_MATH_FUNCTIONS
/// ENABLE_MEMORY_MANAGEMENT ENABLE_PERCENTILE ENABLE_PREUPDATE_HOOK
/// ENABLE_RTREE ENABLE_SESSION ENABLE_STAT4 ENABLE_UNLOCK_NOTIFY
/// MALLOC_SOFT_LIMIT=1024 MAX_ATTACHED=10 MAX_COLUMN=2000
/// MAX_COMPOUND_SELECT=500 MAX_DEFAULT_PAGE_SIZE=8192 MAX_EXPR_DEPTH=1000
/// MAX_FUNCTION_ARG=1000 MAX_LENGTH=1000000000
/// MAX_LIKE_PATTERN_LENGTH=50000 MAX_MMAP_SIZE=0x7fff0000
/// MAX_PAGE_COUNT=0xfffffffe MAX_PAGE_SIZE=65536 MAX_SQL_LENGTH=1000000000
/// MAX_TRIGGER_DEPTH=1000 MAX_VARIABLE_NUMBER=250000 MAX_VDBE_OP=250000000
/// MAX_WORKER_THREADS=8 MUTEX_PTHREADS SYSTEM_MALLOC TEMP_STORE=1
/// THREADSAFE=1 USE_URI
/// ```
pub fn version() -> String {
    // SAFETY: no argument and no state.
    let raw = unsafe { sys::sqlite3_libversion() };
    // SAFETY: the library's version is a static NUL-terminated string.
    let text = unsafe { CStr::from_ptr(raw) };
    text.to_string_lossy().into_owned()
}

/// `sqlite3_libversion_number()`.
pub fn version_number() -> i64 {
    // SAFETY: no argument and no state.
    i64::from(unsafe { sys::sqlite3_libversion_number() })
}

/// A C string of the bytes up to the first NUL, as php hands
/// `ZSTR_VAL` to the library.
fn c_string(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// A C length of a Rust one, the library's `int`.
fn c_len(len: usize) -> c_int {
    c_int::try_from(len).unwrap_or(c_int::MAX)
}

/// The bytes of a NUL-terminated string the library owns, `None` for
/// its null.
fn bytes_of(raw: *const c_char) -> Option<Vec<u8>> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: a non-null string the library answered, NUL-terminated
    // and live until the library's next call, which comes after the copy.
    let text = unsafe { CStr::from_ptr(raw) };
    Some(text.to_bytes().to_vec())
}

/// What one connection and everything opened on it share.
struct Inner {
    raw: NonNull<sys::sqlite3>,
    owners: usize,
    /// The request of the call in progress, where a callback reaches it;
    /// null between calls.
    req: *mut Request,
    /// What a callback threw during the call in progress, the first one.
    pending: Option<Thrown>,
    /// `authorizer_fcc`, what `setAuthorizer()` set.
    authorizer: Option<Handler>,
    /// What the callbacks do in PHP.
    callbacks: &'static dyn Callbacks,
}

/// `sqlite3 *`, one owner of it.
pub struct Db(NonNull<Inner>);

// SAFETY: every owner of one `Db` (the handle, its statements, its
// blobs and the callbacks registered on it) is reachable only from the
// request that opened it, which is served by one thread at a time and
// moves between threads only whole, so the count and the fields behind
// the pointer are never touched from two threads at once.
unsafe impl Send for Db {}

impl Clone for Db {
    fn clone(&self) -> Db {
        // SAFETY: `self` is an owner, so the allocation is live, and no
        // other reference to it is held across this write.
        unsafe { (*self.0.as_ptr()).owners += 1 };
        Db(self.0)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // SAFETY: as in `clone`.
        let left = unsafe {
            let inner = &mut *self.0.as_ptr();
            inner.owners -= 1;
            inner.owners
        };
        if left > 0 {
            return;
        }
        // SAFETY: the last owner: the connection is closed once, every
        // statement and blob on it having dropped its owner first.
        unsafe { sys::sqlite3_close(self.inner_raw()) };
        // SAFETY: the allocation `open` leaked, freed by its last owner.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

impl Db {
    fn inner_raw(&self) -> *mut sys::sqlite3 {
        // SAFETY: `self` is an owner; the field is read, nothing held.
        unsafe { (*self.0.as_ptr()).raw.as_ptr() }
    }

    /// `sqlite3_open_v2(filename, &db, flags, NULL)` with the authorizer
    /// installed as `pdo_sqlite_handle_factory` and `SQLite3::open`
    /// install it, serving its callbacks with `callbacks`: the code and
    /// the text of the error where it failed, `sqlite3_errstr(rc)` where
    /// the library made no connection to ask.
    pub fn open(
        filename: &[u8],
        flags: i32,
        callbacks: &'static dyn Callbacks,
    ) -> Result<Db, (i32, Vec<u8>)> {
        let name = c_string(filename);
        let mut raw = ptr::null_mut();
        // SAFETY: the name is NUL-terminated and outlives the call, the
        // out-pointer a live local, the VFS the default.
        let rc = unsafe { sys::sqlite3_open_v2(name.as_ptr(), &raw mut raw, flags, ptr::null()) };
        let Some(raw) = NonNull::new(raw) else {
            // SAFETY: the library's static text for a code.
            let text = bytes_of(unsafe { sys::sqlite3_errstr(rc) });
            return Err((rc, text.unwrap_or_default()));
        };
        let inner = Box::into_raw(Box::new(Inner {
            raw,
            owners: 1,
            req: ptr::null_mut(),
            pending: None,
            authorizer: None,
            callbacks,
        }));
        // SAFETY: `into_raw` never answers null.
        let db = Db(unsafe { NonNull::new_unchecked(inner) });
        if rc != SQLITE_OK {
            let failed = (db.errcode(), db.errmsg());
            return Err(failed);
        }
        // SAFETY: the connection is live and the argument is its shared
        // state, which lives as long as the connection does.
        unsafe { sys::sqlite3_set_authorizer(raw.as_ptr(), Some(authorize), inner.cast()) };
        Ok(db)
    }

    /// `sqlite3_errcode`.
    pub fn errcode(&self) -> i32 {
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_errcode(self.inner_raw()) }
    }

    /// `sqlite3_errmsg`.
    pub fn errmsg(&self) -> Vec<u8> {
        // SAFETY: a live connection; the message is copied at once.
        bytes_of(unsafe { sys::sqlite3_errmsg(self.inner_raw()) }).unwrap_or_default()
    }

    /// `sqlite3_extended_errcode`.
    pub fn extended_errcode(&self) -> i32 {
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_extended_errcode(self.inner_raw()) }
    }

    /// The callbacks served from here on: `SQLite3::enableExceptions`
    /// changes what its authorizer raises.
    pub fn serve_with(&self, callbacks: &'static dyn Callbacks) {
        // SAFETY: an owner's allocation; the write is the only access, and
        // a trampoline copies the reference out before it calls back.
        unsafe { (*self.0.as_ptr()).callbacks = callbacks };
    }

    /// `sqlite3_db_config(db, SQLITE_DBCONFIG_DEFENSIVE, 1, NULL)`,
    /// `sqlite3.defensive`.
    pub fn defensive(&self) {
        // SAFETY: a live connection; the option takes an `int` and an
        // `int *` it may leave null.
        unsafe {
            sys::sqlite3_db_config(
                self.inner_raw(),
                sys::SQLITE_DBCONFIG_DEFENSIVE,
                1,
                ptr::null_mut::<c_int>(),
            )
        };
    }

    /// `sqlite3_close` now, the functions and collations registered on
    /// the connection taken off it first, their handlers handed back:
    /// `Err` with the connection still open and everything handed back,
    /// the code and the library's text, where something beside them
    /// holds it, the text `sqlite3_close` sets for a connection a
    /// statement, a blob or a call in progress still holds.
    pub fn close(self, registrations: Vec<Registration>) -> Result<Vec<Handler>, Box<Unclosed>> {
        // SAFETY: `self` is an owner; the field is read.
        let owners = unsafe { (*self.0.as_ptr()).owners };
        let held = registrations.iter().filter(|r| r.db.0 == self.0).count();
        if owners > 1 + held {
            let message =
                b"unable to close due to unfinalized statements or unfinished backups".to_vec();
            return Err(Box::new(Unclosed {
                db: self,
                registrations,
                released: Vec::new(),
                code: SQLITE_BUSY,
                message,
            }));
        }
        let released: Vec<Handler> = registrations
            .into_iter()
            .flat_map(Registration::unregister)
            .collect();
        // SAFETY: the last owner, so no statement, blob or registration
        // can reach the connection after it is closed.
        let rc = unsafe { sys::sqlite3_close(self.inner_raw()) };
        if rc != SQLITE_OK {
            let (code, message) = (self.errcode(), self.errmsg());
            return Err(Box::new(Unclosed {
                db: self,
                registrations: Vec::new(),
                released,
                code,
                message,
            }));
        }
        let inner = self.0;
        #[expect(
            clippy::mem_forget,
            reason = "the connection is closed above, so its `Drop` would close it again; the allocation is freed once below"
        )]
        std::mem::forget(self);
        // SAFETY: the allocation `open` leaked, its connection closed and
        // its last owner forgotten, freed once.
        drop(unsafe { Box::from_raw(inner.as_ptr()) });
        Ok(released)
    }

    /// `sqlite3_backup_init`, `sqlite3_backup_step(-1)` until it stops
    /// answering `SQLITE_OK`, and `sqlite3_backup_finish`: the finish's
    /// code, or this connection's error code where the backup could not
    /// start.
    pub fn backup(&self, to: &Db, from_name: &[u8], to_name: &[u8]) -> i32 {
        let (from_name, to_name) = (c_string(from_name), c_string(to_name));
        // SAFETY: two live connections and two NUL-terminated names.
        let backup = unsafe {
            sys::sqlite3_backup_init(
                to.inner_raw(),
                to_name.as_ptr(),
                self.inner_raw(),
                from_name.as_ptr(),
            )
        };
        if backup.is_null() {
            return self.errcode();
        }
        // SAFETY: the backup made above, live until its finish.
        while unsafe { sys::sqlite3_backup_step(backup, -1) } == SQLITE_OK {}
        // SAFETY: as above, finished once.
        unsafe { sys::sqlite3_backup_finish(backup) }
    }

    /// `sqlite3_changes`.
    pub fn changes(&self) -> i64 {
        // SAFETY: a live connection.
        i64::from(unsafe { sys::sqlite3_changes(self.inner_raw()) })
    }

    /// `sqlite3_last_insert_rowid`.
    pub fn last_insert_rowid(&self) -> i64 {
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_last_insert_rowid(self.inner_raw()) }
    }

    /// `sqlite3_get_autocommit`.
    pub fn autocommit(&self) -> bool {
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_get_autocommit(self.inner_raw()) != 0 }
    }

    /// `sqlite3_busy_timeout`.
    pub fn busy_timeout(&self, ms: i64) {
        let ms = c_int::try_from(ms).unwrap_or(if ms < 0 { c_int::MIN } else { c_int::MAX });
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_busy_timeout(self.inner_raw(), ms) };
    }

    /// `sqlite3_extended_result_codes`.
    pub fn extended_codes(&self, on: i64) {
        // php hands the library its `zend_long` as an `int`.
        #[allow(clippy::cast_possible_truncation)]
        // SAFETY: a live connection.
        unsafe {
            sys::sqlite3_extended_result_codes(self.inner_raw(), on as c_int)
        };
    }

    /// `f` served with the request left where a callback finds it, and
    /// what a callback threw meanwhile handed back beside `f`'s answer.
    fn enter<R>(
        &self,
        req: &mut Request,
        f: impl FnOnce(*mut sys::sqlite3) -> R,
    ) -> (R, Option<Thrown>) {
        let slot = self.0.as_ptr();
        // SAFETY: an owner's allocation; the write is the only access.
        let outer = unsafe { std::mem::replace(&mut (*slot).req, ptr::from_mut(req)) };
        let answered = f(self.inner_raw());
        // SAFETY: as above; a callback's own `enter` restored what it
        // found before this read.
        let thrown = unsafe { (*slot).pending.take() };
        // SAFETY: as above.
        unsafe { (*slot).req = outer };
        (answered, thrown)
    }

    /// `sqlite3_exec(db, sql, NULL, NULL, NULL)`.
    pub fn exec(&self, req: &mut Request, sql: &[u8]) -> (i32, Option<Thrown>) {
        let sql = c_string(sql);
        self.enter(req, |raw| {
            // SAFETY: a live connection and a NUL-terminated statement that
            // outlives the call; no row callback.
            unsafe { sys::sqlite3_exec(raw, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) }
        })
    }

    /// `sqlite3_prepare_v2(db, sql, len, &stmt, &tail)`: the statement,
    /// none for a text with no statement in it, or the library's code.
    pub fn prepare(
        &self,
        req: &mut Request,
        sql: &[u8],
    ) -> (Result<Option<Stmt>, i32>, Option<Thrown>) {
        let mut raw = ptr::null_mut();
        let (rc, thrown) = self.enter(req, |db| {
            let mut tail = ptr::null();
            // SAFETY: a live connection; the statement is `sql`'s bytes and
            // length, the two out-pointers live locals.
            unsafe {
                sys::sqlite3_prepare_v2(
                    db,
                    sql.as_ptr().cast(),
                    c_len(sql.len()),
                    &raw mut raw,
                    &raw mut tail,
                )
            }
        });
        let prepared = match NonNull::new(raw) {
            Some(raw) if rc == SQLITE_OK => Ok(Some(Stmt {
                raw,
                db: self.clone(),
            })),
            Some(raw) => {
                // SAFETY: a statement the library made and nothing else holds.
                unsafe { sys::sqlite3_finalize(raw.as_ptr()) };
                Err(rc)
            }
            None if rc == SQLITE_OK => Ok(None),
            None => Err(rc),
        };
        (prepared, thrown)
    }

    /// `sqlite3_create_function` for a scalar or an aggregate, the
    /// registration the connection keeps until it closes.
    pub fn create_function(
        &self,
        name: &[u8],
        argc: i32,
        flags: i32,
        function: Function,
    ) -> Result<Registration, (Box<Function>, i32)> {
        let aggregate = matches!(function, Function::Aggregate { .. });
        let registered = Box::into_raw(Box::new(Registered {
            inner: self.0,
            function,
        }));
        let c_name = c_string(name);
        let (func, step, fin): (Scalar, Scalar, Final) = if aggregate {
            (None, Some(step), Some(finalize))
        } else {
            (Some(scalar), None, None)
        };
        // SAFETY: a live connection, a NUL-terminated name, and as the
        // function's data the registration, which lives until
        // `unregister` removes the function from the library.
        let rc = unsafe {
            sys::sqlite3_create_function_v2(
                self.inner_raw(),
                c_name.as_ptr(),
                argc,
                flags | sys::SQLITE_UTF8,
                registered.cast(),
                func,
                step,
                fin,
                None,
            )
        };
        if rc != SQLITE_OK {
            // SAFETY: the box made above, which the library did not take.
            let registered = unsafe { Box::from_raw(registered) };
            return Err((Box::new(registered.function), rc));
        }
        Ok(Registration {
            db: self.clone(),
            name: c_name,
            argc,
            kind: Kind::Function(registered),
        })
    }

    /// `sqlite3_create_collation(db, name, SQLITE_UTF8, collation, callback)`.
    pub fn create_collation(
        &self,
        name: &[u8],
        handler: Handler,
    ) -> Result<Registration, (Handler, i32)> {
        let registered = Box::into_raw(Box::new(Collation {
            inner: self.0,
            handler,
        }));
        let c_name = c_string(name);
        // SAFETY: as in `create_function`.
        let rc = unsafe {
            sys::sqlite3_create_collation_v2(
                self.inner_raw(),
                c_name.as_ptr(),
                sys::SQLITE_UTF8,
                registered.cast(),
                Some(collate),
                None,
            )
        };
        if rc != SQLITE_OK {
            // SAFETY: the box made above, which the library did not take.
            let registered = unsafe { Box::from_raw(registered) };
            return Err((registered.handler, rc));
        }
        Ok(Registration {
            db: self.clone(),
            name: c_name,
            argc: 0,
            kind: Kind::Collation(registered),
        })
    }

    /// `authorizer_fcc`: the handler set, the one it replaced handed back.
    pub fn set_authorizer(&self, handler: Option<Handler>) -> Option<Handler> {
        // SAFETY: an owner's allocation; the write is the only access.
        unsafe { std::mem::replace(&mut (*self.0.as_ptr()).authorizer, handler) }
    }

    /// What the authorizer set counts, for the cycle collector
    /// (`zend_get_gc_buffer_add_fcc`).
    pub fn visit_authorizer(&self, visit: &mut dyn FnMut(Value)) {
        // SAFETY: an owner's allocation; the field is read and nothing
        // runs while it is.
        if let Some(handler) = unsafe { &(*self.0.as_ptr()).authorizer } {
            handler.visit(visit);
        }
    }

    /// `sqlite3_load_extension` with the C API's loading enabled for the
    /// call alone, `pdosqlite` `loadExtension`'s dance: the library's
    /// error text where it failed.
    pub fn load_extension(&self, path: &[u8]) -> Result<(), Vec<u8>> {
        let path = c_string(path);
        self.load_config(1);
        let mut error: *mut c_char = ptr::null_mut();
        // SAFETY: a live connection, a NUL-terminated path, the default
        // entry point and a live out-pointer for the message.
        let rc = unsafe {
            sys::sqlite3_load_extension(
                self.inner_raw(),
                path.as_ptr(),
                ptr::null(),
                &raw mut error,
            )
        };
        let answered = if rc == SQLITE_OK {
            Ok(())
        } else {
            Err(bytes_of(error).unwrap_or_default())
        };
        if !error.is_null() {
            // SAFETY: the message the library allocated for us to free.
            unsafe { sys::sqlite3_free(error.cast()) };
        }
        self.load_config(0);
        answered
    }

    /// `sqlite3_db_config(db, SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, on, NULL)`.
    fn load_config(&self, on: c_int) {
        // SAFETY: a live connection; the option takes an `int` and an
        // `int *` it may leave null.
        unsafe {
            sys::sqlite3_db_config(
                self.inner_raw(),
                sys::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION,
                on,
                ptr::null_mut::<c_int>(),
            )
        };
    }

    /// `sqlite3_blob_open`: the blob, or the library's error text.
    pub fn blob_open(
        &self,
        dbname: &[u8],
        table: &[u8],
        column: &[u8],
        row: i64,
        writable: bool,
    ) -> Result<Blob, Vec<u8>> {
        let (dbname, table, column) = (c_string(dbname), c_string(table), c_string(column));
        let mut raw = ptr::null_mut();
        // SAFETY: a live connection, three NUL-terminated names and a
        // live out-pointer.
        let rc = unsafe {
            sys::sqlite3_blob_open(
                self.inner_raw(),
                dbname.as_ptr(),
                table.as_ptr(),
                column.as_ptr(),
                row,
                c_int::from(writable),
                &raw mut raw,
            )
        };
        match NonNull::new(raw) {
            Some(raw) if rc == SQLITE_OK => Ok(Blob {
                raw,
                db: self.clone(),
            }),
            _ => Err(self.errmsg()),
        }
    }
}

/// What [`Db::close`] hands back where the connection stays open: the
/// connection, the registrations it was given, the handlers of any it
/// took off, and the code and text of why.
pub struct Unclosed {
    pub db: Db,
    pub registrations: Vec<Registration>,
    pub released: Vec<Handler>,
    pub code: i32,
    pub message: Vec<u8>,
}

/// The type of a scalar or step callback in a registration.
type Scalar =
    Option<unsafe extern "C" fn(*mut sys::sqlite3_context, c_int, *mut *mut sys::sqlite3_value)>;
/// The type of a final callback.
type Final = Option<unsafe extern "C" fn(*mut sys::sqlite3_context)>;

/// A function's data as the library holds it: its connection's shared
/// state and what it serves.
struct Registered {
    inner: NonNull<Inner>,
    function: Function,
}

/// A collation's data as the library holds it.
struct Collation {
    inner: NonNull<Inner>,
    handler: Handler,
}

enum Kind {
    Function(*mut Registered),
    Collation(*mut Collation),
}

/// One function or collation registered on a connection, php's
/// `pdo_sqlite_func` or `pdo_sqlite_collation`, which the connection
/// keeps until it closes ([`Registration::unregister`], [`Db::close`]):
/// an owner of the connection, so the data the library holds for it is
/// never freed while the library can call back with it.
pub struct Registration {
    db: Db,
    name: CString,
    argc: i32,
    kind: Kind,
}

// SAFETY: a registration is held by its connection, in the one request
// that opened it; see `Db`.
unsafe impl Send for Registration {}

impl Registration {
    /// Taken off the library, `sqlite3_create_function` or
    /// `sqlite3_create_collation` with no callback, and the handlers it
    /// held handed back for the caller to release.
    pub fn unregister(self) -> Vec<Handler> {
        let Registration {
            db,
            name,
            argc,
            kind,
        } = self;
        match kind {
            Kind::Function(registered) => {
                // SAFETY: the registration's live connection; no callback
                // and no data, which deletes the function.
                unsafe {
                    sys::sqlite3_create_function_v2(
                        db.inner_raw(),
                        name.as_ptr(),
                        argc,
                        sys::SQLITE_UTF8,
                        ptr::null_mut(),
                        None,
                        None,
                        None,
                        None,
                    )
                };
                // SAFETY: the registration's own box, no longer the
                // library's data.
                let registered = unsafe { Box::from_raw(registered) };
                registered.function.into_handlers()
            }
            Kind::Collation(registered) => {
                // SAFETY: as above, for a collation.
                unsafe {
                    sys::sqlite3_create_collation_v2(
                        db.inner_raw(),
                        name.as_ptr(),
                        sys::SQLITE_UTF8,
                        ptr::null_mut(),
                        None,
                        None,
                    )
                };
                // SAFETY: as above.
                let registered = unsafe { Box::from_raw(registered) };
                vec![registered.handler]
            }
        }
    }

    /// What its handlers count, for the cycle collector.
    pub fn visit(&self, visit: &mut dyn FnMut(Value)) {
        match self.kind {
            Kind::Function(registered) => {
                // SAFETY: a registration's box, live until `unregister`.
                let registered = unsafe { &*registered };
                registered.function.visit(visit);
            }
            Kind::Collation(registered) => {
                // SAFETY: as above.
                let registered = unsafe { &*registered };
                registered.handler.visit(visit);
            }
        }
    }
}

/// The request of the call in progress on `inner`'s connection, `None`
/// outside one.
fn request<'a>(inner: NonNull<Inner>) -> Option<&'a mut Request> {
    // SAFETY: the shared state of a connection a registration or the
    // authorizer names, live while the library calls back.
    let req = unsafe { (*inner.as_ptr()).req };
    // SAFETY: set by `Db::enter` from the `&mut Request` it was given,
    // which that call does not touch until the library returns, so this
    // is the one live mutable borrow of it.
    unsafe { req.as_mut() }
}

/// What `inner`'s connection serves its callbacks with.
fn callbacks(inner: NonNull<Inner>) -> &'static dyn Callbacks {
    // SAFETY: as in `request`; the reference is copied out.
    unsafe { (*inner.as_ptr()).callbacks }
}

/// Whether a callback already threw during the call in progress.
fn pending(inner: NonNull<Inner>) -> bool {
    // SAFETY: as in `request`.
    unsafe { (*inner.as_ptr()).pending.is_some() }
}

/// What a callback threw, kept for the call to hand back; the first
/// one stays.
fn throw_out(inner: NonNull<Inner>, thrown: Thrown) {
    // SAFETY: as in `request`.
    let slot = unsafe { &mut (*inner.as_ptr()).pending };
    if slot.is_none() {
        *slot = Some(thrown);
    }
}

/// One call of a user function, `sqlite3_context` with its arguments.
pub struct Invocation<'a> {
    ctx: *mut sys::sqlite3_context,
    args: &'a [*mut sys::sqlite3_value],
}

/// An argument as `do_callback` reads it.
pub enum Argument {
    /// `sqlite3_value_int64`; `pdo_sqlite` reads the library's 32 bits,
    /// `sqlite3_value_int`, which is this truncated.
    Int(i64),
    Float(f64),
    Null,
    Text(Vec<u8>),
}

impl Invocation<'_> {
    /// The arguments, each as its storage class reads.
    pub fn arguments(&self) -> Vec<Argument> {
        self.args
            .iter()
            .map(|&value| {
                // SAFETY: an argument the library handed this call.
                let class = unsafe { sys::sqlite3_value_type(value) };
                match class {
                    SQLITE_INTEGER => {
                        // SAFETY: as above.
                        Argument::Int(unsafe { sys::sqlite3_value_int64(value) })
                    }
                    SQLITE_FLOAT => {
                        // SAFETY: as above.
                        Argument::Float(unsafe { sys::sqlite3_value_double(value) })
                    }
                    SQLITE_NULL => Argument::Null,
                    _ => {
                        // SAFETY: as above; the text first, then its length.
                        let text = unsafe { sys::sqlite3_value_text(value) };
                        // SAFETY: as above.
                        let len = unsafe { sys::sqlite3_value_bytes(value) };
                        Argument::Text(slice(text.cast(), len))
                    }
                }
            })
            .collect()
    }

    /// `sqlite3_result_int`.
    pub fn return_int(&mut self, n: i32) {
        // SAFETY: this call's context.
        unsafe { sys::sqlite3_result_int(self.ctx, n) };
    }

    /// `sqlite3_result_int64`.
    pub fn return_int64(&mut self, n: i64) {
        // SAFETY: this call's context.
        unsafe { sys::sqlite3_result_int64(self.ctx, n) };
    }

    /// `sqlite3_result_double`.
    pub fn return_double(&mut self, f: f64) {
        // SAFETY: this call's context.
        unsafe { sys::sqlite3_result_double(self.ctx, f) };
    }

    /// `sqlite3_result_null`.
    pub fn return_null(&mut self) {
        // SAFETY: this call's context.
        unsafe { sys::sqlite3_result_null(self.ctx) };
    }

    /// `sqlite3_result_text(..., SQLITE_TRANSIENT)`.
    pub fn return_text(&mut self, text: &[u8]) {
        // SAFETY: this call's context; the library copies the bytes.
        unsafe {
            sys::sqlite3_result_text(
                self.ctx,
                text.as_ptr().cast(),
                c_len(text.len()),
                sys::SQLITE_TRANSIENT(),
            );
        };
    }

    /// `sqlite3_result_error(context, "failed to invoke callback", 0)`,
    /// php's text and php's length of zero, which the library takes as
    /// the empty message.
    pub fn failed(&mut self) {
        // SAFETY: this call's context and a static message.
        unsafe { sys::sqlite3_result_error(self.ctx, c"failed to invoke callback".as_ptr(), 0) };
    }

    /// `sqlite3_aggregate_context`: the aggregate's state for this group,
    /// empty the first time, `None` where the library had no memory.
    fn aggregate(&mut self) -> Option<&mut Option<Box<Aggregate>>> {
        let size = c_len(std::mem::size_of::<Option<Box<Aggregate>>>());
        // SAFETY: this call's context.
        let slot = unsafe { sys::sqlite3_aggregate_context(self.ctx, size) };
        let slot = slot.cast::<Option<Box<Aggregate>>>();
        // SAFETY: memory the library zeroed on first use and keeps for
        // this group, pointer-aligned and sized for the option, whose
        // all-zero value is `None`; nothing else borrows it.
        unsafe { slot.as_mut() }
    }
}

/// A copy of `len` bytes at `at`, the empty string for none.
fn slice(at: *const u8, len: c_int) -> Vec<u8> {
    let Ok(len) = usize::try_from(len) else {
        return Vec::new();
    };
    if at.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: `len` bytes the library answered at `at`, live until its
    // next call on the same value, which comes after the copy.
    unsafe { std::slice::from_raw_parts(at, len) }.to_vec()
}

/// The registration a call of a function carries.
fn registered<'a>(ctx: *mut sys::sqlite3_context) -> Option<&'a Registered> {
    // SAFETY: the context of a call the library is making.
    let data = unsafe { sys::sqlite3_user_data(ctx) };
    // SAFETY: the data `create_function` gave the library, a live
    // registration until `unregister`, read and never written.
    unsafe { data.cast::<Registered>().as_ref() }
}

/// The arguments of a call as a slice.
fn arguments<'a>(
    count: c_int,
    values: *mut *mut sys::sqlite3_value,
) -> &'a [*mut sys::sqlite3_value] {
    let Ok(count) = usize::try_from(count) else {
        return &[];
    };
    if values.is_null() || count == 0 {
        return &[];
    }
    // SAFETY: the library's `argc` argument pointers at `argv`, live for
    // the call.
    unsafe { std::slice::from_raw_parts(values, count) }
}

/// `php_sqlite3_func_callback`.
unsafe extern "C" fn scalar(
    ctx: *mut sys::sqlite3_context,
    count: c_int,
    values: *mut *mut sys::sqlite3_value,
) {
    let mut call = Invocation {
        ctx,
        args: arguments(count, values),
    };
    let Some(registered) = registered(ctx) else {
        return call.failed();
    };
    let Function::Scalar(handler) = &registered.function else {
        return call.failed();
    };
    let inner = registered.inner;
    let Some(req) = request(inner).filter(|_| !pending(inner)) else {
        return call.failed();
    };
    if let Err(thrown) = callbacks(inner).scalar(req, handler, &mut call) {
        throw_out(inner, thrown);
    }
}

/// `php_sqlite3_func_step_callback`.
unsafe extern "C" fn step(
    ctx: *mut sys::sqlite3_context,
    count: c_int,
    values: *mut *mut sys::sqlite3_value,
) {
    let mut call = Invocation {
        ctx,
        args: arguments(count, values),
    };
    let Some(registered) = registered(ctx) else {
        return;
    };
    let Function::Aggregate { step, .. } = &registered.function else {
        return;
    };
    let inner = registered.inner;
    let Some(req) = request(inner).filter(|_| !pending(inner)) else {
        return;
    };
    let args = call.arguments();
    let Some(state) = call.aggregate() else {
        return;
    };
    if let Err(thrown) = callbacks(inner).step(req, step, state, args) {
        throw_out(inner, thrown);
    }
}

/// `php_sqlite3_func_final_callback`.
unsafe extern "C" fn finalize(ctx: *mut sys::sqlite3_context) {
    let mut call = Invocation { ctx, args: &[] };
    let Some(registered) = registered(ctx) else {
        return;
    };
    let Function::Aggregate { finalize, .. } = &registered.function else {
        return;
    };
    let inner = registered.inner;
    let state = call.aggregate().and_then(Option::take);
    let Some(req) = request(inner) else {
        return;
    };
    if pending(inner) {
        drop_aggregate(req, state);
        return call.failed();
    }
    if let Err(thrown) = callbacks(inner).finalize(req, finalize, state, &mut call) {
        throw_out(inner, thrown);
    }
}

/// `php_sqlite3_collation_callback`.
unsafe extern "C" fn collate(
    data: *mut c_void,
    left_len: c_int,
    left: *const c_void,
    right_len: c_int,
    right: *const c_void,
) -> c_int {
    // SAFETY: the data `create_collation` gave the library, a live
    // registration until `unregister`, read and never written.
    let Some(collation) = (unsafe { data.cast::<Collation>().as_ref() }) else {
        return 0;
    };
    let inner = collation.inner;
    let Some(req) = request(inner).filter(|_| !pending(inner)) else {
        return 0;
    };
    let left = slice(left.cast(), left_len);
    let right = slice(right.cast(), right_len);
    let (order, thrown) = callbacks(inner).collate(req, &collation.handler, &left, &right);
    if let Some(thrown) = thrown {
        throw_out(inner, thrown);
    }
    order
}

/// `authorizer`: `SQLITE_OK` without a handler, the handler's answer
/// with one.
unsafe extern "C" fn authorize(
    data: *mut c_void,
    action: c_int,
    first: *const c_char,
    second: *const c_char,
    third: *const c_char,
    fourth: *const c_char,
) -> c_int {
    let Some(inner) = NonNull::new(data.cast::<Inner>()) else {
        return SQLITE_OK;
    };
    // SAFETY: the connection's shared state, live while it calls back;
    // a view of the handler is copied out.
    let handler = unsafe { (*inner.as_ptr()).authorizer.as_deref().cloned() };
    let Some(req) = request(inner) else {
        return SQLITE_OK;
    };
    let names = [first, second, third, fourth].map(bytes_of);
    let served = callbacks(inner).authorize(req, handler.as_ref(), action, names, pending(inner));
    match served {
        Ok(answer) => answer,
        Err(thrown) => {
            throw_out(inner, thrown);
            SQLITE_DENY
        }
    }
}

/// `sqlite3_stmt *`, holding its connection.
pub struct Stmt {
    raw: NonNull<sys::sqlite3_stmt>,
    db: Db,
}

// SAFETY: a statement belongs to the request its connection does; see `Db`.
unsafe impl Send for Stmt {}

impl Drop for Stmt {
    fn drop(&mut self) {
        // SAFETY: a statement prepared on the live connection `db`,
        // finalized once, before the connection's owner goes.
        unsafe { sys::sqlite3_finalize(self.raw.as_ptr()) };
    }
}

impl Stmt {
    fn raw(&self) -> *mut sys::sqlite3_stmt {
        self.raw.as_ptr()
    }

    /// The connection.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// `sqlite3_step`.
    pub fn step(&self, req: &mut Request) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        // SAFETY: a live statement on the live connection.
        self.db.enter(req, |_| unsafe { sys::sqlite3_step(raw) })
    }

    /// `sqlite3_reset`, which may finish an aggregate and so call back.
    pub fn reset(&self, req: &mut Request) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        // SAFETY: as in `step`.
        self.db.enter(req, |_| unsafe { sys::sqlite3_reset(raw) })
    }

    /// `sqlite3_clear_bindings`.
    pub fn clear_bindings(&self) -> i32 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_clear_bindings(self.raw()) }
    }

    /// `sqlite3_bind_parameter_count`.
    pub fn parameter_count(&self) -> i64 {
        // SAFETY: a live statement.
        i64::from(unsafe { sys::sqlite3_bind_parameter_count(self.raw()) })
    }

    /// `sqlite3_sql`, the text the statement was prepared from.
    pub fn sql(&self) -> Vec<u8> {
        // SAFETY: a live statement; the text is copied at once.
        bytes_of(unsafe { sys::sqlite3_sql(self.raw()) }).unwrap_or_default()
    }

    /// `sqlite3_expanded_sql`: the text with the bound values in place.
    pub fn expanded_sql(&self) -> Vec<u8> {
        // SAFETY: a live statement.
        let raw = unsafe { sys::sqlite3_expanded_sql(self.raw()) };
        let text = bytes_of(raw).unwrap_or_default();
        if !raw.is_null() {
            // SAFETY: the text the library allocated for us to free.
            unsafe { sys::sqlite3_free(raw.cast()) };
        }
        text
    }

    /// `sqlite3_data_count`.
    pub fn data_count(&self) -> usize {
        // SAFETY: a live statement.
        usize::try_from(unsafe { sys::sqlite3_data_count(self.raw()) }).unwrap_or(0)
    }

    /// `sqlite3_column_count`.
    pub fn column_count(&self) -> usize {
        // SAFETY: a live statement.
        usize::try_from(unsafe { sys::sqlite3_column_count(self.raw()) }).unwrap_or(0)
    }

    fn column(column: usize) -> c_int {
        c_len(column)
    }

    /// `sqlite3_column_name`, none past the last column.
    pub fn column_name(&self, column: usize) -> Option<Vec<u8>> {
        // SAFETY: a live statement; the name is copied at once.
        bytes_of(unsafe { sys::sqlite3_column_name(self.raw(), Self::column(column)) })
    }

    /// `sqlite3_column_type`.
    pub fn column_type(&self, column: usize) -> i32 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_column_type(self.raw(), Self::column(column)) }
    }

    /// `sqlite3_column_int64`.
    pub fn column_int64(&self, column: usize) -> i64 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_column_int64(self.raw(), Self::column(column)) }
    }

    /// `sqlite3_column_double`.
    pub fn column_double(&self, column: usize) -> f64 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_column_double(self.raw(), Self::column(column)) }
    }

    /// `sqlite3_column_blob` and its length.
    pub fn column_blob(&self, column: usize) -> Vec<u8> {
        // SAFETY: a live statement; the bytes first, then their length.
        let at = unsafe { sys::sqlite3_column_blob(self.raw(), Self::column(column)) };
        // SAFETY: as above.
        let len = unsafe { sys::sqlite3_column_bytes(self.raw(), Self::column(column)) };
        slice(at.cast(), len)
    }

    /// `sqlite3_column_text` and its length.
    pub fn column_text(&self, column: usize) -> Vec<u8> {
        // SAFETY: as in `column_blob`.
        let at = unsafe { sys::sqlite3_column_text(self.raw(), Self::column(column)) };
        // SAFETY: as above.
        let len = unsafe { sys::sqlite3_column_bytes(self.raw(), Self::column(column)) };
        slice(at.cast(), len)
    }

    /// `sqlite3_column_decltype`.
    pub fn column_decltype(&self, column: usize) -> Option<Vec<u8>> {
        // SAFETY: a live statement; the name is copied at once.
        bytes_of(unsafe { sys::sqlite3_column_decltype(self.raw(), Self::column(column)) })
    }

    /// `sqlite3_column_table_name`.
    pub fn column_table_name(&self, column: usize) -> Option<Vec<u8>> {
        // SAFETY: as above.
        bytes_of(unsafe { sys::sqlite3_column_table_name(self.raw(), Self::column(column)) })
    }

    /// `sqlite3_bind_parameter_index`.
    pub fn parameter_index(&self, name: &[u8]) -> i64 {
        let name = c_string(name);
        // SAFETY: a live statement and a NUL-terminated name.
        i64::from(unsafe { sys::sqlite3_bind_parameter_index(self.raw(), name.as_ptr()) })
    }

    fn position(position: i64) -> c_int {
        c_int::try_from(position).unwrap_or(0)
    }

    /// `sqlite3_bind_null`.
    pub fn bind_null(&self, position: i64) -> i32 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_bind_null(self.raw(), Self::position(position)) }
    }

    /// `sqlite3_bind_int64`.
    pub fn bind_int(&self, position: i64, n: i64) -> i32 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_bind_int64(self.raw(), Self::position(position), n) }
    }

    /// `sqlite3_bind_double`.
    pub fn bind_double(&self, position: i64, f: f64) -> i32 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_bind_double(self.raw(), Self::position(position), f) }
    }

    /// `sqlite3_bind_blob`, copied by the library.
    pub fn bind_blob(&self, position: i64, bytes: &[u8]) -> i32 {
        // SAFETY: a live statement; the library copies the bytes.
        unsafe {
            sys::sqlite3_bind_blob(
                self.raw(),
                Self::position(position),
                bytes.as_ptr().cast(),
                c_len(bytes.len()),
                sys::SQLITE_TRANSIENT(),
            )
        }
    }

    /// `sqlite3_bind_text`, copied by the library.
    pub fn bind_text(&self, position: i64, bytes: &[u8]) -> i32 {
        // SAFETY: as in `bind_blob`.
        unsafe {
            sys::sqlite3_bind_text(
                self.raw(),
                Self::position(position),
                bytes.as_ptr().cast(),
                c_len(bytes.len()),
                sys::SQLITE_TRANSIENT(),
            )
        }
    }

    /// `sqlite3_stmt_readonly`.
    pub fn readonly(&self) -> bool {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_stmt_readonly(self.raw()) != 0 }
    }

    /// `sqlite3_stmt_busy`.
    pub fn busy(&self) -> bool {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_stmt_busy(self.raw()) != 0 }
    }

    /// `sqlite3_stmt_isexplain`.
    pub fn explain(&self) -> i64 {
        // SAFETY: a live statement.
        i64::from(unsafe { sys::sqlite3_stmt_isexplain(self.raw()) })
    }

    /// `sqlite3_stmt_explain`.
    pub fn set_explain(&self, mode: i64) -> i32 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_stmt_explain(self.raw(), c_int::try_from(mode).unwrap_or(0)) }
    }
}

/// `sqlite3_blob *`, holding its connection: what `openBlob()` streams.
pub struct Blob {
    raw: NonNull<sys::sqlite3_blob>,
    db: Db,
}

// SAFETY: a blob belongs to the request its connection does; see `Db`.
unsafe impl Send for Blob {}

impl Drop for Blob {
    fn drop(&mut self) {
        // SAFETY: a blob opened on the live connection `db`, closed once.
        unsafe { sys::sqlite3_blob_close(self.raw.as_ptr()) };
        let _ = &self.db;
    }
}

impl Blob {
    /// `sqlite3_blob_bytes`.
    pub fn size(&self) -> usize {
        // SAFETY: a live blob.
        usize::try_from(unsafe { sys::sqlite3_blob_bytes(self.raw.as_ptr()) }).unwrap_or(0)
    }

    /// `sqlite3_blob_read` of `buf.len()` bytes at `offset`.
    pub fn read(&self, buf: &mut [u8], offset: usize) -> bool {
        // SAFETY: a live blob and a buffer of the length given.
        let rc = unsafe {
            sys::sqlite3_blob_read(
                self.raw.as_ptr(),
                buf.as_mut_ptr().cast(),
                c_len(buf.len()),
                c_len(offset),
            )
        };
        rc == SQLITE_OK
    }

    /// `sqlite3_blob_write` of `buf` at `offset`.
    pub fn write(&self, buf: &[u8], offset: usize) -> bool {
        // SAFETY: a live blob and the bytes given.
        let rc = unsafe {
            sys::sqlite3_blob_write(
                self.raw.as_ptr(),
                buf.as_ptr().cast(),
                c_len(buf.len()),
                c_len(offset),
            )
        };
        rc == SQLITE_OK
    }
}
