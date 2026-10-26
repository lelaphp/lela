//! The `pdo_sqlite` extension: `php-src/ext/pdo_sqlite` as one crate
//! under `ext/` (ADR-0004), the sqlite connector behind `ext/pdo`'s connector
//! face over libsqlite3 through `libsqlite3-sys` bundled (design §7.2).
//! Its dependencies are the facade and `pdo`, as `ZEND_MOD_REQUIRED("pdo")`
//! has it; its one entry is `extension()`. The phpt subset is
//! `php-src/ext/pdo_sqlite/tests`, through `xtask phpt --subset`.
//!
//! `PHP_MINIT_FUNCTION(pdo_sqlite)` as a value: the connector registered
//! with PDO (`sqlite::Driver`), `Pdo\Sqlite` declared as
//! `pdo_sqlite.stub.php` declares it with its methods, the three
//! `sqliteCreate*` methods laid on `PDO` as the connector's methods
//! (`sqlite_driver.stub.php`), and the deprecated `PDO::SQLITE_*`
//! aliases on `PDO`. The `unsafe` the library needs is `src/ffi/`'s
//! alone, and [`ffi`] is the one binding of libsqlite3, which `ext/sqlite3`
//! reaches through its safe types (design §4.10, §7.2).
#![deny(unsafe_code)]
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

mod blob;
mod callbacks;
pub mod ffi;

pub use blob::BlobStream;
mod sqlite;
mod throwables;

use php_ext::abi::{
    Callable, Dependency, DependencyKind, Extension, ExtensionBuilder, Path, Raw, This, Untyped,
};
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, argument_error, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};
use php_ext_pdo::connector::Connector;

use ffi::Function;

/// The extension value (design §7.1): the connector registered, `Pdo\Sqlite`,
/// the connector's methods and the aliases laid on `PDO`, `pdo` as the one
/// dependency and the `MINFO` table.
///
/// ```php
/// var_dump(extension_loaded('pdo_sqlite'), in_array('sqlite', PDO::getAvailableDrivers()), Pdo\Sqlite::OPEN_CREATE, Pdo\Sqlite::ATTR_TRANSACTION_MODE);
/// var_dump((new ReflectionClass('Pdo\Sqlite'))->getParentClass()->getName(), (new ReflectionExtension('pdo_sqlite'))->getDependencies());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(4)
/// int(1005)
/// string(3) "PDO"
/// array(1) {
///   ["pdo"]=>
///   string(8) "Required"
/// }
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![Dependency {
            name: String::from("pdo"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .class::<Sqlite>()
        .class::<PDO>()
        .engine_state(throwables::Throwables::start)
        .register::<&'static dyn Connector>(&sqlite::SQLITE)
        .info(pdo_sqlite_info)
        .build("pdo_sqlite")
}

/// `PHP_MINFO_FUNCTION(pdo_sqlite)`.
fn pdo_sqlite_info(req: &mut Request) {
    let version = ffi::version();
    req.extensions_mut().info_table(&[
        &["PDO Driver for SQLite 3.x", "enabled"],
        &["SQLite Library", &version],
    ]);
}

// ---- the declarations

/// `class Pdo\Sqlite extends PDO`, `@strict-properties`, with its
/// constants and methods in the stub's order.
#[php_class(namespace(Pdo), extends(PDO), dynamic_properties = forbidden)]
pub(crate) struct Sqlite;

#[php_class]
impl Sqlite {
    const DETERMINISTIC: i64 = 2048;
    const OPEN_READONLY: i64 = 1;
    const OPEN_READWRITE: i64 = 2;
    const OPEN_CREATE: i64 = 4;
    const ATTR_OPEN_FLAGS: i64 = 1000;
    const ATTR_READONLY_STATEMENT: i64 = 1001;
    const ATTR_EXTENDED_RESULT_CODES: i64 = 1002;
    const ATTR_BUSY_STATEMENT: i64 = 1003;
    const ATTR_EXPLAIN_STATEMENT: i64 = 1004;
    const ATTR_TRANSACTION_MODE: i64 = 1005;
    const TRANSACTION_MODE_DEFERRED: i64 = 0;
    const TRANSACTION_MODE_IMMEDIATE: i64 = 1;
    const TRANSACTION_MODE_EXCLUSIVE: i64 = 2;
    const EXPLAIN_MODE_PREPARED: i64 = 0;
    const EXPLAIN_MODE_EXPLAIN: i64 = 1;
    const EXPLAIN_MODE_EXPLAIN_QUERY_PLAN: i64 = 2;
    const OK: i64 = 0;
    const DENY: i64 = 1;
    const IGNORE: i64 = 2;

    /// `Pdo\Sqlite::createAggregate(string $name, callable $step, callable $finalize, int $numArgs = -1): bool`:
    /// the step handed the aggregate's value by reference and the row number.
    ///
    /// ```php
    /// $db = new Pdo\Sqlite('sqlite::memory:');
    /// $db->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('x'), ('y'), ('z')");
    /// $db->createAggregate('cat', function (&$c, $row, $v) { $c .= "$row$v"; return $c; }, fn ($c, $rows) => "$c/$rows");
    /// var_dump($db->query('SELECT cat(a) FROM t')->fetchColumn());
    /// ```
    /// ```expect
    /// string(8) "1x2y3z/4"
    /// ```
    fn createAggregate(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        step: Callable,
        finalize: Callable,
        #[php(default = -1)] numArgs: i64,
    ) -> Result<bool, Thrown> {
        create_aggregate_as(req, this.object(), name, (step, finalize), numArgs)
    }

    /// `Pdo\Sqlite::createCollation(string $name, callable $callback): bool`.
    ///
    /// ```php
    /// $db = new Pdo\Sqlite('sqlite::memory:');
    /// $db->exec("CREATE TABLE t (a); INSERT INTO t VALUES ('a10'), ('a9'), ('a1')");
    /// $db->createCollation('NAT', 'strnatcmp');
    /// var_dump($db->query('SELECT a FROM t ORDER BY a COLLATE NAT')->fetchAll(PDO::FETCH_COLUMN));
    /// $db->createCollation('BAD', fn ($a, $b) => 'x');
    /// try { $db->query('SELECT a FROM t ORDER BY a COLLATE BAD'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   string(2) "a1"
    ///   [1]=>
    ///   string(2) "a9"
    ///   [2]=>
    ///   string(3) "a10"
    /// }
    /// PDO::query(): Return value of the collation callback must be of type int, string returned
    /// ```
    fn createCollation(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        callback: Callable,
    ) -> Result<bool, Thrown> {
        create_collation_as(req, this.object(), name, callback)
    }

    /// `Pdo\Sqlite::createFunction(string $function_name, callable $callback, int $num_args = -1, int $flags = 0): bool`.
    ///
    /// ```php
    /// $db = new Pdo\Sqlite('sqlite::memory:');
    /// var_dump($db->createFunction('twice', fn ($x) => $x * 2, 1, Pdo\Sqlite::DETERMINISTIC));
    /// var_dump($db->query('SELECT twice(21), twice(1.5)')->fetch(PDO::FETCH_NUM));
    /// $db->createFunction('boom', function () { throw new Exception('boom'); });
    /// try { $db->query('SELECT boom()'); } catch (Exception $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// array(2) {
    ///   [0]=>
    ///   int(42)
    ///   [1]=>
    ///   float(3)
    /// }
    /// Exception: boom
    /// ```
    ///
    /// The callback is kept as `zend_fcc_dup` keeps it, so a
    /// `Class::method` string resolved with `$this` in scope keeps that
    /// object until the connection gives the function up:
    ///
    /// ```php
    /// class B { public $n = 'held'; function __destruct(){ echo "dtor\n"; } function f($x){ return $this->n . $x; }
    ///   function reg($pdo){ $pdo->createFunction('f', 'B::f'); } }
    /// $pdo = PDO::connect('sqlite::memory:');
    /// (new B)->reg($pdo);
    /// echo "registered\n";
    /// var_dump($pdo->query("SELECT f('2')")->fetchColumn());
    /// $pdo = null;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// registered
    /// string(5) "held2"
    /// dtor
    /// end
    /// ```
    fn createFunction(
        req: &mut Request,
        this: This<Self>,
        function_name: PhpString,
        callback: Callable,
        #[php(default = -1)] num_args: i64,
        #[php(default = 0)] flags: i64,
    ) -> Result<bool, Thrown> {
        create_function_as(
            req,
            this.object(),
            function_name,
            callback,
            (num_args, flags),
        )
    }

    /// `Pdo\Sqlite::loadExtension(string $name): void`: a library loaded
    /// through the C API, php's `PDOException` for one that is not there.
    ///
    /// ```php
    /// $db = new Pdo\Sqlite('sqlite::memory:');
    /// try { $db->loadExtension('/no/such/extension'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// try { $db->loadExtension(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Unable to load extension "/no/such/extension"
    /// Pdo\Sqlite::loadExtension(): Argument #1 ($name) must not be empty
    /// ```
    fn loadExtension(req: &mut Request, this: This<Self>, name: Path) -> Result<(), Thrown> {
        let name = name.0.as_bytes().to_vec();
        let ce = throwables::Throwables::of(req)?;
        if name.is_empty() {
            return Err(argument_error(req, ce.value_error, 1, "must not be empty"));
        }
        let this = this.object();
        php_ext_pdo::connector::with_connection(req, this, |req, conn| {
            let shown = String::from_utf8_lossy(&name).into_owned();
            let Some(full) = req.environment_mut().realpath(&name) else {
                return Err(throw_exception_ex(
                    req,
                    ce.pdo_exception,
                    &format!("Unable to load extension \"{shown}\""),
                ));
            };
            let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<sqlite::Handle>()
            else {
                return Ok(());
            };
            let path = full.to_string_lossy().into_owned();
            let Some(db) = handle.db() else {
                return Ok(());
            };
            match db.load_extension(req, path.as_bytes()) {
                Ok(()) => Ok(()),
                Err(text) => {
                    let text = String::from_utf8_lossy(&text).into_owned();
                    Err(throw_exception_ex(
                        req,
                        ce.pdo_exception,
                        &format!("Unable to load extension \"{text}\""),
                    ))
                }
            }
        })?;
        Ok(())
    }

    /// `Pdo\Sqlite::openBlob(string $table, string $column, int $rowid, ?string $dbname = "main", int $flags = Pdo\Sqlite::OPEN_READONLY)`:
    /// the blob as a stream, read only unless the bits ask to write, php's
    /// warning and `false` where the library cannot open it.
    ///
    /// ```php
    /// $db = new Pdo\Sqlite('sqlite::memory:');
    /// $db->exec("CREATE TABLE t (b BLOB); INSERT INTO t VALUES ('hello')");
    /// $s = $db->openBlob('t', 'b', 1);
    /// var_dump(stream_get_contents($s), fwrite($s, 'x'));
    /// $w = $db->openBlob('t', 'b', 1, 'main', Pdo\Sqlite::OPEN_READWRITE);
    /// var_dump(fwrite($w, 'J'), fwrite($w, 'too long'));
    /// fclose($w);
    /// var_dump($db->query('SELECT b FROM t')->fetchColumn(), $db->openBlob('t', 'b', 9));
    /// ```
    /// ```expect
    /// Warning: fwrite(): Can't write to blob stream: is open as read only in %s on line 5
    /// string(5) "hello"
    /// bool(false)
    ///
    /// Warning: fwrite(): It is not possible to increase the size of a BLOB in %s on line 7
    /// int(1)
    /// bool(false)
    ///
    /// Warning: Unable to open blob: no such rowid: 9 in %s on line 9
    /// string(5) "Jello"
    /// bool(false)
    /// ```
    ///
    /// `PDO_CONSTRUCT_CHECK` comes before php's ZPP here, so the body
    /// counts and holds its arguments itself.
    #[php(arity = manual)]
    fn openBlob(
        req: &mut Request,
        this: This<Self>,
        table: Raw<PhpString>,
        column: Raw<PhpString>,
        rowid: Raw<i64>,
        #[php(default = "main")] dbname: Raw<Option<PhpString>>,
        #[php(default = Pdo::Sqlite::OPEN_READONLY)] flags: Raw<i64>,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        blob::open_blob(req, this.object(), [table, column], rowid, dbname, flags).map(Untyped)
    }

    /// `Pdo\Sqlite::setAuthorizer(?callable $callback): void`: the callback
    /// asked of every action a statement prepares, or none.
    ///
    /// ```php
    /// $db = new Pdo\Sqlite('sqlite::memory:');
    /// $db->exec('CREATE TABLE t (a)');
    /// $db->setAuthorizer(fn ($action, $a, $b, $c, $d) => $a === 't' ? Pdo\Sqlite::DENY : Pdo\Sqlite::OK);
    /// try { $db->query('SELECT * FROM t'); } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
    /// $db->setAuthorizer(null);
    /// var_dump($db->query('SELECT COUNT(*) FROM t')->fetchColumn());
    /// ```
    /// ```expect
    /// SQLSTATE[HY000]: General error: 23 access to t.a is prohibited
    /// int(0)
    /// ```
    fn setAuthorizer(
        req: &mut Request,
        this: This<Self>,
        callback: Option<Callable>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        // `PDO_CONSTRUCT_CHECK` throws before `zend_fcc_dup` keeps the
        // callback, so the callback is kept only once a connection stands.
        php_ext_pdo::connector::with_connection(req, this, |req, conn| {
            let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<sqlite::Handle>()
            else {
                return Ok(());
            };
            let handler = callback.map(|callable| Owner::take(req, callable));
            if let Some(db) = handle.db() {
                db.set_authorizer(req, handler.as_deref());
            }
            if let Some(old) = std::mem::replace(&mut handle.authorizer, handler) {
                old.give_up(req);
            }
            Ok(())
        })
    }
}

/// `\PDO`, which `pdo` declares: what `Pdo\Sqlite` extends, and the class
/// this connector lays its methods and the deprecated `SQLITE_*` aliases on.
#[php_class(extern)]
pub(crate) struct PDO;

/// The aliases `REGISTER_PDO_SQLITE_CLASS_CONST_LONG_DEPRECATED_ALIAS_85`
/// declares on `PDO`, in `PHP_MINIT_FUNCTION(pdo_sqlite)`'s order, and
/// `class_PDO_SQLite_Ext_methods`: the three methods laid on `PDO` for a
/// connection of this connector, each `#[\Deprecated]` as
/// `pdo_hash_methods` marks it.
#[php_class(trampolines)]
impl PDO {
    #[php(deprecated(since = "8.5", message = "use Pdo\\Sqlite::DETERMINISTIC instead"))]
    const SQLITE_DETERMINISTIC: i64 = Sqlite::DETERMINISTIC;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Sqlite::ATTR_OPEN_FLAGS instead"))]
    const SQLITE_ATTR_OPEN_FLAGS: i64 = Sqlite::ATTR_OPEN_FLAGS;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Sqlite::OPEN_READONLY instead"))]
    const SQLITE_OPEN_READONLY: i64 = Sqlite::OPEN_READONLY;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Sqlite::OPEN_READWRITE instead"))]
    const SQLITE_OPEN_READWRITE: i64 = Sqlite::OPEN_READWRITE;
    #[php(deprecated(since = "8.5", message = "use Pdo\\Sqlite::OPEN_CREATE instead"))]
    const SQLITE_OPEN_CREATE: i64 = Sqlite::OPEN_CREATE;
    #[php(deprecated(
        since = "8.5",
        message = "use Pdo\\Sqlite::ATTR_READONLY_STATEMENT instead"
    ))]
    const SQLITE_ATTR_READONLY_STATEMENT: i64 = Sqlite::ATTR_READONLY_STATEMENT;
    #[php(deprecated(
        since = "8.5",
        message = "use Pdo\\Sqlite::ATTR_EXTENDED_RESULT_CODES instead"
    ))]
    const SQLITE_ATTR_EXTENDED_RESULT_CODES: i64 = Sqlite::ATTR_EXTENDED_RESULT_CODES;

    /// `PDO::sqliteCreateFunction()`: `createFunction()` under its
    /// deprecation.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// $db->sqliteCreateFunction('up', 'strtoupper', 1);
    /// var_dump($db->query("SELECT up('a')")->fetchColumn());
    /// ```
    /// ```expect
    /// Deprecated: Method PDO::sqliteCreateFunction() is deprecated since 8.5, use Pdo\Sqlite::createFunction() instead in %s on line 3
    /// string(1) "A"
    /// ```
    #[php(
        tentative,
        deprecated(since = "8.5", message = "use Pdo\\Sqlite::createFunction() instead")
    )]
    fn sqliteCreateFunction(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        callback: Callable,
        #[php(default = -1)] numArgs: i64,
        #[php(default = 0)] flags: i64,
    ) -> Result<bool, Thrown> {
        create_function_as(req, this.object(), name, callback, (numArgs, flags))
    }

    /// `PDO::sqliteCreateAggregate()`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->sqliteCreateAggregate('n', fn ($c) => $c + 1, fn ($c) => $c));
    /// var_dump($db->query('SELECT n(1)')->fetchColumn());
    /// ```
    /// ```expect
    /// Deprecated: Method PDO::sqliteCreateAggregate() is deprecated since 8.5, use Pdo\Sqlite::createAggregate() instead in %s on line 3
    /// bool(true)
    /// int(1)
    /// ```
    #[php(
        tentative,
        deprecated(since = "8.5", message = "use Pdo\\Sqlite::createAggregate() instead")
    )]
    fn sqliteCreateAggregate(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        step: Callable,
        finalize: Callable,
        #[php(default = -1)] numArgs: i64,
    ) -> Result<bool, Thrown> {
        create_aggregate_as(req, this.object(), name, (step, finalize), numArgs)
    }

    /// `PDO::sqliteCreateCollation()`.
    ///
    /// ```php
    /// $db = new PDO('sqlite::memory:');
    /// var_dump($db->sqliteCreateCollation('REV', fn ($a, $b) => strcmp($b, $a)));
    /// ```
    /// ```expect
    /// Deprecated: Method PDO::sqliteCreateCollation() is deprecated since 8.5, use Pdo\Sqlite::createCollation() instead in %s on line 3
    /// bool(true)
    /// ```
    #[php(
        tentative,
        deprecated(since = "8.5", message = "use Pdo\\Sqlite::createCollation() instead")
    )]
    fn sqliteCreateCollation(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        callback: Callable,
    ) -> Result<bool, Thrown> {
        create_collation_as(req, this.object(), name, callback)
    }
}

// ---- the bodies each method shares with its deprecated twin

fn create_function_as(
    req: &mut Request,
    this: PhpObject,
    name: PhpString,
    callback: Callable,
    (argc, flags): (i64, i64),
) -> Result<bool, Thrown> {
    let name = name.as_bytes().to_vec();
    sqlite::register(req, this, &name, argc, flags, |req| {
        Function::Scalar(Owner::take(req, callback))
    })
}

fn create_aggregate_as(
    req: &mut Request,
    this: PhpObject,
    name: PhpString,
    (step, finalize): (Callable, Callable),
    argc: i64,
) -> Result<bool, Thrown> {
    let name = name.as_bytes().to_vec();
    sqlite::register(req, this, &name, argc, 0, |req| Function::Aggregate {
        step: Owner::take(req, step),
        finalize: Owner::take(req, finalize),
    })
}

fn create_collation_as(
    req: &mut Request,
    this: PhpObject,
    name: PhpString,
    callback: Callable,
) -> Result<bool, Thrown> {
    let name = name.as_bytes().to_vec();
    sqlite::register_collation(req, this, &name, callback)
}
