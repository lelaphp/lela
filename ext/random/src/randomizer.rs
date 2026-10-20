//! `randomizer.c`: `Random\Randomizer`, the methods that turn an engine's
//! draws into ints, floats, bytes and orders. The object carries the
//! engine it draws from, which is also its readonly `engine` property; a
//! native engine's state stays the engine object's, so the Randomizer and
//! the engine advance one sequence, as php's copied pointers do.

use php_ext::abi::{Instance, This};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Thrown, arg_value_error, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::{self};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value};

use crate::rng::Rng;
use crate::{Algo, Engine, Failure, gammasection};

/// `php_random_randomizer`: the engine object, `None` before a
/// constructor or `__unserialize` has set it.
#[derive(Debug, Default)]
pub(crate) struct RandomizerEngine {
    engine: Option<PhpObject>,
}

/// The engine of the Randomizer being served.
fn engine(req: &mut Request, this: PhpObject) -> Result<Rng, Thrown> {
    match this.native::<RandomizerEngine>(req).and_then(|r| r.engine) {
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
    this: PhpObject,
    f: impl FnOnce(&mut dyn Algo) -> Result<R, Failure>,
) -> Result<R, Thrown> {
    let handle = engine(req, this)?;
    handle.with(req, f).map_err(|failure| failure.raise(req))
}

/// `randomizer_common_init`: the engine the object draws from.
fn common_init(req: &mut Request, this: PhpObject, engine: PhpObject) {
    if let Some(randomizer) = this.native_mut::<RandomizerEngine>(req) {
        randomizer.engine = Some(engine);
    }
}

/// A string of `len` bytes drawn by `f` into a buffer: the allocation
/// php makes first, so a length past the memory limit is php's fatal
/// before any draw.
fn drawn_string(
    req: &mut Request,
    this: PhpObject,
    len: usize,
    f: impl FnOnce(&mut dyn Algo, &mut Vec<u8>) -> Result<(), Failure>,
) -> Result<Owner<PhpString>, Thrown> {
    let reserved = PhpString::filled(req, len, |_| ())?;
    reserved.give_up(req);
    let mut out = Vec::with_capacity(len);
    draw(req, this, |algo| f(algo, &mut out))?;
    PhpString::new(req, &out)
}

/// `final class Random\Randomizer`, `@strict-properties`, uncloneable,
/// over [`RandomizerEngine`]: its engine, which is also its readonly
/// `engine` property.
#[php_class(
    final,
    namespace(Random),
    dynamic_properties = forbidden,
    size = 80, payload = RandomizerEngine,
    handlers = crate::UNCLONEABLE
)]
pub(crate) struct Randomizer;

/// `enum Random\IntervalBoundary`, which of `getFloat`'s two ends are
/// open; `cases()` is the engine's, as `zend_enum_register_funcs` gives
/// it.
///
/// ```php
/// var_dump(Random\IntervalBoundary::cases());
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   enum(Random\IntervalBoundary::ClosedOpen)
///   [1]=>
///   enum(Random\IntervalBoundary::ClosedClosed)
///   [2]=>
///   enum(Random\IntervalBoundary::OpenClosed)
///   [3]=>
///   enum(Random\IntervalBoundary::OpenOpen)
/// }
/// ```
#[php_class(namespace(Random))]
pub(crate) enum IntervalBoundary {
    ClosedOpen,
    ClosedClosed,
    OpenClosed,
    OpenOpen,
}

#[php_class]
impl IntervalBoundary {}

#[php_class]
impl Randomizer {
    #[php(property, readonly)]
    const engine: Instance<Engine>;

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
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] engine: Option<Instance<Engine>>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let engine = if let Some(engine) = engine.map(|engine| engine.object()) {
            Owner::take(req, engine)
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
            engine.give_up(req);
            return Err(throw_error(
                req,
                "Error",
                "Cannot modify readonly property Random\\Randomizer::$engine",
            ));
        }
        let view = *engine;
        php_ext::call::write_prop(req, &this, b"engine", engine.into_value())?;
        common_init(req, this, view);
        Ok(())
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
    fn nextInt(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let drawn = draw(req, this.object(), |algo| algo.generate())?;
        Ok((drawn.result >> 1).cast_signed())
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
    fn nextFloat(req: &mut Request, this: This<Self>) -> Result<f64, Thrown> {
        let result = draw(req, this.object(), |algo| crate::gather(algo, 8))?;
        #[allow(clippy::cast_precision_loss)]
        let float = (result >> 11) as f64 * (1.0 / (1_u64 << 53) as f64);
        Ok(float)
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
    fn getFloat(
        req: &mut Request,
        this: This<Self>,
        min: f64,
        max: f64,
        #[php(default = Random::IntervalBoundary::ClosedOpen)] boundary: IntervalBoundary,
    ) -> Result<f64, Thrown> {
        if !min.is_finite() {
            return Err(arg_value_error(req, 1, "be finite"));
        }
        if !max.is_finite() {
            return Err(arg_value_error(req, 2, "be finite"));
        }
        let (open_low, open_high) = match boundary {
            IntervalBoundary::ClosedOpen => (false, true),
            IntervalBoundary::ClosedClosed => (false, false),
            IntervalBoundary::OpenClosed => (true, false),
            IntervalBoundary::OpenOpen => (true, true),
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
        let float = draw(req, this.object(), |algo| match (open_low, open_high) {
            (false, true) => gammasection::closed_open(algo, min, max),
            (false, false) => gammasection::closed_closed(algo, min, max),
            (true, false) => gammasection::open_closed(algo, min, max),
            (true, true) => gammasection::open_open(algo, min, max),
        })?;
        if open_low && open_high && float.is_nan() {
            return Err(throw_error(
                req,
                "ValueError",
                "The given interval is empty, there are no floats between argument #1 ($min) and argument #2 ($max)",
            ));
        }
        Ok(float)
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
    fn getInt(req: &mut Request, this: This<Self>, min: i64, max: i64) -> Result<i64, Thrown> {
        if max < min {
            return Err(arg_value_error(
                req,
                2,
                "be greater than or equal to argument #1 ($min)",
            ));
        }
        draw(req, this.object(), |algo| {
            if let Some(mt) = algo.as_mt19937()
                && mt.mode == crate::engine_mt19937::Mode::Php
            {
                return Ok(mt.bad_scaling(min, max));
            }
            algo.range(min, max)
        })
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
    fn getBytes(
        req: &mut Request,
        this: This<Self>,
        length: i64,
    ) -> Result<Owner<PhpString>, Thrown> {
        if length < 1 {
            return Err(arg_value_error(req, 1, "be greater than 0"));
        }
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        drawn_string(req, this.object(), length, |algo, out| {
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
    fn getBytesFromString(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        length: i64,
    ) -> Result<Owner<PhpString>, Thrown> {
        let source = string.as_bytes().to_vec();
        if source.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        if length < 1 {
            return Err(arg_value_error(req, 2, "be greater than 0"));
        }
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        let max_offset = source.len() - 1;
        drawn_string(req, this.object(), length, |algo, out| {
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
    fn shuffleArray(
        req: &mut Request,
        this: This<Self>,
        array: PhpArray,
    ) -> Result<Owner<PhpArray>, Thrown> {
        // php shuffles a copy of the array's rows (`zend_array_dup`, then
        // `php_array_data_shuffle`): the rows read under the borrowed
        // request, views the argument keeps while the engine runs.
        let mut values: Vec<Value> = walk::rows(req, array).map(|(_, v)| v).collect();
        let handle = engine(req, this.object())?;
        handle.shuffle(req, &mut values)?;
        let mut out = PhpArray::new(req)?;
        for value in values {
            out.append(req, Owner::take(req, value))?;
        }
        Ok(out)
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
    fn shuffleBytes(
        req: &mut Request,
        this: This<Self>,
        bytes: PhpString,
    ) -> Result<Owner<PhpString>, Thrown> {
        if bytes.len() < 2 {
            return Ok(Owner::take(req, bytes));
        }
        let mut out = bytes.as_bytes().to_vec();
        let handle = engine(req, this.object())?;
        handle.shuffle(req, &mut out)?;
        PhpString::new(req, &out)
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
    fn pickArrayKeys(
        req: &mut Request,
        this: This<Self>,
        array: PhpArray,
        num: i64,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let handle = engine(req, this.object())?;
        let keys = handle.pick_keys(req, array, num)?;
        let mut out = PhpArray::new(req)?;
        for key in keys {
            let value = match key {
                Key::Int(n) => Value::int(n),
                Key::Str(s) => Value::string(s),
            };
            out.append(req, Owner::take(req, value))?;
        }
        Ok(out)
    }

    /// `Random\Randomizer::__serialize(): array`: `[members]`.
    ///
    /// ```php
    /// echo serialize(new Random\Randomizer(new Random\Engine\PcgOneseq128XslRr64(1))), "\n";
    /// ```
    /// ```expect
    /// O:17:"Random\Randomizer":1:{i:0;a:1:{s:6:"engine";O:33:"Random\Engine\PcgOneseq128XslRr64":2:{i:0;a:0:{}i:1;a:2:{i:0;s:16:"9cd108b9ceabd26b";i:1;s:16:"df3b50d88069a5ef";}}}}
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let members = crate::members(req, this.object())?;
        let mut out = PhpArray::new(req)?;
        out.append(req, members.into_value())?;
        Ok(out)
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
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        const CLASS: &str = "Random\\Randomizer";
        let this = this.object();
        if data.len() != 1 {
            return Err(crate::invalid_data(req, CLASS, None));
        }
        let Some(members) = data.get(&Key::Int(0)).and_then(|v| v.as_array()) else {
            return Err(crate::invalid_data(req, CLASS, None));
        };
        if let Err(thrown) = crate::properties_load(req, this, members) {
            return Err(crate::invalid_data(req, CLASS, Some(thrown)));
        }
        let engine = this.prop(req, b"engine").and_then(|v| v.as_object());
        let is_engine = match (engine, lookup_class(req, b"Random\\Engine")?) {
            (Some(engine), Some(class)) => engine.instance_of(req, class),
            _ => false,
        };
        match engine {
            Some(engine) if is_engine => {
                common_init(req, this, engine);
                Ok(())
            }
            _ => Err(crate::invalid_data(req, CLASS, None)),
        }
    }
}
