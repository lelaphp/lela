//! What is neither link, set of rows nor statement: `mysqli_driver`
//! (`mysqli_driver.c`), `mysqli_warning` (`mysqli_warning.c`),
//! `mysqli_sql_exception` (`mysqli_exception.c`), and `mysqli_poll`
//! (`mysqli_nonapi.c`) over mysqlnd's poll.

#![allow(non_camel_case_types)]

use std::ops::ControlFlow;

use php_ext::abi::{Instance, OrFalse, Ref, This};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{to_string, with_string};
use php_ext::raising::{Thrown, arg_type_error, arg_value_error, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_mysqlnd::client::{CLIENT_INFO, CLIENT_VERSION, Mysqlnd, ResultSet};

use crate::link::{self, Link, Need};
use crate::props::{self, Props};
use crate::{On, Resource, State, VERSION_ID, string};

// ---- mysqli_driver

/// `final class mysqli_driver`, its properties read through
/// `mysqli_driver_property_entries` ([`DriverProps`]).
///
/// ```php
/// $d = new mysqli_driver;
/// var_dump($d->client_info, $d->client_version, $d->report_mode);
/// $d->report_mode = MYSQLI_REPORT_OFF; var_dump($d->report_mode, $d->driver_version);
/// try { $d->client_info = 'x'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($d);
/// ```
/// ```expect
/// string(13) "mysqlnd 8.5.9"
/// int(80509)
/// int(3)
///
/// Deprecated: The driver_version property is deprecated in %s on line 4
/// int(0)
/// int(101009)
/// Cannot write read-only property mysqli_driver::$client_info
/// object(mysqli_driver)#1 (3) {
///   ["client_info"]=>
///   string(13) "mysqlnd 8.5.9"
///   ["client_version"]=>
///   int(80509)
///   ["report_mode"]=>
///   int(0)
/// }
/// ```
#[php_class(final, size = 72, payload = (), handlers = DRIVER)]
pub(crate) struct mysqli_driver;

pub(crate) static DRIVER: props::Handlers<DriverProps> = props::Handlers::new();

#[php_class]
impl mysqli_driver {
    #[php(property)]
    const client_info: PhpString;
    #[php(property)]
    const client_version: i64;
    #[php(property)]
    const driver_version: i64;
    #[php(property)]
    const report_mode: i64 = 0;
}

/// `mysqli_driver_property_entries`.
pub(crate) struct DriverProps;

impl Props for DriverProps {
    const NAMES: &'static [&'static str] = &[
        "client_info",
        "client_version",
        "driver_version",
        "report_mode",
    ];

    fn read(
        req: &mut Request,
        _: PhpObject,
        name: &str,
        quiet: bool,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        Ok(Some(match name {
            "client_info" => string(req, CLIENT_INFO.as_bytes())?,
            "client_version" => Owner::int(i64::try_from(CLIENT_VERSION).unwrap_or(0)),
            "driver_version" => {
                if quiet {
                    return Ok(None);
                }
                php_ext::raising::deprecated(req, "The driver_version property is deprecated");
                Owner::int(VERSION_ID)
            }
            "report_mode" => {
                let mode = Value::int(link::read_report_mode(req));
                Owner::take(req, mode)
            }
            _ => return Ok(None),
        }))
    }

    const WRITABLE: &'static [&'static str] = &["report_mode"];

    /// `driver_report_write`: the `int` the standard handler stored.
    fn wrote(req: &mut Request, object: PhpObject, _: &str) -> Result<(), Thrown> {
        let mode = object
            .prop(req, b"report_mode")
            .and_then(|v| v.deref().as_int())
            .unwrap_or(0);
        link::set_report_mode(req, mode);
        Ok(())
    }

    fn free(_: &mut Request, _: PhpObject) {}
}

// ---- mysqli_warning

/// `sizeof(MYSQLI_WARNING)`.
const MYSQLI_WARNING: usize = 48;

/// One row of `SHOW WARNINGS`, `MYSQLI_WARNING` in the allocation php
/// `ecalloc`s for it: the row's reason, one more owner of its string
/// (`ZVAL_COPY` and `convert_to_string`), the `"HY000"` php makes for each
/// and the code.
pub(crate) struct Warning {
    reason: Owner<Value>,
    sqlstate: Owner<Value>,
    errno: i64,
    held: Emalloc,
}

impl Warning {
    /// `php_new_warning`; the `Err` is the memory limit's fatal.
    fn new(req: &mut Request, reason: Owner<Value>, errno: i64) -> Result<Warning, Thrown> {
        let held = Emalloc::new(req, MYSQLI_WARNING)?;
        Ok(Warning {
            reason,
            sqlstate: PhpString::new(req, b"HY000")?.into_value(),
            errno,
            held,
        })
    }
}

/// The warnings, `info`, and the one `ptr` stands at.
pub(crate) struct List {
    list: Vec<Warning>,
    at: usize,
}

/// A `mysqli_warning`'s payload: the warnings behind their
/// `MYSQLI_RESOURCE`, none once php's `ptr` is gone.
#[derive(Default)]
pub(crate) struct Warnings(Option<Resource<List>>);

/// `final class mysqli_warning`, made only by `get_warnings`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = new mysqli('127.0.0.1', 'root', '', 'test');
/// $l->query('SELECT 1/0, CAST("x" AS SIGNED)');
/// $w = $l->get_warnings();
/// do { var_dump($w->message, $w->sqlstate, $w->errno); } while ($w->next());
/// var_dump($w->next(), $w);
/// try { new mysqli_warning; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionMethod('mysqli_warning', '__construct'))->invoke($w); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// string(13) "Division by 0"
/// string(5) "HY000"
/// int(1365)
/// string(38) "Truncated incorrect INTEGER value: 'x'"
/// string(5) "HY000"
/// int(1292)
/// bool(false)
/// object(mysqli_warning)#2 (3) {
///   ["message"]=>
///   string(38) "Truncated incorrect INTEGER value: 'x'"
///   ["sqlstate"]=>
///   string(5) "HY000"
///   ["errno"]=>
///   int(1292)
/// }
/// Call to private mysqli_warning::__construct() from global scope
/// Cannot directly construct mysqli_warning
/// ```
#[php_class(final, size = 72, payload = Box<Warnings>, handlers = WARNING)]
pub(crate) struct mysqli_warning;

pub(crate) static WARNING: props::Handlers<WarningProps> = props::Handlers::new();

#[php_class]
impl mysqli_warning {
    #[php(property)]
    const message: PhpString;
    #[php(property)]
    const sqlstate: PhpString;
    #[php(property)]
    const errno: i64;

    #[php(private)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        Err(throw_error(
            req,
            "Error",
            "Cannot directly construct mysqli_warning",
        ))
    }

    fn next(req: &mut Request, this: This<Self>) -> bool {
        let Some(Warnings(Some(w))) = crate::payload::<Warnings>(req, this.object()) else {
            return false;
        };
        if w.at + 1 < w.list.len() {
            w.at += 1;
            return true;
        }
        false
    }
}

/// `mysqli_warning_property_entries`.
pub(crate) struct WarningProps;

impl Props for WarningProps {
    const NAMES: &'static [&'static str] = &["message", "sqlstate", "errno"];

    fn read(
        req: &mut Request,
        object: PhpObject,
        name: &str,
        quiet: bool,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let at = match crate::payload_ref::<Warnings>(req, object) {
            Some(Warnings(Some(w))) => w.list.get(w.at),
            _ => None,
        };
        let Some(w) = at else {
            return props::gone(req, object, quiet);
        };
        // `ZVAL_COPY(retval, &w->reason)` and the like: one more owner.
        let value = match name {
            "message" => *w.reason,
            "sqlstate" => *w.sqlstate,
            _ => Value::int(w.errno),
        };
        Ok(Some(Owner::take(req, value)))
    }

    /// `mysqli_warning_free_storage`: `php_clear_warnings`, each
    /// warning's strings released and the warning freed, then the
    /// resource.
    fn free(req: &mut Request, object: PhpObject) {
        let held = crate::payload::<Warnings>(req, object).and_then(|w| w.0.take());
        if let Some(resource) = held {
            let list = resource.free(req).list;
            clear(req, list);
        }
    }
}

/// `php_clear_warnings`.
fn clear(req: &mut Request, list: Vec<Warning>) {
    for w in list {
        w.reason.give_up(req);
        w.sqlstate.give_up(req);
        w.held.free(req);
    }
}

/// `php_get_warnings`: `SHOW WARNINGS` read off the connection, each row's
/// code and message.
fn read_warnings(req: &mut Request, conn: Mysqlnd) -> Result<Vec<Warning>, Thrown> {
    if !conn.query(req, b"SHOW WARNINGS")? {
        return Ok(Vec::new());
    }
    let Some(mut set) = conn.use_unbuffered(req)? else {
        return Ok(Vec::new());
    };
    let mut list = Vec::new();
    loop {
        match read_warning(req, &mut set) {
            Ok(Some(warning)) => list.push(warning),
            Ok(None) => break,
            Err(thrown) => {
                clear(req, list);
                return Err(thrown);
            }
        }
    }
    if let Err(thrown) = set.free(req, false) {
        clear(req, list);
        return Err(thrown);
    }
    Ok(list)
}

/// One row of `SHOW WARNINGS` made a warning, `None` past the last.
fn read_warning(req: &mut Request, set: &mut ResultSet) -> Result<Option<Warning>, Thrown> {
    let row = set.fetch_into(req, 2)?;
    let Some(array) = row.as_array() else {
        row.give_up(req);
        return Ok(None);
    };
    // `zend_hash_internal_pointer_reset`, `zend_hash_move_forward`
    // and `zend_hash_get_current_data` (`Z/ext/mysqli/mysqli_warning.c:83`)
    // over the row this native owns, a list: its second and third
    // cells, looked up when each is read.
    let cell = |at: i64| array.get(&Key::Int(at)).map(|v| v.deref());
    let warning = cell(1)
        .map_or(Ok(0), |v| php_ext::coerce::to_long(req, &v))
        .and_then(|errno| {
            // `ZVAL_COPY(&w->reason, reason)` and `convert_to_string`.
            let reason = match cell(2) {
                Some(v) if v.as_str_bytes().is_some() => Owner::take(req, v),
                Some(v) => {
                    let bytes = with_string(req, &v, <[u8]>::to_vec)?;
                    PhpString::new(req, &bytes)?.into_value()
                }
                None => PhpString::new(req, b"")?.into_value(),
            };
            Warning::new(req, reason, errno)
        });
    row.give_up(req);
    warning.map(Some)
}

/// The warnings on a new `mysqli_warning`, `false` where there are none:
/// each `MYSQLI_WARNING` with its reason and its `"HY000"`, behind the
/// object's resource, given back with the object, the reason read off it
/// kept by its reader:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $l->query('SELECT 1/0, CAST("x" AS SIGNED)');
/// $l->get_warnings();
/// $l->query('SELECT 1/0, CAST("x" AS SIGNED)');
/// $m = memory_get_usage();
/// $w = $l->get_warnings();
/// echo memory_get_usage() - $m, "\n";
/// $message = $w->message;
/// $w->next();
/// echo memory_get_usage() - $m, "\n";
/// unset($w);
/// echo memory_get_usage() - $m, "\n";
/// unset($message);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql
/// 400
/// 432
/// 72
/// 32
/// ```
fn warnings(
    req: &mut Request,
    conn: Mysqlnd,
    count: u32,
) -> Result<OrFalse<Owner<Instance<mysqli_warning>>>, Thrown> {
    if count == 0 {
        return Ok(OrFalse::False);
    }
    let list = read_warnings(req, conn)?;
    if list.is_empty() {
        return Ok(OrFalse::False);
    }
    let class = lookup_class(req, b"mysqli_warning")?
        .ok_or_else(|| fatal(req, "InternalError: mysqli_warning is not linked"))?;
    let resource = Resource::new(req, List { list, at: 0 })?;
    let object = new_object(req, class)?;
    if let Some(w) = crate::payload::<Warnings>(req, *object) {
        *w = Warnings(Some(resource));
    }
    Ok(OrFalse::Value(object.into_instance()))
}

/// `mysqli_get_warnings`.
pub(crate) fn link_warnings(
    req: &mut Request,
    on: On,
) -> Result<OrFalse<Owner<Instance<mysqli_warning>>>, Thrown> {
    let conn = link::conn(req, on.object, Need::Open)?;
    let count = conn.status(req).warning_count;
    warnings(req, conn, count)
}

/// `mysqli_stmt_get_warnings`.
pub(crate) fn stmt_warnings(
    req: &mut Request,
    on: On,
) -> Result<OrFalse<Owner<Instance<mysqli_warning>>>, Thrown> {
    let (conn, count) = crate::stmt::connection(req, on)?;
    warnings(req, conn, count)
}

// ---- mysqli_sql_exception

/// `class RuntimeException`, SPL's.
#[php_class(extern)]
pub(crate) struct RuntimeException;

/// `final class mysqli_sql_exception extends RuntimeException`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_ERROR | MYSQLI_REPORT_STRICT);
/// try { new mysqli('127.0.0.1', 'nobody', 'nothing'); } catch (mysqli_sql_exception $e) { var_dump(get_class($e), $e->getSqlState(), $e->getCode()); }
/// $l = new mysqli('127.0.0.1', 'root', '', 'test');
/// try { $l->query('SELECT nope'); } catch (mysqli_sql_exception $e) { var_dump($e->getMessage(), $e->getSqlState(), $e->getCode()); }
/// ```
/// ```expect mysql
/// string(20) "mysqli_sql_exception"
/// string(5) "HY000"
/// int(1045)
/// string(37) "Unknown column 'nope' in 'field list'"
/// string(5) "42S22"
/// int(1054)
/// ```
#[php_class(final, extends(RuntimeException))]
pub(crate) struct mysqli_sql_exception;

#[php_class]
impl mysqli_sql_exception {
    #[php(property, protected)]
    const sqlstate: PhpString = "00000";

    fn getSqlState(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let value = this.object().prop(req, b"sqlstate").unwrap_or_default();
        to_string(req, &value.deref())
    }
}

// ---- poll

/// `mysqlnd_zval_array_to_mysqlnd_array`: each element of the array a
/// connected link, php's `TypeError` or `Error` naming the first that is
/// not.
fn links_of(req: &mut Request, array: PhpArray) -> Result<Vec<(Value, Mysqlnd)>, Thrown> {
    let mut out = Vec::new();
    let class = link::link_class(req)?;
    let mut i = 0usize;
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(in_array), elem)`
    // (`Z/ext/mysqli/mysqli_nonapi.c:658`), no owner of the table taken:
    // the argument holds it while an error reaches the program.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        i += 1;
        let element = row.value().deref();
        let object = match element.as_ref() {
            ValueRef::Object(o) if o.instance_of(req, class) => o,
            _ => {
                let given = php_ext::coerce::value_name(req, &element);
                let n = u32::try_from(i).unwrap_or(u32::MAX);
                return Err(arg_type_error(
                    req,
                    n,
                    &format!("must be an instance of mysqli, {given} given"),
                ));
            }
        };
        let conn = match crate::payload_ref::<Link>(req, object) {
            Some(Link::Open(c)) => c.mysql,
            Some(Link::Made(_)) => return Err(crate::not_initialized(req, object)),
            _ => return Err(crate::closed(req, object)),
        };
        if let Some(conn) = conn {
            out.push((element, conn));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// The links of `links` whose connection `kept` lists, in order, as a new
/// array: `mysqlnd_zval_array_from_mysqlnd_array`.
fn array_of(
    req: &mut Request,
    links: &[(Value, Mysqlnd)],
    kept: &[Mysqlnd],
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (value, conn) in links {
        if kept.contains(conn) {
            out.append(req, Owner::take(req, *value))?;
        }
    }
    Ok(out)
}

/// `mysqli_poll`: the links of `$read` and `$error` polled through
/// mysqlnd, each array cut to the ready ones and `$reject` set to the read
/// links with no query sent.
pub(crate) fn poll(
    req: &mut Request,
    read: &Ref<Option<PhpArray>>,
    error: &Ref<Option<PhpArray>>,
    reject: &Ref<PhpArray>,
    seconds: i64,
    microseconds: i64,
) -> Result<OrFalse<i64>, Thrown> {
    if seconds < 0 {
        return Err(arg_value_error(req, 4, "be greater than or equal to 0"));
    }
    if microseconds < 0 {
        return Err(arg_value_error(req, 5, "be greater than or equal to 0"));
    }
    let read_in = read.get().copied().flatten();
    let error_in = error.get().copied().flatten();
    if read_in.is_none() && error_in.is_none() {
        return Err(throw_error(
            req,
            "ValueError",
            "No stream arrays were passed",
        ));
    }
    let read_links = match read_in {
        Some(array) => Some(links_of(req, array)?),
        None => None,
    };
    let error_links = match error_in {
        Some(array) => Some(links_of(req, array)?),
        None => None,
    };
    let conns = |links: &Option<Vec<(Value, Mysqlnd)>>| {
        links
            .as_ref()
            .map(|l| l.iter().map(|(_, c)| *c).collect::<Vec<_>>())
    };
    let (mut read_conns, mut error_conns) = (conns(&read_links), conns(&error_links));
    let timeout = std::time::Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
        + std::time::Duration::from_micros(u64::try_from(microseconds).unwrap_or(0));
    let (ready, dont_poll) = Mysqlnd::poll(req, read_conns.as_mut(), error_conns.as_mut(), timeout);
    let rejected = match &read_links {
        Some(links) => array_of(req, links, &dont_poll)?,
        None => PhpArray::new(req)?,
    };
    reject.set(req, rejected)?;
    if let (Some(links), Some(kept)) = (&read_links, &read_conns) {
        let ready = array_of(req, links, kept)?;
        read.set(req, Some(ready))?;
    }
    if let (Some(links), Some(kept)) = (&error_links, &error_conns) {
        let ready = array_of(req, links, kept)?;
        error.set(req, Some(ready))?;
    }
    Ok(match ready {
        Some(n) => OrFalse::Value(i64::try_from(n).unwrap_or(i64::MAX)),
        None => OrFalse::False,
    })
}

/// `mysqli_get_client_stats`.
pub(crate) fn client_stats(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let stats = Mysqlnd::client_stats(req);
    link::stats_array(req, &stats)
}

/// `mysqli_report`.
pub(crate) fn set_report(req: &mut Request, flags: i64) {
    req.state::<State>().report_mode = flags;
}
