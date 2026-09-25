//! `hash_murmur.c` over `murmur/PMurHash.c` and `PMurHash128.c`:
//! `MurmurHash3`'s 32-bit, x86 128-bit and x64 128-bit streams, the
//! bytes not yet in a block kept in the carry words with their count in
//! the low bits. The oracle's arm64 takes `PMurHash`'s aligned branch, so
//! after whole blocks the partly filled carry word holds the last input
//! word as read, and a 32-bit stream whose carry was empty keeps it;
//! the digest is the same either way, the serialised carry is not.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::slice;

use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::algos::{Options, Slot, State};

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

fn fmix32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^ (k >> 33)
}

/// `murmur3a`: `h`, `carry` and `len`.
///
/// ```php
/// foreach (['murmur3a', 'murmur3c', 'murmur3f'] as $a) {
///     $c = hash_init($a, 0, '', ['seed' => 42]); hash_update($c, str_repeat('ab', 20)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, ''), ' ', hash($a, 'The quick brown fox jumps over the lazy dog'), ' ', hash_final($c), "\n";
///     $c = hash_init($a); hash_update($c, 'abcde'); hash_update($c, str_repeat('xyz', 11));
///     echo bin2hex(serialize($c)), "\n";
/// }
/// var_dump(hash('murmur3a', 'x', false, ['seed' => 'a']));
/// ```
/// ```expect
/// murmur3a 00000000 2e4ff723 04a81125
/// 4f3a31313a2248617368436f6e74657874223a353a7b693a303b733a383a226d75726d75723361223b693a313b693a303b693a323b613a333a7b693a303b693a2d3836303833363532303b693a313b693a323035343738303933303b693a323b693a33383b7d693a333b693a323b693a343b613a303a7b7d7d
/// murmur3c 00000000000000000000000000000000 2f1583c3ecee2c675d7bf66ce5e91d2c 26e6ea63bf58c36d395a18d1f0fed1ea
/// 4f3a31313a2248617368436f6e74657874223a353a7b693a303b733a383a226d75726d75723363223b693a313b693a303b693a323b613a393a7b693a303b693a2d3739393536313433313b693a313b693a313132303531353232393b693a323b693a3834353632323931373b693a333b693a2d3330303939353336313b693a343b693a323032313239323430383b693a353b693a323035343738313035303b693a363b693a2d313338373939393731353b693a373b693a2d313131383032373030323b693a383b693a33383b7d693a333b693a323b693a343b613a303a7b7d7d
/// murmur3f 00000000000000000000000000000000 e34bbc7bbc071b6c7a433ca9c49a9347 21d1a80a2653f6724059efa1fb33081c
/// 4f3a31313a2248617368436f6e74657874223a353a7b693a303b733a383a226d75726d75723366223b693a313b693a303b693a323b613a393a7b693a303b693a2d313037333636303239393b693a313b693a313030363430333932333b693a323b693a38393637373738373b693a333b693a2d32333832363731393b693a343b693a323033373933383830393b693a353b693a323035343738313035303b693a363b693a2d35313335333835303b693a373b693a343437313230393b693a383b693a33383b7d693a333b693a323b693a343b613a303a7b7d7d
///
/// Deprecated: hash(): Passing a seed of a type other than int is deprecated because it is the same as setting the seed to 0 in %s on line 8
/// string(8) "3e9a9b1b"
/// ```
#[derive(Clone)]
pub(crate) struct Murmur3a {
    h: u32,
    carry: u32,
    len: u32,
}

impl Murmur3a {
    /// `PHP_MURMUR3AInit`.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn init(req: &mut Request, options: Options) -> Result<Box<dyn State>, Thrown> {
        let seed = options.seed(req)? as u32;
        Ok(Box::new(Murmur3a {
            h: seed,
            carry: 0,
            len: 0,
        }))
    }
}

/// `doblock(h1, k1)`, which leaves `k1` mixed, as the macro does to
/// the carry it is handed.
fn block32(h: &mut u32, k: &mut u32) {
    *k = k
        .wrapping_mul(0xcc9e_2d51)
        .rotate_left(15)
        .wrapping_mul(0x1b87_3593);
    *h = (*h ^ *k)
        .rotate_left(13)
        .wrapping_mul(5)
        .wrapping_add(0xe654_6b64);
}

impl State for Murmur3a {
    /// `PMurHash32_Process`.
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        let (mut h, mut c) = (self.h, self.carry);
        let mut n = c & 3;
        let whole = input.len() & !3;
        for word in input[..whole].chunks_exact(4) {
            let w = le32(word);
            let mut k = if n == 0 {
                w
            } else {
                let k = (c >> (32 - 8 * n)) | (w << (8 * n));
                c = w;
                k
            };
            block32(&mut h, &mut k);
        }
        for &byte in &input[whole..] {
            c = (c >> 8) | u32::from(byte) << 24;
            n += 1;
            if n == 4 {
                block32(&mut h, &mut c);
                n = 0;
            }
        }
        self.h = h;
        self.carry = (c & !0xff) | n;
        self.len = self.len.wrapping_add(input.len() as u32);
    }

    /// `PMurHash32_Result`.
    fn finish(&mut self) -> Vec<u8> {
        let (mut h, carry) = (self.h, self.carry);
        let n = carry & 3;
        if n != 0 {
            let k = (carry >> ((4 - n) * 8))
                .wrapping_mul(0xcc9e_2d51)
                .rotate_left(15)
                .wrapping_mul(0x1b87_3593);
            h ^= k;
        }
        fmix32(h ^ self.len).to_be_bytes().to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::L(slice::from_mut(&mut self.h)),
            Slot::L(slice::from_mut(&mut self.carry)),
            Slot::L(slice::from_mut(&mut self.len)),
        ]
    }
}

/// `murmur3c`: `h[4]`, `carry[4]` and `len`.
#[derive(Clone)]
pub(crate) struct Murmur3c {
    h: [u32; 4],
    carry: [u32; 4],
    len: u32,
}

impl Murmur3c {
    /// `PHP_MURMUR3CInit`.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn init(req: &mut Request, options: Options) -> Result<Box<dyn State>, Thrown> {
        let seed = options.seed(req)? as u32;
        Ok(Box::new(Murmur3c {
            h: [seed; 4],
            carry: [0; 4],
            len: 0,
        }))
    }
}

const C1: u32 = 0x239b_961b;
const C2: u32 = 0xab0e_9789;
const C3: u32 = 0x38b3_4ae5;
const C4: u32 = 0xa1e3_8b93;

/// Each lane's multipliers and rotation.
const MUL: [(u32, u32, u32); 4] = [(C1, 15, C2), (C2, 16, C3), (C3, 17, C4), (C4, 18, C1)];

/// `doblock128x86`, which leaves the four carry words mixed.
fn block_x86(h: &mut [u32; 4], k: &mut [u32; 4]) {
    const ADD: [u32; 4] = [0x561c_cd1b, 0x0bca_a747, 0x96cd_1c35, 0x32ac_3b17];
    const ROT: [u32; 4] = [19, 17, 15, 13];
    for i in 0..4 {
        let (a, r, b) = MUL[i];
        k[i] = k[i].wrapping_mul(a).rotate_left(r).wrapping_mul(b);
        h[i] ^= k[i];
        h[i] = h[i]
            .rotate_left(ROT[i])
            .wrapping_add(h[(i + 1) % 4])
            .wrapping_mul(5)
            .wrapping_add(ADD[i]);
    }
}

impl State for Murmur3c {
    /// `PMurHash128x86_Process`.
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        let (h, k) = (&mut self.h, &mut self.carry);
        let mut n = (k[3] & 15) as usize;
        let whole = input.len() & !15;
        for (at, &byte) in input.iter().enumerate() {
            if at == whole && whole > 0 && !n.is_multiple_of(4) {
                k[n / 4] = le32(&input[whole - 4..whole]);
            }
            k[n / 4] = (k[n / 4] >> 8) | u32::from(byte) << 24;
            n += 1;
            if n == 16 {
                block_x86(h, k);
                n = 0;
            }
        }
        if input.len() == whole && whole > 0 && !n.is_multiple_of(4) {
            k[n / 4] = le32(&input[whole - 4..whole]);
        }
        k[3] = (k[3] & !0xff) | n as u32;
        self.len = self.len.wrapping_add(input.len() as u32);
    }

    /// `PMurHash128x86_Result`.
    fn finish(&mut self) -> Vec<u8> {
        let (mut h, carry, len) = (self.h, self.carry, self.len);
        let n = (carry[3] & 15) as usize;
        if n != 0 {
            let top = (n - 1) / 4;
            for i in (0..=top).rev() {
                let mut k = carry[i];
                if i == top {
                    k >>= (4 * (top + 1) - n) * 8;
                }
                let (a, r, b) = MUL[i];
                h[i] ^= k.wrapping_mul(a).rotate_left(r).wrapping_mul(b);
            }
        }
        for word in &mut h {
            *word ^= len;
        }
        let mix = |h: &mut [u32; 4]| {
            h[0] = h[0]
                .wrapping_add(h[1])
                .wrapping_add(h[2])
                .wrapping_add(h[3]);
            for i in 1..4 {
                h[i] = h[i].wrapping_add(h[0]);
            }
        };
        mix(&mut h);
        for word in &mut h {
            *word = fmix32(*word);
        }
        mix(&mut h);
        h.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::L(&mut self.h),
            Slot::L(&mut self.carry),
            Slot::L(slice::from_mut(&mut self.len)),
        ]
    }
}

/// `murmur3f`: `h[2]` and `carry[2]` as 64-bit words, then `len`.
#[derive(Clone)]
pub(crate) struct Murmur3f {
    h: [u64; 2],
    carry: [u64; 2],
    len: u32,
}

impl Murmur3f {
    /// `PHP_MURMUR3FInit`.
    pub(crate) fn init(req: &mut Request, options: Options) -> Result<Box<dyn State>, Thrown> {
        let seed = options.seed(req)?;
        Ok(Box::new(Murmur3f {
            h: [seed; 2],
            carry: [0; 2],
            len: 0,
        }))
    }
}

const L1: u64 = 0x87c3_7b91_1142_53d5;
const L2: u64 = 0x4cf5_ad43_2745_937f;

/// `doblock128x64`, which leaves both carry words mixed.
fn block_x64(h: &mut [u64; 2], k: &mut [u64; 2]) {
    k[0] = k[0].wrapping_mul(L1).rotate_left(31).wrapping_mul(L2);
    h[0] ^= k[0];
    h[0] = h[0]
        .rotate_left(27)
        .wrapping_add(h[1])
        .wrapping_mul(5)
        .wrapping_add(0x52dc_e729);
    k[1] = k[1].wrapping_mul(L2).rotate_left(33).wrapping_mul(L1);
    h[1] ^= k[1];
    h[1] = h[1]
        .rotate_left(31)
        .wrapping_add(h[0])
        .wrapping_mul(5)
        .wrapping_add(0x3849_5ab5);
}

impl State for Murmur3f {
    /// `PMurHash128x64_Process`.
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        let (h, k) = (&mut self.h, &mut self.carry);
        let mut n = (k[1] & 15) as usize;
        let whole = input.len() & !15;
        for (at, &byte) in input.iter().enumerate() {
            if at == whole && whole > 0 && !n.is_multiple_of(8) {
                k[n / 8] = le64(&input[whole - 8..whole]);
            }
            k[n / 8] = (k[n / 8] >> 8) | u64::from(byte) << 56;
            n += 1;
            if n == 16 {
                block_x64(h, k);
                n = 0;
            }
        }
        if input.len() == whole && whole > 0 && !n.is_multiple_of(8) {
            k[n / 8] = le64(&input[whole - 8..whole]);
        }
        k[1] = (k[1] & !0xff) | n as u64;
        self.len = self.len.wrapping_add(input.len() as u32);
    }

    /// `PMurHash128x64_Result`.
    fn finish(&mut self) -> Vec<u8> {
        let ([mut h1, mut h2], [k1, mut k2]) = (self.h, self.carry);
        let n = (k2 & 15) as u32;
        if n != 0 {
            let mut k1 = k1;
            if n > 8 {
                k2 >>= (16 - n) * 8;
                h2 ^= k2.wrapping_mul(L2).rotate_left(33).wrapping_mul(L1);
            } else {
                k1 >>= (8 - n) * 8;
            }
            h1 ^= k1.wrapping_mul(L1).rotate_left(31).wrapping_mul(L2);
        }
        let len = u64::from(self.len);
        h1 ^= len;
        h2 ^= len;
        h1 = h1.wrapping_add(h2);
        h2 = h2.wrapping_add(h1);
        h1 = fmix64(h1);
        h2 = fmix64(h2);
        h1 = h1.wrapping_add(h2);
        h2 = h2.wrapping_add(h1);
        [h1.to_be_bytes(), h2.to_be_bytes()].concat()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::Q(&mut self.h),
            Slot::Q(&mut self.carry),
            Slot::L(slice::from_mut(&mut self.len)),
        ]
    }
}
