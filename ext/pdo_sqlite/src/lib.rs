//! The `pdo_sqlite` extension: `php-src/ext/pdo_sqlite` as one crate
//! under `ext/` (ADR-0004), the sqlite connector behind `ext/pdo`'s connector
//! face over libsqlite3 through `libsqlite3-sys` bundled (design §7.2).
//! Its dependencies are the facade and `pdo`, as `ZEND_MOD_REQUIRED("pdo")`
//! has it; its one entry is `extension()`. The phpt subset is
//! `php-src/ext/pdo_sqlite/tests`, through `xtask phpt --subset`.
//!
//! `PHP_MINIT_FUNCTION(pdo_sqlite)` as a value: the connector registered
//! with PDO (`driver::Sqlite`), `Pdo\Sqlite` declared as
//! `pdo_sqlite.stub.php` declares it with its methods, the three
//! `sqliteCreate*` methods laid on `PDO` as the connector's methods
//! (`sqlite_driver.stub.php`), and the deprecated `PDO::SQLITE_*`
//! aliases on `PDO`. The `unsafe` the library needs is `src/ffi/`'s
//! alone.
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
mod ffi;
mod sqlite;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::coerce::{argument, callable_argument};
use php_ext::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstExpr,
    DynamicProperties, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter, Type, Visibility,
};
use php_ext::raising::{Thrown, arg_count_error, argument_error, deprecated, throw};
use php_ext::request::Request;
use php_ext::values::{PhpObject, Value};
use php_ext_pdo::connector::Connector;

use callbacks::{Function, Handler};

/// A method's entry, the shape `ExtensionBuilder::classes` pairs a
/// declaration with.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The class constants of `Pdo\Sqlite`, in the stub's order.
const CONSTANTS: [(&str, i64); 19] = [
    ("DETERMINISTIC", 2048),
    ("OPEN_READONLY", 1),
    ("OPEN_READWRITE", 2),
    ("OPEN_CREATE", 4),
    ("ATTR_OPEN_FLAGS", sqlite::ATTR_OPEN_FLAGS),
    ("ATTR_READONLY_STATEMENT", sqlite::ATTR_READONLY_STATEMENT),
    (
        "ATTR_EXTENDED_RESULT_CODES",
        sqlite::ATTR_EXTENDED_RESULT_CODES,
    ),
    ("ATTR_BUSY_STATEMENT", sqlite::ATTR_BUSY_STATEMENT),
    ("ATTR_EXPLAIN_STATEMENT", sqlite::ATTR_EXPLAIN_STATEMENT),
    ("ATTR_TRANSACTION_MODE", sqlite::ATTR_TRANSACTION_MODE),
    ("TRANSACTION_MODE_DEFERRED", 0),
    ("TRANSACTION_MODE_IMMEDIATE", 1),
    ("TRANSACTION_MODE_EXCLUSIVE", 2),
    ("EXPLAIN_MODE_PREPARED", 0),
    ("EXPLAIN_MODE_EXPLAIN", 1),
    ("EXPLAIN_MODE_EXPLAIN_QUERY_PLAN", 2),
    ("OK", 0),
    ("DENY", 1),
    ("IGNORE", 2),
];

/// The aliases `REGISTER_PDO_SQLITE_CLASS_CONST_LONG_DEPRECATED_ALIAS_85`
/// declares on `PDO`, in `PHP_MINIT_FUNCTION(pdo_sqlite)`'s order.
const ALIASES: [&str; 7] = [
    "DETERMINISTIC",
    "ATTR_OPEN_FLAGS",
    "OPEN_READONLY",
    "OPEN_READWRITE",
    "OPEN_CREATE",
    "ATTR_READONLY_STATEMENT",
    "ATTR_EXTENDED_RESULT_CODES",
];

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
    let mut deprecated_constants = Vec::new();
    for alias in ALIASES {
        let value = CONSTANTS
            .iter()
            .find(|(name, _)| *name == alias)
            .map_or(0, |(_, value)| *value);
        let mut constant = class_constant(&format!("SQLITE_{alias}"), value);
        constant.attributes = vec![deprecation(&format!("use Pdo\\Sqlite::{alias} instead"))];
        deprecated_constants.push(constant);
    }
    ExtensionBuilder::default()
        .dependencies(vec![Dependency {
            name: String::from("pdo"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .classes(vec![pdo_sqlite_class()])
        .trampolines(b"PDO", driver_methods())
        .class_constants(b"PDO", deprecated_constants)
        .register::<&'static dyn Connector>(&sqlite::SQLITE)
        .info(pdo_sqlite_info)
        .build("pdo_sqlite")
}

/// `PHP_MINFO_FUNCTION(pdo_sqlite)`.
fn pdo_sqlite_info(req: &mut Request) {
    let version = ffi::version();
    req.info_table(&[
        &["PDO Driver for SQLite 3.x", "enabled"],
        &["SQLite Library", &version],
    ]);
}

// ---- the declarations

fn class_constant(name: &str, value: i64) -> ClassConstDecl {
    ClassConstDecl {
        name: Name(name.as_bytes().into()),
        visibility: Visibility::Public,
        is_final: false,
        is_case: false,
        ty: Some(Type::Int),
        value: ConstExpr::Long(value),
        attributes: Vec::new(),
        doc_comment: None,
    }
}

/// `#[\Deprecated(since: "8.5", message: ...)]`.
fn deprecation(message: &str) -> Attribute {
    let argument = |name: &[u8], value: &str| AttributeArgument {
        name: Some(Name(name.into())),
        value: ConstExpr::Str(value.as_bytes().into()),
    };
    Attribute {
        name: Name(b"Deprecated".as_slice().into()),
        arguments: vec![argument(b"since", "8.5"), argument(b"message", message)],
    }
}

fn parameter(name: &str, ty: Type, default: Option<ConstExpr>) -> Parameter {
    Parameter {
        name: Name(name.as_bytes().into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default,
        attributes: Vec::new(),
    }
}

fn method(
    name: &str,
    parameters: Vec<Parameter>,
    returns: Option<Type>,
    tentative: bool,
) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.as_bytes().into()),
            parameters,
            return_type: returns,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: tentative,
    }
}

/// What a `create*` method registers, which names its parameters.
#[derive(Clone, Copy)]
enum Creates {
    /// `createFunction`'s spelling, `$function_name` and `$num_args`.
    Function,
    /// `sqliteCreateFunction`'s, `$name` and `$numArgs`.
    FunctionAlias,
    Aggregate,
    Collation,
}

/// The parameters of the `create*` methods and their deprecated twins.
fn creating(kind: Creates) -> Vec<Parameter> {
    let minus_one = || Some(ConstExpr::Long(-1));
    let function = |name: &str, count: &str| {
        vec![
            parameter(name, Type::String, None),
            parameter("callback", Type::Callable, None),
            parameter(count, Type::Int, minus_one()),
            parameter("flags", Type::Int, Some(ConstExpr::Long(0))),
        ]
    };
    match kind {
        Creates::Function => function("function_name", "num_args"),
        Creates::FunctionAlias => function("name", "numArgs"),
        Creates::Aggregate => vec![
            parameter("name", Type::String, None),
            parameter("step", Type::Callable, None),
            parameter("finalize", Type::Callable, None),
            parameter("numArgs", Type::Int, minus_one()),
        ],
        Creates::Collation => vec![
            parameter("name", Type::String, None),
            parameter("callback", Type::Callable, None),
        ],
    }
}

/// `class Pdo\Sqlite extends PDO`, `@strict-properties`, with its
/// constants and methods in the stub's order.
fn pdo_sqlite_class() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let open_readonly = ConstExpr::ClassConstant {
        class: Name(b"Pdo\\Sqlite".as_slice().into()),
        name: Name(b"OPEN_READONLY".as_slice().into()),
    };
    let nullable = |ty: Type| Type::Nullable(Box::new(ty));
    let methods: Vec<(MethodDecl, Entry)> = vec![
        (
            method(
                "createAggregate",
                creating(Creates::Aggregate),
                Some(Type::Bool),
                false,
            ),
            create_aggregate,
        ),
        (
            method(
                "createCollation",
                creating(Creates::Collation),
                Some(Type::Bool),
                false,
            ),
            create_collation,
        ),
        (
            method(
                "createFunction",
                creating(Creates::Function),
                Some(Type::Bool),
                false,
            ),
            create_function,
        ),
        (
            method(
                "loadExtension",
                vec![parameter("name", Type::String, None)],
                Some(Type::Void),
                false,
            ),
            load_extension,
        ),
        (
            method(
                "openBlob",
                vec![
                    parameter("table", Type::String, None),
                    parameter("column", Type::String, None),
                    parameter("rowid", Type::Int, None),
                    parameter(
                        "dbname",
                        nullable(Type::String),
                        Some(ConstExpr::Str(b"main".as_slice().into())),
                    ),
                    parameter("flags", Type::Int, Some(open_readonly)),
                ],
                None,
                false,
            ),
            blob::open_blob,
        ),
        (
            method(
                "setAuthorizer",
                vec![parameter("callback", nullable(Type::Callable), None)],
                Some(Type::Void),
                false,
            ),
            set_authorizer,
        ),
    ];
    let decl = ClassDecl {
        name: Name(b"Pdo\\Sqlite".as_slice().into()),
        kind: ClassKind::Class,
        flags: ClassFlags {
            dynamic_properties: DynamicProperties::Forbidden,
            ..ClassFlags::default()
        },
        parent: Some(Name(b"PDO".as_slice().into())),
        interfaces: Vec::new(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: CONSTANTS
            .iter()
            .map(|(name, value)| class_constant(name, *value))
            .collect(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    };
    (decl, methods)
}

/// `class_PDO_SQLite_Ext_methods`: the three methods laid on `PDO` for a
/// connection of this connector, each `#[\Deprecated]` as `pdo_hash_methods`
/// marks it.
fn driver_methods() -> Vec<(MethodDecl, Entry)> {
    let marked = |name: &str, kind: Creates, new: &str| {
        let mut decl = method(name, creating(kind), Some(Type::Bool), true);
        decl.function.attributes = vec![deprecation(&format!("use Pdo\\Sqlite::{new}() instead"))];
        decl
    };
    vec![
        (
            marked(
                "sqliteCreateFunction",
                Creates::FunctionAlias,
                "createFunction",
            ),
            sqlite_create_function,
        ),
        (
            marked(
                "sqliteCreateAggregate",
                Creates::Aggregate,
                "createAggregate",
            ),
            sqlite_create_aggregate,
        ),
        (
            marked(
                "sqliteCreateCollation",
                Creates::Collation,
                "createCollation",
            ),
            sqlite_create_collation,
        ),
    ]
}

// ---- the argument readers

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

fn this(req: &mut Request) -> Result<PhpObject, Thrown> {
    match req.this() {
        Some(this) => Ok(this),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a Pdo\\Sqlite method served without its receiver",
        )),
    }
}

fn bytes_arg(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    Ok(argument(req, position)?
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default())
}

fn long_arg(req: &mut Request, position: u32, default: i64) -> Result<i64, Thrown> {
    Ok(argument(req, position)?
        .and_then(|v| v.as_int())
        .unwrap_or(default))
}

/// `Z_PARAM_FUNC`: the callable at `position` kept with its value.
fn handler_arg(req: &mut Request, position: u32) -> Result<Option<Handler>, Thrown> {
    let value = req.argument(position).unwrap_or_default();
    Ok(callable_argument(req, position)?.map(|callable| Handler::keep(req, callable, value)))
}

/// A callable the declaration requires.
fn required_handler(req: &mut Request, position: u32) -> Result<Handler, Thrown> {
    match handler_arg(req, position)? {
        Some(handler) => Ok(handler),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a callable parameter read no callable",
        )),
    }
}

// ---- the methods

/// `Method PDO::x() is deprecated since 8.5, use Pdo\Sqlite::y() instead`,
/// what `zend_deprecated_function` raises before the call.
fn deprecated_method(req: &mut Request, old: &str, new: &str) {
    deprecated(
        req,
        &format!("Method PDO::{old}() is deprecated since 8.5, use Pdo\\Sqlite::{new}() instead"),
    );
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
fn create_function(req: &mut Request) -> Result<Value, Thrown> {
    create_function_as(req)
}

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
fn sqlite_create_function(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_method(req, "sqliteCreateFunction", "createFunction");
    create_function_as(req)
}

fn create_function_as(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 4)?;
    let name = bytes_arg(req, 1)?;
    let handler = required_handler(req, 2)?;
    let argc = long_arg(req, 3, -1)?;
    let flags = long_arg(req, 4, 0)?;
    let this = this(req)?;
    sqlite::register(req, this, &name, argc, flags, Function::Scalar(handler)).map(Value::bool)
}

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
fn create_aggregate(req: &mut Request) -> Result<Value, Thrown> {
    create_aggregate_as(req)
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
fn sqlite_create_aggregate(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_method(req, "sqliteCreateAggregate", "createAggregate");
    create_aggregate_as(req)
}

fn create_aggregate_as(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 4)?;
    let name = bytes_arg(req, 1)?;
    let step = required_handler(req, 2)?;
    let finalize = required_handler(req, 3)?;
    let argc = long_arg(req, 4, -1)?;
    let this = this(req)?;
    sqlite::register(
        req,
        this,
        &name,
        argc,
        0,
        Function::Aggregate { step, finalize },
    )
    .map(Value::bool)
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
fn create_collation(req: &mut Request) -> Result<Value, Thrown> {
    create_collation_as(req)
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
fn sqlite_create_collation(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_method(req, "sqliteCreateCollation", "createCollation");
    create_collation_as(req)
}

fn create_collation_as(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let name = bytes_arg(req, 1)?;
    let handler = required_handler(req, 2)?;
    let this = this(req)?;
    sqlite::register_collation(req, this, &name, handler).map(Value::bool)
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
fn set_authorizer(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let handler = handler_arg(req, 1)?;
    let this = this(req)?;
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        if let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<sqlite::Handle>()
            && let Some(old) = handle.db.set_authorizer(handler)
        {
            old.release(req);
        }
        Ok(())
    })?;
    Ok(Value::null())
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
fn load_extension(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    if name.contains(&0) {
        return Err(argument_error(
            req,
            "ValueError",
            1,
            "must not contain any null bytes",
        ));
    }
    if name.is_empty() {
        return Err(argument_error(req, "ValueError", 1, "must not be empty"));
    }
    let this = this(req)?;
    php_ext_pdo::connector::with_connection(req, this, |req, conn| {
        let shown = String::from_utf8_lossy(&name).into_owned();
        let Some(full) = req.realpath(&name) else {
            return Err(throw(
                req,
                "PDOException",
                &format!("Unable to load extension \"{shown}\""),
            ));
        };
        let Some(handle) = (conn as &mut dyn std::any::Any).downcast_mut::<sqlite::Handle>() else {
            return Ok(());
        };
        let path = full.0.to_string_lossy().into_owned();
        match handle.db.load_extension(path.as_bytes()) {
            Ok(()) => Ok(()),
            Err(text) => {
                let text = String::from_utf8_lossy(&text).into_owned();
                Err(throw(
                    req,
                    "PDOException",
                    &format!("Unable to load extension \"{text}\""),
                ))
            }
        }
    })?;
    Ok(Value::null())
}
