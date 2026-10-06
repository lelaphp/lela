//! `mysql_driver.c` behind PDO's connector face: the factory, the
//! connection's methods, and `_pdo_mysql_error`, which every failure goes
//! through.
//!
//! **Where `pdo_mysql_db_handle` lives.** php's statement keeps a pointer
//! to its connection's `pdo_mysql_db_handle`, `S->H`, and reads its
//! settings at each `executer` call. Here it is a row of this
//! extension's request state,
//! which the connection and its statements name by number, so a setting
//! changed after a prepare reaches the statement's next `executer` as php's
//! does.

use php_ext::request::Request;
use php_ext::values::{PhpString, Value};
use php_ext_mysqlnd::client::{ClientOption, Command, Connect, Idle, Mysqlnd, Stmt as Ps};
use php_ext_pdo::connector::{
    BindType, Connection, Connector, Failure, OnError, Placeholders, Prepared, Scanner, Source,
    SqlError, SqlState, bool_attribute, long_attribute, option_long, option_string,
    parse_data_source, rewrite_named,
};

use crate::Mysql;
use crate::statement::Stmt;

/// PDO's attributes this connector reads.
pub(crate) mod attr {
    pub(crate) const AUTOCOMMIT: i64 = 0;
    pub(crate) const TIMEOUT: i64 = 2;
    pub(crate) const SERVER_VERSION: i64 = 4;
    pub(crate) const CLIENT_VERSION: i64 = 5;
    pub(crate) const SERVER_INFO: i64 = 6;
    pub(crate) const CONNECTION_STATUS: i64 = 7;
    pub(crate) const FETCH_TABLE_NAMES: i64 = 14;
    pub(crate) const STRINGIFY_FETCHES: i64 = 17;
    pub(crate) const MAX_COLUMN_LEN: i64 = 18;
    pub(crate) const EMULATE_PREPARES: i64 = 20;
    pub(crate) const DEFAULT_STR_PARAM: i64 = 21;
}

/// `PDO_PARAM_STR_NATL`, `PDO_PARAM_STR_CHAR` and `PDO_PARAM_LOB`'s bits.
const STR_NATL: i64 = 0x4000_0000;
const STR_CHAR: i64 = 0x2000_0000;
const LOB: i32 = 3;

/// `CLIENT_FOUND_ROWS`, `CLIENT_IGNORE_SPACE`, `CLIENT_MULTI_STATEMENTS`,
/// `CLIENT_MULTI_RESULTS` and the two TLS-check flags.
mod flag {
    pub(super) const FOUND_ROWS: u32 = 2;
    pub(super) const IGNORE_SPACE: u32 = 256;
    pub(super) const MULTI_STATEMENTS: u32 = 1 << 16;
    pub(super) const MULTI_RESULTS: u32 = 1 << 17;
    pub(super) const SSL_DONT_VERIFY_SERVER_CERT: u32 = 64;
    pub(super) const SSL_VERIFY_SERVER_CERT: u32 = 1 << 30;
}

/// `SERVER_STATUS_IN_TRANS`.
const IN_TRANS: u32 = 1;

/// `pdo_mysql_driver`.
pub(crate) struct Driver;

pub(crate) static MYSQL: Driver = Driver;

/// `pdo_mysql_db_handle`, less the server's error, which PDO keeps.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // php's bit fields, one per name
pub(crate) struct H {
    pub(crate) server: Mysqlnd,
    pub(crate) assume_national_character_set_strings: bool,
    pub(crate) buffered: bool,
    pub(crate) emulate_prepare: bool,
    pub(crate) fetch_table_names: bool,
    pub(crate) local_infile: bool,
    /// `dbh->auto_commit`, which only the connector reads and writes.
    pub(crate) auto_commit: bool,
    /// `dbh->alloc_own_columns`, set by the first native prepare.
    pub(crate) own_columns: bool,
}

/// The request's handles, numbered by the connection that opened each.
#[derive(Default)]
pub(crate) struct State {
    handles: Vec<Option<H>>,
}

/// The `pdo_mysql_db_handle` numbered `id`.
pub(crate) fn handle(req: &mut Request, id: usize) -> Option<H> {
    req.state::<State>().handles.get(id).copied().flatten()
}

/// The `pdo_mysql_db_handle` numbered `id` changed by `f`.
fn update(req: &mut Request, id: usize, f: impl FnOnce(&mut H)) {
    if let Some(Some(h)) = req.state::<State>().handles.get_mut(id) {
        f(h);
    }
}

/// `_pdo_mysql_error`: the error of the statement's `MYSQLND_STMT` where
/// it has one, else of the connection, as PDO keeps it; error 2014 worded
/// as php words it by whether results are pending, 2057 as php words it.
pub(crate) fn error_of(req: &mut Request, server: Mysqlnd, ps: Option<&Ps>) -> Failure {
    let status = server.status(req);
    let (errno, sqlstate, error) = match ps {
        Some(ps) => {
            let s = ps.status(req);
            (s.errno, s.sqlstate, s.error)
        }
        None => (status.errno, status.sqlstate, status.error),
    };
    if errno == 0 {
        return Failure::error(SqlError {
            state: SqlState::NONE,
            code: 0,
            message: Vec::new(),
        });
    }
    let message = match errno {
        2014 if status.more_results => b"Cannot execute queries while there are pending result sets. Consider unsetting the previous PDOStatement or calling PDOStatement::closeCursor()".to_vec(),
        2014 => b"Cannot execute queries while other unbuffered queries are active.  Consider using PDOStatement::fetchAll().  Alternatively, if your code is only ever going to run against mysql, you may enable query buffering by setting the Pdo\\Mysql::ATTR_USE_BUFFERED_QUERY attribute.".to_vec(),
        2057 => b"A stored procedure returning result sets of different size was called. This is not supported by libmysql".to_vec(),
        _ => error,
    };
    Failure::error(SqlError {
        state: SqlState(sqlstate),
        code: i64::from(errno),
        message,
    })
}

/// `mysql_warning_count` of the connection `conn` is.
pub(crate) fn warning_count(req: &mut Request, conn: &mut dyn Connection) -> i64 {
    let any: &mut dyn std::any::Any = conn;
    match any.downcast_mut::<Db>() {
        Some(db) => match db.handle(req) {
            Some(h) => i64::from(h.server.status(req).warning_count),
            None => 0,
        },
        None => 0,
    }
}

impl Connector for Driver {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn class(&self) -> Option<&'static [u8]> {
        Some(b"Pdo\\Mysql")
    }

    /// `pdo_mysql_fetch_error_func` reads `S->einfo` for a statement.
    fn errors_per_statement(&self) -> bool {
        true
    }

    /// `pdo_mysql_handle_factory`.
    fn open(&self, req: &mut Request, source: &Source<'_>) -> Result<Box<dyn Connection>, Failure> {
        let socket = req
            .ini_get(b"pdo_mysql.default_socket")
            .map(<[u8]>::to_vec)
            .filter(|s| !s.is_empty());
        let vars = parse_data_source(
            source.dsn,
            &[
                ("charset", None),
                ("dbname", Some(b"")),
                ("host", Some(b"localhost")),
                ("port", Some(b"3306")),
                ("unix_socket", socket.as_deref()),
                ("user", None),
                ("password", None),
            ],
        );
        let options = source.options;
        let mut connect_opts = flag::MULTI_RESULTS;
        if options.is_none() || option_long(req, options, Mysql::ATTR_MULTI_STATEMENTS, 1)? != 0 {
            connect_opts |= flag::MULTI_STATEMENTS;
        }
        let server = Mysqlnd::init(req, source.persistent);
        if source.persistent {
            server.restart_psession(req);
        }
        let mut h = H {
            server,
            assume_national_character_set_strings: false,
            buffered: true,
            emulate_prepare: true,
            fetch_table_names: false,
            local_infile: false,
            auto_commit: option_long(req, options, attr::AUTOCOMMIT, 1)? != 0,
            own_columns: false,
        };
        let failed = |req: &mut Request, server: Mysqlnd| {
            let failure = error_of(req, server, None);
            server
                .close(req, php_ext_mysqlnd::client::CloseType::Explicit)
                .ok();
            Err(failure)
        };
        if options.is_some() && !driver_options(req, options, &mut h, &mut connect_opts)? {
            return failed(req, server);
        }
        if !server.option(req, ClientOption::LocalInfile(h.local_infile)) {
            return failed(req, server);
        }
        let native = option_long(req, options, attr::STRINGIFY_FETCHES, 0)? == 0;
        if !server.option(req, ClientOption::IntAndFloatNative(native)) {
            return failed(req, server);
        }
        let [charset, dbname, host, port, unix_socket, user, password] =
            <[Option<Vec<u8>>; 7]>::try_from(vars).unwrap_or_default();
        if let Some(charset) = &charset
            && !server.option(req, ClientOption::SetCharsetName(charset))
        {
            return failed(req, server);
        }
        let port = port.map_or(3306, |p| atoi(&p));
        let unix_socket = if host.as_deref() == Some(b"localhost".as_slice()) {
            unix_socket
        } else {
            None
        };
        let user = source.username.map(<[u8]>::to_vec).or(user);
        let password = source.password.map(<[u8]>::to_vec).or(password);
        let connected = server.connect(
            req,
            Connect {
                host: host.as_deref(),
                user: user.as_deref(),
                password: password.as_deref(),
                db: dbname.as_deref(),
                port: port.cast_unsigned(),
                socket: unix_socket.as_deref(),
                flags: connect_opts,
            },
        )?;
        if !connected {
            return failed(req, server);
        }
        if !h.auto_commit {
            let _ = server.command(req, Command::Autocommit(false))?;
        }
        let state = req.state::<State>();
        let id = state.handles.len();
        state.handles.push(Some(h));
        Ok(Box::new(Db {
            place: Place::Held(id),
        }))
    }
}

/// The `driver_options` half of `pdo_mysql_handle_factory`: the `H`'s
/// settings and the connection's options read off them, the flags they
/// add; `false` where `mysql_options` refused one.
fn driver_options(
    req: &mut Request,
    options: Option<php_ext::values::PhpArray>,
    h: &mut H,
    connect_opts: &mut u32,
) -> Result<bool, Failure> {
    let server = h.server;
    let timeout = option_long(req, options, attr::TIMEOUT, 30)?;
    h.buffered = option_long(req, options, Mysql::ATTR_USE_BUFFERED_QUERY, 1)? != 0;
    h.emulate_prepare = option_long(req, options, attr::EMULATE_PREPARES, 1)? != 0;
    h.assume_national_character_set_strings =
        option_long(req, options, attr::DEFAULT_STR_PARAM, 0)? == STR_NATL;
    if option_long(req, options, Mysql::ATTR_FOUND_ROWS, 0)? != 0 {
        *connect_opts |= flag::FOUND_ROWS;
    }
    if option_long(req, options, Mysql::ATTR_IGNORE_SPACE, 0)? != 0 {
        *connect_opts |= flag::IGNORE_SPACE;
    }
    if !server.option(req, ClientOption::ConnectTimeout(low32(timeout))) {
        return Ok(false);
    }
    if option_long(req, options, Mysql::ATTR_LOCAL_INFILE, 0)? != 0 {
        h.local_infile = true;
    }
    if let Some(dir) = option_string(req, options, Mysql::ATTR_LOCAL_INFILE_DIRECTORY)?
        && req.check_open_basedir(&dir, true).is_ok()
        && !server.option(req, ClientOption::LoadDataLocalDir(Some(&dir)))
    {
        return Ok(false);
    }
    if let Some(command) = option_string(req, options, Mysql::ATTR_INIT_COMMAND)?
        && !server.option(req, ClientOption::InitCommand(&command))
    {
        return Ok(false);
    }
    if option_long(req, options, Mysql::ATTR_COMPRESS, 0)? != 0
        && !server.option(req, ClientOption::Compress)
    {
        return Ok(false);
    }
    let key = option_string(req, options, Mysql::ATTR_SSL_KEY)?;
    let cert = option_string(req, options, Mysql::ATTR_SSL_CERT)?;
    let ca = option_string(req, options, Mysql::ATTR_SSL_CA)?;
    let capath = option_string(req, options, Mysql::ATTR_SSL_CAPATH)?;
    let cipher = option_string(req, options, Mysql::ATTR_SSL_CIPHER)?;
    if key.is_some() || cert.is_some() || ca.is_some() || capath.is_some() || cipher.is_some() {
        server.option(req, ClientOption::SslKey(key.as_deref()));
        server.option(req, ClientOption::SslCert(cert.as_deref()));
        server.option(req, ClientOption::SslCa(ca.as_deref()));
        server.option(req, ClientOption::SslCapath(capath.as_deref()));
        server.option(req, ClientOption::SslCipher(cipher.as_deref()));
    }
    if let Some(key) = option_string(req, options, Mysql::ATTR_SERVER_PUBLIC_KEY)?
        && !server.option(req, ClientOption::ServerPublicKey(Some(&key)))
    {
        return Ok(false);
    }
    let verify = option_long(req, options, Mysql::ATTR_SSL_VERIFY_SERVER_CERT, -1)?;
    if verify != -1 {
        *connect_opts |= if verify != 0 {
            flag::SSL_VERIFY_SERVER_CERT
        } else {
            flag::SSL_DONT_VERIFY_SERVER_CERT
        };
    }
    Ok(true)
}

/// C's `atoi`: optional space and sign, then digits.
fn atoi(text: &[u8]) -> i32 {
    let s = std::str::from_utf8(text).unwrap_or_default().trim_start();
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'-') => (-1i64, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let n: i64 = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i64, |acc, d| {
            acc.saturating_mul(10).saturating_add(i64::from(d - b'0'))
        });
    i32::try_from(sign * n).unwrap_or(if sign < 0 { i32::MIN } else { i32::MAX })
}

/// The `unsigned int` php reads a `zend_long` option through: its low
/// 32 bits.
fn low32(n: i64) -> u32 {
    let b = n.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// The connection: where its `pdo_mysql_db_handle` stands.
pub(crate) struct Db {
    place: Place,
}

/// Where a connection's `pdo_mysql_db_handle` stands.
enum Place {
    /// In the request's table, at this number.
    Held(usize),
    /// A persistent connection the request gave back to the engine's
    /// pool: out of the request's table, with the connection and its
    /// socket, which wait in PDO's pool with it and in no other list
    /// until a request takes them up.
    Idle(H, Idle),
    /// In no request's table: closed, or given back and not kept.
    Gone,
}

impl Db {
    /// The number of its `pdo_mysql_db_handle` in the request's table.
    fn id(&self) -> Option<usize> {
        match self.place {
            Place::Held(id) => Some(id),
            Place::Idle(..) | Place::Gone => None,
        }
    }

    /// Its `pdo_mysql_db_handle` in the request's table.
    fn handle(&self, req: &mut Request) -> Option<H> {
        handle(req, self.id()?)
    }

    /// Its `pdo_mysql_db_handle` changed by `f`.
    fn update(&self, req: &mut Request, f: impl FnOnce(&mut H)) {
        if let Some(id) = self.id() {
            update(req, id, f);
        }
    }

    /// Its `pdo_mysql_db_handle`, refusing a closed connection as `Silent`.
    fn h(&self, req: &mut Request) -> Result<H, Failure> {
        self.handle(req).ok_or_else(Failure::silent)
    }
}

impl Connection for Db {
    /// `mysql_handle_preparer`.
    fn prepare(
        &mut self,
        req: &mut Request,
        sql: &[u8],
        options: Option<php_ext::values::PhpArray>,
    ) -> Result<Prepared, Failure> {
        let h = self.h(req)?;
        let id = self.id().ok_or_else(Failure::silent)?;
        let mut stmt = Stmt::new(id);
        let emulated = |stmt: Stmt| Prepared {
            statement: Box::new(stmt),
            placeholders: Placeholders::Emulated,
            parameter_map: None,
        };
        if h.emulate_prepare || h.server.status(req).server_version < 40100 {
            return Ok(emulated(stmt));
        }
        let rewritten = rewrite_named(sql, Some(crate::scanner::scan as Scanner))?;
        let (sent, map) = match &rewritten {
            Some(rewritten) => (
                rewritten.sql.as_slice(),
                Some(rewritten.parameter_map.clone()),
            ),
            None => (sql, None),
        };
        let mut ps = Ps::init(req, h.server);
        if !ps.prepare(req, sent)? {
            if h.server.status(req).errno == 1295 {
                ps.close(req, true)?;
                return Ok(emulated(stmt));
            }
            let failure = error_of(req, h.server, None);
            ps.close(req, true)?;
            return Err(failure);
        }
        stmt.num_params = ps.parameter_count();
        stmt.ps = Some(ps);
        update(req, id, |h| h.own_columns = true);
        stmt.max_length = option_long(req, options, attr::MAX_COLUMN_LEN, 0)? & 1 != 0;
        Ok(Prepared {
            statement: Box::new(stmt),
            placeholders: Placeholders::Native {
                named: false,
                positional: true,
            },
            parameter_map: map,
        })
    }

    /// `mysql_handle_doer`: the affected rows, results of further
    /// statements read and dropped.
    fn exec(&mut self, req: &mut Request, sql: &[u8]) -> Result<i64, Failure> {
        let h = self.h(req)?;
        if !h.server.query(req, sql)? {
            return Err(error_of(req, h.server, None));
        }
        let affected = h.server.status(req).affected_rows;
        if affected == u64::MAX {
            let failure = error_of(req, h.server, None);
            return match &failure.cause {
                php_ext_pdo::connector::Cause::Error(e) if e.code != 0 => Err(failure),
                _ => Ok(0),
            };
        }
        while h.server.status(req).more_results {
            if !h.server.next_rowset(req)? {
                return Err(error_of(req, h.server, None));
            }
            if let Some(result) = h.server.store(req)? {
                result.free(req, false)?;
            }
        }
        // php hands back `(int) c`.
        let b = affected.to_le_bytes();
        Ok(i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])))
    }

    /// `mysql_handle_quoter`: escaped by the connection's character set,
    /// `N'…'` for a national string, `_binary'…'` for a LOB.
    fn quote(
        &mut self,
        req: &mut Request,
        text: &[u8],
        ty: BindType,
        raises: OnError,
    ) -> Result<Vec<u8>, Failure> {
        let _ = raises;
        let h = self.h(req)?;
        let raw = i64::from(ty.raw());
        let lob = ty.raw() & 0xFFFF == LOB;
        let mut natl = false;
        if !lob {
            natl = h.assume_national_character_set_strings;
            if raw & STR_NATL == STR_NATL {
                natl = true;
            }
            if raw & STR_CHAR == STR_CHAR {
                natl = false;
            }
        }
        let escaped = h.server.escape(req, text);
        let mut out = Vec::with_capacity(escaped.len() + 10);
        if natl {
            out.extend_from_slice(b"N'");
        } else if lob {
            out.extend_from_slice(b"_binary'");
        } else {
            out.push(b'\'');
        }
        out.extend_from_slice(&escaped);
        out.push(b'\'');
        Ok(out)
    }

    /// `mysql_handle_begin`.
    fn begin(&mut self, req: &mut Request) -> Result<(), Failure> {
        self.exec(req, b"START TRANSACTION").map(drop)
    }

    /// `mysql_handle_commit`.
    fn commit(&mut self, req: &mut Request) -> Result<(), Failure> {
        let h = self.h(req)?;
        let done = h.server.command(
            req,
            Command::Commit {
                flags: 0,
                name: None,
            },
        )?;
        if done {
            Ok(())
        } else {
            Err(error_of(req, h.server, None))
        }
    }

    /// `mysql_handle_rollback`.
    fn rollback(&mut self, req: &mut Request) -> Result<(), Failure> {
        let h = self.h(req)?;
        let done = h.server.command(
            req,
            Command::Rollback {
                flags: 0,
                name: None,
            },
        )?;
        if done {
            Ok(())
        } else {
            Err(error_of(req, h.server, None))
        }
    }

    /// `pdo_mysql_in_transaction`.
    fn in_transaction(&mut self, req: &mut Request) -> Option<bool> {
        let h = self.handle(req)?;
        Some(h.server.status(req).server_status & IN_TRANS != 0)
    }

    /// `pdo_mysql_set_attribute`.
    fn set_attribute(
        &mut self,
        req: &mut Request,
        attr: i64,
        value: &Value,
    ) -> Result<(), Failure> {
        match attr {
            attr::AUTOCOMMIT => {
                let on = bool_attribute(req, value)?;
                let h = self.h(req)?;
                if h.auto_commit != on {
                    self.update(req, |h| h.auto_commit = on);
                    self.set_attribute_autocommit(req)?;
                }
            }
            attr::DEFAULT_STR_PARAM => {
                let n = long_attribute(req, value)?;
                self.update(req, |h| {
                    h.assume_national_character_set_strings = n == STR_NATL;
                });
            }
            Mysql::ATTR_USE_BUFFERED_QUERY => {
                let on = bool_attribute(req, value)?;
                self.update(req, |h| h.buffered = on);
            }
            attr::EMULATE_PREPARES => {
                let on = bool_attribute(req, value)?;
                self.update(req, |h| h.emulate_prepare = on);
            }
            attr::FETCH_TABLE_NAMES => {
                let on = bool_attribute(req, value)?;
                self.update(req, |h| h.fetch_table_names = on);
            }
            attr::STRINGIFY_FETCHES => {
                let on = bool_attribute(req, value)?;
                let h = self.h(req)?;
                if !h.server.option(req, ClientOption::IntAndFloatNative(!on)) {
                    return Err(error_of(req, h.server, None));
                }
            }
            _ => return Err(Failure::silent()),
        }
        Ok(())
    }

    /// `pdo_mysql_get_attribute`.
    fn attribute(&mut self, req: &mut Request, attr: i64) -> Result<Option<Value>, Failure> {
        let h = self.h(req)?;
        let string = |req: &mut Request, bytes: &[u8]| -> Result<Value, Failure> {
            Ok(Value::string(
                PhpString::new(req, bytes)?.hand_on_unmigrated(),
            ))
        };
        Ok(Some(match attr {
            attr::CLIENT_VERSION => string(req, php_ext_mysqlnd::client::CLIENT_INFO.as_bytes())?,
            attr::SERVER_VERSION => {
                let info = h.server.status(req).server_info.unwrap_or_default();
                string(req, &info)?
            }
            attr::CONNECTION_STATUS => {
                let info = h.server.status(req).host_info.unwrap_or_default();
                string(req, &info)?
            }
            attr::SERVER_INFO => match h.server.stat(req)? {
                Some(text) => string(req, &text)?,
                None => return Err(error_of(req, h.server, None)),
            },
            attr::AUTOCOMMIT => Value::bool(h.auto_commit),
            attr::DEFAULT_STR_PARAM => Value::int(if h.assume_national_character_set_strings {
                STR_NATL
            } else {
                STR_CHAR
            }),
            Mysql::ATTR_USE_BUFFERED_QUERY => Value::bool(h.buffered),
            attr::EMULATE_PREPARES => Value::bool(h.emulate_prepare),
            Mysql::ATTR_LOCAL_INFILE => Value::bool(h.local_infile),
            Mysql::ATTR_LOCAL_INFILE_DIRECTORY => match h.server.local_infile_directory(req) {
                Some(dir) => string(req, &dir)?,
                None => Value::null(),
            },
            attr::FETCH_TABLE_NAMES => Value::bool(h.fetch_table_names),
            _ => return Ok(None),
        }))
    }

    /// `pdo_mysql_last_insert_id`.
    fn last_insert_id(
        &mut self,
        req: &mut Request,
        name: Option<&[u8]>,
    ) -> Result<Vec<u8>, Failure> {
        let _ = name;
        let h = self.h(req)?;
        Ok(h.server.status(req).insert_id.to_string().into_bytes())
    }

    fn describes_at_execute(&self) -> bool {
        true
    }

    fn scanner(&self) -> Option<Scanner> {
        Some(crate::scanner::scan)
    }

    /// `pdo_mysql_check_liveness`: `mysql_ping` answered, over a
    /// connection the pool kept taken up by the request first.
    ///
    /// Two persistent handles with one DSN share one connection, which
    /// answers the ping PDO sends as the second asks for it:
    ///
    /// ```php
    /// $dsn = 'mysql:host=127.0.0.1;dbname=test';
    /// $a = new Pdo\Mysql($dsn, 'root', '', [PDO::ATTR_PERSISTENT => true]);
    /// $b = new Pdo\Mysql($dsn, 'root', '', [PDO::ATTR_PERSISTENT => true]);
    /// $c = new Pdo\Mysql($dsn, 'root', '');
    /// $id = fn($db) => $db->query('SELECT CONNECTION_ID()')->fetchColumn();
    /// var_dump($id($a) === $id($b), $id($a) === $id($c));
    /// $a->exec('SET @shared = 1');
    /// var_dump($b->query('SELECT @shared')->fetchColumn());
    /// ```
    /// ```expect mysql
    /// bool(true)
    /// bool(false)
    /// int(1)
    /// ```
    fn check_liveness(&mut self, req: &mut Request) -> bool {
        self.take_up(req);
        let Some(h) = self.handle(req) else {
            return false;
        };
        matches!(h.server.command(req, Command::Ping), Ok(true))
    }

    /// `pdo_mysql_request_shutdown`: `mysqlnd_end_psession`.
    fn persistent_shutdown(&mut self, req: &mut Request) {
        if let Some(h) = self.handle(req) {
            h.server.end_psession(req).ok();
        }
    }

    /// The connection out of the request's tables, its
    /// `pdo_mysql_db_handle` and the connection with its socket kept here,
    /// in PDO's pool with it, for the next request to take up. One mysqlnd
    /// did not keep, as no persistent connection that connected, is gone,
    /// so the next request's `check_liveness` finds it dead.
    fn give_back(&mut self, req: &mut Request) {
        let Some(id) = self.id() else {
            return;
        };
        let Some(h) = handle(req, id) else {
            return;
        };
        let kept = h.server.give_back(req);
        if let Some(slot) = req.state::<State>().handles.get_mut(id) {
            *slot = None;
        }
        self.place = match kept {
            Some(kept) => Place::Idle(h, kept),
            None => Place::Gone,
        };
    }

    /// `mysql_handle_closer`, over a connection the request gave back
    /// taken up first, as PDO closes one past its pool's bound.
    fn close(&mut self, req: &mut Request) {
        self.take_up(req);
        let Some(id) = self.id() else {
            return;
        };
        if let Some(h) = handle(req, id) {
            h.server
                .close(req, php_ext_mysqlnd::client::CloseType::Explicit)
                .ok();
        }
        if let Some(slot) = req.state::<State>().handles.get_mut(id) {
            *slot = None;
        }
        self.place = Place::Gone;
    }
}

impl Db {
    /// A connection given back to the engine's pool held in the request's
    /// table again, over the socket it kept, or gone where the request
    /// could not hold it; one not given back stays where it stands.
    fn take_up(&mut self, req: &mut Request) {
        if !matches!(self.place, Place::Idle(..)) {
            return;
        }
        let Place::Idle(mut h, kept) = std::mem::replace(&mut self.place, Place::Gone) else {
            return;
        };
        let Ok(server) = Mysqlnd::take_up(req, kept) else {
            return;
        };
        h.server = server;
        let state = req.state::<State>();
        self.place = Place::Held(state.handles.len());
        state.handles.push(Some(h));
    }

    /// `mysql_handle_autocommit` after the attribute changed.
    fn set_attribute_autocommit(&self, req: &mut Request) -> Result<(), Failure> {
        let h = self.h(req)?;
        if !h.server.command(req, Command::Autocommit(h.auto_commit))? {
            return Err(error_of(req, h.server, None));
        }
        Ok(())
    }
}
