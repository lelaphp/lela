//! The one-word contexts: `hash_adler32.c`, `hash_crc32.c`,
//! `hash_fnv.c` and `hash_joaat.c`, each `l.` or `q.`. `crc32b` walks
//! ext/standard's table, the one `crc32()` walks; `crc32`, bzip2's, and
//! `crc32c`, Castagnoli's, walk tables built here.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::marker::PhantomData;
use std::slice;

use php_ext_standard::crc32;

use crate::algos::{Slot, State};

/// `PHP_ADLER32_CTX`.
///
/// ```php
/// foreach (['adler32', 'crc32', 'crc32b', 'crc32c', 'fnv132', 'fnv1a32', 'fnv164', 'fnv1a64', 'joaat'] as $a) {
///     $c = hash_init($a); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, ''), ' ', hash($a, 'The quick brown fox jumped over the lazy dog.'), ' ', hash_final($c), "\n";
/// }
/// foreach (['adler32', 'crc32', 'crc32c', 'fnv1a64'] as $a) {
///     $c = hash_init($a); hash_update($c, 'abc'); $s = serialize($c); $d = unserialize($s); hash_update($d, 'def');
///     echo $a, ' ', substr($s, strpos($s, 'i:2;') + 4, 40), ' ', var_export(hash_final($d) === hash($a, 'abcdef'), true), "\n";
/// }
/// ```
/// ```expect
/// adler32 00000001 7bf1105e f1501edc
/// crc32 00000000 413a86af ebe91b04
/// crc32b 00000000 82a34642 4b5bbf3b
/// crc32c 00000000 1fe6425d 969bae2b
/// fnv132 811c9dc5 5c5e120c 2899f36c
/// fnv1a32 811c9dc5 47998ae2 4a38e7c2
/// fnv164 cbf29ce484222325 b132b2f7bd0ac10c 0a8b74ffb089b5ac
/// fnv1a64 cbf29ce484222325 ea2e60cf009d2702 fd59095578199362
/// joaat 00000000 37b1a2e4 504d5a9c
/// adler32 a:1:{i:0;i:38600999;}i:3;i:2;i:4;a:0:{}} true
/// crc32 a:1:{i:0;i:-1686944628;}i:3;i:2;i:4;a:0: true
/// crc32c a:1:{i:0;i:-910901176;}i:3;i:2;i:4;a:0:{ true
/// fnv1a64 a:2:{i:0;i:88168267;i:1;i:-417357287;}i: true
/// ```
#[derive(Clone)]
pub(crate) struct Adler32 {
    state: u32,
}

impl Default for Adler32 {
    fn default() -> Adler32 {
        Adler32 { state: 1 }
    }
}

impl State for Adler32 {
    /// `PHP_ADLER32Update`: the two sums reduced when the second nears
    /// overflow, and at the end.
    fn update(&mut self, input: &[u8]) {
        let (mut s0, mut s1) = (self.state & 0xffff, (self.state >> 16) & 0xffff);
        for &byte in input {
            s0 += u32::from(byte);
            s1 += s0;
            if s1 >= 0x7fff_ffff {
                s0 %= 65521;
                s1 %= 65521;
            }
        }
        self.state = (s0 % 65521) + ((s1 % 65521) << 16);
    }

    fn finish(&mut self) -> Vec<u8> {
        self.state.to_be_bytes().to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![Slot::L(slice::from_mut(&mut self.state))]
    }
}

/// A reflected table of `poly`, the shape of `crc32b_table` and
/// `crc32c_table`.
const fn reflected(poly: u32) -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { poly ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

/// `crc32_table`: bzip2's, `0x04C11DB7` unreflected.
const BZIP2: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = (n as u32) << 24;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

const CASTAGNOLI: [u32; 256] = reflected(0x82F6_3B78);

/// One of `hash_crc32.c`'s three walks over `PHP_CRC32_CTX`, and the
/// byte order its `Final` answers in.
pub(crate) trait Walk: Clone + Send + 'static {
    /// `PHP_CRC32<k>Update`: the state after `input`.
    fn update(state: u32, input: &[u8]) -> u32;
    /// `PHP_CRC32LEFinal` or `PHP_CRC32BEFinal` over the inverted state.
    fn bytes(crc: u32) -> [u8; 4];
}

/// `crc32`: bzip2's walk, answered little-endian.
#[derive(Clone)]
pub(crate) struct Bzip2;

impl Walk for Bzip2 {
    fn update(mut state: u32, input: &[u8]) -> u32 {
        for &byte in input {
            state = (state << 8) ^ BZIP2[((state >> 24) ^ u32::from(byte)) as usize];
        }
        state
    }

    fn bytes(crc: u32) -> [u8; 4] {
        crc.to_le_bytes()
    }
}

/// `crc32b`: the walk `crc32()` takes, ext/standard's.
#[derive(Clone)]
pub(crate) struct Ieee;

impl Walk for Ieee {
    fn update(state: u32, input: &[u8]) -> u32 {
        crc32::update(state, input)
    }

    fn bytes(crc: u32) -> [u8; 4] {
        crc.to_be_bytes()
    }
}

/// `crc32c`: Castagnoli's.
#[derive(Clone)]
pub(crate) struct Castagnoli;

impl Walk for Castagnoli {
    fn update(mut state: u32, input: &[u8]) -> u32 {
        for &byte in input {
            state = (state >> 8) ^ CASTAGNOLI[((state ^ u32::from(byte)) & 0xff) as usize];
        }
        state
    }

    fn bytes(crc: u32) -> [u8; 4] {
        crc.to_be_bytes()
    }
}

/// `PHP_CRC32_CTX` under one walk.
#[derive(Clone)]
pub(crate) struct Crc<W> {
    state: u32,
    walk: PhantomData<W>,
}

impl<W> Default for Crc<W> {
    fn default() -> Crc<W> {
        Crc {
            state: u32::MAX,
            walk: PhantomData,
        }
    }
}

impl<W: Walk> State for Crc<W> {
    fn update(&mut self, input: &[u8]) {
        self.state = W::update(self.state, input);
    }

    fn finish(&mut self) -> Vec<u8> {
        W::bytes(!self.state).to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![Slot::L(slice::from_mut(&mut self.state))]
    }
}

/// `PHP_FNV132_CTX`; `A` is FNV-1a's order, the byte before the prime.
#[derive(Clone)]
pub(crate) struct Fnv32<const A: bool> {
    state: u32,
}

impl<const A: bool> Default for Fnv32<A> {
    fn default() -> Fnv32<A> {
        Fnv32 { state: 0x811c_9dc5 }
    }
}

impl<const A: bool> State for Fnv32<A> {
    fn update(&mut self, input: &[u8]) {
        let h = &mut self.state;
        for &byte in input {
            if A {
                *h = (*h ^ u32::from(byte)).wrapping_mul(0x0100_0193);
            } else {
                *h = h.wrapping_mul(0x0100_0193) ^ u32::from(byte);
            }
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        self.state.to_be_bytes().to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![Slot::L(slice::from_mut(&mut self.state))]
    }
}

/// `PHP_FNV164_CTX`.
#[derive(Clone)]
pub(crate) struct Fnv64<const A: bool> {
    state: u64,
}

impl<const A: bool> Default for Fnv64<A> {
    fn default() -> Fnv64<A> {
        Fnv64 {
            state: 0xcbf2_9ce4_8422_2325,
        }
    }
}

impl<const A: bool> State for Fnv64<A> {
    fn update(&mut self, input: &[u8]) {
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let h = &mut self.state;
        for &byte in input {
            if A {
                *h = (*h ^ u64::from(byte)).wrapping_mul(PRIME);
            } else {
                *h = h.wrapping_mul(PRIME) ^ u64::from(byte);
            }
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        self.state.to_be_bytes().to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![Slot::Q(slice::from_mut(&mut self.state))]
    }
}

/// `PHP_JOAAT_CTX`: Jenkins's one-at-a-time, its avalanche at the end.
#[derive(Clone, Default)]
pub(crate) struct Joaat {
    state: u32,
}

impl State for Joaat {
    fn update(&mut self, input: &[u8]) {
        let h = &mut self.state;
        for &byte in input {
            *h = h.wrapping_add(u32::from(byte));
            *h = h.wrapping_add(*h << 10);
            *h ^= *h >> 6;
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut h = self.state;
        h = h.wrapping_add(h << 3);
        h ^= h >> 11;
        h = h.wrapping_add(h << 15);
        h.to_be_bytes().to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![Slot::L(slice::from_mut(&mut self.state))]
    }
}
