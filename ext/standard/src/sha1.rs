//! `Z/ext/standard/sha1.c`: `sha1` and `sha1_file` over FIPS 180-1's
//! digest, the hex and the file read `md5.c`'s are.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, Path};
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::PhpString;

use crate::md5::{digest_value, file_bytes};

/// `PHP_SHA1Init`, `PHP_SHA1Update` and `PHP_SHA1Final` over one
/// buffer: the twenty bytes.
fn digest(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = INIT;
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    for block in message.chunks_exact(64) {
        transform(&mut h, block);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// `PHP_SHA1InitArgs`' five words, which ext/hash's `sha1` starts from.
pub const INIT: [u32; 5] = [
    0x6745_2301,
    0xEFCD_AB89,
    0x98BA_DCFE,
    0x1032_5476,
    0xC3D2_E1F0,
];

/// `SHA1Transform`: one 64-byte block into the five words, which
/// ext/hash's `sha1` state walks over php's `PHP_SHA1_CTX` buffer. The
/// names are FIPS 180-1's.
///
/// ```php
/// var_dump(sha1(str_repeat('a', 55)), sha1(str_repeat('a', 56)), sha1(str_repeat('a', 200)));
/// ```
/// ```expect
/// string(40) "c1c8bbdc22796e28c0e15163d20899b65621d65a"
/// string(40) "c2db330f6083854c99d4b5bfb6e8f29f201be699"
/// string(40) "e61cfffe0d9195a525fc6cf06ca2d77119c24a40"
/// ```
#[allow(clippy::many_single_char_names)]
pub fn transform(h: &mut [u32; 5], block: &[u8]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, word) in w.iter().enumerate() {
        let (f, k) = match i / 20 {
            0 => ((b & c) | (!b & d), 0x5A82_7999),
            1 => (b ^ c ^ d, 0x6ED9_EBA1),
            2 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
        };
        let t = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (word, add) in h.iter_mut().zip([a, b, c, d, e]) {
        *word = word.wrapping_add(add);
    }
}

/// `sha1(string $string, bool $binary = false): string`.
///
/// ```php
/// var_dump(sha1(""), sha1("abc"), sha1("The quick brown fox jumps over the lazy dog"), sha1(str_repeat("a", 1000)), bin2hex(sha1("abc", true)));
/// ```
/// ```expect
/// string(40) "da39a3ee5e6b4b0d3255bfef95601890afd80709"
/// string(40) "a9993e364706816aba3e25717850c26c9cd0d89d"
/// string(40) "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
/// string(40) "291e9a6c66994949b57ba5e650361e98fc36b1ba"
/// string(40) "a9993e364706816aba3e25717850c26c9cd0d89d"
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// echo new ReflectionFunction('sha1'), new ReflectionFunction('sha1_file');
/// ```
/// ```expect
/// Function [ <internal:standard> function sha1 ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> bool $binary = false ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function sha1_file ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <optional> bool $binary = false ]
///   }
///   - Return [ string|false ]
/// }
/// ```
#[php_function]
fn sha1(
    req: &mut Request,
    string: PhpString,
    #[php(default = false)] binary: bool,
) -> Result<PhpString, Thrown> {
    let digest = digest(string.as_bytes());
    digest_value(req, &digest, binary)
}

/// `sha1_file(string $filename, bool $binary = false): string|false`.
///
/// ```php
/// $p = __DIR__ . '/sha1.txt'; $f = fopen($p, 'w'); fwrite($f, str_repeat("xyz", 999)); fclose($f);
/// var_dump(sha1_file($p), bin2hex(sha1_file($p, true)), sha1_file(__DIR__ . '/nope.txt'));
/// ```
/// ```expect
///
/// Warning: sha1_file(%s/nope.txt): Failed to open stream: No such file or directory in %s on line 3
/// string(40) "13d726010b8b0a0cec871360a41b7bc5ba1e5045"
/// string(40) "13d726010b8b0a0cec871360a41b7bc5ba1e5045"
/// bool(false)
/// ```
#[php_function]
fn sha1_file(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] binary: bool,
) -> Result<OrFalse<PhpString>, Thrown> {
    let Some(bytes) = file_bytes(req, filename.0)? else {
        return Ok(OrFalse::False);
    };
    let digest = digest(&bytes);
    digest_value(req, &digest, binary).map(OrFalse::Value)
}
