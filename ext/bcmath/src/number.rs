//! `BcMath\Number`, 8.4's immutable arbitrary-precision number: the
//! second half of `bcmath.c`. Its methods and its operators reach the
//! arithmetic of [`crate::num`] through `bcmath_number_*_internal`,
//! written once here as [`calc`], and read their operands through
//! [`crate::operand`], the decoder the `bc*` functions use.

use std::cmp::Ordering;

use php_ext::abi::{
    CloneObj, Instance, ObjectHandlers, PropertiesFor, PropertyPlace, PropertyPurpose,
    PropertyRead, PropertyTest, Raw, Std, This, Union,
};
use php_ext::attr::php_class;
use php_ext::call::{clone_with, known_method, lookup_class, new_object};
use php_ext::coerce::{BinaryOp, Exponent, float_format, to_string, value_name};
use php_ext::decl::Name;
use php_ext::raising::{
    Thrown, arg_null_deprecated, arg_type_error, deprecated, fatal, throw_error, throw_exception_ex,
};
use php_ext::request::Request;
use php_ext::values::{
    Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef,
};

use crate::num;
use crate::{Decimal, INT_MAX, RoundingMode, value_error};

/// The class's name, `ZSTR_VAL(bcmath_number_ce->name)`.
const CLASS: &str = "BcMath\\Number";

/// `BC_MATH_NUMBER_EXPAND_SCALE`: how many digits a quotient, a root or
/// a negative power may take past the operand's scale.
const EXPAND_SCALE: usize = 10;

/// `bcmath_number_obj_t` as a payload: the number and its scale, `None`
/// until the constructor or `__unserialize()` sets them, and the values
/// the object keeps for its readers, php's cached `intern->value` and
/// the names its property table is answered under, released with the
/// object.
#[derive(Default)]
pub(crate) struct SlotState {
    held: Option<Held>,
    kept: Option<Kept>,
}

/// `intern->num` and `intern->scale`: the number, and the `bc_num`
/// php allocates for it on the request's heap ([`crate::num::Num::storage`]),
/// `None` where php's is shared: `BCG(_zero_)`, or the original's that
/// `bcmath_number_clone` takes one more reference to (design §5.13).
///
/// ```php
/// $w = new BcMath\Number('1');
/// $m = memory_get_usage(); $a = new BcMath\Number('1'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $b = new BcMath\Number('0.000'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $c = new BcMath\Number(str_repeat('9', 40) . '.5'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $d = $a->add(2); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $e = clone $a; echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($e, $d); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 104
/// 64
/// 144
/// 104
/// 64
/// -168
/// ```
struct Held {
    decimal: Decimal,
    storage: Storage,
}

/// The `bc_num` a number holds on the request's heap: none where php's
/// is `BCG(_zero_)`, which no number allocates; its own, `n_refs` one;
/// or one the original and its clones share, which `bcmath_number_clone`
/// takes one more reference to (`bc_copy_num`), held in the request's
/// [`Shared`] table under its count and given back when the last holder
/// goes (`bc_free_num`).
enum Storage {
    Zero,
    Own(Emalloc),
    Shared(u32),
}

/// One `bc_num` more than one number holds, and php's `n_refs`.
struct SharedNum {
    storage: Emalloc,
    refs: u32,
}

/// bcmath's request state: the `bc_num`s numbers share, by a key the
/// holders name, and the next key. Built at request start and dropped
/// after it, private to this crate (ADR-0005); what it holds at the end
/// is the request's to release with its heap.
#[derive(Default)]
pub(crate) struct Shared {
    nums: std::collections::HashMap<u32, SharedNum>,
    next: u32,
}

impl Storage {
    /// `bc_copy_num`: one more holder of what this one holds, an own
    /// allocation moved into the request's table first.
    fn copy(&mut self, req: &mut Request) -> Storage {
        match std::mem::replace(self, Storage::Zero) {
            Storage::Zero => Storage::Zero,
            Storage::Own(storage) => {
                let shared = req.state::<Shared>();
                let key = shared.next;
                shared.next = shared.next.wrapping_add(1);
                shared.nums.insert(key, SharedNum { storage, refs: 2 });
                *self = Storage::Shared(key);
                Storage::Shared(key)
            }
            Storage::Shared(key) => {
                if let Some(held) = req.state::<Shared>().nums.get_mut(&key) {
                    held.refs += 1;
                }
                *self = Storage::Shared(key);
                Storage::Shared(key)
            }
        }
    }

    /// `bc_free_num`: this holder gone, the allocation given back where
    /// it was the last.
    fn free(self, req: &mut Request) {
        match self {
            Storage::Zero => {}
            Storage::Own(storage) => storage.free(req),
            Storage::Shared(key) => {
                let nums = &mut req.state::<Shared>().nums;
                let last = nums.get_mut(&key).is_some_and(|held| {
                    held.refs -= 1;
                    held.refs == 0
                });
                if last && let Some(held) = nums.remove(&key) {
                    held.storage.free(req);
                }
            }
        }
    }
}

/// The payload: `bcmath_number_obj_t` keeps its number behind pointers,
/// and so the payload is one, inside the `sizeof` php allocates (design
/// §5.13).
pub(crate) type Slot = Box<SlotState>;

/// What a `BcMath\Number` keeps for the readers of its properties: the
/// rendering (`intern->value`), the owner the object holds, and the two
/// property names, php's `ZSTR_KNOWN` strings, interned and so owned by
/// no count.
struct Kept {
    value: Owner<PhpString>,
    value_name: PhpString,
    scale_name: PhpString,
}

/// Views of what a [`Kept`] holds: the rendering, then the two names.
type KeptViews = (PhpString, PhpString, PhpString);

impl Kept {
    fn views(&self) -> KeptViews {
        (*self.value, self.value_name, self.scale_name)
    }
}

/// The handlers of `BcMath\Number`, `bcmath_number_obj_handlers`.
pub(crate) static HANDLERS: Handlers = Handlers;

/// `bcmath_number_obj_handlers`: `std_object_handlers` with the number's
/// `free_obj`, `clone_obj`, `do_operation`, `compare`, the four property
/// slots over the two virtual properties, `get_properties_for` and
/// `cast_object`.
pub(crate) struct Handlers;

/// `final readonly class BcMath\Number implements \Stringable`,
/// `@strict-properties`: `$value` and `$scale` are `@virtual`, answered
/// by the handlers.
///
/// ```php
/// $n = new BcMath\Number('1.50');
/// var_dump($n, $n->value, $n->scale, (string) $n, $n instanceof Stringable);
/// ```
/// ```expect
/// object(BcMath\Number)#1 (2) {
///   ["value"]=>
///   string(4) "1.50"
///   ["scale"]=>
///   int(2)
/// }
/// string(4) "1.50"
/// int(2)
/// string(4) "1.50"
/// bool(true)
/// ```
#[php_class(
    final,
    readonly,
    namespace(BcMath),
    implements(Stringable),
    dynamic_properties = forbidden,
    size = 80, payload = Slot,
    handlers = crate::number::HANDLERS
)]
pub(crate) struct Number;

/// Core's `Stringable`, named for `implements`.
#[php_class(extern)]
pub(crate) struct Stringable;

/// A method's number argument, `Number|string|int`, which
/// `BCMATH_PARAM_NUMBER_OR_STR_OR_LONG` holds by hand.
type Operand = Raw<Union<Instance<Number>, Union<PhpString, i64>>>;

#[php_class]
impl Number {
    #[php(property, virtual, readonly)]
    const value: PhpString;
    #[php(property, virtual, readonly)]
    const scale: i64;

    /// `__construct(string|int $num)`: the number, once; its scale is
    /// the one the string is written with.
    ///
    /// ```php
    /// var_dump(new BcMath\Number('-0.0100'), new BcMath\Number(-5));
    /// try { new BcMath\Number('1.2.3'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { (new BcMath\Number(1))->__construct(2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(BcMath\Number)#1 (2) {
    ///   ["value"]=>
    ///   string(7) "-0.0100"
    ///   ["scale"]=>
    ///   int(4)
    /// }
    /// object(BcMath\Number)#2 (2) {
    ///   ["value"]=>
    ///   string(2) "-5"
    ///   ["scale"]=>
    ///   int(0)
    /// }
    /// BcMath\Number::__construct(): Argument #1 ($num) is not well-formed
    /// Cannot modify readonly property BcMath\Number::$value
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        num: Union<PhpString, i64>,
    ) -> Result<(), Thrown> {
        if held_of(req, this.object()).is_some() {
            return Err(readonly(req, "value"));
        }
        let decimal = match num {
            Union::A(text) => crate::operand(req, text.as_bytes(), 1)?,
            Union::B(value) => Decimal::from_long(value),
        };
        check_full_scale(req, &decimal, 1)?;
        set(req, this.object(), decimal)
    }

    /// `add(Number|string|int $num, ?int $scale = null): Number`: at the
    /// wider of the two scales where none is given.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.25');
    /// var_dump((string) $n->add('2.5'), (string) $n->add(new BcMath\Number('-1.25')), (string) $n->add(1, 4), $n->add('0.75')->scale);
    /// try { $n->add([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $n->add('x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(4) "3.75"
    /// string(4) "0.00"
    /// string(6) "2.2500"
    /// int(2)
    /// BcMath\Number::add(): Argument #1 ($num) must be of type int, string, or BcMath\Number, array given
    /// BcMath\Number::add(): Argument #1 ($num) is not well-formed
    /// ```
    fn add(
        req: &mut Request,
        this: This<Self>,
        num: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        method_calc(req, this, num, scale, Calc::Add)
    }

    /// `sub(Number|string|int $num, ?int $scale = null): Number`.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.25');
    /// var_dump((string) $n->sub('2.5'), (string) $n->sub(1, 1), (string) $n->sub('1.25'));
    /// ```
    /// ```expect
    /// string(5) "-1.25"
    /// string(3) "0.2"
    /// string(4) "0.00"
    /// ```
    fn sub(
        req: &mut Request,
        this: This<Self>,
        num: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        method_calc(req, this, num, scale, Calc::Sub)
    }

    /// `mul(Number|string|int $num, ?int $scale = null): Number`: at
    /// the sum of the two scales where none is given.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.25');
    /// var_dump((string) $n->mul('0.5'), (string) $n->mul('-4'), (string) $n->mul('0.5', 1));
    /// ```
    /// ```expect
    /// string(5) "0.625"
    /// string(5) "-5.00"
    /// string(3) "0.6"
    /// ```
    fn mul(
        req: &mut Request,
        this: This<Self>,
        num: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        method_calc(req, this, num, scale, Calc::Mul)
    }

    /// `div(Number|string|int $num, ?int $scale = null): Number`: at up
    /// to ten digits past the dividend's scale where none is given.
    ///
    /// ```php
    /// $n = new BcMath\Number('1');
    /// var_dump((string) $n->div('3'), (string) $n->div('4'), (string) $n->div('3', 2));
    /// try { $n->div(0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(12) "0.3333333333"
    /// string(4) "0.25"
    /// string(4) "0.33"
    /// Division by zero
    /// ```
    fn div(
        req: &mut Request,
        this: This<Self>,
        num: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        method_calc(req, this, num, scale, Calc::Div)
    }

    /// `mod(Number|string|int $num, ?int $scale = null): Number`.
    ///
    /// ```php
    /// $n = new BcMath\Number('10.5');
    /// var_dump((string) $n->mod('3'), (string) $n->mod('-3', 2));
    /// try { $n->mod('0'); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(3) "1.5"
    /// string(4) "1.50"
    /// Modulo by zero
    /// ```
    fn r#mod(
        req: &mut Request,
        this: This<Self>,
        num: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        method_calc(req, this, num, scale, Calc::Mod)
    }

    /// `divmod(Number|string|int $num, ?int $scale = null): array`: the
    /// integer quotient and the remainder, two numbers.
    ///
    /// ```php
    /// [$q, $r] = (new BcMath\Number('10.5'))->divmod('3');
    /// var_dump((string) $q, (string) $r, $r->scale);
    /// try { (new BcMath\Number(1))->divmod(0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "3"
    /// string(3) "1.5"
    /// int(1)
    /// Division by zero
    /// ```
    fn divmod(
        req: &mut Request,
        this: This<Self>,
        num: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let divisor = method_operand(req, num, 1)?;
        let given = given_scale(req, scale, 2)?;
        let dividend = held(req, this.object())?;
        let scale = given.unwrap_or(dividend.scale.max(divisor.scale));
        let Some((mut quot, mut rem)) = num::divmod(&dividend.num, &divisor.num, scale) else {
            return Err(crate::division_by_zero(req));
        };
        quot.rm_trailing_zeros();
        rem.rm_trailing_zeros();
        let quot = made(
            req,
            Decimal {
                num: quot,
                scale: 0,
            },
        )?;
        let rem = made(req, Decimal { num: rem, scale })?;
        crate::pair(req, quot.into_value(), rem.into_value())
    }

    /// `powmod(Number|string|int $exponent, Number|string|int $modulus,
    /// ?int $scale = null): Number`.
    ///
    /// ```php
    /// var_dump((string) (new BcMath\Number('4'))->powmod(13, '497'), (string) (new BcMath\Number('4'))->powmod(13, 1, 2));
    /// try { (new BcMath\Number('1.5'))->powmod(1, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { (new BcMath\Number('2'))->powmod(-1, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(3) "445"
    /// string(4) "0.00"
    /// Base number cannot have a fractional part
    /// BcMath\Number::powmod(): Argument #1 ($exponent) must be greater than or equal to 0
    /// ```
    fn powmod(
        req: &mut Request,
        this: This<Self>,
        exponent: Operand,
        modulus: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        let exponent = method_operand(req, exponent, 1)?;
        let modulus = method_operand(req, modulus, 2)?;
        let scale = crate::check_scale(req, scale.unwrap_or(0), 3)?;
        let base = held(req, this.object())?;
        match num::raisemod(&base.num, &exponent.num, &modulus.num, scale) {
            Ok(mut result) => {
                result.rm_trailing_zeros();
                made(req, Decimal { num: result, scale })
            }
            Err(error) => Err(crate::raisemod_error(req, error, true)),
        }
    }

    /// `pow(Number|string|int $exponent, ?int $scale = null): Number`:
    /// where no scale is given, the base's times the exponent, or up to
    /// ten digits past the base's for a negative one.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.5');
    /// var_dump((string) $n->pow(3), (string) $n->pow(-2), (string) $n->pow(0), (string) $n->pow(2, 1));
    /// try { $n->pow('0.5'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { (new BcMath\Number(0))->pow(-1); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(5) "3.375"
    /// string(13) "0.44444444444"
    /// string(1) "1"
    /// string(3) "2.2"
    /// BcMath\Number::pow(): Argument #1 ($exponent) exponent cannot have a fractional part
    /// Negative power of zero
    /// ```
    fn pow(
        req: &mut Request,
        this: This<Self>,
        exponent: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        method_calc(req, this, exponent, scale, Calc::Pow)
    }

    /// `sqrt(?int $scale = null): Number`: at up to ten digits past the
    /// number's scale where none is given.
    ///
    /// ```php
    /// var_dump((string) (new BcMath\Number('2'))->sqrt(), (string) (new BcMath\Number('0.25'))->sqrt(), (string) (new BcMath\Number('2'))->sqrt(3));
    /// try { (new BcMath\Number('-1'))->sqrt(); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(12) "1.4142135623"
    /// string(4) "0.50"
    /// string(5) "1.414"
    /// Base number must be greater than or equal to 0
    /// ```
    fn sqrt(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        let given = given_scale(req, scale, 1)?;
        let base = held(req, this.object())?;
        let mut scale = match given {
            Some(scale) => scale,
            None => expanded(req, base.scale)?,
        };
        let Some(mut root) = num::sqrt(&base.num, scale) else {
            return Err(php_ext::raising::value_error(
                req,
                "Base number must be greater than or equal to 0",
            ));
        };
        root.narrow(scale);
        root.rm_trailing_zeros();
        if given.is_none() {
            scale = shrunk(scale, root.scale());
        }
        made(req, Decimal { num: root, scale })
    }

    /// `floor(): Number`.
    ///
    /// ```php
    /// var_dump((string) (new BcMath\Number('-1.5'))->floor(), (new BcMath\Number('1.5'))->floor()->scale);
    /// ```
    /// ```expect
    /// string(2) "-2"
    /// int(0)
    /// ```
    fn floor(req: &mut Request, this: This<Self>) -> Result<Owner<Instance<Number>>, Thrown> {
        let base = held(req, this.object())?;
        let num = num::floor_or_ceil(&base.num, true);
        made(req, Decimal { num, scale: 0 })
    }

    /// `ceil(): Number`.
    ///
    /// ```php
    /// var_dump((string) (new BcMath\Number('-1.5'))->ceil(), (string) (new BcMath\Number('1.01'))->ceil());
    /// ```
    /// ```expect
    /// string(2) "-1"
    /// string(1) "2"
    /// ```
    fn ceil(req: &mut Request, this: This<Self>) -> Result<Owner<Instance<Number>>, Thrown> {
        let base = held(req, this.object())?;
        let num = num::floor_or_ceil(&base.num, false);
        made(req, Decimal { num, scale: 0 })
    }

    /// `round(int $precision = 0, RoundingMode $mode =
    /// RoundingMode::HalfAwayFromZero): Number`.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.955');
    /// var_dump((string) $n->round(2), (string) $n->round(), (string) $n->round(5), $n->round(5)->scale, (string) $n->round(1, RoundingMode::TowardsZero));
    /// ```
    /// ```expect
    /// string(4) "1.96"
    /// string(1) "2"
    /// string(7) "1.95500"
    /// int(5)
    /// string(3) "1.9"
    /// ```
    fn round(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] precision: i64,
        #[php(default = RoundingMode::HalfAwayFromZero)] mode: Instance<RoundingMode>,
    ) -> Result<Owner<Instance<Number>>, Thrown> {
        crate::check_precision(req, precision, 1)?;
        let mode = crate::rounding(req, mode, 2)?;
        let base = held(req, this.object())?;
        let (mut rounded, scale) = num::round(&base.num, precision, mode);
        rounded.rm_trailing_zeros();
        made(
            req,
            Decimal {
                num: rounded,
                scale,
            },
        )
    }

    /// `compare(Number|string|int $num, ?int $scale = null): int`: over
    /// the wider of the two numbers' digits where no scale is given.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.001');
    /// var_dump($n->compare('1'), $n->compare('1', 2), $n->compare(new BcMath\Number('2')));
    /// ```
    /// ```expect
    /// int(1)
    /// int(0)
    /// int(-1)
    /// ```
    fn compare(
        req: &mut Request,
        this: This<Self>,
        num: Operand,
        #[php(default = null)] scale: Option<i64>,
    ) -> Result<i64, Thrown> {
        let other = method_operand(req, num, 1)?;
        let given = given_scale(req, scale, 2)?;
        let base = held(req, this.object())?;
        let scale = given.unwrap_or(base.num.scale().max(other.num.scale()));
        Ok(crate::ordering(num::compare(&base.num, &other.num, scale)))
    }

    /// `__toString(): string`: the number at its scale.
    ///
    /// ```php
    /// echo new BcMath\Number('-0.50'), "|", new BcMath\Number(7), "\n";
    /// ```
    /// ```expect
    /// -0.50|7
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<PhpString, Thrown> {
        rendered(req, this.object())
    }

    /// `__serialize(): array`: `['value' => string]`.
    ///
    /// ```php
    /// echo serialize(new BcMath\Number('1.50')), "\n";
    /// var_dump(unserialize('O:13:"BcMath\Number":1:{s:5:"value";s:4:"1.50";}'));
    /// ```
    /// ```expect
    /// O:13:"BcMath\Number":1:{s:5:"value";s:4:"1.50";}
    /// object(BcMath\Number)#1 (2) {
    ///   ["value"]=>
    ///   string(4) "1.50"
    ///   ["scale"]=>
    ///   int(2)
    /// }
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let value = rendered(req, this.object())?;
        let mut array = PhpArray::new(req)?;
        let value = Owner::take(req, Value::string(value));
        array.set_str(req, b"value", value)?;
        Ok(array)
    }

    /// `__unserialize(array $data): void`: a non-empty well-formed
    /// `value` string, into an object not yet set.
    ///
    /// ```php
    /// foreach (['O:13:"BcMath\Number":0:{}', 'O:13:"BcMath\Number":1:{s:5:"value";s:1:"x";}', 'O:13:"BcMath\Number":1:{s:5:"value";i:1;}'] as $s) {
    ///     try { unserialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// try { (new BcMath\Number(1))->__unserialize(['value' => '2']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Invalid serialization data for BcMath\Number object
    /// Invalid serialization data for BcMath\Number object
    /// Invalid serialization data for BcMath\Number object
    /// Cannot modify readonly property BcMath\Number::$value
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let text = data
            .find(b"value")
            .and_then(|value| value.deref().as_str_bytes().map(<[u8]>::to_vec))
            .filter(|text| !text.is_empty());
        let Some(text) = text else {
            return Err(invalid_serialization(req));
        };
        if held_of(req, this.object()).is_some() {
            return Err(readonly(req, "value"));
        }
        match Decimal::parse(&text).filter(|decimal| decimal.scale <= INT_MAX) {
            Some(decimal) => set(req, this.object(), decimal),
            None => Err(invalid_serialization(req)),
        }
    }
}

/// "Invalid serialization data for BcMath\Number object".
fn invalid_serialization(req: &mut Request) -> Thrown {
    throw_exception_ex(
        req,
        "Exception",
        &format!("Invalid serialization data for {CLASS} object"),
    )
}

/// `zend_readonly_property_modification_error_ex`.
fn readonly(req: &mut Request, name: &str) -> Thrown {
    throw_error(
        req,
        "Error",
        &format!("Cannot modify readonly property {CLASS}::${name}"),
    )
}

/// The number an object holds, `None` before it is set or on an object
/// of another class.
fn held_of(req: &Request, object: PhpObject) -> Option<Decimal> {
    object
        .native::<Slot>(req)?
        .held
        .as_ref()
        .map(|held| held.decimal.clone())
}

/// The number the receiver holds, or the `InternalError` of a method
/// served before the constructor set it, where php reads a null.
fn held(req: &mut Request, object: PhpObject) -> Result<Decimal, Thrown> {
    match held_of(req, object) {
        Some(decimal) => Ok(decimal),
        None => Err(fatal(
            req,
            "InternalError: a BcMath\\Number served before its value was set",
        )),
    }
}

/// Lay `decimal` into the object in the `bc_num` php allocates for it,
/// the old rendering given up; the `Err` is the memory limit's fatal.
fn set(req: &mut Request, object: PhpObject, decimal: Decimal) -> Result<(), Thrown> {
    let storage = match decimal.num.storage() {
        Some(len) => Storage::Own(Emalloc::new(req, len)?),
        None => Storage::Zero,
    };
    let (kept, old) = object
        .native_mut::<Slot>(req)
        .map(|slot| {
            let old = slot.held.replace(Held { decimal, storage });
            (slot.kept.take(), old)
        })
        .unwrap_or_default();
    release_kept(req, kept);
    if let Some(old) = old {
        old.storage.free(req);
    }
    Ok(())
}

/// Give up what an object kept for its readers.
fn release_kept(req: &mut Request, kept: Option<Kept>) {
    if let Some(kept) = kept {
        kept.value.give_up(req);
    }
}

/// `bcmath_number_new_obj`: a fresh number holding `decimal`.
fn made(req: &mut Request, decimal: Decimal) -> Result<Owner<Instance<Number>>, Thrown> {
    let Some(class) = lookup_class(req, CLASS.as_bytes())? else {
        return Err(fatal(req, "InternalError: BcMath\\Number is not linked"));
    };
    let object = new_object(req, class)?;
    set(req, *object, decimal)?;
    Ok(object.into_instance())
}

/// What the object keeps for its readers, made on the first read:
/// `bcmath_number_value_to_str` and the two names.
///
/// ```php
/// $w = new BcMath\Number('1'); serialize($w);
/// $n = new BcMath\Number('1');
/// $m = memory_get_usage(); $s = serialize($n); $v = $n->value; echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 112
/// ```
fn kept(req: &mut Request, object: PhpObject) -> Result<KeptViews, Thrown> {
    if let Some(views) = object
        .native::<Slot>(req)
        .and_then(|slot| slot.kept.as_ref().map(Kept::views))
    {
        return Ok(views);
    }
    let decimal = held(req, object)?;
    let value_name = PhpString::interned(req, b"value")?;
    let scale_name = PhpString::interned(req, b"scale")?;
    // `bc_num2str_ex` is `zend_string_alloc`, a string of its own even
    // of one digit.
    let rendered = decimal.render();
    let value = PhpString::filled(req, rendered.len(), |bytes| {
        bytes.copy_from_slice(&rendered);
    })?;
    let kept = Kept {
        value,
        value_name,
        scale_name,
    };
    let views = kept.views();
    let Some(slot) = object.native_mut::<Slot>(req) else {
        kept.value.give_up(req);
        return Err(fatal(
            req,
            "InternalError: a BcMath\\Number served before its value was set",
        ));
    };
    slot.kept = Some(kept);
    Ok(views)
}

/// The rendering the object keeps, a view.
fn rendered(req: &mut Request, object: PhpObject) -> Result<PhpString, Thrown> {
    kept(req, object).map(|(value, _, _)| value)
}

/// `CHECK_SCALE_OVERFLOW` over an argument's scale.
fn check_full_scale(req: &mut Request, decimal: &Decimal, position: u32) -> Result<(), Thrown> {
    if decimal.scale > INT_MAX {
        return Err(value_error(
            req,
            position,
            &format!("must be between 0 and {INT_MAX}"),
        ));
    }
    Ok(())
}

/// A method's `?int $scale`: `None` for `null`, else held by
/// `bcmath_check_scale`.
fn given_scale(
    req: &mut Request,
    scale: Option<i64>,
    position: u32,
) -> Result<Option<usize>, Thrown> {
    scale
        .map(|scale| crate::check_scale(req, scale, position))
        .transpose()
}

/// `BCMATH_PARAM_NUMBER_OR_STR_OR_LONG` and
/// `bc_num_from_obj_or_str_or_long_with_err`: a number of the class as
/// it is, a string or an int as given, and anything else as
/// `zend_parse_arg_str_or_long_slow` coerces it, int first then string;
/// what neither takes is the class's own `TypeError`.
fn method_operand(req: &mut Request, raw: Operand, position: u32) -> Result<Decimal, Thrown> {
    let value = raw.value();
    let decimal = match value.as_ref() {
        ValueRef::Object(object) if object.native::<Slot>(req).is_some() => held(req, object)?,
        ValueRef::Str(text) => crate::operand(req, text.as_bytes(), position)?,
        ValueRef::Int(n) => Decimal::from_long(n),
        ValueRef::Bool(b) => Decimal::from_long(i64::from(b)),
        ValueRef::Undef | ValueRef::Null => {
            arg_null_deprecated(req, position, "BcMath\\Number|string|int");
            Decimal::from_long(0)
        }
        ValueRef::Float(f) => match long_weak(req, f) {
            Some(n) => Decimal::from_long(n),
            None => stringified(req, &value, position)?,
        },
        ValueRef::Object(object) if has_to_string(req, object) => {
            stringified(req, &value, position)?
        }
        _ => {
            let given = value_name(req, &value);
            return Err(arg_type_error(
                req,
                position,
                &format!("be of type int, string, or {CLASS}, {given} given"),
            ));
        }
    };
    check_full_scale(req, &decimal, position)?;
    Ok(decimal)
}

/// Whether the object's class has a `__toString`, what
/// `zend_parse_arg_str_weak` reads an object through.
fn has_to_string(req: &Request, object: PhpObject) -> bool {
    known_method(
        req,
        object.class(),
        &Name(b"__toString".as_slice().into()),
        None,
    )
    .is_some()
}

/// `zend_parse_arg_str_weak` then `php_str2num`: the value as a string,
/// read as a number.
fn stringified(req: &mut Request, value: &Value, position: u32) -> Result<Decimal, Thrown> {
    let text = to_string(req, value)?;
    let decimal = crate::operand(req, text.as_bytes(), position);
    text.give_up(req);
    decimal
}

/// `zend_parse_arg_long_weak` over a float: the truncated long, with
/// php's deprecation where that loses something, or `None` for a NAN or
/// a float past a long's range.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    clippy::manual_range_contains,
    reason = "php's `ZEND_DOUBLE_FITS_LONG` as two comparisons, `zend_dval_to_lval` over a float in range, and `zend_is_long_compatible`'s `(double) l == d`, the exact equality being copied"
)]
fn long_weak(req: &mut Request, f: f64) -> Option<i64> {
    if f.is_nan() || f >= 9_223_372_036_854_775_808.0 || f < -9_223_372_036_854_775_808.0 {
        return None;
    }
    let long = f as i64;
    if long as f64 != f {
        let text = float_format(f, -1, Exponent::Upper);
        deprecated(
            req,
            &format!("Implicit conversion from float {text} to int loses precision"),
        );
    }
    Some(long)
}

/// `bcmath_number_parse_num` and `bc_num_from_obj_or_str_or_long`
/// under an operator or a comparison: a number of the class, an int or
/// a string, and a float or a bool as `zend_parse_arg_long_slow` reads
/// it. `Ok(None)` is php's `FAILURE` with nothing thrown; a string that
/// is no number is `Err(Unreadable)`.
fn operator_operand(req: &mut Request, value: &Value) -> Result<Option<Decimal>, Unreadable> {
    let value = value.deref();
    Ok(Some(match value.as_ref() {
        ValueRef::Object(object) => match held_of(req, object) {
            Some(decimal) => decimal,
            None => return Ok(None),
        },
        ValueRef::Int(n) => Decimal::from_long(n),
        ValueRef::Str(text) => Decimal::parse(text.as_bytes()).ok_or(Unreadable)?,
        ValueRef::Bool(b) => Decimal::from_long(i64::from(b)),
        ValueRef::Float(f) => match long_weak(req, f) {
            Some(n) => Decimal::from_long(n),
            None => return Ok(None),
        },
        _ => return Ok(None),
    }))
}

/// A string operand that is no number.
struct Unreadable;

/// The six operations `BcMath\Number` computes, as its methods and its
/// operators name them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Calc {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

/// What the scale of a computation is: chosen from the operands'
/// (`auto_scale`) or given.
#[derive(Clone, Copy)]
enum ScaleOf {
    Auto,
    Given(usize),
}

/// Where a computation was asked for, which is how php words its
/// refusals: under an operator, `zend_value_error`; in a method,
/// `zend_argument_value_error` on the first argument.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Operator,
    Method,
}

/// `(scale + BC_MATH_NUMBER_EXPAND_SCALE)` with php's refusal where it
/// passes `INT_MAX`.
fn expanded(req: &mut Request, scale: usize) -> Result<usize, Thrown> {
    let wider = scale + EXPAND_SCALE;
    if wider > INT_MAX {
        return Err(too_large_scale(req));
    }
    Ok(wider)
}

/// php's `ValueError` for a computed scale wider than `INT_MAX`.
fn too_large_scale(req: &mut Request) -> Thrown {
    php_ext::raising::value_error(req, "scale of the result is too large")
}

/// The expanded scale given back down to the digits the quotient, root
/// or power kept, but by no more than the expansion.
fn shrunk(scale: usize, kept: usize) -> usize {
    scale - (scale - kept).min(EXPAND_SCALE)
}

/// `bcmath_number_{add,sub,mul,div,mod,pow}_internal`: the operation
/// over two numbers, the trailing zeros of what it computed dropped,
/// and the scale that reports.
fn calc(
    req: &mut Request,
    calc: Calc,
    n1: &Decimal,
    n2: &Decimal,
    scale: ScaleOf,
    site: Site,
) -> Result<Decimal, Thrown> {
    let (mut num, scale) = match calc {
        Calc::Add | Calc::Sub => {
            let scale = match scale {
                ScaleOf::Auto => n1.scale.max(n2.scale),
                ScaleOf::Given(scale) => scale,
            };
            let mut num = if calc == Calc::Add {
                num::add(&n1.num, &n2.num, scale)
            } else {
                num::sub(&n1.num, &n2.num, scale)
            };
            num.narrow(scale);
            (num, scale)
        }
        Calc::Mul => {
            let scale = match scale {
                ScaleOf::Auto => n1.scale + n2.scale,
                ScaleOf::Given(scale) => scale,
            };
            if scale > INT_MAX {
                return Err(too_large_scale(req));
            }
            let mut num = num::multiply(&n1.num, &n2.num, scale);
            num.narrow(scale);
            (num, scale)
        }
        Calc::Div => {
            let wide = match scale {
                ScaleOf::Auto => expanded(req, n1.scale)?,
                ScaleOf::Given(scale) => scale,
            };
            let Some(mut quot) = num::divide(&n1.num, &n2.num, wide) else {
                return Err(crate::division_by_zero(req));
            };
            quot.rm_trailing_zeros();
            let scale = match scale {
                ScaleOf::Auto => shrunk(wide, quot.scale()),
                ScaleOf::Given(scale) => scale,
            };
            return Ok(Decimal { num: quot, scale });
        }
        Calc::Mod => {
            let scale = match scale {
                ScaleOf::Auto => n1.scale.max(n2.scale),
                ScaleOf::Given(scale) => scale,
            };
            let Some(rem) = num::modulo(&n1.num, &n2.num, scale) else {
                return Err(crate::modulo_by_zero(req));
            };
            (rem, scale)
        }
        Calc::Pow => return power(req, n1, n2, scale, site),
    };
    num.rm_trailing_zeros();
    Ok(Decimal { num, scale })
}

/// `bcmath_number_pow_internal`.
fn power(
    req: &mut Request,
    base: &Decimal,
    exponent: &Decimal,
    scale: ScaleOf,
    site: Site,
) -> Result<Decimal, Thrown> {
    let refuse = |req: &mut Request, tail: &str| match site {
        Site::Operator => php_ext::raising::value_error(req, tail),
        Site::Method => value_error(req, 1, tail),
    };
    if exponent.num.scale() != 0 {
        return Err(refuse(req, "exponent cannot have a fractional part"));
    }
    let power = exponent.num.to_long();
    let (scale, expand) = match scale {
        ScaleOf::Given(scale) => (scale, false),
        ScaleOf::Auto => match power.cmp(&0) {
            Ordering::Greater => {
                let times = usize::try_from(power).unwrap_or(usize::MAX);
                match base.scale.checked_mul(times).filter(|&s| s <= INT_MAX) {
                    Some(scale) => (scale, false),
                    None => return Err(too_large_scale(req)),
                }
            }
            Ordering::Less => (expanded(req, base.scale)?, true),
            Ordering::Equal => (0, false),
        },
    };
    if power == 0 && exponent.num.int_is_nonzero() {
        return Err(refuse(req, "exponent is too large"));
    }
    let mut result = match num::raise(&base.num, power, scale) {
        Ok(result) => result,
        Err(error) => {
            let position = (site == Site::Method).then_some(1);
            return Err(crate::raise_error(req, error, position));
        }
    };
    result.rm_trailing_zeros();
    let scale = if expand {
        shrunk(scale, result.scale())
    } else {
        scale
    };
    Ok(Decimal { num: result, scale })
}

/// `bcmath_number_calc_method`: the argument, the scale, the receiver's
/// number, then the computation into a fresh number.
fn method_calc(
    req: &mut Request,
    this: This<Number>,
    num: Operand,
    scale: Option<i64>,
    calc_of: Calc,
) -> Result<Owner<Instance<Number>>, Thrown> {
    let operand = method_operand(req, num, 1)?;
    let scale = match given_scale(req, scale, 2)? {
        Some(scale) => ScaleOf::Given(scale),
        None => ScaleOf::Auto,
    };
    let base = held(req, this.object())?;
    let result = calc(req, calc_of, &base, &operand, scale, Site::Method)?;
    made(req, result)
}

/// The operation an operator's opcode is, the six
/// `bcmath_number_do_operation` takes.
fn calc_of(op: BinaryOp) -> Option<Calc> {
    Some(match op {
        BinaryOp::Add => Calc::Add,
        BinaryOp::Sub => Calc::Sub,
        BinaryOp::Mul => Calc::Mul,
        BinaryOp::Div => Calc::Div,
        BinaryOp::Mod => Calc::Mod,
        BinaryOp::Pow => Calc::Pow,
        BinaryOp::Concat
        | BinaryOp::BitwiseAnd
        | BinaryOp::BitwiseOr
        | BinaryOp::BitwiseXor
        | BinaryOp::ShiftLeft
        | BinaryOp::ShiftRight
        | BinaryOp::BitwiseNot => return None,
    })
}

/// The two virtual properties.
fn is_virtual(name: &[u8]) -> bool {
    name == b"value" || name == b"scale"
}

impl ObjectHandlers for Handlers {
    /// `bcmath_number_free`: the number's `bc_num` and the kept
    /// rendering given back.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let (kept, held) = object
            .native_mut::<Slot>(req)
            .map(|slot| (slot.kept.take(), slot.held.take()))
            .unwrap_or_default();
        release_kept(req, kept);
        if let Some(held) = held {
            held.storage.free(req);
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_number)
    }

    /// `bcmath_number_read_property`.
    ///
    /// ```php
    /// $n = new BcMath\Number('2.50');
    /// var_dump($n->value, $n->scale, isset($n->value), empty($n->scale), isset($n->nope), property_exists($n, 'scale'));
    /// var_dump($n->nope);
    /// ```
    /// ```expect
    /// string(4) "2.50"
    /// int(2)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    ///
    /// Warning: Undefined property: BcMath\Number::$nope in %s on line 4
    /// NULL
    /// ```
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        match name {
            b"value" => {
                let value = Value::string(rendered(req, object)?);
                Ok(Owner::take(req, value))
            }
            b"scale" => {
                let scale = held(req, object)?.scale;
                let scale = Value::int(i64::try_from(scale).unwrap_or(i64::MAX));
                Ok(Owner::take(req, scale))
            }
            _ => Std.read_property(req, object, name, read),
        }
    }

    /// `bcmath_number_write_property`: the two are read-only, and no
    /// other name may be written, the class being strict.
    ///
    /// ```php
    /// $n = new BcMath\Number(1);
    /// try { $n->value = '2'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $n->scale++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $n->other = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot modify readonly property BcMath\Number::$value
    /// Cannot modify readonly property BcMath\Number::$scale
    /// Cannot create dynamic property BcMath\Number::$other
    /// ```
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if is_virtual(name.as_bytes()) {
            value.give_up(req);
            let thrown = readonly(req, &String::from_utf8_lossy(name.as_bytes()));
            name.give_up(req);
            return Err(thrown);
        }
        Std.write_property(req, object, name, value)
    }

    /// `bcmath_number_get_property_ptr_ptr`: every name through the
    /// read and write handlers.
    fn get_property_ptr_ptr(
        &self,
        _: &mut Request,
        _: PhpObject,
        _: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        Ok(PropertyPlace::Handlers)
    }

    /// `bcmath_number_has_property`: the two exist and are set; `empty`
    /// reads the number's zero and the scale's; no other name exists.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        if test == PropertyTest::NotEmpty {
            match name {
                b"value" => return Ok(!held(req, object)?.num.is_zero()),
                b"scale" => return Ok(held(req, object)?.scale != 0),
                _ => {}
            }
        }
        Ok(is_virtual(name))
    }

    /// `bcmath_number_unset_property`.
    ///
    /// ```php
    /// $n = new BcMath\Number(1);
    /// try { unset($n->value); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($n->scale); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot unset readonly property BcMath\Number::$value
    /// Cannot unset readonly property BcMath\Number::$scale
    /// ```
    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if is_virtual(name) {
            let name = String::from_utf8_lossy(name);
            return Err(throw_error(
                req,
                "Error",
                &format!("Cannot unset readonly property {CLASS}::${name}"),
            ));
        }
        Std.unset_property(req, object, name)
    }

    /// `bcmath_number_cast_object`: a bool is whether the number is not
    /// zero; the rest is `zend_std_cast_object_tostring`'s.
    ///
    /// ```php
    /// var_dump((bool) new BcMath\Number('0.00'), (bool) new BcMath\Number('0.01'), !new BcMath\Number(0));
    /// if (new BcMath\Number(0)) { echo "set\n"; } else { echo "zero\n"; }
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// zero
    /// ```
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        if to == TypeTag::Bool {
            let zero = held(req, object)?.num.is_zero();
            return Ok(Some(Owner::bool(!zero)));
        }
        Std.cast_object(req, object, to)
    }

    /// `bcmath_number_do_operation`: the six arithmetic operators over a
    /// number and a number, an int, a string or a float, into a fresh
    /// number; any other operand is `FAILURE`, the operator's own
    /// `TypeError`.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.5');
    /// var_dump((string) ($n + 1), (string) ('2.25' - $n), (string) ($n * $n), (string) ($n / 4), (string) (7 % $n), (string) ($n ** 2));
    /// $m = $n; $m += '0.5'; $m++; var_dump((string) $m, (string) $n);
    /// try { $n + []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $n + 'x'; } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { 'x' - $n; } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { $n ** '0.5'; } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(3) "2.5"
    /// string(4) "0.75"
    /// string(4) "2.25"
    /// string(5) "0.375"
    /// string(3) "1.0"
    /// string(4) "2.25"
    /// string(3) "3.0"
    /// string(3) "1.5"
    /// Unsupported operand types: BcMath\Number + array
    /// Right string operand cannot be converted to BcMath\Number
    /// Left string operand cannot be converted to BcMath\Number
    /// exponent cannot have a fractional part
    /// ```
    fn do_operation(
        &self,
        req: &mut Request,
        op: BinaryOp,
        left: &Value,
        right: Option<&Value>,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let (Some(calc_op), Some(right)) = (calc_of(op), right) else {
            return Ok(None);
        };
        // php reads the right operand only where the left one parsed.
        let n1 = operator_operand(req, left);
        if matches!(n1, Ok(None)) {
            return Ok(None);
        }
        let n2 = operator_operand(req, right);
        let (n1, n2) = match (n1, n2) {
            (Ok(None), _) | (_, Ok(None)) => return Ok(None),
            (Err(Unreadable), _) => {
                let text = format!("Left string operand cannot be converted to {CLASS}");
                return Err(php_ext::raising::value_error(req, &text));
            }
            (_, Err(Unreadable)) => {
                let text = format!("Right string operand cannot be converted to {CLASS}");
                return Err(php_ext::raising::value_error(req, &text));
            }
            (Ok(Some(n1)), Ok(Some(n2))) => (n1, n2),
        };
        if n1.scale > INT_MAX || n2.scale > INT_MAX {
            let text = format!("scale must be between 0 and {INT_MAX}");
            return Err(php_ext::raising::value_error(req, &text));
        }
        let result = calc(req, calc_op, &n1, &n2, ScaleOf::Auto, Site::Operator)?;
        Ok(Some(made(req, result)?.into_value()))
    }

    /// `bcmath_number_compare`: the two over the wider of their digits;
    /// an operand no number is read from is `ZEND_UNCOMPARABLE`.
    ///
    /// ```php
    /// $n = new BcMath\Number('1.50');
    /// var_dump($n == '1.5', $n < 2, $n <=> new BcMath\Number('-1'), $n == 'x', $n > 'x', 'x' < $n, $n == null, new BcMath\Number(0) == false);
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// int(1)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// ```
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let Ok(Some(n1)) = operator_operand(req, left) else {
            return Ok(None);
        };
        let Ok(Some(n2)) = operator_operand(req, right) else {
            return Ok(None);
        };
        if n1.scale > INT_MAX || n2.scale > INT_MAX {
            let text = format!("scale must be between 0 and {INT_MAX}");
            return Err(php_ext::raising::value_error(req, &text));
        }
        let scale = n1.num.scale().max(n2.num.scale());
        Ok(Some(num::compare(&n1.num, &n2.num, scale)))
    }

    /// `bcmath_number_get_properties_for`: the two as a table, for every
    /// reader.
    ///
    /// ```php
    /// $n = new BcMath\Number('-3.10');
    /// var_dump((array) $n, get_object_vars($n), json_encode($n));
    /// var_export($n); echo "\n";
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["value"]=>
    ///   string(5) "-3.10"
    ///   ["scale"]=>
    ///   int(2)
    /// }
    /// array(2) {
    ///   ["value"]=>
    ///   string(5) "-3.10"
    ///   ["scale"]=>
    ///   int(2)
    /// }
    /// string(27) "{"value":"-3.10","scale":2}"
    /// \BcMath\Number::__set_state(array(
    ///    'value' => '-3.10',
    ///    'scale' => 2,
    /// ))
    /// ```
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        _: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        number_table(req, object).map(PropertiesFor::Temporary)
    }

    /// The debug purpose of `bcmath_number_get_properties_for`, the
    /// table `var_dump()` and `print_r()` show, with one owner, the
    /// caller's.
    ///
    /// ```php
    /// print_r(new BcMath\Number('2.5')); echo "\n";
    /// ```
    /// ```expect
    /// BcMath\Number Object
    /// (
    ///     [value] => 2.5
    ///     [scale] => 1
    /// )
    /// ```
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        number_table(req, object).map(|table| Some(table.into()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }
}

/// `bcmath_number_get_properties_for`'s table, a temporary one, which
/// every purpose reads: `value` and `scale`, each one owner.
fn number_table(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let (value, value_name, scale_name) = kept(req, object)?;
    let scale = held(req, object)?.scale;
    let scale = Value::int(i64::try_from(scale).unwrap_or(i64::MAX));
    let mut table = PhpArray::new(req)?;
    table.set(
        req,
        Owner::take(req, Key::Str(value_name)),
        Owner::take(req, Value::string(value)),
    )?;
    table.set(
        req,
        Owner::take(req, Key::Str(scale_name)),
        Owner::take(req, scale),
    )?;
    Ok(table)
}

/// `bcmath_number_clone`: a fresh number holding the same `bc_num`, one
/// more reference to it, and the same rendering, so the allocation goes
/// back when the last of the original and its clones does:
///
/// ```php
/// $a = new BcMath\Number('1.5');
/// $m = memory_get_usage(); $c = clone $a; $n = memory_get_usage(); unset($a); $o = memory_get_usage(); unset($c);
/// var_dump($n - $m, $o - $n, memory_get_usage() - $o);
/// ```
/// ```expect
/// int(64)
/// int(-64)
/// int(-104)
/// ```
///
///
/// ```php
/// $n = new BcMath\Number('1.5');
/// $c = clone $n;
/// var_dump($c == $n, $c === $n, (string) $c, $c->scale);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(3) "1.5"
/// int(1)
/// ```
fn clone_number(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let taken = object.native_mut::<Slot>(req).map(|slot| {
        let held = slot.held.as_mut().map(|held| {
            let storage = std::mem::replace(&mut held.storage, Storage::Zero);
            (held.decimal.clone(), storage)
        });
        (held, slot.kept.as_ref().map(Kept::views))
    });
    let Some((held, kept)) = taken else {
        return clone_with(req, &object, Slot::default());
    };
    // `bc_copy_num(original->num)`: the clone one more holder of the
    // original's `bc_num`.
    let held = held.map(|(decimal, mut storage)| {
        let copied = storage.copy(req);
        if let Some(original) = object
            .native_mut::<Slot>(req)
            .and_then(|slot| slot.held.as_mut())
        {
            original.storage = storage;
        }
        Held {
            decimal,
            storage: copied,
        }
    });
    // `zend_string_copy(original->value)`: the rendering shared.
    let kept = kept.map(|(value, value_name, scale_name)| Kept {
        value: Owner::take(req, value),
        value_name,
        scale_name,
    });
    clone_with(req, &object, Box::new(SlotState { held, kept }))
}
