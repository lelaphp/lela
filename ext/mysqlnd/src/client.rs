//! What `mysqlnd.h` declares `PHPAPI` and `pdo_mysql` and `mysqli` call
//! (design §4.10): a connection, [`Mysqlnd`], with the commands sent over
//! it; a set of rows, [`ResultSet`], with its rows and columns; the
//! options, the columns' descriptions and the statistics' names. Each item is the
//! php function or macro its rustdoc names.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, Value};

use crate::conn::ConnData;
use crate::consts::{CloseType as Close, ServerOption as Server, fetch, status};
use crate::result::Res;
use crate::{Handle, State};

pub use crate::ReverseApi;
pub use crate::conn::ClientOption;
pub use crate::consts::StmtState;
pub use crate::consts::{FieldType, column};
pub use crate::ps::{Stmt, StmtAttr, StmtFetch, StmtStatus};
pub use crate::result::{Field, Row};

/// `mysqlnd_get_client_info`, `PHP_MYSQLND_VERSION`.
pub const CLIENT_INFO: &str = crate::VERSION;

/// `mysqlnd_get_client_version`, `MYSQLND_VERSION_ID`.
pub const CLIENT_VERSION: u64 = 80509;

/// `MYSQLND *`: one connection, a number into the request's table that
/// every owner copies and counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mysqlnd(Handle);

/// `enum mysqlnd_connection_close_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseType {
    Explicit,
    Implicit,
    Disconnected,
}

/// `enum mysqlnd_server_option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerOption {
    MultiStatementsOn,
    MultiStatementsOff,
}

/// What `mysqlnd_connect` is given beside the connection.
#[derive(Debug, Clone, Copy, Default)]
pub struct Connect<'a> {
    pub host: Option<&'a [u8]>,
    pub user: Option<&'a [u8]>,
    pub password: Option<&'a [u8]>,
    pub db: Option<&'a [u8]>,
    pub port: u32,
    pub socket: Option<&'a [u8]>,
    /// The `CLIENT_*` flags asked for.
    pub flags: u32,
}

/// The connection's facts as its accessors read them, one value:
/// `mysqlnd_errno`, `mysqlnd_sqlstate`, `mysqlnd_error`,
/// `mysqlnd_affected_rows`, `mysqlnd_insert_id`, `mysqlnd_warning_count`,
/// `mysqlnd_server_status`, `mysqlnd_field_count`, `mysqlnd_thread_id`,
/// `mysqlnd_get_server_version`, `mysqlnd_get_server_info`,
/// `mysqlnd_get_host_info`, `mysqlnd_get_proto_info`,
/// `mysqlnd_character_set_name`, `mysqlnd_info` and
/// `mysqlnd_more_results`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub errno: u32,
    pub sqlstate: [u8; 5],
    pub error: Vec<u8>,
    pub affected_rows: u64,
    pub insert_id: u64,
    pub warning_count: u32,
    pub server_status: u32,
    pub field_count: u32,
    pub thread_id: u64,
    pub server_version: u64,
    pub server_info: Option<Vec<u8>>,
    pub host_info: Option<Vec<u8>>,
    pub proto_info: u8,
    pub charset: Option<CharsetInfo>,
    pub info: Option<Vec<u8>>,
    pub more_results: bool,
    /// Whether the connection is in the ready state, past `connect`
    /// and before `close`.
    pub ready: bool,
    /// Whether the stream is a unix socket.
    pub unix_socket: bool,
    /// Every client error set since the last reset, mysqli's `$error_list`.
    pub error_list: Vec<(u32, [u8; 5], Vec<u8>)>,
}

/// `MYSQLND_CHARSET`'s public half, what `mysqli_get_charset` reads:
/// `mysqlnd_charset.c`'s row of the connection's character set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharsetInfo {
    pub nr: u32,
    pub name: &'static str,
    pub collation: &'static str,
    pub min_length: u32,
    pub max_length: u32,
    pub comment: &'static str,
}

/// A command php's API sends with nothing to answer but whether it
/// passed: `mysqlnd_select_db`, `mysqlnd_ping`, `mysqlnd_kill`,
/// `mysqlnd_set_character_set`, `mysqlnd_set_server_option`,
/// `mysqlnd_autocommit`, `mysqlnd_commit`, `mysqlnd_rollback`,
/// `mysqlnd_begin_transaction`, `mysqlnd_savepoint`,
/// `mysqlnd_release_savepoint`, `mysqlnd_refresh`,
/// `mysqlnd_dump_debug_info` and `mysqlnd_change_user_with_silent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command<'a> {
    SelectDb(&'a [u8]),
    Ping,
    Kill(u32),
    SetCharset(&'a [u8]),
    SetServerOption(ServerOption),
    Autocommit(bool),
    Commit {
        flags: u32,
        name: Option<&'a [u8]>,
    },
    Rollback {
        flags: u32,
        name: Option<&'a [u8]>,
    },
    Begin {
        mode: u32,
        name: Option<&'a [u8]>,
    },
    Savepoint(Option<&'a [u8]>),
    ReleaseSavepoint(Option<&'a [u8]>),
    Refresh(u8),
    DumpDebugInfo,
    ChangeUser {
        user: Option<&'a [u8]>,
        password: Option<&'a [u8]>,
        db: Option<&'a [u8]>,
        silent: bool,
    },
}

/// An idle persistent connection, what `mysqlnd.h`'s persistent
/// `MYSQLND *` is between requests (#1674): it owns the connection's
/// `ConnData` with its socket, and a pool entry of `pdo_mysql` or mysqli
/// holds it, so dropping the entry closes the connection, its socket
/// closed with no `COM_QUIT` sent. [`Mysqlnd::give_back`] answers one;
/// a request takes it up ([`Mysqlnd::take_up`]) or closes it with a
/// `COM_QUIT` ([`Idle::close`]).
pub struct Idle(Box<ConnData>);

impl Idle {
    /// `mysqlnd_close` with this request: the connection taken up and
    /// closed, its `COM_QUIT` sent and its socket closed, as an owner
    /// closes one past its pool's bound (ADR-0010's amendment of
    /// ADR-0005).
    pub fn close(self, req: &mut Request) -> Result<bool, Thrown> {
        Mysqlnd::take_up(req, self)?.close(req, CloseType::Implicit)
    }
}

impl Mysqlnd {
    /// The connection's number in the table, for the statements of this
    /// crate.
    pub(crate) fn handle(self) -> Handle {
        self.0
    }

    /// `mysqlnd_init`: a connection not yet connected, its structures on
    /// the request's heap unless `persistent`; the `Err` is the memory
    /// limit's fatal.
    pub fn init(req: &mut Request, persistent: bool) -> Result<Mysqlnd, Thrown> {
        Ok(Mysqlnd(Handle::alloc(req, persistent)?))
    }

    /// A persistent connection given back as the request that holds it
    /// ends: taken out of the request's table, this owner gone, with its
    /// socket, for its owner to keep in its own pool, the one list the
    /// connection waits in as php's `EG(persistent_list)` entry holds the
    /// `MYSQLND` itself; `None` where it is no persistent connection that
    /// connected.
    #[must_use]
    pub fn give_back(self, req: &mut Request) -> Option<Idle> {
        self.0.give_back(req).map(Idle)
    }

    /// The connection a request gave back taken up by this one, one owner
    /// counted, over the socket it kept; one whose socket the server has
    /// closed fails every command as one over a dead link does, as the
    /// `mysqlnd_ping` or `COM_CHANGE_USER` its owner sends first finds.
    pub fn take_up(req: &mut Request, idle: Idle) -> Result<Mysqlnd, Thrown> {
        Ok(Mysqlnd(Handle::take_up(req, idle.0)?))
    }

    /// `get_reference`: one more owner, for a statement or a set of rows
    /// read over the connection.
    #[must_use]
    pub fn acquire(self, req: &mut Request) -> Mysqlnd {
        self.0.acquire(req);
        self
    }

    /// `free_reference`: one owner fewer; the last one closes.
    pub fn release(self, req: &mut Request) {
        self.0.release(req);
    }

    /// `mysqlnd_close`: the close sent and counted, this owner gone.
    pub fn close(self, req: &mut Request, kind: CloseType) -> Result<bool, Thrown> {
        let kind = match kind {
            CloseType::Explicit => Close::Explicit,
            CloseType::Implicit => Close::Implicit,
            CloseType::Disconnected => Close::Disconnected,
        };
        let closed = self.call(req, |req, conn| {
            let closed = conn.close(req, kind);
            // `mnd_pefree(conn, ...)` of `mysqlnd_conn::dtor`.
            if let Some(mysqlnd) = conn.mysqlnd.take() {
                mysqlnd.free(req);
            }
            closed
        })?;
        self.0.release(req);
        Ok(closed.unwrap_or(false))
    }

    /// The connection lent for one call; `None` where the number names
    /// none or it is lent already.
    fn call<R>(
        self,
        req: &mut Request,
        f: impl FnOnce(&mut Request, &mut ConnData) -> Result<R, Thrown>,
    ) -> Result<Option<R>, Thrown> {
        self.0.with(req, f).transpose()
    }

    /// `mysqlnd_options`, `mysqlnd_options4` and `mysqlnd_ssl_set`; the
    /// `Err` is the memory limit's fatal.
    pub fn option(self, req: &mut Request, option: ClientOption<'_>) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, conn| conn.set_option(req, option))?
            .unwrap_or(false))
    }

    /// `mysqlnd_connect` on this connection.
    pub fn connect(self, req: &mut Request, to: Connect<'_>) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, conn| {
                conn.connect(
                    req,
                    to.host,
                    to.user,
                    to.password,
                    to.db,
                    to.port,
                    to.socket,
                    to.flags,
                )
            })?
            .unwrap_or(false))
    }

    /// The facts the accessors read.
    pub fn status(self, req: &mut Request) -> Status {
        self.0
            .with(req, |_, conn| Status {
                errno: conn.error.errno,
                sqlstate: conn.sqlstate(),
                error: conn.error.error.clone(),
                affected_rows: conn.upsert.affected_rows,
                insert_id: conn.upsert.last_insert_id,
                warning_count: conn.upsert.warning_count,
                server_status: conn.upsert.server_status,
                field_count: conn.field_count,
                thread_id: conn.thread_id,
                server_version: conn.server_version(),
                server_info: conn.server_version.as_ref().map(|s| s.bytes().to_vec()),
                host_info: conn.host_info.as_ref().map(|s| s.bytes().to_vec()),
                proto_info: conn.protocol_version,
                charset: conn.charset.map(|c| CharsetInfo {
                    nr: c.nr,
                    name: c.name,
                    collation: c.collation,
                    min_length: c.char_minlen,
                    max_length: c.char_maxlen,
                    comment: c.comment,
                }),
                info: conn.last_message.as_ref().map(|m| m.bytes().to_vec()),
                more_results: conn.upsert.server_status & status::MORE_RESULTS_EXISTS != 0,
                ready: conn.state == crate::consts::ConnState::Ready,
                unix_socket: conn.net.unix,
                error_list: conn
                    .error
                    .list()
                    .iter()
                    .map(|e| (e.errno, e.sqlstate, e.error.clone()))
                    .collect(),
            })
            .unwrap_or_default()
    }

    /// `mysqlnd_query`: sent and its first answer read.
    pub fn query(self, req: &mut Request, sql: &[u8]) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, conn| conn.query(req, sql))?
            .unwrap_or(false))
    }

    /// `mysqlnd_send_query`.
    pub fn send_query(self, req: &mut Request, sql: &[u8]) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, conn| conn.send_query(req, sql))?
            .unwrap_or(false))
    }

    /// `mysqlnd_reap_query`.
    pub fn reap_query(self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, conn| conn.reap_query(req))?
            .unwrap_or(false))
    }

    /// `mysqlnd_store_result`.
    pub fn store(self, req: &mut Request) -> Result<Option<ResultSet>, Thrown> {
        let stored = self.call(req, |req, conn| conn.store(req))?.flatten();
        Ok(stored.map(|res| ResultSet {
            res,
            conn: self.acquire(req),
        }))
    }

    /// `mysqlnd_use_result`.
    pub fn use_unbuffered(self, req: &mut Request) -> Result<Option<ResultSet>, Thrown> {
        let used = self
            .call(req, |req, conn| conn.use_unbuffered(req))?
            .flatten();
        Ok(used.map(|res| ResultSet {
            res,
            conn: self.acquire(req),
        }))
    }

    /// `mysqlnd_next_result`.
    pub fn next_rowset(self, req: &mut Request) -> Result<bool, Thrown> {
        Ok(self
            .call(req, |req, conn| conn.next_rowset(req))?
            .unwrap_or(false))
    }

    /// `mysqlnd_real_escape_string`: the escaped bytes, without quotes.
    pub fn escape(self, req: &mut Request, from: &[u8]) -> Vec<u8> {
        self.0
            .with(req, |_, conn| conn.escape_string(from))
            .unwrap_or_default()
    }

    /// `mysqlnd_stat`: the server's statistics text.
    pub fn stat(self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        Ok(self.call(req, |req, conn| conn.statistic(req))?.flatten())
    }

    /// One of the commands [`Command`] names, sent and answered.
    pub fn command(self, req: &mut Request, command: Command<'_>) -> Result<bool, Thrown> {
        let done = self.call(req, |req, conn| match command {
            Command::SelectDb(db) => conn.select_db(req, db),
            Command::Ping => conn.ping(req),
            Command::Kill(pid) => conn.kill(req, pid),
            Command::SetCharset(name) => conn.set_charset(req, name),
            Command::SetServerOption(option) => conn.set_server_option(
                req,
                match option {
                    ServerOption::MultiStatementsOn => Server::MultiStatementsOn,
                    ServerOption::MultiStatementsOff => Server::MultiStatementsOff,
                },
            ),
            Command::Autocommit(on) => conn.autocommit(req, on),
            Command::Commit { flags, name } => conn.tx_commit_or_rollback(req, true, flags, name),
            Command::Rollback { flags, name } => {
                conn.tx_commit_or_rollback(req, false, flags, name)
            }
            Command::Begin { mode, name } => conn.tx_begin(req, mode, name),
            Command::Savepoint(name) => conn.savepoint(req, name, false),
            Command::ReleaseSavepoint(name) => conn.savepoint(req, name, true),
            Command::Refresh(options) => conn.refresh(req, options),
            Command::DumpDebugInfo => conn.debug(req),
            Command::ChangeUser {
                user,
                password,
                db,
                silent,
            } => conn.change_user(req, user, password, db, silent),
        })?;
        Ok(done.unwrap_or(false))
    }

    /// `mysqlnd_get_connection_stats`: each statistic's name and value.
    pub fn stats(self, req: &mut Request) -> Vec<(&'static str, u64)> {
        self.0
            .with(req, |_, conn| named(&conn.stats.values[..]))
            .unwrap_or_default()
    }

    /// `mysqlnd_get_client_stats`: the request's counters by name.
    pub fn client_stats(req: &mut Request) -> Vec<(&'static str, u64)> {
        named(&State::global_stats(req))
    }

    /// `conn->options->local_infile_directory`.
    pub fn local_infile_directory(self, req: &mut Request) -> Option<Vec<u8>> {
        self.0
            .with(req, |_, conn| {
                let dir = conn.options.local_infile_directory.as_ref();
                dir.map(|d| d.bytes().to_vec())
            })
            .flatten()
    }

    /// The connection's error kept across `f` and put back after it, the
    /// list `f` grew freed: what mysqli does where it closes a statement
    /// or turns multi statements off after a failure it has reported
    /// (`MYSQLND_ERROR_INFO error_info = *conn->error_info`, the list's
    /// head cleared; `zend_llist_clean`; `*conn->error_info = error_info`).
    pub fn keeping_error<R>(self, req: &mut Request, f: impl FnOnce(&mut Request) -> R) -> R {
        let kept = self.0.with(req, |_, conn| conn.error.keep());
        let answered = f(req);
        if let Some(kept) = kept {
            let _ = self.0.with(req, |req, conn| conn.error.restore(req, kept));
        }
        answered
    }

    /// `mysqlnd_restart_psession`: a persistent connection taken up by the
    /// next link, counted as reused, its last message forgotten.
    pub fn restart_psession(self, req: &mut Request) {
        let _ = self.0.with(req, |_, conn| {
            conn.stats.inc(crate::stats::Stat::ConnectReused);
            // `conn->last_message.s = NULL`: a message the last request
            // left was its heap's and went with it, so it is forgotten,
            // not freed; `end_psession` and `give_back` leave none.
            conn.last_message = None;
        });
    }

    /// `mysqlnd_end_psession`: what the next link must not see of a
    /// persistent connection given back, its pending rows and its last
    /// message, dropped.
    pub fn end_psession(self, req: &mut Request) -> Result<(), Thrown> {
        self.call(req, |req, conn| {
            if let Some(result) = conn.current_result.take() {
                result.free(req, conn, true)?;
            }
            conn.set_last_message(req, None)
        })
        .map(|_| ())
    }

    /// `mysqlnd_poll`: the connections of `read` and `error` waited on for
    /// `timeout`, each list cut to those whose stream came ready. A
    /// connection of `read` with no query sent is taken out of it first
    /// and answered beside the count, php's `dont_poll`. The count is
    /// `None` for php's `FAIL`: nothing left to wait on, or a `select(2)`
    /// that failed, each warned.
    pub fn poll(
        req: &mut Request,
        mut read: Option<&mut Vec<Mysqlnd>>,
        error: Option<&mut Vec<Mysqlnd>>,
        timeout: std::time::Duration,
    ) -> (Option<usize>, Vec<Mysqlnd>) {
        use crate::consts::ConnState;
        use php_ext::raising::{Level, docref};
        use php_ext::request::Cast;
        let mut dont_poll = Vec::new();
        if let Some(read) = read.as_deref_mut() {
            read.retain(|conn| {
                let state = conn
                    .0
                    .with(req, |_, c| c.state)
                    .unwrap_or(ConnState::Alloced);
                let idle = state <= ConnState::Ready || state == ConnState::QuitSent;
                if idle {
                    dont_poll.push(*conn);
                }
                !idle
            });
        }
        // `mysqlnd_stream_array_to_fd_set`: each connection's fd, cast for
        // select without the buffered-data warning.
        let fds_of = |req: &mut Request, conns: &[Mysqlnd]| -> Vec<(Mysqlnd, std::os::fd::RawFd)> {
            conns
                .iter()
                .filter_map(|conn| {
                    let stream = conn.stream(req)?;
                    let fd = stream.cast(req, Cast::Select, false)?;
                    Some((*conn, fd))
                })
                .collect()
        };
        let read_fds = read.as_deref().map(|r| fds_of(req, r)).unwrap_or_default();
        let error_fds = error.as_deref().map(|e| fds_of(req, e)).unwrap_or_default();
        if read_fds.is_empty() && error_fds.is_empty() {
            let text = if read.is_some() && !dont_poll.is_empty() {
                "All arrays passed are clear"
            } else {
                "No stream arrays were passed"
            };
            docref(req, Level::Warning, text);
            return (None, dont_poll);
        }
        let mut sets = [
            read_fds.iter().map(|(_, fd)| *fd).collect::<Vec<_>>(),
            Vec::new(),
            error_fds.iter().map(|(_, fd)| *fd).collect::<Vec<_>>(),
        ];
        let [r, w, e] = &mut sets;
        let ready = match php_ext::request::select([r, w, e], Some(timeout)) {
            Ok(ready) => ready,
            Err(failed) => {
                let max = read_fds
                    .iter()
                    .chain(&error_fds)
                    .map(|(_, fd)| *fd)
                    .max()
                    .unwrap_or(0);
                let text = format!(
                    "Unable to select [{}]: {} (max_fd={max})",
                    failed.raw_os_error().unwrap_or(0),
                    req.environment()
                        .os_error_text(failed.raw_os_error().unwrap_or(0))
                );
                docref(req, Level::Warning, &text);
                return (None, dont_poll);
            }
        };
        // `mysqlnd_stream_array_from_fd_set`.
        if let Some(read) = read {
            read.retain(|conn| {
                read_fds
                    .iter()
                    .any(|(c, fd)| c == conn && sets[0].contains(fd))
            });
        }
        if let Some(error) = error {
            error.retain(|conn| {
                error_fds
                    .iter()
                    .any(|(c, fd)| c == conn && sets[2].contains(fd))
            });
        }
        (Some(ready), dont_poll)
    }

    /// The connection's stream, for what selects on it.
    pub fn stream(self, req: &mut Request) -> Option<php_ext::request::Stream> {
        self.0.with(req, |_, conn| conn.net.stream()).flatten()
    }
}

/// The counters paired with `mysqlnd_stats_values_names`.
fn named(values: &[u64]) -> Vec<(&'static str, u64)> {
    crate::stats::NAMES
        .iter()
        .copied()
        .zip(values.iter().copied())
        .collect()
}

/// `MYSQLND_RES *` with the connection it reads over.
#[derive(Debug)]
pub struct ResultSet {
    res: Box<Res>,
    conn: Mysqlnd,
}

/// What one fetch came to.
#[derive(Debug)]
pub enum Fetched {
    Row(Row),
    /// Past the last row.
    Done,
    /// php's `FAIL`: the connection's error says why.
    Failed,
}

impl ResultSet {
    /// A set of rows read over `conn`, which it holds one owner of.
    pub(crate) fn of(res: Box<Res>, conn: Mysqlnd) -> ResultSet {
        ResultSet { res, conn }
    }

    /// `result->conn`: the connection the rows are read over.
    #[must_use]
    pub fn connection(&self) -> Mysqlnd {
        self.conn
    }

    /// `mysqlnd_fetch_row`'s values: the next row.
    pub fn fetch(&mut self, req: &mut Request) -> Result<Fetched, Thrown> {
        let res = &mut self.res;
        let got = self
            .conn
            .0
            .with(req, |req, conn| res.fetch_row(req, conn, true))
            .transpose()?;
        Ok(match got {
            Some(Ok(Some(row))) => Fetched::Row(row),
            Some(Ok(None)) => Fetched::Done,
            _ => Fetched::Failed,
        })
    }

    /// `mysqlnd_res::fetch_into`: the next row as an array keyed as
    /// `flags` (`MYSQLND_FETCH_ASSOC`, `_NUM` or both) asks, a numeric
    /// column name an integer key; `null` past the last, `false` where
    /// the fetch failed. The connection's error is emptied first.
    pub fn fetch_into(&mut self, req: &mut Request, flags: u32) -> Result<Owner<Value>, Thrown> {
        // "We clean the error here because in unbuffered mode we could
        // receive a new error and therefore consumers of this method are
        // checking for errors."
        let _ = self.conn.0.with(req, |req, conn| conn.error.reset(req));
        let row = match self.fetch(req)? {
            Fetched::Row(row) => row,
            Fetched::Done => return Ok(Owner::null()),
            Fetched::Failed => return Ok(Owner::bool(false)),
        };
        let mut array = match php_ext::values::PhpArray::new(req) {
            Ok(array) => array,
            Err(thrown) => {
                for value in row {
                    value.give_up(req);
                }
                return Err(thrown);
            }
        };
        let mut values = row.into_iter();
        let mut filled = Ok(());
        for (i, value) in values.by_ref().enumerate() {
            filled = self.fill(req, &mut array, i, flags, value);
            if filled.is_err() {
                break;
            }
        }
        for value in values {
            value.give_up(req);
        }
        match filled {
            Ok(()) => Ok(array.into_value()),
            Err(thrown) => {
                array.give_up(req);
                Err(thrown)
            }
        }
    }

    /// Column `i`'s value written into the fetched array: under its
    /// number, `zend_hash_index_add`, so a column a numeric name put at
    /// the same index first keeps it, and under its name; the row's owner
    /// handed to the last key written, another taken for the first.
    fn fill(
        &self,
        req: &mut Request,
        array: &mut Owner<php_ext::values::PhpArray>,
        i: usize,
        flags: u32,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let field = self.res.fields.get(i);
        let at = php_ext::values::Key::Int(i64::try_from(i).unwrap_or(i64::MAX));
        let numbered = flags & fetch::NUM != 0 && !array.contains(&at);
        if flags & fetch::ASSOC == 0 {
            if numbered {
                return array.set(req, Owner::take(req, at), value);
            }
            value.give_up(req);
            return Ok(());
        }
        if numbered
            && let Err(thrown) = array.set(req, Owner::take(req, at), Owner::take(req, *value))
        {
            value.give_up(req);
            return Err(thrown);
        }
        // `zend_hash_update(row_ht, meta->fields[i].sname, data)`: the
        // field's name, one more owner of it for the row's key.
        let key = match field {
            Some(Field {
                num_key: Some(n), ..
            }) => php_ext::values::Key::Int(*n),
            Some(f) => php_ext::values::Key::Str(*f.name),
            None => {
                value.give_up(req);
                return Ok(());
            }
        };
        let key = Owner::take(req, key);
        array.set(req, key, value)
    }

    /// The columns' descriptions, `mysqlnd_fetch_fields`.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        self.res.fields()
    }

    /// `mysqlnd_fetch_field`: the next column, the cursor moved on.
    pub fn fetch_field(&mut self) -> Option<&Field> {
        self.res.fetch_field()
    }

    /// `mysqlnd_field_seek`, the old position answered.
    pub fn field_seek(&mut self, at: usize) -> usize {
        self.res.field_seek(at)
    }

    /// `mysqlnd_field_tell`.
    #[must_use]
    pub fn field_tell(&self) -> usize {
        self.res.field_tell()
    }

    /// `mysqlnd_num_fields`.
    #[must_use]
    pub fn num_fields(&self) -> u32 {
        self.res.num_fields()
    }

    /// `mysqlnd_num_rows`.
    #[must_use]
    pub fn num_rows(&self) -> u64 {
        self.res.num_rows()
    }

    /// `mysqlnd_data_seek`.
    pub fn data_seek(&mut self, row: u64) -> bool {
        self.res.data_seek(row)
    }

    /// Whether the rows are held whole, `mysqlnd_res_is_unbuffered`'s
    /// opposite.
    #[must_use]
    pub fn is_buffered(&self) -> bool {
        self.res.is_buffered()
    }

    /// Whether unbuffered rows were read to their end.
    #[must_use]
    pub fn eof(&self) -> bool {
        self.res.eof()
    }

    /// `mysqlnd_fetch_lengths`.
    #[must_use]
    pub fn lengths(&self) -> Option<&[u64]> {
        self.res.lengths()
    }

    /// `mysqlnd_free_result`: what is left of unbuffered rows read off the
    /// line, the rows and their pool given back to the request's heap
    /// (where the connection is gone too), the connection's owner
    /// released.
    pub fn free(self, req: &mut Request, implicit: bool) -> Result<(), Thrown> {
        let ResultSet { res, conn } = self;
        let mut res = Some(res);
        let freed = conn
            .0
            .with(req, |req, c| res.take().map(|r| r.free(req, c, implicit)))
            .flatten()
            .transpose();
        if let Some(res) = res {
            res.release(req);
        }
        conn.release(req);
        freed.map(|_| ())
    }
}
