//! The binding of the platform's libsqlite3, the one the oracle links,
//! through `libsqlite3-sys`'s prebuilt declarations (design §7.2, #1510,
//! `unsafe.homes`' `ext/*/src/ffi/`): the calls
//! `sqlite_driver.c`, `sqlite_statement.c` and `pdo_sqlite.c` make, and
//! the calls `ext/sqlite3`'s `sqlite3.c` makes over the same library,
//! which reaches it through these safe types and keeps no binding of its
//! own (design §4.10, §7.2), and nothing beside them.
//!
//! **Owners.** A connection is a [`Conn`], which the request keeps for
//! the object that opened it (ADR-0013): the object stores its one owner,
//! `request::Foreign<Conn>`, and reaches it by its name, a [`Db`], as a
//! statement or a registration on it does. Nothing beside the owner
//! counts: a statement or a blob keeps no share, because the library
//! itself keeps a connection closed under one until the last is
//! finalized (`sqlite3_close_v2`, what `pdo_sqlite`'s closer calls), and
//! refuses an explicit `sqlite3_close` while one stands ([`Db::close`]).
//!
//! **Callbacks.** A user function, an aggregate, a collation and the
//! authorizer are called by the library from inside `sqlite3_step`,
//! `sqlite3_exec`, `sqlite3_prepare_v2` or `sqlite3_reset`. Each of
//! those calls is made through `Db::call`, which holds the connection
//! for the call (`Request::holding`), so PHP code a callback runs that
//! gives the connection's owner up leaves its close to the call's end;
//! leaves the request where a trampoline finds it, in the connection's
//! lend, for the length of the call; and hands back what a callback
//! threw, which the library cannot carry. A trampoline outside such a
//! call reaches no request and answers as php answers a callback that
//! was not served, as one does whose function was taken off while the
//! library kept it ([`Registration::unregister`]). The lend is a
//! `*mut Request` rather than a hold of the request's own: the library
//! hands a trampoline only the pointer it was given, and the request it
//! must reach is the one the call lent. What a callback does in PHP is
//! the extension's, a [`Callbacks`] value the connection serves with,
//! since `pdo_sqlite`'s and `sqlite3`'s callbacks are two algorithms over
//! the one library.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};

use libsqlite3_sys as sys;
use php_ext::abi::Callable;
use php_ext::raising::Thrown;
use php_ext::request::{Foreign, ForeignId, Request};
use php_ext::values::{Owner, Value};

pub(crate) use sys::SQLITE_MISUSE;
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

unsafe extern "C" {
    /// `sqlite3_close_v2`, which `libsqlite3-sys` leaves out of its
    /// prebuilt declarations for the libraries older than 3.7.14 it
    /// still builds against; the keg's library has it, as php's
    /// `HAVE_SQLITE3_CLOSE_V2` finds.
    fn sqlite3_close_v2(db: *mut sys::sqlite3) -> c_int;
}

/// A pointer the library holds beside the one Rust value that frees
/// what it points at: `sqlite3 *`, `sqlite3_stmt *`, `sqlite3_blob *`,
/// or the data a callback is registered with.
struct Raw<T>(NonNull<T>);

// SAFETY: the library is built `THREADSAFE=1` (the compile options
// `version`'s witness pins) and `Conn::open` opens every connection in
// its serialized mode, where the library's objects may be used from any
// thread; the data of this crate's that the library holds is `Send`
// itself, and the library reads it only from inside a call into it,
// on the thread that makes the call.
unsafe impl<T: Send> Send for Raw<T> {}

impl<T> Clone for Raw<T> {
    fn clone(&self) -> Raw<T> {
        *self
    }
}

impl<T> Copy for Raw<T> {}

impl<T> Raw<T> {
    fn get(self) -> *mut T {
        self.0.as_ptr()
    }
}

/// What the library's callbacks reach of their connection, which the
/// library keeps as the connection's client data and frees as the
/// connection closes, after the last callback it can make
/// (`sqlite3_set_clientdata`), so a callback finds it however late a
/// statement outliving its connection's owner is finalized.
struct Lend {
    /// The request of the call in progress, where a callback reaches it;
    /// null between calls.
    req: *mut Request,
    /// What a callback threw during the call in progress, the first one.
    pending: Option<Thrown>,
    /// `authorizer_fcc`, what `setAuthorizer()` set, a view of the
    /// callable its object keeps.
    authorizer: Option<Callable>,
    /// What the callbacks do in PHP.
    callbacks: &'static dyn Callbacks,
    /// The data of each function or collation taken off the connection
    /// while the library refused to let go of it, a statement on it being
    /// active ([`Registration::unregister`]): the library may still call
    /// back with it, so it is freed with the lend, after the last
    /// callback the connection can make.
    kept: Vec<Kind>,
}

impl Drop for Lend {
    fn drop(&mut self) {
        for kind in self.kept.drain(..) {
            kind.free();
        }
    }
}

// SAFETY: `req` is null but for the length of one call into the library,
// which `Db::call` makes on the thread that holds the request it sets
// there, and the library reads the lend only from inside such a call;
// the other fields are `Send`.
unsafe impl Send for Lend {}

/// The name the library keeps a connection's [`Lend`] under.
const LEND: &CStr = c"php";

/// `sqlite3_set_clientdata`'s destructor of a [`Lend`].
unsafe extern "C" fn free_lend(lend: *mut c_void) {
    // SAFETY: the box `Conn::open` handed the library, which calls this
    // once, as the connection closes or where it could not keep it.
    drop(unsafe { Box::from_raw(lend.cast::<Lend>()) });
}

/// The text `sqlite3_close` sets for a connection a statement, a blob or
/// a call in progress still holds.
const BUSY: &[u8] = b"unable to close due to unfinalized statements or unfinished backups";

/// `sqlite3 *`, the one owner of the connection, which the request keeps
/// for the object that opened it ([`Db::open`], ADR-0013), with what
/// `pdo_sqlite` records beside it. A statement or a blob opened on it
/// keeps no share: the library keeps a connection closed under one open
/// until the last is finalized (`sqlite3_close_v2`), and a call into the
/// library that can serve PHP code holds the connection (`Db::call`).
pub struct Conn {
    raw: Raw<sys::sqlite3>,
    /// The library's, live while the connection is.
    lend: Raw<Lend>,
    /// `H->einfo`, what `pdo_sqlite`'s error function last recorded, kept
    /// with the connection because php's statements write the
    /// connection's through `stmt->dbh`; `ext/sqlite3` leaves it empty.
    einfo: crate::sqlite::Einfo,
}

impl Drop for Conn {
    /// `sqlite3_close_v2`, as `pdo_sqlite`'s closer closes: at once, or
    /// once the last statement or blob still open on it is finalized, a
    /// statement outliving its connection in the cycle collector's order.
    fn drop(&mut self) {
        // SAFETY: the connection's one owner, closed once.
        unsafe { sqlite3_close_v2(self.raw.get()) };
    }
}

impl Conn {
    /// `sqlite3_open_v2(filename, &db, flags, NULL)`, the library
    /// serializing every call on it (`SQLITE_OPEN_NOMUTEX` dropped), with
    /// the authorizer installed as `pdo_sqlite_handle_factory` and
    /// `SQLite3::open` install it,
    /// serving its callbacks with `callbacks`: the code and the text of
    /// the error where it failed, `sqlite3_errstr(rc)` where the library
    /// made no connection to ask.
    fn open(
        filename: &[u8],
        flags: i32,
        callbacks: &'static dyn Callbacks,
    ) -> Result<Conn, (i32, Vec<u8>)> {
        let name = c_string(filename);
        let flags = flags & !sys::SQLITE_OPEN_NOMUTEX;
        let mut raw = ptr::null_mut();
        // SAFETY: the name is NUL-terminated and outlives the call, the
        // out-pointer a live local, the VFS the default.
        let rc = unsafe { sys::sqlite3_open_v2(name.as_ptr(), &raw mut raw, flags, ptr::null()) };
        let Some(raw) = NonNull::new(raw) else {
            return Err((rc, errstr(rc)));
        };
        if rc != SQLITE_OK {
            let failed = (errcode(raw.as_ptr()), errmsg(raw.as_ptr()));
            // SAFETY: the connection the failed open made, closed once.
            unsafe { sys::sqlite3_close(raw.as_ptr()) };
            return Err(failed);
        }
        let lend = NonNull::from(Box::leak(Box::new(Lend {
            req: ptr::null_mut(),
            pending: None,
            authorizer: None,
            callbacks,
            kept: Vec::new(),
        })));
        // SAFETY: a live connection, a static name, and the lend with the
        // destructor that frees it, which the library calls where it
        // cannot keep it.
        let kept = unsafe {
            sys::sqlite3_set_clientdata(
                raw.as_ptr(),
                LEND.as_ptr(),
                lend.as_ptr().cast(),
                Some(free_lend),
            )
        };
        if kept != SQLITE_OK {
            // SAFETY: the connection made above, which nothing else holds,
            // closed once.
            unsafe { sys::sqlite3_close(raw.as_ptr()) };
            return Err((kept, errstr(kept)));
        }
        // SAFETY: the connection is live and the argument its lend, which
        // lives as long as the connection does.
        unsafe { sys::sqlite3_set_authorizer(raw.as_ptr(), Some(authorize), lend.as_ptr().cast()) };
        Ok(Conn {
            raw: Raw(raw),
            lend: Raw(lend),
            einfo: crate::sqlite::Einfo::default(),
        })
    }

    /// `sqlite3_close`: `Err` with the connection still open, the code
    /// and the library's text, where a statement or a blob still holds
    /// it.
    fn close(self) -> Result<(), (Conn, i32, Vec<u8>)> {
        let mut conn = ManuallyDrop::new(self);
        // SAFETY: the connection's one owner; closed once where this
        // answers `SQLITE_OK`, whereupon its `Drop` does not run.
        let rc = unsafe { sys::sqlite3_close(conn.raw.get()) };
        if rc == SQLITE_OK {
            drop(std::mem::take(&mut conn.einfo));
            return Ok(());
        }
        let raw = conn.raw.get();
        Err((ManuallyDrop::into_inner(conn), errcode(raw), errmsg(raw)))
    }

    /// The lend, through the connection's one owner.
    fn lend(&mut self) -> &mut Lend {
        // SAFETY: the library's lend of a live connection, borrowed
        // through its owner's `&mut`; a callback reads it only for the
        // span of its own field accesses, none across a call into PHP.
        unsafe { &mut *self.lend.get() }
    }

    /// Whether a call into the library on the connection is in progress
    /// ([`Db::call`]): the request lent to the callbacks.
    fn serving(&mut self) -> bool {
        !self.lend().req.is_null()
    }
}

/// `sqlite3_errstr`, the library's static text for a code.
fn errstr(rc: i32) -> Vec<u8> {
    // SAFETY: the library's static text for a code.
    bytes_of(unsafe { sys::sqlite3_errstr(rc) }).unwrap_or_default()
}

/// `sqlite3_errcode` of a live connection.
fn errcode(raw: *mut sys::sqlite3) -> i32 {
    // SAFETY: a live connection, or one a statement keeps.
    unsafe { sys::sqlite3_errcode(raw) }
}

/// `sqlite3_errmsg` of a live connection.
fn errmsg(raw: *mut sys::sqlite3) -> Vec<u8> {
    // SAFETY: as in `errcode`; the message is copied at once.
    bytes_of(unsafe { sys::sqlite3_errmsg(raw) }).unwrap_or_default()
}

/// The name of a connection the request keeps, copied freely: what a
/// payload, a statement and a registration reach the connection by, as
/// long as its owner keeps it. Each method answers as the library
/// answers a misused connection (`SQLITE_MISUSE`) where it is gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Db(ForeignId<Conn>);

impl Db {
    /// The connection `owner` keeps.
    #[must_use]
    pub fn of(owner: &Foreign<Conn>) -> Db {
        Db(owner.id())
    }

    /// [`Conn::open`], the connection kept by the request for the
    /// object that stores the answer.
    pub fn open(
        req: &mut Request,
        filename: &[u8],
        flags: i32,
        callbacks: &'static dyn Callbacks,
    ) -> Result<Foreign<Conn>, (i32, Vec<u8>)> {
        Conn::open(filename, flags, callbacks).map(|conn| req.adopt(conn))
    }

    /// The connection, where its owner keeps it.
    fn conn(self, req: &mut Request) -> Option<&mut Conn> {
        req.foreign(self.0)
    }

    /// `sqlite3 *`, where the connection stands.
    fn raw(self, req: &mut Request) -> Option<*mut sys::sqlite3> {
        self.conn(req).map(|conn| conn.raw.get())
    }

    /// `f` served on the connection, held for the call
    /// ([`Request::holding`]) and its request lent to the library's
    /// callbacks, and what a callback threw meanwhile handed back beside
    /// `f`'s answer; `None` where the connection is gone. PHP code a
    /// callback runs may give the connection's owner up: the hold keeps
    /// the connection, its lend and what is registered on it to the end
    /// of the call.
    fn call<R>(
        self,
        req: &mut Request,
        f: impl FnOnce(*mut sys::sqlite3) -> R,
    ) -> Option<(R, Option<Thrown>)> {
        let held = req.holding(self.0, |req, held| {
            let conn = held.get(req)?;
            let (raw, lend) = (conn.raw.get(), conn.lend.get());
            // SAFETY: the lend of a connection the hold keeps open; the
            // write is the only access.
            let outer = unsafe { std::mem::replace(&mut (*lend).req, ptr::from_mut(req)) };
            let answered = f(raw);
            // SAFETY: as above; a callback's own call restored what it
            // found before this read.
            let thrown = unsafe { (*lend).pending.take() };
            // SAFETY: as above.
            unsafe { (*lend).req = outer };
            Some((answered, thrown))
        });
        held.flatten()
    }

    /// `sqlite3_close` of the connection `owner` keeps, given up where
    /// the library closes it, and each of `registrations` made on it
    /// closed with it ([`Registration`]), its handlers kept for the
    /// caller to give up when it will, as `SQLite3::close` leaves php's
    /// `funcs` and `collations` on the object. `Err` with the connection
    /// still open, its owner, the code and the library's text, where a
    /// statement or a blob still holds it, or a call into the library is
    /// in progress on it, which php's `sqlite3_close` from inside a
    /// callback answers `SQLITE_BUSY` for the statement the call runs.
    pub fn close<'r>(
        req: &mut Request,
        owner: Foreign<Conn>,
        registrations: impl IntoIterator<Item = &'r mut Registration>,
    ) -> Result<(), Box<Unclosed>> {
        let db = Db::of(&owner);
        if db.conn(req).is_some_and(Conn::serving) {
            return Err(Box::new(Unclosed {
                owner,
                code: SQLITE_BUSY,
                message: BUSY.to_vec(),
            }));
        }
        let registrations = registrations
            .into_iter()
            .filter(move |registration| registration.db == db);
        let Some(conn) = req.disown(owner) else {
            // No call holds it, or `serving` would have answered; were one
            // to, it closes as the call ends and may call back with what
            // is registered on it until then, so the data is left to it.
            registrations.for_each(|registration| registration.close(Freed::Never));
            return Ok(());
        };
        match conn.close() {
            Ok(()) => {
                registrations.for_each(|registration| registration.close(Freed::Now));
                Ok(())
            }
            Err((conn, code, message)) => {
                // The request keeps it again under a new name, which the
                // registrations on it follow.
                let owner = req.adopt(conn);
                let kept = Db::of(&owner);
                registrations.for_each(|registration| registration.db = kept);
                Err(Box::new(Unclosed {
                    owner,
                    code,
                    message,
                }))
            }
        }
    }

    /// The connection `owner` keeps given up, closed now or as the last
    /// statement on it is finalized (`sqlite3_close_v2`), as
    /// `sqlite_handle_closer` and `php_sqlite3_object_free_storage`
    /// close it once each function and collation is taken off it
    /// ([`Registration::unregister`]).
    pub fn release(req: &mut Request, owner: Foreign<Conn>) {
        drop(req.disown(owner));
    }

    /// `sqlite3_errcode`.
    pub fn errcode(self, req: &mut Request) -> i32 {
        self.raw(req).map_or(SQLITE_MISUSE, errcode)
    }

    /// `sqlite3_errmsg`.
    pub fn errmsg(self, req: &mut Request) -> Vec<u8> {
        self.raw(req).map_or_else(|| errstr(SQLITE_MISUSE), errmsg)
    }

    /// `sqlite3_extended_errcode`.
    pub fn extended_errcode(self, req: &mut Request) -> i32 {
        let Some(raw) = self.raw(req) else {
            return SQLITE_MISUSE;
        };
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_extended_errcode(raw) }
    }

    /// The connection's `H->einfo` replaced by `einfo`, the one it held
    /// handed back, as `Cell::replace` hands it; `einfo` itself where the
    /// connection is gone.
    pub(crate) fn replace_einfo(
        self,
        req: &mut Request,
        einfo: crate::sqlite::Einfo,
    ) -> crate::sqlite::Einfo {
        match self.conn(req) {
            Some(conn) => std::mem::replace(&mut conn.einfo, einfo),
            None => einfo,
        }
    }

    /// The callbacks served from here on: `SQLite3::enableExceptions`
    /// changes what its authorizer raises.
    pub fn serve_with(self, req: &mut Request, callbacks: &'static dyn Callbacks) {
        if let Some(conn) = self.conn(req) {
            conn.lend().callbacks = callbacks;
        }
    }

    /// `authorizer_fcc`: the handler the authorizer calls, a view of the
    /// callable its object keeps and gives up only after it is replaced
    /// here.
    pub fn set_authorizer(self, req: &mut Request, handler: Option<&Callable>) {
        if let Some(conn) = self.conn(req) {
            conn.lend().authorizer = handler.cloned();
        }
    }

    /// `sqlite3_db_config(db, SQLITE_DBCONFIG_DEFENSIVE, 1, NULL)`,
    /// `sqlite3.defensive`.
    pub fn defensive(self, req: &mut Request) {
        let Some(raw) = self.raw(req) else {
            return;
        };
        // SAFETY: a live connection; the option takes an `int` and an
        // `int *` it may leave null.
        unsafe {
            sys::sqlite3_db_config(
                raw,
                sys::SQLITE_DBCONFIG_DEFENSIVE,
                1,
                ptr::null_mut::<c_int>(),
            )
        };
    }

    /// `sqlite3_backup_init`, `sqlite3_backup_step(-1)` until it stops
    /// answering `SQLITE_OK`, and `sqlite3_backup_finish`: the finish's
    /// code, or this connection's error code where the backup could not
    /// start.
    pub fn backup(self, req: &mut Request, to: Db, from_name: &[u8], to_name: &[u8]) -> i32 {
        let (Some(from), Some(to)) = (self.raw(req), to.raw(req)) else {
            return SQLITE_MISUSE;
        };
        let (from_name, to_name) = (c_string(from_name), c_string(to_name));
        // SAFETY: two live connections and two NUL-terminated names.
        let backup =
            unsafe { sys::sqlite3_backup_init(to, to_name.as_ptr(), from, from_name.as_ptr()) };
        if backup.is_null() {
            return errcode(from);
        }
        // SAFETY: the backup made above, live until its finish.
        while unsafe { sys::sqlite3_backup_step(backup, -1) } == SQLITE_OK {}
        // SAFETY: as above, finished once.
        unsafe { sys::sqlite3_backup_finish(backup) }
    }

    /// `sqlite3_changes`.
    pub fn changes(self, req: &mut Request) -> i64 {
        self.raw(req).map_or(0, changes)
    }

    /// `sqlite3_last_insert_rowid`.
    pub fn last_insert_rowid(self, req: &mut Request) -> i64 {
        let Some(raw) = self.raw(req) else {
            return 0;
        };
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_last_insert_rowid(raw) }
    }

    /// `sqlite3_get_autocommit`.
    pub fn autocommit(self, req: &mut Request) -> bool {
        let Some(raw) = self.raw(req) else {
            return true;
        };
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_get_autocommit(raw) != 0 }
    }

    /// `sqlite3_busy_timeout`.
    pub fn busy_timeout(self, req: &mut Request, ms: i64) {
        let Some(raw) = self.raw(req) else {
            return;
        };
        let ms = c_int::try_from(ms).unwrap_or(if ms < 0 { c_int::MIN } else { c_int::MAX });
        // SAFETY: a live connection.
        unsafe { sys::sqlite3_busy_timeout(raw, ms) };
    }

    /// `sqlite3_extended_result_codes`.
    pub fn extended_codes(self, req: &mut Request, on: i64) {
        let Some(raw) = self.raw(req) else {
            return;
        };
        // php hands the library its `zend_long` as an `int`.
        #[allow(clippy::cast_possible_truncation)]
        // SAFETY: a live connection.
        unsafe {
            sys::sqlite3_extended_result_codes(raw, on as c_int)
        };
    }

    /// `sqlite3_exec(db, sql, NULL, NULL, NULL)`.
    pub fn exec(self, req: &mut Request, sql: &[u8]) -> (i32, Option<Thrown>) {
        let sql = c_string(sql);
        let called = self.call(req, |raw| {
            // SAFETY: a live connection and a NUL-terminated statement that
            // outlives the call; no row callback.
            unsafe { sys::sqlite3_exec(raw, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) }
        });
        called.unwrap_or((SQLITE_MISUSE, None))
    }

    /// `sqlite3_prepare_v2(db, sql, len, &stmt, &tail)`: the statement,
    /// none for a text with no statement in it, or the library's code.
    pub fn prepare(
        self,
        req: &mut Request,
        sql: &[u8],
    ) -> (Result<Option<Stmt>, i32>, Option<Thrown>) {
        let mut raw = ptr::null_mut();
        let called = self.call(req, |db| {
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
        let Some((rc, thrown)) = called else {
            return (Err(SQLITE_MISUSE), None);
        };
        let prepared = match NonNull::new(raw) {
            Some(raw) if rc == SQLITE_OK => Ok(Some(Stmt {
                raw: Raw(raw),
                db: self,
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
    /// registration the connection's object keeps until it closes.
    pub fn create_function(
        self,
        req: &mut Request,
        name: &[u8],
        argc: i32,
        flags: i32,
        function: Function,
    ) -> Result<Registration, (Box<Function>, i32)> {
        let Some(conn) = self.conn(req) else {
            return Err((Box::new(function), SQLITE_MISUSE));
        };
        let (raw, lend) = (conn.raw.get(), conn.lend);
        let aggregate = matches!(function, Function::Aggregate { .. });
        let registered = NonNull::from(Box::leak(Box::new(Registered {
            lend,
            function: None,
        })));
        let c_name = c_string(name);
        let (func, step, fin): (Scalar, Scalar, Final) = if aggregate {
            (None, Some(step), Some(finalize))
        } else {
            (Some(scalar), None, None)
        };
        // SAFETY: a live connection, a NUL-terminated name, and as the
        // function's data the registration, which lives until the library
        // lets go of it: `unregister` removing the function, or the
        // connection closing.
        let rc = unsafe {
            sys::sqlite3_create_function_v2(
                raw,
                c_name.as_ptr(),
                argc,
                flags | sys::SQLITE_UTF8,
                registered.as_ptr().cast(),
                func,
                step,
                fin,
                None,
            )
        };
        let kind = Kind::Function(Raw(registered));
        if rc != SQLITE_OK {
            kind.free();
            return Err((Box::new(function), rc));
        }
        // SAFETY: the box made above, which the library holds but calls
        // back with only from inside a later call; the one access.
        unsafe { (*registered.as_ptr()).function = Some(function) };
        Ok(Registration {
            db: self,
            name: c_name,
            argc,
            state: State::Registered(kind),
        })
    }

    /// `sqlite3_create_collation(db, name, SQLITE_UTF8, collation, callback)`.
    pub fn create_collation(
        self,
        req: &mut Request,
        name: &[u8],
        handler: Handler,
    ) -> Result<Registration, (Handler, i32)> {
        let Some(conn) = self.conn(req) else {
            return Err((handler, SQLITE_MISUSE));
        };
        let (raw, lend) = (conn.raw.get(), conn.lend);
        let registered = NonNull::from(Box::leak(Box::new(Collation {
            lend,
            handler: None,
        })));
        let c_name = c_string(name);
        // SAFETY: as in `create_function`.
        let rc = unsafe {
            sys::sqlite3_create_collation_v2(
                raw,
                c_name.as_ptr(),
                sys::SQLITE_UTF8,
                registered.as_ptr().cast(),
                Some(collate),
                None,
            )
        };
        let kind = Kind::Collation(Raw(registered));
        if rc != SQLITE_OK {
            kind.free();
            return Err((handler, rc));
        }
        // SAFETY: as in `create_function`.
        unsafe { (*registered.as_ptr()).handler = Some(handler) };
        Ok(Registration {
            db: self,
            name: c_name,
            argc: 0,
            state: State::Registered(kind),
        })
    }

    /// `sqlite3_load_extension` with the C API's loading enabled for the
    /// call alone, `pdosqlite` `loadExtension`'s dance: the library's
    /// error text where it failed.
    pub fn load_extension(self, req: &mut Request, path: &[u8]) -> Result<(), Vec<u8>> {
        let Some(raw) = self.raw(req) else {
            return Err(errstr(SQLITE_MISUSE));
        };
        let path = c_string(path);
        load_config(raw, 1);
        let mut error: *mut c_char = ptr::null_mut();
        // SAFETY: a live connection, a NUL-terminated path, the default
        // entry point and a live out-pointer for the message.
        let rc =
            unsafe { sys::sqlite3_load_extension(raw, path.as_ptr(), ptr::null(), &raw mut error) };
        let answered = if rc == SQLITE_OK {
            Ok(())
        } else {
            Err(bytes_of(error).unwrap_or_default())
        };
        if !error.is_null() {
            // SAFETY: the message the library allocated for us to free.
            unsafe { sys::sqlite3_free(error.cast()) };
        }
        load_config(raw, 0);
        answered
    }

    /// `sqlite3_blob_open`: the blob, or the library's error text.
    pub fn blob_open(
        self,
        req: &mut Request,
        dbname: &[u8],
        table: &[u8],
        column: &[u8],
        row: i64,
        writable: bool,
    ) -> Result<Blob, Vec<u8>> {
        let Some(db) = self.raw(req) else {
            return Err(errstr(SQLITE_MISUSE));
        };
        let (dbname, table, column) = (c_string(dbname), c_string(table), c_string(column));
        let mut raw = ptr::null_mut();
        // SAFETY: a live connection, three NUL-terminated names and a
        // live out-pointer.
        let rc = unsafe {
            sys::sqlite3_blob_open(
                db,
                dbname.as_ptr(),
                table.as_ptr(),
                column.as_ptr(),
                row,
                c_int::from(writable),
                &raw mut raw,
            )
        };
        match NonNull::new(raw) {
            Some(raw) if rc == SQLITE_OK => Ok(Blob { raw: Raw(raw) }),
            _ => Err(errmsg(db)),
        }
    }
}

/// `sqlite3_changes` of a live connection.
fn changes(raw: *mut sys::sqlite3) -> i64 {
    // SAFETY: a live connection, or one a statement keeps.
    i64::from(unsafe { sys::sqlite3_changes(raw) })
}

/// `sqlite3_db_config(db, SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, on, NULL)`.
fn load_config(raw: *mut sys::sqlite3, on: c_int) {
    // SAFETY: a live connection; the option takes an `int` and an
    // `int *` it may leave null.
    unsafe {
        sys::sqlite3_db_config(
            raw,
            sys::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION,
            on,
            ptr::null_mut::<c_int>(),
        )
    };
}

/// What [`Db::close`] hands back where the connection stays open: its
/// owner, and the code and text of why.
pub struct Unclosed {
    pub owner: Foreign<Conn>,
    pub code: i32,
    pub message: Vec<u8>,
}

/// The type of a scalar or step callback in a registration.
type Scalar =
    Option<unsafe extern "C" fn(*mut sys::sqlite3_context, c_int, *mut *mut sys::sqlite3_value)>;
/// The type of a final callback.
type Final = Option<unsafe extern "C" fn(*mut sys::sqlite3_context)>;

/// A function's data as the library holds it: its connection's lend and
/// what it serves, none once it was taken off ([`Registration`]).
struct Registered {
    lend: Raw<Lend>,
    function: Option<Function>,
}

/// A collation's data as the library holds it.
struct Collation {
    lend: Raw<Lend>,
    handler: Option<Handler>,
}

/// The data a function or a collation is registered with, which the
/// library holds and calls back with. Dropped, it frees nothing.
enum Kind {
    Function(Raw<Registered>),
    Collation(Raw<Collation>),
}

impl Kind {
    /// The handlers taken out of the data, which stays, serving none.
    fn take_handlers(&self) -> Vec<Handler> {
        match *self {
            Kind::Function(registered) => {
                // SAFETY: a registration's live data; no borrow of it spans
                // this write, as a trampoline copies out what it reads
                // before it runs PHP code.
                let function = unsafe { (*registered.get()).function.take() };
                function.map_or_else(Vec::new, Function::into_handlers)
            }
            Kind::Collation(registered) => {
                // SAFETY: as above.
                let handler = unsafe { (*registered.get()).handler.take() };
                handler.into_iter().collect()
            }
        }
    }

    /// The data freed, once the library holds it no more: the function
    /// or the collation taken off, the connection closed, or the data
    /// never taken.
    fn free(self) {
        match self {
            Kind::Function(registered) => {
                // SAFETY: the box `create_function` made, freed once.
                drop(unsafe { Box::from_raw(registered.get()) });
            }
            Kind::Collation(registered) => {
                // SAFETY: the box `create_collation` made, freed once.
                drop(unsafe { Box::from_raw(registered.get()) });
            }
        }
    }
}

/// One function or collation registered on a connection, php's
/// `pdo_sqlite_func`, `pdo_sqlite_collation`, `php_sqlite3_func` or
/// `php_sqlite3_collation`, which the connection's object keeps until it
/// is freed ([`Registration::unregister`]), across a close of the
/// connection ([`Db::close`]) too. Its handlers are given up there
/// whatever the library answers, as php gives them up; the data the
/// library holds is freed only once the library can no longer call back
/// with it.
pub struct Registration {
    db: Db,
    name: CString,
    argc: i32,
    state: State,
}

/// Where a [`Registration`] stands.
enum State {
    /// On its connection, which holds the data and calls back with it.
    Registered(Kind),
    /// Its connection closed by [`Db::close`]: the handlers it served,
    /// its data freed or left to the library.
    Closed(Vec<Handler>),
}

/// Whether [`Registration::close`] frees the data: the library closed
/// the connection, or closes it as the call holding it ends and may call
/// back with the data until then.
#[derive(Clone, Copy)]
enum Freed {
    Now,
    Never,
}

impl Registration {
    /// Taken off the library, `sqlite3_create_function` or
    /// `sqlite3_create_collation` with no callback, and the handlers it
    /// held handed back for the caller to give up whatever the library
    /// answered, as `pdo_sqlite_cleanup_callbacks` and
    /// `php_sqlite3_object_free_storage` give them up. The data is freed
    /// where the library let go of it. Where it refused, a statement on
    /// the connection being active (`SQLITE_BUSY`), it keeps the function
    /// and calls back with the data, which now serves none, so the
    /// connection's lend keeps the data until the connection closes.
    /// Where the connection is gone already, the library may hold the
    /// data until the last statement on it is finalized, which nothing
    /// here can see, so the data is left to it. One whose connection
    /// [`Db::close`] closed hands back the handlers it kept.
    pub fn unregister(self, req: &mut Request) -> Vec<Handler> {
        let kind = match self.state {
            State::Registered(kind) => kind,
            State::Closed(handlers) => return handlers,
        };
        let handlers = kind.take_handlers();
        let Some(conn) = self.db.conn(req) else {
            return handlers;
        };
        let raw = conn.raw.get();
        let rc = match kind {
            Kind::Function(_) => {
                // SAFETY: the registration's live connection; no callback
                // and no data, which deletes the function.
                unsafe {
                    sys::sqlite3_create_function_v2(
                        raw,
                        self.name.as_ptr(),
                        self.argc,
                        sys::SQLITE_UTF8,
                        ptr::null_mut(),
                        None,
                        None,
                        None,
                        None,
                    )
                }
            }
            Kind::Collation(_) => {
                // SAFETY: as above, for a collation.
                unsafe {
                    sys::sqlite3_create_collation_v2(
                        raw,
                        self.name.as_ptr(),
                        sys::SQLITE_UTF8,
                        ptr::null_mut(),
                        None,
                        None,
                    )
                }
            }
        };
        if rc == SQLITE_OK {
            kind.free();
        } else {
            conn.lend().kept.push(kind);
        }
        handlers
    }

    /// Its connection closed: its handlers kept, its data freed where
    /// `freed` says the library let go of it; one closed already keeps
    /// the handlers it holds.
    fn close(&mut self, freed: Freed) {
        let State::Registered(kind) = &self.state else {
            return;
        };
        let handlers = kind.take_handlers();
        let State::Registered(kind) = std::mem::replace(&mut self.state, State::Closed(handlers))
        else {
            return;
        };
        if let Freed::Now = freed {
            kind.free();
        }
    }

    /// What its handlers count, for the cycle collector.
    pub fn visit(&self, visit: &mut dyn FnMut(Value)) {
        match &self.state {
            State::Registered(Kind::Function(registered)) => {
                // SAFETY: a registration's data, live while the library
                // holds it, and read only.
                let registered = unsafe { &*registered.get() };
                if let Some(function) = &registered.function {
                    function.visit(visit);
                }
            }
            State::Registered(Kind::Collation(registered)) => {
                // SAFETY: as above.
                let registered = unsafe { &*registered.get() };
                if let Some(handler) = &registered.handler {
                    handler.visit(visit);
                }
            }
            State::Closed(handlers) => {
                for handler in handlers {
                    handler.visit(visit);
                }
            }
        }
    }
}

/// The request of the call in progress on `lend`'s connection, `None`
/// outside one.
fn request<'a>(lend: Raw<Lend>) -> Option<&'a mut Request> {
    // SAFETY: the lend of a connection a registration or the authorizer
    // names, live while the library can call back.
    let req = unsafe { (*lend.get()).req };
    // SAFETY: set by `Db::call` from the `&mut Request` it was given,
    // which that call does not touch until the library returns, so this
    // is the one live mutable borrow of it.
    unsafe { req.as_mut() }
}

/// What `lend`'s connection serves its callbacks with.
fn callbacks(lend: Raw<Lend>) -> &'static dyn Callbacks {
    // SAFETY: as in `request`; the reference is copied out.
    unsafe { (*lend.get()).callbacks }
}

/// Whether a callback already threw during the call in progress.
fn pending(lend: Raw<Lend>) -> bool {
    // SAFETY: as in `request`.
    unsafe { (*lend.get()).pending.is_some() }
}

/// What a callback threw, kept for the call to hand back; the first
/// one stays.
fn throw_out(lend: Raw<Lend>, thrown: Thrown) {
    // SAFETY: as in `request`.
    let slot = unsafe { &mut (*lend.get()).pending };
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

/// What a call of a function finds in the data it carries: its
/// connection's lend, and a view of the handler `pick` names, none once
/// the function was taken off ([`Registration::unregister`]). The view
/// is copied out, so no borrow of the data spans the PHP code the handler
/// runs, which may take the function off.
fn served(
    ctx: *mut sys::sqlite3_context,
    pick: impl FnOnce(&Function) -> Option<&Handler>,
) -> Option<(Raw<Lend>, Option<Callable>)> {
    // SAFETY: the context of a call the library is making.
    let data = unsafe { sys::sqlite3_user_data(ctx) };
    // SAFETY: the data `create_function` gave the library, live while the
    // library holds it, read for the copy alone.
    let registered = unsafe { data.cast::<Registered>().as_ref() }?;
    let handler = registered.function.as_ref().and_then(pick);
    let view = handler.map(|handler| Callable::clone(handler));
    Some((registered.lend, view))
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

/// `php_sqlite3_func_callback`: `failed to invoke callback`, php's
/// answer for a callback not served, where the function was taken off or
/// no call lends a request.
unsafe extern "C" fn scalar(
    ctx: *mut sys::sqlite3_context,
    count: c_int,
    values: *mut *mut sys::sqlite3_value,
) {
    let mut call = Invocation {
        ctx,
        args: arguments(count, values),
    };
    let found = served(ctx, |function| match function {
        Function::Scalar(handler) => Some(handler),
        Function::Aggregate { .. } => None,
    });
    let Some((lend, Some(handler))) = found else {
        return call.failed();
    };
    let Some(req) = request(lend).filter(|_| !pending(lend)) else {
        return call.failed();
    };
    if let Err(thrown) = callbacks(lend).scalar(req, &handler, &mut call) {
        throw_out(lend, thrown);
    }
}

/// `php_sqlite3_func_step_callback`: nothing, as php's step not served
/// answers, where the function was taken off or no call lends a request.
unsafe extern "C" fn step(
    ctx: *mut sys::sqlite3_context,
    count: c_int,
    values: *mut *mut sys::sqlite3_value,
) {
    let mut call = Invocation {
        ctx,
        args: arguments(count, values),
    };
    let found = served(ctx, |function| match function {
        Function::Aggregate { step, .. } => Some(step),
        Function::Scalar(_) => None,
    });
    let Some((lend, Some(handler))) = found else {
        return;
    };
    let Some(req) = request(lend).filter(|_| !pending(lend)) else {
        return;
    };
    let args = call.arguments();
    let Some(state) = call.aggregate() else {
        return;
    };
    if let Err(thrown) = callbacks(lend).step(req, &handler, state, args) {
        throw_out(lend, thrown);
    }
}

/// `php_sqlite3_func_final_callback`: the group's state given up and
/// `failed to invoke callback` where a callback already threw or the
/// function was taken off.
unsafe extern "C" fn finalize(ctx: *mut sys::sqlite3_context) {
    let mut call = Invocation { ctx, args: &[] };
    let found = served(ctx, |function| match function {
        Function::Aggregate { finalize, .. } => Some(finalize),
        Function::Scalar(_) => None,
    });
    let Some((lend, handler)) = found else {
        return;
    };
    let state = call.aggregate().and_then(Option::take);
    let Some(req) = request(lend) else {
        return;
    };
    let Some(handler) = handler.filter(|_| !pending(lend)) else {
        drop_aggregate(req, state);
        return call.failed();
    };
    if let Err(thrown) = callbacks(lend).finalize(req, &handler, state, &mut call) {
        throw_out(lend, thrown);
    }
}

/// `php_sqlite3_collation_callback`: `0`, php's answer for a callback not
/// served, where the collation was taken off or no call lends a request.
unsafe extern "C" fn collate(
    data: *mut c_void,
    left_len: c_int,
    left: *const c_void,
    right_len: c_int,
    right: *const c_void,
) -> c_int {
    // SAFETY: the data `create_collation` gave the library, live while
    // the library holds it, read for the copy alone, so no borrow of it
    // spans the PHP code the handler runs.
    let Some(collation) = (unsafe { data.cast::<Collation>().as_ref() }) else {
        return 0;
    };
    let (lend, handler) = (collation.lend, collation.handler.as_ref());
    let Some(handler) = handler.map(|handler| Callable::clone(handler)) else {
        return 0;
    };
    let Some(req) = request(lend).filter(|_| !pending(lend)) else {
        return 0;
    };
    let left = slice(left.cast(), left_len);
    let right = slice(right.cast(), right_len);
    let (order, thrown) = callbacks(lend).collate(req, &handler, &left, &right);
    if let Some(thrown) = thrown {
        throw_out(lend, thrown);
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
    let Some(lend) = NonNull::new(data.cast::<Lend>()).map(Raw) else {
        return SQLITE_OK;
    };
    // SAFETY: the connection's lend, live while it calls back; a view
    // of the handler is copied out.
    let handler = unsafe { (*lend.get()).authorizer.clone() };
    let Some(req) = request(lend) else {
        return SQLITE_OK;
    };
    let names = [first, second, third, fourth].map(bytes_of);
    let served = callbacks(lend).authorize(req, handler.as_ref(), action, names, pending(lend));
    match served {
        Ok(answer) => answer,
        Err(thrown) => {
            throw_out(lend, thrown);
            SQLITE_DENY
        }
    }
}

/// `sqlite3_stmt *`, and the name of its connection, which a call that
/// can serve PHP code holds.
pub struct Stmt {
    raw: Raw<sys::sqlite3_stmt>,
    db: Db,
}

impl Drop for Stmt {
    /// `sqlite3_finalize`: the library keeps the connection until its
    /// last statement is finalized, so a statement outliving its
    /// connection's owner finalizes against it.
    fn drop(&mut self) {
        // SAFETY: a statement the library made, finalized once.
        unsafe { sys::sqlite3_finalize(self.raw()) };
    }
}

impl Stmt {
    fn raw(&self) -> *mut sys::sqlite3_stmt {
        self.raw.get()
    }

    /// The connection's name.
    pub fn db(&self) -> Db {
        self.db
    }

    /// `sqlite3_db_handle`, which the library keeps while a statement on
    /// it stands.
    fn connection(&self) -> *mut sys::sqlite3 {
        // SAFETY: a live statement.
        unsafe { sys::sqlite3_db_handle(self.raw()) }
    }

    /// `sqlite3_errcode` of its connection.
    pub fn errcode(&self) -> i32 {
        errcode(self.connection())
    }

    /// `sqlite3_errmsg` of its connection.
    pub fn errmsg(&self) -> Vec<u8> {
        errmsg(self.connection())
    }

    /// `sqlite3_changes` of its connection.
    pub fn changes(&self) -> i64 {
        changes(self.connection())
    }

    /// `sqlite3_step`, the connection held for the call, which can serve
    /// PHP code; `SQLITE_MISUSE` where the connection is gone.
    pub fn step(&self, req: &mut Request) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        // SAFETY: a live statement on the connection the call holds.
        let called = self.db.call(req, |_| unsafe { sys::sqlite3_step(raw) });
        called.unwrap_or((SQLITE_MISUSE, None))
    }

    /// `sqlite3_reset`, which may finish an aggregate and so call back.
    pub fn reset(&self, req: &mut Request) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        // SAFETY: as in `step`.
        let called = self.db.call(req, |_| unsafe { sys::sqlite3_reset(raw) });
        called.unwrap_or((SQLITE_MISUSE, None))
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

/// `sqlite3_blob *`: what `openBlob()` streams. The library keeps its
/// connection until the blob is closed, and no call on it serves PHP
/// code, so it names no connection.
pub struct Blob {
    raw: Raw<sys::sqlite3_blob>,
}

impl Drop for Blob {
    fn drop(&mut self) {
        // SAFETY: a blob the library opened, closed once.
        unsafe { sys::sqlite3_blob_close(self.raw.get()) };
    }
}

impl Blob {
    /// `sqlite3_blob_bytes`.
    pub fn size(&self) -> usize {
        // SAFETY: a live blob.
        usize::try_from(unsafe { sys::sqlite3_blob_bytes(self.raw.get()) }).unwrap_or(0)
    }

    /// `sqlite3_blob_read` of `buf.len()` bytes at `offset`.
    pub fn read(&self, buf: &mut [u8], offset: usize) -> bool {
        // SAFETY: a live blob and a buffer of the length given.
        let rc = unsafe {
            sys::sqlite3_blob_read(
                self.raw.get(),
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
                self.raw.get(),
                buf.as_ptr().cast(),
                c_len(buf.len()),
                c_len(offset),
            )
        };
        rc == SQLITE_OK
    }
}
