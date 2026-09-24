//! `Z/ext/standard/md5.c`: `md5` and `md5_file` over RFC 1321's digest,
//! which php carries as Solar Designer's public-domain `PHP_MD5*`, and
//! `make_digest_ex`'s lowercase hex. The file's bytes are read through
//! the streams facet in 1024-byte reads, as php's loop reads them.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::FunctionDecl;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{optional_bool, string_value};
use crate::{argument_count_between, string_argument};

/// The two, in the stub's order.
///
/// ```php
/// echo new ReflectionFunction('md5'), new ReflectionFunction('md5_file');
/// ```
/// ```expect
/// Function [ <internal:standard> function md5 ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> bool $binary = false ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function md5_file ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <optional> bool $binary = false ]
///   }
///   - Return [ string|false ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            stub("md5(string $string, bool $binary = false): string"),
            md5 as Entry,
        ),
        (
            stub("md5_file(string $filename, bool $binary = false): string|false"),
            md5_file,
        ),
    ]
}

/// `PHP_MD5_CTX` in php's own layout, `lo, hi, a, b, c, d`, the block
/// not yet full and the last block's words, which ext/hash's
/// `HashContext` serialises field by field as `llllllb64l16.`
/// (`Z/ext/standard/md5.h:35-41`). `block` is what `SET` leaves on an
/// architecture outside php's unaligned-read list, the oracle's arm64
/// among them.
#[derive(Clone)]
pub struct Md5 {
    /// `lo`, `hi`, `a`, `b`, `c` and `d`.
    pub words: [u32; 6],
    /// `buffer`.
    pub buffer: [u8; 64],
    /// `block`.
    pub block: [u32; 16],
}

/// The per-round shifts and `floor(abs(sin(i + 1)) * 2^32)`.
const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];
const K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

impl Default for Md5 {
    fn default() -> Md5 {
        Md5::new()
    }
}

impl Md5 {
    /// `PHP_MD5Init`.
    #[must_use]
    pub fn new() -> Md5 {
        Md5 {
            words: [0, 0, 0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buffer: [0; 64],
            block: [0; 16],
        }
    }

    /// `body`: 64-byte blocks through the four rounds, each block's
    /// words left in `block` as `SET` leaves them; the names are the
    /// RFC's.
    #[allow(clippy::many_single_char_names)]
    fn body(&mut self, data: &[u8]) {
        let [_, _, mut a, mut b, mut c, mut d] = self.words;
        for chunk in data.chunks_exact(64) {
            for (i, word) in self.block.iter_mut().enumerate() {
                *word = u32::from_le_bytes([
                    chunk[4 * i],
                    chunk[4 * i + 1],
                    chunk[4 * i + 2],
                    chunk[4 * i + 3],
                ]);
            }
            let saved = [a, b, c, d];
            for i in 0..64 {
                let (f, g) = match i / 16 {
                    0 => ((b & c) | (!b & d), i),
                    1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    2 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let rotated = a
                    .wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(self.block[g])
                    .rotate_left(S[i]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(rotated);
            }
            a = a.wrapping_add(saved[0]);
            b = b.wrapping_add(saved[1]);
            c = c.wrapping_add(saved[2]);
            d = d.wrapping_add(saved[3]);
        }
        self.words[2..].copy_from_slice(&[a, b, c, d]);
    }

    /// `PHP_MD5Update`: `lo` counts bytes in 29 bits and carries into
    /// `hi`, which takes `size >> 29` truncated as C's `uint32_t` does.
    #[allow(clippy::cast_possible_truncation)]
    pub fn update(&mut self, mut data: &[u8]) {
        let saved = self.words[0];
        let size = data.len() as u64;
        let lo = u32::try_from((u64::from(saved) + size) & 0x1fff_ffff).unwrap_or(0);
        self.words[0] = lo;
        if lo < saved {
            self.words[1] = self.words[1].wrapping_add(1);
        }
        self.words[1] = self.words[1].wrapping_add((size >> 29) as u32);
        let used = (saved & 0x3f) as usize;
        if used != 0 {
            let free = 64 - used;
            if data.len() < free {
                self.buffer[used..used + data.len()].copy_from_slice(data);
                return;
            }
            self.buffer[used..].copy_from_slice(&data[..free]);
            data = &data[free..];
            let buffer = self.buffer;
            self.body(&buffer);
        }
        let whole = data.len() & !0x3f;
        self.body(&data[..whole]);
        self.buffer[..data.len() - whole].copy_from_slice(&data[whole..]);
    }

    /// `PHP_MD5Final`: the sixteen bytes.
    pub fn finish(&mut self) -> [u8; 16] {
        let mut used = (self.words[0] & 0x3f) as usize;
        self.buffer[used] = 0x80;
        used += 1;
        if 64 - used < 8 {
            self.buffer[used..].fill(0);
            let buffer = self.buffer;
            self.body(&buffer);
            used = 0;
        }
        self.buffer[used..56].fill(0);
        self.words[0] <<= 3;
        self.buffer[56..60].copy_from_slice(&self.words[0].to_le_bytes());
        self.buffer[60..].copy_from_slice(&self.words[1].to_le_bytes());
        let buffer = self.buffer;
        self.body(&buffer);
        let mut out = [0u8; 16];
        for (i, word) in self.words[2..].iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// `make_digest_ex(md5str, digest, len)`: the digest as lowercase hex.
pub(super) fn hex_digest(digest: &[u8]) -> Vec<u8> {
    const HEXITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEXITS[usize::from(byte >> 4)]);
        out.push(HEXITS[usize::from(byte & 15)]);
    }
    out
}

/// The digest as the native answers it: the raw bytes, or their hex.
pub(super) fn digest_value(
    req: &mut Request,
    digest: &[u8],
    binary: bool,
) -> Result<Value, Thrown> {
    if binary {
        string_value(req, digest)
    } else {
        string_value(req, &hex_digest(digest))
    }
}

/// `md5(string $string, bool $binary = false): string`.
///
/// ```php
/// var_dump(md5(""), md5("a"), md5("The quick brown fox jumps over the lazy dog"), md5(str_repeat("x", 1000)), bin2hex(md5("abc", true)), md5(123));
/// ```
/// ```expect
/// string(32) "d41d8cd98f00b204e9800998ecf8427e"
/// string(32) "0cc175b9c0f1b6a831c399e269772661"
/// string(32) "9e107d9d372bb6826bd81d3542a419d6"
/// string(32) "398533d48111e9f664b1f64cb10c4b63"
/// string(32) "900150983cd24fb0d6963f7d28e17f72"
/// string(32) "202cb962ac59075b964b07152d234b70"
/// ```
fn md5(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let string = string_argument(req, 1)?;
    let binary = optional_bool(req, 2, false)?;
    let mut context = Md5::new();
    context.update(string.as_bytes());
    let digest = context.finish();
    digest_value(req, &digest, binary)
}

/// `Z_PARAM_PATH`: a string with no null byte in it. What
/// `php_stream_open_wrapper(arg, "rb", REPORT_ERRORS, NULL)` and the
/// read loop `md5_file` and `sha1_file` share then read is every byte
/// of the file, or `None` after the warning the open left or a read
/// that stopped short of the end.
pub(super) fn path_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let filename = string_argument(req, position)?;
    if filename.as_bytes().contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    Ok(filename)
}

/// The file's bytes, read as `md5_file`'s loop reads them.
pub(super) fn file_bytes(
    req: &mut Request,
    filename: PhpString,
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(stream) = req.open_stream(filename.as_bytes(), b"rb", false)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    loop {
        match stream.read(req, 1024)? {
            Some(chunk) if !chunk.is_empty() => bytes.extend_from_slice(chunk.as_bytes()),
            _ => break,
        }
    }
    let complete = stream.eof(req);
    stream.close(req);
    Ok(complete.then_some(bytes))
}

/// `md5_file(string $filename, bool $binary = false): string|false`.
///
/// ```php
/// $p = __DIR__ . '/md5.txt'; $f = fopen($p, 'w'); fwrite($f, str_repeat("abc", 1000)); fclose($f);
/// var_dump(md5_file($p), bin2hex(md5_file($p, true)), md5_file(__DIR__ . '/nope.txt'));
/// try { md5_file("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: md5_file(%s/nope.txt): Failed to open stream: No such file or directory in %s on line 3
/// string(32) "8f33d0ecfe648a745b169baf8a77658b"
/// string(32) "8f33d0ecfe648a745b169baf8a77658b"
/// bool(false)
/// md5_file(): Argument #1 ($filename) must not contain any null bytes
/// ```
fn md5_file(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let filename = path_argument(req, 1)?;
    let binary = optional_bool(req, 2, false)?;
    let Some(bytes) = file_bytes(req, filename)? else {
        return Ok(Value::bool(false));
    };
    let mut context = Md5::new();
    context.update(&bytes);
    let digest = context.finish();
    digest_value(req, &digest, binary)
}
