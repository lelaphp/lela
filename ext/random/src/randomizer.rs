//! `randomizer.c`: `Random\Randomizer`, the methods that turn an engine's
//! draws into ints, floats, bytes and orders. The object carries the
//! engine it draws from, which is also its readonly `engine` property; a
//! native engine's state stays the engine object's, so the Randomizer and
//! the engine advance one sequence, as php's copied pointers do.

use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Thrown, arg_value_error, fatal, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value};

use php_ext::decl::{ConstExpr, MethodDecl, Type};

use crate::arginfo::{class_type, method, name, param};
use crate::rng::Rng;
use crate::{Algo, Entry, Failure, gammasection, zpp};

/// `php_random_randomizer`: the engine object, `None` before a
/// constructor or `__unserialize` has set it.
#[derive(Debug, Default)]
pub(crate) struct Randomizer {
    engine: Option<PhpObject>,
}

/// The engine of the Randomizer being served.
fn engine(req: &mut Request) -> Result<Rng, Thrown> {
    let this = zpp::this(req)?;
    match this.native::<Randomizer>(req).and_then(|r| r.engine) {
        Some(engine) => Ok(Rng::of(engine)),
        None => Err(fatal(
            req,
            "InternalError: a Random\\Randomizer served before its engine was set",
        )),
    }
}

/// `f` over the engine, what it raised made php's throwable.
fn draw<R>(
    req: &mut Request,
    f: impl FnOnce(&mut dyn Algo) -> Result<R, Failure>,
) -> Result<R, Thrown> {
    let handle = engine(req)?;
    handle.with(req, f).map_err(|failure| failure.raise(req))
}

/// `randomizer_common_init`: the engine the object draws from.
fn common_init(req: &mut Request, this: PhpObject, engine: PhpObject) {
    if let Some(randomizer) = this.native_mut::<Randomizer>(req) {
        randomizer.engine = Some(engine);
    }
}

/// `Random\Randomizer::__construct(?Random\Engine $engine = null)`: the
/// engine given, or a fresh `Random\Engine\Secure`, written once into the
/// readonly `engine`.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1)); echo get_class($r->engine), "\n";
/// echo get_class((new Random\Randomizer)->engine), "\n";
/// try { $r->__construct(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new Random\Randomizer(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Random\Engine\Mt19937
/// Random\Engine\Secure
/// Cannot modify readonly property Random\Randomizer::$engine
/// Random\Randomizer::__construct(): Argument #1 ($engine) must be of type ?Random\Engine, stdClass given
/// ```
pub(crate) fn construct(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 0, 1)?;
    let this = zpp::this(req)?;
    let given = zpp::given(req, 1)?.and_then(|v| v.as_object().copied());
    let engine = if let Some(engine) = given {
        req.addref(&engine.as_value());
        engine
    } else {
        let Some(secure) = lookup_class(req, b"Random\\Engine\\Secure")? else {
            return Err(fatal(
                req,
                "InternalError: Random\\Engine\\Secure is not linked",
            ));
        };
        new_object(req, secure)?
    };
    if this.prop(req, b"engine").is_some() {
        req.release(engine.as_value());
        return Err(throw(
            req,
            "Error",
            "Cannot modify readonly property Random\\Randomizer::$engine",
        ));
    }
    let name = PhpString::new(req, b"engine")?;
    this.set_prop(req, name, engine.as_value())?;
    common_init(req, this, engine);
    Ok(Value::null())
}

/// `Random\Randomizer::nextInt(): int`: one draw shifted right, so the
/// sign bit is never set.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\PcgOneseq128XslRr64(1)); var_dump($r->nextInt(), $r->nextInt());
/// $m = new Random\Randomizer(new Random\Engine\Mt19937(1)); var_dump($m->nextInt());
/// ```
/// ```expect
/// int(8123070510531100157)
/// int(6944490242553682052)
/// int(895547922)
/// ```
pub(crate) fn next_int(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 0, 0)?;
    let drawn = draw(req, |algo| algo.generate())?;
    Ok(Value::int((drawn.result >> 1).cast_signed()))
}

/// `Random\Randomizer::nextFloat(): float`: the top 53 of eight gathered
/// bytes over 2^53, `[0, 1)`.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Xoshiro256StarStar(1)); var_dump($r->nextFloat(), $r->nextFloat());
/// $m = new Random\Randomizer(new Random\Engine\Mt19937(1)); var_dump($m->nextFloat());
/// ```
/// ```expect
/// float(0.7029218331588505)
/// float(0.5204366199388569)
/// float(0.9971848082302655)
/// ```
pub(crate) fn next_float(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 0, 0)?;
    let result = draw(req, |algo| crate::gather(algo, 8))?;
    #[allow(clippy::cast_precision_loss)]
    let float = (result >> 11) as f64 * (1.0 / (1_u64 << 53) as f64);
    Ok(Value::float(float))
}

/// `Random\Randomizer::getFloat(float $min, float $max,
/// Random\IntervalBoundary $boundary = ClosedOpen): float`: the
/// γ-section over the interval the boundary names, php's `ValueError`s
/// for a bound that is not finite and an interval with no float in it.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(3));
/// var_dump($r->getFloat(1.5, 2.5));
/// foreach ([[NAN, 1.0], [0.0, INF], [1.0, 1.0], [1.0, 1.0000000000000002]] as $ab) {
///     try { $r->getFloat($ab[0], $ab[1], Random\IntervalBoundary::OpenOpen); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// try { $r->getFloat(2.0, 1.0, Random\IntervalBoundary::ClosedClosed); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(2.121779344944182)
/// Random\Randomizer::getFloat(): Argument #1 ($min) must be finite
/// Random\Randomizer::getFloat(): Argument #2 ($max) must be finite
/// Random\Randomizer::getFloat(): Argument #2 ($max) must be greater than argument #1 ($min)
/// The given interval is empty, there are no floats between argument #1 ($min) and argument #2 ($max)
/// Random\Randomizer::getFloat(): Argument #2 ($max) must be greater than or equal to argument #1 ($min)
/// ```
pub(crate) fn get_float(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 2, 3)?;
    let min = zpp::double(req, 1)?;
    let max = zpp::double(req, 2)?;
    let boundary = zpp::given(req, 3)?.and_then(|v| v.as_object().copied());
    if !min.is_finite() {
        return Err(arg_value_error(req, 1, "be finite"));
    }
    if !max.is_finite() {
        return Err(arg_value_error(req, 2, "be finite"));
    }
    let case = match boundary {
        Some(case) => case
            .prop(req, b"name")
            .and_then(|name| name.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default(),
        None => b"ClosedOpen".to_vec(),
    };
    let (open_low, open_high) = match case.as_slice() {
        b"ClosedClosed" => (false, false),
        b"OpenClosed" => (true, false),
        b"OpenOpen" => (true, true),
        _ => (false, true),
    };
    if open_low || open_high {
        if max <= min {
            return Err(arg_value_error(
                req,
                2,
                "be greater than argument #1 ($min)",
            ));
        }
    } else if max < min {
        return Err(arg_value_error(
            req,
            2,
            "be greater than or equal to argument #1 ($min)",
        ));
    }
    let float = draw(req, |algo| match (open_low, open_high) {
        (false, true) => gammasection::closed_open(algo, min, max),
        (false, false) => gammasection::closed_closed(algo, min, max),
        (true, false) => gammasection::open_closed(algo, min, max),
        (true, true) => gammasection::open_open(algo, min, max),
    })?;
    if open_low && open_high && float.is_nan() {
        return Err(throw(
            req,
            "ValueError",
            "The given interval is empty, there are no floats between argument #1 ($min) and argument #2 ($max)",
        ));
    }
    Ok(Value::float(float))
}

/// `Random\Randomizer::getInt(int $min, int $max): int`: the engine's
/// range, and for an `Mt19937` in `MT_RAND_PHP` the scaling `mt_rand`
/// keeps for it.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1)); var_dump($r->getInt(1, 100), $r->getInt(PHP_INT_MIN, PHP_INT_MAX));
/// $p = new Random\Randomizer(new Random\Engine\Mt19937(1, MT_RAND_PHP)); var_dump($p->getInt(1, 100));
/// try { $r->getInt(2, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(46)
/// int(4064269471349965035)
///
/// Deprecated: Constant MT_RAND_PHP is deprecated since 8.3, as it uses a biased non-standard variant of Mt19937 in %s on line 3
///
/// Deprecated: The MT_RAND_PHP variant of Mt19937 is deprecated in %s on line 3
/// int(58)
/// Random\Randomizer::getInt(): Argument #2 ($max) must be greater than or equal to argument #1 ($min)
/// ```
pub(crate) fn get_int(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 2, 2)?;
    let min = zpp::long(req, 1)?;
    let max = zpp::long(req, 2)?;
    if max < min {
        return Err(arg_value_error(
            req,
            2,
            "be greater than or equal to argument #1 ($min)",
        ));
    }
    let n = draw(req, |algo| {
        if let Some(mt) = algo.as_mt19937()
            && mt.mode == crate::engine_mt19937::Mode::Php
        {
            return Ok(mt.bad_scaling(min, max));
        }
        algo.range(min, max)
    })?;
    Ok(Value::int(n))
}

/// A string of `len` bytes drawn by `f` into a buffer: the allocation
/// php makes first, so a length past the memory limit is php's fatal
/// before any draw.
fn drawn_string(
    req: &mut Request,
    len: usize,
    f: impl FnOnce(&mut dyn Algo, &mut Vec<u8>) -> Result<(), Failure>,
) -> Result<Value, Thrown> {
    let reserved = PhpString::filled(req, len, |_| ())?;
    req.release(Value::string(reserved));
    let mut out = Vec::with_capacity(len);
    draw(req, |algo| f(algo, &mut out))?;
    Ok(Value::string(PhpString::new(req, &out)?))
}

/// `Random\Randomizer::getBytes(int $length): string`: whole draws of
/// eight bytes while they fit, then each draw's bytes, least significant
/// first, until `length` are taken.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1)); echo bin2hex($r->getBytes(10)), "\n";
/// $x = new Random\Randomizer(new Random\Engine\Xoshiro256StarStar(1)); echo bin2hex($x->getBytes(10)), "\n";
/// try { $r->getBytes(0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 25f4c16aeb8047ff8c2f
/// c510c70f6daff2b3ea4c
/// Random\Randomizer::getBytes(): Argument #1 ($length) must be greater than 0
/// ```
pub(crate) fn get_bytes(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 1, 1)?;
    let length = zpp::long(req, 1)?;
    if length < 1 {
        return Err(arg_value_error(req, 1, "be greater than 0"));
    }
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    drawn_string(req, length, |algo, out| {
        let mut pending = None;
        while out.len() + 8 <= length {
            let r = algo.generate()?;
            if r.size != 8 {
                pending = Some(r);
                break;
            }
            out.extend_from_slice(&r.result.to_le_bytes());
        }
        loop {
            let r = match pending.take() {
                Some(r) => r,
                None if out.len() < length => algo.generate()?,
                None => return Ok(()),
            };
            let mut result = r.result;
            for _ in 0..r.size {
                #[allow(clippy::cast_possible_truncation)]
                out.push(result as u8);
                result >>= 8;
                if out.len() >= length {
                    break;
                }
            }
        }
    })
}

/// `Random\Randomizer::getBytesFromString(string $string, int $length):
/// string`: bytes of `string` at drawn offsets, by the engine's range
/// for a string past 256 bytes and otherwise by masking each drawn byte
/// and rejecting what falls past the end.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1));
/// echo $r->getBytesFromString("abcdef", 12), " ", strlen($r->getBytesFromString(str_repeat("xy", 200), 5)), "\n";
/// try { $r->getBytesFromString("", 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $r->getBytesFromString("a", 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// febcdaeaaeea 5
/// Random\Randomizer::getBytesFromString(): Argument #1 ($string) must not be empty
/// Random\Randomizer::getBytesFromString(): Argument #2 ($length) must be greater than 0
/// ```
pub(crate) fn get_bytes_from_string(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 2, 2)?;
    let source = zpp::string(req, 1)?;
    let length = zpp::long(req, 2)?;
    let source = source.as_bytes().to_vec();
    if source.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if length < 1 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    let max_offset = source.len() - 1;
    drawn_string(req, length, |algo, out| {
        if max_offset > 0xff {
            while out.len() < length {
                let at = algo.range(0, i64::try_from(max_offset).unwrap_or(i64::MAX))?;
                out.push(
                    source
                        .get(usize::try_from(at).unwrap_or(0))
                        .copied()
                        .unwrap_or(0),
                );
            }
            return Ok(());
        }
        let mut mask = max_offset as u64;
        mask |= mask >> 1;
        mask |= mask >> 2;
        mask |= mask >> 4;
        mask = mask.wrapping_mul(0x0101_0101_0101_0101);
        let mut failures = 0;
        while out.len() < length {
            let r = algo.generate()?;
            let mut offsets = r.result & mask;
            for _ in 0..r.size {
                #[allow(clippy::cast_possible_truncation)]
                let offset = (offsets & 0xff) as usize;
                offsets >>= 8;
                if offset > max_offset {
                    failures += 1;
                    if failures > crate::RANGE_ATTEMPTS {
                        return Err(Failure::Broken);
                    }
                    continue;
                }
                failures = 0;
                out.push(source.get(offset).copied().unwrap_or(0));
                if out.len() >= length {
                    break;
                }
            }
        }
        Ok(())
    })
}

/// `Random\Randomizer::shuffleArray(array $array): array`: the values in
/// the order `php_array_data_shuffle` walks them into, keys dropped.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1)); echo implode(",", $r->shuffleArray(range(1, 10))), "\n";
/// var_dump($r->shuffleArray(["a" => 1]));
/// ```
/// ```expect
/// 1,9,3,8,7,2,4,5,10,6
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
pub(crate) fn shuffle_array(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 1, 1)?;
    let array = zpp::array(req, 1)?;
    let mut values: Vec<Value> = array.snapshot_refs().into_iter().map(|(_, v)| v).collect();
    let handle = engine(req)?;
    handle.shuffle(req, &mut values)?;
    let mut out = PhpArray::new(req)?;
    for value in values {
        req.addref(&value);
        out.append(req, value)?;
    }
    Ok(Value::array(out))
}

/// `Random\Randomizer::shuffleBytes(string $bytes): string`: the bytes in
/// the order `php_binary_string_shuffle` walks them into.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1)); echo $r->shuffleBytes("abcdefghij"), "|", $r->shuffleBytes("a"), "|", $r->shuffleBytes(""), "\n";
/// ```
/// ```expect
/// aichgbdejf|a|
/// ```
pub(crate) fn shuffle_bytes(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 1, 1)?;
    let bytes = zpp::string(req, 1)?;
    if bytes.len() < 2 {
        let value = Value::string(bytes);
        req.addref(&value);
        return Ok(value);
    }
    let mut out = bytes.as_bytes().to_vec();
    let handle = engine(req)?;
    handle.shuffle(req, &mut out)?;
    Ok(Value::string(PhpString::new(req, &out)?))
}

/// `Random\Randomizer::pickArrayKeys(array $array, int $num): array`:
/// `php_array_pick_keys`, always as a list.
///
/// ```php
/// $r = new Random\Randomizer(new Random\Engine\Mt19937(1));
/// var_dump($r->pickArrayKeys(["a" => 1, "b" => 2, "c" => 3, "d" => 4], 2), $r->pickArrayKeys([5, 6, 7], 1));
/// try { $r->pickArrayKeys([], 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $r->pickArrayKeys([1], 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "b"
///   [1]=>
///   string(1) "d"
/// }
/// array(1) {
///   [0]=>
///   int(0)
/// }
/// Random\Randomizer::pickArrayKeys(): Argument #1 ($array) must not be empty
/// Random\Randomizer::pickArrayKeys(): Argument #2 ($num) must be between 1 and the number of elements in argument #1 ($array)
/// ```
pub(crate) fn pick_array_keys(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 2, 2)?;
    let array = zpp::array(req, 1)?;
    let num = zpp::long(req, 2)?;
    let handle = engine(req)?;
    let keys = handle.pick_keys(req, array, num)?;
    let mut out = PhpArray::new(req)?;
    for key in keys {
        let value = match key {
            Key::Int(n) => Value::int(n),
            Key::Str(s) => Value::string(s),
        };
        req.addref(&value);
        out.append(req, value)?;
    }
    Ok(Value::array(out))
}

/// `Random\Randomizer::__serialize(): array`: `[members]`.
///
/// ```php
/// echo serialize(new Random\Randomizer(new Random\Engine\PcgOneseq128XslRr64(1))), "\n";
/// ```
/// ```expect
/// O:17:"Random\Randomizer":1:{i:0;a:1:{s:6:"engine";O:33:"Random\Engine\PcgOneseq128XslRr64":2:{i:0;a:0:{}i:1;a:2:{i:0;s:16:"9cd108b9ceabd26b";i:1;s:16:"df3b50d88069a5ef";}}}}
/// ```
pub(crate) fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 0, 0)?;
    let this = zpp::this(req)?;
    let members = crate::members(req, this)?;
    let mut out = PhpArray::new(req)?;
    out.append(req, Value::array(members))?;
    Ok(Value::array(out))
}

/// `Random\Randomizer::__unserialize(array $data): void`: one element,
/// the members loaded, and the engine read back and checked to be a
/// `Random\Engine`; anything else php's `Exception`.
///
/// ```php
/// $r = unserialize(serialize(new Random\Randomizer(new Random\Engine\Mt19937(5)))); var_dump($r->getInt(1, 1000));
/// try { unserialize('O:17:"Random\Randomizer":1:{i:0;a:1:{s:6:"engine";i:1;}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { unserialize('O:17:"Random\Randomizer":1:{i:0;a:2:{s:3:"foo";N;s:6:"engine";O:32:"Random\Engine\Xoshiro256StarStar":2:{i:0;a:0:{}i:1;a:4:{i:0;s:16:"7520fbc2d6f8de46";i:1;s:16:"84d2d2b9d7ba0a34";i:2;s:16:"d975f36db6490b32";i:3;s:16:"c19991ee16785b94";}}}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(412)
/// Invalid serialization data for Random\Randomizer object
/// Invalid serialization data for Random\Randomizer object
/// ```
pub(crate) fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    const CLASS: &str = "Random\\Randomizer";
    zpp::arity(req, 1, 1)?;
    let this = zpp::this(req)?;
    let data = zpp::array(req, 1)?;
    if data.len() != 1 {
        return Err(crate::invalid_data(req, CLASS, None));
    }
    let Some(members) = data.get(&Key::Int(0)).and_then(|v| v.as_array().copied()) else {
        return Err(crate::invalid_data(req, CLASS, None));
    };
    if let Err(thrown) = crate::properties_load(req, this, members) {
        return Err(crate::invalid_data(req, CLASS, Some(thrown)));
    }
    let engine = this
        .prop(req, b"engine")
        .and_then(|v| v.as_object().copied());
    let is_engine = match (engine, lookup_class(req, b"Random\\Engine")?) {
        (Some(engine), Some(class)) => engine.instance_of(req, class),
        _ => false,
    };
    match engine {
        Some(engine) if is_engine => {
            common_init(req, this, engine);
            Ok(Value::null())
        }
        _ => Err(crate::invalid_data(req, CLASS, None)),
    }
}

/// `Random\Randomizer`'s methods in the stub's order.
pub(super) fn methods() -> Vec<(MethodDecl, Entry)> {
    let int = |name: &str| param(name, Type::Int, None);
    let returns = |ty: Type| Some(ty);
    vec![
        (
            method(
                "__construct",
                vec![param(
                    "engine",
                    Type::Nullable(Box::new(class_type("Random\\Engine"))),
                    Some(ConstExpr::Null),
                )],
                None,
            ),
            construct as Entry,
        ),
        (method("nextInt", Vec::new(), returns(Type::Int)), next_int),
        (
            method("nextFloat", Vec::new(), returns(Type::Float)),
            next_float,
        ),
        (
            method(
                "getFloat",
                vec![
                    param("min", Type::Float, None),
                    param("max", Type::Float, None),
                    param(
                        "boundary",
                        class_type("Random\\IntervalBoundary"),
                        Some(ConstExpr::EnumCase {
                            class: name("Random\\IntervalBoundary"),
                            case: name("ClosedOpen"),
                        }),
                    ),
                ],
                returns(Type::Float),
            ),
            get_float,
        ),
        (
            method("getInt", vec![int("min"), int("max")], returns(Type::Int)),
            get_int,
        ),
        (
            method("getBytes", vec![int("length")], returns(Type::String)),
            get_bytes,
        ),
        (
            method(
                "getBytesFromString",
                vec![param("string", Type::String, None), int("length")],
                returns(Type::String),
            ),
            get_bytes_from_string,
        ),
        (
            method(
                "shuffleArray",
                vec![param("array", Type::Array, None)],
                returns(Type::Array),
            ),
            shuffle_array,
        ),
        (
            method(
                "shuffleBytes",
                vec![param("bytes", Type::String, None)],
                returns(Type::String),
            ),
            shuffle_bytes,
        ),
        (
            method(
                "pickArrayKeys",
                vec![param("array", Type::Array, None), int("num")],
                returns(Type::Array),
            ),
            pick_array_keys,
        ),
        (
            method("__serialize", Vec::new(), returns(Type::Array)),
            serialize,
        ),
        (
            method(
                "__unserialize",
                vec![param("data", Type::Array, None)],
                returns(Type::Void),
            ),
            unserialize,
        ),
    ]
}
