//! `class mysqli_result`: a set of rows, `MYSQL_RES` behind its
//! `MYSQLI_RESOURCE`, with the fetch functions of `mysqli_api.c` and
//! `mysqli_nonapi.c`, `php_mysqli_fetch_into_hash`, the iterator of
//! `mysqli_result_iterator.c` and the properties of `mysqli_result`.

#![allow(non_camel_case_types)]

use std::ops::ControlFlow;

use php_ext::abi::{
    ClassIterator, ClassRef, Instance, IntoReturn, ObjectHandlers, OrFalse, Raw, Refusals, This,
    True, Union,
};
use php_ext::attr::php_class;
use php_ext::call::{
    autoload, call_array, internal_iterator, known_method, lookup_class, new_object,
};
use php_ext::coerce::with_string;
use php_ext::decl::{Name, Type};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, fatal, raise, throw_error, while_pending,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, PropertyScope, Value};
use php_ext_mysqlnd::client::{Field, ResultSet};

use crate::link::{self, Need, mysqli};
use crate::props::{self, Props};
use crate::{On, Resource, class_name, closed, long_or_string, report, report_error, report_mode};

/// `MYSQLI_ASSOC`, `MYSQLI_NUM` and `MYSQLI_BOTH`.
pub(crate) const ASSOC: i64 = 1;
pub(crate) const NUM: i64 = 2;
pub(crate) const BOTH: i64 = 3;

/// The rows: held whole (`MYSQLI_STORE_RESULT`), read one at a time
/// off the line (`MYSQLI_USE_RESULT`), or freed, php's null `ptr`; held
/// rows are behind the `MYSQLI_RESOURCE` that points at them.
#[derive(Default)]
pub(crate) enum Rows {
    #[default]
    Freed,
    Stored(Resource<ResultSet>),
    Used(Resource<ResultSet>),
}

impl Rows {
    /// `ecalloc(1, sizeof(MYSQLI_RESOURCE))` pointed at `set`; the `Err`
    /// is the memory limit's fatal.
    fn of(req: &mut Request, set: ResultSet) -> Result<Rows, Thrown> {
        let buffered = set.is_buffered();
        let resource = Resource::new(req, set)?;
        Ok(if buffered {
            Rows::Stored(resource)
        } else {
            Rows::Used(resource)
        })
    }

    fn set(&mut self) -> Option<&mut ResultSet> {
        match self {
            Rows::Stored(r) | Rows::Used(r) => Some(r),
            Rows::Freed => None,
        }
    }

    /// `mysqli_free_result(result, FALSE)` and the resource's `efree`:
    /// `None` where the rows were freed already.
    fn free(self, req: &mut Request) -> Option<Result<(), Thrown>> {
        let (Rows::Stored(r) | Rows::Used(r)) = self else {
            return None;
        };
        Some(r.free(req).free(req, false))
    }
}

/// `mysqli_result_class_entry`.
fn result_class(req: &mut Request) -> Result<ClassId, Thrown> {
    lookup_class(req, b"mysqli_result")?
        .ok_or_else(|| fatal(req, "InternalError: mysqli_result is not linked"))
}

/// `MYSQLI_RETVAL_RESOURCE(result, mysqli_result_class_entry)`: the rows
/// on a new `mysqli_result`, behind their `MYSQLI_RESOURCE`. What
/// `memory_get_usage()` sees of stored rows is their pool, their row
/// buffers and their resource, which `mysqli_free_result` gives back with
/// the rows, leaving the row fetched:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $m = memory_get_usage();
/// $r = mysqli_query($l, "SELECT REPEAT('x', 1000000) AS a, REPEAT('y', 1000000) AS b");
/// echo memory_get_usage() - $m, "\n";
/// $row = mysqli_fetch_row($r);
/// echo memory_get_usage() - $m, "\n";
/// mysqli_free_result($r);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql
/// 4022472
/// 6029760
/// 2007448
/// ```
pub(crate) fn wrap(
    req: &mut Request,
    set: ResultSet,
) -> Result<Owner<Instance<mysqli_result>>, Thrown> {
    let class = result_class(req)?;
    let rows = Rows::of(req, set)?;
    let object = match new_object(req, class) {
        Ok(object) => object,
        Err(thrown) => {
            rows.free(req);
            return Err(thrown);
        }
    };
    if let Some(slot) = crate::payload::<Rows>(req, *object) {
        *slot = rows;
    }
    Ok(object.into_instance())
}

/// `MYSQLI_FETCH_RESOURCE(result, ..., MYSQLI_STATUS_VALID)` around one use
/// of the rows: lent to `f` and put back, or php's `Error` for freed rows.
pub(crate) fn with_rows<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, &mut ResultSet) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let mut rows = crate::payload::<Rows>(req, object)
        .map(std::mem::take)
        .unwrap_or_default();
    let Some(set) = rows.set() else {
        return Err(closed(req, object));
    };
    let answered = f(req, set);
    if let Some(slot) = crate::payload::<Rows>(req, object)
        && matches!(slot, Rows::Freed)
    {
        *slot = rows;
    }
    answered
}

/// `php_mysqli_fetch_into_hash_aux`: the next row keyed as `mode` asks,
/// `null` past the last and `false` on a failure, and the connection's
/// error reported under `MYSQLI_REPORT_ERROR`.
pub(crate) fn fetch_row(
    req: &mut Request,
    set: &mut ResultSet,
    mode: i64,
) -> Result<Owner<Value>, Thrown> {
    let flags = (u32::from(mode & NUM != 0) << 1) | u32::from(mode & ASSOC != 0);
    let row = set.fetch_into(req, flags)?;
    // `php_mysqli_fetch_into_hash_aux`: `get_error_no(conn)`, and the
    // status read whole only for the error it reports.
    if report_mode(req) & report::ERROR != 0 && set.connection().errno(req) != 0 {
        let status = set.connection().status(req);
        if let Err(thrown) = report_error(req, status.errno, status.sqlstate, &status.error) {
            row.give_up(req);
            return Err(thrown);
        }
    }
    Ok(row)
}

/// `mysqli_fetch_array`'s `$mode`, held to `MYSQLI_ASSOC..=MYSQLI_BOTH`.
fn checked_mode(req: &mut Request, on: On, mode: i64) -> Result<i64, Thrown> {
    if !(ASSOC..=BOTH).contains(&mode) {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be one of MYSQLI_NUM, MYSQLI_ASSOC, or MYSQLI_BOTH",
        ));
    }
    Ok(mode)
}

/// A declared return answered with the value as it is, as php's natives
/// return what they built without a check.
pub(crate) struct AsIs<T>(pub(crate) Owner<Value>, std::marker::PhantomData<fn() -> T>);

impl<T> AsIs<T> {
    pub(crate) fn new(value: Owner<Value>) -> AsIs<T> {
        AsIs(value, std::marker::PhantomData)
    }
}

impl<T: IntoReturn> IntoReturn for AsIs<T> {
    const MASK: u32 = T::MASK;
    fn declared() -> Option<Type> {
        T::declared()
    }
    fn into_return(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        self.0.into_return(req)
    }
}

/// `array|null|false`.
pub(crate) type Fetched = AsIs<Option<OrFalse<PhpArray>>>;
/// `object|null|false`.
pub(crate) type FetchedObject = AsIs<Option<OrFalse<PhpObject>>>;
/// `null|int|float|string|false`.
pub(crate) type Column = AsIs<Option<OrFalse<Union<i64, Union<f64, PhpString>>>>>;

/// `mysqli_fetch_array`, `mysqli_fetch_assoc` and `mysqli_fetch_row`:
/// `MYSQLI_FETCH_RESOURCE`, then `$mode` checked, then the row, in the
/// one lend of the rows:
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, "SELECT 1");
/// mysqli_free_result($r);
/// try { mysqli_fetch_array($r, 99); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// $r = mysqli_query($l, "SELECT 1");
/// try { mysqli_fetch_array($r, 99); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// echo json_encode(mysqli_fetch_array($r, MYSQLI_NUM)), "\n";
/// ```
/// ```expect mysql
/// Error: mysqli_result object is already closed
/// ValueError: mysqli_fetch_array(): Argument #2 ($mode) must be one of MYSQLI_NUM, MYSQLI_ASSOC, or MYSQLI_BOTH
/// ["1"]
/// ```
pub(crate) fn fetch_array(req: &mut Request, on: On, mode: i64) -> Result<Fetched, Thrown> {
    with_rows(req, on.object, |req, set| {
        let mode = checked_mode(req, on, mode)?;
        fetch_row(req, set, mode)
    })
    .map(AsIs::new)
}

/// `mysqli_fetch_object`: the row as properties of a new `class`, written
/// as `zend_merge_properties` writes them, then its constructor served
/// with the arguments given.
pub(crate) fn fetch_object(
    req: &mut Request,
    on: On,
    class: Raw<PhpString>,
    args: Raw<PhpArray>,
    given: bool,
) -> Result<FetchedObject, Thrown> {
    // `"O|Ch"`: the class by `try_convert_to_string`, whose refusal is the
    // conversion's `Error` and not a `TypeError` of the parameter, before
    // the arguments are held.
    let class = with_string(req, &class.value(), <[u8]>::to_vec)?;
    let args = args.hold(req)?;
    let args = given.then_some(args);
    let Some(target) = autoload(req, &class)? else {
        let given = String::from_utf8_lossy(&class).into_owned();
        return Err(arg_type_error(
            req,
            on.at(2),
            &format!("be a valid class name, {given} given"),
        ));
    };
    let refused = req
        .symbols()
        .class_decl(target)
        .is_some_and(|d| d.kind != php_ext::decl::ClassKind::Class || d.flags.is_abstract);
    if refused {
        let name = class_name(req, target);
        return Err(throw_error(
            req,
            "Error",
            &format!("Class {name} cannot be instantiated"),
        ));
    }
    let row = with_rows(req, on.object, |req, set| fetch_row(req, set, ASSOC))?;
    let Some(array) = row.as_array() else {
        return Ok(AsIs::new(row));
    };
    let made = new_object(req, target)?;
    if let Err(thrown) = set_row(req, *made, array) {
        row.give_up(req);
        made.give_up(req);
        return Err(thrown);
    }
    row.give_up(req);
    let constructor = Name(b"__construct".as_slice().into());
    match known_method(req, made.class(), &constructor, Some(*made)) {
        Some(callable) => {
            let args = args.unwrap_or_else(PhpArray::empty);
            match call_array(req, &callable, args) {
                Ok(answered) => answered.give_up(req),
                Err(thrown) => {
                    made.give_up(req);
                    return Err(thrown);
                }
            }
        }
        None if args.is_some_and(|a| !a.is_empty()) => {
            let name = class_name(req, target);
            made.give_up(req);
            return Err(arg_value_error(
                req,
                on.at(3),
                &format!("be empty when the specified class ({name}) does not have a constructor"),
            ));
        }
        None => {}
    }
    Ok(AsIs::new(made.into_value()))
}

/// The row as `made`'s properties: its table in place of the object's
/// where the class declares no property and no `__set`
/// (`zend_symtable_to_proptable`, [`PhpObject::table_from_symtable`]),
/// so a kept object costs what the row's table costs and nothing beside
/// it, an integer column name spelt in decimal in a copy:
///
/// ```php
/// class R {}
/// $m = new mysqli('127.0.0.1', 'root', '', '', 3306);
/// $r = $m->query("SELECT 'ab' AS col1, 'cd' AS col2, 3 AS col3 UNION ALL SELECT 'ef', 'gh', 4");
/// $x = memory_get_usage(); $a = $r->fetch_object('R'); echo memory_get_usage() - $x, "\n";
/// $x = memory_get_usage(); $b = $r->fetch_object(); echo memory_get_usage() - $x, "\n";
/// $c = $m->query("SELECT 'ab' AS `7`, 'cd' AS col2")->fetch_object('R');
/// var_dump($c);
/// ```
/// ```expect mysql
/// 480
/// 480
/// object(R)#6 (2) {
///   ["7"]=>
///   string(2) "ab"
///   ["col2"]=>
///   string(2) "cd"
/// }
/// ```
///
/// Otherwise each column is written through
/// the class's handler in the object's class's scope, the
/// `EG(fake_scope)` `zend_merge_properties` sets: a parent's private
/// property is out of its sight, and the column is a dynamic one. A
/// write that throws does not end the row: `write_property` returns
/// nothing, the later columns are written with the throwable pending,
/// each later one chained onto it, and it leaves after the loop, where
/// the constructor is not served (`Zend/zend_API.c:1405-1424`); a fatal
/// or an `exit` leaves at once.
///
/// ```php
/// class D { public int $a; public $b; function __destruct() { var_dump($this->b); } }
/// $m = new mysqli('127.0.0.1', 'root', '', '', 3306);
/// try { $m->query("SELECT 'x' AS a, 'y' AS b")->fetch_object('D'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// string(1) "y"
/// Cannot assign string to property D::$a of type int
/// ```
///
/// A later typed column that fails while that throwable is pending is
/// left unwritten and raises nothing, as `zend_verify_property_type_error`
/// returns under `EG(exception)`:
///
/// ```php
/// class E { public int $a; public int $b; }
/// $m = new mysqli('127.0.0.1', 'root', '', '', 3306);
/// try { $m->query("SELECT 'x' AS a, 'y' AS b")->fetch_object('E'); } catch (TypeError $e) { echo $e->getMessage(), "|", $e->getPrevious()?->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// Cannot assign string to property E::$a of type int|
/// ```
///
/// ```php
/// class B { private $v = 'b'; }
/// #[AllowDynamicProperties] class D extends B {}
/// $m = new mysqli('127.0.0.1', 'root', '', '', 3306);
/// var_dump($m->query("SELECT 'x' AS v")->fetch_object('D'));
/// ```
/// ```expect mysql
/// object(D)#3 (2) {
///   ["v":"B":private]=>
///   string(1) "b"
///   ["v"]=>
///   string(1) "x"
/// }
/// ```
fn set_row(req: &mut Request, made: PhpObject, row: PhpArray) -> Result<(), Thrown> {
    if declares_nothing(req, made.class()) {
        let row = Owner::take(req, row);
        return made.table_from_symtable(req, row);
    }
    let mut pending: Option<Thrown> = None;
    // `zend_merge_properties`' `ZEND_HASH_MAP_FOREACH_STR_KEY_VAL(properties, key,
    // value)` (`Z/Zend/zend_API.c:1418`) over `zend_symtable_to_proptable`'s
    // table, no owner of the row taken: this native holds it while a
    // property write serves `__set`. A column's name is the row's own
    // string, an integer one spelt in decimal as the conversion spells it.
    let walked = walk::foreach(req, row, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        let name = match key {
            Key::Str(name) => Owner::take(req, name),
            Key::Int(n) => PhpString::new(req, n.to_string().as_bytes())?,
        };
        let value = Owner::take(req, value);
        let scope = PropertyScope::Of(ClassRef(made.class()));
        let wrote = match &pending {
            Some(standing) => while_pending(req, standing, |req| {
                made.write_prop_in(req, scope, name, value)
            })
            .unwrap_or(Ok(())),
            None => made.write_prop_in(req, scope, name, value),
        };
        let Err(thrown) = wrote else {
            return Ok(ControlFlow::Continue(()));
        };
        if thrown.exception().is_none() {
            return Err(thrown);
        }
        pending = Some(match pending.take() {
            // The write refused past the standing throwable answers it,
            // with no owner of its own ([`while_pending`]).
            Some(earlier) if earlier.exception() == thrown.exception() => earlier,
            Some(earlier) => earlier.chain(req, thrown),
            None => thrown,
        });
        Ok(ControlFlow::Continue(()))
    });
    walked?;
    pending.map_or(Ok(()), Err)
}

/// `!ce->default_properties_count && !ce->__set`.
fn declares_nothing(req: &mut Request, class: ClassId) -> bool {
    let setter = Name(b"__set".as_slice().into());
    if known_method(req, class, &setter, None).is_some() {
        return false;
    }
    !req.symbols()
        .property_info_rows(class)
        .filter_map(|row| req.symbols().row_property_info(row))
        .any(|p| p.is_instance && !p.is_virtual)
}

/// `mysqli_fetch_all`.
pub(crate) fn fetch_all(req: &mut Request, on: On, mode: i64) -> Result<Owner<PhpArray>, Thrown> {
    with_rows(req, on.object, |_, _| Ok(()))?;
    if mode == 0 || mode & !BOTH != 0 {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be one of MYSQLI_NUM, MYSQLI_ASSOC, or MYSQLI_BOTH",
        ));
    }
    with_rows(req, on.object, |req, set| {
        let mut all = PhpArray::new(req)?;
        loop {
            let row = match fetch_row(req, set, mode) {
                Ok(row) => row,
                Err(thrown) => {
                    all.give_up(req);
                    return Err(thrown);
                }
            };
            if row.as_array().is_none() {
                row.give_up(req);
                break;
            }
            all.append(req, row)?;
        }
        Ok(all)
    })
}

/// `mysqli_fetch_column`.
pub(crate) fn fetch_column(req: &mut Request, on: On, column: i64) -> Result<Column, Thrown> {
    let fields = with_rows(req, on.object, |_, set| Ok(set.num_fields()))?;
    if column < 0 {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be greater than or equal to 0",
        ));
    }
    if column >= i64::from(fields) {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be less than the number of fields for this result set",
        ));
    }
    let row = with_rows(req, on.object, |req, set| fetch_row(req, set, NUM))?;
    let Some(array) = row.as_array() else {
        row.give_up(req);
        return Ok(AsIs::new(Owner::bool(false)));
    };
    let value = array.get(&Key::Int(column)).unwrap_or_default();
    let value = Owner::take(req, value);
    row.give_up(req);
    Ok(AsIs::new(value))
}

/// `php_add_field_properties`: one column's description as an object,
/// its name the column's interned string and each other string made for
/// its property, the empty ones too:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, "SELECT 1 AS a");
/// $m = memory_get_usage();
/// $f = mysqli_fetch_field($r);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql
/// 1376
/// ```
fn field_object(req: &mut Request, field: &Field) -> Result<Owner<PhpObject>, Thrown> {
    let object = crate::std_object(req)?;
    let facts = [
        ("name", crate::Fact::Str(*field.name)),
        ("orgname", crate::Fact::Text(&field.org_name)),
        ("table", crate::Fact::Text(&field.table)),
        ("orgtable", crate::Fact::Text(&field.org_table)),
        (
            "def",
            crate::Fact::Text(field.def.as_deref().unwrap_or_default()),
        ),
        ("db", crate::Fact::Text(&field.db)),
        ("catalog", crate::Fact::Text(b"def")),
        ("max_length", crate::Fact::Int(0)),
        (
            "length",
            crate::Fact::Int(i64::try_from(field.length).unwrap_or(i64::MAX)),
        ),
        ("charsetnr", crate::Fact::Int(i64::from(field.charsetnr))),
        ("flags", crate::Fact::Int(i64::from(field.flags))),
        ("type", crate::Fact::Int(i64::from(field.type_byte))),
        ("decimals", crate::Fact::Int(i64::from(field.decimals))),
    ];
    if let Err(thrown) = crate::add_facts(req, *object, &facts) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(object)
}

/// `mysqli_fetch_field`.
pub(crate) fn fetch_field(req: &mut Request, on: On) -> Result<OrFalse<Owner<PhpObject>>, Thrown> {
    with_rows(req, on.object, |req, set| match set.fetch_field() {
        Some(field) => field_object(req, field).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    })
}

/// `mysqli_fetch_fields`.
pub(crate) fn fetch_fields(req: &mut Request, on: On) -> Result<Owner<PhpArray>, Thrown> {
    with_rows(req, on.object, |req, set| {
        let mut out = PhpArray::new(req)?;
        for field in set.fields() {
            let object = field_object(req, field)?;
            out.append(req, object.into_value())?;
        }
        Ok(out)
    })
}

/// `mysqli_fetch_field_direct`.
pub(crate) fn fetch_field_direct(
    req: &mut Request,
    on: On,
    index: i64,
) -> Result<OrFalse<Owner<PhpObject>>, Thrown> {
    if index < 0 {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be greater than or equal to 0",
        ));
    }
    with_rows(req, on.object, |req, set| {
        let Some(field) = usize::try_from(index)
            .ok()
            .and_then(|i| set.fields().get(i))
        else {
            return Err(arg_value_error(
                req,
                on.at(2),
                "be less than the number of fields for this result set",
            ));
        };
        field_object(req, field).map(OrFalse::Value)
    })
}

/// `mysqli_fetch_lengths`.
pub(crate) fn fetch_lengths(req: &mut Request, on: On) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let lengths = with_rows(req, on.object, |_, set| Ok(lengths_of(set)))?;
    let Some(lengths) = lengths else {
        return Ok(OrFalse::False);
    };
    lengths_array(req, &lengths).map(OrFalse::Value)
}

fn lengths_of(set: &ResultSet) -> Option<Vec<u64>> {
    let count = set.num_fields() as usize;
    set.lengths().map(|l| {
        l.iter()
            .copied()
            .chain(std::iter::repeat(0))
            .take(count)
            .collect()
    })
}

fn lengths_array(req: &mut Request, lengths: &[u64]) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for n in lengths {
        out.append(req, Owner::int(i64::try_from(*n).unwrap_or(i64::MAX)))?;
    }
    Ok(out)
}

/// `mysqli_data_seek`.
pub(crate) fn data_seek(req: &mut Request, on: On, offset: i64) -> Result<bool, Thrown> {
    if offset < 0 {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be greater than or equal to 0",
        ));
    }
    let used = with_rows(req, on.object, |_, set| Ok(!set.is_buffered()))?;
    if used {
        let name = if on.method {
            "mysqli_result::data_seek()"
        } else {
            "mysqli_data_seek()"
        };
        return Err(throw_error(
            req,
            "Error",
            &format!("{name} cannot be used in MYSQLI_USE_RESULT mode"),
        ));
    }
    with_rows(req, on.object, |_, set| {
        let row = u64::try_from(offset).unwrap_or(u64::MAX);
        if row >= set.num_rows() {
            return Ok(false);
        }
        Ok(set.data_seek(row))
    })
}

/// `mysqli_field_seek`.
pub(crate) fn field_seek(req: &mut Request, on: On, index: i64) -> Result<True, Thrown> {
    if index < 0 {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be greater than or equal to 0",
        ));
    }
    let count = with_rows(req, on.object, |_, set| Ok(set.num_fields()))?;
    if index >= i64::from(count) {
        return Err(arg_value_error(
            req,
            on.at(2),
            "be less than the number of fields for this result set",
        ));
    }
    with_rows(req, on.object, |_, set| {
        set.field_seek(usize::try_from(index).unwrap_or(0));
        Ok(True)
    })
}

/// `mysqli_num_rows`.
pub(crate) fn num_rows(req: &mut Request, on: On) -> Result<AsIs<Union<i64, PhpString>>, Thrown> {
    let (unfinished, rows) = with_rows(req, on.object, |_, set| {
        Ok((!set.is_buffered() && !set.eof(), set.num_rows()))
    })?;
    if unfinished {
        return Err(throw_error(
            req,
            "Error",
            "mysqli_num_rows() cannot be used in MYSQLI_USE_RESULT mode",
        ));
    }
    long_or_string(req, rows).map(AsIs::new)
}

/// `mysqli_free_result`: the rows freed, the object closed.
pub(crate) fn free(req: &mut Request, on: On) -> Result<(), Thrown> {
    let rows = crate::payload::<Rows>(req, on.object)
        .map(std::mem::take)
        .unwrap_or_default();
    match rows.free(req) {
        Some(freed) => freed,
        None => Err(closed(req, on.object)),
    }
}

/// One of the counts of the rows.
pub(crate) fn count(req: &mut Request, on: On, of: fn(&ResultSet) -> u32) -> Result<i64, Thrown> {
    with_rows(req, on.object, |_, set| Ok(i64::from(of(set))))
}

// ---- the class

/// `interface IteratorAggregate` and `interface Iterator`, Core's.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

#[php_class(extern)]
pub(crate) struct Iterator;

/// `class mysqli_result implements IteratorAggregate`, its properties read
/// through `mysqli_result_property_entries` ([`ResultProps`]).
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = new mysqli('127.0.0.1', 'root', '', 'test');
/// $l->query('CREATE TEMPORARY TABLE r (id INT PRIMARY KEY, s VARCHAR(5))');
/// $l->query("INSERT INTO r VALUES (1, 'a'), (2, 'bb'), (3, NULL)");
/// $r = new mysqli_result($l->real_query('SELECT * FROM r ORDER BY id') ? $l : $l);
/// var_dump($r->num_rows, $r->field_count, $r->type, $r->fetch_row(), $r->lengths, $r->fetch_assoc(), $r->fetch_array(MYSQLI_NUM));
/// var_dump($r->data_seek(0), $r->fetch_object()->s, $r->fetch_column(1), $r->fetch_all(MYSQLI_ASSOC));
/// var_dump($r->field_seek(1), $r->current_field, $r->fetch_field()->name, count($r->fetch_fields()), $r->fetch_field_direct(0)->orgname);
/// foreach ($r as $k => $row) echo $k, ':', $row['id'], "\n";
/// echo get_class($r->getIterator()), "\n";
/// $r->free(); $t = $l->query('SELECT 1'); $t->close();
/// $s = $l->query('SELECT 1', MYSQLI_USE_RESULT); var_dump($s->type); $s->free_result();
/// try { $r->fetch_row(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($r);
/// ```
/// ```expect mysql
/// int(3)
/// int(2)
/// int(0)
/// array(2) {
///   [0]=>
///   string(1) "1"
///   [1]=>
///   string(1) "a"
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(1)
/// }
/// array(2) {
///   ["id"]=>
///   string(1) "2"
///   ["s"]=>
///   string(2) "bb"
/// }
/// array(2) {
///   [0]=>
///   string(1) "3"
///   [1]=>
///   NULL
/// }
/// bool(true)
/// string(1) "a"
/// string(2) "bb"
/// array(1) {
///   [0]=>
///   array(2) {
///     ["id"]=>
///     string(1) "3"
///     ["s"]=>
///     NULL
///   }
/// }
/// bool(true)
/// int(1)
/// string(1) "s"
/// int(2)
/// string(2) "id"
/// 0:1
/// 1:2
/// 2:3
/// InternalIterator
/// int(1)
/// mysqli_result object is already closed
/// object(mysqli_result)#2 (0) {
/// }
/// ```
#[php_class(implements(IteratorAggregate), size = 72, payload = Box<Rows>, handlers = RESULT)]
pub(crate) struct mysqli_result;

pub(crate) static RESULT: ResultHandlers = ResultHandlers(props::Handlers::new());

type Answer<T> = Result<T, Thrown>;

#[php_class]
impl mysqli_result {
    #[php(property)]
    const current_field: i64;
    #[php(property)]
    const field_count: i64;
    #[php(property)]
    const lengths: Option<PhpArray>;
    #[php(property)]
    const num_rows: Union<i64, PhpString>;
    #[php(property)]
    const r#type: i64;

    fn __construct(
        req: &mut Request,
        this: This<Self>,
        mysql: Instance<mysqli>,
        #[php(default = MYSQLI_STORE_RESULT)] result_mode: i64,
    ) -> Answer<()> {
        let conn = link::conn(req, mysql.object(), Need::Open)?;
        let set = match result_mode {
            link::STORE => conn.store(req)?,
            link::USE => conn.use_unbuffered(req)?,
            _ => {
                return Err(arg_value_error(
                    req,
                    2,
                    "be either MYSQLI_STORE_RESULT or MYSQLI_USE_RESULT",
                ));
            }
        };
        let Some(set) = set else {
            crate::report_conn(req, conn)?;
            return Ok(());
        };
        let status = conn.status(req).server_status;
        let index = crate::report_index(req, b"from previous query", status);
        let rows = Rows::of(req, set)?;
        if let Some(slot) = crate::payload::<Rows>(req, this.object()) {
            *slot = rows;
        }
        index
    }

    #[php(tentative)]
    fn close(req: &mut Request, this: This<Self>) -> Answer<()> {
        free(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn free(req: &mut Request, this: This<Self>) -> Answer<()> {
        free(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn data_seek(req: &mut Request, this: This<Self>, offset: i64) -> Answer<bool> {
        data_seek(req, On::method(this.object()), offset)
    }

    #[php(tentative)]
    fn fetch_field(req: &mut Request, this: This<Self>) -> Answer<OrFalse<Owner<PhpObject>>> {
        fetch_field(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn fetch_fields(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpArray>> {
        fetch_fields(req, On::method(this.object()))
    }

    #[php(tentative)]
    fn fetch_field_direct(
        req: &mut Request,
        this: This<Self>,
        index: i64,
    ) -> Answer<OrFalse<Owner<PhpObject>>> {
        fetch_field_direct(req, On::method(this.object()), index)
    }

    #[php(tentative)]
    fn fetch_all(
        req: &mut Request,
        this: This<Self>,
        #[php(default = MYSQLI_NUM)] mode: i64,
    ) -> Answer<Owner<PhpArray>> {
        fetch_all(req, On::method(this.object()), mode)
    }

    #[php(tentative)]
    fn fetch_array(
        req: &mut Request,
        this: This<Self>,
        #[php(default = MYSQLI_BOTH)] mode: i64,
    ) -> Answer<Fetched> {
        fetch_array(req, On::method(this.object()), mode)
    }

    #[php(tentative)]
    fn fetch_assoc(req: &mut Request, this: This<Self>) -> Answer<Fetched> {
        fetch_array(req, On::method(this.object()), ASSOC)
    }

    #[php(tentative)]
    fn fetch_object(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "stdClass")] class: Raw<PhpString>,
        #[php(default = [])] constructor_args: Raw<PhpArray>,
    ) -> Answer<FetchedObject> {
        let given = req.frames().argument_count() > 1;
        fetch_object(
            req,
            On::method(this.object()),
            class,
            constructor_args,
            given,
        )
    }

    #[php(tentative)]
    fn fetch_row(req: &mut Request, this: This<Self>) -> Answer<Fetched> {
        fetch_array(req, On::method(this.object()), NUM)
    }

    fn fetch_column(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] column: i64,
    ) -> Answer<Column> {
        fetch_column(req, On::method(this.object()), column)
    }

    #[php(tentative)]
    fn field_seek(req: &mut Request, this: This<Self>, index: i64) -> Answer<True> {
        field_seek(req, On::method(this.object()), index)
    }

    #[php(tentative)]
    fn free_result(req: &mut Request, this: This<Self>) -> Answer<()> {
        free(req, On::method(this.object()))
    }

    fn getIterator(req: &mut Request, this: This<Self>) -> Answer<Owner<Instance<Iterator>>> {
        let object = this.object();
        internal_iterator(req, &object).map(Owner::into_instance)
    }
}

/// `mysqli_object_result_handlers` with `php_mysqli_result_get_iterator`.
pub(crate) struct ResultHandlers(props::Handlers<ResultProps>);

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for ResultHandlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        self.0.free_obj(req, object);
    }
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: php_ext::abi::PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        self.0.read_property(req, object, name, read)
    }
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        self.0.write_property(req, object, name, value)
    }
    fn write_overloaded(&self) -> bool {
        true
    }
    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<php_ext::abi::PropertyPlace, Thrown> {
        self.0.get_property_ptr_ptr(req, object, name)
    }
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: php_ext::abi::PropertyTest,
    ) -> Result<bool, Thrown> {
        self.0.has_property(req, object, name, test)
    }
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        self.0.get_debug_info(req, object)
    }

    /// `php_mysqli_result_get_iterator`.
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            return Err(throw_error(
                req,
                "Error",
                "An iterator cannot be used with foreach by reference",
            ));
        }
        let _ = object;
        Ok(Some(Box::new(RowIter {
            current: None,
            row_num: -1,
        })))
    }

    fn iterator_size(&self) -> usize {
        112
    }
}

/// `php_mysqli_result_iterator`: the row fetched last and its number.
struct RowIter {
    current: Option<Owner<Value>>,
    row_num: i64,
}

impl RowIter {
    /// `php_mysqli_result_iterator_move_forward`.
    fn forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        if let Some(old) = self.current.take() {
            old.give_up(req);
        }
        let row = with_rows(req, object, |req, set| fetch_row(req, set, ASSOC))?;
        if row.as_array().is_some() {
            self.row_num += 1;
        }
        self.current = Some(row);
        Ok(())
    }
}

impl ClassIterator for RowIter {
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let used_up = with_rows(req, object, |_, set| {
            if set.is_buffered() {
                set.data_seek(0);
                Ok(false)
            } else {
                Ok(set.eof())
            }
        })?;
        if used_up {
            raise(
                req,
                Level::Warning,
                "Data fetched with MYSQLI_USE_RESULT can be iterated only once",
            );
            return Ok(());
        }
        self.row_num = -1;
        self.forward(req, object)
    }

    fn valid(&mut self, _: &mut Request, _: PhpObject) -> Result<bool, Thrown> {
        Ok(self
            .current
            .as_deref()
            .is_some_and(|row| row.as_array().is_some()))
    }

    fn current(&mut self, req: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        let row = self.current.as_deref().copied().unwrap_or_default();
        Ok(Owner::take(req, row))
    }

    fn key(&mut self, _: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::int(self.row_num))
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.forward(req, object)
    }

    fn release(&mut self, req: &mut Request, _: PhpObject) {
        if let Some(row) = self.current.take() {
            row.give_up(req);
        }
    }
}

/// `mysqli_result_property_entries`.
pub(crate) struct ResultProps;

impl Props for ResultProps {
    const NAMES: &'static [&'static str] = &[
        "current_field",
        "field_count",
        "lengths",
        "num_rows",
        "type",
    ];

    fn read(
        req: &mut Request,
        object: PhpObject,
        name: &str,
        quiet: bool,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let mut rows = crate::payload::<Rows>(req, object)
            .map(std::mem::take)
            .unwrap_or_default();
        let Some(set) = rows.set() else {
            // `type` and `lengths` read `CHECK_STATUS` alone.
            if matches!(name, "type" | "lengths") {
                return props::not_yet(req, quiet);
            }
            return props::gone(req, object, quiet);
        };
        let value = match name {
            "current_field" => Owner::int(i64::try_from(set.field_tell()).unwrap_or(0)),
            "field_count" => Owner::int(i64::from(set.num_fields())),
            "lengths" => match lengths_of(set).filter(|_| set.num_fields() > 0) {
                Some(lengths) => lengths_array(req, &lengths)?.into_value(),
                None => Owner::null(),
            },
            "num_rows" => long_or_string(req, set.num_rows())?,
            "type" => {
                let kind = if set.is_buffered() {
                    link::STORE
                } else {
                    link::USE
                };
                Owner::int(kind)
            }
            _ => Owner::null(),
        };
        if let Some(slot) = crate::payload::<Rows>(req, object) {
            *slot = rows;
        }
        Ok(Some(value))
    }

    fn free(req: &mut Request, object: PhpObject) {
        let rows = crate::payload::<Rows>(req, object)
            .map(std::mem::take)
            .unwrap_or_default();
        rows.free(req);
    }
}
