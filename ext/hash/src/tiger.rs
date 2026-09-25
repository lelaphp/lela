//! `hash_tiger.c`: Tiger with three passes or four, and its four S-boxes,
//! which `php_hash_tiger_tables.h` spells out and which are built here at
//! compile time by the generator Anderson and Biham published beside the
//! reference code, running Tiger's own compression over the tables as
//! they fill.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::slice;

use crate::algos::{Slot, State};

type Tables = [[u64; 256]; 4];

#[allow(clippy::cast_possible_truncation)]
const fn round(t: &Tables, v: &mut [u64; 3], [a, b, c]: [usize; 3], x: u64, mul: u64) {
    v[c] ^= x;
    let c = v[c].to_le_bytes();
    let [c0, c1, c2, c3, c4, c5, c6, c7] = [
        c[0] as usize,
        c[1] as usize,
        c[2] as usize,
        c[3] as usize,
        c[4] as usize,
        c[5] as usize,
        c[6] as usize,
        c[7] as usize,
    ];
    v[a] = v[a].wrapping_sub(t[0][c0] ^ t[1][c2] ^ t[2][c4] ^ t[3][c6]);
    v[b] = v[b].wrapping_add(t[3][c1] ^ t[2][c3] ^ t[1][c5] ^ t[0][c7]);
    v[b] = v[b].wrapping_mul(mul);
}

/// `pass(a, b, c, mul)`: eight rounds, the roles turning.
const fn pass(t: &Tables, v: &mut [u64; 3], [a, b, c]: [usize; 3], x: &[u64; 8], mul: u64) {
    let mut i = 0;
    while i < 8 {
        let roles = match i % 3 {
            0 => [a, b, c],
            1 => [b, c, a],
            _ => [c, a, b],
        };
        round(t, v, roles, x[i], mul);
        i += 1;
    }
}

/// `key_schedule`.
const fn schedule(x: &mut [u64; 8]) {
    x[0] = x[0].wrapping_sub(x[7] ^ 0xA5A5_A5A5_A5A5_A5A5);
    x[1] ^= x[0];
    x[2] = x[2].wrapping_add(x[1]);
    x[3] = x[3].wrapping_sub(x[2] ^ ((!x[1]) << 19));
    x[4] ^= x[3];
    x[5] = x[5].wrapping_add(x[4]);
    x[6] = x[6].wrapping_sub(x[5] ^ ((!x[4]) >> 23));
    x[7] ^= x[6];
    x[0] = x[0].wrapping_add(x[7]);
    x[1] = x[1].wrapping_sub(x[0] ^ ((!x[7]) << 19));
    x[2] ^= x[1];
    x[3] = x[3].wrapping_add(x[2]);
    x[4] = x[4].wrapping_sub(x[3] ^ ((!x[2]) >> 23));
    x[5] ^= x[4];
    x[6] = x[6].wrapping_add(x[5]);
    x[7] = x[7].wrapping_sub(x[6] ^ 0x0123_4567_89AB_CDEF);
}

/// `tiger_compress(passes, str, state)`: three passes, then `extra`
/// more, each followed by php's rotation of `a`, `b` and `c`.
const fn compress(t: &Tables, extra: u32, block: &[u64; 8], state: &mut [u64; 3]) {
    let mut v = *state;
    let mut x = *block;
    pass(t, &mut v, [0, 1, 2], &x, 5);
    schedule(&mut x);
    pass(t, &mut v, [2, 0, 1], &x, 7);
    schedule(&mut x);
    pass(t, &mut v, [1, 2, 0], &x, 9);
    let mut p = 0;
    while p < extra {
        schedule(&mut x);
        pass(t, &mut v, [0, 1, 2], &x, 9);
        v = [v[2], v[0], v[1]];
        p += 1;
    }
    state[0] ^= v[0];
    state[1] = v[1].wrapping_sub(state[1]);
    state[2] = v[2].wrapping_add(state[2]);
}

/// The initial words.
const IV: [u64; 3] = [
    0x0123_4567_89AB_CDEF,
    0xFEDC_BA98_7654_3210,
    0xF096_A5B4_C3B2_E187,
];

/// The S-box generator: every byte column of each box starts as the
/// identity and is shuffled by five passes of swaps whose partners the
/// running compression of the paper's title string picks.
#[allow(clippy::cast_possible_truncation)]
const TABLES: Tables = {
    let mut t = [[0u64; 256]; 4];
    let text = b"Tiger - A Fast New Hash Function, by Ross Anderson and Eli Biham";
    let mut block = [0u64; 8];
    let mut i = 0;
    while i < 64 {
        block[i / 8] |= (text[i] as u64) << (8 * (i % 8));
        i += 1;
    }
    let mut i = 0;
    while i < 1024 {
        t[i / 256][i % 256] = (i as u64 & 255) * 0x0101_0101_0101_0101;
        i += 1;
    }
    let mut state = IV;
    let mut abc = 2;
    let mut n = 0;
    while n < 5 * 256 * 4 {
        let (i, sb) = ((n / 4) % 256, n % 4);
        abc += 1;
        if abc == 3 {
            abc = 0;
            compress(&t, 0, &block, &mut state);
        }
        let mut shift = 0;
        while shift < 64 {
            let partner = ((state[abc] >> shift) & 0xff) as usize;
            let mine = (t[sb][i] >> shift) & 0xff;
            let theirs = (t[sb][partner] >> shift) & 0xff;
            t[sb][i] = (t[sb][i] & !(0xff << shift)) | (theirs << shift);
            t[sb][partner] = (t[sb][partner] & !(0xff << shift)) | (mine << shift);
            shift += 8;
        }
        n += 1;
    }
    t
};

/// `PHP_TIGER_CTX`: `state`, `passed`, `buffer` and `length`, with
/// `passes` the bit `PHP_4TIGERInit` sets and `D` the digest's bytes.
///
/// ```php
/// foreach (['tiger128,3', 'tiger160,3', 'tiger192,3', 'tiger128,4', 'tiger160,4', 'tiger192,4'] as $a) {
///     $c = hash_init($a); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, ''), ' ', hash_final($c), "\n";
/// }
/// ```
/// ```expect
/// tiger128,3 3293ac630c13f0245f92bbb1766e1616 b5e0a1dfac9cf329a03ab4a300b7493b
/// tiger160,3 3293ac630c13f0245f92bbb1766e16167a4e5849 b5e0a1dfac9cf329a03ab4a300b7493b708da7cb
/// tiger192,3 3293ac630c13f0245f92bbb1766e16167a4e58492dde73f3 b5e0a1dfac9cf329a03ab4a300b7493b708da7cbe33cf202
/// tiger128,4 24cc78a7f6ff3546e7984e59695ca13d 19338ab3bc53f326eda7c7701974604c
/// tiger160,4 24cc78a7f6ff3546e7984e59695ca13d804e0b68 19338ab3bc53f326eda7c7701974604c96cdd27a
/// tiger192,4 24cc78a7f6ff3546e7984e59695ca13d804e0b686e255194 19338ab3bc53f326eda7c7701974604c96cdd27ac70c645c
/// ```
#[derive(Clone)]
pub(crate) struct Tiger<const P: u32, const D: usize> {
    state: [u64; 3],
    /// The bits of every block compressed.
    passed: u64,
    buffer: [u8; 64],
    length: u32,
}

impl<const P: u32, const D: usize> Default for Tiger<P, D> {
    fn default() -> Tiger<P, D> {
        Tiger {
            state: IV,
            passed: 0,
            buffer: [0; 64],
            length: 0,
        }
    }
}

impl<const P: u32, const D: usize> Tiger<P, D> {
    fn compress(&mut self, block: &[u8]) {
        let mut words = [0u64; 8];
        for (word, bytes) in words.iter_mut().zip(block.chunks_exact(8)) {
            let mut b = [0u8; 8];
            b.copy_from_slice(bytes);
            *word = u64::from_le_bytes(b);
        }
        compress(&TABLES, P, &words, &mut self.state);
    }
}

impl<const P: u32, const D: usize> State for Tiger<P, D> {
    /// `PHP_TIGERUpdate`: `passed` counts the bits of every block
    /// compressed.
    fn update(&mut self, input: &[u8]) {
        let length = self.length as usize;
        if length + input.len() < 64 {
            self.buffer[length..length + input.len()].copy_from_slice(input);
            self.length += input.len() as u32;
            return;
        }
        let rest = (length + input.len()) % 64;
        let mut i = 0;
        if length != 0 {
            i = 64 - length;
            self.buffer[length..].copy_from_slice(&input[..i]);
            let block = self.buffer;
            self.compress(&block);
            self.buffer = [0; 64];
            self.passed = self.passed.wrapping_add(512);
        }
        while i + 64 <= input.len() {
            self.buffer.copy_from_slice(&input[i..i + 64]);
            let block = self.buffer;
            self.compress(&block);
            self.passed = self.passed.wrapping_add(512);
            i += 64;
        }
        self.buffer[rest..].fill(0);
        self.buffer[..rest].copy_from_slice(&input[i..]);
        self.length = rest as u32;
    }

    /// `TigerFinalize` and `TigerDigest`: the state's words
    /// little-endian, cut to the digest.
    fn finish(&mut self) -> Vec<u8> {
        let mut length = self.length as usize;
        self.passed = self.passed.wrapping_add((length as u64) << 3);
        self.buffer[length] = 1;
        length += 1;
        if !length.is_multiple_of(8) {
            let pad = 8 - length % 8;
            self.buffer[length..length + pad].fill(0);
            length += pad;
        }
        if length > 56 {
            self.buffer[length..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer[..56].fill(0);
        } else {
            self.buffer[length..56].fill(0);
        }
        self.buffer[56..].copy_from_slice(&self.passed.to_le_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut out: Vec<u8> = self.state.iter().flat_map(|w| w.to_le_bytes()).collect();
        out.truncate(D);
        out
    }

    /// `q3qb64l`.
    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::Q(&mut self.state),
            Slot::Q(slice::from_mut(&mut self.passed)),
            Slot::B(&mut self.buffer),
            Slot::L(slice::from_mut(&mut self.length)),
        ]
    }

    /// `php_tiger_unserialize`: `length < sizeof(buffer)`.
    fn valid(&self) -> bool {
        self.length < 64
    }
}
