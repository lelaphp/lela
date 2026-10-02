//! What libzip's algorithms and encryption sources do to an entry's data
//! on the way out (`zip_algorithm_*.c`, `zip_source_compress.c`,
//! `zip_source_pkware_encode.c`, `zip_source_winzip_aes_encode.c`), over
//! the codecs and ciphers the zip crate reads with; and ZSTD on the way
//! in, which the crate reads only through a C library.

use std::io::{Read, Write};

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

/// `BUFSIZE`: the input a default method stores rather than inflate,
/// read in one go (`compress_read`'s `can_store`).
const BUFSIZE: usize = 8192;

/// `zip_source_compress` with `method` at `level`: `can_store` is
/// `ZIP_CM_IS_DEFAULT`, under which data of one buffer that does not
/// shrink is stored.
pub(crate) fn compress(
    data: &[u8],
    method: u16,
    level: u32,
    can_store: bool,
) -> Result<Compressed, Status> {
    let fail = |_| Status::of(Er::Internal, 0);
    let (out, bitflags, version_needed) = match method {
        8 => {
            let level = if (1..=9).contains(&level) { level } else { 9 };
            let mut e =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
            e.write_all(data).map_err(fail)?;
            let bits = if level < 3 {
                4
            } else if level > 7 {
                2
            } else {
                0
            };
            (e.finish().map_err(fail)?, bits, 20)
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
        _ => return Err(Status::of(Er::Compnotsupp, 0)),
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

/// A ZSTD entry's data, and the error a read of it would meet.
pub(crate) fn unzstd(raw: &[u8]) -> (Vec<u8>, Option<Status>) {
    let mut out = Vec::new();
    let fault = match ruzstd::decoding::StreamingDecoder::new(raw) {
        Ok(mut d) => d
            .read_to_end(&mut out)
            .err()
            .map(|_| Status::of(Er::CompressedData, 0)),
        Err(_) => Some(Status::of(Er::CompressedData, 0)),
    };
    (out, fault)
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
    getrandom::fill(buf).map_err(|_| Status::of(Er::Internal, 0))
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

/// AES at one of `WinZip`'s three strengths.
enum Cipher {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

/// `zip_source_winzip_aes_encode`: the salt, the password verifier, the
/// data under AES-CTR with `WinZip`'s little-endian block number, and ten bytes
/// of HMAC-SHA1 over it. `strength` is 1, 2 or 3.
pub(crate) fn winzip_aes(data: &[u8], password: &[u8], strength: u8) -> Result<Vec<u8>, Status> {
    use aes::cipher::{BlockCipherEncrypt, KeyInit};
    use hmac::Mac;
    let key_len = 8 * usize::from(strength) + 8;
    let mut salt = vec![0u8; key_len / 2];
    random(&mut salt)?;
    let mut keys = vec![0u8; 2 * key_len + 2];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, &salt, 1000, &mut keys);
    let (enc_key, rest) = keys.split_at(key_len);
    let (mac_key, verifier) = rest.split_at(key_len);
    let fail = |_| Status::of(Er::Internal, 0);
    let mut out = salt;
    out.extend_from_slice(verifier);
    let cipher = match strength {
        1 => Cipher::A128(aes::Aes128::new_from_slice(enc_key).map_err(fail)?),
        2 => Cipher::A192(aes::Aes192::new_from_slice(enc_key).map_err(fail)?),
        _ => Cipher::A256(aes::Aes256::new_from_slice(enc_key).map_err(fail)?),
    };
    let mut body = data.to_vec();
    for (n, chunk) in body.chunks_mut(16).enumerate() {
        let mut block = aes::Block::from((n as u128 + 1).to_le_bytes());
        match &cipher {
            Cipher::A128(c) => c.encrypt_block(&mut block),
            Cipher::A192(c) => c.encrypt_block(&mut block),
            Cipher::A256(c) => c.encrypt_block(&mut block),
        }
        for (b, k) in chunk.iter_mut().zip(block.iter()) {
            *b ^= k;
        }
    }
    let mut mac = <hmac::Hmac<sha1::Sha1> as KeyInit>::new_from_slice(mac_key).map_err(fail)?;
    mac.update(&body);
    let code = mac.finalize().into_bytes();
    out.extend_from_slice(&body);
    out.extend_from_slice(&code[..10]);
    Ok(out)
}
