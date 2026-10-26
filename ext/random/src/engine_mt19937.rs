//! `engine_mt19937.c`: the Mersenne Twister, in php's two modes, as the
//! state `php_random_status_state_mt19937` holds it, and the
//! `Random\Engine\Mt19937` class over it. The class's `generate`,
//! `__serialize`, `__unserialize` and `__debugInfo` are the ones the other
//! engines alias (`@implementation-alias`), so they live in `lib.rs` beside
//! the dispatch over every engine's state.

use php_ext::abi::This;
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, arg_value_error, deprecated, throw};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpString};

use crate::throwables::Throwables;
use crate::{Algo, Failure, Field, Generated, Serial, constants, csprng};

/// `php_random_mt19937_seed_default`'s seed: four bytes of the host's
/// CSPRNG, or the fallback seed where it fails, as
/// `php_random_bytes_silent` falls back to
/// `php_random_generate_fallback_seed`.
pub(super) fn default_seed() -> u32 {
    let mut seed = [0; 4];
    match csprng::fill(&mut seed) {
        Ok(()) => u32::from_ne_bytes(seed),
        #[allow(clippy::cast_possible_truncation)]
        Err(_) => crate::fallback_seed() as u32,
    }
}

/// The length of the state vector.
const N: usize = 624;
/// The period parameter.
const M: usize = 397;

/// `enum php_random_mt19937_mode`: `MT_RAND_MT19937`, the reference
/// twister, or `MT_RAND_PHP`, php 5's twist that reads the wrong bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mode {
    #[default]
    Mt19937,
    Php,
}

impl Mode {
    /// The variant's number, what the serialised state carries.
    fn number(self) -> i64 {
        match self {
            Mode::Mt19937 => constants::MT_RAND_MT19937,
            Mode::Php => constants::MT_RAND_PHP,
        }
    }
}

/// `php_random_status_state_mt19937`, field for field: where the next
/// word is read, the variant, and the state vector. `Default` is php's
/// `pecalloc`, every field zero, which is the state an object has before
/// its constructor runs.
#[derive(Debug, Clone)]
pub(crate) struct StateMt19937 {
    pub(crate) count: u32,
    pub(crate) mode: Mode,
    pub(crate) state: [u32; N],
}

/// The payload, `engine.state`: the pointer to the state php allocates
/// beside the object (`php_random_status_alloc`).
pub(crate) type EngineMt19937 = Box<StateMt19937>;

impl Default for StateMt19937 {
    fn default() -> StateMt19937 {
        StateMt19937 {
            count: 0,
            mode: Mode::Mt19937,
            state: [0; N],
        }
    }
}

/// `mixBits(u, v)`: the high bit of `u` over the low bits of `v`.
fn mix(u: u32, v: u32) -> u32 {
    (u & 0x8000_0000) | (v & 0x7FFF_FFFF)
}

/// `twist(m, u, v)` reading the low bit of `v`, or `twist_php` reading
/// the low bit of `u`, as the variant says.
fn twist(mode: Mode, m: u32, u: u32, v: u32) -> u32 {
    let bit = match mode {
        Mode::Mt19937 => v & 1,
        Mode::Php => u & 1,
    };
    m ^ (mix(u, v) >> 1) ^ (bit.wrapping_neg() & 0x9908_B0DF)
}

impl StateMt19937 {
    /// A state seeded with `seed` in `mode`, what `mt_srand` and the
    /// constructor leave.
    pub(crate) fn seeded(seed: u32, mode: Mode) -> StateMt19937 {
        let mut mt = StateMt19937 {
            mode,
            ..StateMt19937::default()
        };
        mt.seed32(seed);
        mt
    }

    /// `php_random_mt19937_seed_default` in the reference variant, what
    /// the request's first draw seeds the default engine with.
    pub(crate) fn seeded_by_default() -> StateMt19937 {
        StateMt19937::seeded(default_seed(), Mode::Mt19937)
    }

    /// `mt19937_reload`: the next 624 words, the count back at zero.
    fn reload(&mut self) {
        let mode = self.mode;
        let s = &mut self.state;
        for i in 0..N - M {
            s[i] = twist(mode, s[i + M], s[i], s[i + 1]);
        }
        for i in N - M..N - 1 {
            s[i] = twist(mode, s[i + M - N], s[i], s[i + 1]);
        }
        s[N - 1] = twist(mode, s[M - 1], s[N - 1], s[0]);
        self.count = 0;
    }

    /// `php_random_mt19937_seed32`: Knuth's initialisation of the vector
    /// from `seed`, then one reload.
    pub(crate) fn seed32(&mut self, seed: u32) {
        self.state[0] = seed;
        for i in 1..N {
            let prev = self.state[i - 1];
            #[allow(clippy::cast_possible_truncation)]
            let i = i as u32;
            self.state[i as usize] = 1_812_433_253_u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(i);
        }
        self.reload();
    }

    /// The tempered next word, what `generate` wraps.
    pub(crate) fn next(&mut self) -> u32 {
        if self.count as usize >= N {
            self.reload();
        }
        let mut s1 = self.state[self.count as usize];
        self.count += 1;
        s1 ^= s1 >> 11;
        s1 ^= (s1 << 7) & 0x9D2C_5680;
        s1 ^= (s1 << 15) & 0xEFC6_0000;
        s1 ^ (s1 >> 18)
    }

    /// `RAND_RANGE_BADSCALING` over one word shifted right, what
    /// `MT_RAND_PHP` answers a range with in `mt_rand` and
    /// `Randomizer::getInt` (`php_mt_rand_common`).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(crate) fn bad_scaling(&mut self, min: i64, max: i64) -> i64 {
        let r = u64::from(self.next() >> 1);
        let offset =
            ((max as f64) - (min as f64) + 1.0) * (r as f64 / (crate::MT_RAND_MAX as f64 + 1.0));
        (offset as u64)
            .wrapping_add(min.cast_unsigned())
            .cast_signed()
    }
}

impl Algo for StateMt19937 {
    fn generate(&mut self) -> Result<Generated, Failure> {
        Ok(Generated {
            result: u64::from(self.next()),
            size: 4,
        })
    }

    fn as_mt19937(&mut self) -> Option<&mut StateMt19937> {
        Some(self)
    }
}

impl Serial for StateMt19937 {
    /// The 624 words as little-endian hex, then the count and the variant.
    fn serialize(&self) -> Vec<Field> {
        let mut fields: Vec<Field> = self
            .state
            .iter()
            .map(|word| Field::Word(word.to_le_bytes().to_vec()))
            .collect();
        fields.push(Field::Long(i64::from(self.count)));
        fields.push(Field::Long(self.mode.number()));
        fields
    }

    /// php's checks in its order, each word written as it is read, so a
    /// state refused part way is left as php leaves it.
    fn unserialize(&mut self, data: PhpArray) -> bool {
        if data.len() != N + 2 {
            return false;
        }
        for i in 0..N {
            let mut word = [0; 4];
            if !crate::read_word(data, i, &mut word) {
                return false;
            }
            self.state[i] = u32::from_le_bytes(word);
        }
        let Some(count) = crate::read_long(data, N) else {
            return false;
        };
        // `s->count = Z_LVAL_P(t)`: the long as a `uint32_t`, written
        // before php compares it.
        #[allow(clippy::cast_possible_truncation)]
        let count = count.cast_unsigned() as u32;
        self.count = count;
        if count as usize > N {
            return false;
        }
        self.mode = match crate::read_long(data, N + 1) {
            Some(0) => Mode::Mt19937,
            Some(1) => Mode::Php,
            _ => return false,
        };
        true
    }
}

/// `final class Random\Engine\Mt19937 implements \Random\Engine`,
/// `@strict-properties`, over [`StateMt19937`]. Its `generate`,
/// `__serialize`, `__unserialize` and `__debugInfo` are the entries the
/// other engines name (`@implementation-alias`), dispatching on the
/// state the receiver's class carries.
///
/// `php_random_engine_common_init`: the engine by `zend_object_alloc`
/// and its state beside it, which `memory_get_usage()` counts (design
/// §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['Random\\Engine\\Mt19937', 'Random\\Engine\\PcgOneseq128XslRr64', 'Random\\Engine\\Xoshiro256StarStar', 'Random\\Engine\\Secure'] as $c) {
///     $m = memory_get_usage();
///     $e = new $c;
///     $used = memory_get_usage() - $m;
///     $e = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// $s = new Random\Engine\Secure;
/// new Random\Randomizer($s);
/// $m = memory_get_usage();
/// $r = new Random\Randomizer($s);
/// echo 'Random\Randomizer ', memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// Random\Engine\Mt19937 2640
/// Random\Engine\PcgOneseq128XslRr64 96
/// Random\Engine\Xoshiro256StarStar 112
/// Random\Engine\Secure 56
/// Random\Randomizer 80
/// ```
#[php_class(
    final,
    namespace(Random::Engine),
    implements(crate::Engine),
    dynamic_properties = forbidden,
    size = 72, beside(2504), payload = EngineMt19937,
    handlers = crate::MT19937
)]
pub(crate) struct Mt19937;

#[php_class]
impl Mt19937 {
    /// `Random\Engine\Mt19937::__construct(?int $seed = null, int $mode =
    /// MT_RAND_MT19937)`: the mode checked, `MT_RAND_PHP` deprecated, and the
    /// state seeded with the seed's low 32 bits, eight CSPRNG bytes where
    /// none is given.
    ///
    /// ```php
    /// $e = new Random\Engine\Mt19937(1234);
    /// echo bin2hex($e->generate()), " ", bin2hex($e->generate()), "\n";
    /// $p = new Random\Engine\Mt19937(1234, MT_RAND_PHP);
    /// echo bin2hex($p->generate()), "\n";
    /// echo strlen((new Random\Engine\Mt19937())->generate()), "\n";
    /// echo bin2hex((new Random\Engine\Mt19937(PHP_INT_MAX))->generate()), "\n";
    /// try { new Random\Engine\Mt19937(1, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 2f6b0731 d3e2667f
    ///
    /// Deprecated: Constant MT_RAND_PHP is deprecated since 8.3, as it uses a biased non-standard variant of Mt19937 in %s on line 4
    ///
    /// Deprecated: The MT_RAND_PHP variant of Mt19937 is deprecated in %s on line 4
    /// c38190cf
    /// 4
    /// a369fe18
    /// Random\Engine\Mt19937::__construct(): Argument #2 ($mode) must be either MT_RAND_MT19937 or MT_RAND_PHP
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] seed: Option<i64>,
        #[php(default = MT_RAND_MT19937)] mode: i64,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let mode = match mode {
            constants::MT_RAND_MT19937 => Mode::Mt19937,
            constants::MT_RAND_PHP => {
                deprecated(req, "The MT_RAND_PHP variant of Mt19937 is deprecated");
                Mode::Php
            }
            _ => {
                return Err(arg_value_error(
                    req,
                    2,
                    "be either MT_RAND_MT19937 or MT_RAND_PHP",
                ));
            }
        };
        let seed = match seed {
            Some(seed) => seed,
            None => csprng::word()
                .map_err(|failure| failure.seed_failure(req))?
                .cast_signed(),
        };
        #[allow(clippy::cast_possible_truncation)]
        let seeded = StateMt19937::seeded(seed.cast_unsigned() as u32, mode);
        if let Some(state) = this.native_mut::<EngineMt19937>(req) {
            **state = seeded;
        }
        Ok(())
    }

    /// `Random\Engine\Mt19937::generate(): string`, which every native
    /// engine aliases: one draw of the object's engine as its `size` bytes,
    /// least significant first.
    ///
    /// ```php
    /// $e = new Random\Engine\Mt19937(0);
    /// echo bin2hex($e->generate()), " ", bin2hex($e->generate()), "\n";
    /// $c = clone $e; var_dump($c->generate() === $e->generate());
    /// ```
    /// ```expect
    /// ac0a7f8c 2faac497
    /// bool(true)
    /// ```
    fn generate(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = this.object();
        let drawn = crate::draw(req, this, |algo| algo.generate())?;
        let bytes = drawn.result.to_le_bytes();
        let bytes = bytes.get(..drawn.size.min(8)).unwrap_or_default();
        PhpString::new(req, bytes)
    }

    /// `Random\Engine\Mt19937::__serialize(): array`, which the other
    /// seedable engines alias: `[members, state]`, the state as
    /// `algo->serialize` writes it.
    ///
    /// ```php
    /// $s = serialize(new Random\Engine\Mt19937(1)); echo strlen($s), " ", substr($s, 0, 60), "\n";
    /// $e = unserialize($s); echo bin2hex($e->generate()), "\n";
    /// ```
    /// ```expect
    /// 13069 O:21:"Random\Engine\Mt19937":2:{i:0;a:0:{}i:1;a:626:{i:0;s:8
    /// 25f4c16a
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let Some(fields) = crate::with_serial(req, this, |s| s.serialize()) else {
            let ce = Throwables::of(req)?;
            return Err(throw(req, ce.exception, "Engine serialize failed"));
        };
        let mut out = PhpArray::new(req)?;
        let members = crate::members(req, this)?;
        out.append(req, members.into_value())?;
        let state = crate::fields(req, &fields)?;
        out.append(req, state.into_value())?;
        Ok(out)
    }

    /// `Random\Engine\Mt19937::__unserialize(array $data): void`, which the
    /// other seedable engines alias: two elements, the members loaded as
    /// `object_properties_load` loads them and the state as
    /// `algo->unserialize` reads it, anything else php's `Exception`.
    ///
    /// ```php
    /// $e = new Random\Engine\Mt19937(1);
    /// foreach ([[], [[], []], [[], array_fill(0, 626, "00000000")], [['x' => 1], []]] as $d) {
    ///     try { $e->__unserialize($d); echo "ok\n"; } catch (Exception $x) { echo $x->getMessage(), " / ", ($x->getPrevious() ? $x->getPrevious()->getMessage() : "-"), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Invalid serialization data for Random\Engine\Mt19937 object / -
    /// Invalid serialization data for Random\Engine\Mt19937 object / -
    /// Invalid serialization data for Random\Engine\Mt19937 object / -
    /// Invalid serialization data for Random\Engine\Mt19937 object / Cannot create dynamic property Random\Engine\Mt19937::$x
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let this = this.object();
        let invalid = |req: &mut Request, pending: Option<Thrown>| {
            let class = crate::class_name(req, this);
            crate::invalid_data(req, &class, pending)
        };
        let part = |at: i64| data.get(&Key::Int(at)).and_then(|v| v.as_array());
        if data.len() != 2 {
            return Err(invalid(req, None));
        }
        let Some(members) = part(0) else {
            return Err(invalid(req, None));
        };
        if let Err(thrown) = crate::properties_load(req, this, members) {
            return Err(invalid(req, Some(thrown)));
        }
        let Some(state) = part(1) else {
            return Err(invalid(req, None));
        };
        if crate::with_serial(req, this, |s| s.unserialize(state)) != Some(true) {
            return Err(invalid(req, None));
        }
        Ok(())
    }

    /// `Random\Engine\Mt19937::__debugInfo(): array`, which the other
    /// seedable engines alias: the members with the serialised state under
    /// `__states`.
    ///
    /// ```php
    /// $d = (new Random\Engine\Xoshiro256StarStar(1))->__debugInfo(); var_dump($d);
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["__states"]=>
    ///   array(4) {
    ///     [0]=>
    ///     string(16) "c15c0289ec2d0a91"
    ///     [1]=>
    ///     string(16) "67ec8e65a18debbe"
    ///     [2]=>
    ///     string(16) "5e5532fbeea293f8"
    ///     [3]=>
    ///     string(16) "0bc942ee9086c171"
    ///   }
    /// }
    /// ```
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let mut out = crate::members(req, this)?;
        if let Some(fields) = crate::with_serial(req, this, |s| s.serialize()) {
            let state = crate::fields(req, &fields)?;
            out.set_str(req, b"__states", state.into_value())?;
        }
        Ok(out)
    }
}
