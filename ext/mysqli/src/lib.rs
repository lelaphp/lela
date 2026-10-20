//! The `mysqli` extension: `php-src/ext/mysqli` as one crate under `ext/`
//! (ADR-0004), `mysqli*.c` over `ext/mysqlnd`'s [`client`], which it
//! reaches as php's mysqli reaches `mysqlnd.h` (design §4.10), and never
//! the wire. The phpt subset is `php-src/ext/mysqli/tests`, through
//! `xtask phpt --subset`, against the server of `ci/mysql/compose.yaml`.
//!
//! **What an object holds.** php's `mysqli_object` points at a
//! `MYSQLI_RESOURCE` whose integer `status` says how far the object got.
//! Here each class's payload is an enum of the states php's status and its
//! null pointer stand for: a link is [`link::Link`] (closed, made and not
//! yet connected, or open), a set of rows is [`result::Rows`] (stored, used or
//! freed), a statement is [`stmt::StmtObj`] (closed, made, or prepared), so
//! `MYSQLI_FETCH_RESOURCE`'s two refusals are a match on the state. The
//! properties php's `mysqli_prop_handler` tables answer are each class's
//! [`props::Props`], read through the one table of handlers
//! [`props::Handlers`].
//!
//! **What a request holds.** `MyG(...)` is the request's [`State`]: the
//! `report_mode`, the last connect error, the link
//! counts, and the `p:` links it took out of the engine's pool or opened,
//! held under php's `hash_key` until `post_deactivate` gives them back. php
//! keeps them in `EG(persistent_list)`, one list per php-fpm process; here
//! the list is a pool of the engine's (design §4.10, revision 55), lent to
//! one request at a time, and the inactive links php counts per process
//! are the pool's idle ones and the request's free ones.
//!
//! **Twins.** php serves `mysqli_query($link, ...)` and `$link->query(...)`
//! with one C function that asks `hasThis()`. Here the procedural function
//! (in [`functions`], in the stub's order) and the method (on its class)
//! are two natives over one body, which takes an [`On`] to say which it
//! serves, for `ERROR_ARG_POS` and the texts that name the call.
//!
//! [`client`]: php_ext_mysqlnd::client
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

mod extras;
mod functions;
mod link;
mod props;
mod result;
mod stmt;

use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, Lent, LongHandler, Pool,
};
use php_ext::attr::php_const;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, docref, new_throwable, throw_error, throw_value};
use php_ext::request::Request;
use php_ext::values::{ClassId, Emalloc, Owner, PhpObject, PhpString, Value};
use php_ext_mysqlnd::client::{CLIENT_INFO, Idle, Mysqlnd, ReverseApi};

/// `MYSQLI_VERSION_ID`, what the deprecated `driver_version` reads.
const VERSION_ID: i64 = 101_009;

/// `MYSQLI_REPORT_*`.
pub(crate) mod report {
    pub(crate) const ERROR: i64 = 1;
    pub(crate) const STRICT: i64 = 2;
    pub(crate) const INDEX: i64 = 4;
}

/// `SERVER_QUERY_NO_GOOD_INDEX_USED` and `SERVER_QUERY_NO_INDEX_USED`.
const NO_GOOD_INDEX_USED: u32 = 16;
const NO_INDEX_USED: u32 = 32;

/// The `mysqli.*` entries, `PHP_INI_BEGIN` in php's order: the name, the
/// default, and whether `ini_set` may set it (`PHP_INI_ALL`) or only the
/// system (`PHP_INI_SYSTEM`).
const INI: [(&str, Option<&str>, bool); 11] = [
    ("mysqli.max_links", Some("-1"), false),
    ("mysqli.max_persistent", Some("-1"), false),
    ("mysqli.allow_persistent", Some("1"), false),
    ("mysqli.rollback_on_cached_plink", Some("0"), false),
    ("mysqli.default_host", None, true),
    ("mysqli.default_user", None, true),
    ("mysqli.default_pw", None, true),
    ("mysqli.default_port", Some("3306"), true),
    ("mysqli.default_socket", Some("/tmp/mysql.sock"), true),
    ("mysqli.allow_local_infile", Some("0"), false),
    ("mysqli.local_infile_directory", None, false),
];

/// `zend_mysqli_globals`, the request's: what `PHP_RINIT_FUNCTION(mysqli)`
/// resets and the links the request counts.
pub(crate) struct State {
    /// `report_mode`, which `mysqli_report` and `mysqli_driver::$report_mode`
    /// set to any integer.
    pub(crate) report_mode: i64,
    /// `error_no` and `error_msg`: the last connect's error, `None` where
    /// it left no text.
    pub(crate) connect_errno: i64,
    pub(crate) connect_error: Option<Vec<u8>>,
    /// `num_links` and `num_active_persistent`, per request where php
    /// keeps them per process and never resets them, so php's active
    /// count drifts across a php-fpm worker's requests and this one starts
    /// at zero: the divergence row
    /// `trace:mysqli-persistent-counts-per-request`, ruled on #1676.
    pub(crate) num_links: i64,
    pub(crate) num_active_persistent: i64,
    /// The free links of [`State::plinks`], the request's share of
    /// `num_inactive_persistent`.
    pub(crate) num_free_persistent: i64,
    /// `EG(persistent_list)`'s `mysqli_plist_entry`s the request holds, by
    /// `hash_key`: each persistent connection it took out of the pool or
    /// opened, open on a link or free for the next connect.
    pub(crate) plinks: Lent<Vec<u8>, link::Plinks>,
}

/// `EG(persistent_list)`'s `mysqli_plist_entry`s: the engine's pool of
/// persistent connections, each key's a stack of free links, php's
/// `plist->free_links`, each link the connection itself with its socket,
/// in no other list between the requests that take it up.
pub(crate) struct Pooled(pub(crate) Pool<Vec<u8>, Idle>);

/// The idle persistent connections the pool keeps; php bounds mysqli's by
/// `mysqli.max_persistent` alone, and this is the engine's own streams'
/// bound.
const POOLED: usize = 1024;

/// `MyG(num_inactive_persistent)`: the pool's idle connections and the
/// request's free ones.
pub(crate) fn num_inactive_persistent(req: &mut Request) -> i64 {
    let pooled = req
        .engine::<Pooled>()
        .map_or(0, |Pooled(pool)| pool.idle_len());
    req.state::<State>().num_free_persistent + i64::try_from(pooled).unwrap_or(i64::MAX)
}

impl Default for State {
    fn default() -> State {
        State {
            report_mode: report::ERROR | report::STRICT,
            connect_errno: 0,
            connect_error: None,
            num_links: 0,
            num_active_persistent: 0,
            num_free_persistent: 0,
            plinks: Lent::default(),
        }
    }
}

/// The 110 constants of `mysqli.stub.php`, in its order, with the values
/// `register_mysqli_symbols` gives them at php-8.5.9.
#[php_const]
mod constants {
    pub(crate) const MYSQLI_READ_DEFAULT_GROUP: i64 = 5;
    pub(crate) const MYSQLI_READ_DEFAULT_FILE: i64 = 4;
    pub(crate) const MYSQLI_OPT_CONNECT_TIMEOUT: i64 = 0;
    pub(crate) const MYSQLI_OPT_LOCAL_INFILE: i64 = 8;
    pub(crate) const MYSQLI_OPT_LOAD_DATA_LOCAL_DIR: i64 = 43;
    pub(crate) const MYSQLI_INIT_COMMAND: i64 = 3;
    pub(crate) const MYSQLI_OPT_READ_TIMEOUT: i64 = 11;
    pub(crate) const MYSQLI_OPT_NET_CMD_BUFFER_SIZE: i64 = 202;
    pub(crate) const MYSQLI_OPT_NET_READ_BUFFER_SIZE: i64 = 203;
    pub(crate) const MYSQLI_OPT_INT_AND_FLOAT_NATIVE: i64 = 201;
    pub(crate) const MYSQLI_OPT_SSL_VERIFY_SERVER_CERT: i64 = 21;
    pub(crate) const MYSQLI_SERVER_PUBLIC_KEY: i64 = 35;
    pub(crate) const MYSQLI_CLIENT_SSL: i64 = 2048;
    pub(crate) const MYSQLI_CLIENT_COMPRESS: i64 = 32;
    pub(crate) const MYSQLI_CLIENT_INTERACTIVE: i64 = 1024;
    pub(crate) const MYSQLI_CLIENT_IGNORE_SPACE: i64 = 256;
    pub(crate) const MYSQLI_CLIENT_NO_SCHEMA: i64 = 16;
    pub(crate) const MYSQLI_CLIENT_FOUND_ROWS: i64 = 2;
    pub(crate) const MYSQLI_CLIENT_SSL_VERIFY_SERVER_CERT: i64 = 1_073_741_824;
    pub(crate) const MYSQLI_CLIENT_SSL_DONT_VERIFY_SERVER_CERT: i64 = 64;
    pub(crate) const MYSQLI_CLIENT_CAN_HANDLE_EXPIRED_PASSWORDS: i64 = 4_194_304;
    pub(crate) const MYSQLI_OPT_CAN_HANDLE_EXPIRED_PASSWORDS: i64 = 37;
    pub(crate) const MYSQLI_STORE_RESULT: i64 = 0;
    pub(crate) const MYSQLI_USE_RESULT: i64 = 1;
    pub(crate) const MYSQLI_ASYNC: i64 = 8;
    #[php(deprecated(
        since = "8.4",
        message = "as the mysqli_store_result() parameter is unused since 8.1"
    ))]
    pub(crate) const MYSQLI_STORE_RESULT_COPY_DATA: i64 = 16;
    pub(crate) const MYSQLI_ASSOC: i64 = 1;
    pub(crate) const MYSQLI_NUM: i64 = 2;
    pub(crate) const MYSQLI_BOTH: i64 = 3;
    pub(crate) const MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH: i64 = 0;
    pub(crate) const MYSQLI_STMT_ATTR_CURSOR_TYPE: i64 = 1;
    pub(crate) const MYSQLI_CURSOR_TYPE_NO_CURSOR: i64 = 0;
    pub(crate) const MYSQLI_CURSOR_TYPE_READ_ONLY: i64 = 1;
    pub(crate) const MYSQLI_NOT_NULL_FLAG: i64 = 1;
    pub(crate) const MYSQLI_PRI_KEY_FLAG: i64 = 2;
    pub(crate) const MYSQLI_UNIQUE_KEY_FLAG: i64 = 4;
    pub(crate) const MYSQLI_MULTIPLE_KEY_FLAG: i64 = 8;
    pub(crate) const MYSQLI_BLOB_FLAG: i64 = 16;
    pub(crate) const MYSQLI_UNSIGNED_FLAG: i64 = 32;
    pub(crate) const MYSQLI_ZEROFILL_FLAG: i64 = 64;
    pub(crate) const MYSQLI_AUTO_INCREMENT_FLAG: i64 = 512;
    pub(crate) const MYSQLI_TIMESTAMP_FLAG: i64 = 1024;
    pub(crate) const MYSQLI_SET_FLAG: i64 = 2048;
    pub(crate) const MYSQLI_NUM_FLAG: i64 = 32768;
    pub(crate) const MYSQLI_PART_KEY_FLAG: i64 = 16384;
    pub(crate) const MYSQLI_GROUP_FLAG: i64 = 32768;
    pub(crate) const MYSQLI_ENUM_FLAG: i64 = 256;
    pub(crate) const MYSQLI_BINARY_FLAG: i64 = 128;
    pub(crate) const MYSQLI_NO_DEFAULT_VALUE_FLAG: i64 = 4096;
    pub(crate) const MYSQLI_ON_UPDATE_NOW_FLAG: i64 = 8192;
    pub(crate) const MYSQLI_TYPE_DECIMAL: i64 = 0;
    pub(crate) const MYSQLI_TYPE_TINY: i64 = 1;
    pub(crate) const MYSQLI_TYPE_SHORT: i64 = 2;
    pub(crate) const MYSQLI_TYPE_LONG: i64 = 3;
    pub(crate) const MYSQLI_TYPE_FLOAT: i64 = 4;
    pub(crate) const MYSQLI_TYPE_DOUBLE: i64 = 5;
    pub(crate) const MYSQLI_TYPE_NULL: i64 = 6;
    pub(crate) const MYSQLI_TYPE_TIMESTAMP: i64 = 7;
    pub(crate) const MYSQLI_TYPE_LONGLONG: i64 = 8;
    pub(crate) const MYSQLI_TYPE_INT24: i64 = 9;
    pub(crate) const MYSQLI_TYPE_DATE: i64 = 10;
    pub(crate) const MYSQLI_TYPE_TIME: i64 = 11;
    pub(crate) const MYSQLI_TYPE_DATETIME: i64 = 12;
    pub(crate) const MYSQLI_TYPE_YEAR: i64 = 13;
    pub(crate) const MYSQLI_TYPE_NEWDATE: i64 = 14;
    pub(crate) const MYSQLI_TYPE_ENUM: i64 = 247;
    pub(crate) const MYSQLI_TYPE_SET: i64 = 248;
    pub(crate) const MYSQLI_TYPE_TINY_BLOB: i64 = 249;
    pub(crate) const MYSQLI_TYPE_MEDIUM_BLOB: i64 = 250;
    pub(crate) const MYSQLI_TYPE_LONG_BLOB: i64 = 251;
    pub(crate) const MYSQLI_TYPE_BLOB: i64 = 252;
    pub(crate) const MYSQLI_TYPE_VAR_STRING: i64 = 253;
    pub(crate) const MYSQLI_TYPE_STRING: i64 = 254;
    pub(crate) const MYSQLI_TYPE_CHAR: i64 = 1;
    pub(crate) const MYSQLI_TYPE_GEOMETRY: i64 = 255;
    pub(crate) const MYSQLI_TYPE_VECTOR: i64 = 242;
    pub(crate) const MYSQLI_TYPE_JSON: i64 = 245;
    pub(crate) const MYSQLI_TYPE_NEWDECIMAL: i64 = 246;
    pub(crate) const MYSQLI_TYPE_BIT: i64 = 16;
    pub(crate) const MYSQLI_SET_CHARSET_NAME: i64 = 7;
    #[php(deprecated(since = "8.1", message = "as it was unused"))]
    pub(crate) const MYSQLI_NO_DATA: i64 = 100;
    #[php(deprecated(since = "8.1", message = "as it was unused"))]
    pub(crate) const MYSQLI_DATA_TRUNCATED: i64 = 101;
    pub(crate) const MYSQLI_REPORT_INDEX: i64 = 4;
    pub(crate) const MYSQLI_REPORT_ERROR: i64 = 1;
    pub(crate) const MYSQLI_REPORT_STRICT: i64 = 2;
    pub(crate) const MYSQLI_REPORT_ALL: i64 = 255;
    pub(crate) const MYSQLI_REPORT_OFF: i64 = 0;
    pub(crate) const MYSQLI_DEBUG_TRACE_ENABLED: i64 = 0;
    #[php(deprecated(since = "8.1", message = "as it was unused"))]
    pub(crate) const MYSQLI_SERVER_QUERY_NO_GOOD_INDEX_USED: i64 = 16;
    #[php(deprecated(since = "8.1", message = "as it was unused"))]
    pub(crate) const MYSQLI_SERVER_QUERY_NO_INDEX_USED: i64 = 32;
    #[php(deprecated(since = "8.1", message = "as it was unused"))]
    pub(crate) const MYSQLI_SERVER_QUERY_WAS_SLOW: i64 = 2048;
    #[php(deprecated(since = "8.1", message = "as it was unused"))]
    pub(crate) const MYSQLI_SERVER_PS_OUT_PARAMS: i64 = 4096;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_GRANT: i64 = 1;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_LOG: i64 = 2;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_TABLES: i64 = 4;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_HOSTS: i64 = 8;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_STATUS: i64 = 16;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_THREADS: i64 = 32;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_REPLICA: i64 = 64;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_SLAVE: i64 = 64;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_MASTER: i64 = 128;
    #[php(deprecated(since = "8.4", message = "as mysqli_refresh() is deprecated"))]
    pub(crate) const MYSQLI_REFRESH_BACKUP_LOG: i64 = 2_097_152;
    pub(crate) const MYSQLI_TRANS_START_WITH_CONSISTENT_SNAPSHOT: i64 = 1;
    pub(crate) const MYSQLI_TRANS_START_READ_WRITE: i64 = 2;
    pub(crate) const MYSQLI_TRANS_START_READ_ONLY: i64 = 4;
    pub(crate) const MYSQLI_TRANS_COR_AND_CHAIN: i64 = 1;
    pub(crate) const MYSQLI_TRANS_COR_AND_NO_CHAIN: i64 = 2;
    pub(crate) const MYSQLI_TRANS_COR_RELEASE: i64 = 4;
    pub(crate) const MYSQLI_TRANS_COR_NO_RELEASE: i64 = 8;
    #[php(deprecated(since = "8.2", message = "as it is always false"))]
    pub(crate) const MYSQLI_IS_MARIADB: bool = false;
}

/// The extension value (design §7.1): `spl` and `mysqlnd` required, the
/// ini entries, the constants, the six classes in `PHP_MINIT_FUNCTION`'s
/// order and the functions in the stub's, the reverse API mysqlnd lists,
/// and the request's [`State`].
///
/// ```php
/// var_dump(extension_loaded('mysqli'), MYSQLI_ASYNC, MYSQLI_REPORT_ALL, ini_get('mysqli.default_port'));
/// print_r((new ReflectionExtension('mysqli'))->getDependencies());
/// var_dump(MYSQLI_IS_MARIADB);
/// ```
/// ```expect
/// bool(true)
/// int(8)
/// int(255)
/// string(4) "3306"
/// Array
/// (
///     [spl] => Required
///     [mysqlnd] => Required
/// )
///
/// Deprecated: Constant MYSQLI_IS_MARIADB is deprecated since 8.2, as it is always false in %s on line 4
/// bool(false)
/// ```
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .dependencies(vec![required("spl"), required("mysqlnd")])
        .ini(
            INI.iter()
                .map(|(name, default, user)| IniDecl {
                    name: Name(name.as_bytes().into()),
                    default: default.map(|d| d.as_bytes().into()),
                    user: *user,
                    per_dir: *user,
                    system: true,
                })
                .collect(),
        )
        .on_update_long(b"mysqli.max_links", LongHandler::Any)
        .on_update_long(b"mysqli.max_persistent", LongHandler::Any)
        .on_update_long(b"mysqli.default_port", LongHandler::Any)
        .constants(constants::declared())
        .class::<extras::mysqli_sql_exception>()
        .class::<extras::mysqli_driver>()
        .class::<link::mysqli>()
        .class::<extras::mysqli_warning>()
        .class::<result::mysqli_result>()
        .class::<stmt::mysqli_stmt>();
    functions::register(builder)
        .register(ReverseApi {
            extension: "mysqli",
        })
        .request_state::<State>()
        .engine_state(|_| Ok(Pooled(Pool::new(POOLED))))
        .post_deactivate(link::post_deactivate)
        .engine_end(link::engine_end)
        .info(mysqli_info)
        .build("mysqli")
}

fn required(name: &str) -> Dependency {
    Dependency {
        name: String::from(name),
        kind: DependencyKind::Required,
        rel: None,
        version: None,
    }
}

/// `PHP_MINFO_FUNCTION(mysqli)`.
///
/// ```php
/// ob_start(); (new ReflectionExtension('mysqli'))->info(); $info = ob_get_clean();
/// echo substr($info, 0, strpos($info, 'Directive'));
/// ```
/// ```expect
/// mysqli
///
/// MysqlI Support => enabled
/// Client API library version => mysqlnd 8.5.9
/// Active Persistent Links => 0
/// Inactive Persistent Links => 0
/// Active Links => 0
/// ```
fn mysqli_info(req: &mut Request) {
    let inactive = num_inactive_persistent(req).to_string();
    let state = req.state::<State>();
    let (active, links) = (
        state.num_active_persistent.to_string(),
        state.num_links.to_string(),
    );
    req.info_table(&[
        &["MysqlI Support", "enabled"],
        &["Client API library version", CLIENT_INFO],
        &["Active Persistent Links", &active],
        &["Inactive Persistent Links", &inactive],
        &["Active Links", &links],
    ]);
    // `DISPLAY_INI_ENTRIES`, each entry through its displayer:
    // `display_link_numbers` for the two limits, the boolean one for the
    // three `STD_PHP_INI_BOOLEAN`s.
    let entries = req.ini_entries();
    let shown = |name: &str, value: &[u8]| -> String {
        match name {
            "mysqli.max_links" | "mysqli.max_persistent" => {
                match std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.parse::<i64>().ok())
                {
                    Some(-1) => String::from("Unlimited"),
                    Some(n) => n.to_string(),
                    None => String::from_utf8_lossy(value).into_owned(),
                }
            }
            "mysqli.allow_persistent"
            | "mysqli.rollback_on_cached_plink"
            | "mysqli.allow_local_infile" => String::from(if Request::ini_parse_bool(value) {
                "On"
            } else {
                "Off"
            }),
            _ => String::from_utf8_lossy(value).into_owned(),
        }
    };
    let rows: Vec<[String; 3]> = INI
        .iter()
        .filter_map(|(name, _, _)| {
            let entry = entries
                .iter()
                .find(|entry| entry.decl.name.0.as_ref() == name.as_bytes())?;
            Some([
                (*name).to_owned(),
                shown(name, entry.value.as_deref().unwrap_or_default()),
                shown(name, entry.master.as_deref().unwrap_or_default()),
            ])
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.info_ini_table(&cells);
}

// ---- what every class shares

/// What a native is served on: the object a procedural function was
/// handed, or a method's `$this`; php's one C function serves both and
/// tells them apart by `hasThis()`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct On {
    pub(crate) object: PhpObject,
    pub(crate) method: bool,
}

impl On {
    pub(crate) fn function(object: PhpObject) -> On {
        On {
            object,
            method: false,
        }
    }

    pub(crate) fn method(object: PhpObject) -> On {
        On {
            object,
            method: true,
        }
    }

    /// `ERROR_ARG_POS`: the procedural position `n`, one less for a
    /// method, whose object is no argument.
    pub(crate) fn at(self, n: u32) -> u32 {
        if self.method { n - 1 } else { n }
    }
}

/// `sizeof(MYSQLI_RESOURCE)`: `ptr`, `info` and `status`.
const RESOURCE: usize = 24;

/// `MYSQLI_RESOURCE`: what an object's `ptr` points at, in the allocation
/// php `ecalloc`s to point at it, on the request's heap until
/// `MYSQLI_CLEAR_RESOURCE` or the object's free gives it back; its
/// `status` is the state the payload holding it is in.
pub(crate) struct Resource<T> {
    ptr: T,
    held: Emalloc,
}

impl<T> Resource<T> {
    /// `ecalloc(1, sizeof(MYSQLI_RESOURCE))` pointed at `ptr`; the `Err`
    /// is the memory limit's fatal, which ends the request with `ptr`.
    pub(crate) fn new(req: &mut Request, ptr: T) -> Result<Resource<T>, Thrown> {
        Ok(Resource {
            ptr,
            held: Emalloc::new(req, RESOURCE)?,
        })
    }

    /// `mysqli_resource->ptr = ptr`: the resource pointed at `ptr`, and
    /// what it pointed at before given back.
    pub(crate) fn repoint<U>(self, ptr: U) -> (Resource<U>, T) {
        (
            Resource {
                ptr,
                held: self.held,
            },
            self.ptr,
        )
    }

    /// `efree(intern->ptr)`: the resource given back, what it pointed at
    /// left to its owner.
    pub(crate) fn free(self, req: &mut Request) -> T {
        self.held.free(req);
        self.ptr
    }
}

impl<T> std::ops::Deref for Resource<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.ptr
    }
}

impl<T> std::ops::DerefMut for Resource<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.ptr
    }
}

/// The payload `T` of `object`, which each class lays down boxed, so
/// `mysqli_object` keeps php's size.
pub(crate) fn payload<T: Send + 'static>(req: &mut Request, object: PhpObject) -> Option<&mut T> {
    object.native_mut::<Box<T>>(req).map(|b| &mut **b)
}

/// The payload `T` of `object`, to read.
pub(crate) fn payload_ref<T: Send + 'static>(req: &Request, object: PhpObject) -> Option<&T> {
    object.native::<Box<T>>(req).map(|b| &**b)
}

/// `object_init`: a new `stdClass`.
pub(crate) fn std_object(req: &mut Request) -> Result<Owner<PhpObject>, Thrown> {
    let class = php_ext::call::lookup_class(req, b"stdClass")?
        .ok_or_else(|| php_ext::raising::fatal(req, "InternalError: stdClass is not linked"))?;
    php_ext::call::new_object(req, class)
}

/// `MyG(report_mode)`.
pub(crate) fn report_mode(req: &mut Request) -> i64 {
    req.state::<State>().report_mode
}

/// `php_mysqli_throw_sql_exception`: under `MYSQLI_REPORT_STRICT` a
/// `mysqli_sql_exception` with the message, the state and the number as
/// its code, thrown; otherwise php's warning `(state/errno): message`.
pub(crate) fn sql_exception(
    req: &mut Request,
    sqlstate: &[u8],
    errno: u32,
    message: &[u8],
) -> Result<(), Thrown> {
    let message = String::from_utf8_lossy(message);
    let state = String::from_utf8_lossy(sqlstate);
    if report_mode(req) & report::STRICT == 0 {
        docref(
            req,
            Level::Warning,
            &format!("({state}/{errno}): {message}"),
        );
        return Ok(());
    }
    let value = new_throwable(req, "mysqli_sql_exception", message.as_bytes())?;
    if let Some(object) = value.as_object() {
        let facts = [
            ("sqlstate", Fact::Text(sqlstate)),
            ("code", Fact::Int(i64::from(errno))),
        ];
        let written = add_facts(req, object, &facts);
        if let Err(thrown) = written {
            value.give_up(req);
            return Err(thrown);
        }
    }
    let thrown = throw_value(req, &value);
    value.give_up(req);
    Err(thrown)
}

/// `php_mysqli_report_error` where `MYSQLI_REPORT_ERROR` is on and the
/// error is one: `MYSQLI_REPORT_MYSQL_ERROR` and `MYSQLI_REPORT_STMT_ERROR`
/// over the facts read.
pub(crate) fn report_error(
    req: &mut Request,
    errno: u32,
    sqlstate: [u8; 5],
    error: &[u8],
) -> Result<(), Thrown> {
    if errno != 0 && report_mode(req) & report::ERROR != 0 {
        return sql_exception(req, &sqlstate, errno, error);
    }
    Ok(())
}

/// `MYSQLI_REPORT_MYSQL_ERROR(mysql)`.
pub(crate) fn report_conn(req: &mut Request, conn: Mysqlnd) -> Result<(), Thrown> {
    if report_mode(req) & report::ERROR == 0 {
        return Ok(());
    }
    let status = conn.status(req);
    report_error(req, status.errno, status.sqlstate, &status.error)
}

/// `php_mysqli_report_index`: under `MYSQLI_REPORT_INDEX`, a query the
/// server ran without a (good) index thrown or warned
/// `"Bad index"` or `"No index"` names.
pub(crate) fn report_index(req: &mut Request, query: &[u8], status: u32) -> Result<(), Thrown> {
    if report_mode(req) & report::INDEX == 0 {
        return Ok(());
    }
    let index = if status & NO_GOOD_INDEX_USED != 0 {
        "Bad index"
    } else if status & NO_INDEX_USED != 0 {
        "No index"
    } else {
        return Ok(());
    };
    let message = format!(
        "{index} used in query/prepared statement {}",
        String::from_utf8_lossy(query)
    );
    sql_exception(req, b"00000", 0, message.as_bytes())
}

/// The class's name, `ZSTR_VAL(ce->name)`.
pub(crate) fn class_name(req: &Request, class: ClassId) -> String {
    req.class_decl(class)
        .map(|d| String::from_utf8_lossy(&d.name.0).into_owned())
        .unwrap_or_default()
}

/// `zend_throw_error(NULL, "%s object is already closed", ...)`.
pub(crate) fn closed(req: &mut Request, object: PhpObject) -> Thrown {
    let name = class_name(req, object.class());
    throw_error(req, "Error", &format!("{name} object is already closed"))
}

/// `zend_throw_error(NULL, "%s object is not fully initialized", ...)`.
pub(crate) fn not_initialized(req: &mut Request, object: PhpObject) -> Thrown {
    let name = class_name(req, object.class());
    throw_error(
        req,
        "Error",
        &format!("{name} object is not fully initialized"),
    )
}

/// `MYSQLI_RETURN_LONG_INT`: a count below `ZEND_LONG_MAX` as an integer,
/// any other as its decimal string.
pub(crate) fn long_or_string(req: &mut Request, n: u64) -> Result<Owner<Value>, Thrown> {
    match i64::try_from(n) {
        Ok(n) if n < i64::MAX => Ok(Owner::int(n)),
        _ => string(req, n.to_string().as_bytes()),
    }
}

/// `mysql_affected_rows` as mysqli answers it: `~0` is `-1`.
pub(crate) fn affected(req: &mut Request, n: u64) -> Result<Owner<Value>, Thrown> {
    if n == u64::MAX {
        return Ok(Owner::int(-1));
    }
    long_or_string(req, n)
}

/// A string value of `bytes`.
pub(crate) fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// One property of an object that describes something, as
/// `add_property_stringl` and `add_property_long` write it.
pub(crate) enum Fact<'a> {
    /// A string made for the property, `ZVAL_STRINGL`'s
    /// `zend_string_init`: allocated whatever its length, the empty
    /// string too, where `ZVAL_STRINGL_FAST` would hand out the interned
    /// one.
    Text(&'a [u8]),
    /// A string shared, one more owner of it: `zend_string_copy`.
    Str(PhpString),
    Int(i64),
}

/// The facts written onto `object` in order, each value made as it is
/// written.
pub(crate) fn add_facts(
    req: &mut Request,
    object: PhpObject,
    facts: &[(&str, Fact<'_>)],
) -> Result<(), Thrown> {
    for (name, fact) in facts {
        let value = match fact {
            Fact::Text(bytes) => {
                PhpString::filled(req, bytes.len(), |to| to.copy_from_slice(bytes))?.into_value()
            }
            Fact::Str(s) => Owner::take(req, Value::string(*s)),
            Fact::Int(n) => Owner::int(*n),
        };
        php_ext::call::write_prop(req, &object, name.as_bytes(), value)?;
    }
    Ok(())
}

/// The ini entry as `OnUpdateString` keeps it: `None` for no value or an
/// empty one.
pub(crate) fn ini_string(req: &Request, name: &[u8]) -> Option<Vec<u8>> {
    req.ini_get(name)
        .filter(|v| !v.is_empty())
        .map(<[u8]>::to_vec)
}

/// The ini entry as `OnUpdateBool` reads it.
pub(crate) fn ini_bool(req: &Request, name: &[u8]) -> bool {
    req.ini_get(name).is_some_and(Request::ini_parse_bool)
}
