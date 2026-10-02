//! What `dba_open` decodes and what it opens: the handler, `$mode` and
//! the `flock(2)` as types read once from the arguments (`php_dba_open`), and
//! the link, php's `dba_info`, with its streams and its handler's state.

use php_ext::raising::{Level, docref};
use php_ext::request::{Request, Stream};

use crate::cdb::{Maker, Reader};
use crate::flatfile::Flatfile;
use crate::inifile::Inifile;

/// One of the handlers php's own C provides, in `handler[]`'s order. All
/// four take php's locks (`DBA_LOCK_ALL`) and have their database
/// stream opened for them (`DBA_STREAM_OPEN`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Handler {
    Cdb,
    CdbMake,
    Inifile,
    Flatfile,
}

impl Handler {
    /// `handler[]`, as `dba_handlers()` lists it.
    pub(crate) const ALL: [Handler; 4] = [
        Handler::Cdb,
        Handler::CdbMake,
        Handler::Inifile,
        Handler::Flatfile,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Handler::Cdb => "cdb",
            Handler::CdbMake => "cdb_make",
            Handler::Inifile => "inifile",
            Handler::Flatfile => "flatfile",
        }
    }

    /// The handler a name names, `strcasecmp` as php looks it up.
    pub(crate) fn named(name: &[u8]) -> Option<Handler> {
        Handler::ALL
            .into_iter()
            .find(|h| h.name().as_bytes().eq_ignore_ascii_case(name))
    }

    /// `hnd->info(hnd, NULL)`: the library's version.
    pub(crate) const fn version(self) -> &'static str {
        match self {
            Handler::Cdb => crate::cdb::READER_VERSION,
            Handler::CdbMake => crate::cdb::MAKER_VERSION,
            Handler::Inifile => crate::inifile::VERSION,
            Handler::Flatfile => crate::flatfile::VERSION,
        }
    }
}

/// `dba_mode_t`, the first letter of `$mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
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

/// The handler's own state over the database stream, `info->dbf`.
pub(crate) enum Store {
    Flatfile(Flatfile),
    Inifile(Inifile),
    /// A cdb opened to read, under either name.
    CdbReader(Reader),
    /// A cdb opened `n`, made as it is written; `None` once finished.
    CdbMaker(Option<Maker>),
}

impl Store {
    /// `hnd->open`: the handler's state, or the reason it refuses.
    pub(crate) fn open(
        req: &mut Request,
        handler: Handler,
        access: Access,
        file: Stream,
    ) -> Result<Store, &'static str> {
        Ok(match handler {
            Handler::Flatfile => Store::Flatfile(Flatfile::default()),
            Handler::Inifile => Store::Inifile(Inifile::default()),
            Handler::Cdb | Handler::CdbMake => match access {
                Access::Read => Store::CdbReader(Reader::default()),
                Access::Truncate => Store::CdbMaker(Some(Maker::start(req, file))),
                Access::Create | Access::Write => {
                    return Err("Update operations are not supported");
                }
            },
        })
    }
}

/// One open database, php's `dba_info`: the path (the opened file's own
/// where the `flock(2)` is on the database), `$mode`'s access, the
/// handler, the `flock(2)`
/// and the streams, and the handler's state.
pub(crate) struct Link {
    pub(crate) path: Vec<u8>,
    pub(crate) access: Access,
    pub(crate) handler: Handler,
    pub(crate) persistent: bool,
    pub(crate) lock: Held,
    /// `info->fp`, the database stream.
    pub(crate) file: Stream,
    /// `info->lock.fp` where it is a stream of its own, a `.lck` file.
    pub(crate) lock_file: Option<Stream>,
    pub(crate) store: Store,
}

impl Link {
    /// `dba_close_info`: the handler closed, a cdb being made finished,
    /// then the streams freed.
    pub(crate) fn close(self, req: &mut Request) {
        if let Store::CdbMaker(Some(maker)) = self.store {
            // `dba_close_cdb` ignores what `cdb_make_finish` answers.
            let _ = maker.finish(req, self.file);
        }
        self.file.free(req);
        if let Some(lock) = self.lock_file {
            lock.free(req);
        }
    }
}

/// `DBA_UPDATE_FUNC`'s warning where the handler could not write.
pub(crate) fn not_possible(req: &mut Request) {
    docref(req, Level::Warning, "Operation not possible");
}
