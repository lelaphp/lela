//! The classes of php's `ext/zend_test/object_handlers.c`: each a class
//! with one slot of `std_object_handlers` replaced, for the tests of the
//! engine's paths through that slot. Each object's one value is its
//! first declared property, `OBJ_PROP_NUM(obj, 0)`, which `create_object`
//! writes before the constructor runs.

use std::cmp::Ordering;

use php_ext::abi::{DimensionRead, ExtensionBuilder, ObjectHandlers, Std, This, Union};
use php_ext::attr::php_class;
use php_ext::call::lookup_class;
use php_ext::coerce::{BinaryOp, to_long};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, TypeTag, Value, ValueRef};

/// The classes in `zend_test_object_handlers_init`'s order.
pub(crate) fn declared(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<DoOperationNoCast>()
        .class::<LongCastableNoOperations>()
        .class::<FloatCastableNoOperations>()
        .class::<NumericCastableNoOperations>()
        .class::<VmInterruptComparable>()
        .class::<DimensionHandlersNoArrayAccess>()
}

/// The object's one value, its first declared slot, written as the
/// object is made and by the constructor.
fn set_value(req: &mut Request, object: PhpObject, value: Value) -> Result<(), Thrown> {
    let value = Owner::take(req, value);
    object.set_slot(req, 0, value)
}

/// `final class DoOperationNoCast`: `+` and `*` over its objects and
/// anything `zval_get_long` reads, into a new object, with no cast.
///
/// ```php
/// $a = new DoOperationNoCast(25);
/// var_dump($a + new DoOperationNoCast(6), $a * 2, 3 + $a);
/// try { var_dump($a - 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(DoOperationNoCast)#3 (1) {
///   ["val":"DoOperationNoCast":private]=>
///   int(31)
/// }
/// object(DoOperationNoCast)#2 (1) {
///   ["val":"DoOperationNoCast":private]=>
///   int(50)
/// }
/// object(DoOperationNoCast)#4 (1) {
///   ["val":"DoOperationNoCast":private]=>
///   int(28)
/// }
/// Unsupported operand types: DoOperationNoCast - int
/// ```
#[php_class(final, handlers = crate::object_handlers::DONC)]
pub(crate) struct DoOperationNoCast;

#[php_class]
impl DoOperationNoCast {
    #[php(property, private)]
    const val: i64;

    /// `__construct(int $val)`.
    fn __construct(req: &mut Request, this: This<Self>, val: i64) -> Result<(), Thrown> {
        set_value(req, this.object(), Value::int(val))
    }
}

/// `donc_object_handlers`.
pub(crate) struct Donc;

/// The one table.
pub(crate) static DONC: Donc = Donc;

impl Donc {
    /// `IS_DONC(zv) ? OBJ_PROP_NUM(obj, 0) : zval_get_long(zv)`.
    fn operand(req: &mut Request, value: &Value) -> Result<i64, Thrown> {
        if let ValueRef::Object(object) = value.as_ref()
            && let Some(class) = lookup_class(req, b"DoOperationNoCast")?
            && object.instance_of(req, class)
        {
            return Ok(object.slot(0).and_then(|v| v.as_int()).unwrap_or_default());
        }
        to_long(req, value)
    }
}

impl ObjectHandlers for Donc {
    /// `donc_object_create`: the value zero.
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        set_value(req, object, Value::int(0))
    }

    /// `donc_do_operation`: `ZEND_ADD` and `ZEND_MUL` into a new object
    /// of the class, every other operator refused.
    fn do_operation(
        &self,
        req: &mut Request,
        op: BinaryOp,
        left: &Value,
        right: Option<&Value>,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let combine: fn(i64, i64) -> i64 = match op {
            BinaryOp::Add => i64::wrapping_add,
            BinaryOp::Mul => i64::wrapping_mul,
            _ => return Ok(None),
        };
        let Some(right) = right else {
            return Ok(None);
        };
        let (a, b) = (Donc::operand(req, left)?, Donc::operand(req, right)?);
        let Some(class) = lookup_class(req, b"DoOperationNoCast")? else {
            return Ok(None);
        };
        let made = php_ext::call::new_object(req, class)?;
        set_value(req, *made, Value::int(combine(a, b)))?;
        Ok(Some(made.into_value()))
    }
}

/// `final class LongCastableNoOperations`: cast to an int, and to nothing
/// else.
///
/// ```php
/// $o = new LongCastableNoOperations(5);
/// var_dump((int) $o);
/// try { var_dump($o + 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $o++; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(5)
/// Unsupported operand types: LongCastableNoOperations + int
/// Cannot increment LongCastableNoOperations
/// ```
///
/// Its handler refuses `_IS_BOOL`, so its truth is
/// `zend_object_is_true`'s `E_RECOVERABLE_ERROR`, which `php_error_cb`
/// bails out of: the request ends there, every object standing marked
/// destructed, and a shutdown function still runs.
///
/// ```php
/// class D { function __destruct() { echo "d\n"; } }
/// $d = new D;
/// register_shutdown_function(function () { echo "sd\n"; });
/// if (new LongCastableNoOperations(1)) { echo "t\n"; }
/// echo "not reached\n";
/// ```
/// ```expect exit=255
/// Recoverable fatal error: Object of class LongCastableNoOperations could not be converted to bool in %s on line 5
/// sd
/// ```
///
/// A native that asks the truth leaves with the request: `array_filter`
/// calls its callback once, and the fatal is raised once, as
/// `zend_bailout` longjmps out of the native.
///
/// ```php
/// var_dump(array_filter([1, 2, 3], function ($x) { echo "cb $x\n"; return new LongCastableNoOperations(1); }));
/// echo "not reached\n";
/// ```
/// ```expect exit=255
/// cb 1
///
/// Recoverable fatal error: Object of class LongCastableNoOperations could not be converted to bool in %s on line 2
/// ```
///
/// A user error handler that takes it lets the request go on, the truth
/// `false`.
///
/// ```php
/// set_error_handler(function ($no, $message) { echo "$no: $message\n"; return true; });
/// $o = new LongCastableNoOperations(1);
/// var_dump((bool) $o, !$o);
/// echo $o ? "t" : "f", "\n";
/// ```
/// ```expect
/// 4096: Object of class LongCastableNoOperations could not be converted to bool
/// 4096: Object of class LongCastableNoOperations could not be converted to bool
/// bool(false)
/// bool(true)
/// 4096: Object of class LongCastableNoOperations could not be converted to bool
/// f
/// ```
#[php_class(final, handlers = crate::object_handlers::LONG_CASTABLE)]
pub(crate) struct LongCastableNoOperations;

#[php_class]
impl LongCastableNoOperations {
    #[php(property, private)]
    const val: i64;

    /// `__construct(int $val)`.
    fn __construct(req: &mut Request, this: This<Self>, val: i64) -> Result<(), Thrown> {
        set_value(req, this.object(), Value::int(val))
    }
}

/// `final class FloatCastableNoOperations`: cast to a float alone.
///
/// ```php
/// $o = new FloatCastableNoOperations(1.5);
/// var_dump((float) $o);
/// try { var_dump($o + 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(1.5)
/// Unsupported operand types: FloatCastableNoOperations + int
/// ```
#[php_class(final, handlers = crate::object_handlers::FLOAT_CASTABLE)]
pub(crate) struct FloatCastableNoOperations;

#[php_class]
impl FloatCastableNoOperations {
    #[php(property, private)]
    const val: f64;

    /// `__construct(float $val)`.
    fn __construct(req: &mut Request, this: This<Self>, val: f64) -> Result<(), Thrown> {
        set_value(req, this.object(), Value::float(val))
    }
}

/// `final class NumericCastableNoOperations`: cast to a number alone,
/// `_IS_NUMBER`.
///
/// ```php
/// $i = new NumericCastableNoOperations(52);
/// $f = new NumericCastableNoOperations(2.5);
/// var_dump($i + 1, $f * 2);
/// $i++;
/// var_dump($i);
/// ```
/// ```expect
/// int(53)
/// float(5)
/// int(53)
/// ```
#[php_class(final, handlers = crate::object_handlers::NUMERIC_CASTABLE)]
pub(crate) struct NumericCastableNoOperations;

#[php_class]
impl NumericCastableNoOperations {
    #[php(property, private)]
    const val: Union<i64, f64>;

    /// `__construct(int|float $val)`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        val: Union<i64, f64>,
    ) -> Result<(), Thrown> {
        let val = match val {
            Union::A(int) => Value::int(int),
            Union::B(float) => Value::float(float),
        };
        set_value(req, this.object(), val)
    }
}

/// A table whose `cast_object` answers the object's value for one type
/// and refuses every other, as the three `*_cast_object`s of the file
/// do; the value is made at zero of that type.
pub(crate) struct Castable(TypeTag);

pub(crate) static LONG_CASTABLE: Castable = Castable(TypeTag::Int);
pub(crate) static FLOAT_CASTABLE: Castable = Castable(TypeTag::Float);
pub(crate) static NUMERIC_CASTABLE: Castable = Castable(TypeTag::Number);

impl ObjectHandlers for Castable {
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let zero = if self.0 == TypeTag::Float {
            Value::float(0.0)
        } else {
            Value::int(0)
        };
        set_value(req, object, zero)
    }

    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        if to != self.0 {
            return Ok(None);
        }
        Ok(object.slot(0).map(|value| Owner::take(req, value)))
    }
}

/// `final class VmInterruptComparable`: compared by value, and every
/// comparison raises the request's interrupt, for the tests of a
/// comparison the interrupt follows.
///
/// ```php
/// var_dump(new VmInterruptComparable(1) < new VmInterruptComparable(2), new VmInterruptComparable(3) == new VmInterruptComparable(3));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_class(final, handlers = crate::object_handlers::VM_INTERRUPT)]
pub(crate) struct VmInterruptComparable;

#[php_class]
impl VmInterruptComparable {
    #[php(property, private)]
    const val: i64;

    /// `__construct(int $val)`.
    fn __construct(req: &mut Request, this: This<Self>, val: i64) -> Result<(), Thrown> {
        set_value(req, this.object(), Value::int(val))
    }
}

/// `vm_interrupt_comparable_object_handlers`.
pub(crate) struct VmInterrupt;

pub(crate) static VM_INTERRUPT: VmInterrupt = VmInterrupt;

impl ObjectHandlers for VmInterrupt {
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        set_value(req, object, Value::int(0))
    }

    /// `vm_interrupt_comparable_compare`: `ZEND_COMPARE_OBJECTS_FALLBACK`
    /// past a pair that is not two of the class, then the interrupt
    /// raised and the values compared.
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let class = lookup_class(req, b"VmInterruptComparable")?;
        let of_class = |value: &Value| match value.as_ref() {
            ValueRef::Object(object) if Some(object.class()) == class => object.slot(0),
            _ => None,
        };
        let (Some(a), Some(b)) = (of_class(left), of_class(right)) else {
            return Std.compare(req, left, right);
        };
        req.limits().interrupt().raise();
        Ok(Some(a.as_int().cmp(&b.as_int())))
    }
}

/// `class DimensionHandlersNoArrayAccess`: dimension handlers with no
/// `ArrayAccess`, each recording what it was asked into the object's
/// properties.
///
/// ```php
/// $o = new DimensionHandlersNoArrayAccess;
/// var_dump($o['k'], isset($o[1]), empty($o[2]));
/// $o['w'] = 1;
/// unset($o['u']);
/// var_dump($o);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// object(DimensionHandlersNoArrayAccess)#1 (8) {
///   ["read"]=>
///   bool(true)
///   ["write"]=>
///   bool(true)
///   ["has"]=>
///   bool(true)
///   ["unset"]=>
///   bool(true)
///   ["readType"]=>
///   int(0)
///   ["hasOffset"]=>
///   bool(true)
///   ["checkEmpty"]=>
///   int(1)
///   ["offset"]=>
///   string(1) "u"
/// }
/// ```
#[php_class(handlers = crate::object_handlers::DIMENSIONS)]
pub(crate) struct DimensionHandlersNoArrayAccess;

#[php_class]
impl DimensionHandlersNoArrayAccess {
    #[php(property)]
    const read: bool = false;
    #[php(property)]
    const write: bool = false;
    #[php(property)]
    const has: bool = false;
    #[php(property)]
    const unset: bool = false;
    #[php(property)]
    const readType: i64;
    #[php(property)]
    const hasOffset: bool = false;
    #[php(property)]
    const checkEmpty: i64;
    #[php(property)]
    const offset: Value;
}

/// `dimension_handlers_no_ArrayAccess_object_handlers`.
pub(crate) struct Dimensions;

pub(crate) static DIMENSIONS: Dimensions = Dimensions;

/// The slots `dimension_common_helper` writes.
const HAS_OFFSET: u32 = 5;
const READ_TYPE: u32 = 4;
const CHECK_EMPTY: u32 = 6;
const OFFSET: u32 = 7;

/// The access each handler records, its slot.
#[derive(Clone, Copy)]
enum Access {
    Read = 0,
    Write = 1,
    Has = 2,
    Unset = 3,
}

impl Dimensions {
    /// `dimension_common_helper`: the access recorded, whether an offset
    /// was given, and the offset.
    fn record(
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        access: Access,
    ) -> Result<(), Thrown> {
        object.set_slot(req, access as u32, Owner::bool(true))?;
        object.set_slot(req, HAS_OFFSET, Owner::bool(offset.is_some()))?;
        if let Some(offset) = offset {
            object.set_slot(req, OFFSET, Owner::take(req, *offset))?;
        }
        Ok(())
    }
}

impl ObjectHandlers for Dimensions {
    /// The read recorded with php's `BP_VAR_*` for its kind, then true.
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        Dimensions::record(req, object, offset, Access::Read)?;
        let kind = match read {
            DimensionRead::Warning => 0,
            DimensionRead::Write => 1,
            DimensionRead::ReadWrite => 2,
            DimensionRead::Silent => 3,
            DimensionRead::Unset => 5,
        };
        object.set_slot(req, READ_TYPE, Owner::int(kind))?;
        Ok(Owner::bool(true))
    }

    /// The write recorded; the value is not kept.
    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        value.give_up(req);
        Dimensions::record(req, object, offset, Access::Write)
    }

    /// The test recorded with whether it is `empty()`, then true.
    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        object.set_slot(req, CHECK_EMPTY, Owner::int(i64::from(not_empty)))?;
        Dimensions::record(req, object, Some(offset), Access::Has)?;
        Ok(true)
    }

    /// The unset recorded.
    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        Dimensions::record(req, object, Some(offset), Access::Unset)
    }
}
