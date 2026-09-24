//! The 64-byte Merkle–Damgård contexts php writes one way
//! (`Z/ext/hash/hash_md.c`, `hash_sha.c`, `hash_ripemd.c`,
//! `Z/ext/standard/sha1.c`): `state`, a bit count in two words and the
//! block not yet full, `l<W>l2b64.`, each block through its algorithm's
//! compression. SHA-1's is ext/standard's, SHA-224 and SHA-256's is the
//! `sha2` crate's; MD4's and RIPEMD's are php's own, ported. MD5 is
//! ext/standard's `PHP_MD5_CTX` and MD2 is php's. The witnesses hash a
//! string whole and in two pieces, so the running block is held too.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext_standard::{md5, sha1};

use crate::algos::{Slot, State, le_words};

/// `PADDING`: the one bit and the zeros after it.
pub(crate) const PADDING: [u8; 128] = {
    let mut pad = [0u8; 128];
    pad[0] = 0x80;
    pad
};

/// One state of the family: `W` words under the
/// compression `K` names.
#[derive(Clone)]
pub(crate) struct Md<const W: usize, const K: u8> {
    state: [u32; W],
    count: [u32; 2],
    buffer: [u8; 64],
}

const MD4: u8 = 0;
const SHA1: u8 = 1;
const SHA224: u8 = 2;
const SHA256: u8 = 3;
const RIPEMD: u8 = 4;

/// `md4`.
///
/// ```php
/// foreach (['md4', 'sha1', 'sha224', 'sha256', 'ripemd128', 'ripemd160', 'ripemd256', 'ripemd320'] as $a) {
///     $c = hash_init($a); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, ''), ' ', hash($a, 'abc'), ' ', hash_final($c), "\n";
/// }
/// ```
/// ```expect
/// md4 31d6cfe0d16ae931b73c59d7e0c089c0 a448017aaf21d8525fc10ae87aa6729d d8ddf1921867477988dcad47ddfa146b
/// sha1 da39a3ee5e6b4b0d3255bfef95601890afd80709 a9993e364706816aba3e25717850c26c9cd0d89d 9ceb6d6d493deb1e9c3ac0bd0f555636a52af35b
/// sha224 d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f 23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7 0fff521ecf5e71011daea8599f03fadd8429200c066e0135bbdf7078
/// sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad 235a2869bb3d0d293e50f1f2ac20c7ba582b04860fa1e7372505385d64c68342
/// ripemd128 cdf26213a150dc3ecb610f18f6b38b46 c14a12199c66e4ba84636b0f69144c77 62ad7eefc05c0ea7d5cdb35aabede606
/// ripemd160 9c1185a5c5e9fc54612808977ee8f548b2258d31 8eb208f7e05d987a9b044a8e98c6b087f15a0bfc 0ca7aa1806eda706b16b747f0c4afe89afa8635a
/// ripemd256 02ba4c4e5f8ecd1877fc52d64d30e37a2d9774fb1e5d026380ae0168e3c5522d afbd6e228b9d8cbbcef5ca2d03e6dba10ac0bc7dcbe4680e1e42d2e975459b65 51b0ed5a16d2b090e731578defb1d9a8891647b156dbaf0c0cdd9e8d1a4a0f17
/// ripemd320 22d65d5661536cdc75c1fdf5c6de7b41b9f27325ebc61e8557177d705a0ec880151c3a32a00899b8 de4c01b3054f8930a79d09ae738e92301e5a17085beffdc1b8d116713e74f82fa942d64cdbc4682d 13d2d82e74d7b152356db97bf29af8fc7d535d9c5676e3a4d26ebb2fe1b785764bba346f70bba84f
/// ```
pub(crate) type Md4 = Md<4, MD4>;
pub(crate) type Sha1 = Md<5, SHA1>;
pub(crate) type Sha224 = Md<8, SHA224>;
pub(crate) type Sha256 = Md<8, SHA256>;
pub(crate) type Ripemd<const W: usize> = Md<W, RIPEMD>;

impl<const W: usize, const K: u8> Default for Md<W, K> {
    /// Each `Init`'s constants.
    fn default() -> Md<W, K> {
        const IV: [u32; 5] = [
            0x6745_2301,
            0xEFCD_AB89,
            0x98BA_DCFE,
            0x1032_5476,
            0xC3D2_E1F0,
        ];
        const SHA224_IV: [u32; 8] = [
            0xc105_9ed8,
            0x367c_d507,
            0x3070_dd17,
            0xf70e_5939,
            0xffc0_0b31,
            0x6858_1511,
            0x64f9_8fa7,
            0xbefa_4fa4,
        ];
        const SHA256_IV: [u32; 8] = [
            0x6a09_e667,
            0xbb67_ae85,
            0x3c6e_f372,
            0xa54f_f53a,
            0x510e_527f,
            0x9b05_688c,
            0x1f83_d9ab,
            0x5be0_cd19,
        ];
        const RIPEMD_HIGH: [u32; 5] = [
            0x7654_3210,
            0xFEDC_BA98,
            0x89AB_CDEF,
            0x0123_4567,
            0x3C2D_1E0F,
        ];
        let mut state = [0u32; W];
        match K {
            SHA224 => state.copy_from_slice(&SHA224_IV[..W]),
            SHA256 => state.copy_from_slice(&SHA256_IV[..W]),
            RIPEMD if W == 8 => {
                state[..4].copy_from_slice(&IV[..4]);
                state[4..].copy_from_slice(&RIPEMD_HIGH[..4]);
            }
            RIPEMD if W == 10 => {
                state[..5].copy_from_slice(&IV);
                state[5..].copy_from_slice(&RIPEMD_HIGH);
            }
            _ => state.copy_from_slice(&IV[..W]),
        }
        Md {
            state,
            count: [0; 2],
            buffer: [0; 64],
        }
    }
}

impl<const W: usize, const K: u8> Md<W, K> {
    /// The SHA family writes its words and its count big-endian.
    const BIG: bool = matches!(K, SHA1 | SHA224 | SHA256);

    fn compress(state: &mut [u32; W], block: &[u8]) {
        match K {
            MD4 => md4(state, block),
            SHA1 => {
                if let Ok(state) = <&mut [u32; 5]>::try_from(&mut state[..]) {
                    sha1::transform(state, block);
                }
            }
            SHA224 | SHA256 => {
                if let (Ok(state), Ok(block)) = (
                    <&mut [u32; 8]>::try_from(&mut state[..]),
                    <[u8; 64]>::try_from(block),
                ) {
                    sha2::block_api::compress256(state, &[block]);
                }
            }
            _ => ripemd(state, block),
        }
    }
}

impl<const W: usize, const K: u8> State for Md<W, K> {
    /// `PHP_MD4Update`'s shape, which the family shares: the bit count
    /// first, then the block topped up and every whole block after it.
    fn update(&mut self, input: &[u8]) {
        let index = ((self.count[0] >> 3) & 0x3f) as usize;
        let bits = (u64::from(self.count[1]) << 32 | u64::from(self.count[0]))
            .wrapping_add((input.len() as u64).wrapping_mul(8));
        self.count = [bits as u32, (bits >> 32) as u32];
        let state = &mut self.state;
        feed(&mut self.buffer, index, input, |block| {
            Self::compress(state, block);
        });
    }

    fn finish(&mut self) -> Vec<u8> {
        let [low, high] = self.count;
        let bits: Vec<u8> = if Self::BIG {
            [high.to_be_bytes(), low.to_be_bytes()].concat()
        } else {
            [low.to_le_bytes(), high.to_le_bytes()].concat()
        };
        let index = ((low >> 3) & 0x3f) as usize;
        let pad = if index < 56 { 56 - index } else { 120 - index };
        self.update(&PADDING[..pad]);
        self.update(&bits);
        let mut out: Vec<u8> = self
            .state
            .iter()
            .flat_map(|w| {
                if Self::BIG {
                    w.to_be_bytes()
                } else {
                    w.to_le_bytes()
                }
            })
            .collect();
        out.truncate(if K == SHA224 { 28 } else { 4 * W });
        out
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::L(&mut self.state),
            Slot::L(&mut self.count),
            Slot::B(&mut self.buffer),
        ]
    }
}

/// The buffered walk of every `Update` php writes for a block hash:
/// `buffer` topped up from `index` and handed to `block`, then each
/// whole block of `input`, then what is left copied in.
///
/// ```php
/// foreach (['sha256', 'sha512'] as $a) {
///     $c = hash_init($a); foreach ([63, 1, 64, 127, 129, 0, 5] as $n) { hash_update($c, str_repeat('z', $n)); }
///     var_dump(hash_final($c) === hash($a, str_repeat('z', 389)));
/// }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn feed<const N: usize>(
    buffer: &mut [u8; N],
    index: usize,
    input: &[u8],
    mut block: impl FnMut(&[u8]),
) {
    let part = N - index;
    let (mut i, mut index) = (0, index);
    if input.len() >= part {
        buffer[index..].copy_from_slice(&input[..part]);
        block(&buffer[..]);
        i = part;
        while i + N <= input.len() {
            block(&input[i..i + N]);
            i += N;
        }
        index = 0;
    }
    let rest = input.len() - i;
    buffer[index..index + rest].copy_from_slice(&input[i..]);
}

/// `MD4Transform`: three rounds of sixteen, the names RFC 1320's.
#[allow(clippy::many_single_char_names)]
fn md4(state: &mut [u32], block: &[u8]) {
    const ORDER: [[usize; 16]; 3] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        [0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15],
        [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15],
    ];
    const SHIFT: [[u32; 4]; 3] = [[3, 7, 11, 19], [3, 5, 9, 13], [3, 9, 11, 15]];
    let x: [u32; 16] = le_words(block);
    let mut v = [state[0], state[1], state[2], state[3]];
    for round in 0..3 {
        for i in 0..16 {
            let [a, b, c, d] = [
                v[(16 - i) % 4],
                v[(17 - i) % 4],
                v[(18 - i) % 4],
                v[(19 - i) % 4],
            ];
            let (f, k) = match round {
                0 => (d ^ (b & (c ^ d)), 0),
                1 => ((b & (c | d)) | (c & d), 0x5A82_7999),
                _ => (b ^ c ^ d, 0x6ED9_EBA1),
            };
            v[(16 - i) % 4] = a
                .wrapping_add(f)
                .wrapping_add(x[ORDER[round][i]])
                .wrapping_add(k)
                .rotate_left(SHIFT[round][i % 4]);
        }
    }
    for (word, add) in state.iter_mut().zip(v) {
        *word = word.wrapping_add(add);
    }
}

/// `RIPEMD128Transform`, `RIPEMD160Transform`, `RIPEMD256Transform` and
/// `RIPEMD320Transform` as one: two lines of four or five words, the
/// wide ones for 160 and 320, and the two lines kept apart, their words
/// swapped after each round, for 256 and 320.
#[allow(clippy::many_single_char_names)]
fn ripemd(state: &mut [u32], block: &[u8]) {
    const R: [u8; 80] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 7, 4, 13, 1, 10, 6, 15, 3, 12, 0, 9,
        5, 2, 14, 11, 8, 3, 10, 14, 4, 9, 15, 8, 1, 2, 7, 0, 6, 13, 11, 5, 12, 1, 9, 11, 10, 0, 8,
        12, 4, 13, 3, 7, 15, 14, 5, 6, 2, 4, 0, 5, 9, 7, 12, 2, 10, 14, 1, 3, 8, 11, 6, 15, 13,
    ];
    const RR: [u8; 80] = [
        5, 14, 7, 0, 9, 2, 11, 4, 13, 6, 15, 8, 1, 10, 3, 12, 6, 11, 3, 7, 0, 13, 5, 10, 14, 15, 8,
        12, 4, 9, 1, 2, 15, 5, 1, 3, 7, 14, 6, 9, 11, 8, 12, 2, 10, 0, 4, 13, 8, 6, 4, 1, 3, 11,
        15, 0, 5, 12, 2, 13, 9, 7, 10, 14, 12, 15, 10, 4, 1, 5, 8, 7, 6, 2, 13, 14, 0, 3, 9, 11,
    ];
    const S: [u8; 80] = [
        11, 14, 15, 12, 5, 8, 7, 9, 11, 13, 14, 15, 6, 7, 9, 8, 7, 6, 8, 13, 11, 9, 7, 15, 7, 12,
        15, 9, 11, 7, 13, 12, 11, 13, 6, 7, 14, 9, 13, 15, 14, 8, 13, 6, 5, 12, 7, 5, 11, 12, 14,
        15, 14, 15, 9, 8, 9, 14, 5, 6, 8, 6, 5, 12, 9, 15, 5, 11, 6, 8, 13, 12, 5, 12, 13, 14, 11,
        8, 5, 6,
    ];
    const SS: [u8; 80] = [
        8, 9, 9, 11, 13, 15, 15, 5, 7, 7, 8, 11, 14, 14, 12, 6, 9, 13, 15, 7, 12, 8, 9, 11, 7, 7,
        12, 7, 6, 15, 13, 11, 9, 7, 15, 11, 8, 6, 6, 14, 12, 13, 5, 14, 13, 13, 7, 5, 15, 5, 8, 11,
        14, 14, 6, 14, 6, 9, 12, 9, 12, 5, 15, 8, 8, 5, 12, 9, 12, 5, 14, 6, 8, 13, 6, 5, 15, 13,
        11, 11,
    ];
    const K: [u32; 5] = [0, 0x5A82_7999, 0x6ED9_EBA1, 0x8F1B_BCDC, 0xA953_FD4E];
    const KK: [u32; 4] = [0x50A2_8BE6, 0x5C4D_D124, 0x6D70_3EF3, 0];
    const KK160: [u32; 5] = [0x50A2_8BE6, 0x5C4D_D124, 0x6D70_3EF3, 0x7A6D_76E9, 0];
    /// The word each round of 256 and 320 swaps between the lines.
    const SWAP4: [usize; 4] = [0, 1, 2, 3];
    const SWAP5: [usize; 5] = [1, 3, 0, 2, 4];
    fn f(n: usize, x: u32, y: u32, z: u32) -> u32 {
        match n {
            0 => x ^ y ^ z,
            1 => (x & y) | (!x & z),
            2 => (x | !y) ^ z,
            3 => (x & z) | (y & !z),
            _ => x ^ (y | !z),
        }
    }
    /// One step of a line: `[a, b, c, d]` or `[a, b, c, d, e]`.
    fn step(v: &mut [u32; 5], wide: bool, f_n: usize, x: u32, k: u32, s: u8) {
        let t = v[0]
            .wrapping_add(f(f_n, v[1], v[2], v[3]))
            .wrapping_add(x)
            .wrapping_add(k)
            .rotate_left(u32::from(s));
        if wide {
            let t = t.wrapping_add(v[4]);
            *v = [v[4], t, v[1], v[2].rotate_left(10), v[3]];
        } else {
            *v = [v[3], t, v[1], v[2], 0];
        }
    }
    let x: [u32; 16] = le_words(block);
    let wide = state.len().is_multiple_of(5);
    let split = state.len() >= 8;
    let n = if wide { 5 } else { 4 };
    let (mut l, mut r) = ([0u32; 5], [0u32; 5]);
    l[..n].copy_from_slice(&state[..n]);
    r[..n].copy_from_slice(if split { &state[n..] } else { &state[..n] });
    for round in 0..n {
        for j in round * 16..round * 16 + 16 {
            step(&mut l, wide, round, x[usize::from(R[j])], K[round], S[j]);
            let kk = if wide { KK160[round] } else { KK[round] };
            step(
                &mut r,
                wide,
                n - 1 - round,
                x[usize::from(RR[j])],
                kk,
                SS[j],
            );
        }
        if split {
            let at = if wide { SWAP5[round] } else { SWAP4[round] };
            std::mem::swap(&mut l[at], &mut r[at]);
        }
    }
    if split {
        for i in 0..n {
            state[i] = state[i].wrapping_add(l[i]);
            state[n + i] = state[n + i].wrapping_add(r[i]);
        }
    } else {
        let old: Vec<u32> = state.to_vec();
        for i in 0..n {
            state[i] = old[(i + 1) % n]
                .wrapping_add(l[(i + 2) % n])
                .wrapping_add(r[(i + 3) % n]);
        }
    }
}

/// ext/standard's `PHP_MD5_CTX`.
///
/// ```php
/// $c = hash_init('md5'); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
/// echo hash('md5', ''), ' ', hash('md5', 'abc'), ' ', hash_final($c), "\n";
/// ```
/// ```expect
/// d41d8cd98f00b204e9800998ecf8427e 900150983cd24fb0d6963f7d28e17f72 0fd134ac5d22e86c6a572183702e754d
/// ```
#[derive(Clone, Default)]
pub(crate) struct Md5(md5::Md5);

impl State for Md5 {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finish(&mut self) -> Vec<u8> {
        self.0.finish().to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::L(&mut self.0.words),
            Slot::B(&mut self.0.buffer),
            Slot::L(&mut self.0.block),
        ]
    }
}

/// `PHP_MD2_CTX`: `state`, `checksum`, `buffer` and `in_buffer`.
///
/// ```php
/// $c = hash_init('md2'); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
/// echo hash('md2', ''), ' ', hash('md2', 'abc'), ' ', hash_final($c), "\n";
/// ```
/// ```expect
/// 8350e5a3e24c153df2275c9f80692773 da853b0d3f88d99b30283a69e6ded6bb c3fb7bb2c81e1bb122e18db16cb0b0d9
/// ```
#[derive(Clone)]
pub(crate) struct Md2 {
    /// `state`, `checksum` and `buffer`, 48, 16 and 16 bytes.
    bytes: [u8; 80],
    in_buffer: [u8; 1],
}

impl Default for Md2 {
    fn default() -> Md2 {
        Md2 {
            bytes: [0; 80],
            in_buffer: [0],
        }
    }
}

/// `MD2_S`, the digits of pi.
#[rustfmt::skip]
const MD2_S: [u8; 256] = [
    41, 46, 67, 201, 162, 216, 124, 1, 61, 54, 84, 161, 236, 240, 6, 19, 98, 167, 5, 243, 192, 199,
    115, 140, 152, 147, 43, 217, 188, 76, 130, 202, 30, 155, 87, 60, 253, 212, 224, 22, 103, 66,
    111, 24, 138, 23, 229, 18, 190, 78, 196, 214, 218, 158, 222, 73, 160, 251, 245, 142, 187, 47,
    238, 122, 169, 104, 121, 145, 21, 178, 7, 63, 148, 194, 16, 137, 11, 34, 95, 33, 128, 127, 93,
    154, 90, 144, 50, 39, 53, 62, 204, 231, 191, 247, 151, 3, 255, 25, 48, 179, 72, 165, 181, 209,
    215, 94, 146, 42, 172, 86, 170, 198, 79, 184, 56, 210, 150, 164, 125, 182, 118, 252, 107, 226,
    156, 116, 4, 241, 69, 157, 112, 89, 100, 113, 135, 32, 134, 91, 207, 101, 230, 45, 168, 2, 27,
    96, 37, 173, 174, 176, 185, 246, 28, 70, 97, 105, 52, 64, 126, 15, 85, 71, 163, 35, 221, 81,
    175, 58, 195, 92, 249, 206, 186, 197, 234, 38, 44, 83, 13, 110, 133, 40, 132, 9, 211, 223, 205,
    244, 65, 129, 77, 82, 106, 220, 55, 200, 108, 193, 171, 250, 36, 225, 123, 8, 12, 189, 177, 74,
    120, 136, 149, 139, 227, 99, 232, 109, 233, 203, 213, 254, 59, 0, 29, 57, 242, 239, 183, 14,
    102, 88, 208, 228, 166, 119, 114, 248, 235, 117, 75, 10, 49, 68, 80, 180, 143, 237, 31, 26,
    219, 153, 141, 51, 159, 17, 131, 20,
];

impl Md2 {
    /// `MD2_Transform`.
    fn transform(&mut self, block: &[u8]) {
        let state = &mut self.bytes;
        for i in 0..16 {
            state[16 + i] = block[i];
            state[32 + i] = block[i] ^ state[i];
        }
        let mut t = 0u8;
        for i in 0..18u8 {
            for byte in state.iter_mut().take(48) {
                *byte ^= MD2_S[usize::from(t)];
                t = *byte;
            }
            t = t.wrapping_add(i);
        }
        let mut t = state[48 + 15];
        for i in 0..16 {
            state[48 + i] ^= MD2_S[usize::from(block[i] ^ t)];
            t = state[48 + i];
        }
    }
}

impl State for Md2 {
    /// `PHP_MD2Update`.
    fn update(&mut self, input: &[u8]) {
        let mut p = 0;
        let held = usize::from(self.in_buffer[0]);
        if held != 0 {
            if held + input.len() < 16 {
                self.bytes[64 + held..64 + held + input.len()].copy_from_slice(input);
                self.in_buffer[0] = (held + input.len()) as u8;
                return;
            }
            self.bytes[64 + held..].copy_from_slice(&input[..16 - held]);
            let buffer: [u8; 16] = self.buffer();
            self.transform(&buffer);
            p = 16 - held;
            self.in_buffer[0] = 0;
        }
        while p + 16 <= input.len() {
            self.transform(&input[p..p + 16]);
            p += 16;
        }
        if p < input.len() {
            let rest = input.len() - p;
            self.bytes[64..64 + rest].copy_from_slice(&input[p..]);
            self.in_buffer[0] = rest as u8;
        }
    }

    /// `PHP_MD2Final`.
    fn finish(&mut self) -> Vec<u8> {
        let held = usize::from(self.in_buffer[0]);
        self.bytes[64 + held..].fill((16 - held) as u8);
        let buffer = self.buffer();
        self.transform(&buffer);
        let checksum: [u8; 16] = self.bytes[48..64].try_into().unwrap_or([0; 16]);
        self.transform(&checksum);
        self.bytes[..16].to_vec()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        let (state, rest) = self.bytes.split_at_mut(48);
        let (checksum, buffer) = rest.split_at_mut(16);
        vec![
            Slot::B(state),
            Slot::B(checksum),
            Slot::B(buffer),
            Slot::B(&mut self.in_buffer),
        ]
    }

    /// `php_md2_unserialize`: `in_buffer < sizeof(buffer)`.
    fn valid(&self) -> bool {
        self.in_buffer[0] < 16
    }
}

impl Md2 {
    fn buffer(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&self.bytes[64..]);
        out
    }
}
