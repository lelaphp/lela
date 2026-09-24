//! The DES hashes: two-character traditional salts and the nine-character
//! `_` extended salts, David Burren's `FreeSec` as php carries it
//! (`Z/ext/standard/crypt_freesec.c:608`).

/// `IP`, the initial permutation (`Z/ext/standard/crypt_freesec.c:72`).
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

/// `key_perm`, `Z/ext/standard/crypt_freesec.c:79`.
const KEY_PERM: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];

/// `key_shifts`, `Z/ext/standard/crypt_freesec.c:86`.
const KEY_SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

/// `comp_perm`, `Z/ext/standard/crypt_freesec.c:90`.
const COMP_PERM: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];

/// `sbox`, the eight DES S-boxes (`Z/ext/standard/crypt_freesec.c:101`).
const SBOX: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// `pbox`, `Z/ext/standard/crypt_freesec.c:152`.
const PBOX: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];

/// `ascii64`, `Z/ext/standard/crypt_freesec.c:171`.
const ASCII64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `_PASSWORD_EFMT1`, the first byte of an extended salt
/// (`Z/ext/standard/crypt_freesec.c:70`).
const PASSWORD_EFMT1: u8 = b'_';

/// `bits32[n]`, `Z/ext/standard/crypt_freesec.c:157`: bit `n` counted from
/// the top.
const fn bits32(n: usize) -> u32 {
    0x8000_0000 >> n
}

/// `bits8[n]`, `Z/ext/standard/crypt_freesec.c:169`.
const fn bits8(n: usize) -> usize {
    0x80 >> n
}

/// The lookup tables `_crypt_extended_init` fills once per process
/// (`Z/ext/standard/crypt_freesec.c:174-179`), computed at compile time.
struct Tables {
    /// `m_sbox`: the S-boxes paired, 12 bits in, 8 bits out.
    m_sbox: [[u8; 4096]; 4],
    /// `psbox`: the P-box applied to each S-box pair's byte.
    psbox: [[u32; 256]; 4],
    /// `ip_maskl`: the initial permutation's left half, per input byte.
    ip_maskl: [[u32; 256]; 8],
    /// `ip_maskr`: the initial permutation's right half, per input byte.
    ip_maskr: [[u32; 256]; 8],
    /// `fp_maskl`: the final permutation's left half, per input byte.
    fp_maskl: [[u32; 256]; 8],
    /// `fp_maskr`: the final permutation's right half, per input byte.
    fp_maskr: [[u32; 256]; 8],
    /// `key_perm_maskl`: the key permutation's left 28 bits, per 7 key bits.
    key_perm_maskl: [[u32; 128]; 8],
    /// `key_perm_maskr`: the key permutation's right 28 bits, per 7 key bits.
    key_perm_maskr: [[u32; 128]; 8],
    /// `comp_maskl`: the compression permutation's left 24 bits.
    comp_maskl: [[u32; 128]; 8],
    /// `comp_maskr`: the compression permutation's right 24 bits.
    comp_maskr: [[u32; 128]; 8],
}

/// The tables, built once by the compiler; `_crypt_extended_init_r`
/// (`Z/ext/standard/php_crypt_r.c:64`) has nothing left to do.
static TABLES: Tables = Tables::build();

impl Tables {
    /// `_crypt_extended_init`, `Z/ext/standard/crypt_freesec.c:209`.
    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        reason = "one C function, with the C's names"
    )]
    const fn build() -> Tables {
        let mut t = Tables {
            m_sbox: [[0; 4096]; 4],
            psbox: [[0; 256]; 4],
            ip_maskl: [[0; 256]; 8],
            ip_maskr: [[0; 256]; 8],
            fp_maskl: [[0; 256]; 8],
            fp_maskr: [[0; 256]; 8],
            key_perm_maskl: [[0; 128]; 8],
            key_perm_maskr: [[0; 128]; 8],
            comp_maskl: [[0; 128]; 8],
            comp_maskr: [[0; 128]; 8],
        };

        // Invert the S-boxes, reordering the input bits.
        let mut u_sbox = [[0u8; 64]; 8];
        let mut i = 0;
        while i < 8 {
            let mut j = 0;
            while j < 64 {
                let b = (j & 0x20) | ((j & 1) << 4) | ((j >> 1) & 0xf);
                u_sbox[i][j] = SBOX[i][b];
                j += 1;
            }
            i += 1;
        }

        // Convert the inverted S-boxes into 4 arrays of 8 bits, each
        // handling 12 bits of the S-box input.
        let mut b = 0;
        while b < 4 {
            let mut i = 0;
            while i < 64 {
                let mut j = 0;
                while j < 64 {
                    t.m_sbox[b][(i << 6) | j] = (u_sbox[b << 1][i] << 4) | u_sbox[(b << 1) + 1][j];
                    j += 1;
                }
                i += 1;
            }
            b += 1;
        }

        // The initial and final permutations, and the inverted key
        // permutation. Each `as usize` in this function widens a `u8`
        // table entry: `From` has no const form.
        let mut init_perm = [0usize; 64];
        let mut final_perm = [0usize; 64];
        let mut inv_key_perm = [255usize; 64];
        let mut inv_comp_perm = [255usize; 56];
        let mut i = 0;
        while i < 64 {
            final_perm[i] = IP[i] as usize - 1;
            init_perm[final_perm[i]] = i;
            i += 1;
        }
        let mut i = 0;
        while i < 56 {
            inv_key_perm[KEY_PERM[i] as usize - 1] = i;
            i += 1;
        }
        let mut i = 0;
        while i < 48 {
            inv_comp_perm[COMP_PERM[i] as usize - 1] = i;
            i += 1;
        }

        // The OR-mask arrays for the initial and final permutations, and
        // for the key initial and compression permutations. `bits28` is
        // `bits32 + 4` and `bits24` is `bits32 + 8`.
        let mut k = 0;
        while k < 8 {
            let mut i = 0;
            while i < 256 {
                let mut j = 0;
                while j < 8 {
                    let inbit = 8 * k + j;
                    if i & bits8(j) != 0 {
                        let obit = init_perm[inbit];
                        if obit < 32 {
                            t.ip_maskl[k][i] |= bits32(obit);
                        } else {
                            t.ip_maskr[k][i] |= bits32(obit - 32);
                        }
                        let obit = final_perm[inbit];
                        if obit < 32 {
                            t.fp_maskl[k][i] |= bits32(obit);
                        } else {
                            t.fp_maskr[k][i] |= bits32(obit - 32);
                        }
                    }
                    j += 1;
                }
                i += 1;
            }
            let mut i = 0;
            while i < 128 {
                let mut j = 0;
                while j < 7 {
                    let inbit = 8 * k + j;
                    if i & bits8(j + 1) != 0 {
                        let obit = inv_key_perm[inbit];
                        if obit == 255 {
                            // Not part of the key.
                        } else if obit < 28 {
                            t.key_perm_maskl[k][i] |= bits32(obit + 4);
                        } else {
                            t.key_perm_maskr[k][i] |= bits32(obit - 28 + 4);
                        }
                    }
                    j += 1;
                }
                let mut j = 0;
                while j < 7 {
                    let inbit = 7 * k + j;
                    if i & bits8(j + 1) != 0 {
                        let obit = inv_comp_perm[inbit];
                        if obit == 255 {
                            // Dropped by the compression.
                        } else if obit < 24 {
                            t.comp_maskl[k][i] |= bits32(obit + 8);
                        } else {
                            t.comp_maskr[k][i] |= bits32(obit - 24 + 8);
                        }
                    }
                    j += 1;
                }
                i += 1;
            }
            k += 1;
        }

        // Invert the P-box permutation, and convert into OR-masks for the
        // S-box arrays above.
        let mut un_pbox = [0usize; 32];
        let mut i = 0;
        while i < 32 {
            un_pbox[PBOX[i] as usize - 1] = i;
            i += 1;
        }
        let mut b = 0;
        while b < 4 {
            let mut i = 0;
            while i < 256 {
                let mut j = 0;
                while j < 8 {
                    if i & bits8(j) != 0 {
                        t.psbox[b][i] |= bits32(un_pbox[8 * b + j]);
                    }
                    j += 1;
                }
                i += 1;
            }
            b += 1;
        }
        t
    }
}

/// `ascii_to_bin`, `Z/ext/standard/crypt_freesec.c:182`: a salt character as
/// six bits, read through a signed `char` and masked, so any byte maps.
fn ascii_to_bin(ch: u8) -> u8 {
    let sch = i32::from(ch.cast_signed());
    let mut retval = sch - i32::from(b'.');
    if sch >= i32::from(b'A') {
        retval = sch - (i32::from(b'A') - 12);
        if sch >= i32::from(b'a') {
            retval = sch - (i32::from(b'a') - 38);
        }
    }
    // `& 0x3f` leaves six bits of the two's-complement value.
    retval.to_le_bytes()[0] & 0x3f
}

/// `ascii_is_unsafe`, `Z/ext/standard/crypt_freesec.c:203`: a byte that would
/// break the passwd file format.
fn ascii_is_unsafe(ch: u8) -> bool {
    ch == 0 || ch == b'\n' || ch == b':'
}

/// `setup_salt`, `Z/ext/standard/crypt_freesec.c:349`: the 24 salt bits
/// reversed into `saltbits`. php caches the last salt in its state; the
/// value is a function of `salt` alone, so no cache is kept here.
fn setup_salt(salt: u32) -> u32 {
    let mut saltbits = 0;
    let mut saltbit = 1u32;
    let mut obit = 0x0080_0000u32;
    for _ in 0..24 {
        if salt & saltbit != 0 {
            saltbits |= obit;
        }
        saltbit <<= 1;
        obit >>= 1;
    }
    saltbits
}

/// The encryption key schedule `des_setkey` leaves in `en_keysl` and
/// `en_keysr` (`Z/ext/standard/crypt_freesec.c:371`). The decryption schedule
/// is not kept: crypt only ever encrypts.
struct Keys {
    /// `en_keysl`, the left 24 bits of each round key.
    l: [u32; 16],
    /// `en_keysr`, the right 24 bits of each round key.
    r: [u32; 16],
}

/// `des_setkey`, `Z/ext/standard/crypt_freesec.c:371`. php skips the work when
/// the raw key repeats; the schedule is a function of the key alone, so it
/// is always computed here. It never fails.
fn des_setkey(key: [u8; 8]) -> Keys {
    let t = &TABLES;
    let rawkey0 = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
    let rawkey1 = u32::from_be_bytes([key[4], key[5], key[6], key[7]]);

    // Every index below is a 7-bit field.
    let idx = |v: u32, shift: u32| usize::from((v >> shift).to_le_bytes()[0] & 0x7f);
    let mut k0 = 0;
    let mut k1 = 0;
    for (n, (raw, shift)) in [
        (rawkey0, 25),
        (rawkey0, 17),
        (rawkey0, 9),
        (rawkey0, 1),
        (rawkey1, 25),
        (rawkey1, 17),
        (rawkey1, 9),
        (rawkey1, 1),
    ]
    .into_iter()
    .enumerate()
    {
        k0 |= t.key_perm_maskl[n][idx(raw, shift)];
        k1 |= t.key_perm_maskr[n][idx(raw, shift)];
    }

    // Rotate subkeys and do the compression permutation.
    let mut keys = Keys {
        l: [0; 16],
        r: [0; 16],
    };
    let mut shifts = 0;
    for (round, &step) in KEY_SHIFTS.iter().enumerate() {
        shifts += step;
        let t0 = (k0 << shifts) | (k0 >> (28 - shifts));
        let t1 = (k1 << shifts) | (k1 >> (28 - shifts));
        let parts = [
            idx(t0, 21),
            idx(t0, 14),
            idx(t0, 7),
            idx(t0, 0),
            idx(t1, 21),
            idx(t1, 14),
            idx(t1, 7),
            idx(t1, 0),
        ];
        let mut l = 0;
        let mut r = 0;
        for (n, &part) in parts.iter().enumerate() {
            l |= t.comp_maskl[n][part];
            r |= t.comp_maskr[n][part];
        }
        keys.l[round] = l;
        keys.r[round] = r;
    }
    keys
}

/// The eight byte lanes of a pseudo-big-endian 64-bit block, as `do_des`
/// indexes its 8 x 256 permutation tables.
fn lanes(l: u32, r: u32) -> [usize; 8] {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&l.to_be_bytes());
    bytes[4..].copy_from_slice(&r.to_be_bytes());
    bytes.map(usize::from)
}

/// The low twelve bits of `v`, one `m_sbox` index
/// (`Z/ext/standard/crypt_freesec.c:537`).
fn low12(v: u32) -> usize {
    let [lo, hi, _, _] = v.to_le_bytes();
    usize::from(u16::from_le_bytes([lo, hi]) & 0xfff)
}

/// `do_des`, `Z/ext/standard/crypt_freesec.c:456`, encrypting `count` times
/// with `saltbits` perturbing the E-box. `None` is the C's non-zero answer
/// for a zero count. crypt never decrypts, so a negative count has no path
/// here.
#[allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    reason = "the C's names"
)]
fn do_des(l_in: u32, r_in: u32, count: u32, keys: &Keys, saltbits: u32) -> Option<(u32, u32)> {
    let t = &TABLES;
    if count == 0 {
        return None;
    }

    // Initial permutation (IP).
    let mut l = 0;
    let mut r = 0;
    for (n, lane) in lanes(l_in, r_in).into_iter().enumerate() {
        l |= t.ip_maskl[n][lane];
        r |= t.ip_maskr[n][lane];
    }

    let mut f = 0;
    for _ in 0..count {
        for (kl, kr) in keys.l.iter().zip(&keys.r) {
            // Expand R to 48 bits (the E-box).
            let mut r48l = ((r & 0x0000_0001) << 23)
                | ((r & 0xf800_0000) >> 9)
                | ((r & 0x1f80_0000) >> 11)
                | ((r & 0x01f8_0000) >> 13)
                | ((r & 0x001f_8000) >> 15);
            let mut r48r = ((r & 0x0001_f800) << 7)
                | ((r & 0x0000_1f80) << 5)
                | ((r & 0x0000_01f8) << 3)
                | ((r & 0x0000_001f) << 1)
                | ((r & 0x8000_0000) >> 31);
            // Salting, and XOR with the permuted key.
            f = (r48l ^ r48r) & saltbits;
            r48l ^= f ^ kl;
            r48r ^= f ^ kr;
            // S-box lookups and the P-box permutation at once. Both halves
            // are 24 bits, so each 12-bit mask only states that.
            let s0 = usize::from(t.m_sbox[0][low12(r48l >> 12)]);
            let s1 = usize::from(t.m_sbox[1][low12(r48l)]);
            let s2 = usize::from(t.m_sbox[2][low12(r48r >> 12)]);
            let s3 = usize::from(t.m_sbox[3][low12(r48r)]);
            f = t.psbox[0][s0] | t.psbox[1][s1] | t.psbox[2][s2] | t.psbox[3][s3];
            f ^= l;
            l = r;
            r = f;
        }
        r = l;
        l = f;
    }

    // Final permutation (inverse of IP).
    let mut l_out = 0;
    let mut r_out = 0;
    for (n, lane) in lanes(l, r).into_iter().enumerate() {
        l_out |= t.fp_maskl[n][lane];
        r_out |= t.fp_maskr[n][lane];
    }
    Some((l_out, r_out))
}

/// `des_cipher`, `Z/ext/standard/crypt_freesec.c:574`: one 8-byte block
/// encrypted `count` times under `salt`.
fn des_cipher(block: [u8; 8], salt: u32, count: u32, keys: &Keys) -> Option<[u8; 8]> {
    let saltbits = setup_salt(salt);
    let raw_l = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
    let raw_r = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);
    let (l_out, r_out) = do_des(raw_l, raw_r, count, keys, saltbits)?;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&l_out.to_be_bytes());
    out[4..].copy_from_slice(&r_out.to_be_bytes());
    Some(out)
}

/// `_crypt_extended_r`, `Z/ext/standard/crypt_freesec.c:608`. `key` and
/// `setting` end before their first NUL; a byte past the end reads as
/// zero. A `_` setting takes 4 characters of count and 4 of salt and the
/// whole key; any other takes 2 characters of salt and 8 of key. `None` is
/// the C's NULL: a zero count, a character outside `ascii64` in an
/// extended setting, or an unsafe byte in a traditional one.
pub(super) fn crypt_extended_r(key: &[u8], setting: &[u8]) -> Option<Vec<u8>> {
    let key_at = |i: usize| key.get(i).copied().unwrap_or(0);
    let at = |i: usize| setting.get(i).copied().unwrap_or(0);

    // Copy the key, shifting each character up by one bit and padding with
    // zeros. `<< 1` drops the top bit as the C's store into `uint8_t` does.
    let mut keybuf = [0u8; 8];
    let mut k = 0;
    for slot in &mut keybuf {
        *slot = key_at(k) << 1;
        if key_at(k) != 0 {
            k += 1;
        }
    }
    let mut keys = des_setkey(keybuf);

    let count;
    let salt;
    let mut out;
    if at(0) == PASSWORD_EFMT1 {
        // "new"-style: underscore, 4 chars of count, 4 chars of salt; the
        // key is unlimited.
        let mut c = 0u32;
        for i in 1..5 {
            let value = ascii_to_bin(at(i));
            if ASCII64[usize::from(value)] != at(i) {
                return None;
            }
            c |= u32::from(value) << ((i - 1) * 6);
        }
        if c == 0 {
            return None;
        }
        count = c;

        let mut s = 0u32;
        for i in 5..9 {
            let value = ascii_to_bin(at(i));
            if ASCII64[usize::from(value)] != at(i) {
                return None;
            }
            s |= u32::from(value) << ((i - 5) * 6);
        }
        salt = s;

        while key_at(k) != 0 {
            // Encrypt the key with itself.
            keybuf = des_cipher(keybuf, 0, 1, &keys)?;
            // And XOR with the next 8 characters of the key.
            for slot in &mut keybuf {
                if key_at(k) == 0 {
                    break;
                }
                *slot ^= key_at(k) << 1;
                k += 1;
            }
            keys = des_setkey(keybuf);
        }
        out = Vec::with_capacity(20);
        out.extend_from_slice(setting.get(..9)?);
    } else {
        // "old"-style: 2 chars of salt, up to 8 characters of key.
        count = 25;
        if ascii_is_unsafe(at(0)) || ascii_is_unsafe(at(1)) {
            return None;
        }
        salt = (u32::from(ascii_to_bin(at(1))) << 6) | u32::from(ascii_to_bin(at(0)));
        out = Vec::with_capacity(13);
        out.extend_from_slice(&[at(0), at(1)]);
    }

    let (r0, r1) = do_des(0, 0, count, &keys, setup_salt(salt))?;

    // Encode the 64 bits as 11 characters.
    let digit = |v: u32| ASCII64[usize::from(v.to_le_bytes()[0] & 0x3f)];
    let l = r0 >> 8;
    out.extend_from_slice(&[digit(l >> 18), digit(l >> 12), digit(l >> 6), digit(l)]);
    let l = (r0 << 16) | ((r1 >> 16) & 0xffff);
    out.extend_from_slice(&[digit(l >> 18), digit(l >> 12), digit(l >> 6), digit(l)]);
    let l = r1 << 2;
    out.extend_from_slice(&[digit(l >> 12), digit(l >> 6), digit(l)]);
    Some(out)
}
