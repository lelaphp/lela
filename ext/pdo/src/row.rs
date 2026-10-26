//! `PDORow`, what `FETCH_LAZY` hands back: one object per statement
//! that reads the statement's current row as it is asked, by column
//! name or number, and refuses every write, with the handlers
//! `pdo_stmt_init` installs on it.

use php_ext::abi::{
    DimensionRead, ObjectHandlers, PropertyPlace, PropertyRead, PropertyTest, Refusals, Std,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, lookup_class, new_object};
use php_ext::coerce::{numeric_string, to_string, truthy};
use php_ext::raising::{Thrown, throw_error, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::stmt;
use crate::throwables::Throwables;

/// `pdo_row_t`: the statement read, an owner of it.
#[derive(Default)]
pub(crate) struct Row {
    stmt: Option<Owner<PhpObject>>,
}

/// `@not-serializable final class PDORow`, as `pdo_stmt.stub.php`
/// declares it, over the statement it reads and
/// `pdo_row_object_handlers`. The object is `pdo_row_t`, which
/// `pdo_stmt_fetch` makes once for the statement's lazy fetches:
///
/// ```php
/// $p = new PDO('sqlite::memory:'); $st = $p->query('select 1 as a, 2 as b'); $st2 = $p->query('select 1 as a, 2 as b');
/// $w = $st->fetch(PDO::FETCH_LAZY);
/// $m = memory_get_usage(); $r = $st2->fetch(PDO::FETCH_LAZY); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 64
/// ```
#[php_class(final, not_serializable, size = 64, payload = Row, handlers = HANDLERS)]
pub(crate) struct PDORow;

/// `public string $queryString`, which the handlers answer from the
/// statement.
#[php_class]
impl PDORow {
    #[php(property)]
    const queryString: PhpString;
}

/// `pdo_row_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// `pdo_get_lazy_object`: the statement's one `PDORow`, made on first
/// use, holding the statement, and named by the statement without an
/// owner; one owner the caller's.
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// $s = $db->query("SELECT 1 AS a, 'x' AS b UNION SELECT 2, 'y'");
/// $row = $s->fetch(PDO::FETCH_LAZY);
/// var_dump(get_class($row), $row->a, $row['b'], $row[0], isset($row->nope), $row->queryString);
/// var_dump($row);
/// try { $row->a = 3; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new PDORow; } catch (PDOException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "PDORow"
/// int(1)
/// string(1) "x"
/// int(1)
/// bool(false)
/// string(43) "SELECT 1 AS a, 'x' AS b UNION SELECT 2, 'y'"
/// object(PDORow)#3 (3) {
///   ["queryString"]=>
///   string(43) "SELECT 1 AS a, 'x' AS b UNION SELECT 2, 'y'"
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   string(1) "x"
/// }
/// Cannot write to PDORow property
/// You may not create a PDORow manually
/// ```
pub(super) fn lazy(req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
    if let Some(row) = stmt::read(req, object, |s| s.lazy).flatten() {
        return Ok(Owner::take(req, row.as_value()));
    }
    let Some(class) = lookup_class(req, b"PDORow")? else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: PDORow is not declared",
        ));
    };
    let row = new_object(req, class)?;
    let held = Owner::take(req, object);
    match row.native_mut::<Row>(req) {
        Some(r) => r.stmt = Some(held),
        None => held.give_up(req),
    }
    stmt::write(req, object, |s| s.lazy = Some(*row));
    Ok(row.into_value())
}

/// The statement a row reads.
fn statement(req: &Request, row: PhpObject) -> Option<PhpObject> {
    row.native::<Row>(req)
        .and_then(|r| r.stmt.as_deref().copied())
}

/// `row_read_column_number`: the value of column `column` of the
/// current row, `None` past the columns.
fn by_number(
    req: &mut Request,
    stmt: PhpObject,
    column: i64,
) -> Result<Option<Owner<Value>>, Thrown> {
    let count = stmt::read(req, stmt, |s| s.column_count).unwrap_or(0);
    if usize::try_from(column).is_ok_and(|c| c < count) {
        return crate::fetch::value(req, stmt, column, None).map(Some);
    }
    Ok(None)
}

/// `row_read_column_name`: the value of the first column so named.
fn by_name(
    req: &mut Request,
    stmt: PhpObject,
    name: &[u8],
) -> Result<Option<Owner<Value>>, Thrown> {
    let at = stmt::read(req, stmt, |s| {
        s.columns
            .as_ref()
            .and_then(|columns| columns.position(name))
    })
    .flatten();
    match at {
        Some(at) => crate::fetch::value(req, stmt, i64::try_from(at).unwrap_or(-1), None).map(Some),
        None => Ok(None),
    }
}

/// A name read as a column number where it is an integer string, as
/// `is_numeric_str_function` reads it, and as a column name otherwise.
fn by_key(req: &mut Request, stmt: PhpObject, name: &[u8]) -> Result<Option<Owner<Value>>, Thrown> {
    match numeric_string(name).and_then(|v| v.as_int()) {
        Some(n) => by_number(req, stmt, n),
        None => by_name(req, stmt, name),
    }
}

/// What the table refuses: cloning and comparison.
const REFUSALS: Refusals = Refusals::NONE.cloning().comparison();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(held) = object.native_mut::<Row>(req).and_then(|r| r.stmt.take()) else {
            return;
        };
        stmt::write(req, *held, |s| s.lazy = None);
        held.give_up(req);
    }

    /// `row_prop_read`: `queryString` off the statement, a column by
    /// number or by name, `null` for anything else.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(stmt) = statement(req, object) else {
            return Ok(Owner::null());
        };
        if name == b"queryString" {
            return Std.read_property(req, stmt, name, read);
        }
        match by_key(req, stmt, name)? {
            Some(column) => Ok(column),
            None => Ok(Owner::null()),
        }
    }

    /// `row_dim_read`: a name is the property of that name, read
    /// silently under `BP_VAR_IS`.
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(offset) = offset else {
            let ce = Throwables::of(req)?;
            return Err(throw_error(req, ce.error, "Cannot append to PDORow offset"));
        };
        let Some(stmt) = statement(req, object) else {
            return Ok(Owner::null());
        };
        if let ValueRef::Int(n) = offset.deref().as_ref() {
            return match by_number(req, stmt, n)? {
                Some(column) => Ok(column),
                None => Ok(Owner::null()),
            };
        }
        let name = to_string(req, &offset.deref())?;
        let how = if read == DimensionRead::Silent {
            PropertyRead::Silent
        } else {
            PropertyRead::Warning
        };
        let read = self.read_property(req, object, name.as_bytes(), how);
        name.give_up(req);
        read
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let _ = object;
        value.give_up(req);
        name.give_up(req);
        let ce = Throwables::of(req)?;
        Err(throw_error(
            req,
            ce.error,
            "Cannot write to PDORow property",
        ))
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let _ = object;
        value.give_up(req);
        let message = if offset.is_some() {
            "Cannot write to PDORow offset"
        } else {
            "Cannot append to PDORow offset"
        };
        let ce = Throwables::of(req)?;
        Err(throw_error(req, ce.error, message))
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        let _ = (req, object, name);
        Ok(PropertyPlace::Handlers)
    }

    /// `row_prop_exists`: a column there and not `null`, or not empty;
    /// php reads `property_exists` as the second.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let Some(stmt) = statement(req, object) else {
            return Ok(false);
        };
        let Some(value) = by_key(req, stmt, name)? else {
            return Ok(false);
        };
        let answer = match test {
            PropertyTest::Isset => Ok(!crate::is_null(&value)),
            PropertyTest::NotEmpty | PropertyTest::Exists => truthy(req, &value),
        };
        value.give_up(req);
        answer
    }

    /// `row_dim_exists`.
    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        let Some(stmt) = statement(req, object) else {
            return Ok(false);
        };
        if let ValueRef::Int(n) = offset.deref().as_ref() {
            if !not_empty {
                let count = stmt::read(req, stmt, |s| s.column_count).unwrap_or(0);
                return Ok(usize::try_from(n).is_ok_and(|c| c < count));
            }
            let Some(value) = by_number(req, stmt, n)? else {
                return Ok(false);
            };
            let answer = truthy(req, &value);
            value.give_up(req);
            let answer = answer?;
            return Ok(answer);
        }
        let name = to_string(req, &offset.deref())?;
        let test = if not_empty {
            PropertyTest::NotEmpty
        } else {
            PropertyTest::Isset
        };
        let has = self.has_property(req, object, name.as_bytes(), test);
        name.give_up(req);
        has
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let _ = (object, name);
        let ce = Throwables::of(req)?;
        Err(throw_error(req, ce.error, "Cannot unset PDORow property"))
    }

    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        let _ = (object, offset);
        let ce = Throwables::of(req)?;
        Err(throw_error(req, ce.error, "Cannot unset PDORow offset"))
    }

    /// What `var_dump` shows, the table `row_get_properties_for` builds
    /// for `ZEND_PROP_PURPOSE_DEBUG`, one owner the caller's, as php's
    /// temporary table is the reader's to release: the statement's
    /// properties, then each column but one named `queryString`, its
    /// value fetched for this table and handed to it.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        let Some(stmt) = statement(req, object) else {
            return Ok(None);
        };
        let mut out = PhpArray::new(req)?;
        let filled = debug_rows(req, stmt, &mut out);
        if let Err(thrown) = filled {
            out.give_up(req);
            return Err(thrown);
        }
        Ok(Some(out.into()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    /// `row_get_ctor`.
    fn get_constructor(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        let _ = object;
        let ce = Throwables::of(req)?;
        Err(throw_exception_ex(
            req,
            ce.pdo_exception,
            "You may not create a PDORow manually",
        ))
    }
}

/// `row_get_properties_for`'s table under `ZEND_PROP_PURPOSE_DEBUG`
/// laid into `out`: `zend_array_dup` of the statement's properties, each
/// row one more owner, then each column but `queryString` fetched,
/// `zend_hash_update` under its name.
fn debug_rows(req: &mut Request, stmt: PhpObject, out: &mut Owner<PhpArray>) -> Result<(), Thrown> {
    let table = Std.get_properties(req, stmt)?;
    let rows: Vec<(Key, Value)> = walk::rows(req, table).collect();
    for (key, value) in rows {
        out.set(req, Owner::take(req, key), Owner::take(req, value))?;
    }
    let count = stmt::read(req, stmt, |s| s.columns.as_ref().map_or(0, |c| c.len())).unwrap_or(0);
    for at in 0..count {
        let name =
            |req: &mut Request| stmt::read(req, stmt, |s| s.columns.as_ref()?.name(at)).flatten();
        if name(req).is_none_or(|n| n.as_bytes() == b"queryString") {
            continue;
        }
        let value = crate::fetch::value(req, stmt, i64::try_from(at).unwrap_or(-1), None)?;
        // `zend_hash_update(props, stmt->columns[i].name, &val)`, the
        // name read after the value as php reads it.
        match name(req) {
            Some(name) => out.set(req, Owner::take(req, name).into_key(), value)?,
            None => value.give_up(req),
        }
    }
    Ok(())
}
