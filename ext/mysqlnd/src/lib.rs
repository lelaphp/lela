//! The `mysqlnd` extension: `php-src/ext/mysqlnd` as one crate under
//! `ext/` (ADR-0004), php's `MySQL` native client ported over
//! `mysql_common`'s scrambles, every byte through the engine's streams
//! (#1345). It declares no native, as php's declares none: its entry is
//! `extension()`, its `mysqlnd.*` ini entries and its `MINFO`; and it
//! exports [`client`], the connection, rows and statement the extensions
//! that include `mysqlnd.h` reach, `pdo_mysql` and `mysqli`.
//!
//! **Where a connection lives.** php's `MYSQLND` is a pointer the
//! connection's owner and every statement and unbuffered set of rows of it
//! share, counted by `get_reference`. Here the connection data is held in
//! the request's table of this extension, under a [`client::Mysqlnd`]
//! number its owners copy and count, and lent out of the table to one
//! call at a time, so nothing is locked or shared between threads (ADR-0005).
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod alloc;
mod auth;
mod charset;
pub mod client;
mod codec;
mod conn;
mod consts;
mod error;
mod loaddata;
mod net;
mod pool;
mod ps;
mod result;
mod stats;
mod wire;

use php_ext::abi::{
    Dependency, DependencyKind, EngineState, Extension, ExtensionBuilder, LongHandler,
};
use php_ext::decl::IniDecl;
use php_ext::raising::Thrown;
use php_ext::request::{Ini, Request};

use crate::conn::{ConnData, IdleConn};

/// `PHP_MYSQLND_VERSION`, `"mysqlnd " PHP_VERSION`.
const VERSION: &str = "mysqlnd 8.5.12";

/// The `mysqlnd.*` entries, `PHP_INI_BEGIN` in php's order: the name,
/// the default, and who may set it.
const INI: [(&str, Option<&str>, Scope); 10] = [
    ("mysqlnd.collect_statistics", Some("1"), Scope::All),
    (
        "mysqlnd.collect_memory_statistics",
        Some("0"),
        Scope::System,
    ),
    ("mysqlnd.debug", None, Scope::System),
    ("mysqlnd.trace_alloc", None, Scope::System),
    ("mysqlnd.net_cmd_buffer_size", Some("4096"), Scope::All),
    ("mysqlnd.net_read_buffer_size", Some("32768"), Scope::All),
    ("mysqlnd.net_read_timeout", Some("86400"), Scope::All),
    ("mysqlnd.log_mask", Some("0"), Scope::All),
    ("mysqlnd.mempool_default_size", Some("16000"), Scope::All),
    ("mysqlnd.sha256_server_public_key", None, Scope::PerDir),
];

/// `PHP_INI_ALL`, `PHP_INI_SYSTEM` and `PHP_INI_PERDIR`.
#[derive(Clone, Copy)]
enum Scope {
    All,
    System,
    PerDir,
}

/// The request's state: the connections, lent out to one call at a
/// time, `mysqlnd_global_stats`, and `MYSQLND_G(collect_statistics)`,
/// the entry as `OnUpdateBool` keeps it ([`on_update_collect_statistics`]).
#[derive(Default)]
struct State {
    conns: Vec<Slot>,
    stats: Vec<u64>,
    collect_statistics: bool,
}

/// One connection of the table: its data, absent while it is lent; the
/// owners that hold its number; the generation a reused slot moves on.
#[derive(Default)]
struct Slot {
    conn: Option<Box<ConnData>>,
    refs: usize,
    generation: u32,
}

/// `MYSQLND_REVERSE_API`: an extension built over mysqlnd, as
/// `mysqlnd_reverse_api_register_api` keeps it and `MINFO` lists it under
/// "API Extensions". An extension hands one over with
/// `ExtensionBuilder::register`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub struct ReverseApi {
    pub extension: &'static str,
}

/// The extension value (design §7.1): the ini entries, `standard` as
/// the one dependency, the request's table and the `MINFO` table.
///
/// ```php
/// var_dump(extension_loaded('mysqlnd'), ini_get('mysqlnd.net_cmd_buffer_size'), ini_get('mysqlnd.collect_statistics'), (new ReflectionExtension('mysqlnd'))->getVersion());
/// ```
/// ```expect
/// bool(true)
/// string(4) "4096"
/// string(1) "1"
/// string(14) "mysqlnd 8.5.12"
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![Dependency {
            name: String::from("standard"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .ini(
            INI.iter()
                .map(|(name, default, scope)| IniDecl {
                    name: name.as_bytes(),
                    default: default.map(str::as_bytes),
                    user: matches!(scope, Scope::All),
                    per_dir: matches!(scope, Scope::All | Scope::PerDir),
                    system: !matches!(scope, Scope::PerDir),
                })
                .collect(),
        )
        .on_update_long(b"mysqlnd.net_read_buffer_size", LongHandler::Any)
        .on_update_long(b"mysqlnd.net_read_timeout", LongHandler::Any)
        .on_update_long(b"mysqlnd.log_mask", LongHandler::Any)
        .on_update_long(b"mysqlnd.mempool_default_size", LongHandler::Any)
        .on_modify(COLLECT_STATISTICS, on_update_collect_statistics)
        .request_state::<State>()
        .rinit(rinit)
        .info(mysqlnd_info)
        .version(VERSION)
        .build("mysqlnd")
}

/// `PHP_MINFO_FUNCTION(mysqlnd)`.
///
/// ```php
/// ob_start(); phpinfo(INFO_MODULES); $info = ob_get_clean();
/// $at = strpos($info, "\nmysqlnd\n");
/// echo substr($info, $at, strpos($info, "Loaded plugins", $at) - $at);
/// ```
/// ```expect
///
/// mysqlnd
///
/// mysqlnd => enabled
/// Version => mysqlnd 8.5.12
/// Compression => supported
/// core SSL => supported
/// extended SSL => supported
/// Command buffer size => 4096
/// Read buffer size => 32768
/// Read timeout => 86400
/// Collecting statistics => Yes
/// Collecting memory statistics => No
/// Tracing => n/a
/// ```
///
/// The four `OnUpdateLong` entries are read by the quantity rule, with its
/// warning where it interprets a value:
///
/// ```php
/// $code = 'ob_start(); phpinfo(INFO_MODULES); preg_match_all("/^Read (buffer size|timeout) => .*$/m", ob_get_clean(), $m); echo implode("\n", $m[0]), "\n"; var_dump(ini_set("mysqlnd.log_mask", "1y"));';
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'mysqlnd.net_read_buffer_size=1K', '-d', 'mysqlnd.net_read_timeout=0x10', '-d', 'mysqlnd.mempool_default_size=16000z', '-r', $code], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
/// ```
/// ```expect process
/// Warning: Invalid "mysqlnd.mempool_default_size" setting. Invalid quantity "16000z": unknown multiplier "z", interpreting as "16000" for backwards compatibility in Unknown on line 0
/// Read buffer size => 1024
/// Read timeout => 16
///
/// Warning: Invalid "mysqlnd.log_mask" setting. Invalid quantity "1y": unknown multiplier "y", interpreting as "1" for backwards compatibility in Command line code on line 1
/// string(1) "0"
/// 0
/// ```
fn mysqlnd_info(req: &mut Request) {
    let long = |req: &Request, name: &[u8]| -> String {
        req.ini()
            .get(name)
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .to_string()
    };
    let yes = |req: &Request, name: &[u8]| -> &'static str {
        if ini_bool(req, name) { "Yes" } else { "No" }
    };
    let cmd = long(req, b"mysqlnd.net_cmd_buffer_size");
    // The two `OnUpdateLong` entries, as their quantities.
    let read = req.ini().long(b"mysqlnd.net_read_buffer_size").to_string();
    let timeout = req.ini().long(b"mysqlnd.net_read_timeout").to_string();
    let stats = yes(req, COLLECT_STATISTICS);
    let memory = yes(req, b"mysqlnd.collect_memory_statistics");
    let tracing = req.ini().value(b"mysqlnd.debug").map_or_else(
        || "n/a".to_owned(),
        |v| String::from_utf8_lossy(v).into_owned(),
    );
    let apis: Vec<&str> = req
        .registered_by::<ReverseApi>()
        .map(|(_, api)| api.extension)
        .collect();
    let apis = apis.join(",");
    req.extensions_mut().info_table(&[
        &["mysqlnd", "enabled"],
        &["Version", VERSION],
        &["Compression", "supported"],
        &["core SSL", "supported"],
        &["extended SSL", "supported"],
        &["Command buffer size", &cmd],
        &["Read buffer size", &read],
        &["Read timeout", &timeout],
        &["Collecting statistics", stats],
        &["Collecting memory statistics", memory],
        &["Tracing", &tracing],
        &["Loaded plugins", auth::Plugin::LOADED],
        &["API Extensions", &apis],
    ]);
}

/// The table's number for one connection: its slot and the generation it
/// was made in, so a number outliving its connection names nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Handle {
    slot: u32,
    generation: u32,
}

impl Handle {
    /// `mysqlnd_init`: a connection made and held in the table, one owner
    /// counted; the `Err` is the memory limit's fatal.
    pub(crate) fn alloc(req: &mut Request, persistent: bool) -> Result<Handle, Thrown> {
        let conn = Box::new(ConnData::new(req, persistent)?);
        Ok(Handle::hold(req, conn))
    }

    /// `conn` held in the table, one owner counted.
    fn hold(req: &mut Request, conn: Box<ConnData>) -> Handle {
        let state = req.state::<State>();
        let at = if let Some(at) = state
            .conns
            .iter()
            .position(|s| s.refs == 0 && s.conn.is_none())
        {
            at
        } else {
            state.conns.push(Slot::default());
            state.conns.len() - 1
        };
        let generation = match state.conns.get_mut(at) {
            Some(entry) => {
                entry.generation = entry.generation.wrapping_add(1);
                entry.conn = Some(conn);
                entry.refs = 1;
                entry.generation
            }
            None => 0,
        };
        Self {
            slot: u32::try_from(at).unwrap_or(u32::MAX),
            generation,
        }
    }

    /// The connection lent to `f` for one call and put back: `None` where
    /// the number names no connection or the connection is lent already.
    pub(crate) fn with<R>(
        self,
        req: &mut Request,
        f: impl FnOnce(&mut Request, &mut ConnData) -> R,
    ) -> Option<R> {
        let state = req.state::<State>();
        let enabled = state.collect_statistics;
        let mut conn = slot_of(state, self)?.conn.take()?;
        conn.stats.enabled = enabled;
        let answered = f(req, &mut conn);
        let state = req.state::<State>();
        flush(state, &mut conn);
        if let Some(slot) = slot_of(state, self) {
            slot.conn = Some(conn);
        }
        Some(answered)
    }

    /// The connection read where it stands, for what reads and counts
    /// nothing: no lend, so no statistic is switched on or flushed and
    /// `mysqlnd.collect_statistics` is not read; `None` where the number
    /// names no connection or the connection is lent already, as for
    /// [`Handle::with`]. The accessors read through it, so reading the
    /// connection's facts counts nothing:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// mysqli_query($l, 'SELEC 1');
    /// $before = mysqli_get_connection_stats($l);
    /// $facts = [mysqli_errno($l), mysqli_sqlstate($l), mysqli_affected_rows($l), mysqli_warning_count($l), mysqli_real_escape_string($l, "a'b"), mysqli_more_results($l), count(mysqli_error_list($l))];
    /// echo json_encode($facts), "\n";
    /// var_dump(array_diff_assoc(mysqli_get_connection_stats($l), $before));
    /// ```
    /// ```expect mysql
    /// [1064,"42000",-1,0,"a\\'b",false,1]
    /// array(0) {
    /// }
    /// ```
    pub(crate) fn peek<R>(self, req: &mut Request, f: impl FnOnce(&ConnData) -> R) -> Option<R> {
        slot_of(req.state::<State>(), self)?.conn.as_deref().map(f)
    }

    /// `get_reference`: one more owner of the connection.
    pub(crate) fn acquire(self, req: &mut Request) {
        if let Some(slot) = slot_of(req.state::<State>(), self) {
            slot.refs += 1;
        }
    }

    /// `free_reference`: one owner fewer, and the last one's close sent and
    /// the connection dropped, counted as `mysqlnd.collect_statistics`
    /// stands when it is:
    ///
    /// ```php
    /// $p = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $r = mysqli_query($l, "SELECT 1 UNION ALL SELECT 2", MYSQLI_USE_RESULT);
    /// ini_set('mysqlnd.collect_statistics', '0');
    /// $before = mysqli_get_client_stats();
    /// unset($p);
    /// $l = null;
    /// $r = null;
    /// echo json_encode(array_diff_assoc(mysqli_get_client_stats(), $before)), "\n";
    /// ```
    /// ```expect mysql
    /// []
    /// ```
    pub(crate) fn release(self, req: &mut Request) {
        let state = req.state::<State>();
        let enabled = state.collect_statistics;
        let taken = match slot_of(state, self) {
            Some(slot) => {
                slot.refs -= 1;
                if slot.refs == 0 {
                    slot.conn.take()
                } else {
                    None
                }
            }
            None => None,
        };
        if let Some(mut conn) = taken {
            // The close counts under the entry as it stands now, as a
            // lend's call does, not as the last lend left it.
            conn.stats.enabled = enabled;
            conn.send_close(req).ok();
            flush(req.state::<State>(), &mut conn);
            conn.dtor(req);
        }
    }
}

impl Handle {
    /// The connection taken out of the table, this owner gone, with its
    /// socket kept by the connection itself, out of the request and out of
    /// the engine's persistent list ([`net::Net::give_back`]): what its
    /// owner keeps in its own pool, the one list a persistent connection
    /// waits in, as php's mysqlnd keeps none of its own. `None` where the
    /// number names none or the connection is not a persistent one that
    /// connected.
    pub(crate) fn give_back(self, req: &mut Request) -> Option<IdleConn> {
        let slot = slot_of(req.state::<State>(), self)?;
        slot.conn.as_ref()?.net.persistent_id.as_ref()?;
        let conn = slot.conn.take()?;
        slot.refs -= 1;
        Some(conn.idle(req))
    }

    /// A connection a request gave back held in the table again, one
    /// owner counted, over the socket it kept.
    pub(crate) fn take_up(req: &mut Request, idle: IdleConn) -> Result<Handle, Thrown> {
        let conn = Box::new(ConnData::take_up(req, idle)?);
        Ok(Handle::hold(req, conn))
    }
}

/// The number's slot while it names the connection it was made for.
fn slot_of(state: &mut State, handle: Handle) -> Option<&mut Slot> {
    state
        .conns
        .get_mut(handle.slot as usize)
        .filter(|s| s.generation == handle.generation && s.refs > 0)
}

/// What a connection counted for the request's statistics added there.
fn flush(state: &mut State, conn: &mut ConnData) {
    state.stats.resize(stats::COUNT, 0);
    conn.stats.flush(&mut state.stats);
}

impl State {
    /// `mysqlnd_global_stats`, the request's.
    pub(crate) fn global_stats(req: &mut Request) -> Vec<u64> {
        let state = req.state::<State>();
        state.stats.resize(stats::COUNT, 0);
        state.stats.clone()
    }
}

/// A boolean entry as `OnUpdateBool` reads it.
fn ini_bool(req: &Request, name: &[u8]) -> bool {
    req.ini().get(name).is_some_and(Ini::parse_bool)
}

/// The entry whose value decides whether a connection counts.
const COLLECT_STATISTICS: &[u8] = b"mysqlnd.collect_statistics";

/// The request's `MYSQLND_G(collect_statistics)` laid from the entry as
/// the request starts with it, as `MYSQLND_G` holds the value the entry
/// was registered with.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `rinit` slot's shape, which a hook that throws needs"
)]
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.state::<State>().collect_statistics = ini_bool(req, COLLECT_STATISTICS);
    Ok(())
}

/// `OnUpdateBool` over `MYSQLND_G(collect_statistics)`: the value read
/// as `zend_ini_parse_bool` reads it, written where every lend of a
/// connection reads it, at `ini_set()`, `ini_restore()` and the
/// request's end alike. A connection counts what it does while the entry
/// is on and nothing while it is off:
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $rows = fn () => mysqli_get_connection_stats($l)['buffered_sets'];
/// $fetch = function () use ($l) { $r = mysqli_query($l, 'SELECT 1'); mysqli_fetch_row($r); };
/// $fetch(); echo $rows(), "\n";
/// var_dump(ini_set('mysqlnd.collect_statistics', '0')); $fetch(); echo $rows(), "\n";
/// ini_restore('mysqlnd.collect_statistics'); var_dump(ini_get('mysqlnd.collect_statistics')); $fetch(); echo $rows(), "\n";
/// ```
/// ```expect mysql
/// 1
/// string(1) "1"
/// 1
/// string(1) "1"
/// 2
/// ```
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_collect_statistics(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    req.state::<State>().collect_statistics = value.is_some_and(Ini::parse_bool);
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_version_is_mysqlnd_and_php_s() {
        assert_eq!(
            super::VERSION,
            format!("mysqlnd {}", php_ext::request::PHP_VERSION)
        );
    }
}
