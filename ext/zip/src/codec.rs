//! What libzip's algorithms and encryption sources do to an entry's data
//! on the way out (`zip_algorithm_*.c`, `zip_source_compress.c`,
//! `zip_source_pkware_encode.c`, `zip_source_winzip_aes_encode.c`), over
//! pure-Rust codecs and ciphers; and the ciphers'
//! state on the way in, which [`crate::read`] decrypts with.

use std::io::Write;

use crate::error::{Er, Status};

/// A compressed entry: its bytes, the method they are in, and the
/// general purpose bits and version the algorithm reports
/// (`ZIP_SOURCE_GET_FILE_ATTRIBUTES`).
pub(crate) struct Compressed {
    pub(crate) data: Vec<u8>,
    pub(crate) method: u16,
    pub(crate) bitflags: u16,
    pub(crate) version_needed: u16,
}

/// `TORRENTZIP_COMPRESSION_FLAGS`, the level a torrentzip entry is
/// written at ([`crate::write`]'s `zip_dirent_torrentzip_normalize`):
/// `Z_BEST_COMPRESSION` at `TORRENTZIP_MEM_LEVEL`.
pub(crate) const TORRENTZIP_COMPRESSION_FLAGS: u32 = 0xffff;

/// `BUFSIZE`: the input a default method stores rather than inflate,
/// read in one go (`compress_read`'s `can_store`).
const BUFSIZE: usize = 8192;

/// `zip_source_compress` with `method` at `level`: `can_store` is
/// `ZIP_CM_IS_DEFAULT`, under which data of one buffer that does not
/// shrink is stored. Deflate is the platform zlib's ([`crate::ffi`]) at
/// libzip's memLevel, 9 or torrentzip's 8, so an entry carries php's
/// bytes at every level, and a stream cut short inflates as far as
/// php's does (#1651).
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
pub(crate) fn compress(
    data: &[u8],
    method: u16,
    level: u32,
    can_store: bool,
) -> Result<Compressed, Status> {
    let fail = |_| Status::new(Er::Internal);
    let (out, bitflags, version_needed) = match method {
        8 => {
            // `zip_algorithm_deflate.c`'s `allocate`: level 0 or past 9
            // is `Z_BEST_COMPRESSION`, and torrentzip's flags ask for
            // `TORRENTZIP_MEM_LEVEL`, every other level `MAX_MEM_LEVEL`.
            let mem_level = if level == TORRENTZIP_COMPRESSION_FLAGS {
                8
            } else {
                9
            };
            let level = if (1..=9).contains(&level) { level } else { 9 };
            let bits = if level < 3 {
                4
            } else if level > 7 {
                2
            } else {
                0
            };
            let Some(out) = crate::ffi::deflate(data, level.cast_signed(), mem_level) else {
                return Err(Status::new(Er::Zlib));
            };
            (out, bits, 20)
        }
        12 => {
            let level = if (1..=9).contains(&level) { level } else { 9 };
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(level));
            e.write_all(data).map_err(fail)?;
            (e.finish().map_err(fail)?, 0, 46)
        }
        14 | 95 => {
            let preset = if level <= 9 { level } else { 6 };
            if method == 95 {
                let mut options = lzma_rust2::XzOptions::with_preset(preset);
                options.set_check_sum_type(lzma_rust2::CheckType::Crc64);
                let mut w = lzma_rust2::XzWriter::new(Vec::new(), options).map_err(fail)?;
                w.write_all(data).map_err(fail)?;
                (w.finish().map_err(fail)?, 0, 63)
            } else {
                let options = lzma_rust2::LzmaOptions::with_preset(preset);
                let mut out = vec![0x09, 0x14, 0x05, 0x00, options.get_props()];
                out.extend_from_slice(&options.dict_size.to_le_bytes());
                let mut w =
                    lzma_rust2::LzmaWriter::new(out, &options, false, true, None).map_err(fail)?;
                w.write_all(data).map_err(fail)?;
                (w.finish().map_err(fail)?, 2, 63)
            }
        }
        93 => {
            let out = ruzstd::encoding::compress_to_vec(
                data,
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            (out, 0, 63)
        }
        _ => return Err(Status::new(Er::Compnotsupp)),
    };
    if can_store && data.len() <= BUFSIZE && data.len() <= out.len() {
        return Ok(Compressed {
            data: data.to_vec(),
            method: 0,
            bitflags: 0,
            version_needed,
        });
    }
    Ok(Compressed {
        data: out,
        method,
        bitflags,
        version_needed,
    })
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

/// `zip_source_pkware_encode`: a header of eleven random bytes and the
/// high byte of the DOS time, then the data, under the password's keys.
pub(crate) fn pkware(data: &[u8], password: &[u8], dostime: u16) -> Result<Vec<u8>, Status> {
    let mut header = [0u8; 12];
    random(&mut header[..11])?;
    header[11] = (dostime >> 8) as u8;
    let mut keys = Keys::of(password);
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend(header.iter().map(|b| keys.encrypt(*b)));
    out.extend(data.iter().map(|b| keys.encrypt(*b)));
    Ok(out)
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

/// `zip_source_winzip_aes_encode`: the salt, the password verifier, the
/// data under AES-CTR, and ten bytes of HMAC-SHA1 over it. `strength`
/// is 1, 2 or 3.
pub(crate) fn winzip_aes(data: &[u8], password: &[u8], strength: u8) -> Result<Vec<u8>, Status> {
    let mut salt = vec![0u8; 4 * usize::from(strength) + 4];
    random(&mut salt)?;
    let (mut ctx, verifier) = WinzipAes::new(password, &salt, strength)?;
    let mut out = salt;
    out.extend_from_slice(&verifier);
    let mut body = data.to_vec();
    ctx.encrypt(&mut body);
    out.extend_from_slice(&body);
    out.extend_from_slice(&ctx.finish()[..10]);
    Ok(out)
}
