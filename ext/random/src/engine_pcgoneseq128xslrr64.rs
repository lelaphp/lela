//! `engine_pcgoneseq128xslrr64.c`: O'Neill's PCG with a 128-bit state,
//! one sequence, and the XSL-RR permutation to 64 bits, as
//! `php_random_status_state_pcgoneseq128xslrr64` holds it, and the
//! `Random\Engine\PcgOneseq128XslRr64` class over it.

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpArray, Value};

use php_ext::decl::{MethodDecl, Type};

use crate::arginfo::{method, param, wide_seed};
use crate::engine_mt19937::{generate_method, serial_methods};
use crate::{Algo, Entry, Failure, Field, Generated, Serial, csprng, zpp};

/// The LCG's multiplier, `php_random_uint128_constant(2549297995355413924,
/// 4865540595714422341)`.
const MULTIPLIER: u128 = 0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645;
/// The LCG's increment, `php_random_uint128_constant(6364136223846793005,
/// 1442695040888963407)`.
const INCREMENT: u128 = 0x5851_f42d_4c95_7f2d_1405_7b7e_f767_814f;

/// `php_random_uint128_t` as php spells it where the compiler has no
/// 128-bit integer: the high and the low half. The heap lays a payload
/// out at eight bytes' alignment, which a `u128` field would exceed, and
/// the halves are the order the serialised state writes them in.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Uint128 {
    pub(crate) hi: u64,
    pub(crate) lo: u64,
}

impl Uint128 {
    fn get(self) -> u128 {
        (u128::from(self.hi) << 64) | u128::from(self.lo)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn of(n: u128) -> Uint128 {
        Uint128 {
            hi: (n >> 64) as u64,
            lo: n as u64,
        }
    }
}

/// `php_random_status_state_pcgoneseq128xslrr64`: the one 128-bit state.
/// `Default` is php's zeroed allocation.
#[derive(Debug, Clone, Default)]
pub(crate) struct PcgOneseq128XslRr64 {
    pub(crate) state: Uint128,
}

impl PcgOneseq128XslRr64 {
    /// `step`: one turn of the LCG.
    fn step(&mut self) {
        self.state = Uint128::of(
            self.state
                .get()
                .wrapping_mul(MULTIPLIER)
                .wrapping_add(INCREMENT),
        );
    }

    /// `php_random_pcgoneseq128xslrr64_seed128`.
    fn seed128(&mut self, seed: u128) {
        self.state = Uint128::default();
        self.step();
        self.state = Uint128::of(self.state.get().wrapping_add(seed));
        self.step();
    }

    /// `php_random_pcgoneseq128xslrr64_advance`: the state `advance`
    /// steps on, by Brown's jump-ahead over the multiplier's powers.
    fn advance(&mut self, mut advance: u64) {
        let (mut cur_mult, mut cur_plus) = (MULTIPLIER, INCREMENT);
        let (mut acc_mult, mut acc_plus) = (1_u128, 0_u128);
        while advance > 0 {
            if advance & 1 == 1 {
                acc_mult = acc_mult.wrapping_mul(cur_mult);
                acc_plus = acc_plus.wrapping_mul(cur_mult).wrapping_add(cur_plus);
            }
            cur_plus = cur_mult.wrapping_add(1).wrapping_mul(cur_plus);
            cur_mult = cur_mult.wrapping_mul(cur_mult);
            advance /= 2;
        }
        self.state = Uint128::of(
            acc_mult
                .wrapping_mul(self.state.get())
                .wrapping_add(acc_plus),
        );
    }
}

impl Algo for PcgOneseq128XslRr64 {
    fn generate(&mut self) -> Result<Generated, Failure> {
        self.step();
        // `php_random_pcgoneseq128xslrr64_rotr64`: the halves xored,
        // rotated right by the state's top six bits.
        let Uint128 { hi, lo } = self.state;
        #[allow(clippy::cast_possible_truncation)]
        let s = (hi >> 58) as u32;
        Ok(Generated {
            result: (hi ^ lo).rotate_right(s),
            size: 8,
        })
    }
}

impl Serial for PcgOneseq128XslRr64 {
    /// The high half, then the low one, each little-endian hex.
    fn serialize(&self) -> Vec<Field> {
        let Uint128 { hi, lo } = self.state;
        vec![
            Field::Word(hi.to_le_bytes().to_vec()),
            Field::Word(lo.to_le_bytes().to_vec()),
        ]
    }

    fn unserialize(&mut self, data: PhpArray) -> bool {
        if data.len() != 2 {
            return false;
        }
        let mut halves = [0_u64; 2];
        for (at, half) in halves.iter_mut().enumerate() {
            let mut word = [0; 8];
            if !crate::read_word(data, at, &mut word) {
                return false;
            }
            *half = u64::from_le_bytes(word);
        }
        self.state = Uint128 {
            hi: halves[0],
            lo: halves[1],
        };
        true
    }
}

/// `Random\Engine\PcgOneseq128XslRr64::__construct(string|int|null $seed
/// = null)`: sixteen bytes read as two little-endian halves, high first,
/// an int as the low half, and sixteen CSPRNG bytes where none is given.
///
/// ```php
/// echo bin2hex((new Random\Engine\PcgOneseq128XslRr64(1234))->generate()), "\n";
/// echo bin2hex((new Random\Engine\PcgOneseq128XslRr64("0123456789abcdef"))->generate()), "\n";
/// echo strlen((new Random\Engine\PcgOneseq128XslRr64())->generate()), "\n";
/// try { new Random\Engine\PcgOneseq128XslRr64("short"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// ecfbe5990a319380
/// 33ab451afc58b814
/// 8
/// Random\Engine\PcgOneseq128XslRr64::__construct(): Argument #1 ($seed) must be a 16 byte (128 bit) string
/// ```
pub(crate) fn construct(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 0, 1)?;
    let this = zpp::this(req)?;
    let seed = zpp::given(req, 1)?;
    let mut state = PcgOneseq128XslRr64::default();
    match seed {
        None => {
            let mut bytes = [0; 16];
            csprng::fill(&mut bytes).map_err(|failure| failure.seed_failure(req))?;
            state.seed128(u128::from_ne_bytes(bytes));
        }
        Some(seed) => {
            if let Some(bytes) = seed.as_str_bytes() {
                let Ok(bytes) = <[u8; 16]>::try_from(bytes) else {
                    return Err(arg_value_error(req, 1, "be a 16 byte (128 bit) string"));
                };
                let half = |at: usize| {
                    let mut word = [0; 8];
                    word.copy_from_slice(bytes.get(at..at + 8).unwrap_or(&[0; 8]));
                    u128::from(u64::from_le_bytes(word))
                };
                state.seed128((half(0) << 64) | half(8));
            } else if let Some(n) = seed.as_int() {
                state.seed128(u128::from(n.cast_unsigned()));
            } else {
                let mut bytes = [0; 16];
                csprng::fill(&mut bytes).map_err(|failure| failure.seed_failure(req))?;
                state.seed128(u128::from_ne_bytes(bytes));
            }
        }
    }
    if let Some(slot) = this.native_mut::<PcgOneseq128XslRr64>(req) {
        *slot = state;
    }
    Ok(Value::null())
}

/// `Random\Engine\PcgOneseq128XslRr64::jump(int $advance): void`: the
/// state `advance` steps on, a negative count php's `ValueError`.
///
/// ```php
/// $a = new Random\Engine\PcgOneseq128XslRr64(1); $b = clone $a;
/// for ($i = 0; $i < 1000; $i++) $a->generate();
/// $b->jump(1000); var_dump($a->generate() === $b->generate());
/// try { $b->jump(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// Random\Engine\PcgOneseq128XslRr64::jump(): Argument #1 ($advance) must be greater than or equal to 0
/// ```
pub(crate) fn jump(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 1, 1)?;
    let this = zpp::this(req)?;
    let advance = zpp::long(req, 1)?;
    if advance < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    if let Some(state) = this.native_mut::<PcgOneseq128XslRr64>(req) {
        state.advance(advance.cast_unsigned());
    }
    Ok(Value::null())
}

/// `Random\Engine\PcgOneseq128XslRr64`'s methods, `generate` and the
/// serial three aliases of `Mt19937`'s.
pub(super) fn methods() -> Vec<(MethodDecl, Entry)> {
    let mut methods = vec![
        (method("__construct", wide_seed(), None), construct as Entry),
        generate_method(),
        (
            method(
                "jump",
                vec![param("advance", Type::Int, None)],
                Some(Type::Void),
            ),
            jump,
        ),
    ];
    methods.extend(serial_methods());
    methods
}
