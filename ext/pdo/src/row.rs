//! `PDORow`, what `FETCH_LAZY` hands back: one object per statement
//! that reads the statement's current row as it is asked, by column
//! name or number, and refuses every write, with the handlers
//! `pdo_stmt_init` installs on it.

use php_ext::abi::{
    CloneObj, CloneObjWith, DimensionRead, ObjectHandlers, PropertyPlace, PropertyPurpose,
    PropertyRead, PropertyTest, Std,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, lookup_class, new_object};
use php_ext::coerce::{numeric_string, to_string, truthy};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::stmt;

/// `pdo_row_t`: the statement read, one owner.
#[derive(Default)]
pub(crate) struct Row {
    stmt: Option<PhpObject>,
}

/// `@not-serializable final class PDORow`, as `pdo_stmt.stub.php`
/// declares it, over the statement it reads and
/// `pdo_row_object_handlers`.
#[php_class(final, not_serializable, payload = Row, handlers = HANDLERS)]
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
pub(super) fn lazy(req: &mut Request, object: PhpObject) -> Result<Value, Thrown> {
    if let Some(row) = stmt::read(req, object, |s| s.lazy).flatten() {
        req.addref(&row.as_value());
        return Ok(row.as_value());
    }
    let Some(class) = lookup_class(req, b"PDORow")? else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: PDORow is not declared",
        ));
    };
    let row = new_object(req, class)?;
    req.addref(&object.as_value());
    if let Some(r) = row.native_mut::<Row>(req) {
        r.stmt = Some(object);
    }
    stmt::write(req, object, |s| s.lazy = Some(row));
    Ok(row.as_value())
}

/// The statement a row reads.
fn statement(req: &Request, row: PhpObject) -> Option<PhpObject> {
    row.native::<Row>(req).and_then(|r| r.stmt)
}

/// `row_read_column_number`: the value of column `column` of the
/// current row, `None` past the columns.
fn by_number(req: &mut Request, stmt: PhpObject, column: i64) -> Result<Option<Value>, Thrown> {
    let count = stmt::read(req, stmt, |s| s.column_count).unwrap_or(0);
    if usize::try_from(column).is_ok_and(|c| c < count) {
        return crate::fetch::value(req, stmt, column, None).map(Some);
    }
    Ok(None)
}

/// `row_read_column_name`: the value of the first column so named.
fn by_name(req: &mut Request, stmt: PhpObject, name: &[u8]) -> Result<Option<Value>, Thrown> {
    let at = stmt::read(req, stmt, |s| {
        s.columns
            .as_ref()
            .and_then(|columns| columns.iter().position(|c| c.name == name))
    })
    .flatten();
    match at {
        Some(at) => crate::fetch::value(req, stmt, i64::try_from(at).unwrap_or(-1), None).map(Some),
        None => Ok(None),
    }
}

/// A name read as a column number where it is an integer string, as
/// `is_numeric_str_function` reads it, and as a column name otherwise.
fn by_key(req: &mut Request, stmt: PhpObject, name: &[u8]) -> Result<Option<Value>, Thrown> {
    match numeric_string(name).and_then(|v| v.as_int()) {
        Some(n) => by_number(req, stmt, n),
        None => by_name(req, stmt, name),
    }
}

impl ObjectHandlers for Handlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(held) = object.native_mut::<Row>(req).and_then(|r| r.stmt.take()) else {
            return;
        };
        stmt::write(req, held, |s| s.lazy = None);
        req.release(held.as_value());
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn clone_obj_with(&self) -> Option<CloneObjWith> {
        None
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        let _ = (req, left, right);
        Ok(None)
    }

    /// `row_prop_read`: `queryString` off the statement, a column by
    /// number or by name, `null` for anything else.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        let Some(stmt) = statement(req, object) else {
            return Ok(Value::null());
        };
        if name == b"queryString" {
            return Std.read_property(req, stmt, name, read);
        }
        Ok(by_key(req, stmt, name)?.unwrap_or_default())
    }

    /// `row_dim_read`: a name is the property of that name, read
    /// silently under `BP_VAR_IS`.
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Value, Thrown> {
        let Some(offset) = offset else {
            return Err(throw(req, "Error", "Cannot append to PDORow offset"));
        };
        let Some(stmt) = statement(req, object) else {
            return Ok(Value::null());
        };
        if let ValueRef::Int(n) = offset.deref().as_ref() {
            return Ok(by_number(req, stmt, n)?.unwrap_or_default());
        }
        let name = to_string(req, &offset.deref())?;
        let how = if read == DimensionRead::Silent {
            PropertyRead::Silent
        } else {
            PropertyRead::Warning
        };
        self.read_property(req, object, name.as_bytes(), how)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let _ = object;
        req.release(value);
        req.release(Value::string(name));
        Err(throw(req, "Error", "Cannot write to PDORow property"))
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        let _ = object;
        req.release(value);
        let message = if offset.is_some() {
            "Cannot write to PDORow offset"
        } else {
            "Cannot append to PDORow offset"
        };
        Err(throw(req, "Error", message))
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
            PropertyTest::Isset => !crate::is_null(&value),
            PropertyTest::NotEmpty | PropertyTest::Exists => truthy(req, &value),
        };
        req.release(value);
        Ok(answer)
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
            req.release(value);
            return Ok(answer);
        }
        let name = to_string(req, &offset.deref())?;
        let test = if not_empty {
            PropertyTest::NotEmpty
        } else {
            PropertyTest::Isset
        };
        self.has_property(req, object, name.as_bytes(), test)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        let _ = (object, name);
        Err(throw(req, "Error", "Cannot unset PDORow property"))
    }

    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        let _ = (object, offset);
        Err(throw(req, "Error", "Cannot unset PDORow offset"))
    }

    /// What `var_dump` shows, the table `row_get_properties_for` builds
    /// for `ZEND_PROP_PURPOSE_DEBUG`, one owner the caller's.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        let Some(rows) = self.get_properties_for(req, object, PropertyPurpose::Debug)? else {
            return Ok(None);
        };
        let mut out = PhpArray::new(req)?;
        for (key, value) in rows {
            req.addref(&value);
            out.set(req, key, value)?;
        }
        Ok(Some(out.into()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    /// `row_get_properties_for` under `ZEND_PROP_PURPOSE_DEBUG`: the
    /// statement's properties, then each column but one named
    /// `queryString`.
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(php_ext::values::Key, Value)>>, Thrown> {
        if purpose != PropertyPurpose::Debug {
            return Ok(None);
        }
        let Some(stmt) = statement(req, object) else {
            return Ok(None);
        };
        // Views, as `get_properties_for` answers them; a column's value is
        // the one this read made, which the request's end frees.
        let mut rows = Std.get_properties(req, stmt)?;
        let names: Vec<Vec<u8>> = stmt::read(req, stmt, |s| {
            s.columns
                .as_ref()
                .map(|c| c.iter().map(|c| c.name.clone()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
        for (at, name) in names.iter().enumerate() {
            if name == b"queryString" {
                continue;
            }
            let value = crate::fetch::value(req, stmt, i64::try_from(at).unwrap_or(-1), None)?;
            let key = PhpString::new(req, name)?;
            match rows
                .iter_mut()
                .find(|(k, _)| k.as_bytes() == name.as_slice())
            {
                Some(slot) => slot.1 = value,
                None => rows.push((key, value)),
            }
        }
        Ok(Some(
            rows.into_iter()
                .map(|(name, value)| (php_ext::values::Key::Str(name), value))
                .collect(),
        ))
    }

    /// `row_get_ctor`.
    fn get_constructor(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        let _ = object;
        Err(throw(
            req,
            "PDOException",
            "You may not create a PDORow manually",
        ))
    }
}
