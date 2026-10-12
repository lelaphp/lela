//! Argon2i and Argon2id as php's `password_hash` reaches them through
//! libargon2 (phc-winner-argon2 20190702, version 0x13, the `ref.c` fill),
//! with php's own reading of argon2 hashes from `Z/ext/standard/password.c`.

use php_ext::coerce::is_space;

/// `ARGON2_SYNC_POINTS`: slices per pass.
const SYNC_POINTS: u32 = 4;
/// `ARGON2_QWORDS_IN_BLOCK`.
const QWORDS_IN_BLOCK: usize = 128;
/// `ARGON2_BLOCK_SIZE` in bytes.
const BLOCK_SIZE: usize = 1024;
/// `ARGON2_ADDRESSES_IN_BLOCK`.
const ADDRESSES_IN_BLOCK: u32 = 128;
/// `ARGON2_PREHASH_DIGEST_LENGTH`: the length of H0.
const PREHASH_DIGEST_LENGTH: usize = 64;
/// `ARGON2_PREHASH_SEED_LENGTH`: H0 and two little-endian words.
const PREHASH_SEED_LENGTH: usize = 72;
/// `ARGON2_MIN_OUTLEN`.
const MIN_OUTLEN: usize = 4;
/// `ARGON2_MIN_MEMORY`: two blocks per slice.
const MIN_MEMORY: u32 = 2 * SYNC_POINTS;
/// `ARGON2_MIN_TIME`.
const MIN_TIME: u32 = 1;
/// `ARGON2_MIN_LANES`.
const MIN_LANES: u32 = 1;
/// `ARGON2_MAX_LANES`, also `ARGON2_MAX_THREADS`.
const MAX_LANES: u32 = 0x00FF_FFFF;
/// `ARGON2_MIN_SALT_LENGTH`.
const MIN_SALT_LENGTH: usize = 8;

/// `ARGON2_VERSION_10`: the version a hash without `$v=` decodes to.
pub(super) const VERSION_10: u32 = 0x10;
/// `ARGON2_VERSION_13`, the `ARGON2_VERSION_NUMBER` php hashes with
/// (`Z/ext/standard/password.c:365`).
pub(super) const VERSION_13: u32 = 0x13;

/// `PHP_PASSWORD_ARGON2_MEMORY_COST` in KiB (`Z/ext/standard/php_password.h:34`).
pub(super) const DEFAULT_MEMORY_COST: i64 = 64 << 10;
/// `PHP_PASSWORD_ARGON2_TIME_COST` (`Z/ext/standard/php_password.h:35`).
pub(super) const DEFAULT_TIME_COST: i64 = 4;
/// `PHP_PASSWORD_ARGON2_THREADS` (`Z/ext/standard/php_password.h:36`).
pub(super) const DEFAULT_THREADS: i64 = 1;
/// The salt length php asks `php_password_get_salt` for (`Z/ext/standard/password.c:337`).
pub(super) const SALT_LEN: usize = 16;
/// The tag length php hashes to (`Z/ext/standard/password.c:341`).
pub(super) const TAG_LEN: usize = 32;

/// The `ValueError` text for a memory cost out of range (`Z/ext/standard/password.c:315`).
pub(super) const MEMORY_COST_RANGE: &str = "Memory cost is outside of allowed memory range";
/// The `ValueError` text for a time cost out of range (`Z/ext/standard/password.c:324`).
pub(super) const TIME_COST_RANGE: &str = "Time cost is outside of allowed time range";
/// The `ValueError` text for a thread count out of range (`Z/ext/standard/password.c:333`).
pub(super) const THREADS_RANGE: &str = "Invalid number of threads";

/// `argon2_type` as php offers it: `Argon2_i` and `Argon2_id` (`Z/ext/standard/password.c:384`,
/// `Z/ext/standard/password.c:403`). A `$argon2d$` hash never reaches this code: php finds
/// no algorithm named `argon2d` and hands the hash to bcrypt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Variant {
    /// `Argon2_i`, data-independent addressing throughout.
    I,
    /// `Argon2_id`, data-independent addressing for the first half of the first pass.
    Id,
}

impl Variant {
    /// The `argon2_type` number hashed into H0 and the address blocks.
    fn type_number(self) -> u32 {
        match self {
            Variant::I => 1,
            Variant::Id => 2,
        }
    }

    /// `argon2_type2string(type, 0)`: the lowercase name in the encoded form.
    pub(super) fn name(self) -> &'static str {
        match self {
            Variant::I => "argon2i",
            Variant::Id => "argon2id",
        }
    }
}

/// The `Argon2_ErrorCodes` this code can give, each with its `argon2_error_message` text,
/// which php raises as a `ValueError` (`Z/ext/standard/password.c:373`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    /// `ARGON2_OUTPUT_TOO_SHORT`.
    OutputTooShort,
    /// `ARGON2_OUTPUT_TOO_LONG`.
    OutputTooLong,
    /// `ARGON2_PWD_TOO_LONG`.
    PwdTooLong,
    /// `ARGON2_SALT_TOO_SHORT`.
    SaltTooShort,
    /// `ARGON2_SALT_TOO_LONG`.
    SaltTooLong,
    /// `ARGON2_AD_TOO_LONG`.
    AdTooLong,
    /// `ARGON2_SECRET_TOO_LONG`.
    SecretTooLong,
    /// `ARGON2_TIME_TOO_SMALL`.
    TimeTooSmall,
    /// `ARGON2_MEMORY_TOO_LITTLE`.
    MemoryTooLittle,
    /// `ARGON2_LANES_TOO_FEW`.
    LanesTooFew,
    /// `ARGON2_LANES_TOO_MANY`.
    LanesTooMany,
    /// `ARGON2_MEMORY_ALLOCATION_ERROR`.
    MemoryAllocation,
    /// `ARGON2_INCORRECT_PARAMETER`, from `fill_memory_blocks` on a bad instance.
    IncorrectParameter,
    /// `ARGON2_ENCODING_FAIL`.
    EncodingFail,
}

impl Error {
    /// `argon2_error_message`: the text php puts in the `ValueError`.
    pub(super) fn message(self) -> &'static str {
        match self {
            Error::OutputTooShort => "Output is too short",
            Error::OutputTooLong => "Output is too long",
            Error::PwdTooLong => "Password is too long",
            Error::SaltTooShort => "Salt is too short",
            Error::SaltTooLong => "Salt is too long",
            Error::AdTooLong => "Associated data is too long",
            Error::SecretTooLong => "Secret is too long",
            Error::TimeTooSmall => "Time cost is too small",
            Error::MemoryTooLittle => "Memory cost is too small",
            Error::LanesTooFew => "Too few lanes",
            Error::LanesTooMany => "Too many lanes",
            Error::MemoryAllocation => "Memory allocation error",
            Error::IncorrectParameter => "Argon2_Context context is NULL",
            Error::EncodingFail => "Encoding failed",
        }
    }
}

/// The cost fields of an `argon2_context` with its type and version: what the encoded
/// form carries besides the salt and the tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Params {
    /// The `argon2_type`.
    pub(super) variant: Variant,
    /// `version`: 0x13, or 0x10 for the older overwrite fill; any other number hashes
    /// like 0x13 but enters H0 as given.
    pub(super) version: u32,
    /// `m_cost` in KiB.
    pub(super) m_cost: u32,
    /// `t_cost`, the number of passes.
    pub(super) t_cost: u32,
    /// `lanes`, which `argon2_hash` and `decode_string` also use as `threads`.
    pub(super) lanes: u32,
}

/// One decoded hash, as `decode_string` leaves the `argon2_context`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Decoded {
    /// The type, version and costs.
    pub(super) params: Params,
    /// The salt bytes.
    pub(super) salt: Vec<u8>,
    /// The tag bytes; their count is the tag length the password is hashed to.
    pub(super) tag: Vec<u8>,
}

/// The inputs of `argon2_ctx`: an `argon2_context` without its buffers' capacities.
struct Inputs<'a> {
    params: Params,
    pwd: &'a [u8],
    salt: &'a [u8],
    secret: &'a [u8],
    ad: &'a [u8],
    outlen: usize,
}

/// True where a C `size_t` length fits the `uint32_t` field it is stored in.
fn fits_u32(len: usize) -> bool {
    u32::try_from(len).is_ok()
}

/// `validate_inputs`, in libargon2's order. The checks against `0xFFFFFFFF` on a `uint32_t`
/// field (`ARGON2_MAX_MEMORY`, `ARGON2_MAX_TIME` on LP64) cannot fire and are left out.
fn validate_inputs(inputs: &Inputs<'_>) -> Result<(), Error> {
    let p = &inputs.params;
    if inputs.outlen < MIN_OUTLEN {
        return Err(Error::OutputTooShort);
    }
    if !fits_u32(inputs.outlen) {
        return Err(Error::OutputTooLong);
    }
    if !fits_u32(inputs.pwd.len()) {
        return Err(Error::PwdTooLong);
    }
    if inputs.salt.len() < MIN_SALT_LENGTH {
        return Err(Error::SaltTooShort);
    }
    if !fits_u32(inputs.salt.len()) {
        return Err(Error::SaltTooLong);
    }
    if !fits_u32(inputs.secret.len()) {
        return Err(Error::SecretTooLong);
    }
    if !fits_u32(inputs.ad.len()) {
        return Err(Error::AdTooLong);
    }
    if p.m_cost < MIN_MEMORY {
        return Err(Error::MemoryTooLittle);
    }
    // `8 * context->lanes` is `uint32_t` arithmetic in C and wraps.
    if p.m_cost < p.lanes.wrapping_mul(8) {
        return Err(Error::MemoryTooLittle);
    }
    if p.t_cost < MIN_TIME {
        return Err(Error::TimeTooSmall);
    }
    if p.lanes < MIN_LANES {
        return Err(Error::LanesTooFew);
    }
    if p.lanes > MAX_LANES {
        return Err(Error::LanesTooMany);
    }
    // `threads` equals `lanes` on every path here, so the thread checks repeat the lane checks.
    Ok(())
}

// ---------------------------------------------------------------------------
// Blake2b, as `blake2b.c` in libargon2.

/// `blake2b_IV`.
const BLAKE2B_IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// `blake2b_sigma`.
const BLAKE2B_SIGMA: [[u8; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

/// `BLAKE2B_BLOCKBYTES`.
const BLAKE2B_BLOCKBYTES: usize = 128;
/// `BLAKE2B_OUTBYTES`.
const BLAKE2B_OUTBYTES: usize = 64;

/// `blake2b_state`, unkeyed, with no last-node bit (Argon2 never sets one).
struct Blake2b {
    h: [u64; 8],
    t: [u64; 2],
    finished: bool,
    buf: [u8; BLAKE2B_BLOCKBYTES],
    buflen: usize,
    outlen: usize,
}

/// `load64`: eight little-endian bytes.
fn load64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    for (dst, src) in word.iter_mut().zip(bytes) {
        *dst = *src;
    }
    u64::from_le_bytes(word)
}

impl Blake2b {
    /// `blake2b_init`: `None` where it returns -1, an `outlen` of 0 or above 64.
    fn new(outlen: usize) -> Option<Blake2b> {
        if outlen == 0 || outlen > BLAKE2B_OUTBYTES {
            return None;
        }
        let mut h = BLAKE2B_IV;
        // The parameter block's first word: digest length, key length 0, fanout 1, depth 1;
        // the rest of the block is zero. `outlen` is at most 64 here.
        h[0] ^= 0x0101_0000 ^ outlen as u64;
        Some(Blake2b {
            h,
            t: [0, 0],
            finished: false,
            buf: [0; BLAKE2B_BLOCKBYTES],
            buflen: 0,
            outlen,
        })
    }

    /// `blake2b_increment_counter`: the 128-bit byte count, wrapping as in C.
    fn increment(&mut self, inc: u64) {
        self.t[0] = self.t[0].wrapping_add(inc);
        self.t[1] = self.t[1].wrapping_add(u64::from(self.t[0] < inc));
    }

    /// `blake2b_compress` over one 128-byte block.
    fn compress(&mut self, block: &[u8]) {
        let mut m = [0u64; 16];
        for (word, chunk) in m.iter_mut().zip(block.chunks(8)) {
            *word = load64(chunk);
        }
        let mut v = [0u64; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&BLAKE2B_IV);
        v[12] ^= self.t[0];
        v[13] ^= self.t[1];
        if self.finished {
            v[14] = !v[14];
        }
        macro_rules! g {
            ($s:expr, $i:expr, $a:expr, $b:expr, $c:expr, $d:expr) => {
                v[$a] = v[$a]
                    .wrapping_add(v[$b])
                    .wrapping_add(m[usize::from($s[2 * $i])]);
                v[$d] = (v[$d] ^ v[$a]).rotate_right(32);
                v[$c] = v[$c].wrapping_add(v[$d]);
                v[$b] = (v[$b] ^ v[$c]).rotate_right(24);
                v[$a] = v[$a]
                    .wrapping_add(v[$b])
                    .wrapping_add(m[usize::from($s[2 * $i + 1])]);
                v[$d] = (v[$d] ^ v[$a]).rotate_right(16);
                v[$c] = v[$c].wrapping_add(v[$d]);
                v[$b] = (v[$b] ^ v[$c]).rotate_right(63);
            };
        }
        for s in &BLAKE2B_SIGMA {
            g!(s, 0, 0, 4, 8, 12);
            g!(s, 1, 1, 5, 9, 13);
            g!(s, 2, 2, 6, 10, 14);
            g!(s, 3, 3, 7, 11, 15);
            g!(s, 4, 0, 5, 10, 15);
            g!(s, 5, 1, 6, 11, 12);
            g!(s, 6, 2, 7, 8, 13);
            g!(s, 7, 3, 4, 9, 14);
        }
        for (i, h) in self.h.iter_mut().enumerate() {
            *h ^= v[i] ^ v[i + 8];
        }
    }

    /// `blake2b_update`: the last block stays buffered until `blake2b_final`.
    fn update(&mut self, input: &[u8]) {
        if input.is_empty() || self.finished {
            return;
        }
        let mut input = input;
        if self.buflen + input.len() > BLAKE2B_BLOCKBYTES {
            let fill = BLAKE2B_BLOCKBYTES - self.buflen;
            let (head, rest) = input.split_at(fill);
            self.buf[self.buflen..].copy_from_slice(head);
            self.increment(BLAKE2B_BLOCKBYTES as u64);
            let buf = self.buf;
            self.compress(&buf);
            self.buflen = 0;
            input = rest;
            while input.len() > BLAKE2B_BLOCKBYTES {
                let (block, rest) = input.split_at(BLAKE2B_BLOCKBYTES);
                self.increment(BLAKE2B_BLOCKBYTES as u64);
                self.compress(block);
                input = rest;
            }
        }
        self.buf[self.buflen..self.buflen + input.len()].copy_from_slice(input);
        self.buflen += input.len();
    }

    /// `blake2b_final`: writes the first `outlen` digest bytes into `out`.
    fn finalize(mut self, out: &mut [u8]) {
        if self.finished {
            return;
        }
        self.increment(self.buflen as u64);
        self.finished = true;
        self.buf[self.buflen..].fill(0);
        let buf = self.buf;
        self.compress(&buf);
        let mut digest = [0u8; BLAKE2B_OUTBYTES];
        for (chunk, h) in digest.chunks_mut(8).zip(&self.h) {
            chunk.copy_from_slice(&h.to_le_bytes());
        }
        for (dst, src) in out.iter_mut().zip(&digest[..self.outlen]) {
            *dst = *src;
        }
    }
}

/// `blake2b`: the unkeyed one-shot digest of `input` into `out` (1 to 64 bytes).
fn blake2b(out: &mut [u8], input: &[u8]) {
    if let Some(mut state) = Blake2b::new(out.len()) {
        state.update(input);
        state.finalize(out);
    }
}

/// `blake2b_long`, H' of the Argon2 paper: `out.len()` bytes from `input`, the length
/// first as a little-endian word. A length above `0xFFFFFFFF` or of 0 writes nothing.
fn blake2b_long(out: &mut [u8], input: &[u8]) {
    let Ok(outlen) = u32::try_from(out.len()) else {
        return;
    };
    let outlen_bytes = outlen.to_le_bytes();
    if out.len() <= BLAKE2B_OUTBYTES {
        if let Some(mut state) = Blake2b::new(out.len()) {
            state.update(&outlen_bytes);
            state.update(input);
            state.finalize(out);
        }
        return;
    }
    let Some(mut state) = Blake2b::new(BLAKE2B_OUTBYTES) else {
        return;
    };
    let mut out_buffer = [0u8; BLAKE2B_OUTBYTES];
    state.update(&outlen_bytes);
    state.update(input);
    state.finalize(&mut out_buffer);
    let half = BLAKE2B_OUTBYTES / 2;
    let (first, mut rest) = out.split_at_mut(half);
    first.copy_from_slice(&out_buffer[..half]);
    while rest.len() > BLAKE2B_OUTBYTES {
        let in_buffer = out_buffer;
        blake2b(&mut out_buffer, &in_buffer);
        let (chunk, tail) = rest.split_at_mut(half);
        chunk.copy_from_slice(&out_buffer[..half]);
        rest = tail;
    }
    let in_buffer = out_buffer;
    blake2b(rest, &in_buffer);
}

// ---------------------------------------------------------------------------
// The memory fill, as `ref.c` and `core.c` in libargon2.

/// `block`: 128 little-endian words.
type Block = [u64; QWORDS_IN_BLOCK];

/// A block of zeros, `init_block_value(&b, 0)`.
const ZERO_BLOCK: Block = [0; QWORDS_IN_BLOCK];

/// The sixteen word indices of each `BLAKE2_ROUND_NOMSG` in `fill_block`: eight over
/// columns of sixteen consecutive words, then eight over rows of word pairs.
const ROUNDS: [[usize; 16]; 16] = round_indices();

/// Builds `ROUNDS` from the index pattern `fill_block` spells out.
const fn round_indices() -> [[usize; 16]; 16] {
    let mut out = [[0usize; 16]; 16];
    let mut i = 0;
    while i < 8 {
        let mut k = 0;
        while k < 16 {
            out[i][k] = 16 * i + k;
            out[8 + i][k] = 2 * i + (k / 2) * 16 + (k % 2);
            k += 1;
        }
        i += 1;
    }
    out
}

/// `fBlaMka`: `x + y + 2 * lo32(x) * lo32(y)`, wrapping as in C.
macro_rules! fblamka {
    ($x:expr, $y:expr) => {{
        let x: u64 = $x;
        let y: u64 = $y;
        let xy = (x & 0xFFFF_FFFF).wrapping_mul(y & 0xFFFF_FFFF);
        x.wrapping_add(y).wrapping_add(xy.wrapping_mul(2))
    }};
}

/// `G` of `blamka-round-ref.h` over four locals.
macro_rules! blamka_g {
    ($a:ident, $b:ident, $c:ident, $d:ident) => {
        $a = fblamka!($a, $b);
        $d = ($d ^ $a).rotate_right(32);
        $c = fblamka!($c, $d);
        $b = ($b ^ $c).rotate_right(24);
        $a = fblamka!($a, $b);
        $d = ($d ^ $a).rotate_right(16);
        $c = fblamka!($c, $d);
        $b = ($b ^ $c).rotate_right(63);
    };
}

/// `BLAKE2_ROUND_NOMSG` over the sixteen words of `r` that `idx` names.
fn blamka_round(r: &mut Block, idx: &[usize; 16]) {
    // Every index in `ROUNDS` is below 128 by construction.
    let [
        i0,
        i1,
        i2,
        i3,
        i4,
        i5,
        i6,
        i7,
        i8,
        i9,
        i10,
        i11,
        i12,
        i13,
        i14,
        i15,
    ] = *idx;
    let (mut v0, mut v1, mut v2, mut v3) = (r[i0], r[i1], r[i2], r[i3]);
    let (mut v4, mut v5, mut v6, mut v7) = (r[i4], r[i5], r[i6], r[i7]);
    let (mut v8, mut v9, mut v10, mut v11) = (r[i8], r[i9], r[i10], r[i11]);
    let (mut v12, mut v13, mut v14, mut v15) = (r[i12], r[i13], r[i14], r[i15]);
    blamka_g!(v0, v4, v8, v12);
    blamka_g!(v1, v5, v9, v13);
    blamka_g!(v2, v6, v10, v14);
    blamka_g!(v3, v7, v11, v15);
    blamka_g!(v0, v5, v10, v15);
    blamka_g!(v1, v6, v11, v12);
    blamka_g!(v2, v7, v8, v13);
    blamka_g!(v3, v4, v9, v14);
    (r[i0], r[i1], r[i2], r[i3]) = (v0, v1, v2, v3);
    (r[i4], r[i5], r[i6], r[i7]) = (v4, v5, v6, v7);
    (r[i8], r[i9], r[i10], r[i11]) = (v8, v9, v10, v11);
    (r[i12], r[i13], r[i14], r[i15]) = (v12, v13, v14, v15);
}

/// `fill_block` of `ref.c`, given `ref_xor_prev`, the reference block XOR the previous
/// block, already formed; with `with_xor` the old `next` is folded in (version 0x13 after
/// the first pass).
fn fill_block(ref_xor_prev: &Block, next: &mut Block, with_xor: bool) {
    let mut block_r = *ref_xor_prev;
    let mut block_tmp = *ref_xor_prev;
    if with_xor {
        for (t, n) in block_tmp.iter_mut().zip(next.iter()) {
            *t ^= *n;
        }
    }
    for idx in &ROUNDS {
        blamka_round(&mut block_r, idx);
    }
    for ((n, t), r) in next.iter_mut().zip(block_tmp.iter()).zip(block_r.iter()) {
        *n = *t ^ *r;
    }
}

/// `next_addresses`: bumps word 6 of the input block and makes the next block of
/// 128 pseudo-random reference words.
fn next_addresses(address_block: &mut Block, input_block: &mut Block) {
    input_block[6] = input_block[6].wrapping_add(1);
    // fill_block(zero, input, address, 0): R = input ^ zero.
    fill_block(input_block, address_block, false);
    // fill_block(zero, address, address, 0): R = address ^ zero.
    let r = *address_block;
    fill_block(&r, address_block, false);
}

/// `argon2_instance_t`: the memory and its shape.
struct Instance {
    memory: Vec<Block>,
    version: u32,
    passes: u32,
    memory_blocks: u32,
    segment_length: u32,
    lane_length: u32,
    lanes: u32,
    variant: Variant,
}

/// `index_alpha`: maps a pseudo-random word to a block index within the reference lane.
/// `uint32_t` arithmetic wraps where C's does.
fn index_alpha(
    instance: &Instance,
    pass: u32,
    slice: u32,
    index: u32,
    pseudo_rand: u32,
    same_lane: bool,
) -> u32 {
    let minus_one_at_start = if index == 0 { u32::MAX } else { 0 };
    let reference_area_size: u32 = if pass == 0 {
        if slice == 0 {
            index.wrapping_sub(1)
        } else if same_lane {
            slice
                .wrapping_mul(instance.segment_length)
                .wrapping_add(index)
                .wrapping_sub(1)
        } else {
            slice
                .wrapping_mul(instance.segment_length)
                .wrapping_add(minus_one_at_start)
        }
    } else if same_lane {
        instance
            .lane_length
            .wrapping_sub(instance.segment_length)
            .wrapping_add(index)
            .wrapping_sub(1)
    } else {
        instance
            .lane_length
            .wrapping_sub(instance.segment_length)
            .wrapping_add(minus_one_at_start)
    };

    let mut relative_position = u64::from(pseudo_rand);
    relative_position = relative_position.wrapping_mul(relative_position) >> 32;
    relative_position = u64::from(reference_area_size.wrapping_sub(1))
        .wrapping_sub(u64::from(reference_area_size).wrapping_mul(relative_position) >> 32);

    let start_position: u32 = if pass != 0 && slice != SYNC_POINTS - 1 {
        slice.wrapping_add(1).wrapping_mul(instance.segment_length)
    } else {
        0
    };

    let absolute =
        u64::from(start_position).wrapping_add(relative_position) % u64::from(instance.lane_length);
    // Below `lane_length`, so it fits the `uint32_t` C stores it in.
    u32::try_from(absolute).unwrap_or(0)
}

/// `fill_segment` of `ref.c`: fills one lane's blocks of one slice of one pass.
fn fill_segment(instance: &mut Instance, pass: u32, lane: u32, slice: u32) -> Result<(), Error> {
    let data_independent_addressing = instance.variant == Variant::I
        || (instance.variant == Variant::Id && pass == 0 && slice < SYNC_POINTS / 2);

    let mut address_block = ZERO_BLOCK;
    let mut input_block = ZERO_BLOCK;
    if data_independent_addressing {
        input_block[0] = u64::from(pass);
        input_block[1] = u64::from(lane);
        input_block[2] = u64::from(slice);
        input_block[3] = u64::from(instance.memory_blocks);
        input_block[4] = u64::from(instance.passes);
        input_block[5] = u64::from(instance.variant.type_number());
    }

    let mut starting_index = 0;
    if pass == 0 && slice == 0 {
        // The first two blocks of each lane are already made.
        starting_index = 2;
        if data_independent_addressing {
            next_addresses(&mut address_block, &mut input_block);
        }
    }

    let lane_length = instance.lane_length;
    let mut curr_offset = lane
        .wrapping_mul(lane_length)
        .wrapping_add(slice.wrapping_mul(instance.segment_length))
        .wrapping_add(starting_index);
    let mut prev_offset = if curr_offset % lane_length == 0 {
        // The last block of this lane.
        curr_offset.wrapping_add(lane_length).wrapping_sub(1)
    } else {
        curr_offset.wrapping_sub(1)
    };

    for i in starting_index..instance.segment_length {
        if curr_offset % lane_length == 1 {
            prev_offset = curr_offset.wrapping_sub(1);
        }

        let prev = instance
            .memory
            .get(prev_offset as usize)
            .ok_or(Error::IncorrectParameter)?;
        let pseudo_rand: u64 = if data_independent_addressing {
            if i % ADDRESSES_IN_BLOCK == 0 {
                next_addresses(&mut address_block, &mut input_block);
            }
            address_block[(i % ADDRESSES_IN_BLOCK) as usize]
        } else {
            prev[0]
        };

        let mut ref_lane = (pseudo_rand >> 32) % u64::from(instance.lanes);
        if pass == 0 && slice == 0 {
            // No other lane can be referenced yet.
            ref_lane = u64::from(lane);
        }

        // The low half of the word, as C's `pseudo_rand & 0xFFFFFFFF`.
        let ref_index = index_alpha(
            instance,
            pass,
            slice,
            i,
            u32::try_from(pseudo_rand & 0xFFFF_FFFF).unwrap_or(0),
            ref_lane == u64::from(lane),
        );

        let ref_offset = u64::from(lane_length)
            .wrapping_mul(ref_lane)
            .wrapping_add(u64::from(ref_index));
        let reference = usize::try_from(ref_offset)
            .ok()
            .and_then(|at| instance.memory.get(at))
            .ok_or(Error::IncorrectParameter)?;
        let mut ref_xor_prev = *reference;
        for (r, p) in ref_xor_prev.iter_mut().zip(prev.iter()) {
            *r ^= *p;
        }

        // Version 0x10 overwrites; 0x13 folds the old block in after the first pass.
        let with_xor = instance.version != VERSION_10 && pass != 0;
        let curr = instance
            .memory
            .get_mut(curr_offset as usize)
            .ok_or(Error::IncorrectParameter)?;
        fill_block(&ref_xor_prev, curr, with_xor);

        curr_offset = curr_offset.wrapping_add(1);
        prev_offset = prev_offset.wrapping_add(1);
    }
    Ok(())
}

/// `fill_memory_blocks`: every segment in pass, slice, lane order. libargon2 fills the
/// lanes of one slice on threads; no lane reads another lane's current slice, so filling
/// them one after another gives the same bytes.
fn fill_memory_blocks(instance: &mut Instance) -> Result<(), Error> {
    if instance.lanes == 0 {
        return Err(Error::IncorrectParameter);
    }
    for pass in 0..instance.passes {
        for slice in 0..SYNC_POINTS {
            for lane in 0..instance.lanes {
                fill_segment(instance, pass, lane, slice)?;
            }
        }
    }
    Ok(())
}

/// `initial_hash`: H0 over the costs, the type, the version and every input with its length.
fn initial_hash(inputs: &Inputs<'_>) -> [u8; PREHASH_DIGEST_LENGTH] {
    let mut out = [0u8; PREHASH_DIGEST_LENGTH];
    let Some(mut state) = Blake2b::new(PREHASH_DIGEST_LENGTH) else {
        return out;
    };
    let p = &inputs.params;
    // Lengths were checked to fit `uint32_t` by `validate_inputs`; C stores them so.
    let word = |n: usize| u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes();
    state.update(&p.lanes.to_le_bytes());
    state.update(&word(inputs.outlen));
    state.update(&p.m_cost.to_le_bytes());
    state.update(&p.t_cost.to_le_bytes());
    state.update(&p.version.to_le_bytes());
    state.update(&p.variant.type_number().to_le_bytes());
    state.update(&word(inputs.pwd.len()));
    state.update(inputs.pwd);
    state.update(&word(inputs.salt.len()));
    state.update(inputs.salt);
    state.update(&word(inputs.secret.len()));
    state.update(inputs.secret);
    state.update(&word(inputs.ad.len()));
    state.update(inputs.ad);
    state.finalize(&mut out);
    out
}

/// `load_block`: 1024 bytes as 128 little-endian words.
fn load_block(dst: &mut Block, bytes: &[u8; BLOCK_SIZE]) {
    for (word, chunk) in dst.iter_mut().zip(bytes.chunks(8)) {
        *word = load64(chunk);
    }
}

/// `fill_first_blocks`: blocks 0 and 1 of every lane from H0, the block number and the lane.
fn fill_first_blocks(blockhash: &mut [u8; PREHASH_SEED_LENGTH], instance: &mut Instance) {
    let mut blockhash_bytes = [0u8; BLOCK_SIZE];
    for lane in 0..instance.lanes {
        let base = u64::from(lane) * u64::from(instance.lane_length);
        for n in 0..2u32 {
            blockhash[PREHASH_DIGEST_LENGTH..PREHASH_DIGEST_LENGTH + 4]
                .copy_from_slice(&n.to_le_bytes());
            blockhash[PREHASH_DIGEST_LENGTH + 4..].copy_from_slice(&lane.to_le_bytes());
            blake2b_long(&mut blockhash_bytes, blockhash);
            let at = usize::try_from(base + u64::from(n)).ok();
            if let Some(block) = at.and_then(|at| instance.memory.get_mut(at)) {
                load_block(block, &blockhash_bytes);
            }
        }
    }
}

/// `finalize`: XOR of every lane's last block, stretched by H' to the tag length.
fn finalize(instance: &Instance, outlen: usize) -> Vec<u8> {
    let mut blockhash = ZERO_BLOCK;
    let last = instance.lane_length.wrapping_sub(1) as usize;
    for lane in 0..instance.lanes {
        let at = (lane as usize)
            .wrapping_mul(instance.lane_length as usize)
            .wrapping_add(last);
        if let Some(block) = instance.memory.get(at) {
            for (h, w) in blockhash.iter_mut().zip(block.iter()) {
                *h ^= *w;
            }
        }
    }
    let mut blockhash_bytes = [0u8; BLOCK_SIZE];
    for (chunk, w) in blockhash_bytes.chunks_mut(8).zip(blockhash.iter()) {
        chunk.copy_from_slice(&w.to_le_bytes());
    }
    let mut out = vec![0u8; outlen];
    blake2b_long(&mut out, &blockhash_bytes);
    out
}

/// `argon2_ctx`: validates, sizes and allocates the memory, fills it and returns the tag.
fn argon2_ctx(inputs: &Inputs<'_>) -> Result<Vec<u8>, Error> {
    validate_inputs(inputs)?;
    let p = inputs.params;

    // At least 8 blocks per lane, then a whole number of segments.
    let mut memory_blocks = p.m_cost;
    let min_blocks = 2u32.wrapping_mul(SYNC_POINTS).wrapping_mul(p.lanes);
    if memory_blocks < min_blocks {
        memory_blocks = min_blocks;
    }
    let lanes_by_slices = p.lanes.wrapping_mul(SYNC_POINTS);
    let segment_length = memory_blocks
        .checked_div(lanes_by_slices)
        .ok_or(Error::IncorrectParameter)?;
    if segment_length == 0 {
        // Unreachable after `validate_inputs`; it keeps every `% lane_length` defined.
        return Err(Error::IncorrectParameter);
    }
    memory_blocks = segment_length.wrapping_mul(lanes_by_slices);

    // `allocate_memory`: a failed allocation is `ARGON2_MEMORY_ALLOCATION_ERROR`.
    let count = memory_blocks as usize;
    let mut memory: Vec<Block> = Vec::new();
    memory
        .try_reserve_exact(count)
        .map_err(|_| Error::MemoryAllocation)?;
    memory.resize(count, ZERO_BLOCK);

    let mut instance = Instance {
        memory,
        version: p.version,
        passes: p.t_cost,
        memory_blocks,
        segment_length,
        lane_length: segment_length.wrapping_mul(SYNC_POINTS),
        lanes: p.lanes,
        variant: p.variant,
    };

    let mut blockhash = [0u8; PREHASH_SEED_LENGTH];
    blockhash[..PREHASH_DIGEST_LENGTH].copy_from_slice(&initial_hash(inputs));
    fill_first_blocks(&mut blockhash, &mut instance);
    fill_memory_blocks(&mut instance)?;
    Ok(finalize(&instance, inputs.outlen))
}

/// `argon2_hash` for the raw tag, with no secret and no associated data: `tag_len` bytes.
/// The errors are libargon2's: `PwdTooLong`, `SaltTooLong`, `OutputTooLong` and
/// `OutputTooShort` first, then `validate_inputs` (`SaltTooShort` under 8 bytes,
/// `MemoryTooLittle` under 8 KiB or under `8 * lanes`, `TimeTooSmall` at 0, `LanesTooFew`
/// at 0, `LanesTooMany` above `0xFFFFFF`), then `MemoryAllocation`.
pub(super) fn hash_raw(
    params: &Params,
    password: &[u8],
    salt: &[u8],
    tag_len: usize,
) -> Result<Vec<u8>, Error> {
    hash_with(params, password, salt, &[], &[], tag_len)
}

/// `argon2_hash` over an `argon2_context` that also carries a secret and associated data.
fn hash_with(
    params: &Params,
    password: &[u8],
    salt: &[u8],
    secret: &[u8],
    ad: &[u8],
    tag_len: usize,
) -> Result<Vec<u8>, Error> {
    if !fits_u32(password.len()) {
        return Err(Error::PwdTooLong);
    }
    if !fits_u32(salt.len()) {
        return Err(Error::SaltTooLong);
    }
    if !fits_u32(tag_len) {
        return Err(Error::OutputTooLong);
    }
    if tag_len < MIN_OUTLEN {
        return Err(Error::OutputTooShort);
    }
    argon2_ctx(&Inputs {
        params: *params,
        pwd: password,
        salt,
        secret,
        ad,
        outlen: tag_len,
    })
}

/// `argon2_hash` with both the raw tag and the encoded form asked for, as php calls it
/// (`Z/ext/standard/password.c:352`): the encoded string, or the error whose message php
/// raises. A failed `encode_string` is `EncodingFail`.
pub(super) fn hash_encoded(
    params: &Params,
    password: &[u8],
    salt: &[u8],
    tag_len: usize,
) -> Result<String, Error> {
    let tag = hash_raw(params, password, salt, tag_len)?;
    encode(params, salt, &tag).map_err(|_| Error::EncodingFail)
}

// ---------------------------------------------------------------------------
// The encoded form, as `encoding.c` in libargon2.

/// `b64_byte_to_char`: one 6-bit value as a standard Base64 character.
fn b64_byte_to_char(x: u32) -> char {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // `x` is masked to six bits by every caller.
    char::from(ALPHABET[(x & 0x3F) as usize])
}

/// `b64_char_to_byte`: the 6-bit value of a Base64 character, or 0xFF.
fn b64_char_to_byte(c: u8) -> u32 {
    match c {
        b'A'..=b'Z' => u32::from(c - b'A'),
        b'a'..=b'z' => u32::from(c - b'a') + 26,
        b'0'..=b'9' => u32::from(c - b'0') + 52,
        b'+' => 62,
        b'/' => 63,
        _ => 0xFF,
    }
}

/// `to_base64`: Base64 without padding, appended to `dst`.
fn to_base64(dst: &mut String, src: &[u8]) {
    let mut acc: u32 = 0;
    let mut acc_len: u32 = 0;
    for &byte in src {
        acc = (acc << 8).wrapping_add(u32::from(byte));
        acc_len += 8;
        while acc_len >= 6 {
            acc_len -= 6;
            dst.push(b64_byte_to_char(acc >> acc_len));
        }
    }
    if acc_len > 0 {
        dst.push(b64_byte_to_char(acc << (6 - acc_len)));
    }
}

/// `encode_string`: `$argon2id$v=19$m=65536,t=4,p=1$<salt>$<tag>`. The inputs pass through
/// `validate_inputs` first, as libargon2 does.
pub(super) fn encode(params: &Params, salt: &[u8], tag: &[u8]) -> Result<String, Error> {
    validate_inputs(&Inputs {
        params: *params,
        pwd: &[],
        salt,
        secret: &[],
        ad: &[],
        outlen: tag.len(),
    })?;
    let mut out = String::new();
    out.push('$');
    out.push_str(params.variant.name());
    out.push_str("$v=");
    out.push_str(&params.version.to_string());
    out.push_str("$m=");
    out.push_str(&params.m_cost.to_string());
    out.push_str(",t=");
    out.push_str(&params.t_cost.to_string());
    out.push_str(",p=");
    out.push_str(&params.lanes.to_string());
    out.push('$');
    to_base64(&mut out, salt);
    out.push('$');
    to_base64(&mut out, tag);
    Ok(out)
}

/// A C string walked by `decode_string`: reading past the end gives the terminating 0.
struct CStr<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl CStr<'_> {
    /// The byte under the cursor, 0 at the end.
    fn cur(&self) -> u8 {
        self.bytes.get(self.at).copied().unwrap_or(0)
    }

    /// `CC` / `CC_opt`: steps over `prefix` if it is next; false otherwise.
    fn eat(&mut self, prefix: &[u8]) -> bool {
        let rest = self.bytes.get(self.at..).unwrap_or(&[]);
        if rest.starts_with(prefix) {
            self.at += prefix.len();
            true
        } else {
            false
        }
    }

    /// `decode_decimal` then the `DECIMAL_U32` range check: at least one digit, no
    /// leading zero, no `unsigned long` overflow, at most `0xFFFFFFFF`.
    fn decimal_u32(&mut self) -> Option<u32> {
        let orig = self.at;
        let mut acc: u64 = 0;
        loop {
            let c = self.cur();
            if !c.is_ascii_digit() {
                break;
            }
            let digit = u64::from(c - b'0');
            if acc > u64::MAX / 10 {
                return None;
            }
            acc *= 10;
            if digit > u64::MAX - acc {
                return None;
            }
            acc += digit;
            self.at += 1;
        }
        let digits = self.at - orig;
        if digits == 0 || (self.bytes.get(orig) == Some(&b'0') && digits != 1) {
            return None;
        }
        u32::try_from(acc).ok()
    }

    /// `from_base64` under the `BIN` macro: decodes up to the first non-Base64 byte into at
    /// most `max_len` bytes; `None` past `max_len`, on a length of 1 modulo 4, or on
    /// nonzero bits left over.
    fn base64(&mut self, max_len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut acc: u32 = 0;
        let mut acc_len: u32 = 0;
        loop {
            let d = b64_char_to_byte(self.cur());
            if d == 0xFF {
                break;
            }
            self.at += 1;
            acc = (acc << 6).wrapping_add(d);
            acc_len += 6;
            if acc_len >= 8 {
                acc_len -= 8;
                if out.len() >= max_len {
                    return None;
                }
                // The low byte, as C's `& 0xFF` into an `unsigned char`.
                out.push((acc >> acc_len).to_le_bytes()[0]);
            }
        }
        if acc_len > 4 || (acc & ((1u32 << acc_len) - 1)) != 0 {
            return None;
        }
        if !fits_u32(out.len()) {
            return None;
        }
        Some(out)
    }
}

/// `decode_string` for the type php's algorithm names (`Z/ext/standard/password.c:384`,
/// `Z/ext/standard/password.c:403`): `$<type>[$v=<n>]$m=<n>,t=<n>,p=<n>$<salt>$<tag>`,
/// read as a C string (up to the first NUL byte). A missing `$v=` is version 0x10; the
/// version number is not checked. The decoded fields must pass `validate_inputs`, and
/// nothing may follow the tag. The salt and tag capacities are the string's length, as
/// `argon2_verify` sizes them.
pub(super) fn decode(encoded: &[u8], variant: Variant) -> Option<Decoded> {
    let encoded = c_string(encoded);
    let max_len = encoded.len();
    let mut s = CStr {
        bytes: encoded,
        at: 0,
    };
    if !s.eat(b"$") || !s.eat(variant.name().as_bytes()) {
        return None;
    }
    let mut version = VERSION_10;
    if s.eat(b"$v=") {
        version = s.decimal_u32()?;
    }
    if !s.eat(b"$m=") {
        return None;
    }
    let m_cost = s.decimal_u32()?;
    if !s.eat(b",t=") {
        return None;
    }
    let t_cost = s.decimal_u32()?;
    if !s.eat(b",p=") {
        return None;
    }
    let lanes = s.decimal_u32()?;
    if !s.eat(b"$") {
        return None;
    }
    let salt = s.base64(max_len)?;
    if !s.eat(b"$") {
        return None;
    }
    let tag = s.base64(max_len)?;

    let params = Params {
        variant,
        version,
        m_cost,
        t_cost,
        lanes,
    };
    validate_inputs(&Inputs {
        params,
        pwd: &[],
        salt: &salt,
        secret: &[],
        ad: &[],
        outlen: tag.len(),
    })
    .ok()?;
    if s.cur() != 0 {
        return None;
    }
    Some(Decoded { params, salt, tag })
}

/// The bytes before the first NUL, as `strlen` sees a `char *`.
fn c_string(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes.get(..end).unwrap_or(bytes)
}

/// `argon2_compare`: equality of two tags of one length without an early exit.
fn tags_equal(a: &[u8], b: &[u8]) -> bool {
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    d == 0 && a.len() == b.len()
}

/// `argon2_verify` as php calls it (`Z/ext/standard/password.c:384`,
/// `Z/ext/standard/password.c:403`): true where it returns `ARGON2_OK`. The hash is read up
/// to its first NUL; the password is hashed with the decoded type, version, costs and
/// salt to the decoded tag's length and compared with it.
pub(super) fn verify(encoded: &[u8], password: &[u8], variant: Variant) -> bool {
    if !fits_u32(password.len()) {
        return false;
    }
    let encoded = c_string(encoded);
    if !fits_u32(encoded.len()) {
        return false;
    }
    let Some(decoded) = decode(encoded, variant) else {
        return false;
    };
    match hash_raw(&decoded.params, password, &decoded.salt, decoded.tag.len()) {
        Ok(tag) => tags_equal(&decoded.tag, &tag),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// php's side: option checks, and reading costs back out of a hash.

/// The memory cost check of `php_password_argon2_hash` (`Z/ext/standard/password.c:314`):
/// the `zend_long` is stored in a `size_t`, so a negative cost is huge; the cost must lie
/// in `8..=0xFFFFFFFF` KiB. `None` means the `MEMORY_COST_RANGE` error.
pub(super) fn memory_cost_in_range(memory_cost: i64) -> Option<u32> {
    // C converts `zend_long` to `size_t`, keeping the bits.
    let as_size_t = memory_cost.cast_unsigned();
    u32::try_from(as_size_t).ok().filter(|&m| m >= MIN_MEMORY)
}

/// The time cost check of `php_password_argon2_hash` (`Z/ext/standard/password.c:323`):
/// `1..=0xFFFFFFFF` after the same `size_t` conversion. `None` means `TIME_COST_RANGE`.
pub(super) fn time_cost_in_range(time_cost: i64) -> Option<u32> {
    // C converts `zend_long` to `size_t`, keeping the bits.
    let as_size_t = time_cost.cast_unsigned();
    u32::try_from(as_size_t).ok().filter(|&t| t >= MIN_TIME)
}

/// The thread count check of `php_password_argon2_hash` (`Z/ext/standard/password.c:332`):
/// `1..=0xFFFFFF` after the same `size_t` conversion. `None` means `THREADS_RANGE`.
pub(super) fn threads_in_range(threads: i64) -> Option<u32> {
    // C converts `zend_long` to `size_t`, keeping the bits.
    let as_size_t = threads.cast_unsigned();
    u32::try_from(as_size_t)
        .ok()
        .filter(|&p| p != 0 && p <= MAX_LANES)
}

/// The `argon2_hash` call of `php_password_argon2_hash` (`Z/ext/standard/password.c:352`):
/// version 0x13, a 32-byte tag, the costs already checked, `salt` the 16 characters
/// `php_password_get_salt` made. `Err` carries the `ValueError` text's source.
pub(super) fn password_hash(
    variant: Variant,
    password: &[u8],
    salt: &[u8],
    memory_cost: u32,
    time_cost: u32,
    threads: u32,
) -> Result<String, Error> {
    let params = Params {
        variant,
        version: VERSION_13,
        m_cost: memory_cost,
        t_cost: time_cost,
        lanes: threads,
    };
    hash_encoded(&params, password, salt, TAG_LEN)
}

/// The four numbers `extract_argon2_parameters` reads (`Z/ext/standard/password.c:237`),
/// as `zend_long`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Costs {
    /// `v`.
    pub(super) version: i64,
    /// `memory_cost`.
    pub(super) memory_cost: i64,
    /// `time_cost`.
    pub(super) time_cost: i64,
    /// `threads`.
    pub(super) threads: i64,
}

/// `extract_argon2_parameters` (`Z/ext/standard/password.c:237`): false for a hash under
/// 11 bytes or not starting `$argon2i$` or `$argon2id$`; otherwise the
/// `sscanf("v=%ld$m=%ld,t=%ld,p=%ld")` of the rest, which overwrites the fields of `costs`
/// in order up to the first that fails to match and leaves the others alone.
pub(super) fn extract_parameters(hash: &[u8], costs: &mut Costs) -> bool {
    if hash.len() < b"$argon2id$".len() + 1 {
        return false;
    }
    let rest = if let Some(rest) = hash.strip_prefix(b"$argon2i$") {
        rest
    } else if let Some(rest) = hash.strip_prefix(b"$argon2id$") {
        rest
    } else {
        return false;
    };
    let mut s = CStr {
        bytes: c_string(rest),
        at: 0,
    };
    let fields: [(&[u8], &mut i64); 4] = [
        (b"v=", &mut costs.version),
        (b"$m=", &mut costs.memory_cost),
        (b",t=", &mut costs.time_cost),
        (b",p=", &mut costs.threads),
    ];
    for (prefix, field) in fields {
        if !s.eat(prefix) {
            break;
        }
        match scan_long(&mut s) {
            Some(n) => *field = n,
            None => break,
        }
    }
    true
}

/// One `%ld` of the BSD `vfscanf` php's libc uses: skips spaces, then takes a sign and
/// decimal digits, at most 512 bytes in all, and converts them with `strtoimax`, which
/// saturates. `None` where no digit was taken, the match failure that ends `sscanf`.
fn scan_long(s: &mut CStr<'_>) -> Option<i64> {
    while s.cur() != 0 && is_space(s.cur()) {
        s.at += 1;
    }
    // `char buf[BUF]` with `BUF` 513 caps the field width at 512.
    let mut width = 512usize;
    let mut negative = false;
    let mut sign_ok = true;
    let mut any_digit = false;
    let mut value: i64 = 0;
    while width > 0 {
        let c = s.cur();
        match c {
            b'+' | b'-' if sign_ok => {
                negative = c == b'-';
                sign_ok = false;
            }
            b'0'..=b'9' => {
                sign_ok = false;
                any_digit = true;
                let digit = i64::from(c - b'0');
                // `strtoimax`: clamps to `INTMAX_MIN` / `INTMAX_MAX` on overflow.
                value = if negative {
                    value.saturating_mul(10).saturating_sub(digit)
                } else {
                    value.saturating_mul(10).saturating_add(digit)
                };
            }
            _ => break,
        }
        s.at += 1;
        width -= 1;
    }
    if !any_digit {
        return None;
    }
    Some(value)
}

/// `php_password_argon2_get_info` (`Z/ext/standard/password.c:260`): the costs from the
/// hash, each defaulting to php's where the scan stops before it.
pub(super) fn get_info(hash: &[u8]) -> Costs {
    let mut costs = Costs {
        version: 0,
        memory_cost: DEFAULT_MEMORY_COST,
        time_cost: DEFAULT_TIME_COST,
        threads: DEFAULT_THREADS,
    };
    extract_parameters(hash, &mut costs);
    costs
}

/// `php_password_argon2_needs_rehash` (`Z/ext/standard/password.c:275`): the wanted costs
/// (php's defaults where an option is absent) against the hash's, which default to 0.
pub(super) fn needs_rehash(
    hash: &[u8],
    new_memory_cost: i64,
    new_time_cost: i64,
    new_threads: i64,
) -> bool {
    let mut costs = Costs {
        version: 0,
        memory_cost: 0,
        time_cost: 0,
        threads: 0,
    };
    extract_parameters(hash, &mut costs);
    new_time_cost != costs.time_cost
        || new_memory_cost != costs.memory_cost
        || new_threads != costs.threads
}

/// One `sscanf("%ld")` over `bytes`, as [`scan_long`] reads it: what
/// `php_password_bcrypt_get_info` reads the cost with after `$2y$`.
pub(super) fn sscanf_long(bytes: &[u8]) -> Option<i64> {
    let mut s = CStr {
        bytes: c_string(bytes),
        at: 0,
    };
    scan_long(&mut s)
}
