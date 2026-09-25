//! The `pdo` extension: `php-src/ext/pdo` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`, and
//! beside it the connector face, [`driver`], which is what the pdo connectors
//! depend on this crate for (design §4.10). The phpt subset is
//! `php-src/ext/pdo/tests`, through `xtask phpt --subset`.
//!
//! `PDOException`, `PDO`, `PDOStatement` and `PDORow` are declared off
//! the text of their stubs ([`stub`]) in the order `PHP_MINIT_FUNCTION(pdo)`
//! registers them. A `PDO` carries its connection as its native payload
//! (`dbh::Dbh`, `pdo_dbh_t`), a `PDOStatement` its statement
//! (`stmt::Stmt`, `pdo_stmt_t`) and a `PDORow` the statement it reads
//! (`row::Row`), each with the handlers `pdo_dbh.c` and `pdo_stmt.c`
//! install. The connectors are whatever the bundle's extensions registered
//! as a [`driver::Driver`], php's `pdo_driver_hash`, read through the
//! request and never held here (ADR-0005).
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

pub mod connector;
mod dbh;
mod error;
mod fetch;
mod parser;
mod row;
mod stmt;
mod stub;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::coerce::argument;
use php_ext::raising::{Thrown, arg_count_error};
use php_ext::request::Request;
use php_ext::values::{ClassId, PhpArray, PhpObject, PhpString, Value, ValueRef};

use connector::Connector;

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The four classes as `pdo.stub.php`, `pdo_dbh.stub.php` and
/// `pdo_stmt.stub.php` declare them, the `@cvalue` of each `UNKNOWN`
/// written in its place.
const STUB: &str = r#"
class PDOException extends RuntimeException {
    protected $code = 0;
    public ?array $errorInfo = null;
}
class PDO {
    public const int PARAM_NULL = 0;
    public const int PARAM_BOOL = 5;
    public const int PARAM_INT = 1;
    public const int PARAM_STR = 2;
    public const int PARAM_LOB = 3;
    public const int PARAM_STMT = 4;
    public const int PARAM_INPUT_OUTPUT = 2147483648;
    public const int PARAM_STR_NATL = 1073741824;
    public const int PARAM_STR_CHAR = 536870912;
    public const int PARAM_EVT_ALLOC = 0;
    public const int PARAM_EVT_FREE = 1;
    public const int PARAM_EVT_EXEC_PRE = 2;
    public const int PARAM_EVT_EXEC_POST = 3;
    public const int PARAM_EVT_FETCH_PRE = 4;
    public const int PARAM_EVT_FETCH_POST = 5;
    public const int PARAM_EVT_NORMALIZE = 6;
    public const int FETCH_DEFAULT = 0;
    public const int FETCH_LAZY = 1;
    public const int FETCH_ASSOC = 2;
    public const int FETCH_NUM = 3;
    public const int FETCH_BOTH = 4;
    public const int FETCH_OBJ = 5;
    public const int FETCH_BOUND = 6;
    public const int FETCH_COLUMN = 7;
    public const int FETCH_CLASS = 8;
    public const int FETCH_INTO = 9;
    public const int FETCH_FUNC = 10;
    public const int FETCH_GROUP = 32;
    public const int FETCH_UNIQUE = 64;
    public const int FETCH_KEY_PAIR = 12;
    public const int FETCH_CLASSTYPE = 128;
    public const int FETCH_SERIALIZE = 512;
    public const int FETCH_PROPS_LATE = 256;
    public const int FETCH_NAMED = 11;
    public const int ATTR_AUTOCOMMIT = 0;
    public const int ATTR_PREFETCH = 1;
    public const int ATTR_TIMEOUT = 2;
    public const int ATTR_ERRMODE = 3;
    public const int ATTR_SERVER_VERSION = 4;
    public const int ATTR_CLIENT_VERSION = 5;
    public const int ATTR_SERVER_INFO = 6;
    public const int ATTR_CONNECTION_STATUS = 7;
    public const int ATTR_CASE = 8;
    public const int ATTR_CURSOR_NAME = 9;
    public const int ATTR_CURSOR = 10;
    public const int ATTR_ORACLE_NULLS = 11;
    public const int ATTR_PERSISTENT = 12;
    public const int ATTR_STATEMENT_CLASS = 13;
    public const int ATTR_FETCH_TABLE_NAMES = 14;
    public const int ATTR_FETCH_CATALOG_NAMES = 15;
    public const int ATTR_DRIVER_NAME = 16;
    public const int ATTR_STRINGIFY_FETCHES = 17;
    public const int ATTR_MAX_COLUMN_LEN = 18;
    public const int ATTR_EMULATE_PREPARES = 20;
    public const int ATTR_DEFAULT_FETCH_MODE = 19;
    public const int ATTR_DEFAULT_STR_PARAM = 21;
    public const int ERRMODE_SILENT = 0;
    public const int ERRMODE_WARNING = 1;
    public const int ERRMODE_EXCEPTION = 2;
    public const int CASE_NATURAL = 0;
    public const int CASE_LOWER = 2;
    public const int CASE_UPPER = 1;
    public const int NULL_NATURAL = 0;
    public const int NULL_EMPTY_STRING = 1;
    public const int NULL_TO_STRING = 2;
    public const string ERR_NONE = "00000";
    public const int FETCH_ORI_NEXT = 0;
    public const int FETCH_ORI_PRIOR = 1;
    public const int FETCH_ORI_FIRST = 2;
    public const int FETCH_ORI_LAST = 3;
    public const int FETCH_ORI_ABS = 4;
    public const int FETCH_ORI_REL = 5;
    public const int CURSOR_FWDONLY = 0;
    public const int CURSOR_SCROLL = 1;
    public function __construct(string $dsn, ?string $username = null, #[\SensitiveParameter] ?string $password = null, ?array $options = null) {}
    public static function connect(string $dsn, ?string $username = null, #[\SensitiveParameter] ?string $password = null, ?array $options = null): static {}
    /** @tentative-return-type */ public function beginTransaction(): bool {}
    /** @tentative-return-type */ public function commit(): bool {}
    /** @tentative-return-type */ public function errorCode(): ?string {}
    /** @tentative-return-type */ public function errorInfo(): array {}
    /** @tentative-return-type */ public function exec(string $statement): int|false {}
    /** @tentative-return-type */ public function getAttribute(int $attribute): mixed {}
    /** @tentative-return-type */ public static function getAvailableDrivers(): array {}
    /** @tentative-return-type */ public function inTransaction(): bool {}
    /** @tentative-return-type */ public function lastInsertId(?string $name = null): string|false {}
    /** @tentative-return-type */ public function prepare(string $query, array $options = []): PDOStatement|false {}
    /** @tentative-return-type */ public function query(string $query, ?int $fetchMode = null, mixed ...$fetchModeArgs): PDOStatement|false {}
    /** @tentative-return-type */ public function quote(string $string, int $type = PDO::PARAM_STR): string|false {}
    /** @tentative-return-type */ public function rollBack(): bool {}
    /** @tentative-return-type */ public function setAttribute(int $attribute, mixed $value): bool {}
}
class PDOStatement implements IteratorAggregate {
    public string $queryString;
    /** @tentative-return-type */ public function bindColumn(string|int $column, mixed &$var, int $type = PDO::PARAM_STR, int $maxLength = 0, mixed $driverOptions = null): bool {}
    /** @tentative-return-type */ public function bindParam(string|int $param, mixed &$var, int $type = PDO::PARAM_STR, int $maxLength = 0, mixed $driverOptions = null): bool {}
    /** @tentative-return-type */ public function bindValue(string|int $param, mixed $value, int $type = PDO::PARAM_STR): bool {}
    /** @tentative-return-type */ public function closeCursor(): bool {}
    /** @tentative-return-type */ public function columnCount(): int {}
    /** @tentative-return-type */ public function debugDumpParams(): ?bool {}
    /** @tentative-return-type */ public function errorCode(): ?string {}
    /** @tentative-return-type */ public function errorInfo(): array {}
    /** @tentative-return-type */ public function execute(?array $params = null): bool {}
    /** @tentative-return-type */ public function fetch(int $mode = PDO::FETCH_DEFAULT, int $cursorOrientation = PDO::FETCH_ORI_NEXT, int $cursorOffset = 0): mixed {}
    /** @tentative-return-type */ public function fetchAll(int $mode = PDO::FETCH_DEFAULT, mixed ...$args): array {}
    /** @tentative-return-type */ public function fetchColumn(int $column = 0): mixed {}
    /** @tentative-return-type */ public function fetchObject(?string $class = "stdClass", array $constructorArgs = []): object|false {}
    /** @tentative-return-type */ public function getAttribute(int $name): mixed {}
    /** @tentative-return-type */ public function getColumnMeta(int $column): array|false {}
    /** @tentative-return-type */ public function nextRowset(): bool {}
    /** @tentative-return-type */ public function rowCount(): int {}
    /** @tentative-return-type */ public function setAttribute(int $attribute, mixed $value): bool {}
    /** @tentative-return-type */ public function setFetchMode(int $mode, mixed ...$args): true {}
    public function getIterator(): Iterator {}
}
final class PDORow {
    public string $queryString;
}
"#;

/// The extension value (design §7.1): `pdo_drivers()`, the four
/// classes with their handlers, `spl` as the one dependency
/// (`ZEND_MOD_REQUIRED("spl")`) and the `MINFO` table.
///
/// ```php
/// var_dump(extension_loaded('pdo'), (new ReflectionClass('PDOException'))->getParentClass()->getName());
/// $db = new PDO('sqlite::memory:');
/// var_dump(get_class($db->query('SELECT 1')));
/// ```
/// ```expect
/// bool(true)
/// string(16) "RuntimeException"
/// string(12) "PDOStatement"
/// ```
pub fn extension() -> Extension {
    let entries: Vec<(&str, Entry)> = dbh::METHODS
        .iter()
        .chain(stmt::METHODS.iter())
        .copied()
        .collect();
    let mut builder = ExtensionBuilder::default()
        .functions(vec![(
            stub::function("pdo_drivers(): array"),
            pdo_drivers as Entry,
        )])
        .dependencies(vec![Dependency {
            name: String::from("spl"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }]);
    for (decl, methods) in stub::classes(STUB, &entries) {
        builder = match decl.name.0.as_ref() {
            b"PDO" => builder.class_payload::<dbh::Dbh>(decl, methods, &dbh::HANDLERS),
            b"PDOStatement" => builder.class_payload::<stmt::Stmt>(decl, methods, &stmt::HANDLERS),
            b"PDORow" => builder.class_payload::<row::Row>(decl, methods, &row::HANDLERS),
            _ => builder.classes(vec![(decl, methods)]),
        };
    }
    builder.info(pdo_info).build("PDO")
}

/// `PHP_MINFO_FUNCTION(pdo)`: support, and the registered connectors
/// joined by `, `.
fn pdo_info(req: &mut Request) {
    let drivers: Vec<&str> = drivers(req).map(connector::Connector::name).collect();
    let drivers = drivers.join(", ");
    req.info_table(&[&["PDO support", "enabled"], &["PDO drivers", &drivers]]);
}

/// `pdo_driver_hash`: the connectors the bundle's extensions registered,
/// in `MINIT` order.
fn drivers(req: &Request) -> impl Iterator<Item = &'static dyn Connector> + '_ {
    req.registered::<&'static dyn Connector>().copied()
}

/// The registered connectors' names, one string each, in `MINIT` order:
/// what `pdo_drivers()` and `PDO::getAvailableDrivers()` answer.
fn driver_names(req: &mut Request) -> Result<Value, Thrown> {
    let names: Vec<&'static str> = drivers(req).map(connector::Connector::name).collect();
    let mut out = PhpArray::new(req)?;
    for name in names {
        let name = PhpString::new(req, name.as_bytes())?;
        out.append(req, Value::string(name))?;
    }
    Ok(Value::array(out))
}

/// `pdo_drivers(): array`.
///
/// ```php
/// var_dump(in_array('sqlite', pdo_drivers(), true), pdo_drivers() === PDO::getAvailableDrivers());
/// try { pdo_drivers(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// pdo_drivers() expects exactly 0 arguments, 1 given
/// ```
fn pdo_drivers(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    driver_names(req)
}

// ---- the `Z_PARAM_*` readers the classes share (`Z/Zend/zend_API.h`)

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
fn arity(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (how, n) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let word = if n == 1 { "argument" } else { "arguments" };
    Err(arg_count_error(
        req,
        &format!("{how} {n} {word}, {given} given"),
    ))
}

/// `$this` of the method being served.
fn this(req: &mut Request) -> Result<PhpObject, Thrown> {
    match req.this() {
        Some(this) => Ok(this),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a PDO method served without its receiver",
        )),
    }
}

/// The argument at `position` held to its declared type, `None` where
/// the call gave none.
fn arg(req: &mut Request, position: u32) -> Result<Option<Value>, Thrown> {
    argument(req, position)
}

/// `Z_PARAM_LONG`, `default` where the call gave none.
fn long_arg(req: &mut Request, position: u32, default: i64) -> Result<i64, Thrown> {
    Ok(arg(req, position)?
        .and_then(|v| v.as_int())
        .unwrap_or(default))
}

/// `Z_PARAM_STR`: the bytes, empty where the call gave none.
fn bytes_arg(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    Ok(arg(req, position)?
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default())
}

/// `Z_PARAM_STR_OR_NULL`: `None` for `null` or nothing given.
fn nullable_bytes_arg(req: &mut Request, position: u32) -> Result<Option<Vec<u8>>, Thrown> {
    Ok(arg(req, position)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec)))
}

/// `Z_PARAM_ARRAY_OR_NULL`: `None` for `null` or nothing given.
fn array_arg(req: &mut Request, position: u32) -> Result<Option<PhpArray>, Thrown> {
    Ok(arg(req, position)?.and_then(|v| v.as_array().copied()))
}

/// Whether `value` is `null`, a reference followed.
fn is_null(value: &Value) -> bool {
    matches!(value.deref().as_ref(), ValueRef::Null | ValueRef::Undef)
}

/// A value handed on with one more owner, `ZVAL_COPY`.
fn copied(req: &Request, value: Value) -> Value {
    let value = value.deref();
    req.addref(&value);
    value
}

/// A string value of `bytes`, one owner the caller's.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// `ce->name`.
fn class_name(req: &Request, class: ClassId) -> String {
    req.class_decl(class)
        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
        .unwrap_or_default()
}

/// `instanceof_function` over the parents alone, which is all PDO asks
/// of a class: whether `class` is or extends the class named `of`.
fn extends(req: &mut Request, class: ClassId, of: &[u8]) -> bool {
    let mut at = Some(class);
    let mut depth = 0;
    while let Some(class) = at {
        let Some(decl) = req.class_decl(class) else {
            return false;
        };
        if decl.name.0.eq_ignore_ascii_case(of) {
            return true;
        }
        depth += 1;
        if depth > 64 {
            return false;
        }
        at = match decl.parent.clone() {
            Some(parent) => php_ext::call::lookup_class(req, &parent.0).ok().flatten(),
            None => None,
        };
    }
    false
}
