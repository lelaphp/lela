//! `php_hash_ops` and the table `PHP_MINIT_FUNCTION(hash)` fills
//! (`Z/ext/hash/hash.c:1590-1656`): each algorithm's name, digest and
//! block size, whether HMAC takes it, its serialisation spec, and the
//! state it starts from. A state is php's own `PHP_*_CTX` struct laid out as
//! fields, so `php_hash_serialize_spec` and `php_hash_unserialize_spec`
//! walk it as they walk the struct's bytes (`hash.c:210-340`).

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::{gost, md, sha, snefru, tiger, whirlpool};

/// A stretch of a state's fields, in the order its spec names them.
pub(crate) enum Slot<'a> {
    /// `l` and `i`: 32-bit words.
    L(&'a mut [u32]),
    /// `q`: 64-bit words.
    Q(&'a mut [u64]),
    /// `b`: bytes.
    B(&'a mut [u8]),
}

/// One field of a slot.
enum Field<'a> {
    L(&'a mut u32),
    Q(&'a mut u64),
    B(&'a mut u8),
}

/// A hash state: `hash_update`, `hash_final` and `hash_copy`, and
/// the fields its spec serialises.
pub(crate) trait State: Fork + Send {
    /// `hash_update`.
    fn update(&mut self, data: &[u8]);
    /// `hash_final`: the digest's bytes. The state is spent.
    fn finish(&mut self) -> Vec<u8>;
    /// The fields the spec walks, in its order.
    fn slots(&mut self) -> Vec<Slot<'_>> {
        Vec::new()
    }
    /// What an algorithm's own `hash_unserialize` checks after the spec
    /// wrote the fields, `CONTEXT_VALIDATION_FAILURE` when it fails.
    fn valid(&self) -> bool {
        true
    }
}

/// `hash_copy`, `php_hash_copy`'s `memcpy`.
pub(crate) trait Fork {
    fn fork(&self) -> Box<dyn State>;
}

impl<T: State + Clone + 'static> Fork for T {
    fn fork(&self) -> Box<dyn State> {
        Box::new(self.clone())
    }
}

/// `hash_init` with the `options` array, which the seeded algorithms
/// read and every other ignores.
pub(crate) type Init = fn(&mut Request, Option<PhpArray>) -> Result<Box<dyn State>, Thrown>;

/// One `php_hash_ops`, under the name it is registered with.
pub(crate) struct Algo {
    /// The key `hash_algos()` lists.
    pub(crate) name: &'static str,
    /// `ops->algo`, what `__serialize` and `__debugInfo` write.
    pub(crate) label: &'static str,
    pub(crate) digest: usize,
    pub(crate) block: usize,
    pub(crate) crypto: bool,
    /// `serialize_spec` and the magic it is written under; `None` for
    /// an algorithm php cannot serialise.
    pub(crate) spec: Option<(&'static str, i64)>,
    pub(crate) init: Init,
}

/// The state of an algorithm with no options.
#[allow(clippy::unnecessary_wraps)]
fn plain<T: State + Default + 'static>(
    _: &mut Request,
    _: Option<PhpArray>,
) -> Result<Box<dyn State>, Thrown> {
    Ok(Box::new(T::default()))
}

/// `PHP_HASH_SERIALIZE_MAGIC_SPEC`.
const MAGIC: i64 = 2;

const fn algo(
    name: &'static str,
    digest: usize,
    block: usize,
    crypto: bool,
    spec: Option<&'static str>,
    init: Init,
) -> Algo {
    Algo {
        name,
        label: name,
        digest,
        block,
        crypto,
        spec: match spec {
            Some(spec) => Some((spec, MAGIC)),
            None => None,
        },
        init,
    }
}

const SHA512: Option<&str> = Some("q8q2b128.");
const SNEFRU: Option<&str> = Some("l16l2bb32");
const TIGER: Option<&str> = Some("q3qb64l");

/// The algorithms in php's order, which `hash_algos()` lists.
#[rustfmt::skip]
pub(crate) static ALGOS: [Algo; 29] = [
    algo("md2", 16, 16, true, Some("b48b16b16b."), plain::<md::Md2>),
    algo("md4", 16, 64, true, Some("l4l2b64."), plain::<md::Md4>),
    algo("md5", 16, 64, true, Some("llllllb64l16."), plain::<md::Md5>),
    algo("sha1", 20, 64, true, Some("l5l2b64."), plain::<md::Sha1>),
    algo("sha224", 28, 64, true, Some("l8l2b64."), plain::<md::Sha224>),
    algo("sha256", 32, 64, true, Some("l8l2b64."), plain::<md::Sha256>),
    algo("sha384", 48, 128, true, SHA512, plain::<sha::Sha384>),
    algo("sha512/224", 28, 128, true, SHA512, plain::<sha::Sha512t224>),
    algo("sha512/256", 32, 128, true, SHA512, plain::<sha::Sha512t256>),
    algo("sha512", 64, 128, true, SHA512, plain::<sha::Sha512Full>),
    keccak("sha3-224", 28, plain::<sha::Sha3<28>>),
    keccak("sha3-256", 32, plain::<sha::Sha3<32>>),
    keccak("sha3-384", 48, plain::<sha::Sha3<48>>),
    keccak("sha3-512", 64, plain::<sha::Sha3<64>>),
    algo("ripemd128", 16, 64, true, Some("l4l2b64."), plain::<md::Ripemd<4>>),
    algo("ripemd160", 20, 64, true, Some("l5l2b64."), plain::<md::Ripemd<5>>),
    algo("ripemd256", 32, 64, true, Some("l8l2b64."), plain::<md::Ripemd<8>>),
    algo("ripemd320", 40, 64, true, Some("l10l2b64."), plain::<md::Ripemd<10>>),
    algo("whirlpool", 64, 64, true, Some("q8b32iib64."), plain::<whirlpool::Whirlpool>),
    algo("tiger128,3", 16, 64, true, TIGER, plain::<tiger::Tiger<0, 16>>),
    algo("tiger160,3", 20, 64, true, TIGER, plain::<tiger::Tiger<0, 20>>),
    algo("tiger192,3", 24, 64, true, TIGER, plain::<tiger::Tiger<0, 24>>),
    algo("tiger128,4", 16, 64, true, TIGER, plain::<tiger::Tiger<1, 16>>),
    algo("tiger160,4", 20, 64, true, TIGER, plain::<tiger::Tiger<1, 20>>),
    algo("tiger192,4", 24, 64, true, TIGER, plain::<tiger::Tiger<1, 24>>),
    algo("snefru", 32, 32, true, SNEFRU, plain::<snefru::Snefru>),
    Algo { label: "snefru", ..algo("snefru256", 32, 32, true, SNEFRU, plain::<snefru::Snefru>) },
    algo("gost", 32, 32, true, SNEFRU, plain::<gost::Gost<false>>),
    algo("gost-crypto", 32, 32, true, SNEFRU, plain::<gost::Gost<true>>),
];

/// `DECLARE_SHA3_OPS`: the block is the sponge's rate, and the state
/// is XKCP's `Keccak_HashInstance` under `PHP_KECCAK_SPEC` and magic
/// 100, the 64-bit lane-complemented build (`hash_sha3.c:258-300`).
const fn keccak(name: &'static str, digest: usize, init: Init) -> Algo {
    Algo {
        spec: Some(("b200IiIIB", 100)),
        ..algo(name, digest, 200 - 2 * digest, true, None, init)
    }
}

/// `php_hash_fetch_ops`: the name lowered, then looked up.
///
/// ```php
/// var_dump(hash('SHA256', ''), hash('Md5', 'x') === md5('x'), in_array('sha3-512', hash_algos()));
/// ```
/// ```expect
/// string(64) "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn fetch(name: &[u8]) -> Option<&'static Algo> {
    let lowered = name.to_ascii_lowercase();
    ALGOS.iter().find(|algo| algo.name.as_bytes() == lowered)
}

/// `parse_serialize_spec` over the whole spec: each item's letter,
/// count and aligned byte offset, up to the `.` that ends it.
fn items(spec: &str) -> Vec<(u8, usize, usize)> {
    let bytes = spec.as_bytes();
    let (mut at, mut pos, mut out) = (0, 0usize, Vec::new());
    while at < bytes.len() && bytes[at] != b'.' {
        let letter = bytes[at];
        let size = size(letter);
        pos = pos.next_multiple_of(size);
        at += 1;
        let digits = bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        let count = std::str::from_utf8(&bytes[at..at + digits])
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(1);
        at += digits;
        out.push((letter, count, pos));
        pos += count * size;
    }
    out
}

/// A spec letter's size, which is its alignment too.
fn size(letter: u8) -> usize {
    match letter.to_ascii_lowercase() {
        b's' => 2,
        b'l' | b'i' => 4,
        b'q' => 8,
        _ => 1,
    }
}

/// The fields one at a time.
fn fields(slots: Vec<Slot<'_>>) -> Vec<Field<'_>> {
    let mut out = Vec::new();
    for slot in slots {
        match slot {
            Slot::L(words) => out.extend(words.iter_mut().map(Field::L)),
            Slot::Q(words) => out.extend(words.iter_mut().map(Field::Q)),
            Slot::B(bytes) => out.extend(bytes.iter_mut().map(Field::B)),
        }
    }
    out
}

impl Field<'_> {
    fn get(&self) -> u64 {
        match self {
            Field::L(w) => u64::from(**w),
            Field::Q(w) => **w,
            Field::B(b) => u64::from(**b),
        }
    }

    /// `one_to_buffer`: the value truncated to the field.
    #[allow(clippy::cast_possible_truncation)]
    fn set(&mut self, value: u64) {
        match self {
            Field::L(w) => **w = value as u32,
            Field::Q(w) => **w = value,
            Field::B(b) => **b = value as u8,
        }
    }
}

/// `php_hash_serialize_spec`: each integer as a 32-bit `zend_long`, a
/// 64-bit one as two, low half first, and two or more bytes in a row as
/// one string; `None` where the fields and the spec disagree.
///
/// ```php
/// $c = hash_init('sha3-256'); hash_update($c, 'abc');
/// $s = serialize($c); echo strlen($s), ' ', substr($s, 0, 60), "\n";
/// $c = hash_init('whirlpool'); hash_update($c, str_repeat('w', 70));
/// echo md5(serialize($c)), "\n";
/// $c = hash_init('sha512'); hash_update($c, 'x'); echo substr(serialize($c), 0, 120), "\n";
/// ```
/// ```expect
/// 301 O:11:"HashContext":5:{i:0;s:8:"sha3-256";i:1;i:0;i:2;a:2:{i:
/// 50d0fa81919299f48ca0b3c8c6af90ab
/// O:11:"HashContext":5:{i:0;s:6:"sha512";i:1;i:0;i:2;a:21:{i:0;i:-205731576;i:1;i:1779033703;i:2;i:-2067093701;i:3;i:-1150
/// ```
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn serialize(
    req: &mut Request,
    state: &mut dyn State,
    spec: &str,
) -> Result<Option<PhpArray>, Thrown> {
    let mut fields = fields(state.slots()).into_iter();
    let mut out = PhpArray::new(req)?;
    for (letter, count, _) in items(spec) {
        if letter.is_ascii_uppercase() {
            continue;
        }
        if letter == b'b' && count > 1 {
            let bytes: Vec<u8> = fields.by_ref().take(count).map(|c| c.get() as u8).collect();
            let string = PhpString::new(req, &bytes)?;
            out.append(req, Value::string(string))?;
            continue;
        }
        for _ in 0..count {
            let Some(cell) = fields.next() else {
                return Ok(None);
            };
            let value = cell.get();
            out.append(req, Value::int(i64::from(value as u32 as i32)))?;
            if size(letter) == 8 {
                out.append(req, Value::int(i64::from((value >> 32) as u32 as i32)))?;
            }
        }
    }
    Ok(Some(out))
}

/// `php_hash_unserialize_spec` and the check an algorithm adds: 0, or
/// php's `hash_spec_result`, `BYTE_OFFSET_POS_ERROR - pos` naming the
/// byte the first element that does not fit would have gone to.
///
/// ```php
/// foreach (['a:1:{i:0;s:3:"xyz";}', 'a:2:{i:0;s:48:"' . str_repeat('x', 48) . '";i:1;s:1:"x";}', 's:1:"x";'] as $f) {
///     try { unserialize('O:11:"HashContext":5:{i:0;s:3:"md2";i:1;i:0;i:2;' . $f . 'i:3;i:2;i:4;a:0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// $c = hash_init('tiger192,3'); hash_update($c, 'abc'); $d = unserialize(serialize($c)); hash_update($d, 'def');
/// var_dump(hash_final($d) === hash('tiger192,3', 'abcdef'));
/// ```
/// ```expect
/// Incomplete or ill-formed serialization data ("md2" code -1000)
/// Incomplete or ill-formed serialization data ("md2" code -1048)
/// Incomplete or ill-formed serialization data ("md2" code -1)
/// bool(true)
/// ```
pub(crate) fn unserialize(state: &mut dyn State, spec: &str, data: &Value) -> i64 {
    let Some(data) = data.deref().as_array().copied() else {
        return -1;
    };
    {
        let mut fields = fields(state.slots()).into_iter();
        let mut next = 0;
        let element = |next: &mut i64| {
            let value = data.get(&Key::Int(*next));
            *next += 1;
            value
        };
        for (letter, count, pos) in items(spec) {
            if letter.is_ascii_uppercase() {
                continue;
            }
            if letter == b'b' && count > 1 {
                let value = element(&mut next);
                let Some(bytes) = value
                    .as_ref()
                    .and_then(Value::as_str_bytes)
                    .filter(|b| b.len() == count)
                else {
                    return -1000 - offset(pos);
                };
                for (byte, mut cell) in bytes.iter().zip(fields.by_ref()) {
                    cell.set(u64::from(*byte));
                }
                continue;
            }
            for k in 0..count {
                let at = offset(pos + k * size(letter));
                let Some(low) = element(&mut next).as_ref().and_then(long) else {
                    return -1000 - at;
                };
                let mut value = u64::from(low as u32);
                if size(letter) == 8 {
                    let Some(high) = element(&mut next).as_ref().and_then(long) else {
                        return -1000 - at;
                    };
                    value = value.wrapping_add(high.cast_unsigned() << 32);
                }
                match fields.next() {
                    Some(mut cell) => cell.set(value),
                    None => return -999,
                }
            }
        }
    }
    if state.valid() { 0 } else { -2000 }
}

/// `Z_TYPE_P(elt) == IS_LONG`.
fn long(value: &Value) -> Option<i64> {
    match value.as_ref() {
        ValueRef::Int(n) => Some(n),
        _ => None,
    }
}

fn offset(pos: usize) -> i64 {
    i64::try_from(pos).unwrap_or(i64::MAX)
}

/// php's little-endian `Decode`: sixteen or thirty-two words of a block.
///
/// ```php
/// var_dump(hash('md4', 'message digest'), hash('gost', str_repeat('U', 33)));
/// ```
/// ```expect
/// string(32) "d9130a8164549fe818874806e1c7014b"
/// string(64) "1eeaafaf9ef16e101875da730eba2df5a4c80ddd998e0f55aa42bdb9662054af"
/// ```
pub(crate) fn le_words<const N: usize>(block: &[u8]) -> [u32; N] {
    let mut words = [0u32; N];
    for (word, bytes) in words.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    words
}
