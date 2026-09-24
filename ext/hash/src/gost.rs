//! `hash_gost.c`: GOST R 34.11-94 after Saarinen's `gost_compress`,
//! under the test parameters (`gost`) or `CryptoPro`'s (`gost-crypto`).
//! The four substitution tables `php_hash_gost_tables.h` spells out are
//! built here from the eight 4-bit S-boxes, and the `SHIFT12`,
//! `SHIFT16` and `SHIFT61` php unrolls are the standard's ψ applied 12,
//! 1 and 61 times.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::algos::{Slot, State, le_words};
use crate::snefru::feed32;

type SBox = [[u32; 16]; 8];

const TEST: SBox = [
    [4, 10, 9, 2, 13, 8, 0, 14, 6, 11, 1, 12, 7, 15, 5, 3],
    [14, 11, 4, 12, 6, 13, 15, 10, 2, 3, 8, 1, 0, 7, 5, 9],
    [5, 8, 1, 13, 10, 3, 4, 2, 14, 15, 12, 7, 6, 0, 9, 11],
    [7, 13, 10, 1, 0, 8, 9, 15, 14, 4, 6, 12, 11, 2, 5, 3],
    [6, 12, 7, 1, 5, 15, 13, 8, 4, 10, 9, 14, 0, 3, 11, 2],
    [4, 11, 10, 0, 7, 2, 1, 13, 3, 6, 8, 5, 9, 12, 15, 14],
    [13, 11, 4, 1, 3, 15, 5, 9, 0, 10, 14, 7, 6, 8, 2, 12],
    [1, 15, 13, 0, 5, 7, 10, 4, 9, 2, 3, 14, 6, 11, 8, 12],
];

const CRYPTO_PRO: SBox = [
    [10, 4, 5, 6, 8, 1, 3, 7, 13, 12, 14, 0, 9, 2, 11, 15],
    [5, 15, 4, 0, 2, 13, 11, 9, 1, 7, 6, 3, 12, 14, 10, 8],
    [7, 15, 12, 14, 9, 4, 1, 0, 3, 11, 5, 2, 6, 10, 8, 13],
    [4, 10, 7, 12, 0, 15, 2, 8, 14, 1, 6, 5, 13, 11, 9, 3],
    [7, 6, 4, 11, 9, 12, 2, 10, 1, 8, 0, 14, 15, 13, 3, 5],
    [7, 6, 2, 4, 13, 9, 15, 0, 10, 1, 5, 11, 8, 14, 12, 3],
    [13, 14, 4, 1, 7, 0, 5, 10, 3, 12, 8, 15, 6, 2, 9, 11],
    [1, 3, 10, 9, 5, 11, 4, 15, 8, 6, 7, 14, 13, 0, 2, 12],
];

/// `tables[k][b]`: byte `k` of a word through its two S-boxes, then the
/// cipher's rotation by eleven.
const fn tables(s: &SBox) -> [[u32; 256]; 4] {
    let mut t = [[0u32; 256]; 4];
    let mut k = 0;
    while k < 4 {
        let mut b = 0;
        while b < 256 {
            let v = ((s[2 * k + 1][b >> 4] << 4) | s[2 * k][b & 15]) << (8 * k);
            t[k][b] = v.rotate_left(11);
            b += 1;
        }
        k += 1;
    }
    t
}

static TABLES: [[[u32; 256]; 4]; 2] = [tables(&TEST), tables(&CRYPTO_PRO)];

/// ψ: the sixteen 16-bit words shifted down one, the new top word the
/// sum of words 0, 1, 2, 3, 12 and 15.
fn psi(x: &mut [u32; 8], times: usize) {
    for _ in 0..times {
        let w = |i: usize| (x[i / 2] >> (16 * (i % 2))) & 0xffff;
        let top = w(0) ^ w(1) ^ w(2) ^ w(3) ^ w(12) ^ w(15);
        for i in 0..7 {
            x[i] = (x[i] >> 16) | (x[i + 1] << 16);
        }
        x[7] = (x[7] >> 16) | (top << 16);
    }
}

/// `Gost`: four keys from `u ^ v`, the four encryptions of `h`, and the
/// mixing of message, state and ciphertext.
fn gost(t: &[[u32; 256]; 4], h: &mut [u32], m: &[u32; 8]) {
    let (mut u, mut v, mut s) = ([0u32; 8], *m, [0u32; 8]);
    u.copy_from_slice(&h[..8]);
    let e = |x: u32| {
        t[0][(x & 0xff) as usize]
            ^ t[1][((x >> 8) & 0xff) as usize]
            ^ t[2][((x >> 16) & 0xff) as usize]
            ^ t[3][(x >> 24) as usize]
    };
    for i in (0..8).step_by(2) {
        let w: [u32; 8] = std::array::from_fn(|j| u[j] ^ v[j]);
        let key: [u32; 8] = std::array::from_fn(|k| {
            (0..4).fold(0, |acc, j| {
                acc | ((w[2 * j + k / 4] >> (8 * (k % 4))) & 0xff) << (8 * j)
            })
        });
        let (mut r, mut l) = (h[i], h[i + 1]);
        for n in 0..16 {
            let (k1, k2) = if n < 12 {
                (key[2 * (n % 4)], key[2 * (n % 4) + 1])
            } else {
                (key[7 - 2 * (n % 4)], key[6 - 2 * (n % 4)])
            };
            l ^= e(k1.wrapping_add(r));
            r ^= e(k2.wrapping_add(l));
        }
        s[i] = l;
        s[i + 1] = r;
        if i != 6 {
            let (l, r) = (u[0] ^ u[2], u[1] ^ u[3]);
            u.copy_within(2.., 0);
            u[6] = l;
            u[7] = r;
            if i == 2 {
                const C: [u32; 8] = [
                    0xff00_ff00,
                    0xff00_ff00,
                    0x00ff_00ff,
                    0x00ff_00ff,
                    0x00ff_ff00,
                    0xff00_00ff,
                    0x0000_00ff,
                    0xff00_ffff,
                ];
                for (word, c) in u.iter_mut().zip(C) {
                    *word ^= c;
                }
            }
            let (l, r) = (v[0], v[2]);
            v[0] = v[4];
            v[2] = v[6];
            v[4] = l ^ r;
            v[6] = v[0] ^ r;
            let (l, r) = (v[1], v[3]);
            v[1] = v[5];
            v[3] = v[7];
            v[5] = l ^ r;
            v[7] = v[1] ^ r;
        }
    }
    psi(&mut s, 12);
    let mut u: [u32; 8] = std::array::from_fn(|j| m[j] ^ s[j]);
    psi(&mut u, 1);
    let mut v: [u32; 8] = std::array::from_fn(|j| h[j] ^ u[j]);
    psi(&mut v, 61);
    h[..8].copy_from_slice(&v);
}

/// `PHP_GOST_CTX`: `state` (the chaining value and the checksum),
/// the bit count, `length` and `buffer`; `C` picks `tables_crypto`.
///
/// ```php
/// foreach (['gost', 'gost-crypto'] as $a) {
///     $c = hash_init($a); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, ''), ' ', hash($a, 'abc'), ' ', hash_final($c), "\n";
/// }
/// ```
/// ```expect
/// gost ce85b99cc46752fffee35cab9a7b0278abb4c2d2055cff685af4912c49490f8d f3134348c44fb1b2a277729e2285ebb5cb5e0f29c975bc753b70497c06a4d51d b1cb2d2673f2b3db7ffefe0abcc060900373e4e93e129cce2515d8032b4289ab
/// gost-crypto 981e5f3ca30c841487830f84fb433e13ac1101569b9c13584ac483234cd656c0 b285056dbf18d7392d7677369524dd14747459ed8143997e163b2986f92fd42c dd74b37a94e453d69695aeba55be9ac9e9ac56b2b392de45424d235244a940ee
/// ```
#[derive(Clone)]
pub(crate) struct Gost<const C: bool> {
    state: [u32; 16],
    count: [u32; 2],
    length: [u8; 1],
    buffer: [u8; 32],
}

impl<const C: bool> Default for Gost<C> {
    fn default() -> Gost<C> {
        Gost {
            state: [0; 16],
            count: [0; 2],
            length: [0],
            buffer: [0; 32],
        }
    }
}

impl<const C: bool> Gost<C> {
    /// `GostTransform`: the block added into the checksum with carry,
    /// then compressed.
    fn transform(&mut self, block: &[u8]) {
        let data: [u32; 8] = le_words(block);
        let mut carry = 0u32;
        for (sum, &word) in self.state[8..].iter_mut().zip(&data) {
            *sum = sum.wrapping_add(word).wrapping_add(carry);
            carry = match (*sum).cmp(&word) {
                std::cmp::Ordering::Less => 1,
                std::cmp::Ordering::Equal => carry,
                std::cmp::Ordering::Greater => 0,
            };
        }
        gost(&TABLES[usize::from(C)], &mut self.state, &data);
    }
}

impl<const C: bool> State for Gost<C> {
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        let bits = input.len() as u64 * 8;
        if u64::from(u32::MAX - self.count[0]) < bits {
            self.count[1] = self.count[1].wrapping_add(1);
            self.count[0] = u32::MAX - self.count[0];
            self.count[0] = (bits as u32).wrapping_sub(self.count[0]);
        } else {
            self.count[0] = self.count[0].wrapping_add(bits as u32);
        }
        let mut buffer = self.buffer;
        let mut length = self.length[0];
        feed32(&mut buffer, &mut length, input, |block| {
            self.transform(block);
        });
        self.buffer = buffer;
        self.length[0] = length;
    }

    /// `PHP_GOSTFinal`: the bit count and then the checksum through the
    /// compression.
    fn finish(&mut self) -> Vec<u8> {
        if self.length[0] != 0 {
            let buffer = self.buffer;
            self.transform(&buffer);
        }
        let table = &TABLES[usize::from(C)];
        let count = [self.count[0], self.count[1], 0, 0, 0, 0, 0, 0];
        gost(table, &mut self.state, &count);
        let mut sum = [0u32; 8];
        sum.copy_from_slice(&self.state[8..]);
        gost(table, &mut self.state, &sum);
        self.state[..8]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::L(&mut self.state),
            Slot::L(&mut self.count),
            Slot::B(&mut self.length),
            Slot::B(&mut self.buffer),
        ]
    }

    /// `php_gost_unserialize`: `length < sizeof(buffer)`.
    fn valid(&self) -> bool {
        self.length[0] < 32
    }
}
