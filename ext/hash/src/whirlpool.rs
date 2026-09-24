//! `hash_whirlpool.c`: Barreto and Rijmen's bit-oriented
//! buffer, which `__serialize` writes as it stands (`q8b32iib64.`), over
//! the eight circulant tables `php_hash_whirlpool_tables.h` spells out,
//! here each a rotation of the first, which is built from the S-box.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::algos::{Slot, State};

/// The S-box, the first byte of each row of `C0`.
#[rustfmt::skip]
const SBOX: [u8; 256] = [
    0x18, 0x23, 0xc6, 0xe8, 0x87, 0xb8, 0x01, 0x4f, 0x36, 0xa6, 0xd2, 0xf5, 0x79, 0x6f, 0x91, 0x52,
    0x60, 0xbc, 0x9b, 0x8e, 0xa3, 0x0c, 0x7b, 0x35, 0x1d, 0xe0, 0xd7, 0xc2, 0x2e, 0x4b, 0xfe, 0x57,
    0x15, 0x77, 0x37, 0xe5, 0x9f, 0xf0, 0x4a, 0xda, 0x58, 0xc9, 0x29, 0x0a, 0xb1, 0xa0, 0x6b, 0x85,
    0xbd, 0x5d, 0x10, 0xf4, 0xcb, 0x3e, 0x05, 0x67, 0xe4, 0x27, 0x41, 0x8b, 0xa7, 0x7d, 0x95, 0xd8,
    0xfb, 0xee, 0x7c, 0x66, 0xdd, 0x17, 0x47, 0x9e, 0xca, 0x2d, 0xbf, 0x07, 0xad, 0x5a, 0x83, 0x33,
    0x63, 0x02, 0xaa, 0x71, 0xc8, 0x19, 0x49, 0xd9, 0xf2, 0xe3, 0x5b, 0x88, 0x9a, 0x26, 0x32, 0xb0,
    0xe9, 0x0f, 0xd5, 0x80, 0xbe, 0xcd, 0x34, 0x48, 0xff, 0x7a, 0x90, 0x5f, 0x20, 0x68, 0x1a, 0xae,
    0xb4, 0x54, 0x93, 0x22, 0x64, 0xf1, 0x73, 0x12, 0x40, 0x08, 0xc3, 0xec, 0xdb, 0xa1, 0x8d, 0x3d,
    0x97, 0x00, 0xcf, 0x2b, 0x76, 0x82, 0xd6, 0x1b, 0xb5, 0xaf, 0x6a, 0x50, 0x45, 0xf3, 0x30, 0xef,
    0x3f, 0x55, 0xa2, 0xea, 0x65, 0xba, 0x2f, 0xc0, 0xde, 0x1c, 0xfd, 0x4d, 0x92, 0x75, 0x06, 0x8a,
    0xb2, 0xe6, 0x0e, 0x1f, 0x62, 0xd4, 0xa8, 0x96, 0xf9, 0xc5, 0x25, 0x59, 0x84, 0x72, 0x39, 0x4c,
    0x5e, 0x78, 0x38, 0x8c, 0xd1, 0xa5, 0xe2, 0x61, 0xb3, 0x21, 0x9c, 0x1e, 0x43, 0xc7, 0xfc, 0x04,
    0x51, 0x99, 0x6d, 0x0d, 0xfa, 0xdf, 0x7e, 0x24, 0x3b, 0xab, 0xce, 0x11, 0x8f, 0x4e, 0xb7, 0xeb,
    0x3c, 0x81, 0x94, 0xf7, 0xb9, 0x13, 0x2c, 0xd3, 0xe7, 0x6e, 0xc4, 0x03, 0x56, 0x44, 0x7f, 0xa9,
    0x2a, 0xbb, 0xc1, 0x53, 0xdc, 0x0b, 0x9d, 0x6c, 0x31, 0x74, 0xf6, 0x46, 0xac, 0x89, 0x14, 0xe1,
    0x16, 0x3a, 0x69, 0x09, 0x70, 0xb6, 0xd0, 0xed, 0xcc, 0x42, 0x98, 0xa4, 0x28, 0x5c, 0xf8, 0x86,
];

/// `x * y` in GF(2^8) modulo Whirlpool's `x^8 + x^4 + x^3 + x^2 + 1`.
const fn mul(mut x: u8, mut y: u8) -> u8 {
    let mut out = 0u8;
    while y != 0 {
        if y & 1 != 0 {
            out ^= x;
        }
        x = (x << 1) ^ if x & 0x80 != 0 { 0x1d } else { 0 };
        y >>= 1;
    }
    out
}

/// `C0`: each S-box byte times the MDS row `1 1 4 1 8 5 2 9`; `Ck` is
/// `C0` rotated right by `8k` bits.
const C0: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut x = 0;
    while x < 256 {
        let s = SBOX[x];
        let row = [1, 1, 4, 1, 8, 5, 2, 9];
        let mut word = 0u64;
        let mut i = 0;
        while i < 8 {
            word = (word << 8) | mul(s, row[i]) as u64;
            i += 1;
        }
        table[x] = word;
        x += 1;
    }
    table
};

/// One round of the cipher on `words` under `key`: `L[i]` is the eight
/// table rows its column bytes pick.
fn round(words: &[u64; 8], key: &[u64; 8]) -> [u64; 8] {
    let mut out = *key;
    for (i, word) in out.iter_mut().enumerate() {
        for k in 0..8 {
            let byte = (words[(i + 8 - k) % 8] >> (56 - 8 * k)) & 0xff;
            *word ^= C0[byte as usize].rotate_right(8 * k as u32);
        }
    }
    out
}

/// `PHP_WHIRLPOOL_CTX`.
///
/// ```php
/// $c = hash_init('whirlpool'); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
/// echo hash('whirlpool', ''), "\n", hash('whirlpool', 'abc'), "\n", hash_final($c), "\n";
/// ```
/// ```expect
/// 19fa61d75522a4669b44e39c1d2e1726c530232130d407f89afee0964997f7a73e83be698b288febcf88e3e03c4f0757ea8964e59b63d93708b138cc42a66eb3
/// 4e2448a4c6f486bb16b6562c73b4020bf3043e3a731bce721ae1b303d97e6d4c7181eebdb6c57e277d0e34957114cbd6c797fc9d95d8b582d225292076d4eef5
/// 801b63252749bc3a3551a8fca2beec4b2d409adde21672b1d5058eb0e94cf7dcd06e2870d7c6fd64fe79d93a73aa2a3b85aaa14518a0f32780e515e0e75b557f
/// ```
#[derive(Clone)]
pub(crate) struct Whirlpool {
    state: [u64; 8],
    bitlength: [u8; 32],
    /// `buffer.pos` and `buffer.bits`.
    at: [u32; 2],
    data: [u8; 64],
}

impl Default for Whirlpool {
    fn default() -> Whirlpool {
        Whirlpool {
            state: [0; 8],
            bitlength: [0; 32],
            at: [0; 2],
            data: [0; 64],
        }
    }
}

impl Whirlpool {
    /// `WhirlpoolTransform`: ten rounds and Miyaguchi–Preneel.
    fn transform(&mut self) {
        let mut block = [0u64; 8];
        for (word, bytes) in block.iter_mut().zip(self.data.chunks_exact(8)) {
            let mut b = [0u8; 8];
            b.copy_from_slice(bytes);
            *word = u64::from_be_bytes(b);
        }
        let mut key = self.state;
        let mut state: [u64; 8] = std::array::from_fn(|i| block[i] ^ key[i]);
        for r in 1..=10 {
            let mut rc = [0u64; 8];
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&SBOX[8 * (r - 1)..8 * r]);
            rc[0] = u64::from_be_bytes(bytes);
            key = round(&key, &rc);
            state = round(&state, &key);
        }
        for i in 0..8 {
            self.state[i] ^= state[i] ^ block[i];
        }
    }
}

impl State for Whirlpool {
    /// `PHP_WHIRLPOOLUpdate`, bit for bit.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn update(&mut self, source: &[u8]) {
        let mut source_bits = source.len() as u64 * 8;
        let mut carry = 0u32;
        let mut value = source_bits;
        for i in (0..32).rev() {
            if carry == 0 && value == 0 {
                break;
            }
            carry += u32::from(self.bitlength[i]) + (value as u32 & 0xff);
            self.bitlength[i] = carry as u8;
            carry >>= 8;
            value >>= 8;
        }
        let rem = self.at[1] as i32 & 7;
        let mut bits = self.at[1] as i32;
        let mut pos = self.at[0] as usize;
        let mut source_pos = 0;
        while source_bits > 8 {
            let b = u32::from(source[source_pos]);
            self.data[pos] |= (b >> rem) as u8;
            pos += 1;
            bits += 8 - rem;
            if bits == 512 {
                self.transform();
                bits = 0;
                pos = 0;
            }
            self.data[pos] = (b << (8 - rem)) as u8;
            bits += rem;
            source_bits -= 8;
            source_pos += 1;
        }
        let b = if source_bits > 0 {
            let b = u32::from(source[source_pos]);
            self.data[pos] |= (b >> rem) as u8;
            b
        } else {
            0
        };
        if rem as u64 + source_bits < 8 {
            bits += source_bits as i32;
        } else {
            pos += 1;
            bits += 8 - rem;
            source_bits -= (8 - rem) as u64;
            if bits == 512 {
                self.transform();
                bits = 0;
                pos = 0;
            }
            self.data[pos] = (b << (8 - rem)) as u8;
            bits += source_bits as i32;
        }
        self.at = [pos as u32, bits as u32];
    }

    /// `PHP_WHIRLPOOLFinal`.
    fn finish(&mut self) -> Vec<u8> {
        let mut pos = self.at[0] as usize;
        self.data[pos] |= 0x80 >> (self.at[1] & 7);
        pos += 1;
        if pos > 32 {
            self.data[pos..].fill(0);
            self.transform();
            pos = 0;
        }
        self.data[pos..32].fill(0);
        self.data[32..].copy_from_slice(&self.bitlength);
        self.transform();
        self.state.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::Q(&mut self.state),
            Slot::B(&mut self.bitlength),
            Slot::L(&mut self.at),
            Slot::B(&mut self.data),
        ]
    }

    /// `php_whirlpool_unserialize`: `0 <= pos < 64` and `bits` within
    /// the byte `pos` names.
    fn valid(&self) -> bool {
        let (pos, bits) = (i64::from(self.at[0] as i32), i64::from(self.at[1] as i32));
        (0..64).contains(&pos) && bits >= pos * 8 && bits < pos * 8 + 8
    }
}
