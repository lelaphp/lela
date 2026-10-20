//! `php_hash_ops` and the table `PHP_MINIT_FUNCTION(hash)` fills
//! (`Z/ext/hash/hash.c:1590-1656`): each algorithm's name, digest and
//! block size, whether HMAC takes it, the magic it serialises under, and
//! the state it starts from. A state is php's own `PHP_*_CTX` struct laid
//! out as slots in its `serialize_spec`'s order, so serialising walks
//! them as `php_hash_serialize_spec` and `php_hash_unserialize_spec` walk
//! the struct's bytes (`hash.c:210-340`); the slots are the spec, and no
//! spec string is kept beside them.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

use crate::checksum::{Bzip2, Castagnoli, Ieee};
use crate::{checksum, gost, haval, md, murmur, sha, snefru, tiger, whirlpool, xxhash};

/// A stretch of a state's fields: one letter of its spec and its count.
pub(crate) enum Slot<'a> {
    /// `l` and `i`: 32-bit words, each one element.
    L(&'a mut [u32]),
    /// `q`: 64-bit words, each two elements, low half first.
    Q(&'a mut [u64]),
    /// `b`: bytes, one string element for two or more, else one integer.
    B(&'a mut [u8]),
    /// `I`: 32-bit fields the spec steps over, which take no element.
    Over(usize),
}

impl Slot<'_> {
    /// One field's size, which is its alignment too.
    fn size(&self) -> usize {
        match self {
            Slot::L(_) | Slot::Over(_) => 4,
            Slot::Q(_) => 8,
            Slot::B(_) => 1,
        }
    }
}

/// Why `php_hash_unserialize_spec`, or the check an algorithm adds after
/// it, refused the state's array; [`Refusal::code`] is the number php's
/// message carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `FAILURE`: no array, or a magic that is not the algorithm's.
    Malformed,
    /// `BYTE_OFFSET_POS_ERROR - pos`: the element for the field at this
    /// byte of the struct is missing, or not the type the field reads.
    At(usize),
    /// `CONTEXT_VALIDATION_FAILURE`: the fields fit, and the algorithm's
    /// own check refused them.
    Invalid,
}

impl Refusal {
    /// `hash_spec_result`'s integer.
    pub(crate) fn code(self) -> i64 {
        match self {
            Refusal::Malformed => -1,
            Refusal::At(pos) => -1000 - i64::try_from(pos).unwrap_or(i64::MAX - 1000),
            Refusal::Invalid => -2000,
        }
    }
}

/// A hash state: `hash_update`, `hash_final` and `hash_copy`, and
/// the fields its spec serialises.
pub(crate) trait State: Fork + Send {
    /// `hash_update`.
    fn update(&mut self, data: &[u8]);
    /// `hash_final`: the digest's bytes. The state is spent.
    fn finish(&mut self) -> Vec<u8>;
    /// The fields, in the order the algorithm's spec names them.
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
pub(crate) type Init = fn(&mut Request, Options) -> Result<Box<dyn State>, Thrown>;

/// The `options` array of `hash()`, `hash_file()` and `hash_init()`,
/// php's `HashTable *args`, `NULL` where the call gave none.
#[derive(Clone, Copy, Default)]
pub(crate) struct Options(pub(crate) Option<PhpArray>);

impl Options {
    /// `zend_hash_str_find_deref(args, name)`, which allocates nothing.
    ///
    /// ```php
    /// $a = hash_init('murmur3a', options: ['seed' => 1]);
    /// $m = memory_get_usage();
    /// $b = hash_init('murmur3a', options: ['seed' => 1]);
    /// $n = memory_get_usage();
    /// unset($b);
    /// var_dump(memory_get_usage() - $n + ($n - $m));
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    pub(crate) fn get(self, name: &[u8]) -> Option<Value> {
        self.0?.find(name).map(|value| value.deref())
    }

    /// The `seed` option as `PHP_MURMUR3AInit` and `PHP_XXH32Init` read
    /// it: an int, or the deprecation and zero.
    pub(crate) fn seed(self, req: &mut Request) -> u64 {
        let Some(value) = self.get(b"seed") else {
            return 0;
        };
        if let ValueRef::Int(n) = value.as_ref() {
            return n.cast_unsigned();
        }
        docref(
            req,
            Level::Deprecated,
            "Passing a seed of a type other than int is deprecated because it is the same as setting the seed to 0",
        );
        0
    }
}

/// One `php_hash_ops`, under the name it is registered with.
pub(crate) struct Algo {
    /// The key `hash_algos()` lists.
    pub(crate) name: &'static str,
    /// `ops->algo`, what `__serialize` and `__debugInfo` write.
    pub(crate) label: &'static str,
    pub(crate) digest: usize,
    pub(crate) block: usize,
    /// What `php_hash_alloc_context` `ecalloc`s for the state:
    /// `ops->context_size`, with `ops->context_align` beside it where
    /// php aligns the state (xxh3's 64).
    pub(crate) context: usize,
    pub(crate) crypto: bool,
    /// The magic `__serialize` writes the state under, which
    /// `__unserialize` asks for; `None` for an algorithm php cannot
    /// serialise.
    pub(crate) magic: Option<i64>,
    pub(crate) init: Init,
}

/// The state of an algorithm with no options.
#[allow(clippy::unnecessary_wraps)]
fn plain<T: State + Default + 'static>(
    _: &mut Request,
    _: Options,
) -> Result<Box<dyn State>, Thrown> {
    Ok(Box::new(T::default()))
}

/// `PHP_HASH_SERIALIZE_MAGIC_SPEC`.
const MAGIC: i64 = 2;

const fn algo(
    name: &'static str,
    digest: usize,
    block: usize,
    context: usize,
    crypto: bool,
    init: Init,
) -> Algo {
    Algo {
        name,
        label: name,
        digest,
        block,
        context,
        crypto,
        magic: Some(MAGIC),
        init,
    }
}

/// The algorithms in php's order, which `hash_algos()` lists.
#[rustfmt::skip]
pub(crate) static ALGOS: [Algo; 60] = [
    algo("md2", 16, 16, 81, true, plain::<md::Md2>),
    algo("md4", 16, 64, 88, true, plain::<md::Md4>),
    algo("md5", 16, 64, 152, true, plain::<md::Md5>),
    algo("sha1", 20, 64, 92, true, plain::<md::Sha1>),
    algo("sha224", 28, 64, 104, true, plain::<md::Sha224>),
    algo("sha256", 32, 64, 104, true, plain::<md::Sha256>),
    algo("sha384", 48, 128, 208, true, plain::<sha::Sha384>),
    algo("sha512/224", 28, 128, 208, true, plain::<sha::Sha512t224>),
    algo("sha512/256", 32, 128, 208, true, plain::<sha::Sha512t256>),
    algo("sha512", 64, 128, 208, true, plain::<sha::Sha512Full>),
    keccak("sha3-224", 28, 224, plain::<sha::Sha3<28>>),
    keccak("sha3-256", 32, 224, plain::<sha::Sha3<32>>),
    keccak("sha3-384", 48, 224, plain::<sha::Sha3<48>>),
    keccak("sha3-512", 64, 224, plain::<sha::Sha3<64>>),
    algo("ripemd128", 16, 64, 88, true, plain::<md::Ripemd<4>>),
    algo("ripemd160", 20, 64, 92, true, plain::<md::Ripemd<5>>),
    algo("ripemd256", 32, 64, 104, true, plain::<md::Ripemd<8>>),
    algo("ripemd320", 40, 64, 112, true, plain::<md::Ripemd<10>>),
    algo("whirlpool", 64, 64, 168, true, plain::<whirlpool::Whirlpool>),
    algo("tiger128,3", 16, 64, 104, true, plain::<tiger::Tiger<0, 16>>),
    algo("tiger160,3", 20, 64, 104, true, plain::<tiger::Tiger<0, 20>>),
    algo("tiger192,3", 24, 64, 104, true, plain::<tiger::Tiger<0, 24>>),
    algo("tiger128,4", 16, 64, 104, true, plain::<tiger::Tiger<1, 16>>),
    algo("tiger160,4", 20, 64, 104, true, plain::<tiger::Tiger<1, 20>>),
    algo("tiger192,4", 24, 64, 104, true, plain::<tiger::Tiger<1, 24>>),
    algo("snefru", 32, 32, 108, true, plain::<snefru::Snefru>),
    Algo { label: "snefru", ..algo("snefru256", 32, 32, 108, true, plain::<snefru::Snefru>) },
    algo("gost", 32, 32, 120, true, plain::<gost::Gost<false>>),
    algo("gost-crypto", 32, 32, 120, true, plain::<gost::Gost<true>>),
    algo("adler32", 4, 4, 4, false, plain::<checksum::Adler32>),
    algo("crc32", 4, 4, 4, false, plain::<checksum::Crc<Bzip2>>),
    algo("crc32b", 4, 4, 4, false, plain::<checksum::Crc<Ieee>>),
    algo("crc32c", 4, 4, 4, false, plain::<checksum::Crc<Castagnoli>>),
    algo("fnv132", 4, 4, 4, false, plain::<checksum::Fnv32<false>>),
    algo("fnv1a32", 4, 4, 4, false, plain::<checksum::Fnv32<true>>),
    algo("fnv164", 8, 4, 8, false, plain::<checksum::Fnv64<false>>),
    algo("fnv1a64", 8, 4, 8, false, plain::<checksum::Fnv64<true>>),
    algo("joaat", 4, 4, 4, false, plain::<checksum::Joaat>),
    algo("murmur3a", 4, 4, 12, false, murmur::Murmur3a::init),
    algo("murmur3c", 16, 4, 36, false, murmur::Murmur3c::init),
    algo("murmur3f", 16, 8, 40, false, murmur::Murmur3f::init),
    algo("xxh32", 4, 4, 48, false, xxhash::Xxh32::init),
    algo("xxh64", 8, 8, 88, false, xxhash::Xxh64::init),
    Algo { magic: None, ..algo("xxh3", 8, 8, 896, false, xxhash::Xxh3::init_64) },
    Algo { magic: None, ..algo("xxh128", 16, 8, 896, false, xxhash::Xxh3::init_128) },
    algo("haval128,3", 16, 128, 184, true, plain::<haval::Haval<3, 128>>),
    algo("haval160,3", 20, 128, 184, true, plain::<haval::Haval<3, 160>>),
    algo("haval192,3", 24, 128, 184, true, plain::<haval::Haval<3, 192>>),
    algo("haval224,3", 28, 128, 184, true, plain::<haval::Haval<3, 224>>),
    algo("haval256,3", 32, 128, 184, true, plain::<haval::Haval<3, 256>>),
    algo("haval128,4", 16, 128, 184, true, plain::<haval::Haval<4, 128>>),
    algo("haval160,4", 20, 128, 184, true, plain::<haval::Haval<4, 160>>),
    algo("haval192,4", 24, 128, 184, true, plain::<haval::Haval<4, 192>>),
    algo("haval224,4", 28, 128, 184, true, plain::<haval::Haval<4, 224>>),
    algo("haval256,4", 32, 128, 184, true, plain::<haval::Haval<4, 256>>),
    algo("haval128,5", 16, 128, 184, true, plain::<haval::Haval<5, 128>>),
    algo("haval160,5", 20, 128, 184, true, plain::<haval::Haval<5, 160>>),
    algo("haval192,5", 24, 128, 184, true, plain::<haval::Haval<5, 192>>),
    algo("haval224,5", 28, 128, 184, true, plain::<haval::Haval<5, 224>>),
    algo("haval256,5", 32, 128, 184, true, plain::<haval::Haval<5, 256>>),
];

/// `DECLARE_SHA3_OPS`: the block is the sponge's rate, and the state
/// is XKCP's `Keccak_HashInstance` under magic 100, the 64-bit
/// lane-complemented build (`hash_sha3.c:258-300`).
const fn keccak(name: &'static str, digest: usize, context: usize, init: Init) -> Algo {
    Algo {
        magic: Some(100),
        ..algo(name, digest, 200 - 2 * digest, context, true, init)
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

/// `php_hash_serialize_spec`: each integer as a 32-bit `zend_long`, a
/// 64-bit one as two, low half first, and two or more bytes in a row as
/// one string.
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
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for slot in state.slots() {
        match slot {
            Slot::B(bytes) if bytes.len() > 1 => {
                let string = PhpString::new(req, bytes)?;
                out.append(req, string.into_value())?;
            }
            Slot::B(bytes) => {
                for byte in bytes {
                    out.append(req, Owner::int(i64::from(*byte)))?;
                }
            }
            Slot::L(words) => {
                for word in words {
                    out.append(req, Owner::int(i64::from(*word as i32)))?;
                }
            }
            Slot::Q(words) => {
                for word in words {
                    out.append(req, Owner::int(i64::from(*word as u32 as i32)))?;
                    out.append(req, Owner::int(i64::from((*word >> 32) as u32 as i32)))?;
                }
            }
            Slot::Over(_) => {}
        }
    }
    Ok(out)
}

/// `php_hash_unserialize_spec` and the check an algorithm adds: the
/// fields written, or why not.
///
/// ```php
/// foreach (['a:1:{i:0;s:3:"xyz";}', 'a:2:{i:0;s:48:"' . str_repeat('x', 48) . '";i:1;s:1:"x";}', 's:1:"x";'] as $f) {
///     try { unserialize('O:11:"HashContext":5:{i:0;s:3:"md2";i:1;i:0;i:2;' . $f . 'i:3;i:2;i:4;a:0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// foreach (['s:1:"x"', 'i:136'] as $i) {
///     $f = 'a:2:{i:0;s:200:"' . str_repeat("\0", 200) . '";i:1;' . $i . ';}';
///     try { unserialize('O:11:"HashContext":5:{i:0;s:8:"sha3-256";i:1;i:0;i:2;' . $f . 'i:3;i:100;i:4;a:0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// $c = hash_init('tiger192,3'); hash_update($c, 'abc'); $d = unserialize(serialize($c)); hash_update($d, 'def');
/// var_dump(hash_final($d) === hash('tiger192,3', 'abcdef'));
/// ```
/// ```expect
/// Incomplete or ill-formed serialization data ("md2" code -1000)
/// Incomplete or ill-formed serialization data ("md2" code -1048)
/// Incomplete or ill-formed serialization data ("md2" code -1)
/// Incomplete or ill-formed serialization data ("sha3-256" code -1204)
/// Incomplete or ill-formed serialization data ("sha3-256" code -2000)
/// bool(true)
/// ```
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn unserialize(state: &mut dyn State, data: &Value) -> Result<(), Refusal> {
    let data = data.deref().as_array().ok_or(Refusal::Malformed)?;
    let mut next = 0;
    let mut element = || {
        let value = data.get(&Key::Int(next));
        next += 1;
        value
    };
    let mut pos = 0usize;
    for slot in state.slots() {
        pos = pos.next_multiple_of(slot.size());
        match slot {
            Slot::B(bytes) if bytes.len() > 1 => {
                let value = element();
                let given = value.as_ref().and_then(Value::as_str_bytes);
                match given.filter(|given| given.len() == bytes.len()) {
                    Some(given) => bytes.copy_from_slice(given),
                    None => return Err(Refusal::At(pos)),
                }
                pos += bytes.len();
            }
            Slot::B(bytes) => {
                for byte in bytes {
                    *byte = int(&mut element, pos)? as u8;
                    pos += 1;
                }
            }
            Slot::L(words) => {
                for word in words {
                    *word = int(&mut element, pos)? as u32;
                    pos += 4;
                }
            }
            Slot::Q(words) => {
                for word in words {
                    let low = u64::from(int(&mut element, pos)? as u32);
                    *word = low.wrapping_add(int(&mut element, pos)?.cast_unsigned() << 32);
                    pos += 8;
                }
            }
            Slot::Over(count) => pos += 4 * count,
        }
    }
    if state.valid() {
        Ok(())
    } else {
        Err(Refusal::Invalid)
    }
}

/// The next element, which `Z_TYPE_P(elt) == IS_LONG` holds to an
/// integer; the field's byte where it is not.
fn int(element: &mut impl FnMut() -> Option<Value>, pos: usize) -> Result<i64, Refusal> {
    match element().as_ref().map(Value::as_ref) {
        Some(ValueRef::Int(n)) => Ok(n),
        _ => Err(Refusal::At(pos)),
    }
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
