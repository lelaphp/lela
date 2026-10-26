//! `GMP`, the class every number of the extension is: `gmp_object`, an
//! `mpz_t` beside the `zend_object`, as the payload [`Mpz`], with
//! `gmp_object_handlers`' slots: the cast, the debug table, the clone,
//! the operators, the comparison and the `C:` unserializer, and the
//! class's three methods.

use std::cmp::Ordering;
use std::ops::ControlFlow;

use php_ext::abi::{ClassUnserialize, CloneObj, DebugInfo, Instance, ObjectHandlers, This, Union};
use php_ext::attr::php_class;
use php_ext::call::{clone_with, lookup_class, new_object};
use php_ext::coerce::{BinaryOp, numeric_string, to_long, type_name, value_name};
use php_ext::raising::{
    Thrown, deprecated, fatal, throw, throw_chained, throw_error, type_error, value_error,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};
use php_ext_standard::Unserializer;

use crate::ffi::Mpz;
use crate::throwables::Throwables;
use crate::{from_text, initialized, operand, verify_base};

/// `gmp_object_handlers`: `std_object_handlers` with php's `cast_object`,
/// `get_debug_info`, `clone_obj`, `do_operation` and `compare`, and the
/// class's `unserialize`; `free_obj`'s `mpz_clear` is [`Mpz`]'s drop.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `GMP`.
pub(crate) struct Handlers;

/// `final class GMP`: one number, `sizeof(gmp_object)`, whose limbs
/// libgmp allocates with `malloc`, so `memory_get_usage()` counts the
/// object alone whatever the number's size.
///
/// ```php
/// $w = gmp_init(1);
/// $m = memory_get_usage(); $a = gmp_init(1); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $b = gmp_init(str_repeat('9', 400)); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $c = $b * $b; echo memory_get_usage() - $m, "\n";
/// var_dump(new GMP('12'), (new ReflectionClass('GMP'))->isFinal());
/// ```
/// ```expect
/// 56
/// 56
/// 56
/// object(GMP)#5 (1) {
///   ["num"]=>
///   string(2) "12"
/// }
/// bool(true)
/// ```
#[php_class(final, size = 72, payload = Mpz, handlers = crate::object::HANDLERS)]
#[allow(clippy::upper_case_acronyms)]
pub(crate) struct GMP;

#[php_class]
impl GMP {
    /// `__construct(int|string $num = 0, int $base = 0)`: the number,
    /// read as `gmp_init` reads it.
    ///
    /// ```php
    /// var_dump((string) new GMP(), (string) new GMP('ff', 16), (string) new GMP(-3));
    /// try { new GMP('ff'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { new GMP(1, 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "0"
    /// string(3) "255"
    /// string(2) "-3"
    /// GMP::__construct(): Argument #1 ($num) is not an integer string
    /// GMP::__construct(): Argument #2 ($base) must be 0 or between 2 and 62
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] num: Union<PhpString, i64>,
        #[php(default = 0)] base: i64,
    ) -> Result<(), Thrown> {
        verify_base(req, base, 2)?;
        let n = initialized(req, num, base)?;
        *this.payload_mut(req)? = n;
        Ok(())
    }

    /// `__serialize(): array`: the number in hexadecimal, then the
    /// properties where the object has any.
    ///
    /// ```php
    /// $n = gmp_init(255);
    /// var_dump($n->__serialize(), serialize(gmp_init(-16)));
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   string(2) "ff"
    /// }
    /// string(28) "O:3:"GMP":1:{i:0;s:3:"-10";}"
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let object = this.object();
        let text = held(req, object, |n| n.to_text(16));
        let mut array = PhpArray::new(req)?;
        let text = PhpString::new(req, &text)?;
        array.append(req, text.into_value())?;
        let table = object.property_table(req)?;
        let rows: Vec<(Key, Value)> = walk::rows(req, table).collect();
        if !rows.is_empty() {
            let mut props = PhpArray::new(req)?;
            for (key, value) in rows {
                let key = match key {
                    Key::Str(name) => Owner::take(req, Key::lookup(name)),
                    Key::Int(n) => Owner::index(n),
                };
                props.set(req, key, Owner::take(req, value))?;
            }
            array.append(req, props.into_value())?;
        }
        Ok(array)
    }

    /// `__unserialize(array $data): void`: the hexadecimal number at 0,
    /// and the properties at 1, loaded as `object_properties_load`
    /// loads them.
    ///
    /// ```php
    /// var_dump(unserialize('O:3:"GMP":1:{i:0;s:2:"ff";}'));
    /// foreach (['O:3:"GMP":0:{}', 'O:3:"GMP":1:{i:0;s:1:"x";}', 'O:3:"GMP":2:{i:0;s:1:"1";i:1;i:0;}'] as $s) {
    ///     try { unserialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// object(GMP)#1 (1) {
    ///   ["num"]=>
    ///   string(3) "255"
    /// }
    /// Could not unserialize number
    /// Could not unserialize number
    /// Could not unserialize properties
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let object = this.object();
        let text = data
            .get(&Key::Int(0))
            .and_then(|num| num.as_php_string().map(|text| text.as_bytes().to_vec()));
        set(req, object, text.as_deref(), 16)?;
        let Some(props) = data.get(&Key::Int(1)) else {
            return Ok(());
        };
        let Some(props) = props.as_array() else {
            let ce = Throwables::of(req)?;
            return Err(throw(req, ce.exception, "Could not unserialize properties"));
        };
        // `object_properties_load`'s `ZEND_HASH_FOREACH_KEY_VAL(properties, …)`
        // (`Z/Zend/zend_API.c:1752`), no owner of the table taken: what
        // unserializes it holds it while a deprecation reaches the program.
        walk::foreach(req, props, Hold::None, |req, row| -> walk::Step {
            let key = row.key().clone();
            let value = row.value().deref();
            let value = Owner::take(req, value);
            match key {
                Key::Str(name) => {
                    let name = Owner::take(req, name);
                    object.unserialize_prop(req, name, value)?;
                }
                // `zend_hash_index_update` into the table, under the
                // deprecation of a dynamic property.
                Key::Int(n) => {
                    deprecated(
                        req,
                        &format!("Creation of dynamic property GMP::${n} is deprecated"),
                    );
                    object.set_row(req, &Key::Int(n), value)?;
                }
            }
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(())
    }
}

/// What `f` reads of the number an object holds, zero's where it holds
/// none.
fn held<T>(req: &Request, object: PhpObject, f: impl FnOnce(&Mpz) -> T) -> T {
    match object.native::<Mpz>(req) {
        Some(n) => f(n),
        None => f(&Mpz::default()),
    }
}

/// `convert_zstr_to_gmp(num, text, base, 0)` into the object, or php's
/// "Could not unserialize number" over what the conversion threw,
/// `zend_throw_exception` over a fixed string.
fn set(req: &mut Request, object: PhpObject, text: Option<&[u8]>, base: i64) -> Result<(), Thrown> {
    let message = "Could not unserialize number";
    let Some(text) = text else {
        let ce = Throwables::of(req)?;
        return Err(throw(req, ce.exception, message));
    };
    match from_text(req, text, base, 0) {
        Ok(n) => {
            if let Some(slot) = object.native_mut::<Mpz>(req) {
                *slot = n;
            }
            Ok(())
        }
        Err(thrown) => {
            let ce = Throwables::of(req)?;
            Err(throw_chained(req, thrown, ce.exception, message))
        }
    }
}

/// `gmp_create`: a fresh `GMP` holding `n`, one owner, the caller's.
pub(crate) fn made(req: &mut Request, n: Mpz) -> Result<Owner<Instance<GMP>>, Thrown> {
    let Some(class) = lookup_class(req, b"GMP")? else {
        return Err(fatal(req, "InternalError: GMP is not linked"));
    };
    let object = new_object(req, class)?;
    if let Some(slot) = object.native_mut::<Mpz>(req) {
        *slot = n;
    }
    Ok(object.into_instance())
}

/// An operand of an operator or a comparison, read by [`operand`] with
/// php's `TypeError` where it is no number.
fn operator_operand(req: &mut Request, value: &Value, position: u32) -> Result<Mpz, Thrown> {
    if let Some(n) = operand(req, value, position, true)? {
        return Ok(n);
    }
    let given = value_name(req, &value.deref());
    let text = [
        b"Number must be of type GMP|string|int, ".as_slice(),
        given.as_slice(),
        b" given".as_slice(),
    ]
    .concat();
    Err(type_error(req, &text))
}

/// `binop_operator_helper`: both operands, the divisor of a division or
/// a modulo held to not zero.
fn binop(
    req: &mut Request,
    left: &Value,
    right: &Value,
    zero: Option<&str>,
) -> Result<(Mpz, Mpz), Thrown> {
    let a = operator_operand(req, left, 1)?;
    let b = operator_operand(req, right, 2)?;
    if let Some(what) = zero
        && b.sign() == 0
    {
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.division_by_zero_error, what));
    }
    Ok((a, b))
}

/// `shift_operator_helper`'s refusal of an operand it cannot read.
fn unsupported(req: &mut Request, op: BinaryOp, left: &Value, right: &Value) -> Thrown {
    let sigil = match op {
        BinaryOp::Pow => "**",
        BinaryOp::ShiftLeft => "<<",
        _ => ">>",
    };
    let (l, r) = (type_name(req, left), type_name(req, right));
    let text = [
        b"Unsupported operand types: ".as_slice(),
        &l,
        b" ",
        sigil.as_bytes(),
        b" ",
        &r,
    ]
    .concat();
    type_error(req, &text)
}

/// `shift_operator_helper`: `**`, `<<` and `>>`, whose right operand is
/// a count read as a long, never negative.
fn shift(req: &mut Request, op: BinaryOp, left: &Value, right: &Value) -> Result<Mpz, Thrown> {
    let count = match right.deref().as_ref() {
        ValueRef::Int(n) => n,
        ValueRef::Object(o) if o.native::<Mpz>(req).is_some() => to_long(req, right)?,
        ValueRef::Float(_) => to_long(req, right)?,
        ValueRef::Str(text) => match numeric_string(text.as_bytes()).and_then(|v| v.as_int()) {
            Some(n) => n,
            None => {
                return Err(value_error(req, "Number is not an integer string"));
            }
        },
        _ => return Err(unsupported(req, op, left, right)),
    };
    let Ok(count) = u64::try_from(count) else {
        let what = if op == BinaryOp::Pow {
            "Exponent"
        } else {
            "Shift"
        };
        let text = format!("{what} must be greater than or equal to 0");
        return Err(value_error(req, &text));
    };
    let Some(a) = operand(req, left, 1, true)? else {
        return Err(unsupported(req, op, left, right));
    };
    Ok(match op {
        BinaryOp::Pow => a.pow_ui(count),
        BinaryOp::ShiftLeft => a.mul_2exp(count),
        _ => a.fdiv_q_2exp(count),
    })
}

impl ObjectHandlers for Handlers {
    /// `gmp_clone_obj`: a fresh number, the members copied, `mpz_set`.
    ///
    /// ```php
    /// $a = gmp_init(5); $b = clone $a;
    /// gmp_setbit($b, 1);
    /// var_dump(gmp_strval($a), gmp_strval($b), $a == 5);
    /// ```
    /// ```expect
    /// string(1) "5"
    /// string(1) "7"
    /// bool(true)
    /// ```
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_gmp)
    }

    /// `gmp_cast_object`: a string in base 10, an int by `mpz_get_si`,
    /// a float by `mpz_get_d`, a number as either where it fits, a bool
    /// for not zero.
    ///
    /// ```php
    /// $n = gmp_init('18446744073709551621');
    /// var_dump((string) $n, (int) $n, (float) $n, (bool) gmp_init(0), $n + 1.5);
    /// ```
    /// ```expect
    /// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 3
    /// string(20) "18446744073709551621"
    /// int(5)
    /// float(1.8446744073709552E+19)
    /// bool(false)
    /// object(GMP)#2 (1) {
    ///   ["num"]=>
    ///   string(20) "18446744073709551622"
    /// }
    /// ```
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let cast = match to {
            TypeTag::String => {
                let text = held(req, object, |n| n.to_text(10));
                return Ok(Some(PhpString::new(req, &text)?.into_value()));
            }
            TypeTag::Int => Value::int(held(req, object, Mpz::to_long)),
            TypeTag::Float => Value::float(held(req, object, Mpz::to_double)),
            TypeTag::Number => held(req, object, |n| {
                if n.fits_long() {
                    Value::int(n.to_long())
                } else {
                    Value::float(n.to_double())
                }
            }),
            TypeTag::Bool => Value::bool(held(req, object, |n| n.sign() != 0)),
            _ => return Ok(None),
        };
        Ok(Some(Owner::take(req, cast)))
    }

    /// `gmp_get_debug_info`: the properties, then `num` in base 10.
    ///
    /// ```php
    /// $n = gmp_init(-12); @$n->tag = 'x';
    /// var_dump($n); print_r(gmp_init(3)); echo "\n";
    /// ```
    /// ```expect
    /// object(GMP)#1 (2) {
    ///   ["tag"]=>
    ///   string(1) "x"
    ///   ["num"]=>
    ///   string(3) "-12"
    /// }
    /// GMP Object
    /// (
    ///     [num] => 3
    /// )
    /// ```
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        let own = object.property_table(req)?;
        let rows: Vec<(Key, Value)> = walk::rows(req, own).collect();
        let mut table = PhpArray::new(req)?;
        for (key, value) in rows {
            let key = Owner::take(req, key.clone());
            table.set(req, key, Owner::take(req, value))?;
        }
        let text = held(req, object, |n| n.to_text(10));
        let num = PhpString::new(req, &text)?;
        table.set_str(req, b"num", num.into_value())?;
        Ok(Some(table.into()))
    }

    /// `gmp_do_operation`: the arithmetic, bitwise and shift operators
    /// over a number and a number, an int or a string, into a fresh
    /// number; `.` is not one of them.
    ///
    /// ```php
    /// $a = gmp_init(42); $b = gmp_init(17);
    /// echo $a + $b, ' ', 42 - $b, ' ', $a * '2', ' ', $a / 5, ' ', -$a % 5, ' ', $a ** 2, ' ', $a | 1, ' ', $a & $b, ' ', $a ^ 3, ' ', $a << 2, ' ', -$a >> 1, ' ', ~$a, "\n";
    /// $c = $a; $c++; $c += 10; echo $c, ' ', $a, "\n";
    /// foreach ([fn () => $a / 0, fn () => $a % 0, fn () => $a + [], fn () => $a + 'x', fn () => $a ** -1, fn () => $a << [], fn () => $a + null] as $f) {
    ///     try { $f(); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// 59 25 84 8 3 1764 43 0 41 168 -21 -43
    /// 53 42
    /// DivisionByZeroError: Division by zero
    /// DivisionByZeroError: Modulo by zero
    /// TypeError: Number must be of type GMP|string|int, array given
    /// ValueError: Number is not an integer string
    /// ValueError: Exponent must be greater than or equal to 0
    /// TypeError: Unsupported operand types: GMP << array
    /// TypeError: Number must be of type GMP|string|int, null given
    /// ```
    fn do_operation(
        &self,
        req: &mut Request,
        op: BinaryOp,
        left: &Value,
        right: Option<&Value>,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let n = match (op, right) {
            (BinaryOp::BitwiseNot, _) => match left.deref().as_object() {
                Some(object) => held(req, object, Mpz::com),
                None => return Ok(None),
            },
            (BinaryOp::Concat, _) | (_, None) => return Ok(None),
            (BinaryOp::Pow | BinaryOp::ShiftLeft | BinaryOp::ShiftRight, Some(right)) => {
                shift(req, op, left, right)?
            }
            (BinaryOp::Div, Some(right)) => {
                let (a, b) = binop(req, left, right, Some("Division by zero"))?;
                a.tdiv_q(&b)
            }
            (BinaryOp::Mod, Some(right)) => {
                let (a, b) = binop(req, left, right, Some("Modulo by zero"))?;
                a.modulo(&b)
            }
            (_, Some(right)) => {
                let (a, b) = binop(req, left, right, None)?;
                match op {
                    BinaryOp::Add => a.add(&b),
                    BinaryOp::Sub => a.sub(&b),
                    BinaryOp::Mul => a.mul(&b),
                    BinaryOp::BitwiseAnd => a.and(&b),
                    BinaryOp::BitwiseOr => a.ior(&b),
                    _ => a.xor(&b),
                }
            }
        };
        Ok(Some(made(req, n)?.into_value()))
    }

    /// `gmp_compare`: the two as numbers, an operand that is none
    /// php's `TypeError`.
    ///
    /// ```php
    /// $n = gmp_init(10);
    /// var_dump($n == 10, $n < '0x0b', 9 <=> $n, $n == gmp_init(10), $n > -1.5);
    /// try { $n == []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Deprecated: Implicit conversion from float -1.5 to int loses precision in %s on line 3
    /// bool(true)
    /// bool(true)
    /// int(-1)
    /// bool(true)
    /// bool(true)
    /// Number must be of type GMP|string|int, array given
    /// ```
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let a = operator_operand(req, left, 1)?;
        let b = operator_operand(req, right, 2)?;
        Ok(Some(a.cmp(&b)))
    }

    fn unserialize(&self) -> Option<ClassUnserialize> {
        Some(unserialize_custom)
    }
}

/// `gmp_clone_obj`.
fn clone_gmp(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let copy = held(req, object, Mpz::clone);
    clone_with(req, &object, copy)
}

/// `gmp_unserialize`, a `C:` record's payload: the number as a
/// serialized base-10 string, then the properties as a serialized
/// array copied into the table, both read in the var hash of the
/// `unserialize()` around it.
///
/// ```php
/// var_dump(unserialize('C:3:"GMP":15:{s:2:"42";a:0:{}}'));
/// foreach (['C:3:"GMP":0:{}', 'C:3:"GMP":9:{s:2:"42";}'] as $s) {
///     try { unserialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// object(GMP)#1 (1) {
///   ["num"]=>
///   string(2) "42"
/// }
/// Could not unserialize number
/// Could not unserialize properties
/// ```
fn unserialize_custom(req: &mut Request, object: PhpObject, data: &[u8]) -> Result<(), Thrown> {
    let mut reader = Unserializer::new(req, data)?;
    let read = read_custom(req, &mut reader, object);
    reader.finish(req, read)
}

/// The two values of a `C:` payload, as [`unserialize_custom`] reads
/// them.
fn read_custom(
    req: &mut Request,
    reader: &mut Unserializer<'_>,
    object: PhpObject,
) -> Result<(), Thrown> {
    let text = reader
        .value(req)?
        .and_then(|num| num.as_php_string().map(|text| text.as_bytes().to_vec()));
    set(req, object, text.as_deref(), 10)?;
    let Some(props) = reader.value(req)?.and_then(|props| props.as_array()) else {
        let ce = Throwables::of(req)?;
        return Err(throw(req, ce.exception, "Could not unserialize properties"));
    };
    // `object_properties_load`'s `ZEND_HASH_FOREACH_KEY_VAL(properties, …)`
    // (`Z/Zend/zend_API.c:1752`), no owner of the table taken: what
    // unserializes it holds it while a deprecation reaches the program.
    walk::foreach(req, props, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        let value = Owner::take(req, value);
        object.set_row(req, &key, value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}
