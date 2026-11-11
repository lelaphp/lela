//! What `dba_open` decodes and what it opens: the handler, `$mode` and
//! the `flock(2)` as types read once from the arguments (`php_dba_open`), and
//! the link, php's `dba_info`, with its streams and its handler's state.

use std::os::fd::OwnedFd;

use php_ext::abi::EngineState;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{FdStream, OwnedStream as _, Request, Stream, Streams as _};
use php_ext::values::Owner;

use crate::cdb::{Maker, Reader};
use crate::flatfile::Flatfile;
use crate::inifile::Inifile;

/// One of the handlers, in `handler[]`'s order: ndbm over the platform's
/// library where the C library has one, as the oracle's build finds it on
/// macOS, then the four php's own C provides. All take php's locks
/// (`DBA_LOCK_ALL`); all but ndbm have their database stream opened for
/// them (`DBA_STREAM_OPEN`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, EngineState)]
pub(crate) enum Handler {
    #[cfg(target_os = "macos")]
    Ndbm,
    Cdb,
    CdbMake,
    Inifile,
    Flatfile,
}

impl Handler {
    /// `handler[]`, as `dba_handlers()` lists it.
    pub(crate) const ALL: &[Handler] = &[
        #[cfg(target_os = "macos")]
        Handler::Ndbm,
        Handler::Cdb,
        Handler::CdbMake,
        Handler::Inifile,
        Handler::Flatfile,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Handler::Ndbm => "ndbm",
            Handler::Cdb => "cdb",
            Handler::CdbMake => "cdb_make",
            Handler::Inifile => "inifile",
            Handler::Flatfile => "flatfile",
        }
    }

    /// The handler a name names, `strcasecmp` as php looks it up.
    pub(crate) fn named(name: &[u8]) -> Option<Handler> {
        Handler::ALL
            .iter()
            .copied()
            .find(|h| h.name().as_bytes().eq_ignore_ascii_case(name))
    }

    /// `DBA_STREAM_OPEN`: whether `php_dba_open` opens the database
    /// stream for the handler.
    pub(crate) const fn streams(self) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            Handler::Ndbm => false,
            Handler::Cdb | Handler::CdbMake | Handler::Inifile | Handler::Flatfile => true,
        }
    }

    /// `hnd->info(hnd, NULL)`: the library's version.
    pub(crate) const fn version(self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Handler::Ndbm => "NDBM",
            Handler::Cdb => crate::cdb::READER_VERSION,
            Handler::CdbMake => crate::cdb::MAKER_VERSION,
            Handler::Inifile => crate::inifile::VERSION,
            Handler::Flatfile => crate::flatfile::VERSION,
        }
    }
}

/// `dba_mode_t`, the first letter of `$mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, EngineState)]
pub(crate) enum Access {
    /// `r`.
    Read,
    /// `w`.
    Write,
    /// `c`.
    Create,
    /// `n`.
    Truncate,
}

impl Access {
    /// `DBA_WRITE_CHECK`: what a modification is allowed on.
    pub(crate) const fn writes(self) -> bool {
        !matches!(self, Access::Read)
    }

    /// `gmode`, the `open(2)` flags `dba_open_ndbm` hands the library.
    #[cfg(target_os = "macos")]
    const fn open_flags(self) -> i32 {
        match self {
            Access::Read => libc::O_RDONLY,
            Access::Write => libc::O_RDWR,
            Access::Create => libc::O_RDWR | libc::O_CREAT,
            Access::Truncate => libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
        }
    }
}

/// Where the `flock(2)` is taken, the second letter of `$mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Locking {
    /// `d`, or no second letter: on the database file itself.
    Database,
    /// `l`: on `<path>.lck` beside it.
    LockFile,
    /// `-`: none.
    Ignored,
    /// `t` as the second letter: a test of a `flock(2)` nothing asked for.
    Unasked,
}

/// The `flock(2)` a link holds, `lock.mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, EngineState)]
pub(crate) enum Held {
    #[default]
    None,
    Shared,
    Exclusive,
}

impl Held {
    /// `LOCK_SH` and `LOCK_EX`.
    pub(crate) const fn operation(self) -> i64 {
        match self {
            Held::None => 0,
            Held::Shared => 1,
            Held::Exclusive => 2,
        }
    }

    /// Whether two locks on one file block each other, as the check
    /// before `php_dba_open` locks has it.
    pub(crate) const fn conflicts(self, other: Held) -> bool {
        matches!(
            (self, other),
            (Held::Exclusive, Held::Shared | Held::Exclusive) | (Held::Shared, Held::Exclusive)
        )
    }
}

/// `$mode` decoded: `[rwnc][dl-t]?t?`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Mode {
    pub(crate) access: Access,
    pub(crate) locking: Locking,
    /// `t`: the `flock(2)` taken without blocking.
    pub(crate) test: bool,
}

/// Why `$mode` was refused, each `zend_argument_value_error`'s tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BadMode {
    TooLong,
    Third,
    Second,
    First,
    IgnoredAndTested,
}

impl BadMode {
    pub(crate) const fn text(self) -> &'static str {
        match self {
            BadMode::TooLong => "must be at most 3 characters",
            BadMode::Third => "third character must be \"t\"",
            BadMode::Second => "second character must be one of \"d\", \"l\", \"-\", or \"t\"",
            BadMode::First => "first character must be one of \"r\", \"w\", \"c\", or \"n\"",
            BadMode::IgnoredAndTested => {
                "cannot combine mode \"-\" (no lock) and \"t\" (test lock)"
            }
        }
    }
}

impl Mode {
    /// The letters in php's order: the length, the third, the second,
    /// then the first, and the test against the `flock(2)` last.
    pub(crate) fn decode(mode: &[u8]) -> Result<Mode, BadMode> {
        if mode.len() > 3 {
            return Err(BadMode::TooLong);
        }
        let mut test = false;
        if mode.len() == 3 {
            if mode[2] != b't' {
                return Err(BadMode::Third);
            }
            test = true;
        }
        let locking = match mode.get(1) {
            None | Some(b'd') => Locking::Database,
            Some(b't') => {
                test = true;
                Locking::Unasked
            }
            Some(b'-') => Locking::Ignored,
            Some(b'l') => Locking::LockFile,
            Some(_) => return Err(BadMode::Second),
        };
        let access = match mode.first() {
            Some(b'r') => Access::Read,
            Some(b'w') => Access::Write,
            Some(b'c') => Access::Create,
            Some(b'n') => Access::Truncate,
            _ => return Err(BadMode::First),
        };
        if test && locking == Locking::Ignored {
            return Err(BadMode::IgnoredAndTested);
        }
        Ok(Mode {
            access,
            locking,
            test,
        })
    }

    /// `lock_mode`: shared to read, exclusive otherwise, where a `flock(2)` is
    /// taken at all.
    pub(crate) const fn lock(self) -> Held {
        match (self.locking, self.access) {
            (Locking::Ignored | Locking::Unasked, _) => Held::None,
            (_, Access::Read) => Held::Shared,
            _ => Held::Exclusive,
        }
    }

    /// `file_mode` for the database stream. Where php opens `a+b` and
    /// then clears `O_APPEND` on the fd so a handler can write
    /// inside the file, the stream is opened `c+b`, the same fd without
    /// `O_APPEND`.
    pub(crate) const fn file_mode(self) -> &'static [u8] {
        match self.access {
            Access::Read => b"r",
            Access::Write => b"r+b",
            Access::Create => b"c+b",
            Access::Truncate => b"w+b",
        }
    }

    /// `lock_file_mode` for the `.lck` file's own stream, or the
    /// database's `file_mode` where the `flock(2)` is on the database;
    /// `w+b` for the `.lck` file a create makes.
    pub(crate) const fn lock_file_mode(self) -> &'static [u8] {
        match (self.access, self.locking) {
            (Access::Create, Locking::LockFile) => b"w+b",
            _ => self.file_mode(),
        }
    }
}

/// The handler's own state, `info->dbf`, with the database stream of a
/// handler that has one opened for it, `info->fp`: a stream in a request,
/// its owner, or its fd while the engine's pool keeps a persistent link,
/// engine state as [`Kept`] holds it.
#[derive(EngineState)]
pub(crate) enum Store<F = Owner<Stream>> {
    /// The library's open database; `None` where `dbm_open` refused, which
    /// `dba_open_ndbm` answers as success all the same.
    #[cfg(target_os = "macos")]
    Ndbm(Option<crate::ffi::Ndbm>),
    Flatfile(Flatfile, F),
    Inifile(Inifile, F),
    /// A cdb opened to read, under either name.
    CdbReader(Reader, F),
    /// A cdb opened `n`, made as it is written; `None` once finished.
    CdbMaker(Option<Maker>, F),
}

impl<F> Store<F> {
    /// The handler's state over what `f` makes of the database's stream or
    /// fd.
    fn map<G, E>(self, f: impl FnOnce(F) -> Result<G, E>) -> Result<Store<G>, E> {
        Ok(match self {
            #[cfg(target_os = "macos")]
            Store::Ndbm(db) => Store::Ndbm(db),
            Store::Flatfile(walk, file) => Store::Flatfile(walk, f(file)?),
            Store::Inifile(ini, file) => Store::Inifile(ini, f(file)?),
            Store::CdbReader(reader, file) => Store::CdbReader(reader, f(file)?),
            Store::CdbMaker(maker, file) => Store::CdbMaker(maker, f(file)?),
        })
    }

    /// The handler's state apart from the database's stream or fd.
    fn split(self) -> (Store<()>, Option<F>) {
        let mut file = None;
        let Ok(store) = self.map(|f| {
            file = Some(f);
            Ok::<(), std::convert::Infallible>(())
        });
        (store, file)
    }
}

impl Store {
    /// `hnd->open`: the handler's state over the database stream, or
    /// over `path` itself for ndbm, which opens it in the library with
    /// `permission`; or the reason it refuses, with the stream handed
    /// back.
    pub(crate) fn open(
        req: &mut Request,
        handler: Handler,
        access: Access,
        file: Option<Owner<Stream>>,
        path: &[u8],
        permission: i64,
    ) -> Result<Store, (&'static str, Option<Owner<Stream>>)> {
        let _ = (path, permission);
        Ok(match (handler, file) {
            #[cfg(target_os = "macos")]
            (Handler::Ndbm, _) => {
                // `int filemode` handed on as `mode_t`.
                let mode = u16::try_from(permission & 0xffff).unwrap_or_default();
                Store::Ndbm(crate::ffi::Ndbm::open(path, access.open_flags(), mode))
            }
            (_, None) => return Err(("", None)),
            (Handler::Flatfile, Some(file)) => Store::Flatfile(Flatfile::default(), file),
            (Handler::Inifile, Some(file)) => Store::Inifile(Inifile::default(), file),
            (Handler::Cdb | Handler::CdbMake, Some(file)) => match access {
                Access::Read => Store::CdbReader(Reader::default(), file),
                Access::Truncate => Store::CdbMaker(Some(Maker::start(req, *file)), file),
                Access::Create | Access::Write => {
                    return Err(("Update operations are not supported", Some(file)));
                }
            },
        })
    }

    /// `info->fp`, where the handler has one.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(
            clippy::unnecessary_wraps,
            reason = "every handler but ndbm has one, and ndbm is macOS's"
        )
    )]
    pub(crate) fn stream(&self) -> Option<Stream> {
        match self {
            #[cfg(target_os = "macos")]
            Store::Ndbm(_) => None,
            Store::Flatfile(_, file)
            | Store::Inifile(_, file)
            | Store::CdbReader(_, file)
            | Store::CdbMaker(_, file) => Some(**file),
        }
    }
}

/// One open database, php's `dba_info`: the path (the opened file's own
/// where the `flock(2)` is on the database), `$mode`'s access, the
/// handler, the `flock(2)` and its stream, and the handler's state with
/// the database stream.
pub(crate) struct Link {
    pub(crate) path: Vec<u8>,
    pub(crate) access: Access,
    pub(crate) handler: Handler,
    pub(crate) persistent: bool,
    pub(crate) lock: Held,
    /// `info->lock.fp` where it is a stream of its own, a `.lck` file.
    pub(crate) lock_file: Option<Owner<Stream>>,
    pub(crate) store: Store,
}

impl Link {
    /// `dba_close_info`: the handler closed, a cdb being made finished
    /// and an ndbm database closed, then the streams freed.
    pub(crate) fn close(self, req: &mut Request) {
        let (store, file) = self.store.split();
        match (store, &file) {
            // `dba_close_cdb` ignores what `cdb_make_finish` answers.
            (Store::CdbMaker(Some(maker), ()), Some(file)) => {
                let _ = maker.finish(req, **file);
            }
            // `dba_close_ndbm`'s `dbm_close`, before the lock is let go.
            (store, _) => drop(store),
        }
        if let Some(file) = file {
            file.free(req);
        }
        if let Some(lock) = self.lock_file {
            lock.free(req);
        }
    }
}

/// A `dba_popen` link between requests, what the engine's pool keeps of
/// php's persistent `dba_info`: the database's and the `.lck` file's fds,
/// open so the `flock(2)` they hold stands, the path, the access, the
/// handler and its walk state. The streams are made again over the fds by
/// the request that takes it up. Engine state, as the pool keeps it.
#[derive(EngineState)]
pub(crate) struct Kept {
    path: Vec<u8>,
    access: Access,
    handler: Handler,
    lock: Held,
    lock_file: Option<KeptFd>,
    store: Store<KeptFd>,
}

/// A stream's fd between requests and the position the stream held,
/// what php's persistent stream keeps in memory.
#[derive(EngineState)]
pub(crate) struct KeptFd {
    fd: OwnedFd,
    position: i64,
}

impl KeptFd {
    /// `stream`'s fd duplicated, written out first; `None` where none
    /// could be had.
    fn of(req: &mut Request, stream: Stream) -> Option<KeptFd> {
        let position = stream.tell(req);
        let fd = stream.dup_fd(req)?;
        Some(KeptFd { fd, position })
    }

    /// The stream made again over the fd at the position it held, as php's
    /// persistent stream is found again.
    fn reopen(self, req: &mut Request, mode: &[u8]) -> Result<Owner<Stream>, Thrown> {
        let position = self.position;
        req.stream_from_fd(self.fd, FdStream::Kept { mode, position })
    }
}

impl Link {
    /// The persistent link given back at the request's end: its streams'
    /// fds duplicated, written out first, and the streams freed; `None`
    /// where a fd could not be had, the link closed.
    pub(crate) fn keep(self, req: &mut Request) -> Option<Kept> {
        let file_fd = self.store.stream().map(|file| KeptFd::of(req, file));
        let lock_fd = self.lock_file.as_deref().map(|lock| KeptFd::of(req, *lock));
        if matches!(file_fd, Some(None)) || matches!(lock_fd, Some(None)) {
            self.close(req);
            return None;
        }
        let (store, file) = self.store.split();
        for stream in file.into_iter().chain(self.lock_file) {
            stream.free(req);
        }
        let store = store.map(|()| file_fd.flatten().ok_or(())).ok()?;
        Some(Kept {
            path: self.path,
            access: self.access,
            handler: self.handler,
            lock: self.lock,
            lock_file: lock_fd.flatten(),
            store,
        })
    }
}

impl Kept {
    /// The link taken up by this request, its streams made over the fds
    /// as `php_stream_fopen_from_fd` makes them, each at the position it
    /// held. The `Err` is the memory limit, met registering a stream.
    pub(crate) fn take_up(self, req: &mut Request) -> Result<Link, Thrown> {
        let mode = Mode {
            access: self.access,
            locking: Locking::Database,
            test: false,
        }
        .file_mode();
        let store = self.store.map(|fd| fd.reopen(req, mode))?;
        let lock_file = match self.lock_file.map(|fd| fd.reopen(req, b"rb")) {
            Some(Ok(lock)) => Some(lock),
            Some(Err(thrown)) => {
                if let (_, Some(file)) = store.split() {
                    file.free(req);
                }
                return Err(thrown);
            }
            None => None,
        };
        Ok(Link {
            path: self.path,
            access: self.access,
            handler: self.handler,
            persistent: true,
            lock: self.lock,
            lock_file,
            store,
        })
    }
}

/// `DBA_UPDATE_FUNC`'s warning where the handler could not write.
pub(crate) fn not_possible(req: &mut Request) {
    docref(req, Level::Warning, "Operation not possible");
}
