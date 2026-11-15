//! The `pdo` extension: `php-src/ext/pdo` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`, and
//! beside it the connector face, [`driver`], which is what the pdo connectors
//! depend on this crate for (design §4.10). The phpt subset is
//! `php-src/ext/pdo/tests`, through `xtask phpt --subset`.
//!
//! `PDOException`, `PDO`, `PDOStatement` and `PDORow` are declared by the
//! attributes as their stubs write them, registered in the order
//! `PHP_MINIT_FUNCTION(pdo)` registers them. A `PDO` carries its connection as its native payload
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

mod bound;
pub mod connector;
mod dbh;
mod error;
mod fetch;
mod parser;
mod row;
mod stmt;
mod throwables;

use php_ext::abi::{Dependency, DependencyKind, Extension, ExtensionBuilder, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::raising::Thrown;
use php_ext::request::{Request, Supplier};
use php_ext::values::{ClassId, Owner, PhpArray, PhpString, Value, ValueRef};

use connector::Connector;
use dbh::PDO;
use row::PDORow;
use stmt::PDOStatement;

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
    ExtensionBuilder::default()
        .functions(&[pdo_drivers::DECLARED])
        .dependencies(vec![Dependency {
            name: String::from("spl"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .class::<PDOException>()
        .class::<PDO>()
        .class::<PDOStatement>()
        .class::<PDORow>()
        .info(pdo_info)
        .request_state::<dbh::Persistent>()
        .engine_state(|start| {
            Ok(dbh::Pooled {
                pool: php_ext::abi::Pool::new(dbh::POOLED),
                throwables: throwables::Throwables::start(start)?,
            })
        })
        .post_deactivate(dbh::post_deactivate)
        .engine_end(dbh::engine_end)
        .build("PDO")
}

/// `class RuntimeException`, spl's, which `PDOException` extends.
#[php_class(extern)]
pub(crate) struct RuntimeException;

/// `class PDOException extends RuntimeException`, as `pdo.stub.php`
/// declares it.
#[php_class(extends(RuntimeException))]
pub(crate) struct PDOException;

#[php_class]
impl PDOException {
    #[php(property, protected)]
    const code: Untyped<Value> = 0_i64;
    #[php(property)]
    const errorInfo: Option<PhpArray> = None;
}

/// `PHP_MINFO_FUNCTION(pdo)`: support, and the registered connectors
/// joined by `, `.
fn pdo_info(req: &mut Request) {
    let drivers: Vec<&str> = drivers(req).map(connector::Connector::name).collect();
    let drivers = drivers.join(", ");
    req.extensions_mut()
        .info_table(&[&["PDO support", "enabled"], &["PDO drivers", &drivers]]);
}

/// `pdo_driver_hash`: the connectors the bundle's extensions registered,
/// in `MINIT` order.
fn drivers(req: &Request) -> impl Iterator<Item = &'static dyn Connector> + '_ {
    req.registered_by::<&'static dyn Connector>()
        .map(|(_, connector)| *connector)
}

/// The extension that registered `driver`, as whose code its calls are
/// served, so what a connector allocates is its own extension's in a debug
/// build and not ext/pdo's.
fn supplier_of(req: &Request, driver: &'static dyn Connector) -> Option<Supplier> {
    req.registered_by::<&'static dyn Connector>()
        .find(|(_, registered)| registered.name() == driver.name())
        .map(|(supplier, _)| supplier)
}

/// `f` served as `driver`'s extension, where one registered it.
pub(crate) fn as_driver<R>(
    req: &mut Request,
    driver: Option<&'static dyn Connector>,
    f: impl FnOnce(&mut Request) -> R,
) -> R {
    match driver.and_then(|driver| supplier_of(req, driver)) {
        Some(supplier) => req.code_mut().serve_supplied(supplier, f),
        None => f(req),
    }
}

/// The registered connectors' names, one string each, in `MINIT` order:
/// what `pdo_drivers()` and `PDO::getAvailableDrivers()` answer.
fn driver_names(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let names: Vec<&'static str> = drivers(req).map(connector::Connector::name).collect();
    let mut out = PhpArray::new(req)?;
    for name in names {
        let name = PhpString::new(req, name.as_bytes())?;
        out.append(req, name.into_value())?;
    }
    Ok(out)
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
///
/// ```php
/// $f = new ReflectionFunction("pdo_drivers");
/// echo $f->getReturnType(), " ", $f->getNumberOfParameters(), " ", $f->getExtensionName(), "\n";
/// ```
/// ```expect
/// array 0 PDO
/// ```
#[php_function]
fn pdo_drivers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    driver_names(req)
}

// ---- what the classes share

/// Whether `value` is `null`, a reference followed.
fn is_null(value: &Value) -> bool {
    matches!(value.deref().as_ref(), ValueRef::Null | ValueRef::Undef)
}

/// A string value of `bytes`, one owner the caller's.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `ce->name`.
fn class_name(req: &Request, class: ClassId) -> String {
    req.symbols()
        .class_decl(class)
        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
        .unwrap_or_default()
}

/// `instanceof_function` over the parents alone, which is all PDO asks
/// of a class: whether `class` is or extends the class named `of`.
fn extends(req: &mut Request, class: ClassId, of: &[u8]) -> bool {
    req.symbols()
        .lineage(class)
        .map_while(|class| req.symbols().class_decl(class))
        .any(|decl| decl.name.0.eq_ignore_ascii_case(of))
}
