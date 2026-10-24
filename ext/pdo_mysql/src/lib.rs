//! The `pdo_mysql` extension: `php-src/ext/pdo_mysql` as one crate under
//! `ext/` (ADR-0004), the `MySQL` connector behind `ext/pdo`'s connector face
//! over `ext/mysqlnd`'s client (#1345). Its dependencies are the facade,
//! `pdo` and `mysqlnd`, as `ZEND_MOD_REQUIRED("pdo")` and
//! `ZEND_MOD_REQUIRED("mysqlnd")` have it; its one entry is `extension()`.
//! The phpt subset is `php-src/ext/pdo_mysql/tests`, through
//! `xtask phpt --subset`, with `ext/pdo/tests` redirected under its DSN.
//!
//! `PHP_MINIT_FUNCTION(pdo_mysql)` as a value: the ini entry, the
//! deprecated `PDO::MYSQL_*` aliases, the reverse API mysqlnd lists, the
//! connector registered with PDO (`driver::MYSQL`) and `Pdo\Mysql` as
//! `pdo_mysql.stub.php` declares it.
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

mod driver;
mod scanner;
mod statement;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder, This};
use php_ext::attr::php_class;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext_mysqlnd::client::ReverseApi;
use php_ext_pdo::connector::Connector;

/// `pdo_mysql.default_socket`'s default, `PDO_MYSQL_UNIX_ADDR`.
const DEFAULT_SOCKET: &str = "/tmp/mysql.sock";

/// The extension value (design §7.1): the ini entry, `pdo` and `mysqlnd`
/// as its dependencies, `Pdo\Mysql`, the aliases on `PDO`, the reverse
/// API and the connector, and the `MINFO` table.
///
/// ```php
/// var_dump(extension_loaded('pdo_mysql'), in_array('mysql', PDO::getAvailableDrivers()), Pdo\Mysql::ATTR_USE_BUFFERED_QUERY, ini_get('pdo_mysql.default_socket'));
/// var_dump((new ReflectionClass('Pdo\Mysql'))->getParentClass()->getName(), array_keys((new ReflectionExtension('pdo_mysql'))->getDependencies()));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(1000)
/// string(15) "/tmp/mysql.sock"
/// string(3) "PDO"
/// array(2) {
///   [0]=>
///   string(3) "pdo"
///   [1]=>
///   string(7) "mysqlnd"
/// }
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![
            Dependency {
                name: String::from("pdo"),
                kind: DependencyKind::Required,
                rel: None,
                version: None,
            },
            Dependency {
                name: String::from("mysqlnd"),
                kind: DependencyKind::Required,
                rel: None,
                version: None,
            },
        ])
        .ini(vec![IniDecl {
            name: Name(b"pdo_mysql.default_socket".as_slice().into()),
            default: Some(DEFAULT_SOCKET.as_bytes().into()),
            user: false,
            per_dir: false,
            system: true,
        }])
        .class::<PDO>()
        .register(ReverseApi {
            extension: "pdo_mysql",
        })
        .class::<Mysql>()
        .register::<&'static dyn Connector>(&driver::MYSQL)
        .request_state::<driver::State>()
        .info(pdo_mysql_info)
        .build("pdo_mysql")
}

/// `PHP_MINFO_FUNCTION(pdo_mysql)`: the connector, the client's version,
/// and the ini entries.
///
/// ```php
/// ob_start(); phpinfo(INFO_MODULES); $info = ob_get_clean();
/// $at = strpos($info, "\npdo_mysql\n");
/// echo substr($info, $at, strpos($info, "pdo_mysql.default_socket", $at) - $at);
/// ```
/// ```expect
///
/// pdo_mysql
///
/// PDO Driver for MySQL => enabled
/// Client API version => mysqlnd 8.5.12
///
/// Directive => Local Value => Master Value
///
/// ```
fn pdo_mysql_info(req: &mut Request) {
    req.extensions_mut().info_table(&[
        &["PDO Driver for MySQL", "enabled"],
        &["Client API version", php_ext_mysqlnd::client::CLIENT_INFO],
    ]);
    req.ini_mut()
        .info_ini_entries(&[b"pdo_mysql.default_socket"]);
}

/// `class Pdo\Mysql extends PDO`, `@strict-properties`, with its
/// constants and its method in the stub's order.
#[php_class(namespace(Pdo), extends(PDO), dynamic_properties = forbidden)]
pub(crate) struct Mysql;

#[php_class]
impl Mysql {
    const ATTR_USE_BUFFERED_QUERY: i64 = 1000;
    const ATTR_LOCAL_INFILE: i64 = 1001;
    const ATTR_INIT_COMMAND: i64 = 1002;
    const ATTR_COMPRESS: i64 = 1003;
    const ATTR_DIRECT_QUERY: i64 = 20;
    const ATTR_FOUND_ROWS: i64 = 1004;
    const ATTR_IGNORE_SPACE: i64 = 1005;
    const ATTR_SSL_KEY: i64 = 1006;
    const ATTR_SSL_CERT: i64 = 1007;
    const ATTR_SSL_CA: i64 = 1008;
    const ATTR_SSL_CAPATH: i64 = 1009;
    const ATTR_SSL_CIPHER: i64 = 1010;
    const ATTR_SERVER_PUBLIC_KEY: i64 = 1011;
    const ATTR_MULTI_STATEMENTS: i64 = 1012;
    const ATTR_SSL_VERIFY_SERVER_CERT: i64 = 1013;
    const ATTR_LOCAL_INFILE_DIRECTORY: i64 = 1014;

    /// `Pdo\Mysql::getWarningCount(): int`: the warnings of the last
    /// statement, `mysql_warning_count`.
    ///
    /// ```php
    /// $db = new Pdo\Mysql('mysql:host=127.0.0.1;dbname=test', 'root', '');
    /// $db->query("SELECT 1/0");
    /// var_dump($db->getWarningCount());
    /// class Later extends Pdo\Mysql { function __construct() {} }
    /// try { (new Later)->getWarningCount(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect mysql
    /// int(1)
    /// Later object is uninitialized
    /// ```
    fn getWarningCount(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        php_ext_pdo::connector::with_connection(req, this.object(), |req, conn| {
            Ok(driver::warning_count(req, conn))
        })
    }
}

/// `\PDO`, which `pdo` declares: what `Pdo\Mysql` extends, and the class
/// this connector lays its deprecated `MYSQL_*` aliases on.
#[php_class(extern)]
pub(crate) struct PDO;

/// The aliases `REGISTER_PDO_MYSQL_CLASS_CONST_LONG_DEPRECATED_ALIAS_85`
/// declares on `PDO`, in `PHP_MINIT_FUNCTION(pdo_mysql)`'s order.
///
/// ```php
/// var_dump(PDO::MYSQL_ATTR_LOCAL_INFILE_DIRECTORY);
/// ```
/// ```expect
/// Deprecated: Constant PDO::MYSQL_ATTR_LOCAL_INFILE_DIRECTORY is deprecated since 8.5, use Pdo\Mysql::ATTR_LOCAL_INFILE_DIRECTORY instead in %s on line 2
/// int(1014)
/// ```
#[php_class]
impl PDO {
    #[php(deprecated(
        since = "8.5",
        message = "use Pdo\\Mysql::ATTR_USE_BUFFERED_QUERY instead"
    ))]
    const MYSQL_ATTR_USE_BUFFERED_QUERY: i64 = Mysql::ATTR_USE_BUFFERED_QUERY;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_LOCAL_INFILE instead"))]
    const MYSQL_ATTR_LOCAL_INFILE: i64 = Mysql::ATTR_LOCAL_INFILE;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_INIT_COMMAND instead"))]
    const MYSQL_ATTR_INIT_COMMAND: i64 = Mysql::ATTR_INIT_COMMAND;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_COMPRESS instead"))]
    const MYSQL_ATTR_COMPRESS: i64 = Mysql::ATTR_COMPRESS;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_DIRECT_QUERY instead"))]
    const MYSQL_ATTR_DIRECT_QUERY: i64 = Mysql::ATTR_DIRECT_QUERY;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_FOUND_ROWS instead"))]
    const MYSQL_ATTR_FOUND_ROWS: i64 = Mysql::ATTR_FOUND_ROWS;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_IGNORE_SPACE instead"))]
    const MYSQL_ATTR_IGNORE_SPACE: i64 = Mysql::ATTR_IGNORE_SPACE;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_SSL_KEY instead"))]
    const MYSQL_ATTR_SSL_KEY: i64 = Mysql::ATTR_SSL_KEY;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_SSL_CERT instead"))]
    const MYSQL_ATTR_SSL_CERT: i64 = Mysql::ATTR_SSL_CERT;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_SSL_CA instead"))]
    const MYSQL_ATTR_SSL_CA: i64 = Mysql::ATTR_SSL_CA;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_SSL_CAPATH instead"))]
    const MYSQL_ATTR_SSL_CAPATH: i64 = Mysql::ATTR_SSL_CAPATH;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Mysql::ATTR_SSL_CIPHER instead"))]
    const MYSQL_ATTR_SSL_CIPHER: i64 = Mysql::ATTR_SSL_CIPHER;
    #[php(deprecated(
        since = "8.5",
        message = "use Pdo\\Mysql::ATTR_SERVER_PUBLIC_KEY instead"
    ))]
    const MYSQL_ATTR_SERVER_PUBLIC_KEY: i64 = Mysql::ATTR_SERVER_PUBLIC_KEY;
    #[php(deprecated(
        since = "8.5",
        message = "use Pdo\\Mysql::ATTR_MULTI_STATEMENTS instead"
    ))]
    const MYSQL_ATTR_MULTI_STATEMENTS: i64 = Mysql::ATTR_MULTI_STATEMENTS;
    #[php(deprecated(
        since = "8.5",
        message = "use Pdo\\Mysql::ATTR_SSL_VERIFY_SERVER_CERT instead"
    ))]
    const MYSQL_ATTR_SSL_VERIFY_SERVER_CERT: i64 = Mysql::ATTR_SSL_VERIFY_SERVER_CERT;
    #[php(deprecated(
        since = "8.5",
        message = "use Pdo\\Mysql::ATTR_LOCAL_INFILE_DIRECTORY instead"
    ))]
    const MYSQL_ATTR_LOCAL_INFILE_DIRECTORY: i64 = Mysql::ATTR_LOCAL_INFILE_DIRECTORY;
}
