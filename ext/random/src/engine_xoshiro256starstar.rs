//! `engine_xoshiro256starstar.c`: Blackman and Vigna's xoshiro256**, as
//! `php_random_status_state_xoshiro256starstar` holds it, and the
//! `Random\Engine\Xoshiro256StarStar` class over it.

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpArray, Value};

use php_ext::decl::{MethodDecl, Type};

use crate::arginfo::{method, wide_seed};
use crate::engine_mt19937::{generate_method, serial_methods};
use crate::{Algo, Entry, Failure, Field, Generated, Serial, csprng, zpp};

/// The polynomial `jump()` advances by, 2^128 draws.
const JUMP: [u64; 4] = [
    0x180e_c6d3_3cfd_0aba,
    0xd5a6_1266_f0c9_392c,
    0xa958_2618_e03f_c9aa,
    0x39ab_dc45_29b1_661c,
];
/// The polynomial `jumpLong()` advances by, 2^192 draws.
const JUMP_LONG: [u64; 4] = [
    0x76e1_5d3e_fefd_cbbf,
    0xc500_4e44_1c52_2fb3,
    0x7771_0069_854e_e241,
    0x3910_9bb0_2acb_e635,
];

/// `php_random_status_state_xoshiro256starstar`: the four words.
/// `Default` is php's zeroed allocation.
#[derive(Debug, Clone, Default)]
pub(crate) struct Xoshiro256StarStar {
    pub(crate) state: [u64; 4],
}

/// `splitmix64`: the next word of Vigna's seeding generator.
fn splitmix64(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut r = *seed;
    r = (r ^ (r >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    r = (r ^ (r >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    r ^ (r >> 31)
}

impl Xoshiro256StarStar {
    /// `generate_state`: the scrambled word of the state as it stands,
    /// and the state one step on.
    fn step(&mut self) -> u64 {
        let s = &mut self.state;
        let r = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        r
    }

    /// `php_random_xoshiro256starstar_seed64`: four words of splitmix64.
    fn seed64(seed: u64) -> Xoshiro256StarStar {
        let mut seed = seed;
        let state = [
            splitmix64(&mut seed),
            splitmix64(&mut seed),
            splitmix64(&mut seed),
            splitmix64(&mut seed),
        ];
        Xoshiro256StarStar { state }
    }

    /// `jump`: the state the polynomial's bits select, xored as the
    /// state steps once per bit.
    fn jump(&mut self, polynomial: &[u64; 4]) {
        let mut acc = [0_u64; 4];
        for word in polynomial {
            for bit in 0..64 {
                if word & (1 << bit) != 0 {
                    for (a, s) in acc.iter_mut().zip(self.state) {
                        *a ^= s;
                    }
                }
                self.step();
            }
        }
        self.state = acc;
    }
}

impl Algo for Xoshiro256StarStar {
    fn generate(&mut self) -> Result<Generated, Failure> {
        Ok(Generated {
            result: self.step(),
            size: 8,
        })
    }
}

impl Serial for Xoshiro256StarStar {
    /// The four words, each little-endian hex.
    fn serialize(&self) -> Vec<Field> {
        self.state
            .iter()
            .map(|word| Field::Word(word.to_le_bytes().to_vec()))
            .collect()
    }

    /// Four words, the all-zero state refused as php refuses it.
    fn unserialize(&mut self, data: PhpArray) -> bool {
        if data.len() != 4 {
            return false;
        }
        for at in 0..4 {
            let mut word = [0; 8];
            if !crate::read_word(data, at, &mut word) {
                return false;
            }
            self.state[at] = u64::from_le_bytes(word);
        }
        self.state != [0; 4]
    }
}

/// Four words from 32 bytes, each little-endian.
fn words(bytes: &[u8; 32]) -> [u64; 4] {
    let mut out = [0; 4];
    for (at, word) in out.iter_mut().enumerate() {
        let mut b = [0; 8];
        b.copy_from_slice(bytes.get(at * 8..at * 8 + 8).unwrap_or(&[0; 8]));
        *word = u64::from_le_bytes(b);
    }
    out
}

/// `Random\Engine\Xoshiro256StarStar::__construct(string|int|null $seed =
/// null)`: 32 bytes read as four little-endian words, an int through
/// splitmix64, and 32 CSPRNG bytes, redrawn while all zero, where none
/// is given.
///
/// ```php
/// echo bin2hex((new Random\Engine\Xoshiro256StarStar(1234))->generate()), "\n";
/// echo bin2hex((new Random\Engine\Xoshiro256StarStar(str_repeat("\x01", 32)))->generate()), "\n";
/// echo strlen((new Random\Engine\Xoshiro256StarStar())->generate()), "\n";
/// try { new Random\Engine\Xoshiro256StarStar("short"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { new Random\Engine\Xoshiro256StarStar(str_repeat("\0", 32)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 53aee3a0d945ab0b
/// 9296969696969696
/// 8
/// Random\Engine\Xoshiro256StarStar::__construct(): Argument #1 ($seed) must be a 32 byte (256 bit) string
/// Random\Engine\Xoshiro256StarStar::__construct(): Argument #1 ($seed) must not consist entirely of NUL bytes
/// ```
pub(crate) fn construct(req: &mut Request) -> Result<Value, Thrown> {
    zpp::arity(req, 0, 1)?;
    let this = zpp::this(req)?;
    let seed = zpp::given(req, 1)?;
    let state = match seed
        .as_ref()
        .and_then(|s| s.as_str_bytes().map(<[u8]>::to_vec))
    {
        Some(bytes) => {
            let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice()) else {
                return Err(arg_value_error(req, 1, "be a 32 byte (256 bit) string"));
            };
            let state = words(&bytes);
            if state == [0; 4] {
                return Err(arg_value_error(req, 1, "not consist entirely of NUL bytes"));
            }
            Xoshiro256StarStar { state }
        }
        None => {
            if let Some(n) = seed.and_then(|s| s.as_int()) {
                Xoshiro256StarStar::seed64(n.cast_unsigned())
            } else {
                let mut state = [0; 4];
                while state == [0; 4] {
                    let mut bytes = [0; 32];
                    csprng::fill(&mut bytes).map_err(|failure| failure.seed_failure(req))?;
                    state = words(&bytes);
                }
                Xoshiro256StarStar { state }
            }
        }
    };
    if let Some(slot) = this.native_mut::<Xoshiro256StarStar>(req) {
        *slot = state;
    }
    Ok(Value::null())
}

/// `Random\Engine\Xoshiro256StarStar::jump(): void`: 2^128 draws on.
///
/// ```php
/// $e = new Random\Engine\Xoshiro256StarStar(1); $e->jump(); echo bin2hex($e->generate()), "\n";
/// ```
/// ```expect
/// d0e9aa1ef8022833
/// ```
pub(crate) fn jump(req: &mut Request) -> Result<Value, Thrown> {
    jump_by(req, &JUMP)
}

/// `Random\Engine\Xoshiro256StarStar::jumpLong(): void`: 2^192 draws on.
///
/// ```php
/// $e = new Random\Engine\Xoshiro256StarStar(1); $e->jumpLong(); echo bin2hex($e->generate()), "\n";
/// ```
/// ```expect
/// 0782204a459ef439
/// ```
pub(crate) fn jump_long(req: &mut Request) -> Result<Value, Thrown> {
    jump_by(req, &JUMP_LONG)
}

fn jump_by(req: &mut Request, polynomial: &[u64; 4]) -> Result<Value, Thrown> {
    zpp::arity(req, 0, 0)?;
    let this = zpp::this(req)?;
    if let Some(state) = this.native_mut::<Xoshiro256StarStar>(req) {
        state.jump(polynomial);
    }
    Ok(Value::null())
}

/// `Random\Engine\Xoshiro256StarStar`'s methods, `generate` and the
/// serial three aliases of `Mt19937`'s.
pub(super) fn methods() -> Vec<(MethodDecl, Entry)> {
    let mut methods = vec![
        (method("__construct", wide_seed(), None), construct as Entry),
        generate_method(),
        (method("jump", Vec::new(), Some(Type::Void)), jump),
        (method("jumpLong", Vec::new(), Some(Type::Void)), jump_long),
    ];
    methods.extend(serial_methods());
    methods
}
