//! `class mysqli`: the link, `MY_MYSQL` behind its `MYSQLI_RESOURCE`, with
//! `mysqli_common_connect`, `php_mysqli_close` and the bodies of the
//! link's functions and methods (`mysqli_api.c`, `mysqli_nonapi.c`), and
//! the link's properties (`mysqli_prop.c`).

#![allow(non_camel_case_types)]

use php_ext::abi::{Instance, OrFalse, This, True, Union, Untyped};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{to_long, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_mysqlnd::client::{
    CLIENT_INFO, CLIENT_VERSION, ClientOption, CloseType, Command, Connect, Mysqlnd, ServerOption,
};

use crate::props::{self, Props};
use crate::result::mysqli_result;
use crate::stmt::mysqli_stmt;
use crate::{
    On, Resource, State, affected, closed, extras, ini_bool, ini_string, long_or_string,
    not_initialized, report_conn, report_index, report_mode, sql_exception, string,
};

/// `CLIENT_MULTI_STATEMENTS` and `CLIENT_MULTI_RESULTS`.
const MULTI_STATEMENTS: u32 = 1 << 16;
const MULTI_RESULTS: u32 = 1 << 17;

/// `MYSQLI_STORE_RESULT`, `MYSQLI_USE_RESULT`, `MYSQLI_ASYNC` and
/// `MYSQLI_STORE_RESULT_COPY_DATA`.
pub(crate) const STORE: i64 = 0;
pub(crate) const USE: i64 = 1;
const ASYNC: i64 = 8;
const COPY_DATA: i64 = 16;

/// `sizeof(MY_MYSQL)`.
const MY_MYSQL: usize = 56;

/// The link: `intern->ptr` and its status. A link that never connected,
/// was closed, or whose constructor failed to connect has no resource,
/// which php's every check reads as "already closed".
#[derive(Default)]
pub(crate) enum Link {
    #[default]
    Closed,
    /// `MYSQLI_STATUS_INITIALIZED`: made by `mysqli_init`, or a
    /// `real_connect` that failed.
    Made(Resource<Conn>),
    /// `MYSQLI_STATUS_VALID`: connected.
    Open(Resource<Conn>),
}

/// `MY_MYSQL`, in the allocation php `ecalloc`s for it.
pub(crate) struct Conn {
    /// `mysql`, which a close leaves unset until the next connect.
    pub(crate) mysql: Option<Mysqlnd>,
    /// `hash_key` of a persistent link, `mysql->persistent` where set:
    /// the request's string `strpprintf` makes, released by
    /// `php_clear_mysql`.
    hash_key: Option<Owner<PhpString>>,
    /// `multi_query`: whether multi statements are on.
    multi_query: bool,
    /// `async_result_fetch_type`: how `reap_async_query` reads the rows.
    async_mode: i64,
    held: Emalloc,
}

impl Conn {
    /// `ecalloc(1, sizeof(MY_MYSQL))`; the `Err` is the memory limit's
    /// fatal.
    fn new(req: &mut Request) -> Result<Conn, Thrown> {
        Ok(Conn {
            mysql: None,
            hash_key: None,
            multi_query: false,
            async_mode: STORE,
            held: Emalloc::new(req, MY_MYSQL)?,
        })
    }

    /// `php_clear_mysql`'s `hash_key` released, which a close of a
    /// persistent link gives back with its connection:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $m = memory_get_usage();
    /// $l = mysqli_connect('p:127.0.0.1', 'root', '', 'test');
    /// echo memory_get_usage() - $m, "\n";
    /// mysqli_close($l);
    /// echo memory_get_usage() - $m, "\n";
    /// $l = mysqli_connect('p:127.0.0.1', 'root', '', 'test');
    /// echo memory_get_usage() - $m, "\n";
    /// $l = null;
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect mysql
    /// 544
    /// 416
    /// 576
    /// 32
    /// ```
    fn clear(&mut self, req: &mut Request) {
        if let Some(key) = self.hash_key.take() {
            key.give_up(req);
        }
    }

    /// `efree(mysql)`, `php_clear_mysql` having released what it holds.
    fn free(mut self, req: &mut Request) {
        self.clear(req);
        self.held.free(req);
    }
}

/// How far a link must have got, `MYSQLI_STATUS_INITIALIZED` or
/// `MYSQLI_STATUS_VALID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Need {
    Made,
    Open,
}

/// `MYSQLI_FETCH_RESOURCE_CONN`: the link's connection, or php's `Error`
/// for a closed link or one short of `need`.
pub(crate) fn conn(req: &mut Request, object: PhpObject, need: Need) -> Result<Mysqlnd, Thrown> {
    let got = match crate::payload_ref::<Link>(req, object) {
        Some(Link::Open(c)) => Some((c.mysql, true)),
        Some(Link::Made(c)) => Some((c.mysql, false)),
        _ => None,
    };
    match got {
        None => Err(closed(req, object)),
        Some((_, false)) if need == Need::Open => Err(not_initialized(req, object)),
        Some((None, _)) => Err(not_initialized(req, object)),
        Some((Some(mysql), _)) => Ok(mysql),
    }
}

/// The link's `MY_MYSQL`, for its flags.
fn with_conn<R>(req: &mut Request, object: PhpObject, f: impl FnOnce(&mut Conn) -> R) -> Option<R> {
    match crate::payload::<Link>(req, object)? {
        Link::Made(c) | Link::Open(c) => Some(f(c)),
        Link::Closed => None,
    }
}

/// `MYSQLI_DISABLE_MQ`.
fn disable_mq(req: &mut Request, object: PhpObject, mysql: Mysqlnd) -> Result<(), Thrown> {
    if with_conn(req, object, |c| std::mem::take(&mut c.multi_query)).unwrap_or(false) {
        mysql.command(
            req,
            Command::SetServerOption(ServerOption::MultiStatementsOff),
        )?;
    }
    Ok(())
}

/// `MYSQLI_ENABLE_MQ`.
fn enable_mq(req: &mut Request, object: PhpObject, mysql: Mysqlnd) -> Result<(), Thrown> {
    if !with_conn(req, object, |c| std::mem::replace(&mut c.multi_query, true)).unwrap_or(true) {
        mysql.command(
            req,
            Command::SetServerOption(ServerOption::MultiStatementsOn),
        )?;
    }
    Ok(())
}

/// `mysqli_link_class_entry`.
pub(crate) fn link_class(req: &mut Request) -> Result<php_ext::values::ClassId, Thrown> {
    lookup_class(req, b"mysqli")?.ok_or_else(|| fatal(req, "InternalError: mysqli is not linked"))
}

// ---- connecting

/// What `mysqli_common_connect` parses: `|s!s!s!s!l!s!`, and `l` for a
/// real connect.
#[derive(Default, Clone, Copy)]
pub(crate) struct Args {
    pub(crate) host: Option<PhpString>,
    pub(crate) user: Option<PhpString>,
    pub(crate) password: Option<PhpString>,
    pub(crate) database: Option<PhpString>,
    pub(crate) port: Option<i64>,
    pub(crate) socket: Option<PhpString>,
    pub(crate) flags: i64,
}

/// `php_mysqli_set_error`.
fn set_connect_error(req: &mut Request, errno: u32, error: &[u8]) {
    let state = req.state::<State>();
    state.connect_errno = i64::from(errno);
    state.connect_error = (!error.is_empty()).then(|| error.to_vec());
}

/// Where a connect goes, the arguments with `MyG(default_*)` filling what
/// they leave out, a host's `p:` taken off.
struct Target {
    host: Option<Vec<u8>>,
    user: Option<Vec<u8>>,
    password: Option<Vec<u8>>,
    database: Option<Vec<u8>>,
    port: i64,
    socket: Option<Vec<u8>>,
    /// Whether the host asked for a persistent link, `p:`.
    persistent: bool,
}

impl Target {
    fn of(req: &Request, args: &Args) -> Target {
        let given = |v: Option<PhpString>| v.map(|v| v.as_bytes().to_vec());
        let unempty = |v: Option<PhpString>| given(v).filter(|v| !v.is_empty());
        let mut host = unempty(args.host).or_else(|| ini_string(req, b"mysqli.default_host"));
        let persistent = host
            .as_ref()
            .is_some_and(|h| h.len() > 2 && h[..2].eq_ignore_ascii_case(b"p:"));
        if persistent {
            host = host.map(|h| h[2..].to_vec());
        }
        Target {
            host,
            user: given(args.user).or_else(|| ini_string(req, b"mysqli.default_user")),
            password: given(args.password).or_else(|| ini_string(req, b"mysqli.default_pw")),
            database: given(args.database),
            port: args
                .port
                .filter(|p| *p != 0)
                .unwrap_or_else(|| req.ini().long(b"mysqli.default_port")),
            socket: unempty(args.socket).or_else(|| ini_string(req, b"mysqli.default_socket")),
            persistent,
        }
    }

    /// `hash_key`: `mysqli_%s_%s%ld%s%s%s` of host, socket, `port`, user,
    /// database and password.
    fn key(&self) -> Vec<u8> {
        let part = |v: &Option<Vec<u8>>| v.clone().unwrap_or_default();
        [
            b"mysqli_".to_vec(),
            part(&self.host),
            b"_".to_vec(),
            part(&self.socket),
            self.port.to_string().into_bytes(),
            part(&self.user),
            part(&self.database),
            part(&self.password),
        ]
        .concat()
    }
}

/// The persistent connections the request holds under one `hash_key`:
/// those open on a link, and those a closed link left free, php's
/// `plist->free_links`.
#[derive(Default)]
pub(crate) struct Plinks {
    open: Vec<Mysqlnd>,
    free: Vec<Mysqlnd>,
}

/// The request's [`Plinks`] under `key`, held from here to
/// `post_deactivate`.
fn plinks<'r>(req: &'r mut Request, key: &[u8]) -> Option<&'r mut Plinks> {
    let plinks = &mut req.state::<State>().plinks;
    let key = key.to_vec();
    let at = match plinks.find(&key) {
        Some(at) => at,
        None => plinks.hold(key, Plinks::default()),
    };
    plinks.get_mut(at)
}

/// A free connection of the key's, the request's or one the engine's pool
/// lends, taken up with `change_user` under the new credentials; `false`
/// where there is none or the change failed, which closes it.
fn reuse(req: &mut Request, mysql: &mut Conn, target: &Target, key: &[u8]) -> Result<bool, Thrown> {
    let pooled = if let Some(free) = plinks(req, key).and_then(|held| held.free.pop()) {
        req.state::<State>().num_free_persistent -= 1;
        free
    } else {
        let kept = req
            .engine::<crate::Pooled>()
            .and_then(|crate::Pooled(pool)| pool.take(&key.to_vec()));
        let Some(kept) = kept else {
            return Ok(false);
        };
        Mysqlnd::take_up(req, kept)?
    };
    if let Some(made) = mysql.mysql.take() {
        made.close(req, CloseType::Implicit)?;
    }
    let changed = pooled.command(
        req,
        Command::ChangeUser {
            user: target.user.as_deref(),
            password: target.password.as_deref(),
            db: target.database.as_deref(),
            silent: true,
        },
    )?;
    if !changed {
        pooled.close(req, CloseType::Implicit)?;
        return Ok(false);
    }
    pooled.restart_psession(req);
    if let Some(held) = plinks(req, key) {
        held.open.push(pooled);
    }
    req.state::<State>().num_active_persistent += 1;
    let status = pooled.status(req);
    set_connect_error(req, status.errno, &status.error);
    mysql.mysql = Some(pooled);
    Ok(true)
}

/// Every persistent connection the request holds given back to the
/// engine's pool after the request's objects are freed, where php's
/// stays in its `EG(persistent_list)` entry's `free_links`: each link was
/// freed in `shutdown_executor`, after every extension's `RSHUTDOWN` (so
/// a session save handler's `write` still had it), and its
/// `php_mysqli_close` put its connection among the free ones. One past
/// the pool's bound is closed, and one still open on no link, which
/// php's order leaves none of, closed as a failed `end_psession` closes
/// it.
pub(crate) fn post_deactivate(req: &mut Request) {
    for (key, plinks) in req.state::<State>().plinks.drain() {
        for handle in plinks.open {
            handle.close(req, CloseType::Explicit).ok();
        }
        for handle in plinks.free {
            give_to_pool(req, &key, handle);
        }
    }
}

/// `handle` kept, with its socket, in the engine's pool of mysqli's free
/// links under `key`, or closed with this request past its bound.
fn give_to_pool(req: &mut Request, key: &[u8], handle: Mysqlnd) {
    let Some(kept) = handle.give_back(req) else {
        return;
    };
    let past = match req.engine::<crate::Pooled>() {
        Some(crate::Pooled(pool)) => pool.give_back(key.to_vec(), kept),
        None => Some(kept),
    };
    if let Some(past) = past {
        past.close(req).ok();
    }
}

/// What php's `zend_shutdown` does to mysqli's free links when it
/// destroys `EG(persistent_list)` (`Z/Zend/zend.c:1166`): each the pool
/// keeps closed on the shutdown request, the last given back first, as
/// `php_mysqli_dtor` cleans `plist->free_links` with
/// `php_mysqli_dtor_p_elements` (`mysqli.c:89-104`), an implicit close
/// that sends the server its `COM_QUIT` (#1961).
pub(crate) fn engine_end(req: &mut Request) {
    let idle = req
        .engine::<crate::Pooled>()
        .map(|crate::Pooled(pool)| pool.drain())
        .unwrap_or_default();
    for (_, idle) in idle {
        idle.close(req).ok();
    }
}

/// `php_mysqli_close`'s persistent half before the push:
/// `mysqlnd_end_psession`, and the rollback `mysqli.rollback_on_cached_plink`
/// asks for; `false` where that rollback failed, which closes the
/// connection instead.
fn end_psession(req: &mut Request, handle: Mysqlnd) -> Result<bool, Thrown> {
    handle.end_psession(req)?;
    Ok(!ini_bool(req, b"mysqli.rollback_on_cached_plink")
        || handle.command(
            req,
            Command::Rollback {
                flags: 0,
                name: None,
            },
        )?)
}

/// `mysqli.max_links` and `mysqli.max_persistent` held to the counts: the
/// warning's text where one is reached.
///
/// The three entries are `OnUpdateLong`'s, read by the quantity rule with
/// its warning: `-0x1` is `-1`, no limit, and `1x` is 1, where nothing
/// listens.
///
/// ```php
/// $code = 'mysqli_report(MYSQLI_REPORT_OFF); var_dump(@mysqli_connect("127.0.0.1", "u", "p", "d"), mysqli_connect_error(), ini_set("mysqli.default_port", "3306q"));';
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'mysqli.max_links=-0x1', '-d', 'mysqli.default_port=1x', '-r', $code], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
/// ```
/// ```expect process
/// Warning: Invalid "mysqli.default_port" setting. Invalid quantity "1x": unknown multiplier "x", interpreting as "1" for backwards compatibility in Unknown on line 0
///
/// Warning: Invalid "mysqli.default_port" setting. Invalid quantity "3306q": unknown multiplier "q", interpreting as "3306" for backwards compatibility in Command line code on line 1
/// bool(false)
/// string(18) "Connection refused"
/// string(2) "1x"
///
/// Warning: Invalid "mysqli.default_port" setting. Invalid quantity "1x": unknown multiplier "x", interpreting as "1" for backwards compatibility in Unknown on line 0
/// 0
/// ```
fn over_limit(req: &mut Request, persistent: bool) -> Option<String> {
    let max_links = req.ini().long(b"mysqli.max_links");
    let max_persistent = req.ini().long(b"mysqli.max_persistent");
    let state = req.state::<State>();
    let persistent_links = state.num_active_persistent;
    let persistent_links = persistent_links + crate::num_inactive_persistent(req);
    let state = req.state::<State>();
    if max_links != -1 && state.num_links >= max_links {
        Some(format!("Too many open links ({})", state.num_links))
    } else if persistent && max_persistent != -1 && persistent_links >= max_persistent {
        Some(format!(
            "Too many open persistent links ({persistent_links})"
        ))
    } else {
        None
    }
}

/// The link's state before a connect: refused where a real connect finds
/// no made link or the constructor runs twice, and otherwise the link as
/// it stood, taken off its object; `Link::Closed` for no link or one with
/// no resource.
fn connect_from(
    req: &mut Request,
    object: Option<PhpObject>,
    real: bool,
    in_ctor: bool,
) -> Result<Option<Link>, Thrown> {
    let Some(object) = object else {
        return Ok((!real).then_some(Link::Closed));
    };
    if real {
        conn(req, object, Need::Made)?;
    } else if in_ctor
        && !matches!(
            crate::payload_ref::<Link>(req, object),
            Some(Link::Closed) | None
        )
    {
        return Err(throw_error(req, "Error", "Cannot call constructor twice"));
    }
    Ok(Some(
        crate::payload::<Link>(req, object)
            .map(std::mem::take)
            .unwrap_or_default(),
    ))
}

/// `mysqli_common_connect`: `object` is the link a method or the
/// constructor is served on, none for `mysqli_connect`; a real connect
/// keeps the link's options and refuses `CLIENT_MULTI_STATEMENTS`. The
/// answer is the link connected, `None` for php's `false`.
pub(crate) fn common_connect(
    req: &mut Request,
    object: Option<PhpObject>,
    args: Args,
    real: bool,
    in_ctor: bool,
) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let (existing, open) = match connect_from(req, object, real, in_ctor)? {
        None => return Ok(None),
        Some(Link::Open(resource)) => (Some(resource), true),
        Some(Link::Made(resource)) => (Some(resource), false),
        Some(Link::Closed) => (None, false),
    };
    let mut flags = u32::try_from(args.flags & 0xFFFF_FFFF).unwrap_or(0);
    if real {
        flags &= !MULTI_STATEMENTS;
    }
    flags |= MULTI_RESULTS;
    let (resource, mut mysql) = match existing {
        Some(resource) => {
            let (resource, mysql) = resource.repoint(());
            (Some(resource), mysql)
        }
        None => (None, Conn::new(req)?),
    };
    let target = Target::of(req, &args);
    if mysql.mysql.is_some() && open {
        close_conn(req, &mut mysql, CloseType::Implicit, true)?;
    }
    let mut persistent = false;
    let mut reused = false;
    if target.persistent {
        if ini_bool(req, b"mysqli.allow_persistent") {
            persistent = true;
            let key = target.key();
            let held = PhpString::new(req, &key)?;
            mysql.clear(req);
            mysql.hash_key = Some(held);
            reused = reuse(req, &mut mysql, &target, &key)?;
        } else {
            let text = "Persistent connections are disabled. Downgrading to normal";
            docref(req, Level::Warning, text);
        }
    }
    let mut new_connection = false;
    if !reused {
        if let Some(refused) = over_limit(req, persistent) {
            docref(req, Level::Warning, &refused);
            return Ok(give_back(req, object, mysql, resource));
        }
        let handle = if let Some(handle) = mysql.mysql {
            handle
        } else {
            new_connection = true;
            let handle = Mysqlnd::init(req, persistent)?;
            mysql.mysql = Some(handle);
            handle
        };
        let to = Connect {
            host: target.host.as_deref(),
            user: target.user.as_deref(),
            password: target.password.as_deref(),
            db: target.database.as_deref(),
            port: u32::try_from(target.port & 0xFFFF_FFFF).unwrap_or(0),
            socket: target.socket.as_deref(),
            flags,
        };
        if !handle.connect(req, to)? {
            let status = handle.status(req);
            set_connect_error(req, status.errno, &status.error);
            let thrown = sql_exception(req, &status.sqlstate, status.errno, &status.error);
            if !real {
                mysql.mysql = None;
                handle.close(req, CloseType::Disconnected)?;
            }
            give_back(req, object, mysql, resource);
            thrown?;
            return Ok(None);
        }
        connected(req, handle)?;
        if let Some(key) = mysql.hash_key.as_ref().map(|k| k.as_bytes().to_vec())
            && let Some(held) = plinks(req, &key)
        {
            held.open.push(handle);
        }
    }
    // `end:`
    let state = req.state::<State>();
    if persistent && (new_connection || real) {
        state.num_active_persistent += 1;
    }
    state.num_links += 1;
    mysql.multi_query = false;
    let resource = match resource {
        Some(resource) => resource.repoint(mysql).0,
        None => Resource::new(req, mysql)?,
    };
    let object = if let Some(object) = object {
        Owner::take(req, object)
    } else {
        let class = link_class(req)?;
        new_object(req, class)?
    };
    if let Some(link) = crate::payload::<Link>(req, *object) {
        *link = Link::Open(resource);
    }
    Ok(Some(object))
}

/// What a connect that passed sets: the connect error cleared, LOCAL
/// INFILE as `mysqli.allow_local_infile` allows it, and its directory
/// where `open_basedir` lets it be.
fn connected(req: &mut Request, handle: Mysqlnd) -> Result<(), Thrown> {
    let status = handle.status(req);
    set_connect_error(req, status.errno, &status.error);
    let allow = ini_bool(req, b"mysqli.allow_local_infile");
    handle.option(req, ClientOption::LocalInfile(allow))?;
    if let Some(dir) = ini_string(req, b"mysqli.local_infile_directory")
        && req
            .environment_mut()
            .check_open_basedir(&dir, false)
            .is_ok()
    {
        handle.option(req, ClientOption::LoadDataLocalDir(Some(&dir)))?;
    }
    Ok(())
}

/// `err:` of `mysqli_common_connect`: the persistent key dropped, and the
/// `MY_MYSQL` put back behind the resource of the link it came from, or
/// freed where the connect made it (a link with no resource).
fn give_back(
    req: &mut Request,
    object: Option<PhpObject>,
    mut mysql: Conn,
    resource: Option<Resource<()>>,
) -> Option<Owner<PhpObject>> {
    mysql.clear(req);
    let link = object.and_then(|object| crate::payload::<Link>(req, object));
    match (resource, link) {
        (Some(resource), Some(link)) => *link = Link::Made(resource.repoint(mysql).0),
        (Some(resource), None) => {
            resource.free(req);
            mysql.free(req);
        }
        (None, _) => mysql.free(req),
    }
    None
}

/// `php_mysqli_close`: the link counted off, its connection closed, or a
/// persistent one given back to its key's free links.
fn close_conn(
    req: &mut Request,
    mysql: &mut Conn,
    kind: CloseType,
    open: bool,
) -> Result<(), Thrown> {
    if open {
        req.state::<State>().num_links -= 1;
    }
    let Some(handle) = mysql.mysql.take() else {
        mysql.clear(req);
        return Ok(());
    };
    let key = mysql.hash_key.as_ref().map(|k| k.as_bytes().to_vec());
    // `php_clear_mysql`, which php calls last: nothing reads the string
    // after its bytes are copied.
    mysql.clear(req);
    match key {
        None => {
            handle.close(req, kind)?;
        }
        Some(key) => {
            req.state::<State>().num_active_persistent -= 1;
            let ended = end_psession(req, handle)?;
            if let Some(held) = plinks(req, &key) {
                held.open.retain(|open| *open != handle);
                if ended {
                    held.free.push(handle);
                }
            }
            if ended {
                req.state::<State>().num_free_persistent += 1;
            } else {
                handle.close(req, kind)?;
            }
        }
    }
    Ok(())
}

/// `php_mysqli_init`: a link made with a connection not yet connected,
/// always persistent-capable as php makes it; on `object` for the method,
/// on a new link otherwise. The link's `MY_MYSQL` and resource are the
/// request's, which `mysqli_close` gives back; the connection's are
/// persistent, which no request counts:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $m = memory_get_usage();
/// $l = mysqli_init();
/// echo memory_get_usage() - $m, "\n";
/// mysqli_close($l);
/// echo memory_get_usage() - $m, "\n";
/// unset($l);
/// echo memory_get_usage() - $m, "\n";
/// $l = new mysqli();
/// echo memory_get_usage() - $m, "\n";
/// $l->close();
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql
/// 464
/// 416
/// 32
/// 496
/// 416
/// ```
pub(crate) fn init(
    req: &mut Request,
    object: Option<PhpObject>,
) -> Result<Owner<PhpObject>, Thrown> {
    if let Some(object) = object
        && !matches!(
            crate::payload_ref::<Link>(req, object),
            Some(Link::Closed) | None
        )
    {
        return Ok(Owner::take(req, object));
    }
    let object = if let Some(object) = object {
        Owner::take(req, object)
    } else {
        let class = link_class(req)?;
        new_object(req, class)?
    };
    let mut mysql = Conn::new(req)?;
    mysql.mysql = Some(Mysqlnd::init(req, true)?);
    let resource = Resource::new(req, mysql)?;
    if let Some(link) = crate::payload::<Link>(req, *object) {
        *link = Link::Made(resource);
    }
    Ok(object)
}

/// `mysqli_close`.
pub(crate) fn close(req: &mut Request, on: On) -> Result<True, Thrown> {
    conn(req, on.object, Need::Made)?;
    let link = crate::payload::<Link>(req, on.object)
        .map(std::mem::take)
        .unwrap_or_default();
    let (mut mysql, open) = match link {
        Link::Open(c) => (c, true),
        Link::Made(c) => (c, false),
        Link::Closed => return Ok(True),
    };
    let closed = close_conn(req, &mut mysql, CloseType::Explicit, open);
    // `MYSQLI_CLEAR_RESOURCE(mysql_link); efree(mysql)`.
    mysql.free(req).free(req);
    closed?;
    Ok(True)
}

/// `mysqli_link_free_storage`.
fn free_link(req: &mut Request, object: PhpObject) {
    let link = crate::payload::<Link>(req, object)
        .map(std::mem::take)
        .unwrap_or_default();
    let (mut mysql, open) = match link {
        Link::Open(c) => (c, true),
        Link::Made(c) => (c, false),
        Link::Closed => return,
    };
    if mysql.mysql.is_some() {
        close_conn(req, &mut mysql, CloseType::Explicit, open).ok();
    }
    // `efree(mysql)`, then `mysqli_objects_free_storage`'s `efree(my_res)`.
    let (resource, mysql) = mysql.repoint(());
    mysql.free(req);
    resource.free(req);
}

// ---- queries

/// `mysqli_query`.
pub(crate) fn query(
    req: &mut Request,
    on: On,
    query: PhpString,
    mode: i64,
) -> Result<Union<Owner<Instance<mysqli_result>>, bool>, Thrown> {
    if query.is_empty() {
        return Err(arg_value_error(req, on.at(2), "not be empty"));
    }
    if mode & !ASYNC != USE && STORE != mode & !(ASYNC | COPY_DATA) {
        return Err(arg_value_error(
            req,
            on.at(3),
            "be either MYSQLI_USE_RESULT or MYSQLI_STORE_RESULT with MYSQLI_ASYNC as an optional bitmask flag",
        ));
    }
    let mysql = conn(req, on.object, Need::Open)?;
    disable_mq(req, on.object, mysql)?;
    let q = query.as_bytes();
    if mode & ASYNC != 0 {
        if !mysql.send_query(req, q)? {
            report_conn(req, mysql)?;
            return Ok(Union::B(false));
        }
        with_conn(req, on.object, |c| c.async_mode = mode & !ASYNC);
        return Ok(Union::B(true));
    }
    if !mysql.query(req, q)? {
        report_conn(req, mysql)?;
        return Ok(Union::B(false));
    }
    rows_of(req, mysql, mode & !(ASYNC | COPY_DATA), q)
}

/// What a query that ran answers: `true` where it read no rows, and
/// otherwise its rows stored or used, the index reported.
fn rows_of(
    req: &mut Request,
    mysql: Mysqlnd,
    mode: i64,
    query: &[u8],
) -> Result<Union<Owner<Instance<mysqli_result>>, bool>, Thrown> {
    let status = mysql.status(req);
    if status.field_count == 0 {
        report_index(req, query, status.server_status)?;
        return Ok(Union::B(true));
    }
    let rows = match mode {
        STORE => mysql.store(req)?,
        USE => mysql.use_unbuffered(req)?,
        _ => None,
    };
    let Some(rows) = rows else {
        report_conn(req, mysql)?;
        return Ok(Union::B(false));
    };
    let result = crate::result::wrap(req, rows)?;
    let status = mysql.status(req);
    if let Err(thrown) = report_index(req, query, status.server_status) {
        result.give_up(req);
        return Err(thrown);
    }
    Ok(Union::A(result))
}

/// `mysqli_real_query`.
pub(crate) fn real_query(req: &mut Request, on: On, query: PhpString) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    disable_mq(req, on.object, mysql)?;
    if !mysql.query(req, query.as_bytes())? {
        report_conn(req, mysql)?;
        return Ok(false);
    }
    let status = mysql.status(req);
    if status.field_count == 0 {
        report_index(req, query.as_bytes(), status.server_status)?;
    }
    Ok(true)
}

/// `mysqli_multi_query`: multi statements on; on a failure the error
/// reported, multi statements off again and the error kept through it.
pub(crate) fn multi_query(req: &mut Request, on: On, query: PhpString) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    enable_mq(req, on.object, mysql)?;
    if mysql.query(req, query.as_bytes())? {
        return Ok(true);
    }
    mysql.keeping_error(req, |req| {
        let reported = report_conn(req, mysql);
        let disabled = disable_mq(req, on.object, mysql);
        reported.and(disabled)
    })?;
    Ok(false)
}

/// `mysqli_reap_async_query`.
pub(crate) fn reap_async_query(
    req: &mut Request,
    on: On,
) -> Result<Union<Owner<Instance<mysqli_result>>, bool>, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    if !mysql.reap_query(req)? {
        report_conn(req, mysql)?;
        return Ok(Union::B(false));
    }
    let status = mysql.status(req);
    if status.field_count == 0 {
        return Ok(Union::B(true));
    }
    let mode = with_conn(req, on.object, |c| c.async_mode).unwrap_or(STORE);
    let rows = match mode {
        STORE => mysql.store(req)?,
        USE => mysql.use_unbuffered(req)?,
        _ => None,
    };
    let Some(rows) = rows else {
        report_conn(req, mysql)?;
        return Ok(Union::B(false));
    };
    crate::result::wrap(req, rows).map(Union::A)
}

/// `mysqli_store_result` and `mysqli_use_result`.
pub(crate) fn take_rows(
    req: &mut Request,
    on: On,
    mode: i64,
) -> Result<OrFalse<Owner<Instance<mysqli_result>>>, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    let rows = if mode == USE {
        mysql.use_unbuffered(req)?
    } else {
        mysql.store(req)?
    };
    let Some(rows) = rows else {
        report_conn(req, mysql)?;
        return Ok(OrFalse::False);
    };
    let result = crate::result::wrap(req, rows)?;
    let status = mysql.status(req);
    if let Err(thrown) = report_index(req, b"from previous query", status.server_status) {
        result.give_up(req);
        return Err(thrown);
    }
    Ok(OrFalse::Value(result))
}

/// `mysqli_store_result`'s deprecated `$mode`.
pub(crate) fn store_result(
    req: &mut Request,
    on: On,
    given: bool,
) -> Result<OrFalse<Owner<Instance<mysqli_result>>>, Thrown> {
    if given {
        php_ext::raising::deprecated(
            req,
            "Passing the $mode parameter is deprecated since 8.4, as it has been ignored since 8.1",
        );
        php_ext::raising::pending(req)?;
    }
    take_rows(req, on, STORE)
}

/// `mysqli_next_result`.
pub(crate) fn next_result(req: &mut Request, on: On) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    if !mysql.next_rowset(req)? {
        report_conn(req, mysql)?;
        return Ok(false);
    }
    Ok(true)
}

/// A command with nothing to answer but whether it passed, the error
/// reported where it did not.
pub(crate) fn command(
    req: &mut Request,
    on: On,
    need: Need,
    command: Command<'_>,
) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, need)?;
    if !mysql.command(req, command)? {
        report_conn(req, mysql)?;
        return Ok(false);
    }
    Ok(true)
}

/// `mysqli_begin_transaction`.
pub(crate) fn begin_transaction(
    req: &mut Request,
    on: On,
    flags: i64,
    name: Option<PhpString>,
) -> Result<bool, Thrown> {
    conn(req, on.object, Need::Open)?;
    if flags < 0 {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be one of the MYSQLI_TRANS_* constants",
        ));
    }
    if name.is_some_and(|n| n.is_empty()) {
        return Err(arg_value_error(req, on.at(3), "not be empty"));
    }
    let mode = u32::try_from(flags & 0xFFFF_FFFF).unwrap_or(0);
    let name = name.as_ref().map(PhpString::as_bytes);
    command(req, on, Need::Open, Command::Begin { mode, name })
}

/// `mysqli_commit` and `mysqli_rollback`.
pub(crate) fn end_transaction(
    req: &mut Request,
    on: On,
    commit: bool,
    flags: i64,
    name: Option<PhpString>,
) -> Result<bool, Thrown> {
    let flags = u32::try_from(flags & 0xFFFF_FFFF).unwrap_or(0);
    let name = name.as_ref().map(PhpString::as_bytes);
    let command_ = if commit {
        Command::Commit { flags, name }
    } else {
        Command::Rollback { flags, name }
    };
    command(req, on, Need::Open, command_)
}

/// `mysqli_savepoint` and `mysqli_release_savepoint`.
pub(crate) fn savepoint(
    req: &mut Request,
    on: On,
    name: PhpString,
    release: bool,
) -> Result<bool, Thrown> {
    conn(req, on.object, Need::Open)?;
    if name.is_empty() {
        return Err(arg_value_error(req, on.at(2), "not be empty"));
    }
    let name = Some(name.as_bytes());
    let command_ = if release {
        Command::ReleaseSavepoint(name)
    } else {
        Command::Savepoint(name)
    };
    command(req, on, Need::Open, command_)
}

/// `mysqli_kill`.
pub(crate) fn kill(req: &mut Request, on: On, process_id: i64) -> Result<bool, Thrown> {
    if process_id <= 0 {
        return Err(arg_value_error(req, on.at(2), "be greater than 0"));
    }
    let pid = u32::try_from(process_id & 0xFFFF_FFFF).unwrap_or(0);
    command(req, on, Need::Open, Command::Kill(pid))
}

/// `mysqli_ping`: the error reported whatever the ping answered.
pub(crate) fn ping(req: &mut Request, on: On) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    let pinged = mysql.command(req, Command::Ping)?;
    report_conn(req, mysql)?;
    Ok(pinged)
}

/// `mysqli_change_user`: the error reported whatever the change answered.
pub(crate) fn change_user(
    req: &mut Request,
    on: On,
    user: PhpString,
    password: PhpString,
    database: Option<PhpString>,
) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    let changed = mysql.command(
        req,
        Command::ChangeUser {
            user: Some(user.as_bytes()),
            password: Some(password.as_bytes()),
            db: database.as_ref().map(PhpString::as_bytes),
            silent: false,
        },
    )?;
    report_conn(req, mysql)?;
    Ok(changed)
}

/// `mysqli_refresh`.
pub(crate) fn refresh(req: &mut Request, on: On, flags: i64) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Made)?;
    mysql.command(
        req,
        Command::Refresh(u8::try_from(flags & 0xFF).unwrap_or(0)),
    )
}

/// `mysqli_dump_debug_info`.
pub(crate) fn dump_debug_info(req: &mut Request, on: On) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    mysql.command(req, Command::DumpDebugInfo)
}

/// Whether a connect on a link answered it, the link's owner given up.
pub(crate) fn connected_link(req: &mut Request, made: Option<Owner<PhpObject>>) -> bool {
    let Some(made) = made else {
        return false;
    };
    made.give_up(req);
    true
}

/// `mysqli_stat`.
pub(crate) fn stat(req: &mut Request, on: On) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    match mysql.stat(req)? {
        Some(text) => Ok(OrFalse::Value(PhpString::new(req, &text)?)),
        None => Ok(OrFalse::False),
    }
}

/// `mysqli_real_escape_string`.
pub(crate) fn escape(
    req: &mut Request,
    on: On,
    from: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    let escaped = mysql.escape(req, from.as_bytes());
    PhpString::new(req, &escaped)
}

/// `mysqli_options`: the value converted to the option's type and the
/// option set, the error reported where it is refused; `false` for an
/// option mysqli does not know.
pub(crate) fn options(
    req: &mut Request,
    on: On,
    option: i64,
    value: Value,
) -> Result<bool, Thrown> {
    let mysql = conn(req, on.object, Need::Made)?;
    let long = match option {
        3 | 4 | 5 | 7 | 35 | 43 => 0,
        _ => u32::try_from(to_long(req, &value)? & 0xFFFF_FFFF).unwrap_or(0),
    };
    let as_long = |_: &mut Request| long;
    let text = match option {
        3 | 4 | 5 | 7 | 35 | 43 => Some(with_string(req, &value, <[u8]>::to_vec)?),
        _ => None,
    };
    let text = text.unwrap_or_default();
    let set = match option {
        0 => ClientOption::ConnectTimeout(as_long(req)),
        1 => ClientOption::Compress,
        2 => ClientOption::NamedPipe,
        3 => ClientOption::InitCommand(&text),
        4 => ClientOption::ReadDefaultFile,
        5 => ClientOption::ReadDefaultGroup,
        7 => ClientOption::SetCharsetName(&text),
        8 => ClientOption::LocalInfile(as_long(req) != 0),
        9 => ClientOption::Protocol(as_long(req)),
        11 => ClientOption::ReadTimeout(as_long(req)),
        12 => ClientOption::WriteTimeout(as_long(req)),
        21 => ClientOption::SslVerifyServerCert(Some(as_long(req) != 0)),
        35 => ClientOption::ServerPublicKey(Some(&text)),
        37 => ClientOption::ExpiredPasswords(as_long(req) != 0),
        43 => ClientOption::LoadDataLocalDir(Some(&text)),
        201 => ClientOption::IntAndFloatNative(as_long(req) != 0),
        202 => ClientOption::NetCmdBufferSize(as_long(req)),
        203 => ClientOption::NetReadBufferSize(as_long(req)),
        _ => return Ok(false),
    };
    if !mysql.option(req, set)? {
        report_conn(req, mysql)?;
        return Ok(false);
    }
    Ok(true)
}

/// `mysqli_ssl_set`: an empty argument is none.
pub(crate) fn ssl_set(
    req: &mut Request,
    on: On,
    parts: [Option<PhpString>; 5],
) -> Result<True, Thrown> {
    let mysql = conn(req, on.object, Need::Made)?;
    let parts = parts.map(|p| p.filter(|p| !p.is_empty()));
    let bytes = |at: usize| parts[at].as_ref().map(PhpString::as_bytes);
    mysql.option(req, ClientOption::SslKey(bytes(0)))?;
    mysql.option(req, ClientOption::SslCert(bytes(1)))?;
    mysql.option(req, ClientOption::SslCa(bytes(2)))?;
    mysql.option(req, ClientOption::SslCapath(bytes(3)))?;
    mysql.option(req, ClientOption::SslCipher(bytes(4)))?;
    Ok(True)
}

// ---- what a link answers

/// One of the link's facts, the connection's status read.
pub(crate) fn status(req: &mut Request, on: On) -> Result<php_ext_mysqlnd::client::Status, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    Ok(mysql.status(req))
}

/// `mysqli_get_charset`: the connection's character set as an object.
pub(crate) fn get_charset(req: &mut Request, on: On) -> Result<Option<Owner<PhpObject>>, Thrown> {
    let Some(cs) = status(req, on)?.charset else {
        docref(
            req,
            Level::Warning,
            "The connection has no charset associated",
        );
        return Ok(None);
    };
    let object = crate::std_object(req)?;
    let facts = [
        ("charset", crate::Fact::Text(cs.name.as_bytes())),
        ("collation", crate::Fact::Text(cs.collation.as_bytes())),
        ("dir", crate::Fact::Text(b"")),
        ("min_length", crate::Fact::Int(i64::from(cs.min_length))),
        ("max_length", crate::Fact::Int(i64::from(cs.max_length))),
        ("number", crate::Fact::Int(i64::from(cs.nr))),
        ("state", crate::Fact::Int(1)),
        ("comment", crate::Fact::Text(cs.comment.as_bytes())),
    ];
    if let Err(thrown) = crate::add_facts(req, *object, &facts) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(Some(object))
}

/// `mysqlnd_fill_stats_hash`: each statistic's name with its value as a
/// decimal string.
pub(crate) fn stats_array(
    req: &mut Request,
    stats: &[(&str, u64)],
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (name, value) in stats {
        let value = string(req, value.to_string().as_bytes())?;
        out.set_str(req, name.as_bytes(), value)?;
    }
    Ok(out)
}

/// `mysqli_get_connection_stats`.
pub(crate) fn connection_stats(req: &mut Request, on: On) -> Result<Owner<PhpArray>, Thrown> {
    let mysql = conn(req, on.object, Need::Open)?;
    let stats = mysql.stats(req);
    stats_array(req, &stats)
}

/// The text of a fact the connection may hold none of, `""` for none.
pub(crate) fn text(req: &mut Request, text: Option<&[u8]>) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, text.unwrap_or_default())
}

/// `mysqli_get_links_stats`.
pub(crate) fn links_stats(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let inactive = crate::num_inactive_persistent(req);
    let state = req.state::<State>();
    let counts = [
        ("total", state.num_links),
        ("active_plinks", state.num_active_persistent),
        ("cached_plinks", inactive),
    ];
    let mut out = PhpArray::new(req)?;
    for (name, n) in counts {
        out.set_str(req, name.as_bytes(), Owner::int(n))?;
    }
    Ok(out)
}

// ---- the class

/// `class mysqli`, its properties read through `mysqli_link_property_entries`
/// ([`LinkProps`]).
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = new mysqli('127.0.0.1', 'root', '', 'test');
/// var_dump($l->ping(), $l->select_db('test'), $l->character_set_name(), $l->get_charset()->charset);
/// var_dump($l->query('SELECT 1 AS a')->fetch_assoc(), $l->real_query('SELECT 2'), $l->store_result()->fetch_row());
/// var_dump($l->real_query('SELECT 3'), $l->use_result()->fetch_row(), $l->multi_query('SELECT 4; SELECT 5'));
/// do { if ($r = $l->store_result()) { var_dump($r->fetch_row()[0]); } } while ($l->more_results() && $l->next_result());
/// var_dump($l->autocommit(false), $l->begin_transaction(), $l->savepoint('s'), $l->release_savepoint('s'), $l->commit(), $l->rollback());
/// var_dump($l->escape_string("a'b"), $l->real_escape_string('"'), $l->options(MYSQLI_OPT_INT_AND_FLOAT_NATIVE, 1), $l->set_opt(MYSQLI_INIT_COMMAND, 'SET @a=1'));
/// var_dump($l->set_charset('utf8mb4'), is_string($l->stat()), is_array($l->get_connection_stats()), $l->get_server_info() === $l->server_info);
/// var_dump($l->query('SELECT 1/0'), $l->get_warnings()->message, $l->thread_safe(), get_class($l->stmt_init()), get_class($l->prepare('SELECT 1')));
/// var_dump($l->execute_query('SELECT ? AS x', [7])->fetch_assoc(), $l->dump_debug_info(), $l->debug('d'), $l->ssl_set(null, null, null, null, null));
/// var_dump($l->query('SELECT SLEEP(0)', MYSQLI_ASYNC), get_class($l->reap_async_query()), $l->change_user('root', '', 'test'));
/// var_dump(@$l->kill(0x7fffffff), @$l->refresh(MYSQLI_REFRESH_LOG), $l->get_client_info() === mysqli_get_client_info());
/// $r = $w = []; $x = [$l]; var_dump(mysqli::poll($r, $w, $x, 0));
/// var_dump($l->close());
/// try { $l->query('SELECT 1'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $m = new mysqli(); var_dump($m->real_connect('127.0.0.1', 'root', '', 'test'), $m->connect('127.0.0.1', 'root', '', 'test'));
/// $n = new mysqli(); $n->init(); var_dump($n->real_connect('127.0.0.1', 'root'));
/// try { $n->__construct(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// Deprecated: Method mysqli::ping() is deprecated since 8.4, because the reconnect feature has been removed in PHP 8.2 and this method is now redundant in %s on line 4
/// bool(true)
/// bool(true)
/// string(7) "utf8mb4"
/// string(7) "utf8mb4"
/// array(1) {
///   ["a"]=>
///   string(1) "1"
/// }
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "2"
/// }
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "3"
/// }
/// bool(true)
/// string(1) "4"
/// string(1) "5"
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// string(4) "a\'b"
/// string(2) "\""
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// object(mysqli_result)#2 (5) {
///   ["current_field"]=>
///   int(0)
///   ["field_count"]=>
///   int(1)
///   ["lengths"]=>
///   NULL
///   ["num_rows"]=>
///   int(1)
///   ["type"]=>
///   int(0)
/// }
/// string(13) "Division by 0"
/// bool(true)
/// string(11) "mysqli_stmt"
/// string(11) "mysqli_stmt"
/// array(1) {
///   ["x"]=>
///   string(1) "7"
/// }
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// string(13) "mysqli_result"
/// bool(true)
///
/// Deprecated: Method mysqli::get_client_info() is deprecated since 8.1, use mysqli_get_client_info() instead in %s on line 14
/// bool(false)
/// bool(false)
/// bool(true)
///
/// Warning: mysqli::poll(): No stream arrays were passed in %s on line 15
/// bool(false)
/// bool(true)
/// mysqli object is already closed
/// bool(true)
/// bool(true)
///
/// Deprecated: Method mysqli::init() is deprecated since 8.1, replace calls to parent::init() with parent::__construct() in %s on line 19
/// bool(true)
/// Cannot call constructor twice
/// ```
#[php_class(size = 72, payload = Box<Link>, handlers = LINK)]
pub(crate) struct mysqli;

pub(crate) static LINK: props::Handlers<LinkProps> = props::Handlers::new();

type Answer<T> = Result<T, Thrown>;
type Rows = Union<Owner<Instance<mysqli_result>>, bool>;

#[php_class]
impl mysqli {
    #[php(property)]
    const affected_rows: Union<i64, PhpString>;
    #[php(property)]
    const client_info: PhpString;
    #[php(property)]
    const client_version: i64;
    #[php(property)]
    const connect_errno: i64;
    #[php(property)]
    const connect_error: Option<PhpString>;
    #[php(property)]
    const errno: i64;
    #[php(property)]
    const error: PhpString;
    #[php(property)]
    const error_list: PhpArray;
    #[php(property)]
    const field_count: i64;
    #[php(property)]
    const host_info: PhpString;
    #[php(property)]
    const info: Option<PhpString>;
    #[php(property)]
    const insert_id: Union<i64, PhpString>;
    #[php(property)]
    const server_info: PhpString;
    #[php(property)]
    const server_version: i64;
    #[php(property)]
    const sqlstate: PhpString;
    #[php(property)]
    const protocol_version: i64;
    #[php(property)]
    const thread_id: i64;
    #[php(property)]
    const warning_count: i64;

    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] hostname: Option<PhpString>,
        #[php(default = null)] username: Option<PhpString>,
        #[php(sensitive, default = null)] password: Option<PhpString>,
        #[php(default = null)] database: Option<PhpString>,
        #[php(default = null)] port: Option<i64>,
        #[php(default = null)] socket: Option<PhpString>,
    ) -> Answer<()> {
        let object = this.object();
        if req.argument_count() == 0 {
            if !matches!(
                crate::payload_ref::<Link>(req, object),
                Some(Link::Closed) | None
            ) {
                return Err(throw_error(req, "Error", "Cannot call constructor twice"));
            }
            init(req, Some(object))?.give_up(req);
            return Ok(());
        }
        let args = Args {
            host: hostname,
            user: username,
            password,
            database,
            port,
            socket,
            flags: 0,
        };
        let made = common_connect(req, Some(object), args, false, true)?;
        if let Some(made) = made {
            made.give_up(req);
        }
        Ok(())
    }

    #[php(tentative)]
    fn autocommit(req: &mut Request, this: This<Self>, enable: bool) -> Answer<bool> {
        command(
            req,
            On::method(this.object()),
            Need::Open,
            Command::Autocommit(enable),
        )
    }

    #[php(tentative)]
    fn begin_transaction(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] flags: i64,
        #[php(default = null)] name: Option<PhpString>,
    ) -> Answer<bool> {
        begin_transaction(req, On::method(this.object()), flags, name)
    }

    #[php(tentative)]
    fn change_user(
        req: &mut Request,
        this: This<Self>,
        username: PhpString,
        #[php(sensitive)] password: PhpString,
        database: Option<PhpString>,
    ) -> Answer<bool> {
        change_user(req, On::method(this.object()), username, password, database)
    }

    #[php(tentative)]
    fn character_set_name(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        let cs = status(req, On::method(this.object()))?.charset;
        text(req, cs.map(|c| c.name.as_bytes()))
    }

    #[php(tentative)]
    fn close(req: &mut Request, this: This<Self>) -> Answer<True> {
        close(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn commit(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] flags: i64,
        #[php(default = null)] name: Option<PhpString>,
    ) -> Answer<bool> {
        end_transaction(req, On::method(this.object()), true, flags, name)
    }

    #[php(tentative)]
    fn connect(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] hostname: Option<PhpString>,
        #[php(default = null)] username: Option<PhpString>,
        #[php(sensitive, default = null)] password: Option<PhpString>,
        #[php(default = null)] database: Option<PhpString>,
        #[php(default = null)] port: Option<i64>,
        #[php(default = null)] socket: Option<PhpString>,
    ) -> Answer<bool> {
        let args = Args {
            host: hostname,
            user: username,
            password,
            database,
            port,
            socket,
            flags: 0,
        };
        let made = common_connect(req, Some(this.object()), args, false, false)?;
        Ok(connected_link(req, made))
    }

    #[php(tentative)]
    fn dump_debug_info(req: &mut Request, this: This<Self>) -> Answer<bool> {
        dump_debug_info(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn debug(this: This<Self>, options: PhpString) -> True {
        let _ = (this, options);
        True
    }

    #[php(tentative)]
    fn get_charset(req: &mut Request, this: This<Self>) -> Answer<Option<Owner<PhpObject>>> {
        get_charset(req, On::method(this.object()))
    }

    fn execute_query(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
        #[php(default = null)] params: Option<PhpArray>,
    ) -> Answer<Rows> {
        crate::stmt::execute_query(req, On::method(this.object()), query, params)
    }

    #[php(
        tentative,
        deprecated(since = "8.1", message = "use mysqli_get_client_info() instead")
    )]
    fn get_client_info(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        let _ = this;
        PhpString::new(req, CLIENT_INFO.as_bytes())
    }

    #[php(tentative)]
    fn get_connection_stats(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpArray>> {
        connection_stats(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn get_server_info(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        let info = status(req, On::method(this.object()))?.server_info;
        text(req, info.as_deref())
    }

    #[php(tentative)]
    fn get_warnings(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<OrFalse<Owner<Instance<extras::mysqli_warning>>>> {
        extras::link_warnings(req, On::method(this.object()))
    }

    #[php(deprecated(
        since = "8.1",
        message = "replace calls to parent::init() with parent::__construct()"
    ))]
    fn init(req: &mut Request, this: This<Self>) -> Answer<Untyped<()>> {
        init(req, Some(this.object()))?.give_up(req);
        Ok(Untyped(()))
    }

    #[php(
        tentative,
        deprecated(
            since = "8.4",
            message = "use KILL CONNECTION/QUERY SQL statement instead"
        )
    )]
    fn kill(req: &mut Request, this: This<Self>, process_id: i64) -> Answer<bool> {
        kill(req, On::method(this.object()), process_id)
    }

    #[php(tentative)]
    fn multi_query(req: &mut Request, this: This<Self>, query: PhpString) -> Answer<bool> {
        multi_query(req, On::method(this.object()), query)
    }

    #[php(tentative)]
    fn more_results(req: &mut Request, this: This<Self>) -> Answer<bool> {
        Ok(status(req, On::method(this.object()))?.more_results)
    }

    #[php(tentative)]
    fn next_result(req: &mut Request, this: This<Self>) -> Answer<bool> {
        next_result(req, On::method(this.object()))
    }

    #[php(
        tentative,
        deprecated(
            since = "8.4",
            message = "because the reconnect feature has been removed in PHP 8.2 and this method is now redundant"
        )
    )]
    fn ping(req: &mut Request, this: This<Self>) -> Answer<bool> {
        ping(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn poll(
        req: &mut Request,
        read: php_ext::abi::Ref<Option<PhpArray>>,
        error: php_ext::abi::Ref<Option<PhpArray>>,
        reject: php_ext::abi::Ref<PhpArray>,
        seconds: i64,
        #[php(default = 0)] microseconds: i64,
    ) -> Answer<OrFalse<i64>> {
        extras::poll(req, &read, &error, &reject, seconds, microseconds)
    }

    #[php(tentative)]
    fn prepare(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
    ) -> Answer<OrFalse<Owner<Instance<mysqli_stmt>>>> {
        crate::stmt::prepare(req, On::method(this.object()), query)
    }

    #[php(tentative)]
    fn query(
        req: &mut Request,
        this: This<Self>,
        query: PhpString,
        #[php(default = MYSQLI_STORE_RESULT)] result_mode: i64,
    ) -> Answer<Rows> {
        crate::link::query(req, On::method(this.object()), query, result_mode)
    }

    #[php(tentative)]
    fn real_connect(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] hostname: Option<PhpString>,
        #[php(default = null)] username: Option<PhpString>,
        #[php(sensitive, default = null)] password: Option<PhpString>,
        #[php(default = null)] database: Option<PhpString>,
        #[php(default = null)] port: Option<i64>,
        #[php(default = null)] socket: Option<PhpString>,
        #[php(default = 0)] flags: i64,
    ) -> Answer<bool> {
        let args = Args {
            host: hostname,
            user: username,
            password,
            database,
            port,
            socket,
            flags,
        };
        let made = common_connect(req, Some(this.object()), args, true, false)?;
        Ok(connected_link(req, made))
    }

    #[php(tentative)]
    fn real_escape_string(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
    ) -> Answer<Owner<PhpString>> {
        escape(req, On::method(this.object()), string)
    }

    #[php(tentative)]
    fn reap_async_query(req: &mut Request, this: This<Self>) -> Answer<Rows> {
        reap_async_query(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn escape_string(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
    ) -> Answer<Owner<PhpString>> {
        escape(req, On::method(this.object()), string)
    }

    #[php(tentative)]
    fn real_query(req: &mut Request, this: This<Self>, query: PhpString) -> Answer<bool> {
        real_query(req, On::method(this.object()), query)
    }

    #[php(tentative)]
    fn release_savepoint(req: &mut Request, this: This<Self>, name: PhpString) -> Answer<bool> {
        savepoint(req, On::method(this.object()), name, true)
    }

    #[php(tentative)]
    fn rollback(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] flags: i64,
        #[php(default = null)] name: Option<PhpString>,
    ) -> Answer<bool> {
        end_transaction(req, On::method(this.object()), false, flags, name)
    }

    #[php(tentative)]
    fn savepoint(req: &mut Request, this: This<Self>, name: PhpString) -> Answer<bool> {
        savepoint(req, On::method(this.object()), name, false)
    }

    #[php(tentative)]
    fn select_db(req: &mut Request, this: This<Self>, database: PhpString) -> Answer<bool> {
        let on = On::method(this.object());
        command(req, on, Need::Open, Command::SelectDb(database.as_bytes()))
    }

    #[php(tentative)]
    fn set_charset(req: &mut Request, this: This<Self>, charset: PhpString) -> Answer<bool> {
        let on = On::method(this.object());
        command(req, on, Need::Open, Command::SetCharset(charset.as_bytes()))
    }

    #[php(tentative)]
    fn options(
        req: &mut Request,
        this: This<Self>,
        option: i64,
        value: Untyped<Value>,
    ) -> Answer<bool> {
        options(req, On::method(this.object()), option, value.0)
    }

    #[php(tentative)]
    fn set_opt(
        req: &mut Request,
        this: This<Self>,
        option: i64,
        value: Untyped<Value>,
    ) -> Answer<bool> {
        options(req, On::method(this.object()), option, value.0)
    }

    #[php(tentative)]
    fn ssl_set(
        req: &mut Request,
        this: This<Self>,
        key: Option<PhpString>,
        certificate: Option<PhpString>,
        ca_certificate: Option<PhpString>,
        ca_path: Option<PhpString>,
        cipher_algos: Option<PhpString>,
    ) -> Answer<True> {
        let parts = [key, certificate, ca_certificate, ca_path, cipher_algos];
        ssl_set(req, On::method(this.object()), parts)
    }

    #[php(tentative)]
    fn stat(req: &mut Request, this: This<Self>) -> Answer<OrFalse<Owner<PhpString>>> {
        stat(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn stmt_init(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<OrFalse<Owner<Instance<mysqli_stmt>>>> {
        crate::stmt::stmt_init(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn store_result(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] mode: i64,
    ) -> Answer<OrFalse<Owner<Instance<mysqli_result>>>> {
        let _ = mode;
        let given = req.argument_count() == 1;
        store_result(req, On::method(this.object()), given)
    }

    #[php(tentative)]
    fn thread_safe(this: This<Self>) -> bool {
        let _ = this;
        true
    }

    #[php(tentative)]
    fn use_result(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<OrFalse<Owner<Instance<mysqli_result>>>> {
        take_rows(req, On::method(this.object()), USE)
    }

    #[php(
        tentative,
        deprecated(since = "8.4", message = "use FLUSH SQL statement instead")
    )]
    fn refresh(req: &mut Request, this: This<Self>, flags: i64) -> Answer<bool> {
        refresh(req, On::method(this.object()), flags)
    }
}

/// `mysqli_link_property_entries`.
pub(crate) struct LinkProps;

impl Props for LinkProps {
    const NAMES: &'static [&'static str] = &[
        "affected_rows",
        "client_info",
        "client_version",
        "connect_errno",
        "connect_error",
        "errno",
        "error",
        "error_list",
        "field_count",
        "host_info",
        "info",
        "insert_id",
        "server_info",
        "server_version",
        "sqlstate",
        "protocol_version",
        "thread_id",
        "warning_count",
    ];

    fn read(
        req: &mut Request,
        object: PhpObject,
        name: &str,
        quiet: bool,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        match name {
            "client_info" => {
                return string(req, CLIENT_INFO.as_bytes()).map(Some);
            }
            "client_version" => {
                return Ok(Some(Owner::int(i64::try_from(CLIENT_VERSION).unwrap_or(0))));
            }
            "connect_errno" => {
                let errno = Value::int(req.state::<State>().connect_errno);
                return Ok(Some(Owner::take(req, errno)));
            }
            "connect_error" => {
                return match req.state::<State>().connect_error.clone() {
                    Some(error) => string(req, &error).map(Some),
                    None => Ok(Some(Owner::null())),
                };
            }
            _ => {}
        }
        // `CHECK_STATUS` and `MYSQLI_GET_MYSQL`: errno and error once made,
        // the rest once open.
        // `affected_rows` and `error_list` read `CHECK_STATUS` alone, which
        // says the same of a closed link.
        let made_enough = matches!(name, "errno" | "error");
        let (mysql, open) = match crate::payload_ref::<Link>(req, object) {
            Some(Link::Open(c)) => (c.mysql, true),
            Some(Link::Made(c)) => (c.mysql, false),
            _ if matches!(name, "affected_rows" | "error_list") => {
                return props::not_yet(req, quiet);
            }
            _ => return props::gone(req, object, quiet),
        };
        if !open && !made_enough {
            return props::not_yet(req, quiet);
        }
        let Some(mysql) = mysql else {
            return props::gone(req, object, quiet);
        };
        let s = mysql.status(req);
        let value = match name {
            "affected_rows" => affected(req, s.affected_rows)?,
            "errno" => Owner::int(i64::from(s.errno)),
            "error" => string(req, &s.error)?,
            "error_list" => props::error_list(req, &s.error_list)?.into_value(),
            "field_count" => Owner::int(i64::from(s.field_count)),
            "host_info" => string(req, s.host_info.as_deref().unwrap_or_default())?,
            "info" => match s.info {
                Some(info) => string(req, &info)?,
                None => Owner::null(),
            },
            "insert_id" => long_or_string(req, s.insert_id)?,
            "server_info" => string(req, s.server_info.as_deref().unwrap_or_default())?,
            "server_version" => Owner::int(i64::try_from(s.server_version).unwrap_or(0)),
            "sqlstate" => string(req, &s.sqlstate)?,
            "protocol_version" => Owner::int(i64::from(s.proto_info)),
            "thread_id" => Owner::int(i64::try_from(s.thread_id).unwrap_or(0)),
            "warning_count" => Owner::int(i64::from(s.warning_count)),
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    fn free(req: &mut Request, object: PhpObject) {
        free_link(req, object);
    }
}

/// `mysqli_report($flags)`.
pub(crate) fn set_report_mode(req: &mut Request, flags: i64) {
    req.state::<State>().report_mode = flags;
}

/// `MyG(report_mode)`, for `mysqli_driver`.
pub(crate) fn read_report_mode(req: &mut Request) -> i64 {
    report_mode(req)
}
