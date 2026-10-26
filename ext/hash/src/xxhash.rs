//! `hash_xxhash.c`: XXH32 and XXH64 over `XXH32_state_t` and
//! `XXH64_state_t` as `xxhash.h` lays them out, which `__serialize`
//! writes field by field, ported; and XXH3 at 64 and 128 bits over the
//! `twox-hash` crate, whose state php never serialises.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::hash::Hasher as _;
use std::slice;

use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Value, ValueRef};
use twox_hash::{XxHash3_64, XxHash3_128};

use crate::algos::{Options, Slot, State};
use crate::throwables::Throwables;

const P32: [u32; 5] = [
    0x9E37_79B1,
    0x85EB_CA77,
    0xC2B2_AE3D,
    0x27D4_EB2F,
    0x1656_67B1,
];
const P64: [u64; 5] = [
    0x9E37_79B1_85EB_CA87,
    0xC2B2_AE3D_27D4_EB4F,
    0x1656_67B1_9E37_79F9,
    0x85EB_CA77_C2B2_AE63,
    0x27D4_EB2F_1656_67C5,
];

fn round32(acc: u32, input: u32) -> u32 {
    acc.wrapping_add(input.wrapping_mul(P32[1]))
        .rotate_left(13)
        .wrapping_mul(P32[0])
}

fn round64(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P64[1]))
        .rotate_left(31)
        .wrapping_mul(P64[0])
}

/// `XXH32_state_t`: `total_len_32`, `large_len`, `v[4]`, `mem32[4]`,
/// `memsize` and `reserved`.
///
/// ```php
/// foreach (['xxh32', 'xxh64', 'xxh3', 'xxh128'] as $a) {
///     $c = hash_init($a, 0, '', ['seed' => 7]); hash_update($c, str_repeat('ab', 40)); hash_update($c, 'c');
///     echo $a, ' ', hash($a, ''), ' ', hash($a, str_repeat('x', 300)), ' ', hash_final($c), "\n";
/// }
/// $secret = str_repeat('0123456789', 14);
/// echo hash('xxh3', 'abc', false, ['secret' => $secret]), ' ', hash('xxh128', 'abc', false, ['secret' => $secret]), "\n";
/// try { hash('xxh3', 'x', false, ['seed' => 1, 'secret' => $secret]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { hash('xxh128', 'x', false, ['secret' => 'short']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize(hash_init('xxh3')); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// xxh32 02cc5d05 7d4c4392 9acf238a
/// xxh64 ef46db3751d8e999 8377db93c80fd5bf 4d8d926cfa2774e0
/// xxh3 2d06800538d394c2 a5d1b4607dc83554 651e67fd172bc97a
/// xxh128 99aa06d3014798d86001c324468d497f 125237ea2ad7bc7aa5d1b4607dc83554 432c2c482732d0bdada92056eba792f4
/// c2316825a070030d bf3b0d0f5eb93fd5c2316825a070030d
/// xxh3: Only one of seed or secret is to be passed for initialization
/// xxh128: Secret length must be >= 136 bytes, 5 bytes passed
/// HashContext for algorithm "xxh3" cannot be serialized
/// ```
#[derive(Clone)]
pub(crate) struct Xxh32 {
    total_len_32: u32,
    large_len: u32,
    v: [u32; 4],
    /// Read and written as the sixteen bytes `xxhash.h` treats it as.
    mem32: [u32; 4],
    memsize: u32,
    reserved: u32,
}

impl Xxh32 {
    /// `PHP_XXH32Init`: `XXH32_reset` at the seed.
    #[allow(clippy::cast_possible_truncation, clippy::unnecessary_wraps)] // `Init`'s signature
    pub(crate) fn init(req: &mut Request, options: Options) -> Result<Box<dyn State>, Thrown> {
        let seed = options.seed(req) as u32;
        Ok(Box::new(Xxh32 {
            total_len_32: 0,
            large_len: 0,
            v: [
                seed.wrapping_add(P32[0]).wrapping_add(P32[1]),
                seed.wrapping_add(P32[1]),
                seed,
                seed.wrapping_sub(P32[0]),
            ],
            mem32: [0; 4],
            memsize: 0,
            reserved: 0,
        }))
    }
}

/// A buffer of words as the bytes `xxhash.h` treats them as.
fn bytes_of<const N: usize>(words: &[u32]) -> [u8; N] {
    let mut out = [0u8; N];
    for (chunk, word) in out.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn le64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

impl State for Xxh32 {
    /// `XXH32_update`.
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        self.total_len_32 = self.total_len_32.wrapping_add(input.len() as u32);
        self.large_len |= u32::from(input.len() >= 16 || self.total_len_32 >= 16);
        let held = self.memsize as usize;
        let mut mem: [u8; 16] = bytes_of(&self.mem32);
        if held + input.len() < 16 {
            mem[held..held + input.len()].copy_from_slice(input);
            self.mem32 = std::array::from_fn(|i| le32(&mem[4 * i..]));
            self.memsize += input.len() as u32;
            return;
        }
        let mut p = 0;
        if held != 0 {
            mem[held..].copy_from_slice(&input[..16 - held]);
            for (i, v) in self.v.iter_mut().enumerate() {
                *v = round32(*v, le32(&mem[4 * i..]));
            }
            p = 16 - held;
            self.memsize = 0;
        }
        while p + 16 <= input.len() {
            for (i, v) in self.v.iter_mut().enumerate() {
                *v = round32(*v, le32(&input[p + 4 * i..]));
            }
            p += 16;
        }
        if p < input.len() {
            mem[..input.len() - p].copy_from_slice(&input[p..]);
            self.memsize = (input.len() - p) as u32;
        }
        self.mem32 = std::array::from_fn(|i| le32(&mem[4 * i..]));
    }

    /// `XXH32_digest` and `XXH32_finalize`.
    fn finish(&mut self) -> Vec<u8> {
        let [v0, v1, v2, v3] = self.v;
        let mut h = if self.large_len != 0 {
            v0.rotate_left(1)
                .wrapping_add(v1.rotate_left(7))
                .wrapping_add(v2.rotate_left(12))
                .wrapping_add(v3.rotate_left(18))
        } else {
            v2.wrapping_add(P32[4])
        };
        h = h.wrapping_add(self.total_len_32);
        let mem: [u8; 16] = bytes_of(&self.mem32);
        let len = (self.memsize & 15) as usize;
        let mut at = 0;
        while at + 4 <= len {
            h = h
                .wrapping_add(le32(&mem[at..]).wrapping_mul(P32[2]))
                .rotate_left(17)
                .wrapping_mul(P32[3]);
            at += 4;
        }
        for &byte in &mem[at..len] {
            h = h
                .wrapping_add(u32::from(byte).wrapping_mul(P32[4]))
                .rotate_left(11)
                .wrapping_mul(P32[0]);
        }
        h ^= h >> 15;
        h = h.wrapping_mul(P32[1]);
        h ^= h >> 13;
        h = h.wrapping_mul(P32[2]);
        h ^= h >> 16;
        h.to_be_bytes().to_vec()
    }

    /// `llllllllllll`.
    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::L(slice::from_mut(&mut self.total_len_32)),
            Slot::L(slice::from_mut(&mut self.large_len)),
            Slot::L(&mut self.v),
            Slot::L(&mut self.mem32),
            Slot::L(slice::from_mut(&mut self.memsize)),
            Slot::L(slice::from_mut(&mut self.reserved)),
        ]
    }

    /// `php_hash_xxh32_unserialize`: `memsize < 16`.
    fn valid(&self) -> bool {
        self.memsize < 16
    }
}

/// `XXH64_state_t`: `total_len`, `v[4]` and `mem64[4]`, then
/// `memsize`, `reserved32` and `reserved64`.
#[derive(Clone)]
pub(crate) struct Xxh64 {
    total_len: u64,
    v: [u64; 4],
    /// Read and written as the thirty-two bytes `xxhash.h` treats it as.
    mem64: [u64; 4],
    memsize: u32,
    reserved32: u32,
    reserved64: u64,
}

impl Xxh64 {
    /// `PHP_XXH64Init`: `XXH64_reset` at the seed.
    #[allow(clippy::unnecessary_wraps)] // `Init`'s signature
    pub(crate) fn init(req: &mut Request, options: Options) -> Result<Box<dyn State>, Thrown> {
        let seed = options.seed(req);
        Ok(Box::new(Xxh64 {
            total_len: 0,
            v: [
                seed.wrapping_add(P64[0]).wrapping_add(P64[1]),
                seed.wrapping_add(P64[1]),
                seed,
                seed.wrapping_sub(P64[0]),
            ],
            mem64: [0; 4],
            memsize: 0,
            reserved32: 0,
            reserved64: 0,
        }))
    }

    fn mem(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(8).zip(&self.mem64) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn set_mem(&mut self, mem: &[u8; 32]) {
        for (word, chunk) in self.mem64.iter_mut().zip(mem.chunks_exact(8)) {
            *word = le64(chunk);
        }
    }
}

impl State for Xxh64 {
    /// `XXH64_update`.
    #[allow(clippy::cast_possible_truncation)]
    fn update(&mut self, input: &[u8]) {
        self.total_len = self.total_len.wrapping_add(input.len() as u64);
        let held = self.memsize as usize;
        let mut mem = self.mem();
        if held + input.len() < 32 {
            mem[held..held + input.len()].copy_from_slice(input);
            self.set_mem(&mem);
            self.memsize += input.len() as u32;
            return;
        }
        let mut p = 0;
        if held != 0 {
            mem[held..].copy_from_slice(&input[..32 - held]);
            for (i, v) in self.v.iter_mut().enumerate() {
                *v = round64(*v, le64(&mem[8 * i..]));
            }
            p = 32 - held;
            self.memsize = 0;
        }
        while p + 32 <= input.len() {
            for (i, v) in self.v.iter_mut().enumerate() {
                *v = round64(*v, le64(&input[p + 8 * i..]));
            }
            p += 32;
        }
        if p < input.len() {
            mem[..input.len() - p].copy_from_slice(&input[p..]);
            self.memsize = (input.len() - p) as u32;
        }
        self.set_mem(&mem);
    }

    /// `XXH64_digest` and `XXH64_finalize`.
    #[allow(clippy::cast_possible_truncation)]
    fn finish(&mut self) -> Vec<u8> {
        let total = self.total_len;
        let [v0, v1, v2, v3] = self.v;
        let mut h = if total >= 32 {
            let mut h = v0
                .rotate_left(1)
                .wrapping_add(v1.rotate_left(7))
                .wrapping_add(v2.rotate_left(12))
                .wrapping_add(v3.rotate_left(18));
            for v in [v0, v1, v2, v3] {
                h = (h ^ round64(0, v))
                    .wrapping_mul(P64[0])
                    .wrapping_add(P64[3]);
            }
            h
        } else {
            v2.wrapping_add(P64[4])
        };
        h = h.wrapping_add(total);
        let mem = self.mem();
        let len = (total & 31) as usize;
        let mut at = 0;
        while at + 8 <= len {
            h ^= round64(0, le64(&mem[at..]));
            h = h.rotate_left(27).wrapping_mul(P64[0]).wrapping_add(P64[3]);
            at += 8;
        }
        if at + 4 <= len {
            h ^= u64::from(le32(&mem[at..])).wrapping_mul(P64[0]);
            h = h.rotate_left(23).wrapping_mul(P64[1]).wrapping_add(P64[2]);
            at += 4;
        }
        for &byte in &mem[at..len] {
            h ^= u64::from(byte).wrapping_mul(P64[4]);
            h = h.rotate_left(11).wrapping_mul(P64[0]);
        }
        h ^= h >> 33;
        h = h.wrapping_mul(P64[1]);
        h ^= h >> 29;
        h = h.wrapping_mul(P64[2]);
        h ^= h >> 32;
        h.to_be_bytes().to_vec()
    }

    /// `qqqqqqqqqllq`.
    fn slots(&mut self) -> Vec<Slot<'_>> {
        vec![
            Slot::Q(slice::from_mut(&mut self.total_len)),
            Slot::Q(&mut self.v),
            Slot::Q(&mut self.mem64),
            Slot::L(slice::from_mut(&mut self.memsize)),
            Slot::L(slice::from_mut(&mut self.reserved32)),
            Slot::Q(slice::from_mut(&mut self.reserved64)),
        ]
    }

    /// `php_hash_xxh64_unserialize`: `memsize < 32`.
    fn valid(&self) -> bool {
        self.memsize < 32
    }
}

/// `PHP_XXH3_64_CTX` or `PHP_XXH3_128_CTX`, the one `_PHP_XXH3_Init`
/// is handed.
#[derive(Clone, Copy)]
pub(crate) enum Width {
    Bits64,
    Bits128,
}

impl Width {
    /// The name `_PHP_XXH3_Init`'s messages carry.
    fn name(self) -> &'static str {
        match self {
            Width::Bits64 => "xxh3",
            Width::Bits128 => "xxh128",
        }
    }
}

/// `XXH3_state_t` at either width.
#[derive(Clone)]
enum Hasher3 {
    Bits64(XxHash3_64),
    Bits128(XxHash3_128),
}

/// XXH3 at 64 or 128 bits. `twox-hash`'s stream digests an input of
/// 240 bytes or fewer under its default secret, where
/// `XXH3_64bits_digest` takes `XXH3_64bits_withSecret`'s path, so with
/// a secret the first 241 bytes are kept and a short input is hashed
/// in one shot.
#[derive(Clone)]
pub(crate) struct Xxh3 {
    hasher: Hasher3,
    secret: Option<Box<[u8]>>,
    head: Vec<u8>,
}

/// `XXH3_SECRET_SIZE_MIN`, which `twox-hash`'s constructors hold a
/// secret to, for the message.
const SECRET_MIN: usize = 136;

impl Xxh3 {
    /// `_PHP_XXH3_Init`: a seed or a secret, not both; a secret of at
    /// least `XXH3_SECRET_SIZE_MIN` bytes, anything past 256 dropped
    /// with a warning.
    fn init(req: &mut Request, options: Options, width: Width) -> Result<Box<dyn State>, Thrown> {
        let name = width.name();
        let seed = options.get(b"seed");
        let secret = options.get(b"secret");
        if seed.is_some() && secret.is_some() {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                &format!("{name}: Only one of seed or secret is to be passed for initialization"),
            ));
        }
        let Some(secret) = secret else {
            let seed = match seed.as_ref().map(Value::as_ref) {
                Some(ValueRef::Int(n)) => n.cast_unsigned(),
                Some(_) => {
                    docref(
                        req,
                        Level::Deprecated,
                        "Passing a seed of a type other than int is deprecated because it is ignored",
                    );
                    0
                }
                None => 0,
            };
            let hasher = match width {
                Width::Bits64 => Hasher3::Bits64(XxHash3_64::with_seed(seed)),
                Width::Bits128 => Hasher3::Bits128(XxHash3_128::with_seed(seed)),
            };
            return Ok(Box::new(Xxh3 {
                hasher,
                secret: None,
                head: Vec::new(),
            }));
        };
        if !matches!(secret.as_ref(), ValueRef::Str(_)) {
            docref(
                req,
                Level::Deprecated,
                "Passing a secret of a type other than string is deprecated because it implicitly converts to a string, potentially hiding bugs",
            );
        }
        let mut bytes = with_string(req, &secret, <[u8]>::to_vec)?;
        let given = bytes.len();
        if given > 256 {
            bytes.truncate(256);
            docref(
                req,
                Level::Warning,
                &format!("{name}: Secret content exceeding 256 bytes discarded"),
            );
        }
        let secret: Box<[u8]> = bytes.into_boxed_slice();
        // `XXH3_64bits_reset_withSecret` seeds with zero; the crate's
        // refusal of a secret under `XXH3_SECRET_SIZE_MIN` is php's.
        let hasher = match width {
            Width::Bits64 => XxHash3_64::with_seed_and_secret(0, secret.clone())
                .ok()
                .map(Hasher3::Bits64),
            Width::Bits128 => XxHash3_128::with_seed_and_secret(0, secret.clone())
                .ok()
                .map(Hasher3::Bits128),
        };
        let Some(hasher) = hasher else {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                &format!(
                    "{name}: Secret length must be >= {SECRET_MIN} bytes, {given} bytes passed"
                ),
            ));
        };
        Ok(Box::new(Xxh3 {
            hasher,
            secret: Some(secret),
            head: Vec::new(),
        }))
    }

    pub(crate) fn init_64(req: &mut Request, options: Options) -> Result<Box<dyn State>, Thrown> {
        Xxh3::init(req, options, Width::Bits64)
    }

    pub(crate) fn init_128(req: &mut Request, options: Options) -> Result<Box<dyn State>, Thrown> {
        Xxh3::init(req, options, Width::Bits128)
    }
}

impl State for Xxh3 {
    fn update(&mut self, input: &[u8]) {
        match &mut self.hasher {
            Hasher3::Bits64(hasher) => hasher.write(input),
            Hasher3::Bits128(hasher) => hasher.write(input),
        }
        if self.secret.is_some() && self.head.len() <= 240 {
            let take = (241 - self.head.len()).min(input.len());
            self.head.extend_from_slice(&input[..take]);
        }
    }

    /// `XXH64_canonicalFromHash` and `XXH128_canonicalFromHash`:
    /// big-endian, the high half first.
    fn finish(&mut self) -> Vec<u8> {
        let short = self.secret.as_deref().filter(|_| self.head.len() <= 240);
        match (&self.hasher, short) {
            (Hasher3::Bits64(hasher), Some(secret)) => {
                XxHash3_64::oneshot_with_secret(secret, &self.head)
                    .unwrap_or_else(|_| hasher.finish())
                    .to_be_bytes()
                    .to_vec()
            }
            (Hasher3::Bits128(hasher), Some(secret)) => {
                XxHash3_128::oneshot_with_secret(secret, &self.head)
                    .unwrap_or_else(|_| hasher.finish_128())
                    .to_be_bytes()
                    .to_vec()
            }
            (Hasher3::Bits64(hasher), None) => hasher.finish().to_be_bytes().to_vec(),
            (Hasher3::Bits128(hasher), None) => hasher.finish_128().to_be_bytes().to_vec(),
        }
    }
}
