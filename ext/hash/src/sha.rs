//! `hash_sha.c`'s 128-byte contexts, SHA-384, SHA-512 and its two
//! truncations, over the `sha2` crate's `compress512`; and `hash_sha3.c`
//! at the oracle's build, XKCP's sponge, over the `keccak` crate's
//! Keccak-f[1600].
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::slice;

use crate::algos::{Slot, State};
use crate::md::{PADDING, feed};

/// `PHP_SHA512_CTX`: eight words, a 128-bit bit count and the block,
/// `q8q2b128.`, under the initial words `V` names.
///
/// ```php
/// foreach (['sha384', 'sha512/224', 'sha512/256', 'sha512'] as $a) {
///     $c = hash_init($a); hash_update($c, str_repeat('ab', 80)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, 'abc'), ' ', hash_final($c), "\n";
/// }
/// ```
/// ```expect
/// sha384 cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7 6b57f621d82f7e4aa7b04e28a8880cf10d99659a89cdd6da148202632cc8384ecd0a631b1442993af7bb5ba3b016115e
/// sha512/224 4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa 64917d01a9c9e6ed8bbd5234172981b788e6845774875509c809e3ad
/// sha512/256 53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23 2c2b81e6601afa11936f2e8a50dc42b215b174cc59d865b9c8dc913ad24fd350
/// sha512 ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f cad29751e44c38d2de9f09487f3e839449891a41ffaaa5d5f69b530f5590a00eb0f65cce6844c73ce8580b9d882ecc29d57972d6c0b6f8b6255430d25532f3af
/// ```
#[derive(Clone)]
pub(crate) struct Sha512<const V: u8> {
    state: [u64; 8],
    count: [u64; 2],
    buffer: [u8; 128],
}

pub(crate) type Sha384 = Sha512<0>;
pub(crate) type Sha512t224 = Sha512<1>;
pub(crate) type Sha512t256 = Sha512<2>;
pub(crate) type Sha512Full = Sha512<3>;

impl<const V: u8> Default for Sha512<V> {
    fn default() -> Sha512<V> {
        const IV: [[u64; 8]; 4] = [
            [
                0xcbbb_9d5d_c105_9ed8,
                0x629a_292a_367c_d507,
                0x9159_015a_3070_dd17,
                0x152f_ecd8_f70e_5939,
                0x6733_2667_ffc0_0b31,
                0x8eb4_4a87_6858_1511,
                0xdb0c_2e0d_64f9_8fa7,
                0x47b5_481d_befa_4fa4,
            ],
            [
                0x8C3D_37C8_1954_4DA2,
                0x73E1_9966_89DC_D4D6,
                0x1DFA_B7AE_32FF_9C82,
                0x679D_D514_582F_9FCF,
                0x0F6D_2B69_7BD4_4DA8,
                0x77E3_6F73_04C4_8942,
                0x3F9D_85A8_6A1D_36C8,
                0x1112_E6AD_91D6_92A1,
            ],
            [
                0x2231_2194_FC2B_F72C,
                0x9F55_5FA3_C84C_64C2,
                0x2393_B86B_6F53_B151,
                0x9638_7719_5940_EABD,
                0x9628_3EE2_A88E_FFE3,
                0xBE5E_1E25_5386_3992,
                0x2B01_99FC_2C85_B8AA,
                0x0EB7_2DDC_81C5_2CA2,
            ],
            [
                0x6a09_e667_f3bc_c908,
                0xbb67_ae85_84ca_a73b,
                0x3c6e_f372_fe94_f82b,
                0xa54f_f53a_5f1d_36f1,
                0x510e_527f_ade6_82d1,
                0x9b05_688c_2b3e_6c1f,
                0x1f83_d9ab_fb41_bd6b,
                0x5be0_cd19_137e_2179,
            ],
        ];
        Sha512 {
            state: IV[usize::from(V)],
            count: [0; 2],
            buffer: [0; 128],
        }
    }
}

impl<const V: u8> State for Sha512<V> {
    /// `PHP_SHA512Update`.
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        let index = ((self.count[0] >> 3) & 0x7f) as usize;
        let bits = (u128::from(self.count[1]) << 64 | u128::from(self.count[0]))
            .wrapping_add((input.len() as u128) * 8);
        self.count = [bits as u64, (bits >> 64) as u64];
        let state = &mut self.state;
        feed(&mut self.buffer, index, input, |block| {
            if let Ok(block) = <[u8; 128]>::try_from(block) {
                sha2::block_api::compress512(state, &[block]);
            }
        });
    }

    /// `PHP_SHA512Final`, and the truncation of `PHP_SHA384Final` and
    /// the two `PHP_SHA512_*Final`s.
    fn finish(&mut self) -> Vec<u8> {
        let [low, high] = self.count;
        let bits = [high.to_be_bytes(), low.to_be_bytes()].concat();
        let index = ((low >> 3) & 0x7f) as usize;
        let pad = if index < 112 {
            112 - index
        } else {
            240 - index
        };
        self.update(&PADDING[..pad]);
        self.update(&bits);
        let mut out: Vec<u8> = self.state.iter().flat_map(|w| w.to_be_bytes()).collect();
        out.truncate([48, 28, 32, 64][usize::from(V)]);
        out
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::Q(&mut self.state),
            Slot::Q(&mut self.count),
            Slot::B(&mut self.buffer),
        ]
    }
}

/// `Keccak_HashInstance` as the 64-bit lane-complemented XKCP build
/// keeps it, which is the state `__serialize` writes: lanes 1, 2, 8,
/// 12, 17 and 20 held inverted, and `byteIOIndex`. `D` is the digest's
/// bytes and the rate is `200 - 2D`.
///
/// ```php
/// foreach (['sha3-224', 'sha3-256', 'sha3-384', 'sha3-512'] as $a) {
///     $c = hash_init($a); hash_update($c, str_repeat('ab', 80)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, ''), ' ', hash_final($c), "\n";
/// }
/// ```
/// ```expect
/// sha3-224 6b4e03423667dbb73b6e15454f0eb1abd4597f9a1b078e3f5b5a6bc7 3661ee3fc02205df26dfd656941e6c8ace89134405844bc1950ff219
/// sha3-256 a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a 8aaf041c86cf58ef3727ce86f077568f5268b1bf40b8231dc8be577ca2645327
/// sha3-384 0c63a75b845e4f7d01107d852e4c2485c51a50aaaa94fc61995e71bbee983a2ac3713831264adb47fb6bd1e058d5f004 86ccb1a1d1445279ea403ed87e05dc76486dd9189da51bdf31aeef5f9b92a4d94f2109dcfac54be90a61b4a6d6c9e161
/// sha3-512 a69f73cca23a9ac5c8b567dc185a756e97c982164fe25859e0d1dcc1475c80a615b2123af1f5f94c11e3e9402c3ac558f500199d95b6d3e301758586281dcd26 89d82bdf4f1cee17860871b5334be69bfa67a58dabaadf73587620cc61d2f415ec75585f4f000fb67f1cc9e46aed461690ae4393152b5987defbad472c047457
/// ```
#[derive(Clone)]
pub(crate) struct Sha3<const D: usize> {
    lanes: [u8; 200],
    index: u32,
}

/// The lanes `generic64lc` keeps complemented.
const COMPLEMENTED: [usize; 6] = [1, 2, 8, 12, 17, 20];

impl<const D: usize> Default for Sha3<D> {
    /// `KeccakP1600_Initialize`: zeros, the complemented lanes all ones.
    fn default() -> Sha3<D> {
        let mut sponge = Sha3 {
            lanes: [0; 200],
            index: 0,
        };
        sponge.complement();
        sponge
    }
}

impl<const D: usize> Sha3<D> {
    const RATE: usize = 200 - 2 * D;

    fn complement(&mut self) {
        for lane in COMPLEMENTED {
            for byte in &mut self.lanes[8 * lane..8 * lane + 8] {
                *byte = !*byte;
            }
        }
    }

    /// `KeccakP1600_Permute_24rounds` on the complemented state.
    fn permute(&mut self) {
        self.complement();
        let mut lanes = [0u64; 25];
        for (lane, bytes) in lanes.iter_mut().zip(self.lanes.chunks_exact(8)) {
            let mut word = [0u8; 8];
            word.copy_from_slice(bytes);
            *lane = u64::from_le_bytes(word);
        }
        keccak::Keccak::new().with_f1600(|f1600| f1600(&mut lanes));
        for (lane, bytes) in lanes.iter().zip(self.lanes.chunks_exact_mut(8)) {
            bytes.copy_from_slice(&lane.to_le_bytes());
        }
        self.complement();
    }
}

impl<const D: usize> State for Sha3<D> {
    /// `KeccakWidth1600_SpongeAbsorb`: bytes into the lanes at
    /// `byteIOIndex`, a permutation each time the rate fills.
    fn update(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            let at = self.index as usize;
            let take = (Self::RATE - at).min(input.len());
            for (lane, byte) in self.lanes[at..at + take].iter_mut().zip(input) {
                *lane ^= byte;
            }
            input = &input[take..];
            self.index += take as u32;
            if self.index as usize == Self::RATE {
                self.permute();
                self.index = 0;
            }
        }
    }

    /// `Keccak_HashFinal`: the suffix `0x06`, the final bit, one
    /// permutation, and the digest out of the rate.
    fn finish(&mut self) -> Vec<u8> {
        let at = self.index as usize;
        self.lanes[at] ^= 0x06;
        self.lanes[Self::RATE - 1] ^= 0x80;
        self.permute();
        self.complement();
        self.lanes[..D].to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::B(&mut self.lanes),
            Slot::Over(1),
            Slot::L(slice::from_mut(&mut self.index)),
        ]
    }

    /// `php_keccak_unserialize`: `byteIOIndex < rate / 8`.
    fn valid(&self) -> bool {
        (self.index as usize) < Self::RATE
    }
}
