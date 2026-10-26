//! What libzip's algorithms and encryption sources do to an entry's data
//! on the way out (`zip_algorithm_*.c`, `zip_source_compress.c`,
//! `zip_source_pkware_encode.c`, `zip_source_winzip_aes_encode.c`), over
//! the platform's zlib and pure-Rust codecs and ciphers, each fed and
//! read a buffer at a time as libzip's layers are, and the attributes
//! each layer gives; and the ciphers' state on the way in, which
//! [`crate::read`] decrypts with.

use std::io::Write;

use crate::error::{Er, Status};
use crate::ffi::Deflate;

/// `TORRENTZIP_COMPRESSION_FLAGS`, the level a torrentzip entry is
/// written at ([`crate::write`]'s `zip_dirent_torrentzip_normalize`):
/// `Z_BEST_COMPRESSION` at `TORRENTZIP_MEM_LEVEL`.
pub(crate) const TORRENTZIP_COMPRESSION_FLAGS: u32 = 0xffff;

/// `BUFSIZE`: what `zip_source_compress` reads its source in, and what
/// `copy_source` reads the layers over it in.
pub(crate) const BUFSIZE: usize = 8192;

/// `ZIP_FILE_ATTRIBUTES_GENERAL_PURPOSE_BIT_FLAGS_ALLOWED_MASK`.
pub(crate) const BITS_ALLOWED: u16 = 0x083e;

/// `ZIP_GPBF_DATA_DESCRIPTOR`.
const DATA_DESCRIPTOR: u16 = 0x0008;

/// A method ext/zip compresses with, decoded once from the entry's
/// `ZIP_CM_ACTUAL` method (`zip_source_compress.c`'s `implementations`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Deflate,
    Bzip2,
    Lzma,
    Xz,
    Zstd,
}

impl Method {
    /// The method `code` names, `None` for one libzip has no algorithm
    /// for.
    pub(crate) fn of(code: u16) -> Option<Method> {
        Some(match code {
            8 => Method::Deflate,
            12 => Method::Bzip2,
            14 => Method::Lzma,
            95 => Method::Xz,
            93 => Method::Zstd,
            _ => return None,
        })
    }

    /// Its `ZIP_CM_*` number.
    pub(crate) fn code(self) -> u16 {
        match self {
            Method::Deflate => 8,
            Method::Bzip2 => 12,
            Method::Lzma => 14,
            Method::Xz => 95,
            Method::Zstd => 93,
        }
    }

    /// The version its algorithm's layer reports needed, compressing or
    /// decompressing alike (`zip_compression_algorithm_t`'s
    /// `version_needed`): bzip2's 4.6, and 2.0 for the rest, whose own
    /// versions `_zip_dirent_apply_attributes` raises by the method.
    fn version_needed(self) -> u16 {
        if self == Method::Bzip2 { 46 } else { 20 }
    }

    /// The general purpose bits its algorithm reports compressing at
    /// `level`: deflate's level, and LZMA's end marker.
    fn bits(self, level: u32) -> u16 {
        match self {
            Method::Deflate => match nine(level) {
                ..3 => 4,
                8.. => 2,
                _ => 0,
            },
            Method::Lzma => 2,
            Method::Bzip2 | Method::Xz | Method::Zstd => 0,
        }
    }
}

/// A level deflate and bzip2 read as given, `0` or past `9` as `9`.
fn nine(level: u32) -> u32 {
    if (1..=9).contains(&level) { level } else { 9 }
}

/// `zip_file_attributes_t` as one layer of a source gives it, or as the
/// layers of a source give it together: the host and external
/// attributes, the version needed, and general purpose bits under their
/// mask. The ASCII bit is not kept: the one source that gives it, an
/// unchanged entry's, gives the entry's own back.
#[derive(Clone, Copy, Default)]
pub(crate) struct Attributes {
    pub(crate) host: Option<(u8, u32)>,
    pub(crate) version_needed: Option<u16>,
    pub(crate) bits: Option<(u16, u16)>,
}

impl Attributes {
    /// `zip_source_get_file_attributes` of a layer that gives `self` over
    /// a source that gives `lower`: what the layer leaves unsaid from
    /// below, the larger version, and the bits below outside the layer's
    /// mask.
    pub(crate) fn over(self, lower: Attributes) -> Attributes {
        Attributes {
            host: self.host.or(lower.host),
            version_needed: match (self.version_needed, lower.version_needed) {
                (Some(v), Some(w)) => Some(v.max(w)),
                (v, w) => v.or(w),
            },
            bits: match (self.bits, lower.bits) {
                (Some((flags, mask)), Some((low_flags, low_mask))) => {
                    let low_mask = low_mask & !mask;
                    Some((flags | (low_flags & low_mask), mask | low_mask))
                }
                (b, c) => b.or(c),
            },
        }
    }

    /// A compression layer's, compressing or decompressing with
    /// `method`: the algorithm's version and its `bits` under the whole
    /// allowed mask (`compress_callback`'s).
    pub(crate) fn codec(method: Method, bits: u16) -> Attributes {
        Attributes {
            host: None,
            version_needed: Some(method.version_needed()),
            bits: Some((bits, BITS_ALLOWED)),
        }
    }
}

/// What one turn of an algorithm's `process` came to, short of an error
/// (`zip_compression_status_t`).
pub(crate) enum Turn {
    /// More to do on the input it has.
    More,
    /// The next input is wanted.
    NeedData,
    /// The stream has ended.
    End,
}

/// The compressed bytes an encoder has made and `process` has not yet
/// handed on.
#[derive(Default)]
struct Made {
    bytes: Vec<u8>,
    at: usize,
}

impl Made {
    /// As much as `out` takes, and how much that was.
    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = copy_from(&self.bytes, self.at, out);
        self.at += n;
        n
    }

    fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// `more` after what is left.
    fn extend(&mut self, more: &[u8]) {
        if self.is_empty() {
            self.bytes.clear();
            self.at = 0;
        }
        self.bytes.extend_from_slice(more);
    }
}

/// As much of `data` from `at` as `out` takes, and how much that was.
pub(crate) fn copy_from(data: &[u8], at: usize, out: &mut [u8]) -> usize {
    let rest = data.get(at..).unwrap_or_default();
    let n = rest.len().min(out.len());
    out[..n].copy_from_slice(&rest[..n]);
    n
}

/// A pure-Rust encoder written to as its input comes: what it has made
/// of its input so far, and the rest once its input ends.
trait Encoder {
    fn feed(&mut self, data: &[u8]) -> Result<Vec<u8>, Status>;
    fn finish(self: Box<Self>) -> Result<Vec<u8>, Status>;
}

/// lzma-rust2's writers, each over a `Vec` its made bytes are taken from.
macro_rules! lzma_encoder {
    ($($writer:ty),*) => {$(
        impl Encoder for $writer {
            fn feed(&mut self, data: &[u8]) -> Result<Vec<u8>, Status> {
                self.write_all(data)
                    .map_err(|_| Status::new(Er::Internal))?;
                Ok(std::mem::take(self.inner_mut()))
            }

            fn finish(self: Box<Self>) -> Result<Vec<u8>, Status> {
                (*self).finish().map_err(|_| Status::new(Er::Internal))
            }
        }
    )*};
}

lzma_encoder!(
    lzma_rust2::XzWriter<Vec<u8>>,
    lzma_rust2::LzmaWriter<Vec<u8>>
);

/// zstd, whose encoder here is ruzstd's one-shot frame: the input
/// gathered, compressed as it ends.
struct Zstd(Vec<u8>);

impl Encoder for Zstd {
    fn feed(&mut self, data: &[u8]) -> Result<Vec<u8>, Status> {
        self.0.extend_from_slice(data);
        Ok(Vec::new())
    }

    fn finish(self: Box<Self>) -> Result<Vec<u8>, Status> {
        Ok(ruzstd::encoding::compress_to_vec(
            self.0.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        ))
    }
}

/// `zip_compression_algorithm_t` compressing: deflate through the
/// platform's zlib, bzip2 through libbz2-rs, as libzip drives them;
/// LZMA, XZ and zstd through an [`Encoder`], which hands on what it has
/// made before it asks for more.
enum Algorithm {
    Deflate(Deflate),
    Bzip2 {
        z: bzip2::Compress,
        input: Vec<u8>,
        at: usize,
    },
    Streamed {
        encoder: Option<Box<dyn Encoder>>,
        made: Made,
    },
}

impl Algorithm {
    /// `allocate` and `start` for `method` at `level`.
    fn start(method: Method, level: u32) -> Result<Algorithm, Status> {
        let fail = |_| Status::new(Er::Internal);
        let preset = if level <= 9 { level } else { 6 };
        let encoder: Box<dyn Encoder> = match method {
            Method::Deflate => {
                // `zip_algorithm_deflate.c`'s `allocate`: torrentzip's flags
                // ask for `TORRENTZIP_MEM_LEVEL`, every other level
                // `MAX_MEM_LEVEL`.
                let mem_level = if level == TORRENTZIP_COMPRESSION_FLAGS {
                    8
                } else {
                    9
                };
                let z = Deflate::new(nine(level).cast_signed(), mem_level).map_err(Status::zlib)?;
                return Ok(Algorithm::Deflate(z));
            }
            Method::Bzip2 => {
                return Ok(Algorithm::Bzip2 {
                    z: bzip2::Compress::new(bzip2::Compression::new(nine(level)), 30),
                    input: Vec::new(),
                    at: 0,
                });
            }
            Method::Xz => {
                let mut options = lzma_rust2::XzOptions::with_preset(preset);
                options.set_check_sum_type(lzma_rust2::CheckType::Crc64);
                Box::new(lzma_rust2::XzWriter::new(Vec::new(), options).map_err(fail)?)
            }
            Method::Lzma => {
                let options = lzma_rust2::LzmaOptions::with_preset(preset);
                let mut out = vec![0x09, 0x14, 0x05, 0x00, options.get_props()];
                out.extend_from_slice(&options.dict_size.to_le_bytes());
                Box::new(
                    lzma_rust2::LzmaWriter::new(out, &options, false, true, None).map_err(fail)?,
                )
            }
            Method::Zstd => Box::new(Zstd(Vec::new())),
        };
        Ok(Algorithm::Streamed {
            encoder: Some(encoder),
            made: Made::default(),
        })
    }

    /// `input`: `data`, the next buffer of the source.
    fn input(&mut self, data: &[u8]) -> Result<(), Status> {
        match self {
            Algorithm::Deflate(z) => z.input(data),
            Algorithm::Bzip2 { input, at, .. } => {
                *input = data.to_vec();
                *at = 0;
            }
            Algorithm::Streamed { encoder, made } => {
                if let Some(e) = encoder {
                    made.extend(&e.feed(data)?);
                }
            }
        }
        Ok(())
    }

    /// `process`: what one turn writes into `out`, `finish` once the
    /// input has ended.
    fn process(&mut self, out: &mut [u8], finish: bool) -> Result<(Turn, usize), Status> {
        match self {
            Algorithm::Deflate(z) => z.process(out, finish).map_err(Status::zlib),
            Algorithm::Bzip2 { z, input, at } => {
                if *at >= input.len() && !finish {
                    return Ok((Turn::NeedData, 0));
                }
                let (before_in, before_out) = (z.total_in(), z.total_out());
                let action = if finish {
                    bzip2::Action::Finish
                } else {
                    bzip2::Action::Run
                };
                let status = z
                    .compress(input.get(*at..).unwrap_or_default(), out, action)
                    .map_err(|_| Status::new(Er::Internal))?;
                *at += usize::try_from(z.total_in() - before_in).unwrap_or(usize::MAX);
                let n = usize::try_from(z.total_out() - before_out).unwrap_or(usize::MAX);
                let turn = match status {
                    bzip2::Status::StreamEnd => Turn::End,
                    bzip2::Status::RunOk if *at >= input.len() => Turn::NeedData,
                    _ => Turn::More,
                };
                Ok((turn, n))
            }
            Algorithm::Streamed { encoder, made } => {
                if made.is_empty()
                    && finish
                    && let Some(e) = encoder.take()
                {
                    made.extend(&e.finish()?);
                }
                if !made.is_empty() {
                    return Ok((Turn::More, made.take(out)));
                }
                let turn = if encoder.is_some() {
                    Turn::NeedData
                } else {
                    Turn::End
                };
                Ok((turn, 0))
            }
        }
    }
}

/// `zip_source_compress` compressing with `method` at `level`, read as
/// `compress_read` reads it: the algorithm fed a buffer of the source
/// each time it asks for more, and as much handed out as each read asks.
/// `can_store` is `ZIP_CM_IS_DEFAULT`, under which data that ends within
/// one buffer of input and one read of what it makes, and does not
/// shrink, is stored. Deflate is the platform zlib's ([`crate::ffi`]) at libzip's
/// memLevel, 9 or torrentzip's 8, so an entry carries php's bytes at
/// every level, and a stream cut short inflates as far as php's does
/// (#1651).
///
/// ```php
/// $d = ''; for ($i = 0; $i < 3000; $i++) { $d .= hash('sha256', "row $i") . "\n"; }
/// $f = sys_get_temp_dir() . '/zip-deflate-' . getmypid() . '.zip';
/// $z = new ZipArchive; $z->open($f, ZipArchive::CREATE|ZipArchive::OVERWRITE); $z->addFromString('a', $d); $z->setCompressionName('a', ZipArchive::CM_DEFLATE); $z->close();
/// $b = file_get_contents($f);
/// $ctx = inflate_init(ZLIB_ENCODING_RAW); echo strlen(inflate_add($ctx, substr($b, 31, 4065), ZLIB_SYNC_FLUSH)), "\n";
/// $z->open($f); $s = $z->getStream('a'); $h = fopen($f, 'r+'); ftruncate($h, 3000); fclose($h);
/// $t = 0; while (!feof($s)) { $r = fread($s, 8192); if ($r === false) break; $t += strlen($r); } echo $t, "\n"; unlink($f);
/// ```
/// ```expect
/// 6548
/// 6548
/// ```
///
/// ```php
/// $d = ''; for ($i = 0; $i < 3000; $i++) { $d .= hash('sha256', "row $i") . "\n"; }
/// $f = sys_get_temp_dir() . '/zip-levels-' . getmypid() . '.zip';
/// $entry = function ($f) {
///     $b = file_get_contents($f);
///     $u = unpack('vmethod/x8/Vsize/x4/vn/vm', $b, 8);
///     return $u['method'] . ' ' . $u['size'] . ' ' . md5(substr($b, 30 + $u['n'] + $u['m'], $u['size']));
/// };
/// foreach ([1, 9, 0] as $level) {
///     $z = new ZipArchive; $z->open($f, ZipArchive::CREATE | ZipArchive::OVERWRITE);
///     $z->addFromString('a', $d); $z->setCompressionName('a', ZipArchive::CM_DEFLATE, $level); $z->close();
///     echo $level, ': ', $entry($f), "\n";
/// }
/// $z = new ZipArchive; $z->open($f, ZipArchive::CREATE | ZipArchive::OVERWRITE);
/// $z->addFromString('a', $d); $z->setArchiveFlag(ZipArchive::AFL_WANT_TORRENTZIP, 1); $z->close();
/// echo 'torrentzip: ', $entry($f), ' ', md5_file($f), "\n";
/// unlink($f);
/// ```
/// ```expect
/// 1: 8 115812 e4ba8163dd2f2d2d09772c6a1c77ab54
/// 9: 8 113050 5e1c764722165253289fc65c3deec994
/// 0: 8 113050 5e1c764722165253289fc65c3deec994
/// torrentzip: 8 112965 09f84361e4aad9b4169caff78195b5b0 fcd9d6e6501fb44f54b639fc1278d21d
/// ```
pub(crate) struct Compressor {
    algorithm: Algorithm,
    method: Method,
    bits: u16,
    /// `ctx->buffer`: the last buffer of input read.
    buffer: Box<[u8; BUFSIZE]>,
    /// How much the first read of input gave, once one was made.
    first_read: Option<usize>,
    /// Whether the data may yet be stored: one buffer of input read so
    /// far, and nothing handed out.
    can_store: bool,
    phase: Phase,
}

/// Where a [`Compressor`]'s stream stands (`end_of_input`,
/// `end_of_stream` and `is_stored`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The source still gives input.
    Input,
    /// The source has ended, and the algorithm finishes.
    Finishing,
    /// The stream has ended, as the algorithm made it or stored.
    Ended { stored: bool },
}

impl Compressor {
    pub(crate) fn new(method: Method, level: u32, can_store: bool) -> Result<Compressor, Status> {
        Ok(Compressor {
            algorithm: Algorithm::start(method, level)?,
            method,
            bits: method.bits(level),
            buffer: Box::new([0; BUFSIZE]),
            first_read: None,
            can_store,
            phase: Phase::Input,
        })
    }

    /// The method the data is in, once it has ended: store, where it was
    /// stored.
    pub(crate) fn method(&self) -> u16 {
        if self.stored() { 0 } else { self.method.code() }
    }

    /// `ZIP_SOURCE_GET_FILE_ATTRIBUTES`: its algorithm's version, and its
    /// general purpose bits, none once stored.
    pub(crate) fn attributes(&self) -> Attributes {
        Attributes::codec(self.method, if self.stored() { 0 } else { self.bits })
    }

    fn stored(&self) -> bool {
        self.phase == Phase::Ended { stored: true }
    }

    /// `compress_read`: up to `out.len()` compressed bytes, fewer only at
    /// the end, the source read through `lower` a buffer at a time. So a
    /// close's progress at each full buffer it makes is how much of the
    /// entry the algorithm has been fed: deflate's points fall where zlib
    /// emits a block, and bzip2, which emits nothing before its 900k
    /// block ends, is at its end by its first:
    ///
    /// ```php
    /// mt_srand(3); $rand = ''; for ($i = 0; $i < 100000; $i++) $rand .= chr(mt_rand(0, 255));
    /// $text = ''; for ($i = 0; $i < 20000; $i++) $text .= md5((string) $i);
    /// foreach ([ZipArchive::CM_DEFLATE, ZipArchive::CM_BZIP2] as $m) foreach (['rand' => $rand, 'text' => $text] as $n => $d) {
    ///     $z = new ZipArchive; $z->open('feed-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE); $z->addFromString($n, $d); $z->setCompressionName($n, $m);
    ///     echo "$m $n: "; $z->registerProgressCallback(0.0001, function ($r) { echo round($r, 4), ' '; }); $z->close();
    ///     $z->open('feed-witness.zip'); echo '| ', $z->statIndex(0)['comp_size'], "\n"; $z->close();
    /// }
    /// unlink('feed-witness.zip');
    /// ```
    /// ```expect
    /// 8 rand: 0 0.4096 0.7373 1 | 100020
    /// 8 text: 0 0.1408 0.2816 0.4352 0.576 0.7168 0.8576 1 | 365142
    /// 12 rand: 0 1 | 100818
    /// 12 text: 0 1 | 325352
    /// ```
    pub(crate) fn read(
        &mut self,
        out: &mut [u8],
        lower: &mut dyn FnMut(&mut [u8]) -> Result<usize, Status>,
    ) -> Result<usize, Status> {
        if out.is_empty() || matches!(self.phase, Phase::Ended { .. }) {
            return Ok(0);
        }
        let mut done = 0;
        while done < out.len() {
            let finish = self.phase != Phase::Input;
            let (turn, n) = self.algorithm.process(&mut out[done..], finish)?;
            done += n;
            match turn {
                Turn::More => {}
                Turn::End => {
                    let Some(first) = self.first_read else {
                        return Err(Status::new(Er::Internal));
                    };
                    let stored = self.can_store && first <= done;
                    self.phase = Phase::Ended { stored };
                    if stored {
                        out[..first].copy_from_slice(&self.buffer[..first]);
                        return Ok(first);
                    }
                    break;
                }
                Turn::NeedData if finish => break,
                Turn::NeedData => {
                    let got = lower(&mut self.buffer[..])?;
                    if got == 0 {
                        self.phase = Phase::Finishing;
                        self.first_read.get_or_insert(0);
                    } else {
                        if self.first_read.is_some() {
                            self.can_store = false;
                        } else {
                            self.first_read = Some(got);
                        }
                        self.algorithm.input(&self.buffer[..got])?;
                    }
                }
            }
        }
        if done > 0 {
            self.can_store = false;
        }
        Ok(done)
    }
}

/// `get_crc_table`: the CRC-32 table PKWARE's keys update through.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut n = 0u32;
    while n < 256 {
        let mut c = n;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[n as usize] = c;
        n += 1;
    }
    t
};

/// `zip_pkware_keys_t`.
struct Keys([u32; 3]);

impl Keys {
    /// `_zip_pkware_keys_reset` and the password fed through them.
    fn of(password: &[u8]) -> Keys {
        let mut k = Keys([0x1234_5678, 0x2345_6789, 0x3456_7890]);
        for b in password {
            k.update(*b);
        }
        k
    }

    fn crc(crc: u32, b: u8) -> u32 {
        CRC_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8)
    }

    fn update(&mut self, b: u8) {
        self.0[0] = Keys::crc(self.0[0], b);
        self.0[1] = (self.0[1].wrapping_add(self.0[0] & 0xff))
            .wrapping_mul(134_775_813)
            .wrapping_add(1);
        self.0[2] = Keys::crc(self.0[2], (self.0[1] >> 24) as u8);
    }

    fn encrypt(&mut self, b: u8) -> u8 {
        let t = (self.0[2] | 2) & 0xffff;
        let c = b ^ (((t * (t ^ 1)) >> 8) & 0xff) as u8;
        self.update(b);
        c
    }
}

/// Random bytes, `zip_secure_random`.
fn random(buf: &mut [u8]) -> Result<(), Status> {
    getrandom::fill(buf).map_err(|_| Status::new(Er::Internal))
}

/// `zip_source_pkware_encode` or `zip_source_winzip_aes_encode`, read as
/// their `ZIP_SOURCE_READ` reads: the header the open makes first, then
/// the data under the cipher, and past the data's end AES's ten bytes of
/// authentication code.
pub(crate) struct Sealer {
    /// The cipher, until the data below has ended.
    seal: Option<Seal>,
    head: Made,
    tail: Made,
    attributes: Attributes,
}

/// What a [`Sealer`] encrypts under: PKWARE's keys, or `WinZip` AES's
/// state.
enum Seal {
    Pkware(Keys),
    Aes(Box<WinzipAes>),
}

impl Sealer {
    /// `zip_source_pkware_encode` opened: a header of eleven random bytes
    /// and the high byte of the DOS time, under the password's keys. It
    /// needs version 2.0 and a data record after the data.
    pub(crate) fn pkware(password: &[u8], dostime: u16) -> Result<Sealer, Status> {
        let mut header = [0u8; 12];
        random(&mut header[..11])?;
        header[11] = (dostime >> 8) as u8;
        let mut keys = Keys::of(password);
        let head: Vec<u8> = header.iter().map(|b| keys.encrypt(*b)).collect();
        let attributes = Attributes {
            host: None,
            version_needed: Some(20),
            bits: Some((DATA_DESCRIPTOR, DATA_DESCRIPTOR)),
        };
        Ok(Sealer::new(Seal::Pkware(keys), &head, attributes))
    }

    /// `zip_source_winzip_aes_encode` opened: the salt and the password
    /// verifier, at `strength` 1, 2 or 3. It needs version 5.1.
    pub(crate) fn aes(password: &[u8], strength: u8) -> Result<Sealer, Status> {
        let mut salt = vec![0u8; 4 * usize::from(strength) + 4];
        random(&mut salt)?;
        let (ctx, verifier) = WinzipAes::new(password, &salt, strength)?;
        salt.extend_from_slice(&verifier);
        let attributes = Attributes {
            version_needed: Some(51),
            ..Attributes::default()
        };
        Ok(Sealer::new(Seal::Aes(Box::new(ctx)), &salt, attributes))
    }

    fn new(seal: Seal, head: &[u8], attributes: Attributes) -> Sealer {
        let mut sealer = Sealer {
            seal: Some(seal),
            head: Made::default(),
            tail: Made::default(),
            attributes,
        };
        sealer.head.extend(head);
        sealer
    }

    /// `ZIP_SOURCE_GET_FILE_ATTRIBUTES`.
    pub(crate) fn attributes(&self) -> Attributes {
        self.attributes
    }

    /// One `ZIP_SOURCE_READ` into `out`, the data read through `lower`.
    pub(crate) fn read(
        &mut self,
        out: &mut [u8],
        lower: &mut dyn FnMut(&mut [u8]) -> Result<usize, Status>,
    ) -> Result<usize, Status> {
        let mut n = self.head.take(out);
        let Some(seal) = &mut self.seal else {
            return Ok(n + self.tail.take(&mut out[n..]));
        };
        let want = out.len() - n;
        let got = lower(&mut out[n..])?;
        let data = &mut out[n..n + got];
        match seal {
            Seal::Pkware(keys) => data.iter_mut().for_each(|b| *b = keys.encrypt(*b)),
            Seal::Aes(ctx) => ctx.encrypt(data),
        }
        n += got;
        // The data below has ended: AES's code follows it.
        if got < want
            && let Some(Seal::Aes(ctx)) = self.seal.take()
        {
            self.tail.extend(&ctx.finish()[..10]);
            n += self.tail.take(&mut out[n..]);
        }
        Ok(n)
    }
}

/// `_zip_pkware_decrypt`'s keys, the password fed through them
/// (`zip_source_pkware_decode.c`).
pub(crate) struct PkwareKeys(Keys);

impl PkwareKeys {
    pub(crate) fn of(password: &[u8]) -> PkwareKeys {
        PkwareKeys(Keys::of(password))
    }

    /// `_zip_pkware_decrypt` in place.
    pub(crate) fn decrypt(&mut self, buf: &mut [u8]) {
        for b in buf {
            let t = (self.0.0[2] | 2) & 0xffff;
            let p = *b ^ (((t * (t ^ 1)) >> 8) & 0xff) as u8;
            self.0.update(p);
            *b = p;
        }
    }
}

/// AES at one of `WinZip`'s three strengths.
enum Cipher {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

/// `zip_winzip_aes_t`: AES-CTR with `WinZip`'s little-endian block
/// number, the keystream carried across calls, and the HMAC-SHA1 over
/// the encrypted bytes.
pub(crate) struct WinzipAes {
    cipher: Cipher,
    mac: hmac::Hmac<sha1::Sha1>,
    counter: u128,
    pad: [u8; 16],
    pad_offset: usize,
}

impl WinzipAes {
    /// `_zip_winzip_aes_new`: the keys PBKDF2 derives from `password`
    /// and `salt` at `strength` 1, 2 or 3, and the two bytes that verify
    /// the password.
    pub(crate) fn new(
        password: &[u8],
        salt: &[u8],
        strength: u8,
    ) -> Result<(WinzipAes, [u8; 2]), Status> {
        use aes::cipher::KeyInit;
        let key_len = 8 * usize::from(strength) + 8;
        let mut keys = vec![0u8; 2 * key_len + 2];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, 1000, &mut keys);
        let (enc_key, rest) = keys.split_at(key_len);
        let (mac_key, verifier) = rest.split_at(key_len);
        let fail = |_| Status::new(Er::Internal);
        let cipher = match strength {
            1 => Cipher::A128(aes::Aes128::new_from_slice(enc_key).map_err(fail)?),
            2 => Cipher::A192(aes::Aes192::new_from_slice(enc_key).map_err(fail)?),
            _ => Cipher::A256(aes::Aes256::new_from_slice(enc_key).map_err(fail)?),
        };
        let mac = <hmac::Hmac<sha1::Sha1> as KeyInit>::new_from_slice(mac_key).map_err(fail)?;
        let ctx = WinzipAes {
            cipher,
            mac,
            counter: 0,
            pad: [0; 16],
            pad_offset: 16,
        };
        Ok((ctx, [verifier[0], verifier[1]]))
    }

    /// `buf` exclusive-or'd with the keystream.
    fn xor(&mut self, buf: &mut [u8]) {
        use aes::cipher::BlockCipherEncrypt;
        for b in buf {
            if self.pad_offset == 16 {
                self.counter += 1;
                let mut block = aes::Block::from(self.counter.to_le_bytes());
                match &self.cipher {
                    Cipher::A128(c) => c.encrypt_block(&mut block),
                    Cipher::A192(c) => c.encrypt_block(&mut block),
                    Cipher::A256(c) => c.encrypt_block(&mut block),
                }
                self.pad.copy_from_slice(&block);
                self.pad_offset = 0;
            }
            *b ^= self.pad[self.pad_offset];
            self.pad_offset += 1;
        }
    }

    /// `_zip_winzip_aes_encrypt`.
    fn encrypt(&mut self, buf: &mut [u8]) {
        use hmac::Mac;
        self.xor(buf);
        self.mac.update(buf);
    }

    /// `_zip_winzip_aes_decrypt`.
    pub(crate) fn decrypt(&mut self, buf: &mut [u8]) {
        use hmac::Mac;
        self.mac.update(buf);
        self.xor(buf);
    }

    /// `_zip_winzip_aes_finish`: the authentication code, whose first ten
    /// bytes the entry ends with.
    pub(crate) fn finish(self) -> [u8; 20] {
        use hmac::Mac;
        self.mac.finalize().into_bytes().into()
    }
}
