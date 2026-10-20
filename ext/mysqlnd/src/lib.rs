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

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder, LongHandler};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::{Ini, Request};

use crate::conn::ConnData;

/// `PHP_MYSQLND_VERSION`, `"mysqlnd " PHP_VERSION`.
const VERSION: &str = "mysqlnd 8.5.9";

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
/// time, and `mysqlnd_global_stats`.
#[derive(Default)]
struct State {
    conns: Vec<Slot>,
    stats: Vec<u64>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// string(13) "mysqlnd 8.5.9"
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
                    name: Name(name.as_bytes().into()),
                    default: default.map(|d| d.as_bytes().into()),
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
        .request_state::<State>()
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
/// Version => mysqlnd 8.5.9
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
    let stats = yes(req, b"mysqlnd.collect_statistics");
    let memory = yes(req, b"mysqlnd.collect_memory_statistics");
    let tracing = req.ini().value(b"mysqlnd.debug").map_or_else(
        || "n/a".to_owned(),
        |v| String::from_utf8_lossy(v).into_owned(),
    );
    let apis: Vec<&str> = req
        .registered::<ReverseApi>()
        .map(|api| api.extension)
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
        let enabled = ini_bool(req, b"mysqlnd.collect_statistics");
        let mut conn = slot_of(req.state::<State>(), self)?.conn.take()?;
        conn.stats.enabled = enabled;
        let answered = f(req, &mut conn);
        flush(req, &mut conn);
        if let Some(slot) = slot_of(req.state::<State>(), self) {
            slot.conn = Some(conn);
        }
        Some(answered)
    }

    /// `get_reference`: one more owner of the connection.
    pub(crate) fn acquire(self, req: &mut Request) {
        if let Some(slot) = slot_of(req.state::<State>(), self) {
            slot.refs += 1;
        }
    }

    /// `free_reference`: one owner fewer, and the last one's close sent and
    /// the connection dropped.
    pub(crate) fn release(self, req: &mut Request) {
        let taken = match slot_of(req.state::<State>(), self) {
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
            conn.send_close(req).ok();
            flush(req, &mut conn);
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
    pub(crate) fn give_back(self, req: &mut Request) -> Option<Box<ConnData>> {
        let slot = slot_of(req.state::<State>(), self)?;
        slot.conn.as_ref()?.net.persistent_id.as_ref()?;
        let mut conn = slot.conn.take()?;
        slot.refs -= 1;
        // A result's pool is the request's heap's, which the request's
        // end releases: none waits with the connection.
        if let Some(result) = conn.current_result.take() {
            result.release(req);
        }
        // Nor does a last message, the request's whatever the connection.
        crate::alloc::Pstr::put(req, &mut conn.last_message, None);
        conn.net.give_back(req);
        Some(conn)
    }

    /// A connection a request gave back held in the table again, one
    /// owner counted, over the socket it kept.
    pub(crate) fn take_up(req: &mut Request, mut conn: Box<ConnData>) -> Result<Handle, Thrown> {
        conn.net.take_up(req)?;
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
fn flush(req: &mut Request, conn: &mut ConnData) {
    let state = req.state::<State>();
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
