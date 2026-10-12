//! The `$5$` and `$6$` hashes: Ulrich Drepper's SHA-crypt as php carries it
//! (`Z/ext/standard/crypt_sha256.c:319`, `Z/ext/standard/crypt_sha512.c:358`),
//! with the SHA-256 and SHA-512 digests of FIPS 180-4 it runs on.

use php_ext::coerce::is_space;

/// The SHA-256 round constants, FIPS 180-4 section 4.2.2.
const K256: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The SHA-512 round constants, FIPS 180-4 section 4.2.3.
const K512: [u64; 80] = [
    0x428a_2f98_d728_ae22,
    0x7137_4491_23ef_65cd,
    0xb5c0_fbcf_ec4d_3b2f,
    0xe9b5_dba5_8189_dbbc,
    0x3956_c25b_f348_b538,
    0x59f1_11f1_b605_d019,
    0x923f_82a4_af19_4f9b,
    0xab1c_5ed5_da6d_8118,
    0xd807_aa98_a303_0242,
    0x1283_5b01_4570_6fbe,
    0x2431_85be_4ee4_b28c,
    0x550c_7dc3_d5ff_b4e2,
    0x72be_5d74_f27b_896f,
    0x80de_b1fe_3b16_96b1,
    0x9bdc_06a7_25c7_1235,
    0xc19b_f174_cf69_2694,
    0xe49b_69c1_9ef1_4ad2,
    0xefbe_4786_384f_25e3,
    0x0fc1_9dc6_8b8c_d5b5,
    0x240c_a1cc_77ac_9c65,
    0x2de9_2c6f_592b_0275,
    0x4a74_84aa_6ea6_e483,
    0x5cb0_a9dc_bd41_fbd4,
    0x76f9_88da_8311_53b5,
    0x983e_5152_ee66_dfab,
    0xa831_c66d_2db4_3210,
    0xb003_27c8_98fb_213f,
    0xbf59_7fc7_beef_0ee4,
    0xc6e0_0bf3_3da8_8fc2,
    0xd5a7_9147_930a_a725,
    0x06ca_6351_e003_826f,
    0x1429_2967_0a0e_6e70,
    0x27b7_0a85_46d2_2ffc,
    0x2e1b_2138_5c26_c926,
    0x4d2c_6dfc_5ac4_2aed,
    0x5338_0d13_9d95_b3df,
    0x650a_7354_8baf_63de,
    0x766a_0abb_3c77_b2a8,
    0x81c2_c92e_47ed_aee6,
    0x9272_2c85_1482_353b,
    0xa2bf_e8a1_4cf1_0364,
    0xa81a_664b_bc42_3001,
    0xc24b_8b70_d0f8_9791,
    0xc76c_51a3_0654_be30,
    0xd192_e819_d6ef_5218,
    0xd699_0624_5565_a910,
    0xf40e_3585_5771_202a,
    0x106a_a070_32bb_d1b8,
    0x19a4_c116_b8d2_d0c8,
    0x1e37_6c08_5141_ab53,
    0x2748_774c_df8e_eb99,
    0x34b0_bcb5_e19b_48a8,
    0x391c_0cb3_c5c9_5a63,
    0x4ed8_aa4a_e341_8acb,
    0x5b9c_ca4f_7763_e373,
    0x682e_6ff3_d6b2_b8a3,
    0x748f_82ee_5def_b2fc,
    0x78a5_636f_4317_2f60,
    0x84c8_7814_a1f0_ab72,
    0x8cc7_0208_1a64_39ec,
    0x90be_fffa_2363_1e28,
    0xa450_6ceb_de82_bde9,
    0xbef9_a3f7_b2c6_7915,
    0xc671_78f2_e372_532b,
    0xca27_3ece_ea26_619c,
    0xd186_b8c7_21c0_c207,
    0xeada_7dd6_cde0_eb1e,
    0xf57d_4f7f_ee6e_d178,
    0x06f0_67aa_7217_6fba,
    0x0a63_7dc5_a2c8_98a6,
    0x113f_9804_bef9_0dae,
    0x1b71_0b35_131c_471b,
    0x28db_77f5_2304_7d84,
    0x32ca_ab7b_40c7_2493,
    0x3c9e_be0a_15c9_bebc,
    0x431d_67c4_9c10_0d4c,
    0x4cc5_d4be_cb3e_42b6,
    0x597f_299c_fc65_7e2a,
    0x5fcb_6fab_3ad6_faec,
    0x6c44_198c_4a47_5817,
];

/// The digest the SHA-crypt loop is written over: `sha256_ctx` or
/// `sha512_ctx` with its `*_init_ctx`, `*_process_bytes` and
/// `*_finish_ctx` (`Z/ext/standard/crypt_sha256.c:48`,
/// `Z/ext/standard/crypt_sha512.c:45`).
pub(super) trait ShaDigest: Sized {
    /// The digest's width in bytes, 32 or 64.
    const LEN: usize;
    /// The digest bytes.
    type Out: AsRef<[u8]> + Copy;
    /// `sha256_init_ctx` / `sha512_init_ctx`.
    fn new() -> Self;
    /// `sha256_process_bytes` / `sha512_process_bytes`.
    fn update(&mut self, data: &[u8]);
    /// `sha256_finish_ctx` / `sha512_finish_ctx`.
    fn finish(self) -> Self::Out;
}

/// A streaming SHA-256 (FIPS 180-4 section 6.2), what `sha256_ctx`
/// computes (`Z/ext/standard/crypt_sha256.c:48`).
pub(super) struct Sha256 {
    /// `H`, the chaining state.
    h: [u32; 8],
    /// `buffer`, the bytes not yet compressed.
    block: [u8; 64],
    /// `buflen`, how much of `block` holds bytes.
    filled: usize,
    /// `total`, the bytes fed so far.
    total: u64,
}

impl Sha256 {
    /// `sha256_process_block` for one 64-byte block
    /// (`Z/ext/standard/crypt_sha256.c:91`), FIPS 180-4 section 6.2.2.
    #[allow(clippy::many_single_char_names, reason = "FIPS 180-4's names")]
    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (wt, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
            *wt = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for (k, wt) in K256.iter().zip(w) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(wt);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (hv, v) in self.h.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *hv = hv.wrapping_add(v);
        }
    }
}

impl ShaDigest for Sha256 {
    const LEN: usize = 32;
    type Out = [u8; 32];

    /// `sha256_init_ctx`, `Z/ext/standard/crypt_sha256.c:189`.
    fn new() -> Self {
        Sha256 {
            h: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            block: [0; 64],
            filled: 0,
            total: 0,
        }
    }

    /// `sha256_process_bytes`, `Z/ext/standard/crypt_sha256.c:241`.
    fn update(&mut self, data: &[u8]) {
        // The length wraps at 2^64 bytes, as the C's 64-bit total does.
        self.total = self
            .total
            .wrapping_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
        for &byte in data {
            if let Some(slot) = self.block.get_mut(self.filled) {
                *slot = byte;
            }
            self.filled += 1;
            if self.filled == 64 {
                let block = self.block;
                self.compress(&block);
                self.filled = 0;
            }
        }
    }

    /// `sha256_finish_ctx`, `Z/ext/standard/crypt_sha256.c:209`: pad, append
    /// the bit length, and the state as big-endian bytes.
    fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.filled != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.h) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// A streaming SHA-512 (FIPS 180-4 section 6.4), what `sha512_ctx`
/// computes (`Z/ext/standard/crypt_sha512.c:45`).
pub(super) struct Sha512 {
    /// `H`, the chaining state.
    h: [u64; 8],
    /// `buffer`, the bytes not yet compressed.
    block: [u8; 128],
    /// `buflen`, how much of `block` holds bytes.
    filled: usize,
    /// `total`, the bytes fed so far.
    total: u128,
}

impl Sha512 {
    /// `sha512_process_block` for one 128-byte block
    /// (`Z/ext/standard/crypt_sha512.c:121`), FIPS 180-4 section 6.4.2.
    #[allow(clippy::many_single_char_names, reason = "FIPS 180-4's names")]
    fn compress(&mut self, block: &[u8; 128]) {
        let mut w = [0u64; 80];
        for (wt, chunk) in w.iter_mut().zip(block.chunks_exact(8)) {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(chunk);
            *wt = u64::from_be_bytes(bytes);
        }
        for t in 16..80 {
            let s0 = w[t - 15].rotate_right(1) ^ w[t - 15].rotate_right(8) ^ (w[t - 15] >> 7);
            let s1 = w[t - 2].rotate_right(19) ^ w[t - 2].rotate_right(61) ^ (w[t - 2] >> 6);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for (k, wt) in K512.iter().zip(w) {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(wt);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (hv, v) in self.h.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *hv = hv.wrapping_add(v);
        }
    }
}

impl ShaDigest for Sha512 {
    const LEN: usize = 64;
    type Out = [u8; 64];

    /// `sha512_init_ctx`, `Z/ext/standard/crypt_sha512.c:220`.
    fn new() -> Self {
        Sha512 {
            h: [
                0x6a09_e667_f3bc_c908,
                0xbb67_ae85_84ca_a73b,
                0x3c6e_f372_fe94_f82b,
                0xa54f_f53a_5f1d_36f1,
                0x510e_527f_ade6_82d1,
                0x9b05_688c_2b3e_6c1f,
                0x1f83_d9ab_fb41_bd6b,
                0x5be0_cd19_137e_2179,
            ],
            block: [0; 128],
            filled: 0,
            total: 0,
        }
    }

    /// `sha512_process_bytes`, `Z/ext/standard/crypt_sha512.c:272`.
    fn update(&mut self, data: &[u8]) {
        self.total = self
            .total
            .wrapping_add(u128::try_from(data.len()).unwrap_or(u128::MAX));
        for &byte in data {
            if let Some(slot) = self.block.get_mut(self.filled) {
                *slot = byte;
            }
            self.filled += 1;
            if self.filled == 128 {
                let block = self.block;
                self.compress(&block);
                self.filled = 0;
            }
        }
    }

    /// `sha512_finish_ctx`, `Z/ext/standard/crypt_sha512.c:240`: pad, append
    /// the bit length, and the state as big-endian bytes.
    fn finish(mut self) -> [u8; 64] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.filled != 112 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 64];
        for (chunk, word) in out.chunks_exact_mut(8).zip(self.h) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// `SALT_LEN_MAX`, `Z/ext/standard/crypt_sha256.c:307`.
const SALT_LEN_MAX: usize = 16;
/// `ROUNDS_DEFAULT`, `Z/ext/standard/crypt_sha256.c:309`.
const ROUNDS_DEFAULT: u64 = 5000;
/// `ROUNDS_MIN`, `Z/ext/standard/crypt_sha256.c:311`.
const ROUNDS_MIN: u64 = 1000;
/// `ROUNDS_MAX`, `Z/ext/standard/crypt_sha256.c:313`.
const ROUNDS_MAX: u64 = 999_999_999;
/// `sha256_rounds_prefix`, `Z/ext/standard/crypt_sha256.c:304`.
const ROUNDS_PREFIX: &[u8] = b"rounds=";
/// `PHP_MAX_SALT_LEN`, the `buflen` `php_crypt` hands both functions
/// (`Z/ext/standard/crypt.c:97`, `Z/ext/standard/php_crypt.h:28`).
const BUFLEN: usize = 123;

/// `b64t`, `Z/ext/standard/crypt_sha256.c:316`.
const B64T: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The `b64_from_24bit` calls of `Z/ext/standard/crypt_sha256.c:542-552`:
/// the digest bytes taken as B2, B1, B0 and how many characters each emits.
const ORDER_256: [(usize, usize, usize, usize); 11] = [
    (0, 10, 20, 4),
    (21, 1, 11, 4),
    (12, 22, 2, 4),
    (3, 13, 23, 4),
    (24, 4, 14, 4),
    (15, 25, 5, 4),
    (6, 16, 26, 4),
    (27, 7, 17, 4),
    (18, 28, 8, 4),
    (9, 19, 29, 4),
    (NONE, 31, 30, 3),
];

/// The `b64_from_24bit` calls of `Z/ext/standard/crypt_sha512.c:581-602`.
const ORDER_512: [(usize, usize, usize, usize); 22] = [
    (0, 21, 42, 4),
    (22, 43, 1, 4),
    (44, 2, 23, 4),
    (3, 24, 45, 4),
    (25, 46, 4, 4),
    (47, 5, 26, 4),
    (6, 27, 48, 4),
    (28, 49, 7, 4),
    (50, 8, 29, 4),
    (9, 30, 51, 4),
    (31, 52, 10, 4),
    (53, 11, 32, 4),
    (12, 33, 54, 4),
    (34, 55, 13, 4),
    (56, 14, 35, 4),
    (15, 36, 57, 4),
    (37, 58, 16, 4),
    (59, 17, 38, 4),
    (18, 39, 60, 4),
    (40, 61, 19, 4),
    (62, 20, 41, 4),
    (NONE, NONE, 63, 2),
];

/// Marks a `b64_from_24bit` operand that is the constant 0 rather than a
/// digest byte.
const NONE: usize = usize::MAX;

/// `php_sha256_crypt_r`, `Z/ext/standard/crypt_sha256.c:319`, with the
/// 123-byte buffer `php_crypt` gives it (`Z/ext/standard/crypt.c:112`). `key`
/// and `salt` end before their first NUL, as the C strings do. `None` is
/// the C's NULL: rounds outside 1000..=999999999, or a buffer too short.
pub(super) fn php_sha256_crypt_r(key: &[u8], salt: &[u8]) -> Option<Vec<u8>> {
    sha_crypt_r::<Sha256>(b"$5$", &ORDER_256, key, salt)
}

/// `php_sha512_crypt_r`, `Z/ext/standard/crypt_sha512.c:358`, with the
/// 123-byte buffer `php_crypt` gives it (`Z/ext/standard/crypt.c:97`). As
/// [`php_sha256_crypt_r`]; with a 9-digit `rounds=` and a 16-byte salt the
/// 86 hash characters do not fit, and php answers NULL.
pub(super) fn php_sha512_crypt_r(key: &[u8], salt: &[u8]) -> Option<Vec<u8>> {
    sha_crypt_r::<Sha512>(b"$6$", &ORDER_512, key, salt)
}

/// `ZEND_STRTOUL(num, &endp, 10)`, which is `strtoull` on a 64-bit build
/// (`Z/Zend/zend_long.h:79`), as FreeBSD's libc on macOS computes it: leading
/// spaces, one sign, decimal digits; an overflow gives `u64::MAX` and ends
/// after the digits; a minus negates modulo 2^64; no digits leaves `endp` at
/// `num`. Returns the value and the offset of `endp` in `num`. Kept beside
/// the face's `strtol`, whose long saturates where this unsigned reading
/// wraps a sign and answers its end; the spaces it skips are the face's
/// `is_space`, C's `isspace` under the thread's `LC_CTYPE`, which under
/// the oracle's `C.UTF-8` on macOS counts 0xA0 (glibc's does not).
fn strtoull10(num: &[u8]) -> (u64, usize) {
    let at = |i: usize| num.get(i).copied().unwrap_or(0);
    let mut s = 0;
    while is_space(at(s)) {
        s += 1;
    }
    let mut neg = false;
    if at(s) == b'-' {
        neg = true;
        s += 1;
    } else if at(s) == b'+' {
        s += 1;
    }
    let mut acc: u64 = 0;
    let mut any: i8 = 0;
    while at(s).is_ascii_digit() {
        let digit = u64::from(at(s) - b'0');
        if any < 0 {
            // Past an overflow the digits are only consumed.
        } else if let Some(next) = acc.checked_mul(10).and_then(|v| v.checked_add(digit)) {
            acc = next;
            any = 1;
        } else {
            any = -1;
        }
        s += 1;
    }
    match any {
        0 => (0, 0),
        -1 => (u64::MAX, s),
        _ if neg => (acc.wrapping_neg(), s),
        _ => (acc, s),
    }
}

/// The body both `php_sha256_crypt_r` (`Z/ext/standard/crypt_sha256.c:319`)
/// and `php_sha512_crypt_r` (`Z/ext/standard/crypt_sha512.c:358`) spell out,
/// over the digest `D`, its salt prefix and its `b64_from_24bit` order.
#[allow(clippy::too_many_lines, reason = "one C function, in the C's order")]
fn sha_crypt_r<D: ShaDigest>(
    prefix: &[u8],
    order: &[(usize, usize, usize, usize)],
    key: &[u8],
    salt: &[u8],
) -> Option<Vec<u8>> {
    let mut salt = salt.strip_prefix(prefix).unwrap_or(salt);
    let mut rounds = ROUNDS_DEFAULT;
    let mut rounds_custom = false;

    if let Some(num) = salt.strip_prefix(ROUNDS_PREFIX) {
        let (srounds, endp) = strtoull10(num);
        if num.get(endp) == Some(&b'$') {
            salt = num.get(endp + 1..).unwrap_or(&[]);
            if !(ROUNDS_MIN..=ROUNDS_MAX).contains(&srounds) {
                return None;
            }
            rounds = srounds;
            rounds_custom = true;
        }
    }

    let salt_len = salt
        .iter()
        .position(|&c| c == b'$')
        .unwrap_or(salt.len())
        .min(SALT_LEN_MAX);
    let salt = salt.get(..salt_len).unwrap_or(salt);
    let key_len = key.len();

    // Prepare for the real work.
    let mut ctx = D::new();
    ctx.update(key);
    ctx.update(salt);

    // Alternate sum with input KEY, SALT, and KEY.
    let mut alt_ctx = D::new();
    alt_ctx.update(key);
    alt_ctx.update(salt);
    alt_ctx.update(key);
    let alt = alt_ctx.finish();
    let alt_bytes = alt.as_ref();

    // For each character in the key one byte of the alternate sum.
    let mut cnt = key_len;
    while cnt > D::LEN {
        ctx.update(alt_bytes);
        cnt -= D::LEN;
    }
    ctx.update(alt_bytes.get(..cnt).unwrap_or(alt_bytes));

    // For every 1 bit of the key length the alternate sum, for every 0 the key.
    let mut cnt = key_len;
    while cnt > 0 {
        if cnt & 1 != 0 {
            ctx.update(alt_bytes);
        } else {
            ctx.update(key);
        }
        cnt >>= 1;
    }

    // Intermediate sum.
    let mut alt_result = ctx.finish();

    // P byte sequence: the key hashed once per key byte, repeated to key_len.
    let mut alt_ctx = D::new();
    for _ in 0..key_len {
        alt_ctx.update(key);
    }
    let temp = alt_ctx.finish();
    let p_bytes: Vec<u8> = temp
        .as_ref()
        .iter()
        .copied()
        .cycle()
        .take(key_len)
        .collect();

    // S byte sequence: the salt hashed 16 + alt_result[0] times.
    let mut alt_ctx = D::new();
    let first = alt_result.as_ref().first().copied().unwrap_or(0);
    for _ in 0..16 + usize::from(first) {
        alt_ctx.update(salt);
    }
    let temp = alt_ctx.finish();
    let s_bytes: Vec<u8> = temp
        .as_ref()
        .iter()
        .copied()
        .cycle()
        .take(salt_len)
        .collect();

    // Repeatedly run the collected hash value through the digest.
    for cnt in 0..rounds {
        let mut ctx = D::new();
        if cnt & 1 != 0 {
            ctx.update(&p_bytes);
        } else {
            ctx.update(alt_result.as_ref());
        }
        if cnt % 3 != 0 {
            ctx.update(&s_bytes);
        }
        if cnt % 7 != 0 {
            ctx.update(&p_bytes);
        }
        if cnt & 1 != 0 {
            ctx.update(alt_result.as_ref());
        } else {
            ctx.update(&p_bytes);
        }
        alt_result = ctx.finish();
    }

    // The hash string, written against php's `buflen` as the C does.
    let mut buflen = BUFLEN;
    let mut out = Vec::with_capacity(BUFLEN);
    out.extend_from_slice(prefix);
    buflen -= prefix.len();

    if rounds_custom {
        // At most 17 characters against 120 left: `snprintf` never
        // truncates here.
        let rounds_text = format!("rounds={rounds}$");
        out.extend_from_slice(rounds_text.as_bytes());
        buflen = buflen.saturating_sub(rounds_text.len());
    }

    let n = buflen.min(salt_len);
    out.extend_from_slice(salt.get(..n).unwrap_or(salt));
    buflen -= n;

    if buflen > 0 {
        out.push(b'$');
        buflen -= 1;
    }

    let digest = alt_result.as_ref();
    let byte = |i: usize| u32::from(digest.get(i).copied().unwrap_or(0));
    for &(b2, b1, b0, n) in order {
        let mut w = (byte(b2) << 16) | (byte(b1) << 8) | byte(b0);
        for _ in 0..n {
            if buflen == 0 {
                break;
            }
            out.push(B64T[usize::from(w.to_le_bytes()[0] & 0x3f)]);
            buflen -= 1;
            w >>= 6;
        }
    }

    if buflen == 0 { None } else { Some(out) }
}
