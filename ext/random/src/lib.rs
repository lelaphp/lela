//! The `random` extension: `php-src/ext/random` as one crate under `ext/`
//! (ADR-0004). One dependency beside the facade, the host's CSPRNG; one
//! entry, `extension()`; its state on the value (ADR-0005). The phpt
//! subset is `php-src/ext/random/tests`, through `xtask phpt --subset`.
//!
//! This file is `random.c` and `php_random.h`: the engine as one trait in
//! `php_random_algo`'s shape ([`Algo`]), the range reduction every engine
//! shares, the request state (the default `Mt19937` and the combined
//! LCG), the natives, and the dispatch from an engine object to the state
//! its class carries. Each engine and the Randomizer is a file named for
//! its php-src file. What `ext/standard` reads is [`Rng`], the RNG design
//! §4.10 names: `php_random_algo_with_state`, with the walks of `shuffle`,
//! `str_shuffle` and `array_rand` beside it.
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

mod csprng;
mod engine_mt19937;
mod engine_pcgoneseq128xslrr64;
mod engine_secure;
mod engine_user;
mod engine_xoshiro256starstar;
mod gammasection;
mod randomizer;
mod rng;
mod throwables;

use std::any::Any;
use std::marker::PhantomData;
use std::ops::ControlFlow;

use php_ext::abi::{CloneObj, Extension, ExtensionBuilder, ObjectHandlers, Passed, Raw, Refusals};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::clone_with;
use php_ext::raising::{
    Thrown, arg_count_error, arg_value_error, deprecated, throw, throw_chained, throw_error,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString};

pub use rng::Rng;

use engine_mt19937::{EngineMt19937, Mode, Mt19937, StateMt19937};
use engine_pcgoneseq128xslrr64::{EnginePcgOneseq128XslRr64, PcgOneseq128XslRr64};
use engine_secure::{Secure, StateSecure};
use engine_xoshiro256starstar::{EngineXoshiro256StarStar, Xoshiro256StarStar};
use randomizer::{IntervalBoundary, Randomizer};
use throwables::Throwables;

/// `PHP_MT_RAND_MAX`, `(1 << 31) - 1`.
pub(crate) const MT_RAND_MAX: i64 = 0x7FFF_FFFF;

/// `PHP_RANDOM_RANGE_ATTEMPTS`.
const RANGE_ATTEMPTS: u32 = 50;

/// `php_random_result`: what one `generate` drew, `size` bytes of
/// `result` from the low end.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Generated {
    pub(crate) result: u64,
    pub(crate) size: usize,
}

/// What a draw raised, carried out of the engine and made php's
/// throwable once the request is in hand again: `EG(exception)` after
/// an engine call.
pub(crate) enum Failure {
    /// What a user engine's `generate()` threw, or raised about what it
    /// answered.
    Thrown(Thrown),
    /// php's `BrokenRandomEngineError` after `PHP_RANDOM_RANGE_ATTEMPTS`
    /// rejected draws.
    Broken,
    /// The CSPRNG's `errstr`, thrown as `Random\RandomException`.
    Csprng(String),
}

impl Failure {
    /// The throwable, as php would have left it pending.
    pub(crate) fn raise(self, req: &mut Request) -> Thrown {
        if let Failure::Thrown(thrown) = self {
            return thrown;
        }
        let ce = match Throwables::of(req) {
            Ok(ce) => ce,
            Err(fatal) => return fatal,
        };
        match self {
            Failure::Thrown(thrown) => thrown,
            Failure::Broken => throw_error(
                req,
                ce.broken_random_engine_error,
                &format!(
                    "Failed to generate an acceptable random number in {RANGE_ATTEMPTS} attempts"
                ),
            ),
            Failure::Csprng(why) => throw(req, ce.random_exception, &why),
        }
    }

    /// A constructor's seed the CSPRNG could not give: php's second
    /// throw, `Failed to generate a random seed`, over the first.
    pub(crate) fn seed_failure(self, req: &mut Request) -> Thrown {
        match self {
            Failure::Csprng(_) => match Throwables::of(req) {
                Ok(ce) => throw(req, ce.random_exception, "Failed to generate a random seed"),
                Err(fatal) => fatal,
            },
            other => other.raise(req),
        }
    }
}

/// `php_random_algo`: one engine over its own state. The state is the
/// implementor, a struct mirroring php's status struct, so what a draw
/// changes is a field of it; `generate` is the one operation every engine
/// has, and `range` is `php_random_range` over it unless the engine has
/// its own, as `Secure` does.
pub(crate) trait Algo {
    /// `algo->generate`.
    fn generate(&mut self) -> Result<Generated, Failure>;

    /// `algo->range`: a uniform integer in `[min, max]`.
    fn range(&mut self, min: i64, max: i64) -> Result<i64, Failure> {
        range(self, min, max)
    }

    /// The state when the engine is `Mt19937`, which
    /// `Randomizer::getInt` asks as php compares `algo->range` against
    /// `php_random_algo_mt19937.range`.
    fn as_mt19937(&mut self) -> Option<&mut StateMt19937> {
        None
    }
}

/// One element of a state's serialised form: a word of the state as
/// its little-endian bytes, which the array holds as
/// `php_random_bin2hex_le` writes them, or a long.
pub(crate) enum Field {
    Word(Vec<u8>),
    Long(i64),
}

/// `algo->serialize` and `algo->unserialize`, which the three seedable
/// engines have and `Secure` and a user engine do not.
pub(crate) trait Serial: Algo {
    fn serialize(&self) -> Vec<Field>;
    fn unserialize(&mut self, data: PhpArray) -> bool;
}

/// Draws until `width` bytes are gathered, `r.result` shifted into place
/// by the bytes already taken, as the loops of `php_random_range32` and
/// `php_random_range64` and `Randomizer::nextFloat` gather them.
fn gather<A: Algo + ?Sized>(algo: &mut A, width: usize) -> Result<u64, Failure> {
    let mut result = 0_u64;
    let mut total = 0;
    while total < width {
        let r = algo.generate()?;
        #[allow(clippy::cast_possible_truncation)]
        let part = if width == 4 {
            u64::from(r.result as u32)
        } else {
            r.result
        };
        result |= part << (total * 8);
        total += r.size;
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(if width == 4 {
        u64::from(result as u32)
    } else {
        result
    })
}

/// `php_random_range32`: `[0, umax]` from four gathered bytes, unbiased
/// by rejecting what lies over the last whole multiple.
///
/// ```php
/// mt_srand(1);
/// var_dump(mt_rand(0, 9), mt_rand(0, 15), mt_rand(-1000, 1000), mt_rand(0, 0xFFFFFFFF));
/// ```
/// ```expect
/// int(5)
/// int(11)
/// int(-988)
/// int(4005303368)
/// ```
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn range32<A: Algo + ?Sized>(algo: &mut A, umax: u32) -> Result<u32, Failure> {
    let mut result = gather(algo, 4)? as u32;
    if umax == u32::MAX {
        return Ok(result);
    }
    let umax = umax + 1;
    if umax.is_power_of_two() {
        return Ok(result & (umax - 1));
    }
    let limit = u32::MAX - (u32::MAX % umax) - 1;
    let mut count = 0;
    while result > limit {
        count += 1;
        if count > RANGE_ATTEMPTS {
            return Err(Failure::Broken);
        }
        result = gather(algo, 4)? as u32;
    }
    Ok(result % umax)
}

/// `php_random_range64`: the same over eight gathered bytes.
fn range64<A: Algo + ?Sized>(algo: &mut A, umax: u64) -> Result<u64, Failure> {
    let mut result = gather(algo, 8)?;
    if umax == u64::MAX {
        return Ok(result);
    }
    let umax = umax + 1;
    if umax.is_power_of_two() {
        return Ok(result & (umax - 1));
    }
    let limit = u64::MAX - (u64::MAX % umax) - 1;
    let mut count = 0;
    while result > limit {
        count += 1;
        if count > RANGE_ATTEMPTS {
            return Err(Failure::Broken);
        }
        result = gather(algo, 8)?;
    }
    Ok(result % umax)
}

/// `php_random_range`: 32 bits where the span fits them, 64 where not.
fn range<A: Algo + ?Sized>(algo: &mut A, min: i64, max: i64) -> Result<i64, Failure> {
    let umax = max.cast_unsigned().wrapping_sub(min.cast_unsigned());
    let drawn = match u32::try_from(umax) {
        Ok(umax) => u64::from(range32(algo, umax)?),
        Err(_) => range64(algo, umax)?,
    };
    Ok(drawn.wrapping_add(min.cast_unsigned()).cast_signed())
}

/// `php_random_bin2hex_le`: the bytes, first to last, as lowercase hex.
fn bin2hex(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 15)]])
        .collect()
}

/// `php_random_hex2bin_le`: two hex digits a byte into `out`, `false`
/// at the first that is not one.
fn hex2bin(hex: &[u8], out: &mut [u8]) -> bool {
    let digit = |c: u8| (c as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    for (at, byte) in out.iter_mut().enumerate() {
        let (Some(hi), Some(lo)) = (
            hex.get(2 * at).copied().and_then(digit),
            hex.get(2 * at + 1).copied().and_then(digit),
        ) else {
            return false;
        };
        *byte = (hi << 4) | lo;
    }
    true
}

/// One word of a serialised state: element `at` a string of exactly
/// twice the word's width in hex, read into `out`.
fn read_word(data: PhpArray, at: usize, out: &mut [u8]) -> bool {
    let key = Key::Int(i64::try_from(at).unwrap_or(i64::MAX));
    match data.get(&key).and_then(|v| v.as_php_string()) {
        Some(s) if s.len() == 2 * out.len() => hex2bin(s.as_bytes(), out),
        _ => false,
    }
}

/// Element `at` of a serialised state when it is an int, `IS_LONG`.
fn read_long(data: PhpArray, at: usize) -> Option<i64> {
    let key = Key::Int(i64::try_from(at).ok()?);
    data.get(&key)?.as_int()
}

/// `php_random_generate_fallback_seed`, what php seeds from when the
/// CSPRNG fails: SHA-1 over the clock, the pids and the hostname there,
/// the clock and the pid here. The CSPRNG does not fail on a host this
/// engine builds for, and no program can hold either to a value.
fn fallback_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    #[allow(clippy::cast_possible_truncation)]
    let nanos = nanos as u64;
    nanos ^ (u64::from(std::process::id()) << 32)
}

/// `ZEND_BEGIN_MODULE_GLOBALS(random)`, the request state (ADR-0005): the
/// combined LCG's two states and the default engine's, each `None`
/// until first drawn from, which is what `combined_lcg_seeded` and
/// `mt19937_seeded` say and what `RINIT` resets. The fallback seed's
/// state is left out with the fallback it serves.
#[derive(Default)]
struct Globals {
    combined_lcg: Option<[i32; 2]>,
    mt19937: Option<StateMt19937>,
}

/// `php_random_default_status()`'s state, seeded as php seeds it on the
/// first draw of a request, and `f` over it.
fn with_default<R>(req: &mut Request, f: impl FnOnce(&mut StateMt19937) -> R) -> R {
    let state = req
        .state::<Globals>()
        .mt19937
        .get_or_insert_with(StateMt19937::seeded_by_default);
    f(state)
}

/// `Z_RANDOM_ENGINE_P(object)->engine`: `f` over the state the object's
/// class carries, and `f` handed back for an object of a class declared
/// in PHP, which has none.
fn with_engine<R, F: FnOnce(&mut dyn Algo) -> R>(
    req: &mut Request,
    object: PhpObject,
    f: F,
) -> Result<R, F> {
    if let Some(state) = object.native_mut::<EngineMt19937>(req) {
        return Ok(f(&mut **state));
    }
    if let Some(state) = object.native_mut::<EnginePcgOneseq128XslRr64>(req) {
        return Ok(f(&mut **state));
    }
    if let Some(state) = object.native_mut::<EngineXoshiro256StarStar>(req) {
        return Ok(f(&mut **state));
    }
    if let Some(state) = object.native_mut::<StateSecure>(req) {
        return Ok(f(state));
    }
    Err(f)
}

/// [`with_engine`] for an engine with `algo->serialize`.
fn with_serial<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut dyn Serial) -> R,
) -> Option<R> {
    if let Some(state) = object.native_mut::<EngineMt19937>(req) {
        return Some(f(&mut **state));
    }
    if let Some(state) = object.native_mut::<EnginePcgOneseq128XslRr64>(req) {
        return Some(f(&mut **state));
    }
    if let Some(state) = object.native_mut::<EngineXoshiro256StarStar>(req) {
        return Some(f(&mut **state));
    }
    None
}

/// One draw from the object's engine, what a raise inside it left made
/// php's throwable.
fn draw<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut dyn Algo) -> Result<R, Failure>,
) -> Result<R, Thrown> {
    match Rng::of(object).with(req, f) {
        Ok(answer) => Ok(answer),
        Err(failure) => Err(failure.raise(req)),
    }
}

/// `ce->name` of the object's class.
fn class_name(req: &Request, object: PhpObject) -> String {
    req.symbols()
        .class_decl(object.class())
        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
        .unwrap_or_default()
}

/// `zend_std_get_properties`, duplicated: the object's properties as an
/// array with one more owner of each value.
fn members(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (key, value) in object.props(req)? {
        out.set(req, Owner::take(req, key), Owner::take(req, value))?;
    }
    Ok(out)
}

/// A serialised state as the array php builds, a word as
/// `php_random_bin2hex_le` writes it.
fn fields(req: &mut Request, fields: &[Field]) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for field in fields {
        let value = match field {
            Field::Word(bytes) => PhpString::new(req, &bin2hex(bytes))?.into_value(),
            Field::Long(n) => Owner::int(*n),
        };
        out.append(req, value)?;
    }
    Ok(out)
}

/// `object_properties_load` (`Z/Zend/zend_API.c:1745`) over a class of
/// this extension, every one `@strict-properties`: a key naming a
/// declared property written into its slot, a `readonly` one only while
/// it is uninitialised, and any other key php's `Error` for a dynamic
/// property, the load stopping at the first refusal.
fn properties_load(req: &mut Request, object: PhpObject, members: PhpArray) -> Result<(), Thrown> {
    let class = class_name(req, object);
    let ce = Throwables::of(req)?;
    let declared: Vec<(Vec<u8>, bool)> = req
        .symbols()
        .class_decl(object.class())
        .map(|decl| {
            decl.properties
                .iter()
                .filter(|p| p.is_instance)
                .map(|p| (p.name.0.to_vec(), p.is_readonly))
                .collect()
        })
        .unwrap_or_default();
    // `object_properties_load`'s walk (`Z/Zend/zend_API.c:1752`), no owner
    // of the table taken: the argument holds it while a property write
    // runs.
    walk::foreach(req, members, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = *row.value();
        let Key::Str(key) = key else {
            let Key::Int(h) = key else {
                return Ok(ControlFlow::Continue(()));
            };
            return Err(throw_error(
                req,
                ce.error,
                &format!("Cannot create dynamic property {class}::${h}"),
            ));
        };
        let bytes = key.as_bytes();
        let plain = match bytes.strip_prefix(b"\0") {
            Some(rest) => rest
                .iter()
                .position(|b| *b == 0)
                .map_or(bytes, |end| rest.get(end + 1..).unwrap_or_default()),
            None => bytes,
        };
        let Some((name, readonly)) = declared.iter().find(|(name, _)| name.as_slice() == plain)
        else {
            return Err(throw_error(
                req,
                ce.error,
                &format!(
                    "Cannot create dynamic property {class}::${}",
                    String::from_utf8_lossy(plain)
                ),
            ));
        };
        if *readonly && object.prop(req, name).is_some() {
            return Err(throw_error(
                req,
                ce.error,
                &format!(
                    "Cannot modify readonly property {class}::${}",
                    String::from_utf8_lossy(name)
                ),
            ));
        }
        let name = PhpString::new(req, name)?;
        let value = Owner::take(req, value);
        object.unserialize_prop(req, name, value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `zend_throw_exception_ex(NULL, 0, "Invalid serialization data for %s object", ...)`:
/// php's `Exception`, with what was pending as its `previous`, as
/// `zend_throw_exception_internal` chains it ([`throw_chained`]).
fn invalid_data(req: &mut Request, class: &str, pending: Option<Thrown>) -> Thrown {
    let message = format!("Invalid serialization data for {class} object");
    let ce = match Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    match pending {
        None => throw(req, ce.exception, &message),
        Some(thrown) => throw_chained(req, thrown, ce.exception, &message),
    }
}

/// The handlers of a seedable engine: `php_random_engine_common_clone_object`,
/// the state copied into the clone.
pub(crate) struct Copying<T>(PhantomData<fn() -> T>);

impl<T: Clone + Default + Any + Send> ObjectHandlers for Copying<T> {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_engine::<T>)
    }
}

fn clone_engine<T: Clone + Default + Any + Send>(
    req: &mut Request,
    object: PhpObject,
) -> Result<Owner<PhpObject>, Thrown> {
    let state = object.native::<T>(req).cloned().unwrap_or_default();
    clone_with(req, &object, state)
}

pub(crate) static MT19937: Copying<EngineMt19937> = Copying(PhantomData);
pub(crate) static PCG: Copying<EnginePcgOneseq128XslRr64> = Copying(PhantomData);
pub(crate) static XOSHIRO: Copying<EngineXoshiro256StarStar> = Copying(PhantomData);

/// The handlers of `Secure` and `Randomizer`, whose `clone_obj` php
/// leaves null: an uncloneable object.
pub(crate) const UNCLONEABLE: Refusals = Refusals::NONE.cloning();

/// `MT_RAND_MT19937` and the deprecated `MT_RAND_PHP`, the two modes of
/// `mt_srand` and `Mt19937`, which [`Mode`] is.
#[php_const]
mod constants {
    pub(crate) const MT_RAND_MT19937: i64 = 0;
    #[php(deprecated(
        since = "8.3",
        message = "as it uses a biased non-standard variant of Mt19937"
    ))]
    pub(crate) const MT_RAND_PHP: i64 = 1;
}

/// `interface Random\Engine`, what a Randomizer draws from: one
/// `generate(): string`.
///
/// ```php
/// class E implements Random\Engine { function generate(): string { return "\x2a"; } }
/// var_dump((new Random\Randomizer(new E))->getInt(0, 255), (new ReflectionMethod('Random\Engine', 'generate'))->isAbstract());
/// ```
/// ```expect
/// int(42)
/// bool(true)
/// ```
#[php_class(interface, namespace(Random))]
pub(crate) struct Engine;

#[php_class]
impl Engine {
    fn generate(this: This<Self>) -> PhpString;
}

/// `interface Random\CryptoSafeEngine extends Engine {}`, the mark of
/// an engine fit for secrets, `Secure`'s.
///
/// ```php
/// var_dump(new Random\Engine\Secure instanceof Random\CryptoSafeEngine, class_implements('Random\CryptoSafeEngine'));
/// ```
/// ```expect
/// bool(true)
/// array(1) {
///   ["Random\Engine"]=>
///   string(13) "Random\Engine"
/// }
/// ```
#[php_class(interface, namespace(Random), extends(Engine))]
pub(crate) struct CryptoSafeEngine;

#[php_class]
impl CryptoSafeEngine {}

/// Core's `Error`, `Random\RandomError`'s parent.
#[php_class(extern)]
pub(crate) struct Error;

/// Core's `Exception`, `Random\RandomException`'s parent.
#[php_class(extern)]
pub(crate) struct Exception;

/// `class Random\RandomError extends \Error`, `@strict-properties`.
///
/// ```php
/// var_dump(get_parent_class('Random\RandomError'), get_parent_class('Random\BrokenRandomEngineError'), get_parent_class('Random\RandomException'));
/// ```
/// ```expect
/// string(5) "Error"
/// string(18) "Random\RandomError"
/// string(9) "Exception"
/// ```
#[php_class(namespace(Random), extends(Error), dynamic_properties = forbidden)]
pub(crate) struct RandomError;

#[php_class]
impl RandomError {}

/// `class Random\BrokenRandomEngineError extends RandomError`,
/// `@strict-properties`.
#[php_class(namespace(Random), extends(RandomError), dynamic_properties = forbidden)]
pub(crate) struct BrokenRandomEngineError;

#[php_class]
impl BrokenRandomEngineError {}

/// `class Random\RandomException extends \Exception`,
/// `@strict-properties`.
#[php_class(namespace(Random), extends(Exception), dynamic_properties = forbidden)]
pub(crate) struct RandomException;

#[php_class]
impl RandomException {}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('random'), MT_RAND_MT19937, mt_getrandmax(), getrandmax());
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// int(2147483647)
/// int(2147483647)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .class::<Engine>()
        .class::<CryptoSafeEngine>()
        .class::<RandomError>()
        .class::<BrokenRandomEngineError>()
        .class::<RandomException>()
        .class::<Mt19937>()
        .class::<PcgOneseq128XslRr64>()
        .class::<Xoshiro256StarStar>()
        .class::<Secure>()
        .class::<Randomizer>()
        .class::<IntervalBoundary>()
        .constants(constants::declared())
        .functions(&[
            lcg_value::DECLARED,
            mt_srand::DECLARED,
            srand::DECLARED,
            rand::DECLARED,
            mt_rand::DECLARED,
            mt_getrandmax::DECLARED,
            getrandmax::DECLARED,
            random_bytes::DECLARED,
            random_int::DECLARED,
        ])
        .request_state::<Globals>()
        .engine_state(throwables::Throwables::start)
        .build("random")
}

/// `lcg_value(): float`, deprecated since 8.4: `php_combined_lcg`, the
/// two L'Ecuyer generators seeded from eight CSPRNG bytes on the
/// request's first call and combined into `(0, 1)`.
///
/// ```php
/// $v = lcg_value(); var_dump($v > 0 && $v < 1);
/// ```
/// ```expect
///
/// Deprecated: Function lcg_value() is deprecated since 8.4, use \Random\Randomizer::getFloat() instead in %s on line 2
/// bool(true)
/// ```
#[php_function(deprecated(
    since = "8.4",
    message = "use \\Random\\Randomizer::getFloat() instead"
))]
fn lcg_value(req: &mut Request) -> f64 {
    let globals = req.state::<Globals>();
    let state = globals.combined_lcg.get_or_insert_with(|| {
        let seed = csprng::word().unwrap_or_else(|_| fallback_seed());
        #[allow(clippy::cast_possible_truncation)]
        let halves = [seed as i32, (seed >> 32) as i32];
        halves
    });
    // `PHP_COMBINED_LCG_MODMULT(a, b, c, m, s)`.
    let modmult = |a: i32, b: i32, c: i32, m: i32, s: &mut i32| {
        let q = *s / a;
        *s = b
            .wrapping_mul(s.wrapping_sub(a.wrapping_mul(q)))
            .wrapping_sub(c.wrapping_mul(q));
        if *s < 0 {
            *s = s.wrapping_add(m);
        }
    };
    modmult(53668, 40014, 12211, 2_147_483_563, &mut state[0]);
    modmult(52774, 40692, 3791, 2_147_483_399, &mut state[1]);
    let mut z = state[0].wrapping_sub(state[1]);
    if z < 1 {
        z = z.wrapping_add(2_147_483_562);
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let answer = (i64::from(z) as u64) as f64 * 4.656_613e-10;
    answer
}

/// `mt_srand(?int $seed = null, int $mode = MT_RAND_MT19937): void` and
/// its alias `srand`: the default engine reseeded, from the seed's low 32
/// bits or four CSPRNG bytes, in `MT_RAND_PHP` (deprecated) or else the
/// reference variant, as `PHP_FUNCTION(mt_srand)` takes any other `$mode`.
///
/// ```php
/// mt_srand(42); echo mt_rand(), " ", mt_rand(), " ", rand(), "\n";
/// srand(42); echo mt_rand(), "\n";
/// mt_srand(42, 99); echo mt_rand(), "\n";
/// mt_srand(42, MT_RAND_PHP); echo mt_rand(), " ", mt_rand(1, 100), "\n";
/// ```
/// ```expect
/// 804318771 1710563033 2041643438
/// 804318771
/// 804318771
///
/// Deprecated: Constant MT_RAND_PHP is deprecated since 8.3, as it uses a biased non-standard variant of Mt19937 in %s on line 5
///
/// Deprecated: The MT_RAND_PHP variant of Mt19937 is deprecated in %s on line 5
/// 1354439493 80
/// ```
#[php_function(aliases(srand))]
fn mt_srand(
    req: &mut Request,
    #[php(default = null)] seed: Option<i64>,
    #[php(default = MT_RAND_MT19937)] mode: i64,
) {
    let mode = if mode == constants::MT_RAND_PHP {
        deprecated(req, "The MT_RAND_PHP variant of Mt19937 is deprecated");
        Mode::Php
    } else {
        Mode::Mt19937
    };
    #[allow(clippy::cast_possible_truncation)]
    let seed = seed.map_or_else(engine_mt19937::default_seed, |seed| {
        seed.cast_unsigned() as u32
    });
    let state = StateMt19937::seeded(seed, mode);
    req.state::<Globals>().mt19937 = Some(state);
}

/// `ZEND_PARSE_PARAMETERS_START(2, 2)` after the zero-argument form of
/// `mt_rand` and `rand`, which php takes before it: `None` for no
/// argument, and otherwise both held as `Z_PARAM_LONG` holds them.
fn min_max(
    req: &mut Request,
    min: Passed<Raw<i64>>,
    max: Passed<Raw<i64>>,
) -> Result<Option<(i64, i64)>, Thrown> {
    let given = req.frames().argument_count();
    if given == 0 {
        return Ok(None);
    }
    if given != 2 {
        return Err(arg_count_error(
            req,
            &format!("exactly 2 arguments, {given} given"),
        ));
    }
    let mut held = |passed: Passed<Raw<i64>>| match passed {
        Passed::Given(raw) => raw.hold(req),
        Passed::Omitted => Ok(0),
    };
    let min = held(min)?;
    Ok(Some((min, held(max)?)))
}

/// `php_mt_rand_common`: the engine's range in the reference variant, and
/// `RAND_RANGE_BADSCALING` over one word in `MT_RAND_PHP`.
fn mt_rand_common(req: &mut Request, min: i64, max: i64) -> Result<i64, Thrown> {
    let drawn = with_default(req, |mt| match mt.mode {
        Mode::Mt19937 => mt.range(min, max),
        Mode::Php => Ok(mt.bad_scaling(min, max)),
    });
    drawn.map_err(|failure| failure.raise(req))
}

/// `mt_rand(int $min = UNKNOWN, int $max = UNKNOWN): int`: one word
/// shifted right, or a number in `[min, max]`, `max` below `min` php's
/// `ValueError`.
///
/// ```php
/// mt_srand(3); var_dump(mt_rand(), mt_rand(10, 20), mt_rand(PHP_INT_MIN, PHP_INT_MAX));
/// try { mt_rand(5, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mt_rand(5); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1182829493)
/// int(18)
/// int(6270952984516570361)
/// mt_rand(): Argument #2 ($max) must be greater than or equal to argument #1 ($min)
/// mt_rand() expects exactly 2 arguments, 1 given
/// ```
#[php_function(arity = manual)]
fn mt_rand(req: &mut Request, min: Passed<Raw<i64>>, max: Passed<Raw<i64>>) -> Result<i64, Thrown> {
    let Some((min, max)) = min_max(req, min, max)? else {
        let word = with_default(req, StateMt19937::next);
        return Ok(i64::from(word >> 1));
    };
    if max < min {
        return Err(arg_value_error(
            req,
            2,
            "be greater than or equal to argument #1 ($min)",
        ));
    }
    mt_rand_common(req, min, max)
}

/// `rand(int $min = UNKNOWN, int $max = UNKNOWN): int`: `mt_rand`, with
/// `max` below `min` taken as the range the other way round.
///
/// ```php
/// mt_srand(3); var_dump(rand(), rand(20, 10));
/// ```
/// ```expect
/// int(1182829493)
/// int(18)
/// ```
#[php_function(arity = manual)]
fn rand(req: &mut Request, min: Passed<Raw<i64>>, max: Passed<Raw<i64>>) -> Result<i64, Thrown> {
    let Some((min, max)) = min_max(req, min, max)? else {
        let word = with_default(req, StateMt19937::next);
        return Ok(i64::from(word >> 1));
    };
    if max < min {
        mt_rand_common(req, max, min)
    } else {
        mt_rand_common(req, min, max)
    }
}

/// `mt_getrandmax(): int` and its alias `getrandmax`:
/// `PHP_MT_RAND_MAX`.
///
/// ```php
/// var_dump(mt_getrandmax());
/// ```
/// ```expect
/// int(2147483647)
/// ```
#[php_function(aliases(getrandmax))]
fn mt_getrandmax() -> i64 {
    MT_RAND_MAX
}

/// `random_bytes(int $length): string`: `length` bytes of the host's
/// CSPRNG, a length below one php's `ValueError`.
///
/// ```php
/// var_dump(strlen(random_bytes(1)), strlen(random_bytes(33)));
/// try { random_bytes(0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { random_bytes(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(33)
/// random_bytes(): Argument #1 ($length) must be greater than 0
/// random_bytes() expects exactly 1 argument, 0 given
/// ```
#[php_function]
fn random_bytes(req: &mut Request, length: i64) -> Result<Owner<PhpString>, Thrown> {
    let size = length;
    if size < 1 {
        return Err(arg_value_error(req, 1, "be greater than 0"));
    }
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    let mut filled = Ok(());
    let bytes = PhpString::filled(req, size, |buf| filled = csprng::fill(buf))?;
    if let Err(failure) = filled {
        bytes.give_up(req);
        return Err(failure.raise(req));
    }
    Ok(bytes)
}

/// `random_int(int $min, int $max): int`: `php_random_int` over the
/// host's CSPRNG, `min` above `max` php's `ValueError`.
///
/// ```php
/// var_dump(random_int(7, 7), random_int(-1, -1));
/// $n = random_int(1, 6); var_dump($n >= 1 && $n <= 6);
/// try { random_int(2, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(7)
/// int(-1)
/// bool(true)
/// random_int(): Argument #1 ($min) must be less than or equal to argument #2 ($max)
/// ```
#[php_function]
fn random_int(req: &mut Request, min: i64, max: i64) -> Result<i64, Thrown> {
    if min > max {
        return Err(arg_value_error(
            req,
            1,
            "be less than or equal to argument #2 ($max)",
        ));
    }
    csprng::int(min, max).map_err(|failure| failure.raise(req))
}
